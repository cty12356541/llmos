//! Gate 2: Least-privilege boundary, immutable package purity, and embedded
//! dependency supply-chain audit (`ADR-0027` §3.2).
//!
//! Enforces:
//! - `[TCB-EGRESS-001]`: Blocks undeclared/unapproved external network endpoints
//!   (`http://`, `https://`), raw socket / loopback control-plane bypass probes,
//!   and cloud metadata service endpoints (`169.254.169.254`).
//! - `[DUI-CHOOSER-001]`: Blocks raw global host filesystem paths (`C:\`, `/etc/`,
//!   `/Users/`, `/home/`, `..` traversal, global directory enumeration) and
//!   requires scoped Artifact chooser mediation.
//! - `[DUI-ASSOC-001]`: Blocks silent default content-type/file association
//!   hijacking (`force` / `hijack-default-handler`), permitting only `suggest`.
//! - `[PKG-IMM-001]` & `[PKG-SEED-001]`: Reuses `nlos_migrate::scan_package_payloads`
//!   to block baked-in mutable `SQLite`/WAL databases and hardcoded plaintext
//!   secrets (strictly redacted in audit output).
//! - `[RISK-B-09]`: Runs [`crate::audit_dependency_supply_chain`] whenever the
//!   package directory embeds a `Cargo.toml` or `package.json`.

use std::fs;
use std::path::Path;

use nlos_migrate::{FindingSeverity, scan_package_payloads};

use crate::model::{
    CertifyError, CertifyFinding, CertifySeverity, GateExecutionSummary, GateId,
    LoadedCertifyPackage,
};
use crate::supply_chain_audit::{SupplyChainPolicy, audit_dependency_supply_chain};

/// Detailed execution output of Gate 2.
#[derive(Clone, Debug)]
pub struct Gate2Outcome {
    /// Number of package payload files scanned.
    pub scanned_files: usize,
    /// Structured findings emitted by Gate 2.
    pub findings: Vec<CertifyFinding>,
    /// Cryptographically committed summary for receipt inclusion.
    pub summary: GateExecutionSummary,
}

/// Runs Gate 2 (least privilege, immutable package purity, and supply-chain audit)
/// against `pkg`.
///
/// # Errors
///
/// Returns [`CertifyError`] only on unexpected filesystem I/O failures while
/// traversing the package directory. All policy violations are recorded as
/// `BLOCKER` [`CertifyFinding`]s inside [`Gate2Outcome`].
pub fn run_gate2_supply_chain_audit(
    pkg: &LoadedCertifyPackage,
    supply_chain_policy: &SupplyChainPolicy,
) -> Result<Gate2Outcome, CertifyError> {
    let mut findings = Vec::new();

    // 1. Reuse `nlos_migrate::scan_package_payloads` for `[PKG-IMM-001]` and `[PKG-SEED-001]`.
    let migrate_findings = scan_package_payloads(&pkg.package_dir, &pkg.base_manifest);
    let scanned_files = count_package_files(&pkg.package_dir)?;

    for mf in migrate_findings {
        let severity = match mf.severity {
            FindingSeverity::Blocker => CertifySeverity::Blocker,
            FindingSeverity::Warning => CertifySeverity::Warning,
        };
        findings.push(CertifyFinding {
            gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
            rule_id: mf.rule_id,
            severity,
            subject: mf.location,
            message: mf.message,
            remediation: mf.remediation.unwrap_or_else(|| {
                "Remove mutable databases and plaintext secrets from immutable package payloads."
                    .to_owned()
            }),
        });
    }

    // 2. Audit manifest-level network egress, chooser scope, and file-association directives.
    let approved_https_prefixes = audit_manifest_least_privilege(pkg, &mut findings);

    // 3. Deep-scan payload files for `[TCB-EGRESS-001]`, `[DUI-CHOOSER-001]`, `[DUI-ASSOC-001]`.
    scan_payload_least_privilege(
        &pkg.package_dir,
        &pkg.package_dir,
        &approved_https_prefixes,
        &mut findings,
    )?;

    // 4. If package bundles `Cargo.toml` or `package.json`, enforce `[RISK-B-09]`.
    if pkg.package_dir.join("Cargo.toml").is_file()
        || pkg.package_dir.join("package.json").is_file()
    {
        let sc_report = audit_dependency_supply_chain(&pkg.package_dir, supply_chain_policy)?;
        for scf in sc_report.findings {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: scf.rule_id,
                severity: scf.severity,
                subject: format!(
                    "{}:{}@{}",
                    scf.ecosystem.as_str(),
                    scf.package_name,
                    scf.package_version
                ),
                message: scf.message,
                remediation: scf.remediation,
            });
        }
    }

    let blocker_count = findings
        .iter()
        .filter(|f| f.severity == CertifySeverity::Blocker)
        .count();
    let summary_line =
        format!("Gate 2 (scanned_files={scanned_files}): {blocker_count} blocker(s)");
    let summary = GateExecutionSummary::from_findings(
        GateId::Gate2LeastPrivilegeAndSupplyChain,
        &findings,
        &(scanned_files as u64).to_be_bytes(),
        summary_line,
    );

    Ok(Gate2Outcome {
        scanned_files,
        findings,
        summary,
    })
}

#[allow(clippy::too_many_lines)]
fn audit_manifest_least_privilege(
    pkg: &LoadedCertifyPackage,
    findings: &mut Vec<CertifyFinding>,
) -> Vec<String> {
    let mut egress_declared = false;
    if let Some((ref policy_val, line_no)) = pkg.directives.network_policy {
        match policy_val.as_str() {
            "none" | "offline-only" => {}
            "egress-declared" => {
                egress_declared = true;
            }
            other => {
                findings.push(CertifyFinding {
                    gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                    rule_id: "TCB-EGRESS-001".to_owned(),
                    severity: CertifySeverity::Blocker,
                    subject: format!("package.manifest:{line_no}"),
                    message: format!(
                        "network-policy='{other}' is forbidden; packages default to zero egress unless 'network-policy = egress-declared'"
                    ),
                    remediation:
                        "Set 'network-policy = none' or declare 'network-policy = egress-declared' with explicit 'egress-allow = https://...' endpoints."
                            .to_owned(),
                });
            }
        }
    }

    let mut approved_https_prefixes = Vec::new();
    for (endpoint, line_no) in &pkg.directives.egress_allowlist {
        let trimmed = endpoint.trim();
        if !egress_declared {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: "TCB-EGRESS-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("package.manifest:{line_no}"),
                message: format!(
                    "egress-allow '{trimmed}' declared without 'network-policy = egress-declared'"
                ),
                remediation:
                    "Declare 'network-policy = egress-declared' before listing egress-allow endpoints."
                        .to_owned(),
            });
        }
        if let Some(reason) = forbidden_egress_target_reason(trimmed, true) {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: "TCB-EGRESS-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("package.manifest:{line_no}"),
                message: format!("unacceptable egress-allow endpoint '{trimmed}': {reason}"),
                remediation:
                    "Allowlist only concrete external https:// origins; never target cleartext HTTP, loopback, or cloud metadata IPs."
                        .to_owned(),
            });
        } else if egress_declared {
            approved_https_prefixes.push(trimmed.to_owned());
        }
    }

    if let Some((ref scope_val, line_no)) = pkg.directives.chooser_scope
        && !matches!(scope_val.as_str(), "artifact-chooser" | "scoped-artifact")
    {
        findings.push(CertifyFinding {
            gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
            rule_id: "DUI-CHOOSER-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: format!("package.manifest:{line_no}"),
            message: format!(
                "chooser-scope='{scope_val}' bypasses scoped Artifact chooser mediation"
            ),
            remediation:
                "Use 'chooser-scope = artifact-chooser' so user file selection grants scoped Artifact capabilities rather than raw host paths."
                    .to_owned(),
        });
    }

    for (fs_mode, line_no) in &pkg.directives.filesystem_access {
        if !matches!(
            fs_mode.as_str(),
            "scoped-artifact" | "artifact-chooser" | "user-data-container"
        ) {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: "DUI-CHOOSER-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("package.manifest:{line_no}"),
                message: format!(
                    "filesystem-access='{fs_mode}' requests unscoped raw host filesystem visibility"
                ),
                remediation:
                    "Replace raw host filesystem access with scoped Artifact chooser capabilities and isolated UserDataContainer storage."
                        .to_owned(),
            });
        }
    }

    // Check component & task relative paths for traversal or raw host roots.
    for comp in &pkg.base_manifest.components {
        if let Some(reason) = detect_raw_host_path(&comp.relative_path) {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: "DUI-CHOOSER-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("package.manifest:{}", comp.line),
                message: format!(
                    "component '{}' path '{}' escapes package sandbox ({reason})",
                    comp.name, comp.relative_path
                ),
                remediation:
                    "Use clean package-relative artifact paths without '..' or host roots."
                        .to_owned(),
            });
        }
    }
    for task in &pkg.base_manifest.tasks {
        for rel in [
            &task.binding_path,
            &task.inputs_path,
            &task.outputs_path,
            &task.policy_path,
            &task.ceiling_path,
        ] {
            if let Some(reason) = detect_raw_host_path(rel) {
                findings.push(CertifyFinding {
                    gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                    rule_id: "DUI-CHOOSER-001".to_owned(),
                    severity: CertifySeverity::Blocker,
                    subject: format!("package.manifest:{}", task.line),
                    message: format!(
                        "task template path '{rel}' escapes package sandbox ({reason})"
                    ),
                    remediation:
                        "Use clean package-relative task artifact paths without '..' or host roots."
                            .to_owned(),
                });
            }
        }
    }

    if let Some((ref assoc_mode, line_no)) = pkg.directives.default_association
        && !matches!(assoc_mode.as_str(), "suggest" | "none")
    {
        findings.push(CertifyFinding {
            gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
            rule_id: "DUI-ASSOC-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: format!("package.manifest:{line_no}"),
            message: format!(
                "default-association='{assoc_mode}' attempts silent default content-type hijacking"
            ),
            remediation:
                "Change default-association to 'suggest'; only the user/system policy plane may bind default handlers."
                    .to_owned(),
        });
    }

    for assoc in &pkg.directives.file_associations {
        if assoc.mode != "suggest" {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: "DUI-ASSOC-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("package.manifest:{}", assoc.line_no),
                message: format!(
                    "file-association for '{}' uses forbidden mode '{}' (only 'suggest' is permitted)",
                    assoc.target, assoc.mode
                ),
                remediation: format!(
                    "Declare 'file-association = {} suggest'.",
                    assoc.target
                ),
            });
        }
    }

    approved_https_prefixes
}

fn scan_payload_least_privilege(
    root: &Path,
    current: &Path,
    approved_https_prefixes: &[String],
    findings: &mut Vec<CertifyFinding>,
) -> Result<(), CertifyError> {
    let entries = fs::read_dir(current).map_err(|source| CertifyError::Io {
        context: format!("read_dir {}", current.display()),
        source,
    })?;
    let mut paths: Vec<_> =
        entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| CertifyError::Io {
                context: format!("dir_entry {}", current.display()),
                source,
            })?;
    paths.sort_by_key(fs::DirEntry::file_name);

    for entry in paths {
        let path = entry.path();
        let ft = entry.file_type().map_err(|source| CertifyError::Io {
            context: format!("file_type {}", path.display()),
            source,
        })?;
        if ft.is_dir() {
            scan_payload_least_privilege(root, &path, approved_https_prefixes, findings)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if matches!(
            rel.as_str(),
            "package.manifest" | "certify.manifest" | "Cargo.lock" | "package-lock.json"
        ) {
            continue;
        }

        let bytes = fs::read(&path).map_err(|source| CertifyError::Io {
            context: format!("read {}", path.display()),
            source,
        })?;
        let text = String::from_utf8_lossy(&bytes);
        scan_text_lines_for_least_privilege(&rel, &text, approved_https_prefixes, findings);
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn scan_text_lines_for_least_privilege(
    rel_path: &str,
    text: &str,
    approved_https_prefixes: &[String],
    findings: &mut Vec<CertifyFinding>,
) {
    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let subject = format!("{rel_path}:{line_no}");

        // 1. `[TCB-EGRESS-001]` network / metadata / raw-socket checks.
        if trimmed.contains("169.254.169.254") || trimmed.contains("metadata.google.internal") {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: "TCB-EGRESS-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: subject.clone(),
                message: "payload references cloud instance metadata endpoint (169.254.169.254 / metadata.google.internal)".to_owned(),
                remediation: "Remove all cloud metadata SSRF/egress probes from package payloads.".to_owned(),
            });
        } else if contains_loopback_or_raw_socket_probe(trimmed) {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: "TCB-EGRESS-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: subject.clone(),
                message: "payload contains raw socket or loopback control-plane bypass endpoint".to_owned(),
                remediation: "Communicate exclusively through typed SABI IPC channels; remove direct socket/loopback endpoints.".to_owned(),
            });
        } else {
            for url in extract_http_urls(trimmed) {
                if url.starts_with("http://") {
                    findings.push(CertifyFinding {
                        gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                        rule_id: "TCB-EGRESS-001".to_owned(),
                        severity: CertifySeverity::Blocker,
                        subject: subject.clone(),
                        message: format!(
                            "payload contains cleartext HTTP endpoint '{url}' (forbidden in certified packages)"
                        ),
                        remediation:
                            "Remove cleartext http:// URLs or migrate approved endpoints to https:// under network-policy = egress-declared."
                                .to_owned(),
                    });
                } else if url.starts_with("https://") {
                    if let Some(reason) = forbidden_egress_target_reason(&url, false) {
                        findings.push(CertifyFinding {
                            gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                            rule_id: "TCB-EGRESS-001".to_owned(),
                            severity: CertifySeverity::Blocker,
                            subject: subject.clone(),
                            message: format!(
                                "payload references forbidden endpoint '{url}': {reason}"
                            ),
                            remediation: "Remove forbidden loopback/metadata endpoint.".to_owned(),
                        });
                    } else {
                        let is_approved = approved_https_prefixes
                            .iter()
                            .any(|prefix| url.starts_with(prefix));
                        if !is_approved {
                            findings.push(CertifyFinding {
                                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                                rule_id: "TCB-EGRESS-001".to_owned(),
                                severity: CertifySeverity::Blocker,
                                subject: subject.clone(),
                                message: format!(
                                    "payload references undeclared external network endpoint '{url}'"
                                ),
                                remediation:
                                    "Declare 'network-policy = egress-declared' and 'egress-allow = https://<origin>' or remove the external URL."
                                        .to_owned(),
                            });
                        }
                    }
                }
            }
        }

        // 2. `[DUI-CHOOSER-001]` raw host filesystem path / global directory enumeration check.
        if let Some(reason) = detect_raw_host_path(trimmed) {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: "DUI-CHOOSER-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: subject.clone(),
                message: format!(
                    "payload requests global raw host filesystem path or traversal ({reason})"
                ),
                remediation:
                    "Request user files exclusively through the scoped Artifact chooser capability."
                        .to_owned(),
            });
        }

        // 3. `[DUI-ASSOC-001]` silent default handler hijacking in payload configs.
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("hijack-default-handler")
            || lower.contains("default-association=force")
            || lower.contains("default-association = force")
            || lower.contains("default_association=force")
            || lower.contains("\"default_association\":\"force\"")
            || lower.contains("\"default_association\": \"force\"")
        {
            findings.push(CertifyFinding {
                gate: GateId::Gate2LeastPrivilegeAndSupplyChain,
                rule_id: "DUI-ASSOC-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject,
                message: "payload configuration attempts silent default content-type association hijacking".to_owned(),
                remediation: "Use 'suggest' association mode instead of forcing default handler takeover.".to_owned(),
            });
        }
    }
}

fn forbidden_egress_target_reason(url: &str, require_https: bool) -> Option<&'static str> {
    let lower = url.to_ascii_lowercase();
    if require_https && !lower.starts_with("https://") {
        return Some("egress-allow must begin with https://");
    }
    if lower.contains('*') {
        return Some("wildcard '*' origins are forbidden");
    }
    if lower.contains("169.254.169.254") || lower.contains("metadata.google.internal") {
        return Some("cloud metadata service IP/host is permanently blocked");
    }
    if lower.contains("://127.")
        || lower.contains("://localhost")
        || lower.contains("://0.0.0.0")
        || lower.contains("://[::1]")
    {
        return Some("loopback / local host bypass endpoints are permanently blocked");
    }
    let after_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    if after_scheme.is_empty() || after_scheme.starts_with('/') {
        return Some("missing authority/hostname");
    }
    None
}

fn contains_loopback_or_raw_socket_probe(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    for marker in [
        "tcp://",
        "udp://",
        "unix://",
        "ws://",
        "wss://127.0.0.1",
        "wss://localhost",
        "127.0.0.1:",
        "0.0.0.0:",
        "localhost:",
        "sock_raw",
    ] {
        if lower.contains(marker) {
            return true;
        }
    }
    false
}

fn extract_http_urls(line: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for scheme in ["http://", "https://"] {
        let mut search_from = 0;
        while let Some(rel_idx) = line[search_from..].find(scheme) {
            let start = search_from + rel_idx;
            let rest = &line[start..];
            let end_len = rest
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '"' | '\'' | '>' | ')' | ']' | ',' | ';')
                })
                .unwrap_or(rest.len());
            let candidate = &rest[..end_len];
            urls.push(candidate.to_owned());
            search_from = start + end_len;
        }
    }
    urls
}

fn detect_raw_host_path(input: &str) -> Option<&'static str> {
    if input.contains("../") || input.contains("..\\") || input == ".." {
        return Some("parent directory '..' traversal");
    }
    let bytes = input.as_bytes();
    // Detect Windows drive-letter root paths such as `C:\` or `D:/` without
    // false-matching URL schemes like `https://` (`s:/`).
    for (idx, window) in bytes.windows(3).enumerate() {
        let preceded_by_ident =
            idx > 0 && (bytes[idx - 1].is_ascii_alphanumeric() || bytes[idx - 1] == b'_');
        if !preceded_by_ident
            && window[0].is_ascii_alphabetic()
            && window[1] == b':'
            && (window[2] == b'\\' || window[2] == b'/')
        {
            return Some("raw Windows drive path (e.g. C:\\)");
        }
    }
    let lower = input.to_ascii_lowercase();
    for unix_root in [
        "/etc/", "/users/", "/home/", "/root/", "/var/", "/proc/", "/sys/", "~/",
    ] {
        if lower.contains(unix_root) {
            return Some("raw host OS directory path");
        }
    }
    for env_escape in [
        "%userprofile%",
        "%appdata%",
        "%systemroot%",
        "enumerate-host-root",
    ] {
        if lower.contains(env_escape) {
            return Some("global host filesystem enumeration / environment root");
        }
    }
    None
}

fn count_package_files(dir: &Path) -> Result<usize, CertifyError> {
    let mut total = 0_usize;
    let entries = fs::read_dir(dir).map_err(|source| CertifyError::Io {
        context: format!("read_dir {}", dir.display()),
        source,
    })?;
    for entry_res in entries {
        let entry = entry_res.map_err(|source| CertifyError::Io {
            context: format!("dir_entry {}", dir.display()),
            source,
        })?;
        let path = entry.path();
        let ft = entry.file_type().map_err(|source| CertifyError::Io {
            context: format!("file_type {}", path.display()),
            source,
        })?;
        if ft.is_dir() {
            total += count_package_files(&path)?;
        } else if ft.is_file() {
            total += 1;
        }
    }
    Ok(total)
}
