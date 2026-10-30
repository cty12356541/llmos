//! Out-of-core SABI compatibility shim (`N-1 / N-2 <-> N` epoch protocol adapter)
//! and `[DRV-SHIM-001]` dual-boundary driver compatibility enforcement (`ADR-0027`, `W78-E2`).
//!
//! Core NLOS authorities (`nlos-cell`, `nlos-task`, `nlos-capability`,
//! `nlos-resource`, `nlos-consensus`, `nlos-system-control`) execute solely at the
//! canonical host epoch `N` with zero historical package branches. Packages
//! declaring `N-1` or `N-2` compatibility mount [`sabi_adapter::SabiEpochAdapterCell`]
//! and, when applicable, [`driver_shim::DriverProtocolCompatShim`] paired with the
//! controlled TCB edge [`driver_shim::TrustedDriverEnforcementGate`].

#![forbid(unsafe_code)]

mod driver_cbor;
pub mod driver_shim;
mod enforcement;
pub mod manifest;
pub mod mount;
pub mod sabi_adapter;
mod wire;

/// Canonical crate identifier retained for workspace discovery and diagnostics.
pub const CRATE_NAME: &str = "nlos-compat-shim";

pub use driver_shim::{
    CustodiedCredentialRef, DriverAdmissionRequest, DriverCompletionSubmission, DriverMethod,
    DriverProtocolCompatShim, DriverShimError, EnforcementGateError, IsolationEnforcementPolicy,
    LegacyDriverWireFormat, ReconcileUncertainRequest, ReservationActivationToken,
    SanitizedDriverDispatchTicket, TrustedDriverEnforcementGate, VerifiedDriverCompletion,
    compute_driver_authorization_digest,
};
pub use manifest::{
    MAX_COMPAT_MANIFEST_BYTES, MAX_FEATURE_NAME_BYTES, MAX_REQUIRED_FEATURES,
    ManifestCompatibilityError, PackageCompatibilitySpec, parse_package_compatibility_manifest,
};
pub use mount::{
    CompatMountRejection, CompatShimMountDecision, CompatShimMountPolicy, MountedCompatShim,
    evaluate_compat_shim_mount, evaluate_compat_shim_mount_with_manifest,
};
pub use sabi_adapter::{
    CompatTranslationDirection, CompatTranslationTrace, DeprecationWarningReceipt,
    MAX_DEPRECATION_WARNINGS, SabiAdapterError, SabiChannelId, SabiEpochAdapterCell, SabiFrameKind,
    TranslatedSabiFrame,
};
