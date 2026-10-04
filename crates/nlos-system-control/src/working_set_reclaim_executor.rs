//! Reclaim-arm [`OperationCommandExecutor`] driving the nlos-task
//! working-set reclaim entry (W29-D plan row: reclaim→WorkingSetReclaim).
//!
//! Execution path of one `reclaim_operation`:
//!
//! 1. the wired [`WorkingSetOccupancySource`] supplies the observed active
//!    working-set count. Hosts that own a live task authority wire the
//!    real [`TaskAuthorityWorkingSetOccupancy`] adapter over
//!    [`nlos_task::SqliteTaskAuthority::inspect_working_set_pressure`]
//!    (the store-wide issued-permit count — the same observation face the
//!    authority's own admission gates consult); hosts without one re-wire
//!    per observation window with [`FixedWorkingSetOccupancy`];
//! 2. [`nlos_task::inspect_working_set_pressure`] computes the pressure
//!    snapshot against the configured [`nlos_task::ScaleProfile`]; the
//!    wire's `expected_generation_or_revision` is the CAS on that observed
//!    count (a moved occupancy is a typed `CONFLICT`), and a count below
//!    the soft reclaim threshold refuses fail-closed (`STATE`) — there is
//!    nothing honest to reclaim;
//! 3. [`nlos_task::plan_working_set_reclaim_execution`] and
//!    [`nlos_task::execute_working_set_reclaim_execution`] — the nlos-task
//!    reclaim execution entry (first `RebuildableCache` phase; the
//!    evicted-unit count is nlos-task's typed synthetic stand-in for the
//!    Context Residency Controller, as documented in `nlos-task`);
//! 4. the control-plane receipt id is derived (domain-separated SHA-256)
//!    from the executed phase, sequence, evicted units, threshold, target,
//!    and command idempotency key, so it cannot exist without the
//!    authority-driven execution.
//!
//! Every other arm refuses fail-closed; hosts compose authorities by
//! delegation, exactly like this type does.

use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_task::{
    ReclaimPhase, ScaleProfile, SqliteTaskAuthority, TaskStoreError,
    execute_working_set_reclaim_execution, inspect_working_set_pressure,
    plan_working_set_reclaim_execution,
};
use nlos_types::ReceiptId;

use crate::executor_receipt::derive_executor_receipt_id;
use crate::{OperationCommandExecutor, OperationControlRequest};

const RECLAIM_RECEIPT_DOMAIN: &[u8] = b"nlos/system-control/reclaim-receipt/v1";

/// Live source of the active working-set count the reclaim executor
/// consults at dispatch time. The count is the host's observation of the
/// configured tier's outstanding units.
///
/// The read is fallible by contract: a host whose observation crosses a
/// durable store surfaces that failure as a typed [`SabiFailure`]
/// (`DURABILITY`) instead of under-reporting a synthetic count, so a
/// broken observation face refuses the arm instead of fabricating
/// pressure facts.
pub trait WorkingSetOccupancySource: Send + Sync {
    /// Current active working-set count under the configured profile.
    ///
    /// # Errors
    ///
    /// Returns a bounded [`SabiFailure`] when the observation face cannot
    /// be read.
    fn active_working_set_count(&self) -> Result<u64, SabiFailure>;
}

/// Fixed occupancy for hosts that re-wire the executor per observation
/// window (and for tests).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FixedWorkingSetOccupancy(pub u64);

impl WorkingSetOccupancySource for FixedWorkingSetOccupancy {
    fn active_working_set_count(&self) -> Result<u64, SabiFailure> {
        Ok(self.0)
    }
}

/// Real [`WorkingSetOccupancySource`] over one live task authority: the
/// store-wide issued-`CommitPermit` count, read through the authority's
/// own public pressure face
/// ([`SqliteTaskAuthority::inspect_working_set_pressure`]) — the same
/// observation the authority's working-set admission gate consults, so a
/// host wired to this adapter and the authority it observes cannot
/// disagree about the tier's occupancy. Minimal real adapter by design:
/// the host still owns the [`ScaleProfile`] the executor judges pressure
/// against (bind it to the tier the authority was opened with).
pub struct TaskAuthorityWorkingSetOccupancy<'a> {
    tasks: &'a SqliteTaskAuthority,
}

impl<'a> TaskAuthorityWorkingSetOccupancy<'a> {
    /// Wires the occupancy observation to one live task authority.
    #[must_use]
    pub const fn new(tasks: &'a SqliteTaskAuthority) -> Self {
        Self { tasks }
    }
}

impl WorkingSetOccupancySource for TaskAuthorityWorkingSetOccupancy<'_> {
    fn active_working_set_count(&self) -> Result<u64, SabiFailure> {
        self.tasks
            .inspect_working_set_pressure()
            .map(|snapshot| snapshot.active_count)
            .map_err(|error| map_task_occupancy_error(&error))
    }
}

fn map_task_occupancy_error(error: &TaskStoreError) -> SabiFailure {
    let code = match error {
        TaskStoreError::CorruptRecord(_) | TaskStoreError::UnsupportedSchema(_) => {
            SabiErrorCode::Driver
        }
        _ => SabiErrorCode::Durability,
    };
    SabiFailure {
        code: code.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: "task authority working-set occupancy read failed".to_owned(),
    }
}

/// Reclaims one operational target's working set through the nlos-task
/// reclaim entry. The executor owns its occupancy source; hosts hand one
/// in by value ([`TaskAuthorityWorkingSetOccupancy`] for a live authority,
/// [`FixedWorkingSetOccupancy`] for per-window re-wiring).
pub struct WorkingSetReclaimExecutor<'a> {
    profile: ScaleProfile,
    occupancy: Box<dyn WorkingSetOccupancySource + 'a>,
}

impl<'a> WorkingSetReclaimExecutor<'a> {
    /// Wires the executor to one scale tier and one owned occupancy source.
    #[must_use]
    pub fn new(profile: ScaleProfile, occupancy: impl WorkingSetOccupancySource + 'a) -> Self {
        Self {
            profile,
            occupancy: Box::new(occupancy),
        }
    }
}

impl OperationCommandExecutor for WorkingSetReclaimExecutor<'_> {
    fn reclaim_operation(
        &self,
        request: OperationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        let active_count = self.occupancy.active_working_set_count()?;
        let snapshot = inspect_working_set_pressure(&self.profile, active_count);
        if snapshot.active_count != request.expected_generation_or_revision {
            return Err(bounded_failure(
                SabiErrorCode::Conflict,
                "reclaim CAS mismatch: the observed working-set count moved",
            ));
        }
        let Some(advisory) = snapshot.reclaim_advisory else {
            return Err(bounded_failure(
                SabiErrorCode::State,
                "working set is below the soft reclaim threshold; nothing to reclaim",
            ));
        };
        let execution = plan_working_set_reclaim_execution(&advisory);
        let outcome = execute_working_set_reclaim_execution(&execution);
        let sequence = [
            outcome.execution_sequence,
            reclaim_phase_code(outcome.phase),
        ];
        let evicted_units = outcome.evicted_units.to_be_bytes();
        let threshold = outcome.advisory.reclaim_threshold_count.to_be_bytes();
        Ok(derive_executor_receipt_id(
            RECLAIM_RECEIPT_DOMAIN,
            &[
                &request.target_id,
                &sequence,
                &evicted_units,
                &threshold,
                &request.idempotency_key,
            ],
        ))
    }

    fn pause_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("pause"))
    }

    fn resume_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("resume"))
    }

    fn cancel_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("cancel"))
    }

    fn kill_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("kill"))
    }

    fn throttle_operation(
        &self,
        _: OperationControlRequest,
        _: u64,
    ) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("throttle"))
    }
}

/// Stable discriminator of the executed reclaim phase, matching the
/// `TASK_DEFAULT_RECLAIM_POLICY` ordering (least to most disruptive).
const fn reclaim_phase_code(phase: ReclaimPhase) -> u8 {
    match phase {
        ReclaimPhase::RebuildableCache => 0,
        ReclaimPhase::DegradeBackgroundQos => 1,
        ReclaimPhase::CheckpointEvict => 2,
        ReclaimPhase::Kill => 3,
    }
}

fn arm_not_wired(arm: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: format!("the {arm} arm is not wired in the working-set reclaim executor"),
    }
}

fn bounded_failure(code: SabiErrorCode, message: &'static str) -> SabiFailure {
    SabiFailure {
        code: code.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}
