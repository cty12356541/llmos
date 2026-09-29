//! Reclaim-arm [`OperationCommandExecutor`] driving the nlos-task
//! working-set reclaim entry (W29-D plan row: reclaim→WorkingSetReclaim).
//!
//! Execution path of one `reclaim_operation`:
//!
//! 1. the wired [`WorkingSetOccupancySource`] supplies the observed active
//!    working-set count (nlos-task publishes no store-wide live counter for
//!    external callers — recorded gap — so the host owns the observation);
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
    ReclaimPhase, ScaleProfile, execute_working_set_reclaim_execution,
    inspect_working_set_pressure, plan_working_set_reclaim_execution,
};
use nlos_types::ReceiptId;

use crate::executor_receipt::derive_executor_receipt_id;
use crate::{OperationCommandExecutor, OperationControlRequest};

const RECLAIM_RECEIPT_DOMAIN: &[u8] = b"nlos/system-control/reclaim-receipt/v1";

/// Live source of the active working-set count the reclaim executor
/// consults at dispatch time. The count is the host's observation of the
/// configured tier's outstanding units.
pub trait WorkingSetOccupancySource: Send + Sync {
    /// Current active working-set count under the configured profile.
    fn active_working_set_count(&self) -> u64;
}

/// Fixed occupancy for hosts that re-wire the executor per observation
/// window (and for tests).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FixedWorkingSetOccupancy(pub u64);

impl WorkingSetOccupancySource for FixedWorkingSetOccupancy {
    fn active_working_set_count(&self) -> u64 {
        self.0
    }
}

/// Reclaims one operational target's working set through the nlos-task
/// reclaim entry.
pub struct WorkingSetReclaimExecutor<'a> {
    profile: ScaleProfile,
    occupancy: &'a dyn WorkingSetOccupancySource,
}

impl<'a> WorkingSetReclaimExecutor<'a> {
    /// Wires the executor to one scale tier and occupancy source.
    #[must_use]
    pub const fn new(profile: ScaleProfile, occupancy: &'a dyn WorkingSetOccupancySource) -> Self {
        Self { profile, occupancy }
    }
}

impl OperationCommandExecutor for WorkingSetReclaimExecutor<'_> {
    fn reclaim_operation(
        &self,
        request: OperationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        let active_count = self.occupancy.active_working_set_count();
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
