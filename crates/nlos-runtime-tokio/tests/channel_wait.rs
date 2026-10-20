//! End-to-end tests for the runtime-tokio Channel sequence wait loop:
//! `TokioRuntimeAdapter::wait_for_channel` (durable registration through the
//! wait authority, high-water self-flip, in-memory pending) and
//! `TokioChannelWakeSink::deliver` (report-driven wake handshake), including
//! the cancellation split — runtime cancellation never touches the durable
//! wait row.
//!
//! Harness and fixtures follow `nlos-wait/tests/wait_registry.rs` (temp
//! `Root`, channel/wait authority pair) and the existing
//! `nlos-runtime-tokio` wake tests (multi-thread runtime, bounded pending
//! probe, generous resolve window).

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_channel::{
    ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest, EnqueueDecision,
    EnqueueRequest,
};
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter, RuntimeError};
use nlos_runtime_tokio::{
    ChannelWaitError, DeliveryReport, TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey, ProcessId,
    ResourceGroupId, SchedulerDomainId,
};
use nlos_wait::{
    BindingId, CancelWaitRequest, NotifyCommitsRequest, RegisterDecision, RegisterWaitRequest,
    WaitAuthority, WaitRecord, WaitState, WakeReport,
};
use tokio::runtime::Handle;

/// Bounded observation window for "still pending" assertions.
const PENDING_PROBE: Duration = Duration::from_millis(100);
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
            "nlos-runtime-tokio-channel-wait-{label}-{}-{nonce}-{sequence}",
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

/// Registers the durable wait directly (bypassing the runtime adapter).
fn register(authority: &WaitAuthority, request: RegisterWaitRequest) -> WaitRecord {
    match authority.register_wait(request).expect("register wait") {
        RegisterDecision::Registered(record) => record,
        RegisterDecision::Replayed(_) => panic!("fresh register cannot replay"),
    }
}

fn runtime() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: 4,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime")
}

/// Spawns a fiber that pends forever; returns its handle and scope for
/// cleanup.
fn spawn_waiter(runtime: &TokioRuntimeAdapter, index: usize) -> (FiberHandle, CancellationScopeId) {
    let scope = CancellationScopeId::from_bytes(id_bytes(300 + index));
    let handle = runtime
        .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
        .expect("spawn");
    (handle, scope)
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

/// Given/When/Then: given a registered durable wait and a live runtime wait;
/// when the channel commits the target sequence and the consumer delivers
/// the notification report; then the wait resolves `Woken`, the delivery is
/// reported as handed off, and the durable row is `WOKEN`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_loop_register_await_notify_deliver_resolves_woken() {
    let root = Root::new("happy");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);
    let sink = runtime.channel_wait_sink();

    let request = register_request(&channel, 1, 1);
    let mut wait = runtime
        .wait_for_channel(handle, &pair.wait, request)
        .expect("wait");
    // The wait future is registered and suspended before any commit.
    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut wait)
            .await
            .is_err(),
        "the wait must stay pending before its sequence commits"
    );

    assert_eq!(enqueue(&pair.channel, channel.channel_id, 50), 1);
    let report = notify(&pair.wait, channel.channel_id, 1, 30);
    assert_eq!(report.woken.len(), 1);
    assert_eq!(
        sink.deliver(&report).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );

    assert_eq!(
        tokio::time::timeout(RESOLVE, wait).await.expect("resolves"),
        WaitOutcome::Woken
    );
    assert_eq!(
        pair.wait
            .inspect_wait(report.woken[0].wait_id)
            .expect("inspect durable row")
            .state,
        WaitState::Woken
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a notification that was committed and delivered
/// before any runtime registration; when `wait_for_channel` replays the
/// already-`WOKEN` durable row; then the wait resolves immediately as
/// `Woken` (at-least-once: the durable wake already happened).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_notify_and_deliver_before_registration_resolves_immediately() {
    let root = Root::new("early");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);
    let sink = runtime.channel_wait_sink();

    let request = register_request(&channel, 1, 1);
    let durable = register(&pair.wait, request);
    assert_eq!(durable.state, WaitState::Pending);

    enqueue(&pair.channel, channel.channel_id, 51);
    let report = notify(&pair.wait, channel.channel_id, 1, 30);
    assert_eq!(report.woken.len(), 1);
    // No registration exists yet: the wake buffers for the future waiter.
    assert_eq!(
        sink.deliver(&report).expect("early deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 1
        }
    );

    let wait = runtime
        .wait_for_channel(handle, &pair.wait, request)
        .expect("wait");
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("replayed wake resolves immediately"),
        WaitOutcome::Woken
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a `PENDING` durable wait whose channel high-water
/// already covers the target (entries committed, notification never sent);
/// when `wait_for_channel` observes the coverage; then it self-flips the row
/// through an explicit notify under a fresh key and resolves immediately as
/// `Woken`, the durable row is `WOKEN`, and a later independent notify flips
/// nothing (idempotent explicit-notify model, no polling).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_with_satisfied_high_water_self_flips_to_woken() {
    let root = Root::new("high-water");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);

    let request = register_request(&channel, 1, 1);
    let durable = register(&pair.wait, request);
    assert_eq!(durable.state, WaitState::Pending);

    assert_eq!(enqueue(&pair.channel, channel.channel_id, 52), 1);
    let wait = runtime
        .wait_for_channel(handle, &pair.wait, request)
        .expect("wait");
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("covered wait resolves immediately"),
        WaitOutcome::Woken
    );
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect flipped row")
            .state,
        WaitState::Woken
    );
    // A distinct notify key over the same range flips nothing: the row is
    // terminal, so the self-flip notification stays idempotent.
    assert_eq!(
        notify(&pair.wait, channel.channel_id, 1, 31).woken,
        [] as [nlos_wait::WaitRecord; 0]
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a live runtime wait on a `PENDING` durable row;
/// when the runtime cancels the fiber's scope; then the wait resolves
/// `Cancelled` while the durable row stays `PENDING` — runtime cancellation
/// never performs the durable cancel, which is an explicit `cancel_wait`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_cancellation_cancels_wait_but_keeps_durable_row_pending() {
    let root = Root::new("runtime-cancel");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);

    let request = register_request(&channel, 1, 1);
    let durable = register(&pair.wait, request);
    let wait = runtime
        .wait_for_channel(handle, &pair.wait, request)
        .expect("wait");

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait).await.expect("resolves"),
        WaitOutcome::Cancelled
    );
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect after runtime cancel")
            .state,
        WaitState::Pending
    );
}

/// Given/When/Then: given a registered channel wait on a fiber that then
/// terminates; when the fiber lifecycle purge runs; then the wait resolves
/// `Cancelled` (the purge covers the channel wait registry) and the durable
/// row stays `PENDING`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fiber_termination_purges_channel_waits_as_cancelled() {
    let root = Root::new("purge");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let runtime = runtime();
    let scope = CancellationScopeId::from_bytes(id_bytes(301));
    // The fiber lingers briefly so the registration lands while it is live.
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                FiberExit::Completed
            }),
        )
        .expect("spawn");
    wait_for_state(&runtime, handle, FiberState::Running).await;

    let request = register_request(&channel, 1, 1);
    let durable = register(&pair.wait, request);
    let wait = runtime
        .wait_for_channel(handle, &pair.wait, request)
        .expect("wait");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("purged wait must resolve"),
        WaitOutcome::Cancelled
    );
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect after purge")
            .state,
        WaitState::Pending
    );
}

/// Given/When/Then: given a live channel wait; when the runtime shuts down;
/// then the pending wait resolves `Cancelled`, a subsequent
/// `wait_for_channel` fails with `ShuttingDown`, and `deliver` fails with
/// `ShuttingDown`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cancels_channel_waits_and_rejects_further_waits_and_deliveries() {
    let root = Root::new("shutdown");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);
    let sink = runtime.channel_wait_sink();

    let request = register_request(&channel, 1, 1);
    register(&pair.wait, request);
    let wait = runtime
        .wait_for_channel(handle, &pair.wait, request)
        .expect("wait");

    runtime.shutdown();
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("shutdown must resolve pending waits"),
        WaitOutcome::Cancelled
    );
    assert!(matches!(
        runtime.wait_for_channel(handle, &pair.wait, request),
        Err(ChannelWaitError::Runtime(RuntimeError::ShuttingDown))
    ));
    assert_eq!(
        sink.deliver(&WakeReport { woken: Vec::new() }),
        Err(RuntimeError::ShuttingDown)
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a delivered notification; when the same report is
/// delivered again; then the second delivery stays successful, reports the
/// buffered (already-consumed) outcome, and the wait still resolved exactly
/// once as `Woken`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_delivery_is_idempotent_and_still_succeeds() {
    let root = Root::new("duplicate-delivery");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);
    let sink = runtime.channel_wait_sink();

    let request = register_request(&channel, 1, 1);
    let wait = runtime
        .wait_for_channel(handle, &pair.wait, request)
        .expect("wait");
    enqueue(&pair.channel, channel.channel_id, 53);
    let report = notify(&pair.wait, channel.channel_id, 1, 30);
    assert_eq!(report.woken.len(), 1);

    assert_eq!(
        sink.deliver(&report).expect("first deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait).await.expect("resolves"),
        WaitOutcome::Woken
    );
    // At-least-once redelivery of the same report: still successful, but the
    // already-consumed wait is not handed off again.
    assert_eq!(
        sink.deliver(&report).expect("repeat deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 1
        }
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a durable `WOKEN` wait row from a previous process
/// run; when all handles drop, the authorities reopen on the same root and
/// the same request is presented to `wait_for_channel`; then the row replays
/// as `WOKEN` and the wait resolves immediately as `Woken`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_replays_woken_durable_row_as_immediate_wake() {
    let root = Root::new("restart");
    let request = {
        let pair = open_pair(&root);
        let channel = create_channel(&pair.channel, 200);
        let request = register_request(&channel, 1, 1);
        let durable = register(&pair.wait, request);
        enqueue(&pair.channel, channel.channel_id, 54);
        assert_eq!(notify(&pair.wait, channel.channel_id, 1, 30).woken.len(), 1);
        assert_eq!(
            pair.wait
                .inspect_wait(durable.wait_id)
                .expect("inspect before restart")
                .state,
            WaitState::Woken
        );
        request
    };

    let pair = open_pair(&root);
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);
    let wait = runtime
        .wait_for_channel(handle, &pair.wait, request)
        .expect("wait");
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("replayed wake resolves immediately"),
        WaitOutcome::Woken
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given stale and unknown fiber handles; when
/// `wait_for_channel` is called; then both fail with `InvalidGeneration` and
/// leave zero durable state — the request key still registers fresh
/// afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_or_unknown_fiber_handle_fails_invalid_generation() {
    let root = Root::new("stale-handle");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);

    let request = register_request(&channel, 1, 1);
    let stale = FiberHandle {
        fiber_id: handle.fiber_id,
        generation: handle.generation.checked_next().expect("next generation"),
    };
    assert!(matches!(
        runtime.wait_for_channel(stale, &pair.wait, request),
        Err(ChannelWaitError::Runtime(RuntimeError::InvalidGeneration))
    ));
    let unknown = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(999)),
        generation: Generation::INITIAL,
    };
    assert!(matches!(
        runtime.wait_for_channel(unknown, &pair.wait, request),
        Err(ChannelWaitError::Runtime(RuntimeError::InvalidGeneration))
    ));

    // The rejected registrations left zero durable state.
    assert!(matches!(
        pair.wait.register_wait(request),
        Ok(RegisterDecision::Registered(_))
    ));
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Regression (self-flip vs concurrent durable cancel, registration side).
///
/// `notify_commits` succeeds even when its flip set is empty, so an explicit
/// `cancel_wait` landing between the runtime's pending read and its
/// high-water self-flip used to resolve the wait `Woken` while the durable
/// row ended `CANCELLED`. The vulnerable window sits between two authority
/// calls inside one registration and cannot be paused deterministically, so
/// each round races one cancellation against one registration with a swept
/// release delay; the oracle holds on EVERY interleaving, so the test never
/// fails spuriously: the resolved outcome must agree with the row's durable
/// terminal state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_flip_racing_cancel_agrees_with_durable_state() {
    const ROUNDS: usize = 40;
    let root = Root::new("self-flip-cancel-race");
    let channel_authority =
        Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait_authority = Arc::new(
        WaitAuthority::open(root.path(), Arc::clone(&channel_authority))
            .expect("open wait authority"),
    );
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);

    for round in 0..ROUNDS {
        let seed = 10 + round;
        let channel = create_channel(
            &channel_authority,
            u8::try_from(seed).expect("channel seed"),
        );
        // One committed entry covers target 1, so the registration takes the
        // high-water self-flip path.
        enqueue(
            &channel_authority,
            channel.channel_id,
            u8::try_from(seed + 50).expect("enqueue seed"),
        );
        let request = register_request(&channel, 1, u8::try_from(seed + 100).expect("seed"));
        let row = register(&wait_authority, request);
        assert_eq!(row.state, WaitState::Pending);

        let go = Arc::new(AtomicBool::new(false));
        let go_canceller = Arc::clone(&go);
        let canceller_authority = Arc::clone(&wait_authority);
        let cancelled_id = row.wait_id;
        let cancel_seed = u8::try_from(seed + 150).expect("cancel seed");
        let canceller = std::thread::spawn(move || {
            while !go_canceller.load(Ordering::Relaxed) {
                std::hint::spin_loop();
            }
            // Swept delay (0 .. ~1ms across the rounds): some round lands
            // the cancellation inside the registration's window between its
            // pending read and its self-flip, whatever the machine speed.
            std::thread::sleep(Duration::from_micros(
                u64::try_from(round).expect("round") * 25,
            ));
            // An error here only means the self-flip won the row first
            // (`WaitNotPending`); the oracle below checks the state.
            let _ = canceller_authority.cancel_wait(CancelWaitRequest {
                wait_id: cancelled_id,
                cancelled_at_ms: 3_000,
                idempotency_key: key(cancel_seed),
            });
        });
        go.store(true, Ordering::Relaxed);

        let wait = runtime
            .wait_for_channel(handle, &wait_authority, request)
            .expect("registration succeeds");
        let outcome = tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("the registered wait resolves on every interleaving");
        canceller.join().expect("canceller thread");
        let final_state = wait_authority
            .inspect_wait(row.wait_id)
            .expect("final row read")
            .state;
        match final_state {
            WaitState::Woken => assert_eq!(
                outcome,
                WaitOutcome::Woken,
                "round {round}: a WOKEN row must resolve Woken"
            ),
            WaitState::Cancelled => assert_eq!(
                outcome,
                WaitOutcome::Cancelled,
                "round {round}: a CANCELLED row must resolve Cancelled, never Woken"
            ),
            WaitState::Pending => {
                panic!("round {round}: the row must be terminal after the race")
            }
        }
    }

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Regression (self-flip vs concurrent durable cancel, rearm side).
///
/// The same race through `rearm_channel_waits`: `arm_durable_row` re-read
/// the post-flip row but did not branch on its state, so a racing
/// cancellation winner was reported `satisfied` carrying a `Woken` future
/// over a `CANCELLED` row. Invariants on every interleaving: every
/// `satisfied` row is durably `WOKEN` and resolves `Woken`, and a row that
/// ended `CANCELLED` is never armed at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rearm_self_flip_racing_cancel_agrees_with_durable_state() {
    const ROUNDS: usize = 40;
    let root = Root::new("rearm-self-flip-cancel-race");
    let channel_authority =
        Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait_authority = Arc::new(
        WaitAuthority::open(root.path(), Arc::clone(&channel_authority))
            .expect("open wait authority"),
    );
    let runtime = runtime();
    let (handle, scope) = spawn_waiter(&runtime, 1);

    for round in 0..ROUNDS {
        let seed = 10 + round;
        let channel = create_channel(
            &channel_authority,
            u8::try_from(seed).expect("channel seed"),
        );
        enqueue(
            &channel_authority,
            channel.channel_id,
            u8::try_from(seed + 50).expect("enqueue seed"),
        );
        let request = register_request(&channel, 1, u8::try_from(seed + 100).expect("seed"));
        let row = register(&wait_authority, request);
        assert_eq!(row.state, WaitState::Pending);

        let go = Arc::new(AtomicBool::new(false));
        let go_canceller = Arc::clone(&go);
        let canceller_authority = Arc::clone(&wait_authority);
        let cancelled_id = row.wait_id;
        let cancel_seed = u8::try_from(seed + 150).expect("cancel seed");
        let canceller = std::thread::spawn(move || {
            while !go_canceller.load(Ordering::Relaxed) {
                std::hint::spin_loop();
            }
            std::thread::sleep(Duration::from_micros(
                u64::try_from(round).expect("round") * 25,
            ));
            let _ = canceller_authority.cancel_wait(CancelWaitRequest {
                wait_id: cancelled_id,
                cancelled_at_ms: 3_000,
                idempotency_key: key(cancel_seed),
            });
        });
        go.store(true, Ordering::Relaxed);

        let mut report = runtime
            .rearm_channel_waits(handle, &wait_authority, Some(channel.channel_id))
            .expect("rearm succeeds");
        canceller.join().expect("canceller thread");
        let final_state = wait_authority
            .inspect_wait(row.wait_id)
            .expect("final row read")
            .state;
        let mut armed_ids = Vec::new();
        for mut armed in report.satisfied.drain(..) {
            assert_eq!(
                armed.record.state,
                WaitState::Woken,
                "round {round}: only a WOKEN row may be reported satisfied"
            );
            armed_ids.push(armed.record.wait_id);
            assert_eq!(
                tokio::time::timeout(RESOLVE, &mut armed.wait)
                    .await
                    .expect("a satisfied rearm resolves immediately"),
                WaitOutcome::Woken,
                "round {round}: a satisfied rearm must resolve Woken"
            );
        }
        armed_ids.extend(report.pending.iter().map(|armed| armed.record.wait_id));
        match final_state {
            WaitState::Woken => assert!(
                armed_ids.contains(&row.wait_id),
                "round {round}: a WOKEN row must be armed by the rearm"
            ),
            WaitState::Cancelled => assert!(
                !armed_ids.contains(&row.wait_id),
                "round {round}: a CANCELLED row must never be armed, satisfied or not"
            ),
            WaitState::Pending => {
                panic!("round {round}: the row must be terminal after the race")
            }
        }
    }

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}
