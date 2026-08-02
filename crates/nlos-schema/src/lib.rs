//! Generated NLOS wire schemas and fail-closed compatibility validation.
//!
//! Consumers may inspect the generated typed message, but forwarding code must
//! use [`ValidatedFrame::wire_bytes`] so protobuf fields unknown to this build
//! are not lost through decode/re-encode.

use std::error::Error;
use std::fmt;

use prost::Message;

pub mod sabi {
    pub mod v1 {
        #![allow(clippy::doc_markdown, clippy::must_use_candidate)]
        include!(concat!(env!("OUT_DIR"), "/nlos.sabi.v1.rs"));
    }
}

pub const SABI_ENVELOPE_SCHEMA: &str = "nlos.sabi.Envelope";
pub const SABI_SERVICE_DIRECTORY_SCHEMA: &str = "nlos.sabi.ServiceDirectory";
pub const SABI_OPERATION_CONTROL_SCHEMA: &str = "nlos.sabi.OperationControl";
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;
pub const MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_OPERATION_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;
pub const REQUEST_ID_BYTES: usize = 16;
pub const SHA256_DIGEST_BYTES: usize = 32;
pub const MAX_ACTIVITY_CONTEXT_BYTES: usize = 4 * 1024;
pub const MAX_CAPABILITY_HANDLES: usize = 64;
pub const MAX_RECEIPT_REFERENCES: usize = 64;
pub const MAX_SAFE_ERROR_MESSAGE_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaDescriptor {
    pub name: &'static str,
    pub major: u32,
    pub minor: u32,
    pub supported_critical_extensions: &'static [u32],
}

const SABI_ENVELOPE_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_ENVELOPE_SCHEMA,
    major: 1,
    minor: 1,
    supported_critical_extensions: &[],
};

const SABI_SERVICE_DIRECTORY_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_SERVICE_DIRECTORY_SCHEMA,
    major: 1,
    minor: 0,
    supported_critical_extensions: &[],
};

const SABI_OPERATION_CONTROL_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_OPERATION_CONTROL_SCHEMA,
    major: 1,
    minor: 0,
    supported_critical_extensions: &[],
};

const REGISTRY: &[SchemaDescriptor] = &[
    SABI_ENVELOPE_V1,
    SABI_SERVICE_DIRECTORY_V1,
    SABI_OPERATION_CONTROL_V1,
];

#[must_use]
pub fn schema_registry() -> &'static [SchemaDescriptor] {
    REGISTRY
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompatibilityError {
    FrameTooLarge {
        actual: usize,
        maximum: usize,
    },
    MalformedProtobuf(String),
    MissingSchemaIdentity,
    MissingExchangeEnvelope,
    MissingServiceDirectoryResult,
    MissingOperationReference,
    InvalidReceiptReference,
    UnspecifiedOperationState,
    UnknownSchema(String),
    UnsupportedMajor {
        schema: String,
        got: u32,
        supported: u32,
    },
    UnsupportedCriticalExtension {
        schema: String,
        extension_id: u32,
    },
    InvalidRequestIdLength {
        actual: usize,
    },
    EmptyService,
    EmptyMethod,
}

impl fmt::Display for CompatibilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge { actual, maximum } => {
                write!(formatter, "frame has {actual} bytes; maximum is {maximum}")
            }
            Self::MalformedProtobuf(message) => write!(formatter, "malformed protobuf: {message}"),
            Self::MissingSchemaIdentity => formatter.write_str("schema identity is missing"),
            Self::MissingExchangeEnvelope => {
                formatter.write_str("exchange wrapper is missing its envelope")
            }
            Self::MissingServiceDirectoryResult => {
                formatter.write_str("service directory response is missing its result")
            }
            Self::MissingOperationReference => {
                formatter.write_str("operation control payload is missing its operation")
            }
            Self::InvalidReceiptReference => {
                formatter.write_str("operation control receipt reference is malformed")
            }
            Self::UnspecifiedOperationState => {
                formatter.write_str("operation control status has unspecified state")
            }
            Self::UnknownSchema(schema) => write!(formatter, "schema {schema:?} is not registered"),
            Self::UnsupportedMajor {
                schema,
                got,
                supported,
            } => write!(
                formatter,
                "schema {schema:?} major {got} is unsupported; this build supports {supported}"
            ),
            Self::UnsupportedCriticalExtension {
                schema,
                extension_id,
            } => write!(
                formatter,
                "schema {schema:?} requires unsupported critical extension {extension_id}"
            ),
            Self::InvalidRequestIdLength { actual } => write!(
                formatter,
                "request_id has {actual} bytes; exactly {REQUEST_ID_BYTES} are required"
            ),
            Self::EmptyService => formatter.write_str("service must not be empty"),
            Self::EmptyMethod => formatter.write_str("method must not be empty"),
        }
    }
}

impl Error for CompatibilityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MethodSemantics {
    pub side_effecting: bool,
    pub long_running: bool,
}

impl MethodSemantics {
    pub const QUERY: Self = Self {
        side_effecting: false,
        long_running: false,
    };
    pub const LONG_RUNNING_QUERY: Self = Self {
        side_effecting: false,
        long_running: true,
    };
    pub const MUTATION: Self = Self {
        side_effecting: true,
        long_running: false,
    };
    pub const LONG_RUNNING_MUTATION: Self = Self {
        side_effecting: true,
        long_running: true,
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommonSemanticsError {
    MissingRequestContext,
    MissingResponseContext,
    MissingCallerIdentity,
    InvalidIdentifierLength { field: &'static str, actual: usize },
    ZeroGeneration(&'static str),
    MissingIdempotencyKey,
    MissingDeadline,
    DeadlineExpired,
    ActivityContextTooLarge { actual: usize, maximum: usize },
    TooManyCapabilityHandles { actual: usize, maximum: usize },
    DuplicateCapabilityHandle,
    InvalidProposalDigestLength { actual: usize },
    TooManyReceiptReferences { actual: usize, maximum: usize },
    DuplicateReceiptReference,
    UnknownErrorCode(i32),
    UnspecifiedErrorCode,
    UnknownRetryDirective(i32),
    UnspecifiedRetryDirective,
    InvalidRetryDirective,
    MissingEffectEvidence,
    MissingOperationForUncertainOutcome,
    MissingReceiptForPartialOutcome,
    UnsafeErrorMessage,
}

impl fmt::Display for CommonSemanticsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequestContext => formatter.write_str("SABI request context is missing"),
            Self::MissingResponseContext => formatter.write_str("SABI response context is missing"),
            Self::MissingCallerIdentity => formatter.write_str("caller identity is missing"),
            Self::InvalidIdentifierLength { field, actual } => write!(
                formatter,
                "{field} has {actual} bytes; exactly {REQUEST_ID_BYTES} are required"
            ),
            Self::ZeroGeneration(field) => write!(formatter, "{field} generation must be non-zero"),
            Self::MissingIdempotencyKey => {
                formatter.write_str("side-effecting call requires a 128-bit idempotency key")
            }
            Self::MissingDeadline => {
                formatter.write_str("long-running call requires a monotonic deadline")
            }
            Self::DeadlineExpired => formatter.write_str("call deadline has expired"),
            Self::ActivityContextTooLarge { actual, maximum } => write!(
                formatter,
                "activity context has {actual} bytes; maximum is {maximum}"
            ),
            Self::TooManyCapabilityHandles { actual, maximum } => write!(
                formatter,
                "request has {actual} capability handles; maximum is {maximum}"
            ),
            Self::DuplicateCapabilityHandle => {
                formatter.write_str("request contains a duplicate capability handle")
            }
            Self::InvalidProposalDigestLength { actual } => write!(
                formatter,
                "proposal/input digest has {actual} bytes; exactly {SHA256_DIGEST_BYTES} are required"
            ),
            Self::TooManyReceiptReferences { actual, maximum } => write!(
                formatter,
                "response has {actual} receipt references; maximum is {maximum}"
            ),
            Self::DuplicateReceiptReference => {
                formatter.write_str("response contains a duplicate receipt reference")
            }
            Self::UnknownErrorCode(code) => write!(formatter, "unknown SABI error code {code}"),
            Self::UnspecifiedErrorCode => {
                formatter.write_str("SABI failure uses the unspecified error code")
            }
            Self::UnknownRetryDirective(directive) => {
                write!(formatter, "unknown SABI retry directive {directive}")
            }
            Self::UnspecifiedRetryDirective => {
                formatter.write_str("SABI failure uses the unspecified retry directive")
            }
            Self::InvalidRetryDirective => formatter
                .write_str("SABI failure retry directive is incompatible with its error code"),
            Self::MissingEffectEvidence => formatter
                .write_str("side-effecting response requires an Operation or Receipt reference"),
            Self::MissingOperationForUncertainOutcome => formatter
                .write_str("uncertain/effect-unknown outcome requires an Operation reference"),
            Self::MissingReceiptForPartialOutcome => {
                formatter.write_str("partial outcome requires at least one Receipt reference")
            }
            Self::UnsafeErrorMessage => {
                formatter.write_str("safe error message is oversized or contains a NUL character")
            }
        }
    }
}

impl Error for CommonSemanticsError {}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedFrame {
    envelope: sabi::v1::Envelope,
    wire: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedExchangeRequest {
    request: sabi::v1::ExchangeRequest,
    wire: Vec<u8>,
}

impl ValidatedExchangeRequest {
    #[must_use]
    pub const fn request(&self) -> &sabi::v1::ExchangeRequest {
        &self.request
    }

    #[must_use]
    /// Returns the validated nested envelope.
    ///
    /// # Panics
    ///
    /// This only panics if the private validated value was constructed without
    /// passing through this crate's decoder.
    pub fn envelope(&self) -> &sabi::v1::Envelope {
        self.request
            .envelope
            .as_ref()
            .expect("validated exchange request always has an envelope")
    }

    #[must_use]
    pub fn wire_bytes(&self) -> &[u8] {
        &self.wire
    }

    #[must_use]
    pub fn into_wire_bytes(self) -> Vec<u8> {
        self.wire
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedExchangeResponse {
    response: sabi::v1::ExchangeResponse,
    wire: Vec<u8>,
}

impl ValidatedExchangeResponse {
    #[must_use]
    pub const fn response(&self) -> &sabi::v1::ExchangeResponse {
        &self.response
    }

    #[must_use]
    /// Returns the validated nested envelope.
    ///
    /// # Panics
    ///
    /// This only panics if the private validated value was constructed without
    /// passing through this crate's decoder.
    pub fn envelope(&self) -> &sabi::v1::Envelope {
        self.response
            .envelope
            .as_ref()
            .expect("validated exchange response always has an envelope")
    }

    #[must_use]
    pub fn wire_bytes(&self) -> &[u8] {
        &self.wire
    }

    #[must_use]
    pub fn into_wire_bytes(self) -> Vec<u8> {
        self.wire
    }
}

impl ValidatedFrame {
    #[must_use]
    pub const fn envelope(&self) -> &sabi::v1::Envelope {
        &self.envelope
    }

    /// Returns the exact input frame, including protobuf fields unknown to the
    /// generated Rust type in this build.
    #[must_use]
    pub fn wire_bytes(&self) -> &[u8] {
        &self.wire
    }

    #[must_use]
    pub fn into_wire_bytes(self) -> Vec<u8> {
        self.wire
    }
}

/// Decodes and validates a SABI envelope against the local schema registry.
///
/// # Errors
///
/// Returns [`CompatibilityError`] when the frame is malformed, exceeds the
/// size bound, names an unsupported schema/major/critical extension, or fails
/// the common envelope invariants.
pub fn decode_sabi_envelope(wire: &[u8]) -> Result<ValidatedFrame, CompatibilityError> {
    if wire.len() > MAX_ENVELOPE_BYTES {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum: MAX_ENVELOPE_BYTES,
        });
    }

    let envelope = sabi::v1::Envelope::decode(wire)
        .map_err(|error| CompatibilityError::MalformedProtobuf(error.to_string()))?;
    validate_envelope(&envelope)?;
    Ok(ValidatedFrame {
        envelope,
        wire: wire.to_vec(),
    })
}

/// Validates and encodes a locally constructed SABI envelope.
///
/// # Errors
///
/// Returns [`CompatibilityError`] when the envelope is incompatible with the
/// local registry, violates common invariants, or exceeds the frame size bound.
pub fn encode_sabi_envelope(envelope: &sabi::v1::Envelope) -> Result<Vec<u8>, CompatibilityError> {
    validate_envelope(envelope)?;
    let wire = envelope.encode_to_vec();
    if wire.len() > MAX_ENVELOPE_BYTES {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum: MAX_ENVELOPE_BYTES,
        });
    }
    Ok(wire)
}

/// Validates and encodes a local unary RPC request wrapper.
///
/// # Errors
///
/// Returns [`CompatibilityError`] when the nested envelope is absent,
/// incompatible, or the complete wrapper exceeds the frame bound.
pub fn encode_exchange_request(
    request: &sabi::v1::ExchangeRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    let envelope = request
        .envelope
        .as_ref()
        .ok_or(CompatibilityError::MissingExchangeEnvelope)?;
    validate_envelope(envelope)?;
    encode_bounded(request)
}

/// Decodes a local unary RPC request while preserving its exact wire bytes.
///
/// # Errors
///
/// Returns [`CompatibilityError`] for malformed, oversized, missing, or
/// incompatible input.
pub fn decode_exchange_request(
    wire: &[u8],
) -> Result<ValidatedExchangeRequest, CompatibilityError> {
    let request: sabi::v1::ExchangeRequest = decode_bounded(wire)?;
    let envelope = request
        .envelope
        .as_ref()
        .ok_or(CompatibilityError::MissingExchangeEnvelope)?;
    validate_envelope(envelope)?;
    Ok(ValidatedExchangeRequest {
        request,
        wire: wire.to_vec(),
    })
}

/// Validates and encodes a local unary RPC response wrapper.
///
/// # Errors
///
/// Returns [`CompatibilityError`] when the nested envelope is absent,
/// incompatible, or the complete wrapper exceeds the frame bound.
pub fn encode_exchange_response(
    response: &sabi::v1::ExchangeResponse,
) -> Result<Vec<u8>, CompatibilityError> {
    let envelope = response
        .envelope
        .as_ref()
        .ok_or(CompatibilityError::MissingExchangeEnvelope)?;
    validate_envelope(envelope)?;
    encode_bounded(response)
}

/// Decodes a local unary RPC response while preserving its exact wire bytes.
///
/// # Errors
///
/// Returns [`CompatibilityError`] for malformed, oversized, missing, or
/// incompatible input.
pub fn decode_exchange_response(
    wire: &[u8],
) -> Result<ValidatedExchangeResponse, CompatibilityError> {
    let response: sabi::v1::ExchangeResponse = decode_bounded(wire)?;
    let envelope = response
        .envelope
        .as_ref()
        .ok_or(CompatibilityError::MissingExchangeEnvelope)?;
    validate_envelope(envelope)?;
    Ok(ValidatedExchangeResponse {
        response,
        wire: wire.to_vec(),
    })
}

/// Returns the v1 identity required on every `ServiceDirectory` payload.
#[must_use]
pub fn service_directory_schema_identity() -> sabi::v1::SchemaIdentity {
    sabi::v1::SchemaIdentity {
        name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
        major: 1,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}

/// Returns the v1 identity required on every `OperationControl` payload.
#[must_use]
pub fn operation_control_schema_identity() -> sabi::v1::SchemaIdentity {
    sabi::v1::SchemaIdentity {
        name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}

/// Encodes a bounded, validated Operation query payload.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity/reference or oversized payload.
pub fn encode_query_operation_request(
    request: &sabi::v1::QueryOperationRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_operation_control_identity(request.schema.as_ref())?;
    validate_operation_reference(request.operation.as_ref())?;
    encode_bounded_with_limit(request, MAX_OPERATION_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded, validated Operation query payload.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_query_operation_request(
    wire: &[u8],
) -> Result<sabi::v1::QueryOperationRequest, CompatibilityError> {
    let request: sabi::v1::QueryOperationRequest =
        decode_bounded_with_limit(wire, MAX_OPERATION_CONTROL_PAYLOAD_BYTES)?;
    validate_operation_control_identity(request.schema.as_ref())?;
    validate_operation_reference(request.operation.as_ref())?;
    Ok(request)
}

/// Encodes a bounded, validated idempotent Operation cancellation payload.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity/reference or oversized payload.
pub fn encode_cancel_operation_request(
    request: &sabi::v1::CancelOperationRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_operation_control_identity(request.schema.as_ref())?;
    validate_operation_reference(request.operation.as_ref())?;
    encode_bounded_with_limit(request, MAX_OPERATION_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded, validated idempotent Operation cancellation payload.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_cancel_operation_request(
    wire: &[u8],
) -> Result<sabi::v1::CancelOperationRequest, CompatibilityError> {
    let request: sabi::v1::CancelOperationRequest =
        decode_bounded_with_limit(wire, MAX_OPERATION_CONTROL_PAYLOAD_BYTES)?;
    validate_operation_control_identity(request.schema.as_ref())?;
    validate_operation_reference(request.operation.as_ref())?;
    Ok(request)
}

/// Encodes a bounded, validated durable Operation status payload.
///
/// # Errors
///
/// Returns a compatibility error for an invalid state/reference or oversized payload.
pub fn encode_operation_status(
    status: &sabi::v1::OperationStatus,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_operation_control_identity(status.schema.as_ref())?;
    validate_operation_reference(status.operation.as_ref())?;
    let state = sabi::v1::OperationLifecycleState::try_from(status.state)
        .map_err(|_| CompatibilityError::UnspecifiedOperationState)?;
    if state == sabi::v1::OperationLifecycleState::Unspecified {
        return Err(CompatibilityError::UnspecifiedOperationState);
    }
    if let Some(receipt) = status.receipt.as_ref()
        && receipt.receipt_id.len() != REQUEST_ID_BYTES
    {
        return Err(CompatibilityError::InvalidReceiptReference);
    }
    encode_bounded_with_limit(status, MAX_OPERATION_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded, validated durable Operation status payload.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_operation_status(
    wire: &[u8],
) -> Result<sabi::v1::OperationStatus, CompatibilityError> {
    let status: sabi::v1::OperationStatus =
        decode_bounded_with_limit(wire, MAX_OPERATION_CONTROL_PAYLOAD_BYTES)?;
    validate_operation_control_identity(status.schema.as_ref())?;
    validate_operation_reference(status.operation.as_ref())?;
    let state = sabi::v1::OperationLifecycleState::try_from(status.state)
        .map_err(|_| CompatibilityError::UnspecifiedOperationState)?;
    if state == sabi::v1::OperationLifecycleState::Unspecified {
        return Err(CompatibilityError::UnspecifiedOperationState);
    }
    if status
        .receipt
        .as_ref()
        .is_some_and(|receipt| receipt.receipt_id.len() != REQUEST_ID_BYTES)
    {
        return Err(CompatibilityError::InvalidReceiptReference);
    }
    Ok(status)
}

/// Validates and encodes a bounded `ServiceDirectory` resolve request.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_resolve_service_request(
    request: &sabi::v1::ResolveServiceRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_service_directory_identity(request.schema.as_ref())?;
    encode_bounded_with_limit(request, MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES)
}

/// Decodes a bounded `ServiceDirectory` resolve request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_resolve_service_request(
    wire: &[u8],
) -> Result<sabi::v1::ResolveServiceRequest, CompatibilityError> {
    let request: sabi::v1::ResolveServiceRequest =
        decode_bounded_with_limit(wire, MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES)?;
    validate_service_directory_identity(request.schema.as_ref())?;
    Ok(request)
}

/// Validates and encodes a bounded `ServiceDirectory` resolve response.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity, missing result, or bound violation.
pub fn encode_resolve_service_response(
    response: &sabi::v1::ResolveServiceResponse,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_service_directory_identity(response.schema.as_ref())?;
    if response.result.is_none() {
        return Err(CompatibilityError::MissingServiceDirectoryResult);
    }
    encode_bounded_with_limit(response, MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES)
}

/// Decodes a bounded `ServiceDirectory` resolve response.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, incomplete, or oversized input.
pub fn decode_resolve_service_response(
    wire: &[u8],
) -> Result<sabi::v1::ResolveServiceResponse, CompatibilityError> {
    let response: sabi::v1::ResolveServiceResponse =
        decode_bounded_with_limit(wire, MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES)?;
    validate_service_directory_identity(response.schema.as_ref())?;
    if response.result.is_none() {
        return Err(CompatibilityError::MissingServiceDirectoryResult);
    }
    Ok(response)
}

/// Validates and encodes a bounded `ServiceDirectory` negotiation request.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_negotiate_service_request(
    request: &sabi::v1::NegotiateServiceRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_service_directory_identity(request.schema.as_ref())?;
    encode_bounded_with_limit(request, MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES)
}

/// Decodes a bounded `ServiceDirectory` negotiation request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_negotiate_service_request(
    wire: &[u8],
) -> Result<sabi::v1::NegotiateServiceRequest, CompatibilityError> {
    let request: sabi::v1::NegotiateServiceRequest =
        decode_bounded_with_limit(wire, MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES)?;
    validate_service_directory_identity(request.schema.as_ref())?;
    Ok(request)
}

/// Validates and encodes a bounded `ServiceDirectory` negotiation response.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity, missing result, or bound violation.
pub fn encode_negotiate_service_response(
    response: &sabi::v1::NegotiateServiceResponse,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_service_directory_identity(response.schema.as_ref())?;
    if response.result.is_none() {
        return Err(CompatibilityError::MissingServiceDirectoryResult);
    }
    encode_bounded_with_limit(response, MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES)
}

/// Decodes a bounded `ServiceDirectory` negotiation response.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, incomplete, or oversized input.
pub fn decode_negotiate_service_response(
    wire: &[u8],
) -> Result<sabi::v1::NegotiateServiceResponse, CompatibilityError> {
    let response: sabi::v1::NegotiateServiceResponse =
        decode_bounded_with_limit(wire, MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES)?;
    validate_service_directory_identity(response.schema.as_ref())?;
    if response.result.is_none() {
        return Err(CompatibilityError::MissingServiceDirectoryResult);
    }
    Ok(response)
}

/// Validates common request metadata against the negotiated method contract.
///
/// `now_monotonic_ns` must use the same host monotonic clock domain as the
/// supplied deadline. A zero value is valid for tests that omit a deadline.
///
/// # Errors
///
/// Returns a fail-closed error for missing identity/fences, malformed IDs,
/// missing mutation idempotency, missing/expired long-running deadline, or
/// unbounded/duplicate authority references.
pub fn validate_sabi_request_context(
    envelope: &sabi::v1::Envelope,
    semantics: MethodSemantics,
    now_monotonic_ns: u64,
) -> Result<&sabi::v1::SabiRequestContext, CommonSemanticsError> {
    let Some(sabi::v1::envelope::CommonContext::RequestContext(context)) =
        envelope.common_context.as_ref()
    else {
        return Err(CommonSemanticsError::MissingRequestContext);
    };
    let caller = context
        .caller
        .as_ref()
        .ok_or(CommonSemanticsError::MissingCallerIdentity)?;
    validate_id("principal_id", &caller.principal_id)?;
    validate_id("application_id", &caller.application_id)?;
    validate_id("process_id", &caller.process_id)?;
    validate_generation("process", caller.process_generation)?;
    validate_id("correlation_id", &context.correlation_id)?;

    if context.idempotency_key.is_empty() {
        if semantics.side_effecting {
            return Err(CommonSemanticsError::MissingIdempotencyKey);
        }
    } else {
        validate_id("idempotency_key", &context.idempotency_key)?;
    }

    if context.deadline_monotonic_ns == 0 {
        if semantics.long_running {
            return Err(CommonSemanticsError::MissingDeadline);
        }
    } else if context.deadline_monotonic_ns <= now_monotonic_ns {
        return Err(CommonSemanticsError::DeadlineExpired);
    }

    if context.activity_context.len() > MAX_ACTIVITY_CONTEXT_BYTES {
        return Err(CommonSemanticsError::ActivityContextTooLarge {
            actual: context.activity_context.len(),
            maximum: MAX_ACTIVITY_CONTEXT_BYTES,
        });
    }
    if let Some(binding) = context.task_execution_binding.as_ref() {
        validate_id("task_attempt_id", &binding.task_attempt_id)?;
        validate_generation("task_authority_term", binding.task_authority_term)?;
        validate_generation("isolation_domain", binding.isolation_domain_generation)?;
    }
    validate_capabilities(&context.capability_handles)?;
    if let Some(handle) = context.reservation_handle.as_ref() {
        validate_capability(handle)?;
        if context.capability_handles.contains(handle) {
            return Err(CommonSemanticsError::DuplicateCapabilityHandle);
        }
    }
    if !context.proposal_or_input_digest_sha256.is_empty()
        && context.proposal_or_input_digest_sha256.len() != SHA256_DIGEST_BYTES
    {
        return Err(CommonSemanticsError::InvalidProposalDigestLength {
            actual: context.proposal_or_input_digest_sha256.len(),
        });
    }
    Ok(context)
}

/// Validates common response metadata and retry safety.
///
/// # Errors
///
/// Returns a fail-closed error for malformed references, unknown common error
/// values, unsafe retry instructions, or uncertain/partial outcomes without
/// the Operation/Receipt evidence required for reconciliation.
pub fn validate_sabi_response_context(
    envelope: &sabi::v1::Envelope,
    semantics: MethodSemantics,
) -> Result<&sabi::v1::SabiResponseContext, CommonSemanticsError> {
    let Some(sabi::v1::envelope::CommonContext::ResponseContext(context)) =
        envelope.common_context.as_ref()
    else {
        return Err(CommonSemanticsError::MissingResponseContext);
    };
    validate_id("correlation_id", &context.correlation_id)?;
    if let Some(operation) = context.operation.as_ref() {
        validate_operation(operation)?;
    }
    validate_receipts(&context.receipts)?;
    if semantics.side_effecting && context.operation.is_none() && context.receipts.is_empty() {
        return Err(CommonSemanticsError::MissingEffectEvidence);
    }

    if let Some(failure) = context.failure.as_ref() {
        let code = sabi::v1::SabiErrorCode::try_from(failure.code)
            .map_err(|_| CommonSemanticsError::UnknownErrorCode(failure.code))?;
        if code == sabi::v1::SabiErrorCode::Unspecified {
            return Err(CommonSemanticsError::UnspecifiedErrorCode);
        }
        let retry = sabi::v1::RetryDirective::try_from(failure.retry)
            .map_err(|_| CommonSemanticsError::UnknownRetryDirective(failure.retry))?;
        if retry == sabi::v1::RetryDirective::Unspecified {
            return Err(CommonSemanticsError::UnspecifiedRetryDirective);
        }
        if failure.safe_message.len() > MAX_SAFE_ERROR_MESSAGE_BYTES
            || failure.safe_message.contains('\0')
        {
            return Err(CommonSemanticsError::UnsafeErrorMessage);
        }

        let operation_present = context.operation.is_some();
        let receipts_present = !context.receipts.is_empty();
        if retry == sabi::v1::RetryDirective::QueryOperationOrRetrySameIdempotencyKey
            && !operation_present
        {
            return Err(CommonSemanticsError::MissingOperationForUncertainOutcome);
        }
        match code {
            sabi::v1::SabiErrorCode::Uncertain | sabi::v1::SabiErrorCode::EffectUnknown => {
                if retry != sabi::v1::RetryDirective::QueryOperationOrRetrySameIdempotencyKey {
                    return Err(CommonSemanticsError::InvalidRetryDirective);
                }
            }
            sabi::v1::SabiErrorCode::Retry => {
                if retry != sabi::v1::RetryDirective::RetrySameIdempotencyKey {
                    return Err(CommonSemanticsError::InvalidRetryDirective);
                }
            }
            sabi::v1::SabiErrorCode::Partial if !receipts_present => {
                return Err(CommonSemanticsError::MissingReceiptForPartialOutcome);
            }
            _ => {}
        }
    }
    Ok(context)
}

fn validate_id(field: &'static str, value: &[u8]) -> Result<(), CommonSemanticsError> {
    if value.len() != REQUEST_ID_BYTES {
        return Err(CommonSemanticsError::InvalidIdentifierLength {
            field,
            actual: value.len(),
        });
    }
    Ok(())
}

fn validate_generation(field: &'static str, value: u64) -> Result<(), CommonSemanticsError> {
    if value == 0 {
        return Err(CommonSemanticsError::ZeroGeneration(field));
    }
    Ok(())
}

fn validate_capability(handle: &sabi::v1::CapabilityHandle) -> Result<(), CommonSemanticsError> {
    validate_generation("capability slot", handle.slot)?;
    validate_generation("capability", handle.generation)
}

fn validate_capabilities(
    handles: &[sabi::v1::CapabilityHandle],
) -> Result<(), CommonSemanticsError> {
    if handles.len() > MAX_CAPABILITY_HANDLES {
        return Err(CommonSemanticsError::TooManyCapabilityHandles {
            actual: handles.len(),
            maximum: MAX_CAPABILITY_HANDLES,
        });
    }
    for (index, handle) in handles.iter().enumerate() {
        validate_capability(handle)?;
        if handles[..index].contains(handle) {
            return Err(CommonSemanticsError::DuplicateCapabilityHandle);
        }
    }
    Ok(())
}

fn validate_operation(
    operation: &sabi::v1::OperationReference,
) -> Result<(), CommonSemanticsError> {
    validate_id("operation_id", &operation.operation_id)?;
    validate_generation("operation", operation.generation)
}

fn validate_receipts(receipts: &[sabi::v1::ReceiptReference]) -> Result<(), CommonSemanticsError> {
    if receipts.len() > MAX_RECEIPT_REFERENCES {
        return Err(CommonSemanticsError::TooManyReceiptReferences {
            actual: receipts.len(),
            maximum: MAX_RECEIPT_REFERENCES,
        });
    }
    for (index, receipt) in receipts.iter().enumerate() {
        validate_id("receipt_id", &receipt.receipt_id)?;
        if receipts[..index]
            .iter()
            .any(|previous| previous.receipt_id == receipt.receipt_id)
        {
            return Err(CommonSemanticsError::DuplicateReceiptReference);
        }
    }
    Ok(())
}

fn encode_bounded(message: &impl Message) -> Result<Vec<u8>, CompatibilityError> {
    encode_bounded_with_limit(message, MAX_ENVELOPE_BYTES)
}

fn encode_bounded_with_limit(
    message: &impl Message,
    maximum: usize,
) -> Result<Vec<u8>, CompatibilityError> {
    let wire = message.encode_to_vec();
    if wire.len() > maximum {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum,
        });
    }
    Ok(wire)
}

fn decode_bounded<M: Message + Default>(wire: &[u8]) -> Result<M, CompatibilityError> {
    decode_bounded_with_limit(wire, MAX_ENVELOPE_BYTES)
}

fn decode_bounded_with_limit<M: Message + Default>(
    wire: &[u8],
    maximum: usize,
) -> Result<M, CompatibilityError> {
    if wire.len() > maximum {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum,
        });
    }
    M::decode(wire).map_err(|error| CompatibilityError::MalformedProtobuf(error.to_string()))
}

fn validate_service_directory_identity(
    identity: Option<&sabi::v1::SchemaIdentity>,
) -> Result<(), CompatibilityError> {
    let identity = identity.ok_or(CompatibilityError::MissingSchemaIdentity)?;
    validate_schema_identity(identity)?;
    if identity.name != SABI_SERVICE_DIRECTORY_SCHEMA {
        return Err(CompatibilityError::UnknownSchema(identity.name.clone()));
    }
    Ok(())
}

fn validate_operation_control_identity(
    identity: Option<&sabi::v1::SchemaIdentity>,
) -> Result<(), CompatibilityError> {
    let identity = identity.ok_or(CompatibilityError::MissingSchemaIdentity)?;
    validate_schema_identity(identity)?;
    if identity.name != SABI_OPERATION_CONTROL_SCHEMA {
        return Err(CompatibilityError::UnknownSchema(identity.name.clone()));
    }
    Ok(())
}

fn validate_operation_reference(
    operation: Option<&sabi::v1::OperationReference>,
) -> Result<(), CompatibilityError> {
    let operation = operation.ok_or(CompatibilityError::MissingOperationReference)?;
    if operation.operation_id.len() != REQUEST_ID_BYTES || operation.generation == 0 {
        return Err(CompatibilityError::MissingOperationReference);
    }
    Ok(())
}

fn validate_schema_identity(identity: &sabi::v1::SchemaIdentity) -> Result<(), CompatibilityError> {
    let descriptor = REGISTRY
        .iter()
        .find(|descriptor| descriptor.name == identity.name)
        .ok_or_else(|| CompatibilityError::UnknownSchema(identity.name.clone()))?;

    if identity.major != descriptor.major {
        return Err(CompatibilityError::UnsupportedMajor {
            schema: identity.name.clone(),
            got: identity.major,
            supported: descriptor.major,
        });
    }

    if let Some(extension_id) = identity.critical_extension_ids.iter().find(|extension_id| {
        !descriptor
            .supported_critical_extensions
            .contains(extension_id)
    }) {
        return Err(CompatibilityError::UnsupportedCriticalExtension {
            schema: identity.name.clone(),
            extension_id: *extension_id,
        });
    }
    Ok(())
}

fn validate_envelope(envelope: &sabi::v1::Envelope) -> Result<(), CompatibilityError> {
    let identity = envelope
        .schema
        .as_ref()
        .ok_or(CompatibilityError::MissingSchemaIdentity)?;
    validate_schema_identity(identity)?;

    if envelope.request_id.len() != REQUEST_ID_BYTES {
        return Err(CompatibilityError::InvalidRequestIdLength {
            actual: envelope.request_id.len(),
        });
    }
    if envelope.service.is_empty() {
        return Err(CompatibilityError::EmptyService);
    }
    if envelope.method.is_empty() {
        return Err(CompatibilityError::EmptyMethod);
    }
    Ok(())
}
