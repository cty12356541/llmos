//! `ecosystem-driver-kit` — shared primitives for the W80-E6B third-party
//! userland driver reference samples (`examples/ecosystem-drivers`).
//!
//! Zero-dependency (plus `sha2`) building blocks mirroring the public Stage E
//! driver contracts:
//! - [`json`]: bounded RFC 8259 codec for JSON-RPC / OpenAI-compatible wire
//!   payloads (follows the in-repo `nlos-driver-llm-http` precedent).
//! - [`effects`]: `DRV-LIFE-001` four-class `EffectDescriptor` declarations,
//!   line-based declaration parsing, and the crash-fenced pending-call ledger
//!   with same-`IdempotencyKey` `UNCERTAIN` reconciliation
//!   (`KABI-UNCERTAIN-001`).
//! - [`extension`]: `DRV-PROTO-001` versioned-extension envelopes and
//!   fail-closed protocol-version negotiation (`DRV-NEGO-001`).

#![forbid(unsafe_code)]

pub mod effects;
pub mod extension;
pub mod json;

pub use effects::{
    CallAdmission, EffectClass, EffectDescriptor, EffectsError, FencedCall, PendingCallFence,
    PendingCallState, ProviderProbe, SettledOutcome, hex16, parse_effect_declarations,
};
pub use extension::{
    DriverExtensionDescriptor, ExtensionNegoError, NegotiatedExtension, negotiate_protocol_version,
    parse_extension_manifest,
};
pub use json::{JsonError, JsonNumber, JsonValue, MAX_JSON_DEPTH, write_escaped_json_str};

/// Domain-separated SHA-256 helper for deterministic sample-side derivations
/// (idempotency keys, operation ids, request digests).
#[must_use]
pub fn domain_digest(domain: &[u8], input: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0]);
    hasher.update(input);
    hasher.finalize().into()
}

/// Derives a 128-bit key from a domain-separated digest (first half).
#[must_use]
pub fn domain_key16(domain: &[u8], input: &[u8]) -> [u8; 16] {
    let digest = domain_digest(domain, input);
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::{domain_digest, domain_key16};

    #[test]
    fn domain_separated_derivations_are_deterministic_and_distinct() {
        let a = domain_digest(b"llmos/driver-kit/test/v1", b"payload");
        let b = domain_digest(b"llmos/driver-kit/test/v1", b"payload");
        let c = domain_digest(b"llmos/driver-kit/other/v1", b"payload");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            domain_key16(b"llmos/driver-kit/test/v1", b"payload").len(),
            16
        );
        assert_eq!(
            domain_key16(b"llmos/driver-kit/test/v1", b"payload"),
            domain_key16(b"llmos/driver-kit/test/v1", b"payload")
        );
    }
}
