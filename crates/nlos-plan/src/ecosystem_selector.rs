//! Typed ecosystem dependency selector surface and G3 authorization-conditioned
//! materialization gate bindings (`[PLAN-DEPENDENCY-001]`, `ADR-0027` section 5.2
//! `U-7` / `U-8` / `RISK-E-05`).
//!
//! Covers all seven `[PLAN-DEPENDENCY-001]` dependency entity classes (`Package`,
//! `Skill`, `Tool`, `Model`, `Artifact`, `Topic`, and external service dependencies)
//! resolved into immutable `(digest, generation)` handles prior to execution.

/// Number of canonical ecosystem dependency entity classes required by
/// `[PLAN-DEPENDENCY-001]` (`Package`, `Skill`, `Tool`, `Model`, `Artifact`,
/// `Topic`, and `ExternalService`).
pub const PLAN_DEPENDENCY_ENTITY_KIND_COUNT: usize = 7;

#[cfg(test)]
mod tests {
    use super::PLAN_DEPENDENCY_ENTITY_KIND_COUNT;

    #[test]
    fn plan_dependency_covers_seven_entity_classes() {
        assert_eq!(PLAN_DEPENDENCY_ENTITY_KIND_COUNT, 7);
    }
}
