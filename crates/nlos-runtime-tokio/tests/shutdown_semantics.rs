//! Shutdown-window semantics for [`TokioRuntimeAdapter`] spawn/join.
//!
//! The adapter's `shutdown()` flag predates this file but historically did
//! not gate `spawn_fiber` or unpark `join_fiber`: a spawn across the
//! shutdown window reached `handle.spawn` on a dead executor (panicking
//! after registering a dead fiber record), and a join on a fiber the
//! executor could never finish parked forever on the terminal condvar.
//!
//! These tests pin the fail-closed contract:
//!
//! 1. `spawn_fiber` after shutdown returns [`RuntimeError::ShuttingDown`]
//!    without panicking and without registering a dead fiber record;
//! 2. `join_fiber` parked on a non-terminal fiber wakes across the
//!    shutdown boundary and returns [`RuntimeError::ShuttingDown`] instead
//!    of hanging;
//! 3. a fiber that already reached a terminal state still joins normally
//!    after shutdown (shutdown fails closed, it does not rewrite history).

use std::future::pending;
use std::sync::mpsc::RecvTimeoutError;
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

/// Given/When/Then: given an adapter whose shutdown flag is set and whose
/// executor has been dropped; when `spawn_fiber` is called; then it returns
/// `ShuttingDown` instead of panicking on `handle.spawn`, and no dead fiber
/// record is registered (admission, scopes, and the fiber map stay clean).
#[test]
fn spawn_after_shutdown_fails_typed_not_panic() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let adapter =
        TokioRuntimeAdapter::new(executor.handle().clone(), TokioRuntimeConfig::default())
            .expect("adapter");

    adapter.shutdown();
    // Dropping the executor mirrors the crash window: without the shutdown
    // gate, `spawn_fiber` reaches `handle.spawn` and panics on the dead
    // runtime after having already inserted an unrunnable fiber record.
    drop(executor);

    let scope = CancellationScopeId::from_bytes(id_bytes(30));
    match adapter.spawn_fiber(fiber_spec(1, scope), Box::pin(pending())) {
        Err(RuntimeError::ShuttingDown) => {}
        other => panic!("expected ShuttingDown, got {other:?}"),
    }
    assert_eq!(
        adapter.registered_fibers(),
        0,
        "no dead fiber record may be registered across the shutdown boundary"
    );
}

/// Given/When/Then: given a join parked on a non-terminal fiber that the
/// executor can never finish; when the adapter shuts down; then the parked
/// join wakes and returns `ShuttingDown` — bounded by a timeout channel so
/// a regression hangs the test briefly instead of forever.
#[tokio::test]
async fn join_after_shutdown_wakes_with_typed_error_instead_of_hanging() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("adapter");
    let scope = CancellationScopeId::from_bytes(id_bytes(31));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    let (joined_tx, joined_rx) = std::sync::mpsc::channel();
    let joining_adapter = runtime.clone();
    let join_thread = std::thread::spawn(move || {
        let _ = joined_tx.send(joining_adapter.join_fiber(handle));
    });

    // The fiber is non-terminal and nothing can finish it, so the join must
    // still be parked here: observing a timeout proves park-not-return
    // before the shutdown boundary is crossed.
    assert_eq!(
        joined_rx.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout),
        "join must stay parked while the fiber is non-terminal"
    );

    runtime.shutdown();

    let join_result = joined_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("join must wake across the shutdown boundary");
    assert_eq!(join_result, Err(RuntimeError::ShuttingDown));
    join_thread.join().expect("joiner thread");
}

/// Given/When/Then: given a fiber that reached `Completed` before shutdown;
/// when it is joined after shutdown; then the stored exit is still returned
/// — shutdown fails closed for parked joins, it does not rewrite history
/// for terminal generations.
#[tokio::test]
async fn join_on_terminal_fiber_after_shutdown_still_returns_exit() {
    let runtime = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("adapter");
    let scope = CancellationScopeId::from_bytes(id_bytes(32));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    runtime.shutdown();

    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));
}
