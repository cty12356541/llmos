//! F4 acceptance: checkpoint modes, long-reader interaction, online backup,
//! and file-level backup/restore semantics of the WAL triplet.

mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use nlos_operation::{CompletionOutcome, OperationHandle, OperationState};
use nlos_store::SqliteOperationStore;
use nlos_types::{CallbackId, OperationId, ReceiptId};
use rusqlite::Connection;

use support::{TestFile, file_size, spec};

/// Result row of `PRAGMA wal_checkpoint[(MODE)]`.
#[derive(Debug)]
struct CheckpointRow {
    busy: i64,
    log: i64,
    checkpointed: i64,
}

fn raw_connection(path: &Path) -> Connection {
    let connection = Connection::open(path).expect("raw connection");
    connection
        .busy_timeout(Duration::from_secs(5))
        .expect("busy timeout");
    connection
}

fn wal_checkpoint(connection: &Connection, mode: &str) -> CheckpointRow {
    let sql = match mode {
        "" => "PRAGMA wal_checkpoint".to_owned(),
        other => format!("PRAGMA wal_checkpoint({other})"),
    };
    connection
        .query_row(&sql, [], |row| {
            Ok(CheckpointRow {
                busy: row.get(0)?,
                log: row.get(1)?,
                checkpointed: row.get(2)?,
            })
        })
        .expect("wal_checkpoint")
}

/// Registers, dispatches and completes one operation in three separate
/// committed transactions (one WAL frame batch each).
fn commit_operation(store: &SqliteOperationStore, seed: u8) {
    let handle = store.register(spec(seed)).expect("register").handle();
    let ticket = store
        .dispatch(handle, CallbackId::from_bytes([seed.wrapping_add(1); 16]))
        .expect("dispatch");
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes([seed.wrapping_add(2); 16]),
            },
        )
        .expect("complete");
}

fn operation_state(store: &SqliteOperationStore, seed: u8) -> Option<OperationState> {
    let handle = OperationHandle {
        operation_id: OperationId::from_bytes([seed; 16]),
        generation: nlos_types::Generation::INITIAL,
    };
    store.inspect(handle).ok().map(|snapshot| snapshot.state)
}

/// F4 / checkpoint modes: `PRAGMA wal_checkpoint`, `(FULL)`, `(RESTART)`,
/// `(TRUNCATE)` each return a `(busy, log, checkpointed)` row consistent
/// with the `-wal` file's actual on-disk size (WAL format: 32-byte header
/// plus `log` frames of `24 + page_size` bytes).
#[test]
fn checkpoint_modes_report_rows_consistent_with_wal_size() {
    let database = TestFile::new("ckpt-modes");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    for seed in [61, 62, 63] {
        commit_operation(&store, seed);
    }
    let observer = raw_connection(&database.path);
    let wal = TestFile::sibling(&database.path, "-wal");
    let page_size: i64 = observer
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .expect("page_size");

    // Passive checkpoint: no readers, so every frame is checkpointed; the
    // file itself is left alone (PASSIVE never truncates).
    let wal_size = file_size(&wal);
    assert!(wal_size > 0, "committed frames live in the WAL");
    let passive = wal_checkpoint(&observer, "");
    assert_eq!(passive.busy, 0, "no reader blocks a passive checkpoint");
    assert_eq!(passive.checkpointed, passive.log);
    assert!(passive.log > 0);
    assert_eq!(
        wal_size,
        32 + u64::try_from(passive.log).unwrap() * (24 + u64::try_from(page_size).unwrap()),
        "log frame count matches the WAL header+frames layout"
    );
    assert_eq!(
        file_size(&wal),
        wal_size,
        "PASSIVE checkpoint does not shrink the WAL file"
    );

    // FULL and RESTART: with no active reader they also complete fully.
    // Observed: every fully completed checkpoint RESETS the log (the next
    // commit restarts at frame 0), so `log` counts frames since the last
    // reset. Restarting overwrites the file from the beginning WITHOUT
    // shrinking it, so the on-disk size is an upper bound for the live
    // frames (stale tail frames beyond the live log remain until TRUNCATE).
    for (seed, mode) in [(64, "FULL"), (65, "RESTART")] {
        commit_operation(&store, seed);
        let row = wal_checkpoint(&observer, mode);
        assert_eq!(row.busy, 0, "{mode} completes with no readers");
        assert_eq!(row.checkpointed, row.log, "{mode}");
        assert!(row.log > 0, "{mode}");
        assert!(
            file_size(&wal)
                >= 32 + u64::try_from(row.log).unwrap() * (24 + u64::try_from(page_size).unwrap()),
            "{mode}: WAL size covers at least the reported live frames"
        );
    }

    // TRUNCATE: checkpoints and then shrinks the WAL file to zero bytes.
    commit_operation(&store, 66);
    let truncate = wal_checkpoint(&observer, "TRUNCATE");
    assert_eq!(truncate.busy, 0);
    assert_eq!(truncate.checkpointed, truncate.log);
    assert_eq!(file_size(&wal), 0, "TRUNCATE zeroes the WAL file");
}

/// F4 / long read transaction: a reader pins the WAL end-mark; writers
/// keep committing; a passive checkpoint then reports `busy = 1` with
/// `checkpointed < log`; once the reader releases, TRUNCATE drives the
/// WAL to zero bytes. Synchronization is by value lifetimes (the read
/// lock is held exactly as long as the unconsumed `rows`), never sleeps.
#[test]
fn long_reader_blocks_checkpoint_until_released() {
    let database = TestFile::new("ckpt-reader");
    let store = SqliteOperationStore::open(&database.path).expect("open");
    commit_operation(&store, 71);

    let reader = raw_connection(&database.path);
    let mut statement = reader
        .prepare("SELECT count(*) FROM operations")
        .expect("prepare read");
    let mut rows = statement.query([]).expect("start read");
    assert!(
        rows.next().expect("read row").is_some(),
        "reader sees the committed operation"
    );
    // `rows` is deliberately left unconsumed: the read lock (and the
    // reader's WAL end-mark) stays held.

    // Writers are not blocked by the reader.
    commit_operation(&store, 72);
    commit_operation(&store, 73);

    let observer = raw_connection(&database.path);
    let wal = TestFile::sibling(&database.path, "-wal");
    let wal_size = file_size(&wal);

    // Observed checkpoint semantics with a pinning reader (wal.c
    // mxSafeFrame: frames past a reader's mark are never copied, because
    // the reader may fall back to the main-db page image):
    // - PASSIVE never invokes the busy handler: it checkpoints up to the
    //   reader's mark and reports busy = 0 with checkpointed < log.
    let passive = wal_checkpoint(&observer, "");
    assert_eq!(passive.busy, 0, "PASSIVE never waits, even when blocked");
    assert!(
        passive.checkpointed < passive.log,
        "frames past the reader's mark stay in the WAL: {passive:?}"
    );
    assert!(
        passive.checkpointed > 0,
        "frames up to the reader's mark were checkpointed: {passive:?}"
    );
    // - FULL must finish every frame; with the busy handler disabled it
    //   gives up and reports busy = 1 with checkpointed < log.
    let impatient = raw_connection(&database.path);
    impatient
        .busy_timeout(Duration::ZERO)
        .expect("disable busy timeout");
    let blocked = wal_checkpoint(&impatient, "FULL");
    assert_eq!(
        blocked.busy, 1,
        "FULL cannot finish while a reader pins the WAL: {blocked:?}"
    );
    assert!(
        blocked.checkpointed < blocked.log,
        "FULL reports the un-checkpointed tail: {blocked:?}"
    );
    assert_eq!(
        file_size(&wal),
        wal_size,
        "WAL keeps the un-checkpointed tail on disk"
    );

    drop(rows);
    drop(statement);

    let released = wal_checkpoint(&observer, "TRUNCATE");
    assert_eq!(
        released.busy, 0,
        "checkpoint completes once the reader left"
    );
    assert_eq!(released.checkpointed, released.log);
    assert_eq!(file_size(&wal), 0, "TRUNCATE zeroes the WAL file");
}

/// F4 / online backup: `backup::Backup::run_to_completion` into a second
/// file yields a database that opens as a `SqliteOperationStore` with all
/// committed operations, their outbox entries, and the same `user_version`.
#[test]
fn online_backup_produces_complete_openable_copy() {
    let source = TestFile::new("backup-src");
    let backup_file = TestFile::new("backup-dst");
    let store = SqliteOperationStore::open(&source.path).expect("open source");
    for seed in [81, 82, 83] {
        commit_operation(&store, seed);
    }

    let source_connection = raw_connection(&source.path);
    let mut backup_connection = raw_connection(&backup_file.path);
    rusqlite::backup::Backup::new(&source_connection, &mut backup_connection)
        .expect("start backup")
        .run_to_completion(8, Duration::ZERO, None)
        .expect("backup to completion");
    drop(backup_connection);

    let restored = SqliteOperationStore::open(&backup_file.path).expect("open backup");
    for seed in [81, 82, 83] {
        assert_eq!(
            operation_state(&restored, seed),
            Some(OperationState::Completed {
                receipt_id: ReceiptId::from_bytes([seed.wrapping_add(2); 16]),
            }),
            "backup carries committed state for seed {seed}"
        );
    }
    assert_eq!(
        restored.pending_outbox(16).expect("outbox").len(),
        3,
        "backup carries the outbox entries"
    );

    let source_version: i64 = source_connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("source user_version");
    let backup_version: i64 = raw_connection(&backup_file.path)
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("backup user_version");
    assert_eq!(source_version, 3);
    assert_eq!(backup_version, source_version, "schema version survives");
}

/// F4 / file-level backup, happy path: copying the whole WAL triplet
/// (`db` + `-wal` + `-shm`) while the writer is idle yields a fully
/// usable database, including the still-un-checkpointed commits.
#[test]
fn copying_db_wal_shm_triplet_preserves_un_checkpointed_commits() {
    let source = TestFile::new("copy-src");
    let copy = TestFile::new("copy-dst");
    let store = SqliteOperationStore::open(&source.path).expect("open source");
    commit_operation(&store, 91);
    commit_operation(&store, 92);

    let source_wal = TestFile::sibling(&source.path, "-wal");
    let source_shm = TestFile::sibling(&source.path, "-shm");
    assert!(
        file_size(&source_wal) > 0,
        "the live connection holds un-checkpointed WAL frames"
    );
    // The writer is idle during the copy, so the triplet is consistent.
    fs::copy(&source.path, &copy.path).expect("copy main db");
    fs::copy(&source_wal, TestFile::sibling(&copy.path, "-wal")).expect("copy wal");
    fs::copy(&source_shm, TestFile::sibling(&copy.path, "-shm")).expect("copy shm");

    let restored = SqliteOperationStore::open(&copy.path).expect("open copy");
    for seed in [91, 92] {
        assert_eq!(
            operation_state(&restored, seed),
            Some(OperationState::Completed {
                receipt_id: ReceiptId::from_bytes([seed.wrapping_add(2); 16]),
            }),
            "triplet copy carries un-checkpointed commit for seed {seed}"
        );
    }
    assert_eq!(restored.pending_outbox(16).expect("outbox").len(), 2);
}

/// F4 / file-level backup, negative case: copying ONLY the main database
/// file while committed transactions still sit un-checkpointed in the WAL
/// loses exactly those transactions. The copy is not corrupt — it is a
/// consistent older snapshot: `integrity_check` passes, the checkpointed
/// operation is fully readable, and the lost ones are simply absent.
#[test]
fn copying_main_db_only_loses_un_checkpointed_commits_but_stays_consistent() {
    let source = TestFile::new("split-src");
    let partial = TestFile::new("split-dst");
    let store = SqliteOperationStore::open(&source.path).expect("open source");

    // Operation 101 is checkpointed into the main database file.
    commit_operation(&store, 101);
    let observer = raw_connection(&source.path);
    let truncate = wal_checkpoint(&observer, "TRUNCATE");
    assert_eq!(truncate.busy, 0);
    assert_eq!(file_size(&TestFile::sibling(&source.path, "-wal")), 0);

    // Operation 102 commits only into the WAL; nothing checkpoints it.
    commit_operation(&store, 102);
    assert!(file_size(&TestFile::sibling(&source.path, "-wal")) > 0);

    fs::copy(&source.path, &partial.path).expect("copy main db only");

    let restored = SqliteOperationStore::open(&partial.path).expect("open partial copy");
    assert_eq!(
        operation_state(&restored, 101),
        Some(OperationState::Completed {
            receipt_id: ReceiptId::from_bytes([103; 16]),
        }),
        "checkpointed commit survives in the main-db-only copy"
    );
    assert_eq!(
        operation_state(&restored, 102),
        None,
        "un-checkpointed commit is LOST in the main-db-only copy (data loss, not corruption)"
    );
    assert_eq!(
        restored.pending_outbox(16).expect("outbox").len(),
        1,
        "only the checkpointed operation's outbox entry survives"
    );
    let integrity: String = raw_connection(&partial.path)
        .pragma_query_value(None, "integrity_check", |row| row.get(0))
        .expect("integrity_check");
    assert_eq!(
        integrity, "ok",
        "the partial copy is consistent, not corrupt"
    );
}

/// F4 / backup racing a concurrent writer: the backup must either produce
/// a consistent snapshot (integrity passes, baseline complete, concurrent
/// commits present as a gap-free prefix) or fail loudly — never a file
/// that looks usable while being torn.
#[test]
fn online_backup_with_concurrent_writer_is_consistent_or_errors() {
    let source = TestFile::new("race-src");
    let backup_file = TestFile::new("race-dst");
    let store = SqliteOperationStore::open(&source.path).expect("open source");
    for seed in [110, 111] {
        commit_operation(&store, seed);
    }

    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    let writer = std::thread::spawn({
        let start = start.clone();
        let path = source.path.clone();
        move || {
            start.wait();
            let writer_store = SqliteOperationStore::open(&path).expect("writer store");
            for seed in 120..130 {
                commit_operation(&writer_store, seed);
            }
        }
    });

    let source_connection = raw_connection(&source.path);
    let mut backup_connection = raw_connection(&backup_file.path);
    let backup = rusqlite::backup::Backup::new(&source_connection, &mut backup_connection)
        .expect("start backup");
    start.wait();
    let backup_result = backup.run_to_completion(4, Duration::ZERO, None);
    drop(backup);
    writer.join().expect("writer thread");

    if let Err(error) = backup_result {
        eprintln!("observed: backup under a concurrent writer errored loudly: {error}");
    } else {
        drop(backup_connection);
        let restored = SqliteOperationStore::open(&backup_file.path).expect("open backup");
        let integrity: String = raw_connection(&backup_file.path)
            .pragma_query_value(None, "integrity_check", |row| row.get(0))
            .expect("integrity_check");
        assert_eq!(integrity, "ok", "backup result is not torn");
        for seed in [110, 111] {
            assert!(
                operation_state(&restored, seed).is_some(),
                "baseline commit {seed} present"
            );
        }
        // The concurrent commits appear as a gap-free prefix: the
        // snapshot is "complete up to a consistent point", never a
        // random subset.
        let mut gap_seen = false;
        for seed in 120..130 {
            match operation_state(&restored, seed) {
                Some(_) => assert!(
                    !gap_seen,
                    "concurrent commit {seed} present after a gap: torn snapshot"
                ),
                None => gap_seen = true,
            }
        }
    }
}
