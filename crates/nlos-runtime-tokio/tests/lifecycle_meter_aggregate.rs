//! Lifecycle meter aggregate prefix tests for [`TokioRuntimeAdapter`].
//!
//! Verifies read-side aggregation of `backpressure_wait` and `suspended`
//! dimensions across live fibers via the internal registry.

use std::future::pending;
use std::time::Duration;

use nlos_runtime::{FiberSpec, RuntimeAdapter};
use nlos_runtime_tokio::{LifecycleMeterAggregate, TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, ProcessId, ResourceGroupId,
    SchedulerDomainId,
};
use tokio::runtime::Handle;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregate_sums_backpressure_and_suspended_across_live_fibers() {
    let runtime = runtime(8);
    let scope = CancellationScopeId::from_bytes(id_bytes(50));

    let backpressure_handles: Vec<_> = (0..2)
        .map(|index| {
            runtime
                .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
                .expect("spawn backpressure fiber")
        })
        .collect();
    let suspended_handles: Vec<_> = (2..4)
        .map(|index| {
            runtime
                .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
                .expect("spawn suspended fiber")
        })
        .collect();
    let running_handles: Vec<_> = (4..6)
        .map(|index| {
            runtime
                .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
                .expect("spawn running fiber")
        })
        .collect();

    for handle in &backpressure_handles {
        runtime
            .begin_backpressure_wait(*handle)
            .expect("begin backpressure");
    }
    for handle in &suspended_handles {
        runtime.begin_suspended(*handle).expect("begin suspended");
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut expected_backpressure = Duration::ZERO;
    for handle in &backpressure_handles {
        let usage = runtime
            .activation_usage(*handle)
            .expect("backpressure usage");
        expected_backpressure += usage.backpressure_wait;
    }

    let mut expected_suspended = Duration::ZERO;
    for handle in &suspended_handles {
        let usage = runtime.activation_usage(*handle).expect("suspended usage");
        expected_suspended += usage.suspended;
    }

    let aggregate = runtime.inspect_lifecycle_meter_aggregate();
    assert_eq!(aggregate.sampled_fibers, 6);
    assert!(
        aggregate.total_backpressure_wait >= expected_backpressure,
        "aggregate backpressure_wait={:?} expected at least {:?}",
        aggregate.total_backpressure_wait,
        expected_backpressure
    );
    assert!(
        aggregate.total_suspended >= expected_suspended,
        "aggregate suspended={:?} expected at least {:?}",
        aggregate.total_suspended,
        expected_suspended
    );
    assert!(
        expected_backpressure >= Duration::from_millis(40),
        "expected_backpressure={expected_backpressure:?}"
    );
    assert!(
        expected_suspended >= Duration::from_millis(40),
        "expected_suspended={expected_suspended:?}"
    );

    for handle in &running_handles {
        let usage = runtime.activation_usage(*handle).expect("running usage");
        assert_eq!(usage.backpressure_wait, Duration::ZERO);
        assert_eq!(usage.suspended, Duration::ZERO);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregate_on_empty_registry_is_zero() {
    let runtime = runtime(4);
    let aggregate = runtime.inspect_lifecycle_meter_aggregate();
    assert_eq!(aggregate.sampled_fibers, 0);
    assert_eq!(aggregate.total_backpressure_wait, Duration::ZERO);
    assert_eq!(aggregate.total_suspended, Duration::ZERO);
}

// Spec-pinned nanosecond input: `1_500_000_000 ns → "1.5"` is exactly the
// conversion under test, so the larger-unit suggestion would defeat the test.
#[allow(clippy::duration_suboptimal_units)]
#[test]
fn open_metrics_text_matches_expected_format_snapshot() {
    // Given: a known aggregate (pure function — no runtime needed)
    let aggregate = LifecycleMeterAggregate {
        total_backpressure_wait: Duration::from_nanos(1_500_000_000),
        total_suspended: Duration::from_millis(250),
        sampled_fibers: 3,
    };
    // When: rendering to OpenMetrics text
    let text = aggregate.to_open_metrics_text();
    // Then: exact snapshot, HELP before TYPE before one sample per metric
    assert_eq!(
        text,
        concat!(
            "# HELP nlos_fiber_backpressure_wait_seconds_total Cumulative time live fibers spent in scheduler/admission backpressure wait.\n",
            "# TYPE nlos_fiber_backpressure_wait_seconds_total counter\n",
            "nlos_fiber_backpressure_wait_seconds_total 1.5\n",
            "# HELP nlos_fiber_suspended_seconds_total Cumulative time live fibers spent cooperatively suspended.\n",
            "# TYPE nlos_fiber_suspended_seconds_total counter\n",
            "nlos_fiber_suspended_seconds_total 0.25\n",
            "# HELP nlos_fiber_sampled Number of live fibers sampled by the latest lifecycle meter aggregate inspect.\n",
            "# TYPE nlos_fiber_sampled gauge\n",
            "nlos_fiber_sampled 3\n",
        )
    );
}

#[test]
fn open_metrics_text_on_zero_aggregate_reports_zero_values() {
    // Given: the zero aggregate (what an empty registry inspect yields)
    let aggregate = LifecycleMeterAggregate::default();
    // When: rendering to OpenMetrics text
    let text = aggregate.to_open_metrics_text();
    // Then: every sample reports 0
    assert_eq!(
        text,
        concat!(
            "# HELP nlos_fiber_backpressure_wait_seconds_total Cumulative time live fibers spent in scheduler/admission backpressure wait.\n",
            "# TYPE nlos_fiber_backpressure_wait_seconds_total counter\n",
            "nlos_fiber_backpressure_wait_seconds_total 0\n",
            "# HELP nlos_fiber_suspended_seconds_total Cumulative time live fibers spent cooperatively suspended.\n",
            "# TYPE nlos_fiber_suspended_seconds_total counter\n",
            "nlos_fiber_suspended_seconds_total 0\n",
            "# HELP nlos_fiber_sampled Number of live fibers sampled by the latest lifecycle meter aggregate inspect.\n",
            "# TYPE nlos_fiber_sampled gauge\n",
            "nlos_fiber_sampled 0\n",
        )
    );
}

// Spec-pinned nanosecond input: see the snapshot test above.
#[allow(clippy::duration_suboptimal_units)]
#[test]
fn open_metrics_text_converts_durations_to_fractional_seconds() {
    // Given: 1.5s of backpressure wait and 50ms of suspension in one aggregate
    let aggregate = LifecycleMeterAggregate {
        total_backpressure_wait: Duration::from_nanos(1_500_000_000),
        total_suspended: Duration::from_millis(50),
        sampled_fibers: 1,
    };
    // When: rendering to OpenMetrics text
    let text = aggregate.to_open_metrics_text();
    // Then: nanoseconds → shortest round-trip float seconds, no trailing zeros
    assert!(text.contains("nlos_fiber_backpressure_wait_seconds_total 1.5\n"));
    assert!(text.contains("nlos_fiber_suspended_seconds_total 0.05\n"));
}

#[test]
fn open_metrics_text_preserves_nanosecond_and_large_second_precision() {
    // Given: sub-microsecond wait and a five-digit second total in one aggregate
    let aggregate = LifecycleMeterAggregate {
        total_backpressure_wait: Duration::from_nanos(42),
        total_suspended: Duration::from_secs(100_000),
        sampled_fibers: 0,
    };
    // When: rendering to OpenMetrics text
    let text = aggregate.to_open_metrics_text();
    // Then: nanosecond precision survives; no scientific notation at scale
    assert!(text.contains("nlos_fiber_backpressure_wait_seconds_total 0.000000042\n"));
    assert!(text.contains("nlos_fiber_suspended_seconds_total 100000\n"));
}
