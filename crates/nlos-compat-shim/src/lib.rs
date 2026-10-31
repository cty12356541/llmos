//! Out-of-core unprivileged SABI epoch adapter and `DRV-SHIM-001` driver protocol
//! translation shim (Stage E E2 / `ADR-0027`).
#![forbid(unsafe_code)]

/// Canonical Cargo package identifier for the `nlos-compat-shim` crate.
pub const CRATE_NAME: &str = "nlos-compat-shim";

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_cargo_pkg_name() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }
}
