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
/// 饱和门 = 每纤 wait 地板(`MIN_BACKPRESSURE_WAIT`)本身,非新魔数:零功
/// 负载(纤只 spawn→入等)的每纤平均 active 全部是 spawn 窗口排队时延
/// (墙钟段可重叠),亚地板(本机实测 36ms<40ms)时「等待主导」比值有
/// 信息量照常断言;一旦均值侵入 wait 地板区间(CI 实测 ~87ms/纤,run
/// 35559126262)即被排队时延支配,带理由跳过(§6.18 家族第三例)。
const SATURATION_ACTIVE_PER_FIBER: Duration = MIN_BACKPRESSURE_WAIT;
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
    TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers,
            ..TokioRuntimeConfig::default()
        },
    )
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
            if handles
                .iter()
                .all(|handle| runtime.inspect_lifecycle_phase(*handle) == Ok(expected))
            {
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

#[allow(clippy::too_many_lines)] // 探针主体:spawn/采样/断言一体,拆分反而伤可读性
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

    let backpressure_aggregate = runtime.inspect_lifecycle_meter_aggregate();
    assert_eq!(backpressure_aggregate.sampled_fibers, count);
    let fiber_count = u32::try_from(count).expect("scale probe fiber count fits u32");
    assert!(
        backpressure_aggregate.total_backpressure_wait >= MIN_BACKPRESSURE_WAIT * fiber_count,
        "aggregate backpressure_wait={:?} for {count} fibers",
        backpressure_aggregate.total_backpressure_wait
    );

    let sample_started = Instant::now();
    // 「等待主导」比值断言是**有主机余量时**的形状断言(§6.18 家族第三例
    // 两轮裁决,run 35558019156/35559126262):meter 的 active_cpu 按墙钟段
    // 计,饱和主机上调度器排队时延(spawn→首 poll→入等)被计入 active——
    // 逐纤与队列聚合两种形式在 2-worker×10K 人口的 CI runner 上均倒挂
    // (127/66ms 每纤;聚合 101.1s/75.2s)。以零功负载的每纤平均
    // active 作饱和门(功≈0 ⇒ active≈纯排队时延):亚阈(健康)才断言比值,
    // 达阈(饱和)带理由跳过——跳过理由打印进日志,非静默。每纤 MIN 下限、
    // external_wait=0、线程/RSS 界与聚合下限无条件保留(CI 的真正载荷)。
    let mut subset_total_active = Duration::ZERO;
    let mut subset_total_wait = Duration::ZERO;
    for (index, handle) in handles[..subset].iter().enumerate() {
        let usage = runtime.activation_usage(*handle).expect("usage");
        assert!(
            usage.backpressure_wait >= MIN_BACKPRESSURE_WAIT,
            "fiber {index}: backpressure_wait={:?}",
            usage.backpressure_wait
        );
        assert_eq!(usage.external_wait, Duration::ZERO);
        subset_total_active += usage.active_cpu;
        subset_total_wait += usage.backpressure_wait;
    }
    if count <= QUICK_COUNT {
        let fiber_count_subset = u32::try_from(subset).expect("subset fits u32");
        let avg_active = subset_total_active / fiber_count_subset;
        if avg_active < SATURATION_ACTIVE_PER_FIBER {
            assert!(
                subset_total_active < subset_total_wait,
                "cohort active_cpu={subset_total_active:?} should stay below cohort backpressure_wait={subset_total_wait:?} (waiting-dominated population)",
            );
        } else {
            println!(
                "SKIP ratio assert (spawn-window saturated): avg active={avg_active:?}/fiber >= {SATURATION_ACTIVE_PER_FIBER:?} — zero-work population's active is scheduler queue latency; cohort active={subset_total_active:?} vs wait={subset_total_wait:?}; per-fiber floors still asserted"
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
         resume_settle={resume_settle:?} rss_kib={rss_kib} threads={threads} \
         aggregate_backpressure_wait={:?} aggregate_sampled_fibers={} total={total:?}",
        backpressure_aggregate.total_backpressure_wait, backpressure_aggregate.sampled_fibers,
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

    let suspended_aggregate = runtime.inspect_lifecycle_meter_aggregate();
    assert_eq!(suspended_aggregate.sampled_fibers, count);
    let fiber_count = u32::try_from(count).expect("scale probe fiber count fits u32");
    assert!(
        suspended_aggregate.total_suspended >= MIN_SUSPENDED * fiber_count,
        "aggregate suspended={:?} for {count} fibers",
        suspended_aggregate.total_suspended
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
         resume_settle={resume_settle:?} threads={threads} \
         aggregate_suspended={:?} aggregate_sampled_fibers={} total={total:?}",
        suspended_aggregate.total_suspended, suspended_aggregate.sampled_fibers,
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
