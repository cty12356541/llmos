//! Gate 3: Transactional update, post-migration health probe, and zero-drift
//! rollback drill (`[PKG-UPDATE-001]` & `[DRV-HEALTH-001]`, `ADR-0027` §3.3).
//!
//! Drives real `IdentityAuthority`, `ArtifactStore`, `ApplicationAuthority`, and
//! `nlos-migrate` `UserDataContainer` snapshots inside an isolated temporary
//! sandbox across three mandatory sub-drills:
//! 1. Healthy `v1 -> v2` upgrade path -> verifies atomic switch to generation `2`.
//! 2. `HealthCheckFailed` fault injection -> verifies active generation and
//!    manifest digest remain at baseline, `UserDataContainer` SHA-256 tree
//!    digest is bitwise identical to pre-upgrade snapshot, and residual temp
//!    files equal `0`.
//! 3. `MidStepFault` crash injection -> verifies half-written state and `.tmp`
//!    artifacts roll back cleanly with bitwise identical `UserDataContainer`
//!    SHA-256 tree digest and `0` residual temp files.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{
    ActivateMigrationDecision, ActivatePackageMigrationRequest, ApplicationAuthority,
    ApplicationStatus, CompatibilityWindow, InstallApplicationRequest, MigrateApplicationRequest,
    MigrationHealthContext, MigrationHealthDecision, MigrationHealthProbe,
    RecordMigrationStepRequest, RollbackMigrationDecision, RollbackPackageMigrationRequest,
};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
    PackageManifestEntry, PackageVerificationReceipt, ProvenanceSourceTriple, PutRevisionRequest,
    SignedPackage, VerifyPackageRequest, derive_artifact_id, package_manifest_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_migrate::{
    ComponentRole, DeclaredMigrationMode, MigrationMode, capture_container_snapshot,
    compute_container_tree_digest, count_residual_temp_files, restore_container_from_snapshot,
};
use nlos_types::{Generation, IdempotencyKey, PackageId, PrincipalId};

use crate::model::{
    CertifyError, CertifyFinding, CertifySeverity, GateExecutionSummary, GateId,
    LoadedCertifyPackage,
};

static GATE3_SANDBOX_SEQ: AtomicU64 = AtomicU64::new(1);

/// Fault-injection / testing knobs for Gate 3.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gate3DrillOptions {
    /// Simulates a faulty rollback handler leaving drifted bytes or `.tmp`
    /// residue in `UserDataContainer` after a failed health probe, proving
    /// Gate 3 fails closed on any container drift.
    pub simulate_unclean_rollback_drift: bool,
}

/// Detailed execution output of Gate 3.
#[derive(Clone, Debug)]
pub struct Gate3Outcome {
    /// Effective migration mode exercised during the drill.
    pub effective_mode: Option<MigrationMode>,
    /// Activated generation observed after the healthy `v1 -> v2` sub-drill.
    pub healthy_activated_generation: Option<u64>,
    /// Baseline SHA-256 tree digest of `UserDataContainer` before rollback drills.
    pub baseline_container_tree_digest: Option<[u8; 32]>,
    /// Post-rollback SHA-256 tree digest after `HealthCheckFailed` drill.
    pub health_fail_restored_tree_digest: Option<[u8; 32]>,
    /// Post-rollback SHA-256 tree digest after `MidStepFault` drill.
    pub mid_step_fault_restored_tree_digest: Option<[u8; 32]>,
    /// Structured findings emitted by Gate 3.
    pub findings: Vec<CertifyFinding>,
    /// Cryptographically committed summary for receipt inclusion.
    pub summary: GateExecutionSummary,
}

/// Runs Gate 3 transactional update & fault-injected rollback drills using
/// default certification options.
///
/// # Errors
///
/// Returns [`CertifyError`] only on unexpected host temp-directory I/O errors.
/// Migration mode, health-check, or rollback invariant violations are recorded
/// as `BLOCKER` [`CertifyFinding`]s inside [`Gate3Outcome`].
pub fn run_gate3_update_rollback_drill(
    baseline_pkg: &LoadedCertifyPackage,
    upgrade_pkg: Option<&LoadedCertifyPackage>,
) -> Result<Gate3Outcome, CertifyError> {
    run_gate3_update_rollback_drill_with_options(
        baseline_pkg,
        upgrade_pkg,
        Gate3DrillOptions::default(),
    )
}

/// Runs Gate 3 with explicit [`Gate3DrillOptions`].
///
/// # Errors
///
/// Returns [`CertifyError`] on unexpected host temp-directory I/O failures.
#[allow(clippy::too_many_lines)]
pub fn run_gate3_update_rollback_drill_with_options(
    baseline_pkg: &LoadedCertifyPackage,
    upgrade_pkg: Option<&LoadedCertifyPackage>,
    options: Gate3DrillOptions,
) -> Result<Gate3Outcome, CertifyError> {
    let mut findings = Vec::new();

    // 1. Validate declared migration-mode on baseline and optional upgrade package.
    let baseline_mode = validate_package_migration_mode(baseline_pkg, "baseline", &mut findings);
    let upgrade_mode = upgrade_pkg.and_then(|up| {
        if up.package_id != baseline_pkg.package_id {
            findings.push(CertifyFinding {
                gate: GateId::Gate3TransactionalUpdateAndRollback,
                rule_id: "PKG-UPDATE-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: "upgrade/package.manifest".to_owned(),
                message: format!(
                    "upgrade package-id '{}' does not match baseline package-id '{}'",
                    up.package_id_label, baseline_pkg.package_id_label
                ),
                remediation: "Ensure upgrade package targets the identical package-id.".to_owned(),
            });
        }
        if up.version_ordinal <= baseline_pkg.version_ordinal {
            findings.push(CertifyFinding {
                gate: GateId::Gate3TransactionalUpdateAndRollback,
                rule_id: "PKG-UPDATE-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: "upgrade/package.manifest".to_owned(),
                message: format!(
                    "upgrade version '{}' is not strictly greater than baseline version '{}'",
                    up.base_manifest.version_raw, baseline_pkg.base_manifest.version_raw
                ),
                remediation: "Increment SemVer minor or patch version on upgrade package."
                    .to_owned(),
            });
        }
        if let Err(err) = CompatibilityWindow::SameMajor.validate(
            baseline_pkg.package_id,
            baseline_pkg.version_ordinal,
            up.version_ordinal,
        ) {
            findings.push(CertifyFinding {
                gate: GateId::Gate3TransactionalUpdateAndRollback,
                rule_id: "PKG-UPDATE-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: "upgrade/package.manifest".to_owned(),
                message: format!("upgrade package violates SameMajor compatibility window: {err}"),
                remediation:
                    "Keep same-major SemVer lineage for automated in-place package updates."
                        .to_owned(),
            });
        }
        validate_package_migration_mode(up, "upgrade", &mut findings)
    });

    let effective_mode = upgrade_mode.or(baseline_mode);
    if !findings.is_empty() {
        let summary = GateExecutionSummary::from_findings(
            GateId::Gate3TransactionalUpdateAndRollback,
            &findings,
            b"pre-flight-rejected",
            format!("Gate 3 pre-flight blocked: {} blocker(s)", findings.len()),
        );
        return Ok(Gate3Outcome {
            effective_mode,
            healthy_activated_generation: None,
            baseline_container_tree_digest: None,
            health_fail_restored_tree_digest: None,
            mid_step_fault_restored_tree_digest: None,
            findings,
            summary,
        });
    }

    let mode = effective_mode.unwrap_or(MigrationMode::SnapshotAssistedRollback);
    let v1_entries = load_real_package_entries(baseline_pkg)?;
    let (v2_version, v2_entries) = match upgrade_pkg {
        Some(up) => (up.version_ordinal, load_real_package_entries(up)?),
        None => (
            baseline_pkg.version_ordinal.saturating_add(1),
            synthesize_v2_entries(&v1_entries),
        ),
    };

    // Sub-drill A: Healthy `v1 -> v2` upgrade -> atomic switch to generation 2.
    let healthy_gen = run_healthy_upgrade_subdrill(
        baseline_pkg.package_id,
        baseline_pkg.version_ordinal,
        &v1_entries,
        v2_version,
        &v2_entries,
        mode,
        &mut findings,
    )?;

    // Sub-drill B: Post-upgrade `HealthCheckFailed` fault injection -> atomic rollback.
    let (pre_digest_b, post_digest_b) = run_fault_injected_rollback_subdrill(
        "health-check-failed",
        baseline_pkg.package_id,
        baseline_pkg.version_ordinal,
        &v1_entries,
        v2_version,
        &v2_entries,
        mode,
        false,
        options.simulate_unclean_rollback_drift,
        &mut findings,
    )?;

    // Sub-drill C: `MidStepFault` crash injection -> atomic rollback & zero temp residue.
    let (_pre_digest_c, post_digest_c) = run_fault_injected_rollback_subdrill(
        "mid-step-fault",
        baseline_pkg.package_id,
        baseline_pkg.version_ordinal,
        &v1_entries,
        v2_version,
        &v2_entries,
        mode,
        true,
        options.simulate_unclean_rollback_drift,
        &mut findings,
    )?;

    let mut extra = Vec::new();
    extra.extend_from_slice(mode.as_str().as_bytes());
    if let Some(gen_val) = healthy_gen {
        extra.extend_from_slice(&gen_val.to_be_bytes());
    }
    if let Some(d) = pre_digest_b {
        extra.extend_from_slice(&d);
    }
    if let Some(d) = post_digest_b {
        extra.extend_from_slice(&d);
    }
    if let Some(d) = post_digest_c {
        extra.extend_from_slice(&d);
    }

    let blocker_count = findings
        .iter()
        .filter(|f| f.severity == CertifySeverity::Blocker)
        .count();
    let summary_line = format!(
        "Gate 3 (mode={}, v1->v2 gen={}, rollback drills=2): {} blocker(s)",
        mode.as_str(),
        healthy_gen.unwrap_or(0),
        blocker_count
    );
    let summary = GateExecutionSummary::from_findings(
        GateId::Gate3TransactionalUpdateAndRollback,
        &findings,
        &extra,
        summary_line,
    );

    Ok(Gate3Outcome {
        effective_mode: Some(mode),
        healthy_activated_generation: healthy_gen,
        baseline_container_tree_digest: pre_digest_b,
        health_fail_restored_tree_digest: post_digest_b,
        mid_step_fault_restored_tree_digest: post_digest_c,
        findings,
        summary,
    })
}

fn validate_package_migration_mode(
    pkg: &LoadedCertifyPackage,
    label: &str,
    findings: &mut Vec<CertifyFinding>,
) -> Option<MigrationMode> {
    match pkg.base_manifest.migration_mode {
        None => Some(MigrationMode::SnapshotAssistedRollback),
        Some(DeclaredMigrationMode::BackwardCompatibleWindow) => {
            Some(MigrationMode::BackwardCompatibleWindow)
        }
        Some(DeclaredMigrationMode::SnapshotAssistedRollback) => {
            Some(MigrationMode::SnapshotAssistedRollback)
        }
        Some(DeclaredMigrationMode::Irreversible) => {
            findings.push(CertifyFinding {
                gate: GateId::Gate3TransactionalUpdateAndRollback,
                rule_id: "PKG-UPDATE-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("{label}/package.manifest"),
                message:
                    "automated store certification forbids migration-mode=IRREVERSIBLE because unattended store updates must guarantee zero-data-loss rollback"
                        .to_owned(),
                remediation:
                    "Declare BACKWARD_COMPATIBLE_WINDOW or SNAPSHOT_ASSISTED_ROLLBACK for store-certified package updates."
                        .to_owned(),
            });
            None
        }
    }
}

pub(crate) struct RawEntrySpec {
    pub(crate) name: String,
    pub(crate) role: PackageEntryRole,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) fn load_real_package_entries(
    pkg: &LoadedCertifyPackage,
) -> Result<Vec<RawEntrySpec>, CertifyError> {
    let mut specs = Vec::new();
    for comp in &pkg.base_manifest.components {
        let full_path = pkg.package_dir.join(&comp.relative_path);
        let bytes = if full_path.is_file() {
            fs::read(&full_path).map_err(|source| CertifyError::Io {
                context: format!("read component {}", full_path.display()),
                source,
            })?
        } else {
            format!("missing-component:{}", comp.name).into_bytes()
        };
        let role = match comp.role {
            ComponentRole::Executable
            | ComponentRole::Driver
            | ComponentRole::BackgroundService
            | ComponentRole::UiSurface => PackageEntryRole::Executable,
            ComponentRole::Data | ComponentRole::SeedTemplate => PackageEntryRole::Data,
        };
        specs.push(RawEntrySpec {
            name: comp.name.clone(),
            role,
            bytes,
        });
    }
    if specs.is_empty() {
        specs.push(RawEntrySpec {
            name: "manifest-anchor".to_owned(),
            role: PackageEntryRole::Data,
            bytes: pkg.filtered_base_manifest_text.as_bytes().to_vec(),
        });
    }
    Ok(specs)
}

fn synthesize_v2_entries(v1: &[RawEntrySpec]) -> Vec<RawEntrySpec> {
    v1.iter()
        .map(|entry| {
            let mut upgraded = entry.bytes.clone();
            upgraded.extend_from_slice(b"\n# certify-gate3-synthesized-v2-upgrade\n");
            RawEntrySpec {
                name: entry.name.clone(),
                role: entry.role,
                bytes: upgraded,
            }
        })
        .collect()
}

pub(crate) struct EphemeralSandboxDir {
    path: PathBuf,
}

impl EphemeralSandboxDir {
    pub(crate) fn new(prefix: &str) -> Result<Self, CertifyError> {
        let seq = GATE3_SANDBOX_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("nlos-certify-{prefix}-{pid}-{seq}"));
        if path.exists() {
            let _ = fs::remove_dir_all(&path);
        }
        fs::create_dir_all(&path).map_err(|source| CertifyError::Io {
            context: format!("create sandbox {}", path.display()),
            source,
        })?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EphemeralSandboxDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct StaticHealthProbe {
    healthy: bool,
}

impl MigrationHealthProbe for StaticHealthProbe {
    fn target_revision_healthy(&self, _context: &MigrationHealthContext<'_>) -> bool {
        self.healthy
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize_verified_package(
    identity: &IdentityAuthority,
    artifacts: &ArtifactStore,
    signing_key: &SigningKey,
    signer: PrincipalId,
    package_id: PackageId,
    version: u64,
    entries_spec: &[RawEntrySpec],
    key_seed: u8,
    at_ms: u64,
) -> Result<PackageVerificationReceipt, CertifyError> {
    let mut entries = Vec::with_capacity(entries_spec.len());
    for (idx, item) in entries_spec.iter().enumerate() {
        let raw_artifact_id = derive_artifact_id(package_id, version, &item.name);
        let artifact_id = nlos_types::ArtifactId::from_bytes(raw_artifact_id);
        let digest = ContentDigest::of_bytes(&item.bytes);
        let mut create_key = [key_seed; 16];
        create_key[14] = u8::try_from((idx >> 8) & 0xff).unwrap_or(0);
        create_key[15] ^= u8::try_from((idx + 1) & 0xff).unwrap_or(1);

        artifacts
            .create_artifact(CreateArtifactSpec {
                artifact_id,
                idempotency_key: IdempotencyKey::from_bytes(create_key),
                content_type: "application/octet-stream".to_owned(),
                application_id: None,
                owner: None,
                created_at_ms: at_ms,
            })
            .map_err(|err| {
                CertifyError::Authority(format!("create_artifact({}): {err}", item.name))
            })?;

        artifacts
            .put_revision(PutRevisionRequest {
                artifact_id,
                expected_head_revision: 0,
                bytes: &item.bytes,
                created_at_ms: at_ms,
                provenance: ProvenanceSourceTriple {
                    source_a: raw_artifact_id,
                    source_b: *package_id.as_bytes(),
                    source_digest: digest,
                },
            })
            .map_err(|err| {
                CertifyError::Authority(format!("put_revision({}): {err}", item.name))
            })?;

        entries.push(PackageManifestEntry {
            name: item.name.clone(),
            artifact_id,
            digest,
            role: item.role,
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
                idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
                verified_at_ms: at_ms,
            },
        )
        .map_err(|err| CertifyError::Authority(format!("verify_package: {err}")))?;

    Ok(decision.receipt().clone())
}

fn seed_user_data_container(container_dir: &Path) -> Result<(), CertifyError> {
    for sub in ["profile", "notes", "state"] {
        let dir = container_dir.join(sub);
        fs::create_dir_all(&dir).map_err(|source| CertifyError::Io {
            context: format!("create {}", dir.display()),
            source,
        })?;
    }
    fs::write(
        container_dir.join("state/schema-version.txt"),
        b"schema=v1\ngeneration=1\n",
    )
    .map_err(|source| CertifyError::Io {
        context: "seed schema-version.txt".to_owned(),
        source,
    })?;
    fs::write(
        container_dir.join("profile/user-settings.json"),
        b"{\"theme\":\"system\",\"notes_count\":2}\n",
    )
    .map_err(|source| CertifyError::Io {
        context: "seed user-settings.json".to_owned(),
        source,
    })?;
    fs::write(
        container_dir.join("notes/important-user-data.txt"),
        b"canonical user data that must survive updates and rollbacks without drift\n",
    )
    .map_err(|source| CertifyError::Io {
        context: "seed important-user-data.txt".to_owned(),
        source,
    })?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn run_healthy_upgrade_subdrill(
    package_id: PackageId,
    v1_version: u64,
    v1_entries: &[RawEntrySpec],
    v2_version: u64,
    v2_entries: &[RawEntrySpec],
    mode: MigrationMode,
    findings: &mut Vec<CertifyFinding>,
) -> Result<Option<u64>, CertifyError> {
    let sandbox = EphemeralSandboxDir::new("gate3-healthy")?;
    let container_dir = sandbox.path().join("user_data_container");
    seed_user_data_container(&container_dir)?;

    let identity = IdentityAuthority::open(sandbox.path().join("identity.db"))
        .map_err(|e| CertifyError::Authority(format!("open IdentityAuthority: {e}")))?;
    let artifacts = ArtifactStore::open(sandbox.path().join("artifacts"))
        .map_err(|e| CertifyError::Authority(format!("open ArtifactStore: {e}")))?;
    let applications = ApplicationAuthority::open(sandbox.path().join("applications.db"))
        .map_err(|e| CertifyError::Authority(format!("open ApplicationAuthority: {e}")))?;

    let publisher_key = SigningKey::from_bytes(&[0x41_u8; 32]);
    let publisher = identity
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
        .map_err(|e| CertifyError::Authority(format!("bootstrap principal: {e}")))?
        .binding();

    let v1_receipt = materialize_verified_package(
        &identity,
        &artifacts,
        &publisher_key,
        publisher.principal_id,
        package_id,
        v1_version,
        v1_entries,
        0x10,
        2_000,
    )?;
    applications
        .install_application(
            &artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: v1_receipt.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x11_u8; 16]),
                installed_at_ms: 2_500,
            },
        )
        .map_err(|e| CertifyError::Authority(format!("install v1: {e}")))?;

    let v2_receipt = materialize_verified_package(
        &identity,
        &artifacts,
        &publisher_key,
        publisher.principal_id,
        package_id,
        v2_version,
        v2_entries,
        0x20,
        3_000,
    )?;

    let mig_key = IdempotencyKey::from_bytes([0x30_u8; 16]);
    applications
        .migrate_application(
            &artifacts,
            MigrateApplicationRequest {
                package_id,
                package_verification_receipt_id: v2_receipt.receipt_id,
                idempotency_key: mig_key,
                compatibility_window: CompatibilityWindow::SameMajor,
                declared_step_count: 2,
                requested_at_ms: 3_500,
            },
        )
        .map_err(|e| CertifyError::Authority(format!("migrate_application(healthy): {e}")))?;

    if mode == MigrationMode::SnapshotAssistedRollback {
        fs::write(
            container_dir.join("state/schema-version.txt"),
            b"schema=v2\ngeneration=2\n",
        )
        .map_err(|source| CertifyError::Io {
            context: "write v2 schema".to_owned(),
            source,
        })?;
    } else {
        fs::write(
            container_dir.join("state/v2-additive.idx"),
            b"additive-dual-readable-index\n",
        )
        .map_err(|source| CertifyError::Io {
            context: "write v2 additive index".to_owned(),
            source,
        })?;
    }

    for (step_idx, ts) in [(1_u64, 3_600_u64), (2_u64, 3_700_u64)] {
        applications
            .record_migration_step(RecordMigrationStepRequest {
                idempotency_key: mig_key,
                step_index: step_idx,
                completed_at_ms: ts,
            })
            .map_err(|e| CertifyError::Authority(format!("record_migration_step: {e}")))?;
    }

    let health = applications
        .run_migration_health_check(
            &artifacts,
            mig_key,
            &StaticHealthProbe { healthy: true },
            3_800,
        )
        .map_err(|e| CertifyError::Authority(format!("run_migration_health_check: {e}")))?;
    let passed = match health {
        MigrationHealthDecision::Recorded(r) | MigrationHealthDecision::Replayed(r) => r.passed,
    };
    if !passed {
        findings.push(CertifyFinding {
            gate: GateId::Gate3TransactionalUpdateAndRollback,
            rule_id: "DRV-HEALTH-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: "gate3/healthy-upgrade".to_owned(),
            message: "healthy v1 -> v2 probe unexpectedly reported unhealthy".to_owned(),
            remediation: "Fix target package post-migration health probe.".to_owned(),
        });
        return Ok(None);
    }

    let activation = applications
        .activate_package_migration(
            &artifacts,
            ActivatePackageMigrationRequest {
                idempotency_key: mig_key,
                activated_at_ms: 3_900,
            },
        )
        .map_err(|e| CertifyError::Authority(format!("activate_package_migration: {e}")))?;
    let activated_receipt = match activation {
        ActivateMigrationDecision::Activated(r) | ActivateMigrationDecision::Replayed(r) => r,
    };

    let inspected = applications
        .inspect_application(package_id)
        .map_err(|e| CertifyError::Authority(format!("inspect_application: {e}")))?
        .ok_or_else(|| CertifyError::Authority("application missing after activate".to_owned()))?;

    let residual_tmp = count_residual_temp_files(&container_dir)
        .map_err(|e| CertifyError::Authority(format!("count_residual_temp_files: {e}")))?;

    if inspected.status != ApplicationStatus::Installed
        || inspected.current_installation_generation.get() != 2
        || inspected.package_manifest_digest != v2_receipt.manifest_digest
        || activated_receipt.installation_generation.get() != 2
        || residual_tmp != 0
    {
        findings.push(CertifyFinding {
            gate: GateId::Gate3TransactionalUpdateAndRollback,
            rule_id: "PKG-UPDATE-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: "gate3/healthy-upgrade".to_owned(),
            message: format!(
                "healthy upgrade failed atomic generation switch (status={:?}, gen={}, residual_tmp={residual_tmp})",
                inspected.status,
                inspected.current_installation_generation.get()
            ),
            remediation: "Ensure v1 -> v2 migration atomically advances to generation 2 without temp residue.".to_owned(),
        });
    }

    Ok(Some(inspected.current_installation_generation.get()))
}

type RollbackDigestPair = (Option<[u8; 32]>, Option<[u8; 32]>);

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_fault_injected_rollback_subdrill(
    drill_label: &str,
    package_id: PackageId,
    v1_version: u64,
    v1_entries: &[RawEntrySpec],
    v2_version: u64,
    v2_entries: &[RawEntrySpec],
    mode: MigrationMode,
    inject_mid_step_crash: bool,
    simulate_unclean_rollback_drift: bool,
    findings: &mut Vec<CertifyFinding>,
) -> Result<RollbackDigestPair, CertifyError> {
    let sandbox = EphemeralSandboxDir::new(drill_label)?;
    let container_dir = sandbox.path().join("user_data_container");
    let snapshot_dir = sandbox.path().join("pre_upgrade_snapshot");
    let staging_dir = sandbox.path().join("window_staging");
    seed_user_data_container(&container_dir)?;

    let pre_tree_digest = compute_container_tree_digest(&container_dir)
        .map_err(|e| CertifyError::Authority(format!("compute_container_tree_digest: {e}")))?;
    let snapshot_receipt = capture_container_snapshot(&container_dir, &snapshot_dir, 2_800)
        .map_err(|e| CertifyError::Authority(format!("capture_container_snapshot: {e}")))?;

    let identity = IdentityAuthority::open(sandbox.path().join("identity.db"))
        .map_err(|e| CertifyError::Authority(format!("open IdentityAuthority: {e}")))?;
    let artifacts = ArtifactStore::open(sandbox.path().join("artifacts"))
        .map_err(|e| CertifyError::Authority(format!("open ArtifactStore: {e}")))?;
    let applications = ApplicationAuthority::open(sandbox.path().join("applications.db"))
        .map_err(|e| CertifyError::Authority(format!("open ApplicationAuthority: {e}")))?;

    let publisher_key = SigningKey::from_bytes(&[0x52_u8; 32]);
    let publisher = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [0x21_u8; 32],
            control_domain_policy_digest: [0x22_u8; 32],
            public_key: publisher_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 1_000,
            key_valid_until_ms: 9_000_000_000,
            idempotency_key: IdempotencyKey::from_bytes([0x02_u8; 16]),
            created_at_ms: 1_000,
        })
        .map_err(|e| CertifyError::Authority(format!("bootstrap principal: {e}")))?
        .binding();

    let v1_receipt = materialize_verified_package(
        &identity,
        &artifacts,
        &publisher_key,
        publisher.principal_id,
        package_id,
        v1_version,
        v1_entries,
        0x12,
        2_000,
    )?;
    applications
        .install_application(
            &artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: v1_receipt.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x13_u8; 16]),
                installed_at_ms: 2_500,
            },
        )
        .map_err(|e| CertifyError::Authority(format!("install v1: {e}")))?;

    let v2_receipt = materialize_verified_package(
        &identity,
        &artifacts,
        &publisher_key,
        publisher.principal_id,
        package_id,
        v2_version,
        v2_entries,
        0x22,
        3_000,
    )?;

    let mig_key = IdempotencyKey::from_bytes([0x32_u8; 16]);
    applications
        .migrate_application(
            &artifacts,
            MigrateApplicationRequest {
                package_id,
                package_verification_receipt_id: v2_receipt.receipt_id,
                idempotency_key: mig_key,
                compatibility_window: CompatibilityWindow::SameMajor,
                declared_step_count: 2,
                requested_at_ms: 3_500,
            },
        )
        .map_err(|e| CertifyError::Authority(format!("migrate_application({drill_label}): {e}")))?;

    // Step 1 mutation.
    if mode == MigrationMode::BackwardCompatibleWindow {
        fs::create_dir_all(&staging_dir).map_err(|source| CertifyError::Io {
            context: "create window staging".to_owned(),
            source,
        })?;
        fs::write(staging_dir.join("step1.idx"), b"staged-step-1\n").map_err(|source| {
            CertifyError::Io {
                context: "write staged step 1".to_owned(),
                source,
            }
        })?;
    } else {
        fs::write(
            container_dir.join("state/schema-version.txt"),
            b"schema=v2-migrating-step-1\n",
        )
        .map_err(|source| CertifyError::Io {
            context: "mutate step 1".to_owned(),
            source,
        })?;
    }
    applications
        .record_migration_step(RecordMigrationStepRequest {
            idempotency_key: mig_key,
            step_index: 1,
            completed_at_ms: 3_600,
        })
        .map_err(|e| CertifyError::Authority(format!("record_migration_step(1): {e}")))?;

    // Step 2: either inject MidStepFault partial writes + .tmp file, or finish step 2 then fail health check.
    if inject_mid_step_crash {
        fs::write(
            container_dir.join("state/schema-version.txt"),
            b"schema=CORRUPTED_MID_STEP_2\n",
        )
        .map_err(|source| CertifyError::Io {
            context: "inject mid-step corruption".to_owned(),
            source,
        })?;
        fs::write(
            container_dir.join("notes/inflight-step-2.tmp"),
            b"HALF_WRITTEN_UNCOMMITTED_TEMP_PAYLOAD",
        )
        .map_err(|source| CertifyError::Io {
            context: "inject mid-step .tmp".to_owned(),
            source,
        })?;
    } else if mode == MigrationMode::SnapshotAssistedRollback {
        fs::write(
            container_dir.join("profile/user-settings.json"),
            b"{\"theme\":\"v2-rewritten\",\"notes_count\":2}\n",
        )
        .map_err(|source| CertifyError::Io {
            context: "mutate step 2".to_owned(),
            source,
        })?;
    }
    applications
        .record_migration_step(RecordMigrationStepRequest {
            idempotency_key: mig_key,
            step_index: 2,
            completed_at_ms: 3_700,
        })
        .map_err(|e| CertifyError::Authority(format!("record_migration_step(2): {e}")))?;

    // Post-migration health probe returns UNHEALTHY (`false`), triggering atomic rollback.
    let health = applications
        .run_migration_health_check(
            &artifacts,
            mig_key,
            &StaticHealthProbe { healthy: false },
            3_800,
        )
        .map_err(|e| CertifyError::Authority(format!("run_migration_health_check: {e}")))?;
    let health_passed = match health {
        MigrationHealthDecision::Recorded(r) | MigrationHealthDecision::Replayed(r) => r.passed,
    };
    if health_passed {
        findings.push(CertifyFinding {
            gate: GateId::Gate3TransactionalUpdateAndRollback,
            rule_id: "DRV-HEALTH-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: format!("gate3/{drill_label}"),
            message: "fault-injected unhealthy probe did not fail closed".to_owned(),
            remediation: "Ensure MigrationHealthProbe failure blocks activation.".to_owned(),
        });
    }

    if staging_dir.exists() {
        let _ = fs::remove_dir_all(&staging_dir);
    }
    if simulate_unclean_rollback_drift {
        // Leave an intentional drift/temp artifact when testing Gate 3 fail-closed behavior.
        fs::write(
            container_dir.join("notes/leaked-drift.tmp"),
            b"DRIFT_NOT_CLEANED",
        )
        .map_err(|source| CertifyError::Io {
            context: "simulate drift".to_owned(),
            source,
        })?;
    } else {
        let _ =
            restore_container_from_snapshot(&snapshot_receipt, &container_dir).map_err(|e| {
                CertifyError::Authority(format!("restore_container_from_snapshot: {e}"))
            })?;
    }

    let rb_decision = applications
        .rollback_package_migration(RollbackPackageMigrationRequest {
            idempotency_key: mig_key,
            rolled_back_at_ms: 3_900,
        })
        .map_err(|e| CertifyError::Authority(format!("rollback_package_migration: {e}")))?;
    let rb_receipt = match rb_decision {
        RollbackMigrationDecision::RolledBack(r) | RollbackMigrationDecision::Replayed(r) => r,
    };

    let post_tree_digest = compute_container_tree_digest(&container_dir)
        .map_err(|e| CertifyError::Authority(format!("compute post_tree_digest: {e}")))?;
    let residual_tmp = count_residual_temp_files(&container_dir)
        .map_err(|e| CertifyError::Authority(format!("count_residual_temp_files: {e}")))?;
    let after_app = applications
        .inspect_application(package_id)
        .map_err(|e| CertifyError::Authority(format!("inspect_application: {e}")))?
        .ok_or_else(|| CertifyError::Authority("application missing after rollback".to_owned()))?;

    if after_app.status != ApplicationStatus::Installed
        || after_app.current_installation_generation != Generation::INITIAL
        || after_app.package_manifest_digest != v1_receipt.manifest_digest
        || rb_receipt.retained_generation != Generation::INITIAL
    {
        findings.push(CertifyFinding {
            gate: GateId::Gate3TransactionalUpdateAndRollback,
            rule_id: "DRV-HEALTH-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: format!("gate3/{drill_label}"),
            message: format!(
                "ApplicationAuthority drifted after {drill_label} rollback (status={:?}, gen={})",
                after_app.status,
                after_app.current_installation_generation.get()
            ),
            remediation:
                "Keep active generation and manifest digest pinned to baseline v1 when health check or step execution fails."
                    .to_owned(),
        });
    }

    if post_tree_digest != pre_tree_digest || residual_tmp != 0 {
        findings.push(CertifyFinding {
            gate: GateId::Gate3TransactionalUpdateAndRollback,
            rule_id: "PKG-UPDATE-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: format!("gate3/{drill_label}"),
            message: format!(
                "UserDataContainer drifted or left temp files after {drill_label} rollback (digest_equal={}, residual_tmp={residual_tmp})",
                post_tree_digest == pre_tree_digest
            ),
            remediation:
                "Restore UserDataContainer bitwise-identically from pre-upgrade snapshot and purge all .tmp files on rollback."
                    .to_owned(),
        });
    }

    Ok((Some(pre_tree_digest), Some(post_tree_digest)))
}
