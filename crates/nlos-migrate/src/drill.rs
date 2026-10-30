//! `[PKG-UPDATE-001]` three-mode data container migration and fault-rollback
//! sandbox drill runner (`W78-E3`).
//!
//! Executes real upgrades across `IdentityAuthority`, `ArtifactStore`,
//! `ApplicationAuthority`, and a `SQLite` + file `UserData` container for:
//! - [`DataMigrationMode::BackwardCompatibleWindow`],
//! - [`DataMigrationMode::SnapshotAssistedRollback`] (including mid-step crash
//!   recovery and post-migration `DRV-HEALTH-001` failure rollback),
//! - [`DataMigrationMode::Irreversible`] (`RISK-E-05` hard gate: physical ban on
//!   disguised destructive `SQL`, plus mandatory `SHA-256` pre-backup and `Z4`
//!   `Trusted UI` consent receipts).

use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_application::{
    ActivateMigrationDecision, ActivatePackageMigrationRequest, ApplicationAuthority,
    ApplicationStatus, CompatibilityWindow, InstallApplicationRequest, MigrateApplicationRequest,
    MigrateDecision, MigrationHealthContext, MigrationHealthProbe, MigrationHealthState,
    MigrationState, RecordMigrationStepDecision, RecordMigrationStepRequest,
    RollbackMigrationDecision, RollbackPackageMigrationRequest,
};
use nlos_artifact::{ArtifactStore, ContentDigest, PackageVerificationReceipt};
use nlos_identity::IdentityAuthority;
use nlos_types::{ApplicationId, Generation, IdempotencyKey};

use crate::drill_fixtures::{
    DrillPackagePair, PreUpgradeBackupReceipt, UserDataSnapshotHandle,
    Z4IrreversibleConsentReceipt, compute_userdata_tree_sha256, copy_dir_recursive,
    create_userdata_snapshot, derive_key_16, derive_z4_consent_token,
    materialize_and_verify_package, open_userdata_sqlite, restore_userdata_snapshot,
    seed_baseline_userdata,
};
use crate::json::{hex_encode, json_opt_string, json_string};
use crate::manifest::DataMigrationMode;
use crate::scanner::{MigrationStepHazard, classify_migration_step_hazard};

const INSTALL_V1_KEY_DOMAIN: &[u8] = b"llmos/migrate/drill-install-v1/v1";
const MIGRATE_DRILL_KEY_DOMAIN: &[u8] = b"llmos/migrate/drill-migrate-v2/v1";

static TEMP_SANDBOX_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Fault-injection profile applied during a sandbox migration drill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrillFaultInjection {
    /// Positive path: all steps complete and `DRV-HEALTH-001` reports healthy.
    None,
    /// Simulates an abrupt process crash immediately after step `k` (`1 <= k < N`),
    /// then reopens durable authorities and converges to `Generation(2)`.
    CrashAfterStep(u64),
    /// Simulates a `DRV-HEALTH-001` post-migration health check failure, triggering
    /// automatic `rollback_package_migration` and `UserData` snapshot restoration.
    PostMigrationHealthFailure,
}

/// Governance provisioning options for ephemeral CLI / test sandboxes in
/// `DataMigrationMode::Irreversible`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EphemeralIrreversibleProvisioning {
    /// Automatically materialize a verified external `UserData` backup receipt.
    pub create_verified_backup: bool,
    /// Automatically issue a bound `Z4` `Trusted UI` consent receipt.
    pub issue_z4_consent: bool,
    /// Optional explicit override token to test mismatched `Z4` consent rejection.
    pub override_z4_token: Option<[u8; 32]>,
}

/// Input specification for [`run_migration_drill`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrillRequest {
    /// Declared `[PKG-UPDATE-001]` data container migration mode.
    pub mode: DataMigrationMode,
    /// Baseline `v1` and target `v2` signed packages.
    pub packages: DrillPackagePair,
    /// Ordered `SQL` migration steps (if empty, mode-appropriate canonical steps are used).
    pub sql_steps: Vec<String>,
    /// Optional fault injection.
    pub fault_injection: DrillFaultInjection,
    /// Optional persistent directory for inspecting post-drill `SQLite` state; if `None`,
    /// an isolated temporary directory is created and removed automatically.
    pub state_root: Option<PathBuf>,
    /// Caller-supplied external pre-upgrade backup receipt (`Irreversible` mode).
    pub pre_backup_receipt: Option<PreUpgradeBackupReceipt>,
    /// Caller-supplied `Z4` `Trusted UI` consent receipt (`Irreversible` mode).
    pub z4_consent_receipt: Option<Z4IrreversibleConsentReceipt>,
    /// Ephemeral sandbox governance helper flags (`Irreversible` mode).
    pub ephemeral_irreversible: EphemeralIrreversibleProvisioning,
}

impl DrillRequest {
    /// Constructs a default positive-path request for `mode` using canonical fixtures.
    #[must_use]
    pub fn for_mode(mode: DataMigrationMode) -> Self {
        Self {
            mode,
            packages: DrillPackagePair::canonical_fixture(),
            sql_steps: default_sql_steps_for_mode(mode),
            fault_injection: DrillFaultInjection::None,
            state_root: None,
            pre_backup_receipt: None,
            z4_consent_receipt: None,
            ephemeral_irreversible: EphemeralIrreversibleProvisioning::default(),
        }
    }
}

/// Terminal outcome classification of a completed migration drill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrillOutcomeKind {
    /// Target `v2` activated cleanly at `Generation(2)`.
    ActivatedGeneration2,
    /// Simulated mid-step crash recovered via durable step replay and activated `Generation(2)`.
    RecoveredCrashAndActivatedGeneration2 {
        /// Step index after which the first authority instance dropped.
        crashed_after_step: u64,
        /// Number of already-committed steps replayed idempotently on restart.
        replayed_steps: u64,
    },
    /// Health check failed (`DRV-HEALTH-001`); `Application` remained on `Generation(1)` and
    /// `UserData` was atomically restored to its exact pre-upgrade `SHA-256`.
    RolledBackOnHealthFailure,
}

impl DrillOutcomeKind {
    /// Canonical snake-case identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActivatedGeneration2 => "activated_generation_2",
            Self::RecoveredCrashAndActivatedGeneration2 { .. } => {
                "recovered_crash_and_activated_generation_2"
            }
            Self::RolledBackOnHealthFailure => "rolled_back_on_health_failure",
        }
    }
}

/// Complete deterministic report emitted by [`run_migration_drill`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrillReport {
    /// Executed `[PKG-UPDATE-001]` migration mode.
    pub mode: DataMigrationMode,
    /// Terminal drill outcome.
    pub outcome: DrillOutcomeKind,
    /// Hex-encoded `PackageId`.
    pub package_id_hex: String,
    /// Hex-encoded authority-derived `ApplicationId`.
    pub application_id_hex: String,
    /// Baseline generation (`1`).
    pub initial_generation: u64,
    /// Active generation after drill completion (`2` on activation, `1` on rollback).
    pub final_generation: u64,
    /// Hex-encoded `v1` manifest digest.
    pub v1_manifest_digest_hex: String,
    /// Hex-encoded `v2` manifest digest.
    pub v2_manifest_digest_hex: String,
    /// Hex-encoded active manifest digest after drill completion.
    pub active_manifest_digest_hex: String,
    /// Pre-upgrade `UserData` tree `SHA-256` hex.
    pub pre_upgrade_userdata_sha256_hex: String,
    /// Mid-upgrade mutated `UserData` tree `SHA-256` hex prior to activation/rollback.
    pub mutated_userdata_sha256_hex: Option<String>,
    /// Final `UserData` tree `SHA-256` hex after drill completion.
    pub final_userdata_sha256_hex: String,
    /// `true` iff `final_userdata_sha256_hex == pre_upgrade_userdata_sha256_hex`.
    pub userdata_matches_pre_upgrade: bool,
    /// Declared migration step count.
    pub steps_declared: u64,
    /// Completed migration step count.
    pub steps_completed: u64,
}

impl DrillReport {
    /// Deterministic text rendering (zero wall-clock timestamps).
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "DRILL outcome={} mode={} package={} application={} gen={}->{} steps={}/{} \
             pre_sha256={} final_sha256={} userdata_identical_to_pre={}",
            self.outcome.as_str(),
            self.mode.as_str(),
            self.package_id_hex,
            self.application_id_hex,
            self.initial_generation,
            self.final_generation,
            self.steps_completed,
            self.steps_declared,
            self.pre_upgrade_userdata_sha256_hex,
            self.final_userdata_sha256_hex,
            self.userdata_matches_pre_upgrade,
        );
        out
    }

    /// Canonical deterministic JSON rendering (zero wall-clock timestamps).
    #[must_use]
    pub fn render_json(&self) -> String {
        let (crash_step, replayed_steps) = match self.outcome {
            DrillOutcomeKind::RecoveredCrashAndActivatedGeneration2 {
                crashed_after_step,
                replayed_steps,
            } => (crashed_after_step.to_string(), replayed_steps.to_string()),
            _ => ("null".to_owned(), "null".to_owned()),
        };
        format!(
            "{{\n  \"active_manifest_digest\": {},\n  \"application_id\": {},\n  \
             \"crashed_after_step\": {},\n  \"final_generation\": {},\n  \
             \"final_userdata_sha256\": {},\n  \"initial_generation\": {},\n  \
             \"mode\": {},\n  \"mutated_userdata_sha256\": {},\n  \"outcome\": {},\n  \
             \"package_id\": {},\n  \"pre_upgrade_userdata_sha256\": {},\n  \
             \"replayed_steps\": {},\n  \"steps_completed\": {},\n  \
             \"steps_declared\": {},\n  \"userdata_matches_pre_upgrade\": {},\n  \
             \"v1_manifest_digest\": {},\n  \"v2_manifest_digest\": {}\n}}",
            json_string(&self.active_manifest_digest_hex),
            json_string(&self.application_id_hex),
            crash_step,
            self.final_generation,
            json_string(&self.final_userdata_sha256_hex),
            self.initial_generation,
            json_string(self.mode.as_str()),
            json_opt_string(self.mutated_userdata_sha256_hex.as_deref()),
            json_string(self.outcome.as_str()),
            json_string(&self.package_id_hex),
            json_string(&self.pre_upgrade_userdata_sha256_hex),
            replayed_steps,
            self.steps_completed,
            self.steps_declared,
            self.userdata_matches_pre_upgrade,
            json_string(&self.v1_manifest_digest_hex),
            json_string(&self.v2_manifest_digest_hex),
        )
    }
}

/// Typed failure returned by [`run_migration_drill`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DrillError {
    /// Destructive `SQL`/plan change disguised under `BackwardCompatibleWindow` or
    /// `SnapshotAssistedRollback` (`RISK-E-05`, CLI exit code `4`).
    DisguisedIrreversibleMigration {
        /// 1-based step index.
        step_index: usize,
        /// Offending `SQL`/plan statement.
        statement: String,
        /// Structural hazard explanation.
        reason: &'static str,
    },
    /// `Irreversible` migration lacked a valid `SHA-256`-verified pre-upgrade backup
    /// (`RISK-E-05`, CLI exit code `4`).
    IrreversibleMissingPreBackup { detail: String },
    /// `Irreversible` migration lacked a bound `Z4` `Trusted UI` consent receipt
    /// (`RISK-E-05`, CLI exit code `4`).
    IrreversibleInvalidZ4Consent { detail: String },
    /// `BackwardCompatibleWindow` schema or dual-reader/writer check failed (CLI exit code `5`).
    WindowIncompatibility { detail: String },
    /// Health check, snapshot rollback restoration, or authority invariant failed (CLI exit `5`).
    RollbackOrHealthFailure { detail: String },
    /// Sandbox setup or invalid drill configuration error (CLI exit code `5`).
    SandboxSetup { detail: String },
}

impl DrillError {
    /// Returns `true` when this error belongs to the `RISK-E-05` irreversible governance gate
    /// (CLI exit code `4`).
    #[must_use]
    pub const fn is_irreversible_governance_block(&self) -> bool {
        matches!(
            self,
            Self::DisguisedIrreversibleMigration { .. }
                | Self::IrreversibleMissingPreBackup { .. }
                | Self::IrreversibleInvalidZ4Consent { .. }
        )
    }
}

impl fmt::Display for DrillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisguisedIrreversibleMigration {
                step_index,
                statement,
                reason,
            } => write!(
                formatter,
                "DisguisedIrreversibleMigration at step #{step_index} ({statement:?}): {reason}"
            ),
            Self::IrreversibleMissingPreBackup { detail } => {
                write!(
                    formatter,
                    "irreversible migration missing pre-backup: {detail}"
                )
            }
            Self::IrreversibleInvalidZ4Consent { detail } => {
                write!(
                    formatter,
                    "irreversible migration missing/invalid Z4 consent: {detail}"
                )
            }
            Self::WindowIncompatibility { detail } => {
                write!(formatter, "backward-compatible window violation: {detail}")
            }
            Self::RollbackOrHealthFailure { detail } => {
                write!(
                    formatter,
                    "migration health/rollback drill failure: {detail}"
                )
            }
            Self::SandboxSetup { detail } => {
                write!(formatter, "drill sandbox setup error: {detail}")
            }
        }
    }
}

impl Error for DrillError {}

struct EphemeralDirGuard {
    path: PathBuf,
    cleanup_on_drop: bool,
}

impl EphemeralDirGuard {
    fn acquire(explicit: Option<&Path>) -> Result<Self, DrillError> {
        if let Some(root) = explicit {
            fs::create_dir_all(root).map_err(|err| DrillError::SandboxSetup {
                detail: format!("create state_root {}: {err}", root.display()),
            })?;
            return Ok(Self {
                path: root.to_path_buf(),
                cleanup_on_drop: false,
            });
        }
        let seq = TEMP_SANDBOX_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("nlos-migrate-drill-{pid}-{seq}"));
        if path.exists() {
            let _ = fs::remove_dir_all(&path);
        }
        fs::create_dir_all(&path).map_err(|err| DrillError::SandboxSetup {
            detail: format!("create temp sandbox {}: {err}", path.display()),
        })?;
        Ok(Self {
            path,
            cleanup_on_drop: true,
        })
    }
}

impl Drop for EphemeralDirGuard {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct ConfigurableHealthProbe {
    healthy: bool,
}

impl MigrationHealthProbe for ConfigurableHealthProbe {
    fn target_revision_healthy(&self, context: &MigrationHealthContext<'_>) -> bool {
        self.healthy && context.completed_step_count == context.declared_step_count
    }
}

struct PreparedSandbox {
    application_id: ApplicationId,
    v1_receipt: PackageVerificationReceipt,
    v2_receipt: PackageVerificationReceipt,
    userdata_dir: PathBuf,
    pre_upgrade_sha256: [u8; 32],
    drill_key: IdempotencyKey,
}

/// Default realistic `SQL` migration steps used when `DrillRequest::sql_steps` is empty.
#[must_use]
pub fn default_sql_steps_for_mode(mode: DataMigrationMode) -> Vec<String> {
    match mode {
        DataMigrationMode::BackwardCompatibleWindow => vec![
            "ALTER TABLE user_documents ADD COLUMN tags TEXT NOT NULL DEFAULT '';".to_owned(),
            "CREATE INDEX IF NOT EXISTS idx_user_documents_title ON user_documents(title);"
                .to_owned(),
        ],
        DataMigrationMode::SnapshotAssistedRollback => vec![
            "CREATE TABLE IF NOT EXISTS user_document_digest_v2 (\
             doc_id TEXT PRIMARY KEY, body_len INTEGER NOT NULL);"
                .to_owned(),
            "INSERT OR REPLACE INTO user_document_digest_v2 (doc_id, body_len) \
             SELECT doc_id, length(body) FROM user_documents;"
                .to_owned(),
        ],
        DataMigrationMode::Irreversible => vec![
            "ALTER TABLE user_documents ADD COLUMN summary TEXT NOT NULL DEFAULT '';".to_owned(),
            "ALTER TABLE user_documents DROP COLUMN body;".to_owned(),
        ],
    }
}

/// Inspects `sql_steps` against `mode`, rejecting disguised destructive operations
/// (`DisguisedIrreversibleMigration`) and window-breaking DDL before any mutation.
///
/// # Errors
///
/// Fails closed with [`DrillError::DisguisedIrreversibleMigration`] or
/// [`DrillError::WindowIncompatibility`].
pub fn validate_migration_steps_for_mode(
    mode: DataMigrationMode,
    sql_steps: &[String],
) -> Result<(), DrillError> {
    if sql_steps.is_empty() {
        return Err(DrillError::SandboxSetup {
            detail: "migration drill requires at least 1 SQL/migration step".to_owned(),
        });
    }
    for (idx, stmt) in sql_steps.iter().enumerate() {
        if let Some(hazard) = classify_migration_step_hazard(stmt) {
            match hazard {
                MigrationStepHazard::DestructiveIrreversible { reason } => {
                    if mode != DataMigrationMode::Irreversible {
                        return Err(DrillError::DisguisedIrreversibleMigration {
                            step_index: idx + 1,
                            statement: stmt.clone(),
                            reason,
                        });
                    }
                }
                MigrationStepHazard::BreaksBackwardWindow { reason } => {
                    if mode == DataMigrationMode::BackwardCompatibleWindow {
                        return Err(DrillError::WindowIncompatibility {
                            detail: format!("step #{} ({stmt:?}): {reason}", idx + 1),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// Executes a full `[PKG-UPDATE-001]` migration and rollback/recovery drill inside an
/// isolated `SQLite` state root.
///
/// # Errors
///
/// Fails closed with [`DrillError`] if disguised irreversible steps are detected,
/// `Irreversible` governance receipts are missing/invalid, window invariants fail,
/// or authority/snapshot invariants fail.
pub fn run_migration_drill(request: &DrillRequest) -> Result<DrillReport, DrillError> {
    let steps = if request.sql_steps.is_empty() {
        default_sql_steps_for_mode(request.mode)
    } else {
        request.sql_steps.clone()
    };
    validate_migration_steps_for_mode(request.mode, &steps)?;
    validate_fault_profile(request.fault_injection, steps.len() as u64)?;

    let sandbox_guard = EphemeralDirGuard::acquire(request.state_root.as_deref())?;
    let root = &sandbox_guard.path;
    let prepared = bootstrap_baseline_v1(root, &request.packages)?;

    if request.mode == DataMigrationMode::Irreversible {
        enforce_irreversible_governance_gate(root, request, &prepared)?;
    }

    let snapshot = if request.mode == DataMigrationMode::SnapshotAssistedRollback {
        let snap_parent = root
            .join("snapshots")
            .join(hex_encode(prepared.drill_key.as_bytes()));
        Some(
            create_userdata_snapshot(&prepared.userdata_dir, &snap_parent)
                .map_err(|detail| DrillError::RollbackOrHealthFailure { detail })?,
        )
    } else {
        None
    };

    execute_staged_drill(root, request, &steps, &prepared, snapshot.as_ref())
}

fn validate_fault_profile(fault: DrillFaultInjection, step_count: u64) -> Result<(), DrillError> {
    if let DrillFaultInjection::CrashAfterStep(crash_at) = fault
        && (crash_at == 0 || crash_at >= step_count)
    {
        return Err(DrillError::SandboxSetup {
            detail: format!(
                "CrashAfterStep({crash_at}) must satisfy 1 <= k < declared_step_count({step_count})"
            ),
        });
    }
    Ok(())
}

fn bootstrap_baseline_v1(
    root: &Path,
    packages: &DrillPackagePair,
) -> Result<PreparedSandbox, DrillError> {
    let identity =
        IdentityAuthority::open(root.join("identity")).map_err(|err| DrillError::SandboxSetup {
            detail: format!("open IdentityAuthority: {err}"),
        })?;
    let artifacts =
        ArtifactStore::open(root.join("artifacts")).map_err(|err| DrillError::SandboxSetup {
            detail: format!("open ArtifactStore: {err}"),
        })?;
    let applications = ApplicationAuthority::open(root.join("applications")).map_err(|err| {
        DrillError::SandboxSetup {
            detail: format!("open ApplicationAuthority: {err}"),
        }
    })?;

    let v1_receipt = materialize_and_verify_package(&identity, &artifacts, &packages.v1, 2_000)
        .map_err(|detail| DrillError::SandboxSetup { detail })?;
    let v2_receipt = materialize_and_verify_package(&identity, &artifacts, &packages.v2, 3_000)
        .map_err(|detail| DrillError::SandboxSetup { detail })?;

    applications
        .install_application(
            &artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: v1_receipt.receipt_id,
                idempotency_key: derive_key_16(
                    INSTALL_V1_KEY_DOMAIN,
                    v1_receipt.receipt_id.as_bytes(),
                ),
                installed_at_ms: 2_500,
            },
        )
        .map_err(|err| DrillError::SandboxSetup {
            detail: format!("install v1 baseline: {err}"),
        })?;

    let app_view = applications
        .inspect_application(packages.v1.package_id)
        .map_err(|err| DrillError::SandboxSetup {
            detail: format!("inspect v1 baseline: {err}"),
        })?
        .ok_or_else(|| DrillError::SandboxSetup {
            detail: "installed v1 application missing".to_owned(),
        })?;

    let userdata_dir = root.join("userdata");
    seed_baseline_userdata(&userdata_dir).map_err(|detail| DrillError::SandboxSetup { detail })?;
    let pre_upgrade_sha256 = compute_userdata_tree_sha256(&userdata_dir)
        .map_err(|detail| DrillError::SandboxSetup { detail })?;

    let drill_key = derive_key_16(MIGRATE_DRILL_KEY_DOMAIN, v2_receipt.receipt_id.as_bytes());
    Ok(PreparedSandbox {
        application_id: app_view.application_id,
        v1_receipt,
        v2_receipt,
        userdata_dir,
        pre_upgrade_sha256,
        drill_key,
    })
}

fn enforce_irreversible_governance_gate(
    root: &Path,
    request: &DrillRequest,
    prepared: &PreparedSandbox,
) -> Result<(), DrillError> {
    let backup_receipt = if let Some(explicit) = request.pre_backup_receipt.clone() {
        explicit
    } else if request.ephemeral_irreversible.create_verified_backup {
        let ext_backup_dir = root.join("external_pre_upgrade_backup");
        copy_dir_recursive(&prepared.userdata_dir, &ext_backup_dir, true)
            .map_err(|detail| DrillError::IrreversibleMissingPreBackup { detail })?;
        PreUpgradeBackupReceipt {
            backup_dir: ext_backup_dir,
            declared_userdata_sha256: prepared.pre_upgrade_sha256,
        }
    } else {
        return Err(DrillError::IrreversibleMissingPreBackup {
            detail: "RISK-E-05 requires a verified PreUpgradeBackupReceipt before IRREVERSIBLE \
                     migration begins"
                .to_owned(),
        });
    };

    verify_pre_upgrade_backup_receipt(
        &backup_receipt,
        &prepared.userdata_dir,
        prepared.pre_upgrade_sha256,
    )?;

    let consent_receipt = if let Some(explicit) = request.z4_consent_receipt {
        explicit
    } else if request.ephemeral_irreversible.issue_z4_consent {
        let mut issued = Z4IrreversibleConsentReceipt::issue(
            prepared.application_id,
            Generation::INITIAL,
            prepared.v2_receipt.manifest_digest,
            backup_receipt.declared_userdata_sha256,
        );
        if let Some(override_tok) = request.ephemeral_irreversible.override_z4_token {
            issued.confirmation_token = override_tok;
        }
        issued
    } else {
        return Err(DrillError::IrreversibleInvalidZ4Consent {
            detail: "RISK-E-05 requires a Z4IrreversibleConsentReceipt from Trusted UI before \
                     IRREVERSIBLE migration begins"
                .to_owned(),
        });
    };

    verify_z4_consent_receipt(&consent_receipt, prepared)
}

fn verify_pre_upgrade_backup_receipt(
    receipt: &PreUpgradeBackupReceipt,
    live_userdata_dir: &Path,
    live_sha256: [u8; 32],
) -> Result<(), DrillError> {
    if receipt.backup_dir == live_userdata_dir || receipt.backup_dir.starts_with(live_userdata_dir)
    {
        return Err(DrillError::IrreversibleMissingPreBackup {
            detail: "pre-upgrade backup directory must be separate from live userdata directory"
                .to_owned(),
        });
    }
    let actual_backup_sha = compute_userdata_tree_sha256(&receipt.backup_dir)
        .map_err(|detail| DrillError::IrreversibleMissingPreBackup { detail })?;
    if actual_backup_sha != receipt.declared_userdata_sha256 || actual_backup_sha != live_sha256 {
        return Err(DrillError::IrreversibleMissingPreBackup {
            detail: "pre-upgrade backup SHA-256 does not match live pre-upgrade UserData digest"
                .to_owned(),
        });
    }
    Ok(())
}

fn verify_z4_consent_receipt(
    receipt: &Z4IrreversibleConsentReceipt,
    prepared: &PreparedSandbox,
) -> Result<(), DrillError> {
    let expected_token = derive_z4_consent_token(
        prepared.application_id,
        Generation::INITIAL,
        prepared.v2_receipt.manifest_digest,
        prepared.pre_upgrade_sha256,
    );
    if receipt.application_id != prepared.application_id
        || receipt.from_generation != Generation::INITIAL
        || receipt.target_manifest_digest != prepared.v2_receipt.manifest_digest
        || receipt.backup_userdata_sha256 != prepared.pre_upgrade_sha256
        || receipt.confirmation_token != expected_token
    {
        return Err(DrillError::IrreversibleInvalidZ4Consent {
            detail: "Z4 consent receipt binding or confirmation_token mismatch".to_owned(),
        });
    }
    Ok(())
}

fn execute_staged_drill(
    root: &Path,
    request: &DrillRequest,
    steps: &[String],
    prepared: &PreparedSandbox,
    snapshot: Option<&UserDataSnapshotHandle>,
) -> Result<DrillReport, DrillError> {
    let declared_steps = steps.len() as u64;
    let mig_req = MigrateApplicationRequest {
        package_id: request.packages.v1.package_id,
        package_verification_receipt_id: prepared.v2_receipt.receipt_id,
        idempotency_key: prepared.drill_key,
        compatibility_window: CompatibilityWindow::SameMajor,
        declared_step_count: declared_steps,
        requested_at_ms: 4_000,
    };

    let outcome = match request.fault_injection {
        DrillFaultInjection::CrashAfterStep(crash_at) => {
            let replayed = run_crash_and_recover_steps(root, &mig_req, steps, prepared, crash_at)?;
            let mutated_sha = compute_userdata_tree_sha256(&prepared.userdata_dir)
                .map_err(|detail| DrillError::RollbackOrHealthFailure { detail })?;
            finalize_healthy_activation(root, request.mode, prepared)?;
            return build_drill_report(
                root,
                request,
                prepared,
                DrillOutcomeKind::RecoveredCrashAndActivatedGeneration2 {
                    crashed_after_step: crash_at,
                    replayed_steps: replayed,
                },
                Some(mutated_sha),
                declared_steps,
            );
        }
        DrillFaultInjection::None => {
            run_all_steps_once(root, &mig_req, steps, prepared)?;
            let mutated_sha = compute_userdata_tree_sha256(&prepared.userdata_dir)
                .map_err(|detail| DrillError::RollbackOrHealthFailure { detail })?;
            finalize_healthy_activation(root, request.mode, prepared)?;
            return build_drill_report(
                root,
                request,
                prepared,
                DrillOutcomeKind::ActivatedGeneration2,
                Some(mutated_sha),
                declared_steps,
            );
        }
        DrillFaultInjection::PostMigrationHealthFailure => {
            run_all_steps_once(root, &mig_req, steps, prepared)?;
            fs::write(
                prepared.userdata_dir.join("v2_unhealthy_index.bin"),
                b"dirty-half-migrated-v2-artifact\n",
            )
            .map_err(|err| DrillError::RollbackOrHealthFailure {
                detail: format!("write dirty marker: {err}"),
            })?;
            let mutated_sha = compute_userdata_tree_sha256(&prepared.userdata_dir)
                .map_err(|detail| DrillError::RollbackOrHealthFailure { detail })?;
            execute_health_failure_rollback(root, prepared, snapshot)?;
            (
                DrillOutcomeKind::RolledBackOnHealthFailure,
                Some(mutated_sha),
            )
        }
    };

    build_drill_report(
        root,
        request,
        prepared,
        outcome.0,
        outcome.1,
        declared_steps,
    )
}

fn run_all_steps_once(
    root: &Path,
    mig_req: &MigrateApplicationRequest,
    steps: &[String],
    prepared: &PreparedSandbox,
) -> Result<(), DrillError> {
    let artifacts = open_artifacts(root)?;
    let applications = open_applications(root)?;
    match applications
        .migrate_application(&artifacts, *mig_req)
        .map_err(|err| DrillError::RollbackOrHealthFailure {
            detail: format!("migrate_application: {err}"),
        })? {
        MigrateDecision::Started(_) | MigrateDecision::Replayed(_) => {}
    }

    for (idx, sql) in steps.iter().enumerate() {
        let step_idx = (idx as u64) + 1;
        apply_userdata_sql(&prepared.userdata_dir, sql)?;
        applications
            .record_migration_step(RecordMigrationStepRequest {
                idempotency_key: prepared.drill_key,
                step_index: step_idx,
                completed_at_ms: 4_000 + step_idx * 100,
            })
            .map_err(|err| DrillError::RollbackOrHealthFailure {
                detail: format!("record_migration_step #{step_idx}: {err}"),
            })?;
    }
    Ok(())
}

fn run_crash_and_recover_steps(
    root: &Path,
    mig_req: &MigrateApplicationRequest,
    steps: &[String],
    prepared: &PreparedSandbox,
    crash_at: u64,
) -> Result<u64, DrillError> {
    {
        let artifacts = open_artifacts(root)?;
        let applications = open_applications(root)?;
        applications
            .migrate_application(&artifacts, *mig_req)
            .map_err(|err| DrillError::RollbackOrHealthFailure {
                detail: format!("migrate_application pre-crash: {err}"),
            })?;
        for (idx, sql) in steps
            .iter()
            .enumerate()
            .take(usize::try_from(crash_at).unwrap_or(0))
        {
            let step_idx = (idx as u64) + 1;
            apply_userdata_sql(&prepared.userdata_dir, sql)?;
            applications
                .record_migration_step(RecordMigrationStepRequest {
                    idempotency_key: prepared.drill_key,
                    step_index: step_idx,
                    completed_at_ms: 4_000 + step_idx * 100,
                })
                .map_err(|err| DrillError::RollbackOrHealthFailure {
                    detail: format!("record step #{step_idx} pre-crash: {err}"),
                })?;
        }
    }

    let artifacts = open_artifacts(root)?;
    let applications = open_applications(root)?;
    let replayed_view = match applications
        .migrate_application(&artifacts, *mig_req)
        .map_err(|err| DrillError::RollbackOrHealthFailure {
            detail: format!("replay migrate_application after crash: {err}"),
        })? {
        MigrateDecision::Replayed(view) => view,
        MigrateDecision::Started(_) => {
            return Err(DrillError::RollbackOrHealthFailure {
                detail: "expected durable Replayed decision after mid-step crash".to_owned(),
            });
        }
    };
    if replayed_view.state != MigrationState::Running
        || replayed_view.completed_step_count != crash_at
    {
        return Err(DrillError::RollbackOrHealthFailure {
            detail: format!(
                "unexpected post-crash migration state {:?} completed={}",
                replayed_view.state, replayed_view.completed_step_count
            ),
        });
    }

    let mut replayed_count = 0u64;
    let crash_len = usize::try_from(crash_at).unwrap_or(0);
    for idx in 0..crash_len {
        let step_idx = (idx as u64) + 1;
        match applications
            .record_migration_step(RecordMigrationStepRequest {
                idempotency_key: prepared.drill_key,
                step_index: step_idx,
                completed_at_ms: 4_000 + step_idx * 100,
            })
            .map_err(|err| DrillError::RollbackOrHealthFailure {
                detail: format!("replay pre-crash step #{step_idx}: {err}"),
            })? {
            RecordMigrationStepDecision::Replayed(_) => replayed_count += 1,
            RecordMigrationStepDecision::Recorded(_) => {
                return Err(DrillError::RollbackOrHealthFailure {
                    detail: format!("step #{step_idx} was not durably replayed"),
                });
            }
        }
    }

    for (idx, sql) in steps.iter().enumerate().skip(crash_len) {
        let step_idx = (idx as u64) + 1;
        apply_userdata_sql(&prepared.userdata_dir, sql)?;
        match applications
            .record_migration_step(RecordMigrationStepRequest {
                idempotency_key: prepared.drill_key,
                step_index: step_idx,
                completed_at_ms: 4_000 + step_idx * 100,
            })
            .map_err(|err| DrillError::RollbackOrHealthFailure {
                detail: format!("record post-recovery step #{step_idx}: {err}"),
            })? {
            RecordMigrationStepDecision::Recorded(_) => {}
            RecordMigrationStepDecision::Replayed(_) => {
                return Err(DrillError::RollbackOrHealthFailure {
                    detail: format!("post-crash step #{step_idx} unexpectedly replayed"),
                });
            }
        }
    }
    Ok(replayed_count)
}

fn finalize_healthy_activation(
    root: &Path,
    mode: DataMigrationMode,
    prepared: &PreparedSandbox,
) -> Result<(), DrillError> {
    let window_ok = if mode == DataMigrationMode::BackwardCompatibleWindow {
        verify_dual_v1_v2_window_queries(&prepared.userdata_dir)?;
        true
    } else {
        true
    };

    let artifacts = open_artifacts(root)?;
    let applications = open_applications(root)?;
    let probe = ConfigurableHealthProbe { healthy: window_ok };
    let health = applications
        .run_migration_health_check(&artifacts, prepared.drill_key, &probe, 6_000)
        .map_err(|err| DrillError::RollbackOrHealthFailure {
            detail: format!("run_migration_health_check: {err}"),
        })?;
    let passed = match health {
        nlos_application::MigrationHealthDecision::Recorded(rep)
        | nlos_application::MigrationHealthDecision::Replayed(rep) => rep.passed,
    };
    if !passed {
        return Err(DrillError::RollbackOrHealthFailure {
            detail: "expected healthy probe verdict on positive path".to_owned(),
        });
    }

    match applications
        .activate_package_migration(
            &artifacts,
            ActivatePackageMigrationRequest {
                idempotency_key: prepared.drill_key,
                activated_at_ms: 6_500,
            },
        )
        .map_err(|err| DrillError::RollbackOrHealthFailure {
            detail: format!("activate_package_migration: {err}"),
        })? {
        ActivateMigrationDecision::Activated(receipt)
        | ActivateMigrationDecision::Replayed(receipt) => {
            if receipt.installation_generation.get() != 2 {
                return Err(DrillError::RollbackOrHealthFailure {
                    detail: format!(
                        "expected activated generation 2, got {}",
                        receipt.installation_generation.get()
                    ),
                });
            }
        }
    }
    Ok(())
}

fn execute_health_failure_rollback(
    root: &Path,
    prepared: &PreparedSandbox,
    snapshot: Option<&UserDataSnapshotHandle>,
) -> Result<(), DrillError> {
    let artifacts = open_artifacts(root)?;
    let applications = open_applications(root)?;
    let unhealthy_probe = ConfigurableHealthProbe { healthy: false };

    applications
        .run_migration_health_check(&artifacts, prepared.drill_key, &unhealthy_probe, 6_000)
        .map_err(|err| DrillError::RollbackOrHealthFailure {
            detail: format!("run unhealthy check: {err}"),
        })?;

    let rollback_decision = applications
        .rollback_package_migration(RollbackPackageMigrationRequest {
            idempotency_key: prepared.drill_key,
            rolled_back_at_ms: 6_500,
        })
        .map_err(|err| DrillError::RollbackOrHealthFailure {
            detail: format!("rollback_package_migration: {err}"),
        })?;
    let receipt = match rollback_decision {
        RollbackMigrationDecision::RolledBack(rec) | RollbackMigrationDecision::Replayed(rec) => {
            rec
        }
    };
    if receipt.retained_generation != Generation::INITIAL
        || receipt.retained_manifest_digest != prepared.v1_receipt.manifest_digest
    {
        return Err(DrillError::RollbackOrHealthFailure {
            detail: "rollback receipt failed to retain baseline generation 1 and v1 digest"
                .to_owned(),
        });
    }

    if let Some(snap_handle) = snapshot {
        let restored_sha = restore_userdata_snapshot(snap_handle, &prepared.userdata_dir)
            .map_err(|detail| DrillError::RollbackOrHealthFailure { detail })?;
        if restored_sha != prepared.pre_upgrade_sha256 {
            return Err(DrillError::RollbackOrHealthFailure {
                detail: "restored UserData SHA-256 did not match pre-upgrade SHA-256".to_owned(),
            });
        }
    }
    Ok(())
}

fn verify_dual_v1_v2_window_queries(userdata_dir: &Path) -> Result<(), DrillError> {
    let conn = open_userdata_sqlite(&userdata_dir.join("app_data.db"))
        .map_err(|detail| DrillError::WindowIncompatibility { detail })?;
    conn.execute(
        "INSERT INTO user_documents (doc_id, title, body, schema_rev)
         VALUES ('doc-v1-writer', 'LegacyV1Write', 'Written by v1 schema client', 1)",
        [],
    )
    .map_err(|err| DrillError::WindowIncompatibility {
        detail: format!("v1 writer INSERT failed during shared window: {err}"),
    })?;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM user_documents WHERE doc_id = 'doc-v1-writer'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| DrillError::WindowIncompatibility {
            detail: format!("v1/v2 shared SELECT failed: {err}"),
        })?;
    if count != 1 {
        return Err(DrillError::WindowIncompatibility {
            detail: "shared v1/v2 readback returned unexpected row count".to_owned(),
        });
    }
    Ok(())
}

fn apply_userdata_sql(userdata_dir: &Path, sql: &str) -> Result<(), DrillError> {
    let conn = open_userdata_sqlite(&userdata_dir.join("app_data.db"))
        .map_err(|detail| DrillError::RollbackOrHealthFailure { detail })?;
    conn.execute_batch(sql)
        .map_err(|err| DrillError::RollbackOrHealthFailure {
            detail: format!("execute UserData SQL ({sql:?}): {err}"),
        })
}

fn open_artifacts(root: &Path) -> Result<ArtifactStore, DrillError> {
    ArtifactStore::open(root.join("artifacts")).map_err(|err| DrillError::SandboxSetup {
        detail: format!("reopen ArtifactStore: {err}"),
    })
}

fn open_applications(root: &Path) -> Result<ApplicationAuthority, DrillError> {
    ApplicationAuthority::open(root.join("applications")).map_err(|err| DrillError::SandboxSetup {
        detail: format!("reopen ApplicationAuthority: {err}"),
    })
}

fn build_drill_report(
    root: &Path,
    request: &DrillRequest,
    prepared: &PreparedSandbox,
    outcome: DrillOutcomeKind,
    mutated_sha256: Option<[u8; 32]>,
    declared_steps: u64,
) -> Result<DrillReport, DrillError> {
    let applications = open_applications(root)?;
    let app_view = applications
        .inspect_application(request.packages.v1.package_id)
        .map_err(|err| DrillError::RollbackOrHealthFailure {
            detail: format!("inspect final application: {err}"),
        })?
        .ok_or_else(|| DrillError::RollbackOrHealthFailure {
            detail: "application row missing after drill".to_owned(),
        })?;
    if app_view.status != ApplicationStatus::Installed {
        return Err(DrillError::RollbackOrHealthFailure {
            detail: format!("unexpected application status {:?}", app_view.status),
        });
    }

    let mig_view = applications
        .inspect_package_migration(prepared.drill_key)
        .map_err(|err| DrillError::RollbackOrHealthFailure {
            detail: format!("inspect final migration view: {err}"),
        })?
        .ok_or_else(|| DrillError::RollbackOrHealthFailure {
            detail: "migration row missing after drill".to_owned(),
        })?;

    if matches!(outcome, DrillOutcomeKind::RolledBackOnHealthFailure)
        && (mig_view.state != MigrationState::Failed
            || mig_view.health != MigrationHealthState::Failed)
    {
        return Err(DrillError::RollbackOrHealthFailure {
            detail: "rollback drill did not commit terminal Failed state".to_owned(),
        });
    }

    let final_userdata_sha256 = compute_userdata_tree_sha256(&prepared.userdata_dir)
        .map_err(|detail| DrillError::RollbackOrHealthFailure { detail })?;

    Ok(DrillReport {
        mode: request.mode,
        outcome,
        package_id_hex: hex_encode(request.packages.v1.package_id.as_bytes()),
        application_id_hex: hex_encode(prepared.application_id.as_bytes()),
        initial_generation: 1,
        final_generation: app_view.current_installation_generation.get(),
        v1_manifest_digest_hex: hex_encode(prepared.v1_receipt.manifest_digest.as_bytes()),
        v2_manifest_digest_hex: hex_encode(prepared.v2_receipt.manifest_digest.as_bytes()),
        active_manifest_digest_hex: hex_encode(app_view.package_manifest_digest.as_bytes()),
        pre_upgrade_userdata_sha256_hex: hex_encode(&prepared.pre_upgrade_sha256),
        mutated_userdata_sha256_hex: mutated_sha256.map(|d| hex_encode(&d)),
        final_userdata_sha256_hex: hex_encode(&final_userdata_sha256),
        userdata_matches_pre_upgrade: final_userdata_sha256 == prepared.pre_upgrade_sha256,
        steps_declared: declared_steps,
        steps_completed: mig_view.completed_step_count,
    })
}

/// Helper to build a bound `(PreUpgradeBackupReceipt, Z4IrreversibleConsentReceipt)` pair
/// for an external pre-upgrade backup directory.
#[must_use]
pub fn issue_irreversible_governance_bundle(
    backup_dir: PathBuf,
    backup_userdata_sha256: [u8; 32],
    application_id: ApplicationId,
    from_generation: Generation,
    target_manifest_digest: ContentDigest,
) -> (PreUpgradeBackupReceipt, Z4IrreversibleConsentReceipt) {
    let backup = PreUpgradeBackupReceipt {
        backup_dir,
        declared_userdata_sha256: backup_userdata_sha256,
    };
    let consent = Z4IrreversibleConsentReceipt::issue(
        application_id,
        from_generation,
        target_manifest_digest,
        backup_userdata_sha256,
    );
    (backup, consent)
}
