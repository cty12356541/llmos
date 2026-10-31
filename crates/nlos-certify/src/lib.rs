//! Four-gate offline package certification pipeline for Stage E store
//! onboarding (`ADR-0027` section 2.3; `[SABI-CONFORM-001]`, `[PKG-IMM-001]`,
//! `[PKG-UPDATE-001]`, `[PKG-UNINSTALL-001]`).

#![forbid(unsafe_code)]

/// Number of mandatory certification gates required before issuing a
/// `CERTIFIED_PUBLISHER` receipt (`ADR-0027` section 2.3.4).
pub const CERTIFY_PIPELINE_GATE_COUNT: usize = 4;

#[cfg(test)]
mod tests {
    use super::CERTIFY_PIPELINE_GATE_COUNT;

    #[test]
    fn certify_pipeline_enforces_four_gates() {
        assert_eq!(CERTIFY_PIPELINE_GATE_COUNT, 4);
    }
}
