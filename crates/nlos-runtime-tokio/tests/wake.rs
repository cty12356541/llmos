use std::future::pending;
use std::time::Duration;

use nlos_runtime::{
    FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter, RuntimeError, WakeOutcome,
    WakeSink,
};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ProcessId,
    ResourceGroupId, SchedulerDomainId,
};
use tokio::runtime::Handle;

/// Bounded observation window for "still pending" assertions.
const PENDING_PROBE: Duration = Duration::from_millis(100);
/// Generous bound for waits that must resolve.
const RESOLVE: Duration = Duration::from_secs(5);

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

async fn wait_for_state(runtime: &TokioRuntimeAdapter, handle: FiberHandle, expected: FiberState) {
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
async fn registered_wait_resolves_woken_and_wake_reports_delivered() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(20));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");
    let sink = runtime.wake_sink();

    let wait = runtime
        .wait_for_operation(handle, operation(1), Generation::INITIAL)
        .expect("wait");
    let outcome = sink.wake(&handle, operation(1), Generation::INITIAL);

    assert_eq!(outcome, Ok(WakeOutcome::Delivered));
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("wait resolves"),
        WaitOutcome::Woken
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_wake_reports_delivered_without_second_logical_wake() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(21));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");
    let sink = runtime.wake_sink();

    let wait = runtime
        .wait_for_operation(handle, operation(2), Generation::INITIAL)
        .expect("wait");
    assert_eq!(
        sink.wake(&handle, operation(2), Generation::INITIAL),
        Ok(WakeOutcome::Delivered)
    );
    // At-least-once redelivery of the same key: still `Delivered`, but the
    // already-consumed wait is not logically woken a second time.
    assert_eq!(
        sink.wake(&handle, operation(2), Generation::INITIAL),
        Ok(WakeOutcome::Delivered)
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("wait resolves"),
        WaitOutcome::Woken
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_wake_is_buffered_and_consumed_by_registration() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(22));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");
    let sink = runtime.wake_sink();

    // The fiber is alive but not waiting yet: the wake is buffered by key.
    assert_eq!(
        sink.wake(&handle, operation(3), Generation::INITIAL),
        Ok(WakeOutcome::Delivered)
    );
    let wait = runtime
        .wait_for_operation(handle, operation(3), Generation::INITIAL)
        .expect("wait");
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("buffered wake resolves immediately"),
        WaitOutcome::Woken
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_to_dropped_receiver_is_rebuffered_for_the_next_registration() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(29));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");
    let sink = runtime.wake_sink();

    // Register and then drop the wait without awaiting it: the receiver is
    // gone, so the wake cannot be handed off and must be re-buffered under
    // the same key instead of being silently lost.
    let wait = runtime
        .wait_for_operation(handle, operation(9), Generation::INITIAL)
        .expect("wait");
    drop(wait);
    assert_eq!(
        sink.wake(&handle, operation(9), Generation::INITIAL),
        Ok(WakeOutcome::Delivered)
    );

    // A later registration for the same key consumes the re-buffered wake
    // and resolves immediately as `Woken`.
    let wait = runtime
        .wait_for_operation(handle, operation(9), Generation::INITIAL)
        .expect("wait");
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("re-buffered wake resolves immediately"),
        WaitOutcome::Woken
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_fiber_generation_reports_fiber_gone() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(23));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");
    let sink = runtime.wake_sink();

    let stale = FiberHandle {
        fiber_id: handle.fiber_id,
        generation: handle.generation.checked_next().expect("next generation"),
    };
    assert_eq!(
        sink.wake(&stale, operation(4), Generation::INITIAL),
        Ok(WakeOutcome::FiberGone)
    );

    let unknown = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(999)),
        generation: Generation::INITIAL,
    };
    assert_eq!(
        sink.wake(&unknown, operation(4), Generation::INITIAL),
        Ok(WakeOutcome::FiberGone)
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_fiber_reports_not_waiting() {
    let runtime = runtime(1);
    let scope = CancellationScopeId::from_bytes(id_bytes(24));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");
    let sink = runtime.wake_sink();

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(
        sink.wake(&handle, operation(5), Generation::INITIAL),
        Ok(WakeOutcome::NotWaiting)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_cancellation_resolves_wait_as_cancelled() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(25));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    let wait = runtime
        .wait_for_operation(handle, operation(6), Generation::INITIAL)
        .expect("wait");
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");

    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("cancelled wait must resolve"),
        WaitOutcome::Cancelled
    );
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_operation_generation_does_not_wake_registered_wait() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(26));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");
    let sink = runtime.wake_sink();
    let current = Generation::INITIAL.checked_next().expect("next generation");

    let mut wait = runtime
        .wait_for_operation(handle, operation(7), current)
        .expect("wait");
    // A wake fenced to a stale operation generation must not touch the wait.
    assert_eq!(
        sink.wake(&handle, operation(7), Generation::INITIAL),
        Ok(WakeOutcome::Delivered)
    );
    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut wait)
            .await
            .is_err(),
        "stale operation generation must leave the wait pending"
    );

    // The correctly fenced wake still resolves it.
    assert_eq!(
        sink.wake(&handle, operation(7), current),
        Ok(WakeOutcome::Delivered)
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("wait resolves"),
        WaitOutcome::Woken
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_waits_across_fibers_and_operations_do_not_cross_talk() {
    let runtime = runtime(8);
    let scope = CancellationScopeId::from_bytes(id_bytes(27));
    let sink = runtime.wake_sink();

    let mut handles = Vec::new();
    for index in 1..=4 {
        handles.push(
            runtime
                .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
                .expect("spawn"),
        );
    }

    // Two operations per fiber; even keys are woken first, odd keys must
    // stay pending until their own wake arrives.
    let mut first_round = Vec::new();
    let mut second_round = Vec::new();
    for (fiber_offset, handle) in handles.iter().enumerate() {
        for operation_offset in 0..2_usize {
            let operation_id = operation(100 + fiber_offset * 2 + operation_offset);
            let wait = runtime
                .wait_for_operation(*handle, operation_id, Generation::INITIAL)
                .expect("wait");
            if operation_offset == 0 {
                first_round.push((*handle, operation_id, wait));
            } else {
                second_round.push((*handle, operation_id, wait));
            }
        }
    }

    let mut deliveries = tokio::task::JoinSet::new();
    for (handle, operation_id, _wait) in &first_round {
        let sink = sink.clone();
        let handle = *handle;
        let operation_id = *operation_id;
        deliveries.spawn(async move { sink.wake(&handle, operation_id, Generation::INITIAL) });
    }
    while let Some(result) = deliveries.join_next().await {
        assert_eq!(result.expect("delivery task"), Ok(WakeOutcome::Delivered));
    }

    for (_handle, _operation_id, wait) in first_round {
        assert_eq!(
            tokio::time::timeout(RESOLVE, wait)
                .await
                .expect("targeted wait resolves"),
            WaitOutcome::Woken
        );
    }
    for (_handle, _operation_id, wait) in &mut second_round {
        assert!(
            tokio::time::timeout(PENDING_PROBE, wait).await.is_err(),
            "untargeted key must stay pending"
        );
    }

    for (handle, operation_id, _wait) in &second_round {
        assert_eq!(
            sink.wake(handle, *operation_id, Generation::INITIAL),
            Ok(WakeOutcome::Delivered)
        );
    }
    for (_handle, _operation_id, wait) in second_round {
        assert_eq!(
            tokio::time::timeout(RESOLVE, wait)
                .await
                .expect("second round wait resolves"),
            WaitOutcome::Woken
        );
    }
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_fails_wakes_and_cancels_pending_waits() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(28));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");
    let sink = runtime.wake_sink();

    let wait = runtime
        .wait_for_operation(handle, operation(8), Generation::INITIAL)
        .expect("wait");
    runtime.shutdown();

    assert_eq!(
        sink.wake(&handle, operation(8), Generation::INITIAL),
        Err(RuntimeError::ShuttingDown)
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("shutdown must resolve pending waits"),
        WaitOutcome::Cancelled
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}
