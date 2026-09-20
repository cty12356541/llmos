//! W27-C / B-PROCESS-003 §W16-003 runtime-side batch-cancel linkage matrix:
//! when a process-side batch cancel (crash/terminate →
//! `propagate_cancel_to_fibers` receipts) arrives,
//! [`TokioRuntimeAdapter::cancel_process_fibers`] consumes the propagation
//! and drives runtime tree-cancel through the cancellation scopes of every
//! live fiber under the fenced `(process_id, process_generation)`.
//!
//! Coverage map (anti-duplication survey against the existing suite):
//!
//! already covered elsewhere, NOT re-tested here:
//! - single-scope cancel semantics (durable-wait suspend cancel, wake/cancel
//!   double-order races, respawn fences, late single-fiber callbacks) —
//!   `tests/cancel_late_callback_matrix.rs` §6.1 rows 1–6;
//! - terminal process fail-closed gates on resume/snapshot —
//!   `tests/process_crash_propagation.rs`;
//! - process-domain receipt facts alone (idempotent replay, stale fence,
//!   inspect fail-closed) — `nlos-process/tests/fiber_cancel_propagation.rs`.
//!
//! new in this file (the batch dimension on top of those):
//! 1. a mixed-state cohort (running / parked-on-durable-wait / already
//!    terminal, one shared scope) driven to unique terminals by one batch
//!    cancel — tree-cancel via scopes, terminal states never rewritten,
//!    durable wait rows left `PENDING`, durable incarnation inspect
//!    fail-closed;
//! 2. generation fences: same process id under another process generation
//!    and a different process id are untouched;
//! 3. wake→batch-cancel ordering: unique `Cancelled` terminal, durable wake
//!    fact kept `WOKEN`, join returns `Cancelled`;
//! 4. batch-cancel→late-callback ordering: buffered delivery without panic,
//!    Operation wake `NotWaiting`, ready-`Cancelled` operation wait, empty
//!    rearm, no resurrection, durable fact still consumable;
//! 5. respawn guards after batch cancel (cancelled scope, scope generation
//!    fence, fresh-scope runtime spawn vs durable incarnation fence) and
//!    idempotent linkage replay;
//! 6. fail-closed authority gates (non-terminal process, stale fencing
//!    token) with zero runtime side effect.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_channel::{
    ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest, EnqueueDecision,
    EnqueueRequest,
};
use nlos_process::{
    CreateIsolationDomainRequest, FiberIncarnationDecision, IsolationDomainDecision,
    ProcessAuthority, ProcessBindingDecision, ProcessLifecycleState,
    PropagateCancelToFibersRequest, PropagateCrashRequest, RegisterDelegatedProcessRequest,
    RegisterFiberIncarnationRequest,
};
use nlos_runtime::{
    FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter, RuntimeError, WakeOutcome,
    WakeSink,
};
use nlos_runtime_tokio::{
    DeliveryReport, ProcessFiberCancelReport, TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, ProcessId, ResourceGroupId, SchedulerDomainId, TaskAttemptId, TaskId,
};
use nlos_wait::{BindingId, NotifyCommitsRequest, RegisterWaitRequest, WaitAuthority, WaitState};
use tokio::runtime::Handle;

/// Generous bound for waits that must resolve.
const RESOLVE: Duration = Duration::from_secs(5);

fn id_bytes(value: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(value as u64).to_be_bytes());
    bytes
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
}

fn next_generation() -> Generation {
    Generation::INITIAL.checked_next().expect("next generation")
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-runtime-tokio-batch-cancel-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A registered process binding under which runtime fibers are linked.
struct ProcessFixture {
    process: ProcessAuthority,
    process_id: ProcessId,
    process_generation: Generation,
    process_fencing_token: nlos_process::FencingToken,
}

fn open_process_fixture(root: &Root, seed: u8) -> ProcessFixture {
    let process = ProcessAuthority::open(root.path()).expect("open process");
    let domain = match process.create_isolation_domain(CreateIsolationDomainRequest {
        policy_digest: [seed; 32],
        idempotency_key: key(seed.wrapping_add(1)),
        created_at_ms: 1_000,
    }) {
        Ok(
            IsolationDomainDecision::Created(record) | IsolationDomainDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("domain: {error}"),
    };
    let binding = match process.register_delegated_process(RegisterDelegatedProcessRequest {
        task_id: TaskId::from_bytes([seed.wrapping_add(2); 16]),
        task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(3); 16]),
        attempt_generation: Generation::INITIAL,
        isolation_domain_id: domain.isolation_domain_id,
        isolation_domain_generation: domain.generation,
        isolation_domain_fencing_token: domain.fencing_token,
        idempotency_key: key(seed.wrapping_add(4)),
        created_at_ms: 2_000,
    }) {
        Ok(
            ProcessBindingDecision::Registered(record) | ProcessBindingDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("register process: {error}"),
    };
    ProcessFixture {
        process,
        process_id: binding.process_id,
        process_generation: binding.process_generation,
        process_fencing_token: binding.process_fencing_token,
    }
}

/// Registers one durable fiber incarnation under the fixture's process; the
/// runtime fiber of the same id is then covered by batch cancel receipts.
fn register_incarnation(fixture: &ProcessFixture, fiber: ExecutionFiberId, seed: u8) {
    match fixture
        .process
        .register_fiber_incarnation(RegisterFiberIncarnationRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            binding: fiber,
            idempotency_key: key(seed),
            registered_at_ms: 3_000,
        }) {
        Ok(FiberIncarnationDecision::Registered(_)) => {}
        other => panic!("incarnation: {other:?}"),
    }
}

fn propagate_crash(fixture: &ProcessFixture, key_seed: u8) {
    fixture
        .process
        .propagate_crash(PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: key(key_seed),
            marked_at_ms: 9_000,
        })
        .expect("propagate crash");
}

/// The batch-cancel linkage entry under test: re-consumes the durable
/// propagation (idempotent replay of the crash-time batch) and drives the
/// runtime-side scope cancels.
fn cancel_process_fibers(
    runtime: &TokioRuntimeAdapter,
    fixture: &ProcessFixture,
    key_seed: u8,
) -> Result<ProcessFiberCancelReport, nlos_runtime_tokio::ChannelWaitError> {
    runtime.cancel_process_fibers(
        &fixture.process,
        PropagateCancelToFibersRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            lifecycle_state: ProcessLifecycleState::Crashed,
            idempotency_key: key(key_seed),
            cancelled_at_ms: 9_000,
        },
    )
}

struct Pair {
    channel: Arc<ChannelAuthority>,
    wait: Arc<WaitAuthority>,
}

fn open_pair(root: &Root) -> Pair {
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait = Arc::new(WaitAuthority::open(root.path(), Arc::clone(&channel)).expect("wait"));
    Pair { channel, wait }
}

fn create_channel(authority: &ChannelAuthority, seed: u8) -> ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: key(seed),
            created_at_ms: 900,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

/// Appends one entry and returns its durable sequence.
fn enqueue(authority: &ChannelAuthority, channel_id: nlos_types::ChannelId, seed: u8) -> u64 {
    let head = authority.inspect_channel(channel_id).expect("channel head");
    match authority
        .enqueue(EnqueueRequest {
            channel_id,
            expected_generation: head.generation,
            expected_fencing_token: head.fencing_token,
            payload: vec![seed; 8],
            idempotency_key: key(seed),
            enqueued_at_ms: 1_500,
        })
        .expect("enqueue")
    {
        EnqueueDecision::Enqueued(record) | EnqueueDecision::Replayed(record) => record.sequence,
    }
}

fn notify(
    authority: &WaitAuthority,
    channel_id: nlos_types::ChannelId,
    up_to_sequence: u64,
    key_seed: u8,
) -> nlos_wait::WakeReport {
    authority
        .notify_commits(NotifyCommitsRequest {
            channel_id,
            up_to_sequence,
            notified_at_ms: 2_000,
            idempotency_key: key(key_seed),
        })
        .expect("notify commits")
}

fn register_request(channel: &ChannelRecord, target: u64, key_seed: u8) -> RegisterWaitRequest {
    RegisterWaitRequest {
        binding: binding(1),
        channel_id: channel.channel_id,
        target_sequence: target,
        idempotency_key: key(key_seed),
        registered_at_ms: 1_000,
    }
}

fn runtime(max_live_fibers: usize) -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime")
}

/// One fiber spec linked to the fixture process (or, for unlinked controls,
/// to an arbitrary process identity), under its own scope.
fn linked_spec(
    index: usize,
    scope: CancellationScopeId,
    process_id: ProcessId,
    process_generation: Generation,
) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: AgentInstanceId::from_bytes(id_bytes(index)),
        agent_generation: Generation::INITIAL,
        process_id,
        process_generation,
        task_attempt_id: None,
        cancellation_scope_id: scope,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(id_bytes(1)),
        scheduler_domain_id: SchedulerDomainId::from_bytes(id_bytes(1)),
        deadline: None,
    }
}

fn spawn(
    runtime: &TokioRuntimeAdapter,
    spec: FiberSpec,
    future: nlos_runtime::FiberFuture,
) -> FiberHandle {
    runtime.spawn_fiber(spec, future).expect("spawn")
}

async fn wait_for_state(runtime: &TokioRuntimeAdapter, handle: FiberHandle, expected: FiberState) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if runtime.inspect(handle) == Ok(expected) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fiber did not reach expected state");
}

/// Lets any (wrong) cross-task wakeup propagate before a "still terminal"
/// re-check.
async fn settle() {
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
}

async fn await_settle_and_assert_state(
    runtime: &TokioRuntimeAdapter,
    handle: FiberHandle,
    expected: FiberState,
) {
    settle().await;
    assert_eq!(
        runtime.inspect(handle),
        Ok(expected),
        "state must be stable across late callbacks"
    );
}

/// The in-body durable-wait park (same shape as
/// `cancel_late_callback_matrix.rs`): registers on its own behalf and
/// resolves only on its wake; `pend_after_wake` keeps the fiber alive-but-
/// idle after a delivered wake for the ordering-race controls.
async fn park_on_durable_wait(
    adapter: TokioRuntimeAdapter,
    waits: Arc<WaitAuthority>,
    handle: FiberHandle,
    request: RegisterWaitRequest,
    pend_after_wake: bool,
) -> FiberExit {
    let wait = adapter
        .wait_for_channel(handle, &waits, request)
        .expect("in-fiber durable wait registration");
    match wait.await {
        WaitOutcome::Woken => {
            if pend_after_wake {
                pending::<()>().await;
            }
            FiberExit::Completed
        }
        WaitOutcome::Cancelled => FiberExit::Cancelled,
    }
}

fn spawn_park(
    runtime: &TokioRuntimeAdapter,
    waits: &Arc<WaitAuthority>,
    request: RegisterWaitRequest,
    spec: FiberSpec,
    pend_after_wake: bool,
) -> FiberHandle {
    let handle = FiberHandle {
        fiber_id: spec.fiber_id,
        generation: spec.fiber_generation,
    };
    runtime
        .spawn_fiber(
            spec,
            Box::pin(park_on_durable_wait(
                runtime.clone(),
                Arc::clone(waits),
                handle,
                request,
                pend_after_wake,
            )),
        )
        .expect("spawn parked fiber")
}

/// Matrix row 1 — 混合状态 cohort 批量取消：given a cohort of one shared
/// scope holding two running fibers, one fiber parked in-body on a durable
/// wait, and one already-terminal fiber; when the process crashes and the
/// batch-cancel linkage runs; then every non-terminal fiber reaches the
/// unique terminal `Cancelled`, the already-terminal fiber keeps its own
/// terminal (`Completed` is never rewritten), the report counts the sweep
/// exactly, the durable wait row stays `PENDING`, and the durable
/// incarnation inspect fails closed.
#[allow(clippy::too_many_lines)] // One test covers the mixed cohort, receipts, terminal uniqueness and dispositions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_cancel_drives_mixed_state_cohort_to_unique_terminals() {
    let root = Root::new("mixed-cohort");
    let fixture = open_process_fixture(&root, 0x11);
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 0xA0);
    let runtime = runtime(8);

    let shared_scope = CancellationScopeId::from_bytes(id_bytes(510));
    let wait_scope = CancellationScopeId::from_bytes(id_bytes(511));
    let done_scope = CancellationScopeId::from_bytes(id_bytes(512));

    let running_a = spawn(
        &runtime,
        linked_spec(
            1,
            shared_scope,
            fixture.process_id,
            fixture.process_generation,
        ),
        Box::pin(pending()),
    );
    let running_b = spawn(
        &runtime,
        linked_spec(
            2,
            shared_scope,
            fixture.process_id,
            fixture.process_generation,
        ),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, running_a, FiberState::Running).await;
    wait_for_state(&runtime, running_b, FiberState::Running).await;

    let request = register_request(&channel, 1, 0xB1);
    let parked = spawn_park(
        &runtime,
        &pair.wait,
        request,
        linked_spec(
            3,
            wait_scope,
            fixture.process_id,
            fixture.process_generation,
        ),
        false,
    );
    wait_for_state(&runtime, parked, FiberState::WaitingIo).await;

    let done = spawn(
        &runtime,
        linked_spec(
            4,
            done_scope,
            fixture.process_id,
            fixture.process_generation,
        ),
        Box::pin(async { FiberExit::Completed }),
    );
    wait_for_state(&runtime, done, FiberState::Completed).await;

    // Durable incarnations for the running and parked fibers only: the
    // batch receipts cover exactly these two bindings.
    register_incarnation(&fixture, running_a.fiber_id, 0x51);
    register_incarnation(&fixture, parked.fiber_id, 0x52);

    propagate_crash(&fixture, 0x70);
    let report = cancel_process_fibers(&runtime, &fixture, 0x70).expect("batch cancel linkage");

    assert_eq!(report.matched_fibers, 4, "all four linked fibers match");
    assert_eq!(report.already_terminal, 1, "exactly the Completed fiber");
    assert_eq!(
        report.canceled_scopes, 3,
        "shared scope + wait scope + terminal fiber's scope"
    );
    assert_eq!(report.vanished_scopes, 0);
    assert_eq!(
        report.decision.receipts().len(),
        2,
        "durable receipts cover the two registered incarnations"
    );
    for binding in [running_a.fiber_id, parked.fiber_id] {
        assert!(
            report
                .decision
                .receipts()
                .iter()
                .any(|receipt| receipt.binding == binding),
            "receipt for {binding:?} must be present"
        );
    }

    wait_for_state(&runtime, running_a, FiberState::Cancelled).await;
    wait_for_state(&runtime, running_b, FiberState::Cancelled).await;
    wait_for_state(&runtime, parked, FiberState::Cancelled).await;
    await_settle_and_assert_state(&runtime, done, FiberState::Completed).await;

    let durable = pair
        .wait
        .list_waits(Some(channel.channel_id))
        .expect("list waits");
    assert_eq!(durable.len(), 1);
    assert_eq!(
        durable[0].state,
        WaitState::Pending,
        "runtime-side batch cancel must leave the durable row PENDING"
    );

    assert!(
        fixture
            .process
            .inspect_fiber_incarnation(fixture.process_id, running_a.fiber_id)
            .is_err(),
        "durable incarnation inspect must fail closed after the batch"
    );
}

/// Matrix row 2 — 代次围栏：given one fiber under `(P, G0)`, one under the
/// same process id at a bumped process generation, and one under a
/// different process id; when the batch cancel runs for `(P, G0)`; then
/// only the first is cancelled — the sweep fence matches exactly the
/// presented `(process_id, process_generation)` and nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_cancel_is_fenced_by_process_identity_and_generation() {
    let root = Root::new("fence");
    let fixture = open_process_fixture(&root, 0x21);
    let runtime = runtime(8);

    let scope_a = CancellationScopeId::from_bytes(id_bytes(520));
    let scope_b = CancellationScopeId::from_bytes(id_bytes(521));
    let scope_c = CancellationScopeId::from_bytes(id_bytes(522));
    let other_process = ProcessId::from_bytes(id_bytes(0xEE));

    let fenced_in = spawn(
        &runtime,
        linked_spec(1, scope_a, fixture.process_id, fixture.process_generation),
        Box::pin(pending()),
    );
    let bumped_generation = spawn(
        &runtime,
        linked_spec(2, scope_b, fixture.process_id, next_generation()),
        Box::pin(pending()),
    );
    let other_process_fiber = spawn(
        &runtime,
        linked_spec(3, scope_c, other_process, fixture.process_generation),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, fenced_in, FiberState::Running).await;
    wait_for_state(&runtime, bumped_generation, FiberState::Running).await;
    wait_for_state(&runtime, other_process_fiber, FiberState::Running).await;

    propagate_crash(&fixture, 0x71);
    let report = cancel_process_fibers(&runtime, &fixture, 0x71).expect("batch cancel linkage");
    assert_eq!(
        report.matched_fibers, 1,
        "only the fiber at the fenced (process, generation) matches"
    );
    assert_eq!(report.canceled_scopes, 1);

    wait_for_state(&runtime, fenced_in, FiberState::Cancelled).await;
    await_settle_and_assert_state(&runtime, bumped_generation, FiberState::Running).await;
    await_settle_and_assert_state(&runtime, other_process_fiber, FiberState::Running).await;
}

/// Matrix row 3 — 先唤醒后批量取消：given a linked fiber whose wake is
/// delivered first (back to `Running`); when the process then crashes and
/// the batch cancel runs; then the terminal is uniquely `Cancelled` (the
/// biased terminal select never lets a consumed wake complete a cancelled
/// fiber), join returns `Cancelled`, and the durable wake fact stays
/// `WOKEN`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_then_batch_cancel_keeps_unique_cancelled_terminal_and_durable_wake() {
    let root = Root::new("wake-then-batch");
    let fixture = open_process_fixture(&root, 0x31);
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 0xA0);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(530));
    let request = register_request(&channel, 1, 0xB1);
    let sink = runtime.channel_wait_sink();

    let handle = spawn_park(
        &runtime,
        &pair.wait,
        request,
        linked_spec(1, scope, fixture.process_id, fixture.process_generation),
        true,
    );
    wait_for_state(&runtime, handle, FiberState::WaitingIo).await;
    register_incarnation(&fixture, handle.fiber_id, 0x53);

    enqueue(&pair.channel, channel.channel_id, 0xE0);
    let report = notify(&pair.wait, channel.channel_id, 1, 0xD0);
    assert_eq!(report.woken.len(), 1);
    assert_eq!(
        sink.deliver(&report).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    wait_for_state(&runtime, handle, FiberState::Running).await;

    propagate_crash(&fixture, 0x72);
    cancel_process_fibers(&runtime, &fixture, 0x72).expect("batch cancel linkage");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;

    assert_eq!(
        runtime.join_fiber(handle).expect("join cancelled fiber"),
        FiberExit::Cancelled,
        "join must observe the unique Cancelled exit"
    );

    let durable = pair
        .wait
        .list_waits(Some(channel.channel_id))
        .expect("list waits");
    assert_eq!(durable.len(), 1);
    assert_eq!(
        durable[0].state,
        WaitState::Woken,
        "the delivered wake is a durable fact; batch cancel must not undo it"
    );
}

/// Matrix row 4 — 批量取消后晚到 callback：given a linked fiber parked
/// in-body and cancelled by the batch; when the commit notification
/// arrives afterwards and every late-callback endpoint fires; then the
/// channel delivery buffers without panic, the Operation wake reports
/// `NotWaiting`, a fresh Operation registration resolves ready
/// `Cancelled`, rearm arms nothing, the fiber stays `Cancelled`, and the
/// durable wake fact remains consumable by a later legitimate waiter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_callbacks_after_batch_cancel_buffer_without_panic_or_resurrection() {
    let root = Root::new("late-callbacks");
    let fixture = open_process_fixture(&root, 0x41);
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 0xA0);
    let runtime = runtime(8);
    let scope = CancellationScopeId::from_bytes(id_bytes(540));
    let request = register_request(&channel, 1, 0xB1);
    let channel_sink = runtime.channel_wait_sink();
    let operation_sink = runtime.wake_sink();

    let handle = spawn_park(
        &runtime,
        &pair.wait,
        request,
        linked_spec(1, scope, fixture.process_id, fixture.process_generation),
        false,
    );
    wait_for_state(&runtime, handle, FiberState::WaitingIo).await;

    propagate_crash(&fixture, 0x73);
    cancel_process_fibers(&runtime, &fixture, 0x73).expect("batch cancel linkage");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;

    enqueue(&pair.channel, channel.channel_id, 0xE0);
    let report = notify(&pair.wait, channel.channel_id, 1, 0xD0);
    assert_eq!(report.woken.len(), 1);
    assert_eq!(
        channel_sink
            .deliver(&report)
            .expect("late deliver must not panic"),
        DeliveryReport {
            delivered: 1,
            buffered: 1
        }
    );
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;

    let operation = OperationId::from_bytes(id_bytes(41));
    assert_eq!(
        operation_sink.wake(&handle, operation, Generation::INITIAL),
        Ok(WakeOutcome::NotWaiting),
        "a cancelled fiber is never woken by the Operation endpoint"
    );
    let operation_wait = runtime
        .wait_for_operation(handle, operation, Generation::INITIAL)
        .expect("operation wait on cancelled fiber");
    assert_eq!(
        tokio::time::timeout(RESOLVE, operation_wait)
            .await
            .expect("resolves"),
        WaitOutcome::Cancelled
    );
    let rearm = runtime
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("rearm on cancelled fiber");
    assert!(
        rearm.satisfied.is_empty() && rearm.pending.is_empty(),
        "a batch-cancelled fiber must arm nothing"
    );
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;

    // The durable wake fact stays consumable by a fresh, unlinked waiter:
    // the cancelled fiber itself is never involved.
    let replay_scope = CancellationScopeId::from_bytes(id_bytes(541));
    let other_process = ProcessId::from_bytes(id_bytes(0xEF));
    let replay_handle = spawn(
        &runtime,
        linked_spec(9, replay_scope, other_process, fixture.process_generation),
        Box::pin(pending()),
    );
    let replay_wait = runtime
        .wait_for_channel(replay_handle, &pair.wait, request)
        .expect("replay wait");
    assert_eq!(
        tokio::time::timeout(RESOLVE, replay_wait)
            .await
            .expect("replay resolves"),
        WaitOutcome::Woken,
        "the WOKEN durable row must stay consumable after the batch cancel"
    );
    runtime
        .cancel_scope(replay_scope, Generation::INITIAL)
        .expect("cleanup replay fiber");
}

/// Matrix row 5 — 批量取消后重 spawn 守卫与幂等重放：after the batch
/// cancel, every respawn into a cancelled scope is fenced (`Cancelled`),
/// the scope id stays generation-locked (`InvalidGeneration` on a bumped
/// cancellation generation), a fresh scope still spawns at the runtime
/// level while the durable side rejects the incarnation registration
/// (`FiberIncarnationCancelled` — the durable fence, not a runtime one),
/// and re-invoking the linkage with the same idempotency key replays
/// without error or double side effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_cancel_respawn_is_fenced_and_linkage_replay_is_idempotent() {
    let root = Root::new("respawn-guard");
    let fixture = open_process_fixture(&root, 0x51);
    let runtime = runtime(8);
    let scope = CancellationScopeId::from_bytes(id_bytes(550));

    let handle = spawn(
        &runtime,
        linked_spec(1, scope, fixture.process_id, fixture.process_generation),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, handle, FiberState::Running).await;
    register_incarnation(&fixture, handle.fiber_id, 0x54);

    propagate_crash(&fixture, 0x74);
    cancel_process_fibers(&runtime, &fixture, 0x74).expect("batch cancel linkage");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;

    // (a) any new fiber identity into the cancelled scope is rejected.
    assert_eq!(
        runtime.spawn_fiber(
            linked_spec(2, scope, fixture.process_id, fixture.process_generation),
            Box::pin(pending())
        ),
        Err(RuntimeError::Cancelled),
        "a batch-cancelled scope must not accept new fibers"
    );
    // (b) the same scope id under a bumped cancellation generation stays
    // generation-locked.
    let mut bumped = linked_spec(2, scope, fixture.process_id, fixture.process_generation);
    bumped.cancellation_generation = next_generation();
    assert_eq!(
        runtime.spawn_fiber(bumped, Box::pin(pending())),
        Err(RuntimeError::InvalidGeneration),
        "a scope id is bound to exactly one cancellation generation"
    );

    // (c) idempotent linkage replay: same batch key → Replayed decision,
    // the still-registered terminal record and its live scope entry keep
    // the sweep deterministic, and no double side effect occurs.
    let replay = cancel_process_fibers(&runtime, &fixture, 0x74).expect("replay linkage");
    assert!(
        matches!(
            replay.decision,
            nlos_process::FiberCancelPropagationDecision::Replayed(_)
        ),
        "same-key re-invocation must replay the durable batch"
    );
    assert_eq!(
        replay.matched_fibers, 1,
        "the terminal record still matches"
    );
    assert_eq!(replay.already_terminal, 1);
    assert_eq!(replay.canceled_scopes, 1, "the scope entry is still live");
    assert_eq!(replay.vanished_scopes, 0);
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;

    // (d) a fresh scope under the SAME (process, generation) still spawns
    // at the runtime level — the runtime fence is scope-granular — while
    // the durable side rejects the incarnation registration.
    let fresh_scope = CancellationScopeId::from_bytes(id_bytes(551));
    let fresh = spawn(
        &runtime,
        linked_spec(
            3,
            fresh_scope,
            fixture.process_id,
            fixture.process_generation,
        ),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, fresh, FiberState::Running).await;
    assert!(
        fixture
            .process
            .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: fixture.process_fencing_token,
                binding: fresh.fiber_id,
                idempotency_key: key(0x55),
                registered_at_ms: 9_500,
            })
            .is_err(),
        "the durable side must fence incarnations under the terminal process"
    );
    runtime
        .cancel_scope(fresh_scope, Generation::INITIAL)
        .expect("cleanup fresh-scope fiber");
}

/// Matrix row 6 — fail-closed 权威门零副作用：when the linkage runs before
/// the process is terminal (corrupt-state rejection) or with a stale
/// fencing token; then the call fails closed, no scope is cancelled and
/// every fiber keeps running — the runtime side effect happens only after
/// the durable decision succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_cancel_fails_closed_on_authority_gates_without_runtime_side_effect() {
    let root = Root::new("fail-closed");
    let fixture = open_process_fixture(&root, 0x61);
    let runtime = runtime(8);
    let scope = CancellationScopeId::from_bytes(id_bytes(560));

    let handle = spawn(
        &runtime,
        linked_spec(1, scope, fixture.process_id, fixture.process_generation),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, handle, FiberState::Running).await;

    // (a) the process is still Active: the propagation entry rejects it
    // before any runtime side effect.
    let active = runtime
        .cancel_process_fibers(
            &fixture.process,
            PropagateCancelToFibersRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: fixture.process_fencing_token,
                lifecycle_state: ProcessLifecycleState::Crashed,
                idempotency_key: key(0x76),
                cancelled_at_ms: 9_000,
            },
        )
        .expect_err("active process must fail the linkage closed");
    assert!(
        matches!(
            active,
            nlos_runtime_tokio::ChannelWaitError::ProcessAuthority(
                nlos_process::ProcessAuthorityError::CorruptRecord(_)
            )
        ),
        "expected CorruptRecord on an active process, got {active:?}"
    );

    // (b) after the crash, a stale fencing token fails closed the same way.
    propagate_crash(&fixture, 0x75);
    let stale = runtime
        .cancel_process_fibers(
            &fixture.process,
            PropagateCancelToFibersRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: [0xAB; 32],
                lifecycle_state: ProcessLifecycleState::Crashed,
                idempotency_key: key(0x77),
                cancelled_at_ms: 9_001,
            },
        )
        .expect_err("stale fence must fail the linkage closed");
    assert!(
        matches!(
            stale,
            nlos_runtime_tokio::ChannelWaitError::ProcessAuthority(
                nlos_process::ProcessAuthorityError::StaleProcessBinding
            )
        ),
        "expected StaleProcessBinding, got {stale:?}"
    );

    // Zero runtime side effect: the fiber keeps running and the scope was
    // never cancelled (a fresh identity into it still spawns).
    await_settle_and_assert_state(&runtime, handle, FiberState::Running).await;
    let probe = spawn(
        &runtime,
        linked_spec(2, scope, fixture.process_id, fixture.process_generation),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, probe, FiberState::Running).await;

    // The correct fence still works afterwards (the gates do not poison the
    // linkage), and cleans the cohort up.
    cancel_process_fibers(&runtime, &fixture, 0x75).expect("correct fence linkage");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;
    wait_for_state(&runtime, probe, FiberState::Cancelled).await;
}
