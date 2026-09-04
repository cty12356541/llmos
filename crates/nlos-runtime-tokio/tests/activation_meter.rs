//! Activation metering tests for [`TokioRuntimeAdapter`].
//!
//! Covers dimensional `external_wait` (`WaitingIo`) and `active_cpu` (`Running`)
//! accumulation plus stable post-join readback.

use std::future::pending;
use std::time::{Duration, Instant};

use nlos_runtime::{FiberExit, FiberSpec, FiberState, RuntimeAdapter, WakeOutcome, WakeSink};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ProcessId,
    ResourceGroupId, SchedulerDomainId,
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

fn operation(index: usize) -> OperationId {
    OperationId::from_bytes(id_bytes(index))
}

fn runtime(max_live_fibers: usize) -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(Handle::current(), TokioRuntimeConfig { max_live_fibers })
        .expect("runtime")
}

async fn wait_for_state(
    runtime: &TokioRuntimeAdapter,
    handle: nlos_runtime::FiberHandle,
    expected: FiberState,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if runtime.inspect(handle) == Ok(expected) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fiber did not reach expected state");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operation_wait_accumulates_external_wait_not_active_cpu() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(30));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");
    let sink = runtime.wake_sink();

    let wait = runtime
        .wait_for_operation(handle, operation(1), Generation::INITIAL)
        .expect("wait");
    wait_for_state(&runtime, handle, FiberState::WaitingIo).await;

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        sink.wake(&handle, operation(1), Generation::INITIAL),
        Ok(WakeOutcome::Delivered)
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("wait resolves"),
        WaitOutcome::Woken
    );

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;

    let usage = runtime.activation_usage(handle).expect("usage");
    assert!(
        usage.external_wait >= Duration::from_millis(40),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compute_fiber_records_active_cpu_against_elapsed_wall() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(31));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async {
                let start = Instant::now();
                let mut acc = 0_u64;
                while start.elapsed() < Duration::from_millis(25) {
                    acc = acc.wrapping_add(1);
                }
                let _ = acc;
                FiberExit::Completed
            }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    let usage = runtime.activation_usage(handle).expect("usage");

    assert!(usage.elapsed_wall >= Duration::from_millis(20));
    assert!(usage.active_cpu >= Duration::from_millis(10));
    assert!(usage.active_cpu <= usage.elapsed_wall);
    assert_eq!(usage.external_wait, Duration::ZERO);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_then_activation_usage_readback_is_stable() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(32));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async {
                tokio::task::yield_now().await;
                FiberExit::Completed
            }),
        )
        .expect("spawn");

    let exit = runtime.join_fiber(handle).expect("join");
    assert_eq!(exit, FiberExit::Completed);

    let first = runtime.activation_usage(handle).expect("first readback");
    let second = runtime.activation_usage(handle).expect("second readback");
    assert_eq!(first, second);
    assert!(first.elapsed_wall > Duration::ZERO);
}
