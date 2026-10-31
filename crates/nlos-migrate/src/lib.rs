//! `nlos-migrate`: Stage E (`W78-E3`) declarative SABI/Driver/Package static
//! contract checker, deprecation upgrade prescription generator, and
//! `[PKG-UPDATE-001]` three-mode container migration & atomic rollback sandbox.

#![forbid(unsafe_code)]

use std::path::Path;

use nlos_schema::{
    DeprecationManifest, DeprecationManifestError, canonical_deprecation_manifest,
    parse_deprecation_manifest,
};

pub mod json;
pub mod manifest_audit;
pub mod prescription;
pub mod report;
pub mod sandbox;
pub mod scanner;

pub use json::{hex_decode_array, hex_encode, json_quote};
pub use manifest_audit::{
    AuditedPackageManifest, ComponentDeclaration, ComponentRole, DeclaredMigrationMode,
    EffectClass, EffectDeclaration, ExtensionCriticality, ManifestParseError, ReconcileDirective,
    SabiEpochTarget, SabiExtensionUse, SabiSymbolUse, TaskDeclaration, format_packed_version,
    load_package_manifest, parse_package_manifest,
};
pub use prescription::{ContractAdjudicationOutcome, adjudicate_manifest_contracts};
pub use report::{
    AuditFinding, AuditOptions, AuditVerdict, FindingSeverity, PackageAuditReport,
    PackageAuditReportInput, UpgradePrescription,
};
pub use sandbox::{
    CliDrillScenario, ContainerMutationClass, ContainerSnapshotReceipt, MigrationDrillDisposition,
    MigrationDrillError, MigrationDrillReceipt, MigrationExecutionPlan, MigrationMode,
    MigrationSandboxHarness, PreUpgradeBackupReceipt, Z4TrustedUiConsentReceipt,
    capture_container_snapshot, compute_container_tree_digest, count_residual_temp_files,
    create_pre_upgrade_backup, execute_ephemeral_cli_drill, render_drill_error,
    restore_container_from_snapshot, sign_z4_irreversible_consent, z4_irreversible_consent_digest,
};
pub use scanner::scan_package_payloads;

/// Audits `package_dir` against the embedded canonical SABI v1 deprecation
/// manifest (`schema/nlos/sabi/v1/deprecation-manifest.yaml`).
///
/// # Panics
///
/// Never panics in practice because the embedded canonical manifest is frozen
/// and verified by `nlos-schema` tests; if parsing ever failed, returns a
/// fail-closed `Blocked` report.
#[must_use]
pub fn audit_package_directory(package_dir: &Path, options: &AuditOptions) -> PackageAuditReport {
    match canonical_deprecation_manifest() {
        Ok(deprecation) => {
            audit_package_directory_with_manifest(package_dir, &deprecation, options)
        }
        Err(err) => PackageAuditReport::new(PackageAuditReportInput {
            package_dir: package_dir.display().to_string(),
            package_id_hex: None,
            version: None,
            target_sabi_epoch: None,
            host_epoch: options.host_epoch.unwrap_or(1),
            negotiated_route: None,
            migration_mode: None,
            component_count: 0,
            findings: vec![AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "COMPAT-DEPRECATE-001".to_owned(),
                location: "canonical-deprecation-manifest".to_owned(),
                message: format!("canonical deprecation manifest failed validation: {err}"),
                remediation: None,
            }],
            prescriptions: Vec::new(),
            warning_receipts: Vec::new(),
        }),
    }
}

/// Parses `deprecation_yaml` and audits `package_dir` against it.
///
/// # Errors
///
/// Returns [`DeprecationManifestError`] if `deprecation_yaml` is malformed or
/// violates `ADR-0027` deprecation state-machine invariants.
pub fn audit_package_directory_with_yaml(
    package_dir: &Path,
    deprecation_yaml: &str,
    options: &AuditOptions,
) -> Result<PackageAuditReport, DeprecationManifestError> {
    let deprecation = parse_deprecation_manifest(deprecation_yaml)?;
    Ok(audit_package_directory_with_manifest(
        package_dir,
        &deprecation,
        options,
    ))
}

/// Audits `package_dir` against a caller-supplied [`DeprecationManifest`].
#[must_use]
pub fn audit_package_directory_with_manifest(
    package_dir: &Path,
    deprecation: &DeprecationManifest,
    options: &AuditOptions,
) -> PackageAuditReport {
    let display_dir = package_dir.display().to_string();
    let effective_host_epoch = options.host_epoch.unwrap_or(deprecation.current_epoch);

    let manifest = match load_package_manifest(package_dir) {
        Ok(parsed) => parsed,
        Err(parse_err) => {
            return PackageAuditReport::new(PackageAuditReportInput {
                package_dir: display_dir,
                package_id_hex: None,
                version: None,
                target_sabi_epoch: None,
                host_epoch: effective_host_epoch,
                negotiated_route: None,
                migration_mode: None,
                component_count: 0,
                findings: vec![AuditFinding {
                    severity: FindingSeverity::Blocker,
                    rule_id: "PKG-MULTI-001".to_owned(),
                    location: "package.manifest".to_owned(),
                    message: parse_err.to_string(),
                    remediation: Some(
                        "Fix syntax or structural errors in package.manifest before re-running `nlos-migrate check`."
                            .to_owned(),
                    ),
                }],
                prescriptions: Vec::new(),
                warning_receipts: Vec::new(),
            });
        }
    };

    let mut findings = scan_package_payloads(package_dir, &manifest);
    let contract_outcome = adjudicate_manifest_contracts(&manifest, deprecation, options);
    findings.extend(contract_outcome.findings);

    PackageAuditReport::new(PackageAuditReportInput {
        package_dir: display_dir,
        package_id_hex: Some(hex_encode(manifest.package_id.as_bytes())),
        version: Some(format_packed_version(manifest.packed_version)),
        target_sabi_epoch: Some(manifest.target_sabi_epoch.to_string()),
        host_epoch: contract_outcome.host_epoch,
        negotiated_route: contract_outcome.negotiated_route,
        migration_mode: manifest.migration_mode.map(|mode| mode.as_str().to_owned()),
        component_count: manifest.components.len(),
        findings,
        prescriptions: contract_outcome.prescriptions,
        warning_receipts: contract_outcome.warning_receipts,
    })
}
