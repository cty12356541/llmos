//! Durable Channel sequence wake delivery for the Tokio runtime adapter.
//!
//! This module closes the Channel endpoint of the wait-authority loop: a
//! fiber registers "wake me when channel `C` commits at least sequence `T`"
//! through [`TokioRuntimeAdapter::wait_for_channel`], and an at-least-once
//! consumer hands the commit notification's [`WakeReport`] to
//! [`TokioChannelWakeSink::deliver`]. Every in-memory wait mirrors an
//! authority-derived durable wait row, so restarts and at-least-once
//! redelivery resolve through the same handshake as the Operation wake path
//! in [`crate::wake`].
//!
//! The full gate sequence of `wait_for_channel`, in order:
//!
//! 1. runtime shutdown → `Runtime(ShuttingDown)`;
//! 2. stale or unknown fiber handle → `Runtime(InvalidGeneration)`;
//! 3. terminal fiber or already-cancelled scope → ready
//!    [`WaitOutcome::Cancelled`] (no durable side effect);
//! 4. durable registration ([`RegisterDecision::Registered`] or
//!    [`RegisterDecision::Replayed`], then the live row is re-read) —
//!    a `WOKEN` row resolves ready [`WaitOutcome::Woken`] (at-least-once:
//!    the durable wake already happened), a `CANCELLED` row resolves ready
//!    [`WaitOutcome::Cancelled`];
//! 5. a still-`PENDING` row whose channel high-water already covers the
//!    target (via `WaitAuthority::channel_high_water`) is self-flipped
//!    through an explicit `notify_commits` under a domain-reserved
//!    idempotency key and resolves ready [`WaitOutcome::Woken`] — the
//!    explicit-notify model, never a polling loop;
//! 6. otherwise the wait registers an in-memory `Pending` entry, resolved by
//!    [`TokioChannelWakeSink::deliver`].
//!
//! Cancellation split: the runtime side (scope cancel, fiber termination,
//! shutdown) only resolves the in-memory future and NEVER touches the
//! durable wait row — the durable `PENDING -> CANCELLED` flip is exclusively
//! the caller's explicit `WaitAuthority::cancel_wait`. A cancelled runtime
//! wait therefore leaves its durable row `PENDING`, and a later notification
//! (or the restart-replay path) still consumes it.
//!
//! Wait-side rehydration: [`TokioRuntimeAdapter::rearm_channel_waits`]
//! rebuilds exactly this in-memory half after a restart, from the durable
//! rows alone. It mirrors the same gate order, enumerates the durable rows
//! through `WaitAuthority::list_waits` (optionally scoped to one channel),
//! and per row: `CANCELLED` is never re-armed; `WOKEN` resolves immediately
//! (at-least-once) and consumes any early-buffered placeholder wake; a
//! high-water-covered `PENDING` row is self-flipped through the same
//! `self_notify_key` transform; any other `PENDING` row re-registers the
//! same `ChannelWaitKey`-keyed wait with the same supersede semantics.
//! The durable rows are otherwise read-only to rearm, and fiber execution
//! state is not rebuilt — the restarted fiber's own code must call rearm
//! (or re-register) after it is spawned.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_runtime::{FiberHandle, RuntimeError};
use nlos_types::{ChannelId, ExecutionFiberId, Generation, IdempotencyKey};
use nlos_wait::{
    NotifyCommitsRequest, RegisterDecision, RegisterWaitRequest, WaitAuthority, WaitAuthorityError,
    WaitId, WaitRecord, WaitState, WakeReport,
};
use tokio::sync::oneshot;

use crate::replay::ResumeRejection;
use crate::wake::{WaitEntry, WaitOutcome, is_terminal};
use crate::{CancellationScope, FiberRecord, Inner, TokioRuntimeAdapter, lock_unpoisoned};

/// The identity of one logical Channel sequence wait: the authority-derived
/// durable [`WaitId`] plus the waiting fiber generation used for terminal
/// purge alignment.
///
/// The `WaitId` is the logical identity (the durable row the wait mirrors);
/// the fiber fields exist so the fiber lifecycle can purge the registry with
/// the same `for_fiber` scan as the Operation wait registry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ChannelWaitKey {
    wait_id: WaitId,
    fiber_id: ExecutionFiberId,
    fiber_generation: Generation,
}

impl ChannelWaitKey {
    const fn new(wait_id: WaitId, fiber: &FiberHandle) -> Self {
        Self {
            wait_id,
            fiber_id: fiber.fiber_id,
            fiber_generation: fiber.generation,
        }
    }

    /// Whether this wait belongs to the given fiber generation. The runtime
    /// lifecycle uses it to purge a terminated fiber's channel waits.
    pub(crate) fn for_fiber(&self, fiber_id: ExecutionFiberId, generation: Generation) -> bool {
        self.fiber_id == fiber_id && self.fiber_generation == generation
    }

    /// The placeholder key for a wake buffered by [`TokioChannelWakeSink::
    /// deliver`] when no registration exists yet: delivery reports carry no
    /// fiber identity, so the buffer is keyed with the zero fiber id and can
    /// never be purged by a real fiber. Registration consumes buffered
    /// entries by `WaitId` regardless of the fiber fields, which is what
    /// gives early deliveries their consume-on-register semantics.
    fn orphaned(wait_id: WaitId) -> Self {
        Self {
            wait_id,
            fiber_id: ExecutionFiberId::from_bytes([0; 16]),
            fiber_generation: Generation::INITIAL,
        }
    }
}

/// Failure modes of [`TokioRuntimeAdapter::wait_for_channel`],
/// [`TokioRuntimeAdapter::rearm_channel_waits`],
/// [`TokioRuntimeAdapter::resume_binding`] and the B-path snapshot entries.
#[derive(Debug)]
pub enum ChannelWaitError {
    /// The runtime rejected the wait: [`RuntimeError::ShuttingDown`] after
    /// [`TokioRuntimeAdapter::shutdown`], or
    /// [`RuntimeError::InvalidGeneration`] for a stale or unknown fiber
    /// handle.
    Runtime(RuntimeError),
    /// The durable wait authority failed (registration, row readback,
    /// high-water read or the self-flip notification).
    WaitAuthority(WaitAuthorityError),
    /// The durable Channel authority failed (consume registration or its
    /// projection read).
    ChannelAuthority(nlos_channel::ChannelAuthorityError),
    /// The durable Task authority failed (effect registration or its
    /// projection read).
    TaskAuthority(nlos_task::TaskStoreError),
    /// The durable Process authority failed (fiber incarnation registration,
    /// entry snapshot write/restore/GC).
    ProcessAuthority(nlos_process::ProcessAuthorityError),
    /// The B path found no entry snapshot for the binding's current
    /// incarnation: nothing was recorded, or the terminal GC already
    /// consumed it. The caller should use the A path.
    SnapshotUnavailable,
    /// The presented durable fiber incarnation is not the binding's current
    /// one (the ADR-0012 generation gate); fail-closed, zero durable side
    /// effect.
    StaleFiberIncarnation,
    /// The durable row returned by the authority does not match the
    /// registered request (binding, channel or target sequence) — an
    /// authority contract violation, failed closed.
    RecordMismatch,
    /// The new incarnation's re-drive ([`ResumableBinding::resume`]) failed
    /// before the framework armed anything; the durable rows are untouched.
    /// The rejection reason is the incarnation's own, propagated verbatim.
    ResumeRejected(ResumeRejection),
    /// The [`ResumePlan`] referenced a [`WaitId`] that is not a
    /// still-`PENDING` wait event of the projected replay — a plan contract
    /// violation, failed closed before any arming (and therefore before any
    /// durable side effect).
    ResumePlanMismatch,
}

impl fmt::Display for ChannelWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(formatter, "runtime channel wait failure: {error}"),
            Self::WaitAuthority(error) => {
                write!(formatter, "wait authority channel wait failure: {error}")
            }
            Self::ChannelAuthority(error) => {
                write!(formatter, "channel authority failure: {error}")
            }
            Self::TaskAuthority(error) => write!(formatter, "task authority failure: {error}"),
            Self::ProcessAuthority(error) => {
                write!(formatter, "process authority failure: {error}")
            }
            Self::SnapshotUnavailable => formatter
                .write_str("no entry snapshot exists for the binding's current incarnation"),
            Self::StaleFiberIncarnation => formatter
                .write_str("the presented fiber incarnation is not the binding's current one"),
            Self::RecordMismatch => formatter
                .write_str("durable wait row does not match the registered channel wait request"),
            Self::ResumeRejected(rejection) => {
                write!(formatter, "fiber replay re-drive rejected: {rejection}")
            }
            Self::ResumePlanMismatch => formatter.write_str(
                "resume plan references a wait that is not a still-pending event of the replay",
            ),
        }
    }
}

impl std::error::Error for ChannelWaitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::WaitAuthority(error) => Some(error),
            Self::ChannelAuthority(error) => Some(error),
            Self::TaskAuthority(error) => Some(error),
            Self::ProcessAuthority(error) => Some(error),
            Self::RecordMismatch
            | Self::SnapshotUnavailable
            | Self::StaleFiberIncarnation
            | Self::ResumePlanMismatch => None,
            Self::ResumeRejected(rejection) => Some(rejection),
        }
    }
}

impl From<ResumeRejection> for ChannelWaitError {
    fn from(rejection: ResumeRejection) -> Self {
        Self::ResumeRejected(rejection)
    }
}

impl From<RuntimeError> for ChannelWaitError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<WaitAuthorityError> for ChannelWaitError {
    fn from(error: WaitAuthorityError) -> Self {
        Self::WaitAuthority(error)
    }
}

impl From<nlos_channel::ChannelAuthorityError> for ChannelWaitError {
    fn from(error: nlos_channel::ChannelAuthorityError) -> Self {
        Self::ChannelAuthority(error)
    }
}

impl From<nlos_task::TaskStoreError> for ChannelWaitError {
    fn from(error: nlos_task::TaskStoreError) -> Self {
        Self::TaskAuthority(error)
    }
}

impl From<nlos_process::ProcessAuthorityError> for ChannelWaitError {
    fn from(error: nlos_process::ProcessAuthorityError) -> Self {
        Self::ProcessAuthority(error)
    }
}

/// The per-report outcome of [`TokioChannelWakeSink::deliver`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeliveryReport {
    /// Number of `WOKEN` wait records processed. Every processed record is a
    /// successful, idempotent delivery — a repeat delivery still counts here,
    /// exactly like the `WakeOutcome::Delivered` the Operation sink reports
    /// for at-least-once redelivery.
    pub delivered: usize,
    /// How many of those deliveries were buffered instead of handed to a live
    /// receiver: an early delivery (no registration yet), a repeat delivery
    /// hitting an already-buffered key, or a consumed receiver that was
    /// already gone.
    pub buffered: usize,
}

/// Removes a still-pending entry so a later wake observes no wait and
/// buffers instead of firing a dead sender.
fn remove_channel_pending(inner: &Inner, key: &ChannelWaitKey) {
    let mut channel_waits = lock_unpoisoned(&inner.channel_waits);
    if matches!(channel_waits.get(key), Some(WaitEntry::Pending(_))) {
        channel_waits.remove(key);
    }
}

/// Removes any buffered wake for `wait_id` — the placeholder a delivery
/// buffered when no registration existed yet (under the fiber-less
/// placeholder key), or a wake re-buffered after its receiver was dropped —
/// and reports whether one was consumed. Consuming the buffer is what turns
/// an early delivery into an immediate wake for the rehydrating wait.
fn consume_channel_buffer(inner: &Inner, wait_id: WaitId) -> bool {
    let mut channel_waits = lock_unpoisoned(&inner.channel_waits);
    let buffered = channel_waits
        .iter()
        .find(|(key, entry)| key.wait_id == wait_id && matches!(entry, WaitEntry::Buffered))
        .map(|(key, _)| *key);
    buffered.is_some_and(|key| channel_waits.remove(&key).is_some())
}

/// The domain-reserved notify idempotency key for the self-flip of one
/// durable wait: a bijective transform of the authority-derived `WaitId`.
/// It is deterministic, so a repeated self-flip for the same wait replays
/// the original notification instead of erroring, and it is distinct from
/// every other wait's key and from the producer key space as long as
/// producers never present this transform of a `WaitId`.
fn self_notify_key(wait_id: WaitId) -> IdempotencyKey {
    const SELF_NOTIFY_MASK: u8 = 0x5A;
    let mut bytes = *wait_id.as_bytes();
    for byte in &mut bytes {
        *byte ^= SELF_NOTIFY_MASK;
    }
    IdempotencyKey::from_bytes(bytes)
}

/// A non-zero wall-clock millisecond timestamp for self-flip notifications.
/// The authority rejects a zero timestamp (it would collide with the durable
/// "not woken" sentinel), so a pre-epoch or coarse clock still yields `1`.
pub(crate) fn now_millis() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    millis.max(1)
}

/// A future that resolves exactly once for a registered Channel sequence
/// wait.
///
/// The wait resolves to [`WaitOutcome::Woken`] when the matching durable
/// wake is delivered, and to [`WaitOutcome::Cancelled`] when the fiber's
/// cancellation scope is cancelled, the fiber terminates first, or the
/// runtime shuts down. It never pends forever. Runtime-side cancellation
/// never touches the durable wait row: the durable `PENDING -> CANCELLED`
/// flip is exclusively an explicit `WaitAuthority::cancel_wait`.
#[must_use = "a ChannelSequenceWait does nothing unless awaited"]
pub struct ChannelSequenceWait {
    driver: Pin<Box<dyn Future<Output = WaitOutcome> + Send>>,
}

impl ChannelSequenceWait {
    /// A wait that has already resolved before it is handed out.
    fn ready(outcome: WaitOutcome) -> Self {
        Self {
            driver: Box::pin(async move { outcome }),
        }
    }
}

impl Future for ChannelSequenceWait {
    type Output = WaitOutcome;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<WaitOutcome> {
        self.driver.as_mut().poll(context)
    }
}

/// Builds the shared resolution driver of one registered Channel sequence
/// wait: `Woken` when the matching wake is delivered, `Cancelled` when the
/// scope is cancelled, the fiber terminates, the wait is superseded, or the
/// runtime shuts down. Never pends forever. Used verbatim by both
/// `wait_for_channel` and `rearm_channel_waits`, so a rehydrated wait
/// behaves exactly like a freshly registered one.
fn channel_wait_driver(
    inner: Arc<Inner>,
    scope: Arc<CancellationScope>,
    key: ChannelWaitKey,
    receiver: oneshot::Receiver<()>,
) -> ChannelSequenceWait {
    ChannelSequenceWait {
        driver: Box::pin(async move {
            // Create the `Notified` future before checking the flag: a
            // cancellation racing this point is then observed either by
            // the flag or by `notify_waiters`, never lost in between.
            let cancelled = scope.notify.notified();
            tokio::pin!(cancelled);
            if scope.is_cancelled() {
                remove_channel_pending(&inner, &key);
                // Contract: the runtime-side cancellation leaves the
                // durable row `PENDING`; durable cancellation is an
                // explicit `WaitAuthority::cancel_wait`.
                return WaitOutcome::Cancelled;
            }
            tokio::select! {
                biased;
                () = &mut cancelled => {
                    remove_channel_pending(&inner, &key);
                    WaitOutcome::Cancelled
                }
                result = receiver => match result {
                    Ok(()) => WaitOutcome::Woken,
                    // The sender was dropped without a signal: the fiber
                    // terminated (lifecycle purge), the wait was
                    // superseded, or the runtime shut down. The wait must
                    // not pend forever.
                    Err(_) => WaitOutcome::Cancelled,
                },
            }
        }),
    }
}

/// How one durable wait row was armed by [`arm_durable_row`].
pub(crate) enum RowArming {
    /// Nothing was armed and nothing is reported: the row was already
    /// terminal-`CANCELLED`, or the fiber turned terminal / its scope was
    /// cancelled while the row was being armed (the row stays durable
    /// `PENDING` for a later pass).
    NotArmed,
    /// The wake fact is already established; the armed future resolves
    /// immediately with [`WaitOutcome::Woken`].
    Satisfied(RearmedChannelWait),
    /// A live in-memory wait was re-armed, resolved by a later delivery.
    Rearmed(RearmedChannelWait),
}

/// The single-row arming logic shared by
/// [`TokioRuntimeAdapter::rearm_channel_waits`] and
/// [`TokioRuntimeAdapter::resume_binding`], verbatim from the rearm loop:
///
/// - a `CANCELLED` row is never armed (terminal on the durable side);
/// - a `WOKEN` row resolves immediately (at-least-once: the durable wake
///   already happened) and consumes any early-buffered placeholder wake for
///   its [`WaitId`];
/// - a still-`PENDING` row whose channel high-water already covers its
///   target is self-flipped through the domain-reserved `self_notify_key`
///   transform (the one durable write this logic can perform) and resolves
///   immediately;
/// - any other still-`PENDING` row re-registers the in-memory
///   `ChannelWaitKey`-keyed wait with the same supersede semantics as
///   `wait_for_channel`: a second arming for the same wait supersedes the
///   first, whose future resolves [`WaitOutcome::Cancelled`].
///
/// # Errors
///
/// Returns [`ChannelWaitError::WaitAuthority`] for high-water read,
/// self-flip or post-flip readback failures.
pub(crate) fn arm_durable_row(
    inner: &Arc<Inner>,
    handle: &FiberHandle,
    record: &FiberRecord,
    waits: &WaitAuthority,
    durable: WaitRecord,
) -> Result<RowArming, ChannelWaitError> {
    match durable.state {
        // A cancelled wait is terminal on the durable side; it is never
        // resurrected.
        WaitState::Cancelled => Ok(RowArming::NotArmed),
        WaitState::Woken => {
            // At-least-once: the durable wake already happened. A delivery
            // that raced the restart is buffered under this `WaitId`;
            // consuming it here is what keeps the buffered placeholder from
            // outliving its rehydrated wait.
            consume_channel_buffer(inner, durable.wait_id);
            Ok(RowArming::Satisfied(RearmedChannelWait {
                record: durable,
                wait: ChannelSequenceWait::ready(WaitOutcome::Woken),
            }))
        }
        WaitState::Pending => {
            // Explicit-notify self-flip, identical to `wait_for_channel`:
            // the channel's durable queue already covers the target, so
            // notify under the domain-reserved key and resolve immediately.
            if waits.channel_high_water(durable.channel_id)? >= durable.target_sequence {
                waits.notify_commits(NotifyCommitsRequest {
                    channel_id: durable.channel_id,
                    up_to_sequence: durable.target_sequence,
                    notified_at_ms: now_millis(),
                    idempotency_key: self_notify_key(durable.wait_id),
                })?;
                // Re-read so the outcome carries the authoritative post-flip
                // row (a notification that raced this self-flip may have
                // flipped the row first).
                let flipped = waits.inspect_wait(durable.wait_id)?;
                consume_channel_buffer(inner, durable.wait_id);
                return Ok(RowArming::Satisfied(RearmedChannelWait {
                    record: flipped,
                    wait: ChannelSequenceWait::ready(WaitOutcome::Woken),
                }));
            }

            let key = ChannelWaitKey::new(durable.wait_id, handle);
            // Same critical section as `wait_for_channel` (and the fiber
            // lifecycle purge): `channel_waits`, then the record's state, so
            // registration and a concurrent purge are mutually exclusive.
            let mut channel_waits = lock_unpoisoned(&inner.channel_waits);
            if is_terminal(*lock_unpoisoned(&record.state)) || record.scope.is_cancelled() {
                // The fiber turned terminal or its scope was cancelled while
                // this row was being armed: skip without registering and
                // without a durable side effect; the row stays `PENDING` for
                // a later pass.
                return Ok(RowArming::NotArmed);
            }
            // A delivery raced this arming and buffered the wake under this
            // `WaitId`: consume the buffer and resolve immediately (the
            // durable flip already happened on the delivery side).
            let buffered_key = channel_waits
                .iter()
                .find(|(buffered, entry)| {
                    buffered.wait_id == key.wait_id && matches!(entry, WaitEntry::Buffered)
                })
                .map(|(buffered, _)| *buffered);
            if let Some(buffered_key) = buffered_key {
                channel_waits.remove(&buffered_key);
                return Ok(RowArming::Satisfied(RearmedChannelWait {
                    record: durable,
                    wait: ChannelSequenceWait::ready(WaitOutcome::Woken),
                }));
            }
            let (sender, receiver) = oneshot::channel();
            // A second arming for the same wait supersedes the first: the
            // dropped sender resolves the superseded wait as `Cancelled`.
            channel_waits.insert(key, WaitEntry::Pending(sender));
            record.begin_wait();
            let scope = Arc::clone(&record.scope);
            drop(channel_waits);

            Ok(RowArming::Rearmed(RearmedChannelWait {
                record: durable,
                wait: channel_wait_driver(Arc::clone(inner), scope, key, receiver),
            }))
        }
    }
}

/// One re-armed durable Channel wait: the durable row as rearm observed or
/// produced it, plus the awaitable runtime side, which is the exact
/// [`ChannelSequenceWait`] handshake type `wait_for_channel` returns — the
/// rehydrated fiber awaits it like any registered wait.
pub struct RearmedChannelWait {
    /// The durable row as the rearm observed or produced it.
    pub record: WaitRecord,
    /// The runtime-side wait; resolve semantics documented on
    /// [`RearmReport`].
    pub wait: ChannelSequenceWait,
}

impl fmt::Debug for RearmedChannelWait {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RearmedChannelWait")
            .field("record", &self.record)
            .field("wait", &stringify!(ChannelSequenceWait))
            .finish()
    }
}

/// The outcome of one [`TokioRuntimeAdapter::rearm_channel_waits`] call.
///
/// Bucket invariant: every `satisfied` entry's future resolves immediately
/// with [`WaitOutcome::Woken`] — the wake fact is already established (the
/// durable row was `WOKEN`, a consumed early-buffered wake, or the
/// high-water self-flip); every `pending` entry's future awaits a later
/// `TokioChannelWakeSink::deliver`, and resolves `Cancelled` on scope
/// cancel, fiber termination, supersession, or shutdown exactly like any
/// registered wait. An empty report is legal: no durable wait matched the
/// filter, or the fiber was already terminal.
#[derive(Debug, Default)]
pub struct RearmReport {
    /// Waits whose wake is already established; each future resolves
    /// immediately with [`WaitOutcome::Woken`].
    pub satisfied: Vec<RearmedChannelWait>,
    /// Waits re-armed as live in-memory waits, resolved by a later
    /// delivery.
    pub pending: Vec<RearmedChannelWait>,
}

/// The Tokio-backed delivery endpoint for durable Channel sequence wakes.
///
/// Shares the channel wait registry with the [`TokioRuntimeAdapter`] it was
/// created from via [`TokioRuntimeAdapter::channel_wait_sink`]. Delivery
/// mirrors the [`crate::wake::TokioWakeSink`] handshake per woken wait
/// record: consuming a `Pending` entry fires its signal once, and a wake
/// that finds no live receiver is buffered so a later registration for the
/// same `WaitId` resolves immediately.
#[derive(Clone)]
pub struct TokioChannelWakeSink {
    inner: Arc<Inner>,
}

impl TokioChannelWakeSink {
    /// Delivers every `WOKEN` wait record of one commit notification report.
    ///
    /// Contract: a record whose `Pending` receiver was already dropped (the
    /// wait was cancelled, superseded, or dropped) does NOT lose the wake —
    /// it is re-buffered under the same key, and a record with no entry at
    /// all is buffered under the fiber-less placeholder key, so a later
    /// registration for that `WaitId` resolves immediately with
    /// [`WaitOutcome::Woken`], exactly like an early wake. A repeat delivery
    /// is idempotent and still reports success, as at-least-once redelivery
    /// requires. An empty report is a valid, successful no-op.
    ///
    /// Delivery never blocks on fiber execution and never touches the
    /// durable rows; the report is the consumer's at-least-once view of the
    /// authority's `PENDING -> WOKEN` flips.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::ShuttingDown`] after
    /// [`TokioRuntimeAdapter::shutdown`]; individual records never fail the
    /// call.
    pub fn deliver(&self, report: &WakeReport) -> Result<DeliveryReport, RuntimeError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown);
        }
        let mut delivered = 0_usize;
        let mut buffered = 0_usize;
        for record in &report.woken {
            if record.state != WaitState::Woken {
                // Reports only ever carry woken rows; skip anything else
                // defensively instead of waking an unflipped wait.
                continue;
            }
            // Every processed record is a successful delivery, mirroring the
            // `WakeOutcome::Delivered` a repeat wake still reports.
            delivered += 1;
            let resume = {
                let mut channel_waits = lock_unpoisoned(&self.inner.channel_waits);
                let pending_key = channel_waits
                    .iter()
                    .find(|(key, entry)| {
                        key.wait_id == record.wait_id && matches!(entry, WaitEntry::Pending(_))
                    })
                    .map(|(key, _)| *key);
                if let Some(key) = pending_key {
                    // Consuming the entry is what makes a duplicate delivery
                    // a no-op instead of a second logical wake.
                    match channel_waits.remove(&key) {
                        Some(WaitEntry::Pending(sender)) => {
                            if sender.send(()).is_err() {
                                // The receiver is gone (the wait was dropped
                                // or superseded): re-buffer the wake under
                                // the same key so a later registration still
                                // resolves immediately.
                                channel_waits.insert(key, WaitEntry::Buffered);
                                buffered += 1;
                            }
                            Some(key)
                        }
                        Some(WaitEntry::Buffered) | None => None,
                    }
                } else {
                    let already_buffered = channel_waits.iter().any(|(key, entry)| {
                        key.wait_id == record.wait_id && matches!(entry, WaitEntry::Buffered)
                    });
                    if !already_buffered {
                        // Early delivery on an unregistered wait: buffer
                        // under the placeholder key so the eventual
                        // registration consumes it immediately.
                        channel_waits.insert(
                            ChannelWaitKey::orphaned(record.wait_id),
                            WaitEntry::Buffered,
                        );
                    }
                    buffered += 1;
                    None
                }
            };
            if let Some(key) = resume {
                // Mirror `TokioWakeSink::wake`: consuming a pending entry
                // best-effort transitions the fiber back out of `WaitingIo`,
                // never overwriting a lifecycle-set state.
                let fibers = lock_unpoisoned(&self.inner.fibers);
                if let Some(fiber_record) = fibers.get(&key.fiber_id)
                    && fiber_record.generation == key.fiber_generation
                {
                    fiber_record.resume_from_wait();
                }
            }
        }
        Ok(DeliveryReport {
            delivered,
            buffered,
        })
    }
}

impl TokioRuntimeAdapter {
    /// Creates the [`TokioChannelWakeSink`] endpoint sharing this adapter's
    /// channel wait registry.
    #[must_use]
    pub fn channel_wait_sink(&self) -> TokioChannelWakeSink {
        TokioChannelWakeSink {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Registers a wait for the durable Channel sequence commit identified by
    /// `request`, on behalf of `handle`, through the durable `waits`
    /// authority.
    ///
    /// See the [module documentation](self) for the full gate order. The
    /// returned future resolves exactly once and never pends forever.
    ///
    /// Cancellation split (contract): runtime-side cancellation — scope
    /// cancel, fiber termination, superseding registration, shutdown — only
    /// resolves the returned future and NEVER cancels the durable wait row,
    /// which stays `PENDING` until an explicit
    /// `WaitAuthority::cancel_wait`. A durable `WOKEN` row (whether observed
    /// here or after a restart replay) always resolves ready
    /// [`WaitOutcome::Woken`], because at-least-once semantics mean the
    /// durable wake already happened.
    ///
    /// A second registration for the same wait key supersedes the first: the
    /// dropped sender resolves the superseded wait as
    /// [`WaitOutcome::Cancelled`].
    ///
    /// # Errors
    ///
    /// Returns [`ChannelWaitError::Runtime`] for shutdown and stale/unknown
    /// fiber handles, [`ChannelWaitError::WaitAuthority`] for durable
    /// authority failures, and [`ChannelWaitError::RecordMismatch`] when the
    /// durable row does not match the request.
    pub fn wait_for_channel(
        &self,
        handle: FiberHandle,
        waits: &WaitAuthority,
        request: RegisterWaitRequest,
    ) -> Result<ChannelSequenceWait, ChannelWaitError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown.into());
        }
        let record = self.record_for(handle)?;

        // Fail-closed gate before any durable side effect: a terminal or
        // already-cancelled fiber never creates a durable wait row.
        if is_terminal(*lock_unpoisoned(&record.state)) || record.scope.is_cancelled() {
            return Ok(ChannelSequenceWait::ready(WaitOutcome::Cancelled));
        }

        // Durable registration. Both decisions yield the row identity; the
        // live row is then re-read so a notification that raced the
        // registration decision is still observed.
        let durable = match waits.register_wait(request)? {
            RegisterDecision::Registered(durable) | RegisterDecision::Replayed(durable) => durable,
        };
        if durable.binding != request.binding
            || durable.channel_id != request.channel_id
            || durable.target_sequence != request.target_sequence
        {
            return Err(ChannelWaitError::RecordMismatch);
        }
        let current = waits.inspect_wait(durable.wait_id)?;
        match current.state {
            WaitState::Woken => {
                // At-least-once: the durable wake already happened.
                return Ok(ChannelSequenceWait::ready(WaitOutcome::Woken));
            }
            WaitState::Cancelled => {
                return Ok(ChannelSequenceWait::ready(WaitOutcome::Cancelled));
            }
            WaitState::Pending => {}
        }

        // Explicit-notify self-flip: if the channel's durable queue already
        // covers the target, notify under a domain-reserved key and resolve
        // immediately. The flip is durable, idempotent under its key, and
        // never a polling loop.
        if waits.channel_high_water(request.channel_id)? >= current.target_sequence {
            waits.notify_commits(NotifyCommitsRequest {
                channel_id: request.channel_id,
                up_to_sequence: current.target_sequence,
                notified_at_ms: now_millis(),
                idempotency_key: self_notify_key(current.wait_id),
            })?;
            return Ok(ChannelSequenceWait::ready(WaitOutcome::Woken));
        }

        let key = ChannelWaitKey::new(current.wait_id, &handle);
        // Same critical section as the fiber lifecycle purge (`channel_waits`,
        // then the record's state), so registration and the purge are mutually
        // exclusive: either the purge removed this fiber's entries and the
        // state reads terminal here, or the entry is registered and a later
        // purge drops its sender, resolving the wait as `Cancelled`.
        let mut channel_waits = lock_unpoisoned(&self.inner.channel_waits);
        if is_terminal(*lock_unpoisoned(&record.state)) || record.scope.is_cancelled() {
            return Ok(ChannelSequenceWait::ready(WaitOutcome::Cancelled));
        }
        // A delivery raced the durable read (the row flipped `WOKEN` after it
        // read `PENDING`) and buffered under this `WaitId`: consume the
        // buffer and resolve immediately.
        let buffered_key = channel_waits
            .iter()
            .find(|(buffered, entry)| {
                buffered.wait_id == key.wait_id && matches!(entry, WaitEntry::Buffered)
            })
            .map(|(buffered, _)| *buffered);
        if let Some(buffered_key) = buffered_key {
            channel_waits.remove(&buffered_key);
            return Ok(ChannelSequenceWait::ready(WaitOutcome::Woken));
        }
        let (sender, receiver) = oneshot::channel();
        // A second registration for the same key supersedes the first: the
        // dropped sender resolves the superseded wait as `Cancelled`.
        channel_waits.insert(key, WaitEntry::Pending(sender));
        record.begin_wait();
        let inner = Arc::clone(&self.inner);
        let scope = Arc::clone(&record.scope);
        drop(channel_waits);

        Ok(channel_wait_driver(inner, scope, key, receiver))
    }

    /// Rebuilds the runtime side of durable Channel waits after a restart
    /// (wait-side rehydration): enumerates the durable wait rows through
    /// `WaitAuthority::list_waits` (optionally scoped to one channel) and
    /// re-mounts each row's in-memory half on behalf of `handle`, exactly as
    /// `wait_for_channel` would have registered it. Fiber execution state is
    /// NOT rebuilt — the restarted fiber's own code must spawn, then call
    /// this (or re-register) to obtain its wait futures.
    ///
    /// Gate order mirrors `wait_for_channel`:
    ///
    /// 1. runtime shutdown → [`RuntimeError::ShuttingDown`];
    /// 2. stale or unknown fiber handle → [`RuntimeError::InvalidGeneration`];
    /// 3. terminal fiber or already-cancelled scope → an empty
    ///    [`RearmReport`] (the ready-`Cancelled` analog: nothing is armed,
    ///    zero durable side effects), not an error.
    ///
    /// Per durable row, mirroring the `wait_for_channel` gates:
    ///
    /// - `CANCELLED` rows are never re-armed and never reported;
    /// - `WOKEN` rows resolve immediately (at-least-once: the durable wake
    ///   already happened) and consume any early-buffered placeholder wake
    ///   for their [`WaitId`];
    /// - a still-`PENDING` row whose channel high-water already covers its
    ///   target is self-flipped through the same domain-reserved
    ///   `self_notify_key` transform `wait_for_channel` uses — the one
    ///   durable write rearm can perform — and resolves immediately;
    /// - any other still-`PENDING` row re-registers the in-memory
    ///   `ChannelWaitKey`-keyed wait under the same critical section,
    ///   re-checks and supersede semantics as `wait_for_channel`: a second
    ///   rearm (or a racing registration) for the same wait supersedes the
    ///   first, whose future resolves [`WaitOutcome::Cancelled`]. If the
    ///   fiber turns terminal or its scope is cancelled while the loop runs,
    ///   the remaining rows are skipped unregistered — their durable rows
    ///   stay `PENDING` for a later rearm.
    ///
    /// The durable rows are otherwise read-only to rearm: it never
    /// re-registers durable rows, never touches `CANCELLED` or `WOKEN`
    /// rows, and never cancels a durable row. The report is the supervisor's
    /// view of what was armed; an empty match is a successful empty report.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelWaitError::Runtime`] for shutdown and stale/unknown
    /// fiber handles, and [`ChannelWaitError::WaitAuthority`] for durable
    /// authority failures (enumeration, high-water reads, the self-flip).
    pub fn rearm_channel_waits(
        &self,
        handle: FiberHandle,
        waits: &WaitAuthority,
        filter: Option<ChannelId>,
    ) -> Result<RearmReport, ChannelWaitError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown.into());
        }
        let record = self.record_for(handle)?;

        // Same fail-closed gate as `wait_for_channel`: a terminal or
        // already-cancelled fiber has nothing to re-arm and never performs
        // a durable side effect.
        if is_terminal(*lock_unpoisoned(&record.state)) || record.scope.is_cancelled() {
            return Ok(RearmReport::default());
        }

        let mut report = RearmReport::default();
        for durable in waits.list_waits(filter)? {
            // The per-row logic is the single-row arming shared with
            // `resume_binding`; the rearm report buckets map 1:1 onto its
            // outcomes (a `CANCELLED` row and a mid-loop terminal skip both
            // arm nothing and are never reported).
            match arm_durable_row(&self.inner, &handle, &record, waits, durable)? {
                RowArming::NotArmed => {}
                RowArming::Satisfied(armed) => report.satisfied.push(armed),
                RowArming::Rearmed(armed) => report.pending.push(armed),
            }
        }
        Ok(report)
    }
}
