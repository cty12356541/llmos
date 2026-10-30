//! Bounded local `HTTP/1.1` LLM provider driver plane (`W77-PRE-B` / `E0`).
//!
//! Mediates user-space `OpenAI`-compatible / local `glm-keypool` model calls
//! through [`LlmEnforcementShim`] (`DRV-CRED-001`, `DRV-SHIM-001`), enforcing
//! pre-dispatch and post-call 5-dimensional token accounting on
//! `nlos_resource::ResourceAuthority` (`[BUD-INT-001]`, `[BUD-ARITH-001]`) and
//! honest `ADR-0024` `[TASK-PROVIDER-001]` `PROVIDER_ISOLATION_UNVERIFIED`
//! tagging without introducing provider-specific syscalls (`KABI-SVC-001`).

#![forbid(unsafe_code)]

pub mod error;
pub mod http;
pub mod intent;
pub mod json;
pub mod keypool;
pub mod shim;

/// Canonical Cargo package identifier for the HTTP LLM provider driver crate.
pub const CRATE_NAME: &str = "nlos-driver-llm-http";

pub use error::LlmDriverError;
pub use http::{
    DEFAULT_HTTP_TIMEOUT_MS, DEFAULT_MAX_HTTP_BODY_BYTES, HttpClient, HttpMethod, HttpRequest,
    HttpResponse, HttpUrl, MAX_HTTP_HEADER_BYTES,
};
pub use intent::{
    DESTRUCTIVE_KEYWORDS_EN, DESTRUCTIVE_KEYWORDS_ZH, IntentTargetDomain, IntentUndoClass,
    KeyPoolCompiledIntent, contains_destructive_keyword, extract_json_object_payload,
};
pub use json::{JsonError, JsonNumber, JsonValue, MAX_JSON_DEPTH, write_escaped_json_str};
pub use keypool::{
    DEFAULT_KEYPOOL_HOST, DEFAULT_KEYPOOL_PORT, DEFAULT_PRIMARY_MODEL,
    DEFAULT_RATE_LIMIT_COOLDOWN_MS, DEFAULT_TRANSIENT_COOLDOWN_MS, DEFAULT_UPSTREAM_BASE,
    DirectKeyPoolConfig, DirectKeyPoolScheduler, DirectKeySlotConfig, DirectPoolHttpOutcome,
    DiscoveredKeyPoolConfig, FailoverDisposition, KeyPoolDiscoverySource, KeySelectionStrategy,
    REDACTED_CREDENTIAL_MARKER, SecretCredential, SelectedKeySlot, discover_local_keypool,
    discover_local_keypool_from_lookup, parse_glm_keypool_config_json, sha256_hex16,
};
pub use nlos_driver_mock::PROVIDER_ISOLATION_UNVERIFIED_TAG;
pub use shim::{
    DEFAULT_TRIAL_INITIAL_CREDIT, DEFAULT_TRIAL_LLM_LIMITS, KeyPoolHealthSnapshot, LlmChatMessage,
    LlmChatRequest, LlmChatRole, LlmEnforcementShim, LlmTokenUsage, MeteredBudgetContext,
    MeteredLlmChatOutcome, current_wall_ms, derive_idempotency_key, derive_operation_id,
    ensure_default_trial_llm_budget, format_receipt_id_hex, parse_redacted_pool_status_json,
};

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn scaffold_crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }
}
