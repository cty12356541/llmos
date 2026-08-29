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

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_runtime::{FiberHandle, RuntimeError};
use nlos_types::{ExecutionFiberId, Generation, IdempotencyKey};
use nlos_wait::{
    NotifyCommitsRequest, RegisterDecision, RegisterWaitRequest, WaitAuthority, WaitAuthorityError,
    WaitId, WaitState, WakeReport,
};
use tokio::sync::oneshot;

use crate::wake::{WaitEntry, WaitOutcome, is_terminal};
use crate::{Inner, TokioRuntimeAdapter, lock_unpoisoned};

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

/// Failure modes of [`TokioRuntimeAdapter::wait_for_channel`].
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
    /// The durable row returned by the authority does not match the
    /// registered request (binding, channel or target sequence) — an
    /// authority contract violation, failed closed.
    RecordMismatch,
}

impl fmt::Display for ChannelWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(formatter, "runtime channel wait failure: {error}"),
            Self::WaitAuthority(error) => {
                write!(formatter, "wait authority channel wait failure: {error}")
            }
            Self::RecordMismatch => formatter
                .write_str("durable wait row does not match the registered channel wait request"),
        }
    }
}

impl std::error::Error for ChannelWaitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::WaitAuthority(error) => Some(error),
            Self::RecordMismatch => None,
        }
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
fn now_millis() -> u64 {
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

        Ok(ChannelSequenceWait {
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
        })
    }
}
