//! Local `glm-keypool` v2 auto-discovery, zero-leak [`SecretCredential`], and
//! in-process multi-key failover scheduler [`DirectKeyPoolScheduler`]
//! (`DRV-CRED-001`, `[PD-COMMIT-004]`).

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest, Sha256};

use crate::error::LlmDriverError;
use crate::http::{HttpClient, HttpResponse, HttpUrl};
use crate::json::JsonValue;

/// Default local `glm-keypool` listen host.
pub const DEFAULT_KEYPOOL_HOST: &str = "127.0.0.1";
/// Default local `glm-keypool` listen port.
pub const DEFAULT_KEYPOOL_PORT: u16 = 8317;
/// Default upstream base in `glm-keypool` v2.
pub const DEFAULT_UPSTREAM_BASE: &str = "https://open.bigmodel.cn";
/// Default primary model when none is specified in config or environment.
pub const DEFAULT_PRIMARY_MODEL: &str = "glm-5.3";
/// Default rate-limit (`429`) cooldown duration in milliseconds (`30s`).
pub const DEFAULT_RATE_LIMIT_COOLDOWN_MS: u64 = 30_000;
/// Default transient (`5xx`) error cooldown duration in milliseconds (`5s`).
pub const DEFAULT_TRANSIENT_COOLDOWN_MS: u64 = 5_000;

/// Redaction placeholder emitted by [`SecretCredential`] in `Debug` and `Display`.
pub const REDACTED_CREDENTIAL_MARKER: &str = "<redacted-credential>";

/// Computes the first 16 lowercase hexadecimal characters (64-bit prefix) of
/// `SHA-256(input)` so slot identifiers and pool keys never leak raw labels or
/// person names into receipts or logs.
#[must_use]
pub fn sha256_hex16(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Credential wrapper enforcing `DRV-CRED-001`: `fmt::Debug` and `fmt::Display`
/// unconditionally emit `"<redacted-credential>"`, and raw access is restricted
/// to crate-internal outbound `EnforcementShim` HTTP header injection.
#[derive(Clone, Default)]
pub struct SecretCredential {
    secret: String,
}

impl SecretCredential {
    /// Wraps a raw secret token or API key.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self { secret: raw.into() }
    }

    /// Creates an empty (unauthenticated/keyless) credential.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            secret: String::new(),
        }
    }

    /// Returns `true` when no secret token is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.secret.is_empty()
    }

    /// Returns a 16-hex-character `SHA-256` fingerprint when non-empty.
    #[must_use]
    pub fn fingerprint_hex16(&self) -> Option<String> {
        if self.secret.is_empty() {
            None
        } else {
            Some(sha256_hex16(&self.secret))
        }
    }

    /// Crate-private raw secret accessor exclusively for `Authorization` header
    /// construction inside `LlmEnforcementShim` / `DirectKeyPoolScheduler`.
    #[must_use]
    pub(crate) fn expose_secret(&self) -> &str {
        &self.secret
    }
}

impl fmt::Debug for SecretCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_CREDENTIAL_MARKER)
    }
}

impl fmt::Display for SecretCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED_CREDENTIAL_MARKER)
    }
}

/// Provenance source from which the active `KeyPool` configuration was resolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyPoolDiscoverySource {
    /// Priority 1: caller-supplied explicit `override_path`.
    ExplicitPath(PathBuf),
    /// Priority 2: `LLMOS_KEYPOOL_CONFIG` environment variable path.
    EnvConfigPath(PathBuf),
    /// Priority 3: `LLMOS_LLM_BASE_URL` direct environment variables.
    EnvDirectBaseUrl,
    /// Priority 4: auto-probed `$USERPROFILE/glm-keypool/config.json` or `$HOME/glm-keypool/config.json`.
    UserHomeDefault,
}

impl KeyPoolDiscoverySource {
    /// Privacy-safe human-readable label omitting host user home paths (`[PD-COMMIT-004]`).
    #[must_use]
    pub const fn redacted_label(&self) -> &'static str {
        match self {
            Self::ExplicitPath(_) => "explicit-override-path",
            Self::EnvConfigPath(_) => "env:LLMOS_KEYPOOL_CONFIG",
            Self::EnvDirectBaseUrl => "env:LLMOS_LLM_BASE_URL",
            Self::UserHomeDefault => "user-home:~/glm-keypool/config.json",
        }
    }
}

/// Sanitized local `KeyPool` or direct `OpenAI`-compatible endpoint configuration.
#[derive(Clone, Debug)]
pub struct DiscoveredKeyPoolConfig {
    pub source: KeyPoolDiscoverySource,
    pub listen_host: String,
    pub listen_port: u16,
    pub upstream_base: String,
    pub openai_base_url: String,
    pub anthropic_base_url: String,
    pub pool_status_url: Option<String>,
    pub egress_key: SecretCredential,
    pub allowed_models: Vec<String>,
    pub default_model: String,
    pub fast_intent_model: String,
    pub configured_key_slots: usize,
}

/// Discovers local `glm-keypool` or direct `LLMOS_LLM_*` configuration from
/// `override_path` and process environment variables.
///
/// Priority order:
/// 1. `override_path`
/// 2. `LLMOS_KEYPOOL_CONFIG`
/// 3. `LLMOS_LLM_BASE_URL` (+ optional `LLMOS_LLM_API_KEY_FILE` / `LLMOS_LLM_API_KEY` / `LLMOS_LLM_MODEL`)
/// 4. `$USERPROFILE/glm-keypool/config.json` or `$HOME/glm-keypool/config.json`
///
/// # Errors
/// Fails closed if an explicit path is unreadable/invalid or if no configuration source exists.
pub fn discover_local_keypool(
    override_path: Option<&Path>,
) -> Result<DiscoveredKeyPoolConfig, LlmDriverError> {
    discover_local_keypool_from_lookup(override_path, |name| std::env::var(name).ok())
}

/// Deterministic environment-injected variant of [`discover_local_keypool`]
/// enabling 100% isolated unit/integration tests without mutating process env.
///
/// # Errors
/// Fails closed if a configured path cannot be read/parsed or no source is available.
pub fn discover_local_keypool_from_lookup<F>(
    override_path: Option<&Path>,
    env_lookup: F,
) -> Result<DiscoveredKeyPoolConfig, LlmDriverError>
where
    F: Fn(&str) -> Option<String>,
{
    let env_model = env_lookup("LLMOS_LLM_MODEL")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    // Priority 1: explicit override_path
    if let Some(path) = override_path {
        let raw = std::fs::read_to_string(path).map_err(LlmDriverError::ConfigIo)?;
        return parse_glm_keypool_config_json(
            &raw,
            env_model.as_deref(),
            KeyPoolDiscoverySource::ExplicitPath(path.to_path_buf()),
        );
    }

    // Priority 2: LLMOS_KEYPOOL_CONFIG
    if let Some(cfg_path_raw) = env_lookup("LLMOS_KEYPOOL_CONFIG") {
        let trimmed = cfg_path_raw.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            let raw = std::fs::read_to_string(&path).map_err(LlmDriverError::ConfigIo)?;
            return parse_glm_keypool_config_json(
                &raw,
                env_model.as_deref(),
                KeyPoolDiscoverySource::EnvConfigPath(path),
            );
        }
    }

    // Priority 3: LLMOS_LLM_BASE_URL
    if let Some(base_url_raw) = env_lookup("LLMOS_LLM_BASE_URL") {
        let base_trimmed = base_url_raw.trim().trim_end_matches('/');
        if !base_trimmed.is_empty() {
            return build_direct_env_config(base_trimmed, env_model.as_deref(), &env_lookup);
        }
    }

    // Priority 4: $USERPROFILE/glm-keypool/config.json or $HOME/glm-keypool/config.json
    for home_var in ["USERPROFILE", "HOME"] {
        if let Some(home_dir) = env_lookup(home_var) {
            let trimmed = home_dir.trim();
            if !trimmed.is_empty() {
                let candidate = PathBuf::from(trimmed)
                    .join("glm-keypool")
                    .join("config.json");
                if candidate.is_file() {
                    let raw =
                        std::fs::read_to_string(&candidate).map_err(LlmDriverError::ConfigIo)?;
                    return parse_glm_keypool_config_json(
                        &raw,
                        env_model.as_deref(),
                        KeyPoolDiscoverySource::UserHomeDefault,
                    );
                }
            }
        }
    }

    Err(LlmDriverError::DiscoveryNotFound(
        "no KeyPool configuration found via override_path, LLMOS_KEYPOOL_CONFIG, LLMOS_LLM_BASE_URL, or ~/glm-keypool/config.json",
    ))
}

fn build_direct_env_config<F>(
    base_url: &str,
    env_model: Option<&str>,
    env_lookup: &F,
) -> Result<DiscoveredKeyPoolConfig, LlmDriverError>
where
    F: Fn(&str) -> Option<String>,
{
    let parsed_url = HttpUrl::parse(base_url)?;
    let credential = if let Some(key_file) = env_lookup("LLMOS_LLM_API_KEY_FILE")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    {
        let content = std::fs::read_to_string(key_file).map_err(LlmDriverError::ConfigIo)?;
        SecretCredential::new(content.trim().to_owned())
    } else if let Some(raw_key) = env_lookup("LLMOS_LLM_API_KEY") {
        SecretCredential::new(raw_key.trim().to_owned())
    } else {
        SecretCredential::empty()
    };

    let default_model = env_model
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_PRIMARY_MODEL)
        .to_owned();
    let fast_intent_model = default_model.clone();
    let host_for_url = format_host_for_url(&parsed_url.host);
    let anthropic_base_url = format!("http://{host_for_url}:{}/api/anthropic", parsed_url.port);
    let configured_key_slots = usize::from(!credential.is_empty());

    Ok(DiscoveredKeyPoolConfig {
        source: KeyPoolDiscoverySource::EnvDirectBaseUrl,
        listen_host: parsed_url.host,
        listen_port: parsed_url.port,
        upstream_base: base_url.to_owned(),
        openai_base_url: base_url.to_owned(),
        anthropic_base_url,
        pool_status_url: None,
        egress_key: credential,
        allowed_models: vec![default_model.clone()],
        default_model,
        fast_intent_model,
        configured_key_slots,
    })
}

/// Parses a `glm-keypool` v2 `config.json` document into [`DiscoveredKeyPoolConfig`].
///
/// Enforces:
/// - `0.0.0.0` / `::` normalization to `127.0.0.1`;
/// - `bigmodel.cn` upstream automatic derivation to `/api/coding/paas/v4`
///   (GLM Coding Plan endpoint, avoiding `/api/paas/v4` error `1113`);
/// - top-level + `groups[*]` model allowlist aggregation and slot counting with
///   zero plaintext upstream key retention (`DRV-CRED-001`).
///
/// # Errors
/// Fails closed if `json_text` is not a valid JSON object or has an invalid port.
pub fn parse_glm_keypool_config_json(
    json_text: &str,
    env_model_override: Option<&str>,
    source: KeyPoolDiscoverySource,
) -> Result<DiscoveredKeyPoolConfig, LlmDriverError> {
    let root = JsonValue::parse(json_text)?;
    if root.as_object().is_none() {
        return Err(LlmDriverError::InvalidConfig(
            "glm-keypool config.json root must be a JSON object".to_owned(),
        ));
    }

    let (listen_host, listen_port) = parse_listen_host_and_port(&root)?;
    let egress_raw = root
        .get("egressKey")
        .and_then(JsonValue::as_str)
        .or_else(|| {
            root.get("auth")
                .and_then(|a| a.get("egressKey"))
                .and_then(JsonValue::as_str)
        })
        .unwrap_or("");
    let egress_key = SecretCredential::new(egress_raw.to_owned());

    let upstream_base = root
        .get("upstreamBase")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_UPSTREAM_BASE)
        .to_owned();

    let url_host = format_host_for_url(&listen_host);
    let openai_base_url = if upstream_base.to_ascii_lowercase().contains("bigmodel.cn") {
        format!("http://{url_host}:{listen_port}/api/coding/paas/v4")
    } else {
        format!("http://{url_host}:{listen_port}/v1")
    };
    let anthropic_base_url = format!("http://{url_host}:{listen_port}/api/anthropic");
    let pool_status_url = Some(format!("http://{url_host}:{listen_port}/__pool__/status"));

    let top_allow_models = extract_string_list(root.get("allowModels"));
    let mut allowed_models = Vec::new();
    for m in &top_allow_models {
        push_unique_string(&mut allowed_models, m);
    }

    let mut configured_key_slots = root
        .get("keys")
        .and_then(JsonValue::as_array)
        .map_or(0, Vec::len);

    if let Some(groups_val) = root.get("groups") {
        if let Some(groups_arr) = groups_val.as_array() {
            for group in groups_arr {
                collect_group_models_and_key_count(
                    group,
                    &mut allowed_models,
                    &mut configured_key_slots,
                );
            }
        } else if let Some(groups_map) = groups_val.as_object() {
            for group in groups_map.values() {
                collect_group_models_and_key_count(
                    group,
                    &mut allowed_models,
                    &mut configured_key_slots,
                );
            }
        }
    }

    let default_model = env_model_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| top_allow_models.first().cloned())
        .unwrap_or_else(|| DEFAULT_PRIMARY_MODEL.to_owned());

    let fast_intent_model = allowed_models
        .iter()
        .find(|m| m.to_ascii_lowercase().contains("flash"))
        .cloned()
        .unwrap_or_else(|| default_model.clone());

    Ok(DiscoveredKeyPoolConfig {
        source,
        listen_host,
        listen_port,
        upstream_base,
        openai_base_url,
        anthropic_base_url,
        pool_status_url,
        egress_key,
        allowed_models,
        default_model,
        fast_intent_model,
        configured_key_slots,
    })
}

fn parse_listen_host_and_port(root: &JsonValue) -> Result<(String, u16), LlmDriverError> {
    let listen = root.get("listen");
    let raw_host = listen
        .and_then(|l| l.get("host"))
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_KEYPOOL_HOST);
    let listen_host = normalize_client_host(raw_host);

    let listen_port = match listen.and_then(|l| l.get("port")) {
        Some(port_val) => {
            let raw_port = port_val.as_u64().ok_or_else(|| {
                LlmDriverError::InvalidConfig("listen.port must be a positive integer".to_owned())
            })?;
            let port = u16::try_from(raw_port).map_err(|_| {
                LlmDriverError::InvalidConfig("listen.port exceeds 65535".to_owned())
            })?;
            if port == 0 {
                return Err(LlmDriverError::InvalidConfig(
                    "listen.port cannot be 0".to_owned(),
                ));
            }
            port
        }
        None => DEFAULT_KEYPOOL_PORT,
    };
    Ok((listen_host, listen_port))
}

fn normalize_client_host(raw_host: &str) -> String {
    match raw_host.trim() {
        "0.0.0.0" | "::" | "[::]" | "*" => DEFAULT_KEYPOOL_HOST.to_owned(),
        other => other
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(other)
            .to_owned(),
    }
}

fn format_host_for_url(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

fn extract_string_list(val: Option<&JsonValue>) -> Vec<String> {
    let Some(arr) = val.and_then(JsonValue::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Some(s) = item.as_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                push_unique_string(&mut out, trimmed);
            }
        }
    }
    out
}

fn collect_group_models_and_key_count(
    group: &JsonValue,
    allowed_models: &mut Vec<String>,
    configured_key_slots: &mut usize,
) {
    for m in extract_string_list(group.get("allowModels")) {
        push_unique_string(allowed_models, &m);
    }
    if let Some(keys_arr) = group.get("keys").and_then(JsonValue::as_array) {
        *configured_key_slots = configured_key_slots.saturating_add(keys_arr.len());
    }
}

fn push_unique_string(target: &mut Vec<String>, candidate: &str) {
    if !target.iter().any(|existing| existing == candidate) {
        target.push(candidate.to_owned());
    }
}

/// Slot selection strategy for [`DirectKeyPoolScheduler`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeySelectionStrategy {
    /// Deterministic `SHA-256(session_id)` sticky slot affinity with healthy fallback.
    Sticky,
    /// Sequential round-robin rotation across healthy slots.
    RoundRobin,
}

impl KeySelectionStrategy {
    /// Canonical lowercase strategy identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sticky => "sticky",
            Self::RoundRobin => "round-robin",
        }
    }
}

/// One credential slot configured inside [`DirectKeyPoolConfig`].
#[derive(Clone, Debug)]
pub struct DirectKeySlotConfig {
    /// Internal slot identifier (only its 16-hex `SHA-256` fingerprint is ever exposed).
    pub slot_id: String,
    /// Logical provider group tag (e.g. `"default"`, `"xai"`).
    pub group: String,
    /// Redacted bearer API key credential.
    pub credential: SecretCredential,
}

impl DirectKeySlotConfig {
    /// Constructs a new direct key slot.
    #[must_use]
    pub fn new(
        slot_id: impl Into<String>,
        group: impl Into<String>,
        credential: SecretCredential,
    ) -> Self {
        Self {
            slot_id: slot_id.into(),
            group: group.into(),
            credential,
        }
    }
}

/// Configuration for the in-process multi-key scheduler [`DirectKeyPoolScheduler`].
#[derive(Clone, Debug)]
pub struct DirectKeyPoolConfig {
    pub openai_base_url: String,
    pub strategy: KeySelectionStrategy,
    pub rate_limit_cooldown_ms: u64,
    pub transient_cooldown_ms: u64,
    pub slots: Vec<DirectKeySlotConfig>,
}

impl DirectKeyPoolConfig {
    /// Builds a direct multi-key pool configuration with default cooldowns.
    #[must_use]
    pub fn new(
        openai_base_url: impl Into<String>,
        strategy: KeySelectionStrategy,
        slots: Vec<DirectKeySlotConfig>,
    ) -> Self {
        Self {
            openai_base_url: openai_base_url.into(),
            strategy,
            rate_limit_cooldown_ms: DEFAULT_RATE_LIMIT_COOLDOWN_MS,
            transient_cooldown_ms: DEFAULT_TRANSIENT_COOLDOWN_MS,
            slots,
        }
    }
}

/// Sanitized selection receipt for one slot pick in [`DirectKeyPoolScheduler`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedKeySlot {
    pub slot_index: usize,
    pub group: String,
    pub slot_fingerprint: String,
}

/// Action taken by [`DirectKeyPoolScheduler`] after an HTTP status failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailoverDisposition {
    /// Status is `429`, `401`, `403`, or `5xx`: slot cooled down / revoked, replay on next healthy slot.
    RetryNextSlot,
    /// Non-retriable client error (e.g. `400`, `404`, `422`): stop immediately without replaying.
    Abort,
}

#[derive(Clone, Debug, Default)]
struct SlotRuntimeState {
    cooldown_until_ms: Option<u64>,
    auth_revoked: bool,
    total_requests: u64,
    ok_requests: u64,
    failed_requests: u64,
}

impl SlotRuntimeState {
    fn is_available_at(&self, now_ms: u64) -> bool {
        if self.auth_revoked {
            return false;
        }
        match self.cooldown_until_ms {
            Some(until_ms) => now_ms >= until_ms,
            None => true,
        }
    }
}

/// Outcome of an HTTP request executed with automatic multi-key failover.
#[derive(Clone, Debug)]
pub struct DirectPoolHttpOutcome {
    pub response: HttpResponse,
    pub selected_slot: SelectedKeySlot,
    pub attempts: usize,
}

/// In-process multi-key scheduler supporting `sticky` (`SHA-256` session
/// affinity) and `round-robin` selection, with automatic cooldown on `429`/`5xx`,
/// revocation on `401`/`403`, and transparent failover replay.
#[derive(Debug)]
pub struct DirectKeyPoolScheduler {
    config: DirectKeyPoolConfig,
    slot_fingerprints: Vec<String>,
    rr_cursor: AtomicUsize,
    states: Mutex<Vec<SlotRuntimeState>>,
}

impl DirectKeyPoolScheduler {
    /// Constructs a validated [`DirectKeyPoolScheduler`].
    ///
    /// # Errors
    /// Fails closed if `slots` is empty, any slot credential is empty, or `openai_base_url` is invalid.
    pub fn new(config: DirectKeyPoolConfig) -> Result<Self, LlmDriverError> {
        let _ = HttpUrl::parse(&config.openai_base_url)?;
        if config.slots.is_empty() {
            return Err(LlmDriverError::InvalidConfig(
                "DirectKeyPoolScheduler requires at least one key slot".to_owned(),
            ));
        }
        if config.slots.iter().any(|s| s.credential.is_empty()) {
            return Err(LlmDriverError::InvalidConfig(
                "DirectKeyPoolScheduler slot credential must not be empty".to_owned(),
            ));
        }
        let slot_fingerprints = config
            .slots
            .iter()
            .map(|s| sha256_hex16(&s.slot_id))
            .collect();
        let states = vec![SlotRuntimeState::default(); config.slots.len()];
        Ok(Self {
            config,
            slot_fingerprints,
            rr_cursor: AtomicUsize::new(0),
            states: Mutex::new(states),
        })
    }

    /// Configured `OpenAI`-compatible base URL.
    #[must_use]
    pub fn openai_base_url(&self) -> &str {
        &self.config.openai_base_url
    }

    /// Configured selection strategy (`sticky` or `round-robin`).
    #[must_use]
    pub const fn strategy(&self) -> KeySelectionStrategy {
        self.config.strategy
    }

    /// Total number of configured key slots.
    #[must_use]
    pub fn total_slots(&self) -> usize {
        self.config.slots.len()
    }

    /// Number of currently healthy (not cooled-down and not auth-revoked) slots at `now_ms`.
    #[must_use]
    pub fn active_slots_at(&self, now_ms: u64) -> usize {
        let Ok(guard) = self.states.lock() else {
            return 0;
        };
        guard.iter().filter(|s| s.is_available_at(now_ms)).count()
    }

    /// Aggregates sanitized telemetry counters across all slots at `now_ms`.
    #[must_use]
    pub fn aggregate_counters_at(&self, now_ms: u64) -> (u64, u64, u64, u64, u64) {
        let total_keys = self.config.slots.len() as u64;
        let Ok(guard) = self.states.lock() else {
            return (total_keys, 0, 0, 0, 0);
        };
        let mut active_keys = 0_u64;
        let mut total_req = 0_u64;
        let mut ok_req = 0_u64;
        let mut failed_req = 0_u64;
        for st in guard.iter() {
            if st.is_available_at(now_ms) {
                active_keys += 1;
            }
            total_req = total_req.saturating_add(st.total_requests);
            ok_req = ok_req.saturating_add(st.ok_requests);
            failed_req = failed_req.saturating_add(st.failed_requests);
        }
        (total_keys, active_keys, total_req, ok_req, failed_req)
    }

    /// Selects the next healthy key slot for `session_id` at `now_ms`,
    /// skipping any slot indices listed in `excluded_indices`.
    ///
    /// # Errors
    /// Returns [`LlmDriverError::AllKeysUnavailable`] if all slots are cooled down,
    /// revoked, or already attempted.
    pub fn select_slot(
        &self,
        session_id: Option<&str>,
        now_ms: u64,
        excluded_indices: &[usize],
    ) -> Result<SelectedKeySlot, LlmDriverError> {
        let slot_count = self.config.slots.len();
        let start_idx = match (self.config.strategy, session_id.filter(|s| !s.is_empty())) {
            (KeySelectionStrategy::Sticky, Some(sid)) => sticky_slot_index(sid, slot_count),
            _ => self.rr_cursor.fetch_add(1, Ordering::Relaxed) % slot_count,
        };

        let guard = self.states.lock().map_err(|_| {
            LlmDriverError::HttpTransport("DirectKeyPoolScheduler state mutex poisoned".to_owned())
        })?;

        let mut unavailable = 0_usize;
        for offset in 0..slot_count {
            let idx = (start_idx + offset) % slot_count;
            if !guard[idx].is_available_at(now_ms) {
                unavailable += 1;
                continue;
            }
            if excluded_indices.contains(&idx) {
                continue;
            }
            return Ok(SelectedKeySlot {
                slot_index: idx,
                group: self.config.slots[idx].group.clone(),
                slot_fingerprint: self.slot_fingerprints[idx].clone(),
            });
        }

        Err(LlmDriverError::AllKeysUnavailable {
            total_keys: slot_count,
            unavailable_keys: unavailable.max(excluded_indices.len()).min(slot_count),
        })
    }

    /// Records a `2xx` completion on `slot_index` and clears any transient cooldown.
    pub fn record_success(&self, slot_index: usize) {
        if let Ok(mut guard) = self.states.lock()
            && let Some(st) = guard.get_mut(slot_index)
        {
            st.total_requests = st.total_requests.saturating_add(1);
            st.ok_requests = st.ok_requests.saturating_add(1);
            st.cooldown_until_ms = None;
        }
    }

    /// Records a non-`2xx` HTTP status on `slot_index`, applying cooldown on
    /// `429`/`5xx` or revocation on `401`/`403`, and returns whether the caller
    /// should failover and replay on the next slot.
    pub fn record_failure(
        &self,
        slot_index: usize,
        status_code: u16,
        retry_after_secs: Option<u64>,
        now_ms: u64,
    ) -> FailoverDisposition {
        let disposition = match status_code {
            429 | 401 | 403 | 500..=599 => FailoverDisposition::RetryNextSlot,
            _ => FailoverDisposition::Abort,
        };
        if let Ok(mut guard) = self.states.lock()
            && let Some(st) = guard.get_mut(slot_index)
        {
            st.total_requests = st.total_requests.saturating_add(1);
            st.failed_requests = st.failed_requests.saturating_add(1);
            match status_code {
                429 => {
                    let cooldown_ms = retry_after_secs
                        .map_or(self.config.rate_limit_cooldown_ms, |s| {
                            s.saturating_mul(1_000).clamp(1_000, 300_000)
                        });
                    st.cooldown_until_ms = Some(now_ms.saturating_add(cooldown_ms));
                }
                401 | 403 => {
                    st.auth_revoked = true;
                }
                500..=599 => {
                    st.cooldown_until_ms =
                        Some(now_ms.saturating_add(self.config.transient_cooldown_ms));
                }
                _ => {}
            }
        }
        disposition
    }

    /// Executes a `POST` JSON request against `endpoint_url` with automatic
    /// multi-key failover across healthy slots on `429`, `401`, `403`, or `5xx`.
    ///
    /// # Errors
    /// Fails closed if all healthy slots fail or if a non-retriable 4xx occurs.
    pub async fn post_json_with_failover(
        &self,
        http_client: &HttpClient,
        endpoint_url: &str,
        session_id: Option<&str>,
        json_body: &[u8],
        now_ms: u64,
    ) -> Result<DirectPoolHttpOutcome, LlmDriverError> {
        let mut tried_indices = Vec::with_capacity(self.config.slots.len());
        let mut last_error: Option<LlmDriverError> = None;

        while tried_indices.len() < self.config.slots.len() {
            let selected = match self.select_slot(session_id, now_ms, &tried_indices) {
                Ok(slot) => slot,
                Err(unavail_err) => {
                    return Err(last_error.unwrap_or(unavail_err));
                }
            };
            tried_indices.push(selected.slot_index);

            let auth_header = format!(
                "Bearer {}",
                self.config.slots[selected.slot_index]
                    .credential
                    .expose_secret()
            );
            let mut headers: Vec<(&str, &str)> = vec![
                ("Accept", "application/json"),
                ("Authorization", auth_header.as_str()),
            ];
            if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
                headers.push(("x-pool-session", sid));
            }

            match http_client
                .post_json(endpoint_url, &headers, json_body)
                .await
            {
                Ok(resp) if (200..300).contains(&resp.status_code) => {
                    self.record_success(selected.slot_index);
                    return Ok(DirectPoolHttpOutcome {
                        response: resp,
                        selected_slot: selected,
                        attempts: tried_indices.len(),
                    });
                }
                Ok(resp) => {
                    let retry_after = resp.retry_after_secs();
                    let disposition = self.record_failure(
                        selected.slot_index,
                        resp.status_code,
                        retry_after,
                        now_ms,
                    );
                    let err = LlmDriverError::UpstreamHttpStatus {
                        status_code: resp.status_code,
                        retry_after_secs: retry_after,
                        summary: summarize_upstream_error_body(&resp.body),
                    };
                    if disposition == FailoverDisposition::Abort {
                        return Err(err);
                    }
                    last_error = Some(err);
                }
                Err(transport_err) => {
                    let _ = self.record_failure(selected.slot_index, 503, None, now_ms);
                    last_error = Some(transport_err);
                }
            }
        }

        Err(last_error.unwrap_or(LlmDriverError::AllKeysUnavailable {
            total_keys: self.config.slots.len(),
            unavailable_keys: self.config.slots.len(),
        }))
    }
}

fn sticky_slot_index(session_id: &str, slot_count: usize) -> usize {
    let digest = Sha256::digest(session_id.as_bytes());
    let mut prefix = [0_u8; 8];
    prefix.copy_from_slice(&digest[..8]);
    let hash_u64 = u64::from_be_bytes(prefix);
    usize::try_from(hash_u64 % (slot_count as u64)).unwrap_or(0)
}

/// Extracts a short, sanitized error summary from an upstream HTTP error body
/// without ever echoing credentials or huge payloads.
#[must_use]
pub(crate) fn summarize_upstream_error_body(body: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(body) else {
        return "non-UTF-8 upstream error payload".to_owned();
    };
    if let Ok(json) = JsonValue::parse(text) {
        if let Some(err_obj) = json.get("error") {
            let code = err_obj
                .get("code")
                .and_then(|c| {
                    c.as_str()
                        .map(str::to_owned)
                        .or_else(|| c.as_u64().map(|n| n.to_string()))
                })
                .unwrap_or_default();
            let msg = err_obj
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or("upstream error");
            let combined = if code.is_empty() {
                msg.to_owned()
            } else {
                format!("code={code} {msg}")
            };
            return truncate_clean(&combined, 160);
        }
        if let Some(msg) = json.get("message").and_then(JsonValue::as_str) {
            return truncate_clean(msg, 160);
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "empty error body".to_owned()
    } else {
        truncate_clean(trimmed, 160)
    }
}

fn truncate_clean(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        if ch.is_control() {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}
