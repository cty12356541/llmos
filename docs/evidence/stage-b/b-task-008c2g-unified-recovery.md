# B-TASK-008C2G-UNIFIED-RECOVERY：统一恢复面（semantic 恢复台账 + worker 双域驱动 + 统一 TaskCommitReceipt）

状态：`PARTIAL_PASS`（2026-09-13，W26）

> 对应：[spec 统一恢复面设计](../../superpowers/specs/2026-09-13-unified-recovery-plane-design.md)（2026-09-13 定稿，W26-000 提交 `4322a17`）、[ADR-0013 跨 authority verify-then-commit 契约](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md)、[ADR-0005 authority-first 顺序](../../management/adrs/0005-task-write-set-authority-first.md)、`b-task-006h`–`006k`（Artifact 台账/worker 先例）、`b-task-008c2g-coord` 系列收敛证据
>
> 实现：`nlos-task` schema v42（`migrations.rs`）/ 台账 API（`recovery.rs`）/ 统一回执（`receipt.rs`）；`nlos-commit-coordinator` worker 双域（`worker.rs`）
>
> 提交范围：`1a14ae0..2b51d28`（14 提交 = 10 个三车道任务提交 + 3 个 chore 集成合并 + 1 个基线 lint 修复），分支 `feat/unified-recovery-plane`，集成分支验证见 §5

## 1. 本切片目标

把 [ADR-0013](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md) 有界收敛半边从「仅 Artifact 自动、Semantic 靠调用方」补齐为**两域均自动**：Semantic 获得与 Artifact 同构的 durable 恢复台账（schema v42 表组）与常驻 worker 驱动，重启后无人调用也能从 durable prefix 收敛到唯一终态；并以**读侧聚合类型**统一 TaskCommitReceipt 的表达（五变体 + 确定性 digest）。非目标（spec §1）：不给 Resource/Operation/Process 建 prepare/finalize 入口（W27+ 候选）、不做跨机/跨 Cell、不改 Artifact 既有台账 schema、不做 compensation 执行与跨进程 attestation。

## 2. 语义契约逐条对应（spec §3 的 15 条规范语句 → 实现位置）

### 2.1 SEM-RECOV-001..007（Semantic 恢复台账）

| 规范语句 | 实现位置 | 测试锚点 |
|---|---|---|
| `[SEM-RECOV-001]` 表组镜像 Artifact 台账语义：plan 身份、`total_failures`、`next_retry_at_ms` 指数退避封顶、`Escalated` 终态需显式 `resume`、告警行与 `acknowledge` | `crates/nlos-task/src/migrations.rs:1445`（`migrate_v42`，存在性守卫）与 `:2769`（`SCHEMA_V42_SQL`：`task_semantic_recovery` 逐列镜像 `SCHEMA_V8_SQL`、due 索引、`task_semantic_recovery_alert_receipts` 镜像 `SCHEMA_V9_SQL`、双不可变触发器）；`recovery.rs:655`（record）/`:991`（resume，Escalated 显式重回 Retrying） | `semantic_recovery_schema` 2 项；`semantic_recovery_ledger` 9 项 |
| `[SEM-RECOV-002]` `record_semantic_recovery_failure` 以 `total_failures` CAS，并发双写至多一次自增 | `recovery.rs:655`（`expected_total_failures` CAS 更新） | `record_failure_roundtrips_and_cas_rejects_stale_expected`；F1/F2 的 CAS 拒绝断言 |
| `[SEM-RECOV-003]` due scan 只返回非 Finalized 且 Retrying、`next_retry_at_ms` 已期；Escalated 不进扫描 | `recovery.rs:930`（`list_due_semantic_commit_plans`） | `due_scan_filters_state_and_time_and_resume_requeues` |
| `[SEM-RECOV-004]` finalize 成功路径按 `resolve_recovery` 同款语义置 `Resolved`；幂等重放不二次记失败 | `recovery.rs`（`resolve_semantic_recovery`）；接线于三个 finalize 成功事务内：`semantic_commit.rs:766`、`reconcile.rs:2639`/`:2671`（失败路径零台账写，grep 全核） | `alert_lifecycle_and_finalize_resolves_ledger`、`persisted_envelope_finalize_resolves_ledger`、F4 终态断言 |
| `[SEM-RECOV-005]` 台账写自身失败视为 infrastructure 失败进 worker 退避；台账行丢失不产生幻影收敛（重扫 durable plan 重建调度） | `worker.rs:652`（`semantic_cycle`：扫描/台账读/写/汇总的 `TaskStoreError` = infra 失败） | `ledger_row_loss_before_finalize_still_converges`（删行自愈）；F1（无台账行回到期扫描）；F4（幻影行不可见 + 自愈重扫） |
| `[SEM-RECOV-006]` 迁移幂等、历史行零改写 | `migrations.rs:1445`：`BEGIN IMMEDIATE` 事务内 `execute_batch`（提交 `3ffa7e7` 修复环：裸 `execute_batch` 事务化 + 部分态守卫返回 `CorruptRecord`，Ruling R6/R6a：命名部件 = 2 表 + 1 索引 + 2 触发器 = 5） | `semantic_recovery_schema` 2 项（幂等重放 + v41 部分态守卫） |
| `[SEM-RECOV-007]` 告警面 `list`/`acknowledge`/`summarize` 与 Artifact 逐位同构 | `recovery.rs:798`（`list_semantic_recovery_alerts`）、`:853`（`acknowledge_semantic_recovery_alert`）、`:756`（`summarize_semantic_recovery`） | `alert_acknowledgement_fences_and_receipts_are_immutable`、`summarize_counts_semantic_recovery_states` |

### 2.2 UNIFIED-WORKER-001..004（worker 双域驱动）

| 规范语句 | 实现位置 | 测试锚点 |
|---|---|---|
| `[UNIFIED-WORKER-001]` `durable_cycle` 每轮先 Artifact 域后 Semantic 域（扫描→converge→失败记台账），两域互不嵌套事务 | `worker.rs`：`run_worker` 主循环顺序驱动 `artifact_cycle`（`:529`，原 `durable_cycle` 主体逐行改名）与 `semantic_cycle`（`:652`，stub 在 `60517bb`、真接线在 `7360698`） | `both_domains_converge_in_one_worker` |
| `[UNIFIED-WORKER-002]` 故障粒度按域独立：单域连续 infra 失败达阈值只把该域置 Faulted；`RecoveryWorkerHealth` 按域分列 | `worker.rs:408`（`account_cycle` 单锁聚合两半）；health 新字段 `worker.rs:146-163`（`semantic_durable_retrying/escalated/unacknowledged_escalated/resolved`、`semantic_consecutive_failed_cycles`、`semantic_total_inspected/finalized`、`semantic_domain_faulted`、`artifact_domain_faulted`） | `semantic_domain_fault_is_isolated_from_artifact_convergence`、`recovery_worker_health_defaults_keep_semantic_domain_quiescent` |
| `[UNIFIED-WORKER-003]` 阈值沿用单一 `failure_threshold`，按域独立累计（不新增每域配置项） | `worker.rs:408`（`account_cycle` 消费既有 `config.failure_threshold`，零新配置字段）；semantic 域故障位粘滞、artifact 域达阈值照旧线程 `Faulted` 终态 | `semantic_authority_failures_escalate_ledger_without_faulting_domain`（计划级走台账）；`mixed_semantic_infra_and_artifact_plan_failures_use_separate_budgets`（infra 预算分离） |
| `[UNIFIED-WORKER-004]` stop/首轮立即 bounded scan/轮询间隔语义与现 worker 一致 | `worker.rs` `run_worker`/`stop` 未动；既有入口 `start` 委托 `start_with_semantic_authority(…, None, …)`（`:247`/`:260`），semantic 半轮按构造静默，既有 43 项 coordinator 测试逐位不变 | `artifact_pending_plan_converges_with_unchanged_health_and_quiescent_semantic_domain`；既有套件零回归（§5） |

### 2.3 RECEIPT-UNIFY-001..004（统一 TaskCommitReceipt 读侧聚合）

| 规范语句 | 实现位置 | 测试锚点 |
|---|---|---|
| `[RECEIPT-UNIFY-001]` 五变体穷举 enum（Plain/Artifact/Semantic/Resource/SemanticResource），不适用 non_exhaustive | `crates/nlos-task/src/receipt.rs:122` | `sample_receipts` 五构造 + `variant_digests_are_pairwise_distinct` |
| `[RECEIPT-UNIFY-002]` `commit_receipt_digest()` 五变体确定性摘要：canonical 编码 + 身份字段 | `receipt.rs:144`（SHA-256 over domain ‖ variant discriminant ‖ canonical 编码） | `digest_is_deterministic_across_equal_constructions`（等价构造同摘要——值确定性钉死，跨重启一致为其直接推论）、`same_variant_digest_tracks_identity_fields` |
| `[RECEIPT-UNIFY-003]` 纯读侧聚合：不建持久化统一表、不改既有 receipt 表与写入路径 | `receipt.rs` 全文件（零 SQL）；`lib.rs` 仅 `mod receipt` + `pub use` 两行 | nlos-task 全量零回归（既有 receipt 路径测试不动照绿） |
| `[RECEIPT-UNIFY-004]` Display/Debug 穷举；digest 公式在类型文档钉死，变体扩展必须同步 | `receipt.rs` 类型文档（六步 canonical 编码公式 + 「变体集或嵌套记录字段扩展必须同步更新公式与文档」加粗钉死）；Display impl 穷举 | `display_contains_variant_name` |

## 3. 验收证据（spec §5 五组验收门，实际命令与结果）

各车道独立 TDD（红→绿）闭环，命令与结果取自车道回执；全仓门见 §5。

| 组（spec §5） | 实际命令 | 结果 |
|---|---|---|
| 1. semantic 台账 F1–F4 故障注入 | `cargo test -p nlos-task --test semantic_recovery_fault_matrix` | **5 passed / 0 failed**（4 故障行 + helper no-op），复跑 2 次稳定（提交 `b07c8fa`） |
| 2. 统一 worker 双域驱动 | `cargo test -p nlos-commit-coordinator --test unified_worker_dual_domain` | **7/7 PASS**（提交后终跑，提交 `7360698`；前序 `60517bb` 时为 2/2） |
| 3. 无 caller 重启收敛（核心） | 同上套件内 `worker_converges_pending_semantic_plan_without_caller` | **PASS**：prepare 后不做任何手动 converge，worker 自动把 pending semantic plan 收敛到唯一终态（plan `Finalized`、publications=1、task `head_commit_seq=1`、`semantic_total_finalized>=1`、artifact 侧全零、stop→`Stopped`） |
| 4. 台账损坏自愈 | `cargo test -p nlos-task --test semantic_recovery_ledger` 内 `ledger_row_loss_before_finalize_still_converges` | **PASS**（套件合计 **9 passed / 0 failed**，提交 `49fd9b4`；含 due scan/resume/escalation 钉死/告警面/汇总） |
| 5. 统一 receipt digest | `cargo test -p nlos-task --test unified_task_commit_receipt` | **4 passed / 0 failed**（提交 `4cfb91b`） |

车道级零回归（合并前各自隔离 worktree 实跑）：W26-001 末态 `cargo test -p nlos-task` → 41 test targets 全 ok / 0 failed；W26-002 `cargo test -p nlos-commit-coordinator` → 50/50（45 既有 + 5 新增）、`cargo test -p nlos-system-control` → 59/59（health 新字段的 6 处穷举字面量机械适配，提交 `467a213`，Ruling R5 写集扩展）；W26-003 `cargo test -p nlos-task` → 39 test targets 全 ok。

## 4. 故障矩阵 F1–F4 结果（`semantic_recovery_fault_matrix`，验证性矩阵、未暴露缺陷）

故障模型：单机本地 SQLite + `nlos-store-fault` VFS（`FAULT_LOCK` 串行、kill-9 子进程 + piped `READY` 管道同步、`wal` 语义），接线照抄同 crate `takeover_fault_injection.rs`/`fault_injection.rs` 既有范式；每行结尾 `PRAGMA integrity_check` = ok。

| 行 | 故障 | 结果 |
|---|---|---|
| F1 | kill-9 中断 record 事务（子进程 `BEGIN IMMEDIATE` 后插入逐位相同的台账行未提交即被强杀） | **PASS**：重开后台账/告警 0 行（完全回滚）、plan 保持 `Planned`、无台账行即回到期扫描（SEM-RECOV-005）、重放恰记一次（total=1 单行）、过期 expected 重放被 `SemanticRecoveryCasMismatch { 0, 1 }` 拒绝——无双记 |
| F2 | commit 后崩溃（公共 API 提交 record 后即杀） | **PASS**：单行逐位持久（全部字段钉死含 next_retry）、持久退避日程驱动扫描（未到期/到期两态）、同请求过期重放被 CAS 拒、按重读值续记 total=2 同行诚实推进 |
| F3 | IoErr（`FailWritesAfter { 0, IoErr }`） | **PASS**：record 以 `TaskStoreError::Sqlite` 显式失败且错误链含 i/o 条件、无半截状态（0 行、plan `Planned`、扫描照常）、disarm 后同请求成功且回读一致 |
| F4 | 静默丢写（`PowerLossAfter { 0 }`：报告成功但未落盘） | **PASS**：连接死亡重开后幻影行不可见（0 行、integrity ok）、自愈重扫发现 plan、redo 恰计一次（丢失写入不计入）、二次重开验证真持久；随后收敛唯一终态：plan `Finalized` + 台账同事务 `Resolved` + finalize 幂等重放返回原 receipt + 告警面/到期扫描为空 + 恒单行 |

按 Ruling R3（SDD 台账）该矩阵为验证性测试（非 TDD 违例），四行全部直接绿、未发现缺陷，如实记录。

## 5. 全仓验证门（W26-004，集成分支 `feat/unified-recovery-plane`，HEAD `2b51d28`）

工具链：`rustc 1.97.1 (8bab26f4f 2026-07-14)` / `cargo 1.97.1`，macOS 本地实跑。

| 命令 | 结果 |
|---|---|
| `cargo test --workspace --no-fail-fast` | **218 测试二进制 / 1177 passed / 0 failed / 11 ignored**，exit 0（对 W25 基线 213/1122/11：+5 二进制恰为 schema 2 / ledger 9 / fault_matrix 5 / dual_domain 7 / receipt 4 共 27 项新测试，+28 来自既有二进制内增补；零 flaky、零复跑） |
| `cargo fmt --check` | **通过（0 差异，exit 0）** |
| `cargo clippy --workspace --all-targets -- -D warnings` | **exit 0 / 0 error**（首跑恰 1 个 error：`effect.rs:1372` doc_markdown——经 `git diff 1a14ae0 HEAD -- effect.rs` 为空 + 行级字节核对证实为基线 `1a14ae0` 既有、非 W26 引入；一词加 backticks 最小修复为提交 `2b51d28`，修复后本门复跑 exit 0，`-p nlos-task` 复验 42 二进制 318 passed / 0 failed / 2 ignored） |
| 推送前五查 | 全过：`git log main..HEAD` 14 提交无夹带、工作区干净、敏感信息扫描零命中（W26 真实写集 `git diff 1a14ae0..2b51d28`：29 files，+5344/−187，零 `schema/`/`gen/`/`sdk/` 文件；T9 五查所记 31 files/−359 为对已前进 main（`4fed1d3` 追加 `/dag` 脚本）的 diff 口径，多出的 `scripts/render_dag.py −143` 系 diff 伪影，无 W26 提交触碰该文件）、fmt、全量 |

三车道经 3 个 chore 集成合并（`7ccbc43` 屏障 1 lane-a→lane-b、`7aa4c4c`/`9333784` 屏障 2 全量并入）零冲突。

## 6. 已知限制（known limitations，如实登记）

- **单机 H3**：全部证据为单节点本地 SQLite/VFS 故障模型；kill-9 模拟进程崩溃（页缓存存活，非断电），落盘丢失由 `PowerLossAfter` 覆盖；不外推跨 Cell、真实断电、远端 attestation（ADR-0013 跨机半边仍为 Stage C 扩展点）。
- **两台账退避分布不对称**（已登记）：Artifact 台账带 per-plan 确定性 jitter（±20%），Semantic 为纯函数 capped exponential（`base × 2^(n-1)` 封顶）；调用方/消费方不得假设两平面重试时刻相同（`worker.rs` 调用点注释钉死）。
- **escalation 阈值常量 vs config 不对称**（已登记）：Artifact 走 `config.failure_threshold`、Semantic 固定常量 8，同一 worker 配置下实际阈值可为 3 vs 8；仅由测试 `escalation_threshold_pinned_at_eight_and_backoff_saturates` 隐式钉死，未提升为配置。
- **health 面观测裂隙**（已登记）：健康面 authority 枚举无 Semantic 变体，Semantic 失败在 `last_failures` 中显示为 Coordinator（台账 `last_source` 正确记 `SemanticAuthority`，测试钉死）；semantic 失败条目 `plan_id = None`（`RecoveryWorkerFailure.plan_id` 为 artifact 时代类型，按 plan 身份的持久查询走 `inspect_semantic_recovery`）；`semantic_domain_faulted` 置位后 `semantic_consecutive_failed_cycles` 冻结在阈值不清零（钉死决策：保留置位证据，新实例归零）。
- **semantic 域对 system-control 运维面不可见**（终审补登，2026-09-13）：worker 的 9 个 semantic health 字段（`worker.rs:146-163`）在生产消费方中被丢弃——system-control 的 recovery metrics 全为 `nlos_artifact_recovery_*` 计数/gauge（`crates/nlos-system-control/src/lib.rs:111-155`），Acknowledge 控制命令固定走 `AcknowledgeArtifactRecoveryAlertCommand`（`crates/nlos-system-control/src/control.rs:387-447`），CLI 无 semantic 通道；因此 Semantic `Escalated` 后的人工 `resume_semantic_recovery` 无任何 metric/IPC/CLI 通道可达（仅进程内 Rust API），semantic 告警与恢复状态对运维面整体不可见。接线（SABI 契约扩展 + Rust/TS/Py 三语言 conformance）登记为 W27 首选车道。
- **complete TaskWriteSet 不因本波次晋升**：统一 receipt 是纯读侧聚合类型，不改变任何持久化事实；本波次不改变 spec §8 登记的完成度口径。
- **契约引用**：本切片兑现的是 [ADR-0013](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md) 的**有界收敛半边**（verify-then-commit 契约的本地恢复调度），不构成跨机原子提交或 compensation 执行。
- 其余登记在案的 deferred minors（节选）：`resume` 不重读 plan 终态——已 Finalized plan 的 Escalated 行 resume 会留惰性孤儿 Retrying 行（F 矩阵未新增缺陷证据）；v42 部分态守卫按表名计数不校验部件形状（与 v39 同海拔）；`list`/`acknowledge` 的 NotFound 无法区分台账行丢失与 plan 丢失；嵌套 receipt 字段翻转敏感性未测（三处 `encode_*` 单行回归可逃逸现有套件）；semantic 故障测试 fixture 跨 crate 复制约 215 行双份维护。

## 7. 未运行项（显式列出）

- **push 与 PR：未执行**——按控制器计划，W26 收尾 push 由控制器在本登记后统一执行（本 Evidence 写就时分支领先 main、未推送；注意 main 已在波次基点之后前进一个纯追加提交 `4fed1d3`（`/dag` 渲染脚本，零交集写集），合并/推送时属平凡非冲突增量）。
- **三平台 CI（"Rust cross-platform verification"）/ MSRV / Pages：未运行**——CI 待 push 后触发；unix/Windows 侧编译与执行以 CI 为准。
- **TS/Py conformance、schema 生成物检查：未运行**——本波次零 schema/proto 改动（`schema/`、`gen/` 不在写集），无触发面。
- 议题 35（TaskPlan/TaskNode 声明面）：未启动。
