//! B-SLICE-K-001 §5: multi-`TaskAttempt` competition on one Task — the
//! constructive proof of Issue 31 §6 items 2-3 ("two competing Attempts on
//! the same `TaskHead`; exactly one Attempt obtains the `CommitPermit`") and
//! the competition lane of ROAD-B-003.
//!
//! Four tests, one per linearization scenario:
//!
//! 1. `competing_attempts_cas_issues_exactly_one_permit_requester_a_first`
//!    — two attempts on one Task request the `CommitPermit` with
//!    independent snapshot bundles, write sets, and idempotency keys; the
//!    first requester is `Issued`, the second is durably `Superseded`
//!    (typed, carrying the winner's permit), and the loser's later
//!    re-request with its own write set is refused fail-closed.
//! 2. `..._requester_b_first` — the same competition with the request
//!    order swapped: the invariant set is order-independent.
//! 3. `cancel_racing_a_live_permit_linearizes_permit_first_...` — the
//!    winner holds the permit and has planned (but not converged) its
//!    commit when the Task is cancelled. As-landed `TaskAuthority`
//!    semantics: the cancellation applies (`cancel_epoch = 1`) but closes
//!    no attempt and does not clear the outstanding permit, so convergence
//!    still finalizes exactly one commit. The terminal state is unique and
//!    consistent: one commit, one cancel epoch, no double terminal.
//! 4. `cancel_before_any_permit_request_fails_closed_both_attempts` — the
//!    cancellation commits first: both open candidate attempts close
//!    pre-permit with closure receipts, and each attempt's later permit
//!    request is refused with the typed `CancelledBeforeEffect` decision
//!    bound to its own closure receipt. No permit is ever issued, nothing
//!    converges, the `TaskHead` never moves.
//!
//! Everything asserts only guarantees the landed `SqliteTaskAuthority`
//! already documents (`[TASK-COMMIT-001]`, `[TASK-CANCEL-002]`,
//! `[TASK-CANCEL-003]`, `[TASK-COMMIT-003]`); no authority semantics are
//! invented here.

use std::sync::Arc;

use nlos_artifact::{ContentDigest, CreateArtifactSpec, PutRevisionRequest, staging_id_for};
use nlos_runtime::FiberSpec;
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{
    FiberOutcome, SliceKRuntime, WriteFiberJob, fixture_bytes, seeded_key, spawn_write_fiber,
};
use nlos_task::{
    ArtifactPublicationExpectation, AttemptSpec, AttemptState, Authorities, CancelDecision,
    CancelRequest, PermitDecision, PermitRequest, PermitState, SnapshotBundle, TaskState,
    TaskStoreError, artifact_publication_plan_root, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CallbackId, CancellationScopeId, CommitPermitId, ExecutionFiberId, Generation,
    IdempotencyKey, OperationId, ReceiptId, ResourceGroupId, SchedulerDomainId, TaskAttemptId,
    TaskId, TaskSnapshotId,
};

fn slice_runtime(name: &str) -> (TempDir, Arc<SliceKRuntime>) {
    let dir = TempDir::new(name);
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    (dir, runtime)
}

fn slice_adapter() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("tokio adapter")
}

struct TempDir {
    root: std::path::PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-compete-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove slice-k temp root: {error}"),
        }
    }
}

/// One competing attempt of the dual-attempt fixture: its durable identity
/// plus the fully independent write set (distinct staging identity, digest,
/// and write-set root) and idempotency key it competes with.
struct Party {
    attempt_id: TaskAttemptId,
    scope_id: CancellationScopeId,
    stage_key: IdempotencyKey,
    write_bytes: Vec<u8>,
    write_set_root: [u8; 32],
    permit_key: IdempotencyKey,
    requested_at_key: IdempotencyKey,
}

/// The package-payload artifact every competitor writes against: head
/// revision 1, so a winner's write lands as revision 2.
fn payload_artifact(runtime: &SliceKRuntime, seed: u8) -> ArtifactId {
    let artifact_id = ArtifactId::from_bytes([seed.wrapping_add(10); 16]);
    let created_at_ms = runtime
        .wall_now_ms(seeded_key(seed, 12))
        .expect("artifact wall reading");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: seeded_key(seed, 11),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms,
        })
        .expect("create competing payload artifact");
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &fixture_bytes(seed, 128),
            created_at_ms,
        })
        .expect("put payload revision 1");
    artifact_id
}

/// Registers the second competing attempt on the same Task with its own
/// independent snapshot bundle (`[TASK-ATTEMPT-001]`: several attempts MAY
/// bind the same Task; each carries its own snapshot identity).
fn register_second_attempt(
    runtime: &SliceKRuntime,
    task_id: TaskId,
    seed: u8,
) -> (TaskAttemptId, CancellationScopeId) {
    let attempt_id = TaskAttemptId::from_bytes([seed.wrapping_add(31); 16]);
    let scope_id = CancellationScopeId::from_bytes([seed.wrapping_add(32); 16]);
    runtime
        .tasks
        .register_attempt(AttemptSpec {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            snapshot: SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(34); 16]),
                snapshot_digest: [seed.wrapping_add(35); 32],
                expected_head_commit_seq: 0,
                effect_history_root: empty_effect_history_root(),
                retry_fence_epoch: 0,
            },
            cancellation_scope_id: scope_id,
            cancellation_generation: Generation::INITIAL,
            idempotency_key: seeded_key(seed, 33),
            registered_at_ms: runtime
                .wall_now_i64(seeded_key(seed, 36))
                .expect("attempt B wall reading"),
        })
        .expect("register competing attempt B");
    (attempt_id, scope_id)
}

/// Builds one competitor's fully independent request material: its own
/// stage key (hence its own staging identity), its own write bytes (hence
/// its own digest), and therefore its own write-set root.
fn party_for(
    artifact: ArtifactId,
    attempt_id: TaskAttemptId,
    scope_id: CancellationScopeId,
    stage_key: IdempotencyKey,
    write_bytes: Vec<u8>,
    permit_key: IdempotencyKey,
    requested_at_key: IdempotencyKey,
) -> Party {
    let expectation = ArtifactPublicationExpectation {
        staging_id: staging_id_for(artifact, stage_key).into_bytes(),
        artifact_id: artifact,
        target_revision: 2,
        digest: ContentDigest::of_bytes(&write_bytes).into_bytes(),
        size_bytes: u64::try_from(write_bytes.len()).unwrap_or(u64::MAX),
    };
    Party {
        attempt_id,
        scope_id,
        stage_key,
        write_set_root: artifact_publication_plan_root(&[expectation])
            .expect("competitor write-set root"),
        write_bytes,
        permit_key,
        requested_at_key,
    }
}

/// Runs the linearized `CommitPermit` CAS for one competitor.
fn request_permit(runtime: &SliceKRuntime, task_id: TaskId, party: &Party) -> PermitDecision {
    runtime
        .tasks
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id,
                attempt_id: party.attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: party.write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: party.permit_key,
                valid_until_ms: i64::MAX,
                requested_at_ms: runtime
                    .wall_now_i64(party.requested_at_key)
                    .expect("permit wall reading"),
            },
        )
        .expect("permit request")
}

/// Runs the permit-bound write fiber of one competitor through its durable
/// prefix: driver Operation (register → dispatch → complete), staged
/// Artifact revision, and the commit plan.
async fn run_competitor_write(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    task_id: TaskId,
    artifact: ArtifactId,
    seed: u8,
    party: &Party,
    permit_id: CommitPermitId,
) -> FiberOutcome {
    let job = WriteFiberJob {
        operation_id: OperationId::from_bytes([seed.wrapping_add(44); 16]),
        callback_id: CallbackId::from_bytes([seed.wrapping_add(45); 16]),
        completion_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(46); 16]),
        expected_head_revision: 1,
        artifact_id: artifact,
        stage_key: party.stage_key,
        stage_bytes: party.write_bytes.clone().into(),
        stage_created_at_ms: runtime
            .wall_now_ms(seeded_key(seed, 41))
            .expect("stage wall reading"),
        permit: Some(permit_id),
        write_set_root: party.write_set_root,
        plan_key: seeded_key(seed, 47),
        planned_at_ms: runtime
            .wall_now_i64(seeded_key(seed, 48))
            .expect("plan wall reading"),
        task_id,
        attempt_id: party.attempt_id,
        attempt_generation: Generation::INITIAL,
    };
    let binding = runtime
        .materialize_process(seed, task_id, party.attempt_id, Generation::INITIAL)
        .expect("materialize competitor process binding");
    let spec = FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes([seed.wrapping_add(50); 16]),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: binding.agent_instance_id,
        agent_generation: binding.agent_instance_generation,
        process_id: binding.process_id,
        process_generation: binding.process_generation,
        task_attempt_id: Some(party.attempt_id),
        cancellation_scope_id: party.scope_id,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes([seed.wrapping_add(53); 16]),
        scheduler_domain_id: SchedulerDomainId::from_bytes([seed.wrapping_add(54); 16]),
        deadline: None,
    };
    let (_fiber, receiver) =
        spawn_write_fiber(Arc::clone(runtime), adapter, spec, job).expect("spawn competitor fiber");
    receiver
        .await
        .expect("fiber outcome channel")
        .expect("competitor write fiber")
}

/// The loser's retry boundary: after the durable `Superseded` fence, a
/// fresh-key re-request with its own write set must be refused fail-closed.
fn loser_retry_is_fenced(runtime: &SliceKRuntime, task_id: TaskId, seed: u8, party: &Party) {
    let retry = runtime.tasks.request_commit_permit_with_authorities_struct(
        Authorities::default(),
        PermitRequest {
            task_id,
            attempt_id: party.attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root: party.write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: seeded_key(seed, 59),
            valid_until_ms: i64::MAX,
            requested_at_ms: runtime
                .wall_now_i64(seeded_key(seed, 60))
                .expect("retry wall reading"),
        },
    );
    assert!(
        matches!(
            retry,
            Err(TaskStoreError::InvalidAttemptState {
                state: AttemptState::Superseded
            })
        ),
        "superseded loser retry must be refused fail-closed"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn competing_attempts_cas_issues_exactly_one_permit_requester_a_first() {
    let (_dir, runtime) = slice_runtime("compete-a-first");
    let adapter = slice_adapter();
    let seed = 0xD0_u8;

    let artifact = payload_artifact(&runtime, seed);
    let (task_id, attempt_a, scope_a) = runtime
        .register_task_and_attempt(seed)
        .expect("task and attempt A");
    let (attempt_b, scope_b) = register_second_attempt(&runtime, task_id, seed);
    let party_a = party_for(
        artifact,
        attempt_a,
        scope_a,
        seeded_key(seed, 40),
        fixture_bytes(seed.wrapping_add(200), 128),
        seeded_key(seed, 43),
        seeded_key(seed, 42),
    );
    let party_b = party_for(
        artifact,
        attempt_b,
        scope_b,
        seeded_key(seed, 58),
        fixture_bytes(seed.wrapping_add(202), 128),
        seeded_key(seed, 57),
        seeded_key(seed, 56),
    );

    // A requests first: the CAS issues exactly one permit, the head is
    // untouched, and the winner is durably the holder.
    let PermitDecision::Issued(winner_permit) = request_permit(&runtime, task_id, &party_a) else {
        panic!("first requester must win the permit CAS on a fresh task");
    };
    assert_eq!(winner_permit.attempt_id, attempt_a);
    let task = runtime.tasks.inspect_task(task_id).expect("task");
    assert_eq!(task.head_commit_seq, 0);
    assert_eq!(task.active_permit, Some(winner_permit.permit_id));

    // B requests second with its own independent write set: durably
    // superseded by A's permit, never receiving a permit row of its own.
    let PermitDecision::Superseded { winner: lost_to } =
        request_permit(&runtime, task_id, &party_b)
    else {
        panic!("second requester must lose the permit CAS");
    };
    assert_eq!(lost_to.permit_id, winner_permit.permit_id);
    assert_eq!(lost_to.attempt_id, attempt_a);
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_b)
            .expect("loser attempt")
            .state,
        AttemptState::Superseded
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_a)
            .expect("winner attempt")
            .state,
        AttemptState::CommitPermitted
    );

    // The winner completes its commit; the head advances exactly once.
    let outcome = run_competitor_write(
        &runtime,
        &adapter,
        task_id,
        artifact,
        seed,
        &party_a,
        winner_permit.permit_id,
    )
    .await;
    assert!(
        outcome.plan_id.is_some(),
        "winner fiber must plan the commit"
    );
    let now_ms = runtime.wall_now_i64(seeded_key(seed, 61)).expect("wall");
    let receipts = runtime.converge_pending(16, now_ms).expect("converge");
    assert_eq!(receipts.len(), 1, "exactly one plan exists to finalize");
    assert_eq!(receipts[0].task_receipt.task_id, task_id);
    assert_eq!(
        receipts[0].task_receipt.permit_id,
        Some(winner_permit.permit_id)
    );
    assert_eq!(receipts[0].task_receipt.new_head_commit_seq, 1);

    // Unique terminal facts: permit consumed, head at 1, winner committed,
    // loser terminally fenced.
    let task = runtime.tasks.inspect_task(task_id).expect("task");
    assert_eq!(task.head_commit_seq, 1);
    assert_eq!(task.state, TaskState::Active);
    assert_eq!(task.active_permit, None, "permit closed by the commit");
    assert_eq!(
        runtime
            .tasks
            .inspect_permit(task_id, winner_permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_a)
            .expect("attempt A")
            .state,
        AttemptState::Committed
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_b)
            .expect("attempt B")
            .state,
        AttemptState::Superseded,
        "supersession is terminal"
    );
    assert_eq!(
        runtime
            .artifacts
            .resolve_head(artifact, u64::MAX)
            .expect("head readback")
            .expect("head after commit")
            .revision,
        2
    );

    // The loser's retry boundary: its own write set under a fresh key is
    // refused fail-closed by the terminal Superseded fence.
    loser_retry_is_fenced(&runtime, task_id, seed, &party_b);

    // No double commit anywhere: a second drain finds nothing, the head
    // stays at 1, and the winner's commit receipt remains the only one.
    let later = runtime.wall_now_i64(seeded_key(seed, 62)).expect("wall");
    assert!(
        runtime
            .converge_pending(16, later)
            .expect("second drain")
            .is_empty()
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_task(task_id)
            .expect("task")
            .head_commit_seq,
        1
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn competing_attempts_cas_issues_exactly_one_permit_requester_b_first() {
    let (_dir, runtime) = slice_runtime("compete-b-first");
    let adapter = slice_adapter();
    let seed = 0xD1_u8;

    let artifact = payload_artifact(&runtime, seed);
    let (task_id, attempt_a, scope_a) = runtime
        .register_task_and_attempt(seed)
        .expect("task and attempt A");
    let (attempt_b, scope_b) = register_second_attempt(&runtime, task_id, seed);
    let party_a = party_for(
        artifact,
        attempt_a,
        scope_a,
        seeded_key(seed, 40),
        fixture_bytes(seed.wrapping_add(200), 128),
        seeded_key(seed, 43),
        seeded_key(seed, 42),
    );
    let party_b = party_for(
        artifact,
        attempt_b,
        scope_b,
        seeded_key(seed, 58),
        fixture_bytes(seed.wrapping_add(202), 128),
        seeded_key(seed, 57),
        seeded_key(seed, 56),
    );

    // B requests first this time: the winner set is order-independent.
    let PermitDecision::Issued(winner_permit) = request_permit(&runtime, task_id, &party_b) else {
        panic!("first requester must win the permit CAS on a fresh task");
    };
    assert_eq!(winner_permit.attempt_id, attempt_b);
    let PermitDecision::Superseded { winner: lost_to } =
        request_permit(&runtime, task_id, &party_a)
    else {
        panic!("second requester must lose the permit CAS");
    };
    assert_eq!(lost_to.permit_id, winner_permit.permit_id);
    assert_eq!(lost_to.attempt_id, attempt_b);
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_a)
            .expect("loser attempt")
            .state,
        AttemptState::Superseded
    );

    // The B winner completes its commit; same unique outcome as A-first.
    let outcome = run_competitor_write(
        &runtime,
        &adapter,
        task_id,
        artifact,
        seed,
        &party_b,
        winner_permit.permit_id,
    )
    .await;
    assert!(
        outcome.plan_id.is_some(),
        "winner fiber must plan the commit"
    );
    let now_ms = runtime.wall_now_i64(seeded_key(seed, 61)).expect("wall");
    let receipts = runtime.converge_pending(16, now_ms).expect("converge");
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].task_receipt.permit_id,
        Some(winner_permit.permit_id)
    );
    assert_eq!(receipts[0].task_receipt.new_head_commit_seq, 1);
    let task = runtime.tasks.inspect_task(task_id).expect("task");
    assert_eq!(task.head_commit_seq, 1);
    assert_eq!(task.active_permit, None);
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_b)
            .expect("winner attempt")
            .state,
        AttemptState::Committed
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_a)
            .expect("loser attempt")
            .state,
        AttemptState::Superseded
    );

    // The A loser's retry boundary is the same typed fail-closed fence.
    loser_retry_is_fenced(&runtime, task_id, seed, &party_a);
    let later = runtime.wall_now_i64(seeded_key(seed, 62)).expect("wall");
    assert!(
        runtime
            .converge_pending(16, later)
            .expect("second drain")
            .is_empty()
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_task(task_id)
            .expect("task")
            .head_commit_seq,
        1
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn cancel_racing_a_live_permit_linearizes_permit_first_with_single_commit() {
    let (_dir, runtime) = slice_runtime("race-cancel-after-permit");
    let adapter = slice_adapter();
    let seed = 0xE0_u8;

    let artifact = payload_artifact(&runtime, seed);
    let (task_id, attempt_a, scope_a) = runtime
        .register_task_and_attempt(seed)
        .expect("task and attempt A");
    let (attempt_b, scope_b) = register_second_attempt(&runtime, task_id, seed);
    let party_a = party_for(
        artifact,
        attempt_a,
        scope_a,
        seeded_key(seed, 40),
        fixture_bytes(seed.wrapping_add(200), 128),
        seeded_key(seed, 43),
        seeded_key(seed, 42),
    );
    let party_b = party_for(
        artifact,
        attempt_b,
        scope_b,
        seeded_key(seed, 58),
        fixture_bytes(seed.wrapping_add(202), 128),
        seeded_key(seed, 57),
        seeded_key(seed, 56),
    );

    // A wins the permit; B is durably superseded.
    let PermitDecision::Issued(winner_permit) = request_permit(&runtime, task_id, &party_a) else {
        panic!("first requester must win the permit CAS");
    };
    let PermitDecision::Superseded { .. } = request_permit(&runtime, task_id, &party_b) else {
        panic!("second requester must lose the permit CAS");
    };

    // The winner completes its durable prefix (staged revision + commit
    // plan) but has not converged yet: the race window is open.
    let outcome = run_competitor_write(
        &runtime,
        &adapter,
        task_id,
        artifact,
        seed,
        &party_a,
        winner_permit.permit_id,
    )
    .await;
    assert!(outcome.plan_id.is_some());
    assert_eq!(
        runtime
            .tasks
            .inspect_task(task_id)
            .expect("task")
            .head_commit_seq,
        0,
        "planned but not converged: the head has not moved"
    );

    // The cancellation lands inside the window.
    let cancel = runtime
        .tasks
        .cancel_task(CancelRequest {
            task_id,
            idempotency_key: seeded_key(seed, 70),
            requested_at_ms: runtime
                .wall_now_i64(seeded_key(seed, 71))
                .expect("cancel wall reading"),
        })
        .expect("cancel task");
    let CancelDecision::Applied {
        cancel_epoch,
        closed_attempts,
    } = cancel
    else {
        panic!("first cancel must apply");
    };
    assert_eq!(cancel_epoch, 1);
    // As-landed semantics ([TASK-CANCEL-002]/[TASK-COMMIT-003]): neither
    // the permit holder (CommitPermitted) nor the already-superseded loser
    // is an open pre-permit candidate, so the cancellation closes nothing
    // and does NOT clear the outstanding permit.
    assert!(
        closed_attempts.is_empty(),
        "permit-first linearization: cancel closes no permit-holding attempt"
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_task(task_id)
            .expect("task")
            .head_commit_seq,
        0,
        "cancel alone never advances the head"
    );

    // Convergence still finalizes the outstanding permit exactly once.
    let now_ms = runtime
        .wall_now_i64(seeded_key(seed, 72))
        .expect("converge wall reading");
    let receipts = runtime
        .converge_pending(16, now_ms)
        .expect("converge after cancel");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].task_receipt.task_id, task_id);
    assert_eq!(
        receipts[0].task_receipt.permit_id,
        Some(winner_permit.permit_id)
    );
    assert_eq!(receipts[0].task_receipt.new_head_commit_seq, 1);

    // Unique consistent terminal state: the commit happened exactly once
    // AND the cancellation is durably recorded — no double terminal, no
    // resurrected permit, monotone head.
    let task = runtime.tasks.inspect_task(task_id).expect("task");
    assert_eq!(task.head_commit_seq, 1);
    assert_eq!(task.state, TaskState::Cancelled);
    assert_eq!(task.cancel_epoch, 1);
    assert_eq!(task.active_permit, None);
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_a)
            .expect("winner attempt")
            .state,
        AttemptState::Committed
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_b)
            .expect("loser attempt")
            .state,
        AttemptState::Superseded
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_permit(task_id, winner_permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    assert_eq!(
        runtime
            .artifacts
            .resolve_head(artifact, u64::MAX)
            .expect("head readback")
            .expect("head after commit")
            .revision,
        2
    );

    // No double commit on a further drain, and a second cancel key cannot
    // re-apply or re-increment the epoch.
    let later = runtime
        .wall_now_i64(seeded_key(seed, 73))
        .expect("drain wall reading");
    assert!(
        runtime
            .converge_pending(16, later)
            .expect("second drain")
            .is_empty()
    );
    let replay = runtime
        .tasks
        .cancel_task(CancelRequest {
            task_id,
            idempotency_key: seeded_key(seed, 74),
            requested_at_ms: later,
        })
        .expect("second cancel");
    assert!(matches!(
        replay,
        CancelDecision::AlreadyCancelled { cancel_epoch: 1 }
    ));
    assert_eq!(
        runtime
            .tasks
            .inspect_task(task_id)
            .expect("task")
            .head_commit_seq,
        1
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn cancel_before_any_permit_request_fails_closed_both_attempts() {
    let (_dir, runtime) = slice_runtime("race-cancel-first");
    let seed = 0xF0_u8;

    let artifact = payload_artifact(&runtime, seed);
    let (task_id, attempt_a, _scope_a) = runtime
        .register_task_and_attempt(seed)
        .expect("task and attempt A");
    let (attempt_b, _scope_b) = register_second_attempt(&runtime, task_id, seed);
    let party_a = party_for(
        artifact,
        attempt_a,
        CancellationScopeId::from_bytes([seed.wrapping_add(22); 16]),
        seeded_key(seed, 40),
        fixture_bytes(seed.wrapping_add(200), 128),
        seeded_key(seed, 43),
        seeded_key(seed, 42),
    );
    let party_b = party_for(
        artifact,
        attempt_b,
        CancellationScopeId::from_bytes([seed.wrapping_add(32); 16]),
        seeded_key(seed, 58),
        fixture_bytes(seed.wrapping_add(202), 128),
        seeded_key(seed, 57),
        seeded_key(seed, 56),
    );

    // The cancellation commits before any permit request: both open
    // candidate attempts close pre-permit with closure receipts.
    let cancel = runtime
        .tasks
        .cancel_task(CancelRequest {
            task_id,
            idempotency_key: seeded_key(seed, 70),
            requested_at_ms: runtime
                .wall_now_i64(seeded_key(seed, 71))
                .expect("cancel wall reading"),
        })
        .expect("cancel task");
    let CancelDecision::Applied {
        cancel_epoch,
        ref closed_attempts,
    } = cancel
    else {
        panic!("first cancel must apply");
    };
    assert_eq!(cancel_epoch, 1);
    assert_eq!(
        closed_attempts.len(),
        2,
        "both open candidates close pre-permit"
    );
    let closed_a = closed_attempts
        .iter()
        .find(|closed| closed.attempt_id == attempt_a)
        .expect("attempt A closed by the cancel");
    let closed_b = closed_attempts
        .iter()
        .find(|closed| closed.attempt_id == attempt_b)
        .expect("attempt B closed by the cancel");

    // Each attempt's later permit request is refused fail-closed with the
    // typed pre-permit cancellation outcome, bound to its own closure
    // receipt; no permit is ever issued to either competitor.
    for (party, closed) in [(&party_a, closed_a), (&party_b, closed_b)] {
        let PermitDecision::CancelledBeforeEffect { receipt_id } =
            request_permit(&runtime, task_id, party)
        else {
            panic!("permit request on a cancelled task must be CancelledBeforeEffect");
        };
        assert_eq!(receipt_id, closed.receipt_id);
    }

    // Nothing was ever issued, nothing converges, the head never moves.
    let now_ms = runtime
        .wall_now_i64(seeded_key(seed, 72))
        .expect("converge wall reading");
    assert!(
        runtime
            .converge_pending(16, now_ms)
            .expect("converge")
            .is_empty()
    );
    let task = runtime.tasks.inspect_task(task_id).expect("task");
    assert_eq!(task.head_commit_seq, 0);
    assert_eq!(task.state, TaskState::Cancelled);
    assert_eq!(task.active_permit, None);
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_a)
            .expect("attempt A")
            .state,
        AttemptState::Cancelled
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_b)
            .expect("attempt B")
            .state,
        AttemptState::Cancelled
    );
    assert_eq!(
        runtime
            .artifacts
            .resolve_head(artifact, u64::MAX)
            .expect("head readback")
            .expect("payload head")
            .revision,
        1,
        "the artifact stays at its pre-task payload revision"
    );
}
