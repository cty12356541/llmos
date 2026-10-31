//! Cryptographic `(PublisherId, ApplicationId)` first-registration ownership,
//! anti-hijack enforcement (`[PKG-APPID-001]`), nominal identity separation
//! (`PackageId != ApplicationId != InstallationId`, `[PKG-SEP-001]`), and
//! three-party co-signed ownership transfer receipts.

use nlos_capability::custody::{
    CustodySignAuditReceipt, CustodySignRequest, SigningKeyCustodyVault, custody_domain_sha256,
};
use nlos_types::{ApplicationId, InstallationId, PackageId, ReceiptId};

use crate::pki::{
    StoreIntermediateCertificate, sign_ed25519_digest_with_seed, verify_strict_digest_sig,
};
use crate::types::{PublisherId, PublisherTrustCheckError, PublisherTrustTier};

/// Domain tag for canonical `OwnershipTransferProposal` digests.
pub const DOMAIN_OWNERSHIP_TRANSFER_PROPOSAL: &[u8] =
    b"nlos/publisher-trust/ownership-transfer-proposal/v1";
/// Domain tag for the outgoing owner's signature on an ownership transfer.
pub const DOMAIN_OWNERSHIP_TRANSFER_FROM_OWNER: &[u8] =
    b"nlos/publisher-trust/ownership-transfer-from-owner/v1";
/// Domain tag for the incoming owner's signature on an ownership transfer.
pub const DOMAIN_OWNERSHIP_TRANSFER_TO_OWNER: &[u8] =
    b"nlos/publisher-trust/ownership-transfer-to-owner/v1";
/// Domain tag for the Store Intermediate CA's co-signature on an ownership transfer.
pub const DOMAIN_OWNERSHIP_TRANSFER_STORE_CA: &[u8] =
    b"nlos/publisher-trust/ownership-transfer-store-ca/v1";
/// Domain tag for deterministic 16-byte ownership transfer [`ReceiptId`] derivation.
pub const DOMAIN_OWNERSHIP_TRANSFER_RECEIPT_ID: &[u8] =
    b"nlos/publisher-trust/ownership-transfer-receipt-id/v1";

/// Immutable binding of a distinct [`PackageId`] release artifact to an
/// [`ApplicationId`] owned by `publisher_id` (`[PKG-IMM-001]`, `[PKG-SEP-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackageReleaseBinding {
    pub package_id: PackageId,
    pub application_id: ApplicationId,
    pub publisher_id: PublisherId,
    pub manifest_digest: [u8; 32],
    pub registered_at_ms: u64,
}

/// Outcome of registering a [`PackageReleaseBinding`] against an [`ApplicationId`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageReleaseDecision {
    /// Newly bound immutable `(package_id -> application_id, manifest_digest)` record.
    Bound(PackageReleaseBinding),
    /// Byte-identical release binding replayed idempotently.
    Replayed(PackageReleaseBinding),
}

impl PackageReleaseDecision {
    /// Returns the durable [`PackageReleaseBinding`].
    #[must_use]
    pub const fn binding(self) -> PackageReleaseBinding {
        match self {
            Self::Bound(binding) | Self::Replayed(binding) => binding,
        }
    }
}

/// Verified non-interchangeability proof across [`PackageId`], [`ApplicationId`],
/// and [`InstallationId`] (`[PKG-SEP-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeparatedPackageApplicationInstallation {
    pub package_id: PackageId,
    pub application_id: ApplicationId,
    pub installation_id: InstallationId,
}

/// Enforces `[PKG-SEP-001]` nominal and byte-level non-interchangeability across
/// [`PackageId`], [`ApplicationId`], and optional [`InstallationId`].
///
/// # Errors
///
/// Returns [`PublisherTrustCheckError::StorageFailure`] if any two distinct nominal
/// identifier slots reuse identical 16-byte payloads.
pub fn verify_identity_separation(
    package_id: PackageId,
    application_id: ApplicationId,
    installation_id: Option<InstallationId>,
) -> Result<(), PublisherTrustCheckError> {
    if package_id.as_bytes() == application_id.as_bytes() {
        return Err(PublisherTrustCheckError::StorageFailure(
            "PKG-SEP-001 violation: PackageId bytes must not alias ApplicationId bytes".to_owned(),
        ));
    }
    if let Some(inst_id) = installation_id {
        if inst_id.as_bytes() == package_id.as_bytes() {
            return Err(PublisherTrustCheckError::StorageFailure(
                "PKG-SEP-001 violation: InstallationId bytes must not alias PackageId bytes"
                    .to_owned(),
            ));
        }
        if inst_id.as_bytes() == application_id.as_bytes() {
            return Err(PublisherTrustCheckError::StorageFailure(
                "PKG-SEP-001 violation: InstallationId bytes must not alias ApplicationId bytes"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

/// Canonical proposal to transfer [`ApplicationId`] ownership from `from_publisher_id`
/// to `to_publisher_id` (`registration_generation -> registration_generation + 1`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnershipTransferProposal {
    pub application_id: ApplicationId,
    pub expected_current_generation: u64,
    pub from_publisher_id: PublisherId,
    pub from_leaf_public_key: [u8; 32],
    pub to_publisher_id: PublisherId,
    pub to_trust_tier: PublisherTrustTier,
    pub to_leaf_public_key: [u8; 32],
    pub intermediate_serial_id: [u8; 16],
    pub transferred_at_ms: u64,
}

impl OwnershipTransferProposal {
    /// Computes the canonical 32-byte proposal digest shared across all three co-signers.
    #[must_use]
    pub fn proposal_digest(&self) -> [u8; 32] {
        custody_domain_sha256(
            DOMAIN_OWNERSHIP_TRANSFER_PROPOSAL,
            &[
                self.application_id.as_bytes(),
                &self.expected_current_generation.to_be_bytes(),
                self.from_publisher_id.as_bytes(),
                self.from_leaf_public_key.as_slice(),
                self.to_publisher_id.as_bytes(),
                &[self.to_trust_tier.encode()],
                self.to_leaf_public_key.as_slice(),
                self.intermediate_serial_id.as_slice(),
                &self.transferred_at_ms.to_be_bytes(),
            ],
        )
    }

    /// Computes the domain-separated digest signed by the outgoing owner (`from_publisher_id`).
    #[must_use]
    pub fn from_owner_signing_digest(&self) -> [u8; 32] {
        let prop = self.proposal_digest();
        custody_domain_sha256(DOMAIN_OWNERSHIP_TRANSFER_FROM_OWNER, &[prop.as_slice()])
    }

    /// Computes the domain-separated digest signed by the incoming owner (`to_publisher_id`).
    #[must_use]
    pub fn to_owner_signing_digest(&self) -> [u8; 32] {
        let prop = self.proposal_digest();
        custody_domain_sha256(DOMAIN_OWNERSHIP_TRANSFER_TO_OWNER, &[prop.as_slice()])
    }

    /// Computes the domain-separated digest signed by the Store Intermediate CA.
    #[must_use]
    pub fn store_ca_signing_digest(&self) -> [u8; 32] {
        let prop = self.proposal_digest();
        custody_domain_sha256(DOMAIN_OWNERSHIP_TRANSFER_STORE_CA, &[prop.as_slice()])
    }
}

/// Immutable three-party co-signed ownership transfer receipt (`[PKG-APPID-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedOwnershipTransferReceipt {
    pub receipt_id: ReceiptId,
    pub proposal: OwnershipTransferProposal,
    pub from_owner_signature: [u8; 64],
    pub to_owner_signature: [u8; 64],
    pub store_ca_signature: [u8; 64],
}

impl SignedOwnershipTransferReceipt {
    pub(crate) fn derive_receipt_id(
        proposal: &OwnershipTransferProposal,
        from_sig: &[u8; 64],
        to_sig: &[u8; 64],
        ca_sig: &[u8; 64],
    ) -> ReceiptId {
        let prop = proposal.proposal_digest();
        let full = custody_domain_sha256(
            DOMAIN_OWNERSHIP_TRANSFER_RECEIPT_ID,
            &[
                prop.as_slice(),
                from_sig.as_slice(),
                to_sig.as_slice(),
                ca_sig.as_slice(),
            ],
        );
        let mut id = [0_u8; 16];
        id.copy_from_slice(&full[..16]);
        ReceiptId::from_bytes(id)
    }

    /// Constructs a three-party co-signed receipt from raw 32-byte seeds.
    #[must_use]
    pub fn sign_three_party_with_seeds(
        proposal: OwnershipTransferProposal,
        from_owner_seed: &[u8; 32],
        to_owner_seed: &[u8; 32],
        intermediate_ca_seed: &[u8; 32],
    ) -> Self {
        let from_owner_signature =
            sign_ed25519_digest_with_seed(from_owner_seed, &proposal.from_owner_signing_digest());
        let to_owner_signature =
            sign_ed25519_digest_with_seed(to_owner_seed, &proposal.to_owner_signing_digest());
        let store_ca_signature = sign_ed25519_digest_with_seed(
            intermediate_ca_seed,
            &proposal.store_ca_signing_digest(),
        );
        let receipt_id = Self::derive_receipt_id(
            &proposal,
            &from_owner_signature,
            &to_owner_signature,
            &store_ca_signature,
        );
        Self {
            receipt_id,
            proposal,
            from_owner_signature,
            to_owner_signature,
            store_ca_signature,
        }
    }

    /// Constructs a three-party co-signed receipt where the Store Intermediate CA
    /// co-signs inside [`SigningKeyCustodyVault`].
    ///
    /// # Errors
    ///
    /// Fails closed if custody signing fails or if the vault public key disagrees
    /// with `intermediate.intermediate_public_key`.
    pub fn sign_three_party_under_store_custody(
        proposal: OwnershipTransferProposal,
        from_owner_seed: &[u8; 32],
        to_owner_seed: &[u8; 32],
        intermediate: &StoreIntermediateCertificate,
        vault: &SigningKeyCustodyVault,
        expected_rotation_epoch: Option<u64>,
    ) -> Result<(Self, CustodySignAuditReceipt), PublisherTrustCheckError> {
        if intermediate.serial_id != proposal.intermediate_serial_id {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "transfer proposal intermediate_serial_id mismatch",
            ));
        }
        let from_owner_signature =
            sign_ed25519_digest_with_seed(from_owner_seed, &proposal.from_owner_signing_digest());
        let to_owner_signature =
            sign_ed25519_digest_with_seed(to_owner_seed, &proposal.to_owner_signing_digest());
        let ca_digest = proposal.store_ca_signing_digest();
        let audit = vault
            .sign_digest_under_custody(CustodySignRequest {
                key_id: intermediate.intermediate_key_id,
                expected_rotation_epoch,
                domain_tag: DOMAIN_OWNERSHIP_TRANSFER_STORE_CA,
                message_digest: ca_digest,
                signed_at_ms: proposal.transferred_at_ms,
            })
            .map_err(|err| {
                PublisherTrustCheckError::StorageFailure(format!(
                    "intermediate CA ownership transfer co-sign failed: {err}"
                ))
            })?;
        let store_ca_signature = audit.signature;
        if !verify_strict_digest_sig(
            &intermediate.intermediate_public_key,
            &ca_digest,
            &store_ca_signature,
        ) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "custody vault key slot does not match StoreIntermediateCertificate public key",
            ));
        }
        let receipt_id = Self::derive_receipt_id(
            &proposal,
            &from_owner_signature,
            &to_owner_signature,
            &store_ca_signature,
        );
        Ok((
            Self {
                receipt_id,
                proposal,
                from_owner_signature,
                to_owner_signature,
                store_ca_signature,
            },
            audit,
        ))
    }
}
