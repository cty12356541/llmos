//! Subprocess CLI integration tests for `nlos-migrate` (`Stage E W78 / E3`),
//! pinning exit codes `0..=5` and deterministic `--json` output.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static CLI_TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempCliDir {
    path: PathBuf,
}

impl TempCliDir {
    fn new(tag: &str) -> Self {
        let seq = CLI_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-migrate-cli-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("mkdir cli temp dir");
        Self { path }
    }
}

impl Drop for TempCliDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nlos-migrate"))
        .args(args)
        .output()
        .expect("spawn nlos-migrate binary")
}

#[test]
fn cli_exit_0_and_deterministic_json_on_scan_and_prescribe() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let sample_pkg = repo_root.join("examples/sample-app/package");
    let sample_str = sample_pkg.to_str().expect("utf8 path");

    let scan_a = run_cli(&["scan", sample_str, "--json"]);
    let scan_b = run_cli(&["scan", sample_str, "--json"]);
    assert_eq!(scan_a.status.code(), Some(0));
    assert_eq!(scan_a.stdout, scan_b.stdout);
    let stdout_text = String::from_utf8(scan_a.stdout).expect("utf8 json");
    assert!(stdout_text.contains("\"passed\": true"));

    let presc_a = run_cli(&["prescribe", sample_str, "--json"]);
    let presc_b = run_cli(&["prescribe", sample_str, "--json"]);
    assert_eq!(presc_a.status.code(), Some(0));
    assert_eq!(presc_a.stdout, presc_b.stdout);
    let presc_text = String::from_utf8(presc_a.stdout).expect("utf8 json");
    assert!(presc_text.contains("\"posture\": \"native_current_ready\""));
}

#[test]
fn cli_exit_1_on_bad_arguments_and_exit_2_on_corrupt_manifest_or_nlospkg() {
    let no_args = run_cli(&[]);
    assert_eq!(no_args.status.code(), Some(1));

    let bad_flag = run_cli(&["scan", "--unknown-flag"]);
    assert_eq!(bad_flag.status.code(), Some(1));

    let tmp = TempCliDir::new("corrupt");
    let bad_pkg = tmp.path.join("broken.nlospkg");
    fs::write(&bad_pkg, b"not-a-valid-nlospkg-header").expect("write broken pkg");
    let corrupt_out = run_cli(&["scan", bad_pkg.to_str().expect("utf8"), "--json"]);
    assert_eq!(corrupt_out.status.code(), Some(2));

    let bad_man = tmp.path.join("package.manifest");
    fs::write(&bad_man, "package-id = not_hex\nversion = 1.0.0\n").expect("write bad manifest");
    let corrupt_man_out = run_cli(&["prescribe", bad_man.to_str().expect("utf8")]);
    assert_eq!(corrupt_man_out.status.code(), Some(2));
}

#[test]
fn cli_exit_3_on_sunset_epoch_or_floating_dependency_selector() {
    let tmp = TempCliDir::new("exit3");
    fs::write(
        tmp.path.join("package.manifest"),
        "package-id = 9a1c7d3e5f0b2a4c6d8e0f1a2b3c4d5e\n\
         version = 1.0.0\n\
         target-sabi-epoch = 1\n\
         dependency = skill summarize:latest\n",
    )
    .expect("write manifest");

    let out = run_cli(&[
        "prescribe",
        tmp.path.to_str().expect("utf8"),
        "--host-epoch",
        "4",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(3));
    let body = String::from_utf8(out.stdout).expect("utf8");
    assert!(body.contains("\"posture\": \"source_migration_required\""));
    assert!(body.contains("pin_dependency_selector"));
}

#[test]
fn cli_exit_4_on_disguised_irreversible_or_missing_z4_governance_and_exit_0_when_authorized() {
    // Disguised DROP COLUMN under window mode -> exit 4.
    let disguised = run_cli(&[
        "drill",
        "--mode",
        "window",
        "--sql",
        "ALTER TABLE user_documents DROP COLUMN body;",
        "--json",
    ]);
    assert_eq!(disguised.status.code(), Some(4));

    // Irreversible without backup/Z4 consent -> exit 4.
    let missing_gov = run_cli(&["drill", "--mode", "irreversible", "--json"]);
    assert_eq!(missing_gov.status.code(), Some(4));

    // Irreversible with verified backup + valid Z4 consent -> exit 0.
    let authorized = run_cli(&[
        "drill",
        "--mode",
        "irreversible",
        "--with-verified-backup",
        "--with-z4-consent",
        "--json",
    ]);
    assert_eq!(authorized.status.code(), Some(0));
    let auth_body = String::from_utf8(authorized.stdout).expect("utf8");
    assert!(auth_body.contains("\"final_generation\": 2"));
}

#[test]
fn cli_exit_5_on_unaccepted_health_failure_and_exit_0_on_verified_rollback_and_crash_drills() {
    // Health failure without --expect-rollback exits 5 while still emitting rollback proof.
    let health_fail = run_cli(&[
        "drill",
        "--mode",
        "snapshot-rollback",
        "--inject-health-failure",
        "--json",
    ]);
    assert_eq!(health_fail.status.code(), Some(5));
    let fail_body = String::from_utf8(health_fail.stdout).expect("utf8");
    assert!(fail_body.contains("\"outcome\": \"rolled_back_on_health_failure\""));
    assert!(fail_body.contains("\"userdata_matches_pre_upgrade\": true"));

    // Positive rollback verification drill with --expect-rollback exits 0.
    let rollback_ok = run_cli(&[
        "drill",
        "--mode",
        "snapshot-rollback",
        "--inject-health-failure",
        "--expect-rollback",
        "--json",
    ]);
    assert_eq!(rollback_ok.status.code(), Some(0));

    // Mid-step crash recovery drill exits 0 and converges to generation 2.
    let crash_ok = run_cli(&[
        "drill",
        "--mode",
        "snapshot-rollback",
        "--inject-crash-after-step",
        "1",
        "--json",
    ]);
    assert_eq!(crash_ok.status.code(), Some(0));
    let crash_body = String::from_utf8(crash_ok.stdout).expect("utf8");
    assert!(crash_body.contains("\"outcome\": \"recovered_crash_and_activated_generation_2\""));
}
