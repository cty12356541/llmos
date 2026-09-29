//! `B6-5` [`BirthDecision`] wiring tests: [`RuntimeAdapter::birth_fiber`]
//! over
//! the real Tokio adapter classifies every spawn-admission rejection the
//! runtime contract defines (capacity / scope / budget / identity /
//! generation fence / availability) and never invents dimensions beyond
//! that family. The [`BirthRejection`] classification itself is pinned at
//! the contract crate; this file proves the runtime's actual spawn gates
//! produce each reason through the default `birth_fiber` surface.

use std::future::pending;
use std::time::{Duration, Instant};

use nlos_runtime::{
    BirthDecision, BirthRejection, FiberExit, FiberSpec, FiberState, RuntimeAdapter,
};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
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
    TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers,
            ..TokioRuntimeConfig::default()
        },
    )
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
async fn birth_decision_admits_runs_and_joins_the_new_generation() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(600));

    let decision = runtime.birth_fiber(
        fiber_spec(1, scope),
        Box::pin(async { FiberExit::Completed }),
    );
    let handle = match decision {
        BirthDecision::Admitted(handle) => handle,
        BirthDecision::Rejected(rejection) => {
            panic!("fresh birth must be admitted, got {rejection:?}")
        }
    };

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn birth_rejection_capacity_when_admission_bound_is_exhausted() {
    let runtime = runtime(1);
    let scope_a = CancellationScopeId::from_bytes(id_bytes(601));
    let scope_b = CancellationScopeId::from_bytes(id_bytes(602));

    let admitted = runtime.birth_fiber(fiber_spec(1, scope_a), Box::pin(pending()));
    assert!(matches!(admitted, BirthDecision::Admitted(_)));

    let rejected = runtime.birth_fiber(fiber_spec(2, scope_b), Box::pin(pending()));
    assert_eq!(
        rejected,
        BirthDecision::Rejected(BirthRejection::Capacity(
            nlos_runtime::RuntimeError::QueueFull
        ))
    );
    assert_eq!(runtime.registered_fibers(), 1);

    // The rejection was the bound, not corruption: releasing the permit by
    // consuming the live fiber re-opens admission.
    runtime
        .cancel_scope(scope_a, Generation::INITIAL)
        .expect("cancel");
    let handle = admitted.handle().expect("admitted handle");
    wait_for_state(&runtime, *handle, FiberState::Cancelled).await;
    runtime.join_fiber(*handle).expect("join");
    assert!(matches!(
        runtime.birth_fiber(fiber_spec(2, scope_b), Box::pin(pending())),
        BirthDecision::Admitted(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn birth_rejection_scope_when_cancellation_scope_is_closed() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(603));
    // The scope entry exists only once a fiber registers it; keep one live
    // fiber so the cancelled scope stays registered for the second birth.
    let holder = match runtime.birth_fiber(fiber_spec(1, scope), Box::pin(pending())) {
        BirthDecision::Admitted(handle) => handle,
        BirthDecision::Rejected(rejection) => panic!("holder birth: {rejection:?}"),
    };
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel scope");

    let decision = runtime.birth_fiber(fiber_spec(2, scope), Box::pin(pending()));
    assert_eq!(
        decision,
        BirthDecision::Rejected(BirthRejection::Scope(nlos_runtime::RuntimeError::Cancelled))
    );
    assert_eq!(runtime.registered_fibers(), 1, "only the holder remains");
    wait_for_state(&runtime, holder, FiberState::Cancelled).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn birth_rejection_budget_when_deadline_already_exceeded() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(604));
    let mut spec = fiber_spec(1, scope);
    spec.deadline = Instant::now().checked_sub(Duration::from_secs(1));

    let decision = runtime.birth_fiber(spec, Box::pin(pending()));
    assert_eq!(
        decision,
        BirthDecision::Rejected(BirthRejection::Budget(
            nlos_runtime::RuntimeError::DeadlineExceeded
        ))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn birth_rejection_identity_on_duplicate_live_generation() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(605));
    let spec = fiber_spec(1, scope);

    let first = runtime.birth_fiber(spec, Box::pin(pending()));
    assert!(matches!(first, BirthDecision::Admitted(_)));
    let duplicate = runtime.birth_fiber(spec, Box::pin(pending()));
    assert_eq!(
        duplicate,
        BirthDecision::Rejected(BirthRejection::Identity(
            nlos_runtime::RuntimeError::DuplicateFiber
        ))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn birth_rejection_generation_fence_on_live_id_and_scope_generation() {
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(606));
    let live = runtime.birth_fiber(fiber_spec(1, scope), Box::pin(pending()));
    assert!(matches!(live, BirthDecision::Admitted(_)));

    // (a) the fiber id is live under a different fiber generation.
    let mut bumped_fiber = fiber_spec(1, scope);
    bumped_fiber.fiber_generation = Generation::INITIAL
        .checked_next()
        .expect("next fiber generation");
    assert_eq!(
        runtime.birth_fiber(bumped_fiber, Box::pin(pending())),
        BirthDecision::Rejected(BirthRejection::GenerationFence(
            nlos_runtime::RuntimeError::InvalidGeneration
        ))
    );

    // (b) the scope id is registered under a different cancellation
    // generation.
    let mut bumped_scope = fiber_spec(2, scope);
    bumped_scope.cancellation_generation = Generation::INITIAL
        .checked_next()
        .expect("next cancellation generation");
    assert_eq!(
        runtime.birth_fiber(bumped_scope, Box::pin(pending())),
        BirthDecision::Rejected(BirthRejection::GenerationFence(
            nlos_runtime::RuntimeError::InvalidGeneration
        ))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn birth_rejection_unavailable_across_shutdown() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(607));
    runtime.shutdown();

    let decision = runtime.birth_fiber(fiber_spec(1, scope), Box::pin(pending()));
    assert_eq!(
        decision,
        BirthDecision::Rejected(BirthRejection::Unavailable(
            nlos_runtime::RuntimeError::ShuttingDown
        ))
    );
}
