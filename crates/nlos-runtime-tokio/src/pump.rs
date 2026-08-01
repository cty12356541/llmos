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
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use nlos_outbox::{
    DrainReport, OutboxConsumer, OutboxError, OutboxItem, OutboxKind, OutboxSource, ReconcileSink,
};
use nlos_runtime::WakeSink;
use nlos_store::{OutboxEntry, SqliteOperationStore};
use nlos_types::{CallbackId, Generation, OperationId};

/// Default fallback poll interval when no delivery hint arrives.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Default number of consecutive drain failures that faults the pump.
const DEFAULT_FAILURE_THRESHOLD: usize = 16;

/// Exponential-backoff cap expressed as a multiple of the poll interval.
/// At the default 25ms interval the backoff sequence is
/// 25ms, 50ms, ..., capped at 1600ms.
const BACKOFF_CAP_MULTIPLE: u32 = 64;

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
            .map_err(|error| OutboxError::Source {
                detail: format!("pending_outbox read failed: {error}"),
            })?;
        entries.iter().map(map_entry).collect()
    }

    fn ack(&self, sequence: u64) -> Result<(), OutboxError> {
        let sequence = i64::try_from(sequence).map_err(|_| OutboxError::Source {
            detail: "outbox sequence exceeds the durable i64 domain".to_owned(),
        })?;
        self.store
            .acknowledge_outbox(sequence)
            .map_err(|error| OutboxError::Source {
                detail: format!("acknowledge_outbox commit failed: {error}"),
            })
    }
}

/// Translates one store row into the consumer-side item.
fn map_entry(entry: &OutboxEntry) -> Result<OutboxItem, OutboxError> {
    Ok(OutboxItem {
        sequence: u64::try_from(entry.sequence).map_err(|_| OutboxError::Source {
            detail: "negative outbox sequence".to_owned(),
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
    /// dropped hint delays delivery by at most this interval. It is also the
    /// base of the failure backoff: consecutive drain failures wait
    /// `poll_interval * 2^(n-1)`, capped at 64 × `poll_interval` (1600ms at
    /// the default interval).
    pub poll_interval: Duration,
    /// Consecutive drain failures (source errors or consumer panics) after
    /// which the pump transitions to [`PumpState::Faulted`] and the pump
    /// thread exits. A successful drain resets the counter.
    pub failure_threshold: usize,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
        }
    }
}

/// Lifecycle state of an [`OutboxPump`], as reported by [`OutboxPump::health`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PumpState {
    /// Draining normally.
    Running,
    /// The pump thread exited after too many consecutive drain failures;
    /// `stop()` joins immediately. The durable Outbox is untouched, so a
    /// fresh pump redelivers everything.
    Faulted,
    /// The pump thread exited cleanly: the runtime signalled shutdown
    /// through [`DrainReport::shutdown`] (terminal) or `stop()` was
    /// requested.
    Stopped,
}

/// Lock-free-readable health snapshot of an [`OutboxPump`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PumpHealth {
    /// Lifecycle state of the pump thread.
    pub state: PumpState,
    /// Consecutive failed drain attempts since the last successful drain.
    pub consecutive_failures: usize,
    /// `Display` text of the most recent drain failure; `None` after a
    /// successful drain.
    pub last_error: Option<String>,
}

const STATE_RUNNING: usize = 0;
const STATE_FAULTED: usize = 1;
const STATE_STOPPED: usize = 2;

/// Shared health counters written by the pump thread, read via `health()`.
struct PumpHealthInner {
    state: AtomicUsize,
    consecutive_failures: AtomicUsize,
    last_error: Mutex<Option<String>>,
}

impl PumpHealthInner {
    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Release);
        *lock(&self.last_error) = None;
    }

    /// Records one failed drain and returns the new failure count.
    fn record_failure(&self, error: String) -> usize {
        *lock(&self.last_error) = Some(error);
        self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn set_state(&self, state: PumpState) {
        let value = match state {
            PumpState::Running => STATE_RUNNING,
            PumpState::Faulted => STATE_FAULTED,
            PumpState::Stopped => STATE_STOPPED,
        };
        self.state.store(value, Ordering::Release);
    }

    fn snapshot(&self) -> PumpHealth {
        let state = match self.state.load(Ordering::Acquire) {
            STATE_FAULTED => PumpState::Faulted,
            STATE_STOPPED => PumpState::Stopped,
            _ => PumpState::Running,
        };
        PumpHealth {
            state,
            consecutive_failures: self.consecutive_failures.load(Ordering::Acquire),
            last_error: lock(&self.last_error).clone(),
        }
    }
}

/// Dedicated-thread driver for the durable Outbox consumer.
///
/// The pump owns one OS thread that repeatedly drains the consumer; blocking
/// `SQLite` I/O and sink applies therefore never run on a Tokio worker
/// (ADR-0001). The writer side interacts with the pump only through
/// [`OutboxPump::hint`], which is bounded and non-blocking.
///
/// Failure semantics: a failed drain (source error or consumer panic) is
/// counted in [`OutboxPump::health`] and retried with bounded exponential
/// backoff; reaching [`PumpConfig::failure_threshold`] transitions the pump
/// to [`PumpState::Faulted`] and ends the thread. A drain reporting
/// [`DrainReport::shutdown`] is terminal and transitions the pump to
/// [`PumpState::Stopped`]. Neither path ever spins silently.
pub struct OutboxPump {
    stop: Arc<AtomicBool>,
    hint: SyncSender<()>,
    health: Arc<PumpHealthInner>,
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
        let health = Arc::new(PumpHealthInner {
            state: AtomicUsize::new(STATE_RUNNING),
            consecutive_failures: AtomicUsize::new(0),
            last_error: Mutex::new(None),
        });
        // Capacity 1: one pending hint is enough to schedule a drain, and a
        // full channel makes `hint` drop instead of blocking the writer.
        let (hint, hints) = sync_channel::<()>(1);
        let worker = {
            let stop = Arc::clone(&stop);
            let health = Arc::clone(&health);
            std::thread::Builder::new()
                .name("nlos-outbox-pump".to_owned())
                .spawn(move || pump_loop(&consumer, &hints, &stop, &health, &config))
                .expect("spawn outbox pump thread")
        };
        Self {
            stop,
            hint,
            health,
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

    /// Current health snapshot: lock-free state and failure counter reads
    /// plus one short mutex acquisition for the last error text. Safe to
    /// call from any thread, including hot paths.
    #[must_use]
    pub fn health(&self) -> PumpHealth {
        self.health.snapshot()
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

/// One guarded drain attempt: a source/contract failure surfaces as error
/// text, and a consumer panic is caught (no `unsafe`) and reported the same
/// way so the pump thread can never die silently.
fn drain_guarded<S, W, R>(consumer: &OutboxConsumer<S, W, R>) -> Result<DrainReport, String>
where
    S: OutboxSource,
    W: WakeSink,
    R: ReconcileSink,
{
    match catch_unwind(AssertUnwindSafe(|| consumer.drain_once())) {
        Ok(Ok(report)) => Ok(report),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("consumer panicked".to_owned()),
    }
}

/// Bounded exponential backoff after the `failures`-th consecutive failure:
/// `poll_interval * 2^(failures-1)`, capped at 64 × `poll_interval`.
fn backoff(poll_interval: Duration, failures: usize) -> Duration {
    let shift = u32::try_from(failures.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(BACKOFF_CAP_MULTIPLE.trailing_zeros());
    poll_interval.saturating_mul(1_u32 << shift)
}

/// The pump thread body: drain until empty, then wait for a hint or the
/// fallback interval. A transient early stop (`stopped_at`, not a failure)
/// is retried after the same wait, so a stuck entry cannot spin the thread.
/// A failed drain backs off exponentially (bounded) and is observable via
/// health; too many consecutive failures fault the pump. A `shutdown` drain
/// report is terminal: the pump stops and leaves unacknowledged entries
/// durable for a future runtime (ADR-0002).
fn pump_loop<S, W, R>(
    consumer: &OutboxConsumer<S, W, R>,
    hints: &Receiver<()>,
    stop: &AtomicBool,
    health: &PumpHealthInner,
    config: &PumpConfig,
) where
    S: OutboxSource,
    W: WakeSink,
    R: ReconcileSink,
{
    'outer: while !stop.load(Ordering::Acquire) {
        loop {
            if stop.load(Ordering::Acquire) {
                break 'outer;
            }
            match drain_guarded(consumer) {
                Ok(report) => {
                    if report.shutdown {
                        health.set_state(PumpState::Stopped);
                        return;
                    }
                    health.record_success();
                    if report.polled > 0 && report.stopped_at.is_none() {
                        continue;
                    }
                    break;
                }
                Err(error) => {
                    let failures = health.record_failure(error);
                    if failures >= config.failure_threshold {
                        health.set_state(PumpState::Faulted);
                        return;
                    }
                    // The wait doubles per consecutive failure; a hint or the
                    // stop signal still wakes the thread out of it early.
                    let _ = hints.recv_timeout(backoff(config.poll_interval, failures));
                    continue 'outer;
                }
            }
        }
        let _ = hints.recv_timeout(config.poll_interval);
    }
    health.set_state(PumpState::Stopped);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
