//! Fiber replay facilities of ADR-0009 (event-sourced resume), minimal
//! prefix: the binding → durable event stream projection, the re-drive
//! contract for a new fiber incarnation, and the framework entry that
//! re-arms the replayed waits through the same single-row logic as
//! [`crate::TokioRuntimeAdapter::rearm_channel_waits`].
//!
//! Decision 1 of ADR-0009: a fiber's external interactions are already
//! durably fact-recorded (effect permits, channel queue entries, wait rows,
//! the attribution ledger); rebuilding a fiber means projecting those facts
//! as one binding's event stream, re-driving the code to its wait point in
//! a **new incarnation**, and re-mounting the waits through wait-side
//! rehydration. This prefix implements exactly that skeleton:
//!
//! - [`BindingEventProjection`] projects one binding's durable wait rows
//!   (the only authority already carrying a binding column) into a typed,
//!   registration-ordered [`BindingReplay`]. The **effect and queue event
//!   projections are later slices**: those authorities do not yet have a
//!   binding-associated column, and none is fabricated here.
//! - [`ResumableBinding`] is the re-drive contract: a new incarnation
//!   declares "I have re-driven this binding's replay to its wait point"
//!   and hands back a [`ResumePlan`]. [`TokioRuntimeAdapter::resume_binding`]
//!   validates the plan fail-closed and re-arms every planned still-`PENDING`
//!   wait through `arm_durable_row`: a high-water-covered row self-flips
//!   satisfied, any other live row re-mounts the in-memory wait with the
//!   same supersede semantics as `rearm_channel_waits` — so a second resume
//!   of the same binding is an idempotent replay that supersedes the
//!   previously armed waits (their futures resolve
//!   [`crate::WaitOutcome::Cancelled`]) and leaves the durable rows raw.
//! - [`SnapshotResumable`] is the ADR-0009 decision-2 placeholder (the
//!   controlled-snapshot fallback path); it is intentionally unwired in
//!   this prefix.
//!
//! Exactly-once boundary (ADR-0009 decision 3): the replay covers only
//! durable interaction boundaries. Pure internal computation between two
//! durable interactions never enters the event stream — a resumed
//! incarnation continues from the last durable boundary, which is the
//! explicitly accepted semantic loss of this ADR. Idempotent consumption of
//! the replayed facts is carried by the existing durable dedup (wait row
//! uniqueness, notify idempotency keys), not by this module.

use std::fmt;
use std::sync::atomic::Ordering;

use nlos_runtime::{FiberHandle, RuntimeError};
use nlos_wait::{BindingId, WaitAuthority, WaitId, WaitRecord, WaitState};

use crate::channel_wait::{ChannelWaitError, RearmedChannelWait, RowArming, arm_durable_row};
use crate::wake::is_terminal;
use crate::{TokioRuntimeAdapter, lock_unpoisoned};

/// One wait event of a binding's replayed event stream: the durable wait
/// row exactly as the authority holds it, read-only to the replay.
///
/// In this prefix the wait registry is the sole projected authority, so a
/// [`BindingReplay`] is a stream of these events; the effect/queue event
/// kinds of ADR-0009 join later slices once their authorities gain a
/// binding association (registered as a `B-PROCESS-002` follow-up, not
/// fabricated here).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayedWaitEvent {
    /// The durable wait row backing this event: channel, target sequence,
    /// state machine (`PENDING`/`WOKEN`/`CANCELLED`) and the registration,
    /// wake and cancellation timestamps.
    pub record: WaitRecord,
}

/// The event-stream snapshot of one binding, projected from the durable
/// face (ADR-0009 decision 1). Events are ordered by registration time (the
/// authority's `(registered_at_ms, wait_id)` order), so the stream reads as
/// the wait history the previous incarnations of this binding produced.
///
/// The projection is a pure read: it never writes the durable face, and
/// terminal (`WOKEN`/`CANCELLED`) events are projected as the facts they
/// are, never re-driven.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingReplay {
    /// The binding this stream belongs to.
    pub binding: BindingId,
    /// The wait events in registration-time order.
    pub events: Vec<ReplayedWaitEvent>,
}

/// The projector of one binding's durable event stream (ADR-0009 decision
/// 1, minimal prefix).
#[derive(Clone, Copy, Debug)]
pub struct BindingEventProjection;

impl BindingEventProjection {
    /// Projects `binding`'s wait events from the durable wait authority.
    ///
    /// The fact source of this prefix is the wait registry: every durable
    /// wait row of the binding is one event of the stream. A binding with
    /// no rows projects the legal empty stream.
    ///
    /// # Errors
    ///
    /// Fails closed for the all-zero binding, any referenced channel that
    /// no longer exists, any tampered wait row, or an authority read
    /// failure.
    pub fn project(
        waits: &WaitAuthority,
        binding: BindingId,
    ) -> Result<BindingReplay, ChannelWaitError> {
        let events = waits
            .list_waits_for_binding(binding)?
            .into_iter()
            .map(|record| ReplayedWaitEvent { record })
            .collect();
        Ok(BindingReplay { binding, events })
    }
}

/// The typed failure of the incarnation-side re-drive
/// ([`ResumableBinding::resume`]): the new incarnation refused or failed
/// its replay-to-wait-point. The framework propagates it verbatim as
/// [`ChannelWaitError::ResumeRejected`] and performs zero durable side
/// effect — nothing was armed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeRejection {
    /// The incarnation's own failure reason, propagated verbatim.
    pub reason: String,
}

impl fmt::Display for ResumeRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "re-drive rejected by the new incarnation: {}",
            self.reason
        )
    }
}

impl std::error::Error for ResumeRejection {}

/// The new incarnation's declaration of what the framework must re-mount
/// after it has re-driven the [`BindingReplay`] to its wait point.
///
/// Only still-`PENDING` wait events are armable; every `rearm_wait_ids`
/// entry must reference one, and [`TokioRuntimeAdapter::resume_binding`]
/// validates this fail-closed before arming anything. `WOKEN` and
/// `CANCELLED` events are reported as facts regardless of the plan — the
/// plan gates arming, not reporting.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResumePlan {
    /// The durable `WaitId`s of the still-`PENDING` replay events the
    /// framework should re-mount as live waits for this incarnation.
    pub rearm_wait_ids: Vec<WaitId>,
}

impl ResumePlan {
    /// The canonical plan: re-arm every still-`PENDING` wait event of the
    /// replay — the framework-side mirror of "re-driven to the wait point".
    #[must_use]
    pub fn all_pending(replay: &BindingReplay) -> Self {
        Self {
            rearm_wait_ids: replay
                .events
                .iter()
                .filter(|event| event.record.state == WaitState::Pending)
                .map(|event| event.record.wait_id)
                .collect(),
        }
    }

    /// A plan that re-arms nothing: the incarnation consumed the replay but
    /// asks for no wait to be re-mounted (every still-`PENDING` event is
    /// still reported as a fact; a later resume can arm it).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

/// The re-drive contract of a new fiber incarnation (ADR-0009 decision 1):
/// the implementation declares that, given the binding's replayed event
/// stream, it can re-drive its code from the last durable boundary up to
/// the wait point.
///
/// Implementations re-execute only up to the nearest durable interaction
/// boundary the stream ends at (the exactly-once boundary of ADR-0009
/// decision 3); internal computation between boundaries is the
/// incarnation's own re-entrant code, never replayed state.
pub trait ResumableBinding {
    /// The binding this incarnation re-drives.
    #[must_use]
    fn binding(&self) -> BindingId;

    /// Consumes the projected replay and declares the re-mount plan.
    ///
    /// Called by [`TokioRuntimeAdapter::resume_binding`] after the
    /// projection and before anything is armed; a returned error aborts the
    /// resume with zero durable side effect.
    ///
    /// # Errors
    ///
    /// The incarnation's own re-drive failure, propagated verbatim as
    /// [`ChannelWaitError::ResumeRejected`].
    fn resume(&self, replay: &BindingReplay) -> Result<ResumePlan, ResumeRejection>;
}

/// The ADR-0009 decision-2 placeholder: the controlled-snapshot fallback
/// path for fibers not yet reshaped into the [`ResumableBinding`]
/// re-entrant form — handler-level "input snapshot + idempotent
/// re-execution" recovery to the handler entry, semantics mirroring
/// B-TASK-006O, with intermediate progress honestly declared lost.
///
/// Intentionally a bare marker in this minimal prefix: no runtime wiring,
/// no snapshot retention policy. Implementing it is a later `B-PROCESS-002`
/// slice.
pub trait SnapshotResumable {}

/// The outcome of one [`TokioRuntimeAdapter::resume_binding`] call.
///
/// Bucket invariant mirrors [`crate::RearmReport`]: every `rearmed_satisfied`
/// entry's future resolves immediately with [`crate::WaitOutcome::Woken`] (the
/// wake fact was already established at arming time), every
/// `rearmed_pending` entry's future awaits a later
/// [`crate::TokioChannelWakeSink::deliver`] and resolves `Cancelled` on
/// scope cancel, fiber termination, supersession or shutdown exactly like
/// any registered wait. `already_woken` and `cancelled` are report-only
/// fact buckets: the framework performed zero action for them.
#[derive(Debug)]
pub struct ResumeReport {
    /// The replay this resume consumed (registration-ordered wait events of
    /// the binding).
    pub replay: BindingReplay,
    /// Planned `PENDING` events whose wake was already established at
    /// arming time (high-water self-flip or a raced delivery buffer); each
    /// future resolves immediately with [`crate::WaitOutcome::Woken`].
    pub rearmed_satisfied: Vec<RearmedChannelWait>,
    /// Planned `PENDING` events re-mounted as live in-memory waits,
    /// resolved by a later delivery.
    pub rearmed_pending: Vec<RearmedChannelWait>,
    /// `WOKEN` events: the durable wake already happened, so the framework
    /// does not re-mount them — the new incarnation decides on its own
    /// whether to skip or re-execute past them. Report-only, and no buffered
    /// placeholder wake is consumed either: if the incarnation later
    /// registers (or rearms) the same wait, the at-least-once contract
    /// still resolves it immediately.
    pub already_woken: Vec<WaitRecord>,
    /// `CANCELLED` events: terminal on the durable side, never resurrected.
    /// Report-only, zero action.
    pub cancelled: Vec<WaitRecord>,
}

impl ResumeReport {
    /// The empty report for `binding`: the terminal-fiber analog of rearm's
    /// empty report — nothing was projected and nothing was armed, without
    /// being an error. Distinguishable from a projected-empty binding only
    /// by the fiber's own state.
    fn empty_for(binding: BindingId) -> Self {
        Self {
            replay: BindingReplay {
                binding,
                events: Vec::new(),
            },
            rearmed_satisfied: Vec::new(),
            rearmed_pending: Vec::new(),
            already_woken: Vec::new(),
            cancelled: Vec::new(),
        }
    }
}

impl TokioRuntimeAdapter {
    /// Re-drives `resumable`'s binding in the new fiber incarnation
    /// `handle` (ADR-0009 decision 1, minimal prefix): projects the
    /// binding's durable event stream, asks the incarnation for its
    /// [`ResumePlan`], then re-arms every planned still-`PENDING` wait
    /// through the exact single-row logic of
    /// [`crate::TokioRuntimeAdapter::rearm_channel_waits`].
    ///
    /// Gate order mirrors the rearm gates, fail-closed:
    ///
    /// 1. runtime shutdown → [`RuntimeError::ShuttingDown`];
    /// 2. stale or unknown fiber handle → [`RuntimeError::InvalidGeneration`]
    ///    — only a live, current-generation incarnation may resume;
    /// 3. terminal fiber or already-cancelled scope → an empty
    ///    [`ResumeReport`] without projecting (nothing is re-driven, zero
    ///    durable side effects), not an error;
    /// 4. projection (`BindingEventProjection::project`);
    /// 5. the incarnation's `resume` — a rejection aborts with
    ///    [`ChannelWaitError::ResumeRejected`] before anything is armed;
    /// 6. plan validation — every planned id must reference a still-`PENDING`
    ///    event of the replay, else [`ChannelWaitError::ResumePlanMismatch`]
    ///    before anything is armed.
    ///
    /// Per replay event:
    ///
    /// - `WOKEN` → `already_woken` bucket, zero action (no re-mount, no
    ///   buffer consumption): the incarnation decides to skip or re-execute;
    ///   a later registration/rearm of the same wait still consumes any
    ///   buffered wake (at-least-once preserved);
    /// - `CANCELLED` → `cancelled` bucket, zero action;
    /// - planned `PENDING` → `rearmed_satisfied` (wake already established,
    ///   including the one durable write this path can perform, the
    ///   high-water self-flip) or `rearmed_pending` (live in-memory wait).
    ///
    /// Idempotent replay contract: resuming the same binding again with the
    /// same (or another) live incarnation re-projects and re-arms the same
    /// still-`PENDING` rows under the same wait keys — the previously armed
    /// waits are superseded and resolve [`crate::WaitOutcome::Cancelled`], exactly
    /// like a second rearm. The durable rows stay read-only to resume
    /// except that self-flip; a raw `WaitRecord` never changes across
    /// resumes.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelWaitError::Runtime`] for shutdown and stale/unknown
    /// fiber handles, [`ChannelWaitError::WaitAuthority`] for durable
    /// authority failures (projection, high-water reads, the self-flip),
    /// [`ChannelWaitError::ResumeRejected`] when the incarnation rejects
    /// the re-drive, and [`ChannelWaitError::ResumePlanMismatch`] for a plan
    /// referencing a wait that is not a still-`PENDING` event of the replay.
    pub fn resume_binding(
        &self,
        handle: FiberHandle,
        waits: &WaitAuthority,
        resumable: &impl ResumableBinding,
    ) -> Result<ResumeReport, ChannelWaitError> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            return Err(RuntimeError::ShuttingDown.into());
        }
        let record = self.record_for(handle)?;

        // Same fail-closed gate as `rearm_channel_waits`: a terminal or
        // already-cancelled fiber has nothing to re-drive and never
        // performs a durable side effect.
        if is_terminal(*lock_unpoisoned(&record.state)) || record.scope.is_cancelled() {
            return Ok(ResumeReport::empty_for(resumable.binding()));
        }

        let replay = BindingEventProjection::project(waits, resumable.binding())?;
        let plan = resumable.resume(&replay)?;

        // Fail-closed plan validation before any arming (the self-flip is a
        // durable write, so nothing may be armed before the plan checks out).
        let pending_ids: Vec<WaitId> = replay
            .events
            .iter()
            .filter(|event| event.record.state == WaitState::Pending)
            .map(|event| event.record.wait_id)
            .collect();
        if plan
            .rearm_wait_ids
            .iter()
            .any(|wait_id| !pending_ids.contains(wait_id))
        {
            return Err(ChannelWaitError::ResumePlanMismatch);
        }

        let mut report = ResumeReport {
            replay: replay.clone(),
            rearmed_satisfied: Vec::new(),
            rearmed_pending: Vec::new(),
            already_woken: Vec::new(),
            cancelled: Vec::new(),
        };
        for event in replay.events {
            match event.record.state {
                // At-least-once: the durable wake already happened. The
                // framework neither re-mounts the wait nor consumes an
                // early-buffered placeholder wake — the incarnation's own
                // later registration or rearm still resolves immediately.
                WaitState::Woken => report.already_woken.push(event.record),
                // Terminal on the durable side; never resurrected.
                WaitState::Cancelled => report.cancelled.push(event.record),
                WaitState::Pending => {
                    if !plan.rearm_wait_ids.contains(&event.record.wait_id) {
                        // Planned out: reported as a fact of the replay, not
                        // armed; a later resume can still arm it.
                        continue;
                    }
                    match arm_durable_row(&self.inner, &handle, &record, waits, event.record)? {
                        // The fiber turned terminal mid-resume: the row
                        // stays durable-`PENDING` for a later pass.
                        RowArming::NotArmed => {}
                        RowArming::Satisfied(armed) => report.rearmed_satisfied.push(armed),
                        RowArming::Rearmed(armed) => report.rearmed_pending.push(armed),
                    }
                }
            }
        }
        Ok(report)
    }
}
