//! Deterministic signed package fixtures, content-addressed `UserData` container
//! snapshotting, and `Z4` `Trusted UI` irreversible consent receipts (`W78-E3`).

use std::fs;
use std::path::{Path, PathBuf};

use nlos_application::pack_package_version;
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageFile,
    PackageFileEntry, PackageVerificationReceipt, ProvenanceSourceTriple, PutRevisionRequest,
    SignedPackage, SignedPackageWithTasks, SignerDescriptor, VerifyPackageRequest,
    VerifyPackageWithTasksRequest, check_package_file, decode_package_file, derive_artifact_id,
    package_manifest_message, package_manifest_with_tasks_message,
};
use nlos_identity::IdentityAuthority;
use nlos_types::{ApplicationId, ArtifactId, Generation, IdempotencyKey, PackageId};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

const USERDATA_TREE_DOMAIN: &[u8] = b"llmos/migrate/userdata-tree/v1\0";
const Z4_CONSENT_DOMAIN: &[u8] = b"llmos/migrate/z4-irreversible-consent/v1\0";
const ARTIFACT_CREATE_KEY_DOMAIN: &[u8] = b"llmos/migrate/create-artifact-key/v1";
const VERIFY_PKG_KEY_DOMAIN: &[u8] = b"llmos/migrate/verify-pkg-key/v1";

/// Deterministic `Ed25519` public key for seed `[0x57; 32]` (pinned by unit test).
pub(crate) const FIXTURE_PUBLIC_KEY: [u8; 32] = [
    0xc8, 0x40, 0x69, 0xfa, 0x22, 0x16, 0xc6, 0xed, 0xc1, 0xd5, 0x2c, 0xa8, 0x3a, 0xa1, 0x1c, 0x26,
    0x7d, 0xd5, 0x44, 0x0a, 0xfa, 0x52, 0x60, 0x90, 0xca, 0x43, 0xeb, 0xd0, 0xca, 0x86, 0xa9, 0xcf,
];

/// Deterministic `Ed25519` signature for `v1` (`1.0.0`) manifest under seed `[0x57; 32]`.
pub(crate) const FIXTURE_V1_SIGNATURE: [u8; 64] = [
    0x08, 0xba, 0x90, 0xdd, 0x2f, 0x51, 0xf1, 0xc9, 0xba, 0xe1, 0xb7, 0xb5, 0xfc, 0xa5, 0x39, 0xc9,
    0xe1, 0x5b, 0x57, 0x08, 0x91, 0xf4, 0x7e, 0x5c, 0xc3, 0x8f, 0x23, 0xcd, 0x50, 0x7e, 0x48, 0x76,
    0x4a, 0x10, 0xe2, 0xe5, 0x9f, 0x29, 0xc2, 0x8a, 0x58, 0x8d, 0x4d, 0xdc, 0x24, 0x24, 0x52, 0x4c,
    0xfd, 0x91, 0x59, 0x56, 0x42, 0xae, 0x80, 0xc9, 0xa4, 0xa9, 0x51, 0xe1, 0x6b, 0x16, 0xb2, 0x09,
];

/// Deterministic `Ed25519` signature for `v2` (`1.1.0`) manifest under seed `[0x57; 32]`.
pub(crate) const FIXTURE_V2_SIGNATURE: [u8; 64] = [
    0xc4, 0x0b, 0x7d, 0xe7, 0x20, 0x84, 0xed, 0x99, 0xba, 0x08, 0x67, 0x9c, 0x4c, 0x11, 0xa1, 0x2c,
    0x4e, 0xd8, 0x7f, 0xf3, 0xf1, 0x7f, 0xc5, 0xa6, 0xde, 0xcd, 0x12, 0xf4, 0xd4, 0xf3, 0x1f, 0xa7,
    0x9b, 0xca, 0x7a, 0x5e, 0x80, 0x56, 0xb2, 0x23, 0x7c, 0xf3, 0x1a, 0xa8, 0x76, 0xd9, 0xed, 0xa5,
    0x03, 0x4a, 0x61, 0x00, 0x6d, 0x48, 0xc1, 0xb0, 0x88, 0x74, 0x01, 0x19, 0x65, 0x7d, 0xa8, 0x08,
];

/// Pair of signed `v1` (baseline) and `v2` (target upgrade) packages used by sandbox drills.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrillPackagePair {
    /// Baseline `v1` package (`Generation(1)`).
    pub v1: PackageFile,
    /// Target `v2` upgrade package (`Generation(2)`).
    pub v2: PackageFile,
}

impl DrillPackagePair {
    /// Returns the built-in deterministic `1.0.0 -> 1.1.0` signed package pair.
    #[must_use]
    pub fn canonical_fixture() -> Self {
        let package_id = PackageId::from_bytes([0x57; 16]);
        let descriptor = SignerDescriptor {
            public_key: FIXTURE_PUBLIC_KEY,
            profile_digest: [0x11; 32],
            policy_digest: [0x22; 32],
            bootstrap_key: [0x33; 16],
            valid_from_ms: 1_000,
            valid_until_ms: 9_000_000_000,
        };
        let v1_ver = pack_package_version(1, 0, 0);
        let v2_ver = pack_package_version(1, 1, 0);

        let v1_payload = b"nlos-migrate-drill-app-v1\n".to_vec();
        let v1_entry = PackageFileEntry {
            name: "app-main".to_owned(),
            role: PackageEntryRole::Executable,
            artifact_id: derive_artifact_id(package_id, v1_ver, "app-main"),
            digest: sha256_bytes(&v1_payload),
            payload: v1_payload,
        };

        let v2_payload = b"nlos-migrate-drill-app-v2\n".to_vec();
        let v2_entry = PackageFileEntry {
            name: "app-main".to_owned(),
            role: PackageEntryRole::Executable,
            artifact_id: derive_artifact_id(package_id, v2_ver, "app-main"),
            digest: sha256_bytes(&v2_payload),
            payload: v2_payload,
        };

        Self {
            v1: PackageFile {
                descriptor: descriptor.clone(),
                package_id,
                version: v1_ver,
                entries: vec![v1_entry],
                tasks: Vec::new(),
                signature: FIXTURE_V1_SIGNATURE,
            },
            v2: PackageFile {
                descriptor,
                package_id,
                version: v2_ver,
                entries: vec![v2_entry],
                tasks: Vec::new(),
                signature: FIXTURE_V2_SIGNATURE,
            },
        }
    }

    /// Decodes and conformance-checks caller-supplied `v1` and `v2` `.nlospkg` archives.
    ///
    /// # Errors
    ///
    /// Fails closed if either archive fails binary framing or `PKG-CONF-*` conformance.
    pub fn from_package_bytes(v1_bytes: &[u8], v2_bytes: &[u8]) -> Result<Self, String> {
        let v1 = decode_and_check("v1 package", v1_bytes)?;
        let v2 = decode_and_check("v2 package", v2_bytes)?;
        if v1.package_id != v2.package_id {
            return Err("v1 and v2 packages must share the same PackageId".to_owned());
        }
        Ok(Self { v1, v2 })
    }
}

fn decode_and_check(label: &str, bytes: &[u8]) -> Result<PackageFile, String> {
    let decoded = decode_package_file(bytes).map_err(|err| format!("{label}: {err}"))?;
    let report = check_package_file(bytes);
    if !report.is_conformant() {
        let reasons: Vec<String> = report
            .findings
            .iter()
            .map(|f| format!("{}: {}", f.rule.id(), f.detail))
            .collect();
        return Err(format!(
            "{label} conformance failed: {}",
            reasons.join("; ")
        ));
    }
    Ok(decoded)
}

/// Sealed read-only snapshot of an application's `UserData` container directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserDataSnapshotHandle {
    /// Read-only snapshot directory containing `userdata/` copy.
    pub snapshot_userdata_dir: PathBuf,
    /// Content-addressed `SHA-256` digest of the pre-upgrade `UserData` tree.
    pub expected_sha256: [u8; 32],
}

/// Verified pre-upgrade external backup receipt required by `DataMigrationMode::Irreversible`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreUpgradeBackupReceipt {
    /// Path to the external backup directory holding the pre-upgrade `UserData` tree.
    pub backup_dir: PathBuf,
    /// Declared `SHA-256` digest of the backed-up `UserData` tree.
    pub declared_userdata_sha256: [u8; 32],
}

/// `Z4` `Trusted UI` irreversible migration consent receipt (`RISK-E-05` hard gate).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Z4IrreversibleConsentReceipt {
    /// Target application identity.
    pub application_id: ApplicationId,
    /// Baseline generation from which destructive upgrade starts.
    pub from_generation: Generation,
    /// Target `v2` package manifest digest.
    pub target_manifest_digest: ContentDigest,
    /// `SHA-256` digest of the verified pre-upgrade `UserData` backup.
    pub backup_userdata_sha256: [u8; 32],
    /// Domain-separated `Z4` confirmation token binding all four parameters above.
    pub confirmation_token: [u8; 32],
}

impl Z4IrreversibleConsentReceipt {
    /// Constructs a bound `Z4` consent receipt for `(application_id, from_generation,
    /// target_manifest_digest, backup_userdata_sha256)`.
    #[must_use]
    pub fn issue(
        application_id: ApplicationId,
        from_generation: Generation,
        target_manifest_digest: ContentDigest,
        backup_userdata_sha256: [u8; 32],
    ) -> Self {
        let confirmation_token = derive_z4_consent_token(
            application_id,
            from_generation,
            target_manifest_digest,
            backup_userdata_sha256,
        );
        Self {
            application_id,
            from_generation,
            target_manifest_digest,
            backup_userdata_sha256,
            confirmation_token,
        }
    }
}

/// Computes the deterministic domain-separated `Z4` consent token binding
/// `(application_id, from_generation, target_manifest_digest, backup_userdata_sha256)`.
#[must_use]
pub fn derive_z4_consent_token(
    application_id: ApplicationId,
    from_generation: Generation,
    target_manifest_digest: ContentDigest,
    backup_userdata_sha256: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(Z4_CONSENT_DOMAIN);
    hasher.update(application_id.as_bytes());
    hasher.update(from_generation.get().to_be_bytes());
    hasher.update(target_manifest_digest.as_bytes());
    hasher.update(backup_userdata_sha256);
    hasher.finalize().into()
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

pub(crate) fn derive_key_16(domain: &[u8], payload: &[u8]) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(payload);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(out)
}

pub(crate) fn materialize_and_verify_package(
    identity: &IdentityAuthority,
    artifacts: &ArtifactStore,
    package: &PackageFile,
    at_ms: u64,
) -> Result<PackageVerificationReceipt, String> {
    let signer = identity
        .bootstrap_principal(package.descriptor.bootstrap_request())
        .map_err(|err| format!("bootstrap signer principal: {err}"))?
        .binding()
        .principal_id;

    for entry in &package.entries {
        let artifact_id = ArtifactId::from_bytes(entry.artifact_id);
        artifacts
            .create_artifact(CreateArtifactSpec {
                artifact_id,
                idempotency_key: derive_key_16(ARTIFACT_CREATE_KEY_DOMAIN, &entry.artifact_id),
                content_type: "application/octet-stream".to_owned(),
                application_id: None,
                owner: None,
                created_at_ms: at_ms,
            })
            .map_err(|err| format!("create artifact {}: {err}", entry.name))?;
        artifacts
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
            .map_err(|err| format!("put revision {}: {err}", entry.name))?;
    }

    let manifest = package.manifest();
    let msg_digest = if package.tasks.is_empty() {
        package_manifest_message(&manifest)
    } else {
        package_manifest_with_tasks_message(&manifest, &package.tasks)
    };
    let verify_key = derive_key_16(VERIFY_PKG_KEY_DOMAIN, &msg_digest);

    let decision = if package.tasks.is_empty() {
        let envelope_pkg = SignedPackage {
            manifest,
            signer,
            signature: package.signature,
        };
        artifacts
            .verify_package(
                identity,
                VerifyPackageRequest {
                    signed: &envelope_pkg,
                    idempotency_key: verify_key,
                    verified_at_ms: at_ms,
                },
            )
            .map_err(|err| format!("verify package: {err}"))?
    } else {
        let templated_pkg = SignedPackageWithTasks {
            manifest,
            tasks: package.tasks.clone(),
            signer,
            signature: package.signature,
        };
        artifacts
            .verify_package_with_tasks(
                identity,
                VerifyPackageWithTasksRequest {
                    signed: &templated_pkg,
                    idempotency_key: verify_key,
                    verified_at_ms: at_ms,
                },
            )
            .map_err(|err| format!("verify templated package: {err}"))?
    };

    Ok(decision.receipt().clone())
}

pub(crate) fn open_userdata_sqlite(db_path: &Path) -> Result<Connection, String> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create userdata dir: {err}"))?;
    }
    let conn = Connection::open(db_path).map_err(|err| format!("open userdata db: {err}"))?;
    conn.pragma_update(None, "journal_mode", "DELETE")
        .map_err(|err| format!("set journal_mode=DELETE: {err}"))?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|err| format!("set synchronous=FULL: {err}"))?;
    Ok(conn)
}

pub(crate) fn seed_baseline_userdata(userdata_dir: &Path) -> Result<(), String> {
    let attachments_dir = userdata_dir.join("attachments");
    fs::create_dir_all(&attachments_dir).map_err(|err| format!("create attachments dir: {err}"))?;
    fs::write(
        attachments_dir.join("welcome.bin"),
        b"userdata-v1-attachment-payload\n",
    )
    .map_err(|err| format!("write welcome.bin: {err}"))?;

    let conn = open_userdata_sqlite(&userdata_dir.join("app_data.db"))?;
    conn.execute_batch(
        "CREATE TABLE user_documents (
            doc_id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            schema_rev INTEGER NOT NULL
         );
         INSERT INTO user_documents (doc_id, title, body, schema_rev)
         VALUES ('doc-001', 'Alpha', 'Original body alpha', 1),
                ('doc-002', 'Beta', 'Original body beta', 1);",
    )
    .map_err(|err| format!("seed user_documents: {err}"))?;
    Ok(())
}

/// Computes a deterministic content-addressed `SHA-256` digest over all files in `dir`.
///
/// # Errors
///
/// Returns an error string if `dir` does not exist or any file cannot be read.
pub fn compute_userdata_tree_sha256(dir: &Path) -> Result<[u8; 32], String> {
    if !dir.is_dir() {
        return Err(format!(
            "userdata directory {} does not exist",
            dir.display()
        ));
    }
    let mut files = Vec::new();
    collect_relative_files(dir, dir, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    hasher.update(USERDATA_TREE_DOMAIN);
    hasher.update((files.len() as u64).to_be_bytes());
    for (rel_path, full_path) in files {
        let bytes =
            fs::read(&full_path).map_err(|err| format!("read {}: {err}", full_path.display()))?;
        hasher.update((rel_path.len() as u64).to_be_bytes());
        hasher.update(rel_path.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(sha256_bytes(&bytes));
    }
    Ok(hasher.finalize().into())
}

fn collect_relative_files(
    base: &Path,
    current: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let entries =
        fs::read_dir(current).map_err(|err| format!("read_dir {}: {err}", current.display()))?;
    for entry_res in entries {
        let entry = entry_res.map_err(|err| format!("dir entry: {err}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("file_type {}: {err}", path.display()))?;
        if file_type.is_dir() {
            collect_relative_files(base, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(base)
                .map_err(|err| format!("strip_prefix: {err}"))?;
            let normalized = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((normalized, path));
        }
    }
    Ok(())
}

/// Copies `userdata_dir` into a sealed read-only snapshot under `snapshot_parent_dir`.
///
/// # Errors
///
/// Fails closed if copying or digest verification fails.
pub fn create_userdata_snapshot(
    userdata_dir: &Path,
    snapshot_parent_dir: &Path,
) -> Result<UserDataSnapshotHandle, String> {
    let expected_sha256 = compute_userdata_tree_sha256(userdata_dir)?;
    let snapshot_userdata_dir = snapshot_parent_dir.join("userdata");
    if snapshot_userdata_dir.exists() {
        clear_readonly_recursive(&snapshot_userdata_dir)?;
        fs::remove_dir_all(&snapshot_userdata_dir)
            .map_err(|err| format!("clean old snapshot: {err}"))?;
    }
    copy_dir_recursive(userdata_dir, &snapshot_userdata_dir, true)?;
    let verify_sha256 = compute_userdata_tree_sha256(&snapshot_userdata_dir)?;
    if verify_sha256 != expected_sha256 {
        return Err("snapshot SHA-256 verification mismatch after copy".to_owned());
    }
    Ok(UserDataSnapshotHandle {
        snapshot_userdata_dir,
        expected_sha256,
    })
}

/// Atomically restores `userdata_dir` from a sealed [`UserDataSnapshotHandle`].
///
/// # Errors
///
/// Fails closed if the snapshot digest was tampered with or restoration fails.
pub fn restore_userdata_snapshot(
    snapshot: &UserDataSnapshotHandle,
    userdata_dir: &Path,
) -> Result<[u8; 32], String> {
    let snap_digest = compute_userdata_tree_sha256(&snapshot.snapshot_userdata_dir)?;
    if snap_digest != snapshot.expected_sha256 {
        return Err("sealed snapshot failed SHA-256 integrity check before restore".to_owned());
    }
    let parent = userdata_dir
        .parent()
        .ok_or_else(|| "userdata_dir has no parent directory".to_owned())?;
    let staging_dir = parent.join("userdata.restore_staging");
    if staging_dir.exists() {
        clear_readonly_recursive(&staging_dir)?;
        fs::remove_dir_all(&staging_dir).map_err(|err| format!("clean staging: {err}"))?;
    }
    copy_dir_recursive(&snapshot.snapshot_userdata_dir, &staging_dir, false)?;
    let staged_digest = compute_userdata_tree_sha256(&staging_dir)?;
    if staged_digest != snapshot.expected_sha256 {
        return Err("restored staging copy failed SHA-256 verification".to_owned());
    }
    if userdata_dir.exists() {
        clear_readonly_recursive(userdata_dir)?;
        fs::remove_dir_all(userdata_dir).map_err(|err| format!("remove dirty userdata: {err}"))?;
    }
    fs::rename(&staging_dir, userdata_dir)
        .map_err(|err| format!("atomic rename restored userdata: {err}"))?;
    compute_userdata_tree_sha256(userdata_dir)
}

/// Copies `src` to `dst` recursively, optionally marking copied files read-only.
///
/// # Errors
///
/// Returns an error string on any filesystem failure.
pub fn copy_dir_recursive(src: &Path, dst: &Path, mark_readonly: bool) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|err| format!("create_dir_all {}: {err}", dst.display()))?;
    let entries = fs::read_dir(src).map_err(|err| format!("read_dir {}: {err}", src.display()))?;
    for entry_res in entries {
        let entry = entry_res.map_err(|err| format!("dir entry: {err}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|err| format!("file_type {}: {err}", src_path.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path, mark_readonly)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path).map_err(|err| {
                format!(
                    "copy {} -> {}: {err}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
            let mut perms = fs::metadata(&dst_path)
                .map_err(|err| format!("metadata {}: {err}", dst_path.display()))?
                .permissions();
            perms.set_readonly(mark_readonly);
            fs::set_permissions(&dst_path, perms)
                .map_err(|err| format!("set_permissions {}: {err}", dst_path.display()))?;
        }
    }
    Ok(())
}

fn clear_readonly_recursive(dir: &Path) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    for entry_res in
        fs::read_dir(dir).map_err(|err| format!("read_dir {}: {err}", dir.display()))?
    {
        let entry = entry_res.map_err(|err| format!("dir entry: {err}"))?;
        let path = entry.path();
        let meta =
            fs::metadata(&path).map_err(|err| format!("metadata {}: {err}", path.display()))?;
        if meta.is_dir() {
            clear_readonly_recursive(&path)?;
        } else {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(&path, perms)
                .map_err(|err| format!("clear readonly {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    #[test]
    fn canonical_fixture_ed25519_signatures_match_seed() {
        let signing_key = SigningKey::from_bytes(&[0x57; 32]);
        let expected_pk = signing_key.verifying_key().to_bytes();
        let pair = DrillPackagePair::canonical_fixture();
        let v1_msg = package_manifest_message(&pair.v1.manifest());
        let v2_msg = package_manifest_message(&pair.v2.manifest());
        let expected_v1_sig = signing_key.sign(&v1_msg).to_bytes();
        let expected_v2_sig = signing_key.sign(&v2_msg).to_bytes();

        assert_eq!(
            FIXTURE_PUBLIC_KEY, expected_pk,
            "expected pk: {expected_pk:02x?}"
        );
        assert_eq!(
            (FIXTURE_V1_SIGNATURE, FIXTURE_V2_SIGNATURE),
            (expected_v1_sig, expected_v2_sig),
            "expected v1: {expected_v1_sig:#04x?}\nexpected v2: {expected_v2_sig:#04x?}"
        );
        assert!(check_package_file(&pair.v1.encode()).is_conformant());
        assert!(check_package_file(&pair.v2.encode()).is_conformant());
    }
}
