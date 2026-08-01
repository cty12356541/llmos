//! Fault-injection VFS integration tests. The fault state in
//! `nlos-store-fault` is process-global, so every test here holds
//! `FAULT_LOCK` for its entire duration (do not add tests outside this
//! serialization, and do not run them in parallel with each other).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use nlos_operation::{CompletionOutcome, OperationSpec, OperationState};
use nlos_runtime::FiberHandle;
use nlos_store::{SqliteOperationStore, StoreError};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{
    CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ReceiptId,
};

const VFS_NAME: &str = "nlos-store-fault-test";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static NEXT_DATABASE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-store-fault-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn spec() -> OperationSpec {
    OperationSpec {
        operation_id: OperationId::from_bytes(bytes(1)),
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes(bytes(2)),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(3)),
        cancellation_generation: Generation::INITIAL,
    }
}

fn open_shim(path: &Path) -> SqliteOperationStore {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    SqliteOperationStore::open_with_vfs(path, Some(VFS_NAME)).expect("open via fault vfs")
}

#[test]
fn registered_vfs_is_transparent_until_armed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("transparent");

    let store = open_shim(&database.path);
    let handle = store.register(spec()).expect("register").handle();
    let ticket = store
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        )
        .expect("complete");

    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        OperationState::Completed {
            receipt_id: ReceiptId::from_bytes(bytes(5)),
        }
    );
    nlos_store_fault::disarm();
}

#[test]
fn injected_io_error_fails_commit_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("io-error");

    let store = open_shim(&database.path);
    let handle = store.register(spec()).expect("register").handle();
    let ticket = store
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        )
        .expect_err("commit must fail under injected IO error");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    assert!(nlos_store_fault::writes_observed() > 0);

    // Fail-closed: the uncommitted transition must not masquerade as durable.
    nlos_store_fault::disarm();
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        OperationState::Dispatched
    );
}

#[test]
fn disarm_restores_normal_writes() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("disarm");

    let store = open_shim(&database.path);
    let handle = store.register(spec()).expect("register").handle();
    let ticket = store
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        )
        .expect_err("commit must fail under injected FULL error");

    nlos_store_fault::disarm();
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        )
        .expect("complete succeeds after disarm");
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        OperationState::Completed {
            receipt_id: ReceiptId::from_bytes(bytes(5)),
        }
    );
}

#[test]
fn power_loss_silently_drops_commits_until_disarm() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("power-loss");

    let store = open_shim(&database.path);
    let handle = store.register(spec()).expect("register").handle();
    let ticket = store
        .dispatch(handle, CallbackId::from_bytes(bytes(4)))
        .expect("dispatch");

    // The commit "succeeds" from SQLite's point of view but never reaches
    // durable storage, like a machine that lost power mid-commit.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(bytes(5)),
            },
        )
        .expect("power loss drops writes silently");
    nlos_store_fault::disarm();

    // The surviving connection keeps a wal-index that references frames the
    // disk never saw; only after it dies (as a real power loss would kill
    // it) can a fresh connection recover from the durable bytes alone.
    drop(store);

    let recovered = SqliteOperationStore::open(&database.path).expect("reopen via default vfs");
    assert_eq!(
        recovered.inspect(handle).expect("inspect").state,
        OperationState::Dispatched
    );
}

#[test]
fn unknown_vfs_name_fails_open() {
    let _serialization = fault_lock();
    let database = TestDatabase::new("unknown-vfs");

    let Err(error) = SqliteOperationStore::open_with_vfs(&database.path, Some("nlos-no-such-vfs"))
    else {
        panic!("open with an unregistered VFS name must fail");
    };
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
}
