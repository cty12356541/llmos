//! Optional [`OperationInspectSource`] adapter backed by the durable
//! [`nlos_store::SqliteOperationStore`] state-machine rows (W32-G, B5-3).
//!
//! Enabled with the crate's `store` feature; the default control prefix
//! uses [`crate::UnwiredOperationInspectSource`] until a host wires this
//! adapter.

use std::num::NonZeroU64;

use nlos_operation::{OperationHandle, OperationState};
use nlos_schema::sabi::v1::{
    DurableOperationState as WireState, RetryDirective, SabiErrorCode, SabiFailure,
};
use nlos_store::{SqliteOperationStore, StoreError};
use nlos_types::{Generation, OperationId};

use crate::OperationInspectSource;
use crate::control::DurableOperationInspection;

/// Reads bounded durable operation facts through the operation store.
pub struct OperationStoreSource<'a> {
    store: &'a SqliteOperationStore,
}

impl<'a> OperationStoreSource<'a> {
    #[must_use]
    pub const fn new(store: &'a SqliteOperationStore) -> Self {
        Self { store }
    }
}

impl OperationInspectSource for OperationStoreSource<'_> {
    fn inspect_operation(
        &self,
        operation_id: [u8; 16],
        generation: u64,
    ) -> Result<DurableOperationInspection, SabiFailure> {
        let Some(generation) = NonZeroU64::new(generation) else {
            return Err(SabiFailure {
                code: SabiErrorCode::InvalidArgument.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "operation generation must be a non-zero generation".to_owned(),
            });
        };
        let snapshot = self
            .store
            .inspect(OperationHandle {
                operation_id: OperationId::from_bytes(operation_id),
                generation: Generation::new(generation),
            })
            .map_err(|error| map_store_error(&error))?;
        let (state, outcome_receipt_id) = operation_state(snapshot.state);
        Ok(DurableOperationInspection {
            operation_id: *snapshot.handle.operation_id.as_bytes(),
            generation: snapshot.handle.generation.get(),
            state,
            cancel_epoch: snapshot.cancel_epoch.get(),
            owner_fiber_id: *snapshot.owner_fiber.fiber_id.as_bytes(),
            owner_fiber_generation: snapshot.owner_fiber.generation.get(),
            outcome_receipt_id,
        })
    }
}

fn operation_state(state: OperationState) -> (WireState, Option<Vec<u8>>) {
    let receipt = |receipt_id: nlos_types::ReceiptId| Some(receipt_id.into_bytes().to_vec());
    match state {
        OperationState::Registered => (WireState::Registered, None),
        OperationState::Dispatched => (WireState::Dispatched, None),
        OperationState::CancelRequested => (WireState::CancelRequested, None),
        OperationState::Completed { receipt_id } => (WireState::Completed, receipt(receipt_id)),
        OperationState::Failed { receipt_id } => (WireState::Failed, receipt(receipt_id)),
        OperationState::CancelledBeforeEffect { receipt_id } => {
            (WireState::CancelledBeforeEffect, receipt(receipt_id))
        }
        OperationState::PartialEffect { receipt_id } => {
            (WireState::PartialEffect, receipt(receipt_id))
        }
        OperationState::EffectUnknown { receipt_id } => {
            (WireState::EffectUnknown, receipt(receipt_id))
        }
    }
}

/// The store exposes no dedicated not-found variant for operation rows: a
/// missing row resolves exactly like a stale generation
/// (`OperationError::InvalidGeneration`), so both map to one bounded
/// `NOT_FOUND` reading that names the ambiguity honestly.
fn map_store_error(error: &StoreError) -> SabiFailure {
    let (code, retry, safe_message) = match &error {
        StoreError::Operation(nlos_operation::OperationError::InvalidGeneration) => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested operation row was not found or the generation is stale",
        ),
        StoreError::Operation(_) => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "operation store rejected the inspection request",
        ),
        StoreError::Sqlite(_) => (
            SabiErrorCode::Durability,
            RetryDirective::RetrySameIdempotencyKey,
            "operation store storage failure; retry with the same idempotency key",
        ),
        StoreError::DurabilityUnavailable { .. }
        | StoreError::UnsupportedSchema(_)
        | StoreError::CorruptRecord(_)
        | StoreError::LockPoisoned => (
            SabiErrorCode::Durability,
            RetryDirective::DoNotRetry,
            "operation store durability configuration is unavailable",
        ),
        _ => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "operation store rejected the inspection request",
        ),
    };
    SabiFailure {
        code: code.into(),
        retry: retry.into(),
        safe_message: safe_message.to_owned(),
    }
}
