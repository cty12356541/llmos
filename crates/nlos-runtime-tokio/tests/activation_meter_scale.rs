//! Explicit Stage B scale probe for ROAD-B-006: dimensional Activation
//! metering (`external_wait` on `WaitingIo`, `active_cpu` on `Running`) must
//! remain correct when many fibers are live on a bounded two-worker runtime.
//!
//! Mirrors the probe style of `scale.rs` and `durable_wait_scale.rs`:
//!
//! 1. **`active_cpu` tier** — `subset` compute fibers run to completion; every
//!    handle must record `active_cpu ≥ threshold` with `external_wait = 0`
//!    (the `active_cpu ≤ elapsed_wall` bound is covered by `activation_meter.rs`
//!    at low concurrency; finalize ordering can invert it by microseconds here).
//! 2. **`external_wait` tier** — `count` fibers park on independent Operation
//!    waits (`WaitingIo`); after a fixed sleep, the first `subset` handles are
//!    sampled for `external_wait` accumulation with `active_cpu` staying small.
//!
//! Both tiers are `#[ignore]`-gated: the regular suite stays green, and the
//! nightly scale-probe CI job can run them via `--include-ignored`.

use std::future::pending;
use std::time::{Duration, Instant};

use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ProcessId,
    ResourceGroupId, SchedulerDomainId,
};
use tokio::runtime::Handle;

/// Quick tier: same shape as the full probe at a tenth of the fiber count.
const QUICK_COUNT: usize = 10_000;
/// The ROAD-B-006 exit-gate scale: one hundred thousand parked Operation waits.
const FULL_COUNT: usize = 100_000;
/// Sampled prefix for metering assertions in both tiers.
const METER_SUBSET: usize = 1_000;
const EXTERNAL_WAIT_SLEEP: Duration = Duration::from_millis(50);
const MIN_EXTERNAL_WAIT: Duration = Duration::from_millis(40);
const MIN_ACTIVE_CPU: Duration = Duration::from_millis(10);
const COMPUTE_TARGET: Duration = Duration::from_millis(25);

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

fn operation(index: usize) -> OperationId {
    OperationId::from_bytes(id_bytes(index))
}

fn runtime(max_live_fibers: usize) -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(Handle::current(), TokioRuntimeConfig { max_live_fibers })
        .expect("runtime")
}

async fn await_all_state(
    runtime: &TokioRuntimeAdapter,
    handles: &[FiberHandle],
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
    .unwrap_or_else(|_| panic!("fibers did not all reach {expected:?}"));
    started.elapsed()
}

/// Phase 1: `subset` compute fibers must still meter `active_cpu` at scale
/// admission limits.
async fn assert_active_cpu_metering_at_scale(subset: usize) -> Duration {
    let runtime = runtime(subset);
    let scope = CancellationScopeId::from_bytes([0x41; 16]);
    let started = Instant::now();
    let handles = (0..subset)
        .map(|index| {
            runtime
                .spawn_fiber(
                    fiber_spec(index, scope),
                    Box::pin(async {
                        let start = Instant::now();
                        let mut acc = 0_u64;
                        while start.elapsed() < COMPUTE_TARGET {
                            acc = acc.wrapping_add(1);
                        }
                        let _ = acc;
                        FiberExit::Completed
                    }),
                )
                .expect("spawn compute fiber")
        })
        .collect::<Vec<_>>();

    let _settle = await_all_state(
        &runtime,
        &handles,
        FiberState::Completed,
        Duration::from_secs(120),
    )
    .await;

    for (index, handle) in handles.iter().enumerate() {
        let usage = runtime.activation_usage(*handle).expect("usage");
        assert!(
            usage.active_cpu >= MIN_ACTIVE_CPU,
            "fiber {index}: active_cpu={:?}",
            usage.active_cpu
        );
        assert_eq!(usage.external_wait, Duration::ZERO);
    }

    started.elapsed()
}

/// Phase 2: `count` Operation-wait fibers park in `WaitingIo`; sample the
/// first `subset` for `external_wait` accumulation.
async fn assert_external_wait_metering_at_scale(count: usize, subset: usize) -> Duration {
    let runtime = runtime(count);
    // One shared cancellation scope: distinct scopes would make the adapter's
    // scope registry insert O(existing scopes) per spawn and turn the probe
    // itself quadratic.
    let scope = CancellationScopeId::from_bytes([0x45; 16]);
    let started = Instant::now();

    let spawn_started = Instant::now();
    let handles = (0..count)
        .map(|index| {
            let handle = runtime
                .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
                .expect("spawn waiting fiber");
            let _wait = runtime
                .wait_for_operation(handle, operation(index), Generation::INITIAL)
                .expect("register operation wait");
            handle
        })
        .collect::<Vec<_>>();
    let spawn_issue = spawn_started.elapsed();
    assert_eq!(runtime.registered_fibers(), count);

    for handle in &handles[..subset.min(handles.len())] {
        assert_eq!(
            runtime.inspect(*handle),
            Ok(FiberState::WaitingIo),
            "begin_wait must flip Operation-wait fibers synchronously at registration"
        );
    }

    tokio::time::sleep(EXTERNAL_WAIT_SLEEP).await;

    let sample_started = Instant::now();
    for handle in &handles[..subset] {
        let usage = runtime.activation_usage(*handle).expect("usage");
        assert!(
            usage.external_wait >= MIN_EXTERNAL_WAIT,
            "external_wait={:?}",
            usage.external_wait
        );
        assert!(
            usage.active_cpu < usage.external_wait,
            "active_cpu={:?} should stay small vs external_wait={:?}",
            usage.active_cpu,
            usage.external_wait
        );
    }
    let sample_elapsed = sample_started.elapsed();

    // Teardown via drop: avoid O(n²) cancel_scope purge at 100K (see §6.3).
    drop(handles);
    drop(runtime);

    let total = started.elapsed();
    eprintln!(
        "{count}-fiber activation-meter profile (2 tokio workers, sample={subset}): \
         spawn_issue={spawn_issue:?} external_wait_sleep={EXTERNAL_WAIT_SLEEP:?} \
         sample_assert={sample_elapsed:?} total={total:?}"
    );
    total
}

async fn run_activation_meter_scale(count: usize, subset: usize) {
    let active_cpu_elapsed = assert_active_cpu_metering_at_scale(subset).await;
    let external_wait_elapsed = assert_external_wait_metering_at_scale(count, subset).await;
    eprintln!(
        "{count}-fiber activation-meter scale probe complete: \
         active_cpu_phase={active_cpu_elapsed:?} external_wait_phase={external_wait_elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit Stage B 10K activation-meter fiber scale probe (quick tier)"]
async fn ten_thousand_activation_meter_fibers_on_two_workers() {
    run_activation_meter_scale(QUICK_COUNT, METER_SUBSET).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit Stage B ROAD-B-006 100K activation-meter fiber scale probe"]
async fn one_hundred_thousand_activation_meter_fibers_on_two_workers() {
    run_activation_meter_scale(FULL_COUNT, METER_SUBSET).await;
}
