//! Cryptographic peer identity signing and verification traits backed by
//! [`nlos_identity::IdentityAuthority`] and
//! [`nlos_capability::custody::SigningKeyCustodyVault`] (`[TCB-ENDPOINT-001]`,
//! `ADR-0026` §2.2 & §6.2).

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use nlos_capability::custody::{CustodySignRequest, CustodyVaultError, SigningKeyCustodyVault};
use nlos_identity::{
    IdentityAuthority, IdentityAuthorityError, VerifyCapabilityCommandSignatureRequest,
};
use nlos_types::{ControlDomainId, Generation, KeyId, PrincipalId};

/// Verified peer key binding produced when a peer's X.509 principal extension
/// or 5-axis TLS Exporter handshake signature passes strict verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedPeerKey {
    /// Authenticated peer `PrincipalId`.
    pub principal_id: PrincipalId,
    /// Authoritative `ControlDomainId` owning `principal_id`.
    pub control_domain_id: ControlDomainId,
    /// Active `KeyId` verified for this signature.
    pub key_id: KeyId,
    /// Active `Generation` (`rotation_epoch`) verified for this signature.
    pub key_generation: Generation,
}

/// Fail-closed identity / custody verification errors during mTLS and 5-axis
/// Exporter handshake (`[TCB-ENDPOINT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PeerIdentityError {
    /// `PrincipalId` is not registered in `IdentityAuthority`.
    PrincipalUnknown(PrincipalId),
    /// Signing key has been revoked in `IdentityAuthority`.
    KeyRevoked {
        /// Subject principal.
        principal: PrincipalId,
        /// Optional revoked key id if known.
        key_id: Option<KeyId>,
    },
    /// Signing key slot carries a durable compromise tombstone in
    /// `SigningKeyCustodyVault`.
    KeyCompromised {
        /// Compromised key id.
        key_id: KeyId,
        /// Compromised rotation epoch.
        rotation_epoch: u64,
        /// Timestamp (`ms`) from which the key was marked compromised.
        revoked_from_ms: u64,
    },
    /// Presented key id or generation does not match the active unrotated key
    /// binding of `principal_id`.
    KeyRotatedOrStale {
        /// Presented key id.
        presented_key_id: KeyId,
        /// Presented key generation.
        presented_generation: Generation,
        /// Active key generation currently registered, if known.
        active_generation: Option<Generation>,
    },
    /// Key validity interval has not started yet.
    KeyNotYetValid,
    /// Key validity interval has expired.
    KeyExpired,
    /// Ed25519 signature failed strict canonical verification.
    SignatureInvalid,
    /// Custody signing failed (e.g. local key revoked/expired).
    CustodySignFailed(String),
    /// Underlying authority storage or configuration failure.
    AuthorityFailure(String),
}

impl fmt::Display for PeerIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrincipalUnknown(id) => write!(formatter, "unknown peer principal {id:?}"),
            Self::KeyRevoked { principal, key_id } => {
                write!(
                    formatter,
                    "peer principal {principal:?} signing key {key_id:?} is revoked"
                )
            }
            Self::KeyCompromised {
                key_id,
                rotation_epoch,
                revoked_from_ms,
            } => write!(
                formatter,
                "peer key ({key_id:?}, epoch {rotation_epoch}) compromised at {revoked_from_ms} ms"
            ),
            Self::KeyRotatedOrStale {
                presented_key_id,
                presented_generation,
                active_generation,
            } => write!(
                formatter,
                "stale or mismatched peer key ({presented_key_id:?}, gen {} vs active {active_generation:?})",
                presented_generation.get()
            ),
            Self::KeyNotYetValid => formatter.write_str("peer signing key is not yet valid"),
            Self::KeyExpired => formatter.write_str("peer signing key has expired"),
            Self::SignatureInvalid => formatter.write_str("peer Ed25519 signature is invalid"),
            Self::CustodySignFailed(reason) => {
                write!(formatter, "local custody signing failed: {reason}")
            }
            Self::AuthorityFailure(reason) => {
                write!(formatter, "identity/custody authority failure: {reason}")
            }
        }
    }
}

impl Error for PeerIdentityError {}

/// Sealed digest signer used by a `CellHost` to bind its ephemeral TLS 1.3
/// certificate and sign per-connection 5-axis TLS Exporter challenges without
/// exposing private key seeds.
pub trait PeerDigestSigner: Send + Sync {
    /// Local node `PrincipalId`.
    fn principal_id(&self) -> PrincipalId;

    /// Local node `KeyId`.
    fn key_id(&self) -> KeyId;

    /// Active key generation (`rotation_epoch`).
    fn key_generation(&self) -> Generation;

    /// Signs `message_digest` inside key custody under `domain_tag` audit
    /// attribution.
    ///
    /// # Errors
    ///
    /// Fails closed if the local custody slot is revoked, rotated, expired, or
    /// unavailable.
    fn sign_domain_digest(
        &self,
        domain_tag: &'static [u8],
        message_digest: [u8; 32],
        signed_at_ms: u64,
    ) -> Result<[u8; 64], PeerIdentityError>;
}

/// Production [`PeerDigestSigner`] backed directly by
/// [`SigningKeyCustodyVault`] (`ADR-0022` & `ADR-0026` §6.2): private seeds
/// never cross the vault boundary, and every handshake/cert signature emits an
/// immutable `CustodySignAuditReceipt`.
#[derive(Clone, Debug)]
pub struct CustodyPeerSigner {
    vault: Arc<SigningKeyCustodyVault>,
    principal_id: PrincipalId,
    key_id: KeyId,
    key_generation: Generation,
}

impl CustodyPeerSigner {
    /// Binds a signer handle to an active `(principal_id, key_id, key_generation)`
    /// slot inside `vault`.
    #[must_use]
    pub const fn new(
        vault: Arc<SigningKeyCustodyVault>,
        principal_id: PrincipalId,
        key_id: KeyId,
        key_generation: Generation,
    ) -> Self {
        Self {
            vault,
            principal_id,
            key_id,
            key_generation,
        }
    }

    /// Reference to the underlying custody vault (for audit receipt inspection).
    #[must_use]
    pub fn vault(&self) -> &SigningKeyCustodyVault {
        &self.vault
    }
}

impl PeerDigestSigner for CustodyPeerSigner {
    fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    fn key_id(&self) -> KeyId {
        self.key_id
    }

    fn key_generation(&self) -> Generation {
        self.key_generation
    }

    fn sign_domain_digest(
        &self,
        domain_tag: &'static [u8],
        message_digest: [u8; 32],
        signed_at_ms: u64,
    ) -> Result<[u8; 64], PeerIdentityError> {
        let receipt = self
            .vault
            .sign_digest_under_custody(CustodySignRequest {
                key_id: self.key_id,
                expected_rotation_epoch: Some(self.key_generation.get()),
                domain_tag,
                message_digest,
                signed_at_ms,
            })
            .map_err(|err| PeerIdentityError::CustodySignFailed(err.to_string()))?;
        Ok(receipt.signature)
    }
}

/// Verifier anchoring peer `PrincipalId` public keys, active key generations,
/// and compromise/revocation tombstones (`[TCB-ENDPOINT-001]`).
pub trait IdentityPeerVerifier: Send + Sync {
    /// Verifies `signature` over `message_digest` against `principal`'s active,
    /// non-revoked, non-compromised `(key_id, key_generation)` binding at
    /// `verified_at_ms`.
    ///
    /// # Errors
    ///
    /// Fails closed on unknown principals, revoked or compromised keys, rotated
    /// stale generations, validity window violations, or bad signatures.
    fn verify_peer_digest(
        &self,
        principal: PrincipalId,
        key_id: KeyId,
        key_generation: Generation,
        message_digest: [u8; 32],
        signature: [u8; 64],
        verified_at_ms: u64,
    ) -> Result<VerifiedPeerKey, PeerIdentityError>;
}

/// Production [`IdentityPeerVerifier`] anchored to [`IdentityAuthority`] and
/// zero or more [`SigningKeyCustodyVault`] tombstone stores (`ADR-0026` §2.2 &
/// §6.2).
pub struct AuthorityCustodyPeerVerifier {
    identity: Arc<IdentityAuthority>,
    custody_vaults: Vec<Arc<SigningKeyCustodyVault>>,
}

impl fmt::Debug for AuthorityCustodyPeerVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorityCustodyPeerVerifier")
            .field("custody_vault_count", &self.custody_vaults.len())
            .finish_non_exhaustive()
    }
}

impl AuthorityCustodyPeerVerifier {
    /// Creates a verifier backed by `IdentityAuthority` and optional custody
    /// tombstone stores.
    #[must_use]
    pub fn new(
        identity: Arc<IdentityAuthority>,
        custody_vaults: Vec<Arc<SigningKeyCustodyVault>>,
    ) -> Self {
        Self {
            identity,
            custody_vaults,
        }
    }
}

impl IdentityPeerVerifier for AuthorityCustodyPeerVerifier {
    fn verify_peer_digest(
        &self,
        principal: PrincipalId,
        key_id: KeyId,
        key_generation: Generation,
        message_digest: [u8; 32],
        signature: [u8; 64],
        verified_at_ms: u64,
    ) -> Result<VerifiedPeerKey, PeerIdentityError> {
        // 1. Check any registered custody vaults first so durable compromise
        // tombstones (`CompromisedRevoked`) fail closed immediately even if an
        // out-of-band sync lag has not yet updated IdentityAuthority.
        for vault in &self.custody_vaults {
            match vault.verify_active_epoch_signature(
                key_id,
                key_generation.get(),
                &message_digest,
                &signature,
                verified_at_ms,
            ) {
                Ok(verified) => {
                    if verified.principal_id != principal {
                        return Err(PeerIdentityError::SignatureInvalid);
                    }
                }
                Err(CustodyVaultError::KeySlotNotFound { .. }) => {
                    // Vault belongs to a different CellHost/subset; continue to
                    // IdentityAuthority verification below.
                }
                Err(CustodyVaultError::KeyCompromised {
                    key_id: compromised_id,
                    rotation_epoch,
                    revoked_from_ms,
                }) => {
                    return Err(PeerIdentityError::KeyCompromised {
                        key_id: compromised_id,
                        rotation_epoch,
                        revoked_from_ms,
                    });
                }
                Err(
                    CustodyVaultError::KeyRotatedVerifyOnly { .. }
                    | CustodyVaultError::RotationEpochFenceConflict { .. },
                ) => {
                    return Err(PeerIdentityError::KeyRotatedOrStale {
                        presented_key_id: key_id,
                        presented_generation: key_generation,
                        active_generation: None,
                    });
                }
                Err(CustodyVaultError::KeyNotYetValid { .. }) => {
                    return Err(PeerIdentityError::KeyNotYetValid);
                }
                Err(CustodyVaultError::KeyExpired { .. }) => {
                    return Err(PeerIdentityError::KeyExpired);
                }
                Err(
                    CustodyVaultError::InvalidSignature
                    | CustodyVaultError::SignerMetadataMismatch(_),
                ) => {
                    return Err(PeerIdentityError::SignatureInvalid);
                }
                Err(other) => {
                    return Err(PeerIdentityError::AuthorityFailure(other.to_string()));
                }
            }
        }

        // 2. Anchor unconditionally in IdentityAuthority against the principal's
        // current active key binding.
        let verified = self
            .identity
            .verify_capability_command_signature(VerifyCapabilityCommandSignatureRequest {
                message_digest,
                principal,
                signature,
                verified_at_ms,
            })
            .map_err(|err| map_identity_error(principal, Some(key_id), err))?;

        if verified.key_id() != key_id || verified.key_generation() != key_generation {
            return Err(PeerIdentityError::KeyRotatedOrStale {
                presented_key_id: key_id,
                presented_generation: key_generation,
                active_generation: Some(verified.key_generation()),
            });
        }

        Ok(VerifiedPeerKey {
            principal_id: verified.principal_id(),
            control_domain_id: verified.control_domain_id(),
            key_id: verified.key_id(),
            key_generation: verified.key_generation(),
        })
    }
}

fn map_identity_error(
    principal: PrincipalId,
    key_id: Option<KeyId>,
    error: IdentityAuthorityError,
) -> PeerIdentityError {
    match error {
        IdentityAuthorityError::PrincipalNotFound(id) => PeerIdentityError::PrincipalUnknown(id),
        IdentityAuthorityError::KeyNotFound(_) => PeerIdentityError::PrincipalUnknown(principal),
        IdentityAuthorityError::KeyRevoked => PeerIdentityError::KeyRevoked { principal, key_id },
        IdentityAuthorityError::KeyNotYetValid => PeerIdentityError::KeyNotYetValid,
        IdentityAuthorityError::KeyExpired => PeerIdentityError::KeyExpired,
        IdentityAuthorityError::InvalidSignature
        | IdentityAuthorityError::InvalidPublicKey
        | IdentityAuthorityError::SignerBindingMismatch
        | IdentityAuthorityError::KeyPurposeMismatch => PeerIdentityError::SignatureInvalid,
        other => PeerIdentityError::AuthorityFailure(other.to_string()),
    }
}
