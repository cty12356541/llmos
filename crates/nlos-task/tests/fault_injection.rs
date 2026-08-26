#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! B-TASK-001 fault-injection acceptance tests: durability invariants of
//! `SqliteTaskAuthority` under the `nlos-store-fault` VFS, mirroring the
//! PoC-0003 F1-F4 fault matrix established for `nlos-store`.
//!
//! **Crash semantics disclaimer**: the kill-9 rows use forced child
//! termination (`SIGKILL` on Unix, `TerminateProcess` on Windows via
//! `Child::kill`) to simulate *process* crashes. The OS page cache survives
//! a process death, so a killed process is NOT a machine power loss; writes
//! the kernel accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation.
//! Child processes are synchronized through piped stdout markers (`READY`),
//! never through sleeps. The fault state in `nlos-store-fault` is
//! process-global, so every test holds `FAULT_LOCK` for its entire duration
//! (each integration binary is its own process).

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_store_fault::{FaultCode, FaultMode};
use nlos_task::{
    AttemptSpec, AttemptState, CancelRequest, FinalizeDecision, FinalizeRequest, PermitDecision,
    PermitRequest, PermitState, SnapshotBundle, SqliteTaskAuthority, TaskSpec, TaskState,
    TaskStoreError, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

const VFS_NAME: &str = "nlos-task-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-fault-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn sibling(path: &Path, suffix: &str) -> PathBuf {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            Self::sibling(&self.path, "-wal"),
            Self::sibling(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

/// Full `Display` chain of a `TaskStoreError`, top cause last, for content
/// assertions (e.g. that `SQLITE_FULL`'s message reaches the caller).
fn error_chain(error: &TaskStoreError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn open_shim(path: &Path) -> SqliteTaskAuthority {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    SqliteTaskAuthority::open_with_vfs(path, Some(VFS_NAME)).expect("open via fault vfs")
}

/// Runs `PRAGMA integrity_check` on the test's own rusqlite connection, as
/// parent-side verification independent of the authority under test.
fn assert_integrity(path: &Path) {
    let connection = rusqlite::Connection::open(path).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

/// Row count of one authority table, read through an independent raw
/// connection (WAL readers do not disturb the writer under test).
fn raw_count(path: &Path, table: &str) -> i64 {
    let connection = rusqlite::Connection::open(path).expect("open raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn task_id(seed: u8) -> TaskId {
    TaskId::from_bytes(bytes(seed))
}

fn task_spec(seed: u8) -> TaskSpec {
    TaskSpec {
        task_id: task_id(seed),
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
    }
}

fn snapshot(head_seq: u64, fence: u64) -> SnapshotBundle {
    let tag = u8::try_from(head_seq).expect("test head fits in u8");
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(bytes(0x10 + tag)),
        snapshot_digest: [0x20 + tag; 32],
        expected_head_commit_seq: head_seq,
        effect_history_root: if head_seq == 0 {
            empty_effect_history_root()
        } else {
            [0x30 + tag; 32]
        },
        retry_fence_epoch: fence,
    }
}

fn attempt_spec(task_seed: u8, seed: u8, bundle: SnapshotBundle) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(task_seed),
        attempt_id: TaskAttemptId::from_bytes(bytes(seed)),
        attempt_generation: Generation::INITIAL,
        snapshot: bundle,
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0xc0 + seed)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xa0 + seed)),
        registered_at_ms: 2_000,
    }
}

fn permit_request(spec: &AttemptSpec, seed: u8) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [seed; 32],
        // This matrix covers the B-TASK-001 tables only; an empty planned
        // effect set keeps the pre-effect-slice permit behavior.
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xb0 + seed)),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

fn cancel_request(task_seed: u8, seed: u8) -> CancelRequest {
    CancelRequest {
        task_id: task_id(task_seed),
        idempotency_key: IdempotencyKey::from_bytes(bytes(seed)),
        requested_at_ms: 4_000,
    }
}

fn finalize_request(
    spec: &AttemptSpec,
    permit_id: CommitPermitId,
    root: u8,
    fence: u64,
) -> FinalizeRequest {
    FinalizeRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id,
        new_effect_history_root: [root; 32],
        new_retry_fence_epoch: fence,
        finalized_at_ms: 5_000,
    }
}

fn issued_permit(decision: PermitDecision) -> nlos_task::PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected Issued, got {other:?}"),
    }
}

/// Registers a task plus one attempt bound to the initial head: the shared
/// committed prefix every injection scenario starts from.
fn seed_task_with_attempt(
    authority: &SqliteTaskAuthority,
    task_seed: u8,
    attempt_seed: u8,
) -> AttemptSpec {
    authority
        .register_task(task_spec(task_seed))
        .expect("register task");
    let spec = attempt_spec(task_seed, attempt_seed, snapshot(0, 0));
    authority.register_attempt(spec).expect("register attempt");
    spec
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (fault_crash.rs 范式: current_exe + env var)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, path: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_TASK_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_TASK_CRASH_CHILD_DATABASE", path)
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

fn hex_encode(value: &[u8]) -> String {
    use std::fmt::Write as _;
    value
        .iter()
        .fold(String::with_capacity(value.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn hex_decode_permit(text: &str) -> CommitPermitId {
    assert_eq!(text.len(), 32, "permit id hex is 16 bytes");
    let mut decoded = [0_u8; 16];
    for (index, slot) in decoded.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex byte");
    }
    CommitPermitId::from_bytes(decoded)
}

fn announce(marker: &str) {
    println!("{marker}");
    std::io::stdout().flush().expect("flush marker");
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(path)) = (
        std::env::var("NLOS_TASK_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_TASK_CRASH_CHILD_DATABASE"),
    ) else {
        return;
    };
    let path = PathBuf::from(path);
    match scenario.as_str() {
        "mid-tx" => {
            let authority = SqliteTaskAuthority::open(&path).expect("open");
            seed_task_with_attempt(&authority, 0x01, 0x0a);
            // Simulate the middle of a decision transaction: a writer
            // transaction is open and has dirtied the durable rows but has
            // not committed when the process dies.
            let raw = rusqlite::Connection::open(&path).expect("raw connection");
            raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
            raw.execute("UPDATE tasks SET revision = revision + 100", [])
                .expect("mid-tx task write");
            raw.execute(
                "UPDATE task_attempts SET attempt_state = attempt_state + 100",
                [],
            )
            .expect("mid-tx attempt write");
            announce("READY");
            let _keepers = (authority, raw);
            loop {
                std::thread::park();
            }
        }
        "after-commit" => {
            let authority = SqliteTaskAuthority::open(&path).expect("open");
            // Task A: full register -> permit -> finalize lifecycle.
            let spec_a = seed_task_with_attempt(&authority, 0x01, 0x0a);
            let permit = issued_permit(
                authority
                    .request_commit_permit(permit_request(&spec_a, 0x01))
                    .expect("permit A"),
            );
            authority
                .finalize_commit(finalize_request(&spec_a, permit.permit_id, 0x31, 0))
                .expect("finalize A");
            // Task B: register -> attempt -> committed cancellation.
            seed_task_with_attempt(&authority, 0x02, 0x0b);
            authority
                .cancel_task(cancel_request(0x02, 0xd1))
                .expect("cancel B");
            announce(&format!(
                "READY {}",
                hex_encode(permit.permit_id.as_bytes())
            ));
            let _keeper = authority;
            loop {
                std::thread::park();
            }
        }
        "wal-setup" => {
            // Register + attempt + issued permit, all committed; the kill
            // leaves the WAL and SHM behind for the parent's file-level
            // tail truncation.
            let authority = SqliteTaskAuthority::open(&path).expect("open");
            let spec_a = seed_task_with_attempt(&authority, 0x01, 0x0a);
            let permit = issued_permit(
                authority
                    .request_commit_permit(permit_request(&spec_a, 0x01))
                    .expect("permit A"),
            );
            announce(&format!(
                "READY {}",
                hex_encode(permit.permit_id.as_bytes())
            ));
            let _keeper = authority;
            loop {
                std::thread::park();
            }
        }
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// Row 1: kill-9 equivalence — an interrupted transaction leaves no trace
// ---------------------------------------------------------------------------

/// kill-9 等价：子进程在 `BEGIN IMMEDIATE` 未提交时被强杀；重开后中断事务
/// 的写入完全回滚，只有中断前的已提交前缀（task + attempt）保留，无
/// Permit/Receipt/Cancel 痕迹，数据库完整且 authority 可正常重开。
#[test]
fn fault_kill9_mid_transaction_leaves_no_traces() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-mid-tx");
    let mut child = spawn_child("mid-tx", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    // Nothing uncommitted may survive: the mid-transaction revision and
    // attempt-state dirt are rolled back; only register+attempt are durable.
    let raw = rusqlite::Connection::open(&database.path).expect("raw reopen");
    let revision: i64 = raw
        .query_row("SELECT revision FROM tasks", [], |row| row.get(0))
        .expect("query task revision");
    assert_eq!(revision, 0, "mid-transaction write must be rolled back");
    drop(raw);
    assert_eq!(raw_count(&database.path, "tasks"), 1);
    assert_eq!(raw_count(&database.path, "task_attempts"), 1);
    assert_eq!(raw_count(&database.path, "commit_permits"), 0);
    assert_eq!(raw_count(&database.path, "task_receipts"), 0);
    assert_eq!(raw_count(&database.path, "task_cancels"), 0);

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let head = authority.inspect_task(task_id(0x01)).expect("head");
    assert_eq!(head.state, TaskState::Active);
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(head.permit_epoch, 0);
    assert_eq!(head.active_permit, None);
    assert_eq!(
        authority
            .inspect_attempt(task_id(0x01), TaskAttemptId::from_bytes(bytes(0x0a)))
            .expect("attempt")
            .state,
        AttemptState::Created,
        "mid-transaction attempt dirt must not be durable"
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// Row 2: kill-9 after commit — committed decisions survive completely
// ---------------------------------------------------------------------------

/// commit 后崩溃等价：子进程在 task 注册、permit 签发、finalize、cancel
/// 全部提交返回后被强杀；重开后全部已提交事实完整保留（head 推进、
/// permit 关闭、commit/closure receipt、cancel epoch），重放返回原结果。
#[test]
fn fault_kill9_after_commit_preserves_all_decisions() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-after-commit");
    let mut child = spawn_child("after-commit", &database.path);
    let marker = await_marker(&mut child);
    let permit_id = hex_decode_permit(
        marker
            .trim()
            .strip_prefix("READY ")
            .expect("marker carries the permit id"),
    );
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&database.path, "tasks"), 2);
    assert_eq!(raw_count(&database.path, "task_attempts"), 2);
    assert_eq!(raw_count(&database.path, "commit_permits"), 1);
    assert_eq!(raw_count(&database.path, "task_receipts"), 2);
    assert_eq!(raw_count(&database.path, "task_cancels"), 1);

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");

    // Task A: committed finalize intact, receipt immutable.
    let head_a = authority.inspect_task(task_id(0x01)).expect("head A");
    assert_eq!(head_a.head_commit_seq, 1);
    assert_eq!(head_a.head_effect_history_root, [0x31; 32]);
    assert_eq!(head_a.permit_epoch, 1);
    assert_eq!(head_a.active_permit, None);
    assert_eq!(
        authority
            .inspect_permit(task_id(0x01), permit_id)
            .expect("permit A")
            .state,
        PermitState::Closed
    );
    let attempt_a = authority
        .inspect_attempt(task_id(0x01), TaskAttemptId::from_bytes(bytes(0x0a)))
        .expect("attempt A");
    assert_eq!(attempt_a.state, AttemptState::Committed);
    let receipt_a = authority
        .inspect_receipt(
            task_id(0x01),
            attempt_a.receipt_id.expect("commit receipt id"),
        )
        .expect("commit receipt");
    assert_eq!(receipt_a.outcome, nlos_task::ReceiptOutcome::Committed);
    assert_eq!(receipt_a.new_head_commit_seq, 1);

    // Task B: committed cancellation intact, closure receipt durable.
    let head_b = authority.inspect_task(task_id(0x02)).expect("head B");
    assert_eq!(head_b.state, TaskState::Cancelled);
    assert_eq!(head_b.cancel_epoch, 1);
    assert_eq!(
        head_b.head_commit_seq, 0,
        "cancel must not advance the head"
    );
    let attempt_b = authority
        .inspect_attempt(task_id(0x02), TaskAttemptId::from_bytes(bytes(0x0b)))
        .expect("attempt B");
    assert_eq!(attempt_b.state, AttemptState::Cancelled);
    let receipt_b = authority
        .inspect_receipt(
            task_id(0x02),
            attempt_b.receipt_id.expect("closure receipt id"),
        )
        .expect("closure receipt");
    assert_eq!(
        receipt_b.outcome,
        nlos_task::ReceiptOutcome::CancelledBeforeEffect
    );

    // Replays after the crash return the original durable decisions.
    let spec_a = attempt_spec(0x01, 0x0a, snapshot(0, 0));
    match authority
        .request_commit_permit(permit_request(&spec_a, 0x01))
        .expect("replay permit A")
    {
        PermitDecision::Replayed(original) => {
            assert_eq!(original.permit_id, permit_id);
            assert_eq!(original.state, PermitState::Closed);
        }
        other => panic!("expected Replayed, got {other:?}"),
    }
    match authority
        .finalize_commit(finalize_request(&spec_a, permit_id, 0x31, 0))
        .expect("replay finalize A")
    {
        FinalizeDecision::Replayed(original) => assert_eq!(*original, receipt_a),
        other @ FinalizeDecision::Committed(_) => panic!("expected Replayed, got {other:?}"),
    }
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// Row 3: hard I/O error propagates as a typed error, never a fake success
// ---------------------------------------------------------------------------

/// 写入硬 I/O 错误：`FailWritesAfter { 0, IoErr }` 下 permit CAS 必须以
/// `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件），不返回假成功；
/// 已提交前缀不变，不产生半截 permit；disarm 后重试成功。
#[test]
fn fault_io_error_propagates_typed_and_never_fakes_success() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("ioerr-permit");
    let authority = open_shim(&database.path);
    let spec_a = seed_task_with_attempt(&authority, 0x01, 0x0a);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .request_commit_permit(permit_request(&spec_a, 0x01))
        .expect_err("permit CAS must fail under injected I/O error");
    assert!(
        matches!(error, TaskStoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(&error).to_lowercase();
    assert!(
        chain.contains("i/o") || chain.contains("ioerr"),
        "error chain must name the I/O condition, got: {chain}"
    );
    assert!(nlos_store_fault::writes_observed() > 0);

    // No half-decision: neither the permit row nor any state transition may
    // become visible; the committed prefix is untouched.
    let head = authority.inspect_task(task_id(0x01)).expect("head");
    assert_eq!(head.permit_epoch, 0);
    assert_eq!(head.active_permit, None);
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(
        authority
            .inspect_attempt(task_id(0x01), spec_a.attempt_id)
            .expect("attempt")
            .state,
        AttemptState::Created
    );
    assert_eq!(raw_count(&database.path, "commit_permits"), 0);
    assert_eq!(raw_count(&database.path, "task_receipts"), 0);

    nlos_store_fault::disarm();
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("permit CAS succeeds after disarm"),
    );
    assert_eq!(permit.permit_epoch, 1);
    assert_eq!(
        authority
            .inspect_task(task_id(0x01))
            .expect("head")
            .active_permit,
        Some(permit.permit_id)
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// Row 4: disk-full (ENOSPC) fails closed without partial state
// ---------------------------------------------------------------------------

/// disk-full：`FailWritesAfter { 0, Full }` 下 finalize（同事务写 receipt +
/// 关 permit + attempt 终态 + head 推进）必须以 `SQLITE_FULL` 显式失败；
/// authority 不产生半截状态（receipt 不可见、permit 仍 `ISSUED`、attempt
/// 仍 `COMMIT_PERMITTED`、head 不变）；disarm 后同一 finalize 成功。
#[test]
fn fault_disk_full_enospc_fails_closed_without_partial_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("full-finalize");
    let authority = open_shim(&database.path);
    let spec_a = seed_task_with_attempt(&authority, 0x01, 0x0a);
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("permit A"),
    );

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .finalize_commit(finalize_request(&spec_a, permit.permit_id, 0x31, 0))
        .expect_err("finalize must fail under injected disk-full");
    assert!(
        matches!(error, TaskStoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(&error).to_lowercase();
    assert!(
        chain.contains("full"),
        "error chain must name the disk-full condition, got: {chain}"
    );
    assert!(nlos_store_fault::writes_observed() > 0);

    // No half-commit: the head, the permit, the attempt, and the receipt
    // table all remain exactly at the committed pre-fault prefix.
    let head = authority.inspect_task(task_id(0x01)).expect("head");
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(head.active_permit, Some(permit.permit_id));
    assert_eq!(
        authority
            .inspect_permit(task_id(0x01), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued,
        "failed finalize must not close the permit"
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(0x01), spec_a.attempt_id)
            .expect("attempt")
            .state,
        AttemptState::CommitPermitted,
        "failed finalize must not transition the attempt"
    );
    assert_eq!(
        raw_count(&database.path, "task_receipts"),
        0,
        "failed finalize must not leak a receipt"
    );

    nlos_store_fault::disarm();
    let receipt = match authority
        .finalize_commit(finalize_request(&spec_a, permit.permit_id, 0x31, 0))
        .expect("finalize succeeds after disarm")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(receipt.new_head_commit_seq, 1);
    assert_eq!(
        authority
            .inspect_task(task_id(0x01))
            .expect("head")
            .head_commit_seq,
        1
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// Row 5: silent write loss / torn tail never fabricates committed facts
// ---------------------------------------------------------------------------

/// 静默丢写/短写：
/// - Phase A（断电模型）：`PowerLossAfter { 0 }` 下 permit CAS “报告成功”
///   但写入从未落盘；重开后幻影 permit 不得冒充已提交事实
///   （`permit_epoch` 回到前缀、`PermitNotFound`、attempt 回到 `CREATED`），
///   且同一请求可重做。
/// - Phase B（短写/撕裂尾部）：子进程提交 register+attempt+permit 后被杀，
///   父进程把 WAL 截断到最后一个 commit 帧的一半；重开后不完整尾部整体
///   隐藏，此前合法提交（task+attempt）保留，幻影 permit 不可解析，同一
///   请求重放重新签发且不产生冲突。
#[test]
fn fault_silent_write_loss_and_torn_tail_hide_uncommitted_facts() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    power_loss_drops_permit_commit_and_redo_is_durable();
    torn_wal_tail_hides_permit_commit_and_reissue_succeeds();
}

/// Phase A: a silently dropped permit commit is invisible after recovery
/// and the lost decision is redoable with the same deterministic id.
fn power_loss_drops_permit_commit_and_redo_is_durable() {
    let database = TestDatabase::new("power-loss-permit");
    let authority = open_shim(&database.path);
    let spec_a = seed_task_with_attempt(&authority, 0x01, 0x0a);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();

    // The surviving connection keeps a wal-index that references frames the
    // disk never saw; it must die first (as a real power loss would kill
    // it) so recovery sees durable bytes alone (fault_crash.rs precedent).
    drop(authority);

    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after power loss");
    let head = recovered.inspect_task(task_id(0x01)).expect("head");
    assert_eq!(
        head.permit_epoch, 0,
        "silently dropped permit must not advance the epoch"
    );
    assert_eq!(head.active_permit, None);
    assert!(
        matches!(
            recovered.inspect_permit(task_id(0x01), phantom.permit_id),
            Err(TaskStoreError::PermitNotFound)
        ),
        "phantom permit must not fabricate a committed fact"
    );
    assert_eq!(
        recovered
            .inspect_attempt(task_id(0x01), spec_a.attempt_id)
            .expect("attempt")
            .state,
        AttemptState::Created
    );
    assert_eq!(raw_count(&database.path, "commit_permits"), 0);
    assert_integrity(&database.path);

    // The lost decision is redoable: the deterministic permit id is reused
    // and this time the permit is genuinely durable across a reopen.
    let redone = issued_permit(
        recovered
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("redo after power loss"),
    );
    assert_eq!(redone.permit_id, phantom.permit_id);
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    assert_eq!(
        verified
            .inspect_permit(task_id(0x01), redone.permit_id)
            .expect("redone permit")
            .state,
        PermitState::Issued
    );
    assert_eq!(
        verified
            .inspect_task(task_id(0x01))
            .expect("head")
            .permit_epoch,
        1
    );
    drop(verified);
    drop(database);
}

fn torn_wal_tail_hides_permit_commit_and_reissue_succeeds() {
    let database = TestDatabase::new("torn-tail-permit");
    let mut child = spawn_child("wal-setup", &database.path);
    let marker = await_marker(&mut child);
    let phantom_id = hex_decode_permit(
        marker
            .trim()
            .strip_prefix("READY ")
            .expect("marker carries the permit id"),
    );
    kill_and_reap(&mut child);

    let wal_path = TestDatabase::sibling(&database.path, "-wal");
    let mut wal = fs::read(&wal_path).expect("read wal");
    assert!(wal.len() > 32, "killed child must leave a populated WAL");
    let page_size = match u32::from_be_bytes(wal[8..12].try_into().expect("page size field")) {
        1 => 65_536,
        value => value as usize,
    };
    assert!(page_size >= 512, "valid SQLite page size");
    let frame_size = 24 + page_size;
    let frame_count = (wal.len() - 32) / frame_size;
    let commits: Vec<usize> = (0..frame_count)
        .filter(|index| {
            let start = 32 + index * frame_size;
            u32::from_be_bytes(wal[start + 8..start + 12].try_into().expect("commit field")) != 0
        })
        .collect();
    assert!(
        commits.len() >= 3,
        "fixture must contain schema + register + attempt + permit commits"
    );
    let last_commit = *commits.last().expect("commits exist");
    let half_frame_cut = 32 + last_commit * frame_size + frame_size / 2;
    wal.truncate(half_frame_cut);
    fs::write(&wal_path, &wal).expect("write truncated wal");
    fs::remove_file(TestDatabase::sibling(&database.path, "-shm")).expect("remove stale shm");

    // Recovery must drop the torn transaction entirely: the permit issuance
    // is invisible, but the committed task+attempt prefix is intact.
    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after truncation");
    let head = recovered.inspect_task(task_id(0x01)).expect("head");
    assert_eq!(head.permit_epoch, 0, "torn permit commit must be hidden");
    assert_eq!(head.active_permit, None);
    assert!(
        matches!(
            recovered.inspect_permit(task_id(0x01), phantom_id),
            Err(TaskStoreError::PermitNotFound)
        ),
        "torn tail must not fabricate a permit"
    );
    assert_eq!(
        recovered
            .inspect_attempt(task_id(0x01), TaskAttemptId::from_bytes(bytes(0x0a)))
            .expect("attempt")
            .state,
        AttemptState::Created,
        "the intact committed prefix survives the torn tail"
    );
    assert_integrity(&database.path);

    // Re-issuing the same request succeeds after recovery; the derived id
    // is stable, proving no conflicting half-record was left behind.
    let spec_a = attempt_spec(0x01, 0x0a, snapshot(0, 0));
    let redone = issued_permit(
        recovered
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("re-issue after torn tail"),
    );
    assert_eq!(redone.permit_id, phantom_id);
}

// ---------------------------------------------------------------------------
// Row 6: after the fault clears, the authority continues from the prefix
// ---------------------------------------------------------------------------

/// 故障解除后：同一 authority（不重建）在 disarm 后继续正常读写；已提交
/// 前缀与故障前完全一致，后续 finalize/新竞争/再 finalize 全部成功，
/// 重开后完整状态可恢复。
#[test]
fn fault_after_disarm_authority_continues_from_committed_prefix() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("disarm-continue");
    let authority = open_shim(&database.path);
    let spec_a = seed_task_with_attempt(&authority, 0x01, 0x0a);
    let permit_a = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_a, 0x01))
            .expect("permit A"),
    );

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    authority
        .finalize_commit(finalize_request(&spec_a, permit_a.permit_id, 0x31, 0))
        .expect_err("finalize must fail while the fault is armed");

    // The committed prefix observed through the same authority is identical
    // to the pre-fault state.
    let head = authority
        .inspect_task(task_id(0x01))
        .expect("head during fault");
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(head.permit_epoch, 1);
    assert_eq!(head.active_permit, Some(permit_a.permit_id));

    nlos_store_fault::disarm();

    // Reads and writes continue on the same authority instance: finalize A,
    // then a full second competition on the advanced head.
    authority
        .finalize_commit(finalize_request(&spec_a, permit_a.permit_id, 0x31, 0))
        .expect("finalize A after disarm");
    let spec_b = attempt_spec(0x01, 0x0b, snapshot(1, 0));
    authority.register_attempt(spec_b).expect("register B");
    let permit_b = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_b, 0x02))
            .expect("permit B"),
    );
    assert_eq!(permit_b.permit_epoch, 2);
    assert_eq!(permit_b.expected_head_commit_seq, 1);
    authority
        .finalize_commit(finalize_request(&spec_b, permit_b.permit_id, 0x32, 0))
        .expect("finalize B");

    let head = authority
        .inspect_task(task_id(0x01))
        .expect("head after recovery");
    assert_eq!(head.head_commit_seq, 2);
    assert_eq!(head.head_effect_history_root, [0x32; 32]);
    assert_eq!(head.permit_epoch, 2);
    assert_eq!(head.active_permit, None);
    assert_eq!(raw_count(&database.path, "task_receipts"), 2);

    // A full reopen confirms the post-recovery writes are durable.
    drop(authority);
    let reopened = SqliteTaskAuthority::open(&database.path).expect("reopen after recovery");
    let head = reopened
        .inspect_task(task_id(0x01))
        .expect("head after reopen");
    assert_eq!(head.head_commit_seq, 2);
    assert_eq!(head.permit_epoch, 2);
    assert_eq!(
        reopened
            .inspect_attempt(task_id(0x01), spec_a.attempt_id)
            .expect("attempt A")
            .state,
        AttemptState::Committed
    );
    assert_eq!(
        reopened
            .inspect_attempt(task_id(0x01), spec_b.attempt_id)
            .expect("attempt B")
            .state,
        AttemptState::Committed
    );
    assert_integrity(&database.path);
}
