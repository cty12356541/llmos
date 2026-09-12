//! Tokio-backed implementation of the runtime-independent NLOS fiber contract.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use nlos_runtime::{
    ActivationUsage, FiberExit, FiberFuture, FiberHandle, FiberSpec, FiberState, RuntimeAdapter,
    RuntimeError,
};
use nlos_types::{CancellationScopeId, ExecutionFiberId, Generation};
use tokio::runtime::Handle;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

mod channel_wait;
mod metrics;
mod pump;
mod replay;
mod snapshot;
mod wake;

use channel_wait::ChannelWaitKey;
pub use channel_wait::{
    ChannelSequenceWait, ChannelWaitError, DeliveryReport, RearmReport, RearmedChannelWait,
    TokioChannelWakeSink,
};
pub use pump::{
    OutboxPump, OutboxPumpStartError, PumpConfig, PumpHealth, PumpState, RecordingReconcileSink,
    StoreOutboxSource,
};
pub use replay::{
    BindingEventProjection, BindingReplay, BindingReplayEvent, ReplayAuthorities,
    ReplayedEffectEvent, ReplayedQueueConsumptionEvent, ReplayedWaitEvent, ResumableBinding,
    ResumePlan, ResumeRejection, ResumeReport,
};
pub use snapshot::{SnapshotResumable, SnapshotResumeReport};
pub use wake::{OperationWait, TokioWakeSink, WaitOutcome};
use wake::{WaitEntry, WaitKey};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ScopeKey {
    id: CancellationScopeId,
    generation: Generation,
}

struct CancellationScope {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationScope {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

enum TerminalOutcome {
    Pending,
    Finished(FiberExit),
}

/// Scheduler / durable-wait lifecycle phase exposed at the runtime boundary.
///
/// Complements [`FiberState`] with metering-specific wait kinds that are not
/// yet modeled as distinct `FiberState` variants (e.g. admission backpressure).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FiberLifecyclePhase {
    #[default]
    Running,
    WaitingExternal,
    BackpressureWait,
    Suspended,
}

/// Read-side aggregate of lifecycle metering dimensions across live fibers.
///
/// Prefix inspect surface linking `backpressure_wait` and `suspended`
/// metering; [`Self::to_open_metrics_text`] renders a minimal `OpenMetrics` /
/// Prometheus text exposition prefix, not a full export (no scrape
/// endpoint, auth, or retention).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LifecycleMeterAggregate {
    pub total_backpressure_wait: Duration,
    pub total_suspended: Duration,
    pub sampled_fibers: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum UsagePhase {
    #[default]
    None,
    Running(Instant),
    WaitingExternal(Instant),
    BackpressureWait(Instant),
    Suspended(Instant),
}

struct UsageAccumulator {
    usage: ActivationUsage,
    phase: UsagePhase,
}

impl UsageAccumulator {
    fn enter_running(&mut self, now: Instant) {
        match self.phase {
            UsagePhase::WaitingExternal(since) => {
                self.usage.external_wait += now.saturating_duration_since(since);
                self.phase = UsagePhase::Running(now);
            }
            UsagePhase::BackpressureWait(since) => {
                self.usage.backpressure_wait += now.saturating_duration_since(since);
                self.phase = UsagePhase::Running(now);
            }
            UsagePhase::Suspended(since) => {
                self.usage.suspended += now.saturating_duration_since(since);
                self.phase = UsagePhase::Running(now);
            }
            UsagePhase::None => {
                self.phase = UsagePhase::Running(now);
            }
            UsagePhase::Running(_) => {}
        }
    }

    fn enter_waiting(&mut self, now: Instant) {
        match self.phase {
            UsagePhase::Running(since) => {
                self.usage.active_cpu += now.saturating_duration_since(since);
                self.phase = UsagePhase::WaitingExternal(now);
            }
            UsagePhase::None => {
                self.phase = UsagePhase::WaitingExternal(now);
            }
            UsagePhase::WaitingExternal(_)
            | UsagePhase::BackpressureWait(_)
            | UsagePhase::Suspended(_) => {}
        }
    }

    fn enter_backpressure_wait(&mut self, now: Instant) {
        match self.phase {
            UsagePhase::Running(since) => {
                self.usage.active_cpu += now.saturating_duration_since(since);
                self.phase = UsagePhase::BackpressureWait(now);
            }
            UsagePhase::None => {
                self.phase = UsagePhase::BackpressureWait(now);
            }
            UsagePhase::WaitingExternal(_)
            | UsagePhase::BackpressureWait(_)
            | UsagePhase::Suspended(_) => {}
        }
    }

    fn enter_suspended(&mut self, now: Instant) {
        match self.phase {
            UsagePhase::Running(since) => {
                self.usage.active_cpu += now.saturating_duration_since(since);
                self.phase = UsagePhase::Suspended(now);
            }
            UsagePhase::None => {
                self.phase = UsagePhase::Suspended(now);
            }
            UsagePhase::WaitingExternal(_)
            | UsagePhase::BackpressureWait(_)
            | UsagePhase::Suspended(_) => {}
        }
    }

    fn finalize(&mut self, now: Instant) {
        if let UsagePhase::Running(since) = self.phase {
            self.usage.active_cpu += now.saturating_duration_since(since);
        }
        if let UsagePhase::WaitingExternal(since) = self.phase {
            self.usage.external_wait += now.saturating_duration_since(since);
        }
        if let UsagePhase::BackpressureWait(since) = self.phase {
            self.usage.backpressure_wait += now.saturating_duration_since(since);
        }
        if let UsagePhase::Suspended(since) = self.phase {
            self.usage.suspended += now.saturating_duration_since(since);
        }
        self.phase = UsagePhase::None;
    }

    fn snapshot(&self, now: Instant) -> ActivationUsage {
        let mut usage = self.usage;
        if let UsagePhase::Running(since) = self.phase {
            usage.active_cpu += now.saturating_duration_since(since);
        }
        if let UsagePhase::WaitingExternal(since) = self.phase {
            usage.external_wait += now.saturating_duration_since(since);
        }
        if let UsagePhase::BackpressureWait(since) = self.phase {
            usage.backpressure_wait += now.saturating_duration_since(since);
        }
        if let UsagePhase::Suspended(since) = self.phase {
            usage.suspended += now.saturating_duration_since(since);
        }
        usage
    }
}

struct FiberRecord {
    generation: Generation,
    scope: Arc<CancellationScope>,
    state: Mutex<FiberState>,
    lifecycle_phase: Mutex<FiberLifecyclePhase>,
    usage: Mutex<UsageAccumulator>,
    accepted_at: Instant,
    terminal: Mutex<TerminalOutcome>,
    terminal_notify: Condvar,
}

impl FiberRecord {
    fn new(generation: Generation, scope: Arc<CancellationScope>) -> Self {
        Self {
            generation,
            scope,
            state: Mutex::new(FiberState::Ready),
            lifecycle_phase: Mutex::new(FiberLifecyclePhase::Running),
            usage: Mutex::new(UsageAccumulator {
                usage: ActivationUsage::default(),
                phase: UsagePhase::None,
            }),
            accepted_at: Instant::now(),
            terminal: Mutex::new(TerminalOutcome::Pending),
            terminal_notify: Condvar::new(),
        }
    }

    fn finish(&self, exit: FiberExit) {
        let mut terminal = lock_unpoisoned(&self.terminal);
        if matches!(*terminal, TerminalOutcome::Pending) {
            *terminal = TerminalOutcome::Finished(exit);
            self.terminal_notify.notify_all();
        }
    }

    /// Terminal transition that writes `elapsed_wall` and closes the open
    /// metering phase in one critical section at the caller-supplied
    /// `finished_at`; a separate terminal timestamp could close the final
    /// segment past the wall interval's endpoint and break the
    /// `active_cpu <= elapsed_wall` invariant.
    fn finish_terminal(&self, state: FiberState, started_at: Instant, finished_at: Instant) {
        let exit = fiber_exit_from_state(state);
        {
            let mut usage = lock_unpoisoned(&self.usage);
            usage.usage.elapsed_wall = finished_at.saturating_duration_since(started_at);
            usage.finalize(finished_at);
        }
        *lock_unpoisoned(&self.state) = state;
        self.finish(exit);
    }

    /// Waits for the fiber generation's terminal outcome.
    ///
    /// Fail-closed across the runtime shutdown boundary: `shutdown_flag` set
    /// by [`TokioRuntimeAdapter::shutdown`] makes a still-pending join return
    /// [`RuntimeError::ShuttingDown`] instead of parking forever on a fiber
    /// the executor may never finish. A generation that already reached a
    /// terminal state still joins normally after shutdown; the terminal
    /// outcome is checked first.
    fn join(&self, shutdown_flag: &AtomicBool) -> Result<FiberExit, RuntimeError> {
        let mut terminal = lock_unpoisoned(&self.terminal);
        loop {
            if let TerminalOutcome::Finished(exit) = *terminal {
                return Ok(exit);
            }
            if shutdown_flag.load(Ordering::Acquire) {
                return Err(RuntimeError::ShuttingDown);
            }
            terminal = self
                .terminal_notify
                .wait(terminal)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Wakes joiners parked in [`Self::join`] across the runtime shutdown
    /// boundary without fabricating a terminal outcome: the woken joiner
    /// re-checks the adapter's shutdown flag and fails with
    /// [`RuntimeError::ShuttingDown`]. Notifying while holding the terminal
    /// mutex closes the lost-wakeup window between a joiner's flag check and
    /// its condvar park. Terminal fibers have no parked joiner to fail.
    fn notify_shutdown(&self) {
        let terminal = lock_unpoisoned(&self.terminal);
        if matches!(*terminal, TerminalOutcome::Pending) {
            self.terminal_notify.notify_all();
        }
    }

    fn set_state(&self, state: FiberState) {
        let now = Instant::now();
        let current = *lock_unpoisoned(&self.state);
        if state == FiberState::Running
            && matches!(
                current,
                FiberState::WaitingIo | FiberState::WaitingModel | FiberState::Suspended
            )
        {
            return;
        }
        {
            let mut usage = lock_unpoisoned(&self.usage);
            match state {
                FiberState::Running => usage.enter_running(now),
                FiberState::Completed | FiberState::Failed | FiberState::Cancelled => {
                    usage.finalize(now);
                }
                _ => {}
            }
        }
        *lock_unpoisoned(&self.state) = state;
    }

    /// Marks a newly admitted fiber as running for inspection without starting
    /// CPU metering; the task body begins metering on first poll.
    fn set_state_without_metering(&self, state: FiberState) {
        *lock_unpoisoned(&self.state) = state;
    }

    /// Best-effort transition into `WaitingIo` while an Operation wait is
    /// registered. Never resurrects a terminal fiber.
    fn begin_wait(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if matches!(*state, FiberState::Ready | FiberState::Running) {
            let now = Instant::now();
            lock_unpoisoned(&self.usage).enter_waiting(now);
            *lock_unpoisoned(&self.lifecycle_phase) = FiberLifecyclePhase::WaitingExternal;
            *state = FiberState::WaitingIo;
        }
    }

    /// Best-effort transition back to `Running` after a delivered wake.
    /// Never overwrites a state set by the fiber lifecycle itself.
    fn resume_from_wait(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if *state == FiberState::WaitingIo {
            let now = Instant::now();
            lock_unpoisoned(&self.usage).enter_running(now);
            *lock_unpoisoned(&self.lifecycle_phase) = FiberLifecyclePhase::Running;
            *state = FiberState::Running;
        }
    }

    /// Scheduler/admission backpressure boundary: leaves `Running` and meters
    /// `backpressure_wait` until [`Self::resume_from_backpressure_wait`].
    fn begin_backpressure_wait(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if matches!(*state, FiberState::Ready | FiberState::Running) {
            let now = Instant::now();
            lock_unpoisoned(&self.usage).enter_backpressure_wait(now);
            *lock_unpoisoned(&self.lifecycle_phase) = FiberLifecyclePhase::BackpressureWait;
            *state = FiberState::WaitingModel;
        }
    }

    fn resume_from_backpressure_wait(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if *state == FiberState::WaitingModel {
            let now = Instant::now();
            lock_unpoisoned(&self.usage).enter_running(now);
            *lock_unpoisoned(&self.lifecycle_phase) = FiberLifecyclePhase::Running;
            *state = FiberState::Running;
        }
    }

    /// Cooperative suspend boundary: leaves `Running` and meters `suspended`
    /// until [`Self::resume_from_suspended`].
    fn begin_suspended(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if matches!(*state, FiberState::Ready | FiberState::Running) {
            let now = Instant::now();
            lock_unpoisoned(&self.usage).enter_suspended(now);
            *lock_unpoisoned(&self.lifecycle_phase) = FiberLifecyclePhase::Suspended;
            *state = FiberState::Suspended;
        }
    }

    fn resume_from_suspended(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if *state == FiberState::Suspended {
            let now = Instant::now();
            lock_unpoisoned(&self.usage).enter_running(now);
            *lock_unpoisoned(&self.lifecycle_phase) = FiberLifecyclePhase::Running;
            *state = FiberState::Running;
        }
    }

    fn lifecycle_phase_snapshot(&self) -> FiberLifecyclePhase {
        *lock_unpoisoned(&self.lifecycle_phase)
    }

    fn activation_usage_snapshot(&self) -> ActivationUsage {
        let usage = lock_unpoisoned(&self.usage);
        usage.snapshot(Instant::now())
    }
}

struct Inner {
    fibers: Mutex<HashMap<ExecutionFiberId, Arc<FiberRecord>>>,
    scopes: Mutex<HashMap<ScopeKey, Arc<CancellationScope>>>,
    waits: Mutex<HashMap<WaitKey, WaitEntry>>,
    channel_waits: Mutex<HashMap<ChannelWaitKey, WaitEntry>>,
    shutdown: AtomicBool,
    admission: Arc<Semaphore>,
}

/// Configuration for a [`TokioRuntimeAdapter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokioRuntimeConfig {
    /// Maximum number of admitted, non-terminal fibers.
    pub max_live_fibers: usize,
}

impl Default for TokioRuntimeConfig {
    fn default() -> Self {
        Self {
            max_live_fibers: 10_000,
        }
    }
}

/// A Tokio executor adapter that preserves NLOS identity and cancellation.
#[derive(Clone)]
pub struct TokioRuntimeAdapter {
    handle: Handle,
    inner: Arc<Inner>,
}

impl TokioRuntimeAdapter {
    /// Creates an adapter attached to an existing Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::QueueFull`] when `max_live_fibers` is zero.
    pub fn new(handle: Handle, config: TokioRuntimeConfig) -> Result<Self, RuntimeError> {
        if config.max_live_fibers == 0 {
            return Err(RuntimeError::QueueFull);
        }

        Ok(Self {
            handle,
            inner: Arc::new(Inner {
                fibers: Mutex::new(HashMap::new()),
                scopes: Mutex::new(HashMap::new()),
                waits: Mutex::new(HashMap::new()),
                channel_waits: Mutex::new(HashMap::new()),
                shutdown: AtomicBool::new(false),
                admission: Arc::new(Semaphore::new(config.max_live_fibers)),
            }),
        })
    }

    #[must_use]
    pub fn registered_fibers(&self) -> usize {
        lock_unpoisoned(&self.inner.fibers).len()
    }

    fn scope_for(&self, spec: &FiberSpec) -> Result<Arc<CancellationScope>, RuntimeError> {
        let key = ScopeKey {
            id: spec.cancellation_scope_id,
            generation: spec.cancellation_generation,
        };
        let mut scopes = lock_unpoisoned(&self.inner.scopes);

        if scopes
            .keys()
            .any(|existing| existing.id == key.id && existing.generation != key.generation)
        {
            return Err(RuntimeError::InvalidGeneration);
        }

        Ok(Arc::clone(
            scopes
                .entry(key)
                .or_insert_with(|| Arc::new(CancellationScope::new())),
        ))
    }

    fn record_for(&self, handle: FiberHandle) -> Result<Arc<FiberRecord>, RuntimeError> {
        let fibers = lock_unpoisoned(&self.inner.fibers);
        let record = fibers
            .get(&handle.fiber_id)
            .ok_or(RuntimeError::InvalidGeneration)?;
        if record.generation != handle.generation {
            return Err(RuntimeError::InvalidGeneration);
        }
        Ok(Arc::clone(record))
    }

    /// Returns the scheduler/durable-wait lifecycle phase for inspection.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale.
    pub fn inspect_lifecycle_phase(
        &self,
        handle: FiberHandle,
    ) -> Result<FiberLifecyclePhase, RuntimeError> {
        let record = self.record_for(handle)?;
        Ok(record.lifecycle_phase_snapshot())
    }

    /// Marks a live fiber as blocked on scheduler/admission backpressure.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale.
    pub fn begin_backpressure_wait(&self, handle: FiberHandle) -> Result<(), RuntimeError> {
        let record = self.record_for(handle)?;
        record.begin_backpressure_wait();
        Ok(())
    }

    /// Resumes a fiber from scheduler/admission backpressure.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale.
    pub fn resume_from_backpressure_wait(&self, handle: FiberHandle) -> Result<(), RuntimeError> {
        let record = self.record_for(handle)?;
        record.resume_from_backpressure_wait();
        Ok(())
    }

    /// Marks a live fiber as cooperatively suspended.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale.
    pub fn begin_suspended(&self, handle: FiberHandle) -> Result<(), RuntimeError> {
        let record = self.record_for(handle)?;
        record.begin_suspended();
        Ok(())
    }

    /// Resumes a cooperatively suspended fiber.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale.
    pub fn resume_from_suspended(&self, handle: FiberHandle) -> Result<(), RuntimeError> {
        let record = self.record_for(handle)?;
        record.resume_from_suspended();
        Ok(())
    }

    /// Sums `backpressure_wait` and `suspended` over every fiber in the internal registry.
    ///
    /// Complexity is O(n) in the number of live fibers.
    #[must_use]
    pub fn inspect_lifecycle_meter_aggregate(&self) -> LifecycleMeterAggregate {
        let fibers = lock_unpoisoned(&self.inner.fibers);
        let mut aggregate = LifecycleMeterAggregate {
            sampled_fibers: fibers.len(),
            ..LifecycleMeterAggregate::default()
        };
        for record in fibers.values() {
            let usage = record.activation_usage_snapshot();
            aggregate.total_backpressure_wait += usage.backpressure_wait;
            aggregate.total_suspended += usage.suspended;
        }
        aggregate
    }
}

impl RuntimeAdapter for TokioRuntimeAdapter {
    fn spawn_fiber(
        &self,
        spec: FiberSpec,
        future: FiberFuture,
    ) -> Result<FiberHandle, RuntimeError> {
        // Gate order, fail-closed: runtime shutdown first, so a spawn across
        // the shutdown boundary never reaches `handle.spawn` on a dead
        // executor (which would panic) and never registers an unrunnable
        // fiber record. The gate closes the deterministic vector, not the
        // race itself: a `shutdown()` running concurrently can still flip
        // the flag after this check and the spawn then returns `Ok` —
        // while the runtime stays alive the fiber is driven as usual, and
        // an executor that has already died behaves exactly as it did
        // before the gate existed.
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown);
        }

        if spec
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            return Err(RuntimeError::DeadlineExceeded);
        }

        let permit = Arc::clone(&self.inner.admission)
            .try_acquire_owned()
            .map_err(|_| RuntimeError::QueueFull)?;
        let scope = self.scope_for(&spec)?;
        if scope.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }

        let record = Arc::new(FiberRecord::new(spec.fiber_generation, Arc::clone(&scope)));
        {
            let mut fibers = lock_unpoisoned(&self.inner.fibers);
            if let Some(existing) = fibers.get(&spec.fiber_id) {
                return Err(if existing.generation == spec.fiber_generation {
                    RuntimeError::DuplicateFiber
                } else {
                    RuntimeError::InvalidGeneration
                });
            }
            fibers.insert(spec.fiber_id, Arc::clone(&record));
        }
        // Admission succeeded: the generation is live from the caller's view
        // even before the executor polls the spawned task body.
        record.set_state_without_metering(FiberState::Running);

        let handle = FiberHandle {
            fiber_id: spec.fiber_id,
            generation: spec.fiber_generation,
        };
        let task_record = Arc::clone(&record);
        let task_inner = Arc::clone(&self.inner);
        self.handle.spawn(async move {
            run_fiber(spec, future, scope, task_record, permit, task_inner).await;
        });
        Ok(handle)
    }

    fn cancel_scope(
        &self,
        scope_id: CancellationScopeId,
        generation: Generation,
    ) -> Result<(), RuntimeError> {
        let scopes = lock_unpoisoned(&self.inner.scopes);
        let scope = scopes
            .get(&ScopeKey {
                id: scope_id,
                generation,
            })
            .ok_or(RuntimeError::InvalidGeneration)?;
        scope.cancel();
        Ok(())
    }

    fn inspect(&self, handle: FiberHandle) -> Result<FiberState, RuntimeError> {
        let record = self.record_for(handle)?;
        let state = *lock_unpoisoned(&record.state);
        Ok(state)
    }

    fn activation_usage(&self, handle: FiberHandle) -> Result<ActivationUsage, RuntimeError> {
        let record = self.record_for(handle)?;
        Ok(record.activation_usage_snapshot())
    }

    fn join_fiber(&self, handle: FiberHandle) -> Result<FiberExit, RuntimeError> {
        let record = self.record_for(handle)?;
        record.join(&self.inner.shutdown)
    }

    fn detach_fiber(&self, handle: FiberHandle) -> Result<(), RuntimeError> {
        let _record = self.record_for(handle)?;
        Ok(())
    }
}

async fn run_fiber(
    spec: FiberSpec,
    future: FiberFuture,
    scope: Arc<CancellationScope>,
    record: Arc<FiberRecord>,
    _permit: OwnedSemaphorePermit,
    inner: Arc<Inner>,
) {
    let started_at = Instant::now();
    {
        let mut usage = lock_unpoisoned(&record.usage);
        usage.usage.scheduler_wait = started_at.saturating_duration_since(record.accepted_at);
    }
    record.set_state(FiberState::Running);
    let guarded_future = AssertUnwindSafe(future).catch_unwind();

    let state = if scope.is_cancelled() {
        FiberState::Cancelled
    } else if let Some(deadline) = spec.deadline {
        tokio::select! {
            biased;
            () = scope.cancelled() => FiberState::Cancelled,
            () = tokio::time::sleep_until(deadline.into()) => FiberState::Cancelled,
            result = guarded_future => terminal_result(result),
        }
    } else {
        tokio::select! {
            biased;
            () = scope.cancelled() => FiberState::Cancelled,
            result = guarded_future => terminal_result(result),
        }
    };

    let finished_at = Instant::now();
    // The terminal transition and the wait-registry purges share one critical
    // section, so a wake either observes the live fiber (and hands off) or the
    // terminal state (and reports `NotWaiting`), never an orphaned buffer.
    // Both wait registries — Operation and Channel sequence — are purged for
    // the terminated fiber generation, resolving their waits as `Cancelled`.
    let mut waits = lock_unpoisoned(&inner.waits);
    let mut channel_waits = lock_unpoisoned(&inner.channel_waits);
    record.finish_terminal(state, started_at, finished_at);
    waits.retain(|key, _entry| !key.for_fiber(spec.fiber_id, spec.fiber_generation));
    channel_waits.retain(|key, _entry| !key.for_fiber(spec.fiber_id, spec.fiber_generation));
}

fn fiber_exit_from_state(state: FiberState) -> FiberExit {
    match state {
        FiberState::Completed => FiberExit::Completed,
        FiberState::Failed => FiberExit::Failed,
        FiberState::Cancelled => FiberExit::Cancelled,
        other => {
            debug_assert!(
                false,
                "fiber_exit_from_state called with non-terminal state: {other:?}"
            );
            FiberExit::Failed
        }
    }
}

const fn terminal_state(exit: nlos_runtime::FiberExit) -> FiberState {
    match exit {
        nlos_runtime::FiberExit::Completed => FiberState::Completed,
        nlos_runtime::FiberExit::Failed => FiberState::Failed,
        nlos_runtime::FiberExit::Cancelled => FiberState::Cancelled,
    }
}

fn terminal_result(
    result: Result<nlos_runtime::FiberExit, Box<dyn std::any::Any + Send>>,
) -> FiberState {
    result.map_or(FiberState::Failed, terminal_state)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
