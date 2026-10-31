//! Declarative SABI contract static checker, package manifest auditor, and
//! `PKG-UPDATE-001` three-mode container migration rollback runner (Stage E E3 / `ADR-0027`).
#![forbid(unsafe_code)]

/// Canonical Cargo package identifier for the `nlos-migrate` crate.
pub const CRATE_NAME: &str = "nlos-migrate";

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_cargo_pkg_name() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }
}
