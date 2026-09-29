//! Production Outbox wiring for the Slice K runtime: the fail-closed
//! reconcile sink, its inspectable refusal surface, and the bounded-stop
//! helper used by [`SliceKRuntime`]'s pump lifecycle.
//!
//! Position in the closed loop: terminal Operation commits write
//! `WakeFiber`/`ReconcileEffect` rows into `operation_outbox` in the same
//! transaction. [`SliceKRuntime::start_pump`] drives the landed
//! [`OutboxPump`](nlos_runtime_tokio::OutboxPump) over the shared
//! `SqliteOperationStore`: wake entries route to the
//! [`TokioWakeSink`](nlos_runtime_tokio::TokioWakeSink) of the caller's
//! runtime adapter, and reconcile entries route to
//! [`FailClosedReconcileSink`] below — explicitly not consumable in this
//! slice, never silently dropped.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nlos_outbox::{OutboxError, OutboxItem, ReconcileSink};

/// Snapshot of the fail-closed reconcile lane: how many `ReconcileEffect`
/// applications the slice refused, and the most recent typed reason.
///
/// This is the health surface that makes "explicitly not consumable"
/// observable: the durable outbox keeps the entries (they are never
/// acknowledged), and this counter proves the refusals are routing
/// decisions, not lost wake-ups.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileRefusalSnapshot {
    /// Total refused `ReconcileEffect` applications since `open` (monotonic;
    /// at-least-once redelivery deliberately counts each retry).
    pub total: u64,
    /// `Display` text of the most recent refusal reason; `None` when nothing
    /// was refused yet.
    pub last_detail: Option<String>,
}

/// Interior-shared refusal statistics between the runtime and the sink.
#[derive(Default)]
struct ReconcileRefusals {
    total: AtomicU64,
    last_detail: Mutex<Option<String>>,
}

impl ReconcileRefusals {
    fn record(&self, detail: String) {
        self.total.fetch_add(1, Ordering::AcqRel);
        *self.lock() = Some(detail);
    }

    fn snapshot(&self) -> ReconcileRefusalSnapshot {
        ReconcileRefusalSnapshot {
            total: self.total.load(Ordering::Acquire),
            last_detail: self.lock().clone(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<String>> {
        self.last_detail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Fail-closed [`ReconcileSink`] for the Slice K lane.
///
/// Why not a direct connection to the task authority's
/// `reconcile_effect` API: that API takes a `ReconcileRequest` naming
/// `task_id`, `permit_id`, `permit_epoch`, `effect_seq`, the durable
/// `adoption_receipt_id` the reconcile runs under, the outcome, and a
/// gateway closure-proof digest. An outbox [`OutboxItem`] carries only the
/// Operation identity/generation, owner fiber, callback id, and terminal
/// state — none of the task-plane parameters are derivable, and inventing a
/// mapping would add semantics this slice must not add. So the sink fails
/// closed with a typed [`OutboxError::Reconcile`]: the consumer stops the
/// batch at that entry, the pump backs off and retries, the entry stays
/// durable (never silently acknowledged), and every refusal is counted on
/// the runtime's inspectable refusal surface.
///
/// A drain stopped by this sink is treated by the landed pump as
/// backpressure (not a drain failure), so the pump stays `Running` and
/// re-offers the entry every poll interval; visibility lives in
/// [`SliceKRuntime::reconcile_refusals`] plus the never-shrinking
/// `pending_outbox` prefix.
pub struct FailClosedReconcileSink {
    refusals: Arc<ReconcileRefusals>,
}

impl FailClosedReconcileSink {
    /// Builds the sink sharing `refusals` with the runtime that will expose
    /// it. The stats handle is created once per runtime and survives pump
    /// restarts.
    fn new(refusals: Arc<ReconcileRefusals>) -> Self {
        Self { refusals }
    }
}

impl ReconcileSink for FailClosedReconcileSink {
    fn reconcile(&self, item: &OutboxItem) -> Result<(), OutboxError> {
        let detail = format!(
            "slice-k has no reconcile consumer for operation {} generation {} \
             (callback {:?}, terminal state {:?}): the task-authority \
             reconcile_effect API requires task/permit/adoption parameters \
             the outbox entry does not carry; the entry stays durable for \
             redelivery instead of being acknowledged away",
            crate::short_hex(item.operation_id.as_bytes()),
            item.operation_generation.get(),
            item.callback_id,
            item.state,
        );
        self.refusals.record(detail.clone());
        Err(OutboxError::Reconcile { detail })
    }
}

/// How long [`SliceKRuntime`]'s `Drop` waits for the pump thread to join
/// before giving up on the join (the stop flag and wake-up hint are already
/// delivered, so the pump thread still exits; only the joiner stops
/// waiting). Far above any healthy stop latency — the pump thread wakes
/// from its hint immediately.
const STOP_JOIN_DEADLINE: Duration = Duration::from_secs(5);

/// Stops `pump` with a bounded join.
///
/// The landed `OutboxPump::stop` joins its thread without a timeout, and a
/// stuck consumer (for example blocked on storage I/O) would otherwise hang
/// runtime teardown forever. This helper performs the stop on a short-lived
/// helper thread and waits at most [`STOP_JOIN_DEADLINE`]:
///
/// - `true`: the pump thread joined within the deadline — teardown is clean
///   and no thread is left behind;
/// - `false`: the deadline expired. The stop flag was set and the wake-up
///   hint delivered before the wait, so the pump thread still observes the
///   shutdown request; the detached helper thread finishes the join once
///   the pump thread exits. Dropping the runtime must not hang on a lane
///   the process is tearing down anyway.
pub(crate) fn stop_pump_bounded(pump: nlos_runtime_tokio::OutboxPump) -> bool {
    let (done, done_rx) = std::sync::mpsc::channel::<()>();
    let helper = std::thread::Builder::new()
        .name("nlos-slice-k-pump-stop".to_owned())
        .spawn(move || {
            pump.stop();
            let _ = done.send(());
        });
    match helper {
        Ok(_) => done_rx.recv_timeout(STOP_JOIN_DEADLINE).is_ok(),
        // Thread spawn itself failed (resource exhaustion): the closure is
        // dropped here, and dropping an `OutboxPump` delivers the same
        // stop-and-join inline, so the shutdown signal still reaches the
        // pump thread — only the bound is lost this one time.
        Err(_) => true,
    }
}

/// The pump lane one runtime owns: the shared refusal stats the sink
/// reports into, plus the construction point for the sink itself.
pub(crate) struct PumpLane {
    refusals: Arc<ReconcileRefusals>,
}

impl PumpLane {
    pub(crate) fn new() -> Self {
        Self {
            refusals: Arc::new(ReconcileRefusals::default()),
        }
    }

    pub(crate) fn sink(&self) -> FailClosedReconcileSink {
        FailClosedReconcileSink::new(Arc::clone(&self.refusals))
    }

    pub(crate) fn snapshot(&self) -> ReconcileRefusalSnapshot {
        self.refusals.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use nlos_operation::OperationState;
    use nlos_outbox::{OutboxError, OutboxKind, ReconcileSink};
    use nlos_runtime::FiberHandle;
    use nlos_types::{ExecutionFiberId, Generation, OperationId, ReceiptId};

    use super::PumpLane;

    /// Given/When/Then: given a fresh refusal lane; when a reconcile item is
    /// offered to the fail-closed sink; then the sink returns the typed
    /// `Reconcile` error (never `Ok`), the refusal counter advances, and the
    /// recorded detail names the unconsumable entry so the health surface
    /// can point at the durable outbox row.
    #[test]
    fn fail_closed_sink_refuses_visibly() {
        let lane = PumpLane::new();
        let sink = lane.sink();
        let item = nlos_outbox::OutboxItem {
            sequence: 7,
            kind: OutboxKind::ReconcileEffect,
            operation_id: OperationId::from_bytes([1_u8; 16]),
            operation_generation: Generation::INITIAL,
            owner_fiber: FiberHandle {
                fiber_id: ExecutionFiberId::from_bytes([2_u8; 16]),
                generation: Generation::INITIAL,
            },
            callback_id: None,
            state: OperationState::EffectUnknown {
                receipt_id: ReceiptId::from_bytes([3_u8; 16]),
            },
        };

        let error = sink.reconcile(&item).expect_err("sink must fail closed");
        match error {
            OutboxError::Reconcile { detail } => {
                assert!(detail.contains("slice-k has no reconcile consumer"));
                assert!(detail.contains("stays durable"));
            }
            other @ OutboxError::Source { .. } => {
                panic!("expected a typed Reconcile refusal, got {other:?}")
            }
        }

        let snapshot = lane.snapshot();
        assert_eq!(snapshot.total, 1);
        let detail = snapshot.last_detail.expect("refusal recorded");
        assert!(detail.contains("slice-k has no reconcile consumer"));

        // At-least-once redelivery: a retry counts again and stays typed.
        assert!(sink.reconcile(&item).is_err());
        assert_eq!(lane.snapshot().total, 2);
    }
}
