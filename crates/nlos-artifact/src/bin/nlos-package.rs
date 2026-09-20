//! `nlos-package` — developer Package SDK CLI (W33-A, B1-4).
//!
//! A thin shell around the `nlos-artifact` package surface: it owns no
//! verification logic of its own. `build` turns a developer tree (line-based
//! manifest + payload files + optional `tasks` templates) into one signed,
//! self-contained package file; `verify` materializes that file's payloads
//! into a real [`ArtifactStore`], bootstraps the signer principal into a
//! real [`IdentityAuthority`], and runs the crate's authoritative
//! `verify_package` / `verify_package_with_tasks` pipeline — the same path
//! the kernel consumes. `keygen` derives a deterministic developer signing
//! key descriptor from caller-supplied seed material.
//!
//! # Usage
//!
//! ```text
//! nlos-package keygen --seed <HEX64> [--out <KEYFILE>]
//! nlos-package build <DIR> --key <KEYFILE> [--out <PKGFILE>]
//! nlos-package verify <PKGFILE> [--store <DIR>] [--identity <DIR>] [--at-ms <U64>]
//! ```
//!
//! # Determinism
//!
//! The package file is canonical length-prefixed framing (u64 BE fields,
//! mirroring the manifest message framing). No wall-clock input exists on
//! the build path, so identical trees signed with the same key produce
//! byte-identical package files and digests.
//!
//! # Exit codes
//!
//! `0` success · `1` usage · `2` malformed input (manifest, key file,
//! package file, shape) · `3` signature/identity verification failure ·
//! `4` content-binding failure (tampered payload) · `5` internal I/O or
//! store failure.
//!
//! # Trust boundary (dev toolchain only)
//!
//! `keygen` keys are developer convenience keys: `verify` bootstraps the
//! signer principal from the descriptor embedded in the package file, which
//! proves the signature matches that key — it is NOT a trust decision.
//! Production signing-key custody, trust roots, and signature chains are
//! deployment concerns outside this CLI (see docs/developers/packaging.md).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ed25519_dalek::{Signer, SigningKey};
use nlos_artifact::{
    ArtifactError, ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole,
    PackageManifest, PackageManifestEntry, PackageTaskKind, PackageTaskTemplate,
    PackageVerificationDecision, ProvenanceSourceTriple, PutRevisionRequest, SignedPackage,
    SignedPackageWithTasks, VerifyPackageRequest, VerifyPackageWithTasksRequest,
    package_manifest_message, package_manifest_with_tasks_message, validate_task_templates,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_types::{ArtifactId, IdempotencyKey, PackageId};
use sha2::{Digest, Sha256};

const USAGE: &str = "usage: nlos-package keygen --seed <HEX64> [--out <KEYFILE>] \
 | build <DIR> --key <KEYFILE> [--out <PKGFILE>] \
 | verify <PKGFILE> [--store <DIR>] [--identity <DIR>] [--at-ms <U64>]";

/// Domain separators for CLI-owned derivations; each derivation is plain
/// SHA-256 over `domain ‖ input` (the crate's receipt-id precedent).
const ARTIFACT_ID_DOMAIN: &[u8] = b"llmos/package-file/artifact-id/v1";
const CREATE_KEY_DOMAIN: &[u8] = b"llmos/package-file/create-key/v1";
const VERIFY_KEY_DOMAIN: &[u8] = b"llmos/package-file/verify-key/v1";
const KEYGEN_PROFILE_DOMAIN: &[u8] = b"llmos/package-keygen/profile/v1";
const KEYGEN_POLICY_DOMAIN: &[u8] = b"llmos/package-keygen/policy/v1";
const KEYGEN_BOOTSTRAP_DOMAIN: &[u8] = b"llmos/package-keygen/bootstrap-key/v1";

/// Developer-manifest and package-file entry-name bound; mirrors the
/// artifact authority's `MAX_TEXT_COMPONENT_BYTES`.
const MAX_ENTRY_NAME_BYTES: usize = 255;

/// Typed CLI failure, mapped onto the documented exit codes.
#[derive(Debug)]
enum ToolError {
    /// Bad invocation (exit 1).
    Usage,
    /// Malformed input: manifest, key file, package file, shape (exit 2).
    Input(String),
    /// Signature or identity verification failure (exit 3).
    Signature(String),
    /// Content-binding failure, e.g. tampered payload (exit 4).
    Binding(String),
    /// Internal I/O or store failure (exit 5).
    Internal(String),
}

impl ToolError {
    fn input(context: &str, detail: &str) -> Self {
        Self::Input(format!("{context}: {detail}"))
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::Usage => 1,
            Self::Input(_) => 2,
            Self::Signature(_) => 3,
            Self::Binding(_) => 4,
            Self::Internal(_) => 5,
        }
    }
}

fn from_artifact_error(error: ArtifactError) -> ToolError {
    match error {
        ArtifactError::PackageSignatureInvalid => {
            ToolError::Signature("package signature invalid".to_string())
        }
        ArtifactError::PackagePrincipalUnknown(id) => {
            ToolError::Signature(format!("signer principal unknown: {}", hex(id.as_bytes())))
        }
        ArtifactError::PackageKeyRevoked => ToolError::Signature("signer key revoked".to_string()),
        ArtifactError::PackageIdentity(source) => {
            ToolError::Signature(format!("identity authority: {source}"))
        }
        ArtifactError::IdempotencyConflict => ToolError::Signature(
            "idempotency key reused with a different request shape".to_string(),
        ),
        ArtifactError::PackageTampered { entry, .. } => ToolError::Binding(format!(
            "entry {entry:?} does not match its declared digest"
        )),
        ArtifactError::ArtifactNotFound(id) => {
            ToolError::Binding(format!("entry artifact unknown: {}", hex(id.as_bytes())))
        }
        ArtifactError::PackageManifestInvalid(reason) => {
            ToolError::input("package manifest invalid", reason)
        }
        other => ToolError::Internal(other.to_string()),
    }
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(operation) = arguments.first().cloned() else {
        eprintln!("{USAGE}");
        return ExitCode::from(1);
    };
    let result = match operation.as_str() {
        "keygen" => keygen_command(&arguments[1..]),
        "build" => build_command(&arguments[1..]),
        "verify" => verify_command(&arguments[1..]),
        _ => Err(ToolError::Usage),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(ToolError::Usage) => {
            eprintln!("{USAGE}");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("nlos-package: {}", error.render());
            ExitCode::from(error.exit_code())
        }
    }
}

impl ToolError {
    fn render(&self) -> String {
        match self {
            Self::Usage => "bad invocation".to_string(),
            Self::Input(text)
            | Self::Signature(text)
            | Self::Binding(text)
            | Self::Internal(text) => text.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// keygen
// ---------------------------------------------------------------------------

/// The developer signing-key descriptor: everything needed to (a) sign at
/// build time and (b) deterministically bootstrap the identical signer
/// principal inside any `nlos-identity` authority at verify time. The
/// principal id itself is NOT stored — it is always derived by the identity
/// authority from the descriptor (single derivation authority).
struct DevKey {
    seed: [u8; 32],
    profile_digest: [u8; 32],
    policy_digest: [u8; 32],
    bootstrap_key: [u8; 16],
    valid_from_ms: u64,
    valid_until_ms: u64,
}

impl DevKey {
    fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.seed)
    }

    fn bootstrap_request(&self) -> BootstrapPrincipalRequest {
        SignerDescriptor {
            public_key: self.signing_key().verifying_key().to_bytes(),
            profile_digest: self.profile_digest,
            policy_digest: self.policy_digest,
            bootstrap_key: self.bootstrap_key,
            valid_from_ms: self.valid_from_ms,
            valid_until_ms: self.valid_until_ms,
        }
        .bootstrap_request()
    }

    /// The public half that travels inside the package file: enough to
    /// bootstrap the identical signer principal at verify time, never the
    /// seed.
    fn public_descriptor(&self) -> SignerDescriptor {
        SignerDescriptor {
            public_key: self.signing_key().verifying_key().to_bytes(),
            profile_digest: self.profile_digest,
            policy_digest: self.policy_digest,
            bootstrap_key: self.bootstrap_key,
            valid_from_ms: self.valid_from_ms,
            valid_until_ms: self.valid_until_ms,
        }
    }

    fn encode(&self) -> String {
        format!(
            "# nlos-package dev signing key v1 — SECRET; never commit or ship\n\
             seed = {}\n\
             principal-profile-digest = {}\n\
             control-domain-policy-digest = {}\n\
             bootstrap-idempotency-key = {}\n\
             key-valid-from-ms = {}\n\
             key-valid-until-ms = {}\n",
            hex(&self.seed),
            hex(&self.profile_digest),
            hex(&self.policy_digest),
            hex(&self.bootstrap_key),
            self.valid_from_ms,
            self.valid_until_ms,
        )
    }
}

fn keygen_command(arguments: &[String]) -> Result<(), ToolError> {
    let mut seed = None;
    let mut out = None;
    parse_flags(
        arguments,
        &mut |flag, value| {
            match flag {
                "seed" => seed = Some(value.to_string()),
                "out" => out = Some(value.to_string()),
                _ => return false,
            }
            true
        },
        &mut |_| false,
    )?;
    let Some(seed) = seed else {
        return Err(ToolError::Usage);
    };
    let Some(seed) = hex_array::<32>(&seed) else {
        return Err(ToolError::input(
            "keygen",
            "seed must be exactly 64 hex chars",
        ));
    };

    let public_key = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    let key = DevKey {
        seed,
        profile_digest: derive_32(KEYGEN_PROFILE_DOMAIN, &public_key),
        policy_digest: derive_32(KEYGEN_POLICY_DOMAIN, &public_key),
        bootstrap_key: derive_16(KEYGEN_BOOTSTRAP_DOMAIN, &public_key),
        // i64::MAX (≈ year 2262): the widest validity window the durable
        // identity ledger can store; u64::MAX would not round-trip SQLite.
        valid_from_ms: 0,
        valid_until_ms: u64::try_from(i64::MAX).expect("i64::MAX fits u64"),
    };

    let principal = derive_principal(&key)?;

    let path = out.unwrap_or_else(|| "nlos-package.devkey".to_string());
    fs::write(&path, key.encode())
        .map_err(|error| ToolError::Internal(format!("write key file {path}: {error}")))?;
    restrict_permissions(&path);

    println!("KEYGEN {path}");
    println!("principal {}", hex(principal.as_bytes()));
    println!("public_key {}", hex(&public_key));
    Ok(())
}

/// Derives the signer principal id by bootstrapping (or replaying) the
/// descriptor inside a throwaway identity authority — the authority is the
/// single owner of the derivation, so the CLI never duplicates it.
fn derive_principal(key: &DevKey) -> Result<nlos_types::PrincipalId, ToolError> {
    let temp = TempDir::new("keygen-identity").map_err(ToolError::Internal)?;
    let authority = IdentityAuthority::open(temp.root())
        .map_err(|error| ToolError::Internal(format!("open identity authority: {error}")))?;
    let decision = authority
        .bootstrap_principal(key.bootstrap_request())
        .map_err(|error| ToolError::input("bootstrap signer principal", &error.to_string()))?;
    Ok(decision.binding().principal_id)
}

fn restrict_permissions(path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

struct DevEntry {
    name: String,
    role: PackageEntryRole,
    path: String,
}

struct DevTask {
    node_key: [u8; 16],
    kind: PackageTaskKind,
    binding_path: String,
    inputs_path: String,
    outputs_path: String,
    policy_path: String,
    ceiling_path: String,
    dependency_keys: Vec<[u8; 16]>,
}

struct DevManifest {
    package_id: PackageId,
    version: u64,
    entries: Vec<DevEntry>,
    tasks: Vec<DevTask>,
}

fn build_command(arguments: &[String]) -> Result<(), ToolError> {
    let mut directory = None;
    let mut key_path = None;
    let mut out = None;
    parse_flags(
        arguments,
        &mut |flag, value| {
            match flag {
                "key" => key_path = Some(value.to_string()),
                "out" => out = Some(value.to_string()),
                _ => return false,
            }
            true
        },
        &mut |token| {
            if directory.is_none() {
                directory = Some(token.to_string());
                true
            } else {
                false
            }
        },
    )?;
    let Some(directory) = directory else {
        return Err(ToolError::Usage);
    };
    let Some(key_path) = key_path else {
        return Err(ToolError::Usage);
    };

    let key = read_dev_key(&key_path)?;
    let tree = Path::new(&directory);
    let manifest = read_dev_manifest(tree)?;

    let entries = build_entries(tree, &manifest)?;
    let tasks = build_templates(tree, &manifest)?;
    if !tasks.is_empty() {
        validate_task_templates(&tasks)
            .map_err(|error| ToolError::input("task segment", &error.to_string()))?;
    }

    let package_manifest = manifest_of(&manifest.package_id, manifest.version, &entries);
    let manifest_digest = if tasks.is_empty() {
        package_manifest_message(&package_manifest)
    } else {
        package_manifest_with_tasks_message(&package_manifest, &tasks)
    };
    let signer = derive_principal(&key)?;
    let signature = key.signing_key().sign(&manifest_digest).to_bytes();

    let package = PackageFile {
        descriptor: key.public_descriptor(),
        package_id: manifest.package_id,
        version: manifest.version,
        entries,
        tasks,
        signature,
    };
    let path = out.unwrap_or_else(|| {
        format!(
            "{}.v{}.nlospkg",
            hex(package.package_id.as_bytes()),
            package.version
        )
    });
    fs::write(&path, package.encode())
        .map_err(|error| ToolError::Internal(format!("write package {path}: {error}")))?;

    println!("BUILT {path}");
    println!(
        "package {} version {}",
        hex(package.package_id.as_bytes()),
        package.version
    );
    println!("manifest_digest {}", hex(&manifest_digest));
    println!("signer {}", hex(signer.as_bytes()));
    println!(
        "entries {} tasks {}",
        package.entries.len(),
        package.tasks.len()
    );
    Ok(())
}

fn build_entries(tree: &Path, manifest: &DevManifest) -> Result<Vec<PackageFileEntry>, ToolError> {
    let mut entries = Vec::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let payload = read_tree_file(tree, &entry.path)?;
        entries.push(PackageFileEntry {
            name: entry.name.clone(),
            role: entry.role,
            artifact_id: derive_artifact_id(manifest.package_id, manifest.version, &entry.name),
            digest: ContentDigest::of_bytes(&payload).into_bytes(),
            payload,
        });
    }
    Ok(entries)
}

fn build_templates(
    tree: &Path,
    manifest: &DevManifest,
) -> Result<Vec<PackageTaskTemplate>, ToolError> {
    let mut templates = Vec::with_capacity(manifest.tasks.len());
    for task in &manifest.tasks {
        templates.push(PackageTaskTemplate {
            node_key: task.node_key,
            kind: task.kind,
            binding_digest: digest_of_tree_file(tree, &task.binding_path)?,
            dependency_keys: task.dependency_keys.clone(),
            input_selectors_digest: digest_of_tree_file(tree, &task.inputs_path)?,
            output_contract_digest: digest_of_tree_file(tree, &task.outputs_path)?,
            policy_digest: digest_of_tree_file(tree, &task.policy_path)?,
            resource_ceiling_digest: digest_of_tree_file(tree, &task.ceiling_path)?,
        });
    }
    Ok(templates)
}

fn manifest_of(
    package_id: &PackageId,
    version: u64,
    entries: &[PackageFileEntry],
) -> PackageManifest {
    PackageManifest {
        package_id: *package_id,
        version,
        entries: entries
            .iter()
            .map(|entry| PackageManifestEntry {
                name: entry.name.clone(),
                artifact_id: ArtifactId::from_bytes(entry.artifact_id),
                digest: ContentDigest::from_bytes(entry.digest),
                role: entry.role,
            })
            .collect(),
    }
}

fn read_dev_key(path: &str) -> Result<DevKey, ToolError> {
    let text = fs::read_to_string(path)
        .map_err(|error| ToolError::input("read key file", &format!("{path}: {error}")))?;
    parse_dev_key(&text)
}

fn parse_dev_key(text: &str) -> Result<DevKey, ToolError> {
    let context = "key file";
    let mut seed = None;
    let mut profile = None;
    let mut policy = None;
    let mut bootstrap = None;
    let mut from = None;
    let mut until = None;
    let mut encountered: Vec<String> = Vec::new();
    for line in numbered_lines(text) {
        let (key, value) = scalar_line(&line).map_err(|error| ToolError::input(context, &error))?;
        if encountered.iter().any(|existing| existing == &key) {
            return Err(ToolError::input(context, &format!("duplicate {key}")));
        }
        encountered.push(key.clone());
        match key.as_str() {
            "seed" => seed = hex_array::<32>(&value),
            "principal-profile-digest" => profile = hex_array::<32>(&value),
            "control-domain-policy-digest" => policy = hex_array::<32>(&value),
            "bootstrap-idempotency-key" => bootstrap = hex_array::<16>(&value),
            "key-valid-from-ms" => from = value.parse::<u64>().ok(),
            "key-valid-until-ms" => until = value.parse::<u64>().ok(),
            other => {
                return Err(ToolError::input(context, &format!("unknown key {other:?}")));
            }
        }
    }
    let seed = seed.ok_or_else(|| ToolError::input(context, "missing seed"))?;
    let profile = profile.ok_or_else(|| ToolError::input(context, "missing profile digest"))?;
    let policy = policy.ok_or_else(|| ToolError::input(context, "missing policy digest"))?;
    let bootstrap = bootstrap.ok_or_else(|| ToolError::input(context, "missing bootstrap key"))?;
    let from = from.ok_or_else(|| ToolError::input(context, "missing valid-from"))?;
    let until = until.ok_or_else(|| ToolError::input(context, "missing valid-until"))?;
    if from > until {
        return Err(ToolError::input(context, "valid-from after valid-until"));
    }
    Ok(DevKey {
        seed,
        profile_digest: profile,
        policy_digest: policy,
        bootstrap_key: bootstrap,
        valid_from_ms: from,
        valid_until_ms: until,
    })
}

fn read_dev_manifest(directory: &Path) -> Result<DevManifest, ToolError> {
    let path = directory.join("package.manifest");
    let text = fs::read_to_string(&path).map_err(|error| {
        ToolError::input(
            "read developer manifest",
            &format!("{}: {error}", path.display()),
        )
    })?;
    parse_dev_manifest(&text)
}

fn parse_dev_manifest(text: &str) -> Result<DevManifest, ToolError> {
    let context = "developer manifest";
    let mut package_id = None;
    let mut version = None;
    let mut entries: Vec<DevEntry> = Vec::new();
    let mut tasks: Vec<DevTask> = Vec::new();
    let mut seen_scalars: Vec<String> = Vec::new();

    for line in numbered_lines(text) {
        let (key, value) = scalar_line(&line).map_err(|error| ToolError::input(context, &error))?;
        // `entry`/`task` are repeatable records; only true scalars dedup.
        if key != "entry" && key != "task" {
            if seen_scalars.iter().any(|seen| seen == &key) {
                return Err(ToolError::input(context, &format!("duplicate {key}")));
            }
            seen_scalars.push(key.clone());
        }
        match key.as_str() {
            "package-id" => {
                package_id = hex_array::<16>(&value).map(PackageId::from_bytes);
            }
            "version" => version = parse_version(&value),
            "entry" => {
                entries.push(
                    parse_entry_line(&value).map_err(|error| ToolError::input(context, &error))?,
                );
            }
            "task" => {
                tasks.push(
                    parse_task_line(&value).map_err(|error| ToolError::input(context, &error))?,
                );
            }
            other => {
                return Err(ToolError::input(context, &format!("unknown key {other:?}")));
            }
        }
    }

    let package_id = package_id.ok_or_else(|| ToolError::input(context, "missing package-id"))?;
    let version = version.ok_or_else(|| ToolError::input(context, "missing version"))?;
    if entries.is_empty() {
        return Err(ToolError::input(context, "at least one entry is required"));
    }
    let mut names = std::collections::HashSet::with_capacity(entries.len());
    for entry in &entries {
        let valid = !entry.name.is_empty()
            && entry.name.len() <= MAX_ENTRY_NAME_BYTES
            && !entry.name.contains('\0');
        if !valid {
            return Err(ToolError::input(
                context,
                &format!(
                    "entry name {:?} is empty, oversized, or NUL-bearing",
                    entry.name
                ),
            ));
        }
        if !names.insert(entry.name.as_str()) {
            return Err(ToolError::input(
                context,
                &format!("duplicate entry name {:?}", entry.name),
            ));
        }
    }
    Ok(DevManifest {
        package_id,
        version,
        entries,
        tasks,
    })
}

fn parse_entry_line(value: &str) -> Result<DevEntry, String> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 3 {
        return Err("entry = <name> <role> <path>".to_string());
    }
    let role = match tokens[1] {
        "executable" => PackageEntryRole::Executable,
        "background-service" => PackageEntryRole::BackgroundService,
        "data" => PackageEntryRole::Data,
        other => return Err(format!("unknown role {other:?}")),
    };
    Ok(DevEntry {
        name: tokens[0].to_string(),
        role,
        path: tokens[2].to_string(),
    })
}

fn parse_task_line(value: &str) -> Result<DevTask, String> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 7 && tokens.len() != 8 {
        return Err(concat!(
            "task = <node-key-HEX32> <agent-role|executable> <binding-file> ",
            "<inputs-file> <outputs-file> <policy-file> <ceiling-file> [deps=<HEX32,...>]",
        )
        .to_string());
    }
    let Some(node_key) = hex_array::<16>(tokens[0]) else {
        return Err("node key must be exactly 32 hex chars".to_string());
    };
    let kind = match tokens[1] {
        "agent-role" => PackageTaskKind::AgentRole,
        "executable" => PackageTaskKind::Executable,
        other => return Err(format!("unknown kind {other:?}")),
    };
    let mut dependency_keys = Vec::new();
    if tokens.len() == 8 {
        let Some(list) = tokens[7].strip_prefix("deps=") else {
            return Err("eighth field must be deps=<HEX32,...>".to_string());
        };
        for item in list.split(',') {
            let Some(dependency) = hex_array::<16>(item) else {
                return Err(format!("dependency {item:?} must be exactly 32 hex chars"));
            };
            dependency_keys.push(dependency);
        }
    }
    Ok(DevTask {
        node_key,
        kind,
        binding_path: tokens[2].to_string(),
        inputs_path: tokens[3].to_string(),
        outputs_path: tokens[4].to_string(),
        policy_path: tokens[5].to_string(),
        ceiling_path: tokens[6].to_string(),
        dependency_keys,
    })
}

/// `version = <u64>` or a dotted `<major>.<minor>.<patch>` packed with the
/// same shift formula as `nlos_application::pack_package_version` (major in
/// the upper 32 bits, minor the next 16, patch the low 16). The formula is
/// mirrored here because `nlos-artifact` cannot depend on
/// `nlos-application` (the dependency points the other way).
fn parse_version(value: &str) -> Option<u64> {
    if value.contains('.') {
        let mut parts = value.split('.');
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next()?.parse().ok()?;
        let patch: u32 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some((u64::from(major) << 32) | (u64::from(minor) << 16) | u64::from(patch))
    } else {
        value.parse::<u64>().ok()
    }
}

fn read_tree_file(base: &Path, relative: &str) -> Result<Vec<u8>, ToolError> {
    let path = safe_join(base, relative)?;
    fs::read(&path).map_err(|error| {
        ToolError::input("read tree file", &format!("{}: {error}", path.display()))
    })
}

fn digest_of_tree_file(base: &Path, relative: &str) -> Result<[u8; 32], ToolError> {
    let bytes = read_tree_file(base, relative)?;
    Ok(ContentDigest::of_bytes(&bytes).into_bytes())
}

/// Joins a manifest-declared relative path, refusing absolute paths and
/// parent traversal so a developer tree cannot read outside itself.
fn safe_join(base: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    let candidate = Path::new(relative);
    let escapes = candidate.is_absolute()
        || relative.is_empty()
        || relative.len() > 4096
        || relative.contains('\0')
        || candidate.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        });
    if escapes {
        return Err(ToolError::input(
            "tree path",
            &format!("{relative:?} must be a plain relative path"),
        ));
    }
    Ok(base.join(candidate))
}

// ---------------------------------------------------------------------------
// Package file codec
// ---------------------------------------------------------------------------

const PACKAGE_FILE_MAGIC: &[u8] = b"nlos/package-file/v1";

/// Public signer descriptor embedded in every package file: the exact
/// material `nlos-identity` needs to bootstrap (or replay) the signer
/// principal at verify time. No secret travels with the package.
struct SignerDescriptor {
    public_key: [u8; 32],
    profile_digest: [u8; 32],
    policy_digest: [u8; 32],
    bootstrap_key: [u8; 16],
    valid_from_ms: u64,
    valid_until_ms: u64,
}

impl SignerDescriptor {
    fn bootstrap_request(&self) -> BootstrapPrincipalRequest {
        BootstrapPrincipalRequest {
            principal_profile_digest: self.profile_digest,
            control_domain_policy_digest: self.policy_digest,
            public_key: self.public_key,
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: self.valid_from_ms,
            key_valid_until_ms: self.valid_until_ms,
            idempotency_key: IdempotencyKey::from_bytes(self.bootstrap_key),
            created_at_ms: self.valid_from_ms,
        }
    }
}

struct PackageFileEntry {
    name: String,
    role: PackageEntryRole,
    artifact_id: [u8; 16],
    digest: [u8; 32],
    payload: Vec<u8>,
}

struct PackageFile {
    descriptor: SignerDescriptor,
    package_id: PackageId,
    version: u64,
    entries: Vec<PackageFileEntry>,
    tasks: Vec<PackageTaskTemplate>,
    signature: [u8; 64],
}

impl PackageFile {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PACKAGE_FILE_MAGIC);
        bytes.extend_from_slice(&self.descriptor.public_key);
        bytes.extend_from_slice(&self.descriptor.profile_digest);
        bytes.extend_from_slice(&self.descriptor.policy_digest);
        bytes.extend_from_slice(&self.descriptor.bootstrap_key);
        bytes.extend_from_slice(&self.descriptor.valid_from_ms.to_be_bytes());
        bytes.extend_from_slice(&self.descriptor.valid_until_ms.to_be_bytes());
        bytes.extend_from_slice(self.package_id.as_bytes());
        bytes.extend_from_slice(&self.version.to_be_bytes());
        extend_count(&mut bytes, self.entries.len());
        for entry in &self.entries {
            extend_count(&mut bytes, entry.name.len());
            bytes.extend_from_slice(entry.name.as_bytes());
            bytes.push(entry.role.encode());
            bytes.extend_from_slice(&entry.artifact_id);
            bytes.extend_from_slice(&entry.digest);
            extend_count(&mut bytes, entry.payload.len());
            bytes.extend_from_slice(&entry.payload);
        }
        extend_count(&mut bytes, self.tasks.len());
        for task in &self.tasks {
            bytes.extend_from_slice(&task.node_key);
            bytes.push(task.kind.encode());
            bytes.extend_from_slice(&task.binding_digest);
            extend_count(&mut bytes, task.dependency_keys.len());
            for dependency in &task.dependency_keys {
                bytes.extend_from_slice(dependency);
            }
            bytes.extend_from_slice(&task.input_selectors_digest);
            bytes.extend_from_slice(&task.output_contract_digest);
            bytes.extend_from_slice(&task.policy_digest);
            bytes.extend_from_slice(&task.resource_ceiling_digest);
        }
        bytes.extend_from_slice(&self.signature);
        bytes
    }
}

fn extend_count(bytes: &mut Vec<u8>, count: usize) {
    bytes.extend_from_slice(&u64::try_from(count).unwrap_or(u64::MAX).to_be_bytes());
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| format!("truncated package file at {what}"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn take_array<const N: usize>(&mut self, what: &str) -> Result<[u8; N], String> {
        let mut array = [0_u8; N];
        array.copy_from_slice(self.take(N, what)?);
        Ok(array)
    }

    fn take_count(&mut self, what: &str) -> Result<usize, String> {
        let raw = u64::from_be_bytes(self.take_array::<8>(what)?);
        usize::try_from(raw).map_err(|_| format!("{what} count exceeds platform usize"))
    }

    fn finish(self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "trailing bytes after signature: {}",
                self.bytes.len() - self.position
            ))
        }
    }
}

fn decode_package_file(bytes: &[u8]) -> Result<PackageFile, ToolError> {
    let context = "package file";
    let mut decoder = Decoder::new(bytes);
    let fail = |error: String| ToolError::input(context, &error);

    let magic = decoder
        .take(PACKAGE_FILE_MAGIC.len(), "magic")
        .map_err(fail)?;
    if magic != PACKAGE_FILE_MAGIC {
        return Err(fail("bad magic".to_string()));
    }
    let descriptor = SignerDescriptor {
        public_key: decoder.take_array("signer public key").map_err(fail)?,
        profile_digest: decoder.take_array("profile digest").map_err(fail)?,
        policy_digest: decoder.take_array("policy digest").map_err(fail)?,
        bootstrap_key: decoder.take_array("bootstrap key").map_err(fail)?,
        valid_from_ms: u64::from_be_bytes(decoder.take_array("key valid-from").map_err(fail)?),
        valid_until_ms: u64::from_be_bytes(decoder.take_array("key valid-until").map_err(fail)?),
    };
    if descriptor.valid_from_ms > descriptor.valid_until_ms {
        return Err(fail("signer key validity window is empty".to_string()));
    }
    let package_id = PackageId::from_bytes(decoder.take_array("package id").map_err(fail)?);
    let version = u64::from_be_bytes(decoder.take_array("version").map_err(fail)?);

    let entry_count = decoder.take_count("entry count").map_err(fail)?;
    let mut entries = Vec::new();
    for _ in 0..entry_count {
        let name_len = decoder.take_count("entry name length").map_err(fail)?;
        if name_len == 0 || name_len > MAX_ENTRY_NAME_BYTES {
            return Err(fail("entry name length out of bounds".to_string()));
        }
        let name_bytes = decoder.take(name_len, "entry name").map_err(fail)?;
        let name = String::from_utf8(name_bytes.to_vec())
            .map_err(|_| fail("entry name is not UTF-8".to_string()))?;
        if name.contains('\0') {
            return Err(fail("entry name contains NUL".to_string()));
        }
        let role_byte = decoder.take_array::<1>("entry role").map_err(fail)?[0];
        let role = match role_byte {
            1 => PackageEntryRole::Executable,
            2 => PackageEntryRole::BackgroundService,
            3 => PackageEntryRole::Data,
            _ => return Err(fail(format!("unknown entry role byte {role_byte}"))),
        };
        let artifact_id = decoder.take_array("entry artifact id").map_err(fail)?;
        let digest = decoder.take_array("entry digest").map_err(fail)?;
        let payload_len = decoder.take_count("entry payload length").map_err(fail)?;
        let payload = decoder
            .take(payload_len, "entry payload")
            .map_err(fail)?
            .to_vec();
        entries.push(PackageFileEntry {
            name,
            role,
            artifact_id,
            digest,
            payload,
        });
    }

    let task_count = decoder.take_count("task count").map_err(fail)?;
    let mut tasks = Vec::new();
    for _ in 0..task_count {
        let node_key = decoder.take_array("task node key").map_err(fail)?;
        let kind_byte = decoder.take_array::<1>("task kind").map_err(fail)?[0];
        let kind = match kind_byte {
            1 => PackageTaskKind::AgentRole,
            2 => PackageTaskKind::Executable,
            _ => return Err(fail(format!("unknown task kind byte {kind_byte}"))),
        };
        let binding_digest = decoder.take_array("task binding").map_err(fail)?;
        let dependency_count = decoder.take_count("task dependency count").map_err(fail)?;
        let mut dependency_keys = Vec::with_capacity(dependency_count);
        for _ in 0..dependency_count {
            dependency_keys.push(decoder.take_array("task dependency").map_err(fail)?);
        }
        tasks.push(PackageTaskTemplate {
            node_key,
            kind,
            binding_digest,
            dependency_keys,
            input_selectors_digest: decoder.take_array("task inputs").map_err(fail)?,
            output_contract_digest: decoder.take_array("task outputs").map_err(fail)?,
            policy_digest: decoder.take_array("task policy").map_err(fail)?,
            resource_ceiling_digest: decoder.take_array("task ceiling").map_err(fail)?,
        });
    }

    let signature = decoder.take_array("signature").map_err(fail)?;
    decoder.finish().map_err(fail)?;
    Ok(PackageFile {
        descriptor,
        package_id,
        version,
        entries,
        tasks,
        signature,
    })
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

fn verify_command(arguments: &[String]) -> Result<(), ToolError> {
    let mut package_path = None;
    let mut store = None;
    let mut identity = None;
    let mut at_ms = 0_u64;
    parse_flags(
        arguments,
        &mut |flag, value| {
            match flag {
                "store" => store = Some(value.to_string()),
                "identity" => identity = Some(value.to_string()),
                "at-ms" => {
                    let Some(parsed) = value.parse::<u64>().ok() else {
                        return false;
                    };
                    at_ms = parsed;
                }
                _ => return false,
            }
            true
        },
        &mut |token| {
            if package_path.is_none() {
                package_path = Some(token.to_string());
                true
            } else {
                false
            }
        },
    )?;
    let Some(package_path) = package_path else {
        return Err(ToolError::Usage);
    };

    let bytes = fs::read(&package_path)
        .map_err(|error| ToolError::input("read package", &format!("{package_path}: {error}")))?;
    let package = decode_package_file(&bytes)?;

    let mut temporary: Vec<TempDir> = Vec::new();
    let store_directory = ensure_root(store.as_deref(), "verify-store", &mut temporary)?;
    let identity_directory = ensure_root(identity.as_deref(), "verify-identity", &mut temporary)?;

    let artifact_store = ArtifactStore::open(&store_directory)
        .map_err(|error| ToolError::Internal(format!("open artifact store: {error}")))?;
    let authority = IdentityAuthority::open(&identity_directory)
        .map_err(|error| ToolError::Internal(format!("open identity authority: {error}")))?;

    let signer = authority
        .bootstrap_principal(package.descriptor.bootstrap_request())
        .map_err(|error| ToolError::input("bootstrap signer principal", &error.to_string()))?
        .binding()
        .principal_id;

    for entry in &package.entries {
        materialize_entry(&artifact_store, &package, entry, at_ms)?;
    }

    let decision = verify_signed(&artifact_store, &authority, &package, signer, at_ms)?;
    print_decision(&decision);
    Ok(())
}

/// Resolves a store root: the caller's override, or a fresh temp root kept
/// alive (and cleaned up) via `temporary`.
fn ensure_root(
    path: Option<&str>,
    tag: &str,
    temporary: &mut Vec<TempDir>,
) -> Result<PathBuf, ToolError> {
    if let Some(path) = path {
        Ok(PathBuf::from(path))
    } else {
        let directory = TempDir::new(tag).map_err(ToolError::Internal)?;
        let root = directory.root().to_path_buf();
        temporary.push(directory);
        Ok(root)
    }
}

fn verify_signed(
    store: &ArtifactStore,
    authority: &IdentityAuthority,
    package: &PackageFile,
    signer: nlos_types::PrincipalId,
    at_ms: u64,
) -> Result<PackageVerificationDecision, ToolError> {
    let manifest = manifest_of(&package.package_id, package.version, &package.entries);
    let message_digest = if package.tasks.is_empty() {
        package_manifest_message(&manifest)
    } else {
        package_manifest_with_tasks_message(&manifest, &package.tasks)
    };
    let idempotency_key = verify_idempotency_key(message_digest);
    if package.tasks.is_empty() {
        let legacy = SignedPackage {
            manifest,
            signer,
            signature: package.signature,
        };
        store
            .verify_package(
                authority,
                VerifyPackageRequest {
                    signed: &legacy,
                    idempotency_key,
                    verified_at_ms: at_ms,
                },
            )
            .map_err(from_artifact_error)
    } else {
        let templated = SignedPackageWithTasks {
            manifest,
            tasks: package.tasks.clone(),
            signer,
            signature: package.signature,
        };
        store
            .verify_package_with_tasks(
                authority,
                VerifyPackageWithTasksRequest {
                    signed: &templated,
                    idempotency_key,
                    verified_at_ms: at_ms,
                },
            )
            .map_err(from_artifact_error)
    }
}

fn print_decision(decision: &PackageVerificationDecision) {
    let outcome = if matches!(decision, PackageVerificationDecision::Verified(_)) {
        "VERIFIED"
    } else {
        "REPLAYED"
    };
    let receipt = decision.receipt();
    println!("{} {}", outcome, hex(receipt.receipt_id.as_bytes()));
    println!(
        "manifest_digest {}",
        hex(receipt.manifest_digest.as_bytes())
    );
    println!(
        "package {} version {}",
        hex(receipt.package_id.as_bytes()),
        receipt.package_version
    );
    println!(
        "signer {} key {}",
        hex(receipt.signer.as_bytes()),
        hex(receipt.key_id.as_bytes())
    );
}

/// Materializes one package entry into the artifact store exactly as an
/// ingest path would: create (idempotent by a derived key) then put
/// revision 0 (replays when identical content is already the head).
fn materialize_entry(
    store: &ArtifactStore,
    package: &PackageFile,
    entry: &PackageFileEntry,
    at_ms: u64,
) -> Result<(), ToolError> {
    let artifact_id = ArtifactId::from_bytes(entry.artifact_id);
    store
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes(derive_16(
                CREATE_KEY_DOMAIN,
                &entry.artifact_id,
            )),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms: at_ms,
        })
        .map_err(from_artifact_error)?;
    store
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &entry.payload,
            created_at_ms: at_ms,
            provenance: ProvenanceSourceTriple {
                source_a: entry.artifact_id,
                source_b: *package.package_id.as_bytes(),
                source_digest: ContentDigest::from_bytes(entry.digest),
            },
        })
        .map_err(from_artifact_error)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Argument scanner: every flag takes exactly one value (`--flag value`
/// with the value never starting with `--`); bare tokens are positionals.
/// A callback returning `false` is a usage failure.
fn parse_flags(
    arguments: &[String],
    on_flag: &mut dyn FnMut(&str, &str) -> bool,
    on_positional: &mut dyn FnMut(&str) -> bool,
) -> Result<(), ToolError> {
    let mut index = 0;
    while index < arguments.len() {
        let token = arguments[index].as_str();
        if let Some(flag) = token.strip_prefix("--") {
            let Some(value) = arguments.get(index + 1) else {
                return Err(ToolError::Usage);
            };
            if value.starts_with("--") || !on_flag(flag, value) {
                return Err(ToolError::Usage);
            }
            index += 2;
        } else if !on_positional(token) {
            return Err(ToolError::Usage);
        } else {
            index += 1;
        }
    }
    Ok(())
}

/// Iterates `(line number, trimmed content)` skipping blanks and `#`
/// comments — the shared line discipline of the manifest and key formats.
fn numbered_lines(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.trim().to_string()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// Splits one `key = value` line (both sides trimmed).
fn scalar_line(line: &(usize, String)) -> Result<(String, String), String> {
    let Some((key, value)) = line.1.split_once('=') else {
        return Err("expected `key = value`".to_string());
    };
    let key = key.trim();
    if key.is_empty() {
        return Err("empty key".to_string());
    }
    Ok((key.to_string(), value.trim().to_string()))
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn hex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut array = [0_u8; N];
    for (index, slot) in array.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(array)
}

fn derive_16(domain: &[u8], input: &[u8]) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&derive_32(domain, input)[..16]);
    bytes
}

fn derive_32(domain: &[u8], input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(input);
    hasher.finalize().into()
}

fn derive_artifact_id(package_id: PackageId, version: u64, name: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(ARTIFACT_ID_DOMAIN);
    hasher.update(package_id.as_bytes());
    hasher.update(version.to_be_bytes());
    hasher.update(u64::try_from(name.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes
}

/// The verify idempotency key is derived from the manifest digest, so
/// re-verifying the same package against a persistent store replays the
/// same durable receipt instead of committing a second one.
fn verify_idempotency_key(manifest_digest: [u8; 32]) -> IdempotencyKey {
    IdempotencyKey::from_bytes(derive_16(VERIFY_KEY_DOMAIN, &manifest_digest))
}

/// Unique temporary directory, removed recursively on drop (the CLI-side
/// analogue of the test-support `TestStoreDir`).
struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Result<Self, String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-package-{tag}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).map_err(|error| format!("temp dir: {error}"))?;
        Ok(Self { root })
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod fixture {
    use super::{DevKey, PackageFile, PackageFileEntry, SignerDescriptor};
    use ed25519_dalek::SigningKey;
    use nlos_artifact::{PackageEntryRole, PackageTaskKind, PackageTaskTemplate};
    use nlos_types::PackageId;
    use sha2::{Digest, Sha256};

    pub fn dev_key(seed: u8) -> DevKey {
        DevKey {
            seed: [seed; 32],
            profile_digest: [seed.wrapping_add(1); 32],
            policy_digest: [seed.wrapping_add(2); 32],
            bootstrap_key: [seed.wrapping_add(3); 16],
            valid_from_ms: 0,
            valid_until_ms: u64::MAX,
        }
    }

    pub fn package_file(with_tasks: bool) -> PackageFile {
        let key = dev_key(0x5a);
        let payload_entry = |name: &str, artifact: [u8; 16], body: &[u8]| PackageFileEntry {
            name: name.to_string(),
            role: PackageEntryRole::Executable,
            artifact_id: artifact,
            digest: {
                let mut hasher = Sha256::new();
                hasher.update(body);
                hasher.finalize().into()
            },
            payload: body.to_vec(),
        };
        let tasks = if with_tasks {
            vec![PackageTaskTemplate {
                node_key: [0x33; 16],
                kind: PackageTaskKind::AgentRole,
                binding_digest: [0x44; 32],
                dependency_keys: vec![[0x55; 16]],
                input_selectors_digest: [0x66; 32],
                output_contract_digest: [0x77; 32],
                policy_digest: [0x88; 32],
                resource_ceiling_digest: [0x99; 32],
            }]
        } else {
            Vec::new()
        };
        PackageFile {
            descriptor: SignerDescriptor {
                public_key: SigningKey::from_bytes(&key.seed).verifying_key().to_bytes(),
                profile_digest: key.profile_digest,
                policy_digest: key.policy_digest,
                bootstrap_key: key.bootstrap_key,
                valid_from_ms: key.valid_from_ms,
                valid_until_ms: key.valid_until_ms,
            },
            package_id: PackageId::from_bytes([0xab; 16]),
            version: 1 << 32 | 2 << 16 | 3,
            entries: vec![
                payload_entry("hello", [0x11; 16], b"payload-hello"),
                payload_entry("config", [0x22; 16], b"payload-config"),
            ],
            tasks,
            signature: [0xcd; 64],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_ENTRY_NAME_BYTES, PACKAGE_FILE_MAGIC, decode_package_file, derive_artifact_id, fixture,
        hex_array, numbered_lines, parse_dev_key, parse_dev_manifest, parse_version, scalar_line,
    };
    use nlos_artifact::PackageEntryRole;

    #[test]
    fn version_accepts_plain_and_dotted_forms() {
        assert_eq!(parse_version("7"), Some(7));
        assert_eq!(parse_version("7.3.1"), Some(7_u64 << 32 | 3 << 16 | 1));
        assert_eq!(parse_version("0.0.0"), Some(0));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("-1"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn manifest_parses_scalars_entries_and_tasks() {
        let text = "\
# comment\n\
package-id = 0f1e2d3c4b5a69788796a5b4c3d2e1f0\n\
version = 1.4.2\n\
entry = hello executable hello.bin\n\
entry = config data assets/config.bin\n\
task = 0102030405060708090a0b0c0d0e0f10 agent-role b.txt i.txt o.txt p.txt c.txt\n\
task = 1112131415161718191a1b1c1d1e1f20 executable b.txt i.txt o.txt p.txt c.txt deps=0102030405060708090a0b0c0d0e0f10\n";
        let manifest = parse_dev_manifest(text).expect("parse");
        assert_eq!(
            manifest.package_id.as_bytes(),
            &hex_array::<16>("0f1e2d3c4b5a69788796a5b4c3d2e1f0").expect("hex")
        );
        assert_eq!(manifest.version, 1_u64 << 32 | 4 << 16 | 2);
        assert_eq!(manifest.entries.len(), 2);
        assert_eq!(manifest.entries[0].name, "hello");
        assert_eq!(manifest.entries[0].role, PackageEntryRole::Executable);
        assert_eq!(manifest.entries[1].path, "assets/config.bin");
        assert_eq!(manifest.entries[1].role, PackageEntryRole::Data);
        assert_eq!(manifest.tasks.len(), 2);
        assert_eq!(
            manifest.tasks[1].dependency_keys,
            vec![hex_array::<16>("0102030405060708090a0b0c0d0e0f10").expect("hex")]
        );
    }

    #[test]
    fn manifest_rejects_missing_scalars_duplicates_and_unknown_keys() {
        let missing = "version = 1\nentry = a data a.bin\n";
        assert!(parse_dev_manifest(missing).is_err());
        let duplicate_scalar = "package-id = 0f1e2d3c4b5a69788796a5b4c3d2e1f0\n\
             package-id = 0f1e2d3c4b5a69788796a5b4c3d2e1f0\n\
             version = 1\nentry = a data a.bin\n";
        assert!(parse_dev_manifest(duplicate_scalar).is_err());
        let unknown = "package-id = 0f1e2d3c4b5a69788796a5b4c3d2e1f0\n\
             version = 1\npublisher = me\nentry = a data a.bin\n";
        assert!(parse_dev_manifest(unknown).is_err());
        let duplicate_name = "package-id = 0f1e2d3c4b5a69788796a5b4c3d2e1f0\n\
             version = 1\nentry = a data a.bin\nentry = a data b.bin\n";
        assert!(parse_dev_manifest(duplicate_name).is_err());
        let no_entries = "package-id = 0f1e2d3c4b5a69788796a5b4c3d2e1f0\nversion = 1\n";
        assert!(parse_dev_manifest(no_entries).is_err());
        let bad_role = "package-id = 0f1e2d3c4b5a69788796a5b4c3d2e1f0\n\
             version = 1\nentry = a driver a.bin\n";
        assert!(parse_dev_manifest(bad_role).is_err());
    }

    #[test]
    fn key_file_round_trips_and_rejects_bad_shapes() {
        let key = fixture::dev_key(0x77);
        let parsed = parse_dev_key(&key.encode()).expect("parse");
        assert_eq!(parsed.seed, key.seed);
        assert_eq!(parsed.profile_digest, key.profile_digest);
        assert_eq!(parsed.valid_until_ms, key.valid_until_ms);

        assert!(parse_dev_key("seed = not-hex\n").is_err());
        assert!(parse_dev_key("seed = 11\n").is_err());
        assert!(parse_dev_key("").is_err());
        let reversed_window = format!(
            "seed = {}\nprincipal-profile-digest = {}\ncontrol-domain-policy-digest = {}\n\
             bootstrap-idempotency-key = {}\nkey-valid-from-ms = 9\nkey-valid-until-ms = 1\n",
            "11".repeat(32),
            "22".repeat(32),
            "33".repeat(32),
            "44".repeat(16),
        );
        assert!(parse_dev_key(&reversed_window).is_err());
    }

    #[test]
    fn package_codec_round_trips_and_fails_closed() {
        for with_tasks in [false, true] {
            let package = fixture::package_file(with_tasks);
            let encoded = package.encode();
            assert_eq!(&encoded[..PACKAGE_FILE_MAGIC.len()], PACKAGE_FILE_MAGIC);
            let decoded = decode_package_file(&encoded).expect("decode");
            assert_eq!(decoded.package_id.as_bytes(), package.package_id.as_bytes());
            assert_eq!(decoded.version, package.version);
            assert_eq!(decoded.entries.len(), package.entries.len());
            assert_eq!(decoded.entries[0].payload, package.entries[0].payload);
            assert_eq!(decoded.entries[0].digest, package.entries[0].digest);
            assert_eq!(decoded.tasks, package.tasks);
            assert_eq!(decoded.signature, package.signature);
            assert_eq!(decoded.descriptor.public_key, package.descriptor.public_key);

            assert!(decode_package_file(&encoded[..encoded.len() - 1]).is_err());
            let mut trailing = encoded.clone();
            trailing.push(0);
            assert!(decode_package_file(&trailing).is_err());
        }
    }

    #[test]
    fn artifact_id_is_deterministic_and_name_bounded() {
        let package_id = nlos_types::PackageId::from_bytes([9; 16]);
        let first = derive_artifact_id(package_id, 1, "alpha");
        assert_eq!(first, derive_artifact_id(package_id, 1, "alpha"));
        assert_ne!(first, derive_artifact_id(package_id, 2, "alpha"));
        assert_ne!(
            first,
            derive_artifact_id(package_id, 1, &"x".repeat(MAX_ENTRY_NAME_BYTES))
        );
        // Length prefix participates: a name boundary shift moves the id.
        assert_ne!(
            derive_artifact_id(package_id, 1, "ab"),
            derive_artifact_id(package_id, 1, "a")
        );
    }

    #[test]
    fn line_discipline_skips_comments_and_blanks() {
        assert_eq!(
            numbered_lines("# head\n\n  body  \n"),
            vec![(3, "body".to_string())]
        );
        assert_eq!(
            scalar_line(&(1, " key = value ".to_string())).expect("scalar"),
            ("key".to_string(), "value".to_string())
        );
        assert!(scalar_line(&(1, "no equals".to_string())).is_err());
    }
}
