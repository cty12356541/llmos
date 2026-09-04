//! B-APPLICATION-001 (kill-window): fault-injection matrix for the durable
//! Application/Installation authority — the single
//! `ApplicationAuthority::install_application` entry, plus the trigger-
//! guarded tamper surface after an injected crash and recovery.
//!
//! Harness and fixtures follow the established matrices exactly, most
//! recently `nlos-clock/tests/clock_fault_injection.rs` (kill-9 children
//! synchronized through piped `READY` markers — never sleeps, `FAULT_LOCK`
//! process-wide serialization, WAL tail truncation sweeps, typed error-chain
//! assertions, raw row counts, `PRAGMA integrity_check` per scenario).
//!
//! **Fault-VFS plumbing (documented harness constraint, same deviation as
//! the clock/wait/channel/topic matrices)**: `ApplicationAuthority` has no
//! `open_with_vfs` constructor and the workspace forbids `unsafe`, so the
//! shim is routed in through a `SQLite` **URI filename**: `rusqlite`'s
//! `Connection::open` sets `SQLITE_OPEN_URI`, and `ApplicationAuthority::
//! open` passes `root.join("application-authority.db")` through unchanged
//! (and skips `create_dir_all` for `file:` roots), so a root of
//! `file:<db>?vfs=<shim>&tail=` routes that one authority connection through
//! the registered fault VFS (the appended `/application-authority.db` tail
//! lands in the ignored `tail=` query parameter). Pragmas and the migration
//! run while faults are disarmed, so the schema prefix is always durable
//! before any injection. The artifact store (whose verified package receipts
//! every scenario consumes) and every reopen / raw reader / integrity check
//! use the plain default VFS and can never be faulted.
//!
//! Matrix (the nlos-clock matrix mirrored onto the installation domain's
//! single entry, `install_application`):
//! - C1 pre-commit IOERR on both install phases (first-call application
//!   creation and generation advance) — typed `Sqlite` error whose chain
//!   names the injected condition, zero applications and zero installation
//!   receipts after reopen (zero partial state: the application row and the
//!   receipt are one transaction), the disarmed same-key redo converges
//!   onto the deterministic installation the phantom would have had, and
//!   the durable result replays byte-equal;
//! - C2 pre-commit ENOSPC (`SQLITE_FULL`) on the same two phases — the same
//!   fail-closed convergence;
//! - C3 commit-point `PowerLoss` both directions: Phase A (invisible, page-
//!   cache loss modeled) — the install "reports success" but after reopen
//!   the application is wholly absent (no row, no receipt), and the
//!   same-key redo produces exactly the phantom installation (installation
//!   ids derive from key + application + generation, deterministically);
//!   Phase B (kill-9 after commit, visible) — every committed installation
//!   survives whole and the next fresh install continues at exactly
//!   generation + 1, never below;
//! - C4 torn WAL tail — every representative cut inside the final
//!   transactions' frame span (and one transaction deeper) leaves the
//!   application generation wholly old or wholly new (never torn:
//!   `integrity_check` passes and generation == receipt count == max
//!   installed generation at every cut — co-life), never below the deeper
//!   surviving commit, and the same-key redo converges byte-equal onto the
//!   control installations;
//! - C5 replay storm — same-key replays 3+ times and once after reopen:
//!   byte-equal receipts, the generation advanced exactly once per key, no
//!   double-jump, fresh keys continue densely from the durable generation;
//! - C6 trigger guards after recovery — the DDL guards (monotonic
//!   generation, frozen identity, durable rows, receipt immutability and
//!   durability, current-generation-bounded receipts, legal status
//!   transitions with terminal `disabled`) still abort raw tampering on a
//!   database that went through an injected crash and its recovery.
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

use nlos_application::{
    ApplicationAuthority, ApplicationAuthorityError, InstallApplicationRequest, InstallDecision,
    derive_application_id, derive_installation_id,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{Generation, IdempotencyKey, ReceiptId};
use rusqlite::Connection;

mod support;

use support::{TestStack, authority_database};

const VFS_NAME: &str = "nlos-application-fault";

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

fn request(receipt_id: ReceiptId, seed: u8, at_ms: u64) -> InstallApplicationRequest {
    InstallApplicationRequest {
        package_verification_receipt_id: receipt_id,
        idempotency_key: key(seed),
        installed_at_ms: at_ms,
    }
}

fn installed(
    authority: &ApplicationAuthority,
    stack: &TestStack,
    receipt_id: ReceiptId,
    seed: u8,
    at_ms: u64,
) -> nlos_application::InstallationReceipt {
    match authority
        .install_application(&stack.artifacts, request(receipt_id, seed, at_ms))
        .expect("install must succeed")
    {
        InstallDecision::Installed(receipt) => receipt,
        InstallDecision::Replayed(receipt) => panic!("fresh key cannot replay, got {receipt:?}"),
    }
}

fn replayed(
    authority: &ApplicationAuthority,
    stack: &TestStack,
    receipt_id: ReceiptId,
    seed: u8,
    at_ms: u64,
) -> nlos_application::InstallationReceipt {
    match authority
        .install_application(&stack.artifacts, request(receipt_id, seed, at_ms))
        .expect("install must replay")
    {
        InstallDecision::Replayed(receipt) => receipt,
        InstallDecision::Installed(receipt) => {
            panic!("expected Replayed, got Installed {receipt:?}")
        }
    }
}

/// The child install fixture's dense command schedule: key `0xA0 + ordinal`
/// installs generation `ordinal` at `child_at_ms(ordinal)`.
const fn child_at_ms(ordinal: u64) -> u64 {
    1_999 + ordinal
}

// ---------------------------------------------------------------------------
// test roots, sandbox CWD, fault-VFS open (clock-matrix deviation note)
// ---------------------------------------------------------------------------

/// RAII test root: one fresh directory per scenario, removed on drop.
struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-application-fault-{label}-{}-{suffix}",
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

/// RAII sandbox process CWD: keeps any literal-URI junk path out of the
/// worktree (see the clock matrix note). All fault tests are serialized by
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
            "nlos-application-fault-cwd-{label}-{}-{suffix}",
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

/// The URI root that routes the application authority's connection through
/// the registered fault VFS (see the header deviation note).
fn fault_authority_root(base: &Path) -> String {
    // SQLite URI paths need forward slashes; Windows drive letters get the
    // `file:///C:/...` authority form or the URI fails to resolve.
    let uri_path = authority_database(base)
        .to_string_lossy()
        .replace('\\', "/");
    let trimmed = uri_path.trim_start_matches('/');
    format!("file:///{trimmed}?vfs={VFS_NAME}&tail=")
}

fn register_fault_vfs() {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
}

/// Opens the application authority with its connection on the fault VFS.
fn open_application_fault(base: &Path) -> ApplicationAuthority {
    register_fault_vfs();
    ApplicationAuthority::open(fault_authority_root(base)).expect("open authority via fault vfs")
}

fn reopen_application(base: &Path) -> ApplicationAuthority {
    ApplicationAuthority::open(base).expect("reopen application authority")
}

// ---------------------------------------------------------------------------
// shared assertions (clock_fault_injection.rs 范式)
// ---------------------------------------------------------------------------

/// Full `Display` chain of an `ApplicationAuthorityError`, top cause last,
/// for content assertions (e.g. that `SQLITE_FULL`'s message reaches the
/// caller).
fn error_chain(error: &ApplicationAuthorityError) -> String {
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
fn assert_sqlite_error_chain(error: &ApplicationAuthorityError, needles: &[&str]) {
    assert!(
        matches!(error, ApplicationAuthorityError::Sqlite(_)),
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

/// Row counts of the two authority tables: `applications`,
/// `installation_receipts`.
fn assert_authority_counts(base: &Path, expected: [i64; 2]) {
    let database = authority_database(base);
    for (table, want) in ["applications", "installation_receipts"]
        .iter()
        .zip(expected)
    {
        assert_eq!(
            raw_count(&database, &format!("SELECT COUNT(*) FROM {table}")),
            want,
            "unexpected row count in {table}"
        );
    }
}

/// The durable generation (0 = no application row exists yet).
fn raw_generation(base: &Path) -> u64 {
    let connection = Connection::open(authority_database(base)).expect("open raw reader");
    let value: Option<i64> = connection
        .query_row(
            "SELECT current_installation_generation FROM applications LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();
    u64::try_from(value.unwrap_or(0)).expect("non-negative generation")
}

fn assert_integrity(base: &Path) {
    let connection = Connection::open(authority_database(base)).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (clock_fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, root: &TestRoot, artifacts: &Path, receipt_id: ReceiptId) -> Child {
    // ASCII-safe env encoding (Windows lesson): the receipt id crosses the
    // process boundary as comma-separated decimals, never raw bytes.
    let receipt_env = receipt_id
        .as_bytes()
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_APPLICATION_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_APPLICATION_CRASH_CHILD_ROOT", root.base().as_os_str())
        .env(
            "NLOS_APPLICATION_CRASH_CHILD_ARTIFACTS",
            artifacts.as_os_str(),
        )
        .env("NLOS_APPLICATION_CRASH_CHILD_RECEIPT", receipt_env)
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

/// Decodes the plain marker: `READY <final-generation>`.
fn decode_marker(marker: &str) -> u64 {
    let payload = marker.trim().strip_prefix("READY ").expect("marker");
    payload.parse().expect("marker generation is a number")
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root), Ok(artifacts), Ok(receipt)) = (
        std::env::var("NLOS_APPLICATION_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_APPLICATION_CRASH_CHILD_ROOT"),
        std::env::var("NLOS_APPLICATION_CRASH_CHILD_ARTIFACTS"),
        std::env::var("NLOS_APPLICATION_CRASH_CHILD_RECEIPT"),
    ) else {
        return;
    };
    let receipt_bytes: [u8; 16] = receipt
        .split(',')
        .filter_map(|token| token.parse::<u8>().ok())
        .collect::<Vec<_>>()
        .try_into()
        .expect("16 receipt id bytes");
    let root = PathBuf::from(root);
    let artifacts = PathBuf::from(artifacts);
    match scenario.as_str() {
        "install-three-commit" => {
            child_install_commit(&root, &artifacts, ReceiptId::from_bytes(receipt_bytes), 3);
        }
        "install-five-commit" => {
            child_install_commit(&root, &artifacts, ReceiptId::from_bytes(receipt_bytes), 5);
        }
        other => panic!("unknown crash child scenario {other}"),
    }
}

/// Child fixture: `count` fully committed installs with the dense keys
/// `0xA1..`, one transaction each, over the artifact store the parent
/// prepared. Marker: `READY <final-generation>`.
fn child_install_commit(
    root: &Path,
    artifacts_root: &Path,
    receipt_id: ReceiptId,
    count: u64,
) -> ! {
    let artifacts =
        nlos_artifact::ArtifactStore::open(artifacts_root).expect("child artifact store");
    let authority = reopen_application(root);
    let mut last = Generation::INITIAL;
    for index in 0..count {
        let seed = u8::try_from(0xA1 + index).expect("small count");
        let ordinal = index + 1;
        match authority
            .install_application(&artifacts, request(receipt_id, seed, child_at_ms(ordinal)))
            .expect("child install must commit")
        {
            InstallDecision::Installed(receipt) => {
                assert_eq!(
                    receipt.installation_generation.get(),
                    ordinal,
                    "child installs are dense"
                );
                last = receipt.installation_generation;
            }
            InstallDecision::Replayed(receipt) => {
                panic!("fresh child key cannot replay, got {receipt:?}")
            }
        }
    }
    announce(&format!("READY {}", last.get()));
    let _keeper = (authority, artifacts);
    loop {
        std::thread::park();
    }
}

// ---------------------------------------------------------------------------
// WAL tail truncation (clock_fault_injection.rs 范式)
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

/// The on-disk state a killed child leaves behind (application authority),
/// restorable per sweep iteration so every torn-tail cut starts from
/// identical bytes.
struct AuthoritySnapshot {
    database: Vec<u8>,
    wal: Vec<u8>,
}

impl AuthoritySnapshot {
    fn capture(base: &Path) -> Self {
        Self {
            database: fs::read(authority_database(base)).expect("read database"),
            wal: fs::read(sibling_path(&authority_database(base), "-wal")).expect("read wal"),
        }
    }

    /// Restores the database, rewrites the WAL truncated to `cut` (or in
    /// full for `None`), and drops the stale wal-index.
    fn restore(&self, base: &Path, cut: Option<usize>) {
        fs::write(authority_database(base), &self.database).expect("restore database");
        let wal = match cut {
            Some(cut) => &self.wal[..cut],
            None => &self.wal[..],
        };
        fs::write(sibling_path(&authority_database(base), "-wal"), wal).expect("restore wal");
        let _ = fs::remove_file(sibling_path(&authority_database(base), "-shm"));
    }
}

// ---------------------------------------------------------------------------
// C1: pre-commit IOERR fails typed and converges (create + advance phases)
// ---------------------------------------------------------------------------

/// C1：`FailWritesAfter { 0, IoErr }` 分别注入 install 的两个阶段——首个
/// 安装（建 application 行 + gen 1 receipt）与既有代际上的重装推进——的
/// 提交写入 → typed `Sqlite` 失败（错误链含 I/O 条件）；重开后行计数整体
/// 保持旧值（零部分状态：application 行与回执同事务）、integrity ok；
/// disarm 后同 key 重做 → 恰为幻影应得的确定性 installation id；重放逐
/// 字节幂等。
#[test]
#[allow(clippy::too_many_lines)]
fn application_fault_install_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr-init");
    let stack = TestStack::new("ioerr-init-artifacts", 0x31);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let root = TestRoot::new("ioerr-init");
    let authority = open_application_fault(root.base());

    // Phase 1: the first (application-creating) install under injected
    // I/O errors.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .install_application(&stack.artifacts, request(verified.receipt_id, 0x01, 2_000))
        .expect_err("creating install must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_authority_counts(root.base(), [0, 0]);
    assert_eq!(raw_generation(root.base()), 0, "zero partial state");
    nlos_store_fault::disarm();

    assert_integrity(root.base());

    // The redo is deterministic: same key, no application yet → generation 1
    // with the derived installation id the phantom would have had.
    assert_eq!(
        installed(&authority, &stack, verified.receipt_id, 0x01, 2_000).installation_id,
        derive_installation_id(
            key(0x01),
            derive_application_id(verified.package_id),
            Generation::INITIAL,
        )
    );
    assert_authority_counts(root.base(), [1, 1]);
    replayed(&authority, &stack, verified.receipt_id, 0x01, 2_000);

    // Phase 2: an advancing install on the same faulted connection under
    // the same injection.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .install_application(&stack.artifacts, request(verified.receipt_id, 0x02, 3_000))
        .expect_err("advancing install must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    nlos_store_fault::disarm();
    assert_authority_counts(root.base(), [1, 1]);
    assert_eq!(
        raw_generation(root.base()),
        1,
        "the previous generation survives whole"
    );

    assert_eq!(
        installed(&authority, &stack, verified.receipt_id, 0x02, 3_000)
            .installation_generation
            .get(),
        2
    );
    drop(authority);
    let verified_state = reopen_application(root.base());
    assert_eq!(
        replayed(&verified_state, &stack, verified.receipt_id, 0x01, 2_000).installation_generation,
        Generation::INITIAL
    );
    assert_eq!(
        replayed(&verified_state, &stack, verified.receipt_id, 0x02, 3_000)
            .installation_generation
            .get(),
        2
    );
    assert_authority_counts(root.base(), [1, 2]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// C2: pre-commit ENOSPC (SQLITE_FULL), same two phases
// ---------------------------------------------------------------------------

/// C2：`FailWritesAfter { 0, Full }`（`SQLITE_FULL`）对建行 / 推进两个阶段
/// 同一收敛——typed 失败链含 "full"、行计数整体保持旧值；同一连接 disarm
/// 后重做成功、行恰好一套、重放幂等、integrity ok。
#[test]
fn application_fault_install_precommit_enospc_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("full-init");
    let stack = TestStack::new("full-init-artifacts", 0x32);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let root = TestRoot::new("full-init");
    let authority = open_application_fault(root.base());

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .install_application(&stack.artifacts, request(verified.receipt_id, 0x01, 2_000))
        .expect_err("creating install must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert_authority_counts(root.base(), [0, 0]);
    assert_eq!(raw_generation(root.base()), 0);

    nlos_store_fault::disarm();
    assert_eq!(
        installed(&authority, &stack, verified.receipt_id, 0x01, 2_000).installation_generation,
        Generation::INITIAL
    );

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .install_application(&stack.artifacts, request(verified.receipt_id, 0x02, 3_000))
        .expect_err("advancing install must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    nlos_store_fault::disarm();
    assert_authority_counts(root.base(), [1, 1]);
    assert_eq!(raw_generation(root.base()), 1);

    assert_eq!(
        installed(&authority, &stack, verified.receipt_id, 0x02, 3_000)
            .installation_generation
            .get(),
        2
    );
    drop(authority);
    let verified_state = reopen_application(root.base());
    assert_eq!(
        replayed(&verified_state, &stack, verified.receipt_id, 0x01, 2_000).installation_generation,
        Generation::INITIAL
    );
    assert_eq!(
        replayed(&verified_state, &stack, verified.receipt_id, 0x02, 3_000)
            .installation_generation
            .get(),
        2
    );
    assert_authority_counts(root.base(), [1, 2]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// C3: PowerLoss commit point, both directions — old or new, never earlier
// ---------------------------------------------------------------------------

/// C3（commit 点断电双向）：
/// - Phase A（断电不可见方向，建模丢页缓存写）：`PowerLossAfter { 0 }` 下
///   安装 "报告成功"；幸存连接先 drop（真实断电会杀死它）；重开 →
///   application 整体不存在（行与回执全无，旧状态 "0/无" 成立——绝不回退
///   到更早的安装）；同 key 重做 → 与幻影逐字节相等的 installation id 与
///   代际；重放幂等。
/// - Phase B（提交后 kill-9 可见方向）：子进程完整提交 3 个安装后被强杀；
///   重开 → 代际 3、恰好 3 条回执；同 key 重放逐字节相等；fresh key 下一
///   安装恰为代际 4（= durable 代际 + 1，绝不回退到更早）；integrity ok。
#[test]
#[allow(clippy::too_many_lines)]
fn application_fault_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    // Phase A: invisible direction (modeled lost page-cache write).
    {
        let _sandbox = SandboxCwd::new("pl-install");
        let stack = TestStack::new("pl-install-artifacts", 0x33);
        let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
        let root = TestRoot::new("pl-install");
        let authority = open_application_fault(root.base());

        nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
        let phantom = installed(&authority, &stack, verified.receipt_id, 0x01, 2_000);
        assert_eq!(phantom.installation_generation, Generation::INITIAL);
        nlos_store_fault::disarm();
        // The surviving authority connection keeps a wal-index referencing
        // frames the disk never saw; it must die first (as a real power
        // loss would kill it) so recovery sees durable bytes alone.
        drop(authority);

        let recovered = reopen_application(root.base());
        assert!(
            recovered
                .inspect_application(verified.package_id)
                .expect("inspect after power loss")
                .is_none(),
            "the lost install is wholly absent: no application row survives"
        );
        assert_authority_counts(root.base(), [0, 0]);
        assert_integrity(root.base());

        // Same-key redo converges onto exactly the phantom installation.
        let redo = installed(&recovered, &stack, verified.receipt_id, 0x01, 2_000);
        assert_eq!(redo, phantom);
        assert_eq!(
            replayed(&recovered, &stack, verified.receipt_id, 0x01, 2_000),
            phantom
        );
        assert_authority_counts(root.base(), [1, 1]);
        assert_integrity(root.base());
    }

    // Phase B: visible direction (kill-9 after the commit).
    {
        let stack = TestStack::new("kill9-install-artifacts", 0x34);
        let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
        let root = TestRoot::new("kill9-install");
        let mut child = spawn_child(
            "install-three-commit",
            &root,
            &stack.root.root().join("art"),
            verified.receipt_id,
        );
        let marker = await_marker(&mut child);
        assert_eq!(decode_marker(&marker), 3, "child committed three installs");
        kill_and_reap(&mut child);

        let authority = reopen_application(root.base());
        assert_eq!(
            raw_generation(root.base()),
            3,
            "all three committed generations survive whole"
        );
        assert_authority_counts(root.base(), [1, 3]);
        for ordinal in 1..=3_u64 {
            let seed = u8::try_from(0xA0 + ordinal).expect("small ordinal");
            let replay = replayed(
                &authority,
                &stack,
                verified.receipt_id,
                seed,
                child_at_ms(ordinal),
            );
            assert_eq!(replay.installation_generation.get(), ordinal);
        }
        // The next fresh install continues at exactly generation + 1.
        let next = installed(&authority, &stack, verified.receipt_id, 0xA4, 5_000);
        assert_eq!(next.installation_generation.get(), 4);
        assert_authority_counts(root.base(), [1, 4]);
        assert_integrity(root.base());
    }
}

// ---------------------------------------------------------------------------
// C4: torn WAL tail — generation whole-or-absent, never below the survivor
// ---------------------------------------------------------------------------

/// C4：子进程提交 5 个安装后被强杀，父进程对 authority WAL 末段事务帧组的
/// 每个截断点与再深一段的截断点（合计 ≥6 个代表点）逐一恢复重开：代际恒
/// 为旧值或新值之一（绝不撕裂：integrity ok，且 **代际 == 回执数 == 已落
/// 最大代际**——推进与回执同事务、同生同灭）；恒 ≥ 再深一段的幸存提交
/// （绝不回退到更早）；每个截断点同 key 重做缺失安装 → 与控制 installation
/// id 逐字节相等；重放幂等；完整恢复对照逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn application_fault_torn_wal_tail_generation_whole_or_absent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let stack = TestStack::new("torn-install-artifacts", 0x35);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let root = TestRoot::new("torn-install");
    let mut child = spawn_child(
        "install-five-commit",
        &root,
        &stack.root.root().join("art"),
        verified.receipt_id,
    );
    let marker = await_marker(&mut child);
    assert_eq!(decode_marker(&marker), 5);
    kill_and_reap(&mut child);

    let snapshot = AuthoritySnapshot::capture(root.base());

    // Visible control: the untouched WAL recovers all five installs whole.
    {
        let authority = reopen_application(root.base());
        assert_eq!(raw_generation(root.base()), 5);
        assert_authority_counts(root.base(), [1, 5]);
        drop(authority);
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
        let authority = reopen_application(root.base());
        assert_integrity(root.base());

        // Co-life: the generation advance and its receipt are one
        // transaction, so generation == receipt count == max installed
        // generation at every cut — a torn tail can split neither of them.
        let seen = raw_generation(root.base());
        assert_eq!(
            raw_count(
                &authority_database(root.base()),
                "SELECT COUNT(*) FROM installation_receipts"
            ),
            i64::try_from(seen).expect("generation fits i64"),
            "receipts and the generation live and die together"
        );
        assert!(
            (3..=4).contains(&seen),
            "the cut leaves install 3 or 4 whole (never 5, never below 3): got {seen}"
        );

        // Same-key redo of the missing installs converges byte-equal.
        for ordinal in 1..=5_u64 {
            let install_seed = u8::try_from(0xA0 + ordinal).expect("small ordinal");
            let generation =
                Generation::new(std::num::NonZeroU64::new(ordinal).expect("dense ordinal"));
            let expected_id = derive_installation_id(
                key(install_seed),
                derive_application_id(verified.package_id),
                generation,
            );
            if ordinal > seen {
                assert_eq!(
                    installed(
                        &authority,
                        &stack,
                        verified.receipt_id,
                        install_seed,
                        child_at_ms(ordinal)
                    )
                    .installation_id,
                    expected_id,
                    "redo of install {ordinal} must be byte-equal"
                );
            }
            assert_eq!(
                replayed(
                    &authority,
                    &stack,
                    verified.receipt_id,
                    install_seed,
                    child_at_ms(ordinal)
                )
                .installation_id,
                expected_id
            );
        }
        assert_authority_counts(root.base(), [1, 5]);
        assert_eq!(raw_generation(root.base()), 5);
        assert_integrity(root.base());
        drop(authority);
    }

    // Full restore returns to the visible world.
    snapshot.restore(root.base(), None);
    let _authority = reopen_application(root.base());
    assert_eq!(raw_generation(root.base()), 5);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// C5: replay storm — no double-jump, no regress
// ---------------------------------------------------------------------------

/// C5：同 key 连放 3 次 + 重开后再放 → 每次返回与原始 durable 回执逐字节
/// 相等，代际每个 key 恒推进一次（不双跳）；fresh key 从 durable 代际稠密
/// 续推（不回退）；integrity ok。
#[test]
fn application_fault_replay_storm_no_double_jump_no_regress() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let stack = TestStack::new("storm-artifacts", 0x36);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let root = TestRoot::new("storm");
    let authority = reopen_application(root.base());

    let first = installed(&authority, &stack, verified.receipt_id, 0x01, 2_000);
    for _ in 0..3 {
        assert_eq!(
            replayed(&authority, &stack, verified.receipt_id, 0x01, 2_000),
            first
        );
    }
    assert_authority_counts(root.base(), [1, 1]);
    assert_eq!(
        raw_generation(root.base()),
        1,
        "storm advanced exactly once"
    );

    let second = installed(&authority, &stack, verified.receipt_id, 0x02, 3_000);
    for _ in 0..2 {
        assert_eq!(
            replayed(&authority, &stack, verified.receipt_id, 0x02, 3_000),
            second
        );
    }
    assert_eq!(raw_generation(root.base()), 2, "second key advanced once");

    drop(authority);
    let verified_state = reopen_application(root.base());
    assert_eq!(
        replayed(&verified_state, &stack, verified.receipt_id, 0x01, 2_000).installation_generation,
        Generation::INITIAL,
        "replays after reopen advance nothing"
    );
    assert_eq!(
        replayed(&verified_state, &stack, verified.receipt_id, 0x02, 3_000)
            .installation_generation
            .get(),
        2
    );
    assert_eq!(raw_generation(root.base()), 2);
    let third = installed(&verified_state, &stack, verified.receipt_id, 0x03, 4_000);
    assert_eq!(third.installation_generation.get(), 3);
    assert_authority_counts(root.base(), [1, 3]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// C6: trigger guards survive injection and recovery
// ---------------------------------------------------------------------------

/// C6：一次注入崩溃（断电安装 + 恢复 + 重做）之后，raw SQL 的非法写仍被
/// DDL 守卫 abort——代际不可减、身份冻结、行不可删、回执不可改不可删、
/// 回执不可越过当前代际、状态机非法转移 abort（disable 不得动代际、
/// disabled 终态、未知状态拒绝）；守卫下的权威读路径照常服务、已 durable
/// 安装未被扰动。
#[test]
#[allow(clippy::too_many_lines)]
fn application_fault_trigger_guards_survive_injection_and_recovery() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("guards");
    let stack = TestStack::new("guards-artifacts", 0x37);
    let verified = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let root = TestRoot::new("guards");
    let authority = open_application_fault(root.base());
    let first = installed(&authority, &stack, verified.receipt_id, 0x01, 2_000);

    // Crash through an install window: the phantom advance is lost, then
    // redone — the recovered database is a post-crash recovery product.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = installed(&authority, &stack, verified.receipt_id, 0x02, 3_000);
    assert_eq!(phantom.installation_generation.get(), 2);
    nlos_store_fault::disarm();
    drop(authority);
    let recovered = reopen_application(root.base());
    assert_authority_counts(root.base(), [1, 1]);
    assert_eq!(raw_generation(root.base()), 1, "phantom vanished whole");
    let redo = installed(&recovered, &stack, verified.receipt_id, 0x02, 3_000);
    assert_eq!(redo, phantom);
    assert_authority_counts(root.base(), [1, 2]);

    let raw = Connection::open(authority_database(root.base())).expect("open raw connection");
    // The generation can never move backwards (monotonic guard).
    assert!(
        raw.execute(
            "UPDATE applications SET current_installation_generation=1",
            []
        )
        .is_err()
    );
    // The identity is frozen.
    assert!(
        raw.execute(
            "UPDATE applications SET application_id=x'CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC'",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE applications SET package_id=x'CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC'",
            []
        )
        .is_err()
    );
    // The application row and the receipts are durable.
    assert!(raw.execute("DELETE FROM applications", []).is_err());
    assert!(
        raw.execute("DELETE FROM installation_receipts", [])
            .is_err()
    );
    assert!(
        raw.execute("UPDATE installation_receipts SET installed_at_ms=99", [])
            .is_err(),
        "an installation receipt can never be rewritten"
    );
    // A receipt can never record a generation beyond the current one.
    assert!(
        raw.execute(
            "INSERT INTO installation_receipts (
                installation_id, idempotency_key, application_id,
                installation_generation, package_id, package_manifest_digest,
                package_version, entry_count, package_verification_receipt_id,
                installer_principal, installed_at_ms
             ) VALUES (
                x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB',
                x'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB',
                ?1, 99, ?2, ?3, 1, 1, ?4, ?5, 3
             )",
            rusqlite::params![
                first.application_id.as_bytes().as_slice(),
                verified.package_id.as_bytes().as_slice(),
                verified.manifest_digest.as_bytes().as_slice(),
                verified.receipt_id.as_bytes().as_slice(),
                verified.signer.as_bytes().as_slice(),
            ],
        )
        .is_err(),
        "receipts are current-generation-bounded"
    );
    // Disabling must not move the generation in the same statement.
    assert!(
        raw.execute(
            "UPDATE applications SET status=2, current_installation_generation=3",
            []
        )
        .is_err(),
        "a disable is not an installation"
    );
    // The legal disable, after which the application is terminally disabled.
    raw.execute("UPDATE applications SET status=2", [])
        .expect("legal disable transition");
    assert!(
        raw.execute("UPDATE applications SET status=1", []).is_err(),
        "disabled cannot return to installed in this slice"
    );
    assert!(
        raw.execute(
            "UPDATE applications SET status=3, current_installation_generation=current_installation_generation+1",
            []
        )
        .is_err(),
        "uninstall must not move the generation"
    );

    // The guarded authority still serves reads; the durable installs are
    // untouched.
    let view = recovered
        .inspect_application(verified.package_id)
        .expect("inspect after tamper sweep")
        .expect("exists");
    assert_eq!(view.status, nlos_application::ApplicationStatus::Disabled);
    assert_eq!(view.current_installation_generation.get(), 2);
    assert_eq!(
        recovered
            .inspect_installation(first.installation_id)
            .expect("receipt survives"),
        first
    );
    assert_authority_counts(root.base(), [1, 2]);
    assert_integrity(root.base());
}
