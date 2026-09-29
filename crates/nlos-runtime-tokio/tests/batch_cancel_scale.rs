//! 100K-tier batch-cancel scale probe (W35-P6 / B-RUNTIME-002 §6.16.2,
//! ROAD-B-006): one batch cancel per process must drive a large
//! multi-process, multi-scope cohort to unique `Cancelled` terminals,
//! already-terminal fibers must keep their own terminal, and the join/reap
//! sweep must reclaim the whole registry — with the cancel-phase cost
//! family proven linear-or-better across tiers by a per-fiber ratio, not an
//! absolute wall-clock bound.
//!
//! Cohort shape per tier (`count` fibers across `PROCESSES` processes, each
//! process holding `SCOPES_PER_PROCESS` shared scopes):
//!
//! - `TERMINAL_PER_PROCESS` fibers pre-completed (`Completed`) — the batch
//!   must count them as `already_terminal` and never rewrite them;
//! - `WAITERS` fibers (tier-invariant, 1000 total across all tiers) parked
//!   in-fiber on an Operation wait — the terminal purge runs against a
//!   NON-EMPTY wait registry at scale. Held CONSTANT across tiers so the
//!   registered O(waits²) purge family contributes a fixed cost to both
//!   tiers and the 10K→100K ratio isolates the sweep/cancel/reap family
//!   (its scaling stays registered, unchanged, in §5/§6.19);
//! - the remainder parked on `pending()` (running).
//!
//! Assertions:
//!
//! 1. unique terminals: sum of per-process `matched_fibers` == count, the
//!    pre-completed fibers keep `Completed`, every other fiber settles
//!    `Cancelled` (exact per-state census, no third state), and every join
//!    returns the matching exit — nothing lost, duplicated, or rewritten;
//! 2. registry reclaim (the deterministic hard bound): after the joins,
//!    `registered_fibers() == 0` and `registered_scopes() == 0` — joins reap
//!    synchronously, so the reap window is the join loop itself;
//! 3. memory stays population-proportional: RSS growth at the full cohort
//!    is bounded by `RSS_PER_FIBER_KIB × count` (W8-era measurement was
//!    ~2KiB/fiber; 8KiB is a 4× headroom population-proportional bound —
//!    no absolute MBs, no runner-core assumptions, §6.18 discipline);
//! 4. thread count is fiber-count independent: growth over the in-tier
//!    baseline is bounded by the runtime's 2 workers plus slack;
//! 5. O-family ratio (the CI-profile-honest scaling bound): the per-fiber
//!    cancel-phase cost (linkage + settle + join) at 100K must not exceed
//!    `SUPERLINEAR_RATIO_BOUND ×` the per-fiber cost at 10K measured in the
//!    SAME process. Linear scaling keeps the ratio ≈1; the registered
//!    O(n²) failure family would push it toward the population ratio (10×),
//!    far past the 3.0 bound. A ratio, not an absolute duration, is the
//!    contract — it is insensitive to runner speed, core count, and disk.
//!
//! Both tiers are `#[ignore]`-gated and share a serialization slot with the
//! §6.18 discipline: with `--include-ignored` the two tests in this binary
//! would otherwise run in parallel inside one process and poison each
//! other's RSS/thread/timing deltas.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nlos_process::{
    CreateIsolationDomainRequest, IsolationDomainDecision, ProcessAuthority,
    ProcessBindingDecision, ProcessLifecycleState, PropagateCancelToFibersRequest,
    PropagateCrashRequest, RegisterDelegatedProcessRequest,
};
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter};
use nlos_runtime_tokio::{
    ProcessFiberCancelReport, TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, ProcessId, ResourceGroupId, SchedulerDomainId, TaskAttemptId, TaskId,
};
use tokio::runtime::Handle;

const WORKER_THREADS: usize = 2;
/// Cohort fan-out: 4 processes × 16 scopes each at every tier.
const PROCESSES: usize = 4;
const SCOPES_PER_PROCESS: usize = 16;
/// Tier-invariant wait-bearing subset: exercises the terminal purge against
/// a non-empty wait registry without letting the registered O(waits²)
/// family contaminate the tier ratio.
const WAITERS: usize = 1_000;
const TERMINAL_PER_PROCESS: usize = 4;
const TERMINAL_TOTAL: usize = TERMINAL_PER_PROCESS * PROCESSES;
const QUICK_COUNT: usize = 10_000;
/// The ROAD-B-006 exit-gate scale: one hundred thousand batch-cancelled fibers.
const FULL_COUNT: usize = 100_000;
/// Population-proportional RSS bound at the full cohort (W8 measured ~2KiB
/// per fiber; 4× headroom, no absolute-MB or runner assumptions).
const RSS_PER_FIBER_KIB: u64 = 8;
/// Allowed host-thread growth over the in-tier baseline: the probe runtime's
/// own workers plus slack. Batch cancel must never spawn threads.
const THREAD_GROWTH_BOUND: usize = WORKER_THREADS + 2;
/// Per-fiber cancel-phase cost at 100K may be at most 3× the 10K tier's
/// (same process). Linear ⇒ ≈1×; the O(n²) family ⇒ ≈10× at a 10×
/// population. Ratio bound, not wall-clock.
const SUPERLINEAR_RATIO_BOUND: f64 = 3.0;

static PROBE_SERIALIZE: Mutex<()> = Mutex::new(());

fn acquire_probe_slot() -> MutexGuard<'static, ()> {
    // A panicked probe poisons the slot without corrupting shared state;
    // the other tier still deserves its own measurement window.
    PROBE_SERIALIZE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn bounded_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()
        .expect("bounded runtime")
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
            "nlos-runtime-tokio-batch-cancel-scale-{label}-{}-{nonce}-{sequence}",
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

fn id_bytes(value: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(value as u64).to_be_bytes());
    bytes
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

/// One registered process binding under which a slice of the cohort is linked.
struct ProcessFixture {
    process_id: ProcessId,
    process_generation: Generation,
    process_fencing_token: nlos_process::FencingToken,
    crash_key_seed: u8,
}

/// Registers `PROCESSES` delegated processes under one authority (distinct
/// isolation domains and idempotency keys), mirroring the W27-C fixture.
fn open_process_fixtures(root: &Root) -> (ProcessAuthority, Vec<ProcessFixture>) {
    let process = ProcessAuthority::open(root.path()).expect("open process");
    let mut fixtures = Vec::with_capacity(PROCESSES);
    for index in 0..PROCESSES {
        let seed = 0x10 + u8::try_from(index * 0x08).expect("seed fits u8");
        let domain = match process.create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [seed; 32],
            idempotency_key: key(seed.wrapping_add(1)),
            created_at_ms: 1_000,
        }) {
            Ok(
                IsolationDomainDecision::Created(record)
                | IsolationDomainDecision::Replayed(record),
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
                ProcessBindingDecision::Registered(record)
                | ProcessBindingDecision::Replayed(record),
            ) => record,
            Err(error) => panic!("register process: {error}"),
        };
        fixtures.push(ProcessFixture {
            process_id: binding.process_id,
            process_generation: binding.process_generation,
            process_fencing_token: binding.process_fencing_token,
            crash_key_seed: 0x70 + u8::try_from(index).expect("seed fits u8"),
        });
    }
    (process, fixtures)
}

/// Tier cohort layout: fiber index → (process slot, scope slot, role).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Running,
    Waiter,
    PreTerminal,
}

fn role_of(index: usize, fibers_per_process: usize) -> Role {
    let in_process = index % fibers_per_process;
    if in_process < TERMINAL_PER_PROCESS {
        Role::PreTerminal
    } else if in_process < TERMINAL_PER_PROCESS + WAITERS / PROCESSES {
        Role::Waiter
    } else {
        Role::Running
    }
}

fn spec_for(index: usize, fibers_per_process: usize, fixture: &ProcessFixture) -> FiberSpec {
    let process_slot = index / fibers_per_process;
    let scope_slot = index % SCOPES_PER_PROCESS;
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: AgentInstanceId::from_bytes(id_bytes(index)),
        agent_generation: Generation::INITIAL,
        process_id: fixture.process_id,
        process_generation: fixture.process_generation,
        task_attempt_id: None,
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(
            process_slot * SCOPES_PER_PROCESS + scope_slot,
        )),
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(id_bytes(1)),
        scheduler_domain_id: SchedulerDomainId::from_bytes(id_bytes(1)),
        deadline: None,
    }
}

/// In-fiber Operation wait park: registers in its own task body (in-memory
/// wait registry, no durable rows) and exits with the wait outcome.
async fn park_on_operation_wait(
    adapter: TokioRuntimeAdapter,
    handle: FiberHandle,
    operation_id: OperationId,
) -> FiberExit {
    let wait = adapter
        .wait_for_operation(handle, operation_id, Generation::INITIAL)
        .expect("in-fiber operation wait registration");
    match wait.await {
        WaitOutcome::Woken => FiberExit::Completed,
        WaitOutcome::Cancelled => FiberExit::Cancelled,
    }
}

/// Per-tier measurement record; durations feed the ratio, RSS/threads are
/// evidence readouts (only the population-proportional and growth bounds
/// above are asserted).
struct TierProfile {
    count: usize,
    spawn_issue: Duration,
    cancel_phase: Duration,
    settle: Duration,
    join_reap: Duration,
    rss_before_kib: u64,
    rss_ready_kib: u64,
    rss_after_reap_kib: u64,
    threads_baseline: usize,
    threads_at_settle: usize,
}

impl TierProfile {
    #[allow(clippy::cast_precision_loss)] // Ratio display only: nanos/count into f64.
    fn per_fiber_cancel_phase(&self) -> f64 {
        self.cancel_phase.as_nanos() as f64 / self.count as f64
    }
}

/// State census over the whole cohort.
struct Census {
    running: usize,
    waiting_io: usize,
    completed: usize,
    cancelled: usize,
    other: usize,
}

fn census(runtime: &TokioRuntimeAdapter, handles: &[FiberHandle]) -> Census {
    let mut counts = Census {
        running: 0,
        waiting_io: 0,
        completed: 0,
        cancelled: 0,
        other: 0,
    };
    for handle in handles {
        match runtime.inspect(*handle) {
            Ok(FiberState::Running) => counts.running += 1,
            Ok(FiberState::WaitingIo) => counts.waiting_io += 1,
            Ok(FiberState::Completed) => counts.completed += 1,
            Ok(FiberState::Cancelled) => counts.cancelled += 1,
            _ => counts.other += 1,
        }
    }
    counts
}

async fn await_census(
    runtime: &TokioRuntimeAdapter,
    handles: &[FiberHandle],
    budget: Duration,
    expect_waiting_io: usize,
    expect_completed: usize,
) -> Census {
    tokio::time::timeout(budget, async {
        loop {
            let counts = census(runtime, handles);
            if counts.waiting_io >= expect_waiting_io && counts.completed >= expect_completed {
                return counts;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "cohort did not reach {expect_waiting_io} WaitingIo / {expect_completed} Completed \
             within {budget:?}"
        )
    })
}

#[allow(clippy::too_many_lines)] // One tier profile covers spawn, census, batch cancel, settle, join, reclaim.
async fn run_batch_cancel_tier(count: usize) -> TierProfile {
    let fibers_per_process = count / PROCESSES;
    assert_eq!(
        fibers_per_process * PROCESSES,
        count,
        "cohort must divide evenly across {PROCESSES} processes"
    );
    assert!(
        fibers_per_process > TERMINAL_PER_PROCESS + WAITERS / PROCESSES,
        "tier too small for the fixed composition"
    );

    let root = Root::new(&format!("{count}"));
    let (process, fixtures) = open_process_fixtures(&root);
    let rss_before_kib = process_rss_kib();
    let runtime = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: count,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime");
    let threads_baseline = process_thread_count();

    let spawn_started = Instant::now();
    let mut handles = Vec::with_capacity(count);
    for index in 0..count {
        let fixture = &fixtures[index / fibers_per_process];
        let spec = spec_for(index, fibers_per_process, fixture);
        let body = match role_of(index, fibers_per_process) {
            Role::Running => Box::pin(pending()) as nlos_runtime::FiberFuture,
            Role::Waiter => {
                let handle = FiberHandle {
                    fiber_id: spec.fiber_id,
                    generation: spec.fiber_generation,
                };
                let operation_id = OperationId::from_bytes(id_bytes(20_000 + index));
                Box::pin(park_on_operation_wait(
                    runtime.clone(),
                    handle,
                    operation_id,
                ))
            }
            Role::PreTerminal => Box::pin(async { FiberExit::Completed }),
        };
        handles.push(
            runtime
                .spawn_fiber(spec, body)
                .unwrap_or_else(|error| panic!("spawn fiber {index}: {error:?}")),
        );
    }
    let spawn_issue = spawn_started.elapsed();
    assert_eq!(runtime.registered_fibers(), count);

    // Cohort readiness: pre-terminal fibers Completed, waiters parked
    // WaitingIo (their waits entries are live in the registry).
    let ready = await_census(
        &runtime,
        &handles,
        Duration::from_mins(1),
        WAITERS,
        TERMINAL_TOTAL,
    )
    .await;
    assert_eq!(ready.completed, TERMINAL_TOTAL, "pre-terminal census");
    assert_eq!(ready.waiting_io, WAITERS, "waiter census");
    assert_eq!(
        ready.running,
        count - TERMINAL_TOTAL - WAITERS,
        "running census"
    );
    assert_eq!(ready.cancelled, 0);
    assert_eq!(ready.other, 0);

    let rss_ready_kib = process_rss_kib();
    let rss_growth_kib = rss_ready_kib.saturating_sub(rss_before_kib);
    let rss_bound_kib = RSS_PER_FIBER_KIB * count as u64;
    assert!(
        rss_growth_kib <= rss_bound_kib,
        "RSS growth {rss_growth_kib} KiB at {count} fibers exceeds the \
         population-proportional bound {rss_bound_kib} KiB"
    );

    // Batch cancel: crash-propagate then linkage, one per process.
    let cancel_started = Instant::now();
    let mut linkage = Duration::ZERO;
    let mut matched_total = 0;
    let mut already_terminal_total = 0;
    let mut canceled_scopes_total = 0;
    for fixture in &fixtures {
        process
            .propagate_crash(PropagateCrashRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: fixture.process_fencing_token,
                idempotency_key: key(fixture.crash_key_seed),
                marked_at_ms: 9_000,
            })
            .expect("propagate crash");
        let call_started = Instant::now();
        let report: ProcessFiberCancelReport = runtime
            .cancel_process_fibers(
                &process,
                PropagateCancelToFibersRequest {
                    process_id: fixture.process_id,
                    expected_process_generation: fixture.process_generation,
                    expected_process_fencing_token: fixture.process_fencing_token,
                    lifecycle_state: ProcessLifecycleState::Crashed,
                    idempotency_key: key(fixture.crash_key_seed),
                    cancelled_at_ms: 9_000,
                },
            )
            .expect("batch cancel linkage");
        linkage += call_started.elapsed();
        assert_eq!(
            report.matched_fibers, fibers_per_process,
            "matched per process"
        );
        assert_eq!(
            report.already_terminal, TERMINAL_PER_PROCESS,
            "already-terminal per process"
        );
        assert_eq!(
            report.canceled_scopes, SCOPES_PER_PROCESS,
            "cancelled scopes per process"
        );
        assert_eq!(report.vanished_scopes, 0);
        matched_total += report.matched_fibers;
        already_terminal_total += report.already_terminal;
        canceled_scopes_total += report.canceled_scopes;
    }
    assert_eq!(
        matched_total, count,
        "no fiber lost or duplicated by the sweep"
    );
    assert_eq!(already_terminal_total, TERMINAL_TOTAL);
    assert_eq!(canceled_scopes_total, PROCESSES * SCOPES_PER_PROCESS);

    let settle_started = Instant::now();
    let settle_budget = Duration::from_secs(30 + u64::try_from(count / 1_000).expect("budget"));
    let settled = tokio::time::timeout(settle_budget, async {
        loop {
            let counts = census(&runtime, &handles);
            if counts.cancelled + counts.completed == count {
                return counts;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("cohort did not settle terminal within {settle_budget:?}"));
    let settle = settle_started.elapsed();
    assert_eq!(
        settled.cancelled,
        count - TERMINAL_TOTAL,
        "every non-pre-terminal fiber reaches the unique Cancelled terminal"
    );
    assert_eq!(
        settled.completed, TERMINAL_TOTAL,
        "already-terminal fibers keep their own terminal"
    );
    assert_eq!(settled.running + settled.waiting_io + settled.other, 0);

    let threads_at_settle = process_thread_count();
    assert!(
        threads_at_settle <= threads_baseline + THREAD_GROWTH_BOUND,
        "host threads {threads_at_settle} exceed the fiber-count-independent \
         growth bound over baseline {threads_baseline} (+{THREAD_GROWTH_BOUND} allowed)"
    );

    // Join/consume every fiber: exact exit census, synchronous reap.
    let join_started = Instant::now();
    let mut cancelled_exits = 0;
    let mut completed_exits = 0;
    for (index, handle) in handles.iter().enumerate() {
        let exit = runtime
            .join_fiber(*handle)
            .unwrap_or_else(|error| panic!("join fiber {index}: {error:?}"));
        match exit {
            FiberExit::Cancelled => cancelled_exits += 1,
            FiberExit::Completed => completed_exits += 1,
            FiberExit::Failed => panic!("fiber {index} joined with unexpected exit Failed"),
        }
    }
    let join_reap = join_started.elapsed();
    assert_eq!(cancelled_exits, count - TERMINAL_TOTAL, "Cancelled exits");
    assert_eq!(completed_exits, TERMINAL_TOTAL, "Completed exits");

    // Registry reclaim: the deterministic hard bound — the reap window is
    // the join loop itself, so both registries must be exactly empty now.
    assert_eq!(
        runtime.registered_fibers(),
        0,
        "every joined fiber must be reaped from the registry"
    );
    assert_eq!(
        runtime.registered_scopes(),
        0,
        "every cohort scope must be released after its last fiber reaped"
    );
    let rss_after_reap_kib = process_rss_kib();

    let profile = TierProfile {
        count,
        spawn_issue,
        cancel_phase: cancel_started.elapsed(),
        settle,
        join_reap,
        rss_before_kib,
        rss_ready_kib,
        rss_after_reap_kib,
        threads_baseline,
        threads_at_settle,
    };
    eprintln!(
        "{count}-fiber batch-cancel profile (workers={WORKER_THREADS}, processes={PROCESSES}, \
         scopes/process={SCOPES_PER_PROCESS}, waiters={WAITERS}, pre-terminal={TERMINAL_TOTAL}): \
         spawn_issue={:?} linkage={linkage:?} settle={settle:?} join_reap={join_reap:?} \
         per_fiber_cancel_phase={:.3}µs rss_kib before={} ready={} after_reap={} \
         (growth {} ≤ bound {}, proportional {RSS_PER_FIBER_KIB} KiB/fiber) \
         threads {} -> {} (growth bound +{THREAD_GROWTH_BOUND})",
        profile.spawn_issue,
        profile.per_fiber_cancel_phase() / 1_000.0,
        profile.rss_before_kib,
        profile.rss_ready_kib,
        profile.rss_after_reap_kib,
        rss_growth_kib,
        rss_bound_kib,
        profile.threads_baseline,
        profile.threads_at_settle,
    );
    profile
}

#[test]
#[ignore = "explicit Stage B 10K batch-cancel scale probe (quick tier)"]
fn ten_thousand_batch_cancelled_fibers_across_processes() {
    let _slot = acquire_probe_slot();
    bounded_runtime().block_on(async {
        run_batch_cancel_tier(QUICK_COUNT).await;
    });
}

#[test]
#[ignore = "explicit Stage B ROAD-B-006 100K batch-cancel scale probe (full tier, includes the 10K ratio reference tier)"]
fn one_hundred_thousand_batch_cancelled_fibers_scale_linearly_or_better() {
    let _slot = acquire_probe_slot();
    bounded_runtime().block_on(async {
        let quick = run_batch_cancel_tier(QUICK_COUNT).await;
        let full = run_batch_cancel_tier(FULL_COUNT).await;
        let ratio = full.per_fiber_cancel_phase() / quick.per_fiber_cancel_phase();
        assert!(
            ratio <= SUPERLINEAR_RATIO_BOUND,
            "per-fiber cancel-phase cost grew superlinearly: {ratio:.2}x from 10K \
             ({:.3}µs) to 100K ({:.3}µs), bound {SUPERLINEAR_RATIO_BOUND}x",
            quick.per_fiber_cancel_phase() / 1_000.0,
            full.per_fiber_cancel_phase() / 1_000.0,
        );
        eprintln!(
            "batch-cancel O-family ratio: per-fiber cancel phase {:.3}µs @10K vs {:.3}µs @100K \
             → {ratio:.2}x (linear-or-better bound {SUPERLINEAR_RATIO_BOUND}x); \
             phase breakdown 10K settle={:?} join_reap={:?}, 100K settle={:?} join_reap={:?}",
            quick.per_fiber_cancel_phase() / 1_000.0,
            full.per_fiber_cancel_phase() / 1_000.0,
            quick.settle,
            quick.join_reap,
            full.settle,
            full.join_reap,
        );
    });
}

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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_rss_kib() -> u64 {
    0
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_thread_count() -> usize {
    0
}
