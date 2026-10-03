//! Production Outbox wiring for the Slice K runtime: the task-routing
//! reconcile sink, its inspectable routed/refusal surface, and the
//! bounded-stop helper used by [`SliceKRuntime`]'s pump lifecycle.
//!
//! Position in the closed loop: terminal Operation commits write
//! `WakeFiber`/`ReconcileEffect` rows into `operation_outbox` in the same
//! transaction. [`SliceKRuntime::start_pump`] drives the landed
//! [`OutboxPump`](nlos_runtime_tokio::OutboxPump) over the shared
//! `SqliteOperationStore`: wake entries route to the
//! [`TokioWakeSink`](nlos_runtime_tokio::TokioWakeSink) of the caller's
//! runtime adapter, and reconcile entries route to
//! [`LateOutcomeReconcileSink`] below — real routing into the task
//! authority's `reconcile_late_operation_outcome` lane when the outbox
//! facts are constructible, and an explicit, observable fail-closed
//! refusal when they are not.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nlos_clock::{AuthorityClock, NowRequest};
use nlos_operation::OperationHandle;
use nlos_outbox::{OutboxError, OutboxItem, ReconcileSink};
use nlos_task::{LateOperationOutcomeDecision, LateOperationOutcomeRequest, SqliteTaskAuthority};
use nlos_types::IdempotencyKey;
use sha2::{Digest, Sha256};

/// Snapshot of the reconcile lane: how many late outcomes routed into the
/// task authority, how many refusals happened and of which kind, and the
/// most recent refusal reason.
///
/// This is the health surface that makes every reconcile entry's fate
/// observable: routed counts entries whose bound effect slot converged (or
/// replayed) and were acknowledged; the two refusal counters keep the
/// fail-closed lanes visible — the durable outbox holds those entries (they
/// are never acknowledged away), and the counters prove the refusals are
/// routing decisions, not lost wake-ups.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileLaneSnapshot {
    /// `ReconcileEffect` applications the sink routed into
    /// [`SqliteTaskAuthority::reconcile_late_operation_outcome`] (fresh
    /// closures and idempotent replays alike; both are apply-successes the
    /// consumer may acknowledge).
    pub routed: u64,
    /// Refusals because no effect slot binds the operation handle: the
    /// outbox fact is durable truth, but the task plane holds no route for
    /// it. Monotonic; at-least-once redelivery deliberately counts each
    /// retry.
    pub no_route: u64,
    /// Refusals because routing failed: a non-consumable terminal state, a
    /// slot/lease/adoption guard, or a storage failure. Monotonic;
    /// at-least-once redelivery deliberately counts each retry.
    pub failed: u64,
    /// `Display` text of the most recent refusal reason (no-route or
    /// failure); `None` when nothing was refused yet.
    pub last_detail: Option<String>,
}

impl ReconcileLaneSnapshot {
    /// Total refusals of both fail-closed lanes.
    #[must_use]
    pub const fn refused_total(&self) -> u64 {
        self.no_route + self.failed
    }
}

/// Interior-shared lane statistics between the runtime and the sink.
#[derive(Default)]
struct ReconcileLaneStats {
    routed: AtomicU64,
    no_route: AtomicU64,
    failed: AtomicU64,
    last_detail: Mutex<Option<String>>,
}

impl ReconcileLaneStats {
    fn record_route(&self) {
        self.routed.fetch_add(1, Ordering::AcqRel);
    }

    fn record_no_route(&self, detail: String) {
        self.no_route.fetch_add(1, Ordering::AcqRel);
        *self.lock() = Some(detail);
    }

    fn record_failure(&self, detail: String) {
        self.failed.fetch_add(1, Ordering::AcqRel);
        *self.lock() = Some(detail);
    }

    fn snapshot(&self) -> ReconcileLaneSnapshot {
        ReconcileLaneSnapshot {
            routed: self.routed.load(Ordering::Acquire),
            no_route: self.no_route.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
            last_detail: self.lock().clone(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<String>> {
        self.last_detail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Deterministic clock idempotency key of one outbox entry: the same
/// redelivered entry takes the same durable wall reading (the clock's
/// replay branch), so a crash-before-ack re-route writes the same
/// `reconciled_at_ms` the original routing would have.
fn entry_clock_key(item: &OutboxItem) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/slice-k-late-outcome-clock/v1");
    hasher.update(item.operation_id.as_bytes());
    hasher.update(item.operation_generation.get().to_be_bytes());
    hasher.update(item.sequence.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    IdempotencyKey::from_bytes(digest[..16].try_into().expect("16-byte prefix"))
}

/// Task-routing [`ReconcileSink`] for the Slice K lane (W48-2).
///
/// Routing: the sink holds the runtime's own task authority and clock, and
/// every `ReconcileEffect` entry is offered to
/// [`SqliteTaskAuthority::reconcile_late_operation_outcome`] with the
/// entry's facts carried verbatim — the fenced
/// `(operation_id, generation)` handle, the canonical terminal state (its
/// receipt id is the authoritative closure evidence), and a durable clock
/// reading taken under a per-entry idempotency key. A `Closed` or
/// `Replayed` decision is a successful apply the consumer acknowledges.
///
/// Fail-closed lanes (never silently acknowledged):
/// - the task authority answers the typed no-route decision (no effect slot
///   binds the handle) — the entry stays durable for redelivery and the
///   refusal is counted as no-route;
/// - routing fails (non-consumable terminal state, slot/lease/adoption
///   guard, clock or storage failure) — the entry stays durable for
///   redelivery and the refusal is counted as failed.
///
/// Both refusal lanes return the typed [`OutboxError::Reconcile`]: the
/// consumer stops the batch at that entry, the pump backs off and retries,
/// and a drain stopped by this sink is treated by the landed pump as
/// backpressure (not a drain failure), so the pump stays `Running` and
/// re-offers the entry every poll interval. Visibility lives in
/// [`SliceKRuntime::reconcile_lane`] plus the never-shrinking
/// `pending_outbox` prefix.
pub struct LateOutcomeReconcileSink {
    tasks: Arc<SqliteTaskAuthority>,
    clock: Arc<AuthorityClock>,
    stats: Arc<ReconcileLaneStats>,
}

impl LateOutcomeReconcileSink {
    /// Builds the sink over the runtime's task authority and clock, sharing
    /// `stats` with the runtime that will expose it. The stats handle is
    /// created once per runtime and survives pump restarts.
    fn new(
        tasks: Arc<SqliteTaskAuthority>,
        clock: Arc<AuthorityClock>,
        stats: Arc<ReconcileLaneStats>,
    ) -> Self {
        Self {
            tasks,
            clock,
            stats,
        }
    }
}

impl ReconcileSink for LateOutcomeReconcileSink {
    fn reconcile(&self, item: &OutboxItem) -> Result<(), OutboxError> {
        let handle = format!(
            "operation {} generation {}",
            crate::short_hex(item.operation_id.as_bytes()),
            item.operation_generation.get(),
        );
        let reconciled_at_ms = match self.clock.wall_now(NowRequest {
            idempotency_key: entry_clock_key(item),
        }) {
            Ok(decision) => i64::try_from(decision.reading().as_u64()).map_err(|_| {
                self.stats.record_failure(format!(
                    "clock reading for {handle} exceeds the task timestamp domain"
                ));
                OutboxError::Reconcile {
                    detail: format!(
                        "late-outcome routing for {handle} failed: the clock reading \
                         exceeds the task authority's signed timestamp domain"
                    ),
                }
            })?,
            Err(error) => {
                let detail = format!(
                    "late-outcome routing for {handle} could not take a durable clock \
                     reading: {error}; the entry stays durable for redelivery instead \
                     of being acknowledged away",
                );
                self.stats.record_failure(detail.clone());
                return Err(OutboxError::Reconcile { detail });
            }
        };
        match self
            .tasks
            .reconcile_late_operation_outcome(LateOperationOutcomeRequest {
                operation: OperationHandle {
                    operation_id: item.operation_id,
                    generation: item.operation_generation,
                },
                terminal_state: item.state,
                reconciled_at_ms,
            }) {
            Ok(
                LateOperationOutcomeDecision::Closed(_) | LateOperationOutcomeDecision::Replayed(_),
            ) => {
                self.stats.record_route();
                Ok(())
            }
            Ok(LateOperationOutcomeDecision::NoBoundSlot {
                operation_id,
                generation,
            }) => {
                let detail = format!(
                    "no effect slot binds {handle}: the task authority answers the typed \
                     no-route decision for operation {} generation {}, so the outbox \
                     fact has no task-plane consumer; the entry stays durable for \
                     redelivery instead of being acknowledged away",
                    crate::short_hex(operation_id.as_bytes()),
                    generation.get(),
                );
                self.stats.record_no_route(detail.clone());
                Err(OutboxError::Reconcile { detail })
            }
            Err(error) => {
                let detail = format!(
                    "late-outcome routing for {handle} failed: {error}; the entry \
                     stays durable for redelivery instead of being acknowledged away",
                );
                self.stats.record_failure(detail.clone());
                Err(OutboxError::Reconcile { detail })
            }
        }
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

/// The pump lane one runtime owns: the shared lane stats the sink reports
/// into, plus the construction point for the sink itself.
pub(crate) struct PumpLane {
    stats: Arc<ReconcileLaneStats>,
}

impl PumpLane {
    pub(crate) fn new() -> Self {
        Self {
            stats: Arc::new(ReconcileLaneStats::default()),
        }
    }

    pub(crate) fn sink(
        &self,
        tasks: Arc<SqliteTaskAuthority>,
        clock: Arc<AuthorityClock>,
    ) -> LateOutcomeReconcileSink {
        LateOutcomeReconcileSink::new(tasks, clock, Arc::clone(&self.stats))
    }

    pub(crate) fn snapshot(&self) -> ReconcileLaneSnapshot {
        self.stats.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use nlos_operation::OperationState;
    use nlos_outbox::{OutboxError, OutboxKind, ReconcileSink};
    use nlos_runtime::FiberHandle;
    use nlos_types::{ExecutionFiberId, Generation, OperationId, ReceiptId};

    use super::PumpLane;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "nlos-slice-k-pump-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }

        fn root(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn unbound_item() -> nlos_outbox::OutboxItem {
        nlos_outbox::OutboxItem {
            sequence: 7,
            kind: OutboxKind::ReconcileEffect,
            operation_id: OperationId::from_bytes([1_u8; 16]),
            operation_generation: Generation::INITIAL,
            owner_fiber: FiberHandle {
                fiber_id: ExecutionFiberId::from_bytes([2_u8; 16]),
                generation: Generation::INITIAL,
            },
            callback_id: None,
            state: OperationState::Completed {
                receipt_id: ReceiptId::from_bytes([3_u8; 16]),
            },
        }
    }

    /// Given/When/Then: given a sink over a fresh (empty) task authority
    /// and clock; when a reconcile entry for an operation no write set ever
    /// bound is offered; then the sink keeps the fail-closed refusal
    /// semantics — the typed `Reconcile` error (never `Ok`), the no-route
    /// counter advances (routing and failure lanes stay zero), and the
    /// recorded detail names the typed no-route decision so the health
    /// surface can point at the durable outbox row.
    #[test]
    fn unbound_entry_is_refused_visibly_as_no_route() {
        let root = TempDir::new("no-route");
        std::fs::create_dir_all(root.root()).expect("create temp root");
        let tasks = std::sync::Arc::new(
            nlos_task::SqliteTaskAuthority::open(root.root().join("tasks.sqlite3")).unwrap(),
        );
        let clock = std::sync::Arc::new(
            nlos_clock::AuthorityClock::open(root.root().join("clock")).unwrap(),
        );
        let lane = PumpLane::new();
        let sink = lane.sink(tasks, clock);
        let item = unbound_item();

        let error = sink.reconcile(&item).expect_err("sink must fail closed");
        match error {
            OutboxError::Reconcile { detail } => {
                assert!(detail.contains("no effect slot binds"));
                assert!(detail.contains("stays durable"));
            }
            other @ OutboxError::Source { .. } => {
                panic!("expected a typed Reconcile refusal, got {other:?}")
            }
        }

        let snapshot = lane.snapshot();
        assert_eq!(snapshot.routed, 0);
        assert_eq!(snapshot.no_route, 1);
        assert_eq!(snapshot.failed, 0);
        assert_eq!(snapshot.refused_total(), 1);
        let detail = snapshot.last_detail.expect("refusal recorded");
        assert!(detail.contains("no effect slot binds"));

        // At-least-once redelivery: a retry counts again and stays typed.
        assert!(sink.reconcile(&item).is_err());
        let retried = lane.snapshot();
        assert_eq!(retried.no_route, 2);
        assert_eq!(retried.routed, 0);
    }
}
