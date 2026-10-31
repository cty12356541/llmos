//! Core types, error taxonomy, and extended package manifest loader for the
//! `nlos-certify` automated store certification pipeline (ADR-0027 §3).

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};

use nlos_artifact::ContentDigest;
use nlos_migrate::{AuditedPackageManifest, ManifestParseError, parse_package_manifest};
use nlos_publisher_trust::PublisherTrustCheckError;
use nlos_types::{ApplicationId, PackageId};
use sha2::{Digest, Sha256};

use crate::json::{JsonParseError, hex_decode_array, hex_encode, quote_json};

/// Canonical default host SABI epoch for Stage E store certification (`N = 3`,
/// supporting `N / N-1 / N-2` = `{3, 2, 1}`).
pub const DEFAULT_CERTIFY_HOST_SABI_EPOCH: u32 = 3;

/// Typed fail-closed error surface for the store certification pipeline.
#[derive(Debug)]
pub enum CertifyError {
    /// Filesystem I/O failure while reading a package or workspace directory.
    Io {
        /// Contextual operation label.
        context: String,
        /// Underlying OS error.
        source: std::io::Error,
    },
    /// Missing `package.manifest` in the target package directory.
    MissingPackageManifest(PathBuf),
    /// Syntax error in `package.manifest` or `certify.manifest`.
    ManifestSyntax {
        /// Manifest file name (`package.manifest` or `certify.manifest`).
        file: String,
        /// 1-based line number (`0` for missing required header directives).
        line: usize,
        /// Human-readable syntax failure reason.
        detail: String,
    },
    /// Malformed JSON document encountered during audit or receipt decoding.
    JsonParse {
        /// File or envelope label.
        context: String,
        /// Underlying RFC 8259 parser failure.
        source: JsonParseError,
    },
    /// Publisher identity or `ApplicationId` ownership verification failure.
    PublisherTrust(PublisherTrustCheckError),
    /// Store certification receipt refused because not all 4 gates passed.
    GatesNotAllPassed {
        /// List of gates that ended in [`GateStatus::Fail`].
        failed_gates: Vec<GateId>,
    },
    /// Cryptographic custody or authority execution failure.
    Authority(String),
    /// Signed `StoreCertificationReceipt` verification failure.
    ReceiptVerification(String),
}

impl fmt::Display for CertifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(f, "I/O error ({context}): {source}"),
            Self::MissingPackageManifest(path) => {
                write!(f, "missing package.manifest in {}", path.display())
            }
            Self::ManifestSyntax { file, line, detail } => {
                write!(f, "{file} syntax error at line {line}: {detail}")
            }
            Self::JsonParse { context, source } => {
                write!(f, "invalid JSON in {context}: {source}")
            }
            Self::PublisherTrust(err) => write!(f, "publisher trust verification failed: {err}"),
            Self::GatesNotAllPassed { failed_gates } => {
                let names: Vec<&str> = failed_gates.iter().map(|g| g.as_str()).collect();
                write!(
                    f,
                    "cannot issue StoreCertificationReceipt when gates failed: {}",
                    names.join(", ")
                )
            }
            Self::Authority(detail) => write!(f, "authority execution error: {detail}"),
            Self::ReceiptVerification(detail) => {
                write!(f, "store certification receipt rejected: {detail}")
            }
        }
    }
}

impl Error for CertifyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::JsonParse { source, .. } => Some(source),
            Self::PublisherTrust(err) => Some(err),
            _ => None,
        }
    }
}

impl From<PublisherTrustCheckError> for CertifyError {
    fn from(value: PublisherTrustCheckError) -> Self {
        Self::PublisherTrust(value)
    }
}

impl From<ManifestParseError> for CertifyError {
    fn from(value: ManifestParseError) -> Self {
        let (line, detail) = match value {
            ManifestParseError::SyntaxError { line, detail } => (line, detail),
            ManifestParseError::DuplicateScalar { line, key } => {
                (line, format!("duplicate scalar directive '{key}'"))
            }
            ManifestParseError::DuplicateComponentName { line, name } => {
                (line, format!("duplicate entry/component name '{name}'"))
            }
            other => (0, other.to_string()),
        };
        Self::ManifestSyntax {
            file: "package.manifest".to_owned(),
            line,
            detail,
        }
    }
}

/// Target surface profile declared for store certification (`[SABI-CONFORM-001]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SurfaceProfile {
    /// Headless / background / agent / platform service profile (`APPLICATION_PLATFORM`).
    /// Does not require a windowed UI surface and forbids desktop-exclusive SABI services.
    ApplicationPlatform,
    /// Full desktop shell profile (`DESKTOP`), permitting windowed `UI_SURFACE`,
    /// `WORKSPACE`, and `SESSION` desktop SABI services.
    Desktop,
}

impl SurfaceProfile {
    /// Parses a canonical (`APPLICATION_PLATFORM`, `DESKTOP`) or CLI
    /// (`application-platform`, `desktop`) profile token.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "APPLICATION_PLATFORM" | "application-platform" | "application_platform" => {
                Some(Self::ApplicationPlatform)
            }
            "DESKTOP" | "desktop" => Some(Self::Desktop),
            _ => None,
        }
    }

    /// Canonical wire string recorded in [`crate::StoreCertificationReceipt`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApplicationPlatform => "APPLICATION_PLATFORM",
            Self::Desktop => "DESKTOP",
        }
    }

    /// Lowercase CLI flag representation.
    #[must_use]
    pub const fn cli_name(self) -> &'static str {
        match self {
            Self::ApplicationPlatform => "application-platform",
            Self::Desktop => "desktop",
        }
    }
}

impl fmt::Display for SurfaceProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Canonical identifier for each of the four automated store certification gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GateId {
    /// Gate 1: Protocol, SABI v1 surface conformance, and `EffectDescriptor` audit.
    Gate1ProtocolAndEffects,
    /// Gate 2: Least privilege, immutable package purity, and supply-chain audit.
    Gate2LeastPrivilegeAndSupplyChain,
    /// Gate 3: Transactional update, health probe, and zero-drift rollback drill.
    Gate3TransactionalUpdateAndRollback,
    /// Gate 4: Zero-residue uninstall, capability revocation, and `UserDataContainer` preservation.
    Gate4ZeroResidueUninstall,
}

impl GateId {
    /// Ordered sequence of all four mandatory certification gates.
    pub const ALL: [Self; 4] = [
        Self::Gate1ProtocolAndEffects,
        Self::Gate2LeastPrivilegeAndSupplyChain,
        Self::Gate3TransactionalUpdateAndRollback,
        Self::Gate4ZeroResidueUninstall,
    ];

    /// Canonical wire token for JSON receipts and domain-separated digests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gate1ProtocolAndEffects => "GATE1_PROTOCOL_AND_EFFECTS",
            Self::Gate2LeastPrivilegeAndSupplyChain => "GATE2_LEAST_PRIVILEGE_AND_SUPPLY_CHAIN",
            Self::Gate3TransactionalUpdateAndRollback => "GATE3_TRANSACTIONAL_UPDATE_AND_ROLLBACK",
            Self::Gate4ZeroResidueUninstall => "GATE4_ZERO_RESIDUE_UNINSTALL",
        }
    }

    /// Parses a canonical [`GateId`] wire token.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "GATE1_PROTOCOL_AND_EFFECTS" => Some(Self::Gate1ProtocolAndEffects),
            "GATE2_LEAST_PRIVILEGE_AND_SUPPLY_CHAIN" => {
                Some(Self::Gate2LeastPrivilegeAndSupplyChain)
            }
            "GATE3_TRANSACTIONAL_UPDATE_AND_ROLLBACK" => {
                Some(Self::Gate3TransactionalUpdateAndRollback)
            }
            "GATE4_ZERO_RESIDUE_UNINSTALL" => Some(Self::Gate4ZeroResidueUninstall),
            _ => None,
        }
    }

    /// 1-based ordinal (`1..=4`).
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Gate1ProtocolAndEffects => 1,
            Self::Gate2LeastPrivilegeAndSupplyChain => 2,
            Self::Gate3TransactionalUpdateAndRollback => 3,
            Self::Gate4ZeroResidueUninstall => 4,
        }
    }
}

impl fmt::Display for GateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pass/Fail outcome of an individual store certification gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GateStatus {
    /// Zero `BLOCKER` findings observed; all invariants held.
    Pass,
    /// At least one `BLOCKER` finding or invariant violation occurred.
    Fail,
}

impl GateStatus {
    /// Canonical wire string (`"PASS"` or `"FAIL"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
        }
    }

    /// Parses `"PASS"` or `"FAIL"`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "PASS" => Some(Self::Pass),
            "FAIL" => Some(Self::Fail),
            _ => None,
        }
    }
}

impl fmt::Display for GateStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Diagnostic severity for a certification finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CertifySeverity {
    /// Blocks certification and fails the owning gate.
    Blocker,
    /// Non-blocking advisory diagnostic (e.g. deprecated SABI field usage).
    Warning,
    /// Informational audit note.
    Info,
}

impl CertifySeverity {
    /// Canonical wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocker => "BLOCKER",
            Self::Warning => "WARNING",
            Self::Info => "INFO",
        }
    }
}

impl fmt::Display for CertifySeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured finding emitted by one of the four certification gates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertifyFinding {
    /// Owning certification gate.
    pub gate: GateId,
    /// Normative architecture rule ID (e.g. `"SABI-CONFORM-001"`).
    pub rule_id: String,
    /// Severity level (`BLOCKER` fails the gate).
    pub severity: CertifySeverity,
    /// Relative file, component, or authority invariant subject.
    pub subject: String,
    /// Detailed violation or diagnostic message (secrets redacted).
    pub message: String,
    /// Actionable remediation step.
    pub remediation: String,
}

impl CertifyFinding {
    /// Serializes this finding into canonical JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\"gate\":{},\"rule_id\":{},\"severity\":{},\"subject\":{},\"message\":{},\"remediation\":{}}}",
            quote_json(self.gate.as_str()),
            quote_json(&self.rule_id),
            quote_json(self.severity.as_str()),
            quote_json(&self.subject),
            quote_json(&self.message),
            quote_json(&self.remediation),
        )
    }
}

/// Compact, cryptographically committed summary of one gate's execution,
/// embedded inside [`crate::StoreCertificationReceipt`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateExecutionSummary {
    /// Gate identifier.
    pub gate: GateId,
    /// Gate pass/fail status.
    pub status: GateStatus,
    /// Number of `BLOCKER` findings emitted by this gate.
    pub blocker_count: usize,
    /// Number of `WARNING` findings emitted by this gate.
    pub warning_count: usize,
    /// Domain-separated SHA-256 digest committing to the gate's findings and metrics.
    pub evidence_digest: [u8; 32],
    /// Concise human-readable summary sentence.
    pub summary_line: String,
}

impl GateExecutionSummary {
    /// Builds a deterministic [`GateExecutionSummary`] from a gate's findings and
    /// an auxiliary gate-specific metric transcript.
    #[must_use]
    pub fn from_findings(
        gate: GateId,
        findings: &[CertifyFinding],
        extra_commitment: &[u8],
        summary_line: String,
    ) -> Self {
        let blocker_count = findings
            .iter()
            .filter(|f| f.severity == CertifySeverity::Blocker)
            .count();
        let warning_count = findings
            .iter()
            .filter(|f| f.severity == CertifySeverity::Warning)
            .count();
        let status = if blocker_count == 0 {
            GateStatus::Pass
        } else {
            GateStatus::Fail
        };

        let mut hasher = Sha256::new();
        hasher.update(b"nlos/certify/gate-evidence/v1\0");
        hasher.update(gate.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(status.as_str().as_bytes());
        hasher.update((blocker_count as u64).to_be_bytes());
        hasher.update((warning_count as u64).to_be_bytes());
        for finding in findings {
            hasher.update(finding.rule_id.as_bytes());
            hasher.update([0]);
            hasher.update(finding.severity.as_str().as_bytes());
            hasher.update([0]);
            hasher.update(finding.subject.as_bytes());
            hasher.update([0]);
            hasher.update(finding.message.as_bytes());
            hasher.update([0]);
        }
        hasher.update(extra_commitment);
        let evidence_digest: [u8; 32] = hasher.finalize().into();

        Self {
            gate,
            status,
            blocker_count,
            warning_count,
            evidence_digest,
            summary_line,
        }
    }

    /// Serializes this summary into canonical JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\"gate\":{},\"status\":{},\"blocker_count\":{},\"warning_count\":{},\"evidence_digest\":{},\"summary_line\":{}}}",
            quote_json(self.gate.as_str()),
            quote_json(self.status.as_str()),
            self.blocker_count,
            self.warning_count,
            quote_json(&hex_encode(&self.evidence_digest)),
            quote_json(&self.summary_line),
        )
    }
}

/// Direction of a package's SABI service binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SabiServiceDirection {
    /// Package consumes/imports a SABI v1 service from the host or peer.
    Import,
    /// Package provides/exports a SABI v1 service endpoint.
    Export,
}

/// Parsed `sabi-service` / `sabi-import` / `sabi-export` declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredSabiServiceBinding {
    /// Import vs export direction.
    pub direction: SabiServiceDirection,
    /// Uppercase canonicalized or raw service token.
    pub service_name: String,
    /// Optional feature flags negotiated on this service.
    pub features: Vec<String>,
    /// Source 1-based line number.
    pub line_no: usize,
}

/// Parsed `idempotency-passthrough` directive for a component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredIdempotencyPolicy {
    /// Target component name.
    pub component: String,
    /// Declared passthrough mode (e.g. `"128-bit"`, `"true"`, `"false"`, `"drop"`).
    pub mode: String,
    /// Source 1-based line number.
    pub line_no: usize,
}

/// Parsed `file-association` directive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredFileAssociation {
    /// Content-type or extension (e.g. `"text/markdown"` or `".md"`).
    pub target: String,
    /// Association mode (`"suggest"`, `"force"`, `"hijack-default-handler"`, etc.).
    pub mode: String,
    /// Source 1-based line number.
    pub line_no: usize,
}

/// Extended store-certification directives extracted from `package.manifest`
/// and optional `certify.manifest`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CertifyExtendedDirectives {
    /// Optional `surface-profile = ...` declared inside the package.
    pub declared_surface_profile: Option<(String, usize)>,
    /// Imported and exported SABI v1 service bindings.
    pub sabi_services: Vec<DeclaredSabiServiceBinding>,
    /// Component-level `idempotency-passthrough` declarations.
    pub idempotency_policies: Vec<DeclaredIdempotencyPolicy>,
    /// Optional `network-policy = none | offline-only | egress-declared | unrestricted`.
    pub network_policy: Option<(String, usize)>,
    /// Explicitly declared external egress allowlist prefixes (`egress-allow = https://...`).
    pub egress_allowlist: Vec<(String, usize)>,
    /// Optional `chooser-scope = ...` declaration.
    pub chooser_scope: Option<(String, usize)>,
    /// Optional `filesystem-access = ...` declarations.
    pub filesystem_access: Vec<(String, usize)>,
    /// Optional `default-association = ...` declaration.
    pub default_association: Option<(String, usize)>,
    /// Per-content-type `file-association = <type> <mode>` declarations.
    pub file_associations: Vec<DeclaredFileAssociation>,
    /// Optional publisher identifier hint (`publisher-id = ...`).
    pub publisher_id_hint: Option<String>,
    /// Optional application identifier hint (`application-id = ...`).
    pub application_id_hint: Option<String>,
    /// Raw `package-id` value written in `package.manifest` (either 32-hex or symbolic).
    pub raw_package_id_label: Option<String>,
    /// Whether `package.manifest` itself contained inline certification-only directives
    /// or a symbolic `package-id` normalization.
    pub has_inline_manifest_extensions: bool,
}

/// Loaded and parsed package directory ready for Gate 1..Gate 4 execution.
#[derive(Clone, Debug)]
pub struct LoadedCertifyPackage {
    /// Root directory of the package on disk.
    pub package_dir: PathBuf,
    /// Parsed `nlos-migrate` base package manifest.
    pub base_manifest: AuditedPackageManifest,
    /// Base manifest text stripped of certification-only directives so
    /// `nlos_migrate::adjudicate_manifest_contracts` runs losslessly.
    pub filtered_base_manifest_text: String,
    /// Certification-specific directives from `package.manifest` + `certify.manifest`.
    pub directives: CertifyExtendedDirectives,
    /// Deterministic SHA-256 content commitment over the package manifests and payload files.
    pub manifest_digest: ContentDigest,
    /// Human-readable `package-id` label from `package.manifest`.
    pub package_id_label: String,
    /// Deterministic 128-bit `PackageId` derived from `base_manifest.package_id`.
    pub package_id: PackageId,
    /// Deterministic 128-bit `ApplicationId` derived from `application-id` or `package-id`.
    pub application_id: ApplicationId,
    /// Strictly monotonic `u64` version ordinal derived from `base_manifest.packed_version`.
    pub version_ordinal: u64,
}

impl LoadedCertifyPackage {
    /// Loads `<package_dir>/package.manifest` (and optional `<package_dir>/certify.manifest`),
    /// parses both base and extended certification directives, and computes the
    /// deterministic package content digest.
    ///
    /// # Errors
    ///
    /// Fails closed if `package.manifest` is missing, malformed, or contains
    /// unknown directives, or if payload files cannot be read for hashing.
    pub fn load(package_dir: impl AsRef<Path>) -> Result<Self, CertifyError> {
        let dir = package_dir.as_ref().to_path_buf();
        let manifest_path = dir.join("package.manifest");
        if !manifest_path.is_file() {
            return Err(CertifyError::MissingPackageManifest(dir));
        }
        let raw_manifest =
            fs::read_to_string(&manifest_path).map_err(|source| CertifyError::Io {
                context: format!("read {}", manifest_path.display()),
                source,
            })?;

        let mut directives = CertifyExtendedDirectives::default();
        let filtered_base_manifest_text =
            split_and_extract_directives(&raw_manifest, "package.manifest", true, &mut directives)?;

        let certify_sidecar_path = dir.join("certify.manifest");
        let sidecar_bytes = if certify_sidecar_path.is_file() {
            let sidecar_text =
                fs::read_to_string(&certify_sidecar_path).map_err(|source| CertifyError::Io {
                    context: format!("read {}", certify_sidecar_path.display()),
                    source,
                })?;
            let _ = split_and_extract_directives(
                &sidecar_text,
                "certify.manifest",
                false,
                &mut directives,
            )?;
            Some(sidecar_text.into_bytes())
        } else {
            None
        };

        let base_manifest = parse_package_manifest(&filtered_base_manifest_text)?;
        let package_id = base_manifest.package_id;
        let package_id_label = directives
            .raw_package_id_label
            .clone()
            .unwrap_or_else(|| hex_encode(package_id.as_bytes()));
        let application_id = match directives.application_id_hint.as_deref() {
            Some(hint) => derive_application_id_from_str(hint),
            None => derive_application_id_from_str(&package_id_label),
        };
        let version_ordinal = base_manifest.packed_version;
        let manifest_digest = compute_package_directory_digest(
            &dir,
            raw_manifest.as_bytes(),
            sidecar_bytes.as_deref(),
        )?;

        Ok(Self {
            package_dir: dir,
            base_manifest,
            filtered_base_manifest_text,
            directives,
            manifest_digest,
            package_id_label,
            package_id,
            application_id,
            version_ordinal,
        })
    }
}

/// Derives a deterministic 128-bit [`PackageId`] from either a 32-char hex string
/// or a symbolic package identifier (such as `"starter.assistant.app"`).
#[must_use]
pub fn derive_package_id_from_str(raw: &str) -> PackageId {
    if let Ok(bytes) = hex_decode_array::<16>(raw.trim()) {
        return PackageId::from_bytes(bytes);
    }
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/certify/package-id/v1\0");
    hasher.update(raw.trim().as_bytes());
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    PackageId::from_bytes(out)
}

/// Derives a deterministic 128-bit [`ApplicationId`] from either a 32-char hex
/// string or a symbolic application/package identifier.
#[must_use]
pub fn derive_application_id_from_str(raw: &str) -> ApplicationId {
    if let Ok(bytes) = hex_decode_array::<16>(raw.trim()) {
        return ApplicationId::from_bytes(bytes);
    }
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/certify/application-id/v1\0");
    hasher.update(raw.trim().as_bytes());
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    ApplicationId::from_bytes(out)
}

/// Converts a semver (`1.0.0`, `1.1.0-rc1`) or integer version string into the
/// canonical bit-packed `u64` version (`nlos_application::pack_package_version`)
/// expected by `ApplicationAuthority` compatibility windows.
#[must_use]
pub fn parse_version_ordinal(version: &str) -> u64 {
    let core = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("1.0.0");
    if !core.contains('.') {
        let minor = core.parse::<u32>().unwrap_or(0);
        return nlos_application::pack_package_version(1, minor, 0);
    }
    let parts: Vec<&str> = core.split('.').collect();
    let major = parts
        .first()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1);
    let minor = parts
        .get(1)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let patch = parts
        .get(2)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    nlos_application::pack_package_version(major, minor, patch)
}

const BASE_MIGRATE_KEYS: &[&str] = &[
    "package-id",
    "version",
    "target-sabi-epoch",
    "driver-abi-epoch",
    "migration-mode",
    "entry",
    "component",
    "effect",
    "sabi-use",
    "sabi-ext",
    "task",
];

fn split_and_extract_directives(
    raw_text: &str,
    file_label: &str,
    allow_base_keys: bool,
    out: &mut CertifyExtendedDirectives,
) -> Result<String, CertifyError> {
    let mut base_lines = String::with_capacity(raw_text.len());
    for (idx, raw_line) in raw_text.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            if allow_base_keys {
                let _ = writeln!(base_lines, "{raw_line}");
            }
            continue;
        }
        let Some((raw_key, raw_val)) = trimmed.split_once('=') else {
            return Err(CertifyError::ManifestSyntax {
                file: file_label.to_owned(),
                line: line_no,
                detail: format!("expected 'key = value' directive, got '{trimmed}'"),
            });
        };
        let key = raw_key.trim();
        let val = raw_val.trim();
        if val.is_empty() {
            return Err(CertifyError::ManifestSyntax {
                file: file_label.to_owned(),
                line: line_no,
                detail: format!("directive '{key}' has empty value"),
            });
        }

        if parse_extended_directive(key, val, line_no, file_label, out)? {
            if allow_base_keys {
                out.has_inline_manifest_extensions = true;
                // Preserve exact 1-based line numbering for base manifest diagnostics.
                let _ = writeln!(base_lines, "# certify-ext: {key}");
            }
            continue;
        }

        if allow_base_keys && BASE_MIGRATE_KEYS.contains(&key) {
            if key == "package-id" {
                out.raw_package_id_label = Some(val.to_owned());
                if hex_decode_array::<16>(val).is_err() {
                    out.has_inline_manifest_extensions = true;
                    let derived = derive_package_id_from_str(val);
                    let _ = writeln!(
                        base_lines,
                        "package-id = {}",
                        hex_encode(derived.as_bytes())
                    );
                    continue;
                }
            }
            let _ = writeln!(base_lines, "{raw_line}");
        } else {
            return Err(CertifyError::ManifestSyntax {
                file: file_label.to_owned(),
                line: line_no,
                detail: format!("unknown manifest directive '{key}'"),
            });
        }
    }
    Ok(base_lines)
}

#[allow(clippy::too_many_lines)]
fn parse_extended_directive(
    key: &str,
    val: &str,
    line_no: usize,
    file_label: &str,
    out: &mut CertifyExtendedDirectives,
) -> Result<bool, CertifyError> {
    match key {
        "surface-profile" => {
            out.declared_surface_profile = Some((val.to_owned(), line_no));
            Ok(true)
        }
        "sabi-service" => {
            let tokens: Vec<&str> = val.split_whitespace().collect();
            if tokens.len() < 2 {
                return Err(CertifyError::ManifestSyntax {
                    file: file_label.to_owned(),
                    line: line_no,
                    detail: "sabi-service requires '<import|export> <SERVICE> [feature=a,b]'"
                        .to_owned(),
                });
            }
            let direction = match tokens[0] {
                "import" => SabiServiceDirection::Import,
                "export" => SabiServiceDirection::Export,
                other => {
                    return Err(CertifyError::ManifestSyntax {
                        file: file_label.to_owned(),
                        line: line_no,
                        detail: format!(
                            "sabi-service direction must be 'import' or 'export', got '{other}'"
                        ),
                    });
                }
            };
            let features = parse_feature_tokens(&tokens[2..]);
            out.sabi_services.push(DeclaredSabiServiceBinding {
                direction,
                service_name: tokens[1].to_owned(),
                features,
                line_no,
            });
            Ok(true)
        }
        "sabi-import" | "sabi-export" => {
            let tokens: Vec<&str> = val.split_whitespace().collect();
            if tokens.is_empty() {
                return Err(CertifyError::ManifestSyntax {
                    file: file_label.to_owned(),
                    line: line_no,
                    detail: format!("{key} requires '<SERVICE> [feature=a,b]'"),
                });
            }
            let direction = if key == "sabi-import" {
                SabiServiceDirection::Import
            } else {
                SabiServiceDirection::Export
            };
            let features = parse_feature_tokens(&tokens[1..]);
            out.sabi_services.push(DeclaredSabiServiceBinding {
                direction,
                service_name: tokens[0].to_owned(),
                features,
                line_no,
            });
            Ok(true)
        }
        "idempotency-passthrough" => {
            let tokens: Vec<&str> = val.split_whitespace().collect();
            let (component, mode) = match tokens.as_slice() {
                [only_mode] => ("*".to_owned(), (*only_mode).to_owned()),
                [comp, mode_tok, ..] => ((*comp).to_owned(), (*mode_tok).to_owned()),
                [] => {
                    return Err(CertifyError::ManifestSyntax {
                        file: file_label.to_owned(),
                        line: line_no,
                        detail: "idempotency-passthrough requires '[<component>] <mode>'"
                            .to_owned(),
                    });
                }
            };
            out.idempotency_policies.push(DeclaredIdempotencyPolicy {
                component,
                mode,
                line_no,
            });
            Ok(true)
        }
        "network-policy" => {
            out.network_policy = Some((val.to_owned(), line_no));
            Ok(true)
        }
        "egress-allow" => {
            out.egress_allowlist.push((val.to_owned(), line_no));
            Ok(true)
        }
        "chooser-scope" => {
            out.chooser_scope = Some((val.to_owned(), line_no));
            Ok(true)
        }
        "filesystem-access" => {
            out.filesystem_access.push((val.to_owned(), line_no));
            Ok(true)
        }
        "default-association" => {
            out.default_association = Some((val.to_owned(), line_no));
            Ok(true)
        }
        "file-association" => {
            let tokens: Vec<&str> = val.split_whitespace().collect();
            if tokens.len() < 2 {
                return Err(CertifyError::ManifestSyntax {
                    file: file_label.to_owned(),
                    line: line_no,
                    detail: "file-association requires '<mime-or-ext> <suggest|force>'".to_owned(),
                });
            }
            out.file_associations.push(DeclaredFileAssociation {
                target: tokens[0].to_owned(),
                mode: tokens[1].to_owned(),
                line_no,
            });
            Ok(true)
        }
        "publisher-id" => {
            out.publisher_id_hint = Some(val.to_owned());
            Ok(true)
        }
        "application-id" => {
            out.application_id_hint = Some(val.to_owned());
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn parse_feature_tokens(tokens: &[&str]) -> Vec<String> {
    let mut features = Vec::new();
    for tok in tokens {
        let stripped = tok
            .strip_prefix("feature=")
            .or_else(|| tok.strip_prefix("features="))
            .unwrap_or(tok);
        for item in stripped.split(',') {
            let clean = item.trim();
            if !clean.is_empty() {
                features.push(clean.to_owned());
            }
        }
    }
    features
}

fn compute_package_directory_digest(
    root: &Path,
    raw_manifest: &[u8],
    sidecar: Option<&[u8]>,
) -> Result<ContentDigest, CertifyError> {
    let mut files = BTreeMap::new();
    collect_files_sorted(root, root, &mut files)?;

    let mut hasher = Sha256::new();
    hasher.update(b"nlos/certify/package-dir-digest/v1\0");
    hasher.update((raw_manifest.len() as u64).to_be_bytes());
    hasher.update(raw_manifest);
    if let Some(sc) = sidecar {
        hasher.update([1]);
        hasher.update((sc.len() as u64).to_be_bytes());
        hasher.update(sc);
    } else {
        hasher.update([0]);
    }
    hasher.update((files.len() as u64).to_be_bytes());
    for (rel, content_digest) in &files {
        hasher.update((rel.len() as u64).to_be_bytes());
        hasher.update(rel.as_bytes());
        hasher.update(content_digest);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Ok(ContentDigest::from_bytes(digest))
}

fn collect_files_sorted(
    root: &Path,
    current: &Path,
    out: &mut BTreeMap<String, [u8; 32]>,
) -> Result<(), CertifyError> {
    let entries = fs::read_dir(current).map_err(|source| CertifyError::Io {
        context: format!("read_dir {}", current.display()),
        source,
    })?;
    for entry_res in entries {
        let entry = entry_res.map_err(|source| CertifyError::Io {
            context: format!("dir_entry {}", current.display()),
            source,
        })?;
        let path = entry.path();
        let ft = entry.file_type().map_err(|source| CertifyError::Io {
            context: format!("file_type {}", path.display()),
            source,
        })?;
        if ft.is_dir() {
            collect_files_sorted(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path).map_err(|source| CertifyError::Io {
                context: format!("read {}", path.display()),
                source,
            })?;
            let file_hash: [u8; 32] = Sha256::digest(&bytes).into();
            out.insert(rel, file_hash);
        }
    }
    Ok(())
}
