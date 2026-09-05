//! Lifecycle phase prefix tests for [`TokioRuntimeAdapter`].
//!
//! Covers `FiberLifecyclePhase::{BackpressureWait,Suspended}` exposure at the
//! scheduler boundary and dimensional metering for `backpressure_wait` /
//! `suspended`.

use std::future::pending;
use std::time::Duration;

use nlos_runtime::{FiberSpec, FiberState, RuntimeAdapter};
use nlos_runtime_tokio::{FiberLifecyclePhase, TokioRuntimeAdapter, TokioRuntimeConfig};
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
async fn backpressure_wait_exposes_lifecycle_phase_and_fiber_state() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(40));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    assert_eq!(
        runtime.inspect_lifecycle_phase(handle).expect("phase"),
        FiberLifecyclePhase::Running
    );

    runtime
        .begin_backpressure_wait(handle)
        .expect("backpressure");
    assert_eq!(
        runtime.inspect_lifecycle_phase(handle).expect("phase"),
        FiberLifecyclePhase::BackpressureWait
    );
    assert_eq!(
        runtime.inspect(handle).expect("state"),
        FiberState::WaitingModel
    );

    runtime
        .resume_from_backpressure_wait(handle)
        .expect("resume backpressure");
    assert_eq!(
        runtime.inspect_lifecycle_phase(handle).expect("phase"),
        FiberLifecyclePhase::Running
    );
    assert_eq!(runtime.inspect(handle).expect("state"), FiberState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backpressure_wait_accumulates_backpressure_not_external_wait() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(41));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    runtime
        .begin_backpressure_wait(handle)
        .expect("backpressure");
    tokio::time::sleep(Duration::from_millis(50)).await;
    runtime
        .resume_from_backpressure_wait(handle)
        .expect("resume backpressure");

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    while runtime.inspect(handle) != Ok(FiberState::Cancelled) {
        tokio::task::yield_now().await;
    }

    let usage = runtime.activation_usage(handle).expect("usage");
    assert!(
        usage.backpressure_wait >= Duration::from_millis(40),
        "backpressure_wait={:?}",
        usage.backpressure_wait
    );
    assert_eq!(usage.external_wait, Duration::ZERO);
    assert!(
        usage.active_cpu < usage.backpressure_wait,
        "active_cpu={:?} should stay small vs backpressure_wait={:?}",
        usage.active_cpu,
        usage.backpressure_wait
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_exposes_lifecycle_phase_and_fiber_state() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(42));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    runtime.begin_suspended(handle).expect("suspend");
    assert_eq!(
        runtime.inspect_lifecycle_phase(handle).expect("phase"),
        FiberLifecyclePhase::Suspended
    );
    assert_eq!(
        runtime.inspect(handle).expect("state"),
        FiberState::Suspended
    );

    runtime.resume_from_suspended(handle).expect("resume");
    assert_eq!(
        runtime.inspect_lifecycle_phase(handle).expect("phase"),
        FiberLifecyclePhase::Running
    );
    assert_eq!(runtime.inspect(handle).expect("state"), FiberState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_accumulates_suspended_not_external_wait() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(43));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    runtime.begin_suspended(handle).expect("suspend");
    tokio::time::sleep(Duration::from_millis(50)).await;
    runtime.resume_from_suspended(handle).expect("resume");

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    while runtime.inspect(handle) != Ok(FiberState::Cancelled) {
        tokio::task::yield_now().await;
    }

    let usage = runtime.activation_usage(handle).expect("usage");
    assert!(
        usage.suspended >= Duration::from_millis(40),
        "suspended={:?}",
        usage.suspended
    );
    assert_eq!(usage.external_wait, Duration::ZERO);
    assert_eq!(usage.backpressure_wait, Duration::ZERO);
}
