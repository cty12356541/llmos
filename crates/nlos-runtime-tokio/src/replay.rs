//! Fiber replay facilities of ADR-0009/0012 (event-sourced resume): the
//! binding → durable event stream projection across the wait, effect and
//! queue authorities, the re-drive contract for a new fiber incarnation, and
//! the framework entry that re-arms the replayed waits through the same
//! single-row logic as [`crate::TokioRuntimeAdapter::rearm_channel_waits`].
//!
//! Decision 1 of ADR-0009, as completed by ADR-0012's registration-based
//! projection: a fiber's external interactions are durably fact-recorded
//! with the fiber's own registration identity (wait rows, effect fiber
//! registrations, queue consumption registrations), so rebuilding a fiber
//! means projecting those facts as one binding's event stream, re-driving
//! the code to its wait point in a **new incarnation**, and re-mounting the
//! waits through wait-side rehydration.
//!
//! - [`BindingEventProjection`] projects one binding's durable facts into a
//!   typed, registration-ordered [`BindingReplay`]. Every fact source is a
//!   read-only view over an existing authority (ADR-0009: no authority is
//!   invented); the effect and queue sources joined in ADR-0012, once their
//!   authorities gained the binding columns.
//! - [`ResumableBinding`] is the re-drive contract, gated by the ADR-0012
//!   generation check: before anything is re-driven the new incarnation can
//!   present its durable incarnation generation, validated against the
//!   process authority's current registration (stale → fail-closed, zero
//!   side effect). [`TokioRuntimeAdapter::resume_binding`] validates the
//!   [`ResumePlan`] fail-closed and re-arms every planned still-`PENDING`
//!   wait through `arm_durable_row`; effect and queue events are report-only
//!   facts.
//! - The ADR-0009 decision-2 fallback (handler-entry snapshot) lives in
//!   [`crate::snapshot`].
//!
//! Exactly-once boundary (ADR-0009 decision 3): the replay covers only
//! durable interaction boundaries. Pure internal computation between two
//! durable interactions never enters the event stream — a resumed
//! incarnation continues from the last durable boundary, which is the
//! explicitly accepted semantic loss of this ADR. Idempotent consumption of
//! the replayed facts is carried by the existing durable dedup (wait row
//! uniqueness, notify idempotency keys, effect registration uniqueness),
//! not by this module.

use std::fmt;
use std::sync::atomic::Ordering;

use nlos_channel::ChannelAuthority;
use nlos_process::ProcessAuthority;
use nlos_runtime::{FiberHandle, RuntimeError};
use nlos_task::SqliteTaskAuthority;
use nlos_wait::{BindingId, WaitAuthority, WaitId, WaitRecord, WaitState};

use crate::channel_wait::{ChannelWaitError, RearmedChannelWait, RowArming, arm_durable_row};
use crate::wake::is_terminal;
use crate::{TokioRuntimeAdapter, lock_unpoisoned};

/// One wait event of a binding's replayed event stream: the durable wait
/// row exactly as the authority holds it, read-only to the replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayedWaitEvent {
    /// The durable wait row backing this event: channel, target sequence,
    /// state machine (`PENDING`/`WOKEN`/`CANCELLED`) and the registration,
    /// wake and cancellation timestamps.
    pub record: WaitRecord,
}

/// One effect event of a binding's replayed event stream: the durable
/// effect fiber registration joined with the slot's current state at
/// projection time (ADR-0012). A terminal slot state plus its receipt id is
/// the effect-completion fact; the receipt itself is read separately
/// through `SqliteTaskAuthority::inspect_effect_receipt`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayedEffectEvent {
    pub registration: nlos_task::EffectFiberRegistrationRecord,
}

/// One queue-consumption event of a binding's replayed event stream: the
/// durable consumption registration row (ADR-0012).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayedQueueConsumptionEvent {
    pub registration: nlos_channel::QueueConsumptionRecord,
}

/// One durable event of a binding's replayed event stream, across the three
/// registered authorities. Ordering is by registration time with a
/// deterministic authority tie-break (wait, effect, queue).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingReplayEvent {
    Wait(ReplayedWaitEvent),
    Effect(ReplayedEffectEvent),
    QueueConsumed(ReplayedQueueConsumptionEvent),
}

impl BindingReplayEvent {
    /// The registration timestamp of the durable fact.
    #[must_use]
    pub fn recorded_at_ms(&self) -> u64 {
        match self {
            Self::Wait(event) => event.record.registered_at_ms,
            Self::Effect(event) => {
                u64::try_from(event.registration.registered_at_ms).unwrap_or(u64::MAX)
            }
            Self::QueueConsumed(event) => event.registration.registered_at_ms,
        }
    }

    /// The authority rank of the tie-break (wait `0`, effect `1`, queue `2`).
    fn rank(&self) -> u8 {
        match self {
            Self::Wait(_) => 0,
            Self::Effect(_) => 1,
            Self::QueueConsumed(_) => 2,
        }
    }

    /// The wait event, if this is one.
    #[must_use]
    pub fn as_wait(&self) -> Option<&ReplayedWaitEvent> {
        match self {
            Self::Wait(event) => Some(event),
            _ => None,
        }
    }
}

/// The event-stream snapshot of one binding, projected from the durable
/// face (ADR-0009 decision 1, completed by ADR-0012). Events are ordered by
/// registration time across authorities, so the stream reads as the
/// interaction history the previous incarnations of this binding produced.
///
/// The projection is a pure read: it never writes the durable face, and
/// terminal facts are projected as they are, never re-driven.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingReplay {
    /// The binding this stream belongs to.
    pub binding: BindingId,
    /// The events in registration-time order.
    pub events: Vec<BindingReplayEvent>,
}

/// The borrowed fact sources of a projection: each is a read-only view over
/// an existing authority, and `None` simply skips that fact source. The
/// default value projects the wait registry alone (the minimal-prefix
/// behavior).
#[derive(Clone, Copy, Default)]
pub struct ReplayAuthorities<'a> {
    /// Queue-consumption registration source.
    pub channel: Option<&'a ChannelAuthority>,
    /// Effect-registration source.
    pub task: Option<&'a SqliteTaskAuthority>,
    /// The ADR-0012 generation gate source: when present together with the
    /// incarnation's [`ResumableBinding::expected_incarnation`], the
    /// binding's durable incarnation is validated against it before anything
    /// is re-driven.
    pub process: Option<&'a ProcessAuthority>,
}

impl fmt::Debug for ReplayAuthorities<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplayAuthorities")
            .field("channel", &self.channel.is_some())
            .field("task", &self.task.is_some())
            .field("process", &self.process.is_some())
            .finish()
    }
}

/// The projector of one binding's durable event stream (ADR-0009 decision 1,
/// ADR-0012 registration-based extension).
#[derive(Clone, Copy, Debug)]
pub struct BindingEventProjection;

impl BindingEventProjection {
    /// Projects `binding`'s durable events from the given fact sources.
    ///
    /// The wait registry is always consulted; `channel` and `task` add the
    /// ADR-0012 consumption and effect registration events. A binding with
    /// no facts projects the legal empty stream.
    ///
    /// # Errors
    ///
    /// Fails closed for the all-zero binding, any referenced channel that no
    /// longer exists, any tampered row, or an authority read failure.
    pub fn project(
        waits: &WaitAuthority,
        sources: ReplayAuthorities<'_>,
        binding: BindingId,
    ) -> Result<BindingReplay, ChannelWaitError> {
        let mut events: Vec<BindingReplayEvent> = waits
            .list_waits_for_binding(binding)?
            .into_iter()
            .map(|record| BindingReplayEvent::Wait(ReplayedWaitEvent { record }))
            .collect();
        if let Some(channel) = sources.channel {
            let fiber = nlos_types::ExecutionFiberId::from_bytes(*binding.as_bytes());
            events.extend(
                channel
                    .list_consumptions_for_binding(fiber)?
                    .into_iter()
                    .map(|registration| {
                        BindingReplayEvent::QueueConsumed(ReplayedQueueConsumptionEvent {
                            registration,
                        })
                    }),
            );
        }
        if let Some(task) = sources.task {
            let fiber = nlos_types::ExecutionFiberId::from_bytes(*binding.as_bytes());
            events.extend(
                task.list_effect_registrations_for_binding(fiber)?
                    .into_iter()
                    .map(|registration| {
                        BindingReplayEvent::Effect(ReplayedEffectEvent { registration })
                    }),
            );
        }
        events.sort_by(|left, right| {
            (left.recorded_at_ms(), left.rank()).cmp(&(right.recorded_at_ms(), right.rank()))
        });
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
/// Only still-`PENDING` *wait* events are armable; every `rearm_wait_ids`
/// entry must reference one, and [`TokioRuntimeAdapter::resume_binding`]
/// validates this fail-closed before arming anything. Effect and queue
/// events are not armable — the incarnation consumed them as facts inside
/// its `resume`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResumePlan {
    /// The durable `WaitId`s of the still-`PENDING` wait events the
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
                .filter_map(|event| event.as_wait())
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

    /// The durable fiber incarnation this re-drive belongs to, when the
    /// incarnation wants the ADR-0012 generation gate enforced: the gate
    /// fails the resume closed unless this is the binding's current
    /// registered incarnation. `None` (the default) skips the gate.
    #[must_use]
    fn expected_incarnation(&self) -> Option<nlos_types::Generation> {
        None
    }

    /// The durable process identity owning this binding, when the
    /// incarnation wants the ADR-0012 generation gate enforced. `None`
    /// (the default) skips the gate.
    #[must_use]
    fn process_id(&self) -> Option<nlos_types::ProcessId> {
        None
    }
}

/// The outcome of one [`TokioRuntimeAdapter::resume_binding`] call.
///
/// Bucket invariant mirrors [`crate::RearmReport`]: every `rearmed_satisfied`
/// entry's future resolves immediately with [`crate::WaitOutcome::Woken`] (the
/// wake fact was already established at arming time), every
/// `rearmed_pending` entry's future awaits a later
/// [`crate::TokioChannelWakeSink::deliver`] and resolves `Cancelled` on
/// scope cancel, fiber termination, supersession or shutdown exactly like
/// any registered wait. `already_woken` and `cancelled` are report-only
/// fact buckets: the framework performed zero action for them. The effect
/// and queue event buckets are likewise report-only: the framework never
/// re-drives them — the incarnation consumed them inside its `resume`.
#[derive(Debug)]
pub struct ResumeReport {
    /// The replay this resume consumed (registration-ordered events of
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
    /// Effect events of the replay: report-only facts (registration plus
    /// the slot's current state), never re-driven by the framework.
    pub effect_events: Vec<ReplayedEffectEvent>,
    /// Queue-consumption events of the replay: report-only facts, never
    /// re-driven by the framework.
    pub queue_events: Vec<ReplayedQueueConsumptionEvent>,
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
            effect_events: Vec::new(),
            queue_events: Vec::new(),
        }
    }
}

impl TokioRuntimeAdapter {
    /// Re-drives `resumable`'s binding in the new fiber incarnation
    /// `handle` (ADR-0009 decision 1, ADR-0012 extension): optionally gates
    /// on the binding's durable incarnation generation, projects the
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
    /// 4. the ADR-0012 generation gate: when `sources.process` is present
    ///    and the incarnation declares its expected incarnation and process,
    ///    the binding's current registered incarnation must equal the
    ///    expected one ([`ChannelWaitError::StaleFiberIncarnation`]
    ///    otherwise) — a stale incarnation's resume takes zero durable side
    ///    effect;
    /// 5. projection ([`BindingEventProjection::project`]);
    /// 6. the incarnation's `resume` — a rejection aborts with
    ///    [`ChannelWaitError::ResumeRejected`] before anything is armed;
    /// 7. plan validation — every planned id must reference a still-`PENDING`
    ///    *wait* event of the replay, else
    ///    [`ChannelWaitError::ResumePlanMismatch`] before anything is armed.
    ///
    /// Per replay event:
    ///
    /// - `WOKEN` → `already_woken` bucket, zero action (no re-mount, no
    ///   buffer consumption): the incarnation decides to skip or re-execute;
    ///   a later registration/rearm of the same wait still resolves
    ///   immediately (at-least-once preserved);
    /// - `CANCELLED` → `cancelled` bucket, zero action;
    /// - effect / queue-consumption events → report-only fact buckets, the
    ///   framework never re-drives them;
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
    /// fiber handles, [`ChannelWaitError::WaitAuthority`] /
    /// [`ChannelWaitError::ChannelAuthority`] /
    /// [`ChannelWaitError::TaskAuthority`] for durable authority failures
    /// (projection, high-water reads, the self-flip),
    /// [`ChannelWaitError::ProcessAuthority`] for the generation gate's
    /// readback, [`ChannelWaitError::StaleFiberIncarnation`] when the gate
    /// rejects the incarnation,
    /// [`ChannelWaitError::ResumeRejected`] when the incarnation rejects
    /// the re-drive, and [`ChannelWaitError::ResumePlanMismatch`] for a plan
    /// referencing a wait that is not a still-`PENDING` event of the replay.
    pub fn resume_binding(
        &self,
        handle: FiberHandle,
        waits: &WaitAuthority,
        sources: ReplayAuthorities<'_>,
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

        // ADR-0012 generation gate: validate the binding's durable
        // incarnation before anything is re-driven.
        if let (Some(process), Some(process_id), Some(expected)) = (
            sources.process,
            resumable.process_id(),
            resumable.expected_incarnation(),
        ) {
            let fiber = nlos_types::ExecutionFiberId::from_bytes(*resumable.binding().as_bytes());
            let current = process.inspect_fiber_incarnation(process_id, fiber)?;
            if current.incarnation_generation != expected {
                return Err(ChannelWaitError::StaleFiberIncarnation);
            }
        }

        let replay = BindingEventProjection::project(waits, sources, resumable.binding())?;
        let plan = resumable.resume(&replay)?;

        // Fail-closed plan validation before any arming (the self-flip is a
        // durable write, so nothing may be armed before the plan checks out).
        let pending_ids: Vec<WaitId> = replay
            .events
            .iter()
            .filter_map(|event| event.as_wait())
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
            effect_events: Vec::new(),
            queue_events: Vec::new(),
        };
        for event in replay.events {
            match event {
                // Report-only facts: the durable face is the authority and
                // the framework never re-drives these.
                BindingReplayEvent::Effect(effect) => report.effect_events.push(effect),
                BindingReplayEvent::QueueConsumed(queue) => report.queue_events.push(queue),
                BindingReplayEvent::Wait(event) => match event.record.state {
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
                },
            }
        }
        Ok(report)
    }
}
