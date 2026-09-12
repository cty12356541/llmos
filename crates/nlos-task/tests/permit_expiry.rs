//! TK-B1 acceptance tests: the `EffectPermit` TTL (`valid_until_ms`) is
//! enforced fail-closed at both effect-plane entries — issuance
//! (`request_effect_permit`) and dispatch (`consume_dispatch_token`).
//!
//! Boundary semantics under test: a permit is expired only when the
//! request-carried observation time is STRICTLY greater than
//! `valid_until_ms`; equality (`observed_ms == valid_until_ms`) is still
//! inside the valid window. The store holds no clock — the observation
//! time is always the request's own timestamp (`requested_at_ms` /
//! `dispatched_at_ms`).

#![allow(deprecated)] // The minimal fixture copies the legacy commit-permit ladder entry.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptSpec, EffectPermitDecision, EffectPermitRequest, EffectReceiptDecision, IssuedPermit,
    LogicalEffectDescriptor, NoEffectReason, NoEffectRequest, Outcome, OutcomeRequest,
    PermitDecision, PermitRecord, PlannedEffect, SlotState, SnapshotBundle, SqliteTaskAuthority,
    TaskSpec, TaskStoreError,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId, TaskSnapshotId,
};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-permit-expiry-{name}-{}-{sequence}.sqlite3",
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

fn snapshot() -> SnapshotBundle {
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(bytes(0x10)),
        snapshot_digest: [0x20; 32],
        expected_head_commit_seq: 0,
        effect_history_root: nlos_task::empty_effect_history_root(),
        retry_fence_epoch: 0,
    }
}

fn attempt_spec() -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes(bytes(0x0a)),
        attempt_generation: Generation::INITIAL,
        snapshot: snapshot(),
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0xca)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xaa)),
        registered_at_ms: 2_000,
    }
}

fn planned(stable_action_slot: u64) -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            intent_spec_id: [0x44; 32],
            stable_action_slot,
            target_authority_object_id: [0x55; 32],
            effect_class: 7,
            idempotency_scope: 3,
        },
        required: true,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }
}

fn commit_request(effects: Vec<PlannedEffect>) -> nlos_task::PermitRequest {
    let spec = attempt_spec();
    nlos_task::PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [0x01; 32],
        planned_effects: effects,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xb1)),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

fn effect_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    key_seed: u8,
    valid_until_ms: i64,
    requested_at_ms: i64,
) -> EffectPermitRequest {
    EffectPermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        valid_until_ms,
        requested_at_ms,
    }
}

fn dispatch_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    issued: &IssuedPermit,
    dispatched_at_ms: i64,
) -> nlos_task::DispatchRequest {
    nlos_task::DispatchRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_permit_id: issued.effect_permit_id,
        dispatch_token: issued.one_shot_dispatch_token,
        dispatched_at_ms,
    }
}

fn outcome_request(spec: &AttemptSpec, permit: &PermitRecord, effect_seq: u64) -> OutcomeRequest {
    OutcomeRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        outcome: Outcome::Closed {
            authoritative_closure_digest: [0xaa; 32],
        },
        recorded_at_ms: 11_000,
    }
}

fn no_effect_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    token: Option<[u8; 32]>,
) -> NoEffectRequest {
    NoEffectRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        reason: NoEffectReason::ExpiredBeforeDispatch,
        dispatch_token: token,
        recorded_at_ms: 11_000,
    }
}

fn issued_permit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected Issued, got {other:?}"),
    }
}

fn issued_effect_permit(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Issued(record) => *record,
        other @ EffectPermitDecision::Replayed(_) => panic!("expected Issued, got {other:?}"),
    }
}

/// Registers one task with one declared effect and issues the attempt's
/// `CommitPermit` (TTL `9_999`, far above every observation time used for
/// in-window traffic; the TTL under test is the `EffectPermit`'s own).
fn setup() -> (TestDatabase, AttemptSpec, PermitRecord) {
    let database = TestDatabase::new("setup");
    let authority = database.open();
    let spec = attempt_spec();
    authority.register_task(task_spec()).expect("register task");
    authority.register_attempt(spec).expect("register attempt");
    let permit = issued_permit(
        authority
            .request_commit_permit(commit_request(vec![planned(0)]))
            .expect("commit permit"),
    );
    drop(authority);
    (database, spec, permit)
}

/// Bullet ①: a permit past its TTL can never consume the dispatch token;
/// the typed rejection carries all three fields — the expired permit's
/// identity, the TTL boundary, and the request-carried observation time.
#[test]
fn expired_effect_permit_refuses_dispatch_with_typed_error() {
    let (database, spec, permit) = setup();
    let authority = database.open();
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1, 9_000, 4_000))
            .expect("issue inside TTL"),
    );

    let error = authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued, 10_000))
        .expect_err("dispatch past valid_until_ms must fail closed");
    match error {
        TaskStoreError::PermitExpired {
            permit_id,
            valid_until_ms,
            observed_ms,
        } => {
            assert_eq!(permit_id, issued.effect_permit_id);
            assert_eq!(valid_until_ms, 9_000);
            assert_eq!(observed_ms, 10_000);
        }
        other => panic!("expected PermitExpired, got {other:?}"),
    }

    // Zero dispatch side effect: the slot never left Permitted and its
    // state sequence never advanced.
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Permitted);
    assert_eq!(slot.state_seq, 1);
}

/// Bullet ②: expiry is strict — `observed_ms == valid_until_ms` is still
/// inside the valid window at BOTH effect-plane entries.
#[test]
fn valid_until_boundary_equality_still_permits_issuance_and_dispatch() {
    let (database, spec, permit) = setup();
    let authority = database.open();

    // Issuance at the boundary: requested_at_ms == valid_until_ms mints.
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1, 4_000, 4_000))
            .expect("issuance at the TTL boundary is still valid"),
    );

    // Dispatch at the boundary: dispatched_at_ms == valid_until_ms consumes.
    let dispatched = authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued, 4_000))
        .expect("dispatch at the TTL boundary is still valid");
    assert_eq!(dispatched.state, SlotState::Dispatched);
    assert_eq!(dispatched.state_seq, 2);
}

/// Bullet ③: both expiry rejections leave zero partial state, and replaying
/// the exact same request re-derives the same rejection — no idempotency
/// row is written (a written row would turn the replay into `Replayed`),
/// the slot never moves, and the task's control epoch never advances.
#[test]
fn expiry_rejection_keeps_zero_partial_state_and_replays_identically() {
    let (database, spec, permit) = setup();
    let authority = database.open();

    // Issuance: a born-expired request (requested_at_ms past its own
    // valid_until_ms) is refused twice with the identical rejection.
    let born_expired = effect_request(&spec, &permit, 0, 0xe1, 3_000, 4_000);
    let first = authority
        .request_effect_permit(born_expired)
        .expect_err("born-expired issuance must fail closed");
    let second = authority
        .request_effect_permit(born_expired)
        .expect_err("replay must re-derive the same rejection");
    assert!(matches!(
        (&first, &second),
        (
            TaskStoreError::PermitExpired {
                valid_until_ms: 3_000,
                observed_ms: 4_000,
                ..
            },
            TaskStoreError::PermitExpired { .. }
        )
    ));
    assert_eq!(first.to_string(), second.to_string());
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(
        slot.state,
        SlotState::Planned,
        "no failed mint may move the slot"
    );
    assert_eq!(slot.state_seq, 0);

    // Dispatch: an in-window permit expires; the same late request is
    // rejected twice with identical bytes and no state movement.
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe2, 9_000, 4_000))
            .expect("issue inside TTL"),
    );
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;
    let late = dispatch_request(&spec, &permit, &issued, 10_000);
    let first = authority
        .consume_dispatch_token(late)
        .expect_err("expired dispatch must fail closed");
    let second = authority
        .consume_dispatch_token(late)
        .expect_err("replay must re-derive the same rejection");
    assert!(matches!(
        (&first, &second),
        (
            TaskStoreError::PermitExpired {
                valid_until_ms: 9_000,
                observed_ms: 10_000,
                ..
            },
            TaskStoreError::PermitExpired { .. }
        )
    ));
    assert_eq!(first.to_string(), second.to_string());
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Permitted);
    assert_eq!(slot.state_seq, 1);
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before,
        "rejected dispatch must not advance the control epoch"
    );
}

/// Bullet ④: the existing rejection families around an expired permit are
/// unchanged — `record_effect_outcome` still refuses with the slot-state
/// family (the slot never reached `Dispatched`), and the caller self-report
/// no-effect closure (`ExpiredBeforeDispatch`) keeps working for the
/// still-`Permitted` slot.
#[test]
fn record_effect_outcome_rejection_family_unchanged_for_expired_permit() {
    let (database, spec, permit) = setup();
    let authority = database.open();
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1, 9_000, 4_000))
            .expect("issue inside TTL"),
    );
    assert!(matches!(
        authority.consume_dispatch_token(dispatch_request(&spec, &permit, &issued, 10_000)),
        Err(TaskStoreError::PermitExpired { .. })
    ));

    // The outcome path keeps its existing slot-state rejection: an expired
    // permit's slot is still Permitted, so outcome registration is refused
    // by the state machine, not by a new expiry path.
    assert!(matches!(
        authority.record_effect_outcome(outcome_request(&spec, &permit, 0)),
        Err(TaskStoreError::InvalidEffectSlotState {
            state: SlotState::Permitted
        })
    ));

    // The caller self-report path is deliberately untouched: the
    // unconsumed token of an expired permit still closes the slot as
    // no-effect with the caller-declared expiry reason.
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec,
            &permit,
            0,
            Some(issued.one_shot_dispatch_token)
        )),
        Ok(EffectReceiptDecision::Recorded(_))
    ));
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::NoEffect);
}
