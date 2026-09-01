//! ROAD-B-006 cancel/late-callback matrix for the real durable-wait suspend
//! path: every fiber under test parks **in its own body** through
//! `TokioRuntimeAdapter::wait_for_channel` (durable registration plus the
//! in-memory handshake), mirroring `durable_wait_scale.rs` — not a
//! `pending()` stub with an externally registered wait.
//!
//! Coverage map (anti-duplication survey against the existing suite):
//!
//! already covered elsewhere, NOT re-tested here:
//! - scope cancel of a plain `pending()` fiber → `Cancelled` +
//!   stale-generation fence (`tests/runtime.rs`);
//! - scope cancel resolves an externally registered Operation wait
//!   (`tests/wake.rs::scope_cancellation_resolves_wait_as_cancelled`);
//! - scope cancel keeps the durable row `PENDING` for an externally
//!   registered channel wait
//!   (`tests/channel_wait.rs::scope_cancellation_cancels_wait_but_keeps_durable_row_pending`);
//! - fiber termination purge, shutdown, rearm + scope cancel
//!   (`tests/channel_wait.rs`, `tests/channel_rehydration.rs`);
//! - durable-side `cancel_wait` replay facts
//!   (`tests/fiber_replay.rs::resume_reports_cancelled_events_as_facts_with_zero_action`);
//! - Operation/outbox-level late callback after `request_cancel`
//!   (`tests/outbox.rs`).
//!
//! new in this file:
//! 1. cancel while parked in-body on a durable wait (terminal state +
//!    durable row disposition);
//! 2. wait registration after scope cancel → ready `Cancelled` with zero
//!    durable side effect;
//! 3. wake→cancel race order: unique `Cancelled` terminal, durable wake
//!    kept;
//! 4. cancel→wake race order: unique `Cancelled` terminal, late delivery
//!    buffers, Operation wake reports `NotWaiting`;
//! 5. respawn after cancel fenced by scope cancellation, scope generation
//!    and fiber generation;
//! 6. late channel delivery to a cancelled fiber: buffered without panic,
//!    no false wakeup, state stays `Cancelled`, durable row stays
//!    consumable.
//!
//! Structured join/detach: no such API exists on the `nlos_runtime::
//! RuntimeAdapter` trait or `TokioRuntimeAdapter` (surface is
//! `spawn_fiber`/`cancel_scope`/`inspect`/`activation_usage`); per the
//! ROAD-B-006 test plan the gap is registered in
//! `docs/evidence/stage-b/b-runtime-002-fiber-scale.md` instead of inventing
//! an API.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_channel::{
    ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest, EnqueueDecision,
    EnqueueRequest,
};
use nlos_runtime::{
    FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter, RuntimeError, WakeOutcome,
    WakeSink,
};
use nlos_runtime_tokio::{DeliveryReport, TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, ProcessId, ResourceGroupId, SchedulerDomainId,
};
use nlos_wait::{
    BindingId, NotifyCommitsRequest, RegisterDecision, RegisterWaitRequest, WaitAuthority,
    WaitState, WakeReport,
};
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

fn fiber_spec(index: usize, scope: CancellationScopeId) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: AgentInstanceId::from_bytes(id_bytes(index)),
        agent_generation: Generation::INITIAL,
        process_id: ProcessId::from_bytes(id_bytes(1)),
        process_generation: Generation::INITIAL,
        task_attempt_id: None,
        cancellation_scope_id: scope,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(id_bytes(1)),
        scheduler_domain_id: SchedulerDomainId::from_bytes(id_bytes(1)),
        deadline: None,
    }
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
            "nlos-runtime-tokio-cancel-matrix-{label}-{}-{nonce}-{sequence}",
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

struct Pair {
    channel: Arc<ChannelAuthority>,
    wait: WaitAuthority,
}

fn open_pair(root: &Root) -> Pair {
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait = WaitAuthority::open(root.path(), Arc::clone(&channel)).expect("open wait authority");
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
) -> WakeReport {
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
    TokioRuntimeAdapter::new(Handle::current(), TokioRuntimeConfig { max_live_fibers })
        .expect("runtime")
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

/// The in-body park used by every fiber in this file: registers the durable
/// Channel sequence wait on its own behalf and resolves only on its wake.
/// `pend_after_wake` keeps the fiber alive-but-idle after a delivered wake so
/// a subsequent scope cancel deterministically wins the runtime's biased
/// terminal select (the wake→cancel race-order control).
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

/// Spawns a fiber that parks in-body on `request` and returns its handle.
fn spawn_park(
    runtime: &TokioRuntimeAdapter,
    waits: &Arc<WaitAuthority>,
    request: RegisterWaitRequest,
    index: usize,
    scope: CancellationScopeId,
    pend_after_wake: bool,
) -> FiberHandle {
    let spec = fiber_spec(index, scope);
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

/// Matrix row 1 — durable wait 挂起中取消：given a fiber parked in-body on a
/// durable wait; when its scope is cancelled; then the fiber reaches the
/// unique terminal `Cancelled` state and the durable wait row is left
/// exactly as the landed contract prescribes: still `PENDING` (the runtime
/// side never performs the durable cancel; that is an explicit
/// `WaitAuthority::cancel_wait`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_while_parked_on_durable_wait_yields_cancelled_and_keeps_row_pending() {
    let root = Root::new("park-cancel");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 0xA0);
    let waits = Arc::new(pair.wait);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(310));
    let request = register_request(&channel, 1, 0xB1);

    let handle = spawn_park(&runtime, &waits, request, 1, scope, false);
    wait_for_state(&runtime, handle, FiberState::WaitingIo).await;

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;

    let durable = waits
        .list_waits(Some(channel.channel_id))
        .expect("list waits");
    assert_eq!(durable.len(), 1, "exactly the parked fiber's wait row");
    assert_eq!(
        durable[0].state,
        WaitState::Pending,
        "runtime-side cancellation must leave the durable row PENDING"
    );
}

/// Matrix row 2 — 取消后注册：given a cancelled scope; when
/// `wait_for_channel` is called on the cancelled fiber's behalf; then the
/// wait resolves ready `Cancelled` (never pends) and zero durable side
/// effect occurs — the request idempotency key is still unused afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_registration_after_scope_cancel_resolves_ready_cancelled_with_zero_durable_side_effect()
 {
    let root = Root::new("cancel-then-register");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 0xA0);
    let waits = Arc::new(pair.wait);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(311));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    // The fiber may read Running (scope gate) or already Cancelled (terminal
    // gate); both gates must resolve the registration ready-Cancelled.
    let request = register_request(&channel, 1, 0xB1);
    let wait = runtime
        .wait_for_channel(handle, &waits, request)
        .expect("wait after cancel");
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait).await.expect("resolves"),
        WaitOutcome::Cancelled,
        "registration on a cancelled fiber must not pend"
    );

    assert!(
        waits
            .list_waits(Some(channel.channel_id))
            .expect("list waits")
            .is_empty(),
        "the rejected registration must leave zero durable rows"
    );
    assert!(
        matches!(
            waits.register_wait(request),
            Ok(RegisterDecision::Registered(_))
        ),
        "the idempotency key must still be unused (zero durable side effect)"
    );
}

/// Matrix row 3 — 先唤醒后取消：given a parked fiber whose wake is delivered
/// first; when the scope is then cancelled while the fiber idles post-wake;
/// then the terminal state is uniquely `Cancelled` (the biased terminal
/// select never lets a consumed wake complete a cancelled fiber) and the
/// durable wake fact stays `WOKEN` (durable consistency: the wake happened).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_then_cancel_order_has_unique_cancelled_terminal_with_durable_wake_kept() {
    let root = Root::new("wake-then-cancel");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 0xA0);
    let waits = Arc::new(pair.wait);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(312));
    let request = register_request(&channel, 1, 0xB1);
    let sink = runtime.channel_wait_sink();

    let handle = spawn_park(&runtime, &waits, request, 1, scope, true);
    wait_for_state(&runtime, handle, FiberState::WaitingIo).await;

    enqueue(&pair.channel, channel.channel_id, 0xE0);
    let report = notify(&waits, channel.channel_id, 1, 0xD0);
    assert_eq!(report.woken.len(), 1);
    assert_eq!(
        sink.deliver(&report).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    // The wake was consumed while the fiber was live: back to Running.
    wait_for_state(&runtime, handle, FiberState::Running).await;

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;

    let durable = waits
        .list_waits(Some(channel.channel_id))
        .expect("list waits");
    assert_eq!(durable.len(), 1);
    assert_eq!(
        durable[0].state,
        WaitState::Woken,
        "the delivered wake is a durable fact; cancellation must not undo it"
    );
}

/// Matrix row 4 — 先取消后唤醒：given a parked fiber cancelled first; when
/// the commit notification arrives afterwards and both delivery endpoints
/// fire; then the terminal state stays uniquely `Cancelled`, the channel
/// delivery buffers without panic (the in-memory wait was purged), the
/// Operation wake reports `NotWaiting`, and a fresh Operation registration
/// resolves ready `Cancelled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_then_wake_order_has_unique_cancelled_terminal_and_delivery_buffers() {
    let root = Root::new("cancel-then-wake");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 0xA0);
    let waits = Arc::new(pair.wait);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(313));
    let request = register_request(&channel, 1, 0xB1);
    let channel_sink = runtime.channel_wait_sink();
    let operation_sink = runtime.wake_sink();

    let handle = spawn_park(&runtime, &waits, request, 1, scope, false);
    wait_for_state(&runtime, handle, FiberState::WaitingIo).await;

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;

    // The late commit notification still flips the durable row (the runtime
    // cancel never touched it), and the delivery finds no live in-memory
    // wait — it must buffer, not panic.
    enqueue(&pair.channel, channel.channel_id, 0xE0);
    let report = notify(&waits, channel.channel_id, 1, 0xD0);
    assert_eq!(report.woken.len(), 1);
    assert_eq!(
        channel_sink.deliver(&report).expect("late deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 1
        }
    );
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;

    // The Operation-side endpoint reports the cancelled fiber as not waiting
    // and never wakes it.
    let operation = OperationId::from_bytes(id_bytes(41));
    assert_eq!(
        operation_sink.wake(&handle, operation, Generation::INITIAL),
        Ok(WakeOutcome::NotWaiting)
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
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;
}

/// Matrix row 5 — 取消后重 spawn 的代次守卫：after a scope cancel, every
/// respawn path into the cancelled scope is fenced (`Cancelled` for any new
/// fiber, `InvalidGeneration` for a bumped scope generation), the terminated
/// fiber identity itself stays fenced under a live scope
/// (`DuplicateFiber` same generation, `InvalidGeneration` bumped generation),
/// and a fresh identity in a fresh scope still spawns (the adapter stays
/// usable).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respawn_after_cancel_is_fenced_by_scope_and_fiber_generations() {
    let root = Root::new("respawn-fence");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 0xA0);
    let waits = Arc::new(pair.wait);
    let runtime = runtime(8);
    let scope = CancellationScopeId::from_bytes(id_bytes(314));
    let request = register_request(&channel, 1, 0xB1);

    let handle = spawn_park(&runtime, &waits, request, 1, scope, false);
    wait_for_state(&runtime, handle, FiberState::WaitingIo).await;
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;

    // (a) any new fiber identity into the cancelled scope is rejected.
    assert_eq!(
        runtime.spawn_fiber(fiber_spec(2, scope), Box::pin(pending())),
        Err(RuntimeError::Cancelled),
        "a cancelled scope must not accept new fibers"
    );
    // (b) the same scope id under a bumped cancellation generation is
    // generation-fenced at scope resolution.
    let mut bumped_scope_spec = fiber_spec(2, scope);
    bumped_scope_spec.cancellation_generation = next_generation();
    assert_eq!(
        runtime.spawn_fiber(bumped_scope_spec, Box::pin(pending())),
        Err(RuntimeError::InvalidGeneration),
        "a scope id is bound to exactly one cancellation generation"
    );
    // (c) the terminated fiber identity stays occupied: same generation is a
    // duplicate, a bumped fiber generation is stale.
    let live_scope = CancellationScopeId::from_bytes(id_bytes(315));
    assert_eq!(
        runtime.spawn_fiber(fiber_spec(1, live_scope), Box::pin(pending())),
        Err(RuntimeError::DuplicateFiber),
        "the terminal record still fences its fiber id + generation"
    );
    let mut bumped_fiber_spec = fiber_spec(1, live_scope);
    bumped_fiber_spec.fiber_generation = next_generation();
    assert_eq!(
        runtime.spawn_fiber(bumped_fiber_spec, Box::pin(pending())),
        Err(RuntimeError::InvalidGeneration),
        "a stale fiber generation must never resurrect the identity"
    );
    // (d) control: a fresh identity in a fresh scope still spawns.
    let control = runtime
        .spawn_fiber(fiber_spec(3, live_scope), Box::pin(pending()))
        .expect("fresh identity spawns");
    wait_for_state(&runtime, control, FiberState::Running).await;
    runtime
        .cancel_scope(live_scope, Generation::INITIAL)
        .expect("cleanup control fiber");
}

/// Late-callback — 晚到 channel 投递不复活已取消 fiber：given a fiber
/// cancelled while parked on its durable wait; when the commit notification
/// afterwards flips the untouched `PENDING` row and the consumer delivers
/// it; then the delivery buffers without panic, the fiber stays `Cancelled`
/// (no false wakeup), rearm arms nothing for the cancelled fiber, and the
/// durable wake fact remains consumable by a later legitimate waiter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_delivery_to_cancelled_fiber_buffers_without_wakeup_and_stays_consumable() {
    let root = Root::new("late-callback");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 0xA0);
    let waits = Arc::new(pair.wait);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(316));
    let request = register_request(&channel, 1, 0xB1);
    let sink = runtime.channel_wait_sink();

    let handle = spawn_park(&runtime, &waits, request, 1, scope, false);
    wait_for_state(&runtime, handle, FiberState::WaitingIo).await;
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;

    // Late commit: the row was still PENDING, so the notification flips it —
    // the durable fact outlives the cancelled runtime wait.
    enqueue(&pair.channel, channel.channel_id, 0xE0);
    let report = notify(&waits, channel.channel_id, 1, 0xD0);
    assert_eq!(report.woken.len(), 1);
    assert_eq!(
        sink.deliver(&report).expect("late deliver must not panic"),
        DeliveryReport {
            delivered: 1,
            buffered: 1
        }
    );
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;

    // The cancelled fiber re-arms nothing: the buffered placeholder must not
    // be resurrected into a live wait for a terminal fiber.
    let rearm = runtime
        .rearm_channel_waits(handle, &waits, None)
        .expect("rearm on cancelled fiber");
    assert!(
        rearm.satisfied.is_empty() && rearm.pending.is_empty(),
        "a cancelled fiber must arm nothing"
    );
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;

    // The durable wake fact stays consumable: a fresh waiter replaying the
    // same request resolves immediately Woken (at-least-once), in a fresh
    // scope — the cancelled fiber itself is never involved.
    let replay_scope = CancellationScopeId::from_bytes(id_bytes(317));
    let replay_handle = runtime
        .spawn_fiber(fiber_spec(9, replay_scope), Box::pin(pending()))
        .expect("spawn fresh waiter");
    let replay_wait = runtime
        .wait_for_channel(replay_handle, &waits, request)
        .expect("replay wait");
    assert_eq!(
        tokio::time::timeout(RESOLVE, replay_wait)
            .await
            .expect("replay resolves"),
        WaitOutcome::Woken,
        "the WOKEN durable row must stay consumable after the late delivery"
    );
    await_settle_and_assert_state(&runtime, handle, FiberState::Cancelled).await;
    runtime
        .cancel_scope(replay_scope, Generation::INITIAL)
        .expect("cleanup replay fiber");
}

/// Settles the executor and then asserts the state is still `expected` —
/// the bounded inverse of `wait_for_state`, used to prove no late callback
/// resurrects or retargets a terminal fiber.
async fn await_settle_and_assert_state(
    runtime: &TokioRuntimeAdapter,
    handle: FiberHandle,
    expected: FiberState,
) {
    settle().await;
    assert_eq!(
        runtime.inspect(handle),
        Ok(expected),
        "terminal state must be stable across late callbacks"
    );
}
