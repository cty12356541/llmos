use std::future::pending;
use std::time::Duration;

use nlos_runtime::{FiberSpec, FiberState, RuntimeAdapter};
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

async fn run_waiting_fiber_scale(count: usize) {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: count,
        },
    )
    .expect("runtime");
    let scope = CancellationScopeId::from_bytes(id_bytes(count + 1));
    let mut handles = Vec::with_capacity(count);

    for index in 0..count {
        handles.push(
            runtime
                .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
                .expect("spawn"),
        );
    }

    assert_eq!(runtime.registered_fibers(), count);
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if handles
                .iter()
                .all(|handle| runtime.inspect(*handle) == Ok(FiberState::Cancelled))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all fibers should cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ten_thousand_waiting_fibers_on_two_threads() {
    run_waiting_fiber_scale(10_000).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit Stage B scale profile probe"]
async fn one_hundred_thousand_waiting_fibers_on_two_threads() {
    run_waiting_fiber_scale(100_000).await;
}
