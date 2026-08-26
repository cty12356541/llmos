#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! B-TASK-008C2G cross-term adoption acceptance slice.
//!
//! The happy path proves that an old quarantined permit can be adopted only
//! after takeover completion and successor-registry reopen, then reconcile
//! under the successor lease and close with the current registry binding.
//! The receipt is a separate v38 immutable evidence plane; same-term
//! adoption remains unchanged.

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_task::{
    AdoptionReplay, AttemptSpec, AuthorityLeaseCrossTermAdoptionRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseReconcileRequest, AuthorityLeaseRequest,
    AuthorityLeaseTakeoverFenceRequest, AuthoritySuccessorRegistryReopenRequest,
    AuthorityTakeoverBarrierReceiptRequest, BarrierObservationSignature,
    CompleteAuthorityTakeoverRequest, EffectPermitDecision, FinalizeRequest, FinalizeRequestV3,
    IssuedPermit, LogicalEffectDescriptor, Outcome, OutcomeRequest, ParticipantRecord,
    PermitDecision, PermitRecord, PermitRequest, PlannedEffect, ReconcileOutcome, ReconcileRequest,
    SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec,
    TaskStoreError, TaskWriteSetDecision, TaskWriteSetEffectEndpointRequest, TaskWriteSetRequest,
    barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ProcessId, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-cross-term-{name}-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open task authority")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

struct IdentityRoot(PathBuf);

impl IdentityRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-task-cross-term-identity-{label}-{nonce}-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for IdentityRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct BarrierSigner {
    key: SigningKey,
    binding: nlos_identity::IdentityBinding,
}

fn barrier_signer(identity: &IdentityAuthority, seed: u8) -> BarrierSigner {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::BarrierObservationSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap barrier signer")
        .binding();
    BarrierSigner { key, binding }
}

fn lease_request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: ProcessId::from_bytes([key.wrapping_add(holder); 16]),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: at_ms,
        ttl_ms,
    }
}

fn task_id() -> TaskId {
    TaskId::from_bytes([0x61; 16])
}

fn attempt() -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([0x62; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x63; 16]),
            snapshot_digest: [0x64; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x65; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x66; 16]),
        registered_at_ms: 2,
    }
}

fn planned_effect() -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            intent_spec_id: [0x71; 32],
            stable_action_slot: 0,
            target_authority_object_id: [0x72; 32],
            effect_class: 7,
            idempotency_scope: 3,
        },
        required: true,
        required_condition_digest: None,
        success_criteria_digest: [0x73; 32],
        action_proposal_digest: [0x74; 32],
    }
}

fn permit_request(spec: &AttemptSpec, write_set_root: [u8; 32]) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root,
        planned_effects: vec![planned_effect()],
        idempotency_key: IdempotencyKey::from_bytes([0x76; 16]),
        valid_until_ms: 10_000,
        requested_at_ms: 150,
    }
}

fn finalize_request(
    spec: &AttemptSpec,
    permit_id: nlos_types::CommitPermitId,
) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id,
            new_effect_history_root: [0; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms: 190,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0x77; 32],
    }
}

fn issued_permit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected issued permit, got {other:?}"),
    }
}

fn issued_effect(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Issued(record) => *record,
        EffectPermitDecision::Replayed(record) => {
            panic!("expected issued effect permit, got replayed {record:?}")
        }
    }
}

fn record_signed_observation(
    authority: &SqliteTaskAuthority,
    identity: &IdentityAuthority,
    signer: &BarrierSigner,
    takeover: &nlos_task::AuthorityTakeoverReceiptRecord,
    participant: ParticipantRecord,
) {
    let remote_receipt_id = ReceiptId::from_bytes([0x81; 16]);
    let barrier_digest = [0x82; 32];
    let message = barrier_observation_signature_message(
        takeover.receipt_id,
        &participant,
        remote_receipt_id,
        barrier_digest,
        takeover.exact_fence_set_root.expect("fence root"),
    );
    authority
        .record_authority_takeover_barrier_receipt_signed(
            identity,
            AuthorityTakeoverBarrierReceiptRequest {
                takeover_receipt_id: takeover.receipt_id,
                participant,
                remote_receipt_id,
                barrier_digest,
                observed_at_ms: 220,
            },
            BarrierObservationSignature {
                issuer: signer.binding.principal_id,
                control_domain_id: signer.binding.control_domain_id,
                key_id: signer.binding.key_id,
                signature: signer.key.sign(&message).to_bytes(),
            },
        )
        .expect("record signed observation");
}

#[test]
#[allow(clippy::too_many_lines)]
fn cross_term_adoption_reconciles_old_permit_under_successor_proof() {
    let database = TestDatabase::new("happy");
    let identity_root = IdentityRoot::new("happy");
    let artifact_root = IdentityRoot::new("artifact");
    let semantic_root = IdentityRoot::new("semantic");
    let authority = database.open();
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).expect("artifact");
    let semantic = nlos_semantic::SemanticAuthority::open(&semantic_root.0).expect("semantic");
    let identity = IdentityAuthority::open(&identity_root.0).expect("identity");
    let signer = barrier_signer(&identity, 0x90);

    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1,
        })
        .expect("task");
    let spec = attempt();
    let initial_registry = authority
        .inspect_participant_registry(task_id())
        .expect("registry");
    authority
        .register_semantic_admission_participant(
            &semantic,
            task_id(),
            nlos_task::ParticipantRegistryBinding {
                generation: initial_registry.generation,
                root: initial_registry.root,
            },
            3,
        )
        .expect("semantic participant");
    let snapshot_receipt_id = ReceiptId::from_bytes([0x67; 16]);
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id: snapshot_receipt_id,
            builder_id: [0x68; 16],
            builder_version_digest: [0x69; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x6a; 16])],
            dependency_closure_root: [0x6b; 32],
            semantic_resolver_digest: [0x6c; 32],
            canonical_iteration_digest: [0x6d; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 4,
            authority_id: [0x6e; 16],
            key_id: [0x6f; 16],
            signature: [0x70; 64],
        })
        .expect("snapshot receipt");
    authority
        .register_attempt_with_snapshot_receipt(spec, snapshot_receipt_id)
        .expect("attempt");
    let current_registry = authority
        .inspect_participant_registry(task_id())
        .expect("registry");
    let write_set = match authority
        .seal_task_write_set_with_semantic_authority(
            &artifact,
            &semantic,
            TaskWriteSetRequest {
                task_id: spec.task_id,
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: Vec::new(),
                resource_reservations: Vec::new(),
                planned_effects: vec![planned_effect()],
                effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::SemanticAdmission {
                    effect_seq: 0,
                }],
                idempotency_key: IdempotencyKey::from_bytes([0x79; 16]),
                sealed_at_ms: 5,
            },
        )
        .expect("write set")
    {
        TaskWriteSetDecision::Sealed(record) | TaskWriteSetDecision::Replayed(record) => record,
    };
    assert_eq!(
        write_set.participant_registry_binding,
        nlos_task::ParticipantRegistryBinding {
            generation: current_registry.generation,
            root: current_registry.root,
        }
    );
    let lease_one = authority
        .acquire_authority_lease(lease_request(1, 0x91, 100, 100))
        .expect("lease one")
        .record();
    let permit = issued_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, write_set.write_set_root),
                lease: lease_one,
            })
            .expect("permit"),
    );
    let registry_binding = permit
        .participant_registry_binding
        .expect("original registry");
    let effect = issued_effect(
        authority
            .request_effect_permit(nlos_task::EffectPermitRequest {
                task_id: spec.task_id,
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: 0,
                idempotency_key: IdempotencyKey::from_bytes([0x83; 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 160,
            })
            .expect("effect permit"),
    );
    authority
        .consume_dispatch_token(nlos_task::DispatchRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_permit_id: effect.effect_permit_id,
            dispatch_token: effect.one_shot_dispatch_token,
            dispatched_at_ms: 170,
        })
        .expect("dispatch");
    authority
        .record_effect_outcome(OutcomeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            outcome: Outcome::Unknown {
                uncertainty_digest: [0x84; 32],
            },
            recorded_at_ms: 180,
        })
        .expect("unknown");
    assert!(matches!(
        authority.finalize_commit_v3_with_authority_lease(
            nlos_task::AuthorityLeaseFinalizeRequest {
                finalize: finalize_request(&spec, permit.permit_id),
                lease: lease_one,
            }
        ),
        Err(TaskStoreError::Quarantined)
    ));

    let lease_two = authority
        .acquire_authority_lease(lease_request(2, 0x92, 201, 1_000))
        .expect("lease two")
        .record();
    let frozen = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: spec.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 210,
        })
        .expect("freeze");
    let fence = authority
        .inspect_authority_takeover_fence_receipt(spec.task_id, registry_binding)
        .expect("fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(spec.task_id, fence.receipt_id)
        .expect("takeover receipt");
    assert_eq!(
        frozen.state,
        nlos_task::ParticipantRegistryState::FrozenForTakeover
    );
    let members = authority
        .inspect_authority_takeover_fence_members(spec.task_id, registry_binding)
        .expect("fence members");
    assert!(!members.is_empty());
    for member in members.iter().map(|member| member.participant) {
        record_signed_observation(&authority, &identity, &signer, &takeover, member);
    }
    authority
        .complete_authority_takeover(CompleteAuthorityTakeoverRequest {
            takeover_receipt_id: takeover.receipt_id,
            lease: lease_two,
            completed_at_ms: 230,
        })
        .expect("complete takeover");
    let reopened = authority
        .reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
            takeover_receipt_id: takeover.receipt_id,
            lease: lease_two,
            reopened_at_ms: 240,
        })
        .expect("reopen successor registry");

    let adoption_request = nlos_task::AdoptionRequest {
        task_id: spec.task_id,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        idempotency_key: IdempotencyKey::from_bytes([0x85; 16]),
        adopted_at_ms: 250,
    };
    let adoption = match authority
        .adopt_permit_across_takeover(AuthorityLeaseCrossTermAdoptionRequest {
            adoption: adoption_request,
            takeover_receipt_id: takeover.receipt_id,
            successor_lease: lease_two,
        })
        .expect("cross-term adoption")
    {
        AdoptionReplay::Adopted(record) => *record,
        AdoptionReplay::Replayed(record) => {
            panic!("expected adopted receipt, got replayed {record:?}")
        }
    };
    assert_eq!(adoption.takeover_receipt_id, Some(takeover.receipt_id));
    assert_eq!(
        adoption.original_authority_lease_binding,
        Some(lease_one.binding())
    );
    assert_eq!(
        adoption.original_participant_registry_binding,
        Some(registry_binding)
    );
    assert_eq!(
        adoption.current_assignment_id,
        Some(reopened.active_assignment_id)
    );
    assert_eq!(
        adoption.current_participant_registry_binding,
        Some(reopened.successor_registry_binding)
    );
    assert!(adoption.exact_fenced_participant_root.is_some());
    assert_eq!(
        authority
            .inspect_permit(spec.task_id, permit.permit_id)
            .unwrap()
            .state,
        nlos_task::PermitState::Quarantined
    );
    assert!(matches!(
        authority.adopt_permit_across_takeover(AuthorityLeaseCrossTermAdoptionRequest {
            adoption: adoption_request,
            takeover_receipt_id: takeover.receipt_id,
            successor_lease: lease_two,
        }),
        Ok(AdoptionReplay::Replayed(_))
    ));

    authority
        .reconcile_effect_with_authority_lease(AuthorityLeaseReconcileRequest {
            reconcile: ReconcileRequest {
                task_id: spec.task_id,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: 0,
                adoption_receipt_id: adoption.receipt_id,
                outcome: ReconcileOutcome::EffectClosed,
                closure_proof_digest: [0x86; 32],
                reconciled_at_ms: 260,
            },
            lease: lease_two,
        })
        .expect("reconcile under successor lease");
    assert_eq!(
        authority
            .inspect_permit(spec.task_id, permit.permit_id)
            .unwrap()
            .state,
        nlos_task::PermitState::Issued
    );
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    let receipt = authority
        .inspect_effect_receipt(slot.effect_receipt_id.expect("effect receipt"))
        .expect("effect receipt");
    let proof = nlos_task::RequiredSatisfaction {
        effect_seq: 0,
        proof: nlos_task::RequiredSatisfactionProof::EffectClosedSuccess {
            success_assertion_digest: nlos_task::expected_success_assertion_digest(&slot, &receipt),
        },
    };
    let mut finalize = finalize_request(&spec, permit.permit_id);
    finalize.base.finalized_at_ms = 270;
    finalize.required_satisfaction = vec![proof];
    let committed = authority
        .finalize_commit_v3_with_authority_lease(nlos_task::AuthorityLeaseFinalizeRequest {
            finalize,
            lease: lease_two,
        })
        .expect("finalize adopted permit")
        .into_committed_receipt();
    assert_eq!(
        committed.participant_registry_binding,
        Some(reopened.successor_registry_binding)
    );
    assert_eq!(committed.new_head_commit_seq, 1);

    let reopened_authority = database.open();
    assert_eq!(
        reopened_authority
            .inspect_adoption_receipt(spec.task_id, adoption.receipt_id)
            .expect("adoption after restart"),
        adoption
    );
}

trait FinalizeDecisionExt {
    fn into_committed_receipt(self) -> nlos_task::TaskReceiptRecord;
}

impl FinalizeDecisionExt for nlos_task::FinalizeDecision {
    fn into_committed_receipt(self) -> nlos_task::TaskReceiptRecord {
        match self {
            nlos_task::FinalizeDecision::Committed(receipt)
            | nlos_task::FinalizeDecision::Replayed(receipt) => *receipt,
        }
    }
}
