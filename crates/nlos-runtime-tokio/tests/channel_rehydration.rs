//! End-to-end tests for wait-side rehydration:
//! `TokioRuntimeAdapter::rearm_channel_waits` rebuilds the in-memory half of
//! durable Channel waits after a restart — `PENDING` rows re-arm as live
//! waits, high-water-covered rows self-flip satisfied, already-`WOKEN` rows
//! resolve immediately and consume early-buffered placeholder wakes,
//! `CANCELLED` rows are never re-armed, and the durable rows stay read-only
//! to rearm except the self-flip. Harness follows
//! `tests/channel_wait.rs` (temp `Root`, channel/wait authority pair,
//! multi-thread runtime, bounded pending probe, generous resolve window).

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_channel::{
    ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest, EnqueueDecision,
    EnqueueRequest,
};
use nlos_runtime::{FiberHandle, FiberSpec, RuntimeAdapter, RuntimeError};
use nlos_runtime_tokio::{
    ChannelSequenceWait, ChannelWaitError, DeliveryReport, RearmedChannelWait, TokioRuntimeAdapter,
    TokioRuntimeConfig, WaitOutcome,
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

fn fiber_spec(index: usize, scope: CancellationScopeId, generation: Generation) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        fiber_generation: generation,
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
            "nlos-runtime-tokio-channel-rehydration-{label}-{}-{nonce}-{sequence}",
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
    TokioRuntimeAdapter::new(Handle::current(), TokioRuntimeConfig { max_live_fibers: 4 })
        .expect("runtime")
}

/// Spawns a fiber on `index`'s identity that pends forever; returns its
/// handle and scope id.
fn spawn_waiter(
    runtime: &TokioRuntimeAdapter,
    index: usize,
    generation: Generation,
) -> (FiberHandle, CancellationScopeId) {
    let scope = CancellationScopeId::from_bytes(id_bytes(300 + index));
    let handle = runtime
        .spawn_fiber(fiber_spec(index, scope, generation), Box::pin(pending()))
        .expect("spawn");
    (handle, scope)
}

/// Asserts the re-armed wait stays pending inside the observation window.
async fn assert_stays_pending(wait: &mut ChannelSequenceWait) {
    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut *wait)
            .await
            .is_err(),
        "the re-armed wait must stay pending before its wake arrives"
    );
}

/// Given/When/Then: given a `PENDING` durable wait registered by a previous
/// process incarnation; when the process "restarts" (all runtime handles
/// drop, the authorities reopen, a new fiber incarnation is spawned) and the
/// supervisor calls `rearm_channel_waits`; then the wait is re-armed
/// pending, a later commit notification delivered through the new runtime's
/// sink wakes it, and the durable row ends `WOKEN`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_rearm_reshoots_pending_wait_through_deliver() {
    let root = Root::new("restart-pending");
    let (request, durable, channel_id) = {
        let pair = open_pair(&root);
        let channel = create_channel(&pair.channel, 200);
        let adapter = runtime();
        let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);
        let request = register_request(&channel, 1, 1);
        let durable = register(&pair.wait, request);
        let mut live = adapter
            .wait_for_channel(handle, &pair.wait, request)
            .expect("pre-restart wait");
        assert_stays_pending(&mut live).await;
        adapter
            .cancel_scope(scope, Generation::INITIAL)
            .expect("cancel");
        (request, durable, channel.channel_id)
    };

    // The "restart": fresh authorities on the same root, fresh adapter, and
    // a new fiber incarnation (same identity, next generation).
    let pair = open_pair(&root);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(
        &adapter,
        1,
        Generation::INITIAL.checked_next().expect("next"),
    );
    let mut report = adapter
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("rearm");
    assert!(report.satisfied.is_empty());
    assert_eq!(report.pending.len(), 1);
    let RearmedChannelWait {
        record: armed_record,
        wait: mut armed_wait,
    } = report.pending.remove(0);
    assert_eq!(armed_record.wait_id, durable.wait_id);
    assert_eq!(armed_record.channel_id, channel_id);
    assert_eq!(armed_record.target_sequence, request.target_sequence);
    assert_eq!(armed_record.state, WaitState::Pending);
    assert_stays_pending(&mut armed_wait).await;

    assert_eq!(enqueue(&pair.channel, channel_id, 50), 1);
    let wake = notify(&pair.wait, channel_id, 1, 30);
    assert_eq!(wake.woken.len(), 1);
    // The re-armed wait is found by the delivery: handed off, not buffered.
    assert_eq!(
        adapter.channel_wait_sink().deliver(&wake).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, armed_wait)
            .await
            .expect("resolves"),
        WaitOutcome::Woken
    );
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect durable row")
            .state,
        WaitState::Woken
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a `PENDING` durable wait whose channel high-water
/// already covers the target (entries committed, notification never sent);
/// when rearm observes the coverage; then the row is self-flipped through
/// the domain-reserved notify transform, reported satisfied, the future
/// resolves immediately `Woken`, and a later independent notify flips
/// nothing (the self-flip is the only durable write rearm performs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rearm_self_flips_high_water_covered_wait_satisfied() {
    let root = Root::new("rearm-high-water");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let durable = register(&pair.wait, register_request(&channel, 1, 1));
    assert_eq!(enqueue(&pair.channel, channel.channel_id, 52), 1);

    let mut report = adapter
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("rearm");
    assert!(report.pending.is_empty());
    assert_eq!(report.satisfied.len(), 1);
    let RearmedChannelWait { record, wait } = report.satisfied.remove(0);
    // The report carries the authoritative post-flip row.
    assert_eq!(record.wait_id, durable.wait_id);
    assert_eq!(record.state, WaitState::Woken);
    assert!(record.woken_at_ms > 0);
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait).await.expect("resolves"),
        WaitOutcome::Woken
    );
    let flipped = pair
        .wait
        .inspect_wait(durable.wait_id)
        .expect("inspect flipped row");
    assert_eq!(flipped.state, WaitState::Woken);
    assert_eq!(flipped.woken_up_to_sequence, 1);
    // A distinct notify key over the same range flips nothing: the row is
    // terminal, so the self-flip notification stays idempotent.
    assert!(
        notify(&pair.wait, channel.channel_id, 1, 31)
            .woken
            .is_empty()
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a notification that was committed and delivered
/// before any registration (the runtime buffered the placeholder wake) and
/// whose durable row is therefore already `WOKEN`; when rearm enumerates the
/// rows; then the same `wait_id` consumes the placeholder and resolves
/// immediately `Woken` with no further delivery — at-least-once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rearm_consumes_placeholder_buffered_early_wake() {
    let root = Root::new("rearm-placeholder");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);
    let sink = adapter.channel_wait_sink();

    let durable = register(&pair.wait, register_request(&channel, 1, 1));
    assert_eq!(enqueue(&pair.channel, channel.channel_id, 51), 1);
    let wake = notify(&pair.wait, channel.channel_id, 1, 30);
    assert_eq!(wake.woken.len(), 1);
    // No registration exists yet: the wake buffers under the placeholder key.
    assert_eq!(
        sink.deliver(&wake).expect("early deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 1
        }
    );

    let mut report = adapter
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("rearm");
    assert!(report.pending.is_empty());
    assert_eq!(report.satisfied.len(), 1);
    let RearmedChannelWait { record, wait } = report.satisfied.remove(0);
    assert_eq!(record.wait_id, durable.wait_id);
    assert_eq!(record.state, WaitState::Woken);
    // The buffered placeholder is consumed: the rehydrated wait resolves
    // immediately without any further delivery.
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait).await.expect("resolves"),
        WaitOutcome::Woken
    );
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect durable row")
            .state,
        WaitState::Woken
    );
    // At-least-once redelivery of the same report stays successful.
    assert_eq!(sink.deliver(&wake).expect("repeat deliver").delivered, 1);
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given `PENDING` durable waits on two different channels;
/// when rearm runs with a channel filter; then only the matching channel's
/// wait is re-armed and the other channel's wait is left fully untouched —
/// a second filtered rearm still finds and arms it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_rearms_only_matching_channel() {
    let root = Root::new("rearm-filter");
    let pair = open_pair(&root);
    let channel_a = create_channel(&pair.channel, 200);
    let channel_b = create_channel(&pair.channel, 201);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let wait_a = register(&pair.wait, register_request(&channel_a, 1, 1));
    let wait_b = register(&pair.wait, register_request(&channel_b, 1, 2));

    let mut report = adapter
        .rearm_channel_waits(handle, &pair.wait, Some(channel_a.channel_id))
        .expect("filtered rearm");
    assert!(report.satisfied.is_empty());
    assert_eq!(report.pending.len(), 1);
    assert_eq!(report.pending.remove(0).record.wait_id, wait_a.wait_id);
    // Channel B was untouched by the first rearm: its row is still `PENDING`
    // and a second filtered rearm still finds and arms it.
    assert_eq!(
        pair.wait
            .inspect_wait(wait_b.wait_id)
            .expect("inspect channel-B row")
            .state,
        WaitState::Pending
    );
    let mut second = adapter
        .rearm_channel_waits(handle, &pair.wait, Some(channel_b.channel_id))
        .expect("second filtered rearm");
    assert!(second.satisfied.is_empty());
    assert_eq!(second.pending.len(), 1);
    let RearmedChannelWait {
        record: armed_b,
        wait: mut armed_b_wait,
    } = second.pending.remove(0);
    assert_eq!(armed_b.wait_id, wait_b.wait_id);
    assert_stays_pending(&mut armed_b_wait).await;

    // The enumeration helper itself respects the filter: only channel B's
    // row is listed under the `Some(B)` filter.
    let listed = pair
        .wait
        .list_waits(Some(channel_b.channel_id))
        .expect("list channel B waits");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].wait_id, wait_b.wait_id);

    // And the re-armed channel-B wait is woken by the normal deliver path.
    assert_eq!(enqueue(&pair.channel, channel_b.channel_id, 53), 1);
    let wake = notify(&pair.wait, channel_b.channel_id, 1, 32);
    assert_eq!(wake.woken.len(), 1);
    assert_eq!(
        adapter.channel_wait_sink().deliver(&wake).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, armed_b_wait)
            .await
            .expect("resolves"),
        WaitOutcome::Woken
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given one `WOKEN`, one `PENDING` and one `CANCELLED`
/// durable wait on the same channel; when rearm runs; then the `WOKEN` row
/// is reported satisfied, the `PENDING` row is re-armed, and the `CANCELLED`
/// row is never re-armed nor reported (durable states are unchanged).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_durable_states_only_pending_and_woken_rearm() {
    let root = Root::new("rearm-mixed");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let woken_row = register(&pair.wait, register_request(&channel, 1, 1));
    let pending_row = register(&pair.wait, register_request(&channel, 2, 2));
    let cancelled_row = register(&pair.wait, register_request(&channel, 3, 3));
    // Nothing is enqueued yet (high-water 0), so a notify over sequence 1
    // flips only the target-1 row.
    assert_eq!(notify(&pair.wait, channel.channel_id, 1, 30).woken.len(), 1);
    pair.wait
        .cancel_wait(CancelWaitRequest {
            wait_id: cancelled_row.wait_id,
            cancelled_at_ms: 2_500,
            idempotency_key: key(90),
        })
        .expect("cancel third wait");

    let mut report = adapter
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("rearm");
    assert_eq!(report.satisfied.len(), 1);
    assert_eq!(report.pending.len(), 1);
    let RearmedChannelWait { record, wait } = report.satisfied.remove(0);
    assert_eq!(record.wait_id, woken_row.wait_id);
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait).await.expect("resolves"),
        WaitOutcome::Woken
    );
    let RearmedChannelWait {
        record: armed_record,
        wait: mut armed_wait,
    } = report.pending.remove(0);
    assert_eq!(armed_record.wait_id, pending_row.wait_id);
    assert_stays_pending(&mut armed_wait).await;

    // Durable states are exactly as the rearm found (or flipped) them; the
    // cancelled row was never resurrected.
    assert_eq!(
        pair.wait
            .inspect_wait(woken_row.wait_id)
            .expect("inspect woken row")
            .state,
        WaitState::Woken
    );
    assert_eq!(
        pair.wait
            .inspect_wait(pending_row.wait_id)
            .expect("inspect pending row")
            .state,
        WaitState::Pending
    );
    assert_eq!(
        pair.wait
            .inspect_wait(cancelled_row.wait_id)
            .expect("inspect cancelled row")
            .state,
        WaitState::Cancelled
    );

    // The re-armed `PENDING` wait still wakes through the deliver path once
    // its target commits.
    assert_eq!(enqueue(&pair.channel, channel.channel_id, 54), 1);
    assert_eq!(enqueue(&pair.channel, channel.channel_id, 55), 2);
    let wake = notify(&pair.wait, channel.channel_id, 2, 31);
    assert_eq!(wake.woken.len(), 1);
    assert_eq!(
        adapter.channel_wait_sink().deliver(&wake).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, armed_wait)
            .await
            .expect("resolves"),
        WaitOutcome::Woken
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a re-armed `PENDING` wait; when the runtime cancels
/// the fiber's cancellation scope; then the re-armed wait resolves
/// `Cancelled` while its durable row stays `PENDING` — the cancellation
/// split contract carries over unchanged through rehydration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rearmed_wait_scope_cancel_keeps_durable_row_pending() {
    let root = Root::new("rearm-scope-cancel");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let durable = register(&pair.wait, register_request(&channel, 5, 1));
    let mut report = adapter
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("rearm");
    assert!(report.satisfied.is_empty());
    assert_eq!(report.pending.len(), 1);
    let mut armed = report.pending.remove(0).wait;
    assert_stays_pending(&mut armed).await;

    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    assert_eq!(
        tokio::time::timeout(RESOLVE, armed)
            .await
            .expect("resolves"),
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

/// Given/When/Then: given the same `PENDING` durable wait re-armed twice on
/// the same fiber; when the second rearm registers the same wait key; then
/// the first armed wait is superseded (resolves `Cancelled`), the second
/// stays live and is woken by the normal deliver path, and the durable row
/// stayed `PENDING` throughout (rearm's zero-durable-write contract for
/// uncovered waits).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_rearm_supersedes_first_armed_wait() {
    let root = Root::new("rearm-supersede");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let durable = register(&pair.wait, register_request(&channel, 5, 1));
    let mut first = adapter
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("first rearm")
        .pending
        .remove(0)
        .wait;
    assert_stays_pending(&mut first).await;

    let mut second_report = adapter
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("second rearm");
    assert!(second_report.satisfied.is_empty());
    assert_eq!(second_report.pending.len(), 1);
    let mut second = second_report.pending.remove(0).wait;
    // Same-key substitution: the first armed wait resolves `Cancelled`.
    assert_eq!(
        tokio::time::timeout(RESOLVE, first)
            .await
            .expect("resolves"),
        WaitOutcome::Cancelled
    );
    assert_stays_pending(&mut second).await;
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect after rearm")
            .state,
        WaitState::Pending
    );

    // The surviving armed wait still wakes through the deliver path.
    for seed in 60..65 {
        enqueue(&pair.channel, channel.channel_id, seed);
    }
    let wake = notify(&pair.wait, channel.channel_id, 5, 33);
    assert_eq!(wake.woken.len(), 1);
    assert_eq!(
        adapter.channel_wait_sink().deliver(&wake).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, second)
            .await
            .expect("resolves"),
        WaitOutcome::Woken
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given stale and unknown fiber handles and a shut-down
/// runtime; when rearm is called; then each fails with the mirrored
/// `wait_for_channel` error and leaves the durable rows byte-for-byte
/// unchanged (fail-closed error paths perform zero durable side effects).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_handle_and_shutdown_fail_without_durable_side_effects() {
    let root = Root::new("rearm-errors");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);
    let durable = register(&pair.wait, register_request(&channel, 1, 1));

    let stale = FiberHandle {
        fiber_id: handle.fiber_id,
        generation: handle.generation.checked_next().expect("next generation"),
    };
    assert!(matches!(
        adapter.rearm_channel_waits(stale, &pair.wait, None),
        Err(ChannelWaitError::Runtime(RuntimeError::InvalidGeneration))
    ));
    let unknown = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(999)),
        generation: Generation::INITIAL,
    };
    assert!(matches!(
        adapter.rearm_channel_waits(unknown, &pair.wait, None),
        Err(ChannelWaitError::Runtime(RuntimeError::InvalidGeneration))
    ));

    // The rejected rearms left the durable enumeration unchanged.
    let listed = pair.wait.list_waits(None).expect("list waits");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].wait_id, durable.wait_id);
    assert_eq!(listed[0].state, WaitState::Pending);

    adapter.shutdown();
    assert!(matches!(
        adapter.rearm_channel_waits(handle, &pair.wait, None),
        Err(ChannelWaitError::Runtime(RuntimeError::ShuttingDown))
    ));
    // Shutdown does not touch the durable rows either.
    let listed = pair.wait.list_waits(None).expect("list waits");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, WaitState::Pending);
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}
