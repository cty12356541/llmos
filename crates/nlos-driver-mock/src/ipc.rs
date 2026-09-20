//! Typed SABI envelope face for the mock provider.
//!
//! [`MockDriverService`] adapts the shared [`MockProvider`] core onto the
//! common SABI envelope: it validates the request context (all three methods
//! are `MUTATION`), decodes the crate-local deterministic payload, delegates
//! the policy decision to the injected [`MockDriverAuthorizer`] (there is no
//! default allow policy; mere handle presence is not authorization), calls
//! the core, and encodes the authoritative result into an immutable response
//! envelope with authority-derived receipt references.
//!
//! The service never opens a transport itself: the only serving path in this
//! crate is the [`crate::authenticated`] ADR-0011 entry, which threads the
//! verified principal into every authorization decision.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;

use nlos_operation::OperationSpec;
use nlos_runtime::FiberHandle;
use nlos_schema::sabi::v1::{
    Envelope, ReceiptReference, RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext,
    SabiResponseContext, envelope,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, MethodSemantics, REQUEST_ID_BYTES,
    validate_sabi_request_context,
};
use nlos_store::StoreError;
use nlos_types::{
    CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, PrincipalId,
};

use crate::codec::{
    self, CodecError, CompleteOperationResultWire, DispatchOperationResultWire,
    RegisterOperationResultWire,
};
use crate::provider::{CompleteProviderOperation, DispatchProviderOperation, MockProvider};

pub const MOCK_DRIVER_SERVICE: &str = "driver_mock";
pub const REGISTER_OPERATION_METHOD: &str = "register_operation";
pub const DISPATCH_OPERATION_METHOD: &str = "dispatch_operation";
pub const COMPLETE_OPERATION_METHOD: &str = "complete_operation";

/// Policy boundary for the mock driver face. The verified principal is
/// threaded into every decision; implementations validate capability handles
/// against their authority. There is no default allow policy.
pub trait MockDriverAuthorizer {
    /// Authorizes one provider registration.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_register(
        &self,
        principal: PrincipalId,
        context: &SabiRequestContext,
    ) -> Result<(), &'static str>;

    /// Authorizes one provider dispatch.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_dispatch(
        &self,
        principal: PrincipalId,
        context: &SabiRequestContext,
    ) -> Result<(), &'static str>;

    /// Authorizes one provider completion.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_complete(
        &self,
        principal: PrincipalId,
        context: &SabiRequestContext,
    ) -> Result<(), &'static str>;
}

impl<T> MockDriverAuthorizer for &T
where
    T: MockDriverAuthorizer + ?Sized,
{
    fn authorize_register(
        &self,
        principal: PrincipalId,
        context: &SabiRequestContext,
    ) -> Result<(), &'static str> {
        T::authorize_register(self, principal, context)
    }

    fn authorize_dispatch(
        &self,
        principal: PrincipalId,
        context: &SabiRequestContext,
    ) -> Result<(), &'static str> {
        T::authorize_dispatch(self, principal, context)
    }

    fn authorize_complete(
        &self,
        principal: PrincipalId,
        context: &SabiRequestContext,
    ) -> Result<(), &'static str> {
        T::authorize_complete(self, principal, context)
    }
}

#[derive(Debug)]
pub enum MockDriverError {
    /// The request envelope or payload violated the wire contract.
    Payload(CompatibilityError),
    /// The crate-local deterministic payload codec rejected the bytes.
    Codec(CodecError),
    /// The common SABI request context was missing or invalid.
    Common(CommonSemanticsError),
    UnknownMethod,
    AuthorizationDenied(&'static str),
    /// A payload field is outside the bounded contract (identifier width or
    /// a zero generation).
    InvalidRequest,
    /// The durable operation authority rejected the request.
    Store(StoreError),
}

impl fmt::Display for MockDriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payload(error) => write!(formatter, "invalid mock driver envelope: {error}"),
            Self::Codec(error) => write!(formatter, "invalid mock driver payload: {error}"),
            Self::Common(error) => write!(formatter, "invalid mock driver context: {error}"),
            Self::UnknownMethod => formatter.write_str("unknown mock driver service or method"),
            Self::AuthorizationDenied(reason) => {
                write!(formatter, "mock driver authorization denied: {reason}")
            }
            Self::InvalidRequest => {
                formatter.write_str("mock driver request field is out of contract")
            }
            Self::Store(error) => {
                write!(
                    formatter,
                    "operation authority rejected the driver: {error}"
                )
            }
        }
    }
}

impl Error for MockDriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Payload(error) => Some(error),
            Self::Codec(error) => Some(error),
            Self::Common(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::UnknownMethod | Self::AuthorizationDenied(_) | Self::InvalidRequest => None,
        }
    }
}

impl From<CompatibilityError> for MockDriverError {
    fn from(error: CompatibilityError) -> Self {
        Self::Payload(error)
    }
}

impl From<CodecError> for MockDriverError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

impl From<CommonSemanticsError> for MockDriverError {
    fn from(error: CommonSemanticsError) -> Self {
        Self::Common(error)
    }
}

impl From<StoreError> for MockDriverError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl MockDriverError {
    /// Maps one local rejection to the bounded common SABI failure class.
    /// The mapping never includes the source error's display text: `SQLite`
    /// messages, static policy reasons, and durable-record details are local
    /// diagnostics and must not cross the driver boundary.
    #[must_use]
    pub fn to_sabi_failure(&self) -> SabiFailure {
        let (code, retry, safe_message) = match self {
            Self::Payload(_) | Self::Codec(_) | Self::InvalidRequest => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the mock driver payload contract",
            ),
            Self::Common(CommonSemanticsError::DeadlineExpired) => (
                SabiErrorCode::Deadline,
                RetryDirective::DoNotRetry,
                "call deadline has expired",
            ),
            Self::Common(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the common SABI contract",
            ),
            Self::UnknownMethod => (
                SabiErrorCode::NotSupported,
                RetryDirective::DoNotRetry,
                "unknown mock driver service or method",
            ),
            Self::AuthorizationDenied(_) => (
                SabiErrorCode::Rights,
                RetryDirective::DoNotRetry,
                "mock driver authorization denied",
            ),
            Self::Store(error) => store_failure(error),
        };
        SabiFailure {
            code: code.into(),
            retry: retry.into(),
            safe_message: safe_message.to_owned(),
        }
    }
}

fn store_failure(error: &StoreError) -> (SabiErrorCode, RetryDirective, &'static str) {
    use nlos_operation::OperationError as Operation;
    use nlos_store::StoreError as Store;

    match error {
        Store::Sqlite(_) => (
            SabiErrorCode::Durability,
            RetryDirective::RetrySameIdempotencyKey,
            "operation authority storage failure; retry with the same request bytes",
        ),
        Store::DurabilityUnavailable { .. } => (
            SabiErrorCode::Durability,
            RetryDirective::DoNotRetry,
            "operation authority durability configuration is unavailable",
        ),
        Store::UnsupportedSchema(_) => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "operation authority schema version is unsupported",
        ),
        Store::OutboxEntryNotFound
        | Store::InvalidIdempotencyScope
        | Store::IdempotencyRecordNotFound
        | Store::DurableResultTooLarge { .. } => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "mock driver request hit an operation authority path it never issues",
        ),
        Store::IdempotencyConflict => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "idempotency key conflicts with durable operation state",
        ),
        Store::DispatchPreparationNotFound => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "operation has no durable dispatch preparation",
        ),
        Store::DispatchPreparationConflict | Store::OperationNotActivated => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "dispatch preparation conflicts with the durable owner-bound request",
        ),
        Store::CancelEpochConflict { .. } => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "operation cancel epoch conflicts with durable state",
        ),
        Store::Operation(Operation::DuplicateOperation) => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "operation id was reused for a different specification",
        ),
        Store::Operation(Operation::InvalidGeneration) => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "operation generation is stale or the callback identity is unknown",
        ),
        Store::Operation(Operation::InvalidState) => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "operation state does not allow this transition",
        ),
        Store::Operation(Operation::CancelEpochExhausted) => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "operation cancellation epoch is exhausted",
        ),
        Store::Operation(Operation::CallbackIdentityConflict) => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "callback identity was reused with different completion data",
        ),
        Store::CorruptRecord(_) | Store::LockPoisoned => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "local operation authority defect; do not retry",
        ),
    }
}

/// Typed mock driver service bound to one shared [`MockProvider`] core.
pub struct MockDriverService<A> {
    provider: Arc<MockProvider>,
    authorizer: A,
}

impl<A> MockDriverService<A>
where
    A: MockDriverAuthorizer,
{
    #[must_use]
    pub const fn new(provider: Arc<MockProvider>, authorizer: A) -> Self {
        Self {
            provider,
            authorizer,
        }
    }

    /// Handles one validated-envelope-shaped request for the verified
    /// `principal`. The returned envelope retains the request id.
    ///
    /// # Errors
    ///
    /// Returns typed payload/context/policy/authority errors. A failed
    /// request never manufactures success evidence.
    pub fn handle(
        &self,
        request: &Envelope,
        principal: PrincipalId,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, MockDriverError> {
        if request.service != MOCK_DRIVER_SERVICE {
            return Err(MockDriverError::UnknownMethod);
        }
        match request.method.as_str() {
            REGISTER_OPERATION_METHOD => self.handle_register(request, principal, now_monotonic_ns),
            DISPATCH_OPERATION_METHOD => self.handle_dispatch(request, principal, now_monotonic_ns),
            COMPLETE_OPERATION_METHOD => self.handle_complete(request, principal, now_monotonic_ns),
            _ => Err(MockDriverError::UnknownMethod),
        }
    }

    /// [`Self::handle`] for a local IPC adapter: typed failures become the
    /// bounded failure envelope instead.
    #[must_use]
    pub fn handle_for_ipc(
        &self,
        request: &Envelope,
        principal: PrincipalId,
        now_monotonic_ns: u64,
    ) -> Envelope {
        match self.handle(request, principal, now_monotonic_ns) {
            Ok(response) => response,
            Err(error) => failure_envelope(request, &error),
        }
    }

    fn handle_register(
        &self,
        request: &Envelope,
        principal: PrincipalId,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, MockDriverError> {
        let context =
            validate_sabi_request_context(request, MethodSemantics::MUTATION, now_monotonic_ns)?;
        let payload = codec::decode_register_request(&request.payload)?;
        self.authorizer
            .authorize_register(principal, context)
            .map_err(MockDriverError::AuthorizationDenied)?;
        let spec = OperationSpec {
            operation_id: OperationId::from_bytes(payload.operation_id),
            generation: generation(payload.operation_generation)?,
            owner_fiber: FiberHandle {
                fiber_id: ExecutionFiberId::from_bytes(payload.owner_fiber_id),
                generation: generation(payload.owner_fiber_generation)?,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes(payload.cancellation_scope_id),
            cancellation_generation: generation(payload.cancellation_generation)?,
        };
        let registered = self.provider.register(spec)?;
        let result = RegisterOperationResultWire {
            replayed: registered.replayed,
            operation_id: id16(registered.handle.operation_id.as_bytes())?,
            operation_generation: registered.handle.generation.get(),
            admission_receipt_id: id16(registered.admission_receipt_id.as_bytes())?,
        };
        let receipts = vec![ReceiptReference {
            receipt_id: registered.admission_receipt_id.as_bytes().to_vec(),
        }];
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            codec::encode_register_result(&result)?,
            receipts,
        ))
    }

    fn handle_dispatch(
        &self,
        request: &Envelope,
        principal: PrincipalId,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, MockDriverError> {
        let context =
            validate_sabi_request_context(request, MethodSemantics::MUTATION, now_monotonic_ns)?;
        let payload = codec::decode_dispatch_request(&request.payload)?;
        self.authorizer
            .authorize_dispatch(principal, context)
            .map_err(MockDriverError::AuthorizationDenied)?;
        let dispatched = self.provider.dispatch(DispatchProviderOperation {
            handle: nlos_operation::OperationHandle {
                operation_id: OperationId::from_bytes(payload.operation_id),
                generation: generation(payload.operation_generation)?,
            },
            callback_id: CallbackId::from_bytes(payload.callback_id),
        })?;
        let result = DispatchOperationResultWire {
            replayed: dispatched.replayed,
            preparation_receipt_id: id16(dispatched.preparation_receipt_id.as_bytes())?,
            activation_receipt_id: id16(dispatched.activation_receipt_id.as_bytes())?,
            callback_id: id16(dispatched.ticket.callback_id.as_bytes())?,
            operation_id: id16(dispatched.ticket.operation.operation_id.as_bytes())?,
            operation_generation: dispatched.ticket.operation.generation.get(),
            owner_fiber_id: id16(dispatched.ticket.owner_fiber.fiber_id.as_bytes())?,
            owner_fiber_generation: dispatched.ticket.owner_fiber.generation.get(),
            cancel_epoch: dispatched.ticket.cancel_epoch.get(),
        };
        let receipts = vec![
            ReceiptReference {
                receipt_id: dispatched.preparation_receipt_id.as_bytes().to_vec(),
            },
            ReceiptReference {
                receipt_id: dispatched.activation_receipt_id.as_bytes().to_vec(),
            },
        ];
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            codec::encode_dispatch_result(&result)?,
            receipts,
        ))
    }

    fn handle_complete(
        &self,
        request: &Envelope,
        principal: PrincipalId,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, MockDriverError> {
        let context =
            validate_sabi_request_context(request, MethodSemantics::MUTATION, now_monotonic_ns)?;
        let payload = codec::decode_complete_request(&request.payload)?;
        self.authorizer
            .authorize_complete(principal, context)
            .map_err(MockDriverError::AuthorizationDenied)?;
        let completion = self.provider.complete(CompleteProviderOperation {
            handle: nlos_operation::OperationHandle {
                operation_id: OperationId::from_bytes(payload.operation_id),
                generation: generation(payload.operation_generation)?,
            },
            callback_id: CallbackId::from_bytes(payload.callback_id),
            seed: payload.seed,
        })?;
        let receipt_id = match completion.outcome {
            nlos_operation::CompletionOutcome::Completed { receipt_id }
            | nlos_operation::CompletionOutcome::Failed { receipt_id }
            | nlos_operation::CompletionOutcome::CancelledBeforeEffect { receipt_id }
            | nlos_operation::CompletionOutcome::PartialEffect { receipt_id }
            | nlos_operation::CompletionOutcome::EffectUnknown { receipt_id } => receipt_id,
        };
        let result = CompleteOperationResultWire {
            replayed: completion.replayed,
            outcome_code: codec::outcome_wire_code(completion.outcome),
            receipt_id: id16(receipt_id.as_bytes())?,
            state_code: codec::state_wire_code(completion.state),
        };
        let receipts = vec![ReceiptReference {
            receipt_id: receipt_id.as_bytes().to_vec(),
        }];
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            codec::encode_complete_result(&result)?,
            receipts,
        ))
    }
}

fn generation(value: u64) -> Result<Generation, MockDriverError> {
    NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(MockDriverError::InvalidRequest)
}

fn id16(bytes: &[u8]) -> Result<[u8; 16], MockDriverError> {
    bytes
        .try_into()
        .map_err(|_| MockDriverError::InvalidRequest)
}

/// Builds a typed failure envelope for one rejected request, mirroring the
/// sibling control services: the request id and service/method are retained,
/// the payload and all Operation/Receipt evidence are cleared.
#[must_use]
pub fn failure_envelope(request: &Envelope, error: &MockDriverError) -> Envelope {
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
    payload: Vec<u8>,
    receipts: Vec<ReceiptReference>,
) -> Envelope {
    let mut response = request.clone();
    response.payload = payload;
    response.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: None,
            receipts,
            failure: None,
        },
    ));
    response
}
