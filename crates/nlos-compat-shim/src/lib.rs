//! Out-of-core unprivileged SABI epoch adapter and `DRV-SHIM-001` driver protocol
//! translation shim (Stage E `M-E2` / Work Package `E2` / `ADR-0027`).
//!
//! Core components:
//! - [`mount`]: Per-package dynamic epoch compatibility mount planner
//!   (`PackageCompatibilitySpec`, `DriverWireEpoch`, `CompatMountPlanner`,
//!   `CompatMountDecision::NativePassthrough` vs `MountOutOfCoreShim`).
//! - [`sabi_adapter`]: Deterministic bidirectional SABI `Envelope` epoch translator
//!   (`SabiEpochAdapterCell`, `EpochTranslationRules`, `DuplexFrameChannel`,
//!   `InMemoryFrameDuplex`, `ByteStreamFrameTransport`, `SabiShimBridge`).
//! - [`proto_unknown`]: Safe bounded Protobuf wire-tag extractor and injector
//!   preserving unknown non-critical wire tags losslessly across epoch translation
//!   (`[COMPAT-VER-002]`).
//! - [`driver_shim`]: Deterministic bidirectional Stage B `minicbor` / historical
//!   `DriverProvider` <-> frozen `ADR-0024` 8th channel `nlos.sabi.DriverProvider`
//!   Protobuf bridge enforcing `[TASK-PROVIDER-001]` isolation honesty.
//! - [`boundary`]: Four-layer `ADR-0027` section 2.2.2 enforcement gateway
//!   (`TrustedEnforcementGate`, `SecretCredential`, `OutOfCoreCompatShimRuntime`,
//!   `WireCaptureTranscript`) proving zero privilege bypass (`[DRV-SHIM-001]`),
//!   zero raw credential leakage (`[DRV-CRED-001]`), `verify-then-commit`
//!   incarnation fencing (`[DRV-BOUND-001]`, `[DRV-ACTIVE-001]`), and crash
//!   uncertainty quarantine (`[DRV-LIFE-001]`, `[KABI-UNCERTAIN-001]`).
#![forbid(unsafe_code)]

pub mod boundary;
pub mod driver_shim;
pub mod mount;
pub mod proto_unknown;
pub mod sabi_adapter;

/// Canonical Cargo package identifier for the `nlos-compat-shim` crate.
pub const CRATE_NAME: &str = "nlos-compat-shim";

/// Fixed upper bound on buffered [`nlos_service_directory::DeprecationWarningReceipt`]
/// entries inside [`sabi_adapter::SabiEpochAdapterCell`] and [`mount::CompatMountPlanner`].
pub const MAX_SHIM_DEPRECATION_WARNINGS: usize = 256;

pub use boundary::{
    AuthorizedDriverCompletionReceipt, AuthorizedDriverDispatchReceipt, BoundaryViolation,
    CrashQuarantineReceipt, MediatedDriverDispatchAuth, OutOfCoreCompatShimRuntime,
    OutOfCoreShimError, ReconciledUncertaintyReceipt, SecretCredential, SignedMeteredUsageProof,
    TrustedEnforcementGate, WireCaptureTranscript, build_driver_request_envelope,
    compute_dispatch_semantic_event_id, compute_metered_usage_proof_digest,
};
pub use driver_shim::{
    CancelOperationResultWire, CancelOperationWire, DriverProtocolShim, DriverShimError,
    DriverShimMode, LEGACY_MINICBOR_UNVERIFIED_PROFILE, LegacyCompleteBridgeContext,
    LegacyMinicborRequest, LegacyMinicborResponse, ProviderTrustCeiling, decode_cancel_request,
    decode_cancel_result, encode_cancel_request, encode_cancel_result,
    legacy_unverified_isolation_attestation,
};
pub use mount::{
    CompatMountDecision, CompatMountError, CompatMountPlanner, DriverWireEpoch, MountedCompatStack,
    PackageCompatibilitySpec, PackageServiceDemand,
};
pub use proto_unknown::{
    ExtractedEnvelopeUnknowns, RawUnknownField, WireScanError, encode_envelope_with_unknown_fields,
    extract_envelope_unknown_fields,
};
pub use sabi_adapter::{
    BridgePumpOutcome, ByteStreamFrameTransport, DuplexFrameChannel, EpochTranslationRules,
    InMemoryFrameDuplex, SabiAdapterError, SabiEpochAdapterCell, SabiShimBridge,
    TranslatedWireFrame, TranslationRuleError, TransportBridgeError,
};

#[cfg(test)]
mod tests {
    use super::{CRATE_NAME, MAX_SHIM_DEPRECATION_WARNINGS};

    #[test]
    fn crate_name_matches_cargo_pkg_name() {
        assert_eq!(CRATE_NAME, env!("CARGO_PKG_NAME"));
        assert_eq!(MAX_SHIM_DEPRECATION_WARNINGS, 256);
    }
}
