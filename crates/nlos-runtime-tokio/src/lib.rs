//! Tokio-backed implementation of the runtime-independent NLOS fiber contract.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use futures_util::FutureExt;
use nlos_runtime::{
    ActivationUsage, FiberFuture, FiberHandle, FiberSpec, FiberState, RuntimeAdapter, RuntimeError,
};
use nlos_types::{CancellationScopeId, ExecutionFiberId, Generation};
use tokio::runtime::Handle;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

mod pump;
mod wake;

pub use pump::{
    OutboxPump, OutboxPumpStartError, PumpConfig, PumpHealth, PumpState, RecordingReconcileSink,
    StoreOutboxSource,
};
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

struct FiberRecord {
    generation: Generation,
    scope: Arc<CancellationScope>,
    state: Mutex<FiberState>,
    usage: Mutex<ActivationUsage>,
    accepted_at: Instant,
}

impl FiberRecord {
    fn new(generation: Generation, scope: Arc<CancellationScope>) -> Self {
        Self {
            generation,
            scope,
            state: Mutex::new(FiberState::Ready),
            usage: Mutex::new(ActivationUsage::default()),
            accepted_at: Instant::now(),
        }
    }

    fn set_state(&self, state: FiberState) {
        *lock_unpoisoned(&self.state) = state;
    }

    /// Best-effort transition into `WaitingIo` while an Operation wait is
    /// registered. Never resurrects a terminal fiber.
    fn begin_wait(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if matches!(*state, FiberState::Ready | FiberState::Running) {
            *state = FiberState::WaitingIo;
        }
    }

    /// Best-effort transition back to `Running` after a delivered wake.
    /// Never overwrites a state set by the fiber lifecycle itself.
    fn resume_from_wait(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if *state == FiberState::WaitingIo {
            *state = FiberState::Running;
        }
    }
}

struct Inner {
    fibers: Mutex<HashMap<ExecutionFiberId, Arc<FiberRecord>>>,
    scopes: Mutex<HashMap<ScopeKey, Arc<CancellationScope>>>,
    waits: Mutex<HashMap<WaitKey, WaitEntry>>,
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
}

impl RuntimeAdapter for TokioRuntimeAdapter {
    fn spawn_fiber(
        &self,
        spec: FiberSpec,
        future: FiberFuture,
    ) -> Result<FiberHandle, RuntimeError> {
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
        let usage = *lock_unpoisoned(&record.usage);
        Ok(usage)
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
        usage.scheduler_wait = started_at.saturating_duration_since(record.accepted_at);
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
    {
        let mut usage = lock_unpoisoned(&record.usage);
        usage.elapsed_wall = finished_at.saturating_duration_since(started_at);
    }
    // The terminal transition and the wait-registry purge share one critical
    // section, so a wake either observes the live fiber (and hands off) or the
    // terminal state (and reports `NotWaiting`), never an orphaned buffer.
    let mut waits = lock_unpoisoned(&inner.waits);
    record.set_state(state);
    waits.retain(|key, _entry| !key.for_fiber(spec.fiber_id, spec.fiber_generation));
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
