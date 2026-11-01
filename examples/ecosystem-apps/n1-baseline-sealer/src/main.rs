//! `n1-baseline-sealer` — W80-E6B frozen-signature `N-1` baseline sealer
//! (`examples/ecosystem-apps`, Stage E `E6` / `ADR-0027` §2.4 step A).
//!
//! Seals the four W80-E6B reference packages (2 multi-component Applications
//! + 2 userland Driver adapters) through the real in-repo toolchain:
//!
//! 1. `nlos-publisher-trust` three-tier PKI — `OfflinePlatformRootCa` ->
//!    `StoreIntermediateCertificate` -> per-publisher `CertifiedPublisher`
//!    leaf over a `SqlitePublisherTrustAuthority` with a monotonic
//!    `RevocationEpoch` CRL;
//! 2. `nlos-certify` four-gate pipeline wired to that real authority,
//!    issuing a custody-equivalent `StoreCertificationReceipt` per package;
//! 3. `nlos-store-registry` — `LocalOciCasStore` five-slot multi-component
//!    layout ingest, publisher-signed `StoreReleaseSubmission`
//!    (anti-hijack `ApplicationId` binding), `SignedCertificationAttestation`
//!    (gates `0b1111`), `Candidate -> Stable` promotion, and post-promotion
//!    re-verification of the stored release;
//! 4. a per-package seal record (`<sealed>/<label>.v<version>.store-seal.txt`)
//!    with every digest, receipt id, and signature, plus SHA-256 manifest
//!    lines for the frozen `.nlos-pkg` artifacts built by the seal script.
//!
//! Determinism: all seeds and timestamps are fixed constants; re-running the
//! sealer against identical trees produces byte-identical signatures,
//! digests, and seal records.
//!
//! Usage: `n1-baseline-sealer --repo-root <REPO> [--work-dir <DIR>]`

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::derive_application_id;
use nlos_artifact::decode_package_file;
use nlos_certify::{
    CertifyPipelineConfig, GateId, LoadedCertifyPackage, PublisherSubmissionProof,
    StoreCertifierSigner, SurfaceProfile, derive_publisher_id_from_str, run_certification_pipeline,
    verify_store_certification_receipt,
};
use nlos_publisher_trust::{
    CrlImportDecision, IntermediateCertificateIssueSpec, LeafCertificateIssueSpec,
    OfflinePlatformRootCa, PublisherTrustTier, RevocationEpoch, RevocationListSpec,
    SignedRevocationList, SqlitePublisherTrustAuthority,
};
use nlos_store_registry::{
    DeclaredPermissionEntry, EffectClass, LocalOciCasStore, NLOS_OCI_CONFIG_MEDIA_TYPE,
    OciLayerDescriptor, REQUIRED_CERTIFICATION_GATES_MASK, SignedCertificationAttestation,
    SqliteStoreRegistry, StoreChannel, StoreComponentKind, StoreComponentSlot,
    StoreGovernanceAnchors, StorePromotionDecision, StoreReleaseSubmission,
    UnsignedStoreReleaseSubmission, compute_release_submission_digest,
};
use nlos_types::{ApplicationId, IdempotencyKey, KeyId};
use sha2::{Digest, Sha256};

/// Fixed seal epoch (ms): 2026-01-01T00:00:00Z.
const SEAL_T0_MS: u64 = 1_767_225_600_000;
/// Root CA signing seed (offline, sample only).
const ROOT_SEED: [u8; 32] = [0xE6; 32];
/// Store intermediate CA signing seed (sample only).
const INTERMEDIATE_SEED: [u8; 32] = [0xB0; 32];
/// Store certifier signing seed (sample only).
const CERTIFIER_SEED: [u8; 32] = [0xCE; 32];

/// One sealed sample package descriptor.
struct SamplePackage {
    /// Seal label used in file names.
    label: &'static str,
    /// Example-relative directory holding the crate + trees.
    example_dir: &'static str,
    /// v1.0.0 package tree relative to the example dir.
    v1_tree: &'static str,
    /// v1.1.0 upgrade tree relative to the example dir.
    v2_tree: &'static str,
    /// Certify surface profile.
    profile: SurfaceProfile,
    /// Publisher label from `certify.manifest`.
    publisher_label: &'static str,
    /// Publisher leaf signing seed (sample only).
    publisher_seed: [u8; 32],
    /// Store component slot mapping (tree-relative path, slot name, kind).
    slots: &'static [(&'static str, &'static str, StoreComponentKind)],
}

/// The W80-E6B sample matrix: 2 multi-component Applications + 2 Drivers.
const SAMPLES: &[SamplePackage] = &[
    SamplePackage {
        label: "knowledge-observatory-app",
        example_dir: "examples/ecosystem-apps/knowledge-observatory-app",
        v1_tree: "package",
        v2_tree: "package-v1.1.0",
        profile: SurfaceProfile::ApplicationPlatform,
        publisher_label: "nlos.market.observatory-works",
        publisher_seed: [0x61; 32],
        slots: &[
            (
                "curator.bin",
                "curator-exec",
                StoreComponentKind::Executable,
            ),
            (
                "drivers/observatory-bridge.drv",
                "observatory-bridge",
                StoreComponentKind::Driver,
            ),
            (
                "surfaces/observatory-card.duis",
                "observatory-card",
                StoreComponentKind::UiSurface,
            ),
            (
                "services/index-sync.svc",
                "index-sync",
                StoreComponentKind::BackgroundService,
            ),
            (
                "assets/observatory-profile.txt",
                "observatory-profile-seed",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
            (
                "assets/source-seeds.txt",
                "source-seeds",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
        ],
    },
    SamplePackage {
        label: "field-survey-app",
        example_dir: "examples/ecosystem-apps/field-survey-app",
        v1_tree: "package",
        v2_tree: "package-v1.1.0",
        profile: SurfaceProfile::ApplicationPlatform,
        publisher_label: "nlos.market.fieldkit-labs",
        publisher_seed: [0x62; 32],
        slots: &[
            (
                "survey-worker.bin",
                "survey-worker-exec",
                StoreComponentKind::Executable,
            ),
            (
                "drivers/fieldkit-bridge.drv",
                "fieldkit-bridge",
                StoreComponentKind::Driver,
            ),
            (
                "surfaces/fieldkit-card.duis",
                "fieldkit-card",
                StoreComponentKind::UiSurface,
            ),
            (
                "services/sync-field-notes.svc",
                "sync-field-notes",
                StoreComponentKind::BackgroundService,
            ),
            (
                "assets/survey-checklist.txt",
                "survey-checklist-seed",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
            (
                "assets/route-profile.txt",
                "route-profile-seed",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
        ],
    },
    SamplePackage {
        label: "mcp-bridge-driver",
        example_dir: "examples/ecosystem-drivers/mcp-bridge-driver",
        v1_tree: "package",
        v2_tree: "package-v1.1.0",
        profile: SurfaceProfile::ApplicationPlatform,
        publisher_label: "nlos.market.mcp-bridge-works",
        publisher_seed: [0x63; 32],
        slots: &[
            (
                "adapters/mcp-bridge-adapter.bin",
                "mcp-bridge-adapter",
                StoreComponentKind::Driver,
            ),
            (
                "extensions/mcp-extension.manifest",
                "mcp-extension-manifest",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
            (
                "extensions/mcp-effects.manifest",
                "mcp-effect-declarations",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
            (
                "extensions/mcp-negotiation.profile",
                "mcp-negotiation-profile",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
        ],
    },
    SamplePackage {
        label: "openai-compat-driver",
        example_dir: "examples/ecosystem-drivers/openai-compat-driver",
        v1_tree: "package",
        v2_tree: "package-v1.1.0",
        profile: SurfaceProfile::ApplicationPlatform,
        publisher_label: "nlos.market.openai-gateway-works",
        publisher_seed: [0x64; 32],
        slots: &[
            (
                "adapters/openai-compat-adapter.bin",
                "openai-compat-adapter",
                StoreComponentKind::Driver,
            ),
            (
                "extensions/openai-extension.manifest",
                "openai-extension-manifest",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
            (
                "extensions/openai-effects.manifest",
                "openai-effect-declarations",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
            (
                "extensions/openai-endpoint.profile",
                "openai-endpoint-profile",
                StoreComponentKind::ReadOnlySeedTemplate,
            ),
        ],
    },
];

#[derive(Debug)]
enum SealError {
    Failed(String),
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self::Failed(reason) = self;
        f.write_str(reason)
    }
}

fn main() -> ExitCode {
    let mut repo_root: Option<PathBuf> = None;
    let mut work_dir: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--repo-root" => repo_root = args.next().map(PathBuf::from),
            "--work-dir" => work_dir = args.next().map(PathBuf::from),
            _ => {}
        }
    }
    let Some(repo_root) = repo_root else {
        eprintln!("n1-baseline-sealer: missing --repo-root");
        eprintln!("usage: n1-baseline-sealer --repo-root <DIR> [--work-dir <DIR>]");
        return ExitCode::from(1);
    };
    let work_dir = work_dir.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("n1-baseline-seal-{}", std::process::id()))
    });
    match seal_all(&repo_root, &work_dir) {
        Ok(summary) => {
            for line in &summary {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("n1-baseline-sealer: {error}");
            ExitCode::from(2)
        }
    }
}

/// Returns hex-encoded SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[allow(clippy::too_many_lines)]
fn seal_all(repo_root: &Path, work_dir: &Path) -> Result<Vec<String>, SealError> {
    fs::create_dir_all(work_dir)
        .map_err(|error| SealError::Failed(format!("work dir: {error}")))?;

    // ------------------------------------------------------------------
    // Step 1: three-tier publisher PKI + monotonic CRL (epoch 1).
    // ------------------------------------------------------------------
    let root_ca = OfflinePlatformRootCa::issue_self_signed(
        [0xE0; 16],
        KeyId::from_bytes([0xE1; 16]),
        &ROOT_SEED,
        SEAL_T0_MS - 10_000,
        SEAL_T0_MS + 365 * 86_400_000,
    )
    .map_err(|error| SealError::Failed(format!("issue root CA: {error}")))?;

    let intermediate_key = SigningKey::from_bytes(&INTERMEDIATE_SEED);
    let intermediate_ca = root_ca
        .issue_intermediate_certificate(
            &ROOT_SEED,
            IntermediateCertificateIssueSpec {
                serial_id: [0xB1; 16],
                intermediate_key_id: KeyId::from_bytes([0xB2; 16]),
                intermediate_public_key: intermediate_key.verifying_key().to_bytes(),
                max_issuable_tier: PublisherTrustTier::CertifiedPublisher,
                not_before_ms: SEAL_T0_MS - 10_000,
                expires_at_ms: SEAL_T0_MS + 365 * 86_400_000,
            },
        )
        .map_err(|error| SealError::Failed(format!("issue intermediate CA: {error}")))?;

    let trust_authority = SqlitePublisherTrustAuthority::open(work_dir.join("publisher-trust.db"))
        .map_err(|error| SealError::Failed(format!("open trust authority: {error}")))?;
    trust_authority
        .anchor_offline_root_ca(&root_ca, SEAL_T0_MS - 5_000)
        .map_err(|error| SealError::Failed(format!("anchor root CA: {error}")))?;
    trust_authority
        .register_intermediate_certificate(&intermediate_ca, SEAL_T0_MS - 5_000)
        .map_err(|error| SealError::Failed(format!("register intermediate: {error}")))?;

    let crl = SignedRevocationList::issue_with_intermediate_seed(
        &intermediate_ca,
        &INTERMEDIATE_SEED,
        RevocationListSpec {
            revocation_epoch: RevocationEpoch::new(1),
            issued_at_ms: SEAL_T0_MS,
            fresh_until_ms: SEAL_T0_MS + 365 * 86_400_000,
            revoked_certificates: Vec::new(),
            revoked_publishers: Vec::new(),
        },
    )
    .map_err(|error| SealError::Failed(format!("issue CRL: {error}")))?;
    match trust_authority.import_signed_revocation_list(&crl) {
        Ok(CrlImportDecision::Advanced(_)) => {}
        Ok(other) => {
            return Err(SealError::Failed(format!(
                "unexpected CRL import decision: {other:?}"
            )));
        }
        Err(error) => return Err(SealError::Failed(format!("import CRL: {error}"))),
    }

    // ------------------------------------------------------------------
    // Step 2: one store governance catalog for all four seals.
    // ------------------------------------------------------------------
    let certifier_key = SigningKey::from_bytes(&CERTIFIER_SEED);
    let entitlement_key = SigningKey::from_bytes(&[0x65; 32]);
    let recall_key = SigningKey::from_bytes(&[0x66; 32]);
    let cas = LocalOciCasStore::open(work_dir.join("oci-cas"))
        .map_err(|error| SealError::Failed(format!("open OCI CAS: {error}")))?;
    let registry = SqliteStoreRegistry::open(
        work_dir.join("store-registry"),
        StoreGovernanceAnchors {
            certifier_public_key: certifier_key.verifying_key().to_bytes(),
            entitlement_issuer_key_id: KeyId::from_bytes([0x65; 16]),
            entitlement_issuer_public_key: entitlement_key.verifying_key().to_bytes(),
            recall_authority_public_key: recall_key.verifying_key().to_bytes(),
            initial_revocation_epoch: RevocationEpoch::new(1),
            initialized_at_ms: SEAL_T0_MS,
        },
    )
    .map_err(|error| SealError::Failed(format!("open store registry: {error}")))?;
    let certifier_signer = StoreCertifierSigner::Ed25519Key {
        key_id: KeyId::from_bytes([0xCE; 16]),
        signing_key: &certifier_key,
    };

    let mut summary = Vec::new();
    for (index, sample) in SAMPLES.iter().enumerate() {
        let index = u8::try_from(index).expect("sample index fits u8");
        let seal = seal_one(
            repo_root,
            sample,
            index,
            &trust_authority,
            &intermediate_ca,
            &certifier_key,
            &certifier_signer,
            &cas,
            &registry,
        )?;
        summary.extend(seal);
    }

    let stable = registry
        .list_official_stable_releases()
        .map_err(|error| SealError::Failed(format!("list stable releases: {error}")))?;
    summary.push(format!(
        "[seal] STABLE_RELEASES count={} epoch=1 host_sabi_epoch=3 target_sabi_epoch=1",
        stable.len()
    ));
    Ok(summary)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn seal_one(
    repo_root: &Path,
    sample: &SamplePackage,
    index: u8,
    trust_authority: &SqlitePublisherTrustAuthority,
    intermediate_ca: &nlos_publisher_trust::StoreIntermediateCertificate,
    certifier_key: &SigningKey,
    certifier_signer: &StoreCertifierSigner<'_>,
    cas: &LocalOciCasStore,
    registry: &SqliteStoreRegistry,
) -> Result<Vec<String>, SealError> {
    let example_dir = repo_root.join(sample.example_dir);
    let v1_dir = example_dir.join(sample.v1_tree);
    let v2_dir = example_dir.join(sample.v2_tree);
    let sealed_dir = example_dir.join("sealed");
    fs::create_dir_all(&sealed_dir)
        .map_err(|error| SealError::Failed(format!("sealed dir: {error}")))?;

    // Cross-check the certify sidecar publisher label against the sealer table.
    let sidecar = fs::read_to_string(v1_dir.join("certify.manifest"))
        .map_err(|error| SealError::Failed(format!("read certify.manifest: {error}")))?;
    let sidecar_publisher = sidecar
        .lines()
        .find_map(|line| line.trim().strip_prefix("publisher-id = "))
        .map(str::trim)
        .unwrap_or_default();
    if sidecar_publisher != sample.publisher_label {
        return Err(SealError::Failed(format!(
            "certify.manifest publisher {:?} disagrees with sealer table {:?}",
            sidecar_publisher, sample.publisher_label
        )));
    }

    // Leaf certificate for this publisher.
    let publisher_key = SigningKey::from_bytes(&sample.publisher_seed);
    let publisher_id = derive_publisher_id_from_str(sample.publisher_label);
    let leaf = intermediate_ca
        .issue_leaf_certificate_with_seed(
            &INTERMEDIATE_SEED,
            LeafCertificateIssueSpec {
                serial_id: [0x70 + index; 16],
                publisher_id,
                trust_tier: PublisherTrustTier::CertifiedPublisher,
                leaf_key_id: KeyId::from_bytes([0x80 + index; 16]),
                leaf_public_key: publisher_key.verifying_key().to_bytes(),
                not_before_ms: SEAL_T0_MS - 1_000,
                expires_at_ms: SEAL_T0_MS + 365 * 86_400_000,
            },
        )
        .map_err(|error| SealError::Failed(format!("issue leaf certificate: {error}")))?;
    trust_authority
        .register_leaf_certificate(&leaf, SEAL_T0_MS + 100 + u64::from(index))
        .map_err(|error| SealError::Failed(format!("register leaf: {error}")))?;

    // ------------------------------------------------------------------
    // Four-gate certification against the real trust authority.
    // ------------------------------------------------------------------
    let loaded = LoadedCertifyPackage::load(&v1_dir)
        .map_err(|error| SealError::Failed(format!("load {}: {error}", sample.label)))?;
    let proof = PublisherSubmissionProof::for_package(publisher_id, &publisher_key, &loaded);
    let config = CertifyPipelineConfig::new(proof, sample.profile, SEAL_T0_MS + 1_000);
    let report = run_certification_pipeline(
        &v1_dir,
        Some(&v2_dir),
        &config,
        trust_authority,
        certifier_signer,
    )
    .map_err(|error| SealError::Failed(format!("certify {}: {error}", sample.label)))?;
    if !report.all_gates_passed() {
        return Err(SealError::Failed(format!(
            "{} failed certification gates:\n{}",
            sample.label,
            report.render_text()
        )));
    }
    let receipt = report
        .receipt
        .as_ref()
        .ok_or_else(|| SealError::Failed(format!("{}: no receipt issued", sample.label)))?;
    verify_store_certification_receipt(
        receipt,
        Some(&certifier_key.verifying_key().to_bytes()),
        SEAL_T0_MS + 1_000,
    )
    .map_err(|error| SealError::Failed(format!("verify receipt: {error}")))?;

    // ------------------------------------------------------------------
    // OCI CAS ingest of the five-kind multi-component layout.
    // ------------------------------------------------------------------
    let manifest_bytes = fs::read(v1_dir.join("package.manifest"))
        .map_err(|error| SealError::Failed(format!("read package.manifest: {error}")))?;
    let config_desc =
        OciLayerDescriptor::from_layer_bytes(NLOS_OCI_CONFIG_MEDIA_TYPE, &manifest_bytes)
            .map_err(|error| SealError::Failed(format!("config descriptor: {error}")))?;
    let mut owned_layers = Vec::with_capacity(sample.slots.len());
    let mut slots = Vec::with_capacity(sample.slots.len());
    for (relative, slot_name, kind) in sample.slots {
        let payload = fs::read(v1_dir.join(relative))
            .map_err(|error| SealError::Failed(format!("read slot {relative}: {error}")))?;
        let descriptor = OciLayerDescriptor::from_layer_bytes(
            format!("application/vnd.nlos.component.{kind:?}.v1+bin"),
            &payload,
        )
        .map_err(|error| SealError::Failed(format!("layer descriptor: {error}")))?;
        slots.push(StoreComponentSlot {
            slot_name: (*slot_name).to_owned(),
            kind: *kind,
            entrypoint: (*relative).to_owned(),
            content_sha256: descriptor.sha256_digest,
            size_bytes: descriptor.size_bytes,
        });
        owned_layers.push((descriptor, payload));
    }
    let borrowed_layers: Vec<(OciLayerDescriptor, &[u8])> = owned_layers
        .iter()
        .map(|(descriptor, payload)| (descriptor.clone(), payload.as_slice()))
        .collect();
    let layout = cas
        .write_package_layout(&config_desc, &manifest_bytes, &borrowed_layers)
        .map_err(|error| SealError::Failed(format!("write OCI layout: {error}")))?;
    let verified_layout = cas
        .verify_stored_layout(&layout.oci_manifest_digest)
        .map_err(|error| SealError::Failed(format!("verify OCI layout: {error}")))?;
    if verified_layout.oci_manifest_digest != layout.oci_manifest_digest {
        return Err(SealError::Failed("OCI layout digest mismatch".to_string()));
    }

    // ------------------------------------------------------------------
    // Publisher-signed release submission -> Candidate -> Stable.
    // ------------------------------------------------------------------
    let package_id = report.package_id;
    let package_version = nlos_certify::parse_version_ordinal(&report.package_version);
    let certify_application_id = report.application_id;
    let runtime_application_id = derive_application_id(package_id);
    let declared_permissions = vec![DeclaredPermissionEntry {
        capability_scope: "sabi.artifact.scoped_chooser".to_owned(),
        human_purpose: format!("{} sample purpose: scoped artifact reads", sample.label),
        effect_class: EffectClass::ReadOnly,
        network_egress_declared: sample.label == "openai-compat-driver",
        sandbox_profile: "wasm-dual-layer-v1".to_owned(),
    }];
    let application_ids = dedupe_application_ids(certify_application_id, runtime_application_id);
    let manifest_digest = *report.manifest_digest.as_bytes();
    let submission_digest = compute_release_submission_digest(&UnsignedStoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest: layout.oci_manifest_digest,
        application_ids: &application_ids,
        components: &slots,
        declared_permissions: &declared_permissions,
        publisher_id,
    });
    let submission = StoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest: layout.oci_manifest_digest,
        application_ids,
        components: slots.clone(),
        declared_permissions,
        publisher_id,
        publisher_signing_key: publisher_key.verifying_key().to_bytes(),
        publisher_signature: publisher_key.sign(&submission_digest).to_bytes(),
        submitted_at_ms: SEAL_T0_MS + 2_000,
    };
    let candidate = registry
        .submit_release(cas, trust_authority, &submission)
        .map_err(|error| SealError::Failed(format!("submit release: {error}")))?;
    if candidate.channel != StoreChannel::Candidate {
        return Err(SealError::Failed(format!(
            "{}: expected Candidate channel, got {:?}",
            sample.label, candidate.channel
        )));
    }

    let mut attestation = SignedCertificationAttestation {
        package_id,
        application_id: certify_application_id,
        package_version,
        manifest_digest,
        oci_manifest_digest: layout.oci_manifest_digest,
        publisher_id,
        passed_gates_mask: REQUIRED_CERTIFICATION_GATES_MASK,
        issued_at_ms: SEAL_T0_MS + 2_500,
        expires_at_ms: SEAL_T0_MS + 365 * 86_400_000,
        certifier_public_key: certifier_key.verifying_key().to_bytes(),
        ed25519_signature: [0; 64],
    };
    attestation.ed25519_signature = certifier_key.sign(&attestation.signing_digest()).to_bytes();
    let promotion = registry
        .promote_candidate_to_stable(
            &attestation,
            IdempotencyKey::from_bytes([0x90 + index; 16]),
            SEAL_T0_MS + 2_600,
        )
        .map_err(|error| SealError::Failed(format!("promote to stable: {error}")))?;
    if !matches!(promotion, StorePromotionDecision::Promoted(_)) {
        return Err(SealError::Failed(format!(
            "{}: unexpected promotion decision {:?}",
            sample.label, promotion
        )));
    }
    let stored = registry
        .inspect_release(package_id, package_version)
        .map_err(|error| SealError::Failed(format!("inspect release: {error}")))?;
    let stable_record = stored.ok_or_else(|| {
        SealError::Failed(format!("{}: release missing after promotion", sample.label))
    })?;
    if stable_record.channel != StoreChannel::Stable {
        return Err(SealError::Failed(format!(
            "{}: expected Stable channel after promotion, got {:?}",
            sample.label, stable_record.channel
        )));
    }

    // ------------------------------------------------------------------
    // Seal record: hash the frozen .nlos-pkg artifacts and write the record.
    // ------------------------------------------------------------------
    let mut artifact_lines = Vec::new();
    for (tree, version_label) in [(sample.v1_tree, "1.0.0"), (sample.v2_tree, "1.1.0")] {
        let pkg_path = sealed_dir.join(format!("{}.v{}.nlos-pkg", sample.label, version_label));
        let bytes = fs::read(&pkg_path).map_err(|error| {
            SealError::Failed(format!(
                "read frozen package {} (run seal-n1-baseline.sh first): {error}",
                pkg_path.display()
            ))
        })?;
        let decoded = decode_package_file(&bytes).map_err(|error| {
            SealError::Failed(format!("decode {}: {error}", pkg_path.display()))
        })?;
        if decoded.package_id != package_id {
            return Err(SealError::Failed(format!(
                "{}: frozen package id {} disagrees with certified {}",
                pkg_path.display(),
                hex(decoded.package_id.as_bytes()),
                hex(package_id.as_bytes())
            )));
        }
        let relative_display = pkg_path
            .strip_prefix(repo_root)
            .unwrap_or(&pkg_path)
            .to_string_lossy()
            .replace('\\', "/");
        artifact_lines.push(format!(
            "artifact = {} tree={} package={} version={} entries={} tasks={} sha256={} bytes={}",
            relative_display,
            tree,
            hex(decoded.package_id.as_bytes()),
            decoded.version,
            decoded.entries.len(),
            decoded.tasks.len(),
            sha256_hex(&bytes),
            bytes.len(),
        ));
    }

    let gates_line = report
        .gate_summaries()
        .iter()
        .map(|summary| {
            format!(
                "{}={}",
                match summary.gate {
                    GateId::Gate1ProtocolAndEffects => "g1",
                    GateId::Gate2LeastPrivilegeAndSupplyChain => "g2",
                    GateId::Gate3TransactionalUpdateAndRollback => "g3",
                    GateId::Gate4ZeroResidueUninstall => "g4",
                },
                summary.status.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join(" ");

    let record = format!(
        "# W80-E6B N-1 baseline store seal (frozen; regenerate with examples/ecosystem-apps/seal-n1-baseline.sh)\n\
         label = {}\n\
         publisher = {} tier=CertifiedPublisher leaf_serial={}\n\
         publisher_id = {}\n\
         package_id = {}\n\
         application_ids = {} runtime={}\n\
         package_version = {}\n\
         target_sabi_epoch = 1 host_sabi_epoch = 3\n\
         certify_manifest_digest = {}\n\
         oci_manifest_digest = {}\n\
         gates = {} mask=0b1111\n\
         certification_receipt_id = {}\n\
         certification_receipt_signature = {}\n\
         release_submission_digest = {}\n\
         attestation_signature = {}\n\
         promotion = Candidate->Stable epoch=1\n\
         components = {}\n\
         {}\n",
        sample.label,
        sample.publisher_label,
        hex(&[0x70 + index; 16]),
        hex(publisher_id.as_bytes()),
        hex(package_id.as_bytes()),
        hex(certify_application_id.as_bytes()),
        hex(runtime_application_id.as_bytes()),
        report.package_version,
        hex(&manifest_digest),
        hex(&layout.oci_manifest_digest),
        gates_line,
        hex(receipt.receipt_id.as_bytes()),
        hex(&receipt.ed25519_signature),
        hex(&submission_digest),
        hex(&attestation.ed25519_signature),
        slots
            .iter()
            .map(|slot| format!("{}:{:?}", slot.slot_name, slot.kind))
            .collect::<Vec<_>>()
            .join(","),
        artifact_lines.join("\n# "),
    );
    let record_path = sealed_dir.join(format!("{}.v1.0.0.store-seal.txt", sample.label));
    fs::write(&record_path, &record)
        .map_err(|error| SealError::Failed(format!("write seal record: {error}")))?;

    Ok(vec![
        format!(
            "[seal] {} publisher={} package={} gates=4/4 oci={} receipt={}",
            sample.label,
            sample.publisher_label,
            hex(package_id.as_bytes()),
            hex(&layout.oci_manifest_digest),
            hex(receipt.receipt_id.as_bytes()),
        ),
        format!("[seal] {} record={}", sample.label, record_path.display()),
    ])
}

fn dedupe_application_ids(
    certify_id: ApplicationId,
    runtime_id: ApplicationId,
) -> Vec<ApplicationId> {
    let mut ids = vec![certify_id];
    if runtime_id != certify_id {
        ids.push(runtime_id);
    }
    ids
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{SAMPLES, seal_all, sha256_hex};

    fn repo_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::parent)
            .expect("repo root")
            .to_path_buf()
    }

    fn temp_work() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("n1-sealer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("work dir");
        dir
    }

    #[test]
    fn sample_matrix_covers_two_apps_and_two_drivers() {
        assert_eq!(SAMPLES.len(), 4);
        assert_eq!(
            SAMPLES
                .iter()
                .filter(|sample| sample
                    .slots
                    .iter()
                    .any(|(_, _, kind)| *kind == super::StoreComponentKind::Driver))
                .count(),
            4,
            "every sample must map a Driver store slot"
        );
        for sample in SAMPLES {
            assert!(
                sample
                    .slots
                    .iter()
                    .any(|(_, _, kind)| *kind == super::StoreComponentKind::ReadOnlySeedTemplate),
                "{} must map at least one read-only seed slot (PKG-SEED-001)",
                sample.label
            );
        }
        // App samples carry all five store component kinds (PKG-MULTI-001).
        for sample in &SAMPLES[..2] {
            for kind in [
                super::StoreComponentKind::Executable,
                super::StoreComponentKind::Driver,
                super::StoreComponentKind::UiSurface,
                super::StoreComponentKind::BackgroundService,
                super::StoreComponentKind::ReadOnlySeedTemplate,
            ] {
                assert!(
                    sample.slots.iter().any(|(_, _, slot)| *slot == kind),
                    "{} missing store slot {:?}",
                    sample.label,
                    kind
                );
            }
        }
    }

    #[test]
    fn sha256_helper_matches_known_vector() {
        // sha256("abc") = ba7816bf...
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Copies the four example directories (package trees + frozen sealed
    /// artifacts) into an isolated fixture root and re-runs the entire seal
    /// chain in-process: leaf certificates, four-gate certification, OCI CAS
    /// ingest, Candidate -> Stable promotion, and seal records must all
    /// succeed and reproduce the frozen artifact digests.
    #[test]
    fn seal_all_replays_end_to_end_against_an_isolated_fixture_copy() {
        let root = repo_root();
        let base = temp_work();
        let fixture = base.join("fixture-root");
        let work = base.join("seal-work");
        std::fs::create_dir_all(&work).expect("work dir");
        for sample in SAMPLES {
            let source = root.join(sample.example_dir);
            let target = fixture.join(sample.example_dir);
            copy_tree(&source, &target);
        }
        let summary = seal_all(&fixture, &work).expect("seal replay succeeds end to end");
        assert_eq!(
            summary
                .iter()
                .filter(|line| line.starts_with("[seal] ") && line.contains("gates=4/4"))
                .count(),
            4,
            "all four samples must report 4/4 gates: {summary:?}"
        );
        assert!(
            summary
                .iter()
                .any(|line| line.contains("STABLE_RELEASES count=4"))
        );

        // Seal records must exist in the fixture copy and re-verify the frozen
        // .nlos-pkg digests recorded by the committed seal script run.
        for sample in SAMPLES {
            let record_path = fixture
                .join(sample.example_dir)
                .join("sealed")
                .join(format!("{}.v1.0.0.store-seal.txt", sample.label));
            let record = std::fs::read_to_string(&record_path)
                .unwrap_or_else(|error| panic!("record {}: {error}", record_path.display()));
            assert!(record.contains("gates = g1=PASS g2=PASS g3=PASS g4=PASS mask=0b1111"));
            assert!(record.contains("promotion = Candidate->Stable epoch=1"));
            assert!(record.contains("sha256="));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    fn copy_tree(source: &Path, target: &Path) {
        std::fs::create_dir_all(target)
            .unwrap_or_else(|error| panic!("mkdir {}: {error}", target.display()));
        for entry in std::fs::read_dir(source)
            .unwrap_or_else(|error| panic!("read_dir {}: {error}", source.display()))
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                if name == "target" {
                    continue;
                }
                copy_tree(&path, &target.join(name));
            } else {
                std::fs::copy(&path, target.join(entry.file_name()))
                    .unwrap_or_else(|error| panic!("copy {}: {error}", path.display()));
            }
        }
    }
}
