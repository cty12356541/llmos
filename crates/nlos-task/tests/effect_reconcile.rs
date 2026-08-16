//! Acceptance tests for the B-TASK-003 quarantine/adoption/reconcile
//! lifecycle (`[TASK-EFFECT-003]` / `[TASK-COMMIT-003]`, single-authority
//! subset) on top of the B-TASK-001/002 `TaskAuthority`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AdoptionReplay, AdoptionRequest, AttemptSpec, AuthorityLeaseAdoptionRequest,
    AuthorityLeaseFinalizeRequest, AuthorityLeasePermitRequest, AuthorityLeaseReconcileRequest,
    AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRequest, ClosePermitDecision,
    ClosePermitRequest, EffectPermitDecision, EffectPermitRequest, FinalizeDecision,
    FinalizeRequestV3, IssuedPermit, LogicalEffectDescriptor, NoEffectReason, NoEffectRequest,
    Outcome, OutcomeRequest, PermitClosureOutcome, PermitDecision, PermitRecord, PermitRequest,
    PermitState, PlannedEffect, ReconcileOutcome, ReconcileReplay, ReconcileRequest,
    RequiredSatisfaction, RequiredSatisfactionProof, SlotState, SnapshotBundle,
    SqliteTaskAuthority, TaskSpec, TaskStoreError, empty_effect_history_root,
    expected_success_assertion_digest,
};
use nlos_types::{
    CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, ProcessId, ReceiptId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-reconcile-{name}-{}-{sequence}.sqlite3",
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
    fence_digest: [u8; 32],
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
        fenced_participant_digest: fence_digest,
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

fn close_request(
    spec: &AttemptSpec,
    permit_id: CommitPermitId,
    outcome: PermitClosureOutcome,
) -> ClosePermitRequest {
    ClosePermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id,
        outcome,
        fenced_participant_digest: [0xf1; 32],
        closed_at_ms: 7_000,
    }
}

fn adopt_request(spec: &AttemptSpec, permit: &PermitRecord, key_seed: u8) -> AdoptionRequest {
    AdoptionRequest {
        task_id: spec.task_id,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        adopted_at_ms: 8_000,
    }
}

fn reconcile_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    adoption_receipt_id: ReceiptId,
    outcome: ReconcileOutcome,
    proof: [u8; 32],
) -> ReconcileRequest {
    ReconcileRequest {
        task_id: spec.task_id,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        adoption_receipt_id,
        outcome,
        closure_proof_digest: proof,
        reconciled_at_ms: 9_000,
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

/// Registers a task plus one attempt, issues its `CommitPermit` with the
/// given effect set, and dispatches slot 0 into `EffectUnknown`.
fn setup_unknown(effects: Vec<PlannedEffect>) -> (TestDatabase, AttemptSpec, PermitRecord) {
    let (database, spec, permit, _) = setup_unknown_inner(effects, None);
    (database, spec, permit)
}

fn setup_unknown_bound(
    effects: Vec<PlannedEffect>,
) -> (
    TestDatabase,
    AttemptSpec,
    PermitRecord,
    nlos_task::AuthorityLeaseRecord,
) {
    let (database, spec, permit, lease) = setup_unknown_inner(
        effects,
        Some(AuthorityLeaseRequest {
            holder_id: ProcessId::from_bytes([0x81; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
            requested_at_ms: 7_000,
            ttl_ms: 10_000,
        }),
    );
    (
        database,
        spec,
        permit,
        lease.expect("bound setup returns lease"),
    )
}

fn setup_unknown_inner(
    effects: Vec<PlannedEffect>,
    lease_request: Option<AuthorityLeaseRequest>,
) -> (
    TestDatabase,
    AttemptSpec,
    PermitRecord,
    Option<nlos_task::AuthorityLeaseRecord>,
) {
    let database = TestDatabase::new("unknown");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    authority.register_attempt(spec).expect("register attempt");
    let authority_lease = lease_request.map(|request| {
        authority
            .acquire_authority_lease(request)
            .expect("authority lease")
            .record()
    });
    let permit_request = permit_request(&spec, 0x01, effects);
    let permit = issued_permit(match authority_lease {
        Some(lease) => authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request,
                lease,
            })
            .expect("lease-bound permit"),
        None => authority
            .request_commit_permit(permit_request)
            .expect("permit"),
    });
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("issue slot 0"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect("dispatch slot 0");
    authority
        .record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Unknown {
                uncertainty_digest: [0x99; 32],
            },
        ))
        .expect("register uncertainty");
    drop(authority);
    (database, spec, permit, authority_lease)
}

/// Bullet: any `EFFECT_UNKNOWN` slot at finalize time turns the active
/// permit into a non-reusable `QUARANTINED` tombstone — durable receipt,
/// frozen `TaskHead`, no new winner — and replaying the original
/// PermitId/finalize returns the same lifecycle state
/// (`[TASK-EFFECT-003]` / `[TASK-COMMIT-003]`).
#[test]
fn unknown_at_finalize_quarantines_permit_and_freezes_head() {
    let (database, spec, permit) = setup_unknown(vec![planned(0, true), planned(1, false)]);
    let authority = database.open();
    let head_before = authority.inspect_task(task_id()).expect("head");

    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32])),
        Err(TaskStoreError::Quarantined)
    ));
    let quarantine = authority
        .inspect_quarantine_receipt(permit.permit_id)
        .expect("quarantine receipt")
        .expect("tombstone persisted");
    assert_eq!(quarantine.permit_id, permit.permit_id);
    assert_eq!(quarantine.permit_epoch, permit.permit_epoch);
    assert_eq!(quarantine.unknown_slots, vec![0]);
    assert_eq!(quarantine.fenced_participant_digest, [0xf1; 32]);
    assert!(
        !quarantine.known_effect_receipts.is_empty(),
        "the uncertainty receipt is a known receipt of the tombstone"
    );
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Quarantined,
        "the active permit is the non-reusable tombstone"
    );
    let head_after = authority.inspect_task(task_id()).expect("head after");
    assert_eq!(head_after.head_commit_seq, head_before.head_commit_seq);
    assert_eq!(
        head_after.head_effect_history_root, head_before.head_effect_history_root,
        "TaskHead must not advance while quarantined"
    );
    assert_eq!(head_after.retry_fence_epoch, head_before.retry_fence_epoch);
    assert_eq!(
        head_after.active_permit,
        Some(permit.permit_id),
        "the tombstone is still the outstanding permit"
    );

    // Replay of the same finalize observes the same lifecycle state; a
    // different participant-fence digest fails closed.
    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32])),
        Err(TaskStoreError::Quarantined)
    ));
    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf2; 32])),
        Err(TaskStoreError::HistoryConflict)
    ));
    // Replaying the original PermitId/idempotency key returns the same
    // lifecycle state.
    match authority
        .request_commit_permit(permit_request(
            &spec,
            0x01,
            vec![planned(0, true), planned(1, false)],
        ))
        .expect("permit replay")
    {
        PermitDecision::Replayed(original) => {
            assert_eq!(original.state, PermitState::Quarantined);
        }
        other => panic!("expected Replayed, got {other:?}"),
    }

    // No new winner may be issued while the tombstone stands.
    let spec_b = attempt_spec(0x0b, snapshot(0, 0));
    authority.register_attempt(spec_b).expect("register B");
    match authority
        .request_commit_permit(permit_request(&spec_b, 0x02, vec![planned(2, true)]))
        .expect("B is fenced by the tombstone")
    {
        PermitDecision::Quarantined {
            quarantine_receipt_id,
        } => {
            assert_eq!(quarantine_receipt_id, quarantine.receipt_id);
        }
        other => panic!("expected Quarantined, got {other:?}"),
    }
    assert_eq!(
        authority
            .inspect_attempt(task_id(), spec_b.attempt_id)
            .expect("attempt B")
            .state,
        nlos_task::AttemptState::Superseded
    );
}

/// Bullet: the `close_permit` path hits the same tombstone — quarantine
/// first, replay returns the original tombstone.
#[test]
fn close_permit_quarantines_on_unknown_and_replays_tombstone() {
    let (database, spec, permit) = setup_unknown(vec![planned(0, true)]);
    let authority = database.open();

    match authority
        .close_permit(close_request(
            &spec,
            permit.permit_id,
            PermitClosureOutcome::CancelledBeforeEffect,
        ))
        .expect("close decision")
    {
        ClosePermitDecision::Quarantined(record) => {
            assert_eq!(record.unknown_slots, vec![0]);
        }
        other => panic!("expected Quarantined, got {other:?}"),
    }
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        0,
        "TaskHead must not advance"
    );
    match authority
        .close_permit(close_request(
            &spec,
            permit.permit_id,
            PermitClosureOutcome::CancelledBeforeEffect,
        ))
        .expect("close replay")
    {
        ClosePermitDecision::ReplayedQuarantine(record) => {
            assert_eq!(record.permit_id, permit.permit_id);
        }
        other => panic!("expected ReplayedQuarantine, got {other:?}"),
    }
}

/// Bullet: the adoption scope is `RECONCILE_CLOSE_OR_QUARANTINE_ONLY` —
/// adoption requires a quarantined permit, replays by idempotency key,
/// and once adopted the permit refuses new `EffectPermits` and dispatches
/// (`[TASK-COMMIT-003]`).
#[test]
fn adoption_scope_forbids_new_permits_dispatches_and_effects() {
    let (database, spec, permit) = setup_unknown(vec![planned(0, true), planned(1, false)]);
    let authority = database.open();

    // Adoption requires the tombstone.
    assert!(matches!(
        authority.adopt_permit(adopt_request(&spec, &permit, 0xd1)),
        Err(TaskStoreError::InvalidReconcileState { .. })
    ));
    authority
        .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32]))
        .expect_err("quarantine");
    let mut wrong_epoch = adopt_request(&spec, &permit, 0xd2);
    wrong_epoch.permit_epoch = 99;
    assert!(matches!(
        authority.adopt_permit(wrong_epoch),
        Err(TaskStoreError::PermitEpochMismatch)
    ));
    let adoption = match authority
        .adopt_permit(adopt_request(&spec, &permit, 0xd1))
        .expect("adopt")
    {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    };
    assert_eq!(adoption.original_permit_id, permit.permit_id);
    assert_eq!(adoption.original_permit_epoch, permit.permit_epoch);
    assert_eq!(adoption.adoption_epoch, 1);
    // Same key + same bytes replays; same key + different bytes fails.
    assert!(matches!(
        authority.adopt_permit(adopt_request(&spec, &permit, 0xd1)),
        Ok(AdoptionReplay::Replayed(_))
    ));
    let mut conflicting = adopt_request(&spec, &permit, 0xd1);
    conflicting.permit_epoch = permit.permit_epoch + 7;
    assert!(matches!(
        authority.adopt_permit(conflicting),
        Err(TaskStoreError::IdempotencyConflict)
    ));

    // Reconcile the unknown slot away; the permit returns to Issued but
    // the adoption scope still forbids new EffectPermits and dispatches.
    authority
        .reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption.receipt_id,
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        ))
        .expect("reconcile to closed");
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert!(matches!(
        authority.request_effect_permit(effect_request(&spec, &permit, 1, 0xe2)),
        Err(TaskStoreError::AdoptionScopeViolation)
    ));
    let slot1 = authority
        .inspect_effect_slot(permit.permit_id, 1)
        .expect("slot 1");
    assert_eq!(slot1.state, SlotState::Planned, "no dispatch happened");
}

/// Bullet: reconcile to `EFFECT_CLOSED` writes the reconciliation receipt,
/// appends the history entry, lifts the tombstone, and unblocks a
/// `COMMITTED` finalize (required rule now satisfiable); reconcile replays
/// are byte-exact and fail closed on a different proof
/// (`[TASK-EFFECT-003]` / `[TASK-COMMIT-002]`).
#[test]
fn reconcile_to_effect_closed_unblocks_committed_finalize() {
    let (database, spec, permit) = setup_unknown(vec![planned(0, true)]);
    let authority = database.open();
    authority
        .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32]))
        .expect_err("quarantine");
    let adoption = match authority
        .adopt_permit(adopt_request(&spec, &permit, 0xd1))
        .expect("adopt")
    {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    };

    // Wrong adoption binding and unknown adoption are refused.
    assert!(matches!(
        authority.reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            ReceiptId::from_bytes(bytes(0xfe)),
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        )),
        Err(TaskStoreError::ReceiptNotFound)
    ));

    let record = match authority
        .reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption.receipt_id,
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        ))
        .expect("reconcile")
    {
        ReconcileReplay::Reconciled(record) => *record,
        other @ ReconcileReplay::Replayed(_) => panic!("expected Reconciled, got {other:?}"),
    };
    assert_eq!(record.outcome, ReconcileOutcome::EffectClosed);
    assert_eq!(record.permit_adoption_receipt_id, adoption.receipt_id);
    assert!(record.effect_receipt_id.is_some());
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::EffectClosed);
    assert_eq!(slot.effect_receipt_id, record.effect_receipt_id);
    let history = authority.list_effect_history(task_id()).expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].outcome,
        nlos_task::EffectHistoryOutcome::EffectClosed
    );
    assert_eq!(
        history[0].authoritative_effect_receipt_id,
        record.effect_receipt_id.expect("closure receipt")
    );
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued,
        "the last unknown slot resolved: the tombstone lifts"
    );
}

/// Bullet: reconcile replay is byte-exact — same slot, adoption, outcome,
/// and proof return the original receipt (no double reconcile); a
/// different proof fails closed (`[TASK-EFFECT-003]`).
#[test]
fn reconcile_replay_is_byte_exact_and_proof_conflicts_fail_closed() {
    let (database, spec, permit) = setup_unknown(vec![planned(0, true)]);
    let authority = database.open();
    authority
        .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32]))
        .expect_err("quarantine");
    let adoption = match authority
        .adopt_permit(adopt_request(&spec, &permit, 0xd1))
        .expect("adopt")
    {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    };
    authority
        .reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption.receipt_id,
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        ))
        .expect("reconcile");
    // Reconcile replay: same bytes return the original receipt, different
    // proof fails closed — no double reconcile.
    assert!(matches!(
        authority.reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption.receipt_id,
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        )),
        Ok(ReconcileReplay::Replayed(_))
    ));
    assert!(matches!(
        authority.reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption.receipt_id,
            ReconcileOutcome::EffectClosed,
            [0xab; 32],
        )),
        Err(TaskStoreError::HistoryConflict)
    ));

    // Finalize now commits — but only with the required success proof.
    match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
            [0xf1; 32],
        ))
        .expect("finalize commits")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.outcome, nlos_task::ReceiptOutcome::Committed);
            assert_eq!(receipt.new_head_commit_seq, 1);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_effect_history_root,
        authority
            .compute_effect_history_root(task_id())
            .expect("root"),
        "the committed head root equals the recomputed history root"
    );
}

/// Bullet: `CONFIRMED_NO_EFFECT` on a required slot can never satisfy
/// `COMMITTED` — with no other effect the permit stays open (typed
/// `RequiredEffectUnsatisfied`) and only a pre-effect closure may close
/// it, counting `CONFIRMED_NO_EFFECT` as a valid absence proof
/// (`[TASK-COMMIT-002]` / `[TASK-EFFECT-003]`).
#[test]
fn confirmed_no_effect_on_required_slot_never_commits() {
    let (database, spec, permit) = setup_unknown(vec![planned(0, true)]);
    let authority = database.open();
    authority
        .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32]))
        .expect_err("quarantine");
    let adoption = match authority
        .adopt_permit(adopt_request(&spec, &permit, 0xd1))
        .expect("adopt")
    {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    };
    authority
        .reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption.receipt_id,
            ReconcileOutcome::ConfirmedNoEffect,
            [0xcc; 32],
        ))
        .expect("reconcile to confirmed-no-effect");
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::ConfirmedNoEffect);

    // No proof shape can satisfy the required slot, and with zero effects
    // happened the permit stays open.
    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32])),
        Err(TaskStoreError::RequiredEffectUnsatisfied { effect_seq: 0, .. })
    ));
    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
            [0xf1; 32],
        )),
        Err(TaskStoreError::RequiredEffectUnsatisfied { effect_seq: 0, .. })
    ));
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued,
        "permit stays open"
    );
    // The pre-effect closure path accepts CONFIRMED_NO_EFFECT as a valid
    // absence proof and keeps the TaskHead unchanged.
    match authority
        .close_permit(close_request(
            &spec,
            permit.permit_id,
            PermitClosureOutcome::FailedBeforeEffect,
        ))
        .expect("closure")
    {
        ClosePermitDecision::Closed(receipt) => {
            assert_eq!(
                receipt.outcome,
                nlos_task::ReceiptOutcome::FailedBeforeEffect
            );
            assert_eq!(receipt.new_head_commit_seq, receipt.prior_head_commit_seq);
        }
        other => panic!("expected Closed, got {other:?}"),
    }
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        0
    );
}

/// Bullet: a still-unknown reconcile returns the slot to `EFFECT_UNKNOWN`
/// and keeps the permit `QUARANTINED`; a later adoption + reconcile can
/// still resolve it (`[TASK-EFFECT-003]`).
#[test]
fn still_unknown_reconcile_returns_to_quarantine() {
    let (database, spec, permit) = setup_unknown(vec![planned(0, true)]);
    let authority = database.open();
    authority
        .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32]))
        .expect_err("quarantine");
    let adoption = match authority
        .adopt_permit(adopt_request(&spec, &permit, 0xd1))
        .expect("adopt")
    {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    };
    let record = match authority
        .reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption.receipt_id,
            ReconcileOutcome::EffectUnknown,
            [0xdd; 32],
        ))
        .expect("still unknown")
    {
        ReconcileReplay::Reconciled(record) => *record,
        other @ ReconcileReplay::Replayed(_) => panic!("expected Reconciled, got {other:?}"),
    };
    assert_eq!(record.outcome, ReconcileOutcome::EffectUnknown);
    assert_eq!(record.effect_receipt_id, None);
    assert_eq!(
        authority
            .inspect_effect_slot(permit.permit_id, 0)
            .expect("slot")
            .state,
        SlotState::EffectUnknown
    );
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Quarantined,
        "permit stays QUARANTINED"
    );
    assert!(
        authority
            .list_effect_history(task_id())
            .expect("history")
            .is_empty(),
        "an unknown reconcile writes no history entry"
    );

    // A fresh adoption (new uncertainty round) reconciles to closure.
    let adoption2 = match authority
        .adopt_permit(adopt_request(&spec, &permit, 0xd2))
        .expect("second adoption")
    {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    };
    assert_eq!(adoption2.adoption_epoch, 2);
    authority
        .reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption2.receipt_id,
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        ))
        .expect("resolve");
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
}

#[test]
fn lease_bound_adoption_and_reconcile_require_the_live_binding() {
    let (database, spec, permit, lease) = setup_unknown_bound(vec![planned(0, true)]);
    let authority = database.open();
    let finalize = finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32]);
    assert!(matches!(
        authority.finalize_commit_v3(finalize.clone()),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    assert!(matches!(
        authority.finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize,
            lease,
        }),
        Err(TaskStoreError::Quarantined)
    ));

    let adoption_request = adopt_request(&spec, &permit, 0xe1);
    assert!(matches!(
        authority.adopt_permit(adoption_request),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    let adoption = match authority
        .adopt_permit_with_authority_lease(AuthorityLeaseAdoptionRequest {
            adoption: adoption_request,
            lease,
        })
        .expect("lease-bound adoption")
    {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    };
    assert_eq!(adoption.authority_lease_binding, Some(lease.binding()));
    assert_eq!(
        authority
            .inspect_adoption_receipt(spec.task_id, adoption.receipt_id)
            .expect("adoption readback")
            .authority_lease_binding,
        Some(lease.binding())
    );
    let raw = Connection::open(&database.path).expect("raw task database");
    assert!(
        raw.execute(
            "UPDATE task_adoption_receipts SET authority_lease_term = ?1
             WHERE receipt_id = ?2",
            rusqlite::params![
                [0u8; 8].as_slice(),
                adoption.receipt_id.as_bytes().as_slice()
            ],
        )
        .is_err()
    );
    drop(raw);

    let reconcile = reconcile_request(
        &spec,
        &permit,
        0,
        adoption.receipt_id,
        ReconcileOutcome::EffectClosed,
        [0xab; 32],
    );
    assert!(matches!(
        authority.reconcile_effect(reconcile),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    let reconciled = authority
        .reconcile_effect_with_authority_lease(AuthorityLeaseReconcileRequest { reconcile, lease })
        .expect("lease-bound reconcile");
    assert!(matches!(reconciled, ReconcileReplay::Reconciled(_)));
    assert_eq!(
        authority
            .inspect_permit(spec.task_id, permit.permit_id)
            .expect("permit after reconcile")
            .state,
        PermitState::Issued
    );

    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_adoption_receipt(spec.task_id, adoption.receipt_id)
            .expect("reopened adoption")
            .authority_lease_binding,
        Some(lease.binding())
    );
    assert!(matches!(
        reopened.reconcile_effect(reconcile),
        Ok(ReconcileReplay::Replayed(_))
    ));
}

#[test]
fn takeover_fence_blocks_fresh_adoption_but_keeps_exact_replay_readable() {
    let (database, spec, permit, lease_one) = setup_unknown_bound(vec![planned(0, true)]);
    let authority = database.open();
    let finalize = finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32]);
    assert!(matches!(
        authority.finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize,
            lease: lease_one,
        }),
        Err(TaskStoreError::Quarantined)
    ));
    let original_adoption = adopt_request(&spec, &permit, 0xe2);
    let original_adoption_record = match authority
        .adopt_permit_with_authority_lease(AuthorityLeaseAdoptionRequest {
            adoption: original_adoption,
            lease: lease_one,
        })
        .expect("initial adoption")
    {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    };
    let lease_two = authority
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: ProcessId::from_bytes([0x83; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x84; 16]),
            requested_at_ms: 17_001,
            ttl_ms: 10_000,
        })
        .expect("takeover lease")
        .record();
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");
    authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: spec.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 17_002,
        })
        .expect("takeover fence");

    let adoption_request = adopt_request(&spec, &permit, 0xe3);
    assert!(matches!(
        authority.adopt_permit(adoption_request),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: nlos_task::ParticipantRegistryState::FrozenForTakeover
        })
    ));
    assert!(matches!(
        authority.adopt_permit_with_authority_lease(AuthorityLeaseAdoptionRequest {
            adoption: adoption_request,
            lease: lease_two,
        }),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: nlos_task::ParticipantRegistryState::FrozenForTakeover
        })
    ));
    let reconcile = reconcile_request(
        &spec,
        &permit,
        0,
        original_adoption_record.receipt_id,
        ReconcileOutcome::EffectClosed,
        [0xac; 32],
    );
    assert!(matches!(
        authority.reconcile_effect(reconcile),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: nlos_task::ParticipantRegistryState::FrozenForTakeover
        })
    ));
    assert!(matches!(
        authority.reconcile_effect_with_authority_lease(AuthorityLeaseReconcileRequest {
            reconcile,
            lease: lease_two,
        }),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: nlos_task::ParticipantRegistryState::FrozenForTakeover
        })
    ));
    assert!(matches!(
        authority.adopt_permit(original_adoption),
        Ok(AdoptionReplay::Replayed(_))
    ));
}

/// Bullet: replay consistency across restart — quarantine, adoption, and
/// reconcile state survive reopening and every replay returns the
/// original lifecycle (`[TASK-COMMIT-003]`).
#[test]
fn quarantine_adoption_reconcile_replay_consistent_across_restart() {
    let (database, spec, permit) = setup_unknown(vec![planned(0, true)]);
    {
        let authority = database.open();
        authority
            .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32]))
            .expect_err("quarantine");
        match authority
            .adopt_permit(adopt_request(&spec, &permit, 0xd1))
            .expect("adopt")
        {
            AdoptionReplay::Adopted(_) => {}
            other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
        }
    }

    let authority = database.open();
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Quarantined,
        "tombstone survives restart"
    );
    assert!(
        matches!(
            authority.finalize_commit_v3(finalize_v3(
                &spec,
                permit.permit_id,
                Vec::new(),
                [0xf1; 32],
            )),
            Err(TaskStoreError::Quarantined)
        ),
        "finalize replay across restart observes the tombstone"
    );
    assert!(
        matches!(
            authority.adopt_permit(adopt_request(&spec, &permit, 0xd1)),
            Ok(AdoptionReplay::Replayed(_))
        ),
        "adoption replay across restart"
    );
}

/// Bullet: reconcile and finalize replays are consistent across restart —
/// the persisted reconcile receipt is byte-identical, the finalize
/// commits once, and its replays return the original lifecycle
/// (`[TASK-COMMIT-003]`).
#[test]
fn reconcile_and_finalize_replay_consistent_across_restart() {
    let (database, spec, permit) = setup_unknown(vec![planned(0, true)]);
    let adoption;
    {
        let authority = database.open();
        authority
            .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new(), [0xf1; 32]))
            .expect_err("quarantine");
        adoption = match authority
            .adopt_permit(adopt_request(&spec, &permit, 0xd1))
            .expect("adopt")
        {
            AdoptionReplay::Adopted(record) => *record,
            other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
        };
    }

    let authority = database.open();
    let record = match authority
        .reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption.receipt_id,
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        ))
        .expect("reconcile")
    {
        ReconcileReplay::Reconciled(record) => *record,
        other @ ReconcileReplay::Replayed(_) => panic!("expected Reconciled, got {other:?}"),
    };
    drop(authority);

    let authority = database.open();
    assert!(
        matches!(
            authority.reconcile_effect(reconcile_request(
                &spec,
                &permit,
                0,
                adoption.receipt_id,
                ReconcileOutcome::EffectClosed,
                [0xaa; 32],
            )),
            Ok(ReconcileReplay::Replayed(_))
        ),
        "reconcile replay across restart"
    );
    let stored = authority
        .inspect_reconcile_receipt(permit.permit_id, 0)
        .expect("reconcile receipt")
        .expect("persisted");
    assert_eq!(stored, record);
    match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
            [0xf1; 32],
        ))
        .expect("finalize")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.outcome, nlos_task::ReceiptOutcome::Committed);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    assert!(
        matches!(
            authority.finalize_commit_v3(finalize_v3(
                &spec,
                permit.permit_id,
                vec![success_proof(&authority, permit.permit_id, 0)],
                [0xf1; 32],
            )),
            Ok(FinalizeDecision::Replayed(_))
        ),
        "finalize replay across restart"
    );
    assert!(
        matches!(
            authority.finalize_commit_v3(finalize_v3(
                &spec,
                permit.permit_id,
                Vec::new(),
                [0xf1; 32],
            )),
            Err(TaskStoreError::HistoryConflict)
        ),
        "different proof bytes fail closed"
    );
}

/// Bullet: pre-effect closure still requires every effect provably
/// absent — a permit with an `EFFECT_CLOSED` slot is refused; `NO_EFFECT`
/// slots close with the `TaskHead` unchanged (`[TASK-RETRY-EFFECT-001]` /
/// `[TASK-CANCEL-003]`).
#[test]
fn close_permit_requires_all_effects_provably_absent() {
    let database = TestDatabase::new("closure");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    authority.register_attempt(spec).expect("register attempt");
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(
                &spec,
                0x01,
                vec![planned(0, true), planned(1, false)],
            ))
            .expect("permit"),
    );
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("issue slot 0"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect("dispatch slot 0");
    authority
        .record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect("close slot 0");
    authority
        .record_no_effect(no_effect_request(
            &spec,
            &permit,
            1,
            NoEffectReason::NotSelected,
        ))
        .expect("no-effect slot 1");

    assert!(
        matches!(
            authority.close_permit(close_request(
                &spec,
                permit.permit_id,
                PermitClosureOutcome::FailedBeforeEffect,
            )),
            Err(TaskStoreError::PermitHasEffects { count: 1 })
        ),
        "an EFFECT_CLOSED slot forbids the pre-effect closure"
    );
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued,
        "refusal changes nothing"
    );
}

/// Bullet: a pure no-effect permit closes as `CANCELLED_BEFORE_EFFECT` with
/// the `TaskHead` unchanged, replays byte-exactly, and releases the
/// winner CAS gate (`[TASK-RETRY-EFFECT-001]` / `[TASK-CANCEL-003]`).
#[test]
fn close_permit_closes_pure_no_effect_permit_with_head_unchanged() {
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    // A pure no-effect permit closes with the head unchanged and frees
    // the CAS gate for the next competition.
    let database2 = TestDatabase::new("closure-pure");
    let authority2 = database2.open();
    authority2
        .register_task(task_spec())
        .expect("register task");
    authority2.register_attempt(spec).expect("register attempt");
    let permit2 = issued_permit(
        authority2
            .request_commit_permit(permit_request(&spec, 0x01, vec![planned(0, true)]))
            .expect("permit 2"),
    );
    authority2
        .record_no_effect(no_effect_request(
            &spec,
            &permit2,
            0,
            NoEffectReason::NotSelected,
        ))
        .expect("no-effect");
    let head_before = authority2.inspect_task(task_id()).expect("head");
    match authority2
        .close_permit(close_request(
            &spec,
            permit2.permit_id,
            PermitClosureOutcome::CancelledBeforeEffect,
        ))
        .expect("closure")
    {
        ClosePermitDecision::Closed(receipt) => {
            assert_eq!(
                receipt.outcome,
                nlos_task::ReceiptOutcome::CancelledBeforeEffect
            );
            assert_eq!(receipt.new_head_commit_seq, head_before.head_commit_seq);
        }
        other => panic!("expected Closed, got {other:?}"),
    }
    assert!(matches!(
        authority2.close_permit(close_request(
            &spec,
            permit2.permit_id,
            PermitClosureOutcome::CancelledBeforeEffect,
        )),
        Ok(ClosePermitDecision::Replayed(_))
    ));
    assert!(
        matches!(
            authority2.close_permit(close_request(
                &spec,
                permit2.permit_id,
                PermitClosureOutcome::FailedBeforeEffect,
            )),
            Err(TaskStoreError::HistoryConflict)
        ),
        "same permit, different closure outcome fails closed"
    );
    let spec_b = attempt_spec(0x0b, snapshot(0, 0));
    authority2.register_attempt(spec_b).expect("register B");
    assert!(
        matches!(
            authority2.request_commit_permit(permit_request(&spec_b, 0x02, Vec::new())),
            Ok(PermitDecision::Issued(_))
        ),
        "closure releases the CAS gate"
    );
}
