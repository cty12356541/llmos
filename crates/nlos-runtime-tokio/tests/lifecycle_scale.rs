//! Explicit Stage B scale probe for ROAD-B-006: lifecycle phases
//! (`backpressure_wait`, `suspended`) must remain correct when many fibers
//! are live on a bounded two-worker runtime.
//!
//! Mirrors the probe style of `activation_meter_scale.rs` and `lifecycle_phase.rs`:
//!
//! 1. **`backpressure_wait` tier** — `count` fibers enter
//!    `FiberLifecyclePhase::BackpressureWait` / `FiberState::WaitingModel`; after a
//!    fixed sleep the first `subset` handles are sampled for dimensional metering
//!    with `external_wait = 0`.
//! 2. **`suspended` tier** — same shape for `FiberLifecyclePhase::Suspended` /
//!    `FiberState::Suspended` with `suspended` accumulation.
//!
//! Both tiers are `#[ignore]`-gated: the regular suite stays green, and the
//! nightly scale-probe CI job can run them via `--include-ignored`.

use std::future::pending;
use std::time::{Duration, Instant};

use nlos_runtime::{FiberSpec, FiberState, RuntimeAdapter};
use nlos_runtime_tokio::{FiberLifecyclePhase, TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, ProcessId, ResourceGroupId,
    SchedulerDomainId,
};
use tokio::runtime::Handle;

const QUICK_COUNT: usize = 10_000;
/// The ROAD-B-006 exit-gate scale: one hundred thousand lifecycle phase fibers.
const FULL_COUNT: usize = 100_000;
const METER_SUBSET: usize = 1_000;
const PHASE_SLEEP: Duration = Duration::from_millis(50);
const MIN_BACKPRESSURE_WAIT: Duration = Duration::from_millis(40);
const MIN_SUSPENDED: Duration = Duration::from_millis(40);
/// Host thread bound independent of fiber count (main + 2 tokio workers + probe slack).
const THREAD_BOUND: usize = 10;

fn id_bytes(value: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(value as u64).to_be_bytes());
    bytes
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

fn runtime(max_live_fibers: usize) -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(Handle::current(), TokioRuntimeConfig { max_live_fibers })
        .expect("runtime")
}

async fn await_all_lifecycle_phase(
    runtime: &TokioRuntimeAdapter,
    handles: &[nlos_runtime::FiberHandle],
    expected: FiberLifecyclePhase,
    budget: Duration,
) -> Duration {
    let started = Instant::now();
    tokio::time::timeout(budget, async {
        loop {
            if handles.iter().all(|handle| {
                runtime.inspect_lifecycle_phase(*handle) == Ok(expected)
            }) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fibers did not all reach lifecycle phase {expected:?}"));
    started.elapsed()
}

async fn await_all_state(
    runtime: &TokioRuntimeAdapter,
    handles: &[nlos_runtime::FiberHandle],
    expected: FiberState,
    budget: Duration,
) -> Duration {
    let started = Instant::now();
    tokio::time::timeout(budget, async {
        loop {
            if handles
                .iter()
                .all(|handle| runtime.inspect(*handle) == Ok(expected))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fibers did not all reach state {expected:?}"));
    started.elapsed()
}

async fn assert_backpressure_wait_at_scale(count: usize, subset: usize) -> Duration {
    let runtime = runtime(count);
    let scope = CancellationScopeId::from_bytes([0x42; 16]);
    let started = Instant::now();

    let spawn_started = Instant::now();
    let handles = (0..count)
        .map(|index| {
            runtime
                .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
                .expect("spawn fiber")
        })
        .collect::<Vec<_>>();
    let spawn_issue = spawn_started.elapsed();
    assert_eq!(runtime.registered_fibers(), count);

    let enter_started = Instant::now();
    for handle in &handles {
        runtime
            .begin_backpressure_wait(*handle)
            .expect("begin backpressure");
    }
    let enter_elapsed = enter_started.elapsed();

    let park_settle = await_all_lifecycle_phase(
        &runtime,
        &handles,
        FiberLifecyclePhase::BackpressureWait,
        Duration::from_secs(30),
    )
    .await;

    for handle in &handles[..subset] {
        assert_eq!(
            runtime.inspect_lifecycle_phase(*handle).expect("phase"),
            FiberLifecyclePhase::BackpressureWait
        );
        assert_eq!(
            runtime.inspect(*handle).expect("state"),
            FiberState::WaitingModel
        );
    }

    tokio::time::sleep(PHASE_SLEEP).await;

    let rss_kib = process_rss_kib();
    let threads = process_thread_count();
    assert!(
        threads <= THREAD_BOUND,
        "host threads {threads} exceed fiber-count-independent bound {THREAD_BOUND}"
    );

    let sample_started = Instant::now();
    for (index, handle) in handles[..subset].iter().enumerate() {
        let usage = runtime.activation_usage(*handle).expect("usage");
        assert!(
            usage.backpressure_wait >= MIN_BACKPRESSURE_WAIT,
            "fiber {index}: backpressure_wait={:?}",
            usage.backpressure_wait
        );
        assert_eq!(usage.external_wait, Duration::ZERO);
        // Prefix fibers accumulate spawn-window active_cpu at 100K; dimensional
        // separation is covered at 10K (`lifecycle_phase.rs` + quick tier).
        if count <= QUICK_COUNT {
            assert!(
                usage.active_cpu < usage.backpressure_wait,
                "fiber {index}: active_cpu={:?} should stay small vs backpressure_wait={:?}",
                usage.active_cpu,
                usage.backpressure_wait
            );
        }
    }
    let sample_elapsed = sample_started.elapsed();

    for handle in &handles {
        runtime
            .resume_from_backpressure_wait(*handle)
            .expect("resume backpressure");
    }
    let resume_settle = await_all_lifecycle_phase(
        &runtime,
        &handles,
        FiberLifecyclePhase::Running,
        Duration::from_secs(30),
    )
    .await;

    drop(handles);
    drop(runtime);

    let total = started.elapsed();
    eprintln!(
        "{count}-fiber backpressure_wait profile (2 tokio workers, sample={subset}): \
         spawn_issue={spawn_issue:?} enter_backpressure={enter_elapsed:?} \
         park_settle={park_settle:?} phase_sleep={PHASE_SLEEP:?} sample_assert={sample_elapsed:?} \
         resume_settle={resume_settle:?} rss_kib={rss_kib} threads={threads} total={total:?}"
    );
    total
}

async fn assert_suspended_at_scale(count: usize, subset: usize) -> Duration {
    let runtime = runtime(count);
    let scope = CancellationScopeId::from_bytes([0x53; 16]);
    let started = Instant::now();

    let spawn_started = Instant::now();
    let handles = (0..count)
        .map(|index| {
            runtime
                .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
                .expect("spawn fiber")
        })
        .collect::<Vec<_>>();
    let spawn_issue = spawn_started.elapsed();
    assert_eq!(runtime.registered_fibers(), count);

    let enter_started = Instant::now();
    for handle in &handles {
        runtime.begin_suspended(*handle).expect("begin suspended");
    }
    let enter_elapsed = enter_started.elapsed();

    let park_settle = await_all_lifecycle_phase(
        &runtime,
        &handles,
        FiberLifecyclePhase::Suspended,
        Duration::from_secs(30),
    )
    .await;

    for handle in &handles[..subset] {
        assert_eq!(
            runtime.inspect_lifecycle_phase(*handle).expect("phase"),
            FiberLifecyclePhase::Suspended
        );
        assert_eq!(
            runtime.inspect(*handle).expect("state"),
            FiberState::Suspended
        );
    }

    tokio::time::sleep(PHASE_SLEEP).await;

    let threads = process_thread_count();
    assert!(
        threads <= THREAD_BOUND,
        "host threads {threads} exceed fiber-count-independent bound {THREAD_BOUND}"
    );

    let sample_started = Instant::now();
    for (index, handle) in handles[..subset].iter().enumerate() {
        let usage = runtime.activation_usage(*handle).expect("usage");
        assert!(
            usage.suspended >= MIN_SUSPENDED,
            "fiber {index}: suspended={:?}",
            usage.suspended
        );
        assert_eq!(usage.external_wait, Duration::ZERO);
        assert_eq!(usage.backpressure_wait, Duration::ZERO);
    }
    let sample_elapsed = sample_started.elapsed();

    for handle in &handles {
        runtime
            .resume_from_suspended(*handle)
            .expect("resume suspended");
    }
    let resume_settle = await_all_state(
        &runtime,
        &handles,
        FiberState::Running,
        Duration::from_secs(30),
    )
    .await;

    drop(handles);
    drop(runtime);

    let total = started.elapsed();
    eprintln!(
        "{count}-fiber suspended profile (2 tokio workers, sample={subset}): \
         spawn_issue={spawn_issue:?} enter_suspended={enter_elapsed:?} \
         park_settle={park_settle:?} phase_sleep={PHASE_SLEEP:?} sample_assert={sample_elapsed:?} \
         resume_settle={resume_settle:?} threads={threads} total={total:?}"
    );
    total
}

async fn run_lifecycle_scale(count: usize, subset: usize) {
    let backpressure_elapsed = assert_backpressure_wait_at_scale(count, subset).await;
    let suspended_elapsed = assert_suspended_at_scale(count, subset).await;
    eprintln!(
        "{count}-fiber lifecycle scale probe complete: \
         backpressure_wait_phase={backpressure_elapsed:?} suspended_phase={suspended_elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit Stage B 10K lifecycle phase fiber scale probe (backpressure_wait + suspended)"]
async fn ten_thousand_lifecycle_phase_fibers_on_two_workers() {
    run_lifecycle_scale(QUICK_COUNT, METER_SUBSET).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit Stage B ROAD-B-006 100K lifecycle phase fiber scale probe (backpressure_wait + suspended)"]
async fn one_hundred_thousand_lifecycle_phase_fibers_on_two_workers() {
    run_lifecycle_scale(FULL_COUNT, METER_SUBSET).await;
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
