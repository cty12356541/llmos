//! Publisher trust authority contracts, three-tier PKI certificate hierarchy,
//! monotonic `RevocationEpoch` CRLs, anti-freeze / `[DUI-OFFLINE-001]` verification,
//! and `[PKG-APPID-001]` / `[PKG-SEP-001]` application ownership ledger for
//! Stage E store governance (`ADR-0027` section 2.3; `[PKG-IMM-001]`).

#![forbid(unsafe_code)]

pub mod authority;
pub mod ownership;
pub mod pki;
pub mod revocation;
pub mod schema;
pub mod types;

pub use authority::SqlitePublisherTrustAuthority;
pub use ownership::{
    OwnershipTransferProposal, PackageReleaseBinding, PackageReleaseDecision,
    SeparatedPackageApplicationInstallation, SignedOwnershipTransferReceipt,
    verify_identity_separation,
};
pub use pki::{
    IntermediateCertificateIssueSpec, KeyRotationProof, LeafCertificateIssueSpec,
    LeafCertificateStatus, OfflinePlatformRootCa, PublisherKeyRotationReceipt,
    PublisherLeafCertificate, RotatedKeyDisposition, StoreIntermediateCertificate,
    ed25519_public_key_from_seed, sign_ed25519_digest_with_seed, sign_package_digest_under_custody,
};
pub use revocation::{
    CrlImportDecision, CrlIssuerKind, RevocationListSpec, RevocationReason,
    RevocationStateSnapshot, RevokedCertificateEntry, SignedRevocationList, TrustFreshnessMode,
};
pub use schema::{PUBLISHER_TRUST_DB_FILENAME, PUBLISHER_TRUST_SCHEMA_VERSION};
pub use types::*;
