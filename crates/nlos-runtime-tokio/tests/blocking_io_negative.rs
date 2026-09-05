//! ROAD-B-006 blocking I/O negative proof (W15-B): fibers on the real
//! durable-wait suspend path that perform blocking I/O MUST NOT linearly
//! increase the host process thread count as fiber count grows.
//!
//! This is the complement to `durable_wait_scale.rs`: that probe shows async
//! park on independent durable waits stays thread-bounded; this file adds
//! explicit blocking I/O on the fiber body (via Tokio's bounded
//! `spawn_blocking` pool and, in one small test, a deliberate misplaced
//! `std::thread::sleep` misuse) and proves thread growth stays sub-linear.
//!
//! Negative proof methodology:
//! 1. comparative tiers — measure thread count after full `WaitingIo` settle
//!    at a low fiber count vs a higher count (8×); assert
//!    `threads(high) <= threads(low) + SUBLINEAR_HEADROOM`;
//! 2. absolute bound — parked load on a fixed two-worker runtime stays below
//!    `THREAD_BOUND`, independent of fiber count;
//! 3. optional `#[ignore]` 10K tier for evidence parity with
//!    `durable_wait_scale.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nlos_channel::{ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest};
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ChannelId, ExecutionFiberId, Generation, IdempotencyKey,
    ProcessId, ResourceGroupId, SchedulerDomainId,
};
use nlos_wait::{RegisterWaitRequest, WaitAuthority};
use tokio::runtime::Handle;

/// Low tier for the comparative sub-linear proof (regular CI).
const LOW_COUNT: usize = 32;
/// High tier: eight times low; linear thread growth would fail the assertion.
const HIGH_COUNT: usize = 256;
/// Quick ignored tier for local re-measurement.
const QUICK_COUNT: usize = 10_000;
/// Allowed thread growth between low and high tiers — far below the 8× fiber
/// ratio if threads tracked fiber count linearly.
const SUBLINEAR_HEADROOM: usize = 15;
/// Tokio blocking-pool cap for `spawn_blocking` probes: proves blocking I/O
/// uses a bounded pool rather than one OS thread per fiber.
const MAX_BLOCKING_THREADS: usize = 8;
/// Upper bound with two workers + capped blocking pool + test/adapter headroom.
const BLOCKING_IO_THREAD_BOUND: usize = 2 + MAX_BLOCKING_THREADS + 6;
/// Simulated blocking I/O latency inside `spawn_blocking`.
const BLOCKING_IO_LATENCY: Duration = Duration::from_millis(2);
const REGISTERED_AT_MS: u64 = 1_000;
/// Host-thread upper bound when fibers are parked on async durable wait
/// without an expanded blocking pool (same rationale as `durable_wait_scale.rs`).
const THREAD_BOUND: usize = 10;

fn bounded_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .enable_all()
        .build()
        .expect("bounded runtime")
}

fn id_bytes(value: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(value as u64).to_be_bytes());
    bytes
}

fn binding(index: usize) -> nlos_wait::BindingId {
    let mut bytes = [0x42_u8; 16];
    bytes[8..].copy_from_slice(&(index as u64).to_be_bytes());
    nlos_wait::BindingId::from_bytes(bytes)
}

fn wait_key(index: usize) -> IdempotencyKey {
    let mut bytes = [0x4B_u8; 16];
    bytes[8..].copy_from_slice(&(index as u64).to_be_bytes());
    IdempotencyKey::from_bytes(bytes)
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
            "nlos-runtime-tokio-blocking-io-negative-{label}-{}-{nonce}-{sequence}",
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

/// Blocking I/O isolated through Tokio's bounded blocking pool, then the real
/// durable-wait park path.
async fn spawn_blocking_then_park(
    adapter: TokioRuntimeAdapter,
    waits: Arc<WaitAuthority>,
    channel_id: ChannelId,
    index: usize,
) -> FiberExit {
    tokio::task::spawn_blocking(|| std::thread::sleep(BLOCKING_IO_LATENCY))
        .await
        .expect("blocking io task join");
    park_on_durable_wait(adapter, waits, channel_id, index).await
}

/// Deliberate misuse: blocking sleep on a worker thread before durable wait.
/// Still must not spawn one OS thread per fiber.
async fn blocking_sleep_then_park(
    adapter: TokioRuntimeAdapter,
    waits: Arc<WaitAuthority>,
    channel_id: ChannelId,
    index: usize,
) -> FiberExit {
    std::thread::sleep(BLOCKING_IO_LATENCY);
    park_on_durable_wait(adapter, waits, channel_id, index).await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockingIoPattern {
    SpawnBlocking,
    MisplacedSleep,
}

fn spawn_blocking_io_fibers(
    runtime: &TokioRuntimeAdapter,
    waits: &Arc<WaitAuthority>,
    channel_id: ChannelId,
    scope: CancellationScopeId,
    count: usize,
    pattern: BlockingIoPattern,
) -> Vec<FiberHandle> {
    let adapter = runtime.clone();
    (0..count)
        .map(|index| {
            let future: nlos_runtime::FiberFuture = match pattern {
                BlockingIoPattern::SpawnBlocking => Box::pin(spawn_blocking_then_park(
                    adapter.clone(),
                    Arc::clone(waits),
                    channel_id,
                    index,
                )),
                BlockingIoPattern::MisplacedSleep => Box::pin(blocking_sleep_then_park(
                    adapter.clone(),
                    Arc::clone(waits),
                    channel_id,
                    index,
                )),
            };
            runtime
                .spawn_fiber(fiber_spec(index, scope), future)
                .expect("spawn fiber")
        })
        .collect()
}

async fn await_all_parked(
    runtime: &TokioRuntimeAdapter,
    handles: &[FiberHandle],
    budget: Duration,
) {
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
}

/// Runs the probe and returns the thread count after every fiber has completed
/// blocking I/O and settled on `WaitingIo`.
async fn threads_after_blocking_io_park(count: usize, pattern: BlockingIoPattern) -> usize {
    let root = Root::new("probe");
    let (channel_authority, waits) = open_authorities(&root);
    let channel = create_scale_channel(&channel_authority);
    let runtime = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: count,
        },
    )
    .expect("runtime adapter");

    let scope = CancellationScopeId::from_bytes([0x53; 16]);
    let handles =
        spawn_blocking_io_fibers(&runtime, &waits, channel.channel_id, scope, count, pattern);
    assert_eq!(runtime.registered_fibers(), count);

    let register_budget = (Duration::from_millis(20)
        * u32::try_from(count).expect("count fits u32"))
    .max(Duration::from_mins(2));
    await_all_parked(&runtime, &handles, register_budget).await;

    process_thread_count()
}

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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_thread_count() -> usize {
    0
}

#[test]
fn blocking_io_on_durable_wait_path_grows_threads_sublinearly() {
    bounded_runtime().block_on(async {
        let threads_low =
            threads_after_blocking_io_park(LOW_COUNT, BlockingIoPattern::SpawnBlocking).await;
        let threads_high =
            threads_after_blocking_io_park(HIGH_COUNT, BlockingIoPattern::SpawnBlocking).await;

        assert!(
            threads_high <= threads_low + SUBLINEAR_HEADROOM,
            "thread count grew linearly with fiber count: low={threads_low} high={threads_high} \
             (8× fibers, headroom={SUBLINEAR_HEADROOM})"
        );
        assert!(
            threads_high <= BLOCKING_IO_THREAD_BOUND,
            "host threads {threads_high} exceed bounded blocking-pool cap \
             {BLOCKING_IO_THREAD_BOUND}"
        );

        eprintln!(
            "blocking-io sublinear proof (max_blocking={MAX_BLOCKING_THREADS}): \
             fibers {LOW_COUNT}->{HIGH_COUNT}, threads {threads_low}->{threads_high}"
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn misplaced_blocking_sleep_stays_thread_bounded_on_durable_wait_path() {
    let threads_before = process_thread_count();
    let threads_after =
        threads_after_blocking_io_park(HIGH_COUNT, BlockingIoPattern::MisplacedSleep).await;

    assert!(
        threads_after <= THREAD_BOUND,
        "misplaced blocking sleep caused unbounded threads: {threads_after} > {THREAD_BOUND}"
    );
    assert!(
        threads_after <= threads_before + SUBLINEAR_HEADROOM,
        "threads grew with fiber count after misplaced blocking sleep: \
         before={threads_before} after={threads_after}"
    );

    eprintln!(
        "misplaced blocking sleep proof: {HIGH_COUNT} fibers, \
         threads {threads_before}->{threads_after}"
    );
}

#[test]
#[ignore = "explicit Stage B 10K blocking-I/O durable-wait negative proof (quick tier)"]
fn ten_thousand_blocking_io_fibers_stay_thread_bounded() {
    bounded_runtime().block_on(async {
        let threads_before = process_thread_count();
        let started = Instant::now();
        let threads_after =
            threads_after_blocking_io_park(QUICK_COUNT, BlockingIoPattern::SpawnBlocking).await;
        let elapsed = started.elapsed();

        assert!(
            threads_after <= BLOCKING_IO_THREAD_BOUND,
            "10K blocking-io fibers exceeded bounded thread cap: {threads_after} > \
             {BLOCKING_IO_THREAD_BOUND}"
        );
        assert!(
            threads_after <= threads_before + 2,
            "10K blocking-io fibers grew host threads: {threads_before} -> {threads_after}"
        );

        eprintln!(
            "10000-fiber blocking-io negative profile (2 tokio workers, \
             max_blocking={MAX_BLOCKING_THREADS}): register_settle={elapsed:?} \
             threads_before={threads_before} threads_after={threads_after}"
        );
    });
}
