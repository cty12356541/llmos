//! Declarative SABI contract static checker, package manifest auditor, and
//! `[PKG-UPDATE-001]` three-mode data migration rollback drill toolkit
//! (`W78-E3-MIGRATE-KIT` / `E3`).

#![forbid(unsafe_code)]

/// Canonical Cargo package identifier for the migration toolkit crate.
pub const CRATE_NAME: &str = "nlos-migrate";

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn scaffold_crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }
}
