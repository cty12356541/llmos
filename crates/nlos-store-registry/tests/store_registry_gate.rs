//! Stage E `W79-E5A` integration gates for `nlos-store-registry` (`ADR-0027`
//! section 2.3; `[PKG-OCI-001]`, `[PKG-MULTI-001]`, `[PKG-APPID-001]`,
//! `[TUI-CONSENT-001]`, `[CAP-REVOKE-001]`, `[DUI-OFFLINE-001]`,
//! `[NLOS-DEFER-001]`).

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use nlos_application::{
    ApplicationAuthority, ApplicationStatus, InstallApplicationRequest, derive_application_id,
    pack_package_version,
};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
    PackageManifestEntry, ProvenanceSourceTriple, PutRevisionRequest, SignedPackage,
    VerifyPackageRequest, package_manifest_message,
};
use nlos_capability::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityHandle, CapabilityRights,
    CapabilityTarget, ConsumeCapabilityRequest, DelegateCapabilityRequest,
    IssueRootCapabilityRequest, SignedDelegateCapabilityRequest, SignedIssueRootCapabilityRequest,
    delegate_command_message, issue_root_command_message,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
    VerifySemanticSignatureRequest, semantic_signature_message,
};
use nlos_publisher_trust::{
    ApplicationOwnershipBinding, PublisherId, PublisherTrustCheckError, PublisherTrustTier,
    PublisherTrustVerifier, RevocationEpoch, VerifiedPublisherIdentity,
};
use nlos_store_registry::{
    DeclaredPermissionEntry, DirectEd25519RevocationAuthorizer, EffectClass,
    EntitlementRedeemDecision, LocalLaunchDisposition, LocalOciCasStore,
    NLOS_OCI_CONFIG_MEDIA_TYPE, OciCasError, OciLayerDescriptor, OciPackageLayout,
    REQUIRED_CERTIFICATION_GATES_MASK, RecallApplyDecision, RecallEnforcementAuthorities,
    RecallSeverity, RedeemEntitlementRequest, RegisterGrantedCapabilityRequest,
    SignedCertificationAttestation, SignedEntitlementToken, SignedRecallAdvisory,
    SqliteStoreRegistry, StoreCatalogTrack, StoreChannel, StoreComponentKind, StoreComponentSlot,
    StoreGovernanceAnchors, StorePromotionDecision, StoreRegistryError, StoreReleaseSubmission,
    UnsignedStoreReleaseSubmission, compute_entitlement_token_digest,
    compute_recall_advisory_digest, compute_release_submission_digest, compute_sha256,
};
use nlos_types::{
    ApplicationId, ArtifactId, Generation, IdempotencyKey, KeyId, NamespaceId, PackageId,
    PrincipalId, SemanticEventId,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let seq = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nlos-store-reg-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp test workspace");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
struct RegisteredTestPublisher {
    publisher_id: PublisherId,
    trust_tier: PublisherTrustTier,
    key_id: KeyId,
    public_key: [u8; 32],
}

/// Real `Ed25519`-verifying [`PublisherTrustVerifier`] test harness exercising
/// `nlos-store-registry` solely through the frozen cross-crate trait seam.
struct TestPublisherTrustHarness {
    epoch: RevocationEpoch,
    publishers: Mutex<HashMap<PublisherId, RegisteredTestPublisher>>,
    owners: Mutex<HashMap<ApplicationId, ApplicationOwnershipBinding>>,
}

impl TestPublisherTrustHarness {
    fn new(epoch: u64) -> Self {
        Self {
            epoch: RevocationEpoch::new(epoch),
            publishers: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
        }
    }

    fn register_publisher(
        &self,
        seed: u8,
        trust_tier: PublisherTrustTier,
    ) -> (PublisherId, SigningKey) {
        let publisher_id = PublisherId::from_bytes([seed; 16]);
        let key_id = KeyId::from_bytes([seed.wrapping_add(0x40); 16]);
        let signing_key = SigningKey::from_bytes(&[seed; 32]);
        let public_key = signing_key.verifying_key().to_bytes();
        self.publishers.lock().expect("publishers mutex").insert(
            publisher_id,
            RegisteredTestPublisher {
                publisher_id,
                trust_tier,
                key_id,
                public_key,
            },
        );
        (publisher_id, signing_key)
    }
}

impl PublisherTrustVerifier for TestPublisherTrustHarness {
    fn verify_publisher_signature(
        &self,
        claimed_publisher: PublisherId,
        signing_pubkey: &[u8; 32],
        payload_digest: &[u8; 32],
        ed25519_signature: &[u8; 64],
        now_ms: u64,
    ) -> Result<VerifiedPublisherIdentity, PublisherTrustCheckError> {
        let publishers = self.publishers.lock().expect("publishers mutex");
        let Some(entry) = publishers.get(&claimed_publisher).copied() else {
            return Err(PublisherTrustCheckError::UnknownPublisher(
                claimed_publisher,
            ));
        };
        if &entry.public_key != signing_pubkey {
            return Err(PublisherTrustCheckError::SignatureInvalid);
        }
        let verifying_key = VerifyingKey::from_bytes(signing_pubkey)
            .map_err(|_| PublisherTrustCheckError::SignatureInvalid)?;
        let signature = Signature::from_bytes(ed25519_signature);
        verifying_key
            .verify_strict(payload_digest, &signature)
            .map_err(|_| PublisherTrustCheckError::SignatureInvalid)?;
        Ok(VerifiedPublisherIdentity {
            publisher_id: entry.publisher_id,
            trust_tier: entry.trust_tier,
            leaf_key_id: entry.key_id,
            leaf_public_key: entry.public_key,
            revocation_epoch: self.epoch,
            verified_at_ms: now_ms,
        })
    }

    fn verify_or_bind_application_owner(
        &self,
        publisher_id: PublisherId,
        trust_tier: PublisherTrustTier,
        application_id: ApplicationId,
        now_ms: u64,
    ) -> Result<ApplicationOwnershipBinding, PublisherTrustCheckError> {
        let mut owners = self.owners.lock().expect("owners mutex");
        if let Some(existing) = owners.get(&application_id).copied() {
            if existing.owner_publisher_id != publisher_id {
                return Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
                    application_id,
                    registered_owner: existing.owner_publisher_id,
                    attempted_publisher: publisher_id,
                });
            }
            return Ok(existing);
        }
        let binding = ApplicationOwnershipBinding {
            application_id,
            owner_publisher_id: publisher_id,
            trust_tier,
            first_registered_at_ms: now_ms,
            registration_generation: 1,
        };
        owners.insert(application_id, binding);
        Ok(binding)
    }

    fn current_revocation_epoch(&self) -> Result<RevocationEpoch, PublisherTrustCheckError> {
        Ok(self.epoch)
    }
}

struct GovernanceSigners {
    certifier_key: SigningKey,
    entitlement_key_id: KeyId,
    entitlement_key: SigningKey,
    recall_key: SigningKey,
    anchors: StoreGovernanceAnchors,
}

impl GovernanceSigners {
    fn new(initial_epoch: u64) -> Self {
        let certifier_key = SigningKey::from_bytes(&[0xA1; 32]);
        let entitlement_key_id = KeyId::from_bytes([0xB2; 16]);
        let entitlement_key = SigningKey::from_bytes(&[0xB3; 32]);
        let recall_key = SigningKey::from_bytes(&[0xC4; 32]);
        let anchors = StoreGovernanceAnchors {
            certifier_public_key: certifier_key.verifying_key().to_bytes(),
            entitlement_issuer_key_id: entitlement_key_id,
            entitlement_issuer_public_key: entitlement_key.verifying_key().to_bytes(),
            recall_authority_public_key: recall_key.verifying_key().to_bytes(),
            initial_revocation_epoch: RevocationEpoch::new(initial_epoch),
            initialized_at_ms: 1_700_000_000_000,
        };
        Self {
            certifier_key,
            entitlement_key_id,
            entitlement_key,
            recall_key,
            anchors,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn sign_attestation(
        &self,
        package_id: PackageId,
        application_id: ApplicationId,
        package_version: u64,
        manifest_digest: [u8; 32],
        oci_manifest_digest: [u8; 32],
        publisher_id: PublisherId,
        passed_gates_mask: u8,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> SignedCertificationAttestation {
        let mut att = SignedCertificationAttestation {
            package_id,
            application_id,
            package_version,
            manifest_digest,
            oci_manifest_digest,
            publisher_id,
            passed_gates_mask,
            issued_at_ms,
            expires_at_ms,
            certifier_public_key: self.certifier_key.verifying_key().to_bytes(),
            ed25519_signature: [0u8; 64],
        };
        let digest = att.signing_digest();
        att.ed25519_signature = self.certifier_key.sign(&digest).to_bytes();
        att
    }

    #[allow(clippy::too_many_arguments)]
    fn sign_entitlement(
        &self,
        token_id: [u8; 16],
        holder_principal: PrincipalId,
        application_id: ApplicationId,
        allowed_channel: StoreChannel,
        max_seat_or_install_count: u32,
        issued_at_ms: u64,
        expires_at_ms: u64,
        min_revocation_epoch: u64,
    ) -> SignedEntitlementToken {
        let epoch = RevocationEpoch::new(min_revocation_epoch);
        let digest = compute_entitlement_token_digest(
            &token_id,
            holder_principal,
            application_id,
            allowed_channel,
            max_seat_or_install_count,
            issued_at_ms,
            expires_at_ms,
            epoch,
            self.entitlement_key_id,
        );
        SignedEntitlementToken {
            token_id,
            holder_principal,
            application_id,
            allowed_channel,
            max_seat_or_install_count,
            issued_at_ms,
            expires_at_ms,
            min_revocation_epoch: epoch,
            issuer_key_id: self.entitlement_key_id,
            ed25519_signature: self.entitlement_key.sign(&digest).to_bytes(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn sign_recall(
        &self,
        advisory_id: [u8; 16],
        package_id: PackageId,
        application_id: ApplicationId,
        target_version: Option<u64>,
        severity: RecallSeverity,
        revocation_epoch: u64,
        reason: &str,
        issued_at_ms: u64,
    ) -> SignedRecallAdvisory {
        let epoch = RevocationEpoch::new(revocation_epoch);
        let digest = compute_recall_advisory_digest(
            &advisory_id,
            package_id,
            application_id,
            target_version,
            severity,
            epoch,
            reason,
            issued_at_ms,
        );
        SignedRecallAdvisory {
            advisory_id,
            package_id,
            application_id,
            target_version,
            severity,
            revocation_epoch: epoch,
            reason: reason.to_owned(),
            issued_at_ms,
            authority_signature: self.recall_key.sign(&digest).to_bytes(),
        }
    }
}

/// Writes a 5-component composite `OCI` layout into `cas` and returns the
/// verified layout plus matching [`StoreComponentSlot`] declarations.
fn ingest_five_component_oci_layout(
    cas: &LocalOciCasStore,
    tag_seed: u8,
) -> (OciPackageLayout, Vec<StoreComponentSlot>) {
    let config_bytes = format!("{{\"package_seed\":{tag_seed},\"sabi\":\"1.0\"}}").into_bytes();
    let config_desc =
        OciLayerDescriptor::from_layer_bytes(NLOS_OCI_CONFIG_MEDIA_TYPE, &config_bytes)
            .expect("valid config descriptor");

    let payloads: [(&str, StoreComponentKind, &str, &str, Vec<u8>); 5] = [
        (
            "main-cli",
            StoreComponentKind::Executable,
            "bin/main.wasm",
            "application/vnd.nlos.component.executable.v1+wasm",
            vec![tag_seed, 0x01, 0x02, 0x03, 0x04],
        ),
        (
            "usb-bridge-driver",
            StoreComponentKind::Driver,
            "drivers/usb_bridge.wasm",
            "application/vnd.nlos.component.driver.v1+wasm",
            vec![tag_seed, 0x11, 0x12, 0x13, 0x14, 0x15],
        ),
        (
            "desktop-panel",
            StoreComponentKind::UiSurface,
            "surfaces/panel.duis",
            "application/vnd.nlos.component.ui-surface.v1+cbor",
            vec![tag_seed, 0x21, 0x22, 0x23],
        ),
        (
            "sync-indexer",
            StoreComponentKind::BackgroundService,
            "services/indexer.wasm",
            "application/vnd.nlos.component.service.v1+wasm",
            vec![tag_seed, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36],
        ),
        (
            "starter-notebook-seed",
            StoreComponentKind::ReadOnlySeedTemplate,
            "seeds/welcome.md",
            "application/vnd.nlos.component.seed.v1+markdown",
            b"# Read-Only Seed Template\n".to_vec(),
        ),
    ];

    let mut owned_layers: Vec<(OciLayerDescriptor, Vec<u8>)> = Vec::with_capacity(5);
    let mut slots: Vec<StoreComponentSlot> = Vec::with_capacity(5);
    for (slot_name, kind, entrypoint, media_type, bytes) in payloads {
        let desc =
            OciLayerDescriptor::from_layer_bytes(media_type, &bytes).expect("valid layer desc");
        slots.push(StoreComponentSlot {
            slot_name: slot_name.to_owned(),
            kind,
            entrypoint: entrypoint.to_owned(),
            content_sha256: desc.sha256_digest,
            size_bytes: desc.size_bytes,
        });
        owned_layers.push((desc, bytes));
    }

    let borrowed_layers: Vec<(OciLayerDescriptor, &[u8])> = owned_layers
        .iter()
        .map(|(desc, bytes)| (desc.clone(), bytes.as_slice()))
        .collect();
    let layout = cas
        .write_package_layout(&config_desc, &config_bytes, &borrowed_layers)
        .expect("write verified 5-layer OCI package layout");

    (layout, slots)
}

#[allow(clippy::too_many_arguments)]
fn build_signed_submission(
    package_id: PackageId,
    package_version: u64,
    manifest_digest: [u8; 32],
    oci_manifest_digest: [u8; 32],
    application_ids: Vec<ApplicationId>,
    components: Vec<StoreComponentSlot>,
    declared_permissions: Vec<DeclaredPermissionEntry>,
    publisher_id: PublisherId,
    publisher_key: &SigningKey,
    submitted_at_ms: u64,
) -> StoreReleaseSubmission {
    let digest = compute_release_submission_digest(&UnsignedStoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest,
        application_ids: &application_ids,
        components: &components,
        declared_permissions: &declared_permissions,
        publisher_id,
    });
    let signature = publisher_key.sign(&digest).to_bytes();
    StoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest,
        application_ids,
        components,
        declared_permissions,
        publisher_id,
        publisher_signing_key: publisher_key.verifying_key().to_bytes(),
        publisher_signature: signature,
        submitted_at_ms,
    }
}

#[test]
fn test_01_oci_cas_layer_tamper_rejection_and_store_governance_decoupling() {
    let ws = TempWorkspace::new("oci-cas-decoupling");
    let cas = LocalOciCasStore::open(ws.path().join("oci-cas")).expect("open OCI CAS");
    let signers = GovernanceSigners::new(1);
    let registry =
        SqliteStoreRegistry::open(ws.path().join("store-db"), signers.anchors).expect("open store");

    let valid_bytes = b"verified-wasm-component-payload";
    let valid_desc =
        OciLayerDescriptor::from_layer_bytes("application/vnd.nlos.wasm.v1", valid_bytes)
            .expect("valid descriptor");

    // 1. Bit-flipped layer payload must fail closed with LayerDigestMismatch.
    let mut tampered_bytes = *valid_bytes;
    tampered_bytes[0] ^= 0xFF;
    let digest_err = cas
        .put_layer_blob(&valid_desc, &tampered_bytes)
        .expect_err("tampered bytes must be rejected");
    assert!(matches!(
        digest_err,
        OciCasError::LayerDigestMismatch { .. }
    ));
    assert!(!cas.blob_path(&valid_desc.sha256_digest).exists());

    // 2. Truncated stream must fail closed with LayerSizeMismatch.
    let size_err = cas
        .put_layer_blob(&valid_desc, &valid_bytes[..10])
        .expect_err("truncated bytes must be rejected");
    assert!(matches!(
        size_err,
        OciCasError::LayerSizeMismatch {
            declared: 31,
            actual: 10
        }
    ));

    // 3. Empty layer slice and malformed media_type must fail closed.
    let cfg_desc =
        OciLayerDescriptor::from_layer_bytes(NLOS_OCI_CONFIG_MEDIA_TYPE, b"{}").expect("cfg");
    assert!(matches!(
        cas.write_package_layout(&cfg_desc, b"{}", &[]),
        Err(OciCasError::EmptyLayers)
    ));
    assert!(matches!(
        OciLayerDescriptor::from_layer_bytes("invalid media type", b"abc"),
        Err(OciCasError::InvalidMediaType(_))
    ));

    // 4. Valid 5-layer package layout writes cleanly into CAS, YET grants zero
    //    store catalog visibility or entitlement authorization ([PKG-OCI-001]).
    let (layout, _slots) = ingest_five_component_oci_layout(&cas, 0x10);
    let verified = cas
        .verify_stored_layout(&layout.oci_manifest_digest)
        .expect("stored OCI layout verifies bitwise");
    assert_eq!(verified, layout);

    let unsubmitted_pkg = PackageId::from_bytes([0x10; 16]);
    let unsubmitted_app = ApplicationId::from_bytes([0x20; 16]);
    let v1 = pack_package_version(1, 0, 0);
    assert_eq!(
        registry
            .inspect_release(unsubmitted_pkg, v1)
            .expect("inspect"),
        None,
        "OCI CAS ingestion alone must never create a store release row"
    );
    assert_eq!(
        registry
            .list_official_stable_releases()
            .expect("list stable"),
        Vec::new()
    );
    assert_eq!(
        registry
            .list_community_sandbox_candidates()
            .expect("list community"),
        Vec::new()
    );

    let holder = PrincipalId::from_bytes([0x30; 16]);
    let token = signers.sign_entitlement(
        [0x40; 16],
        holder,
        unsubmitted_app,
        StoreChannel::Candidate,
        1,
        1_700_000_000_000,
        1_700_000_100_000,
        1,
    );
    let redeem_err = registry
        .verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            token,
            presenter_principal: holder,
            requested_application_id: unsubmitted_app,
            package_id: unsubmitted_pkg,
            package_version: v1,
            idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
            now_ms: 1_700_000_010_000,
        })
        .expect_err("OCI CAS presence without store governance release must fail closed");
    assert!(matches!(
        redeem_err,
        StoreRegistryError::ReleaseNotFound { .. }
    ));

    // 5. Post-write on-disk blob tampering is caught on re-verification.
    let target_blob = cas.blob_path(&layout.layers[0].sha256_digest);
    let mut on_disk = fs::read(&target_blob).expect("read stored blob");
    on_disk[0] ^= 0x01;
    fs::write(&target_blob, &on_disk).expect("corrupt blob on disk");
    assert!(matches!(
        cas.verify_stored_layout(&layout.oci_manifest_digest),
        Err(OciCasError::LayerDigestMismatch { .. })
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_02_multi_component_catalog_appid_anti_hijack_community_isolation_and_four_gate_promotion() {
    let ws = TempWorkspace::new("catalog-appid-promotion");
    let cas = LocalOciCasStore::open(ws.path().join("oci-cas")).expect("open OCI CAS");
    let signers = GovernanceSigners::new(1);
    let registry =
        SqliteStoreRegistry::open(ws.path().join("store-db"), signers.anchors).expect("open store");

    let trust = TestPublisherTrustHarness::new(1);
    let (certified_pub, certified_key) =
        trust.register_publisher(0x11, PublisherTrustTier::CertifiedPublisher);
    let (community_pub, community_key) =
        trust.register_publisher(0x22, PublisherTrustTier::CommunitySandboxed);
    let (hijacker_pub, hijacker_key) =
        trust.register_publisher(0x33, PublisherTrustTier::CertifiedPublisher);

    // 1. Onboard a 5-component composite application from CertifiedPublisher.
    let (layout_official, slots_official) = ingest_five_component_oci_layout(&cas, 0x51);
    let pkg_official = PackageId::from_bytes([0x51; 16]);
    let app_shared_target = ApplicationId::from_bytes([0x61; 16]);
    let ver_1_0_0 = pack_package_version(1, 0, 0);
    let manifest_digest_official = compute_sha256(b"llmos.package.official.v1");
    let perms_official = vec![DeclaredPermissionEntry {
        capability_scope: "sabi.artifact.read".to_owned(),
        human_purpose: "Read workspace documents".to_owned(),
        effect_class: EffectClass::ReadOnly,
        network_egress_declared: false,
        sandbox_profile: "wasm-dual-layer-v1".to_owned(),
    }];

    let submission_official = build_signed_submission(
        pkg_official,
        ver_1_0_0,
        manifest_digest_official,
        layout_official.oci_manifest_digest,
        vec![app_shared_target],
        slots_official.clone(),
        perms_official,
        certified_pub,
        &certified_key,
        1_700_000_001_000,
    );
    let record_official = registry
        .submit_release(&cas, &trust, &submission_official)
        .expect("admit 5-component composite release");
    assert_eq!(record_official.channel, StoreChannel::Candidate);
    assert_eq!(record_official.track, StoreCatalogTrack::OfficialTrack);
    assert_eq!(record_official.components.len(), 5);
    assert_eq!(record_official.components, slots_official);

    let owner_binding = registry
        .inspect_application_owner(app_shared_target)
        .expect("query owner")
        .expect("owner bound");
    assert_eq!(owner_binding.owner_publisher_id, certified_pub);

    // 2. Anti-hijack ([PKG-APPID-001]): hijacker_pub attempts to claim both a
    //    brand-new app id AND certified_pub's app_shared_target in one submission.
    //    Must fail closed with zero dirty rows for either app or package!
    let (layout_hijack, slots_hijack) = ingest_five_component_oci_layout(&cas, 0x52);
    let pkg_hijack = PackageId::from_bytes([0x52; 16]);
    let app_hijacker_fresh = ApplicationId::from_bytes([0x62; 16]);
    let hijack_submission = build_signed_submission(
        pkg_hijack,
        ver_1_0_0,
        compute_sha256(b"llmos.package.hijack.v1"),
        layout_hijack.oci_manifest_digest,
        vec![app_hijacker_fresh, app_shared_target],
        slots_hijack,
        Vec::new(),
        hijacker_pub,
        &hijacker_key,
        1_700_000_002_000,
    );
    let hijack_err = registry
        .submit_release(&cas, &trust, &hijack_submission)
        .expect_err("cross-publisher ApplicationId hijack must fail closed");
    assert_eq!(
        match hijack_err {
            StoreRegistryError::PublisherTrust(inner) => Some(inner),
            _ => None,
        },
        Some(PublisherTrustCheckError::ApplicationIdHijackAttempt {
            application_id: app_shared_target,
            registered_owner: certified_pub,
            attempted_publisher: hijacker_pub,
        })
    );
    assert_eq!(
        registry.inspect_release(pkg_hijack, ver_1_0_0).expect("db"),
        None,
        "rejected hijack submission must leave zero release rows"
    );
    assert_eq!(
        registry
            .inspect_application_owner(app_hijacker_fresh)
            .expect("db"),
        None,
        "rejected hijack submission must leave zero partial owner bindings"
    );

    // 3. CommunitySandboxed isolation: admitted into CommunityTrack Candidate,
    //    never visible in official Stable, and strictly barred from Stable promotion.
    let (layout_comm, slots_comm) = ingest_five_component_oci_layout(&cas, 0x53);
    let pkg_comm = PackageId::from_bytes([0x53; 16]);
    let app_comm = ApplicationId::from_bytes([0x63; 16]);
    let manifest_digest_comm = compute_sha256(b"llmos.package.community.v1");
    let comm_submission = build_signed_submission(
        pkg_comm,
        ver_1_0_0,
        manifest_digest_comm,
        layout_comm.oci_manifest_digest,
        vec![app_comm],
        slots_comm,
        Vec::new(),
        community_pub,
        &community_key,
        1_700_000_003_000,
    );
    let comm_record = registry
        .submit_release(&cas, &trust, &comm_submission)
        .expect("admit community sandboxed candidate");
    assert_eq!(comm_record.channel, StoreChannel::Candidate);
    assert_eq!(comm_record.track, StoreCatalogTrack::CommunityTrack);
    assert_eq!(
        registry
            .list_community_sandbox_candidates()
            .expect("community list")
            .len(),
        1
    );
    assert_eq!(
        registry
            .list_official_stable_releases()
            .expect("official stable list"),
        Vec::new()
    );

    let comm_att = signers.sign_attestation(
        pkg_comm,
        app_comm,
        ver_1_0_0,
        manifest_digest_comm,
        layout_comm.oci_manifest_digest,
        community_pub,
        REQUIRED_CERTIFICATION_GATES_MASK,
        1_700_000_004_000,
        1_700_000_090_000,
    );
    let comm_promote_err = registry
        .promote_candidate_to_stable(
            &comm_att,
            IdempotencyKey::from_bytes([0x71; 16]),
            1_700_000_005_000,
        )
        .expect_err("CommunitySandboxed package must never enter official Stable");
    assert!(matches!(
        comm_promote_err,
        StoreRegistryError::CommunityPublisherForbiddenInStable {
            publisher_id,
            package_id,
        } if publisher_id == community_pub && package_id == pkg_comm
    ));

    // 4. Four-gate SignedCertificationAttestation negative gates on official Candidate.
    let incomplete_att = signers.sign_attestation(
        pkg_official,
        app_shared_target,
        ver_1_0_0,
        manifest_digest_official,
        layout_official.oci_manifest_digest,
        certified_pub,
        0b0111, // Gate 4 missing
        1_700_000_004_000,
        1_700_000_090_000,
    );
    assert!(matches!(
        registry.promote_candidate_to_stable(
            &incomplete_att,
            IdempotencyKey::from_bytes([0x72; 16]),
            1_700_000_005_000
        ),
        Err(StoreRegistryError::AttestationIncompleteGates {
            passed_gates_mask: 0b0111,
            required_mask: 0b1111
        })
    ));

    let valid_att = signers.sign_attestation(
        pkg_official,
        app_shared_target,
        ver_1_0_0,
        manifest_digest_official,
        layout_official.oci_manifest_digest,
        certified_pub,
        REQUIRED_CERTIFICATION_GATES_MASK,
        1_700_000_004_000,
        1_700_000_090_000,
    );

    // Forged signature check
    let mut forged_att = valid_att;
    forged_att.ed25519_signature[0] ^= 0x80;
    assert!(matches!(
        registry.promote_candidate_to_stable(
            &forged_att,
            IdempotencyKey::from_bytes([0x73; 16]),
            1_700_000_005_000
        ),
        Err(StoreRegistryError::AttestationSignatureInvalid)
    ));

    // Expired attestation check
    assert!(matches!(
        registry.promote_candidate_to_stable(
            &valid_att,
            IdempotencyKey::from_bytes([0x74; 16]),
            1_700_000_099_999
        ),
        Err(StoreRegistryError::AttestationExpired { .. })
    ));

    // 5. Valid 4-gate promotion commits Candidate -> Stable and replays idempotently.
    let promote_idem = IdempotencyKey::from_bytes([0x75; 16]);
    let promoted = registry
        .promote_candidate_to_stable(&valid_att, promote_idem, 1_700_000_005_000)
        .expect("4-gate promotion succeeds");
    assert!(matches!(promoted, StorePromotionDecision::Promoted(_)));

    let replayed = registry
        .promote_candidate_to_stable(&valid_att, promote_idem, 1_700_000_005_000)
        .expect("idempotent promotion replay");
    assert_eq!(
        replayed,
        StorePromotionDecision::Replayed(promoted.receipt())
    );

    let stable_list = registry
        .list_official_stable_releases()
        .expect("list official stable");
    assert_eq!(stable_list.len(), 1);
    assert_eq!(stable_list[0].package_id, pkg_official);
    assert_eq!(stable_list[0].channel, StoreChannel::Stable);
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_03_permission_transparency_ledger_and_cross_version_escalation_diff() {
    let ws = TempWorkspace::new("permission-transparency-diff");
    let cas = LocalOciCasStore::open(ws.path().join("oci-cas")).expect("open OCI CAS");
    let signers = GovernanceSigners::new(1);
    let registry =
        SqliteStoreRegistry::open(ws.path().join("store-db"), signers.anchors).expect("open store");
    let trust = TestPublisherTrustHarness::new(1);
    let (pub_id, pub_key) = trust.register_publisher(0x44, PublisherTrustTier::PlatformCore);

    let pkg_id = PackageId::from_bytes([0x81; 16]);
    let app_id = ApplicationId::from_bytes([0x82; 16]);
    let v1_0 = pack_package_version(1, 0, 0);
    let v1_1 = pack_package_version(1, 1, 0);
    let v2_0 = pack_package_version(2, 0, 0);

    let (layout_1, slots_1) = ingest_five_component_oci_layout(&cas, 0x81);
    let perms_v1_0 = vec![
        DeclaredPermissionEntry {
            capability_scope: "sabi.artifact.notes".to_owned(),
            human_purpose: "Read and edit local markdown notes".to_owned(),
            effect_class: EffectClass::Reversible,
            network_egress_declared: false,
            sandbox_profile: "wasm-strict-v1".to_owned(),
        },
        DeclaredPermissionEntry {
            capability_scope: "sabi.search.local".to_owned(),
            human_purpose: "Index local notes for search".to_owned(),
            effect_class: EffectClass::ReadOnly,
            network_egress_declared: false,
            sandbox_profile: "wasm-strict-v1".to_owned(),
        },
    ];
    registry
        .submit_release(
            &cas,
            &trust,
            &build_signed_submission(
                pkg_id,
                v1_0,
                compute_sha256(b"manifest-v1.0"),
                layout_1.oci_manifest_digest,
                vec![app_id],
                slots_1,
                perms_v1_0.clone(),
                pub_id,
                &pub_key,
                1_700_000_010_000,
            ),
        )
        .expect("submit v1.0");

    let report_v1 = registry
        .inspect_permission_transparency(pkg_id, v1_0)
        .expect("inspect transparency v1.0");
    assert!(!report_v1.any_network_egress);
    assert!(!report_v1.any_irreversible_effect);
    assert_eq!(report_v1.permissions.len(), 2);

    // v1.1 keeps identical permissions (no escalation -> silent update permitted).
    let (layout_1_1, slots_1_1) = ingest_five_component_oci_layout(&cas, 0x82);
    registry
        .submit_release(
            &cas,
            &trust,
            &build_signed_submission(
                pkg_id,
                v1_1,
                compute_sha256(b"manifest-v1.1"),
                layout_1_1.oci_manifest_digest,
                vec![app_id],
                slots_1_1,
                perms_v1_0,
                pub_id,
                &pub_key,
                1_700_000_020_000,
            ),
        )
        .expect("submit v1.1");

    let benign_diff = registry
        .diff_release_permission_transparency(pkg_id, v1_0, v1_1)
        .expect("diff v1.0 -> v1.1");
    assert!(!benign_diff.requires_tui_reconsent);

    // v2.0 adds camera scope, enables network egress, escalates notes to
    // Irreversible, and alters human purpose -> MUST trigger [TUI-CONSENT-001].
    let (layout_2, slots_2) = ingest_five_component_oci_layout(&cas, 0x83);
    let perms_v2_0 = vec![
        DeclaredPermissionEntry {
            capability_scope: "sabi.artifact.notes".to_owned(),
            human_purpose: "Sync and permanently purge cloud-backed notes".to_owned(),
            effect_class: EffectClass::Irreversible,
            network_egress_declared: true,
            sandbox_profile: "wasm-strict-v1".to_owned(),
        },
        DeclaredPermissionEntry {
            capability_scope: "sabi.hardware.camera".to_owned(),
            human_purpose: "Scan paper documents".to_owned(),
            effect_class: EffectClass::ReadOnly,
            network_egress_declared: false,
            sandbox_profile: "wasm-strict-v1".to_owned(),
        },
    ];
    registry
        .submit_release(
            &cas,
            &trust,
            &build_signed_submission(
                pkg_id,
                v2_0,
                compute_sha256(b"manifest-v2.0"),
                layout_2.oci_manifest_digest,
                vec![app_id],
                slots_2,
                perms_v2_0,
                pub_id,
                &pub_key,
                1_700_000_030_000,
            ),
        )
        .expect("submit v2.0");

    let escalation_diff = registry
        .diff_release_permission_transparency(pkg_id, v1_1, v2_0)
        .expect("diff v1.1 -> v2.0");
    assert!(
        escalation_diff.requires_tui_reconsent,
        "any privilege/egress/irreversible/purpose escalation must block silent upgrade"
    );
    assert_eq!(escalation_diff.added_capabilities.len(), 1);
    assert_eq!(
        escalation_diff.added_capabilities[0].capability_scope,
        "sabi.hardware.camera"
    );
    assert_eq!(
        escalation_diff.removed_capability_scopes,
        vec!["sabi.search.local"]
    );
    assert_eq!(
        escalation_diff.newly_enabled_network_egress,
        vec!["sabi.artifact.notes"]
    );
    assert_eq!(escalation_diff.escalated_to_irreversible.len(), 1);
    assert_eq!(
        escalation_diff.escalated_to_irreversible[0].capability_scope,
        "sabi.artifact.notes"
    );
    assert_eq!(escalation_diff.purpose_changes.len(), 1);
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_04_opaque_signed_entitlement_token_offline_verification_and_seat_accounting() {
    let ws = TempWorkspace::new("entitlement-token-seam");
    let cas = LocalOciCasStore::open(ws.path().join("oci-cas")).expect("open OCI CAS");
    let signers = GovernanceSigners::new(2);
    let registry =
        SqliteStoreRegistry::open(ws.path().join("store-db"), signers.anchors).expect("open store");
    let trust = TestPublisherTrustHarness::new(2);
    let (pub_id, pub_key) = trust.register_publisher(0x55, PublisherTrustTier::CertifiedPublisher);

    let pkg_id = PackageId::from_bytes([0x91; 16]);
    let app_id = ApplicationId::from_bytes([0x92; 16]);
    let other_app_id = ApplicationId::from_bytes([0x93; 16]);
    let ver = pack_package_version(1, 0, 0);
    let manifest_digest = compute_sha256(b"entitled-app-manifest-v1");
    let (layout, slots) = ingest_five_component_oci_layout(&cas, 0x91);

    registry
        .submit_release(
            &cas,
            &trust,
            &build_signed_submission(
                pkg_id,
                ver,
                manifest_digest,
                layout.oci_manifest_digest,
                vec![app_id],
                slots,
                Vec::new(),
                pub_id,
                &pub_key,
                1_700_000_001_000,
            ),
        )
        .expect("submit candidate release");

    let alice = PrincipalId::from_bytes([0xA5; 16]);
    let mallory = PrincipalId::from_bytes([0xA6; 16]);
    let token_id = [0xE1; 16];
    let token = signers.sign_entitlement(
        token_id,
        alice,
        app_id,
        StoreChannel::Stable,
        2, // 2-seat allowance
        1_700_000_010_000,
        1_700_000_090_000,
        2,
    );

    // 1. Token requires Stable channel, but release is still Candidate -> refused!
    let ch_err = registry
        .verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            token,
            presenter_principal: alice,
            requested_application_id: app_id,
            package_id: pkg_id,
            package_version: ver,
            idempotency_key: IdempotencyKey::from_bytes([0x01; 16]),
            now_ms: 1_700_000_020_000,
        })
        .expect_err("Stable-only entitlement must reject unpromoted Candidate release");
    assert!(matches!(
        ch_err,
        StoreRegistryError::EntitlementChannelDisallowed {
            allowed_channel: StoreChannel::Stable,
            release_channel: StoreChannel::Candidate,
        }
    ));

    // Promote release to Stable.
    let att = signers.sign_attestation(
        pkg_id,
        app_id,
        ver,
        manifest_digest,
        layout.oci_manifest_digest,
        pub_id,
        REQUIRED_CERTIFICATION_GATES_MASK,
        1_700_000_005_000,
        1_700_000_095_000,
    );
    registry
        .promote_candidate_to_stable(
            &att,
            IdempotencyKey::from_bytes([0x02; 16]),
            1_700_000_006_000,
        )
        .expect("promote to Stable");

    // 2. Negative gates: wrong principal, wrong app, expired, forged sig, unknown key, stale epoch.
    assert!(matches!(
        registry.verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            token,
            presenter_principal: mallory,
            requested_application_id: app_id,
            package_id: pkg_id,
            package_version: ver,
            idempotency_key: IdempotencyKey::from_bytes([0x03; 16]),
            now_ms: 1_700_000_020_000,
        }),
        Err(StoreRegistryError::EntitlementPrincipalMismatch { .. })
    ));

    assert!(matches!(
        registry.verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            token,
            presenter_principal: alice,
            requested_application_id: other_app_id,
            package_id: pkg_id,
            package_version: ver,
            idempotency_key: IdempotencyKey::from_bytes([0x04; 16]),
            now_ms: 1_700_000_020_000,
        }),
        Err(StoreRegistryError::EntitlementApplicationMismatch { .. })
    ));

    assert!(matches!(
        registry.verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            token,
            presenter_principal: alice,
            requested_application_id: app_id,
            package_id: pkg_id,
            package_version: ver,
            idempotency_key: IdempotencyKey::from_bytes([0x05; 16]),
            now_ms: 1_700_000_099_000,
        }),
        Err(StoreRegistryError::EntitlementExpired { .. })
    ));

    let mut forged_token = token;
    forged_token.ed25519_signature[7] ^= 0x55;
    assert!(matches!(
        registry.verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            token: forged_token,
            presenter_principal: alice,
            requested_application_id: app_id,
            package_id: pkg_id,
            package_version: ver,
            idempotency_key: IdempotencyKey::from_bytes([0x06; 16]),
            now_ms: 1_700_000_020_000,
        }),
        Err(StoreRegistryError::EntitlementSignatureInvalid)
    ));

    let mut unknown_key_token = token;
    unknown_key_token.issuer_key_id = KeyId::from_bytes([0xFF; 16]);
    assert!(matches!(
        registry.verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            token: unknown_key_token,
            presenter_principal: alice,
            requested_application_id: app_id,
            package_id: pkg_id,
            package_version: ver,
            idempotency_key: IdempotencyKey::from_bytes([0x07; 16]),
            now_ms: 1_700_000_020_000,
        }),
        Err(StoreRegistryError::EntitlementUnknownIssuerKey(_))
    ));

    let future_epoch_token = signers.sign_entitlement(
        [0xE2; 16],
        alice,
        app_id,
        StoreChannel::Stable,
        1,
        1_700_000_010_000,
        1_700_000_090_000,
        99, // local store is at epoch 2
    );
    assert!(matches!(
        registry.verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            token: future_epoch_token,
            presenter_principal: alice,
            requested_application_id: app_id,
            package_id: pkg_id,
            package_version: ver,
            idempotency_key: IdempotencyKey::from_bytes([0x08; 16]),
            now_ms: 1_700_000_020_000,
        }),
        Err(StoreRegistryError::EntitlementStoreEpochTooStale { .. })
    ));

    assert_eq!(
        registry
            .inspect_entitlement_redeemed_seats(&token_id)
            .expect("zero seats consumed after rejections"),
        0
    );

    // 3. Seat 1 redemption + exact-once idempotent replay without double-charging.
    let seat1_idem = IdempotencyKey::from_bytes([0x10; 16]);
    let seat1_req = RedeemEntitlementRequest {
        token,
        presenter_principal: alice,
        requested_application_id: app_id,
        package_id: pkg_id,
        package_version: ver,
        idempotency_key: seat1_idem,
        now_ms: 1_700_000_020_000,
    };
    let decision1 = registry
        .verify_and_redeem_entitlement(&seat1_req)
        .expect("redeem seat 1");
    assert!(matches!(decision1, EntitlementRedeemDecision::Granted(_)));
    assert_eq!(decision1.receipt().seat_ordinal, 1);
    assert_eq!(decision1.receipt().remaining_seats, 1);

    let replay1 = registry
        .verify_and_redeem_entitlement(&seat1_req)
        .expect("idempotent replay seat 1");
    assert_eq!(
        replay1,
        EntitlementRedeemDecision::Replayed(decision1.receipt())
    );
    assert_eq!(
        registry
            .inspect_entitlement_redeemed_seats(&token_id)
            .expect("still 1 seat"),
        1
    );

    // Reusing seat1_idem with altered timestamp must fail closed with IdempotencyConflict.
    let mut conflicting_req = seat1_req;
    conflicting_req.now_ms += 1;
    assert!(matches!(
        registry.verify_and_redeem_entitlement(&conflicting_req),
        Err(StoreRegistryError::IdempotencyConflict)
    ));

    // 4. Seat 2 redemption exhausts allowance; Seat 3 fails closed.
    let decision2 = registry
        .verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
            now_ms: 1_700_000_025_000,
            ..seat1_req
        })
        .expect("redeem seat 2");
    assert_eq!(decision2.receipt().seat_ordinal, 2);
    assert_eq!(decision2.receipt().remaining_seats, 0);

    let seat3_err = registry
        .verify_and_redeem_entitlement(&RedeemEntitlementRequest {
            idempotency_key: IdempotencyKey::from_bytes([0x12; 16]),
            now_ms: 1_700_000_030_000,
            ..seat1_req
        })
        .expect_err("seat 3 must fail closed");
    assert!(matches!(
        seat3_err,
        StoreRegistryError::EntitlementSeatLimitExceeded {
            max_seats: 2,
            redeemed_seats: 2,
            ..
        }
    ));
}

fn bootstrap_test_principal(
    identity: &IdentityAuthority,
    seed: u8,
) -> (SigningKey, IdentityBinding, [u8; 32]) {
    let raw_seed = [seed; 32];
    let signing_key = SigningKey::from_bytes(&raw_seed);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: signing_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 1_700_000_000_000,
            key_valid_until_ms: 1_800_000_000_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 1_700_000_000_000,
        })
        .expect("bootstrap principal")
        .binding();
    (signing_key, binding, raw_seed)
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_05_three_tier_recall_advisory_capability_revocation_application_quarantine_and_restart_recovery()
 {
    let ws = TempWorkspace::new("three-tier-recall-e2e");
    let identity_dir = ws.path().join("identity");
    let artifact_dir = ws.path().join("artifact");
    let application_dir = ws.path().join("application");
    let capability_dir = ws.path().join("capability");
    let cas_dir = ws.path().join("oci-cas");
    let store_dir = ws.path().join("store-db");
    let user_data_dir = ws.path().join("user-data-container");
    fs::create_dir_all(&user_data_dir).expect("create user data dir");

    // Persist simulated user workspace data that MUST remain untouched even
    // under Level 3 QuarantineInstallation (`ADR-0027` section 2.3.2).
    let user_notes_path = user_data_dir.join("workspace-notes.json");
    let precious_user_bytes =
        b"{\"owner\":\"local-user\",\"notes\":\"preserve across QuarantineInstallation\"}";
    fs::write(&user_notes_path, precious_user_bytes).expect("write user data");

    let identity = IdentityAuthority::open(&identity_dir).expect("open IdentityAuthority");
    let artifacts = ArtifactStore::open(&artifact_dir).expect("open ArtifactStore");
    let applications =
        ApplicationAuthority::open(&application_dir).expect("open ApplicationAuthority");
    let capabilities =
        CapabilityAuthority::open(&capability_dir).expect("open CapabilityAuthority");
    let cas = LocalOciCasStore::open(&cas_dir).expect("open OCI CAS");

    let signers = GovernanceSigners::new(1);
    let registry = SqliteStoreRegistry::open(&store_dir, signers.anchors).expect("open store");
    let trust = TestPublisherTrustHarness::new(1);
    let (pub_id, pub_key) = trust.register_publisher(0x66, PublisherTrustTier::CertifiedPublisher);

    let (operator_key, operator_binding, operator_seed) = bootstrap_test_principal(&identity, 0x71);
    let (runtime_key, runtime_binding, _runtime_seed) = bootstrap_test_principal(&identity, 0x72);

    // 1. Verify and install a real package into ArtifactStore + ApplicationAuthority.
    let package_id = PackageId::from_bytes([0xB1; 16]);
    let application_id = derive_application_id(package_id);
    let package_version = pack_package_version(1, 2, 0);
    let artifact_id = ArtifactId::from_bytes([0xB2; 16]);
    let wasm_bytes = b"(module ;; real composite app binary)";

    artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0xC1; 16]),
            content_type: "application/wasm".to_owned(),
            application_id: Some(application_id),
            owner: Some("store-e2e".to_owned()),
            created_at_ms: 1_700_000_001_000,
        })
        .expect("create artifact");
    let rev = artifacts
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: wasm_bytes,
            created_at_ms: 1_700_000_002_000,
            provenance: ProvenanceSourceTriple {
                source_a: [0x01; 16],
                source_b: [0x02; 16],
                source_digest: ContentDigest::of_bytes(wasm_bytes),
            },
        })
        .expect("put artifact revision");
    let entry_digest = match rev {
        nlos_artifact::PutRevisionDecision::Committed(rec)
        | nlos_artifact::PutRevisionDecision::Replayed(rec) => rec.digest,
    };

    let pkg_manifest = PackageManifest {
        package_id,
        version: package_version,
        entries: vec![PackageManifestEntry {
            name: "bin/main.wasm".to_owned(),
            artifact_id,
            digest: entry_digest,
            role: PackageEntryRole::Executable,
        }],
    };
    let manifest_msg = package_manifest_message(&pkg_manifest);
    let signed_pkg = SignedPackage {
        manifest: pkg_manifest,
        signer: operator_binding.principal_id,
        signature: operator_key.sign(&manifest_msg).to_bytes(),
    };
    let verify_decision = artifacts
        .verify_package(
            &identity,
            VerifyPackageRequest {
                signed: &signed_pkg,
                idempotency_key: IdempotencyKey::from_bytes([0xC2; 16]),
                verified_at_ms: 1_700_000_003_000,
            },
        )
        .expect("verify signed package");
    let install_receipt = applications
        .install_application(
            &artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verify_decision.receipt().receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0xC3; 16]),
                installed_at_ms: 1_700_000_004_000,
            },
        )
        .expect("install application")
        .receipt();
    assert_eq!(
        applications
            .inspect_application(package_id)
            .expect("inspect app")
            .expect("installed")
            .status,
        ApplicationStatus::Installed
    );

    // 2. Onboard and promote the release to Official Stable in SqliteStoreRegistry.
    let (layout, slots) = ingest_five_component_oci_layout(&cas, 0xB1);
    let manifest_digest_bytes = verify_decision.receipt().manifest_digest.into_bytes();
    registry
        .submit_release(
            &cas,
            &trust,
            &build_signed_submission(
                package_id,
                package_version,
                manifest_digest_bytes,
                layout.oci_manifest_digest,
                vec![application_id],
                slots,
                Vec::new(),
                pub_id,
                &pub_key,
                1_700_000_005_000,
            ),
        )
        .expect("submit store release");
    let att = signers.sign_attestation(
        package_id,
        application_id,
        package_version,
        manifest_digest_bytes,
        layout.oci_manifest_digest,
        pub_id,
        REQUIRED_CERTIFICATION_GATES_MASK,
        1_700_000_006_000,
        1_700_000_090_000,
    );
    registry
        .promote_candidate_to_stable(
            &att,
            IdempotencyKey::from_bytes([0xC4; 16]),
            1_700_000_007_000,
        )
        .expect("promote to Stable");

    // 3. Issue a root capability to runtime_binding AND delegate a child capability,
    //    then register the root capability in SqliteStoreRegistry.
    let target_ns = CapabilityTarget::Namespace(NamespaceId::from_bytes([0xD1; 16]));
    let root_cmd = IssueRootCapabilityRequest {
        issuer_key_id: operator_binding.key_id,
        holder_key_id: runtime_binding.key_id,
        target: target_ns,
        rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
        purpose_digest: None,
        valid_from_ms: 1_700_000_000_000,
        valid_until_ms: 1_800_000_000_000,
        delegation_depth_remaining: 2,
        call_limit: Some(10),
        idempotency_key: IdempotencyKey::from_bytes([0xC5; 16]),
        issued_at_ms: 1_700_000_008_000,
    };
    let root_cmd_msg = issue_root_command_message(root_cmd);
    let root_handle = capabilities
        .issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command: root_cmd,
                signer: operator_binding.principal_id,
                signature: operator_key.sign(&root_cmd_msg).to_bytes(),
            },
        )
        .expect("issue root capability")
        .record()
        .handle;

    let delegate_cmd = DelegateCapabilityRequest {
        parent: root_handle,
        delegator_key_id: runtime_binding.key_id,
        recipient_key_id: runtime_binding.key_id,
        target: target_ns,
        rights: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: None,
        valid_from_ms: 1_700_000_000_000,
        valid_until_ms: 1_800_000_000_000,
        delegation_depth_remaining: 1,
        call_limit: Some(5),
        idempotency_key: IdempotencyKey::from_bytes([0xC6; 16]),
        delegated_at_ms: 1_700_000_009_000,
    };
    let delegate_msg = delegate_command_message(delegate_cmd);
    let child_handle = capabilities
        .delegate_signed(
            &identity,
            SignedDelegateCapabilityRequest {
                command: delegate_cmd,
                signer: runtime_binding.principal_id,
                signature: runtime_key.sign(&delegate_msg).to_bytes(),
            },
        )
        .expect("delegate child capability")
        .record()
        .handle;

    registry
        .register_granted_capability(
            &capabilities,
            &RegisterGrantedCapabilityRequest {
                handle: root_handle,
                package_id,
                package_version,
                application_id,
                installation_id: Some(install_receipt.installation_id),
                registered_at_ms: 1_700_000_010_000,
            },
        )
        .expect("register granted capability");

    // Helper closure to exercise a capability via real IdentityAuthority + CapabilityAuthority.
    let consume_with_runtime = |caps: &CapabilityAuthority,
                                id_auth: &IdentityAuthority,
                                handle: CapabilityHandle,
                                idem_byte: u8,
                                at_ms: u64| {
        let event_id = SemanticEventId::from_bytes([idem_byte; 32]);
        let sem_msg = semantic_signature_message(event_id);
        let verified_signer = id_auth
            .verify_semantic_signature(VerifySemanticSignatureRequest {
                event_id,
                issuer: runtime_binding.principal_id,
                control_domain_id: runtime_binding.control_domain_id,
                key_id: runtime_binding.key_id,
                signature: runtime_key.sign(&sem_msg).to_bytes(),
                admitted_at_ms: at_ms,
            })
            .expect("verify runtime semantic signer");
        caps.consume(ConsumeCapabilityRequest {
            handle,
            signer: verified_signer,
            target: target_ns,
            required_right: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: None,
            idempotency_key: IdempotencyKey::from_bytes([idem_byte; 16]),
            consumed_at_ms: at_ms,
        })
    };

    // Baseline before recall: install gate open, launch gate Allowed, capability consumable.
    registry
        .evaluate_new_install_gate(package_id, package_version)
        .expect("baseline install allowed");
    assert_eq!(
        registry
            .evaluate_local_launch_gate(package_id, package_version)
            .expect("baseline launch allowed"),
        LocalLaunchDisposition::Allowed
    );
    consume_with_runtime(
        &capabilities,
        &identity,
        root_handle,
        0xD0,
        1_700_000_011_000,
    )
    .expect("baseline capability consume succeeds");

    // 4. Forged advisory signature & non-advancing epoch must fail closed.
    let mut forged_adv = signers.sign_recall(
        [0xF0; 16],
        package_id,
        application_id,
        Some(package_version),
        RecallSeverity::WarnNewInstall,
        2,
        "CVE-2026-0001 dependency warning",
        1_700_000_012_000,
    );
    forged_adv.authority_signature[0] ^= 0xFF;
    assert!(matches!(
        registry.apply_recall_advisory(
            &forged_adv,
            IdempotencyKey::from_bytes([0xE0; 16]),
            1_700_000_012_500,
            None,
        ),
        Err(StoreRegistryError::RecallSignatureInvalid)
    ));

    let stale_epoch_adv = signers.sign_recall(
        [0xF1; 16],
        package_id,
        application_id,
        Some(package_version),
        RecallSeverity::WarnNewInstall,
        1, // store is already at initial epoch 1
        "stale epoch",
        1_700_000_012_000,
    );
    assert!(matches!(
        registry.apply_recall_advisory(
            &stale_epoch_adv,
            IdempotencyKey::from_bytes([0xE1; 16]),
            1_700_000_012_500,
            None,
        ),
        Err(StoreRegistryError::RecallEpochRollback { .. })
    ));

    // 5. Level 1 Recall (WarnNewInstall, epoch = 2):
    //    Blocks new store installs & entitlement redemptions, while preserving
    //    existing offline local launch with risk warning and active capabilities.
    let adv_lvl1 = signers.sign_recall(
        [0xF2; 16],
        package_id,
        application_id,
        Some(package_version),
        RecallSeverity::WarnNewInstall,
        2,
        "Deprecated TLS cipher in optional sync plugin",
        1_700_000_013_000,
    );
    let receipt_lvl1 = registry
        .apply_recall_advisory(
            &adv_lvl1,
            IdempotencyKey::from_bytes([0xE2; 16]),
            1_700_000_013_500,
            None,
        )
        .expect("apply Level 1 WarnNewInstall")
        .receipt();
    assert_eq!(receipt_lvl1.revoked_capability_count, 0);
    assert!(!receipt_lvl1.application_quarantined);
    assert_eq!(
        registry.current_store_revocation_epoch().expect("epoch"),
        RevocationEpoch::new(2)
    );

    assert!(matches!(
        registry.evaluate_new_install_gate(package_id, package_version),
        Err(StoreRegistryError::ReleaseRecalled {
            severity: RecallSeverity::WarnNewInstall,
            ..
        })
    ));
    let launch_lvl1 = registry
        .evaluate_local_launch_gate(package_id, package_version)
        .expect("offline local launch preserved at Level 1");
    assert!(matches!(
        launch_lvl1,
        LocalLaunchDisposition::AllowedWithRiskWarning { .. }
    ));
    consume_with_runtime(
        &capabilities,
        &identity,
        root_handle,
        0xD1,
        1_700_000_014_000,
    )
    .expect("existing local capability stays active under WarnNewInstall");

    // 6. Level 2 Recall (BlockLaunchAndRevokeCap, epoch = 3):
    //    Transitions channel -> Recalled, blocks launch, and IMMEDIATELY revokes
    //    registered CapabilityId in CapabilityAuthority!
    let adv_lvl2 = signers.sign_recall(
        [0xF3; 16],
        package_id,
        application_id,
        Some(package_version),
        RecallSeverity::BlockLaunchAndRevokeCap,
        3,
        "Active credential exfiltration gadget in usb-bridge-driver",
        1_700_000_015_000,
    );

    // Without enforcement authorities, Level 2 must fail closed without advancing epoch.
    assert!(matches!(
        registry.apply_recall_advisory(
            &adv_lvl2,
            IdempotencyKey::from_bytes([0xE3; 16]),
            1_700_000_015_500,
            None,
        ),
        Err(StoreRegistryError::RecallCapabilityAuthoritiesRequired)
    ));
    assert_eq!(
        registry.current_store_revocation_epoch().expect("epoch"),
        RevocationEpoch::new(2)
    );

    let authorizer = DirectEd25519RevocationAuthorizer::from_seed(
        operator_binding.principal_id,
        operator_binding.key_id,
        &operator_seed,
    );
    let enforcement = RecallEnforcementAuthorities {
        identity: &identity,
        capabilities: &capabilities,
        revocation_authorizer: &authorizer,
        applications: Some(&applications),
    };
    let receipt_lvl2 = registry
        .apply_recall_advisory(
            &adv_lvl2,
            IdempotencyKey::from_bytes([0xE3; 16]),
            1_700_000_015_500,
            Some(&enforcement),
        )
        .expect("apply Level 2 BlockLaunchAndRevokeCap")
        .receipt();
    assert_eq!(receipt_lvl2.revoked_capability_count, 1);
    assert!(!receipt_lvl2.application_quarantined);
    assert_eq!(
        registry
            .inspect_release(package_id, package_version)
            .expect("inspect")
            .expect("exists")
            .channel,
        StoreChannel::Recalled
    );
    assert_eq!(
        registry
            .list_official_stable_releases()
            .expect("recalled package purged from official stable"),
        Vec::new()
    );
    assert!(matches!(
        registry.evaluate_local_launch_gate(package_id, package_version),
        Err(StoreRegistryError::ReleaseRecalled {
            severity: RecallSeverity::BlockLaunchAndRevokeCap,
            ..
        })
    ));

    // Verify immediate CapabilityAuthority revocation:
    // - Current generation (generation 2) fails closed with CapabilityRevoked!
    // - Pre-revocation handle (generation 1) fails closed with GenerationFenceConflict!
    // - Delegated child handle fails closed with AncestorRevokedOrFenced!
    let gen_2 = Generation::new(NonZeroU64::new(2).expect("non-zero"));
    let current_revoked_handle = CapabilityHandle {
        capability_id: root_handle.capability_id,
        generation: gen_2,
    };
    let consume_revoked_err = consume_with_runtime(
        &capabilities,
        &identity,
        current_revoked_handle,
        0xD2,
        1_700_000_016_000,
    )
    .expect_err("revoked capability at current generation must return CapabilityRevoked");
    assert!(matches!(
        consume_revoked_err,
        CapabilityAuthorityError::CapabilityRevoked
    ));

    let consume_stale_err = consume_with_runtime(
        &capabilities,
        &identity,
        root_handle,
        0xD3,
        1_700_000_016_000,
    )
    .expect_err("pre-revocation generation handle must be fenced");
    assert!(matches!(
        consume_stale_err,
        CapabilityAuthorityError::GenerationFenceConflict
    ));

    let consume_child_err = consume_with_runtime(
        &capabilities,
        &identity,
        child_handle,
        0xD4,
        1_700_000_016_000,
    )
    .expect_err("delegated child capability must be fenced immediately");
    assert!(matches!(
        consume_child_err,
        CapabilityAuthorityError::AncestorRevokedOrFenced
    ));

    // Application is still Installed at Level 2 (quarantine happens at Level 3).
    assert_eq!(
        applications
            .inspect_application(package_id)
            .expect("inspect app")
            .expect("exists")
            .status,
        ApplicationStatus::Installed
    );

    // 7. Level 3 Recall (QuarantineInstallation, epoch = 4):
    //    Transitions ApplicationAuthority -> ApplicationStatus::Disabled while
    //    preserving user data container bytes 100% untouched!
    let adv_lvl3 = signers.sign_recall(
        [0xF4; 16],
        package_id,
        application_id,
        None, // package-wide recall across all versions
        RecallSeverity::QuarantineInstallation,
        4,
        "Confirmed supply-chain compromise requiring local quarantine",
        1_700_000_017_000,
    );

    // Calling Level 3 without ApplicationAuthority must fail closed.
    let enforcement_no_app = RecallEnforcementAuthorities {
        identity: &identity,
        capabilities: &capabilities,
        revocation_authorizer: &authorizer,
        applications: None,
    };
    assert!(matches!(
        registry.apply_recall_advisory(
            &adv_lvl3,
            IdempotencyKey::from_bytes([0xE4; 16]),
            1_700_000_017_500,
            Some(&enforcement_no_app),
        ),
        Err(StoreRegistryError::RecallApplicationAuthorityRequired)
    ));

    let lvl3_idem = IdempotencyKey::from_bytes([0xE4; 16]);
    let decision_lvl3 = registry
        .apply_recall_advisory(&adv_lvl3, lvl3_idem, 1_700_000_017_500, Some(&enforcement))
        .expect("apply Level 3 QuarantineInstallation");
    assert!(matches!(decision_lvl3, RecallApplyDecision::Applied(_)));
    let receipt_lvl3 = decision_lvl3.receipt();
    assert!(receipt_lvl3.application_quarantined);
    assert_eq!(receipt_lvl3.revocation_epoch, RevocationEpoch::new(4));

    assert_eq!(
        applications
            .inspect_application(package_id)
            .expect("inspect app")
            .expect("exists")
            .status,
        ApplicationStatus::Disabled,
        "Level 3 QuarantineInstallation must transition local app to Disabled"
    );
    assert!(
        applications
            .inspect_disable_receipt(package_id)
            .expect("disable receipt query")
            .is_some()
    );
    assert_eq!(
        fs::read(&user_notes_path).expect("user data file still exists"),
        precious_user_bytes,
        "QuarantineInstallation must preserve user data untouched"
    );

    // 8. Anti-downgrade, idempotent replay, anti-rollback, and anti-repromotion.
    let replay_lvl3 = registry
        .apply_recall_advisory(&adv_lvl3, lvl3_idem, 1_700_000_017_500, None)
        .expect("idempotent replay of Level 3 recall");
    assert_eq!(
        replay_lvl3,
        RecallApplyDecision::Replayed(receipt_lvl3.clone())
    );

    let downgrade_adv = signers.sign_recall(
        [0xF5; 16],
        package_id,
        application_id,
        Some(package_version),
        RecallSeverity::WarnNewInstall,
        5,
        "Attempted severity downgrade",
        1_700_000_018_000,
    );
    assert!(matches!(
        registry.apply_recall_advisory(
            &downgrade_adv,
            IdempotencyKey::from_bytes([0xE5; 16]),
            1_700_000_018_500,
            Some(&enforcement),
        ),
        Err(StoreRegistryError::RecallSeverityDowngradeRefused {
            current_severity: RecallSeverity::QuarantineInstallation,
            attempted_severity: RecallSeverity::WarnNewInstall,
        })
    ));

    assert!(matches!(
        registry.promote_candidate_to_stable(
            &att,
            IdempotencyKey::from_bytes([0xE6; 16]),
            1_700_000_019_000,
        ),
        Err(StoreRegistryError::ReleaseRecalled {
            severity: RecallSeverity::QuarantineInstallation,
            ..
        })
    ));

    // 9. Drop all in-memory handles and verify full SQLite durable recovery across restart.
    drop(registry);
    drop(capabilities);
    drop(applications);
    drop(artifacts);
    drop(identity);

    let reopened_registry =
        SqliteStoreRegistry::reopen(&store_dir).expect("reopen SqliteStoreRegistry from disk");
    let reopened_identity =
        IdentityAuthority::open(&identity_dir).expect("reopen IdentityAuthority");
    let reopened_capabilities =
        CapabilityAuthority::open(&capability_dir).expect("reopen CapabilityAuthority");
    let reopened_applications =
        ApplicationAuthority::open(&application_dir).expect("reopen ApplicationAuthority");

    assert_eq!(
        reopened_registry
            .current_store_revocation_epoch()
            .expect("durable epoch"),
        RevocationEpoch::new(4)
    );
    let durable_release = reopened_registry
        .inspect_release(package_id, package_version)
        .expect("inspect release after restart")
        .expect("release persisted");
    assert_eq!(durable_release.channel, StoreChannel::Recalled);
    assert_eq!(
        durable_release.active_recall_severity,
        Some(RecallSeverity::QuarantineInstallation)
    );
    assert_eq!(
        reopened_registry
            .inspect_recall_receipt(&[0xF4; 16])
            .expect("inspect recall receipt"),
        Some(receipt_lvl3)
    );
    let durable_bindings = reopened_registry
        .list_granted_capabilities(package_id, application_id)
        .expect("list granted capabilities after restart");
    assert_eq!(durable_bindings.len(), 1);
    assert_eq!(durable_bindings[0].revoked_by_advisory_id, Some([0xF3; 16]));
    assert_eq!(
        durable_bindings[0].revoked_resulting_generation,
        Some(gen_2)
    );

    assert_eq!(
        reopened_applications
            .inspect_application(package_id)
            .expect("inspect app after restart")
            .expect("app persisted")
            .status,
        ApplicationStatus::Disabled
    );
    assert!(matches!(
        consume_with_runtime(
            &reopened_capabilities,
            &reopened_identity,
            current_revoked_handle,
            0xD5,
            1_700_000_020_000,
        ),
        Err(CapabilityAuthorityError::CapabilityRevoked)
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_06_governance_anchor_mismatch_and_sqlite_ddl_immutability_triggers() {
    let ws = TempWorkspace::new("ddl-immutability-triggers");
    let uninit_dir = ws.path().join("uninitialized-store");
    assert!(matches!(
        SqliteStoreRegistry::reopen(&uninit_dir),
        Err(StoreRegistryError::GovernanceUninitialized)
    ));

    let cas = LocalOciCasStore::open(ws.path().join("oci-cas")).expect("open OCI CAS");
    let signers = GovernanceSigners::new(5);
    let store_dir = ws.path().join("store-db");
    let registry = SqliteStoreRegistry::open(&store_dir, signers.anchors).expect("open store");

    let trust = TestPublisherTrustHarness::new(5);
    let (comm_pub, comm_key) =
        trust.register_publisher(0x77, PublisherTrustTier::CommunitySandboxed);
    let pkg_comm = PackageId::from_bytes([0x78; 16]);
    let app_comm = ApplicationId::from_bytes([0x79; 16]);
    let ver = pack_package_version(1, 0, 0);
    let (layout, slots) = ingest_five_component_oci_layout(&cas, 0x78);

    registry
        .submit_release(
            &cas,
            &trust,
            &build_signed_submission(
                pkg_comm,
                ver,
                compute_sha256(b"comm-ddl-manifest"),
                layout.oci_manifest_digest,
                vec![app_comm],
                slots,
                Vec::new(),
                comm_pub,
                &comm_key,
                1_700_000_001_000,
            ),
        )
        .expect("submit community release");

    let adv = signers.sign_recall(
        [0x88; 16],
        pkg_comm,
        app_comm,
        Some(ver),
        RecallSeverity::WarnNewInstall,
        6,
        "Community package warning",
        1_700_000_002_000,
    );
    registry
        .apply_recall_advisory(
            &adv,
            IdempotencyKey::from_bytes([0x89; 16]),
            1_700_000_002_500,
            None,
        )
        .expect("apply Level 1 recall");
    drop(registry);

    // 1. Reopening with a mutated certifier anchor must fail closed.
    let mut wrong_anchors = signers.anchors;
    wrong_anchors.certifier_public_key = SigningKey::from_bytes(&[0xEE; 32])
        .verifying_key()
        .to_bytes();
    assert!(matches!(
        SqliteStoreRegistry::open(&store_dir, wrong_anchors),
        Err(StoreRegistryError::GovernanceAnchorMismatch(
            "certifier_public_key"
        ))
    ));

    // 2. Direct SQLite DDL triggers physically abort epoch regression,
    //    CommunitySandboxed -> Stable bypass, owner mutation, and receipt deletion.
    let raw_conn = rusqlite::Connection::open(store_dir.join("store-registry.db"))
        .expect("open raw SQLite connection");
    assert!(
        raw_conn
            .execute(
                "UPDATE store_governance_meta SET store_revocation_epoch = 3 WHERE singleton_id = 1",
                [],
            )
            .is_err(),
        "trg_store_meta_monotonic_epoch must abort epoch regression"
    );
    assert!(
        raw_conn
            .execute("UPDATE store_releases SET channel = 2", [])
            .is_err(),
        "CHECK constraint must forbid CommunitySandboxed row entering Stable channel"
    );
    assert!(
        raw_conn
            .execute(
                "UPDATE store_releases SET active_recall_severity = NULL",
                []
            )
            .is_err(),
        "trg_store_releases_state_guard must abort clearing active recall"
    );
    assert!(
        raw_conn
            .execute("DELETE FROM store_application_owners", [])
            .is_err(),
        "trg_store_app_owners_no_delete must abort ApplicationId owner deletion"
    );
    assert!(
        raw_conn
            .execute("DELETE FROM store_recall_advisories", [])
            .is_err(),
        "trg_store_recall_advisories_no_delete must abort recall receipt deletion"
    );
}
