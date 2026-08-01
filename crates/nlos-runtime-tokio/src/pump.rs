//! Blocking pump glue between the durable Outbox and the Tokio runtime.
//!
//! Position in the stage-B closed loop (ADR-0001/ADR-0002): the `SQLite`
//! authority commits `WakeFiber`/`ReconcileEffect` entries in the same
//! transaction as the Operation terminal state; [`OutboxPump`] drives the
//! synchronous [`OutboxConsumer`] on a dedicated OS thread — never on a
//! Tokio worker, because the consumer performs blocking `SQLite` I/O.
//!
//! Wake-up is a bounded hint plus a fallback poll interval:
//!
//! - writers call [`OutboxPump::hint`] after a successful commit; the hint is
//!   a capacity-1 `try_send`, so a writer is never blocked by the consumer;
//! - the pump drains until the queue is empty, then waits for a hint or the
//!   configured poll interval, so a lost hint only delays delivery by one
//!   interval;
//! - apply happens outside the store lock (the consumer owns that contract),
//!   so the authority writer and cancel paths never wait on sink latency.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use nlos_outbox::{
    OutboxConsumer, OutboxError, OutboxItem, OutboxKind, OutboxSource, ReconcileSink,
};
use nlos_runtime::WakeSink;
use nlos_store::{OutboxEntry, SqliteOperationStore};
use nlos_types::{CallbackId, Generation, OperationId};

/// Default fallback poll interval when no delivery hint arrives.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// [`OutboxSource`] bridge over the `SQLite` authority.
///
/// The store is shared with the writer side through an [`Arc`]; the bridge
/// only ever issues short read/ACK transactions, so the single-writer
/// admission gate is never held across a sink apply.
#[derive(Clone)]
pub struct StoreOutboxSource {
    store: Arc<SqliteOperationStore>,
}

impl StoreOutboxSource {
    /// Wraps a shared authority store as an [`OutboxSource`].
    #[must_use]
    pub fn new(store: Arc<SqliteOperationStore>) -> Self {
        Self { store }
    }
}

impl OutboxSource for StoreOutboxSource {
    fn pending(&self, limit: usize) -> Result<Vec<OutboxItem>, OutboxError> {
        let entries = self
            .store
            .pending_outbox(limit)
            .map_err(|_| OutboxError::Source {
                detail: "pending_outbox read failed",
            })?;
        entries.iter().map(map_entry).collect()
    }

    fn ack(&self, sequence: u64) -> Result<(), OutboxError> {
        let sequence = i64::try_from(sequence).map_err(|_| OutboxError::Source {
            detail: "outbox sequence exceeds the durable i64 domain",
        })?;
        self.store
            .acknowledge_outbox(sequence)
            .map_err(|_| OutboxError::Source {
                detail: "acknowledge_outbox commit failed",
            })
    }
}

/// Translates one store row into the consumer-side item.
fn map_entry(entry: &OutboxEntry) -> Result<OutboxItem, OutboxError> {
    Ok(OutboxItem {
        sequence: u64::try_from(entry.sequence).map_err(|_| OutboxError::Source {
            detail: "negative outbox sequence",
        })?,
        kind: match entry.kind {
            nlos_store::OutboxKind::WakeFiber => OutboxKind::WakeFiber,
            nlos_store::OutboxKind::ReconcileEffect => OutboxKind::ReconcileEffect,
        },
        operation_id: entry.operation.operation_id,
        operation_generation: entry.operation.generation,
        owner_fiber: entry.owner_fiber,
        callback_id: entry.callback_id,
        state: entry.state,
    })
}

/// The idempotency key for one reconciliation effect.
type ReconcileKey = (OperationId, Generation, Option<CallbackId>);

/// Recording [`ReconcileSink`] for tests and `PoC` integration harnesses.
///
/// Applies are deduplicated per `(operation, operation generation, callback)`
/// so at-least-once redelivery records exactly one effect, matching the
/// idempotency the [`ReconcileSink`] contract requires. All recorded items
/// are observable through [`RecordingReconcileSink::records`].
#[derive(Clone, Default)]
pub struct RecordingReconcileSink {
    seen: Arc<Mutex<HashSet<ReconcileKey>>>,
    applied: Arc<Mutex<Vec<OutboxItem>>>,
}

impl RecordingReconcileSink {
    /// The deduplicated effects applied so far, in apply order.
    #[must_use]
    pub fn records(&self) -> Vec<OutboxItem> {
        lock(&self.applied).clone()
    }

    /// Number of deduplicated effects applied so far.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.applied).len()
    }

    /// Whether no effect has been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        lock(&self.applied).is_empty()
    }
}

impl ReconcileSink for RecordingReconcileSink {
    fn reconcile(&self, item: &OutboxItem) -> Result<(), OutboxError> {
        let key = (
            item.operation_id,
            item.operation_generation,
            item.callback_id,
        );
        if lock(&self.seen).insert(key) {
            lock(&self.applied).push(*item);
        }
        Ok(())
    }
}

/// Tuning for an [`OutboxPump`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PumpConfig {
    /// Fallback poll interval used when no delivery hint arrives. A lost or
    /// dropped hint delays delivery by at most this interval.
    pub poll_interval: Duration,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

/// Dedicated-thread driver for the durable Outbox consumer.
///
/// The pump owns one OS thread that repeatedly drains the consumer; blocking
/// `SQLite` I/O and sink applies therefore never run on a Tokio worker
/// (ADR-0001). The writer side interacts with the pump only through
/// [`OutboxPump::hint`], which is bounded and non-blocking.
pub struct OutboxPump {
    stop: Arc<AtomicBool>,
    hint: SyncSender<()>,
    worker: Option<JoinHandle<()>>,
}

impl OutboxPump {
    /// Spawns the pump thread driving `consumer`.
    ///
    /// # Panics
    ///
    /// Panics when the OS refuses to spawn the pump thread.
    #[must_use]
    pub fn start<S, W, R>(consumer: OutboxConsumer<S, W, R>, config: PumpConfig) -> Self
    where
        S: OutboxSource + 'static,
        W: WakeSink + 'static,
        R: ReconcileSink + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        // Capacity 1: one pending hint is enough to schedule a drain, and a
        // full channel makes `hint` drop instead of blocking the writer.
        let (hint, hints) = sync_channel::<()>(1);
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("nlos-outbox-pump".to_owned())
                .spawn(move || pump_loop(&consumer, &hints, &stop, config.poll_interval))
                .expect("spawn outbox pump thread")
        };
        Self {
            stop,
            hint,
            worker: Some(worker),
        }
    }

    /// Delivers a bounded wake-up hint after a commit returned.
    ///
    /// Returns `true` when the hint was queued. A `false` result means a hint
    /// was already pending (or the pump is stopping); the dropped hint is
    /// harmless because the pending drain observes the same committed
    /// entries, and the fallback poll interval bounds the worst case anyway.
    #[must_use]
    pub fn hint(&self) -> bool {
        self.hint.try_send(()).is_ok()
    }

    /// Signals the pump thread to stop and joins it.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the thread out of `recv_timeout`; the bounded channel may be
        // full, which is fine because the flag is the real stop signal.
        let _ = self.hint.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for OutboxPump {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The pump thread body: drain until empty, then wait for a hint or the
/// fallback interval. A drain that stops early (transient apply/ACK failure)
/// is retried after the same wait, so a stuck entry cannot spin the thread.
fn pump_loop<S, W, R>(
    consumer: &OutboxConsumer<S, W, R>,
    hints: &Receiver<()>,
    stop: &AtomicBool,
    poll_interval: Duration,
) where
    S: OutboxSource,
    W: WakeSink,
    R: ReconcileSink,
{
    while !stop.load(Ordering::Acquire) {
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match consumer.drain_once() {
                Ok(report) if report.polled > 0 && report.stopped_at.is_none() => {}
                Ok(_) | Err(_) => break,
            }
        }
        let _ = hints.recv_timeout(poll_interval);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
