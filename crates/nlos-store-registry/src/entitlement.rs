//! Opaque `Ed25519`-signed `EntitlementToken` local verification seam and seat
//! counter (`ADR-0027` section 2.3 & section 4; `[NLOS-DEFER-001]`).
//!
//! Boundary (`[NLOS-DEFER-001]`, `v0.5` section 29.1): this module strictly
//! performs cryptographic verification, revocation/expiry checks, and bounded
//! seat/install redemption accounting over opaque signed entitlement tokens.
//! It contains zero fiat currency pricing, zero payment gateway bindings, and
//! zero subscription billing or invoicing logic.

use ed25519_dalek::{Signature, VerifyingKey};
use nlos_publisher_trust::RevocationEpoch;
use nlos_types::{ApplicationId, IdempotencyKey, KeyId, PackageId, PrincipalId, ReceiptId};
use sha2::{Digest, Sha256};

use crate::catalog::StoreChannel;
use crate::error::StoreRegistryError;

/// Domain separator for store-signed [`SignedEntitlementToken`] digests.
pub const ENTITLEMENT_TOKEN_SIGNING_DOMAIN: &[u8] = b"nlos/store-registry/entitlement-token/v1";

/// Domain separator for [`RedeemEntitlementRequest`] idempotency binding digests.
pub(crate) const ENTITLEMENT_REDEEM_REQUEST_DOMAIN: &[u8] =
    b"nlos/store-registry/entitlement-redeem-request/v1";

/// Opaque cryptographically signed entitlement token authorizing a specific
/// [`PrincipalId`] to install an [`ApplicationId`] up to a bounded seat count
/// (`ADR-0027` section 2.3; `[NLOS-DEFER-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedEntitlementToken {
    /// Unique 128-bit entitlement token identifier.
    pub token_id: [u8; 16],
    /// Authorized holder [`PrincipalId`].
    pub holder_principal: PrincipalId,
    /// Authorized logical [`ApplicationId`].
    pub application_id: ApplicationId,
    /// Minimum channel assurance authorized (`Stable` or `Candidate`).
    pub allowed_channel: StoreChannel,
    /// Maximum seat or installation redemptions permitted under this token.
    pub max_seat_or_install_count: u32,
    /// Token issuance timestamp (`ms` since Unix epoch).
    pub issued_at_ms: u64,
    /// Token expiration timestamp (`ms` since Unix epoch).
    pub expires_at_ms: u64,
    /// Minimum `store_revocation_epoch` required on the local store registry to
    /// redeem this token (prevents redeeming against a stale/frozen CRL state).
    pub min_revocation_epoch: RevocationEpoch,
    /// Store entitlement signing [`KeyId`].
    pub issuer_key_id: KeyId,
    /// Domain-separated `Ed25519` signature over [`Self::signing_digest`].
    pub ed25519_signature: [u8; 64],
}

impl SignedEntitlementToken {
    /// Computes the canonical domain-separated 32-byte signing digest of this
    /// entitlement token.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        compute_entitlement_token_digest(
            &self.token_id,
            self.holder_principal,
            self.application_id,
            self.allowed_channel,
            self.max_seat_or_install_count,
            self.issued_at_ms,
            self.expires_at_ms,
            self.min_revocation_epoch,
            self.issuer_key_id,
        )
    }

    /// Verifies `self.ed25519_signature` against `issuer_public_key` under
    /// strict canonical `Ed25519` rules.
    ///
    /// # Errors
    ///
    /// Returns [`StoreRegistryError::InvalidPublicKey`] if `issuer_public_key`
    /// is malformed, or [`StoreRegistryError::EntitlementSignatureInvalid`] if
    /// the signature does not match.
    pub fn verify_signature(&self, issuer_public_key: &[u8; 32]) -> Result<(), StoreRegistryError> {
        let verifying_key = VerifyingKey::from_bytes(issuer_public_key)
            .map_err(|_| StoreRegistryError::InvalidPublicKey("entitlement_issuer_public_key"))?;
        let signature = Signature::from_bytes(&self.ed25519_signature);
        verifying_key
            .verify_strict(&self.signing_digest(), &signature)
            .map_err(|_| StoreRegistryError::EntitlementSignatureInvalid)
    }
}

/// Computes the canonical domain-separated 32-byte signing digest for an
/// entitlement token.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn compute_entitlement_token_digest(
    token_id: &[u8; 16],
    holder_principal: PrincipalId,
    application_id: ApplicationId,
    allowed_channel: StoreChannel,
    max_seat_or_install_count: u32,
    issued_at_ms: u64,
    expires_at_ms: u64,
    min_revocation_epoch: RevocationEpoch,
    issuer_key_id: KeyId,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ENTITLEMENT_TOKEN_SIGNING_DOMAIN);
    hasher.update(token_id);
    hasher.update(holder_principal.as_bytes());
    hasher.update(application_id.as_bytes());
    hasher.update([allowed_channel.encode()]);
    hasher.update(max_seat_or_install_count.to_be_bytes());
    hasher.update(issued_at_ms.to_be_bytes());
    hasher.update(expires_at_ms.to_be_bytes());
    hasher.update(min_revocation_epoch.get().to_be_bytes());
    hasher.update(issuer_key_id.as_bytes());
    hasher.finalize().into()
}

/// Request to offline-verify and redeem one seat against a
/// [`SignedEntitlementToken`] for a specific store release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RedeemEntitlementRequest {
    /// Signed opaque entitlement token presented by the caller.
    pub token: SignedEntitlementToken,
    /// Authenticated local principal requesting installation authorization.
    pub presenter_principal: PrincipalId,
    /// Logical [`ApplicationId`] being installed.
    pub requested_application_id: ApplicationId,
    /// Target [`PackageId`] in the store catalog.
    pub package_id: PackageId,
    /// Target packed package version in the store catalog.
    pub package_version: u64,
    /// Caller-supplied idempotency key for exact-once seat accounting.
    pub idempotency_key: IdempotencyKey,
    /// Local evaluation timestamp (`ms` since Unix epoch).
    pub now_ms: u64,
}

impl RedeemEntitlementRequest {
    pub(crate) fn request_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(ENTITLEMENT_REDEEM_REQUEST_DOMAIN);
        hasher.update(self.token.signing_digest());
        hasher.update(self.token.ed25519_signature);
        hasher.update(self.presenter_principal.as_bytes());
        hasher.update(self.requested_application_id.as_bytes());
        hasher.update(self.package_id.as_bytes());
        hasher.update(self.package_version.to_be_bytes());
        hasher.update(self.idempotency_key.as_bytes());
        hasher.update(self.now_ms.to_be_bytes());
        hasher.finalize().into()
    }
}

/// Immutable receipt produced when an entitlement token is verified and redeemed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntitlementGrantReceipt {
    /// Deterministic immutable receipt identifier.
    pub receipt_id: ReceiptId,
    /// Idempotency key that produced this grant receipt.
    pub idempotency_key: IdempotencyKey,
    /// Redeemed entitlement token identifier.
    pub token_id: [u8; 16],
    /// Verified holder principal.
    pub holder_principal: PrincipalId,
    /// Authorized application identity.
    pub application_id: ApplicationId,
    /// Authorized package identity.
    pub package_id: PackageId,
    /// Authorized package version.
    pub package_version: u64,
    /// Channel of the authorized release at redemption time.
    pub granted_channel: StoreChannel,
    /// 1-based seat ordinal consumed by this grant (`1..=max_seat_or_install_count`).
    pub seat_ordinal: u32,
    /// Remaining seats available on the token after this grant.
    pub remaining_seats: u32,
    /// Durable `store_revocation_epoch` observed at redemption time.
    pub store_revocation_epoch: RevocationEpoch,
    /// Timestamp when the grant committed (`ms` since Unix epoch).
    pub redeemed_at_ms: u64,
}

/// Outcome of [`crate::SqliteStoreRegistry::verify_and_redeem_entitlement`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntitlementRedeemDecision {
    /// First execution of this idempotency key: one seat was consumed and the
    /// immutable receipt committed.
    Granted(EntitlementGrantReceipt),
    /// Durable idempotent replay: returns the original receipt without
    /// consuming an additional seat.
    Replayed(EntitlementGrantReceipt),
}

impl EntitlementRedeemDecision {
    /// Returns the underlying [`EntitlementGrantReceipt`] for either branch.
    #[must_use]
    pub const fn receipt(self) -> EntitlementGrantReceipt {
        match self {
            Self::Granted(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}
