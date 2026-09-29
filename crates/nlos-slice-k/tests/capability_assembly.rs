//! Wave D-b: the minimal capability assembly of [`SliceKRuntime`].
//!
//! The runtime opens the landed [`CapabilityAuthority`] under its root
//! (`<root>/capability/capability-authority.db`, the directory-per-authority
//! path style of the other authorities) and exposes it read-only, so the
//! semantic-admission plane (`authorize_semantic`) is production-reachable
//! through the slice. These tests evidence the assembly only — issuance,
//! delegation, and admission semantics stay in `nlos-capability`'s own
//! tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_capability::{CapabilityAuthorityError, CapabilityHandle};
use nlos_slice_k::SliceKRuntime;
use nlos_types::{CapabilityId, Generation};

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-capability-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove slice-k temp root: {error}"),
        }
    }
}

/// Given/When/Then: given a runtime opened over a fresh root; when the
/// capability authority is read through the runtime's accessor; then the
/// authority database exists at the slice's fixed sub-path (WAL/FULL
/// validated by the authority itself during `open`), and `inspect_active`
/// over the empty active set fails closed with the typed not-found error —
/// usable, honest, and never a panic.
#[test]
fn open_creates_capability_database_and_empty_inspect_fails_closed() {
    let dir = TempDir::new("open");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");

    let database = dir
        .root()
        .join("capability")
        .join("capability-authority.db");
    assert!(
        database.is_file(),
        "the capability authority must live at {}",
        database.display()
    );

    let probe = CapabilityHandle {
        capability_id: CapabilityId::from_bytes([0xB1; 16]),
        generation: Generation::INITIAL,
    };
    match runtime.capability().inspect_active(probe, 0) {
        Err(CapabilityAuthorityError::CapabilityNotFound(id)) => {
            assert_eq!(id, probe.capability_id);
        }
        other => {
            panic!("the empty active set must fail closed as CapabilityNotFound, got {other:?}");
        }
    }
}

/// Given/When/Then: given a runtime opened, dropped, and reopened over the
/// same root; when the capability authority is read again; then the reopen
/// is idempotent (schema-version path, not re-migration) and the empty-set
/// inspect behaves identically.
#[test]
fn reopen_over_same_root_is_idempotent() {
    let dir = TempDir::new("reopen");
    let root = dir.root().to_path_buf();
    {
        let runtime = SliceKRuntime::open(&root).expect("first open");
        let _ = runtime.capability();
    }
    let runtime = SliceKRuntime::open(&root).expect("reopen over the same root");

    let probe = CapabilityHandle {
        capability_id: CapabilityId::from_bytes([0xB2; 16]),
        generation: Generation::INITIAL,
    };
    assert!(matches!(
        runtime.capability().inspect_active(probe, 0),
        Err(CapabilityAuthorityError::CapabilityNotFound(_))
    ));
}
