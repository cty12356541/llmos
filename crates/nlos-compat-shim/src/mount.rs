//! Per-package dynamic epoch compatibility mount planner (`[SABI-DISC-001]`,
//! `[DRV-NEGO-001]`, `ADR-0027` section 2.2, Stage E Decision Point `E1`).
//!
//! Inspects a package's declared [`PackageCompatibilitySpec`] against the host's
//! [`VersionCompatibilityPolicy`], [`DeprecationManifest`], and optional
//! [`SnapshotDirectory`]:
//! - native `N` packages without legacy driver protocol needs receive
//!   [`CompatMountDecision::NativePassthrough`] (`0` shim hops);
//! - historical `N-1` / `N-2` packages or Stage B `minicbor` / historical
//!   `DriverProvider` adapters dynamically mount an unprivileged out-of-core
//!   [`SabiEpochAdapterCell`] and/or [`DriverProtocolShim`] while collecting
//!   structured [`DeprecationWarningReceipt`] telemetry;
//! - epoch `0`, future unknown `> N`, sunset `<= N-3`, unsupported
//!   `critical_extensions` (`[COMPAT-VER-002]`), or any reference to a `SUNSET`
//!   or permanently `RESERVED` symbol/field slot in [`DeprecationManifest`]
//!   fails closed and maps deterministically to `SabiErrorCode::AbiVersion` +
//!   `RetryDirective::DoNotRetry`.

use std::error::Error;
use std::fmt;

use nlos_schema::sabi::v1::{
    NegotiateServiceRequest, RetryDirective, SabiErrorCode, SabiFailure, SchemaIdentity,
    ServiceBinding, negotiate_service_response,
};
use nlos_schema::{
    CompatibilityError, DeprecationManifest, MAX_EXTENSION_IDS, SABI_DRIVER_PROVIDER_SCHEMA,
    SABI_ENVELOPE_SCHEMA, SABI_SERVICE_DIRECTORY_SCHEMA, SymbolAuditVerdict,
    VersionCompatibilityPolicy, VersionNegotiationOutcome, VersionRoute,
    canonical_deprecation_manifest, schema_registry,
};
use nlos_service_directory::{
    DeprecationWarningReceipt, MAX_NAME_BYTES, PolicyNegotiationError, SnapshotDirectory,
};

use crate::MAX_SHIM_DEPRECATION_WARNINGS;
use crate::driver_shim::{DriverProtocolShim, DriverShimError, ProviderTrustCeiling};
use crate::sabi_adapter::{
    EpochTranslationRules, SabiAdapterError, SabiEpochAdapterCell, entry_to_warning_receipt,
};

/// Declared user-mode driver wire ABI epoch in `llmos.package` compatibility metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverWireEpoch {
    /// Stage B pre-freeze deterministic `minicbor` payload (`nlos.driver-mock.payload` v1).
    LegacyStageBMinicborV1,
    /// Frozen `ADR-0024` 8th channel `nlos.sabi.DriverProvider` Protobuf at major epoch.
    FrozenDriverProviderEpoch(u32),
}

/// Package-level SABI and driver wire compatibility declaration (`llmos.package`
/// `package.compatibility` block, `ADR-0027` section 2.2.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageCompatibilitySpec {
    /// Logical package identifier (`1..=255` UTF-8 bytes, no NUL).
    pub package_name: String,
    /// Target SABI major epoch (`N`, `N-1`, or `N-2`).
    pub target_sabi_epoch: u32,
    /// Target SABI minor version within `target_sabi_epoch`.
    pub target_sabi_minor: u32,
    /// Optional user-mode driver wire ABI epoch if the package hosts or binds a driver.
    pub driver_abi_epoch: Option<DriverWireEpoch>,
    /// Required critical SABI extensions (`[COMPAT-VER-002]`).
    pub critical_extensions: Vec<u32>,
    /// Optional non-critical SABI extensions (`[COMPAT-VER-002]`).
    pub noncritical_extensions: Vec<u32>,
    /// Declared `(channel_schema_name, symbol_or_field_slot)` references audited
    /// against `deprecation-manifest.yaml`. Field-slot references may be written
    /// either by symbol name or as `"field:<u32>"` / `"<ParentType>.field:<u32>"`.
    pub referenced_symbols: Vec<(String, String)>,
}

impl PackageCompatibilitySpec {
    /// Creates a baseline SABI package compatibility specification with no driver
    /// wire bridge and no extensions.
    #[must_use]
    pub fn new(
        package_name: impl Into<String>,
        target_sabi_epoch: u32,
        target_sabi_minor: u32,
    ) -> Self {
        Self {
            package_name: package_name.into(),
            target_sabi_epoch,
            target_sabi_minor,
            driver_abi_epoch: None,
            critical_extensions: Vec::new(),
            noncritical_extensions: Vec::new(),
            referenced_symbols: Vec::new(),
        }
    }
}

/// Service negotiation specification passed when planning mounts jointly with
/// [`SnapshotDirectory`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageServiceDemand {
    pub service: String,
    pub schema_name: String,
    pub minimum_minor: u32,
    pub required_feature_ids: Vec<u32>,
    pub supported_transport_kinds: Vec<i32>,
}

/// Mounted out-of-core unprivileged compatibility components for a historical
/// package or legacy driver (`ADR-0027` section 2.2).
#[derive(Clone, Debug)]
pub struct MountedCompatStack {
    pub package_name: String,
    pub sabi_negotiation: VersionNegotiationOutcome,
    pub sabi_adapter: Option<SabiEpochAdapterCell>,
    pub driver_shim: Option<DriverProtocolShim>,
    pub warnings: Vec<DeprecationWarningReceipt>,
    pub negotiated_bindings: Vec<ServiceBinding>,
}

impl MountedCompatStack {
    /// Number of out-of-core translation hops mounted on the package's path (`1` or `2`).
    #[must_use]
    pub const fn shim_hop_count(&self) -> usize {
        (self.sabi_adapter.is_some() as usize) + (self.driver_shim.is_some() as usize)
    }
}

/// Dynamic mount decision produced by [`CompatMountPlanner`].
#[derive(Clone, Debug)]
pub enum CompatMountDecision {
    /// Package targets current host epoch `N` (`VersionRoute::NativeCurrentEpoch`)
    /// and requires zero driver wire translation (`shim_hop_count() == 0`).
    NativePassthrough {
        package_name: String,
        sabi_negotiation: VersionNegotiationOutcome,
        warnings: Vec<DeprecationWarningReceipt>,
        negotiated_bindings: Vec<ServiceBinding>,
    },
    /// Package targets `N-1` / `N-2` (`VersionRoute::OutOfCoreCompatShim`) and/or
    /// requires a [`DriverProtocolShim`] (`LegacyStageBMinicborV1` or `N-1..=N-2`).
    MountOutOfCoreShim(Box<MountedCompatStack>),
}

impl CompatMountDecision {
    /// Returns `0` for [`Self::NativePassthrough`], or the mounted shim hop count
    /// (`>= 1`) for [`Self::MountOutOfCoreShim`].
    #[must_use]
    pub fn shim_hop_count(&self) -> usize {
        match self {
            Self::NativePassthrough { .. } => 0,
            Self::MountOutOfCoreShim(stack) => stack.shim_hop_count(),
        }
    }

    /// Returns all [`DeprecationWarningReceipt`]s emitted during mount planning.
    #[must_use]
    pub fn warnings(&self) -> &[DeprecationWarningReceipt] {
        match self {
            Self::NativePassthrough { warnings, .. } => warnings,
            Self::MountOutOfCoreShim(stack) => &stack.warnings,
        }
    }

    /// Returns negotiated `ServiceDirectory` bindings when directory negotiation was run.
    #[must_use]
    pub fn negotiated_bindings(&self) -> &[ServiceBinding] {
        match self {
            Self::NativePassthrough {
                negotiated_bindings,
                ..
            } => negotiated_bindings,
            Self::MountOutOfCoreShim(stack) => &stack.negotiated_bindings,
        }
    }
}

/// Fail-closed mount rejections produced by [`CompatMountPlanner`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompatMountError {
    /// `package_name` was empty, contained NUL, or exceeded `MAX_NAME_BYTES`.
    InvalidPackageName(String),
    /// SABI or `DriverProvider` version window (`0`, `> N`, `<= N-3`) or
    /// `[COMPAT-VER-002]` critical-extension adjudication failed.
    Compatibility(CompatibilityError),
    /// Referenced symbol or field slot in `DeprecationManifest` is `SUNSET` or
    /// permanently `RESERVED`.
    SunsetOrReservedContract {
        channel: String,
        symbol: String,
        field_number: Option<u32>,
        reason: &'static str,
    },
    /// Policy-governed `SnapshotDirectory` negotiation failed closed.
    DirectoryNegotiation(PolicyNegotiationError),
    /// Directory negotiation returned a domain `DirectoryError` instead of a binding.
    ServiceBindingUnavailable { service: String, error_code: i32 },
    /// Out-of-core adapter construction failed validation.
    AdapterInit(SabiAdapterError),
}

impl fmt::Display for CompatMountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPackageName(name) => {
                write!(
                    formatter,
                    "invalid package_name {name:?} in compatibility spec"
                )
            }
            Self::Compatibility(err) => write!(formatter, "compatibility policy rejection: {err}"),
            Self::SunsetOrReservedContract {
                channel,
                symbol,
                field_number: Some(field),
                reason,
            } => write!(
                formatter,
                "package references retired contract {symbol:?} (field {field}) in {channel:?}: {reason}"
            ),
            Self::SunsetOrReservedContract {
                channel,
                symbol,
                field_number: None,
                reason,
            } => write!(
                formatter,
                "package references retired contract {symbol:?} in {channel:?}: {reason}"
            ),
            Self::DirectoryNegotiation(err) => {
                write!(
                    formatter,
                    "ServiceDirectory policy negotiation failed: {err}"
                )
            }
            Self::ServiceBindingUnavailable {
                service,
                error_code,
            } => write!(
                formatter,
                "ServiceDirectory could not bind service {service:?} (code={error_code})"
            ),
            Self::AdapterInit(err) => write!(formatter, "failed to initialize compat shim: {err}"),
        }
    }
}

impl Error for CompatMountError {}

impl CompatMountError {
    /// Deterministically maps mount-time compatibility rejections to a wire-safe
    /// [`SabiFailure`].
    ///
    /// All version-window (`0`, `> N`, `<= N-3`), unknown-channel,
    /// `[COMPAT-VER-002]` critical-extension, and `SUNSET`/`RESERVED` rejections
    /// map to [`SabiErrorCode::AbiVersion`] (`E_ABI_VERSION`) with
    /// [`RetryDirective::DoNotRetry`].
    #[must_use]
    pub fn to_sabi_failure(&self) -> SabiFailure {
        match self {
            Self::Compatibility(err) => err.to_sabi_failure(),
            Self::SunsetOrReservedContract { .. } => SabiFailure {
                code: SabiErrorCode::AbiVersion.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message:
                    "package references a SUNSET or permanently RESERVED SABI contract element"
                        .to_owned(),
            },
            Self::DirectoryNegotiation(err) => err.to_sabi_failure(),
            Self::AdapterInit(err) => err.to_sabi_failure(),
            Self::InvalidPackageName(_) => SabiFailure {
                code: SabiErrorCode::InvalidArgument.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "package compatibility declaration has an invalid package_name"
                    .to_owned(),
            },
            Self::ServiceBindingUnavailable { .. } => SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "requested SABI service binding is unavailable in ServiceDirectory"
                    .to_owned(),
            },
        }
    }
}

/// Dynamic per-package out-of-core compatibility shim mount planner.
#[derive(Clone, Debug)]
pub struct CompatMountPlanner {
    manifest: DeprecationManifest,
    translation_rules: EpochTranslationRules,
    driver_trust_ceiling: ProviderTrustCeiling,
}

impl CompatMountPlanner {
    /// Creates a planner governed by an explicit [`DeprecationManifest`] (for
    /// example a multi-epoch evolution manifest in conformance/upgrade tests).
    #[must_use]
    pub fn new(manifest: DeprecationManifest) -> Self {
        Self {
            manifest,
            translation_rules: EpochTranslationRules::default(),
            driver_trust_ceiling: ProviderTrustCeiling::PreserveHonestAttestation,
        }
    }

    /// Creates a planner governed by the embedded canonical
    /// `CANONICAL_DEPRECATION_MANIFEST_YAML`.
    ///
    /// # Panics
    ///
    /// Panics only if the compile-time embedded canonical deprecation manifest is corrupted.
    #[must_use]
    pub fn with_canonical_manifest() -> Self {
        let manifest = canonical_deprecation_manifest().expect("canonical manifest is valid");
        Self::new(manifest)
    }

    /// Overrides the declarative [`EpochTranslationRules`] seeded into mounted
    /// [`SabiEpochAdapterCell`] instances.
    #[must_use]
    pub fn with_translation_rules(mut self, rules: EpochTranslationRules) -> Self {
        self.translation_rules = rules;
        self
    }

    /// Overrides the `[TASK-PROVIDER-001]` trust ceiling seeded into mounted
    /// historical frozen [`DriverProtocolShim`] instances (`LegacyStageBMinicborV1`
    /// is unconditionally `ForceUnverified`).
    #[must_use]
    pub const fn with_driver_trust_ceiling(mut self, ceiling: ProviderTrustCeiling) -> Self {
        self.driver_trust_ceiling = ceiling;
        self
    }

    /// Borrows the active [`DeprecationManifest`].
    #[must_use]
    pub const fn manifest(&self) -> &DeprecationManifest {
        &self.manifest
    }

    /// Adjudicates `spec` against `policy` and `self.manifest`, returning either
    /// [`CompatMountDecision::NativePassthrough`] (`0` shim hops) or
    /// [`CompatMountDecision::MountOutOfCoreShim`].
    ///
    /// # Errors
    ///
    /// Fails closed with [`CompatMountError`] on invalid package names, invalid
    /// SABI or driver epochs (`0`, `> N`, `<= N-3`), unknown `critical_extensions`,
    /// or `SUNSET`/`RESERVED` symbols and field slots.
    pub fn plan_mount(
        &self,
        spec: &PackageCompatibilitySpec,
        policy: &VersionCompatibilityPolicy<'_>,
    ) -> Result<CompatMountDecision, CompatMountError> {
        self.plan_mount_internal(spec, policy, None, &[])
    }

    /// Adjudicates `spec` against `policy` and `self.manifest`, AND negotiates
    /// all `service_demands` against `directory` using policy-governed
    /// `SnapshotDirectory::negotiate_with_policy`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`CompatMountError`] on any compatibility, deprecation,
    /// or directory negotiation failure.
    pub fn plan_mount_with_directory(
        &self,
        spec: &PackageCompatibilitySpec,
        policy: &VersionCompatibilityPolicy<'_>,
        directory: &mut SnapshotDirectory,
        service_demands: &[PackageServiceDemand],
    ) -> Result<CompatMountDecision, CompatMountError> {
        self.plan_mount_internal(spec, policy, Some(directory), service_demands)
    }

    #[allow(clippy::too_many_lines)]
    fn plan_mount_internal(
        &self,
        spec: &PackageCompatibilitySpec,
        policy: &VersionCompatibilityPolicy<'_>,
        directory: Option<&mut SnapshotDirectory>,
        service_demands: &[PackageServiceDemand],
    ) -> Result<CompatMountDecision, CompatMountError> {
        validate_package_name(&spec.package_name)?;

        let envelope_identity = SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: spec.target_sabi_epoch,
            minor: spec.target_sabi_minor,
            critical_extension_ids: spec.critical_extensions.clone(),
            non_critical_extension_ids: spec.noncritical_extensions.clone(),
        };
        let sabi_negotiation = policy
            .adjudicate_schema_identity(&envelope_identity)
            .map_err(CompatMountError::Compatibility)?;

        let mut warnings = Vec::new();

        // Adjudicate optional driver wire ABI epoch.
        let driver_shim = match spec.driver_abi_epoch {
            None => None,
            Some(DriverWireEpoch::LegacyStageBMinicborV1) => {
                self.audit_single_symbol(
                    SABI_DRIVER_PROVIDER_SCHEMA,
                    "LegacyStageBMinicborV1",
                    spec.target_sabi_epoch,
                    spec.target_sabi_minor,
                    &spec.package_name,
                    &mut warnings,
                )?;
                let shim =
                    DriverProtocolShim::new_legacy_minicbor(policy.host_epoch).map_err(|err| {
                        match err {
                            DriverShimError::InvalidDriverEpoch { .. } => {
                                CompatMountError::Compatibility(
                                    CompatibilityError::ZeroMajorVersion {
                                        schema: SABI_DRIVER_PROVIDER_SCHEMA.to_owned(),
                                    },
                                )
                            }
                            _ => CompatMountError::Compatibility(
                                CompatibilityError::MalformedProtobuf(err.to_string()),
                            ),
                        }
                    })?;
                Some(shim)
            }
            Some(DriverWireEpoch::FrozenDriverProviderEpoch(driver_major)) => {
                let driver_identity = SchemaIdentity {
                    name: SABI_DRIVER_PROVIDER_SCHEMA.to_owned(),
                    major: driver_major,
                    minor: 0,
                    critical_extension_ids: spec.critical_extensions.clone(),
                    non_critical_extension_ids: spec.noncritical_extensions.clone(),
                };
                let driver_outcome = policy
                    .adjudicate_schema_identity(&driver_identity)
                    .map_err(CompatMountError::Compatibility)?;
                match driver_outcome.route {
                    VersionRoute::NativeCurrentEpoch => None,
                    VersionRoute::OutOfCoreCompatShim {
                        client_epoch,
                        host_epoch,
                        ..
                    } => {
                        let shim = DriverProtocolShim::new_historical_frozen_epoch(
                            client_epoch,
                            host_epoch,
                            self.driver_trust_ceiling,
                        )
                        .map_err(|_| {
                            CompatMountError::Compatibility(CompatibilityError::UnsupportedMajor {
                                schema: SABI_DRIVER_PROVIDER_SCHEMA.to_owned(),
                                got: client_epoch,
                                supported: host_epoch,
                            })
                        })?;
                        Some(shim)
                    }
                }
            }
        };

        // Audit all declared `(channel, symbol_or_field)` references against `self.manifest`.
        for (channel, symbol) in &spec.referenced_symbols {
            self.audit_channel_and_symbol_or_field(
                channel,
                symbol,
                spec.target_sabi_epoch,
                spec.target_sabi_minor,
                &spec.package_name,
                &mut warnings,
            )?;
        }

        // Run policy-governed ServiceDirectory negotiations when requested.
        let mut negotiated_bindings = Vec::new();
        if let Some(dir) = directory {
            for demand in service_demands {
                let request = NegotiateServiceRequest {
                    schema: Some(SchemaIdentity {
                        name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
                        major: spec.target_sabi_epoch,
                        minor: spec.target_sabi_minor,
                        critical_extension_ids: spec.critical_extensions.clone(),
                        non_critical_extension_ids: spec.noncritical_extensions.clone(),
                    }),
                    service: demand.service.clone(),
                    schema_name: demand.schema_name.clone(),
                    // Directory bindings themselves are registered at the host's
                    // active schema major (normally 1 in v1.x) or caller target;
                    // negotiate_with_policy adjudicates both caller & target epochs.
                    major: resolve_registered_channel_major(
                        &demand.schema_name,
                        spec.target_sabi_epoch,
                        policy.host_epoch,
                        dir,
                        &demand.service,
                    ),
                    minimum_minor: demand.minimum_minor,
                    required_feature_ids: demand.required_feature_ids.clone(),
                    supported_transport_kinds: demand.supported_transport_kinds.clone(),
                };
                let outcome = dir
                    .negotiate_with_policy(&request, policy, &self.manifest)
                    .map_err(CompatMountError::DirectoryNegotiation)?;
                append_bounded_warnings(&mut warnings, &outcome.emitted_warnings);
                match outcome.response.result {
                    Some(negotiate_service_response::Result::Binding(binding)) => {
                        negotiated_bindings.push(binding);
                    }
                    Some(negotiate_service_response::Result::Error(dir_err)) => {
                        return Err(CompatMountError::ServiceBindingUnavailable {
                            service: demand.service.clone(),
                            error_code: dir_err.code,
                        });
                    }
                    None => {
                        return Err(CompatMountError::ServiceBindingUnavailable {
                            service: demand.service.clone(),
                            error_code: 0,
                        });
                    }
                }
            }
        }

        match (sabi_negotiation.route, driver_shim) {
            (VersionRoute::NativeCurrentEpoch, None) => {
                Ok(CompatMountDecision::NativePassthrough {
                    package_name: spec.package_name.clone(),
                    sabi_negotiation,
                    warnings,
                    negotiated_bindings,
                })
            }
            (route, maybe_driver_shim) => {
                let sabi_adapter = match route {
                    VersionRoute::NativeCurrentEpoch => None,
                    VersionRoute::OutOfCoreCompatShim {
                        client_epoch,
                        host_epoch,
                        ..
                    } => {
                        if policy.known_critical_extensions.len() > MAX_EXTENSION_IDS {
                            return Err(CompatMountError::Compatibility(
                                CompatibilityError::TooManyExtensionIds {
                                    schema: SABI_ENVELOPE_SCHEMA.to_owned(),
                                    actual: policy.known_critical_extensions.len(),
                                    maximum: MAX_EXTENSION_IDS,
                                },
                            ));
                        }
                        let mut cell = SabiEpochAdapterCell::new(
                            client_epoch,
                            host_epoch,
                            policy.max_backward_epochs,
                            policy.known_critical_extensions.to_vec(),
                            self.manifest.clone(),
                            self.translation_rules.clone(),
                        )
                        .map_err(CompatMountError::AdapterInit)?;
                        cell.push_warnings(&warnings);
                        Some(cell)
                    }
                };
                Ok(CompatMountDecision::MountOutOfCoreShim(Box::new(
                    MountedCompatStack {
                        package_name: spec.package_name.clone(),
                        sabi_negotiation,
                        sabi_adapter,
                        driver_shim: maybe_driver_shim,
                        warnings,
                        negotiated_bindings,
                    },
                )))
            }
        }
    }

    fn audit_channel_and_symbol_or_field(
        &self,
        channel: &str,
        symbol: &str,
        caller_major: u32,
        caller_minor: u32,
        service_label: &str,
        warnings: &mut Vec<DeprecationWarningReceipt>,
    ) -> Result<(), CompatMountError> {
        let channel_known = self.manifest.channel(channel).is_some()
            || schema_registry().iter().any(|desc| desc.name == channel);
        if !channel_known {
            return Err(CompatMountError::Compatibility(
                CompatibilityError::UnknownSchema(channel.to_owned()),
            ));
        }

        if let Some((parent_type, field_number)) = parse_field_slot_reference(symbol) {
            let verdict = self
                .manifest
                .audit_field_number(channel, parent_type, field_number);
            return apply_mount_verdict(
                verdict,
                channel,
                symbol,
                Some(field_number),
                caller_major,
                caller_minor,
                service_label,
                warnings,
            );
        }

        self.audit_single_symbol(
            channel,
            symbol,
            caller_major,
            caller_minor,
            service_label,
            warnings,
        )
    }

    fn audit_single_symbol(
        &self,
        channel: &str,
        symbol: &str,
        caller_major: u32,
        caller_minor: u32,
        service_label: &str,
        warnings: &mut Vec<DeprecationWarningReceipt>,
    ) -> Result<(), CompatMountError> {
        let verdict = self.manifest.audit_symbol(channel, symbol);
        apply_mount_verdict(
            verdict,
            channel,
            symbol,
            None,
            caller_major,
            caller_minor,
            service_label,
            warnings,
        )
    }
}

fn validate_package_name(name: &str) -> Result<(), CompatMountError> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.contains('\0') {
        return Err(CompatMountError::InvalidPackageName(name.to_owned()));
    }
    Ok(())
}

fn parse_field_slot_reference(symbol: &str) -> Option<(Option<&str>, u32)> {
    if let Some(num_str) = symbol.strip_prefix("field:") {
        let field_num = num_str.parse::<u32>().ok()?;
        return Some((None, field_num));
    }
    let (parent, num_str) = symbol.rsplit_once(".field:")?;
    if parent.is_empty() {
        return None;
    }
    let field_num = num_str.parse::<u32>().ok()?;
    Some((Some(parent), field_num))
}

#[allow(clippy::too_many_arguments)]
fn apply_mount_verdict(
    verdict: SymbolAuditVerdict<'_>,
    channel: &str,
    fallback_symbol: &str,
    fallback_field_number: Option<u32>,
    caller_major: u32,
    caller_minor: u32,
    service_label: &str,
    warnings: &mut Vec<DeprecationWarningReceipt>,
) -> Result<(), CompatMountError> {
    match verdict {
        SymbolAuditVerdict::Active(_) => Ok(()),
        SymbolAuditVerdict::Deprecated(entry) => {
            let receipt =
                entry_to_warning_receipt(entry, caller_major, caller_minor, service_label);
            append_bounded_warnings(warnings, &[receipt]);
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
            Err(CompatMountError::SunsetOrReservedContract {
                channel: channel.to_owned(),
                symbol,
                field_number,
                reason,
            })
        }
    }
}

fn append_bounded_warnings(
    target: &mut Vec<DeprecationWarningReceipt>,
    incoming: &[DeprecationWarningReceipt],
) {
    for warning in incoming {
        if target.len() >= MAX_SHIM_DEPRECATION_WARNINGS {
            target.remove(0);
        }
        target.push(warning.clone());
    }
}

fn resolve_registered_channel_major(
    schema_name: &str,
    target_sabi_epoch: u32,
    host_epoch: u32,
    directory: &SnapshotDirectory,
    service: &str,
) -> u32 {
    let probe = directory.resolve(&nlos_schema::sabi::v1::ResolveServiceRequest {
        schema: Some(nlos_schema::service_directory_schema_identity()),
        service: service.to_owned(),
    });
    if let Some(nlos_schema::sabi::v1::resolve_service_response::Result::Candidates(set)) =
        probe.result
    {
        if let Some(found) = set.candidates.iter().find_map(|cand| {
            let ver = cand.version.as_ref()?;
            (ver.schema_name == schema_name && ver.major == target_sabi_epoch).then_some(ver.major)
        }) {
            return found;
        }
        if let Some(found) = set.candidates.iter().find_map(|cand| {
            let ver = cand.version.as_ref()?;
            (ver.schema_name == schema_name && ver.major == host_epoch).then_some(ver.major)
        }) {
            return found;
        }
    }
    target_sabi_epoch
}
