//! Package conformance kit (W33-C, X-3 middle).
//!
//! A reusable rule set + checker that validates ANY `nlos/package-file/v1`
//! package — not just ones built by the `nlos-package` CLI — against the
//! format and verification invariants: structure/framing, manifest schema
//! completeness (including the `tasks` template segment shapes), the
//! signature chain against the embedded signer descriptor, payload/digest
//! consistency, and compatibility-window metadata sanity. Every violation
//! is a typed [`ConformanceFinding`] carrying a stable rule id
//! (`PKG-CONF-###`).
//!
//! Distinct from `nlos-artifact`'s `verify_package` pipeline: verification
//! is the kernel's fail-closed admission of ONE package against live
//! artifact heads and an identity authority; the conformance kit is an
//! offline, store-free, identity-free producer-side checker that collects
//! ALL violations into a report. Rule documentation:
//! `docs/developers/package-conformance.md`.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::model::ContentDigest;
use crate::package::{
    MAX_TASK_DEPENDENCIES_PER_TEMPLATE, MAX_TASK_TEMPLATES_PER_MANIFEST, package_manifest_message,
    package_manifest_with_tasks_message,
};
use crate::package_file::{self, PackageFile, PackageFileError};

/// One violated conformance rule plus a human-readable detail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConformanceFinding {
    /// The violated rule (stable id via [`ConformanceRule::id`]).
    pub rule: ConformanceRule,
    /// What was found, e.g. the offending entry name.
    pub detail: String,
}

impl ConformanceFinding {
    fn new(rule: ConformanceRule, detail: String) -> Self {
        Self { rule, detail }
    }
}

/// The conformance rule set. Ids are stable contract surface
/// (`PKG-CONF-001` style, three digits); never renumber.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ConformanceRule {
    /// File is not a `nlos/package-file/v1` package (bad magic).
    StructureMagic,
    /// Canonical framing broken: truncation, trailing bytes, unusable counts.
    StructureFraming,
    /// Entry name shape: bounded, UTF-8, NUL-free.
    StructureEntryName,
    /// Unknown entry-role or task-kind byte.
    StructureEnumByte,
    /// Manifest must declare at least one entry.
    ManifestEntries,
    /// Entry names must be unique within the manifest.
    ManifestNameUnique,
    /// Task template node keys must be unique within the segment.
    TaskNodeKeyUnique,
    /// No task template may depend on itself.
    TaskSelfDependency,
    /// Every dependency must reference a key declared in the same segment.
    TaskDanglingDependency,
    /// Task segment admission bound on template count.
    TaskBoundTemplates,
    /// Task segment admission bound on per-template dependency count.
    TaskBoundDependencies,
    /// Signature must verify against the embedded signer descriptor over
    /// the domain-separated manifest digest of the declared face.
    SignatureChain,
    /// Each entry's payload must hash to its declared digest.
    DigestPayload,
    /// Each entry artifact id must follow the documented derivation.
    DigestArtifactId,
    /// Signer key validity window must be non-empty.
    WindowEmpty,
    /// Signer key validity window must fit the durable identity ledger.
    WindowLedgerBounds,
    /// Version metadata must be a nonzero dotted-triple packing.
    VersionMetadata,
}

impl ConformanceRule {
    /// The full rule table, in id order.
    pub const ALL: [ConformanceRule; 17] = [
        ConformanceRule::StructureMagic,
        ConformanceRule::StructureFraming,
        ConformanceRule::StructureEntryName,
        ConformanceRule::StructureEnumByte,
        ConformanceRule::ManifestEntries,
        ConformanceRule::ManifestNameUnique,
        ConformanceRule::TaskNodeKeyUnique,
        ConformanceRule::TaskSelfDependency,
        ConformanceRule::TaskDanglingDependency,
        ConformanceRule::TaskBoundTemplates,
        ConformanceRule::TaskBoundDependencies,
        ConformanceRule::SignatureChain,
        ConformanceRule::DigestPayload,
        ConformanceRule::DigestArtifactId,
        ConformanceRule::WindowEmpty,
        ConformanceRule::WindowLedgerBounds,
        ConformanceRule::VersionMetadata,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::StructureMagic => "PKG-CONF-001",
            Self::StructureFraming => "PKG-CONF-002",
            Self::StructureEntryName => "PKG-CONF-003",
            Self::StructureEnumByte => "PKG-CONF-004",
            Self::ManifestEntries => "PKG-CONF-010",
            Self::ManifestNameUnique => "PKG-CONF-011",
            Self::TaskNodeKeyUnique => "PKG-CONF-012",
            Self::TaskSelfDependency => "PKG-CONF-013",
            Self::TaskDanglingDependency => "PKG-CONF-014",
            Self::TaskBoundTemplates => "PKG-CONF-015",
            Self::TaskBoundDependencies => "PKG-CONF-016",
            Self::SignatureChain => "PKG-CONF-020",
            Self::DigestPayload => "PKG-CONF-030",
            Self::DigestArtifactId => "PKG-CONF-031",
            Self::WindowEmpty => "PKG-CONF-040",
            Self::WindowLedgerBounds => "PKG-CONF-041",
            Self::VersionMetadata => "PKG-CONF-042",
        }
    }
}

/// The collected findings of one conformance run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConformanceReport {
    pub findings: Vec<ConformanceFinding>,
}

impl ConformanceReport {
    /// A package conforms iff no rule fired.
    #[must_use]
    pub fn is_conformant(&self) -> bool {
        self.findings.is_empty()
    }
}

/// The durable identity ledger stores millisecond timestamps as `SQLite`
/// INTEGERs, so a signer key window beyond `i64::MAX` cannot round-trip
/// a bootstrap — the bound `nlos-package keygen` already pins.
const LEDGER_MS_BOUND: u64 = 9_223_372_036_854_775_807;

/// Runs the full rule set over one package file's bytes.
///
/// Structure failures (magic/framing) stop the run — nothing deeper can
/// be trusted — while every post-decode rule (schema, tasks shapes,
/// signature chain, digest consistency, window sanity) is collected into
/// one report.
#[must_use]
pub fn check_package_file(bytes: &[u8]) -> ConformanceReport {
    let mut report = ConformanceReport::default();
    if bytes.len() < package_file::PACKAGE_FILE_MAGIC.len()
        || bytes[..package_file::PACKAGE_FILE_MAGIC.len()] != *package_file::PACKAGE_FILE_MAGIC
    {
        report.findings.push(ConformanceFinding::new(
            ConformanceRule::StructureMagic,
            "file does not start with the nlos/package-file/v1 magic".to_string(),
        ));
        return report;
    }
    let package = match package_file::decode_package_file(bytes) {
        Ok(package) => package,
        Err(error) => {
            report.findings.push(ConformanceFinding::new(
                decode_error_rule(&error),
                error.to_string(),
            ));
            return report;
        }
    };
    check_manifest(&package, &mut report);
    report
        .findings
        .extend(task_segment_findings(&package.tasks));
    check_signature_chain(&package, &mut report);
    check_digest_consistency(&package, &mut report);
    check_metadata_sanity(&package, &mut report);
    report
}

fn decode_error_rule(error: &PackageFileError) -> ConformanceRule {
    match error {
        PackageFileError::BadMagic => ConformanceRule::StructureMagic,
        PackageFileError::Truncated { .. }
        | PackageFileError::TrailingBytes { .. }
        | PackageFileError::CountOverflow { .. } => ConformanceRule::StructureFraming,
        PackageFileError::EntryNameLength { .. }
        | PackageFileError::EntryNameNotUtf8
        | PackageFileError::EntryNameNul => ConformanceRule::StructureEntryName,
        PackageFileError::UnknownEntryRoleByte(_) | PackageFileError::UnknownTaskKindByte(_) => {
            ConformanceRule::StructureEnumByte
        }
        PackageFileError::EmptyKeyValidityWindow => ConformanceRule::WindowEmpty,
    }
}

fn check_manifest(package: &PackageFile, report: &mut ConformanceReport) {
    if package.entries.is_empty() {
        report.findings.push(ConformanceFinding::new(
            ConformanceRule::ManifestEntries,
            "package must declare at least one entry".to_string(),
        ));
        return;
    }
    let mut seen = std::collections::HashSet::with_capacity(package.entries.len());
    for entry in &package.entries {
        if !seen.insert(entry.name.as_str()) {
            report.findings.push(ConformanceFinding::new(
                ConformanceRule::ManifestNameUnique,
                format!("duplicate entry name {:?}", entry.name),
            ));
        }
    }
}

/// Task-segment rules over a non-empty segment; mirrors
/// `validate_task_templates` with per-rule granularity (a test pins the
/// two authorities in agreement).
fn task_segment_findings(tasks: &[crate::package::PackageTaskTemplate]) -> Vec<ConformanceFinding> {
    let mut findings = Vec::new();
    if tasks.is_empty() {
        return findings;
    }
    if tasks.len() > MAX_TASK_TEMPLATES_PER_MANIFEST {
        findings.push(ConformanceFinding::new(
            ConformanceRule::TaskBoundTemplates,
            format!(
                "task segment declares {} templates, bound is {MAX_TASK_TEMPLATES_PER_MANIFEST}",
                tasks.len()
            ),
        ));
    }
    let mut seen = std::collections::HashSet::with_capacity(tasks.len());
    for template in tasks {
        if !seen.insert(template.node_key) {
            findings.push(ConformanceFinding::new(
                ConformanceRule::TaskNodeKeyUnique,
                format!("duplicate task node key {}", key_hex(&template.node_key)),
            ));
        }
        if template.dependency_keys.len() > MAX_TASK_DEPENDENCIES_PER_TEMPLATE {
            findings.push(ConformanceFinding::new(
                ConformanceRule::TaskBoundDependencies,
                format!(
                    "task node {} declares {} dependencies, bound is \
                     {MAX_TASK_DEPENDENCIES_PER_TEMPLATE}",
                    key_hex(&template.node_key),
                    template.dependency_keys.len()
                ),
            ));
        }
        for dependency in &template.dependency_keys {
            if *dependency == template.node_key {
                findings.push(ConformanceFinding::new(
                    ConformanceRule::TaskSelfDependency,
                    format!("task node {} depends on itself", key_hex(dependency)),
                ));
            }
        }
    }
    for template in tasks {
        for dependency in &template.dependency_keys {
            if !seen.contains(dependency) {
                findings.push(ConformanceFinding::new(
                    ConformanceRule::TaskDanglingDependency,
                    format!(
                        "task node {} references undeclared dependency {}",
                        key_hex(&template.node_key),
                        key_hex(dependency)
                    ),
                ));
            }
        }
    }
    findings
}

fn check_signature_chain(package: &PackageFile, report: &mut ConformanceReport) {
    let face = if package.tasks.is_empty() {
        "legacy"
    } else {
        "task-templated"
    };
    let message = if package.tasks.is_empty() {
        package_manifest_message(&package.manifest())
    } else {
        package_manifest_with_tasks_message(&package.manifest(), &package.tasks)
    };
    let finding = |detail: String| {
        Some(ConformanceFinding::new(
            ConformanceRule::SignatureChain,
            detail,
        ))
    };
    let failure = match VerifyingKey::from_bytes(&package.descriptor.public_key) {
        Err(_) => {
            finding("embedded signer public key is not a valid Ed25519 verifying key".to_string())
        }
        Ok(key) => match key.verify(&message, &Signature::from_bytes(&package.signature)) {
            Ok(()) => None,
            Err(_) => finding(format!(
                "signature does not verify against the embedded signer key over the {face} \
                 manifest digest"
            )),
        },
    };
    if let Some(finding) = failure {
        report.findings.push(finding);
    }
}

fn check_digest_consistency(package: &PackageFile, report: &mut ConformanceReport) {
    for entry in &package.entries {
        let actual = ContentDigest::of_bytes(&entry.payload).into_bytes();
        if actual != entry.digest {
            report.findings.push(ConformanceFinding::new(
                ConformanceRule::DigestPayload,
                format!(
                    "entry {:?}: payload digest does not match the declared digest",
                    entry.name
                ),
            ));
        }
        let derived =
            package_file::derive_artifact_id(package.package_id, package.version, &entry.name);
        if derived != entry.artifact_id {
            report.findings.push(ConformanceFinding::new(
                ConformanceRule::DigestArtifactId,
                format!(
                    "entry {:?}: artifact id does not follow the documented derivation",
                    entry.name
                ),
            ));
        }
    }
}

fn check_metadata_sanity(package: &PackageFile, report: &mut ConformanceReport) {
    if package.descriptor.valid_until_ms > LEDGER_MS_BOUND {
        report.findings.push(ConformanceFinding::new(
            ConformanceRule::WindowLedgerBounds,
            "signer key validity window exceeds the durable identity ledger bound (i64::MAX \
             ms); verification-time bootstrap could never persist it"
                .to_string(),
        ));
    }
    if package.version == 0 {
        report.findings.push(ConformanceFinding::new(
            ConformanceRule::VersionMetadata,
            "version 0 carries no dotted-triple release metadata; the update compatibility \
             windows (SameMajor/SameMinor) need a nonzero packed version"
                .to_string(),
        ));
    }
}

fn key_hex(key: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(32);
    for byte in key {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};
    use nlos_types::PackageId;

    use super::{ConformanceFinding, ConformanceRule, check_package_file, task_segment_findings};
    use crate::package::{
        MAX_TASK_TEMPLATES_PER_MANIFEST, PackageTaskKind, PackageTaskTemplate,
        validate_task_templates,
    };
    use crate::package_file::{PackageFile, PackageFileEntry, PackageFileError, SignerDescriptor};

    fn template(node_key: [u8; 16], dependency_keys: Vec<[u8; 16]>) -> PackageTaskTemplate {
        PackageTaskTemplate {
            node_key,
            kind: PackageTaskKind::AgentRole,
            binding_digest: [0x61; 32],
            dependency_keys,
            input_selectors_digest: [0x62; 32],
            output_contract_digest: [0x63; 32],
            policy_digest: [0x64; 32],
            resource_ceiling_digest: [0x65; 32],
        }
    }

    fn minimal_package() -> PackageFile {
        use crate::package::PackageEntryRole;
        use crate::package_file::derive_artifact_id;
        use sha2::{Digest as _, Sha256};

        let package_id = PackageId::from_bytes([0xab; 16]);
        let body = b"payload";
        let mut digest = [0_u8; 32];
        let mut hasher = Sha256::new();
        hasher.update(body);
        digest.copy_from_slice(&hasher.finalize());
        let name = "solo";
        let entry = PackageFileEntry {
            name: name.to_string(),
            role: PackageEntryRole::Data,
            artifact_id: derive_artifact_id(package_id, 5, name),
            digest,
            payload: body.to_vec(),
        };
        let manifest = crate::package::PackageManifest {
            package_id,
            version: 5,
            entries: [entry.manifest_entry()].into(),
        };
        let key = SigningKey::from_bytes(&[0x31; 32]);
        let signature = key
            .sign(&crate::package::package_manifest_message(&manifest))
            .to_bytes();
        PackageFile {
            descriptor: SignerDescriptor {
                public_key: key.verifying_key().to_bytes(),
                profile_digest: [0x51; 32],
                policy_digest: [0x52; 32],
                bootstrap_key: [0x53; 16],
                valid_from_ms: 0,
                valid_until_ms: super::LEDGER_MS_BOUND,
            },
            package_id,
            version: 5,
            entries: [entry].into(),
            tasks: Vec::new(),
            signature,
        }
    }

    #[test]
    fn rule_ids_are_stable_and_distinct() {
        let mut ids: Vec<&str> = ConformanceRule::ALL.iter().map(|rule| rule.id()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "rule ids must be pairwise distinct");
        for id in &ids {
            let digits = id.strip_prefix("PKG-CONF-").expect("id prefix");
            assert_eq!(digits.len(), 3, "id {id} must be three digits");
        }
    }

    #[test]
    fn empty_input_is_a_structure_magic_finding() {
        let report = check_package_file(&[]);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].rule, ConformanceRule::StructureMagic);
        assert!(!report.is_conformant());
    }

    #[test]
    fn minimal_package_is_conformant() {
        let bytes = minimal_package().encode();
        let report = check_package_file(&bytes);
        assert!(report.is_conformant(), "{:?}", report.findings);
    }

    #[test]
    fn decode_failures_map_to_their_structure_rules() {
        let bytes = minimal_package().encode();

        let mut truncated = bytes.clone();
        truncated.truncate(bytes.len() - 1);
        let report = check_package_file(&truncated);
        assert_eq!(report.findings[0].rule, ConformanceRule::StructureFraming);

        let mut trailing = bytes.clone();
        trailing.push(0);
        let report = check_package_file(&trailing);
        assert_eq!(report.findings[0].rule, ConformanceRule::StructureFraming);
        assert_eq!(
            report.findings[0].detail,
            PackageFileError::TrailingBytes { count: 1 }.to_string()
        );

        let mut reversed = minimal_package();
        reversed.descriptor.valid_from_ms = 9;
        reversed.descriptor.valid_until_ms = 1;
        let report = check_package_file(&reversed.encode());
        assert_eq!(report.findings[0].rule, ConformanceRule::WindowEmpty);
    }

    #[test]
    fn task_rules_agree_with_validate_task_templates() {
        let valid = vec![
            template([0x01; 16], Vec::new()),
            template([0x02; 16], vec![[0x01; 16]]),
        ];
        assert!(validate_task_templates(&valid).is_ok());
        assert!(task_segment_findings(&valid).is_empty());

        let violations: Vec<Vec<PackageTaskTemplate>> = vec![
            vec![
                template([0x01; 16], Vec::new()),
                template([0x01; 16], Vec::new()),
            ],
            vec![template([0x02; 16], vec![[0x02; 16]])],
            vec![template([0x03; 16], vec![[0x9f; 16]])],
        ];
        for segment in &violations {
            assert!(
                validate_task_templates(segment).is_err(),
                "authority must reject {segment:?}"
            );
            assert!(
                !task_segment_findings(segment).is_empty(),
                "kit must flag {segment:?}"
            );
        }
    }

    #[test]
    fn rule_015_template_admission_bound() {
        let tasks: Vec<PackageTaskTemplate> = (0..=MAX_TASK_TEMPLATES_PER_MANIFEST as u64)
            .map(|index| {
                let mut node_key = [0_u8; 16];
                node_key[8..].copy_from_slice(&index.to_be_bytes());
                template(node_key, Vec::new())
            })
            .collect();
        let findings = task_segment_findings(&tasks);
        let bound_findings: Vec<&ConformanceFinding> = findings
            .iter()
            .filter(|finding| finding.rule == ConformanceRule::TaskBoundTemplates)
            .collect();
        assert_eq!(bound_findings.len(), 1);
        assert_eq!(
            findings.len(),
            1,
            "unique dep-free templates must fire nothing else: {findings:?}"
        );
    }
}
