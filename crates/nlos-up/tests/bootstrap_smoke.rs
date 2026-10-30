//! Integration smoke tests for `nlos-up` trial bootstrap, daemon assembly, and CLI modes.

use std::fs;
use std::path::{Path, PathBuf};
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

fn read_full_http_request(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read as _;

    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 1024];
    let mut header_end: Option<usize> = None;
    let mut expected_body_len = 0_usize;

    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if header_end.is_none()
                    && let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    let end = pos + 4;
                    header_end = Some(end);
                    let header_str = String::from_utf8_lossy(&buf[..end]);
                    for line in header_str.lines() {
                        if let Some((k, v)) = line.split_once(':')
                            && k.trim().eq_ignore_ascii_case("content-length")
                            && let Ok(len) = v.trim().parse::<usize>()
                        {
                            expected_body_len = len;
                        }
                    }
                }
                if let Some(end) = header_end
                    && buf.len() >= end.saturating_add(expected_body_len)
                {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn write_http_json_response(
    stream: &mut std::net::TcpStream,
    status_line: &str,
    extra_headers: &[(&str, &str)],
    json_body: &str,
) {
    use std::io::Write as _;

    let body_bytes = json_body.as_bytes();
    let mut response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body_bytes.len()
    );
    for (k, v) in extra_headers {
        response.push_str(k);
        response.push_str(": ");
        response.push_str(v);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body_bytes);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

fn write_synthetic_keypool_config(path: &Path, port: u16) {
    let config_json = format!(
        r#"{{
  "listen": {{ "host": "127.0.0.1", "port": {port} }},
  "upstreamBase": "https://open.bigmodel.cn",
  "egressKey": "SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK",
  "allowModels": ["glm-5.3", "glm-5.3-flash"],
  "groups": [
    {{
      "name": "glm-coding",
      "allowModels": ["glm-5.3", "glm-5.3-flash"],
      "keys": [
        {{ "id": "mock-sensitive-owner-name", "key": "SYNTHETIC_UPSTREAM_KEY_DO_NOT_LEAK" }}
      ]
    }}
  ]
}}"#
    );
    fs::write(path, config_json).expect("write synthetic glm-keypool config.json");
}

#[test]
fn cli_pool_status_online_and_offline_redacts_secrets_and_slot_names() {
    let temp = TempTrialDir::new("cli-pool-status");
    fs::create_dir_all(&temp.0).expect("create temp dir");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock keypool");
    let port = listener.local_addr().expect("mock keypool addr").port();
    let config_path = temp.0.join("config.json");
    write_synthetic_keypool_config(&config_path, port);

    let server_thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept status probe");
        let req_text = read_full_http_request(&mut stream);
        assert!(
            req_text.starts_with("GET /__pool__/status HTTP/1.1"),
            "unexpected status request: {req_text}"
        );
        assert!(
            req_text.contains("Authorization: Bearer SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK"),
            "missing egress bearer header"
        );
        let status_body = r#"{
  "strategy": "sticky",
  "totalKeys": 3,
  "activeKeys": 3,
  "activeSessions": 2,
  "totalRequests": 14,
  "okRequests": 13,
  "failedRequests": 1,
  "tokensIn": 2048,
  "tokensOut": 512,
  "keys": [
    { "id": "mock-sensitive-owner-name", "masked": "sk-synth-never-leak" }
  ]
}"#;
        write_http_json_response(&mut stream, "200 OK", &[], status_body);
    });

    let bin = env!("CARGO_BIN_EXE_nlos-up");
    let live_out = Command::new(bin)
        .args([
            "pool-status",
            "--keypool-config",
            &config_path.display().to_string(),
            "--require-live",
        ])
        .output()
        .expect("run nlos-up pool-status --require-live");
    server_thread.join().expect("join mock status server");

    assert!(
        live_out.status.success(),
        "pool-status stderr: {}",
        String::from_utf8_lossy(&live_out.stderr)
    );
    let live_stdout = String::from_utf8_lossy(&live_out.stdout);
    assert!(live_stdout.contains("NLOS_UP_POOL_STATUS configured=true reachable=true"));
    assert!(live_stdout.contains("openai_base_path     = /api/coding/paas/v4"));
    assert!(live_stdout.contains("default_model        = glm-5.3"));
    assert!(live_stdout.contains("fast_intent_model    = glm-5.3-flash"));
    assert!(live_stdout.contains("credential_present   = true [REDACTED]"));
    assert!(live_stdout.contains("isolation_tag        = PROVIDER_ISOLATION_UNVERIFIED"));
    assert!(live_stdout.contains("pool_keys            = 3/3"));
    for forbidden in [
        "SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK",
        "SYNTHETIC_UPSTREAM_KEY_DO_NOT_LEAK",
        "mock-sensitive-owner-name",
        "sk-synth-never-leak",
    ] {
        assert!(
            !live_stdout.contains(forbidden),
            "pool-status leaked sensitive string '{forbidden}':\n{live_stdout}"
        );
    }

    // Bind and immediately drop a socket to obtain a guaranteed closed loopback port.
    let closed_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind closed port");
    let closed_port = closed_listener.local_addr().expect("closed addr").port();
    drop(closed_listener);
    let offline_cfg = temp.0.join("offline-config.json");
    write_synthetic_keypool_config(&offline_cfg, closed_port);

    // Without --require-live: graceful reachable=false report with exit code 0.
    let graceful_out = Command::new(bin)
        .args([
            "pool-status",
            "--keypool-config",
            &offline_cfg.display().to_string(),
        ])
        .output()
        .expect("run nlos-up pool-status offline graceful");
    assert!(graceful_out.status.success());
    let graceful_stdout = String::from_utf8_lossy(&graceful_out.stdout);
    assert!(graceful_stdout.contains("NLOS_UP_POOL_STATUS configured=true reachable=false"));
    assert!(graceful_stdout.contains("unreachable_reason   ="));
    assert!(!graceful_stdout.contains("SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK"));

    // With --require-live: must fail closed with non-zero exit status.
    let strict_out = Command::new(bin)
        .args([
            "pool-status",
            "--keypool-config",
            &offline_cfg.display().to_string(),
            "--require-live",
        ])
        .output()
        .expect("run nlos-up pool-status offline --require-live");
    assert!(!strict_out.status.success());
}

fn spawn_mock_chat_and_intent_server(
    listener: std::net::TcpListener,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // Turn 1: `nlos-up chat`
        let (mut chat_stream, _) = listener.accept().expect("accept chat request");
        let chat_req = read_full_http_request(&mut chat_stream);
        assert!(
            chat_req.starts_with("POST /api/coding/paas/v4/chat/completions HTTP/1.1"),
            "unexpected chat request: {chat_req}"
        );
        assert!(chat_req.contains("x-pool-session: trial-sess-01"));
        let chat_resp = r#"{
  "id": "chatcmpl-trial-01",
  "model": "glm-5.3",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Trial KeyPool handshake verified cleanly."
      }
    }
  ],
  "usage": {
    "prompt_tokens": 18,
    "completion_tokens": 24,
    "completion_tokens_details": { "reasoning_tokens": 6 },
    "total_tokens": 48
  }
}"#;
        write_http_json_response(
            &mut chat_stream,
            "200 OK",
            &[
                ("x-pool-group", "glm-coding"),
                ("x-pool-key", "mock-sensitive-owner-name"),
            ],
            chat_resp,
        );

        // Turn 2: `nlos-up intent` with destructive verb ("删除") where model attempts LOSSLESS downgrade.
        let (mut intent_stream, _) = listener.accept().expect("accept intent request");
        let intent_req = read_full_http_request(&mut intent_stream);
        assert!(
            intent_req.starts_with("POST /api/coding/paas/v4/chat/completions HTTP/1.1"),
            "unexpected intent request: {intent_req}"
        );
        let intent_resp = r#"{
  "id": "chatcmpl-trial-02",
  "model": "glm-5.3-flash",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "{\"summary\":\"清理试验缓存制品\",\"target_domain\":\"artifact\",\"undo_class\":\"LOSSLESS\",\"needs_clarification\":false,\"clarification_reason\":\"\",\"steps\":[\"定位试验缓存目录\",\"执行清理动作\"]}"
      }
    }
  ],
  "usage": {
    "prompt_tokens": 30,
    "completion_tokens": 22,
    "total_tokens": 52
  }
}"#;
        write_http_json_response(
            &mut intent_stream,
            "200 OK",
            &[
                ("x-pool-group", "glm-coding"),
                ("x-pool-key", "mock-sensitive-owner-name"),
            ],
            intent_resp,
        );
    })
}

fn assert_compiled_intent_and_durable_budget(intent_stdout: &str, root_dir: &Path) {
    let parsed_intent =
        nlos_driver_llm_http::JsonValue::parse(intent_stdout).expect("valid intent JSON output");
    assert_eq!(
        parsed_intent
            .get("undo_class")
            .and_then(nlos_driver_llm_http::JsonValue::as_str),
        Some("IRREVERSIBLE")
    );
    assert_eq!(
        parsed_intent
            .get("needs_clarification")
            .and_then(nlos_driver_llm_http::JsonValue::as_bool),
        Some(true)
    );
    assert_eq!(
        parsed_intent
            .get("safety_escalated")
            .and_then(nlos_driver_llm_http::JsonValue::as_bool),
        Some(true)
    );
    assert_eq!(
        parsed_intent
            .get("isolation_tag")
            .and_then(nlos_driver_llm_http::JsonValue::as_str),
        Some("PROVIDER_ISOLATION_UNVERIFIED")
    );
    let receipt_obj = parsed_intent.get("receipt").expect("intent receipt object");
    assert_eq!(
        receipt_obj
            .get("sequence")
            .and_then(nlos_driver_llm_http::JsonValue::as_u64),
        Some(2)
    );
    assert_eq!(
        receipt_obj
            .get("cumulative_total_tokens")
            .and_then(nlos_driver_llm_http::JsonValue::as_u64),
        Some(100)
    );
    assert_eq!(
        receipt_obj
            .get("cumulative_api_calls")
            .and_then(nlos_driver_llm_http::JsonValue::as_u64),
        Some(2)
    );

    // Re-open `<root>/resource` directly and confirm durable 5D accounting against `<root>/trial-account.txt`.
    let marker_hex = fs::read_to_string(root_dir.join(TRIAL_ACCOUNT_FILE))
        .expect("read trial-account.txt")
        .trim()
        .to_owned();
    let mut acct_bytes = [0_u8; 16];
    for (idx, slot) in acct_bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&marker_hex[2 * idx..2 * idx + 2], 16).expect("account hex");
    }
    let account_id = nlos_types::ResourceAccountId::from_bytes(acct_bytes);
    let resource =
        ResourceAuthority::open(root_dir.join("resource")).expect("open trial resource authority");
    let durable_budget = resource
        .inspect_llm_token_budget(account_id)
        .expect("inspect durable 5D budget");
    assert_eq!(durable_budget.consumed_api_calls, 2);
    assert_eq!(durable_budget.consumed_prompt_tokens, 48);
    assert_eq!(durable_budget.consumed_total_tokens, 100);
}

#[test]
fn cli_chat_and_intent_charge_trial_5d_budget_with_monotonic_receipts_and_safety_escalation() {
    let temp = TempTrialDir::new("cli-chat-intent");
    let root_dir = temp.0.join("trial-root");
    fs::create_dir_all(&temp.0).expect("create temp dir");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock keypool");
    let port = listener.local_addr().expect("mock keypool addr").port();
    let config_path = temp.0.join("config.json");
    write_synthetic_keypool_config(&config_path, port);

    let server_thread = spawn_mock_chat_and_intent_server(listener);

    let bin = env!("CARGO_BIN_EXE_nlos-up");
    let expected_slot_fp = nlos_driver_llm_http::sha256_hex16("mock-sensitive-owner-name");

    let chat_out = Command::new(bin)
        .args([
            "chat",
            "--root",
            &root_dir.display().to_string(),
            "--keypool-config",
            &config_path.display().to_string(),
            "--max-tokens",
            "256",
            "--session",
            "trial-sess-01",
            "--prompt",
            "Verify local KeyPool trial bridge",
        ])
        .output()
        .expect("run nlos-up chat");
    assert!(
        chat_out.status.success(),
        "chat stderr: {}",
        String::from_utf8_lossy(&chat_out.stderr)
    );
    let chat_stdout = String::from_utf8_lossy(&chat_out.stdout);
    assert!(chat_stdout.contains("NLOS_UP_CHAT status=ok"));
    assert!(chat_stdout.contains("model                   = glm-5.3"));
    assert!(chat_stdout.contains("isolation_tag           = PROVIDER_ISOLATION_UNVERIFIED"));
    assert!(chat_stdout.contains("served_group            = glm-coding"));
    assert!(
        chat_stdout.contains(&format!("served_slot_fingerprint = {expected_slot_fp}")),
        "expected 16-hex slot fingerprint in:\n{chat_stdout}"
    );
    assert!(chat_stdout.contains("sequence                = 1"));
    assert!(chat_stdout.contains("cumulative_total_tokens = 48"));
    assert!(chat_stdout.contains("cumulative_api_calls    = 1"));
    assert!(chat_stdout.contains("Trial KeyPool handshake verified cleanly."));
    assert!(!chat_stdout.contains("mock-sensitive-owner-name"));
    assert!(!chat_stdout.contains("SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK"));

    let intent_out = Command::new(bin)
        .args([
            "intent",
            "--root",
            &root_dir.display().to_string(),
            "--keypool-config",
            &config_path.display().to_string(),
            "--query",
            "删除过期试验缓存制品",
        ])
        .output()
        .expect("run nlos-up intent");
    server_thread.join().expect("join mock chat+intent server");

    assert!(
        intent_out.status.success(),
        "intent stderr: {}",
        String::from_utf8_lossy(&intent_out.stderr)
    );
    let intent_stdout = String::from_utf8_lossy(&intent_out.stdout);
    assert!(!intent_stdout.contains("mock-sensitive-owner-name"));
    assert!(!intent_stdout.contains("SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK"));

    assert_compiled_intent_and_durable_budget(&intent_stdout, &root_dir);
}
