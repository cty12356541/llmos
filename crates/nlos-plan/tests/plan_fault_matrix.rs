//! W28-A plan authority F1–F4 kill-window fault matrix: the v1
//! `apply_plan_revision` / `record_node_transition` write path under the
//! house PoC-0003-aligned fault model, following the
//! `semantic_recovery_fault_matrix.rs` harness verbatim: kill-9 child
//! processes synchronized through piped `READY` markers (never sleeps),
//! `FAULT_LOCK` process-wide serialization, typed error-chain assertions,
//! raw table-level counts, and a `PRAGMA integrity_check` re-verification
//! at the end of every scenario.
//!
//! Covered rows (public API only):
//! - F1 kill-9 mid-apply transaction: the interrupted revision insert
//!   (carrying the victim idempotency key) rolls back completely; the
//!   restarted caller applies the same request cleanly and replays exactly
//!   once;
//! - F2 kill-9 after the apply commit: the durable revision prefix
//!   survives bitwise, the pre-crash key replay is answered from the
//!   receipt (no double apply), and the caller continues to revision 2 and
//!   a converged transition;
//! - F3 hard I/O error on the apply transaction: the call fails closed
//!   with a typed storage error naming the injected condition, no half
//!   state commits, and the identical request succeeds once the fault is
//!   removed;
//! - F4 silent write loss (`PowerLossAfter`): the phantom revision that
//!   "committed" never becomes durable, the redo applies exactly once, and
//!   the authority converges to the same two-revision chain with a
//!   replayable transition voucher.
//!
//! **Crash semantics disclaimer**: the kill-9 rows use forced child
//! termination to simulate *process* crashes; the OS page cache survives a
//! process death, so a killed process is NOT a machine power loss. Writes
//! the kernel accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`]. All evidence is single-node local
//! `SQLite` under the `nlos-store-fault` VFS shim — it proves nothing
//! about cross-authority atomicity (Plan/Task/Resource remain separate
//! durable domains). The fault state in `nlos-store-fault` is
//! process-global, so every test holds `FAULT_LOCK` for its entire
//! duration; children never arm the fault machinery.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_plan::{
    ApplyPlanRevisionRequest, NodeTransitionDecision, NodeTransitionRequest, PlanNodeDeclaration,
    PlanNodeKind, PlanNodeState, PlanRevisionDecision, PlanStoreError, SqlitePlanAuthority,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{IdempotencyKey, TaskPlanId};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-plan-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    base: PathBuf,
    db_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("nlos-plan-fault-{}-{suffix}", std::process::id()));
        let db_path = base.join("plan-authority.sqlite3");
        std::fs::create_dir_all(&base).expect("create fixture directory");
        Self { base, db_path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.db_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn node(key: u8, payload: u8) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [key; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [payload; 32],
        dependency_keys: Vec::new(),
        input_selectors_digest: [payload; 32],
        output_contract_digest: [payload; 32],
        policy_digest: [payload; 32],
        resource_ceiling_digest: [payload; 32],
    }
}

fn revision_request(
    plan_id: Option<TaskPlanId>,
    nodes: Vec<PlanNodeDeclaration>,
    key: u8,
) -> ApplyPlanRevisionRequest {
    ApplyPlanRevisionRequest {
        plan_id,
        nodes,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        applied_at_ms: 1_000,
    }
}

fn apply_revision_one(authority: &SqlitePlanAuthority) -> PlanRevisionDecision {
    authority
        .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x01)], 0x11))
        .expect("apply revision 1")
}

fn open_shim(path: &Path) -> SqlitePlanAuthority {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    SqlitePlanAuthority::open_with_vfs(path, Some(VFS_NAME)).expect("open via fault vfs")
}

fn assert_integrity(path: &Path) {
    let connection = Connection::open(path).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

fn raw_count(path: &Path, table: &str) -> i64 {
    let connection = Connection::open(path).expect("open raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn error_chain(error: &PlanStoreError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn assert_sqlite_error_chain(error: &PlanStoreError, needles: &[&str]) {
    assert!(
        matches!(error, PlanStoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(error).to_lowercase();
    assert!(
        needles.iter().any(|needle| chain.contains(needle)),
        "error chain must name the injected condition, got: {chain}"
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

fn spawn_child(scenario: &str, base: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_PLAN_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_PLAN_CRASH_CHILD_BASE", base)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

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
        Ok(Ok(line)) => line,
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

fn plan_id_from_marker(marker: &str) -> TaskPlanId {
    let hex = marker.strip_prefix("READY:").expect("READY marker payload");
    assert_eq!(hex.len(), 32, "plan id hex width");
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("plan id hex digit");
    }
    TaskPlanId::from_bytes(bytes)
}

fn announce_ready(plan_id: TaskPlanId) {
    println!("READY:{}", hex_encode(plan_id.as_bytes()));
    std::io::stdout().flush().expect("flush marker");
}

// ---------------------------------------------------------------------------
// kill-9 child scenarios
// ---------------------------------------------------------------------------

/// F1 fixture: the schema exists, then a writer transaction inserts the
/// phantom revision-1 prefix the real `apply_plan_revision` would commit
/// (plan head + revision receipt carrying the victim idempotency key) and
/// dies before commit. If the interrupted prefix survived, the parent's
/// real apply would collide on the unique idempotency key or the plan
/// primary key.
fn child_mid_apply_tx(base: &Path) -> ! {
    let db_path = base.join("plan-authority.sqlite3");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    let raw = Connection::open(&db_path).expect("open raw connection");
    raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
    raw.execute(
        "INSERT INTO plans (plan_id, current_revision, created_at_ms, updated_at_ms)
         VALUES (?1, 1, 1000, 1000)",
        rusqlite::params![[0x50u8; 16].as_slice()],
    )
    .expect("mid-tx phantom plan head");
    raw.execute(
        "INSERT INTO plan_revisions (
            plan_id, revision, idempotency_key, parent_revision_digest,
            nodes_root, dependencies_root, plan_digest,
            declared_node_count, applied_at_ms
         ) VALUES (?1, 1, ?2, NULL, ?3, ?4, ?5, 1, 1000)",
        rusqlite::params![
            [0x50u8; 16].as_slice(),
            [0x11u8; 16].as_slice(),
            [0x60u8; 32].as_slice(),
            [0x61u8; 32].as_slice(),
            [0x62u8; 32].as_slice(),
        ],
    )
    .expect("mid-tx phantom revision receipt");
    announce_ready(TaskPlanId::from_bytes([0x50; 16]));
    let _keepers = (authority, raw);
    loop {
        std::thread::park();
    }
}

/// F2 fixture: exactly one `apply_plan_revision` transaction (key 0x11,
/// one node) is committed through the public API and returns before the
/// kill; the plan head sits at revision 1.
fn child_apply_commit_complete(base: &Path) -> ! {
    let db_path = base.join("plan-authority.sqlite3");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    let receipt = apply_revision_one(&authority).receipt();
    assert_eq!(receipt.revision, 1);
    announce_ready(receipt.plan_id);
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(base)) = (
        std::env::var("NLOS_PLAN_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_PLAN_CRASH_CHILD_BASE"),
    ) else {
        return;
    };
    let base = PathBuf::from(base);
    match scenario.as_str() {
        "mid-apply-tx" => child_mid_apply_tx(&base),
        "apply-commit-complete" => child_apply_commit_complete(&base),
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// F1: kill-9 mid-apply transaction
// ---------------------------------------------------------------------------

/// kill-9 中断 apply 事务：子进程在 `BEGIN IMMEDIATE` 未提交（已插入
/// 携带真实幂等键的幻影 plan 头 + revision 回执）时被强杀；重开后中断
/// 事务完全回滚——幻影行不存在、真实 apply 不碰撞且返回 `Applied`；
/// 同键重放由持久回执应答（`Replayed`、单行）；无任何双应用。
#[test]
fn fault_kill9_mid_apply_tx_rolls_back_and_real_apply_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let mut child = spawn_child("mid-apply-tx", &fixture.base);
    let _marker = await_marker(&mut child);
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&fixture.db_path, "plans"), 0);
    assert_eq!(raw_count(&fixture.db_path, "plan_revisions"), 0);
    assert_eq!(raw_count(&fixture.db_path, "plan_nodes"), 0);

    let authority = SqlitePlanAuthority::open(&fixture.db_path).expect("reopen after kill");
    let applied = apply_revision_one(&authority);
    assert!(matches!(applied, PlanRevisionDecision::Applied(_)));
    let replay = apply_revision_one(&authority);
    assert!(matches!(replay, PlanRevisionDecision::Replayed(_)));
    assert_eq!(raw_count(&fixture.db_path, "plans"), 1);
    assert_eq!(raw_count(&fixture.db_path, "plan_revisions"), 1);
    assert_eq!(raw_count(&fixture.db_path, "plan_nodes"), 1);
    assert_integrity(&fixture.db_path);
}

// ---------------------------------------------------------------------------
// F2: kill-9 after the apply commit
// ---------------------------------------------------------------------------

/// commit 后崩溃：子进程在 apply revision 1 提交返回后被强杀；重开后已
/// 提交前缀逐位保留（单行、digest 完整）、同键重放由回执应答（不双应
/// 用）、后续推进到 revision 2 且链验证通过；transition 在重启后收敛且
/// 幂等。
#[test]
fn fault_kill9_after_apply_commit_keeps_prefix_and_replays_from_receipt() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let mut child = spawn_child("apply-commit-complete", &fixture.base);
    let marker = await_marker(&mut child);
    let plan_id = plan_id_from_marker(&marker);
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&fixture.db_path, "plan_revisions"), 1);
    let authority = SqlitePlanAuthority::open(&fixture.db_path).expect("reopen after kill");
    let durable = authority
        .inspect_plan_revision(plan_id, 1)
        .expect("inspect revision 1")
        .expect("committed prefix survives the kill");
    assert_eq!(durable.plan_id, plan_id);
    assert_eq!(durable.revision, 1);
    assert_eq!(durable.parent_revision_digest, None);

    // The stale pre-crash replay of the very same request is answered from
    // the durable receipt, never double-applied.
    let replay = apply_revision_one(&authority);
    assert!(matches!(replay, PlanRevisionDecision::Replayed(_)));
    assert_eq!(raw_count(&fixture.db_path, "plan_revisions"), 1);

    // The caller continues honestly to revision 2.
    let second = authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x12,
        ))
        .expect("revision 2 after crash");
    assert!(matches!(second, PlanRevisionDecision::Applied(_)));
    assert_eq!(
        second.receipt().parent_revision_digest,
        Some(durable.plan_digest)
    );
    let verification = authority.verify_revision_chain(plan_id).expect("chain");
    assert_eq!(verification.revision_count, 2);

    // A transition on the restarted authority converges and replays.
    let node_id = authority
        .list_plan_nodes(plan_id)
        .expect("list")
        .into_iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("node a")
        .node_id;
    let transition = NodeTransitionRequest {
        plan_id,
        node_id,
        from_state: PlanNodeState::Declared,
        to_state: PlanNodeState::Eligible,
        // The node was still pre-execution when revision 2 re-declared it,
        // so it is bound to revision 2 now.
        expected_declared_revision: 2,
        idempotency_key: IdempotencyKey::from_bytes([0xe1; 16]),
        transitioned_at_ms: 2_000,
    };
    let recorded = authority
        .record_node_transition(transition)
        .expect("transition after crash");
    assert!(matches!(recorded, NodeTransitionDecision::Recorded(_)));
    let replayed = authority
        .record_node_transition(transition)
        .expect("transition replay");
    assert!(matches!(replayed, NodeTransitionDecision::Replayed(_)));
    assert_eq!(raw_count(&fixture.db_path, "plan_node_transitions"), 1);
    assert_integrity(&fixture.db_path);
}

// ---------------------------------------------------------------------------
// F3: hard I/O error on the apply transaction fails closed
// ---------------------------------------------------------------------------

/// 写入硬 I/O 错误（revision 2 的 apply 事务）：`FailWritesAfter { 0,
/// IoErr }` 下 `apply_plan_revision` 必须以 `PlanStoreError::Sqlite` 显式
/// 失败（错误链含 I/O 条件），不返回假成功；无半截状态（head 仍在
/// revision 1、无新回执、节点行不被推进到 2）；disarm 后同一请求重试成
/// 功且链完整。
#[test]
fn fault_io_error_on_apply_fails_closed_and_retry_succeeds() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let authority = open_shim(&fixture.db_path);
    let first = apply_revision_one(&authority);
    let plan_id = first.receipt().plan_id;

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x12,
        ))
        .expect_err("apply must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);

    // Fail-closed leaves no half state.
    assert_eq!(raw_count(&fixture.db_path, "plan_revisions"), 1);
    assert_eq!(
        authority
            .inspect_plan(plan_id)
            .expect("inspect")
            .expect("plan")
            .current_revision,
        1
    );
    let node_row = authority
        .list_plan_nodes(plan_id)
        .expect("list")
        .into_iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("node a");
    assert_eq!(node_row.declared_revision, 1);

    // Removing the fault makes the very same request succeed.
    nlos_store_fault::disarm();
    let retried = authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x12,
        ))
        .expect("apply after disarm");
    assert!(matches!(retried, PlanRevisionDecision::Applied(_)));
    assert_eq!(raw_count(&fixture.db_path, "plan_revisions"), 2);
    assert_eq!(
        authority
            .verify_revision_chain(plan_id)
            .expect("chain")
            .revision_count,
        2
    );
    assert_integrity(&fixture.db_path);
}

// ---------------------------------------------------------------------------
// F4: silent write loss — converge to the same durable state
// ---------------------------------------------------------------------------

/// 静默丢写（断电模型）：`PowerLossAfter { 0 }` 下 revision 2 的 apply
/// 「报告成功」但写入从未落盘；连接必须先死亡（真实断电同样会杀死它）
/// 使恢复只看落盘字节；重开后幻影 revision 不冒充已提交事实（head 仍在
/// revision 1、integrity ok）；丢失的 revision 2 可重做且恰好应用一次；
/// 二次重开验证真持久；随后收敛：revision 2 链验证通过、transition
/// voucher 重放幂等、恒单行。
#[test]
fn fault_silent_write_loss_redo_applies_once_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let authority = open_shim(&fixture.db_path);
    let first = apply_revision_one(&authority);
    let plan_id = first.receipt().plan_id;

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x12,
        ))
        .expect("apply reports success under power loss");
    assert_eq!(phantom.receipt().revision, 2);
    nlos_store_fault::disarm();
    // The surviving connection keeps a wal-index that references frames the
    // disk never saw; it must die first (as a real power loss would kill it)
    // so recovery sees durable bytes alone.
    drop(authority);

    let recovered = SqlitePlanAuthority::open(&fixture.db_path).expect("reopen after power loss");
    assert_eq!(raw_count(&fixture.db_path, "plan_revisions"), 1);
    assert_eq!(
        recovered
            .inspect_plan(plan_id)
            .expect("inspect")
            .expect("plan")
            .current_revision,
        1,
        "silently dropped revision must not fabricate durability"
    );
    assert_integrity(&fixture.db_path);

    // The lost revision is redoable and applies exactly once.
    let redone = recovered
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x12,
        ))
        .expect("redo revision 2");
    assert!(matches!(redone, PlanRevisionDecision::Applied(_)));
    drop(recovered);

    let verified = SqlitePlanAuthority::open(&fixture.db_path).expect("reopen after redo");
    assert_eq!(
        verified
            .inspect_plan_revision(plan_id, 2)
            .expect("inspect revision 2")
            .expect("redone revision is durable"),
        redone.receipt()
    );
    assert_eq!(raw_count(&fixture.db_path, "plan_revisions"), 2);
    assert_eq!(
        verified
            .verify_revision_chain(plan_id)
            .expect("chain")
            .revision_count,
        2
    );

    // Convergence: a transition voucher commits and replays idempotently.
    let node_id = verified
        .list_plan_nodes(plan_id)
        .expect("list")
        .into_iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("node a")
        .node_id;
    let transition = NodeTransitionRequest {
        plan_id,
        node_id,
        from_state: PlanNodeState::Declared,
        to_state: PlanNodeState::Eligible,
        expected_declared_revision: 2,
        idempotency_key: IdempotencyKey::from_bytes([0xf1; 16]),
        transitioned_at_ms: 2_000,
    };
    let recorded = verified
        .record_node_transition(transition)
        .expect("voucher after power loss");
    assert!(matches!(recorded, NodeTransitionDecision::Recorded(_)));
    let replayed = verified
        .record_node_transition(transition)
        .expect("voucher replay");
    assert!(matches!(replayed, NodeTransitionDecision::Replayed(_)));
    assert_eq!(raw_count(&fixture.db_path, "plan_node_transitions"), 1);
    assert_integrity(&fixture.db_path);
}
