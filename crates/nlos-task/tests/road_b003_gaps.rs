//! ROAD-B-003 task-layer gap acceptance tests (v0.5 §28.2, task slice).
//!
//! Two scenario families the existing nlos-task coverage leaves open
//! (coverage surveyed 2026-08-30 at HEAD `afd05ae`):
//!
//! 1. Cross-Task handle leakage: every prior holder-fencing test is
//!    cross-ATTEMPT inside ONE task (`NotPermitHolder`); no prior test swaps
//!    a `CommitPermit` or attempt identity across two registered Tasks. The
//!    authority scopes every handle lookup by `task_id`, so foreign-task
//!    presentation must fail typed (`PermitNotFound` / `AttemptNotFound`)
//!    with zero partial state on both Tasks, in both orthogonal directions:
//!    a foreign permit identity under this task's paths, and this task's
//!    permit context hijacked by a foreign attempt identity.
//! 2. Snapshot drift: prior tests cover a stale `expected_head_commit_seq`
//!    (after head advance) and a stale retry-fence epoch, but never an
//!    `effect_history_root`-only drift, and the terminal fence of the
//!    `Conflicted` loser (non-revivability under a fresh idempotency key)
//!    is asserted only in the slice-k vertical, never at the task layer.
//!
//! Every scenario ends with a head-monotonicity / no-double-commit
//! assertion: rejected handles write nothing, the legitimate winner
//! advances the head exactly one step per commit, and the finalize replay
//! returns the byte-equal original receipt.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptSpec, AttemptState, Authorities, ClosePermitRequest, DispatchRequest,
    EffectBindingDecision, EffectPermitDecision, FinalizeDecision, FinalizeRequest, IssuedPermit,
    NoEffectReason, NoEffectRequest, PermitClosureOutcome, PermitConflict, PermitDecision,
    PermitRecord, PermitRequest, PlannedEffect, RegisterEffectBindingRequest, SnapshotBundle,
    SqliteTaskAuthority, TaskReceiptRecord, TaskSpec, TaskStoreError, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
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
            "nlos-task-road-b003-{name}-{}-{sequence}.sqlite3",
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

/// Task one: the legitimate permit holder every misuse test attacks from.
fn task_one() -> TaskId {
    TaskId::from_bytes(bytes(0x01))
}

/// Task two: the foreign task whose paths must never accept task one's
/// handles (and whose attempt identity must never hijack task one's).
fn task_two() -> TaskId {
    TaskId::from_bytes(bytes(0x02))
}

fn task_spec(task_id: TaskId) -> TaskSpec {
    TaskSpec {
        task_id,
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
    }
}

/// A `SnapshotBundle` claiming the head was `(seq, root, fence)`. The tag
/// only separates immutable snapshot identities; the binding triples are
/// what the authority validates.
fn snapshot(tag: u8, head_seq: u64, root: [u8; 32], fence: u64) -> SnapshotBundle {
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(bytes(0x10 + tag)),
        snapshot_digest: [0x20 + tag; 32],
        expected_head_commit_seq: head_seq,
        effect_history_root: root,
        retry_fence_epoch: fence,
    }
}

/// The genesis head binding both fresh tasks share.
fn genesis_snapshot(tag: u8) -> SnapshotBundle {
    snapshot(tag, 0, empty_effect_history_root(), 0)
}

fn attempt_spec(task_id: TaskId, seed: u8, bundle: SnapshotBundle) -> AttemptSpec {
    AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes(bytes(seed)),
        attempt_generation: Generation::INITIAL,
        snapshot: bundle,
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0x30 + seed)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(seed)),
        registered_at_ms: 2_000,
    }
}

fn descriptor(task_id: TaskId, stable_action_slot: u64) -> nlos_task::LogicalEffectDescriptor {
    nlos_task::LogicalEffectDescriptor {
        task_id,
        task_generation: Generation::INITIAL,
        intent_spec_id: [0x44; 32],
        stable_action_slot,
        target_authority_object_id: [0x55; 32],
        effect_class: 7,
        idempotency_scope: 3,
    }
}

/// One non-required planned slot (no success proof needed at finalize).
fn planned(task_id: TaskId, stable_action_slot: u64) -> PlannedEffect {
    PlannedEffect {
        descriptor: descriptor(task_id, stable_action_slot),
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }
}

#[allow(clippy::too_many_arguments)]
fn effect_permit_request(
    task_id: TaskId,
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    key_seed: u8,
) -> nlos_task::EffectPermitRequest {
    nlos_task::EffectPermitRequest {
        task_id,
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
    task_id: TaskId,
    spec: &AttemptSpec,
    permit: &PermitRecord,
    issued: &IssuedPermit,
) -> DispatchRequest {
    DispatchRequest {
        task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_permit_id: issued.effect_permit_id,
        dispatch_token: issued.one_shot_dispatch_token,
        dispatched_at_ms: 5_000,
    }
}

#[allow(clippy::too_many_arguments)]
fn binding_request(
    task_id: TaskId,
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    binding: ExecutionFiberId,
    key_seed: u8,
) -> RegisterEffectBindingRequest {
    RegisterEffectBindingRequest {
        task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        binding,
        fiber_generation: Generation::new(
            std::num::NonZeroU64::new(1).expect("test generations are non-zero"),
        ),
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        registered_at_ms: 4_500,
    }
}

fn finalize_request(
    task_id: TaskId,
    spec: &AttemptSpec,
    permit: &PermitRecord,
    finalized_at_ms: i64,
) -> FinalizeRequest {
    FinalizeRequest {
        task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        new_effect_history_root: empty_effect_history_root(),
        new_retry_fence_epoch: 0,
        finalized_at_ms,
    }
}

fn close_request(
    task_id: TaskId,
    spec: &AttemptSpec,
    permit: &PermitRecord,
    closed_at_ms: i64,
) -> ClosePermitRequest {
    ClosePermitRequest {
        task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        outcome: PermitClosureOutcome::FailedBeforeEffect,
        fenced_participant_digest: [0x00; 32],
        closed_at_ms,
    }
}

/// Uniform permit-CAS call for the head-advance drift matrix.
fn permit_call(task_id: TaskId, spec: &AttemptSpec, key_seed: u8) -> PermitRequest {
    PermitRequest {
        task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [0x05; 32],
        planned_effects: vec![],
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

fn issued_permit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected Issued, got {other:?}"),
    }
}

fn committed_receipt(decision: FinalizeDecision) -> TaskReceiptRecord {
    match decision {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
}

fn replayed_receipt(decision: FinalizeDecision) -> TaskReceiptRecord {
    match decision {
        FinalizeDecision::Replayed(receipt) => *receipt,
        other @ FinalizeDecision::Committed(_) => panic!("expected Replayed, got {other:?}"),
    }
}

fn fiber(seed: u8) -> ExecutionFiberId {
    ExecutionFiberId::from_bytes([seed; 16])
}

/// Registers both tasks, one attempt per task (both on the genesis head
/// binding), and issues the winner's `CommitPermit` on task one with the
/// given declared effect set.
fn setup_two_tasks_with_holder(
    holder_effects: Vec<PlannedEffect>,
) -> (TestDatabase, AttemptSpec, AttemptSpec, PermitRecord) {
    let database = TestDatabase::new("setup");
    let authority = database.open();
    authority
        .register_task(task_spec(task_one()))
        .expect("register task one");
    authority
        .register_task(task_spec(task_two()))
        .expect("register task two");
    let holder = attempt_spec(task_one(), 0xa1, genesis_snapshot(0x01));
    let foreign = attempt_spec(task_two(), 0xa2, genesis_snapshot(0x02));
    authority
        .register_attempt(holder)
        .expect("register holder attempt");
    authority
        .register_attempt(foreign)
        .expect("register foreign attempt");
    let permit = issued_permit(
        authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                PermitRequest {
                    task_id: task_one(),
                    attempt_id: holder.attempt_id,
                    attempt_generation: holder.attempt_generation,
                    write_set_root: [0x01; 32],
                    planned_effects: holder_effects,
                    idempotency_key: IdempotencyKey::from_bytes(bytes(0xb1)),
                    valid_until_ms: 9_999,
                    requested_at_ms: 3_000,
                },
            )
            .expect("holder permit"),
    );
    drop(authority);
    (database, holder, foreign, permit)
}

/// Direction 1 of the cross-Task matrix: task one's `CommitPermit`
/// identity presented under task two's effect, fiber-binding, finalize, and
/// pre-effect-close paths. Every entry must fail typed
/// (`PermitNotFound`: permits are scoped by task) with zero partial state
/// on either task, and the legitimate holder must still be able to commit
/// afterwards (head monotonic 0→1, finalize replay byte-equal).
#[test]
fn foreign_task_permit_identity_is_rejected_in_all_task_scoped_paths() {
    let (database, holder, foreign, permit) = setup_two_tasks_with_holder(vec![]);
    let authority = database.open();

    // Task two's effect-permit path must not accept task one's permit.
    assert!(matches!(
        authority.request_effect_permit(effect_permit_request(
            task_two(),
            &foreign,
            &permit,
            0,
            0xe2,
        )),
        Err(TaskStoreError::PermitNotFound)
    ));
    // Task two's fiber-binding registration path likewise.
    assert!(matches!(
        authority.register_effect_binding(binding_request(
            task_two(),
            &foreign,
            &permit,
            0,
            fiber(0x70),
            0xd2,
        )),
        Err(TaskStoreError::PermitNotFound)
    ));
    // Task two's finalize path: the permit does not exist under task two.
    assert!(matches!(
        authority.finalize_commit(finalize_request(task_two(), &foreign, &permit, 7_000)),
        Err(TaskStoreError::PermitNotFound)
    ));
    // Task two's pre-effect close path: same task scoping.
    assert!(matches!(
        authority.close_permit(close_request(task_two(), &foreign, &permit, 7_100)),
        Err(TaskStoreError::PermitNotFound)
    ));

    // Zero partial state: task two never grew a permit, head, or epoch, and
    // task one's permit is still issued and unharmed.
    let task_two_record = authority
        .inspect_task(task_two())
        .expect("inspect task two");
    assert_eq!(task_two_record.head_commit_seq, 0);
    assert_eq!(task_two_record.active_permit, None);
    assert_eq!(task_two_record.permit_epoch, 0);
    // Tasks are born with control_epoch = 1; the failed misuse attempts
    // must not have advanced it past the genesis value.
    assert_eq!(task_two_record.control_epoch, 1);
    let task_one_record = authority
        .inspect_task(task_one())
        .expect("inspect task one");
    assert_eq!(task_one_record.head_commit_seq, 0);
    assert_eq!(task_one_record.permit_epoch, 1);
    let survivor = authority
        .inspect_permit(task_one(), permit.permit_id)
        .expect("holder permit survives foreign misuse");
    assert_eq!(survivor.state, nlos_task::PermitState::Issued);

    // The legitimate holder still finalizes: head advances exactly one step.
    let receipt = committed_receipt(
        authority
            .finalize_commit(finalize_request(task_one(), &holder, &permit, 8_000))
            .expect("holder finalize"),
    );
    assert_eq!(receipt.prior_head_commit_seq, 0);
    assert_eq!(receipt.new_head_commit_seq, 1);
    // The finalize replay is byte-equal: no double commit.
    let replay = replayed_receipt(
        authority
            .finalize_commit(finalize_request(task_one(), &holder, &permit, 8_100))
            .expect("finalize replay"),
    );
    assert_eq!(replay.receipt_id, receipt.receipt_id);
    assert_eq!(replay.new_head_commit_seq, 1);
    assert_eq!(
        authority
            .inspect_task(task_one())
            .expect("task one after commit")
            .head_commit_seq,
        1
    );

    // Positive control: task two's own attempt independently obtains its own
    // permit — task isolation never blocks legitimate foreign-task progress.
    let foreign_permit = issued_permit(
        authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                PermitRequest {
                    task_id: task_two(),
                    attempt_id: foreign.attempt_id,
                    attempt_generation: foreign.attempt_generation,
                    write_set_root: [0x02; 32],
                    planned_effects: vec![],
                    idempotency_key: IdempotencyKey::from_bytes(bytes(0xb2)),
                    valid_until_ms: 9_999,
                    requested_at_ms: 8_200,
                },
            )
            .expect("foreign task permit"),
    );
    assert_ne!(foreign_permit.permit_id, permit.permit_id);
    assert_eq!(foreign_permit.expected_head_commit_seq, 0);
    assert_eq!(
        authority
            .inspect_task(task_two())
            .expect("task two with own permit")
            .head_commit_seq,
        0
    );
}

/// Direction 2 of the cross-Task matrix: task two's attempt identity
/// hijacking task one's live permit context (same task id, same permit,
/// same epoch, even the real dispatch token). The holder check binds the
/// permit to its exact attempt, and the attempt lookup is task-scoped, so
/// every entry fails typed `AttemptNotFound` — including the fiber-binding
/// registration on a slot task one legitimately registered — with zero
/// partial state, and the real holder still completes its full effect
/// lifecycle afterwards.
#[allow(clippy::too_many_lines)] // One test covers the whole hijack matrix and the holder's recovery.
#[test]
fn foreign_attempt_identity_cannot_hijack_holder_permit_context() {
    let (database, holder, foreign, permit) =
        setup_two_tasks_with_holder(vec![planned(task_one(), 0)]);
    let authority = database.open();

    // The real holder registers its fiber binding and mints the effect
    // permit: a fully healthy context worth hijacking.
    let holder_binding = match authority
        .register_effect_binding(binding_request(
            task_one(),
            &holder,
            &permit,
            0,
            fiber(0x70),
            0xd1,
        ))
        .expect("holder registers binding")
    {
        EffectBindingDecision::Registered(record) => *record,
        EffectBindingDecision::Replayed(_) => panic!("fresh registration cannot replay"),
    };
    let issued = match authority
        .request_effect_permit(effect_permit_request(task_one(), &holder, &permit, 0, 0xe1))
        .expect("holder effect permit")
    {
        EffectPermitDecision::Issued(record) => *record,
        EffectPermitDecision::Replayed(_) => panic!("fresh effect permit cannot replay"),
    };

    // The foreign attempt identity (valid under task two) presented on task
    // one's permit context: every path rejects it as `AttemptNotFound`
    // before touching the slot, the registration, or the head.
    assert!(matches!(
        authority.request_effect_permit(effect_permit_request(
            task_one(),
            &foreign,
            &permit,
            0,
            0xe3,
        )),
        Err(TaskStoreError::AttemptNotFound)
    ));
    assert!(matches!(
        authority.register_effect_binding(binding_request(
            task_one(),
            &foreign,
            &permit,
            0,
            fiber(0x71),
            0xd3,
        )),
        Err(TaskStoreError::AttemptNotFound)
    ));
    assert!(matches!(
        authority.consume_dispatch_token(dispatch_request(task_one(), &foreign, &permit, &issued)),
        Err(TaskStoreError::AttemptNotFound)
    ));
    assert!(matches!(
        authority.finalize_commit(finalize_request(task_one(), &foreign, &permit, 7_000)),
        Err(TaskStoreError::AttemptNotFound)
    ));
    assert!(matches!(
        authority.close_permit(close_request(task_one(), &foreign, &permit, 7_100)),
        Err(TaskStoreError::AttemptNotFound)
    ));

    // Zero partial state: exactly the holder's registration exists, the slot
    // is untouched (still Permitted, token unconsumed), the permit is still
    // issued, and the head never moved.
    let listed = authority
        .list_effect_registrations_for_binding(fiber(0x70))
        .expect("list holder binding");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].registration_id, holder_binding.registration_id);
    assert!(
        authority
            .list_effect_registrations_for_binding(fiber(0x71))
            .expect("list foreign binding")
            .is_empty()
    );
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("inspect slot");
    assert_eq!(slot.state, nlos_task::SlotState::Permitted);
    assert_eq!(
        authority
            .inspect_task(task_one())
            .expect("task one after hijack attempts")
            .head_commit_seq,
        0
    );

    // The real holder closes the slot no-effect and commits: head monotonic
    // 0→1, single commit, byte-equal replay.
    let _ = authority
        .record_no_effect(NoEffectRequest {
            task_id: task_one(),
            attempt_id: holder.attempt_id,
            attempt_generation: holder.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            reason: NoEffectReason::NotSelected,
            dispatch_token: Some(issued.one_shot_dispatch_token),
            recorded_at_ms: 7_200,
        })
        .expect("holder no-effect closure");
    let receipt = committed_receipt(
        authority
            .finalize_commit(finalize_request(task_one(), &holder, &permit, 7_300))
            .expect("holder finalize"),
    );
    assert_eq!(receipt.prior_head_commit_seq, 0);
    assert_eq!(receipt.new_head_commit_seq, 1);
    let replay = replayed_receipt(
        authority
            .finalize_commit(finalize_request(task_one(), &holder, &permit, 7_400))
            .expect("finalize replay"),
    );
    assert_eq!(replay.receipt_id, receipt.receipt_id);
}

/// Snapshot drift, root-only variant: an attempt bound to a snapshot whose
/// `expected_head_commit_seq` still matches but whose
/// `effect_history_root` names a root this task never had. The permit CAS
/// must reject it as `Conflicted { StaleEffectHistoryRoot }`, durably fence
/// the attempt terminal `Conflicted`, refuse revival under a fresh
/// idempotency key, and leave the head untouched until a correctly bound
/// attempt commits exactly once.
#[test]
fn stale_effect_history_root_conflicts_fences_and_cannot_revive() {
    let database = TestDatabase::new("root-drift");
    let authority = database.open();
    authority
        .register_task(task_spec(task_one()))
        .expect("register task");
    let drift_root = [0xaa; 32];
    let drifted = attempt_spec(task_one(), 0xa1, snapshot(0x01, 0, drift_root, 0));
    authority
        .register_attempt(drifted)
        .expect("register drifted attempt");

    // The drift is invisible at registration (binding validation happens at
    // the permit CAS) and surfaces as a typed conflict there.
    let decision = authority
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id: task_one(),
                attempt_id: drifted.attempt_id,
                attempt_generation: drifted.attempt_generation,
                write_set_root: [0x03; 32],
                planned_effects: vec![],
                idempotency_key: IdempotencyKey::from_bytes(bytes(0xb1)),
                valid_until_ms: 9_999,
                requested_at_ms: 3_000,
            },
        )
        .expect("permit CAS runs");
    assert!(matches!(
        decision,
        PermitDecision::Conflicted {
            reason: PermitConflict::StaleEffectHistoryRoot
        }
    ));

    // The attempt is durably terminal `Conflicted` …
    let record = authority
        .inspect_attempt(task_one(), drifted.attempt_id)
        .expect("inspect drifted attempt");
    assert_eq!(record.state, AttemptState::Conflicted);
    assert!(record.state.is_terminal());
    // … and cannot be revived, not even under a fresh idempotency key.
    assert!(matches!(
        authority.request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id: task_one(),
                attempt_id: drifted.attempt_id,
                attempt_generation: drifted.attempt_generation,
                write_set_root: [0x03; 32],
                planned_effects: vec![],
                idempotency_key: IdempotencyKey::from_bytes(bytes(0xb9)),
                valid_until_ms: 9_999,
                requested_at_ms: 3_100,
            },
        ),
        Err(TaskStoreError::InvalidAttemptState {
            state: AttemptState::Conflicted
        })
    ));

    // Zero partial state: no permit was ever issued, the head never moved.
    let task = authority.inspect_task(task_one()).expect("inspect task");
    assert_eq!(task.head_commit_seq, 0);
    assert_eq!(task.active_permit, None);
    assert_eq!(task.permit_epoch, 0);

    // A correctly bound attempt still commits exactly once: head monotonic
    // 0→1 with a byte-equal finalize replay.
    let fresh = attempt_spec(task_one(), 0xa3, genesis_snapshot(0x03));
    authority
        .register_attempt(fresh)
        .expect("register fresh attempt");
    let fresh_permit = issued_permit(
        authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                PermitRequest {
                    task_id: task_one(),
                    attempt_id: fresh.attempt_id,
                    attempt_generation: fresh.attempt_generation,
                    write_set_root: [0x04; 32],
                    planned_effects: vec![],
                    idempotency_key: IdempotencyKey::from_bytes(bytes(0xb3)),
                    valid_until_ms: 9_999,
                    requested_at_ms: 3_200,
                },
            )
            .expect("fresh permit"),
    );
    let receipt = committed_receipt(
        authority
            .finalize_commit(finalize_request(task_one(), &fresh, &fresh_permit, 8_000))
            .expect("fresh finalize"),
    );
    assert_eq!(receipt.prior_head_commit_seq, 0);
    assert_eq!(receipt.new_head_commit_seq, 1);
    let replay = replayed_receipt(
        authority
            .finalize_commit(finalize_request(task_one(), &fresh, &fresh_permit, 8_100))
            .expect("finalize replay"),
    );
    assert_eq!(replay.receipt_id, receipt.receipt_id);
}

/// Snapshot drift after head advance: an attempt bound to the pre-commit
/// snapshot requests a permit after another attempt already committed. The
/// CAS must reject it as `Conflicted { StaleTaskHead { expected: 0,
/// current: 1 } }`, fence it terminal, refuse revival, keep rejecting every
/// later attempt bound to the same stale snapshot, and still let a fresh
/// attempt bound to the new head commit — head monotonic 0→1→2, no double
/// commit anywhere.
#[allow(clippy::too_many_lines)] // One test covers the full head-advance drift matrix.
#[test]
fn stale_snapshot_after_head_advance_conflicts_fences_and_cannot_revive() {
    let database = TestDatabase::new("head-advance");
    let authority = database.open();
    authority
        .register_task(task_spec(task_one()))
        .expect("register task");
    let stale_bundle = genesis_snapshot(0x01);
    let winner = attempt_spec(task_one(), 0xa1, stale_bundle);
    let stale = attempt_spec(task_one(), 0xa2, stale_bundle);
    authority.register_attempt(winner).expect("register winner");
    authority
        .register_attempt(stale)
        .expect("register stale attempt");

    // The winner commits: head advances 0→1.
    let winner_permit = issued_permit(
        authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                permit_call(task_one(), &winner, 0xb1),
            )
            .expect("winner permit"),
    );
    let winner_receipt = committed_receipt(
        authority
            .finalize_commit(finalize_request(task_one(), &winner, &winner_permit, 4_000))
            .expect("winner finalize"),
    );
    assert_eq!(winner_receipt.new_head_commit_seq, 1);

    // The stale attempt's permit request conflicts with the advanced head…
    let decision = authority
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            permit_call(task_one(), &stale, 0xb2),
        )
        .expect("stale permit CAS runs");
    assert!(matches!(
        &decision,
        PermitDecision::Conflicted {
            reason: PermitConflict::StaleTaskHead {
                expected: 0,
                current: 1
            }
        }
    ));
    // … is durably fenced terminal …
    let record = authority
        .inspect_attempt(task_one(), stale.attempt_id)
        .expect("inspect stale attempt");
    assert_eq!(record.state, AttemptState::Conflicted);
    // … and cannot be revived under a fresh key.
    assert!(matches!(
        authority.request_commit_permit_with_authorities_struct(
            Authorities::default(),
            permit_call(task_one(), &stale, 0xb9),
        ),
        Err(TaskStoreError::InvalidAttemptState {
            state: AttemptState::Conflicted
        })
    ));

    // A brand-new attempt bound to the SAME stale snapshot conflicts and
    // fences identically: the drift belongs to the snapshot, not the attempt.
    let stale_too = attempt_spec(task_one(), 0xa4, stale_bundle);
    authority
        .register_attempt(stale_too)
        .expect("register second stale attempt");
    let decision = authority
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            permit_call(task_one(), &stale_too, 0xb4),
        )
        .expect("second stale permit CAS runs");
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
            .inspect_attempt(task_one(), stale_too.attempt_id)
            .expect("inspect second stale attempt")
            .state,
        AttemptState::Conflicted
    );

    // Zero double commit: the head sits at exactly 1 with no outstanding
    // permit after all stale traffic.
    let task = authority.inspect_task(task_one()).expect("inspect task");
    assert_eq!(task.head_commit_seq, 1);
    assert_eq!(task.active_permit, None);

    // A fresh attempt bound to the NEW head still commits: head monotonic
    // 1→2 with its own receipt (no double commit of step 1).
    let fresh = attempt_spec(
        task_one(),
        0xa5,
        snapshot(0x05, 1, empty_effect_history_root(), 0),
    );
    authority
        .register_attempt(fresh)
        .expect("register fresh attempt");
    let fresh_permit = issued_permit(
        authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                permit_call(task_one(), &fresh, 0xb5),
            )
            .expect("fresh permit"),
    );
    assert_eq!(fresh_permit.expected_head_commit_seq, 1);
    let fresh_receipt = committed_receipt(
        authority
            .finalize_commit(finalize_request(task_one(), &fresh, &fresh_permit, 5_000))
            .expect("fresh finalize"),
    );
    assert_eq!(fresh_receipt.prior_head_commit_seq, 1);
    assert_eq!(fresh_receipt.new_head_commit_seq, 2);
    assert_ne!(fresh_receipt.receipt_id, winner_receipt.receipt_id);
    assert_eq!(
        authority
            .inspect_task(task_one())
            .expect("task one final")
            .head_commit_seq,
        2
    );
}
