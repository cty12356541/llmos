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
        include!(concat!(env!("OUT_DIR"), "/nlos.sabi.v1.rs"));
    }
}

pub const SABI_ENVELOPE_SCHEMA: &str = "nlos.sabi.Envelope";
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;
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

const REGISTRY: &[SchemaDescriptor] = &[SABI_ENVELOPE_V1];

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

fn validate_envelope(envelope: &sabi::v1::Envelope) -> Result<(), CompatibilityError> {
    let identity = envelope
        .schema
        .as_ref()
        .ok_or(CompatibilityError::MissingSchemaIdentity)?;
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
