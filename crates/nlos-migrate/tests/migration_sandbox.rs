//! Integration tests for `[PKG-UPDATE-001]` three-mode container migration and
//! atomic rollback sandbox (`BackwardCompatibleWindow`,
//! `SnapshotAssistedRollback`, `Irreversible`) backed by real
//! `IdentityAuthority`, `ArtifactStore`, and `ApplicationAuthority`.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_application::MigrationState;
use nlos_migrate::{
    ContainerMutationClass, MigrationDrillDisposition, MigrationDrillError, MigrationExecutionPlan,
    MigrationMode, MigrationSandboxHarness, compute_container_tree_digest,
    count_residual_temp_files, create_pre_upgrade_backup, sign_z4_irreversible_consent,
};
use nlos_types::{IdempotencyKey, PackageId};

static SANDBOX_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempSandboxDir {
    path: PathBuf,
}

impl TempSandboxDir {
    fn new(label: &str) -> Self {
        let seq = SANDBOX_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("nlos-migrate-sandbox-{label}-{pid}-{seq}"));
        if path.exists() {
            let _ = fs::remove_dir_all(&path);
        }
        fs::create_dir_all(&path).expect("create temp sandbox dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempSandboxDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn backward_compatible_window_healthy_switch_commits_atomic_generation_advance() {
    let dir = TempSandboxDir::new("window-pass");
    let harness = MigrationSandboxHarness::bootstrap(dir.path()).expect("bootstrap sandbox");
    let pre_digest =
        compute_container_tree_digest(&harness.user_data_container).expect("pre digest");

    let target = harness
        .stage_target_package(1, 0x31, 4_000)
        .expect("stage v1.1.0");
    let key = IdempotencyKey::from_bytes([0x41_u8; 16]);
    let receipt = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::BackwardCompatibleWindow,
            mutation_class: ContainerMutationClass::AdditiveDualReadable,
            target_receipt: target.clone(),
            idempotency_key: key,
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: true,
            pre_upgrade_backup: None,
            z4_consent: None,
            started_at_ms: 5_000,
        })
        .expect("run window migration");

    assert_eq!(receipt.from_generation.get(), 1);
    assert_eq!(receipt.active_generation_after.get(), 2);
    assert_eq!(receipt.pre_tree_digest, pre_digest);
    assert_ne!(receipt.post_tree_digest, pre_digest);
    assert_eq!(receipt.residual_temp_files, 0);

    match receipt.disposition {
        MigrationDrillDisposition::Activated {
            installation_receipt,
        } => {
            assert_eq!(installation_receipt.installation_generation.get(), 2);
            assert_eq!(
                installation_receipt.package_manifest_digest,
                target.manifest_digest
            );
        }
        MigrationDrillDisposition::RolledBackAtomically { .. } => {
            panic!("expected Activated disposition")
        }
    }

    let mig_view = harness
        .applications
        .inspect_package_migration(key)
        .expect("inspect migration")
        .expect("migration row");
    assert_eq!(mig_view.state, MigrationState::Done);

    // Baseline v1 user data remains intact alongside additive window indices.
    let prefs = fs::read_to_string(harness.user_data_container.join("profile/preferences.json"))
        .expect("read preferences");
    assert!(prefs.contains("legacy_v1_token"));
    assert!(
        harness
            .user_data_container
            .join("state/dual-window-step-1.idx")
            .is_file()
    );
}

#[test]
fn backward_compatible_window_health_failure_rolls_back_with_zero_container_pollution() {
    let dir = TempSandboxDir::new("window-rollback");
    let harness = MigrationSandboxHarness::bootstrap(dir.path()).expect("bootstrap sandbox");
    let pre_digest =
        compute_container_tree_digest(&harness.user_data_container).expect("pre digest");

    let target = harness
        .stage_target_package(1, 0x32, 4_000)
        .expect("stage v1.1.0");
    let key = IdempotencyKey::from_bytes([0x42_u8; 16]);
    let receipt = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::BackwardCompatibleWindow,
            mutation_class: ContainerMutationClass::AdditiveDualReadable,
            target_receipt: target,
            idempotency_key: key,
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: false,
            pre_upgrade_backup: None,
            z4_consent: None,
            started_at_ms: 5_000,
        })
        .expect("run window rollback");

    assert_eq!(receipt.from_generation.get(), 1);
    assert_eq!(receipt.active_generation_after.get(), 1);
    assert_eq!(receipt.pre_tree_digest, pre_digest);
    assert_eq!(receipt.post_tree_digest, pre_digest);
    assert!(receipt.byte_identical_on_rollback);
    assert_eq!(receipt.residual_temp_files, 0);

    match receipt.disposition {
        MigrationDrillDisposition::RolledBackAtomically { rollback_receipt } => {
            assert_eq!(rollback_receipt.retained_generation.get(), 1);
            assert_eq!(
                rollback_receipt.retained_manifest_digest,
                harness.baseline_installation.package_manifest_digest
            );
        }
        MigrationDrillDisposition::Activated { .. } => {
            panic!("expected RolledBackAtomically disposition")
        }
    }

    let mig_view = harness
        .applications
        .inspect_package_migration(key)
        .expect("inspect migration")
        .expect("migration row");
    assert_eq!(mig_view.state, MigrationState::Failed);
}

#[test]
fn snapshot_assisted_rollback_restores_100_percent_byte_identical_tree_on_mid_step_crash() {
    let dir = TempSandboxDir::new("snapshot-crash");
    let harness = MigrationSandboxHarness::bootstrap(dir.path()).expect("bootstrap sandbox");
    let pre_digest =
        compute_container_tree_digest(&harness.user_data_container).expect("pre digest");
    let pre_schema =
        fs::read_to_string(harness.user_data_container.join("state/schema-version.txt"))
            .expect("read baseline schema");

    let target = harness
        .stage_target_package(1, 0x33, 4_000)
        .expect("stage v1.1.0");
    let key = IdempotencyKey::from_bytes([0x43_u8; 16]);
    let receipt = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::SnapshotAssistedRollback,
            mutation_class: ContainerMutationClass::SchemaTransformRequiringSnapshot,
            target_receipt: target,
            idempotency_key: key,
            declared_steps: 3,
            crash_at_step: Some(2),
            health_check_passes: true,
            pre_upgrade_backup: None,
            z4_consent: None,
            started_at_ms: 5_000,
        })
        .expect("run snapshot crash recovery");

    assert!(receipt.snapshot_receipt.is_some());
    assert_eq!(receipt.from_generation.get(), 1);
    assert_eq!(receipt.active_generation_after.get(), 1);
    assert_eq!(receipt.pre_tree_digest, pre_digest);
    assert_eq!(receipt.post_tree_digest, pre_digest);
    assert!(receipt.byte_identical_on_rollback);
    assert_eq!(receipt.residual_temp_files, 0);
    assert_eq!(
        count_residual_temp_files(&harness.user_data_container).expect("count tmp"),
        0
    );

    let post_schema =
        fs::read_to_string(harness.user_data_container.join("state/schema-version.txt"))
            .expect("read restored schema");
    assert_eq!(post_schema, pre_schema);
    assert!(
        !harness
            .user_data_container
            .join("notes/inflight-step.tmp")
            .exists()
    );
}

#[test]
fn snapshot_assisted_rollback_recovers_from_health_failure_then_succeeds_on_retry() {
    let dir = TempSandboxDir::new("snapshot-health-then-pass");
    let harness = MigrationSandboxHarness::bootstrap(dir.path()).expect("bootstrap sandbox");
    let pre_digest =
        compute_container_tree_digest(&harness.user_data_container).expect("pre digest");

    let target = harness
        .stage_target_package(1, 0x34, 4_000)
        .expect("stage v1.1.0");

    // Attempt 1: health probe fails after all v2 container rewrites complete.
    let fail_receipt = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::SnapshotAssistedRollback,
            mutation_class: ContainerMutationClass::SchemaTransformRequiringSnapshot,
            target_receipt: target.clone(),
            idempotency_key: IdempotencyKey::from_bytes([0x44_u8; 16]),
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: false,
            pre_upgrade_backup: None,
            z4_consent: None,
            started_at_ms: 5_000,
        })
        .expect("run snapshot health fail");

    assert_eq!(fail_receipt.active_generation_after.get(), 1);
    assert_eq!(fail_receipt.post_tree_digest, pre_digest);
    assert!(fail_receipt.byte_identical_on_rollback);

    // Attempt 2: healthy target passes and switches atomically to generation 2.
    let ok_receipt = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::SnapshotAssistedRollback,
            mutation_class: ContainerMutationClass::SchemaTransformRequiringSnapshot,
            target_receipt: target,
            idempotency_key: IdempotencyKey::from_bytes([0x45_u8; 16]),
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: true,
            pre_upgrade_backup: None,
            z4_consent: None,
            started_at_ms: 7_000,
        })
        .expect("run snapshot healthy");

    assert_eq!(ok_receipt.active_generation_after.get(), 2);
    assert_ne!(ok_receipt.post_tree_digest, pre_digest);
    let final_schema =
        fs::read_to_string(harness.user_data_container.join("state/schema-version.txt"))
            .expect("read v2 schema");
    assert!(final_schema.contains("schema=v2"));
}

#[test]
fn disguised_irreversible_or_unprotected_schema_rewrite_fails_closed_before_authority_write() {
    let dir = TempSandboxDir::new("disguised-blocked");
    let harness = MigrationSandboxHarness::bootstrap(dir.path()).expect("bootstrap sandbox");
    let pre_digest =
        compute_container_tree_digest(&harness.user_data_container).expect("pre digest");
    let target = harness
        .stage_target_package(1, 0x35, 4_000)
        .expect("stage v1.1.0");

    for bad_mode in [
        MigrationMode::BackwardCompatibleWindow,
        MigrationMode::SnapshotAssistedRollback,
    ] {
        let err = harness
            .run_migration_plan(&MigrationExecutionPlan {
                mode: bad_mode,
                mutation_class:
                    ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite,
                target_receipt: target.clone(),
                idempotency_key: IdempotencyKey::from_bytes([0x46_u8; 16]),
                declared_steps: 2,
                crash_at_step: None,
                health_check_passes: true,
                pre_upgrade_backup: None,
                z4_consent: None,
                started_at_ms: 5_000,
            })
            .expect_err("must reject disguised irreversible migration");
        assert!(matches!(
            err,
            MigrationDrillError::DisguisedIrreversibleMigration { .. }
        ));
    }

    let window_schema_err = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::BackwardCompatibleWindow,
            mutation_class: ContainerMutationClass::SchemaTransformRequiringSnapshot,
            target_receipt: target,
            idempotency_key: IdempotencyKey::from_bytes([0x47_u8; 16]),
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: true,
            pre_upgrade_backup: None,
            z4_consent: None,
            started_at_ms: 5_000,
        })
        .expect_err("must reject unprotected schema transform in window mode");
    assert!(matches!(
        window_schema_err,
        MigrationDrillError::IncompatibleMutationClass { .. }
    ));

    // Zero migration rows recorded in ApplicationAuthority and zero container mutation.
    assert_eq!(
        harness
            .applications
            .list_package_migrations(harness.package_id)
            .expect("list migrations"),
        Vec::new()
    );
    assert_eq!(
        compute_container_tree_digest(&harness.user_data_container).expect("post digest"),
        pre_digest
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn irreversible_mode_enforces_dual_backup_and_ed25519_z4_consent_gates_with_zero_leakage() {
    let dir = TempSandboxDir::new("irreversible-gates");
    let harness = MigrationSandboxHarness::bootstrap(dir.path()).expect("bootstrap sandbox");
    let pre_digest =
        compute_container_tree_digest(&harness.user_data_container).expect("pre digest");

    let target_v1_1 = harness
        .stage_target_package(1, 0x36, 4_000)
        .expect("stage v1.1.0");
    let target_v1_2 = harness
        .stage_target_package(2, 0x37, 4_100)
        .expect("stage v1.2.0");

    // 1. Missing PreUpgradeBackupReceipt -> rejected pre-flight.
    let err_no_backup = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::Irreversible,
            mutation_class: ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite,
            target_receipt: target_v1_1.clone(),
            idempotency_key: IdempotencyKey::from_bytes([0x50_u8; 16]),
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: true,
            pre_upgrade_backup: None,
            z4_consent: None,
            started_at_ms: 5_000,
        })
        .expect_err("missing backup must fail");
    assert_eq!(err_no_backup, MigrationDrillError::MissingPreUpgradeBackup);

    // Create a valid on-disk backup archive.
    let backup_path = harness.backups_dir.join("v1-backup.nlosbak");
    let valid_backup = create_pre_upgrade_backup(
        &harness.user_data_container,
        &backup_path,
        harness.package_id,
        harness.baseline_installation.package_manifest_digest,
        4_500,
    )
    .expect("create backup");

    // 2. Tampered backup archive on disk -> rejected pre-flight.
    let tampered_path = harness.backups_dir.join("v1-tampered.nlosbak");
    let mut tampered_backup = create_pre_upgrade_backup(
        &harness.user_data_container,
        &tampered_path,
        harness.package_id,
        harness.baseline_installation.package_manifest_digest,
        4_500,
    )
    .expect("create second backup");
    let mut raw_archive = fs::read(&tampered_path).expect("read archive");
    let last_idx = raw_archive.len() - 1;
    raw_archive[last_idx] ^= 0xFF;
    fs::write(&tampered_path, &raw_archive).expect("overwrite tampered archive");
    tampered_backup.backup_archive_path = tampered_path;

    let valid_consent = sign_z4_irreversible_consent(
        &harness.z4_operator_key,
        harness.z4_operator_binding.principal_id,
        harness.package_id,
        harness.baseline_installation.package_manifest_digest,
        target_v1_1.manifest_digest,
        valid_backup.archive_digest,
        4_800,
    );

    let err_tampered_backup = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::Irreversible,
            mutation_class: ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite,
            target_receipt: target_v1_1.clone(),
            idempotency_key: IdempotencyKey::from_bytes([0x51_u8; 16]),
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: true,
            pre_upgrade_backup: Some(tampered_backup),
            z4_consent: Some(valid_consent.clone()),
            started_at_ms: 5_000,
        })
        .expect_err("tampered backup must fail");
    assert!(matches!(
        err_tampered_backup,
        MigrationDrillError::InvalidPreUpgradeBackup { .. }
    ));

    // 3. Missing Z4TrustedUiConsentReceipt -> rejected pre-flight.
    let err_no_z4 = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::Irreversible,
            mutation_class: ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite,
            target_receipt: target_v1_1.clone(),
            idempotency_key: IdempotencyKey::from_bytes([0x52_u8; 16]),
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: true,
            pre_upgrade_backup: Some(valid_backup.clone()),
            z4_consent: None,
            started_at_ms: 5_000,
        })
        .expect_err("missing Z4 consent must fail");
    assert_eq!(err_no_z4, MigrationDrillError::MissingZ4TrustedUiConsent);

    // 4. Replaying Z4 signature bound to another package, another target digest, or forged bytes -> rejected.
    let wrong_pkg_consent = sign_z4_irreversible_consent(
        &harness.z4_operator_key,
        harness.z4_operator_binding.principal_id,
        PackageId::from_bytes([0x99_u8; 16]),
        harness.baseline_installation.package_manifest_digest,
        target_v1_1.manifest_digest,
        valid_backup.archive_digest,
        4_800,
    );
    let wrong_target_consent = sign_z4_irreversible_consent(
        &harness.z4_operator_key,
        harness.z4_operator_binding.principal_id,
        harness.package_id,
        harness.baseline_installation.package_manifest_digest,
        target_v1_2.manifest_digest,
        valid_backup.archive_digest,
        4_800,
    );
    let mut forged_sig_consent = valid_consent.clone();
    forged_sig_consent.signature[0] ^= 0x5A;

    for bad_consent in [wrong_pkg_consent, wrong_target_consent, forged_sig_consent] {
        let err_bad_z4 = harness
            .run_migration_plan(&MigrationExecutionPlan {
                mode: MigrationMode::Irreversible,
                mutation_class:
                    ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite,
                target_receipt: target_v1_1.clone(),
                idempotency_key: IdempotencyKey::from_bytes([0x53_u8; 16]),
                declared_steps: 2,
                crash_at_step: None,
                health_check_passes: true,
                pre_upgrade_backup: Some(valid_backup.clone()),
                z4_consent: Some(bad_consent),
                started_at_ms: 5_000,
            })
            .expect_err("invalid/replayed Z4 consent must fail");
        assert!(matches!(
            err_bad_z4,
            MigrationDrillError::InvalidZ4TrustedUiConsent { .. }
        ));
    }

    // Confirm zero authority or container writes occurred across all 6 failed gate attempts!
    assert_eq!(
        harness
            .applications
            .list_package_migrations(harness.package_id)
            .expect("list migrations"),
        Vec::new()
    );
    assert_eq!(
        compute_container_tree_digest(&harness.user_data_container).expect("unchanged digest"),
        pre_digest
    );

    // 5. Valid backup + valid Z4 Ed25519 signature -> succeeds and advances generation 1 -> 2.
    let ok_receipt = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::Irreversible,
            mutation_class: ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite,
            target_receipt: target_v1_1.clone(),
            idempotency_key: IdempotencyKey::from_bytes([0x54_u8; 16]),
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: true,
            pre_upgrade_backup: Some(valid_backup.clone()),
            z4_consent: Some(valid_consent),
            started_at_ms: 5_000,
        })
        .expect("dual-gated irreversible migration must succeed");

    assert_eq!(ok_receipt.active_generation_after.get(), 2);
    assert_eq!(ok_receipt.backup_digest, Some(valid_backup.archive_digest));
    assert_eq!(
        ok_receipt.z4_operator_principal,
        Some(harness.z4_operator_binding.principal_id)
    );

    // 6. Revoke Z4 operator key in IdentityAuthority; subsequent irreversible upgrade signed by revoked key must fail-closed!
    harness
        .revoke_z4_operator_key(8_000)
        .expect("revoke operator key");
    let v1_1_backup = create_pre_upgrade_backup(
        &harness.user_data_container,
        &harness.backups_dir.join("v1_1-backup.nlosbak"),
        harness.package_id,
        target_v1_1.manifest_digest,
        8_200,
    )
    .expect("create v1.1 backup");
    let revoked_consent = sign_z4_irreversible_consent(
        &harness.z4_operator_key,
        harness.z4_operator_binding.principal_id,
        harness.package_id,
        target_v1_1.manifest_digest,
        target_v1_2.manifest_digest,
        v1_1_backup.archive_digest,
        8_500,
    );

    let err_revoked = harness
        .run_migration_plan(&MigrationExecutionPlan {
            mode: MigrationMode::Irreversible,
            mutation_class: ContainerMutationClass::DestructiveFieldDeletionOrIrreversibleRewrite,
            target_receipt: target_v1_2,
            idempotency_key: IdempotencyKey::from_bytes([0x55_u8; 16]),
            declared_steps: 2,
            crash_at_step: None,
            health_check_passes: true,
            pre_upgrade_backup: Some(v1_1_backup),
            z4_consent: Some(revoked_consent),
            started_at_ms: 9_000,
        })
        .expect_err("revoked Z4 operator key must fail-closed");
    assert!(matches!(
        err_revoked,
        MigrationDrillError::InvalidZ4TrustedUiConsent { .. }
    ));
    assert_eq!(
        harness
            .applications
            .list_package_migrations(harness.package_id)
            .expect("list migrations")
            .len(),
        1
    );
}
