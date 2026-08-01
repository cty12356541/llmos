//! Synchronous consumer core for the durable Operation Outbox.
//!
//! Position in the stage-B closed loop (ADR-0002): the persistent authority
//! commits `WakeFiber`/`ReconcileEffect` entries in the same transaction as
//! the Operation terminal state. This crate drains those entries in bounded
//! batches, applies each one idempotently to a [`WakeSink`] (runtime wake
//! delivery) or a [`ReconcileSink`] (late-effect reconciliation), and
//! acknowledges the entry only after the apply succeeded.
//!
//! Delivery is at-least-once: a crash after a successful apply but before the
//! ACK causes redelivery, and the sinks' per-`(operation, callback)`
//! idempotency absorbs the duplicate. A committed entry is never permanently
//! lost because unacknowledged entries are redelivered in durable sequence
//! order.
//!
//! This crate is pure synchronous code with no Tokio, `SQLite`, or locking of
//! its own; the async pump and the store adapter live in the integrating
//! runtime crate.

use std::error::Error;
use std::fmt;

use nlos_operation::OperationState;
use nlos_runtime::{FiberHandle, WakeOutcome, WakeSink};
use nlos_types::{CallbackId, Generation, OperationId};

/// The kind of a durable Outbox entry, mirroring the store-side taxonomy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxKind {
    /// Deliver the terminal Operation wake to the owner fiber.
    WakeFiber,
    /// Reconcile a late side effect whose wake permission was fenced.
    ReconcileEffect,
}

/// One durable Outbox entry as seen by the consumer.
///
/// This is the consumer-side mirror of the store's Outbox row; the store
/// adapter translates between the two so this crate never depends on a
/// storage backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutboxItem {
    /// Durable monotonic sequence assigned by the authority at commit time.
    pub sequence: u64,
    /// Which sink must apply this entry.
    pub kind: OutboxKind,
    /// Stable Operation identity.
    pub operation_id: OperationId,
    /// Operation generation fencing the entry.
    pub operation_generation: Generation,
    /// Fiber that owns the Operation at commit time.
    pub owner_fiber: FiberHandle,
    /// Accepted callback identity; `None` on the cancel-before-dispatch path.
    pub callback_id: Option<CallbackId>,
    /// Canonical terminal Operation state carried by the entry.
    pub state: OperationState,
}

/// Errors surfaced by Outbox sources and reconcile sinks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxError {
    /// The Outbox source could not serve a `pending` or `ack` call.
    Source {
        /// Static description of the failing operation.
        detail: &'static str,
    },
    /// The reconcile sink failed to apply an effect; redelivery is expected.
    Reconcile {
        /// Static description of the failing reconciliation.
        detail: &'static str,
    },
}

impl fmt::Display for OutboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source { detail } => write!(formatter, "outbox source error: {detail}"),
            Self::Reconcile { detail } => {
                write!(formatter, "outbox reconcile error: {detail}")
            }
        }
    }
}

impl Error for OutboxError {}

/// Read/acknowledge boundary over the durable Outbox.
///
/// Implementations bridge the persistent authority (for example the `SQLite`
/// store) to this crate. The consumer holds no lock across an apply; the
/// owned `Vec` returned by [`OutboxSource::pending`] is the entire handoff.
pub trait OutboxSource: Send {
    /// Returns up to `limit` unacknowledged entries.
    ///
    /// The returned entries MUST be in strictly ascending `sequence` order.
    /// The consumer never reorders them.
    ///
    /// # Errors
    ///
    /// Returns [`OutboxError::Source`] when the backing store cannot be read.
    fn pending(&self, limit: usize) -> Result<Vec<OutboxItem>, OutboxError>;

    /// Acknowledges the entry with `sequence` after it has been applied.
    ///
    /// Repeating an ACK MUST be safe; the authority may redeliver an entry
    /// whose ACK was lost to a crash.
    ///
    /// # Errors
    ///
    /// Returns [`OutboxError::Source`] when the ACK cannot be committed.
    fn ack(&self, sequence: u64) -> Result<(), OutboxError>;
}

/// Idempotent sink for `ReconcileEffect` entries.
pub trait ReconcileSink: Send {
    /// Applies one reconciliation effect.
    ///
    /// Implementations MUST be idempotent per `(operation, callback)` pair so
    /// that at-least-once redelivery never applies the effect twice.
    ///
    /// # Errors
    ///
    /// Returns [`OutboxError::Reconcile`] only for transient failures where
    /// redelivery is meaningful; the consumer then stops the batch and the
    /// entry is retried on a later drain.
    fn reconcile(&self, item: &OutboxItem) -> Result<(), OutboxError>;
}

/// Tuning for one drain pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsumerConfig {
    /// Maximum number of entries polled and applied per [`OutboxConsumer::drain_once`].
    pub batch_limit: usize,
}

/// Outcome of one [`OutboxConsumer::drain_once`] pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrainReport {
    /// Entries returned by the source for this pass.
    pub polled: usize,
    /// Entries successfully applied to their sink (applied implies
    /// wake/reconcile returned successfully, including permanent terminal
    /// wake outcomes).
    pub applied: usize,
    /// Entries whose ACK was committed during this pass.
    pub acked: usize,
    /// Sequence of the first entry that stopped the batch, if any. That
    /// entry and every entry after it were neither applied (past the failing
    /// apply) nor acknowledged, and will be redelivered on a later drain.
    pub stopped_at: Option<u64>,
}

/// Bounded, in-order, idempotent consumer of the durable Operation Outbox.
///
/// The consumer is synchronous and holds no state between passes; drive it
/// from any scheduler. Wake outcomes [`WakeOutcome::FiberGone`] and
/// [`WakeOutcome::NotWaiting`] are permanent terminal conditions and are
/// acknowledged like [`WakeOutcome::Delivered`] so they cannot poison the
/// queue.
pub struct OutboxConsumer<S: OutboxSource, W: WakeSink, R: ReconcileSink> {
    /// Durable entry source and ACK boundary.
    pub source: S,
    /// Runtime wake delivery sink.
    pub wake_sink: W,
    /// Late-effect reconciliation sink.
    pub reconcile_sink: R,
    /// Drain tuning.
    pub config: ConsumerConfig,
}

impl<S: OutboxSource, W: WakeSink, R: ReconcileSink> OutboxConsumer<S, W, R> {
    /// Polls one bounded batch and applies it in durable sequence order.
    ///
    /// Each entry is applied to its sink first and acknowledged second. The
    /// batch stops at the first transient apply failure or failed ACK: that
    /// entry and all later entries are left unacknowledged for redelivery,
    /// and the stop is reported through [`DrainReport::stopped_at`] as
    /// `Ok(report)` because backpressure is a normal path, not an error. The
    /// consumer never reorders, skips, or deduplicates entries; replayed
    /// entries are applied again and absorbed by sink idempotency.
    ///
    /// # Errors
    ///
    /// Returns [`OutboxError::Source`] when the source cannot serve
    /// `pending`, or when the returned batch violates the strictly ascending
    /// sequence contract (a source bug the consumer must not paper over by
    /// reordering).
    pub fn drain_once(&self) -> Result<DrainReport, OutboxError> {
        let items = self.source.pending(self.config.batch_limit)?;
        let mut report = DrainReport {
            polled: items.len(),
            ..DrainReport::default()
        };
        if items
            .windows(2)
            .any(|pair| pair[1].sequence <= pair[0].sequence)
        {
            return Err(OutboxError::Source {
                detail: "pending entries are not in strictly ascending sequence order",
            });
        }
        for item in &items {
            let applied = match item.kind {
                OutboxKind::WakeFiber => match self.wake_sink.wake(
                    &item.owner_fiber,
                    item.operation_id,
                    item.operation_generation,
                ) {
                    // `FiberGone`/`NotWaiting` are permanent terminal states;
                    // acknowledging them prevents poison-entry loops.
                    Ok(
                        WakeOutcome::Delivered | WakeOutcome::FiberGone | WakeOutcome::NotWaiting,
                    ) => true,
                    Err(_) => false,
                },
                OutboxKind::ReconcileEffect => self.reconcile_sink.reconcile(item).is_ok(),
            };
            if !applied {
                report.stopped_at = Some(item.sequence);
                return Ok(report);
            }
            report.applied += 1;
            if self.source.ack(item.sequence).is_err() {
                // The entry was applied; replay relies on sink idempotency.
                report.stopped_at = Some(item.sequence);
                return Ok(report);
            }
            report.acked += 1;
        }
        Ok(report)
    }
}
