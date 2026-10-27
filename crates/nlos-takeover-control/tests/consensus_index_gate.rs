//! Integration verification for [`CommittedControlIndexGate`] (`ADR-0026`
//! `C-CONSENSUS-CP`, `[DUR-ACK-002]`, `[TASK-SCALE-002]`, `S-A` vs `S-B`).

use std::env;
use std::fs;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose, RevokeKeyRequest,
};
use nlos_takeover_control::{
    AdmittedCommitDisposition, CommittedControlIndexGate, ConsensusBackingMode,
    ConsensusIndexGateError, ControlCommitSubject, QuorumCommitAttestation, VoterCommitAttestation,
    quorum_commit_attestation_digest,
};
use nlos_task::{
    AuthorityAssignmentState, AuthorityLeaseBinding, AuthorityTakeoverReceiptState,
    ParticipantRegistryBinding, TaskAuthorityAssignment, TaskAuthorityTakeoverReceipt,
};
use nlos_types::{
    ControlDomainId, Generation, IdempotencyKey, ProcessId, ReceiptId, TaskAuthorityAssignmentId,
    TaskId, TaskParticipantId,
};

static DIR_SEQ: AtomicU64 = AtomicU64::new(1);

const NOW_MS: u64 = 1_780_000_000_000;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "nlos-takeover-cgate-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct TestVoter {
    signing_key: SigningKey,
    binding: IdentityBinding,
}

impl TestVoter {
    fn bootstrap(identity: &IdentityAuthority, tag: u8) -> Self {
        let signing_key = SigningKey::from_bytes(&[tag.wrapping_add(0x20); 32]);
        let binding = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [tag.wrapping_add(1); 32],
                control_domain_policy_digest: [tag.wrapping_add(2); 32],
                public_key: signing_key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: NOW_MS - 60_000,
                key_valid_until_ms: NOW_MS + 86_400_000,
                idempotency_key: IdempotencyKey::from_bytes([tag.wrapping_add(0x50); 16]),
                created_at_ms: NOW_MS - 60_000,
            })
            .expect("bootstrap voter principal")
            .binding();
        Self {
            signing_key,
            binding,
        }
    }

    fn sign_digest(&self, digest: &[u8; 32]) -> VoterCommitAttestation {
        let sig = self.signing_key.sign(digest).to_bytes();
        VoterCommitAttestation {
            voter_principal: self.binding.principal_id,
            voter_control_domain: self.binding.control_domain_id,
            voter_key_id: self.binding.key_id,
            signature: sig,
        }
    }
}

fn sample_assignment(
    assignment_tag: u8,
    task_tag: u8,
    authority_term: u64,
    control_epoch: u64,
) -> TaskAuthorityAssignment {
    TaskAuthorityAssignment {
        assignment_id: TaskAuthorityAssignmentId::from_bytes([assignment_tag; 16]),
        task_id: TaskId::from_bytes([task_tag; 16]),
        task_generation: Generation::INITIAL,
        authority_lease_binding: AuthorityLeaseBinding {
            authority_id: TaskParticipantId::from_bytes([0xA1; 16]),
            holder_id: ProcessId::from_bytes([0xB1; 16]),
            term: authority_term,
            lease_epoch: control_epoch,
            fencing_token: [assignment_tag; 32],
            expires_at_ms: 2_000_000_000_000,
        },
        control_epoch,
        participant_registry_binding: ParticipantRegistryBinding {
            generation: authority_term,
            root: [assignment_tag.wrapping_add(0x10); 32],
        },
        state: AuthorityAssignmentState::Active,
        created_at_ms: 1_780_000_000_000,
        updated_at_ms: 1_780_000_000_000,
    }
}

fn sample_completed_takeover(
    receipt_tag: u8,
    task_tag: u8,
    old_assignment_tag: u8,
    new_assignment_tag: u8,
    old_term: u64,
    new_term: u64,
    consensus_commit_index: Option<u64>,
) -> TaskAuthorityTakeoverReceipt {
    TaskAuthorityTakeoverReceipt {
        receipt_id: ReceiptId::from_bytes([receipt_tag; 16]),
        task_id: TaskId::from_bytes([task_tag; 16]),
        task_generation: Generation::INITIAL,
        old_assignment_id: TaskAuthorityAssignmentId::from_bytes([old_assignment_tag; 16]),
        new_assignment_id: Some(TaskAuthorityAssignmentId::from_bytes(
            [new_assignment_tag; 16],
        )),
        fence_receipt_id: ReceiptId::from_bytes([receipt_tag.wrapping_add(1); 16]),
        frozen_old_authority_term: old_term,
        frozen_old_control_epoch: old_term,
        new_authority_lease_binding: AuthorityLeaseBinding {
            authority_id: TaskParticipantId::from_bytes([0xA2; 16]),
            holder_id: ProcessId::from_bytes([0xB2; 16]),
            term: new_term,
            lease_epoch: new_term,
            fencing_token: [new_assignment_tag; 32],
            expires_at_ms: 2_000_000_000_000,
        },
        new_control_epoch: new_term,
        frozen_registry_binding: ParticipantRegistryBinding {
            generation: old_term,
            root: [old_assignment_tag.wrapping_add(0x10); 32],
        },
        exact_fence_set_root: Some([0xEE; 32]),
        outstanding_operation_participant_root: None,
        outstanding_effect_quarantine_root: Some([0xCC; 32]),
        consensus_commit_index,
        barrier_state: AuthorityTakeoverReceiptState::Complete,
        created_at_ms: 1_780_000_001_000,
    }
}

#[test]
fn multi_host_quorum_enforces_majority_distinct_domains_and_monotonic_commit_index() {
    let tmp = TempDir::new("multihost");
    let identity =
        IdentityAuthority::open(tmp.path.join("identity.sqlite3")).expect("open identity");

    let v1 = TestVoter::bootstrap(&identity, 1);
    let v2 = TestVoter::bootstrap(&identity, 2);
    let v3 = TestVoter::bootstrap(&identity, 3);

    // Duplicate ControlDomainId in roster MUST fail closed (`[TASK-SCALE-002]`).
    let shared_domain = ControlDomainId::from_bytes([0x99; 16]);
    let dup_err = ConsensusBackingMode::multi_host_quorum(&[
        (v1.binding.principal_id, shared_domain),
        (v2.binding.principal_id, shared_domain),
        (v3.binding.principal_id, v3.binding.control_domain_id),
    ])
    .expect_err("duplicate ControlDomainId must fail closed");
    assert!(matches!(
        dup_err,
        ConsensusIndexGateError::NonIndependentControlDomain(dom) if dom == shared_domain
    ));

    let mode = ConsensusBackingMode::multi_host_quorum(&[
        (v1.binding.principal_id, v1.binding.control_domain_id),
        (v2.binding.principal_id, v2.binding.control_domain_id),
        (v3.binding.principal_id, v3.binding.control_domain_id),
    ])
    .expect("valid 3-voter independent-domain roster");
    assert_eq!(mode.required_quorum(), 2);

    let mut gate = CommittedControlIndexGate::new(mode);

    // 1. Admit initial TaskAuthorityAssignment at consensus_commit_index = 4, term = 1.
    let assign_v1 = sample_assignment(0x11, 0x77, 1, 1);
    let subject_assign = ControlCommitSubject::from_assignment(&assign_v1).expect("subject");
    let cidx_4 = NonZeroU64::new(4).unwrap();
    let cterm_1 = NonZeroU64::new(1).unwrap();
    let root_4 = [0x44; 32];
    let digest_4 = quorum_commit_attestation_digest(cidx_4, cterm_1, &root_4, &subject_assign);

    // Minority (1-of-3) attestation MUST fail closed with InsufficientQuorum.
    let minority_att = QuorumCommitAttestation {
        consensus_commit_index: cidx_4,
        consensus_term: cterm_1,
        state_root_digest: root_4,
        verified_at_ms: NOW_MS,
        voter_attestations: vec![v1.sign_digest(&digest_4)],
    };
    assert!(matches!(
        gate.admit_assignment(&identity, &assign_v1, cidx_4, &minority_att),
        Err(ConsensusIndexGateError::InsufficientQuorum {
            verified_count: 1,
            required_quorum: 2,
        })
    ));

    // 2-of-3 distinct-domain quorum succeeds!
    let quorum_att_4 = QuorumCommitAttestation {
        consensus_commit_index: cidx_4,
        consensus_term: cterm_1,
        state_root_digest: root_4,
        verified_at_ms: NOW_MS,
        voter_attestations: vec![v1.sign_digest(&digest_4), v2.sign_digest(&digest_4)],
    };
    let proof_4 = gate
        .admit_assignment(&identity, &assign_v1, cidx_4, &quorum_att_4)
        .expect("2-of-3 quorum must admit assignment");
    assert_eq!(proof_4.disposition, AdmittedCommitDisposition::Advanced);
    assert_eq!(proof_4.consensus_commit_index, cidx_4);
    assert_eq!(proof_4.verified_voter_domains.len(), 2);

    // Exact replay at index 4 returns IdempotentReplay.
    let replay_4 = gate
        .admit_assignment(&identity, &assign_v1, cidx_4, &quorum_att_4)
        .expect("idempotent replay");
    assert_eq!(
        replay_4.disposition,
        AdmittedCommitDisposition::IdempotentReplay
    );
}

#[test]
fn multi_host_quorum_enforces_takeover_commit_index_monotonicity_and_key_revocation() {
    let tmp = TempDir::new("multihost-takeover");
    let identity =
        IdentityAuthority::open(tmp.path.join("identity.sqlite3")).expect("open identity");

    let v1 = TestVoter::bootstrap(&identity, 1);
    let v2 = TestVoter::bootstrap(&identity, 2);
    let v3 = TestVoter::bootstrap(&identity, 3);

    let mode = ConsensusBackingMode::multi_host_quorum(&[
        (v1.binding.principal_id, v1.binding.control_domain_id),
        (v2.binding.principal_id, v2.binding.control_domain_id),
        (v3.binding.principal_id, v3.binding.control_domain_id),
    ])
    .expect("valid 3-voter independent-domain roster");
    let mut gate = CommittedControlIndexGate::new(mode);

    let assign_v1 = sample_assignment(0x11, 0x77, 1, 1);
    let subject_assign = ControlCommitSubject::from_assignment(&assign_v1).expect("subject");
    let cidx_4 = NonZeroU64::new(4).unwrap();
    let cterm_1 = NonZeroU64::new(1).unwrap();
    let digest_4 = quorum_commit_attestation_digest(cidx_4, cterm_1, &[0x44; 32], &subject_assign);
    gate.admit_assignment(
        &identity,
        &assign_v1,
        cidx_4,
        &QuorumCommitAttestation {
            consensus_commit_index: cidx_4,
            consensus_term: cterm_1,
            state_root_digest: [0x44; 32],
            verified_at_ms: NOW_MS,
            voter_attestations: vec![v1.sign_digest(&digest_4), v2.sign_digest(&digest_4)],
        },
    )
    .expect("admit initial assignment");

    // Takeover receipt with `None` or mismatched `consensus_commit_index` fails closed.
    let cidx_9 = NonZeroU64::new(9).unwrap();
    let cterm_2 = NonZeroU64::new(2).unwrap();
    let root_9 = [0x99; 32];
    let receipt_missing_idx = sample_completed_takeover(0x31, 0x77, 0x11, 0x12, 1, 2, None);
    let receipt_valid = sample_completed_takeover(0x31, 0x77, 0x11, 0x12, 1, 2, Some(9));
    let subject_takeover =
        ControlCommitSubject::from_takeover_receipt(&receipt_valid).expect("subject");
    let digest_9 = quorum_commit_attestation_digest(cidx_9, cterm_2, &root_9, &subject_takeover);
    let quorum_att_9 = QuorumCommitAttestation {
        consensus_commit_index: cidx_9,
        consensus_term: cterm_2,
        state_root_digest: root_9,
        verified_at_ms: NOW_MS,
        voter_attestations: vec![v2.sign_digest(&digest_9), v3.sign_digest(&digest_9)],
    };

    assert!(matches!(
        gate.admit_takeover_receipt(&identity, &receipt_missing_idx, &quorum_att_9),
        Err(ConsensusIndexGateError::MissingReceiptConsensusCommitIndex)
    ));

    let receipt_wrong_idx = sample_completed_takeover(0x31, 0x77, 0x11, 0x12, 1, 2, Some(8));
    assert!(matches!(
        gate.admit_takeover_receipt(&identity, &receipt_wrong_idx, &quorum_att_9),
        Err(ConsensusIndexGateError::ConsensusCommitIndexMismatch {
            record_index: 8,
            attested_index,
        }) if attested_index == cidx_9
    ));

    // Valid takeover receipt at index = 9, consensus_term = 2, authority_term = 2 succeeds!
    let proof_9 = gate
        .admit_takeover_receipt(&identity, &receipt_valid, &quorum_att_9)
        .expect("admit majority-committed takeover receipt");
    assert_eq!(proof_9.disposition, AdmittedCommitDisposition::Advanced);
    assert_eq!(proof_9.consensus_commit_index, cidx_9);
    assert_eq!(proof_9.authority_term, NonZeroU64::new(2).unwrap());

    // Stale commit index (e.g. 6 < 9) MUST fail closed!
    let cidx_6 = NonZeroU64::new(6).unwrap();
    let stale_receipt = sample_completed_takeover(0x32, 0x77, 0x12, 0x13, 2, 3, Some(6));
    let stale_subj = ControlCommitSubject::from_takeover_receipt(&stale_receipt).unwrap();
    let digest_6 = quorum_commit_attestation_digest(cidx_6, cterm_2, &[0x66; 32], &stale_subj);
    let stale_att = QuorumCommitAttestation {
        consensus_commit_index: cidx_6,
        consensus_term: cterm_2,
        state_root_digest: [0x66; 32],
        verified_at_ms: NOW_MS,
        voter_attestations: vec![v1.sign_digest(&digest_6), v3.sign_digest(&digest_6)],
    };
    assert!(matches!(
        gate.admit_takeover_receipt(&identity, &stale_receipt, &stale_att),
        Err(ConsensusIndexGateError::StaleConsensusCommitIndex {
            high_water_index,
            attempted_index,
        }) if high_water_index == cidx_9 && attempted_index == cidx_6
    ));
}

#[test]
fn multi_host_quorum_rejects_revoked_voter_key_via_identity_authority() {
    let tmp = TempDir::new("multihost-revoke");
    let identity =
        IdentityAuthority::open(tmp.path.join("identity.sqlite3")).expect("open identity");

    let v1 = TestVoter::bootstrap(&identity, 1);
    let v2 = TestVoter::bootstrap(&identity, 2);
    let v3 = TestVoter::bootstrap(&identity, 3);

    let mode = ConsensusBackingMode::multi_host_quorum(&[
        (v1.binding.principal_id, v1.binding.control_domain_id),
        (v2.binding.principal_id, v2.binding.control_domain_id),
        (v3.binding.principal_id, v3.binding.control_domain_id),
    ])
    .expect("valid 3-voter independent-domain roster");
    let mut gate = CommittedControlIndexGate::new(mode);

    // Revoking v2's key causes subsequent v2 attestations to be rejected by IdentityAuthority!
    identity
        .revoke_key(RevokeKeyRequest {
            key_id: v2.binding.key_id,
            expected_key_generation: v2.binding.key_generation,
            expected_identity_snapshot_id: v2.binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0xE2; 16]),
            revoked_at_ms: NOW_MS + 100,
        })
        .expect("revoke v2 key");

    let cidx_12 = NonZeroU64::new(12).unwrap();
    let cterm_2 = NonZeroU64::new(2).unwrap();
    let receipt_12 = sample_completed_takeover(0x35, 0x77, 0x12, 0x14, 2, 3, Some(12));
    let subj_12 = ControlCommitSubject::from_takeover_receipt(&receipt_12).unwrap();
    let digest_12 = quorum_commit_attestation_digest(cidx_12, cterm_2, &[0x12; 32], &subj_12);
    let revoked_att = QuorumCommitAttestation {
        consensus_commit_index: cidx_12,
        consensus_term: cterm_2,
        state_root_digest: [0x12; 32],
        verified_at_ms: NOW_MS + 200,
        voter_attestations: vec![v1.sign_digest(&digest_12), v2.sign_digest(&digest_12)],
    };
    assert!(matches!(
        gate.admit_takeover_receipt(&identity, &receipt_12, &revoked_att),
        Err(ConsensusIndexGateError::VoterIdentityRejected { voter, .. })
            if voter == v2.binding.principal_id
    ));
}

#[test]
fn single_host_passthrough_mode_verifies_local_commit_and_rejects_foreign_principal() {
    let tmp = TempDir::new("singlehost");
    let identity =
        IdentityAuthority::open(tmp.path.join("identity.sqlite3")).expect("open identity");

    let local = TestVoter::bootstrap(&identity, 10);
    let foreign = TestVoter::bootstrap(&identity, 11);

    let mut gate = CommittedControlIndexGate::new(ConsensusBackingMode::single_host_passthrough(
        local.binding.principal_id,
        local.binding.control_domain_id,
    ));
    assert_eq!(gate.backing_mode().required_quorum(), 1);

    let assign = sample_assignment(0x21, 0x88, 1, 1);
    let subj = ControlCommitSubject::from_assignment(&assign).unwrap();
    let cidx_1 = NonZeroU64::new(1).unwrap();
    let cterm_1 = NonZeroU64::new(1).unwrap();
    let digest = quorum_commit_attestation_digest(cidx_1, cterm_1, &[0xAA; 32], &subj);

    // Foreign principal must be rejected in S-B passthrough mode.
    let bad_att = QuorumCommitAttestation {
        consensus_commit_index: cidx_1,
        consensus_term: cterm_1,
        state_root_digest: [0xAA; 32],
        verified_at_ms: NOW_MS,
        voter_attestations: vec![foreign.sign_digest(&digest)],
    };
    assert!(matches!(
        gate.admit_assignment(&identity, &assign, cidx_1, &bad_att),
        Err(ConsensusIndexGateError::UnauthorizedVoter {
            principal,
            domain,
        }) if principal == foreign.binding.principal_id
            && domain == foreign.binding.control_domain_id
    ));

    // Local principal succeeds with quorum = 1!
    let ok_att = QuorumCommitAttestation {
        consensus_commit_index: cidx_1,
        consensus_term: cterm_1,
        state_root_digest: [0xAA; 32],
        verified_at_ms: NOW_MS,
        voter_attestations: vec![local.sign_digest(&digest)],
    };
    let proof = gate
        .admit_assignment(&identity, &assign, cidx_1, &ok_att)
        .expect("single-host passthrough commit proof must succeed");
    assert_eq!(proof.consensus_commit_index, cidx_1);
    assert_eq!(proof.verified_voter_domains.len(), 1);
}
