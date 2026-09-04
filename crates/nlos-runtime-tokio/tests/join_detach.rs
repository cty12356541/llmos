//! Structured join/detach contract tests for [`TokioRuntimeAdapter`].
//!
//! Covers join waiting, generation fencing, terminal idempotence, and implicit
//! detach admission recovery without leaking bounded slots.

use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use nlos_runtime::{FiberExit, FiberSpec, FiberState, RuntimeAdapter, RuntimeError};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, ProcessId, ResourceGroupId,
    SchedulerDomainId,
};
use tokio::sync::Barrier;

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

async fn join_in_background(
    runtime: TokioRuntimeAdapter,
    handle: nlos_runtime::FiberHandle,
) -> Result<FiberExit, RuntimeError> {
    tokio::task::spawn_blocking(move || runtime.join_fiber(handle))
        .await
        .expect("join task panicked")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_waits_for_fiber_completion() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 2 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(20));
    let started = Arc::new(Barrier::new(2));
    let release = Arc::clone(&started);
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async move {
                release.wait().await;
                FiberExit::Completed
            }),
        )
        .expect("spawn");

    assert_eq!(runtime.inspect(handle), Ok(FiberState::Running));

    let join_handle = tokio::spawn(join_in_background(runtime.clone(), handle));
    started.wait().await;
    let exit = join_handle.await.expect("join future").expect("join_fiber");
    assert_eq!(exit, FiberExit::Completed);
    assert_eq!(runtime.inspect(handle), Ok(FiberState::Completed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_generation_join_is_rejected() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 1 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(21));
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

    let stale = nlos_runtime::FiberHandle {
        fiber_id: handle.fiber_id,
        generation: Generation::INITIAL.checked_next().expect("next generation"),
    };
    assert_eq!(
        runtime.join_fiber(stale),
        Err(RuntimeError::InvalidGeneration)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_on_terminal_fiber_is_idempotent() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 1 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(22));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;

    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn implicit_detach_recovers_admission_without_join() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 1 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(23));
    let _handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                FiberExit::Completed
            }),
        )
        .expect("spawn");

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if runtime
                .spawn_fiber(fiber_spec(2, scope), Box::pin(pending()))
                .is_ok()
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("admission slot was not recovered after implicit detach");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_returns_cancelled_after_scope_cancel() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 1 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(25));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    let join_handle = tokio::spawn(join_in_background(runtime.clone(), handle));
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");

    let exit = join_handle.await.expect("join future").expect("join_fiber");
    assert_eq!(exit, FiberExit::Cancelled);
    assert_eq!(runtime.inspect(handle), Ok(FiberState::Cancelled));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_detach_is_a_noop_that_validates_handle() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig { max_live_fibers: 1 },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(24));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    runtime.detach_fiber(handle).expect("detach live fiber");

    let stale = nlos_runtime::FiberHandle {
        fiber_id: handle.fiber_id,
        generation: Generation::INITIAL.checked_next().expect("next generation"),
    };
    assert_eq!(
        runtime.detach_fiber(stale),
        Err(RuntimeError::InvalidGeneration)
    );
}
