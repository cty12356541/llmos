//! Single-node NLOS developer bootstrap orchestrator scaffold.
//!
//! Concrete workspace bootstrap, resident daemon supervision, and smoke checks
//! land in the follow-up implementation lane.

/// Canonical Cargo package identifier for the `nlos-up` bootstrap crate.
pub const CRATE_NAME: &str = "nlos-up";

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn scaffold_crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
    }
}
