//! Stage E `W80-E6D` (`E6` right half) `H7` supply-chain and revocation
//! adversarial regression matrix, executing the six attack classes listed in
//! [stage-e-progress.md](../../../docs/management/stage-e-progress.md) §E.2.2
//! `E6` item 4 and §E.5.5 exit gate 2 against the REAL cross-crate
//! authorities (`nlos-publisher-trust` `SqlitePublisherTrustAuthority`, the
//! `PublisherTrustVerifier` trait consumed by `nlos-certify` /
//! `nlos-store-registry`, `LocalOciCasStore` + `SqliteStoreRegistry`, and the
//! `nlos-certify` Gate 3 / Gate 4 fault-injection sandboxes):
//!
//! | # | Attack class (§E.5.5 gate 2 clause) | Test function | Interception point |
//! |---|---|---|---|
//! | 1 | Tampered `OCI` blob (layer bytes / length / `SHA-256` mismatch) | [`h7_attack1_tampered_oci_blob_rejected_by_store_cas_and_certification_gate`] | `LocalOciCasStore` blob re-verification + `SqliteStoreRegistry::submit_release` pre-transaction `CAS` gate |
//! | 2 | Forged publisher leaf certificate (Ed25519 cert issued outside the trusted chain) | [`h7_attack2_forged_publisher_leaf_certificate_rejected_by_publisher_trust_verifier`] | `SqlitePublisherTrustAuthority` registration + `PublisherTrustVerifier::verify_publisher_signature` + store & certification receipt gates |
//! | 3 | Replayed stale / rolled-back `RevocationEpoch` (monotonic epoch regression, `SQLite` `trust_revocation_state_no_rollback` trigger) | [`h7_attack3_revocation_epoch_rollback_replay_and_sqlite_tamper_rejected`] | `import_signed_revocation_list` monotonic gate, raw `SQLite` trigger `RAISE(ABORT)`, `apply_recall_advisory` `RecallEpochRollback` |
//! | 4 | Cross-publisher squatting of an already-owned `(PublisherId, ApplicationId)` name | [`h7_attack4_cross_publisher_application_id_squatting_blocked_at_all_gates`] | `PublisherTrustVerifier::verify_or_bind_application_owner` + store registry + `StoreCertificationReceipt` issuance |
//! | 5 | Malicious upgrade crashing mid-flight (health-probe failure / half-written state) | [`h7_attack5_malicious_upgrade_health_fail_and_mid_crash_restore_bitwise_baseline`] | `nlos-certify` Gate 3 fault-injected `SHA-256` byte-equality atomic rollback drill |
//! | 6 | Post-uninstall permission / background-task / cache residue | [`h7_attack6_uninstall_zero_capability_background_and_cache_residue`] | `nlos-certify` Gate 4 zero-residue uninstall audit (`CAP-REVOKE-001`, `PKG-UNINSTALL-001`) |
//!
//! Every negative path asserts the concrete error variant (or exact
//! `&'static str` reason / message fragment) — never a bare `is_err()`.

#![forbid(unsafe_code)]
#![allow(clippy::too_many_lines)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::{CustodyTier, ProvisionCustodyKeyRequest, SigningKeyCustodyVault};
use nlos_certify::{
    CertifyError, CertifySeverity, Gate3DrillOptions, Gate4DrillOptions, GateExecutionSummary,
    GateId, GateStatus, IssueStoreCertificationRequest, LoadedCertifyPackage,
    PublisherSubmissionProof, StoreCertifierSigner, SurfaceProfile, derive_publisher_id_from_str,
    issue_store_certification_receipt, run_gate3_update_rollback_drill_with_options,
    run_gate4_uninstall_audit_with_options,
};
use nlos_identity::KeyPurpose;
use nlos_publisher_trust::{
    CrlImportDecision, IntermediateCertificateIssueSpec, LeafCertificateIssueSpec,
    OfflinePlatformRootCa, PublisherId, PublisherLeafCertificate, PublisherTrustCheckError,
    PublisherTrustTier, PublisherTrustVerifier, RevocationEpoch, RevocationListSpec,
    RevocationReason, RevokedCertificateEntry, SignedRevocationList, SqlitePublisherTrustAuthority,
    StoreIntermediateCertificate,
};
use nlos_store_registry::{
    LocalOciCasStore, NLOS_OCI_CONFIG_MEDIA_TYPE, OciCasError, OciLayerDescriptor,
    OciPackageLayout, RecallSeverity, SignedRecallAdvisory, SqliteStoreRegistry, StoreChannel,
    StoreComponentKind, StoreComponentSlot, StoreGovernanceAnchors, StoreRegistryError,
    StoreReleaseSubmission, UnsignedStoreReleaseSubmission, compute_recall_advisory_digest,
    compute_release_submission_digest, compute_sha256,
};
use nlos_types::{ApplicationId, ControlDomainId, IdempotencyKey, KeyId, PackageId, PrincipalId};
use rusqlite::Connection;

static NEXT_H7_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempH7Workspace(PathBuf);

impl TempH7Workspace {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock after UNIX_EPOCH")
            .as_nanos();
        let seq = NEXT_H7_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nlos-w80-h7-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create H7 temp workspace");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempH7Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn starter_assistant_v1_dir() -> PathBuf {
    repo_root().join("examples/starter-assistant-app/package")
}

fn starter_assistant_v1_1_dir() -> PathBuf {
    repo_root().join("examples/starter-assistant-app/package-v1.1.0")
}

/// Shared real-authority fixture: `SigningKeyCustodyVault` 3-tier PKI
/// (`OfflinePlatformRootCa -> StoreIntermediateCertificate -> CertifiedPublisher`
/// leaves for a legitimate publisher and a registered-but-malicious second
/// publisher) anchored into a durable `SqlitePublisherTrustAuthority` with an
/// imported `RevocationEpoch(1)` `SignedRevocationList`.
struct H7TrustFixture {
    ws: TempH7Workspace,
    vault: SigningKeyCustodyVault,
    trust: SqlitePublisherTrustAuthority,
    certified_ca: StoreIntermediateCertificate,
    legit_publisher: PublisherId,
    legit_sk: SigningKey,
    malicious_publisher: PublisherId,
    malicious_sk: SigningKey,
    malicious_leaf_serial: [u8; 16],
    certifier_sk: SigningKey,
    recall_sk: SigningKey,
    entitlement_sk: SigningKey,
    t0: u64,
}

impl H7TrustFixture {
    fn new(label: &str) -> Self {
        let t0: u64 = 1_761_868_800_000;
        let ws = TempH7Workspace::new(label);
        let vault = SigningKeyCustodyVault::new_in_memory([0xA7; 32]).expect("open KEK vault");

        let root_seed = [0x11; 32];
        let root_ca = OfflinePlatformRootCa::issue_self_signed(
            [0x10; 16],
            KeyId::from_bytes([0x12; 16]),
            &root_seed,
            t0 - 10_000,
            t0 + 86_400_000,
        )
        .expect("issue offline root CA");

        let certified_ca_key_id = KeyId::from_bytes([0x21; 16]);
        let certified_ca_slot = vault
            .provision_key_slot(ProvisionCustodyKeyRequest {
                key_id: certified_ca_key_id,
                principal_id: PrincipalId::from_bytes([0x22; 16]),
                control_domain_id: ControlDomainId::from_bytes([0x23; 16]),
                authority_id: [0x24; 16],
                key_purpose: KeyPurpose::SemanticSigning,
                custody_tier: CustodyTier::PlatformKeychainSealed,
                rotation_epoch: 1,
                valid_from_ms: t0 - 10_000,
                valid_until_ms: t0 + 86_400_000,
                seed: [0x25; 32],
                provisioned_at_ms: t0 - 10_000,
            })
            .expect("provision StoreIntermediateCertificate custody key slot");

        let certified_ca = root_ca
            .issue_intermediate_certificate(
                &root_seed,
                IntermediateCertificateIssueSpec {
                    serial_id: [0x26; 16],
                    intermediate_key_id: certified_ca_key_id,
                    intermediate_public_key: certified_ca_slot.public_key,
                    max_issuable_tier: PublisherTrustTier::CertifiedPublisher,
                    not_before_ms: t0 - 10_000,
                    expires_at_ms: t0 + 86_400_000,
                },
            )
            .expect("issue CertifiedPublisher StoreIntermediateCertificate");

        let trust = SqlitePublisherTrustAuthority::open(ws.path().join("publisher-trust.db"))
            .expect("open SqlitePublisherTrustAuthority");
        trust
            .anchor_offline_root_ca(&root_ca, t0 - 5_000)
            .expect("anchor offline root CA");
        trust
            .register_intermediate_certificate(&certified_ca, t0 - 5_000)
            .expect("register intermediate CA");

        let initial_crl = issue_crl(
            &certified_ca,
            &vault,
            RevocationEpoch::new(1),
            t0,
            t0 + 3_600_000,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            trust
                .import_signed_revocation_list(&initial_crl)
                .expect("import initial CRL"),
            CrlImportDecision::Advanced(RevocationEpoch::new(1))
        );

        let legit_publisher = derive_publisher_id_from_str("nlos.certified.h7-legit-supplier");
        let legit_sk = SigningKey::from_bytes(&[0x31; 32]);
        let (legit_leaf, _) = certified_ca
            .issue_leaf_certificate_under_custody(
                &vault,
                Some(1),
                t0 + 100,
                LeafCertificateIssueSpec {
                    serial_id: [0x32; 16],
                    publisher_id: legit_publisher,
                    trust_tier: PublisherTrustTier::CertifiedPublisher,
                    leaf_key_id: KeyId::from_bytes([0x33; 16]),
                    leaf_public_key: legit_sk.verifying_key().to_bytes(),
                    not_before_ms: t0 - 1_000,
                    expires_at_ms: t0 + 86_400_000,
                },
            )
            .expect("issue legitimate CertifiedPublisher leaf certificate");
        trust
            .register_leaf_certificate(&legit_leaf, t0 + 100)
            .expect("register legitimate leaf certificate");

        let malicious_publisher = derive_publisher_id_from_str("nlos.certified.h7-impostor");
        let malicious_sk = SigningKey::from_bytes(&[0x41; 32]);
        let malicious_leaf_serial = [0x42; 16];
        let (malicious_leaf, _) = certified_ca
            .issue_leaf_certificate_under_custody(
                &vault,
                Some(1),
                t0 + 200,
                LeafCertificateIssueSpec {
                    serial_id: malicious_leaf_serial,
                    publisher_id: malicious_publisher,
                    trust_tier: PublisherTrustTier::CertifiedPublisher,
                    leaf_key_id: KeyId::from_bytes([0x43; 16]),
                    leaf_public_key: malicious_sk.verifying_key().to_bytes(),
                    not_before_ms: t0 - 1_000,
                    expires_at_ms: t0 + 86_400_000,
                },
            )
            .expect("issue second CertifiedPublisher leaf certificate");
        trust
            .register_leaf_certificate(&malicious_leaf, t0 + 200)
            .expect("register second leaf certificate");

        Self {
            ws,
            vault,
            trust,
            certified_ca,
            legit_publisher,
            legit_sk,
            malicious_publisher,
            malicious_sk,
            malicious_leaf_serial,
            certifier_sk: SigningKey::from_bytes(&[0x55; 32]),
            recall_sk: SigningKey::from_bytes(&[0x63; 32]),
            entitlement_sk: SigningKey::from_bytes(&[0x62; 32]),
            t0,
        }
    }

    fn open_cas(&self) -> LocalOciCasStore {
        LocalOciCasStore::open(self.ws.path().join("oci-cas")).expect("open LocalOciCasStore")
    }

    fn open_store_registry(&self) -> SqliteStoreRegistry {
        let anchors = StoreGovernanceAnchors {
            certifier_public_key: self.certifier_sk.verifying_key().to_bytes(),
            entitlement_issuer_key_id: KeyId::from_bytes([0x61; 16]),
            entitlement_issuer_public_key: self.entitlement_sk.verifying_key().to_bytes(),
            recall_authority_public_key: self.recall_sk.verifying_key().to_bytes(),
            initial_revocation_epoch: RevocationEpoch::new(1),
            initialized_at_ms: self.t0,
        };
        SqliteStoreRegistry::open(self.ws.path().join("store-db"), anchors)
            .expect("open SqliteStoreRegistry")
    }

    fn certifier_signer(&self) -> StoreCertifierSigner<'_> {
        StoreCertifierSigner::Ed25519Key {
            key_id: KeyId::from_bytes([0x51; 16]),
            signing_key: &self.certifier_sk,
        }
    }
}

fn issue_crl(
    certified_ca: &StoreIntermediateCertificate,
    vault: &SigningKeyCustodyVault,
    revocation_epoch: RevocationEpoch,
    issued_at_ms: u64,
    fresh_until_ms: u64,
    revoked_certificates: Vec<RevokedCertificateEntry>,
    revoked_publishers: Vec<PublisherId>,
) -> SignedRevocationList {
    let (crl, _) = SignedRevocationList::issue_with_intermediate_custody(
        certified_ca,
        vault,
        Some(1),
        RevocationListSpec {
            revocation_epoch,
            issued_at_ms,
            fresh_until_ms,
            revoked_certificates,
            revoked_publishers,
        },
    )
    .expect("issue SignedRevocationList under intermediate CA custody");
    crl
}

/// Ingests a synthetic two-layer composite `OCI` package layout into the local
/// `CAS` and returns the verified layout plus matching component slots.
fn ingest_synthetic_oci_layout(
    cas: &LocalOciCasStore,
) -> (OciPackageLayout, Vec<StoreComponentSlot>) {
    let config_bytes = b"{\"schema\":\"h7-synthetic\",\"version\":\"1.0.0\"}\n".to_vec();
    let exec_bytes = b"h7-synthetic-executable-payload-v1".to_vec();
    let seed_bytes = b"h7-synthetic-seed-profile-v1".to_vec();

    let config_desc =
        OciLayerDescriptor::from_layer_bytes(NLOS_OCI_CONFIG_MEDIA_TYPE, &config_bytes)
            .expect("valid OCI config descriptor");
    let exec_desc = OciLayerDescriptor::from_layer_bytes(
        "application/vnd.nlos.component.executable.v1+bin",
        &exec_bytes,
    )
    .expect("valid executable layer");
    let seed_desc = OciLayerDescriptor::from_layer_bytes(
        "application/vnd.nlos.component.seed.v1+text",
        &seed_bytes,
    )
    .expect("valid seed layer");

    let mut slots = Vec::with_capacity(2);
    for (slot_name, kind, entrypoint, desc) in [
        (
            "h7-worker-exec",
            StoreComponentKind::Executable,
            "bin/h7-worker.bin",
            &exec_desc,
        ),
        (
            "h7-profile-seed",
            StoreComponentKind::ReadOnlySeedTemplate,
            "assets/h7-profile.txt",
            &seed_desc,
        ),
    ] {
        slots.push(StoreComponentSlot {
            slot_name: slot_name.to_owned(),
            kind,
            entrypoint: entrypoint.to_owned(),
            content_sha256: desc.sha256_digest,
            size_bytes: desc.size_bytes,
        });
    }

    let layout = cas
        .write_package_layout(
            &config_desc,
            &config_bytes,
            &[
                (exec_desc.clone(), exec_bytes.as_slice()),
                (seed_desc.clone(), seed_bytes.as_slice()),
            ],
        )
        .expect("write synthetic 2-layer OCI package layout");
    (layout, slots)
}

#[allow(clippy::too_many_arguments)]
fn build_signed_store_submission(
    package_id: PackageId,
    package_version: u64,
    oci_manifest_digest: [u8; 32],
    application_ids: Vec<ApplicationId>,
    components: Vec<StoreComponentSlot>,
    publisher_id: PublisherId,
    publisher_key: &SigningKey,
    submitted_at_ms: u64,
) -> StoreReleaseSubmission {
    let manifest_digest = [0x53; 32];
    let no_declared_permissions: Vec<nlos_store_registry::DeclaredPermissionEntry> = Vec::new();
    let digest = compute_release_submission_digest(&UnsignedStoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest,
        application_ids: &application_ids,
        components: &components,
        declared_permissions: &no_declared_permissions,
        publisher_id,
    });
    StoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest,
        application_ids,
        components,
        declared_permissions: Vec::new(),
        publisher_id,
        publisher_signing_key: publisher_key.verifying_key().to_bytes(),
        publisher_signature: publisher_key.sign(&digest).to_bytes(),
        submitted_at_ms,
    }
}

fn four_pass_gate_summaries() -> [GateExecutionSummary; 4] {
    [
        GateExecutionSummary::from_findings(
            GateId::Gate1ProtocolAndEffects,
            &[],
            b"h7-adversarial-pass",
            "Gate 1 PASS".to_owned(),
        ),
        GateExecutionSummary::from_findings(
            GateId::Gate2LeastPrivilegeAndSupplyChain,
            &[],
            b"h7-adversarial-pass",
            "Gate 2 PASS".to_owned(),
        ),
        GateExecutionSummary::from_findings(
            GateId::Gate3TransactionalUpdateAndRollback,
            &[],
            b"h7-adversarial-pass",
            "Gate 3 PASS".to_owned(),
        ),
        GateExecutionSummary::from_findings(
            GateId::Gate4ZeroResidueUninstall,
            &[],
            b"h7-adversarial-pass",
            "Gate 4 PASS".to_owned(),
        ),
    ]
}

fn blocker_rule_ids(summaries: &[nlos_certify::CertifyFinding]) -> Vec<String> {
    summaries
        .iter()
        .filter(|finding| finding.severity == CertifySeverity::Blocker)
        .map(|finding| finding.rule_id.clone())
        .collect()
}

// ===========================================================================
// Attack class 1: tampered OCI blobs (layer bytes / length / SHA-256 mismatch)
// must be rejected by the store CAS verifier AND the store certification gate.
// ===========================================================================
#[test]
fn h7_attack1_tampered_oci_blob_rejected_by_store_cas_and_certification_gate() {
    let fixture = H7TrustFixture::new("attack1-oci-tamper");
    let t0 = fixture.t0;
    let cas = fixture.open_cas();
    let registry = fixture.open_store_registry();
    let (layout, slots) = ingest_synthetic_oci_layout(&cas);

    let tampered_layer = layout.layers[0].clone();
    let blob_path = cas.blob_path(&tampered_layer.sha256_digest);
    let original_bytes = fs::read(&blob_path).expect("read original layer blob");
    let app_id = ApplicationId::from_bytes([0x52; 16]);
    let package_version = 1_u64 << 48; // pack_package_version(1, 0, 0)-style ordinal

    // --- 1a. Same-length byte substitution (SHA-256 digest tamper) ----------
    let mut substituted = original_bytes.clone();
    substituted[0] ^= 0xFF;
    fs::write(&blob_path, &substituted).expect("write tampered layer bytes");
    let substituted_digest = compute_sha256(&substituted);

    assert!(matches!(
        cas.read_verified_blob(&tampered_layer),
        Err(OciCasError::LayerDigestMismatch { declared, computed })
            if declared == tampered_layer.sha256_digest && computed == substituted_digest
    ));
    assert!(matches!(
        cas.verify_stored_layout(&layout.oci_manifest_digest),
        Err(OciCasError::LayerDigestMismatch { declared, computed })
            if declared == tampered_layer.sha256_digest && computed == substituted_digest
    ));

    let tamper_sub = build_signed_store_submission(
        PackageId::from_bytes([0x71; 16]),
        package_version,
        layout.oci_manifest_digest,
        vec![app_id],
        slots.clone(),
        fixture.legit_publisher,
        &fixture.legit_sk,
        t0 + 2_000,
    );
    let tamper_err = registry
        .submit_release(&cas, &fixture.trust, &tamper_sub)
        .expect_err("store certification gate must reject digest-tampered CAS blob");
    assert!(matches!(
        tamper_err,
        StoreRegistryError::OciCas(OciCasError::LayerDigestMismatch { declared, computed })
            if declared == tampered_layer.sha256_digest && computed == substituted_digest
    ));
    assert_eq!(
        registry
            .inspect_release(PackageId::from_bytes([0x71; 16]), package_version)
            .expect("inspect rejected tampered package"),
        None,
        "digest-tampered submission must commit zero catalog rows"
    );

    // Restore the original bytes and confirm the CAS recovers bitwise.
    fs::write(&blob_path, &original_bytes).expect("restore original layer blob");
    assert_eq!(
        cas.verify_stored_layout(&layout.oci_manifest_digest)
            .expect("restored layout must verify"),
        layout
    );

    // --- 1b. Truncation (byte-length mismatch) ------------------------------
    let truncated = original_bytes[..original_bytes.len() - 9].to_vec();
    fs::write(&blob_path, &truncated).expect("write truncated layer blob");
    let declared_len = u64::try_from(original_bytes.len()).expect("usize fits u64");
    let actual_len = u64::try_from(truncated.len()).expect("usize fits u64");

    assert!(matches!(
        cas.verify_stored_layout(&layout.oci_manifest_digest),
        Err(OciCasError::LayerSizeMismatch { declared, actual })
            if declared == declared_len && actual == actual_len
    ));
    let truncate_sub = build_signed_store_submission(
        PackageId::from_bytes([0x72; 16]),
        package_version,
        layout.oci_manifest_digest,
        vec![app_id],
        slots.clone(),
        fixture.legit_publisher,
        &fixture.legit_sk,
        t0 + 2_100,
    );
    let truncate_err = registry
        .submit_release(&cas, &fixture.trust, &truncate_sub)
        .expect_err("store certification gate must reject truncated CAS blob");
    assert!(matches!(
        truncate_err,
        StoreRegistryError::OciCas(OciCasError::LayerSizeMismatch { declared, actual })
            if declared == declared_len && actual == actual_len
    ));
    assert_eq!(
        registry
            .inspect_release(PackageId::from_bytes([0x72; 16]), package_version)
            .expect("inspect rejected truncated package"),
        None,
        "truncated submission must commit zero catalog rows"
    );
    fs::write(&blob_path, &original_bytes).expect("restore original layer blob again");

    // --- 1c. Forged component slot digest (blob not present in layout) ------
    let mut forged_slots = slots.clone();
    forged_slots[0].content_sha256 = [0xEE; 32];
    let forged_sub = build_signed_store_submission(
        PackageId::from_bytes([0x73; 16]),
        package_version,
        layout.oci_manifest_digest,
        vec![app_id],
        forged_slots,
        fixture.legit_publisher,
        &fixture.legit_sk,
        t0 + 2_200,
    );
    let forged_err = registry
        .submit_release(&cas, &fixture.trust, &forged_sub)
        .expect_err("store certification gate must reject forged slot digest");
    assert!(matches!(
        forged_err,
        StoreRegistryError::OciCas(OciCasError::BlobMissing { digest }) if digest == [0xEE; 32]
    ));
    assert_eq!(
        registry
            .inspect_release(PackageId::from_bytes([0x73; 16]), package_version)
            .expect("inspect rejected forged-digest package"),
        None,
        "forged-digest submission must commit zero catalog rows"
    );

    // --- 1d. Declared slot size lie (descriptor length mismatch) ------------
    let mut lying_slots = slots.clone();
    lying_slots[0].size_bytes += 7;
    let lying_sub = build_signed_store_submission(
        PackageId::from_bytes([0x74; 16]),
        package_version,
        layout.oci_manifest_digest,
        vec![app_id],
        lying_slots,
        fixture.legit_publisher,
        &fixture.legit_sk,
        t0 + 2_300,
    );
    let lying_err = registry
        .submit_release(&cas, &fixture.trust, &lying_sub)
        .expect_err("store certification gate must reject slot size lie");
    assert!(matches!(
        lying_err,
        StoreRegistryError::OciCas(OciCasError::LayerSizeMismatch { declared, actual })
            if declared == slots[0].size_bytes && actual == slots[0].size_bytes + 7
    ));
    assert_eq!(
        registry
            .inspect_release(PackageId::from_bytes([0x74; 16]), package_version)
            .expect("inspect rejected size-lie package"),
        None,
        "size-lie submission must commit zero catalog rows"
    );

    // --- Control: the intact CAS + legitimate publisher still onboards. -----
    let clean_sub = build_signed_store_submission(
        PackageId::from_bytes([0x75; 16]),
        package_version,
        layout.oci_manifest_digest,
        vec![app_id],
        slots,
        fixture.legit_publisher,
        &fixture.legit_sk,
        t0 + 2_400,
    );
    let clean = registry
        .submit_release(&cas, &fixture.trust, &clean_sub)
        .expect("intact CAS submission must onboard as Candidate");
    assert_eq!(clean.channel, StoreChannel::Candidate);
}

// ===========================================================================
// Attack class 2: forged publisher leaf certificates issued OUTSIDE the
// anchored trust chain (self-signed tier escalation, rogue intermediate CA,
// key substitution, unknown publishers) must fail closed at
// `PublisherTrustVerifier` and every downstream gate.
// ===========================================================================
#[test]
fn h7_attack2_forged_publisher_leaf_certificate_rejected_by_publisher_trust_verifier() {
    let fixture = H7TrustFixture::new("attack2-forged-leaf");
    let t0 = fixture.t0;
    let trust = &fixture.trust;

    // --- 2a. Self-signed impostor leaf claiming CertifiedPublisher tier -----
    let rogue_publisher = derive_publisher_id_from_str("nlos.certified.h7-rogue-selfsigned");
    let rogue_sk = SigningKey::from_bytes(&[0x91; 32]);
    let rogue_serial = [0x92; 16];
    let mut self_signed_claim = PublisherLeafCertificate {
        serial_id: rogue_serial,
        publisher_id: rogue_publisher,
        trust_tier: PublisherTrustTier::CertifiedPublisher,
        leaf_key_id: KeyId::from_bytes([0x93; 16]),
        leaf_public_key: rogue_sk.verifying_key().to_bytes(),
        not_before_ms: t0 - 1_000,
        expires_at_ms: t0 + 86_400_000,
        issuer_serial_id: rogue_serial, // self-signed
        signature: [0_u8; 64],
    };
    self_signed_claim.signature = rogue_sk
        .sign(&self_signed_claim.signing_digest())
        .to_bytes();
    assert_eq!(
        trust.register_leaf_certificate(&self_signed_claim, t0 + 300),
        Err(PublisherTrustCheckError::UntrustedCertificateChain(
            "self-signed leaf certificate is strictly confined to COMMUNITY_SANDBOXED"
        )),
        "self-signed certificate claiming CERTIFIED_PUBLISHER must fail closed at registration"
    );

    // --- 2b. Leaf "signed" by an unregistered rogue intermediate CA ---------
    let rogue_ca_serial = [0x94; 16];
    let rogue_ca_sk = SigningKey::from_bytes(&[0x95; 32]);
    let mut rogue_ca_leaf = PublisherLeafCertificate {
        serial_id: [0x96; 16],
        publisher_id: rogue_publisher,
        trust_tier: PublisherTrustTier::CertifiedPublisher,
        leaf_key_id: KeyId::from_bytes([0x97; 16]),
        leaf_public_key: rogue_ca_sk.verifying_key().to_bytes(),
        not_before_ms: t0 - 1_000,
        expires_at_ms: t0 + 86_400_000,
        issuer_serial_id: rogue_ca_serial, // attacker-controlled CA, not registered
        signature: [0_u8; 64],
    };
    rogue_ca_leaf.signature = rogue_ca_sk.sign(&rogue_ca_leaf.signing_digest()).to_bytes();
    assert_eq!(
        trust.register_leaf_certificate(&rogue_ca_leaf, t0 + 400),
        Err(PublisherTrustCheckError::UntrustedCertificateChain(
            "leaf certificate issuer_serial_id is not a registered StoreIntermediateCertificate"
        )),
        "leaf issued by an unregistered intermediate CA must fail closed at registration"
    );

    // --- 2c. Key substitution against the legitimate PublisherId -----------
    let message_digest = [0x33; 32];
    let impostor_sk = SigningKey::from_bytes(&[0x98; 32]);
    let impostor_public_key = impostor_sk.verifying_key().to_bytes();
    let impostor_signature = impostor_sk.sign(&message_digest).to_bytes();
    assert_eq!(
        trust.verify_publisher_signature(
            fixture.legit_publisher,
            &impostor_public_key,
            &message_digest,
            &impostor_signature,
            t0 + 500,
        ),
        Err(PublisherTrustCheckError::UntrustedCertificateChain(
            "signing public key is not bound to any registered certificate of publisher"
        )),
        "unregistered signing key impersonating a known PublisherId must fail closed"
    );

    // --- 2d. Fully unknown publisher at PublisherTrustVerifier --------------
    let rogue_public_key = rogue_sk.verifying_key().to_bytes();
    let rogue_signature = rogue_sk.sign(&message_digest).to_bytes();
    assert_eq!(
        trust.verify_publisher_signature(
            rogue_publisher,
            &rogue_public_key,
            &message_digest,
            &rogue_signature,
            t0 + 600,
        ),
        Err(PublisherTrustCheckError::UnknownPublisher(rogue_publisher))
    );

    // --- 2e. Store certification gate rejects the forged publisher ----------
    let cas = fixture.open_cas();
    let registry = fixture.open_store_registry();
    let (layout, slots) = ingest_synthetic_oci_layout(&cas);
    let rogue_submission = build_signed_store_submission(
        PackageId::from_bytes([0x81; 16]),
        1_u64 << 48,
        layout.oci_manifest_digest,
        vec![ApplicationId::from_bytes([0x82; 16])],
        slots,
        rogue_publisher,
        &rogue_sk,
        t0 + 700,
    );
    let store_err = registry
        .submit_release(&cas, trust, &rogue_submission)
        .expect_err("store gate must reject submission signed by an unregistered publisher");
    match store_err {
        StoreRegistryError::PublisherTrust(inner) => {
            assert_eq!(
                inner,
                PublisherTrustCheckError::UnknownPublisher(rogue_publisher)
            );
        }
        other => panic!("expected PublisherTrust(UnknownPublisher), got {other:?}"),
    }
    assert_eq!(
        registry
            .inspect_release(PackageId::from_bytes([0x81; 16]), 1_u64 << 48)
            .expect("inspect rejected forged-publisher package"),
        None,
        "forged-publisher submission must commit zero catalog rows"
    );

    // --- 2f. StoreCertificationReceipt issuance rejects the forged publisher
    let loaded_v1 =
        LoadedCertifyPackage::load(starter_assistant_v1_dir()).expect("load starter v1");
    let rogue_proof = PublisherSubmissionProof::for_package(rogue_publisher, &rogue_sk, &loaded_v1);
    let refused = issue_store_certification_receipt(
        trust,
        &fixture.certifier_signer(),
        IssueStoreCertificationRequest {
            submission_proof: rogue_proof,
            application_id: loaded_v1.application_id,
            package_id: loaded_v1.package_id,
            package_version: loaded_v1.base_manifest.version_raw.clone(),
            manifest_digest: loaded_v1.manifest_digest,
            surface_profile: SurfaceProfile::ApplicationPlatform,
            target_sabi_epoch: 3,
            gate_summaries: four_pass_gate_summaries(),
            issued_at_ms: t0 + 800,
            expires_at_ms: t0 + 86_400_000,
        },
    );
    assert!(matches!(
        refused,
        Err(CertifyError::PublisherTrust(
            PublisherTrustCheckError::UnknownPublisher(publisher_id)
        )) if publisher_id == rogue_publisher
    ));
}

// ===========================================================================
// Attack class 3: replaying a stale / rolled-back RevocationEpoch — at the
// signed-CRL import gate, via a raw SQLite downgrade attack against the
// `trust_revocation_state_no_rollback` trigger, via an un-revoke DELETE
// attack, and at the store recall advisory boundary.
// ===========================================================================
#[test]
fn h7_attack3_revocation_epoch_rollback_replay_and_sqlite_tamper_rejected() {
    let fixture = H7TrustFixture::new("attack3-epoch-rollback");
    let t0 = fixture.t0;
    let trust = &fixture.trust;

    // Advance to epoch 2, revoking the malicious publisher and its leaf cert.
    let epoch2_crl = issue_crl(
        &fixture.certified_ca,
        &fixture.vault,
        RevocationEpoch::new(2),
        t0 + 1_000,
        t0 + 3_600_000,
        vec![RevokedCertificateEntry {
            serial_or_key: fixture.malicious_leaf_serial,
            reason: RevocationReason::MaliciousPackage,
        }],
        vec![fixture.malicious_publisher],
    );
    assert_eq!(
        trust
            .import_signed_revocation_list(&epoch2_crl)
            .expect("advance to epoch 2"),
        CrlImportDecision::Advanced(RevocationEpoch::new(2))
    );

    // --- 3a. Replay of the stale epoch-1 CRL --------------------------------
    let stale_crl = issue_crl(
        &fixture.certified_ca,
        &fixture.vault,
        RevocationEpoch::new(1),
        t0,
        t0 + 3_600_000,
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(
        trust.import_signed_revocation_list(&stale_crl),
        Err(PublisherTrustCheckError::StaleOrRollbackRevocationEpoch {
            current_epoch: RevocationEpoch::new(2),
            candidate_epoch: RevocationEpoch::new(1),
        }),
        "epoch-1 CRL replay after epoch 2 must fail closed"
    );

    // --- 3b. Same-epoch equivocation (epoch 2 with a different digest) ------
    let equivocating_crl = issue_crl(
        &fixture.certified_ca,
        &fixture.vault,
        RevocationEpoch::new(2),
        t0 + 1_500,
        t0 + 3_600_000,
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(
        trust.import_signed_revocation_list(&equivocating_crl),
        Err(PublisherTrustCheckError::StaleOrRollbackRevocationEpoch {
            current_epoch: RevocationEpoch::new(2),
            candidate_epoch: RevocationEpoch::new(2),
        }),
        "same-epoch CRL with a different digest must fail closed as equivocation"
    );

    // --- 3c. Raw SQLite epoch downgrade against the no_rollback trigger -----
    let raw_conn = Connection::open(fixture.ws.path().join("publisher-trust.db"))
        .expect("open raw SQLite connection to trust ledger");
    raw_conn
        .busy_timeout(std::time::Duration::from_secs(5))
        .expect("set busy timeout");
    let downgrade_err = raw_conn
        .execute(
            "UPDATE trust_revocation_state SET revocation_epoch = 1 WHERE singleton_id = 1",
            [],
        )
        .expect_err("raw SQLite epoch downgrade must be aborted by trigger");
    assert!(
        downgrade_err
            .to_string()
            .contains("revocation_epoch rollback is forbidden"),
        "expected trust_revocation_state_no_rollback trigger abort, got {downgrade_err}"
    );
    drop(raw_conn);
    assert_eq!(
        trust
            .inspect_revocation_state()
            .expect("inspect revocation state")
            .revocation_epoch,
        RevocationEpoch::new(2),
        "durable epoch must remain 2 after the failed downgrade attack"
    );

    // --- 3d. Un-revoke DELETE attack + revoked publisher still fails closed -
    let raw_conn2 = Connection::open(fixture.ws.path().join("publisher-trust.db"))
        .expect("reopen raw SQLite connection");
    raw_conn2
        .busy_timeout(std::time::Duration::from_secs(5))
        .expect("set busy timeout");
    let unrevoke_err = raw_conn2
        .execute("DELETE FROM revoked_certificates", [])
        .expect_err("un-revoke DELETE must be aborted by trigger");
    assert!(
        unrevoke_err
            .to_string()
            .contains("revoked_certificates rows cannot be deleted"),
        "expected revoked_certificates_no_delete trigger abort, got {unrevoke_err}"
    );
    drop(raw_conn2);

    let message_digest = [0x44; 32];
    let malicious_pk = fixture.malicious_sk.verifying_key().to_bytes();
    let malicious_sig = fixture.malicious_sk.sign(&message_digest).to_bytes();
    assert_eq!(
        trust.verify_publisher_signature(
            fixture.malicious_publisher,
            &malicious_pk,
            &message_digest,
            &malicious_sig,
            t0 + 2_000,
        ),
        Err(PublisherTrustCheckError::RevokedPublisher {
            publisher_id: fixture.malicious_publisher,
            revoked_at_epoch: RevocationEpoch::new(2),
        }),
        "revoked publisher must remain unusable after the failed un-revoke attack"
    );

    // --- 3e. Store-side recall advisory epoch replay ------------------------
    let registry = fixture.open_store_registry();
    let advisory_id = [0xD1; 16];
    let package_id = PackageId::from_bytes([0xD2; 16]);
    let application_id = ApplicationId::from_bytes([0xD3; 16]);
    let replay_epoch = RevocationEpoch::new(1); // == durable store epoch
    let reason = "h7 adversarial: replayed stale recall advisory";
    let recall_digest = compute_recall_advisory_digest(
        &advisory_id,
        package_id,
        application_id,
        Some(1_u64 << 48),
        RecallSeverity::WarnNewInstall,
        replay_epoch,
        reason,
        t0 + 3_000,
    );
    let replayed_advisory = SignedRecallAdvisory {
        advisory_id,
        package_id,
        application_id,
        target_version: Some(1_u64 << 48),
        severity: RecallSeverity::WarnNewInstall,
        revocation_epoch: replay_epoch,
        reason: reason.to_owned(),
        issued_at_ms: t0 + 3_000,
        authority_signature: fixture.recall_sk.sign(&recall_digest).to_bytes(),
    };
    let recall_err = registry
        .apply_recall_advisory(
            &replayed_advisory,
            IdempotencyKey::from_bytes([0xD4; 16]),
            t0 + 3_100,
            None,
        )
        .expect_err("non-advancing recall advisory epoch must be rejected");
    assert!(matches!(
        recall_err,
        StoreRegistryError::RecallEpochRollback {
            current_epoch,
            candidate_epoch,
        } if current_epoch == RevocationEpoch::new(1) && candidate_epoch == RevocationEpoch::new(1)
    ));
}

// ===========================================================================
// Attack class 4: a second (even CertifiedPublisher-tier) publisher squatting
// an ApplicationId already owned by another publisher must be blocked at the
// trust authority, the store registry boundary, and the certification receipt
// issuance gate — with zero committed rows.
// ===========================================================================
#[test]
fn h7_attack4_cross_publisher_application_id_squatting_blocked_at_all_gates() {
    let fixture = H7TrustFixture::new("attack4-appid-squat");
    let t0 = fixture.t0;
    let trust = &fixture.trust;
    let cas = fixture.open_cas();
    let registry = fixture.open_store_registry();
    let (layout, slots) = ingest_synthetic_oci_layout(&cas);

    let legit_pkg = PackageId::from_bytes([0xB1; 16]);
    let hijacked_app = ApplicationId::from_bytes([0xB2; 16]);
    let package_version = 1_u64 << 48;

    // Legitimate first registration binds (hijacked_app -> legit publisher).
    let legit_sub = build_signed_store_submission(
        legit_pkg,
        package_version,
        layout.oci_manifest_digest,
        vec![hijacked_app],
        slots.clone(),
        fixture.legit_publisher,
        &fixture.legit_sk,
        t0 + 2_000,
    );
    let candidate = registry
        .submit_release(&cas, trust, &legit_sub)
        .expect("legitimate submission onboards");
    assert_eq!(candidate.channel, StoreChannel::Candidate);

    // --- 4a. Trust authority anti-hijack ------------------------------------
    assert_eq!(
        trust.verify_or_bind_application_owner(
            fixture.malicious_publisher,
            PublisherTrustTier::CertifiedPublisher,
            hijacked_app,
            t0 + 2_500,
        ),
        Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
            application_id: hijacked_app,
            registered_owner: fixture.legit_publisher,
            attempted_publisher: fixture.malicious_publisher,
        })
    );

    // --- 4b. Store registry boundary anti-hijack (zero dirty rows) ---------
    let hijack_pkg = PackageId::from_bytes([0xB3; 16]);
    let hijack_sub = build_signed_store_submission(
        hijack_pkg,
        package_version,
        layout.oci_manifest_digest,
        vec![hijacked_app],
        slots,
        fixture.malicious_publisher,
        &fixture.malicious_sk,
        t0 + 2_600,
    );
    let hijack_err = registry
        .submit_release(&cas, trust, &hijack_sub)
        .expect_err("cross-publisher ApplicationId squatting must fail closed");
    match hijack_err {
        StoreRegistryError::PublisherTrust(inner) => {
            assert_eq!(
                inner,
                PublisherTrustCheckError::ApplicationIdHijackAttempt {
                    application_id: hijacked_app,
                    registered_owner: fixture.legit_publisher,
                    attempted_publisher: fixture.malicious_publisher,
                }
            );
        }
        other => panic!("expected PublisherTrust(ApplicationIdHijackAttempt), got {other:?}"),
    }
    assert_eq!(
        registry
            .inspect_release(hijack_pkg, package_version)
            .expect("inspect rejected hijack package"),
        None,
        "hijacking submission must commit zero catalog rows"
    );
    assert_eq!(
        registry
            .inspect_release(legit_pkg, package_version)
            .expect("inspect legitimate release")
            .expect("legitimate release exists")
            .channel,
        StoreChannel::Candidate,
        "legitimate release must remain untouched by the hijack attempt"
    );

    // --- 4c. StoreCertificationReceipt issuance anti-hijack -----------------
    let loaded_v1 =
        LoadedCertifyPackage::load(starter_assistant_v1_dir()).expect("load starter v1");
    trust
        .verify_or_bind_application_owner(
            fixture.legit_publisher,
            PublisherTrustTier::CertifiedPublisher,
            loaded_v1.application_id,
            t0 + 2_700,
        )
        .expect("legit publisher binds starter app id first");
    let malicious_proof = PublisherSubmissionProof::for_package(
        fixture.malicious_publisher,
        &fixture.malicious_sk,
        &loaded_v1,
    );
    let refused = issue_store_certification_receipt(
        trust,
        &fixture.certifier_signer(),
        IssueStoreCertificationRequest {
            submission_proof: malicious_proof,
            application_id: loaded_v1.application_id,
            package_id: loaded_v1.package_id,
            package_version: loaded_v1.base_manifest.version_raw.clone(),
            manifest_digest: loaded_v1.manifest_digest,
            surface_profile: SurfaceProfile::ApplicationPlatform,
            target_sabi_epoch: 3,
            gate_summaries: four_pass_gate_summaries(),
            issued_at_ms: t0 + 2_800,
            expires_at_ms: t0 + 86_400_000,
        },
    );
    assert!(matches!(
        refused,
        Err(CertifyError::PublisherTrust(
            PublisherTrustCheckError::ApplicationIdHijackAttempt {
                application_id,
                registered_owner,
                attempted_publisher,
            }
        )) if application_id == loaded_v1.application_id
            && registered_owner == fixture.legit_publisher
            && attempted_publisher == fixture.malicious_publisher
    ));
}

// ===========================================================================
// Attack class 5: malicious upgrade crashing mid-flight — Gate 3 injects a
// failing health probe (HealthCheckFailed) and a half-written MidStepFault
// crash against the REAL starter package v1.0.0 -> v1.1.0 and requires the
// UserDataContainer SHA-256 tree digest to be restored BITWISE to baseline;
// a faulty rollback handler leaving drift must make the gate fail closed.
// ===========================================================================
#[test]
fn h7_attack5_malicious_upgrade_health_fail_and_mid_crash_restore_bitwise_baseline() {
    let loaded_v1 =
        LoadedCertifyPackage::load(starter_assistant_v1_dir()).expect("load starter v1.0.0");
    let loaded_v1_1 =
        LoadedCertifyPackage::load(starter_assistant_v1_1_dir()).expect("load starter v1.1.0");

    // --- 5a. Both injected faults restore the container bitwise -------------
    let outcome = run_gate3_update_rollback_drill_with_options(
        &loaded_v1,
        Some(&loaded_v1_1),
        Gate3DrillOptions::default(),
    )
    .expect("run Gate 3 default fault-injected drills");
    assert_eq!(
        outcome.summary.status,
        GateStatus::Pass,
        "default Gate 3 drill must pass with zero blockers: {}",
        outcome.summary.summary_line
    );
    assert_eq!(
        outcome.findings.len(),
        0,
        "default Gate 3 drill must emit zero findings"
    );
    assert_eq!(outcome.healthy_activated_generation, Some(2));
    let baseline = outcome
        .baseline_container_tree_digest
        .expect("baseline tree digest recorded");
    assert_eq!(
        outcome.health_fail_restored_tree_digest,
        Some(baseline),
        "HealthCheckFailed rollback must restore the UserDataContainer SHA-256 tree digest bitwise"
    );
    assert_eq!(
        outcome.mid_step_fault_restored_tree_digest,
        Some(baseline),
        "MidStepFault crash rollback must restore the UserDataContainer SHA-256 tree digest bitwise"
    );

    // --- 5b. Malicious/faulty rollback handler leaving drift fails closed ---
    let drifted = run_gate3_update_rollback_drill_with_options(
        &loaded_v1,
        Some(&loaded_v1_1),
        Gate3DrillOptions {
            simulate_unclean_rollback_drift: true,
        },
    )
    .expect("run Gate 3 with simulated unclean rollback drift");
    assert_eq!(
        drifted.summary.status,
        GateStatus::Fail,
        "unclean rollback drift must fail Gate 3 closed"
    );
    let drift_rules = blocker_rule_ids(&drifted.findings);
    assert_eq!(
        drift_rules,
        vec!["PKG-UPDATE-001".to_owned(), "PKG-UPDATE-001".to_owned()],
        "both the health-check-failed and mid-step-fault sub-drills must catch container drift"
    );
    assert_ne!(
        drifted.health_fail_restored_tree_digest,
        drifted.baseline_container_tree_digest
    );
    assert_ne!(
        drifted.mid_step_fault_restored_tree_digest,
        drifted.baseline_container_tree_digest
    );
}

// ===========================================================================
// Attack class 6: post-uninstall residue — Gate 4 drives the REAL starter
// package through install/grant/background-task/cache/uninstall and requires
// zero capability, background-task, and private-cache residue; malicious
// uninstall behavior (skipped revocation, left-behind cache) must make the
// gate fail closed.
// ===========================================================================
#[test]
fn h7_attack6_uninstall_zero_capability_background_and_cache_residue() {
    let loaded_v1 =
        LoadedCertifyPackage::load(starter_assistant_v1_dir()).expect("load starter v1.0.0");

    // --- 6a. Clean uninstall leaves zero residue ----------------------------
    let clean = run_gate4_uninstall_audit_with_options(&loaded_v1, Gate4DrillOptions::default())
        .expect("run Gate 4 default uninstall audit");
    assert_eq!(
        clean.summary.status,
        GateStatus::Pass,
        "default Gate 4 audit must pass with zero blockers"
    );
    assert_eq!(
        clean.findings.len(),
        0,
        "default Gate 4 audit must emit zero findings"
    );
    assert!(
        clean.active_task_guard_verified,
        "uninstall while background tasks are Active must be refused with zero dirty state"
    );
    assert_eq!(clean.revoked_capabilities_verified, 2);
    assert_eq!(
        clean.residual_private_cache_files, 0,
        "post-uninstall private cache must be 100% empty"
    );
    assert!(clean.shared_artifact_preserved);
    assert!(clean.user_data_preserved_by_default_and_z4_gated);

    // --- 6b. Malicious uninstaller leaving private-cache residue is caught --
    let residue = run_gate4_uninstall_audit_with_options(
        &loaded_v1,
        Gate4DrillOptions {
            simulate_residual_private_cache: true,
            ..Gate4DrillOptions::default()
        },
    )
    .expect("run Gate 4 with simulated residual private cache");
    assert_eq!(residue.summary.status, GateStatus::Fail);
    assert!(residue.residual_private_cache_files > 0);
    assert!(
        residue
            .findings
            .iter()
            .any(|finding| finding.severity == CertifySeverity::Blocker
                && finding.rule_id == "PKG-UNINSTALL-001"
                && finding.subject == "gate4/private-cache"),
        "left-over private cache must raise a PKG-UNINSTALL-001 BLOCKER"
    );

    // --- 6c. Malicious uninstaller keeping capabilities alive is caught -----
    let unrevoked = run_gate4_uninstall_audit_with_options(
        &loaded_v1,
        Gate4DrillOptions {
            simulate_unrevoked_capability: true,
            ..Gate4DrillOptions::default()
        },
    )
    .expect("run Gate 4 with simulated unrevoked capabilities");
    assert_eq!(unrevoked.summary.status, GateStatus::Fail);
    assert_eq!(unrevoked.revoked_capabilities_verified, 0);
    assert_eq!(
        blocker_rule_ids(&unrevoked.findings),
        vec!["CAP-REVOKE-001".to_owned(), "CAP-REVOKE-001".to_owned()],
        "both surviving capability handles must raise CAP-REVOKE-001 BLOCKERs"
    );
}
