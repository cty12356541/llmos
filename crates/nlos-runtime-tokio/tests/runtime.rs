use std::future::pending;
use std::time::Duration;

use nlos_runtime::{FiberExit, FiberSpec, FiberState, RuntimeAdapter, RuntimeError};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, ProcessId, ResourceGroupId,
    SchedulerDomainId,
};

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
async fn cancellation_is_structured_and_generation_fenced() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 2 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(10));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;

    let stale = nlos_runtime::FiberHandle {
        fiber_id: handle.fiber_id,
        generation: Generation::INITIAL.checked_next().expect("next generation"),
    };
    assert_eq!(runtime.inspect(stale), Err(RuntimeError::InvalidGeneration));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_is_bounded() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 1 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(11));
    let _first = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("first spawn");
    let second = runtime.spawn_fiber(fiber_spec(2, scope), Box::pin(pending()));

    assert_eq!(second, Err(RuntimeError::QueueFull));
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_fiber_records_wall_and_scheduler_time() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 1 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(12));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                FiberExit::Completed
            }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    let usage = runtime.activation_usage(handle).expect("usage");
    assert!(usage.elapsed_wall >= Duration::from_millis(5));
    assert!(usage.scheduler_wait <= Duration::from_secs(1));
    assert!(usage.active_cpu <= usage.elapsed_wall);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_fiber_is_reported_as_failed() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 1 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(13));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { panic!("intentional test panic") }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Failed).await;
}
