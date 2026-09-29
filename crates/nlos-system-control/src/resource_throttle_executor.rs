//! Throttle-arm [`OperationCommandExecutor`] backed by
//! [`nlos_resource::ResourceAuthority`] demand rows (W29-D plan row:
//! throttle→ResourceDemand).
//!
//! Execution path of one `throttle_operation`:
//!
//! 1. [`nlos_resource::ResourceAuthority::inspect_reservation`] — the
//!    target addresses a reservation; its row carries the current declared
//!    `ResourceDemand` and the `usage_high_water_seq` that serves as the
//!    revision CAS (mismatch is a typed `CONFLICT`);
//! 2. [`nlos_resource::ResourceAuthority::inspect_quote`] — the
//!    reservation's immutable quote row carries the declared per-dimension
//!    demand capacity, the admission bound;
//! 3. [`nlos_resource::throttle_demand`] — the authoritative adjustment:
//!    every demand dimension scales down to the requested whole percent
//!    and the result is admission-checked against the quote capacity;
//! 4. the control-plane receipt id is derived (domain-separated SHA-256)
//!    from the before/after demand dimensions, the percent, and the
//!    command idempotency key, so it cannot exist without the
//!    authority-driven adjustment.
//!
//! Honest scope: the reservation model declares demand immutably at reserve
//! time, so the adjusted demand is computed and admission-checked here and
//! durably recorded through the append-only throttle-decision ledger
//! (`record_throttle_decision`, W44-RA): the decision chain folds into the
//! effective-demand read face and survives restarts. Working-set reclamation
//! is a separate lane (its occupancy model is not isomorphic to demand
//! throttling; recorded as a gap in the W29-D evidence).
//!
//! Every other arm refuses fail-closed; hosts compose authorities by
//! delegation, exactly like this type does.

use nlos_resource::{
    RecordThrottleDecisionRequest, ResourceAuthority, ResourceAuthorityError, throttle_demand,
};
use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_types::{IdempotencyKey, ReceiptId, ReservationId};

use crate::executor_receipt::derive_executor_receipt_id;
use crate::{OperationCommandExecutor, OperationControlRequest};

const THROTTLE_RECEIPT_DOMAIN: &[u8] = b"nlos/system-control/throttle-receipt/v1";

/// Throttles one reservation's declared demand through the real resource
/// authority read path and the authoritative demand adjustment.
pub struct ResourceDemandThrottleExecutor<'a> {
    authority: &'a ResourceAuthority,
}

impl<'a> ResourceDemandThrottleExecutor<'a> {
    /// Wires the executor to one durable resource authority.
    #[must_use]
    pub const fn new(authority: &'a ResourceAuthority) -> Self {
        Self { authority }
    }
}

impl OperationCommandExecutor for ResourceDemandThrottleExecutor<'_> {
    fn throttle_operation(
        &self,
        request: OperationControlRequest,
        throttle_percent: u64,
    ) -> Result<ReceiptId, SabiFailure> {
        if !(1..=100).contains(&throttle_percent) {
            return Err(bounded_failure(
                SabiErrorCode::InvalidArgument,
                "throttle percent must be a whole percent from 1 to 100",
            ));
        }
        let reservation_id = ReservationId::from_bytes(request.target_id);
        let reservation = self
            .authority
            .inspect_reservation(reservation_id)
            .map_err(|error| map_resource_throttle_error(&error))?;
        if reservation.usage_high_water_seq != request.expected_generation_or_revision {
            return Err(bounded_failure(
                SabiErrorCode::Conflict,
                "throttle CAS mismatch: expected revision is not the reservation usage sequence",
            ));
        }
        // Command-identity replay: the ledger's `demand_before` is
        // chain-derived, so a recomputed attempt would otherwise disagree
        // with the recorded transition. The submit handler owns command
        // idempotency; here the key is the command identity, and a recorded
        // decision under it replays byte-equal.
        let command_key = IdempotencyKey::from_bytes(request.idempotency_key);
        if let Some(existing) = self
            .authority
            .inspect_throttle_decision_by_key(reservation_id, command_key)
            .map_err(|error| map_resource_throttle_error(&error))?
        {
            return Ok(throttle_receipt(
                &request,
                existing.throttle_percent,
                existing.demand_before,
                existing.demand_after,
            ));
        }
        let quote = self
            .authority
            .inspect_quote(reservation.quote_id)
            .map_err(|error| map_resource_throttle_error(&error))?;
        // Chain-aware: the decision ledger validates `demand_before` against
        // the current effective demand (declared demand folded with the
        // decision chain), so a second throttle composes on the first
        // instead of re-deriving from the declared demand.
        let effective = self
            .authority
            .inspect_effective_demand(reservation_id)
            .map_err(|error| map_resource_throttle_error(&error))?;
        let throttle = throttle_demand(
            effective.effective_demand,
            quote.demand_capacity,
            throttle_percent,
        );
        let decided_at_ms = u64::try_from(request.requested_at_ms).map_err(|_| {
            bounded_failure(
                SabiErrorCode::InvalidArgument,
                "requested_at_ms must be a non-negative wall-clock millisecond value",
            )
        })?;
        self.authority
            .record_throttle_decision(RecordThrottleDecisionRequest {
                reservation_id,
                throttle_percent,
                demand_before: throttle.demand_before,
                demand_after: throttle.demand_after,
                idempotency_key: command_key,
                decided_at_ms,
            })
            .map_err(|error| map_resource_throttle_error(&error))?;
        Ok(throttle_receipt(
            &request,
            throttle.throttle_percent,
            throttle.demand_before,
            throttle.demand_after,
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

    fn reclaim_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(arm_not_wired("reclaim"))
    }
}

/// Derives the control-plane receipt over one recorded transition: the
/// demand dimensions before and after, the percent, and the command's
/// idempotency key — replaying a recorded decision re-derives byte-equal
/// inputs.
fn throttle_receipt(
    request: &OperationControlRequest,
    percent: u64,
    before_demand: nlos_resource::ResourceDemand,
    after_demand: nlos_resource::ResourceDemand,
) -> ReceiptId {
    let before = [
        before_demand.cpu_shares,
        before_demand.memory_mib,
        before_demand.io_weight,
    ]
    .map(u64::to_be_bytes);
    let after = [
        after_demand.cpu_shares,
        after_demand.memory_mib,
        after_demand.io_weight,
    ]
    .map(u64::to_be_bytes);
    let percent = percent.to_be_bytes();
    derive_executor_receipt_id(
        THROTTLE_RECEIPT_DOMAIN,
        &[
            &request.target_id,
            &before[0],
            &before[1],
            &before[2],
            &after[0],
            &after[1],
            &after[2],
            &percent,
            &request.idempotency_key,
        ],
    )
}

fn arm_not_wired(arm: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: format!("the {arm} arm is not wired in the resource throttle executor"),
    }
}

fn bounded_failure(code: SabiErrorCode, message: &'static str) -> SabiFailure {
    SabiFailure {
        code: code.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

fn map_resource_throttle_error(error: &ResourceAuthorityError) -> SabiFailure {
    let (code, safe_message) = match error {
        ResourceAuthorityError::ReservationNotFound => (
            SabiErrorCode::NotFound,
            "requested resource reservation was not found",
        ),
        ResourceAuthorityError::QuoteNotFound => (
            SabiErrorCode::NotFound,
            "requested resource quote was not found",
        ),
        ResourceAuthorityError::StaleThrottleDemand { .. } => (
            SabiErrorCode::Conflict,
            "throttle demand_before is not the reservation's current effective demand",
        ),
        ResourceAuthorityError::InvalidThrottleAdjustment { .. } => (
            SabiErrorCode::Conflict,
            "throttle demand_after does not equal the clamped demand_before",
        ),
        ResourceAuthorityError::InvalidThrottlePercent => (
            SabiErrorCode::InvalidArgument,
            "throttle percent is outside the authority domain",
        ),
        ResourceAuthorityError::InvalidThrottleTimestamp => (
            SabiErrorCode::InvalidArgument,
            "throttle decision timestamp predates the reservation history",
        ),
        ResourceAuthorityError::Sqlite(_)
        | ResourceAuthorityError::DurabilityUnavailable { .. }
        | ResourceAuthorityError::CorruptRecord(_)
        | ResourceAuthorityError::SchemaVersionUnsupported(_)
        | ResourceAuthorityError::LockPoisoned => (
            SabiErrorCode::Durability,
            "resource authority storage failure",
        ),
        ResourceAuthorityError::Io(_) => (SabiErrorCode::Driver, "resource authority I/O failure"),
        _ => (
            SabiErrorCode::Conflict,
            "resource authority rejected the throttle read",
        ),
    };
    bounded_failure(code, safe_message)
}
