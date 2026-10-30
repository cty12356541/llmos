//! `W77-PRE-D` (`Lane D Desktop KeyPool Bridge`) 桌面端本地 `glm-keypool` 桥接集成测试。
//!
//! 覆盖权威不变量：
//! - 在线 `/__pool__/status`、`/api/coding/paas/v4/chat/completions` 回路与 100% 凭据/槽位人名/私有目录脱敏（`DRV-CRED-001`, `[PD-COMMIT-004]`）；
//! - 复用 `<root>/trial-account.txt` 与 `<root>/resource` 持久化扣减五维 `LlmTokenBudget`，产出单调递增 `LlmTokenConsumptionReceipt`；
//! - `[NL-AMBIG-001]` 破坏性动词（如“删除”/`delete`）无论模型返回何值或处于离线回退模式，均强制升级 `IRREVERSIBLE` + `needsClarification = true`；
//! - `[DUI-OFFLINE-001]` 未配置或网关离线（端口关闭）时确定性降级至 `LOCAL_FALLBACK` / `LOCAL_DETERMINISTIC`，且 L1/L2 文案零架构黑话泄露；
//! - 五维预算耗尽时严格 `ErrorCode::Control` 失败闭合，绝不绕过权威配额门。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use llmos_desktop_lib::error::ErrorCode;
use llmos_desktop_lib::keypool_bridge::{
    ASSIST_SOURCE_LIVE_KEYPOOL, ASSIST_SOURCE_LOCAL_FALLBACK, FALLBACK_MODE_LIVE_KEYPOOL,
    FALLBACK_MODE_LOCAL_DETERMINISTIC, KeyPoolBridgeContext, KeyPoolChatInput,
    KeyPoolIntentAssistInput, assist_intent_with_keypool_context, chat_with_keypool_context,
    inspect_keypool_status_with_context,
};
use nlos_driver_llm_http::{PROVIDER_ISOLATION_UNVERIFIED_TAG, sha256_hex16};
use nlos_resource::{
    ConsumeLlmTokensRequest, CreateAccountRequest, CreateLlmTokenBudgetRequest, LlmTokenLimits,
    ResourceAuthority,
};
use nlos_types::{IdempotencyKey, OperationId};

static NEXT_TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

const BANNED_L1_L2_JARGON: &[&str] = &[
    "Z1",
    "Z2",
    "Z3",
    "Z4",
    "TypedIntent",
    "0x1F",
    "0x1f",
    "Quota",
    "quota",
    "ROAD-D-001",
    "ADR-0011",
    "TaskAttempt",
    "ExecutionFiber",
    "Cell",
    "receipt_hex",
    "control_command_id",
    "CAS_WINNER",
    "SHARED_THREAD_LIKE",
    "LOCAL_SINGLE_NODE",
];

fn assert_zero_jargon(label: &str, text: &str) {
    for &banned in BANNED_L1_L2_JARGON {
        assert!(
            !text.contains(banned),
            "{label} leaked banned architecture jargon '{banned}' in '{text}'"
        );
    }
}

struct TempBridgeDir(PathBuf);

impl TempBridgeDir {
    fn new(tag: &str) -> Self {
        let seq = NEXT_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "nlos-desktop-kp-{tag}-{}-{seq}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp bridge test dir");
        Self(dir)
    }
}

impl Drop for TempBridgeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn format_hex16_bytes(bytes: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
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
            Ok(0) => break,
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
            Err(_) => break,
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
        {{ "id": "mock-sensitive-owner-name", "key": "SYNTHETIC_UPSTREAM_KEY_DO_NOT_LEAK" }},
        {{ "id": "mock-second-slot-owner", "key": "SYNTHETIC_UPSTREAM_KEY_SECOND" }},
        {{ "id": "mock-third-slot-owner", "key": "SYNTHETIC_UPSTREAM_KEY_THIRD" }}
      ]
    }}
  ]
}}"#
    );
    fs::write(path, config_json).expect("write synthetic glm-keypool config.json");
}

#[tokio::test]
async fn online_status_probe_reports_sanitized_pool_snapshot_with_zero_secret_or_slot_leakage() {
    let temp = TempBridgeDir::new("status-online");
    let private_subdir = temp.0.join("private-host-profile-segment");
    fs::create_dir_all(&private_subdir).expect("create private subdir");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock keypool");
    let port = listener.local_addr().expect("mock keypool addr").port();
    let config_path = private_subdir.join("config.json");
    write_synthetic_keypool_config(&config_path, port);

    let resource_dir = temp.0.join("trial-root").join("resource");
    fs::create_dir_all(&resource_dir).expect("create resource dir");

    let server_thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept status probe");
        let req_text = read_full_http_request(&mut stream);
        assert!(
            req_text.starts_with("GET /__pool__/status HTTP/1.1"),
            "unexpected status request: {req_text}"
        );
        assert!(
            req_text.contains("Authorization: Bearer SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK"),
            "missing egress bearer token on loopback request"
        );
        let status_body = r#"{
  "strategy": "round-robin",
  "totalKeys": 3,
  "activeKeys": 3,
  "activeSessions": 1,
  "totalRequests": 9,
  "okRequests": 9,
  "failedRequests": 0,
  "tokensIn": 1024,
  "tokensOut": 256,
  "keys": [
    { "id": "mock-sensitive-owner-name", "masked": "sk-synth-never-leak" }
  ]
}"#;
        write_http_json_response(&mut stream, "200 OK", &[], status_body);
    });

    let ctx = KeyPoolBridgeContext::isolated(Some(config_path), Some(resource_dir));
    let dto = inspect_keypool_status_with_context(ctx)
        .await
        .expect("inspect_keypool_status_with_context");
    server_thread.join().expect("join mock status server");

    assert!(dto.configured);
    assert!(dto.reachable);
    assert_eq!(dto.discovery_source, "explicit:config.json");
    assert_eq!(
        dto.gateway_origin.as_deref(),
        Some(format!("http://127.0.0.1:{port}").as_str())
    );
    assert_eq!(dto.openai_base_path.as_deref(), Some("/api/coding/paas/v4"));
    assert_eq!(dto.default_model.as_deref(), Some("glm-5.3"));
    assert_eq!(dto.fast_intent_model.as_deref(), Some("glm-5.3"));
    assert_eq!(dto.active_keys, Some(3));
    assert_eq!(dto.total_keys, Some(3));
    assert_eq!(dto.strategy.as_deref(), Some("round-robin"));
    assert!(dto.budget_metered);
    assert_eq!(dto.isolation_tag, PROVIDER_ISOLATION_UNVERIFIED_TAG);
    assert_eq!(dto.fallback_mode, FALLBACK_MODE_LIVE_KEYPOOL);

    let serialized = serde_json::to_string(&dto).expect("serialize KeyPoolStatusDto");
    assert!(serialized.contains("\"openaiBasePath\":\"/api/coding/paas/v4\""));
    assert!(serialized.contains("\"activeKeys\":3"));
    assert!(serialized.contains("\"fallbackMode\":\"LIVE_KEYPOOL\""));
    assert!(serialized.contains("\"budgetMetered\":true"));

    for forbidden in [
        "SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK",
        "SYNTHETIC_UPSTREAM_KEY_DO_NOT_LEAK",
        "SYNTHETIC_UPSTREAM_KEY_SECOND",
        "SYNTHETIC_UPSTREAM_KEY_THIRD",
        "mock-sensitive-owner-name",
        "mock-second-slot-owner",
        "mock-third-slot-owner",
        "sk-synth-never-leak",
        "private-host-profile-segment",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "KeyPoolStatusDto JSON leaked sensitive token '{forbidden}':\n{serialized}"
        );
    }
}

#[tokio::test]
async fn online_chat_and_assist_intent_charge_5d_budget_reuse_trial_marker_and_escalate_destructive_intent()
 {
    let temp = TempBridgeDir::new("chat-assist-online");
    let trial_root = temp.0.join("trial-state");
    let resource_dir = trial_root.join("resource");
    fs::create_dir_all(&resource_dir).expect("create trial resource dir");

    // Pre-seed a custom trial account marker `<root>/trial-account.txt` + 5D budget in `<root>/resource`
    // to verify seamless cross-tool ledger sharing between `nlos-up` and `desktop/src-tauri`.
    let custom_account = {
        let resource = ResourceAuthority::open(&resource_dir).expect("open ResourceAuthority");
        let acct = resource
            .create_account(CreateAccountRequest {
                initial_credit: 50_000_000,
                idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
                created_at_ms: 1_700_000_000_000,
            })
            .expect("create custom trial account");
        resource
            .create_llm_token_budget(CreateLlmTokenBudgetRequest {
                account_id: acct.account_id,
                limits: LlmTokenLimits {
                    max_prompt_tokens: 100_000,
                    max_completion_tokens: 100_000,
                    max_total_tokens: 200_000,
                    max_api_calls: 500,
                    max_cost_microcredits: 50_000_000,
                },
                idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
                created_at_ms: acct.created_at_ms,
            })
            .expect("seed custom trial 5D LlmTokenBudget");
        acct.account_id
    };
    fs::write(
        trial_root.join("trial-account.txt"),
        format!("{}\n", format_hex16_bytes(*custom_account.as_bytes())),
    )
    .expect("write trial-account.txt");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock keypool");
    let port = listener.local_addr().expect("mock keypool addr").port();
    let config_path = temp.0.join("config.json");
    write_synthetic_keypool_config(&config_path, port);

    let server_thread = std::thread::spawn(move || {
        // Request 1: `chat_with_keypool_context`
        let (mut chat_stream, _) = listener.accept().expect("accept chat request");
        let chat_req = read_full_http_request(&mut chat_stream);
        assert!(
            chat_req.starts_with("POST /api/coding/paas/v4/chat/completions HTTP/1.1"),
            "unexpected chat request: {chat_req}"
        );
        assert!(chat_req.contains("x-pool-session: desktop-omnibar-sess-1"));
        let chat_resp = r#"{
  "id": "chatcmpl-desktop-01",
  "model": "glm-5.3",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Desktop KeyPool bridge verified."
      }
    }
  ],
  "usage": {
    "prompt_tokens": 20,
    "completion_tokens": 28,
    "completion_tokens_details": { "reasoning_tokens": 8 },
    "total_tokens": 56
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

        // Request 2: `assist_intent_with_keypool_context` with destructive verb ("删除旧归档文件")
        // where mock model attempts unsafe downgrade (`undo_class: "LOSSLESS", needs_clarification: false`).
        let (mut assist_stream, _) = listener.accept().expect("accept assist request");
        let assist_req = read_full_http_request(&mut assist_stream);
        assert!(
            assist_req.starts_with("POST /api/coding/paas/v4/chat/completions HTTP/1.1"),
            "unexpected assist request: {assist_req}"
        );
        let assist_resp = r#"{
  "id": "chatcmpl-desktop-02",
  "model": "glm-5.3",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "{\"summary\":\"清理旧归档文件并释放空间\",\"target_domain\":\"artifact\",\"undo_class\":\"LOSSLESS\",\"needs_clarification\":false,\"clarification_reason\":\"\",\"steps\":[\"扫描旧归档目录\",\"移除过期归档文件\"]}"
      }
    }
  ],
  "usage": {
    "prompt_tokens": 32,
    "completion_tokens": 22,
    "total_tokens": 54
  }
}"#;
        write_http_json_response(
            &mut assist_stream,
            "200 OK",
            &[
                ("x-pool-group", "glm-coding"),
                ("x-pool-key", "mock-sensitive-owner-name"),
            ],
            assist_resp,
        );
    });

    let ctx = KeyPoolBridgeContext::isolated(Some(config_path), Some(resource_dir.clone()));
    let expected_fp = sha256_hex16("mock-sensitive-owner-name");

    let chat_dto = chat_with_keypool_context(
        ctx.clone(),
        KeyPoolChatInput {
            prompt: "Check desktop KeyPool metering".to_owned(),
            model: None,
            max_tokens: Some(128),
            session_id: Some("desktop-omnibar-sess-1".to_owned()),
            keypool_config_path: None,
        },
    )
    .await
    .expect("chat_with_keypool_context");

    assert_eq!(chat_dto.model, "glm-5.3");
    assert_eq!(chat_dto.content, "Desktop KeyPool bridge verified.");
    assert_eq!(chat_dto.served_group.as_deref(), Some("glm-coding"));
    assert_eq!(
        chat_dto.served_slot_fingerprint.as_deref(),
        Some(expected_fp.as_str())
    );
    assert_eq!(chat_dto.prompt_tokens, 20);
    assert_eq!(chat_dto.completion_tokens, 28);
    assert_eq!(chat_dto.reasoning_tokens, 8);
    assert_eq!(chat_dto.total_tokens, 56);
    assert_eq!(chat_dto.isolation_tag, PROVIDER_ISOLATION_UNVERIFIED_TAG);

    let chat_receipt = chat_dto
        .budget_receipt
        .as_ref()
        .expect("chat 5D budget receipt");
    assert_eq!(chat_receipt.receipt_id_hex.len(), 32);
    assert_eq!(chat_receipt.sequence, 1);
    assert_eq!(chat_receipt.total_tokens, 56);
    assert_eq!(chat_receipt.cumulative_total_tokens, 56);
    assert_eq!(chat_receipt.cumulative_api_calls, 1);

    let chat_json = serde_json::to_string(&chat_dto).expect("serialize KeyPoolChatDto");
    assert!(!chat_json.contains("mock-sensitive-owner-name"));
    assert!(!chat_json.contains("SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK"));

    let assist_dto = assist_intent_with_keypool_context(
        ctx,
        KeyPoolIntentAssistInput {
            utterance: "删除旧归档文件".to_owned(),
            model: None,
            locale: Some("zh-CN".to_owned()),
            keypool_config_path: None,
        },
    )
    .await
    .expect("assist_intent_with_keypool_context");
    server_thread.join().expect("join mock chat+assist server");

    assert_eq!(assist_dto.source, ASSIST_SOURCE_LIVE_KEYPOOL);
    assert_eq!(assist_dto.target_domain, "artifact");
    assert_eq!(assist_dto.undo_class, "IRREVERSIBLE");
    assert!(assist_dto.needs_clarification);
    assert!(assist_dto.safety_escalated);
    assert!(!assist_dto.clarification_reason.is_empty());
    assert_eq!(assist_dto.model.as_deref(), Some("glm-5.3"));
    assert_eq!(assist_dto.isolation_tag, PROVIDER_ISOLATION_UNVERIFIED_TAG);
    assert!(assist_dto.fallback_notice.is_none());

    assert_zero_jargon("online assist summary", &assist_dto.summary);
    assert_zero_jargon(
        "online assist clarification_reason",
        &assist_dto.clarification_reason,
    );
    for step in &assist_dto.steps {
        assert_zero_jargon("online assist step", step);
    }

    let assist_receipt = assist_dto
        .budget_receipt
        .as_ref()
        .expect("assist 5D budget receipt");
    assert_eq!(assist_receipt.sequence, 2);
    assert_eq!(assist_receipt.prompt_tokens, 32);
    assert_eq!(assist_receipt.completion_tokens, 22);
    assert_eq!(assist_receipt.total_tokens, 54);
    assert_eq!(assist_receipt.cumulative_total_tokens, 110);
    assert_eq!(assist_receipt.cumulative_api_calls, 2);

    let assist_json = serde_json::to_string(&assist_dto).expect("serialize KeyPoolIntentAssistDto");
    assert!(!assist_json.contains("mock-sensitive-owner-name"));
    assert!(!assist_json.contains("SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK"));

    // Verify durable SQLite ledger state in ResourceAuthority against custom_account.
    let resource = ResourceAuthority::open(&resource_dir).expect("reopen ResourceAuthority");
    let durable = resource
        .inspect_llm_token_budget(custom_account)
        .expect("inspect durable custom trial budget");
    assert_eq!(durable.consumed_api_calls, 2);
    assert_eq!(durable.consumed_prompt_tokens, 52);
    assert_eq!(durable.consumed_completion_tokens, 50);
    assert_eq!(durable.consumed_reasoning_tokens, 8);
    assert_eq!(durable.consumed_total_tokens, 110);
}

#[tokio::test]
async fn offline_and_unconfigured_keypool_degrade_deterministically_with_zero_jargon_and_destructive_guardrail()
 {
    let temp = TempBridgeDir::new("offline-fallback");
    let resource_dir = temp.0.join("resource");
    fs::create_dir_all(&resource_dir).expect("create resource dir");

    // 1. Unconfigured isolated context (no config override, host env discovery disabled).
    let unconfigured_ctx = KeyPoolBridgeContext::isolated(None, Some(resource_dir.clone()));
    let unconf_status = inspect_keypool_status_with_context(unconfigured_ctx.clone())
        .await
        .expect("unconfigured status probe");
    assert!(!unconf_status.configured);
    assert!(!unconf_status.reachable);
    assert_eq!(
        unconf_status.fallback_mode,
        FALLBACK_MODE_LOCAL_DETERMINISTIC
    );

    let unconf_assist_en = assist_intent_with_keypool_context(
        unconfigured_ctx,
        KeyPoolIntentAssistInput {
            utterance: "Open document editor".to_owned(),
            model: None,
            locale: Some("en-US".to_owned()),
            keypool_config_path: None,
        },
    )
    .await
    .expect("unconfigured en-US assist fallback");
    assert_eq!(unconf_assist_en.source, ASSIST_SOURCE_LOCAL_FALLBACK);
    assert_eq!(unconf_assist_en.target_domain, "application");
    assert_eq!(unconf_assist_en.undo_class, "LOSSLESS");
    assert!(!unconf_assist_en.needs_clarification);
    assert!(!unconf_assist_en.safety_escalated);
    let en_notice = unconf_assist_en
        .fallback_notice
        .as_deref()
        .expect("en-US fallback notice");
    assert_zero_jargon("en-US fallback notice", en_notice);
    for step in &unconf_assist_en.steps {
        assert_zero_jargon("en-US fallback step", step);
    }

    // 2. Configured but closed loopback port (gateway process offline -> `[DUI-OFFLINE-001]`).
    let closed_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let closed_port = closed_listener.local_addr().expect("closed port").port();
    drop(closed_listener);

    let offline_cfg = temp.0.join("offline-config.json");
    write_synthetic_keypool_config(&offline_cfg, closed_port);

    let offline_ctx = KeyPoolBridgeContext::isolated(Some(offline_cfg), Some(resource_dir));
    let offline_status = inspect_keypool_status_with_context(offline_ctx.clone())
        .await
        .expect("offline status probe");
    assert!(offline_status.configured);
    assert!(!offline_status.reachable);
    assert_eq!(offline_status.active_keys, Some(0));
    assert_eq!(offline_status.total_keys, Some(3));
    assert_eq!(
        offline_status.fallback_mode,
        FALLBACK_MODE_LOCAL_DETERMINISTIC
    );
    let offline_status_json =
        serde_json::to_string(&offline_status).expect("serialize offline KeyPoolStatusDto");
    assert!(!offline_status_json.contains("SYNTHETIC_EGRESS_TOKEN_DO_NOT_LEAK"));
    assert!(!offline_status_json.contains("mock-sensitive-owner-name"));

    // Destructive utterance while offline MUST still escalate to IRREVERSIBLE + needsClarification!
    let offline_destructive = assist_intent_with_keypool_context(
        offline_ctx,
        KeyPoolIntentAssistInput {
            utterance: "删除旧归档文件".to_owned(),
            model: None,
            locale: Some("zh-CN".to_owned()),
            keypool_config_path: None,
        },
    )
    .await
    .expect("offline destructive assist fallback");
    assert_eq!(offline_destructive.source, ASSIST_SOURCE_LOCAL_FALLBACK);
    assert_eq!(offline_destructive.undo_class, "IRREVERSIBLE");
    assert!(offline_destructive.needs_clarification);
    assert!(offline_destructive.safety_escalated);
    assert!(!offline_destructive.clarification_reason.is_empty());
    let zh_notice = offline_destructive
        .fallback_notice
        .as_deref()
        .expect("zh-CN fallback notice");
    assert_zero_jargon(
        "zh-CN offline destructive summary",
        &offline_destructive.summary,
    );
    assert_zero_jargon(
        "zh-CN offline destructive clarification",
        &offline_destructive.clarification_reason,
    );
    assert_zero_jargon("zh-CN offline fallback notice", zh_notice);
    for step in &offline_destructive.steps {
        assert_zero_jargon("zh-CN offline destructive step", step);
    }
}

#[tokio::test]
async fn budget_exhaustion_fails_closed_with_control_error_and_zero_upstream_egress() {
    let temp = TempBridgeDir::new("budget-exhausted");
    let trial_root = temp.0.join("trial-exhausted");
    let resource_dir = trial_root.join("resource");
    fs::create_dir_all(&resource_dir).expect("create resource dir");

    let exhausted_account = {
        let resource = ResourceAuthority::open(&resource_dir).expect("open ResourceAuthority");
        let acct = resource
            .create_account(CreateAccountRequest {
                initial_credit: 10_000,
                idempotency_key: IdempotencyKey::from_bytes([0x31; 16]),
                created_at_ms: 1_700_000_000_000,
            })
            .expect("create account for exhaustion test");
        resource
            .create_llm_token_budget(CreateLlmTokenBudgetRequest {
                account_id: acct.account_id,
                limits: LlmTokenLimits {
                    max_prompt_tokens: 100,
                    max_completion_tokens: 100,
                    max_total_tokens: 100,
                    max_api_calls: 1,
                    max_cost_microcredits: 10_000,
                },
                idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
                created_at_ms: acct.created_at_ms,
            })
            .expect("configure 1-call budget");
        resource
            .consume_llm_tokens(ConsumeLlmTokensRequest {
                account_id: acct.account_id,
                operation_id: OperationId::from_bytes([0x51; 16]),
                prompt_tokens: 10,
                completion_tokens: 10,
                reasoning_tokens: 0,
                api_calls: 1,
                cost_microcredits: 0,
                idempotency_key: IdempotencyKey::from_bytes([0x61; 16]),
                consumed_at_ms: acct.created_at_ms + 1,
            })
            .expect("consume sole API call so remaining_api_calls == 0");
        acct.account_id
    };
    fs::write(
        trial_root.join("trial-account.txt"),
        format!("{}\n", format_hex16_bytes(*exhausted_account.as_bytes())),
    )
    .expect("write trial-account.txt");

    // Bind a port that never accepts connections; pre-flight budget check must reject BEFORE socket connect.
    let closed_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let closed_port = closed_listener.local_addr().expect("port").port();
    drop(closed_listener);
    let config_path = temp.0.join("config.json");
    write_synthetic_keypool_config(&config_path, closed_port);

    let ctx = KeyPoolBridgeContext::isolated(Some(config_path), Some(resource_dir));

    let chat_err = chat_with_keypool_context(
        ctx.clone(),
        KeyPoolChatInput {
            prompt: "Should be blocked by pre-flight 5D budget check".to_owned(),
            model: None,
            max_tokens: Some(32),
            session_id: None,
            keypool_config_path: None,
        },
    )
    .await
    .expect_err("exhausted 5D budget must reject chat");
    assert_eq!(chat_err.code, ErrorCode::Control);

    let assist_err = assist_intent_with_keypool_context(
        ctx,
        KeyPoolIntentAssistInput {
            utterance: "查看当前任务进度".to_owned(),
            model: None,
            locale: Some("zh-CN".to_owned()),
            keypool_config_path: None,
        },
    )
    .await
    .expect_err(
        "exhausted 5D budget must fail closed on assist rather than bypassing ResourceAuthority",
    );
    assert_eq!(assist_err.code, ErrorCode::Control);
}
