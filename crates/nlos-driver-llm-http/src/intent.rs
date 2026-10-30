//! Natural language intent auxiliary compiler shared by the Desktop Omnibar
//! and `nlos-llm-pool-cli` (`[NL-AMBIG-001]`, `ADR-0024`).

use nlos_resource::LlmTokenConsumptionReceipt;

use crate::error::LlmDriverError;
use crate::json::JsonValue;
use crate::shim::{
    LlmChatMessage, LlmChatRequest, LlmEnforcementShim, LlmTokenUsage, MeteredBudgetContext,
    format_receipt_id_hex,
};

/// Destructive keywords in `zh-CN` and `en-US` that unconditionally trigger
/// `[NL-AMBIG-001]` safety escalation to `IRREVERSIBLE` + `needs_clarification = true`.
pub const DESTRUCTIVE_KEYWORDS_ZH: [&str; 3] = ["删除", "卸载", "清空"];
/// English destructive keywords matched case-insensitively.
pub const DESTRUCTIVE_KEYWORDS_EN: [&str; 3] = ["delete", "wipe", "uninstall"];

/// Canonical NLOS domain targeted by a compiled natural-language intent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentTargetDomain {
    Task,
    Application,
    Resource,
    Process,
    Workspace,
    Artifact,
    Settings,
    General,
}

impl IntentTargetDomain {
    /// Canonical lowercase domain identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Application => "application",
            Self::Resource => "resource",
            Self::Process => "process",
            Self::Workspace => "workspace",
            Self::Artifact => "artifact",
            Self::Settings => "settings",
            Self::General => "general",
        }
    }

    /// Parses a model-emitted domain label, defaulting unrecognized labels to [`Self::General`].
    #[must_use]
    pub fn from_model_str(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "task" | "tasks" => Self::Task,
            "application" | "applications" | "app" | "apps" => Self::Application,
            "resource" | "resources" | "budget" => Self::Resource,
            "process" | "processes" => Self::Process,
            "workspace" | "workspaces" => Self::Workspace,
            "artifact" | "artifacts" => Self::Artifact,
            "settings" | "setting" | "config" => Self::Settings,
            _ => Self::General,
        }
    }
}

/// Reversibility classification of a compiled intent (`[RCPT-UNDO-001]`, `[NL-AMBIG-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentUndoClass {
    /// Pure read or lossless reversible action (`"LOSSLESS"`).
    Lossless,
    /// Action reversible via compensating workflow (`"COMPENSATABLE"`).
    Compensatable,
    /// Destructive or external irreversible action (`"IRREVERSIBLE"`).
    Irreversible,
}

impl IntentUndoClass {
    /// Canonical uppercase undo classification token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lossless => "LOSSLESS",
            Self::Compensatable => "COMPENSATABLE",
            Self::Irreversible => "IRREVERSIBLE",
        }
    }

    /// Parses a model-emitted undo class, defaulting unknown values fail-safe to [`Self::Irreversible`].
    #[must_use]
    pub fn from_model_str(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "LOSSLESS" | "REVERSIBLE" | "READ_ONLY" => Self::Lossless,
            "COMPENSATABLE" => Self::Compensatable,
            _ => Self::Irreversible,
        }
    }
}

/// Structured intent plan returned by [`LlmEnforcementShim::compile_nl_intent`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyPoolCompiledIntent {
    pub summary: String,
    pub target_domain: IntentTargetDomain,
    pub undo_class: IntentUndoClass,
    pub needs_clarification: bool,
    pub clarification_reason: String,
    pub steps: Vec<String>,
    /// `true` when `[NL-AMBIG-001]` safety guardrail forced escalation due to destructive keywords.
    pub safety_escalated: bool,
    pub model: String,
    pub usage: LlmTokenUsage,
    pub consumption_receipt: Option<LlmTokenConsumptionReceipt>,
    pub isolation_tag: &'static str,
}

impl KeyPoolCompiledIntent {
    /// Serializes the compiled intent (and optional budget receipt summary) to [`JsonValue`].
    #[must_use]
    pub fn to_json_value(&self) -> JsonValue {
        let steps_arr = JsonValue::Array(
            self.steps
                .iter()
                .map(|s| JsonValue::from(s.as_str()))
                .collect(),
        );
        let receipt_val = match self.consumption_receipt {
            Some(r) => JsonValue::object([
                (
                    "receipt_id",
                    JsonValue::from(format_receipt_id_hex(r.receipt_id)),
                ),
                ("sequence", JsonValue::from(r.sequence)),
                ("prompt_tokens", JsonValue::from(r.prompt_tokens)),
                ("completion_tokens", JsonValue::from(r.completion_tokens)),
                ("reasoning_tokens", JsonValue::from(r.reasoning_tokens)),
                ("total_tokens", JsonValue::from(r.total_tokens)),
                (
                    "cumulative_total_tokens",
                    JsonValue::from(r.cumulative_total_tokens),
                ),
                (
                    "cumulative_api_calls",
                    JsonValue::from(r.cumulative_api_calls),
                ),
            ]),
            None => JsonValue::Null,
        };

        JsonValue::object([
            ("summary", JsonValue::from(self.summary.as_str())),
            (
                "target_domain",
                JsonValue::from(self.target_domain.as_str()),
            ),
            ("undo_class", JsonValue::from(self.undo_class.as_str())),
            (
                "needs_clarification",
                JsonValue::from(self.needs_clarification),
            ),
            (
                "clarification_reason",
                JsonValue::from(self.clarification_reason.as_str()),
            ),
            ("steps", steps_arr),
            ("safety_escalated", JsonValue::from(self.safety_escalated)),
            ("model", JsonValue::from(self.model.as_str())),
            ("isolation_tag", JsonValue::from(self.isolation_tag)),
            ("receipt", receipt_val),
        ])
    }
}

/// Returns `true` when `query` contains any destructive keyword requiring
/// `[NL-AMBIG-001]` mandatory `IRREVERSIBLE` + clarification escalation.
#[must_use]
pub fn contains_destructive_keyword(query: &str) -> bool {
    if DESTRUCTIVE_KEYWORDS_ZH.iter().any(|kw| query.contains(kw)) {
        return true;
    }
    let lower = query.to_ascii_lowercase();
    DESTRUCTIVE_KEYWORDS_EN.iter().any(|kw| lower.contains(kw))
}

/// Strips optional Markdown ```` ```json ... ``` ```` fences (or extracts the
/// outer `{ ... }` object slice) from a model completion string.
#[must_use]
pub fn extract_json_object_payload(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(after_open) = trimmed.strip_prefix("```") {
        let without_lang = if let Some(nl_idx) = after_open.find('\n') {
            let first_line = after_open[..nl_idx].trim();
            if first_line.is_empty()
                || first_line.eq_ignore_ascii_case("json")
                || first_line.eq_ignore_ascii_case("jsonc")
            {
                &after_open[nl_idx + 1..]
            } else {
                after_open
            }
        } else {
            after_open
        };
        if let Some(end_fence) = without_lang.rfind("```") {
            return without_lang[..end_fence].trim();
        }
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}'))
        && start < end
    {
        return &trimmed[start..=end];
    }
    trimmed
}

impl LlmEnforcementShim {
    /// Compiles a natural-language `query` into a structured [`KeyPoolCompiledIntent`]
    /// using `self.fast_intent_model()` by default, enforcing `[NL-AMBIG-001]`
    /// destructive-command safety escalation.
    ///
    /// # Errors
    /// Fails closed on empty query, budget exhaustion, transport failure, or malformed JSON output.
    pub async fn compile_nl_intent(
        &self,
        budget_ctx: Option<MeteredBudgetContext<'_>>,
        query: &str,
        locale: &str,
    ) -> Result<KeyPoolCompiledIntent, LlmDriverError> {
        self.compile_nl_intent_with_model(budget_ctx, query, locale, None)
            .await
    }

    /// Variant of [`Self::compile_nl_intent`] allowing an optional explicit `model_override`.
    ///
    /// # Errors
    /// Fails closed on empty query, budget exhaustion, transport failure, or malformed JSON output.
    pub async fn compile_nl_intent_with_model(
        &self,
        budget_ctx: Option<MeteredBudgetContext<'_>>,
        query: &str,
        locale: &str,
        model_override: Option<&str>,
    ) -> Result<KeyPoolCompiledIntent, LlmDriverError> {
        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return Err(LlmDriverError::InvalidProviderPayload(
                "NL intent query must not be empty",
            ));
        }

        let effective_locale = if locale.trim().is_empty() {
            "zh-CN"
        } else {
            locale.trim()
        };
        let model = model_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.fast_intent_model());

        let system_prompt = format!(
            "You are the NLOS Omnibar structured intent compiler (locale={effective_locale}). \
             Output ONLY a single compact RFC 8259 JSON object with exact schema: \
             {{\"summary\":\"...\",\"target_domain\":\"task|application|resource|process|workspace|artifact|settings|general\",\
             \"undo_class\":\"LOSSLESS|COMPENSATABLE|IRREVERSIBLE\",\"needs_clarification\":false,\
             \"clarification_reason\":\"\",\"steps\":[\"...\"]}}. Do not wrap in prose."
        );

        let chat_req = LlmChatRequest::new(vec![
            LlmChatMessage::system(system_prompt),
            LlmChatMessage::user(trimmed_query),
        ])
        .with_model(model)
        .with_temperature(0.1)
        .with_max_tokens(512);

        let outcome = self.execute_metered_chat(budget_ctx, &chat_req).await?;
        decode_compiled_intent_outcome(trimmed_query, effective_locale, outcome)
    }
}

fn decode_compiled_intent_outcome(
    trimmed_query: &str,
    effective_locale: &str,
    outcome: crate::shim::MeteredLlmChatOutcome,
) -> Result<KeyPoolCompiledIntent, LlmDriverError> {
    let json_slice = extract_json_object_payload(&outcome.content);
    let parsed = JsonValue::parse(json_slice)?;
    if parsed.as_object().is_none() {
        return Err(LlmDriverError::InvalidProviderPayload(
            "compiled NL intent payload must be a JSON object",
        ));
    }

    let summary = parsed
        .get("summary")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed_query)
        .to_owned();

    let target_domain = parsed
        .get("target_domain")
        .and_then(JsonValue::as_str)
        .map_or(
            IntentTargetDomain::General,
            IntentTargetDomain::from_model_str,
        );

    let mut undo_class = parsed.get("undo_class").and_then(JsonValue::as_str).map_or(
        IntentUndoClass::Irreversible,
        IntentUndoClass::from_model_str,
    );

    let mut needs_clarification = parsed
        .get("needs_clarification")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);

    let mut clarification_reason = parsed
        .get("clarification_reason")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();

    let mut steps = Vec::new();
    if let Some(arr) = parsed.get("steps").and_then(JsonValue::as_array) {
        for item in arr {
            if let Some(step) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                steps.push(step.to_owned());
            }
        }
    }
    if steps.is_empty() {
        steps.push(summary.clone());
    }

    // [NL-AMBIG-001] Mandatory destructive-keyword safety guardrail:
    // Never trust a model's LOSSLESS/COMPENSATABLE downgrade when the raw
    // user query names a destructive verb.
    let safety_escalated = if contains_destructive_keyword(trimmed_query) {
        undo_class = IntentUndoClass::Irreversible;
        needs_clarification = true;
        if clarification_reason.is_empty() {
            clarification_reason = default_destructive_clarification_reason(effective_locale);
        }
        true
    } else {
        false
    };

    Ok(KeyPoolCompiledIntent {
        summary,
        target_domain,
        undo_class,
        needs_clarification,
        clarification_reason,
        steps,
        safety_escalated,
        model: outcome.model,
        usage: outcome.usage,
        consumption_receipt: outcome.consumption_receipt,
        isolation_tag: outcome.isolation_tag,
    })
}

fn default_destructive_clarification_reason(locale: &str) -> String {
    if locale.to_ascii_lowercase().starts_with("en") {
        "Destructive keyword detected ([NL-AMBIG-001]); explicit target scope confirmation is required before irreversible execution.".to_owned()
    } else {
        "检测到破坏性操作关键词（[NL-AMBIG-001]），执行前须明确目标范围并确认不可逆影响。"
            .to_owned()
    }
}
