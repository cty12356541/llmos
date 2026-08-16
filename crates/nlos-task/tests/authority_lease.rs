//! Acceptance tests for the schema-v29 durable `TaskAuthority` lease/term
//! primitive, opt-in `CommitPermit` binding, and same-term lease-bound
//! adoption guard. The slice proves local `SQLite` fencing and restart
//! readback; it does not claim IPC peer authentication or cross-term
//! `PermitAdoption` semantics.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptRegistrationDecision, AttemptSpec, AuthorityLeaseCloseRequest, AuthorityLeaseDecision,
    AuthorityLeaseFinalizeRequest, AuthorityLeasePermitRequest, AuthorityLeaseRequest,
    ClosePermitRequest, FinalizeDecision, FinalizeRequest, FinalizeRequestV3,
    MAX_AUTHORITY_LEASE_TTL_MS, PermitClosureOutcome, PermitDecision, PermitRequest,
    SnapshotBundle, SqliteTaskAuthority, TaskRegistrationDecision, TaskSpec, TaskStoreError,
    empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ProcessId, TaskAttemptId, TaskId,
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
