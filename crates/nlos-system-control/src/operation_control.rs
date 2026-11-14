//! Daemon-mounted `OperationControl` adapter (`L-011`, W82-A): the frozen
//! v1.0 `service = "operation_control"` channel behind the system-control
//! daemon's five-way dispatch, answering `query` and `cancel` against the
//! durable [`SqliteOperationStore`] the daemon already owns
//! (`SystemControlDaemon::operations`).
//!
//! Semantics mirror the conformance reference
//! (`nlos-ipc/src/bin/nlos-directory-chain.rs`): `query` is a QUERY, `cancel`
//! is a MUTATION with the cancel-epoch compare-and-set fence and a
//! deterministic cancel receipt (`nlos.operation-control.cancel-receipt.v1`
//! over the operation id, generation, and expected epoch — the same
//! derivation the conformance harness uses, so replays agree across mounts).
//! One deliberate difference from the reference harness: where the harness
//! drops the connection on a store rejection, this resident dispatch answers
//! with a typed failure envelope (same code vocabulary, bounded safe message)
//! so one bad request never breaks a persistent session — the daemon's
//! failure envelopes keep the frozen invariants (payload cleared, no
//! receipts, no operation, correlation echo).

use nlos_operation::{OperationHandle, OperationSnapshot, OperationState};
use nlos_schema::sabi::v1::{
    Envelope, OperationLifecycleState, OperationReference, OperationStatus, ReceiptReference,
    RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext, SabiResponseContext, envelope,
};
use nlos_schema::{
    MethodSemantics, REQUEST_ID_BYTES, decode_cancel_operation_request,
    decode_query_operation_request, encode_operation_status, operation_control_schema_identity,
    validate_sabi_request_context,
};
use nlos_store::{SqliteOperationStore, StoreError};
use nlos_types::{CancelEpoch, Generation, OperationId, ReceiptId};
use sha2::{Digest, Sha256};
use std::num::NonZeroU64;

/// Frozen v1.0 service name (matches the conformance harness constant).
pub const OPERATION_CONTROL_SERVICE: &str = "operation_control";
/// Frozen v1.0 method names.
pub const QUERY_OPERATION_METHOD: &str = "query";
pub const CANCEL_OPERATION_METHOD: &str = "cancel";

/// Typed `OperationControl` dispatch failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationControlError {
    UnknownMethod,
    InvalidRequest,
    InvalidIdentifier,
    NotFoundOrStale,
    Conflict,
    Store,
    Encode,
}

impl OperationControlError {
    fn to_sabi_failure(self) -> SabiFailure {
        let (code, retry, message): (SabiErrorCode, RetryDirective, &str) = match self {
            Self::UnknownMethod => (
                SabiErrorCode::NotSupported,
                RetryDirective::DoNotRetry,
                "unknown OperationControl service or method",
            ),
            Self::InvalidRequest | Self::InvalidIdentifier => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the OperationControl contract",
            ),
            Self::NotFoundOrStale => (
                SabiErrorCode::NotFound,
                RetryDirective::DoNotRetry,
                "operation not found or the generation is stale",
            ),
            Self::Conflict => (
                SabiErrorCode::Conflict,
                RetryDirective::DoNotRetry,
                "OperationControl compare-and-set fence rejected the request",
            ),
            Self::Store => (
                SabiErrorCode::Driver,
                RetryDirective::DoNotRetry,
                "durable OperationControl store failure; do not retry",
            ),
            Self::Encode => (
                SabiErrorCode::Driver,
                RetryDirective::DoNotRetry,
                "OperationControl result encoding failed",
            ),
        };
        SabiFailure {
            code: code.into(),
            retry: retry.into(),
            safe_message: message.to_owned(),
        }
    }
}

fn failure_envelope(request: &Envelope, error: OperationControlError) -> Envelope {
    let correlation_id = match request.common_context.as_ref() {
        Some(envelope::CommonContext::RequestContext(context))
            if context.correlation_id.len() == REQUEST_ID_BYTES =>
        {
            context.correlation_id.clone()
        }
        _ if request.request_id.len() == REQUEST_ID_BYTES => request.request_id.clone(),
        _ => vec![0; REQUEST_ID_BYTES],
    };
    let mut response = request.clone();
    response.payload.clear();
    response.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: None,
            receipts: Vec::new(),
            failure: Some(error.to_sabi_failure()),
        },
    ));
    response
}

fn response_envelope(
    request: &Envelope,
    correlation_id: Vec<u8>,
    status: OperationStatus,
) -> Result<Envelope, OperationControlError> {
    let mut response = request.clone();
    response.payload =
        encode_operation_status(&status).map_err(|_| OperationControlError::Encode)?;
    response.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: status.operation,
            receipts: status.receipt.clone().into_iter().collect(),
            failure: None,
        },
    ));
    Ok(response)
}

const fn operation_lifecycle_state(state: OperationState) -> OperationLifecycleState {
    match state {
        OperationState::Registered => OperationLifecycleState::Registered,
        OperationState::Dispatched => OperationLifecycleState::Dispatched,
        OperationState::CancelRequested => OperationLifecycleState::CancelRequested,
        OperationState::Completed { .. } => OperationLifecycleState::Completed,
        OperationState::Failed { .. } => OperationLifecycleState::Failed,
        OperationState::CancelledBeforeEffect { .. } => {
            OperationLifecycleState::CancelledBeforeEffect
        }
        OperationState::PartialEffect { .. } => OperationLifecycleState::PartialEffect,
        OperationState::EffectUnknown { .. } => OperationLifecycleState::EffectUnknown,
    }
}

const fn receipt_from_operation_state(state: OperationState) -> Option<ReceiptId> {
    match state {
        OperationState::Completed { receipt_id }
        | OperationState::Failed { receipt_id }
        | OperationState::CancelledBeforeEffect { receipt_id }
        | OperationState::PartialEffect { receipt_id }
        | OperationState::EffectUnknown { receipt_id } => Some(receipt_id),
        OperationState::Registered
        | OperationState::Dispatched
        | OperationState::CancelRequested => None,
    }
}

fn operation_reference(handle: nlos_operation::OperationHandle) -> OperationReference {
    OperationReference {
        operation_id: handle.operation_id.as_bytes().to_vec(),
        generation: handle.generation.get(),
    }
}

fn operation_handle_from_reference(
    reference: &OperationReference,
) -> Result<OperationHandle, OperationControlError> {
    Ok(OperationHandle {
        operation_id: OperationId::from_bytes(
            reference
                .operation_id
                .clone()
                .try_into()
                .map_err(|_| OperationControlError::InvalidIdentifier)?,
        ),
        generation: Generation::new(
            NonZeroU64::new(reference.generation)
                .ok_or(OperationControlError::InvalidIdentifier)?,
        ),
    })
}

/// Deterministic no-effect cancel receipt, identical to the conformance
/// harness derivation (`nlos.operation-control.cancel-receipt.v1` over the
/// operation id, generation, and expected epoch), so a cancel replayed
/// against either mount names the same durable receipt.
fn stable_cancel_receipt(operation: OperationHandle, expected_cancel_epoch: u64) -> ReceiptId {
    let digest = Sha256::new()
        .chain_update(b"nlos.operation-control.cancel-receipt.v1")
        .chain_update(operation.operation_id.as_bytes())
        .chain_update(operation.generation.get().to_be_bytes())
        .chain_update(expected_cancel_epoch.to_be_bytes())
        .finalize();
    let mut receipt_id = [0_u8; 16];
    receipt_id.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(receipt_id)
}

fn store_error(error: &StoreError) -> OperationControlError {
    match error {
        StoreError::Operation(nlos_operation::OperationError::InvalidGeneration) => {
            OperationControlError::NotFoundOrStale
        }
        StoreError::CancelEpochConflict { .. } | StoreError::IdempotencyConflict => {
            OperationControlError::Conflict
        }
        _ => OperationControlError::Store,
    }
}

fn status_of(snapshot: OperationSnapshot) -> OperationStatus {
    let receipt = receipt_from_operation_state(snapshot.state).map(|receipt_id| ReceiptReference {
        receipt_id: receipt_id.as_bytes().to_vec(),
    });
    OperationStatus {
        schema: Some(operation_control_schema_identity()),
        operation: Some(operation_reference(snapshot.handle)),
        state: i32::from(operation_lifecycle_state(snapshot.state)),
        cancel_epoch: snapshot.cancel_epoch.get(),
        receipt: receipt.clone(),
    }
}

/// Typed `OperationControl` service bound to the daemon's durable operation
/// store: `query` (QUERY) and `cancel` (MUTATION, cancel-epoch fence).
pub struct OperationControlService<'a> {
    store: &'a SqliteOperationStore,
}

impl<'a> OperationControlService<'a> {
    #[must_use]
    pub const fn new(store: &'a SqliteOperationStore) -> Self {
        Self { store }
    }

    /// Handles one validated-envelope-shaped request and always returns a
    /// typed response envelope. `_now_wall_ms` is accepted for handler-surface
    /// parity with the sibling control services; the durable store owns every
    /// timestamp this path stamps.
    #[must_use]
    pub fn handle_for_ipc(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        _now_wall_ms: i64,
    ) -> Envelope {
        match self.handle(request, now_monotonic_ns) {
            Ok(response) => response,
            Err(error) => failure_envelope(request, error),
        }
    }

    fn handle(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, OperationControlError> {
        if request.service != OPERATION_CONTROL_SERVICE {
            return Err(OperationControlError::UnknownMethod);
        }
        let correlation_id = |context: &SabiRequestContext| context.correlation_id.clone();
        match request.method.as_str() {
            QUERY_OPERATION_METHOD => {
                let context = validate_sabi_request_context(
                    request,
                    MethodSemantics::QUERY,
                    now_monotonic_ns,
                )
                .map_err(|_| OperationControlError::InvalidRequest)?;
                let payload = decode_query_operation_request(&request.payload)
                    .map_err(|_| OperationControlError::InvalidRequest)?;
                let handle = payload
                    .operation
                    .as_ref()
                    .ok_or(OperationControlError::InvalidRequest)
                    .and_then(operation_handle_from_reference)?;
                let snapshot = self
                    .store
                    .inspect(handle)
                    .map_err(|error| store_error(&error))?;
                response_envelope(request, correlation_id(context), status_of(snapshot))
            }
            CANCEL_OPERATION_METHOD => {
                let context = validate_sabi_request_context(
                    request,
                    MethodSemantics::MUTATION,
                    now_monotonic_ns,
                )
                .map_err(|_| OperationControlError::InvalidRequest)?;
                let payload = decode_cancel_operation_request(&request.payload)
                    .map_err(|_| OperationControlError::InvalidRequest)?;
                let handle = payload
                    .operation
                    .as_ref()
                    .ok_or(OperationControlError::InvalidRequest)
                    .and_then(operation_handle_from_reference)?;
                let receipt = stable_cancel_receipt(handle, payload.expected_cancel_epoch);
                let snapshot = self
                    .store
                    .request_cancel_idempotent(
                        handle,
                        CancelEpoch::new(payload.expected_cancel_epoch),
                        receipt,
                    )
                    .map_err(|error| store_error(&error))?
                    .snapshot();
                response_envelope(request, correlation_id(context), status_of(snapshot))
            }
            _ => Err(OperationControlError::UnknownMethod),
        }
    }
}
