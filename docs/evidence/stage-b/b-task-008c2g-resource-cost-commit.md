# B-TASK-008C2G：Task 侧 Resource cost-receipt 桥接（owner-derived 全量聚合）

- 状态：`PARTIAL_PASS`
- 日期：2026-08-24（增量验收，Attempt `TASK-RESOURCE-COMMIT-01`）
- 基线：HEAD `3661112`（= origin/main）；候选为工作区未提交变更。本 attempt 不提交、不 push、不更新 `stage-b-progress.md`；提交与进度表同步由后续单一 integrator 负责。
- 范围：单节点 `SqliteTaskAuthority` ↔ `ResourceAuthority` 的 verify-then-commit 桥接：resource-aware v3 finalize、schema v39 两张 STRICT immutable 子表、重启重放。**不是**跨 authority 原子提交，不是混合 Semantic+Resource 权威rung，不是 Resource 多维账本。

## 1. 结论（用户固定决策的落实）

用户在 2026-08-24 固定选择 **authority-first 全量聚合**：调用方不提供任何成本事实。`finalize_commit_v3_with_resource_authority(&ResourceAuthority, FinalizeRequestV3)` 与 `..._and_authority_lease` 变体从 permit 绑定的 sealed `TaskWriteSet.resource_reservations`（按 `ReservationId` 排序、拒绝重复）推导精确 Reservation 集，在 Task 事务打开**之前**逐项调用 committed `ResourceAuthority::inspect_cost_receipt`（B-RESOURCE-006，FINALIZED 门 + 七项绑定等式 + 全部有序 consumption 闭合 high-water），比对 reservation/account/quote/call/operation/upper_bound，然后在一个 Task 终结事务内：`insert_receipt` → 插入两张 nested 子表（父表每 `(task_receipt_id, reservation_id)` 一行 + 子表每 `(task_receipt_id, reservation_id, sequence)` 一行）→ permit close → head advance。请求/幂等身份就是既有 `FinalizeRequestV3` + `finalize_proof_digest`；草稿的 `ResourceCommitReceiptRequest`/`request_matches_receipts`（caller-selected receipts）已删除，任何 public/internal API 均不接受 activation/finalization ID、consumption sequence、usage 或 refund 数值。重放（permit Closed）走既有 replay proof-digest/history 校验后**只加载 Task nested 行**并与 sealed write set 本地比对，不再触碰 Resource owner（测试 5 以空 owner authority 证明）。空 Reservation 集的 write set 行为与 plain v3 完全一致（不插入任何行）。

## 2. 已实现事实

- `crates/nlos-task/src/resource_commit.rs`（重写草稿，358 纯 LOC）：`NestedResourceCostReceipt`（复用 public `nlos_resource::{ActivationReceipt, ConsumptionReceipt, FinalizationReceipt}` 值，Clone/Debug/Eq/PartialEq、非 Copy）、`ResourceTaskCommitReceipt{task_receipt, resource_cost_receipts}`、`ResourceFinalizeDecision::{Committed,Replayed}` + `receipt()` 访问器；`verify_owner_cost_receipts`（owner 回读 + 七项比对；**不**调用要求 RESERVED 状态的 `validate_resource_reservation_bindings`）、`insert_resource_cost_receipts`（父+子同一事务）、`load_resource_cost_receipts`（子行按 sequence 排序；fail-close：末条 `(sequence,cumulative_usage)` 必须等于父行 `(high_water_seq,high_water)`、空子集蕴含 `(0,0)`、cumulative 单调不回退、`final_seq >= high_water_seq`、`upper_bound − final_usage == refund_credit`（checked_sub，无 `as` 收窄）、绑定身份重建）、公共 `inspect_resource_cost_receipts(task_id, receipt_id)`（legacy receipt 解码为空集）。
- schema v39（`migrations.rs` 的 `SCHEMA_V39_SQL`，与其他 `SCHEMA_VN_SQL` 同址）：STRICT 父表 `task_resource_cost_receipts`（18 列全量 owner 聚合，PK `(task_receipt_id, reservation_id)`，FK→`task_receipts`/`tasks`，索引 `by_task`）+ STRICT 子表 `task_resource_cost_consumptions`（PK `(task_receipt_id, reservation_id, sequence)`，复合 FK→父表），各配 UPDATE/DELETE immutable 触发器（共 4 个）；全部 u64 用 8 字节大端 BLOB + checked helpers，无整数收窄。`migrate_v39` 幂等预检（2 表 + 4 触发器 = 6 完整；部分 schema → `CorruptRecord` fail-closed），纯增量、无回填；`SCHEMA_VERSION = 39`。
- `reconcile.rs` 最小线程化：`FinalizeImplResult::Resource` 新变体；`finalize_impl_inner` 增加 `resource_receipts: Option<&[...]>`（类比 `semantic_plan_id`）；Issued 路径在事务内重新加载 sealed write set 并对已验证聚合做 fail-closed 本地比对；Closed 路径只从 Task 行加载 nested 并比对；`write_commit_receipt` 增加 `Option<&[...]>` 参数，nested 行在 `insert_receipt` 之后、`close_permit` 之前写入；`semantic && resource` 组合rung fail-closed（`CorruptRecord`，记录为缺口）；lease parity 通过 `..._and_authority_lease` 保留。
- `lib.rs`：`mod resource_commit` + 导出三个类型；crate 文档补记 v39 语义与边界。
- 附带必要变更：7 个既有测试文件中 8 处 `assert_eq!(version, 38)` → `39`（`artifact_commit_plan.rs`×3、`authority_lease.rs`、`barrier_signature.rs`、`effect_history.rs`、`effect_permit.rs`、`takeover_completion.rs`、`task_group.rs`×2）——这些断言固定“迁移 stamped 当前 schema 版本”，前一版本 bump（v38，commit `b35fe30`）同样更新过它们；未删除/未改写任何既有测试语义。

## 3. Evidence（命令与结果）

- TDD RED：先写 `crates/nlos-task/tests/resource_commit.rs`（6 个 Given/When/Then 测试），`cargo test -p nlos-task --test resource_commit` → 编译失败 `error[E0433]: cannot find resource_commit in crate`（resource-aware 模块/API 不存在，正确原因）。
- TDD GREEN：`cargo test -p nlos-task --test resource_commit` → **6 通过 / 0 失败 / 0 忽略**：
  1. `resource_aware_finalize_commits_full_owner_aggregate_for_multiple_consumptions`：双 Reservation（R1 upper 100，consume seq1=30/seq2=37，finalize 37 → refund 63；R2 upper 25，consume seq1=10，finalize 10 → refund 15）；`Committed`，nested 与 `inspect_cost_receipt` 逐字段相等，守恒 `upper_bound − final_usage == refund_credit`，head +1，permit Closed，两表 UPDATE/DELETE 触发器拒绝。
  2. `zero_consumption_closure_commits_with_and_without_final_usage_and_lease_parity`：lease-bound permit；缺 lease → `AuthorityLeaseRequired` 且 permit 保持 Issued；带 lease → `Committed`；零消费 final_usage=0（refund=50）与零消费 final_usage=5（owner 允许的合法非零终值，refund=35）均成立，子表 0 行。
  3. `non_finalized_owner_reservation_fails_closed_without_task_mutation`：Reservation 仅 activate 未 finalize → `Err(ResourceParticipantAuthority(_))`（typed），permit Issued、`task_receipts` 0 行、父表 0 行。
  4. `sealed_binding_mismatch_fails_closed_without_terminal_mutation`：跨 authority 同 `ReservationId`（同 key+operation、不同 account/quote/upper）已 FINALIZED → `Err(TaskWriteSetResourceReservationConflict)`，无任何 Task 终结变更。
  5. `replay_after_restart_reads_only_task_rows_and_duplicates_nothing`：重启 Task authority 后以**全新空 ResourceAuthority** 重放同一请求 → `Replayed` 逐字节相等（证明 replay 不读 owner），父表 1 行/子表 2 行无重复。
  6. `v38_database_migrates_to_v39_and_preserves_legacy_receipts`：v38 库（手工 DROP 两 v39 表 + `user_version=38`）重开 → `user_version=39`；既有 Task receipt 可读、nested 为空集；合成行 UPDATE/DELETE 被两表触发器拒绝。
- `cargo check -p nlos-task`：通过。
- `cargo test -p nlos-task --quiet`：**186 通过 / 0 失败 / 0 忽略**（22 个 test 二进制）。
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings`：通过（0 warning）。
- `cargo fmt --all -- --check`：通过；`git diff --check`：通过。
- LSP 诊断：本机 LSP daemon/rust-analyzer 不可用（`Unknown binary 'rust-analyzer'`）；以 `cargo check` + `cargo clippy -D warnings`（更强的编译器诊断门）替代，二者均通过。
- 本地 macOS/arm64；无 CI 结果。

## 4. 明确限制与缺口

- **无跨 authority 原子性**：owner 读在 Task 事务之外（verify-then-commit）；Task CAS 期间 owner 状态理论上可漂移（owner FINALIZED 后不可再变，风险限于读取瞬间与事务提交之间的窗口），两 authority 仍是两个事务域，不声称分布式提交。
- **无混合 Semantic + Resource 权威rung**：`finalize_impl_inner` 对 `semantic_plan_id && resource_receipts` fail-closed（`CorruptRecord`）；组合 API 为后续独立门。
- **无 kill-window 故障矩阵 / 真实断电验证**：未做 F1–F6 类故障注入；嵌套行插入依赖同事务原子性推演，未经真实 power-loss 验证。
- 跨路径重放边界：若 permit 携带 Reservation 的 write set 经 plain v3（非 resource-aware）终结，则 nested 行为空，之后 resource-aware replay 以 `CorruptRecord` fail-closed（不静默降级、不补造证据）。
- effect-closed proof digest 仍为 owner 侧 caller-asserted opaque 摘要（沿 B-RESOURCE-005/006 限制）；无 endpoint/enforcement-gateway 签名。
- 单机 strict reference profile；`resource_commit.rs` 358 纯 LOC 超出通用 250 行天花板（沿本 crate authority-bridge 模块惯例，如 `semantic_commit.rs` 1460 行，且任务固定写集不允许新增模块文件）；既有测试仅更新 schema 版本常量断言。
- 无本 attempt CI 结果；不据此外推 DONE 或 H4+；`stage-b-progress.md` 未由本 attempt 更新。
