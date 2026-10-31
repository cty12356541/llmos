//! Out-of-core unprivileged SABI Epoch Adapter Cell (`[COMPAT-VER-002]`,
//! `[SABI-TRANSPORT-001]`, `[KABI-IDEM-001]`, `ADR-0027` section 2.2).
//!
//! [`SabiEpochAdapterCell`] performs pure, deterministic, bidirectional wire
//! translation between historical SABI client epochs (`N-1` / `N-2`) and the
//! canonical host epoch (`N`):
//! - upgrades/downgrades `SchemaIdentity.major` (`client_epoch <-> host_epoch`);
//! - applies declarative default field backfilling and safe error-code downgrades;
//! - preserves 128-bit `request_id` and `IdempotencyKey` byte-for-byte;
//! - preserves both `SchemaIdentity.non_critical_extension_ids` and raw unknown
//!   Protobuf tag bytes losslessly across round-trips while failing closed on
//!   any unsupported `critical_extension_ids`;
//! - operates over transport-neutral frame channels (`InMemoryFrameDuplex` and
//!   `ByteStreamFrameTransport`) with zero hardcoded socket paths or language
//!   identity assumptions;
//! - buffers structured [`DeprecationWarningReceipt`] telemetry within a fixed
//!   `MAX_SHIM_DEPRECATION_WARNINGS` (`256`) FIFO ring.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use nlos_schema::sabi::v1::{
    Envelope, RetryDirective, SabiErrorCode, SabiFailure, SabiResponseContext, SchemaIdentity,
    envelope,
};
use nlos_schema::{
    CompatibilityError, DeprecationEntry, DeprecationManifest, MAX_ACTIVITY_CONTEXT_BYTES,
    MAX_ENVELOPE_BYTES, MAX_EXTENSION_IDS, MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS, REQUEST_ID_BYTES,
    SABI_CHANNEL_CONTROL_SCHEMA, SABI_DRIVER_PROVIDER_SCHEMA, SABI_ENVELOPE_SCHEMA,
    SABI_OPERATION_CONTROL_SCHEMA, SABI_PRINCIPAL_HANDSHAKE_SCHEMA, SABI_SERVICE_DIRECTORY_SCHEMA,
    SABI_SYSTEM_CONTROL_SCHEMA, SABI_TAKEOVER_CONTROL_SCHEMA, SABI_WAIT_CONTROL_SCHEMA,
    SymbolAuditVerdict, VersionCompatibilityPolicy, canonical_deprecation_manifest,
};
use nlos_service_directory::DeprecationWarningReceipt;
use nlos_types::IdempotencyKey;
use prost::Message;

use crate::MAX_SHIM_DEPRECATION_WARNINGS;
use crate::proto_unknown::{
    ExtractedEnvelopeUnknowns, RawUnknownField, WireScanError, encode_envelope_with_unknown_fields,
    extract_envelope_unknown_fields,
};

/// Declarative translation rules applied by [`SabiEpochAdapterCell`] across an
/// `(client_epoch <-> host_epoch)` boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpochTranslationRules {
    /// Minimum `SchemaIdentity.minor` stamped onto `nlos.sabi.Envelope` when
    /// upgrading `client_epoch -> host_epoch`.
    pub host_envelope_minor: u32,
    /// Optional target `SchemaIdentity.minor` stamped onto `nlos.sabi.Envelope`
    /// when downgrading `host_epoch -> client_epoch` (`None` preserves the frame's minor).
    pub client_envelope_minor: Option<u32>,
    /// When `true`, an empty `SabiRequestContext.correlation_id` from an older
    /// client epoch is deterministically backfilled with the 16-byte `request_id`.
    pub backfill_empty_correlation_from_request_id: bool,
    /// Bounded default `activity_context` bytes backfilled when an older client
    /// frame omits `SabiRequestContext.activity_context`.
    pub default_activity_context: Vec<u8>,
    /// Declarative mapping `(modern_host_code, historical_client_code)` applied
    /// to `SabiResponseContext.failure.code` on `host_epoch -> client_epoch` egress.
    pub error_code_downgrades: Vec<(SabiErrorCode, SabiErrorCode)>,
}

impl Default for EpochTranslationRules {
    fn default() -> Self {
        Self {
            host_envelope_minor: 1,
            client_envelope_minor: Some(0),
            backfill_empty_correlation_from_request_id: true,
            default_activity_context: Vec::new(),
            error_code_downgrades: vec![
                (SabiErrorCode::Quota, SabiErrorCode::Budget),
                (SabiErrorCode::NotSupported, SabiErrorCode::InvalidArgument),
            ],
        }
    }
}

/// Validation errors for [`EpochTranslationRules`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranslationRuleError {
    /// `default_activity_context` exceeds `MAX_ACTIVITY_CONTEXT_BYTES`.
    ActivityContextDefaultTooLarge { actual: usize, maximum: usize },
    /// Attempted to map to or from `SabiErrorCode::Unspecified`.
    UnspecifiedErrorCodeMapping,
    /// Attempted to collapse `Uncertain`, `Partial`, or `EffectUnknown` into a
    /// non-uncertain error code (`[KABI-UNCERTAIN-001]`, `[SABI-TRANSPORT-001]`).
    UnsafeUncertaintyDowngrade {
        from: SabiErrorCode,
        to: SabiErrorCode,
    },
}

impl fmt::Display for TranslationRuleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActivityContextDefaultTooLarge { actual, maximum } => write!(
                formatter,
                "default activity_context length {actual} exceeds maximum {maximum}"
            ),
            Self::UnspecifiedErrorCodeMapping => {
                formatter.write_str("error-code downgrade rule must not use Unspecified")
            }
            Self::UnsafeUncertaintyDowngrade { from, to } => write!(
                formatter,
                "forbidden downgrade of uncertainty/partial code {from:?} to {to:?}"
            ),
        }
    }
}

impl Error for TranslationRuleError {}

impl EpochTranslationRules {
    /// Validates all declarative backfill bounds and error-code downgrade invariants.
    ///
    /// # Errors
    ///
    /// Fails closed if `default_activity_context` exceeds bounds or any error-code
    /// downgrade rule violates `[KABI-UNCERTAIN-001]` / `[SABI-TRANSPORT-001]`.
    pub fn validate(&self) -> Result<(), TranslationRuleError> {
        if self.default_activity_context.len() > MAX_ACTIVITY_CONTEXT_BYTES {
            return Err(TranslationRuleError::ActivityContextDefaultTooLarge {
                actual: self.default_activity_context.len(),
                maximum: MAX_ACTIVITY_CONTEXT_BYTES,
            });
        }
        for &(from, to) in &self.error_code_downgrades {
            if from == SabiErrorCode::Unspecified || to == SabiErrorCode::Unspecified {
                return Err(TranslationRuleError::UnspecifiedErrorCodeMapping);
            }
            if is_uncertainty_or_partial_code(from)
                && !is_compatible_uncertainty_downgrade(from, to)
            {
                return Err(TranslationRuleError::UnsafeUncertaintyDowngrade { from, to });
            }
        }
        Ok(())
    }

    #[must_use]
    fn map_error_code_for_client(&self, code: SabiErrorCode) -> SabiErrorCode {
        self.error_code_downgrades
            .iter()
            .find_map(|&(from, to)| (from == code).then_some(to))
            .unwrap_or(code)
    }
}

const fn is_uncertainty_or_partial_code(code: SabiErrorCode) -> bool {
    matches!(
        code,
        SabiErrorCode::Uncertain | SabiErrorCode::EffectUnknown | SabiErrorCode::Partial
    )
}

const fn is_compatible_uncertainty_downgrade(from: SabiErrorCode, to: SabiErrorCode) -> bool {
    match from {
        SabiErrorCode::Partial => matches!(to, SabiErrorCode::Partial),
        SabiErrorCode::Uncertain | SabiErrorCode::EffectUnknown => {
            matches!(to, SabiErrorCode::Uncertain | SabiErrorCode::EffectUnknown)
        }
        _ => true,
    }
}

/// Result of one deterministic wire translation through [`SabiEpochAdapterCell`].
#[derive(Clone, Debug, PartialEq)]
pub struct TranslatedWireFrame {
    /// Re-encoded target-epoch wire bytes (including all preserved unknown Protobuf fields).
    pub wire: Vec<u8>,
    /// Decoded target-epoch typed [`Envelope`] view.
    pub envelope: Envelope,
    /// Original 128-bit `request_id` verified unchanged across translation.
    pub preserved_request_id: [u8; REQUEST_ID_BYTES],
    /// Original 128-bit `IdempotencyKey` verified unchanged across translation (when present).
    pub preserved_idempotency_key: Option<IdempotencyKey>,
    /// Non-critical extension IDs preserved verbatim on `SchemaIdentity`.
    pub preserved_non_critical_extensions: Vec<u32>,
    /// Extracted raw unknown Protobuf wire fields preserved verbatim on output.
    pub preserved_unknown_fields: ExtractedEnvelopeUnknowns,
    /// Deprecation warnings emitted during this frame's translation.
    pub emitted_warnings: Vec<DeprecationWarningReceipt>,
}

/// Fail-closed errors returned by [`SabiEpochAdapterCell`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SabiAdapterError {
    /// Invalid `(client_epoch, host_epoch)` adapter configuration.
    InvalidEpochPair {
        client_epoch: u32,
        host_epoch: u32,
        max_backward_epochs: u32,
    },
    /// Declarative translation rule violated bounds or uncertainty preservation.
    InvalidRules(TranslationRuleError),
    /// Wire scanning detected malformed or truncated Protobuf tag bytes.
    WireScan(WireScanError),
    /// Frame failed `VersionCompatibilityPolicy` or envelope schema checks.
    Compatibility(CompatibilityError),
    /// Incoming frame's `SchemaIdentity.major` did not match the expected side's epoch.
    EpochMismatch { expected: u32, actual: u32 },
    /// `SabiRequestContext.idempotency_key` was present with a width other than 16 bytes.
    InvalidIdempotencyKeyLength { actual: usize },
    /// Referenced service, method, or wire field slot is `SUNSET` or permanently `RESERVED`.
    SunsetOrReservedContract {
        channel: String,
        symbol: String,
        field_number: Option<u32>,
        reason: &'static str,
    },
    /// Internal defensive post-condition check failed (`request_id` or `IdempotencyKey` drift).
    IdempotencyInvariantViolation,
}

impl fmt::Display for SabiAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEpochPair {
                client_epoch,
                host_epoch,
                max_backward_epochs,
            } => write!(
                formatter,
                "invalid SABI adapter epoch pair client={client_epoch}, host={host_epoch} (window={max_backward_epochs})"
            ),
            Self::InvalidRules(err) => write!(formatter, "invalid translation rules: {err}"),
            Self::WireScan(err) => write!(formatter, "protobuf wire scan error: {err}"),
            Self::Compatibility(err) => write!(formatter, "SABI compatibility error: {err}"),
            Self::EpochMismatch { expected, actual } => write!(
                formatter,
                "envelope major epoch {actual} did not match expected adapter epoch {expected}"
            ),
            Self::InvalidIdempotencyKeyLength { actual } => write!(
                formatter,
                "idempotency_key has {actual} bytes; 0 or {REQUEST_ID_BYTES} bytes required"
            ),
            Self::SunsetOrReservedContract {
                channel,
                symbol,
                field_number: Some(field),
                reason,
            } => write!(
                formatter,
                "retired contract element {symbol:?} (field {field}) in {channel:?}: {reason}"
            ),
            Self::SunsetOrReservedContract {
                channel,
                symbol,
                field_number: None,
                reason,
            } => write!(
                formatter,
                "retired contract element {symbol:?} in {channel:?}: {reason}"
            ),
            Self::IdempotencyInvariantViolation => {
                formatter.write_str("128-bit request_id or idempotency_key invariant failed")
            }
        }
    }
}

impl Error for SabiAdapterError {}

impl SabiAdapterError {
    /// Maps any fail-closed [`SabiAdapterError`] to a bounded [`SabiFailure`].
    ///
    /// Version-window mismatches, unsupported critical extensions, and `SUNSET` /
    /// `RESERVED` contract references deterministically produce
    /// [`SabiErrorCode::AbiVersion`] (`E_ABI_VERSION`) with
    /// [`RetryDirective::DoNotRetry`].
    #[must_use]
    pub fn to_sabi_failure(&self) -> SabiFailure {
        match self {
            Self::Compatibility(err) => err.to_sabi_failure(),
            Self::InvalidEpochPair { .. }
            | Self::EpochMismatch { .. }
            | Self::SunsetOrReservedContract { .. } => SabiFailure {
                code: SabiErrorCode::AbiVersion.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message:
                    "SABI epoch, critical extension, or sunset contract rejected by compat shim"
                        .to_owned(),
            },
            Self::WireScan(_)
            | Self::InvalidIdempotencyKeyLength { .. }
            | Self::InvalidRules(_)
            | Self::IdempotencyInvariantViolation => SabiFailure {
                code: SabiErrorCode::InvalidArgument.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "malformed SABI wire frame rejected by compat shim".to_owned(),
            },
        }
    }
}

/// Unprivileged out-of-core SABI epoch translation cell (`ADR-0027` section 2.2).
///
/// Holds zero private keys, zero raw credentials, and zero authority database
/// handles. All authority decisions remain strictly inside core authorities and
/// the TCB-edge enforcement gate.
#[derive(Clone, Debug)]
pub struct SabiEpochAdapterCell {
    client_epoch: u32,
    host_epoch: u32,
    max_backward_epochs: u32,
    known_critical_extensions: Vec<u32>,
    manifest: DeprecationManifest,
    rules: EpochTranslationRules,
    deprecation_warnings: VecDeque<DeprecationWarningReceipt>,
}

impl SabiEpochAdapterCell {
    /// Creates a new out-of-core SABI epoch adapter cell bridging
    /// `client_epoch` (`N-1` or `N-2`) <-> `host_epoch` (`N`).
    ///
    /// # Errors
    ///
    /// Fails closed if `client_epoch == 0`, `host_epoch == 0`,
    /// `max_backward_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS`,
    /// `client_epoch >= host_epoch`, `host_epoch - client_epoch > max_backward_epochs`,
    /// `known_critical_extensions` violates bounds, or `rules` is invalid.
    pub fn new(
        client_epoch: u32,
        host_epoch: u32,
        max_backward_epochs: u32,
        known_critical_extensions: Vec<u32>,
        manifest: DeprecationManifest,
        rules: EpochTranslationRules,
    ) -> Result<Self, SabiAdapterError> {
        if max_backward_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS {
            return Err(SabiAdapterError::Compatibility(
                CompatibilityError::InvalidCompatibilityWindow {
                    got: max_backward_epochs,
                    minimum: MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
                },
            ));
        }
        if client_epoch == 0 || host_epoch == 0 || client_epoch >= host_epoch {
            return Err(SabiAdapterError::InvalidEpochPair {
                client_epoch,
                host_epoch,
                max_backward_epochs,
            });
        }
        if host_epoch - client_epoch > max_backward_epochs {
            return Err(SabiAdapterError::Compatibility(
                CompatibilityError::SunsetEpoch {
                    schema: SABI_ENVELOPE_SCHEMA.to_owned(),
                    client_epoch,
                    host_epoch,
                    max_backward_epochs,
                },
            ));
        }
        if known_critical_extensions.len() > MAX_EXTENSION_IDS {
            return Err(SabiAdapterError::Compatibility(
                CompatibilityError::TooManyExtensionIds {
                    schema: SABI_ENVELOPE_SCHEMA.to_owned(),
                    actual: known_critical_extensions.len(),
                    maximum: MAX_EXTENSION_IDS,
                },
            ));
        }
        if known_critical_extensions.contains(&0) {
            return Err(SabiAdapterError::Compatibility(
                CompatibilityError::UnsupportedCriticalExtension {
                    schema: SABI_ENVELOPE_SCHEMA.to_owned(),
                    extension_id: 0,
                },
            ));
        }
        rules.validate().map_err(SabiAdapterError::InvalidRules)?;

        Ok(Self {
            client_epoch,
            host_epoch,
            max_backward_epochs,
            known_critical_extensions,
            manifest,
            rules,
            deprecation_warnings: VecDeque::new(),
        })
    }

    /// Convenience constructor using the canonical embedded SABI deprecation
    /// manifest (`CANONICAL_DEPRECATION_MANIFEST_YAML`) and default rules.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid epoch pair (`client_epoch` outside `N-1..=N-2`).
    ///
    /// # Panics
    ///
    /// Panics only if the compile-time embedded canonical deprecation manifest is corrupted.
    pub fn with_canonical_manifest(
        client_epoch: u32,
        host_epoch: u32,
    ) -> Result<Self, SabiAdapterError> {
        let manifest = canonical_deprecation_manifest().expect("canonical manifest is valid");
        Self::new(
            client_epoch,
            host_epoch,
            MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
            Vec::new(),
            manifest,
            EpochTranslationRules::default(),
        )
    }

    /// Historical client major epoch served by this adapter (`N-1` or `N-2`).
    #[must_use]
    pub const fn client_epoch(&self) -> u32 {
        self.client_epoch
    }

    /// Current host major epoch targeted by this adapter (`N`).
    #[must_use]
    pub const fn host_epoch(&self) -> u32 {
        self.host_epoch
    }

    /// Epoch distance (`host_epoch - client_epoch`, in `1..=max_backward_epochs`).
    #[must_use]
    pub const fn epoch_distance(&self) -> u32 {
        self.host_epoch - self.client_epoch
    }

    /// Active declarative translation rules.
    #[must_use]
    pub const fn rules(&self) -> &EpochTranslationRules {
        &self.rules
    }

    /// Current number of buffered [`DeprecationWarningReceipt`] items (`<= 256`).
    #[must_use]
    pub fn deprecation_warning_count(&self) -> usize {
        self.deprecation_warnings.len()
    }

    /// Returns all buffered [`DeprecationWarningReceipt`]s in FIFO order.
    #[must_use]
    pub fn deprecation_warnings(&self) -> Vec<DeprecationWarningReceipt> {
        self.deprecation_warnings.iter().cloned().collect()
    }

    /// Drains and returns all buffered [`DeprecationWarningReceipt`]s in FIFO order.
    pub fn drain_deprecation_warnings(&mut self) -> Vec<DeprecationWarningReceipt> {
        self.deprecation_warnings.drain(..).collect()
    }

    /// Translates an incoming historical client wire frame (`client_epoch`) into
    /// a canonical host-epoch (`host_epoch`) SABI [`Envelope`] wire frame.
    ///
    /// # Errors
    ///
    /// Fails closed on oversized/malformed Protobuf bytes, mismatched client
    /// major epoch, unsupported `critical_extension_ids` (`[COMPAT-VER-002]`),
    /// invalid 128-bit `request_id`/`idempotency_key`, or `SUNSET`/`RESERVED`
    /// contract elements.
    pub fn translate_client_to_host_wire(
        &mut self,
        client_wire: &[u8],
    ) -> Result<TranslatedWireFrame, SabiAdapterError> {
        let unknowns =
            extract_envelope_unknown_fields(client_wire).map_err(SabiAdapterError::WireScan)?;
        let mut envelope = Envelope::decode(client_wire).map_err(|err| {
            SabiAdapterError::Compatibility(CompatibilityError::MalformedProtobuf(err.to_string()))
        })?;

        let policy = VersionCompatibilityPolicy::with_backward_window(
            self.host_epoch,
            self.max_backward_epochs,
            &self.known_critical_extensions,
        );
        let negotiation = policy
            .adjudicate_envelope(&envelope)
            .map_err(SabiAdapterError::Compatibility)?;

        if negotiation.client_major != self.client_epoch {
            return Err(SabiAdapterError::EpochMismatch {
                expected: self.client_epoch,
                actual: negotiation.client_major,
            });
        }

        let preserved_request_id = snapshot_request_id(&envelope)?;
        let preserved_idempotency_key = snapshot_idempotency_key(&envelope)?;

        let emitted_warnings = self.audit_envelope_and_unknowns(
            &envelope,
            &unknowns,
            negotiation.client_major,
            negotiation.client_minor,
        )?;

        if let Some(schema) = envelope.schema.as_mut() {
            schema.major = self.host_epoch;
            schema.minor = schema.minor.max(self.rules.host_envelope_minor);
        }

        if let Some(envelope::CommonContext::RequestContext(req_ctx)) =
            envelope.common_context.as_mut()
        {
            if self.rules.backfill_empty_correlation_from_request_id
                && req_ctx.correlation_id.is_empty()
            {
                req_ctx.correlation_id = preserved_request_id.to_vec();
            }
            if req_ctx.activity_context.is_empty()
                && !self.rules.default_activity_context.is_empty()
            {
                req_ctx
                    .activity_context
                    .clone_from(&self.rules.default_activity_context);
            }
        }

        let wire = encode_envelope_with_unknown_fields(&envelope, &unknowns)
            .map_err(SabiAdapterError::WireScan)?;
        verify_post_translation_invariants(
            &wire,
            preserved_request_id,
            preserved_idempotency_key,
            &negotiation.retained_non_critical_extensions,
        )?;

        self.push_warnings(&emitted_warnings);

        Ok(TranslatedWireFrame {
            wire,
            envelope,
            preserved_request_id,
            preserved_idempotency_key,
            preserved_non_critical_extensions: negotiation.retained_non_critical_extensions,
            preserved_unknown_fields: unknowns,
            emitted_warnings,
        })
    }

    /// Translates an outgoing canonical host wire frame (`host_epoch`) back into
    /// the historical client's target epoch (`client_epoch`).
    ///
    /// # Errors
    ///
    /// Fails closed on oversized/malformed Protobuf bytes, mismatched host major
    /// epoch, unsupported `critical_extension_ids`, invalid 128-bit `request_id`
    /// or `idempotency_key`, or `SUNSET`/`RESERVED` contract elements.
    pub fn translate_host_to_client_wire(
        &mut self,
        host_wire: &[u8],
    ) -> Result<TranslatedWireFrame, SabiAdapterError> {
        let unknowns =
            extract_envelope_unknown_fields(host_wire).map_err(SabiAdapterError::WireScan)?;
        let mut envelope = Envelope::decode(host_wire).map_err(|err| {
            SabiAdapterError::Compatibility(CompatibilityError::MalformedProtobuf(err.to_string()))
        })?;

        let policy = VersionCompatibilityPolicy::with_backward_window(
            self.host_epoch,
            self.max_backward_epochs,
            &self.known_critical_extensions,
        );
        let negotiation = policy
            .adjudicate_envelope(&envelope)
            .map_err(SabiAdapterError::Compatibility)?;

        if negotiation.client_major != self.host_epoch {
            return Err(SabiAdapterError::EpochMismatch {
                expected: self.host_epoch,
                actual: negotiation.client_major,
            });
        }

        let preserved_request_id = snapshot_request_id(&envelope)?;
        let preserved_idempotency_key = snapshot_idempotency_key(&envelope)?;

        let emitted_warnings = self.audit_envelope_and_unknowns(
            &envelope,
            &unknowns,
            self.client_epoch,
            self.rules
                .client_envelope_minor
                .unwrap_or(negotiation.client_minor),
        )?;

        if let Some(schema) = envelope.schema.as_mut() {
            schema.major = self.client_epoch;
            if let Some(target_minor) = self.rules.client_envelope_minor {
                schema.minor = target_minor;
            }
        }

        if let Some(envelope::CommonContext::ResponseContext(resp_ctx)) =
            envelope.common_context.as_mut()
            && let Some(failure) = resp_ctx.failure.as_mut()
            && let Ok(parsed_code) = SabiErrorCode::try_from(failure.code)
        {
            let downgraded = self.rules.map_error_code_for_client(parsed_code);
            failure.code = downgraded.into();
        }

        let wire = encode_envelope_with_unknown_fields(&envelope, &unknowns)
            .map_err(SabiAdapterError::WireScan)?;
        verify_post_translation_invariants(
            &wire,
            preserved_request_id,
            preserved_idempotency_key,
            &negotiation.retained_non_critical_extensions,
        )?;

        self.push_warnings(&emitted_warnings);

        Ok(TranslatedWireFrame {
            wire,
            envelope,
            preserved_request_id,
            preserved_idempotency_key,
            preserved_non_critical_extensions: negotiation.retained_non_critical_extensions,
            preserved_unknown_fields: unknowns,
            emitted_warnings,
        })
    }

    pub(crate) fn push_warnings(&mut self, warnings: &[DeprecationWarningReceipt]) {
        for warning in warnings {
            if self.deprecation_warnings.len() >= MAX_SHIM_DEPRECATION_WARNINGS {
                self.deprecation_warnings.pop_front();
            }
            self.deprecation_warnings.push_back(warning.clone());
        }
    }

    fn audit_envelope_and_unknowns(
        &self,
        envelope: &Envelope,
        unknowns: &ExtractedEnvelopeUnknowns,
        caller_major: u32,
        caller_minor: u32,
    ) -> Result<Vec<DeprecationWarningReceipt>, SabiAdapterError> {
        let mut warnings = Vec::new();

        // Audit service and method symbols across both the mapped service channel
        // (if known) and all registered channels in the deprecation manifest.
        if let Some(service_channel) = map_service_to_channel(&envelope.service) {
            record_verdict(
                self.manifest
                    .audit_symbol(service_channel, &envelope.service),
                service_channel,
                &envelope.service,
                None,
                caller_major,
                caller_minor,
                &envelope.service,
                &mut warnings,
            )?;
            record_verdict(
                self.manifest
                    .audit_symbol(service_channel, &envelope.method),
                service_channel,
                &envelope.method,
                None,
                caller_major,
                caller_minor,
                &envelope.service,
                &mut warnings,
            )?;
            let qualified = format!("{}.{}", envelope.service, envelope.method);
            record_verdict(
                self.manifest.audit_symbol(service_channel, &qualified),
                service_channel,
                &qualified,
                None,
                caller_major,
                caller_minor,
                &envelope.service,
                &mut warnings,
            )?;
        } else {
            for channel in &self.manifest.channels {
                record_verdict(
                    self.manifest.audit_symbol(&channel.name, &envelope.service),
                    &channel.name,
                    &envelope.service,
                    None,
                    caller_major,
                    caller_minor,
                    &envelope.service,
                    &mut warnings,
                )?;
                let qualified = format!("{}.{}", envelope.service, envelope.method);
                record_verdict(
                    self.manifest.audit_symbol(&channel.name, &qualified),
                    &channel.name,
                    &qualified,
                    None,
                    caller_major,
                    caller_minor,
                    &envelope.service,
                    &mut warnings,
                )?;
            }
        }

        // Audit any unknown wire field numbers on Envelope and its standard submessages.
        audit_unknown_field_slice(
            &self.manifest,
            "Envelope",
            &unknowns.envelope_fields,
            caller_major,
            caller_minor,
            &envelope.service,
            &mut warnings,
        )?;
        audit_unknown_field_slice(
            &self.manifest,
            "SchemaIdentity",
            &unknowns.schema_fields,
            caller_major,
            caller_minor,
            &envelope.service,
            &mut warnings,
        )?;
        audit_unknown_field_slice(
            &self.manifest,
            "SabiRequestContext",
            &unknowns.request_context_fields,
            caller_major,
            caller_minor,
            &envelope.service,
            &mut warnings,
        )?;
        audit_unknown_field_slice(
            &self.manifest,
            "SabiResponseContext",
            &unknowns.response_context_fields,
            caller_major,
            caller_minor,
            &envelope.service,
            &mut warnings,
        )?;

        Ok(warnings)
    }
}

fn audit_unknown_field_slice(
    manifest: &DeprecationManifest,
    parent_type: &str,
    fields: &[RawUnknownField],
    caller_major: u32,
    caller_minor: u32,
    service: &str,
    warnings: &mut Vec<DeprecationWarningReceipt>,
) -> Result<(), SabiAdapterError> {
    for field in fields {
        let verdict = manifest.audit_field_number(
            SABI_ENVELOPE_SCHEMA,
            Some(parent_type),
            field.field_number,
        );
        let fallback_symbol = format!("{parent_type}.field:{}", field.field_number);
        record_verdict(
            verdict,
            SABI_ENVELOPE_SCHEMA,
            &fallback_symbol,
            Some(field.field_number),
            caller_major,
            caller_minor,
            service,
            warnings,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_verdict(
    verdict: SymbolAuditVerdict<'_>,
    channel: &str,
    fallback_symbol: &str,
    fallback_field_number: Option<u32>,
    caller_major: u32,
    caller_minor: u32,
    service: &str,
    warnings: &mut Vec<DeprecationWarningReceipt>,
) -> Result<(), SabiAdapterError> {
    match verdict {
        SymbolAuditVerdict::Active(_) => Ok(()),
        SymbolAuditVerdict::Deprecated(entry) => {
            warnings.push(entry_to_warning_receipt(
                entry,
                caller_major,
                caller_minor,
                service,
            ));
            Ok(())
        }
        SymbolAuditVerdict::SunsetOrReserved { entry, reason } => {
            let (symbol, field_number) = entry.map_or_else(
                || (fallback_symbol.to_owned(), fallback_field_number),
                |item| {
                    (
                        item.symbol.clone(),
                        item.field_number.or(fallback_field_number),
                    )
                },
            );
            Err(SabiAdapterError::SunsetOrReservedContract {
                channel: channel.to_owned(),
                symbol,
                field_number,
                reason,
            })
        }
    }
}

pub(crate) fn entry_to_warning_receipt(
    entry: &DeprecationEntry,
    caller_major: u32,
    caller_minor: u32,
    service: &str,
) -> DeprecationWarningReceipt {
    DeprecationWarningReceipt {
        channel: entry.channel.clone(),
        symbol: entry.symbol.clone(),
        field_number: entry.field_number,
        deprecated_in_epoch: entry
            .deprecated_in_epoch
            .unwrap_or(entry.introduced_in_epoch),
        earliest_sunset_epoch: entry
            .earliest_sunset_epoch
            .unwrap_or(entry.introduced_in_epoch),
        replacement: entry.replacement.clone().unwrap_or_default(),
        caller_major,
        caller_minor,
        service: service.to_owned(),
    }
}

#[must_use]
pub(crate) fn map_service_to_channel(service: &str) -> Option<&'static str> {
    match service {
        "service_directory" | SABI_SERVICE_DIRECTORY_SCHEMA => Some(SABI_SERVICE_DIRECTORY_SCHEMA),
        "operation_control" | SABI_OPERATION_CONTROL_SCHEMA => Some(SABI_OPERATION_CONTROL_SCHEMA),
        "system_control" | SABI_SYSTEM_CONTROL_SCHEMA => Some(SABI_SYSTEM_CONTROL_SCHEMA),
        "takeover_control" | SABI_TAKEOVER_CONTROL_SCHEMA => Some(SABI_TAKEOVER_CONTROL_SCHEMA),
        "wait_control" | SABI_WAIT_CONTROL_SCHEMA => Some(SABI_WAIT_CONTROL_SCHEMA),
        "principal_handshake" | SABI_PRINCIPAL_HANDSHAKE_SCHEMA => {
            Some(SABI_PRINCIPAL_HANDSHAKE_SCHEMA)
        }
        "driver_provider" | SABI_DRIVER_PROVIDER_SCHEMA => Some(SABI_DRIVER_PROVIDER_SCHEMA),
        "channel_control" | SABI_CHANNEL_CONTROL_SCHEMA => Some(SABI_CHANNEL_CONTROL_SCHEMA),
        SABI_ENVELOPE_SCHEMA => Some(SABI_ENVELOPE_SCHEMA),
        _ => None,
    }
}

fn snapshot_request_id(envelope: &Envelope) -> Result<[u8; REQUEST_ID_BYTES], SabiAdapterError> {
    envelope.request_id.as_slice().try_into().map_err(|_| {
        SabiAdapterError::Compatibility(CompatibilityError::InvalidRequestIdLength {
            actual: envelope.request_id.len(),
        })
    })
}

fn snapshot_idempotency_key(
    envelope: &Envelope,
) -> Result<Option<IdempotencyKey>, SabiAdapterError> {
    match &envelope.common_context {
        Some(envelope::CommonContext::RequestContext(req_ctx))
            if !req_ctx.idempotency_key.is_empty() =>
        {
            let bytes: [u8; REQUEST_ID_BYTES] =
                req_ctx.idempotency_key.as_slice().try_into().map_err(|_| {
                    SabiAdapterError::InvalidIdempotencyKeyLength {
                        actual: req_ctx.idempotency_key.len(),
                    }
                })?;
            Ok(Some(IdempotencyKey::from_bytes(bytes)))
        }
        _ => Ok(None),
    }
}

fn verify_post_translation_invariants(
    wire: &[u8],
    expected_request_id: [u8; REQUEST_ID_BYTES],
    expected_idempotency_key: Option<IdempotencyKey>,
    expected_non_critical: &[u32],
) -> Result<(), SabiAdapterError> {
    let decoded = Envelope::decode(wire).map_err(|err| {
        SabiAdapterError::Compatibility(CompatibilityError::MalformedProtobuf(err.to_string()))
    })?;
    let actual_request_id = snapshot_request_id(&decoded)?;
    let actual_idempotency_key = snapshot_idempotency_key(&decoded)?;
    let non_critical_matches = decoded
        .schema
        .as_ref()
        .is_some_and(|schema| schema.non_critical_extension_ids == expected_non_critical);
    if actual_request_id != expected_request_id
        || actual_idempotency_key != expected_idempotency_key
        || !non_critical_matches
    {
        return Err(SabiAdapterError::IdempotencyInvariantViolation);
    }
    Ok(())
}

/// Transport-neutral frame channel errors (`[SABI-TRANSPORT-001]`).
#[derive(Debug)]
pub enum TransportBridgeError {
    /// Underlying byte stream or queue I/O failure.
    Io(io::Error),
    /// Frame exceeded `MAX_ENVELOPE_BYTES` (`1 MiB`).
    FrameTooLarge { actual: usize, maximum: usize },
    /// Peer disconnected cleanly or closed the channel while sending.
    ChannelClosed,
    /// Adapter rejected an outbound host frame (`host -> client` direction).
    HostFrameRejected(SabiAdapterError),
}

impl fmt::Display for TransportBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(formatter, "frame transport I/O error: {err}"),
            Self::FrameTooLarge { actual, maximum } => write!(
                formatter,
                "transport frame length {actual} exceeds maximum {maximum}"
            ),
            Self::ChannelClosed => formatter.write_str("frame transport channel is closed"),
            Self::HostFrameRejected(err) => {
                write!(formatter, "host egress frame failed translation: {err}")
            }
        }
    }
}

impl Error for TransportBridgeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::HostFrameRejected(err) => Some(err),
            Self::FrameTooLarge { .. } | Self::ChannelClosed => None,
        }
    }
}

impl From<io::Error> for TransportBridgeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Transport-neutral duplex wire-frame channel (`[SABI-TRANSPORT-001]`).
///
/// Implementations never embed fixed socket paths or assume implementation
/// language identity; callers may bind in-memory queues, OS pipes, Unix
/// domain streams, WASI host streams, or mTLS byte streams.
pub trait DuplexFrameChannel {
    /// Sends one bounded raw SABI wire frame.
    ///
    /// # Errors
    ///
    /// Returns [`TransportBridgeError`] if `frame.len() > MAX_ENVELOPE_BYTES`
    /// or the underlying transport fails.
    fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportBridgeError>;

    /// Receives the next bounded raw SABI wire frame, or `Ok(None)` when no
    /// frame is currently queued / the stream reached clean EOF at a frame boundary.
    ///
    /// # Errors
    ///
    /// Returns [`TransportBridgeError`] on truncated frames, oversized length
    /// headers, or transport I/O errors.
    fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportBridgeError>;
}

#[derive(Debug, Default)]
struct SharedQueue {
    frames: VecDeque<Vec<u8>>,
    closed: bool,
}

/// One endpoint of an in-memory bidirectional frame queue (`[SABI-TRANSPORT-001]`).
#[derive(Clone, Debug)]
pub struct InMemoryFrameDuplex {
    inbound: Arc<Mutex<SharedQueue>>,
    outbound: Arc<Mutex<SharedQueue>>,
}

impl InMemoryFrameDuplex {
    /// Creates a connected bidirectional pair `(endpoint_a, endpoint_b)` where
    /// frames sent on `endpoint_a` arrive on `endpoint_b` and vice versa.
    #[must_use]
    pub fn pair() -> (Self, Self) {
        let a_to_b = Arc::new(Mutex::new(SharedQueue::default()));
        let b_to_a = Arc::new(Mutex::new(SharedQueue::default()));
        let endpoint_a = Self {
            inbound: Arc::clone(&b_to_a),
            outbound: Arc::clone(&a_to_b),
        };
        let endpoint_b = Self {
            inbound: a_to_b,
            outbound: b_to_a,
        };
        (endpoint_a, endpoint_b)
    }

    /// Closes the outbound queue to simulate peer disconnect or transport crash.
    pub fn close_outbound(&self) {
        let mut guard = self
            .outbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.closed = true;
    }
}

impl DuplexFrameChannel for InMemoryFrameDuplex {
    fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportBridgeError> {
        if frame.len() > MAX_ENVELOPE_BYTES {
            return Err(TransportBridgeError::FrameTooLarge {
                actual: frame.len(),
                maximum: MAX_ENVELOPE_BYTES,
            });
        }
        let mut guard = self
            .outbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.closed {
            return Err(TransportBridgeError::ChannelClosed);
        }
        guard.frames.push_back(frame.to_vec());
        Ok(())
    }

    fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportBridgeError> {
        let mut guard = self
            .inbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(frame) = guard.frames.pop_front() {
            return Ok(Some(frame));
        }
        if guard.closed {
            return Err(TransportBridgeError::ChannelClosed);
        }
        Ok(None)
    }
}

/// Transport-neutral 4-byte big-endian length-prefixed byte stream framing
/// over any `Read + Write` pair (`[SABI-TRANSPORT-001]`).
#[derive(Debug)]
pub struct ByteStreamFrameTransport<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> ByteStreamFrameTransport<R, W> {
    /// Wraps an arbitrary `(reader, writer)` byte stream pair.
    #[must_use]
    pub const fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    /// Consumes the transport and returns the underlying `(reader, writer)`.
    #[must_use]
    pub fn into_parts(self) -> (R, W) {
        (self.reader, self.writer)
    }
}

impl<R: Read, W: Write> DuplexFrameChannel for ByteStreamFrameTransport<R, W> {
    fn send_frame(&mut self, frame: &[u8]) -> Result<(), TransportBridgeError> {
        if frame.len() > MAX_ENVELOPE_BYTES {
            return Err(TransportBridgeError::FrameTooLarge {
                actual: frame.len(),
                maximum: MAX_ENVELOPE_BYTES,
            });
        }
        let len_u32 =
            u32::try_from(frame.len()).map_err(|_| TransportBridgeError::FrameTooLarge {
                actual: frame.len(),
                maximum: MAX_ENVELOPE_BYTES,
            })?;
        self.writer.write_all(&len_u32.to_be_bytes())?;
        self.writer.write_all(frame)?;
        self.writer.flush()?;
        Ok(())
    }

    fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportBridgeError> {
        let mut header = [0_u8; 4];
        let mut read_header = 0_usize;
        while read_header < 4 {
            match self.reader.read(&mut header[read_header..]) {
                Ok(0) if read_header == 0 => return Ok(None),
                Ok(0) => {
                    return Err(TransportBridgeError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated 4-byte SABI frame length header",
                    )));
                }
                Ok(count) => read_header += count,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(TransportBridgeError::Io(err)),
            }
        }
        let frame_len = usize::try_from(u32::from_be_bytes(header)).unwrap_or(usize::MAX);
        if frame_len > MAX_ENVELOPE_BYTES {
            return Err(TransportBridgeError::FrameTooLarge {
                actual: frame_len,
                maximum: MAX_ENVELOPE_BYTES,
            });
        }
        let mut payload = vec![0_u8; frame_len];
        self.reader.read_exact(&mut payload)?;
        Ok(Some(payload))
    }
}

/// Outcome of pumping one frame across [`SabiShimBridge`].
#[derive(Clone, Debug, PartialEq)]
pub enum BridgePumpOutcome {
    /// No frame was available on the source channel.
    Idle,
    /// Frame was translated and forwarded to the destination channel.
    Forwarded(Box<TranslatedWireFrame>),
    /// Client ingress frame failed validation and a fail-closed `SabiFailure`
    /// response envelope was returned directly to the client channel.
    RejectedToClient {
        error: SabiAdapterError,
        failure: SabiFailure,
    },
}

/// Bidirectional transport-neutral duplex bridge connecting a historical-epoch
/// client channel to a host-epoch channel through [`SabiEpochAdapterCell`].
#[derive(Debug)]
pub struct SabiShimBridge<C, H> {
    adapter: SabiEpochAdapterCell,
    client_channel: C,
    host_channel: H,
}

impl<C, H> SabiShimBridge<C, H> {
    /// Binds an unprivileged [`SabiEpochAdapterCell`] between `client_channel`
    /// and `host_channel`.
    #[must_use]
    pub const fn new(adapter: SabiEpochAdapterCell, client_channel: C, host_channel: H) -> Self {
        Self {
            adapter,
            client_channel,
            host_channel,
        }
    }

    /// Borrows the underlying [`SabiEpochAdapterCell`].
    #[must_use]
    pub const fn adapter(&self) -> &SabiEpochAdapterCell {
        &self.adapter
    }

    /// Mutably borrows the underlying [`SabiEpochAdapterCell`].
    pub fn adapter_mut(&mut self) -> &mut SabiEpochAdapterCell {
        &mut self.adapter
    }
}

impl<C: DuplexFrameChannel, H: DuplexFrameChannel> SabiShimBridge<C, H> {
    /// Pumps at most one client ingress frame (`client_epoch -> host_epoch`).
    ///
    /// If the client frame violates version, critical-extension, or sunset
    /// rules, the frame is never forwarded to `host_channel`; instead, a
    /// fail-closed `SabiFailure` (`E_ABI_VERSION`) response envelope at
    /// `client_epoch` is sent back on `client_channel`.
    ///
    /// # Errors
    ///
    /// Returns [`TransportBridgeError`] if reading from `client_channel` or
    /// writing to `host_channel` / `client_channel` fails.
    pub fn pump_client_to_host_once(&mut self) -> Result<BridgePumpOutcome, TransportBridgeError> {
        let Some(raw_client_frame) = self.client_channel.recv_frame()? else {
            return Ok(BridgePumpOutcome::Idle);
        };
        match self
            .adapter
            .translate_client_to_host_wire(&raw_client_frame)
        {
            Ok(translated) => {
                self.host_channel.send_frame(&translated.wire)?;
                Ok(BridgePumpOutcome::Forwarded(Box::new(translated)))
            }
            Err(error) => {
                let failure = error.to_sabi_failure();
                let rejection_envelope = synthesize_client_rejection_envelope(
                    &raw_client_frame,
                    self.adapter.client_epoch(),
                    failure.clone(),
                );
                self.client_channel
                    .send_frame(&rejection_envelope.encode_to_vec())?;
                Ok(BridgePumpOutcome::RejectedToClient { error, failure })
            }
        }
    }

    /// Pumps at most one host egress frame (`host_epoch -> client_epoch`).
    ///
    /// # Errors
    ///
    /// Returns [`TransportBridgeError`] on transport failure or invalid host frame.
    pub fn pump_host_to_client_once(&mut self) -> Result<BridgePumpOutcome, TransportBridgeError> {
        let Some(raw_host_frame) = self.host_channel.recv_frame()? else {
            return Ok(BridgePumpOutcome::Idle);
        };
        let translated = self
            .adapter
            .translate_host_to_client_wire(&raw_host_frame)
            .map_err(TransportBridgeError::HostFrameRejected)?;
        self.client_channel.send_frame(&translated.wire)?;
        Ok(BridgePumpOutcome::Forwarded(Box::new(translated)))
    }
}

fn synthesize_client_rejection_envelope(
    raw_client_frame: &[u8],
    client_epoch: u32,
    failure: SabiFailure,
) -> Envelope {
    let decoded = Envelope::decode(raw_client_frame).ok();
    let request_id = decoded
        .as_ref()
        .filter(|env| env.request_id.len() == REQUEST_ID_BYTES)
        .map_or_else(
            || vec![0_u8; REQUEST_ID_BYTES],
            |env| env.request_id.clone(),
        );
    let service = decoded
        .as_ref()
        .filter(|env| !env.service.is_empty())
        .map_or_else(|| "compat_shim".to_owned(), |env| env.service.clone());
    let method = decoded
        .as_ref()
        .filter(|env| !env.method.is_empty())
        .map_or_else(|| "reject".to_owned(), |env| env.method.clone());
    let correlation_id = decoded
        .as_ref()
        .and_then(|env| match &env.common_context {
            Some(envelope::CommonContext::RequestContext(ctx))
                if !ctx.correlation_id.is_empty() =>
            {
                Some(ctx.correlation_id.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| request_id.clone());

    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: client_epoch.max(1),
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id,
        service,
        method,
        common_context: Some(envelope::CommonContext::ResponseContext(
            SabiResponseContext {
                correlation_id,
                operation: None,
                receipts: Vec::new(),
                failure: Some(failure),
            },
        )),
        payload: Vec::new(),
    }
}
