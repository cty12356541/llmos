//! Package payload scanner enforcing immutable-tree locality,
//! `[PKG-SEED-001]` mutable database / WAL exclusion, and
//! `[PKG-IMM-001]` / `[DRV-CRED-001]` hardcoded plaintext credential blocking.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::manifest_audit::AuditedPackageManifest;
use crate::report::{AuditFinding, FindingSeverity};

/// Maximum payload file size scanned in full (`16 MiB`); larger files fail closed.
pub const MAX_SCANNED_PAYLOAD_BYTES: u64 = 16 * 1024 * 1024;

const SQLITE3_HEADER_PREFIX: &[u8] = b"SQLite format 3";
const SQLITE_WAL_MAGIC_BE: [u8; 4] = [0x37, 0x7f, 0x06, 0x82];
const SQLITE_WAL_MAGIC_LE: [u8; 4] = [0x37, 0x7f, 0x06, 0x83];

const MUTABLE_SIDECAR_SUFFIXES: &[&str] = &[
    ".sqlite-wal",
    ".db-wal",
    ".sqlite3-wal",
    ".sqlite-shm",
    ".db-shm",
    ".sqlite3-shm",
    ".sqlite-journal",
    ".db-journal",
    ".sqlite3-journal",
];

const MUTABLE_DB_SUFFIXES: &[&str] = &[".sqlite", ".sqlite3", ".db"];

/// Scans all declared components, task template artifacts, and package-tree
/// files inside `package_dir` for locality, mutable database/WAL, and secret
/// violations.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn scan_package_payloads(
    package_dir: &Path,
    manifest: &AuditedPackageManifest,
) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    let mut reported_seed_violations: BTreeSet<String> = BTreeSet::new();

    for comp in &manifest.components {
        let location = format!("package.manifest:{}", comp.line);
        let Some(resolved) = resolve_confined_path(package_dir, &comp.relative_path) else {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "PKG-IMM-001".to_owned(),
                location,
                message: format!(
                    "component {:?} path {:?} escapes package root or uses a non-relative path",
                    comp.name, comp.relative_path
                ),
                remediation: Some(
                    "Use a normalized relative path confined strictly inside the package directory."
                        .to_owned(),
                ),
            });
            continue;
        };

        let Ok(metadata) = fs::metadata(&resolved) else {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "PKG-MULTI-001".to_owned(),
                location,
                message: format!(
                    "component {:?} ({}) payload {:?} does not exist",
                    comp.name,
                    comp.role.as_str(),
                    comp.relative_path
                ),
                remediation: Some(
                    "Ensure every declared entry/component file exists inside the package tree."
                        .to_owned(),
                ),
            });
            continue;
        };

        if !metadata.is_file() {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "PKG-MULTI-001".to_owned(),
                location,
                message: format!(
                    "component {:?} payload {:?} is not a regular file",
                    comp.name, comp.relative_path
                ),
                remediation: Some(
                    "Bind each component entry to a regular payload file.".to_owned(),
                ),
            });
            continue;
        }

        if comp.role.is_seed_or_data() {
            let normalized_rel = normalize_rel_display(&comp.relative_path);
            let lower = normalized_rel.to_ascii_lowercase();
            if has_any_suffix(&lower, MUTABLE_SIDECAR_SUFFIXES)
                || has_any_suffix(&lower, MUTABLE_DB_SUFFIXES)
            {
                if reported_seed_violations.insert(normalized_rel.clone()) {
                    findings.push(AuditFinding {
                        severity: FindingSeverity::Blocker,
                        rule_id: "PKG-SEED-001".to_owned(),
                        location: normalized_rel,
                        message: format!(
                            "{} component {:?} bundles a mutable database or WAL/SHM file ({:?})",
                            comp.role.as_str(),
                            comp.name,
                            comp.relative_path
                        ),
                        remediation: Some(
                            "Ship read-only declarative schema/seed templates and materialize live databases inside UserDataContainer at runtime."
                                .to_owned(),
                        ),
                    });
                }
            } else if let Ok(bytes) = fs::read(&resolved)
                && is_sqlite_binary_payload(&bytes)
                && reported_seed_violations.insert(normalized_rel.clone())
            {
                findings.push(AuditFinding {
                    severity: FindingSeverity::Blocker,
                    rule_id: "PKG-SEED-001".to_owned(),
                    location: normalized_rel,
                    message: format!(
                        "{} component {:?} contains a binary SQLite database/WAL header",
                        comp.role.as_str(),
                        comp.name
                    ),
                    remediation: Some(
                        "Replace binary SQLite database images with immutable SQL/seed templates copied or applied into UserDataContainer."
                            .to_owned(),
                    ),
                });
            }
        }
    }

    for task in &manifest.tasks {
        let task_location = format!("package.manifest:{}", task.line);
        for (field_label, rel_path) in [
            ("binding", &task.binding_path),
            ("inputs", &task.inputs_path),
            ("outputs", &task.outputs_path),
            ("policy", &task.policy_path),
            ("ceiling", &task.ceiling_path),
        ] {
            let Some(resolved) = resolve_confined_path(package_dir, rel_path) else {
                findings.push(AuditFinding {
                    severity: FindingSeverity::Blocker,
                    rule_id: "PKG-IMM-001".to_owned(),
                    location: task_location.clone(),
                    message: format!(
                        "task {field_label} path {rel_path:?} escapes package root or uses a non-relative path"
                    ),
                    remediation: Some(
                        "Use a relative path confined inside the package directory.".to_owned(),
                    ),
                });
                continue;
            };
            match fs::metadata(&resolved) {
                Ok(meta) if meta.is_file() => {}
                _ => {
                    findings.push(AuditFinding {
                        severity: FindingSeverity::Blocker,
                        rule_id: "PKG-MULTI-001".to_owned(),
                        location: task_location.clone(),
                        message: format!(
                            "task {field_label} artifact {rel_path:?} does not exist as a regular file"
                        ),
                        remediation: Some(
                            "Ensure all task template artifact files exist in the package tree."
                                .to_owned(),
                        ),
                    });
                }
            }
        }
    }

    let mut tree_files = Vec::new();
    if let Err(err) = collect_tree_files(package_dir, package_dir, &mut tree_files) {
        findings.push(AuditFinding {
            severity: FindingSeverity::Blocker,
            rule_id: "PKG-IMM-001".to_owned(),
            location: package_dir.display().to_string(),
            message: format!("failed to enumerate package directory tree: {err}"),
            remediation: Some(
                "Verify read permissions and remove broken directory entries.".to_owned(),
            ),
        });
        return findings;
    }
    tree_files.sort_by(|left, right| left.0.cmp(&right.0));

    for (rel_path, full_path) in tree_files {
        let lower = rel_path.to_ascii_lowercase();
        if has_any_suffix(&lower, MUTABLE_SIDECAR_SUFFIXES)
            && reported_seed_violations.insert(rel_path.clone())
        {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "PKG-SEED-001".to_owned(),
                location: rel_path.clone(),
                message: "immutable package directory contains a mutable database WAL/SHM/journal sidecar".to_owned(),
                remediation: Some(
                    "Remove live `.sqlite-wal` / `.db-wal` / `.db-shm` / journal files from the package tree."
                        .to_owned(),
                ),
            });
        }

        let metadata = match fs::metadata(&full_path) {
            Ok(meta) => meta,
            Err(err) => {
                findings.push(AuditFinding {
                    severity: FindingSeverity::Blocker,
                    rule_id: "PKG-IMM-001".to_owned(),
                    location: rel_path,
                    message: format!("failed to stat package file: {err}"),
                    remediation: None,
                });
                continue;
            }
        };

        if metadata.len() > MAX_SCANNED_PAYLOAD_BYTES {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "PKG-IMM-001".to_owned(),
                location: rel_path,
                message: format!(
                    "payload size {} exceeds static scan bound {MAX_SCANNED_PAYLOAD_BYTES} bytes",
                    metadata.len()
                ),
                remediation: Some("Split or externalize oversized immutable assets.".to_owned()),
            });
            continue;
        }

        let bytes = match fs::read(&full_path) {
            Ok(data) => data,
            Err(err) => {
                findings.push(AuditFinding {
                    severity: FindingSeverity::Blocker,
                    rule_id: "PKG-IMM-001".to_owned(),
                    location: rel_path,
                    message: format!("failed to read package file: {err}"),
                    remediation: None,
                });
                continue;
            }
        };

        if is_sqlite_binary_payload(&bytes) && reported_seed_violations.insert(rel_path.clone()) {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "PKG-SEED-001".to_owned(),
                location: rel_path.clone(),
                message:
                    "immutable package directory contains a binary SQLite database or WAL image"
                        .to_owned(),
                remediation: Some(
                    "Exclude binary SQLite images from immutable packages; initialize runtime databases inside UserDataContainer."
                        .to_owned(),
                ),
            });
        }

        scan_bytes_for_secrets(&rel_path, &bytes, &mut findings);
    }

    findings
}

fn resolve_confined_path(root: &Path, raw_relative: &str) -> Option<PathBuf> {
    if raw_relative.is_empty()
        || raw_relative.starts_with('/')
        || raw_relative.starts_with('\\')
        || raw_relative.contains(':')
    {
        return None;
    }
    let normalized = raw_relative.replace('\\', "/");
    let candidate = Path::new(&normalized);
    let mut joined = root.to_path_buf();
    let mut has_normal = false;
    for component in candidate.components() {
        match component {
            Component::Normal(part) => {
                has_normal = true;
                joined.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if has_normal { Some(joined) } else { None }
}

fn normalize_rel_display(raw_relative: &str) -> String {
    raw_relative
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_owned()
}

fn has_any_suffix(lower_name: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|suffix| lower_name.ends_with(suffix))
}

fn is_sqlite_binary_payload(bytes: &[u8]) -> bool {
    if bytes.starts_with(SQLITE3_HEADER_PREFIX) {
        return true;
    }
    bytes.len() >= 4 && (bytes[..4] == SQLITE_WAL_MAGIC_BE || bytes[..4] == SQLITE_WAL_MAGIC_LE)
}

fn collect_tree_files(
    root: &Path,
    current: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let entries = fs::read_dir(current).map_err(|err| err.to_string())?;
    for entry_res in entries {
        let entry = entry_res.map_err(|err| err.to_string())?;
        let file_type = entry.file_type().map_err(|err| err.to_string())?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|err| err.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        if file_type.is_symlink() {
            return Err(format!(
                "symbolic link {rel:?} is forbidden inside immutable package tree"
            ));
        }
        if file_type.is_dir() {
            collect_tree_files(root, &path, out)?;
        } else if file_type.is_file() {
            out.push((rel, path));
        }
    }
    Ok(())
}

fn scan_bytes_for_secrets(rel_path: &str, bytes: &[u8], findings: &mut Vec<AuditFinding>) {
    let lossy = String::from_utf8_lossy(bytes);
    for (index, line) in lossy.lines().enumerate() {
        let line_no = index + 1;
        let location = format!("{rel_path}:{line_no}");
        for redacted_kind in detect_line_secrets(line) {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "PKG-IMM-001".to_owned(),
                location: location.clone(),
                message: format!(
                    "hardcoded plaintext credential detected ({redacted_kind}); immutable packages and user-space drivers must never embed raw secrets"
                ),
                remediation: Some(
                    "Remove the secret from the package tree and delegate credential custody/injection to the Trusted Enforcement Shim (`[DRV-CRED-001]`)."
                        .to_owned(),
                ),
            });
        }
    }
}

fn detect_line_secrets(line: &str) -> Vec<&'static str> {
    let mut hits = Vec::new();
    if contains_pem_private_key_header(line) {
        hits.push("PEM private key header `-----BEGIN [REDACTED] PRIVATE KEY-----`");
    }
    if contains_sk_secret_token(line) {
        hits.push("API token prefix `sk-[REDACTED]`");
    }
    if contains_bearer_secret_token(line) {
        hits.push("Authorization token `Bearer [REDACTED]`");
    }
    if contains_api_key_assignment(line) {
        hits.push("hardcoded credential assignment `api_key=[REDACTED]`");
    }
    hits
}

fn contains_pem_private_key_header(line: &str) -> bool {
    if let Some(start) = line.find("-----BEGIN ") {
        let rest = &line[start + "-----BEGIN ".len()..];
        return rest.contains("PRIVATE KEY-----");
    }
    false
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn contains_sk_secret_token(line: &str) -> bool {
    let bytes = line.as_bytes();
    let prefix = b"sk-";
    if bytes.len() < prefix.len() + 8 {
        return false;
    }
    let mut idx = 0;
    while idx + prefix.len() <= bytes.len() {
        if &bytes[idx..idx + prefix.len()] == prefix {
            let boundary_ok = idx == 0 || !is_ident_byte(bytes[idx - 1]);
            if boundary_ok {
                let tail = &bytes[idx + prefix.len()..];
                let token_len = tail
                    .iter()
                    .take_while(|&&ch| ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'-')
                    .count();
                if token_len >= 8 {
                    return true;
                }
            }
        }
        idx += 1;
    }
    false
}

fn contains_bearer_secret_token(line: &str) -> bool {
    let bytes = line.as_bytes();
    let prefix = b"Bearer ";
    if bytes.len() < prefix.len() + 8 {
        return false;
    }
    let mut idx = 0;
    while idx + prefix.len() <= bytes.len() {
        if &bytes[idx..idx + prefix.len()] == prefix {
            let boundary_ok = idx == 0 || !is_ident_byte(bytes[idx - 1]);
            if boundary_ok {
                let tail = &bytes[idx + prefix.len()..];
                if !tail.starts_with(b"<") && !tail.starts_with(b"$") {
                    let token_len = tail
                        .iter()
                        .take_while(|&&ch| {
                            ch.is_ascii_alphanumeric()
                                || matches!(ch, b'.' | b'_' | b'-' | b'~' | b'+' | b'/' | b'=')
                        })
                        .count();
                    if token_len >= 8 {
                        return true;
                    }
                }
            }
        }
        idx += 1;
    }
    false
}

fn contains_api_key_assignment(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for keyword in ["api_key", "apikey", "api-key", "secret_key"] {
        let kw = keyword.as_bytes();
        if bytes.len() <= kw.len() {
            continue;
        }
        let mut idx = 0;
        while idx + kw.len() <= bytes.len() {
            if &bytes[idx..idx + kw.len()] == kw {
                let left_ok =
                    idx == 0 || !(is_ident_byte(bytes[idx - 1]) || bytes[idx - 1] == b'-');
                let after = idx + kw.len();
                let right_ok =
                    after == bytes.len() || !(is_ident_byte(bytes[after]) || bytes[after] == b'-');
                if left_ok && right_ok {
                    let rest = line[after..].trim_start_matches([' ', '\t', '"', '\'']);
                    if let Some(after_sep) =
                        rest.strip_prefix('=').or_else(|| rest.strip_prefix(':'))
                    {
                        let value_trimmed = after_sep.trim_start();
                        let unquoted = value_trimmed.trim_start_matches(['"', '\'']);
                        let raw_val: &str = unquoted
                            .split(['"', '\'', ' ', '\t', ',', ';', '#'])
                            .next()
                            .unwrap_or("");
                        if raw_val.len() >= 6
                            && !raw_val.starts_with("${")
                            && !raw_val.starts_with('$')
                            && !raw_val.starts_with('<')
                            && !matches!(
                                raw_val.to_ascii_lowercase().as_str(),
                                "redacted" | "[redacted]" | "none" | "null" | "changeme"
                            )
                        {
                            return true;
                        }
                    }
                }
            }
            idx += 1;
        }
    }
    false
}
