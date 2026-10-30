//! `N / N-1 / N-2` SABI version compatibility matrix adjudicator and
//! `[COMPAT-VER-002]` critical/non-critical extension negotiation.
//!
//! Authority:
//! - `06-架构设计总纲-v0.5.md` section 9 (`COMPAT-VER-001`, `COMPAT-VER-002`)
//! - `ADR-0027` section 2.1 / section 2.2 (out-of-core compatibility shim routing)
//! - `stage-e-progress.md` Decision Point `E1` (minimum two-epoch rolling window)

use prost::Message;

use crate::{
    CompatibilityError, MAX_ENVELOPE_BYTES, MAX_EXTENSION_IDS, MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES,
    REQUEST_ID_BYTES, ValidatedFrame, sabi, schema_registry,
};

/// Minimum rolling backward-compatibility window in major SABI epochs (`ADR-0027`
/// Decision Point `E1`: hosts at epoch `N` must support `N`, `N-1`, and `N-2`).
pub const MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS: u32 = 2;

/// Host-side compatibility policy used to adjudicate incoming SABI frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionCompatibilityPolicy<'a> {
    /// Current canonical SABI major epoch served natively by core authorities (`N`).
    pub host_epoch: u32,
    /// Number of historical major epochs routed to the out-of-core compatibility
    /// shim (`>= MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS`, default `2` for `N-1`/`N-2`).
    pub max_backward_epochs: u32,
    /// Receiver-supported critical extension identifiers (`[COMPAT-VER-002]`).
    pub known_critical_extensions: &'a [u32],
}

impl Default for VersionCompatibilityPolicy<'static> {
    fn default() -> Self {
        Self {
            host_epoch: 1,
            max_backward_epochs: MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
            known_critical_extensions: &[],
        }
    }
}

impl<'a> VersionCompatibilityPolicy<'a> {
    /// Constructs a policy for `host_epoch` with the statutory default
    /// `MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS` (`2`, covering `N`, `N-1`, `N-2`).
    #[must_use]
    pub const fn new(host_epoch: u32, known_critical_extensions: &'a [u32]) -> Self {
        Self {
            host_epoch,
            max_backward_epochs: MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
            known_critical_extensions,
        }
    }

    /// Constructs a policy with an explicit backward-compatibility epoch window.
    #[must_use]
    pub const fn with_backward_window(
        host_epoch: u32,
        max_backward_epochs: u32,
        known_critical_extensions: &'a [u32],
    ) -> Self {
        Self {
            host_epoch,
            max_backward_epochs,
            known_critical_extensions,
        }
    }

    /// Adjudicates a caller's [`sabi::v1::SchemaIdentity`] against the `N / N-1 / N-2`
    /// matrix and `[COMPAT-VER-002]` extension rules.
    ///
    /// # Errors
    ///
    /// Fails closed with [`CompatibilityError`] when:
    /// - the policy itself violates `host_epoch >= 1` or
    ///   `max_backward_epochs >= MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS`;
    /// - `identity.name` is not registered in [`schema_registry`];
    /// - `critical_extension_ids` or `non_critical_extension_ids` exceeds
    ///   [`MAX_EXTENSION_IDS`];
    /// - `identity.major == 0`, `identity.major > host_epoch` (`> N`), or
    ///   `host_epoch - identity.major > max_backward_epochs` (`<= N-3` sunset);
    /// - any `critical_extension_ids` entry is `0` or outside the receiver's
    ///   known critical extension set (`[COMPAT-VER-002]`).
    pub fn adjudicate_schema_identity(
        &self,
        identity: &sabi::v1::SchemaIdentity,
    ) -> Result<VersionNegotiationOutcome, CompatibilityError> {
        if self.host_epoch == 0 {
            return Err(CompatibilityError::ZeroMajorVersion {
                schema: identity.name.clone(),
            });
        }
        if self.max_backward_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS {
            return Err(CompatibilityError::InvalidCompatibilityWindow {
                got: self.max_backward_epochs,
                minimum: MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
            });
        }
        if self.known_critical_extensions.len() > MAX_EXTENSION_IDS {
            return Err(CompatibilityError::TooManyExtensionIds {
                schema: identity.name.clone(),
                actual: self.known_critical_extensions.len(),
                maximum: MAX_EXTENSION_IDS,
            });
        }

        let descriptor = schema_registry()
            .iter()
            .find(|entry| entry.name == identity.name)
            .ok_or_else(|| CompatibilityError::UnknownSchema(identity.name.clone()))?;

        if identity.critical_extension_ids.len() > MAX_EXTENSION_IDS {
            return Err(CompatibilityError::TooManyExtensionIds {
                schema: identity.name.clone(),
                actual: identity.critical_extension_ids.len(),
                maximum: MAX_EXTENSION_IDS,
            });
        }
        if identity.non_critical_extension_ids.len() > MAX_EXTENSION_IDS {
            return Err(CompatibilityError::TooManyExtensionIds {
                schema: identity.name.clone(),
                actual: identity.non_critical_extension_ids.len(),
                maximum: MAX_EXTENSION_IDS,
            });
        }

        if identity.major == 0 {
            return Err(CompatibilityError::ZeroMajorVersion {
                schema: identity.name.clone(),
            });
        }
        if identity.major > self.host_epoch {
            return Err(CompatibilityError::UnsupportedMajor {
                schema: identity.name.clone(),
                got: identity.major,
                supported: self.host_epoch,
            });
        }

        let epoch_distance = self.host_epoch - identity.major;
        if epoch_distance > self.max_backward_epochs {
            return Err(CompatibilityError::SunsetEpoch {
                schema: identity.name.clone(),
                client_epoch: identity.major,
                host_epoch: self.host_epoch,
                max_backward_epochs: self.max_backward_epochs,
            });
        }

        if let Some(&extension_id) = identity.critical_extension_ids.iter().find(|&&ext_id| {
            ext_id == 0
                || (!self.known_critical_extensions.contains(&ext_id)
                    && !descriptor.supported_critical_extensions.contains(&ext_id))
        }) {
            return Err(CompatibilityError::UnsupportedCriticalExtension {
                schema: identity.name.clone(),
                extension_id,
            });
        }

        let route = if epoch_distance == 0 {
            VersionRoute::NativeCurrentEpoch
        } else {
            VersionRoute::OutOfCoreCompatShim {
                client_epoch: identity.major,
                host_epoch: self.host_epoch,
                epoch_distance,
            }
        };

        Ok(VersionNegotiationOutcome {
            channel: descriptor.name,
            client_major: identity.major,
            client_minor: identity.minor,
            host_epoch: self.host_epoch,
            registry_minor: descriptor.minor,
            route,
            accepted_critical_extensions: identity.critical_extension_ids.clone(),
            retained_non_critical_extensions: identity.non_critical_extension_ids.clone(),
        })
    }

    /// Adjudicates an optional [`sabi::v1::SchemaIdentity`], failing closed on `None`.
    ///
    /// # Errors
    ///
    /// Returns [`CompatibilityError::MissingSchemaIdentity`] when `identity` is `None`,
    /// or any error from [`Self::adjudicate_schema_identity`].
    pub fn adjudicate_optional_schema_identity(
        &self,
        identity: Option<&sabi::v1::SchemaIdentity>,
    ) -> Result<VersionNegotiationOutcome, CompatibilityError> {
        let identity = identity.ok_or(CompatibilityError::MissingSchemaIdentity)?;
        self.adjudicate_schema_identity(identity)
    }

    /// Adjudicates a decoded [`sabi::v1::Envelope`] against this compatibility policy
    /// and common envelope invariants.
    ///
    /// # Errors
    ///
    /// Fails closed when the envelope lacks a valid `nlos.sabi.Envelope` identity,
    /// violates the compatibility policy, or has malformed `request_id`/`service`/`method`.
    pub fn adjudicate_envelope(
        &self,
        envelope: &sabi::v1::Envelope,
    ) -> Result<VersionNegotiationOutcome, CompatibilityError> {
        let identity = envelope
            .schema
            .as_ref()
            .ok_or(CompatibilityError::MissingSchemaIdentity)?;
        if identity.name != crate::SABI_ENVELOPE_SCHEMA {
            return Err(CompatibilityError::UnknownSchema(identity.name.clone()));
        }
        let outcome = self.adjudicate_schema_identity(identity)?;
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
        Ok(outcome)
    }

    /// Decodes a bounded SABI [`sabi::v1::Envelope`] wire frame, adjudicates its
    /// [`sabi::v1::SchemaIdentity`] under this policy, and retains the exact input
    /// wire bytes for lossless unknown-field / non-critical extension forwarding.
    ///
    /// # Errors
    ///
    /// Fails closed on oversized frames, malformed protobuf, or policy rejection.
    pub fn adjudicate_envelope_wire(
        &self,
        wire: &[u8],
    ) -> Result<(ValidatedFrame, VersionNegotiationOutcome), CompatibilityError> {
        if wire.len() > MAX_ENVELOPE_BYTES {
            return Err(CompatibilityError::FrameTooLarge {
                actual: wire.len(),
                maximum: MAX_ENVELOPE_BYTES,
            });
        }
        let envelope = sabi::v1::Envelope::decode(wire)
            .map_err(|error| CompatibilityError::MalformedProtobuf(error.to_string()))?;
        let outcome = self.adjudicate_envelope(&envelope)?;
        Ok((ValidatedFrame::from_parts(envelope, wire.to_vec()), outcome))
    }

    /// Validates an [`sabi::v1::Envelope`] under this policy and re-encodes it,
    /// preserving all known and unknown `non_critical_extension_ids`.
    ///
    /// # Errors
    ///
    /// Fails closed on policy rejection, envelope invariant failure, or size overflow.
    pub fn encode_adjudicated_envelope(
        &self,
        envelope: &sabi::v1::Envelope,
    ) -> Result<(Vec<u8>, VersionNegotiationOutcome), CompatibilityError> {
        let outcome = self.adjudicate_envelope(envelope)?;
        let wire = envelope.encode_to_vec();
        if wire.len() > MAX_ENVELOPE_BYTES {
            return Err(CompatibilityError::FrameTooLarge {
                actual: wire.len(),
                maximum: MAX_ENVELOPE_BYTES,
            });
        }
        Ok((wire, outcome))
    }
}

/// Deterministic routing decision produced by [`VersionCompatibilityPolicy`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionRoute {
    /// Current host epoch `N` (`client_major == host_epoch`, any additive `minor`):
    /// dispatched directly to core authorities with zero shim overhead.
    NativeCurrentEpoch,
    /// Historical epoch within `1 <= host_epoch - client_major <= max_backward_epochs`
    /// (`N-1` or `N-2`): routed to the out-of-core `nlos-compat-shim` adapter cell
    /// so core authorities remain free of historical protocol branches.
    OutOfCoreCompatShim {
        client_epoch: u32,
        host_epoch: u32,
        epoch_distance: u32,
    },
}

/// Complete negotiation verdict returned when a [`sabi::v1::SchemaIdentity`] passes
/// version-window and `[COMPAT-VER-002]` adjudication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionNegotiationOutcome {
    /// Registered canonical SABI channel name.
    pub channel: &'static str,
    /// Caller's declared `major` version / epoch.
    pub client_major: u32,
    /// Caller's declared `minor` version.
    pub client_minor: u32,
    /// Host's active major epoch (`N`).
    pub host_epoch: u32,
    /// Current minor version registered in [`schema_registry`].
    pub registry_minor: u32,
    /// Selected execution route (`NativeCurrentEpoch` vs `OutOfCoreCompatShim`).
    pub route: VersionRoute,
    /// Critical extension IDs verified against the receiver's supported set.
    pub accepted_critical_extensions: Vec<u32>,
    /// Non-critical extension IDs retained verbatim (both known and unknown) for
    /// round-trip preservation (`[COMPAT-VER-002]`).
    pub retained_non_critical_extensions: Vec<u32>,
}

/// Maps a [`CompatibilityError`] to a bounded, wire-safe [`sabi::v1::SabiFailure`].
///
/// All schema identity, version window (`major == 0`, `> N`, `<= N-3` sunset),
/// unknown channel, extension-count, and `[COMPAT-VER-002]` unsupported critical
/// extension rejections map deterministically to [`sabi::v1::SabiErrorCode::AbiVersion`]
/// (`E_ABI_VERSION`) with [`sabi::v1::RetryDirective::DoNotRetry`].
#[must_use]
pub fn compatibility_error_to_sabi_failure(error: &CompatibilityError) -> sabi::v1::SabiFailure {
    let (code, safe_message) = match error {
        CompatibilityError::MissingSchemaIdentity => (
            sabi::v1::SabiErrorCode::AbiVersion,
            "SABI frame is missing required SchemaIdentity",
        ),
        CompatibilityError::UnknownSchema(_) => (
            sabi::v1::SabiErrorCode::AbiVersion,
            "SABI channel schema name is not registered",
        ),
        CompatibilityError::ZeroMajorVersion { .. } => (
            sabi::v1::SabiErrorCode::AbiVersion,
            "SABI major version zero is invalid",
        ),
        CompatibilityError::UnsupportedMajor { .. } => (
            sabi::v1::SabiErrorCode::AbiVersion,
            "SABI major version exceeds supported host epoch",
        ),
        CompatibilityError::SunsetEpoch { .. } => (
            sabi::v1::SabiErrorCode::AbiVersion,
            "SABI client epoch is outside the N-2 compatibility window and has sunset",
        ),
        CompatibilityError::UnsupportedCriticalExtension { .. } => (
            sabi::v1::SabiErrorCode::AbiVersion,
            "SABI frame requires an unsupported critical extension",
        ),
        CompatibilityError::TooManyExtensionIds { .. } => (
            sabi::v1::SabiErrorCode::AbiVersion,
            "SABI frame exceeds MAX_EXTENSION_IDS bound",
        ),
        CompatibilityError::InvalidCompatibilityWindow { .. } => (
            sabi::v1::SabiErrorCode::AbiVersion,
            "SABI host compatibility policy window is below statutory minimum",
        ),
        _ => (
            sabi::v1::SabiErrorCode::InvalidArgument,
            "SABI payload failed bounded schema validation",
        ),
    };
    sabi::v1::SabiFailure {
        code: code.into(),
        retry: sabi::v1::RetryDirective::DoNotRetry.into(),
        safe_message: safe_message.to_owned(),
    }
}

/// Encodes a [`sabi::v1::ResolveServiceRequest`] within
/// [`MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES`] without pinning `major == 1`, so
/// policy-governed `N / N-1 / N-2` callers and tests can form bounded wire frames.
///
/// # Errors
///
/// Returns [`CompatibilityError::FrameTooLarge`] if the encoded frame exceeds
/// [`MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES`].
pub fn encode_resolve_service_request_frame(
    request: &sabi::v1::ResolveServiceRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    let wire = request.encode_to_vec();
    if wire.len() > MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum: MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES,
        });
    }
    Ok(wire)
}

/// Decodes a bounded [`sabi::v1::ResolveServiceRequest`] wire frame prior to
/// [`VersionCompatibilityPolicy`] adjudication.
///
/// # Errors
///
/// Returns [`CompatibilityError::FrameTooLarge`] or
/// [`CompatibilityError::MalformedProtobuf`] on invalid wire input.
pub fn decode_resolve_service_request_frame(
    wire: &[u8],
) -> Result<sabi::v1::ResolveServiceRequest, CompatibilityError> {
    if wire.len() > MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum: MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES,
        });
    }
    sabi::v1::ResolveServiceRequest::decode(wire)
        .map_err(|error| CompatibilityError::MalformedProtobuf(error.to_string()))
}

/// Encodes a [`sabi::v1::NegotiateServiceRequest`] within
/// [`MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES`] without pinning `major == 1`, so
/// policy-governed `N / N-1 / N-2` callers and tests can form bounded wire frames.
///
/// # Errors
///
/// Returns [`CompatibilityError::FrameTooLarge`] if the encoded frame exceeds
/// [`MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES`].
pub fn encode_negotiate_service_request_frame(
    request: &sabi::v1::NegotiateServiceRequest,
) -> Result<Vec<u8>, CompatibilityError> {
    let wire = request.encode_to_vec();
    if wire.len() > MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum: MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES,
        });
    }
    Ok(wire)
}

/// Decodes a bounded [`sabi::v1::NegotiateServiceRequest`] wire frame prior to
/// [`VersionCompatibilityPolicy`] adjudication.
///
/// # Errors
///
/// Returns [`CompatibilityError::FrameTooLarge`] or
/// [`CompatibilityError::MalformedProtobuf`] on invalid wire input.
pub fn decode_negotiate_service_request_frame(
    wire: &[u8],
) -> Result<sabi::v1::NegotiateServiceRequest, CompatibilityError> {
    if wire.len() > MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum: MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES,
        });
    }
    sabi::v1::NegotiateServiceRequest::decode(wire)
        .map_err(|error| CompatibilityError::MalformedProtobuf(error.to_string()))
}
