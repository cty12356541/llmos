//! Explicit Stage B scale probe for ROAD-B-006 (front slice): the carrying
//! capacity of a bounded-worker Tokio runtime for dormant execution fibers
//! parked on independent durable Channel sequence waits.
//!
//! Every spawned fiber registers its own durable wait row through the
//! `nlos-wait` authority via `TokioRuntimeAdapter::wait_for_channel` and then
//! pends on it, so the probe exercises the real suspend path (durable
//! registration plus the in-memory handshake), not an in-memory `pending()`
//! stub.
//!
//! Measured and asserted, in the probe style of
//! `nlos-store/tests/store_scale.rs`:
//!
//! 1. spawn issue time and durable-registration settle time for the full
//!    fiber count, all of which must become visible as
//!    `FiberState::WaitingIo`;
//! 2. selective wake of the first `WAKE_SUBSET` targets through one
//!    `notify_commits` plus one `deliver` round: exactly the targeted fibers
//!    complete, every un-targeted fiber stays parked, and the durable rows
//!    agree (`WOKEN` prefix, `PENDING` tail);
//! 3. host-thread boundedness: the process thread count does not grow with
//!    the fiber count on a fixed two-worker runtime;
//! 4. resident set size before and after the full park — recorded, machine
//!    dependent, deliberately not asserted.
//!
//! The cancel/late-callback matrix, structured join/detach and Process crash
//! propagation are separate ROAD-B-006 follow-ups and deliberately out of
//! scope here. Both probes are `#[ignore]`-gated like the store scale probe:
//! the regular suite is unaffected, and the nightly scale-probe CI job runs
//! them via `--include-ignored`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nlos_channel::{ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest};
use nlos_runtime::{
    FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter, WakeOutcome, WakeSink,
};
use nlos_runtime_tokio::{
    DeliveryReport, TokioChannelWakeSink, TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ChannelId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, ProcessId, ResourceGroupId, SchedulerDomainId,
};
use nlos_wait::{
    BindingId, NotifyCommitsRequest, RegisterDecision, RegisterWaitRequest, WaitAuthority,
    WaitState, WakeReport,
};
use tokio::runtime::Handle;

/// Quick tier: same shape as the full probe at a tenth of the durable
/// registration cost, for fast local re-measurement.
const QUICK_COUNT: usize = 10_000;
/// The ROAD-B-006 exit-gate scale: one hundred thousand dormant fibers.
const FULL_COUNT: usize = 100_000;
/// Selectively woken prefix in both tiers; the remaining fibers must stay
/// parked.
const WAKE_SUBSET: usize = 1_000;
/// Registration timestamp carried by every durable wait row.
const REGISTERED_AT_MS: u64 = 1_000;
/// Notification timestamp of the selective wake.
const NOTIFIED_AT_MS: u64 = 2_000;
/// Host-thread upper bound under full park load on a two-worker runtime:
/// two tokio workers, the driving test thread, and headroom for auxiliary
/// runtime/libtest threads. The bound must stay independent of the fiber count —
/// that independence is the ROAD-B-006 front-slice claim.
const THREAD_BOUND: usize = 10;

/// Serialization guard so scale probes in this binary do not run concurrently
/// inside the same OS process and contaminate each other's RSS/thread/timing
/// readouts.
static PROBE_SERIALIZE: Mutex<()> = Mutex::new(());

fn acquire_probe_slot() -> MutexGuard<'static, ()> {
    PROBE_SERIALIZE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn bounded_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("bounded runtime")
}

const fn notify_key_seeded(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn operation(index: usize) -> OperationId {
    OperationId::from_bytes(id_bytes(index))
}

fn id_bytes(value: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(value as u64).to_be_bytes());
    bytes
}

/// A stable per-fiber [`BindingId`]: a fixed non-zero domain prefix plus the
/// fiber index, so every waiter is its own binding.
fn binding(index: usize) -> BindingId {
    let mut bytes = [0x42_u8; 16];
    bytes[8..].copy_from_slice(&(index as u64).to_be_bytes());
    BindingId::from_bytes(bytes)
}

/// A stable per-fiber registration idempotency key with the same shape.
fn wait_key(index: usize) -> IdempotencyKey {
    let mut bytes = [0x4B_u8; 16];
    bytes[8..].copy_from_slice(&(index as u64).to_be_bytes());
    IdempotencyKey::from_bytes(bytes)
}

/// The selective wake's idempotency key; single-use per probe run.
const fn notify_key() -> IdempotencyKey {
    IdempotencyKey::from_bytes([0x4E; 16])
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
            "nlos-runtime-tokio-durable-wait-scale-{label}-{}-{nonce}-{sequence}",
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

fn open_authorities(root: &Root) -> (Arc<ChannelAuthority>, Arc<WaitAuthority>) {
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait = Arc::new(
        WaitAuthority::open(root.path(), Arc::clone(&channel)).expect("open wait authority"),
    );
    (channel, wait)
}

fn create_scale_channel(authority: &ChannelAuthority) -> ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: IdempotencyKey::from_bytes([0xC1; 16]),
            created_at_ms: 900,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

/// The body of one parked fiber: registers its own durable Channel sequence
/// wait (target sequence `index + 1` on the shared channel) and resolves
/// `Completed` only on its wake. Any other resolution is a probe failure,
/// and surfaces as a `Failed` fiber state.
async fn park_on_durable_wait(
    adapter: TokioRuntimeAdapter,
    waits: Arc<WaitAuthority>,
    channel_id: ChannelId,
    index: usize,
) -> FiberExit {
    let handle = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        generation: Generation::INITIAL,
    };
    let request = RegisterWaitRequest {
        binding: binding(index),
        channel_id,
        target_sequence: index as u64 + 1,
        idempotency_key: wait_key(index),
        registered_at_ms: REGISTERED_AT_MS,
    };
    let wait = adapter
        .wait_for_channel(handle, &waits, request)
        .expect("durable wait registration");
    match wait.await {
        WaitOutcome::Woken => FiberExit::Completed,
        outcome @ WaitOutcome::Cancelled => {
            panic!("fiber {index} resolved without its wake: {outcome:?}")
        }
    }
}

/// Spawns `count` fibers that each park on their own durable wait and
/// returns the handles plus the spawn issue time.
fn spawn_parked_fibers(
    runtime: &TokioRuntimeAdapter,
    waits: &Arc<WaitAuthority>,
    channel_id: ChannelId,
    scope: CancellationScopeId,
    count: usize,
) -> (Vec<FiberHandle>, Duration) {
    let adapter = runtime.clone();
    let started = Instant::now();
    let handles = (0..count)
        .map(|index| {
            runtime
                .spawn_fiber(
                    fiber_spec(index, scope),
                    Box::pin(park_on_durable_wait(
                        adapter.clone(),
                        Arc::clone(waits),
                        channel_id,
                        index,
                    )),
                )
                .expect("spawn parked fiber")
        })
        .collect::<Vec<_>>();
    let elapsed = started.elapsed();
    assert_eq!(runtime.registered_fibers(), count);
    (handles, elapsed)
}

/// Polls until every spawned fiber is visible as `FiberState::WaitingIo` —
/// the state only a completed durable registration produces — and returns
/// the settle time. `FiberState::WaitingIo` is set synchronously inside
/// `wait_for_channel`, so full visibility is exactly "all durable wait rows
/// registered".
async fn await_all_parked(
    runtime: &TokioRuntimeAdapter,
    handles: &[FiberHandle],
    budget: Duration,
) -> Duration {
    let started = Instant::now();
    tokio::time::timeout(budget, async {
        loop {
            let unsettled = handles
                .iter()
                .any(|handle| runtime.inspect(*handle) != Ok(FiberState::WaitingIo));
            if !unsettled {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("every fiber must reach WaitingIo");
    started.elapsed()
}

/// Wakes exactly the first `subset` targets (their durable waits target
/// sequences `1..=subset` on the shared channel), delivers the wake report,
/// and asserts the targeted fibers complete while every un-targeted fiber
/// stays parked. Returns the notify, deliver and wake-settle durations.
async fn run_wake_phase(
    runtime: &TokioRuntimeAdapter,
    sink: &TokioChannelWakeSink,
    waits: &WaitAuthority,
    channel_id: ChannelId,
    handles: &[FiberHandle],
    subset: usize,
) -> (Duration, Duration, Duration) {
    let notify_started = Instant::now();
    let wake = waits
        .notify_commits(NotifyCommitsRequest {
            channel_id,
            up_to_sequence: subset as u64,
            notified_at_ms: NOTIFIED_AT_MS,
            idempotency_key: notify_key(),
        })
        .expect("notify commits");
    let notify_elapsed = notify_started.elapsed();
    assert_eq!(wake.woken.len(), subset);
    assert!(wake.woken.iter().all(|record| {
        record.state == WaitState::Woken && record.target_sequence <= subset as u64
    }));

    let deliver_started = Instant::now();
    let delivery = sink.deliver(&wake).expect("deliver wake report");
    let deliver_elapsed = deliver_started.elapsed();
    assert_eq!(
        delivery,
        DeliveryReport {
            delivered: subset,
            buffered: 0
        }
    );

    let settle_started = Instant::now();
    let settle_budget = (Duration::from_millis(5)
        * u32::try_from(subset).expect("subset fits u32"))
    .max(Duration::from_mins(1));
    tokio::time::timeout(settle_budget, async {
        loop {
            let unfinished = handles[..subset]
                .iter()
                .any(|handle| runtime.inspect(*handle) != Ok(FiberState::Completed));
            if !unfinished {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("every targeted fiber must complete on its wake");
    let settle_elapsed = settle_started.elapsed();

    for handle in &handles[subset..] {
        assert_eq!(
            runtime.inspect(*handle),
            Ok(FiberState::WaitingIo),
            "an un-targeted fiber was woken"
        );
    }
    (notify_elapsed, deliver_elapsed, settle_elapsed)
}

/// Durable-side mis-wake check: exactly the targeted prefix of the durable
/// wait rows is `WOKEN`, the rest is still `PENDING`. Returns the readback
/// duration.
fn assert_durable_rows(
    waits: &WaitAuthority,
    channel_id: ChannelId,
    count: usize,
    subset: usize,
) -> Duration {
    let started = Instant::now();
    let rows = waits
        .inspect_channel_waits(channel_id)
        .expect("durable wait rows");
    let elapsed = started.elapsed();
    assert_eq!(rows.len(), count);
    let woken = rows
        .iter()
        .filter(|record| record.state == WaitState::Woken)
        .count();
    assert_eq!(woken, subset, "exactly the targeted durable rows may wake");
    for record in &rows {
        let expected = if record.target_sequence <= subset as u64 {
            WaitState::Woken
        } else {
            WaitState::Pending
        };
        assert_eq!(
            record.state, expected,
            "durable row for target {}",
            record.target_sequence
        );
    }
    elapsed
}

/// The carrying-capacity probe body: spawn `count` fibers parked on
/// independent durable waits, assert full `WaitingIo` visibility, wake a
/// `subset` of them selectively, and record the timing, memory and thread
/// profile. Asserted bounds are platform independent; everything else is
/// recorded for the evidence file, never fabricated.
async fn run_durable_wait_fiber_scale(count: usize, subset: usize) {
    let root = Root::new("durable-wait-scale");
    let (channel_authority, waits) = open_authorities(&root);
    let channel = create_scale_channel(&channel_authority);
    let runtime = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: count,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime adapter");
    let sink = runtime.channel_wait_sink();

    let rss_before_kib = process_rss_kib();
    let threads_before = process_thread_count();

    // One shared cancellation scope: distinct scopes would make the adapter's
    // scope registry insert O(existing scopes) per spawn and turn the probe
    // itself quadratic.
    let scope = CancellationScopeId::from_bytes([0x53; 16]);
    let (handles, spawn_issue) =
        spawn_parked_fibers(&runtime, &waits, channel.channel_id, scope, count);

    // fsync-bound honest upper bound for `synchronous=FULL` single-row
    // durable registrations; the measured value is recorded, not this bound.
    let register_budget = (Duration::from_millis(20)
        * u32::try_from(count).expect("count fits u32"))
    .max(Duration::from_mins(2));
    let register_settle = await_all_parked(&runtime, &handles, register_budget).await;

    let threads_after = process_thread_count();
    let rss_after_kib = process_rss_kib();
    assert!(
        threads_after <= THREAD_BOUND,
        "host threads {threads_after} exceed the fiber-count-independent bound {THREAD_BOUND}"
    );
    assert!(
        threads_after <= threads_before + 2,
        "host threads grew with the fiber count: {threads_before} -> {threads_after}"
    );

    let (wake_notify, wake_deliver, wake_settle) = run_wake_phase(
        &runtime,
        &sink,
        &waits,
        channel.channel_id,
        &handles,
        subset,
    )
    .await;
    let durable_readback = assert_durable_rows(&waits, channel.channel_id, count, subset);

    eprintln!(
        "{count}-fiber durable-wait profile (2 tokio workers): \
         spawn_issue={spawn_issue:?} register_settle={register_settle:?} \
         wake_notify={wake_notify:?} wake_deliver={wake_deliver:?} \
         wake_settle={wake_settle:?} durable_readback={durable_readback:?} \
         rss_before={rss_before_kib}KiB rss_after={rss_after_kib}KiB \
         threads_before={threads_before} threads_after={threads_after}"
    );
}

/// Resident set size of this process in KiB, read out-of-band so the probe
/// adds no measurement dependency to the crate.
#[cfg(target_os = "macos")]
fn process_rss_kib() -> u64 {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps rss readout");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .expect("ps rss is an integer")
}

/// Mach thread count via `ps -M`: one header line followed by one line per
/// thread.
#[cfg(target_os = "macos")]
fn process_thread_count() -> usize {
    let output = std::process::Command::new("/bin/ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps thread readout");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .count()
        .saturating_sub(1)
}

#[cfg(target_os = "linux")]
fn process_rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("proc status readout");
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .expect("VmRSS present");
    line["VmRSS:".len()..]
        .split_whitespace()
        .next()
        .expect("VmRSS value")
        .parse::<u64>()
        .expect("VmRSS is an integer")
}

#[cfg(target_os = "linux")]
fn process_thread_count() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").expect("proc status readout");
    let line = status
        .lines()
        .find(|line| line.starts_with("Threads:"))
        .expect("Threads present");
    line["Threads:".len()..]
        .trim()
        .parse::<usize>()
        .expect("Threads is an integer")
}

/// The probe is `#[ignore]`-gated and never executed on platforms without a
/// cheap out-of-band measurement readout; these stubs keep the target
/// compiling there.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_rss_kib() -> u64 {
    0
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_thread_count() -> usize {
    0
}

#[test]
#[ignore = "explicit Stage B 10K durable-wait fiber scale probe (quick tier)"]
fn ten_thousand_durable_wait_fibers_on_two_workers() {
    let _slot = acquire_probe_slot();
    bounded_runtime().block_on(async {
        tokio::task::yield_now().await;
        run_durable_wait_fiber_scale(QUICK_COUNT, WAKE_SUBSET).await;
    });
}

#[test]
#[ignore = "explicit Stage B ROAD-B-006 100K durable-wait fiber scale probe"]
fn one_hundred_thousand_durable_wait_fibers_on_two_workers() {
    let _slot = acquire_probe_slot();
    bounded_runtime().block_on(async {
        tokio::task::yield_now().await;
        run_durable_wait_fiber_scale(FULL_COUNT, WAKE_SUBSET).await;
    });
}

/// Non-ignored U-11 regression: 5,000 fibers park on durable Channel waits,
/// receive mixed Operation waits (both `Pending` and re-buffered `Buffered`)
/// plus secondary un-delivered Channel waits (including fiber `0` with
/// `ExecutionFiberId([0; 16])`) alongside a bounded FIFO orphan buffer (64
/// deliveries clamped to 32). Waking and terminating 2,500 fibers while leaving
/// 2,500 parked verifies O(k) `purge_fiber` exact closure: `wait_count ==
/// 2,500`, `channel_wait_count == 2,532`, zero leaked keys, zero false wakes,
/// and all 32 surviving orphans intact; waking the remaining 2,500 drains
/// both registries, fibers, and scopes to zero.
#[test]
#[allow(clippy::too_many_lines)]
fn five_thousand_mixed_waits_partial_wake_storm_and_orphan_boundary_zero_leak() {
    const REGRESSION_COUNT: usize = 5_000;
    const WAKE_HALF: usize = 2_500;
    const OP_BUFFERED_SUBSET: usize = 250;
    const EXTRA_CHANNEL_WAITS: usize = 250;
    const ORPHAN_CAPACITY: usize = 32;
    const ORPHAN_DELIVERIES: usize = 64;
    const EXTRA_TARGET_OFFSET: usize = 100_000;
    const ORPHAN_TARGET_OFFSET: usize = 200_000;

    let _slot = acquire_probe_slot();
    bounded_runtime().block_on(async {
        let started = Instant::now();
        let root = Root::new("u11-mixed-5k");
        let (channel_authority, waits) = open_authorities(&root);
        let channel = create_scale_channel(&channel_authority);
        let runtime = TokioRuntimeAdapter::new(
            Handle::current(),
            TokioRuntimeConfig {
                max_live_fibers: REGRESSION_COUNT + 16,
                orphan_buffer_capacity: ORPHAN_CAPACITY,
                ..TokioRuntimeConfig::default()
            },
        )
        .expect("runtime adapter");
        let op_sink = runtime.wake_sink();
        let ch_sink = runtime.channel_wait_sink();

        let scope = CancellationScopeId::from_bytes([0x61; 16]);
        let (handles, spawn_issue) = spawn_parked_fibers(
            &runtime,
            &waits,
            channel.channel_id,
            scope,
            REGRESSION_COUNT,
        );
        let register_settle = await_all_parked(&runtime, &handles, Duration::from_mins(2)).await;
        assert_eq!(runtime.channel_wait_count(), REGRESSION_COUNT);
        assert_eq!(runtime.wait_count(), 0);

        // Attach mixed Operation waits across all 5,000 fibers: 0..250 (which
        // includes fiber 0 = [0; 16]) are dropped and re-buffered as
        // `WaitEntry::Buffered`, while 250..5000 stay live `Pending` futures.
        let mut held_op_waits = Vec::with_capacity(REGRESSION_COUNT - OP_BUFFERED_SUBSET);
        for (index, handle) in handles.iter().enumerate() {
            let op_id = operation(10_000 + index);
            let wait = runtime
                .wait_for_operation(*handle, op_id, Generation::INITIAL)
                .expect("wait_for_operation");
            if index < OP_BUFFERED_SUBSET {
                drop(wait);
                assert_eq!(
                    op_sink.wake(handle, op_id, Generation::INITIAL),
                    Ok(WakeOutcome::Delivered)
                );
            } else {
                held_op_waits.push((index, wait));
            }
        }
        assert_eq!(runtime.wait_count(), REGRESSION_COUNT);

        // Attach secondary un-delivered Channel waits on 0..250 (half Pending,
        // half dropped + re-buffered under the fiber's key) so `ChannelWaitRegistry::purge_fiber`
        // also exercises both `Pending` and fiber-bound `Buffered` purges.
        let mut held_extra_channel_waits = Vec::with_capacity(EXTRA_CHANNEL_WAITS / 2);
        for (index, handle) in handles.iter().take(EXTRA_CHANNEL_WAITS).enumerate() {
            let req = RegisterWaitRequest {
                binding: binding(EXTRA_TARGET_OFFSET + index),
                channel_id: channel.channel_id,
                target_sequence: (EXTRA_TARGET_OFFSET + index + 1) as u64,
                idempotency_key: wait_key(EXTRA_TARGET_OFFSET + index),
                registered_at_ms: REGISTERED_AT_MS,
            };
            let durable = match waits
                .register_wait(req)
                .expect("register extra channel wait")
            {
                RegisterDecision::Registered(rec) | RegisterDecision::Replayed(rec) => rec,
            };
            let wait = runtime
                .wait_for_channel(*handle, &waits, req)
                .expect("wait_for_channel extra");
            if index < EXTRA_CHANNEL_WAITS / 2 {
                held_extra_channel_waits.push(wait);
            } else {
                drop(wait);
                let mut synthetic = durable;
                synthetic.state = WaitState::Woken;
                let report = ch_sink
                    .deliver(&WakeReport {
                        woken: vec![synthetic],
                    })
                    .expect("deliver rebuffer");
                assert_eq!(
                    report,
                    DeliveryReport {
                        delivered: 1,
                        buffered: 1
                    }
                );
            }
        }
        assert_eq!(
            runtime.channel_wait_count(),
            REGRESSION_COUNT + EXTRA_CHANNEL_WAITS
        );

        // Deliver 64 orphaned wakes into capacity-32 orphan buffer -> 32 oldest
        // evicted (0..32), 32 newest (32..64) buffered.
        let mut orphan_requests = Vec::with_capacity(ORPHAN_DELIVERIES);
        let mut orphan_woken = Vec::with_capacity(ORPHAN_DELIVERIES);
        for offset in 0..ORPHAN_DELIVERIES {
            let req = RegisterWaitRequest {
                binding: binding(ORPHAN_TARGET_OFFSET + offset),
                channel_id: channel.channel_id,
                target_sequence: (ORPHAN_TARGET_OFFSET + offset + 1) as u64,
                idempotency_key: wait_key(ORPHAN_TARGET_OFFSET + offset),
                registered_at_ms: REGISTERED_AT_MS,
            };
            let mut rec = match waits.register_wait(req).expect("register orphan wait") {
                RegisterDecision::Registered(rec) | RegisterDecision::Replayed(rec) => rec,
            };
            rec.state = WaitState::Woken;
            orphan_requests.push(req);
            orphan_woken.push(rec);
        }
        assert_eq!(
            ch_sink
                .deliver(&WakeReport {
                    woken: orphan_woken,
                })
                .expect("deliver orphans"),
            DeliveryReport {
                delivered: ORPHAN_DELIVERIES,
                buffered: ORPHAN_DELIVERIES,
            }
        );
        assert_eq!(
            runtime.health().orphan_buffer_dropped_total,
            (ORPHAN_DELIVERIES - ORPHAN_CAPACITY) as u64
        );
        assert_eq!(
            runtime.channel_wait_count(),
            REGRESSION_COUNT + EXTRA_CHANNEL_WAITS + ORPHAN_CAPACITY
        );

        // Wake the first 2,500 fibers simultaneously (includes fiber 0 = [0; 16]).
        let wake_half_started = Instant::now();
        let wake_report_1 = waits
            .notify_commits(NotifyCommitsRequest {
                channel_id: channel.channel_id,
                up_to_sequence: WAKE_HALF as u64,
                notified_at_ms: NOTIFIED_AT_MS,
                idempotency_key: notify_key_seeded(0x51),
            })
            .expect("notify half 1");
        assert_eq!(wake_report_1.woken.len(), WAKE_HALF);
        assert_eq!(
            ch_sink.deliver(&wake_report_1).expect("deliver half 1"),
            DeliveryReport {
                delivered: WAKE_HALF,
                buffered: 0,
            }
        );
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if handles[..WAKE_HALF]
                    .iter()
                    .all(|handle| runtime.inspect(*handle) == Ok(FiberState::Completed))
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first 2,500 fibers must complete");
        let wake_half_elapsed = wake_half_started.elapsed();

        for handle in &handles[..WAKE_HALF] {
            assert_eq!(runtime.join_fiber(*handle), Ok(FiberExit::Completed));
        }
        for handle in &handles[WAKE_HALF..] {
            assert_eq!(runtime.inspect(*handle), Ok(FiberState::WaitingIo));
        }
        let mut remaining_op_waits = Vec::with_capacity(REGRESSION_COUNT - WAKE_HALF);
        tokio::time::timeout(Duration::from_secs(5), async {
            for wait in held_extra_channel_waits {
                assert_eq!(wait.await, WaitOutcome::Cancelled);
            }
            for (index, wait) in held_op_waits {
                if index < WAKE_HALF {
                    assert_eq!(wait.await, WaitOutcome::Cancelled);
                } else {
                    remaining_op_waits.push(wait);
                }
            }
        })
        .await
        .expect("purged half waits must resolve Cancelled");

        // Exact mid-point closure: zero leaked keys from 0..2500, zero lost
        // keys from 2500..5000, and all 32 orphan entries intact despite fiber 0
        // terminating.
        assert_eq!(runtime.wait_count(), REGRESSION_COUNT - WAKE_HALF);
        assert_eq!(
            runtime.channel_wait_count(),
            (REGRESSION_COUNT - WAKE_HALF) + ORPHAN_CAPACITY
        );
        assert_eq!(runtime.registered_fibers(), REGRESSION_COUNT - WAKE_HALF);
        assert_eq!(
            runtime.health().orphan_buffer_dropped_total,
            (ORPHAN_DELIVERIES - ORPHAN_CAPACITY) as u64
        );

        // Verify orphan FIFO boundary: index 0 was evicted, indices 32..64
        // survived and are consumed cleanly.
        let probe_scope = CancellationScopeId::from_bytes([0x62; 16]);
        let probe_handle = runtime
            .spawn_fiber(
                fiber_spec(REGRESSION_COUNT, probe_scope),
                Box::pin(std::future::pending()),
            )
            .expect("spawn orphan probe fiber");
        let mut evicted_wait = runtime
            .wait_for_channel(probe_handle, &waits, orphan_requests[0])
            .expect("register evicted orphan");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut evicted_wait)
                .await
                .is_err(),
            "FIFO-evicted orphan must remain pending"
        );
        drop(evicted_wait);
        tokio::time::timeout(Duration::from_secs(5), async {
            for req in &orphan_requests[ORPHAN_CAPACITY..] {
                let survived = runtime
                    .wait_for_channel(probe_handle, &waits, *req)
                    .expect("register surviving orphan");
                assert_eq!(survived.await, WaitOutcome::Woken);
            }
        })
        .await
        .expect("surviving orphans must resolve Woken immediately");
        runtime
            .cancel_scope(probe_scope, Generation::INITIAL)
            .expect("cancel probe scope");
        assert_eq!(runtime.join_fiber(probe_handle), Ok(FiberExit::Cancelled));
        assert_eq!(runtime.channel_wait_count(), REGRESSION_COUNT - WAKE_HALF);

        // Wake the remaining 2,500 fibers and verify complete zero-residual drain.
        let wake_rest_started = Instant::now();
        let wake_report_2 = waits
            .notify_commits(NotifyCommitsRequest {
                channel_id: channel.channel_id,
                up_to_sequence: REGRESSION_COUNT as u64,
                notified_at_ms: NOTIFIED_AT_MS + 1,
                idempotency_key: notify_key_seeded(0x52),
            })
            .expect("notify half 2");
        assert_eq!(wake_report_2.woken.len(), REGRESSION_COUNT - WAKE_HALF);
        assert_eq!(
            ch_sink.deliver(&wake_report_2).expect("deliver half 2"),
            DeliveryReport {
                delivered: REGRESSION_COUNT - WAKE_HALF,
                buffered: 0,
            }
        );
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if handles[WAKE_HALF..]
                    .iter()
                    .all(|handle| runtime.inspect(*handle) == Ok(FiberState::Completed))
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("remaining 2,500 fibers must complete");
        let wake_rest_elapsed = wake_rest_started.elapsed();

        for handle in &handles[WAKE_HALF..] {
            assert_eq!(runtime.join_fiber(*handle), Ok(FiberExit::Completed));
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            for wait in remaining_op_waits {
                assert_eq!(wait.await, WaitOutcome::Cancelled);
            }
        })
        .await
        .expect("remaining purged operation waits must resolve Cancelled");

        assert_eq!(runtime.wait_count(), 0);
        assert_eq!(runtime.channel_wait_count(), 0);
        assert_eq!(runtime.registered_fibers(), 0);
        assert_eq!(runtime.registered_scopes(), 0);

        eprintln!(
            "5000-fiber U-11 mixed-wait + orphan-boundary regression: \
             spawn_issue={spawn_issue:?} register_settle={register_settle:?} \
             wake_half={wake_half_elapsed:?} wake_rest={wake_rest_elapsed:?} \
             total={:?}",
            started.elapsed()
        );
    });
}

/// Explicit Stage B `--release` 100K simultaneous wake storm and full terminal
/// purge probe (U-11 / ROAD-B-006).
///
/// Spawns `FULL_COUNT = 100,000` fibers parked on independent durable Channel
/// waits plus `100,000` in-memory Operation waits, wakes all `100,000` fibers
/// in one `notify_commits` + `deliver` storm, drains every fiber to terminal
/// and joins/reaps the full cohort, asserting `host_threads <= THREAD_BOUND`
/// and exact zero residual across `wait_count`, `channel_wait_count`,
/// `registered_fibers`, and `registered_scopes`.
#[test]
#[ignore = "explicit Stage B U-11 100K simultaneous wake storm and full terminal purge probe"]
#[allow(clippy::too_many_lines)]
fn one_hundred_thousand_simultaneous_wake_storm_and_terminal_purge() {
    let _slot = acquire_probe_slot();
    bounded_runtime().block_on(async {
        tokio::task::yield_now().await;
        let root = Root::new("wake-storm-100k");
        let (channel_authority, waits) = open_authorities(&root);
        let channel = create_scale_channel(&channel_authority);
        let runtime = TokioRuntimeAdapter::new(
            Handle::current(),
            TokioRuntimeConfig {
                max_live_fibers: FULL_COUNT,
                ..TokioRuntimeConfig::default()
            },
        )
        .expect("runtime adapter");
        let sink = runtime.channel_wait_sink();

        let rss_before_kib = process_rss_kib();
        let threads_before = process_thread_count();

        let scope = CancellationScopeId::from_bytes([0x73; 16]);
        let (handles, spawn_issue) =
            spawn_parked_fibers(&runtime, &waits, channel.channel_id, scope, FULL_COUNT);

        let register_budget = (Duration::from_millis(20)
            * u32::try_from(FULL_COUNT).expect("count fits u32"))
        .max(Duration::from_mins(2));
        let register_settle = await_all_parked(&runtime, &handles, register_budget).await;
        assert_eq!(runtime.channel_wait_count(), FULL_COUNT);

        // Attach 100,000 un-woken Operation waits so `OperationWaitRegistry::purge_fiber`
        // executes 100K concurrent O(1) purges during the terminal drain.
        let op_attach_started = Instant::now();
        let mut op_waits = Vec::with_capacity(FULL_COUNT);
        for (index, handle) in handles.iter().enumerate() {
            op_waits.push(
                runtime
                    .wait_for_operation(*handle, operation(10_000 + index), Generation::INITIAL)
                    .expect("attach operation wait"),
            );
        }
        let op_attach_elapsed = op_attach_started.elapsed();
        assert_eq!(runtime.wait_count(), FULL_COUNT);

        let threads_parked = process_thread_count();
        let rss_parked_kib = process_rss_kib();
        assert!(
            threads_parked <= THREAD_BOUND,
            "host threads {threads_parked} exceed the fiber-count-independent bound {THREAD_BOUND}"
        );
        assert!(
            threads_parked <= threads_before + 4,
            "host threads grew unexpectedly during park: {threads_before} -> {threads_parked}"
        );

        let (wake_notify, wake_deliver, wake_settle) = run_wake_phase(
            &runtime,
            &sink,
            &waits,
            channel.channel_id,
            &handles,
            FULL_COUNT,
        )
        .await;

        let threads_after_storm = process_thread_count();
        let rss_after_storm_kib = process_rss_kib();
        assert!(
            threads_after_storm <= THREAD_BOUND,
            "host threads {threads_after_storm} exceed bound {THREAD_BOUND} during wake storm"
        );

        let join_started = Instant::now();
        for (index, handle) in handles.iter().enumerate() {
            assert_eq!(
                runtime.join_fiber(*handle),
                Ok(FiberExit::Completed),
                "fiber {index} must join as Completed"
            );
        }
        let join_reap = join_started.elapsed();

        tokio::time::timeout(Duration::from_secs(10), async {
            for (index, op_wait) in op_waits.into_iter().enumerate() {
                assert_eq!(
                    op_wait.await,
                    WaitOutcome::Cancelled,
                    "operation wait {index} must resolve Cancelled on terminal purge"
                );
            }
        })
        .await
        .expect("all 100K purged operation waits must resolve Cancelled");

        assert_eq!(runtime.wait_count(), 0);
        assert_eq!(runtime.channel_wait_count(), 0);
        assert_eq!(runtime.registered_fibers(), 0);
        assert_eq!(runtime.registered_scopes(), 0);

        let durable_readback =
            assert_durable_rows(&waits, channel.channel_id, FULL_COUNT, FULL_COUNT);
        let threads_after_reap = process_thread_count();
        let rss_after_reap_kib = process_rss_kib();
        assert!(
            threads_after_reap <= THREAD_BOUND,
            "host threads {threads_after_reap} exceed bound {THREAD_BOUND} after reap"
        );

        eprintln!(
            "{FULL_COUNT}-fiber simultaneous wake storm + terminal purge (2 tokio workers): \
             spawn_issue={spawn_issue:?} register_settle={register_settle:?} \
             op_attach={op_attach_elapsed:?} wake_notify={wake_notify:?} \
             wake_deliver={wake_deliver:?} wake_settle={wake_settle:?} \
             join_reap={join_reap:?} durable_readback={durable_readback:?} \
             rss_before={rss_before_kib}KiB rss_parked={rss_parked_kib}KiB \
             rss_after_storm={rss_after_storm_kib}KiB rss_after_reap={rss_after_reap_kib}KiB \
             threads_before={threads_before} threads_parked={threads_parked} \
             threads_after_storm={threads_after_storm} threads_after_reap={threads_after_reap}"
        );
    });
}
