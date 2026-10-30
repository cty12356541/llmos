//! Deterministic `UpgradePrescription` compiler (`W78-E3`).
//!
//! Compiles a [`ScanReport`] into an ordered, wall-clock-free remediation plan
//! covering symbol replacements, manifest `SABI` epoch & extension updates,
//! dependency selector pinning, `[PKG-UPDATE-001]` migration-mode corrections,
//! and `nlos-compat-shim` grace-window eligibility.

use std::fmt::Write as _;

use crate::json::{hex_encode, json_opt_string, json_opt_u32, json_string};
use crate::scanner::{FindingSeverity, RouteClassification, ScanReport};

/// Runtime admission posture of the scanned package prior to applying source edits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionPosture {
    /// Package targets `host_epoch` natively with zero blocking findings.
    NativeCurrentReady,
    /// Package has zero blocking findings and can run transiently via out-of-core
    /// `nlos-compat-shim` (`N-1` / `N-2`) or deprecation grace window while upgrading.
    TransientCompatShimEligible,
    /// Package has one or more blocking findings (`SUNSET`, unknown critical extension,
    /// floating dependency, or disguised irreversible migration) and MUST be migrated in
    /// source before admission.
    SourceMigrationRequired,
}

impl ExecutionPosture {
    /// Canonical snake-case identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeCurrentReady => "native_current_ready",
            Self::TransientCompatShimEligible => "transient_compat_shim_eligible",
            Self::SourceMigrationRequired => "source_migration_required",
        }
    }
}

/// Kind of remediation step in an [`UpgradePrescription`].
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RemediationKind {
    /// Reclassify disguised destructive migration or rewrite as additive expand-contract.
    FixDisguisedIrreversible,
    /// Pin floating dependency selector (`[PLAN-DEPENDENCY-001]`).
    PinDependencySelector,
    /// Update `target-sabi-epoch` / channel major version or `driver-abi-epoch`.
    UpdateManifestEpoch,
    /// Remove unsupported critical extension or move to `non-critical-extensions`.
    UpdateExtensionList,
    /// Supply mandatory compensation handler on `COMPENSATABLE` effect.
    BindEffectCompensation,
    /// Replace a `SUNSET` or `DEPRECATED` symbol with its canonical replacement.
    ReplaceContractSymbol,
}

impl RemediationKind {
    /// Canonical snake-case identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FixDisguisedIrreversible => "fix_disguised_irreversible",
            Self::PinDependencySelector => "pin_dependency_selector",
            Self::UpdateManifestEpoch => "update_manifest_epoch",
            Self::UpdateExtensionList => "update_extension_list",
            Self::BindEffectCompensation => "bind_effect_compensation",
            Self::ReplaceContractSymbol => "replace_contract_symbol",
        }
    }
}

/// Single ordered remediation step inside an [`UpgradePrescription`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemediationAction {
    /// 1-based execution order in the prescription.
    pub step_order: usize,
    /// Remediation category.
    pub kind: RemediationKind,
    /// `true` if required immediately to unblock admission; `false` if scheduled within
    /// a deprecation/shim grace window.
    pub mandatory_before_admission: bool,
    /// Target symbol, channel, or manifest directive.
    pub target: String,
    /// Concrete replacement value or manifest edit directive.
    pub replacement: String,
    /// Optional rule identifier (`MIG-*`, `RISK-E-05`, `PLAN-DEPENDENCY-001`, etc.).
    pub rule_id: Option<String>,
    /// Earliest `SABI` epoch at which a grace-window item becomes blocking (`SUNSET`).
    pub deadline_epoch: Option<u32>,
    /// Concise rationale.
    pub rationale: String,
}

/// Deterministic upgrade prescription compiled from a [`ScanReport`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradePrescription {
    /// Hex-encoded 16-byte package identity.
    pub package_id_hex: String,
    /// Formatted `<major.minor.patch>` package version.
    pub package_version: String,
    /// Host `SABI` epoch targeted by this prescription.
    pub host_epoch: u32,
    /// Admission posture prior to applying remediation steps.
    pub posture: ExecutionPosture,
    /// Whether the package can execute transiently via `nlos-compat-shim` today.
    pub compat_shim_transient_allowed: bool,
    /// Minimum remaining grace-window epochs across all deprecated symbols/channels (`None`
    /// when blocked or when zero deprecations/shim routes exist).
    pub min_remaining_grace_epochs: Option<u32>,
    /// Ordered remediation actions.
    pub actions: Vec<RemediationAction>,
}

impl UpgradePrescription {
    /// Compiles a deterministic [`UpgradePrescription`] from `report`.
    #[must_use]
    pub fn from_scan_report(report: &ScanReport) -> Self {
        let mut draft_actions: Vec<RemediationAction> = Vec::new();
        collect_blocking_actions(report, &mut draft_actions);
        collect_shim_and_deprecation_actions(report, &mut draft_actions);

        draft_actions.sort_by(|left, right| {
            (
                !left.mandatory_before_admission,
                left.kind,
                left.target.as_str(),
                left.replacement.as_str(),
            )
                .cmp(&(
                    !right.mandatory_before_admission,
                    right.kind,
                    right.target.as_str(),
                    right.replacement.as_str(),
                ))
        });
        draft_actions.dedup_by(|left, right| {
            left.kind == right.kind
                && left.target == right.target
                && left.replacement == right.replacement
        });
        for (idx, action) in draft_actions.iter_mut().enumerate() {
            action.step_order = idx + 1;
        }

        let (posture, compat_shim_transient_allowed) = classify_posture(report);
        let min_remaining_grace_epochs = compute_min_grace_epochs(report);
        let (maj, min, pat) = report.target.version;

        Self {
            package_id_hex: hex_encode(report.target.package_id.as_bytes()),
            package_version: format!("{maj}.{min}.{pat}"),
            host_epoch: report.host_epoch,
            posture,
            compat_shim_transient_allowed,
            min_remaining_grace_epochs,
            actions: draft_actions,
        }
    }

    /// Deterministic human-readable text rendering (zero timestamps).
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "PRESCRIPTION package={} version={} host_epoch={} posture={} shim_transient={} grace_epochs={} actions={}",
            self.package_id_hex,
            self.package_version,
            self.host_epoch,
            self.posture.as_str(),
            self.compat_shim_transient_allowed,
            self.min_remaining_grace_epochs
                .map_or_else(|| "none".to_owned(), |v| v.to_string()),
            self.actions.len(),
        );
        for action in &self.actions {
            let urgency = if action.mandatory_before_admission {
                "REQUIRED"
            } else {
                "GRACE_WINDOW"
            };
            let _ = writeln!(
                out,
                "ACTION #{} [{}] kind={} target={} -> {} (rule={}, deadline_epoch={}): {}",
                action.step_order,
                urgency,
                action.kind.as_str(),
                action.target,
                action.replacement,
                action.rule_id.as_deref().unwrap_or("none"),
                action
                    .deadline_epoch
                    .map_or_else(|| "immediate".to_owned(), |ep| ep.to_string()),
                action.rationale,
            );
        }
        out
    }

    /// Canonical deterministic JSON rendering (zero timestamps).
    #[must_use]
    pub fn render_json(&self) -> String {
        let actions_json: Vec<String> = self
            .actions
            .iter()
            .map(|act| {
                format!(
                    "{{\"deadline_epoch\": {}, \"kind\": {}, \"mandatory_before_admission\": {}, \
                     \"rationale\": {}, \"replacement\": {}, \"rule_id\": {}, \"step_order\": {}, \
                     \"target\": {}}}",
                    json_opt_u32(act.deadline_epoch),
                    json_string(act.kind.as_str()),
                    act.mandatory_before_admission,
                    json_string(&act.rationale),
                    json_string(&act.replacement),
                    json_opt_string(act.rule_id.as_deref()),
                    act.step_order,
                    json_string(&act.target),
                )
            })
            .collect();

        format!(
            "{{\n  \"action_count\": {},\n  \"actions\": [{}],\n  \
             \"compat_shim_transient_allowed\": {},\n  \"host_epoch\": {},\n  \
             \"min_remaining_grace_epochs\": {},\n  \"package_id\": {},\n  \
             \"package_version\": {},\n  \"posture\": {}\n}}",
            self.actions.len(),
            actions_json.join(", "),
            self.compat_shim_transient_allowed,
            self.host_epoch,
            json_opt_u32(self.min_remaining_grace_epochs),
            json_string(&self.package_id_hex),
            json_string(&self.package_version),
            json_string(self.posture.as_str()),
        )
    }
}

fn classify_posture(report: &ScanReport) -> (ExecutionPosture, bool) {
    if !report.passed() {
        return (ExecutionPosture::SourceMigrationRequired, false);
    }
    if report.uses_compat_shim() || !report.deprecations.is_empty() {
        return (ExecutionPosture::TransientCompatShimEligible, true);
    }
    (ExecutionPosture::NativeCurrentReady, false)
}

fn compute_min_grace_epochs(report: &ScanReport) -> Option<u32> {
    if !report.passed() {
        return None;
    }
    let mut min_grace: Option<u32> = None;
    for dep in &report.deprecations {
        min_grace = Some(min_grace.map_or(dep.remaining_grace_epochs, |cur| {
            cur.min(dep.remaining_grace_epochs)
        }));
    }
    for route in &report.channel_routes {
        if let RouteClassification::OutOfCoreCompatShim { epoch_distance } = route.route {
            let rem = report
                .max_backward_epochs
                .saturating_add(1)
                .saturating_sub(epoch_distance);
            min_grace = Some(min_grace.map_or(rem, |cur| cur.min(rem)));
        }
    }
    min_grace
}

fn collect_blocking_actions(report: &ScanReport, actions: &mut Vec<RemediationAction>) {
    for finding in &report.findings {
        if finding.severity != FindingSeverity::BlockingError {
            continue;
        }
        let kind = match finding.code {
            "E_DISGUISED_IRREVERSIBLE_MIGRATION" | "E_WINDOW_SCHEMA_INCOMPATIBLE" => {
                RemediationKind::FixDisguisedIrreversible
            }
            "E_FLOATING_DEPENDENCY_SELECTOR" | "E_INVALID_DEPENDENCY_KIND" => {
                RemediationKind::PinDependencySelector
            }
            "E_SUNSET_EPOCH"
            | "E_UNSUPPORTED_SABI_EPOCH"
            | "E_DRIVER_ABI_EPOCH_OUT_OF_WINDOW"
            | "E_MISSING_DATA_MIGRATION_MODE"
            | "E_UNKNOWN_SABI_CHANNEL" => RemediationKind::UpdateManifestEpoch,
            "E_UNKNOWN_CRITICAL_EXTENSION" => RemediationKind::UpdateExtensionList,
            "E_COMPENSATABLE_MISSING_HANDLER" => RemediationKind::BindEffectCompensation,
            _ => RemediationKind::ReplaceContractSymbol,
        };
        let replacement = finding
            .suggested_replacement
            .clone()
            .unwrap_or_else(|| "manual source contract remediation required".to_owned());
        actions.push(RemediationAction {
            step_order: 0,
            kind,
            mandatory_before_admission: true,
            target: finding.subject.clone(),
            replacement,
            rule_id: finding.migration_rule_id.clone(),
            deadline_epoch: None,
            rationale: finding.message.clone(),
        });
    }
}

fn collect_shim_and_deprecation_actions(report: &ScanReport, actions: &mut Vec<RemediationAction>) {
    for route in &report.channel_routes {
        if let RouteClassification::OutOfCoreCompatShim { epoch_distance } = route.route {
            let sunset_epoch = route
                .client_major
                .saturating_add(report.max_backward_epochs)
                .saturating_add(1);
            actions.push(RemediationAction {
                step_order: 0,
                kind: RemediationKind::UpdateManifestEpoch,
                mandatory_before_admission: false,
                target: format!("channel::{}", route.channel),
                replacement: format!(
                    "target-sabi-epoch = {} (channel {} {}.0)",
                    report.host_epoch, route.channel, report.host_epoch
                ),
                rule_id: Some("COMPAT-VER-001".to_owned()),
                deadline_epoch: Some(sunset_epoch),
                rationale: format!(
                    "currently served via nlos-compat-shim at distance N-{epoch_distance}; \
                     reaches sunset at epoch {sunset_epoch}"
                ),
            });
        }
    }

    for dep in &report.deprecations {
        actions.push(RemediationAction {
            step_order: 0,
            kind: RemediationKind::ReplaceContractSymbol,
            mandatory_before_admission: false,
            target: format!("{}::{}", dep.channel, dep.symbol),
            replacement: dep.replacement_symbol.clone(),
            rule_id: dep.migration_rule_id.clone(),
            deadline_epoch: Some(dep.earliest_sunset_epoch),
            rationale: format!(
                "deprecated in epoch {} ({} grace epoch(s) remaining): {}",
                dep.deprecated_in_epoch, dep.remaining_grace_epochs, dep.reason
            ),
        });
    }
}
