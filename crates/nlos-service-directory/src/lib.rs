//! Deterministic in-memory `ServiceDirectory` snapshot, `N / N-1 / N-2` policy
//! negotiation, and `ADR-0027` deprecation warning telemetry.
//!
//! The bootstrap endpoint or handle is supplied by a trusted Namespace. This
//! crate resolves names, enforces `VersionCompatibilityPolicy` and
//! `DeprecationManifest` contracts fail-closed, emits bounded
//! [`DeprecationWarningReceipt`] telemetry for `DEPRECATED` contracts without
//! interrupting traffic, and never invents a fixed socket path or treats an
//! implementation language as service identity.

#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use nlos_schema::sabi::v1::{
    DirectoryError, DirectoryErrorCode, LocalEndpoint, LocalTransportKind, NegotiateServiceRequest,
    NegotiateServiceResponse, ResolveServiceRequest, ResolveServiceResponse, RetryDirective,
    SabiErrorCode, SabiFailure, SchemaIdentity, ServiceBinding, ServiceCandidate,
    ServiceCandidateSet, negotiate_service_response, resolve_service_response,
};
use nlos_schema::{
    CompatibilityError, DeprecationEntry, DeprecationManifest, DeprecationState,
    SABI_SERVICE_DIRECTORY_SCHEMA, SymbolAuditVerdict, VersionCompatibilityPolicy,
    VersionNegotiationOutcome, VersionRoute, decode_negotiate_service_request_frame,
    decode_resolve_service_request_frame, encode_negotiate_service_request,
    encode_negotiate_service_request_frame, encode_resolve_service_request,
    encode_resolve_service_request_frame, encode_resolve_service_response, schema_registry,
    service_directory_schema_identity,
};

pub const BINDING_ID_BYTES: usize = 16;
pub const MAX_NAME_BYTES: usize = 255;
pub const MAX_ENDPOINT_BYTES: usize = 4096;
pub const MAX_FEATURE_IDS: usize = 128;
pub const MAX_TRANSPORT_KINDS: usize = 8;
pub const MAX_REGISTRATIONS: usize = 256;
/// Bounded ring capacity for [`DeprecationWarningReceipt`] telemetry stored on
/// [`SnapshotDirectory`] (`ADR-0027` section 3.3).
pub const MAX_DEPRECATION_WARNINGS: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceRegistration {
    pub candidate: ServiceCandidate,
    pub endpoint: LocalEndpoint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistrationError {
    InvalidBindingIdLength { actual: usize },
    DuplicateBindingId,
    TooManyRegistrations,
    ResolveResponseTooLarge,
    ZeroGeneration,
    InvalidServiceName,
    MissingVersion,
    InvalidSchemaName,
    InvalidFeatureIds,
    InvalidTransportKinds,
    InvalidEndpoint,
    EndpointTransportMismatch,
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBindingIdLength { actual } => write!(
                formatter,
                "binding_id has {actual} bytes; exactly {BINDING_ID_BYTES} are required"
            ),
            Self::DuplicateBindingId => formatter.write_str("binding_id is duplicated"),
            Self::TooManyRegistrations => formatter.write_str("directory snapshot is too large"),
            Self::ResolveResponseTooLarge => {
                formatter.write_str("a service resolve response exceeds its schema bound")
            }
            Self::ZeroGeneration => formatter.write_str("binding generation must be non-zero"),
            Self::InvalidServiceName => formatter.write_str("service name is invalid"),
            Self::MissingVersion => formatter.write_str("service version is missing"),
            Self::InvalidSchemaName => formatter.write_str("service schema name is invalid"),
            Self::InvalidFeatureIds => {
                formatter.write_str("feature IDs must be non-zero, bounded, sorted, and unique")
            }
            Self::InvalidTransportKinds => formatter
                .write_str("transport kinds must be known, bounded, sorted, unique, and non-empty"),
            Self::InvalidEndpoint => formatter.write_str("local endpoint is invalid"),
            Self::EndpointTransportMismatch => formatter
                .write_str("candidate transport kinds must contain only the endpoint transport"),
        }
    }
}

impl Error for RegistrationError {}

/// Structured deprecation audit receipt emitted by [`SnapshotDirectory`] when a
/// caller resolves or negotiates a `DEPRECATED` SABI contract symbol, feature,
/// or field within its rolling grace window (`ADR-0027` section 3.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeprecationWarningReceipt {
    pub channel: String,
    pub symbol: String,
    pub field_number: Option<u32>,
    pub deprecated_in_epoch: u32,
    pub earliest_sunset_epoch: u32,
    pub replacement: String,
    pub caller_major: u32,
    pub caller_minor: u32,
    pub service: String,
}

/// Policy-governed `ServiceDirectory` resolution or negotiation result pairing
/// the response with version-route adjudication, preserved input wire bytes, and
/// any [`DeprecationWarningReceipt`]s emitted by the call.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyDirectoryOutcome<T> {
    pub response: T,
    pub caller_negotiation: VersionNegotiationOutcome,
    pub target_negotiation: Option<VersionNegotiationOutcome>,
    pub preserved_wire: Vec<u8>,
    pub emitted_warnings: Vec<DeprecationWarningReceipt>,
}

/// Fail-closed rejection produced when policy-governed `ServiceDirectory`
/// resolution or negotiation violates `VersionCompatibilityPolicy`,
/// `[COMPAT-VER-002]` critical-extension rules, or a `SUNSET`/`RESERVED`
/// contract tombstone in `deprecation-manifest.yaml`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyNegotiationError {
    /// Wire size, protobuf decoding, `N / N-1 / N-2` version window, or
    /// `[COMPAT-VER-002]` critical-extension failure.
    Compatibility(CompatibilityError),
    /// Requested service symbol or feature/field slot has reached `SUNSET` or
    /// is permanently `RESERVED` (`ADR-0027` section 3.2).
    SunsetContract {
        channel: String,
        symbol: String,
        field_number: Option<u32>,
        reason: &'static str,
    },
}

impl fmt::Display for PolicyNegotiationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compatibility(error) => write!(formatter, "{error}"),
            Self::SunsetContract {
                channel,
                symbol,
                field_number: Some(field),
                reason,
            } => write!(
                formatter,
                "contract element {symbol:?} (field {field}) in {channel:?} is retired: {reason}"
            ),
            Self::SunsetContract {
                channel,
                symbol,
                field_number: None,
                reason,
            } => write!(
                formatter,
                "contract element {symbol:?} in {channel:?} is retired: {reason}"
            ),
        }
    }
}

impl Error for PolicyNegotiationError {}

impl PolicyNegotiationError {
    /// Converts this fail-closed rejection into a bounded, wire-safe [`SabiFailure`].
    ///
    /// All version-window (`0`, `> N`, `<= N-3`), unknown-channel, `[COMPAT-VER-002]`
    /// critical-extension, and `SUNSET`/`RESERVED` rejections map deterministically to
    /// [`SabiErrorCode::AbiVersion`] (`E_ABI_VERSION`) with
    /// [`RetryDirective::DoNotRetry`].
    #[must_use]
    pub fn to_sabi_failure(&self) -> SabiFailure {
        match self {
            Self::Compatibility(error) => error.to_sabi_failure(),
            Self::SunsetContract { .. } => SabiFailure {
                code: SabiErrorCode::AbiVersion.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "SABI contract element has reached SUNSET or is permanently RESERVED"
                    .to_owned(),
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SnapshotDirectory {
    registrations: Vec<ServiceRegistration>,
    deprecation_warnings: Vec<DeprecationWarningReceipt>,
}

impl SnapshotDirectory {
    /// Creates an immutable directory snapshot after validating every binding.
    ///
    /// # Errors
    ///
    /// Rejects malformed registrations and duplicate 128-bit binding IDs.
    pub fn new(
        registrations: impl IntoIterator<Item = ServiceRegistration>,
    ) -> Result<Self, RegistrationError> {
        let mut validated = Vec::new();
        let mut binding_ids = HashSet::new();
        for registration in registrations {
            if validated.len() == MAX_REGISTRATIONS {
                return Err(RegistrationError::TooManyRegistrations);
            }
            validate_registration(&registration)?;
            if !binding_ids.insert(registration.candidate.binding_id.clone()) {
                return Err(RegistrationError::DuplicateBindingId);
            }
            validated.push(registration);
        }
        validated.sort_by(|left, right| compare_candidates(&left.candidate, &right.candidate));
        validate_resolve_response_bounds(&validated)?;
        Ok(Self {
            registrations: validated,
            deprecation_warnings: Vec::new(),
        })
    }

    /// Returns the bounded FIFO slice of [`DeprecationWarningReceipt`]s emitted
    /// by policy-governed resolution and negotiation calls.
    #[must_use]
    pub fn deprecation_warnings(&self) -> &[DeprecationWarningReceipt] {
        &self.deprecation_warnings
    }

    /// Drains and returns all buffered [`DeprecationWarningReceipt`]s in FIFO order.
    pub fn drain_deprecation_warnings(&mut self) -> Vec<DeprecationWarningReceipt> {
        std::mem::take(&mut self.deprecation_warnings)
    }

    /// Returns transport-neutral candidates in deterministic order.
    #[must_use]
    pub fn resolve(&self, request: &ResolveServiceRequest) -> ResolveServiceResponse {
        if encode_resolve_service_request(request).is_err() || !valid_name(&request.service) {
            return resolve_error(DirectoryErrorCode::InvalidRequest, request);
        }

        let candidates: Vec<_> = self
            .registrations
            .iter()
            .filter(|registration| registration.candidate.service == request.service)
            .map(|registration| registration.candidate.clone())
            .collect();
        if candidates.is_empty() {
            return resolve_error(DirectoryErrorCode::NotFound, request);
        }
        ResolveServiceResponse {
            schema: Some(service_directory_schema_identity()),
            result: Some(resolve_service_response::Result::Candidates(
                ServiceCandidateSet { candidates },
            )),
        }
    }

    /// Selects one compatible endpoint after deterministic version/feature negotiation.
    #[must_use]
    pub fn negotiate(&self, request: &NegotiateServiceRequest) -> NegotiateServiceResponse {
        if !valid_negotiate_request(request) {
            return negotiate_error(DirectoryErrorCode::InvalidRequest, request);
        }

        let service: Vec<_> = self
            .registrations
            .iter()
            .filter(|registration| registration.candidate.service == request.service)
            .collect();
        if service.is_empty() {
            return negotiate_error(DirectoryErrorCode::NotFound, request);
        }

        let schema: Vec<_> = service
            .into_iter()
            .filter(|registration| {
                registration
                    .candidate
                    .version
                    .as_ref()
                    .is_some_and(|version| version.schema_name == request.schema_name)
            })
            .collect();
        if schema.is_empty() {
            return negotiate_error(DirectoryErrorCode::SchemaUnsupported, request);
        }

        let version: Vec<_> = schema
            .into_iter()
            .filter(|registration| {
                registration
                    .candidate
                    .version
                    .as_ref()
                    .is_some_and(|version| {
                        version.major == request.major && version.minor >= request.minimum_minor
                    })
            })
            .collect();
        if version.is_empty() {
            return negotiate_error(DirectoryErrorCode::VersionUnsupported, request);
        }

        let features: Vec<_> = version
            .into_iter()
            .filter(|registration| {
                request.required_feature_ids.iter().all(|required| {
                    registration
                        .candidate
                        .feature_ids
                        .binary_search(required)
                        .is_ok()
                })
            })
            .collect();
        if features.is_empty() {
            return negotiate_error(DirectoryErrorCode::RequiredFeatureUnsupported, request);
        }

        let mut transports: Vec<_> = features
            .into_iter()
            .filter(|registration| {
                request
                    .supported_transport_kinds
                    .binary_search(&registration.endpoint.kind)
                    .is_ok()
            })
            .collect();
        if transports.is_empty() {
            return negotiate_error(DirectoryErrorCode::TransportUnsupported, request);
        }
        transports.sort_by(|left, right| {
            compare_negotiated_candidates(&left.candidate, &right.candidate)
        });
        let selected = transports[0];
        NegotiateServiceResponse {
            schema: Some(service_directory_schema_identity()),
            result: Some(negotiate_service_response::Result::Binding(
                ServiceBinding {
                    candidate: Some(selected.candidate.clone()),
                    endpoint: Some(selected.endpoint.clone()),
                },
            )),
        }
    }

    /// Decodes a bounded `ResolveServiceRequest` wire payload, enforces
    /// [`VersionCompatibilityPolicy`] and [`DeprecationManifest`], preserves exact
    /// input wire bytes, and emits [`DeprecationWarningReceipt`]s for `DEPRECATED`
    /// contracts without interrupting candidate resolution.
    ///
    /// # Errors
    ///
    /// Fails closed with [`PolicyNegotiationError`] on oversized/malformed wire
    /// payloads, incompatible caller epochs (`0`, `> N`, `<= N-3`), unsupported
    /// critical extensions, or `SUNSET`/`RESERVED` service symbols.
    pub fn resolve_wire_with_policy(
        &mut self,
        wire: &[u8],
        policy: &VersionCompatibilityPolicy<'_>,
        manifest: &DeprecationManifest,
    ) -> Result<PolicyDirectoryOutcome<ResolveServiceResponse>, PolicyNegotiationError> {
        let request = decode_resolve_service_request_frame(wire)
            .map_err(PolicyNegotiationError::Compatibility)?;
        self.resolve_with_policy_inner(&request, wire.to_vec(), policy, manifest)
    }

    /// Enforces [`VersionCompatibilityPolicy`] and [`DeprecationManifest`] over a
    /// typed [`ResolveServiceRequest`].
    ///
    /// # Errors
    ///
    /// Fails closed with [`PolicyNegotiationError`] on oversized payloads,
    /// incompatible caller epochs, unsupported critical extensions, or
    /// `SUNSET`/`RESERVED` service symbols.
    pub fn resolve_with_policy(
        &mut self,
        request: &ResolveServiceRequest,
        policy: &VersionCompatibilityPolicy<'_>,
        manifest: &DeprecationManifest,
    ) -> Result<PolicyDirectoryOutcome<ResolveServiceResponse>, PolicyNegotiationError> {
        let wire = encode_resolve_service_request_frame(request)
            .map_err(PolicyNegotiationError::Compatibility)?;
        self.resolve_with_policy_inner(request, wire, policy, manifest)
    }

    /// Decodes a bounded `NegotiateServiceRequest` wire payload, enforces
    /// [`VersionCompatibilityPolicy`] (`N / N-1 / N-2` + `[COMPAT-VER-002]`) and
    /// [`DeprecationManifest`] (`ACTIVE` / `DEPRECATED` / `SUNSET`), preserves
    /// exact input wire bytes, and records [`DeprecationWarningReceipt`] telemetry
    /// when a `DEPRECATED` contract is negotiated.
    ///
    /// # Errors
    ///
    /// Fails closed with [`PolicyNegotiationError`] (mapping to `E_ABI_VERSION` +
    /// `DO_NOT_RETRY`) on oversized/malformed wire payloads, unsupported caller or
    /// target epochs (`0`, `> N`, `<= N-3`), unknown `critical_extension_ids`, or
    /// `SUNSET`/`RESERVED` symbols and feature/field numbers.
    pub fn negotiate_resolve_wire(
        &mut self,
        wire: &[u8],
        policy: &VersionCompatibilityPolicy<'_>,
        manifest: &DeprecationManifest,
    ) -> Result<PolicyDirectoryOutcome<NegotiateServiceResponse>, PolicyNegotiationError> {
        let request = decode_negotiate_service_request_frame(wire)
            .map_err(PolicyNegotiationError::Compatibility)?;
        self.negotiate_with_policy_inner(&request, wire.to_vec(), policy, manifest)
    }

    /// Enforces [`VersionCompatibilityPolicy`] and [`DeprecationManifest`] over a
    /// typed [`NegotiateServiceRequest`].
    ///
    /// # Errors
    ///
    /// Fails closed with [`PolicyNegotiationError`] on oversized payloads,
    /// incompatible caller or target epochs, unsupported critical extensions, or
    /// `SUNSET`/`RESERVED` symbols and feature/field numbers.
    pub fn negotiate_with_policy(
        &mut self,
        request: &NegotiateServiceRequest,
        policy: &VersionCompatibilityPolicy<'_>,
        manifest: &DeprecationManifest,
    ) -> Result<PolicyDirectoryOutcome<NegotiateServiceResponse>, PolicyNegotiationError> {
        let wire = encode_negotiate_service_request_frame(request)
            .map_err(PolicyNegotiationError::Compatibility)?;
        self.negotiate_with_policy_inner(request, wire, policy, manifest)
    }

    fn resolve_with_policy_inner(
        &mut self,
        request: &ResolveServiceRequest,
        preserved_wire: Vec<u8>,
        policy: &VersionCompatibilityPolicy<'_>,
        manifest: &DeprecationManifest,
    ) -> Result<PolicyDirectoryOutcome<ResolveServiceResponse>, PolicyNegotiationError> {
        let caller_negotiation = adjudicate_directory_caller(request.schema.as_ref(), policy)?;
        let mut staged_warnings = Vec::new();

        audit_verdict_into_warnings(
            manifest.audit_symbol(SABI_SERVICE_DIRECTORY_SCHEMA, "ResolveServiceRequest"),
            SABI_SERVICE_DIRECTORY_SCHEMA,
            "ResolveServiceRequest",
            None,
            caller_negotiation.client_major,
            caller_negotiation.client_minor,
            &request.service,
            &mut staged_warnings,
        )?;

        if !valid_name(&request.service) {
            return Ok(PolicyDirectoryOutcome {
                response: resolve_error(DirectoryErrorCode::InvalidRequest, request),
                caller_negotiation,
                target_negotiation: None,
                preserved_wire,
                emitted_warnings: Vec::new(),
            });
        }

        // Fail closed if the requested service name is retired as SUNSET or RESERVED
        // in any channel of the manifest, even if already removed from active bindings.
        audit_service_across_manifest(
            manifest,
            &request.service,
            caller_negotiation.client_major,
            caller_negotiation.client_minor,
            &mut staged_warnings,
        )?;

        let candidates: Vec<_> = self
            .registrations
            .iter()
            .filter(|registration| registration.candidate.service == request.service)
            .map(|registration| registration.candidate.clone())
            .collect();

        if candidates.is_empty() {
            return Ok(PolicyDirectoryOutcome {
                response: resolve_error(DirectoryErrorCode::NotFound, request),
                caller_negotiation,
                target_negotiation: None,
                preserved_wire,
                emitted_warnings: Vec::new(),
            });
        }

        self.push_warnings(&staged_warnings);
        Ok(PolicyDirectoryOutcome {
            response: ResolveServiceResponse {
                schema: Some(response_directory_identity(policy.host_epoch)),
                result: Some(resolve_service_response::Result::Candidates(
                    ServiceCandidateSet { candidates },
                )),
            },
            caller_negotiation,
            target_negotiation: None,
            preserved_wire,
            emitted_warnings: staged_warnings,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn negotiate_with_policy_inner(
        &mut self,
        request: &NegotiateServiceRequest,
        preserved_wire: Vec<u8>,
        policy: &VersionCompatibilityPolicy<'_>,
        manifest: &DeprecationManifest,
    ) -> Result<PolicyDirectoryOutcome<NegotiateServiceResponse>, PolicyNegotiationError> {
        let caller_negotiation = adjudicate_directory_caller(request.schema.as_ref(), policy)?;
        let target_negotiation = adjudicate_target_schema(request, policy)?;

        if !valid_name(&request.service)
            || !valid_name(&request.schema_name)
            || !valid_feature_ids(&request.required_feature_ids)
            || !valid_transport_kinds(&request.supported_transport_kinds)
        {
            return Ok(PolicyDirectoryOutcome {
                response: negotiate_error_for_epoch(
                    DirectoryErrorCode::InvalidRequest,
                    request,
                    policy.host_epoch,
                ),
                caller_negotiation,
                target_negotiation,
                preserved_wire,
                emitted_warnings: Vec::new(),
            });
        }

        let mut staged_warnings = Vec::new();
        audit_verdict_into_warnings(
            manifest.audit_symbol(SABI_SERVICE_DIRECTORY_SCHEMA, "NegotiateServiceRequest"),
            SABI_SERVICE_DIRECTORY_SCHEMA,
            "NegotiateServiceRequest",
            None,
            caller_negotiation.client_major,
            caller_negotiation.client_minor,
            &request.service,
            &mut staged_warnings,
        )?;
        audit_verdict_into_warnings(
            manifest.audit_symbol(&request.schema_name, &request.service),
            &request.schema_name,
            &request.service,
            None,
            request.major,
            request.minimum_minor,
            &request.service,
            &mut staged_warnings,
        )?;
        for &feature_id in &request.required_feature_ids {
            let verdict = manifest.audit_field_number(&request.schema_name, None, feature_id);
            let fallback_symbol = format!("feature_id:{feature_id}");
            audit_verdict_into_warnings(
                verdict,
                &request.schema_name,
                &fallback_symbol,
                Some(feature_id),
                request.major,
                request.minimum_minor,
                &request.service,
                &mut staged_warnings,
            )?;
        }

        let service: Vec<_> = self
            .registrations
            .iter()
            .filter(|registration| registration.candidate.service == request.service)
            .collect();
        if service.is_empty() {
            return Ok(PolicyDirectoryOutcome {
                response: negotiate_error_for_epoch(
                    DirectoryErrorCode::NotFound,
                    request,
                    policy.host_epoch,
                ),
                caller_negotiation,
                target_negotiation,
                preserved_wire,
                emitted_warnings: Vec::new(),
            });
        }

        let schema: Vec<_> = service
            .into_iter()
            .filter(|registration| {
                registration
                    .candidate
                    .version
                    .as_ref()
                    .is_some_and(|version| version.schema_name == request.schema_name)
            })
            .collect();
        if schema.is_empty() {
            return Ok(PolicyDirectoryOutcome {
                response: negotiate_error_for_epoch(
                    DirectoryErrorCode::SchemaUnsupported,
                    request,
                    policy.host_epoch,
                ),
                caller_negotiation,
                target_negotiation,
                preserved_wire,
                emitted_warnings: Vec::new(),
            });
        }

        let mut version: Vec<_> = schema
            .iter()
            .copied()
            .filter(|registration| {
                registration.candidate.version.as_ref().is_some_and(|ver| {
                    ver.major == request.major && ver.minor >= request.minimum_minor
                })
            })
            .collect();

        // When no direct historical-major binding is registered and the target
        // epoch was approved for OutOfCoreCompatShim routing (N-1 / N-2), select
        // from the host's current-epoch authority bindings so the shim cell can
        // mediate protocol translation without duplicating authority state.
        let shim_routed = matches!(
            target_negotiation.as_ref().map(|out| out.route),
            Some(VersionRoute::OutOfCoreCompatShim { .. })
        ) || request.major < policy.host_epoch;
        if version.is_empty() && shim_routed {
            version = schema
                .into_iter()
                .filter(|registration| {
                    registration
                        .candidate
                        .version
                        .as_ref()
                        .is_some_and(|ver| ver.major == policy.host_epoch)
                })
                .collect();
        }
        if version.is_empty() {
            return Ok(PolicyDirectoryOutcome {
                response: negotiate_error_for_epoch(
                    DirectoryErrorCode::VersionUnsupported,
                    request,
                    policy.host_epoch,
                ),
                caller_negotiation,
                target_negotiation,
                preserved_wire,
                emitted_warnings: Vec::new(),
            });
        }

        let features: Vec<_> = version
            .into_iter()
            .filter(|registration| {
                request.required_feature_ids.iter().all(|required| {
                    registration
                        .candidate
                        .feature_ids
                        .binary_search(required)
                        .is_ok()
                })
            })
            .collect();
        if features.is_empty() {
            return Ok(PolicyDirectoryOutcome {
                response: negotiate_error_for_epoch(
                    DirectoryErrorCode::RequiredFeatureUnsupported,
                    request,
                    policy.host_epoch,
                ),
                caller_negotiation,
                target_negotiation,
                preserved_wire,
                emitted_warnings: Vec::new(),
            });
        }

        let mut transports: Vec<_> = features
            .into_iter()
            .filter(|registration| {
                request
                    .supported_transport_kinds
                    .binary_search(&registration.endpoint.kind)
                    .is_ok()
            })
            .collect();
        if transports.is_empty() {
            return Ok(PolicyDirectoryOutcome {
                response: negotiate_error_for_epoch(
                    DirectoryErrorCode::TransportUnsupported,
                    request,
                    policy.host_epoch,
                ),
                caller_negotiation,
                target_negotiation,
                preserved_wire,
                emitted_warnings: Vec::new(),
            });
        }

        transports.sort_by(|left, right| {
            compare_negotiated_candidates(&left.candidate, &right.candidate)
        });
        let selected = transports[0];
        let binding = ServiceBinding {
            candidate: Some(selected.candidate.clone()),
            endpoint: Some(selected.endpoint.clone()),
        };

        // Emit deprecation telemetry without disrupting traffic to DEPRECATED bindings.
        self.push_warnings(&staged_warnings);

        Ok(PolicyDirectoryOutcome {
            response: NegotiateServiceResponse {
                schema: Some(response_directory_identity(policy.host_epoch)),
                result: Some(negotiate_service_response::Result::Binding(binding)),
            },
            caller_negotiation,
            target_negotiation,
            preserved_wire,
            emitted_warnings: staged_warnings,
        })
    }

    fn push_warnings(&mut self, receipts: &[DeprecationWarningReceipt]) {
        for receipt in receipts {
            if self.deprecation_warnings.len() == MAX_DEPRECATION_WARNINGS {
                self.deprecation_warnings.remove(0);
            }
            self.deprecation_warnings.push(receipt.clone());
        }
    }
}

fn adjudicate_directory_caller(
    identity: Option<&SchemaIdentity>,
    policy: &VersionCompatibilityPolicy<'_>,
) -> Result<VersionNegotiationOutcome, PolicyNegotiationError> {
    let identity = identity.ok_or(PolicyNegotiationError::Compatibility(
        CompatibilityError::MissingSchemaIdentity,
    ))?;
    if identity.name != SABI_SERVICE_DIRECTORY_SCHEMA {
        return Err(PolicyNegotiationError::Compatibility(
            CompatibilityError::UnknownSchema(identity.name.clone()),
        ));
    }
    policy
        .adjudicate_schema_identity(identity)
        .map_err(PolicyNegotiationError::Compatibility)
}

fn adjudicate_target_schema(
    request: &NegotiateServiceRequest,
    policy: &VersionCompatibilityPolicy<'_>,
) -> Result<Option<VersionNegotiationOutcome>, PolicyNegotiationError> {
    let is_registered = schema_registry()
        .iter()
        .any(|descriptor| descriptor.name == request.schema_name);
    if is_registered || request.schema_name.starts_with("nlos.sabi.") {
        let target_identity = SchemaIdentity {
            name: request.schema_name.clone(),
            major: request.major,
            minor: request.minimum_minor,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        };
        let outcome = policy
            .adjudicate_schema_identity(&target_identity)
            .map_err(PolicyNegotiationError::Compatibility)?;
        return Ok(Some(outcome));
    }

    // Even for non-SABI application service schemas, enforce the host's rolling
    // N / N-1 / N-2 major version window fail-closed.
    if request.major == 0 {
        return Err(PolicyNegotiationError::Compatibility(
            CompatibilityError::ZeroMajorVersion {
                schema: request.schema_name.clone(),
            },
        ));
    }
    if request.major > policy.host_epoch {
        return Err(PolicyNegotiationError::Compatibility(
            CompatibilityError::UnsupportedMajor {
                schema: request.schema_name.clone(),
                got: request.major,
                supported: policy.host_epoch,
            },
        ));
    }
    if policy.host_epoch - request.major > policy.max_backward_epochs {
        return Err(PolicyNegotiationError::Compatibility(
            CompatibilityError::SunsetEpoch {
                schema: request.schema_name.clone(),
                client_epoch: request.major,
                host_epoch: policy.host_epoch,
                max_backward_epochs: policy.max_backward_epochs,
            },
        ));
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn audit_verdict_into_warnings(
    verdict: SymbolAuditVerdict<'_>,
    channel: &str,
    fallback_symbol: &str,
    fallback_field_number: Option<u32>,
    caller_major: u32,
    caller_minor: u32,
    service: &str,
    staged_warnings: &mut Vec<DeprecationWarningReceipt>,
) -> Result<(), PolicyNegotiationError> {
    match verdict {
        SymbolAuditVerdict::Active(_) => Ok(()),
        SymbolAuditVerdict::Deprecated(entry) => {
            staged_warnings.push(build_warning_receipt(
                entry,
                caller_major,
                caller_minor,
                service,
            ));
            Ok(())
        }
        SymbolAuditVerdict::SunsetOrReserved { entry, reason } => {
            Err(PolicyNegotiationError::SunsetContract {
                channel: channel.to_owned(),
                symbol: entry
                    .map_or_else(|| fallback_symbol.to_owned(), |item| item.symbol.clone()),
                field_number: entry
                    .and_then(|item| item.field_number)
                    .or(fallback_field_number),
                reason,
            })
        }
    }
}

fn audit_service_across_manifest(
    manifest: &DeprecationManifest,
    service: &str,
    caller_major: u32,
    caller_minor: u32,
    staged_warnings: &mut Vec<DeprecationWarningReceipt>,
) -> Result<(), PolicyNegotiationError> {
    for channel in &manifest.channels {
        if channel.reserved_symbols.iter().any(|sym| sym == service) {
            return Err(PolicyNegotiationError::SunsetContract {
                channel: channel.name.clone(),
                symbol: service.to_owned(),
                field_number: None,
                reason: "symbol is permanently reserved in channel tombstone list",
            });
        }
    }
    for entry in &manifest.entries {
        if entry.symbol == service {
            match entry.state {
                DeprecationState::Active => {}
                DeprecationState::Deprecated => {
                    staged_warnings.push(build_warning_receipt(
                        entry,
                        caller_major,
                        caller_minor,
                        service,
                    ));
                }
                DeprecationState::Sunset => {
                    return Err(PolicyNegotiationError::SunsetContract {
                        channel: entry.channel.clone(),
                        symbol: entry.symbol.clone(),
                        field_number: entry.field_number,
                        reason: "symbol has reached SUNSET and is permanently retired",
                    });
                }
            }
        }
    }
    Ok(())
}

fn build_warning_receipt(
    entry: &DeprecationEntry,
    caller_major: u32,
    caller_minor: u32,
    service: &str,
) -> DeprecationWarningReceipt {
    DeprecationWarningReceipt {
        channel: entry.channel.clone(),
        symbol: entry.symbol.clone(),
        field_number: entry.field_number,
        deprecated_in_epoch: entry.deprecated_in_epoch.unwrap_or(0),
        earliest_sunset_epoch: entry.earliest_sunset_epoch.unwrap_or(0),
        replacement: entry.replacement.clone().unwrap_or_default(),
        caller_major,
        caller_minor,
        service: service.to_owned(),
    }
}

fn response_directory_identity(host_epoch: u32) -> SchemaIdentity {
    let mut identity = service_directory_schema_identity();
    identity.major = host_epoch;
    identity
}

fn validate_resolve_response_bounds(
    registrations: &[ServiceRegistration],
) -> Result<(), RegistrationError> {
    let mut offset = 0;
    while offset < registrations.len() {
        let service = &registrations[offset].candidate.service;
        let end = registrations[offset..]
            .iter()
            .position(|registration| registration.candidate.service != *service)
            .map_or(registrations.len(), |relative| offset + relative);
        let response = ResolveServiceResponse {
            schema: Some(service_directory_schema_identity()),
            result: Some(resolve_service_response::Result::Candidates(
                ServiceCandidateSet {
                    candidates: registrations[offset..end]
                        .iter()
                        .map(|registration| registration.candidate.clone())
                        .collect(),
                },
            )),
        };
        encode_resolve_service_response(&response)
            .map_err(|_| RegistrationError::ResolveResponseTooLarge)?;
        offset = end;
    }
    Ok(())
}

fn valid_negotiate_request(request: &NegotiateServiceRequest) -> bool {
    encode_negotiate_service_request(request).is_ok()
        && valid_name(&request.service)
        && valid_name(&request.schema_name)
        && request.major != 0
        && valid_feature_ids(&request.required_feature_ids)
        && valid_transport_kinds(&request.supported_transport_kinds)
}

fn validate_registration(registration: &ServiceRegistration) -> Result<(), RegistrationError> {
    let candidate = &registration.candidate;
    if candidate.binding_id.len() != BINDING_ID_BYTES {
        return Err(RegistrationError::InvalidBindingIdLength {
            actual: candidate.binding_id.len(),
        });
    }
    if candidate.generation == 0 {
        return Err(RegistrationError::ZeroGeneration);
    }
    if !valid_name(&candidate.service) {
        return Err(RegistrationError::InvalidServiceName);
    }
    let version = candidate
        .version
        .as_ref()
        .ok_or(RegistrationError::MissingVersion)?;
    if !valid_name(&version.schema_name) || version.major == 0 {
        return Err(RegistrationError::InvalidSchemaName);
    }
    if !valid_feature_ids(&candidate.feature_ids) {
        return Err(RegistrationError::InvalidFeatureIds);
    }
    if !valid_transport_kinds(&candidate.transport_kinds) {
        return Err(RegistrationError::InvalidTransportKinds);
    }
    if !valid_endpoint(&registration.endpoint) {
        return Err(RegistrationError::InvalidEndpoint);
    }
    if candidate.transport_kinds.as_slice() != [registration.endpoint.kind] {
        return Err(RegistrationError::EndpointTransportMismatch);
    }
    Ok(())
}

fn valid_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_NAME_BYTES && !value.contains('\0')
}

fn valid_feature_ids(values: &[u32]) -> bool {
    values.len() <= MAX_FEATURE_IDS
        && values.iter().all(|value| *value != 0)
        && values.windows(2).all(|pair| pair[0] < pair[1])
}

fn valid_transport_kinds(values: &[i32]) -> bool {
    !values.is_empty()
        && values.len() <= MAX_TRANSPORT_KINDS
        && values.windows(2).all(|pair| pair[0] < pair[1])
        && values.iter().all(|value| {
            LocalTransportKind::try_from(*value)
                .is_ok_and(|kind| kind != LocalTransportKind::Unspecified)
        })
}

fn valid_endpoint(endpoint: &LocalEndpoint) -> bool {
    LocalTransportKind::try_from(endpoint.kind)
        .is_ok_and(|kind| kind != LocalTransportKind::Unspecified)
        && !endpoint.address.is_empty()
        && endpoint.address.len() <= MAX_ENDPOINT_BYTES
        && !endpoint.address.contains('\0')
}

fn compare_candidates(left: &ServiceCandidate, right: &ServiceCandidate) -> Ordering {
    let left_version = left
        .version
        .as_ref()
        .expect("validated candidate has a version");
    let right_version = right
        .version
        .as_ref()
        .expect("validated candidate has a version");
    left.service
        .cmp(&right.service)
        .then_with(|| left_version.schema_name.cmp(&right_version.schema_name))
        .then_with(|| right_version.major.cmp(&left_version.major))
        .then_with(|| right_version.minor.cmp(&left_version.minor))
        .then_with(|| right.generation.cmp(&left.generation))
        .then_with(|| left.binding_id.cmp(&right.binding_id))
}

fn compare_negotiated_candidates(left: &ServiceCandidate, right: &ServiceCandidate) -> Ordering {
    let left_version = left
        .version
        .as_ref()
        .expect("validated candidate has a version");
    let right_version = right
        .version
        .as_ref()
        .expect("validated candidate has a version");
    right_version
        .minor
        .cmp(&left_version.minor)
        .then_with(|| right.generation.cmp(&left.generation))
        .then_with(|| left.binding_id.cmp(&right.binding_id))
}

fn safe_error_service(service: &str) -> String {
    if valid_name(service) {
        service.to_owned()
    } else {
        String::new()
    }
}

fn resolve_error(
    code: DirectoryErrorCode,
    request: &ResolveServiceRequest,
) -> ResolveServiceResponse {
    ResolveServiceResponse {
        schema: Some(service_directory_schema_identity()),
        result: Some(resolve_service_response::Result::Error(DirectoryError {
            code: code.into(),
            service: safe_error_service(&request.service),
        })),
    }
}

fn negotiate_error(
    code: DirectoryErrorCode,
    request: &NegotiateServiceRequest,
) -> NegotiateServiceResponse {
    negotiate_error_for_epoch(code, request, 1)
}

fn negotiate_error_for_epoch(
    code: DirectoryErrorCode,
    request: &NegotiateServiceRequest,
    host_epoch: u32,
) -> NegotiateServiceResponse {
    NegotiateServiceResponse {
        schema: Some(response_directory_identity(host_epoch)),
        result: Some(negotiate_service_response::Result::Error(DirectoryError {
            code: code.into(),
            service: safe_error_service(&request.service),
        })),
    }
}
