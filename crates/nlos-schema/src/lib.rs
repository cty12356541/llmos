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
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;
pub const MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES: usize = 64 * 1024;
pub const REQUEST_ID_BYTES: usize = 16;

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
    minor: 0,
    supported_critical_extensions: &[],
};

const SABI_SERVICE_DIRECTORY_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_SERVICE_DIRECTORY_SCHEMA,
    major: 1,
    minor: 0,
    supported_critical_extensions: &[],
};

const REGISTRY: &[SchemaDescriptor] = &[SABI_ENVELOPE_V1, SABI_SERVICE_DIRECTORY_V1];

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
