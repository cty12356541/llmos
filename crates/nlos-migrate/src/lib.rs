//! Declarative `SABI` contract static checker, `UpgradePrescription` compiler,
//! and `[PKG-UPDATE-001]` three-mode data migration rollback drill toolkit
//! (`Stage E W78 / E3`).
//!
//! # Authority
//!
//! - `06-架构设计总纲-v0.5.md` section 9 (`COMPAT-VER-001`, `COMPAT-VER-002`,
//!   `COMPAT-DEPRECATE-001`) and section 23 (`PKG-UPDATE-001`)
//! - `ADR-0027` (`N / N-1 / N-2` rolling compatibility matrix, deprecation
//!   lifecycle, and `RISK-E-05` irreversible migration hard gate)

#![forbid(unsafe_code)]

pub mod drill;
pub mod drill_fixtures;
pub mod json;
pub mod manifest;
pub mod prescription;
pub mod scanner;

pub use drill::{
    DrillError, DrillFaultInjection, DrillOutcomeKind, DrillReport, DrillRequest,
    EphemeralIrreversibleProvisioning, default_sql_steps_for_mode,
    issue_irreversible_governance_bundle, run_migration_drill, validate_migration_steps_for_mode,
};
pub use drill_fixtures::{
    DrillPackagePair, PreUpgradeBackupReceipt, UserDataSnapshotHandle,
    Z4IrreversibleConsentReceipt, compute_userdata_tree_sha256, create_userdata_snapshot,
    derive_z4_consent_token, restore_userdata_snapshot,
};
pub use manifest::{
    DataMigrationMode, DeclaredChannelUsage, DeclaredDependencySelector, DeclaredEffect,
    DeclaredSymbolRef, EffectClassDeclaration, InspectedPackageTarget, ManifestLoadError,
    TargetSourceKind, inspect_package_target,
};
pub use prescription::{ExecutionPosture, RemediationAction, RemediationKind, UpgradePrescription};
pub use scanner::{
    ChannelRouteSummary, DeprecationDiagnostic, FindingCategory, FindingSeverity, MigrationFinding,
    MigrationStepHazard, RouteClassification, ScanError, ScanOptions, ScanReport,
    classify_migration_step_hazard, scan_inspected_target, scan_path,
};
