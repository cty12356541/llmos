//! Domain-separated [`StoreCertificationReceipt`] issuance, pure verification,
//! and canonical RFC 8259 JSON serialization (`ADR-0027` §3.5).
//!
//! Issuance is fail-closed on two orthogonal pre-conditions:
//! 1. Submitting publisher identity AND `ApplicationId` ownership MUST verify
//!    cleanly through `&dyn PublisherTrustVerifier`.
//! 2. All four certification gates (`Gate 1`..`Gate 4`) MUST report
//!    `GateStatus::Pass` with `blocker_count == 0`.
//!
//! Signing supports both hardware/KEK-sealed [`SigningKeyCustodyVault`] slots
//! (zero private-key export) and direct Ed25519 certifier keys over the exact
//! same domain-separated 32-byte preimage digest.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use nlos_artifact::ContentDigest;
use nlos_capability::{CustodySignRequest, SigningKeyCustodyVault};
use nlos_publisher_trust::{
    ApplicationOwnershipBinding, PublisherId, PublisherTrustCheckError, PublisherTrustTier,
    PublisherTrustVerifier, RevocationEpoch, VerifiedPublisherIdentity,
};
use nlos_types::{ApplicationId, KeyId, PackageId, ReceiptId};
use sha2::{Digest, Sha256};

use crate::json::{JsonValue, hex_decode_array, hex_encode, parse_json, quote_json};
use crate::model::{
    CertifyError, GateExecutionSummary, GateId, GateStatus, LoadedCertifyPackage, SurfaceProfile,
};

/// Domain-separation tag for [`StoreCertificationReceipt`] Ed25519 signatures.
pub const STORE_CERT_RECEIPT_DOMAIN: &[u8] = b"nlos/certify/store-certification-receipt/v1";

/// Domain-separation tag for publisher certification submission proofs verified
/// via [`PublisherTrustVerifier::verify_publisher_signature`].
pub const PUBLISHER_CERT_SUBMISSION_DOMAIN: &[u8] = b"nlos/certify/publisher-submission/v1\0";

/// Domain-separation tag for deterministic [`ReceiptId`] derivation.
const STORE_CERT_RECEIPT_ID_DOMAIN: &[u8] = b"nlos/certify/store-certification-receipt-id/v1";

/// Default certification receipt validity duration (90 days in milliseconds).
pub const DEFAULT_CERT_RECEIPT_TTL_MS: u64 = 90 * 24 * 60 * 60 * 1_000;

/// Computes the canonical 32-byte SHA-256 digest signed by a submitting publisher
/// when requesting automated store certification for `(application_id, package_id, manifest_digest)`.
#[must_use]
pub fn publisher_certification_submission_digest(
    publisher_id: PublisherId,
    application_id: ApplicationId,
    package_id: PackageId,
    manifest_digest: ContentDigest,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PUBLISHER_CERT_SUBMISSION_DOMAIN);
    hasher.update(publisher_id.as_bytes());
    hasher.update(application_id.as_bytes());
    hasher.update(package_id.as_bytes());
    hasher.update(manifest_digest.as_bytes());
    hasher.finalize().into()
}

/// Cryptographic proof signed by the submitting publisher's leaf signing key and
/// verified through [`PublisherTrustVerifier::verify_publisher_signature`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherSubmissionProof {
    /// Submitting publisher identifier.
    pub publisher_id: PublisherId,
    /// 32-byte Ed25519 leaf signing public key registered for `publisher_id`.
    pub signing_public_key: [u8; 32],
    /// 64-byte Ed25519 signature over [`publisher_certification_submission_digest`].
    pub signature: [u8; 64],
}

impl PublisherSubmissionProof {
    /// Signs a deterministic publisher submission proof for explicit identifiers.
    #[must_use]
    pub fn sign(
        publisher_id: PublisherId,
        signing_key: &SigningKey,
        application_id: ApplicationId,
        package_id: PackageId,
        manifest_digest: ContentDigest,
    ) -> Self {
        let digest = publisher_certification_submission_digest(
            publisher_id,
            application_id,
            package_id,
            manifest_digest,
        );
        let signature = signing_key.sign(&digest).to_bytes();
        Self {
            publisher_id,
            signing_public_key: signing_key.verifying_key().to_bytes(),
            signature,
        }
    }

    /// Signs a deterministic publisher submission proof for a loaded package.
    #[must_use]
    pub fn for_package(
        publisher_id: PublisherId,
        signing_key: &SigningKey,
        pkg: &LoadedCertifyPackage,
    ) -> Self {
        Self::sign(
            publisher_id,
            signing_key,
            pkg.application_id,
            pkg.package_id,
            pkg.manifest_digest,
        )
    }
}

/// Certifier signing backend: either a KEK-sealed [`SigningKeyCustodyVault`]
/// slot or an in-memory Ed25519 certifier key.
pub enum StoreCertifierSigner<'a> {
    /// Non-exportable key slot inside [`SigningKeyCustodyVault`] (`ADR-0022`).
    CustodyVault {
        /// Reference to the opened custody vault.
        vault: &'a SigningKeyCustodyVault,
        /// Certifier key slot identifier.
        key_id: KeyId,
        /// Optional pinned key rotation epoch fence.
        expected_rotation_epoch: Option<u64>,
    },
    /// Direct Ed25519 certifier signing key (e.g. offline CLI / test certifier).
    Ed25519Key {
        /// Certifier key identifier.
        key_id: KeyId,
        /// Ed25519 signing key.
        signing_key: &'a SigningKey,
    },
}

impl StoreCertifierSigner<'_> {
    fn resolve_public_key(&self) -> Result<(KeyId, [u8; 32]), CertifyError> {
        match *self {
            Self::CustodyVault {
                vault,
                key_id,
                expected_rotation_epoch,
            } => {
                let slot = match expected_rotation_epoch {
                    Some(epoch) => vault.inspect_key_slot_epoch(key_id, epoch),
                    None => vault.inspect_latest_key_slot(key_id),
                }
                .map_err(|e| {
                    CertifyError::Authority(format!("inspect certifier custody key slot: {e}"))
                })?;
                Ok((slot.key_id, slot.public_key))
            }
            Self::Ed25519Key {
                key_id,
                signing_key,
            } => Ok((key_id, signing_key.verifying_key().to_bytes())),
        }
    }

    fn sign_digest(
        &self,
        message_digest: [u8; 32],
        signed_at_ms: u64,
    ) -> Result<[u8; 64], CertifyError> {
        match *self {
            Self::CustodyVault {
                vault,
                key_id,
                expected_rotation_epoch,
            } => {
                let audit = vault
                    .sign_digest_under_custody(CustodySignRequest {
                        key_id,
                        expected_rotation_epoch,
                        domain_tag: STORE_CERT_RECEIPT_DOMAIN,
                        message_digest,
                        signed_at_ms,
                    })
                    .map_err(|e| {
                        CertifyError::Authority(format!("custody vault signing failed: {e}"))
                    })?;
                Ok(audit.signature)
            }
            Self::Ed25519Key { signing_key, .. } => {
                Ok(signing_key.sign(&message_digest).to_bytes())
            }
        }
    }
}

/// Request parameters for [`issue_store_certification_receipt`].
#[derive(Clone, Debug)]
pub struct IssueStoreCertificationRequest {
    /// Publisher submission proof verified against [`PublisherTrustVerifier`].
    pub submission_proof: PublisherSubmissionProof,
    /// Target `ApplicationId` whose ownership is verified for `submission_proof.publisher_id`.
    pub application_id: ApplicationId,
    /// Certified `PackageId`.
    pub package_id: PackageId,
    /// Certified package `SemVer` string (e.g. `"1.0.0"`).
    pub package_version: String,
    /// Content digest committing to the certified package directory.
    pub manifest_digest: ContentDigest,
    /// Certified target surface profile (`APPLICATION_PLATFORM` or `DESKTOP`).
    pub surface_profile: SurfaceProfile,
    /// Declared `target-sabi-epoch` of the certified package.
    pub target_sabi_epoch: u32,
    /// Ordered execution summaries for Gate 1..Gate 4.
    pub gate_summaries: [GateExecutionSummary; 4],
    /// Authoritative issuance timestamp (ms since Unix epoch).
    pub issued_at_ms: u64,
    /// Authoritative expiration timestamp (ms since Unix epoch; must exceed `issued_at_ms`).
    pub expires_at_ms: u64,
}

/// Immutable, Ed25519-signed store certification receipt proving a package
/// passed publisher ownership checks and all 4 automated certification gates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreCertificationReceipt {
    /// Deterministic 128-bit receipt identifier.
    pub receipt_id: ReceiptId,
    /// Verified publisher identifier.
    pub publisher_id: PublisherId,
    /// Publisher trust tier attested by [`PublisherTrustVerifier`] at issuance.
    pub trust_tier: PublisherTrustTier,
    /// Verified owned `ApplicationId`.
    pub application_id: ApplicationId,
    /// Certified `PackageId`.
    pub package_id: PackageId,
    /// Certified package version string.
    pub package_version: String,
    /// Package manifest & payload content commitment.
    pub manifest_digest: ContentDigest,
    /// Certified `SurfaceProfile`.
    pub surface_profile: SurfaceProfile,
    /// Certified `target-sabi-epoch`.
    pub target_sabi_epoch: u32,
    /// Ordered `[Gate 1, Gate 2, Gate 3, Gate 4]` summaries (all `PASS`).
    pub gate_summaries: [GateExecutionSummary; 4],
    /// Publisher trust revocation epoch observed at issuance.
    pub revocation_epoch: RevocationEpoch,
    /// Issuance timestamp (ms since Unix epoch).
    pub issued_at_ms: u64,
    /// Expiration timestamp (ms since Unix epoch).
    pub expires_at_ms: u64,
    /// Certifier key identifier.
    pub certifier_key_id: KeyId,
    /// 32-byte Ed25519 verifying key of the certifier.
    pub certifier_public_key: [u8; 32],
    /// 64-byte Ed25519 signature over [`store_certification_receipt_signing_digest`].
    pub ed25519_signature: [u8; 64],
}

/// Verifies publisher identity & `ApplicationId` ownership via `trust_verifier`,
/// enforces `4/4 PASS` across `request.gate_summaries`, and signs a
/// domain-separated [`StoreCertificationReceipt`].
///
/// # Errors
///
/// Fails closed if publisher trust fails, if any gate is not `PASS`, if
/// `expires_at_ms <= issued_at_ms`, or if the certifier signing key slot fails.
pub fn issue_store_certification_receipt(
    trust_verifier: &dyn PublisherTrustVerifier,
    signer: &StoreCertifierSigner<'_>,
    request: IssueStoreCertificationRequest,
) -> Result<StoreCertificationReceipt, CertifyError> {
    if request.expires_at_ms <= request.issued_at_ms {
        return Err(CertifyError::ReceiptVerification(
            "expires_at_ms must be strictly greater than issued_at_ms".to_owned(),
        ));
    }

    // 1. Publisher signature + ApplicationId ownership verification (fail-closed).
    let publisher_id = request.submission_proof.publisher_id;
    let submission_digest = publisher_certification_submission_digest(
        publisher_id,
        request.application_id,
        request.package_id,
        request.manifest_digest,
    );
    let verified_pub = trust_verifier.verify_publisher_signature(
        publisher_id,
        &request.submission_proof.signing_public_key,
        &submission_digest,
        &request.submission_proof.signature,
        request.issued_at_ms,
    )?;
    if verified_pub.publisher_id != publisher_id {
        return Err(CertifyError::PublisherTrust(
            PublisherTrustCheckError::UnknownPublisher(publisher_id),
        ));
    }
    let ownership = trust_verifier.verify_or_bind_application_owner(
        publisher_id,
        verified_pub.trust_tier,
        request.application_id,
        request.issued_at_ms,
    )?;
    if ownership.owner_publisher_id != publisher_id
        || ownership.application_id != request.application_id
    {
        return Err(CertifyError::PublisherTrust(
            PublisherTrustCheckError::ApplicationIdHijackAttempt {
                application_id: request.application_id,
                registered_owner: ownership.owner_publisher_id,
                attempted_publisher: publisher_id,
            },
        ));
    }

    // 2. Enforce 4/4 PASS in canonical Gate 1..Gate 4 order.
    let mut failed_gates = Vec::new();
    for (idx, expected_gate) in GateId::ALL.into_iter().enumerate() {
        let summary = &request.gate_summaries[idx];
        if summary.gate != expected_gate {
            return Err(CertifyError::ReceiptVerification(format!(
                "gate_summaries[{idx}] has gate {}, expected {}",
                summary.gate.as_str(),
                expected_gate.as_str()
            )));
        }
        if summary.status != GateStatus::Pass || summary.blocker_count != 0 {
            failed_gates.push(summary.gate);
        }
    }
    if !failed_gates.is_empty() {
        return Err(CertifyError::GatesNotAllPassed { failed_gates });
    }

    // 3. Resolve certifier public key, derive ReceiptId, and sign preimage digest.
    let (certifier_key_id, certifier_public_key) = signer.resolve_public_key()?;

    let mut receipt = StoreCertificationReceipt {
        receipt_id: ReceiptId::from_bytes([0_u8; 16]),
        publisher_id,
        trust_tier: verified_pub.trust_tier,
        application_id: request.application_id,
        package_id: request.package_id,
        package_version: request.package_version,
        manifest_digest: request.manifest_digest,
        surface_profile: request.surface_profile,
        target_sabi_epoch: request.target_sabi_epoch,
        gate_summaries: request.gate_summaries,
        revocation_epoch: verified_pub.revocation_epoch,
        issued_at_ms: request.issued_at_ms,
        expires_at_ms: request.expires_at_ms,
        certifier_key_id,
        certifier_public_key,
        ed25519_signature: [0_u8; 64],
    };
    receipt.receipt_id = derive_receipt_id(&receipt);

    let signing_digest = store_certification_receipt_signing_digest(&receipt);
    receipt.ed25519_signature = signer.sign_digest(signing_digest, receipt.issued_at_ms)?;
    Ok(receipt)
}

/// Pure, deterministic verification of a [`StoreCertificationReceipt`].
///
/// Fails closed on any single-bit field or signature tamper, any single gate
/// not in `PASS` with `0` blockers, out-of-order gates, unissued/expired
/// validity window at `now_ms`, or mismatch against `expected_certifier_public_key`.
///
/// # Errors
///
/// Returns [`CertifyError::ReceiptVerification`] on any structural, temporal,
/// gate-verdict, or Ed25519 cryptographic failure.
pub fn verify_store_certification_receipt(
    receipt: &StoreCertificationReceipt,
    expected_certifier_public_key: Option<&[u8; 32]>,
    now_ms: u64,
) -> Result<(), CertifyError> {
    if receipt.package_version.trim().is_empty() {
        return Err(CertifyError::ReceiptVerification(
            "package_version must be non-empty".to_owned(),
        ));
    }
    if receipt.expires_at_ms <= receipt.issued_at_ms {
        return Err(CertifyError::ReceiptVerification(
            "invalid receipt validity window (expires_at_ms <= issued_at_ms)".to_owned(),
        ));
    }
    if now_ms < receipt.issued_at_ms {
        return Err(CertifyError::ReceiptVerification(format!(
            "receipt is not yet valid at {now_ms} (issued_at_ms={})",
            receipt.issued_at_ms
        )));
    }
    if now_ms > receipt.expires_at_ms {
        return Err(CertifyError::ReceiptVerification(format!(
            "receipt expired at {} (now_ms={now_ms})",
            receipt.expires_at_ms
        )));
    }

    for (idx, expected_gate) in GateId::ALL.into_iter().enumerate() {
        let gate_summary = &receipt.gate_summaries[idx];
        if gate_summary.gate != expected_gate {
            return Err(CertifyError::ReceiptVerification(format!(
                "gate slot {idx} mismatch: expected {}, got {}",
                expected_gate.as_str(),
                gate_summary.gate.as_str()
            )));
        }
        if gate_summary.status != GateStatus::Pass || gate_summary.blocker_count != 0 {
            return Err(CertifyError::ReceiptVerification(format!(
                "receipt carries non-passing gate {} (status={}, blockers={})",
                gate_summary.gate.as_str(),
                gate_summary.status.as_str(),
                gate_summary.blocker_count
            )));
        }
    }

    if let Some(expected_pk) = expected_certifier_public_key
        && &receipt.certifier_public_key != expected_pk
    {
        return Err(CertifyError::ReceiptVerification(
            "certifier_public_key does not match trusted certifier key".to_owned(),
        ));
    }

    let expected_id = derive_receipt_id(receipt);
    if receipt.receipt_id != expected_id {
        return Err(CertifyError::ReceiptVerification(
            "receipt_id does not match canonical field derivation".to_owned(),
        ));
    }

    let verifying_key = VerifyingKey::from_bytes(&receipt.certifier_public_key)
        .map_err(|_| CertifyError::ReceiptVerification("invalid Ed25519 public key".to_owned()))?;
    let signature = Signature::from_bytes(&receipt.ed25519_signature);
    let signing_digest = store_certification_receipt_signing_digest(receipt);
    verifying_key
        .verify_strict(&signing_digest, &signature)
        .map_err(|_| {
            CertifyError::ReceiptVerification(
                "Ed25519 strict signature verification failed".to_owned(),
            )
        })?;

    Ok(())
}

/// Computes the canonical domain-separated 32-byte SHA-256 signing digest over
/// every unsigned field of `receipt` (including `receipt_id`).
#[must_use]
pub fn store_certification_receipt_signing_digest(receipt: &StoreCertificationReceipt) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(STORE_CERT_RECEIPT_DOMAIN);
    hasher.update([0]);
    hasher.update(receipt.receipt_id.as_bytes());
    hash_unsigned_fields(&mut hasher, receipt);
    hasher.finalize().into()
}

fn derive_receipt_id(receipt: &StoreCertificationReceipt) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(STORE_CERT_RECEIPT_ID_DOMAIN);
    hasher.update([0]);
    hash_unsigned_fields(&mut hasher, receipt);
    let digest = hasher.finalize();
    let mut id_bytes = [0_u8; 16];
    id_bytes.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(id_bytes)
}

fn hash_unsigned_fields(hasher: &mut Sha256, receipt: &StoreCertificationReceipt) {
    hasher.update(receipt.publisher_id.as_bytes());
    hasher.update([receipt.trust_tier.encode()]);
    hasher.update(receipt.application_id.as_bytes());
    hasher.update(receipt.package_id.as_bytes());
    hasher.update((receipt.package_version.len() as u64).to_be_bytes());
    hasher.update(receipt.package_version.as_bytes());
    hasher.update(receipt.manifest_digest.as_bytes());
    hasher.update((receipt.surface_profile.as_str().len() as u64).to_be_bytes());
    hasher.update(receipt.surface_profile.as_str().as_bytes());
    hasher.update(receipt.target_sabi_epoch.to_be_bytes());
    for gs in &receipt.gate_summaries {
        hasher.update([gs.gate.ordinal()]);
        hasher.update((gs.status.as_str().len() as u64).to_be_bytes());
        hasher.update(gs.status.as_str().as_bytes());
        hasher.update((gs.blocker_count as u64).to_be_bytes());
        hasher.update((gs.warning_count as u64).to_be_bytes());
        hasher.update(gs.evidence_digest);
        hasher.update((gs.summary_line.len() as u64).to_be_bytes());
        hasher.update(gs.summary_line.as_bytes());
    }
    hasher.update(receipt.revocation_epoch.get().to_be_bytes());
    hasher.update(receipt.issued_at_ms.to_be_bytes());
    hasher.update(receipt.expires_at_ms.to_be_bytes());
    hasher.update(receipt.certifier_key_id.as_bytes());
    hasher.update(receipt.certifier_public_key);
}

impl StoreCertificationReceipt {
    /// Serializes this receipt into deterministic RFC 8259 JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        let gates_json: Vec<String> = self
            .gate_summaries
            .iter()
            .map(GateExecutionSummary::to_json)
            .collect();
        format!(
            "{{\"receipt_id\":{},\"publisher_id\":{},\"trust_tier\":{},\"application_id\":{},\"package_id\":{},\"package_version\":{},\"manifest_digest\":{},\"surface_profile\":{},\"target_sabi_epoch\":{},\"gate_summaries\":[{}],\"revocation_epoch\":{},\"issued_at_ms\":{},\"expires_at_ms\":{},\"certifier_key_id\":{},\"certifier_public_key\":{},\"ed25519_signature\":{}}}",
            quote_json(&hex_encode(self.receipt_id.as_bytes())),
            quote_json(&hex_encode(self.publisher_id.as_bytes())),
            quote_json(self.trust_tier.as_str()),
            quote_json(&hex_encode(self.application_id.as_bytes())),
            quote_json(&hex_encode(self.package_id.as_bytes())),
            quote_json(&self.package_version),
            quote_json(&hex_encode(self.manifest_digest.as_bytes())),
            quote_json(self.surface_profile.as_str()),
            self.target_sabi_epoch,
            gates_json.join(","),
            self.revocation_epoch.get(),
            self.issued_at_ms,
            self.expires_at_ms,
            quote_json(&hex_encode(self.certifier_key_id.as_bytes())),
            quote_json(&hex_encode(&self.certifier_public_key)),
            quote_json(&hex_encode(&self.ed25519_signature)),
        )
    }

    /// Deserializes a [`Self`] from an RFC 8259 JSON string.
    ///
    /// # Errors
    ///
    /// Returns [`CertifyError`] if the JSON syntax is invalid or any required
    /// field is missing or malformed.
    pub fn from_json(json_str: &str) -> Result<Self, CertifyError> {
        let val = parse_json(json_str).map_err(|source| CertifyError::JsonParse {
            context: "StoreCertificationReceipt".to_owned(),
            source,
        })?;
        Self::from_json_value(&val)
    }

    /// Decodes a [`Self`] from an already-parsed [`JsonValue`].
    ///
    /// # Errors
    ///
    /// Returns [`CertifyError::ReceiptVerification`] if any field is missing or invalid.
    pub fn from_json_value(val: &JsonValue) -> Result<Self, CertifyError> {
        let obj = val.as_object().ok_or_else(|| {
            CertifyError::ReceiptVerification("receipt JSON must be an object".to_owned())
        })?;

        let receipt_id = ReceiptId::from_bytes(req_hex::<16>(obj, "receipt_id")?);
        let publisher_id = PublisherId::from_bytes(req_hex::<16>(obj, "publisher_id")?);
        let trust_tier = parse_trust_tier(req_str(obj, "trust_tier")?).ok_or_else(|| {
            CertifyError::ReceiptVerification("unknown trust_tier in receipt JSON".to_owned())
        })?;
        let application_id = ApplicationId::from_bytes(req_hex::<16>(obj, "application_id")?);
        let package_id = PackageId::from_bytes(req_hex::<16>(obj, "package_id")?);
        let package_version = req_str(obj, "package_version")?.to_owned();
        let manifest_digest = ContentDigest::from_bytes(req_hex::<32>(obj, "manifest_digest")?);
        let surface_profile =
            SurfaceProfile::parse(req_str(obj, "surface_profile")?).ok_or_else(|| {
                CertifyError::ReceiptVerification(
                    "invalid surface_profile in receipt JSON".to_owned(),
                )
            })?;
        let target_sabi_epoch = obj
            .get("target_sabi_epoch")
            .and_then(JsonValue::as_u32)
            .ok_or_else(|| {
                CertifyError::ReceiptVerification("missing/invalid target_sabi_epoch".to_owned())
            })?;

        let gates_arr = obj
            .get("gate_summaries")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| {
                CertifyError::ReceiptVerification("missing gate_summaries array".to_owned())
            })?;
        if gates_arr.len() != 4 {
            return Err(CertifyError::ReceiptVerification(format!(
                "expected 4 gate_summaries, got {}",
                gates_arr.len()
            )));
        }
        let gate_summaries = [
            decode_gate_summary(&gates_arr[0])?,
            decode_gate_summary(&gates_arr[1])?,
            decode_gate_summary(&gates_arr[2])?,
            decode_gate_summary(&gates_arr[3])?,
        ];

        let revocation_epoch = RevocationEpoch::new(req_u64(obj, "revocation_epoch")?);
        let issued_at_ms = req_u64(obj, "issued_at_ms")?;
        let expires_at_ms = req_u64(obj, "expires_at_ms")?;
        let certifier_key_id = KeyId::from_bytes(req_hex::<16>(obj, "certifier_key_id")?);
        let certifier_public_key = req_hex::<32>(obj, "certifier_public_key")?;
        let ed25519_signature = req_hex::<64>(obj, "ed25519_signature")?;

        Ok(Self {
            receipt_id,
            publisher_id,
            trust_tier,
            application_id,
            package_id,
            package_version,
            manifest_digest,
            surface_profile,
            target_sabi_epoch,
            gate_summaries,
            revocation_epoch,
            issued_at_ms,
            expires_at_ms,
            certifier_key_id,
            certifier_public_key,
            ed25519_signature,
        })
    }
}

fn decode_gate_summary(val: &JsonValue) -> Result<GateExecutionSummary, CertifyError> {
    let obj = val.as_object().ok_or_else(|| {
        CertifyError::ReceiptVerification("gate_summary entry must be an object".to_owned())
    })?;
    let gate = GateId::parse(req_str(obj, "gate")?).ok_or_else(|| {
        CertifyError::ReceiptVerification("invalid gate id in gate_summary".to_owned())
    })?;
    let status = GateStatus::parse(req_str(obj, "status")?).ok_or_else(|| {
        CertifyError::ReceiptVerification("invalid gate status in gate_summary".to_owned())
    })?;
    let blocker_count = obj
        .get("blocker_count")
        .and_then(JsonValue::as_usize)
        .ok_or_else(|| CertifyError::ReceiptVerification("missing blocker_count".to_owned()))?;
    let warning_count = obj
        .get("warning_count")
        .and_then(JsonValue::as_usize)
        .ok_or_else(|| CertifyError::ReceiptVerification("missing warning_count".to_owned()))?;
    let evidence_digest = req_hex::<32>(obj, "evidence_digest")?;
    let summary_line = req_str(obj, "summary_line")?.to_owned();
    Ok(GateExecutionSummary {
        gate,
        status,
        blocker_count,
        warning_count,
        evidence_digest,
        summary_line,
    })
}

fn req_str<'a>(
    obj: &'a std::collections::BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<&'a str, CertifyError> {
    obj.get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| CertifyError::ReceiptVerification(format!("missing string field '{key}'")))
}

fn req_u64(
    obj: &std::collections::BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<u64, CertifyError> {
    obj.get(key)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| CertifyError::ReceiptVerification(format!("missing u64 field '{key}'")))
}

fn req_hex<const N: usize>(
    obj: &std::collections::BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<[u8; N], CertifyError> {
    let raw = req_str(obj, key)?;
    hex_decode_array::<N>(raw).map_err(|detail| {
        CertifyError::ReceiptVerification(format!("invalid hex in '{key}': {detail}"))
    })
}

fn parse_trust_tier(raw: &str) -> Option<PublisherTrustTier> {
    match raw.trim() {
        "PLATFORM_CORE" => Some(PublisherTrustTier::PlatformCore),
        "CERTIFIED_PUBLISHER" => Some(PublisherTrustTier::CertifiedPublisher),
        "COMMUNITY_SANDBOXED" => Some(PublisherTrustTier::CommunitySandboxed),
        _ => None,
    }
}

/// Deterministic in-memory [`PublisherTrustVerifier`] used by offline local CLI
/// certification runs and unit/integration test harnesses.
#[derive(Clone, Debug)]
pub struct StaticPublisherTrustVerifier {
    verified_publishers: std::collections::BTreeMap<PublisherId, VerifiedPublisherIdentity>,
    ownership_bindings: std::collections::BTreeMap<ApplicationId, ApplicationOwnershipBinding>,
    revocation_epoch: RevocationEpoch,
}

impl Default for StaticPublisherTrustVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticPublisherTrustVerifier {
    /// Creates an empty [`Self`] that rejects all unknown publishers and applications.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            verified_publishers: std::collections::BTreeMap::new(),
            ownership_bindings: std::collections::BTreeMap::new(),
            revocation_epoch: RevocationEpoch::new(0),
        }
    }

    /// Registers a verified publisher identity.
    pub fn register_publisher(&mut self, identity: VerifiedPublisherIdentity) {
        if identity.revocation_epoch > self.revocation_epoch {
            self.revocation_epoch = identity.revocation_epoch;
        }
        self.verified_publishers
            .insert(identity.publisher_id, identity);
    }

    /// Registers an `ApplicationId` ownership binding.
    pub fn register_ownership(&mut self, binding: ApplicationOwnershipBinding) {
        self.ownership_bindings
            .insert(binding.application_id, binding);
    }

    /// Convenience constructor registering one verified publisher leaf key owning one `ApplicationId`.
    #[must_use]
    pub fn single_verified_owner(
        publisher_id: PublisherId,
        trust_tier: PublisherTrustTier,
        leaf_public_key: [u8; 32],
        application_id: ApplicationId,
        revocation_epoch: RevocationEpoch,
        at_ms: u64,
    ) -> Self {
        let mut v = Self::new();
        v.revocation_epoch = revocation_epoch;
        v.register_publisher(VerifiedPublisherIdentity {
            publisher_id,
            trust_tier,
            leaf_key_id: KeyId::from_bytes([0xC1_u8; 16]),
            leaf_public_key,
            revocation_epoch,
            verified_at_ms: at_ms,
        });
        v.register_ownership(ApplicationOwnershipBinding {
            application_id,
            owner_publisher_id: publisher_id,
            trust_tier,
            first_registered_at_ms: at_ms,
            registration_generation: 1,
        });
        v
    }
}

impl PublisherTrustVerifier for StaticPublisherTrustVerifier {
    fn current_revocation_epoch(&self) -> Result<RevocationEpoch, PublisherTrustCheckError> {
        Ok(self.revocation_epoch)
    }

    fn verify_publisher_signature(
        &self,
        publisher_id: PublisherId,
        signing_public_key: &[u8; 32],
        message_digest: &[u8; 32],
        signature: &[u8; 64],
        now_ms: u64,
    ) -> Result<VerifiedPublisherIdentity, PublisherTrustCheckError> {
        let Some(mut identity) = self.verified_publishers.get(&publisher_id).copied() else {
            return Err(PublisherTrustCheckError::UnknownPublisher(publisher_id));
        };
        if &identity.leaf_public_key != signing_public_key {
            return Err(PublisherTrustCheckError::UntrustedCertificateChain(
                "signing_public_key does not match registered publisher leaf key",
            ));
        }
        let verifying_key = VerifyingKey::from_bytes(signing_public_key)
            .map_err(|_| PublisherTrustCheckError::SignatureInvalid)?;
        let sig = Signature::from_bytes(signature);
        verifying_key
            .verify_strict(message_digest, &sig)
            .map_err(|_| PublisherTrustCheckError::SignatureInvalid)?;
        identity.verified_at_ms = now_ms;
        Ok(identity)
    }

    fn verify_or_bind_application_owner(
        &self,
        publisher_id: PublisherId,
        _trust_tier: PublisherTrustTier,
        application_id: ApplicationId,
        _now_ms: u64,
    ) -> Result<ApplicationOwnershipBinding, PublisherTrustCheckError> {
        let Some(binding) = self.ownership_bindings.get(&application_id).copied() else {
            return Err(PublisherTrustCheckError::UnknownPublisher(publisher_id));
        };
        if binding.owner_publisher_id != publisher_id {
            return Err(PublisherTrustCheckError::ApplicationIdHijackAttempt {
                application_id,
                registered_owner: binding.owner_publisher_id,
                attempted_publisher: publisher_id,
            });
        }
        Ok(binding)
    }
}
