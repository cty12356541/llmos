//! Crash-safe content-addressed blob I/O.
//!
//! The commit protocol is the safety-critical invariant of this crate:
//!
//! ```text
//! tmp file write -> fsync(file) -> re-read + digest verify
//!   -> atomic rename into blobs/<2-hex>/<digest> -> fsync(parent dir)
//! ```
//!
//! Only after every step above has durably succeeded may the metadata
//! transaction referencing the digest commit (see `store.rs`). A crash
//! anywhere before the rename leaves at most an orphan tmp file (cleaned by
//! `recover`); a crash after the rename but before the metadata commit
//! leaves at most an orphan blob (listed by `recover`, never deleted in this
//! slice); a crash after the metadata commit is a fully usable revision.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::ArtifactError;
use crate::model::ContentDigest;

/// `ENOSPC`/`EDQUOT` on Unix; `ERROR_DISK_FULL`/`ERROR_HANDLE_DISK_FULL` on
/// Windows.
#[cfg(unix)]
const NO_SPACE_CODES: [i32; 2] = [28, 122];
#[cfg(windows)]
const NO_SPACE_CODES: [i32; 2] = [112, 39];

/// `EXDEV` on Unix; `ERROR_NOT_SAME_DEVICE` on Windows.
#[cfg(unix)]
const CROSS_DEVICE_CODE: i32 = 18;
#[cfg(windows)]
const CROSS_DEVICE_CODE: i32 = 17;

static TMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Filesystem layout of one retention domain (`artifacts/` or `cache/`).
#[derive(Clone, Debug)]
pub(crate) struct DomainPaths {
    pub(crate) blobs: PathBuf,
    pub(crate) tmp: PathBuf,
}

impl DomainPaths {
    pub(crate) fn blob_path(&self, digest: ContentDigest) -> PathBuf {
        let hex = digest.to_hex();
        self.blobs.join(&hex[..2]).join(&hex)
    }
}

/// Outcome of [`commit_blob`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlobCommit {
    /// The blob was written, verified, renamed, and directory-synced.
    Stored,
    /// A blob with this digest already existed; content addressing makes the
    /// write a no-op (read paths re-verify the digest).
    AlreadyPresent,
}

/// Writes `bytes` durably under its content digest in `domain`.
///
/// Fails closed: any I/O error, digest mismatch of the bytes that hit the
/// file, or cross-device rename surfaces as a typed error and no blob path
/// is published. The tmp file is removed on a best-effort basis on failure.
pub(crate) fn commit_blob(
    domain: &DomainPaths,
    digest: ContentDigest,
    bytes: &[u8],
) -> Result<BlobCommit, ArtifactError> {
    let final_path = domain.blob_path(digest);
    if final_path.try_exists().map_err(ArtifactError::Io)? {
        return Ok(BlobCommit::AlreadyPresent);
    }
    let shard = final_path.parent().ok_or(ArtifactError::CorruptRecord(
        "blob path has no shard parent",
    ))?;
    fs::create_dir_all(shard).map_err(map_write_error)?;
    sync_dir(&domain.blobs)?;

    let sequence = TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp_path = domain
        .tmp
        .join(format!("{}.{}.tmp", digest.to_hex(), sequence));

    let write_result = write_and_verify(&tmp_path, digest, bytes);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }

    match fs::rename(&tmp_path, &final_path) {
        Ok(()) => {}
        Err(error) => {
            let _ = fs::remove_file(&tmp_path);
            if error.raw_os_error() == Some(CROSS_DEVICE_CODE) {
                return Err(ArtifactError::CrossDeviceRename);
            }
            // A concurrent commit of identical content may have won the
            // rename; content addressing makes that benign.
            if final_path.try_exists().map_err(ArtifactError::Io)? {
                return Ok(BlobCommit::AlreadyPresent);
            }
            return Err(ArtifactError::Io(error));
        }
    }
    sync_dir(shard)?;
    Ok(BlobCommit::Stored)
}

fn write_and_verify(
    tmp_path: &Path,
    expected: ContentDigest,
    bytes: &[u8],
) -> Result<(), ArtifactError> {
    {
        let mut file = File::create(tmp_path).map_err(map_write_error)?;
        file.write_all(bytes).map_err(map_write_error)?;
        file.sync_all().map_err(map_write_error)?;
    }
    // Verify what actually reached the file, not the in-memory buffer: a
    // corrupted or short write must never be renamed into a digest address.
    let mut file = File::open(tmp_path).map_err(ArtifactError::Io)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(ArtifactError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = ContentDigest::from_bytes(hasher.finalize().into());
    if actual != expected {
        return Err(ArtifactError::DigestMismatch {
            expected,
            actual,
            path: tmp_path.to_path_buf(),
        });
    }
    Ok(())
}

/// Reads the blob for `digest`, re-verifying its content.
///
/// Returns `Ok(None)` when the blob file is absent (the caller maps this to
/// its typed `BlobMissing` error or to a cache miss). A digest mismatch is
/// always a hard error: wrong bytes are never returned silently.
pub(crate) fn read_blob_verified(
    domain: &DomainPaths,
    digest: ContentDigest,
) -> Result<Option<Vec<u8>>, ArtifactError> {
    let path = domain.blob_path(digest);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ArtifactError::Io(error)),
    };
    let actual = ContentDigest::of_bytes(&bytes);
    if actual != digest {
        return Err(ArtifactError::DigestMismatch {
            expected: digest,
            actual,
            path,
        });
    }
    Ok(Some(bytes))
}

/// Result of scanning a blob tree.
#[derive(Clone, Debug, Default)]
pub(crate) struct BlobScan {
    pub(crate) present: Vec<ContentDigest>,
    pub(crate) foreign: Vec<PathBuf>,
}

/// Lists every digest-addressed file under `blobs`. Files or directories
/// whose names do not match the `<2-hex>/<64-hex>` layout are reported as
/// foreign and never touched.
pub(crate) fn scan_blobs(blobs_dir: &Path) -> Result<BlobScan, ArtifactError> {
    let mut scan = BlobScan::default();
    let shards = match fs::read_dir(blobs_dir) {
        Ok(shards) => shards,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(scan),
        Err(error) => return Err(ArtifactError::Io(error)),
    };
    for shard_entry in shards {
        let shard_entry = shard_entry.map_err(ArtifactError::Io)?;
        let shard_name = shard_entry.file_name();
        let valid_shard = shard_name.to_str().is_some_and(|name| {
            name.len() == 2 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        if !valid_shard || !shard_entry.path().is_dir() {
            scan.foreign.push(shard_entry.path());
            continue;
        }
        for blob_entry in fs::read_dir(shard_entry.path()).map_err(ArtifactError::Io)? {
            let blob_entry = blob_entry.map_err(ArtifactError::Io)?;
            let name = blob_entry.file_name();
            match name.to_str().and_then(ContentDigest::from_hex) {
                Some(digest) => scan.present.push(digest),
                None => scan.foreign.push(blob_entry.path()),
            }
        }
    }
    Ok(scan)
}

/// Removes every leftover tmp file in `tmp_dir`, returning the count.
/// Anything in the tmp directory is by definition pre-rename and therefore
/// uncommitted, so removal is always safe.
pub(crate) fn clean_tmp(tmp_dir: &Path) -> Result<u64, ArtifactError> {
    let mut removed = 0_u64;
    let entries = match fs::read_dir(tmp_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(ArtifactError::Io(error)),
    };
    for entry in entries {
        let entry = entry.map_err(ArtifactError::Io)?;
        if entry.path().is_file() {
            fs::remove_file(entry.path()).map_err(ArtifactError::Io)?;
            removed += 1;
        }
    }
    sync_dir(tmp_dir)?;
    Ok(removed)
}

/// Removes the blob file for `digest` if present. The only two call sites
/// are cache eviction (which touches only `cache/`) and the explicit
/// orphan GC `collect_orphan_blobs` (which removes only provably orphaned
/// `artifacts/` blobs); there is no other deletion path. Returns whether
/// a file was removed.
pub(crate) fn remove_blob(
    domain: &DomainPaths,
    digest: ContentDigest,
) -> Result<bool, ArtifactError> {
    let path = domain.blob_path(digest);
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Some(shard) = path.parent() {
                sync_dir(shard)?;
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ArtifactError::Io(error)),
    }
}

/// fsyncs a directory so a rename/create inside it becomes durable.
///
/// On Unix this opens the directory read-only and `sync_all`s it. On Windows
/// the directory must be opened with [`FILE_FLAG_BACKUP_SEMANTICS`] so
/// [`FlushFileBuffers`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)
/// can flush directory metadata; std's [`File::sync_all`] issues that call.
/// Other platforms have no portable directory fsync and use a documented no-op.
#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<(), ArtifactError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(ArtifactError::Io)
}

#[cfg(windows)]
fn sync_dir(path: &Path) -> Result<(), ArtifactError> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(ArtifactError::Io)
}

#[cfg(not(any(unix, windows)))]
// The fallible signature is shared with the Unix/Windows branches (which can fail);
// on this platform directory sync is a documented no-op (see above).
#[allow(clippy::unnecessary_wraps)]
fn sync_dir(_path: &Path) -> Result<(), ArtifactError> {
    Ok(())
}

fn map_write_error(error: io::Error) -> ArtifactError {
    match error.raw_os_error() {
        Some(code) if NO_SPACE_CODES.contains(&code) => ArtifactError::BlobNoSpace,
        _ => ArtifactError::Io(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn digest_hex_round_trips() {
        let digest = ContentDigest::of_bytes(b"hello artifact");
        let hex = digest.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(ContentDigest::from_hex(&hex), Some(digest));
        assert_eq!(ContentDigest::from_hex("zz"), None);
        assert_eq!(ContentDigest::from_hex(&hex[..62]), None);
    }

    /// The ENOSPC/EDQUOT classifier must map genuine OS error codes to the
    /// typed `BlobNoSpace` variant. A full end-to-end disk-full write probe
    /// needs a full filesystem (no writable `/dev/full` on macOS); the
    /// integration-level ordering guarantee (blob write failure -> no
    /// metadata commit) is covered by the permission-denied blob write test.
    #[test]
    fn enospc_maps_to_typed_blob_no_space() {
        for code in NO_SPACE_CODES {
            let error = io::Error::from_raw_os_error(code);
            assert!(
                matches!(map_write_error(error), ArtifactError::BlobNoSpace),
                "raw OS error {code} must classify as BlobNoSpace"
            );
        }
        let other = io::Error::from_raw_os_error(5); // EIO
        assert!(matches!(map_write_error(other), ArtifactError::Io(_)));
    }

    #[test]
    fn cross_device_code_is_distinct_from_no_space() {
        assert!(!NO_SPACE_CODES.contains(&CROSS_DEVICE_CODE));
    }

    /// Smoke-test that `sync_dir` accepts an on-disk directory on every host
    /// where this crate is built. Unix flushes the directory inode; Windows
    /// opens with `FILE_FLAG_BACKUP_SEMANTICS` and `FlushFileBuffers`; other
    /// hosts use the documented no-op branch.
    #[test]
    fn sync_dir_succeeds_on_existing_directory() {
        let sequence = TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nlos-artifact-sync-dir-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp directory for sync_dir probe");
        sync_dir(&dir).expect("sync_dir must succeed on an existing directory");
        fs::remove_dir(&dir).expect("remove temp directory");
    }

    /// Exercises the rename-then-shard-sync slice of the commit protocol on
    /// Windows, where directory entry durability is the platform-specific
    /// concern addressed by `sync_dir`.
    #[cfg(windows)]
    #[test]
    fn windows_sync_dir_after_rename_in_shard_layout() {
        let sequence = TMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-artifact-win-sync-{}-{sequence}",
            std::process::id()
        ));
        let blobs = root.join("blobs");
        let shard = blobs.join("ab");
        fs::create_dir_all(&shard).expect("create shard directory");
        sync_dir(&blobs).expect("sync blobs root before rename");
        let tmp = root.join("tmp").join("ab.tmp");
        fs::create_dir_all(tmp.parent().expect("tmp parent")).expect("create tmp dir");
        fs::write(&tmp, b"windows shard sync probe").expect("write tmp blob");
        let final_path = shard.join("cd");
        fs::rename(&tmp, &final_path).expect("atomic rename into shard");
        sync_dir(&shard).expect("flush shard directory after rename");
        assert!(final_path.is_file());
        fs::remove_dir_all(&root).expect("cleanup windows sync probe");
    }
}
