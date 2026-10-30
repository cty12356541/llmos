//! Fail-closed typed error surface for `nlos-driver-llm-http`.
//!
//! All variants strictly omit raw credentials, authorization headers, and
//! host-private filesystem user paths (`DRV-CRED-001`).

use std::error::Error;
use std::fmt;

use nlos_resource::{LlmBudgetDimension, ResourceAuthorityError};
use nlos_types::ResourceAccountId;

use crate::json::JsonError;

/// Fail-closed error returned by the HTTP/1.1 LLM driver, `KeyPool` discovery,
/// `LlmEnforcementShim`, and `NL` intent compiler.
#[derive(Debug)]
pub enum LlmDriverError {
    /// RFC 8259 JSON parsing or schema decoding failure.
    Json(JsonError),
    /// Malformed or unsupported HTTP/1.1 endpoint URL.
    InvalidUrl(String),
    /// TCP socket or HTTP/1.1 wire framing error.
    HttpTransport(String),
    /// Outbound HTTP request exceeded the configured timeout budget.
    HttpTimeout { timeout_ms: u64 },
    /// Upstream gateway or provider returned a non-2xx HTTP status code.
    UpstreamHttpStatus {
        status_code: u16,
        retry_after_secs: Option<u64>,
        summary: String,
    },
    /// No local `KeyPool` configuration or direct environment endpoint found.
    DiscoveryNotFound(&'static str),
    /// Filesystem I/O failure while reading an explicit config or key file.
    ConfigIo(std::io::Error),
    /// Configuration payload failed structural or security validation.
    InvalidConfig(String),
    /// All candidate slots in `DirectKeyPoolScheduler` are cooled down or revoked.
    AllKeysUnavailable {
        total_keys: usize,
        unavailable_keys: usize,
    },
    /// `ResourceAuthority` 5D LLM token budget is exhausted (fail-closed).
    BudgetExhausted {
        account_id: ResourceAccountId,
        exhausted_dimension: Option<LlmBudgetDimension>,
        remaining_api_calls: u64,
        remaining_total_tokens: u64,
    },
    /// Underlying durable `ResourceAuthority` storage or accounting failure.
    ResourceAuthority(ResourceAuthorityError),
    /// Request input or upstream provider JSON payload violated contract.
    InvalidProviderPayload(&'static str),
}

impl fmt::Display for LlmDriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => write!(f, "JSON codec error: {err}"),
            Self::InvalidUrl(msg) => write!(f, "invalid HTTP/1.1 URL: {msg}"),
            Self::HttpTransport(msg) => write!(f, "HTTP/1.1 transport error: {msg}"),
            Self::HttpTimeout { timeout_ms } => {
                write!(f, "HTTP/1.1 request timed out after {timeout_ms}ms")
            }
            Self::UpstreamHttpStatus {
                status_code,
                retry_after_secs,
                summary,
            } => {
                if let Some(secs) = retry_after_secs {
                    write!(
                        f,
                        "upstream HTTP {status_code} (retry_after={secs}s): {summary}"
                    )
                } else {
                    write!(f, "upstream HTTP {status_code}: {summary}")
                }
            }
            Self::DiscoveryNotFound(msg) => write!(f, "KeyPool discovery failed: {msg}"),
            Self::ConfigIo(err) => write!(f, "configuration I/O failure: {err}"),
            Self::InvalidConfig(msg) => write!(f, "invalid KeyPool configuration: {msg}"),
            Self::AllKeysUnavailable {
                total_keys,
                unavailable_keys,
            } => write!(
                f,
                "all direct KeyPool slots unavailable ({unavailable_keys}/{total_keys} cooled down or revoked)"
            ),
            Self::BudgetExhausted {
                account_id,
                exhausted_dimension,
                remaining_api_calls,
                remaining_total_tokens,
            } => write!(
                f,
                "LLM token budget exhausted for {account_id:?} (dimension={exhausted_dimension:?}, remaining_calls={remaining_api_calls}, remaining_tokens={remaining_total_tokens})"
            ),
            Self::ResourceAuthority(err) => write!(f, "ResourceAuthority error: {err}"),
            Self::InvalidProviderPayload(msg) => write!(f, "invalid LLM payload: {msg}"),
        }
    }
}

impl Error for LlmDriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            Self::ConfigIo(err) => Some(err),
            Self::ResourceAuthority(err) => Some(err),
            _ => None,
        }
    }
}

impl From<JsonError> for LlmDriverError {
    fn from(err: JsonError) -> Self {
        Self::Json(err)
    }
}

impl From<ResourceAuthorityError> for LlmDriverError {
    fn from(err: ResourceAuthorityError) -> Self {
        Self::ResourceAuthority(err)
    }
}
