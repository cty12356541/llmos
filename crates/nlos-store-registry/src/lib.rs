//! Store governance catalog and `OCI` content-addressed metadata registry
//! (`ADR-0027` section 2.3; `[PKG-OCI-001]`, `[PKG-MULTI-001]`).

#![forbid(unsafe_code)]

/// Major schema generation of the `nlos-store-registry` governance catalog.
pub const STORE_REGISTRY_SCHEMA_MAJOR: u16 = 1;

#[cfg(test)]
mod tests {
    use super::STORE_REGISTRY_SCHEMA_MAJOR;

    #[test]
    fn store_registry_schema_major_is_v1() {
        assert_eq!(STORE_REGISTRY_SCHEMA_MAJOR, 1);
    }
}
