//! Integration tests for `nlos-migrate` static declarative manifest checker,
//! `[PKG-SEED-001]` / `[PKG-IMM-001]` payload scanner, `[DRV-LIFE-001]` driver
//! effect rules, `[COMPAT-DEPRECATE-001]` upgrade prescriptions, and CLI exit
//! codes (`0`, `1`, `2`, `3`).

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_migrate::{
    AuditOptions, AuditVerdict, FindingSeverity, audit_package_directory,
    audit_package_directory_with_yaml,
};

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempPkgDir {
    path: PathBuf,
}

impl TempPkgDir {
    fn new(label: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("nlos-migrate-static-{label}-{pid}-{seq}"));
        if path.exists() {
            let _ = fs::remove_dir_all(&path);
        }
        fs::create_dir_all(&path).expect("create temp package dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(&full, bytes).expect("write test payload");
    }
}

impl Drop for TempPkgDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn starter_assistant_app_v1_0_0_and_v1_1_0_pass_static_audit_cleanly() {
    let root = workspace_root();
    let v1_0 = root.join("examples/starter-assistant-app/package");
    let v1_1 = root.join("examples/starter-assistant-app/package-v1.1.0");

    for (pkg_dir, expected_ver) in [(&v1_0, "1.0.0"), (&v1_1, "1.1.0")] {
        let report = audit_package_directory(pkg_dir, &AuditOptions::default());
        assert_eq!(report.verdict, AuditVerdict::Pass);
        assert!(report.verdict.is_compliant());
        assert_eq!(report.verdict.exit_code(), 0);
        assert_eq!(report.blocker_count, 0);
        assert_eq!(report.warning_count, 0);
        assert_eq!(report.findings, Vec::new());
        assert_eq!(report.prescriptions, Vec::new());
        assert_eq!(report.version.as_deref(), Some(expected_ver));
        assert_eq!(
            report.negotiated_route.as_deref(),
            Some("NATIVE_CURRENT_EPOCH")
        );
        assert_eq!(report.component_count, 2);

        let json = report.render_json();
        assert!(json.contains("\"verdict\": \"PASS\""));
        assert!(json.contains("\"exit_code\": 0"));
    }
}

#[test]
fn multi_component_manifest_with_complete_driver_effects_passes() {
    let pkg = TempPkgDir::new("multi-pass");
    pkg.write(
        "package.manifest",
        b"package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
          version = 1.2.0\n\
          target-sabi-epoch = 1.1\n\
          driver-abi-epoch = 1\n\
          migration-mode = SNAPSHOT_ASSISTED_ROLLBACK\n\
          component = app-main executable bin/app.bin\n\
          component = audio-drv driver drivers/audio.wasm\n\
          component = dashboard ui-surface ui/dashboard.bundle\n\
          component = indexer background-service svc/indexer.bin\n\
          component = init-seed seed-template seed/init.sql\n\
          component = lexicon data assets/lexicon.txt\n\
          effect = audio-drv REVERSIBLE reconcile=same-idempotency-key\n\
          effect = audio-drv::query READ_ONLY reconcile=none\n\
          sabi-use = nlos.sabi.ServiceDirectory ResolveServiceRequest\n\
          sabi-use = nlos.sabi.DriverProvider DriverProviderRequest\n\
          sabi-ext = nlos.sabi.Envelope noncritical 42\n",
    );
    pkg.write("bin/app.bin", b"ELF-BINARY-PLACEHOLDER");
    pkg.write("drivers/audio.wasm", b"\0asm\x01\0\0\0");
    pkg.write("ui/dashboard.bundle", b"declarative-ui-surface-v1");
    pkg.write("svc/indexer.bin", b"background-service-v1");
    pkg.write(
        "seed/init.sql",
        b"CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT);\n",
    );
    pkg.write("assets/lexicon.txt", b"task-scheduler\ndisk-quota\n");

    let report = audit_package_directory(pkg.path(), &AuditOptions::default());
    assert_eq!(
        report.verdict,
        AuditVerdict::Pass,
        "{}",
        report.render_text()
    );
    assert_eq!(report.component_count, 6);
    assert_eq!(
        report.migration_mode.as_deref(),
        Some("SNAPSHOT_ASSISTED_ROLLBACK")
    );
    assert_eq!(report.blocker_count, 0);
}

#[test]
fn driver_missing_effect_descriptor_or_declaring_fresh_key_blind_retry_blocks() {
    // Case A: Driver declared without any EffectDescriptor.
    let missing_effect = TempPkgDir::new("drv-no-effect");
    missing_effect.write(
        "package.manifest",
        b"package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
          version = 1.0.0\n\
          component = storage-drv driver drivers/storage.wasm\n",
    );
    missing_effect.write("drivers/storage.wasm", b"\0asm\x01\0\0\0");

    let rep_a = audit_package_directory(missing_effect.path(), &AuditOptions::default());
    assert_eq!(rep_a.verdict, AuditVerdict::Blocked);
    assert_eq!(rep_a.verdict.exit_code(), 2);
    assert!(rep_a.findings.iter().any(|finding| {
        finding.severity == FindingSeverity::Blocker
            && finding.rule_id == "DRV-LIFE-001"
            && finding
                .message
                .contains("missing a required EffectDescriptor")
    }));

    // Case B: Driver declares forbidden fresh-key blind retry or mutating reconcile=none.
    let bad_reconcile = TempPkgDir::new("drv-bad-reconcile");
    bad_reconcile.write(
        "package.manifest",
        b"package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
          version = 1.0.0\n\
          component = payment-drv driver drivers/payment.wasm\n\
          effect = payment-drv::charge IRREVERSIBLE reconcile=fresh-key-blind-retry\n\
          effect = payment-drv::refund COMPENSATABLE reconcile=none\n\
          effect = payment-drv::stage REVERSIBLE\n",
    );
    bad_reconcile.write("drivers/payment.wasm", b"\0asm\x01\0\0\0");

    let rep_b = audit_package_directory(bad_reconcile.path(), &AuditOptions::default());
    assert_eq!(rep_b.verdict, AuditVerdict::Blocked);
    assert_eq!(rep_b.blocker_count, 3, "{}", rep_b.render_text());
    assert!(
        rep_b
            .findings
            .iter()
            .all(|finding| finding.rule_id == "DRV-LIFE-001")
    );
}

#[test]
fn mutable_sqlite_wal_shm_and_binary_sqlite3_headers_in_seed_templates_block() {
    // Case A: `.sqlite-wal` and stray `.db-shm` sidecars.
    let wal_pkg = TempPkgDir::new("seed-wal");
    wal_pkg.write(
        "package.manifest",
        b"package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
          version = 1.0.0\n\
          component = app executable app.bin\n\
          component = seed-wal seed-template seed/state.sqlite-wal\n",
    );
    wal_pkg.write("app.bin", b"binary");
    wal_pkg.write("seed/state.sqlite-wal", b"mutable-wal-frames");
    wal_pkg.write("seed/state.db-shm", b"mutable-shm-index");

    let rep_wal = audit_package_directory(wal_pkg.path(), &AuditOptions::default());
    assert_eq!(rep_wal.verdict, AuditVerdict::Blocked);
    assert!(
        rep_wal
            .findings
            .iter()
            .filter(|finding| finding.rule_id == "PKG-SEED-001")
            .count()
            >= 2,
        "{}",
        rep_wal.render_text()
    );

    // Case B: Disguised file extension containing `SQLite format 3\0` binary header.
    let bin_db_pkg = TempPkgDir::new("seed-sqlite-header");
    bin_db_pkg.write(
        "package.manifest",
        b"package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
          version = 1.0.0\n\
          component = app executable app.bin\n\
          component = seed-blob seed-template seed/template.dat\n",
    );
    bin_db_pkg.write("app.bin", b"binary");
    bin_db_pkg.write(
        "seed/template.dat",
        b"SQLite format 3\0\x10\x00\x01\x01\x00@  \x00\x00\x00\x01",
    );

    let rep_bin = audit_package_directory(bin_db_pkg.path(), &AuditOptions::default());
    assert_eq!(rep_bin.verdict, AuditVerdict::Blocked);
    assert!(rep_bin.findings.iter().any(|finding| {
        finding.severity == FindingSeverity::Blocker && finding.rule_id == "PKG-SEED-001"
    }));
}

#[test]
fn hardcoded_secrets_are_blocked_and_redacted_from_all_reports() {
    let secret_pkg = TempPkgDir::new("secrets");
    secret_pkg.write(
        "package.manifest",
        b"package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
          version = 1.0.0\n\
          component = provider-drv executable provider.bin\n\
          component = defaults seed-template config/defaults.toml\n",
    );
    let raw_sk = "sk-live-ULTRASECRET9988776655";
    let raw_bearer = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.SECRETCLAIM";
    let raw_api_key = "top_secret_vendor_credential_xyz";
    secret_pkg.write(
        "provider.bin",
        format!("auth_header = Authorization: Bearer {raw_bearer}\ntoken = {raw_sk}\n").as_bytes(),
    );
    secret_pkg.write(
        "config/defaults.toml",
        format!(
            "api_key = \"{raw_api_key}\"\n-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC\n-----END PRIVATE KEY-----\n"
        )
        .as_bytes(),
    );

    let report = audit_package_directory(secret_pkg.path(), &AuditOptions::default());
    assert_eq!(report.verdict, AuditVerdict::Blocked);
    assert!(
        report
            .findings
            .iter()
            .filter(|finding| finding.rule_id == "PKG-IMM-001")
            .count()
            >= 4,
        "{}",
        report.render_text()
    );

    let rendered_text = report.render_text();
    let rendered_json = report.render_json();
    for sensitive in [raw_sk, raw_bearer, raw_api_key, "MIIEvQIBADANBgkqhkiG"] {
        assert!(
            !rendered_text.contains(sensitive),
            "leaked raw secret {sensitive} in text report"
        );
        assert!(
            !rendered_json.contains(sensitive),
            "leaked raw secret {sensitive} in JSON report"
        );
    }
}

const EVOLVED_EPOCH_3_YAML: &str = r#"
manifest_version: 1
current_epoch: 3
min_compatibility_window_epochs: 2
frozen_channels_total: 2

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 3
    minor: 1
    frozen: true
    reserved_field_numbers: [99]
    reserved_symbols: ["LegacyDirectoryV0"]

  - name: "nlos.sabi.Envelope"
    major: 3
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.legacy_hint"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 14
    introduced_in_epoch: 1
    state: "DEPRECATED"
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    replacement: "NegotiateServiceRequest.capability_filter"
    migration_rule_id: "MIG-SABI-SD-014"
    reason: "replaced by structured capability negotiation"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "RetiredAlphaLookup"
    kind: "METHOD"
    introduced_in_epoch: 1
    state: "SUNSET"
    deprecated_in_epoch: 1
    earliest_sunset_epoch: 3
    sunset_in_epoch: 3
    replacement: "ResolveServiceRequest"
    migration_rule_id: "MIG-SABI-SD-001"
    reason: "retired after completing two-epoch grace window"
"#;

#[test]
fn deprecation_adjudicator_emits_warnings_for_deprecated_and_blocks_sunset_reserved_and_n_minus_3()
{
    // 1. DEPRECATED symbol inside grace window (epoch 3, earliest_sunset 4 -> 1 epoch left) + N-1 route.
    let deprecated_pkg = TempPkgDir::new("deprecated-warn");
    deprecated_pkg.write(
        "package.manifest",
        b"package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
          version = 1.1.0\n\
          target-sabi-epoch = 2.0\n\
          component = app executable app.bin\n\
          sabi-use = nlos.sabi.ServiceDirectory ResolveServiceRequest\n\
          sabi-use = nlos.sabi.ServiceDirectory ResolveServiceRequest.legacy_hint field=14\n",
    );
    deprecated_pkg.write("app.bin", b"binary");

    let warn_report = audit_package_directory_with_yaml(
        deprecated_pkg.path(),
        EVOLVED_EPOCH_3_YAML,
        &AuditOptions::default(),
    )
    .expect("valid epoch 3 yaml");

    assert_eq!(warn_report.verdict, AuditVerdict::PassWithWarnings);
    assert!(warn_report.verdict.is_compliant());
    assert_eq!(warn_report.verdict.exit_code(), 0);
    assert_eq!(warn_report.blocker_count, 0);
    assert_eq!(warn_report.warning_count, 1);
    assert_eq!(
        warn_report.negotiated_route.as_deref(),
        Some("OUT_OF_CORE_COMPAT_SHIM")
    );
    assert_eq!(warn_report.prescriptions.len(), 1);
    let rx = &warn_report.prescriptions[0];
    assert_eq!(rx.lifecycle_state, "DEPRECATED");
    assert_eq!(rx.remaining_grace_epochs, Some(1));
    assert_eq!(
        rx.replacement_symbol.as_deref(),
        Some("NegotiateServiceRequest.capability_filter")
    );
    assert_eq!(rx.migration_rule_id.as_deref(), Some("MIG-SABI-SD-014"));
    assert_eq!(warn_report.warning_receipts.len(), 1);

    // 2. SUNSET symbol, RESERVED symbol, RESERVED field=99, unknown channel, unknown critical ext, and N-3 epoch.
    let blocked_pkg = TempPkgDir::new("sunset-blocked");
    blocked_pkg.write(
        "package.manifest",
        b"package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
          version = 1.1.0\n\
          target-sabi-epoch = 1.0\n\
          component = app executable app.bin\n\
          sabi-use = nlos.sabi.ServiceDirectory RetiredAlphaLookup\n\
          sabi-use = nlos.sabi.ServiceDirectory LegacyDirectoryV0\n\
          sabi-use = nlos.sabi.ServiceDirectory ResolveServiceRequest field=99\n\
          sabi-use = nlos.sabi.UnknownChannel FooMessage\n\
          sabi-ext = nlos.sabi.Envelope critical 777\n",
    );
    blocked_pkg.write("app.bin", b"binary");

    let blocked_report = audit_package_directory_with_yaml(
        blocked_pkg.path(),
        EVOLVED_EPOCH_3_YAML,
        &AuditOptions {
            host_epoch: Some(4), // target-sabi-epoch 1.0 is N-3 -> outside N/N-1/N-2!
            max_backward_epochs: None,
            known_critical_extensions: Vec::new(),
        },
    )
    .expect("valid epoch 3 yaml");

    assert_eq!(blocked_report.verdict, AuditVerdict::Blocked);
    assert_eq!(blocked_report.verdict.exit_code(), 2);
    // Expect 6 blockers: N-3 target-sabi-epoch, RetiredAlphaLookup (SUNSET),
    // LegacyDirectoryV0 (RESERVED), field=99 (RESERVED), UnknownChannel, critical ext 777.
    assert_eq!(
        blocked_report.blocker_count,
        6,
        "{}",
        blocked_report.render_text()
    );
    assert_eq!(blocked_report.prescriptions.len(), 3);
}

#[test]
fn cli_binary_enforces_exit_codes_0_1_2_and_3() {
    let bin = env!("CARGO_BIN_EXE_nlos-migrate");
    let root = workspace_root();
    let starter = root.join("examples/starter-assistant-app/package");

    // Exit 0: compliant static check.
    let ok_check = Command::new(bin)
        .args(["check", starter.to_str().expect("utf8 path"), "--json"])
        .output()
        .expect("run check");
    assert_eq!(ok_check.status.code(), Some(0));
    let ok_stdout = String::from_utf8_lossy(&ok_check.stdout);
    assert!(ok_stdout.contains("\"verdict\": \"PASS\""));

    // Exit 0: atomic rollback drill passes invariants.
    let ok_drill = Command::new(bin)
        .args([
            "drill",
            "--mode",
            "window",
            "--inject-health-fail",
            "--json",
        ])
        .output()
        .expect("run window rollback drill");
    assert_eq!(ok_drill.status.code(), Some(0));
    let drill_stdout = String::from_utf8_lossy(&ok_drill.stdout);
    assert!(drill_stdout.contains("\"outcome\": \"ROLLED_BACK_ATOMICALLY\""));

    // Exit 1: CLI usage error.
    let bad_cli = Command::new(bin)
        .args(["check", "--nonexistent-flag"])
        .output()
        .expect("run bad cli");
    assert_eq!(bad_cli.status.code(), Some(1));

    // Exit 2: static check BLOCKER violation.
    let bad_pkg = TempPkgDir::new("cli-exit-2");
    bad_pkg.write(
        "package.manifest",
        b"package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
          version = 1.0.0\n\
          component = drv driver drv.wasm\n",
    );
    bad_pkg.write("drv.wasm", b"\0asm\x01\0\0\0");
    let blocked_check = Command::new(bin)
        .args(["check", bad_pkg.path().to_str().expect("utf8"), "--json"])
        .output()
        .expect("run blocked check");
    assert_eq!(blocked_check.status.code(), Some(2));

    // Exit 3: migration sandbox blocked by safety gate (`--omit-z4-consent` and `--disguise-destructive`).
    let gate_z4 = Command::new(bin)
        .args([
            "drill",
            "--mode",
            "irreversible",
            "--omit-z4-consent",
            "--json",
        ])
        .output()
        .expect("run omit z4 drill");
    assert_eq!(gate_z4.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&gate_z4.stdout).contains("MISSING_Z4_TRUSTED_UI_CONSENT"));

    let gate_disguise = Command::new(bin)
        .args([
            "drill",
            "--mode",
            "window",
            "--disguise-destructive",
            "--json",
        ])
        .output()
        .expect("run disguised destructive drill");
    assert_eq!(gate_disguise.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&gate_disguise.stdout).contains("DISGUISED_IRREVERSIBLE_MIGRATION")
    );
}
