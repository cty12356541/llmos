//! Acceptance tests for the B-TASK-003 cross-attempt effect history,
//! retry fence (`[TASK-EFFECT-ID-001]` / `[TASK-RETRY-EFFECT-001]`), the
//! required-slot success matrix (`[TASK-COMMIT-002]`), and the schema
//! v2 → v3 additive migration.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptSpec, ClosePermitDecision, ClosePermitRequest, EffectHistoryOutcome,
    EffectPermitDecision, EffectPermitRequest, FinalizeDecision, FinalizeRequestV3, IssuedPermit,
    LogicalEffectDescriptor, NoEffectReason, NoEffectRequest, Outcome, OutcomeRequest,
    PermitClosureOutcome, PermitConflict, PermitDecision, PermitRecord, PermitRequest, PermitState,
    PlannedEffect, RequiredSatisfaction, RequiredSatisfactionProof, SlotState, SnapshotBundle,
    SqliteTaskAuthority, TaskSpec, TaskStoreError, effect_history_root_of,
    empty_effect_history_root, expected_success_assertion_digest,
};
use nlos_types::{
    CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use sha2::{Digest, Sha256};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-history-{name}-{}-{sequence}.sqlite3",
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

fn snapshot(head_seq: u64, root: [u8; 32], fence: u64) -> SnapshotBundle {
    let tag = u8::try_from(head_seq).expect("test head fits in u8");
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(bytes(0x10 + tag)),
        snapshot_digest: [0x20 + tag; 32],
        expected_head_commit_seq: head_seq,
        effect_history_root: root,
        retry_fence_epoch: fence,
    }
}

fn initial_snapshot() -> SnapshotBundle {
    snapshot(0, empty_effect_history_root(), 0)
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

fn planned_conditional(stable_action_slot: u64, condition: [u8; 32]) -> PlannedEffect {
    PlannedEffect {
        descriptor: descriptor(stable_action_slot),
        required: true,
        required_condition_digest: Some(condition),
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
) -> NoEffectRequest {
    NoEffectRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        reason,
        dispatch_token: None,
        recorded_at_ms: 6_000,
    }
}

fn finalize_v3(
    spec: &AttemptSpec,
    permit_id: CommitPermitId,
    proofs: Vec<RequiredSatisfaction>,
) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: nlos_task::FinalizeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id,
            new_effect_history_root: [0u8; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms: 7_000,
        },
        required_satisfaction: proofs,
        fenced_participant_digest: [0xf1; 32],
    }
}

fn success_proof(
    authority: &SqliteTaskAuthority,
    permit_id: CommitPermitId,
    effect_seq: u64,
) -> RequiredSatisfaction {
    let slot = authority
        .inspect_effect_slot(permit_id, effect_seq)
        .expect("effect slot");
    let receipt = authority
        .inspect_effect_receipt(slot.effect_receipt_id.expect("effect receipt"))
        .expect("effect receipt record");
    RequiredSatisfaction {
        effect_seq,
        proof: RequiredSatisfactionProof::EffectClosedSuccess {
            success_assertion_digest: expected_success_assertion_digest(&slot, &receipt),
        },
    }
}

fn close_request(spec: &AttemptSpec, permit_id: CommitPermitId) -> ClosePermitRequest {
    ClosePermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id,
        outcome: PermitClosureOutcome::FailedBeforeEffect,
        fenced_participant_digest: [0xf1; 32],
        closed_at_ms: 7_000,
    }
}

fn issued_permit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected Issued, got {other:?}"),
    }
}

const CNA_CONDITION: [u8; 32] = [0x88; 32];

fn expected_cna_proof(snapshot_digest: [u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-condition-false-proof/v1");
    hasher.update(snapshot_digest);
    hasher.update(CNA_CONDITION);
    hasher.finalize().into()
}

fn cna_proof(digest: [u8; 32]) -> RequiredSatisfaction {
    RequiredSatisfaction {
        effect_seq: 0,
        proof: RequiredSatisfactionProof::ConditionNotApplicable {
            condition_false_proof_digest: digest,
        },
    }
}

fn issued_effect_permit(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Issued(record) => *record,
        other @ EffectPermitDecision::Replayed(_) => panic!("expected Issued, got {other:?}"),
    }
}

fn setup(effects: Vec<PlannedEffect>) -> (TestDatabase, AttemptSpec, PermitRecord) {
    let database = TestDatabase::new("history");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let spec = attempt_spec(0x0a, initial_snapshot());
    authority.register_attempt(spec).expect("register attempt");
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec, 0x01, effects))
            .expect("permit"),
    );
    drop(authority);
    (database, spec, permit)
}

fn close_slot(
    authority: &SqliteTaskAuthority,
    spec: &AttemptSpec,
    permit: &PermitRecord,
    seq: u64,
) {
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(
                spec,
                permit,
                seq,
                0xe0 + u8::try_from(seq).expect("test seq fits in u8"),
            ))
            .expect("issue"),
    );
    authority
        .consume_dispatch_token(dispatch_request(spec, permit, &issued))
        .expect("dispatch");
    authority
        .record_effect_outcome(outcome_request(
            spec,
            permit,
            seq,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect("close");
}

/// Bullet: the empty history hashes to exactly the B-TASK-001 initial
/// head formula — bit-compatible (`[TASK-EFFECT-ID-001]`).
#[test]
fn empty_history_root_is_bit_compatible_with_initial_head() {
    assert_eq!(effect_history_root_of(&[]), empty_effect_history_root());
    let database = TestDatabase::new("empty-root");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    assert_eq!(
        authority
            .compute_effect_history_root(task_id())
            .expect("root"),
        empty_effect_history_root(),
        "a fresh task's recomputed root equals the fixed initial formula"
    );
}

/// Bullet: history entries are appended in the SAME transaction that
/// closes a slot with an effect, the sequence is strictly increasing
/// from 1 with no gaps, and the root is recomputed per append
/// (`[TASK-EFFECT-ID-001]`).
#[test]
fn history_entries_append_atomically_gapless_with_recomputed_root() {
    let (database, spec, permit) = setup(vec![planned(0, true), planned(1, false)]);
    let authority = database.open();
    let root0 = authority
        .compute_effect_history_root(task_id())
        .expect("root 0");
    assert_eq!(root0, empty_effect_history_root());

    close_slot(&authority, &spec, &permit, 0);
    let root1 = authority
        .compute_effect_history_root(task_id())
        .expect("root 1");
    assert_ne!(root1, root0, "root recomputed on first append");
    close_slot(&authority, &spec, &permit, 1);
    let root2 = authority
        .compute_effect_history_root(task_id())
        .expect("root 2");
    assert_ne!(root2, root1, "root recomputed on second append");

    let entries = authority.list_effect_history(task_id()).expect("history");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].effect_history_seq, 1);
    assert_eq!(entries[1].effect_history_seq, 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.outcome == EffectHistoryOutcome::EffectClosed
                && entry.retry_fence_epoch == 0
                && entry.operation_id.is_none()
                && entry.compensation_receipt_id.is_none())
    );
    assert_eq!(
        entries[0].logical_effect_id,
        descriptor(0).logical_effect_id()
    );
    assert_eq!(
        entries[1].logical_effect_id,
        descriptor(1).logical_effect_id()
    );
    assert_eq!(
        entries[0].idempotency_identity_digest,
        descriptor(0).idempotency_identity_digest()
    );
    assert_eq!(
        root2,
        effect_history_root_of(&entries),
        "durable root equals the fixed formula over canonical entries"
    );

    // Restart: entries and root survive; the table is immutable.
    drop(authority);
    let authority = database.open();
    assert_eq!(
        authority.list_effect_history(task_id()).expect("history"),
        entries
    );
    let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
    assert!(
        connection
            .execute("UPDATE effect_history SET outcome = outcome", [])
            .is_err(),
        "effect history immutability trigger must enforce"
    );
}

/// Bullet: `[TASK-RETRY-EFFECT-001]` — required unsatisfied with effects
/// happened and at least one required satisfied → `PARTIAL_EFFECT`: the
/// history gains a `PartialEffect` entry, the fence strictly increments,
/// and head/root/fence advance in the same CAS; stale-fence snapshots
/// are CONFLICTED and new attempts inherit the new root/epoch.
#[test]
fn partial_effect_advances_head_root_and_fence_and_new_attempts_inherit() {
    let (database, spec, permit) =
        setup(vec![planned(0, true), planned(1, true), planned(2, false)]);
    let authority = database.open();
    close_slot(&authority, &spec, &permit, 0);
    authority
        .record_no_effect(no_effect_request(
            &spec,
            &permit,
            1,
            NoEffectReason::NotSelected,
        ))
        .expect("required slot 1 skipped (plain no-effect)");
    close_slot(&authority, &spec, &permit, 2);

    let receipt = match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
        ))
        .expect("partial finalize")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(receipt.outcome, nlos_task::ReceiptOutcome::PartialEffect);
    assert_eq!(receipt.new_head_commit_seq, 1);
    assert_eq!(receipt.prior_retry_fence_epoch, 0);
    assert_eq!(receipt.new_retry_fence_epoch, 1, "fence strictly +1");

    let entries = authority.list_effect_history(task_id()).expect("history");
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].outcome, EffectHistoryOutcome::PartialEffect);
    assert_eq!(entries[2].retry_fence_epoch, 1);
    assert_eq!(
        entries[2].logical_effect_id,
        descriptor(1).logical_effect_id()
    );
    let new_root = authority
        .compute_effect_history_root(task_id())
        .expect("root");
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.retry_fence_epoch, 1);
    assert_eq!(head.head_effect_history_root, new_root);
    assert_eq!(receipt.new_effect_history_root, new_root);

    // A stale fence (or stale root) snapshot is durably CONFLICTED.
    let stale = attempt_spec(0x0b, snapshot(1, new_root, 0));
    authority.register_attempt(stale).expect("register stale");
    match authority
        .request_commit_permit(permit_request(&stale, 0x02, Vec::new()))
        .expect("stale fence")
    {
        PermitDecision::Conflicted { reason } => {
            assert_eq!(reason, PermitConflict::StaleRetryFenceEpoch);
        }
        other => panic!("expected Conflicted, got {other:?}"),
    }
    // The next attempt inherits the new root/epoch bit-for-bit.
    let mut bundle_c = snapshot(1, new_root, 1);
    bundle_c.snapshot_id = TaskSnapshotId::from_bytes(bytes(0x1c));
    let spec_c = attempt_spec(0x0c, bundle_c);
    authority.register_attempt(spec_c).expect("register C");
    let permit_c = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_c, 0x03, Vec::new()))
            .expect("permit C"),
    );
    assert_eq!(permit_c.expected_retry_fence_epoch, 1);
    assert_eq!(permit_c.expected_effect_history_root, new_root);
}

/// Bullet: required unsatisfied + effects happened + zero required
/// satisfied → `FAILED_AFTER_EFFECT` (the attempt's goal failed);
/// required unsatisfied + zero effects → typed `RequiredEffectUnsatisfied`
/// and the permit stays open for the pre-effect closure path.
#[test]
fn failed_after_effect_and_no_effect_unsatisfied_rules() {
    let (database, spec, permit) = setup(vec![planned(0, true), planned(1, false)]);
    let authority = database.open();
    authority
        .record_no_effect(no_effect_request(
            &spec,
            &permit,
            0,
            NoEffectReason::NotSelected,
        ))
        .expect("required slot skipped");
    close_slot(&authority, &spec, &permit, 1);
    let receipt = match authority
        .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new()))
        .expect("failed-after-effect finalize")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(
        receipt.outcome,
        nlos_task::ReceiptOutcome::FailedAfterEffect
    );
    assert_eq!(receipt.new_retry_fence_epoch, 1);
    assert_eq!(
        authority
            .inspect_attempt(task_id(), spec.attempt_id)
            .expect("attempt")
            .state,
        nlos_task::AttemptState::Failed
    );
    let entries = authority.list_effect_history(task_id()).expect("history");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].outcome, EffectHistoryOutcome::PartialEffect);

    // Required unsatisfied with zero effects: typed refusal, permit open,
    // and only the pre-effect closure path may close it.
    let (database2, spec2, permit2) = setup(vec![planned(0, true)]);
    let authority2 = database2.open();
    authority2
        .record_no_effect(no_effect_request(
            &spec2,
            &permit2,
            0,
            NoEffectReason::NotSelected,
        ))
        .expect("required slot skipped");
    assert!(matches!(
        authority2.finalize_commit_v3(finalize_v3(&spec2, permit2.permit_id, Vec::new())),
        Err(TaskStoreError::RequiredEffectUnsatisfied { effect_seq: 0, .. })
    ));
    assert_eq!(
        authority2
            .inspect_permit(task_id(), permit2.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    let head_before = authority2.inspect_task(task_id()).expect("head");
    match authority2
        .close_permit(close_request(&spec2, permit2.permit_id))
        .expect("closure")
    {
        ClosePermitDecision::Closed(receipt) => {
            assert_eq!(receipt.new_head_commit_seq, head_before.head_commit_seq);
            assert_eq!(
                receipt.outcome,
                nlos_task::ReceiptOutcome::FailedBeforeEffect
            );
        }
        other => panic!("expected Closed, got {other:?}"),
    }
}

/// Bullet: cross-attempt idempotency read — a later attempt reads the
/// original result back via `lookup_effect_history`, and issuing an
/// `EffectPermit` for an already-`EFFECT_CLOSED` logical effect is
/// fail-closed (`[TASK-RETRY-EFFECT-001]`).
#[test]
fn lookup_effect_history_readback_and_re_dispatch_refused() {
    let (database, spec, permit) = setup(vec![planned(0, true)]);
    let authority = database.open();
    close_slot(&authority, &spec, &permit, 0);
    authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
        ))
        .expect("finalize");
    let root = authority
        .compute_effect_history_root(task_id())
        .expect("root");

    let lookup = authority
        .lookup_effect_history(task_id(), descriptor(0).logical_effect_id())
        .expect("lookup")
        .expect("entry exists");
    assert_eq!(lookup.entry.effect_history_seq, 1);
    assert_eq!(lookup.entry.outcome, EffectHistoryOutcome::EffectClosed);
    assert_eq!(lookup.original_receipt.proof_digest, [0xaa; 32]);
    assert!(
        authority
            .lookup_effect_history(task_id(), [0xee; 32])
            .expect("lookup miss")
            .is_none()
    );

    // The next competition may declare the same logical effect (read-back
    // path) but must never silently re-dispatch it.
    let spec_b = attempt_spec(0x0b, snapshot(1, root, 0));
    authority.register_attempt(spec_b).expect("register B");
    let permit_b = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_b, 0x02, vec![planned(0, true)]))
            .expect("permit B"),
    );
    assert!(matches!(
        authority.request_effect_permit(effect_request(&spec_b, &permit_b, 0, 0xe1)),
        Err(TaskStoreError::EffectAlreadyClosed)
    ));
    let slot = authority
        .inspect_effect_slot(permit_b.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Planned, "no re-dispatch happened");
    let readback = authority
        .lookup_effect_history(task_id(), descriptor(0).logical_effect_id())
        .expect("lookup")
        .expect("original result readable");
    assert_eq!(
        readback.entry.authoritative_effect_receipt_id,
        lookup.entry.authoritative_effect_receipt_id
    );
}

/// Bullet: required-slot success matrix (`[TASK-COMMIT-002]`) —
/// `EFFECT_CLOSED` + caller-asserted success satisfies;
/// `CONDITION_NOT_APPLICABLE` with the snapshot-bound false-proof
/// satisfies; `EFFECT_CLOSED` without proof, plain `NO_EFFECT`, and a CNA
/// proof on the wrong no-effect reason never satisfy; skip is never
/// written as `COMMITTED`.
#[test]
fn required_slot_satisfaction_matrix() {
    // EFFECT_CLOSED + proof → COMMITTED; without proof → FAILED_AFTER_EFFECT.
    let (database, spec, permit) = setup(vec![planned(0, true)]);
    let authority = database.open();
    close_slot(&authority, &spec, &permit, 0);
    let receipt = match authority
        .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new()))
        .expect("no proof: failed-after-effect")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(
        receipt.outcome,
        nlos_task::ReceiptOutcome::FailedAfterEffect,
        "an unproved required closure can never be COMMITTED"
    );
    drop(authority);

    let (database, spec, permit) = setup(vec![planned(0, true)]);
    let authority = database.open();
    close_slot(&authority, &spec, &permit, 0);
    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![RequiredSatisfaction {
                effect_seq: 0,
                proof: RequiredSatisfactionProof::EffectClosedSuccess {
                    success_assertion_digest: [0x5a; 32],
                },
            }],
        )),
        Err(TaskStoreError::RequiredEffectUnsatisfied { effect_seq: 0, .. })
    ));
    let receipt = match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
        ))
        .expect("proved finalize")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(receipt.outcome, nlos_task::ReceiptOutcome::Committed);
    assert_eq!(receipt.new_retry_fence_epoch, 0, "no partial: fence stays");
    drop(authority);
}

/// Bullet: `CONDITION_NOT_APPLICABLE` satisfies a conditional required
/// slot only with the snapshot-bound condition-false proof digest; a
/// digest not bound to the snapshot fails closed (`[TASK-COMMIT-002]`).
#[test]
fn required_condition_not_applicable_satisfies_with_snapshot_bound_proof() {
    // CONDITION_NOT_APPLICABLE with the snapshot-bound false proof →
    // COMMITTED; a wrong digest fails closed.
    let (database, spec, permit) = setup(vec![planned_conditional(0, CNA_CONDITION)]);
    let authority = database.open();
    authority
        .record_no_effect(no_effect_request(
            &spec,
            &permit,
            0,
            NoEffectReason::ConditionNotApplicable,
        ))
        .expect("condition-not-applicable closure");
    let expected_proof = expected_cna_proof(spec.snapshot.snapshot_digest);
    assert!(
        matches!(
            authority.finalize_commit_v3(finalize_v3(
                &spec,
                permit.permit_id,
                vec![cna_proof([0x89; 32])],
            )),
            Err(TaskStoreError::RequiredEffectUnsatisfied { effect_seq: 0, .. })
        ),
        "a digest not bound to the snapshot fails closed"
    );
    let receipt = match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![cna_proof(expected_proof)],
        ))
        .expect("cna finalize")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(receipt.outcome, nlos_task::ReceiptOutcome::Committed);
    assert!(
        authority
            .list_effect_history(task_id())
            .expect("history")
            .is_empty(),
        "a CNA-satisfied commit appends no history entries"
    );
    drop(authority);
}

/// Bullet: plain `NO_EFFECT` reasons and proofs for non-required/unknown
/// slots never satisfy a required slot — the skip is never written as
/// `COMMITTED` and the permit stays open (`[TASK-COMMIT-002]`).
#[test]
fn plain_no_effect_and_foreign_proofs_never_satisfy_required() {
    // A CNA proof on a plain NO_EFFECT reason never satisfies; the skip
    // is not written as COMMITTED and the permit stays open.
    let (database, spec, permit) = setup(vec![planned_conditional(0, CNA_CONDITION)]);
    let authority = database.open();
    authority
        .record_no_effect(no_effect_request(
            &spec,
            &permit,
            0,
            NoEffectReason::NotSelected,
        ))
        .expect("plain no-effect");
    let expected_proof = expected_cna_proof(spec.snapshot.snapshot_digest);
    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![cna_proof(expected_proof)],
        )),
        Err(TaskStoreError::RequiredEffectUnsatisfied { effect_seq: 0, .. })
    ));
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    // A proof for a non-required/unknown slot is likewise fail-closed.
    let (database, spec, permit) = setup(vec![planned(0, false)]);
    let authority = database.open();
    close_slot(&authority, &spec, &permit, 0);
    assert!(
        matches!(
            authority.finalize_commit_v3(finalize_v3(
                &spec,
                permit.permit_id,
                vec![success_proof(&authority, permit.permit_id, 0)],
            )),
            Err(TaskStoreError::RequiredEffectUnsatisfied { .. })
        ),
        "proof for a non-required slot fails closed"
    );
}

/// Frozen golden copy of the B-TASK-002 v2 schema: v1 DDL plus the v2
/// effect plane, intentionally duplicated so the fixture cannot drift
/// with the live source.
const GOLDEN_V2_DDL: &str = concat!(
    include_str!("golden_v1_ddl.sql"),
    "\n",
    include_str!("golden_v2_ddl.sql")
);

fn blob8(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// Seeds a complete v2 database: one task, one snapshot, one
/// `CommitPermitted` attempt, one outstanding permit (ID `0x77..`) with a
/// one-slot declared effect set in `Planned` state.
fn seed_golden_v2_core(database: &TestDatabase) {
    let connection = rusqlite::Connection::open(&database.path).expect("create raw v2 database");
    connection
        .execute_batch(GOLDEN_V2_DDL)
        .expect("create golden v2 schema");
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

fn seed_golden_v2_effects(database: &TestDatabase) {
    let connection = rusqlite::Connection::open(&database.path).expect("raw v2 connection");
    connection
        .execute(
            "INSERT INTO effect_slots (
                permit_id, effect_seq, task_id, effect_slot_id, logical_effect_id,
                idempotency_identity_digest, required, required_condition_digest,
                success_criteria_digest, action_proposal_digest, slot_state,
                state_seq, effect_permit_id, effect_receipt_id,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, NULL, ?7, ?8, 0, 0, NULL, NULL, 3000, 3000)",
            rusqlite::params![
                bytes(0x77).as_slice(),
                blob8(0),
                bytes(0x01).as_slice(),
                bytes(0x78).as_slice(),
                descriptor(0).logical_effect_id().as_slice(),
                descriptor(0).idempotency_identity_digest().as_slice(),
                [0x66u8; 32].as_slice(),
                [0x77u8; 32].as_slice(),
            ],
        )
        .expect("seed slot");
    connection
        .execute(
            "INSERT INTO permit_effect_sets (
                permit_id, task_id, effect_set_root, effect_slot_state_root,
                required_effect_count, satisfied_required_effect_count,
                terminal_effect_count, issued_effect_root, dispatched_effect_root,
                closed_effect_root, outstanding_effect_root, revision,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, 1, 0, 0, ?5, ?5, ?5, ?5, 0, 3000, 3000)",
            rusqlite::params![
                bytes(0x77).as_slice(),
                bytes(0x01).as_slice(),
                [0x02u8; 32].as_slice(),
                [0x03u8; 32].as_slice(),
                [0x04u8; 32].as_slice(),
            ],
        )
        .expect("seed effect set");
}
fn seed_golden_v2(database: &TestDatabase) {
    seed_golden_v2_core(database);
    seed_golden_v2_effects(database);
}

/// Bullet: schema migration v2 → v3 is lossless — every v2 row survives,
/// the effect history starts empty (root = fixed initial formula), old
/// flows keep their B-TASK-002 behavior, and the new v3 flows are usable
/// on the migrated database.
#[test]
fn golden_v2_database_migrates_losslessly_to_v3() {
    let database = TestDatabase::new("golden-v2");
    seed_golden_v2(&database);
    let seeded_permit_id = CommitPermitId::from_bytes(bytes(0x77));

    let authority = database.open();
    {
        let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 39, "migration stamps the current schema version");
    }

    // All v2 data intact.
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(head.active_permit, Some(seeded_permit_id));
    let slot = authority
        .inspect_effect_slot(seeded_permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Planned);
    let summary = authority
        .inspect_effect_set(seeded_permit_id)
        .expect("summary")
        .expect("declared set");
    assert_eq!(summary.required_effect_count, 1);

    // The v3 plane starts empty and bit-compatible with the initial head.
    assert!(
        authority
            .list_effect_history(task_id())
            .expect("history")
            .is_empty()
    );
    assert_eq!(
        authority
            .compute_effect_history_root(task_id())
            .expect("root"),
        empty_effect_history_root()
    );
    assert!(
        authority
            .lookup_effect_history(task_id(), descriptor(0).logical_effect_id())
            .expect("lookup")
            .is_none()
    );

    // The seeded v2 permit remains queryable but is explicitly unbound.
    // Schema v12 must not invent participant evidence, and no new effect
    // or terminal authority decision may proceed from that legacy row.
    let spec_a = attempt_spec(0x0a, initial_snapshot());
    let permit = authority
        .inspect_permit(task_id(), seeded_permit_id)
        .expect("permit");
    assert_eq!(permit.participant_registry_binding, None);
    assert!(matches!(
        authority.finalize_commit(nlos_task::FinalizeRequest {
            task_id: spec_a.task_id,
            attempt_id: spec_a.attempt_id,
            attempt_generation: spec_a.attempt_generation,
            permit_id: seeded_permit_id,
            new_effect_history_root: [0x31; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms: 7_000,
        }),
        Err(TaskStoreError::ParticipantRegistryBindingMissing)
    ));
    assert!(authority.list_effect_history(task_id()).unwrap().is_empty());

    // The v2 immutability triggers still enforce, and the v3 ones do too.
    let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
    let v2_trigger: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='trigger' AND name='effect_receipt_is_immutable'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        v2_trigger, 1,
        "v2 effect receipt trigger survives migration"
    );
    let v3_trigger: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='trigger' AND name='effect_history_is_immutable'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(v3_trigger, 1, "v3 history trigger survives migration");
}

/// Bullet: v2 → v3 migration is transactional and fail-closed — a v2
/// database on which the v3 step cannot complete stays a complete v2
/// database, never a half-migrated one.
#[test]
fn failed_v3_migration_rolls_back_to_complete_v2() {
    let database = TestDatabase::new("migration-v3-failure");
    seed_golden_v2(&database);
    {
        let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
        connection
            .execute_batch("CREATE TABLE effect_history (bogus INTEGER) STRICT;")
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
    assert_eq!(version, 2, "rolled-back migration keeps the v2 stamp");
    let permits: i64 = connection
        .query_row("SELECT COUNT(*) FROM commit_permits", [], |row| row.get(0))
        .expect("v2 permit data intact");
    assert_eq!(permits, 1);
    let slots: i64 = connection
        .query_row("SELECT COUNT(*) FROM effect_slots", [], |row| row.get(0))
        .expect("v2 slot data intact");
    assert_eq!(slots, 1);
    let v3_tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN (
                'task_quarantine_receipts', 'task_adoption_receipts',
                'task_reconcile_receipts', 'task_effect_sequences',
                'task_finalize_proofs')",
            [],
            |row| row.get(0),
        )
        .expect("inspect schema");
    assert_eq!(v3_tables, 0, "no partial v3 table may survive the rollback");
}
