//! Multi-component composite application catalog, release channel tracks, and
//! `Ed25519`-signed four-gate certification attestation (`ADR-0027` section 2.3;
//! `[PKG-MULTI-001]`, `[PKG-APPID-001]`).

use std::collections::HashSet;
use std::fmt;

use ed25519_dalek::{Signature, VerifyingKey};
use nlos_migrate::ComponentRole;
use nlos_publisher_trust::{PublisherId, PublisherTrustTier};
use nlos_types::{ApplicationId, IdempotencyKey, PackageId, ReceiptId};
use sha2::{Digest, Sha256};

use crate::error::StoreRegistryError;
use crate::recall::RecallSeverity;
use crate::transparency::{DeclaredPermissionEntry, encode_effect_class};

/// Domain separator for publisher-signed [`StoreReleaseSubmission`] digests.
pub const RELEASE_SUBMISSION_SIGNING_DOMAIN: &[u8] = b"nlos/store-registry/release-submission/v1";

/// Domain separator for certifier-signed [`SignedCertificationAttestation`] digests.
pub const CERTIFICATION_ATTESTATION_SIGNING_DOMAIN: &[u8] =
    b"nlos/store-registry/certification-attestation/v1";

/// Bitmask requiring all four `nlos-certify` gates (`0b1111`) before promoting
/// `Candidate -> Stable` (`ADR-0027` section 2.3.4).
pub const REQUIRED_CERTIFICATION_GATES_MASK: u8 = 0b1111;

/// Store release distribution channel (`ADR-0027` section 2.3.3).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StoreChannel {
    /// Newly submitted candidate release awaiting certification or confined to
    /// the community sandbox track.
    Candidate = 1,
    /// Official certified release channel (`PlatformCore` or `CertifiedPublisher`
    /// with a valid four-gate [`SignedCertificationAttestation`]).
    Stable = 2,
    /// Emergency recalled channel blocked from new installs and launches.
    Recalled = 3,
}

impl StoreChannel {
    /// Returns the closed 1-byte wire and `SQLite` storage code (`1..=3`).
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }

    /// Decodes a `SQLite` integer column into a closed [`StoreChannel`].
    #[must_use]
    pub const fn decode(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::Candidate),
            2 => Some(Self::Stable),
            3 => Some(Self::Recalled),
            _ => None,
        }
    }

    /// Returns the canonical uppercase identifier token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "CANDIDATE",
            Self::Stable => "STABLE",
            Self::Recalled => "RECALLED",
        }
    }
}

impl fmt::Display for StoreChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Store catalog isolation track separating official/certified publishers from
/// `COMMUNITY_SANDBOXED` unendorsed community submissions (`ADR-0027` section 2.3.1).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StoreCatalogTrack {
    /// Official catalog track eligible for `Candidate -> Stable` certification
    /// (`PlatformCore` and `CertifiedPublisher`).
    OfficialTrack = 1,
    /// Restricted dual-layer sandbox candidate track for `CommunitySandboxed`
    /// publishers; physically barred from official `Stable` promotion.
    CommunityTrack = 2,
}

impl StoreCatalogTrack {
    /// Maps a [`PublisherTrustTier`] to its mandatory [`StoreCatalogTrack`].
    #[must_use]
    pub const fn for_trust_tier(tier: PublisherTrustTier) -> Self {
        match tier {
            PublisherTrustTier::PlatformCore | PublisherTrustTier::CertifiedPublisher => {
                Self::OfficialTrack
            }
            PublisherTrustTier::CommunitySandboxed => Self::CommunityTrack,
        }
    }

    /// Encodes the track into its `SQLite` integer discriminant (`1..=2`).
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }

    /// Decodes a `SQLite` integer discriminant into a closed [`StoreCatalogTrack`].
    #[must_use]
    pub const fn decode(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::OfficialTrack),
            2 => Some(Self::CommunityTrack),
            _ => None,
        }
    }

    /// Canonical uppercase identifier string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OfficialTrack => "OFFICIAL_TRACK",
            Self::CommunityTrack => "COMMUNITY_TRACK",
        }
    }
}

impl fmt::Display for StoreCatalogTrack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Multi-component composite package slot classification (`[PKG-MULTI-001]`,
/// `[PKG-SEED-001]`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StoreComponentKind {
    /// Foreground or CLI executable payload (`executableComponents`).
    Executable = 1,
    /// User-space `DriverProvider` adapter (`drivers`).
    Driver = 2,
    /// Declarative `Z2`/`Z3` desktop UI surface bundle (`uiSurfaceComponents`).
    UiSurface = 3,
    /// Managed background service or scheduled worker (`backgroundServiceComponents`).
    BackgroundService = 4,
    /// Immutable read-only seed template or static asset (`seedTemplates`).
    ReadOnlySeedTemplate = 5,
}

impl StoreComponentKind {
    /// Encodes the component kind into its closed 1-byte discriminant (`1..=5`).
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }

    /// Decodes a `SQLite` integer column into a closed [`StoreComponentKind`].
    #[must_use]
    pub const fn decode(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::Executable),
            2 => Some(Self::Driver),
            3 => Some(Self::UiSurface),
            4 => Some(Self::BackgroundService),
            5 => Some(Self::ReadOnlySeedTemplate),
            _ => None,
        }
    }

    /// Converts a declarative `nlos-migrate` [`ComponentRole`] into its store
    /// catalog slot kind.
    #[must_use]
    pub const fn from_manifest_role(role: ComponentRole) -> Self {
        match role {
            ComponentRole::Executable => Self::Executable,
            ComponentRole::Driver => Self::Driver,
            ComponentRole::UiSurface => Self::UiSurface,
            ComponentRole::BackgroundService => Self::BackgroundService,
            ComponentRole::SeedTemplate | ComponentRole::Data => Self::ReadOnlySeedTemplate,
        }
    }

    /// Canonical manifest token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Executable => "executable",
            Self::Driver => "driver",
            Self::UiSurface => "ui-surface",
            Self::BackgroundService => "background-service",
            Self::ReadOnlySeedTemplate => "seed-template",
        }
    }
}

impl fmt::Display for StoreComponentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One immutable component slot declared by a multi-component composite release
/// (`[PKG-MULTI-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreComponentSlot {
    /// Unique component slot name within the release.
    pub slot_name: String,
    /// Component role classification.
    pub kind: StoreComponentKind,
    /// Relative entrypoint path or `OCI` layer selector inside the package.
    pub entrypoint: String,
    /// Content-addressed `SHA-256` digest of this component's artifact payload.
    pub content_sha256: [u8; 32],
    /// Byte size of the component payload.
    pub size_bytes: u64,
}

/// Unsigned payload fields of a store release submission used to compute the
/// publisher's `Ed25519` submission signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsignedStoreReleaseSubmission<'a> {
    /// Immutable package identity.
    pub package_id: PackageId,
    /// Packed semantic version (`pack_package_version(major, minor, patch)`).
    pub package_version: u64,
    /// `llmos.package` manifest `SHA-256` digest.
    pub manifest_digest: [u8; 32],
    /// Content-addressed `OCI` package layout digest in `CAS` (`[PKG-OCI-001]`).
    pub oci_manifest_digest: [u8; 32],
    /// One or more logical [`ApplicationId`]s served by this composite package.
    pub application_ids: &'a [ApplicationId],
    /// Multi-component slots declared by this package (`[PKG-MULTI-001]`).
    pub components: &'a [StoreComponentSlot],
    /// Permission and purpose transparency declarations (`[TUI-CONSENT-001]`).
    pub declared_permissions: &'a [DeclaredPermissionEntry],
    /// Submitting publisher identity (`[PKG-APPID-001]`).
    pub publisher_id: PublisherId,
}

/// Publisher-signed multi-component release submission presented to
/// [`crate::SqliteStoreRegistry::submit_release`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreReleaseSubmission {
    /// Immutable package identity.
    pub package_id: PackageId,
    /// Packed semantic version.
    pub package_version: u64,
    /// `llmos.package` manifest `SHA-256` digest.
    pub manifest_digest: [u8; 32],
    /// Content-addressed `OCI` manifest digest (`[PKG-OCI-001]`).
    pub oci_manifest_digest: [u8; 32],
    /// One or more stable [`ApplicationId`]s declared by the package.
    pub application_ids: Vec<ApplicationId>,
    /// Multi-component slots (`Executable`, `Driver`, `UiSurface`,
    /// `BackgroundService`, `ReadOnlySeedTemplate`).
    pub components: Vec<StoreComponentSlot>,
    /// Declared capability scopes, purposes, effect classes, and egress flags.
    pub declared_permissions: Vec<DeclaredPermissionEntry>,
    /// Claimed publisher identity verified via `PublisherTrustVerifier`.
    pub publisher_id: PublisherId,
    /// Publisher leaf `Ed25519` public key.
    pub publisher_signing_key: [u8; 32],
    /// Publisher `Ed25519` signature over [`compute_release_submission_digest`].
    pub publisher_signature: [u8; 64],
    /// Submission timestamp (`ms` since Unix epoch).
    pub submitted_at_ms: u64,
}

impl StoreReleaseSubmission {
    /// Computes the canonical 32-byte signing digest for this submission.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        compute_release_submission_digest(&UnsignedStoreReleaseSubmission {
            package_id: self.package_id,
            package_version: self.package_version,
            manifest_digest: self.manifest_digest,
            oci_manifest_digest: self.oci_manifest_digest,
            application_ids: &self.application_ids,
            components: &self.components,
            declared_permissions: &self.declared_permissions,
            publisher_id: self.publisher_id,
        })
    }

    /// Validates structural bounds and uniqueness invariants before trust or
    /// storage checks.
    pub(crate) fn validate_structure(&self) -> Result<(), StoreRegistryError> {
        if self.application_ids.is_empty() {
            return Err(StoreRegistryError::EmptyApplications);
        }
        let mut seen_apps = HashSet::with_capacity(self.application_ids.len());
        for app_id in &self.application_ids {
            if !seen_apps.insert(*app_id) {
                return Err(StoreRegistryError::DuplicateApplicationId(*app_id));
            }
        }

        if self.components.is_empty() {
            return Err(StoreRegistryError::EmptyComponents);
        }
        let mut seen_slots = HashSet::with_capacity(self.components.len());
        for slot in &self.components {
            if slot.slot_name.trim().is_empty() {
                return Err(StoreRegistryError::InvalidInput(
                    "component slot_name must be non-empty",
                ));
            }
            if slot.entrypoint.trim().is_empty() {
                return Err(StoreRegistryError::InvalidInput(
                    "component entrypoint must be non-empty",
                ));
            }
            if !seen_slots.insert(slot.slot_name.as_str()) {
                return Err(StoreRegistryError::DuplicateComponentSlot {
                    slot_name: slot.slot_name.clone(),
                });
            }
        }

        let mut seen_scopes = HashSet::with_capacity(self.declared_permissions.len());
        for perm in &self.declared_permissions {
            if perm.capability_scope.trim().is_empty() {
                return Err(StoreRegistryError::InvalidInput(
                    "declared permission capability_scope must be non-empty",
                ));
            }
            if perm.human_purpose.trim().is_empty() {
                return Err(StoreRegistryError::InvalidInput(
                    "declared permission human_purpose must be non-empty",
                ));
            }
            if perm.sandbox_profile.trim().is_empty() {
                return Err(StoreRegistryError::InvalidInput(
                    "declared permission sandbox_profile must be non-empty",
                ));
            }
            if !seen_scopes.insert(perm.capability_scope.as_str()) {
                return Err(StoreRegistryError::DuplicatePermissionScope {
                    capability_scope: perm.capability_scope.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Computes the domain-separated `SHA-256` digest signed by a publisher when
/// submitting a multi-component release to the store governance catalog.
#[must_use]
pub fn compute_release_submission_digest(
    submission: &UnsignedStoreReleaseSubmission<'_>,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(RELEASE_SUBMISSION_SIGNING_DOMAIN);
    hasher.update(submission.package_id.as_bytes());
    hasher.update(submission.package_version.to_be_bytes());
    hasher.update(submission.manifest_digest);
    hasher.update(submission.oci_manifest_digest);
    hasher.update(submission.publisher_id.as_bytes());

    let app_count = u64::try_from(submission.application_ids.len()).unwrap_or(u64::MAX);
    hasher.update(app_count.to_be_bytes());
    for app_id in submission.application_ids {
        hasher.update(app_id.as_bytes());
    }

    let comp_count = u64::try_from(submission.components.len()).unwrap_or(u64::MAX);
    hasher.update(comp_count.to_be_bytes());
    for comp in submission.components {
        hash_len_prefixed(&mut hasher, comp.slot_name.as_bytes());
        hasher.update([comp.kind.encode()]);
        hash_len_prefixed(&mut hasher, comp.entrypoint.as_bytes());
        hasher.update(comp.content_sha256);
        hasher.update(comp.size_bytes.to_be_bytes());
    }

    let perm_count = u64::try_from(submission.declared_permissions.len()).unwrap_or(u64::MAX);
    hasher.update(perm_count.to_be_bytes());
    for perm in submission.declared_permissions {
        hash_len_prefixed(&mut hasher, perm.capability_scope.as_bytes());
        hash_len_prefixed(&mut hasher, perm.human_purpose.as_bytes());
        hasher.update(encode_effect_class(perm.effect_class).to_be_bytes());
        hasher.update([u8::from(perm.network_egress_declared)]);
        hash_len_prefixed(&mut hasher, perm.sandbox_profile.as_bytes());
    }

    hasher.finalize().into()
}

/// Persisted store catalog record for one `(package_id, package_version)` release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreReleaseRecord {
    /// Package identity.
    pub package_id: PackageId,
    /// Packed package version.
    pub package_version: u64,
    /// Package manifest `SHA-256` digest.
    pub manifest_digest: [u8; 32],
    /// Content-addressed `OCI` manifest digest in `CAS`.
    pub oci_manifest_digest: [u8; 32],
    /// Verified publisher identity.
    pub publisher_id: PublisherId,
    /// Verified publisher trust domain at submission time.
    pub publisher_trust_tier: PublisherTrustTier,
    /// Publisher leaf public key used to sign the submission.
    pub publisher_signing_key: [u8; 32],
    /// Publisher `Ed25519` signature over the submission digest.
    pub publisher_signature: [u8; 64],
    /// Current release channel (`Candidate`, `Stable`, or `Recalled`).
    pub channel: StoreChannel,
    /// Catalog isolation track (`OfficialTrack` vs `CommunityTrack`).
    pub track: StoreCatalogTrack,
    /// Active recall severity if a `SignedRecallAdvisory` targets this release.
    pub active_recall_severity: Option<RecallSeverity>,
    /// Ordered [`ApplicationId`]s bound to this release.
    pub application_ids: Vec<ApplicationId>,
    /// Ordered multi-component slots (`[PKG-MULTI-001]`).
    pub components: Vec<StoreComponentSlot>,
    /// Ordered permission transparency entries (`[TUI-CONSENT-001]`).
    pub declared_permissions: Vec<DeclaredPermissionEntry>,
    /// Timestamp when the release was admitted into `Candidate`.
    pub submitted_at_ms: u64,
    /// Timestamp of the last channel or recall transition.
    pub updated_at_ms: u64,
}

/// Independently `Ed25519`-verifiable four-gate certification attestation
/// required to promote an `OfficialTrack` release from `Candidate` to `Stable`
/// (`ADR-0027` section 2.3.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedCertificationAttestation {
    /// Certified package identity.
    pub package_id: PackageId,
    /// Certified logical application identity.
    pub application_id: ApplicationId,
    /// Certified packed package version.
    pub package_version: u64,
    /// Certified `llmos.package` manifest digest.
    pub manifest_digest: [u8; 32],
    /// Certified `OCI` content-addressed manifest digest.
    pub oci_manifest_digest: [u8; 32],
    /// Certified owning publisher identity.
    pub publisher_id: PublisherId,
    /// Bitmask of passed `nlos-certify` gates (must equal `0b1111` for `Stable`).
    pub passed_gates_mask: u8,
    /// Attestation issuance timestamp (`ms` since Unix epoch).
    pub issued_at_ms: u64,
    /// Attestation expiration timestamp (`ms` since Unix epoch).
    pub expires_at_ms: u64,
    /// Certifier `Ed25519` public key.
    pub certifier_public_key: [u8; 32],
    /// Strict `Ed25519` signature over [`Self::signing_digest`].
    pub ed25519_signature: [u8; 64],
}

impl SignedCertificationAttestation {
    /// Computes the canonical domain-separated 32-byte signing digest of this
    /// certification attestation.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(CERTIFICATION_ATTESTATION_SIGNING_DOMAIN);
        hasher.update(self.package_id.as_bytes());
        hasher.update(self.application_id.as_bytes());
        hasher.update(self.package_version.to_be_bytes());
        hasher.update(self.manifest_digest);
        hasher.update(self.oci_manifest_digest);
        hasher.update(self.publisher_id.as_bytes());
        hasher.update([self.passed_gates_mask]);
        hasher.update(self.issued_at_ms.to_be_bytes());
        hasher.update(self.expires_at_ms.to_be_bytes());
        hasher.update(self.certifier_public_key);
        hasher.finalize().into()
    }

    /// Independently verifies `self.ed25519_signature` against
    /// `self.certifier_public_key` under strict `Ed25519` canonical rules.
    ///
    /// # Errors
    ///
    /// Returns [`StoreRegistryError::InvalidPublicKey`] if `certifier_public_key`
    /// is not a valid curve point, or
    /// [`StoreRegistryError::AttestationSignatureInvalid`] if verification fails.
    pub fn verify_signature(&self) -> Result<(), StoreRegistryError> {
        let verifying_key = VerifyingKey::from_bytes(&self.certifier_public_key)
            .map_err(|_| StoreRegistryError::InvalidPublicKey("certifier_public_key"))?;
        let signature = Signature::from_bytes(&self.ed25519_signature);
        verifying_key
            .verify_strict(&self.signing_digest(), &signature)
            .map_err(|_| StoreRegistryError::AttestationSignatureInvalid)
    }
}

/// Immutable receipt persisted when a release promotes from `Candidate` to `Stable`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreCertificationPromotionReceipt {
    /// Deterministic promotion receipt identifier.
    pub receipt_id: ReceiptId,
    /// Caller idempotency key.
    pub idempotency_key: IdempotencyKey,
    /// Promoted package identity.
    pub package_id: PackageId,
    /// Promoted package version.
    pub package_version: u64,
    /// Certified application identity.
    pub application_id: ApplicationId,
    /// Verified four-gate bitmask (`0b1111`).
    pub passed_gates_mask: u8,
    /// Promotion commit timestamp (`ms` since Unix epoch).
    pub promoted_at_ms: u64,
}

/// Outcome of [`crate::SqliteStoreRegistry::promote_candidate_to_stable`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorePromotionDecision {
    /// First execution of the idempotency key: `Candidate -> Stable` committed.
    Promoted(StoreCertificationPromotionReceipt),
    /// Durable idempotent replay returning the original immutable receipt.
    Replayed(StoreCertificationPromotionReceipt),
}

impl StorePromotionDecision {
    /// Unwraps the promotion receipt regardless of whether it was newly committed
    /// or durably replayed.
    #[must_use]
    pub const fn receipt(self) -> StoreCertificationPromotionReceipt {
        match self {
            Self::Promoted(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    hasher.update(len.to_be_bytes());
    hasher.update(bytes);
}
