//! Pure-Rust external dependency license & lockfile supply-chain auditor
//! advancing `RISK-B-09` (`ADR-0027` §3.2).
//!
//! Audits Rust (`Cargo.toml` + `Cargo.lock`) and frontend/toolchain npm
//! (`package.json` + `package-lock.json`) dependency graphs completely offline
//! without invoking external binaries:
//! 1. Verifies lockfile presence, rejects floating wildcard (`"*"` / `"latest"`)
//!    and unpinned `git` dependencies lacking an immutable commit SHA, and
//!    requires cryptographic integrity checksums (`checksum` in `Cargo.lock`,
//!    `integrity` in `package-lock.json`) on every external registry package.
//! 2. Evaluates SPDX license expressions (`OR`, `AND`, `/`, parentheses,
//!    `WITH <exception>`) against an explicit permissive SPDX allowlist,
//!    blocking viral Copyleft (`GPL-*`, `AGPL-*`, `LGPL-*`), `UNLICENSED`, or
//!    missing license metadata with `RISK-B-09` `BLOCKER`.
//! 3. Enforces a caller-configurable [`AdvisoryDenyEntry`] blocklist for known
//!    vulnerable `(ecosystem, package, version)` coordinates.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::json::{JsonValue, parse_json, quote_json};
use crate::model::{CertifyError, CertifySeverity};

/// Default permissive SPDX license identifiers allowed in the NLOS TCB and
/// certified store packages (`RISK-B-09`).
pub const DEFAULT_ALLOWED_SPDX_LICENSES: &[&str] = &[
    "0BSD",
    "Apache-2.0",
    "BSD-1-Clause",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "BSL-1.0",
    "BlueOak-1.0.0",
    "CC0-1.0",
    "ISC",
    "MIT",
    "MIT-0",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "Unlicense",
    "Zlib",
];

/// Default SPDX `WITH` exception identifiers permitted on allowed base licenses.
pub const DEFAULT_ALLOWED_SPDX_EXCEPTIONS: &[&str] = &["LLVM-exception"];

/// Dependency ecosystem audited by [`audit_dependency_supply_chain`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DependencyEcosystem {
    /// Rust Cargo (`Cargo.toml` + `Cargo.lock`).
    Cargo,
    /// Node/npm (`package.json` + `package-lock.json`).
    Npm,
}

impl DependencyEcosystem {
    /// Canonical lowercase ecosystem string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm",
        }
    }
}

/// High-severity vulnerability / advisory blocklist entry (`RISK-B-09`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvisoryDenyEntry {
    /// Target package ecosystem (`Cargo` or `Npm`).
    pub ecosystem: DependencyEcosystem,
    /// Exact package name.
    pub package_name: String,
    /// Exact vulnerable version string.
    pub version: String,
    /// Advisory identifier (e.g. `"RUSTSEC-2026-0001"` or `"GHSA-xxxx-yyyy-zzzz"`).
    pub advisory_id: String,
    /// Concise vulnerability description.
    pub reason: String,
}

/// Caller-configurable policy for [`audit_dependency_supply_chain`].
#[derive(Clone, Debug)]
pub struct SupplyChainPolicy {
    /// Case-insensitive set of permitted SPDX license identifiers.
    pub allowed_spdx_licenses: BTreeSet<String>,
    /// Case-insensitive set of permitted SPDX `WITH` exception identifiers.
    pub allowed_spdx_exceptions: BTreeSet<String>,
    /// Explicit `(ecosystem, name, version)` vulnerability deny list.
    pub advisory_deny_list: Vec<AdvisoryDenyEntry>,
}

impl Default for SupplyChainPolicy {
    fn default() -> Self {
        Self {
            allowed_spdx_licenses: DEFAULT_ALLOWED_SPDX_LICENSES
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
            allowed_spdx_exceptions: DEFAULT_ALLOWED_SPDX_EXCEPTIONS
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
            advisory_deny_list: Vec::new(),
        }
    }
}

impl SupplyChainPolicy {
    /// Adds a high-severity advisory deny rule to this policy.
    #[must_use]
    pub fn with_advisory_deny(mut self, entry: AdvisoryDenyEntry) -> Self {
        self.advisory_deny_list.push(entry);
        self
    }
}

/// Classification of a supply-chain audit finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupplyChainViolationKind {
    /// No `Cargo.toml` or `package.json` found in audited root.
    NoManifestFound,
    /// Missing `Cargo.lock` or `package-lock.json` alongside manifest.
    LockfileMissing,
    /// Wildcard `"*"` / `"latest"` version or unpinned `git` dependency without commit SHA.
    FloatingDependency,
    /// Missing or malformed SHA-256 `checksum` / SRI `integrity` on registry package.
    MissingChecksum,
    /// Missing or empty `license` metadata.
    MissingLicense,
    /// SPDX license expression rejected by the permissive allowlist (e.g. `GPL-3.0`, `UNLICENSED`).
    DisallowedLicense,
    /// Package coordinate matched [`SupplyChainPolicy::advisory_deny_list`].
    AdvisoryDenied,
}

impl SupplyChainViolationKind {
    /// Canonical category string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoManifestFound => "NO_MANIFEST_FOUND",
            Self::LockfileMissing => "LOCKFILE_MISSING",
            Self::FloatingDependency => "FLOATING_DEPENDENCY",
            Self::MissingChecksum => "MISSING_CHECKSUM",
            Self::MissingLicense => "MISSING_LICENSE",
            Self::DisallowedLicense => "DISALLOWED_LICENSE",
            Self::AdvisoryDenied => "ADVISORY_DENIED",
        }
    }
}

/// Structured supply-chain finding (`RISK-B-09`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupplyChainFinding {
    /// Normative risk/rule identifier (`"RISK-B-09"`).
    pub rule_id: String,
    /// Diagnostic severity (`BLOCKER` fails the audit).
    pub severity: CertifySeverity,
    /// Dependency ecosystem (`cargo` or `npm`).
    pub ecosystem: DependencyEcosystem,
    /// Package name (or manifest relative path for workspace-level findings).
    pub package_name: String,
    /// Package version (or `"*"` / `"unpinned"`).
    pub package_version: String,
    /// Violation category.
    pub kind: SupplyChainViolationKind,
    /// Detailed explanation.
    pub message: String,
    /// Actionable remediation step.
    pub remediation: String,
}

impl SupplyChainFinding {
    /// Renders this finding as canonical JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            "{{\"rule_id\":{},\"severity\":{},\"ecosystem\":{},\"package_name\":{},\"package_version\":{},\"kind\":{},\"message\":{},\"remediation\":{}}}",
            quote_json(&self.rule_id),
            quote_json(self.severity.as_str()),
            quote_json(self.ecosystem.as_str()),
            quote_json(&self.package_name),
            quote_json(&self.package_version),
            quote_json(self.kind.as_str()),
            quote_json(&self.message),
            quote_json(&self.remediation),
        )
    }
}

/// Complete report returned by [`audit_dependency_supply_chain`].
#[derive(Clone, Debug)]
pub struct SupplyChainAuditReport {
    /// Sanitized display label for the audited root directory.
    pub root_label: String,
    /// Number of Cargo packages audited from `Cargo.lock` / `Cargo.toml`.
    pub audited_cargo_packages: usize,
    /// Number of npm packages audited from `package-lock.json` / `package.json`.
    pub audited_npm_packages: usize,
    /// Count of `BLOCKER` findings.
    pub blocker_count: usize,
    /// Count of `WARNING` findings.
    pub warning_count: usize,
    /// Ordered findings list.
    pub findings: Vec<SupplyChainFinding>,
}

impl SupplyChainAuditReport {
    /// Returns `true` iff zero `BLOCKER` findings were emitted.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.blocker_count == 0
    }

    /// Renders a deterministic human-readable text report.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let verdict = if self.passed() { "PASS" } else { "BLOCKED" };
        let _ = writeln!(
            out,
            "NLOS Supply-Chain & SPDX License Audit (RISK-B-09): {verdict}"
        );
        let _ = writeln!(out, "  root: {}", self.root_label);
        let _ = writeln!(
            out,
            "  audited_cargo_packages: {}",
            self.audited_cargo_packages
        );
        let _ = writeln!(out, "  audited_npm_packages: {}", self.audited_npm_packages);
        let _ = writeln!(
            out,
            "  blockers: {}, warnings: {}",
            self.blocker_count, self.warning_count
        );
        for finding in &self.findings {
            let _ = writeln!(
                out,
                "  [{}][{}][{}:{}@{}] {}",
                finding.severity.as_str(),
                finding.rule_id,
                finding.ecosystem.as_str(),
                finding.package_name,
                finding.package_version,
                finding.message
            );
            let _ = writeln!(out, "    remediation: {}", finding.remediation);
        }
        out
    }

    /// Renders a deterministic RFC 8259 JSON report.
    #[must_use]
    pub fn render_json(&self) -> String {
        let findings_json: Vec<String> = self
            .findings
            .iter()
            .map(SupplyChainFinding::to_json)
            .collect();
        let verdict = if self.passed() { "PASS" } else { "BLOCKED" };
        format!(
            "{{\"rule_id\":\"RISK-B-09\",\"verdict\":{},\"root\":{},\"audited_cargo_packages\":{},\"audited_npm_packages\":{},\"blocker_count\":{},\"warning_count\":{},\"findings\":[{}]}}",
            quote_json(verdict),
            quote_json(&self.root_label),
            self.audited_cargo_packages,
            self.audited_npm_packages,
            self.blocker_count,
            self.warning_count,
            findings_json.join(","),
        )
    }
}

/// Audits Cargo (`Cargo.toml` + `Cargo.lock`) and npm (`package.json` +
/// `package-lock.json`) dependency graphs rooted at `workspace_or_pkg_root`.
///
/// # Errors
///
/// Returns [`CertifyError`] if an existing manifest/lockfile cannot be read or
/// contains malformed JSON. Policy/license/checksum/pinning violations are
/// reported as `BLOCKER` [`SupplyChainFinding`]s in [`SupplyChainAuditReport`].
pub fn audit_dependency_supply_chain(
    workspace_or_pkg_root: &Path,
    policy: &SupplyChainPolicy,
) -> Result<SupplyChainAuditReport, CertifyError> {
    let root_label = workspace_or_pkg_root.file_name().map_or_else(
        || "workspace".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );

    let mut findings = Vec::new();
    let mut audited_cargo_packages = 0_usize;
    let mut audited_npm_packages = 0_usize;
    let mut saw_any_manifest = false;

    let cargo_toml_path = workspace_or_pkg_root.join("Cargo.toml");
    let mut is_cargo_workspace = false;
    if cargo_toml_path.is_file() {
        saw_any_manifest = true;
        let (count, ws) = audit_cargo_root(
            workspace_or_pkg_root,
            &cargo_toml_path,
            policy,
            &mut findings,
        )?;
        audited_cargo_packages += count;
        is_cargo_workspace = ws;
    }

    let root_pkg_json = workspace_or_pkg_root.join("package.json");
    if root_pkg_json.is_file() {
        saw_any_manifest = true;
        audited_npm_packages +=
            audit_npm_directory(workspace_or_pkg_root, "", policy, &mut findings)?;
    }

    // When auditing a workspace root, automatically cover standard desktop frontend sub-packages.
    if is_cargo_workspace {
        for sub in ["desktop", "apps/desktop"] {
            let sub_dir = workspace_or_pkg_root.join(sub);
            if sub_dir.join("package.json").is_file() {
                audited_npm_packages += audit_npm_directory(&sub_dir, sub, policy, &mut findings)?;
            }
        }
    }

    if !saw_any_manifest {
        findings.push(SupplyChainFinding {
            rule_id: "RISK-B-09".to_owned(),
            severity: CertifySeverity::Blocker,
            ecosystem: DependencyEcosystem::Cargo,
            package_name: root_label.clone(),
            package_version: "0.0.0".to_owned(),
            kind: SupplyChainViolationKind::NoManifestFound,
            message: "neither Cargo.toml nor package.json found in target root".to_owned(),
            remediation:
                "Point supply-chain audit at a valid Rust crate/workspace or npm package directory."
                    .to_owned(),
        });
    }

    let blocker_count = findings
        .iter()
        .filter(|f| f.severity == CertifySeverity::Blocker)
        .count();
    let warning_count = findings
        .iter()
        .filter(|f| f.severity == CertifySeverity::Warning)
        .count();

    Ok(SupplyChainAuditReport {
        root_label,
        audited_cargo_packages,
        audited_npm_packages,
        blocker_count,
        warning_count,
        findings,
    })
}

/// Evaluates whether an SPDX license expression satisfies `policy`.
///
/// Supports `OR`, `AND`, legacy Cargo `/` (treated as `OR`), `<license> WITH <exception>`,
/// and arbitrarily nested parentheses `(...)`. Fails closed (`false`) on empty strings,
/// `UNLICENSED` / `NONE`, unknown operators, unbalanced parentheses, or disallowed licenses.
#[must_use]
pub fn evaluate_spdx_expression(expr: &str, policy: &SupplyChainPolicy) -> bool {
    let tokens = tokenize_spdx(expr);
    if tokens.is_empty() {
        return false;
    }
    let mut parser = SpdxParser {
        tokens: &tokens,
        pos: 0,
        policy,
    };
    let Some(result) = parser.parse_or() else {
        return false;
    };
    parser.pos == tokens.len() && result
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SpdxToken {
    LParen,
    RParen,
    Or,
    And,
    With,
    Ident(String),
}

fn tokenize_spdx(expr: &str) -> Vec<SpdxToken> {
    let mut out = Vec::new();
    let mut current = String::new();
    let flush = |buf: &mut String, tokens: &mut Vec<SpdxToken>| {
        if buf.is_empty() {
            return;
        }
        let word = std::mem::take(buf);
        match word.as_str() {
            "OR" => tokens.push(SpdxToken::Or),
            "AND" => tokens.push(SpdxToken::And),
            "WITH" => tokens.push(SpdxToken::With),
            _ => tokens.push(SpdxToken::Ident(word)),
        }
    };

    for ch in expr.chars() {
        match ch {
            '(' => {
                flush(&mut current, &mut out);
                out.push(SpdxToken::LParen);
            }
            ')' => {
                flush(&mut current, &mut out);
                out.push(SpdxToken::RParen);
            }
            '/' => {
                flush(&mut current, &mut out);
                out.push(SpdxToken::Or);
            }
            c if c.is_whitespace() => {
                flush(&mut current, &mut out);
            }
            c => current.push(c),
        }
    }
    flush(&mut current, &mut out);
    out
}

struct SpdxParser<'a> {
    tokens: &'a [SpdxToken],
    pos: usize,
    policy: &'a SupplyChainPolicy,
}

impl SpdxParser<'_> {
    fn parse_or(&mut self) -> Option<bool> {
        let mut lhs = self.parse_and()?;
        while matches!(self.tokens.get(self.pos), Some(SpdxToken::Or)) {
            self.pos += 1;
            let rhs = self.parse_and()?;
            lhs = lhs || rhs;
        }
        Some(lhs)
    }

    fn parse_and(&mut self) -> Option<bool> {
        let mut lhs = self.parse_primary()?;
        while matches!(self.tokens.get(self.pos), Some(SpdxToken::And)) {
            self.pos += 1;
            let rhs = self.parse_primary()?;
            lhs = lhs && rhs;
        }
        Some(lhs)
    }

    fn parse_primary(&mut self) -> Option<bool> {
        match self.tokens.get(self.pos)? {
            SpdxToken::LParen => {
                self.pos += 1;
                let inner = self.parse_or()?;
                if !matches!(self.tokens.get(self.pos), Some(SpdxToken::RParen)) {
                    return None;
                }
                self.pos += 1;
                Some(inner)
            }
            SpdxToken::Ident(lic) => {
                let lic_lower = lic.to_ascii_lowercase();
                self.pos += 1;
                let base_allowed = !matches!(
                    lic_lower.as_str(),
                    "unlicensed" | "none" | "proprietary" | "unknown"
                ) && self.policy.allowed_spdx_licenses.contains(&lic_lower);

                if matches!(self.tokens.get(self.pos), Some(SpdxToken::With)) {
                    self.pos += 1;
                    let Some(SpdxToken::Ident(exc)) = self.tokens.get(self.pos) else {
                        return None;
                    };
                    let exc_lower = exc.to_ascii_lowercase();
                    self.pos += 1;
                    let exc_allowed = self.policy.allowed_spdx_exceptions.contains(&exc_lower);
                    Some(base_allowed && exc_allowed)
                } else {
                    Some(base_allowed)
                }
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Cargo (`Cargo.toml` + `Cargo.lock`) supply-chain inspection
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ParsedCargoTomlInfo {
    is_workspace: bool,
    workspace_members: Vec<String>,
    workspace_license: Option<String>,
    package_name: Option<String>,
    package_version: Option<String>,
    package_license: Option<String>,
    package_license_uses_workspace: bool,
    dep_specs: Vec<CargoDepSpec>,
}

struct CargoDepSpec {
    name: String,
    version_req: Option<String>,
    git_url: Option<String>,
    git_rev: Option<String>,
    line_no: usize,
}

#[derive(Default)]
struct LockPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    inline_license: Option<String>,
}

#[allow(clippy::too_many_lines)]
fn audit_cargo_root(
    root: &Path,
    cargo_toml_path: &Path,
    policy: &SupplyChainPolicy,
    findings: &mut Vec<SupplyChainFinding>,
) -> Result<(usize, bool), CertifyError> {
    let toml_text = fs::read_to_string(cargo_toml_path).map_err(|source| CertifyError::Io {
        context: format!("read {}", cargo_toml_path.display()),
        source,
    })?;
    let root_info = parse_cargo_toml(&toml_text);

    audit_cargo_toml_deps("Cargo.toml", &root_info, findings);

    let mut local_member_names = BTreeSet::new();
    if let Some(ref pkg_name) = root_info.package_name {
        local_member_names.insert(pkg_name.clone());
        let pkg_ver = root_info
            .package_version
            .clone()
            .unwrap_or_else(|| "0.1.0".to_owned());
        let resolved_lic = if root_info.package_license_uses_workspace {
            root_info.workspace_license.clone()
        } else {
            root_info.package_license.clone()
        };
        check_package_license(
            DependencyEcosystem::Cargo,
            pkg_name,
            &pkg_ver,
            resolved_lic.as_deref(),
            policy,
            findings,
        );
    } else if root_info.is_workspace
        && let Some(ref ws_lic) = root_info.workspace_license
        && !evaluate_spdx_expression(ws_lic, policy)
    {
        findings.push(SupplyChainFinding {
            rule_id: "RISK-B-09".to_owned(),
            severity: CertifySeverity::Blocker,
            ecosystem: DependencyEcosystem::Cargo,
            package_name: "workspace.package".to_owned(),
            package_version: "0.0.0".to_owned(),
            kind: SupplyChainViolationKind::DisallowedLicense,
            message: format!("[workspace.package] declares non-compliant SPDX license '{ws_lic}'"),
            remediation: "Use an approved permissive SPDX license (e.g. Apache-2.0 or MIT)."
                .to_owned(),
        });
    }

    for member_rel in &root_info.workspace_members {
        let member_toml = root.join(member_rel).join("Cargo.toml");
        if !member_toml.is_file() {
            continue;
        }
        let m_text = fs::read_to_string(&member_toml).map_err(|source| CertifyError::Io {
            context: format!("read {}", member_toml.display()),
            source,
        })?;
        let m_info = parse_cargo_toml(&m_text);
        let rel_label = format!("{member_rel}/Cargo.toml");
        audit_cargo_toml_deps(&rel_label, &m_info, findings);
        if let Some(ref m_name) = m_info.package_name {
            local_member_names.insert(m_name.clone());
            let m_ver = m_info
                .package_version
                .or_else(|| root_info.package_version.clone())
                .unwrap_or_else(|| "0.1.0".to_owned());
            let resolved_lic = if m_info.package_license_uses_workspace {
                root_info.workspace_license.clone()
            } else {
                m_info.package_license
            };
            check_package_license(
                DependencyEcosystem::Cargo,
                m_name,
                &m_ver,
                resolved_lic.as_deref(),
                policy,
                findings,
            );
        }
    }

    let lock_path = root.join("Cargo.lock");
    if !lock_path.is_file() {
        findings.push(SupplyChainFinding {
            rule_id: "RISK-B-09".to_owned(),
            severity: CertifySeverity::Blocker,
            ecosystem: DependencyEcosystem::Cargo,
            package_name: "Cargo.lock".to_owned(),
            package_version: "missing".to_owned(),
            kind: SupplyChainViolationKind::LockfileMissing,
            message: "Cargo.lock is missing alongside Cargo.toml".to_owned(),
            remediation:
                "Commit a deterministic Cargo.lock with pinned versions and SHA-256 checksums."
                    .to_owned(),
        });
        return Ok((local_member_names.len(), root_info.is_workspace));
    }

    let lock_text = fs::read_to_string(&lock_path).map_err(|source| CertifyError::Io {
        context: format!("read {}", lock_path.display()),
        source,
    })?;
    let lock_packages = parse_cargo_lock(&lock_text);
    let registry_roots = discover_cargo_registry_src_dirs();

    for pkg in &lock_packages {
        check_advisory_deny(
            DependencyEcosystem::Cargo,
            &pkg.name,
            &pkg.version,
            policy,
            findings,
        );

        if let Some(ref src) = pkg.source {
            if src.starts_with("registry+") {
                let valid_cksum = pkg.checksum.as_deref().is_some_and(is_valid_sha256_hex);
                if !valid_cksum {
                    findings.push(SupplyChainFinding {
                        rule_id: "RISK-B-09".to_owned(),
                        severity: CertifySeverity::Blocker,
                        ecosystem: DependencyEcosystem::Cargo,
                        package_name: pkg.name.clone(),
                        package_version: pkg.version.clone(),
                        kind: SupplyChainViolationKind::MissingChecksum,
                        message: format!(
                            "registry crate '{}@{}' in Cargo.lock is missing a valid 64-hex SHA-256 checksum",
                            pkg.name, pkg.version
                        ),
                        remediation:
                            "Regenerate Cargo.lock from a verified registry index with SHA-256 package checksums."
                                .to_owned(),
                    });
                }
            } else if src.starts_with("git+") && !has_pinned_git_commit_fragment(src) {
                findings.push(SupplyChainFinding {
                    rule_id: "RISK-B-09".to_owned(),
                    severity: CertifySeverity::Blocker,
                    ecosystem: DependencyEcosystem::Cargo,
                    package_name: pkg.name.clone(),
                    package_version: pkg.version.clone(),
                    kind: SupplyChainViolationKind::FloatingDependency,
                    message: format!(
                        "git dependency '{}@{}' in Cargo.lock lacks an immutable commit SHA fragment ('{src}')",
                        pkg.name, pkg.version
                    ),
                    remediation: "Pin git dependencies to an exact 40-char commit rev.".to_owned(),
                });
            }

            if let Some(ref inline_lic) = pkg.inline_license {
                check_package_license(
                    DependencyEcosystem::Cargo,
                    &pkg.name,
                    &pkg.version,
                    if inline_lic.trim().is_empty() {
                        None
                    } else {
                        Some(inline_lic.as_str())
                    },
                    policy,
                    findings,
                );
            } else if let Some(ext_toml_path) =
                locate_external_crate_toml(root, &registry_roots, &pkg.name, &pkg.version)
            {
                let ext_text =
                    fs::read_to_string(&ext_toml_path).map_err(|source| CertifyError::Io {
                        context: format!("read {}", ext_toml_path.display()),
                        source,
                    })?;
                let ext_info = parse_cargo_toml(&ext_text);
                check_package_license(
                    DependencyEcosystem::Cargo,
                    &pkg.name,
                    &pkg.version,
                    ext_info.package_license.as_deref(),
                    policy,
                    findings,
                );
            }
        } else if let Some(ref inline_lic) = pkg.inline_license {
            check_package_license(
                DependencyEcosystem::Cargo,
                &pkg.name,
                &pkg.version,
                if inline_lic.trim().is_empty() {
                    None
                } else {
                    Some(inline_lic.as_str())
                },
                policy,
                findings,
            );
        }
    }

    Ok((lock_packages.len(), root_info.is_workspace))
}

fn audit_cargo_toml_deps(
    manifest_label: &str,
    info: &ParsedCargoTomlInfo,
    findings: &mut Vec<SupplyChainFinding>,
) {
    for dep in &info.dep_specs {
        if let Some(ref req) = dep.version_req {
            let trimmed = req.trim();
            if trimmed == "*" || trimmed.contains('*') || trimmed.eq_ignore_ascii_case("latest") {
                findings.push(SupplyChainFinding {
                    rule_id: "RISK-B-09".to_owned(),
                    severity: CertifySeverity::Blocker,
                    ecosystem: DependencyEcosystem::Cargo,
                    package_name: dep.name.clone(),
                    package_version: trimmed.to_owned(),
                    kind: SupplyChainViolationKind::FloatingDependency,
                    message: format!(
                        "{manifest_label}:{} declares floating wildcard dependency '{} = \"{trimmed}\"'",
                        dep.line_no, dep.name
                    ),
                    remediation:
                        "Pin dependency version to an explicit SemVer requirement and lock it in Cargo.lock."
                            .to_owned(),
                });
            }
        }
        if let Some(ref git_url) = dep.git_url {
            let pinned = dep.git_rev.as_deref().is_some_and(is_valid_git_commit_rev);
            if !pinned {
                findings.push(SupplyChainFinding {
                    rule_id: "RISK-B-09".to_owned(),
                    severity: CertifySeverity::Blocker,
                    ecosystem: DependencyEcosystem::Cargo,
                    package_name: dep.name.clone(),
                    package_version: git_url.clone(),
                    kind: SupplyChainViolationKind::FloatingDependency,
                    message: format!(
                        "{manifest_label}:{} declares git dependency '{}' ('{git_url}') without an explicit commit 'rev'",
                        dep.line_no, dep.name
                    ),
                    remediation:
                        "Add 'rev = \"<40-hex-commit-sha>\"' or publish the crate to an audited registry."
                            .to_owned(),
                });
            }
        }
    }
}

fn parse_cargo_toml(text: &str) -> ParsedCargoTomlInfo {
    let mut info = ParsedCargoTomlInfo::default();
    let mut section = String::new();
    let mut collecting_members = false;

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if collecting_members {
            for item in extract_quoted_strings(line) {
                info.workspace_members.push(item);
            }
            if line.contains(']') {
                collecting_members = false;
            }
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') && !line.starts_with("[[") {
            line[1..line.len() - 1].trim().clone_into(&mut section);
            if section == "workspace" || section.starts_with("workspace.") {
                info.is_workspace = true;
            }
            continue;
        }
        if line.starts_with("[[") {
            section.clear();
            continue;
        }

        let Some((raw_k, raw_v)) = line.split_once('=') else {
            continue;
        };
        let key = raw_k.trim();
        let val = raw_v.trim();

        if section == "workspace" && key == "members" {
            for item in extract_quoted_strings(val) {
                info.workspace_members.push(item);
            }
            if !val.contains(']') {
                collecting_members = true;
            }
            continue;
        }

        if section == "workspace.package" && key == "license" {
            info.workspace_license = unquote_toml_string(val);
            continue;
        }

        if section == "package" {
            match key {
                "name" => info.package_name = unquote_toml_string(val),
                "version" => info.package_version = unquote_toml_string(val),
                "license" => info.package_license = unquote_toml_string(val),
                "license.workspace" if val == "true" => {
                    info.package_license_uses_workspace = true;
                }
                _ => {}
            }
            continue;
        }

        if is_cargo_dependency_section(&section) {
            if let Some(spec) = parse_cargo_dep_line(key, val, line_no) {
                info.dep_specs.push(spec);
            }
        } else if let Some(dep_name) = section_specific_dependency_name(&section) {
            // Handles `[dependencies.foo]` table style.
            update_or_insert_table_dep(&mut info.dep_specs, dep_name, key, val, line_no);
        }
    }
    info
}

fn is_cargo_dependency_section(section: &str) -> bool {
    matches!(
        section,
        "dependencies" | "dev-dependencies" | "build-dependencies" | "workspace.dependencies"
    ) || (section.starts_with("target.")
        && (section.ends_with(".dependencies")
            || section.ends_with(".dev-dependencies")
            || section.ends_with(".build-dependencies")))
}

fn section_specific_dependency_name(section: &str) -> Option<&str> {
    for prefix in [
        "dependencies.",
        "dev-dependencies.",
        "build-dependencies.",
        "workspace.dependencies.",
    ] {
        if let Some(rest) = section.strip_prefix(prefix)
            && !rest.contains('.')
        {
            return Some(rest);
        }
    }
    None
}

fn update_or_insert_table_dep(
    specs: &mut Vec<CargoDepSpec>,
    dep_name: &str,
    key: &str,
    val: &str,
    line_no: usize,
) {
    let entry = if let Some(existing) = specs.iter_mut().find(|d| d.name == dep_name) {
        existing
    } else {
        specs.push(CargoDepSpec {
            name: dep_name.to_owned(),
            version_req: None,
            git_url: None,
            git_rev: None,
            line_no,
        });
        let last_idx = specs.len() - 1;
        &mut specs[last_idx]
    };
    match key {
        "version" => entry.version_req = unquote_toml_string(val),
        "git" => entry.git_url = unquote_toml_string(val),
        "rev" => entry.git_rev = unquote_toml_string(val),
        _ => {}
    }
}

fn parse_cargo_dep_line(key: &str, val: &str, line_no: usize) -> Option<CargoDepSpec> {
    let name = key.trim_matches('"').to_owned();
    if let Some(ver) = unquote_toml_string(val) {
        return Some(CargoDepSpec {
            name,
            version_req: Some(ver),
            git_url: None,
            git_rev: None,
            line_no,
        });
    }
    if val.starts_with('{') {
        let inner = val.trim_start_matches('{').trim_end_matches('}');
        let mut version_req = None;
        let mut git_url = None;
        let mut git_rev = None;
        for part in split_inline_toml_fields(inner) {
            if let Some((fk, fv)) = part.split_once('=') {
                match fk.trim() {
                    "version" => version_req = unquote_toml_string(fv.trim()),
                    "git" => git_url = unquote_toml_string(fv.trim()),
                    "rev" => git_rev = unquote_toml_string(fv.trim()),
                    _ => {}
                }
            }
        }
        return Some(CargoDepSpec {
            name,
            version_req,
            git_url,
            git_rev,
            line_no,
        });
    }
    None
}

fn split_inline_toml_fields(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut bracket_depth = 0_usize;
    let bytes = inner.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'"' if idx == 0 || bytes[idx - 1] != b'\\' => in_quotes = !in_quotes,
            b'[' if !in_quotes => bracket_depth += 1,
            b']' if !in_quotes => bracket_depth = bracket_depth.saturating_sub(1),
            b',' if !in_quotes && bracket_depth == 0 => {
                parts.push(inner[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
        idx += 1;
    }
    let tail = inner[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

fn parse_cargo_lock(text: &str) -> Vec<LockPackage> {
    let mut packages = Vec::new();
    let mut current: Option<LockPackage> = None;
    let mut in_package = false;

    for raw_line in text.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[package]]" {
            if let Some(pkg) = current.take()
                && !pkg.name.is_empty()
            {
                packages.push(pkg);
            }
            current = Some(LockPackage::default());
            in_package = true;
            continue;
        }
        if line.starts_with('[') {
            if let Some(pkg) = current.take()
                && !pkg.name.is_empty()
            {
                packages.push(pkg);
            }
            in_package = false;
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((raw_k, raw_v)) = line.split_once('=') else {
            continue;
        };
        let Some(ref mut pkg) = current else {
            continue;
        };
        let key = raw_k.trim();
        let val = raw_v.trim();
        match key {
            "name" => {
                if let Some(s) = unquote_toml_string(val) {
                    pkg.name = s;
                }
            }
            "version" => {
                if let Some(s) = unquote_toml_string(val) {
                    pkg.version = s;
                }
            }
            "source" => pkg.source = unquote_toml_string(val),
            "checksum" => pkg.checksum = unquote_toml_string(val),
            "license" => pkg.inline_license = unquote_toml_string(val),
            _ => {}
        }
    }
    if let Some(pkg) = current
        && !pkg.name.is_empty()
    {
        packages.push(pkg);
    }
    packages
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let bytes = line.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'"' if idx == 0 || bytes[idx - 1] != b'\\' => in_quotes = !in_quotes,
            b'#' if !in_quotes => return &line[..idx],
            _ => {}
        }
        idx += 1;
    }
    line
}

fn unquote_toml_string(val: &str) -> Option<String> {
    let trimmed = val.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        Some(trimmed[1..trimmed.len() - 1].to_owned())
    } else {
        None
    }
}

fn extract_quoted_strings(line: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut in_quotes = false;
    let mut current = String::new();
    for ch in line.chars() {
        if ch == '"' {
            if in_quotes {
                items.push(std::mem::take(&mut current));
                in_quotes = false;
            } else {
                in_quotes = true;
            }
        } else if in_quotes {
            current.push(ch);
        }
    }
    items
}

fn discover_cargo_registry_src_dirs() -> Vec<PathBuf> {
    let cargo_home = env::var_os("CARGO_HOME").map_or_else(
        || {
            env::var_os("USERPROFILE")
                .or_else(|| env::var_os("HOME"))
                .map(|home| PathBuf::from(home).join(".cargo"))
        },
        |p| Some(PathBuf::from(p)),
    );
    let mut dirs = Vec::new();
    if let Some(home) = cargo_home {
        let reg_src = home.join("registry").join("src");
        if let Ok(entries) = fs::read_dir(reg_src) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                }
            }
        }
    }
    dirs
}

fn locate_external_crate_toml(
    root: &Path,
    registry_src_dirs: &[PathBuf],
    name: &str,
    version: &str,
) -> Option<PathBuf> {
    let folder = format!("{name}-{version}");
    let vendor_versioned = root.join("vendor").join(&folder).join("Cargo.toml");
    if vendor_versioned.is_file() {
        return Some(vendor_versioned);
    }
    let vendor_plain = root.join("vendor").join(name).join("Cargo.toml");
    if vendor_plain.is_file() {
        return Some(vendor_plain);
    }
    for idx_dir in registry_src_dirs {
        let candidate = idx_dir.join(&folder).join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// npm (`package.json` + `package-lock.json`) supply-chain inspection
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn audit_npm_directory(
    dir: &Path,
    rel_prefix: &str,
    policy: &SupplyChainPolicy,
    findings: &mut Vec<SupplyChainFinding>,
) -> Result<usize, CertifyError> {
    let pkg_json_path = dir.join("package.json");
    let pkg_label = if rel_prefix.is_empty() {
        "package.json".to_owned()
    } else {
        format!("{rel_prefix}/package.json")
    };
    let pkg_text = fs::read_to_string(&pkg_json_path).map_err(|source| CertifyError::Io {
        context: format!("read {}", pkg_json_path.display()),
        source,
    })?;
    let pkg_val = parse_json(&pkg_text).map_err(|source| CertifyError::JsonParse {
        context: pkg_label.clone(),
        source,
    })?;
    let Some(pkg_obj) = pkg_val.as_object() else {
        return Err(CertifyError::Authority(format!(
            "{pkg_label} top-level value must be a JSON object"
        )));
    };

    let root_name = pkg_obj
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or(&pkg_label);
    let root_ver = pkg_obj
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.0.0");
    let is_private = pkg_obj
        .get("private")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let root_license = pkg_obj.get("license").and_then(JsonValue::as_str);

    if !is_private || root_license.is_some() {
        check_package_license(
            DependencyEcosystem::Npm,
            root_name,
            root_ver,
            root_license,
            policy,
            findings,
        );
    }

    for section_key in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        if let Some(deps_obj) = pkg_obj.get(section_key).and_then(JsonValue::as_object) {
            audit_npm_manifest_dep_map(&pkg_label, deps_obj, findings);
        }
    }

    let lock_path = dir.join("package-lock.json");
    let lock_label = if rel_prefix.is_empty() {
        "package-lock.json".to_owned()
    } else {
        format!("{rel_prefix}/package-lock.json")
    };
    if !lock_path.is_file() {
        findings.push(SupplyChainFinding {
            rule_id: "RISK-B-09".to_owned(),
            severity: CertifySeverity::Blocker,
            ecosystem: DependencyEcosystem::Npm,
            package_name: lock_label,
            package_version: "missing".to_owned(),
            kind: SupplyChainViolationKind::LockfileMissing,
            message: format!("package-lock.json is missing alongside {pkg_label}"),
            remediation:
                "Commit a deterministic package-lock.json with SRI integrity hashes and SPDX license metadata."
                    .to_owned(),
        });
        return Ok(0);
    }

    let lock_text = fs::read_to_string(&lock_path).map_err(|source| CertifyError::Io {
        context: format!("read {}", lock_path.display()),
        source,
    })?;
    let lock_val = parse_json(&lock_text).map_err(|source| CertifyError::JsonParse {
        context: lock_label.clone(),
        source,
    })?;
    let Some(lock_obj) = lock_val.as_object() else {
        return Err(CertifyError::Authority(format!(
            "{lock_label} top-level value must be a JSON object"
        )));
    };

    let mut audited = 0_usize;
    if let Some(packages_map) = lock_obj.get("packages").and_then(JsonValue::as_object) {
        for (pkg_key, entry_val) in packages_map {
            if pkg_key.is_empty() {
                continue;
            }
            let Some(entry_obj) = entry_val.as_object() else {
                continue;
            };
            if entry_obj
                .get("link")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            audited += 1;
            let derived_name = pkg_key
                .rsplit_once("node_modules/")
                .map_or(pkg_key.as_str(), |(_, tail)| tail);
            let name = entry_obj
                .get("name")
                .and_then(JsonValue::as_str)
                .unwrap_or(derived_name);
            let version = entry_obj
                .get("version")
                .and_then(JsonValue::as_str)
                .unwrap_or("0.0.0");

            check_advisory_deny(DependencyEcosystem::Npm, name, version, policy, findings);

            let integrity_ok = entry_obj
                .get("integrity")
                .and_then(JsonValue::as_str)
                .is_some_and(|s| {
                    (s.starts_with("sha512-")
                        || s.starts_with("sha256-")
                        || s.starts_with("sha384-"))
                        && s.len() > 16
                });
            if !integrity_ok {
                findings.push(SupplyChainFinding {
                    rule_id: "RISK-B-09".to_owned(),
                    severity: CertifySeverity::Blocker,
                    ecosystem: DependencyEcosystem::Npm,
                    package_name: name.to_owned(),
                    package_version: version.to_owned(),
                    kind: SupplyChainViolationKind::MissingChecksum,
                    message: format!(
                        "npm package '{name}@{version}' in {lock_label} is missing a valid SRI integrity digest"
                    ),
                    remediation:
                        "Regenerate package-lock.json so every external package records a non-empty sha512/sha256 integrity hash."
                            .to_owned(),
                });
            }

            if let Some(resolved) = entry_obj.get("resolved").and_then(JsonValue::as_str)
                && resolved.starts_with("git+")
                && !has_pinned_git_commit_fragment(resolved)
            {
                findings.push(SupplyChainFinding {
                    rule_id: "RISK-B-09".to_owned(),
                    severity: CertifySeverity::Blocker,
                    ecosystem: DependencyEcosystem::Npm,
                    package_name: name.to_owned(),
                    package_version: version.to_owned(),
                    kind: SupplyChainViolationKind::FloatingDependency,
                    message: format!(
                        "npm git package '{name}@{version}' in {lock_label} lacks a pinned commit SHA ('{resolved}')"
                    ),
                    remediation: "Pin git URLs to an exact commit hash.".to_owned(),
                });
            }

            let lic_str = extract_npm_license_expr(entry_obj);
            check_package_license(
                DependencyEcosystem::Npm,
                name,
                version,
                lic_str.as_deref(),
                policy,
                findings,
            );
        }
    }

    Ok(audited)
}

fn audit_npm_manifest_dep_map(
    pkg_label: &str,
    deps_obj: &BTreeMap<String, JsonValue>,
    findings: &mut Vec<SupplyChainFinding>,
) {
    for (dep_name, spec_val) in deps_obj {
        let Some(spec) = spec_val.as_str() else {
            continue;
        };
        let trimmed = spec.trim();
        if trimmed == "*"
            || trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("latest")
            || trimmed.eq_ignore_ascii_case("next")
        {
            findings.push(SupplyChainFinding {
                rule_id: "RISK-B-09".to_owned(),
                severity: CertifySeverity::Blocker,
                ecosystem: DependencyEcosystem::Npm,
                package_name: dep_name.clone(),
                package_version: trimmed.to_owned(),
                kind: SupplyChainViolationKind::FloatingDependency,
                message: format!(
                    "{pkg_label} declares floating dependency '{dep_name}: \"{trimmed}\"'"
                ),
                remediation:
                    "Replace wildcard/floating tag with a bounded SemVer range and lock it in package-lock.json."
                        .to_owned(),
            });
        } else if (trimmed.starts_with("git+")
            || trimmed.starts_with("git://")
            || trimmed.starts_with("github:"))
            && !has_pinned_git_commit_fragment(trimmed)
        {
            findings.push(SupplyChainFinding {
                rule_id: "RISK-B-09".to_owned(),
                severity: CertifySeverity::Blocker,
                ecosystem: DependencyEcosystem::Npm,
                package_name: dep_name.clone(),
                package_version: trimmed.to_owned(),
                kind: SupplyChainViolationKind::FloatingDependency,
                message: format!(
                    "{pkg_label} declares unpinned git dependency '{dep_name}: \"{trimmed}\"' without commit SHA"
                ),
                remediation: "Append '#<40-hex-commit-sha>' or depend on a pinned registry release."
                    .to_owned(),
            });
        }
    }
}

fn extract_npm_license_expr(entry_obj: &BTreeMap<String, JsonValue>) -> Option<String> {
    if let Some(s) = entry_obj.get("license").and_then(JsonValue::as_str) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    if let Some(arr) = entry_obj.get("licenses").and_then(JsonValue::as_array) {
        let mut parts = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                parts.push(s.to_owned());
            } else if let Some(obj) = item.as_object()
                && let Some(t) = obj.get("type").and_then(JsonValue::as_str)
            {
                parts.push(t.to_owned());
            }
        }
        if !parts.is_empty() {
            return Some(parts.join(" OR "));
        }
    }
    None
}

fn check_package_license(
    ecosystem: DependencyEcosystem,
    name: &str,
    version: &str,
    license_expr: Option<&str>,
    policy: &SupplyChainPolicy,
    findings: &mut Vec<SupplyChainFinding>,
) {
    let Some(raw_expr) = license_expr.map(str::trim).filter(|s| !s.is_empty()) else {
        findings.push(SupplyChainFinding {
            rule_id: "RISK-B-09".to_owned(),
            severity: CertifySeverity::Blocker,
            ecosystem,
            package_name: name.to_owned(),
            package_version: version.to_owned(),
            kind: SupplyChainViolationKind::MissingLicense,
            message: format!(
                "{} package '{name}@{version}' is missing SPDX license metadata",
                ecosystem.as_str()
            ),
            remediation:
                "Declare an explicit permissive SPDX license identifier on every dependency."
                    .to_owned(),
        });
        return;
    };

    if !evaluate_spdx_expression(raw_expr, policy) {
        findings.push(SupplyChainFinding {
            rule_id: "RISK-B-09".to_owned(),
            severity: CertifySeverity::Blocker,
            ecosystem,
            package_name: name.to_owned(),
            package_version: version.to_owned(),
            kind: SupplyChainViolationKind::DisallowedLicense,
            message: format!(
                "{} package '{name}@{version}' declares disallowed SPDX license '{raw_expr}'",
                ecosystem.as_str()
            ),
            remediation:
                "Replace dependency with a permissively licensed alternative (e.g. Apache-2.0, MIT, BSD-3-Clause, ISC, Zlib)."
                    .to_owned(),
        });
    }
}

fn check_advisory_deny(
    ecosystem: DependencyEcosystem,
    name: &str,
    version: &str,
    policy: &SupplyChainPolicy,
    findings: &mut Vec<SupplyChainFinding>,
) {
    for deny in &policy.advisory_deny_list {
        if deny.ecosystem == ecosystem && deny.package_name == name && deny.version == version {
            findings.push(SupplyChainFinding {
                rule_id: "RISK-B-09".to_owned(),
                severity: CertifySeverity::Blocker,
                ecosystem,
                package_name: name.to_owned(),
                package_version: version.to_owned(),
                kind: SupplyChainViolationKind::AdvisoryDenied,
                message: format!(
                    "{} package '{name}@{version}' is blocked by security advisory {}: {}",
                    ecosystem.as_str(),
                    deny.advisory_id,
                    deny.reason
                ),
                remediation: format!(
                    "Upgrade or remove '{name}@{version}' to remediate {}.",
                    deny.advisory_id
                ),
            });
        }
    }
}

fn is_valid_sha256_hex(candidate: &str) -> bool {
    candidate.len() == 64 && candidate.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_valid_git_commit_rev(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    (7..=64).contains(&trimmed.len()) && trimmed.bytes().all(|b| b.is_ascii_hexdigit())
}

fn has_pinned_git_commit_fragment(url: &str) -> bool {
    let Some((_, frag)) = url.rsplit_once('#') else {
        return false;
    };
    is_valid_git_commit_rev(frag)
}
