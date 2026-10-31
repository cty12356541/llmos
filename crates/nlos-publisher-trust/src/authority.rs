//! `SQLite`-backed [`SqlitePublisherTrustAuthority`] implementing
//! [`PublisherTrustVerifier`], certificate chain management, key rotation,
//! monotonic CRL import, anti-freeze / `[DUI-OFFLINE-001]` verification, and
//! `[PKG-APPID-001]` / `[PKG-SEP-001]` ownership ledger (`ADR-0027` section 2.3).

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use nlos_types::{ApplicationId, InstallationId, KeyId, PackageId, ReceiptId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::ownership::{
    OwnershipTransferProposal, PackageReleaseBinding, PackageReleaseDecision,
    SeparatedPackageApplicationInstallation, SignedOwnershipTransferReceipt,
    verify_identity_separation,
};
use crate::pki::{
    DOMAIN_KEY_ROTATION_NEW_LEAF, DOMAIN_KEY_ROTATION_OLD_LEAF, DOMAIN_KEY_ROTATION_STORE_CA,
    KeyRotationProof, LeafCertificateStatus, OfflinePlatformRootCa, PublisherKeyRotationReceipt,
    PublisherLeafCertificate, RotatedKeyDisposition, StoreIntermediateCertificate,
    verify_strict_digest_sig,
};
use crate::revocation::{
    CrlImportDecision, CrlIssuerKind, RevocationReason, RevocationStateSnapshot,
    SignedRevocationList, TrustFreshnessMode,
};
use crate::schema::{
    REVOCATION_STATE_SINGLETON_ID, decode_u64, encode_u64, fixed16, fixed32, fixed64,
    open_verified_connection, sql_err,
};
use crate::types::{
    ApplicationOwnershipBinding, PublisherId, PublisherTrustCheckError, PublisherTrustTier,
    PublisherTrustVerifier, RevocationEpoch, VerifiedPublisherIdentity,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoredLeafRow {
    cert: PublisherLeafCertificate,
    status: LeafCertificateStatus,
    compromised_at_epoch: RevocationEpoch,
}

/// Durable `SQLite` (`WAL` + `FULL`, `STRICT` tables) authority for publisher PKI
/// trust chains, monotonic CRL epochs, and `ApplicationId` cryptographic ownership.
pub struct SqlitePublisherTrustAuthority {
    connection: Mutex<Connection>,
}

impl SqlitePublisherTrustAuthority {
    /// Opens or initializes `<path>/publisher-trust.db` (or the exact file `path`
    /// if it carries a file extension) with verified `WAL` + `FULL` durability.
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::StorageFailure`] if `SQLite` cannot open,
    /// `WAL` + `FULL` durability cannot be verified, or the schema version is unsupported.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PublisherTrustCheckError> {
        let connection = open_verified_connection(path.as_ref())?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, PublisherTrustCheckError> {
        self.connection.lock().map_err(|_| {
            PublisherTrustCheckError::StorageFailure(
                "publisher trust authority mutex is poisoned".to_owned(),
            )
        })
    }

    /// Anchors an offline first-party [`OfflinePlatformRootCa`] into `trust_root_anchors`.
    ///
    /// Idempotent when re-anchoring a byte-identical root certificate.
    ///
    /// # Errors
    ///
    /// Fails closed if the root anchor's fingerprint or self-signature is invalid,
    /// expired at `anchored_at_ms`, or conflicts with an already-anchored root serial.
    pub fn anchor_offline_root_ca(
        &self,
        root: &OfflinePlatformRootCa,
        anchored_at_ms: u64,
    ) -> Result<(), PublisherTrustCheckError> {
        root.verify_self_signed(Some(anchored_at_ms))?;
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        if let Some(existing) = load_root_anchor(&tx, root.root_serial_id)? {
            if &existing == root {
                tx.commit().map_err(sql_err)?;
                return Ok(());
            }
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "conflicting offline root CA anchor for existing root_serial_id",
            ));
        }
        tx.execute(
            "INSERT INTO trust_root_anchors (
                root_serial_id, root_key_id, root_public_key, public_key_fingerprint,
                not_before_ms, expires_at_ms, self_signature, anchored_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                root.root_serial_id.as_slice(),
                root.root_key_id.as_bytes().as_slice(),
                root.root_public_key.as_slice(),
                root.public_key_fingerprint.as_slice(),
                encode_u64(root.not_before_ms)?,
                encode_u64(root.expires_at_ms)?,
                root.self_signature.as_slice(),
                encode_u64(anchored_at_ms)?,
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    /// Registers a root-signed [`StoreIntermediateCertificate`] after verifying its
    /// chain to an anchored [`OfflinePlatformRootCa`] and checking revocation state.
    ///
    /// # Errors
    ///
    /// Fails closed with [`PublisherTrustCheckError::UntrustedCertificateChain`],
    /// [`PublisherTrustCheckError::ExpiredCertificate`], or
    /// [`PublisherTrustCheckError::RevokedCertificate`] on any chain/revocation failure.
    pub fn register_intermediate_certificate(
        &self,
        cert: &StoreIntermediateCertificate,
        registered_at_ms: u64,
    ) -> Result<(), PublisherTrustCheckError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let root = load_root_anchor(&tx, cert.issuer_root_serial_id)?.ok_or(
            PublisherTrustCheckError::UntrustedCertificateChain(
                "intermediate CA issuer_root_serial_id is not an anchored platform root CA",
            ),
        )?;
        cert.verify_against_root(&root, Some(registered_at_ms))?;
        ensure_not_revoked_serial_or_key(&tx, cert.serial_id, cert.intermediate_key_id)?;

        if let Some(existing) = load_intermediate(&tx, cert.serial_id)? {
            if &existing == cert {
                tx.commit().map_err(sql_err)?;
                return Ok(());
            }
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "conflicting intermediate CA certificate for existing serial_id",
            ));
        }

        tx.execute(
            "INSERT INTO store_intermediate_certificates (
                serial_id, issuer_root_serial_id, intermediate_key_id,
                intermediate_public_key, max_issuable_tier, not_before_ms,
                expires_at_ms, root_signature, registered_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                cert.serial_id.as_slice(),
                cert.issuer_root_serial_id.as_slice(),
                cert.intermediate_key_id.as_bytes().as_slice(),
                cert.intermediate_public_key.as_slice(),
                i64::from(cert.max_issuable_tier.encode()),
                encode_u64(cert.not_before_ms)?,
                encode_u64(cert.expires_at_ms)?,
                cert.root_signature.as_slice(),
                encode_u64(registered_at_ms)?,
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    /// Registers a [`PublisherLeafCertificate`], enforcing three-tier domain isolation:
    /// - [`PublisherTrustTier::PlatformCore`] and [`PublisherTrustTier::CertifiedPublisher`]
    ///   MUST chain through an active, unrevoked [`StoreIntermediateCertificate`] whose
    ///   `max_issuable_tier` permits the leaf tier up to an anchored [`OfflinePlatformRootCa`].
    /// - Self-signed certificates are permitted ONLY for [`PublisherTrustTier::CommunitySandboxed`];
    ///   any self-signed certificate claiming `PlatformCore` or `CertifiedPublisher` fails
    ///   closed with [`PublisherTrustCheckError::UntrustedCertificateChain`].
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError`] if the publisher or certificate is revoked,
    /// expired, untrusted, or conflicts with an existing certificate record.
    pub fn register_leaf_certificate(
        &self,
        cert: &PublisherLeafCertificate,
        registered_at_ms: u64,
    ) -> Result<(), PublisherTrustCheckError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        ensure_publisher_not_revoked(&tx, cert.publisher_id)?;
        ensure_not_revoked_serial_or_key(&tx, cert.serial_id, cert.leaf_key_id)?;
        verify_leaf_chain_in_tx(&tx, cert, Some(registered_at_ms))?;

        if let Some(existing) = load_leaf_by_serial(&tx, cert.serial_id)? {
            if &existing.cert == cert {
                if existing.status == LeafCertificateStatus::Compromised {
                    return Err(PublisherTrustCheckError::RevokedCertificate {
                        serial_or_key: cert.serial_id,
                        revoked_at_epoch: existing.compromised_at_epoch,
                    });
                }
                tx.commit().map_err(sql_err)?;
                return Ok(());
            }
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "conflicting publisher leaf certificate for existing serial_id",
            ));
        }

        insert_leaf_row(
            &tx,
            cert,
            LeafCertificateStatus::Active,
            0,
            registered_at_ms,
        )?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    /// Executes an authenticated publisher key rotation (`old_leaf_key_id -> new_leaf_key_id`)
    /// verified either by dual leaf signatures or Store Intermediate CA attestation.
    ///
    /// Preserves all existing [`ApplicationId`] ownership bindings for `receipt.publisher_id`,
    /// while immediately invalidating the outgoing key if `receipt.disposition` is
    /// [`RotatedKeyDisposition::RevokeCompromised`].
    ///
    /// # Errors
    ///
    /// Fails closed on unknown publisher, revoked publisher, invalid proof signatures,
    /// compromised outgoing key attempting self-signed dual rotation, or untrusted incoming leaf.
    #[allow(clippy::too_many_lines)]
    pub fn rotate_publisher_key(
        &self,
        receipt: &PublisherKeyRotationReceipt,
    ) -> Result<(), PublisherTrustCheckError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        if rotation_receipt_exists(&tx, receipt.receipt_id)? {
            tx.commit().map_err(sql_err)?;
            return Ok(());
        }

        ensure_publisher_not_revoked(&tx, receipt.publisher_id)?;

        let old_row = load_leaf_by_serial(&tx, receipt.old_leaf_serial_id)?.ok_or(
            PublisherTrustCheckError::UnknownPublisher(receipt.publisher_id),
        )?;
        if old_row.cert.publisher_id != receipt.publisher_id
            || old_row.cert.leaf_key_id != receipt.old_leaf_key_id
        {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "key rotation receipt outgoing leaf metadata does not match stored certificate",
            ));
        }

        let new_cert = &receipt.new_leaf_certificate;
        if new_cert.publisher_id != receipt.publisher_id {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "key rotation incoming certificate belongs to a different PublisherId",
            ));
        }
        if new_cert.serial_id == old_row.cert.serial_id
            || new_cert.leaf_key_id == old_row.cert.leaf_key_id
            || new_cert.leaf_public_key == old_row.cert.leaf_public_key
        {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "key rotation incoming certificate must introduce a fresh serial_id, KeyId, and public key",
            ));
        }

        ensure_not_revoked_serial_or_key(&tx, new_cert.serial_id, new_cert.leaf_key_id)?;
        verify_leaf_chain_in_tx(&tx, new_cert, Some(receipt.rotated_at_ms))?;

        let prop_digest = PublisherKeyRotationReceipt::proposal_digest(
            receipt.publisher_id,
            receipt.old_leaf_serial_id,
            receipt.old_leaf_key_id,
            new_cert,
            receipt.disposition,
            receipt.rotated_at_ms,
        );
        let new_sig_digest = PublisherKeyRotationReceipt::role_signing_digest(
            DOMAIN_KEY_ROTATION_NEW_LEAF,
            &prop_digest,
        );

        let (proof_kind, proof_signer_id, primary_sig, new_leaf_sig) = match receipt.proof {
            KeyRotationProof::DualSignedByOldAndNewLeaf {
                old_leaf_signature,
                new_leaf_signature,
            } => {
                // An already-compromised outgoing key cannot authorize its own rotation.
                ensure_not_revoked_serial_or_key(
                    &tx,
                    old_row.cert.serial_id,
                    old_row.cert.leaf_key_id,
                )?;
                if old_row.status == LeafCertificateStatus::Compromised {
                    return Err(PublisherTrustCheckError::RevokedCertificate {
                        serial_or_key: old_row.cert.serial_id,
                        revoked_at_epoch: old_row.compromised_at_epoch,
                    });
                }
                if old_row.status != LeafCertificateStatus::Active {
                    return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                        "dual-signed rotation requires outgoing leaf certificate to be Active",
                    ));
                }
                let old_sig_digest = PublisherKeyRotationReceipt::role_signing_digest(
                    DOMAIN_KEY_ROTATION_OLD_LEAF,
                    &prop_digest,
                );
                if !verify_strict_digest_sig(
                    &old_row.cert.leaf_public_key,
                    &old_sig_digest,
                    &old_leaf_signature,
                ) || !verify_strict_digest_sig(
                    &new_cert.leaf_public_key,
                    &new_sig_digest,
                    &new_leaf_signature,
                ) {
                    return Err(PublisherTrustCheckError::SignatureInvalid);
                }
                (
                    1_i64,
                    old_row.cert.serial_id,
                    old_leaf_signature,
                    new_leaf_signature,
                )
            }
            KeyRotationProof::IntermediateCaAttested {
                intermediate_serial_id,
                intermediate_signature,
                new_leaf_signature,
            } => {
                let intermediate = load_intermediate(&tx, intermediate_serial_id)?.ok_or(
                    PublisherTrustCheckError::UntrustedCertificateChain(
                        "key rotation attesting intermediate CA is unknown",
                    ),
                )?;
                ensure_not_revoked_serial_or_key(
                    &tx,
                    intermediate.serial_id,
                    intermediate.intermediate_key_id,
                )?;
                let root = load_root_anchor(&tx, intermediate.issuer_root_serial_id)?.ok_or(
                    PublisherTrustCheckError::UntrustedCertificateChain(
                        "key rotation intermediate CA root anchor is missing",
                    ),
                )?;
                intermediate.verify_against_root(&root, Some(receipt.rotated_at_ms))?;
                if !intermediate.can_issue_tier(new_cert.trust_tier) {
                    return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                        "key rotation intermediate CA cannot attest requested trust tier",
                    ));
                }
                let ca_sig_digest = PublisherKeyRotationReceipt::role_signing_digest(
                    DOMAIN_KEY_ROTATION_STORE_CA,
                    &prop_digest,
                );
                if !verify_strict_digest_sig(
                    &intermediate.intermediate_public_key,
                    &ca_sig_digest,
                    &intermediate_signature,
                ) || !verify_strict_digest_sig(
                    &new_cert.leaf_public_key,
                    &new_sig_digest,
                    &new_leaf_signature,
                ) {
                    return Err(PublisherTrustCheckError::SignatureInvalid);
                }
                (
                    2_i64,
                    intermediate_serial_id,
                    intermediate_signature,
                    new_leaf_signature,
                )
            }
        };

        let current_state = load_revocation_state(&tx)?;
        let (disposition_code, new_old_status, recorded_epoch) = match receipt.disposition {
            RotatedKeyDisposition::RetireVerifyOnly => {
                let preserved = if old_row.status == LeafCertificateStatus::Compromised {
                    LeafCertificateStatus::Compromised
                } else {
                    LeafCertificateStatus::RotatedRetired
                };
                (1_i64, preserved, old_row.compromised_at_epoch.get())
            }
            RotatedKeyDisposition::RevokeCompromised { revoked_at_epoch } => {
                let effective_epoch = revoked_at_epoch
                    .get()
                    .max(current_state.revocation_epoch.get())
                    .max(1);
                insert_revoked_cert_entry(
                    &tx,
                    old_row.cert.serial_id,
                    effective_epoch,
                    RevocationReason::KeyCompromise,
                    receipt.rotated_at_ms,
                )?;
                insert_revoked_cert_entry(
                    &tx,
                    old_row.cert.leaf_key_id.into_bytes(),
                    effective_epoch,
                    RevocationReason::KeyCompromise,
                    receipt.rotated_at_ms,
                )?;
                (2_i64, LeafCertificateStatus::Compromised, effective_epoch)
            }
        };

        if old_row.status != LeafCertificateStatus::Compromised {
            tx.execute(
                "UPDATE publisher_leaf_certificates
                 SET status = ?2, compromised_at_epoch = ?3
                 WHERE serial_id = ?1",
                params![
                    old_row.cert.serial_id.as_slice(),
                    new_old_status.encode(),
                    encode_u64(recorded_epoch)?,
                ],
            )
            .map_err(sql_err)?;
        }

        insert_leaf_row(
            &tx,
            new_cert,
            LeafCertificateStatus::Active,
            0,
            receipt.rotated_at_ms,
        )?;

        tx.execute(
            "INSERT INTO publisher_key_rotations (
                receipt_id, publisher_id, old_leaf_serial_id, old_leaf_key_id,
                new_leaf_serial_id, new_leaf_key_id, disposition_code,
                revoked_at_epoch, proof_kind, proof_signer_id,
                primary_signature, new_leaf_signature, rotated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                receipt.receipt_id.as_bytes().as_slice(),
                receipt.publisher_id.as_bytes().as_slice(),
                receipt.old_leaf_serial_id.as_slice(),
                receipt.old_leaf_key_id.as_bytes().as_slice(),
                new_cert.serial_id.as_slice(),
                new_cert.leaf_key_id.as_bytes().as_slice(),
                disposition_code,
                encode_u64(recorded_epoch)?,
                proof_kind,
                proof_signer_id.as_slice(),
                primary_sig.as_slice(),
                new_leaf_sig.as_slice(),
                encode_u64(receipt.rotated_at_ms)?,
            ],
        )
        .map_err(sql_err)?;

        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    /// Inspects the current [`LeafCertificateStatus`] of a registered leaf serial.
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::StorageFailure`] on corrupt storage.
    pub fn inspect_leaf_certificate_status(
        &self,
        serial_id: [u8; 16],
    ) -> Result<Option<LeafCertificateStatus>, PublisherTrustCheckError> {
        let conn = self.lock()?;
        Ok(load_leaf_by_serial(&conn, serial_id)?.map(|row| row.status))
    }

    /// Imports and cryptographically verifies a [`SignedRevocationList`], enforcing
    /// strictly monotonic [`RevocationEpoch`] progression (`ADR-0027` section 2.3).
    ///
    /// # Errors
    ///
    /// - [`PublisherTrustCheckError::StaleOrRollbackRevocationEpoch`] if
    ///   `crl.revocation_epoch < current_epoch`, `crl.revocation_epoch == 0`, or
    ///   `crl.revocation_epoch == current_epoch` with a different CRL digest.
    /// - [`PublisherTrustCheckError::UntrustedCertificateChain`] /
    ///   [`PublisherTrustCheckError::RevokedCertificate`] /
    ///   [`PublisherTrustCheckError::SignatureInvalid`] if the CRL issuer is unknown,
    ///   revoked, expired at `issued_at_ms`, or signature verification fails.
    pub fn import_signed_revocation_list(
        &self,
        crl: &SignedRevocationList,
    ) -> Result<CrlImportDecision, PublisherTrustCheckError> {
        if crl.fresh_until_ms <= crl.issued_at_ms {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "CRL fresh_until_ms must strictly exceed issued_at_ms",
            ));
        }

        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        let current = load_revocation_state(&tx)?;
        if crl.revocation_epoch.get() == 0 || crl.revocation_epoch < current.revocation_epoch {
            return Err(PublisherTrustCheckError::StaleOrRollbackRevocationEpoch {
                current_epoch: current.revocation_epoch,
                candidate_epoch: crl.revocation_epoch,
            });
        }

        let candidate_digest = crl.crl_digest();
        if crl.revocation_epoch == current.revocation_epoch {
            if candidate_digest != current.crl_digest {
                return Err(PublisherTrustCheckError::StaleOrRollbackRevocationEpoch {
                    current_epoch: current.revocation_epoch,
                    candidate_epoch: crl.revocation_epoch,
                });
            }
            verify_crl_issuer_signature(&tx, crl, &candidate_digest)?;
            tx.commit().map_err(sql_err)?;
            return Ok(CrlImportDecision::Replayed(current.revocation_epoch));
        }

        verify_crl_issuer_signature(&tx, crl, &candidate_digest)?;

        let epoch_u64 = crl.revocation_epoch.get();
        for entry in &crl.revoked_certificates {
            insert_revoked_cert_entry(
                &tx,
                entry.serial_or_key,
                epoch_u64,
                entry.reason,
                crl.issued_at_ms,
            )?;
            tx.execute(
                "UPDATE publisher_leaf_certificates
                 SET status = ?2, compromised_at_epoch = CASE
                     WHEN compromised_at_epoch = 0 THEN ?3
                     ELSE compromised_at_epoch
                 END
                 WHERE (serial_id = ?1 OR leaf_key_id = ?1) AND status != ?2",
                params![
                    entry.serial_or_key.as_slice(),
                    LeafCertificateStatus::Compromised.encode(),
                    encode_u64(epoch_u64)?,
                ],
            )
            .map_err(sql_err)?;
        }

        for publisher_id in &crl.revoked_publishers {
            tx.execute(
                "INSERT INTO revoked_publishers (publisher_id, revoked_at_epoch, recorded_at_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(publisher_id) DO NOTHING",
                params![
                    publisher_id.as_bytes().as_slice(),
                    encode_u64(epoch_u64)?,
                    encode_u64(crl.issued_at_ms)?,
                ],
            )
            .map_err(sql_err)?;
        }

        tx.execute(
            "UPDATE trust_revocation_state
             SET revocation_epoch = ?2, issued_at_ms = ?3, fresh_until_ms = ?4, crl_digest = ?5
             WHERE singleton_id = ?1",
            params![
                REVOCATION_STATE_SINGLETON_ID,
                encode_u64(epoch_u64)?,
                encode_u64(crl.issued_at_ms)?,
                encode_u64(crl.fresh_until_ms)?,
                candidate_digest.as_slice(),
            ],
        )
        .map_err(sql_err)?;

        tx.commit().map_err(sql_err)?;
        Ok(CrlImportDecision::Advanced(crl.revocation_epoch))
    }

    /// Reads the durable [`RevocationStateSnapshot`].
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::StorageFailure`] on corrupt storage.
    pub fn inspect_revocation_state(
        &self,
    ) -> Result<RevocationStateSnapshot, PublisherTrustCheckError> {
        let conn = self.lock()?;
        load_revocation_state(&conn)
    }

    /// Verifies a publisher `Ed25519` signature under an explicit [`TrustFreshnessMode`].
    ///
    /// - [`TrustFreshnessMode::OnlineInstallOrUpgrade`] enforces `now_ms <= fresh_until_ms`
    ///   (anti-freeze check for store installs and upgrades).
    /// - [`TrustFreshnessMode::OfflineInstalledLaunch`] permits already-installed offline
    ///   launches past `fresh_until_ms` (`[DUI-OFFLINE-001]`) while still enforcing cached
    ///   revocations, certificate-chain integrity, expiry, and `Ed25519` verification.
    ///
    /// # Errors
    ///
    /// Fails closed with the exact [`PublisherTrustCheckError`] describing the violation.
    pub fn verify_publisher_signature_with_mode(
        &self,
        publisher_id: PublisherId,
        signing_public_key: &[u8; 32],
        message_digest: &[u8; 32],
        signature: &[u8; 64],
        now_ms: u64,
        freshness_mode: TrustFreshnessMode,
    ) -> Result<VerifiedPublisherIdentity, PublisherTrustCheckError> {
        let conn = self.lock()?;

        // 1. Whole-publisher revocation always blocks immediately in both modes.
        ensure_publisher_not_revoked(&conn, publisher_id)?;

        // 2. Resolve the publisher's registered leaf certificate for `signing_public_key`.
        let Some(leaf_row) =
            load_leaf_by_publisher_and_pubkey(&conn, publisher_id, signing_public_key)?
        else {
            if publisher_has_any_certificate(&conn, publisher_id)? {
                return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                    "signing public key is not bound to any registered certificate of publisher",
                ));
            }
            return Err(PublisherTrustCheckError::UnknownPublisher(publisher_id));
        };

        // 3. Leaf certificate / KeyId revocation check (irrevocable across online & offline).
        ensure_not_revoked_serial_or_key(
            &conn,
            leaf_row.cert.serial_id,
            leaf_row.cert.leaf_key_id,
        )?;
        if leaf_row.status == LeafCertificateStatus::Compromised {
            return Err(PublisherTrustCheckError::RevokedCertificate {
                serial_or_key: leaf_row.cert.serial_id,
                revoked_at_epoch: leaf_row.compromised_at_epoch,
            });
        }

        // 4. Full certificate-chain & validity-window verification (including intermediate CA revocation).
        verify_leaf_chain_in_tx(&conn, &leaf_row.cert, Some(now_ms))?;

        // 5. CRL anti-freeze freshness policy check.
        let rev_state = load_revocation_state(&conn)?;
        if freshness_mode == TrustFreshnessMode::OnlineInstallOrUpgrade
            && (rev_state.revocation_epoch.get() == 0 || now_ms > rev_state.fresh_until_ms)
        {
            return Err(PublisherTrustCheckError::FrozenRevocationList {
                expires_at_ms: rev_state.fresh_until_ms,
                now_ms,
            });
        }

        // 6. Strict RFC 8032 Ed25519 signature verification over the 32-byte digest.
        if !verify_strict_digest_sig(signing_public_key, message_digest, signature) {
            return Err(PublisherTrustCheckError::SignatureInvalid);
        }

        Ok(VerifiedPublisherIdentity {
            publisher_id,
            trust_tier: leaf_row.cert.trust_tier,
            leaf_key_id: leaf_row.cert.leaf_key_id,
            leaf_public_key: leaf_row.cert.leaf_public_key,
            revocation_epoch: rev_state.revocation_epoch,
            verified_at_ms: now_ms,
        })
    }

    /// Dedicated entry point for local offline launch verification of an already-installed
    /// application (`[DUI-OFFLINE-001]`, `ADR-0027` section 2.3).
    ///
    /// # Errors
    ///
    /// Fails closed if the cached CRL or local key-rotation ledger marks the publisher,
    /// leaf certificate, or intermediate CA as revoked, if the certificate is untrusted
    /// or expired, or if the signature is invalid.
    pub fn verify_cached_offline_launch(
        &self,
        publisher_id: PublisherId,
        signing_public_key: &[u8; 32],
        message_digest: &[u8; 32],
        signature: &[u8; 64],
        now_ms: u64,
    ) -> Result<VerifiedPublisherIdentity, PublisherTrustCheckError> {
        self.verify_publisher_signature_with_mode(
            publisher_id,
            signing_public_key,
            message_digest,
            signature,
            now_ms,
            TrustFreshnessMode::OfflineInstalledLaunch,
        )
    }

    /// Inspects the durable [`ApplicationOwnershipBinding`] for `application_id`, if registered.
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::StorageFailure`] on corrupt storage.
    pub fn inspect_application_owner(
        &self,
        application_id: ApplicationId,
    ) -> Result<Option<ApplicationOwnershipBinding>, PublisherTrustCheckError> {
        let conn = self.lock()?;
        load_application_owner(&conn, application_id)
    }

    /// Atomically verifies `(publisher_id -> application_id)` ownership (`[PKG-APPID-001]`)
    /// and binds an immutable [`PackageId`] release artifact (`[PKG-IMM-001]`, `[PKG-SEP-001]`),
    /// proving that multiple distinct `PackageId` versions evolve under one stable
    /// `ApplicationId` without identity aliasing.
    ///
    /// # Errors
    ///
    /// - [`PublisherTrustCheckError::ApplicationIdHijackAttempt`] if another publisher owns `application_id`.
    /// - [`PublisherTrustCheckError::StorageFailure`] if `package_id` aliases `application_id`
    ///   (`[PKG-SEP-001]`) or attempts to mutate an already-bound `PackageId` (`[PKG-IMM-001]`).
    pub fn bind_package_release_for_application(
        &self,
        package_id: PackageId,
        application_id: ApplicationId,
        publisher_id: PublisherId,
        trust_tier: PublisherTrustTier,
        manifest_digest: [u8; 32],
        now_ms: u64,
    ) -> Result<(ApplicationOwnershipBinding, PackageReleaseDecision), PublisherTrustCheckError>
    {
        verify_identity_separation(package_id, application_id, None)?;
        let ownership = self.verify_or_bind_application_owner(
            publisher_id,
            trust_tier,
            application_id,
            now_ms,
        )?;

        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        if let Some(existing) = load_package_release(&tx, package_id)? {
            if existing.application_id == application_id
                && existing.publisher_id == publisher_id
                && existing.manifest_digest == manifest_digest
            {
                tx.commit().map_err(sql_err)?;
                return Ok((ownership, PackageReleaseDecision::Replayed(existing)));
            }
            return Err(PublisherTrustCheckError::StorageFailure(
                "PKG-IMM-001 violation: immutable PackageId cannot be rebound to another ApplicationId or manifest digest".to_owned(),
            ));
        }

        let binding = PackageReleaseBinding {
            package_id,
            application_id,
            publisher_id,
            manifest_digest,
            registered_at_ms: now_ms,
        };
        tx.execute(
            "INSERT INTO package_release_bindings (
                package_id, application_id, publisher_id, manifest_digest, registered_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                package_id.as_bytes().as_slice(),
                application_id.as_bytes().as_slice(),
                publisher_id.as_bytes().as_slice(),
                manifest_digest.as_slice(),
                encode_u64(now_ms)?,
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok((ownership, PackageReleaseDecision::Bound(binding)))
    }

    /// Lists all immutable [`PackageReleaseBinding`] records published under `application_id`.
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::StorageFailure`] on corrupt storage.
    pub fn list_application_package_releases(
        &self,
        application_id: ApplicationId,
    ) -> Result<Vec<PackageReleaseBinding>, PublisherTrustCheckError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT package_id, application_id, publisher_id, manifest_digest, registered_at_ms
                 FROM package_release_bindings
                 WHERE application_id = ?1
                 ORDER BY registered_at_ms, package_id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([application_id.as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(sql_err)?;

        let mut out = Vec::new();
        for row in rows {
            let (pkg, app, pub_id, digest, ts) = row.map_err(sql_err)?;
            out.push(PackageReleaseBinding {
                package_id: PackageId::from_bytes(fixed16(pkg, "package_id")?),
                application_id: ApplicationId::from_bytes(fixed16(app, "application_id")?),
                publisher_id: PublisherId::from_bytes(fixed16(pub_id, "publisher_id")?),
                manifest_digest: fixed32(digest, "manifest_digest")?,
                registered_at_ms: decode_u64(ts, "registered_at_ms")?,
            });
        }
        Ok(out)
    }

    /// Verifies `[PKG-SEP-001]` (`PackageId != ApplicationId != InstallationId`) for a
    /// concrete installation instance of a registered `(package_id -> application_id)` release.
    ///
    /// # Errors
    ///
    /// Fails closed if any two identifiers alias bytes or if `package_id` is not bound
    /// to `application_id`.
    pub fn verify_installation_identity_separation(
        &self,
        package_id: PackageId,
        application_id: ApplicationId,
        installation_id: InstallationId,
    ) -> Result<SeparatedPackageApplicationInstallation, PublisherTrustCheckError> {
        verify_identity_separation(package_id, application_id, Some(installation_id))?;
        let conn = self.lock()?;
        let Some(release) = load_package_release(&conn, package_id)? else {
            return Err(PublisherTrustCheckError::StorageFailure(
                "PackageId has no registered immutable release binding".to_owned(),
            ));
        };
        if release.application_id != application_id {
            return Err(PublisherTrustCheckError::StorageFailure(
                "PackageId is bound to a different ApplicationId".to_owned(),
            ));
        }
        Ok(SeparatedPackageApplicationInstallation {
            package_id,
            application_id,
            installation_id,
        })
    }

    /// Executes an atomic [`ApplicationId`] ownership transfer (`generation -> generation + 1`)
    /// authorized by a three-party co-signed [`SignedOwnershipTransferReceipt`]
    /// (`old_owner + new_owner + Store Intermediate CA`, `[PKG-APPID-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if the CRL is frozen, `from_publisher_id` is not the current owner
    /// ([`PublisherTrustCheckError::ApplicationIdHijackAttempt`]), any of the three
    /// certificates is untrusted/revoked/expired, or any of the three role-separated
    /// signatures is invalid ([`PublisherTrustCheckError::SignatureInvalid`]).
    #[allow(clippy::too_many_lines)]
    pub fn transfer_application_ownership(
        &self,
        receipt: &SignedOwnershipTransferReceipt,
    ) -> Result<ApplicationOwnershipBinding, PublisherTrustCheckError> {
        let proposal = &receipt.proposal;
        let expected_id = SignedOwnershipTransferReceipt::derive_receipt_id(
            proposal,
            &receipt.from_owner_signature,
            &receipt.to_owner_signature,
            &receipt.store_ca_signature,
        );
        if receipt.receipt_id != expected_id {
            return Err(PublisherTrustCheckError::SignatureInvalid);
        }
        if proposal.from_publisher_id == proposal.to_publisher_id {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "ownership transfer target publisher must differ from current owner",
            ));
        }

        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        let rev_state = load_revocation_state(&tx)?;
        if rev_state.revocation_epoch.get() == 0
            || proposal.transferred_at_ms > rev_state.fresh_until_ms
        {
            return Err(PublisherTrustCheckError::FrozenRevocationList {
                expires_at_ms: rev_state.fresh_until_ms,
                now_ms: proposal.transferred_at_ms,
            });
        }

        let current = load_application_owner(&tx, proposal.application_id)?.ok_or_else(|| {
            PublisherTrustCheckError::StorageFailure(
                "cannot transfer ownership of an unregistered ApplicationId".to_owned(),
            )
        })?;

        if current.owner_publisher_id != proposal.from_publisher_id {
            return Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
                application_id: proposal.application_id,
                registered_owner: current.owner_publisher_id,
                attempted_publisher: proposal.from_publisher_id,
            });
        }
        if current.registration_generation != proposal.expected_current_generation {
            return Err(PublisherTrustCheckError::StorageFailure(format!(
                "ownership transfer generation CAS mismatch: expected {}, actual {}",
                proposal.expected_current_generation, current.registration_generation
            )));
        }
        if proposal.transferred_at_ms < current.first_registered_at_ms {
            return Err(PublisherTrustCheckError::StorageFailure(
                "ownership transfer timestamp precedes first_registered_at_ms".to_owned(),
            ));
        }

        // Verify outgoing owner leaf key & signature.
        ensure_publisher_not_revoked(&tx, proposal.from_publisher_id)?;
        let from_leaf = load_leaf_by_publisher_and_pubkey(
            &tx,
            proposal.from_publisher_id,
            &proposal.from_leaf_public_key,
        )?
        .ok_or(PublisherTrustCheckError::UntrustedCertificateChain(
            "outgoing owner signing key is not bound to from_publisher_id",
        ))?;
        ensure_not_revoked_serial_or_key(
            &tx,
            from_leaf.cert.serial_id,
            from_leaf.cert.leaf_key_id,
        )?;
        if from_leaf.status != LeafCertificateStatus::Active {
            return Err(PublisherTrustCheckError::RevokedCertificate {
                serial_or_key: from_leaf.cert.serial_id,
                revoked_at_epoch: from_leaf.compromised_at_epoch,
            });
        }
        verify_leaf_chain_in_tx(&tx, &from_leaf.cert, Some(proposal.transferred_at_ms))?;

        // Verify incoming owner leaf key, claimed trust tier & signature.
        ensure_publisher_not_revoked(&tx, proposal.to_publisher_id)?;
        let to_leaf = load_leaf_by_publisher_and_pubkey(
            &tx,
            proposal.to_publisher_id,
            &proposal.to_leaf_public_key,
        )?
        .ok_or(PublisherTrustCheckError::UnknownPublisher(
            proposal.to_publisher_id,
        ))?;
        ensure_not_revoked_serial_or_key(&tx, to_leaf.cert.serial_id, to_leaf.cert.leaf_key_id)?;
        if to_leaf.status != LeafCertificateStatus::Active {
            return Err(PublisherTrustCheckError::RevokedCertificate {
                serial_or_key: to_leaf.cert.serial_id,
                revoked_at_epoch: to_leaf.compromised_at_epoch,
            });
        }
        if to_leaf.cert.trust_tier != proposal.to_trust_tier {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "incoming owner leaf certificate trust_tier disagrees with transfer proposal",
            ));
        }
        verify_leaf_chain_in_tx(&tx, &to_leaf.cert, Some(proposal.transferred_at_ms))?;

        // Verify Store Intermediate CA co-signer.
        let intermediate = load_intermediate(&tx, proposal.intermediate_serial_id)?.ok_or(
            PublisherTrustCheckError::UntrustedCertificateChain(
                "ownership transfer co-signing intermediate CA is not registered",
            ),
        )?;
        ensure_not_revoked_serial_or_key(
            &tx,
            intermediate.serial_id,
            intermediate.intermediate_key_id,
        )?;
        let root = load_root_anchor(&tx, intermediate.issuer_root_serial_id)?.ok_or(
            PublisherTrustCheckError::UntrustedCertificateChain(
                "ownership transfer intermediate CA root anchor is missing",
            ),
        )?;
        intermediate.verify_against_root(&root, Some(proposal.transferred_at_ms))?;
        if !intermediate.can_issue_tier(proposal.to_trust_tier) {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "co-signing intermediate CA tier ceiling forbids target ownership tier",
            ));
        }

        // Verify all 3 role-separated Ed25519 signatures.
        if !verify_strict_digest_sig(
            &proposal.from_leaf_public_key,
            &proposal.from_owner_signing_digest(),
            &receipt.from_owner_signature,
        ) || !verify_strict_digest_sig(
            &proposal.to_leaf_public_key,
            &proposal.to_owner_signing_digest(),
            &receipt.to_owner_signature,
        ) || !verify_strict_digest_sig(
            &intermediate.intermediate_public_key,
            &proposal.store_ca_signing_digest(),
            &receipt.store_ca_signature,
        ) {
            return Err(PublisherTrustCheckError::SignatureInvalid);
        }

        let new_generation = current
            .registration_generation
            .checked_add(1)
            .ok_or_else(|| {
                PublisherTrustCheckError::StorageFailure(
                    "registration_generation overflow".to_owned(),
                )
            })?;

        // Insert immutable transfer receipt FIRST so the `application_ownership_bindings_transfer_gated`
        // BEFORE UPDATE trigger sees the verified 3-party audit record.
        tx.execute(
            "INSERT INTO application_ownership_transfers (
                receipt_id, application_id, prior_generation, new_generation,
                from_publisher_id, from_leaf_public_key, to_publisher_id,
                to_trust_tier, to_leaf_public_key, intermediate_serial_id,
                from_owner_signature, to_owner_signature, store_ca_signature,
                transferred_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                receipt.receipt_id.as_bytes().as_slice(),
                proposal.application_id.as_bytes().as_slice(),
                encode_u64(current.registration_generation)?,
                encode_u64(new_generation)?,
                proposal.from_publisher_id.as_bytes().as_slice(),
                proposal.from_leaf_public_key.as_slice(),
                proposal.to_publisher_id.as_bytes().as_slice(),
                i64::from(proposal.to_trust_tier.encode()),
                proposal.to_leaf_public_key.as_slice(),
                proposal.intermediate_serial_id.as_slice(),
                receipt.from_owner_signature.as_slice(),
                receipt.to_owner_signature.as_slice(),
                receipt.store_ca_signature.as_slice(),
                encode_u64(proposal.transferred_at_ms)?,
            ],
        )
        .map_err(sql_err)?;

        tx.execute(
            "UPDATE application_ownership_bindings
             SET owner_publisher_id = ?2,
                 trust_tier = ?3,
                 updated_at_ms = ?4,
                 registration_generation = ?5
             WHERE application_id = ?1",
            params![
                proposal.application_id.as_bytes().as_slice(),
                proposal.to_publisher_id.as_bytes().as_slice(),
                i64::from(proposal.to_trust_tier.encode()),
                encode_u64(proposal.transferred_at_ms)?,
                encode_u64(new_generation)?,
            ],
        )
        .map_err(sql_err)?;

        tx.commit().map_err(sql_err)?;
        Ok(ApplicationOwnershipBinding {
            application_id: proposal.application_id,
            owner_publisher_id: proposal.to_publisher_id,
            trust_tier: proposal.to_trust_tier,
            first_registered_at_ms: current.first_registered_at_ms,
            registration_generation: new_generation,
        })
    }

    /// Lists all immutable [`SignedOwnershipTransferReceipt`] audit rows for `application_id`
    /// in ascending `new_generation` order.
    ///
    /// # Errors
    ///
    /// Returns [`PublisherTrustCheckError::StorageFailure`] on corrupt storage.
    pub fn inspect_ownership_transfers(
        &self,
        application_id: ApplicationId,
    ) -> Result<Vec<SignedOwnershipTransferReceipt>, PublisherTrustCheckError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT receipt_id, application_id, prior_generation, from_publisher_id,
                        from_leaf_public_key, to_publisher_id, to_trust_tier,
                        to_leaf_public_key, intermediate_serial_id, from_owner_signature,
                        to_owner_signature, store_ca_signature, transferred_at_ms
                 FROM application_ownership_transfers
                 WHERE application_id = ?1
                 ORDER BY new_generation ASC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([application_id.as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                    row.get::<_, Vec<u8>>(8)?,
                    row.get::<_, Vec<u8>>(9)?,
                    row.get::<_, Vec<u8>>(10)?,
                    row.get::<_, Vec<u8>>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            })
            .map_err(sql_err)?;

        let mut out = Vec::new();
        for row in rows {
            let (
                rid,
                app,
                prior_gen,
                from_pub,
                from_pk,
                to_pub,
                to_tier_i64,
                to_pk,
                inter_serial,
                from_sig,
                to_sig,
                ca_sig,
                ts,
            ) = row.map_err(sql_err)?;
            let to_trust_tier = PublisherTrustTier::decode(to_tier_i64).ok_or_else(|| {
                PublisherTrustCheckError::StorageFailure(format!(
                    "corrupt to_trust_tier={to_tier_i64} in application_ownership_transfers"
                ))
            })?;
            out.push(SignedOwnershipTransferReceipt {
                receipt_id: ReceiptId::from_bytes(fixed16(rid, "receipt_id")?),
                proposal: OwnershipTransferProposal {
                    application_id: ApplicationId::from_bytes(fixed16(app, "application_id")?),
                    expected_current_generation: decode_u64(prior_gen, "prior_generation")?,
                    from_publisher_id: PublisherId::from_bytes(fixed16(
                        from_pub,
                        "from_publisher_id",
                    )?),
                    from_leaf_public_key: fixed32(from_pk, "from_leaf_public_key")?,
                    to_publisher_id: PublisherId::from_bytes(fixed16(to_pub, "to_publisher_id")?),
                    to_trust_tier,
                    to_leaf_public_key: fixed32(to_pk, "to_leaf_public_key")?,
                    intermediate_serial_id: fixed16(inter_serial, "intermediate_serial_id")?,
                    transferred_at_ms: decode_u64(ts, "transferred_at_ms")?,
                },
                from_owner_signature: fixed64(from_sig, "from_owner_signature")?,
                to_owner_signature: fixed64(to_sig, "to_owner_signature")?,
                store_ca_signature: fixed64(ca_sig, "store_ca_signature")?,
            });
        }
        Ok(out)
    }
}

impl PublisherTrustVerifier for SqlitePublisherTrustAuthority {
    fn current_revocation_epoch(&self) -> Result<RevocationEpoch, PublisherTrustCheckError> {
        Ok(self.inspect_revocation_state()?.revocation_epoch)
    }

    fn verify_publisher_signature(
        &self,
        publisher_id: PublisherId,
        signing_public_key: &[u8; 32],
        message_digest: &[u8; 32],
        signature: &[u8; 64],
        now_ms: u64,
    ) -> Result<VerifiedPublisherIdentity, PublisherTrustCheckError> {
        self.verify_publisher_signature_with_mode(
            publisher_id,
            signing_public_key,
            message_digest,
            signature,
            now_ms,
            TrustFreshnessMode::OnlineInstallOrUpgrade,
        )
    }

    fn verify_or_bind_application_owner(
        &self,
        publisher_id: PublisherId,
        trust_tier: PublisherTrustTier,
        application_id: ApplicationId,
        now_ms: u64,
    ) -> Result<ApplicationOwnershipBinding, PublisherTrustCheckError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;

        ensure_publisher_not_revoked(&tx, publisher_id)?;

        if let Some(existing) = load_application_owner(&tx, application_id)? {
            if existing.owner_publisher_id != publisher_id {
                return Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
                    application_id,
                    registered_owner: existing.owner_publisher_id,
                    attempted_publisher: publisher_id,
                });
            }
            // Owner matches: ensure the owner still holds at least one unrevoked certificate.
            ensure_publisher_has_active_certificate_for_tier(&tx, publisher_id, None)?;
            tx.commit().map_err(sql_err)?;
            return Ok(existing);
        }

        // First registration: publisher must be registered and hold an active unrevoked
        // certificate matching `trust_tier`.
        ensure_publisher_has_active_certificate_for_tier(&tx, publisher_id, Some(trust_tier))?;

        let binding = ApplicationOwnershipBinding {
            application_id,
            owner_publisher_id: publisher_id,
            trust_tier,
            first_registered_at_ms: now_ms,
            registration_generation: 1,
        };
        tx.execute(
            "INSERT INTO application_ownership_bindings (
                application_id, owner_publisher_id, trust_tier,
                first_registered_at_ms, updated_at_ms, registration_generation
             ) VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![
                application_id.as_bytes().as_slice(),
                publisher_id.as_bytes().as_slice(),
                i64::from(trust_tier.encode()),
                encode_u64(now_ms)?,
                encode_u64(now_ms)?,
            ],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(binding)
    }
}

// ---------------------------------------------------------------------------
// Internal SQLite query and chain-verification helpers
// ---------------------------------------------------------------------------

fn load_root_anchor(
    conn: &Connection,
    root_serial_id: [u8; 16],
) -> Result<Option<OfflinePlatformRootCa>, PublisherTrustCheckError> {
    let raw = conn
        .query_row(
            "SELECT root_serial_id, root_key_id, root_public_key, public_key_fingerprint,
                    not_before_ms, expires_at_ms, self_signature
             FROM trust_root_anchors WHERE root_serial_id = ?1",
            [root_serial_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(sql_err)?;

    let Some((serial, kid, pk, fp, nb, exp, sig)) = raw else {
        return Ok(None);
    };
    Ok(Some(OfflinePlatformRootCa {
        root_serial_id: fixed16(serial, "root_serial_id")?,
        root_key_id: KeyId::from_bytes(fixed16(kid, "root_key_id")?),
        root_public_key: fixed32(pk, "root_public_key")?,
        public_key_fingerprint: fixed32(fp, "public_key_fingerprint")?,
        not_before_ms: decode_u64(nb, "not_before_ms")?,
        expires_at_ms: decode_u64(exp, "expires_at_ms")?,
        self_signature: fixed64(sig, "self_signature")?,
    }))
}

fn load_intermediate(
    conn: &Connection,
    serial_id: [u8; 16],
) -> Result<Option<StoreIntermediateCertificate>, PublisherTrustCheckError> {
    let raw = conn
        .query_row(
            "SELECT serial_id, issuer_root_serial_id, intermediate_key_id,
                    intermediate_public_key, max_issuable_tier, not_before_ms,
                    expires_at_ms, root_signature
             FROM store_intermediate_certificates WHERE serial_id = ?1",
            [serial_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                ))
            },
        )
        .optional()
        .map_err(sql_err)?;

    let Some((serial, root_serial, kid, pk, tier_i64, nb, exp, sig)) = raw else {
        return Ok(None);
    };
    let max_issuable_tier = PublisherTrustTier::decode(tier_i64).ok_or_else(|| {
        PublisherTrustCheckError::StorageFailure(format!(
            "corrupt max_issuable_tier={tier_i64} in store_intermediate_certificates"
        ))
    })?;
    Ok(Some(StoreIntermediateCertificate {
        serial_id: fixed16(serial, "serial_id")?,
        issuer_root_serial_id: fixed16(root_serial, "issuer_root_serial_id")?,
        intermediate_key_id: KeyId::from_bytes(fixed16(kid, "intermediate_key_id")?),
        intermediate_public_key: fixed32(pk, "intermediate_public_key")?,
        max_issuable_tier,
        not_before_ms: decode_u64(nb, "not_before_ms")?,
        expires_at_ms: decode_u64(exp, "expires_at_ms")?,
        root_signature: fixed64(sig, "root_signature")?,
    }))
}

type RawStoredLeafTuple = (
    Vec<u8>,
    Vec<u8>,
    i64,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
);

fn decode_stored_leaf_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawStoredLeafTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn parse_stored_leaf_row(
    raw: RawStoredLeafTuple,
) -> Result<StoredLeafRow, PublisherTrustCheckError> {
    let (serial, pub_id, tier_i64, kid, pk, nb, exp, issuer, sig, status_i64, comp_epoch) = raw;
    let trust_tier = PublisherTrustTier::decode(tier_i64).ok_or_else(|| {
        PublisherTrustCheckError::StorageFailure(format!(
            "corrupt trust_tier={tier_i64} in publisher_leaf_certificates"
        ))
    })?;
    let status = LeafCertificateStatus::decode(status_i64).map_err(|_| {
        PublisherTrustCheckError::StorageFailure(format!(
            "corrupt status={status_i64} in publisher_leaf_certificates"
        ))
    })?;
    Ok(StoredLeafRow {
        cert: PublisherLeafCertificate {
            serial_id: fixed16(serial, "serial_id")?,
            publisher_id: PublisherId::from_bytes(fixed16(pub_id, "publisher_id")?),
            trust_tier,
            leaf_key_id: KeyId::from_bytes(fixed16(kid, "leaf_key_id")?),
            leaf_public_key: fixed32(pk, "leaf_public_key")?,
            not_before_ms: decode_u64(nb, "not_before_ms")?,
            expires_at_ms: decode_u64(exp, "expires_at_ms")?,
            issuer_serial_id: fixed16(issuer, "issuer_serial_id")?,
            signature: fixed64(sig, "signature")?,
        },
        status,
        compromised_at_epoch: RevocationEpoch::new(decode_u64(comp_epoch, "compromised_at_epoch")?),
    })
}

fn load_leaf_by_serial(
    conn: &Connection,
    serial_id: [u8; 16],
) -> Result<Option<StoredLeafRow>, PublisherTrustCheckError> {
    let raw = conn
        .query_row(
            "SELECT serial_id, publisher_id, trust_tier, leaf_key_id, leaf_public_key,
                    not_before_ms, expires_at_ms, issuer_serial_id, signature,
                    status, compromised_at_epoch
             FROM publisher_leaf_certificates WHERE serial_id = ?1",
            [serial_id.as_slice()],
            decode_stored_leaf_row,
        )
        .optional()
        .map_err(sql_err)?;
    raw.map(parse_stored_leaf_row).transpose()
}

fn load_leaf_by_publisher_and_pubkey(
    conn: &Connection,
    publisher_id: PublisherId,
    signing_public_key: &[u8; 32],
) -> Result<Option<StoredLeafRow>, PublisherTrustCheckError> {
    let raw = conn
        .query_row(
            "SELECT serial_id, publisher_id, trust_tier, leaf_key_id, leaf_public_key,
                    not_before_ms, expires_at_ms, issuer_serial_id, signature,
                    status, compromised_at_epoch
             FROM publisher_leaf_certificates
             WHERE publisher_id = ?1 AND leaf_public_key = ?2",
            params![
                publisher_id.as_bytes().as_slice(),
                signing_public_key.as_slice(),
            ],
            decode_stored_leaf_row,
        )
        .optional()
        .map_err(sql_err)?;
    raw.map(parse_stored_leaf_row).transpose()
}

fn publisher_has_any_certificate(
    conn: &Connection,
    publisher_id: PublisherId,
) -> Result<bool, PublisherTrustCheckError> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM publisher_leaf_certificates WHERE publisher_id = ?1 LIMIT 1",
            [publisher_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(exists.is_some())
}

fn ensure_publisher_has_active_certificate_for_tier(
    conn: &Connection,
    publisher_id: PublisherId,
    required_tier: Option<PublisherTrustTier>,
) -> Result<(), PublisherTrustCheckError> {
    let mut stmt = conn
        .prepare(
            "SELECT serial_id, publisher_id, trust_tier, leaf_key_id, leaf_public_key,
                    not_before_ms, expires_at_ms, issuer_serial_id, signature,
                    status, compromised_at_epoch
             FROM publisher_leaf_certificates
             WHERE publisher_id = ?1",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([publisher_id.as_bytes().as_slice()], decode_stored_leaf_row)
        .map_err(sql_err)?;

    let mut saw_any = false;
    let mut saw_unrevoked = false;
    let mut last_revoked: Option<PublisherTrustCheckError> = None;

    for row in rows {
        saw_any = true;
        let parsed = parse_stored_leaf_row(row.map_err(sql_err)?)?;
        if parsed.status == LeafCertificateStatus::Compromised {
            last_revoked = Some(PublisherTrustCheckError::RevokedCertificate {
                serial_or_key: parsed.cert.serial_id,
                revoked_at_epoch: parsed.compromised_at_epoch,
            });
            continue;
        }
        if let Err(err) =
            ensure_not_revoked_serial_or_key(conn, parsed.cert.serial_id, parsed.cert.leaf_key_id)
        {
            last_revoked = Some(err);
            continue;
        }
        if parsed.status == LeafCertificateStatus::Active {
            saw_unrevoked = true;
            if required_tier.is_none_or(|tier| parsed.cert.trust_tier == tier) {
                return Ok(());
            }
        }
    }

    if !saw_any {
        return Err(PublisherTrustCheckError::UnknownPublisher(publisher_id));
    }
    if !saw_unrevoked && let Some(err) = last_revoked {
        return Err(err);
    }
    Err(PublisherTrustCheckError::UntrustedCertificateChain(
        "publisher has no active unrevoked leaf certificate for claimed trust tier",
    ))
}

fn insert_leaf_row(
    conn: &Connection,
    cert: &PublisherLeafCertificate,
    status: LeafCertificateStatus,
    compromised_at_epoch: u64,
    registered_at_ms: u64,
) -> Result<(), PublisherTrustCheckError> {
    conn.execute(
        "INSERT INTO publisher_leaf_certificates (
            serial_id, publisher_id, trust_tier, leaf_key_id, leaf_public_key,
            not_before_ms, expires_at_ms, issuer_serial_id, signature,
            status, compromised_at_epoch, registered_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            cert.serial_id.as_slice(),
            cert.publisher_id.as_bytes().as_slice(),
            i64::from(cert.trust_tier.encode()),
            cert.leaf_key_id.as_bytes().as_slice(),
            cert.leaf_public_key.as_slice(),
            encode_u64(cert.not_before_ms)?,
            encode_u64(cert.expires_at_ms)?,
            cert.issuer_serial_id.as_slice(),
            cert.signature.as_slice(),
            status.encode(),
            encode_u64(compromised_at_epoch)?,
            encode_u64(registered_at_ms)?,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn verify_leaf_chain_in_tx(
    conn: &Connection,
    cert: &PublisherLeafCertificate,
    now_ms: Option<u64>,
) -> Result<(), PublisherTrustCheckError> {
    if cert.is_self_signed() {
        return cert.verify_self_signed_community(now_ms);
    }
    let intermediate = load_intermediate(conn, cert.issuer_serial_id)?.ok_or(
        PublisherTrustCheckError::UntrustedCertificateChain(
            "leaf certificate issuer_serial_id is not a registered StoreIntermediateCertificate",
        ),
    )?;
    ensure_not_revoked_serial_or_key(
        conn,
        intermediate.serial_id,
        intermediate.intermediate_key_id,
    )?;
    let root = load_root_anchor(conn, intermediate.issuer_root_serial_id)?.ok_or(
        PublisherTrustCheckError::UntrustedCertificateChain(
            "intermediate CA issuer_root_serial_id is not an anchored platform root CA",
        ),
    )?;
    cert.verify_against_intermediate(&intermediate, &root, now_ms)
}

fn verify_crl_issuer_signature(
    conn: &Connection,
    crl: &SignedRevocationList,
    crl_digest: &[u8; 32],
) -> Result<(), PublisherTrustCheckError> {
    match crl.issuer {
        CrlIssuerKind::RootCa { root_serial_id } => {
            let root = load_root_anchor(conn, root_serial_id)?.ok_or(
                PublisherTrustCheckError::UntrustedCertificateChain(
                    "CRL root_serial_id is not an anchored platform root CA",
                ),
            )?;
            root.verify_self_signed(Some(crl.issued_at_ms))?;
            if !verify_strict_digest_sig(&root.root_public_key, crl_digest, &crl.signature) {
                return Err(PublisherTrustCheckError::SignatureInvalid);
            }
            Ok(())
        }
        CrlIssuerKind::StoreIntermediate {
            intermediate_serial_id,
        } => {
            let intermediate = load_intermediate(conn, intermediate_serial_id)?.ok_or(
                PublisherTrustCheckError::UntrustedCertificateChain(
                    "CRL intermediate_serial_id is not a registered StoreIntermediateCertificate",
                ),
            )?;
            ensure_not_revoked_serial_or_key(
                conn,
                intermediate.serial_id,
                intermediate.intermediate_key_id,
            )?;
            if crl.revoked_certificates.iter().any(|entry| {
                entry.serial_or_key == intermediate.serial_id
                    || entry.serial_or_key == intermediate.intermediate_key_id.into_bytes()
            }) {
                return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                    "intermediate CA cannot sign a CRL that revokes itself; root CA must sign",
                ));
            }
            let root = load_root_anchor(conn, intermediate.issuer_root_serial_id)?.ok_or(
                PublisherTrustCheckError::UntrustedCertificateChain(
                    "CRL intermediate CA root anchor is missing",
                ),
            )?;
            intermediate.verify_against_root(&root, Some(crl.issued_at_ms))?;
            if !verify_strict_digest_sig(
                &intermediate.intermediate_public_key,
                crl_digest,
                &crl.signature,
            ) {
                return Err(PublisherTrustCheckError::SignatureInvalid);
            }
            Ok(())
        }
    }
}

fn ensure_publisher_not_revoked(
    conn: &Connection,
    publisher_id: PublisherId,
) -> Result<(), PublisherTrustCheckError> {
    let epoch_i64: Option<i64> = conn
        .query_row(
            "SELECT revoked_at_epoch FROM revoked_publishers WHERE publisher_id = ?1",
            [publisher_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if let Some(raw_epoch) = epoch_i64 {
        return Err(PublisherTrustCheckError::RevokedPublisher {
            publisher_id,
            revoked_at_epoch: RevocationEpoch::new(decode_u64(raw_epoch, "revoked_at_epoch")?),
        });
    }
    Ok(())
}

fn ensure_not_revoked_serial_or_key(
    conn: &Connection,
    serial_id: [u8; 16],
    key_id: KeyId,
) -> Result<(), PublisherTrustCheckError> {
    for candidate in [serial_id, key_id.into_bytes()] {
        let epoch_i64: Option<i64> = conn
            .query_row(
                "SELECT revoked_at_epoch FROM revoked_certificates WHERE serial_or_key = ?1",
                [candidate.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some(raw_epoch) = epoch_i64 {
            return Err(PublisherTrustCheckError::RevokedCertificate {
                serial_or_key: candidate,
                revoked_at_epoch: RevocationEpoch::new(decode_u64(raw_epoch, "revoked_at_epoch")?),
            });
        }
    }
    Ok(())
}

fn insert_revoked_cert_entry(
    conn: &Connection,
    serial_or_key: [u8; 16],
    revoked_at_epoch: u64,
    reason: RevocationReason,
    recorded_at_ms: u64,
) -> Result<(), PublisherTrustCheckError> {
    conn.execute(
        "INSERT INTO revoked_certificates (serial_or_key, revoked_at_epoch, reason_code, recorded_at_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(serial_or_key) DO NOTHING",
        params![
            serial_or_key.as_slice(),
            encode_u64(revoked_at_epoch)?,
            i64::from(reason.encode()),
            encode_u64(recorded_at_ms)?,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn load_revocation_state(
    conn: &Connection,
) -> Result<RevocationStateSnapshot, PublisherTrustCheckError> {
    let (epoch_i64, issued_i64, fresh_i64, digest_vec) = conn
        .query_row(
            "SELECT revocation_epoch, issued_at_ms, fresh_until_ms, crl_digest
             FROM trust_revocation_state WHERE singleton_id = ?1",
            [REVOCATION_STATE_SINGLETON_ID],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            },
        )
        .map_err(sql_err)?;
    Ok(RevocationStateSnapshot {
        revocation_epoch: RevocationEpoch::new(decode_u64(epoch_i64, "revocation_epoch")?),
        issued_at_ms: decode_u64(issued_i64, "issued_at_ms")?,
        fresh_until_ms: decode_u64(fresh_i64, "fresh_until_ms")?,
        crl_digest: fixed32(digest_vec, "crl_digest")?,
    })
}

fn rotation_receipt_exists(
    conn: &Connection,
    receipt_id: ReceiptId,
) -> Result<bool, PublisherTrustCheckError> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM publisher_key_rotations WHERE receipt_id = ?1",
            [receipt_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(exists.is_some())
}

fn load_application_owner(
    conn: &Connection,
    application_id: ApplicationId,
) -> Result<Option<ApplicationOwnershipBinding>, PublisherTrustCheckError> {
    let raw = conn
        .query_row(
            "SELECT application_id, owner_publisher_id, trust_tier,
                    first_registered_at_ms, registration_generation
             FROM application_ownership_bindings WHERE application_id = ?1",
            [application_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_err)?;

    let Some((app, owner, tier_i64, first_ms, gen_i64)) = raw else {
        return Ok(None);
    };
    let trust_tier = PublisherTrustTier::decode(tier_i64).ok_or_else(|| {
        PublisherTrustCheckError::StorageFailure(format!(
            "corrupt trust_tier={tier_i64} in application_ownership_bindings"
        ))
    })?;
    Ok(Some(ApplicationOwnershipBinding {
        application_id: ApplicationId::from_bytes(fixed16(app, "application_id")?),
        owner_publisher_id: PublisherId::from_bytes(fixed16(owner, "owner_publisher_id")?),
        trust_tier,
        first_registered_at_ms: decode_u64(first_ms, "first_registered_at_ms")?,
        registration_generation: decode_u64(gen_i64, "registration_generation")?,
    }))
}

fn load_package_release(
    conn: &Connection,
    package_id: PackageId,
) -> Result<Option<PackageReleaseBinding>, PublisherTrustCheckError> {
    let raw = conn
        .query_row(
            "SELECT package_id, application_id, publisher_id, manifest_digest, registered_at_ms
             FROM package_release_bindings WHERE package_id = ?1",
            [package_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_err)?;

    let Some((pkg, app, pub_id, digest, ts)) = raw else {
        return Ok(None);
    };
    Ok(Some(PackageReleaseBinding {
        package_id: PackageId::from_bytes(fixed16(pkg, "package_id")?),
        application_id: ApplicationId::from_bytes(fixed16(app, "application_id")?),
        publisher_id: PublisherId::from_bytes(fixed16(pub_id, "publisher_id")?),
        manifest_digest: fixed32(digest, "manifest_digest")?,
        registered_at_ms: decode_u64(ts, "registered_at_ms")?,
    }))
}
