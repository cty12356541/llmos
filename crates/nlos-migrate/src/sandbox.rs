//! `[PKG-UPDATE-001]` three-mode data container migration and atomic rollback
//! sandbox runner (`BackwardCompatibleWindow`, `SnapshotAssistedRollback`,
//! `Irreversible`) backed by real `IdentityAuthority`, `ArtifactStore`, and
//! `ApplicationAuthority`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{
    ActivateMigrationDecision, ActivatePackageMigrationRequest, ApplicationAuthority,
    CompatibilityWindow, InstallApplicationRequest, InstallationReceipt, MigrateApplicationRequest,
    MigrationHealthContext, MigrationHealthProbe, MigrationRollbackReceipt,
    RecordMigrationStepRequest, RollbackMigrationDecision, RollbackPackageMigrationRequest,
    pack_package_version,
};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
    PackageManifestEntry, PackageVerificationReceipt, ProvenanceSourceTriple, PutRevisionRequest,
    SignedPackage, VerifyPackageRequest, derive_artifact_id, package_manifest_message,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose, RevokeKeyRequest,
    VerifyCapabilityCommandSignatureRequest,
};
use nlos_types::{Generation, IdempotencyKey, PackageId, PrincipalId};
use sha2::{Digest, Sha256};

use crate::json::{hex_encode, json_opt_str, json_opt_u64, json_quote};

const TREE_DIGEST_DOMAIN: &[u8] = b"llmos/migrate/container-tree/v1";
const BACKUP_ARCHIVE_MAGIC: &[u8] = b"NLOS-MIGRATE-BACKUP-V1\0";
const Z4_CONSENT_DOMAIN: &[u8] = b"llmos/migrate/z4-trusted-ui-consent/v1";

static EPHEMERAL_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Statutory `[PKG-UPDATE-001]` container migration mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationMode {
    /// Mode 1: Read/write backward-compatible window + single-transaction
    /// atomic generation switch (or zero-pollution rollback on health failure).
    BackwardCompatibleWindow,
    /// Mode 2: Content-addressed container snapshot prior to step 1 +
    /// byte-identical restore on mid-step crash or health-check failure.
    SnapshotAssistedRollback,
    /// Mode 3: Destructive/irreversible migration gated by mandatory verified
    /// `PreUpgradeBackupReceipt` + `IdentityAuthority`-verified `Z4` Trusted-UI
    /// Ed25519 consent signature (`RISK-E-05`).
    Irreversible,
}

impl MigrationMode {
    /// Canonical specification token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BackwardCompatibleWindow => "BACKWARD_COMPATIBLE_WINDOW",
            Self::SnapshotAssistedRollback => "SNAPSHOT_ASSISTED_ROLLBACK",
            Self::Irreversible => "IRREVERSIBLE",
        }
    }

    /// Parses CLI `--mode` values (`window`, `snapshot`, `irreversible`, or
    /// full specification tokens).
    #[must_use]
    pub fn parse_cli(token: &str) -> Option<Self> {
        match token {
            "window" | "BACKWARD_COMPATIBLE_WINDOW" => Some(Self::BackwardCompatibleWindow),
            "snapshot" | "SNAPSHOT_ASSISTED_ROLLBACK" => Some(Self::SnapshotAssistedRollback),
            "irreversible" | "IRREVERSIBLE" => Some(Self::Irreversible),
            _ => None,
        }
    }
}

/// Classification of the actual container data transformation performed by a
/// migration plan, used to intercept destructive rewrites disguised as
/// reversible modes (`RISK-E-05`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerMutationClass {
    /// Additive or dual-readable change compatible with `from_generation`.
    AdditiveDualReadable,
    /// In-place v2 schema rewrite requiring pre-step snapshot restore capability.
    SchemaTransformRequiringSnapshot,
    /// Destructive column/field deletion or irreversible state compaction.
    DestructiveFieldDeletionOrIrreversibleRewrite,
}

impl ContainerMutationClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AdditiveDualReadable => "ADDITIVE_DUAL_READABLE",
            Self::SchemaTransformRequiringSnapshot => "SCHEMA_TRANSFORM_REQUIRING_SNAPSHOT",
            Self::DestructiveFieldDeletionOrIrreversibleRewrite => {
                "DESTRUCTIVE_IRREVERSIBLE_REWRITE"
            }
        }
    }
}

/// Content-addressed snapshot receipt captured before step 1 in
/// [`MigrationMode::SnapshotAssistedRollback`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerSnapshotReceipt {
    pub snapshot_id_hex: String,
    pub snapshot_dir: PathBuf,
    pub tree_digest: [u8; 32],
    pub file_count: usize,
    pub total_bytes: u64,
    pub captured_at_ms: u64,
}

/// On-disk verified archive receipt required by [`MigrationMode::Irreversible`]
/// (`RISK-E-05` Gate A).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreUpgradeBackupReceipt {
    pub backup_archive_path: PathBuf,
    pub archive_digest: [u8; 32],
    pub source_tree_digest: [u8; 32],
    pub package_id: PackageId,
    pub from_manifest_digest: ContentDigest,
    pub created_at_ms: u64,
}

/// Trusted-UI (`Z4`) explicit user/operator confirmation receipt verified via
/// `IdentityAuthority` Ed25519 before [`MigrationMode::Irreversible`] executes
/// (`RISK-E-05` Gate B).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Z4TrustedUiConsentReceipt {
    pub operator_principal: PrincipalId,
    pub package_id: PackageId,
    pub from_manifest_digest: ContentDigest,
    pub target_manifest_digest: ContentDigest,
    pub backup_archive_digest: [u8; 32],
    pub mode_tag: String,
    pub message_digest: [u8; 32],
    pub signature: [u8; 64],
    pub consented_at_ms: u64,
}

/// Fail-closed error returned when a migration drill is blocked by a safety
/// gate or encounters an authority/IO/invariant failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationDrillError {
    /// Destructive/irreversible data mutation disguised as `BackwardCompatibleWindow`
    /// or `SnapshotAssistedRollback` (`RISK-E-05`).
    DisguisedIrreversibleMigration {
        declared_mode: MigrationMode,
        mutation_class: ContainerMutationClass,
        detail: String,
    },
    /// In-place schema rewrite declared under `BackwardCompatibleWindow` without
    /// snapshot protection.
    IncompatibleMutationClass {
        declared_mode: MigrationMode,
        mutation_class: ContainerMutationClass,
        detail: String,
    },
    /// `Irreversible` migration attempted without a `PreUpgradeBackupReceipt`.
    MissingPreUpgradeBackup,
    /// `PreUpgradeBackupReceipt` file missing, corrupted, tampered, or bound to
    /// a different package/baseline.
    InvalidPreUpgradeBackup { detail: String },
    /// `Irreversible` migration attempted without a `Z4TrustedUiConsentReceipt`.
    MissingZ4TrustedUiConsent,
    /// `Z4TrustedUiConsentReceipt` failed binding or `IdentityAuthority` Ed25519
    /// verification.
    InvalidZ4TrustedUiConsent { detail: String },
    /// Underlying authority or filesystem failure.
    AuthorityFailure { detail: String },
    /// Post-rollback mechanical verification detected digest drift or residual
    /// temp files.
    RollbackInvariantViolation { detail: String },
}

impl MigrationDrillError {
    /// Machine-readable error code for JSON reports.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::DisguisedIrreversibleMigration { .. } => "DISGUISED_IRREVERSIBLE_MIGRATION",
            Self::IncompatibleMutationClass { .. } => "INCOMPATIBLE_MUTATION_CLASS",
            Self::MissingPreUpgradeBackup => "MISSING_PRE_UPGRADE_BACKUP",
            Self::InvalidPreUpgradeBackup { .. } => "INVALID_PRE_UPGRADE_BACKUP",
            Self::MissingZ4TrustedUiConsent => "MISSING_Z4_TRUSTED_UI_CONSENT",
            Self::InvalidZ4TrustedUiConsent { .. } => "INVALID_Z4_TRUSTED_UI_CONSENT",
            Self::AuthorityFailure { .. } => "AUTHORITY_FAILURE",
            Self::RollbackInvariantViolation { .. } => "ROLLBACK_INVARIANT_VIOLATION",
        }
    }
}

impl fmt::Display for MigrationDrillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DisguisedIrreversibleMigration {
                declared_mode,
                mutation_class,
                detail,
            } => write!(
                formatter,
                "blocked disguised irreversible migration (declared={}, actual={}): {detail}",
                declared_mode.as_str(),
                mutation_class.as_str()
            ),
            Self::IncompatibleMutationClass {
                declared_mode,
                mutation_class,
                detail,
            } => write!(
                formatter,
                "incompatible container mutation {} for mode {}: {detail}",
                mutation_class.as_str(),
                declared_mode.as_str()
            ),
            Self::MissingPreUpgradeBackup => formatter.write_str(
                "IRREVERSIBLE migration blocked: missing mandatory PreUpgradeBackupReceipt",
            ),
            Self::InvalidPreUpgradeBackup { detail } => {
                write!(
                    formatter,
                    "IRREVERSIBLE migration blocked: invalid PreUpgradeBackupReceipt ({detail})"
                )
            }
            Self::MissingZ4TrustedUiConsent => formatter.write_str(
                "IRREVERSIBLE migration blocked: missing mandatory Z4TrustedUiConsentReceipt",
            ),
            Self::InvalidZ4TrustedUiConsent { detail } => {
                write!(
                    formatter,
                    "IRREVERSIBLE migration blocked: Z4TrustedUiConsentReceipt rejected ({detail})"
                )
            }
            Self::AuthorityFailure { detail } => {
                write!(formatter, "migration authority failure: {detail}")
            }
            Self::RollbackInvariantViolation { detail } => {
                write!(formatter, "rollback invariant violation: {detail}")
            }
        }
    }
}

impl Error for MigrationDrillError {}

/// Execution parameters for one sandbox migration run.
#[derive(Clone, Debug)]
pub struct MigrationExecutionPlan {
    pub mode: MigrationMode,
    pub mutation_class: ContainerMutationClass,
    pub target_receipt: PackageVerificationReceipt,
    pub idempotency_key: IdempotencyKey,
    pub declared_steps: u64,
    pub crash_at_step: Option<u64>,
    pub health_check_passes: bool,
    pub pre_upgrade_backup: Option<PreUpgradeBackupReceipt>,
    pub z4_consent: Option<Z4TrustedUiConsentReceipt>,
    pub started_at_ms: u64,
}

/// Terminal disposition of a completed migration drill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationDrillDisposition {
    /// Target revision passed health check and atomically activated at
    /// `from_generation + 1`.
    Activated {
        installation_receipt: InstallationReceipt,
    },
    /// Health check failed or mid-step crash occurred; `ApplicationAuthority`
    /// committed `MigrationRollbackReceipt` (keeping `from_generation` active)
    /// and `UserDataContainer` verified byte-identical to pre-upgrade state.
    RolledBackAtomically {
        rollback_receipt: MigrationRollbackReceipt,
    },
}

/// Complete receipt returned by [`MigrationSandboxHarness::run_migration_plan`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationDrillReceipt {
    pub mode: MigrationMode,
    pub package_id: PackageId,
    pub from_generation: Generation,
    pub active_generation_after: Generation,
    pub pre_tree_digest: [u8; 32],
    pub post_tree_digest: [u8; 32],
    pub snapshot_receipt: Option<ContainerSnapshotReceipt>,
    pub backup_digest: Option<[u8; 32]>,
    pub z4_operator_principal: Option<PrincipalId>,
    pub residual_temp_files: usize,
    pub byte_identical_on_rollback: bool,
    pub disposition: MigrationDrillDisposition,
}

impl MigrationDrillReceipt {
    #[must_use]
    pub const fn outcome_str(&self) -> &'static str {
        match self.disposition {
            MigrationDrillDisposition::Activated { .. } => "ACTIVATED",
            MigrationDrillDisposition::RolledBackAtomically { .. } => "ROLLED_BACK_ATOMICALLY",
        }
    }

    /// Renders a deterministic plain-text drill summary (zero Emoji).
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "NLOS-MIGRATE SANDBOX DRILL REPORT");
        let _ = writeln!(out, "status: PASS");
        let _ = writeln!(out, "mode: {}", self.mode.as_str());
        let _ = writeln!(out, "outcome: {}", self.outcome_str());
        let _ = writeln!(
            out,
            "package_id: {}",
            hex_encode(self.package_id.as_bytes())
        );
        let _ = writeln!(out, "from_generation: {}", self.from_generation.get());
        let _ = writeln!(
            out,
            "active_generation_after: {}",
            self.active_generation_after.get()
        );
        let _ = writeln!(
            out,
            "pre_tree_digest: {}",
            hex_encode(&self.pre_tree_digest)
        );
        let _ = writeln!(
            out,
            "post_tree_digest: {}",
            hex_encode(&self.post_tree_digest)
        );
        let _ = writeln!(out, "residual_temp_files: {}", self.residual_temp_files);
        let _ = writeln!(
            out,
            "byte_identical_on_rollback: {}",
            self.byte_identical_on_rollback
        );
        if let Some(snap) = &self.snapshot_receipt {
            let _ = writeln!(out, "snapshot_id: {}", snap.snapshot_id_hex);
            let _ = writeln!(out, "snapshot_files: {}", snap.file_count);
        }
        if let Some(backup) = &self.backup_digest {
            let _ = writeln!(out, "backup_archive_digest: {}", hex_encode(backup));
        }
        if let Some(operator) = &self.z4_operator_principal {
            let _ = writeln!(
                out,
                "z4_operator_principal: {}",
                hex_encode(operator.as_bytes())
            );
        }
        match &self.disposition {
            MigrationDrillDisposition::Activated {
                installation_receipt,
            } => {
                let _ = writeln!(
                    out,
                    "installation_id: {}",
                    hex_encode(installation_receipt.installation_id.as_bytes())
                );
            }
            MigrationDrillDisposition::RolledBackAtomically { rollback_receipt } => {
                let _ = writeln!(
                    out,
                    "retained_generation: {}",
                    rollback_receipt.retained_generation.get()
                );
                let _ = writeln!(
                    out,
                    "retained_manifest_digest: {}",
                    hex_encode(rollback_receipt.retained_manifest_digest.as_bytes())
                );
            }
        }
        out
    }

    /// Renders a deterministic RFC 8259 JSON drill report.
    #[must_use]
    pub fn render_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        let _ = writeln!(out, "  \"schema\": \"nlos.migrate.drill.v1\",");
        let _ = writeln!(out, "  \"status\": \"PASS\",");
        let _ = writeln!(out, "  \"exit_code\": 0,");
        let _ = writeln!(out, "  \"mode\": {},", json_quote(self.mode.as_str()));
        let _ = writeln!(out, "  \"outcome\": {},", json_quote(self.outcome_str()));
        let _ = writeln!(
            out,
            "  \"package_id\": {},",
            json_quote(&hex_encode(self.package_id.as_bytes()))
        );
        let _ = writeln!(
            out,
            "  \"from_generation\": {},",
            self.from_generation.get()
        );
        let _ = writeln!(
            out,
            "  \"active_generation_after\": {},",
            self.active_generation_after.get()
        );
        let _ = writeln!(
            out,
            "  \"pre_tree_digest\": {},",
            json_quote(&hex_encode(&self.pre_tree_digest))
        );
        let _ = writeln!(
            out,
            "  \"post_tree_digest\": {},",
            json_quote(&hex_encode(&self.post_tree_digest))
        );
        let _ = writeln!(
            out,
            "  \"residual_temp_files\": {},",
            self.residual_temp_files
        );
        let _ = writeln!(
            out,
            "  \"byte_identical_on_rollback\": {},",
            self.byte_identical_on_rollback
        );
        let snapshot_id = self
            .snapshot_receipt
            .as_ref()
            .map(|snap| snap.snapshot_id_hex.as_str());
        let backup_hex = self.backup_digest.map(|digest| hex_encode(&digest));
        let operator_hex = self
            .z4_operator_principal
            .map(|principal| hex_encode(principal.as_bytes()));
        let _ = writeln!(out, "  \"snapshot_id\": {},", json_opt_str(snapshot_id));
        let _ = writeln!(
            out,
            "  \"backup_archive_digest\": {},",
            json_opt_str(backup_hex.as_deref())
        );
        let _ = writeln!(
            out,
            "  \"z4_operator_principal\": {},",
            json_opt_str(operator_hex.as_deref())
        );
        match &self.disposition {
            MigrationDrillDisposition::Activated {
                installation_receipt,
            } => {
                let _ = writeln!(
                    out,
                    "  \"installation_id\": {},",
                    json_quote(&hex_encode(installation_receipt.installation_id.as_bytes()))
                );
                let _ = writeln!(out, "  \"retained_generation\": null");
            }
            MigrationDrillDisposition::RolledBackAtomically { rollback_receipt } => {
                let _ = writeln!(out, "  \"installation_id\": null,");
                let _ = writeln!(
                    out,
                    "  \"retained_generation\": {}",
                    json_opt_u64(Some(rollback_receipt.retained_generation.get()))
                );
            }
        }
        out.push_str("}\n");
        out
    }
}

/// Renders a blocked/failed drill error in plain text or RFC 8259 JSON.
#[must_use]
pub fn render_drill_error(
    mode: MigrationMode,
    error: &MigrationDrillError,
    as_json: bool,
) -> String {
    if as_json {
        let mut out = String::new();
        out.push_str("{\n");
        let _ = writeln!(out, "  \"schema\": \"nlos.migrate.drill.v1\",");
        let _ = writeln!(out, "  \"status\": \"BLOCKED\",");
        let _ = writeln!(out, "  \"exit_code\": 3,");
        let _ = writeln!(out, "  \"mode\": {},", json_quote(mode.as_str()));
        let _ = writeln!(out, "  \"error_code\": {},", json_quote(error.code()));
        let _ = writeln!(out, "  \"message\": {}", json_quote(&error.to_string()));
        out.push_str("}\n");
        out
    } else {
        format!(
            "NLOS-MIGRATE SANDBOX DRILL REPORT\nstatus: BLOCKED\nmode: {}\nerror_code: {}\nmessage: {error}\n",
            mode.as_str(),
            error.code()
        )
    }
}

/// Computes the domain-separated 32-byte `Z4` Trusted-UI confirmation digest
/// strictly binding `(package_id, from_manifest_digest, target_manifest_digest,
/// backup_archive_digest, "IRREVERSIBLE")`.
#[must_use]
pub fn z4_irreversible_consent_digest(
    package_id: PackageId,
    from_manifest_digest: ContentDigest,
    target_manifest_digest: ContentDigest,
    backup_archive_digest: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(Z4_CONSENT_DOMAIN);
    hasher.update([0_u8]);
    hasher.update(package_id.as_bytes());
    hasher.update(from_manifest_digest.as_bytes());
    hasher.update(target_manifest_digest.as_bytes());
    hasher.update(backup_archive_digest);
    let mode_bytes = MigrationMode::Irreversible.as_str().as_bytes();
    let mode_len = u32::try_from(mode_bytes.len()).unwrap_or(0);
    hasher.update(mode_len.to_be_bytes());
    hasher.update(mode_bytes);
    hasher.finalize().into()
}

/// Signs a [`Z4TrustedUiConsentReceipt`] using an operator's Ed25519 signing key.
#[must_use]
pub fn sign_z4_irreversible_consent(
    signing_key: &SigningKey,
    operator_principal: PrincipalId,
    package_id: PackageId,
    from_manifest_digest: ContentDigest,
    target_manifest_digest: ContentDigest,
    backup_archive_digest: [u8; 32],
    consented_at_ms: u64,
) -> Z4TrustedUiConsentReceipt {
    let message_digest = z4_irreversible_consent_digest(
        package_id,
        from_manifest_digest,
        target_manifest_digest,
        backup_archive_digest,
    );
    let signature = signing_key.sign(&message_digest).to_bytes();
    Z4TrustedUiConsentReceipt {
        operator_principal,
        package_id,
        from_manifest_digest,
        target_manifest_digest,
        backup_archive_digest,
        mode_tag: MigrationMode::Irreversible.as_str().to_owned(),
        message_digest,
        signature,
        consented_at_ms,
    }
}

/// Computes a deterministic SHA-256 tree digest across every file inside
/// `container_dir` in lexicographic relative-path order.
///
/// # Errors
///
/// Returns [`MigrationDrillError::AuthorityFailure`] if directory traversal or
/// file reading fails.
pub fn compute_container_tree_digest(
    container_dir: &Path,
) -> Result<[u8; 32], MigrationDrillError> {
    let files = collect_container_files(container_dir)?;
    let mut hasher = Sha256::new();
    hasher.update(TREE_DIGEST_DOMAIN);
    let count = u64::try_from(files.len()).unwrap_or(u64::MAX);
    hasher.update(count.to_be_bytes());
    for (rel_path, bytes) in &files {
        let path_len = u32::try_from(rel_path.len()).unwrap_or(u32::MAX);
        hasher.update(path_len.to_be_bytes());
        hasher.update(rel_path.as_bytes());
        let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        hasher.update(byte_len.to_be_bytes());
        let file_hash: [u8; 32] = Sha256::digest(bytes).into();
        hasher.update(file_hash);
    }
    Ok(hasher.finalize().into())
}

/// Counts residual `.tmp` or `.partial` files inside `container_dir`.
///
/// # Errors
///
/// Returns [`MigrationDrillError::AuthorityFailure`] if traversal fails.
pub fn count_residual_temp_files(container_dir: &Path) -> Result<usize, MigrationDrillError> {
    let files = collect_container_files(container_dir)?;
    Ok(files
        .keys()
        .filter(|rel| {
            Path::new(rel.as_str()).extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("tmp") || ext.eq_ignore_ascii_case("partial")
            })
        })
        .count())
}

/// Captures an immutable content-addressed snapshot of `user_data_container`
/// into `snapshot_dir`.
///
/// # Errors
///
/// Fails closed if `user_data_container` cannot be read or `snapshot_dir`
/// cannot be written.
pub fn capture_container_snapshot(
    user_data_container: &Path,
    snapshot_dir: &Path,
    captured_at_ms: u64,
) -> Result<ContainerSnapshotReceipt, MigrationDrillError> {
    let files = collect_container_files(user_data_container)?;
    let tree_digest = compute_container_tree_digest(user_data_container)?;
    if snapshot_dir.exists() {
        fs::remove_dir_all(snapshot_dir).map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("clean snapshot dir {}: {err}", snapshot_dir.display()),
        })?;
    }
    fs::create_dir_all(snapshot_dir).map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("create snapshot dir {}: {err}", snapshot_dir.display()),
    })?;

    let mut total_bytes = 0_u64;
    for (rel_path, bytes) in &files {
        total_bytes = total_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(0));
        let dest = snapshot_dir.join(rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("create snapshot parent {}: {err}", parent.display()),
            })?;
        }
        fs::write(&dest, bytes).map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("write snapshot file {}: {err}", dest.display()),
        })?;
    }

    Ok(ContainerSnapshotReceipt {
        snapshot_id_hex: hex_encode(&tree_digest[..16]),
        snapshot_dir: snapshot_dir.to_path_buf(),
        tree_digest,
        file_count: files.len(),
        total_bytes,
        captured_at_ms,
    })
}

/// Atomically restores `user_data_container` from `snapshot`, removing all
/// v2 mutations and `.tmp` residuals and mechanically verifying SHA-256 tree
/// byte-identity.
///
/// # Errors
///
/// Fails closed if the snapshot itself was tampered or if the restored tree
/// digest differs by even a single bit.
pub fn restore_container_from_snapshot(
    snapshot: &ContainerSnapshotReceipt,
    user_data_container: &Path,
) -> Result<[u8; 32], MigrationDrillError> {
    let verify_snap_digest = compute_container_tree_digest(&snapshot.snapshot_dir)?;
    if verify_snap_digest != snapshot.tree_digest {
        return Err(MigrationDrillError::RollbackInvariantViolation {
            detail: "snapshot directory digest drifted before restore".to_owned(),
        });
    }

    let snap_files = collect_container_files(&snapshot.snapshot_dir)?;
    clear_directory_contents(user_data_container)?;

    for (rel_path, bytes) in &snap_files {
        let dest = user_data_container.join(rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("create restore parent {}: {err}", parent.display()),
            })?;
        }
        fs::write(&dest, bytes).map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("restore container file {}: {err}", dest.display()),
        })?;
    }

    let restored_digest = compute_container_tree_digest(user_data_container)?;
    let residual_tmp = count_residual_temp_files(user_data_container)?;
    if restored_digest != snapshot.tree_digest || residual_tmp != 0 {
        return Err(MigrationDrillError::RollbackInvariantViolation {
            detail: format!(
                "restored digest {} != snapshot digest {} (residual_tmp={residual_tmp})",
                hex_encode(&restored_digest),
                hex_encode(&snapshot.tree_digest)
            ),
        });
    }
    Ok(restored_digest)
}

/// Creates a persisted pre-upgrade backup archive on disk and returns its
/// SHA-256 [`PreUpgradeBackupReceipt`] (`RISK-E-05` Gate A).
///
/// # Errors
///
/// Fails closed if `user_data_container` cannot be archived or `archive_path`
/// cannot be written.
pub fn create_pre_upgrade_backup(
    user_data_container: &Path,
    archive_path: &Path,
    package_id: PackageId,
    from_manifest_digest: ContentDigest,
    created_at_ms: u64,
) -> Result<PreUpgradeBackupReceipt, MigrationDrillError> {
    let files = collect_container_files(user_data_container)?;
    let source_tree_digest = compute_container_tree_digest(user_data_container)?;

    let mut archive = Vec::new();
    archive.extend_from_slice(BACKUP_ARCHIVE_MAGIC);
    archive.extend_from_slice(package_id.as_bytes());
    archive.extend_from_slice(from_manifest_digest.as_bytes());
    archive.extend_from_slice(&source_tree_digest);
    let file_count = u64::try_from(files.len()).unwrap_or(0);
    archive.extend_from_slice(&file_count.to_be_bytes());
    for (rel_path, bytes) in &files {
        let path_len = u32::try_from(rel_path.len()).unwrap_or(0);
        archive.extend_from_slice(&path_len.to_be_bytes());
        archive.extend_from_slice(rel_path.as_bytes());
        let data_len = u64::try_from(bytes.len()).unwrap_or(0);
        archive.extend_from_slice(&data_len.to_be_bytes());
        archive.extend_from_slice(bytes);
    }

    if let Some(parent) = archive_path.parent() {
        fs::create_dir_all(parent).map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("create backup parent {}: {err}", parent.display()),
        })?;
    }
    fs::write(archive_path, &archive).map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("write backup archive {}: {err}", archive_path.display()),
    })?;

    let archive_digest: [u8; 32] = Sha256::digest(&archive).into();
    Ok(PreUpgradeBackupReceipt {
        backup_archive_path: archive_path.to_path_buf(),
        archive_digest,
        source_tree_digest,
        package_id,
        from_manifest_digest,
        created_at_ms,
    })
}

/// Real authority-backed sandbox environment coordinating `IdentityAuthority`,
/// `ArtifactStore`, `ApplicationAuthority`, and `UserDataContainer`.
pub struct MigrationSandboxHarness {
    pub root_dir: PathBuf,
    pub user_data_container: PathBuf,
    pub snapshots_dir: PathBuf,
    pub backups_dir: PathBuf,
    pub identity: IdentityAuthority,
    pub artifacts: ArtifactStore,
    pub applications: ApplicationAuthority,
    pub package_id: PackageId,
    pub publisher_key: SigningKey,
    pub publisher_binding: IdentityBinding,
    pub z4_operator_key: SigningKey,
    pub z4_operator_binding: IdentityBinding,
    pub baseline_verification: PackageVerificationReceipt,
    pub baseline_installation: InstallationReceipt,
}

impl MigrationSandboxHarness {
    /// Initializes a clean sandbox root with real `IdentityAuthority`,
    /// `ArtifactStore`, `ApplicationAuthority`, and `UserDataContainer`, and
    /// installs baseline package `v1.0.0` at `Generation = 1`.
    ///
    /// # Errors
    ///
    /// Returns [`MigrationDrillError::AuthorityFailure`] if authority bootstrap
    /// or baseline installation fails.
    #[allow(clippy::too_many_lines)]
    pub fn bootstrap(root_dir: &Path) -> Result<Self, MigrationDrillError> {
        fs::create_dir_all(root_dir).map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("create sandbox root {}: {err}", root_dir.display()),
        })?;
        let user_data_container = root_dir.join("user_data_container");
        let snapshots_dir = root_dir.join("snapshots");
        let backups_dir = root_dir.join("backups");
        for dir in [&user_data_container, &snapshots_dir, &backups_dir] {
            fs::create_dir_all(dir).map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("create sandbox subdir {}: {err}", dir.display()),
            })?;
        }

        let identity =
            IdentityAuthority::open(root_dir.join("identity.sqlite")).map_err(|err| {
                MigrationDrillError::AuthorityFailure {
                    detail: format!("open IdentityAuthority: {err}"),
                }
            })?;
        let artifacts = ArtifactStore::open(root_dir.join("artifacts.sqlite")).map_err(|err| {
            MigrationDrillError::AuthorityFailure {
                detail: format!("open ArtifactStore: {err}"),
            }
        })?;
        let applications = ApplicationAuthority::open(root_dir.join("applications.sqlite"))
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("open ApplicationAuthority: {err}"),
            })?;

        let publisher_key = SigningKey::from_bytes(&[0x31_u8; 32]);
        let publisher_binding = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [0x11_u8; 32],
                control_domain_policy_digest: [0x12_u8; 32],
                public_key: publisher_key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 1_000,
                key_valid_until_ms: 9_000_000_000,
                idempotency_key: IdempotencyKey::from_bytes([0x01_u8; 16]),
                created_at_ms: 1_000,
            })
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("bootstrap publisher principal: {err}"),
            })?
            .binding();

        let z4_operator_key = SigningKey::from_bytes(&[0x72_u8; 32]);
        let z4_operator_binding = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [0x21_u8; 32],
                control_domain_policy_digest: [0x22_u8; 32],
                public_key: z4_operator_key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 1_000,
                key_valid_until_ms: 9_000_000_000,
                idempotency_key: IdempotencyKey::from_bytes([0x02_u8; 16]),
                created_at_ms: 1_000,
            })
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("bootstrap Z4 operator principal: {err}"),
            })?
            .binding();

        let package_id = PackageId::from_bytes([
            0x4e, 0x10, 0x77, 0xc1, 0xa5, 0xb2, 0x4d, 0x8e, 0x9f, 0x01, 0x23, 0x45, 0x67, 0x89,
            0xab, 0xcd,
        ]);

        let baseline_verification = materialize_and_verify_package(
            &identity,
            &artifacts,
            &publisher_key,
            publisher_binding.principal_id,
            package_id,
            pack_package_version(1, 0, 0),
            b"assistant-exec-v1.0.0",
            b"seed-schema-v1.0.0",
            [0x10_u8; 16],
            2_000,
        )?;

        let baseline_installation = applications
            .install_application(
                &artifacts,
                InstallApplicationRequest {
                    package_verification_receipt_id: baseline_verification.receipt_id,
                    idempotency_key: IdempotencyKey::from_bytes([0x11_u8; 16]),
                    installed_at_ms: 2_500,
                },
            )
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("install baseline v1.0.0: {err}"),
            })?
            .receipt();

        seed_initial_user_container(&user_data_container)?;

        Ok(Self {
            root_dir: root_dir.to_path_buf(),
            user_data_container,
            snapshots_dir,
            backups_dir,
            identity,
            artifacts,
            applications,
            package_id,
            publisher_key,
            publisher_binding,
            z4_operator_key,
            z4_operator_binding,
            baseline_verification,
            baseline_installation,
        })
    }

    /// Materializes, signs, and verifies a target package `1.<minor>.0` inside
    /// `ArtifactStore`.
    ///
    /// # Errors
    ///
    /// Returns [`MigrationDrillError::AuthorityFailure`] if verification fails.
    pub fn stage_target_package(
        &self,
        minor_version: u32,
        verify_key_seed: u8,
        verified_at_ms: u64,
    ) -> Result<PackageVerificationReceipt, MigrationDrillError> {
        let exec_bytes = format!("assistant-exec-v1.{minor_version}.0");
        let seed_bytes = format!("seed-schema-v1.{minor_version}.0");
        materialize_and_verify_package(
            &self.identity,
            &self.artifacts,
            &self.publisher_key,
            self.publisher_binding.principal_id,
            self.package_id,
            pack_package_version(1, minor_version, 0),
            exec_bytes.as_bytes(),
            seed_bytes.as_bytes(),
            [verify_key_seed; 16],
            verified_at_ms,
        )
    }

    /// Revokes the `Z4` Trusted-UI operator signing key in `IdentityAuthority`.
    ///
    /// # Errors
    ///
    /// Returns [`MigrationDrillError::AuthorityFailure`] if revocation fails.
    pub fn revoke_z4_operator_key(&self, revoked_at_ms: u64) -> Result<(), MigrationDrillError> {
        self.identity
            .revoke_key(RevokeKeyRequest {
                key_id: self.z4_operator_binding.key_id,
                expected_key_generation: self.z4_operator_binding.key_generation,
                expected_identity_snapshot_id: self.z4_operator_binding.identity_snapshot_id,
                idempotency_key: IdempotencyKey::from_bytes([0xfe_u8; 16]),
                revoked_at_ms,
            })
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("revoke Z4 operator key: {err}"),
            })?;
        Ok(())
    }

    /// Executes a `[PKG-UPDATE-001]` migration plan across `ApplicationAuthority`
    /// and `UserDataContainer`, enforcing pre-flight mode/backup/`Z4` gates and
    /// atomic rollback invariants.
    ///
    /// # Errors
    ///
    /// Fails closed with [`MigrationDrillError`] before any authority or
    /// container write if any safety gate rejects the plan.
    #[allow(clippy::too_many_lines)]
    pub fn run_migration_plan(
        &self,
        plan: &MigrationExecutionPlan,
    ) -> Result<MigrationDrillReceipt, MigrationDrillError> {
        validate_mode_vs_mutation_class(plan.mode, plan.mutation_class)?;

        let current_app = self
            .applications
            .inspect_application(self.package_id)
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("inspect application: {err}"),
            })?
            .ok_or_else(|| MigrationDrillError::AuthorityFailure {
                detail: "baseline application not installed".to_owned(),
            })?;

        let from_generation = current_app.current_installation_generation;
        let from_manifest_digest = current_app.package_manifest_digest;
        let pre_tree_digest = compute_container_tree_digest(&self.user_data_container)?;

        let mut verified_backup_digest: Option<[u8; 32]> = None;
        let mut verified_z4_operator: Option<PrincipalId> = None;

        if plan.mode == MigrationMode::Irreversible {
            let backup = plan
                .pre_upgrade_backup
                .as_ref()
                .ok_or(MigrationDrillError::MissingPreUpgradeBackup)?;
            verify_pre_upgrade_backup(
                backup,
                self.package_id,
                from_manifest_digest,
                pre_tree_digest,
            )?;
            verified_backup_digest = Some(backup.archive_digest);

            let consent = plan
                .z4_consent
                .as_ref()
                .ok_or(MigrationDrillError::MissingZ4TrustedUiConsent)?;
            verify_z4_consent(
                &self.identity,
                consent,
                self.package_id,
                from_manifest_digest,
                plan.target_receipt.manifest_digest,
                backup.archive_digest,
                plan.started_at_ms,
            )?;
            verified_z4_operator = Some(consent.operator_principal);
        }

        let snapshot_receipt = if plan.mode == MigrationMode::SnapshotAssistedRollback {
            let snap_path = self.snapshots_dir.join(format!(
                "snap-{}",
                hex_encode(plan.idempotency_key.as_bytes())
            ));
            Some(capture_container_snapshot(
                &self.user_data_container,
                &snap_path,
                plan.started_at_ms,
            )?)
        } else {
            None
        };

        let declared_steps = plan.declared_steps.max(1);
        self.applications
            .migrate_application(
                &self.artifacts,
                MigrateApplicationRequest {
                    package_id: self.package_id,
                    package_verification_receipt_id: plan.target_receipt.receipt_id,
                    idempotency_key: plan.idempotency_key,
                    compatibility_window: CompatibilityWindow::SameMajor,
                    declared_step_count: declared_steps,
                    requested_at_ms: plan.started_at_ms,
                },
            )
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("migrate_application: {err}"),
            })?;

        let window_staging = self.root_dir.join(format!(
            "window-stage-{}",
            hex_encode(plan.idempotency_key.as_bytes())
        ));
        let mut crashed = false;
        let mut clock_ms = plan.started_at_ms;

        for step_idx in 1..=declared_steps {
            clock_ms += 100;
            if !crashed {
                if plan.crash_at_step == Some(step_idx) {
                    crashed = true;
                    inject_mid_step_crash_mutations(&self.user_data_container, step_idx)?;
                } else {
                    apply_step_container_work(
                        plan.mode,
                        &self.user_data_container,
                        &window_staging,
                        step_idx,
                        declared_steps,
                    )?;
                }
            }
            self.applications
                .record_migration_step(RecordMigrationStepRequest {
                    idempotency_key: plan.idempotency_key,
                    step_index: step_idx,
                    completed_at_ms: clock_ms,
                })
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("record_migration_step({step_idx}): {err}"),
                })?;
        }

        clock_ms += 100;
        let effective_health = !crashed && plan.health_check_passes;
        let probe = StaticVerdictProbe {
            healthy: effective_health,
        };
        let health_report = self
            .applications
            .run_migration_health_check(&self.artifacts, plan.idempotency_key, &probe, clock_ms)
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("run_migration_health_check: {err}"),
            })?;

        let passed = match health_report {
            nlos_application::MigrationHealthDecision::Recorded(rep)
            | nlos_application::MigrationHealthDecision::Replayed(rep) => rep.passed,
        };

        clock_ms += 100;
        if passed {
            if plan.mode == MigrationMode::BackwardCompatibleWindow && window_staging.exists() {
                promote_staged_files(&window_staging, &self.user_data_container)?;
                let _ = fs::remove_dir_all(&window_staging);
            }
            if plan.mode == MigrationMode::Irreversible {
                apply_irreversible_container_compaction(&self.user_data_container)?;
            }

            let decision = self
                .applications
                .activate_package_migration(
                    &self.artifacts,
                    ActivatePackageMigrationRequest {
                        idempotency_key: plan.idempotency_key,
                        activated_at_ms: clock_ms,
                    },
                )
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("activate_package_migration: {err}"),
                })?;
            let installation_receipt = match decision {
                ActivateMigrationDecision::Activated(rec)
                | ActivateMigrationDecision::Replayed(rec) => rec,
            };

            let post_tree_digest = compute_container_tree_digest(&self.user_data_container)?;
            let residual_temp_files = count_residual_temp_files(&self.user_data_container)?;
            let after_app = self
                .applications
                .inspect_application(self.package_id)
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("inspect post-activate application: {err}"),
                })?
                .ok_or_else(|| MigrationDrillError::AuthorityFailure {
                    detail: "application missing after activation".to_owned(),
                })?;

            Ok(MigrationDrillReceipt {
                mode: plan.mode,
                package_id: self.package_id,
                from_generation,
                active_generation_after: after_app.current_installation_generation,
                pre_tree_digest,
                post_tree_digest,
                snapshot_receipt,
                backup_digest: verified_backup_digest,
                z4_operator_principal: verified_z4_operator,
                residual_temp_files,
                byte_identical_on_rollback: false,
                disposition: MigrationDrillDisposition::Activated {
                    installation_receipt,
                },
            })
        } else {
            if window_staging.exists() {
                let _ = fs::remove_dir_all(&window_staging);
            }

            if let Some(snap) = &snapshot_receipt {
                restore_container_from_snapshot(snap, &self.user_data_container)?;
            } else if plan.mode == MigrationMode::Irreversible
                && let Some(backup) = &plan.pre_upgrade_backup
            {
                restore_container_from_backup_archive(backup, &self.user_data_container)?;
            }

            let decision = self
                .applications
                .rollback_package_migration(RollbackPackageMigrationRequest {
                    idempotency_key: plan.idempotency_key,
                    rolled_back_at_ms: clock_ms,
                })
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("rollback_package_migration: {err}"),
                })?;
            let rollback_receipt = match decision {
                RollbackMigrationDecision::RolledBack(rec)
                | RollbackMigrationDecision::Replayed(rec) => rec,
            };

            let post_tree_digest = compute_container_tree_digest(&self.user_data_container)?;
            let residual_temp_files = count_residual_temp_files(&self.user_data_container)?;
            let after_app = self
                .applications
                .inspect_application(self.package_id)
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("inspect post-rollback application: {err}"),
                })?
                .ok_or_else(|| MigrationDrillError::AuthorityFailure {
                    detail: "application missing after rollback".to_owned(),
                })?;

            let byte_identical = post_tree_digest == pre_tree_digest;
            if !byte_identical
                || residual_temp_files != 0
                || after_app.current_installation_generation != from_generation
            {
                return Err(MigrationDrillError::RollbackInvariantViolation {
                    detail: format!(
                        "byte_identical={byte_identical}, residual_temp_files={residual_temp_files}, active_gen={} (expected {})",
                        after_app.current_installation_generation.get(),
                        from_generation.get()
                    ),
                });
            }

            Ok(MigrationDrillReceipt {
                mode: plan.mode,
                package_id: self.package_id,
                from_generation,
                active_generation_after: after_app.current_installation_generation,
                pre_tree_digest,
                post_tree_digest,
                snapshot_receipt,
                backup_digest: verified_backup_digest,
                z4_operator_principal: verified_z4_operator,
                residual_temp_files,
                byte_identical_on_rollback: byte_identical,
                disposition: MigrationDrillDisposition::RolledBackAtomically { rollback_receipt },
            })
        }
    }
}

/// CLI scenario knobs for `nlos-migrate drill`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct CliDrillScenario {
    pub mode: MigrationMode,
    pub inject_health_fail: bool,
    pub inject_step_crash: bool,
    pub omit_backup: bool,
    pub omit_z4_consent: bool,
    pub disguise_destructive: bool,
}

/// Runs a complete `[PKG-UPDATE-001]` drill in an isolated ephemeral temporary
/// directory using real authorities, guaranteeing directory cleanup before return.
///
/// # Errors
///
/// Returns [`MigrationDrillError`] when blocked by a safety gate or if any
/// authority/rollback invariant fails.
pub fn execute_ephemeral_cli_drill(
    scenario: CliDrillScenario,
) -> Result<MigrationDrillReceipt, MigrationDrillError> {
    let ephemeral = EphemeralDirGuard::new("nlos-migrate-drill")?;
    let harness = MigrationSandboxHarness::bootstrap(ephemeral.path())?;
    let target_receipt = harness.stage_target_package(1, 0x44, 5_000)?;

    let mutation_class = if scenario.disguise_destructive {
        ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite
    } else {
        match scenario.mode {
            MigrationMode::BackwardCompatibleWindow => ContainerMutationClass::AdditiveDualReadable,
            MigrationMode::SnapshotAssistedRollback => {
                ContainerMutationClass::SchemaTransformRequiringSnapshot
            }
            MigrationMode::Irreversible => {
                ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite
            }
        }
    };

    let pre_upgrade_backup =
        if scenario.mode == MigrationMode::Irreversible && !scenario.omit_backup {
            let backup_path = harness.backups_dir.join("pre-upgrade-v1.nlosbak");
            Some(create_pre_upgrade_backup(
                &harness.user_data_container,
                &backup_path,
                harness.package_id,
                harness.baseline_installation.package_manifest_digest,
                5_200,
            )?)
        } else {
            None
        };

    let z4_consent = if scenario.mode == MigrationMode::Irreversible && !scenario.omit_z4_consent {
        let backup_digest = pre_upgrade_backup
            .as_ref()
            .map_or([0_u8; 32], |bak| bak.archive_digest);
        Some(sign_z4_irreversible_consent(
            &harness.z4_operator_key,
            harness.z4_operator_binding.principal_id,
            harness.package_id,
            harness.baseline_installation.package_manifest_digest,
            target_receipt.manifest_digest,
            backup_digest,
            5_500,
        ))
    } else {
        None
    };

    let plan = MigrationExecutionPlan {
        mode: scenario.mode,
        mutation_class,
        target_receipt,
        idempotency_key: IdempotencyKey::from_bytes([0x55_u8; 16]),
        declared_steps: 2,
        crash_at_step: if scenario.inject_step_crash {
            Some(2)
        } else {
            None
        },
        health_check_passes: !scenario.inject_health_fail,
        pre_upgrade_backup,
        z4_consent,
        started_at_ms: 6_000,
    };

    harness.run_migration_plan(&plan)
}

struct EphemeralDirGuard {
    path: PathBuf,
}

impl EphemeralDirGuard {
    fn new(prefix: &str) -> Result<Self, MigrationDrillError> {
        let seq = EPHEMERAL_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("{prefix}-{pid}-{seq}"));
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        fs::create_dir_all(&dir).map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("create ephemeral dir {}: {err}", dir.display()),
        })?;
        Ok(Self { path: dir })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EphemeralDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct StaticVerdictProbe {
    healthy: bool,
}

impl MigrationHealthProbe for StaticVerdictProbe {
    fn target_revision_healthy(&self, _context: &MigrationHealthContext<'_>) -> bool {
        self.healthy
    }
}

fn validate_mode_vs_mutation_class(
    mode: MigrationMode,
    mutation_class: ContainerMutationClass,
) -> Result<(), MigrationDrillError> {
    if mutation_class == ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite
        && mode != MigrationMode::Irreversible
    {
        return Err(MigrationDrillError::DisguisedIrreversibleMigration {
            declared_mode: mode,
            mutation_class,
            detail: "destructive field deletion or irreversible compaction cannot declare BACKWARD_COMPATIBLE_WINDOW or SNAPSHOT_ASSISTED_ROLLBACK (`RISK-E-05`)".to_owned(),
        });
    }
    if mutation_class == ContainerMutationClass::SchemaTransformRequiringSnapshot
        && mode == MigrationMode::BackwardCompatibleWindow
    {
        return Err(MigrationDrillError::IncompatibleMutationClass {
            declared_mode: mode,
            mutation_class,
            detail: "in-place v2 schema rewrite breaks backward-compatible window; declare SNAPSHOT_ASSISTED_ROLLBACK".to_owned(),
        });
    }
    Ok(())
}

fn verify_pre_upgrade_backup(
    backup: &PreUpgradeBackupReceipt,
    expected_package_id: PackageId,
    expected_from_manifest: ContentDigest,
    current_tree_digest: [u8; 32],
) -> Result<(), MigrationDrillError> {
    if backup.package_id != expected_package_id {
        return Err(MigrationDrillError::InvalidPreUpgradeBackup {
            detail: "backup receipt package_id does not match target application".to_owned(),
        });
    }
    if backup.from_manifest_digest != expected_from_manifest {
        return Err(MigrationDrillError::InvalidPreUpgradeBackup {
            detail: "backup receipt from_manifest_digest does not match active baseline generation"
                .to_owned(),
        });
    }
    if backup.source_tree_digest != current_tree_digest {
        return Err(MigrationDrillError::InvalidPreUpgradeBackup {
            detail: "backup source_tree_digest does not match current UserDataContainer state"
                .to_owned(),
        });
    }

    let bytes = fs::read(&backup.backup_archive_path).map_err(|err| {
        MigrationDrillError::InvalidPreUpgradeBackup {
            detail: format!(
                "unable to read backup archive {}: {err}",
                backup.backup_archive_path.display()
            ),
        }
    })?;
    let actual_digest: [u8; 32] = Sha256::digest(&bytes).into();
    if actual_digest != backup.archive_digest {
        return Err(MigrationDrillError::InvalidPreUpgradeBackup {
            detail: format!(
                "backup archive SHA-256 mismatch (expected {}, actual {})",
                hex_encode(&backup.archive_digest),
                hex_encode(&actual_digest)
            ),
        });
    }

    let min_header = BACKUP_ARCHIVE_MAGIC.len() + 16 + 32 + 32 + 8;
    if bytes.len() < min_header || !bytes.starts_with(BACKUP_ARCHIVE_MAGIC) {
        return Err(MigrationDrillError::InvalidPreUpgradeBackup {
            detail: "backup archive magic header is missing or truncated".to_owned(),
        });
    }
    let pkg_offset = BACKUP_ARCHIVE_MAGIC.len();
    if bytes[pkg_offset..pkg_offset + 16] != *expected_package_id.as_bytes() {
        return Err(MigrationDrillError::InvalidPreUpgradeBackup {
            detail: "backup archive header package_id mismatch".to_owned(),
        });
    }
    let manifest_offset = pkg_offset + 16;
    if bytes[manifest_offset..manifest_offset + 32] != *expected_from_manifest.as_bytes() {
        return Err(MigrationDrillError::InvalidPreUpgradeBackup {
            detail: "backup archive header from_manifest_digest mismatch".to_owned(),
        });
    }
    Ok(())
}

fn verify_z4_consent(
    identity: &IdentityAuthority,
    consent: &Z4TrustedUiConsentReceipt,
    expected_package_id: PackageId,
    expected_from_manifest: ContentDigest,
    expected_target_manifest: ContentDigest,
    expected_backup_digest: [u8; 32],
    verified_at_ms: u64,
) -> Result<(), MigrationDrillError> {
    if consent.mode_tag != MigrationMode::Irreversible.as_str() {
        return Err(MigrationDrillError::InvalidZ4TrustedUiConsent {
            detail: format!(
                "expected mode_tag=\"IRREVERSIBLE\", got {:?}",
                consent.mode_tag
            ),
        });
    }
    if consent.package_id != expected_package_id {
        return Err(MigrationDrillError::InvalidZ4TrustedUiConsent {
            detail: "Z4 consent package_id does not match target package".to_owned(),
        });
    }
    if consent.from_manifest_digest != expected_from_manifest {
        return Err(MigrationDrillError::InvalidZ4TrustedUiConsent {
            detail: "Z4 consent from_manifest_digest does not match active baseline generation"
                .to_owned(),
        });
    }
    if consent.target_manifest_digest != expected_target_manifest {
        return Err(MigrationDrillError::InvalidZ4TrustedUiConsent {
            detail: "Z4 consent target_manifest_digest does not match target package revision"
                .to_owned(),
        });
    }
    if consent.backup_archive_digest != expected_backup_digest {
        return Err(MigrationDrillError::InvalidZ4TrustedUiConsent {
            detail:
                "Z4 consent backup_archive_digest does not match verified PreUpgradeBackupReceipt"
                    .to_owned(),
        });
    }

    let expected_digest = z4_irreversible_consent_digest(
        expected_package_id,
        expected_from_manifest,
        expected_target_manifest,
        expected_backup_digest,
    );
    if consent.message_digest != expected_digest {
        return Err(MigrationDrillError::InvalidZ4TrustedUiConsent {
            detail: "Z4 consent preimage message_digest mismatch".to_owned(),
        });
    }

    identity
        .verify_capability_command_signature(VerifyCapabilityCommandSignatureRequest {
            message_digest: expected_digest,
            principal: consent.operator_principal,
            signature: consent.signature,
            verified_at_ms,
        })
        .map_err(|err| MigrationDrillError::InvalidZ4TrustedUiConsent {
            detail: format!("IdentityAuthority signature verification failed: {err}"),
        })?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn materialize_and_verify_package(
    identity: &IdentityAuthority,
    artifacts: &ArtifactStore,
    signing_key: &SigningKey,
    signer: PrincipalId,
    package_id: PackageId,
    version: u64,
    exec_payload: &[u8],
    seed_payload: &[u8],
    verify_idempotency_bytes: [u8; 16],
    at_ms: u64,
) -> Result<PackageVerificationReceipt, MigrationDrillError> {
    let entries_spec = [
        ("assistant", PackageEntryRole::Executable, exec_payload),
        ("seed-template", PackageEntryRole::Data, seed_payload),
    ];
    let mut entries = Vec::with_capacity(entries_spec.len());

    for (idx, (name, role, payload)) in entries_spec.into_iter().enumerate() {
        let raw_artifact_id = derive_artifact_id(package_id, version, name);
        let artifact_id = nlos_types::ArtifactId::from_bytes(raw_artifact_id);
        let digest = ContentDigest::of_bytes(payload);
        let mut create_key = verify_idempotency_bytes;
        create_key[15] ^= u8::try_from(idx + 1).unwrap_or(1);

        artifacts
            .create_artifact(CreateArtifactSpec {
                artifact_id,
                idempotency_key: IdempotencyKey::from_bytes(create_key),
                content_type: "application/octet-stream".to_owned(),
                application_id: None,
                owner: None,
                created_at_ms: at_ms,
            })
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("create_artifact({name}): {err}"),
            })?;

        artifacts
            .put_revision(PutRevisionRequest {
                artifact_id,
                expected_head_revision: 0,
                bytes: payload,
                created_at_ms: at_ms,
                provenance: ProvenanceSourceTriple {
                    source_a: raw_artifact_id,
                    source_b: *package_id.as_bytes(),
                    source_digest: digest,
                },
            })
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("put_revision({name}): {err}"),
            })?;

        entries.push(PackageManifestEntry {
            name: name.to_owned(),
            artifact_id,
            digest,
            role,
        });
    }

    let manifest = PackageManifest {
        package_id,
        version,
        entries,
    };
    let msg_digest = package_manifest_message(&manifest);
    let signature = signing_key.sign(&msg_digest).to_bytes();
    let signed_package = SignedPackage {
        manifest,
        signer,
        signature,
    };

    let decision = artifacts
        .verify_package(
            identity,
            VerifyPackageRequest {
                signed: &signed_package,
                idempotency_key: IdempotencyKey::from_bytes(verify_idempotency_bytes),
                verified_at_ms: at_ms,
            },
        )
        .map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("verify_package: {err}"),
        })?;

    Ok(decision.receipt().clone())
}

fn seed_initial_user_container(container_dir: &Path) -> Result<(), MigrationDrillError> {
    for sub in ["profile", "notes", "state"] {
        fs::create_dir_all(container_dir.join(sub)).map_err(|err| {
            MigrationDrillError::AuthorityFailure {
                detail: format!("create container subdir {sub}: {err}"),
            }
        })?;
    }
    fs::write(
        container_dir.join("state/schema-version.txt"),
        b"schema=v1\ncompat=v1\n",
    )
    .map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("seed schema-version.txt: {err}"),
    })?;
    fs::write(
        container_dir.join("profile/preferences.json"),
        b"{\"theme\":\"light\",\"legacy_v1_token\":\"keep-in-v1\"}\n",
    )
    .map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("seed preferences.json: {err}"),
    })?;
    fs::write(
        container_dir.join("notes/journal-001.txt"),
        b"baseline user note preserved across atomic rollbacks\n",
    )
    .map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("seed journal-001.txt: {err}"),
    })?;
    Ok(())
}

fn apply_step_container_work(
    mode: MigrationMode,
    user_data_container: &Path,
    window_staging: &Path,
    step_idx: u64,
    declared_steps: u64,
) -> Result<(), MigrationDrillError> {
    match mode {
        MigrationMode::BackwardCompatibleWindow => {
            fs::create_dir_all(window_staging.join("state")).map_err(|err| {
                MigrationDrillError::AuthorityFailure {
                    detail: format!("create window staging: {err}"),
                }
            })?;
            fs::write(
                window_staging.join(format!("state/dual-window-step-{step_idx}.idx")),
                format!("additive-window-index-step-{step_idx}-of-{declared_steps}\n"),
            )
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("write window staging index: {err}"),
            })?;
        }
        MigrationMode::SnapshotAssistedRollback => {
            if step_idx == 1 {
                fs::write(
                    user_data_container.join("state/schema-version.txt"),
                    b"schema=v2-migrating\n",
                )
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("write v2 migrating marker: {err}"),
                })?;
                fs::write(
                    user_data_container.join("profile/preferences.json"),
                    b"{\"theme\":\"light\",\"v2_layout\":true}\n",
                )
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("rewrite v2 preferences: {err}"),
                })?;
            } else {
                fs::write(
                    user_data_container.join("state/schema-version.txt"),
                    b"schema=v2\ncompat=snapshot-assisted\n",
                )
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("finalize v2 schema-version.txt: {err}"),
                })?;
                fs::write(
                    user_data_container.join("notes/v2-summary.idx"),
                    b"materialized-v2-index\n",
                )
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("write v2-summary.idx: {err}"),
                })?;
            }
        }
        MigrationMode::Irreversible => {
            fs::write(
                user_data_container.join("state/schema-version.txt"),
                format!("schema=v2-irreversible-step-{step_idx}\n"),
            )
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("write irreversible step state: {err}"),
            })?;
        }
    }
    Ok(())
}

fn inject_mid_step_crash_mutations(
    user_data_container: &Path,
    step_idx: u64,
) -> Result<(), MigrationDrillError> {
    fs::write(
        user_data_container.join("state/schema-version.txt"),
        format!("schema=v2-CORRUPTED-AT-STEP-{step_idx}\n"),
    )
    .map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("inject crash schema mutation: {err}"),
    })?;
    fs::write(
        user_data_container.join("notes/inflight-step.tmp"),
        b"HALF-WRITTEN-UNFLUSHED-TEMP-RECORD",
    )
    .map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("inject crash temp file: {err}"),
    })?;
    Ok(())
}

fn promote_staged_files(staging: &Path, target: &Path) -> Result<(), MigrationDrillError> {
    let staged_files = collect_container_files(staging)?;
    for (rel_path, bytes) in &staged_files {
        let dest = target.join(rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("create promoted parent {}: {err}", parent.display()),
            })?;
        }
        fs::write(&dest, bytes).map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("promote file {}: {err}", dest.display()),
        })?;
    }
    Ok(())
}

fn apply_irreversible_container_compaction(
    user_data_container: &Path,
) -> Result<(), MigrationDrillError> {
    fs::write(
        user_data_container.join("state/schema-version.txt"),
        b"schema=v2-irreversible-final\nlegacy_v1_support=removed\n",
    )
    .map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("finalize irreversible schema: {err}"),
    })?;
    fs::write(
        user_data_container.join("profile/preferences.json"),
        b"{\"theme\":\"light\",\"v2_compacted\":true}\n",
    )
    .map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("compact irreversible preferences: {err}"),
    })?;
    Ok(())
}

fn restore_container_from_backup_archive(
    backup: &PreUpgradeBackupReceipt,
    user_data_container: &Path,
) -> Result<(), MigrationDrillError> {
    let bytes = fs::read(&backup.backup_archive_path).map_err(|err| {
        MigrationDrillError::AuthorityFailure {
            detail: format!("read backup archive for restore: {err}"),
        }
    })?;
    let mut cursor = BACKUP_ARCHIVE_MAGIC.len() + 16 + 32 + 32;
    if cursor + 8 > bytes.len() {
        return Err(MigrationDrillError::InvalidPreUpgradeBackup {
            detail: "truncated backup archive file count".to_owned(),
        });
    }
    let count = u64::from_be_bytes(bytes[cursor..cursor + 8].try_into().unwrap_or([0_u8; 8]));
    cursor += 8;

    clear_directory_contents(user_data_container)?;
    for _ in 0..count {
        if cursor + 4 > bytes.len() {
            return Err(MigrationDrillError::InvalidPreUpgradeBackup {
                detail: "truncated backup entry path length".to_owned(),
            });
        }
        let path_len = usize::try_from(u32::from_be_bytes(
            bytes[cursor..cursor + 4].try_into().unwrap_or([0_u8; 4]),
        ))
        .unwrap_or(0);
        cursor += 4;
        if cursor + path_len + 8 > bytes.len() {
            return Err(MigrationDrillError::InvalidPreUpgradeBackup {
                detail: "truncated backup entry header".to_owned(),
            });
        }
        let rel_path = std::str::from_utf8(&bytes[cursor..cursor + path_len]).map_err(|_| {
            MigrationDrillError::InvalidPreUpgradeBackup {
                detail: "non-UTF8 path in backup archive".to_owned(),
            }
        })?;
        cursor += path_len;
        let data_len = usize::try_from(u64::from_be_bytes(
            bytes[cursor..cursor + 8].try_into().unwrap_or([0_u8; 8]),
        ))
        .unwrap_or(0);
        cursor += 8;
        if cursor + data_len > bytes.len() {
            return Err(MigrationDrillError::InvalidPreUpgradeBackup {
                detail: "truncated backup entry payload".to_owned(),
            });
        }
        let dest = user_data_container.join(rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("create backup restore parent: {err}"),
            })?;
        }
        fs::write(&dest, &bytes[cursor..cursor + data_len]).map_err(|err| {
            MigrationDrillError::AuthorityFailure {
                detail: format!("restore backup entry {rel_path}: {err}"),
            }
        })?;
        cursor += data_len;
    }
    Ok(())
}

fn clear_directory_contents(dir: &Path) -> Result<(), MigrationDrillError> {
    if dir.exists() {
        for entry_res in fs::read_dir(dir).map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("read_dir {}: {err}", dir.display()),
        })? {
            let entry = entry_res.map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("dir entry {}: {err}", dir.display()),
            })?;
            let path = entry.path();
            let ft = entry
                .file_type()
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("file_type {}: {err}", path.display()),
                })?;
            if ft.is_dir() {
                fs::remove_dir_all(&path).map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("remove_dir_all {}: {err}", path.display()),
                })?;
            } else {
                fs::remove_file(&path).map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: format!("remove_file {}: {err}", path.display()),
                })?;
            }
        }
    } else {
        fs::create_dir_all(dir).map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("create_dir_all {}: {err}", dir.display()),
        })?;
    }
    Ok(())
}

fn collect_container_files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, MigrationDrillError> {
    let mut map = BTreeMap::new();
    if !root.exists() {
        return Ok(map);
    }
    collect_container_files_rec(root, root, &mut map)?;
    Ok(map)
}

fn collect_container_files_rec(
    root: &Path,
    current: &Path,
    map: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), MigrationDrillError> {
    for entry_res in fs::read_dir(current).map_err(|err| MigrationDrillError::AuthorityFailure {
        detail: format!("read_dir {}: {err}", current.display()),
    })? {
        let entry = entry_res.map_err(|err| MigrationDrillError::AuthorityFailure {
            detail: format!("dir entry {}: {err}", current.display()),
        })?;
        let path = entry.path();
        let ft = entry
            .file_type()
            .map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("file_type {}: {err}", path.display()),
            })?;
        if ft.is_dir() {
            collect_container_files_rec(root, &path, map)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|err| MigrationDrillError::AuthorityFailure {
                    detail: err.to_string(),
                })?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path).map_err(|err| MigrationDrillError::AuthorityFailure {
                detail: format!("read {}: {err}", path.display()),
            })?;
            map.insert(rel, bytes);
        }
    }
    Ok(())
}
