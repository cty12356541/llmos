# B-TASK-008C2G-RES-PREPARE-FINALIZE：Resource 跨 authority prepare/finalize 有界 coordinator（ADR-0017 决定 R-C，车道 W28-C）

状态：`PARTIAL_PASS`（2026-09-20，W28-C-1/C-2/C-4 落地；W28-C-3 worker 接线与 G8 运维面按写集边界递延，见 §6）

> 对应：[ADR-0017](../../management/adrs/0017-resource-operation-cross-authority-prepare-finalize.md)（决定 1 R-C + 附录 A 车道表 W28-C-1..5 与验收门 G1–G7）、[B-TASK-008C2G-RES-COMMIT](b-task-008c2g-resource-cost-commit.md)（resource-aware v3 单事务路径 + W1–W6 桥接矩阵，本车道原样复用并扩展）、[B-TASK-008C2G-UNIFIED-RECOVERY](b-task-008c2g-unified-recovery.md)（v42 台账/plan 模式，逐条镜像）、[B-RESOURCE-005](b-resource-005-finalize-refund.md)/[B-RESOURCE-006](b-resource-006-cost-receipt-aggregate.md)（owner FINALIZED 门与 `inspect_cost_receipt` 聚合）
>
> 实现：`nlos-task` schema v43（`migrations.rs`）/ plan 状态机 + envelope + 扫描/收敛 API（`resource_commit.rs`）/ 恢复台账第三域（`recovery.rs`）/ plan 线程化终结事务（`reconcile.rs`）；测试 `tests/resource_prepare_finalize.rs`、`tests/resource_recovery_ledger.rs`、`tests/resource_recovery_schema.rs`、`tests/resource_bridge_fault_injection.rs`（WE1–WE3 扩展）
>
> 基线：HEAD `6c7a404`（分支 `feat/w28-c`）；nlos-commit-coordinator 与根 Cargo.toml 不在本轮写集，未被触碰（`cargo check -p nlos-commit-coordinator -p nlos-system-control` 通过，零编译面破坏）

## 1. 本切片目标

把 ADR-0017 决定 1（R-C 有界 coordinator）落进 Task 侧：终结请求身份（`FinalizeRequestV3` 的不可再推导字节：`required_satisfaction` + `fenced_participant_digest`）与从 sealed write set 推导的 Reservation 集事实持久化为 **immutable envelope + plan 状态机**（schema v43 additive 表组，镜像 v26 envelope / v25 plan 模式）；`converge_resource_commit_plan` 重启扫描 incomplete plan：owner 全部 FINALIZED（`inspect_cost_receipt` FINALIZED 门）⇒ 经既有 resource-aware v3 单事务路径收敛/幂等重放（plan 翻转与台账 resolve 骑同一终结事务）；任一 owner Reservation 未结算（Reserved/Active/Quarantined/NotFound）⇒ **not-due：零 owner 变更、零台账失败记录**，plan 保持可被运维面 inspect 的 durable fact。owner 侧 finalize/refund/quarantine 仍归调用方/未来 enforcement-gateway（ADR-0017 约束 4，不凭空造 effect-closed 证明）。

## 2. 语义契约逐条对应（ADR-0017 附录 A 门 → 实现位置）

| 门 | 实现位置 | 测试锚点 |
|---|---|---|
| G1 envelope/plan 不变量：exact replay、immutable 触发器、冲突 typed fail-closed、迁移幂等 + partial fail-closed | `SCHEMA_V43_SQL`（plan 身份不可变触发器 + no-delete、envelope/satisfaction 双不可变触发器）；`prepare_resource_finalize`（逐字节 replay / `InvalidResourcePlan` 冲突拒绝，零终结变更）；`migrate_v43`（14 命名部件存在性守卫，`v42 [SEM-RECOV-006]` 模式） | `prepare_persists_envelope_and_plan_and_exact_replays`、`prepare_fails_closed_for_non_resource_write_sets_and_non_issued_permits`、`schema_v43_creates_resource_coordinator_tables_idempotently`、`partial_resource_coordinator_schema_fails_closed_on_reopen` |
| G2 既有 verify-then-commit 语义零回归：v3/mixed 全部既有测试逐位不变；permit Closed 重放只读 Task 行；envelope 只增不改终结事务结构 | `reconcile.rs` 仅给 `finalize_impl_inner` 增 `resource_plan_id: Option<_>` 参数（既有 6 个包装点全部传 `None`）；终结事务仍为 `insert_receipt → nested → close → head` 单事务，plan 翻转 + `resolve_resource_recovery` 追加在 head 推进之后同事务内（`bind_resource_plan_receipt`） | `resource_commit` 6/6、`mixed_semantic_resource_commit` 7/7、`resource_bridge_fault_injection` W1–W6 11/11 照绿；replay 只读 Task 行由 WE2 Phase B（空 owner converge 重放）与 `replay_after_restart_reads_only_task_rows` 双向证明 |
| G3 无 caller 收敛（核心门，nlos-task 半边）：prepare → owner 逐项 finalize → 丢弃重开 → 仅凭 durable plan/envelope + owner 读收敛唯一终态 | `converge_resource_commit_plan`（plan+envelope 重建 `FinalizeRequestV3`；owner 结算门 ⇒ `verify_owner_cost_receipts` ⇒ `finalize_impl_with_resource_plan`）；`list_incomplete_resource_commit_plans` 重启扫描 | `converge_pending_after_restart_reaches_unique_terminal_state_without_caller`（plan FINALIZED、恰一套嵌套行、head +1、显式 replay 逐字节相等、扫描清空）；worker 周期驱动半边属 W28-C-3（nlos-commit-coordinator，本轮写集外，见 §6） |
| G4 not-due 负向门：owner 未结算 ⇒ converge 对 owner 零变更、plan 不记失败/不退避/不 escalate；stuck plan 可 inspect | `owner_settlement`（逐 Reservation `inspect_reservation` 只读；Reserved/Active/Quarantined/`ReservationNotFound` ⇒ `NotDue` 决策而非错误；其余 owner 读失败 ⇒ `ResourceParticipantAuthority` infra 错误供 worker 记账）；converge not-due 路径零 SQL 写 | `converge_not_due_leaves_owner_untouched_and_records_nothing`（ReservationRecord/AccountRecord 前后逐字段相等 + 台账 0 行 + plan Planned + 扫描仍返回）、`envelope_window_crash_after_prepare_before_owner_finalize_keeps_g4_semantics`（WE1 崩溃窗口后同样成立）、`not_due_converge_records_no_failure_and_due_converge_resolves_ledger` |
| G5 台账镜像：`SEM-RECOV-001..007` 逐条镜像（CAS 单增、due scan 过滤、Escalated 显式 resume、finalize 置 Resolved、infra 失败入退避、迁移幂等、告警 acknowledge 面） | `recovery.rs` resource 域（`task_resource_recovery` + alert 表 = v42 列级镜像，FK 指向 `task_resource_commit_plans`）；`RESOURCE_ESCALATION_THRESHOLD = 8` 与退避公式钉死为 v42 同款（L-B，不新增配置面） | `resource_recovery_ledger` 8/8（CAS/due-scan/resume/escalation 钉死/summarize/告警幂等回执/收敛 resolve/丢行自愈）+ `resource_recovery_schema` 2/2；F1–F4 故障注入矩阵行未在本轮建（worker 半边递延，见 §6，丢行自愈已有语义级测试） |
| G7 桥接 kill-window 扩展矩阵：envelope 边界两窗口 + PowerLossAfter 双向 + torn WAL tail | `resource_bridge_fault_injection.rs` WE1/WE2/WE3（新增 kill-9 子场景 `resource-envelope-prepare`/`resource-envelope-converge`） | 见 §4 矩阵表，每行 `PRAGMA integrity_check` = ok |
| G8 运维面 + 全量门 | resource 域 Escalated/resume 的 IPC/CLI 通道接线属 W27-A 引入通道家族的扩展，`nlos-system-control` 不在本轮写集 ⇒ **递延**（命名债登记，见 §6）；`cargo test --workspace` 由波次屏障执行 | 本轮以 `cargo test -p nlos-task`（串行位 1 crate）+ fmt/clippy 双 0 + 依赖 crate `cargo check` 为门 |

## 3. 已实现事实

- **schema v43**（`SCHEMA_V43_SQL`，一次 `BEGIN IMMEDIATE` additive 落地，幂等 + partial fail-closed）：`task_resource_commit_plans`（plan 状态机：`plan_state IN (0,1)`、`CHECK((plan_state=1)=(task_receipt_id IS NOT NULL))`、身份不可变触发器 + no-delete；列镜像 v25 semantic plan，以 `resource_reservation_set_root`/`expected_reservation_count` 承载从 sealed write set 推导的 Reservation 集事实）+ `task_resource_finalize_envelopes`/`task_resource_finalize_satisfactions`（镜像 v26：plan_id PK + fenced digest + satisfactions 表，四触发器）+ `task_resource_recovery`/`task_resource_recovery_due`/`task_resource_recovery_alert_receipts`（v42 列级镜像 + 双触发器）——14 命名部件。
- **`prepare_resource_finalize`**（`resource_commit.rs`）：单事务持久化 plan（Planned）+ envelope；exact retry 逐字节 `Replayed`；冲突请求 `InvalidResourcePlan` typed fail-closed（零 Task 终结变更）；拒绝非 Issued permit/非持有者/陈旧 head/带 Semantic appends 的 write set（组合 rung 留在直连 API）/空 Reservation 集/无 slot 却带 satisfaction；slot 形状校验复用 `validate_finalize_satisfaction_shape`（`semantic_commit.rs` 改 `pub(crate)`，零行为变化）。plan_id = `SHA-256("llmos/task-resource-commit-plan/v1" ‖ permit_id)` 前 16 字节（镜像 semantic 派生域）。
- **`converge_resource_commit_plan`**：`NotDue(Box<plan>)` / `Finalized` / `Replayed` 三值决策；due 门 = 逐 sealed Reservation `inspect_reservation` 只读（非 FINALIZED 或 NotFound ⇒ NotDue；其余 owner 错误 ⇒ `ResourceParticipantAuthority` infra）；due ⇒ `verify_owner_cost_receipts`（FINALIZED 门 + 七项绑定比对，与既有 v3 路径完全同源）⇒ plan 线程化终结；plan 已 FINALIZED ⇒ 纯 Task 行重放（不触碰 owner，WE2 Phase B 以空 owner 证明）。
- **`reconcile.rs` plan 线程化**：`finalize_impl_inner` 增 `resource_plan_id` 参数；Issued→Committed 路径在 `write_commit_receipt`（receipt→nested→close→head）之后、同一事务内 `bind_resource_plan_receipt`（Planned→Finalized CAS 绑 receipt）+ `resolve_resource_recovery`；Closed 重放路径在嵌套行 fail-closed 比对之后同样绑定/幂等校验（已 Finalized 且同 receipt ⇒ no-op，不同 receipt ⇒ `CorruptRecord`）——**plan 翻转与嵌套行/回执/permit 关闭/head 推进同生同灭**（WE3 证明）。permit 先被无 plan 直连 v3 终结的窗口由重放路径收敛（`converge_flips_plan_when_permit_was_finalized_directly`）。
- **`recovery.rs` 第三域**：`record/inspect/summarize/list_alerts/acknowledge/list_due/resume/resolve(_resource_recovery)` 逐条镜像 semantic 家族（零参 list、`Acknowledged/Replayed` 命名沿 semantic、`ResourceCommitPlanNotFound` 复用为台账行缺失语义、escalation 阈值常量 8、退避 `base×2^(n-1)` 封顶无 jitter）；告警回执派生域 `llmos/task-resource-recovery-alert-ack/v1`。
- **`lib.rs`**：`TaskStoreError` 新增 6 变体（`ResourceCommitPlanNotFound`/`InvalidResourcePlan`/`InvalidResourceRecoveryPolicy`/`ResourceRecoveryCasMismatch`/`InvalidResourceRecoveryState` + Display）与 16 个新公共类型导出；既有变体零改动。
- **既有测试机械适配**：10 处「迁移 stamped 当前版本」断言 42→43（`artifact_commit_plan`×3、`authority_lease`、`barrier_signature`、`channel_endpoint`、`effect_fiber_registration`、`effect_history`、`effect_permit`、`resource_commit`、`task_group`、`semantic_recovery_schema` 首测——沿 v39 bump 先例，v42 partial 测试的 41/42 语义断言原样保留）。

## 4. 桥接 kill-window 扩展矩阵（G7，`resource_bridge_fault_injection.rs` WE1–WE3）

故障模型与 W1–W6 同源：单机本地 SQLite + `nlos-store-fault` VFS（`PowerLossAfter` 进程级丢写、kill-9 子进程 + piped `READY` 同步、WAL 尾部截断、`FAULT_LOCK` 串行），全部注入指向 Task authority 连接，owner 连接走普通 VFS；矩阵证明 **verify-then-commit + 幂等收敛**，不是跨 authority 原子性。每行结尾 `PRAGMA integrity_check` = ok。

| 窗口 | 测试 | 预期（沿 W1–W6 断言式样） | 实际 |
|---|---|---|---|
| WE1 prepare 后、owner finalize 前崩溃（窗口 i） | `envelope_window_crash_after_prepare_before_owner_finalize_keeps_g4_semantics` | 重开后 plan 仍 `Planned`、envelope 存活、permit Issued/head 0；G4 语义成立（converge ⇒ `NotDue`、owner ReservationRecord 逐字段不变、台账 0 行）；owner 逐项结算后同请求继续推进 → `Finalized`（聚合完整、plan FINALIZED 绑 receipt）→ 重放逐字节相等、恰一套嵌套行 | 一致（PASS） |
| WE2 owner 全部 FINALIZED 后、Task finalize 前崩溃（窗口 ii，PowerLossAfter 双向） | `envelope_window_power_loss_after_owner_finalized_before_task_finalize_converges_both_ways` | Phase A：converge 报告成功但重开后**完全不可见**（plan 翻转+嵌套行+回执+permit 关闭+head 推进一起消失、台账 0 行）；同请求重做 `Finalized` 与幻影决策逐字节相等（确定性 `derive_commit_receipt_id`）。Phase B（kill-9 after commit）：**完全可见**（plan FINALIZED、恰 1 回执/2 父行/3 子行）；以**空 ResourceAuthority** converge 重放 ⇒ `Replayed` 逐字节相等、零重复（证明重放不读 owner）。两方向绝无部分可见 | 一致（PASS） |
| WE3 torn WAL tail（窗口 ii 变体） | `envelope_window_torn_wal_tail_discards_plan_flip_with_bridge_together` | 子进程提交完整 converge 后被杀，父进程截断最后 commit 帧一半 ⇒ 终结事务整体隐藏（plan 回到 `Planned`、四终结表 0 行）而已提交前缀（task/attempt/write-set/permit/envelope）保留；同请求重做 `Finalized`、聚合完整；重放逐字节相等、恰一套嵌套行——plan 翻转与桥接同生同灭的直接证明 | 一致（PASS） |

## 5. Evidence（命令与结果）

工具链：`rustc 1.97.1 (8bab26f4f 2026-07-14)` / `cargo 1.97.1`，macOS/arm64 本地实跑（worktree `llmos-w28-c`，分支 `feat/w28-c`，基线 `6c7a404` 工作区 clean 已验证）。

- TDD RED：先写 `tests/resource_prepare_finalize.rs` + `tests/resource_recovery_schema.rs`，`cargo test -p nlos-task --test resource_prepare_finalize` → 编译失败 27 个错误且全部指向缺失 API（`E0599 no method named prepare_resource_finalize/converge_resource_commit_plan/...`、`E0432 unresolved import PrepareResourceFinalizeRequest`、`E0599 no variant InvalidResourcePlan`——正确原因）；实现后转绿。
- 新增测试单列：`cargo test -p nlos-task --test resource_prepare_finalize` → **5/5**；`--test resource_recovery_schema` → **2/2**；`--test resource_recovery_ledger` → **8/8**；`--test resource_bridge_fault_injection` → **14/14**（= 既有 W1–W6 11 + WE1–WE3 3），复跑 2 次稳定。
- 零回归：`cargo test -p nlos-task` → **45 test targets / 340 passed / 0 failed / 0 ignored**（基线 `6c7a404` 为 322 → +18 恰为 5+2+8+3 新测试；`resource_commit` 6/6、`mixed_semantic_resource_commit` 7/7、`semantic_recovery_ledger` 9/9、`semantic_recovery_fault_matrix` 5/5、`unified_task_commit_receipt` 4/4 逐一照绿）。
- `cargo check -p nlos-task` 通过；`cargo check -p nlos-commit-coordinator -p nlos-system-control` 通过（依赖 crate 编译面零破坏）。
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings` → **exit 0 / 0 error**（首跑 3 处：deprecated 梯子调用 ×2 + fixture 114 行 → 按既有测试文件先例 `#![allow(deprecated)]`/`#[allow(clippy::too_many_lines)]`；生产代码 1 处 `prepare_resource_finalize` 113 行 → 抽出 `prepare_new_resource_finalize` 助手消除，无 allow）。
- `cargo fmt -p nlos-task -- --check` 通过；`git diff --check` 通过。
- LSP 诊断：daemon 请求超时（与 W26/W27 各增量同状）；以 `cargo check` + `clippy -D warnings` 替代并记录，二者均通过。
- 新增代码 0 `unsafe`、0 生产 `unwrap/expect`、无 `as` 数值收窄（u64 一律 8 字节大端 BLOB + checked helpers）、无新增告警抑制（2 处 by-value `needless_pass_by_value` 沿本 crate finalize 家族既有惯例）。

## 6. 已知限制与 deferred minors（如实登记）

- **W28-C-3 worker 第三域驱动未接线（G3 worker 半边 + G6 三域隔离）**：`resource_cycle`（artifact→semantic→resource 顺序）与 `RecoveryWorkerHealth` resource 域字段属 `nlos-commit-coordinator` 写集，本轮按写集纪律未触碰。nlos-task 半边已就绪：`list_due_resource_commit_plans`（退避/Escalated 过滤 + 无台账即到期）、`record_resource_recovery_failure`（CAS/退避/escalation）、`converge_resource_commit_plan`（NotDue/infra 错误二分，worker 据此只对 infra 失败记账）——接线车道可直接消费。
- **G8 运维面递延（命名债）**：resource 域 Escalated 告警/resume 尚无 IPC/CLI 通道（W27-A 通道家族在 `nlos-system-control`，本轮写集外）；进程内 Rust API（`list_resource_recovery_alerts`/`acknowledge_resource_recovery_alert`/`resume_resource_recovery`）已可用。ADR-0017 运维责任条款要求 W28-C 硬门可达——按本轮写集限制登记为递延项，随接线车道补齐，不构成「对运维面整体不可见」的静默缺陷（本条即显式登记）。
- **台账 F1–F4 故障注入矩阵行未建**：v42 语义级镜像已由 8 项台账测试 + 丢行自愈覆盖；kill-9/IoErr/PowerLossAfter 对 `task_resource_recovery` 表自身的注入随 worker 接线车道补（沿 `semantic_recovery_fault_matrix` 模板）。
- **组合 rung（Semantic+Resource）不经 plan coordinator**：`prepare_resource_finalize` 对带 Semantic appends 的 write set typed fail-closed——混合 rung 的 Task 侧恢复由 semantic plan/envelope 机制拥有（`FinalizeSpec` 组合面已可用）；按 ADR 决定 1「既有 resource-aware v3 单事务路径」的单路径裁定，不扩混合面。
- **lease-bound permit 不自动收敛**：converge 不携带 `AuthorityLeaseRecord`（镜像 semantic coordinator `finalize_ready` 同界）；lease-bound 资源 plan 的 converge 会得到 typed `AuthorityLeaseRequired`，作为 worker 可记账的失败面。候选后续：converge 变体接受 lease 或从 durable lease 表重读。
- **not-due 的 NotFound 语义**：owner 不认识某 sealed Reservation（`ReservationNotFound`）判 NotDue 而非 infra 错误——「本 owner 上未结算」的诚实读法；误接线 authority 会表现为永久 stuck plan（可 inspect），由运维面/Gateway 通道处置。
- 单机 strict reference profile；kill-9 = 进程崩溃（页缓存存活），落盘丢失由 `PowerLossAfter`/WAL 截断建模，不外推真实断电与跨 Cell（沿 W1–W6 disclaimer）。
- `resource_commit.rs` 生产模块行数超 250 行天花板（沿 §4 既载 authority-bridge SIZE 例外与本 crate `semantic_commit.rs` 1465 行先例；任务写集限定既有模块文件）。

## 7. 未运行项（显式列出）

- **push / PR / 三平台 CI / MSRV / Pages：未执行**——按波次屏障由控制器统一收尾。
- **`cargo test --workspace`：未整跑**——车道纪律限定定向门（本轮写集仅 nlos-task + docs；依赖面以 `cargo check -p nlos-commit-coordinator -p nlos-system-control` 覆盖编译完整性）。
- **stage-b-progress.md / ADR-0017 附录勾稽 / evidence 索引：未更新**——本轮 MUST NOT 边界，由 W28-C-5 证据落档车道或控制器屏障统一执行。
- nlos-resource 生产代码零改动（owner 侧无新 API 需求，`inspect_reservation` 只读复用），`cargo test -p nlos-resource` 未跑（未触碰）。
