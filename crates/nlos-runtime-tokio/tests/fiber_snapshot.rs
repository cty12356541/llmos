//! Aggregate fiber inspection snapshot tests for [`TokioRuntimeAdapter`].
//!
//! Coverage map (W44-RB):
//!
//! - Field parity: on a quiescent fiber every field of
//!   [`FiberSnapshot`] equals the corresponding separate query
//!   ([`RuntimeAdapter::inspect`], `inspect_lifecycle_phase`,
//!   [`RuntimeAdapter::activation_usage`]); on a terminal record — where all
//!   three domains are frozen — the equality is exact and repeatable;
//! - Internal consistency under concurrency: while a racer toggles the
//!   suspend/resume boundary, every snapshot observes `state` and
//!   `lifecycle_phase` from the same side of the transition (never
//!   `Suspended` on one domain with `Running` on the other), and the
//!   metering dimensions never regress — the snapshot cannot mix epochs;
//! - Obtainability: the snapshot resolves across the running, suspended,
//!   and terminal states of a fiber;
//! - Error parity: a join-reaped generation fails with
//!   [`RuntimeError::FiberReaped`], the same error surface as the three
//!   separate queries.
//!
//! The concurrent test is the regression guard for the single-lock-window
//! implementation: an aggregate built from three separate acquisitions (the
//! torn view the separate queries hand to a caller like the
//! system-control fiber inspector) would eventually violate the state/phase
//! coupling assertion under this contention.

use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter, RuntimeError};
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
    TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime")
}

fn reaped(handle: FiberHandle) -> RuntimeError {
    RuntimeError::FiberReaped {
        fiber_id: handle.fiber_id,
        generation: handle.generation,
    }
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
    .expect("expected fiber state within timeout");
}

/// Given a terminal, unjoined fiber (its record stays registered and none of
/// the three domains mutates anymore); when the aggregate snapshot and the
/// three separate queries are taken; then every snapshot field equals the
/// corresponding separate query, repeatably — exact equality, including all
/// six usage dimensions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_matches_separate_queries_on_quiescent_terminal_fiber() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(50));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");
    wait_for_state(&runtime, handle, FiberState::Completed).await;

    for _ in 0..3 {
        let snapshot = runtime.inspect_fiber_snapshot(handle).expect("snapshot");
        assert_eq!(snapshot.state, FiberState::Completed);
        assert_eq!(snapshot.state, runtime.inspect(handle).expect("state"));
        assert_eq!(
            snapshot.lifecycle_phase,
            runtime.inspect_lifecycle_phase(handle).expect("phase")
        );
        assert_eq!(
            snapshot.usage,
            runtime.activation_usage(handle).expect("usage")
        );
    }
}

/// Given a live fiber held in `Suspended` (quiescent — no transition in
/// flight, so `state`/`lifecycle_phase` cannot move); when the snapshot is
/// taken and compared against the separate queries; then the discrete
/// domains match exactly, and the usage comparison holds the monotone
/// ordering (`suspended` only grows between the two instants — exact usage
/// equality is not asserted because the open phase folds up to each read's
/// own timestamp).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_matches_discrete_queries_on_quiescent_live_fiber() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(51));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    runtime.begin_suspended(handle).expect("suspend");
    let snapshot = runtime.inspect_fiber_snapshot(handle).expect("snapshot");
    assert_eq!(snapshot.state, FiberState::Suspended);
    assert_eq!(snapshot.lifecycle_phase, FiberLifecyclePhase::Suspended);
    assert_eq!(snapshot.state, runtime.inspect(handle).expect("state"));
    assert_eq!(
        snapshot.lifecycle_phase,
        runtime.inspect_lifecycle_phase(handle).expect("phase")
    );
    let later_usage = runtime.activation_usage(handle).expect("usage");
    assert!(
        later_usage.suspended >= snapshot.usage.suspended,
        "suspended must not regress: {:?} -> {:?}",
        snapshot.usage.suspended,
        later_usage.suspended
    );
    assert_eq!(snapshot.usage.external_wait, Duration::ZERO);
}

/// Internal consistency under concurrent transitions: a racer toggles the
/// suspend/resume boundary while a reader samples aggregate snapshots. Every
/// snapshot must observe `state` and `lifecycle_phase` from the same side of
/// the transition, and the metering dimensions must never regress — the
/// snapshot cannot mix epochs. Both epochs must actually be observed, or the
/// pass would be vacuous.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_stays_internally_consistent_under_concurrent_transitions() {
    const TRANSITIONS: usize = 2_000;
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(52));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    let done = Arc::new(AtomicBool::new(false));
    let racer_runtime = runtime.clone();
    let racer_done = Arc::clone(&done);
    let racer = tokio::task::spawn_blocking(move || {
        for _ in 0..TRANSITIONS {
            racer_runtime.begin_suspended(handle).expect("suspend");
            // Hold the suspended epoch long enough for the reader to sample
            // mid-epoch snapshots, not only the endpoints.
            std::thread::sleep(Duration::from_micros(100));
            racer_runtime.resume_from_suspended(handle).expect("resume");
        }
        racer_done.store(true, Ordering::Release);
    });

    let reader_runtime = runtime.clone();
    let reader_done = Arc::clone(&done);
    let reader = tokio::task::spawn_blocking(move || {
        let mut saw_running = false;
        let mut saw_suspended = false;
        let mut previous_suspended = Duration::ZERO;
        let mut previous_active_cpu = Duration::ZERO;
        let mut samples = 0_u64;
        while !reader_done.load(Ordering::Acquire) {
            let snapshot = reader_runtime
                .inspect_fiber_snapshot(handle)
                .expect("snapshot");
            samples += 1;
            match snapshot.state {
                FiberState::Running => {
                    saw_running = true;
                    assert_eq!(
                        snapshot.lifecycle_phase,
                        FiberLifecyclePhase::Running,
                        "torn snapshot: state=Running with phase={:?}",
                        snapshot.lifecycle_phase
                    );
                }
                FiberState::Suspended => {
                    saw_suspended = true;
                    assert_eq!(
                        snapshot.lifecycle_phase,
                        FiberLifecyclePhase::Suspended,
                        "torn snapshot: state=Suspended with phase={:?}",
                        snapshot.lifecycle_phase
                    );
                }
                other => panic!("unexpected state under suspend/resume racing: {other:?}"),
            }
            assert!(
                snapshot.usage.suspended >= previous_suspended,
                "suspended regressed: {:?} -> {:?}",
                previous_suspended,
                snapshot.usage.suspended
            );
            assert!(
                snapshot.usage.active_cpu >= previous_active_cpu,
                "active_cpu regressed: {:?} -> {:?}",
                previous_active_cpu,
                snapshot.usage.active_cpu
            );
            previous_suspended = snapshot.usage.suspended;
            previous_active_cpu = snapshot.usage.active_cpu;
        }
        (samples, saw_running, saw_suspended)
    });

    racer.await.expect("racer task");
    let (samples, saw_running, saw_suspended) = reader.await.expect("reader task");
    assert!(samples > 0, "reader must have sampled at least once");
    assert!(
        saw_running && saw_suspended,
        "both epochs must be observed for a non-vacuous pass (samples={samples})"
    );
}

/// The snapshot resolves across the running, suspended, and terminal states
/// of one fiber; at the terminal boundary all three domains are frozen, so
/// the snapshot equals the separate queries exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_is_obtainable_across_running_suspended_and_terminal_states() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(53));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    let running = runtime.inspect_fiber_snapshot(handle).expect("snapshot");
    assert_eq!(running.state, FiberState::Running);
    assert_eq!(running.lifecycle_phase, FiberLifecyclePhase::Running);

    runtime.begin_suspended(handle).expect("suspend");
    let suspended = runtime.inspect_fiber_snapshot(handle).expect("snapshot");
    assert_eq!(suspended.state, FiberState::Suspended);
    assert_eq!(suspended.lifecycle_phase, FiberLifecyclePhase::Suspended);

    runtime.resume_from_suspended(handle).expect("resume");
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;
    let terminal = runtime.inspect_fiber_snapshot(handle).expect("snapshot");
    assert_eq!(terminal.state, FiberState::Cancelled);
    assert_eq!(terminal.state, runtime.inspect(handle).expect("state"));
    assert_eq!(
        terminal.lifecycle_phase,
        runtime.inspect_lifecycle_phase(handle).expect("phase")
    );
    assert_eq!(
        terminal.usage,
        runtime.activation_usage(handle).expect("usage")
    );
}

/// Error parity: a join-reaped generation rejects the aggregate snapshot
/// with the same typed error as the separate queries (FIBER-REAP-002 via the
/// shared handle resolution).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaped_handle_rejects_fiber_snapshot_with_fiber_reaped() {
    let runtime = runtime(2);
    let scope = CancellationScopeId::from_bytes(id_bytes(54));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");
    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));

    assert_eq!(runtime.inspect_fiber_snapshot(handle), Err(reaped(handle)));
    assert_eq!(runtime.inspect(handle), Err(reaped(handle)));
}
