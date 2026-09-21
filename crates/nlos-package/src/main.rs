//! `nlos-package` — developer Package SDK CLI (W33-A, B1-4; install
//! subcommand W35-P2 / handover #2 first slice).
//!
//! A thin shell around the `nlos-artifact` package surface: it owns no
//! verification logic of its own. `build` turns a developer tree (line-based
//! manifest + payload files + optional `tasks` templates) into one signed,
//! self-contained package file; `verify` materializes that file's payloads
//! into a real [`ArtifactStore`], bootstraps the signer principal into a
//! real [`IdentityAuthority`], and runs the crate's authoritative
//! `verify_package` / `verify_package_with_tasks` pipeline — the same path
//! the kernel consumes. `keygen` derives a deterministic developer signing
//! key descriptor from caller-supplied seed material. `install` (W35-P2)
//! drives the same verify pipeline against one persistent state root and
//! then hands the durable receipt to the public application-authority
//! install path — what the slice-k library demo drove in-process, now from
//! the CLI (W33-H §2 boundary 3).
//!
//! # Usage
//!
//! ```text
//! nlos-package keygen --seed <HEX64> [--out <KEYFILE>]
//! nlos-package build <DIR> --key <KEYFILE> [--out <PKGFILE>]
//! nlos-package verify <PKGFILE> [--store <DIR>] [--identity <DIR>] [--at-ms <U64>]
//! nlos-package conformance <PKGFILE>
//! nlos-package install <PKGFILE> --root <DIR>
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
//! store failure · `6` conformance findings (see
//! docs/developers/package-conformance.md) · `7` application-authority
//! install rejection.
//!
//! # Conformance (W33-C)
//!
//! `conformance` is the offline producer-side gate: it runs the
//! crate-level rule set (`nlos_artifact::check_package_file`) over ANY
//! `nlos/package-file/v1` package — not just ones built by this CLI —
//! and prints every typed `PKG-CONF-###` finding. Unlike `verify`, it
//! needs no store or identity authority and never admits a package; it
//! checks the format invariants (structure, manifest schema, signature
//! against the embedded descriptor, digest consistency, window sanity).
//!
//! # Trust boundary (dev toolchain only)
//!
//! `keygen` keys are developer convenience keys: `verify` and `install`
//! bootstrap the signer principal from the descriptor embedded in the
//! package file, which proves the signature matches that key — it is NOT
//! a trust decision. Production signing-key custody, trust roots, and
//! signature chains are deployment concerns outside this CLI (see
//! docs/developers/packaging.md).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{InstallApplicationRequest, InstallDecision};
use nlos_artifact::{
    ArtifactError, ArtifactStore, CollectOrphanBlobsRequest, ContentDigest, CreateArtifactSpec,
    MAX_ENTRY_NAME_BYTES, PackageEntryRole, PackageFile, PackageFileEntry, PackageManifest,
    PackageTaskKind, PackageTaskTemplate, PackageVerificationDecision, ProvenanceSourceTriple,
    PutRevisionRequest, SignedPackage, SignedPackageWithTasks, SignerDescriptor,
    VerifyPackageRequest, VerifyPackageWithTasksRequest, decode_package_file, derive_artifact_id,
    package_manifest_message, package_manifest_with_tasks_message, validate_task_templates,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority};
use nlos_slice_k::SliceKRuntime;
use nlos_types::{ArtifactId, IdempotencyKey, PackageId};
use sha2::{Digest, Sha256};

const USAGE: &str = "usage: nlos-package keygen --seed <HEX64> [--out <KEYFILE>] \
  | build <DIR> --key <KEYFILE> [--out <PKGFILE>] \
  | verify <PKGFILE> [--store <DIR>] [--identity <DIR>] [--at-ms <U64>] \
  | conformance <PKGFILE> \
  | install <PKGFILE> --root <DIR>";

/// Domain separators for CLI-owned derivations; each derivation is plain
/// SHA-256 over `domain ‖ input` (the crate's receipt-id precedent).
const CREATE_KEY_DOMAIN: &[u8] = b"llmos/package-file/create-key/v1";
const VERIFY_KEY_DOMAIN: &[u8] = b"llmos/package-file/verify-key/v1";
const KEYGEN_PROFILE_DOMAIN: &[u8] = b"llmos/package-keygen/profile/v1";
const KEYGEN_POLICY_DOMAIN: &[u8] = b"llmos/package-keygen/policy/v1";
const KEYGEN_BOOTSTRAP_DOMAIN: &[u8] = b"llmos/package-keygen/bootstrap-key/v1";
/// Install-lane clock reading before verification (input: manifest digest,
/// known before a receipt exists).
const INSTALL_VERIFY_CLOCK_DOMAIN: &[u8] = b"llmos/package-file/install-verify-clock/v1";
/// Install-scoped orphan-GC idempotency + clock keys (input: verification
/// receipt id) — the W22-001 pass slice-k runs before its install call.
const INSTALL_GC_KEY_DOMAIN: &[u8] = b"llmos/package-file/install-gc-key/v1";
const INSTALL_GC_CLOCK_DOMAIN: &[u8] = b"llmos/package-file/install-gc-clock/v1";
/// Installation idempotency + clock keys (input: verification receipt id):
/// reinstalling the same package replays the durable receipt; a different
/// package derives a different key, so one root can hold many packages.
const INSTALL_KEY_DOMAIN: &[u8] = b"llmos/package-file/install-key/v1";
const INSTALL_CLOCK_DOMAIN: &[u8] = b"llmos/package-file/install-clock/v1";

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
    /// Conformance findings (exit 6); the findings themselves are
    /// already printed to stdout by the conformance command.
    Conformance(usize),
    /// The application authority refused the installation (exit 7):
    /// unknown verification receipt, disabled/uninstalled terminal state,
    /// idempotency conflict, or an out-of-order timestamp.
    Install(String),
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
            Self::Conformance(_) => 6,
            Self::Install(_) => 7,
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
        "conformance" => conformance_command(&arguments[1..]),
        "install" => install_command(&arguments[1..]),
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
            | Self::Internal(text)
            | Self::Install(text) => text.clone(),
            Self::Conformance(count) => format!("package conformance violations: {count}"),
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

#[cfg_attr(not(unix), allow(unused_variables))]
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
            .map(PackageFileEntry::manifest_entry)
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
    let package = decode_package_file(&bytes)
        .map_err(|error| ToolError::input("package file", &error.to_string()))?;

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
// conformance
// ---------------------------------------------------------------------------

/// Offline producer-side rule check over one package file: collects the
/// typed `PKG-CONF-###` findings of the crate's conformance kit and
/// prints one line per finding plus a summary. Clean reports exit 0,
/// violations exit 6; an unreadable file is a typed input failure (2).
fn conformance_command(arguments: &[String]) -> Result<(), ToolError> {
    let mut package_path = None;
    parse_flags(arguments, &mut |_, _| false, &mut |token| {
        if package_path.is_none() {
            package_path = Some(token.to_string());
            true
        } else {
            false
        }
    })?;
    let Some(package_path) = package_path else {
        return Err(ToolError::Usage);
    };

    let bytes = fs::read(&package_path)
        .map_err(|error| ToolError::input("read package", &format!("{package_path}: {error}")))?;
    let report = nlos_artifact::check_package_file(&bytes);
    for finding in &report.findings {
        println!("{} {}", finding.rule.id(), finding.detail);
    }
    if report.is_conformant() {
        println!("CONFORMANT {package_path}");
        Ok(())
    } else {
        println!(
            "NONCONFORMANT {package_path} findings {}",
            report.findings.len()
        );
        Err(ToolError::Conformance(report.findings.len()))
    }
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

/// Installs one package file into a persistent state root: decode →
/// materialize entries into the root's real `ArtifactStore` → run the
/// authoritative verify pipeline → install-scoped orphan-GC pass → the
/// application authority's verify-then-commit install (receipt
/// digest-binding, generation-advancing CAS). The durable verification
/// receipt is the only thing handed to the install authority — the same
/// authority-first path the slice-k library demo drives, now from the
/// CLI. Every idempotency/clock key derives from the manifest digest or
/// the receipt id, so reinstalling the same package replays every
/// receipt while a different package installs fresh beside it.
fn install_command(arguments: &[String]) -> Result<(), ToolError> {
    let mut package_path = None;
    let mut root = None;
    parse_flags(
        arguments,
        &mut |flag, value| {
            if flag == "root" {
                root = Some(value.to_string());
                true
            } else {
                false
            }
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
    let Some(root) = root else {
        return Err(ToolError::Usage);
    };

    let bytes = fs::read(&package_path)
        .map_err(|error| ToolError::input("read package", &format!("{package_path}: {error}")))?;
    let package = decode_package_file(&bytes)
        .map_err(|error| ToolError::input("package file", &error.to_string()))?;

    let runtime = SliceKRuntime::open(&root)
        .map_err(|error| ToolError::Internal(format!("open state root {root}: {error}")))?;
    let (decision, installation, fresh) = install_into_root(&runtime, &package)?;

    print_decision(&decision);
    println!("INSTALL {}", hex(installation.installation_id.as_bytes()));
    println!("decision {}", if fresh { "installed" } else { "replayed" });
    let executables: Vec<&str> = package
        .entries
        .iter()
        .filter(|entry| entry.role == PackageEntryRole::Executable)
        .map(|entry| entry.name.as_str())
        .collect();
    println!(
        "application {} package {} generation {} version {} entries {} installer {}",
        hex(installation.application_id.as_bytes()),
        hex(installation.package_id.as_bytes()),
        installation.installation_generation.get(),
        installation.package_version,
        installation.entry_count,
        hex(installation.installer_principal.as_bytes()),
    );
    println!("executables {}", executables.join(","));
    Ok(())
}

/// The install lane proper: bootstrap the signer into the root's identity
/// authority, materialize the entries, run the authoritative verify
/// pipeline, execute the install-scoped orphan-GC pass, and hand the
/// durable verification receipt to the application authority's
/// verify-then-commit install — the same authority-first path the
/// slice-k library demo drives. Returns the verification decision, the
/// immutable installation receipt, and whether this call advanced the
/// generation (`true`) or replayed the durable receipt (`false`).
fn install_into_root(
    runtime: &SliceKRuntime,
    package: &PackageFile,
) -> Result<
    (
        PackageVerificationDecision,
        nlos_application::InstallationReceipt,
        bool,
    ),
    ToolError,
> {
    let signer = runtime
        .identity
        .bootstrap_principal(package.descriptor.bootstrap_request())
        .map_err(|error| ToolError::input("bootstrap signer principal", &error.to_string()))?
        .binding()
        .principal_id;

    let manifest = manifest_of(&package.package_id, package.version, &package.entries);
    let message_digest = if package.tasks.is_empty() {
        package_manifest_message(&manifest)
    } else {
        package_manifest_with_tasks_message(&manifest, &package.tasks)
    };
    let verified_at_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_16(
            INSTALL_VERIFY_CLOCK_DOMAIN,
            &message_digest,
        )))
        .map_err(|error| ToolError::Internal(format!("verify clock: {error}")))?;

    for entry in &package.entries {
        materialize_entry(&runtime.artifacts, package, entry, verified_at_ms)?;
    }
    let decision = verify_signed(
        &runtime.artifacts,
        &runtime.identity,
        package,
        signer,
        verified_at_ms,
    )?;
    let receipt = decision.receipt();

    let gc_clock = IdempotencyKey::from_bytes(derive_16(
        INSTALL_GC_CLOCK_DOMAIN,
        receipt.receipt_id.as_bytes(),
    ));
    runtime
        .artifacts
        .collect_orphan_blobs(CollectOrphanBlobsRequest {
            idempotency_key: IdempotencyKey::from_bytes(derive_16(
                INSTALL_GC_KEY_DOMAIN,
                receipt.receipt_id.as_bytes(),
            )),
            collected_at_ms: runtime
                .wall_now_ms(gc_clock)
                .map_err(|error| ToolError::Internal(format!("gc clock: {error}")))?,
        })
        .map_err(from_artifact_error)?;

    let installed_at_ms = runtime
        .wall_now_ms(IdempotencyKey::from_bytes(derive_16(
            INSTALL_CLOCK_DOMAIN,
            receipt.receipt_id.as_bytes(),
        )))
        .map_err(|error| ToolError::Internal(format!("install clock: {error}")))?;
    match runtime
        .applications
        .install_application(
            &runtime.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: receipt.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes(derive_16(
                    INSTALL_KEY_DOMAIN,
                    receipt.receipt_id.as_bytes(),
                )),
                installed_at_ms,
            },
        )
        .map_err(|error| ToolError::Install(error.to_string()))?
    {
        InstallDecision::Installed(installation) => Ok((decision, installation, true)),
        InstallDecision::Replayed(installation) => Ok((decision, installation, false)),
    }
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
    use super::DevKey;

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
}

#[cfg(test)]
mod tests {
    use super::{
        fixture, hex_array, numbered_lines, parse_dev_key, parse_dev_manifest, parse_version,
        scalar_line,
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
