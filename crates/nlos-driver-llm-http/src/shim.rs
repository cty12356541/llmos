//! Trusted `LlmEnforcementShim` binding local `KeyPool` / `DirectKeyPoolScheduler`
//! transport to `nlos-resource` v8 5-dimensional LLM token budget accounting
//! and `ADR-0024` `[TASK-PROVIDER-001]` honest isolation tagging
//! (`DRV-CRED-001`, `DRV-SHIM-001`, `KABI-SVC-001`).

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_driver_mock::{
    PROVIDER_ISOLATION_UNVERIFIED_TAG, ProviderIsolationAttestation, ProviderIsolationProfile,
};
use nlos_resource::{
    ConsumeLlmTokensRequest, CreateAccountRequest, CreateLlmTokenBudgetRequest, LlmBudgetDimension,
    LlmTokenBudgetRecord, LlmTokenConsumptionReceipt, LlmTokenLimits, ResourceAuthority,
    ResourceAuthorityError,
};
use nlos_types::{IdempotencyKey, OperationId, ReceiptId, ResourceAccountId};
use sha2::{Digest, Sha256};

use crate::error::LlmDriverError;
use crate::http::HttpClient;
use crate::json::{JsonNumber, JsonValue};
use crate::keypool::{
    DEFAULT_PRIMARY_MODEL, DirectKeyPoolScheduler, DiscoveredKeyPoolConfig, discover_local_keypool,
    sha256_hex16, summarize_upstream_error_body,
};

/// Default bootstrap credit assigned when initializing a local trial account.
pub const DEFAULT_TRIAL_INITIAL_CREDIT: u64 = 100_000_000;

/// Default 5-dimensional LLM token budget ceilings for local trial sessions.
pub const DEFAULT_TRIAL_LLM_LIMITS: LlmTokenLimits = LlmTokenLimits {
    max_prompt_tokens: 10_000_000,
    max_completion_tokens: 10_000_000,
    max_total_tokens: 20_000_000,
    max_api_calls: 10_000,
    max_cost_microcredits: 100_000_000,
};

const TRIAL_ACCOUNT_BOOT_DOMAIN: &[u8] = b"nlos/driver-llm-http/trial-account/v1";
const TRIAL_BUDGET_BOOT_DOMAIN: &[u8] = b"nlos/driver-llm-http/trial-llm-budget/v1";

/// Derives a deterministic 128-bit [`IdempotencyKey`] from a domain separator and byte parts.
#[must_use]
pub fn derive_idempotency_key(domain: &[u8], parts: &[&[u8]]) -> IdempotencyKey {
    IdempotencyKey::from_bytes(hash16(domain, parts))
}

/// Derives a deterministic 128-bit [`OperationId`] from a domain separator and byte parts.
#[must_use]
pub fn derive_operation_id(domain: &[u8], parts: &[&[u8]]) -> OperationId {
    OperationId::from_bytes(hash16(domain, parts))
}

/// Formats a 128-bit [`ReceiptId`] as 32 lowercase hexadecimal characters.
#[must_use]
pub fn format_receipt_id_hex(receipt_id: ReceiptId) -> String {
    let mut out = String::with_capacity(32);
    for byte in receipt_id.as_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn hash16(domain: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0xFF]);
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Idempotently initializes (or replays) the default local trial `ResourceAccount`
/// and 5-dimensional `LlmTokenBudget` inside `resource`.
///
/// # Errors
/// Fails closed if `ResourceAuthority` storage or idempotency verification fails.
pub fn ensure_default_trial_llm_budget(
    resource: &ResourceAuthority,
    now_ms: u64,
) -> Result<LlmTokenBudgetRecord, LlmDriverError> {
    let account_key = derive_idempotency_key(TRIAL_ACCOUNT_BOOT_DOMAIN, &[b"default-trial"]);
    let account = resource.create_account(CreateAccountRequest {
        initial_credit: DEFAULT_TRIAL_INITIAL_CREDIT,
        idempotency_key: account_key,
        created_at_ms: now_ms,
    })?;

    if let Ok(existing) = resource.inspect_llm_token_budget(account.account_id) {
        return Ok(existing);
    }

    let budget_key =
        derive_idempotency_key(TRIAL_BUDGET_BOOT_DOMAIN, &[account.account_id.as_bytes()]);
    let decision = resource.create_llm_token_budget(CreateLlmTokenBudgetRequest {
        account_id: account.account_id,
        limits: DEFAULT_TRIAL_LLM_LIMITS,
        idempotency_key: budget_key,
        created_at_ms: account.created_at_ms,
    })?;
    Ok(decision.record())
}

/// Redacted aggregate health telemetry returned by `GET /__pool__/status` or
/// [`DirectKeyPoolScheduler`].
///
/// Strictly omits `keys[*]` masks, labels, and human owner names (`DRV-CRED-001`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyPoolHealthSnapshot {
    pub strategy: String,
    pub total_keys: u64,
    pub active_keys: u64,
    pub active_sessions: u64,
    pub total_requests: u64,
    pub ok_requests: u64,
    pub failed_requests: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// Authoritative `ResourceAuthority` accounting context supplied to metered
/// LLM invocations.
#[derive(Clone, Copy)]
pub struct MeteredBudgetContext<'a> {
    pub resource: &'a ResourceAuthority,
    pub account_id: ResourceAccountId,
    pub operation_id: OperationId,
    pub idempotency_key: IdempotencyKey,
    pub now_ms: u64,
}

/// Chat message role in `OpenAI`-compatible `/chat/completions` payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LlmChatRole {
    /// `"system"`
    System,
    /// `"user"`
    User,
    /// `"assistant"`
    Assistant,
}

impl LlmChatRole {
    /// Wire role identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One message turn in [`LlmChatRequest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlmChatMessage {
    pub role: LlmChatRole,
    pub content: String,
}

impl LlmChatMessage {
    /// Creates a `system` role message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: LlmChatRole::System,
            content: content.into(),
        }
    }

    /// Creates a `user` role message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: LlmChatRole::User,
            content: content.into(),
        }
    }

    /// Creates an `assistant` role message.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: LlmChatRole::Assistant,
            content: content.into(),
        }
    }
}

/// Outbound chat completion parameters mediated by [`LlmEnforcementShim`].
#[derive(Clone, Debug, PartialEq)]
pub struct LlmChatRequest {
    pub model: Option<String>,
    pub messages: Vec<LlmChatMessage>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub session_id: Option<String>,
    pub cost_microcredits: u64,
}

impl LlmChatRequest {
    /// Constructs a request from an ordered list of chat messages.
    #[must_use]
    pub const fn new(messages: Vec<LlmChatMessage>) -> Self {
        Self {
            model: None,
            messages,
            max_tokens: None,
            temperature: None,
            session_id: None,
            cost_microcredits: 0,
        }
    }

    /// Constructs a single-turn user prompt request.
    #[must_use]
    pub fn single_user_prompt(prompt: impl Into<String>) -> Self {
        Self::new(vec![LlmChatMessage::user(prompt)])
    }

    /// Overrides the target model identifier.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Sets `max_tokens` ceiling on the completion request.
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets sampling `temperature`.
    #[must_use]
    pub const fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Binds a sticky `x-pool-session` identifier to preserve GLM prefix cache affinity.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Attaches an explicit microcredit charge for this invocation.
    #[must_use]
    pub const fn with_cost_microcredits(mut self, cost_microcredits: u64) -> Self {
        self.cost_microcredits = cost_microcredits;
        self
    }
}

/// Parsed (or `UTF-8` byte-estimated fallback) token usage counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LlmTokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub estimated_fallback: bool,
}

/// Complete metered execution outcome returned by [`LlmEnforcementShim::execute_metered_chat`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MeteredLlmChatOutcome {
    pub model: String,
    pub content: String,
    pub reasoning_content: Option<String>,
    pub usage: LlmTokenUsage,
    pub served_group: Option<String>,
    pub served_slot_fingerprint: Option<String>,
    pub consumption_receipt: Option<LlmTokenConsumptionReceipt>,
    pub isolation_tag: &'static str,
}

#[derive(Clone, Debug)]
enum ShimBackend {
    LocalGateway(DiscoveredKeyPoolConfig),
    InProcessPool {
        scheduler: Arc<DirectKeyPoolScheduler>,
        default_model: String,
        fast_intent_model: String,
        allowed_models: Vec<String>,
    },
}

/// Trusted `EnforcementShim` (`DRV-SHIM-001`, `DRV-CRED-001`) mediating
/// credential custody, pre-dispatch 5D token budget gating, post-call durable
/// receipt charging, slot identity redaction, and `ADR-0024` isolation posture.
#[derive(Clone, Debug)]
pub struct LlmEnforcementShim {
    backend: ShimBackend,
    http_client: HttpClient,
}

impl LlmEnforcementShim {
    /// Discovers the local `KeyPool` configuration and constructs an [`LlmEnforcementShim`].
    ///
    /// # Errors
    /// Fails closed if `KeyPool` discovery or config parsing fails.
    pub fn discover(override_path: Option<&Path>) -> Result<Self, LlmDriverError> {
        let discovered = discover_local_keypool(override_path)?;
        Ok(Self::from_discovered(discovered))
    }

    /// Wraps an already-parsed [`DiscoveredKeyPoolConfig`] with default `HTTP/1.1` settings.
    #[must_use]
    pub fn from_discovered(config: DiscoveredKeyPoolConfig) -> Self {
        Self {
            backend: ShimBackend::LocalGateway(config),
            http_client: HttpClient::default(),
        }
    }

    /// Wraps an in-process [`DirectKeyPoolScheduler`] when running without a local gateway daemon.
    #[must_use]
    pub fn from_direct_pool(
        scheduler: Arc<DirectKeyPoolScheduler>,
        default_model: impl Into<String>,
        fast_intent_model: impl Into<String>,
        allowed_models: Vec<String>,
    ) -> Self {
        let primary = default_model.into();
        let fast = fast_intent_model.into();
        Self {
            backend: ShimBackend::InProcessPool {
                scheduler,
                default_model: if primary.is_empty() {
                    DEFAULT_PRIMARY_MODEL.to_owned()
                } else {
                    primary
                },
                fast_intent_model: if fast.is_empty() {
                    DEFAULT_PRIMARY_MODEL.to_owned()
                } else {
                    fast
                },
                allowed_models,
            },
            http_client: HttpClient::default(),
        }
    }

    /// Overrides the underlying bounded [`HttpClient`] (e.g. custom request timeout).
    #[must_use]
    pub const fn with_http_client(mut self, http_client: HttpClient) -> Self {
        self.http_client = http_client;
        self
    }

    /// Returns the discovered local gateway configuration when running in gateway mode.
    #[must_use]
    pub const fn discovered_config(&self) -> Option<&DiscoveredKeyPoolConfig> {
        match &self.backend {
            ShimBackend::LocalGateway(cfg) => Some(cfg),
            ShimBackend::InProcessPool { .. } => None,
        }
    }

    /// Active `OpenAI`-compatible base URL.
    #[must_use]
    pub fn openai_base_url(&self) -> &str {
        match &self.backend {
            ShimBackend::LocalGateway(cfg) => &cfg.openai_base_url,
            ShimBackend::InProcessPool { scheduler, .. } => scheduler.openai_base_url(),
        }
    }

    /// Default primary chat/reasoning model (e.g. `"glm-5.3"`).
    #[must_use]
    pub fn default_model(&self) -> &str {
        match &self.backend {
            ShimBackend::LocalGateway(cfg) => &cfg.default_model,
            ShimBackend::InProcessPool { default_model, .. } => default_model,
        }
    }

    /// Fast low-latency intent compilation model (e.g. `"glm-5.3-flash"`).
    #[must_use]
    pub fn fast_intent_model(&self) -> &str {
        match &self.backend {
            ShimBackend::LocalGateway(cfg) => &cfg.fast_intent_model,
            ShimBackend::InProcessPool {
                fast_intent_model, ..
            } => fast_intent_model,
        }
    }

    /// Allowed model identifiers aggregated from configuration.
    #[must_use]
    pub fn allowed_models(&self) -> &[String] {
        match &self.backend {
            ShimBackend::LocalGateway(cfg) => &cfg.allowed_models,
            ShimBackend::InProcessPool { allowed_models, .. } => allowed_models,
        }
    }

    /// Honest `ADR-0024` `[TASK-PROVIDER-001]` isolation attestation for external
    /// HTTP/cloud model endpoints (`PROVIDER_ISOLATION_UNVERIFIED`).
    #[must_use]
    pub fn isolation_attestation() -> ProviderIsolationAttestation {
        ProviderIsolationProfile::SharedCacheUnverified.to_attestation(None)
    }

    /// Probes `GET /__pool__/status` (or inspects the in-process scheduler) and
    /// returns a 100% redacted [`KeyPoolHealthSnapshot`].
    ///
    /// # Errors
    /// Fails closed if no `/__pool__/status` URL is configured, transport fails,
    /// or upstream returns non-2xx / malformed JSON.
    pub async fn probe_pool_status(&self) -> Result<KeyPoolHealthSnapshot, LlmDriverError> {
        match &self.backend {
            ShimBackend::LocalGateway(cfg) => {
                let status_url = cfg.pool_status_url.as_deref().ok_or_else(|| {
                    LlmDriverError::InvalidConfig(
                        "pool_status_url is not available for direct LLMOS_LLM_BASE_URL endpoints"
                            .to_owned(),
                    )
                })?;
                let mut headers: Vec<(&str, &str)> = vec![("Accept", "application/json")];
                let auth_val;
                if !cfg.egress_key.is_empty() {
                    auth_val = format!("Bearer {}", cfg.egress_key.expose_secret());
                    headers.push(("Authorization", auth_val.as_str()));
                }
                let resp = self.http_client.get(status_url, &headers).await?;
                if !(200..300).contains(&resp.status_code) {
                    return Err(LlmDriverError::UpstreamHttpStatus {
                        status_code: resp.status_code,
                        retry_after_secs: resp.retry_after_secs(),
                        summary: summarize_upstream_error_body(&resp.body),
                    });
                }
                let body_str = resp.body_utf8()?;
                parse_redacted_pool_status_json(body_str)
            }
            ShimBackend::InProcessPool { scheduler, .. } => {
                let now_ms = current_wall_ms();
                let (total_keys, active_keys, total_requests, ok_requests, failed_requests) =
                    scheduler.aggregate_counters_at(now_ms);
                Ok(KeyPoolHealthSnapshot {
                    strategy: scheduler.strategy().as_str().to_owned(),
                    total_keys,
                    active_keys,
                    active_sessions: 0,
                    total_requests,
                    ok_requests,
                    failed_requests,
                    tokens_in: 0,
                    tokens_out: 0,
                })
            }
        }
    }

    /// Executes a metered `/chat/completions` call:
    /// 1. Pre-dispatch fail-closed check against `ResourceAuthority` (`remaining_api_calls() > 0 && remaining_total_tokens() > 0`);
    /// 2. Outbound `HTTP/1.1` dispatch with `Authorization: Bearer` and `x-pool-session`;
    /// 3. Usage extraction (with `UTF-8` byte fallback `>= 1`) and `SHA-256` 16-hex slot redaction;
    /// 4. Post-call `ResourceAuthority::consume_llm_tokens` durable receipt issuance;
    /// 5. Honest `PROVIDER_ISOLATION_UNVERIFIED` tagging (`ADR-0024`).
    ///
    /// # Errors
    /// Fails closed before network I/O when budget is exhausted, or on transport/upstream/accounting failure.
    pub async fn execute_metered_chat(
        &self,
        budget_ctx: Option<MeteredBudgetContext<'_>>,
        req: &LlmChatRequest,
    ) -> Result<MeteredLlmChatOutcome, LlmDriverError> {
        if req.messages.is_empty() {
            return Err(LlmDriverError::InvalidProviderPayload(
                "LlmChatRequest messages must not be empty",
            ));
        }

        // 1. Pre-dispatch budget admission check (fail-closed, zero network packets on exhaustion).
        let pre_budget = budget_ctx
            .map(|ctx| enforce_pre_dispatch_budget_gate(ctx, req.cost_microcredits))
            .transpose()?;

        // 2. Construct outbound JSON body.
        let requested_model = req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.default_model());
        let payload_bytes = encode_chat_request_json(requested_model, req)?;
        let session_id = req
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // 3. Dispatch HTTP request via gateway or in-process pool scheduler.
        let call_now_ms = budget_ctx.map_or_else(current_wall_ms, |c| c.now_ms);
        let (resp, fallback_group, fallback_fingerprint) = self
            .dispatch_chat_http(&payload_bytes, session_id, call_now_ms)
            .await?;

        // 4. Parse response JSON, extract content/reasoning/usage, and redact pool headers.
        let body_text = resp.body_utf8()?;
        let parsed = JsonValue::parse(body_text)?;
        let (resolved_model, content, reasoning_content, usage) =
            extract_chat_completion_fields(&parsed, requested_model, req)?;

        let served_group = resp
            .header("x-pool-group")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(sanitize_identifier_token)
            .or(fallback_group);

        let served_slot_fingerprint = resp
            .header("x-pool-key")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(sha256_hex16)
            .or(fallback_fingerprint);

        // 5. Post-call authoritative 5D token charge.
        let consumption_receipt = budget_ctx
            .map(|ctx| {
                charge_post_call_tokens(ctx, pre_budget.as_ref(), usage, req.cost_microcredits)
            })
            .transpose()?;

        Ok(MeteredLlmChatOutcome {
            model: resolved_model,
            content,
            reasoning_content,
            usage,
            served_group,
            served_slot_fingerprint,
            consumption_receipt,
            isolation_tag: PROVIDER_ISOLATION_UNVERIFIED_TAG,
        })
    }

    async fn dispatch_chat_http(
        &self,
        payload_bytes: &[u8],
        session_id: Option<&str>,
        now_ms: u64,
    ) -> Result<(crate::http::HttpResponse, Option<String>, Option<String>), LlmDriverError> {
        let endpoint = format!(
            "{}/chat/completions",
            self.openai_base_url().trim_end_matches('/')
        );
        match &self.backend {
            ShimBackend::LocalGateway(cfg) => {
                let mut headers: Vec<(&str, &str)> = vec![("Accept", "application/json")];
                let auth_val;
                if !cfg.egress_key.is_empty() {
                    auth_val = format!("Bearer {}", cfg.egress_key.expose_secret());
                    headers.push(("Authorization", auth_val.as_str()));
                }
                if let Some(sid) = session_id {
                    headers.push(("x-pool-session", sid));
                }
                let resp = self
                    .http_client
                    .post_json(&endpoint, &headers, payload_bytes)
                    .await?;
                if !(200..300).contains(&resp.status_code) {
                    return Err(LlmDriverError::UpstreamHttpStatus {
                        status_code: resp.status_code,
                        retry_after_secs: resp.retry_after_secs(),
                        summary: summarize_upstream_error_body(&resp.body),
                    });
                }
                Ok((resp, None, None))
            }
            ShimBackend::InProcessPool { scheduler, .. } => {
                let outcome = scheduler
                    .post_json_with_failover(
                        &self.http_client,
                        &endpoint,
                        session_id,
                        payload_bytes,
                        now_ms,
                    )
                    .await?;
                Ok((
                    outcome.response,
                    Some(outcome.selected_slot.group),
                    Some(outcome.selected_slot.slot_fingerprint),
                ))
            }
        }
    }
}

fn enforce_pre_dispatch_budget_gate(
    ctx: MeteredBudgetContext<'_>,
    cost_microcredits: u64,
) -> Result<LlmTokenBudgetRecord, LlmDriverError> {
    let budget = ctx.resource.inspect_llm_token_budget(ctx.account_id)?;
    if budget.remaining_api_calls() == 0
        || budget.remaining_total_tokens() == 0
        || cost_microcredits > budget.remaining_cost_microcredits()
    {
        let exhausted_dimension = if budget.remaining_api_calls() == 0 {
            Some(LlmBudgetDimension::ApiCalls)
        } else if budget.remaining_total_tokens() == 0 {
            Some(LlmBudgetDimension::TotalTokens)
        } else {
            Some(LlmBudgetDimension::CostMicrocredits)
        };
        return Err(LlmDriverError::BudgetExhausted {
            account_id: ctx.account_id,
            exhausted_dimension,
            remaining_api_calls: budget.remaining_api_calls(),
            remaining_total_tokens: budget.remaining_total_tokens(),
        });
    }
    Ok(budget)
}

fn charge_post_call_tokens(
    ctx: MeteredBudgetContext<'_>,
    pre_budget: Option<&LlmTokenBudgetRecord>,
    usage: LlmTokenUsage,
    cost_microcredits: u64,
) -> Result<LlmTokenConsumptionReceipt, LlmDriverError> {
    let min_ts = pre_budget.map_or(ctx.now_ms, |b| b.created_at_ms);
    let consumed_at_ms = ctx.now_ms.max(min_ts);
    match ctx.resource.consume_llm_tokens(ConsumeLlmTokensRequest {
        account_id: ctx.account_id,
        operation_id: ctx.operation_id,
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        api_calls: 1,
        cost_microcredits,
        idempotency_key: ctx.idempotency_key,
        consumed_at_ms,
    }) {
        Ok(decision) => Ok(decision.receipt()),
        Err(ResourceAuthorityError::LlmTokenBudgetExhausted { dimension, .. }) => {
            let latest = ctx.resource.inspect_llm_token_budget(ctx.account_id).ok();
            Err(LlmDriverError::BudgetExhausted {
                account_id: ctx.account_id,
                exhausted_dimension: Some(dimension),
                remaining_api_calls: latest.map_or(0, |b| b.remaining_api_calls()),
                remaining_total_tokens: latest.map_or(0, |b| b.remaining_total_tokens()),
            })
        }
        Err(other) => Err(LlmDriverError::ResourceAuthority(other)),
    }
}

fn encode_chat_request_json(model: &str, req: &LlmChatRequest) -> Result<Vec<u8>, LlmDriverError> {
    let msgs: Vec<JsonValue> = req
        .messages
        .iter()
        .map(|m| {
            JsonValue::object([
                ("role", JsonValue::from(m.role.as_str())),
                ("content", JsonValue::from(m.content.as_str())),
            ])
        })
        .collect();

    let mut entries: Vec<(&str, JsonValue)> = vec![
        ("model", JsonValue::from(model)),
        ("messages", JsonValue::Array(msgs)),
        ("stream", JsonValue::from(false)),
    ];
    if let Some(max_tok) = req.max_tokens {
        entries.push(("max_tokens", JsonValue::from(max_tok)));
    }
    if let Some(temp) = req.temperature {
        let num = JsonNumber::from_f64(temp).ok_or(LlmDriverError::InvalidProviderPayload(
            "temperature must be a finite number",
        ))?;
        entries.push(("temperature", JsonValue::Number(num)));
    }
    Ok(JsonValue::object(entries).to_json_string().into_bytes())
}

fn extract_chat_completion_fields(
    root: &JsonValue,
    requested_model: &str,
    req: &LlmChatRequest,
) -> Result<(String, String, Option<String>, LlmTokenUsage), LlmDriverError> {
    let resolved_model = root
        .get("model")
        .and_then(JsonValue::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(requested_model)
        .to_owned();

    let message = root
        .get("choices")
        .and_then(|c| c.get_index(0))
        .and_then(|choice| choice.get("message"))
        .ok_or(LlmDriverError::InvalidProviderPayload(
            "missing choices[0].message in chat completion response",
        ))?;

    let reasoning_content = message
        .get("reasoning_content")
        .and_then(JsonValue::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let content = match message.get("content") {
        Some(JsonValue::String(s)) => s.clone(),
        Some(JsonValue::Null) if reasoning_content.is_some() => String::new(),
        _ => {
            return Err(LlmDriverError::InvalidProviderPayload(
                "missing string choices[0].message.content in chat completion response",
            ));
        }
    };

    let usage = extract_or_estimate_usage(root, req, &content, reasoning_content.as_deref());
    Ok((resolved_model, content, reasoning_content, usage))
}

fn extract_or_estimate_usage(
    root: &JsonValue,
    req: &LlmChatRequest,
    content: &str,
    reasoning: Option<&str>,
) -> LlmTokenUsage {
    if let Some(usage_obj) = root.get("usage").filter(|u| u.as_object().is_some()) {
        let prompt_opt = usage_obj.get("prompt_tokens").and_then(JsonValue::as_u64);
        let completion_opt = usage_obj
            .get("completion_tokens")
            .and_then(JsonValue::as_u64);
        let reasoning_tokens = usage_obj
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);

        if let (Some(prompt_tokens), Some(completion_tokens)) = (prompt_opt, completion_opt) {
            let computed_total = prompt_tokens
                .saturating_add(completion_tokens)
                .saturating_add(reasoning_tokens);
            let total_tokens = usage_obj
                .get("total_tokens")
                .and_then(JsonValue::as_u64)
                .unwrap_or(computed_total)
                .max(1);
            return LlmTokenUsage {
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                total_tokens,
                estimated_fallback: false,
            };
        }
    }

    let prompt_bytes: usize = req.messages.iter().map(|m| m.content.len()).sum();
    let prompt_tokens = estimate_tokens_from_utf8_bytes(prompt_bytes);
    let completion_tokens = estimate_tokens_from_utf8_bytes(content.len());
    let reasoning_tokens = reasoning.map_or(0, |r| estimate_tokens_from_utf8_bytes(r.len()));
    let total_tokens = prompt_tokens
        .saturating_add(completion_tokens)
        .saturating_add(reasoning_tokens)
        .max(1);

    LlmTokenUsage {
        prompt_tokens,
        completion_tokens,
        reasoning_tokens,
        total_tokens,
        estimated_fallback: true,
    }
}

const fn estimate_tokens_from_utf8_bytes(byte_len: usize) -> u64 {
    let approx = (byte_len as u64).saturating_add(3) / 4;
    if approx == 0 { 1 } else { approx }
}

/// Parses `GET /__pool__/status` JSON into [`KeyPoolHealthSnapshot`], dropping
/// all `keys[*]` masks, labels, and human owner strings.
///
/// # Errors
/// Fails closed if `json_text` is not a valid JSON object.
pub fn parse_redacted_pool_status_json(
    json_text: &str,
) -> Result<KeyPoolHealthSnapshot, LlmDriverError> {
    let root = JsonValue::parse(json_text)?;
    if root.as_object().is_none() {
        return Err(LlmDriverError::InvalidProviderPayload(
            "/__pool__/status response must be a JSON object",
        ));
    }

    let raw_strategy = root
        .get("strategy")
        .or_else(|| root.get("mode"))
        .and_then(JsonValue::as_str)
        .unwrap_or("unknown");
    let strategy = sanitize_strategy_label(raw_strategy);

    let keys_arr = root.get("keys").and_then(JsonValue::as_array);
    let fallback_total_keys = keys_arr.map_or(0, |arr| arr.len() as u64);
    let fallback_active_keys = keys_arr.map_or(0, |arr| {
        arr.iter()
            .filter(|k| {
                k.get("active").and_then(JsonValue::as_bool) == Some(true)
                    || k.get("healthy").and_then(JsonValue::as_bool) == Some(true)
                    || (k.get("disabled").and_then(JsonValue::as_bool) != Some(true)
                        && k.get("active").is_none()
                        && k.get("healthy").is_none())
            })
            .count() as u64
    });

    let stats = root
        .get("stats")
        .or_else(|| root.get("totals"))
        .or_else(|| root.get("summary"));

    let total_keys =
        lookup_u64(&root, stats, &["total_keys", "totalKeys"]).unwrap_or(fallback_total_keys);
    let active_keys = lookup_u64(
        &root,
        stats,
        &["active_keys", "activeKeys", "healthyKeys", "healthy_keys"],
    )
    .unwrap_or(fallback_active_keys);
    let active_sessions = lookup_u64(
        &root,
        stats,
        &[
            "active_sessions",
            "activeSessions",
            "stickySessions",
            "sessions",
        ],
    )
    .unwrap_or(0);
    let total_requests = lookup_u64(
        &root,
        stats,
        &["total_requests", "totalRequests", "requests"],
    )
    .unwrap_or(0);
    let ok_requests = lookup_u64(&root, stats, &["ok_requests", "okRequests", "ok"]).unwrap_or(0);
    let failed_requests = lookup_u64(
        &root,
        stats,
        &["failed_requests", "failedRequests", "errors", "failed"],
    )
    .unwrap_or(0);
    let tokens_in = lookup_u64(
        &root,
        stats,
        &["tokens_in", "tokensIn", "prompt_tokens", "inputTokens"],
    )
    .unwrap_or(0);
    let tokens_out = lookup_u64(
        &root,
        stats,
        &[
            "tokens_out",
            "tokensOut",
            "completion_tokens",
            "outputTokens",
        ],
    )
    .unwrap_or(0);

    Ok(KeyPoolHealthSnapshot {
        strategy,
        total_keys,
        active_keys,
        active_sessions,
        total_requests,
        ok_requests,
        failed_requests,
        tokens_in,
        tokens_out,
    })
}

fn lookup_u64(root: &JsonValue, nested: Option<&JsonValue>, names: &[&str]) -> Option<u64> {
    for name in names {
        if let Some(v) = root.get(name).and_then(JsonValue::as_u64) {
            return Some(v);
        }
        if let Some(v) = nested.and_then(|n| n.get(name)).and_then(JsonValue::as_u64) {
            return Some(v);
        }
    }
    None
}

fn sanitize_strategy_label(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "sticky" => "sticky".to_owned(),
        "round-robin" | "round_robin" | "rr" => "round-robin".to_owned(),
        "least-busy" | "least_busy" => "least-busy".to_owned(),
        "weighted" => "weighted".to_owned(),
        "random" => "random".to_owned(),
        _ => sanitize_identifier_token(raw),
    }
}

fn sanitize_identifier_token(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.trim().chars().take(32) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        }
    }
    if out.is_empty() {
        "unknown".to_owned()
    } else {
        out
    }
}

/// Returns current Unix epoch milliseconds (saturating to `0` on pre-epoch clock).
#[must_use]
pub fn current_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}
