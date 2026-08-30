//! B-CLOCK-001 (kill-window): fault-injection matrix for the durable local
//! monotonic clock authority — the single `AuthorityClock::now` entry, plus
//! the trigger-guarded tamper surface after an injected crash and recovery.
//!
//! Harness and fixtures follow the established matrices exactly, most
//! recently `nlos-wait/tests/wait_fault_injection.rs` (kill-9 children
//! synchronized through piped `READY` markers — never sleeps, `FAULT_LOCK`
//! process-wide serialization, WAL tail truncation sweeps, typed error-chain
//! assertions, raw row counts, `PRAGMA integrity_check` per scenario).
//!
//! **Fault-VFS plumbing (documented harness constraint, same deviation as
//! the wait/channel/topic matrices)**: `AuthorityClock` has no
//! `open_with_vfs` constructor and the workspace forbids `unsafe`, so the
//! shim is routed in through a `SQLite` **URI filename**: `rusqlite`'s
//! `Connection::open` sets `SQLITE_OPEN_URI`, and `AuthorityClock::open`
//! passes `root.join("clock-authority.db")` through unchanged, so a root of
//! `file:<db>?vfs=<shim>&tail=` routes that one authority connection through
//! the registered fault VFS (the appended `/clock-authority.db` tail lands
//! in the ignored `tail=` query parameter). The junk directory that the
//! authority `create_dir_all(root)` call creates for the literal URI path is
//! kept inside a RAII sandbox process CWD — the worktree is never touched.
//! Every reopen / raw reader / integrity check uses the plain default VFS
//! and can never be faulted.
//!
//! Matrix (trimmed from the 13-item wait matrix to this domain's single
//! entry, `now`):
//! - C1 pre-commit IOERR on both `now` phases (first-call initialization and
//!   high-water advance) — typed `Sqlite` error whose chain names the
//!   injected condition, watermark wholly at the previous value and zero new
//!   receipts after reopen (zero partial state), the disarmed same-key redo
//!   converges onto the deterministic reading the phantom would have had,
//!   and the durable result replays byte-equal;
//! - C2 pre-commit ENOSPC (`SQLITE_FULL`) on the same two phases — the same
//!   fail-closed convergence;
//! - C3 commit-point `PowerLoss` both directions: Phase A (invisible, page-
//!   cache loss modeled) — the tick "reports success" but after reopen the
//!   watermark is back at the old high-water (which is itself >= every
//!   earlier value: no regression) with zero receipts, and the same-key redo
//!   produces exactly the phantom reading (the clock is a deterministic
//!   counter); Phase B (kill-9 after commit, visible) — every committed
//!   reading survives whole and the next fresh tick continues at exactly
//!   high-water + 1, never below;
//! - C4 torn WAL tail — every representative cut inside the final
//!   transactions' frame span (and one transaction deeper) leaves the
//!   watermark wholly old or wholly new (never torn: `integrity_check` passes
//!   and watermark == receipt count == max issued reading at every cut),
//!   never below the deeper surviving commit, and the same-key redo converges
//!   byte-equal onto the control readings;
//! - C5 replay storm — same-key replays 3+ times and once after reopen:
//!   byte-equal readings, the watermark advanced exactly once per key, no
//!   double-jump, fresh keys continue densely from the durable high-water;
//! - C6 trigger guards after recovery — the DDL guards (monotonic watermark,
//!   no insert/delete, frozen singleton, receipt immutability and
//!   watermark-bounded receipts) still abort raw tampering on a database
//!   that went through an injected crash and its recovery.
//!
//! Wall write-window matrix (schema v2 `wall_now`, trimmed per the same
//! single-entry-per-domain rule — the wall entry is `wall_now`):
//! - W1 pre-commit IOERR on `wall_now` (bootstrap and advance phases) —
//!   typed `Sqlite` failure, wall watermark wholly at the previous value
//!   and zero new wall receipts (zero partial state); the disarmed same-key
//!   redo converges to `max(durable, system)` (≥ the durable watermark —
//!   the wall redo is *monotone*, not byte-deterministic like the tick
//!   counter, because the source is the system clock); replay idempotent;
//! - W2 pre-commit ENOSPC (`SQLITE_FULL`) on the same two phases — the same
//!   fail-closed convergence;
//! - W3 commit-point `PowerLoss` both directions: Phase A (invisible) — the
//!   wall tick "reports success" but after reopen the wall watermark is
//!   wholly back at the old value with zero receipts, and the same-key redo
//!   re-issues at ≥ the durable watermark; Phase B (kill-9 after commit,
//!   visible) — every committed wall reading survives whole, replays are
//!   byte-equal, and a fresh key reads ≥ the durable watermark;
//! - W4 torn WAL tail on the wall write window — every representative cut
//!   inside the final wall transactions' frame span (and one transaction
//!   deeper) leaves the wall watermark wholly old or wholly new
//!   (`integrity_check` passes; watermark == the last surviving receipt's
//!   reading — advance and receipt co-live in one transaction), never below
//!   the deeper surviving commit; surviving replays stay byte-equal; the
//!   redo of missing keys re-issues at ≥ the surviving watermark.
//!
//! **Crash semantics disclaimer** (as in every prior matrix): kill-9
//! simulates *process* crashes; the OS page cache survives process death,
//! so a killed process is NOT a machine power loss. Writes the kernel
//! accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation —
//! both are *models* of a lost write, not real power-cut measurements.
//!
//! `allow: SIZE_OK` — one fault matrix per binary is the established repo
//! shape (all prior `*_fault_injection.rs` files are monolithic); fixtures
//! are duplicated per matrix file by convention.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_clock::{
    AuthorityClock, AuthorityClockError, NowDecision, NowRequest, Reading, WallNowDecision,
    WallReading,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::IdempotencyKey;
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-clock-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());
static NEXT: AtomicU64 = AtomicU64::new(1);

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn request(seed: u8) -> NowRequest {
    NowRequest {
        idempotency_key: key(seed),
    }
}

fn ticked(clock: &AuthorityClock, seed: u8) -> Reading {
    match clock.now(request(seed)).expect("now must tick") {
        NowDecision::Tick(reading) => reading,
        NowDecision::Replayed(reading) => panic!("fresh key cannot replay, got {reading}"),
    }
}

fn replayed(clock: &AuthorityClock, seed: u8) -> Reading {
    match clock.now(request(seed)).expect("now must replay") {
        NowDecision::Replayed(reading) => reading,
        NowDecision::Tick(reading) => panic!("expected Replayed, got Tick {reading}"),
    }
}

fn wall_advanced(clock: &AuthorityClock, seed: u8) -> WallReading {
    match clock
        .wall_now(request(seed))
        .expect("wall_now must advance")
    {
        WallNowDecision::Advanced(reading) => reading,
        WallNowDecision::Replayed(reading) => panic!("fresh key cannot replay, got {reading}"),
    }
}

fn wall_replayed(clock: &AuthorityClock, seed: u8) -> WallReading {
    match clock.wall_now(request(seed)).expect("wall_now must replay") {
        WallNowDecision::Replayed(reading) => reading,
        WallNowDecision::Advanced(reading) => panic!("expected Replayed, got Advanced {reading}"),
    }
}

// ---------------------------------------------------------------------------
// test roots, sandbox CWD, fault-VFS open (wait-matrix deviation note)
// ---------------------------------------------------------------------------

/// RAII test root: one fresh directory per scenario, removed on drop.
struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-clock-fault-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("create test root");
        Self(base)
    }

    fn base(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// RAII sandbox process CWD. The fault-VFS open runs `create_dir_all(root)`
/// on the literal URI root string, which is a relative OS path; the sandbox
/// keeps that junk directory tree inside a temp directory that is removed on
/// drop, so the worktree stays clean. All fault tests are serialized by
/// `FAULT_LOCK`, and every other test in this binary is either a no-op
/// (`crash_child_helper` without the scenario environment) or uses absolute
/// paths only.
struct SandboxCwd {
    previous: PathBuf,
    directory: PathBuf,
}

impl SandboxCwd {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "nlos-clock-fault-cwd-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create sandbox cwd");
        let previous = std::env::current_dir().expect("capture previous cwd");
        std::env::set_current_dir(&directory).expect("enter sandbox cwd");
        Self {
            previous,
            directory,
        }
    }
}

impl Drop for SandboxCwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn clock_database(base: &Path) -> PathBuf {
    base.join("clock-authority.db")
}

/// The URI root that routes the clock authority's connection through the
/// registered fault VFS (see the header deviation note).
fn fault_clock_root(base: &Path) -> String {
    // SQLite URI paths need forward slashes; Windows drive letters get the
    // `file:///C:/...` authority form or the URI fails to resolve.
    let uri_path = clock_database(base).to_string_lossy().replace('\\', "/");
    let trimmed = uri_path.trim_start_matches('/');
    format!("file:///{trimmed}?vfs={VFS_NAME}&tail=")
}

fn register_fault_vfs() {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
}

/// Opens the clock authority with its connection on the fault VFS. Pragmas
/// and the migration run while faults are disarmed, so the schema prefix is
/// always durable before any injection.
fn open_clock_fault(base: &Path) -> AuthorityClock {
    register_fault_vfs();
    AuthorityClock::open(fault_clock_root(base)).expect("open clock authority via fault vfs")
}

fn reopen_clock(base: &Path) -> AuthorityClock {
    AuthorityClock::open(base).expect("reopen clock authority")
}

// ---------------------------------------------------------------------------
// shared assertions (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

/// Full `Display` chain of an `AuthorityClockError`, top cause last, for
/// content assertions (e.g. that `SQLITE_FULL`'s message reaches the
/// caller).
fn error_chain(error: &AuthorityClockError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Asserts a typed storage failure whose cause chain names the injected
/// condition (`"i/o"` / `"ioerr"` / `"full"`): never a fake success, never
/// a panic.
fn assert_sqlite_error_chain(error: &AuthorityClockError, needles: &[&str]) {
    assert!(
        matches!(error, AuthorityClockError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(error).to_lowercase();
    assert!(
        needles.iter().any(|needle| chain.contains(needle)),
        "error chain must name the injected condition, got: {chain}"
    );
}

fn raw_count(database: &Path, sql: &str) -> i64 {
    let connection = Connection::open(database).expect("open raw reader");
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
}

/// Row counts of the two clock tables: `watermark`, `tick_receipts`.
fn assert_clock_counts(base: &Path, expected: [i64; 2]) {
    let tables = ["watermark", "tick_receipts"];
    for (table, want) in tables.iter().zip(expected) {
        assert_eq!(
            raw_count(
                &clock_database(base),
                &format!("SELECT COUNT(*) FROM {table}")
            ),
            want,
            "unexpected row count in {table}"
        );
    }
}

fn raw_reading(base: &Path) -> u64 {
    let value: i64 = raw_count(
        &clock_database(base),
        "SELECT reading FROM watermark WHERE singleton=1",
    );
    u64::try_from(value).expect("non-negative watermark")
}

fn raw_receipt_count(base: &Path) -> i64 {
    raw_count(&clock_database(base), "SELECT COUNT(*) FROM tick_receipts")
}

fn raw_wall_watermark(base: &Path) -> u64 {
    let value: i64 = raw_count(
        &clock_database(base),
        "SELECT reading_ms FROM wall_watermark WHERE singleton=1",
    );
    u64::try_from(value).expect("non-negative wall watermark")
}

fn raw_wall_receipt_count(base: &Path) -> i64 {
    raw_count(&clock_database(base), "SELECT COUNT(*) FROM wall_receipts")
}

/// Insertion-ordered wall receipts as `(idempotency_key, reading_ms)`.
fn raw_wall_receipts(base: &Path) -> Vec<(Vec<u8>, u64)> {
    let connection = Connection::open(clock_database(base)).expect("open raw reader");
    let mut statement = connection
        .prepare("SELECT idempotency_key, reading_ms FROM wall_receipts ORDER BY rowid")
        .expect("prepare wall receipt scan");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
        })
        .expect("query wall receipts");
    rows.map(|row| {
        let (key, reading) = row.expect("wall receipt row");
        (key, u64::try_from(reading).expect("non-negative reading"))
    })
    .collect()
}

fn assert_integrity(base: &Path) {
    let connection = Connection::open(clock_database(base)).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, root: &TestRoot) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_CLOCK_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_CLOCK_CRASH_CHILD_ROOT", root.base().as_os_str())
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

fn kill_and_reap(child: &mut Child) {
    child.kill().expect("force-terminate child");
    let status = child.wait().expect("wait child");
    assert!(
        !status.success(),
        "killed child must not exit cleanly: {status}"
    );
}

fn announce(marker: &str) {
    println!("{marker}");
    std::io::stdout().flush().expect("flush marker");
}

/// Decodes the plain marker: `READY <final-reading>`.
fn decode_marker(marker: &str) -> u64 {
    let payload = marker.trim().strip_prefix("READY ").expect("marker");
    payload.parse().expect("marker reading is a number")
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root)) = (
        std::env::var("NLOS_CLOCK_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_CLOCK_CRASH_CHILD_ROOT"),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    match scenario.as_str() {
        "tick-three-commit" => child_tick_commit(&root, 3),
        "tick-five-commit" => child_tick_commit(&root, 5),
        "wall-three-commit" => child_wall_commit(&root, 3),
        "wall-five-commit" => child_wall_commit(&root, 5),
        other => panic!("unknown crash child scenario {other}"),
    }
}

/// Child fixture: `count` fully committed ticks with the dense keys
/// `0xA1..`, one transaction each. Marker: `READY <final-reading>`.
fn child_tick_commit(root: &Path, count: u64) -> ! {
    let clock = reopen_clock(root);
    let mut last = Reading::from_u64(0);
    for index in 0..count {
        last = ticked(&clock, 0xA1 + u8::try_from(index).expect("small count"));
        assert_eq!(last.as_u64(), index + 1, "child ticks are dense");
    }
    announce(&format!("READY {}", last.as_u64()));
    let _keeper = clock;
    loop {
        std::thread::park();
    }
}

/// Child fixture: `count` fully committed wall readings with the keys
/// `0xB1..`, one transaction each. Marker: `READY <final-reading-ms>`.
fn child_wall_commit(root: &Path, count: u64) -> ! {
    let clock = reopen_clock(root);
    let mut last = WallReading::from_u64(0);
    for index in 0..count {
        let seed = 0xB1 + u8::try_from(index).expect("small count");
        match clock
            .wall_now(request(seed))
            .expect("child wall_now must advance")
        {
            WallNowDecision::Advanced(reading) => {
                assert!(
                    reading.as_u64() >= last.as_u64(),
                    "child wall readings never regress"
                );
                last = reading;
            }
            WallNowDecision::Replayed(reading) => {
                panic!("child wall keys are fresh, got replay {reading}")
            }
        }
    }
    announce(&format!("READY {}", last.as_u64()));
    let _keeper = clock;
    loop {
        std::thread::park();
    }
}

// ---------------------------------------------------------------------------
// WAL tail truncation (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

/// Returns `(page_size, frame_size, frame_count)` of a WAL image.
fn wal_frame_layout(wal: &[u8]) -> (usize, usize, usize) {
    assert!(wal.len() >= 32, "WAL must have a header");
    let page_size = match u32::from_be_bytes(wal[8..12].try_into().expect("page size field")) {
        1 => 65_536,
        value => value as usize,
    };
    assert!(page_size >= 512, "valid SQLite page size");
    let frame_size = 24 + page_size;
    let frame_count = (wal.len() - 32) / frame_size;
    assert!(frame_count > 0, "fixture must contain frames");
    (page_size, frame_size, frame_count)
}

/// Indices of the frames that carry a commit marker (a non-zero
/// database-size-in-pages field at frame-header offset 4..8 — the last
/// frame of each committed transaction).
fn commit_frames(wal: &[u8]) -> Vec<usize> {
    let (_, frame_size, frame_count) = wal_frame_layout(wal);
    (0..frame_count)
        .filter(|index| {
            let start = 32 + index * frame_size;
            u32::from_be_bytes(wal[start + 4..start + 8].try_into().expect("commit field")) != 0
        })
        .collect()
}

/// Every cut offset that truncates the WAL inside the final `txs_from_end+1`
/// transactions' frame span (from the end of the last surviving commit to
/// the last commit frame inclusive): frame boundaries, half-frame points and
/// last-byte points. `txs_from_end == 0` cuts away the last transaction
/// (torn or whole); `txs_from_end == 1` cuts away the last two.
fn tail_tx_cuts(wal: &[u8], txs_from_end: usize) -> Vec<usize> {
    let (_, frame_size, _) = wal_frame_layout(wal);
    let commits = commit_frames(wal);
    assert!(
        commits.len() >= 2 + txs_from_end,
        "fixture must have enough committed transactions"
    );
    let keep = commits[commits.len() - 2 - txs_from_end];
    let last = commits[commits.len() - 1];
    let mut cuts = vec![32 + (keep + 1) * frame_size];
    for index in (keep + 1)..=last {
        let start = 32 + index * frame_size;
        cuts.push(start);
        cuts.push(start + frame_size / 2);
        cuts.push(start + frame_size - 1);
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.retain(|cut| *cut < wal.len());
    cuts
}

/// The on-disk state a killed child leaves behind (clock authority),
/// restorable per sweep iteration so every torn-tail cut starts from
/// identical bytes.
struct ClockSnapshot {
    database: Vec<u8>,
    wal: Vec<u8>,
}

impl ClockSnapshot {
    fn capture(base: &Path) -> Self {
        Self {
            database: fs::read(clock_database(base)).expect("read database"),
            wal: fs::read(sibling_path(&clock_database(base), "-wal")).expect("read wal"),
        }
    }

    /// Restores the database, rewrites the WAL truncated to `cut` (or in
    /// full for `None`), and drops the stale wal-index.
    fn restore(&self, base: &Path, cut: Option<usize>) {
        fs::write(clock_database(base), &self.database).expect("restore database");
        let wal = match cut {
            Some(cut) => &self.wal[..cut],
            None => &self.wal[..],
        };
        fs::write(sibling_path(&clock_database(base), "-wal"), wal).expect("restore wal");
        let _ = fs::remove_file(sibling_path(&clock_database(base), "-shm"));
    }
}

// ---------------------------------------------------------------------------
// C1: pre-commit IOERR fails typed and converges (init + advance phases)
// ---------------------------------------------------------------------------

/// C1：`FailWritesAfter { 0, IoErr }` 分别注入 `now` 的两个阶段——
/// 首次初始化 tick 与既有高水位上的推进 tick——的提交写入 → typed
/// `Sqlite` 失败（错误链含 I/O 条件）；重开后水位整体保持旧值、零新回执
/// （零部分状态）、integrity ok；disarm 后同 key 重做 → 恰为幻影应得的
/// 确定性读数（时钟是确定性计数器）；重放逐字节幂等。
#[test]
#[allow(clippy::too_many_lines)]
fn clock_fault_now_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr-init");
    let root = TestRoot::new("ioerr-init");
    let clock = open_clock_fault(root.base());
    let first = request(0x01);

    // Phase 1: the first (initializing) tick under injected I/O errors.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = clock
        .now(first)
        .expect_err("initializing now must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_clock_counts(root.base(), [1, 0]);
    assert_eq!(raw_reading(root.base()), 0, "zero partial state");
    nlos_store_fault::disarm();

    assert_eq!(
        clock.inspect().expect("high-water after failed init"),
        Reading::from_u64(0)
    );
    assert_integrity(root.base());

    // The redo is deterministic: same key, watermark still 0 → reading 1.
    assert_eq!(ticked(&clock, 0x01), Reading::from_u64(1));
    assert_eq!(replayed(&clock, 0x01), Reading::from_u64(1));
    assert_clock_counts(root.base(), [1, 1]);

    // Phase 2: an advancing tick on the same faulted connection under the
    // same injection.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = clock
        .now(request(0x02))
        .expect_err("advancing now must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    nlos_store_fault::disarm();
    assert_clock_counts(root.base(), [1, 1]);
    assert_eq!(
        clock.inspect().expect("high-water stays at 1"),
        Reading::from_u64(1),
        "the previous high-water survives whole"
    );

    assert_eq!(ticked(&clock, 0x02), Reading::from_u64(2));
    drop(clock);
    let verified = reopen_clock(root.base());
    assert_eq!(
        verified.inspect().expect("durable high-water"),
        2_u64.into()
    );
    assert_eq!(replayed(&verified, 0x01), Reading::from_u64(1));
    assert_eq!(replayed(&verified, 0x02), Reading::from_u64(2));
    assert_clock_counts(root.base(), [1, 2]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// C2: pre-commit ENOSPC (SQLITE_FULL), same two phases
// ---------------------------------------------------------------------------

/// C2：`FailWritesAfter { 0, Full }`（`SQLITE_FULL`）对 init / advance 两个
/// 阶段同一收敛——typed 失败链含 "full"、水位整体保持旧值、零新回执；同
/// 一连接 disarm 后重做成功、行恰好一套、重放幂等、integrity ok。
#[test]
fn clock_fault_now_precommit_enospc_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("full-init");
    let root = TestRoot::new("full-init");
    let clock = open_clock_fault(root.base());

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = clock
        .now(request(0x01))
        .expect_err("initializing now must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert_clock_counts(root.base(), [1, 0]);
    assert_eq!(raw_reading(root.base()), 0);

    nlos_store_fault::disarm();
    assert_eq!(ticked(&clock, 0x01), Reading::from_u64(1));

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = clock
        .now(request(0x02))
        .expect_err("advancing now must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    nlos_store_fault::disarm();
    assert_clock_counts(root.base(), [1, 1]);
    assert_eq!(
        clock.inspect().expect("high-water stays at 1"),
        1_u64.into()
    );

    assert_eq!(ticked(&clock, 0x02), Reading::from_u64(2));
    drop(clock);
    let verified = reopen_clock(root.base());
    assert_eq!(
        verified.inspect().expect("durable high-water"),
        2_u64.into()
    );
    assert_eq!(replayed(&verified, 0x01), Reading::from_u64(1));
    assert_eq!(replayed(&verified, 0x02), Reading::from_u64(2));
    assert_clock_counts(root.base(), [1, 2]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// C3: PowerLoss commit point, both directions — old or new, never earlier
// ---------------------------------------------------------------------------

/// C3（commit 点断电双向）：
/// - Phase A（断电不可见方向，建模丢页缓存写）：`PowerLossAfter { 0 }` 下
///   tick "报告成功"；幸存连接先 drop（真实断电会杀死它）；重开 → 水位回
///   到旧高水位 0（旧值本身 ≥ 更早的一切值——不回退）、零回执；同 key 重
///   做 → 与幻影逐字节相等的读数 1；重放幂等。
/// - Phase B（提交后 kill-9 可见方向）：子进程完整提交 3 个 tick 后被强
///   杀；重开 → 水位 3、恰好 3 条回执；同 key 重放逐字节相等；fresh key
///   下一 tick 恰为 4（= 高水位 + 1，绝不回退到更早）；integrity ok。
#[test]
#[allow(clippy::too_many_lines)]
fn clock_fault_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    // Phase A: invisible direction (modeled lost page-cache write).
    {
        let _sandbox = SandboxCwd::new("pl-tick");
        let root = TestRoot::new("pl-tick");
        let clock = open_clock_fault(root.base());

        nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
        let phantom = ticked(&clock, 0x01);
        assert_eq!(phantom, Reading::from_u64(1));
        nlos_store_fault::disarm();
        // The surviving clock connection keeps a wal-index referencing
        // frames the disk never saw; it must die first (as a real power
        // loss would kill it) so recovery sees durable bytes alone.
        drop(clock);

        let recovered = reopen_clock(root.base());
        assert_eq!(
            recovered.inspect().expect("high-water after power loss"),
            Reading::from_u64(0),
            "the lost tick is wholly absent: the old high-water stands"
        );
        assert_clock_counts(root.base(), [1, 0]);
        assert_integrity(root.base());

        // Same-key redo converges onto exactly the phantom reading.
        assert_eq!(ticked(&recovered, 0x01), phantom);
        assert_eq!(replayed(&recovered, 0x01), phantom);
        assert_clock_counts(root.base(), [1, 1]);
        assert_integrity(root.base());
    }

    // Phase B: visible direction (kill-9 after the commit).
    {
        let root = TestRoot::new("kill9-tick");
        let mut child = spawn_child("tick-three-commit", &root);
        let marker = await_marker(&mut child);
        assert_eq!(decode_marker(&marker), 3, "child committed three ticks");
        kill_and_reap(&mut child);

        let clock = reopen_clock(root.base());
        assert_eq!(
            clock.inspect().expect("committed high-water survives"),
            Reading::from_u64(3),
            "all three committed readings survive whole"
        );
        assert_clock_counts(root.base(), [1, 3]);
        assert_eq!(replayed(&clock, 0xA1), Reading::from_u64(1));
        assert_eq!(replayed(&clock, 0xA2), Reading::from_u64(2));
        assert_eq!(replayed(&clock, 0xA3), Reading::from_u64(3));
        // The next fresh tick continues at exactly high-water + 1.
        assert_eq!(ticked(&clock, 0xA4), Reading::from_u64(4));
        assert_clock_counts(root.base(), [1, 4]);
        assert_integrity(root.base());
    }
}

// ---------------------------------------------------------------------------
// C4: torn WAL tail — watermark whole-or-absent, never below the survivor
// ---------------------------------------------------------------------------

/// C4：子进程提交 5 个 tick 后被强杀，父进程对 clock WAL 末段事务帧组的每
/// 个截断点与再深一段的截断点（合计 ≥6 个代表点）逐一恢复重开：水位恒为
/// 旧值或新值之一（绝不撕裂：integrity ok，且 **水位 == 回执数 == 已签发
/// 最大读数**——推进与回执同事务、同生同灭）；恒 ≥ 再深一段的幸存提交
/// （绝不回退到更早）；每个截断点同 key 重做缺失 tick → 与控制读数逐字
/// 节相等；重放幂等；完整恢复对照逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn clock_fault_torn_wal_tail_high_water_whole_or_absent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("torn-tick");
    let mut child = spawn_child("tick-five-commit", &root);
    let marker = await_marker(&mut child);
    assert_eq!(decode_marker(&marker), 5);
    kill_and_reap(&mut child);

    let snapshot = ClockSnapshot::capture(root.base());

    // Visible control: the untouched WAL recovers all five ticks whole.
    {
        let clock = reopen_clock(root.base());
        assert_eq!(clock.inspect().expect("control high-water"), 5_u64.into());
        assert_clock_counts(root.base(), [1, 5]);
        drop(clock);
    }

    let mut cuts = [
        tail_tx_cuts(&snapshot.wal, 0),
        tail_tx_cuts(&snapshot.wal, 1),
    ]
    .concat();
    cuts.sort_unstable();
    cuts.dedup();
    assert!(
        cuts.len() >= 6,
        "sweep must cover at least 6 representative cut points, got {cuts:?}"
    );

    for cut in cuts {
        snapshot.restore(root.base(), Some(cut));
        let clock = reopen_clock(root.base());
        assert_integrity(root.base());

        // Co-life: the watermark advance and its receipt are one
        // transaction, so watermark == receipt count == max issued reading
        // at every cut — a torn tail can split neither of them.
        let seen = clock.inspect().expect("high-water after torn tail");
        assert_eq!(
            u64::try_from(raw_receipt_count(root.base())).expect("receipt count"),
            seen.as_u64(),
            "receipts and the watermark live and die together"
        );
        assert_eq!(raw_reading(root.base()), seen.as_u64());
        assert!(
            (3..=4).contains(&seen.as_u64()),
            "the cut leaves tick 3 or 4 whole (never 5, never below 3): got {}",
            seen.as_u64()
        );

        // Same-key redo of the missing ticks converges byte-equal.
        for seed in 0xA1..=0xA5 {
            let expected = Reading::from_u64(u64::from(seed - 0xA0));
            if expected.as_u64() > seen.as_u64() {
                assert_eq!(
                    ticked(&clock, seed),
                    expected,
                    "redo of tick {seed:#x} must be byte-equal"
                );
            }
            assert_eq!(replayed(&clock, seed), expected);
        }
        assert_clock_counts(root.base(), [1, 5]);
        assert_eq!(
            clock.inspect().expect("high-water after redo"),
            5_u64.into()
        );
        assert_integrity(root.base());
        drop(clock);
    }

    // Full restore returns to the visible world.
    snapshot.restore(root.base(), None);
    let clock = reopen_clock(root.base());
    assert_eq!(
        clock.inspect().expect("high-water after full restore"),
        Reading::from_u64(5)
    );
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// C5: replay storm — no double-jump, no regress
// ---------------------------------------------------------------------------

/// C5：同 key 连放 3 次 + 重开后再放 → 每次返回与原始 durable 读数逐字节
/// 相等，水位每个 key 恰推进一次（不双跳）；fresh key 从 durable 高水位
/// 稠密续推（不回退）；integrity ok。
#[test]
fn clock_fault_replay_storm_no_double_jump_no_regress() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("storm");
    let clock = reopen_clock(root.base());

    assert_eq!(ticked(&clock, 0x01), Reading::from_u64(1));
    for _ in 0..3 {
        assert_eq!(replayed(&clock, 0x01), Reading::from_u64(1));
    }
    assert_eq!(
        clock.inspect().expect("storm advanced exactly once"),
        Reading::from_u64(1)
    );

    assert_eq!(ticked(&clock, 0x02), Reading::from_u64(2));
    for _ in 0..2 {
        assert_eq!(replayed(&clock, 0x02), Reading::from_u64(2));
    }
    assert_eq!(
        clock.inspect().expect("second key advanced once"),
        2_u64.into()
    );

    drop(clock);
    let verified = reopen_clock(root.base());
    assert_eq!(replayed(&verified, 0x01), Reading::from_u64(1));
    assert_eq!(replayed(&verified, 0x02), Reading::from_u64(2));
    assert_eq!(
        verified
            .inspect()
            .expect("replays after reopen advance nothing"),
        Reading::from_u64(2)
    );
    assert_eq!(ticked(&verified, 0x03), Reading::from_u64(3));
    assert_clock_counts(root.base(), [1, 3]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// C6: trigger guards survive injection and recovery
// ---------------------------------------------------------------------------

/// C6：一次注入崩溃（断电 tick + 恢复 + 重做）之后，raw SQL 的非法写仍被
/// DDL 守卫 abort——水位不可减、不可插第二行、不可删、singleton 冻结、
/// 回执不可改不可删、回执读数不可越过水位；守卫下的权威读路径照常服务、
/// 已 durable 读数未被扰动。
#[test]
#[allow(clippy::too_many_lines)]
fn clock_fault_trigger_guards_survive_injection_and_recovery() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("guards");
    let root = TestRoot::new("guards");
    let clock = open_clock_fault(root.base());
    assert_eq!(ticked(&clock, 0x01), Reading::from_u64(1));

    // Crash through a tick window: the phantom advance is lost, then
    // redone — the recovered database is a post-crash recovery product.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = ticked(&clock, 0x02);
    assert_eq!(phantom, Reading::from_u64(2));
    nlos_store_fault::disarm();
    drop(clock);
    let recovered = reopen_clock(root.base());
    assert_eq!(
        recovered.inspect().expect("phantom vanished whole"),
        Reading::from_u64(1)
    );
    assert_clock_counts(root.base(), [1, 1]);
    assert_eq!(ticked(&recovered, 0x02), phantom);
    assert_clock_counts(root.base(), [1, 2]);

    let raw = Connection::open(clock_database(root.base())).expect("open raw connection");
    // The watermark can never move backwards (monotonic guard).
    assert!(
        raw.execute("UPDATE watermark SET reading=1", []).is_err(),
        "the watermark can never decrease"
    );
    assert!(raw.execute("UPDATE watermark SET reading=0", []).is_err());
    // The singleton identity is frozen.
    assert!(
        raw.execute("UPDATE watermark SET singleton=2", []).is_err(),
        "the watermark singleton is trigger-frozen"
    );
    // The watermark row is single and durable.
    assert!(
        raw.execute(
            "INSERT INTO watermark (singleton, reading) VALUES (1, 99)",
            []
        )
        .is_err(),
        "no second watermark row can be inserted"
    );
    assert!(
        raw.execute(
            "INSERT INTO watermark (singleton, reading) VALUES (2, 99)",
            []
        )
        .is_err()
    );
    assert!(raw.execute("DELETE FROM watermark", []).is_err());
    // The tick receipts are immutable and durable.
    assert!(
        raw.execute("UPDATE tick_receipts SET reading=99", [])
            .is_err(),
        "a tick receipt can never be rewritten"
    );
    assert!(
        raw.execute(
            "UPDATE tick_receipts SET idempotency_key=x'CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC'",
            []
        )
        .is_err()
    );
    assert!(raw.execute("DELETE FROM tick_receipts", []).is_err());
    // A receipt can never record a reading beyond the watermark.
    assert!(
        raw.execute(
            "INSERT INTO tick_receipts (idempotency_key, reading)
             VALUES (x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB', 99)",
            []
        )
        .is_err(),
        "receipts are watermark-bounded"
    );

    // The guarded authority still serves reads; the durable readings are
    // untouched.
    assert_eq!(
        recovered.inspect().expect("high-water after tamper sweep"),
        Reading::from_u64(2)
    );
    assert_eq!(replayed(&recovered, 0x01), Reading::from_u64(1));
    assert_eq!(replayed(&recovered, 0x02), Reading::from_u64(2));
    assert_clock_counts(root.base(), [1, 2]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W1: wall pre-commit IOERR fails typed and converges (bootstrap + advance)
// ---------------------------------------------------------------------------

/// W1：`FailWritesAfter { 0, IoErr }` 分别注入 `wall_now` 的两个阶段——
/// 首次 bootstrap（以系统时钟初始化 durable 水位）与既有水位上的推进——的
/// 提交写入 → typed `Sqlite` 失败（错误链含 I/O 条件）；wall 水位整体保持
/// 旧值、零新 wall 回执（零部分状态）、integrity ok；disarm 后同 key 重做
/// 收敛到 `max(durable, system)`（≥ durable 水位；wall 重做是**单调**收敛
/// 而非 tick 那样的逐字节确定性收敛——源是系统时钟）；重放幂等。
#[test]
#[allow(clippy::too_many_lines)]
fn wall_fault_now_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("wall-ioerr-init");
    let root = TestRoot::new("wall-ioerr-init");
    let clock = open_clock_fault(root.base());

    // Phase 1: the bootstrap wall_now under injected I/O errors.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = clock
        .wall_now(request(0x01))
        .expect_err("bootstrap wall_now must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_eq!(raw_wall_receipt_count(root.base()), 0);
    assert_eq!(raw_wall_watermark(root.base()), 0, "zero partial state");
    nlos_store_fault::disarm();

    assert_eq!(
        clock
            .inspect_wall()
            .expect("wall high-water after failed bootstrap"),
        WallReading::from_u64(0)
    );
    assert_integrity(root.base());

    // The redo converges monotonically onto max(durable, system).
    let redo = wall_advanced(&clock, 0x01);
    assert!(redo.as_u64() > 0, "bootstrap reading is the system clock");
    assert_eq!(
        clock.inspect_wall().expect("watermark moved onto the redo"),
        redo
    );
    assert_eq!(wall_replayed(&clock, 0x01), redo);
    assert_eq!(raw_wall_receipt_count(root.base()), 1);

    // Phase 2: an advancing wall_now on the same faulted connection.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = clock
        .wall_now(request(0x02))
        .expect_err("advancing wall_now must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    nlos_store_fault::disarm();
    assert_eq!(raw_wall_receipt_count(root.base()), 1);
    assert_eq!(
        clock.inspect_wall().expect("wall high-water stays at redo"),
        redo,
        "the previous wall high-water survives whole"
    );

    let second = wall_advanced(&clock, 0x02);
    assert!(second.as_u64() >= redo.as_u64(), "wall never regresses");
    drop(clock);
    let verified = reopen_clock(root.base());
    assert_eq!(
        verified.inspect_wall().expect("durable wall high-water"),
        second
    );
    assert_eq!(wall_replayed(&verified, 0x01), redo);
    assert_eq!(wall_replayed(&verified, 0x02), second);
    assert_eq!(raw_wall_receipt_count(root.base()), 2);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W2: wall pre-commit ENOSPC (SQLITE_FULL), same two phases
// ---------------------------------------------------------------------------

/// W2：`FailWritesAfter { 0, Full }`（`SQLITE_FULL`）对 wall bootstrap /
/// advance 两个阶段同一收敛——typed 失败链含 "full"、wall 水位整体保持旧
/// 值、零新回执；disarm 后重做单调收敛、行恰好一套、重放幂等、integrity ok。
#[test]
fn wall_fault_now_precommit_enospc_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("wall-full-init");
    let root = TestRoot::new("wall-full-init");
    let clock = open_clock_fault(root.base());

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = clock
        .wall_now(request(0x01))
        .expect_err("bootstrap wall_now must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert_eq!(raw_wall_receipt_count(root.base()), 0);
    assert_eq!(raw_wall_watermark(root.base()), 0);

    nlos_store_fault::disarm();
    let redo = wall_advanced(&clock, 0x01);
    assert_eq!(clock.inspect_wall().expect("watermark == redo"), redo);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = clock
        .wall_now(request(0x02))
        .expect_err("advancing wall_now must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    nlos_store_fault::disarm();
    assert_eq!(raw_wall_receipt_count(root.base()), 1);
    assert_eq!(
        clock.inspect_wall().expect("wall high-water stays at redo"),
        redo
    );

    let second = wall_advanced(&clock, 0x02);
    assert!(second.as_u64() >= redo.as_u64());
    drop(clock);
    let verified = reopen_clock(root.base());
    assert_eq!(
        verified.inspect_wall().expect("durable wall high-water"),
        second
    );
    assert_eq!(wall_replayed(&verified, 0x01), redo);
    assert_eq!(wall_replayed(&verified, 0x02), second);
    assert_eq!(raw_wall_receipt_count(root.base()), 2);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W3: wall PowerLoss commit point, both directions
// ---------------------------------------------------------------------------

/// W3（wall 写窗口 commit 点断电双向）：
/// - Phase A（不可见方向，`PowerLossAfter { 0 }` 建模丢写）：wall "报告成
///   功"；幸存连接先 drop；重开 → wall 水位整体回到旧值 0、零回执（不回
///   退到任何更早值）；同 key 重做单调收敛并推进水位；重放幂等。
/// - Phase B（提交后 kill-9 可见方向）：子进程完整提交 3 个 wall 读数后
///   被强杀；重开 → 水位 == 通告终值、恰好 3 条回执且读数非降；已提交 key
///   重放逐字节相等；fresh key 读数 ≥ durable 水位（不回退）；integrity ok。
#[test]
#[allow(clippy::too_many_lines)]
fn wall_fault_power_loss_commit_point_converges_both_ways() {
    fn wall_key(index: usize) -> u8 {
        0xB1 + u8::try_from(index).expect("small count")
    }

    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    // Phase A: invisible direction (modeled lost page-cache write).
    {
        let _sandbox = SandboxCwd::new("wall-pl");
        let root = TestRoot::new("wall-pl");
        let clock = open_clock_fault(root.base());

        nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
        let phantom = wall_advanced(&clock, 0x01);
        assert!(phantom.as_u64() > 0);
        nlos_store_fault::disarm();
        drop(clock);

        let recovered = reopen_clock(root.base());
        assert_eq!(
            recovered
                .inspect_wall()
                .expect("wall high-water after power loss"),
            WallReading::from_u64(0),
            "the lost wall reading is wholly absent: the old watermark stands"
        );
        assert_eq!(raw_wall_receipt_count(root.base()), 0);
        assert_integrity(root.base());

        let redo = wall_advanced(&recovered, 0x01);
        assert_eq!(
            recovered.inspect_wall().expect("watermark == redo"),
            redo,
            "the redo re-issues at max(durable, system)"
        );
        assert_eq!(wall_replayed(&recovered, 0x01), redo);
        assert_eq!(raw_wall_receipt_count(root.base()), 1);
        assert_integrity(root.base());
    }

    // Phase B: visible direction (kill-9 after the commit).
    {
        let root = TestRoot::new("wall-kill9");
        let mut child = spawn_child("wall-three-commit", &root);
        let marker = await_marker(&mut child);
        let final_ms = decode_marker(&marker);
        kill_and_reap(&mut child);

        let clock = reopen_clock(root.base());
        assert_eq!(
            clock
                .inspect_wall()
                .expect("committed wall watermark survives"),
            WallReading::from_u64(final_ms),
            "the committed wall reading survives whole"
        );
        let receipts = raw_wall_receipts(root.base());
        assert_eq!(receipts.len(), 3, "exactly three committed wall receipts");
        for (index, (_, reading_ms)) in receipts.iter().enumerate() {
            assert!(
                *reading_ms <= final_ms,
                "committed wall readings never exceed the final watermark"
            );
            assert_eq!(
                wall_replayed(&clock, wall_key(index)),
                WallReading::from_u64(*reading_ms),
                "replays of the child's committed keys are byte-equal"
            );
        }
        let fresh = wall_advanced(&clock, wall_key(3));
        assert!(
            fresh.as_u64() >= final_ms,
            "a fresh key reads at least the durable watermark"
        );
        assert_eq!(raw_wall_receipt_count(root.base()), 4);
        assert_integrity(root.base());
    }
}

// ---------------------------------------------------------------------------
// W4: wall torn WAL tail — watermark whole-or-absent, never below survivor
// ---------------------------------------------------------------------------

/// W4：wall 子进程提交 5 个读数后被强杀，父进程对 wall 末段事务帧组的每个
/// 截断点与再深一段的截断点（合计 ≥6 代表点）逐一恢复重开：wall 水位恒为
/// 旧值或新值之一（绝不撕裂：integrity ok；**水位 == 幸存最后一条回执的
/// 读数**——推进与回执同事务、同生同灭）、恒 ≥ 再深一段的幸存提交（不回
/// 退）；幸存回执与控制值逐字节相等、重放幂等；缺失 key 重做单调收敛（≥
/// 幸存水位，wall 无逐字节确定性）；完整恢复对照相等。
#[test]
#[allow(clippy::too_many_lines)]
fn wall_fault_torn_wal_tail_watermark_whole_or_absent() {
    fn wall_key(index: usize) -> u8 {
        0xB1 + u8::try_from(index).expect("small count")
    }

    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("wall-torn");
    let mut child = spawn_child("wall-five-commit", &root);
    let marker = await_marker(&mut child);
    let final_ms = decode_marker(&marker);
    kill_and_reap(&mut child);

    let snapshot = ClockSnapshot::capture(root.base());

    // Visible control: the untouched WAL recovers all five wall txs whole.
    let control: Vec<u64> = {
        let clock = reopen_clock(root.base());
        assert_eq!(
            clock.inspect_wall().expect("control wall watermark"),
            WallReading::from_u64(final_ms)
        );
        let receipts = raw_wall_receipts(root.base());
        assert_eq!(receipts.len(), 5, "control has all five wall receipts");
        drop(clock);
        receipts.into_iter().map(|(_, reading)| reading).collect()
    };
    for pair in control.windows(2) {
        assert!(pair[0] <= pair[1], "control wall readings never regress");
    }

    let mut cuts = [
        tail_tx_cuts(&snapshot.wal, 0),
        tail_tx_cuts(&snapshot.wal, 1),
    ]
    .concat();
    cuts.sort_unstable();
    cuts.dedup();
    assert!(
        cuts.len() >= 6,
        "sweep must cover at least 6 representative cut points, got {cuts:?}"
    );

    for cut in cuts {
        snapshot.restore(root.base(), Some(cut));
        let clock = reopen_clock(root.base());
        assert_integrity(root.base());

        // Co-life: the watermark advance and its receipt are one
        // transaction, so the watermark equals the last surviving receipt's
        // reading, is one of the committed control values, and never falls
        // below the deepest surviving commit (the third wall reading).
        let surviving = raw_wall_receipts(root.base());
        assert!(!surviving.is_empty(), "a wall receipt always survives");
        let watermark = clock.inspect_wall().expect("wall watermark after cut");
        assert_eq!(
            watermark.as_u64(),
            surviving.last().expect("nonempty").1,
            "watermark and the last receipt live and die together"
        );
        assert!(
            control.contains(&watermark.as_u64()),
            "the watermark is a committed value, never torn: got {}",
            watermark.as_u64()
        );
        assert!(
            watermark.as_u64() >= control[2],
            "never below the deeper surviving commit"
        );

        // Surviving receipts are the byte-equal control prefix; their
        // replays are byte-equal too.
        for (index, (_, reading_ms)) in surviving.iter().enumerate() {
            assert_eq!(*reading_ms, control[index], "survivor == control prefix");
            assert_eq!(
                wall_replayed(&clock, wall_key(index)),
                WallReading::from_u64(*reading_ms)
            );
        }

        // Redo of the missing keys converges monotonically at or above the
        // surviving watermark (wall redo is monotone, not byte-equal).
        for index in surviving.len()..control.len() {
            let redo = wall_advanced(&clock, wall_key(index));
            assert!(
                redo.as_u64() >= watermark.as_u64(),
                "redo never falls below the surviving watermark"
            );
        }
        assert_eq!(
            raw_wall_receipt_count(root.base()),
            5,
            "all five keys accounted for after redo"
        );
        drop(clock);
    }

    // Full restore returns to the visible world.
    snapshot.restore(root.base(), None);
    let clock = reopen_clock(root.base());
    assert_eq!(
        clock
            .inspect_wall()
            .expect("wall watermark after full restore"),
        WallReading::from_u64(final_ms)
    );
    assert_integrity(root.base());
}
