//! Acceptance tests for the B-TASK first slice: durable `TaskAuthority` with
//! dual `TaskAttempt` competition and unique `CommitPermit` issuance.
//!
//! Every test maps to one acceptance-gate bullet in
//! `docs/management/stage-b-progress.md` §5.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptRegistrationDecision, AttemptSpec, AttemptState, CancelDecision, CancelRequest,
    FinalizeDecision, FinalizeRequest, PermitConflict, PermitDecision, PermitRequest, PermitState,
    ReceiptOutcome, SnapshotBundle, SqliteTaskAuthority, TaskRegistrationDecision, TaskSpec,
    TaskState, TaskStoreError, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
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

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn task_id() -> TaskId {
    TaskId::from_bytes(bytes(0x01))
}

fn task_spec() -> TaskSpec {
    TaskSpec {
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
    }
}

fn snapshot(head_seq: u64, fence: u64) -> SnapshotBundle {
    let tag = u8::try_from(head_seq).expect("test head fits in u8");
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(bytes(0x10 + tag)),
        snapshot_digest: [0x20 + tag; 32],
        expected_head_commit_seq: head_seq,
        effect_history_root: if head_seq == 0 {
            empty_effect_history_root()
        } else {
            [0x30 + tag; 32]
        },
        retry_fence_epoch: fence,
    }
}

fn snapshot_with_root(head_seq: u64, root: u8, fence: u64) -> SnapshotBundle {
    let mut bundle = snapshot(head_seq, fence);
    bundle.effect_history_root = [root; 32];
    bundle
}

fn attempt_spec(seed: u8, bundle: SnapshotBundle) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes(bytes(seed)),
        attempt_generation: Generation::INITIAL,
        snapshot: bundle,
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0xc0 + seed)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xa0 + seed)),
        registered_at_ms: 2_000,
    }
}

fn permit_request(spec: &AttemptSpec, seed: u8) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [seed; 32],
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xb0 + seed)),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

fn cancel_request(seed: u8) -> CancelRequest {
    CancelRequest {
        task_id: task_id(),
        idempotency_key: IdempotencyKey::from_bytes(bytes(seed)),
        requested_at_ms: 4_000,
    }
}

fn finalize_request(
    spec: &AttemptSpec,
    permit_id: CommitPermitId,
    root: u8,
    fence: u64,
) -> FinalizeRequest {
    FinalizeRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id,
        new_effect_history_root: [root; 32],
        new_retry_fence_epoch: fence,
        finalized_at_ms: 5_000,
    }
}

fn issued_permit(decision: PermitDecision) -> nlos_task::PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected Issued, got {other:?}"),
    }
}

/// Gate bullet 2: dual `TaskAttempts` register with independent generations
/// and cancellation scopes, both bound to the same frozen snapshot.
#[test]
fn dual_attempts_register_independently_on_one_snapshot() {
    let database = TestDatabase::new("dual-attempts");
    let authority = database.open();
    assert_eq!(
        authority.register_task(task_spec()).expect("register task"),
        TaskRegistrationDecision::Created(task_id())
    );

    let bundle = snapshot(0, 0);
    let spec_a = attempt_spec(0x0a, bundle);
    let spec_b = attempt_spec(0x0b, bundle);
    let first = authority.register_attempt(spec_a).expect("register A");
    let second = authority.register_attempt(spec_b).expect("register B");

    let (handle_a, handle_b) = match (first, second) {
        (
            AttemptRegistrationDecision::Created(handle_a),
            AttemptRegistrationDecision::Created(handle_b),
        ) => (handle_a, handle_b),
        other => panic!("expected two Created decisions, got {other:?}"),
    };
    assert_ne!(handle_a.attempt_id, handle_b.attempt_id);
    assert_eq!(handle_a.snapshot_id, handle_b.snapshot_id);

    let record_a = authority
        .inspect_attempt(task_id(), spec_a.attempt_id)
        .expect("inspect A");
    let record_b = authority
        .inspect_attempt(task_id(), spec_b.attempt_id)
        .expect("inspect B");
    assert_eq!(record_a.state, AttemptState::Created);
    assert_eq!(record_b.state, AttemptState::Created);
    assert_ne!(
        record_a.cancellation_scope_id, record_b.cancellation_scope_id,
        "each attempt owns an independent cancellation scope"
    );
    assert_eq!(record_a.snapshot, bundle);
    assert_eq!(record_b.snapshot, bundle);

    // Exact replay returns the original handle; conflicting bytes under the
    // same key fail closed.
    assert_eq!(
        authority.register_attempt(spec_a).expect("replay A"),
        AttemptRegistrationDecision::Existing(handle_a)
    );
    let mut conflicting = spec_a;
    conflicting.snapshot.snapshot_digest = [0xff; 32];
    assert!(matches!(
        authority.register_attempt(conflicting),
        Err(TaskStoreError::IdempotencyConflict)
    ));
}

/// Gate bullets 1+3: `TaskHead` revision CAS and unique permit issuance —
/// exactly one attempt obtains the permit and advances the head.
#[test]
fn permit_cas_issues_exactly_one_permit_and_supersedes_loser() {
    let database = TestDatabase::new("permit-cas");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let bundle = snapshot(0, 0);
    let spec_a = attempt_spec(0x0a, bundle);
    let spec_b = attempt_spec(0x0b, bundle);
    authority.register_attempt(spec_a).expect("register A");
    authority.register_attempt(spec_b).expect("register B");

    let winner = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("request A"),
    );
    assert_eq!(winner.attempt_id, spec_a.attempt_id);
    assert_eq!(winner.permit_epoch, 1);
    assert_eq!(winner.cancel_epoch, 0);
    assert_eq!(winner.expected_head_commit_seq, 0);
    assert_eq!(
        winner.expected_effect_history_root,
        empty_effect_history_root()
    );

    let loser = authority
        .request_commit_permit(permit_request(&spec_b, 0x02))
        .expect("request B");
    match loser {
        PermitDecision::Superseded { winner: recorded } => {
            assert_eq!(recorded.permit_id, winner.permit_id);
            assert_eq!(recorded.attempt_id, spec_a.attempt_id);
        }
        other => panic!("expected Superseded, got {other:?}"),
    }
    assert_eq!(
        authority
            .inspect_attempt(task_id(), spec_b.attempt_id)
            .expect("inspect B")
            .state,
        AttemptState::Superseded
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), spec_a.attempt_id)
            .expect("inspect A")
            .state,
        AttemptState::CommitPermitted
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("inspect task")
            .active_permit,
        Some(winner.permit_id)
    );

    // Only the permit holder advances the TaskHead.
    let receipt = match authority
        .finalize_commit(finalize_request(&spec_a, winner.permit_id, 0x31, 0))
        .expect("finalize")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(receipt.outcome, ReceiptOutcome::Committed);
    assert_eq!(receipt.prior_head_commit_seq, 0);
    assert_eq!(receipt.new_head_commit_seq, 1);
    let head = authority.inspect_task(task_id()).expect("inspect head");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.head_effect_history_root, [0x31; 32]);
    assert_eq!(head.active_permit, None);
    assert_eq!(
        authority
            .inspect_permit(task_id(), winner.permit_id)
            .expect("inspect permit")
            .state,
        PermitState::Closed
    );
}

/// Gate bullet 3 (idempotency): same key + same bytes replays the original
/// permit; same key + different bytes fails closed.
#[test]
fn permit_replay_returns_original_and_conflicting_bytes_fail_closed() {
    let database = TestDatabase::new("permit-replay");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let spec_a = attempt_spec(0x0a, snapshot(0, 0));
    authority.register_attempt(spec_a).expect("register A");

    let request = permit_request(&spec_a, 0x01);
    let issued = issued_permit(authority.request_commit_permit(request).expect("issue"));
    let replayed = authority.request_commit_permit(request).expect("replay");
    match replayed {
        PermitDecision::Replayed(original) => {
            assert_eq!(original.permit_id, issued.permit_id);
            assert_eq!(original.permit_epoch, issued.permit_epoch);
        }
        other => panic!("expected Replayed, got {other:?}"),
    }
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("inspect")
            .permit_epoch,
        1,
        "replay must not issue a second permit"
    );

    let mut conflicting = request;
    conflicting.write_set_root = [0xee; 32];
    assert!(matches!(
        authority.request_commit_permit(conflicting),
        Err(TaskStoreError::IdempotencyConflict)
    ));
}

/// Gate bullet 4: losing/superseded/stale attempts cannot advance the head
/// or overwrite the winner's receipt.
#[test]
fn losing_or_stale_attempt_cannot_finalize_or_overwrite_winner_receipt() {
    let database = TestDatabase::new("loser-finalize");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let bundle = snapshot(0, 0);
    let spec_a = attempt_spec(0x0a, bundle);
    let spec_b = attempt_spec(0x0b, bundle);
    authority.register_attempt(spec_a).expect("register A");
    authority.register_attempt(spec_b).expect("register B");
    let winner = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("request A"),
    );
    authority
        .request_commit_permit(permit_request(&spec_b, 0x02))
        .expect("request B");

    // The loser presenting the winner's permit is not the holder.
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec_b, winner.permit_id, 0x40, 0)),
        Err(TaskStoreError::NotPermitHolder)
    ));
    // The loser presenting a fabricated permit ID finds nothing.
    assert!(matches!(
        authority.finalize_commit(finalize_request(
            &spec_b,
            CommitPermitId::from_bytes(bytes(0x99)),
            0x40,
            0
        )),
        Err(TaskStoreError::PermitNotFound)
    ));
    // A stale generation for the holder is fenced.
    let mut stale = finalize_request(&spec_a, winner.permit_id, 0x40, 0);
    stale.attempt_generation = Generation::INITIAL.checked_next().expect("next generation");
    assert!(matches!(
        authority.finalize_commit(stale),
        Err(TaskStoreError::InvalidGeneration)
    ));
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        0,
        "no losing or stale finalize may advance the head"
    );

    let receipt = match authority
        .finalize_commit(finalize_request(&spec_a, winner.permit_id, 0x41, 0))
        .expect("winner finalize")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };

    // The loser still cannot rewrite history after the winner committed.
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec_b, winner.permit_id, 0x42, 0)),
        Err(TaskStoreError::IdempotencyConflict)
    ));
    let head = authority.inspect_task(task_id()).expect("head after");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.head_effect_history_root, [0x41; 32]);
    let stored = authority
        .inspect_receipt(task_id(), receipt.receipt_id)
        .expect("winner receipt");
    assert_eq!(stored, receipt, "winner receipt is immutable and untouched");
}

/// Gate bullet 5 (cancel-first): cancel commits before the permit request —
/// no permit is issued, the attempt closes pre-permit with a closure
/// receipt, and the `TaskHead` stays unchanged.
#[test]
fn cancel_first_blocks_permit_and_closes_attempt_with_head_unchanged() {
    let database = TestDatabase::new("cancel-first");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let bundle = snapshot(0, 0);
    let spec_a = attempt_spec(0x0a, bundle);
    let spec_b = attempt_spec(0x0b, bundle);
    authority.register_attempt(spec_a).expect("register A");
    authority.register_attempt(spec_b).expect("register B");

    let cancelled = authority.cancel_task(cancel_request(0xd1)).expect("cancel");
    let closed = match cancelled {
        CancelDecision::Applied {
            cancel_epoch,
            closed_attempts,
        } => {
            assert_eq!(cancel_epoch, 1);
            assert_eq!(closed_attempts.len(), 2);
            closed_attempts
        }
        other => panic!("expected Applied, got {other:?}"),
    };
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 0, "cancel must not advance the head");
    assert_eq!(head.state, TaskState::Cancelled);
    assert_eq!(head.cancel_epoch, 1);

    let receipt_a = closed
        .iter()
        .find(|entry| entry.attempt_id == spec_a.attempt_id)
        .expect("closure for A")
        .receipt_id;
    let decision = authority
        .request_commit_permit(permit_request(&spec_a, 0x01))
        .expect("permit after cancel");
    match decision {
        PermitDecision::CancelledBeforeEffect { receipt_id } => {
            assert_eq!(receipt_id, receipt_a);
        }
        other => panic!("expected CancelledBeforeEffect, got {other:?}"),
    }
    let after = authority.inspect_task(task_id()).expect("head after");
    assert_eq!(after.head_commit_seq, 0);
    assert_eq!(
        after.permit_epoch, 0,
        "no permit may be issued after cancel"
    );
    assert_eq!(after.active_permit, None);

    // Replaying the refused request returns the same durable closure.
    let replayed = authority
        .request_commit_permit(permit_request(&spec_a, 0x01))
        .expect("replay refused request");
    assert_eq!(
        replayed,
        PermitDecision::CancelledBeforeEffect {
            receipt_id: receipt_a
        }
    );

    // A brand-new attempt cannot be registered on the cancelled task.
    assert!(matches!(
        authority.register_attempt(attempt_spec(0x0c, bundle)),
        Err(TaskStoreError::TaskCancelled)
    ));
}

/// Gate bullet 5 (idempotent cancel): the cancel epoch advances exactly
/// once across replays.
#[test]
fn cancel_replay_increments_epoch_exactly_once() {
    let database = TestDatabase::new("cancel-replay");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let spec_a = attempt_spec(0x0a, snapshot(0, 0));
    authority.register_attempt(spec_a).expect("register A");

    assert!(matches!(
        authority.cancel_task(cancel_request(0xd1)),
        Ok(CancelDecision::Applied {
            cancel_epoch: 1,
            ..
        })
    ));
    assert_eq!(
        authority.cancel_task(cancel_request(0xd1)).expect("replay"),
        CancelDecision::Replayed { cancel_epoch: 1 }
    );
    assert_eq!(
        authority
            .cancel_task(cancel_request(0xd2))
            .expect("second key"),
        CancelDecision::AlreadyCancelled { cancel_epoch: 1 }
    );
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.cancel_epoch, 1, "epoch advances exactly once");
    let attempt = authority
        .inspect_attempt(task_id(), spec_a.attempt_id)
        .expect("inspect A");
    assert_eq!(attempt.state, AttemptState::Cancelled);
    assert!(matches!(
        authority
            .inspect_receipt(task_id(), attempt.receipt_id.expect("closure receipt"))
            .expect("inspect receipt")
            .outcome,
        ReceiptOutcome::CancelledBeforeEffect
    ));
}

/// Gate bullet 5 (permit-first): an issued permit is not cleared by cancel
/// (`[TASK-COMMIT-003]`); the holder can still finalize. Effect-level
/// fencing is deferred to the `EffectPermit` slice.
#[test]
fn permit_first_survives_cancel_and_holder_can_finalize() {
    let database = TestDatabase::new("permit-first");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let bundle = snapshot(0, 0);
    let spec_a = attempt_spec(0x0a, bundle);
    let spec_b = attempt_spec(0x0b, bundle);
    authority.register_attempt(spec_a).expect("register A");
    authority.register_attempt(spec_b).expect("register B");
    let winner = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("request A"),
    );

    let cancelled = authority.cancel_task(cancel_request(0xd1)).expect("cancel");
    match cancelled {
        CancelDecision::Applied {
            cancel_epoch,
            closed_attempts,
        } => {
            assert_eq!(cancel_epoch, 1);
            assert_eq!(
                closed_attempts.len(),
                1,
                "only the open pre-permit attempt closes"
            );
            assert_eq!(closed_attempts[0].attempt_id, spec_b.attempt_id);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    let permit = authority
        .inspect_permit(task_id(), winner.permit_id)
        .expect("permit survives cancel");
    assert_eq!(
        permit.state,
        PermitState::Issued,
        "cancel must not clear an issued permit"
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), spec_a.attempt_id)
            .expect("inspect A")
            .state,
        AttemptState::CommitPermitted
    );

    let receipt = match authority
        .finalize_commit(finalize_request(&spec_a, winner.permit_id, 0x50, 0))
        .expect("holder finalizes after cancel")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(receipt.new_head_commit_seq, 1);
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.cancel_epoch, 1);
}

struct RestartFixture {
    spec_a: AttemptSpec,
    spec_b: AttemptSpec,
    spec_c: AttemptSpec,
    request_a: PermitRequest,
    finalize_a: FinalizeRequest,
    winner_id: CommitPermitId,
    winner_receipt: nlos_task::TaskReceiptRecord,
    second_permit_id: CommitPermitId,
}

fn seed_restart_fixture(database: &TestDatabase) -> RestartFixture {
    let authority = database.open();
    let bundle0 = snapshot(0, 0);
    let spec_a = attempt_spec(0x0a, bundle0);
    let spec_b = attempt_spec(0x0b, bundle0);
    authority.register_task(task_spec()).expect("register task");
    authority.register_attempt(spec_a).expect("register A");
    authority.register_attempt(spec_b).expect("register B");
    let request_a = permit_request(&spec_a, 0x01);
    let winner = issued_permit(
        authority
            .request_commit_permit(request_a)
            .expect("permit A"),
    );
    authority
        .request_commit_permit(permit_request(&spec_b, 0x02))
        .expect("permit B");
    let finalize_a = finalize_request(&spec_a, winner.permit_id, 0x31, 0);
    let winner_receipt = match authority.finalize_commit(finalize_a).expect("finalize A") {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    // A second competition starts on the advanced head and is left
    // outstanding across the restart.
    let spec_c = attempt_spec(0x0c, snapshot(1, 0));
    authority.register_attempt(spec_c).expect("register C");
    let second_permit_id = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_c, 0x03))
            .expect("permit C"),
    )
    .permit_id;
    RestartFixture {
        spec_a,
        spec_b,
        spec_c,
        request_a,
        finalize_a,
        winner_id: winner.permit_id,
        winner_receipt,
        second_permit_id,
    }
}

/// Gate bullet 6: after reopening the database, head/attempt/permit state
/// is intact, replays return the original results, and no ghost permit can
/// appear.
#[test]
fn restart_recovers_state_without_ghost_permits() {
    let database = TestDatabase::new("restart");
    let fixture = seed_restart_fixture(&database);
    let authority = database.open();

    let head = authority
        .inspect_task(task_id())
        .expect("head after restart");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.head_effect_history_root, [0x31; 32]);
    assert_eq!(head.permit_epoch, 2);
    assert_eq!(head.active_permit, Some(fixture.second_permit_id));

    assert_eq!(
        authority
            .inspect_attempt(task_id(), fixture.spec_a.attempt_id)
            .expect("A")
            .state,
        AttemptState::Committed
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), fixture.spec_b.attempt_id)
            .expect("B")
            .state,
        AttemptState::Superseded
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), fixture.spec_c.attempt_id)
            .expect("C")
            .state,
        AttemptState::CommitPermitted
    );

    // Replaying the original permit idempotency key returns the same
    // lifecycle record; replaying the finalize returns the original receipt.
    match authority
        .request_commit_permit(fixture.request_a)
        .expect("replay permit A")
    {
        PermitDecision::Replayed(original) => {
            assert_eq!(original.permit_id, fixture.winner_id);
            assert_eq!(original.state, PermitState::Closed);
        }
        other => panic!("expected Replayed, got {other:?}"),
    }
    match authority
        .finalize_commit(fixture.finalize_a)
        .expect("replay finalize A")
    {
        FinalizeDecision::Replayed(original) => {
            assert_eq!(*original, fixture.winner_receipt);
        }
        other @ FinalizeDecision::Committed(_) => panic!("expected Replayed, got {other:?}"),
    }
    assert_eq!(
        authority
            .inspect_receipt(task_id(), fixture.winner_receipt.receipt_id)
            .expect("receipt after restart"),
        fixture.winner_receipt
    );

    // No ghost permit: an ID that was never durably issued cannot resolve,
    // and replays did not mint new permits.
    assert!(matches!(
        authority.inspect_permit(task_id(), CommitPermitId::from_bytes(bytes(0x77))),
        Err(TaskStoreError::PermitNotFound)
    ));
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .permit_epoch,
        2
    );
}

/// Gate bullet 1 (schema): an unknown stored schema version is refused
/// fail-closed.
#[test]
fn unknown_schema_version_fails_closed() {
    let database = TestDatabase::new("schema-version");
    {
        let connection = rusqlite::Connection::open(&database.path).expect("create raw database");
        connection
            .pragma_update(None, "user_version", 99)
            .expect("set foreign schema version");
    }
    assert!(matches!(
        SqliteTaskAuthority::open(&database.path),
        Err(TaskStoreError::UnsupportedSchema(99))
    ));
}

/// Invalid transitions all fail closed with typed errors.
#[test]
fn invalid_transitions_fail_closed() {
    let database = TestDatabase::new("invalid-transitions");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let spec_a = attempt_spec(0x0a, snapshot(0, 0));
    authority.register_attempt(spec_a).expect("register A");

    // Finalize without any issued permit.
    assert!(matches!(
        authority.finalize_commit(finalize_request(
            &spec_a,
            CommitPermitId::from_bytes(bytes(0x88)),
            0x60,
            0
        )),
        Err(TaskStoreError::PermitNotFound)
    ));

    // Snapshot/head digest mismatch: the attempt froze head 1 while the
    // authority is still at head 0.
    let stale = attempt_spec(0x0d, snapshot(1, 0));
    authority.register_attempt(stale).expect("register stale");
    let decision = authority
        .request_commit_permit(permit_request(&stale, 0x04))
        .expect("stale permit request");
    assert_eq!(
        decision,
        PermitDecision::Conflicted {
            reason: PermitConflict::StaleTaskHead {
                expected: 1,
                current: 0
            }
        }
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), stale.attempt_id)
            .expect("inspect stale")
            .state,
        AttemptState::Conflicted
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        0
    );

    // Retry-fence regression is refused on finalize.
    let winner = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("permit A"),
    );
    authority
        .finalize_commit(finalize_request(&spec_a, winner.permit_id, 0x61, 1))
        .expect("finalize with fence 1");
    let mut bundle_e = snapshot_with_root(1, 0x61, 1);
    bundle_e.snapshot_id = TaskSnapshotId::from_bytes(bytes(0x1e));
    let spec_e = attempt_spec(0x0e, bundle_e);
    authority.register_attempt(spec_e).expect("register E");
    let second = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_e, 0x05))
            .expect("permit E"),
    );
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec_e, second.permit_id, 0x62, 0)),
        Err(TaskStoreError::FenceRegression)
    ));
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        1
    );
}

/// A closed permit with no outstanding effects releases the CAS gate: a new
/// competition on the advanced head may obtain a new permit
/// (`[TASK-COMMIT-001]` second clause).
#[test]
fn closed_permit_releases_cas_gate_for_next_competition() {
    let database = TestDatabase::new("permit-regate");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let spec_a = attempt_spec(0x0a, snapshot(0, 0));
    authority.register_attempt(spec_a).expect("register A");
    let first = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("permit A"),
    );
    authority
        .finalize_commit(finalize_request(&spec_a, first.permit_id, 0x31, 0))
        .expect("finalize A");

    let spec_b = attempt_spec(0x0b, snapshot(1, 0));
    authority.register_attempt(spec_b).expect("register B");
    let second = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_b, 0x02))
            .expect("permit B"),
    );
    assert_eq!(second.permit_epoch, 2);
    assert_eq!(second.expected_head_commit_seq, 1);
    assert_ne!(second.permit_id, first.permit_id);
}

/// Task registration is idempotent and the initial head follows the fixed
/// empty-history formula (`[TASK-EFFECT-ID-001]`).
#[test]
fn task_registration_is_idempotent_with_fixed_initial_head() {
    let database = TestDatabase::new("task-registration");
    let authority = database.open();
    assert_eq!(
        authority.register_task(task_spec()).expect("register"),
        TaskRegistrationDecision::Created(task_id())
    );
    assert_eq!(
        authority.register_task(task_spec()).expect("replay"),
        TaskRegistrationDecision::Existing(task_id())
    );
    let mut conflicting = task_spec();
    conflicting.task_generation = Generation::INITIAL.checked_next().expect("next generation");
    assert!(matches!(
        authority.register_task(conflicting),
        Err(TaskStoreError::DuplicateTask)
    ));

    let head = authority.inspect_task(task_id()).expect("inspect head");
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(head.head_effect_history_root, empty_effect_history_root());
    assert_eq!(head.retry_fence_epoch, 0);
    assert_eq!(head.cancel_epoch, 0);
    assert_eq!(head.active_permit, None);
}

/// Gate bullet 4 (stale snapshot): an attempt whose frozen snapshot was
/// overtaken by a winner's commit is rejected as Conflicted and changes
/// nothing.
#[test]
fn stale_snapshot_detected_after_head_advance() {
    let database = TestDatabase::new("stale-snapshot");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let spec_a = attempt_spec(0x0a, snapshot(0, 0));
    let spec_b = attempt_spec(0x0b, snapshot(0, 0));
    authority.register_attempt(spec_a).expect("register A");
    authority.register_attempt(spec_b).expect("register B");
    let winner = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("permit A"),
    );
    authority
        .finalize_commit(finalize_request(&spec_a, winner.permit_id, 0x31, 0))
        .expect("finalize A");

    let decision = authority
        .request_commit_permit(permit_request(&spec_b, 0x02))
        .expect("stale B");
    assert!(matches!(
        decision,
        PermitDecision::Conflicted {
            reason: PermitConflict::StaleTaskHead {
                expected: 0,
                current: 1
            }
        }
    ));
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        1
    );
}

/// Gate bullet 3 under true concurrency: two threads race the permit CAS;
/// exactly one wins, the loser learns the winner's identity.
#[test]
fn concurrent_permit_requests_have_exactly_one_winner() {
    let database = TestDatabase::new("permit-race");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let bundle = snapshot(0, 0);
    let spec_a = attempt_spec(0x0a, bundle);
    let spec_b = attempt_spec(0x0b, bundle);
    authority.register_attempt(spec_a).expect("register A");
    authority.register_attempt(spec_b).expect("register B");

    let barrier = Barrier::new(2);
    let authority = &authority;
    let (first, second) = std::thread::scope(|scope| {
        let race_a = scope.spawn(|| {
            barrier.wait();
            authority.request_commit_permit(permit_request(&spec_a, 0x01))
        });
        let race_b = scope.spawn(|| {
            barrier.wait();
            authority.request_commit_permit(permit_request(&spec_b, 0x02))
        });
        (
            race_a.join().expect("thread A").expect("request A"),
            race_b.join().expect("thread B").expect("request B"),
        )
    });

    let outcomes = [first, second];
    let winners: Vec<_> = outcomes
        .iter()
        .filter(|decision| matches!(decision, PermitDecision::Issued(_)))
        .collect();
    let losers: Vec<_> = outcomes
        .iter()
        .filter(|decision| matches!(decision, PermitDecision::Superseded { .. }))
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one permit may be issued: {outcomes:?}"
    );
    assert_eq!(
        losers.len(),
        1,
        "the loser is durably superseded: {outcomes:?}"
    );

    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.permit_epoch, 1);
    assert!(head.active_permit.is_some());
}
