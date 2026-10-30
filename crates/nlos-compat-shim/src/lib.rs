//! Out-of-core unprivileged SABI epoch adapter and `DriverProvider` protocol
//! compatibility shim (`W78-E2-COMPAT-SHIM` / `E2`).

#![forbid(unsafe_code)]

/// Canonical Cargo package identifier for the out-of-core compatibility shim crate.
pub const CRATE_NAME: &str = "nlos-compat-shim";

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn scaffold_crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }
}
