//! Integration tests for `[PKG-UPDATE-001]` three-mode data container migration
//! and fault-rollback sandbox drills (`Stage E W78 / E3`).

#![forbid(unsafe_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_application::ApplicationAuthority;
use nlos_migrate::{
    DataMigrationMode, DrillError, DrillFaultInjection, DrillOutcomeKind, DrillRequest,
    EphemeralIrreversibleProvisioning, compute_userdata_tree_sha256, create_userdata_snapshot,
    restore_userdata_snapshot, run_migration_drill,
};
use nlos_store_fault::{FaultCode, FaultMode};
use rusqlite::{Connection, OpenFlags};

static DRILL_TEST_SEQ: AtomicU64 = AtomicU64::new(1);
static VFS_FAULT_LOCK: Mutex<()> = Mutex::new(());

struct TempSandbox {
    path: PathBuf,
}

impl TempSandbox {
    fn new(tag: &str) -> Self {
        let seq = DRILL_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-migrate-drill-it-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("mkdir temp sandbox");
        Self { path }
    }
}

impl Drop for TempSandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn mode_1_backward_compatible_window_activates_v2_and_blocks_breaking_ddl() {
    let sandbox = TempSandbox::new("window-pos");
    let mut req = DrillRequest::for_mode(DataMigrationMode::BackwardCompatibleWindow);
    req.state_root = Some(sandbox.path.clone());

    let report = run_migration_drill(&req).expect("window migration must succeed");
    assert_eq!(report.outcome, DrillOutcomeKind::ActivatedGeneration2);
    assert_eq!(report.initial_generation, 1);
    assert_eq!(report.final_generation, 2);
    assert_eq!(
        report.active_manifest_digest_hex,
        report.v2_manifest_digest_hex
    );
    assert!(!report.userdata_matches_pre_upgrade);

    // Adding a NOT NULL column without DEFAULT breaks v1 writers during window.
    let mut bad_req = DrillRequest::for_mode(DataMigrationMode::BackwardCompatibleWindow);
    bad_req.sql_steps =
        vec!["ALTER TABLE user_documents ADD COLUMN req_col TEXT NOT NULL;".to_owned()];
    let err = run_migration_drill(&bad_req).expect_err("window-breaking NOT NULL must fail");
    assert!(matches!(err, DrillError::WindowIncompatibility { .. }));
}

#[test]
fn mode_2_snapshot_assisted_mid_step_crash_recovers_and_converges_to_v2() {
    let sandbox = TempSandbox::new("snap-crash");
    let mut req = DrillRequest::for_mode(DataMigrationMode::SnapshotAssistedRollback);
    req.sql_steps = vec![
        "CREATE TABLE IF NOT EXISTS user_doc_audit_v2 (doc_id TEXT PRIMARY KEY, tag TEXT NOT NULL);"
            .to_owned(),
        "INSERT OR REPLACE INTO user_doc_audit_v2 (doc_id, tag) VALUES ('doc-001', 'migrated');"
            .to_owned(),
        "INSERT OR REPLACE INTO user_doc_audit_v2 (doc_id, tag) VALUES ('doc-002', 'migrated');"
            .to_owned(),
    ];
    req.fault_injection = DrillFaultInjection::CrashAfterStep(2);
    req.state_root = Some(sandbox.path.clone());

    let report = run_migration_drill(&req).expect("crash recovery must converge");
    assert_eq!(
        report.outcome,
        DrillOutcomeKind::RecoveredCrashAndActivatedGeneration2 {
            crashed_after_step: 2,
            replayed_steps: 2,
        }
    );
    assert_eq!(report.final_generation, 2);
    assert_eq!(report.steps_declared, 3);
    assert_eq!(report.steps_completed, 3);
    assert_eq!(
        report.active_manifest_digest_hex,
        report.v2_manifest_digest_hex
    );
}

#[test]
fn mode_2_snapshot_assisted_health_failure_rolls_back_generation_and_userdata_sha256() {
    let sandbox = TempSandbox::new("snap-rollback");
    let mut req = DrillRequest::for_mode(DataMigrationMode::SnapshotAssistedRollback);
    req.fault_injection = DrillFaultInjection::PostMigrationHealthFailure;
    req.state_root = Some(sandbox.path.clone());

    let report = run_migration_drill(&req).expect("rollback drill must complete cleanly");
    assert_eq!(report.outcome, DrillOutcomeKind::RolledBackOnHealthFailure);
    assert_eq!(report.initial_generation, 1);
    assert_eq!(report.final_generation, 1);
    assert_eq!(
        report.active_manifest_digest_hex,
        report.v1_manifest_digest_hex
    );
    assert_ne!(
        report.mutated_userdata_sha256_hex.as_deref(),
        Some(report.pre_upgrade_userdata_sha256_hex.as_str()),
        "UserData must actually mutate before rollback restores it"
    );
    assert_eq!(
        report.final_userdata_sha256_hex,
        report.pre_upgrade_userdata_sha256_hex
    );
    assert!(report.userdata_matches_pre_upgrade);
    assert!(
        !sandbox
            .path
            .join("userdata/v2_unhealthy_index.bin")
            .exists(),
        "dirty half-migrated v2 file must be purged by atomic snapshot restore"
    );

    let apps = ApplicationAuthority::open(sandbox.path.join("applications")).expect("open apps");
    let migrations = apps
        .list_package_migrations(req.packages.v1.package_id)
        .expect("list migrations");
    assert_eq!(migrations.len(), 1);
    let rb_receipt = apps
        .inspect_migration_rollback_receipt(migrations[0].idempotency_key)
        .expect("inspect rollback receipt")
        .expect("rollback receipt must exist");
    assert_eq!(rb_receipt.retained_generation.get(), 1);
}

#[test]
fn mode_2_sqlite_vfs_io_fault_mid_migration_restores_exact_userdata_snapshot() {
    let _guard = VFS_FAULT_LOCK.lock().expect("lock VFS fault state");
    nlos_store_fault::register("nlos-migrate-fault").expect("register fault VFS");
    nlos_store_fault::disarm();

    let sandbox = TempSandbox::new("vfs-ioerr");
    let userdata_dir = sandbox.path.join("userdata");
    fs::create_dir_all(&userdata_dir).expect("mkdir userdata");
    let db_path = userdata_dir.join("app_data.db");

    {
        let conn = Connection::open(&db_path).expect("open baseline db");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("journal DELETE");
        conn.execute_batch(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, val TEXT NOT NULL);
             INSERT INTO items (id, val) VALUES (1, 'v1-intact');",
        )
        .expect("seed items");
    }

    let snap_dir = sandbox.path.join("snapshot_vault");
    let snap = create_userdata_snapshot(&userdata_dir, &snap_dir).expect("create snapshot");

    // Mutate step 1, then inject SQLITE_IOERR via nlos-store-fault on step 2.
    let uri = format!("file:{}?vfs=nlos-migrate-fault", db_path.display());
    let fault_conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open fault-wrapped SQLite connection");
    fault_conn
        .execute("INSERT INTO items (id, val) VALUES (2, 'dirty-step-1')", [])
        .expect("step 1 write");

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let fault_res = fault_conn.execute("INSERT INTO items (id, val) VALUES (3, 'boom')", []);
    nlos_store_fault::disarm();
    drop(fault_conn);
    assert!(fault_res.is_err(), "armed VFS must inject SQLITE_IOERR");

    let dirty_sha = compute_userdata_tree_sha256(&userdata_dir).expect("dirty sha");
    assert_ne!(dirty_sha, snap.expected_sha256);

    let restored_sha = restore_userdata_snapshot(&snap, &userdata_dir).expect("restore snapshot");
    assert_eq!(restored_sha, snap.expected_sha256);
}

#[test]
fn mode_3_disguised_irreversible_sql_is_physically_banned_in_window_and_snapshot_modes() {
    for disguised_mode in [
        DataMigrationMode::BackwardCompatibleWindow,
        DataMigrationMode::SnapshotAssistedRollback,
    ] {
        for destructive_sql in [
            "DROP TABLE user_documents;",
            "ALTER TABLE user_documents DROP COLUMN body;",
            "TRUNCATE TABLE user_documents;",
            "-- ONE_WAY_REWRITE v1_blob -> v2_cipher",
        ] {
            let sandbox = TempSandbox::new("disguise-ban");
            let mut req = DrillRequest::for_mode(disguised_mode);
            req.sql_steps = vec![destructive_sql.to_owned()];
            req.state_root = Some(sandbox.path.clone());

            let err = run_migration_drill(&req).expect_err("disguised destructive SQL must fail");
            assert!(
                matches!(err, DrillError::DisguisedIrreversibleMigration { .. }),
                "expected DisguisedIrreversibleMigration for {destructive_sql:?}, got {err:?}"
            );
            assert!(
                !sandbox.path.join("applications").exists(),
                "disguised migration must be rejected before opening/mutating state root"
            );
        }
    }
}

#[test]
fn mode_3_irreversible_enforces_pre_backup_and_z4_consent_before_mutation() {
    // 1. Missing pre-upgrade backup -> fails closed with zero migration rows.
    let no_backup_sb = TempSandbox::new("irrev-no-backup");
    let mut no_backup_req = DrillRequest::for_mode(DataMigrationMode::Irreversible);
    no_backup_req.state_root = Some(no_backup_sb.path.clone());
    let err1 = run_migration_drill(&no_backup_req).expect_err("missing backup must fail");
    assert!(matches!(
        err1,
        DrillError::IrreversibleMissingPreBackup { .. }
    ));
    let apps1 = ApplicationAuthority::open(no_backup_sb.path.join("applications")).expect("apps1");
    assert_eq!(
        apps1
            .list_package_migrations(no_backup_req.packages.v1.package_id)
            .expect("list1")
            .len(),
        0
    );

    // 2. Backup present, missing Z4 consent -> fails closed with zero migration rows.
    let no_z4_sb = TempSandbox::new("irrev-no-z4");
    let mut no_z4_req = DrillRequest::for_mode(DataMigrationMode::Irreversible);
    no_z4_req.state_root = Some(no_z4_sb.path.clone());
    no_z4_req.ephemeral_irreversible = EphemeralIrreversibleProvisioning {
        create_verified_backup: true,
        issue_z4_consent: false,
        override_z4_token: None,
    };
    let err2 = run_migration_drill(&no_z4_req).expect_err("missing Z4 consent must fail");
    assert!(matches!(
        err2,
        DrillError::IrreversibleInvalidZ4Consent { .. }
    ));
    let apps2 = ApplicationAuthority::open(no_z4_sb.path.join("applications")).expect("apps2");
    assert_eq!(
        apps2
            .list_package_migrations(no_z4_req.packages.v1.package_id)
            .expect("list2")
            .len(),
        0
    );

    // 3. Forged Z4 consent token -> fails closed.
    let bad_z4_sb = TempSandbox::new("irrev-forged-z4");
    let mut bad_z4_req = DrillRequest::for_mode(DataMigrationMode::Irreversible);
    bad_z4_req.state_root = Some(bad_z4_sb.path.clone());
    bad_z4_req.ephemeral_irreversible = EphemeralIrreversibleProvisioning {
        create_verified_backup: true,
        issue_z4_consent: true,
        override_z4_token: Some([0xAA; 32]),
    };
    let err3 = run_migration_drill(&bad_z4_req).expect_err("forged Z4 token must fail");
    assert!(matches!(
        err3,
        DrillError::IrreversibleInvalidZ4Consent { .. }
    ));

    // 4. Both SHA-256 backup + bound Z4 consent valid -> controlled destructive migration succeeds.
    let ok_sb = TempSandbox::new("irrev-ok");
    let mut ok_req = DrillRequest::for_mode(DataMigrationMode::Irreversible);
    ok_req.state_root = Some(ok_sb.path.clone());
    ok_req.ephemeral_irreversible = EphemeralIrreversibleProvisioning {
        create_verified_backup: true,
        issue_z4_consent: true,
        override_z4_token: None,
    };
    let report = run_migration_drill(&ok_req).expect("authorized irreversible migration must pass");
    assert_eq!(report.outcome, DrillOutcomeKind::ActivatedGeneration2);
    assert_eq!(report.final_generation, 2);
}
