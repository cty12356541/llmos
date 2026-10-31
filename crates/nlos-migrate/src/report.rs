//! Structured static-audit findings, upgrade prescriptions, and deterministic
//! plain-text / RFC 8259 JSON report renderers (`nlos-migrate check`).

use std::fmt::Write as _;

use nlos_service_directory::DeprecationWarningReceipt;

use crate::json::{json_opt_str, json_opt_u32, json_quote};

/// Severity of one static audit finding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FindingSeverity {
    /// Non-blocking notice (such as a `DEPRECATED` SABI contract inside its
    /// rolling grace window).
    Warning,
    /// Blocking violation that causes `nlos-migrate check` to fail-closed with
    /// exit code `2`.
    Blocker,
}

impl FindingSeverity {
    /// Canonical uppercase token for text and JSON reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "WARNING",
            Self::Blocker => "BLOCKER",
        }
    }
}

/// Top-level compliance verdict for one audited package directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditVerdict {
    /// Zero blockers and zero warnings.
    Pass,
    /// Zero blockers and at least one non-blocking warning.
    PassWithWarnings,
    /// One or more blocking violations.
    Blocked,
}

impl AuditVerdict {
    /// Canonical uppercase token for text and JSON reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::PassWithWarnings => "PASS_WITH_WARNINGS",
            Self::Blocked => "BLOCKED",
        }
    }

    /// Whether the audited package passed admission without blockers.
    #[must_use]
    pub const fn is_compliant(self) -> bool {
        matches!(self, Self::Pass | Self::PassWithWarnings)
    }

    /// Process exit code mandated by `nlos-migrate check` (`0` compliant, `2` blocked).
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        if self.is_compliant() { 0 } else { 2 }
    }
}

/// One typed static-audit finding tied to a normative architecture rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditFinding {
    pub severity: FindingSeverity,
    pub rule_id: String,
    pub location: String,
    pub message: String,
    pub remediation: Option<String>,
}

/// Actionable contract migration prescription emitted for `DEPRECATED` or
/// `SUNSET`/`RESERVED` SABI references (`[COMPAT-DEPRECATE-001]`, `ADR-0027`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradePrescription {
    pub channel: String,
    pub symbol: String,
    pub field_number: Option<u32>,
    pub lifecycle_state: String,
    pub host_epoch: u32,
    pub deprecated_in_epoch: Option<u32>,
    pub earliest_sunset_epoch: Option<u32>,
    pub remaining_grace_epochs: Option<u32>,
    pub replacement_symbol: Option<String>,
    pub migration_rule_id: Option<String>,
    pub patch_summary: String,
}

impl UpgradePrescription {
    /// Converts a `DEPRECATED` prescription into a [`DeprecationWarningReceipt`]
    /// matching `nlos-service-directory` runtime telemetry.
    #[must_use]
    pub fn to_warning_receipt(
        &self,
        caller_major: u32,
        caller_minor: u32,
        service: &str,
    ) -> Option<DeprecationWarningReceipt> {
        if self.lifecycle_state != "DEPRECATED" {
            return None;
        }
        Some(DeprecationWarningReceipt {
            channel: self.channel.clone(),
            symbol: self.symbol.clone(),
            field_number: self.field_number,
            deprecated_in_epoch: self.deprecated_in_epoch?,
            earliest_sunset_epoch: self.earliest_sunset_epoch?,
            replacement: self.replacement_symbol.clone().unwrap_or_default(),
            caller_major,
            caller_minor,
            service: service.to_owned(),
        })
    }
}

/// Configuration knobs for static package auditing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuditOptions {
    /// Overrides the host SABI major epoch `N` (`--host-epoch`). Defaults to
    /// `DeprecationManifest::current_epoch` when `None`.
    pub host_epoch: Option<u32>,
    /// Overrides the rolling backward-compatibility window in major epochs.
    /// Defaults to `DeprecationManifest::min_compatibility_window_epochs` when `None`.
    pub max_backward_epochs: Option<u32>,
    /// Receiver-known `[COMPAT-VER-002]` critical extension identifiers.
    pub known_critical_extensions: Vec<u32>,
}

/// Complete deterministic report emitted by `nlos-migrate check`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageAuditReport {
    pub package_dir: String,
    pub package_id_hex: Option<String>,
    pub version: Option<String>,
    pub target_sabi_epoch: Option<String>,
    pub host_epoch: u32,
    pub negotiated_route: Option<String>,
    pub migration_mode: Option<String>,
    pub component_count: usize,
    pub verdict: AuditVerdict,
    pub blocker_count: usize,
    pub warning_count: usize,
    pub findings: Vec<AuditFinding>,
    pub prescriptions: Vec<UpgradePrescription>,
    pub warning_receipts: Vec<DeprecationWarningReceipt>,
}

/// Builder inputs for [`PackageAuditReport::new`].
#[derive(Clone, Debug)]
pub struct PackageAuditReportInput {
    pub package_dir: String,
    pub package_id_hex: Option<String>,
    pub version: Option<String>,
    pub target_sabi_epoch: Option<String>,
    pub host_epoch: u32,
    pub negotiated_route: Option<String>,
    pub migration_mode: Option<String>,
    pub component_count: usize,
    pub findings: Vec<AuditFinding>,
    pub prescriptions: Vec<UpgradePrescription>,
    pub warning_receipts: Vec<DeprecationWarningReceipt>,
}

impl PackageAuditReport {
    /// Builds a [`PackageAuditReport`] and computes summary counts and verdict.
    #[must_use]
    pub fn new(input: PackageAuditReportInput) -> Self {
        let blocker_count = input
            .findings
            .iter()
            .filter(|finding| finding.severity == FindingSeverity::Blocker)
            .count();
        let warning_count = input
            .findings
            .iter()
            .filter(|finding| finding.severity == FindingSeverity::Warning)
            .count();
        let verdict = if blocker_count > 0 {
            AuditVerdict::Blocked
        } else if warning_count > 0 {
            AuditVerdict::PassWithWarnings
        } else {
            AuditVerdict::Pass
        };
        Self {
            package_dir: input.package_dir,
            package_id_hex: input.package_id_hex,
            version: input.version,
            target_sabi_epoch: input.target_sabi_epoch,
            host_epoch: input.host_epoch,
            negotiated_route: input.negotiated_route,
            migration_mode: input.migration_mode,
            component_count: input.component_count,
            verdict,
            blocker_count,
            warning_count,
            findings: input.findings,
            prescriptions: input.prescriptions,
            warning_receipts: input.warning_receipts,
        }
    }

    /// Renders a deterministic, ASCII/UTF-8 plain-text report with zero Emoji.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "NLOS-MIGRATE STATIC AUDIT REPORT");
        let _ = writeln!(out, "verdict: {}", self.verdict.as_str());
        let _ = writeln!(out, "package_dir: {}", self.package_dir);
        let _ = writeln!(
            out,
            "package_id: {}",
            self.package_id_hex.as_deref().unwrap_or("<unparsed>")
        );
        let _ = writeln!(
            out,
            "version: {}",
            self.version.as_deref().unwrap_or("<unparsed>")
        );
        let _ = writeln!(
            out,
            "target_sabi_epoch: {}",
            self.target_sabi_epoch.as_deref().unwrap_or("<unparsed>")
        );
        let _ = writeln!(out, "host_epoch: {}", self.host_epoch);
        let _ = writeln!(
            out,
            "negotiated_route: {}",
            self.negotiated_route.as_deref().unwrap_or("<none>")
        );
        let _ = writeln!(
            out,
            "migration_mode: {}",
            self.migration_mode.as_deref().unwrap_or("<undeclared>")
        );
        let _ = writeln!(out, "components: {}", self.component_count);
        let _ = writeln!(
            out,
            "summary: {} blocker(s), {} warning(s), {} prescription(s)",
            self.blocker_count,
            self.warning_count,
            self.prescriptions.len()
        );

        for finding in &self.findings {
            let _ = writeln!(
                out,
                "[{}] [{}] {}: {}",
                finding.severity.as_str(),
                finding.rule_id,
                finding.location,
                finding.message
            );
            if let Some(remediation) = &finding.remediation {
                let _ = writeln!(out, "  remediation: {remediation}");
            }
        }

        for prescription in &self.prescriptions {
            let _ = writeln!(
                out,
                "[PRESCRIPTION] channel={} symbol={} field={} state={} rule={} replacement={} remaining_grace_epochs={}",
                prescription.channel,
                prescription.symbol,
                prescription
                    .field_number
                    .map_or_else(|| "none".to_owned(), |num| num.to_string()),
                prescription.lifecycle_state,
                prescription.migration_rule_id.as_deref().unwrap_or("none"),
                prescription.replacement_symbol.as_deref().unwrap_or("none"),
                prescription
                    .remaining_grace_epochs
                    .map_or_else(|| "0".to_owned(), |epochs| epochs.to_string()),
            );
            let _ = writeln!(out, "  patch: {}", prescription.patch_summary);
        }
        out
    }

    /// Renders a deterministic RFC 8259 JSON report with zero external
    /// serialization dependencies.
    #[must_use]
    pub fn render_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        let _ = writeln!(out, "  \"schema\": \"nlos.migrate.check.v1\",");
        let _ = writeln!(out, "  \"verdict\": {},", json_quote(self.verdict.as_str()));
        let _ = writeln!(out, "  \"compliant\": {},", self.verdict.is_compliant());
        let _ = writeln!(out, "  \"exit_code\": {},", self.verdict.exit_code());
        let _ = writeln!(out, "  \"package_dir\": {},", json_quote(&self.package_dir));
        let _ = writeln!(
            out,
            "  \"package_id\": {},",
            json_opt_str(self.package_id_hex.as_deref())
        );
        let _ = writeln!(
            out,
            "  \"version\": {},",
            json_opt_str(self.version.as_deref())
        );
        let _ = writeln!(
            out,
            "  \"target_sabi_epoch\": {},",
            json_opt_str(self.target_sabi_epoch.as_deref())
        );
        let _ = writeln!(out, "  \"host_epoch\": {},", self.host_epoch);
        let _ = writeln!(
            out,
            "  \"negotiated_route\": {},",
            json_opt_str(self.negotiated_route.as_deref())
        );
        let _ = writeln!(
            out,
            "  \"migration_mode\": {},",
            json_opt_str(self.migration_mode.as_deref())
        );
        let _ = writeln!(out, "  \"component_count\": {},", self.component_count);
        let _ = writeln!(out, "  \"blocker_count\": {},", self.blocker_count);
        let _ = writeln!(out, "  \"warning_count\": {},", self.warning_count);

        out.push_str("  \"findings\": [");
        if !self.findings.is_empty() {
            out.push('\n');
            for (idx, finding) in self.findings.iter().enumerate() {
                let comma = if idx + 1 == self.findings.len() {
                    ""
                } else {
                    ","
                };
                let _ = writeln!(
                    out,
                    "    {{\"severity\": {}, \"rule_id\": {}, \"location\": {}, \"message\": {}, \"remediation\": {}}}{comma}",
                    json_quote(finding.severity.as_str()),
                    json_quote(&finding.rule_id),
                    json_quote(&finding.location),
                    json_quote(&finding.message),
                    json_opt_str(finding.remediation.as_deref()),
                );
            }
            out.push_str("  ");
        }
        out.push_str("],\n");

        out.push_str("  \"prescriptions\": [");
        if !self.prescriptions.is_empty() {
            out.push('\n');
            for (idx, item) in self.prescriptions.iter().enumerate() {
                let comma = if idx + 1 == self.prescriptions.len() {
                    ""
                } else {
                    ","
                };
                let _ = writeln!(
                    out,
                    "    {{\"channel\": {}, \"symbol\": {}, \"field_number\": {}, \"lifecycle_state\": {}, \"host_epoch\": {}, \"deprecated_in_epoch\": {}, \"earliest_sunset_epoch\": {}, \"remaining_grace_epochs\": {}, \"replacement_symbol\": {}, \"migration_rule_id\": {}, \"patch_summary\": {}}}{comma}",
                    json_quote(&item.channel),
                    json_quote(&item.symbol),
                    json_opt_u32(item.field_number),
                    json_quote(&item.lifecycle_state),
                    item.host_epoch,
                    json_opt_u32(item.deprecated_in_epoch),
                    json_opt_u32(item.earliest_sunset_epoch),
                    json_opt_u32(item.remaining_grace_epochs),
                    json_opt_str(item.replacement_symbol.as_deref()),
                    json_opt_str(item.migration_rule_id.as_deref()),
                    json_quote(&item.patch_summary),
                );
            }
            out.push_str("  ");
        }
        out.push_str("]\n");
        out.push_str("}\n");
        out
    }
}
