//! End-to-end wake-latency distribution measurement probe (W35-P6 /
//! B-RUNTIME-002, closing PoC-0004 §6's deferred 「真实规模 backpressure
//! 计量」 as a first cut): the REAL durable closed loop — `SQLite` Operation
//! authority commits → `OutboxPump` OS thread (fallback poll + writer hint)
//! → `TokioWakeSink` → the fiber's `wait_for_operation` observation — under
//! a bounded commit burst, with per-entry commit→observe wall-clock
//! latencies recorded as percentiles.
//!
//! This is a MEASUREMENT artifact, not a deterministic assertion: the
//! numbers are recorded to evidence (`--nocapture`), and only sanity bounds
//! are asserted (every entry delivered `Woken`, the outbox fully
//! acknowledged, no pump failures, every latency non-negative and inside a
//! deliberately generous ceiling). No absolute latency contract is implied
//! — runner speed, disk fsync behavior, and scheduler load all shift the
//! distribution, which is exactly why the distribution itself is the
//! deliverable.
//!
//! Measured per entry `i`:
//!
//! - `commit_at[i]`: the instant `store.complete()` returned — the wake
//!   outbox entry is durable from this point (same transaction);
//! - `observed_at[i]`: the instant the fiber body observed `Woken`;
//! - latency = `observed_at` − `commit_at` (one process, one monotonic clock).
//!
//! Burst shape: `BURST` operations all registered and parked first, then
//! completions committed back-to-back from one writer thread, then a single
//! `pump.hint()` — the pump drains in `BATCH_LIMIT`-sized batches, so the
//! distribution reflects queue-position backpressure, not per-entry hints.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nlos_operation::{CompletionOutcome, OperationSpec};
use nlos_outbox::{ConsumerConfig, OutboxConsumer};
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, RuntimeAdapter};
use nlos_runtime_tokio::{
    OutboxPump, PumpConfig, PumpState, RecordingReconcileSink, StoreOutboxSource,
    TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome,
};
use nlos_store::SqliteOperationStore;
use nlos_types::{
    AgentInstanceId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId,
    ProcessId, ReceiptId, ResourceGroupId, SchedulerDomainId,
};
use tokio::runtime::Handle;

/// Bounded burst: the first-cut measurement population.
const BURST: usize = 256;
/// Pump batch limit: queue-position backpressure is visible in the spread.
const BATCH_LIMIT: usize = 16;
/// Generous bound for events that must happen (sanity only).
const RESOLVE: Duration = Duration::from_mins(1);
/// Poll step for bounded waits.
const POLL_STEP: Duration = Duration::from_millis(10);

/// Nearest-rank percentile (rank given as a whole percentage) of a
/// non-empty ascending slice.
fn percentile(sorted: &[Duration], percent: u64) -> Duration {
    let len = u64::try_from(sorted.len()).expect("population fits u64");
    let rank = (percent.saturating_mul(len).saturating_add(99) / 100).clamp(1, len);
    let index = usize::try_from(rank - 1).expect("index fits usize");
    sorted[index]
}
/// Sanity ceiling for any single latency sample (NOT a contract).
const LATENCY_SANITY: Duration = Duration::from_secs(30);

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-outbox-latency-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn id_bytes(value: u64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&value.to_be_bytes());
    bytes
}

fn fiber_spec(index: u64) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: AgentInstanceId::from_bytes(id_bytes(10_000 + index)),
        agent_generation: Generation::INITIAL,
        process_id: ProcessId::from_bytes(id_bytes(1)),
        process_generation: Generation::INITIAL,
        task_attempt_id: None,
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(30_000 + index)),
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(id_bytes(1)),
        scheduler_domain_id: SchedulerDomainId::from_bytes(id_bytes(1)),
        deadline: None,
    }
}

fn op_spec(index: u64, owner: FiberHandle) -> OperationSpec {
    OperationSpec {
        operation_id: OperationId::from_bytes(id_bytes(index)),
        generation: Generation::INITIAL,
        owner_fiber: owner,
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(40_000 + index)),
        cancellation_generation: Generation::INITIAL,
    }
}

/// What one fiber observed: the wait outcome and when it observed it.
#[derive(Clone, Copy)]
struct Sample {
    outcome: WaitOutcome,
    observed_at: Instant,
}

fn spawn_waking_fiber(
    runtime: &TokioRuntimeAdapter,
    index: usize,
    observed: &Arc<Mutex<Vec<Option<Sample>>>>,
) -> FiberHandle {
    let spec = fiber_spec(index as u64);
    let handle = FiberHandle {
        fiber_id: spec.fiber_id,
        generation: spec.fiber_generation,
    };
    let adapter = runtime.clone();
    let observed = Arc::clone(observed);
    let body = async move {
        let wait = adapter
            .wait_for_operation(
                handle,
                OperationId::from_bytes(id_bytes(index as u64)),
                Generation::INITIAL,
            )
            .expect("wait registration");
        let outcome = wait.await;
        lock(&observed)[index] = Some(Sample {
            outcome,
            observed_at: Instant::now(),
        });
        FiberExit::Completed
    };
    runtime
        .spawn_fiber(spec, Box::pin(body))
        .expect("spawn waking fiber")
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(RESOLVE, async move {
        while !condition() {
            tokio::time::sleep(POLL_STEP).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn pending_count(store: &SqliteOperationStore) -> usize {
    store.pending_outbox(BURST).expect("pending outbox").len()
}

#[allow(clippy::too_many_lines)] // One measurement run: setup, burst, drain, and the percentile report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement probe: end-to-end SQLite→pump→wake wall-clock distribution (records percentiles, sanity bounds only)"]
async fn outbox_wake_end_to_end_wall_clock_distribution_measurement() {
    let database = TestDatabase::new("burst");
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("open"));
    let runtime = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: BURST + 8,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime");

    let observed: Arc<Mutex<Vec<Option<Sample>>>> = Arc::new(Mutex::new(vec![None; BURST]));
    let mut tickets = Vec::with_capacity(BURST);
    let mut handles = Vec::with_capacity(BURST);
    for index in 0..BURST {
        let handle = spawn_waking_fiber(&runtime, index, &observed);
        let op_handle = store
            .register(op_spec(index as u64, handle))
            .expect("register")
            .handle();
        tickets.push(
            store
                .dispatch(
                    op_handle,
                    CallbackId::from_bytes(id_bytes(50_000 + index as u64)),
                )
                .expect("dispatch"),
        );
        handles.push(handle);
    }
    wait_until("all fibers parked on their operation wait", || {
        handles
            .iter()
            .all(|handle| runtime.inspect(*handle) == Ok(nlos_runtime::FiberState::WaitingIo))
    })
    .await;

    let reconcile = RecordingReconcileSink::default();
    let pump = OutboxPump::start(
        OutboxConsumer {
            source: StoreOutboxSource::new(Arc::clone(&store)),
            wake_sink: runtime.wake_sink(),
            reconcile_sink: reconcile.clone(),
            config: ConsumerConfig {
                batch_limit: BATCH_LIMIT,
            },
        },
        PumpConfig::default(),
    )
    .expect("spawn outbox pump thread");

    // Writer side: commit every completion back-to-back (the durable wake
    // entry commits in the same transaction), recording each return
    // instant plus the per-complete duration for the writer-side picture.
    let mut commit_at = Vec::with_capacity(BURST);
    let mut commit_durations = Vec::with_capacity(BURST);
    let burst_started = Instant::now();
    for (index, ticket) in tickets.iter().enumerate() {
        let complete_started = Instant::now();
        store
            .complete(
                *ticket,
                CompletionOutcome::Completed {
                    receipt_id: ReceiptId::from_bytes(id_bytes(60_000 + index as u64)),
                },
            )
            .expect("complete");
        commit_durations.push(complete_started.elapsed());
        commit_at.push(Instant::now());
    }
    let commit_wall = burst_started.elapsed();
    let _ = pump.hint();

    wait_until("every fiber observed its wake", || {
        lock(&observed).iter().all(Option::is_some)
    })
    .await;
    let last_observed = lock(&observed)
        .iter()
        .flatten()
        .map(|sample| sample.observed_at)
        .max()
        .expect("at least one sample");
    wait_until("outbox fully acked", || pending_count(&store) == 0).await;
    let health = pump.health();
    pump.stop();
    assert_eq!(health.state, PumpState::Running);
    assert_eq!(health.consecutive_failures, 0, "pump must stay healthy");
    assert!(reconcile.is_empty(), "every entry takes the wake path");
    assert_eq!(pending_count(&store), 0);

    let samples = lock(&observed)
        .iter()
        .copied()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), BURST, "no observation lost");
    assert!(
        samples
            .iter()
            .all(|sample| sample.outcome == WaitOutcome::Woken),
        "every burst entry must deliver exactly one logical wake"
    );

    let mut latencies = samples
        .iter()
        .zip(&commit_at)
        .map(|(sample, committed)| {
            let latency = sample.observed_at.duration_since(*committed);
            assert!(
                latency <= LATENCY_SANITY,
                "latency sanity bound exceeded: {latency:?}"
            );
            latency
        })
        .collect::<Vec<_>>();
    latencies.sort_unstable();

    let mut commit_sorted = commit_durations.clone();
    commit_sorted.sort_unstable();

    let burst_total = last_observed.duration_since(burst_started);
    let wakes_per_second =
        f64::from(u32::try_from(BURST).expect("burst fits u32")) / burst_total.as_secs_f64();

    eprintln!(
        "outbox wake end-to-end wall-clock distribution (burst={BURST}, batch_limit={BATCH_LIMIT}, \
         poll_interval={:?}): latency p50={:?} p90={:?} p99={:?} max={:?} min={:?} | \
         writer complete() p50={:?} p99={:?} max={:?} total_commit_wall={commit_wall:?} | \
         burst_commit_start→last_observe={:?} | throughput: {wakes_per_second:.1} wakes/s",
        PumpConfig::default().poll_interval,
        percentile(&latencies, 50),
        percentile(&latencies, 90),
        percentile(&latencies, 99),
        latencies[BURST - 1],
        latencies[0],
        percentile(&commit_sorted, 50),
        percentile(&commit_sorted, 99),
        commit_sorted[BURST - 1],
        burst_total,
    );
}
