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
use std::error::Error;
use std::fmt;
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

/// Floor for [`PumpConfig::poll_interval`], enforced at
/// [`OutboxPump::start`]: a sub-millisecond interval would make both the
/// success path (`recv_timeout`) and the failure backoff degenerate into an
/// unbacked busy-poll hammering the durable source. `poll_interval` is also
/// the base of the failure backoff, so the floor keeps every derived backoff
/// step non-zero by construction — there is no separate backoff knob to
/// misconfigure.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(1);

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
    /// the default interval). Must be at least 1ms — the floor is enforced
    /// at [`OutboxPump::start`], and being the backoff base it keeps every
    /// backoff step non-zero by construction. No upper bound is enforced: a
    /// long interval delays delivery but is a legitimate tuning, not a
    /// hazard.
    pub poll_interval: Duration,
    /// Consecutive drain failures (source errors or consumer panics) after
    /// which the pump transitions to [`PumpState::Faulted`] and the pump
    /// thread exits. A successful drain resets the counter. `0` is allowed
    /// and means the very first failure faults the pump.
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
    /// Consecutive failed drain attempts (source errors or consumer
    /// panics) since the last successful drain. Apply-phase stops are
    /// counted separately, in [`PumpHealth::consecutive_apply_failures`].
    pub consecutive_failures: usize,
    /// `Display` text of the most recent source/panic failure; `None`
    /// after a successful drain.
    pub last_error: Option<String>,
    /// Durable sequence of the entry the last drain stopped at without
    /// completing, when that stop was an apply or ACK failure (not
    /// shutdown); `None` after a pass that completed.
    pub stuck_sequence: Option<u64>,
    /// Consecutive drain passes that stopped at
    /// [`PumpHealth::stuck_sequence`]. Progress past the stuck entry (the
    /// stop moves to a later sequence, or a pass completes) resets this to
    /// zero, mirroring how a successful drain resets
    /// [`PumpHealth::consecutive_failures`].
    pub consecutive_apply_failures: usize,
    /// `Display` text of the most recent apply/ACK stop (the
    /// [`DrainStop`](nlos_outbox::DrainStop) error of the last stopped
    /// report); `None` after a pass that completed.
    pub last_apply_error: Option<String>,
}

const STATE_RUNNING: usize = 0;
const STATE_FAULTED: usize = 1;
const STATE_STOPPED: usize = 2;

/// The apply-failure half of the health state, written under one mutex so
/// the stuck sequence, its consecutive-failure count and the last error text
/// stay mutually consistent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PumpApplyHealth {
    stuck_sequence: Option<u64>,
    consecutive_failures: usize,
    last_error: Option<String>,
}

/// Shared health counters written by the pump thread, read via `health()`.
struct PumpHealthInner {
    state: AtomicUsize,
    consecutive_failures: AtomicUsize,
    last_error: Mutex<Option<String>>,
    apply: Mutex<PumpApplyHealth>,
}

impl PumpHealthInner {
    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Release);
        *lock(&self.last_error) = None;
        *lock(&self.apply) = PumpApplyHealth::default();
    }

    /// Records one failed drain and returns the new failure count.
    fn record_failure(&self, error: String) -> usize {
        *lock(&self.last_error) = Some(error);
        self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Records one apply/ACK stop at `sequence` and returns the new count of
    /// consecutive stops at that same sequence. A stop at a different
    /// sequence means the previous stuck entry finally progressed, so the
    /// count restarts at one for the new stuck entry.
    fn record_apply_failure(&self, sequence: u64, error: &str) -> usize {
        let mut apply = lock(&self.apply);
        if apply.stuck_sequence == Some(sequence) {
            apply.consecutive_failures += 1;
        } else {
            apply.stuck_sequence = Some(sequence);
            apply.consecutive_failures = 1;
        }
        apply.last_error = Some(error.to_owned());
        apply.consecutive_failures
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
        let apply = lock(&self.apply).clone();
        PumpHealth {
            state,
            consecutive_failures: self.consecutive_failures.load(Ordering::Acquire),
            last_error: lock(&self.last_error).clone(),
            stuck_sequence: apply.stuck_sequence,
            consecutive_apply_failures: apply.consecutive_failures,
            last_apply_error: apply.last_error,
        }
    }
}

/// Failure to start an [`OutboxPump`]: the OS refused to create the pump
/// thread. No pump state exists in this case and the durable Outbox is
/// untouched, so a caller may retry `start` after resolving the cause.
#[derive(Debug)]
pub enum OutboxPumpStartError {
    /// The configuration is unusable. Two shapes are rejected before the
    /// pump thread is spawned:
    ///
    /// - a [`PumpConfig::poll_interval`] below 1ms, which would turn the
    ///   success path (`recv_timeout`) into an unbacked busy-poll;
    /// - a consumer [`ConsumerConfig`](nlos_outbox::ConsumerConfig)
    ///   `batch_limit` of zero, which the consumer only debug-asserts — in a
    ///   release build it would poll empty batches forever while the pump
    ///   idled as if healthy.
    InvalidConfig(&'static str),
    /// The OS could not spawn the dedicated pump thread.
    Spawn(std::io::Error),
}

impl fmt::Display for OutboxPumpStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(reason) => {
                write!(formatter, "invalid outbox pump config: {reason}")
            }
            Self::Spawn(error) => write!(formatter, "could not spawn outbox pump thread: {error}"),
        }
    }
}

impl Error for OutboxPumpStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(_) => None,
            Self::Spawn(error) => Some(error),
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
    /// # Errors
    ///
    /// Returns [`OutboxPumpStartError::InvalidConfig`] when the combined
    /// configuration is unusable — a [`PumpConfig::poll_interval`] below
    /// [`MIN_POLL_INTERVAL`] (1ms) or a consumer `batch_limit` of zero; no
    /// pump state exists and no thread is spawned in either case. Returns
    /// [`OutboxPumpStartError::Spawn`] when the OS refuses to create the
    /// pump thread; no pump state exists in that case either.
    pub fn start<S, W, R>(
        consumer: OutboxConsumer<S, W, R>,
        config: PumpConfig,
    ) -> Result<Self, OutboxPumpStartError>
    where
        S: OutboxSource + 'static,
        W: WakeSink + 'static,
        R: ReconcileSink + 'static,
    {
        if config.poll_interval < MIN_POLL_INTERVAL {
            return Err(OutboxPumpStartError::InvalidConfig(
                "poll_interval must be at least 1ms",
            ));
        }
        // `batch_limit` lives on the consumer's config and is only
        // debug-asserted in `drain_once`; enforce it here so a release build
        // cannot spawn a pump that polls empty batches forever.
        if consumer.config.batch_limit == 0 {
            return Err(OutboxPumpStartError::InvalidConfig(
                "consumer batch_limit must be positive",
            ));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let health = Arc::new(PumpHealthInner {
            state: AtomicUsize::new(STATE_RUNNING),
            consecutive_failures: AtomicUsize::new(0),
            last_error: Mutex::new(None),
            apply: Mutex::new(PumpApplyHealth::default()),
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
                .map_err(OutboxPumpStartError::Spawn)?
        };
        Ok(Self {
            stop,
            hint,
            health,
            worker: Some(worker),
        })
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
/// fallback interval. An apply/ACK stop (`stopped_at` with a typed
/// [`DrainReport::failure`]) is a poison-entry candidate: the drain retried
/// the same durable sequence and it failed again, so the retry goes through
/// the same bounded exponential backoff channel as source failures
/// (keyed on the consecutive same-sequence count) and the stop is visible
/// in health (`stuck_sequence`, `consecutive_apply_failures`,
/// `last_apply_error`). Deliberately NO dead-letter and no skip: Outbox
/// rows are durable and undeletable, and sidelining an entry is a design
/// decision this pump does not take — it keeps retrying, bounded, with the
/// failure observable. A source/panic failure still backs off
/// exponentially and faults the pump past
/// [`PumpConfig::failure_threshold`]. A `shutdown` drain report is
/// terminal: the pump stops and leaves unacknowledged entries durable for
/// a future runtime (ADR-0002).
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
                    match (report.stopped_at, &report.failure) {
                        // Apply/ACK stop with its typed cause: count the
                        // consecutive stops at this sequence and retry the
                        // drain after the shared exponential backoff. A hint
                        // or the stop signal still wakes the thread early.
                        (Some(sequence), Some(failure)) => {
                            let failures = health.record_apply_failure(sequence, &failure.error);
                            let _ = hints.recv_timeout(backoff(config.poll_interval, failures));
                            continue 'outer;
                        }
                        // Completed pass (an early stop without a failure
                        // detail is unreachable after the shutdown return).
                        _ => health.record_success(),
                    }
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

#[cfg(test)]
mod tests {
    use super::OutboxPumpStartError;

    /// Given/When/Then: given a `Spawn` start error built from a synthetic
    /// `io` cause; when rendered through `Display`/`Debug`/`Error::source`;
    /// then the failed operation is named, the root cause is carried inline,
    /// and the `io` error stays reachable through the error chain.
    #[test]
    fn spawn_start_error_is_descriptive_and_chained() {
        let error =
            OutboxPumpStartError::Spawn(std::io::Error::other("synthetic thread exhaustion"));

        let text = error.to_string();
        assert!(
            text.contains("outbox pump thread"),
            "Display must name the failed operation: {text}"
        );
        assert!(
            text.contains("synthetic thread exhaustion"),
            "Display must carry the io root cause: {text}"
        );
        assert!(
            format!("{error:?}").contains("Spawn"),
            "Debug must expose the variant: {error:?}"
        );

        let source = std::error::Error::source(&error).expect("io cause stays chained");
        assert_eq!(source.to_string(), "synthetic thread exhaustion");
    }
}
