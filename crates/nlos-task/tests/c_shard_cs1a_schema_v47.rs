//! Integration tests for `CS1-A`: three-record §26.1 alignment and schema `v47`
//! migration (`TaskAuthorityAssignment`, `TaskAuthorityParticipantRegistry`,
//! `TaskAuthorityTakeoverReceipt`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_task::{
    AuthorityAssignmentState, AuthorityAssignmentStatus, AuthorityLeaseFinalizeRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseRecord, AuthorityLeaseRequest,
    AuthorityLeaseTakeoverFenceRequest, AuthoritySuccessorRegistryReopenRequest,
    AuthorityTakeoverBarrierReceiptRequest, AuthorityTakeoverBarrierReceiptState,
    AuthorityTakeoverReceiptRecord, AuthorityTakeoverReceiptState, BarrierObservationSignature,
    CompleteAuthorityTakeoverRequest, FinalizeRequest, FinalizeRequestV3, ParticipantRecord,
    ParticipantRegistryBinding, ParticipantRegistrySnapshot, ParticipantRegistryState,
    PermitDecision, PermitRequest, SnapshotBundle, SqliteTaskAuthority, TaskAuthorityAssignment,
    TaskAuthorityParticipantRegistry, TaskAuthorityTakeoverReceipt, TaskSpec, TaskStoreError,
    barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TestDatabase {
    dir: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new(label: &str) -> Self {
        let mut dir = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        dir.push(format!(
            "llmos-cs1a-v47-{label}-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp test dir");
        let path = dir.join("task-authority.sqlite");
        Self { dir, path }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open SqliteTaskAuthority")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
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

fn acquire_lease(
    authority: &SqliteTaskAuthority,
    holder: u8,
    key: u8,
    requested_at_ms: i64,
    ttl_ms: i64,
) -> AuthorityLeaseRecord {
    authority
        .acquire_authority_lease(lease_request(holder, key, requested_at_ms, ttl_ms))
        .expect("acquire lease")
        .record()
}

fn register_task_attempt(authority: &SqliteTaskAuthority, seed: u8) -> nlos_task::AttemptSpec {
    let task_id = TaskId::from_bytes([seed; 16]);
    authority
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
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

fn seed_task_with_active_assignment(
    authority: &SqliteTaskAuthority,
    seed: u8,
    finalize_first_permit: bool,
) -> (
    nlos_task::AttemptSpec,
    AuthorityLeaseRecord,
    TaskAuthorityAssignment,
    ParticipantRegistryBinding,
) {
    let lease_one = acquire_lease(authority, 1, seed, 100, 100);
    let attempt = register_task_attempt(authority, seed);
    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                write_set_root: [seed.wrapping_add(0x21); 32],
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(0x31); 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 150,
            },
            lease: lease_one,
        })
        .expect("request lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let binding = permit
        .participant_registry_binding
        .expect("permit registry binding");
    if finalize_first_permit {
        authority
            .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
                finalize: FinalizeRequestV3 {
                    base: FinalizeRequest {
                        task_id: attempt.task_id,
                        attempt_id: attempt.attempt_id,
                        attempt_generation: attempt.attempt_generation,
                        permit_id: permit.permit_id,
                        new_effect_history_root: [0; 32],
                        new_retry_fence_epoch: 0,
                        finalized_at_ms: 160,
                    },
                    required_satisfaction: Vec::new(),
                    fenced_participant_digest: [0; 32],
                },
                lease: lease_one,
            })
            .expect("close first permit before takeover");
    }
    let assignment = authority
        .inspect_authority_assignment(attempt.task_id)
        .expect("inspect assignment");
    (attempt, lease_one, assignment, binding)
}

fn record_signed_observation(
    authority: &SqliteTaskAuthority,
    identity: &IdentityAuthority,
    signer: &BarrierSigner,
    takeover: &AuthorityTakeoverReceiptRecord,
    participant: ParticipantRecord,
) {
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
}

fn regress_to_v46(path: &Path) {
    let raw = Connection::open(path).expect("open raw connection");
    raw.pragma_update(None, "foreign_keys", "OFF")
        .expect("fk off");
    raw.execute_batch(
        "DROP TRIGGER task_authority_assignment_identity_immutable;
         DROP TRIGGER task_authority_assignment_no_delete;
         CREATE TABLE task_authority_assignments_v46 (
             assignment_id BLOB PRIMARY KEY NOT NULL CHECK(length(assignment_id) = 16),
             task_id BLOB NOT NULL CHECK(length(task_id) = 16),
             task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
             authority_id BLOB NOT NULL CHECK(length(authority_id) = 16),
             authority_lease_holder_id BLOB NOT NULL CHECK(length(authority_lease_holder_id) = 16),
             authority_lease_term BLOB NOT NULL CHECK(length(authority_lease_term) = 8),
             authority_lease_epoch BLOB NOT NULL CHECK(length(authority_lease_epoch) = 8),
             authority_lease_fencing_token BLOB NOT NULL CHECK(length(authority_lease_fencing_token) = 32),
             authority_lease_expires_at_ms INTEGER NOT NULL CHECK(authority_lease_expires_at_ms >= 0),
             control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
             participant_registry_generation BLOB NOT NULL CHECK(length(participant_registry_generation) = 8),
             participant_registry_root BLOB NOT NULL CHECK(length(participant_registry_root) = 32),
             assignment_state INTEGER NOT NULL CHECK(assignment_state BETWEEN 1 AND 3),
             created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
             updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
             UNIQUE(task_id, assignment_id),
             FOREIGN KEY(task_id) REFERENCES tasks(task_id)
         ) STRICT;
         INSERT INTO task_authority_assignments_v46
         SELECT assignment_id, task_id, task_generation, authority_id, authority_lease_holder_id,
                authority_lease_term, authority_lease_epoch, authority_lease_fencing_token,
                authority_lease_expires_at_ms, control_epoch, participant_registry_generation,
                participant_registry_root, assignment_state, created_at_ms, updated_at_ms
         FROM task_authority_assignments;
         DROP TABLE task_authority_assignments;
         ALTER TABLE task_authority_assignments_v46 RENAME TO task_authority_assignments;
         CREATE TRIGGER task_authority_assignment_identity_immutable
         BEFORE UPDATE ON task_authority_assignments
         WHEN NEW.assignment_id != OLD.assignment_id OR NEW.task_id != OLD.task_id
           OR NEW.task_generation != OLD.task_generation OR NEW.authority_id != OLD.authority_id
           OR NEW.authority_lease_term != OLD.authority_lease_term OR NEW.created_at_ms != OLD.created_at_ms
         BEGIN SELECT RAISE(ABORT, 'task authority assignment identity is immutable'); END;
         CREATE TRIGGER task_authority_assignment_no_delete
         BEFORE DELETE ON task_authority_assignments
         BEGIN SELECT RAISE(ABORT, 'task authority assignment is durable evidence'); END;

         DROP TRIGGER participant_registry_signature_coupled_insert;
         DROP TRIGGER task_participant_registry_identity_immutable;
         ALTER TABLE task_participant_registries DROP COLUMN prior_participant_registry_root;
         ALTER TABLE task_participant_registries DROP COLUMN authority_id;
         ALTER TABLE task_participant_registries DROP COLUMN signature;
         CREATE TRIGGER task_participant_registry_identity_immutable
         BEFORE UPDATE ON task_participant_registries
         WHEN NEW.registry_id != OLD.registry_id OR NEW.task_id != OLD.task_id
           OR NEW.task_generation != OLD.task_generation
           OR NEW.registry_generation != OLD.registry_generation
           OR NEW.prior_registry_root != OLD.prior_registry_root
           OR NEW.participant_registry_root != OLD.participant_registry_root
           OR NEW.created_at_ms != OLD.created_at_ms
         BEGIN SELECT RAISE(ABORT, 'participant registry identity is immutable'); END;

         DROP TRIGGER task_authority_takeover_receipt_immutable;
         ALTER TABLE task_authority_takeover_receipts DROP COLUMN outstanding_effect_quarantine_root;
         ALTER TABLE task_authority_takeover_receipts DROP COLUMN consensus_commit_index;
         CREATE TRIGGER task_authority_takeover_receipt_immutable
         BEFORE UPDATE ON task_authority_takeover_receipts
         WHEN NOT (
             OLD.barrier_state = 1
             AND NEW.barrier_state = 2
             AND OLD.new_assignment_id IS NULL
             AND NEW.new_assignment_id IS NOT NULL
             AND NEW.receipt_id IS OLD.receipt_id
             AND NEW.task_id IS OLD.task_id
             AND NEW.task_generation IS OLD.task_generation
             AND NEW.old_assignment_id IS OLD.old_assignment_id
             AND NEW.fence_receipt_id IS OLD.fence_receipt_id
             AND NEW.frozen_old_authority_term IS OLD.frozen_old_authority_term
             AND NEW.frozen_old_control_epoch IS OLD.frozen_old_control_epoch
             AND NEW.new_authority_id IS OLD.new_authority_id
             AND NEW.new_authority_lease_holder_id IS OLD.new_authority_lease_holder_id
             AND NEW.new_authority_lease_term IS OLD.new_authority_lease_term
             AND NEW.new_authority_lease_epoch IS OLD.new_authority_lease_epoch
             AND NEW.new_authority_lease_fencing_token IS OLD.new_authority_lease_fencing_token
             AND NEW.new_authority_lease_expires_at_ms IS OLD.new_authority_lease_expires_at_ms
             AND NEW.new_control_epoch IS OLD.new_control_epoch
             AND NEW.frozen_registry_generation IS OLD.frozen_registry_generation
             AND NEW.frozen_registry_root IS OLD.frozen_registry_root
             AND NEW.exact_fence_set_root IS OLD.exact_fence_set_root
             AND NEW.outstanding_operation_participant_root IS OLD.outstanding_operation_participant_root
             AND NEW.created_at_ms IS OLD.created_at_ms
         )
         BEGIN
             SELECT RAISE(ABORT, 'task authority takeover receipt is immutable');
         END;

         PRAGMA user_version = 46;",
    )
    .expect("regress schema to v46");
}

#[test]
fn v46_schema_migrates_to_v47_preserving_all_three_authority_records() {
    let database = TestDatabase::new("v46-upgrade");
    let (task_id, assignment_before, binding_gen1) = {
        let authority = database.open();
        let (attempt, _lease_one, assignment, binding_gen1) =
            seed_task_with_active_assignment(&authority, 0x11, true);
        let lease_two = acquire_lease(&authority, 2, 0x22, 201, 100);
        authority
            .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
                task_id: attempt.task_id,
                expected_registry_binding: binding_gen1,
                lease: lease_two,
                requested_at_ms: 210,
            })
            .expect("prepare fence");
        (attempt.task_id, assignment, binding_gen1)
    };

    regress_to_v46(&database.path);
    {
        let raw = Connection::open(&database.path).expect("raw v46 connection");
        let version: i64 = raw
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read v46 version");
        assert_eq!(version, 46);
    }

    let authority = database.open();
    {
        let raw = Connection::open(&database.path).expect("raw v47 connection");
        let version: i64 = raw
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read v47 version");
        assert_eq!(version, 47);
    }

    let assignment_after = authority
        .inspect_authority_assignment(task_id)
        .expect("assignment survives v47 upgrade");
    assert_eq!(
        assignment_after.assignment_id,
        assignment_before.assignment_id
    );
    assert_eq!(
        assignment_after.status(),
        AuthorityAssignmentStatus::TakeoverPending
    );

    let registry_after: TaskAuthorityParticipantRegistry = authority
        .inspect_participant_registry(task_id)
        .expect("registry survives v47 upgrade");
    assert_eq!(registry_after.generation, 1);
    assert_eq!(registry_after.prior_participant_registry_root, None);
    assert_eq!(registry_after.authority, None);
    assert_eq!(registry_after.signature, None);

    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id, binding_gen1)
        .expect("fence receipt survives v47 upgrade");
    let takeover: TaskAuthorityTakeoverReceipt = authority
        .inspect_authority_takeover_receipt(task_id, fence.receipt_id)
        .expect("takeover receipt survives v47 upgrade");
    assert_eq!(takeover.outstanding_effect_quarantine_root, None);
    assert_eq!(takeover.consensus_commit_index, None);
}

#[test]
fn partial_v47_schema_fails_closed_with_corrupt_record() {
    let database = TestDatabase::new("partial-v47");
    drop(database.open());
    regress_to_v46(&database.path);

    // Inject a partial v47 column on `task_authority_takeover_receipts` while
    // leaving `user_version = 46`.
    {
        let raw = Connection::open(&database.path).expect("raw connection");
        raw.execute_batch(
            "ALTER TABLE task_authority_takeover_receipts
                 ADD COLUMN outstanding_effect_quarantine_root BLOB;
             PRAGMA user_version = 46;",
        )
        .expect("inject partial v47 column");
    }

    let reopen = SqliteTaskAuthority::open(&database.path);
    assert!(
        matches!(
            reopen,
            Err(TaskStoreError::CorruptRecord(
                "partial v47 authority record alignment schema"
            ))
        ),
        "partial v47 schema must fail closed with CorruptRecord"
    );
    let raw = Connection::open(&database.path).expect("raw connection after failed open");
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version");
    assert_eq!(version, 46);
}

#[test]
fn assignment_five_state_expired_and_quarantined_transitions_and_triggers() {
    let database_one = TestDatabase::new("assignment-expired");
    let authority_one = database_one.open();

    // Task 1: Active -> Expired (4), idempotent replay, and terminal lock.
    let (attempt_one, _lease_one, assignment_one, _binding_one) =
        seed_task_with_active_assignment(&authority_one, 0x31, true);
    let task_one = attempt_one.task_id;
    assert_eq!(assignment_one.status(), AuthorityAssignmentStatus::Active);
    assert_eq!(AuthorityAssignmentState::Expired.code(), 4);
    assert_eq!(AuthorityAssignmentState::Quarantined.code(), 5);

    assert!(matches!(
        authority_one.expire_authority_assignment(task_one, assignment_one.assignment_id, -1),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));
    assert!(matches!(
        authority_one.expire_authority_assignment(
            task_one,
            assignment_one.assignment_id,
            assignment_one.updated_at_ms - 1
        ),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));

    let expired = authority_one
        .expire_authority_assignment(task_one, assignment_one.assignment_id, 500)
        .expect("expire active assignment");
    assert_eq!(expired.status(), AuthorityAssignmentStatus::Expired);
    assert_eq!(expired.updated_at_ms, 500);

    // Idempotent replay with a different timestamp keeps original record.
    let expired_replay = authority_one
        .expire_authority_assignment(task_one, assignment_one.assignment_id, 600)
        .expect("idempotent expire replay");
    assert_eq!(expired_replay, expired);

    // Terminal Expired assignment rejects Quarantine transition.
    assert!(matches!(
        authority_one.quarantine_authority_assignment(task_one, assignment_one.assignment_id, 600),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));

    // Raw SQL trigger blocks mutating an Expired (4) assignment back to Active (1).
    {
        let raw = Connection::open(&database_one.path).expect("raw connection");
        assert!(
            raw.execute(
                "UPDATE task_authority_assignments SET assignment_state = 1 WHERE assignment_id = ?1",
                [assignment_one.assignment_id.as_bytes().as_slice()],
            )
            .is_err(),
            "terminal Expired assignment must be immutable at SQL trigger level"
        );
    }

    // Task 2: TakeoverPending -> Quarantined (5), idempotent replay, and terminal lock.
    let database_two = TestDatabase::new("assignment-quarantined");
    let authority_two = database_two.open();
    let (attempt_two, _lease_two_initial, assignment_two, binding_two) =
        seed_task_with_active_assignment(&authority_two, 0x51, true);
    let task_two = attempt_two.task_id;
    let lease_two_successor = acquire_lease(&authority_two, 2, 0x62, 201, 100);
    authority_two
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: task_two,
            expected_registry_binding: binding_two,
            lease: lease_two_successor,
            requested_at_ms: 210,
        })
        .expect("prepare fence on task two");
    let pending_two = authority_two
        .inspect_authority_assignment_by_id(task_two, assignment_two.assignment_id)
        .expect("inspect pending assignment");
    assert_eq!(
        pending_two.status(),
        AuthorityAssignmentStatus::TakeoverPending
    );

    let quarantined = authority_two
        .quarantine_authority_assignment(task_two, assignment_two.assignment_id, 250)
        .expect("quarantine pending assignment");
    assert_eq!(quarantined.status(), AuthorityAssignmentStatus::Quarantined);
    assert_eq!(quarantined.updated_at_ms, 250);

    let quarantined_replay = authority_two
        .quarantine_authority_assignment(task_two, assignment_two.assignment_id, 300)
        .expect("idempotent quarantine replay");
    assert_eq!(quarantined_replay, quarantined);

    assert!(matches!(
        authority_two.expire_authority_assignment(task_two, assignment_two.assignment_id, 300),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));

    {
        let raw = Connection::open(&database_two.path).expect("raw connection");
        assert!(
            raw.execute(
                "UPDATE task_authority_assignments SET assignment_state = 1 WHERE assignment_id = ?1",
                [assignment_two.assignment_id.as_bytes().as_slice()],
            )
            .is_err(),
            "terminal Quarantined assignment must be immutable at SQL trigger level"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn participant_registry_and_takeover_receipt_v47_fields_persist_and_lock() {
    let database = TestDatabase::new("registry-and-takeover-v47");
    let identity_dir = database.dir.join("identity");
    fs::create_dir_all(&identity_dir).expect("identity dir");
    let identity = IdentityAuthority::open(&identity_dir).expect("open identity authority");
    let signer = bootstrap_barrier_signer(&identity, 0x71, KeyPurpose::BarrierObservationSigning);

    let authority = database.open();
    let (attempt, _lease_one, _assignment_one, binding_gen1) =
        seed_task_with_active_assignment(&authority, 0x71, true);
    let task_id = attempt.task_id;

    // Record signature on generation 1 registry snapshot.
    let authority_bytes = [0xA5; 16];
    let signature_bytes = [0x5A; 64];
    let signed_gen1: ParticipantRegistrySnapshot = authority
        .record_participant_registry_signature(
            task_id,
            binding_gen1,
            authority_bytes,
            signature_bytes,
            165,
        )
        .expect("record registry signature");
    assert_eq!(signed_gen1.prior_participant_registry_root, None);
    assert_eq!(signed_gen1.authority, Some(authority_bytes));
    assert_eq!(signed_gen1.signature, Some(signature_bytes));

    // Idempotent replay with same signature succeeds; conflicting bytes fail closed.
    let signed_replay = authority
        .record_participant_registry_signature(
            task_id,
            binding_gen1,
            authority_bytes,
            signature_bytes,
            168,
        )
        .expect("idempotent signature replay");
    assert_eq!(signed_replay, signed_gen1);
    assert!(matches!(
        authority.record_participant_registry_signature(
            task_id,
            binding_gen1,
            [0xFF; 16],
            signature_bytes,
            169,
        ),
        Err(TaskStoreError::ParticipantRegistryBindingMismatch)
    ));

    // Raw SQL trigger rejects uncoupled authority_id / signature updates.
    {
        let raw = Connection::open(&database.path).expect("raw connection");
        assert!(
            raw.execute(
                "UPDATE task_participant_registries SET authority_id = NULL WHERE registry_id = ?1",
                [signed_gen1.registry_id.as_bytes().as_slice()],
            )
            .is_err(),
            "uncoupled or mutated registry signature must abort"
        );
    }

    // Prepare takeover fence with `outstanding_effect_quarantine_root` and
    // `consensus_commit_index` extensions.
    let lease_two = acquire_lease(&authority, 2, 0x82, 201, 100);
    let quarantine_root = [0xCC; 32];
    let commit_index = 42_u64;
    let frozen_registry = authority
        .prepare_authority_takeover_fence_with_receipt_extensions(
            AuthorityLeaseTakeoverFenceRequest {
                task_id,
                expected_registry_binding: binding_gen1,
                lease: lease_two,
                requested_at_ms: 210,
            },
            Some(quarantine_root),
            Some(commit_index),
        )
        .expect("prepare fence with v47 receipt extensions");
    assert_eq!(
        frozen_registry.state,
        ParticipantRegistryState::FrozenForTakeover
    );
    assert_eq!(frozen_registry.authority, Some(authority_bytes));
    assert_eq!(frozen_registry.signature, Some(signature_bytes));

    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id, binding_gen1)
        .expect("inspect fence");
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id, fence.receipt_id)
        .expect("inspect takeover receipt");
    assert_eq!(
        takeover.outstanding_effect_quarantine_root,
        Some(quarantine_root)
    );
    assert_eq!(takeover.consensus_commit_index, Some(commit_index));

    // Idempotent replay of extensions succeeds; conflicting values fail closed.
    let replayed_takeover = authority
        .record_authority_takeover_receipt_extensions(
            takeover.receipt_id,
            Some(quarantine_root),
            Some(commit_index),
        )
        .expect("idempotent extension replay");
    assert_eq!(replayed_takeover, takeover);
    assert!(matches!(
        authority.record_authority_takeover_receipt_extensions(
            takeover.receipt_id,
            Some([0xDD; 32]),
            Some(commit_index),
        ),
        Err(TaskStoreError::AuthorityLeaseBindingMismatch)
    ));

    // Complete takeover and reopen successor registry (generation 2) to verify
    // `prior_participant_registry_root` is populated on successor reopen.
    let members = authority
        .inspect_authority_takeover_fence_members(task_id, binding_gen1)
        .expect("fence members");
    for member in &members {
        record_signed_observation(
            &authority,
            &identity,
            &signer,
            &takeover,
            member.participant,
        );
    }

    let completion = authority
        .complete_authority_takeover(CompleteAuthorityTakeoverRequest {
            takeover_receipt_id: takeover.receipt_id,
            lease: lease_two,
            completed_at_ms: 230,
        })
        .expect("complete takeover");
    assert_eq!(
        completion.barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );

    let completed_takeover = authority
        .inspect_authority_takeover_receipt(task_id, fence.receipt_id)
        .expect("inspect completed takeover");
    assert_eq!(
        completed_takeover.outstanding_effect_quarantine_root,
        Some(quarantine_root)
    );
    assert_eq!(
        completed_takeover.consensus_commit_index,
        Some(commit_index)
    );

    let reopen = authority
        .reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
            takeover_receipt_id: takeover.receipt_id,
            lease: lease_two,
            reopened_at_ms: 240,
        })
        .expect("reopen successor registry");
    let registry_gen2 = authority
        .inspect_participant_registry(task_id)
        .expect("inspect reopened registry");
    assert_eq!(registry_gen2.generation, 2);
    assert_eq!(
        registry_gen2.prior_participant_registry_root,
        Some(binding_gen1.root)
    );
    assert_eq!(registry_gen2.root, reopen.successor_registry_binding.root);
}
