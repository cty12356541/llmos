//! Three-tier malicious package `RecallAdvisory` state machine with immediate
//! `CapabilityAuthority` revocation and `ApplicationAuthority` quarantine
//! (`ADR-0027` section 2.3.2; `[CAP-REVOKE-001]`, `[DUI-OFFLINE-001]`).

use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use nlos_application::ApplicationAuthority;
use nlos_capability::{CapabilityAuthority, CapabilityHandle};
use nlos_identity::IdentityAuthority;
use nlos_publisher_trust::RevocationEpoch;
use nlos_types::{
    ApplicationId, CapabilityId, Generation, IdempotencyKey, InstallationId, KeyId, PackageId,
    PrincipalId, ReceiptId,
};
use sha2::{Digest, Sha256};

use crate::error::StoreRegistryError;

/// Domain separator for store security authority [`SignedRecallAdvisory`] digests.
pub const RECALL_ADVISORY_SIGNING_DOMAIN: &[u8] = b"nlos/store-registry/recall-advisory/v1";

/// Domain separator for [`SignedRecallAdvisory`] idempotency request binding.
pub(crate) const RECALL_APPLY_REQUEST_DOMAIN: &[u8] =
    b"nlos/store-registry/recall-apply-request/v1";

/// Three-level malicious or vulnerable package recall disposition
/// (`ADR-0027` section 2.3.2; `[CAP-REVOKE-001]`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecallSeverity {
    /// Level 1: Blocks/warns new store downloads and entitlement redemptions,
    /// while preserving offline usability of already-installed local instances
    /// with an explicit risk warning (`[DUI-OFFLINE-001]`).
    WarnNewInstall = 1,
    /// Level 2: Atomically transitions targeted releases to
    /// [`crate::StoreChannel::Recalled`], blocks new installs and new launches,
    /// and immediately revokes all granted [`CapabilityId`] handles registered
    /// for the package/application in [`CapabilityAuthority`] (`[CAP-REVOKE-001]`).
    BlockLaunchAndRevokeCap = 2,
    /// Level 3: Performs full Level 2 channel recall and immediate
    /// [`CapabilityAuthority`] handle revocation, and additionally quarantines
    /// the local installation into `ApplicationStatus::Disabled` via
    /// [`ApplicationAuthority::disable_application`] while preserving user data
    /// containers untouched for forensics or user export.
    QuarantineInstallation = 3,
}

impl RecallSeverity {
    /// Returns the closed 1-byte discriminant (`1..=3`).
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }

    /// Decodes a `SQLite` integer column into a closed [`RecallSeverity`].
    #[must_use]
    pub const fn decode(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::WarnNewInstall),
            2 => Some(Self::BlockLaunchAndRevokeCap),
            3 => Some(Self::QuarantineInstallation),
            _ => None,
        }
    }

    /// Returns the canonical `ADR-0027` uppercase token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WarnNewInstall => "WARN_NEW_INSTALL",
            Self::BlockLaunchAndRevokeCap => "BLOCK_LAUNCH_AND_REVOKE_CAP",
            Self::QuarantineInstallation => "QUARANTINE_INSTALLATION",
        }
    }
}

impl fmt::Display for RecallSeverity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Store security authority `Ed25519`-signed recall advisory (`ADR-0027`
/// section 2.3.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedRecallAdvisory {
    /// Unique 128-bit advisory identifier.
    pub advisory_id: [u8; 16],
    /// Recalled package identity.
    pub package_id: PackageId,
    /// Recalled logical application identity.
    pub application_id: ApplicationId,
    /// Optional packed version target (`None` recalls every version of `package_id`).
    pub target_version: Option<u64>,
    /// Three-tier recall severity (`WarnNewInstall`, `BlockLaunchAndRevokeCap`,
    /// `QuarantineInstallation`).
    pub severity: RecallSeverity,
    /// Strictly monotonic revocation epoch carried by this advisory.
    pub revocation_epoch: RevocationEpoch,
    /// Human-readable security advisory reason.
    pub reason: String,
    /// Advisory issuance timestamp (`ms` since Unix epoch).
    pub issued_at_ms: u64,
    /// Store security authority `Ed25519` signature over [`Self::signing_digest`].
    pub authority_signature: [u8; 64],
}

impl SignedRecallAdvisory {
    /// Computes the canonical domain-separated 32-byte signing digest of this
    /// recall advisory.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        compute_recall_advisory_digest(
            &self.advisory_id,
            self.package_id,
            self.application_id,
            self.target_version,
            self.severity,
            self.revocation_epoch,
            &self.reason,
            self.issued_at_ms,
        )
    }

    /// Verifies `self.authority_signature` against `recall_authority_public_key`
    /// under strict canonical `Ed25519` rules.
    ///
    /// # Errors
    ///
    /// Returns [`StoreRegistryError::InvalidPublicKey`] if the authority key is
    /// malformed, or [`StoreRegistryError::RecallSignatureInvalid`] if signature
    /// verification fails.
    pub fn verify_signature(
        &self,
        recall_authority_public_key: &[u8; 32],
    ) -> Result<(), StoreRegistryError> {
        let verifying_key = VerifyingKey::from_bytes(recall_authority_public_key)
            .map_err(|_| StoreRegistryError::InvalidPublicKey("recall_authority_public_key"))?;
        let signature = Signature::from_bytes(&self.authority_signature);
        verifying_key
            .verify_strict(&self.signing_digest(), &signature)
            .map_err(|_| StoreRegistryError::RecallSignatureInvalid)
    }

    pub(crate) fn apply_request_digest(
        &self,
        idempotency_key: IdempotencyKey,
        applied_at_ms: u64,
    ) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(RECALL_APPLY_REQUEST_DOMAIN);
        hasher.update(self.signing_digest());
        hasher.update(self.authority_signature);
        hasher.update(idempotency_key.as_bytes());
        hasher.update(applied_at_ms.to_be_bytes());
        hasher.finalize().into()
    }
}

/// Computes the canonical domain-separated 32-byte signing digest for a
/// [`SignedRecallAdvisory`].
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn compute_recall_advisory_digest(
    advisory_id: &[u8; 16],
    package_id: PackageId,
    application_id: ApplicationId,
    target_version: Option<u64>,
    severity: RecallSeverity,
    revocation_epoch: RevocationEpoch,
    reason: &str,
    issued_at_ms: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(RECALL_ADVISORY_SIGNING_DOMAIN);
    hasher.update(advisory_id);
    hasher.update(package_id.as_bytes());
    hasher.update(application_id.as_bytes());
    match target_version {
        Some(version) => {
            hasher.update([1_u8]);
            hasher.update(version.to_be_bytes());
        }
        None => hasher.update([0_u8]),
    }
    hasher.update([severity.encode()]);
    hasher.update(revocation_epoch.get().to_be_bytes());
    let reason_bytes = reason.as_bytes();
    let reason_len = u64::try_from(reason_bytes.len()).unwrap_or(u64::MAX);
    hasher.update(reason_len.to_be_bytes());
    hasher.update(reason_bytes);
    hasher.update(issued_at_ms.to_be_bytes());
    hasher.finalize().into()
}

/// Request to bind an issued [`CapabilityHandle`] to a local package/application
/// installation so Level 2 and Level 3 `RecallAdvisory` transitions immediately
/// fence and revoke all granted capabilities (`[CAP-REVOKE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterGrantedCapabilityRequest {
    /// Active capability handle issued to or for the installed application.
    pub handle: CapabilityHandle,
    /// Package identity owning this grant.
    pub package_id: PackageId,
    /// Packed package version owning this grant.
    pub package_version: u64,
    /// Logical application identity owning this grant.
    pub application_id: ApplicationId,
    /// Optional local [`InstallationId`] receipt binding.
    pub installation_id: Option<InstallationId>,
    /// Registration timestamp (`ms` since Unix epoch).
    pub registered_at_ms: u64,
}

/// Durable binding record linking a granted [`CapabilityId`] to an installed
/// store package/application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GrantedCapabilityBindingRecord {
    /// Granted capability identifier.
    pub capability_id: CapabilityId,
    /// Generation at which the capability was bound.
    pub issued_generation: Generation,
    /// Owning package identity.
    pub package_id: PackageId,
    /// Owning package version.
    pub package_version: u64,
    /// Owning application identity.
    pub application_id: ApplicationId,
    /// Optional local installation identifier.
    pub installation_id: Option<InstallationId>,
    /// Timestamp when the binding was recorded.
    pub registered_at_ms: u64,
    /// Advisory identifier that revoked this capability, if revoked by recall.
    pub revoked_by_advisory_id: Option<[u8; 16]>,
    /// Resulting fenced generation in `CapabilityAuthority` after revocation.
    pub revoked_resulting_generation: Option<Generation>,
    /// Revocation timestamp (`ms` since Unix epoch), if revoked.
    pub revoked_at_ms: Option<u64>,
}

/// Signing seam producing `ADR-0010` `Ed25519` command signatures for
/// [`CapabilityAuthority::revoke_signed`] during Level 2 / Level 3 recalls.
pub trait CapabilityRevocationAuthorizer {
    /// Returns the acting revoker [`PrincipalId`] (must be the capability's
    /// issuer or holder in `CapabilityAuthority`).
    fn revoker_principal(&self) -> PrincipalId;

    /// Returns the acting revoker's active [`KeyId`] in `IdentityAuthority`.
    fn revoker_key_id(&self) -> KeyId;

    /// Signs the 32-byte `nlos_capability::revoke_command_message` digest.
    ///
    /// # Errors
    ///
    /// Returns [`StoreRegistryError`] if signing cannot be completed.
    fn sign_revoke_command_digest(
        &self,
        command_digest: &[u8; 32],
    ) -> Result<[u8; 64], StoreRegistryError>;
}

/// Operator/issuer [`CapabilityRevocationAuthorizer`] backed by an in-memory
/// `Ed25519` [`SigningKey`] registered in [`IdentityAuthority`].
pub struct DirectEd25519RevocationAuthorizer {
    principal_id: PrincipalId,
    key_id: KeyId,
    signing_key: SigningKey,
}

impl fmt::Debug for DirectEd25519RevocationAuthorizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DirectEd25519RevocationAuthorizer")
            .field("principal_id", &self.principal_id)
            .field("key_id", &self.key_id)
            .field("signing_key", &"<redacted-ed25519-key>")
            .finish()
    }
}

impl DirectEd25519RevocationAuthorizer {
    /// Constructs a revocation command authorizer from an operator/issuer
    /// principal, active `KeyId`, and 32-byte `Ed25519` private seed.
    #[must_use]
    pub fn from_seed(principal_id: PrincipalId, key_id: KeyId, seed: &[u8; 32]) -> Self {
        Self {
            principal_id,
            key_id,
            signing_key: SigningKey::from_bytes(seed),
        }
    }
}

impl CapabilityRevocationAuthorizer for DirectEd25519RevocationAuthorizer {
    fn revoker_principal(&self) -> PrincipalId {
        self.principal_id
    }

    fn revoker_key_id(&self) -> KeyId {
        self.key_id
    }

    fn sign_revoke_command_digest(
        &self,
        command_digest: &[u8; 32],
    ) -> Result<[u8; 64], StoreRegistryError> {
        Ok(self.signing_key.sign(command_digest).to_bytes())
    }
}

/// Live local authorities wired into [`crate::SqliteStoreRegistry::apply_recall_advisory`]
/// to enforce immediate capability revocation (`Level 2/3`) and local
/// installation quarantine (`Level 3`).
pub struct RecallEnforcementAuthorities<'a> {
    /// Local `IdentityAuthority` authenticating capability revocation commands.
    pub identity: &'a IdentityAuthority,
    /// Local `CapabilityAuthority` executing immediate generation-fenced revocations.
    pub capabilities: &'a CapabilityAuthority,
    /// Operator/issuer command signer authorized to revoke the package's capabilities.
    pub revocation_authorizer: &'a dyn CapabilityRevocationAuthorizer,
    /// Local `ApplicationAuthority` transitioning quarantined installations to
    /// `ApplicationStatus::Disabled` (required for `QuarantineInstallation`).
    pub applications: Option<&'a ApplicationAuthority>,
}

/// Local launch policy evaluation for an already-installed offline instance
/// (`[DUI-OFFLINE-001]`, `ADR-0027` Decision Point `E4`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalLaunchDisposition {
    /// Release has no active recall; normal launch is permitted.
    Allowed,
    /// Release is under `RecallSeverity::WarnNewInstall`: new store installs are
    /// blocked, while existing offline local instances remain runnable with a
    /// recorded risk warning.
    AllowedWithRiskWarning {
        /// Advisory reason surfaced to the local audit log / UI banner.
        reason: String,
    },
}

/// Immutable audit receipt produced when a [`SignedRecallAdvisory`] is applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecallExecutionReceipt {
    /// Deterministic receipt identifier.
    pub receipt_id: ReceiptId,
    /// Caller idempotency key.
    pub idempotency_key: IdempotencyKey,
    /// Applied advisory identifier.
    pub advisory_id: [u8; 16],
    /// Recalled package identity.
    pub package_id: PackageId,
    /// Recalled application identity.
    pub application_id: ApplicationId,
    /// Optional targeted package version (`None` = all versions).
    pub target_version: Option<u64>,
    /// Applied recall severity.
    pub severity: RecallSeverity,
    /// New durable `store_revocation_epoch` after applying this advisory.
    pub revocation_epoch: RevocationEpoch,
    /// Number of granted capabilities revoked in `CapabilityAuthority`.
    pub revoked_capability_count: u32,
    /// Whether the local installation in `ApplicationAuthority` was quarantined
    /// into `ApplicationStatus::Disabled` (`Level 3`).
    pub application_quarantined: bool,
    /// Timestamp when the recall was applied (`ms` since Unix epoch).
    pub applied_at_ms: u64,
}

/// Outcome of [`crate::SqliteStoreRegistry::apply_recall_advisory`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecallApplyDecision {
    /// First execution of the idempotency key: epoch advanced and enforcement
    /// side effects committed.
    Applied(RecallExecutionReceipt),
    /// Durable idempotent replay returning the recorded execution receipt.
    Replayed(RecallExecutionReceipt),
}

impl RecallApplyDecision {
    /// Unwraps the [`RecallExecutionReceipt`] for either branch.
    #[must_use]
    pub fn receipt(self) -> RecallExecutionReceipt {
        match self {
            Self::Applied(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}
