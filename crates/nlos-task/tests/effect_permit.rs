//! Acceptance tests for the B-TASK second slice: `EffectPermit` issuance
//! with one-shot dispatch tokens and the per-slot `EffectSlot` state
//! machine on top of the B-TASK-001 `TaskAuthority`.
//!
//! Every test maps to one acceptance bullet of the B-TASK-002 slice
//! (discussions 31 evidence gate items 4+7, permit dimension).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptSpec, CancelRequest, EffectPermitDecision, EffectPermitRequest, EffectReceiptDecision,
    FinalizeDecision, FinalizeRequest, IssuedPermit, LogicalEffectDescriptor, NoEffectReason,
    NoEffectRequest, Outcome, OutcomeRequest, PermitDecision, PermitRecord, PermitRequest,
    PlannedEffect, ReceiptKind, SlotState, SnapshotBundle, SqliteTaskAuthority, TaskSpec,
    TaskStoreError, empty_effect_history_root, empty_effect_set_root, idempotency_identity_digest,
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
            "nlos-task-effect-{name}-{}-{sequence}.sqlite3",
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

fn descriptor(stable_action_slot: u64) -> LogicalEffectDescriptor {
    LogicalEffectDescriptor {
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        intent_spec_id: [0x44; 32],
        stable_action_slot,
        target_authority_object_id: [0x55; 32],
        effect_class: 7,
        idempotency_scope: 3,
    }
}

fn planned(stable_action_slot: u64, required: bool) -> PlannedEffect {
    PlannedEffect {
        descriptor: descriptor(stable_action_slot),
        required,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }
}

fn permit_request(spec: &AttemptSpec, seed: u8, effects: Vec<PlannedEffect>) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [seed; 32],
        planned_effects: effects,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xb0 + seed)),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

fn effect_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    key_seed: u8,
) -> EffectPermitRequest {
    EffectPermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        valid_until_ms: 9_999,
        requested_at_ms: 4_000,
    }
}

fn dispatch_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    issued: &IssuedPermit,
) -> nlos_task::DispatchRequest {
    nlos_task::DispatchRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_permit_id: issued.effect_permit_id,
        dispatch_token: issued.one_shot_dispatch_token,
        dispatched_at_ms: 5_000,
    }
}

fn outcome_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    outcome: Outcome,
) -> OutcomeRequest {
    OutcomeRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        outcome,
        recorded_at_ms: 6_000,
    }
}

fn no_effect_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    reason: NoEffectReason,
    token: Option<[u8; 32]>,
) -> NoEffectRequest {
    NoEffectRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        reason,
        dispatch_token: token,
        recorded_at_ms: 6_000,
    }
}

fn finalize_request(spec: &AttemptSpec, permit_id: CommitPermitId) -> FinalizeRequest {
    FinalizeRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id,
        new_effect_history_root: [0x31; 32],
        new_retry_fence_epoch: 0,
        finalized_at_ms: 7_000,
    }
}

fn cancel_request(seed: u8) -> CancelRequest {
    CancelRequest {
        task_id: task_id(),
        idempotency_key: IdempotencyKey::from_bytes(bytes(seed)),
        requested_at_ms: 8_000,
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

/// Registers a task plus two competing attempts, and issues the winner's
/// `CommitPermit` with the given declared effect set.
fn setup_winner(
    effects: Vec<PlannedEffect>,
) -> (TestDatabase, AttemptSpec, AttemptSpec, PermitRecord) {
    let database = TestDatabase::new("setup");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let bundle = snapshot(0, 0);
    let spec_a = attempt_spec(0x0a, bundle);
    let spec_b = attempt_spec(0x0b, bundle);
    authority.register_attempt(spec_a).expect("register A");
    authority.register_attempt(spec_b).expect("register B");
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01, effects))
            .expect("permit A"),
    );
    drop(authority);
    (database, spec_a, spec_b, permit)
}

/// Bullet: `LogicalEffectId` / `idempotency_identity_digest` follow the
/// fixed domain-separated formulas and are attempt-independent
/// (`[TASK-EFFECT-ID-001]`). The descriptor type has no
/// attempt/action/operation/incarnation/nonce fields by construction.
#[test]
fn logical_effect_identity_is_deterministic_and_attempt_independent() {
    let first = descriptor(0);
    let second = descriptor(0);
    assert_eq!(first.logical_effect_id(), second.logical_effect_id());
    assert_eq!(
        first.idempotency_identity_digest(),
        idempotency_identity_digest(&first.logical_effect_id()),
        "identity digest is the fixed formula over LogicalEffectId"
    );
    assert_ne!(
        first.idempotency_identity_digest(),
        first.logical_effect_id(),
        "domain separation must distinguish the two digests"
    );

    let mut other_slot = descriptor(1);
    assert_ne!(first.logical_effect_id(), other_slot.logical_effect_id());
    other_slot = descriptor(0);
    other_slot.effect_class = 8;
    assert_ne!(first.logical_effect_id(), other_slot.logical_effect_id());

    // Nothing attempt-shaped can enter the identity: two attempts declaring
    // the same descriptor commit the same LogicalEffectId.
    let (database, _spec_a, _spec_b, permit) = setup_winner(vec![planned(0, true)]);
    let authority = database.open();
    let slots = authority
        .list_effect_slots(permit.permit_id)
        .expect("slots");
    assert_eq!(
        slots[0].logical_effect_id,
        descriptor(0).logical_effect_id()
    );
    assert_eq!(
        slots[0].idempotency_identity_digest,
        descriptor(0).idempotency_identity_digest()
    );
}

/// Bullet: the `effect_set_root` committed at permit issuance covers the
/// complete, densely sequenced planned slot set (`[TASK-EFFECT-002]`
/// front); invalid sets fail closed before any permit exists.
#[test]
fn permit_issuance_commits_dense_unique_effect_set() {
    let (database, _spec_a, _spec_b, permit) =
        setup_winner(vec![planned(0, true), planned(1, false)]);
    let authority = database.open();

    let slots = authority
        .list_effect_slots(permit.permit_id)
        .expect("slots");
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0].effect_seq, 0);
    assert_eq!(slots[1].effect_seq, 1);
    assert!(slots.iter().all(|slot| slot.state == SlotState::Planned));
    assert!(slots.iter().all(|slot| slot.state_seq == 0));
    assert_ne!(slots[0].effect_slot_id, slots[1].effect_slot_id);

    let summary = authority
        .inspect_effect_set(permit.permit_id)
        .expect("summary")
        .expect("declared set");
    assert_ne!(summary.effect_set_root, empty_effect_set_root());
    assert_eq!(summary.required_effect_count, 1);
    assert_eq!(summary.satisfied_required_effect_count, 0);
    assert_eq!(summary.terminal_effect_count, 0);
    assert_eq!(summary.outstanding_effect_root, empty_outstanding_marker());

    // A duplicate LogicalEffectId inside one set rejects the whole permit
    // request fail-closed: no permit, no slots, no epoch advance.
    let database2 = TestDatabase::new("duplicate-set");
    let authority2 = database2.open();
    authority2
        .register_task(task_spec())
        .expect("register task");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    authority2.register_attempt(spec).expect("register");
    let duplicate = permit_request(&spec, 0x01, vec![planned(0, true), planned(0, false)]);
    assert!(matches!(
        authority2.request_commit_permit(duplicate),
        Err(TaskStoreError::InvalidEffectSet { .. })
    ));
    let head = authority2.inspect_task(task_id()).expect("head");
    assert_eq!(head.permit_epoch, 0, "rejected set must not issue a permit");
    assert_eq!(head.active_permit, None);

    // A descriptor bound to another task/generation is likewise rejected.
    let mut foreign = planned(0, true);
    foreign.descriptor.task_generation =
        Generation::INITIAL.checked_next().expect("next generation");
    assert!(matches!(
        authority2.request_commit_permit(permit_request(&spec, 0x02, vec![foreign])),
        Err(TaskStoreError::InvalidEffectSet { .. })
    ));
}

/// Marker helper: the issued/outstanding roots of a fresh set equal the
/// domain-separated hash over zero entries.
fn empty_outstanding_marker() -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-effect-outstanding/v1");
    hasher.finalize().into()
}

/// Bullet: only the outstanding `CommitPermit` holder can obtain an
/// `EffectPermit` for a declared slot (`[TASK-RACE-001]`); the competing
/// loser asking for the SAME `LogicalEffectId` is fenced.
#[test]
fn only_commit_permit_holder_obtains_effect_permit() {
    let (database, spec_a, spec_b, permit) = setup_winner(vec![planned(0, true)]);
    let authority = database.open();
    authority
        .request_commit_permit(permit_request(&spec_b, 0x02, vec![planned(0, true)]))
        .expect("B loses the commit CAS");

    // The loser presenting the winner's permit binding is not the holder.
    assert!(matches!(
        authority.request_effect_permit(effect_request(&spec_b, &permit, 0, 0xe1)),
        Err(TaskStoreError::NotPermitHolder)
    ));
    // A stale attempt generation for the holder is fenced.
    let mut stale = effect_request(&spec_a, &permit, 0, 0xe2);
    stale.attempt_generation = Generation::INITIAL.checked_next().expect("next generation");
    assert!(matches!(
        authority.request_effect_permit(stale),
        Err(TaskStoreError::InvalidGeneration)
    ));
    // A stale permit epoch is fenced.
    let mut wrong_epoch = effect_request(&spec_a, &permit, 0, 0xe3);
    wrong_epoch.permit_epoch = 99;
    assert!(matches!(
        authority.request_effect_permit(wrong_epoch),
        Err(TaskStoreError::PermitEpochMismatch)
    ));
    // A slot outside the declared set does not exist.
    assert!(matches!(
        authority.request_effect_permit(effect_request(&spec_a, &permit, 7, 0xe4)),
        Err(TaskStoreError::EffectSlotNotFound)
    ));
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(
        slot.state,
        SlotState::Planned,
        "no failed request may move the slot"
    );
}

/// Bullet: issuance CAS moves the slot `PLANNED → PERMITTED`, mints the
/// one-shot token, and updates issued/outstanding roots
/// (`[TASK-EFFECT-001]` first half); replay returns the original permit and
/// token, conflicting bytes fail closed.
#[test]
fn issuance_cas_moves_slot_to_permitted_and_replays_original() {
    let (database, spec_a, _spec_b, permit) = setup_winner(vec![planned(0, true)]);
    let authority = database.open();
    let before = authority
        .inspect_effect_set(permit.permit_id)
        .expect("summary")
        .expect("declared set");
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
            .expect("issue effect permit"),
    );
    assert_eq!(issued.permit_id, permit.permit_id);
    assert_eq!(issued.permit_epoch, permit.permit_epoch);
    assert_eq!(issued.effect_seq, 0);
    assert_eq!(issued.logical_effect_id, descriptor(0).logical_effect_id());
    assert_eq!(issued.effect_set_root, before.effect_set_root);
    assert_eq!(issued.cancel_epoch, 0);

    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Permitted);
    assert_eq!(slot.state_seq, 1);
    assert_eq!(slot.effect_permit_id, Some(issued.effect_permit_id));

    let after = authority
        .inspect_effect_set(permit.permit_id)
        .expect("summary")
        .expect("declared set");
    assert_ne!(
        after.issued_effect_root, before.issued_effect_root,
        "issued root must cover the newly permitted slot"
    );
    assert_ne!(
        after.outstanding_effect_root,
        before.outstanding_effect_root
    );
    assert_ne!(after.effect_slot_state_root, before.effect_slot_state_root);
    assert_eq!(
        after.effect_set_root, before.effect_set_root,
        "set root is immutable"
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before + 1,
        "issuance CAS advances the control epoch"
    );

    // Same key + same bytes replays the original permit with the same token.
    match authority
        .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
        .expect("replay")
    {
        EffectPermitDecision::Replayed(original) => {
            assert_eq!(original.effect_permit_id, issued.effect_permit_id);
            assert_eq!(
                original.one_shot_dispatch_token,
                issued.one_shot_dispatch_token
            );
        }
        other @ EffectPermitDecision::Issued(_) => panic!("expected Replayed, got {other:?}"),
    }
    // Same key + different bytes fails closed.
    let mut conflicting = effect_request(&spec_a, &permit, 0, 0xe1);
    conflicting.valid_until_ms = 1;
    assert!(matches!(
        authority.request_effect_permit(conflicting),
        Err(TaskStoreError::IdempotencyConflict)
    ));
    // A second permit for the same slot under a new key is impossible.
    assert!(matches!(
        authority.request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe5)),
        Err(TaskStoreError::InvalidEffectSlotState {
            state: SlotState::Permitted
        })
    ));
}

/// Bullet: the dispatch token is consumed atomically and exactly once
/// (`PERMITTED → DISPATCHED`); a second spend fails closed and never
/// silently re-dispatches (`[TASK-EFFECT-001]` second half).
#[test]
fn dispatch_token_is_single_use_and_fail_closed() {
    let (database, spec_a, _spec_b, permit) = setup_winner(vec![planned(0, true)]);
    let authority = database.open();
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
            .expect("issue"),
    );

    // A wrong token does not move the slot.
    let mut forged = dispatch_request(&spec_a, &permit, &issued);
    forged.dispatch_token = [0xee; 32];
    assert!(matches!(
        authority.consume_dispatch_token(forged),
        Err(TaskStoreError::DispatchTokenMismatch)
    ));
    assert_eq!(
        authority
            .inspect_effect_slot(permit.permit_id, 0)
            .expect("slot")
            .state,
        SlotState::Permitted
    );

    let dispatched = authority
        .consume_dispatch_token(dispatch_request(&spec_a, &permit, &issued))
        .expect("consume");
    assert_eq!(dispatched.state, SlotState::Dispatched);
    assert_eq!(dispatched.state_seq, 2);

    // The same token a second time: fail closed, never re-dispatch.
    assert!(matches!(
        authority.consume_dispatch_token(dispatch_request(&spec_a, &permit, &issued)),
        Err(TaskStoreError::DispatchTokenConsumed)
    ));
    // A wrong token on a dispatched slot is a plain mismatch.
    assert!(matches!(
        authority.consume_dispatch_token(forged),
        Err(TaskStoreError::DispatchTokenMismatch)
    ));
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Dispatched);
    assert_eq!(
        slot.state_seq, 2,
        "failed consumes must not advance state_seq"
    );
}

/// Bullet: under true concurrency two consumers of the same token produce
/// exactly one `DISPATCHED` and one fail-closed rejection.
#[test]
fn concurrent_token_consumption_has_exactly_one_winner() {
    let (database, spec_a, _spec_b, permit) = setup_winner(vec![planned(0, true)]);
    let authority = database.open();
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
            .expect("issue"),
    );

    let barrier = Barrier::new(2);
    let authority = &authority;
    let (first, second) = std::thread::scope(|scope| {
        let race_a = scope.spawn(|| {
            barrier.wait();
            authority.consume_dispatch_token(dispatch_request(&spec_a, &permit, &issued))
        });
        let race_b = scope.spawn(|| {
            barrier.wait();
            authority.consume_dispatch_token(dispatch_request(&spec_a, &permit, &issued))
        });
        (
            race_a.join().expect("thread A"),
            race_b.join().expect("thread B"),
        )
    });

    let outcomes = [first, second];
    let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    let consumed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Err(TaskStoreError::DispatchTokenConsumed)))
        .count();
    assert_eq!(winners, 1, "exactly one consumption wins: {outcomes:?}");
    assert_eq!(consumed, 1, "the loser fails closed: {outcomes:?}");
    assert_eq!(
        authority
            .inspect_effect_slot(permit.permit_id, 0)
            .expect("slot")
            .state,
        SlotState::Dispatched
    );
}

/// Bullet: a cancellation committed after `EffectPermit` issuance fences
/// the late permit at dispatch time with a typed rejection, the slot stays
/// `PERMITTED`, and the cancel path closes it as no-effect with the
/// unconsumed-token proof (`[TASK-CANCEL-003]`). A token consumed BEFORE
/// the cancel reconciles by its real effect and can never masquerade as
/// unexecuted.
#[test]
fn cancel_fences_late_dispatch_and_preserves_pre_cancel_window() {
    let (database, spec_a, _spec_b, permit) =
        setup_winner(vec![planned(0, true), planned(1, false)]);
    let authority = database.open();
    let issued_0 = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
            .expect("issue slot 0"),
    );
    // Slot 1 is dispatched before the cancel lands.
    let issued_1 = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_a, &permit, 1, 0xe2))
            .expect("issue slot 1"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec_a, &permit, &issued_1))
        .expect("dispatch slot 1 pre-cancel");

    authority.cancel_task(cancel_request(0xd1)).expect("cancel");

    // The late permit for slot 0 is fenced at dispatch; slot stays PERMITTED.
    assert!(matches!(
        authority.consume_dispatch_token(dispatch_request(&spec_a, &permit, &issued_0)),
        Err(TaskStoreError::CancellationCommitted { cancel_epoch: 1 })
    ));
    assert_eq!(
        authority
            .inspect_effect_slot(permit.permit_id, 0)
            .expect("slot 0")
            .state,
        SlotState::Permitted
    );
    // No new effect permit may be issued after the cancel committed.
    assert!(matches!(
        authority.request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe9)),
        Err(TaskStoreError::CancellationCommitted { cancel_epoch: 1 })
    ));

    // The cancel path closes slot 0 as no-effect: the unconsumed token is
    // the proof.
    let decision = authority
        .record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            0,
            NoEffectReason::CancelledBeforeDispatch,
            Some(issued_0.one_shot_dispatch_token),
        ))
        .expect("close slot 0 as no-effect");
    match decision {
        EffectReceiptDecision::Recorded(receipt) => {
            assert_eq!(receipt.kind, ReceiptKind::NoEffect);
            assert_eq!(receipt.prior_slot_state, SlotState::Permitted);
            assert_eq!(
                receipt.no_effect_reason,
                Some(NoEffectReason::CancelledBeforeDispatch)
            );
        }
        other @ EffectReceiptDecision::Replayed(_) => {
            panic!("expected Recorded, got {other:?}")
        }
    }

    // Slot 1's token was consumed pre-cancel: it can never be renamed
    // no-effect, but its real outcome may still be registered post-cancel.
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            1,
            NoEffectReason::CancelledBeforeDispatch,
            Some(issued_1.one_shot_dispatch_token),
        )),
        Err(TaskStoreError::InvalidEffectSlotState {
            state: SlotState::Dispatched
        })
    ));
    let outcome = authority
        .record_effect_outcome(outcome_request(
            &spec_a,
            &permit,
            1,
            Outcome::Unknown {
                uncertainty_digest: [0x99; 32],
            },
        ))
        .expect("post-cancel uncertainty registration");
    assert!(matches!(outcome, EffectReceiptDecision::Recorded(_)));
}

/// Bullet: no-effect closure requires a verifiably unconsumed token
/// (`PLANNED`/`PERMITTED → NO_EFFECT`); the condition-not-applicable reason
/// requires the pre-bound condition digest.
#[test]
fn no_effect_requires_verifiably_unconsumed_token() {
    let (database, spec_a, _spec_b, permit) =
        setup_winner(vec![planned(0, true), planned(1, false)]);
    let authority = database.open();

    // A PLANNED slot has no token; presenting one is a mismatch.
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            1,
            NoEffectReason::NotSelected,
            Some([0x01; 32]),
        )),
        Err(TaskStoreError::DispatchTokenMismatch)
    ));
    // PLANNED → NO_EFFECT with no token.
    authority
        .record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            1,
            NoEffectReason::NotSelected,
            None,
        ))
        .expect("planned slot closes as no-effect");
    // CONDITION_NOT_APPLICABLE requires the pre-bound condition digest.
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            0,
            NoEffectReason::ConditionNotApplicable,
            None,
        )),
        Err(TaskStoreError::ConditionNotBound)
    ));

    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
            .expect("issue slot 0"),
    );
    // PERMITTED → NO_EFFECT without the token, or with a wrong one, fails.
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            0,
            NoEffectReason::PolicySkipped,
            None,
        )),
        Err(TaskStoreError::DispatchTokenMismatch)
    ));
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            0,
            NoEffectReason::PolicySkipped,
            Some([0x02; 32]),
        )),
        Err(TaskStoreError::DispatchTokenMismatch)
    ));
    let recorded = authority
        .record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            0,
            NoEffectReason::PolicySkipped,
            Some(issued.one_shot_dispatch_token),
        ))
        .expect("unconsumed token proves no-effect");
    let receipt_id = match recorded {
        EffectReceiptDecision::Recorded(receipt) => receipt.receipt_id,
        other @ EffectReceiptDecision::Replayed(_) => panic!("expected Recorded, got {other:?}"),
    };
    // Same reason replays; a different reason for the same slot fails closed.
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            0,
            NoEffectReason::PolicySkipped,
            Some(issued.one_shot_dispatch_token),
        )),
        Ok(EffectReceiptDecision::Replayed(_))
    ));
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            0,
            NoEffectReason::NotSelected,
            Some(issued.one_shot_dispatch_token),
        )),
        Err(TaskStoreError::IdempotencyConflict)
    ));
    let receipt = authority
        .inspect_effect_receipt(receipt_id)
        .expect("receipt");
    assert_eq!(receipt.prior_slot_state, SlotState::Permitted);
}

/// Bullet: finalize tightening — any `PLANNED`/`PERMITTED`/`DISPATCHED`/
/// `EFFECT_UNKNOWN` slot forbids closing the permit; commit requires every
/// declared slot in `EFFECT_CLOSED` or `NO_EFFECT` (`[TASK-COMMIT-002]`
/// subset).
#[test]
fn finalize_blocked_until_every_declared_slot_is_terminal() {
    let (database, spec_a, _spec_b, permit) =
        setup_winner(vec![planned(0, true), planned(1, false)]);
    let authority = database.open();

    // Both slots PLANNED.
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec_a, permit.permit_id)),
        Err(TaskStoreError::OutstandingEffectSlots { count: 2 })
    ));
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
            .expect("issue slot 0"),
    );
    // PERMITTED + PLANNED.
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec_a, permit.permit_id)),
        Err(TaskStoreError::OutstandingEffectSlots { count: 2 })
    ));
    authority
        .consume_dispatch_token(dispatch_request(&spec_a, &permit, &issued))
        .expect("dispatch slot 0");
    // DISPATCHED + PLANNED.
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec_a, permit.permit_id)),
        Err(TaskStoreError::OutstandingEffectSlots { count: 2 })
    ));
    authority
        .record_effect_outcome(outcome_request(
            &spec_a,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect("close slot 0");
    // EFFECT_CLOSED + PLANNED.
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec_a, permit.permit_id)),
        Err(TaskStoreError::OutstandingEffectSlots { count: 1 })
    ));
    authority
        .record_no_effect(no_effect_request(
            &spec_a,
            &permit,
            1,
            NoEffectReason::NotSelected,
            None,
        ))
        .expect("no-effect slot 1");

    let decision = authority
        .finalize_commit(finalize_request(&spec_a, permit.permit_id))
        .expect("finalize with all slots terminal");
    match decision {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.new_head_commit_seq, 1);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    let summary = authority
        .inspect_effect_set(permit.permit_id)
        .expect("summary")
        .expect("declared set");
    assert_eq!(summary.terminal_effect_count, 2);
    assert_eq!(summary.satisfied_required_effect_count, 1);
}

/// Bullet: crash-window registration — the caller reports uncertainty and
/// the slot becomes durably `EFFECT_UNKNOWN`, which blocks permit closure
/// across restarts (quarantine flow is the next slice).
#[test]
fn effect_unknown_is_durable_and_blocks_closure_across_restart() {
    let (database, spec_a, _spec_b, permit) = setup_winner(vec![planned(0, true)]);
    let uncertainty = Outcome::Unknown {
        uncertainty_digest: [0x99; 32],
    };
    {
        let authority = database.open();
        let issued = issued_effect_permit(
            authority
                .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
                .expect("issue"),
        );
        authority
            .consume_dispatch_token(dispatch_request(&spec_a, &permit, &issued))
            .expect("dispatch");
        authority
            .record_effect_outcome(outcome_request(&spec_a, &permit, 0, uncertainty))
            .expect("register uncertainty");
    }

    let authority = database.open();
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot after restart");
    assert_eq!(slot.state, SlotState::EffectUnknown);
    assert!(
        matches!(
            authority.finalize_commit(finalize_request(&spec_a, permit.permit_id)),
            Err(TaskStoreError::OutstandingEffectSlots { count: 1 })
        ),
        "EFFECT_UNKNOWN durably blocks closure"
    );
    // Exact replay returns the original receipt; a conflicting digest and a
    // closure rewrite are both refused (reconcile is the next slice).
    assert!(matches!(
        authority.record_effect_outcome(outcome_request(&spec_a, &permit, 0, uncertainty)),
        Ok(EffectReceiptDecision::Replayed(_))
    ));
    assert!(matches!(
        authority.record_effect_outcome(outcome_request(
            &spec_a,
            &permit,
            0,
            Outcome::Unknown {
                uncertainty_digest: [0x98; 32],
            },
        )),
        Err(TaskStoreError::IdempotencyConflict)
    ));
    assert!(matches!(
        authority.record_effect_outcome(outcome_request(
            &spec_a,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        )),
        Err(TaskStoreError::InvalidEffectSlotState {
            state: SlotState::EffectUnknown
        })
    ));
}

/// Bullet: restart recovery mid-flight — a `PERMITTED` slot, its token, and
/// the effect-set roots survive reopening; replay returns the same token.
#[test]
fn restart_recovers_permitted_slot_and_token() {
    let (database, spec_a, _spec_b, permit) = setup_winner(vec![planned(0, true)]);
    let issued = {
        let authority = database.open();
        issued_effect_permit(
            authority
                .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
                .expect("issue"),
        )
    };

    let authority = database.open();
    match authority
        .request_effect_permit(effect_request(&spec_a, &permit, 0, 0xe1))
        .expect("replay after restart")
    {
        EffectPermitDecision::Replayed(original) => {
            assert_eq!(
                original.one_shot_dispatch_token, issued.one_shot_dispatch_token,
                "replay after restart must return the same token"
            );
        }
        other @ EffectPermitDecision::Issued(_) => panic!("expected Replayed, got {other:?}"),
    }
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Permitted);
    authority
        .consume_dispatch_token(dispatch_request(&spec_a, &permit, &issued))
        .expect("consume after restart");
    authority
        .record_effect_outcome(outcome_request(
            &spec_a,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect("close after restart");
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec_a, permit.permit_id)),
        Ok(FinalizeDecision::Committed(_))
    ));
}

/// Frozen golden copy of the B-TASK-001 v1 schema (including its
/// `user_version`), intentionally duplicated so the fixture cannot drift
/// with the live source.
const GOLDEN_V1_DDL: &str =
    "CREATE TABLE tasks (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            head_commit_seq BLOB NOT NULL CHECK(length(head_commit_seq) = 8),
            head_effect_history_root BLOB NOT NULL CHECK(length(head_effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            task_state INTEGER NOT NULL,
            revision INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE task_snapshots (
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            snapshot_digest BLOB NOT NULL CHECK(length(snapshot_digest) = 32),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            effect_history_root BLOB NOT NULL CHECK(length(effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(task_id, snapshot_id),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TRIGGER task_snapshot_is_immutable
        BEFORE UPDATE ON task_snapshots
        BEGIN
            SELECT RAISE(ABORT, 'task snapshot is immutable');
        END;

        CREATE TABLE task_attempts (
            attempt_id BLOB PRIMARY KEY NOT NULL CHECK(length(attempt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
            cancellation_generation BLOB NOT NULL CHECK(length(cancellation_generation) = 8),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_state INTEGER NOT NULL,
            receipt_id BLOB CHECK(receipt_id IS NULL OR length(receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE commit_permits (
            permit_id BLOB PRIMARY KEY NOT NULL CHECK(length(permit_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            expected_effect_history_root BLOB NOT NULL CHECK(length(expected_effect_history_root) = 32),
            expected_retry_fence_epoch BLOB NOT NULL CHECK(length(expected_retry_fence_epoch) = 8),
            write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            valid_until_ms INTEGER NOT NULL,
            permit_state INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE UNIQUE INDEX commit_permits_single_active
            ON commit_permits(task_id) WHERE permit_state = 0;

        CREATE TABLE task_cancels (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            cancel_epoch_after BLOB NOT NULL CHECK(length(cancel_epoch_after) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE task_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB CHECK(permit_id IS NULL OR length(permit_id) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            outcome INTEGER NOT NULL,
            prior_head_commit_seq BLOB NOT NULL CHECK(length(prior_head_commit_seq) = 8),
            prior_effect_history_root BLOB NOT NULL CHECK(length(prior_effect_history_root) = 32),
            prior_retry_fence_epoch BLOB NOT NULL CHECK(length(prior_retry_fence_epoch) = 8),
            new_head_commit_seq BLOB NOT NULL CHECK(length(new_head_commit_seq) = 8),
            new_effect_history_root BLOB NOT NULL CHECK(length(new_effect_history_root) = 32),
            new_retry_fence_epoch BLOB NOT NULL CHECK(length(new_retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX task_receipts_by_permit
            ON task_receipts(task_id, permit_id);

        CREATE TRIGGER task_receipt_is_immutable
        BEFORE UPDATE ON task_receipts
        BEGIN
            SELECT RAISE(ABORT, 'task receipt is immutable');
        END;

        PRAGMA user_version = 1;";

fn blob8(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// Seeds a complete v1 database: one task, one snapshot, one
/// `CommitPermitted` attempt, and one outstanding permit (ID `0x77..`).
fn seed_golden_v1(database: &TestDatabase) {
    let connection = rusqlite::Connection::open(&database.path).expect("create raw v1 database");
    connection
        .execute_batch(GOLDEN_V1_DDL)
        .expect("create golden v1 schema");
    connection
        .execute(
            "INSERT INTO tasks (
                task_id, task_generation, head_commit_seq, head_effect_history_root,
                retry_fence_epoch, control_epoch, cancel_epoch, permit_epoch,
                task_state, revision, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 1, 1000, 3000)",
            rusqlite::params![
                bytes(0x01).as_slice(),
                blob8(1),
                blob8(0),
                empty_effect_history_root().as_slice(),
                blob8(0),
                blob8(2),
                blob8(0),
                blob8(1),
            ],
        )
        .expect("seed task");
    connection
        .execute(
            "INSERT INTO task_snapshots (
                task_id, snapshot_id, snapshot_digest, expected_head_commit_seq,
                effect_history_root, retry_fence_epoch, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 2000)",
            rusqlite::params![
                bytes(0x01).as_slice(),
                bytes(0x10).as_slice(),
                [0x20u8; 32].as_slice(),
                blob8(0),
                empty_effect_history_root().as_slice(),
                blob8(0),
            ],
        )
        .expect("seed snapshot");
    connection
        .execute(
            "INSERT INTO task_attempts (
                attempt_id, task_id, attempt_generation, snapshot_id,
                cancellation_scope_id, cancellation_generation, idempotency_key,
                attempt_state, receipt_id, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 10, NULL, 2000, 3000)",
            rusqlite::params![
                bytes(0x0a).as_slice(),
                bytes(0x01).as_slice(),
                blob8(1),
                bytes(0x10).as_slice(),
                bytes(0xca).as_slice(),
                blob8(1),
                bytes(0xaa).as_slice(),
            ],
        )
        .expect("seed attempt");
    connection
        .execute(
            "INSERT INTO commit_permits (
                permit_id, task_id, idempotency_key, attempt_id, attempt_generation,
                expected_head_commit_seq, expected_effect_history_root,
                expected_retry_fence_epoch, write_set_root, permit_epoch,
                control_epoch, cancel_epoch, valid_until_ms, permit_state,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 9999, 0, 3000, 3000)",
            rusqlite::params![
                bytes(0x77).as_slice(),
                bytes(0x01).as_slice(),
                bytes(0xb1).as_slice(),
                bytes(0x0a).as_slice(),
                blob8(1),
                blob8(0),
                empty_effect_history_root().as_slice(),
                blob8(0),
                [0x01u8; 32].as_slice(),
                blob8(1),
                blob8(2),
                blob8(0),
            ],
        )
        .expect("seed permit");
}

/// Bullet: schema migration v1 → v2 is lossless — every B-TASK-001 row
/// survives, old invariants stay enforceable, old flows keep working, and
/// the new effect plane is usable on the migrated database.
#[test]
fn golden_v1_database_migrates_losslessly() {
    let database = TestDatabase::new("golden-v1");
    seed_golden_v1(&database);
    let seeded_permit_id = CommitPermitId::from_bytes(bytes(0x77));

    let authority = database.open();
    {
        let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        // B-TASK-003 adaptation: the open now runs the v1 → v2 → v3
        // additive chain, so the stamped version is 3; every assertion
        // below is unchanged (v1/v2 semantics preserved bit-for-bit).
        assert_eq!(version, 9, "migration stamps the current schema version");
    }

    // All v1 data intact.
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(head.permit_epoch, 1);
    assert_eq!(head.control_epoch, 2);
    assert_eq!(head.active_permit, Some(seeded_permit_id));
    let attempt = authority
        .inspect_attempt(task_id(), TaskAttemptId::from_bytes(bytes(0x0a)))
        .expect("attempt");
    assert_eq!(attempt.state, nlos_task::AttemptState::CommitPermitted);
    let permit = authority
        .inspect_permit(task_id(), seeded_permit_id)
        .expect("permit");
    assert_eq!(permit.state, nlos_task::PermitState::Issued);
    assert_eq!(permit.write_set_root, [0x01; 32]);

    // The seeded v1 permit has no declared effects: replay and finalize
    // behave exactly as in B-TASK-001.
    let spec_a = attempt_spec(0x0a, snapshot(0, 0));
    let mut replay = permit_request(&spec_a, 0x01, Vec::new());
    replay.idempotency_key = IdempotencyKey::from_bytes(bytes(0xb1));
    match authority.request_commit_permit(replay).expect("replay") {
        PermitDecision::Replayed(original) => {
            assert_eq!(original.permit_id, seeded_permit_id);
        }
        other => panic!("expected Replayed, got {other:?}"),
    }
    assert!(
        matches!(
            authority.finalize_commit(finalize_request(&spec_a, seeded_permit_id)),
            Ok(FinalizeDecision::Committed(_))
        ),
        "a v1 no-effect permit still finalizes after migration"
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        1
    );

    // The v1 immutability triggers still enforce.
    {
        let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
        assert!(
            connection
                .execute("UPDATE task_receipts SET created_at_ms = created_at_ms", [])
                .is_err(),
            "task receipt immutability trigger must survive migration"
        );
        assert!(
            connection
                .execute(
                    "UPDATE task_snapshots SET created_at_ms = created_at_ms",
                    []
                )
                .is_err(),
            "snapshot immutability trigger must survive migration"
        );
    }

    // The effect plane is fully usable on the migrated database.
    let spec_c = attempt_spec(0x0c, snapshot(1, 0));
    authority.register_attempt(spec_c).expect("register C");
    let permit_c = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_c, 0x03, vec![planned(0, true)]))
            .expect("permit C"),
    );
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_c, &permit_c, 0, 0xe1))
            .expect("issue on migrated db"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec_c, &permit_c, &issued))
        .expect("dispatch on migrated db");
    authority
        .record_effect_outcome(outcome_request(
            &spec_c,
            &permit_c,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect("close on migrated db");
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec_c, permit_c.permit_id)),
        Ok(FinalizeDecision::Committed(_))
    ));
}

/// Bullet: migration is transactional and fail-closed — a v1 database on
/// which the v2 step cannot complete stays a complete v1 database.
#[test]
fn failed_migration_rolls_back_to_complete_v1() {
    let database = TestDatabase::new("migration-failure");
    seed_golden_v1(&database);
    {
        let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
        connection
            .execute_batch("CREATE TABLE effect_slots (bogus INTEGER) STRICT;")
            .expect("plant conflicting table");
    }

    assert!(
        SqliteTaskAuthority::open(&database.path).is_err(),
        "migration conflict must fail closed"
    );

    let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version");
    assert_eq!(version, 1, "rolled-back migration keeps the v1 stamp");
    let tasks: i64 = connection
        .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
        .expect("v1 task data intact");
    assert_eq!(tasks, 1);
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM commit_permits", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("v1 permit data intact"),
        1
    );
    // The half-created v2 tables must not exist; the planted conflict does.
    let v2_tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('permit_effect_sets', 'effect_permits', 'effect_receipts')",
            [],
            |row| row.get(0),
        )
        .expect("inspect schema");
    assert_eq!(v2_tables, 0, "no partial v2 table may survive the rollback");
}
