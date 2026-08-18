//! B-TASK-008C2G takeover completion + successor assignment activation
//! (schema v37) acceptance tests.
//!
//! Each test proves one slice of the completion gate: happy-path activation
//! with restart readback, byte-equal replay, the unsigned-observation and
//! partial/missing-manifest rejections, stale/expired/superseded lease
//! rejections, the narrowed immutable trigger's fail-closed surface, the
//! v36 → v37 migration with FK integrity, and one fail-closed
//! fault-injection row (hard I/O error leaves no half state). The tests do
//! not claim IPC peer authentication, remote barrier truth, cross-term
//! adoption, or fresh endpoint attestation after registry reopen.
//!
//! The fault VFS state in `nlos-store-fault` is process-global, so every
//! test in this binary holds `COMPLETION_LOCK` for its entire duration.

use std::error::Error as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_task::{
    AuthorityAssignmentState, AuthorityLeaseDecision, AuthorityLeaseFinalizeRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseRecord, AuthorityLeaseRequest,
    AuthorityLeaseTakeoverFenceRequest, AuthoritySuccessorRegistryReopenRecord,
    AuthoritySuccessorRegistryReopenRequest, AuthorityTakeoverBarrierReceiptRequest,
    AuthorityTakeoverBarrierReceiptState, AuthorityTakeoverCompletionRecord,
    AuthorityTakeoverReceiptRecord, AuthorityTakeoverReceiptState, BarrierObservationSignature,
    CompleteAuthorityTakeoverRequest, FinalizeRequest, FinalizeRequestV3, ParticipantRecord,
    ParticipantRegistryBinding, ParticipantRegistryState, PermitDecision, PermitRequest,
    SnapshotBundle, SqliteTaskAuthority, TaskSpec, TaskStoreError,
    barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use rusqlite::Connection;

static COMPLETION_LOCK: Mutex<()> = Mutex::new(());

fn completion_lock() -> MutexGuard<'static, ()> {
    COMPLETION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static NEXT: AtomicU64 = AtomicU64::new(1);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-takeover-completion-{name}-{}-{sequence}.sqlite3",
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
            "nlos-task-takeover-completion-identity-{label}-{nonce}-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
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

fn lease_request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: nlos_types::ProcessId::from_bytes([key.wrapping_add(holder); 16]),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: at_ms,
        ttl_ms,
    }
}

fn lease_record(decision: AuthorityLeaseDecision) -> AuthorityLeaseRecord {
    decision.record()
}

fn register_task_attempt(authority: &SqliteTaskAuthority, seed: u8) -> nlos_task::AttemptSpec {
    let task_id = TaskId::from_bytes([seed; 16]);
    authority
        .register_task(TaskSpec {
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
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(2); 16]),
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

/// The committed prefix every completion scenario starts from: term-1
/// lease, lease-bound permit (Active assignment baseline), optional
/// finalize, term-2 takeover lease, and the frozen takeover fence.
struct PendingTakeover {
    attempt: nlos_task::AttemptSpec,
    registry_binding: ParticipantRegistryBinding,
    lease_two: AuthorityLeaseRecord,
    takeover: AuthorityTakeoverReceiptRecord,
    fence_members: Vec<ParticipantRecord>,
}

fn seed_pending_takeover(
    authority: &SqliteTaskAuthority,
    seed: u8,
    finalize_first_permit: bool,
) -> PendingTakeover {
    let lease_one = lease_record(
        authority
            .acquire_authority_lease(lease_request(1, seed, 100, 100))
            .expect("initial lease"),
    );
    let attempt = register_task_attempt(authority, seed);
    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&attempt, seed.wrapping_add(0x21), 150),
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
    if finalize_first_permit {
        authority
            .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
                finalize: finalize_request(&attempt, permit.permit_id, 160),
                lease: lease_one,
            })
            .expect("close first permit before takeover");
    }
    let lease_two = lease_record(
        authority
            .acquire_authority_lease(lease_request(2, seed.wrapping_add(0x31), 201, 100))
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
    assert_eq!(frozen.state, ParticipantRegistryState::FrozenForTakeover);
    let fence_receipt = authority
        .inspect_authority_takeover_fence_receipt(attempt.task_id, registry_binding)
        .expect("takeover fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(attempt.task_id, fence_receipt.receipt_id)
        .expect("pending takeover receipt");
    assert_eq!(
        takeover.barrier_state,
        AuthorityTakeoverReceiptState::Pending
    );
    assert_eq!(takeover.new_assignment_id, None);
    let fence_members = authority
        .inspect_authority_takeover_fence_members(attempt.task_id, registry_binding)
        .expect("fence member manifest");
    assert_eq!(
        !fence_members.is_empty(),
        fence_receipt.exact_fence_set_root.is_some(),
        "the member manifest exists exactly when the exact fence root is resolvable"
    );
    PendingTakeover {
        attempt,
        registry_binding,
        lease_two,
        takeover,
        fence_members: fence_members
            .into_iter()
            .map(|member| member.participant)
            .collect(),
    }
}

/// Records a principal-signed observation for one manifest member.
fn record_signed_observation(
    authority: &SqliteTaskAuthority,
    identity: &IdentityAuthority,
    signer: &BarrierSigner,
    takeover: &AuthorityTakeoverReceiptRecord,
    participant: ParticipantRecord,
) -> nlos_task::AuthorityTakeoverBarrierReceiptRecord {
    let request = AuthorityTakeoverBarrierReceiptRequest {
        takeover_receipt_id: takeover.receipt_id,
        participant,
        remote_receipt_id: ReceiptId::from_bytes([0x91; 16]),
        barrier_digest: [0x92; 32],
        observed_at_ms: 220,
    };
    let message_digest = barrier_observation_signature_message(
        takeover.receipt_id,
        &participant,
        request.remote_receipt_id,
        request.barrier_digest,
        takeover.exact_fence_set_root.expect("exact fence set root"),
    );
    let signature = BarrierObservationSignature {
        issuer: signer.binding.principal_id,
        control_domain_id: signer.binding.control_domain_id,
        key_id: signer.binding.key_id,
        signature: signer.key.sign(&message_digest).to_bytes(),
    };
    let record = authority
        .record_authority_takeover_barrier_receipt_signed(identity, request, signature)
        .expect("record signed barrier observation");
    assert_eq!(record.state, AuthorityTakeoverBarrierReceiptState::Observed);
    assert!(record.signer.is_some());
    record
}

fn record_all_signed(
    authority: &SqliteTaskAuthority,
    identity: &IdentityAuthority,
    signer: &BarrierSigner,
    pending: &PendingTakeover,
) -> Vec<nlos_task::AuthorityTakeoverBarrierReceiptRecord> {
    pending
        .fence_members
        .iter()
        .map(|participant| {
            record_signed_observation(authority, identity, signer, &pending.takeover, *participant)
        })
        .collect()
}

fn completion_request(
    pending: &PendingTakeover,
    completed_at_ms: i64,
) -> CompleteAuthorityTakeoverRequest {
    CompleteAuthorityTakeoverRequest {
        takeover_receipt_id: pending.takeover.receipt_id,
        lease: pending.lease_two,
        completed_at_ms,
    }
}

fn raw_count(path: &Path, table: &str) -> i64 {
    let connection = Connection::open(path).expect("open raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn raw_assignment_state(path: &Path, assignment_id: &[u8]) -> i64 {
    let connection = Connection::open(path).expect("open raw reader");
    connection
        .query_row(
            "SELECT assignment_state FROM task_authority_assignments WHERE assignment_id = ?1",
            [assignment_id],
            |row| row.get(0),
        )
        .expect("read old assignment state")
}

fn hex_encode(value: &[u8]) -> String {
    use std::fmt::Write as _;
    value
        .iter()
        .fold(String::with_capacity(value.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the complete activation lifecycle.
fn completion_activates_successor_assignment_and_survives_restart() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("happy-path");
    let identity_root = IdentityRoot::new("happy-path");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x41, KeyPurpose::BarrierObservationSigning);
    let pending = seed_pending_takeover(&authority, 0x41, true);
    record_all_signed(&authority, &identity, &signer, &pending);

    let completed = authority
        .complete_authority_takeover(completion_request(&pending, 230))
        .expect("complete takeover");
    assert_eq!(
        completed,
        AuthorityTakeoverCompletionRecord {
            takeover_receipt_id: pending.takeover.receipt_id,
            task_id: pending.attempt.task_id,
            old_assignment_id: pending.takeover.old_assignment_id,
            new_assignment_id: completed.new_assignment_id,
            barrier_state: AuthorityTakeoverReceiptState::Complete,
            completed_at_ms: 230,
        }
    );

    let receipt = authority
        .inspect_authority_takeover_receipt(
            pending.attempt.task_id,
            pending.takeover.fence_receipt_id,
        )
        .expect("completed takeover receipt");
    assert_eq!(
        receipt.barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );
    assert_eq!(receipt.new_assignment_id, Some(completed.new_assignment_id));
    assert_eq!(
        receipt.old_assignment_id,
        pending.takeover.old_assignment_id
    );

    let successor = authority
        .inspect_authority_assignment(pending.attempt.task_id)
        .expect("successor assignment");
    assert_eq!(successor.state, AuthorityAssignmentState::Active);
    assert_eq!(successor.assignment_id, completed.new_assignment_id);
    assert_eq!(
        successor.authority_lease_binding,
        pending.lease_two.binding()
    );
    assert_eq!(successor.control_epoch, pending.takeover.new_control_epoch);
    assert_eq!(
        successor.participant_registry_binding,
        pending.registry_binding
    );
    assert_eq!(successor.created_at_ms, 230);

    assert_eq!(
        raw_assignment_state(
            &database.path,
            pending.takeover.old_assignment_id.as_bytes(),
        ),
        3,
        "old assignment must be durably Fenced"
    );
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 2);
    assert_eq!(
        authority
            .inspect_participant_registry(pending.attempt.task_id)
            .expect("registry after completion")
            .state,
        ParticipantRegistryState::FrozenForTakeover,
        "registry stays frozen after completion in this slice"
    );
    let task_head = authority
        .inspect_task(pending.attempt.task_id)
        .expect("task head after completion");
    let second_attempt = nlos_task::AttemptSpec {
        task_id: pending.attempt.task_id,
        attempt_id: TaskAttemptId::from_bytes([0x52; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x53; 16]),
            snapshot_digest: [0x54; 32],
            expected_head_commit_seq: task_head.head_commit_seq,
            effect_history_root: task_head.head_effect_history_root,
            retry_fence_epoch: task_head.retry_fence_epoch,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x55; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x56; 16]),
        registered_at_ms: 240,
    };
    assert!(matches!(
        authority.register_attempt(second_attempt),
        Ok(nlos_task::AttemptRegistrationDecision::Created(_))
    ));
    assert!(matches!(
        authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&second_attempt, 0x57, 241),
            lease: pending.lease_two,
        }),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: ParticipantRegistryState::FrozenForTakeover
        })
    ));

    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_authority_takeover_receipt(
                pending.attempt.task_id,
                pending.takeover.fence_receipt_id
            )
            .expect("completed receipt after restart"),
        receipt
    );
    assert_eq!(
        reopened
            .inspect_authority_assignment(pending.attempt.task_id)
            .expect("successor assignment after restart"),
        successor
    );
}

#[test]
fn completion_replay_returns_byte_equal_record_and_mutates_nothing() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("replay");
    let identity_root = IdentityRoot::new("replay");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x42, KeyPurpose::BarrierObservationSigning);
    let pending = seed_pending_takeover(&authority, 0x42, true);
    let observations = record_all_signed(&authority, &identity, &signer, &pending);
    let control_epoch = authority
        .inspect_task(pending.attempt.task_id)
        .expect("task")
        .control_epoch;

    let completed = authority
        .complete_authority_takeover(completion_request(&pending, 230))
        .expect("complete takeover");
    assert_eq!(
        authority
            .complete_authority_takeover(completion_request(&pending, 230))
            .expect("identical replay"),
        completed
    );
    assert_eq!(
        authority
            .inspect_task(pending.attempt.task_id)
            .expect("task after replay")
            .control_epoch,
        control_epoch,
        "replay must not advance the control epoch"
    );
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 2);
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_receipts(pending.takeover.receipt_id)
            .expect("observations after replay"),
        observations
    );

    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened
            .complete_authority_takeover(completion_request(&pending, 230))
            .expect("replay after restart"),
        completed,
        "replay across restart must return the byte-equal durable record"
    );
}

#[test]
fn unsigned_observation_blocks_completion_fail_closed() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("unsigned");
    let authority = database.open();
    let pending = seed_pending_takeover(&authority, 0x43, true);
    for participant in &pending.fence_members {
        authority
            .record_authority_takeover_barrier_receipt(AuthorityTakeoverBarrierReceiptRequest {
                takeover_receipt_id: pending.takeover.receipt_id,
                participant: *participant,
                remote_receipt_id: ReceiptId::from_bytes([0x91; 16]),
                barrier_digest: [0x92; 32],
                observed_at_ms: 220,
            })
            .expect("record unsigned barrier observation");
    }
    let coverage = authority
        .inspect_authority_takeover_barrier_coverage(pending.takeover.receipt_id)
        .expect("coverage is locally complete");
    assert_eq!(
        coverage.state,
        nlos_task::AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );

    assert!(matches!(
        authority.complete_authority_takeover(completion_request(&pending, 230)),
        Err(TaskStoreError::BarrierObservationUnsigned)
    ));
    let receipt = authority
        .inspect_authority_takeover_receipt(
            pending.attempt.task_id,
            pending.takeover.fence_receipt_id,
        )
        .expect("receipt after rejection");
    assert_eq!(
        receipt.barrier_state,
        AuthorityTakeoverReceiptState::Pending
    );
    assert_eq!(receipt.new_assignment_id, None);
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 1);
    assert_eq!(
        raw_assignment_state(
            &database.path,
            pending.takeover.old_assignment_id.as_bytes(),
        ),
        2,
        "old assignment must remain TakeoverPending"
    );
}

/// T4 uses the `ManifestUnavailable` shape: an outstanding permit whose
/// write-set mapping is incomplete leaves the exact fence root `NULL`, so
/// the fence manifest cannot even be resolved. Building a ≥2-member
/// manifest instead requires an owner-verified endpoint participant path,
/// which is disproportionate to this gate; the missing-manifest branch is
/// the same inline-coverage entry point.
#[test]
fn unavailable_manifest_blocks_completion_fail_closed() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("manifest-unavailable");
    let authority = database.open();
    let pending = seed_pending_takeover(&authority, 0x44, false);
    assert!(pending.takeover.exact_fence_set_root.is_none());
    let coverage = authority
        .inspect_authority_takeover_barrier_coverage(pending.takeover.receipt_id)
        .expect("coverage view");
    assert_eq!(
        coverage.state,
        nlos_task::AuthorityTakeoverBarrierCoverageState::ManifestUnavailable
    );

    assert!(matches!(
        authority.complete_authority_takeover(completion_request(&pending, 230)),
        Err(TaskStoreError::CorruptRecord(
            "takeover fence set root is incomplete"
        ))
    ));
    let receipt = authority
        .inspect_authority_takeover_receipt(
            pending.attempt.task_id,
            pending.takeover.fence_receipt_id,
        )
        .expect("receipt after rejection");
    assert_eq!(
        receipt.barrier_state,
        AuthorityTakeoverReceiptState::Pending
    );
    assert_eq!(receipt.new_assignment_id, None);
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 1);
}

#[test]
fn expired_successor_lease_rejects_completion_without_mutation() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("expired");
    let identity_root = IdentityRoot::new("expired");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x45, KeyPurpose::BarrierObservationSigning);
    let pending = seed_pending_takeover(&authority, 0x45, true);
    record_all_signed(&authority, &identity, &signer, &pending);

    assert!(pending.lease_two.expires_at_ms <= 350);
    assert!(matches!(
        authority.complete_authority_takeover(completion_request(&pending, 350)),
        Err(TaskStoreError::AuthorityLeaseExpired)
    ));
    let receipt = authority
        .inspect_authority_takeover_receipt(
            pending.attempt.task_id,
            pending.takeover.fence_receipt_id,
        )
        .expect("receipt after rejection");
    assert_eq!(
        receipt.barrier_state,
        AuthorityTakeoverReceiptState::Pending
    );
    assert_eq!(receipt.new_assignment_id, None);
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 1);
}

#[test]
fn superseded_successor_lease_rejects_completion_without_mutation() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("superseded");
    let identity_root = IdentityRoot::new("superseded");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x46, KeyPurpose::BarrierObservationSigning);
    let pending = seed_pending_takeover(&authority, 0x46, true);
    record_all_signed(&authority, &identity, &signer, &pending);

    let lease_three = lease_record(
        authority
            .acquire_authority_lease(lease_request(3, 0x46 + 0x41, 401, 100))
            .expect("supersede the successor lease"),
    );
    assert_eq!(lease_three.term, pending.lease_two.term + 1);
    assert!(matches!(
        authority.complete_authority_takeover(completion_request(&pending, 410)),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    assert!(matches!(
        authority.complete_authority_takeover(CompleteAuthorityTakeoverRequest {
            takeover_receipt_id: pending.takeover.receipt_id,
            lease: lease_three,
            completed_at_ms: 410,
        }),
        Err(TaskStoreError::AuthorityLeaseBindingMismatch)
    ));
    let receipt = authority
        .inspect_authority_takeover_receipt(
            pending.attempt.task_id,
            pending.takeover.fence_receipt_id,
        )
        .expect("receipt after rejection");
    assert_eq!(
        receipt.barrier_state,
        AuthorityTakeoverReceiptState::Pending
    );
    assert_eq!(receipt.new_assignment_id, None);
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 1);
}

#[test]
fn narrowed_trigger_permits_only_the_completion_transition() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("narrowed-trigger");
    let identity_root = IdentityRoot::new("narrowed-trigger");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x47, KeyPurpose::BarrierObservationSigning);
    let pending = seed_pending_takeover(&authority, 0x47, true);

    let receipt_hex = hex_encode(pending.takeover.receipt_id.as_bytes());
    let raw = Connection::open(&database.path).expect("raw pre-completion connection");
    assert!(
        raw.execute_batch(&format!(
            "UPDATE task_authority_takeover_receipts
             SET new_assignment_id = zeroblob(16)
             WHERE receipt_id = X'{receipt_hex}'"
        ))
        .is_err(),
        "bare new_assignment_id write without the barrier transition must still abort"
    );
    drop(raw);

    record_all_signed(&authority, &identity, &signer, &pending);
    authority
        .complete_authority_takeover(completion_request(&pending, 230))
        .expect("complete takeover");

    let raw = Connection::open(&database.path).expect("raw post-completion connection");
    for statement in [
        format!(
            "UPDATE task_authority_takeover_receipts SET barrier_state = 1
             WHERE receipt_id = X'{receipt_hex}'"
        ),
        format!(
            "UPDATE task_authority_takeover_receipts SET new_assignment_id = zeroblob(16)
             WHERE receipt_id = X'{receipt_hex}'"
        ),
        format!(
            "UPDATE task_authority_takeover_receipts
             SET created_at_ms = created_at_ms + 1
             WHERE receipt_id = X'{receipt_hex}'"
        ),
        format!(
            "UPDATE task_authority_takeover_receipts
             SET exact_fence_set_root = zeroblob(32)
             WHERE receipt_id = X'{receipt_hex}'"
        ),
        format!("DELETE FROM task_authority_takeover_receipts WHERE receipt_id = X'{receipt_hex}'"),
    ] {
        assert!(
            raw.execute_batch(&statement).is_err(),
            "narrowed trigger must abort: {statement}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full v36 → v37 migration contract.
fn v36_schema_migrates_to_v37_with_child_fk_intact_and_activates() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("migration-v37");
    let identity_root = IdentityRoot::new("migration-v37");
    let (pending, observations) = {
        let authority = database.open();
        let identity = IdentityAuthority::open(identity_root.path()).unwrap();
        let signer =
            bootstrap_barrier_signer(&identity, 0x48, KeyPurpose::BarrierObservationSigning);
        let pending = seed_pending_takeover(&authority, 0x48, true);
        let observations = record_all_signed(&authority, &identity, &signer, &pending);
        (pending, observations)
    };

    // Manually rebuild the v36 shape: CHECK(new_assignment_id IS NULL),
    // CHECK(barrier_state = 1), and the blanket immutable trigger. The
    // table is an FK parent of the durable barrier observations, so the
    // copy runs with FK enforcement relaxed exactly like the v37 migration
    // itself does.
    let raw = Connection::open(&database.path).expect("raw schema database");
    raw.execute_batch(
        "PRAGMA foreign_keys = OFF;
         DROP TRIGGER task_authority_takeover_receipt_immutable;
         DROP TRIGGER task_authority_takeover_receipt_no_delete;
         CREATE TABLE task_authority_takeover_receipts_v36 (
             receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
             task_id BLOB NOT NULL CHECK(length(task_id) = 16),
             task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
             old_assignment_id BLOB NOT NULL CHECK(length(old_assignment_id) = 16),
             new_assignment_id BLOB CHECK(new_assignment_id IS NULL),
             fence_receipt_id BLOB NOT NULL CHECK(length(fence_receipt_id) = 16),
             frozen_old_authority_term BLOB NOT NULL CHECK(length(frozen_old_authority_term) = 8),
             frozen_old_control_epoch BLOB NOT NULL CHECK(length(frozen_old_control_epoch) = 8),
             new_authority_id BLOB NOT NULL CHECK(length(new_authority_id) = 16),
             new_authority_lease_holder_id BLOB NOT NULL
                 CHECK(length(new_authority_lease_holder_id) = 16),
             new_authority_lease_term BLOB NOT NULL CHECK(length(new_authority_lease_term) = 8),
             new_authority_lease_epoch BLOB NOT NULL CHECK(length(new_authority_lease_epoch) = 8),
             new_authority_lease_fencing_token BLOB NOT NULL
                 CHECK(length(new_authority_lease_fencing_token) = 32),
             new_authority_lease_expires_at_ms INTEGER NOT NULL
                 CHECK(new_authority_lease_expires_at_ms >= 0),
             new_control_epoch BLOB NOT NULL CHECK(length(new_control_epoch) = 8),
             frozen_registry_generation BLOB NOT NULL
                 CHECK(length(frozen_registry_generation) = 8),
             frozen_registry_root BLOB NOT NULL CHECK(length(frozen_registry_root) = 32),
             exact_fence_set_root BLOB
                 CHECK(exact_fence_set_root IS NULL OR length(exact_fence_set_root) = 32),
             outstanding_operation_participant_root BLOB
                 CHECK(outstanding_operation_participant_root IS NULL
                       OR length(outstanding_operation_participant_root) = 32),
             barrier_state INTEGER NOT NULL CHECK(barrier_state = 1),
             created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
             UNIQUE(task_id, old_assignment_id, fence_receipt_id),
             FOREIGN KEY(task_id) REFERENCES tasks(task_id),
             FOREIGN KEY(old_assignment_id)
                 REFERENCES task_authority_assignments(assignment_id),
             FOREIGN KEY(fence_receipt_id)
                 REFERENCES task_authority_takeover_fence_receipts(receipt_id)
          ) STRICT;
         INSERT INTO task_authority_takeover_receipts_v36 (
             receipt_id, task_id, task_generation, old_assignment_id,
             new_assignment_id, fence_receipt_id, frozen_old_authority_term,
             frozen_old_control_epoch, new_authority_id,
             new_authority_lease_holder_id, new_authority_lease_term,
             new_authority_lease_epoch, new_authority_lease_fencing_token,
             new_authority_lease_expires_at_ms, new_control_epoch,
             frozen_registry_generation, frozen_registry_root,
             exact_fence_set_root, outstanding_operation_participant_root,
             barrier_state, created_at_ms
         )
         SELECT receipt_id, task_id, task_generation, old_assignment_id,
                new_assignment_id, fence_receipt_id, frozen_old_authority_term,
                frozen_old_control_epoch, new_authority_id,
                new_authority_lease_holder_id, new_authority_lease_term,
                new_authority_lease_epoch, new_authority_lease_fencing_token,
                new_authority_lease_expires_at_ms, new_control_epoch,
                frozen_registry_generation, frozen_registry_root,
                exact_fence_set_root, outstanding_operation_participant_root,
                barrier_state, created_at_ms
         FROM task_authority_takeover_receipts;
         DROP TABLE task_authority_takeover_receipts;
         ALTER TABLE task_authority_takeover_receipts_v36
             RENAME TO task_authority_takeover_receipts;
         CREATE TRIGGER task_authority_takeover_receipt_immutable
         BEFORE UPDATE ON task_authority_takeover_receipts
         BEGIN
             SELECT RAISE(ABORT, 'task authority takeover receipt is immutable');
         END;
         CREATE TRIGGER task_authority_takeover_receipt_no_delete
         BEFORE DELETE ON task_authority_takeover_receipts
         BEGIN
             SELECT RAISE(ABORT, 'task authority takeover receipt is durable evidence');
         END;
         PRAGMA user_version = 36;",
    )
    .expect("construct v36 takeover schema");
    drop(raw);

    let authority = database.open();
    let raw = Connection::open(&database.path).expect("migrated schema database");
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read migrated schema version");
    assert_eq!(version, 38);
    let trigger_sql: String = raw
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type='trigger' AND name='task_authority_takeover_receipt_immutable'",
            [],
            |row| row.get(0),
        )
        .expect("read narrowed trigger");
    assert!(
        trigger_sql.contains("OLD.barrier_state") && trigger_sql.contains("NEW.barrier_state"),
        "migration must restore the narrowed immutable trigger"
    );
    let foreign_key_violations: i64 = raw
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .expect("run foreign_key_check");
    assert_eq!(
        foreign_key_violations, 0,
        "child FKs must resolve after copy"
    );
    raw.pragma_update(None, "foreign_keys", "ON")
        .expect("enable FK enforcement");
    raw.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         INSERT INTO task_authority_takeover_barrier_receipts (
             receipt_id, takeover_receipt_id, task_id, task_generation,
             participant_type, participant_id, participant_generation,
             admission_receipt_id, remote_receipt_id, barrier_receipt_digest,
             fence_set_root, barrier_state, observed_at_ms,
             signer_principal_id, signer_control_domain_id, signer_key_id,
             signer_key_generation, signer_signature
         ) VALUES (
             X'{probe_receipt}', X'{takeover_receipt}', X'{task_id}', X'{task_generation}',
             1, X'{probe_participant}', X'0000000000000001',
             X'77777777777777777777777777777777', X'cccccccccccccccccccccccccccccccc',
             NULL, X'{fence_set_root}', 1, 230,
             NULL, NULL, NULL, NULL, NULL
         );
         ROLLBACK;",
        probe_receipt = "ee".repeat(16),
        takeover_receipt = hex_encode(pending.takeover.receipt_id.as_bytes()),
        task_id = hex_encode(pending.attempt.task_id.as_bytes()),
        task_generation = hex_encode(&pending.takeover.task_generation.get().to_be_bytes()),
        probe_participant = "66".repeat(16),
        fence_set_root = hex_encode(
            pending
                .takeover
                .exact_fence_set_root
                .expect("exact fence set root")
                .as_slice()
        ),
    ))
    .expect("child FK must resolve against the migrated parent inside a transaction");
    drop(raw);

    assert_eq!(
        authority
            .inspect_authority_takeover_receipt(
                pending.attempt.task_id,
                pending.takeover.fence_receipt_id
            )
            .expect("pending receipt survives migration byte-equal"),
        pending.takeover
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_receipts(pending.takeover.receipt_id)
            .expect("observations survive migration byte-equal"),
        observations
    );
    let completed = authority
        .complete_authority_takeover(completion_request(&pending, 230))
        .expect("activation succeeds on the migrated database");
    assert_eq!(
        authority
            .inspect_authority_assignment(pending.attempt.task_id)
            .expect("successor on migrated database")
            .assignment_id,
        completed.new_assignment_id
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_receipt(
                pending.attempt.task_id,
                pending.takeover.fence_receipt_id
            )
            .expect("receipt after activation")
            .barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );
}

const VFS_NAME: &str = "nlos-task-takeover-completion-fault";

fn error_chain(error: &TaskStoreError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Fail-closed fault row (the full completion F1-F6 matrix is the next
/// slice): a hard I/O error on the completion transaction must surface a
/// typed storage failure, leave zero half state (receipt still `Pending`,
/// old assignment still `TakeoverPending`, no successor row), and the same
/// completion must succeed after the fault is disarmed.
#[test]
fn fault_io_error_on_completion_writes_fails_closed() {
    let _serialization = completion_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("ioerr-completion");
    let identity_root = IdentityRoot::new("ioerr-completion");
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    let authority = SqliteTaskAuthority::open_with_vfs(&database.path, Some(VFS_NAME))
        .expect("open via fault vfs");
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x49, KeyPurpose::BarrierObservationSigning);
    let pending = seed_pending_takeover(&authority, 0x49, true);
    record_all_signed(&authority, &identity, &signer, &pending);

    nlos_store_fault::arm(nlos_store_fault::FaultMode::FailWritesAfter {
        remaining: 0,
        code: nlos_store_fault::FaultCode::IoErr,
    });
    let error = authority
        .complete_authority_takeover(completion_request(&pending, 230))
        .expect_err("completion must fail under injected I/O error");
    assert!(matches!(error, TaskStoreError::Sqlite(_)));
    let chain = error_chain(&error).to_lowercase();
    assert!(
        chain.contains("i/o") || chain.contains("ioerr"),
        "error chain must name the injected condition, got: {chain}"
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_receipt(
                pending.attempt.task_id,
                pending.takeover.fence_receipt_id
            )
            .expect("receipt after fault")
            .barrier_state,
        AuthorityTakeoverReceiptState::Pending,
        "failed completion must leave the receipt pending"
    );
    assert_eq!(
        raw_assignment_state(
            &database.path,
            pending.takeover.old_assignment_id.as_bytes(),
        ),
        2,
        "failed completion must leave the old assignment TakeoverPending"
    );
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 1);

    nlos_store_fault::disarm();
    let completed = authority
        .complete_authority_takeover(completion_request(&pending, 230))
        .expect("completion succeeds after disarm");
    assert_eq!(
        completed.barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 2);
}

#[test]
#[allow(clippy::too_many_lines)]
fn successor_registry_reopens_and_new_permit_uses_new_generation() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("successor-registry");
    let identity_root = IdentityRoot::new("successor-registry");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x4a, KeyPurpose::BarrierObservationSigning);
    let pending = seed_pending_takeover(&authority, 0x4a, true);
    record_all_signed(&authority, &identity, &signer, &pending);
    let completed = authority
        .complete_authority_takeover(completion_request(&pending, 230))
        .expect("complete takeover");

    let reopened = authority
        .reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
            takeover_receipt_id: pending.takeover.receipt_id,
            lease: pending.lease_two,
            reopened_at_ms: 240,
        })
        .expect("reopen successor registry");
    assert_eq!(
        reopened,
        AuthoritySuccessorRegistryReopenRecord {
            takeover_receipt_id: pending.takeover.receipt_id,
            task_id: pending.attempt.task_id,
            old_registry_binding: pending.registry_binding,
            successor_registry_binding: reopened.successor_registry_binding,
            fenced_assignment_id: completed.new_assignment_id,
            active_assignment_id: reopened.active_assignment_id,
        }
    );
    assert_eq!(
        reopened.successor_registry_binding.generation,
        pending.registry_binding.generation + 1
    );
    assert_ne!(
        reopened.successor_registry_binding.root,
        pending.registry_binding.root
    );
    let registry = authority
        .inspect_participant_registry(pending.attempt.task_id)
        .expect("successor registry");
    assert_eq!(registry.state, ParticipantRegistryState::Open);
    assert_eq!(
        ParticipantRegistryBinding {
            generation: registry.generation,
            root: registry.root,
        },
        reopened.successor_registry_binding
    );
    let successor = authority
        .inspect_authority_assignment(pending.attempt.task_id)
        .expect("rotated active assignment");
    assert_eq!(successor.assignment_id, reopened.active_assignment_id);
    assert_eq!(successor.state, AuthorityAssignmentState::Active);
    assert_eq!(
        successor.participant_registry_binding,
        reopened.successor_registry_binding
    );
    assert_eq!(
        raw_assignment_state(&database.path, completed.new_assignment_id.as_bytes()),
        3,
        "completion successor assignment must be fenced before registry rotation"
    );
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 3);

    let head = authority
        .inspect_task(pending.attempt.task_id)
        .expect("task head");
    let second_attempt = nlos_task::AttemptSpec {
        task_id: pending.attempt.task_id,
        attempt_id: TaskAttemptId::from_bytes([0x5a; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x5b; 16]),
            snapshot_digest: [0x5c; 32],
            expected_head_commit_seq: head.head_commit_seq,
            effect_history_root: head.head_effect_history_root,
            retry_fence_epoch: head.retry_fence_epoch,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x5d; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x5e; 16]),
        registered_at_ms: 241,
    };
    authority
        .register_attempt(second_attempt)
        .expect("register successor attempt");
    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&second_attempt, 0x5f, 242),
            lease: pending.lease_two,
        })
        .expect("issue successor-term permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected successor permit, got {other:?}"),
    };
    assert_eq!(
        permit.participant_registry_binding,
        Some(reopened.successor_registry_binding)
    );
    assert_eq!(
        authority
            .inspect_participant_registry(pending.attempt.task_id)
            .expect("registry after permit")
            .state,
        ParticipantRegistryState::FrozenForPermit
    );
    assert_eq!(
        authority
            .inspect_authority_assignment(pending.attempt.task_id)
            .expect("assignment after permit")
            .assignment_id,
        reopened.active_assignment_id
    );

    // Rotating the registry must not make the original completion replay
    // look corrupt; the completion fact is still durable evidence for the
    // prior successor assignment.
    assert_eq!(
        authority
            .complete_authority_takeover(completion_request(&pending, 243))
            .expect("completion replay after registry reopen"),
        completed
    );
}

#[test]
fn successor_registry_reopen_replay_is_byte_equal_after_restart() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("successor-registry-replay");
    let identity_root = IdentityRoot::new("successor-registry-replay");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x4b, KeyPurpose::BarrierObservationSigning);
    let pending = seed_pending_takeover(&authority, 0x4b, true);
    record_all_signed(&authority, &identity, &signer, &pending);
    authority
        .complete_authority_takeover(completion_request(&pending, 230))
        .expect("complete takeover");
    let first = authority
        .reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
            takeover_receipt_id: pending.takeover.receipt_id,
            lease: pending.lease_two,
            reopened_at_ms: 240,
        })
        .expect("reopen successor registry");
    assert_eq!(
        authority
            .reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
                takeover_receipt_id: pending.takeover.receipt_id,
                lease: pending.lease_two,
                reopened_at_ms: 250,
            })
            .expect("same-process replay"),
        first
    );

    drop(authority);
    let reopened_authority = database.open();
    assert_eq!(
        reopened_authority
            .reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
                takeover_receipt_id: pending.takeover.receipt_id,
                lease: pending.lease_two,
                reopened_at_ms: 400,
            })
            .expect("restart replay does not mutate"),
        first
    );
    assert_eq!(
        reopened_authority
            .inspect_participant_registry(pending.attempt.task_id)
            .expect("registry after replay")
            .state,
        ParticipantRegistryState::Open
    );
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 3);
}

#[test]
fn successor_registry_reopen_requires_completed_receipt_and_exact_lease() {
    let _serialization = completion_lock();
    let database = TestDatabase::new("successor-registry-guards");
    let identity_root = IdentityRoot::new("successor-registry-guards");
    let authority = database.open();
    let identity = IdentityAuthority::open(identity_root.path()).unwrap();
    let signer = bootstrap_barrier_signer(&identity, 0x4c, KeyPurpose::BarrierObservationSigning);
    let pending = seed_pending_takeover(&authority, 0x4c, true);
    assert!(matches!(
        authority.reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
            takeover_receipt_id: pending.takeover.receipt_id,
            lease: pending.lease_two,
            reopened_at_ms: 240,
        }),
        Err(TaskStoreError::CorruptRecord(
            "takeover receipt is not complete"
        ))
    ));
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 1);

    record_all_signed(&authority, &identity, &signer, &pending);
    authority
        .complete_authority_takeover(completion_request(&pending, 230))
        .expect("complete takeover");
    let mut wrong_lease = pending.lease_two;
    wrong_lease.fencing_token[0] ^= 0xff;
    assert!(matches!(
        authority.reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
            takeover_receipt_id: pending.takeover.receipt_id,
            lease: wrong_lease,
            reopened_at_ms: 240,
        }),
        Err(TaskStoreError::AuthorityLeaseBindingMismatch)
    ));
    assert_eq!(
        authority
            .inspect_participant_registry(pending.attempt.task_id)
            .expect("registry after rejected reopen")
            .state,
        ParticipantRegistryState::FrozenForTakeover
    );
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 2);
}
