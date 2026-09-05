//! Optional [`ResourceInspector`] adapter backed by
//! [`nlos_resource::ResourceAuthority`].
//!
//! Enabled with the crate's `resource` feature; the default control prefix uses
//! [`crate::control::UnwiredResourceInspector`] until a host wires this adapter.

use nlos_resource::{ResourceAuthority, ResourceAuthorityError};
use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_types::ReservationId;

use crate::control::{ResourceInspection, ResourceInspector};

/// Reads bounded resource cost facts through the durable Resource authority.
pub struct ResourceAuthorityInspector<'a> {
    authority: &'a ResourceAuthority,
}

impl<'a> ResourceAuthorityInspector<'a> {
    #[must_use]
    pub const fn new(authority: &'a ResourceAuthority) -> Self {
        Self { authority }
    }
}

impl ResourceInspector for ResourceAuthorityInspector<'_> {
    fn inspect_resource(
        &self,
        reservation_id: [u8; 16],
    ) -> Result<ResourceInspection, SabiFailure> {
        let reservation_id = ReservationId::from_bytes(reservation_id);
        let receipt = self
            .authority
            .inspect_cost_receipt(reservation_id)
            .map_err(map_resource_authority_error)?;
        Ok(ResourceInspection {
            reservation_id: *receipt.reservation_id.as_bytes(),
            account_id: *receipt.account_id.as_bytes(),
            upper_bound: receipt.upper_bound,
            usage_high_water: receipt.finalization.high_water,
            consumption_count: u32::try_from(receipt.consumptions.len()).unwrap_or(u32::MAX),
        })
    }
}

fn map_resource_authority_error(error: &ResourceAuthorityError) -> SabiFailure {
    let (code, retry, safe_message) = match error {
        ResourceAuthorityError::ReservationNotFound
        | ResourceAuthorityError::AccountNotFound
        | ResourceAuthorityError::QuoteNotFound
        | ResourceAuthorityError::DriverNotFound => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested resource reservation was not found",
        ),
        ResourceAuthorityError::ReservationNotActive
        | ResourceAuthorityError::ReservationAlreadyActive
        | ResourceAuthorityError::ReservationQuarantined
        | ResourceAuthorityError::ReservationFinalized => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "resource reservation is not settled for cost inspection",
        ),
        ResourceAuthorityError::Sqlite(_)
        | ResourceAuthorityError::DurabilityUnavailable { .. }
        | ResourceAuthorityError::CorruptRecord(_)
        | ResourceAuthorityError::SchemaVersionUnsupported(_)
        | ResourceAuthorityError::LockPoisoned => (
            SabiErrorCode::Durability,
            RetryDirective::DoNotRetry,
            "resource authority storage failure",
        ),
        ResourceAuthorityError::Io(_) => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "resource authority I/O failure",
        ),
        ResourceAuthorityError::IdempotencyConflict
        | ResourceAuthorityError::ConsumptionSequenceConflict
        | ResourceAuthorityError::FinalizeSequenceConflict
        | ResourceAuthorityError::ReservationBindingMismatch => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "resource authority state conflict",
        ),
        ResourceAuthorityError::StaleDriver
        | ResourceAuthorityError::InvalidUpperBound
        | ResourceAuthorityError::QuoteExpired
        | ResourceAuthorityError::InvalidUsageSequence
        | ResourceAuthorityError::UsageExceedsUpperBound { .. }
        | ResourceAuthorityError::UsageNotMonotonic { .. }
        | ResourceAuthorityError::InvalidQuarantineTimestamp
        | ResourceAuthorityError::InvalidFinalizeTimestamp
        | ResourceAuthorityError::GenerationExhausted
        | ResourceAuthorityError::InsufficientCredit { .. } => (
            SabiErrorCode::InvalidArgument,
            RetryDirective::DoNotRetry,
            "resource inspection request violates the authority contract",
        ),
    };
    SabiFailure {
        code: code.into(),
        retry: retry.into(),
        safe_message: safe_message.to_owned(),
    }
}
