//! Layer 1 filesystem-semantics probes: calibrate the `nlos-store-fault`
//! write-loss model (`PowerLossAfter` / torn writes) against real APFS
//! behavior observed through plain file surgery and `kill -9` children.
//!
//! Probe families (all databases are private per-test files in temp dirs;
//! no repository file is ever touched):
//!
//! 1. **fsync visibility** — commits that returned under `WAL` +
//!    `synchronous=FULL`/`NORMAL` survive `SIGKILL` of the writing process
//!    (page cache survives process death); an open transaction's writes do
//!    not.
//! 2. **WAL recovery** — truncating the real WAL to every interesting byte
//!    length (frame boundaries ±1, mid-frame) must recover exactly the
//!    committed prefix, and every `PowerLossAfter` outcome must coincide
//!    with real truncation at the same byte length (the calibration core
//!    assertion).
//! 3. **directory persistence** — created/renamed database files are
//!    visible to the surviving system after `SIGKILL`; a power loss
//!    simulated by the fault VFS during creation leaves an empty file.
//!
//! Crash-semantics disclaimer (same as `nlos-store/tests/fault_crash.rs`):
//! `SIGKILL` proves page-cache visibility, not machine power loss. Machine
//! power loss remains modeled exclusively by `PowerLossAfter` at this layer;
//! dm-flakey (layer 2) and hardware (layer 3) validation is future work.

use std::fs;
use std::path::{Path, PathBuf};

use nlos_store_fault::FaultMode;

mod ffi;
mod harness;
mod wal;

use ffi::RawDb;
use harness::{
    TempDir, VFS_NAME, arm, await_ready, fault_lock, kill_and_reap, print_ready, spawn_child,
};

/// Rows inserted by the measurement workload (one auto-commit transaction
/// per row).
const COMMIT_ROWS: i64 = 6;

// ---------------------------------------------------------------------------
// Shared workload helpers
// ---------------------------------------------------------------------------

fn insert_rows(db: &RawDb, from: i64, to: i64) {
    for k in from..=to {
        db.exec(&format!("INSERT INTO t(k, v) VALUES ({k}, 'row-{k}')"));
    }
}

fn sidecar_path(db: &Path, suffix: &str) -> PathBuf {
    let name = db.file_name().expect("file name").to_str().expect("utf8");
    db.with_file_name(format!("{name}{suffix}"))
}

/// Byte image of `db` plus its `-wal` sidecar (if any), taken while the
/// writing connection is still open.
struct Snapshot {
    db: Vec<u8>,
    wal: Option<Vec<u8>>,
}

fn take_snapshot(db_path: &Path) -> Snapshot {
    let db = fs::read(db_path).expect("read main db bytes");
    let wal = fs::read(sidecar_path(db_path, "-wal")).ok();
    Snapshot { db, wal }
}

/// Materializes `db_bytes` plus (optionally truncated) `wal_bytes` as a
/// fresh `tag.db` inside `dir` and returns its path. `-shm` is never
/// copied: recovery must rebuild it, exactly as after a crash.
fn write_copy(dir: &Path, tag: &str, db_bytes: &[u8], wal_bytes: Option<&[u8]>) -> PathBuf {
    let db_path = dir.join(format!("{tag}.db"));
    fs::write(&db_path, db_bytes).expect("write db copy");
    if let Some(wal_bytes) = wal_bytes {
        fs::write(sidecar_path(&db_path, "-wal"), wal_bytes).expect("write wal copy");
    }
    db_path
}

/// Rows visible through a reopened connection. A missing table (e.g. after
/// total WAL loss on a freshly created database) counts as no rows; the
/// caller separately asserts `integrity_check`.
fn rows_or_empty(db: &RawDb) -> Vec<i64> {
    db.query_ints_result("SELECT k FROM t ORDER BY k")
        .unwrap_or_default()
}

/// Commit frames map to transactions as: frame 1 = `CREATE TABLE`, frame
/// `2 + k` = insert of row `k`. Returns the rows a recovered prefix of
/// `commits` commit frames must show.
fn expected_rows_for_commits(commits: usize) -> Vec<i64> {
    if commits < 2 {
        Vec::new()
    } else {
        (0..=(i64::try_from(commits).expect("commit count fits i64") - 2)).collect()
    }
}

fn open_fault(path: &Path) -> RawDb {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    RawDb::open(path, Some(VFS_NAME))
}

/// Builds the reference byte image for WAL surgery probes: a default-VFS
/// database with `CREATE TABLE` plus rows `0..=COMMIT_ROWS`, snapshotted
/// while the connection is still open (so the WAL still holds everything).
struct Reference {
    snapshot: Snapshot,
    layout: wal::WalLayout,
}

fn build_reference(dir: &Path) -> Reference {
    let db_path = dir.join("ref.db");
    let db = RawDb::open(&db_path, None);
    db.configure_wal("FULL");
    db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)");
    insert_rows(&db, 0, COMMIT_ROWS);
    let snapshot = take_snapshot(&db_path);
    drop(db);
    let wal_bytes = snapshot
        .wal
        .as_deref()
        .expect("WAL must exist before close");
    let layout = wal::parse(wal_bytes);
    assert_eq!(
        layout.commit_ends.len(),
        usize::try_from(COMMIT_ROWS + 2).expect("frame count fits usize"),
        "workload assumption: one commit frame per transaction \
         (CREATE TABLE + rows 0..=COMMIT_ROWS), got {:?}",
        layout.commit_ends,
    );
    Reference { snapshot, layout }
}

// ---------------------------------------------------------------------------
// Child-process roles (kill-9 probes); entry at the bottom of this file
// ---------------------------------------------------------------------------

fn child_run(role: &str, db_path: &Path) -> Option<RawDb> {
    match role {
        // The returned connection is held open by the child until the
        // parent kills it, so the WAL/-shm files survive on disk like a
        // live process about to crash.
        "commit-loop-full" => Some(commit_loop(db_path, "FULL")),
        "commit-loop-normal" => Some(commit_loop(db_path, "NORMAL")),
        "mid-txn" => Some(mid_txn(db_path)),
        "rename-db" => rename_db(db_path),
        "create-empty" => {
            fs::File::create(db_path).expect("create empty file");
            None
        }
        other => panic!("unknown child role {other}"),
    }
}

fn commit_loop(db_path: &Path, synchronous: &str) -> RawDb {
    let db = RawDb::open(db_path, None);
    db.configure_wal(synchronous);
    db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)");
    insert_rows(&db, 1, COMMIT_ROWS);
    db
}

fn mid_txn(db_path: &Path) -> RawDb {
    let db = RawDb::open(db_path, None);
    db.configure_wal("FULL");
    db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)");
    insert_rows(&db, 0, 0);
    db.exec("BEGIN IMMEDIATE");
    insert_rows(&db, 1, 1);
    // No COMMIT: the process dies inside the open transaction.
    db
}

fn rename_db(db_path: &Path) -> Option<RawDb> {
    let source = db_path.with_file_name("rename-src.db");
    let db = RawDb::open(&source, None);
    db.configure_wal("FULL");
    db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)");
    insert_rows(&db, 0, COMMIT_ROWS);
    drop(db);
    fs::rename(&source, db_path).expect("rename database file");
    None
}

// ---------------------------------------------------------------------------
// Probe family 1: fsync / commit visibility under SIGKILL
// ---------------------------------------------------------------------------

#[test]
fn commit_survives_kill9_after_return_synchronous_full() {
    let dir = TempDir::new("kill9-full");
    let db_path = dir.path().join("probe.db");
    let mut child = spawn_child("commit-loop-full", &db_path);
    await_ready(&mut child);
    kill_and_reap(&mut child);

    assert!(db_path.exists(), "main db file must exist after kill");
    let wal_path = sidecar_path(&db_path, "-wal");
    let wal_len = fs::metadata(&wal_path).expect("wal file must exist").len();
    assert!(wal_len > 0, "WAL must hold the un-checkpointed commits");
    assert!(
        sidecar_path(&db_path, "-shm").exists(),
        "shm file must exist"
    );

    let db = RawDb::open(&db_path, None);
    db.assert_integrity();
    assert_eq!(db.query_texts("PRAGMA journal_mode"), ["wal"]);
    assert_eq!(
        db.query_ints("SELECT k FROM t ORDER BY k"),
        (1..=COMMIT_ROWS).collect::<Vec<_>>(),
        "every commit that returned before SIGKILL must be visible"
    );
    println!(
        "CALIBRATION full-sync: kill -9 after commit return preserves all \
         {COMMIT_ROWS} commits (page-cache visibility), wal_len={wal_len}"
    );
}

#[test]
fn commit_survives_kill9_after_return_synchronous_normal() {
    let dir = TempDir::new("kill9-normal");
    let db_path = dir.path().join("probe.db");
    let mut child = spawn_child("commit-loop-normal", &db_path);
    await_ready(&mut child);
    kill_and_reap(&mut child);

    let db = RawDb::open(&db_path, None);
    db.assert_integrity();
    assert_eq!(
        db.query_ints("SELECT k FROM t ORDER BY k"),
        (1..=COMMIT_ROWS).collect::<Vec<_>>(),
        "kill -9 cannot distinguish FULL from NORMAL: the page cache \
         survives process death in both modes"
    );
    println!(
        "CALIBRATION normal-sync: identical visibility to FULL — SIGKILL \
         probes cannot falsify the fsync-dropping part of the fault model; \
         only PowerLossAfter (or layer 2/3) can"
    );
}

#[test]
fn uncommitted_txn_invisible_after_kill9() {
    let dir = TempDir::new("kill9-midtxn");
    let db_path = dir.path().join("probe.db");
    let mut child = spawn_child("mid-txn", &db_path);
    await_ready(&mut child);
    kill_and_reap(&mut child);

    let db = RawDb::open(&db_path, None);
    db.assert_integrity();
    assert_eq!(
        db.query_ints("SELECT k FROM t ORDER BY k"),
        vec![0],
        "uncommitted writes must be invisible; earlier commits survive"
    );
}

// ---------------------------------------------------------------------------
// Probe family 2a: fault VFS power loss between commits vs real snapshot
// ---------------------------------------------------------------------------

#[test]
fn power_loss_between_commits_matches_real_snapshot_restore() {
    let _lock = fault_lock();

    // Real path: snapshot the byte image after commit 1, commit 2, then
    // restore the snapshot — the disk image a power loss between the two
    // commits would leave.
    let real_dir = TempDir::new("powerloss-real");
    let real_path = real_dir.path().join("probe.db");
    let db = RawDb::open(&real_path, None);
    db.configure_wal("FULL");
    db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)");
    insert_rows(&db, 0, 0);
    let snapshot = take_snapshot(&real_path);
    insert_rows(&db, 1, 1);
    drop(db);
    fs::write(&real_path, &snapshot.db).expect("restore db bytes");
    fs::write(
        sidecar_path(&real_path, "-wal"),
        snapshot.wal.as_deref().expect("wal snapshot"),
    )
    .expect("restore wal bytes");
    let _ = fs::remove_file(sidecar_path(&real_path, "-shm"));
    let real_db = RawDb::open(&real_path, None);
    real_db.assert_integrity();
    let real_rows = rows_or_empty(&real_db);
    drop(real_db);

    // Fault path: PowerLossAfter{0} before the second commit — every write
    // and sync of that commit is silently dropped.
    let fault_dir = TempDir::new("powerloss-fault");
    let fault_path = fault_dir.path().join("probe.db");
    let db = open_fault(&fault_path);
    db.configure_wal("FULL");
    db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)");
    insert_rows(&db, 0, 0);
    let armed = arm(FaultMode::PowerLossAfter { remaining: 0 });
    insert_rows(&db, 1, 1);
    let wal_image = fs::read(sidecar_path(&fault_path, "-wal"))
        .expect("wal must exist after simulated power loss");
    drop(db); // close while armed: checkpoint writes are dropped too
    drop(armed);
    let twin_path = write_copy(
        fault_dir.path(),
        "twin",
        &fs::read(&fault_path).expect("read post-loss db bytes"),
        Some(&wal_image),
    );
    let fault_db = RawDb::open(&twin_path, None);
    fault_db.assert_integrity();
    let fault_rows = rows_or_empty(&fault_db);
    drop(fault_db);

    assert_eq!(
        fault_rows, real_rows,
        "fault VFS power loss and real byte restore must agree"
    );
    assert_eq!(fault_rows, vec![0], "only the first commit survives");
    println!(
        "CALIBRATION power-loss-between-commits: fault VFS and real image \
         restore agree (rows={fault_rows:?})"
    );
}

// ---------------------------------------------------------------------------
// Probe family 2b: WAL byte-truncation matrix + fault-boundary equivalence
// ---------------------------------------------------------------------------

#[test]
fn torn_wal_truncation_recovers_exactly_the_committed_prefix() {
    let ref_dir = TempDir::new("torn-ref");
    let reference = build_reference(ref_dir.path());
    let wal_bytes = reference.snapshot.wal.as_deref().expect("ref wal");

    let mut lens: Vec<usize> = vec![0, wal::WAL_HEADER_LEN];
    for &end in &reference.layout.commit_ends {
        lens.extend([end - 1, end, end + 1, end + 13]);
    }
    lens.retain(|&l| l <= wal_bytes.len());
    lens.sort_unstable();
    lens.dedup();

    let sweep_dir = TempDir::new("torn-sweep");
    for &l in &lens {
        let db_path = write_copy(
            sweep_dir.path(),
            "twin",
            &reference.snapshot.db,
            Some(&wal_bytes[..l]),
        );
        let db = RawDb::open(&db_path, None);
        db.assert_integrity();
        let rows = rows_or_empty(&db);
        drop(db);
        let expected = expected_rows_for_commits(wal::committed_prefix_count(&reference.layout, l));
        assert_eq!(
            rows, expected,
            "WAL truncated to {l} bytes recovered the wrong committed prefix"
        );
    }
    println!(
        "CALIBRATION torn-WAL: {} truncation lengths (frame boundaries ±1, \
         mid-frame, header-only, zero) all recover exactly the committed \
         prefix; full_wal_len={} page_size={} frame_size={}",
        lens.len(),
        reference.layout.len,
        reference.layout.page_size,
        reference.layout.frame_size
    );
}

#[test]
fn fault_write_boundary_loss_matches_real_truncation_at_same_byte_length() {
    let ref_dir = TempDir::new("fault-ref");
    let reference = build_reference(ref_dir.path());
    let setup_commits = 2usize; // CREATE TABLE + row 0
    let total_rows_u64 = u64::try_from(COMMIT_ROWS).expect("fits u64");

    for b in 0..=(COMMIT_ROWS + 1) {
        let remaining = u64::try_from(b).expect("fits u64");
        let run_dir = TempDir::new("fault-run");
        let db_path = run_dir.path().join("run.db");
        let _lock = fault_lock();
        let db = open_fault(&db_path);
        db.configure_wal("FULL");
        db.exec("CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT)");
        insert_rows(&db, 0, 0);
        let armed = arm(FaultMode::PowerLossAfter { remaining });
        insert_rows(&db, 1, COMMIT_ROWS);
        let wal_len = fs::metadata(sidecar_path(&db_path, "-wal"))
            .expect("wal exists")
            .len();
        let fault_wal = fs::read(sidecar_path(&db_path, "-wal")).expect("read fault wal");
        drop(db); // close while armed
        drop(armed);

        let surviving = remaining.min(total_rows_u64);
        let surviving_usize = usize::try_from(surviving).expect("fits usize");
        let commits = setup_commits + surviving_usize;
        let expected = expected_rows_for_commits(commits);
        // Byte anchor from the reference image: end of the row-0 commit
        // frame (the setup boundary) plus one frame per surviving insert.
        // Frame COUNT alone under-counts bytes (the journal-mode switch
        // leaves a non-commit frame in the WAL), so the arithmetic is
        // anchored to a measured commit-frame end.
        let setup_boundary = reference.layout.commit_ends[setup_commits - 1];
        let expected_len = setup_boundary + surviving_usize * reference.layout.frame_size;
        assert_eq!(
            usize::try_from(wal_len).expect("wal len fits usize"),
            expected_len,
            "fault B={b}: on-disk WAL length must equal the committed-frame \
             boundary the model implies"
        );

        // The calibration core assertion: the fault-run disk image and the
        // reference WAL cut to the same byte length must recover the same
        // committed prefix.
        let twin_dir = TempDir::new("fault-twin");
        let fault_twin = write_copy(
            twin_dir.path(),
            "fault-twin",
            &reference.snapshot.db,
            Some(&fault_wal),
        );
        let fault_db = RawDb::open(&fault_twin, None);
        fault_db.assert_integrity();
        let fault_rows = rows_or_empty(&fault_db);
        drop(fault_db);

        let real_wal_len =
            expected_len.min(reference.snapshot.wal.as_deref().expect("ref wal").len());
        let real_twin = write_copy(
            twin_dir.path(),
            "real-twin",
            &reference.snapshot.db,
            Some(&reference.snapshot.wal.as_deref().expect("ref wal")[..real_wal_len]),
        );
        let real_db = RawDb::open(&real_twin, None);
        real_db.assert_integrity();
        let real_rows = rows_or_empty(&real_db);
        drop(real_db);

        assert_eq!(
            fault_rows, expected,
            "fault B={b} recovered the wrong committed prefix"
        );
        assert_eq!(
            fault_rows, real_rows,
            "fault B={b} disagrees with real truncation at the same byte length"
        );
    }
    println!(
        "CALIBRATION fault-boundary equivalence: PowerLossAfter B=0..={} — \
         every simulated tear point lands exactly on a WAL frame boundary \
         and recovers identically to real byte truncation at that length",
        COMMIT_ROWS + 1
    );
}

#[test]
fn wal_frame_corruption_hides_corrupted_tail_only() {
    let ref_dir = TempDir::new("corrupt-ref");
    let reference = build_reference(ref_dir.path());
    let wal_bytes = reference.snapshot.wal.as_deref().expect("ref wal");
    let frame_size = reference.layout.frame_size;
    let sweep_dir = TempDir::new("corrupt-sweep");

    let cases: Vec<(usize, &str)> = vec![
        (reference.layout.commit_ends.len(), "last commit frame"),
        (
            reference.layout.commit_ends.len() / 2,
            "middle commit frame",
        ),
    ];
    for (frame_index, label) in cases {
        let mut damaged = wal_bytes.to_vec();
        let frame_start = reference.layout.commit_ends[frame_index - 1] - frame_size;
        damaged[frame_start + 16] ^= 0xff; // checksum-1 first byte
        let db_path = write_copy(
            sweep_dir.path(),
            "twin",
            &reference.snapshot.db,
            Some(&damaged),
        );
        let db = RawDb::open(&db_path, None);
        db.assert_integrity();
        let rows = rows_or_empty(&db);
        drop(db);
        assert_eq!(
            rows,
            expected_rows_for_commits(frame_index - 1),
            "{label} checksum corruption must hide that commit and everything \
             after it, preserving the earlier prefix"
        );
    }

    // A zeroed WAL magic invalidates the whole WAL: the freshly created
    // database falls back to its (empty) checkpointed image.
    let mut damaged = wal_bytes.to_vec();
    damaged[0..4].fill(0);
    let db_path = write_copy(
        sweep_dir.path(),
        "magic",
        &reference.snapshot.db,
        Some(&damaged),
    );
    let db = RawDb::open(&db_path, None);
    db.assert_integrity();
    assert!(
        db.query_ints_result("SELECT k FROM t ORDER BY k").is_err(),
        "invalid WAL magic must yield an empty schema, never partial rows"
    );
    drop(db);
    println!(
        "CALIBRATION frame-corruption: checksum damage hides the damaged \
         commit and its tail (prefix semantics); fault VFS only models \
         whole-write loss, so corruption damage is strictly finer-grained \
         than the model — same outcome class, larger real damage domain"
    );
}

#[test]
fn shm_rebuilds_after_crash() {
    let dir = TempDir::new("shm-rebuild");
    let db_path = dir.path().join("probe.db");
    let mut child = spawn_child("commit-loop-full", &db_path);
    await_ready(&mut child);
    kill_and_reap(&mut child);

    let shm_path = sidecar_path(&db_path, "-shm");
    assert!(shm_path.exists(), "crashed process leaves the shm file");
    fs::remove_file(&shm_path).expect("delete shm");

    // Model blind spot: -shm is written via mmap and never passes through
    // xWrite, so PowerLossAfter cannot touch it. The real filesystem probe
    // must show recovery does not depend on it.
    let db = RawDb::open(&db_path, None);
    db.assert_integrity();
    assert_eq!(
        db.query_ints("SELECT k FROM t ORDER BY k"),
        (1..=COMMIT_ROWS).collect::<Vec<_>>(),
    );
    println!(
        "CALIBRATION shm: deleted -shm after crash rebuilds from the WAL; \
         the fault model is structurally blind to shm loss (mmap writes \
         bypass xWrite) — accepted deviation, real recovery is robust"
    );
}

// ---------------------------------------------------------------------------
// Probe family 3: directory-entry persistence
// ---------------------------------------------------------------------------

#[test]
fn renamed_database_visible_after_kill9() {
    let dir = TempDir::new("rename-kill9");
    let db_path = dir.path().join("renamed.db");
    let mut child = spawn_child("rename-db", &db_path);
    await_ready(&mut child);
    kill_and_reap(&mut child);

    assert!(
        db_path.exists(),
        "renamed database entry must survive process death"
    );
    assert!(
        !db_path.with_file_name("rename-src.db").exists(),
        "old name must be gone after rename"
    );
    let db = RawDb::open(&db_path, None);
    db.assert_integrity();
    assert_eq!(
        db.query_ints("SELECT k FROM t ORDER BY k"),
        (0..=COMMIT_ROWS).collect::<Vec<_>>(),
    );
    println!(
        "CALIBRATION dir-rename: rename visible to the surviving system \
         after kill -9 (page-cache level); durability across a real power \
         loss without dir-fsync remains unverifiable at layer 1"
    );
}

#[test]
fn power_loss_during_creation_leaves_empty_file() {
    let _lock = fault_lock();

    // Fault model: file creation (xOpen) is NOT intercepted, so a power
    // loss during database creation must leave a zero-byte file — creation
    // durable, contents lost.
    let fault_dir = TempDir::new("creation-fault");
    let fault_path = fault_dir.path().join("creation.db");
    let db = open_fault(&fault_path); // creation lands for real
    let armed = arm(FaultMode::PowerLossAfter { remaining: 0 });
    let pragma_ok = db.exec_result("PRAGMA journal_mode=WAL").is_ok();
    let create_ok = db
        .exec_result("CREATE TABLE t(k INTEGER PRIMARY KEY)")
        .is_ok();
    drop(db); // close while armed: checkpoint writes dropped
    drop(armed);
    assert_eq!(
        fs::metadata(&fault_path)
            .expect("db file exists after power loss")
            .len(),
        0,
        "the model requires creation to survive with zero real content bytes"
    );
    let reopened = RawDb::open(&fault_path, None);
    assert_eq!(
        reopened.query_texts("SELECT count(*) FROM sqlite_master"),
        ["0"],
        "a zero-byte database is a valid empty database"
    );
    drop(reopened);

    // Real twin: a child process creates the file and is killed before any
    // write — the page-cache-visible outcome must match the model.
    let real_dir = TempDir::new("creation-real");
    let real_path = real_dir.path().join("creation.db");
    let mut child = spawn_child("create-empty", &real_path);
    await_ready(&mut child);
    kill_and_reap(&mut child);
    assert!(real_path.exists(), "created entry survives kill -9");
    assert_eq!(fs::metadata(&real_path).expect("metadata").len(), 0);

    println!(
        "CALIBRATION creation: model assumes dir entry durable \
         (pragma_ok={pragma_ok}, create_ok={create_ok} — all dropped \
         silently); APFS page-cache view matches (file exists, 0 bytes). \
         Whether a REAL power loss preserves the entry without dir-fsync \
         is unverifiable at layer 1 — flagged for layer 2/3"
    );
}

// ---------------------------------------------------------------------------
// Child entry point
// ---------------------------------------------------------------------------

/// Runs only when spawned by a parent test with the role environment set;
/// a no-op in the normal test run.
#[test]
fn fs_semantics_child() {
    let (Ok(role), Ok(db_path)) = (
        std::env::var(harness::CHILD_ROLE_ENV),
        std::env::var(harness::CHILD_DB_ENV),
    ) else {
        return;
    };
    let keeper = child_run(&role, Path::new(&db_path));
    print_ready();
    let _alive = keeper;
    loop {
        std::thread::park();
    }
}
