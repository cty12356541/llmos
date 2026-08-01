//! B-STORE-FAULT Unit B acceptance tests: F1 kill-9 crash matrix and F2
//! torn-write / power-loss durability proofs.
//!
//! **Crash semantics disclaimer**: F1 uses forced child termination
//! (`SIGKILL` on Unix, `TerminateProcess` on Windows via `Child::kill`) to
//! simulate *process* crashes. The OS page cache survives a process death,
//! so a killed process observes storage exactly as a crashed process left it —
//! but a killed process is NOT a machine power loss: writes the kernel has
//! accepted are still durable here. Machine-power-loss semantics (accepted
//! writes silently vanishing) are covered by F2 via
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tampering.
//!
//! Child processes are synchronized through piped stdout markers (`READY`),
//! never through sleeps. The fault-injection state in `nlos-store-fault` is
//! process-global, so every F2 test holds `FAULT_LOCK` for its entire
//! duration (same discipline as `fault_vfs.rs`).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_operation::{CallbackTicket, CompletionOutcome, OperationState};
use nlos_store::{OutboxKind, SqliteOperationStore, StoreError};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{CallbackId, ReceiptId};

mod support;

use support::{TestFile, file_size, spec};

const VFS_NAME: &str = "nlos-store-fault-crash";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn callback(seed: u8) -> CallbackId {
    CallbackId::from_bytes([seed.wrapping_add(64); 16])
}

fn receipt(seed: u8) -> ReceiptId {
    ReceiptId::from_bytes([seed.wrapping_add(128); 16])
}

fn outcome(seed: u8) -> CompletionOutcome {
    CompletionOutcome::Completed {
        receipt_id: receipt(seed),
    }
}

fn dispatch_operation(store: &SqliteOperationStore, seed: u8) -> CallbackTicket {
    let handle = store.register(spec(seed)).expect("register").handle();
    store.dispatch(handle, callback(seed)).expect("dispatch")
}

fn complete_operation(store: &SqliteOperationStore, seed: u8) {
    let ticket = dispatch_operation(store, seed);
    store.complete(ticket, outcome(seed)).expect("complete");
}

fn assert_completed(store: &SqliteOperationStore, seed: u8) {
    let handle = store
        .register(spec(seed))
        .expect("idempotent register")
        .handle();
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        OperationState::Completed {
            receipt_id: receipt(seed),
        }
    );
}

fn assert_dispatched(store: &SqliteOperationStore, seed: u8) {
    let handle = store
        .register(spec(seed))
        .expect("idempotent register")
        .handle();
    assert_eq!(
        store.inspect(handle).expect("inspect").state,
        OperationState::Dispatched
    );
}

/// Runs `PRAGMA integrity_check` on the test's own rusqlite connection, as
/// the acceptance criteria require parent-side verification independent of
/// the store under test.
fn assert_integrity(path: &Path) {
    let connection = rusqlite::Connection::open(path).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

fn open_shim(path: &Path) -> SqliteOperationStore {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    SqliteOperationStore::open_with_vfs(path, Some(VFS_NAME)).expect("open via fault vfs")
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (current_exe + env var, operation_store.rs 范式)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, path: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_CRASH_CHILD_DATABASE", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

/// Blocks until the child prints its `READY` marker (pipe synchronization,
/// no sleeps); kills and reaps the child on timeout or early exit.
fn await_marker(child: &mut Child) -> String {
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The libtest harness prints its own banner lines before the helper's
        // marker; scan until the marker (or EOF when the child dies early).
        let mut lines = BufReader::new(stdout).lines();
        let mut marker = None;
        for line in lines.by_ref() {
            match line {
                Ok(line) if line.starts_with("READY") => {
                    marker = Some(line);
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(marker.ok_or_else(|| "child exited without READY".to_string()));
    });
    match receiver.recv_timeout(Duration::from_mins(1)) {
        Ok(Ok(marker)) => marker,
        other => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not report READY: {other:?}");
        }
    }
}

/// Force-terminates the child and proves it did not exit cleanly.
fn kill_and_reap(child: &mut Child) {
    child.kill().expect("force-terminate child");
    let status = child.wait().expect("wait child");
    assert!(
        !status.success(),
        "killed child must not exit cleanly: {status}"
    );
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(path)) = (
        std::env::var("NLOS_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_CRASH_CHILD_DATABASE"),
    ) else {
        return;
    };
    let path = PathBuf::from(path);
    match scenario.as_str() {
        "mid-tx" => {
            let store = SqliteOperationStore::open(&path).expect("open");
            let _ticket = dispatch_operation(&store, 1);
            // Simulate the middle of the complete transaction: a writer
            // transaction is open and has dirtied the operations row but has
            // not committed when the process dies.
            let raw = rusqlite::Connection::open(&path).expect("raw connection");
            raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
            raw.execute("UPDATE operations SET revision = revision + 100", [])
                .expect("mid-tx write");
            println!("READY");
            std::io::stdout().flush().expect("flush marker");
            let _keepers = (store, raw);
            loop {
                std::thread::park();
            }
        }
        "after-commit" => {
            let store = SqliteOperationStore::open(&path).expect("open");
            complete_operation(&store, 1);
            println!("READY");
            std::io::stdout().flush().expect("flush marker");
            let _keeper = store;
            loop {
                std::thread::park();
            }
        }
        "pre-ack" => {
            let store = SqliteOperationStore::open(&path).expect("open");
            complete_operation(&store, 1);
            let pending = store.pending_outbox(10).expect("pending");
            assert_eq!(pending.len(), 1);
            // The consumer has seen the entry but dies before ACKing it.
            println!("READY {}", pending[0].sequence);
            std::io::stdout().flush().expect("flush marker");
            let _keeper = store;
            loop {
                std::thread::park();
            }
        }
        "wal-setup" => {
            // Two fully committed completions; the kill leaves the WAL and
            // SHM behind for the parent's file-level tampering.
            let store = SqliteOperationStore::open(&path).expect("open");
            complete_operation(&store, 31);
            complete_operation(&store, 32);
            println!("READY");
            std::io::stdout().flush().expect("flush marker");
            let _keeper = store;
            loop {
                std::thread::park();
            }
        }
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// F1: kill-9 crash matrix
// ---------------------------------------------------------------------------

#[test]
fn kill9_mid_transaction_rolls_back_completely() {
    let database = TestFile::new("kill9-mid-tx");
    let mut child = spawn_child("mid-tx", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    // Nothing uncommitted may survive: the revision bump inside the killed
    // transaction is rolled back, no receipt, no outbox row.
    let raw = rusqlite::Connection::open(&database.path).expect("raw reopen");
    let (revision, receipt_id): (i64, Option<Vec<u8>>) = raw
        .query_row("SELECT revision, receipt_id FROM operations", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .expect("query operations row");
    assert_eq!(revision, 1, "only register+dispatch may be durable");
    assert!(receipt_id.is_none(), "no receipt may be durable");
    let outbox_rows: i64 = raw
        .query_row("SELECT COUNT(*) FROM operation_outbox", [], |row| {
            row.get(0)
        })
        .expect("count outbox");
    assert_eq!(outbox_rows, 0, "no outbox entry may be durable");
    drop(raw);

    let store = SqliteOperationStore::open(&database.path).expect("reopen after kill");
    assert_dispatched(&store, 1);
    assert!(
        store.pending_outbox(10).expect("pending").is_empty(),
        "outbox must be empty"
    );
    assert_integrity(&database.path);
}

#[test]
fn kill9_after_commit_keeps_everything() {
    let database = TestFile::new("kill9-after-commit");
    let mut child = spawn_child("after-commit", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    // Everything committed before the kill must survive: terminal state,
    // receipt, and the outbox entry.
    let raw = rusqlite::Connection::open(&database.path).expect("raw reopen");
    let receipt_id: Option<Vec<u8>> = raw
        .query_row("SELECT receipt_id FROM operations", [], |row| row.get(0))
        .expect("query receipt");
    assert_eq!(receipt_id, Some(receipt(1).into_bytes().to_vec()));
    let outbox_rows: i64 = raw
        .query_row("SELECT COUNT(*) FROM operation_outbox", [], |row| {
            row.get(0)
        })
        .expect("count outbox");
    assert_eq!(outbox_rows, 1);
    drop(raw);

    let store = SqliteOperationStore::open(&database.path).expect("reopen after kill");
    assert_completed(&store, 1);
    let pending = store.pending_outbox(10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, OutboxKind::WakeFiber);
    assert_eq!(pending[0].callback_id, Some(callback(1)));
    assert_integrity(&database.path);
}

#[test]
fn kill9_consumer_before_ack_preserves_entry_for_redelivery() {
    let database = TestFile::new("kill9-pre-ack");
    let mut child = spawn_child("pre-ack", &database.path);
    let marker = await_marker(&mut child);
    let sequence: i64 = marker
        .trim()
        .strip_prefix("READY ")
        .expect("marker carries the outbox sequence")
        .parse()
        .expect("sequence is an integer");
    kill_and_reap(&mut child);

    // The un-ACKed entry must be intact for redelivery.
    let store = SqliteOperationStore::open(&database.path).expect("reopen after kill");
    assert_completed(&store, 1);
    let pending = store.pending_outbox(10).expect("pending");
    assert_eq!(
        pending.len(),
        1,
        "un-ACKed entry must survive for redelivery"
    );
    assert_eq!(pending[0].sequence, sequence);
    assert_eq!(pending[0].kind, OutboxKind::WakeFiber);

    // Redelivery path: a fresh consumer ACKs it and the outbox drains.
    store.acknowledge_outbox(sequence).expect("redelivered ACK");
    assert!(store.pending_outbox(10).expect("pending").is_empty());
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// F2: torn-write and power-loss durability
// ---------------------------------------------------------------------------

/// Sweeps the injected write-failure point across every xWrite of the
/// complete transaction: at every point the commit must fail closed (state
/// stays DISPATCHED, no outbox growth, database intact), and after disarm
/// the retried complete must succeed.
#[test]
fn torn_write_scan_fails_closed_until_writes_exhausted() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestFile::new("torn-scan");
    let store = open_shim(&database.path);

    // Warm up so the probe below measures a steady-state complete
    // transaction rather than first-WAL-touch overhead.
    complete_operation(&store, 241);

    let probe_ticket = dispatch_operation(&store, 240);
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: u64::MAX,
        code: FaultCode::IoErr,
    });
    store
        .complete(probe_ticket, outcome(240))
        .expect("probe complete must pass with an unreachable failure point");
    let steady_writes = nlos_store_fault::writes_observed();
    nlos_store_fault::disarm();
    assert!(steady_writes > 0, "complete must issue writes");

    let mut remaining = 0_u64;
    loop {
        assert!(
            remaining <= steady_writes,
            "failure point {remaining} exceeds the probed write count {steady_writes}"
        );
        let seed = u8::try_from(remaining).expect("handful of writes per commit");
        let ticket = dispatch_operation(&store, seed);
        let pending_before = store.pending_outbox(256).expect("pending").len();

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining,
            code: FaultCode::IoErr,
        });
        let result = store.complete(ticket, outcome(seed));
        nlos_store_fault::disarm();

        let Err(error) = result else {
            break; // Failure point past the last write: no longer torn.
        };
        assert!(
            matches!(error, StoreError::Sqlite(_)),
            "expected a storage error, got {error}"
        );
        assert_dispatched(&store, seed);
        assert_eq!(
            store.pending_outbox(256).expect("pending").len(),
            pending_before,
            "torn commit must not grow the outbox"
        );
        assert_integrity(&database.path);

        store
            .complete(ticket, outcome(seed))
            .expect("retry after disarm must succeed");
        assert_completed(&store, seed);
        remaining += 1;
    }
}

/// A commit the kernel "accepted" but the disk never saw (machine power
/// loss) must be completely invisible after recovery, and the transition
/// must be redoable.
#[test]
fn power_loss_drops_complete_commit_and_recovery_allows_redo() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestFile::new("power-loss-complete");
    let store = open_shim(&database.path);

    let handle = store.register(spec(10)).expect("register").handle();
    let ticket = store.dispatch(handle, callback(10)).expect("dispatch");

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    store
        .complete(ticket, outcome(10))
        .expect("power loss drops writes silently");
    nlos_store_fault::disarm();

    // The surviving connection keeps a wal-index that references frames the
    // disk never saw; it must die first (as a real power loss would kill
    // it) so recovery sees durable bytes alone (fault_vfs.rs precedent).
    drop(store);

    let recovered = SqliteOperationStore::open(&database.path).expect("reopen after power loss");
    assert_dispatched(&recovered, 10);
    assert!(
        recovered.pending_outbox(10).expect("pending").is_empty(),
        "no half-committed outbox entry may survive"
    );
    assert_integrity(&database.path);

    recovered
        .complete(ticket, outcome(10))
        .expect("redo after disarm must succeed");
    assert_completed(&recovered, 10);
    assert_eq!(recovered.pending_outbox(10).expect("pending").len(), 1);
}

// ---------------------------------------------------------------------------
// F2: file-level WAL tampering (pure std::fs, isomorphic to SQLite wal.test)
// ---------------------------------------------------------------------------

/// Commits two operations in a child and force-terminates it, leaving the WAL
/// and SHM behind exactly as an abrupt process death would.
fn spawn_wal_fixture(name: &str) -> TestFile {
    let database = TestFile::new(name);
    let mut child = spawn_child("wal-setup", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);
    let wal_path = TestFile::sibling(&database.path, "-wal");
    assert!(
        file_size(&wal_path) > 32,
        "killed child must leave a populated WAL behind"
    );
    database
}

/// WAL layout: 32-byte header, then frames of 24-byte header + page. Returns
/// the frame size and the indices of all commit frames (a frame whose
/// "database size in pages after commit" field is nonzero).
fn wal_commit_frames(wal: &[u8]) -> (usize, Vec<usize>) {
    assert!(wal.len() >= 32, "WAL must have a header");
    let page_size = match u32::from_be_bytes(wal[8..12].try_into().expect("page size field")) {
        1 => 65_536,
        value => value as usize,
    };
    assert!(page_size >= 512, "valid SQLite page size");
    let frame_size = 24 + page_size;
    let frame_count = (wal.len() - 32) / frame_size;
    assert!(frame_count > 0, "fixture must contain frames");
    let commits: Vec<usize> = (0..frame_count)
        .filter(|index| {
            let start = 32 + index * frame_size;
            u32::from_be_bytes(wal[start + 8..start + 12].try_into().expect("commit field")) != 0
        })
        .collect();
    assert!(
        commits.len() >= 2,
        "fixture must contain several committed transactions"
    );
    (frame_size, commits)
}

fn remove_shm(database: &TestFile) {
    fs::remove_file(TestFile::sibling(&database.path, "-shm")).expect("remove stale shm");
}

/// Corrupting one frame of the last committed transaction breaks the WAL's
/// cumulative checksum chain there: recovery must drop that transaction and
/// everything after it while keeping every earlier commit.
#[test]
fn wal_corrupt_frame_hides_later_commits_but_keeps_earlier_ones() {
    let database = spawn_wal_fixture("wal-flip");
    let wal_path = TestFile::sibling(&database.path, "-wal");
    let mut wal = fs::read(&wal_path).expect("read wal");
    let (frame_size, commits) = wal_commit_frames(&wal);

    let last_transaction_first_frame = match &commits[..commits.len() - 1] {
        [.., previous] => previous + 1,
        [] => 0,
    };
    // Flip a byte in the frame's data area (past the 24-byte frame header).
    let offset = 32 + last_transaction_first_frame * frame_size + 24 + 64;
    wal[offset] ^= 0xA5;
    fs::write(&wal_path, &wal).expect("write corrupted wal");
    remove_shm(&database);

    let store = SqliteOperationStore::open(&database.path).expect("reopen after corruption");
    assert_completed(&store, 31);
    assert_dispatched(&store, 32);
    assert_eq!(
        store.pending_outbox(10).expect("pending").len(),
        1,
        "only the intact completion's outbox entry may survive"
    );
    assert_integrity(&database.path);
}

/// Truncating the WAL to half a frame makes the torn tail unreadable:
/// recovery must ignore the partial frame and the uncommitted transaction
/// it belonged to, keeping every earlier commit.
#[test]
fn wal_truncated_to_half_frame_hides_torn_tail() {
    let database = spawn_wal_fixture("wal-truncate");
    let wal_path = TestFile::sibling(&database.path, "-wal");
    let wal = fs::read(&wal_path).expect("read wal");
    let (frame_size, commits) = wal_commit_frames(&wal);

    let last_commit = *commits.last().expect("commits exist");
    let half_frame_cut = 32 + last_commit * frame_size + frame_size / 2;
    fs::OpenOptions::new()
        .write(true)
        .open(&wal_path)
        .expect("open wal for truncation")
        .set_len(half_frame_cut as u64)
        .expect("truncate wal to half frame");
    remove_shm(&database);

    let store = SqliteOperationStore::open(&database.path).expect("reopen after truncation");
    assert_completed(&store, 31);
    assert_dispatched(&store, 32);
    assert_eq!(
        store.pending_outbox(10).expect("pending").len(),
        1,
        "only the intact completion's outbox entry may survive"
    );
    assert_integrity(&database.path);
}

/// The SHM is a rebuildable wal-index cache: deleting only it must not lose
/// any committed data.
#[test]
fn wal_recovers_normally_when_only_shm_is_deleted() {
    let database = spawn_wal_fixture("wal-shm");
    remove_shm(&database);

    let store = SqliteOperationStore::open(&database.path).expect("reopen without shm");
    assert_completed(&store, 31);
    assert_completed(&store, 32);
    assert_eq!(store.pending_outbox(10).expect("pending").len(), 2);
    assert_integrity(&database.path);
}
