//! Acceptance tests for the schema-v35 durable `TaskAuthority` lease/term
//! primitive, opt-in `CommitPermit` binding, same-term lease-bound adoption
//! guard, and the local `FROZEN_FOR_TAKEOVER` fence pre-gate. The slice
//! proves local `SQLite` fencing, barrier-digest readback, and restart readback; it does not claim IPC
//! peer authentication, remote barrier completion, or cross-term
//! `PermitAdoption` semantics.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptRegistrationDecision, AttemptSpec, AuthorityLeaseCloseRequest, AuthorityLeaseDecision,
    AuthorityLeaseFinalizeRequest, AuthorityLeasePermitRequest, AuthorityLeaseRequest,
    AuthorityLeaseTakeoverFenceRequest, AuthorityTakeoverBarrierReceiptRequest, ClosePermitRequest,
    FinalizeDecision, FinalizeRequest, FinalizeRequestV3, MAX_AUTHORITY_LEASE_TTL_MS,
    PermitClosureOutcome, PermitDecision, PermitRequest, SnapshotBundle, SqliteTaskAuthority,
    TaskRegistrationDecision, TaskSpec, TaskStoreError, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ProcessId, ReceiptId, TaskAttemptId, TaskId,
    TaskParticipantId,
};
use rusqlite::Connection;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-authority-lease-{name}-{}-{sequence}.sqlite3",
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

fn process(seed: u8) -> ProcessId {
    ProcessId::from_bytes([seed; 16])
}

fn request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: process(holder),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: at_ms,
        ttl_ms,
    }
}

fn record(decision: AuthorityLeaseDecision) -> nlos_task::AuthorityLeaseRecord {
    decision.record()
}

fn task_id(seed: u8) -> TaskId {
    TaskId::from_bytes([seed; 16])
}

fn register_task_attempt(authority: &SqliteTaskAuthority, seed: u8) -> AttemptSpec {
    let task_id = task_id(seed);
    assert!(matches!(
        authority.register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1,
        }),
        Ok(TaskRegistrationDecision::Created(_))
    ));
    let attempt = AttemptSpec {
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
    assert!(matches!(
        authority.register_attempt(attempt),
        Ok(AttemptRegistrationDecision::Created(_))
    ));
    attempt
}

fn permit_request(attempt: &AttemptSpec, seed: u8, requested_at_ms: i64) -> PermitRequest {
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
    attempt: &AttemptSpec,
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

#[test]
#[allow(clippy::too_many_lines)] // One test covers the complete lease fence lifecycle.
fn lease_renewal_takeover_and_restart_fence_old_terms() {
    let database = TestDatabase::new("lifecycle");
    let authority = database.open();

    assert!(matches!(
        authority.inspect_authority_lease(),
        Err(TaskStoreError::AuthorityLeaseNotFound)
    ));
    let first_request = request(1, 0xa1, 100, 50);
    let first = record(
        authority
            .acquire_authority_lease(first_request)
            .expect("initial lease"),
    );
    assert_eq!(first.term, 1);
    assert_eq!(first.lease_epoch, 1);
    assert_eq!(first.expires_at_ms, 150);
    authority
        .validate_authority_lease(first, 149)
        .expect("initial lease is live");

    assert!(matches!(
        authority.acquire_authority_lease(request(2, 0xa2, 120, 50)),
        Err(TaskStoreError::AuthorityLeaseHeld)
    ));
    assert!(matches!(
        authority.acquire_authority_lease(request(1, 0xa1, 100, 51)),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));
    assert!(matches!(
        authority.acquire_authority_lease(first_request),
        Ok(AuthorityLeaseDecision::Replayed(replayed)) if replayed == first
    ));

    let renewed = record(
        authority
            .acquire_authority_lease(request(1, 0xa3, 120, 50))
            .expect("renew lease"),
    );
    assert_eq!(renewed.term, first.term, "renewal stays in the same term");
    assert_eq!(renewed.lease_epoch, 2);
    assert_ne!(renewed.fencing_token, first.fencing_token);
    assert!(matches!(
        authority.validate_authority_lease(first, 130),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    assert!(matches!(
        authority.validate_authority_lease(renewed, 170),
        Err(TaskStoreError::AuthorityLeaseExpired)
    ));

    let takeover = record(
        authority
            .acquire_authority_lease(request(2, 0xa4, 171, 100))
            .expect("expired lease takeover"),
    );
    assert_eq!(takeover.term, 2);
    assert_eq!(takeover.lease_epoch, 3);
    assert_ne!(takeover.fencing_token, renewed.fencing_token);
    assert!(matches!(
        authority.validate_authority_lease(renewed, 180),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    assert!(matches!(
        authority.acquire_authority_lease(first_request),
        Ok(AuthorityLeaseDecision::Replayed(replayed)) if replayed == first
    ));
    authority
        .validate_authority_lease(takeover, 270)
        .expect("takeover lease is live");

    let authority_id = takeover.authority_id;
    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened.inspect_authority_lease().expect("lease replay"),
        takeover
    );
    assert_eq!(
        reopened
            .inspect_authority_lease()
            .expect("authority id")
            .authority_id,
        authority_id
    );

    let raw = Connection::open(&database.path).expect("raw connection");
    let history_count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM task_authority_lease_history",
            [],
            |row| row.get(0),
        )
        .expect("history count");
    assert_eq!(
        history_count, 3,
        "acquire, renew, takeover are durable facts"
    );
    assert!(
        raw.execute(
            "UPDATE task_authority_lease_history SET transition_kind = 2
             WHERE authority_id = ?1 AND lease_epoch = ?2",
            rusqlite::params![
                authority_id.as_bytes().as_slice(),
                [0u8, 0, 0, 0, 0, 0, 0, 1].as_slice()
            ],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_authority_lease_history WHERE authority_id = ?1 AND lease_epoch = ?2",
            rusqlite::params![
                authority_id.as_bytes().as_slice(),
                [0u8, 0, 0, 0, 0, 0, 0, 1].as_slice()
            ],
        )
        .is_err()
    );
}

#[test]
fn lease_request_bounds_and_validation_fail_closed() {
    let database = TestDatabase::new("bounds");
    let authority = database.open();
    assert!(matches!(
        authority.acquire_authority_lease(request(1, 0xb1, -1, 10)),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));
    assert!(matches!(
        authority.acquire_authority_lease(request(1, 0xb2, 1, 0)),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));
    assert!(matches!(
        authority.acquire_authority_lease(request(1, 0xb3, 1, MAX_AUTHORITY_LEASE_TTL_MS + 1,)),
        Err(TaskStoreError::InvalidAuthorityLease { .. })
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn lease_binding_fences_permit_issue_and_terminal_mutation() {
    let database = TestDatabase::new("permit-binding");
    let authority = database.open();
    let lease_one = record(
        authority
            .acquire_authority_lease(request(1, 0xc1, 100, 100))
            .expect("initial lease"),
    );

    let first_attempt = register_task_attempt(&authority, 0x31);
    let first_permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&first_attempt, 0xd1, 150),
            lease: lease_one,
        })
        .expect("lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    assert_eq!(
        first_permit.authority_lease_binding,
        Some(lease_one.binding())
    );
    assert!(matches!(
        authority.finalize_commit_v3(finalize_request(
            &first_attempt,
            first_permit.permit_id,
            160
        )),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    assert!(matches!(
        authority.finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&first_attempt, first_permit.permit_id, 160),
            lease: lease_one,
        }),
        Ok(FinalizeDecision::Committed(_))
    ));

    let second_attempt = register_task_attempt(&authority, 0x41);
    let second_permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&second_attempt, 0xd2, 170),
            lease: lease_one,
        })
        .expect("second lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    assert!(matches!(
        authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&second_attempt, 0xd2, 170),
            lease: lease_one,
        }),
        Ok(PermitDecision::Replayed(_))
    ));

    let lease_two = record(
        authority
            .acquire_authority_lease(request(2, 0xc2, 201, 100))
            .expect("take over expired lease"),
    );
    assert_eq!(lease_two.term, lease_one.term + 1);
    assert!(matches!(
        authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&second_attempt, 0xd2, 170),
            lease: lease_two,
        }),
        Err(TaskStoreError::IdempotencyConflict)
    ));
    let third_attempt = register_task_attempt(&authority, 0x51);
    assert!(matches!(
        authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&third_attempt, 0xd3, 220),
            lease: lease_one,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    let third_permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&third_attempt, 0xd3, 220),
            lease: lease_two,
        })
        .expect("new term lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    assert!(matches!(
        authority.finalize_commit_v3(finalize_request(
            &second_attempt,
            second_permit.permit_id,
            210
        )),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    assert!(matches!(
        authority.finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&second_attempt, second_permit.permit_id, 210),
            lease: lease_one,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    assert!(matches!(
        authority.close_permit_with_authority_lease(AuthorityLeaseCloseRequest {
            close: ClosePermitRequest {
                task_id: second_attempt.task_id,
                attempt_id: second_attempt.attempt_id,
                attempt_generation: second_attempt.attempt_generation,
                permit_id: second_permit.permit_id,
                outcome: PermitClosureOutcome::FailedBeforeEffect,
                fenced_participant_digest: [0; 32],
                closed_at_ms: 210,
            },
            lease: lease_one,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    assert!(matches!(
        authority.finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&second_attempt, second_permit.permit_id, 210),
            lease: lease_two,
        }),
        Err(TaskStoreError::AuthorityLeaseBindingMismatch)
    ));
    assert!(matches!(
        authority.finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&third_attempt, third_permit.permit_id, 230),
            lease: lease_two,
        }),
        Ok(FinalizeDecision::Committed(_))
    ));
    assert_eq!(
        authority
            .inspect_permit(second_attempt.task_id, second_permit.permit_id)
            .expect("stale permit remains durable")
            .authority_lease_binding,
        Some(lease_one.binding())
    );
    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_permit(second_attempt.task_id, second_permit.permit_id)
            .expect("lease binding survives restart")
            .authority_lease_binding,
        Some(lease_one.binding())
    );
    let raw = Connection::open(&database.path).expect("raw connection");
    assert!(
        raw.execute(
            "UPDATE commit_permits SET authority_lease_term = ?1 WHERE permit_id = ?2",
            rusqlite::params![
                [0u8; 8].as_slice(),
                second_permit.permit_id.as_bytes().as_slice()
            ],
        )
        .is_err()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn takeover_fence_freezes_registry_and_replays_after_restart() {
    let database = TestDatabase::new("takeover-fence");
    let authority = database.open();
    let lease_one = record(
        authority
            .acquire_authority_lease(request(1, 0xd1, 100, 100))
            .expect("initial lease"),
    );
    let first_attempt = register_task_attempt(&authority, 0x61);
    let first_permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&first_attempt, 0xe1, 150),
            lease: lease_one,
        })
        .expect("lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let registry_binding = first_permit
        .participant_registry_binding
        .expect("permit registry binding");
    let assignment = authority
        .inspect_authority_assignment(first_attempt.task_id)
        .expect("active assignment");
    assert_eq!(
        assignment.state,
        nlos_task::AuthorityAssignmentState::Active
    );
    assert_eq!(assignment.authority_lease_binding, lease_one.binding());
    assert_eq!(assignment.participant_registry_binding, registry_binding);
    authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&first_attempt, first_permit.permit_id, 160),
            lease: lease_one,
        })
        .expect("close first permit before takeover");

    let lease_two = record(
        authority
            .acquire_authority_lease(request(2, 0xd2, 201, 100))
            .expect("new term lease"),
    );
    assert_eq!(lease_two.term, lease_one.term + 1);
    assert!(matches!(
        authority.prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: first_attempt.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_one,
            requested_at_ms: 210,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));

    let task_before = authority.inspect_task(first_attempt.task_id).expect("task");
    let control_before = task_before.control_epoch;
    let frozen = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: first_attempt.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 210,
        })
        .expect("freeze current registry");
    assert_eq!(
        frozen.state,
        nlos_task::ParticipantRegistryState::FrozenForTakeover
    );
    assert_eq!(frozen.generation, registry_binding.generation);
    assert_eq!(frozen.root, registry_binding.root);
    let fence_receipt = authority
        .inspect_authority_takeover_fence_receipt(first_attempt.task_id, registry_binding)
        .expect("takeover fence receipt");
    assert_eq!(fence_receipt.authority_lease_binding, lease_two.binding());
    assert_eq!(fence_receipt.frozen_registry_binding, registry_binding);
    assert!(fence_receipt.exact_fence_set_root.is_some());
    assert_eq!(
        fence_receipt.outstanding_operation_participant_root,
        Some([0; 32])
    );
    let fence_members = authority
        .inspect_authority_takeover_fence_members(first_attempt.task_id, registry_binding)
        .expect("exact fence member manifest");
    assert!(!fence_members.is_empty());
    assert_eq!(
        authority
            .inspect_task(first_attempt.task_id)
            .expect("task after fence")
            .control_epoch,
        control_before + 1
    );
    let takeover_receipt = authority
        .inspect_authority_takeover_receipt(first_attempt.task_id, fence_receipt.receipt_id)
        .expect("pending takeover receipt");
    assert_eq!(
        takeover_receipt.barrier_state,
        nlos_task::AuthorityTakeoverReceiptState::Pending
    );
    assert_eq!(takeover_receipt.old_assignment_id, assignment.assignment_id);
    assert_eq!(takeover_receipt.new_assignment_id, None);
    assert_eq!(
        takeover_receipt.frozen_old_authority_term,
        assignment.authority_lease_binding.term
    );
    assert_eq!(takeover_receipt.new_control_epoch, control_before + 1);
    assert_eq!(
        takeover_receipt.exact_fence_set_root,
        fence_receipt.exact_fence_set_root
    );
    let pending_assignment = authority
        .inspect_authority_assignment(first_attempt.task_id)
        .expect("pending assignment");
    assert_eq!(
        pending_assignment.state,
        nlos_task::AuthorityAssignmentState::TakeoverPending
    );
    let participant = frozen
        .participants
        .first()
        .copied()
        .expect("frozen registry participant");
    let barrier = authority
        .record_authority_takeover_barrier_receipt(AuthorityTakeoverBarrierReceiptRequest {
            takeover_receipt_id: takeover_receipt.receipt_id,
            participant,
            remote_receipt_id: ReceiptId::from_bytes([0x91; 16]),
            barrier_digest: [0x92; 32],
            observed_at_ms: 213,
        })
        .expect("record endpoint barrier observation");
    assert_eq!(
        barrier.state,
        nlos_task::AuthorityTakeoverBarrierReceiptState::Observed
    );
    assert_eq!(
        barrier.fence_set_root,
        takeover_receipt.exact_fence_set_root.unwrap()
    );
    assert_eq!(barrier.barrier_digest, Some([0x92; 32]));
    assert_eq!(
        authority
            .record_authority_takeover_barrier_receipt(AuthorityTakeoverBarrierReceiptRequest {
                takeover_receipt_id: takeover_receipt.receipt_id,
                participant,
                remote_receipt_id: ReceiptId::from_bytes([0x91; 16]),
                barrier_digest: [0x92; 32],
                observed_at_ms: 213,
            })
            .expect("replay endpoint barrier observation"),
        barrier
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_receipts(takeover_receipt.receipt_id)
            .expect("inspect endpoint barrier observations"),
        vec![barrier]
    );
    let coverage = authority
        .inspect_authority_takeover_barrier_coverage(takeover_receipt.receipt_id)
        .expect("inspect barrier coverage");
    assert_eq!(
        coverage.state,
        nlos_task::AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    assert_eq!(coverage.expected_member_count, fence_members.len());
    assert_eq!(coverage.observed_member_count, 1);
    assert!(coverage.missing_participants.is_empty());
    let mut unknown_participant = participant;
    unknown_participant.participant_id = TaskParticipantId::from_bytes([0xee; 16]);
    assert!(matches!(
        authority.record_authority_takeover_barrier_receipt(
            AuthorityTakeoverBarrierReceiptRequest {
                takeover_receipt_id: takeover_receipt.receipt_id,
                participant: unknown_participant,
                remote_receipt_id: ReceiptId::from_bytes([0x93; 16]),
                barrier_digest: [0x94; 32],
                observed_at_ms: 214,
            }
        ),
        Err(TaskStoreError::ParticipantRegistryBindingMismatch)
    ));

    let replayed = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: first_attempt.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 211,
        })
        .expect("fence replay");
    assert_eq!(replayed, frozen);
    assert_eq!(
        authority
            .inspect_task(first_attempt.task_id)
            .expect("task after replay")
            .control_epoch,
        control_before + 1,
        "takeover fence replay must not advance control epoch twice"
    );

    let second_attempt = AttemptSpec {
        task_id: first_attempt.task_id,
        attempt_id: TaskAttemptId::from_bytes([0x72; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: nlos_types::TaskSnapshotId::from_bytes([0x73; 16]),
            snapshot_digest: [0x74; 32],
            expected_head_commit_seq: 1,
            effect_history_root: task_before.head_effect_history_root,
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x75; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x76; 16]),
        registered_at_ms: 220,
    };
    let second_decision = authority
        .register_attempt(second_attempt)
        .expect("register second attempt");
    assert!(matches!(
        second_decision,
        AttemptRegistrationDecision::Created(_)
    ));
    assert!(matches!(
        authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&second_attempt, 0xe2, 221),
            lease: lease_two,
        }),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: nlos_task::ParticipantRegistryState::FrozenForTakeover
        })
    ));

    let raw = Connection::open(&database.path).expect("raw takeover receipt database");
    assert!(
        raw.execute(
            "UPDATE task_authority_takeover_fence_receipts
             SET exact_fence_set_root = zeroblob(32)
             WHERE receipt_id = ?1",
            rusqlite::params![fence_receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_authority_takeover_fence_receipts WHERE receipt_id = ?1",
            rusqlite::params![fence_receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE task_authority_takeover_receipts
             SET new_assignment_id = zeroblob(16)
             WHERE receipt_id = ?1",
            rusqlite::params![takeover_receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_authority_takeover_receipts WHERE receipt_id = ?1",
            rusqlite::params![takeover_receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE task_authority_takeover_barrier_receipts
             SET barrier_state = 1
             WHERE receipt_id = ?1",
            rusqlite::params![barrier.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_authority_takeover_barrier_receipts WHERE receipt_id = ?1",
            rusqlite::params![barrier.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    let member = fence_members.first().expect("fence member");
    assert!(
        raw.execute(
            "UPDATE task_authority_takeover_fence_members
             SET participant_generation = zeroblob(8)
             WHERE fence_receipt_id = ?1
               AND participant_type = ?2 AND participant_id = ?3",
            rusqlite::params![
                member.fence_receipt_id.as_bytes().as_slice(),
                match member.participant.participant_type {
                    nlos_task::ParticipantType::TaskStore => 1,
                    nlos_task::ParticipantType::ArtifactHead => 2,
                    nlos_task::ParticipantType::SemanticAdmission => 3,
                    nlos_task::ParticipantType::ChannelTopic => 4,
                    nlos_task::ParticipantType::DriverGateway => 5,
                    nlos_task::ParticipantType::ResourceLedger => 6,
                    nlos_task::ParticipantType::ProcessBinding => 7,
                    nlos_task::ParticipantType::OperationBinding => 8,
                },
                member.participant.participant_id.as_bytes().as_slice(),
            ],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_authority_takeover_fence_members
             WHERE fence_receipt_id = ?1
               AND participant_type = ?2 AND participant_id = ?3",
            rusqlite::params![
                member.fence_receipt_id.as_bytes().as_slice(),
                match member.participant.participant_type {
                    nlos_task::ParticipantType::TaskStore => 1,
                    nlos_task::ParticipantType::ArtifactHead => 2,
                    nlos_task::ParticipantType::SemanticAdmission => 3,
                    nlos_task::ParticipantType::ChannelTopic => 4,
                    nlos_task::ParticipantType::DriverGateway => 5,
                    nlos_task::ParticipantType::ResourceLedger => 6,
                    nlos_task::ParticipantType::ProcessBinding => 7,
                    nlos_task::ParticipantType::OperationBinding => 8,
                },
                member.participant.participant_id.as_bytes().as_slice(),
            ],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE task_authority_assignments
             SET authority_id = zeroblob(16)
             WHERE assignment_id = ?1",
            rusqlite::params![assignment.assignment_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_authority_assignments WHERE assignment_id = ?1",
            rusqlite::params![assignment.assignment_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    drop(raw);

    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_participant_registry(first_attempt.task_id)
            .expect("frozen registry after restart")
            .state,
        nlos_task::ParticipantRegistryState::FrozenForTakeover
    );
    assert_eq!(
        reopened
            .inspect_authority_takeover_fence_receipt(first_attempt.task_id, registry_binding)
            .expect("fence receipt after restart"),
        fence_receipt
    );
    assert_eq!(
        reopened
            .inspect_authority_takeover_fence_members(first_attempt.task_id, registry_binding)
            .expect("fence member manifest after restart"),
        fence_members
    );
    assert_eq!(
        reopened
            .inspect_authority_assignment(first_attempt.task_id)
            .expect("assignment after restart"),
        pending_assignment
    );
    assert_eq!(
        reopened
            .inspect_authority_takeover_receipt(first_attempt.task_id, fence_receipt.receipt_id)
            .expect("takeover receipt after restart"),
        takeover_receipt
    );
    assert_eq!(
        reopened
            .inspect_authority_takeover_barrier_receipts(takeover_receipt.receipt_id)
            .expect("barrier observation after restart"),
        vec![barrier]
    );
    assert_eq!(
        reopened
            .inspect_authority_takeover_barrier_coverage(takeover_receipt.receipt_id)
            .expect("barrier coverage after restart"),
        coverage
    );
    assert!(matches!(
        reopened.prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: first_attempt.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 212,
        }),
        Ok(registry) if registry.state == nlos_task::ParticipantRegistryState::FrozenForTakeover
    ));
}

#[test]
fn v34_takeover_barrier_schema_migrates_digest_column_without_fabrication() {
    let database = TestDatabase::new("migration-v35");
    drop(database.open());

    let raw = Connection::open(&database.path).expect("raw schema database");
    raw.execute_batch(
        "ALTER TABLE task_authority_takeover_barrier_receipts
             DROP COLUMN barrier_receipt_digest;
         PRAGMA user_version = 34;",
    )
    .expect("construct v34 barrier schema");
    drop(raw);

    drop(database.open());
    let raw = Connection::open(&database.path).expect("migrated schema database");
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read migrated schema version");
    assert_eq!(version, 41);
    let (column_count, not_null): (i64, i64) = raw
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(\"notnull\"), 0)
             FROM pragma_table_info('task_authority_takeover_barrier_receipts')
             WHERE name = 'barrier_receipt_digest'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("inspect migrated digest column");
    assert_eq!(column_count, 1);
    assert_eq!(not_null, 0, "legacy rows must remain representable as NULL");
}
