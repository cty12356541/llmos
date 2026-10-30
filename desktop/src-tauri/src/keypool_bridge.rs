//! 本地 `glm-keypool` HTTP/1.1 桌面端桥接模块（`W77-PRE-D` / `D-OMNIBAR-MULTIMODAL`）。
//!
//! 职责与权威约束：
//! - 通过 [`LlmEnforcementShim`] 连接本地 `glm-keypool` 网关，零明文密钥、零槽位昵称、零宿主私有目录泄露（`DRV-CRED-001`, `[PD-COMMIT-004]`）；
//! - 绑定会话 `resource_root` 下的 [`ResourceAuthority`]（优先复用父目录 `trial-account.txt` 试用账户，否则幂等初始化默认五维 `LlmTokenBudget`），使桌面每次模型推理均产出单调递增五维 `LlmTokenConsumptionReceipt`；
//! - 当本地 `KeyPool` 未配置或离线不可达时，按 `[DUI-OFFLINE-001]` 确定性降级至本地规则拆解，不崩溃、不锁死 `Z3` 面板，并继续强制执行 `[NL-AMBIG-001]` 破坏性动词 `IRREVERSIBLE` + `needsClarification` 安全升级；
//! - 所有后台网络与 `SQLite` I/O 均经阻塞线程池脱离 UI 主线程执行。

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_driver_llm_http::{
    DiscoveredKeyPoolConfig, HttpUrl, IntentTargetDomain, IntentUndoClass, KeyPoolDiscoverySource,
    LlmChatRequest, LlmDriverError, LlmEnforcementShim, MeteredBudgetContext,
    PROVIDER_ISOLATION_UNVERIFIED_TAG, contains_destructive_keyword, current_wall_ms,
    derive_idempotency_key, derive_operation_id, discover_local_keypool,
    discover_local_keypool_from_lookup, ensure_default_trial_llm_budget, format_receipt_id_hex,
};
use nlos_resource::{LlmTokenBudgetRecord, LlmTokenConsumptionReceipt, ResourceAuthority};
use nlos_types::ResourceAccountId;
use serde::{Deserialize, Serialize};

use crate::error::{DesktopError, ErrorCode};
use crate::ipc::{AppState, ENV_RESOURCE_ROOT, get_config};

/// 在线模型池回退模式常量。
pub const FALLBACK_MODE_LIVE_KEYPOOL: &str = "LIVE_KEYPOOL";
/// 离线/未配置确定性本地解析回退模式常量（`[DUI-OFFLINE-001]`）。
pub const FALLBACK_MODE_LOCAL_DETERMINISTIC: &str = "LOCAL_DETERMINISTIC";

/// 辅助意图拆解来源：在线本地 `KeyPool`。
pub const ASSIST_SOURCE_LIVE_KEYPOOL: &str = "LIVE_KEYPOOL";
/// 辅助意图拆解来源：离线本地确定性规则回退（`[DUI-OFFLINE-001]`）。
pub const ASSIST_SOURCE_LOCAL_FALLBACK: &str = "LOCAL_FALLBACK";

/// `nlos-up` 写入的试用账户标记文件名。
const TRIAL_ACCOUNT_MARKER_FILE: &str = "trial-account.txt";

static NEXT_BRIDGE_CALL_NONCE: AtomicU64 = AtomicU64::new(1);

/// 桌面端本地 `KeyPool` 状态快照 DTO（100% 脱敏，零密钥、零槽位人名、零私有全路径）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPoolStatusDto {
    pub configured: bool,
    pub reachable: bool,
    pub discovery_source: String,
    pub gateway_origin: Option<String>,
    pub openai_base_path: Option<String>,
    pub default_model: Option<String>,
    pub fast_intent_model: Option<String>,
    pub allowed_models: Vec<String>,
    pub active_keys: Option<u64>,
    pub total_keys: Option<u64>,
    pub strategy: Option<String>,
    pub budget_metered: bool,
    pub isolation_tag: String,
    pub fallback_mode: String,
    pub diagnostic_summary: String,
}

/// 五维 `LlmTokenBudget` 扣减回执 DTO。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPoolBudgetReceiptDto {
    pub receipt_id_hex: String,
    pub sequence: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub cumulative_total_tokens: u64,
    pub cumulative_api_calls: u64,
}

impl From<LlmTokenConsumptionReceipt> for KeyPoolBudgetReceiptDto {
    fn from(receipt: LlmTokenConsumptionReceipt) -> Self {
        Self {
            receipt_id_hex: format_receipt_id_hex(receipt.receipt_id),
            sequence: receipt.sequence,
            prompt_tokens: receipt.prompt_tokens,
            completion_tokens: receipt.completion_tokens,
            reasoning_tokens: receipt.reasoning_tokens,
            total_tokens: receipt.total_tokens,
            cumulative_total_tokens: receipt.cumulative_total_tokens,
            cumulative_api_calls: receipt.cumulative_api_calls,
        }
    }
}

/// `Z3` Omnibar 模型辅助意图拆解请求输入。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPoolIntentAssistInput {
    pub utterance: String,
    pub model: Option<String>,
    pub locale: Option<String>,
    pub keypool_config_path: Option<String>,
}

/// `Z3` Omnibar 模型辅助意图拆解结果 DTO（含 `[DUI-OFFLINE-001]` 回退与 `[NL-AMBIG-001]` 安全升级）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPoolIntentAssistDto {
    pub source: String,
    pub summary: String,
    pub target_domain: String,
    pub undo_class: String,
    pub needs_clarification: bool,
    pub clarification_reason: String,
    pub steps: Vec<String>,
    pub safety_escalated: bool,
    pub model: Option<String>,
    pub served_slot_fingerprint: Option<String>,
    pub isolation_tag: String,
    pub budget_receipt: Option<KeyPoolBudgetReceiptDto>,
    pub fallback_notice: Option<String>,
}

/// 单轮提示词推理请求输入（`llm_keypool_chat`）。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPoolChatInput {
    pub prompt: String,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub keypool_config_path: Option<String>,
}

/// 单轮提示词推理脱敏结果 DTO（`llm_keypool_chat`）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPoolChatDto {
    pub model: String,
    pub content: String,
    pub served_group: Option<String>,
    pub served_slot_fingerprint: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub isolation_tag: String,
    pub budget_receipt: Option<KeyPoolBudgetReceiptDto>,
}

/// 桥接执行上下文（供 Tauri 命令与无全局环境变量污染的 Rust 集成测试共用）。
#[derive(Clone, Debug)]
pub struct KeyPoolBridgeContext {
    /// 可选显式 `config.json` 覆盖路径（最高优先级）。
    pub keypool_config_override: Option<PathBuf>,
    /// 可选 `ResourceAuthority` 根目录（如 `<root>/resource`）。
    pub resource_root: Option<PathBuf>,
    /// 是否允许回退读取进程环境变量（`LLMOS_KEYPOOL_CONFIG` / `USERPROFILE`）。
    /// 生产运行为 `true`；隔离单测设为 `false` 可确保零宿主机文件探测。
    pub allow_host_env_discovery: bool,
}

impl Default for KeyPoolBridgeContext {
    fn default() -> Self {
        Self {
            keypool_config_override: None,
            resource_root: None,
            allow_host_env_discovery: true,
        }
    }
}

impl KeyPoolBridgeContext {
    /// 构建严格隔离的测试桥接上下文（绝不读取宿主机环境变量或用户主目录配置）。
    #[must_use]
    pub fn isolated(
        keypool_config_override: Option<PathBuf>,
        resource_root: Option<PathBuf>,
    ) -> Self {
        Self {
            keypool_config_override,
            resource_root,
            allow_host_env_discovery: false,
        }
    }

    fn from_tauri_state(
        state: tauri::State<'_, AppState>,
        explicit_config_path: Option<String>,
    ) -> Result<Self, DesktopError> {
        let cfg = get_config(state)?;
        let resource_root = cfg
            .resource_root
            .or_else(|| std::env::var(ENV_RESOURCE_ROOT).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let keypool_config_override = explicit_config_path
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        Ok(Self {
            keypool_config_override,
            resource_root,
            allow_host_env_discovery: true,
        })
    }
}

/// 将阻塞型 `SQLite` + 本地回环 HTTP 推理放到后台工作线程池上的独立轻量单线程运行时中驱动，
/// 确保桌面 UI 主线程永不因网络超时或磁盘锁等待而卡顿。
async fn run_keypool_off_main<F, R>(work: F) -> Result<R, DesktopError>
where
    F: FnOnce() -> Result<R, DesktopError> + Send + 'static,
    R: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|_| DesktopError::internal("本地模型池桥接后台线程 join 失败"))?
}

fn block_on_current_thread<Fut, R>(future: Fut) -> Result<R, DesktopError>
where
    Fut: std::future::Future<Output = Result<R, DesktopError>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|err| {
            DesktopError::internal(format!("初始化模型池短生命周期运行时失败: {err}"))
        })?;
    runtime.block_on(future)
}

/// 探测本地 `KeyPool` 配置与 `/__pool__/status` 在线状态（无 Tauri 句柄核心）。
pub async fn inspect_keypool_status_with_context(
    ctx: KeyPoolBridgeContext,
) -> Result<KeyPoolStatusDto, DesktopError> {
    run_keypool_off_main(move || {
        block_on_current_thread(async move { Ok(inspect_status_blocking(&ctx).await) })
    })
    .await
}

/// 执行 `Z3` Omnibar 模型辅助意图拆解（无 Tauri 句柄核心；离线自动按 `[DUI-OFFLINE-001]` 降级）。
pub async fn assist_intent_with_keypool_context(
    ctx: KeyPoolBridgeContext,
    input: KeyPoolIntentAssistInput,
) -> Result<KeyPoolIntentAssistDto, DesktopError> {
    let trimmed_utterance = input.utterance.trim().to_owned();
    if trimmed_utterance.is_empty() {
        return Err(DesktopError::config("辅助解析的自然语言输入不能为空"));
    }
    run_keypool_off_main(move || {
        block_on_current_thread(async move {
            assist_intent_blocking(&ctx, &trimmed_utterance, &input).await
        })
    })
    .await
}

/// 执行五维 Token 预算计量的单轮 `KeyPool` 提示词推理（无 Tauri 句柄核心）。
pub async fn chat_with_keypool_context(
    ctx: KeyPoolBridgeContext,
    input: KeyPoolChatInput,
) -> Result<KeyPoolChatDto, DesktopError> {
    let trimmed_prompt = input.prompt.trim().to_owned();
    if trimmed_prompt.is_empty() {
        return Err(DesktopError::config("模型对话提示词不能为空"));
    }
    if input.max_tokens == Some(0) {
        return Err(DesktopError::config("maxTokens 必须大于 0"));
    }
    run_keypool_off_main(move || {
        block_on_current_thread(async move { chat_blocking(&ctx, &trimmed_prompt, &input).await })
    })
    .await
}

#[tauri::command]
pub async fn llm_keypool_inspect_status(
    state: tauri::State<'_, AppState>,
    keypool_config_path: Option<String>,
) -> Result<KeyPoolStatusDto, DesktopError> {
    let ctx = KeyPoolBridgeContext::from_tauri_state(state, keypool_config_path)?;
    inspect_keypool_status_with_context(ctx).await
}

#[tauri::command]
pub async fn llm_keypool_assist_intent(
    state: tauri::State<'_, AppState>,
    input: KeyPoolIntentAssistInput,
) -> Result<KeyPoolIntentAssistDto, DesktopError> {
    let ctx = KeyPoolBridgeContext::from_tauri_state(state, input.keypool_config_path.clone())?;
    assist_intent_with_keypool_context(ctx, input).await
}

#[tauri::command]
pub async fn llm_keypool_chat(
    state: tauri::State<'_, AppState>,
    input: KeyPoolChatInput,
) -> Result<KeyPoolChatDto, DesktopError> {
    let ctx = KeyPoolBridgeContext::from_tauri_state(state, input.keypool_config_path.clone())?;
    chat_with_keypool_context(ctx, input).await
}

fn discover_for_context(
    ctx: &KeyPoolBridgeContext,
) -> Result<DiscoveredKeyPoolConfig, LlmDriverError> {
    let override_path = ctx.keypool_config_override.as_deref();
    if ctx.allow_host_env_discovery {
        // Priority: explicit override -> LLMOS_KEYPOOL_CONFIG -> LLMOS_LLM_BASE_URL -> ~/glm-keypool/config.json
        discover_local_keypool(override_path)
    } else {
        discover_local_keypool_from_lookup(override_path, |_| None)
    }
}

async fn inspect_status_blocking(ctx: &KeyPoolBridgeContext) -> KeyPoolStatusDto {
    let budget_metered = is_resource_budget_ready(ctx.resource_root.as_deref());
    let discovered = match discover_for_context(ctx) {
        Ok(cfg) => cfg,
        Err(err) => {
            let reason = sanitize_driver_error(&err);
            return KeyPoolStatusDto {
                configured: false,
                reachable: false,
                discovery_source: "unconfigured".to_owned(),
                gateway_origin: None,
                openai_base_path: None,
                default_model: None,
                fast_intent_model: None,
                allowed_models: Vec::new(),
                active_keys: None,
                total_keys: None,
                strategy: None,
                budget_metered,
                isolation_tag: PROVIDER_ISOLATION_UNVERIFIED_TAG.to_owned(),
                fallback_mode: FALLBACK_MODE_LOCAL_DETERMINISTIC.to_owned(),
                diagnostic_summary: format!(
                    "Local KeyPool unconfigured ({reason}); deterministic local parser active"
                ),
            };
        }
    };

    let source_label = redacted_source_summary(&discovered.source);
    let (gateway_origin, openai_base_path) =
        split_gateway_origin_and_path(&discovered.openai_base_url);
    let default_model = Some(discovered.default_model.clone());
    let fast_intent_model = Some(discovered.fast_intent_model.clone());
    let allowed_models = discovered.allowed_models.clone();
    let configured_slots = discovered.configured_key_slots as u64;

    let shim = LlmEnforcementShim::from_discovered(discovered);
    match shim.probe_pool_status().await {
        Ok(health) => {
            let origin_disp = gateway_origin.as_deref().unwrap_or("loopback");
            let path_disp = openai_base_path.as_deref().unwrap_or("");
            let model_disp = default_model.as_deref().unwrap_or("glm-5.3");
            let diagnostic_summary = format!(
                "Local KeyPool online via {source_label} ({origin_disp}{path_disp}, model={model_disp}, keys={}/{})",
                health.active_keys, health.total_keys
            );
            KeyPoolStatusDto {
                configured: true,
                reachable: true,
                discovery_source: source_label,
                gateway_origin,
                openai_base_path,
                default_model,
                fast_intent_model,
                allowed_models,
                active_keys: Some(health.active_keys),
                total_keys: Some(health.total_keys),
                strategy: Some(health.strategy),
                budget_metered,
                isolation_tag: PROVIDER_ISOLATION_UNVERIFIED_TAG.to_owned(),
                fallback_mode: FALLBACK_MODE_LIVE_KEYPOOL.to_owned(),
                diagnostic_summary,
            }
        }
        Err(err) => {
            let reason = sanitize_driver_error(&err);
            let origin_disp = gateway_origin.as_deref().unwrap_or("loopback");
            let diagnostic_summary = format!(
                "Local KeyPool configured via {source_label} ({origin_disp}) is unreachable ({reason}); deterministic local fallback active"
            );
            KeyPoolStatusDto {
                configured: true,
                reachable: false,
                discovery_source: source_label,
                gateway_origin,
                openai_base_path,
                default_model,
                fast_intent_model,
                allowed_models,
                active_keys: Some(0),
                total_keys: if configured_slots > 0 {
                    Some(configured_slots)
                } else {
                    None
                },
                strategy: None,
                budget_metered,
                isolation_tag: PROVIDER_ISOLATION_UNVERIFIED_TAG.to_owned(),
                fallback_mode: FALLBACK_MODE_LOCAL_DETERMINISTIC.to_owned(),
                diagnostic_summary,
            }
        }
    }
}

async fn assist_intent_blocking(
    ctx: &KeyPoolBridgeContext,
    trimmed_utterance: &str,
    input: &KeyPoolIntentAssistInput,
) -> Result<KeyPoolIntentAssistDto, DesktopError> {
    let locale = normalize_locale(input.locale.as_deref());
    let discovered = match discover_for_context(ctx) {
        Ok(cfg) => cfg,
        Err(err) => {
            return Ok(build_local_fallback_assist(
                trimmed_utterance,
                locale,
                &sanitize_driver_error(&err),
            ));
        }
    };

    let shim = LlmEnforcementShim::from_discovered(discovered);
    let model_override = input
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let opened_budget = open_optional_budget_session(
        ctx.resource_root.as_deref(),
        b"llmos/desktop-keypool/assist-op/v1",
        b"llmos/desktop-keypool/assist-idem/v1",
    )?;

    let compile_res = match (&opened_budget, model_override) {
        (Some(session), Some(model)) => {
            shim.compile_nl_intent_with_model(
                Some(session.as_context()),
                trimmed_utterance,
                locale,
                Some(model),
            )
            .await
        }
        (Some(session), None) => {
            shim.compile_nl_intent(Some(session.as_context()), trimmed_utterance, locale)
                .await
        }
        (None, Some(model)) => {
            shim.compile_nl_intent_with_model(None, trimmed_utterance, locale, Some(model))
                .await
        }
        (None, None) => {
            shim.compile_nl_intent(None, trimmed_utterance, locale)
                .await
        }
    };
    drop(opened_budget);

    match compile_res {
        Ok(compiled) => {
            let clarification_reason = if compiled.safety_escalated
                && (compiled.clarification_reason.is_empty()
                    || compiled.clarification_reason.contains("[NL-AMBIG-001]"))
            {
                plain_destructive_clarification_reason(locale)
            } else {
                compiled.clarification_reason.clone()
            };
            Ok(KeyPoolIntentAssistDto {
                source: ASSIST_SOURCE_LIVE_KEYPOOL.to_owned(),
                summary: compiled.summary,
                target_domain: compiled.target_domain.as_str().to_owned(),
                undo_class: compiled.undo_class.as_str().to_owned(),
                needs_clarification: compiled.needs_clarification,
                clarification_reason,
                steps: compiled.steps,
                safety_escalated: compiled.safety_escalated,
                model: Some(compiled.model),
                served_slot_fingerprint: None,
                isolation_tag: compiled.isolation_tag.to_owned(),
                budget_receipt: compiled
                    .consumption_receipt
                    .map(KeyPoolBudgetReceiptDto::from),
                fallback_notice: None,
            })
        }
        // Budget exhaustion or ResourceAuthority failure is an authoritative policy gate: fail closed!
        Err(
            err @ (LlmDriverError::BudgetExhausted { .. } | LlmDriverError::ResourceAuthority(_)),
        ) => Err(map_driver_error_to_desktop(&err)),
        // Network unreachable / gateway offline / upstream transient error -> [DUI-OFFLINE-001] deterministic fallback!
        Err(err) => Ok(build_local_fallback_assist(
            trimmed_utterance,
            locale,
            &sanitize_driver_error(&err),
        )),
    }
}

async fn chat_blocking(
    ctx: &KeyPoolBridgeContext,
    trimmed_prompt: &str,
    input: &KeyPoolChatInput,
) -> Result<KeyPoolChatDto, DesktopError> {
    let discovered = discover_for_context(ctx).map_err(|e| map_driver_error_to_desktop(&e))?;
    let shim = LlmEnforcementShim::from_discovered(discovered);

    let mut req = LlmChatRequest::single_user_prompt(trimmed_prompt);
    if let Some(model) = input
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        req = req.with_model(model);
    }
    if let Some(max_tokens) = input.max_tokens {
        req = req.with_max_tokens(max_tokens);
    }
    if let Some(session_id) = input
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        req = req.with_session_id(session_id);
    }

    let opened_budget = open_optional_budget_session(
        ctx.resource_root.as_deref(),
        b"llmos/desktop-keypool/chat-op/v1",
        b"llmos/desktop-keypool/chat-idem/v1",
    )?;

    let outcome_res = match &opened_budget {
        Some(session) => {
            shim.execute_metered_chat(Some(session.as_context()), &req)
                .await
        }
        None => shim.execute_metered_chat(None, &req).await,
    };
    drop(opened_budget);

    let outcome = outcome_res.map_err(|e| map_driver_error_to_desktop(&e))?;
    Ok(KeyPoolChatDto {
        model: outcome.model,
        content: outcome.content,
        served_group: outcome.served_group,
        served_slot_fingerprint: outcome.served_slot_fingerprint,
        prompt_tokens: outcome.usage.prompt_tokens,
        completion_tokens: outcome.usage.completion_tokens,
        reasoning_tokens: outcome.usage.reasoning_tokens,
        total_tokens: outcome.usage.total_tokens,
        isolation_tag: outcome.isolation_tag.to_owned(),
        budget_receipt: outcome
            .consumption_receipt
            .map(KeyPoolBudgetReceiptDto::from),
    })
}

struct OpenedBudgetSession {
    resource: ResourceAuthority,
    account_id: ResourceAccountId,
    operation_id: nlos_types::OperationId,
    idempotency_key: nlos_types::IdempotencyKey,
    now_ms: u64,
}

impl OpenedBudgetSession {
    const fn as_context(&self) -> MeteredBudgetContext<'_> {
        MeteredBudgetContext {
            resource: &self.resource,
            account_id: self.account_id,
            operation_id: self.operation_id,
            idempotency_key: self.idempotency_key,
            now_ms: self.now_ms,
        }
    }
}

fn is_resource_budget_ready(resource_root: Option<&Path>) -> bool {
    let Some(res_dir) = resource_root else {
        return false;
    };
    ResourceAuthority::open(res_dir).is_ok()
}

fn open_optional_budget_session(
    resource_root: Option<&Path>,
    op_domain: &[u8],
    idem_domain: &[u8],
) -> Result<Option<OpenedBudgetSession>, DesktopError> {
    let Some(res_dir) = resource_root else {
        return Ok(None);
    };
    let resource = ResourceAuthority::open(res_dir)
        .map_err(|err| DesktopError::config(format!("无法打开本地资源预算存储: {err}")))?;

    let wall_ms = current_wall_ms();
    let budget = resolve_or_seed_trial_budget(&resource, res_dir, wall_ms)?;
    let now_ms = wall_ms.max(budget.updated_at_ms);

    let pid_bytes = std::process::id().to_be_bytes();
    let nanos_bytes = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |d| d.as_nanos())
        .to_be_bytes();
    let seq_bytes = budget.consumed_api_calls.saturating_add(1).to_be_bytes();
    let nonce_bytes = NEXT_BRIDGE_CALL_NONCE
        .fetch_add(1, Ordering::Relaxed)
        .to_be_bytes();
    let parts: [&[u8]; 5] = [
        budget.account_id.as_bytes(),
        &pid_bytes,
        &nanos_bytes,
        &seq_bytes,
        &nonce_bytes,
    ];

    Ok(Some(OpenedBudgetSession {
        resource,
        account_id: budget.account_id,
        operation_id: derive_operation_id(op_domain, &parts),
        idempotency_key: derive_idempotency_key(idem_domain, &parts),
        now_ms,
    }))
}

fn resolve_or_seed_trial_budget(
    resource: &ResourceAuthority,
    resource_dir: &Path,
    wall_ms: u64,
) -> Result<LlmTokenBudgetRecord, DesktopError> {
    // 优先检查 `<root>/trial-account.txt`（即 `resource_dir` 的父目录）或 `resource_dir/trial-account.txt`
    let mut candidate_markers = Vec::with_capacity(2);
    if let Some(parent) = resource_dir.parent() {
        candidate_markers.push(parent.join(TRIAL_ACCOUNT_MARKER_FILE));
    }
    candidate_markers.push(resource_dir.join(TRIAL_ACCOUNT_MARKER_FILE));

    for marker_path in candidate_markers {
        if let Some(account_id) = try_read_trial_account_marker(&marker_path)
            && let Ok(existing) = resource.inspect_llm_token_budget(account_id)
        {
            return Ok(existing);
        }
    }

    ensure_default_trial_llm_budget(resource, wall_ms)
        .map_err(|err| map_driver_error_to_desktop(&err))
}

fn try_read_trial_account_marker(path: &Path) -> Option<ResourceAccountId> {
    if !path.is_file() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.len() != 32 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0_u8; 16];
    for (idx, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&trimmed[2 * idx..2 * idx + 2], 16).ok()?;
    }
    Some(ResourceAccountId::from_bytes(bytes))
}

fn build_local_fallback_assist(
    utterance: &str,
    locale: &str,
    sanitized_reason: &str,
) -> KeyPoolIntentAssistDto {
    let domain = classify_local_domain(utterance);
    let destructive =
        contains_destructive_keyword(utterance) || contains_extra_high_risk_verb(utterance);
    let mutating = !destructive && contains_compensatable_verb(utterance);

    let (undo_class, needs_clarification, safety_escalated, clarification_reason) = if destructive {
        (
            IntentUndoClass::Irreversible,
            true,
            true,
            plain_destructive_clarification_reason(locale),
        )
    } else if mutating {
        (IntentUndoClass::Compensatable, false, false, String::new())
    } else {
        (IntentUndoClass::Lossless, false, false, String::new())
    };

    let steps = build_local_fallback_steps(utterance, domain, undo_class, locale);
    let fallback_notice = if locale.starts_with("en") {
        format!(
            "Local model pool is currently unavailable ({sanitized_reason}); switched to built-in deterministic plan breakdown."
        )
    } else {
        format!("本地模型池当前未连接（{sanitized_reason}），已自动切换至内置确定性方案拆解。")
    };

    KeyPoolIntentAssistDto {
        source: ASSIST_SOURCE_LOCAL_FALLBACK.to_owned(),
        summary: utterance.to_owned(),
        target_domain: domain.as_str().to_owned(),
        undo_class: undo_class.as_str().to_owned(),
        needs_clarification,
        clarification_reason,
        steps,
        safety_escalated,
        model: None,
        served_slot_fingerprint: None,
        isolation_tag: PROVIDER_ISOLATION_UNVERIFIED_TAG.to_owned(),
        budget_receipt: None,
        fallback_notice: Some(fallback_notice),
    }
}

fn classify_local_domain(utterance: &str) -> IntentTargetDomain {
    let lower = utterance.to_ascii_lowercase();
    if utterance.contains("设置")
        || utterance.contains("配置")
        || lower.contains("setting")
        || lower.contains("config")
    {
        IntentTargetDomain::Settings
    } else if utterance.contains("工作区") || lower.contains("workspace") {
        IntentTargetDomain::Workspace
    } else if utterance.contains("预算")
        || utterance.contains("资源")
        || lower.contains("budget")
        || lower.contains("resource")
    {
        IntentTargetDomain::Resource
    } else if utterance.contains("进程") || lower.contains("process") {
        IntentTargetDomain::Process
    } else if utterance.contains("应用")
        || utterance.contains("工作台")
        || utterance.contains("打开")
        || utterance.contains("启动")
        || lower.contains("app")
        || lower.contains("launch")
        || lower.contains("open")
    {
        IntentTargetDomain::Application
    } else if utterance.contains("文件")
        || utterance.contains("文档")
        || utterance.contains("备份")
        || utterance.contains("归档")
        || lower.contains("artifact")
        || lower.contains("file")
        || lower.contains("backup")
    {
        IntentTargetDomain::Artifact
    } else if utterance.contains("任务") || lower.contains("task") {
        IntentTargetDomain::Task
    } else {
        IntentTargetDomain::General
    }
}

fn contains_extra_high_risk_verb(utterance: &str) -> bool {
    let zh_high_risk = ["永久移除", "销毁", "格式化", "抹除"];
    if zh_high_risk.iter().any(|kw| utterance.contains(kw)) {
        return true;
    }
    let lower = utterance.to_ascii_lowercase();
    let en_high_risk = ["permanently remove", "destroy", "purge", "erase", "format"];
    en_high_risk.iter().any(|kw| lower.contains(kw))
}

fn contains_compensatable_verb(utterance: &str) -> bool {
    let zh_mut = ["转发", "导出", "修改", "更新", "迁移", "归档", "发送"];
    if zh_mut.iter().any(|kw| utterance.contains(kw)) {
        return true;
    }
    let lower = utterance.to_ascii_lowercase();
    let en_mut = [
        "forward", "export", "modify", "update", "migrate", "archive", "send",
    ];
    en_mut.iter().any(|kw| lower.contains(kw))
}

fn build_local_fallback_steps(
    utterance: &str,
    domain: IntentTargetDomain,
    undo_class: IntentUndoClass,
    locale: &str,
) -> Vec<String> {
    if locale.starts_with("en") {
        let mut steps = vec![
            format!(
                "Verify target scope in {} domain for: {utterance}",
                domain.as_str()
            ),
            "Check active workspace dependencies and allowance ceiling".to_owned(),
        ];
        if undo_class == IntentUndoClass::Irreversible {
            steps.push(
                "Require explicit candidate confirmation and system security approval before execution"
                    .to_owned(),
            );
        } else {
            steps.push("Preview reversible execution summary before running".to_owned());
        }
        steps
    } else {
        let mut steps = vec![
            format!("核对请求目标与作用范围：{utterance}"),
            "校验当前工作区已核验依赖与资源上限".to_owned(),
        ];
        if undo_class == IntentUndoClass::Irreversible {
            steps.push("执行前须点选明确候选解释并通过系统安全确认屏二次核准".to_owned());
        } else {
            steps.push("生成可回退行动方案预览并等待提交确认".to_owned());
        }
        steps
    }
}

fn plain_destructive_clarification_reason(locale: &str) -> String {
    if locale.starts_with("en") {
        "High-impact irreversible operation detected; please confirm the exact target scope before running."
            .to_owned()
    } else {
        "检测到高影响不可逆操作关键词，执行前请先明确目标范围并核对不可逆影响。".to_owned()
    }
}

fn normalize_locale(raw: Option<&str>) -> &'static str {
    match raw.map(str::trim) {
        Some(val) if val.to_ascii_lowercase().starts_with("en") => "en-US",
        _ => "zh-CN",
    }
}

/// 严格抹除宿主目录前缀，仅保留枚举标签与文件名（如 `config.json`）。
fn redacted_source_summary(source: &KeyPoolDiscoverySource) -> String {
    match source {
        KeyPoolDiscoverySource::ExplicitPath(path) => {
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| !n.is_empty())
                .unwrap_or("config.json");
            format!("explicit:{file_name}")
        }
        KeyPoolDiscoverySource::EnvConfigPath(path) => {
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| !n.is_empty())
                .unwrap_or("config.json");
            format!("env:LLMOS_KEYPOOL_CONFIG({file_name})")
        }
        KeyPoolDiscoverySource::EnvDirectBaseUrl => "env:LLMOS_LLM_BASE_URL".to_owned(),
        KeyPoolDiscoverySource::UserHomeDefault => "user-home:config.json".to_owned(),
    }
}

fn split_gateway_origin_and_path(openai_base_url: &str) -> (Option<String>, Option<String>) {
    let Ok(parsed) = HttpUrl::parse(openai_base_url) else {
        return (None, None);
    };
    let host = if parsed.host.contains(':') && !parsed.host.starts_with('[') {
        format!("[{}]", parsed.host)
    } else {
        parsed.host
    };
    (
        Some(format!("http://{host}:{}", parsed.port)),
        Some(parsed.path_and_query),
    )
}

/// 将驱动层错误转换为不含任何密钥、槽位名或宿主私有路径的诊断短语。
fn sanitize_driver_error(error: &LlmDriverError) -> String {
    match error {
        LlmDriverError::DiscoveryNotFound(_) => "no local keypool config discovered".to_owned(),
        LlmDriverError::ConfigIo(io_err) => format!("config file i/o ({:?})", io_err.kind()),
        LlmDriverError::InvalidConfig(_) => "invalid keypool config structure".to_owned(),
        LlmDriverError::InvalidUrl(_) => "invalid keypool loopback endpoint url".to_owned(),
        LlmDriverError::HttpTransport(_) => {
            "local keypool gateway connection refused or timed out".to_owned()
        }
        LlmDriverError::HttpTimeout { timeout_ms } => {
            format!("local keypool gateway timed out after {timeout_ms}ms")
        }
        LlmDriverError::UpstreamHttpStatus { status_code, .. } => {
            format!("upstream HTTP status {status_code}")
        }
        LlmDriverError::AllKeysUnavailable {
            total_keys,
            unavailable_keys,
        } => format!("all key slots unavailable ({unavailable_keys}/{total_keys})"),
        LlmDriverError::BudgetExhausted {
            remaining_api_calls,
            remaining_total_tokens,
            ..
        } => format!(
            "token budget exhausted (remaining_calls={remaining_api_calls}, remaining_tokens={remaining_total_tokens})"
        ),
        LlmDriverError::InvalidProviderPayload(_) | LlmDriverError::Json(_) => {
            "malformed provider completion payload".to_owned()
        }
        LlmDriverError::ResourceAuthority(_) => "resource authority budget ledger error".to_owned(),
    }
}

fn map_driver_error_to_desktop(error: &LlmDriverError) -> DesktopError {
    let summary = sanitize_driver_error(error);
    match error {
        LlmDriverError::DiscoveryNotFound(_)
        | LlmDriverError::ConfigIo(_)
        | LlmDriverError::InvalidConfig(_)
        | LlmDriverError::InvalidUrl(_) => DesktopError::config(summary),
        LlmDriverError::HttpTransport(_)
        | LlmDriverError::HttpTimeout { .. }
        | LlmDriverError::UpstreamHttpStatus { .. }
        | LlmDriverError::AllKeysUnavailable { .. }
        | LlmDriverError::InvalidProviderPayload(_)
        | LlmDriverError::Json(_) => DesktopError::ipc(summary),
        LlmDriverError::BudgetExhausted { .. } | LlmDriverError::ResourceAuthority(_) => {
            DesktopError::new(ErrorCode::Control, summary)
        }
    }
}
