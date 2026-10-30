//! Integration smoke tests for `nlos-up` trial bootstrap, daemon assembly, and CLI modes.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ed25519_dalek::{Signer as _, SigningKey};
use nlos_resource::ResourceAuthority;
use nlos_system_control::auth::dispatch_over_authenticated_socket;
use nlos_system_control::control::{ControlCommand, dispatch_over_plain_endpoint, receipt_to_hex};
use nlos_system_control::daemon::{assemble, serve_authenticated_endpoint, serve_plain_endpoint};
use nlos_types::PackageId;
use nlos_up::{
    BootstrapOptions, DEFAULT_TRIAL_INITIAL_CREDIT, DEFAULT_TRIAL_MAX_API_CALLS,
    DEFAULT_TRIAL_MAX_COMPLETION_TOKENS, DEFAULT_TRIAL_MAX_COST_MICROCREDITS,
    DEFAULT_TRIAL_MAX_PROMPT_TOKENS, DEFAULT_TRIAL_MAX_TOTAL_TOKENS, KeyOrigin,
    SampleAppSeedStatus, TRIAL_ACCOUNT_FILE, bootstrap_trial_root, daemon_options_for_receipt,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

struct TempTrialDir(PathBuf);

impl TempTrialDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nlos-up-smoke-{tag}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for TempTrialDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn parse_seed32(hex: &str) -> [u8; 32] {
    let trimmed = hex.trim();
    assert_eq!(trimmed.len(), 64);
    let mut out = [0_u8; 32];
    for (idx, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&trimmed[2 * idx..2 * idx + 2], 16).expect("valid hex byte");
    }
    out
}

#[test]
fn bootstrap_trial_root_is_idempotent_across_replays_and_seeds_llm_budget() {
    let temp = TempTrialDir::new("idempotent");
    let options = BootstrapOptions::new()
        .with_root(&temp.0)
        .without_default_keypool_probe();

    let first = bootstrap_trial_root(&options).expect("initial bootstrap");
    assert_eq!(first.key_origin, KeyOrigin::GeneratedDevTrial);
    assert!(!first.principal_replayed);
    assert!(!first.llm_budget_replayed);
    assert_eq!(
        first.account_record.initial_credit,
        DEFAULT_TRIAL_INITIAL_CREDIT
    );
    assert_eq!(
        first.llm_budget.limits.max_prompt_tokens,
        DEFAULT_TRIAL_MAX_PROMPT_TOKENS
    );
    assert_eq!(
        first.llm_budget.limits.max_completion_tokens,
        DEFAULT_TRIAL_MAX_COMPLETION_TOKENS
    );
    assert_eq!(
        first.llm_budget.limits.max_total_tokens,
        DEFAULT_TRIAL_MAX_TOTAL_TOKENS
    );
    assert_eq!(
        first.llm_budget.limits.max_api_calls,
        DEFAULT_TRIAL_MAX_API_CALLS
    );
    assert_eq!(
        first.llm_budget.limits.max_cost_microcredits,
        DEFAULT_TRIAL_MAX_COST_MICROCREDITS
    );

    let key_content_first = fs::read_to_string(&first.key_file).expect("read operator key");
    let trimmed_key = key_content_first.trim();
    assert_eq!(trimmed_key.len(), 64);
    assert!(
        trimmed_key
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    );

    let recorded_account =
        fs::read_to_string(temp.0.join(TRIAL_ACCOUNT_FILE)).expect("read trial-account.txt");
    assert_eq!(recorded_account.trim(), first.account_id_hex);

    // Replay against the exact same root: key, principal, and account_id must stay identical.
    let second = bootstrap_trial_root(&options).expect("replayed bootstrap");
    assert_eq!(second.key_origin, KeyOrigin::ReusedExisting);
    assert!(second.principal_replayed);
    assert!(second.llm_budget_replayed);
    assert_eq!(second.public_key_hex, first.public_key_hex);
    assert_eq!(second.principal_id, first.principal_id);
    assert_eq!(second.principal_hex, first.principal_hex);
    assert_eq!(second.account_id, first.account_id);
    assert_eq!(second.account_id_hex, first.account_id_hex);
    assert_eq!(second.llm_budget, first.llm_budget);

    let key_content_second = fs::read_to_string(&second.key_file).expect("read operator key again");
    assert_eq!(key_content_second, key_content_first);

    // Re-opening `ResourceAuthority` independently confirms no lingering lock and durable budget.
    let resource = ResourceAuthority::open(&second.resource_root).expect("open resource authority");
    let inspected = resource
        .inspect_llm_token_budget(second.account_id)
        .expect("inspect seeded llm token budget");
    assert_eq!(
        inspected.limits.max_total_tokens,
        DEFAULT_TRIAL_MAX_TOTAL_TOKENS
    );
}

#[tokio::test]
async fn bootstrap_trial_root_with_sample_app_assembles_daemon_and_serves_dual_ipc() {
    let temp = TempTrialDir::new("daemon-assemble");
    let options = BootstrapOptions::new()
        .with_root(&temp.0)
        .with_seed_sample_app(true)
        .without_default_keypool_probe();

    let receipt = bootstrap_trial_root(&options).expect("bootstrap with sample app");
    let expected_pkg_id = match &receipt.sample_app {
        SampleAppSeedStatus::Installed { package_id_hex, .. } => package_id_hex.clone(),
        other => panic!("expected sample app to be installed from workspace, got {other:?}"),
    };

    // Replay with sample app enabled must report `AlreadyInstalled`.
    let replayed = bootstrap_trial_root(&options).expect("replay bootstrap with sample app");
    assert!(matches!(
        replayed.sample_app,
        SampleAppSeedStatus::AlreadyInstalled { .. }
    ));

    // Immediately assemble `SystemControlDaemon` over the bootstrapped root: zero SQLite lock conflict!
    let (daemon, endpoints) = assemble(
        daemon_options_for_receipt(&receipt),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble resident daemon after bootstrap");

    assert_eq!(daemon.auth_socket_path, receipt.auth_socket_path);
    assert_eq!(daemon.plain_socket_path, receipt.plain_socket_path);
    assert_eq!(
        daemon.bootstrapped_principal_hex.as_deref(),
        Some(receipt.principal_hex.as_str())
    );
    assert!(daemon.operator_root.is_some());

    // Confirm the assembled daemon observes both the seeded LLM budget and the installed sample app.
    let budget = daemon
        .resource
        .inspect_llm_token_budget(receipt.account_id)
        .expect("daemon sees seeded trial budget");
    assert_eq!(budget.limits.max_api_calls, DEFAULT_TRIAL_MAX_API_CALLS);

    let mut pkg_bytes = [0_u8; 16];
    for (idx, slot) in pkg_bytes.iter_mut().enumerate() {
        *slot =
            u8::from_str_radix(&expected_pkg_id[2 * idx..2 * idx + 2], 16).expect("package id hex");
    }
    let app_view = daemon
        .application
        .inspect_application(PackageId::from_bytes(pkg_bytes))
        .expect("inspect application")
        .expect("sample application present in daemon");
    assert_eq!(app_view.current_installation_generation.get(), 1);

    // Serve both endpoints (Windows Named Pipe / Unix Socket) and verify live IPC round-trips.
    let stop = Arc::new(AtomicBool::new(false));
    let auth_task = tokio::spawn(serve_authenticated_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_authenticated,
        Arc::clone(&stop),
    ));
    let plain_task = tokio::spawn(serve_plain_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_plain,
        Arc::clone(&stop),
    ));

    let seed = parse_seed32(&fs::read_to_string(&receipt.key_file).expect("read seed"));
    let signing_key = SigningKey::from_bytes(&seed);

    let auth_receipt = dispatch_over_authenticated_socket(
        &daemon.auth_socket_path,
        receipt.principal_id,
        |digest| Ok(signing_key.sign(digest).to_bytes()),
        &ControlCommand::InspectHealth,
        None,
        None,
        None,
    )
    .await
    .expect("authenticated IPC InspectHealth round-trip");

    let plain_receipt = dispatch_over_plain_endpoint(
        &daemon.plain_socket_path,
        &ControlCommand::InspectHealth,
        None,
        None,
        None,
    )
    .await
    .expect("plain IPC InspectHealth round-trip");

    assert_eq!(
        receipt_to_hex(&auth_receipt),
        receipt_to_hex(&plain_receipt)
    );

    stop.store(true, Ordering::Relaxed);
    auth_task.abort();
    plain_task.abort();
    let _ = auth_task.await;
    let _ = plain_task.await;
    daemon.stop_worker();
    daemon.stop_materialization();
    #[cfg(unix)]
    {
        let _ = fs::remove_file(&daemon.auth_socket_path);
        let _ = fs::remove_file(&daemon.plain_socket_path);
    }
}

#[test]
fn cli_init_and_env_powershell_posix_succeed_without_secret_leakage() {
    let temp = TempTrialDir::new("cli-env");
    let root_dir = temp.0.join("state");
    let mock_keypool = temp.0.join("mock-keypool-config.json");
    fs::create_dir_all(&temp.0).expect("create temp parent");
    // Synthetic test-only JSON fixture to verify port extraction and zero secret leakage.
    fs::write(
        &mock_keypool,
        r#"{"port": 18317, "egressKey": "SYNTHETIC_DUMMY_EGRESS_NEVER_PRINT", "keys": ["SYNTHETIC_DUMMY_KEY_NEVER_PRINT"]}"#,
    )
    .expect("write synthetic mock keypool json");

    let bin = env!("CARGO_BIN_EXE_nlos-up");

    let init_out = Command::new(bin)
        .args([
            "init",
            "--root",
            &root_dir.display().to_string(),
            "--keypool-config",
            &mock_keypool.display().to_string(),
        ])
        .output()
        .expect("run nlos-up init");
    assert!(
        init_out.status.success(),
        "init stderr: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    let init_stdout = String::from_utf8_lossy(&init_out.stdout);
    assert!(init_stdout.contains("NLOS_UP_INIT status=ok"));
    assert!(init_stdout.contains("http://127.0.0.1:18317"));
    assert!(!init_stdout.contains("SYNTHETIC_DUMMY"));

    let ps_out = Command::new(bin)
        .args([
            "env",
            "--root",
            &root_dir.display().to_string(),
            "--shell",
            "powershell",
            "--keypool-config",
            &mock_keypool.display().to_string(),
        ])
        .output()
        .expect("run nlos-up env powershell");
    assert!(
        ps_out.status.success(),
        "env powershell stderr: {}",
        String::from_utf8_lossy(&ps_out.stderr)
    );
    let ps_stdout = String::from_utf8_lossy(&ps_out.stdout);
    for required in [
        "$env:LLMOS_DESKTOP_SOCKET =",
        "$env:LLMOS_DESKTOP_PRINCIPAL =",
        "$env:LLMOS_DESKTOP_KEY_FILE =",
        "$env:LLMOS_DESKTOP_CLI_SOCKET =",
        "$env:LLMOS_DESKTOP_RESOURCE_ROOT =",
        "$env:LLMOS_DESKTOP_APPLICATION_ROOT =",
        "$env:LLMOS_KEYPOOL_CONFIG =",
    ] {
        assert!(
            ps_stdout.contains(required),
            "missing {required} in powershell output:\n{ps_stdout}"
        );
    }
    assert!(!ps_stdout.contains("SYNTHETIC_DUMMY"));

    let posix_out = Command::new(bin)
        .args([
            "env",
            "--root",
            &root_dir.display().to_string(),
            "--shell",
            "posix",
            "--no-keypool-probe",
        ])
        .output()
        .expect("run nlos-up env posix");
    assert!(
        posix_out.status.success(),
        "env posix stderr: {}",
        String::from_utf8_lossy(&posix_out.stderr)
    );
    let posix_stdout = String::from_utf8_lossy(&posix_out.stdout);
    for required in [
        "export LLMOS_DESKTOP_SOCKET=",
        "export LLMOS_DESKTOP_PRINCIPAL=",
        "export LLMOS_DESKTOP_KEY_FILE=",
        "export LLMOS_DESKTOP_CLI_SOCKET=",
        "export LLMOS_DESKTOP_RESOURCE_ROOT=",
        "export LLMOS_DESKTOP_APPLICATION_ROOT=",
    ] {
        assert!(
            posix_stdout.contains(required),
            "missing {required} in posix output:\n{posix_stdout}"
        );
    }
}

#[test]
fn cli_desktop_dry_run_verifies_embedded_daemon_and_env_handoff() {
    let temp = TempTrialDir::new("cli-dry-run");
    let root_dir = temp.0.join("state");
    let mock_keypool = temp.0.join("mock-keypool-config.json");
    fs::create_dir_all(&temp.0).expect("create temp parent");
    fs::write(
        &mock_keypool,
        r#"{"port": 18317, "egressKey": "SYNTHETIC_DUMMY_EGRESS_NEVER_PRINT", "keys": ["SYNTHETIC_DUMMY_KEY_NEVER_PRINT"]}"#,
    )
    .expect("write synthetic mock keypool json");

    let bin = env!("CARGO_BIN_EXE_nlos-up");
    let dry_out = Command::new(bin)
        .args([
            "desktop",
            "--root",
            &root_dir.display().to_string(),
            "--dry-run",
            "--keypool-config",
            &mock_keypool.display().to_string(),
        ])
        .output()
        .expect("run nlos-up desktop --dry-run");
    assert!(
        dry_out.status.success(),
        "desktop --dry-run stderr: {}",
        String::from_utf8_lossy(&dry_out.stderr)
    );
    let dry_stdout = String::from_utf8_lossy(&dry_out.stdout);
    assert!(dry_stdout.contains("READY service=system_control"));
    assert!(dry_stdout.contains("DRY_RUN desktop_dir="));
    for required in [
        "ENV LLMOS_DESKTOP_SOCKET=",
        "ENV LLMOS_DESKTOP_PRINCIPAL=",
        "ENV LLMOS_DESKTOP_KEY_FILE=",
        "ENV LLMOS_DESKTOP_CLI_SOCKET=",
        "ENV LLMOS_DESKTOP_RESOURCE_ROOT=",
        "ENV LLMOS_DESKTOP_APPLICATION_ROOT=",
        "ENV LLMOS_KEYPOOL_CONFIG=",
    ] {
        assert!(
            dry_stdout.contains(required),
            "missing {required} in dry-run output:\n{dry_stdout}"
        );
    }
    assert!(dry_stdout.contains("STOPPED service=system_control mode=dry-run"));
    assert!(!dry_stdout.contains("SYNTHETIC_DUMMY"));
}
