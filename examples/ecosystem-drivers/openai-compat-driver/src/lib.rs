//! `openai-compat-driver` — W80-E6B third-party userland OpenAI-compatible
//! inference gateway reference adapter (`examples/ecosystem-drivers`,
//! Stage E `E6`, `DRV-PROTO-001`).
//!
//! - [`chat`]: `/v1/chat/completions` request/response codec;
//! - [`sse`]: Server-Sent-Events stream parsing for `chat.completion.chunk`
//!   deltas and `data: [DONE]` termination;
//! - [`metering`]: five-dimension token accounting over the real
//!   `nlos-resource` `ResourceAuthority` with request-digest-derived
//!   idempotency keys (same-key replay, never double charge).

#![forbid(unsafe_code)]

pub mod chat;
pub mod metering;
pub mod sse;

use ecosystem_driver_kit::DriverExtensionDescriptor;

pub use chat::{
    ChatChoice, ChatCodecError, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ChatRole, ChatUsage,
};
pub use metering::{MeteredChatGateway, MeteringError};
pub use sse::{
    DONE_SENTINEL, MAX_SSE_EVENT_BYTES, SseError, SseEvent, SseParser, StreamAggregator,
};

/// Canonical Cargo package identifier for this sample crate.
pub const CRATE_NAME: &str = "openai-compat-driver";

/// Builds the versioned-extension descriptor for this adapter build
/// (`DRV-PROTO-001`: the OpenAI-compatible HTTP bridge wrapped as a
/// versioned driver extension over the frozen provider codec).
#[must_use]
pub fn extension_descriptor() -> DriverExtensionDescriptor {
    DriverExtensionDescriptor::new(
        "llmos.ext.driver.openai-compat",
        (1, 0),
        "openai-chat-completions",
        &["v1"],
        "nlos.sabi.DriverProvider/v1.0",
    )
}

/// Derives the deterministic request digest a completion is keyed on.
#[must_use]
pub fn request_digest(model: &str, messages_len: usize, stream: bool) -> [u8; 32] {
    let mut input = String::with_capacity(model.len() + 32);
    input.push_str(model);
    input.push('\0');
    input.push_str(&messages_len.to_string());
    input.push('\0');
    input.push_str(if stream { "stream" } else { "sync" });
    ecosystem_driver_kit::domain_digest(b"llmos/openai-compat-driver/request/v1", input.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, extension_descriptor, request_digest};
    use ecosystem_driver_kit::negotiate_protocol_version;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }

    #[test]
    fn extension_negotiates_v1_wire_protocol() {
        let descriptor = extension_descriptor();
        assert_eq!(descriptor.extension_id, "llmos.ext.driver.openai-compat");
        assert_eq!(descriptor.codec_freeze_tag, "nlos.sabi.DriverProvider/v1.0");
        let negotiated =
            negotiate_protocol_version(&descriptor, &["v0", "v1"]).expect("v1 supported");
        assert_eq!(negotiated.chosen_protocol_version, "v1");
        assert!(negotiate_protocol_version(&descriptor, &["v0"]).is_err());
    }

    #[test]
    fn request_digests_are_deterministic_and_shape_sensitive() {
        let a = request_digest("glm-4.6", 3, false);
        let b = request_digest("glm-4.6", 3, false);
        let c = request_digest("glm-4.6", 3, true);
        let d = request_digest("glm-4.6", 4, false);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }
}
