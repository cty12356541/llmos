//! Typed fail-closed error hierarchy for `nlos-store-registry` (`ADR-0027`
//! section 2.3; `[PKG-OCI-001]`, `[PKG-MULTI-001]`, `[PKG-APPID-001]`,
//! `[CAP-REVOKE-001]`).

use std::error::Error;
use std::fmt;

use nlos_application::ApplicationAuthorityError;
use nlos_capability::CapabilityAuthorityError;
use nlos_publisher_trust::{PublisherId, PublisherTrustCheckError, RevocationEpoch};
use nlos_types::{ApplicationId, CapabilityId, KeyId, PackageId, PrincipalId};

use crate::catalog::StoreChannel;
use crate::recall::RecallSeverity;

/// Content-addressed `OCI` blob and layout verification errors (`[PKG-OCI-001]`).
#[derive(Debug)]
pub enum OciCasError {
    /// Underlying filesystem I/O error in the local `OCI` `CAS` directory.
    Io(std::io::Error),
    /// Layer or config `media_type` string was empty or contained ASCII control bytes.
    InvalidMediaType(String),
    /// Package layout declared zero content layers.
    EmptyLayers,
    /// Raw byte stream length differed from `OciLayerDescriptor::size_bytes`.
    LayerSizeMismatch {
        /// Declared size in bytes.
        declared: u64,
        /// Actual observed byte count.
        actual: u64,
    },
    /// Computed `SHA-256` digest differed from `OciLayerDescriptor::sha256_digest`.
    LayerDigestMismatch {
        /// Declared 32-byte `SHA-256` digest.
        declared: [u8; 32],
        /// Actual computed 32-byte `SHA-256` digest.
        computed: [u8; 32],
    },
    /// Referenced content-addressed blob does not exist in `blobs/sha256/`.
    BlobMissing {
        /// Missing 32-byte `SHA-256` blob digest.
        digest: [u8; 32],
    },
    /// Referenced content-addressed manifest does not exist in `manifests/sha256/`.
    ManifestMissing {
        /// Missing 32-byte `SHA-256` manifest digest.
        digest: [u8; 32],
    },
    /// Stored manifest file on disk failed canonical decoding.
    ManifestCorrupt(&'static str),
    /// Recomputed canonical `oci_manifest_digest` did not match the addressed digest.
    ManifestDigestMismatch {
        /// Expected manifest digest used as content address.
        expected: [u8; 32],
        /// Recomputed canonical manifest digest.
        computed: [u8; 32],
    },
}

impl fmt::Display for OciCasError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "OCI CAS I/O failure: {error}"),
            Self::InvalidMediaType(media_type) => {
                write!(formatter, "invalid OCI media_type {media_type:?}")
            }
            Self::EmptyLayers => {
                formatter.write_str("OCI package layout must contain at least one layer")
            }
            Self::LayerSizeMismatch { declared, actual } => write!(
                formatter,
                "OCI layer size mismatch: declared {declared} bytes, actual {actual} bytes"
            ),
            Self::LayerDigestMismatch { declared, computed } => write!(
                formatter,
                "OCI layer SHA-256 mismatch: declared {}, computed {}",
                hex_32(declared),
                hex_32(computed)
            ),
            Self::BlobMissing { digest } => {
                write!(formatter, "OCI CAS blob missing: sha256:{}", hex_32(digest))
            }
            Self::ManifestMissing { digest } => write!(
                formatter,
                "OCI CAS manifest missing: sha256:{}",
                hex_32(digest)
            ),
            Self::ManifestCorrupt(detail) => {
                write!(formatter, "corrupt OCI CAS manifest record: {detail}")
            }
            Self::ManifestDigestMismatch { expected, computed } => write!(
                formatter,
                "OCI manifest digest mismatch: expected {}, computed {}",
                hex_32(expected),
                hex_32(computed)
            ),
        }
    }
}

impl Error for OciCasError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for OciCasError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Fail-closed governance, persistence, entitlement, and recall errors emitted
/// by [`crate::SqliteStoreRegistry`].
#[derive(Debug)]
pub enum StoreRegistryError {
    /// `SQLite` persistence or DDL constraint failure.
    Sqlite(rusqlite::Error),
    /// Filesystem I/O failure when opening the registry directory.
    Io(std::io::Error),
    /// `SQLite` could not establish `journal_mode = WAL` and `synchronous = FULL`.
    DurabilityUnavailable {
        /// Observed `journal_mode`.
        journal_mode: String,
        /// Observed `synchronous` integer code.
        synchronous: i64,
    },
    /// Stored `PRAGMA user_version` is incompatible with this crate.
    SchemaVersionUnsupported(i64),
    /// Durable row violated an invariant during decoding.
    CorruptRecord(&'static str),
    /// Internal `SQLite` connection mutex was poisoned.
    LockPoisoned,
    /// Governance trust anchors supplied on open differed from durable metadata.
    GovernanceAnchorMismatch(&'static str),
    /// Governance database has not yet been initialized with trust anchors.
    GovernanceUninitialized,
    /// Content-addressed `OCI` `CAS` validation failed (`[PKG-OCI-001]`).
    OciCas(OciCasError),
    /// Publisher certificate chain, signature, revocation, or `ApplicationId`
    /// anti-hijack ownership verification failed (`[PKG-APPID-001]`).
    PublisherTrust(PublisherTrustCheckError),
    /// Downstream `CapabilityAuthority` revocation or inspection failed.
    CapabilityAuthority(CapabilityAuthorityError),
    /// Downstream `ApplicationAuthority` quarantine (`disable_application`) failed.
    ApplicationAuthority(ApplicationAuthorityError),
    /// Malformed 32-byte `Ed25519` public key.
    InvalidPublicKey(&'static str),
    /// Invalid caller input or empty structural identifier.
    InvalidInput(&'static str),
    /// An `IdempotencyKey` was reused with a different request payload.
    IdempotencyConflict,
    /// Release submission declared zero `ApplicationId`s.
    EmptyApplications,
    /// Release submission repeated an `ApplicationId`.
    DuplicateApplicationId(ApplicationId),
    /// Release submission declared zero component slots (`[PKG-MULTI-001]`).
    EmptyComponents,
    /// Release submission repeated a component `slot_name`.
    DuplicateComponentSlot {
        /// Duplicate component slot identifier.
        slot_name: String,
    },
    /// Release submission repeated a `capability_scope` in transparency metadata.
    DuplicatePermissionScope {
        /// Duplicate capability scope string.
        capability_scope: String,
    },
    /// `(package_id, package_version)` is already registered with different content.
    ReleaseAlreadyExists {
        /// Package identity.
        package_id: PackageId,
        /// Packed package version.
        package_version: u64,
    },
    /// Requested `(package_id, package_version)` does not exist in the store catalog.
    ReleaseNotFound {
        /// Package identity.
        package_id: PackageId,
        /// Packed package version.
        package_version: u64,
    },
    /// Requested `ApplicationId` is not bound to the target store release.
    ApplicationNotInRelease {
        /// Package identity.
        package_id: PackageId,
        /// Packed package version.
        package_version: u64,
        /// Unbound application identity.
        application_id: ApplicationId,
    },
    /// `CommunitySandboxed` publishers are strictly confined to `CommunityTrack`
    /// and forbidden from entering the official `Stable` channel (`ADR-0027` section 2.3).
    CommunityPublisherForbiddenInStable {
        /// Community publisher identity.
        publisher_id: PublisherId,
        /// Package identity.
        package_id: PackageId,
    },
    /// `SignedCertificationAttestation` certifier key did not match the store trust anchor.
    AttestationUntrustedCertifier,
    /// `SignedCertificationAttestation` `Ed25519` signature failed strict verification.
    AttestationSignatureInvalid,
    /// `SignedCertificationAttestation` did not pass all four mandatory gates (`0b1111`).
    AttestationIncompleteGates {
        /// Observed bitmask of passed gates.
        passed_gates_mask: u8,
        /// Required bitmask (`0b1111`).
        required_mask: u8,
    },
    /// `SignedCertificationAttestation` `issued_at_ms` is in the future relative to `now_ms`.
    AttestationNotYetValid {
        /// Attestation issuance timestamp.
        issued_at_ms: u64,
        /// Evaluation timestamp.
        now_ms: u64,
    },
    /// `SignedCertificationAttestation` has expired at `now_ms`.
    AttestationExpired {
        /// Attestation expiry timestamp.
        expires_at_ms: u64,
        /// Evaluation timestamp.
        now_ms: u64,
    },
    /// `SignedCertificationAttestation` fields did not match the stored candidate release.
    AttestationBindingMismatch(&'static str),
    /// Requested channel transition violates the release channel state machine.
    ChannelTransitionForbidden {
        /// Observed durable channel.
        current_channel: StoreChannel,
        /// Attempted target channel.
        target_channel: StoreChannel,
    },
    /// Operation refused because the release has an active `RecallAdvisory`.
    ReleaseRecalled {
        /// Recalled package identity.
        package_id: PackageId,
        /// Recalled package version.
        package_version: u64,
        /// Active recall severity.
        severity: RecallSeverity,
    },
    /// `SignedEntitlementToken` named an unknown store entitlement `KeyId`.
    EntitlementUnknownIssuerKey(KeyId),
    /// `SignedEntitlementToken` `Ed25519` signature failed strict verification.
    EntitlementSignatureInvalid,
    /// `SignedEntitlementToken` is not yet valid at `now_ms`.
    EntitlementNotYetValid {
        /// Token issuance timestamp.
        issued_at_ms: u64,
        /// Verification timestamp.
        now_ms: u64,
    },
    /// `SignedEntitlementToken` has expired at `now_ms`.
    EntitlementExpired {
        /// Token expiration timestamp.
        expires_at_ms: u64,
        /// Verification timestamp.
        now_ms: u64,
    },
    /// `SignedEntitlementToken` belongs to a different `PrincipalId`.
    EntitlementPrincipalMismatch {
        /// Holder recorded inside the signed token.
        token_holder: PrincipalId,
        /// Principal attempting redemption.
        presenter: PrincipalId,
    },
    /// `SignedEntitlementToken` authorizes a different `ApplicationId`.
    EntitlementApplicationMismatch {
        /// Application recorded inside the signed token.
        token_application: ApplicationId,
        /// Requested application identity.
        requested_application: ApplicationId,
    },
    /// `SignedEntitlementToken` does not permit installing from the release's channel.
    EntitlementChannelDisallowed {
        /// Channel allowed by the token.
        allowed_channel: StoreChannel,
        /// Actual channel of the target release.
        release_channel: StoreChannel,
    },
    /// Local store registry `store_revocation_epoch` is older than the token's
    /// `min_revocation_epoch` freshness floor.
    EntitlementStoreEpochTooStale {
        /// Minimum revocation epoch required by the token.
        token_min_epoch: RevocationEpoch,
        /// Current durable `store_revocation_epoch`.
        store_epoch: RevocationEpoch,
    },
    /// `SignedEntitlementToken` seat/install allowance is exhausted.
    EntitlementSeatLimitExceeded {
        /// 16-byte entitlement token identifier.
        token_id: [u8; 16],
        /// Maximum seats authorized by the token.
        max_seats: u32,
        /// Already redeemed seat count.
        redeemed_seats: u32,
    },
    /// `SignedRecallAdvisory` `Ed25519` signature failed strict verification.
    RecallSignatureInvalid,
    /// `SignedRecallAdvisory` `revocation_epoch` did not strictly advance
    /// `store_revocation_epoch` (anti-replay / anti-rollback refusal).
    RecallEpochRollback {
        /// Current durable `store_revocation_epoch`.
        current_epoch: RevocationEpoch,
        /// Candidate advisory epoch.
        candidate_epoch: RevocationEpoch,
    },
    /// Attempted to downgrade an active recall severity (e.g. `QuarantineInstallation`
    /// -> `WarnNewInstall`).
    RecallSeverityDowngradeRefused {
        /// Current active severity on the release.
        current_severity: RecallSeverity,
        /// Attempted lower severity.
        attempted_severity: RecallSeverity,
    },
    /// Level 2 or Level 3 recall requires `IdentityAuthority` + `CapabilityAuthority`
    /// revocation enforcement context.
    RecallCapabilityAuthoritiesRequired,
    /// Level 3 (`QuarantineInstallation`) recall requires `ApplicationAuthority`
    /// quarantine enforcement context.
    RecallApplicationAuthorityRequired,
    /// Attempted to re-bind an existing `CapabilityId` to conflicting package metadata.
    CapabilityBindingConflict {
        /// Conflicting capability identifier.
        capability_id: CapabilityId,
    },
}

impl fmt::Display for StoreRegistryError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite store registry failure: {error}"),
            Self::Io(error) => write!(formatter, "store registry I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported store registry schema version {version}"
                )
            }
            Self::CorruptRecord(reason) => {
                write!(formatter, "corrupt store registry record: {reason}")
            }
            Self::LockPoisoned => formatter.write_str("store registry mutex is poisoned"),
            Self::GovernanceAnchorMismatch(field) => write!(
                formatter,
                "store governance trust anchor mismatch on reopen: {field}"
            ),
            Self::GovernanceUninitialized => {
                formatter.write_str("store governance database has not been initialized")
            }
            Self::OciCas(error) => write!(formatter, "OCI CAS verification failed: {error}"),
            Self::PublisherTrust(error) => {
                write!(formatter, "publisher trust verification failed: {error}")
            }
            Self::CapabilityAuthority(error) => {
                write!(
                    formatter,
                    "capability authority revocation failed: {error}"
                )
            }
            Self::ApplicationAuthority(error) => {
                write!(
                    formatter,
                    "application authority quarantine failed: {error}"
                )
            }
            Self::InvalidPublicKey(context) => {
                write!(formatter, "invalid Ed25519 public key for {context}")
            }
            Self::InvalidInput(reason) => write!(formatter, "invalid store input: {reason}"),
            Self::IdempotencyConflict => {
                formatter.write_str("idempotency key reused with conflicting store command")
            }
            Self::EmptyApplications => {
                formatter.write_str("store release must declare at least one ApplicationId")
            }
            Self::DuplicateApplicationId(app_id) => {
                write!(
                    formatter,
                    "duplicate ApplicationId {app_id:?} in release submission"
                )
            }
            Self::EmptyComponents => formatter.write_str(
                "store release must declare at least one component slot (PKG-MULTI-001)",
            ),
            Self::DuplicateComponentSlot { slot_name } => {
                write!(formatter, "duplicate component slot {slot_name:?}")
            }
            Self::DuplicatePermissionScope { capability_scope } => {
                write!(
                    formatter,
                    "duplicate declared permission capability_scope {capability_scope:?}"
                )
            }
            Self::ReleaseAlreadyExists {
                package_id,
                package_version,
            } => write!(
                formatter,
                "store release ({package_id:?}, version={package_version}) already exists"
            ),
            Self::ReleaseNotFound {
                package_id,
                package_version,
            } => write!(
                formatter,
                "store release ({package_id:?}, version={package_version}) not found in governance catalog"
            ),
            Self::ApplicationNotInRelease {
                package_id,
                package_version,
                application_id,
            } => write!(
                formatter,
                "application {application_id:?} is not declared by release ({package_id:?}, version={package_version})"
            ),
            Self::CommunityPublisherForbiddenInStable {
                publisher_id,
                package_id,
            } => write!(
                formatter,
                "COMMUNITY_SANDBOXED publisher {publisher_id} cannot promote {package_id:?} to official Stable channel"
            ),
            Self::AttestationUntrustedCertifier => {
                formatter.write_str("certification attestation public key is not trusted")
            }
            Self::AttestationSignatureInvalid => formatter.write_str(
                "certification attestation Ed25519 signature verification failed",
            ),
            Self::AttestationIncompleteGates {
                passed_gates_mask,
                required_mask,
            } => write!(
                formatter,
                "certification attestation passed_gates_mask 0b{passed_gates_mask:04b} does not satisfy required 0b{required_mask:04b}"
            ),
            Self::AttestationNotYetValid {
                issued_at_ms,
                now_ms,
            } => write!(
                formatter,
                "certification attestation issued at {issued_at_ms}ms is not yet valid at {now_ms}ms"
            ),
            Self::AttestationExpired {
                expires_at_ms,
                now_ms,
            } => write!(
                formatter,
                "certification attestation expired at {expires_at_ms}ms (now {now_ms}ms)"
            ),
            Self::AttestationBindingMismatch(field) => write!(
                formatter,
                "certification attestation binding mismatch on {field}"
            ),
            Self::ChannelTransitionForbidden {
                current_channel,
                target_channel,
            } => write!(
                formatter,
                "forbidden store channel transition from {current_channel} to {target_channel}"
            ),
            Self::ReleaseRecalled {
                package_id,
                package_version,
                severity,
            } => write!(
                formatter,
                "release ({package_id:?}, version={package_version}) is under active recall ({severity})"
            ),
            Self::EntitlementUnknownIssuerKey(key_id) => {
                write!(formatter, "unknown entitlement issuer key {key_id:?}")
            }
            Self::EntitlementSignatureInvalid => {
                formatter.write_str("entitlement token Ed25519 signature verification failed")
            }
            Self::EntitlementNotYetValid {
                issued_at_ms,
                now_ms,
            } => write!(
                formatter,
                "entitlement token issued at {issued_at_ms}ms is not yet valid at {now_ms}ms"
            ),
            Self::EntitlementExpired {
                expires_at_ms,
                now_ms,
            } => write!(
                formatter,
                "entitlement token expired at {expires_at_ms}ms (now {now_ms}ms)"
            ),
            Self::EntitlementPrincipalMismatch {
                token_holder,
                presenter,
            } => write!(
                formatter,
                "entitlement holder mismatch: token bound to {token_holder:?}, presented by {presenter:?}"
            ),
            Self::EntitlementApplicationMismatch {
                token_application,
                requested_application,
            } => write!(
                formatter,
                "entitlement application mismatch: token bound to {token_application:?}, requested {requested_application:?}"
            ),
            Self::EntitlementChannelDisallowed {
                allowed_channel,
                release_channel,
            } => write!(
                formatter,
                "entitlement allows channel {allowed_channel}, but release is in {release_channel}"
            ),
            Self::EntitlementStoreEpochTooStale {
                token_min_epoch,
                store_epoch,
            } => write!(
                formatter,
                "local store revocation epoch {store_epoch} is older than token min_revocation_epoch {token_min_epoch}"
            ),
            Self::EntitlementSeatLimitExceeded {
                token_id,
                max_seats,
                redeemed_seats,
            } => write!(
                formatter,
                "entitlement token {} seat limit exhausted ({redeemed_seats}/{max_seats})",
                hex_16(token_id)
            ),
            Self::RecallSignatureInvalid => {
                formatter.write_str("recall advisory Ed25519 signature verification failed")
            }
            Self::RecallEpochRollback {
                current_epoch,
                candidate_epoch,
            } => write!(
                formatter,
                "recall advisory revocation_epoch {candidate_epoch} does not advance durable store_revocation_epoch {current_epoch}"
            ),
            Self::RecallSeverityDowngradeRefused {
                current_severity,
                attempted_severity,
            } => write!(
                formatter,
                "refusing recall severity downgrade from {current_severity} to {attempted_severity}"
            ),
            Self::RecallCapabilityAuthoritiesRequired => formatter.write_str(
                "Level 2/3 recall requires IdentityAuthority and CapabilityAuthority enforcement context",
            ),
            Self::RecallApplicationAuthorityRequired => formatter.write_str(
                "Level 3 QuarantineInstallation recall requires ApplicationAuthority enforcement context",
            ),
            Self::CapabilityBindingConflict { capability_id } => write!(
                formatter,
                "capability {capability_id:?} is already bound to conflicting store installation metadata"
            ),
        }
    }
}

impl Error for StoreRegistryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::OciCas(error) => Some(error),
            Self::PublisherTrust(error) => Some(error),
            Self::CapabilityAuthority(error) => Some(error),
            Self::ApplicationAuthority(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreRegistryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<OciCasError> for StoreRegistryError {
    fn from(error: OciCasError) -> Self {
        Self::OciCas(error)
    }
}

impl From<PublisherTrustCheckError> for StoreRegistryError {
    fn from(error: PublisherTrustCheckError) -> Self {
        Self::PublisherTrust(error)
    }
}

impl From<CapabilityAuthorityError> for StoreRegistryError {
    fn from(error: CapabilityAuthorityError) -> Self {
        Self::CapabilityAuthority(error)
    }
}

impl From<ApplicationAuthorityError> for StoreRegistryError {
    fn from(error: ApplicationAuthorityError) -> Self {
        Self::ApplicationAuthority(error)
    }
}

pub(crate) fn hex_16(bytes: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

pub(crate) fn hex_32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}
