//! Durable Operation wake delivery for the Tokio runtime adapter.
//!
//! This module is the runtime endpoint of the Outbox closed loop: an
//! at-least-once consumer hands terminal Operation wakes to [`TokioWakeSink`],
//! and fibers register their side of the handshake with
//! [`TokioRuntimeAdapter::wait_for_operation`]. Delivery is idempotent per
//! `(fiber, fiber generation, operation, operation generation)` key and never
//! blocks on fiber execution.

use std::collections::hash_map::Entry;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

use nlos_runtime::{FiberHandle, FiberState, RuntimeError, WakeOutcome, WakeSink};
use nlos_types::{ExecutionFiberId, Generation, OperationId};
use tokio::sync::oneshot;

use crate::{Inner, TokioRuntimeAdapter, lock_unpoisoned};

/// The identity of one logical Operation wait: the waiting fiber generation
/// plus the awaited Operation identity and generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct WaitKey {
    fiber_id: ExecutionFiberId,
    fiber_generation: Generation,
    operation_id: OperationId,
    operation_generation: Generation,
}

impl WaitKey {
    const fn new(
        fiber: &FiberHandle,
        operation_id: OperationId,
        operation_generation: Generation,
    ) -> Self {
        Self {
            fiber_id: fiber.fiber_id,
            fiber_generation: fiber.generation,
            operation_id,
            operation_generation,
        }
    }

    /// Whether this wait belongs to the given fiber generation. The runtime
    /// lifecycle uses it to purge a terminated fiber's waits.
    pub(crate) fn for_fiber(&self, fiber_id: ExecutionFiberId, generation: Generation) -> bool {
        self.fiber_id == fiber_id && self.fiber_generation == generation
    }
}

/// The delivery state of one registered or buffered Operation wake.
pub(crate) enum WaitEntry {
    /// A live wait; the sender fires exactly once and the entry is removed.
    Pending(oneshot::Sender<()>),
    /// A wake that arrived before its wait was registered. The next
    /// registration for the same key consumes it and resolves immediately.
    Buffered,
}

const fn is_terminal(state: FiberState) -> bool {
    matches!(
        state,
        FiberState::Completed | FiberState::Failed | FiberState::Cancelled
    )
}

/// Removes a still-pending entry so a later wake observes no wait and
/// buffers instead of firing a dead sender.
fn remove_pending(inner: &Inner, key: &WaitKey) {
    let mut waits = lock_unpoisoned(&inner.waits);
    if matches!(waits.get(key), Some(WaitEntry::Pending(_))) {
        waits.remove(key);
    }
}

/// How an [`OperationWait`] resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    /// The matching durable wake was delivered to the wait.
    Woken,
    /// The wait ended without a wake: the cancellation scope was cancelled,
    /// the fiber terminated, or the runtime shut down.
    Cancelled,
}

/// A future that resolves exactly once for a registered Operation wait.
///
/// The wait resolves to [`WaitOutcome::Woken`] when the matching wake is
/// delivered, and to [`WaitOutcome::Cancelled`] when the fiber's cancellation
/// scope is cancelled or the fiber terminates first. It never pends forever.
#[must_use = "an OperationWait does nothing unless awaited"]
pub struct OperationWait {
    driver: Pin<Box<dyn Future<Output = WaitOutcome> + Send>>,
}

impl OperationWait {
    /// A wait that has already resolved before it is handed out.
    fn ready(outcome: WaitOutcome) -> Self {
        Self {
            driver: Box::pin(async move { outcome }),
        }
    }
}

impl Future for OperationWait {
    type Output = WaitOutcome;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<WaitOutcome> {
        self.driver.as_mut().poll(context)
    }
}

/// The Tokio-backed [`WakeSink`] endpoint for durable Operation wakes.
///
/// Shares its fiber and wait registries with the [`TokioRuntimeAdapter`] it
/// was created from via [`TokioRuntimeAdapter::wake_sink`]. All delivery
/// semantics of the [`WakeSink`] contract — generation fencing, per-key
/// idempotency, and non-blocking handoff — are implemented in [`Self::wake`].
#[derive(Clone)]
pub struct TokioWakeSink {
    inner: Arc<Inner>,
}

impl WakeSink for TokioWakeSink {
    fn wake(
        &self,
        fiber: &FiberHandle,
        operation_id: OperationId,
        operation_generation: Generation,
    ) -> Result<WakeOutcome, RuntimeError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown);
        }
        let key = WaitKey::new(fiber, operation_id, operation_generation);

        let record = {
            let fibers = lock_unpoisoned(&self.inner.fibers);
            match fibers.get(&fiber.fiber_id) {
                Some(record) if record.generation == fiber.generation => Arc::clone(record),
                _ => return Ok(WakeOutcome::FiberGone),
            }
        };

        // Lock order matches `run_fiber`'s terminal transition: `waits` first,
        // then the fiber record's state. Holding `waits` here means the purge
        // cannot interleave, so a wake either observes a live fiber and hands
        // off, or observes the terminal state and reports `NotWaiting` — never
        // an orphaned buffer.
        let mut waits = lock_unpoisoned(&self.inner.waits);
        if is_terminal(*lock_unpoisoned(&record.state)) {
            return Ok(WakeOutcome::NotWaiting);
        }
        match waits.entry(key) {
            Entry::Occupied(entry) => {
                if matches!(entry.get(), WaitEntry::Pending(_)) {
                    // Consuming the entry is what makes a duplicate delivery a
                    // no-op instead of a second logical wake. The signal is
                    // the entire handoff: `wake` never awaits the fiber.
                    if let WaitEntry::Pending(sender) = entry.remove() {
                        let _delivered = sender.send(());
                        record.resume_from_wait();
                    }
                }
            }
            Entry::Vacant(entry) => {
                // Early wake on a live fiber: buffer by key so a later
                // registration for the same Operation resolves immediately.
                entry.insert(WaitEntry::Buffered);
            }
        }
        // A repeat wake that hits a buffered or just-consumed key still
        // reports `Delivered`, as at-least-once redelivery requires.
        Ok(WakeOutcome::Delivered)
    }
}

impl TokioRuntimeAdapter {
    /// Creates the [`WakeSink`] endpoint sharing this adapter's fiber and
    /// wait registries.
    #[must_use]
    pub fn wake_sink(&self) -> TokioWakeSink {
        TokioWakeSink {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Marks the runtime as shutting down: subsequent wakes fail with
    /// [`RuntimeError::ShuttingDown`] and every currently registered wait
    /// resolves as [`WaitOutcome::Cancelled`], so no wait can pend forever
    /// across the shutdown boundary. The flag is one-way.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Release);
        lock_unpoisoned(&self.inner.waits).clear();
    }

    /// Registers a wait for the terminal wake of `operation_id` +
    /// `operation_generation` on behalf of `handle`.
    ///
    /// If a wake for the same key already arrived (at-least-once delivery can
    /// precede registration), the returned wait resolves immediately with
    /// [`WaitOutcome::Woken`]. If the fiber is already terminal or its scope
    /// already cancelled, it resolves immediately with
    /// [`WaitOutcome::Cancelled`].
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when `handle` is stale or
    /// unknown, and [`RuntimeError::ShuttingDown`] after [`Self::shutdown`].
    pub fn wait_for_operation(
        &self,
        handle: FiberHandle,
        operation_id: OperationId,
        operation_generation: Generation,
    ) -> Result<OperationWait, RuntimeError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown);
        }
        let key = WaitKey::new(&handle, operation_id, operation_generation);
        let record = self.record_for(handle)?;

        // Same lock order as `run_fiber`'s terminal transition (`waits`, then
        // the record's state), so registration and the lifecycle purge are
        // mutually exclusive: either the purge removed this fiber's entries
        // and the state reads terminal here, or the entry is registered and a
        // later purge drops its sender, resolving the wait as `Cancelled`.
        let mut waits = lock_unpoisoned(&self.inner.waits);
        if is_terminal(*lock_unpoisoned(&record.state)) || record.scope.is_cancelled() {
            return Ok(OperationWait::ready(WaitOutcome::Cancelled));
        }
        if matches!(waits.get(&key), Some(WaitEntry::Buffered)) {
            // The wake arrived before the registration: consume the buffer
            // and resolve immediately.
            waits.remove(&key);
            return Ok(OperationWait::ready(WaitOutcome::Woken));
        }
        let (sender, receiver) = oneshot::channel();
        // A second registration for the same key supersedes the first: the
        // dropped sender resolves the superseded wait as `Cancelled`.
        waits.insert(key, WaitEntry::Pending(sender));
        record.begin_wait();
        let inner = Arc::clone(&self.inner);
        let scope = Arc::clone(&record.scope);
        drop(waits);

        Ok(OperationWait {
            driver: Box::pin(async move {
                // Create the `Notified` future before checking the flag: a
                // cancellation racing this point is then observed either by
                // the flag or by `notify_waiters`, never lost in between.
                let cancelled = scope.notify.notified();
                tokio::pin!(cancelled);
                if scope.is_cancelled() {
                    remove_pending(&inner, &key);
                    return WaitOutcome::Cancelled;
                }
                tokio::select! {
                    biased;
                    () = &mut cancelled => {
                        remove_pending(&inner, &key);
                        WaitOutcome::Cancelled
                    }
                    result = receiver => match result {
                        Ok(()) => WaitOutcome::Woken,
                        // The sender was dropped without a signal: the fiber
                        // terminated (lifecycle purge) or the runtime shut
                        // down. The wait must not pend forever.
                        Err(_) => WaitOutcome::Cancelled,
                    },
                }
            }),
        })
    }
}
