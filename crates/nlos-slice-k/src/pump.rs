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
//!
//! Poison-head policy (W57-B): a refused reconcile entry is retried
//! forever by the landed pump, which blocks every later entry behind it.
//! The sink counts consecutive apply failures per durable sequence and,
//! at [`DEFAULT_PARK_THRESHOLD`], parks the entry through the store's
//! one-way schema-v5 dead-letter surface — the queue head unlocks, the
//! parking (count and reason) becomes visible on
//! [`SliceKRuntime::reconcile_lane`], and the entry stays durable and
//! unacknowledged for manual adjudication. Parking is explicit
//! operational debt: there is deliberately no automatic un-park anywhere
//! in this lane — recovery is a human decision taken through
//! [`SliceKRuntime::unpark_outbox_entry`] (W58-1), whose store-side
//! reverse returns the entry to the pending lane in durable sequence
//! order; this lane then counts the adjudication (`unparked`) and the
//! redelivery that finally applies (`recovered`) on the same health
//! surface.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nlos_clock::{AuthorityClock, NowRequest};
use nlos_operation::OperationHandle;
use nlos_outbox::{OutboxError, OutboxItem, ReconcileSink};
use nlos_store::{OutboxParkDecision, SqliteOperationStore};
use nlos_task::{LateOperationOutcomeDecision, LateOperationOutcomeRequest, SqliteTaskAuthority};
use nlos_types::IdempotencyKey;
use sha2::{Digest, Sha256};

/// Consecutive apply failures of the same durable outbox sequence after
/// which the sink parks the entry (W57-B poison-head policy).
///
/// 64 consecutive failures of the same head entry is far outside any
/// healthy retry transient (the landed pump backs off exponentially, so
/// reaching this threshold takes minutes of durable refusal), yet far
/// inside "forever", which is how long a genuinely unappliable entry
/// would otherwise block the queue head. The value is a lane default,
/// not a physical constant: tests drive the same code with small
/// thresholds through [`PumpLane::with_park_threshold`].
pub const DEFAULT_PARK_THRESHOLD: u32 = 64;

/// Bound of the durable park reason, mirroring the store-side column
/// `CHECK` (schema v5 keeps park reasons within 1024 bytes; the store's
/// constant is private, so the lane states the same bound it must obey).
const MAX_PARK_REASON_BYTES: usize = 1024;

/// Snapshot of the reconcile lane: how many late outcomes routed into the
/// task authority, how many refusals happened and of which kind, the most
/// recent refusal reason, and the W57-B dead-letter parking state.
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
    /// Park decisions this lane observed (W57-B): entries it parked after
    /// [`DEFAULT_PARK_THRESHOLD`] (or the lane's configured threshold)
    /// consecutive same-sequence apply failures. Idempotent park replays
    /// count too — the counter reports park *events observed*, mirroring
    /// how the refusal counters count at-least-once retries. Parked
    /// entries are explicit operational debt: they stay durable and
    /// unacknowledged (visible through
    /// [`SqliteOperationStore::inspect_parked_outbox`]) until a human
    /// adjudicates them — nothing in this lane ever un-parks
    /// automatically.
    pub parked: u64,
    /// The durable park reason of the most recent park decision; `None`
    /// when this lane parked nothing yet.
    pub last_park_reason: Option<String>,
    /// Manual unpark adjudications this lane observed (W58-1): unpark
    /// decisions taken through [`SliceKRuntime::unpark_outbox_entry`].
    /// Idempotent unpark replays count too — the counter reports unpark
    /// *events observed*, mirroring how `parked` counts park events.
    pub unparked: u64,
    /// The durable reason of the most recent unpark adjudication; `None`
    /// when no unpark was observed yet.
    pub last_unpark_reason: Option<String>,
    /// Post-unpark recovered deliveries (W58-1): reconcile entries that
    /// were formerly parked, got recovered by a manual unpark, and then
    /// applied through this lane (the application the consumer
    /// acknowledges — the visible proof that the adjudication restored
    /// delivery). Redeliveries that still refuse count only on the
    /// refusal counters.
    pub recovered: u64,
}

impl ReconcileLaneSnapshot {
    /// Total refusals of both fail-closed lanes.
    #[must_use]
    pub const fn refused_total(&self) -> u64 {
        self.no_route + self.failed
    }
}

/// The poison-head tracker of one lane: which durable sequence is
/// currently stuck at the head of the consumer's retry loop, how many
/// times it failed consecutively, and the `Display` text of its most
/// recent failure. At most one entry can be stuck (the consumer stops the
/// batch at the first apply failure and re-offers the same head), so this
/// is O(1) state that mirrors the pump's `stuck_sequence` health model.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ParkTracker {
    stuck_sequence: Option<u64>,
    consecutive_failures: u32,
    last_error: String,
}

/// Interior-shared lane statistics between the runtime and the sink.
#[derive(Default)]
struct ReconcileLaneStats {
    routed: AtomicU64,
    no_route: AtomicU64,
    failed: AtomicU64,
    parked: AtomicU64,
    unparked: AtomicU64,
    recovered: AtomicU64,
    last_detail: Mutex<Option<String>>,
    last_park_reason: Mutex<Option<String>>,
    last_unpark_reason: Mutex<Option<String>>,
    park_tracker: Mutex<ParkTracker>,
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

    /// Records one apply success of `sequence` and, when it was the stuck
    /// head, clears the tracker: the entry will be acknowledged and never
    /// offered again, so its failure history is obsolete.
    fn record_apply_progress(&self, sequence: u64) {
        let mut tracker = self.lock_tracker();
        if tracker.stuck_sequence == Some(sequence) {
            *tracker = ParkTracker::default();
        }
    }

    /// Records one apply failure of `sequence` and returns the new count
    /// of consecutive failures at that same sequence. A failure at a
    /// different sequence means the previously stuck head finally
    /// progressed, so the count restarts at one for the new head — exactly
    /// the pump health model of `stuck_sequence`.
    fn record_apply_failure(&self, sequence: u64, detail: &str) -> u32 {
        let mut tracker = self.lock_tracker();
        if tracker.stuck_sequence == Some(sequence) {
            tracker.consecutive_failures = tracker.consecutive_failures.saturating_add(1);
        } else {
            tracker.stuck_sequence = Some(sequence);
            tracker.consecutive_failures = 1;
        }
        detail.clone_into(&mut tracker.last_error);
        tracker.consecutive_failures
    }

    /// Records one observed park decision (`Parked` and idempotent
    /// `Replayed` alike) and clears the tracker: a parked entry never
    /// returns from `pending_outbox`, so it cannot fail again.
    fn record_park(&self, reason: String) {
        self.parked.fetch_add(1, Ordering::AcqRel);
        *self.lock_park_reason() = Some(reason);
        *self.lock_tracker() = ParkTracker::default();
    }

    /// Records one observed manual unpark adjudication (W58-1) — a fresh
    /// `Unparked` and an idempotent `Replayed` alike — keeping the durable
    /// reason of the most recent one visible.
    fn record_unpark(&self, reason: String) {
        self.unparked.fetch_add(1, Ordering::AcqRel);
        *self.lock_unpark_reason() = Some(reason);
    }

    /// Records one post-unpark recovered delivery (W58-1): a formerly
    /// parked entry whose redelivery applied through this lane.
    fn record_recovered(&self) {
        self.recovered.fetch_add(1, Ordering::AcqRel);
    }

    fn snapshot(&self) -> ReconcileLaneSnapshot {
        ReconcileLaneSnapshot {
            routed: self.routed.load(Ordering::Acquire),
            no_route: self.no_route.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
            last_detail: self.lock().clone(),
            parked: self.parked.load(Ordering::Acquire),
            last_park_reason: self.lock_park_reason().clone(),
            unparked: self.unparked.load(Ordering::Acquire),
            last_unpark_reason: self.lock_unpark_reason().clone(),
            recovered: self.recovered.load(Ordering::Acquire),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<String>> {
        self.last_detail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_park_reason(&self) -> MutexGuard<'_, Option<String>> {
        self.last_park_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_unpark_reason(&self) -> MutexGuard<'_, Option<String>> {
        self.last_unpark_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_tracker(&self) -> MutexGuard<'_, ParkTracker> {
        self.park_tracker
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

/// Deterministic clock idempotency key of one entry's *parking* timestamp:
/// a distinct clock domain from [`entry_clock_key`], so taking the park
/// reading never disturbs the routing reading, and a retried park (a
/// racing pump generation, a crash between the park commit and the health
/// record) replays the same timestamp instead of a fresh one.
fn park_clock_key(item: &OutboxItem) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/slice-k-outbox-park-clock/v1");
    hasher.update(item.operation_id.as_bytes());
    hasher.update(item.operation_generation.get().to_be_bytes());
    hasher.update(item.sequence.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    IdempotencyKey::from_bytes(digest[..16].try_into().expect("16-byte prefix"))
}

/// Deterministic clock idempotency key of one entry's *unparking* timestamp
/// (W58-1): a third clock domain, so an adjudication reading never disturbs
/// the routing or parking readings, and a retried unpark (a replaying
/// adjudication, a crash between the unpark commit and the health record)
/// replays the same timestamp instead of a fresh one. The durable outbox
/// sequence alone identifies the row.
pub(crate) fn unpark_clock_key(sequence: i64) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/slice-k-outbox-unpark-clock/v1");
    hasher.update(sequence.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    IdempotencyKey::from_bytes(digest[..16].try_into().expect("16-byte prefix"))
}

/// The bounded park reason handed to
/// [`SqliteOperationStore::park_outbox_entry`]: a fixed summary prefix
/// (which entry, how many consecutive failures) plus the last failure's
/// `Display` text, truncated on a UTF-8 character boundary to the
/// store-side 1024-byte column bound. Never empty.
fn bounded_park_reason(sequence: i64, failures: u32, last_error: &str) -> String {
    let mut reason = format!(
        "slice-k outbox lane parked sequence {sequence} after {failures} consecutive \
         apply failures; last failure: {last_error}"
    );
    if reason.len() > MAX_PARK_REASON_BYTES {
        let mut cut = MAX_PARK_REASON_BYTES;
        while !reason.is_char_boundary(cut) {
            cut -= 1;
        }
        reason.truncate(cut);
    }
    reason
}

/// Task-routing [`ReconcileSink`] for the Slice K lane (W48-2), with the
/// W57-B poison-head parking policy.
///
/// Routing: the sink holds the runtime's own task authority, clock, and
/// operation store, and every `ReconcileEffect` entry is offered to
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
///
/// Poison-head parking (W57-B): each refusal also feeds the lane's
/// consecutive same-sequence failure tracker. When one entry fails
/// [`DEFAULT_PARK_THRESHOLD`] (or the configured threshold) times in a
/// row, the sink parks it through
/// [`SqliteOperationStore::park_outbox_entry`] with a bounded
/// last-failure summary as the durable reason. Parking is explicit
/// operational debt and strictly one-way: the entry leaves the pending
/// lane (the queue head unlocks and later entries flow), stays durable
/// and unacknowledged for manual adjudication through
/// [`SqliteOperationStore::inspect_parked_outbox`], and nothing in this
/// lane ever un-parks it. The park itself still fails closed — this
/// apply attempt returns the refusal error, because parking is never an
/// acknowledgement.
pub struct LateOutcomeReconcileSink {
    tasks: Arc<SqliteTaskAuthority>,
    clock: Arc<AuthorityClock>,
    store: Arc<SqliteOperationStore>,
    park_threshold: NonZeroU32,
    stats: Arc<ReconcileLaneStats>,
}

/// Which fail-closed lane a refused routing belongs to: the typed no-route
/// decision (no effect slot binds the handle) or a routing failure
/// (non-consumable state, guard, clock, or storage).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefusalKind {
    NoRoute,
    Failed,
}

impl LateOutcomeReconcileSink {
    /// Builds the sink over the runtime's task authority, clock, and
    /// operation store, sharing `stats` with the runtime that will expose
    /// it. The stats handle is created once per runtime and survives pump
    /// restarts. `park_threshold` is the consecutive same-sequence apply
    /// failure count at which the sink parks the entry.
    fn new(
        tasks: Arc<SqliteTaskAuthority>,
        clock: Arc<AuthorityClock>,
        store: Arc<SqliteOperationStore>,
        park_threshold: NonZeroU32,
        stats: Arc<ReconcileLaneStats>,
    ) -> Self {
        Self {
            tasks,
            clock,
            store,
            park_threshold,
            stats,
        }
    }

    /// Parks one poison entry after it reached the lane's failure
    /// threshold. Every failure mode stays observable and fail-closed: a
    /// failed park records a `failed` refusal detail and leaves the entry
    /// durable for redelivery (the threshold tracker is not reset, so the
    /// next failure attempts the park again); a successful park — fresh or
    /// an idempotent replay of the same reason — is counted on the lane's
    /// health surface. The entry is never acknowledged here.
    fn park_poison_entry(&self, item: &OutboxItem, failures: u32, last_error: &str) {
        let Ok(sequence) = i64::try_from(item.sequence) else {
            self.stats.record_failure(format!(
                "parking outbox sequence {} failed: the sequence exceeds the durable \
                 i64 domain; the entry stays durable for redelivery instead of being \
                 acknowledged away",
                item.sequence,
            ));
            return;
        };
        let decision = match self.clock.wall_now(NowRequest {
            idempotency_key: park_clock_key(item),
        }) {
            Ok(decision) => decision,
            Err(error) => {
                self.stats.record_failure(format!(
                    "parking outbox sequence {sequence} could not take a durable clock \
                     reading: {error}; the entry stays durable for redelivery instead of \
                     being acknowledged away",
                ));
                return;
            }
        };
        let Ok(now_ms) = i64::try_from(decision.reading().as_u64()) else {
            self.stats.record_failure(format!(
                "parking outbox sequence {sequence} failed: the clock reading \
                 exceeds the park timestamp domain; the entry stays durable for \
                 redelivery instead of being acknowledged away",
            ));
            return;
        };
        let reason = bounded_park_reason(sequence, failures, last_error);
        match self.store.park_outbox_entry(sequence, &reason, now_ms) {
            // A fresh park and an idempotent replay of the same decision
            // are the same observed outcome for the health surface; the
            // durable row keeps the original timestamp either way.
            Ok(OutboxParkDecision::Parked { .. } | OutboxParkDecision::Replayed { .. }) => {
                self.stats.record_park(reason);
            }
            Err(error) => {
                // A typed park conflict (already parked under a different
                // reason — a racing manual adjudication or another lane
                // generation) leaves the entry parked and the pending lane
                // unlocked, so this is still a refusal, not a lost entry.
                self.stats.record_failure(format!(
                    "parking outbox sequence {sequence} failed: {error}; the entry stays \
                     durable for redelivery instead of being acknowledged away",
                ));
            }
        }
    }

    /// Observes whether one successfully applied entry is a post-unpark
    /// recovery (W58-1): a formerly parked entry that a manual adjudication
    /// unparked and whose redelivery just applied here. This is advisory
    /// visibility only — the apply already succeeded and will be
    /// acknowledged — so a read failure is skipped silently instead of
    /// turning a healthy delivery into a reported failure.
    fn observe_recovery(&self, sequence: u64) {
        let Some(sequence) = i64::try_from(sequence).ok() else {
            return;
        };
        if let Ok(Some(history)) = self.store.inspect_outbox_park_history_entry(sequence)
            && !history.parked
            && history.unpark_count > 0
        {
            self.stats.record_recovered();
        }
    }

    /// The routing attempt without the lane bookkeeping: exactly the
    /// fail-closed semantics documented on [`Self`], with each refusal
    /// tagged by its fail-closed lane so [`ReconcileSink::reconcile`] can
    /// count it and feed the poison-head tracker.
    fn route(&self, item: &OutboxItem) -> Result<(), (OutboxError, RefusalKind)> {
        let handle = format!(
            "operation {} generation {}",
            crate::short_hex(item.operation_id.as_bytes()),
            item.operation_generation.get(),
        );
        let reconciled_at_ms = match self.clock.wall_now(NowRequest {
            idempotency_key: entry_clock_key(item),
        }) {
            Ok(decision) => i64::try_from(decision.reading().as_u64()).map_err(|_| {
                (
                    OutboxError::Reconcile {
                        detail: format!(
                            "late-outcome routing for {handle} failed: the clock reading \
                             exceeds the task authority's signed timestamp domain"
                        ),
                    },
                    RefusalKind::Failed,
                )
            })?,
            Err(error) => {
                return Err((
                    OutboxError::Reconcile {
                        detail: format!(
                            "late-outcome routing for {handle} could not take a durable clock \
                             reading: {error}; the entry stays durable for redelivery instead \
                             of being acknowledged away",
                        ),
                    },
                    RefusalKind::Failed,
                ));
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
            ) => Ok(()),
            Ok(LateOperationOutcomeDecision::NoBoundSlot {
                operation_id,
                generation,
            }) => Err((
                OutboxError::Reconcile {
                    detail: format!(
                        "no effect slot binds {handle}: the task authority answers the typed \
                         no-route decision for operation {} generation {}, so the outbox \
                         fact has no task-plane consumer; the entry stays durable for \
                         redelivery instead of being acknowledged away",
                        crate::short_hex(operation_id.as_bytes()),
                        generation.get(),
                    ),
                },
                RefusalKind::NoRoute,
            )),
            Err(error) => Err((
                OutboxError::Reconcile {
                    detail: format!(
                        "late-outcome routing for {handle} failed: {error}; the entry \
                         stays durable for redelivery instead of being acknowledged away",
                    ),
                },
                RefusalKind::Failed,
            )),
        }
    }
}

impl ReconcileSink for LateOutcomeReconcileSink {
    fn reconcile(&self, item: &OutboxItem) -> Result<(), OutboxError> {
        match self.route(item) {
            Ok(()) => {
                self.stats.record_route();
                self.stats.record_apply_progress(item.sequence);
                self.observe_recovery(item.sequence);
                Ok(())
            }
            Err((error, kind)) => {
                let detail = match &error {
                    OutboxError::Reconcile { detail } | OutboxError::Source { detail } => {
                        detail.as_str()
                    }
                };
                match kind {
                    RefusalKind::NoRoute => self.stats.record_no_route(detail.to_owned()),
                    RefusalKind::Failed => self.stats.record_failure(detail.to_owned()),
                }
                let failures = self.stats.record_apply_failure(item.sequence, detail);
                if failures >= self.park_threshold.get() {
                    self.park_poison_entry(item, failures, detail);
                }
                Err(error)
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
    park_threshold: NonZeroU32,
    stats: Arc<ReconcileLaneStats>,
}

impl PumpLane {
    /// The lane with the landed default poison-head tuning
    /// ([`DEFAULT_PARK_THRESHOLD`] consecutive same-sequence apply
    /// failures before parking).
    pub(crate) fn new() -> Self {
        Self {
            park_threshold: NonZeroU32::new(DEFAULT_PARK_THRESHOLD)
                .expect("DEFAULT_PARK_THRESHOLD is positive"),
            stats: Arc::new(ReconcileLaneStats::default()),
        }
    }

    /// The lane with an explicit poison-head threshold (tests drive the
    /// identical park code with small thresholds; production lanes use
    /// [`PumpLane::new`]).
    #[cfg(test)]
    pub(crate) fn with_park_threshold(park_threshold: NonZeroU32) -> Self {
        Self {
            park_threshold,
            stats: Arc::new(ReconcileLaneStats::default()),
        }
    }

    pub(crate) fn sink(
        &self,
        store: Arc<SqliteOperationStore>,
        tasks: Arc<SqliteTaskAuthority>,
        clock: Arc<AuthorityClock>,
    ) -> LateOutcomeReconcileSink {
        LateOutcomeReconcileSink::new(
            tasks,
            clock,
            store,
            self.park_threshold,
            Arc::clone(&self.stats),
        )
    }

    pub(crate) fn snapshot(&self) -> ReconcileLaneSnapshot {
        self.stats.snapshot()
    }

    /// Records one manual unpark adjudication observed through
    /// [`SliceKRuntime::unpark_outbox_entry`] (W58-1) so the lane's health
    /// surface carries the adjudication alongside the parks it answers.
    pub(crate) fn record_unpark(&self, reason: String) {
        self.stats.record_unpark(reason);
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use nlos_operation::{CompletionDecision, CompletionOutcome, OperationSpec, OperationState};
    use nlos_outbox::{ConsumerConfig, OutboxConsumer, OutboxError, OutboxKind, ReconcileSink};
    use nlos_runtime::{FiberHandle, RuntimeError, WakeOutcome, WakeSink};
    use nlos_runtime_tokio::StoreOutboxSource;
    use nlos_store::{OutboxKind as StoreOutboxKind, RegistrationDecision, SqliteOperationStore};
    use nlos_types::{
        CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation, OperationId,
        ReceiptId,
    };

    use super::{DEFAULT_PARK_THRESHOLD, PumpLane};

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

    /// Wake sink double: every wake delivers, so healthy `WakeFiber`
    /// entries always apply and acknowledge.
    struct DeliveringWakeSink;

    impl WakeSink for DeliveringWakeSink {
        fn wake(
            &self,
            _fiber: &FiberHandle,
            _operation_id: OperationId,
            _operation_generation: Generation,
        ) -> Result<WakeOutcome, RuntimeError> {
            Ok(WakeOutcome::Delivered)
        }
    }

    fn seeded_spec(seed: u8) -> OperationSpec {
        OperationSpec {
            operation_id: OperationId::from_bytes([seed; 16]),
            generation: Generation::INITIAL,
            owner_fiber: FiberHandle {
                fiber_id: ExecutionFiberId::from_bytes([seed.wrapping_add(1); 16]),
                generation: Generation::INITIAL,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(2); 16]),
            cancellation_generation: Generation::INITIAL,
        }
    }

    /// Commits exactly one `ReconcileEffect` outbox row for `seed`: a
    /// dispatched Operation whose cancel won the CAS before completion, so
    /// the late terminal callback canonicalizes for reconciliation (the
    /// durable poison shape when no effect slot binds it).
    fn commit_poison_reconcile_entry(store: &SqliteOperationStore, seed: u8) {
        let handle = match store.register(seeded_spec(seed)).expect("register") {
            RegistrationDecision::Created(handle) => handle,
            RegistrationDecision::Existing(_) => panic!("fresh register cannot exist"),
        };
        let ticket = store
            .dispatch(handle, CallbackId::from_bytes([seed; 16]))
            .expect("dispatch");
        store
            .request_cancel_idempotent(
                handle,
                CancelEpoch::INITIAL,
                ReceiptId::from_bytes([seed; 16]),
            )
            .expect("request cancel");
        match store
            .complete(
                ticket,
                CompletionOutcome::Completed {
                    receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x40); 16]),
                },
            )
            .expect("late completion")
        {
            CompletionDecision::CanonicalizedForReconciliation { .. } => {}
            other => panic!("expected a reconciliation canonicalization, got {other:?}"),
        }
    }

    /// Commits exactly one healthy `WakeFiber` outbox row for `seed`: a
    /// dispatched Operation completing with an unfenced callback.
    fn commit_healthy_wake_entry(store: &SqliteOperationStore, seed: u8) {
        let handle = match store.register(seeded_spec(seed)).expect("register") {
            RegistrationDecision::Created(handle) => handle,
            RegistrationDecision::Existing(_) => panic!("fresh register cannot exist"),
        };
        let ticket = store
            .dispatch(handle, CallbackId::from_bytes([seed; 16]))
            .expect("dispatch");
        match store
            .complete(
                ticket,
                CompletionOutcome::Completed {
                    receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x40); 16]),
                },
            )
            .expect("completion")
        {
            CompletionDecision::CanonicalizedAndWake { .. } => {}
            other => panic!("expected a wake canonicalization, got {other:?}"),
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
        let store =
            Arc::new(SqliteOperationStore::open(root.root().join("operations.sqlite3")).unwrap());
        let tasks = Arc::new(
            nlos_task::SqliteTaskAuthority::open(root.root().join("tasks.sqlite3")).unwrap(),
        );
        let clock = Arc::new(nlos_clock::AuthorityClock::open(root.root().join("clock")).unwrap());
        let lane = PumpLane::new();
        let sink = lane.sink(store, tasks, clock);
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
        assert_eq!(snapshot.parked, 0, "two refusals never park");
        assert_eq!(snapshot.last_park_reason, None);
        let detail = snapshot.last_detail.expect("refusal recorded");
        assert!(detail.contains("no effect slot binds"));

        // At-least-once redelivery: a retry counts again and stays typed.
        assert!(sink.reconcile(&item).is_err());
        let retried = lane.snapshot();
        assert_eq!(retried.no_route, 2);
        assert_eq!(retried.routed, 0);
    }

    /// Given/When/Then: given a durable outbox whose head is a poison
    /// `ReconcileEffect` entry (no effect slot ever binds it) followed by a
    /// healthy `WakeFiber` entry, a lane with park threshold 3, and the
    /// real consumer over the real store; when the poison head is drained
    /// threshold-minus-one times; then nothing is parked — the head stays
    /// pending and the healthy entry stays queued behind it.
    #[test]
    fn below_threshold_the_head_stays_pending_and_nothing_parks() {
        let root = TempDir::new("below-threshold");
        std::fs::create_dir_all(root.root()).expect("create temp root");
        let store =
            Arc::new(SqliteOperationStore::open(root.root().join("operations.sqlite3")).unwrap());
        commit_poison_reconcile_entry(&store, 0xa1);
        commit_healthy_wake_entry(&store, 0xb1);
        let tasks = Arc::new(
            nlos_task::SqliteTaskAuthority::open(root.root().join("tasks.sqlite3")).unwrap(),
        );
        let clock = Arc::new(nlos_clock::AuthorityClock::open(root.root().join("clock")).unwrap());
        let lane = PumpLane::with_park_threshold(NonZeroU32::new(3).expect("positive"));
        let consumer = OutboxConsumer {
            source: StoreOutboxSource::new(Arc::clone(&store)),
            wake_sink: DeliveringWakeSink,
            reconcile_sink: lane.sink(Arc::clone(&store), tasks, clock),
            config: ConsumerConfig { batch_limit: 8 },
        };

        for _ in 0..2 {
            let report = consumer.drain_once().expect("drain stays observable");
            assert_eq!(
                report.stopped_at,
                Some(1),
                "the poison head stops the batch"
            );
            assert_eq!(report.acked, 0, "nothing behind the head is touched");
        }

        let pending = store.pending_outbox(10).expect("pending");
        assert_eq!(
            pending
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2],
            "below the threshold the head stays pending and blocks the queue"
        );
        assert!(
            store.inspect_parked_outbox(10).expect("parked").is_empty(),
            "below the threshold nothing is parked"
        );
        let snapshot = lane.snapshot();
        assert_eq!(snapshot.no_route, 2);
        assert_eq!(snapshot.parked, 0);
        assert_eq!(snapshot.last_park_reason, None);
    }

    /// Given/When/Then: given the same poison-head outbox; when the poison
    /// head is drained once more (reaching the lane's threshold of 3) and
    /// the next drain runs; then the poison entry is parked one-way — it
    /// leaves the pending lane so the healthy entry behind it flows through
    /// and acknowledges, the parked row stays durable and unacknowledged
    /// with a bounded reason carrying the last failure, and the lane
    /// health shows the park (count and reason).
    #[test]
    fn poison_head_parks_at_threshold_unlocks_queue_and_surfaces_health() {
        let root = TempDir::new("park-at-threshold");
        std::fs::create_dir_all(root.root()).expect("create temp root");
        let store =
            Arc::new(SqliteOperationStore::open(root.root().join("operations.sqlite3")).unwrap());
        commit_poison_reconcile_entry(&store, 0xa2);
        commit_healthy_wake_entry(&store, 0xb2);
        let tasks = Arc::new(
            nlos_task::SqliteTaskAuthority::open(root.root().join("tasks.sqlite3")).unwrap(),
        );
        let clock = Arc::new(nlos_clock::AuthorityClock::open(root.root().join("clock")).unwrap());
        let lane = PumpLane::with_park_threshold(NonZeroU32::new(3).expect("positive"));
        let consumer = OutboxConsumer {
            source: StoreOutboxSource::new(Arc::clone(&store)),
            wake_sink: DeliveringWakeSink,
            reconcile_sink: lane.sink(Arc::clone(&store), tasks, clock),
            config: ConsumerConfig { batch_limit: 8 },
        };

        // Two failures: below the threshold, the head is merely refused.
        for _ in 0..2 {
            let report = consumer.drain_once().expect("drain");
            assert_eq!(report.stopped_at, Some(1));
        }
        // The third consecutive failure of the same sequence parks it. The
        // parking drain itself still stops at the entry (parking is never
        // an acknowledgement), so the park and the stop coexist.
        let report = consumer.drain_once().expect("parking drain");
        assert_eq!(report.stopped_at, Some(1), "parking never acknowledges");
        assert_eq!(report.acked, 0);

        let parked = store.inspect_parked_outbox(10).expect("parked listing");
        assert_eq!(parked.len(), 1, "exactly the poison head is parked");
        assert_eq!(parked[0].sequence, 1);
        assert_eq!(parked[0].kind, StoreOutboxKind::ReconcileEffect);
        assert!(!parked[0].acknowledged, "parking is not an acknowledgement");
        assert!(parked[0].parked_at_ms >= 0);
        assert!(
            parked[0]
                .park_reason
                .contains("parked sequence 1 after 3 consecutive apply failures"),
            "the reason carries the entry, the failure count, and the cause: {}",
            parked[0].park_reason
        );
        assert!(
            parked[0].park_reason.contains("no effect slot binds"),
            "the reason carries the last failure's summary: {}",
            parked[0].park_reason
        );

        // The queue head is unlocked: the next drain serves the healthy
        // wake entry behind the parked row and acknowledges it.
        let report = consumer.drain_once().expect("unblocked drain");
        assert_eq!(report.polled, 1, "the parked head no longer blocks");
        assert_eq!(report.applied, 1);
        assert_eq!(report.acked, 1);
        assert_eq!(report.stopped_at, None);
        assert!(
            store.pending_outbox(10).expect("pending").is_empty(),
            "the outbox is fully drained apart from the parked row"
        );

        let snapshot = lane.snapshot();
        assert_eq!(snapshot.parked, 1, "the park is visible on lane health");
        let reason = snapshot.last_park_reason.expect("park reason recorded");
        assert!(reason.contains("parked sequence 1 after 3 consecutive apply failures"));
    }

    /// Given/When/Then: given an entry a first lane generation already
    /// parked; when a second lane generation (fresh tracker, same store,
    /// same deterministic reason) drives the identical poison entry to its
    /// own threshold and re-offers it; then the store replays the original
    /// park decision instead of conflicting — the durable row keeps its
    /// original timestamp and reason, exactly one parked row exists, and
    /// the second lane observes the park on its own health surface. There
    /// is no un-park: the entry stays parked, durable, unacknowledged.
    #[test]
    fn park_is_idempotent_across_lane_generations() {
        let root = TempDir::new("park-replay");
        std::fs::create_dir_all(root.root()).expect("create temp root");
        let store =
            Arc::new(SqliteOperationStore::open(root.root().join("operations.sqlite3")).unwrap());
        let tasks = Arc::new(
            nlos_task::SqliteTaskAuthority::open(root.root().join("tasks.sqlite3")).unwrap(),
        );
        let clock = Arc::new(nlos_clock::AuthorityClock::open(root.root().join("clock")).unwrap());
        let item = unbound_item();

        // First generation parks a manually committed outbox row. The row
        // is the store's first outbox entry (sequence 1); the item carries
        // that sequence plus an operation no effect slot binds, so every
        // routing attempt refuses with the same deterministic no-route
        // detail.
        commit_poison_reconcile_entry(&store, 0xa3);
        let delivered = nlos_outbox::OutboxItem {
            sequence: 1,
            ..item
        };
        let first = PumpLane::with_park_threshold(NonZeroU32::new(2).expect("positive"));
        let first_sink = first.sink(Arc::clone(&store), Arc::clone(&tasks), Arc::clone(&clock));
        assert!(first_sink.reconcile(&delivered).is_err());
        assert!(first_sink.reconcile(&delivered).is_err());
        assert_eq!(first.snapshot().parked, 1);
        let parked = store.inspect_parked_outbox(10).expect("parked listing");
        assert_eq!(parked.len(), 1);
        let original = &parked[0];

        // Second generation: a fresh tracker redelivers the same entry (an
        // apply already in flight when the park committed) and reaches its
        // own threshold. The reason is deterministic (same sequence, same
        // failure count, same last failure), so the store replays.
        let second = PumpLane::with_park_threshold(NonZeroU32::new(2).expect("positive"));
        let second_sink = second.sink(Arc::clone(&store), Arc::clone(&tasks), Arc::clone(&clock));
        assert!(second_sink.reconcile(&delivered).is_err());
        assert!(second_sink.reconcile(&delivered).is_err());
        assert_eq!(
            second.snapshot().parked,
            1,
            "the replayed park decision is observed, not a conflict"
        );

        let after = store.inspect_parked_outbox(10).expect("parked listing");
        assert_eq!(after.len(), 1, "still exactly one parked row");
        assert_eq!(
            after[0].parked_at_ms, original.parked_at_ms,
            "the replay keeps the original parking timestamp"
        );
        assert_eq!(after[0].park_reason, original.park_reason);
        assert!(!after[0].acknowledged);
    }

    /// Given/When/Then: given the landed default threshold; when its value
    /// is inspected; then it is 64 — the documented W57-B poison-head
    /// tuning (this pins the default so an accidental change must pass
    /// through here).
    #[test]
    fn default_park_threshold_is_the_documented_default() {
        assert_eq!(DEFAULT_PARK_THRESHOLD, 64);
    }

    /// Given/When/Then: given a poison head parked at the lane's threshold
    /// (the healthy entry behind it already flowed and acknowledged); when
    /// a manual adjudication unparks the parked entry through the store's
    /// W58-1 controlled reverse — the exact store call
    /// [`SliceKRuntime::unpark_outbox_entry`] drives; then the entry
    /// rejoins the pending lane at its durable sequence position, the next
    /// drain re-offers it (recovery is redelivery, not forgiveness: the
    /// refusal lanes count again and the drain stops at the re-offered
    /// head), the parked listing empties, and the park history reads the
    /// entry as recovered with exactly one unpark.
    #[test]
    fn unparked_head_rejoins_the_lane_and_is_reoffered() {
        let root = TempDir::new("unpark-reoffer");
        std::fs::create_dir_all(root.root()).expect("create temp root");
        let store =
            Arc::new(SqliteOperationStore::open(root.root().join("operations.sqlite3")).unwrap());
        commit_poison_reconcile_entry(&store, 0xa4);
        commit_healthy_wake_entry(&store, 0xb4);
        let tasks = Arc::new(
            nlos_task::SqliteTaskAuthority::open(root.root().join("tasks.sqlite3")).unwrap(),
        );
        let clock = Arc::new(nlos_clock::AuthorityClock::open(root.root().join("clock")).unwrap());
        let lane = PumpLane::with_park_threshold(NonZeroU32::new(3).expect("positive"));
        let consumer = OutboxConsumer {
            source: StoreOutboxSource::new(Arc::clone(&store)),
            wake_sink: DeliveringWakeSink,
            reconcile_sink: lane.sink(Arc::clone(&store), tasks, clock),
            config: ConsumerConfig { batch_limit: 8 },
        };

        // Park the poison head at the threshold, then let the healthy
        // entry behind it flow through (the W57-B unlock).
        for _ in 0..3 {
            let report = consumer.drain_once().expect("parking drains");
            assert_eq!(report.stopped_at, Some(1));
        }
        consumer.drain_once().expect("unblocked drain");
        assert!(
            store.pending_outbox(10).expect("pending").is_empty(),
            "only the parked row remains"
        );
        let parked = store.inspect_parked_outbox(10).expect("parked listing");
        assert_eq!(parked.len(), 1);
        let sequence = parked[0].sequence;

        // The adjudication recovers the entry; its lane-level bookkeeping
        // mirrors what SliceKRuntime::unpark_outbox_entry records.
        let decision = store
            .unpark_outbox_entry(sequence, "adjudicated: route restored", 8_000)
            .expect("unpark");
        assert_eq!(
            decision,
            nlos_store::OutboxUnparkDecision::Unparked {
                sequence,
                unparked_at_ms: 8_000,
            }
        );
        lane.record_unpark("adjudicated: route restored".to_owned());

        // The recovered head is pending again at its durable position.
        assert_eq!(
            store
                .pending_outbox(10)
                .expect("pending")
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![sequence],
            "the recovered head rejoins the pending lane"
        );
        assert!(
            store.inspect_parked_outbox(10).expect("parked").is_empty(),
            "no entry is parked anymore"
        );

        // The next drain re-offers it; still unbindable, the refusal
        // counts again and the batch stops at the head (it would re-park
        // at the threshold — exactly the pre-unpark behavior).
        let before = lane.snapshot().no_route;
        let report = consumer.drain_once().expect("re-offer drain");
        assert_eq!(
            report.stopped_at,
            Some(sequence.try_into().expect("positive sequence")),
            "the head is re-offered"
        );
        assert_eq!(report.acked, 0);
        assert_eq!(lane.snapshot().no_route, before + 1);

        let unparked = lane.snapshot();
        assert_eq!(unparked.unparked, 1, "the adjudication is visible");
        assert_eq!(
            unparked.last_unpark_reason.as_deref(),
            Some("adjudicated: route restored")
        );
        assert_eq!(
            unparked.recovered, 0,
            "a re-offered entry that still refuses is no recovered delivery"
        );

        let history = store
            .inspect_outbox_park_history_entry(sequence)
            .expect("history")
            .expect("the recovered row keeps its history");
        assert!(!history.parked);
        assert_eq!(history.park_count, 1);
        assert_eq!(history.unpark_count, 1);
        assert_eq!(history.unparked_at_ms, Some(8_000));
        assert_eq!(
            history.unpark_reason.as_deref(),
            Some("adjudicated: route restored")
        );
    }
}
