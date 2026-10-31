//! Durable `SQLite` store governance catalog, entitlement token verifier, and
//! three-tier recall enforcement coordinator (`ADR-0027` section 2.3;
//! `[PKG-OCI-001]`, `[PKG-MULTI-001]`, `[PKG-APPID-001]`, `[TUI-CONSENT-001]`,
//! `[CAP-REVOKE-001]`, `[NLOS-DEFER-001]`).

use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use ed25519_dalek::VerifyingKey;
use nlos_application::{ApplicationStatus, DisableApplicationRequest};
use nlos_capability::{
    CapabilityAuthority, CapabilityHandle, RevokeCapabilityRequest, SignedRevokeCapabilityRequest,
    revoke_command_message,
};
use nlos_migrate::EffectClass;
use nlos_publisher_trust::{
    ApplicationOwnershipBinding, PublisherId, PublisherTrustCheckError, PublisherTrustTier,
    PublisherTrustVerifier, RevocationEpoch,
};
use nlos_types::{
    ApplicationId, CapabilityId, Generation, IdempotencyKey, InstallationId, KeyId, PackageId,
    PrincipalId, ReceiptId,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::catalog::{
    REQUIRED_CERTIFICATION_GATES_MASK, SignedCertificationAttestation, StoreCatalogTrack,
    StoreCertificationPromotionReceipt, StoreChannel, StoreComponentKind, StoreComponentSlot,
    StorePromotionDecision, StoreReleaseRecord, StoreReleaseSubmission,
};
use crate::entitlement::{
    EntitlementGrantReceipt, EntitlementRedeemDecision, RedeemEntitlementRequest,
};
use crate::error::{OciCasError, StoreRegistryError};
use crate::oci_cas::LocalOciCasStore;
use crate::recall::{
    GrantedCapabilityBindingRecord, LocalLaunchDisposition, RecallApplyDecision,
    RecallEnforcementAuthorities, RecallExecutionReceipt, RecallSeverity,
    RegisterGrantedCapabilityRequest, SignedRecallAdvisory,
};
use crate::schema::{self, SCHEMA_VERSION};
use crate::transparency::{
    DeclaredPermissionEntry, PermissionEscalationDiff, PermissionTransparencyReport,
    decode_effect_class, diff_permission_transparency, encode_effect_class,
};

const PROMOTION_REQUEST_DOMAIN: &[u8] = b"nlos/store-registry/promotion-request/v1";
const PROMOTION_RECEIPT_ID_DOMAIN: &[u8] = b"nlos/store-registry/promotion-receipt-id/v1";
const ENTITLEMENT_RECEIPT_ID_DOMAIN: &[u8] = b"nlos/store-registry/entitlement-receipt-id/v1";
const RECALL_RECEIPT_ID_DOMAIN: &[u8] = b"nlos/store-registry/recall-receipt-id/v1";
const CAP_REVOKE_IDEMPOTENCY_DOMAIN: &[u8] = b"nlos/store-registry/cap-revoke-idempotency/v1";
const APP_DISABLE_IDEMPOTENCY_DOMAIN: &[u8] = b"nlos/store-registry/app-disable-idempotency/v1";

/// Immutable root cryptographic trust anchors and initial revocation epoch
/// persisted inside `<root>/store-registry.db`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreGovernanceAnchors {
    /// `Ed25519` public key verifying four-gate [`SignedCertificationAttestation`]s.
    pub certifier_public_key: [u8; 32],
    /// Expected store entitlement signing [`KeyId`].
    pub entitlement_issuer_key_id: KeyId,
    /// `Ed25519` public key verifying [`crate::SignedEntitlementToken`]s.
    pub entitlement_issuer_public_key: [u8; 32],
    /// `Ed25519` public key verifying store security [`SignedRecallAdvisory`]s.
    pub recall_authority_public_key: [u8; 32],
    /// Initial monotonic `store_revocation_epoch` when bootstrapping a fresh DB.
    pub initial_revocation_epoch: RevocationEpoch,
    /// Initialization timestamp (`ms` since Unix epoch).
    pub initialized_at_ms: u64,
}

impl StoreGovernanceAnchors {
    fn validate_public_keys(&self) -> Result<(), StoreRegistryError> {
        VerifyingKey::from_bytes(&self.certifier_public_key)
            .map_err(|_| StoreRegistryError::InvalidPublicKey("certifier_public_key"))?;
        VerifyingKey::from_bytes(&self.entitlement_issuer_public_key)
            .map_err(|_| StoreRegistryError::InvalidPublicKey("entitlement_issuer_public_key"))?;
        VerifyingKey::from_bytes(&self.recall_authority_public_key)
            .map_err(|_| StoreRegistryError::InvalidPublicKey("recall_authority_public_key"))?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DurableGovernanceMeta {
    store_revocation_epoch: RevocationEpoch,
    certifier_public_key: [u8; 32],
    entitlement_issuer_key_id: KeyId,
    entitlement_issuer_public_key: [u8; 32],
    recall_authority_public_key: [u8; 32],
    updated_at_ms: u64,
}

/// Durable `SQLite`-backed (`WAL + FULL`) store governance catalog, entitlement
/// verification ledger, and recall enforcement authority (`ADR-0027` section 2.3).
pub struct SqliteStoreRegistry {
    connection: Mutex<Connection>,
}

impl SqliteStoreRegistry {
    /// Opens or creates `<root>/store-registry.db` with `WAL + FULL` durability
    /// and initializes (or verifies) the durable [`StoreGovernanceAnchors`].
    ///
    /// # Errors
    ///
    /// Fails closed if any `Ed25519` trust anchor is malformed, `WAL + FULL`
    /// durability cannot be established, schema version is unsupported, or
    /// supplied anchors conflict with already-persisted trust anchors.
    pub fn open(
        root: impl AsRef<Path>,
        anchors: StoreGovernanceAnchors,
    ) -> Result<Self, StoreRegistryError> {
        anchors.validate_public_keys()?;
        let mut connection = open_configured_connection(root.as_ref())?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => schema::migrate_v1(&mut connection)?,
            SCHEMA_VERSION => {}
            other => return Err(StoreRegistryError::SchemaVersionUnsupported(other)),
        }

        {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(existing) = load_governance_meta_optional(&transaction)? {
                if existing.certifier_public_key != anchors.certifier_public_key {
                    return Err(StoreRegistryError::GovernanceAnchorMismatch(
                        "certifier_public_key",
                    ));
                }
                if existing.entitlement_issuer_key_id != anchors.entitlement_issuer_key_id {
                    return Err(StoreRegistryError::GovernanceAnchorMismatch(
                        "entitlement_issuer_key_id",
                    ));
                }
                if existing.entitlement_issuer_public_key != anchors.entitlement_issuer_public_key {
                    return Err(StoreRegistryError::GovernanceAnchorMismatch(
                        "entitlement_issuer_public_key",
                    ));
                }
                if existing.recall_authority_public_key != anchors.recall_authority_public_key {
                    return Err(StoreRegistryError::GovernanceAnchorMismatch(
                        "recall_authority_public_key",
                    ));
                }
                if existing.store_revocation_epoch < anchors.initial_revocation_epoch {
                    return Err(StoreRegistryError::GovernanceAnchorMismatch(
                        "initial_revocation_epoch exceeds durable store_revocation_epoch",
                    ));
                }
            } else {
                transaction.execute(
                    "INSERT INTO store_governance_meta (
                        singleton_id, store_revocation_epoch, certifier_public_key,
                        entitlement_issuer_key_id, entitlement_issuer_public_key,
                        recall_authority_public_key, updated_at_ms
                     ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        encode_u64(anchors.initial_revocation_epoch.get())?,
                        anchors.certifier_public_key.as_slice(),
                        anchors.entitlement_issuer_key_id.as_bytes().as_slice(),
                        anchors.entitlement_issuer_public_key.as_slice(),
                        anchors.recall_authority_public_key.as_slice(),
                        encode_u64(anchors.initialized_at_ms)?,
                    ],
                )?;
            }
            transaction.commit()?;
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Reopens an existing initialized `<root>/store-registry.db` using its
    /// durable trust anchors on disk.
    ///
    /// # Errors
    ///
    /// Fails closed if the database has not been initialized, `WAL + FULL`
    /// durability is unavailable, or durable trust anchors are corrupt.
    pub fn reopen(root: impl AsRef<Path>) -> Result<Self, StoreRegistryError> {
        let connection = open_configured_connection(root.as_ref())?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => return Err(StoreRegistryError::GovernanceUninitialized),
            SCHEMA_VERSION => {}
            other => return Err(StoreRegistryError::SchemaVersionUnsupported(other)),
        }
        let meta = load_governance_meta(&connection)?;
        let anchors = StoreGovernanceAnchors {
            certifier_public_key: meta.certifier_public_key,
            entitlement_issuer_key_id: meta.entitlement_issuer_key_id,
            entitlement_issuer_public_key: meta.entitlement_issuer_public_key,
            recall_authority_public_key: meta.recall_authority_public_key,
            initial_revocation_epoch: meta.store_revocation_epoch,
            initialized_at_ms: meta.updated_at_ms,
        };
        anchors.validate_public_keys()?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Reads the current durable monotonic `store_revocation_epoch`.
    ///
    /// # Errors
    ///
    /// Fails closed on mutex poison, uninitialized metadata, or storage failure.
    pub fn current_store_revocation_epoch(&self) -> Result<RevocationEpoch, StoreRegistryError> {
        let connection = self.lock()?;
        Ok(load_governance_meta(&connection)?.store_revocation_epoch)
    }

    /// Reads the durable [`StoreGovernanceAnchors`] snapshot (reporting the
    /// current `store_revocation_epoch` and last metadata update timestamp).
    ///
    /// # Errors
    ///
    /// Fails closed on mutex poison, uninitialized metadata, or storage failure.
    pub fn governance_anchors(&self) -> Result<StoreGovernanceAnchors, StoreRegistryError> {
        let connection = self.lock()?;
        let meta = load_governance_meta(&connection)?;
        Ok(StoreGovernanceAnchors {
            certifier_public_key: meta.certifier_public_key,
            entitlement_issuer_key_id: meta.entitlement_issuer_key_id,
            entitlement_issuer_public_key: meta.entitlement_issuer_public_key,
            recall_authority_public_key: meta.recall_authority_public_key,
            initial_revocation_epoch: meta.store_revocation_epoch,
            initialized_at_ms: meta.updated_at_ms,
        })
    }

    /// Admits a verified multi-component composite release into the store
    /// governance catalog (`StoreChannel::Candidate`) after enforcing:
    ///
    /// 1. Structural multi-component & transparency validation (`[PKG-MULTI-001]`).
    /// 2. Content-addressed `OCI` `CAS` layout and component layer verification (`[PKG-OCI-001]`).
    /// 3. Cryptographic publisher signature & trust chain verification (`PublisherTrustVerifier`).
    /// 4. Anti-hijack `ApplicationId -> PublisherId` ownership binding (`[PKG-APPID-001]`)
    ///    with zero dirty rows written on rejection.
    /// 5. Track isolation (`CommunitySandboxed -> CommunityTrack`,
    ///    `PlatformCore | CertifiedPublisher -> OfficialTrack`).
    ///
    /// # Errors
    ///
    /// Fails closed with zero durable catalog writes if any check fails.
    #[allow(clippy::too_many_lines)]
    pub fn submit_release(
        &self,
        cas: &LocalOciCasStore,
        trust: &dyn PublisherTrustVerifier,
        submission: &StoreReleaseSubmission,
    ) -> Result<StoreReleaseRecord, StoreRegistryError> {
        submission.validate_structure()?;

        let verified_layout = cas.verify_stored_layout(&submission.oci_manifest_digest)?;
        for slot in &submission.components {
            let mut matched_digest_size: Option<u64> = None;
            for layer in &verified_layout.layers {
                if layer.sha256_digest == slot.content_sha256 {
                    if layer.size_bytes == slot.size_bytes {
                        matched_digest_size = Some(layer.size_bytes);
                        break;
                    }
                    matched_digest_size = Some(layer.size_bytes);
                }
            }
            match matched_digest_size {
                Some(layer_size) if layer_size == slot.size_bytes => {}
                Some(layer_size) => {
                    return Err(StoreRegistryError::OciCas(OciCasError::LayerSizeMismatch {
                        declared: layer_size,
                        actual: slot.size_bytes,
                    }));
                }
                None => {
                    return Err(StoreRegistryError::OciCas(OciCasError::BlobMissing {
                        digest: slot.content_sha256,
                    }));
                }
            }
        }

        let signing_digest = submission.signing_digest();
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let _meta = load_governance_meta(&transaction)?;

        if release_exists(
            &transaction,
            submission.package_id,
            submission.package_version,
        )? {
            return Err(StoreRegistryError::ReleaseAlreadyExists {
                package_id: submission.package_id,
                package_version: submission.package_version,
            });
        }

        let verified_publisher = trust.verify_publisher_signature(
            submission.publisher_id,
            &submission.publisher_signing_key,
            &signing_digest,
            &submission.publisher_signature,
            submission.submitted_at_ms,
        )?;
        if verified_publisher.publisher_id != submission.publisher_id
            || verified_publisher.leaf_public_key != submission.publisher_signing_key
        {
            return Err(StoreRegistryError::PublisherTrust(
                PublisherTrustCheckError::SignatureInvalid,
            ));
        }

        // Pre-check local durable ApplicationId ownership table before calling
        // into the external trust authority so multi-app submissions fail closed
        // without partial bindings.
        let mut existing_local_owners = Vec::with_capacity(submission.application_ids.len());
        for app_id in &submission.application_ids {
            let local_binding = load_application_owner_optional(&transaction, *app_id)?;
            if let Some(binding) = local_binding
                && binding.owner_publisher_id != submission.publisher_id
            {
                return Err(StoreRegistryError::PublisherTrust(
                    PublisherTrustCheckError::ApplicationIdHijackAttempt {
                        application_id: *app_id,
                        registered_owner: binding.owner_publisher_id,
                        attempted_publisher: submission.publisher_id,
                    },
                ));
            }
            existing_local_owners.push((*app_id, local_binding.is_some()));
        }

        let mut new_local_bindings = Vec::new();
        for (app_id, already_stored_locally) in existing_local_owners {
            let binding = trust.verify_or_bind_application_owner(
                submission.publisher_id,
                verified_publisher.trust_tier,
                app_id,
                submission.submitted_at_ms,
            )?;
            if binding.owner_publisher_id != submission.publisher_id {
                return Err(StoreRegistryError::PublisherTrust(
                    PublisherTrustCheckError::ApplicationIdHijackAttempt {
                        application_id: app_id,
                        registered_owner: binding.owner_publisher_id,
                        attempted_publisher: submission.publisher_id,
                    },
                ));
            }
            if !already_stored_locally {
                new_local_bindings.push(binding);
            }
        }

        for binding in &new_local_bindings {
            transaction.execute(
                "INSERT INTO store_application_owners (
                    application_id, owner_publisher_id, trust_tier,
                    first_registered_at_ms, registration_generation
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    binding.application_id.as_bytes().as_slice(),
                    binding.owner_publisher_id.as_bytes().as_slice(),
                    i64::from(binding.trust_tier.encode()),
                    encode_u64(binding.first_registered_at_ms)?,
                    encode_u64(binding.registration_generation.max(1))?,
                ],
            )?;
        }

        let channel = StoreChannel::Candidate;
        let track = StoreCatalogTrack::for_trust_tier(verified_publisher.trust_tier);

        transaction.execute(
            "INSERT INTO store_releases (
                package_id, package_version, manifest_digest, oci_manifest_digest,
                publisher_id, publisher_trust_tier, publisher_signing_key, publisher_signature,
                channel, track, active_recall_severity, active_recall_reason,
                submitted_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, NULL, ?11, ?12)",
            params![
                submission.package_id.as_bytes().as_slice(),
                encode_u64(submission.package_version)?,
                submission.manifest_digest.as_slice(),
                submission.oci_manifest_digest.as_slice(),
                submission.publisher_id.as_bytes().as_slice(),
                i64::from(verified_publisher.trust_tier.encode()),
                submission.publisher_signing_key.as_slice(),
                submission.publisher_signature.as_slice(),
                i64::from(channel.encode()),
                i64::from(track.encode()),
                encode_u64(submission.submitted_at_ms)?,
                encode_u64(submission.submitted_at_ms)?,
            ],
        )?;

        for (idx, app_id) in submission.application_ids.iter().enumerate() {
            transaction.execute(
                "INSERT INTO store_release_applications (
                    package_id, package_version, app_ordinal, application_id
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    submission.package_id.as_bytes().as_slice(),
                    encode_u64(submission.package_version)?,
                    encode_usize(idx)?,
                    app_id.as_bytes().as_slice(),
                ],
            )?;
        }

        for (idx, comp) in submission.components.iter().enumerate() {
            transaction.execute(
                "INSERT INTO store_release_components (
                    package_id, package_version, component_ordinal, slot_name,
                    component_kind, entrypoint, content_sha256, size_bytes
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    submission.package_id.as_bytes().as_slice(),
                    encode_u64(submission.package_version)?,
                    encode_usize(idx)?,
                    comp.slot_name.as_str(),
                    i64::from(comp.kind.encode()),
                    comp.entrypoint.as_str(),
                    comp.content_sha256.as_slice(),
                    encode_u64(comp.size_bytes)?,
                ],
            )?;
        }

        for (idx, perm) in submission.declared_permissions.iter().enumerate() {
            transaction.execute(
                "INSERT INTO store_permission_transparency (
                    package_id, package_version, perm_ordinal, capability_scope,
                    human_purpose, effect_class, network_egress_declared, sandbox_profile
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    submission.package_id.as_bytes().as_slice(),
                    encode_u64(submission.package_version)?,
                    encode_usize(idx)?,
                    perm.capability_scope.as_str(),
                    perm.human_purpose.as_str(),
                    encode_effect_class(perm.effect_class),
                    i64::from(u8::from(perm.network_egress_declared)),
                    perm.sandbox_profile.as_str(),
                ],
            )?;
        }

        transaction.commit()?;

        Ok(StoreReleaseRecord {
            package_id: submission.package_id,
            package_version: submission.package_version,
            manifest_digest: submission.manifest_digest,
            oci_manifest_digest: submission.oci_manifest_digest,
            publisher_id: submission.publisher_id,
            publisher_trust_tier: verified_publisher.trust_tier,
            publisher_signing_key: submission.publisher_signing_key,
            publisher_signature: submission.publisher_signature,
            channel,
            track,
            active_recall_severity: None,
            application_ids: submission.application_ids.clone(),
            components: submission.components.clone(),
            declared_permissions: submission.declared_permissions.clone(),
            submitted_at_ms: submission.submitted_at_ms,
            updated_at_ms: submission.submitted_at_ms,
        })
    }

    /// Promotes an `OfficialTrack` release from `StoreChannel::Candidate` to
    /// `StoreChannel::Stable` upon verifying an `Ed25519`-signed four-gate
    /// [`SignedCertificationAttestation`] (`passed_gates_mask == 0b1111`).
    ///
    /// `CommunitySandboxed` releases are strictly barred from `Stable`
    /// (`StoreRegistryError::CommunityPublisherForbiddenInStable`).
    ///
    /// # Errors
    ///
    /// Fails closed on recalled releases, community sandboxed publishers,
    /// untrusted certifier key, invalid signature, incomplete gates, expired
    /// attestation, digest mismatch, or idempotency conflict.
    #[allow(clippy::too_many_lines)]
    pub fn promote_candidate_to_stable(
        &self,
        attestation: &SignedCertificationAttestation,
        idempotency_key: IdempotencyKey,
        now_ms: u64,
    ) -> Result<StorePromotionDecision, StoreRegistryError> {
        let request_digest = promotion_request_digest(attestation, idempotency_key, now_ms);
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(replay) = load_promotion_replay(&transaction, idempotency_key, request_digest)?
        {
            transaction.commit()?;
            return Ok(StorePromotionDecision::Replayed(replay));
        }

        let meta = load_governance_meta(&transaction)?;
        let (release, _recall_reason) = load_release_record_optional(
            &transaction,
            attestation.package_id,
            attestation.package_version,
        )?
        .ok_or(StoreRegistryError::ReleaseNotFound {
            package_id: attestation.package_id,
            package_version: attestation.package_version,
        })?;

        if let Some(severity) = release.active_recall_severity {
            return Err(StoreRegistryError::ReleaseRecalled {
                package_id: release.package_id,
                package_version: release.package_version,
                severity,
            });
        }
        if release.channel == StoreChannel::Recalled {
            return Err(StoreRegistryError::ReleaseRecalled {
                package_id: release.package_id,
                package_version: release.package_version,
                severity: RecallSeverity::BlockLaunchAndRevokeCap,
            });
        }

        if release.publisher_trust_tier == PublisherTrustTier::CommunitySandboxed
            || release.track == StoreCatalogTrack::CommunityTrack
        {
            return Err(StoreRegistryError::CommunityPublisherForbiddenInStable {
                publisher_id: release.publisher_id,
                package_id: release.package_id,
            });
        }

        if release.channel != StoreChannel::Candidate {
            return Err(StoreRegistryError::ChannelTransitionForbidden {
                current_channel: release.channel,
                target_channel: StoreChannel::Stable,
            });
        }

        if attestation.certifier_public_key != meta.certifier_public_key {
            return Err(StoreRegistryError::AttestationUntrustedCertifier);
        }
        attestation.verify_signature()?;

        if attestation.expires_at_ms < attestation.issued_at_ms {
            return Err(StoreRegistryError::InvalidInput(
                "certification attestation expires_at_ms precedes issued_at_ms",
            ));
        }
        if now_ms < attestation.issued_at_ms {
            return Err(StoreRegistryError::AttestationNotYetValid {
                issued_at_ms: attestation.issued_at_ms,
                now_ms,
            });
        }
        if now_ms > attestation.expires_at_ms {
            return Err(StoreRegistryError::AttestationExpired {
                expires_at_ms: attestation.expires_at_ms,
                now_ms,
            });
        }

        if attestation.passed_gates_mask != REQUIRED_CERTIFICATION_GATES_MASK {
            return Err(StoreRegistryError::AttestationIncompleteGates {
                passed_gates_mask: attestation.passed_gates_mask,
                required_mask: REQUIRED_CERTIFICATION_GATES_MASK,
            });
        }

        if !release
            .application_ids
            .contains(&attestation.application_id)
        {
            return Err(StoreRegistryError::ApplicationNotInRelease {
                package_id: release.package_id,
                package_version: release.package_version,
                application_id: attestation.application_id,
            });
        }
        if attestation.manifest_digest != release.manifest_digest {
            return Err(StoreRegistryError::AttestationBindingMismatch(
                "manifest_digest",
            ));
        }
        if attestation.oci_manifest_digest != release.oci_manifest_digest {
            return Err(StoreRegistryError::AttestationBindingMismatch(
                "oci_manifest_digest",
            ));
        }
        if attestation.publisher_id != release.publisher_id {
            return Err(StoreRegistryError::AttestationBindingMismatch(
                "publisher_id",
            ));
        }
        if now_ms < release.submitted_at_ms {
            return Err(StoreRegistryError::InvalidInput(
                "promotion timestamp precedes release submission timestamp",
            ));
        }

        let changed = transaction.execute(
            "UPDATE store_releases
             SET channel = ?1, updated_at_ms = ?2
             WHERE package_id = ?3 AND package_version = ?4 AND channel = ?5",
            params![
                i64::from(StoreChannel::Stable.encode()),
                encode_u64(now_ms)?,
                release.package_id.as_bytes().as_slice(),
                encode_u64(release.package_version)?,
                i64::from(StoreChannel::Candidate.encode()),
            ],
        )?;
        if changed != 1 {
            return Err(StoreRegistryError::CorruptRecord(
                "Candidate -> Stable channel CAS lost",
            ));
        }

        let receipt_id = derive_receipt_id(PROMOTION_RECEIPT_ID_DOMAIN, &request_digest);
        let receipt = StoreCertificationPromotionReceipt {
            receipt_id,
            idempotency_key,
            package_id: release.package_id,
            package_version: release.package_version,
            application_id: attestation.application_id,
            passed_gates_mask: attestation.passed_gates_mask,
            promoted_at_ms: now_ms,
        };

        transaction.execute(
            "INSERT INTO store_certification_receipts (
                idempotency_key, request_digest, receipt_id, package_id, package_version,
                application_id, manifest_digest, oci_manifest_digest, publisher_id,
                passed_gates_mask, issued_at_ms, expires_at_ms, certifier_public_key,
                ed25519_signature, promoted_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                idempotency_key.as_bytes().as_slice(),
                request_digest.as_slice(),
                receipt_id.as_bytes().as_slice(),
                release.package_id.as_bytes().as_slice(),
                encode_u64(release.package_version)?,
                attestation.application_id.as_bytes().as_slice(),
                attestation.manifest_digest.as_slice(),
                attestation.oci_manifest_digest.as_slice(),
                attestation.publisher_id.as_bytes().as_slice(),
                i64::from(attestation.passed_gates_mask),
                encode_u64(attestation.issued_at_ms)?,
                encode_u64(attestation.expires_at_ms)?,
                attestation.certifier_public_key.as_slice(),
                attestation.ed25519_signature.as_slice(),
                encode_u64(now_ms)?,
            ],
        )?;

        transaction.commit()?;
        Ok(StorePromotionDecision::Promoted(receipt))
    }

    /// Reads a single release record from the governance catalog.
    ///
    /// # Errors
    ///
    /// Fails closed on mutex poison, corrupt row data, or storage failure.
    pub fn inspect_release(
        &self,
        package_id: PackageId,
        package_version: u64,
    ) -> Result<Option<StoreReleaseRecord>, StoreRegistryError> {
        let connection = self.lock()?;
        Ok(
            load_release_record_optional(&connection, package_id, package_version)?
                .map(|(record, _)| record),
        )
    }

    /// Reads the durable first-registration [`ApplicationOwnershipBinding`] for
    /// `application_id` (`[PKG-APPID-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed on storage or decoding error.
    pub fn inspect_application_owner(
        &self,
        application_id: ApplicationId,
    ) -> Result<Option<ApplicationOwnershipBinding>, StoreRegistryError> {
        let connection = self.lock()?;
        load_application_owner_optional(&connection, application_id)
    }

    /// Lists every store release bound to `application_id`, ordered by
    /// `(package_version ASC, package_id ASC)`.
    ///
    /// # Errors
    ///
    /// Fails closed on storage or decoding error.
    pub fn list_releases_for_application(
        &self,
        application_id: ApplicationId,
    ) -> Result<Vec<StoreReleaseRecord>, StoreRegistryError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT package_id, package_version
             FROM store_release_applications
             WHERE application_id = ?1
             ORDER BY package_version ASC, package_id ASC",
        )?;
        let mut rows = statement.query([application_id.as_bytes().as_slice()])?;
        let mut keys = Vec::new();
        while let Some(row) = rows.next()? {
            let pkg = decode_id(row.get(0)?, PackageId::from_bytes, "package_id")?;
            let ver = decode_u64(row.get(1)?)?;
            keys.push((pkg, ver));
        }
        drop(rows);
        drop(statement);

        let mut out = Vec::with_capacity(keys.len());
        for (pkg, ver) in keys {
            let (record, _) = load_release_record_optional(&connection, pkg, ver)?.ok_or(
                StoreRegistryError::CorruptRecord("dangling application release key"),
            )?;
            out.push(record);
        }
        Ok(out)
    }

    /// Lists all active official `Stable` releases (`OfficialTrack`,
    /// `StoreChannel::Stable`, and no active `RecallAdvisory`).
    ///
    /// `CommunitySandboxed` releases and recalled releases never appear here.
    ///
    /// # Errors
    ///
    /// Fails closed on storage or decoding error.
    pub fn list_official_stable_releases(
        &self,
    ) -> Result<Vec<StoreReleaseRecord>, StoreRegistryError> {
        let connection = self.lock()?;
        query_releases_by_predicate(
            &connection,
            "WHERE track = 1 AND channel = 2 AND active_recall_severity IS NULL
             ORDER BY package_id ASC, package_version ASC",
        )
    }

    /// Lists all unendorsed community sandbox candidate releases
    /// (`CommunityTrack`, `StoreChannel::Candidate`).
    ///
    /// # Errors
    ///
    /// Fails closed on storage or decoding error.
    pub fn list_community_sandbox_candidates(
        &self,
    ) -> Result<Vec<StoreReleaseRecord>, StoreRegistryError> {
        let connection = self.lock()?;
        query_releases_by_predicate(
            &connection,
            "WHERE track = 2 AND channel = 1
             ORDER BY package_id ASC, package_version ASC",
        )
    }

    /// Builds the structured [`PermissionTransparencyReport`] for a stored
    /// release (`[TUI-CONSENT-001]`, `[TCB-EGRESS-001]`).
    ///
    /// # Errors
    ///
    /// Returns [`StoreRegistryError::ReleaseNotFound`] if the release does not
    /// exist, or fails closed on storage error.
    pub fn inspect_permission_transparency(
        &self,
        package_id: PackageId,
        package_version: u64,
    ) -> Result<PermissionTransparencyReport, StoreRegistryError> {
        let release = self.inspect_release(package_id, package_version)?.ok_or(
            StoreRegistryError::ReleaseNotFound {
                package_id,
                package_version,
            },
        )?;
        let any_network_egress = release
            .declared_permissions
            .iter()
            .any(|entry| entry.network_egress_declared);
        let any_irreversible_effect = release
            .declared_permissions
            .iter()
            .any(|entry| entry.effect_class == EffectClass::Irreversible);
        Ok(PermissionTransparencyReport {
            package_id: release.package_id,
            package_version: release.package_version,
            publisher_id: release.publisher_id,
            publisher_trust_tier: release.publisher_trust_tier,
            channel: release.channel,
            track: release.track,
            permissions: release.declared_permissions,
            any_network_egress,
            any_irreversible_effect,
        })
    }

    /// Computes the cross-version [`PermissionEscalationDiff`] between two
    /// stored versions of `package_id` to gate silent upgrades and force
    /// `[TUI-CONSENT-001]` `Z4` re-authorization on any escalation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreRegistryError::ReleaseNotFound`] if either version is
    /// absent from the governance catalog.
    pub fn diff_release_permission_transparency(
        &self,
        package_id: PackageId,
        old_version: u64,
        new_version: u64,
    ) -> Result<PermissionEscalationDiff, StoreRegistryError> {
        let connection = self.lock()?;
        let (old_release, _) = load_release_record_optional(&connection, package_id, old_version)?
            .ok_or(StoreRegistryError::ReleaseNotFound {
                package_id,
                package_version: old_version,
            })?;
        let (new_release, _) = load_release_record_optional(&connection, package_id, new_version)?
            .ok_or(StoreRegistryError::ReleaseNotFound {
                package_id,
                package_version: new_version,
            })?;
        Ok(diff_permission_transparency(
            &old_release.declared_permissions,
            &new_release.declared_permissions,
        ))
    }

    /// Offline-verifies an opaque `Ed25519`-signed [`crate::SignedEntitlementToken`]
    /// and atomically redeems one seat against its bounded allowance, producing
    /// an immutable [`EntitlementGrantReceipt`] (`ADR-0027` section 2.3;
    /// `[NLOS-DEFER-001]`).
    ///
    /// Zero fiat pricing, payment gateway, or billing logic exists in this path.
    ///
    /// # Errors
    ///
    /// Fails closed on unknown issuer key, invalid `Ed25519` signature, expired
    /// or not-yet-valid token, `PrincipalId` or `ApplicationId` mismatch, stale
    /// local `store_revocation_epoch`, recalled target release, disallowed
    /// channel, seat exhaustion, or idempotency conflict.
    #[allow(clippy::too_many_lines)]
    pub fn verify_and_redeem_entitlement(
        &self,
        request: &RedeemEntitlementRequest,
    ) -> Result<EntitlementRedeemDecision, StoreRegistryError> {
        let request_digest = request.request_digest();
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(replay) =
            load_entitlement_replay(&transaction, request.idempotency_key, request_digest)?
        {
            transaction.commit()?;
            return Ok(EntitlementRedeemDecision::Replayed(replay));
        }

        let meta = load_governance_meta(&transaction)?;

        if request.token.issuer_key_id != meta.entitlement_issuer_key_id {
            return Err(StoreRegistryError::EntitlementUnknownIssuerKey(
                request.token.issuer_key_id,
            ));
        }
        request
            .token
            .verify_signature(&meta.entitlement_issuer_public_key)?;

        if request.token.max_seat_or_install_count == 0 {
            return Err(StoreRegistryError::InvalidInput(
                "entitlement max_seat_or_install_count must be >= 1",
            ));
        }
        if request.token.allowed_channel == StoreChannel::Recalled {
            return Err(StoreRegistryError::InvalidInput(
                "entitlement token allowed_channel cannot be Recalled",
            ));
        }
        if request.token.expires_at_ms < request.token.issued_at_ms {
            return Err(StoreRegistryError::InvalidInput(
                "entitlement token expires_at_ms precedes issued_at_ms",
            ));
        }
        if request.now_ms < request.token.issued_at_ms {
            return Err(StoreRegistryError::EntitlementNotYetValid {
                issued_at_ms: request.token.issued_at_ms,
                now_ms: request.now_ms,
            });
        }
        if request.now_ms > request.token.expires_at_ms {
            return Err(StoreRegistryError::EntitlementExpired {
                expires_at_ms: request.token.expires_at_ms,
                now_ms: request.now_ms,
            });
        }
        if request.token.holder_principal != request.presenter_principal {
            return Err(StoreRegistryError::EntitlementPrincipalMismatch {
                token_holder: request.token.holder_principal,
                presenter: request.presenter_principal,
            });
        }
        if request.token.application_id != request.requested_application_id {
            return Err(StoreRegistryError::EntitlementApplicationMismatch {
                token_application: request.token.application_id,
                requested_application: request.requested_application_id,
            });
        }
        if meta.store_revocation_epoch < request.token.min_revocation_epoch {
            return Err(StoreRegistryError::EntitlementStoreEpochTooStale {
                token_min_epoch: request.token.min_revocation_epoch,
                store_epoch: meta.store_revocation_epoch,
            });
        }

        let (release, _) = load_release_record_optional(
            &transaction,
            request.package_id,
            request.package_version,
        )?
        .ok_or(StoreRegistryError::ReleaseNotFound {
            package_id: request.package_id,
            package_version: request.package_version,
        })?;

        if !release
            .application_ids
            .contains(&request.requested_application_id)
        {
            return Err(StoreRegistryError::ApplicationNotInRelease {
                package_id: release.package_id,
                package_version: release.package_version,
                application_id: request.requested_application_id,
            });
        }

        if let Some(severity) = release.active_recall_severity {
            return Err(StoreRegistryError::ReleaseRecalled {
                package_id: release.package_id,
                package_version: release.package_version,
                severity,
            });
        }
        if release.channel == StoreChannel::Recalled {
            return Err(StoreRegistryError::ReleaseRecalled {
                package_id: release.package_id,
                package_version: release.package_version,
                severity: RecallSeverity::BlockLaunchAndRevokeCap,
            });
        }

        let channel_permitted = match request.token.allowed_channel {
            StoreChannel::Stable => release.channel == StoreChannel::Stable,
            StoreChannel::Candidate => {
                matches!(
                    release.channel,
                    StoreChannel::Candidate | StoreChannel::Stable
                )
            }
            StoreChannel::Recalled => false,
        };
        if !channel_permitted {
            return Err(StoreRegistryError::EntitlementChannelDisallowed {
                allowed_channel: request.token.allowed_channel,
                release_channel: release.channel,
            });
        }

        let usage_row = transaction
            .query_row(
                "SELECT holder_principal, application_id, max_seat_count, redeemed_seat_count
                 FROM store_entitlement_usage WHERE token_id = ?1",
                [request.token.token_id.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        let seat_ordinal = if let Some((holder_raw, app_raw, max_raw, redeemed_raw)) = usage_row {
            let stored_holder = decode_id(holder_raw, PrincipalId::from_bytes, "holder_principal")?;
            let stored_app = decode_id(app_raw, ApplicationId::from_bytes, "application_id")?;
            let stored_max = decode_u32(max_raw, "max_seat_count")?;
            let redeemed = decode_u32(redeemed_raw, "redeemed_seat_count")?;
            if stored_holder != request.token.holder_principal
                || stored_app != request.token.application_id
                || stored_max != request.token.max_seat_or_install_count
            {
                return Err(StoreRegistryError::CorruptRecord(
                    "entitlement token_id rebound to conflicting token claims",
                ));
            }
            if redeemed >= request.token.max_seat_or_install_count {
                return Err(StoreRegistryError::EntitlementSeatLimitExceeded {
                    token_id: request.token.token_id,
                    max_seats: request.token.max_seat_or_install_count,
                    redeemed_seats: redeemed,
                });
            }
            let next_seat = redeemed + 1;
            transaction.execute(
                "UPDATE store_entitlement_usage
                 SET redeemed_seat_count = ?1, updated_at_ms = ?2
                 WHERE token_id = ?3 AND redeemed_seat_count = ?4",
                params![
                    i64::from(next_seat),
                    encode_u64(request.now_ms)?,
                    request.token.token_id.as_slice(),
                    i64::from(redeemed),
                ],
            )?;
            next_seat
        } else {
            transaction.execute(
                "INSERT INTO store_entitlement_usage (
                    token_id, holder_principal, application_id, max_seat_count,
                    redeemed_seat_count, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, 1, ?5)",
                params![
                    request.token.token_id.as_slice(),
                    request.token.holder_principal.as_bytes().as_slice(),
                    request.token.application_id.as_bytes().as_slice(),
                    i64::from(request.token.max_seat_or_install_count),
                    encode_u64(request.now_ms)?,
                ],
            )?;
            1
        };

        let remaining_seats = request.token.max_seat_or_install_count - seat_ordinal;
        let receipt_id = derive_receipt_id(ENTITLEMENT_RECEIPT_ID_DOMAIN, &request_digest);
        let receipt = EntitlementGrantReceipt {
            receipt_id,
            idempotency_key: request.idempotency_key,
            token_id: request.token.token_id,
            holder_principal: request.token.holder_principal,
            application_id: request.requested_application_id,
            package_id: request.package_id,
            package_version: request.package_version,
            granted_channel: release.channel,
            seat_ordinal,
            remaining_seats,
            store_revocation_epoch: meta.store_revocation_epoch,
            redeemed_at_ms: request.now_ms,
        };

        transaction.execute(
            "INSERT INTO store_entitlement_receipts (
                idempotency_key, request_digest, receipt_id, token_id, holder_principal,
                application_id, package_id, package_version, granted_channel,
                seat_ordinal, remaining_seats, store_revocation_epoch, redeemed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                request.idempotency_key.as_bytes().as_slice(),
                request_digest.as_slice(),
                receipt_id.as_bytes().as_slice(),
                request.token.token_id.as_slice(),
                request.token.holder_principal.as_bytes().as_slice(),
                request.requested_application_id.as_bytes().as_slice(),
                request.package_id.as_bytes().as_slice(),
                encode_u64(request.package_version)?,
                i64::from(release.channel.encode()),
                i64::from(seat_ordinal),
                i64::from(remaining_seats),
                encode_u64(meta.store_revocation_epoch.get())?,
                encode_u64(request.now_ms)?,
            ],
        )?;

        transaction.commit()?;
        Ok(EntitlementRedeemDecision::Granted(receipt))
    }

    /// Reads the number of seats currently redeemed against `token_id` (`0` if
    /// never redeemed).
    ///
    /// # Errors
    ///
    /// Fails closed on storage or decoding error.
    pub fn inspect_entitlement_redeemed_seats(
        &self,
        token_id: &[u8; 16],
    ) -> Result<u32, StoreRegistryError> {
        let connection = self.lock()?;
        let count = connection
            .query_row(
                "SELECT redeemed_seat_count FROM store_entitlement_usage WHERE token_id = ?1",
                [token_id.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        match count {
            Some(raw) => decode_u32(raw, "redeemed_seat_count"),
            None => Ok(0),
        }
    }

    /// Registers an active [`CapabilityHandle`] issued for a store release so
    /// future Level 2 / Level 3 `RecallAdvisory` transitions immediately fence
    /// and revoke it in [`CapabilityAuthority`] (`[CAP-REVOKE-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if the capability is inactive/revoked in `CapabilityAuthority`,
    /// if the release does not exist or is already recalled at Level 2/3, or if
    /// the `CapabilityId` is already bound to conflicting metadata.
    pub fn register_granted_capability(
        &self,
        capabilities: &CapabilityAuthority,
        request: &RegisterGrantedCapabilityRequest,
    ) -> Result<GrantedCapabilityBindingRecord, StoreRegistryError> {
        let _active_record =
            capabilities.inspect_active(request.handle, request.registered_at_ms)?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let (release, _) = load_release_record_optional(
            &transaction,
            request.package_id,
            request.package_version,
        )?
        .ok_or(StoreRegistryError::ReleaseNotFound {
            package_id: request.package_id,
            package_version: request.package_version,
        })?;

        if !release.application_ids.contains(&request.application_id) {
            return Err(StoreRegistryError::ApplicationNotInRelease {
                package_id: release.package_id,
                package_version: release.package_version,
                application_id: request.application_id,
            });
        }

        if let Some(severity) = release.active_recall_severity
            && severity >= RecallSeverity::BlockLaunchAndRevokeCap
        {
            return Err(StoreRegistryError::ReleaseRecalled {
                package_id: release.package_id,
                package_version: release.package_version,
                severity,
            });
        }
        if release.channel == StoreChannel::Recalled {
            return Err(StoreRegistryError::ReleaseRecalled {
                package_id: release.package_id,
                package_version: release.package_version,
                severity: RecallSeverity::BlockLaunchAndRevokeCap,
            });
        }

        if let Some(existing) =
            load_granted_capability_optional(&transaction, request.handle.capability_id)?
        {
            if existing.issued_generation == request.handle.generation
                && existing.package_id == request.package_id
                && existing.package_version == request.package_version
                && existing.application_id == request.application_id
                && existing.installation_id == request.installation_id
            {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(StoreRegistryError::CapabilityBindingConflict {
                capability_id: request.handle.capability_id,
            });
        }

        transaction.execute(
            "INSERT INTO store_granted_capabilities (
                capability_id, issued_generation, package_id, package_version,
                application_id, installation_id, registered_at_ms,
                revoked_by_advisory_id, revoked_resulting_generation, revoked_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL)",
            params![
                request.handle.capability_id.as_bytes().as_slice(),
                encode_u64(request.handle.generation.get())?,
                request.package_id.as_bytes().as_slice(),
                encode_u64(request.package_version)?,
                request.application_id.as_bytes().as_slice(),
                request
                    .installation_id
                    .as_ref()
                    .map(|id| id.as_bytes().as_slice()),
                encode_u64(request.registered_at_ms)?,
            ],
        )?;

        transaction.commit()?;
        Ok(GrantedCapabilityBindingRecord {
            capability_id: request.handle.capability_id,
            issued_generation: request.handle.generation,
            package_id: request.package_id,
            package_version: request.package_version,
            application_id: request.application_id,
            installation_id: request.installation_id,
            registered_at_ms: request.registered_at_ms,
            revoked_by_advisory_id: None,
            revoked_resulting_generation: None,
            revoked_at_ms: None,
        })
    }

    /// Lists all capability bindings registered for `(package_id, application_id)`.
    ///
    /// # Errors
    ///
    /// Fails closed on storage or decoding error.
    pub fn list_granted_capabilities(
        &self,
        package_id: PackageId,
        application_id: ApplicationId,
    ) -> Result<Vec<GrantedCapabilityBindingRecord>, StoreRegistryError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT capability_id, issued_generation, package_id, package_version,
                    application_id, installation_id, registered_at_ms,
                    revoked_by_advisory_id, revoked_resulting_generation, revoked_at_ms
             FROM store_granted_capabilities
             WHERE package_id = ?1 AND application_id = ?2
             ORDER BY registered_at_ms ASC, capability_id ASC",
        )?;
        let mut rows = statement.query(params![
            package_id.as_bytes().as_slice(),
            application_id.as_bytes().as_slice(),
        ])?;
        let mut bindings = Vec::new();
        while let Some(row) = rows.next()? {
            bindings.push(decode_granted_capability_row(row)?);
        }
        Ok(bindings)
    }

    /// Evaluates whether a new store download/installation is permitted for
    /// `(package_id, package_version)`. Any active `RecallAdvisory` (including
    /// Level 1 `WarnNewInstall`) blocks fresh store installs fail-closed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreRegistryError::ReleaseNotFound`] or
    /// [`StoreRegistryError::ReleaseRecalled`] when blocked.
    pub fn evaluate_new_install_gate(
        &self,
        package_id: PackageId,
        package_version: u64,
    ) -> Result<StoreReleaseRecord, StoreRegistryError> {
        let connection = self.lock()?;
        let (release, _) = load_release_record_optional(&connection, package_id, package_version)?
            .ok_or(StoreRegistryError::ReleaseNotFound {
                package_id,
                package_version,
            })?;
        if let Some(severity) = release.active_recall_severity {
            return Err(StoreRegistryError::ReleaseRecalled {
                package_id,
                package_version,
                severity,
            });
        }
        if release.channel == StoreChannel::Recalled {
            return Err(StoreRegistryError::ReleaseRecalled {
                package_id,
                package_version,
                severity: RecallSeverity::BlockLaunchAndRevokeCap,
            });
        }
        Ok(release)
    }

    /// Evaluates whether an already-installed offline local instance of
    /// `(package_id, package_version)` may launch (`[DUI-OFFLINE-001]`):
    ///
    /// - No recall -> [`LocalLaunchDisposition::Allowed`]
    /// - `WarnNewInstall` -> [`LocalLaunchDisposition::AllowedWithRiskWarning`]
    /// - `BlockLaunchAndRevokeCap` / `QuarantineInstallation` ->
    ///   `Err(StoreRegistryError::ReleaseRecalled)`
    ///
    /// # Errors
    ///
    /// Returns [`StoreRegistryError::ReleaseNotFound`] or
    /// [`StoreRegistryError::ReleaseRecalled`] when launch is blocked.
    pub fn evaluate_local_launch_gate(
        &self,
        package_id: PackageId,
        package_version: u64,
    ) -> Result<LocalLaunchDisposition, StoreRegistryError> {
        let connection = self.lock()?;
        let (release, recall_reason) =
            load_release_record_optional(&connection, package_id, package_version)?.ok_or(
                StoreRegistryError::ReleaseNotFound {
                    package_id,
                    package_version,
                },
            )?;
        match release.active_recall_severity {
            None => {
                if release.channel == StoreChannel::Recalled {
                    Err(StoreRegistryError::ReleaseRecalled {
                        package_id,
                        package_version,
                        severity: RecallSeverity::BlockLaunchAndRevokeCap,
                    })
                } else {
                    Ok(LocalLaunchDisposition::Allowed)
                }
            }
            Some(RecallSeverity::WarnNewInstall) => {
                Ok(LocalLaunchDisposition::AllowedWithRiskWarning {
                    reason: recall_reason.unwrap_or_default(),
                })
            }
            Some(
                severity @ (RecallSeverity::BlockLaunchAndRevokeCap
                | RecallSeverity::QuarantineInstallation),
            ) => Err(StoreRegistryError::ReleaseRecalled {
                package_id,
                package_version,
                severity,
            }),
        }
    }

    /// Verifies and applies a store security [`SignedRecallAdvisory`] with
    /// monotonic `store_revocation_epoch` anti-rollback, severity anti-downgrade,
    /// immediate [`CapabilityAuthority::revoke_signed`] enforcement (`Level 2/3`),
    /// and [`nlos_application::ApplicationAuthority::disable_application`]
    /// quarantine (`Level 3`, preserving user data containers untouched).
    ///
    /// # Errors
    ///
    /// Fails closed on forged authority signature, rolled-back or replayed
    /// `revocation_epoch`, severity downgrade attempt, missing enforcement
    /// authorities for Level 2/3, or downstream revocation/quarantine error.
    #[allow(clippy::too_many_lines)]
    pub fn apply_recall_advisory(
        &self,
        advisory: &SignedRecallAdvisory,
        idempotency_key: IdempotencyKey,
        applied_at_ms: u64,
        authorities: Option<&RecallEnforcementAuthorities<'_>>,
    ) -> Result<RecallApplyDecision, StoreRegistryError> {
        let request_digest = advisory.apply_request_digest(idempotency_key, applied_at_ms);
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(replay) = load_recall_replay(&transaction, idempotency_key, request_digest)? {
            transaction.commit()?;
            return Ok(RecallApplyDecision::Replayed(replay));
        }

        if advisory.reason.trim().is_empty() {
            return Err(StoreRegistryError::InvalidInput(
                "recall advisory reason must be non-empty",
            ));
        }
        if applied_at_ms < advisory.issued_at_ms {
            return Err(StoreRegistryError::InvalidInput(
                "recall applied_at_ms precedes advisory issued_at_ms",
            ));
        }

        let meta = load_governance_meta(&transaction)?;
        advisory.verify_signature(&meta.recall_authority_public_key)?;

        if advisory.revocation_epoch <= meta.store_revocation_epoch {
            return Err(StoreRegistryError::RecallEpochRollback {
                current_epoch: meta.store_revocation_epoch,
                candidate_epoch: advisory.revocation_epoch,
            });
        }

        let targeted_releases = load_targeted_releases_for_recall(&transaction, advisory)?;
        for release in &targeted_releases {
            if let Some(current_severity) = release.active_recall_severity
                && advisory.severity < current_severity
            {
                return Err(StoreRegistryError::RecallSeverityDowngradeRefused {
                    current_severity,
                    attempted_severity: advisory.severity,
                });
            }
        }

        // Pre-validate downstream enforcement context before mutating external
        // authorities so missing handles or timestamp violations fail closed cleanly.
        let pending_local_app_view = match advisory.severity {
            RecallSeverity::WarnNewInstall => None,
            RecallSeverity::BlockLaunchAndRevokeCap => {
                let _ =
                    authorities.ok_or(StoreRegistryError::RecallCapabilityAuthoritiesRequired)?;
                None
            }
            RecallSeverity::QuarantineInstallation => {
                let auth =
                    authorities.ok_or(StoreRegistryError::RecallCapabilityAuthoritiesRequired)?;
                let app_auth = auth
                    .applications
                    .ok_or(StoreRegistryError::RecallApplicationAuthorityRequired)?;
                let maybe_view = app_auth.inspect_application(advisory.package_id)?;
                if let Some(ref view) = maybe_view
                    && view.status == ApplicationStatus::Installed
                    && applied_at_ms < view.updated_at_ms
                {
                    return Err(StoreRegistryError::InvalidInput(
                        "recall applied_at_ms precedes local application updated_at_ms",
                    ));
                }
                maybe_view
            }
        };

        let mut revoked_updates: Vec<(CapabilityId, Generation)> = Vec::new();
        if advisory.severity >= RecallSeverity::BlockLaunchAndRevokeCap {
            let auth =
                authorities.ok_or(StoreRegistryError::RecallCapabilityAuthoritiesRequired)?;
            let unrevoked_bindings =
                load_unrevoked_capabilities_for_recall(&transaction, advisory)?;
            for binding in unrevoked_bindings {
                let cap_idem = derive_sub_idempotency_key(
                    CAP_REVOKE_IDEMPOTENCY_DOMAIN,
                    idempotency_key,
                    &advisory.advisory_id,
                    binding.capability_id.as_bytes(),
                );
                let revoke_command = RevokeCapabilityRequest {
                    handle: CapabilityHandle {
                        capability_id: binding.capability_id,
                        generation: binding.issued_generation,
                    },
                    revoker_key_id: auth.revocation_authorizer.revoker_key_id(),
                    idempotency_key: cap_idem,
                    revoked_at_ms: applied_at_ms,
                };
                let command_digest = revoke_command_message(revoke_command);
                let signature = auth
                    .revocation_authorizer
                    .sign_revoke_command_digest(&command_digest)?;
                let decision = auth.capabilities.revoke_signed(
                    auth.identity,
                    SignedRevokeCapabilityRequest {
                        command: revoke_command,
                        signer: auth.revocation_authorizer.revoker_principal(),
                        signature,
                    },
                )?;
                revoked_updates.push((
                    binding.capability_id,
                    decision.receipt().resulting_generation,
                ));
            }
        }

        let mut application_quarantined = false;
        if advisory.severity == RecallSeverity::QuarantineInstallation {
            let auth =
                authorities.ok_or(StoreRegistryError::RecallCapabilityAuthoritiesRequired)?;
            let app_auth = auth
                .applications
                .ok_or(StoreRegistryError::RecallApplicationAuthorityRequired)?;
            if let Some(view) = pending_local_app_view {
                match view.status {
                    ApplicationStatus::Installed => {
                        let disable_idem = derive_sub_idempotency_key(
                            APP_DISABLE_IDEMPOTENCY_DOMAIN,
                            idempotency_key,
                            &advisory.advisory_id,
                            advisory.package_id.as_bytes(),
                        );
                        let _ = app_auth.disable_application(DisableApplicationRequest {
                            package_id: advisory.package_id,
                            idempotency_key: disable_idem,
                            disabled_at_ms: applied_at_ms,
                        })?;
                        application_quarantined = true;
                    }
                    ApplicationStatus::Disabled => {
                        application_quarantined = true;
                    }
                    ApplicationStatus::Uninstalled => {
                        application_quarantined = false;
                    }
                }
            }
        }

        transaction.execute(
            "UPDATE store_governance_meta
             SET store_revocation_epoch = ?1, updated_at_ms = ?2
             WHERE singleton_id = 1 AND store_revocation_epoch = ?3",
            params![
                encode_u64(advisory.revocation_epoch.get())?,
                encode_u64(applied_at_ms.max(meta.updated_at_ms))?,
                encode_u64(meta.store_revocation_epoch.get())?,
            ],
        )?;

        for release in &targeted_releases {
            let next_channel = if advisory.severity >= RecallSeverity::BlockLaunchAndRevokeCap {
                StoreChannel::Recalled
            } else {
                release.channel
            };
            transaction.execute(
                "UPDATE store_releases
                 SET channel = ?1,
                     active_recall_severity = ?2,
                     active_recall_reason = ?3,
                     updated_at_ms = ?4
                 WHERE package_id = ?5 AND package_version = ?6",
                params![
                    i64::from(next_channel.encode()),
                    i64::from(advisory.severity.encode()),
                    advisory.reason.as_str(),
                    encode_u64(applied_at_ms.max(release.updated_at_ms))?,
                    release.package_id.as_bytes().as_slice(),
                    encode_u64(release.package_version)?,
                ],
            )?;
        }

        for (capability_id, resulting_generation) in &revoked_updates {
            transaction.execute(
                "UPDATE store_granted_capabilities
                 SET revoked_by_advisory_id = ?1,
                     revoked_resulting_generation = ?2,
                     revoked_at_ms = ?3
                 WHERE capability_id = ?4 AND revoked_by_advisory_id IS NULL",
                params![
                    advisory.advisory_id.as_slice(),
                    encode_u64(resulting_generation.get())?,
                    encode_u64(applied_at_ms)?,
                    capability_id.as_bytes().as_slice(),
                ],
            )?;
        }

        let revoked_capability_count = u32::try_from(revoked_updates.len())
            .map_err(|_| StoreRegistryError::CorruptRecord("revoked capability count overflow"))?;
        let receipt_id = derive_receipt_id(RECALL_RECEIPT_ID_DOMAIN, &request_digest);
        let receipt = RecallExecutionReceipt {
            receipt_id,
            idempotency_key,
            advisory_id: advisory.advisory_id,
            package_id: advisory.package_id,
            application_id: advisory.application_id,
            target_version: advisory.target_version,
            severity: advisory.severity,
            revocation_epoch: advisory.revocation_epoch,
            revoked_capability_count,
            application_quarantined,
            applied_at_ms,
        };

        transaction.execute(
            "INSERT INTO store_recall_advisories (
                idempotency_key, request_digest, receipt_id, advisory_id,
                package_id, application_id, target_version, severity,
                revocation_epoch, reason, issued_at_ms, applied_at_ms,
                authority_signature, revoked_capability_count, application_quarantined
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                idempotency_key.as_bytes().as_slice(),
                request_digest.as_slice(),
                receipt_id.as_bytes().as_slice(),
                advisory.advisory_id.as_slice(),
                advisory.package_id.as_bytes().as_slice(),
                advisory.application_id.as_bytes().as_slice(),
                advisory.target_version.map(encode_u64).transpose()?,
                i64::from(advisory.severity.encode()),
                encode_u64(advisory.revocation_epoch.get())?,
                advisory.reason.as_str(),
                encode_u64(advisory.issued_at_ms)?,
                encode_u64(applied_at_ms)?,
                advisory.authority_signature.as_slice(),
                i64::from(revoked_capability_count),
                i64::from(u8::from(application_quarantined)),
            ],
        )?;

        transaction.commit()?;
        Ok(RecallApplyDecision::Applied(receipt))
    }

    /// Reads an immutable [`RecallExecutionReceipt`] by `advisory_id`.
    ///
    /// # Errors
    ///
    /// Fails closed on storage or decoding error.
    pub fn inspect_recall_receipt(
        &self,
        advisory_id: &[u8; 16],
    ) -> Result<Option<RecallExecutionReceipt>, StoreRegistryError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT receipt_id, idempotency_key, advisory_id, package_id, application_id,
                        target_version, severity, revocation_epoch, revoked_capability_count,
                        application_quarantined, applied_at_ms
                 FROM store_recall_advisories WHERE advisory_id = ?1",
                [advisory_id.as_slice()],
                decode_recall_receipt_tuple,
            )
            .optional()?
            .transpose()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreRegistryError> {
        self.connection
            .lock()
            .map_err(|_| StoreRegistryError::LockPoisoned)
    }
}

fn open_configured_connection(root: &Path) -> Result<Connection, StoreRegistryError> {
    if !root.to_string_lossy().starts_with("file:") {
        std::fs::create_dir_all(root).map_err(StoreRegistryError::Io)?;
    }
    let connection = Connection::open(root.join("store-registry.db"))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;

    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
        return Err(StoreRegistryError::DurabilityUnavailable {
            journal_mode,
            synchronous,
        });
    }
    Ok(connection)
}

fn load_governance_meta(
    connection: &Connection,
) -> Result<DurableGovernanceMeta, StoreRegistryError> {
    load_governance_meta_optional(connection)?.ok_or(StoreRegistryError::GovernanceUninitialized)
}

fn load_governance_meta_optional(
    connection: &Connection,
) -> Result<Option<DurableGovernanceMeta>, StoreRegistryError> {
    let row = connection
        .query_row(
            "SELECT store_revocation_epoch, certifier_public_key, entitlement_issuer_key_id,
                    entitlement_issuer_public_key, recall_authority_public_key, updated_at_ms
             FROM store_governance_meta WHERE singleton_id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(DurableGovernanceMeta {
        store_revocation_epoch: RevocationEpoch::new(decode_u64(row.0)?),
        certifier_public_key: decode_array::<32>(row.1, "certifier_public_key")?,
        entitlement_issuer_key_id: decode_id(
            row.2,
            KeyId::from_bytes,
            "entitlement_issuer_key_id",
        )?,
        entitlement_issuer_public_key: decode_array::<32>(row.3, "entitlement_issuer_public_key")?,
        recall_authority_public_key: decode_array::<32>(row.4, "recall_authority_public_key")?,
        updated_at_ms: decode_u64(row.5)?,
    }))
}

fn release_exists(
    connection: &Connection,
    package_id: PackageId,
    package_version: u64,
) -> Result<bool, StoreRegistryError> {
    let found = connection
        .query_row(
            "SELECT 1 FROM store_releases WHERE package_id = ?1 AND package_version = ?2",
            params![
                package_id.as_bytes().as_slice(),
                encode_u64(package_version)?,
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn load_application_owner_optional(
    connection: &Connection,
    application_id: ApplicationId,
) -> Result<Option<ApplicationOwnershipBinding>, StoreRegistryError> {
    let row = connection
        .query_row(
            "SELECT owner_publisher_id, trust_tier, first_registered_at_ms, registration_generation
             FROM store_application_owners WHERE application_id = ?1",
            [application_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    let owner_publisher_id = decode_id(row.0, PublisherId::from_bytes, "owner_publisher_id")?;
    let trust_tier = PublisherTrustTier::decode(row.1)
        .ok_or(StoreRegistryError::CorruptRecord("owner trust_tier"))?;
    Ok(Some(ApplicationOwnershipBinding {
        application_id,
        owner_publisher_id,
        trust_tier,
        first_registered_at_ms: decode_u64(row.2)?,
        registration_generation: decode_u64(row.3)?,
    }))
}

#[allow(clippy::too_many_lines)]
fn load_release_record_optional(
    connection: &Connection,
    package_id: PackageId,
    package_version: u64,
) -> Result<Option<(StoreReleaseRecord, Option<String>)>, StoreRegistryError> {
    let ver_i64 = encode_u64(package_version)?;
    let row = connection
        .query_row(
            "SELECT manifest_digest, oci_manifest_digest, publisher_id, publisher_trust_tier,
                    publisher_signing_key, publisher_signature, channel, track,
                    active_recall_severity, active_recall_reason, submitted_at_ms, updated_at_ms
             FROM store_releases WHERE package_id = ?1 AND package_version = ?2",
            params![package_id.as_bytes().as_slice(), ver_i64],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };

    let publisher_trust_tier = PublisherTrustTier::decode(row.3)
        .ok_or(StoreRegistryError::CorruptRecord("publisher_trust_tier"))?;
    let channel =
        StoreChannel::decode(row.6).ok_or(StoreRegistryError::CorruptRecord("store channel"))?;
    let track =
        StoreCatalogTrack::decode(row.7).ok_or(StoreRegistryError::CorruptRecord("store track"))?;
    let active_recall_severity = match row.8 {
        Some(code) => Some(
            RecallSeverity::decode(code)
                .ok_or(StoreRegistryError::CorruptRecord("active_recall_severity"))?,
        ),
        None => None,
    };

    let mut app_stmt = connection.prepare(
        "SELECT application_id FROM store_release_applications
         WHERE package_id = ?1 AND package_version = ?2
         ORDER BY app_ordinal ASC",
    )?;
    let mut app_rows = app_stmt.query(params![package_id.as_bytes().as_slice(), ver_i64])?;
    let mut application_ids = Vec::new();
    while let Some(app_row) = app_rows.next()? {
        application_ids.push(decode_id(
            app_row.get(0)?,
            ApplicationId::from_bytes,
            "application_id",
        )?);
    }
    drop(app_rows);
    drop(app_stmt);

    let mut comp_stmt = connection.prepare(
        "SELECT slot_name, component_kind, entrypoint, content_sha256, size_bytes
         FROM store_release_components
         WHERE package_id = ?1 AND package_version = ?2
         ORDER BY component_ordinal ASC",
    )?;
    let mut comp_rows = comp_stmt.query(params![package_id.as_bytes().as_slice(), ver_i64])?;
    let mut components = Vec::new();
    while let Some(comp_row) = comp_rows.next()? {
        let kind_code: i64 = comp_row.get(1)?;
        let kind = StoreComponentKind::decode(kind_code)
            .ok_or(StoreRegistryError::CorruptRecord("component_kind"))?;
        components.push(StoreComponentSlot {
            slot_name: comp_row.get(0)?,
            kind,
            entrypoint: comp_row.get(2)?,
            content_sha256: decode_array::<32>(comp_row.get(3)?, "component content_sha256")?,
            size_bytes: decode_u64(comp_row.get(4)?)?,
        });
    }
    drop(comp_rows);
    drop(comp_stmt);

    let mut perm_stmt = connection.prepare(
        "SELECT capability_scope, human_purpose, effect_class, network_egress_declared, sandbox_profile
         FROM store_permission_transparency
         WHERE package_id = ?1 AND package_version = ?2
         ORDER BY perm_ordinal ASC",
    )?;
    let mut perm_rows = perm_stmt.query(params![package_id.as_bytes().as_slice(), ver_i64])?;
    let mut declared_permissions = Vec::new();
    while let Some(perm_row) = perm_rows.next()? {
        let effect_code: i64 = perm_row.get(2)?;
        let effect_class = decode_effect_class(effect_code)
            .ok_or(StoreRegistryError::CorruptRecord("effect_class"))?;
        let egress_code: i64 = perm_row.get(3)?;
        let network_egress_declared = match egress_code {
            0 => false,
            1 => true,
            _ => {
                return Err(StoreRegistryError::CorruptRecord("network_egress_declared"));
            }
        };
        declared_permissions.push(DeclaredPermissionEntry {
            capability_scope: perm_row.get(0)?,
            human_purpose: perm_row.get(1)?,
            effect_class,
            network_egress_declared,
            sandbox_profile: perm_row.get(4)?,
        });
    }

    Ok(Some((
        StoreReleaseRecord {
            package_id,
            package_version,
            manifest_digest: decode_array::<32>(row.0, "manifest_digest")?,
            oci_manifest_digest: decode_array::<32>(row.1, "oci_manifest_digest")?,
            publisher_id: decode_id(row.2, PublisherId::from_bytes, "publisher_id")?,
            publisher_trust_tier,
            publisher_signing_key: decode_array::<32>(row.4, "publisher_signing_key")?,
            publisher_signature: decode_array::<64>(row.5, "publisher_signature")?,
            channel,
            track,
            active_recall_severity,
            application_ids,
            components,
            declared_permissions,
            submitted_at_ms: decode_u64(row.10)?,
            updated_at_ms: decode_u64(row.11)?,
        },
        row.9,
    )))
}

fn query_releases_by_predicate(
    connection: &Connection,
    predicate_and_order_sql: &str,
) -> Result<Vec<StoreReleaseRecord>, StoreRegistryError> {
    let sql =
        format!("SELECT package_id, package_version FROM store_releases {predicate_and_order_sql}");
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut keys = Vec::new();
    while let Some(row) = rows.next()? {
        let pkg = decode_id(row.get(0)?, PackageId::from_bytes, "package_id")?;
        let ver = decode_u64(row.get(1)?)?;
        keys.push((pkg, ver));
    }
    drop(rows);
    drop(statement);

    let mut out = Vec::with_capacity(keys.len());
    for (pkg, ver) in keys {
        let (record, _) = load_release_record_optional(connection, pkg, ver)?.ok_or(
            StoreRegistryError::CorruptRecord("release disappeared inside read"),
        )?;
        out.push(record);
    }
    Ok(out)
}

fn load_targeted_releases_for_recall(
    transaction: &Transaction<'_>,
    advisory: &SignedRecallAdvisory,
) -> Result<Vec<StoreReleaseRecord>, StoreRegistryError> {
    if let Some(target_version) = advisory.target_version {
        let (release, _) =
            load_release_record_optional(transaction, advisory.package_id, target_version)?.ok_or(
                StoreRegistryError::ReleaseNotFound {
                    package_id: advisory.package_id,
                    package_version: target_version,
                },
            )?;
        if !release.application_ids.contains(&advisory.application_id) {
            return Err(StoreRegistryError::ApplicationNotInRelease {
                package_id: release.package_id,
                package_version: release.package_version,
                application_id: advisory.application_id,
            });
        }
        return Ok(vec![release]);
    }

    let mut statement = transaction.prepare(
        "SELECT r.package_version
         FROM store_releases r
         INNER JOIN store_release_applications a
            ON r.package_id = a.package_id AND r.package_version = a.package_version
         WHERE r.package_id = ?1 AND a.application_id = ?2
         ORDER BY r.package_version ASC",
    )?;
    let mut rows = statement.query(params![
        advisory.package_id.as_bytes().as_slice(),
        advisory.application_id.as_bytes().as_slice(),
    ])?;
    let mut versions = Vec::new();
    while let Some(row) = rows.next()? {
        versions.push(decode_u64(row.get(0)?)?);
    }
    drop(rows);
    drop(statement);

    if versions.is_empty() {
        return Err(StoreRegistryError::ReleaseNotFound {
            package_id: advisory.package_id,
            package_version: 0,
        });
    }

    let mut releases = Vec::with_capacity(versions.len());
    for version in versions {
        let (release, _) = load_release_record_optional(transaction, advisory.package_id, version)?
            .ok_or(StoreRegistryError::CorruptRecord(
                "targeted recall release missing",
            ))?;
        releases.push(release);
    }
    Ok(releases)
}

fn load_granted_capability_optional(
    connection: &Connection,
    capability_id: CapabilityId,
) -> Result<Option<GrantedCapabilityBindingRecord>, StoreRegistryError> {
    connection
        .query_row(
            "SELECT capability_id, issued_generation, package_id, package_version,
                    application_id, installation_id, registered_at_ms,
                    revoked_by_advisory_id, revoked_resulting_generation, revoked_at_ms
             FROM store_granted_capabilities WHERE capability_id = ?1",
            [capability_id.as_bytes().as_slice()],
            |row| Ok(decode_granted_capability_row(row)),
        )
        .optional()?
        .transpose()
}

fn load_unrevoked_capabilities_for_recall(
    transaction: &Transaction<'_>,
    advisory: &SignedRecallAdvisory,
) -> Result<Vec<GrantedCapabilityBindingRecord>, StoreRegistryError> {
    let mut bindings = Vec::new();
    if let Some(target_version) = advisory.target_version {
        let mut stmt = transaction.prepare(
            "SELECT capability_id, issued_generation, package_id, package_version,
                    application_id, installation_id, registered_at_ms,
                    revoked_by_advisory_id, revoked_resulting_generation, revoked_at_ms
             FROM store_granted_capabilities
             WHERE package_id = ?1 AND application_id = ?2 AND package_version = ?3
               AND revoked_by_advisory_id IS NULL
             ORDER BY registered_at_ms ASC, capability_id ASC",
        )?;
        let mut rows = stmt.query(params![
            advisory.package_id.as_bytes().as_slice(),
            advisory.application_id.as_bytes().as_slice(),
            encode_u64(target_version)?,
        ])?;
        while let Some(row) = rows.next()? {
            bindings.push(decode_granted_capability_row(row)?);
        }
    } else {
        let mut stmt = transaction.prepare(
            "SELECT capability_id, issued_generation, package_id, package_version,
                    application_id, installation_id, registered_at_ms,
                    revoked_by_advisory_id, revoked_resulting_generation, revoked_at_ms
             FROM store_granted_capabilities
             WHERE package_id = ?1 AND application_id = ?2
               AND revoked_by_advisory_id IS NULL
             ORDER BY registered_at_ms ASC, capability_id ASC",
        )?;
        let mut rows = stmt.query(params![
            advisory.package_id.as_bytes().as_slice(),
            advisory.application_id.as_bytes().as_slice(),
        ])?;
        while let Some(row) = rows.next()? {
            bindings.push(decode_granted_capability_row(row)?);
        }
    }
    Ok(bindings)
}

fn decode_granted_capability_row(
    row: &rusqlite::Row<'_>,
) -> Result<GrantedCapabilityBindingRecord, StoreRegistryError> {
    let capability_id = decode_id(row.get(0)?, CapabilityId::from_bytes, "capability_id")?;
    let issued_generation = decode_generation(row.get(1)?)?;
    let package_id = decode_id(row.get(2)?, PackageId::from_bytes, "package_id")?;
    let package_version = decode_u64(row.get(3)?)?;
    let application_id = decode_id(row.get(4)?, ApplicationId::from_bytes, "application_id")?;
    let installation_id = match row.get::<_, Option<Vec<u8>>>(5)? {
        Some(bytes) => Some(decode_id(
            bytes,
            InstallationId::from_bytes,
            "installation_id",
        )?),
        None => None,
    };
    let registered_at_ms = decode_u64(row.get(6)?)?;
    let revoked_by_advisory_id = match row.get::<_, Option<Vec<u8>>>(7)? {
        Some(bytes) => Some(decode_array::<16>(bytes, "revoked_by_advisory_id")?),
        None => None,
    };
    let revoked_resulting_generation = match row.get::<_, Option<i64>>(8)? {
        Some(raw) => Some(decode_generation(raw)?),
        None => None,
    };
    let revoked_at_ms = match row.get::<_, Option<i64>>(9)? {
        Some(raw) => Some(decode_u64(raw)?),
        None => None,
    };

    Ok(GrantedCapabilityBindingRecord {
        capability_id,
        issued_generation,
        package_id,
        package_version,
        application_id,
        installation_id,
        registered_at_ms,
        revoked_by_advisory_id,
        revoked_resulting_generation,
        revoked_at_ms,
    })
}

fn promotion_request_digest(
    attestation: &SignedCertificationAttestation,
    idempotency_key: IdempotencyKey,
    now_ms: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PROMOTION_REQUEST_DOMAIN);
    hasher.update(attestation.signing_digest());
    hasher.update(attestation.ed25519_signature);
    hasher.update(idempotency_key.as_bytes());
    hasher.update(now_ms.to_be_bytes());
    hasher.finalize().into()
}

fn load_promotion_replay(
    transaction: &Transaction<'_>,
    idempotency_key: IdempotencyKey,
    expected_request_digest: [u8; 32],
) -> Result<Option<StoreCertificationPromotionReceipt>, StoreRegistryError> {
    let row = transaction
        .query_row(
            "SELECT request_digest, receipt_id, package_id, package_version,
                    application_id, passed_gates_mask, promoted_at_ms
             FROM store_certification_receipts WHERE idempotency_key = ?1",
            [idempotency_key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    if decode_array::<32>(row.0, "promotion request_digest")? != expected_request_digest {
        return Err(StoreRegistryError::IdempotencyConflict);
    }
    let mask =
        u8::try_from(row.5).map_err(|_| StoreRegistryError::CorruptRecord("passed_gates_mask"))?;
    Ok(Some(StoreCertificationPromotionReceipt {
        receipt_id: decode_id(row.1, ReceiptId::from_bytes, "receipt_id")?,
        idempotency_key,
        package_id: decode_id(row.2, PackageId::from_bytes, "package_id")?,
        package_version: decode_u64(row.3)?,
        application_id: decode_id(row.4, ApplicationId::from_bytes, "application_id")?,
        passed_gates_mask: mask,
        promoted_at_ms: decode_u64(row.6)?,
    }))
}

fn load_entitlement_replay(
    transaction: &Transaction<'_>,
    idempotency_key: IdempotencyKey,
    expected_request_digest: [u8; 32],
) -> Result<Option<EntitlementGrantReceipt>, StoreRegistryError> {
    let row = transaction
        .query_row(
            "SELECT request_digest, receipt_id, token_id, holder_principal,
                    application_id, package_id, package_version, granted_channel,
                    seat_ordinal, remaining_seats, store_revocation_epoch, redeemed_at_ms
             FROM store_entitlement_receipts WHERE idempotency_key = ?1",
            [idempotency_key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    if decode_array::<32>(row.0, "entitlement request_digest")? != expected_request_digest {
        return Err(StoreRegistryError::IdempotencyConflict);
    }
    let granted_channel =
        StoreChannel::decode(row.7).ok_or(StoreRegistryError::CorruptRecord("granted_channel"))?;
    Ok(Some(EntitlementGrantReceipt {
        receipt_id: decode_id(row.1, ReceiptId::from_bytes, "receipt_id")?,
        idempotency_key,
        token_id: decode_array::<16>(row.2, "token_id")?,
        holder_principal: decode_id(row.3, PrincipalId::from_bytes, "holder_principal")?,
        application_id: decode_id(row.4, ApplicationId::from_bytes, "application_id")?,
        package_id: decode_id(row.5, PackageId::from_bytes, "package_id")?,
        package_version: decode_u64(row.6)?,
        granted_channel,
        seat_ordinal: decode_u32(row.8, "seat_ordinal")?,
        remaining_seats: decode_u32(row.9, "remaining_seats")?,
        store_revocation_epoch: RevocationEpoch::new(decode_u64(row.10)?),
        redeemed_at_ms: decode_u64(row.11)?,
    }))
}

fn load_recall_replay(
    transaction: &Transaction<'_>,
    idempotency_key: IdempotencyKey,
    expected_request_digest: [u8; 32],
) -> Result<Option<RecallExecutionReceipt>, StoreRegistryError> {
    let row = transaction
        .query_row(
            "SELECT request_digest, receipt_id, idempotency_key, advisory_id, package_id,
                    application_id, target_version, severity, revocation_epoch,
                    revoked_capability_count, application_quarantined, applied_at_ms
             FROM store_recall_advisories WHERE idempotency_key = ?1",
            [idempotency_key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    if decode_array::<32>(row.0, "recall request_digest")? != expected_request_digest {
        return Err(StoreRegistryError::IdempotencyConflict);
    }
    let severity = RecallSeverity::decode(row.7)
        .ok_or(StoreRegistryError::CorruptRecord("recall severity"))?;
    let quarantined = match row.10 {
        0 => false,
        1 => true,
        _ => {
            return Err(StoreRegistryError::CorruptRecord("application_quarantined"));
        }
    };
    Ok(Some(RecallExecutionReceipt {
        receipt_id: decode_id(row.1, ReceiptId::from_bytes, "receipt_id")?,
        idempotency_key: decode_id(row.2, IdempotencyKey::from_bytes, "idempotency_key")?,
        advisory_id: decode_array::<16>(row.3, "advisory_id")?,
        package_id: decode_id(row.4, PackageId::from_bytes, "package_id")?,
        application_id: decode_id(row.5, ApplicationId::from_bytes, "application_id")?,
        target_version: row.6.map(decode_u64).transpose()?,
        severity,
        revocation_epoch: RevocationEpoch::new(decode_u64(row.8)?),
        revoked_capability_count: decode_u32(row.9, "revoked_capability_count")?,
        application_quarantined: quarantined,
        applied_at_ms: decode_u64(row.11)?,
    }))
}

fn decode_recall_receipt_tuple(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<RecallExecutionReceipt, StoreRegistryError>> {
    let receipt_raw: Vec<u8> = row.get(0)?;
    let idem_raw: Vec<u8> = row.get(1)?;
    let adv_raw: Vec<u8> = row.get(2)?;
    let pkg_raw: Vec<u8> = row.get(3)?;
    let app_raw: Vec<u8> = row.get(4)?;
    let target_ver_raw: Option<i64> = row.get(5)?;
    let sev_raw: i64 = row.get(6)?;
    let epoch_raw: i64 = row.get(7)?;
    let revoked_cnt_raw: i64 = row.get(8)?;
    let quarantined_raw: i64 = row.get(9)?;
    let applied_raw: i64 = row.get(10)?;

    Ok((|| {
        let severity = RecallSeverity::decode(sev_raw)
            .ok_or(StoreRegistryError::CorruptRecord("recall severity"))?;
        let application_quarantined = match quarantined_raw {
            0 => false,
            1 => true,
            _ => {
                return Err(StoreRegistryError::CorruptRecord("application_quarantined"));
            }
        };
        Ok(RecallExecutionReceipt {
            receipt_id: decode_id(receipt_raw, ReceiptId::from_bytes, "receipt_id")?,
            idempotency_key: decode_id(idem_raw, IdempotencyKey::from_bytes, "idempotency_key")?,
            advisory_id: decode_array::<16>(adv_raw, "advisory_id")?,
            package_id: decode_id(pkg_raw, PackageId::from_bytes, "package_id")?,
            application_id: decode_id(app_raw, ApplicationId::from_bytes, "application_id")?,
            target_version: target_ver_raw.map(decode_u64).transpose()?,
            severity,
            revocation_epoch: RevocationEpoch::new(decode_u64(epoch_raw)?),
            revoked_capability_count: decode_u32(revoked_cnt_raw, "revoked_capability_count")?,
            application_quarantined,
            applied_at_ms: decode_u64(applied_raw)?,
        })
    })())
}

fn derive_receipt_id(domain: &[u8], request_digest: &[u8; 32]) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(request_digest);
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(bytes)
}

fn derive_sub_idempotency_key(
    domain: &[u8],
    parent_key: IdempotencyKey,
    advisory_id: &[u8; 16],
    target_id: &[u8; 16],
) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(parent_key.as_bytes());
    hasher.update(advisory_id);
    hasher.update(target_id);
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(bytes)
}

fn encode_u64(value: u64) -> Result<i64, StoreRegistryError> {
    i64::try_from(value)
        .map_err(|_| StoreRegistryError::InvalidInput("u64 exceeds SQLite i64::MAX"))
}

fn encode_usize(value: usize) -> Result<i64, StoreRegistryError> {
    i64::try_from(value).map_err(|_| StoreRegistryError::CorruptRecord("ordinal exceeds i64::MAX"))
}

fn decode_u64(value: i64) -> Result<u64, StoreRegistryError> {
    u64::try_from(value).map_err(|_| StoreRegistryError::CorruptRecord("negative integer column"))
}

fn decode_u32(value: i64, _context: &'static str) -> Result<u32, StoreRegistryError> {
    u32::try_from(value).map_err(|_| StoreRegistryError::CorruptRecord("out-of-range u32 column"))
}

fn decode_generation(value: i64) -> Result<Generation, StoreRegistryError> {
    let raw = decode_u64(value)?;
    let non_zero = NonZeroU64::new(raw).ok_or(StoreRegistryError::CorruptRecord(
        "generation column is zero",
    ))?;
    Ok(Generation::new(non_zero))
}

fn decode_array<const N: usize>(
    raw: Vec<u8>,
    context: &'static str,
) -> Result<[u8; N], StoreRegistryError> {
    <[u8; N]>::try_from(raw).map_err(|_| StoreRegistryError::CorruptRecord(context))
}

fn decode_id<T>(
    raw: Vec<u8>,
    constructor: fn([u8; 16]) -> T,
    context: &'static str,
) -> Result<T, StoreRegistryError> {
    Ok(constructor(decode_array::<16>(raw, context)?))
}
