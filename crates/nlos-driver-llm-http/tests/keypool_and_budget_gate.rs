//! Deterministic local closed-loop verification suite for `nlos-driver-llm-http`
//! (`W77-PRE-B` / `E0`).
//!
//! All tests bind ephemeral `127.0.0.1:0` mock `HTTP/1.1` TCP servers and
//! isolated temporary directories. Zero tests connect to host port `8317` or
//! read any real host `~/glm-keypool/config.json` (`[PD-COMMIT-004]`, `DRV-CRED-001`).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_driver_llm_http::{
    DirectKeyPoolConfig, DirectKeyPoolScheduler, DirectKeySlotConfig, HttpClient,
    IntentTargetDomain, IntentUndoClass, KeyPoolDiscoverySource, KeySelectionStrategy,
    LlmChatRequest, LlmDriverError, LlmEnforcementShim, MeteredBudgetContext,
    PROVIDER_ISOLATION_UNVERIFIED_TAG, REDACTED_CREDENTIAL_MARKER, SecretCredential,
    derive_idempotency_key, derive_operation_id, discover_local_keypool_from_lookup,
    parse_glm_keypool_config_json, sha256_hex16,
};
use nlos_resource::{
    CreateAccountRequest, CreateLlmTokenBudgetRequest, LlmTokenLimits, ResourceAuthority,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

const SAMPLE_V2_CONFIG_JSON: &str = r#"{
    "listen": { "host": "0.0.0.0", "port": 18317 },
    "egressKey": "synthetic-egress-token-do-not-leak",
    "upstreamBase": "https://open.bigmodel.cn",
    "allowModels": ["glm-5.3", "glm-5.3-flash"],
    "keys": [
        { "id": "slot-1", "key": "synthetic-upstream-secret-alpha" },
        { "id": "slot-2", "key": "synthetic-upstream-secret-beta" }
    ],
    "groups": [
        {
            "name": "xai",
            "allowModels": ["grok-3-mini", "glm-5.3-flash"],
            "keys": [
                { "id": "slot-3", "key": "synthetic-upstream-secret-gamma" }
            ]
        }
    ]
}"#;

struct ScopedTempDir {
    path: PathBuf,
}

impl ScopedTempDir {
    fn new(prefix: &str) -> Self {
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir_name = format!(
            "nlos-llm-http-{prefix}-{}-{nanos}-{seq}",
            std::process::id()
        );
        let path = std::env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&path).expect("create isolated temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScopedTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Debug)]
struct CapturedWireRequest {
    request_line: String,
    headers: BTreeMap<String, String>,
    body: String,
}

#[derive(Clone, Debug)]
enum MockBodyFraming {
    ContentLength,
    Chunked,
    EofClose,
}

#[derive(Clone, Debug)]
struct MockWireResponse {
    status_code: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: String,
    framing: MockBodyFraming,
}

async fn read_incoming_http_request(stream: &mut TcpStream) -> CapturedWireRequest {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk).await.expect("mock server read");
        assert!(n > 0, "unexpected client EOF before HTTP headers completed");
        buf.extend_from_slice(&chunk[..n]);
    };

    let head_str = std::str::from_utf8(&buf[..header_end]).expect("UTF-8 request headers");
    let mut lines = head_str.split("\r\n");
    let request_line = lines.next().unwrap_or("").to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }

    let content_len = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buf.len() < body_start + content_len {
        let n = stream
            .read(&mut chunk)
            .await
            .expect("mock server body read");
        assert!(n > 0, "unexpected client EOF during request body");
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = std::str::from_utf8(&buf[body_start..body_start + content_len])
        .expect("UTF-8 request body")
        .to_owned();

    CapturedWireRequest {
        request_line,
        headers,
        body,
    }
}

async fn write_mock_http_response(stream: &mut TcpStream, resp: &MockWireResponse) {
    let mut raw = format!("HTTP/1.1 {} {}\r\n", resp.status_code, resp.reason);
    for (k, v) in &resp.headers {
        let _ = write!(raw, "{k}: {v}\r\n");
    }
    let body_bytes = resp.body.as_bytes();
    match resp.framing {
        MockBodyFraming::ContentLength => {
            let _ = write!(raw, "Content-Length: {}\r\n\r\n", body_bytes.len());
            stream
                .write_all(raw.as_bytes())
                .await
                .expect("write headers");
            stream.write_all(body_bytes).await.expect("write body");
        }
        MockBodyFraming::Chunked => {
            raw.push_str("Transfer-Encoding: chunked\r\n\r\n");
            stream
                .write_all(raw.as_bytes())
                .await
                .expect("write chunked headers");
            let mid = body_bytes.len() / 2;
            let (first, second) = body_bytes.split_at(mid);
            if !first.is_empty() {
                let h1 = format!("{:X};ext=nlos\r\n", first.len());
                stream.write_all(h1.as_bytes()).await.expect("chunk 1 size");
                stream.write_all(first).await.expect("chunk 1 data");
                stream.write_all(b"\r\n").await.expect("chunk 1 crlf");
            }
            if !second.is_empty() {
                let h2 = format!("{:x}\r\n", second.len());
                stream.write_all(h2.as_bytes()).await.expect("chunk 2 size");
                stream.write_all(second).await.expect("chunk 2 data");
                stream.write_all(b"\r\n").await.expect("chunk 2 crlf");
            }
            stream
                .write_all(b"0\r\nX-Trailer-Ignored: ok\r\n\r\n")
                .await
                .expect("terminating chunk");
        }
        MockBodyFraming::EofClose => {
            raw.push_str("Connection: close\r\n\r\n");
            stream
                .write_all(raw.as_bytes())
                .await
                .expect("write eof headers");
            stream.write_all(body_bytes).await.expect("write eof body");
        }
    }
    let _ = stream.shutdown().await;
}

#[test]
fn glm_keypool_v2_config_derivation_and_credential_redaction() {
    let parsed = parse_glm_keypool_config_json(
        SAMPLE_V2_CONFIG_JSON,
        None,
        KeyPoolDiscoverySource::UserHomeDefault,
    )
    .expect("parse glm-keypool v2 config.json");

    assert_eq!(parsed.listen_host, "127.0.0.1");
    assert_eq!(parsed.listen_port, 18317);
    assert_eq!(
        parsed.openai_base_url,
        "http://127.0.0.1:18317/api/coding/paas/v4"
    );
    assert_eq!(
        parsed.anthropic_base_url,
        "http://127.0.0.1:18317/api/anthropic"
    );
    assert_eq!(
        parsed.pool_status_url.as_deref(),
        Some("http://127.0.0.1:18317/__pool__/status")
    );
    assert_eq!(parsed.default_model, "glm-5.3");
    assert_eq!(parsed.fast_intent_model, "glm-5.3-flash");
    assert_eq!(
        parsed.allowed_models,
        vec![
            "glm-5.3".to_owned(),
            "glm-5.3-flash".to_owned(),
            "grok-3-mini".to_owned()
        ]
    );
    assert_eq!(parsed.configured_key_slots, 3);

    let generic_json = r#"{
        "listen": { "host": "::", "port": 19000 },
        "upstreamBase": "https://api.example.invalid"
    }"#;
    let generic_cfg = parse_glm_keypool_config_json(
        generic_json,
        Some("custom-reasoner-v1"),
        KeyPoolDiscoverySource::UserHomeDefault,
    )
    .expect("parse generic upstream config");
    assert_eq!(generic_cfg.listen_host, "127.0.0.1");
    assert_eq!(generic_cfg.openai_base_url, "http://127.0.0.1:19000/v1");
    assert_eq!(generic_cfg.default_model, "custom-reasoner-v1");
    assert_eq!(generic_cfg.fast_intent_model, "custom-reasoner-v1");

    let cred = SecretCredential::new("top-secret-value-999");
    assert_eq!(format!("{cred:?}"), REDACTED_CREDENTIAL_MARKER);
    assert_eq!(format!("{cred}"), REDACTED_CREDENTIAL_MARKER);
    let debug_dump = format!("{parsed:?}");
    assert!(!debug_dump.contains("synthetic-egress-token-do-not-leak"));
    assert!(!debug_dump.contains("synthetic-upstream-secret"));
}

#[test]
fn discover_local_keypool_four_tier_priority_order() {
    let tmp = ScopedTempDir::new("discovery");
    let home_pool_dir = tmp.path().join("fake-home").join("glm-keypool");
    std::fs::create_dir_all(&home_pool_dir).expect("create fake home keypool dir");
    let home_cfg_path = home_pool_dir.join("config.json");
    std::fs::write(&home_cfg_path, SAMPLE_V2_CONFIG_JSON).expect("write fake home config");

    let env_cfg_path = tmp.path().join("env-override-config.json");
    std::fs::write(
        &env_cfg_path,
        r#"{"listen":{"host":"127.0.0.1","port":19000},"upstreamBase":"https://api.example.invalid"}"#,
    )
    .expect("write env config");

    let key_file_path = tmp.path().join("direct-key.txt");
    std::fs::write(&key_file_path, "synthetic-file-key\n").expect("write key file");

    let fake_home_str = tmp
        .path()
        .join("fake-home")
        .to_str()
        .expect("utf8 path")
        .to_owned();
    let d4 = discover_local_keypool_from_lookup(None, |k| match k {
        "USERPROFILE" => Some(fake_home_str.clone()),
        _ => None,
    })
    .expect("discover tier 4");
    assert_eq!(d4.source, KeyPoolDiscoverySource::UserHomeDefault);
    assert_eq!(d4.listen_port, 18317);

    let key_file_str = key_file_path.to_str().expect("utf8 path").to_owned();
    let d3 = discover_local_keypool_from_lookup(None, |k| match k {
        "LLMOS_LLM_BASE_URL" => Some("http://127.0.0.1:11434/v1/".to_owned()),
        "LLMOS_LLM_API_KEY_FILE" => Some(key_file_str.clone()),
        "LLMOS_LLM_MODEL" => Some("qwen3-coder".to_owned()),
        "USERPROFILE" => Some(fake_home_str.clone()),
        _ => None,
    })
    .expect("discover tier 3");
    assert_eq!(d3.source, KeyPoolDiscoverySource::EnvDirectBaseUrl);
    assert_eq!(d3.openai_base_url, "http://127.0.0.1:11434/v1");
    assert_eq!(d3.default_model, "qwen3-coder");
    assert_eq!(d3.configured_key_slots, 1);
    assert!(d3.pool_status_url.is_none());

    let env_cfg_str = env_cfg_path.to_str().expect("utf8 path").to_owned();
    let d2 = discover_local_keypool_from_lookup(None, |k| match k {
        "LLMOS_KEYPOOL_CONFIG" => Some(env_cfg_str.clone()),
        "LLMOS_LLM_BASE_URL" => Some("http://127.0.0.1:11434/v1".to_owned()),
        _ => None,
    })
    .expect("discover tier 2");
    assert_eq!(
        d2.source,
        KeyPoolDiscoverySource::EnvConfigPath(env_cfg_path.clone())
    );
    assert_eq!(d2.listen_port, 19000);

    let d1 = discover_local_keypool_from_lookup(Some(&home_cfg_path), |k| match k {
        "LLMOS_KEYPOOL_CONFIG" => Some(env_cfg_str.clone()),
        _ => None,
    })
    .expect("discover tier 1");
    assert_eq!(
        d1.source,
        KeyPoolDiscoverySource::ExplicitPath(home_cfg_path)
    );
    assert_eq!(d1.listen_port, 18317);
}

#[tokio::test]
async fn pool_status_probe_parses_chunked_transfer_and_redacts_individual_key_labels() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral mock keypool");
    let port = listener.local_addr().expect("local addr").port();

    let server_task = tokio::spawn(async move {
        let (mut stream1, _) = listener.accept().await.expect("accept status");
        let req1 = read_incoming_http_request(&mut stream1).await;
        assert_eq!(req1.request_line, "GET /__pool__/status HTTP/1.1");
        assert_eq!(
            req1.headers.get("authorization").map(String::as_str),
            Some("Bearer mock-egress-secret")
        );

        let status_payload = r#"{
            "strategy": "sticky",
            "totalKeys": 6,
            "activeKeys": 5,
            "activeSessions": 4,
            "totals": {"requests": 240, "ok": 237, "failed": 3, "tokensIn": 128500, "tokensOut": 44200},
            "keys": [{"id": "sensitive-person-name-slot-01", "maskedKey": "sk-sensitive-mask-abcd", "owner": "PersonSensitiveAlpha", "active": true}]
        }"#;
        write_mock_http_response(
            &mut stream1,
            &MockWireResponse {
                status_code: 200,
                reason: "OK",
                headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
                body: status_payload.to_owned(),
                framing: MockBodyFraming::Chunked,
            },
        )
        .await;

        let (mut stream2, _) = listener.accept().await.expect("accept eof");
        let _req2 = read_incoming_http_request(&mut stream2).await;
        write_mock_http_response(
            &mut stream2,
            &MockWireResponse {
                status_code: 200,
                reason: "OK",
                headers: vec![("X-Custom-MixedCase".to_owned(), "verified-eof".to_owned())],
                body: r#"{"eof_ok":true}"#.to_owned(),
                framing: MockBodyFraming::EofClose,
            },
        )
        .await;
    });

    let cfg_json = format!(
        r#"{{"listen":{{"host":"127.0.0.1","port":{port}}},"egressKey":"mock-egress-secret","upstreamBase":"https://open.bigmodel.cn"}}"#
    );
    let shim = LlmEnforcementShim::from_discovered(
        parse_glm_keypool_config_json(&cfg_json, None, KeyPoolDiscoverySource::UserHomeDefault)
            .expect("parse config"),
    );

    let snapshot = shim
        .probe_pool_status()
        .await
        .expect("probe chunked status");
    assert_eq!(snapshot.strategy, "sticky");
    assert_eq!(snapshot.total_keys, 6);
    assert_eq!(snapshot.active_keys, 5);
    assert_eq!(snapshot.active_sessions, 4);
    assert_eq!(snapshot.total_requests, 240);
    assert_eq!(snapshot.ok_requests, 237);
    assert_eq!(snapshot.failed_requests, 3);
    assert_eq!(snapshot.tokens_in, 128_500);
    assert_eq!(snapshot.tokens_out, 44_200);

    let snap_debug = format!("{snapshot:?}");
    assert!(!snap_debug.contains("sensitive-person-name"));
    assert!(!snap_debug.contains("sk-sensitive-mask"));
    assert!(!snap_debug.contains("PersonSensitiveAlpha"));

    let client = HttpClient::with_timeout(Duration::from_secs(5));
    let eof_resp = client
        .get(&format!("http://127.0.0.1:{port}/eof-check"), &[])
        .await
        .expect("EOF response");
    assert_eq!(eof_resp.header("x-custom-mixedcase"), Some("verified-eof"));
    assert_eq!(eof_resp.body_utf8().expect("utf8"), r#"{"eof_ok":true}"#);

    server_task.await.expect("mock server clean exit");
}

async fn spawn_single_metered_chat_mock_server() -> (u16, Arc<AtomicUsize>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral mock server");
    let port = listener.local_addr().expect("local addr").port();
    let server_hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&server_hits);

    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept single allowed call");
        hits_clone.fetch_add(1, Ordering::SeqCst);
        let req = read_incoming_http_request(&mut stream).await;
        assert_eq!(
            req.request_line,
            "POST /api/coding/paas/v4/chat/completions HTTP/1.1"
        );
        assert_eq!(
            req.headers.get("authorization").map(String::as_str),
            Some("Bearer pool-egress-key-xyz")
        );
        assert_eq!(
            req.headers.get("x-pool-session").map(String::as_str),
            Some("desktop-omnibar-sess-42")
        );
        assert!(req.body.contains(r#""model":"glm-5.3""#));
        assert!(req.body.contains(r#""stream":false"#));

        let completion_json = r#"{
            "id": "chatcmpl-mock-001",
            "model": "glm-5.3",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "NLOS ResourceAuthority budget verified.",
                    "reasoning_content": "Checked 5D ceilings and sticky cache session."
                }
            }],
            "usage": {
                "prompt_tokens": 18,
                "completion_tokens": 22,
                "total_tokens": 50,
                "completion_tokens_details": {"reasoning_tokens": 10}
            }
        }"#;
        write_mock_http_response(
            &mut stream,
            &MockWireResponse {
                status_code: 200,
                reason: "OK",
                headers: vec![
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                    ("X-Pool-Group".to_owned(), "default".to_owned()),
                    (
                        "X-Pool-Key".to_owned(),
                        "raw-human-owner-label-secret".to_owned(),
                    ),
                ],
                body: completion_json.to_owned(),
                framing: MockBodyFraming::ContentLength,
            },
        )
        .await;
    });
    (port, server_hits, handle)
}

#[tokio::test]
async fn execute_metered_chat_charges_sqlite_resource_authority_and_fails_closed_when_exhausted() {
    let (port, server_hits, server_task) = spawn_single_metered_chat_mock_server().await;

    let tmp = ScopedTempDir::new("budget-gate");
    let resource = ResourceAuthority::open(tmp.path().join("resource")).expect("open SQLite");
    let account = resource
        .create_account(CreateAccountRequest {
            initial_credit: 10_000,
            idempotency_key: derive_idempotency_key(b"test/account", &[b"acct-1"]),
            created_at_ms: 1_700_000_000_000,
        })
        .expect("create account");
    resource
        .create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account.account_id,
            limits: LlmTokenLimits {
                max_prompt_tokens: 100,
                max_completion_tokens: 100,
                max_total_tokens: 50,
                max_api_calls: 1,
                max_cost_microcredits: 1_000,
            },
            idempotency_key: derive_idempotency_key(b"test/budget", &[b"bud-1"]),
            created_at_ms: 1_700_000_000_000,
        })
        .expect("create 5D LLM token budget");

    let cfg_json = format!(
        r#"{{"listen":{{"host":"127.0.0.1","port":{port}}},"egressKey":"pool-egress-key-xyz","upstreamBase":"https://open.bigmodel.cn","allowModels":["glm-5.3","glm-5.3-flash"]}}"#
    );
    let shim = LlmEnforcementShim::from_discovered(
        parse_glm_keypool_config_json(&cfg_json, None, KeyPoolDiscoverySource::UserHomeDefault)
            .expect("parse config"),
    );

    let chat_req = LlmChatRequest::single_user_prompt("Verify 5D token accounting")
        .with_session_id("desktop-omnibar-sess-42")
        .with_cost_microcredits(25);

    let outcome = shim
        .execute_metered_chat(
            Some(MeteredBudgetContext {
                resource: &resource,
                account_id: account.account_id,
                operation_id: derive_operation_id(b"test/op", &[b"call-1"]),
                idempotency_key: derive_idempotency_key(b"test/consume", &[b"call-1"]),
                now_ms: 1_700_000_000_500,
            }),
            &chat_req,
        )
        .await
        .expect("first metered call within budget");

    assert_eq!(outcome.content, "NLOS ResourceAuthority budget verified.");
    assert_eq!(
        outcome.reasoning_content.as_deref(),
        Some("Checked 5D ceilings and sticky cache session.")
    );
    assert_eq!(outcome.isolation_tag, PROVIDER_ISOLATION_UNVERIFIED_TAG);
    assert_eq!(outcome.served_group.as_deref(), Some("default"));
    assert_eq!(
        outcome.served_slot_fingerprint,
        Some(sha256_hex16("raw-human-owner-label-secret"))
    );
    assert!(!format!("{outcome:?}").contains("raw-human-owner-label-secret"));

    let receipt = outcome.consumption_receipt.expect("consumption receipt");
    assert_eq!(receipt.sequence, 1);
    assert_eq!(receipt.prompt_tokens, 18);
    assert_eq!(receipt.completion_tokens, 22);
    assert_eq!(receipt.reasoning_tokens, 10);
    assert_eq!(receipt.total_tokens, 50);
    assert_eq!(receipt.api_calls, 1);
    assert_eq!(receipt.cost_microcredits, 25);

    let inspected = resource
        .inspect_llm_token_budget(account.account_id)
        .expect("inspect durable budget");
    assert_eq!(inspected.remaining_api_calls(), 0);
    assert_eq!(inspected.remaining_total_tokens(), 0);
    server_task
        .await
        .expect("mock server closed after 1st request");

    let blocked = shim
        .execute_metered_chat(
            Some(MeteredBudgetContext {
                resource: &resource,
                account_id: account.account_id,
                operation_id: derive_operation_id(b"test/op", &[b"call-2"]),
                idempotency_key: derive_idempotency_key(b"test/consume", &[b"call-2"]),
                now_ms: 1_700_000_001_000,
            }),
            &chat_req,
        )
        .await;
    assert!(
        matches!(blocked, Err(LlmDriverError::BudgetExhausted { .. })),
        "expected pre-dispatch BudgetExhausted, got {blocked:?}"
    );
    assert_eq!(server_hits.load(Ordering::SeqCst), 1);
}

async fn spawn_failover_mock_server() -> (u16, Arc<std::sync::Mutex<Vec<String>>>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind failover mock server");
    let port = listener.local_addr().expect("local addr").port();
    let seen_auth = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_clone = Arc::clone(&seen_auth);

    let handle = tokio::spawn(async move {
        for idx in 0..3 {
            let (mut stream, _) = listener.accept().await.expect("accept failover req");
            let req = read_incoming_http_request(&mut stream).await;
            let auth = req
                .headers
                .get("authorization")
                .cloned()
                .unwrap_or_default();
            seen_clone.lock().expect("lock seen").push(auth);

            let resp = if idx == 0 {
                MockWireResponse {
                    status_code: 429,
                    reason: "Too Many Requests",
                    headers: vec![
                        ("Content-Type".to_owned(), "application/json".to_owned()),
                        ("Retry-After".to_owned(), "15".to_owned()),
                    ],
                    body: r#"{"error":{"code":"1302","message":"rate limit"}}"#.to_owned(),
                    framing: MockBodyFraming::ContentLength,
                }
            } else {
                MockWireResponse {
                    status_code: 200,
                    reason: "OK",
                    headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
                    body: r#"{"model":"glm-5.3","choices":[{"message":{"role":"assistant","content":"failover-ok"}}],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}"#.to_owned(),
                    framing: MockBodyFraming::ContentLength,
                }
            };
            write_mock_http_response(&mut stream, &resp).await;
        }
    });
    (port, seen_auth, handle)
}

fn open_failover_resource_budget(
    tmp: &ScopedTempDir,
) -> (ResourceAuthority, nlos_types::ResourceAccountId) {
    let resource = ResourceAuthority::open(tmp.path().join("resource")).expect("open SQLite");
    let account = resource
        .create_account(CreateAccountRequest {
            initial_credit: 10_000,
            idempotency_key: derive_idempotency_key(b"test/fo-acct", &[b"1"]),
            created_at_ms: 1_000_000,
        })
        .expect("account");
    resource
        .create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account.account_id,
            limits: LlmTokenLimits {
                max_prompt_tokens: 1_000,
                max_completion_tokens: 1_000,
                max_total_tokens: 1_000,
                max_api_calls: 10,
                max_cost_microcredits: 1_000,
            },
            idempotency_key: derive_idempotency_key(b"test/fo-bud", &[b"1"]),
            created_at_ms: 1_000_000,
        })
        .expect("budget");
    (resource, account.account_id)
}

#[tokio::test]
async fn direct_keypool_scheduler_sticky_affinity_and_429_cooldown_failover() {
    let (port, seen_auth_headers, server_task) = spawn_failover_mock_server().await;

    let slots = vec![
        DirectKeySlotConfig::new(
            "slot-alpha",
            "default",
            SecretCredential::new("synthetic-key-alpha"),
        ),
        DirectKeySlotConfig::new(
            "slot-beta",
            "default",
            SecretCredential::new("synthetic-key-beta"),
        ),
        DirectKeySlotConfig::new(
            "slot-gamma",
            "xai",
            SecretCredential::new("synthetic-key-gamma"),
        ),
    ];
    let scheduler = Arc::new(
        DirectKeyPoolScheduler::new(DirectKeyPoolConfig::new(
            format!("http://127.0.0.1:{port}/v1"),
            KeySelectionStrategy::Sticky,
            slots,
        ))
        .expect("build DirectKeyPoolScheduler"),
    );

    let s1 = scheduler
        .select_slot(Some("sticky-session-abc"), 1_000_000, &[])
        .expect("select 1");
    let s2 = scheduler
        .select_slot(Some("sticky-session-abc"), 1_000_000, &[])
        .expect("select 2");
    assert_eq!(s1, s2);

    let shim = LlmEnforcementShim::from_direct_pool(
        Arc::clone(&scheduler),
        "glm-5.3",
        "glm-5.3-flash",
        vec!["glm-5.3".to_owned(), "glm-5.3-flash".to_owned()],
    );

    let tmp = ScopedTempDir::new("failover");
    let (resource, account_id) = open_failover_resource_budget(&tmp);

    let req = LlmChatRequest::single_user_prompt("Test 429 failover")
        .with_session_id("sticky-session-abc");

    let out1 = shim
        .execute_metered_chat(
            Some(MeteredBudgetContext {
                resource: &resource,
                account_id,
                operation_id: derive_operation_id(b"test/fo-op", &[b"1"]),
                idempotency_key: derive_idempotency_key(b"test/fo-idem", &[b"1"]),
                now_ms: 1_000_000,
            }),
            &req,
        )
        .await
        .expect("failover call 1 succeeds");
    assert_eq!(out1.content, "failover-ok");
    assert_ne!(
        out1.served_slot_fingerprint.as_deref(),
        Some(s1.slot_fingerprint.as_str())
    );
    assert_eq!(scheduler.active_slots_at(1_005_000), 2);

    let out2 = shim
        .execute_metered_chat(
            Some(MeteredBudgetContext {
                resource: &resource,
                account_id,
                operation_id: derive_operation_id(b"test/fo-op", &[b"2"]),
                idempotency_key: derive_idempotency_key(b"test/fo-idem", &[b"2"]),
                now_ms: 1_005_000,
            }),
            &req,
        )
        .await
        .expect("call 2 skips cooled slot");
    assert_eq!(out2.served_slot_fingerprint, out1.served_slot_fingerprint);

    server_task.await.expect("mock failover server finished");
    let recorded = seen_auth_headers.lock().expect("lock seen");
    assert_eq!(recorded.len(), 3);
    assert_ne!(recorded[0], recorded[1], "must switch key after 429");
    assert_eq!(
        recorded[1], recorded[2],
        "must keep backup key while cooled down"
    );

    assert_eq!(scheduler.active_slots_at(1_015_000), 3);
    assert_eq!(
        scheduler
            .select_slot(Some("sticky-session-abc"), 1_015_000, &[])
            .expect("recovered slot"),
        s1
    );
}

#[tokio::test]
async fn compile_nl_intent_extracts_fenced_json_and_escalates_destructive_queries() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind intent mock server");
    let port = listener.local_addr().expect("local addr").port();

    let server_task = tokio::spawn(async move {
        let (mut s1, _) = listener.accept().await.expect("accept intent 1");
        let r1 = read_incoming_http_request(&mut s1).await;
        assert!(
            r1.body.contains(r#""model":"glm-5.3-flash""#),
            "compile_nl_intent must default to fast_intent_model"
        );
        let fenced_reply = r#"{"model":"glm-5.3-flash","choices":[{"message":{"role":"assistant","content":"```json\n{\"summary\":\"查看当前工作区任务列表\",\"target_domain\":\"task\",\"undo_class\":\"LOSSLESS\",\"needs_clarification\":false,\"clarification_reason\":\"\",\"steps\":[\"list_tasks\"]}\n```"}}],"usage":{"prompt_tokens":30,"completion_tokens":25,"total_tokens":55}}"#;
        write_mock_http_response(
            &mut s1,
            &MockWireResponse {
                status_code: 200,
                reason: "OK",
                headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
                body: fenced_reply.to_owned(),
                framing: MockBodyFraming::ContentLength,
            },
        )
        .await;

        let (mut s2, _) = listener.accept().await.expect("accept intent 2");
        let _r2 = read_incoming_http_request(&mut s2).await;
        let unsafe_model_reply = r#"{"model":"glm-5.3-flash","choices":[{"message":{"role":"assistant","content":"{\"summary\":\"清理归档\",\"target_domain\":\"artifact\",\"undo_class\":\"LOSSLESS\",\"needs_clarification\":false,\"clarification_reason\":\"\",\"steps\":[\"remove_artifacts\"]}"}}]}"#;
        write_mock_http_response(
            &mut s2,
            &MockWireResponse {
                status_code: 200,
                reason: "OK",
                headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
                body: unsafe_model_reply.to_owned(),
                framing: MockBodyFraming::ContentLength,
            },
        )
        .await;
    });

    let tmp = ScopedTempDir::new("intent");
    let cfg_path = tmp.path().join("mock-pool-config.json");
    let cfg_json = format!(
        r#"{{"listen":{{"host":"127.0.0.1","port":{port}}},"egressKey":"intent-egress-key","upstreamBase":"https://open.bigmodel.cn","allowModels":["glm-5.3","glm-5.3-flash"]}}"#
    );
    std::fs::write(&cfg_path, &cfg_json).expect("write mock config");
    let shim = LlmEnforcementShim::discover(Some(&cfg_path)).expect("discover shim");

    let benign = shim
        .compile_nl_intent(None, "展示当前工作区运行中的任务", "zh-CN")
        .await
        .expect("compile benign intent");
    assert_eq!(benign.target_domain, IntentTargetDomain::Task);
    assert_eq!(benign.undo_class, IntentUndoClass::Lossless);
    assert!(!benign.needs_clarification);
    assert!(!benign.safety_escalated);
    assert_eq!(benign.steps, vec!["list_tasks".to_owned()]);
    assert_eq!(benign.isolation_tag, PROVIDER_ISOLATION_UNVERIFIED_TAG);

    let destructive = shim
        .compile_nl_intent(None, "删除所有旧产物并清空缓存", "zh-CN")
        .await
        .expect("compile destructive intent");
    assert_eq!(destructive.target_domain, IntentTargetDomain::Artifact);
    assert_eq!(destructive.undo_class, IntentUndoClass::Irreversible);
    assert!(destructive.needs_clarification);
    assert!(destructive.safety_escalated);
    assert_ne!(destructive.clarification_reason, "");
    assert!(destructive.usage.estimated_fallback);
    assert!(destructive.usage.prompt_tokens >= 1);
    assert!(destructive.usage.completion_tokens >= 1);

    server_task.await.expect("mock server finished");
}

#[tokio::test]
async fn cli_binary_executes_metered_chat_end_to_end_with_redacted_output() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind cli mock server");
    let port = listener.local_addr().expect("local addr").port();

    let server_task = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept cli chat");
        let _r = read_incoming_http_request(&mut s).await;
        let cli_chat_reply = r#"{"model":"glm-5.3","choices":[{"message":{"role":"assistant","content":"CLI metered chat verified."}}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#;
        write_mock_http_response(
            &mut s,
            &MockWireResponse {
                status_code: 200,
                reason: "OK",
                headers: vec![
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                    ("X-Pool-Group".to_owned(), "default".to_owned()),
                    ("X-Pool-Key".to_owned(), "cli-slot-01".to_owned()),
                ],
                body: cli_chat_reply.to_owned(),
                framing: MockBodyFraming::ContentLength,
            },
        )
        .await;
    });

    let tmp = ScopedTempDir::new("cli-e2e");
    let cfg_path = tmp.path().join("mock-pool-config.json");
    let cfg_json = format!(
        r#"{{"listen":{{"host":"127.0.0.1","port":{port}}},"egressKey":"intent-egress-key","upstreamBase":"https://open.bigmodel.cn","allowModels":["glm-5.3","glm-5.3-flash"]}}"#
    );
    std::fs::write(&cfg_path, &cfg_json).expect("write mock config");

    let cli_bin = env!("CARGO_BIN_EXE_nlos-llm-pool-cli");
    let state_root = tmp.path().join("cli-state-root");
    let cli_out = tokio::task::spawn_blocking(move || {
        Command::new(cli_bin)
            .args([
                "chat",
                "--prompt",
                "ping from cli",
                "--session",
                "cli-sess-1",
                "--root",
                state_root.to_str().expect("utf8 root"),
                "--pool-config",
                cfg_path.to_str().expect("utf8 cfg"),
            ])
            .output()
            .expect("execute nlos-llm-pool-cli")
    })
    .await
    .expect("join cli blocking task");
    assert!(
        cli_out.status.success(),
        "CLI failed: stderr={}",
        String::from_utf8_lossy(&cli_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&cli_out.stdout);
    assert!(stdout.contains("isolation_tag: PROVIDER_ISOLATION_UNVERIFIED"));
    assert!(stdout.contains("receipt_id: "));
    assert!(stdout.contains("CLI metered chat verified."));
    assert!(!stdout.contains("cli-slot-01"));
    assert!(!stdout.contains("intent-egress-key"));

    server_task.await.expect("mock server finished");
}
