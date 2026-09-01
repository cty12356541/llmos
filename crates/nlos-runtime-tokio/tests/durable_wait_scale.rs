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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nlos_channel::{ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest};
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter};
use nlos_runtime_tokio::{
    DeliveryReport, TokioChannelWakeSink, TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ChannelId, ExecutionFiberId, Generation, IdempotencyKey,
    ProcessId, ResourceGroupId, SchedulerDomainId,
};
use nlos_wait::{BindingId, NotifyCommitsRequest, RegisterWaitRequest, WaitAuthority, WaitState};
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
/// runtime threads. The bound must stay independent of the fiber count —
/// that independence is the ROAD-B-006 front-slice claim.
const THREAD_BOUND: usize = 10;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit Stage B 10K durable-wait fiber scale probe (quick tier)"]
async fn ten_thousand_durable_wait_fibers_on_two_workers() {
    run_durable_wait_fiber_scale(QUICK_COUNT, WAKE_SUBSET).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit Stage B ROAD-B-006 100K durable-wait fiber scale probe"]
async fn one_hundred_thousand_durable_wait_fibers_on_two_workers() {
    run_durable_wait_fiber_scale(FULL_COUNT, WAKE_SUBSET).await;
}
