//! Shared helpers for `nlos-store` acceptance test binaries (F3/F4).
//!
//! Each integration test file is its own crate, so this module is compiled
//! once per consumer; helpers stay small and file-local on purpose.

use std::fs;
use std::path::{Path, PathBuf};

use nlos_operation::OperationSpec;
use nlos_runtime::FiberHandle;
use nlos_types::{CancellationScopeId, ExecutionFiberId, Generation, OperationId};

static NEXT_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A temporary `SQLite` database path that removes itself and its WAL/SHM
/// siblings on drop, restoring write permission first so cleanup also works
/// after read-only (`chmod 444`) test phases.
pub struct TestFile {
    pub path: PathBuf,
}

impl TestFile {
    pub fn new(name: &str) -> Self {
        let sequence = NEXT_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-store-accept-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    /// The `-wal` or `-shm` sibling path of `path`.
    pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    }
}

impl Drop for TestFile {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            Self::sibling(&self.path, "-wal"),
            Self::sibling(&self.path, "-shm"),
        ] {
            // A test may have chmod'ed the main file read-only; restore
            // owner-write so removal cannot fail on permission grounds.
            if let Ok(metadata) = fs::metadata(&path) {
                let mut permissions = metadata.permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    permissions.set_mode(0o644);
                }
                #[cfg(not(unix))]
                {
                    permissions.set_readonly(false);
                }
                let _ = fs::set_permissions(&path, permissions);
            }
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

/// Builds a unique [`OperationSpec`] from a small seed byte.
#[allow(dead_code)]
pub fn spec(seed: u8) -> OperationSpec {
    OperationSpec {
        operation_id: OperationId::from_bytes([seed; 16]),
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([seed.wrapping_add(1); 16]),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(2); 16]),
        cancellation_generation: Generation::INITIAL,
    }
}

/// Size of `path` in bytes, or 0 when the file does not exist (e.g. a `-wal`
/// file removed by a clean close or a TRUNCATE checkpoint).
pub fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}
