//! W31-A materialization-gate crash-window fault matrix: the
//! `request_materialization` / `resolve_materialization` write path
//! under the house PoC-0003-aligned fault model, following
//! `plan_fault_matrix.rs` verbatim: kill-9 child processes synchronized
//! through piped `READY` markers (never sleeps), `FAULT_LOCK`
//! process-wide serialization, typed error-chain assertions, raw
//! table-level counts, and a `PRAGMA integrity_check` re-verification at
//! the end of every scenario.
//!
//! Covered rows (public API only):
//! - F1 kill-9 mid-request transaction: the interrupted pending-request
//!   insert (carrying the victim idempotency key) rolls back completely
//!   together with its driving vouchers; the restarted caller's real
//!   request converges and replays exactly once;
//! - F2 kill-9 between request and approval: the committed `PENDING`
//!   prefix survives bitwise (request row + `WAITING_RESOURCE` node),
//!   the pre-crash request replay is answered from the durable row, and
//!   the post-restart approval converges to `MATERIALIZING` with an
//!   idempotent resolution replay;
//! - F3 hard I/O error on the approval transaction: the resolution fails
//!   closed with a typed storage error naming the injected condition,
//!   no half state commits (request still `PENDING`, node still
//!   `WAITING_RESOURCE`, no voucher), and the identical resolution
//!   succeeds once the fault is removed;
//! - F4 silent write loss (`PowerLossAfter`) on the approval: the
//!   phantom "approved" resolution never becomes durable — there is no
//!   approved-without-flip window because the verdict and the
//!   `WAITING_* → MATERIALIZING` flip share one transaction; recovery
//!   sees the pending prefix alone, the redo resolves exactly once, and
//!   a second reopen verifies true durability.
//!
//! **Crash semantics disclaimer**: the kill-9 rows use forced child
//! termination to simulate *process* crashes; the OS page cache survives
//! a process death. Writes the kernel accepted but the disk never saw
//! are covered by [`FaultMode::PowerLossAfter`]. All evidence is
//! single-node local `SQLite` under the `nlos-store-fault` VFS shim —
//! it proves nothing about cross-authority atomicity (Plan/Task remain
//! separate durable domains; the Task-side consult is read-only). The
//! fault state in `nlos-store-fault` is process-global, so every test
//! holds `FAULT_LOCK` for its entire duration; children never arm the
//! fault machinery.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_plan::{
    ApplyPlanRevisionRequest, MaterializationAdmission, MaterializationAdmissionVerdict,
    MaterializationRequest, MaterializationRequestDecision, MaterializationResolution,
    MaterializationResolutionDecision, PlanNodeDeclaration, PlanNodeKind, PlanNodeState,
    PlanStoreError, SqlitePlanAuthority,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{IdempotencyKey, TaskPlanId};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-plan-matfault";

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
        let base = std::env::temp_dir().join(format!(
            "nlos-plan-matfault-{}-{suffix}",
            std::process::id()
        ));
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
        conditions: None,
    }
}

fn revision_request(key: u8) -> ApplyPlanRevisionRequest {
    ApplyPlanRevisionRequest {
        plan_id: None,
        nodes: vec![node(0x0a, 0x01)],
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        applied_at_ms: 1_000,
    }
}

fn apply_revision_one(authority: &SqlitePlanAuthority) -> TaskPlanId {
    authority
        .apply_plan_revision_ungated(revision_request(0x11))
        .expect("apply revision 1")
        .receipt()
        .plan_id
}

fn first_node_id(authority: &SqlitePlanAuthority, plan_id: TaskPlanId) -> nlos_types::TaskNodeId {
    authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("declared node")
        .node_id
}

fn gate_request(
    plan_id: TaskPlanId,
    node_id: nlos_types::TaskNodeId,
    key: u8,
) -> MaterializationRequest {
    MaterializationRequest {
        plan_id,
        node_id,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: 2_000,
    }
}

fn gate_approval(key: u8) -> MaterializationResolution {
    MaterializationResolution {
        request_key: IdempotencyKey::from_bytes([key; 16]),
        verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
            profile_id: "task-10k".to_string(),
            projected_task_nodes: 1,
            projected_active_working_set: 1,
        }),
        resolved_at_ms: 2_500,
    }
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

fn spawn_child(scenario: &str, base: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_PLAN_MATFAULT_CHILD_SCENARIO", scenario)
        .env("NLOS_PLAN_MATFAULT_CHILD_BASE", base)
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
    use std::fmt::Write as _;

    let mut hex = String::with_capacity(32);
    for byte in plan_id.as_bytes() {
        let _ = write!(hex, "{byte:02x}");
    }
    println!("READY:{hex}");
    std::io::stdout().flush().expect("flush marker");
}

// ---------------------------------------------------------------------------
// kill-9 child scenarios
// ---------------------------------------------------------------------------

/// F1 fixture: revision 1 is committed, then a raw writer transaction
/// inserts the phantom pending-request prefix (carrying the victim
/// idempotency key and the driving `WAITING_RESOURCE` node state the
/// real request would commit) and dies before commit. If the
/// interrupted prefix survived, the parent's real request would collide
/// on the unique idempotency key or observe an already-waiting node.
fn child_mid_request_tx(base: &Path) -> ! {
    let db_path = base.join("plan-authority.sqlite3");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    let plan_id = apply_revision_one(&authority);
    let node_id = first_node_id(&authority, plan_id);
    let raw = Connection::open(&db_path).expect("open raw connection");
    raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
    raw.execute(
        "INSERT INTO plan_materialization_requests (
            request_id, idempotency_key, plan_id, task_node_id,
            observed_declared_revision, status, requested_at_ms
         ) VALUES (?1, ?2, ?3, ?4, 1, 1, 2000)",
        rusqlite::params![
            [0x70u8; 16].as_slice(),
            [0x21u8; 16].as_slice(),
            plan_id.as_bytes().as_slice(),
            node_id.as_bytes().as_slice(),
        ],
    )
    .expect("mid-tx phantom pending request");
    announce_ready(plan_id);
    let _keepers = (authority, raw);
    loop {
        std::thread::park();
    }
}

/// F2 fixture: revision 1 and one full `request_materialization`
/// transaction (key 0x21) are committed through the public API and
/// return before the kill; the node sits `WAITING_RESOURCE` with a
/// durable `PENDING` request.
fn child_request_commit_complete(base: &Path) -> ! {
    let db_path = base.join("plan-authority.sqlite3");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    let plan_id = apply_revision_one(&authority);
    let node_id = first_node_id(&authority, plan_id);
    authority
        .request_materialization(gate_request(plan_id, node_id, 0x21))
        .expect("commit the gate request");
    announce_ready(plan_id);
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(base)) = (
        std::env::var("NLOS_PLAN_MATFAULT_CHILD_SCENARIO"),
        std::env::var("NLOS_PLAN_MATFAULT_CHILD_BASE"),
    ) else {
        return;
    };
    let base = PathBuf::from(base);
    match scenario.as_str() {
        "mid-request-tx" => child_mid_request_tx(&base),
        "request-commit-complete" => child_request_commit_complete(&base),
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// F1: kill-9 mid-request transaction
// ---------------------------------------------------------------------------

/// kill-9 中断 request 事务：子进程在 `BEGIN IMMEDIATE` 未提交（已插入
/// 携带真实幂等键的幻影 pending 请求行）时被强杀；重开后中断事务完全
/// 回滚——幻影行不存在、节点仍在 `DECLARED`、真实 request 不碰撞且返回
/// `Requested`（驱动出 `ELIGIBLE/WAITING_RESOURCE` 双凭证）；同键重放由
/// 持久行应答（`Replayed`、恒单行）。
#[test]
fn fault_kill9_mid_request_tx_rolls_back_and_real_request_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let mut child = spawn_child("mid-request-tx", &fixture.base);
    let marker = await_marker(&mut child);
    let plan_id = plan_id_from_marker(&marker);
    kill_and_reap(&mut child);

    assert_eq!(
        raw_count(&fixture.db_path, "plan_materialization_requests"),
        0
    );
    let authority = SqlitePlanAuthority::open(&fixture.db_path).expect("reopen after kill");
    let node_id = first_node_id(&authority, plan_id);
    let request = gate_request(plan_id, node_id, 0x21);
    let applied = authority
        .request_materialization(request)
        .expect("real request");
    assert!(matches!(
        applied,
        MaterializationRequestDecision::Requested(_)
    ));
    let replay = authority
        .request_materialization(request)
        .expect("replay request");
    assert!(matches!(
        replay,
        MaterializationRequestDecision::Replayed(_)
    ));
    assert_eq!(
        raw_count(&fixture.db_path, "plan_materialization_requests"),
        1
    );
    let node = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(node.state, PlanNodeState::WaitingResource);
    assert_eq!(node.transition_count, 2);
    assert_integrity(&fixture.db_path);
}

// ---------------------------------------------------------------------------
// F2: kill-9 between request and approval
// ---------------------------------------------------------------------------

/// request 提交后、approval 前崩溃：子进程在 request 事务提交返回后被强
/// 杀；重开后已提交前缀逐位保留（`PENDING` 单行 + `WAITING_RESOURCE` 节
/// 点 + 双驱动凭证）、同键 request 重放由持久行应答；随后 approval 收敛
/// 到 `MATERIALIZING`（请求行翻 `APPROVED` + 第三张凭证同事务落盘），
/// resolution 重放幂等（`ReplayedApproved`、凭证恒三张）。
#[test]
fn fault_kill9_between_request_and_approval_converges_uniquely() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let mut child = spawn_child("request-commit-complete", &fixture.base);
    let marker = await_marker(&mut child);
    let plan_id = plan_id_from_marker(&marker);
    kill_and_reap(&mut child);

    assert_eq!(
        raw_count(&fixture.db_path, "plan_materialization_requests"),
        1
    );
    let authority = SqlitePlanAuthority::open(&fixture.db_path).expect("reopen after kill");
    let node_id = first_node_id(&authority, plan_id);
    let node = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(node.state, PlanNodeState::WaitingResource);
    assert_eq!(node.transition_count, 2);

    // The stale pre-crash request replay is answered from the durable
    // row, never a second pending request.
    let replay = authority
        .request_materialization(gate_request(plan_id, node_id, 0x21))
        .expect("replay request");
    assert!(matches!(
        replay,
        MaterializationRequestDecision::Replayed(_)
    ));
    assert_eq!(
        raw_count(&fixture.db_path, "plan_materialization_requests"),
        1
    );

    // The approval converges to the unique post-crash outcome.
    let approved = authority
        .resolve_materialization(gate_approval(0x21))
        .expect("approve after restart");
    assert!(matches!(
        approved,
        MaterializationResolutionDecision::Approved(_)
    ));
    let node = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(node.state, PlanNodeState::Materializing);
    assert_eq!(node.transition_count, 3);
    assert_eq!(
        raw_count(&fixture.db_path, "plan_materialization_requests"),
        1,
        "approval resolves the existing request, it does not add one"
    );

    // And replays exactly once.
    let replayed = authority
        .resolve_materialization(gate_approval(0x21))
        .expect("approval replay");
    assert!(matches!(
        replayed,
        MaterializationResolutionDecision::ReplayedApproved(_)
    ));
    assert_eq!(
        authority
            .inspect_node_vouchers(plan_id, node_id)
            .expect("vouchers")
            .len(),
        3
    );
    assert_integrity(&fixture.db_path);
}

// ---------------------------------------------------------------------------
// F3: hard I/O error on the approval transaction fails closed
// ---------------------------------------------------------------------------

/// approval 事务写入硬 I/O 错误：`FailWritesAfter { 0, IoErr }` 下
/// `resolve_materialization` 必须以 `PlanStoreError::Sqlite` 显式失败（错
/// 误链含 I/O 条件），不返回假成功；无半截状态（请求仍 `PENDING`、节点
/// 仍 `WAITING_RESOURCE`、凭证恒两张）；disarm 后同一 resolution 重试成
/// 功且收敛到 F2 同一终态。
#[test]
fn fault_io_error_on_approval_fails_closed_and_retry_succeeds() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let authority = open_shim(&fixture.db_path);
    let plan_id = apply_revision_one(&authority);
    let node_id = first_node_id(&authority, plan_id);
    authority
        .request_materialization(gate_request(plan_id, node_id, 0x21))
        .expect("pending request");

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .resolve_materialization(gate_approval(0x21))
        .expect_err("approval must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);

    // Fail-closed leaves no half state.
    assert_eq!(
        raw_count(&fixture.db_path, "plan_materialization_requests"),
        1
    );
    let request = authority
        .inspect_materialization_request(IdempotencyKey::from_bytes([0x21; 16]))
        .expect("inspect request")
        .expect("request row");
    assert_eq!(
        request.status,
        nlos_plan::MaterializationRequestStatus::Pending
    );
    let node = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(node.state, PlanNodeState::WaitingResource);
    assert_eq!(
        authority
            .inspect_node_vouchers(plan_id, node_id)
            .expect("vouchers")
            .len(),
        2
    );

    // Removing the fault makes the very same resolution succeed.
    nlos_store_fault::disarm();
    let retried = authority
        .resolve_materialization(gate_approval(0x21))
        .expect("approval after disarm");
    assert!(matches!(
        retried,
        MaterializationResolutionDecision::Approved(_)
    ));
    let node = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(node.state, PlanNodeState::Materializing);
    assert_eq!(
        authority
            .inspect_node_vouchers(plan_id, node_id)
            .expect("vouchers")
            .len(),
        3
    );
    assert_integrity(&fixture.db_path);
}

// ---------------------------------------------------------------------------
// F4: silent write loss on the approval — converge to the same durable state
// ---------------------------------------------------------------------------

/// approval 静默丢写（断电模型）：`PowerLossAfter { 0 }` 下 resolution
/// 「报告成功」但写入从未落盘；连接必须先死亡（真实断电同样会杀死它）
/// 使恢复只看落盘字节；重开后幻影 approval 不冒充已提交事实（请求仍
/// `PENDING`、节点仍 `WAITING_RESOURCE`、凭证恒两张、integrity ok）——
/// 不存在 approved-但未翻状态的崩溃窗口（verdict 与状态翻转同事务）；
/// 丢失的 resolution 可重做且恰好解析一次；二次重开验证真持久（
/// `APPROVED` + `MATERIALIZING` + 三张凭证 + 重放幂等）。
#[test]
fn fault_silent_write_loss_on_approval_redo_resolves_once_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let authority = open_shim(&fixture.db_path);
    let plan_id = apply_revision_one(&authority);
    let node_id = first_node_id(&authority, plan_id);
    authority
        .request_materialization(gate_request(plan_id, node_id, 0x21))
        .expect("pending request");

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = authority
        .resolve_materialization(gate_approval(0x21))
        .expect("resolution reports success under power loss");
    assert!(matches!(
        phantom,
        MaterializationResolutionDecision::Approved(_)
    ));
    nlos_store_fault::disarm();
    // The surviving connection keeps a wal-index that references frames
    // the disk never saw; it must die first (as a real power loss would
    // kill it) so recovery sees durable bytes alone.
    drop(authority);

    let recovered = SqlitePlanAuthority::open(&fixture.db_path).expect("reopen after power loss");
    let request = recovered
        .inspect_materialization_request(IdempotencyKey::from_bytes([0x21; 16]))
        .expect("inspect request")
        .expect("request row");
    assert_eq!(
        request.status,
        nlos_plan::MaterializationRequestStatus::Pending,
        "silently dropped approval must not fabricate durability"
    );
    let node = recovered
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(node.state, PlanNodeState::WaitingResource);
    assert_eq!(
        recovered
            .inspect_node_vouchers(plan_id, node_id)
            .expect("vouchers")
            .len(),
        2
    );
    assert_integrity(&fixture.db_path);

    // The lost resolution is redoable and resolves exactly once.
    let redone = recovered
        .resolve_materialization(gate_approval(0x21))
        .expect("redo the approval");
    assert!(matches!(
        redone,
        MaterializationResolutionDecision::Approved(_)
    ));
    drop(recovered);

    let verified = SqlitePlanAuthority::open(&fixture.db_path).expect("reopen after redo");
    let request = verified
        .inspect_materialization_request(IdempotencyKey::from_bytes([0x21; 16]))
        .expect("inspect request")
        .expect("request row");
    assert_eq!(
        request.status,
        nlos_plan::MaterializationRequestStatus::Approved
    );
    assert!(request.approved_voucher_id.is_some());
    let node = verified
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(node.state, PlanNodeState::Materializing);
    assert_eq!(
        verified
            .inspect_node_vouchers(plan_id, node_id)
            .expect("vouchers")
            .len(),
        3
    );
    let replayed = verified
        .resolve_materialization(gate_approval(0x21))
        .expect("approval replay after redo");
    assert!(matches!(
        replayed,
        MaterializationResolutionDecision::ReplayedApproved(_)
    ));
    assert_eq!(
        raw_count(&fixture.db_path, "plan_materialization_requests"),
        1
    );
    assert_integrity(&fixture.db_path);
}
