//! Step A (`ADR-0027` section 2.4 Step A) — Frozen `N-1` Baseline Verification.
//!
//! Mechanically verifies all 16 committed `N-1` sealed baseline files (`8`
//! `.nlos-pkg` packages, `4` `.v1.0.0.certify.json` reports, and `4`
//! `.v1.0.0.store-seal.txt` governance records) across the 4 reference
//! third-party samples (`knowledge-observatory-app`, `field-survey-app`,
//! `mcp-bridge-driver`, `openai-compat-driver`), replaying:
//! 1. SHA-256 and byte-length equality against both [`FROZEN_N1_SAMPLES`] and
//!    each `.store-seal.txt` (`artifact = ...` and `# artifact = ...`);
//! 2. `nlos_artifact::check_package_file` (`0` conformance findings) and
//!    `nlos_artifact::decode_package_file` on every frozen `v1.0.0` and `v1.1.0`
//!    `.nlos-pkg` binary;
//! 3. `.v1.0.0.certify.json` `CERTIFIED` verdict, 4/4 `PASS` gate summaries,
//!    and `nlos_certify::verify_store_certification_receipt` over the embedded
//!    signed receipt;
//! 4. 3-tier PKI publisher trust (`OfflinePlatformRootCa` ->
//!    `StoreIntermediateCertificate` -> `CertifiedPublisher` leaf + monotonic
//!    `RevocationEpoch(1)` CRL), custody-equivalent `StoreCertifierSigner`
//!    receipt replay matching `certification_receipt_id` &
//!    `certification_receipt_signature` in `.store-seal.txt`, and
//!    `LocalOciCasStore` + `SqliteStoreRegistry` `Candidate -> Stable`
//!    (`0b1111`) promotion matching `oci_manifest_digest`,
//!    `release_submission_digest`, and `attestation_signature`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::derive_application_id;
use nlos_artifact::{PackageFile, check_package_file, decode_package_file};
use nlos_certify::json::{JsonValue, parse_json};
use nlos_certify::{
    IssueStoreCertificationRequest, LoadedCertifyPackage, PublisherSubmissionProof,
    StoreCertificationReceipt, StoreCertifierSigner, SurfaceProfile, derive_publisher_id_from_str,
    issue_store_certification_receipt, verify_store_certification_receipt,
};
use nlos_publisher_trust::{
    CrlImportDecision, IntermediateCertificateIssueSpec, LeafCertificateIssueSpec,
    OfflinePlatformRootCa, PublisherId, PublisherTrustTier, RevocationEpoch, RevocationListSpec,
    SignedRevocationList, SqlitePublisherTrustAuthority, StoreIntermediateCertificate,
};
use nlos_store_registry::{
    DeclaredPermissionEntry, EffectClass, LocalOciCasStore, NLOS_OCI_CONFIG_MEDIA_TYPE,
    OciLayerDescriptor, REQUIRED_CERTIFICATION_GATES_MASK, SignedCertificationAttestation,
    SqliteStoreRegistry, StoreChannel, StoreComponentKind, StoreComponentSlot,
    StoreGovernanceAnchors, StorePromotionDecision, StoreReleaseSubmission,
    UnsignedStoreReleaseSubmission, compute_release_submission_digest,
};
use nlos_types::{ApplicationId, IdempotencyKey, KeyId, PackageId};

use crate::{
    FROZEN_N1_SAMPLES, FrozenSampleSpec, SEAL_CERTIFIER_SEED, SEAL_ENTITLEMENT_SEED,
    SEAL_INTERMEDIATE_CA_SEED, SEAL_RECALL_SEED, SEAL_ROOT_CA_SEED, SEAL_T0_MS,
    SealedBaselineSnapshot, TempHostWorkspace, hex_decode_exact, hex_encode,
};

/// One `artifact = ...` (or `# artifact = ...`) entry parsed from `.store-seal.txt`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSealArtifactEntry {
    /// Workspace-relative `.nlos-pkg` path.
    pub rel_path: String,
    /// Source package tree (`package` or `package-v1.1.0`).
    pub tree: String,
    /// 32-hex `PackageId`.
    pub package_id_hex: String,
    /// Packed `u64` version ordinal.
    pub packed_version: u64,
    /// Declared entry count.
    pub entry_count: usize,
    /// Declared task template count.
    pub task_count: usize,
    /// 64-hex SHA-256 digest of the frozen `.nlos-pkg`.
    pub sha256_hex: String,
    /// Exact byte length of the frozen `.nlos-pkg`.
    pub size_bytes: usize,
}

/// Complete structured representation of one `.v1.0.0.store-seal.txt` record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedStoreSealRecord {
    /// Sample label (`knowledge-observatory-app`, etc.).
    pub label: String,
    /// Symbolic publisher handle.
    pub publisher_label: String,
    /// 32-hex `PublisherId`.
    pub publisher_id_hex: String,
    /// 32-hex `PackageId`.
    pub package_id_hex: String,
    /// 32-hex certify `ApplicationId`.
    pub certify_application_id_hex: String,
    /// 32-hex runtime `ApplicationId`.
    pub runtime_application_id_hex: String,
    /// 64-hex `certify_manifest_digest`.
    pub certify_manifest_digest_hex: String,
    /// 64-hex `oci_manifest_digest`.
    pub oci_manifest_digest_hex: String,
    /// 32-hex `certification_receipt_id` (3-tier PKI certifier).
    pub certification_receipt_id_hex: String,
    /// 128-hex `certification_receipt_signature`.
    pub certification_receipt_signature_hex: String,
    /// 64-hex `release_submission_digest`.
    pub release_submission_digest_hex: String,
    /// 128-hex `attestation_signature`.
    pub attestation_signature_hex: String,
    /// Both frozen `.nlos-pkg` artifact entries (`v1.0.0` and `v1.1.0`).
    pub artifacts: Vec<ParsedSealArtifactEntry>,
}

/// Verified per-sample frozen `N-1` package pair and governance metadata produced by Step A.
#[derive(Clone, Debug)]
pub struct VerifiedFrozenSample {
    /// Static sample specification.
    pub spec: FrozenSampleSpec,
    /// Decoded `v1.0.0` `PackageFile` (loaded directly from frozen bytes).
    pub pkg_v1_0_0: PackageFile,
    /// Raw frozen bytes of `v1.0.0.nlos-pkg`.
    pub raw_v1_0_0_bytes: Vec<u8>,
    /// Decoded `v1.1.0` `PackageFile` (loaded directly from frozen bytes).
    pub pkg_v1_1_0: PackageFile,
    /// Raw frozen bytes of `v1.1.0.nlos-pkg`.
    pub raw_v1_1_0_bytes: Vec<u8>,
    /// Derived 128-bit `PublisherId`.
    pub publisher_id: PublisherId,
    /// Certify-derived `ApplicationId`.
    pub certify_application_id: ApplicationId,
    /// Runtime-derived `ApplicationId` (`derive_application_id(package_id)`).
    pub runtime_application_id: ApplicationId,
    /// Canonical `oci_manifest_digest` verified in `LocalOciCasStore`.
    pub oci_manifest_digest: [u8; 32],
    /// Parsed `.store-seal.txt` record.
    pub seal_record: ParsedStoreSealRecord,
}

/// Receipt proving Step A (`ADR-0027` section 2.4 Step A) passed across all 16
/// frozen `N-1` baseline files.
#[derive(Clone, Debug)]
pub struct StepAFrozenSealReceipt {
    /// Verified 4-sample frozen baseline entries (`knowledge-observatory-app`,
    /// `field-survey-app`, `mcp-bridge-driver`, `openai-compat-driver`).
    pub samples: Vec<VerifiedFrozenSample>,
    /// Count of verified frozen `.nlos-pkg` files (`8`).
    pub verified_nlos_pkg_count: usize,
    /// Count of verified `.certify.json` reports (`4`).
    pub verified_certify_report_count: usize,
    /// Count of verified `.store-seal.txt` records (`4`).
    pub verified_store_seal_count: usize,
    /// Count of `Stable` official releases verified in `SqliteStoreRegistry` (`4`).
    pub promoted_stable_release_count: usize,
}

/// Parses a `.v1.0.0.store-seal.txt` file, extracting both `artifact = ...` and
/// `# artifact = ...` lines so all frozen `.nlos-pkg` digests are audited.
///
/// # Errors
///
/// Fails closed if any mandatory key or `artifact` token is missing or malformed.
pub fn parse_store_seal_record(raw: &str) -> Result<ParsedStoreSealRecord, String> {
    let mut kv: BTreeMap<String, String> = BTreeMap::new();
    let mut artifacts = Vec::new();

    for raw_line in raw.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# artifact = ") {
            artifacts.push(parse_artifact_tokens(rest)?);
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("artifact = ") {
            artifacts.push(parse_artifact_tokens(rest)?);
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(format!("malformed line in .store-seal.txt: {trimmed:?}"));
        };
        kv.insert(key.trim().to_owned(), value.trim().to_owned());
    }

    let req = |key: &str| -> Result<String, String> {
        kv.get(key)
            .cloned()
            .ok_or_else(|| format!("missing key {key:?} in .store-seal.txt"))
    };

    let publisher_line = req("publisher")?;
    let publisher_tokens: Vec<&str> = publisher_line.split_whitespace().collect();
    if publisher_tokens.is_empty() || !publisher_line.contains("tier=CertifiedPublisher") {
        return Err(format!("unexpected publisher line: {publisher_line:?}"));
    }

    let app_ids_line = req("application_ids")?;
    let app_tokens: Vec<&str> = app_ids_line.split_whitespace().collect();
    if app_tokens.len() != 2 {
        return Err(format!("unexpected application_ids line: {app_ids_line:?}"));
    }
    let runtime_app_hex = app_tokens[1]
        .strip_prefix("runtime=")
        .ok_or_else(|| format!("missing runtime= in application_ids: {app_ids_line:?}"))?;

    let gates_line = req("gates")?;
    if gates_line != "g1=PASS g2=PASS g3=PASS g4=PASS mask=0b1111" {
        return Err(format!(
            "expected 4/4 PASS 0b1111 gates, got {gates_line:?}"
        ));
    }
    let promotion_line = req("promotion")?;
    if promotion_line != "Candidate->Stable epoch=1" {
        return Err(format!("unexpected promotion line: {promotion_line:?}"));
    }
    if artifacts.len() != 2 {
        return Err(format!(
            "expected exactly 2 frozen .nlos-pkg artifact entries in .store-seal.txt, got {}",
            artifacts.len()
        ));
    }

    Ok(ParsedStoreSealRecord {
        label: req("label")?,
        publisher_label: publisher_tokens[0].to_owned(),
        publisher_id_hex: req("publisher_id")?,
        package_id_hex: req("package_id")?,
        certify_application_id_hex: app_tokens[0].to_owned(),
        runtime_application_id_hex: runtime_app_hex.to_owned(),
        certify_manifest_digest_hex: req("certify_manifest_digest")?,
        oci_manifest_digest_hex: req("oci_manifest_digest")?,
        certification_receipt_id_hex: req("certification_receipt_id")?,
        certification_receipt_signature_hex: req("certification_receipt_signature")?,
        release_submission_digest_hex: req("release_submission_digest")?,
        attestation_signature_hex: req("attestation_signature")?,
        artifacts,
    })
}

fn parse_artifact_tokens(rest: &str) -> Result<ParsedSealArtifactEntry, String> {
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    if tokens.len() != 8 {
        return Err(format!(
            "malformed artifact entry in .store-seal.txt: {rest:?}"
        ));
    }
    let rel_path = tokens[0].to_owned();
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for tok in &tokens[1..] {
        let Some((k, v)) = tok.split_once('=') else {
            return Err(format!("malformed artifact token {tok:?} in {rest:?}"));
        };
        fields.insert(k, v);
    }
    let get = |k: &str| -> Result<&str, String> {
        fields
            .get(k)
            .copied()
            .ok_or_else(|| format!("missing artifact token {k:?} in {rest:?}"))
    };
    Ok(ParsedSealArtifactEntry {
        rel_path,
        tree: get("tree")?.to_owned(),
        package_id_hex: get("package")?.to_owned(),
        packed_version: get("version")?
            .parse::<u64>()
            .map_err(|err| format!("invalid packed version: {err}"))?,
        entry_count: get("entries")?
            .parse::<usize>()
            .map_err(|err| format!("invalid entries count: {err}"))?,
        task_count: get("tasks")?
            .parse::<usize>()
            .map_err(|err| format!("invalid tasks count: {err}"))?,
        sha256_hex: get("sha256")?.to_owned(),
        size_bytes: get("bytes")?
            .parse::<usize>()
            .map_err(|err| format!("invalid bytes count: {err}"))?,
    })
}

#[allow(clippy::too_many_lines)]
fn slot_specs_for_sample(
    label: &str,
) -> &'static [(&'static str, &'static str, StoreComponentKind)] {
    match label {
        "knowledge-observatory-app" => &[
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
        "field-survey-app" => &[
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
        "mcp-bridge-driver" => &[
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
        "openai-compat-driver" => &[
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
        _ => &[],
    }
}

/// Copies a package source tree into `dst_dir` while normalizing UTF-8 text
/// files from Windows `core.autocrlf` `\r\n` back to canonical Git LF (`\n`).
///
/// This guarantees cross-platform (`Windows` / `Linux` / `macOS`) bit-exact
/// reproduction of LF-sealed `certify_manifest_digest` and `oci_manifest_digest`
/// values without modifying a single byte in `repo_root`.
fn stage_lf_normalized_package_tree(src_dir: &Path, dst_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dst_dir)
        .map_err(|err| format!("create LF staging dir {}: {err}", dst_dir.display()))?;
    let entries =
        fs::read_dir(src_dir).map_err(|err| format!("read_dir {}: {err}", src_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("dir entry in {}: {err}", src_dir.display()))?;
        let src_path = entry.path();
        let dst_path = dst_dir.join(entry.file_name());
        if src_path.is_dir() {
            stage_lf_normalized_package_tree(&src_path, &dst_path)?;
        } else {
            let raw =
                fs::read(&src_path).map_err(|err| format!("read {}: {err}", src_path.display()))?;
            let normalized = if let Ok(text) = std::str::from_utf8(&raw) {
                text.replace("\r\n", "\n").into_bytes()
            } else {
                raw
            };
            fs::write(&dst_path, normalized)
                .map_err(|err| format!("write {}: {err}", dst_path.display()))?;
        }
    }
    Ok(())
}

/// Provisions the 3-tier PKI `SqlitePublisherTrustAuthority`, `LocalOciCasStore`,
/// and `SqliteStoreRegistry` under `governance_root`, registering all 4
/// `CertifiedPublisher` leaf certificates and onboarding + promoting all 4
/// `v1.0.0` releases from `Candidate` to `Stable` (`0b1111`).
///
/// # Errors
///
/// Fails closed if any PKI certificate, CRL import, OCI CAS verification,
/// release submission, or `Candidate -> Stable` promotion fails.
#[allow(clippy::too_many_lines)]
pub fn provision_store_governance_catalog(
    repo_root: &Path,
    governance_root: &Path,
    expected_seals: Option<&[ParsedStoreSealRecord]>,
) -> Result<
    (
        SqlitePublisherTrustAuthority,
        LocalOciCasStore,
        SqliteStoreRegistry,
        StoreIntermediateCertificate,
    ),
    String,
> {
    fs::create_dir_all(governance_root).map_err(|err| {
        format!(
            "create governance_root {}: {err}",
            governance_root.display()
        )
    })?;

    let root_ca = OfflinePlatformRootCa::issue_self_signed(
        [0xE0; 16],
        KeyId::from_bytes([0xE1; 16]),
        &SEAL_ROOT_CA_SEED,
        SEAL_T0_MS - 10_000,
        SEAL_T0_MS + 365 * 86_400_000,
    )
    .map_err(|err| format!("issue OfflinePlatformRootCa: {err}"))?;

    let intermediate_key = SigningKey::from_bytes(&SEAL_INTERMEDIATE_CA_SEED);
    let intermediate_ca = root_ca
        .issue_intermediate_certificate(
            &SEAL_ROOT_CA_SEED,
            IntermediateCertificateIssueSpec {
                serial_id: [0xB1; 16],
                intermediate_key_id: KeyId::from_bytes([0xB2; 16]),
                intermediate_public_key: intermediate_key.verifying_key().to_bytes(),
                max_issuable_tier: PublisherTrustTier::CertifiedPublisher,
                not_before_ms: SEAL_T0_MS - 10_000,
                expires_at_ms: SEAL_T0_MS + 365 * 86_400_000,
            },
        )
        .map_err(|err| format!("issue StoreIntermediateCertificate: {err}"))?;

    let trust_authority =
        SqlitePublisherTrustAuthority::open(governance_root.join("publisher-trust.db"))
            .map_err(|err| format!("open SqlitePublisherTrustAuthority: {err}"))?;
    trust_authority
        .anchor_offline_root_ca(&root_ca, SEAL_T0_MS - 5_000)
        .map_err(|err| format!("anchor_offline_root_ca: {err}"))?;
    trust_authority
        .register_intermediate_certificate(&intermediate_ca, SEAL_T0_MS - 5_000)
        .map_err(|err| format!("register_intermediate_certificate: {err}"))?;

    let crl = SignedRevocationList::issue_with_intermediate_seed(
        &intermediate_ca,
        &SEAL_INTERMEDIATE_CA_SEED,
        RevocationListSpec {
            revocation_epoch: RevocationEpoch::new(1),
            issued_at_ms: SEAL_T0_MS,
            fresh_until_ms: SEAL_T0_MS + 365 * 86_400_000,
            revoked_certificates: Vec::new(),
            revoked_publishers: Vec::new(),
        },
    )
    .map_err(|err| format!("issue SignedRevocationList: {err}"))?;
    match trust_authority
        .import_signed_revocation_list(&crl)
        .map_err(|err| format!("import_signed_revocation_list: {err}"))?
    {
        CrlImportDecision::Advanced(_) | CrlImportDecision::Replayed(_) => {}
    }

    let certifier_key = SigningKey::from_bytes(&SEAL_CERTIFIER_SEED);
    let entitlement_key = SigningKey::from_bytes(&SEAL_ENTITLEMENT_SEED);
    let recall_key = SigningKey::from_bytes(&SEAL_RECALL_SEED);
    let cas = LocalOciCasStore::open(governance_root.join("oci-cas"))
        .map_err(|err| format!("open LocalOciCasStore: {err}"))?;
    let registry = SqliteStoreRegistry::open(
        governance_root.join("store-registry"),
        StoreGovernanceAnchors {
            certifier_public_key: certifier_key.verifying_key().to_bytes(),
            entitlement_issuer_key_id: KeyId::from_bytes([0x65; 16]),
            entitlement_issuer_public_key: entitlement_key.verifying_key().to_bytes(),
            recall_authority_public_key: recall_key.verifying_key().to_bytes(),
            initial_revocation_epoch: RevocationEpoch::new(1),
            initialized_at_ms: SEAL_T0_MS,
        },
    )
    .map_err(|err| format!("open SqliteStoreRegistry: {err}"))?;

    for (idx, spec) in FROZEN_N1_SAMPLES.iter().enumerate() {
        let publisher_key = SigningKey::from_bytes(&[spec.publisher_seed_byte; 32]);
        let publisher_id = derive_publisher_id_from_str(spec.publisher_label);
        let leaf = intermediate_ca
            .issue_leaf_certificate_with_seed(
                &SEAL_INTERMEDIATE_CA_SEED,
                LeafCertificateIssueSpec {
                    serial_id: [0x70 + spec.index; 16],
                    publisher_id,
                    trust_tier: PublisherTrustTier::CertifiedPublisher,
                    leaf_key_id: KeyId::from_bytes([0x80 + spec.index; 16]),
                    leaf_public_key: publisher_key.verifying_key().to_bytes(),
                    not_before_ms: SEAL_T0_MS - 1_000,
                    expires_at_ms: SEAL_T0_MS + 365 * 86_400_000,
                },
            )
            .map_err(|err| format!("issue leaf cert for {}: {err}", spec.label))?;
        trust_authority
            .register_leaf_certificate(&leaf, SEAL_T0_MS + 100 + u64::from(spec.index))
            .map_err(|err| format!("register leaf cert for {}: {err}", spec.label))?;

        let src_v1_dir = repo_root.join(spec.sample_rel_dir).join("package");
        let v1_dir = governance_root.join("lf-staged-packages").join(spec.label);
        stage_lf_normalized_package_tree(&src_v1_dir, &v1_dir)?;
        let loaded = LoadedCertifyPackage::load(&v1_dir)
            .map_err(|err| format!("load certify package {}: {err}", spec.label))?;

        let manifest_bytes = fs::read(v1_dir.join("package.manifest"))
            .map_err(|err| format!("read package.manifest for {}: {err}", spec.label))?;
        let config_desc =
            OciLayerDescriptor::from_layer_bytes(NLOS_OCI_CONFIG_MEDIA_TYPE, &manifest_bytes)
                .map_err(|err| format!("OciLayerDescriptor config {}: {err}", spec.label))?;

        let slot_defs = slot_specs_for_sample(spec.label);
        let mut owned_layers = Vec::with_capacity(slot_defs.len());
        let mut slots = Vec::with_capacity(slot_defs.len());
        for (relative, slot_name, kind) in slot_defs {
            let payload = fs::read(v1_dir.join(relative))
                .map_err(|err| format!("read OCI slot {relative} for {}: {err}", spec.label))?;
            let descriptor = OciLayerDescriptor::from_layer_bytes(
                format!("application/vnd.nlos.component.{kind:?}.v1+bin"),
                &payload,
            )
            .map_err(|err| format!("OciLayerDescriptor {relative}: {err}"))?;
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
            .map(|(desc, bytes)| (desc.clone(), bytes.as_slice()))
            .collect();
        let layout = cas
            .write_package_layout(&config_desc, &manifest_bytes, &borrowed_layers)
            .map_err(|err| format!("write_package_layout {}: {err}", spec.label))?;
        let verified_layout = cas
            .verify_stored_layout(&layout.oci_manifest_digest)
            .map_err(|err| format!("verify_stored_layout {}: {err}", spec.label))?;
        if verified_layout.oci_manifest_digest != layout.oci_manifest_digest {
            return Err(format!("OCI manifest digest mismatch for {}", spec.label));
        }

        let package_id = loaded.package_id;
        let package_version = loaded.version_ordinal;
        let certify_app_id = loaded.application_id;
        let runtime_app_id = derive_application_id(package_id);
        let mut application_ids = vec![certify_app_id];
        if runtime_app_id != certify_app_id {
            application_ids.push(runtime_app_id);
        }
        let declared_permissions = vec![DeclaredPermissionEntry {
            capability_scope: "sabi.artifact.scoped_chooser".to_owned(),
            human_purpose: format!("{} sample purpose: scoped artifact reads", spec.label),
            effect_class: EffectClass::ReadOnly,
            network_egress_declared: spec.label == "openai-compat-driver",
            sandbox_profile: "wasm-dual-layer-v1".to_owned(),
        }];
        let manifest_digest = *loaded.manifest_digest.as_bytes();
        let submission_digest =
            compute_release_submission_digest(&UnsignedStoreReleaseSubmission {
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
            components: slots,
            declared_permissions,
            publisher_id,
            publisher_signing_key: publisher_key.verifying_key().to_bytes(),
            publisher_signature: publisher_key.sign(&submission_digest).to_bytes(),
            submitted_at_ms: SEAL_T0_MS + 2_000,
        };
        let candidate = registry
            .submit_release(&cas, &trust_authority, &submission)
            .map_err(|err| format!("submit_release {}: {err}", spec.label))?;
        if candidate.channel != StoreChannel::Candidate {
            return Err(format!(
                "expected Candidate channel for {}, got {:?}",
                spec.label, candidate.channel
            ));
        }

        let mut attestation = SignedCertificationAttestation {
            package_id,
            application_id: certify_app_id,
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
        attestation.ed25519_signature =
            certifier_key.sign(&attestation.signing_digest()).to_bytes();

        let promo = registry
            .promote_candidate_to_stable(
                &attestation,
                IdempotencyKey::from_bytes([0x90 + spec.index; 16]),
                SEAL_T0_MS + 2_600,
            )
            .map_err(|err| format!("promote_candidate_to_stable {}: {err}", spec.label))?;
        if !matches!(
            promo,
            StorePromotionDecision::Promoted(_) | StorePromotionDecision::Replayed(_)
        ) {
            return Err(format!(
                "unexpected promotion decision for {}: {promo:?}",
                spec.label
            ));
        }

        if let Some(seals) = expected_seals {
            let seal = &seals[idx];
            if hex_encode(&layout.oci_manifest_digest) != seal.oci_manifest_digest_hex {
                return Err(format!(
                    "{}: OCI manifest digest {} != sealed {}",
                    spec.label,
                    hex_encode(&layout.oci_manifest_digest),
                    seal.oci_manifest_digest_hex
                ));
            }
            if hex_encode(&submission_digest) != seal.release_submission_digest_hex {
                return Err(format!(
                    "{}: release_submission_digest {} != sealed {}",
                    spec.label,
                    hex_encode(&submission_digest),
                    seal.release_submission_digest_hex
                ));
            }
            if hex_encode(&attestation.ed25519_signature) != seal.attestation_signature_hex {
                return Err(format!(
                    "{}: attestation_signature {} != sealed {}",
                    spec.label,
                    hex_encode(&attestation.ed25519_signature),
                    seal.attestation_signature_hex
                ));
            }
        }
    }

    Ok((trust_authority, cas, registry, intermediate_ca))
}

/// Executes Step A (`ADR-0027` section 2.4 Step A): verifies all 16 frozen `N-1`
/// baseline files, decodes and conformance-checks all 8 `.nlos-pkg` binaries,
/// verifies all 4 `.certify.json` signed receipts, and replays the 3-tier PKI +
/// OCI CAS `Candidate -> Stable` promotion against all 4 `.store-seal.txt` files.
///
/// # Errors
///
/// Fails closed if any digest, signature, conformance rule, certification gate,
/// or store seal field mismatches.
#[allow(clippy::too_many_lines)]
pub fn run_step_a_verify_frozen_n1_baseline(
    repo_root: &Path,
    pre_snapshot: &SealedBaselineSnapshot,
) -> Result<StepAFrozenSealReceipt, String> {
    if pre_snapshot.files.len() != 16 {
        return Err(format!(
            "Step A expected 16 sealed N-1 baseline files, got {}",
            pre_snapshot.files.len()
        ));
    }

    let mut parsed_seals = Vec::with_capacity(FROZEN_N1_SAMPLES.len());
    let mut verified_samples = Vec::with_capacity(FROZEN_N1_SAMPLES.len());

    // First pass: parse `.store-seal.txt`, `.certify.json`, and frozen `.nlos-pkg` files.
    let mut raw_certify_receipts: Vec<StoreCertificationReceipt> =
        Vec::with_capacity(FROZEN_N1_SAMPLES.len());

    for spec in &FROZEN_N1_SAMPLES {
        let seal_raw = fs::read_to_string(repo_root.join(spec.store_seal_rel_path))
            .map_err(|err| format!("read {}: {err}", spec.store_seal_rel_path))?;
        let seal = parse_store_seal_record(&seal_raw)?;
        if seal.label != spec.label {
            return Err(format!(
                "seal label mismatch: expected {}, got {}",
                spec.label, seal.label
            ));
        }
        if seal.package_id_hex != spec.expected_package_id_hex {
            return Err(format!(
                "seal package_id mismatch for {}: expected {}, got {}",
                spec.label, spec.expected_package_id_hex, seal.package_id_hex
            ));
        }

        // Cross-check both frozen `.nlos-pkg` digests in `.store-seal.txt` against `pre_snapshot` & `FROZEN_N1_SAMPLES`.
        let art_v1 = &seal.artifacts[0];
        let art_v2 = &seal.artifacts[1];
        if art_v1.rel_path != spec.pkg_v1_0_0_rel_path
            || art_v1.sha256_hex != spec.expected_v1_0_0_sha256
            || art_v1.size_bytes != spec.expected_v1_0_0_bytes
        {
            return Err(format!(
                "v1.0.0 artifact seal mismatch for {}: {:?}",
                spec.label, art_v1
            ));
        }
        if art_v2.rel_path != spec.pkg_v1_1_0_rel_path
            || art_v2.sha256_hex != spec.expected_v1_1_0_sha256
            || art_v2.size_bytes != spec.expected_v1_1_0_bytes
        {
            return Err(format!(
                "v1.1.0 artifact seal mismatch for {}: {:?}",
                spec.label, art_v2
            ));
        }
        let Some((live_v1_sha, live_v1_len)) = pre_snapshot.files.get(spec.pkg_v1_0_0_rel_path)
        else {
            return Err(format!(
                "missing {} in pre_snapshot",
                spec.pkg_v1_0_0_rel_path
            ));
        };
        if live_v1_sha != &art_v1.sha256_hex || *live_v1_len != art_v1.size_bytes {
            return Err(format!(
                "live v1.0.0.nlos-pkg SHA-256 drift for {}: live=({live_v1_sha}, {live_v1_len}) != sealed=({}, {})",
                spec.label, art_v1.sha256_hex, art_v1.size_bytes
            ));
        }
        let Some((live_v2_sha, live_v2_len)) = pre_snapshot.files.get(spec.pkg_v1_1_0_rel_path)
        else {
            return Err(format!(
                "missing {} in pre_snapshot",
                spec.pkg_v1_1_0_rel_path
            ));
        };
        if live_v2_sha != &art_v2.sha256_hex || *live_v2_len != art_v2.size_bytes {
            return Err(format!(
                "live v1.1.0.nlos-pkg SHA-256 drift for {}: live=({live_v2_sha}, {live_v2_len}) != sealed=({}, {})",
                spec.label, art_v2.sha256_hex, art_v2.size_bytes
            ));
        }

        // Conformance-check and decode both frozen `.nlos-pkg` binaries.
        let raw_v1 = fs::read(repo_root.join(spec.pkg_v1_0_0_rel_path))
            .map_err(|err| format!("read {}: {err}", spec.pkg_v1_0_0_rel_path))?;
        let conf_v1 = check_package_file(&raw_v1);
        if !conf_v1.is_conformant() {
            return Err(format!(
                "{} failed check_package_file with {} findings",
                spec.pkg_v1_0_0_rel_path,
                conf_v1.findings.len()
            ));
        }
        let pkg_v1 = decode_package_file(&raw_v1)
            .map_err(|err| format!("decode {}: {err}", spec.pkg_v1_0_0_rel_path))?;

        let raw_v2 = fs::read(repo_root.join(spec.pkg_v1_1_0_rel_path))
            .map_err(|err| format!("read {}: {err}", spec.pkg_v1_1_0_rel_path))?;
        let conf_v2 = check_package_file(&raw_v2);
        if !conf_v2.is_conformant() {
            return Err(format!(
                "{} failed check_package_file with {} findings",
                spec.pkg_v1_1_0_rel_path,
                conf_v2.findings.len()
            ));
        }
        let pkg_v2 = decode_package_file(&raw_v2)
            .map_err(|err| format!("decode {}: {err}", spec.pkg_v1_1_0_rel_path))?;

        let expected_pkg_id = PackageId::from_bytes(hex_decode_exact::<16>(
            spec.expected_package_id_hex,
            "package_id",
        )?);
        if pkg_v1.package_id != expected_pkg_id || pkg_v2.package_id != expected_pkg_id {
            return Err(format!("decoded PackageId mismatch for {}", spec.label));
        }
        if pkg_v1.entries.len() != art_v1.entry_count || pkg_v1.tasks.len() != art_v1.task_count {
            return Err(format!("decoded v1.0.0 shape mismatch for {}", spec.label));
        }
        if pkg_v2.entries.len() != art_v2.entry_count || pkg_v2.tasks.len() != art_v2.task_count {
            return Err(format!("decoded v1.1.0 shape mismatch for {}", spec.label));
        }

        // Parse & verify `.v1.0.0.certify.json` report and its embedded `StoreCertificationReceipt`.
        let certify_raw = fs::read_to_string(repo_root.join(spec.certify_json_rel_path))
            .map_err(|err| format!("read {}: {err}", spec.certify_json_rel_path))?;
        let certify_val = parse_json(&certify_raw)
            .map_err(|err| format!("parse_json {}: {err}", spec.certify_json_rel_path))?;
        let certify_obj = certify_val
            .as_object()
            .ok_or_else(|| format!("{} root must be JSON object", spec.certify_json_rel_path))?;
        let verdict = certify_obj
            .get("verdict")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        if verdict != "CERTIFIED" {
            return Err(format!(
                "{} expected verdict=CERTIFIED, got {verdict:?}",
                spec.certify_json_rel_path
            ));
        }
        let receipt_val = certify_obj
            .get("receipt")
            .ok_or_else(|| format!("{} missing receipt object", spec.certify_json_rel_path))?;
        let cli_receipt = StoreCertificationReceipt::from_json_value(receipt_val)
            .map_err(|err| format!("decode receipt in {}: {err}", spec.certify_json_rel_path))?;
        verify_store_certification_receipt(
            &cli_receipt,
            Some(&cli_receipt.certifier_public_key),
            cli_receipt.issued_at_ms,
        )
        .map_err(|err| {
            format!(
                "verify_store_certification_receipt failed for {}: {err}",
                spec.certify_json_rel_path
            )
        })?;
        if hex_encode(cli_receipt.manifest_digest.as_bytes()) != seal.certify_manifest_digest_hex {
            return Err(format!(
                "certify.json manifest_digest disagrees with .store-seal.txt for {}",
                spec.label
            ));
        }
        raw_certify_receipts.push(cli_receipt);

        let publisher_id = derive_publisher_id_from_str(spec.publisher_label);
        if hex_encode(publisher_id.as_bytes()) != seal.publisher_id_hex {
            return Err(format!(
                "PublisherId derivation mismatch for {}",
                spec.label
            ));
        }
        let certify_app_id = ApplicationId::from_bytes(hex_decode_exact::<16>(
            &seal.certify_application_id_hex,
            "certify_application_id",
        )?);
        let runtime_app_id = derive_application_id(expected_pkg_id);
        if hex_encode(runtime_app_id.as_bytes()) != seal.runtime_application_id_hex {
            return Err(format!("runtime ApplicationId mismatch for {}", spec.label));
        }
        let oci_manifest_digest =
            hex_decode_exact::<32>(&seal.oci_manifest_digest_hex, "oci_manifest_digest")?;

        parsed_seals.push(seal.clone());
        verified_samples.push(VerifiedFrozenSample {
            spec: *spec,
            pkg_v1_0_0: pkg_v1,
            raw_v1_0_0_bytes: raw_v1,
            pkg_v1_1_0: pkg_v2,
            raw_v1_1_0_bytes: raw_v2,
            publisher_id,
            certify_application_id: certify_app_id,
            runtime_application_id: runtime_app_id,
            oci_manifest_digest,
            seal_record: seal,
        });
    }

    // Second pass: replay 3-tier PKI + OCI CAS + StoreRegistry promotion in an isolated audit dir
    // AND verify the 3-tier PKI `StoreCertificationReceipt` matches `.store-seal.txt`.
    let step_a_ws = TempHostWorkspace::new("step-a-seal-audit");
    let (trust_authority, _cas, registry, _intermediate_ca) =
        provision_store_governance_catalog(repo_root, step_a_ws.path(), Some(&parsed_seals))?;

    let certifier_key = SigningKey::from_bytes(&SEAL_CERTIFIER_SEED);
    let certifier_signer = StoreCertifierSigner::Ed25519Key {
        key_id: KeyId::from_bytes([0xCE; 16]),
        signing_key: &certifier_key,
    };

    for (idx, sample) in verified_samples.iter().enumerate() {
        let cli_receipt = &raw_certify_receipts[idx];
        let v1_dir = step_a_ws
            .path()
            .join("lf-staged-packages")
            .join(sample.spec.label);
        let loaded = LoadedCertifyPackage::load(&v1_dir)
            .map_err(|err| format!("load certify package {}: {err}", sample.spec.label))?;
        let publisher_key = SigningKey::from_bytes(&[sample.spec.publisher_seed_byte; 32]);
        let submission_proof =
            PublisherSubmissionProof::for_package(sample.publisher_id, &publisher_key, &loaded);
        let gate_summaries = cli_receipt.gate_summaries.clone();

        let pki_receipt = issue_store_certification_receipt(
            &trust_authority,
            &certifier_signer,
            IssueStoreCertificationRequest {
                submission_proof,
                application_id: loaded.application_id,
                package_id: loaded.package_id,
                package_version: cli_receipt.package_version.clone(),
                manifest_digest: loaded.manifest_digest,
                surface_profile: SurfaceProfile::ApplicationPlatform,
                target_sabi_epoch: 1,
                gate_summaries,
                issued_at_ms: SEAL_T0_MS + 1_000,
                expires_at_ms: SEAL_T0_MS + 1_000 + nlos_certify::DEFAULT_CERT_RECEIPT_TTL_MS,
            },
        )
        .map_err(|err| {
            format!(
                "issue_store_certification_receipt under 3-tier PKI failed for {}: {err}",
                sample.spec.label
            )
        })?;
        verify_store_certification_receipt(
            &pki_receipt,
            Some(&certifier_key.verifying_key().to_bytes()),
            SEAL_T0_MS + 1_000,
        )
        .map_err(|err| format!("verify 3-tier PKI receipt for {}: {err}", sample.spec.label))?;

        if hex_encode(pki_receipt.receipt_id.as_bytes())
            != sample.seal_record.certification_receipt_id_hex
        {
            return Err(format!(
                "3-tier PKI certification_receipt_id mismatch for {}: computed {} != sealed {}",
                sample.spec.label,
                hex_encode(pki_receipt.receipt_id.as_bytes()),
                sample.seal_record.certification_receipt_id_hex
            ));
        }
        if hex_encode(&pki_receipt.ed25519_signature)
            != sample.seal_record.certification_receipt_signature_hex
        {
            return Err(format!(
                "3-tier PKI certification_receipt_signature mismatch for {}",
                sample.spec.label
            ));
        }
    }

    let stable_releases = registry
        .list_official_stable_releases()
        .map_err(|err| format!("list_official_stable_releases: {err}"))?;
    if stable_releases.len() != 4 {
        return Err(format!(
            "expected 4 Stable releases in SqliteStoreRegistry, got {}",
            stable_releases.len()
        ));
    }

    Ok(StepAFrozenSealReceipt {
        samples: verified_samples,
        verified_nlos_pkg_count: 8,
        verified_certify_report_count: 4,
        verified_store_seal_count: 4,
        promoted_stable_release_count: stable_releases.len(),
    })
}
