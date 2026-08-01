//! F3 acceptance: disk-full / read-only / IO-error behavior of
//! `SqliteOperationStore`.
//!
//! The fault state in `nlos-store-fault` is process-global, so every test
//! that arms the shim holds `FAULT_LOCK` for its entire duration (same
//! discipline as `fault_vfs.rs`; each integration binary is its own
//! process, so the lock only serializes within this file).

mod support;

use std::error::Error as _;
use std::fs;
use std::io::Write as _;
use std::sync::{Mutex, MutexGuard};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use nlos_operation::CompletionOutcome;
use nlos_store::{SqliteOperationStore, StoreError};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{CallbackId, ReceiptId};

use support::{TestFile, file_size, spec};

const VFS_NAME: &str = "nlos-store-fault-io";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Full `Display` chain of a `StoreError`, top cause last, for content
/// assertions (e.g. that `SQLITE_FULL`'s message reaches the caller).
fn error_chain(error: &StoreError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn callback(value: u8) -> CallbackId {
    CallbackId::from_bytes([value; 16])
}

fn receipt(value: u8) -> ReceiptId {
    ReceiptId::from_bytes([value; 16])
}

fn open_shim(path: &std::path::Path) -> SqliteOperationStore {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    SqliteOperationStore::open_with_vfs(path, Some(VFS_NAME)).expect("open via fault vfs")
}

/// F3 / disk-full injection: an armed `FailWritesAfter { 0, Full }` turns
/// the complete-commit path into a `StoreError` whose cause chain reports
/// the disk-full condition, leaves the operation and outbox untouched (no
/// half-commit), and `disarm` fully restores the write path.
#[test]
fn injected_full_fails_complete_closed_and_disarm_recovers() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestFile::new("full");

    let store = open_shim(&database.path);
    let handle = store.register(spec(11)).expect("register").handle();
    let ticket = store.dispatch(handle, callback(12)).expect("dispatch");

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(13),
            },
        )
        .expect_err("commit must fail under injected disk-full");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(&error).to_lowercase();
    assert!(
        chain.contains("full"),
        "error chain must name the disk-full condition, got: {chain}"
    );
    assert!(nlos_store_fault::writes_observed() > 0);

    // No half-commit: neither the state transition nor its outbox entry may
    // become visible.
    assert!(
        store.pending_outbox(16).expect("outbox read").is_empty(),
        "failed commit must not leak an outbox entry"
    );
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        nlos_operation::OperationState::Dispatched
    );

    nlos_store_fault::disarm();
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(13),
            },
        )
        .expect("complete succeeds after disarm");
    assert_eq!(
        store.pending_outbox(16).expect("outbox read").len(),
        1,
        "recovered commit emits exactly one outbox entry"
    );
}

/// F3 / IO-error on the ACK path plus read consistency under injection.
///
/// Honesty note: the shim only intercepts `xWrite`/`xSync`/`xTruncate`, so
/// pure reads (`pending_outbox`, `inspect`) cannot be made to fail
/// directly. What is asserted for them is the fail-closed contract that
/// matters to callers: while writes are failing, reads neither panic nor
/// silently return data inconsistent with the durable pre-failure state.
#[test]
fn injected_ioerr_fails_ack_and_reads_stay_consistent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestFile::new("ioerr-ack");

    let store = open_shim(&database.path);
    let handle = store.register(spec(21)).expect("register").handle();
    let ticket = store.dispatch(handle, callback(22)).expect("dispatch");
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(23),
            },
        )
        .expect("complete");
    let entries = store.pending_outbox(16).expect("outbox read");
    assert_eq!(entries.len(), 1);
    let sequence = entries[0].sequence;

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });

    // ACK is a write (UPDATE + commit): it must surface the injected IO
    // error as a StoreError, not panic and not pretend success.
    let error = store
        .acknowledge_outbox(sequence)
        .expect_err("ack must fail under injected IO error");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(&error).to_lowercase();
    assert!(
        chain.contains("i/o") || chain.contains("ioerr"),
        "error chain must name the IO condition, got: {chain}"
    );

    // Writes in general fail: a fresh registration must not go through.
    store
        .register(spec(24))
        .expect_err("register must fail under injected IO error");

    // Reads stay consistent with the durable pre-failure state.
    let entries = store
        .pending_outbox(16)
        .expect("outbox read stays available");
    assert_eq!(
        entries.len(),
        1,
        "failed ACK must not silently drop the entry"
    );
    assert_eq!(entries[0].sequence, sequence);
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        nlos_operation::OperationState::Completed {
            receipt_id: receipt(23),
        }
    );

    nlos_store_fault::disarm();
    store
        .acknowledge_outbox(sequence)
        .expect("ack succeeds after disarm");
    assert!(
        store.pending_outbox(16).expect("outbox read").is_empty(),
        "acked entry leaves the pending set"
    );
}

/// F3 / read-only media, matrix cell "no `-wal`/`-shm` on disk".
///
/// Observed behavior (macOS 15, bundled `SQLite` 3.51, per wal.html §5):
/// - `SqliteOperationStore::open` asks for `O_RDWR`, the OS refuses on a
///   `chmod 444` file, and `SQLite`'s unix VFS falls back to `O_RDONLY`.
///   Because a clean close already checkpointed and removed `-wal`/`-shm`,
///   no recovery is needed and the open SUCCEEDS as a read-only
///   connection: `inspect`/`pending_outbox` keep working.
/// - Any write (`register`/`complete`/`acknowledge_outbox`) then fails
///   closed with `StoreError::Sqlite` (`SQLITE_READONLY`, "attempt to write
///   a readonly database") and leaves the file undamaged.
/// - Side effect of the read-only open: `SQLite` creates fresh
///   `-wal`/`-shm` files inheriting the main file's 0o444 mode, so a
///   permission restore must cover all three files; after restoring
///   0o644, a reopened store reads and writes with data intact.
#[cfg(unix)]
#[test]
fn chmod_readonly_without_wal_files() {
    let database = TestFile::new("ro-nowal");
    let handle;
    {
        let store = SqliteOperationStore::open(&database.path).expect("open");
        handle = store.register(spec(31)).expect("register").handle();
        let ticket = store.dispatch(handle, callback(32)).expect("dispatch");
        store
            .complete(
                ticket,
                CompletionOutcome::Completed {
                    receipt_id: receipt(33),
                },
            )
            .expect("complete");
    }
    // Clean close checkpointed everything and removed -wal/-shm.
    assert_eq!(file_size(&TestFile::sibling(&database.path, "-wal")), 0);
    assert_eq!(file_size(&TestFile::sibling(&database.path, "-shm")), 0);

    let metadata = fs::metadata(&database.path).expect("stat");
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o444);
    fs::set_permissions(&database.path, permissions).expect("chmod 444");

    // Store API: open succeeds read-only (O_RDONLY fallback, no recovery
    // needed), reads keep working, writes fail closed with SQLITE_READONLY.
    let store = SqliteOperationStore::open(&database.path)
        .expect("open on 0444 media falls back to read-only");
    assert_eq!(
        store
            .inspect(handle)
            .expect("read works on read-only media")
            .state,
        nlos_operation::OperationState::Completed {
            receipt_id: receipt(33),
        }
    );
    let error = store
        .register(spec(34))
        .expect_err("write on read-only media must fail");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(&error).to_lowercase();
    assert!(
        chain.contains("readonly"),
        "expected SQLITE_READONLY in the chain, got: {chain}"
    );
    drop(store);

    // Raw read-only connection: committed data still readable, writes
    // rejected with SQLITE_READONLY, file undamaged.
    let readonly = rusqlite::Connection::open_with_flags(
        &database.path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read-only open works on clean-closed database");
    let count: i64 = readonly
        .query_row("SELECT count(*) FROM operations", [], |row| row.get(0))
        .expect("read committed data from read-only media");
    assert_eq!(count, 1);
    let write_error = readonly
        .execute("UPDATE operations SET revision = revision + 1", [])
        .expect_err("write on read-only connection must fail");
    assert!(
        write_error.to_string().to_lowercase().contains("readonly"),
        "expected SQLITE_READONLY, got: {write_error}"
    );
    drop(readonly);

    // Restore write permission: store opens and writes again, data intact.
    // Observed wrinkle: the read-only open above CREATED `-wal`/`-shm`
    // inheriting the main file's 0o444 mode (verified via stat on macOS),
    // so all three files must be restored, not just the main database.
    for suffix in ["", "-wal", "-shm"] {
        let path = TestFile::sibling(&database.path, suffix);
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&path, permissions).expect("chmod 644");
    }

    let store = SqliteOperationStore::open(&database.path).expect("reopen after chmod 644");
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        nlos_operation::OperationState::Completed {
            receipt_id: receipt(33),
        },
        "committed data survives the read-only phase"
    );
    store
        .register(spec(34))
        .expect("write resumes after chmod 644");
}

/// F3 / read-only media, matrix cell "`-wal`/`-shm` present" (a live writer
/// kept the WAL side files on disk when the media went read-only).
///
/// Observed behavior (macOS 15, bundled `SQLite` 3.51; wal.html §5 read-only
/// rules, `SQLite` >= 3.22):
/// - With `-shm` and `-wal` present and readable, a read-only open (both
///   the raw `SQLITE_OPEN_READ_ONLY` connection and the store's `O_RDONLY`
///   fallback) reads the committed data via a heap wal-index.
/// - Writes fail closed with `StoreError::Sqlite` (`SQLITE_READONLY`).
/// - Restoring 0o644 lets a fresh store continue with all data intact.
#[cfg(unix)]
#[test]
fn chmod_readonly_with_wal_shm_present() {
    let database = TestFile::new("ro-wal");
    let wal = TestFile::sibling(&database.path, "-wal");
    let shm = TestFile::sibling(&database.path, "-shm");

    // Keep this store open: its live connection keeps -wal/-shm on disk.
    let store = SqliteOperationStore::open(&database.path).expect("open");
    let handle = store.register(spec(41)).expect("register").handle();
    let ticket = store.dispatch(handle, callback(42)).expect("dispatch");
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(43),
            },
        )
        .expect("complete");
    assert!(
        file_size(&wal) > 0,
        "live connection holds un-checkpointed WAL frames"
    );
    assert!(file_size(&shm) > 0, "live connection holds the wal-index");

    for path in [&database.path, &wal, &shm] {
        let metadata = fs::metadata(path).expect("stat");
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o444);
        fs::set_permissions(path, permissions).expect("chmod 444");
    }

    // Read-only reopen with side files present: data readable.
    let readonly = rusqlite::Connection::open_with_flags(
        &database.path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read-only open works with -wal/-shm present and readable");
    let count: i64 = readonly
        .query_row("SELECT count(*) FROM operations", [], |row| row.get(0))
        .expect("read committed data from read-only WAL media");
    assert_eq!(count, 1);
    readonly
        .execute("UPDATE operations SET revision = revision + 1", [])
        .expect_err("write on read-only connection must fail");
    drop(readonly);

    // Store API on read-only WAL media: opens read-only, writes fail.
    let readonly_store = SqliteOperationStore::open(&database.path)
        .expect("open on 0444 WAL media falls back to read-only");
    assert_eq!(
        readonly_store
            .inspect(handle)
            .expect("read works on read-only media")
            .state,
        nlos_operation::OperationState::Completed {
            receipt_id: receipt(43),
        }
    );
    readonly_store
        .register(spec(44))
        .expect_err("write on read-only media must fail");
    drop(readonly_store);

    // Restore: fresh store continues, prior commit intact.
    for path in [&database.path, &wal, &shm] {
        let metadata = fs::metadata(path).expect("stat");
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(path, permissions).expect("chmod 644");
    }
    let reopened = SqliteOperationStore::open(&database.path).expect("reopen after chmod 644");
    assert_eq!(
        reopened.inspect(handle).expect("inspect").state,
        nlos_operation::OperationState::Completed {
            receipt_id: receipt(43),
        },
        "committed data survives the read-only phase"
    );
    reopened
        .register(spec(44))
        .expect("write resumes after chmod 644");
}

#[cfg(not(unix))]
#[test]
fn readonly_media_chmod_matrix_requires_unix_permissions() {
    eprintln!("SKIP read-only chmod matrix: platform has no Unix mode bits");
}

/// F3 / real full disk (environment-gated, macOS `hdiutil` RAM volume).
///
/// Fills a real filesystem to ENOSPC and asserts the commit path returns a
/// disk-full `StoreError` AND the process survives (regression: a write
/// into an mmap-backed or exhausted volume must surface as an error, not a
/// fatal SIGBUS). Skips with `Ok(())` whenever the environment cannot
/// provide a RAM volume (non-macOS, missing/failed `hdiutil`).
struct RamVolume {
    device: String,
    mount_point: std::path::PathBuf,
}

impl RamVolume {
    /// Creates, partitions, formats and mounts an 8 MiB RAM volume; `None`
    /// means the environment cannot provide one and the caller must skip.
    ///
    /// Observed on macOS 15: `newfs_hfs` on the whole RAM device followed
    /// by `hdiutil mount` is flaky ("no mountable file system"), while
    /// `diskutil erasevolume` (GPT partition + format + mount in one step)
    /// is reliable, so the latter is used. A partially built volume is
    /// always detached before returning `None`.
    fn create() -> Option<Self> {
        if !cfg!(target_os = "macos") {
            eprintln!("SKIP real full disk: hdiutil RAM volumes need macOS");
            return None;
        }
        let run = |command: &str, arguments: &[&str]| -> Result<String, String> {
            let output = std::process::Command::new(command)
                .args(arguments)
                .output()
                .map_err(|error| format!("{command}: {error}"))?;
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            } else {
                Err(format!(
                    "{command} {:?}: {}",
                    arguments,
                    String::from_utf8_lossy(&output.stderr)
                ))
            }
        };
        // 16384 sectors x 512 B = 8 MiB: small enough to fill in a few
        // writes, large enough for HFS+ plus a small WAL database.
        let attached = run("hdiutil", &["attach", "-nomount", "ram://16384"])
            .map(|stdout| stdout.trim().to_owned());
        let Ok(device) = attached else {
            eprintln!("SKIP real full disk: {}", attached.expect_err("checked"));
            return None;
        };
        let volume_name = format!("NlosFault{}", std::process::id());
        let erased = run("diskutil", &["erasevolume", "HFS+", &volume_name, &device]);
        match erased {
            Ok(_) => {
                let mount_point = std::path::PathBuf::from("/Volumes").join(&volume_name);
                Some(Self {
                    device,
                    mount_point,
                })
            }
            Err(reason) => {
                let _ = run("hdiutil", &["detach", &device, "-force"]);
                eprintln!("SKIP real full disk: RAM volume setup failed: {reason}");
                None
            }
        }
    }
}

impl Drop for RamVolume {
    fn drop(&mut self) {
        let output = std::process::Command::new("hdiutil")
            .args(["detach", &self.device, "-force"])
            .output();
        match output {
            Ok(done) if done.status.success() => {}
            Ok(done) => eprintln!(
                "WARN hdiutil detach {} failed: {}",
                self.device,
                String::from_utf8_lossy(&done.stderr)
            ),
            Err(error) => eprintln!("WARN hdiutil detach {}: {error}", self.device),
        }
        let _ = fs::remove_dir(&self.mount_point);
    }
}

#[test]
fn real_full_disk_returns_full_error_and_process_survives() {
    let Some(volume) = RamVolume::create() else {
        return;
    };
    let database = volume.mount_point.join("store.sqlite3");
    let store = SqliteOperationStore::open(&database).expect("open on RAM volume");
    let handle = store.register(spec(51)).expect("register").handle();
    let ticket = store.dispatch(handle, callback(52)).expect("dispatch");

    // Fill the volume to ENOSPC in decreasing granularity; a single coarse
    // granularity is not enough because up to chunk-size minus one bytes
    // would stay free and a small WAL commit could still fit (observed:
    // with only 1 MiB chunks the leftover let `complete` succeed).
    let filler_path = volume.mount_point.join("filler.bin");
    let mut filler = fs::File::create(&filler_path).expect("create filler");
    for chunk_size in [1024 * 1024, 64 * 1024, 4096] {
        let chunk = vec![0u8; chunk_size];
        while filler.write_all(&chunk).is_ok() {}
    }
    drop(filler);
    assert!(
        fs::metadata(&filler_path).expect("filler stat").len() > 0,
        "filler consumed space before the volume filled up"
    );
    // Proof of "really full": one more allocation block cannot be had.
    fs::write(volume.mount_point.join("probe.bin"), [0u8; 4096])
        .expect_err("volume must reject a further 4 KiB allocation");

    let error = store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(53),
            },
        )
        .expect_err("commit on a full volume must fail");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(&error).to_lowercase();
    assert!(
        chain.contains("full"),
        "expected the disk-full condition in the chain, got: {chain}"
    );

    // SIGBUS regression: reaching these further observations proves the
    // failing write surfaced as a catchable error instead of killing the
    // process.
    assert_eq!(
        store.inspect(handle).expect("inspect after ENOSPC").state,
        nlos_operation::OperationState::Dispatched,
        "failed commit left the durable state untouched"
    );

    // Cleanup must free space before detach; TestFile is not used here
    // because the database lives on the RAM volume, so remove by hand.
    drop(store);
    fs::remove_file(&filler_path).expect("remove filler");
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(TestFile::sibling(&database, suffix));
    }
}
