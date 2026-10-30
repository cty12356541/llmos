//! Static `SABI` contract, deprecation lifecycle, `EffectDescriptor`, dependency
//! selector, and `[PKG-UPDATE-001]` migration-mode auditor (`W78-E3`).

use std::fmt::Write as _;
use std::path::Path;

use nlos_schema::{
    CompatibilityError, DeprecationEntry, DeprecationManifest, DeprecationManifestError,
    MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS, SymbolAuditVerdict, VersionCompatibilityPolicy,
    VersionRoute, canonical_deprecation_manifest, parse_deprecation_manifest, sabi,
};

use crate::json::{hex_encode, json_opt_string, json_opt_u32, json_string, json_u32_array};
use crate::manifest::{
    DataMigrationMode, DeclaredChannelUsage, DeclaredDependencySelector, DeclaredEffect,
    DeclaredSymbolRef, EffectClassDeclaration, InspectedPackageTarget, ManifestLoadError,
    inspect_package_target,
};

/// Severity classification of a static migration finding.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FindingSeverity {
    /// Fail-closed blocking contract, dependency, or governance violation.
    BlockingError,
    /// Grace-window deprecation warning (`[COMPAT-DEPRECATE-001]`).
    DeprecationWarning,
    /// Informational routing or governance note.
    Info,
}

impl FindingSeverity {
    /// Canonical uppercase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BlockingError => "BLOCKING_ERROR",
            Self::DeprecationWarning => "DEPRECATION_WARNING",
            Self::Info => "INFO",
        }
    }
}

/// High-level governance domain of a [`MigrationFinding`], driving CLI exit codes
/// and prescription compilation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FindingCategory {
    /// Disguised destructive `SQL`/data step declared under a non-`IRREVERSIBLE` mode (exit `4`).
    DisguisedIrreversibleMigration,
    /// Sunset/out-of-window epoch, reserved symbol, unknown critical extension, or floating
    /// dependency selector (exit `3`).
    ContractIncompatibility,
    /// Deprecated symbol within active grace window (exit `0` with structured diagnostic).
    DeprecationGraceWindow,
    /// Informational routing or governance note (exit `0`).
    GovernanceNotice,
}

impl FindingCategory {
    /// Canonical snake-case identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DisguisedIrreversibleMigration => "disguised_irreversible_migration",
            Self::ContractIncompatibility => "contract_incompatibility",
            Self::DeprecationGraceWindow => "deprecation_grace_window",
            Self::GovernanceNotice => "governance_notice",
        }
    }
}

/// Structured grace-window deprecation diagnostic (`[COMPAT-DEPRECATE-001]`, `ADR-0027`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeprecationDiagnostic {
    /// Owning `SABI` channel.
    pub channel: String,
    /// Deprecated symbol or field identifier.
    pub symbol: String,
    /// Epoch in which the symbol entered `DEPRECATED`.
    pub deprecated_in_epoch: u32,
    /// Earliest epoch at which the symbol may transition to `SUNSET`.
    pub earliest_sunset_epoch: u32,
    /// Remaining major epochs in the grace window (`earliest_sunset_epoch - host_epoch`).
    pub remaining_grace_epochs: u32,
    /// Replacement symbol recommended by `deprecation-manifest.yaml`.
    pub replacement_symbol: String,
    /// Optional machine-readable migration rule identifier.
    pub migration_rule_id: Option<String>,
    /// Human-readable rationale from the deprecation manifest.
    pub reason: String,
}

/// Single deterministic finding emitted by [`scan_inspected_target`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationFinding {
    /// Stable diagnostic code (e.g. `E_SUNSET_EPOCH`, `W_SYMBOL_DEPRECATED`).
    pub code: &'static str,
    /// Finding severity.
    pub severity: FindingSeverity,
    /// Governance category.
    pub category: FindingCategory,
    /// Target channel, symbol, or manifest section.
    pub subject: String,
    /// Detailed explanation.
    pub message: String,
    /// Optional replacement symbol or remediation target.
    pub suggested_replacement: Option<String>,
    /// Optional rule identifier.
    pub migration_rule_id: Option<String>,
}

/// Negotiated route classification for one `SABI` channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteClassification {
    /// Handled natively by current-epoch `N` core authorities.
    NativeCurrentEpoch,
    /// Routed through out-of-core `nlos-compat-shim` (`N-1` / `N-2`).
    OutOfCoreCompatShim {
        /// `host_epoch - client_major` (`1` or `2`).
        epoch_distance: u32,
    },
    /// Rejected fail-closed (`E_ABI_VERSION`).
    Blocked,
}

impl RouteClassification {
    /// Canonical snake-case identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeCurrentEpoch => "native_current_epoch",
            Self::OutOfCoreCompatShim { .. } => "out_of_core_compat_shim",
            Self::Blocked => "blocked",
        }
    }
}

/// Per-channel negotiation summary recorded in a [`ScanReport`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelRouteSummary {
    /// Canonical `SABI` channel name.
    pub channel: String,
    /// Package-declared channel major epoch.
    pub client_major: u32,
    /// Package-declared channel minor version.
    pub client_minor: u32,
    /// Host epoch used for evaluation.
    pub host_epoch: u32,
    /// Resulting route classification.
    pub route: RouteClassification,
}

/// Host compatibility and deprecation policy supplied to the static scanner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanOptions {
    /// Host `SABI` major epoch (`N`, default `1`).
    pub host_epoch: u32,
    /// Rolling backward-compatibility window in major epochs (`>= 2`, default `2`).
    pub max_backward_epochs: u32,
    /// Critical extension IDs supported by the target host.
    pub known_critical_extensions: Vec<u32>,
    /// Host `DRV-*` ABI epoch (`default == host_epoch`).
    pub host_driver_abi_epoch: u32,
    /// Validated `DeprecationManifest` used for symbol and slot auditing.
    pub deprecation_manifest: DeprecationManifest,
}

impl ScanOptions {
    /// Builds default options anchored to [`canonical_deprecation_manifest`].
    ///
    /// # Errors
    ///
    /// Propagates [`DeprecationManifestError`] if the embedded manifest fails validation.
    pub fn canonical_for_host_epoch(host_epoch: u32) -> Result<Self, DeprecationManifestError> {
        let deprecation_manifest = canonical_deprecation_manifest()?;
        Ok(Self {
            host_epoch,
            max_backward_epochs: MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
            known_critical_extensions: Vec::new(),
            host_driver_abi_epoch: host_epoch,
            deprecation_manifest,
        })
    }

    /// Builds options using a custom `deprecation-manifest.yaml` document.
    ///
    /// # Errors
    ///
    /// Fails closed with [`DeprecationManifestError`] if `manifest_yaml` violates any
    /// `ADR-0027` schema or lifecycle invariant.
    pub fn from_manifest_yaml(
        host_epoch: Option<u32>,
        manifest_yaml: &str,
    ) -> Result<Self, DeprecationManifestError> {
        let deprecation_manifest = parse_deprecation_manifest(manifest_yaml)?;
        let effective_epoch = host_epoch.unwrap_or(deprecation_manifest.current_epoch);
        Ok(Self {
            host_epoch: effective_epoch,
            max_backward_epochs: deprecation_manifest.min_compatibility_window_epochs,
            known_critical_extensions: Vec::new(),
            host_driver_abi_epoch: effective_epoch,
            deprecation_manifest,
        })
    }
}

/// Top-level error returned when loading inputs for `scan`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanError {
    /// Target package/manifest could not be loaded (CLI exit code `2`).
    Manifest(ManifestLoadError),
    /// Supplied `deprecation-manifest.yaml` is invalid (CLI exit code `2`).
    DeprecationManifest(DeprecationManifestError),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(err) => write!(formatter, "{err}"),
            Self::DeprecationManifest(err) => write!(formatter, "{err}"),
        }
    }
}

impl std::error::Error for ScanError {}

/// Complete deterministic result of scanning a package target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanReport {
    /// Normalized target metadata that was audited.
    pub target: InspectedPackageTarget,
    /// Host `SABI` epoch evaluated against.
    pub host_epoch: u32,
    /// Host backward-compatibility window (`N-1` / `N-2`).
    pub max_backward_epochs: u32,
    /// Per-channel negotiation routes.
    pub channel_routes: Vec<ChannelRouteSummary>,
    /// Structured grace-window deprecation diagnostics.
    pub deprecations: Vec<DeprecationDiagnostic>,
    /// Deterministically ordered findings.
    pub findings: Vec<MigrationFinding>,
    /// Count of `ACTIVE` symbols verified without warnings.
    pub active_symbols_verified: usize,
}

impl ScanReport {
    /// Returns `true` iff zero `BlockingError` findings were emitted.
    #[must_use]
    pub fn passed(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|item| item.severity == FindingSeverity::BlockingError)
    }

    /// Number of `BlockingError` findings.
    #[must_use]
    pub fn blocking_error_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|item| item.severity == FindingSeverity::BlockingError)
            .count()
    }

    /// Returns `true` if any blocking error is a `DisguisedIrreversibleMigration`.
    #[must_use]
    pub fn has_disguised_irreversible(&self) -> bool {
        self.findings.iter().any(|item| {
            item.severity == FindingSeverity::BlockingError
                && item.category == FindingCategory::DisguisedIrreversibleMigration
        })
    }

    /// Returns `true` if at least one channel routes via `nlos-compat-shim`.
    #[must_use]
    pub fn uses_compat_shim(&self) -> bool {
        self.channel_routes
            .iter()
            .any(|route| matches!(route.route, RouteClassification::OutOfCoreCompatShim { .. }))
    }

    /// Deterministic human-readable text rendering (zero timestamps).
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let (maj, min, pat) = self.target.version;
        let verdict = if self.passed() { "PASS" } else { "BLOCKED" };
        let _ = writeln!(
            out,
            "SCAN {verdict} package={} version={maj}.{min}.{pat} source={} target_epoch={}.{} host_epoch={}",
            hex_encode(self.target.package_id.as_bytes()),
            self.target.source_kind.as_str(),
            self.target.target_sabi_epoch,
            self.target.target_sabi_minor,
            self.host_epoch,
        );
        for route in &self.channel_routes {
            match route.route {
                RouteClassification::OutOfCoreCompatShim { epoch_distance } => {
                    let _ = writeln!(
                        out,
                        "CHANNEL {} client={}.{} host={} route={} distance={epoch_distance}",
                        route.channel,
                        route.client_major,
                        route.client_minor,
                        route.host_epoch,
                        route.route.as_str(),
                    );
                }
                _ => {
                    let _ = writeln!(
                        out,
                        "CHANNEL {} client={}.{} host={} route={}",
                        route.channel,
                        route.client_major,
                        route.client_minor,
                        route.host_epoch,
                        route.route.as_str(),
                    );
                }
            }
        }
        for dep in &self.deprecations {
            let _ = writeln!(
                out,
                "DEPRECATION {}::{} deprecated_in={} earliest_sunset={} grace_remaining={} replacement={} rule={}",
                dep.channel,
                dep.symbol,
                dep.deprecated_in_epoch,
                dep.earliest_sunset_epoch,
                dep.remaining_grace_epochs,
                dep.replacement_symbol,
                dep.migration_rule_id.as_deref().unwrap_or("none"),
            );
        }
        for finding in &self.findings {
            let _ = writeln!(
                out,
                "FINDING [{}] {} ({}): {}",
                finding.severity.as_str(),
                finding.code,
                finding.subject,
                finding.message,
            );
        }
        out
    }

    /// Canonical deterministic JSON rendering (zero timestamps).
    #[must_use]
    pub fn render_json(&self) -> String {
        let (maj, min, pat) = self.target.version;
        let routes_json: Vec<String> = self
            .channel_routes
            .iter()
            .map(|route| {
                let dist = match route.route {
                    RouteClassification::OutOfCoreCompatShim { epoch_distance } => {
                        Some(epoch_distance)
                    }
                    _ => None,
                };
                format!(
                    "{{\"channel\": {}, \"client_major\": {}, \"client_minor\": {}, \
                     \"epoch_distance\": {}, \"host_epoch\": {}, \"route\": {}}}",
                    json_string(&route.channel),
                    route.client_major,
                    route.client_minor,
                    json_opt_u32(dist),
                    route.host_epoch,
                    json_string(route.route.as_str()),
                )
            })
            .collect();

        let deps_json: Vec<String> = self
            .deprecations
            .iter()
            .map(|dep| {
                format!(
                    "{{\"channel\": {}, \"deprecated_in_epoch\": {}, \"earliest_sunset_epoch\": {}, \
                     \"migration_rule_id\": {}, \"reason\": {}, \"remaining_grace_epochs\": {}, \
                     \"replacement_symbol\": {}, \"symbol\": {}}}",
                    json_string(&dep.channel),
                    dep.deprecated_in_epoch,
                    dep.earliest_sunset_epoch,
                    json_opt_string(dep.migration_rule_id.as_deref()),
                    json_string(&dep.reason),
                    dep.remaining_grace_epochs,
                    json_string(&dep.replacement_symbol),
                    json_string(&dep.symbol),
                )
            })
            .collect();

        let findings_json: Vec<String> = self
            .findings
            .iter()
            .map(|item| {
                format!(
                    "{{\"category\": {}, \"code\": {}, \"message\": {}, \
                     \"migration_rule_id\": {}, \"severity\": {}, \"subject\": {}, \
                     \"suggested_replacement\": {}}}",
                    json_string(item.category.as_str()),
                    json_string(item.code),
                    json_string(&item.message),
                    json_opt_string(item.migration_rule_id.as_deref()),
                    json_string(item.severity.as_str()),
                    json_string(&item.subject),
                    json_opt_string(item.suggested_replacement.as_deref()),
                )
            })
            .collect();

        format!(
            "{{\n  \"active_symbols_verified\": {},\n  \"blocking_error_count\": {},\n  \
             \"channel_routes\": [{}],\n  \"critical_extensions\": {},\n  \
             \"data_migration_mode\": {},\n  \"deprecation_count\": {},\n  \
             \"deprecations\": [{}],\n  \"findings\": [{}],\n  \"host_epoch\": {},\n  \
             \"non_critical_extensions\": {},\n  \"package_id\": {},\n  \
             \"package_version\": {},\n  \"passed\": {},\n  \"source_kind\": {},\n  \
             \"target_sabi_epoch\": {},\n  \"target_sabi_minor\": {}\n}}",
            self.active_symbols_verified,
            self.blocking_error_count(),
            routes_json.join(", "),
            json_u32_array(&self.target.critical_extensions),
            json_opt_string(
                self.target
                    .data_migration_mode
                    .map(DataMigrationMode::as_str)
            ),
            self.deprecations.len(),
            deps_json.join(", "),
            findings_json.join(", "),
            self.host_epoch,
            json_u32_array(&self.target.non_critical_extensions),
            json_string(&hex_encode(self.target.package_id.as_bytes())),
            json_string(&format!("{maj}.{min}.{pat}")),
            self.passed(),
            json_string(self.target.source_kind.as_str()),
            self.target.target_sabi_epoch,
            self.target.target_sabi_minor,
        )
    }
}

/// Loads `path` and audits its contract, symbol, effect, dependency, and migration
/// declarations against `options`.
///
/// # Errors
///
/// Returns [`ScanError::Manifest`] if `path` cannot be loaded or fails `PKG-CONF-*`
/// conformance checks.
pub fn scan_path(path: &Path, options: &ScanOptions) -> Result<ScanReport, ScanError> {
    let target = inspect_package_target(path).map_err(ScanError::Manifest)?;
    Ok(scan_inspected_target(target, options))
}

/// Audits an already-loaded [`InspectedPackageTarget`] against `options`.
#[must_use]
pub fn scan_inspected_target(target: InspectedPackageTarget, options: &ScanOptions) -> ScanReport {
    let mut findings = Vec::new();
    let mut deprecations = Vec::new();
    let mut channel_routes = Vec::new();
    let mut active_symbols_verified = 0usize;

    let policy = VersionCompatibilityPolicy::with_backward_window(
        options.host_epoch,
        options.max_backward_epochs,
        &options.known_critical_extensions,
    );

    for ch_decl in &target.channels {
        audit_channel_declaration(ch_decl, &policy, &mut channel_routes, &mut findings);
    }
    if let Some(driver_epoch) = target.driver_abi_epoch {
        audit_driver_abi_epoch(driver_epoch, options, &mut findings);
    }
    for sym_ref in &target.symbols {
        audit_symbol_reference(
            sym_ref,
            options,
            &mut active_symbols_verified,
            &mut deprecations,
            &mut findings,
        );
    }
    for effect in &target.effects {
        audit_effect_declaration(effect, &mut findings);
    }
    for dep in &target.dependencies {
        audit_dependency_selector(dep, &mut findings);
    }
    audit_migration_plan(&target, &mut findings);

    findings.sort_by(|left, right| {
        (
            left.severity,
            left.category,
            left.code,
            left.subject.as_str(),
            left.message.as_str(),
        )
            .cmp(&(
                right.severity,
                right.category,
                right.code,
                right.subject.as_str(),
                right.message.as_str(),
            ))
    });
    deprecations.sort_by(|left, right| {
        (left.channel.as_str(), left.symbol.as_str())
            .cmp(&(right.channel.as_str(), right.symbol.as_str()))
    });

    ScanReport {
        target,
        host_epoch: options.host_epoch,
        max_backward_epochs: options.max_backward_epochs,
        channel_routes,
        deprecations,
        findings,
        active_symbols_verified,
    }
}

fn audit_channel_declaration(
    ch_decl: &DeclaredChannelUsage,
    policy: &VersionCompatibilityPolicy<'_>,
    channel_routes: &mut Vec<ChannelRouteSummary>,
    findings: &mut Vec<MigrationFinding>,
) {
    let identity = sabi::v1::SchemaIdentity {
        name: ch_decl.channel.clone(),
        major: ch_decl.major,
        minor: ch_decl.minor,
        critical_extension_ids: ch_decl.critical_extensions.clone(),
        non_critical_extension_ids: ch_decl.non_critical_extensions.clone(),
    };

    match policy.adjudicate_schema_identity(&identity) {
        Ok(outcome) => {
            let route = match outcome.route {
                VersionRoute::NativeCurrentEpoch => RouteClassification::NativeCurrentEpoch,
                VersionRoute::OutOfCoreCompatShim { epoch_distance, .. } => {
                    findings.push(MigrationFinding {
                        code: "I_COMPAT_SHIM_ROUTE",
                        severity: FindingSeverity::Info,
                        category: FindingCategory::GovernanceNotice,
                        subject: ch_decl.channel.clone(),
                        message: format!(
                            "channel {} v{}.{} is {} epoch(s) behind host epoch {} and will route \
                             transiently via out-of-core nlos-compat-shim",
                            ch_decl.channel,
                            ch_decl.major,
                            ch_decl.minor,
                            epoch_distance,
                            policy.host_epoch,
                        ),
                        suggested_replacement: Some(format!(
                            "{} {}.0",
                            ch_decl.channel, policy.host_epoch
                        )),
                        migration_rule_id: None,
                    });
                    RouteClassification::OutOfCoreCompatShim { epoch_distance }
                }
            };
            if !outcome.retained_non_critical_extensions.is_empty() {
                findings.push(MigrationFinding {
                    code: "I_NON_CRITICAL_EXTENSIONS_RETAINED",
                    severity: FindingSeverity::Info,
                    category: FindingCategory::GovernanceNotice,
                    subject: ch_decl.channel.clone(),
                    message: format!(
                        "retained {} non-critical extension ID(s) without blocking negotiation",
                        outcome.retained_non_critical_extensions.len()
                    ),
                    suggested_replacement: None,
                    migration_rule_id: None,
                });
            }
            channel_routes.push(ChannelRouteSummary {
                channel: ch_decl.channel.clone(),
                client_major: ch_decl.major,
                client_minor: ch_decl.minor,
                host_epoch: policy.host_epoch,
                route,
            });
        }
        Err(err) => {
            let (code, suggestion) = classify_compat_error(&err, policy.host_epoch);
            findings.push(MigrationFinding {
                code,
                severity: FindingSeverity::BlockingError,
                category: FindingCategory::ContractIncompatibility,
                subject: ch_decl.channel.clone(),
                message: err.to_string(),
                suggested_replacement: suggestion,
                migration_rule_id: None,
            });
            channel_routes.push(ChannelRouteSummary {
                channel: ch_decl.channel.clone(),
                client_major: ch_decl.major,
                client_minor: ch_decl.minor,
                host_epoch: policy.host_epoch,
                route: RouteClassification::Blocked,
            });
        }
    }
}

fn classify_compat_error(
    error: &CompatibilityError,
    host_epoch: u32,
) -> (&'static str, Option<String>) {
    match error {
        CompatibilityError::SunsetEpoch { schema, .. } => {
            ("E_SUNSET_EPOCH", Some(format!("{schema} {host_epoch}.0")))
        }
        CompatibilityError::UnsupportedMajor { schema, .. }
        | CompatibilityError::ZeroMajorVersion { schema } => (
            "E_UNSUPPORTED_SABI_EPOCH",
            Some(format!("{schema} {host_epoch}.0")),
        ),
        CompatibilityError::UnsupportedCriticalExtension { extension_id, .. } => (
            "E_UNKNOWN_CRITICAL_EXTENSION",
            Some(format!(
                "remove critical extension {extension_id} or downgrade to non-critical-extensions"
            )),
        ),
        CompatibilityError::UnknownSchema(_) => ("E_UNKNOWN_SABI_CHANNEL", None),
        _ => ("E_SABI_POLICY_VIOLATION", None),
    }
}

fn audit_driver_abi_epoch(
    driver_epoch: u32,
    options: &ScanOptions,
    findings: &mut Vec<MigrationFinding>,
) {
    let min_supported = options
        .host_driver_abi_epoch
        .saturating_sub(options.max_backward_epochs)
        .max(1);
    if driver_epoch == 0
        || driver_epoch > options.host_driver_abi_epoch
        || driver_epoch < min_supported
    {
        findings.push(MigrationFinding {
            code: "E_DRIVER_ABI_EPOCH_OUT_OF_WINDOW",
            severity: FindingSeverity::BlockingError,
            category: FindingCategory::ContractIncompatibility,
            subject: "driver-abi-epoch".to_owned(),
            message: format!(
                "driver-abi-epoch={driver_epoch} is outside host supported window \
                 {min_supported}..={}",
                options.host_driver_abi_epoch
            ),
            suggested_replacement: Some(format!(
                "driver-abi-epoch = {}",
                options.host_driver_abi_epoch
            )),
            migration_rule_id: None,
        });
    }
}

fn audit_symbol_reference(
    sym_ref: &DeclaredSymbolRef,
    options: &ScanOptions,
    active_symbols_verified: &mut usize,
    deprecations: &mut Vec<DeprecationDiagnostic>,
    findings: &mut Vec<MigrationFinding>,
) {
    let subject = format!("{}::{}", sym_ref.channel, sym_ref.symbol);
    if options
        .deprecation_manifest
        .channel(&sym_ref.channel)
        .is_none()
    {
        findings.push(MigrationFinding {
            code: "E_UNKNOWN_SABI_CHANNEL",
            severity: FindingSeverity::BlockingError,
            category: FindingCategory::ContractIncompatibility,
            subject,
            message: format!(
                "symbol {:?} references unregistered SABI channel {:?}",
                sym_ref.symbol, sym_ref.channel
            ),
            suggested_replacement: None,
            migration_rule_id: None,
        });
        return;
    }

    let sym_verdict = options
        .deprecation_manifest
        .audit_symbol(&sym_ref.channel, &sym_ref.symbol);
    let field_verdict = sym_ref.field_number.map(|num| {
        options.deprecation_manifest.audit_field_number(
            &sym_ref.channel,
            sym_ref.parent_type.as_deref(),
            num,
        )
    });

    if let Some(SymbolAuditVerdict::SunsetOrReserved { entry, reason }) = field_verdict {
        record_sunset_finding(&subject, entry, reason, findings);
        return;
    }
    match sym_verdict {
        SymbolAuditVerdict::Active(_) => {
            if let Some(SymbolAuditVerdict::Deprecated(entry)) = field_verdict {
                record_deprecated_entry(
                    entry,
                    options.host_epoch,
                    &subject,
                    deprecations,
                    findings,
                );
            } else {
                *active_symbols_verified += 1;
            }
        }
        SymbolAuditVerdict::Deprecated(entry) => {
            record_deprecated_entry(entry, options.host_epoch, &subject, deprecations, findings);
        }
        SymbolAuditVerdict::SunsetOrReserved { entry, reason } => {
            record_sunset_finding(&subject, entry, reason, findings);
        }
    }
}

fn record_deprecated_entry(
    entry: &DeprecationEntry,
    host_epoch: u32,
    subject: &str,
    deprecations: &mut Vec<DeprecationDiagnostic>,
    findings: &mut Vec<MigrationFinding>,
) {
    let deprecated_in = entry.deprecated_in_epoch.unwrap_or(1);
    let earliest_sunset = entry
        .earliest_sunset_epoch
        .unwrap_or(deprecated_in + MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS);
    let replacement = entry
        .replacement
        .clone()
        .unwrap_or_else(|| "<unspecified>".to_owned());
    let reason = entry
        .reason
        .clone()
        .unwrap_or_else(|| "deprecated SABI contract element".to_owned());

    if host_epoch >= earliest_sunset {
        findings.push(MigrationFinding {
            code: "E_DEPRECATION_WINDOW_EXPIRED",
            severity: FindingSeverity::BlockingError,
            category: FindingCategory::ContractIncompatibility,
            subject: subject.to_owned(),
            message: format!(
                "deprecated symbol {:?} reached earliest_sunset_epoch={earliest_sunset} at \
                 host_epoch={host_epoch}; migrate to {replacement:?}",
                entry.symbol
            ),
            suggested_replacement: Some(replacement),
            migration_rule_id: entry.migration_rule_id.clone(),
        });
        return;
    }

    let remaining_grace_epochs = earliest_sunset - host_epoch;
    deprecations.push(DeprecationDiagnostic {
        channel: entry.channel.clone(),
        symbol: entry.symbol.clone(),
        deprecated_in_epoch: deprecated_in,
        earliest_sunset_epoch: earliest_sunset,
        remaining_grace_epochs,
        replacement_symbol: replacement.clone(),
        migration_rule_id: entry.migration_rule_id.clone(),
        reason: reason.clone(),
    });
    findings.push(MigrationFinding {
        code: "W_SYMBOL_DEPRECATED",
        severity: FindingSeverity::DeprecationWarning,
        category: FindingCategory::DeprecationGraceWindow,
        subject: subject.to_owned(),
        message: format!(
            "deprecated in epoch {deprecated_in} (earliest sunset epoch {earliest_sunset}, \
             {remaining_grace_epochs} grace epoch(s) remaining): {reason}; replace with {replacement}"
        ),
        suggested_replacement: Some(replacement),
        migration_rule_id: entry.migration_rule_id.clone(),
    });
}

fn record_sunset_finding(
    subject: &str,
    entry: Option<&DeprecationEntry>,
    reason: &'static str,
    findings: &mut Vec<MigrationFinding>,
) {
    let replacement = entry.and_then(|item| item.replacement.clone());
    let rule_id = entry.and_then(|item| item.migration_rule_id.clone());
    findings.push(MigrationFinding {
        code: "E_SYMBOL_SUNSET_OR_RESERVED",
        severity: FindingSeverity::BlockingError,
        category: FindingCategory::ContractIncompatibility,
        subject: subject.to_owned(),
        message: match &replacement {
            Some(rep) => format!("{reason}; migrate to {rep}"),
            None => reason.to_owned(),
        },
        suggested_replacement: replacement,
        migration_rule_id: rule_id,
    });
}

fn audit_effect_declaration(effect: &DeclaredEffect, findings: &mut Vec<MigrationFinding>) {
    match effect.effect_class {
        EffectClassDeclaration::Compensatable => {
            if effect.compensation_handler.is_none() {
                findings.push(MigrationFinding {
                    code: "E_COMPENSATABLE_MISSING_HANDLER",
                    severity: FindingSeverity::BlockingError,
                    category: FindingCategory::ContractIncompatibility,
                    subject: format!("effect::{}", effect.effect_id),
                    message: format!(
                        "effect {:?} is declared COMPENSATABLE without a non-empty \
                         `compensation=<handler>` binding",
                        effect.effect_id
                    ),
                    suggested_replacement: Some(format!(
                        "effect = {} COMPENSATABLE compensation=<rollback_handler>",
                        effect.effect_id
                    )),
                    migration_rule_id: None,
                });
            }
        }
        EffectClassDeclaration::Irreversible => {
            findings.push(MigrationFinding {
                code: "I_EFFECT_REQUIRES_Z4_CONSENT",
                severity: FindingSeverity::Info,
                category: FindingCategory::GovernanceNotice,
                subject: format!("effect::{}", effect.effect_id),
                message: format!(
                    "effect {:?} is IRREVERSIBLE and requires runtime Z4 Trusted UI consent",
                    effect.effect_id
                ),
                suggested_replacement: None,
                migration_rule_id: None,
            });
        }
        EffectClassDeclaration::ReadOnly | EffectClassDeclaration::Reversible => {}
    }
}

fn audit_dependency_selector(
    dep: &DeclaredDependencySelector,
    findings: &mut Vec<MigrationFinding>,
) {
    let valid_kinds = [
        "package", "skill", "tool", "model", "artifact", "topic", "service",
    ];
    let subject = format!("dependency::{}:{}", dep.entity_kind, dep.raw_selector);
    if !valid_kinds.contains(&dep.entity_kind.as_str()) {
        findings.push(MigrationFinding {
            code: "E_INVALID_DEPENDENCY_KIND",
            severity: FindingSeverity::BlockingError,
            category: FindingCategory::ContractIncompatibility,
            subject,
            message: format!(
                "unsupported dependency kind {:?}; expected one of {}",
                dep.entity_kind,
                valid_kinds.join(", ")
            ),
            suggested_replacement: None,
            migration_rule_id: None,
        });
        return;
    }

    if let Err(reason) = validate_pinned_selector(&dep.raw_selector) {
        let base_name = dep
            .raw_selector
            .split(['@', ':'])
            .next()
            .unwrap_or("target");
        findings.push(MigrationFinding {
            code: "E_FLOATING_DEPENDENCY_SELECTOR",
            severity: FindingSeverity::BlockingError,
            category: FindingCategory::ContractIncompatibility,
            subject,
            message: format!(
                "[PLAN-DEPENDENCY-001] floating or unpinned dependency selector {:?} is \
                 forbidden ({reason})",
                dep.raw_selector
            ),
            suggested_replacement: Some(format!("{base_name}@gen:1")),
            migration_rule_id: Some("PLAN-DEPENDENCY-001".to_owned()),
        });
    }
}

fn validate_pinned_selector(selector: &str) -> Result<(), &'static str> {
    let lower = selector.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return Err("empty selector");
    }
    if lower == "latest"
        || lower.ends_with(":latest")
        || lower.ends_with("@latest")
        || lower.contains('*')
        || lower.starts_with('^')
        || lower.starts_with('~')
        || lower.contains(">=")
        || lower.ends_with(":head")
        || lower.ends_with("@head")
        || lower.ends_with(":main")
        || lower.ends_with("@main")
    {
        return Err("floating tag (`latest`, wildcard, branch, or open range) is prohibited");
    }

    let Some((entity, pin)) = selector.split_once('@') else {
        return Err("missing explicit `@gen:<N>`, `@digest:<hex64>`, or `@version:<x.y.z>` pin");
    };
    if entity.trim().is_empty() {
        return Err("missing entity identifier before `@`");
    }
    if let Some(gen_raw) = pin.strip_prefix("gen:") {
        let parsed = gen_raw
            .parse::<u64>()
            .map_err(|_| "invalid integer in `@gen:<N>`")?;
        if parsed == 0 {
            return Err("generation in `@gen:<N>` must be >= 1");
        }
        return Ok(());
    }
    if let Some(digest_raw) = pin.strip_prefix("digest:") {
        if digest_raw.len() != 64 || !digest_raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("`@digest:<hex64>` requires 64 hex characters");
        }
        return Ok(());
    }
    if let Some(ver_raw) = pin.strip_prefix("version:") {
        let parts: Vec<&str> = ver_raw.split('.').collect();
        if parts.len() != 3 || !parts.iter().all(|p| p.parse::<u32>().is_ok()) {
            return Err("`@version:<major.minor.patch>` requires exact numeric semver");
        }
        return Ok(());
    }
    Err("pin suffix must start with `gen:`, `digest:`, or `version:`")
}

/// Classification of a `SQL` or data-migration step's structural safety.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationStepHazard {
    /// Destructive change (`DROP TABLE`, `DROP COLUMN`, `TRUNCATE`, type narrowing, or one-way
    /// format rewrite) that is physically forbidden outside `DataMigrationMode::Irreversible`.
    DestructiveIrreversible { reason: &'static str },
    /// Non-destructive structural change (`RENAME` or `ADD COLUMN ... NOT NULL` without `DEFAULT`)
    /// that breaks live `v1` writers in `BackwardCompatibleWindow` mode.
    BreaksBackwardWindow { reason: &'static str },
}

/// Inspects one `SQL` or migration-plan statement for destructive or window-breaking operations.
#[must_use]
pub fn classify_migration_step_hazard(statement: &str) -> Option<MigrationStepHazard> {
    let collapsed = collapse_upper_tokens(statement);
    if collapsed.contains("DROP TABLE") {
        return Some(MigrationStepHazard::DestructiveIrreversible {
            reason: "DROP TABLE permanently destroys prior container schema/data",
        });
    }
    if collapsed.contains("DROP COLUMN") {
        return Some(MigrationStepHazard::DestructiveIrreversible {
            reason: "DROP COLUMN permanently destroys column values needed for v1 rollback",
        });
    }
    if collapsed.contains("TRUNCATE ") || collapsed.starts_with("TRUNCATE;") {
        return Some(MigrationStepHazard::DestructiveIrreversible {
            reason: "TRUNCATE permanently purges table rows",
        });
    }
    if collapsed.contains("DELETE FROM") {
        return Some(MigrationStepHazard::DestructiveIrreversible {
            reason: "DELETE FROM purges existing user rows destructively",
        });
    }
    if collapsed.contains("ALTER COLUMN") && collapsed.contains(" TYPE ") {
        return Some(MigrationStepHazard::DestructiveIrreversible {
            reason: "in-place column type rewrite is destructive across versions",
        });
    }
    if collapsed.contains("NARROW_ENUM")
        || collapsed.contains("ENUM_NARROW")
        || collapsed.contains("ONE_WAY_REWRITE")
    {
        return Some(MigrationStepHazard::DestructiveIrreversible {
            reason: "narrowing enum variants or one-way format rewrite is irreversible",
        });
    }
    if collapsed.contains("RENAME COLUMN") || collapsed.contains("RENAME TO") {
        return Some(MigrationStepHazard::BreaksBackwardWindow {
            reason: "renaming table/column breaks concurrent v1 reader/writer queries",
        });
    }
    if collapsed.contains("ADD COLUMN")
        && collapsed.contains("NOT NULL")
        && !collapsed.contains("DEFAULT")
    {
        return Some(MigrationStepHazard::BreaksBackwardWindow {
            reason: "ADD COLUMN ... NOT NULL without DEFAULT breaks v1 inserts during window",
        });
    }
    None
}

fn collapse_upper_tokens(statement: &str) -> String {
    statement
        .split_whitespace()
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn audit_migration_plan(target: &InspectedPackageTarget, findings: &mut Vec<MigrationFinding>) {
    if !target.migration_steps.is_empty() && target.data_migration_mode.is_none() {
        findings.push(MigrationFinding {
            code: "E_MISSING_DATA_MIGRATION_MODE",
            severity: FindingSeverity::BlockingError,
            category: FindingCategory::ContractIncompatibility,
            subject: "data-migration-mode".to_owned(),
            message:
                "package declares migration-step entries without declaring data-migration-mode"
                    .to_owned(),
            suggested_replacement: Some(
                "data-migration-mode = BACKWARD_COMPATIBLE_WINDOW".to_owned(),
            ),
            migration_rule_id: Some("PKG-UPDATE-001".to_owned()),
        });
    }

    for (idx, step) in target.migration_steps.iter().enumerate() {
        let step_num = idx + 1;
        let subject = format!("migration-step#{step_num}");
        if let Some(hazard) = classify_migration_step_hazard(step) {
            match hazard {
                MigrationStepHazard::DestructiveIrreversible { reason } => {
                    if target.data_migration_mode != Some(DataMigrationMode::Irreversible) {
                        let declared = target
                            .data_migration_mode
                            .map_or("UNSPECIFIED", DataMigrationMode::as_str);
                        findings.push(MigrationFinding {
                            code: "E_DISGUISED_IRREVERSIBLE_MIGRATION",
                            severity: FindingSeverity::BlockingError,
                            category: FindingCategory::DisguisedIrreversibleMigration,
                            subject,
                            message: format!(
                                "[RISK-E-05] DisguisedIrreversibleMigration: step {step_num} \
                                 ({step:?}) is destructive ({reason}) while data-migration-mode \
                                 is {declared}"
                            ),
                            suggested_replacement: Some(
                                "declare `data-migration-mode = IRREVERSIBLE` with pre-upgrade \
                                 SHA-256 backup + Z4 consent, or replace with additive migration"
                                    .to_owned(),
                            ),
                            migration_rule_id: Some("RISK-E-05".to_owned()),
                        });
                    }
                }
                MigrationStepHazard::BreaksBackwardWindow { reason } => {
                    if target.data_migration_mode
                        == Some(DataMigrationMode::BackwardCompatibleWindow)
                    {
                        findings.push(MigrationFinding {
                            code: "E_WINDOW_SCHEMA_INCOMPATIBLE",
                            severity: FindingSeverity::BlockingError,
                            category: FindingCategory::ContractIncompatibility,
                            subject,
                            message: format!(
                                "[PKG-UPDATE-001] step {step_num} ({step:?}) violates \
                                 BACKWARD_COMPATIBLE_WINDOW: {reason}"
                            ),
                            suggested_replacement: Some(
                                "add a DEFAULT value for new NOT NULL columns or use \
                                 SNAPSHOT_ASSISTED_ROLLBACK"
                                    .to_owned(),
                            ),
                            migration_rule_id: Some("PKG-UPDATE-001".to_owned()),
                        });
                    }
                }
            }
        }
    }
}
