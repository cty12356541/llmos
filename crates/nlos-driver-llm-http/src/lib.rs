//! Bounded local HTTP/1.1 LLM provider driver plane scaffold.
//!
//! Concrete HTTP transport, resource budget charging, and durable `Operation`
//! lifecycle wiring land in the follow-up implementation lane.

/// Canonical Cargo package identifier for the HTTP LLM provider driver crate.
pub const CRATE_NAME: &str = "nlos-driver-llm-http";

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn scaffold_crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }
}
