//! Tokio-backed implementation of the runtime-independent NLOS fiber contract.

use std::collections::{HashMap, VecDeque};
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
    /// A join consumed the exit: the record was reaped from the registry and
    /// its tombstone pushed. A joiner still holding an `Arc` clone of the
    /// record sees this state and fails with
    /// [`RuntimeError::FiberReaped`] instead of double-consuming the exit.
    Consumed,
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
    fiber_id: ExecutionFiberId,
    generation: Generation,
    scope: Arc<CancellationScope>,
    state: Mutex<FiberState>,
    lifecycle_phase: Mutex<FiberLifecyclePhase>,
    usage: Mutex<UsageAccumulator>,
    accepted_at: Instant,
    terminal: Mutex<TerminalOutcome>,
    terminal_notify: Condvar,
    /// FIBER-REAP-003: set by `detach_fiber` on a live record so the terminal
    /// transition reclaims it. Read inside the terminal critical section.
    reap_on_terminal: AtomicBool,
}

impl FiberRecord {
    fn new(
        fiber_id: ExecutionFiberId,
        generation: Generation,
        scope: Arc<CancellationScope>,
    ) -> Self {
        Self {
            fiber_id,
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
            reap_on_terminal: AtomicBool::new(false),
        }
    }

    fn finish(&self, exit: FiberExit, inner: &Inner) {
        let mut terminal = lock_unpoisoned(&self.terminal);
        if matches!(*terminal, TerminalOutcome::Pending) {
            *terminal = TerminalOutcome::Finished(exit);
            self.terminal_notify.notify_all();
            // FIBER-REAP-003: a detached live fiber is reclaimed inside this
            // terminal critical section — the same section a consuming join
            // uses — so the record never outlives its terminal transition.
            if self.reap_on_terminal.load(Ordering::Acquire) {
                inner.reap_fiber(self.fiber_id, self.generation);
            }
        }
    }

    /// Terminal transition that writes `elapsed_wall` and closes the open
    /// metering phase in one critical section at the caller-supplied
    /// `finished_at`; a separate terminal timestamp could close the final
    /// segment past the wall interval's endpoint and break the
    /// `active_cpu <= elapsed_wall` invariant.
    fn finish_terminal(
        &self,
        state: FiberState,
        started_at: Instant,
        finished_at: Instant,
        inner: &Inner,
    ) {
        let exit = fiber_exit_from_state(state);
        {
            let mut usage = lock_unpoisoned(&self.usage);
            usage.usage.elapsed_wall = finished_at.saturating_duration_since(started_at);
            usage.finalize(finished_at);
        }
        *lock_unpoisoned(&self.state) = state;
        self.finish(exit, inner);
    }

    /// Waits for the fiber generation's terminal outcome; a successful wait
    /// consumes it (FIBER-REAP-001): inside the terminal critical section the
    /// record is reaped from the registry, its tombstone is pushed, and the
    /// outcome transitions to [`TerminalOutcome::Consumed`] so a racing
    /// second joiner holding an `Arc` clone cannot double-consume the exit.
    ///
    /// Fail-closed across the runtime shutdown boundary: the shutdown flag
    /// set by [`TokioRuntimeAdapter::shutdown`] makes a still-pending join
    /// return [`RuntimeError::ShuttingDown`] instead of parking forever on a
    /// fiber the executor may never finish. A generation that already reached
    /// a terminal state still joins normally after shutdown; the terminal
    /// outcome is checked first.
    fn join(&self, inner: &Inner) -> Result<FiberExit, RuntimeError> {
        let mut terminal = lock_unpoisoned(&self.terminal);
        loop {
            if let TerminalOutcome::Finished(exit) = *terminal {
                *terminal = TerminalOutcome::Consumed;
                inner.reap_fiber(self.fiber_id, self.generation);
                return Ok(exit);
            }
            if matches!(*terminal, TerminalOutcome::Consumed) {
                return Err(RuntimeError::FiberReaped {
                    fiber_id: self.fiber_id,
                    generation: self.generation,
                });
            }
            if inner.shutdown.load(Ordering::Acquire) {
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

/// The fiber registry and its bounded tombstone ring under one lock
/// (FIBER-REAP-001..005): the "record exists **or** tombstone hit" checks of
/// join/detach/spawn are atomic against the reap path, and a reap is a
/// single lock acquisition. Tombstones are pure memory (FIBER-REAP-005) and
/// FIFO-bounded — a full ring evicts its oldest entry, after which the
/// evicted identity is spawnable as a new fiber.
struct FiberRegistry {
    fibers: HashMap<ExecutionFiberId, Arc<FiberRecord>>,
    tombstones: VecDeque<(ExecutionFiberId, Generation)>,
    tombstone_capacity: usize,
}

impl FiberRegistry {
    fn new(tombstone_capacity: usize) -> Self {
        Self {
            fibers: HashMap::new(),
            tombstones: VecDeque::new(),
            tombstone_capacity,
        }
    }

    fn len(&self) -> usize {
        self.fibers.len()
    }

    fn get(&self, fiber_id: &ExecutionFiberId) -> Option<&Arc<FiberRecord>> {
        self.fibers.get(fiber_id)
    }

    fn values(&self) -> std::collections::hash_map::Values<'_, ExecutionFiberId, Arc<FiberRecord>> {
        self.fibers.values()
    }

    fn insert(&mut self, fiber_id: ExecutionFiberId, record: Arc<FiberRecord>) {
        self.fibers.insert(fiber_id, record);
    }

    /// Resolves a handle against records first, then tombstones
    /// (FIBER-REAP-002): a live record with a mismatched generation is stale
    /// (`InvalidGeneration`); an absent record whose `(id, generation)` is in
    /// the tombstone ring was reaped (`FiberReaped`); anything else keeps the
    /// pre-existing unknown-handle rejection.
    fn resolve(&self, handle: FiberHandle) -> Result<Arc<FiberRecord>, RuntimeError> {
        if let Some(record) = self.fibers.get(&handle.fiber_id) {
            if record.generation != handle.generation {
                return Err(RuntimeError::InvalidGeneration);
            }
            return Ok(Arc::clone(record));
        }
        if self.tombstoned(&handle.fiber_id, handle.generation) {
            return Err(RuntimeError::FiberReaped {
                fiber_id: handle.fiber_id,
                generation: handle.generation,
            });
        }
        Err(RuntimeError::InvalidGeneration)
    }

    /// Whether the tombstone ring still fences this exact `(id, generation)`.
    fn tombstoned(&self, fiber_id: &ExecutionFiberId, generation: Generation) -> bool {
        self.tombstones
            .iter()
            .any(|(id, reaped_generation)| id == fiber_id && *reaped_generation == generation)
    }

    /// Removes the record and pushes its tombstone. Idempotent: a record that
    /// is already gone (a racing consumer won the terminal critical section)
    /// pushes nothing, so the ring never holds duplicates from one identity.
    fn reap(&mut self, fiber_id: ExecutionFiberId, generation: Generation) {
        if self.fibers.remove(&fiber_id).is_none() {
            return;
        }
        // `0` is a zero-capacity ring: pure consumption, no window protection.
        if self.tombstone_capacity == 0 {
            return;
        }
        while self.tombstones.len() >= self.tombstone_capacity {
            self.tombstones.pop_front();
        }
        self.tombstones.push_back((fiber_id, generation));
    }
}

struct Inner {
    fibers: Mutex<FiberRegistry>,
    scopes: Mutex<HashMap<ScopeKey, Arc<CancellationScope>>>,
    waits: Mutex<HashMap<WaitKey, WaitEntry>>,
    channel_waits: Mutex<HashMap<ChannelWaitKey, WaitEntry>>,
    shutdown: AtomicBool,
    admission: Arc<Semaphore>,
}

impl Inner {
    /// Reaps a fiber record from the registry. Callers hold the record's
    /// terminal mutex (the join consumption path, the detach terminal path,
    /// or `run_fiber`'s terminal transition), so the reap is ordered inside
    /// that existing critical section; the registry lock is a leaf relative
    /// to every record lock — no path acquires a record lock while holding
    /// it — so the added `terminal → registry` edge cannot cycle.
    fn reap_fiber(&self, fiber_id: ExecutionFiberId, generation: Generation) {
        lock_unpoisoned(&self.fibers).reap(fiber_id, generation);
    }
}

/// Configuration for a [`TokioRuntimeAdapter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokioRuntimeConfig {
    /// Maximum number of admitted, non-terminal fibers.
    pub max_live_fibers: usize,
    /// Capacity of the fiber tombstone ring (FIBER-REAP-001..005): reaped
    /// `(fiber_id, generation)` pairs stay fenced (`DuplicateFiber` on
    /// re-spawn, `FiberReaped` on re-join) until FIFO-evicted. `0` is a
    /// zero-capacity ring — pure consumption, no window protection.
    pub tombstone_capacity: usize,
    /// Capacity of the scope tombstone ring (SCOPE-IDX-003, reserved for the
    /// scope-registry task): `0` is a zero-capacity ring. Defined here so the
    /// lifecycle-reaping configuration surface is stable across the W25
    /// tasks; this adapter does not read it yet.
    pub scope_tombstone_capacity: usize,
    /// Bound of the orphaned `channel_waits` buffer (ORPHAN-001, reserved
    /// for the orphan-bound task): `0` drops every orphaned entry. Defined
    /// here so the lifecycle-reaping configuration surface is stable across
    /// the W25 tasks; this adapter does not read it yet.
    pub orphan_buffer_capacity: usize,
}

impl Default for TokioRuntimeConfig {
    fn default() -> Self {
        Self {
            max_live_fibers: 10_000,
            tombstone_capacity: 65_536,
            scope_tombstone_capacity: 65_536,
            orphan_buffer_capacity: 1_024,
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
                fibers: Mutex::new(FiberRegistry::new(config.tombstone_capacity)),
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
        lock_unpoisoned(&self.inner.fibers).resolve(handle)
    }

    /// Returns the scheduler/durable-wait lifecycle phase for inspection.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale,
    /// and [`RuntimeError::FiberReaped`] when the generation's record was
    /// already reaped (consumed by a join or reclaimed by a detach,
    /// FIBER-REAP-002 via the shared handle resolution).
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
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale,
    /// and [`RuntimeError::FiberReaped`] when the generation's record was
    /// already reaped (consumed by a join or reclaimed by a detach,
    /// FIBER-REAP-002 via the shared handle resolution).
    pub fn begin_backpressure_wait(&self, handle: FiberHandle) -> Result<(), RuntimeError> {
        let record = self.record_for(handle)?;
        record.begin_backpressure_wait();
        Ok(())
    }

    /// Resumes a fiber from scheduler/admission backpressure.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale,
    /// and [`RuntimeError::FiberReaped`] when the generation's record was
    /// already reaped (consumed by a join or reclaimed by a detach,
    /// FIBER-REAP-002 via the shared handle resolution).
    pub fn resume_from_backpressure_wait(&self, handle: FiberHandle) -> Result<(), RuntimeError> {
        let record = self.record_for(handle)?;
        record.resume_from_backpressure_wait();
        Ok(())
    }

    /// Marks a live fiber as cooperatively suspended.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale,
    /// and [`RuntimeError::FiberReaped`] when the generation's record was
    /// already reaped (consumed by a join or reclaimed by a detach,
    /// FIBER-REAP-002 via the shared handle resolution).
    pub fn begin_suspended(&self, handle: FiberHandle) -> Result<(), RuntimeError> {
        let record = self.record_for(handle)?;
        record.begin_suspended();
        Ok(())
    }

    /// Resumes a cooperatively suspended fiber.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale,
    /// and [`RuntimeError::FiberReaped`] when the generation's record was
    /// already reaped (consumed by a join or reclaimed by a detach,
    /// FIBER-REAP-002 via the shared handle resolution).
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
        let registry = lock_unpoisoned(&self.inner.fibers);
        let mut aggregate = LifecycleMeterAggregate {
            sampled_fibers: registry.len(),
            ..LifecycleMeterAggregate::default()
        };
        for record in registry.values() {
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

        let record = Arc::new(FiberRecord::new(
            spec.fiber_id,
            spec.fiber_generation,
            Arc::clone(&scope),
        ));
        {
            let mut registry = lock_unpoisoned(&self.inner.fibers);
            if let Some(existing) = registry.get(&spec.fiber_id) {
                return Err(if existing.generation == spec.fiber_generation {
                    RuntimeError::DuplicateFiber
                } else {
                    RuntimeError::InvalidGeneration
                });
            }
            // FIBER-REAP-004: the duplicate fence extends past the record's
            // lifetime — a reaped `(id, generation)` whose tombstone is still
            // in the ring stays `DuplicateFiber` (window protection). An
            // evicted tombstone fences nothing: the identity spawns as a new
            // fiber.
            if registry.tombstoned(&spec.fiber_id, spec.fiber_generation) {
                return Err(RuntimeError::DuplicateFiber);
            }
            registry.insert(spec.fiber_id, Arc::clone(&record));
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
        record.join(&self.inner)
    }

    fn detach_fiber(&self, handle: FiberHandle) -> Result<(), RuntimeError> {
        let record = self.record_for(handle)?;
        // FIBER-REAP-003: mark first, then inspect the terminal outcome under
        // its mutex. Either the terminal transition observes the flag and
        // reaps inside its own critical section, or this inspection observes
        // `Finished` and reaps in the same critical section a consuming join
        // would use — the double-check closes the detach/terminal race, and
        // `reap` itself is idempotent.
        record.reap_on_terminal.store(true, Ordering::Release);
        let terminal = lock_unpoisoned(&record.terminal);
        if matches!(*terminal, TerminalOutcome::Finished(_)) {
            self.inner.reap_fiber(record.fiber_id, record.generation);
        }
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
    // A fiber marked `reap_on_terminal` is additionally reclaimed inside the
    // terminal critical section reached below (FIBER-REAP-003).
    let mut waits = lock_unpoisoned(&inner.waits);
    let mut channel_waits = lock_unpoisoned(&inner.channel_waits);
    record.finish_terminal(state, started_at, finished_at, &inner);
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
