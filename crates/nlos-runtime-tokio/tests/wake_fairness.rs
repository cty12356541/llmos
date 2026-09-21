//! Wake latency and fairness tests for the durable Outbox wake consumer
//! endpoint ([`TokioWakeSink`] + [`TokioRuntimeAdapter::wait_for_operation`]).
//!
//! Determinism strategy: every test accounts progress in **scheduler
//! rounds** — one `yield_now().await` by the driver task is one round, i.e.
//! one pass over the ready queue. There are no wall-clock waits
//! (`tokio::time` is never used): a wait that would be starved stays pending
//! forever and deterministically trips the round bound, while a delivered
//! wake is observed in a small fixed number of rounds. Tokio paused time is
//! not used because the workspace `tokio` dependency does not enable the
//! `test-util` feature; these tests register no timers, so round accounting
//! alone is fully deterministic.
//!
//! Two flavors:
//!
//! - the original `current_thread` tests (W27-F): a single scheduler makes
//!   the round counter a total order over task progress, so fixed round
//!   bounds are fully deterministic;
//! - the `multi_thread` tests (W35-P6, closing §6.17.2's registered
//!   multi-worker gap): with `workers` workers the driver's yield only
//!   guarantees one pass over ONE worker's ready queue, so progress is
//!   accounted in rounds via shared atomics and every bound is scaled by
//!   the worker count (see `multi_worker_bounds`). Wall-clock asserts are
//!   still avoided: a starved waiter never resolves and trips the scaled
//!   round bound regardless of scheduling order.

use std::future::pending;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nlos_runtime::{
    FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter, WakeOutcome, WakeSink,
};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ProcessId,
    ResourceGroupId, SchedulerDomainId,
};
use tokio::runtime::Handle;

/// Round budget for all waiters to finish registering their durable wait.
const REGISTRATION_ROUND_BOUND: usize = 32;
/// Latency bound: a woken fiber must observe its wake within this many
/// scheduler rounds after the wake was issued.
const LATENCY_ROUND_BOUND: usize = 4;
/// Fairness bound: every waiter in a burst must be observed within this many
/// rounds, and no waiter may lag the fastest by more than the spread.
const FAIRNESS_ROUND_BOUND: usize = 8;
const FAIRNESS_SPREAD_ROUNDS: usize = 4;
/// Round budget for terminal-state polls (scope cancellation reaching the
/// fiber task) and for the competing load fibers to finish.
const TERMINAL_ROUND_BOUND: usize = 16;
/// Round budget for consuming a re-buffered wake by a fresh registration.
const REBUFFER_ROUND_BOUND: usize = 2;

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
    TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime")
}

/// Yields exactly `rounds` times, giving every other ready task that many
/// scheduling passes.
async fn yield_rounds(rounds: usize) {
    for _ in 0..rounds {
        tokio::task::yield_now().await;
    }
}

/// What one in-fiber waiter observed when its wait resolved, and in which
/// scheduler round (per the shared round counter stored by the driver).
#[derive(Clone, Copy, Debug)]
struct Observation {
    outcome: WaitOutcome,
    round: usize,
}

/// Spawns a fiber that registers its own Operation wait in its task body,
/// parks on it, and records the outcome plus the round counter value at the
/// moment the wake was observed.
fn spawn_waiter(
    runtime: &TokioRuntimeAdapter,
    fiber_index: usize,
    operation_index: usize,
    registered: &Arc<AtomicUsize>,
    round: &Arc<AtomicUsize>,
    observations: &Arc<Mutex<Vec<Option<Observation>>>>,
) -> FiberHandle {
    let spec = fiber_spec(
        fiber_index,
        CancellationScopeId::from_bytes(id_bytes(500 + fiber_index)),
    );
    let handle = FiberHandle {
        fiber_id: spec.fiber_id,
        generation: spec.fiber_generation,
    };
    let adapter = runtime.clone();
    let registered = Arc::clone(registered);
    let round = Arc::clone(round);
    let observations = Arc::clone(observations);
    let body = async move {
        let wait = adapter
            .wait_for_operation(handle, operation(operation_index), Generation::INITIAL)
            .expect("wait registration");
        registered.fetch_add(1, Ordering::SeqCst);
        let outcome = wait.await;
        let round = round.load(Ordering::SeqCst);
        observations.lock().expect("observations lock")[fiber_index] =
            Some(Observation { outcome, round });
        FiberExit::Completed
    };
    runtime
        .spawn_fiber(spec, Box::pin(body))
        .expect("spawn waiter")
}

/// Drives scheduler rounds until `condition` holds, within `bound` rounds.
/// Returns the round in which the condition was met.
async fn rounds_until(
    bound: usize,
    round: &Arc<AtomicUsize>,
    condition: impl Fn() -> bool,
) -> Option<usize> {
    for r in 1..=bound {
        round.store(r, Ordering::SeqCst);
        tokio::task::yield_now().await;
        if condition() {
            return Some(r);
        }
    }
    None
}

fn observed(observations: &Arc<Mutex<Vec<Option<Observation>>>>) -> Vec<Observation> {
    observations
        .lock()
        .expect("observations lock")
        .iter()
        .copied()
        .flatten()
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn woken_fiber_observes_wake_within_bounded_scheduler_rounds() {
    const WAITERS: usize = 8;
    let runtime = runtime(64);
    let sink = runtime.wake_sink();

    let registered = Arc::new(AtomicUsize::new(0));
    let round = Arc::new(AtomicUsize::new(0));
    let observations: Arc<Mutex<Vec<Option<Observation>>>> =
        Arc::new(Mutex::new(vec![None; WAITERS]));

    let mut handles = Vec::new();
    for index in 0..WAITERS {
        handles.push(spawn_waiter(
            &runtime,
            index,
            index,
            &registered,
            &round,
            &observations,
        ));
    }
    rounds_until(REGISTRATION_ROUND_BOUND, &round, || {
        registered.load(Ordering::SeqCst) == WAITERS
    })
    .await
    .expect("all waiters registered within the round bound");

    // Burst: every wake is issued back-to-back with no scheduling pass in
    // between, then the fibers must observe them within the latency bound.
    for (index, handle) in handles.iter().enumerate() {
        assert_eq!(
            sink.wake(handle, operation(index), Generation::INITIAL),
            Ok(WakeOutcome::Delivered)
        );
    }

    let drained = rounds_until(LATENCY_ROUND_BOUND, &round, || {
        observed(&observations).len() == WAITERS
    })
    .await;
    assert!(
        drained.is_some(),
        "every woken fiber must observe its wake within {LATENCY_ROUND_BOUND} scheduler rounds"
    );

    let seen = observed(&observations);
    assert_eq!(seen.len(), WAITERS, "no waiter may stay unobserved");
    for observation in &seen {
        assert_eq!(observation.outcome, WaitOutcome::Woken);
        assert!(
            observation.round >= 1 && observation.round <= LATENCY_ROUND_BOUND,
            "wake observed outside the latency bound: {observation:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn burst_wake_is_fair_across_waiters_under_yield_load() {
    const WAITERS: usize = 32;
    const LOAD_FIBERS: usize = 8;
    const LOAD_SPINS: usize = 16;
    let runtime = runtime(WAITERS + LOAD_FIBERS + 8);
    let sink = runtime.wake_sink();

    let registered = Arc::new(AtomicUsize::new(0));
    let round = Arc::new(AtomicUsize::new(0));
    let observations: Arc<Mutex<Vec<Option<Observation>>>> =
        Arc::new(Mutex::new(vec![None; WAITERS]));
    let load_completed = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for index in 0..WAITERS {
        handles.push(spawn_waiter(
            &runtime,
            index,
            10_000 + index,
            &registered,
            &round,
            &observations,
        ));
    }
    // Competing fibers that only ever yield: they interleave with the woken
    // waiters on the single scheduler but never block anybody.
    for index in 0..LOAD_FIBERS {
        let load_completed = Arc::clone(&load_completed);
        let spec = fiber_spec(
            1000 + index,
            CancellationScopeId::from_bytes(id_bytes(700 + index)),
        );
        runtime
            .spawn_fiber(
                spec,
                Box::pin(async move {
                    for _ in 0..LOAD_SPINS {
                        tokio::task::yield_now().await;
                    }
                    load_completed.fetch_add(1, Ordering::SeqCst);
                    FiberExit::Completed
                }),
            )
            .expect("spawn load fiber");
    }

    rounds_until(REGISTRATION_ROUND_BOUND, &round, || {
        registered.load(Ordering::SeqCst) == WAITERS
    })
    .await
    .expect("all waiters registered within the round bound");

    for (index, handle) in handles.iter().enumerate() {
        assert_eq!(
            sink.wake(handle, operation(10_000 + index), Generation::INITIAL),
            Ok(WakeOutcome::Delivered)
        );
    }

    let drained = rounds_until(FAIRNESS_ROUND_BOUND, &round, || {
        observed(&observations).len() == WAITERS
    })
    .await;
    assert!(
        drained.is_some(),
        "all burst-woken waiters must progress within {FAIRNESS_ROUND_BOUND} rounds"
    );

    let seen = observed(&observations);
    assert_eq!(seen.len(), WAITERS, "no waiter may be starved");
    assert!(
        seen.iter().all(|o| o.outcome == WaitOutcome::Woken),
        "a lost or misrouted wake must fail loudly, not resolve otherwise"
    );
    let max_round = seen.iter().map(|o| o.round).max().unwrap_or(0);
    let min_round = seen.iter().map(|o| o.round).min().unwrap_or(0);
    assert!(
        max_round <= FAIRNESS_ROUND_BOUND,
        "slowest waiter observed at round {max_round}"
    );
    assert!(
        max_round - min_round <= FAIRNESS_SPREAD_ROUNDS,
        "waiter spread {min_round}..{max_round} exceeds the fairness spread"
    );

    rounds_until(TERMINAL_ROUND_BOUND, &round, || {
        load_completed.load(Ordering::SeqCst) == LOAD_FIBERS
    })
    .await
    .expect("background load must also finish within the round bound");
}

#[tokio::test(flavor = "current_thread")]
async fn burst_wake_with_interleaved_cancel_never_drops_wakes() {
    const FIBERS: usize = 8;
    let runtime = runtime(FIBERS + 8);
    let sink = runtime.wake_sink();

    let mut handles = Vec::new();
    let mut scopes = Vec::new();
    for index in 0..FIBERS {
        let scope = CancellationScopeId::from_bytes(id_bytes(300 + index));
        handles.push(
            runtime
                .spawn_fiber(fiber_spec(index, scope), Box::pin(pending()))
                .expect("spawn"),
        );
        scopes.push(scope);
    }

    // Every fiber's wait is registered from the driver task so the wait
    // outcome itself (not the fiber body, which scope cancellation drops)
    // can be asserted.
    let mut waits = Vec::new();
    for (index, handle) in handles.iter().enumerate() {
        waits.push(
            runtime
                .wait_for_operation(*handle, operation(200 + index), Generation::INITIAL)
                .expect("wait"),
        );
    }
    // Fiber 7's wait receiver is dropped BEFORE its wake is issued: the sink
    // must re-buffer that wake instead of losing it.
    drop(waits.pop().expect("fiber 7 wait"));

    // Interleaved burst: wake(i) then cancel the even fibers' scopes, with
    // no scheduling pass between the calls.
    for index in 0..FIBERS {
        assert_eq!(
            sink.wake(&handles[index], operation(200 + index), Generation::INITIAL),
            Ok(WakeOutcome::Delivered)
        );
        if index % 2 == 0 {
            runtime
                .cancel_scope(scopes[index], Generation::INITIAL)
                .expect("cancel");
        }
    }

    // `waits` now holds fibers 0..=6 in order. Even fibers: the wake was
    // delivered first and the cancellation linearized after it — the biased
    // wait resolves `Cancelled` and the wake is accounted, not lost. Odd
    // fibers: survivors must observe `Woken`.
    let mut even_outcomes = Vec::new();
    let mut odd_outcomes = Vec::new();
    for (index, wait) in waits.into_iter().enumerate() {
        let outcome = tokio::select! {
            biased;
            outcome = wait => outcome,
            () = yield_rounds(TERMINAL_ROUND_BOUND) => panic!(
                "fiber {index} wait did not resolve within {TERMINAL_ROUND_BOUND} rounds"
            ),
        };
        if index % 2 == 0 {
            even_outcomes.push(outcome);
        } else {
            odd_outcomes.push(outcome);
        }
    }
    assert!(
        even_outcomes
            .iter()
            .all(|outcome| *outcome == WaitOutcome::Cancelled),
        "woken-then-cancelled waits must resolve Cancelled: {even_outcomes:?}"
    );
    assert!(
        odd_outcomes
            .iter()
            .all(|outcome| *outcome == WaitOutcome::Woken),
        "survivor waits must resolve Woken: {odd_outcomes:?}"
    );

    // Even fibers reach the unique `Cancelled` terminal.
    for index in (0..FIBERS).step_by(2) {
        let handle = handles[index];
        let mut settled = false;
        for _ in 0..TERMINAL_ROUND_BOUND {
            if runtime.inspect(handle) == Ok(FiberState::Cancelled) {
                settled = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(settled, "fiber {index} must settle Cancelled");
    }

    // The wake issued for the dropped wait was re-buffered and must survive
    // the interleaved cancellations of the other scopes: a fresh registration
    // for the same key consumes it immediately.
    let rewait = runtime
        .wait_for_operation(handles[7], operation(207), Generation::INITIAL)
        .expect("re-register fiber 7 wait");
    let outcome = tokio::select! {
        biased;
        outcome = rewait => outcome,
        () = yield_rounds(REBUFFER_ROUND_BOUND) => panic!(
            "re-buffered wake must be consumed within {REBUFFER_ROUND_BOUND} rounds"
        ),
    };
    assert_eq!(outcome, WaitOutcome::Woken);

    // A further wake for a cancelled fiber reports `NotWaiting`: nothing is
    // silently swallowed on the terminal side of the interleave either.
    assert_eq!(
        sink.wake(&handles[0], operation(200), Generation::INITIAL),
        Ok(WakeOutcome::NotWaiting)
    );
}

// ---------------------------------------------------------------------------
// Multi-worker flavor (W35-P6, §6.17.2 gap "multi_thread/work-stealing 下的
// 公平性形状"). Same wake path, same burst shape, different scheduler: a
// `multi_thread` runtime with a fixed worker count.
//
// Calibration (why every bound scales with the worker count, and why a
// multi-worker round is bigger than a current_thread round): the test driver
// is the `block_on` root task, and on a `current_thread` runtime one driver
// `yield_now().await` IS one pass over the single scheduler — every other
// ready task runs synchronously inside it. On a `multi_thread` runtime the
// root task's yield round-trips in microseconds on its own thread while the
// workers are independent OS threads that may not have been scheduled at
// all yet, so a bare yield cannot bound their progress. A multi-worker
// driver round is therefore a COOPERATIVE SCHEDULING SLOT: one yield (a
// pass over the driver's own queue) plus a 1ms sleep (OS time for the other
// workers). The assertion values remain round counts — never milliseconds —
// and each round covers at least 1/workers of the ready population in the
// worst case, so the drain bound and the allowed spread both scale linearly
// with `workers` (`bound = base × workers`).
//
// The burst itself still happens back-to-back with zero yields in between.
// Latency is measured in rounds elapsed since the burst completion marker
// (`round - burst_round`), so a waiter resolved on an already-idle worker
// while the burst is still issuing reads latency 0 instead of inheriting a
// stale registration-phase round number.
//
// Starvation stays a deterministic failure: a starved waiter never resolves,
// so the drain loop trips its scaled round bound regardless of scheduling
// order; a delivered wake resolves within a few cooperative rounds per
// worker pass, which the scaled bound accommodates. The 1ms slot makes the
// round unit coarse enough that normal OS jitter cannot burn the whole
// budget before a healthy worker completes its few polls.
// ---------------------------------------------------------------------------

/// One cooperative scheduling slot for the multi-worker flavor: pass over
/// the driver's own queue, then OS time for the other workers.
const WORKER_ROUND_SLOT: Duration = Duration::from_millis(1);

/// Multi-worker burst fairness parameters and the worker-scaled bounds.
struct MultiWorkerBounds {
    workers: usize,
    registration_round_bound: usize,
    drain_round_bound: usize,
    spread_round_bound: usize,
}

impl MultiWorkerBounds {
    fn new(workers: usize) -> Self {
        Self {
            workers,
            registration_round_bound: REGISTRATION_ROUND_BOUND * workers,
            drain_round_bound: FAIRNESS_ROUND_BOUND * workers,
            spread_round_bound: FAIRNESS_SPREAD_ROUNDS * workers,
        }
    }
}

/// What one multi-worker waiter observed: the wait outcome and the round
/// latency since the burst completion marker.
#[derive(Clone, Copy, Debug)]
struct MultiWorkerObservation {
    outcome: WaitOutcome,
    round_latency: usize,
}

#[allow(clippy::too_many_lines)] // One scenario covers registration, burst, drain, spread, and load phases.
async fn multi_worker_burst_fairness(bounds: MultiWorkerBounds) {
    const WAITERS: usize = 32;
    const LOAD_FIBERS: usize = 8;
    const LOAD_SPINS: usize = 16;
    let runtime = runtime(WAITERS + LOAD_FIBERS + 8);
    let sink = runtime.wake_sink();

    let registered = Arc::new(AtomicUsize::new(0));
    let round = Arc::new(AtomicUsize::new(0));
    let observations: Arc<Mutex<Vec<Option<MultiWorkerObservation>>>> =
        Arc::new(Mutex::new(vec![None; WAITERS]));
    let load_completed = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for index in 0..WAITERS {
        handles.push(spawn_multi_worker_waiter(
            &runtime,
            index,
            &registered,
            &round,
            &observations,
        ));
    }
    for index in 0..LOAD_FIBERS {
        let load_completed = Arc::clone(&load_completed);
        let spec = fiber_spec(
            2000 + index,
            CancellationScopeId::from_bytes(id_bytes(800 + index)),
        );
        runtime
            .spawn_fiber(
                spec,
                Box::pin(async move {
                    for _ in 0..LOAD_SPINS {
                        tokio::task::yield_now().await;
                    }
                    load_completed.fetch_add(1, Ordering::SeqCst);
                    FiberExit::Completed
                }),
            )
            .expect("spawn load fiber");
    }

    // Registration phase: the round counter stores absolute, monotonically
    // increasing round numbers so no phase ever rewinds it.
    let mut phase = 0_usize;
    loop {
        phase += 1;
        round.store(phase, Ordering::SeqCst);
        tokio::task::yield_now().await;
        tokio::time::sleep(WORKER_ROUND_SLOT).await;
        if registered.load(Ordering::SeqCst) == WAITERS {
            break;
        }
        assert!(
            phase < bounds.registration_round_bound,
            "all waiters must register within {} rounds on {} workers",
            bounds.registration_round_bound,
            bounds.workers
        );
    }

    // Burst completion marker: waiters resolving while the burst is still
    // issuing compute a saturated (zero) latency against this round number.
    let burst_round = phase;
    for (index, handle) in handles.iter().enumerate() {
        assert_eq!(
            sink.wake(handle, operation(10_000 + index), Generation::INITIAL),
            Ok(WakeOutcome::Delivered)
        );
    }

    loop {
        phase += 1;
        round.store(phase, Ordering::SeqCst);
        tokio::task::yield_now().await;
        tokio::time::sleep(WORKER_ROUND_SLOT).await;
        if observed_multi_worker(&observations).len() == WAITERS {
            break;
        }
        assert!(
            phase - burst_round < bounds.drain_round_bound,
            "all burst-woken waiters must progress within {} rounds on {} workers",
            bounds.drain_round_bound,
            bounds.workers
        );
    }

    let seen = observed_multi_worker(&observations);
    assert_eq!(seen.len(), WAITERS, "no waiter may be starved");
    assert!(
        seen.iter().all(|o| o.outcome == WaitOutcome::Woken),
        "a lost or misrouted wake must fail loudly, not resolve otherwise"
    );
    let max_latency = seen.iter().map(|o| o.round_latency).max().unwrap_or(0);
    let min_latency = seen.iter().map(|o| o.round_latency).min().unwrap_or(0);
    assert!(
        max_latency <= bounds.drain_round_bound,
        "slowest waiter observed at round latency {max_latency}"
    );
    assert!(
        max_latency - min_latency <= bounds.spread_round_bound,
        "waiter round-latency spread {min_latency}..{max_latency} exceeds the \
         worker-scaled fairness spread"
    );

    loop {
        phase += 1;
        round.store(phase, Ordering::SeqCst);
        tokio::task::yield_now().await;
        tokio::time::sleep(WORKER_ROUND_SLOT).await;
        if load_completed.load(Ordering::SeqCst) == LOAD_FIBERS {
            break;
        }
        assert!(
            phase - burst_round < bounds.drain_round_bound + TERMINAL_ROUND_BOUND * bounds.workers,
            "background load must also finish within the worker-scaled round bound"
        );
    }

    eprintln!(
        "multi-worker burst fairness (workers={}): {} waiters drained, \
         round latency min={min_latency} max={max_latency} \
         (drain bound {}, spread bound {})",
        bounds.workers, WAITERS, bounds.drain_round_bound, bounds.spread_round_bound
    );
}

fn spawn_multi_worker_waiter(
    runtime: &TokioRuntimeAdapter,
    fiber_index: usize,
    registered: &Arc<AtomicUsize>,
    round: &Arc<AtomicUsize>,
    observations: &Arc<Mutex<Vec<Option<MultiWorkerObservation>>>>,
) -> FiberHandle {
    let spec = fiber_spec(
        fiber_index,
        CancellationScopeId::from_bytes(id_bytes(900 + fiber_index)),
    );
    let handle = FiberHandle {
        fiber_id: spec.fiber_id,
        generation: spec.fiber_generation,
    };
    let adapter = runtime.clone();
    let registered = Arc::clone(registered);
    let round = Arc::clone(round);
    let observations = Arc::clone(observations);
    let body = async move {
        let wait = adapter
            .wait_for_operation(handle, operation(10_000 + fiber_index), Generation::INITIAL)
            .expect("wait registration");
        registered.fetch_add(1, Ordering::SeqCst);
        let outcome = wait.await;
        let round_latency = round.load(Ordering::SeqCst);
        observations.lock().expect("observations lock")[fiber_index] =
            Some(MultiWorkerObservation {
                outcome,
                round_latency,
            });
        FiberExit::Completed
    };
    runtime
        .spawn_fiber(spec, Box::pin(body))
        .expect("spawn waiter")
}

fn observed_multi_worker(
    observations: &Arc<Mutex<Vec<Option<MultiWorkerObservation>>>>,
) -> Vec<MultiWorkerObservation> {
    observations
        .lock()
        .expect("observations lock")
        .iter()
        .copied()
        .flatten()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burst_wake_is_fair_across_waiters_on_two_workers() {
    multi_worker_burst_fairness(MultiWorkerBounds::new(2)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn burst_wake_is_fair_across_waiters_on_four_workers() {
    multi_worker_burst_fairness(MultiWorkerBounds::new(4)).await;
}
