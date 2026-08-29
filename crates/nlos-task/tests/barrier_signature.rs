//! Acceptance tests for schema-v36 signed takeover-barrier observations.
//! Each test proves one slice of the `nlos-identity` principal signature
//! gate: verified-signer persistence and readback, purpose/binding/signature
//! fail-closed paths, unsigned/signed replay conflicts, restart readback,
//! and the v35 → v36 schema migration. The tests do not claim IPC peer
//! authentication or cross-term adoption semantics.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_task::{
    AuthorityLeaseDecision, AuthorityLeaseFinalizeRequest, AuthorityLeasePermitRequest,
    AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRequest,
    AuthorityTakeoverBarrierCoverageState, AuthorityTakeoverBarrierReceiptRecord,
    AuthorityTakeoverBarrierReceiptRequest, AuthorityTakeoverBarrierSigner,
    BarrierObservationSignature, FinalizeRequest, FinalizeRequestV3, ParticipantRecord,
    PermitDecision, PermitRequest, SnapshotBundle, SqliteTaskAuthority, TaskStoreError,
    barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, PrincipalId, ProcessId, ReceiptId,
    TaskAttemptId, TaskId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-barrier-signature-{name}-{}-{sequence}.sqlite3",
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
            .expect("monotonic clock")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-task-barrier-signature-identity-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for IdentityRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct BarrierSigner {
    key: SigningKey,
    binding: nlos_identity::IdentityBinding,
}

fn bootstrap_barrier_signer(
    identity: &IdentityAuthority,
    seed: u8,
    purpose: KeyPurpose,
) -> BarrierSigner {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: purpose,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap signer")
        .binding();
    BarrierSigner { key, binding }
}

struct FrozenFence {
    takeover_receipt_id: ReceiptId,
    participant: ParticipantRecord,
    fence_set_root: [u8; 32],
}

fn lease_request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: ProcessId::from_bytes([key.wrapping_add(holder); 16]),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: at_ms,
        ttl_ms,
    }
}

fn lease_record(decision: AuthorityLeaseDecision) -> nlos_task::AuthorityLeaseRecord {
    decision.record()
}

fn register_task_attempt(authority: &SqliteTaskAuthority, seed: u8) -> nlos_task::AttemptSpec {
    let task_id = TaskId::from_bytes([seed; 16]);
    authority
        .register_task(nlos_task::TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1,
        })
        .expect("register task");
    let attempt = nlos_task::AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(1); 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: nlos_types::TaskSnapshotId::from_bytes([seed.wrapping_add(2); 16]),
            snapshot_digest: [seed.wrapping_add(3); 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(4); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
        registered_at_ms: 2,
    };
    authority
        .register_attempt(attempt)
        .expect("register attempt");
    attempt
}

fn permit_request(
    attempt: &nlos_task::AttemptSpec,
    seed: u8,
    requested_at_ms: i64,
) -> PermitRequest {
    PermitRequest {
        task_id: attempt.task_id,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        write_set_root: [seed; 32],
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(10); 16]),
        valid_until_ms: 10_000,
        requested_at_ms,
    }
}

fn finalize_request(
    attempt: &nlos_task::AttemptSpec,
    permit_id: nlos_types::CommitPermitId,
    finalized_at_ms: i64,
) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: attempt.task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id,
            new_effect_history_root: [0; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0; 32],
    }
}

fn fence_takeover(authority: &SqliteTaskAuthority, seed: u8) -> FrozenFence {
    let lease_one = lease_record(
        authority
            .acquire_authority_lease(lease_request(1, seed, 100, 100))
            .expect("initial lease"),
    );
    let attempt = register_task_attempt(authority, seed);
    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&attempt, seed, 150),
            lease: lease_one,
        })
        .expect("lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");
    authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&attempt, permit.permit_id, 160),
            lease: lease_one,
        })
        .expect("close permit before takeover");
    let lease_two = lease_record(
        authority
            .acquire_authority_lease(lease_request(2, seed.wrapping_add(0x11), 201, 100))
            .expect("takeover lease"),
    );
    assert_eq!(lease_two.term, lease_one.term + 1);
    let frozen = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: attempt.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 210,
        })
        .expect("freeze current registry");
    let fence_receipt = authority
        .inspect_authority_takeover_fence_receipt(attempt.task_id, registry_binding)
        .expect("takeover fence receipt");
    let takeover_receipt = authority
        .inspect_authority_takeover_receipt(attempt.task_id, fence_receipt.receipt_id)
        .expect("pending takeover receipt");
    assert_eq!(takeover_receipt.new_assignment_id, None);
    FrozenFence {
        takeover_receipt_id: takeover_receipt.receipt_id,
        participant: frozen
            .participants
            .first()
            .copied()
            .expect("frozen registry participant"),
        fence_set_root: takeover_receipt
            .exact_fence_set_root
            .expect("exact fence set root"),
    }
}

fn barrier_request(
    fence: &FrozenFence,
    observed_at_ms: i64,
) -> AuthorityTakeoverBarrierReceiptRequest {
    AuthorityTakeoverBarrierReceiptRequest {
        takeover_receipt_id: fence.takeover_receipt_id,
        participant: fence.participant,
        remote_receipt_id: ReceiptId::from_bytes([0x91; 16]),
        barrier_digest: [0x92; 32],
        observed_at_ms,
    }
}

fn observation_digest(fence: &FrozenFence) -> [u8; 32] {
    barrier_observation_signature_message(
        fence.takeover_receipt_id,
        &fence.participant,
        ReceiptId::from_bytes([0x91; 16]),
        [0x92; 32],
        fence.fence_set_root,
    )
}

fn barrier_signature(
    signer: &BarrierSigner,
    message_digest: [u8; 32],
) -> BarrierObservationSignature {
    BarrierObservationSignature {
        issuer: signer.binding.principal_id,
        control_domain_id: signer.binding.control_domain_id,
        key_id: signer.binding.key_id,
        signature: signer.key.sign(&message_digest).to_bytes(),
    }
}

#[test]
fn signed_observation_persists_verifier_proven_signer_and_replays() {
    let database = TestDatabase::new("happy-path");
    let identity_root = IdentityRoot::new("happy-path");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x21, KeyPurpose::BarrierObservationSigning);
    let fence = fence_takeover(&authority, 0x21);

    let request = barrier_request(&fence, 220);
    let signature = barrier_signature(&signer, observation_digest(&fence));
    let record = authority
        .record_authority_takeover_barrier_receipt_signed(&identity, request, signature)
        .expect("record signed barrier observation");
    let expected_signer = AuthorityTakeoverBarrierSigner {
        principal_id: signer.binding.principal_id,
        control_domain_id: signer.binding.control_domain_id,
        key_id: signer.binding.key_id,
        key_generation: signer.binding.key_generation,
        signature: signature.signature,
    };
    assert_eq!(record.signer, Some(expected_signer));
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_receipts(fence.takeover_receipt_id)
            .expect("inspect barrier observations"),
        vec![record]
    );
    let coverage = authority
        .inspect_authority_takeover_barrier_coverage(fence.takeover_receipt_id)
        .expect("barrier coverage");
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    assert!(coverage.missing_participants.is_empty());
    assert_eq!(
        authority
            .record_authority_takeover_barrier_receipt_signed(&identity, request, signature)
            .expect("byte-equal signed replay"),
        record
    );
}

#[test]
fn semantic_signing_key_is_rejected_and_writes_no_row() {
    let database = TestDatabase::new("wrong-purpose");
    let identity_root = IdentityRoot::new("wrong-purpose");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x22, KeyPurpose::SemanticSigning);
    let fence = fence_takeover(&authority, 0x22);

    let request = barrier_request(&fence, 220);
    let signature = barrier_signature(&signer, observation_digest(&fence));
    assert!(matches!(
        authority.record_authority_takeover_barrier_receipt_signed(&identity, request, signature),
        Err(TaskStoreError::BarrierSignerIdentityAuthority(
            nlos_identity::IdentityAuthorityError::KeyPurposeMismatch
        ))
    ));
    assert!(
        authority
            .inspect_authority_takeover_barrier_receipts(fence.takeover_receipt_id)
            .expect("inspect barrier observations")
            .is_empty()
    );
    let coverage = authority
        .inspect_authority_takeover_barrier_coverage(fence.takeover_receipt_id)
        .expect("barrier coverage");
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::Partial
    );
    assert_eq!(coverage.missing_participants, vec![fence.participant]);
}

#[test]
fn signature_over_other_material_is_rejected_and_writes_no_row() {
    let database = TestDatabase::new("invalid-signature");
    let identity_root = IdentityRoot::new("invalid-signature");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x23, KeyPurpose::BarrierObservationSigning);
    let fence = fence_takeover(&authority, 0x23);

    let request = barrier_request(&fence, 220);
    let mut foreign_digest = observation_digest(&fence);
    foreign_digest[0] ^= 1;
    let signature = barrier_signature(&signer, foreign_digest);
    assert!(matches!(
        authority.record_authority_takeover_barrier_receipt_signed(&identity, request, signature),
        Err(TaskStoreError::BarrierSignerIdentityAuthority(
            nlos_identity::IdentityAuthorityError::InvalidSignature
        ))
    ));
    assert!(
        authority
            .inspect_authority_takeover_barrier_receipts(fence.takeover_receipt_id)
            .expect("inspect barrier observations")
            .is_empty()
    );
}

#[test]
fn foreign_issuer_binding_is_rejected_and_writes_no_row() {
    let database = TestDatabase::new("binding-mismatch");
    let identity_root = IdentityRoot::new("binding-mismatch");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x24, KeyPurpose::BarrierObservationSigning);
    let fence = fence_takeover(&authority, 0x24);

    let request = barrier_request(&fence, 220);
    let mut signature = barrier_signature(&signer, observation_digest(&fence));
    signature.issuer = PrincipalId::from_bytes([0xee; 16]);
    assert!(matches!(
        authority.record_authority_takeover_barrier_receipt_signed(&identity, request, signature),
        Err(TaskStoreError::BarrierSignerIdentityAuthority(
            nlos_identity::IdentityAuthorityError::SignerBindingMismatch
        ))
    ));
    assert!(
        authority
            .inspect_authority_takeover_barrier_receipts(fence.takeover_receipt_id)
            .expect("inspect barrier observations")
            .is_empty()
    );
}

#[test]
fn unsigned_then_signed_replay_of_the_same_observation_fails_closed() {
    let database = TestDatabase::new("mixed-replay");
    let identity_root = IdentityRoot::new("mixed-replay");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x25, KeyPurpose::BarrierObservationSigning);
    let fence = fence_takeover(&authority, 0x25);

    let request = barrier_request(&fence, 220);
    let unsigned = authority
        .record_authority_takeover_barrier_receipt(request)
        .expect("record unsigned barrier observation");
    assert_eq!(unsigned.signer, None);
    let signature = barrier_signature(&signer, observation_digest(&fence));
    assert!(matches!(
        authority.record_authority_takeover_barrier_receipt_signed(&identity, request, signature),
        Err(TaskStoreError::CorruptRecord(
            "takeover barrier receipt changed during replay"
        ))
    ));
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_receipts(fence.takeover_receipt_id)
            .expect("original unsigned row intact"),
        vec![unsigned]
    );
    assert_eq!(
        authority
            .record_authority_takeover_barrier_receipt(request)
            .expect("unsigned replay still succeeds"),
        unsigned
    );
}

#[test]
fn signed_observation_survives_restart_byte_equally() {
    let database = TestDatabase::new("restart");
    let identity_root = IdentityRoot::new("restart");
    let record = {
        let authority = database.open();
        let identity = IdentityAuthority::open(identity_root.path()).unwrap();
        let signer =
            bootstrap_barrier_signer(&identity, 0x26, KeyPurpose::BarrierObservationSigning);
        let fence = fence_takeover(&authority, 0x26);
        let request = barrier_request(&fence, 220);
        let signature = barrier_signature(&signer, observation_digest(&fence));
        authority
            .record_authority_takeover_barrier_receipt_signed(&identity, request, signature)
            .expect("record signed barrier observation")
    };

    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_authority_takeover_barrier_receipts(record.takeover_receipt_id)
            .expect("barrier observations after restart"),
        vec![record]
    );
}

#[test]
fn v35_takeover_barrier_schema_migrates_signer_columns() {
    let database = TestDatabase::new("migration-v36");
    let identity_root = IdentityRoot::new("migration-v36");
    let unsigned = {
        let authority = database.open();
        let identity = IdentityAuthority::open(identity_root.path()).unwrap();
        let signer =
            bootstrap_barrier_signer(&identity, 0x27, KeyPurpose::BarrierObservationSigning);
        let fence = fence_takeover(&authority, 0x27);
        assert_eq!(
            signer.binding.key_purpose,
            KeyPurpose::BarrierObservationSigning
        );
        let request = barrier_request(&fence, 220);
        authority
            .record_authority_takeover_barrier_receipt(request)
            .expect("record legacy unsigned observation")
    };
    assert_eq!(unsigned.signer, None);

    let raw = Connection::open(&database.path).expect("raw schema database");
    raw.execute_batch(
        "DROP TRIGGER task_authority_takeover_barrier_receipts_signer_coupled;
         ALTER TABLE task_authority_takeover_barrier_receipts DROP COLUMN signer_principal_id;
         ALTER TABLE task_authority_takeover_barrier_receipts DROP COLUMN signer_control_domain_id;
         ALTER TABLE task_authority_takeover_barrier_receipts DROP COLUMN signer_key_id;
         ALTER TABLE task_authority_takeover_barrier_receipts DROP COLUMN signer_key_generation;
         ALTER TABLE task_authority_takeover_barrier_receipts DROP COLUMN signer_signature;
         PRAGMA user_version = 35;",
    )
    .expect("construct v35 barrier schema");
    drop(raw);

    drop(database.open());
    let raw = Connection::open(&database.path).expect("migrated schema database");
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read migrated schema version");
    assert_eq!(version, 41);
    let signer_column_count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('task_authority_takeover_barrier_receipts')
             WHERE name IN (
                 'signer_principal_id', 'signer_control_domain_id', 'signer_key_id',
                 'signer_key_generation', 'signer_signature'
             )",
            [],
            |row| row.get(0),
        )
        .expect("inspect migrated signer columns");
    assert_eq!(signer_column_count, 5);
    drop(raw);

    let migrated = database.open();
    let readback: Vec<AuthorityTakeoverBarrierReceiptRecord> = migrated
        .inspect_authority_takeover_barrier_receipts(unsigned.takeover_receipt_id)
        .expect("legacy unsigned observations after migration");
    assert_eq!(readback, vec![unsigned]);
    assert_eq!(readback[0].signer, None);
}
