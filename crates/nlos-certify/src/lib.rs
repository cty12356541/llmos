//! Stage E `E5` (right half) — Automated 4-gate store certification pipeline,
//! pure-Rust SPDX/lockfile supply-chain auditor (`RISK-B-09`), and
//! domain-separated [`StoreCertificationReceipt`] issuer (`ADR-0027` §3).

#![forbid(unsafe_code)]

pub mod gate1_protocol;
pub mod gate2_supply_chain;
pub mod gate3_update_rollback;
pub mod gate4_uninstall_audit;
pub mod json;
pub mod model;
pub mod receipt;
pub mod supply_chain_audit;

use std::fmt::Write as _;
use std::path::Path;

use nlos_artifact::ContentDigest;
use nlos_publisher_trust::{PublisherId, PublisherTrustVerifier};
use nlos_types::{ApplicationId, PackageId};
use sha2::{Digest, Sha256};

pub use gate1_protocol::{
    FROZEN_SABI_V1_CATALOG, FrozenSabiServiceSpec, Gate1Outcome, run_gate1_protocol_audit,
};
pub use gate2_supply_chain::{Gate2Outcome, run_gate2_supply_chain_audit};
pub use gate3_update_rollback::{
    Gate3DrillOptions, Gate3Outcome, run_gate3_update_rollback_drill,
    run_gate3_update_rollback_drill_with_options,
};
pub use gate4_uninstall_audit::{
    Gate4DrillOptions, Gate4Outcome, UserDataRetentionDisposition, Z4UserDataPurgeConsentReceipt,
    purge_user_data_container_if_z4_confirmed, run_gate4_uninstall_audit,
    run_gate4_uninstall_audit_with_options, sign_z4_user_data_purge_consent,
    z4_user_data_purge_digest,
};
pub use model::{
    CertifyError, CertifyExtendedDirectives, CertifyFinding, CertifySeverity,
    DEFAULT_CERTIFY_HOST_SABI_EPOCH, DeclaredFileAssociation, DeclaredIdempotencyPolicy,
    DeclaredSabiServiceBinding, GateExecutionSummary, GateId, GateStatus, LoadedCertifyPackage,
    SabiServiceDirection, SurfaceProfile, derive_application_id_from_str,
    derive_package_id_from_str, parse_version_ordinal,
};
pub use receipt::{
    DEFAULT_CERT_RECEIPT_TTL_MS, IssueStoreCertificationRequest, PUBLISHER_CERT_SUBMISSION_DOMAIN,
    PublisherSubmissionProof, STORE_CERT_RECEIPT_DOMAIN, StaticPublisherTrustVerifier,
    StoreCertificationReceipt, StoreCertifierSigner, issue_store_certification_receipt,
    publisher_certification_submission_digest, store_certification_receipt_signing_digest,
    verify_store_certification_receipt,
};
pub use supply_chain_audit::{
    AdvisoryDenyEntry, DEFAULT_ALLOWED_SPDX_EXCEPTIONS, DEFAULT_ALLOWED_SPDX_LICENSES,
    DependencyEcosystem, SupplyChainAuditReport, SupplyChainFinding, SupplyChainPolicy,
    SupplyChainViolationKind, audit_dependency_supply_chain, evaluate_spdx_expression,
};

use crate::json::{hex_decode_array, hex_encode, quote_json};

/// Canonical pipeline schema version for Stage E automated store certification.
pub const STORE_CERTIFY_PIPELINE_VERSION: u32 = 1;

/// Execution parameters for [`run_certification_pipeline`].
#[derive(Clone, Debug)]
pub struct CertifyPipelineConfig {
    /// Target surface profile (`APPLICATION_PLATFORM` or `DESKTOP`).
    pub surface_profile: SurfaceProfile,
    /// Host SABI epoch against which `N / N-1 / N-2` compatibility is evaluated.
    pub host_sabi_epoch: u32,
    /// Cryptographic submission proof signed by the submitting publisher and
    /// verified through [`PublisherTrustVerifier`].
    pub submission_proof: PublisherSubmissionProof,
    /// Issuance timestamp (ms since Unix epoch).
    pub issued_at_ms: u64,
    /// Expiration timestamp (ms since Unix epoch).
    pub expires_at_ms: u64,
    /// Dependency SPDX & lockfile supply-chain policy for Gate 2.
    pub supply_chain_policy: SupplyChainPolicy,
}

impl CertifyPipelineConfig {
    /// Constructs a default pipeline configuration for `submission_proof` and
    /// `surface_profile` at `issued_at_ms`.
    #[must_use]
    pub fn new(
        submission_proof: PublisherSubmissionProof,
        surface_profile: SurfaceProfile,
        issued_at_ms: u64,
    ) -> Self {
        Self {
            surface_profile,
            host_sabi_epoch: DEFAULT_CERTIFY_HOST_SABI_EPOCH,
            submission_proof,
            issued_at_ms,
            expires_at_ms: issued_at_ms.saturating_add(DEFAULT_CERT_RECEIPT_TTL_MS),
            supply_chain_policy: SupplyChainPolicy::default(),
        }
    }
}

/// Derives a deterministic 128-bit [`PublisherId`] from either a 32-hex string
/// or a symbolic publisher handle (e.g. `"nlos.first-party.starter"`).
#[must_use]
pub fn derive_publisher_id_from_str(raw: &str) -> PublisherId {
    if let Ok(bytes) = hex_decode_array::<16>(raw.trim()) {
        return PublisherId::from_bytes(bytes);
    }
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/certify/publisher-id/v1\0");
    hasher.update(raw.trim().as_bytes());
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    PublisherId::from_bytes(out)
}

/// Complete report across Gate 1..Gate 4 and conditional [`StoreCertificationReceipt`]
/// issuance.
#[derive(Clone, Debug)]
pub struct CertificationPipelineReport {
    /// Symbolic `package-id` declared in `package.manifest`.
    pub package_id_label: String,
    /// Derived 128-bit `PackageId`.
    pub package_id: PackageId,
    /// Derived 128-bit `ApplicationId`.
    pub application_id: ApplicationId,
    /// Declared SemVer/version string.
    pub package_version: String,
    /// Content digest committing to the certified package directory.
    pub manifest_digest: ContentDigest,
    /// Certified surface profile.
    pub surface_profile: SurfaceProfile,
    /// Declared `target-sabi-epoch`.
    pub target_sabi_epoch: u32,
    /// Gate 1 detailed outcome.
    pub gate1: Gate1Outcome,
    /// Gate 2 detailed outcome.
    pub gate2: Gate2Outcome,
    /// Gate 3 detailed outcome.
    pub gate3: Gate3Outcome,
    /// Gate 4 detailed outcome.
    pub gate4: Gate4Outcome,
    /// Signed certification receipt (`Some` iff publisher trust passed and 4/4 gates passed).
    pub receipt: Option<StoreCertificationReceipt>,
    /// Diagnostic reason when `receipt` was withheld (`None` when `receipt.is_some()`).
    pub receipt_rejection_reason: Option<String>,
}

impl CertificationPipelineReport {
    /// Returns the ordered 4-element gate summary array.
    #[must_use]
    pub fn gate_summaries(&self) -> [GateExecutionSummary; 4] {
        [
            self.gate1.summary.clone(),
            self.gate2.summary.clone(),
            self.gate3.summary.clone(),
            self.gate4.summary.clone(),
        ]
    }

    /// Returns `true` iff all 4 gates passed with `0` blockers.
    #[must_use]
    pub fn all_gates_passed(&self) -> bool {
        self.gate1.summary.status == GateStatus::Pass
            && self.gate2.summary.status == GateStatus::Pass
            && self.gate3.summary.status == GateStatus::Pass
            && self.gate4.summary.status == GateStatus::Pass
    }

    /// Returns `true` iff all 4 gates passed AND a signed [`StoreCertificationReceipt`]
    /// was issued.
    #[must_use]
    pub const fn certified(&self) -> bool {
        self.receipt.is_some()
    }

    /// Collects all findings emitted across Gate 1..Gate 4 in gate order.
    #[must_use]
    pub fn all_findings(&self) -> Vec<&CertifyFinding> {
        let mut out = Vec::new();
        out.extend(self.gate1.findings.iter());
        out.extend(self.gate2.findings.iter());
        out.extend(self.gate3.findings.iter());
        out.extend(self.gate4.findings.iter());
        out
    }

    /// Renders a deterministic human-readable text report.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let verdict = if self.certified() {
            "CERTIFIED (4/4 GATES PASS)"
        } else {
            "REJECTED"
        };
        let _ = writeln!(
            out,
            "NLOS Store Certification Pipeline v{STORE_CERTIFY_PIPELINE_VERSION}: {verdict}"
        );
        let _ = writeln!(
            out,
            "  package: {}@{} (package_id={}, application_id={})",
            self.package_id_label,
            self.package_version,
            hex_encode(self.package_id.as_bytes()),
            hex_encode(self.application_id.as_bytes())
        );
        let _ = writeln!(
            out,
            "  profile: {}, target_sabi_epoch: {}, manifest_digest: {}",
            self.surface_profile.as_str(),
            self.target_sabi_epoch,
            hex_encode(self.manifest_digest.as_bytes())
        );
        for gs in self.gate_summaries() {
            let _ = writeln!(
                out,
                "  [{}] {} (blockers={}, warnings={}) -- {}",
                gs.status.as_str(),
                gs.gate.as_str(),
                gs.blocker_count,
                gs.warning_count,
                gs.summary_line
            );
        }
        for finding in self.all_findings() {
            let _ = writeln!(
                out,
                "    [{}][{}][{}] {}: {}",
                finding.severity.as_str(),
                finding.gate.as_str(),
                finding.rule_id,
                finding.subject,
                finding.message
            );
            let _ = writeln!(out, "      remediation: {}", finding.remediation);
        }
        if let Some(ref receipt) = self.receipt {
            let _ = writeln!(
                out,
                "  receipt_id: {}",
                hex_encode(receipt.receipt_id.as_bytes())
            );
            let _ = writeln!(
                out,
                "  signature: {}",
                hex_encode(&receipt.ed25519_signature)
            );
        } else if let Some(ref reason) = self.receipt_rejection_reason {
            let _ = writeln!(out, "  receipt_withheld: {reason}");
        }
        out
    }

    /// Renders a deterministic RFC 8259 JSON report embedding all 4 gate
    /// summaries, findings, and the signed [`StoreCertificationReceipt`] (if issued).
    #[must_use]
    pub fn render_json(&self) -> String {
        let summaries = self.gate_summaries();
        let gates_json: Vec<String> = summaries
            .iter()
            .map(GateExecutionSummary::to_json)
            .collect();
        let findings_json: Vec<String> = self
            .all_findings()
            .into_iter()
            .map(CertifyFinding::to_json)
            .collect();
        let receipt_json = self
            .receipt
            .as_ref()
            .map_or_else(|| "null".to_owned(), StoreCertificationReceipt::to_json);
        let rejection_json = self
            .receipt_rejection_reason
            .as_deref()
            .map_or_else(|| "null".to_owned(), quote_json);
        let verdict = if self.certified() {
            "CERTIFIED"
        } else {
            "REJECTED"
        };
        format!(
            "{{\"pipeline_version\":{STORE_CERTIFY_PIPELINE_VERSION},\"verdict\":{},\"package_id_label\":{},\"package_id\":{},\"application_id\":{},\"package_version\":{},\"manifest_digest\":{},\"surface_profile\":{},\"target_sabi_epoch\":{},\"gate_summaries\":[{}],\"findings\":[{}],\"receipt\":{},\"receipt_rejection_reason\":{}}}",
            quote_json(verdict),
            quote_json(&self.package_id_label),
            quote_json(&hex_encode(self.package_id.as_bytes())),
            quote_json(&hex_encode(self.application_id.as_bytes())),
            quote_json(&self.package_version),
            quote_json(&hex_encode(self.manifest_digest.as_bytes())),
            quote_json(self.surface_profile.as_str()),
            self.target_sabi_epoch,
            gates_json.join(","),
            findings_json.join(","),
            receipt_json,
            rejection_json,
        )
    }
}

/// Executes the complete 4-gate store certification pipeline against `package_dir`
/// (and optional `upgrade_dir` for Gate 3) and signs a [`StoreCertificationReceipt`]
/// if and only if publisher ownership verifies and all 4 gates pass.
///
/// # Errors
///
/// Returns [`CertifyError`] if `package.manifest` is missing or syntactically
/// malformed, or if an unexpected host I/O error prevents gate execution.
pub fn run_certification_pipeline(
    package_dir: &Path,
    upgrade_dir: Option<&Path>,
    config: &CertifyPipelineConfig,
    trust_verifier: &dyn PublisherTrustVerifier,
    signer: &StoreCertifierSigner<'_>,
) -> Result<CertificationPipelineReport, CertifyError> {
    let baseline_pkg = LoadedCertifyPackage::load(package_dir)?;
    let upgrade_pkg = match upgrade_dir {
        Some(dir) => Some(LoadedCertifyPackage::load(dir)?),
        None => None,
    };

    let gate1 = run_gate1_protocol_audit(
        &baseline_pkg,
        config.surface_profile,
        config.host_sabi_epoch,
    )?;
    let gate2 = run_gate2_supply_chain_audit(&baseline_pkg, &config.supply_chain_policy)?;
    let gate3 = run_gate3_update_rollback_drill(&baseline_pkg, upgrade_pkg.as_ref())?;
    let gate4 = run_gate4_uninstall_audit(&baseline_pkg)?;

    let gate_summaries = [
        gate1.summary.clone(),
        gate2.summary.clone(),
        gate3.summary.clone(),
        gate4.summary.clone(),
    ];

    let target_sabi_epoch = baseline_pkg.base_manifest.target_sabi_epoch.major;
    let package_version = baseline_pkg.base_manifest.version_raw.clone();

    let issue_attempt = issue_store_certification_receipt(
        trust_verifier,
        signer,
        IssueStoreCertificationRequest {
            submission_proof: config.submission_proof.clone(),
            application_id: baseline_pkg.application_id,
            package_id: baseline_pkg.package_id,
            package_version: package_version.clone(),
            manifest_digest: baseline_pkg.manifest_digest,
            surface_profile: config.surface_profile,
            target_sabi_epoch,
            gate_summaries,
            issued_at_ms: config.issued_at_ms,
            expires_at_ms: config.expires_at_ms,
        },
    );

    let (receipt, receipt_rejection_reason) = match issue_attempt {
        Ok(rec) => (Some(rec), None),
        Err(err) => (None, Some(err.to_string())),
    };

    Ok(CertificationPipelineReport {
        package_id_label: baseline_pkg.package_id_label,
        package_id: baseline_pkg.package_id,
        application_id: baseline_pkg.application_id,
        package_version,
        manifest_digest: baseline_pkg.manifest_digest,
        surface_profile: config.surface_profile,
        target_sabi_epoch,
        gate1,
        gate2,
        gate3,
        gate4,
        receipt,
        receipt_rejection_reason,
    })
}
