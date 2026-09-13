# W26 统一恢复面 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把跨 authority 提交契约的有界收敛补齐为两域自动(Semantic 恢复台账 + worker 双域驱动),并以读侧聚合类型统一 TaskCommitReceipt。

**Architecture:** 克隆 Artifact 已验证的"durable 台账 + CAS/backoff + 常驻 worker"模式到 Semantic(schema v42 两张新表);worker 的 `durable_cycle` 拆为按域顺序执行的两半,故障粒度按域独立;统一 receipt 为纯读侧 enum + 确定性 digest,不改任何持久化事实。

**Tech Stack:** Rust 2024 edition(工具链 1.97)、SQLite(STRICT 表 + 触发器不可变约束)、既有 crate:`nlos-task`、`nlos-commit-coordinator`、`nlos-store-fault`(故障注入)。

**Spec:** `docs/superpowers/specs/2026-09-13-unified-recovery-plane-design.md`

## Global Constraints

- 提交作者:`cty12356541 <171764500+cty12356541@users.noreply.github.com>`(仓库级已配置,勿改)
- 提交信息:`<type>(<scope>): <中文主题> (W26-00N)`,只暂存本任务写集,禁 `git add -A`
- 每任务提交前:`cargo fmt --check` + 本任务定向测试绿
- 零新外部依赖;clippy `--workspace --all-targets -- -D warnings` 零警告
- schema 只加不改:v42 不触碰既有表任何列;历史行零改写
- 语义边界:全部仍是单机本地 SQLite H3 证据,任何文档/注释不得声称跨 authority 原子性或分布式收敛
- 并行车道写集不相交;W26-002 的 semantic 半边在 W26-001 Task 3 落地后的屏障点集成(见 Task 7 前置)

## Lane W26-001:Semantic 恢复台账(nlos-task)

### Task 1: schema v42 迁移

**Files:**
- Modify: `crates/nlos-task/src/migrations.rs`(文件尾部,`migrate_v41` 之后)
- Modify: `crates/nlos-task/src/store.rs:47`(import 列表)、`store.rs:69`(`SCHEMA_VERSION`)、迁移分派处(`store.rs:356-357` 之后)
- Test: `crates/nlos-task/tests/semantic_recovery_schema.rs`(新建)

**Interfaces:**
- Consumes: 既有 `migrate_v41`、`task_semantic_commit_plans` 表(schema v25)
- Produces: `pub(crate) fn migrate_v42(connection: &mut Connection) -> Result<(), TaskStoreError>`;`SCHEMA_VERSION = 42`;表 `task_semantic_recovery`、`task_semantic_recovery_alert_receipts`

- [ ] **Step 1: 写失败测试**

```rust
// crates/nlos-task/tests/semantic_recovery_schema.rs
use nlos_task::SqliteTaskAuthority;

#[test]
fn schema_v42_creates_semantic_recovery_tables_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let authority = SqliteTaskAuthority::open(dir.path()).unwrap();
    // 打开即迁移到当前版本;两张新表存在且 user_version = 42
    let count: i64 = authority
        .sqlite_connection()
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table'
                    AND name IN ('task_semantic_recovery',
                                 'task_semantic_recovery_alert_receipts')", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let version: i64 = authority
        .sqlite_connection()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 42);
    // 重开同一 root 幂等
    drop(authority);
    let authority = SqliteTaskAuthority::open(dir.path()).unwrap();
    let version: i64 = authority
        .sqlite_connection()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 42);
}
```

注:若 `SqliteTaskAuthority` 未暴露 `sqlite_connection()` 测试访问器,改用 `rusqlite::Connection::open` 直开同一文件断言(参照既有 migration 测试的访问方式;先 grep `tests/` 里对 `user_version` 的既有断言抄访问路径)。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nlos-task --test semantic_recovery_schema`
Expected: FAIL(表不存在 / user_version = 41)

- [ ] **Step 3: 实现**

`migrations.rs` 尾部新增(逐列镜像 `SCHEMA_V8_SQL`/`SCHEMA_V9_SQL`,recovery.rs:722/746,只换名与外键指向):

```rust
pub(crate) const SCHEMA_V42_SQL: &str = "CREATE TABLE task_semantic_recovery (
        plan_id BLOB PRIMARY KEY NOT NULL CHECK(length(plan_id) = 16),
        recovery_state INTEGER NOT NULL CHECK(recovery_state IN (0, 1, 2)),
        consecutive_failures BLOB NOT NULL CHECK(length(consecutive_failures) = 8),
        total_failures BLOB NOT NULL CHECK(length(total_failures) = 8),
        last_failure_source INTEGER NOT NULL CHECK(last_failure_source IN (0, 1, 2)),
        first_failed_at_ms INTEGER NOT NULL CHECK(first_failed_at_ms >= 0),
        last_failed_at_ms INTEGER NOT NULL CHECK(last_failed_at_ms >= first_failed_at_ms),
        next_retry_at_ms INTEGER,
        escalated_at_ms INTEGER,
        resolved_at_ms INTEGER,
        updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
        FOREIGN KEY(plan_id) REFERENCES task_semantic_commit_plans(plan_id),
        CHECK(total_failures >= consecutive_failures),
        CHECK((recovery_state = 0) = (next_retry_at_ms IS NOT NULL)),
        CHECK((recovery_state = 1) = (escalated_at_ms IS NOT NULL)),
        CHECK((recovery_state = 2) = (resolved_at_ms IS NOT NULL))
     ) STRICT;

     CREATE INDEX task_semantic_recovery_due
        ON task_semantic_recovery(recovery_state, next_retry_at_ms, plan_id);

     CREATE TABLE task_semantic_recovery_alert_receipts (
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
        total_failures BLOB NOT NULL CHECK(length(total_failures) = 8),
        principal_id BLOB NOT NULL CHECK(length(principal_id) = 16),
        idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
        acknowledged_at_ms INTEGER NOT NULL CHECK(acknowledged_at_ms >= 0),
        FOREIGN KEY(plan_id) REFERENCES task_semantic_recovery(plan_id),
        UNIQUE(plan_id, total_failures)
     ) STRICT;

     CREATE TRIGGER task_semantic_recovery_alert_receipts_immutable_update
     BEFORE UPDATE ON task_semantic_recovery_alert_receipts
     BEGIN
        SELECT RAISE(ABORT, 'Semantic recovery alert receipts are immutable');
     END;

     CREATE TRIGGER task_semantic_recovery_alert_receipts_immutable_delete
     BEFORE DELETE ON task_semantic_recovery_alert_receipts
     BEGIN
        SELECT RAISE(ABORT, 'Semantic recovery alert receipts are immutable');
     END;

     PRAGMA user_version = 42;";

pub(crate) fn migrate_v42(connection: &mut Connection) -> Result<(), TaskStoreError> {
    // 镜像 migrate_v40 的存在性守卫风格(migrations.rs:1342):表已存在则只补 user_version
    let exists: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table'
         AND name = 'task_semantic_recovery'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        connection.execute_batch(SCHEMA_V42_SQL)?;
    } else {
        connection.pragma_update(None, "user_version", 42)?;
    }
    Ok(())
}
```

`store.rs`:import 列表加 `migrate_v42`;`SCHEMA_VERSION` 改 `42`;迁移分派在 `migrate_v41(&mut connection)?;`(store.rs:357)之后加 `migrate_v42(&mut connection)?;`。全新库的 bootstrap 路径若直接执行最新全量 SQL,把 `SCHEMA_V42_SQL` 的表/触发器部分并入该全量常量(参照 v40/v41 在 bootstrap 中的接线方式,grep `SCHEMA_V4` 常量引用点确认)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p nlos-task --test semantic_recovery_schema`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add crates/nlos-task/src/migrations.rs crates/nlos-task/src/store.rs crates/nlos-task/tests/semantic_recovery_schema.rs
git commit -m "feat(nlos-task): semantic 恢复台账 schema v42 表组 (W26-001)"
```

### Task 2: 台账类型 + CAS 记录 + 检查回读

**Files:**
- Modify: `crates/nlos-task/src/recovery.rs`(Artifact 家族之后新增 Semantic 家族)
- Modify: `crates/nlos-task/src/lib.rs:218`(recovery 导出块旁新增)、`lib.rs:305` 附近(TaskStoreError 新变体)
- Test: `crates/nlos-task/tests/semantic_recovery_ledger.rs`(新建)

**Interfaces:**
- Consumes: Task 1 的两张表;`SemanticCommitPlanId`(semantic_commit.rs:64)
- Produces(后续 Task 与 W26-002 依赖,签名逐字):
```rust
pub enum SemanticRecoveryState { Retrying, Escalated, Resolved }
pub enum SemanticRecoveryFailureSource { TaskAuthority, SemanticAuthority, Coordinator }
pub struct SemanticRecoveryFailureRequest {
    pub plan_id: SemanticCommitPlanId,
    pub expected_total_failures: u64,
    pub source: SemanticRecoveryFailureSource,
    pub observed_at_ms: i64,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}
pub struct SemanticRecoveryRecord { /* 逐字段镜像 ArtifactRecoveryRecord(recovery.rs:89),plan_id 类型换 SemanticCommitPlanId */ }
// SqliteTaskAuthority 方法:
pub fn record_semantic_recovery_failure(&self, request: SemanticRecoveryFailureRequest)
    -> Result<SemanticRecoveryRecord, TaskStoreError>;
pub fn inspect_semantic_recovery(&self, plan_id: SemanticCommitPlanId)
    -> Result<Option<SemanticRecoveryRecord>, TaskStoreError>;
```
错误变体(镜像 lib.rs:305 的 `ArtifactRecoveryCasMismatch { expected, current }`):`SemanticRecoveryCasMismatch { expected: u64, current: u64 }`、`InvalidSemanticRecoveryState { /* 镜像 artifact 同名变体字段 */ }`,Display 穷举接 `lib.rs:678` 同款格式。

- [ ] **Step 1: 写失败测试**

```rust
// crates/nlos-task/tests/semantic_recovery_ledger.rs
// 复用 semantic_pending_restart_scan.rs 的 store/plan 构造 helper(grep 该文件里
// 建 task store + prepare semantic plan 的既有函数,直接调用,勿新造 fixture)。
#[test]
fn record_failure_roundtrips_and_cas_rejects_stale_expected() {
    let (authority, plan_id) = semantic_store_with_pending_plan();
    let first = authority.record_semantic_recovery_failure(SemanticRecoveryFailureRequest {
        plan_id, expected_total_failures: 0,
        source: SemanticRecoveryFailureSource::Coordinator,
        observed_at_ms: 1_000, base_delay_ms: 100, max_delay_ms: 5_000,
    }).unwrap();
    assert_eq!(first.total_failures, 1);
    assert_eq!(first.state, SemanticRecoveryState::Retrying);
    assert_eq!(first.next_retry_at_ms, Some(1_000 + 100));
    // 过期 expected 被 CAS 拒绝
    let err = authority.record_semantic_recovery_failure(SemanticRecoveryFailureRequest {
        plan_id, expected_total_failures: 0,
        source: SemanticRecoveryFailureSource::Coordinator,
        observed_at_ms: 1_100, base_delay_ms: 100, max_delay_ms: 5_000,
    }).unwrap_err();
    assert!(matches!(err, TaskStoreError::SemanticRecoveryCasMismatch { expected: 0, current: 1 }));
    // 回读一致
    let read = authority.inspect_semantic_recovery(plan_id).unwrap().unwrap();
    assert_eq!(read.total_failures, 1);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nlos-task --test semantic_recovery_ledger`
Expected: FAIL(类型/方法未定义)

- [ ] **Step 3: 实现**

在 `recovery.rs` 按 `record_artifact_recovery_failure`(recovery.rs:159-245)的完整事务逻辑镜像:先 `inspect` 读 prior → CAS 比对 → `Retrying` 状态校验 → 退避 `next_retry = observed + min(base * 2^consecutive, max)`(指数封顶公式照抄 artifact 实现)→ 同事务 UPSERT 行。计数字段按 DDL 存 8 字节大端 BLOB(照抄 artifact 的编码 helper)。达到 escalation 阈值的行为照抄 artifact(阈值语义与 artifact 逐位一致,grep artifact 实现中的 escalated 转换条件)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p nlos-task --test semantic_recovery_ledger`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add crates/nlos-task/src/recovery.rs crates/nlos-task/src/lib.rs crates/nlos-task/tests/semantic_recovery_ledger.rs
git commit -m "feat(nlos-task): semantic 恢复台账 CAS 记录与回读 (W26-001)"
```

### Task 3: 到期扫描 + resume + 汇总

**Files:**
- Modify: `crates/nlos-task/src/recovery.rs`、`crates/nlos-task/src/lib.rs`(导出)
- Test: `crates/nlos-task/tests/semantic_recovery_ledger.rs`(追加)

**Interfaces:**
- Produces(W26-002 Task 7 依赖,签名逐字):
```rust
pub fn list_due_semantic_commit_plans(&self, limit: usize, now_ms: i64)
    -> Result<Vec<SemanticCommitPlanRecord>, TaskStoreError>;
pub fn resume_semantic_recovery(&self, request: SemanticRecoveryResumeRequest)
    -> Result<SemanticRecoveryRecord, TaskStoreError>;
pub struct SemanticRecoveryResumeRequest {
    pub plan_id: SemanticCommitPlanId,
    pub expected_total_failures: u64,
    pub resumed_at_ms: i64,
}
pub fn summarize_semantic_recovery(&self) -> Result<SemanticRecoverySummary, TaskStoreError>;
// SemanticRecoverySummary 逐字段镜像 ArtifactRecoverySummary(recovery.rs:104)
```

- [ ] **Step 1: 写失败测试**(追加到 semantic_recovery_ledger.rs)

```rust
#[test]
fn due_scan_filters_state_and_time_and_resume_requeues() {
    let (authority, plan_id) = semantic_store_with_pending_plan();
    // 三次失败推高 next_retry
    let mut expected = 0u64;
    for t in [1_000i64, 2_000, 3_000] {
        let rec = authority.record_semantic_recovery_failure(SemanticRecoveryFailureRequest {
            plan_id, expected_total_failures: expected,
            source: SemanticRecoveryFailureSource::Coordinator,
            observed_at_ms: t, base_delay_ms: 100, max_delay_ms: 5_000,
        }).unwrap();
        expected = rec.total_failures;
    }
    // 未到期:不返回
    assert!(authority.list_due_semantic_commit_plans(10, 3_000).unwrap().is_empty());
    // 到期:返回该 plan
    let due = authority.list_due_semantic_commit_plans(10, 3_500).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);
    // limit=0 返回空(镜像 list_incomplete 的守卫)
    assert!(authority.list_due_semantic_commit_plans(0, 3_500).unwrap().is_empty());
    // Escalated 不进扫描;resume 后重回 Retrying
    // (escalate 路径按 artifact 阈值语义构造:连续失败达到阈值后 state=Escalated)
    // —— 具体构造次数以 Task 2 实现的 artifact 镜像阈值为准,断言:
    // assert_eq!(record.state, SemanticRecoveryState::Escalated);
    // assert!(authority.list_due_semantic_commit_plans(10, 远期).unwrap().is_empty());
    let resumed = authority.resume_semantic_recovery(SemanticRecoveryResumeRequest {
        plan_id, expected_total_failures: expected, resumed_at_ms: 10_000,
    }).unwrap();
    assert_eq!(resumed.state, SemanticRecoveryState::Retrying);
    assert!(resumed.next_retry_at_ms.is_some());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nlos-task --test semantic_recovery_ledger`
Expected: FAIL

- [ ] **Step 3: 实现**

`list_due_semantic_commit_plans` 镜像 `list_due_artifact_commit_plans`(recovery.rs:409-459)的 SQL 语义:JOIN `task_semantic_recovery`(recovery_state=0 且 next_retry_at_ms ≤ now)∪ 无台账行的 incomplete plan(镜像 artifact 对无台账 plan 的处理——先读 artifact 版 SQL 判断"无台账即到期"还是"无台账排除",照抄同款决定);**无台账行的 plan 必须可被扫描**(spec §3.1-005:台账丢失重扫 durable plan 重建调度)。`resume` 镜像 recovery.rs:460-504(Escalated→Retrying CAS)。`summarize` 镜像 recovery.rs:260-291。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p nlos-task --test semantic_recovery_ledger`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add crates/nlos-task/src/recovery.rs crates/nlos-task/src/lib.rs crates/nlos-task/tests/semantic_recovery_ledger.rs
git commit -m "feat(nlos-task): semantic 到期扫描/resume/汇总 + 无台账重扫 (W26-001)"
```

### Task 4: 告警面 + finalize 成功置 Resolved

**Files:**
- Modify: `crates/nlos-task/src/recovery.rs`、`crates/nlos-task/src/lib.rs`(导出)、`crates/nlos-task/src/semantic_commit.rs`(成功路径调 resolve)、`crates/nlos-task/src/reconcile.rs:1969` 附近(`finalize_commit_v3_with_persisted_semantic_envelope` 成功路径调 resolve)
- Test: `crates/nlos-task/tests/semantic_recovery_ledger.rs`(追加)

**Interfaces:**
- Consumes: `resolve_recovery`(recovery.rs:505,pub(crate))的镜像位
- Produces:
```rust
pub fn list_semantic_recovery_alerts(&self) -> Result<Vec<SemanticRecoveryAlert>, TaskStoreError>;
pub fn acknowledge_semantic_recovery_alert(&self, request: SemanticRecoveryAlertAcknowledgeRequest)
    -> Result<SemanticRecoveryAlertReceipt, TaskStoreError>;
pub struct SemanticRecoveryAlertAcknowledgeRequest { /* 镜像 recovery.rs:112 */ }
pub struct SemanticRecoveryAlertReceipt { /* 镜像 recovery.rs:121 */ }
pub enum SemanticRecoveryAlertAcknowledgeDecision { Acknowledged, Replayed /* 镜像 recovery.rs:131 全集 */ }
pub struct SemanticRecoveryAlert { /* 镜像 recovery.rs:146 */ }
pub(crate) fn resolve_semantic_recovery(/* 镜像 resolve_recovery 参数,plan 身份换 SemanticCommitPlanId */);
```

- [ ] **Step 1: 写失败测试**(追加;告警生命周期 + 幂等 acknowledge + immutable 触发器 + finalize 成功后台账 Resolved——用 semantic_pending_restart_scan.rs 的 converge 成功路径驱动 finalize,断言 `inspect_semantic_recovery` 为 Resolved;以及"台账行被删后 converge 仍收敛"的自愈场景)

```rust
#[test]
fn alert_lifecycle_and_finalize_resolves_ledger() {
    let (authority, plan_id) = semantic_store_with_pending_plan();
    // 失败至 escalation,产生告警
    /* 按 Task 2/3 阈值推到 Escalated */
    let alerts = authority.list_semantic_recovery_alerts().unwrap();
    assert!(!alerts.is_empty());
    let receipt = authority.acknowledge_semantic_recovery_alert(
        SemanticRecoveryAlertAcknowledgeRequest { /* plan_id, total_failures, principal, idempotency_key, acknowledged_at_ms */ },
    ).unwrap();
    // 同 key 重放 Acknowledged→Replayed,不双记(UNIQUE(plan_id,total_failures))
    // finalize 成功(手动 converge 路径,镜像 semantic_pending_restart_scan.rs 的驱动)
    // 断言 inspect_semantic_recovery(plan_id) 为 Resolved
    // 删台账行 → 重扫 converge → 终态不变(自愈)
}
```

- [ ] **Step 2: 跑测试确认失败** — `cargo test -p nlos-task --test semantic_recovery_ledger`,Expected: FAIL
- [ ] **Step 3: 实现** — `list/acknowledge` 镜像 recovery.rs:292-408(含 idempotency 重放与触发器 ABORT 映射);`resolve_semantic_recovery` 镜像 recovery.rs:505;在 `semantic_commit.rs` 与 `reconcile.rs` 的 semantic finalize 成功事务内调用(镜像 artifact finalize 调 `resolve_recovery` 的接线点,grep `resolve_recovery` 的全部调用方照抄结构)
- [ ] **Step 4: 跑测试确认通过** — 同上,Expected: PASS
- [ ] **Step 5: 提交**

```bash
git add crates/nlos-task/src/recovery.rs crates/nlos-task/src/lib.rs crates/nlos-task/src/semantic_commit.rs crates/nlos-task/src/reconcile.rs crates/nlos-task/tests/semantic_recovery_ledger.rs
git commit -m "feat(nlos-task): semantic 恢复告警面 + finalize 置 Resolved + 台账自愈 (W26-001)"
```

### Task 5: 台账 F1–F4 故障注入矩阵

**Files:**
- Test: `crates/nlos-task/tests/semantic_recovery_fault_matrix.rs`(新建;harness 结构照抄 `artifact_multi_record_fault_restart.rs` 的 nlos-store-fault 接线)

**Interfaces:**
- Consumes: Task 2–4 全部 API;`nlos-store-fault` 注入器(照抄 artifact 故障测试的构造方式)

- [ ] **Step 1: 写四个失败测试** — F1 kill-9 中断(record 事务中途中断→重开重放无双记);F2 commit 后崩溃(total 已持久,重放幂等);F3 IoErr(record 返回错误,plan 事实无损);F4 静默丢写(重开收敛终态唯一)。断言全部镜像 artifact 故障矩阵的既有断言形态。
- [ ] **Step 2: 跑测试确认失败或暴露真缺陷**(若实现已正确,F1/F2 允许直接绿——如实记录,不算 TDD 违例,故障矩阵是验证性测试)
- [ ] **Step 3: 修补暴露的缺陷**(若有)
- [ ] **Step 4: 全绿** — `cargo test -p nlos-task --test semantic_recovery_fault_matrix`,Expected: 4 passed
- [ ] **Step 5: 提交**

```bash
git add crates/nlos-task/tests/semantic_recovery_fault_matrix.rs
git commit -m "test(nlos-task): semantic 恢复台账 F1-F4 故障注入矩阵 (W26-001)"
```

## Lane W26-002:Worker 双域驱动(nlos-commit-coordinator)

### Task 6: durable_cycle 拆分 + 按域健康(artifact 行为不变)

**Files:**
- Modify: `crates/nlos-commit-coordinator/src/worker.rs`(durable_cycle worker.rs:329 起重构)
- Test: `crates/nlos-commit-coordinator/tests/unified_worker_dual_domain.rs`(新建,先放 artifact 回归)

**Interfaces:**
- Produces(W26-002 内部 + W26-004 消费):
```rust
// RecoveryWorkerHealth 增量字段(既有字段语义不变,代表 artifact 域):
pub semantic_durable_retrying: u64,
pub semantic_durable_escalated: u64,
pub semantic_durable_unacknowledged_escalated: u64,
pub semantic_durable_resolved: u64,
pub semantic_consecutive_failed_cycles: usize,
pub semantic_total_inspected: u64,
pub semantic_total_finalized: u64,
// 域故障位(线程 state 不变,Faulted 仍是线程级终态):
pub semantic_domain_faulted: bool,   // true = 停扫 semantic,artifact 继续
pub artifact_domain_faulted: bool,
```

- [ ] **Step 1: 写回归测试** — 从既有 worker 测试(grep worker 相关测试文件)复制一个"artifact pending → worker 收敛"用例进 `unified_worker_dual_domain.rs`,断言既有 `RecoveryWorkerHealth` 字段行为与重构前逐位一致 + 新 semantic 字段全零。
- [ ] **Step 2: 跑确认基线绿**(重构前测试即可编译——新字段默认零,旧断言不变)→ 先跑既有 worker 测试全绿作基线:`cargo test -p nlos-commit-coordinator`
- [ ] **Step 3: 实现** — 把 `durable_cycle` 拆成 `artifact_cycle(...)` + `semantic_cycle_stub(...)`(先返回空 outcome);线程主循环顺序调用两半;per-domain 失败累计与 `semantic_domain_faulted` 置位逻辑(阈值沿用 `failure_threshold`,按域独立计数);health 聚合两半。
- [ ] **Step 4: 跑全绿** — `cargo test -p nlos-commit-coordinator`,Expected: 全部 PASS(含既有测试零回归)
- [ ] **Step 5: 提交**

```bash
git add crates/nlos-commit-coordinator/src/worker.rs crates/nlos-commit-coordinator/tests/unified_worker_dual_domain.rs
git commit -m "refactor(nlos-commit-coordinator): durable_cycle 拆分双域 + 按域健康 (W26-002)"
```

### Task 7: semantic 半边接线(屏障:Task 3 之后)

**前置:W26-001 Task 3 已合并。**

**Files:**
- Modify: `crates/nlos-commit-coordinator/src/worker.rs`(`semantic_cycle_stub` 换真实现)
- Test: `crates/nlos-commit-coordinator/tests/unified_worker_dual_domain.rs`(追加)

**Interfaces:**
- Consumes: `list_due_semantic_commit_plans(limit, now_ms)`、`record_semantic_recovery_failure`、`inspect_semantic_recovery`、`SemanticRecoveryFailureRequest/Source`(Task 2/3 签名)
- Consumes: `SemanticCommitCoordinator::converge(ConvergeSemanticCommitRequest { plan_id, now_ms })`(commit-coordinator lib.rs:352 既有)

- [ ] **Step 1: 写失败测试(spec §5 核心场景:无 caller 重启收敛)**

```rust
#[test]
fn worker_converges_pending_semantic_plan_without_caller() {
    // 建 task store + semantic store,prepare 一个 pending semantic plan
    // (fixture 照抄 semantic_pending_restart_scan.rs 的构造)
    // 启动 worker(短 poll_interval),不调用任何手动 converge
    // 轮询 health 直至 semantic_total_finalized >= 1(带超时)
    // 断言 plan 终态 Finalized;semantic_durable_resolved >= 1
}
```

再追加:`both_domains_converge_in_one_worker`(两域各一 pending,单 worker 全收敛)与 `semantic_domain_fault_isolated`(对 semantic store 注入连续失败——照抄 semantic_convergence_fault_injection.rs 的故障构造——达到阈值后 `semantic_domain_faulted == true` 且 artifact 域继续收敛另一 plan)。

- [ ] **Step 2: 跑测试确认失败** — `cargo test -p nlos-commit-coordinator --test unified_worker_dual_domain`,Expected: 新用例 FAIL
- [ ] **Step 3: 实现** — `semantic_cycle` 镜像 `artifact_cycle` 的结构:scan → `SemanticCommitCoordinator::converge` → 失败走 `record_semantic_recovery_failure`(source 映射:`CoordinatorError::Task→TaskAuthority`、`Semantic→SemanticAuthority`、`InvalidTimestamp→Coordinator`;注意 artifact 版把 `CoordinatorError::Semantic` 归 Coordinator 的映射在 semantic 域改为 SemanticAuthority)
- [ ] **Step 4: 跑全绿** — 同上,Expected: PASS
- [ ] **Step 5: 提交**

```bash
git add crates/nlos-commit-coordinator/src/worker.rs crates/nlos-commit-coordinator/tests/unified_worker_dual_domain.rs
git commit -m "feat(nlos-commit-coordinator): worker 接线 semantic 域自动收敛 + 域故障隔离 (W26-002)"
```

## Lane W26-003:统一 TaskCommitReceipt(nlos-task)

### Task 8: TaskCommitReceipt enum + digest

**Files:**
- Create: `crates/nlos-task/src/receipt.rs`
- Modify: `crates/nlos-task/src/lib.rs`(mod + pub use)
- Test: `crates/nlos-task/tests/unified_task_commit_receipt.rs`(新建)

**Interfaces:**
- Consumes: `TaskReceiptRecord`(model.rs:1203)、`ArtifactTaskCommitReceipt`(commit.rs:166)、`SemanticTaskCommitReceipt`(semantic_commit.rs:159)、`ResourceTaskCommitReceipt`/`SemanticResourceTaskCommitReceipt`(resource_commit.rs:68/93)
- Produces:
```rust
pub enum TaskCommitReceipt {
    Plain(TaskReceiptRecord),
    Artifact(ArtifactTaskCommitReceipt),
    Semantic(SemanticTaskCommitReceipt),
    Resource(ResourceTaskCommitReceipt),
    SemanticResource(SemanticResourceTaskCommitReceipt),
}
impl TaskCommitReceipt {
    /// 确定性摘要:canonical 逐字段编码(变体 discriminant u8 + 各 receipt 身份字段
    /// 大端定长编码)→ sha2 已在依赖树内则用之,否则手写 FNV-1a 64 并在文档钉死公式
    pub fn commit_receipt_digest(&self) -> [u8; 32];
}
```

- [ ] **Step 1: 写失败测试** — 五变体各构造一个最小 receipt(从既有测试的构造代码复制),断言:(a) 同构造两次 digest 相等;(b) 五个 digest 两两不同;(c) 序列化重建后 digest 相等(若 receipt 未实现序列化则跳过 c,如实登记);(d) `Display` 含变体名。
- [ ] **Step 2: 跑测试确认失败** — `cargo test -p nlos-task --test unified_task_commit_receipt`,Expected: FAIL
- [ ] **Step 3: 实现** — 新文件 receipt.rs;digest 用 SHA-256(crates 依赖树已有 sha2,先 `grep -rn "sha2" Cargo.toml crates/*/Cargo.toml` 确认,无则 FNV-1a 并在 `# Panics`/文档钉死);编码顺序与字段集在类型文档逐条列出,声明"变体扩展时必须同步此公式"
- [ ] **Step 4: 跑测试确认通过** — 同上,Expected: PASS
- [ ] **Step 5: 提交**

```bash
git add crates/nlos-task/src/receipt.rs crates/nlos-task/src/lib.rs crates/nlos-task/tests/unified_task_commit_receipt.rs
git commit -m "feat(nlos-task): 统一 TaskCommitReceipt 读侧聚合与确定性摘要 (W26-003)"
```

## Lane W26-004:登记收尾(串行,屏障:全部车道后)

### Task 9: 全仓验证门

**Files:** 无代码改动(验证-only)

- [ ] **Step 1: 全量测试** — `cargo test --workspace --no-fail-fast`,记录二进制数/passed/failed/ignored(与 W25 基线 213 二进制 1122 passed 对比,新增应为 semantic_recovery_schema、semantic_recovery_ledger、semantic_recovery_fault_matrix、unified_worker_dual_domain、unified_task_commit_receipt)
- [ ] **Step 2: fmt + clippy** — `cargo fmt --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 双 0
- [ ] **Step 3: push 前五查**(CLAUDE.md §4)——`git log origin/main..HEAD` 无夹带、staged diff 逐行、敏感信息扫描
- [ ] **Step 4: push 并确认三平台 CI 绿**(如实报告,失败则定位)

### Task 10: 台账登记

**Files:**
- Create: `docs/evidence/stage-b/b-task-008c2g-unified-recovery.md`
- Modify: `docs/management/stage-b-progress.md`(§3 波次 26 行 + §5/§6 同步)、`docs/superpowers/specs/2026-09-13-unified-recovery-plane-design.md`(状态行补"已实现+证据指针")

**Interfaces:**
- Consumes: Task 1–9 的实际命令输出与结果数字(回执真实性优先于好看)

- [ ] **Step 1: 写 Evidence 文件** — 载荷:目标(spec 链接)、五组验收测试的实际命令与结果、故障矩阵结果、未运行项显式列出、边界声明(单机 H3、不晋升 complete TaskWriteSet、verify-then-commit 契约引用 ADR-0013)
- [ ] **Step 2: 更新进度单** — §3 登记波次 26 车道表(镜像 W25 条目格式:每车道 commit、测试数、证据链接);§5 下一验收门更新;§6 ROAD-B-003 行追加;最后更新时间与 HEAD 基线
- [ ] **Step 3: 提交 + push**

```bash
git add docs/evidence/stage-b/b-task-008c2g-unified-recovery.md docs/management/stage-b-progress.md docs/superpowers/specs/2026-09-13-unified-recovery-plane-design.md
git commit -m "docs: 登记波次 26 四车道 + §3/§6 同步 (W26-004)"
git push origin main
```

---

## 执行编排

- 并行波次内:W26-001(Task 1→5 串行)、W26-003(Task 8)、W26-002 的 Task 6 可同时启动(写集不相交)
- 屏障 1:Task 3 完成后 → Task 7 可启动
- 屏障 2:Task 5/7/8 全绿后 → Task 9 → Task 10 串行收尾
- 单车道失败只阻塞其依赖者;多车道共享 target 锁竞争时用独立 `CARGO_TARGET_DIR`(W22 惯例)
