//! Monotonic `RevocationEpoch` signed revocation lists (CRL), anti-rollback
//! enforcement, and anti-freeze vs. `[DUI-OFFLINE-001]` cached offline launch
//! verification (`ADR-0027` section 2.3; decision point `E4`).

use nlos_capability::custody::{
    CustodySignAuditReceipt, CustodySignRequest, SigningKeyCustodyVault, custody_domain_sha256,
};

use crate::pki::{
    OfflinePlatformRootCa, StoreIntermediateCertificate, ed25519_public_key_from_seed,
    sign_ed25519_digest_with_seed, verify_strict_digest_sig,
};
use crate::types::{PublisherId, PublisherTrustCheckError, RevocationEpoch};

/// Domain tag for canonical `SignedRevocationList` signing digests.
pub const DOMAIN_SIGNED_REVOCATION_LIST: &[u8] = b"nlos/publisher-trust/signed-revocation-list/v1";

/// CRL freshness policy selector balancing online MITM anti-freeze guarantees
/// against `[DUI-OFFLINE-001]` already-installed offline desktop launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrustFreshnessMode {
    /// Store browse/download, fresh package install, or remote upgrade:
    /// requires an active CRL with `now_ms <= fresh_until_ms`, failing closed
    /// with [`PublisherTrustCheckError::FrozenRevocationList`] when expired.
    OnlineInstallOrUpgrade,
    /// Local launch of an already-installed application while offline
    /// (`[DUI-OFFLINE-001]`): permits launch past `fresh_until_ms` provided
    /// the local cached revocation ledger has not revoked the certificate chain
    /// or [`PublisherId`]; blocks immediately once a synced CRL revokes either.
    OfflineInstalledLaunch,
}

/// Closed revocation reason code carried by a [`RevokedCertificateEntry`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RevocationReason {
    /// Private signing key was leaked or compromised.
    KeyCompromise = 1,
    /// Issuing intermediate CA key was compromised.
    CaCompromise = 2,
    /// Package or update signed by the key violated store malware/safety rules.
    MaliciousPackage = 3,
    /// Store governance or legal policy revocation.
    PolicyViolation = 4,
    /// Explicit retirement/supersession via CRL.
    Superseded = 5,
}

impl RevocationReason {
    /// Encodes the reason into its stable 1-byte wire and `SQLite` code (`1..=5`).
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }

    /// Decodes a `SQLite` integer column into a [`RevocationReason`].
    #[must_use]
    pub const fn decode(code: i64) -> Option<Self> {
        match code {
            1 => Some(Self::KeyCompromise),
            2 => Some(Self::CaCompromise),
            3 => Some(Self::MaliciousPackage),
            4 => Some(Self::PolicyViolation),
            5 => Some(Self::Superseded),
            _ => None,
        }
    }
}

/// One certificate serial or 16-byte [`nlos_types::KeyId`] entry inside a
/// [`SignedRevocationList`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RevokedCertificateEntry {
    pub serial_or_key: [u8; 16],
    pub reason: RevocationReason,
}

/// Authority that signed a [`SignedRevocationList`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrlIssuerKind {
    /// Signed directly by the [`OfflinePlatformRootCa`] (required when revoking an intermediate CA).
    RootCa { root_serial_id: [u8; 16] },
    /// Signed by an active, unrevoked [`StoreIntermediateCertificate`].
    StoreIntermediate { intermediate_serial_id: [u8; 16] },
}

/// Outcome of importing a [`SignedRevocationList`] into the durable trust ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrlImportDecision {
    /// Advanced the durable [`RevocationEpoch`] to a strictly newer epoch.
    Advanced(RevocationEpoch),
    /// Byte-identical CRL for the current [`RevocationEpoch`] replayed idempotently.
    Replayed(RevocationEpoch),
}

impl CrlImportDecision {
    /// Returns the resulting [`RevocationEpoch`] regardless of whether it was newly
    /// advanced or idempotently replayed.
    #[must_use]
    pub const fn epoch(self) -> RevocationEpoch {
        match self {
            Self::Advanced(epoch) | Self::Replayed(epoch) => epoch,
        }
    }
}

/// Durable snapshot of the authority's active revocation epoch and freshness window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RevocationStateSnapshot {
    pub revocation_epoch: RevocationEpoch,
    pub issued_at_ms: u64,
    pub fresh_until_ms: u64,
    pub crl_digest: [u8; 32],
}

/// Cryptographically signed certificate and whole-publisher revocation list
/// (`ADR-0027` section 2.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedRevocationList {
    pub revocation_epoch: RevocationEpoch,
    pub issued_at_ms: u64,
    pub fresh_until_ms: u64,
    pub revoked_certificates: Vec<RevokedCertificateEntry>,
    pub revoked_publishers: Vec<PublisherId>,
    pub issuer: CrlIssuerKind,
    pub signature: [u8; 64],
}

/// Unsigned input specification for constructing a [`SignedRevocationList`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevocationListSpec {
    pub revocation_epoch: RevocationEpoch,
    pub issued_at_ms: u64,
    pub fresh_until_ms: u64,
    pub revoked_certificates: Vec<RevokedCertificateEntry>,
    pub revoked_publishers: Vec<PublisherId>,
}

impl SignedRevocationList {
    /// Computes the canonical order-independent `SHA-256` signing digest of this CRL.
    #[must_use]
    pub fn crl_digest(&self) -> [u8; 32] {
        Self::compute_crl_digest(
            self.revocation_epoch,
            self.issued_at_ms,
            self.fresh_until_ms,
            &self.revoked_certificates,
            &self.revoked_publishers,
            self.issuer,
        )
    }

    pub(crate) fn compute_crl_digest(
        revocation_epoch: RevocationEpoch,
        issued_at_ms: u64,
        fresh_until_ms: u64,
        revoked_certificates: &[RevokedCertificateEntry],
        revoked_publishers: &[PublisherId],
        issuer: CrlIssuerKind,
    ) -> [u8; 32] {
        let mut certs = revoked_certificates.to_vec();
        certs.sort_unstable();
        certs.dedup_by_key(|entry| entry.serial_or_key);

        let mut publishers = revoked_publishers.to_vec();
        publishers.sort_unstable();
        publishers.dedup();

        let (issuer_tag, issuer_serial) = match issuer {
            CrlIssuerKind::RootCa { root_serial_id } => (1_u8, root_serial_id),
            CrlIssuerKind::StoreIntermediate {
                intermediate_serial_id,
            } => (2_u8, intermediate_serial_id),
        };

        let mut certs_buf = Vec::with_capacity(certs.len() * 17);
        for entry in &certs {
            certs_buf.extend_from_slice(&entry.serial_or_key);
            certs_buf.push(entry.reason.encode());
        }

        let mut pubs_buf = Vec::with_capacity(publishers.len() * 16);
        for pub_id in &publishers {
            pubs_buf.extend_from_slice(pub_id.as_bytes());
        }

        let cert_count = u64::try_from(certs.len()).unwrap_or(u64::MAX);
        let pub_count = u64::try_from(publishers.len()).unwrap_or(u64::MAX);

        custody_domain_sha256(
            DOMAIN_SIGNED_REVOCATION_LIST,
            &[
                &revocation_epoch.get().to_be_bytes(),
                &issued_at_ms.to_be_bytes(),
                &fresh_until_ms.to_be_bytes(),
                &[issuer_tag],
                issuer_serial.as_slice(),
                &cert_count.to_be_bytes(),
                certs_buf.as_slice(),
                &pub_count.to_be_bytes(),
                pubs_buf.as_slice(),
            ],
        )
    }

    fn validate_spec(spec: &RevocationListSpec) -> Result<(), PublisherTrustCheckError> {
        if spec.revocation_epoch.get() == 0 {
            return Err(PublisherTrustCheckError::StaleOrRollbackRevocationEpoch {
                current_epoch: RevocationEpoch::new(0),
                candidate_epoch: spec.revocation_epoch,
            });
        }
        if spec.fresh_until_ms <= spec.issued_at_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "CRL fresh_until_ms must strictly exceed issued_at_ms",
            ));
        }
        Ok(())
    }

    /// Signs a [`SignedRevocationList`] directly with the [`OfflinePlatformRootCa`] seed.
    ///
    /// # Errors
    ///
    /// Fails closed if `revocation_epoch == 0`, `fresh_until_ms <= issued_at_ms`, or
    /// `root_seed` does not match `root.root_public_key`.
    pub fn issue_with_root_seed(
        root: &OfflinePlatformRootCa,
        root_seed: &[u8; 32],
        spec: RevocationListSpec,
    ) -> Result<Self, PublisherTrustCheckError> {
        Self::validate_spec(&spec)?;
        if ed25519_public_key_from_seed(root_seed) != root.root_public_key {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "root seed does not match OfflinePlatformRootCa public key",
            ));
        }
        let issuer = CrlIssuerKind::RootCa {
            root_serial_id: root.root_serial_id,
        };
        let digest = Self::compute_crl_digest(
            spec.revocation_epoch,
            spec.issued_at_ms,
            spec.fresh_until_ms,
            &spec.revoked_certificates,
            &spec.revoked_publishers,
            issuer,
        );
        let signature = sign_ed25519_digest_with_seed(root_seed, &digest);
        Ok(Self {
            revocation_epoch: spec.revocation_epoch,
            issued_at_ms: spec.issued_at_ms,
            fresh_until_ms: spec.fresh_until_ms,
            revoked_certificates: spec.revoked_certificates,
            revoked_publishers: spec.revoked_publishers,
            issuer,
            signature,
        })
    }

    /// Signs a [`SignedRevocationList`] with a [`StoreIntermediateCertificate`] seed.
    ///
    /// # Errors
    ///
    /// Fails closed if `revocation_epoch == 0`, `fresh_until_ms <= issued_at_ms`, or
    /// `intermediate_seed` does not match `intermediate.intermediate_public_key`.
    pub fn issue_with_intermediate_seed(
        intermediate: &StoreIntermediateCertificate,
        intermediate_seed: &[u8; 32],
        spec: RevocationListSpec,
    ) -> Result<Self, PublisherTrustCheckError> {
        Self::validate_spec(&spec)?;
        if ed25519_public_key_from_seed(intermediate_seed) != intermediate.intermediate_public_key {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate seed does not match StoreIntermediateCertificate public key",
            ));
        }
        let issuer = CrlIssuerKind::StoreIntermediate {
            intermediate_serial_id: intermediate.serial_id,
        };
        let digest = Self::compute_crl_digest(
            spec.revocation_epoch,
            spec.issued_at_ms,
            spec.fresh_until_ms,
            &spec.revoked_certificates,
            &spec.revoked_publishers,
            issuer,
        );
        let signature = sign_ed25519_digest_with_seed(intermediate_seed, &digest);
        Ok(Self {
            revocation_epoch: spec.revocation_epoch,
            issued_at_ms: spec.issued_at_ms,
            fresh_until_ms: spec.fresh_until_ms,
            revoked_certificates: spec.revoked_certificates,
            revoked_publishers: spec.revoked_publishers,
            issuer,
            signature,
        })
    }

    /// Signs a [`SignedRevocationList`] inside [`SigningKeyCustodyVault`] using the
    /// Store Intermediate CA's sealed key slot.
    ///
    /// # Errors
    ///
    /// Fails closed if spec validation fails, custody signing fails, or the vault key
    /// does not match `intermediate.intermediate_public_key`.
    pub fn issue_with_intermediate_custody(
        intermediate: &StoreIntermediateCertificate,
        vault: &SigningKeyCustodyVault,
        expected_rotation_epoch: Option<u64>,
        spec: RevocationListSpec,
    ) -> Result<(Self, CustodySignAuditReceipt), PublisherTrustCheckError> {
        Self::validate_spec(&spec)?;
        let issuer = CrlIssuerKind::StoreIntermediate {
            intermediate_serial_id: intermediate.serial_id,
        };
        let digest = Self::compute_crl_digest(
            spec.revocation_epoch,
            spec.issued_at_ms,
            spec.fresh_until_ms,
            &spec.revoked_certificates,
            &spec.revoked_publishers,
            issuer,
        );
        let audit = vault
            .sign_digest_under_custody(CustodySignRequest {
                key_id: intermediate.intermediate_key_id,
                expected_rotation_epoch,
                domain_tag: DOMAIN_SIGNED_REVOCATION_LIST,
                message_digest: digest,
                signed_at_ms: spec.issued_at_ms,
            })
            .map_err(|err| {
                PublisherTrustCheckError::StorageFailure(format!(
                    "intermediate CA custody CRL signing failed: {err}"
                ))
            })?;
        let signature = audit.signature;
        if !verify_strict_digest_sig(&intermediate.intermediate_public_key, &digest, &signature) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "custody vault key slot does not match intermediate CA public key for CRL",
            ));
        }
        Ok((
            Self {
                revocation_epoch: spec.revocation_epoch,
                issued_at_ms: spec.issued_at_ms,
                fresh_until_ms: spec.fresh_until_ms,
                revoked_certificates: spec.revoked_certificates,
                revoked_publishers: spec.revoked_publishers,
                issuer,
                signature,
            },
            audit,
        ))
    }
}
