//! Three-tier publisher PKI certificate hierarchy, RFC 8032 `Ed25519` signing,
//! [`SigningKeyCustodyVault`] integration, and key rotation receipts
//! (`ADR-0027` section 2.3; `[PKG-IMM-001]`, `[PKG-APPID-001]`).

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::custody::{
    CustodySignAuditReceipt, CustodySignRequest, SigningKeyCustodyVault, custody_domain_sha256,
    verify_ed25519_signature_strict,
};
use nlos_types::{KeyId, ReceiptId};

use crate::types::{PublisherId, PublisherTrustCheckError, PublisherTrustTier, RevocationEpoch};

/// Domain tag for computing an offline root CA's 32-byte public-key fingerprint.
pub const DOMAIN_ROOT_CA_FINGERPRINT: &[u8] = b"nlos/publisher-trust/root-pubkey-fingerprint/v1";
/// Domain tag for an offline root CA's self-signed root anchor digest.
pub const DOMAIN_ROOT_CA_SELF_SIGN: &[u8] = b"nlos/publisher-trust/root-self-cert/v1";
/// Domain tag for a root-signed store intermediate CA certificate digest.
pub const DOMAIN_STORE_INTERMEDIATE_CERT: &[u8] =
    b"nlos/publisher-trust/store-intermediate-cert/v1";
/// Domain tag for a publisher leaf signing certificate digest.
pub const DOMAIN_PUBLISHER_LEAF_CERT: &[u8] = b"nlos/publisher-trust/publisher-leaf-cert/v1";
/// Domain tag for canonical key-rotation proposal digests.
pub const DOMAIN_KEY_ROTATION_PROPOSAL: &[u8] = b"nlos/publisher-trust/key-rotation-proposal/v1";
/// Domain tag for the outgoing leaf key's co-signature on a key rotation.
pub const DOMAIN_KEY_ROTATION_OLD_LEAF: &[u8] = b"nlos/publisher-trust/key-rotation-old-leaf/v1";
/// Domain tag for the incoming leaf key's co-signature on a key rotation.
pub const DOMAIN_KEY_ROTATION_NEW_LEAF: &[u8] = b"nlos/publisher-trust/key-rotation-new-leaf/v1";
/// Domain tag for a Store Intermediate CA attestation on an emergency key rotation.
pub const DOMAIN_KEY_ROTATION_STORE_CA: &[u8] = b"nlos/publisher-trust/key-rotation-store-ca/v1";
/// Domain tag for deterministic 16-byte rotation receipt identifiers.
pub const DOMAIN_KEY_ROTATION_RECEIPT_ID: &[u8] =
    b"nlos/publisher-trust/key-rotation-receipt-id/v1";
/// Domain tag recorded in [`SigningKeyCustodyVault`] audit logs when signing a package digest.
pub const DOMAIN_PACKAGE_DIGEST_SIGN: &[u8] = b"nlos/publisher-trust/package-digest-sign/v1";

/// Signs an exact 32-byte message digest with a raw 32-byte `Ed25519` private seed.
#[must_use]
pub fn sign_ed25519_digest_with_seed(seed: &[u8; 32], digest: &[u8; 32]) -> [u8; 64] {
    let signing_key = SigningKey::from_bytes(seed);
    signing_key.sign(digest).to_bytes()
}

/// Derives the 32-byte `Ed25519` public key from a raw 32-byte private seed.
#[must_use]
pub fn ed25519_public_key_from_seed(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// Signs a 32-byte package/artifact digest inside [`SigningKeyCustodyVault`]
/// without exposing the private seed (`#17 C-CUSTODY-GATEWAY`).
///
/// # Errors
///
/// Returns [`PublisherTrustCheckError::StorageFailure`] or [`PublisherTrustCheckError::RevokedCertificate`]
/// if the custody slot is missing, compromised, expired, or rotated verify-only.
pub fn sign_package_digest_under_custody(
    vault: &SigningKeyCustodyVault,
    key_id: KeyId,
    expected_rotation_epoch: Option<u64>,
    message_digest: [u8; 32],
    signed_at_ms: u64,
) -> Result<([u8; 64], CustodySignAuditReceipt), PublisherTrustCheckError> {
    let receipt = vault
        .sign_digest_under_custody(CustodySignRequest {
            key_id,
            expected_rotation_epoch,
            domain_tag: DOMAIN_PACKAGE_DIGEST_SIGN,
            message_digest,
            signed_at_ms,
        })
        .map_err(|err| {
            PublisherTrustCheckError::StorageFailure(format!("custody vault sign failed: {err}"))
        })?;
    Ok((receipt.signature, receipt))
}

pub(crate) fn verify_strict_digest_sig(
    public_key: &[u8; 32],
    digest: &[u8; 32],
    signature: &[u8; 64],
) -> bool {
    verify_ed25519_signature_strict(public_key, digest, signature)
}

/// Offline first-party platform root CA trust anchor (`ADR-0027` section 2.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfflinePlatformRootCa {
    pub root_serial_id: [u8; 16],
    pub root_key_id: KeyId,
    pub root_public_key: [u8; 32],
    pub public_key_fingerprint: [u8; 32],
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
    pub self_signature: [u8; 64],
}

/// Parameters for issuing a [`StoreIntermediateCertificate`] under an [`OfflinePlatformRootCa`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntermediateCertificateIssueSpec {
    pub serial_id: [u8; 16],
    pub intermediate_key_id: KeyId,
    pub intermediate_public_key: [u8; 32],
    pub max_issuable_tier: PublisherTrustTier,
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
}

impl OfflinePlatformRootCa {
    /// Computes the canonical 32-byte domain-separated fingerprint of a root CA public key.
    #[must_use]
    pub fn compute_public_key_fingerprint(root_public_key: &[u8; 32]) -> [u8; 32] {
        custody_domain_sha256(DOMAIN_ROOT_CA_FINGERPRINT, &[root_public_key.as_slice()])
    }

    /// Issues a self-signed [`OfflinePlatformRootCa`] anchor from an offline 32-byte seed.
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::UntrustedCertificateChain`] if
    /// `expires_at_ms <= not_before_ms`.
    pub fn issue_self_signed(
        root_serial_id: [u8; 16],
        root_key_id: KeyId,
        root_seed: &[u8; 32],
        not_before_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self, PublisherTrustCheckError> {
        if expires_at_ms <= not_before_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "root CA expires_at_ms must exceed not_before_ms",
            ));
        }
        let root_public_key = ed25519_public_key_from_seed(root_seed);
        let public_key_fingerprint = Self::compute_public_key_fingerprint(&root_public_key);
        let digest = Self::compute_signing_digest(
            root_serial_id,
            root_key_id,
            &root_public_key,
            &public_key_fingerprint,
            not_before_ms,
            expires_at_ms,
        );
        let self_signature = sign_ed25519_digest_with_seed(root_seed, &digest);
        Ok(Self {
            root_serial_id,
            root_key_id,
            root_public_key,
            public_key_fingerprint,
            not_before_ms,
            expires_at_ms,
            self_signature,
        })
    }

    /// Computes the canonical signing digest of this root anchor.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        Self::compute_signing_digest(
            self.root_serial_id,
            self.root_key_id,
            &self.root_public_key,
            &self.public_key_fingerprint,
            self.not_before_ms,
            self.expires_at_ms,
        )
    }

    fn compute_signing_digest(
        root_serial_id: [u8; 16],
        root_key_id: KeyId,
        root_public_key: &[u8; 32],
        public_key_fingerprint: &[u8; 32],
        not_before_ms: u64,
        expires_at_ms: u64,
    ) -> [u8; 32] {
        custody_domain_sha256(
            DOMAIN_ROOT_CA_SELF_SIGN,
            &[
                root_serial_id.as_slice(),
                root_key_id.as_bytes(),
                root_public_key.as_slice(),
                public_key_fingerprint.as_slice(),
                &not_before_ms.to_be_bytes(),
                &expires_at_ms.to_be_bytes(),
            ],
        )
    }

    /// Verifies the root anchor's public-key fingerprint, self-signature, and
    /// optional timestamp validity window.
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::UntrustedCertificateChain`] or
    /// [`PublisherTrustCheckError::ExpiredCertificate`] on invalid or expired anchors.
    pub fn verify_self_signed(&self, now_ms: Option<u64>) -> Result<(), PublisherTrustCheckError> {
        if self.expires_at_ms <= self.not_before_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "root CA validity window is empty or inverted",
            ));
        }
        let expected_fp = Self::compute_public_key_fingerprint(&self.root_public_key);
        if self.public_key_fingerprint != expected_fp {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "root CA public_key_fingerprint mismatch",
            ));
        }
        if !verify_strict_digest_sig(
            &self.root_public_key,
            &self.signing_digest(),
            &self.self_signature,
        ) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "root CA self-signature invalid",
            ));
        }
        if let Some(now) = now_ms {
            if now < self.not_before_ms {
                return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                    "root CA certificate is not yet valid",
                ));
            }
            if now > self.expires_at_ms {
                return Err(PublisherTrustCheckError::ExpiredCertificate {
                    expires_at_ms: self.expires_at_ms,
                    now_ms: now,
                });
            }
        }
        Ok(())
    }

    /// Signs a [`StoreIntermediateCertificate`] using the offline root CA seed.
    ///
    /// # Errors
    ///
    /// Fails closed if the root seed does not match `self.root_public_key` or if
    /// the intermediate validity window exceeds the root CA's validity bounds.
    pub fn issue_intermediate_certificate(
        &self,
        root_seed: &[u8; 32],
        spec: IntermediateCertificateIssueSpec,
    ) -> Result<StoreIntermediateCertificate, PublisherTrustCheckError> {
        self.verify_self_signed(None)?;
        if ed25519_public_key_from_seed(root_seed) != self.root_public_key {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "offline root seed does not match root CA public key",
            ));
        }
        if spec.expires_at_ms <= spec.not_before_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA expires_at_ms must exceed not_before_ms",
            ));
        }
        if spec.not_before_ms < self.not_before_ms || spec.expires_at_ms > self.expires_at_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA validity window exceeds root CA bounds",
            ));
        }
        if spec.serial_id == self.root_serial_id {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA serial_id cannot alias root CA serial_id",
            ));
        }
        let digest = StoreIntermediateCertificate::compute_signing_digest(
            spec.serial_id,
            self.root_serial_id,
            spec.intermediate_key_id,
            &spec.intermediate_public_key,
            spec.max_issuable_tier,
            spec.not_before_ms,
            spec.expires_at_ms,
        );
        let root_signature = sign_ed25519_digest_with_seed(root_seed, &digest);
        Ok(StoreIntermediateCertificate {
            serial_id: spec.serial_id,
            issuer_root_serial_id: self.root_serial_id,
            intermediate_key_id: spec.intermediate_key_id,
            intermediate_public_key: spec.intermediate_public_key,
            max_issuable_tier: spec.max_issuable_tier,
            not_before_ms: spec.not_before_ms,
            expires_at_ms: spec.expires_at_ms,
            root_signature,
        })
    }
}

/// Root-signed Store Intermediate CA certificate (`ADR-0027` section 2.3),
/// whose private key may be sealed under a [`SigningKeyCustodyVault`] KEK.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreIntermediateCertificate {
    pub serial_id: [u8; 16],
    pub issuer_root_serial_id: [u8; 16],
    pub intermediate_key_id: KeyId,
    pub intermediate_public_key: [u8; 32],
    pub max_issuable_tier: PublisherTrustTier,
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
    pub root_signature: [u8; 64],
}

/// Parameters for issuing a [`PublisherLeafCertificate`] under a [`StoreIntermediateCertificate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeafCertificateIssueSpec {
    pub serial_id: [u8; 16],
    pub publisher_id: PublisherId,
    pub trust_tier: PublisherTrustTier,
    pub leaf_key_id: KeyId,
    pub leaf_public_key: [u8; 32],
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
}

impl StoreIntermediateCertificate {
    /// Returns `true` iff this intermediate CA is authorized to issue certificates
    /// for `target_tier`.
    ///
    /// Because [`PublisherTrustTier`] orders highest privilege first
    /// (`PlatformCore = 1 < CertifiedPublisher = 2 < CommunitySandboxed = 3`),
    /// `target_tier` is permitted iff `target_tier.encode() >= self.max_issuable_tier.encode()`.
    #[must_use]
    pub const fn can_issue_tier(&self, target_tier: PublisherTrustTier) -> bool {
        target_tier.encode() >= self.max_issuable_tier.encode()
    }

    /// Computes the canonical signing digest signed by the offline root CA.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        Self::compute_signing_digest(
            self.serial_id,
            self.issuer_root_serial_id,
            self.intermediate_key_id,
            &self.intermediate_public_key,
            self.max_issuable_tier,
            self.not_before_ms,
            self.expires_at_ms,
        )
    }

    pub(crate) fn compute_signing_digest(
        serial_id: [u8; 16],
        issuer_root_serial_id: [u8; 16],
        intermediate_key_id: KeyId,
        intermediate_public_key: &[u8; 32],
        max_issuable_tier: PublisherTrustTier,
        not_before_ms: u64,
        expires_at_ms: u64,
    ) -> [u8; 32] {
        custody_domain_sha256(
            DOMAIN_STORE_INTERMEDIATE_CERT,
            &[
                serial_id.as_slice(),
                issuer_root_serial_id.as_slice(),
                intermediate_key_id.as_bytes(),
                intermediate_public_key.as_slice(),
                &[max_issuable_tier.encode()],
                &not_before_ms.to_be_bytes(),
                &expires_at_ms.to_be_bytes(),
            ],
        )
    }

    /// Verifies this intermediate certificate against a trusted [`OfflinePlatformRootCa`].
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::UntrustedCertificateChain`] on root mismatch
    /// or bad signature, or [`PublisherTrustCheckError::ExpiredCertificate`] if expired at `now_ms`.
    pub fn verify_against_root(
        &self,
        root: &OfflinePlatformRootCa,
        now_ms: Option<u64>,
    ) -> Result<(), PublisherTrustCheckError> {
        root.verify_self_signed(now_ms)?;
        if self.issuer_root_serial_id != root.root_serial_id {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA issuer_root_serial_id does not match root anchor",
            ));
        }
        if self.serial_id == root.root_serial_id {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA serial_id cannot alias root CA serial_id",
            ));
        }
        if self.expires_at_ms <= self.not_before_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA validity window is empty or inverted",
            ));
        }
        if self.not_before_ms < root.not_before_ms || self.expires_at_ms > root.expires_at_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA validity window exceeds root CA bounds",
            ));
        }
        if !verify_strict_digest_sig(
            &root.root_public_key,
            &self.signing_digest(),
            &self.root_signature,
        ) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA root signature verification failed",
            ));
        }
        if let Some(now) = now_ms {
            if now < self.not_before_ms {
                return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                    "intermediate CA certificate is not yet valid",
                ));
            }
            if now > self.expires_at_ms {
                return Err(PublisherTrustCheckError::ExpiredCertificate {
                    expires_at_ms: self.expires_at_ms,
                    now_ms: now,
                });
            }
        }
        Ok(())
    }

    fn validate_leaf_spec(
        &self,
        spec: &LeafCertificateIssueSpec,
    ) -> Result<[u8; 32], PublisherTrustCheckError> {
        if !self.can_issue_tier(spec.trust_tier) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA tier ceiling forbids issuing requested leaf trust tier",
            ));
        }
        if spec.serial_id == self.serial_id {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "CA-issued leaf serial_id cannot equal issuer_serial_id",
            ));
        }
        if spec.expires_at_ms <= spec.not_before_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "leaf certificate expires_at_ms must exceed not_before_ms",
            ));
        }
        if spec.not_before_ms < self.not_before_ms || spec.expires_at_ms > self.expires_at_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "leaf certificate validity window exceeds intermediate CA bounds",
            ));
        }
        Ok(PublisherLeafCertificate::compute_signing_digest(
            spec.serial_id,
            spec.publisher_id,
            spec.trust_tier,
            spec.leaf_key_id,
            &spec.leaf_public_key,
            spec.not_before_ms,
            spec.expires_at_ms,
            self.serial_id,
        ))
    }

    /// Issues a [`PublisherLeafCertificate`] using a raw 32-byte intermediate CA seed.
    ///
    /// # Errors
    ///
    /// Fails closed if `spec.trust_tier` exceeds `self.max_issuable_tier`, if the seed
    /// does not match `self.intermediate_public_key`, or if validity bounds are violated.
    pub fn issue_leaf_certificate_with_seed(
        &self,
        intermediate_seed: &[u8; 32],
        spec: LeafCertificateIssueSpec,
    ) -> Result<PublisherLeafCertificate, PublisherTrustCheckError> {
        if ed25519_public_key_from_seed(intermediate_seed) != self.intermediate_public_key {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA seed does not match intermediate_public_key",
            ));
        }
        let digest = self.validate_leaf_spec(&spec)?;
        let signature = sign_ed25519_digest_with_seed(intermediate_seed, &digest);
        Ok(PublisherLeafCertificate {
            serial_id: spec.serial_id,
            publisher_id: spec.publisher_id,
            trust_tier: spec.trust_tier,
            leaf_key_id: spec.leaf_key_id,
            leaf_public_key: spec.leaf_public_key,
            not_before_ms: spec.not_before_ms,
            expires_at_ms: spec.expires_at_ms,
            issuer_serial_id: self.serial_id,
            signature,
        })
    }

    /// Issues a [`PublisherLeafCertificate`] directly through [`SigningKeyCustodyVault`]
    /// so the Store Intermediate CA private seed never leaves KEK custody.
    ///
    /// # Errors
    ///
    /// Fails closed on tier escalation, custody vault failure, or public-key mismatch.
    pub fn issue_leaf_certificate_under_custody(
        &self,
        vault: &SigningKeyCustodyVault,
        expected_rotation_epoch: Option<u64>,
        signed_at_ms: u64,
        spec: LeafCertificateIssueSpec,
    ) -> Result<(PublisherLeafCertificate, CustodySignAuditReceipt), PublisherTrustCheckError> {
        let digest = self.validate_leaf_spec(&spec)?;
        let receipt = vault
            .sign_digest_under_custody(CustodySignRequest {
                key_id: self.intermediate_key_id,
                expected_rotation_epoch,
                domain_tag: DOMAIN_PUBLISHER_LEAF_CERT,
                message_digest: digest,
                signed_at_ms,
            })
            .map_err(|err| {
                PublisherTrustCheckError::StorageFailure(format!(
                    "intermediate CA custody signing failed: {err}"
                ))
            })?;
        let signature = receipt.signature;
        if !verify_strict_digest_sig(&self.intermediate_public_key, &digest, &signature) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "custody vault key slot does not match StoreIntermediateCertificate public key",
            ));
        }
        Ok((
            PublisherLeafCertificate {
                serial_id: spec.serial_id,
                publisher_id: spec.publisher_id,
                trust_tier: spec.trust_tier,
                leaf_key_id: spec.leaf_key_id,
                leaf_public_key: spec.leaf_public_key,
                not_before_ms: spec.not_before_ms,
                expires_at_ms: spec.expires_at_ms,
                issuer_serial_id: self.serial_id,
                signature,
            },
            receipt,
        ))
    }
}

/// Lifecycle state of a registered [`PublisherLeafCertificate`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeafCertificateStatus {
    /// Active signing certificate.
    Active = 1,
    /// Cleanly retired by a [`PublisherKeyRotationReceipt`] (`RetireVerifyOnly`).
    RotatedRetired = 2,
    /// Revoked due to key compromise or CRL entry; fails closed immediately.
    Compromised = 3,
}

impl LeafCertificateStatus {
    #[must_use]
    pub const fn encode(self) -> i64 {
        self as i64
    }

    pub(crate) const fn decode(code: i64) -> Result<Self, PublisherTrustCheckError> {
        match code {
            1 => Ok(Self::Active),
            2 => Ok(Self::RotatedRetired),
            3 => Ok(Self::Compromised),
            _ => Err(PublisherTrustCheckError::StorageFailure(String::new())),
        }
    }
}

/// Publisher leaf signing certificate (`ADR-0027` section 2.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublisherLeafCertificate {
    pub serial_id: [u8; 16],
    pub publisher_id: PublisherId,
    pub trust_tier: PublisherTrustTier,
    pub leaf_key_id: KeyId,
    pub leaf_public_key: [u8; 32],
    pub not_before_ms: u64,
    pub expires_at_ms: u64,
    pub issuer_serial_id: [u8; 16],
    pub signature: [u8; 64],
}

impl PublisherLeafCertificate {
    /// Returns `true` when `issuer_serial_id == serial_id` (self-signed developer key).
    #[must_use]
    pub fn is_self_signed(&self) -> bool {
        self.issuer_serial_id == self.serial_id
    }

    /// Issues a self-signed developer leaf certificate locked strictly to
    /// [`PublisherTrustTier::CommunitySandboxed`].
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::UntrustedCertificateChain`] if
    /// `expires_at_ms <= not_before_ms`.
    pub fn issue_self_signed_community(
        serial_id: [u8; 16],
        publisher_id: PublisherId,
        leaf_key_id: KeyId,
        developer_seed: &[u8; 32],
        not_before_ms: u64,
        expires_at_ms: u64,
    ) -> Result<Self, PublisherTrustCheckError> {
        if expires_at_ms <= not_before_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "self-signed community certificate expires_at_ms must exceed not_before_ms",
            ));
        }
        let leaf_public_key = ed25519_public_key_from_seed(developer_seed);
        let digest = Self::compute_signing_digest(
            serial_id,
            publisher_id,
            PublisherTrustTier::CommunitySandboxed,
            leaf_key_id,
            &leaf_public_key,
            not_before_ms,
            expires_at_ms,
            serial_id,
        );
        let signature = sign_ed25519_digest_with_seed(developer_seed, &digest);
        Ok(Self {
            serial_id,
            publisher_id,
            trust_tier: PublisherTrustTier::CommunitySandboxed,
            leaf_key_id,
            leaf_public_key,
            not_before_ms,
            expires_at_ms,
            issuer_serial_id: serial_id,
            signature,
        })
    }

    /// Computes the canonical signing digest of this leaf certificate.
    #[must_use]
    pub fn signing_digest(&self) -> [u8; 32] {
        Self::compute_signing_digest(
            self.serial_id,
            self.publisher_id,
            self.trust_tier,
            self.leaf_key_id,
            &self.leaf_public_key,
            self.not_before_ms,
            self.expires_at_ms,
            self.issuer_serial_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn compute_signing_digest(
        serial_id: [u8; 16],
        publisher_id: PublisherId,
        trust_tier: PublisherTrustTier,
        leaf_key_id: KeyId,
        leaf_public_key: &[u8; 32],
        not_before_ms: u64,
        expires_at_ms: u64,
        issuer_serial_id: [u8; 16],
    ) -> [u8; 32] {
        custody_domain_sha256(
            DOMAIN_PUBLISHER_LEAF_CERT,
            &[
                serial_id.as_slice(),
                publisher_id.as_bytes(),
                &[trust_tier.encode()],
                leaf_key_id.as_bytes(),
                leaf_public_key.as_slice(),
                &not_before_ms.to_be_bytes(),
                &expires_at_ms.to_be_bytes(),
                issuer_serial_id.as_slice(),
            ],
        )
    }

    /// Verifies a self-signed leaf certificate, enforcing that self-signed keys
    /// can NEVER claim [`PublisherTrustTier::PlatformCore`] or
    /// [`PublisherTrustTier::CertifiedPublisher`].
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::UntrustedCertificateChain`] on tier
    /// escalation or bad self-signature, or [`PublisherTrustCheckError::ExpiredCertificate`]
    /// if expired at `now_ms`.
    pub fn verify_self_signed_community(
        &self,
        now_ms: Option<u64>,
    ) -> Result<(), PublisherTrustCheckError> {
        if !self.is_self_signed() {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "certificate is not self-signed",
            ));
        }
        if self.trust_tier != PublisherTrustTier::CommunitySandboxed {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "self-signed leaf certificate is strictly confined to COMMUNITY_SANDBOXED",
            ));
        }
        if self.expires_at_ms <= self.not_before_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "leaf certificate validity window is empty or inverted",
            ));
        }
        if !verify_strict_digest_sig(
            &self.leaf_public_key,
            &self.signing_digest(),
            &self.signature,
        ) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "self-signed community leaf signature verification failed",
            ));
        }
        if let Some(now) = now_ms {
            if now < self.not_before_ms {
                return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                    "leaf certificate is not yet valid",
                ));
            }
            if now > self.expires_at_ms {
                return Err(PublisherTrustCheckError::ExpiredCertificate {
                    expires_at_ms: self.expires_at_ms,
                    now_ms: now,
                });
            }
        }
        Ok(())
    }

    /// Verifies a CA-issued leaf certificate against its [`StoreIntermediateCertificate`]
    /// and [`OfflinePlatformRootCa`].
    ///
    /// # Errors
    ///
    /// Fails closed if the leaf is self-signed, exceeds the intermediate CA's
    /// `max_issuable_tier`, fails signature verification, or is expired at `now_ms`.
    pub fn verify_against_intermediate(
        &self,
        intermediate: &StoreIntermediateCertificate,
        root: &OfflinePlatformRootCa,
        now_ms: Option<u64>,
    ) -> Result<(), PublisherTrustCheckError> {
        if self.is_self_signed() {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "self-signed certificate cannot be verified as CA-issued",
            ));
        }
        intermediate.verify_against_root(root, now_ms)?;
        if self.issuer_serial_id != intermediate.serial_id {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "leaf issuer_serial_id does not match intermediate CA serial_id",
            ));
        }
        if !intermediate.can_issue_tier(self.trust_tier) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "leaf trust_tier exceeds intermediate CA max_issuable_tier ceiling",
            ));
        }
        if self.expires_at_ms <= self.not_before_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "leaf certificate validity window is empty or inverted",
            ));
        }
        if self.not_before_ms < intermediate.not_before_ms
            || self.expires_at_ms > intermediate.expires_at_ms
        {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "leaf certificate validity window exceeds intermediate CA bounds",
            ));
        }
        if !verify_strict_digest_sig(
            &intermediate.intermediate_public_key,
            &self.signing_digest(),
            &self.signature,
        ) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "leaf certificate intermediate CA signature invalid",
            ));
        }
        if let Some(now) = now_ms {
            if now < self.not_before_ms {
                return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                    "leaf certificate is not yet valid",
                ));
            }
            if now > self.expires_at_ms {
                return Err(PublisherTrustCheckError::ExpiredCertificate {
                    expires_at_ms: self.expires_at_ms,
                    now_ms: now,
                });
            }
        }
        Ok(())
    }
}

/// Disposition applied to the outgoing leaf key during a publisher key rotation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RotatedKeyDisposition {
    /// Clean planned rotation: old key transitions to [`LeafCertificateStatus::RotatedRetired`].
    RetireVerifyOnly,
    /// Emergency compromise rotation: old key is immediately marked
    /// [`LeafCertificateStatus::Compromised`] and tombstoned at `revoked_at_epoch`.
    RevokeCompromised { revoked_at_epoch: RevocationEpoch },
}

/// Cryptographic authorization proof attached to a [`PublisherKeyRotationReceipt`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyRotationProof {
    /// Co-signed by both the outgoing leaf key and the incoming leaf key.
    DualSignedByOldAndNewLeaf {
        old_leaf_signature: [u8; 64],
        new_leaf_signature: [u8; 64],
    },
    /// Attested by an active [`StoreIntermediateCertificate`] plus the incoming leaf key
    /// (used when the outgoing key was lost or compromised).
    IntermediateCaAttested {
        intermediate_serial_id: [u8; 16],
        intermediate_signature: [u8; 64],
        new_leaf_signature: [u8; 64],
    },
}

/// Signed key-rotation receipt (`old_leaf_key_id -> new_leaf_key_id`) preserving
/// [`PublisherId`] identity and [`nlos_types::ApplicationId`] ownership across rotations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublisherKeyRotationReceipt {
    pub receipt_id: ReceiptId,
    pub publisher_id: PublisherId,
    pub old_leaf_serial_id: [u8; 16],
    pub old_leaf_key_id: KeyId,
    pub new_leaf_certificate: PublisherLeafCertificate,
    pub disposition: RotatedKeyDisposition,
    pub rotated_at_ms: u64,
    pub proof: KeyRotationProof,
}

impl PublisherKeyRotationReceipt {
    /// Computes the canonical proposal digest bound to `(publisher_id, old_leaf, new_leaf, disposition, rotated_at_ms)`.
    #[must_use]
    pub fn proposal_digest(
        publisher_id: PublisherId,
        old_leaf_serial_id: [u8; 16],
        old_leaf_key_id: KeyId,
        new_leaf_certificate: &PublisherLeafCertificate,
        disposition: RotatedKeyDisposition,
        rotated_at_ms: u64,
    ) -> [u8; 32] {
        let (disp_tag, epoch_u64) = match disposition {
            RotatedKeyDisposition::RetireVerifyOnly => (1_u8, 0_u64),
            RotatedKeyDisposition::RevokeCompromised { revoked_at_epoch } => {
                (2_u8, revoked_at_epoch.get())
            }
        };
        let new_cert_digest = new_leaf_certificate.signing_digest();
        custody_domain_sha256(
            DOMAIN_KEY_ROTATION_PROPOSAL,
            &[
                publisher_id.as_bytes(),
                old_leaf_serial_id.as_slice(),
                old_leaf_key_id.as_bytes(),
                new_leaf_certificate.serial_id.as_slice(),
                new_leaf_certificate.leaf_key_id.as_bytes(),
                &new_cert_digest,
                &[disp_tag],
                &epoch_u64.to_be_bytes(),
                &rotated_at_ms.to_be_bytes(),
            ],
        )
    }

    pub(crate) fn role_signing_digest(role_domain: &[u8], proposal_digest: &[u8; 32]) -> [u8; 32] {
        custody_domain_sha256(role_domain, &[proposal_digest.as_slice()])
    }

    fn derive_receipt_id(proposal_digest: &[u8; 32], proof_tag: u8) -> ReceiptId {
        let full = custody_domain_sha256(
            DOMAIN_KEY_ROTATION_RECEIPT_ID,
            &[proposal_digest.as_slice(), &[proof_tag]],
        );
        let mut id = [0_u8; 16];
        id.copy_from_slice(&full[..16]);
        ReceiptId::from_bytes(id)
    }

    /// Creates a dual-signed (`old_leaf_seed + new_leaf_seed`) key-rotation receipt.
    #[must_use]
    pub fn issue_dual_signed(
        old_leaf: &PublisherLeafCertificate,
        old_leaf_seed: &[u8; 32],
        new_leaf_certificate: PublisherLeafCertificate,
        new_leaf_seed: &[u8; 32],
        disposition: RotatedKeyDisposition,
        rotated_at_ms: u64,
    ) -> Self {
        let prop = Self::proposal_digest(
            old_leaf.publisher_id,
            old_leaf.serial_id,
            old_leaf.leaf_key_id,
            &new_leaf_certificate,
            disposition,
            rotated_at_ms,
        );
        let old_digest = Self::role_signing_digest(DOMAIN_KEY_ROTATION_OLD_LEAF, &prop);
        let new_digest = Self::role_signing_digest(DOMAIN_KEY_ROTATION_NEW_LEAF, &prop);
        let old_leaf_signature = sign_ed25519_digest_with_seed(old_leaf_seed, &old_digest);
        let new_leaf_signature = sign_ed25519_digest_with_seed(new_leaf_seed, &new_digest);
        Self {
            receipt_id: Self::derive_receipt_id(&prop, 1),
            publisher_id: old_leaf.publisher_id,
            old_leaf_serial_id: old_leaf.serial_id,
            old_leaf_key_id: old_leaf.leaf_key_id,
            new_leaf_certificate,
            disposition,
            rotated_at_ms,
            proof: KeyRotationProof::DualSignedByOldAndNewLeaf {
                old_leaf_signature,
                new_leaf_signature,
            },
        }
    }

    /// Creates a Store-Intermediate-CA-attested key-rotation receipt using
    /// [`SigningKeyCustodyVault`] for the intermediate CA signature.
    ///
    /// # Errors
    ///
    /// Fails closed if custody signing fails or the vault public key disagrees with
    /// `intermediate.intermediate_public_key`.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_intermediate_attested_under_custody(
        old_leaf: &PublisherLeafCertificate,
        new_leaf_certificate: PublisherLeafCertificate,
        new_leaf_seed: &[u8; 32],
        intermediate: &StoreIntermediateCertificate,
        vault: &SigningKeyCustodyVault,
        expected_rotation_epoch: Option<u64>,
        disposition: RotatedKeyDisposition,
        rotated_at_ms: u64,
    ) -> Result<(Self, CustodySignAuditReceipt), PublisherTrustCheckError> {
        let prop = Self::proposal_digest(
            old_leaf.publisher_id,
            old_leaf.serial_id,
            old_leaf.leaf_key_id,
            &new_leaf_certificate,
            disposition,
            rotated_at_ms,
        );
        let ca_digest = Self::role_signing_digest(DOMAIN_KEY_ROTATION_STORE_CA, &prop);
        let new_digest = Self::role_signing_digest(DOMAIN_KEY_ROTATION_NEW_LEAF, &prop);
        let audit = vault
            .sign_digest_under_custody(CustodySignRequest {
                key_id: intermediate.intermediate_key_id,
                expected_rotation_epoch,
                domain_tag: DOMAIN_KEY_ROTATION_STORE_CA,
                message_digest: ca_digest,
                signed_at_ms: rotated_at_ms,
            })
            .map_err(|err| {
                PublisherTrustCheckError::StorageFailure(format!(
                    "intermediate CA rotation attestation failed: {err}"
                ))
            })?;
        let intermediate_signature = audit.signature;
        if !verify_strict_digest_sig(
            &intermediate.intermediate_public_key,
            &ca_digest,
            &intermediate_signature,
        ) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "custody vault key mismatch during rotation attestation",
            ));
        }
        let new_leaf_signature = sign_ed25519_digest_with_seed(new_leaf_seed, &new_digest);
        Ok((
            Self {
                receipt_id: Self::derive_receipt_id(&prop, 2),
                publisher_id: old_leaf.publisher_id,
                old_leaf_serial_id: old_leaf.serial_id,
                old_leaf_key_id: old_leaf.leaf_key_id,
                new_leaf_certificate,
                disposition,
                rotated_at_ms,
                proof: KeyRotationProof::IntermediateCaAttested {
                    intermediate_serial_id: intermediate.serial_id,
                    intermediate_signature,
                    new_leaf_signature,
                },
            },
            audit,
        ))
    }
}
