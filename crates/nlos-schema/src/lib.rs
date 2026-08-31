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
pub const SABI_SYSTEM_CONTROL_SCHEMA: &str = "nlos.sabi.SystemControl";
pub const SABI_TAKEOVER_CONTROL_SCHEMA: &str = "nlos.sabi.TakeoverControl";
pub const SABI_WAIT_CONTROL_SCHEMA: &str = "nlos.sabi.WaitControl";
/// ADR-0011 connection-level principal handshake channel; the seventh
/// registry entry, added additively under the ADR-0014 v1-beta freeze.
pub const SABI_PRINCIPAL_HANDSHAKE_SCHEMA: &str = "nlos.sabi.PrincipalHandshake";
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;
pub const MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_OPERATION_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_SYSTEM_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_WAIT_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES: usize = 4 * 1024;
pub const MAX_SYSTEM_CONTROL_ALERTS: usize = 256;
pub const MAX_SYSTEM_CONTROL_FAILURES: usize = 64;
pub const MAX_CONTROL_REASON_BYTES: usize = 512;
pub const REQUEST_ID_BYTES: usize = 16;
pub const SHA256_DIGEST_BYTES: usize = 32;
/// Ed25519 challenge nonces are exactly 32 bytes on the handshake wire.
pub const HANDSHAKE_NONCE_BYTES: usize = 32;
/// Ed25519 signatures are exactly 64 bytes on the handshake wire.
pub const HANDSHAKE_SIGNATURE_BYTES: usize = 64;
/// The transport-derived channel binding is bounded and must be non-empty so
/// every attestation is pinned to one concrete connection context.
pub const MAX_HANDSHAKE_CHANNEL_BINDING_BYTES: usize = 256;
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
    /// ADR-0014 v1-beta freeze marker. Frozen entries lock their wire bytes:
    /// only additive extension (new field numbers / new message types) is
    /// permitted; renumbering, semantic changes, or field removal require a
    /// new ADR. This is descriptive registry metadata, not a compile-time
    /// enforcement — the byte-level defense remains the conformance goldens.
    pub frozen: bool,
}

const SABI_ENVELOPE_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_ENVELOPE_SCHEMA,
    major: 1,
    minor: 1,
    supported_critical_extensions: &[],
    frozen: true,
};

const SABI_SERVICE_DIRECTORY_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_SERVICE_DIRECTORY_SCHEMA,
    major: 1,
    minor: 0,
    supported_critical_extensions: &[],
    frozen: true,
};

const SABI_OPERATION_CONTROL_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_OPERATION_CONTROL_SCHEMA,
    major: 1,
    minor: 0,
    supported_critical_extensions: &[],
    frozen: true,
};

const SABI_SYSTEM_CONTROL_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_SYSTEM_CONTROL_SCHEMA,
    major: 1,
    minor: 0,
    supported_critical_extensions: &[],
    frozen: true,
};

const SABI_TAKEOVER_CONTROL_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_TAKEOVER_CONTROL_SCHEMA,
    major: 1,
    minor: 0,
    supported_critical_extensions: &[],
    frozen: true,
};

const SABI_WAIT_CONTROL_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_WAIT_CONTROL_SCHEMA,
    major: 1,
    minor: 0,
    supported_critical_extensions: &[],
    frozen: true,
};

const SABI_PRINCIPAL_HANDSHAKE_V1: SchemaDescriptor = SchemaDescriptor {
    name: SABI_PRINCIPAL_HANDSHAKE_SCHEMA,
    major: 1,
    minor: 0,
    supported_critical_extensions: &[],
    frozen: false,
};

const REGISTRY: &[SchemaDescriptor] = &[
    SABI_ENVELOPE_V1,
    SABI_SERVICE_DIRECTORY_V1,
    SABI_OPERATION_CONTROL_V1,
    SABI_SYSTEM_CONTROL_V1,
    SABI_TAKEOVER_CONTROL_V1,
    SABI_WAIT_CONTROL_V1,
    SABI_PRINCIPAL_HANDSHAKE_V1,
];

#[must_use]
pub fn schema_registry() -> &'static [SchemaDescriptor] {
    REGISTRY
}

/// Returns the ADR-0014 v1-beta freeze state of a registered schema.
///
/// [`Some`] carries the descriptor's `frozen` marker for a known schema
/// name; [`None`] means the name is not in the registry. Frozen entries
/// admit additive wire extension only; the byte-level lock itself is
/// enforced by the conformance goldens, not by this metadata.
#[must_use]
pub fn registry_frozen(name: &str) -> Option<bool> {
    REGISTRY
        .iter()
        .find(|descriptor| descriptor.name == name)
        .map(|descriptor| descriptor.frozen)
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
    MissingSystemControlCommand,
    MissingSystemControlMetrics,
    InvalidSystemControlMetrics,
    UnspecifiedSystemControlView,
    UnspecifiedControlCommandSource,
    UnspecifiedControlScope,
    UnspecifiedControlCommandState,
    InvalidSystemControlIdentifier,
    InvalidSystemControlAlertLimit,
    InvalidSystemControlAlert,
    TooManySystemControlAlerts,
    TooManySystemControlFailures,
    UnsafeControlReason,
    MissingTakeoverControlTarget,
    MissingTakeoverControlEvidence,
    MissingTakeoverControlSignature,
    InvalidTakeoverControlIdentifier,
    UnspecifiedTakeoverControlParticipantType,
    InvalidTakeoverControlTimestamp,
    InvalidTakeoverControlGeneration,
    MissingTakeoverControlSigner,
    UnsignedTakeoverControlRecord,
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
    InvalidHandshakeNonceLength {
        actual: usize,
    },
    InvalidHandshakePrincipalIdLength {
        actual: usize,
    },
    InvalidHandshakeSignatureLength {
        actual: usize,
    },
    InvalidHandshakeChannelBindingLength {
        actual: usize,
    },
}

// A flat Display match grows linearly with the variant count; splitting it
// by service would only obscure the one-message-per-variant table.
#[allow(clippy::too_many_lines)]
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
            Self::MissingSystemControlCommand => {
                formatter.write_str("SystemControl payload is missing its command")
            }
            Self::MissingSystemControlMetrics => {
                formatter.write_str("SystemControl snapshot is missing recovery metrics")
            }
            Self::InvalidSystemControlMetrics => {
                formatter.write_str("SystemControl recovery metrics are malformed")
            }
            Self::UnspecifiedSystemControlView => {
                formatter.write_str("SystemControl request uses an unspecified view")
            }
            Self::UnspecifiedControlCommandSource => {
                formatter.write_str("ControlCommand uses an unspecified source")
            }
            Self::UnspecifiedControlScope => {
                formatter.write_str("ControlCommand uses an unspecified scope")
            }
            Self::UnspecifiedControlCommandState => {
                formatter.write_str("ControlCommand result uses an unspecified state")
            }
            Self::InvalidSystemControlIdentifier => {
                formatter.write_str("SystemControl payload contains an invalid identifier")
            }
            Self::InvalidSystemControlAlertLimit => {
                formatter.write_str("SystemControl alert limit is outside its bounded range")
            }
            Self::InvalidSystemControlAlert => {
                formatter.write_str("SystemControl recovery alert is malformed")
            }
            Self::TooManySystemControlAlerts => {
                formatter.write_str("SystemControl snapshot contains too many alerts")
            }
            Self::TooManySystemControlFailures => {
                formatter.write_str("SystemControl metrics contain too many failure summaries")
            }
            Self::UnsafeControlReason => {
                formatter.write_str("ControlCommand reason is oversized or contains NUL")
            }
            Self::MissingTakeoverControlTarget => {
                formatter.write_str("TakeoverControl payload is missing its target")
            }
            Self::MissingTakeoverControlEvidence => {
                formatter.write_str("TakeoverControl payload is missing its evidence")
            }
            Self::MissingTakeoverControlSignature => {
                formatter.write_str("TakeoverControl payload is missing its signature")
            }
            Self::InvalidTakeoverControlIdentifier => {
                formatter.write_str("TakeoverControl payload contains an invalid identifier length")
            }
            Self::UnspecifiedTakeoverControlParticipantType => {
                formatter.write_str("TakeoverControl participant type is outside 1..=8")
            }
            Self::InvalidTakeoverControlTimestamp => {
                formatter.write_str("TakeoverControl observation timestamp is negative")
            }
            Self::InvalidTakeoverControlGeneration => {
                formatter.write_str("TakeoverControl generation is zero")
            }
            Self::MissingTakeoverControlSigner => {
                formatter.write_str("TakeoverControl record is missing its verified signer")
            }
            Self::UnsignedTakeoverControlRecord => {
                formatter.write_str("TakeoverControl record must be signed on this path")
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
            Self::InvalidHandshakeNonceLength { actual } => write!(
                formatter,
                "handshake nonce has {actual} bytes; exactly {HANDSHAKE_NONCE_BYTES} are required"
            ),
            Self::InvalidHandshakePrincipalIdLength { actual } => write!(
                formatter,
                "handshake principal_id has {actual} bytes; exactly {REQUEST_ID_BYTES} are required"
            ),
            Self::InvalidHandshakeSignatureLength { actual } => write!(
                formatter,
                "handshake signature has {actual} bytes; exactly {HANDSHAKE_SIGNATURE_BYTES} are required"
            ),
            Self::InvalidHandshakeChannelBindingLength { actual } => write!(
                formatter,
                "handshake channel binding has {actual} bytes; 1..={MAX_HANDSHAKE_CHANNEL_BINDING_BYTES} are required"
            ),
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

/// Returns the v1 identity required on every `SystemControl` payload.
#[must_use]
pub fn system_control_schema_identity() -> sabi::v1::SchemaIdentity {
    sabi::v1::SchemaIdentity {
        name: SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}

/// Returns the v1 identity required on every `TakeoverControl` payload.
#[must_use]
pub fn takeover_control_schema_identity() -> sabi::v1::SchemaIdentity {
    sabi::v1::SchemaIdentity {
        name: SABI_TAKEOVER_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}

/// Returns the v1 identity required on every `WaitControl` payload.
#[must_use]
pub fn wait_control_schema_identity() -> sabi::v1::SchemaIdentity {
    sabi::v1::SchemaIdentity {
        name: SABI_WAIT_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}

/// Encodes a bounded, validated `TakeoverControl.submit_barrier_observation`
/// request.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity, target, evidence,
/// signature, or bound.
pub fn encode_submit_barrier_observation_request(
    request: &sabi::v1::SubmitBarrierObservationRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_submit_barrier_observation_request(request)?;
    encode_bounded_with_limit(request, MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded, validated `TakeoverControl.submit_barrier_observation`
/// request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_submit_barrier_observation_request(
    wire: &[u8],
) -> Result<sabi::v1::SubmitBarrierObservationRequest, CompatibilityError> {
    let request: sabi::v1::SubmitBarrierObservationRequest =
        decode_bounded_with_limit(wire, MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES)?;
    validate_submit_barrier_observation_request(&request)?;
    Ok(request)
}

/// Encodes a bounded, validated durable barrier observation record.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity, signer, or bound.
pub fn encode_barrier_observation_record(
    record: &sabi::v1::BarrierObservationRecord,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_barrier_observation_record(record)?;
    encode_bounded_with_limit(record, MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded, validated durable barrier observation record.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_barrier_observation_record(
    wire: &[u8],
) -> Result<sabi::v1::BarrierObservationRecord, CompatibilityError> {
    let record: sabi::v1::BarrierObservationRecord =
        decode_bounded_with_limit(wire, MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES)?;
    validate_barrier_observation_record(&record)?;
    Ok(record)
}

/// Encodes a bounded, typed `SystemControl.get` request.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity, view, limit, or bound.
pub fn encode_get_system_control_request(
    request: &sabi::v1::GetSystemControlRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_get_system_control_request(request)?;
    encode_bounded_with_limit(request, MAX_SYSTEM_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded, typed `SystemControl.get` request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_get_system_control_request(
    wire: &[u8],
) -> Result<sabi::v1::GetSystemControlRequest, CompatibilityError> {
    let request: sabi::v1::GetSystemControlRequest =
        decode_bounded_with_limit(wire, MAX_SYSTEM_CONTROL_PAYLOAD_BYTES)?;
    validate_get_system_control_request(&request)?;
    Ok(request)
}

/// Encodes a bounded, sanitized Artifact recovery operations snapshot.
///
/// # Errors
///
/// Returns a compatibility error for malformed metrics, alerts, or identities.
pub fn encode_artifact_recovery_operations_snapshot(
    snapshot: &sabi::v1::ArtifactRecoveryOperationsSnapshot,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_artifact_recovery_operations_snapshot(snapshot)?;
    encode_bounded_with_limit(snapshot, MAX_SYSTEM_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded, sanitized Artifact recovery operations snapshot.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_artifact_recovery_operations_snapshot(
    wire: &[u8],
) -> Result<sabi::v1::ArtifactRecoveryOperationsSnapshot, CompatibilityError> {
    let snapshot: sabi::v1::ArtifactRecoveryOperationsSnapshot =
        decode_bounded_with_limit(wire, MAX_SYSTEM_CONTROL_PAYLOAD_BYTES)?;
    validate_artifact_recovery_operations_snapshot(&snapshot)?;
    Ok(snapshot)
}

/// Encodes a bounded `SystemControl.submit` command request.
///
/// # Errors
///
/// Returns a compatibility error for malformed command identity, source,
/// scope, target, CAS, command variant, reason, or payload size.
pub fn encode_submit_control_command_request(
    request: &sabi::v1::SubmitControlCommandRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_system_control_identity(request.schema.as_ref())?;
    validate_control_command(request.command.as_ref())?;
    encode_bounded_with_limit(request, MAX_SYSTEM_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `SystemControl.submit` command request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_submit_control_command_request(
    wire: &[u8],
) -> Result<sabi::v1::SubmitControlCommandRequest, CompatibilityError> {
    let request: sabi::v1::SubmitControlCommandRequest =
        decode_bounded_with_limit(wire, MAX_SYSTEM_CONTROL_PAYLOAD_BYTES)?;
    validate_system_control_identity(request.schema.as_ref())?;
    validate_control_command(request.command.as_ref())?;
    Ok(request)
}

/// Encodes a bounded durable `ControlCommand` result.
///
/// # Errors
///
/// Returns a compatibility error for malformed identity, state, or Receipt.
pub fn encode_control_command_result(
    result: &sabi::v1::ControlCommandResult,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_control_command_result(result)?;
    encode_bounded_with_limit(result, MAX_SYSTEM_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded durable `ControlCommand` result.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_control_command_result(
    wire: &[u8],
) -> Result<sabi::v1::ControlCommandResult, CompatibilityError> {
    let result: sabi::v1::ControlCommandResult =
        decode_bounded_with_limit(wire, MAX_SYSTEM_CONTROL_PAYLOAD_BYTES)?;
    validate_control_command_result(&result)?;
    Ok(result)
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

/// Validates and encodes a bounded `WaitControl.register_wait` request.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_register_wait_request(
    request: &sabi::v1::RegisterWaitRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(request.schema.as_ref())?;
    encode_bounded_with_limit(request, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.register_wait` request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_register_wait_request(
    wire: &[u8],
) -> Result<sabi::v1::RegisterWaitRequest, CompatibilityError> {
    let request: sabi::v1::RegisterWaitRequest =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(request.schema.as_ref())?;
    Ok(request)
}

/// Validates and encodes a bounded `WaitControl.register_wait` result.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_register_wait_result(
    result: &sabi::v1::RegisterWaitResult,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(result.schema.as_ref())?;
    encode_bounded_with_limit(result, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.register_wait` result.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_register_wait_result(
    wire: &[u8],
) -> Result<sabi::v1::RegisterWaitResult, CompatibilityError> {
    let result: sabi::v1::RegisterWaitResult =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(result.schema.as_ref())?;
    Ok(result)
}

/// Validates and encodes a bounded `WaitControl.notify_commits` request.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_notify_commits_request(
    request: &sabi::v1::NotifyCommitsRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(request.schema.as_ref())?;
    encode_bounded_with_limit(request, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.notify_commits` request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_notify_commits_request(
    wire: &[u8],
) -> Result<sabi::v1::NotifyCommitsRequest, CompatibilityError> {
    let request: sabi::v1::NotifyCommitsRequest =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(request.schema.as_ref())?;
    Ok(request)
}

/// Validates and encodes a bounded `WaitControl.notify_commits` result.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_notify_commits_result(
    report: &sabi::v1::WakeReport,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(report.schema.as_ref())?;
    encode_bounded_with_limit(report, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.notify_commits` result.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_notify_commits_result(
    wire: &[u8],
) -> Result<sabi::v1::WakeReport, CompatibilityError> {
    let report: sabi::v1::WakeReport =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(report.schema.as_ref())?;
    Ok(report)
}

/// Validates and encodes a bounded `WaitControl.cancel_wait` request.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_cancel_wait_request(
    request: &sabi::v1::CancelWaitRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(request.schema.as_ref())?;
    encode_bounded_with_limit(request, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.cancel_wait` request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_cancel_wait_request(
    wire: &[u8],
) -> Result<sabi::v1::CancelWaitRequest, CompatibilityError> {
    let request: sabi::v1::CancelWaitRequest =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(request.schema.as_ref())?;
    Ok(request)
}

/// Validates and encodes a bounded `WaitControl.cancel_wait` result.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_cancel_wait_result(
    result: &sabi::v1::CancelWaitResult,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(result.schema.as_ref())?;
    encode_bounded_with_limit(result, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.cancel_wait` result.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_cancel_wait_result(
    wire: &[u8],
) -> Result<sabi::v1::CancelWaitResult, CompatibilityError> {
    let result: sabi::v1::CancelWaitResult =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(result.schema.as_ref())?;
    Ok(result)
}

/// Validates and encodes a bounded `WaitControl.list_waits` request.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_list_waits_request(
    request: &sabi::v1::ListWaitsRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(request.schema.as_ref())?;
    encode_bounded_with_limit(request, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.list_waits` request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_list_waits_request(
    wire: &[u8],
) -> Result<sabi::v1::ListWaitsRequest, CompatibilityError> {
    let request: sabi::v1::ListWaitsRequest =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(request.schema.as_ref())?;
    Ok(request)
}

/// Validates and encodes a bounded `WaitControl.list_waits` result.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_list_waits_result(
    result: &sabi::v1::ListWaitsResult,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(result.schema.as_ref())?;
    encode_bounded_with_limit(result, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.list_waits` result.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_list_waits_result(
    wire: &[u8],
) -> Result<sabi::v1::ListWaitsResult, CompatibilityError> {
    let result: sabi::v1::ListWaitsResult =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(result.schema.as_ref())?;
    Ok(result)
}

/// Validates and encodes a bounded `WaitControl.inspect_wait` request.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_inspect_wait_request(
    request: &sabi::v1::InspectWaitRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(request.schema.as_ref())?;
    encode_bounded_with_limit(request, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.inspect_wait` request.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_inspect_wait_request(
    wire: &[u8],
) -> Result<sabi::v1::InspectWaitRequest, CompatibilityError> {
    let request: sabi::v1::InspectWaitRequest =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(request.schema.as_ref())?;
    Ok(request)
}

/// Validates and encodes a bounded `WaitControl.inspect_wait` result.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or oversized payload.
pub fn encode_inspect_wait_result(
    result: &sabi::v1::InspectWaitResult,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_wait_control_identity(result.schema.as_ref())?;
    encode_bounded_with_limit(result, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
}

/// Decodes a bounded `WaitControl.inspect_wait` result.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_inspect_wait_result(
    wire: &[u8],
) -> Result<sabi::v1::InspectWaitResult, CompatibilityError> {
    let result: sabi::v1::InspectWaitResult =
        decode_bounded_with_limit(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
    validate_wait_control_identity(result.schema.as_ref())?;
    Ok(result)
}

/// Returns the v1 identity required on every `PrincipalHandshake` payload.
#[must_use]
pub fn principal_handshake_schema_identity() -> sabi::v1::SchemaIdentity {
    sabi::v1::SchemaIdentity {
        name: SABI_PRINCIPAL_HANDSHAKE_SCHEMA.to_owned(),
        major: 1,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}

/// Validates and encodes a bounded `PrincipalHandshake` challenge.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity or malformed nonce.
pub fn encode_principal_handshake_challenge(
    challenge: &sabi::v1::PrincipalHandshakeChallenge,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_principal_handshake_identity(challenge.schema.as_ref())?;
    validate_handshake_nonce(&challenge.nonce)?;
    encode_bounded_with_limit(challenge, MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES)
}

/// Decodes a bounded `PrincipalHandshake` challenge.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_principal_handshake_challenge(
    wire: &[u8],
) -> Result<sabi::v1::PrincipalHandshakeChallenge, CompatibilityError> {
    let challenge: sabi::v1::PrincipalHandshakeChallenge =
        decode_bounded_with_limit(wire, MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES)?;
    validate_principal_handshake_identity(challenge.schema.as_ref())?;
    validate_handshake_nonce(&challenge.nonce)?;
    Ok(challenge)
}

/// Validates and encodes a bounded `PrincipalHandshake` attestation.
///
/// # Errors
///
/// Returns a compatibility error for an invalid identity, malformed
/// principal/nonce/signature, or unbounded channel binding.
pub fn encode_principal_handshake_attestation(
    attestation: &sabi::v1::PrincipalHandshakeAttestation,
) -> Result<Vec<u8>, CompatibilityError> {
    validate_principal_handshake_identity(attestation.schema.as_ref())?;
    validate_principal_handshake_attestation_fields(attestation)?;
    encode_bounded_with_limit(attestation, MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES)
}

/// Decodes a bounded `PrincipalHandshake` attestation.
///
/// # Errors
///
/// Returns a compatibility error for malformed, incompatible, or oversized input.
pub fn decode_principal_handshake_attestation(
    wire: &[u8],
) -> Result<sabi::v1::PrincipalHandshakeAttestation, CompatibilityError> {
    let attestation: sabi::v1::PrincipalHandshakeAttestation =
        decode_bounded_with_limit(wire, MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES)?;
    validate_principal_handshake_identity(attestation.schema.as_ref())?;
    validate_principal_handshake_attestation_fields(&attestation)?;
    Ok(attestation)
}

fn validate_principal_handshake_identity(
    identity: Option<&sabi::v1::SchemaIdentity>,
) -> Result<(), CompatibilityError> {
    let identity = identity.ok_or(CompatibilityError::MissingSchemaIdentity)?;
    validate_schema_identity(identity)?;
    if identity.name != SABI_PRINCIPAL_HANDSHAKE_SCHEMA {
        return Err(CompatibilityError::UnknownSchema(identity.name.clone()));
    }
    Ok(())
}

fn validate_handshake_nonce(nonce: &[u8]) -> Result<(), CompatibilityError> {
    if nonce.len() != HANDSHAKE_NONCE_BYTES {
        return Err(CompatibilityError::InvalidHandshakeNonceLength {
            actual: nonce.len(),
        });
    }
    Ok(())
}

fn validate_principal_handshake_attestation_fields(
    attestation: &sabi::v1::PrincipalHandshakeAttestation,
) -> Result<(), CompatibilityError> {
    validate_handshake_nonce(&attestation.nonce)?;
    if attestation.principal_id.len() != REQUEST_ID_BYTES {
        return Err(CompatibilityError::InvalidHandshakePrincipalIdLength {
            actual: attestation.principal_id.len(),
        });
    }
    if attestation.signature.len() != HANDSHAKE_SIGNATURE_BYTES {
        return Err(CompatibilityError::InvalidHandshakeSignatureLength {
            actual: attestation.signature.len(),
        });
    }
    if attestation.channel_binding.is_empty()
        || attestation.channel_binding.len() > MAX_HANDSHAKE_CHANNEL_BINDING_BYTES
    {
        return Err(CompatibilityError::InvalidHandshakeChannelBindingLength {
            actual: attestation.channel_binding.len(),
        });
    }
    Ok(())
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
    // A known terminal failure is effect evidence that no mutation was
    // accepted; uncertain/partial failures still require their reconciliation
    // references and are checked below.
    if semantics.side_effecting
        && context.operation.is_none()
        && context.receipts.is_empty()
        && context.failure.is_none()
    {
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

fn validate_system_control_identity(
    identity: Option<&sabi::v1::SchemaIdentity>,
) -> Result<(), CompatibilityError> {
    let identity = identity.ok_or(CompatibilityError::MissingSchemaIdentity)?;
    validate_schema_identity(identity)?;
    if identity.name != SABI_SYSTEM_CONTROL_SCHEMA {
        return Err(CompatibilityError::UnknownSchema(identity.name.clone()));
    }
    Ok(())
}

fn validate_takeover_control_identity(
    identity: Option<&sabi::v1::SchemaIdentity>,
) -> Result<(), CompatibilityError> {
    let identity = identity.ok_or(CompatibilityError::MissingSchemaIdentity)?;
    validate_schema_identity(identity)?;
    if identity.name != SABI_TAKEOVER_CONTROL_SCHEMA {
        return Err(CompatibilityError::UnknownSchema(identity.name.clone()));
    }
    Ok(())
}

fn validate_wait_control_identity(
    identity: Option<&sabi::v1::SchemaIdentity>,
) -> Result<(), CompatibilityError> {
    let identity = identity.ok_or(CompatibilityError::MissingSchemaIdentity)?;
    validate_schema_identity(identity)?;
    if identity.name != SABI_WAIT_CONTROL_SCHEMA {
        return Err(CompatibilityError::UnknownSchema(identity.name.clone()));
    }
    Ok(())
}

fn validate_submit_barrier_observation_request(
    request: &sabi::v1::SubmitBarrierObservationRequest,
) -> Result<(), CompatibilityError> {
    validate_takeover_control_identity(request.schema.as_ref())?;
    validate_barrier_observation_target(
        request
            .target
            .as_ref()
            .ok_or(CompatibilityError::MissingTakeoverControlTarget)?,
    )?;
    validate_barrier_observation_evidence(
        request
            .evidence
            .as_ref()
            .ok_or(CompatibilityError::MissingTakeoverControlEvidence)?,
    )?;
    validate_barrier_observation_signature(
        request
            .signature
            .as_ref()
            .ok_or(CompatibilityError::MissingTakeoverControlSignature)?,
    )
}

fn validate_barrier_observation_target(
    target: &sabi::v1::BarrierObservationTarget,
) -> Result<(), CompatibilityError> {
    if target.takeover_receipt_id.len() != REQUEST_ID_BYTES
        || target.participant_id.len() != REQUEST_ID_BYTES
        || target.admission_receipt_id.len() != REQUEST_ID_BYTES
    {
        return Err(CompatibilityError::InvalidTakeoverControlIdentifier);
    }
    if !(1..=8).contains(&target.participant_type) {
        return Err(CompatibilityError::UnspecifiedTakeoverControlParticipantType);
    }
    if target.participant_generation == 0 {
        return Err(CompatibilityError::InvalidTakeoverControlGeneration);
    }
    Ok(())
}

fn validate_barrier_observation_evidence(
    evidence: &sabi::v1::BarrierObservationEvidence,
) -> Result<(), CompatibilityError> {
    if evidence.remote_receipt_id.len() != REQUEST_ID_BYTES
        || evidence.barrier_digest.len() != SHA256_DIGEST_BYTES
    {
        return Err(CompatibilityError::InvalidTakeoverControlIdentifier);
    }
    if evidence.observed_at_ms < 0 {
        return Err(CompatibilityError::InvalidTakeoverControlTimestamp);
    }
    Ok(())
}

fn validate_barrier_observation_signature(
    signature: &sabi::v1::BarrierObservationSignature,
) -> Result<(), CompatibilityError> {
    if signature.signer_principal_id.len() != REQUEST_ID_BYTES
        || signature.signer_control_domain_id.len() != REQUEST_ID_BYTES
        || signature.signer_key_id.len() != REQUEST_ID_BYTES
        || signature.signature.len() != 64
    {
        return Err(CompatibilityError::InvalidTakeoverControlIdentifier);
    }
    Ok(())
}

fn validate_barrier_observation_record(
    record: &sabi::v1::BarrierObservationRecord,
) -> Result<(), CompatibilityError> {
    validate_takeover_control_identity(record.schema.as_ref())?;
    if record.receipt_id.len() != REQUEST_ID_BYTES
        || record.participant_id.len() != REQUEST_ID_BYTES
        || record.barrier_digest.len() != SHA256_DIGEST_BYTES
        || record.signer_principal_id.len() != REQUEST_ID_BYTES
        || record.signer_key_id.len() != REQUEST_ID_BYTES
    {
        return Err(CompatibilityError::InvalidTakeoverControlIdentifier);
    }
    if !(1..=8).contains(&record.participant_type) {
        return Err(CompatibilityError::UnspecifiedTakeoverControlParticipantType);
    }
    if record.observed_at_ms < 0 {
        return Err(CompatibilityError::InvalidTakeoverControlTimestamp);
    }
    if record.signer_key_generation == 0 {
        return Err(CompatibilityError::InvalidTakeoverControlGeneration);
    }
    if !record.signed {
        return Err(CompatibilityError::UnsignedTakeoverControlRecord);
    }
    Ok(())
}

fn validate_get_system_control_request(
    request: &sabi::v1::GetSystemControlRequest,
) -> Result<(), CompatibilityError> {
    validate_system_control_identity(request.schema.as_ref())?;
    let view = sabi::v1::SystemControlView::try_from(request.view)
        .map_err(|_| CompatibilityError::UnspecifiedSystemControlView)?;
    if view == sabi::v1::SystemControlView::Unspecified {
        return Err(CompatibilityError::UnspecifiedSystemControlView);
    }
    if request.alert_limit == 0
        || usize::try_from(request.alert_limit).unwrap_or(usize::MAX) > MAX_SYSTEM_CONTROL_ALERTS
    {
        return Err(CompatibilityError::InvalidSystemControlAlertLimit);
    }
    Ok(())
}

fn validate_artifact_recovery_operations_snapshot(
    snapshot: &sabi::v1::ArtifactRecoveryOperationsSnapshot,
) -> Result<(), CompatibilityError> {
    validate_system_control_identity(snapshot.schema.as_ref())?;
    let metrics = snapshot
        .metrics
        .as_ref()
        .ok_or(CompatibilityError::MissingSystemControlMetrics)?;
    let worker_state = sabi::v1::RecoveryWorkerLifecycleState::try_from(metrics.worker_state)
        .map_err(|_| CompatibilityError::InvalidSystemControlMetrics)?;
    if worker_state == sabi::v1::RecoveryWorkerLifecycleState::Unspecified {
        return Err(CompatibilityError::InvalidSystemControlMetrics);
    }
    if metrics.last_failures.len() > MAX_SYSTEM_CONTROL_FAILURES {
        return Err(CompatibilityError::TooManySystemControlFailures);
    }
    for failure in &metrics.last_failures {
        if !failure.plan_id.is_empty() && failure.plan_id.len() != REQUEST_ID_BYTES {
            return Err(CompatibilityError::InvalidSystemControlIdentifier);
        }
        let authority = sabi::v1::RecoveryFailureAuthority::try_from(failure.authority)
            .map_err(|_| CompatibilityError::InvalidSystemControlMetrics)?;
        if authority == sabi::v1::RecoveryFailureAuthority::Unspecified {
            return Err(CompatibilityError::InvalidSystemControlMetrics);
        }
    }
    if snapshot.alerts.len() > MAX_SYSTEM_CONTROL_ALERTS {
        return Err(CompatibilityError::TooManySystemControlAlerts);
    }
    for alert in &snapshot.alerts {
        if alert.plan_id.len() != REQUEST_ID_BYTES
            || alert.total_failures == 0
            || alert.first_failed_at_ms < 0
            || alert.last_failed_at_ms < alert.first_failed_at_ms
            || alert.escalated_at_ms < alert.last_failed_at_ms
        {
            return Err(CompatibilityError::InvalidSystemControlAlert);
        }
        let authority = sabi::v1::RecoveryFailureAuthority::try_from(alert.last_failure_authority)
            .map_err(|_| CompatibilityError::InvalidSystemControlAlert)?;
        if authority == sabi::v1::RecoveryFailureAuthority::Unspecified {
            return Err(CompatibilityError::InvalidSystemControlAlert);
        }
        if alert
            .acknowledgement_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.receipt_id.len() != REQUEST_ID_BYTES)
        {
            return Err(CompatibilityError::InvalidReceiptReference);
        }
    }
    Ok(())
}

fn validate_control_command(
    command: Option<&sabi::v1::ControlCommand>,
) -> Result<(), CompatibilityError> {
    let command = command.ok_or(CompatibilityError::MissingSystemControlCommand)?;
    if command.control_command_id.len() != REQUEST_ID_BYTES
        || command.issuer_principal_id.len() != REQUEST_ID_BYTES
        || command.target_id.len() != REQUEST_ID_BYTES
        || command.expected_generation_or_revision == 0
    {
        return Err(CompatibilityError::InvalidSystemControlIdentifier);
    }
    let source = sabi::v1::ControlCommandSource::try_from(command.source)
        .map_err(|_| CompatibilityError::UnspecifiedControlCommandSource)?;
    if source == sabi::v1::ControlCommandSource::Unspecified {
        return Err(CompatibilityError::UnspecifiedControlCommandSource);
    }
    let scope = sabi::v1::ControlScope::try_from(command.scope)
        .map_err(|_| CompatibilityError::UnspecifiedControlScope)?;
    if scope != sabi::v1::ControlScope::Operation {
        return Err(CompatibilityError::UnspecifiedControlScope);
    }
    if command.command.is_none() {
        return Err(CompatibilityError::MissingSystemControlCommand);
    }
    if command.reason.len() > MAX_CONTROL_REASON_BYTES || command.reason.contains('\0') {
        return Err(CompatibilityError::UnsafeControlReason);
    }
    Ok(())
}

fn validate_control_command_result(
    result: &sabi::v1::ControlCommandResult,
) -> Result<(), CompatibilityError> {
    validate_system_control_identity(result.schema.as_ref())?;
    if result.control_command_id.len() != REQUEST_ID_BYTES {
        return Err(CompatibilityError::InvalidSystemControlIdentifier);
    }
    let state = sabi::v1::ControlCommandLifecycleState::try_from(result.state)
        .map_err(|_| CompatibilityError::UnspecifiedControlCommandState)?;
    if state == sabi::v1::ControlCommandLifecycleState::Unspecified {
        return Err(CompatibilityError::UnspecifiedControlCommandState);
    }
    if result
        .receipt
        .as_ref()
        .is_none_or(|receipt| receipt.receipt_id.len() != REQUEST_ID_BYTES)
    {
        return Err(CompatibilityError::InvalidReceiptReference);
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
