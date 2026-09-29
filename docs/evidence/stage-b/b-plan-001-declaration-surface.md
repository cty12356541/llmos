# B-PLAN-001：nlos-plan 声明面状态权威骨架（TaskPlan/TaskNode state face）+ Dependency Resolver + Context Residency 分级 + 10K/100K 逻辑 TaskNode benchmark + 惰性物化门 + 分层 Scheduler 最小版 + 生态 selector 半边与 G3 三条件结构化

状态：`PARTIAL_PASS`（**W28-A 状态面骨架 + W29-B Dependency Resolver + W31-E Context residency 分级最小版 + W31-D 10K/100K 逻辑 TaskNode benchmark + W31-A 惰性物化门（G3）+ W31-F 分层 Scheduler 最小版 + W36-P7 G4 生态 selector 半边 + G3 三条件结构化 + W36-P8 apply consult / PINNED / 调度器探针**，2026-09-22）

> 对应：[ADR-0016 决定 2](../../management/adrs/0016-task-plan-declaration-surface.md)（独立 `nlos-plan` authority）与 [决定 5](../../management/adrs/0016-task-plan-declaration-surface.md)（Resolver 结果 durable）与 [决定 4](../../management/adrs/0016-task-plan-declaration-surface.md)（ScaleProfile 维度正规化）；[议题 35 §6](../../discussions/35-TaskPlan声明面设计.md) 验收门 G1（§2–§6，W28-A）、G4（§7，W29-B）、G2/G5（§9，W31-D）与 G3（§10，W31-A）；[进度单 §6.5.3](../../management/stage-b-progress.md) W28-A / W29-B / W31-E / W31-D / W31-A / W31-F 车道行
>
> 实现：crate `crates/nlos-plan`（schema v1：`plans` / `plan_revisions` / `plan_nodes` / `plan_node_transitions` 四表 + 12 trigger；schema v2 additive：`plan_revision_nodes` / `plan_revision_edges` / `plan_resolution_receipts` 三表 + 8 trigger；schema v3 additive：`plan_node_residency_transitions` 一表 + `plan_nodes` 两列 + 4 trigger；schema v4 additive：`plan_materialization_requests` 一表 + 1 partial unique index + 5 trigger；schema v5 additive：`ecosystem_resolution_receipts` 一表 + 2 写一次 trigger；schema v6 additive：`plan_revision_nodes.conditions_body` 可空列）+ `tests/tasknode_scale_probe.rs`（§9 规模探针）+ `tests/materialization_gate.rs` / `tests/materialization_fault_matrix.rs`（§10 G3 证伪与物化门故障矩阵）+ `crates/nlos-task/src/materialization.rs`（§10 Task 侧消费接线）+ `src/scheduler.rs` / `tests/scheduler.rs`（§11 W31-F 两层调度器）+ `src/selector.rs` / `src/artifact_source.rs`（特性门）/ `tests/ecosystem_selector.rs`（§12 前半）+ `model.rs` `NodeConditions` 三件套 / `tests/structured_conditions.rs`（§12 后半）+ `crates/nlos-application/src/selector_source.rs`（§12 Application 薄扩展适配器）
>
> 范围纪律：W28-A 只落**状态权威落点**（§1–§6）；W29-B 只落 **Dependency Resolver**（§7，B4-3）；W31-E 只落 **Context residency 分级最小版**（§8，B4-5）；W31-D 只落 **10K/100K 逻辑 TaskNode benchmark**（§9，B4-9，G2/G5 后半）；W31-A 只落**惰性物化门**（§10，B4-4，G3——request/resolve 门 + Task 侧 admission consult 接线 + 存储层 MATERIALIZING 边门禁）；W31-F 只落**分层 Scheduler 最小版**（§11，B4-6——Global/Worker 两层 + 物化窗口调度 + 决策 inspect）；W36-P7 只落**生态 selector 半边 + G3 三条件结构化**（§12，C-SELECTOR 移交#7——typed selector→generation handle 解析负路径面 + Namespace/ResourceContract/fanout typed 声明面；G3 三条件的**enforcement**与节点声明对 resolution handle 的结构化绑定不在本 lane）。manifest 模板面（W28-B）与 TaskSpec 关联字段（W29-A 已落）不在本 evidence 声明范围。

## 1. 本切片目标

按 ADR-0016 决定 2 落地 TaskPlan/TaskNode 的 durable 状态权威骨架，镜像 clock/topic/wait/application/nlos-task 既成 authority 模式：

1. **G1 证伪测试先行（红→绿）**：已执行节点的 revision/digest 不存在任何改写路径（`[PLAN-DAG-001]` 末句，规范行 3648）；
2. **重启 replay + 幂等键**：任意两个 durable effect 之间崩溃 → replay 收敛到同一状态，无双重应用；
3. **kill-window 精简故障矩阵（F1–F4）**：单机 SQLite + `nlos-store-fault` VFS，每行 `integrity_check` ok + 收敛。

## 2. 实现事实

### 2.1 Schema v1（`crates/nlos-plan/src/schema.rs`，单事务 `BEGIN IMMEDIATE` + 部分态守卫）

| 表 | 角色 | 关键不变量（DDL trigger/约束承载） |
|---|---|---|
| `plans` | 每 plan 的 current-state 头 | `current_revision` 单调不减；身份冻结；禁止 DELETE |
| `plan_revisions` | **immutable digest 链**（每 revision 一行回执） | 禁止 UPDATE/DELETE；`plan_revisions_chain_link` AFTER INSERT 守卫：revision 必须稠密衔接（= 头当前 revision）且 `parent_revision_digest` 必须等于前一 revision 的 `plan_digest`（revision 1 必须为 NULL） |
| `plan_nodes` | 每 TaskNode 有界 durable metadata（`[SCALE-LOGICAL-001]` 姿态） | 身份列冻结；禁止 DELETE；`plan_nodes_executed_shape_frozen`：`node_state >= MATERIALIZING` 后任何 `declared_revision`/`node_digest`/`node_kind` 变更 ABORT（G1 存储层守卫） |
| `plan_node_transitions` | **状态机迁移凭证**（§25.2.1） | 禁止 UPDATE/DELETE；`plan_node_transitions_seq_bound`：voucher 必须是该节点第 N 条稠密序列 |

所有表 STRICT；blob 宽度 CHECK（16/32）；`user_version = 1`；未知版本 fail-closed（`SchemaVersionUnsupported`）。打开路径逐位镜像 `nlos-task`：WAL/FULL 回读校验（静默回退拒绝）、foreign_keys、5s busy_timeout、进程内 mutex 之后的 `BEGIN IMMEDIATE` 写者栅栏、`open_with_vfs` 供故障注入。

### 2.2 域分隔 ID 派生（nlos-types nominal ID 模式，authority-assigned）

| 域 | 公式 | 输入 |
|---|---|---|
| `TaskPlanId` | `H("llmos/plan/plan-id/v1")` 截 16B | revision-1 幂等键（调用者永不自报 plan id） |
| `TaskNodeId` | `H("llmos/plan/task-node-id/v1")` 截 16B | `(plan_id, node_key)`——同 `node_key` 跨 revision 稳定寻址同一逻辑节点 |
| `node_digest` | `H("llmos/plan/node-digest/v1")` | kind + binding + 排序依赖键集 + selector/contract/policy/ceiling digest（不含 plan_id：同形跨 revision 必须逐位同哈希，G1 比较的根基） |
| `nodes_root` / `dependencies_root` | 各自域前缀 | 按 node_id / (from,to) 排序的确定性编码 + revision |
| `plan_digest`（链环节） | `H("llmos/plan/revision-digest/v1")` | `plan_id ‖ revision ‖ parent(0x00/0x01 前缀) ‖ nodes_root ‖ dependencies_root`（公式钉死，域版本即兼容边界） |
| `voucher_id` | `H("llmos/plan/transition-voucher-id/v1")` 截 16B | `(幂等键, node_id, to_state)` |

依赖边以声明内 `node_key` 结构化承载（`[PLAN-DAG-001]` 显式边）；selector/contract/policy/资源上界以 digest 绑定（其解析模型属 W28-B/W29-B）。声明校验：非空集合、`node_key` 唯一、禁自依赖、依赖必须同 revision 可解析、三色 DFS 环检测 fail-closed（`PlanCycle`）、结构性上界（100K 节点 / 每节点 256 依赖——仅拒绝无界声明，不构成 G2 声明）。

### 2.3 API 面（`store.rs`；全程 typed error，新代码零 unwrap/panic）

- `apply_plan_revision`：revision 1 建 plan（id 由幂等键派生），后续 revision 须持 plan 句柄 + 完整节点集（revision 是全量声明）；单事务内推进 `plans.current_revision`、upsert `plan_nodes`、追加链回执（trigger 校验链衔接）。
  - **G1 双守卫**：已越过执行边界（`MATERIALIZING` 起）的节点，若新 revision 以不同形状重声明 → **写前** typed 拒绝 `FrozenNodeShapeRewrite { plan_id, node_id, declared_revision }`；逐位同形重声明合法且原行不动（revision/digest 保持原值）。存储层另有 trigger 兜底（见 2.1）。
  - 执行边界语义：授权事实本身存于其他权威（ADR-0013 verify-then-commit 在物化门核验），本权威的 durable fence 是物化边界——如实登记，不冒充授权边界。
  - 幂等：事务内先按键查回执；重放按（派生 plan_id、双 root、节点数、时间戳）逐位比对——相等返回 `Replayed(原回执)`，不等 `IdempotencyConflict`。
- `record_node_transition`：§25.2.1 合法边子集校验（`IllegalNodeTransition`）→ 声明 revision CAS（`StaleNodeRevision`，`[PLAN-DAG-001]` fence：形状被新 revision 推进后，持旧视图的飞行中迁移被拒）→ 状态 CAS（`NodeStateCasMismatch`）→ 凭证稠密插入 + 节点推进同事务；幂等键重放返回原凭证。
- 读面：`inspect_plan` / `inspect_plan_revision` / `inspect_node` / `list_plan_nodes`（按 node id 确定性排序；行为 lifetime metadata——被后续 revision 丢弃的节点行保留，当前声明集由头 revision 的 `nodes_root` 定义）/ `inspect_node_vouchers`；`verify_revision_chain` 从 revision 1 逐环节重推导 digest，任何篡改/跳链以 `CorruptRecord` fail-closed。

### 2.4 TDD 红→绿记录（G1）

1. 测试先行：`tests/g1_revision_immutability.rs` 写就后，`apply_plan_revision` 对「已执行节点 + 不同形状」的新 revision **静默返回 `Ok(Applied)`**——证伪条件「存在改写已执行节点 revision 的路径」在公共 API 面成立（revision 内容携带新形状而节点行保留旧形状，静默分歧）。实测红：`g1_executed_node_revision_cannot_be_rewritten_by_new_revision ... FAILED`（got `Ok(Applied(..revision: 2..))`）。
2. 加守卫（typed 写前拒绝）后同测试绿；存储层 trigger 由 `g1_storage_triggers_block_raw_rewrites` 钉死（裸 SQL 改 frozen 节点 declared_revision / node_digest、改已提交回执，均 ABORT 且 integrity ok）。

## 3. 验证证据

本地实跑（macOS，rustc/cargo 1.97.1，分支 `feat/w28-a`）：

```text
cargo test -p nlos-plan          # 6 test targets 全 ok / 0 failed
                                 #   authority 7 + g1_revision_immutability 3
                                 #   + plan_fault_matrix 5（含 helper no-op）+ restart_replay 2
cargo fmt --all --check          # 通过（0 差异）
cargo clippy -p nlos-plan --all-targets -- -D warnings   # exit 0 / 0 warning
```

- G1：`g1_executed_node_revision_cannot_be_rewritten_by_new_revision`（typed 拒绝 + 零 durable 损伤：头不动、节点行逐位不变、链仍验证）、`g1_frozen_node_redeclared_bit_identical_keeps_original_revision_and_digest`（同形重声明合法、已执行节点 revision/digest/state 保持、pre-execution 节点自由改形且头推进）、`g1_storage_triggers_block_raw_rewrites`（三层 trigger 全 ABORT）。
- 重启 replay（`tests/restart_replay.rs`）：效果间重启逐轮重放——revision 1/2 各恰一行、每个凭证恰一条且 `Replayed` 不再推进状态、rebound 键 typed 冲突、frozen 节点保持 revision 1、链 2 节点验证、`integrity_check` ok；崩溃前旧视图被 CAS 栅栏拒（`NodeStateCasMismatch`/`StaleNodeRevision`）。
- 权威面（`tests/authority.rs`）：跨库确定性派生、幂等重放/键冲突、三节链验证 + 未知 plan typed、结构性负路径（空集/重复键/自依赖/未知依赖/环）、§25.2.1 全边走查（含 `REHYDRATING → MATERIALIZING` 再入）到 `COMPLETED` 终态、终态无出边、reopen 识别 v1 / 拒绝未知版本。

## 4. 故障矩阵 F1–F4 结果（`tests/plan_fault_matrix.rs`）

故障模型：单机本地 SQLite + `nlos-store-fault` VFS（`FAULT_LOCK` 串行、kill-9 子进程 + piped `READY` 管道同步、wal 语义），接线照抄同仓 `semantic_recovery_fault_matrix.rs` / `takeover_fault_injection.rs` 范式；每行结尾 `PRAGMA integrity_check` = ok。

| 行 | 故障 | 结果 |
|---|---|---|
| F1 | kill-9 中断 apply 事务（子进程 `BEGIN IMMEDIATE` 后插入携带真实幂等键的幻影 plan 头 + revision 回执，未提交即被强杀） | **PASS**：重开幻影全回滚（三表 0 行）；真实 apply 干净 `Applied` 不碰撞；同键重放由持久回执应答（单行）；无双应用 |
| F2 | commit 后崩溃（revision 1 经公共 API 提交返回后被强杀） | **PASS**：已提交前缀逐位保留；同键重放 `Replayed`（恒单回执）；续推 revision 2 链衔接（parent = rev1 digest）且 `verify_revision_chain` 2 节点通过；重启后 transition 凭证收敛且重放幂等（恒 1 行） |
| F3 | IoErr（`FailWritesAfter { 0, IoErr }` 于 revision 2 的 apply 事务） | **PASS**：`PlanStoreError::Sqlite` 显式失败且错误链含 i/o 条件；无半截状态（head 仍 revision 1、节点行不推进、恒 1 回执）；disarm 后同请求成功、链 2 节点完整 |
| F4 | 静默丢写（`PowerLossAfter { 0 }`：revision 2「报告成功」但未落盘） | **PASS**：连接死亡重开后幻影 revision 不冒充事实（head 仍 1、integrity ok）；redo 恰应用一次；二次重开验证真持久且回执逐位一致；链验证 2 节点；transition 凭证提交 + 幂等重放（恒 1 行） |

矩阵复跑 2 次稳定。kill-9 模拟进程崩溃（页缓存存活，非断电）；落盘丢失由 `PowerLossAfter` 覆盖；不外推跨 Cell / 真实断电 / plan-task-resource 三权威原子性。

## 5. 已知限制与 deferred minors（如实登记）

- **SKELETON 范围（W28-A 视角）**：仅状态面。无 manifest 模板编译入口（W28-B）、无 TaskSpec 关联字段与 ScaleProfile 维度重绑（W29-A，决定 3/4——`max_task_nodes` 目前仍以 Task 注册近似承载，口径切换未发生）、无物化门/Resource/Operation 接线、无 IPC/CLI/SABI 面。Resolver/解析回执已由 §7（W29-B，决定 5）落地。
- **G1 fence 边界 = 物化边界**：「已授权」半边（`WAITING_AUTHORIZATION` 及之前）不在冻结集——授权事实属其他权威，经 ADR-0013 在物化门核验；若后续接线发现需要把授权回执本身落本权威，按新 revision 演进 schema，不回改 v1 语义。
- **历史 revision 节点集不可从 `plan_nodes` 行重建**：`plan_nodes` 是每节点单行（有界 metadata 姿态），历史 revision 的完整节点集在 v1 仅由 `nodes_root`/digest 链见证（apply 时校验，之后由不可变 trigger 保真）；`verify_revision_chain` 验证链结构与存储 digest 公式，不重推导历史 nodes_root。**W29-B 更新**：schema v2 起每 revision 的声明形状（节点集+边集）持久于 `plan_revision_nodes`/`plan_revision_edges`（§7.1），v2 后 revision 的形状可逐位重建并经 root 复核；v2 前 revision 无形状行，解析按 `RevisionShapeUnavailable` fail-closed。
- **pre-execution 节点同形重声明仍推进 `declared_revision`**：revision 是全量声明、节点随当前 revision 走（已测试钉死该语义）；副作用是飞行中迁移以 `StaleNodeRevision` 被拒（typed，非静默）。若未来要求「未变形不推进」，属行为决策变更，需同移测试钉死。
- **apply 无调用方 revision CAS**：单写者 + `BEGIN IMMEDIATE` 串行化下并发 revision 为「最后合法全量声明胜」（链式衔接，无静默改写）；`expected_current_revision` CAS 登记为 deferred minor。
- **结构性上界非规模声明**：100K/256 仅为拒绝无界声明；G2（惰性有界、RSS/metadata 上界、100K probe）属 W31（**W31-D 更新**：该组数字指标已由 §9 落地）。
- 其余 deferred：无 golden DDL 文件（nlos-task golden_v*.sql 先例，schema 首次 churn 时补）；`plan_nodes` 无 `(plan_id, node_state)` 索引（读面仅按 node id，规模车道再评估）；`plan_digest` UNIQUE 依赖密码学抗碰撞性质。

## 6. 未运行项（显式列出，W28-A）

- **push 与 PR：未执行**——按 W28-A 派工纪律，push 由控制器统一执行。
- **`cargo test --workspace`：未运行**——派工单 MUST NOT 条款（波次屏障由控制器收口）；本 crate 全量 + workspace `cargo metadata` 成员解析已验证。
- **三平台 CI / MSRV：未运行**——待 push 后 CI 触发。
- **TS/Py conformance、schema 生成物检查：未运行**——零 `schema/`/`gen/` 写集触碰，无触发面。
- **G2/G5 数字指标：未运行**——W31 车道。

## 7. W29-B：Dependency Resolver（B4-3；ADR-0016 决定 5；G4）

状态：`PASS`（2026-09-20，lane W29-B，分支 `feat/w29-b`，base 6c7a404）

> 决定 5 原文落点：「依赖解析落 immutable 解析 receipt（typed selector → 带版本/generation 的 handle，`[PLAN-DEPENDENCY-001]`），可审计、可 fence；不采用纯查询返回」。本 lane 将「typed selector」实例化为 **plan revision 选择器**（`PlanRevisionSelector::Current | At`），handle 即 durable receipt 本身（携带 `revision` + `plan_digest` 双版本锚）。

### 7.1 实现事实

**Schema v2（additive，`schema.rs::migrate_v2`，单事务 `BEGIN IMMEDIATE`，v1→v2 与 v1 表缺失均 fail-closed）**：

| 表 | 角色 | 关键不变量（DDL trigger/约束承载） |
|---|---|---|
| `plan_revision_nodes` | 每 revision 声明节点集（node_id/node_key/kind/node_digest）——Resolver 的 durable 输入，v2 起 revision 形状可逐位重建 | 禁止 UPDATE/DELETE（write-once）；`UNIQUE(plan_id, revision, node_key)`；FK `(plan_id, revision)` → `plan_revisions` |
| `plan_revision_edges` | 每 revision 声明依赖边 `(dependent, dependency)` | 禁止 UPDATE/DELETE；两条 AFTER INSERT trigger 钉死「边两端点必须在该 revision 声明集内」 |
| `plan_resolution_receipts` | **immutable 解析回执**（= handle） | 禁止 UPDATE/DELETE；`idempotency_key` UNIQUE；`resolved_order` BLOB(16n) / `resolved_edges` BLOB(32m) 宽度 CHECK；FK → `plan_revisions` |

`apply_plan_revision` 写集同事务扩展：追加 receipt 后写入该 revision 全量形状行（节点+边）。**v2 admission 收紧（行为差异，如实登记）**：同一节点重复依赖键由 v1 的「重复计入 digest」改为 typed 拒绝 `InvalidRequest { "duplicate dependency key in one node" }`——否则重复边将撞 `plan_revision_edges` 主键且与 roots 复核不可一致；W28-A 测试未覆盖重复键语义，零回归。

**API 面（`resolver.rs`，新 `impl SqlitePlanAuthority` 块，store.rs 不新增公共面）**：

- `resolve_plan(ResolvePlanRequest) -> PlanResolutionDecision::{Resolved, Replayed}`：selector 解析（`Current` → 头 revision；`At` → 具名 revision，`PlanNotFound`/`RevisionNotFound` typed）→ 载入该 revision 形状行 → **resolve-time root 复核**（`nodes_root`/`dependencies_root` 从存储行重推导并与 immutable receipt 比对，篡改即 `CorruptRecord`）→ Kahn 拓扑排序（最小 `TaskNodeId` tiebreak，确定性）→ 环 fail-closed `PlanCycleMembers { plan_id, revision, members }`（成员点名，见 7.2）→ 单事务落 receipt。
- `inspect_resolution(resolution_id)`：回执读回（decode 时重推导 `resolution_digest`，不符即 `CorruptRecord`）。
- `inspect_resolved_nodes(resolution_id) -> Vec<ResolvedPlanNode>`：**pinned view**——按 resolved order 返回该 resolution 钉住 revision 的形状（读 `plan_revision_nodes` write-once 行，绝不读可变 `plan_nodes` 当前行）；形状行缺失 → `RevisionShapeUnavailable` fail-closed，绝不从当前行静默重推导。

**digest 公式（域版本即兼容边界，公式钉死）**：

- `resolution_id` = `H16("llmos/plan/resolution-id/v1", 幂等键 ‖ plan_id ‖ revision_be)`
- `resolution_digest` = `H("llmos/plan/resolution-digest/v1", plan_id ‖ revision_be ‖ plan_digest ‖ len_be ‖ order 节点 id 序列 ‖ len_be ‖ edges (dependent,dependency) 规范序对)`

**幂等重放规则**：事务内先按键查回执。重放判据 = 同 plan + 同 `resolved_at_ms` + `At` 具名 revision 与原回执一致 + **原回执仍能从其钉住 revision 逐位复算**（order/edges/双 digest 全等；复算失败 = 篡改 → `CorruptRecord`）。`Current` 重试在头推进后仍由原回执应答（crash-retry 语义，receipt 不浮）——重放分支逐一镜像 `apply_plan_revision` 的键重放纪律。

### 7.2 环检测与成员点名

- 排序：Kahn 算法，就绪集取最小 `TaskNodeId`（确定性拓扑序）；依赖先于依赖者。
- 环检测顺序：**先环检后 root 复核**——注入环边（端点均声明、INSERT trigger 不拦）时以 `PlanCycleMembers` 点名报错，非环篡改落 root 复核以 `CorruptRecord` 报错，两者都 fail-closed。
- 成员提取：Kahn 残余子图上的迭代三色 DFS，回边落到当前路径即切出环成员；**仅下游节点不算成员**（测试钉死 d ∉ members）。自环（v2 后仅可经篡改出现）同样点名。
- `PlanCycleMembers` 与 W28-A 单元 `PlanCycle`（apply 面）构成 PlanCycle-family：前者带成员名单，后者保持原判据不改（W28-A 测试零触碰）。

### 7.3 G4 红→绿记录（证伪条件：「存在绕过版本解析直达授权的路径」）

1. **红 #0（测试先行）**：`tests/g4_resolution_fencing.rs` 先写就——`cargo test -p nlos-plan --test g4_resolution_fencing` 编译失败 14 errors（API 不存在）。
2. **红 #1（naive 实现实测红）**：实现初版 `inspect_resolved_nodes` 从**可变 `plan_nodes` 当前行**取形状（现存表即诱因，属真实工程捷径）。实测：`g4_resolution_pins_shapes_against_later_revision_reshape ... FAILED`——revision 1 上 resolve 后，revision 2 重塑未解析节点 b/c，旧 resolution 读形状**静默返回 revision 2 digest**（node b digest `143,121,189,…` ≠ 钉住值 `111,49,147,…`；node c 同理）。此即 G4 证伪面：handle 未经版本解析漂移到 latest。
3. **绿**：形状读取切到 `plan_revision_nodes` write-once 行（pinned view）；同测试绿——旧回执逐位不变、旧键重放 byte-equal、新键解析钉 revision 2、双回执并存可审计。任务书措辞「typed stale/fence error or pinned old view」取 **pinned old view** 为主实现，另以 `g4_stale_resolution_revision_cannot_drive_transitions_past_fence` 钉死 handle 侧影：持旧 resolution revision 推进节点被 W28-A 既有 `StaleNodeRevision` CAS typed 拒绝。

### 7.4 测试清单（新增 14，W28-A 17 项零回归）

| 文件 | 测试 | 覆盖 |
|---|---|---|
| `tests/g4_resolution_fencing.rs`（3） | `g4_resolution_pins_shapes_against_later_revision_reshape` | **G4 主证伪**：N+1 重塑后旧 handle 视图逐位钉 N；重放 byte-equal；新键钉 N+1；双回执并存 |
| | `g4_current_selector_pins_once_and_receipt_never_floats` | Current 只钉一次；头推进后重放由原回执应答；键重绑 typed 冲突；At/Current 同代同内容 |
| | `g4_stale_resolution_revision_cannot_drive_transitions_past_fence` | handle 不提供绕过 generation 的状态推进路径（`StaleNodeRevision`） |
| `tests/resolver.rs`（9） | `diamond_graph_resolves_deterministic_topological_order` | 菱形图拓扑序 + 最小 id tiebreak + 规范边集 + 同头双键同内容 |
| | `resolution_is_deterministic_across_databases` | 跨库逐位确定性 |
| | `selector_and_receipt_negatives_fail_typed` | 未知 plan/revision/回执 typed 负路径 |
| | `resolution_idempotency_matrix` | 双 selector 形态重放 byte-equal；异时刻/异 plan/异 revision 重绑冲突；恒单回执 |
| | `duplicate_dependency_keys_are_refused_typed` | v2 收紧：重复依赖键 typed 拒绝 |
| | `tampered_non_cyclic_shape_fails_closed_on_root_verification` | raw INSERT 追加边/幻影节点 → root 复核 `CorruptRecord`，不解析被篡改内容 |
| | `injected_cycle_fails_closed_naming_exact_members` | 注入回边成环 → `PlanCycleMembers` 恰点名环成员 {a,b,c}，下游 d 不点名 |
| | `missing_revision_shape_fails_typed_never_rederived` | 形状行缺失 → `ResolutionShapeUnavailable`（resolve + pinned view 双面）；已提交回执仍 durable |
| | `schema_v2_migration_paths` | fresh→v2、reopen、v1 戳记幂等再迁移、99 拒绝 |
| `tests/resolver_restart_replay.rs`（2） | `restart_between_resolver_effects_replays_once_and_converges` | 效果间重启逐轮重放恒单回执；头推进后旧键 crash-retry 仍答原回执；链完整、integrity ok |
| | `restart_preserves_pinned_view_across_post_crash_reshape` | pinned view 跨两次重启 + 重塑不漂移 |

### 7.5 验证证据

本地实跑（macOS，rustc/cargo 1.97.1，分支 `feat/w29-b`）：

```text
cargo test -p nlos-plan                              # 8 test targets 全 ok / 0 failed
                                                     #   authority 7 + g1_revision_immutability 3
                                                     #   + plan_fault_matrix 5 + restart_replay 2（W28-A，零回归）
                                                     #   + g4_resolution_fencing 3 + resolver 9
                                                     #   + resolver_restart_replay 2（W29-B 新增 14）
cargo fmt --all --check                              # 通过（0 差异）
cargo clippy -p nlos-plan --all-targets -- -D warnings   # exit 0 / 0 warning
```

### 7.6 已知限制与 deferred minors（如实登记）

- **[PLAN-DEPENDENCY-001] 生态半边**：Package/Skill/Tool/Model/Artifact/Topic/外部服务的 typed selector → generation handle 解析不在本 lane（其声明现以 `input_selectors_digest` 摘要绑定）；本 lane selector 指 plan revision。跨权威 selector 解析随 B4-4 物化门/后续车道推进，「`latest`/搜索结果不得当已授权依赖」在那些面上的硬门届时逐面落测试。
- **apply 写放大**：每 revision 增写 `|nodes| + |edges|` 行（同事务）；100K 档吞吐/尺寸评估归 W31（G2/G5）。
- **回执内联 BLOB**：`resolved_order`/`resolved_edges` 整段存回执（100K 节点 order ≈ 1.6MB）；大图分页或按行引用化 deferred 至 W31 规模车道。
- **无按 plan 的 resolution 列表/审计查询面**：minimal lane 仅 id/键直查；列表面按需 additive。
- **无显式 freshness/stale 探测 API**：G4 以 pinned view + 既有 `StaleNodeRevision` CAS 满足（议题 35 §6 G4 措辞为「typed stale/fence error 或 pinned old view」二选一）；若物化门（B4-4）需要显式 stale 探测，按新需求评估。
- **数据承载 v1 库不可真实构造**：W28-A 代码即 v1，无更旧二进制可产数据；`RevisionShapeUnavailable` 以「drop trigger + 删行」模拟测试，migration 空库/幂等路径为真实路径。
- **`plan_revision_nodes`/`edges` 允许 raw INSERT**（apply 本身走 INSERT，trigger 只拦 UPDATE/DELETE 与未声明端点）；post-commit 注入由 resolve 时 root 复核拒绝（7.4 tamper 测试钉死），非 INSERT 期阻断。
- **环成员提取为残余子图三色 DFS**：全部独立环都会被点名（去重排序）；复杂 SCC 语义（如仅报告最小环）未做，属报告美学非正确性。

### 7.7 未运行项（W29-B，显式列出）

- **push 与 PR：未执行**——派工单 MUST NOT；由控制器统一执行。
- **`cargo test --workspace`：未运行**——派工单 MUST NOT（波次屏障由控制器收口）。
- **三平台 CI / MSRV：未运行**——待 push 后 CI 触发。
- **G2/G5 数字指标、100K benchmark：未运行**——W31 车道。

## 8. W31-E：Context residency 分级最小版（B4-5）

> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W31-E 车道行（验收门：分级读回 + evict 边界测试）；规范 [v0.5 §25.2.1「驻留与惰性物化」](../../design/06-架构设计总纲-v0.5.md)（`ResidencyClass` 定义与 `EVICTED (residency=WARM|COLD)` 语义）与 §28.2 阶段 B「Context residency」交付项；语义链 [议题 28 定案 2](../../discussions/28-海量Agent执行与多层手动调度.md)（`METADATA_ONLY → COLD → WARM → HOT → RUNNING`，`↘ PINNED`）

### 8.1 分级模型与合法边集（规范引用）

**五级 tier**（`NodeResidencyTier`，判别值 1–5 按链序）：`METADATA_ONLY`（仅 TaskNode/AgentRole/依赖与资源声明——`[SCALE-LOGICAL-001]` 有界 durable metadata，每个声明节点的默认 tier）→ `COLD`（checkpoint/Artifact 位于持久存储）→ `WARM`（代码/索引/部分 Context 可快速恢复）→ `HOT`（Process/AgentInstance 已物化）→ `RUNNING`（当前占用执行槽）。逐字对应 v0.5 §25.2.1 `ResidencyClass` 前五级；链序取议题 28 定案 2 线性链。

**合法边集（保守）**：沿链**单步相邻 ±1**（上行 rehydrate / 下行 evict 双向）；自环、跳级（如 HOT→COLD 一步、METADATA_ONLY→RUNNING）一律 `IllegalResidencyTransition` typed 拒绝。依据：议题 28 呈线性链；§25.2.1 驱逐落点 `EVICTED (residency=WARM|COLD)` 即 HOT→WARM→COLD 逐级下走的姿态；无规范文本要求跨级跳迁；每步一凭证使 HOT→WARM→COLD 驱逐与 rehydrate 全程逐级可审计。

**分离轴纪律**：residency 与 §25.2.1 生命周期状态机互为独立轴——独立凭证表（`plan_node_residency_transitions`）、独立 per-node 稠密序列（从 1 起）、独立幂等键、独立 tier CAS（`ResidencyTierCasMismatch`）；residency 合法性**不查询**生命周期状态，生命周期推进也**不触碰** tier 列（互零耦合守卫，测试 `residency_axis_is_orthogonal_to_lifecycle_state_machine` 钉死）。两轴唯一共享的纪律是 `[PLAN-DAG-001]` declared-revision CAS：重塑 revision 推进后，持旧 revision 视图的飞行中 residency 迁移被 `StaleNodeRevision` typed 拒绝（与生命周期面同一 fence 语义，`residency_transition_cas_fences_stale_tier_and_revision` 钉死）。

**PINNED 显式不在本 lane**：v0.5 §25.2.1 第六级 `PINNED` 受 `[SCALE-PIN-001]` 约束（每 pin 必须绑定 ResourceAllocation、owner、reason、上界、expiry/renewal 与 release/fence procedure）——Resource 权威面语义，随物化/资源车道（B4-4 及后续）落，不在 nlos-plan 最小版伪造。

### 8.2 实现事实

| 面 | 内容 |
|---|---|
| schema v3（additive，单 `BEGIN IMMEDIATE`） | `plan_nodes` 增 `residency_tier INTEGER NOT NULL DEFAULT 1`（1=METADATA_ONLY；回填语义：v3 之前权威只记声明、未承载任何执行足迹，保守口径即 metadata-only）与 `residency_transition_count`；新表 `plan_node_residency_transitions`（voucher：idempotency_key UNIQUE、`(plan,node,seq)` UNIQUE、from/to tier CHECK 1–5、observed_revision、时间戳；STRICT） |
| 存储 trigger（4） | `plan_nodes_residency_adjacent`（BEFORE UPDATE OF residency_tier：非相邻变更 ABORT——裸 SQL 跳级在存储层拒绝）；voucher immutable / no_delete；`plan_node_residency_transitions_seq_bound`（voucher 必须是该节点 residency 序列第 N 条稠密衔接） |
| G1 互操作 | residency 列**不是 shape**：`plan_nodes_executed_shape_frozen` 只拦 (declared_revision, node_digest, node_kind)，不拦 tier 迁移——已越过执行边界（G1 冻结）的节点可被 evict（HOT→WARM→COLD 正是其语义），且 identity/shape/state/生命周期凭证全部原样保留（`evicted_to_cold_node_preserves_metadata_facts` 钉死：digest/kind/revision/key/state/生命周期凭证数逐位不变，revision 链仍 verify） |
| 写面 `record_residency_transition` | 幂等重放先行（durable voucher 为权威，重放逐位返回原凭证；重绑 `IdempotencyConflict`）→ 合法边（`IllegalResidencyTransition`）→ revision CAS（`StaleNodeRevision`）→ tier CAS（`ResidencyTierCasMismatch`）→ 时间戳下界（`InvalidRequest`）→ voucher INSERT + 节点 tier/count/updated_at 推进同事务 |
| 读面（分级读回） | `inspect_node_residency` 返回 `NodeResidencyView { tier, transition_count, last_voucher }`（tier + 最后一张迁移凭证一次读回）；`inspect_node_residency_vouchers` 全序列审计列表；`PlanNodeRecord` 增 `residency_tier` / `residency_transition_count`（`inspect_node`/`list_plan_nodes` 同步读回） |
| ID 派生 | `voucher_id = H("llmos/plan/residency-voucher-id/v1")` 截 16B，输入 `(幂等键, node_id, to_tier)`——与生命周期 voucher 域分隔，无碰撞 |
| 打开路径 | 迁移链 0→v1→v2→v3、1→v2→v3、2→v3、3=头；未知版本 `SchemaVersionUnsupported` fail-closed；v2 戳记幂等再迁移路径测试覆盖 |

### 8.3 TDD 红→绿记录

1. **红 #0（测试先行）**：`tests/residency.rs` 先写就——`cargo test -p nlos-plan --test residency` 编译失败 **29 errors**（E0425/E0432/E0599/E0609：tier 类型、三 API、record 字段均不存在）。
2. **绿**：model/schema/store/residency 四面落齐后同套件 10/10 绿；W28-A 17 项、W29-B 14 项零回归（G1/G4 测试逐条原样通过，未触碰任何断言）。

### 8.4 测试清单（新增 10；另 1 处既有测试版本期望更新，见 8.6）

| 测试（`tests/residency.rs`） | 覆盖 |
|---|---|
| `nodes_default_to_metadata_only_tier` | 默认 tier=METADATA_ONLY、count=0、last_voucher=None；未知节点双读面 None |
| `residency_round_trip_walks_adjacent_chain_and_reads_back` | 沿链全上下往返（METADATA_ONLY→COLD→WARM→HOT→RUNNING→…→METADATA_ONLY→COLD），每步凭证稠密、tier+last_voucher 同步读回；全程生命周期轴零触碰 |
| `illegal_residency_edges_fail_typed` | 5×5 全矩阵纯函数断言（相邻 ±1 且非自环为唯一合法集）；API 面 12 条非法边（跳级+自环）逐条 `IllegalResidencyTransition` 且携带 from/to；拒绝后 durable tier 不动；未知节点 typed |
| `residency_transition_cas_fences_stale_tier_and_revision` | tier CAS（期望 COLD 实为 METADATA_ONLY）与 revision CAS（revision 2 重塑后持旧视图被拒）；纠正后合法提交且重塑未重置 residency 轴；时间戳早于首声明拒绝 |
| `residency_eviction_is_idempotent_and_replay_safe` | evict 边界：HOT→WARM→COLD 逐键重放 byte-equal 原凭证；重绑 typed 冲突；恰 5 张凭证（3 上 2 下）；落点 tier=COLD |
| `evicted_to_cold_node_preserves_metadata_facts` | 执行冻结（ACTIVE）节点 evict 到 COLD：node_key/kind/declared_revision/node_digest/state/生命周期凭证逐位保留；revision 链仍 verify；分级读回 tier=COLD + last_voucher（G2 姿态） |
| `residency_axis_is_orthogonal_to_lifecycle_state_machine` | DECLARED/ELIGIBLE 节点可独立持 COLD；两轴计数/凭证序列互不污染；residency evict 不驱动生命周期 |
| `residency_restart_replay_converges` | 三段重启：升链后重启重放全键 byte-equal→evict→再重启；恒 5 凭证零漂移；integrity ok |
| `schema_v3_migration_paths` | fresh→v3、reopen、v2 戳记幂等再迁移（事实存活）、99 拒绝、integrity ok |
| `storage_triggers_guard_raw_residency_rewrites` | 裸 SQL 跳级 UPDATE / voucher UPDATE / voucher DELETE 全 ABORT；事后分级读回原样 |

### 8.5 验证证据

本地实跑（macOS，rustc/cargo 1.97.1，分支 `feat/w31-e`）：

```text
cargo test -p nlos-plan                              # 8 个含测试 target 全 ok / 0 failed（41 项）
                                                     #   authority 7 + g1_revision_immutability 3
                                                     #   + plan_fault_matrix 5 + restart_replay 2（W28-A 零回归）
                                                     #   + g4_resolution_fencing 3 + resolver 9
                                                     #   + resolver_restart_replay 2（W29-B 零回归）
                                                     #   + residency 10（W31-E 新增）
cargo fmt --all --check                              # 通过（0 差异）
cargo clippy -p nlos-plan --all-targets -- -D warnings   # exit 0 / 0 warning
```

### 8.6 已知限制与 deferred minors（如实登记）

- **PINNED 未落**（见 8.1）：随 Resource/物化车道按 `[SCALE-PIN-001]` 全语义落，本权威届时 additive 扩。
- **生命周期↔residency 运行期联动不在本权威强制**：§25.2.1 注释语义（`ACTIVE (residency=HOT; runtime=RUNNABLE|RUNNING|BLOCKED)` 等）是 Materialization/Residency Controller 的运行期不变量（`[SCALE-MATERIALIZE-001]`/`[SCALE-CONTEXT-001]`），归 B4-4/W31-A 及后续 Residency Controller 车道；本权威只记 tier 轴，不做跨轴强制（分离轴纪律）。
- **resident bytes / pin reason / rebuild cost / last-use 台账未落**：`[SCALE-CONTEXT-001]` 的完整 Context Residency Controller 面（工作集字节计量、回收类、pressure 排序）不在本 lane；本 lane 是 plan-node tier 轴最小版。
- **无 IPC/CLI 面**：沿 ADR-0016 骨架范围纪律（本 crate 无控制面），tier 读写仅库面。
- **residency 面 kill-window 故障矩阵未单独扩**：车道门为「分级读回 + evict 边界测试」，已以重启 replay 三段矩阵钉收敛；voucher+节点推进同一事务（沿 W28-A 纪律）。物化门（W31-A）接线如需 kill 注入矩阵，按 B-TASK-008C2G 模式补。
- **W29-B `schema_v2_migration_paths` 版本期望 2→3 更新**：迁移链头随 v3 推进，断言语义不变（fresh→头、reopen、v1 戳记幂等再迁移、99 拒绝、integrity）——非测试弱化；G1/G4 全部断言零触碰。

### 8.7 未运行项（W31-E，显式列出）

- **push 与 PR：未执行**——派工单 MUST NOT；由控制器统一执行。
- **`cargo test --workspace`：未运行**——派工单 MUST NOT（波次屏障由控制器收口）。
- **三平台 CI / MSRV：未运行**——待 push 后 CI 触发。
- **G2/G5 数字指标、100K benchmark、working-set 比例矩阵：未运行**——W31-B 车道（**W31-D 更新**：G2/G5 数字指标与 100K benchmark 已由 §9 落地；working-set 比例矩阵仍归 W31-B）。

## 9. W31-D：10K/100K 逻辑 TaskNode benchmark（B4-9；G2/G5 后半）

> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W31-D 车道行（验收门：100K METADATA_ONLY 惰性有界——RSS/metadata 上界；Task 注册近似退役注明）；[议题 35 §6](../../discussions/35-TaskPlan声明面设计.md) G2（惰性有界）与 G5（维度正规化）后半；`[SCALE-LOGICAL-001]`/`[PERF-SCALE-001]`；[ROAD-B-004](../../design/06-架构设计总纲-v0.5.md) §28.2。
>
> 状态：`PASS`（本切片范围）；ROAD-B-004 整体收口归 W31-G 六门评审，本节不宣称整体达成。

### 9.1 口径与诚实规则（先行声明）

1. **逻辑 TaskNode = `nlos-plan` `plan_nodes` 持久计数**。探针经已落地公共 API `apply_plan_revision` 声明真实节点集，落真实 `plan_nodes`/`plan_revision_nodes`/`plan_revision_edges` 行——**waiting Fiber 不作为 TaskNode 代理**（ROAD-B-003 §6 行诚实规则的沿用）。
2. **Task 注册近似退役注明**（G5 显式条款）：ADR-0016 决定 4 已把 `ScaleProfile::max_task_nodes` 正规化为 TaskNode（`plan_nodes`）持久计数，Task 注册保留独立第二维度；旧近似口径的注册维度 10K/100K 数字见 [B-TASK-SCALE-001](b-task-scale-001.md) §3/§3.1/§12（§12 为口径切换后的复跑）。本节为新口径首次 10K/100K 全量实跑，两口径数字**不混写**。
3. **确定性纪律**：吞吐数字只记录、不断言；断言仅硬上界（每节点 durable 字节 ≤ 4096B、声明+解析活动 RSS 增量 ≤ 256MiB@10K / 768MiB@100K、活动字节增量 ≤ 256B/节点 + 64KiB）与惰性对比面（点读 p95 ≤ 基线 ×16 且 < 100ms，镜像 B-TASK-SCALE-001 探针姿态）。

### 9.2 benchmark 设计（`tests/tasknode_scale_probe.rs`）

- **两档 `#[ignore]` 探针**（10_000 / 100_000 节点，各一个 revision 携带全量节点集、单事务一次 fsync 应用）+ 默认套件内 500 节点全管线 smoke 与纯函数生成器测试（保持探针 helper 不腐化）。
- **混合依赖形状**（确定性构造，G5 要求的 resolved/pending 混合）：约 90% 链式 dependent（`i%10≠0` 依赖前驱，「pending」面）、约 9% 独立 root（零依赖，「resolved/eligible」面）、约 1% hub 叶（非零 100 倍数额外依赖 0 号节点，给 resolver dependents 面加压）；所有边严格指向更小 index，构造上无环。两档边数 9,099 / 90,999（10K/100K）。
- **测量面**：①声明吞吐（apply 总时）；②metadata 足迹（干净关闭后主库+WAL 字节数、每节点分摊、进程 RSS 三点采样）；③惰性点读（`inspect_node`/`inspect_node_residency` 各 64 散布样本 vs 100 节点基线库）；④resolver/分级读回规模面（`resolve_plan` 全图解析、`inspect_resolved_nodes` 批量钉住视图读回、64 个 root 节点 `DECLARED→ELIGIBLE` 生命周期样本、8 个样本节点 `METADATA_ONLY→COLD→WARM→HOT` 上行 + `HOT→WARM→COLD` evict 下行 residency 全链）。
- **evict 对比**（METADATA_ONLY vs COLD/WARM tier）：tier 走位后断言分级读回 tier=COLD、恰 5 张凭证、且节点 metadata 事实（node_key/kind/declared_revision/node_digest）逐位保留 + revision 链仍 verify——即 **G2 姿态：tier 与足迹无关，durable metadata 有界地板不随 residency 变化**；residency 凭证按迁移逐张追加（每步独立事务计时）。

### 9.3 实跑数字（原样誊录）

命令：`cargo test -p nlos-plan --test tasknode_scale_probe -- --ignored --nocapture`（debug/test profile，单平台 macOS arm64，两档同 run，全程 7.5s）。

```
10K declare phase: nodes=10000 apply_total=595.789042ms bytes_after_apply=5120000 per_node_after_apply=512 rss_after_declare=Some(49152000)
10K logical TaskNode profile (single platform): nodes=10000 edges=9099 apply_total=595.789042ms resolve_total=58.974875ms resolved_view_total=21.237542ms inspect_p50_base=10.958µs inspect_p95_base=14.041µs inspect_p95_scale=13.958µs inspect_max_scale=24.5µs residency_p50_base=10.958µs residency_p95_base=12.958µs residency_p95_scale=10.625µs residency_max_scale=11.708µs eligible_sample=64 eligible_total=7.915914ms eligible_max=174.75µs evict_sample=8 evict_walk_total=4.554625ms tier_step_max=128.916µs baseline_database_bytes=296648 bytes_after_apply=5120000 bytes_after_activity=5578752 per_node_after_apply=512 per_node_after_activity=557 activity_delta=458752 rss_before=Some(5865472) rss_after_declare=Some(49152000) rss_after_activity=Some(52740096)
test ten_thousand_logical_task_nodes_stay_lazy_and_bounded ... ok
100K declare phase: nodes=100000 apply_total=6.384175667s bytes_after_apply=50872320 per_node_after_apply=508 rss_after_declare=Some(59244544)
100K logical TaskNode profile (single platform): nodes=100000 edges=90999 apply_total=6.384175667s resolve_total=712.785583ms resolved_view_total=272.266875ms inspect_p50_base=10.916µs inspect_p95_base=11.584µs inspect_p95_scale=14.167µs inspect_max_scale=67.166µs residency_p50_base=11.125µs residency_p95_base=12.791µs residency_p95_scale=10.875µs residency_max_scale=12.125µs eligible_sample=64 eligible_total=7.252124ms eligible_max=384.333µs evict_sample=8 evict_walk_total=4.469458ms tier_step_max=153.667µs baseline_database_bytes=296648 bytes_after_apply=50872320 bytes_after_activity=55394304 per_node_after_apply=508 per_node_after_activity=553 activity_delta=4521984 rss_before=Some(5865472) rss_after_declare=Some(59244544) rss_after_activity=Some(97648640)
test one_hundred_thousand_logical_task_nodes_stay_lazy_and_bounded ... ok
```

### 9.4 要点解读（G2/G5 对表）

| G2 证伪条件 | 实测 | 判定 |
|---|---|---|
| metadata 随节点数超线性 | 每节点 durable 字节：10K=512B(声明)/557B(终态)，100K=508B/553B——**两档逐量级一致（线性）**，远低于 4096B 硬上界；解析活动增量 458,752B/4,521,984B ≈ 45.9B/45.2B 每节点（回执内联 order/edges blob 为主） | 未证伪 |
| 未物化节点预占执行资源 | 全部节点 METADATA_ONLY 声明 + 全图解析后进程 RSS 增量 ≈ 46.9MiB@10K / 91.8MiB@100K（SQLite 页缓存 + 解析向量为主要构成），≪ 256MiB/768MiB 硬上界；零进程/线程/会话预占面 | 未证伪 |
| （惰性读面退化） | `inspect_node` p95：100K 库 14.167µs vs 同 run 100 节点基线 11.584µs（~1.22x，≤16x 断言限）；`inspect_node_residency` p95 10.875µs < 基线 12.791µs（~0.85x）——key-scoped（`plan_nodes` PK）成立 | 未证伪 |

- **resolver 规模面**：100K 节点 + 90,999 边全图解析（root 复核 + Kahn + 双 digest + 回执落库单事务）712.8ms；批量钉住视图读回 `inspect_resolved_nodes` 272.3ms（O(population) by design，记录不断言）。
- **声明吞吐**（记录非断言）：10K=595.8ms（59.6µs/节点）、100K=6.384s（63.8µs/节点）——线性外推无超线性拐点；单事务一次 fsync（对比 B-TASK-SCALE-001 注册维度逐注册 fsync 的 28.3s/100K，声明面按 revision 批量提交）。
- **residency/evict**：tier 单步 ≤ 153.7µs（8 节点 ×5 步全链 4.5ms），跨 evict metadata 事实逐位保留（断言）；COLD/WARM/HOT tier 不改变 per-node durable 地板（tier 为单整数列，凭证按迁移追加）。
- **G5 口径**：本节 100K 档即 `TASK_PROFILE_100K.max_task_nodes = 100_000`（nlos-task 侧常量）对应的逻辑 TaskNode 计数维度；两维度（TaskNode/Task 注册）数字分列于本节与 B-TASK-SCALE-001 §12，近似映射已退役。

### 9.5 验证门（W31-D 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt -p nlos-plan -- --check` | PASS |
| 全量测试 | `cargo test -p nlos-plan` | PASS（11 个含测试 target 全 ok，43 passed / 0 failed / 2 ignored 即两探针；W28-A 17 + W29-B 14 + W31-E 10 零回归，+3 新默认套件用例） |
| clippy | `cargo clippy -p nlos-plan --all-targets -- -D warnings` | PASS（修 `doc_markdown` ×2、`manual_is_multiple_of` ×4 后） |
| 探针实跑 | `cargo test -p nlos-plan --test tasknode_scale_probe -- --ignored --nocapture` | PASS（§9.3 两档数字，单 run 7.5s） |

### 9.6 已知限制与 deferred minors（如实登记）

- **debug/test profile 单平台数字**：release profile 复测、多平台（Linux/Windows）与 CI 化未做；RSS 读数仅 macOS 可移植（`ps`），其余 target 如实 `None`。
- **ScaleProfile admission 未接线**：`max_task_nodes` 的 store 路径 consult（跨 authority 读 `plan_nodes` 计数）仍缺（W29-A §12 缺口 #1 / W30-D 接线位）；本 benchmark 钉死逻辑计数与上界事实，不声称 admission 已强制。
- **`inspect_resolution` 按 id 读回为 O(population)**：回执内联 order/edges blob 解码 + digest 复核随节点数线性（W29-B §7.6 已登记的写放大同源）；批量面记录于 `inspect_resolved_nodes`，未做分页。
- **应用形状为合成混合图**：链段 + hub 叶 + 独立 root；真实 workload 形状（深链/宽扇出矩阵）未扫——按 ADR-0016 骨架范围属后续规模车道可选项。
- **checkpoint/rehydrate benchmark、working-set 比例矩阵**：W31-B/W31-C 车道，非本节。

### 9.7 未运行项（W31-D，显式列出）

- **push 与 PR：未执行**——派工单 MUST NOT；由控制器统一执行。
- **`cargo test --workspace`：未运行**——派工单 MUST NOT（波次屏障由控制器收口）。
- **三平台 CI / MSRV：未运行**——待 push 后 CI 触发。
- **release profile / 多平台复测：未运行**——见 §9.6。

## 10. W31-A：惰性物化门（B4-4；G3）

> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W31-A 车道行（验收门：G3 证伪测试；物化门崩溃窗口故障矩阵）；[议题 35 §6](../../discussions/35-TaskPlan声明面设计.md) G3 逐字（「仅依赖+授权+Namespace+ResourceContract+fanout gate 全满足的节点进入 MATERIALIZING；窗口收缩停止新物化并 checkpoint/evict」；证伪条件「未满足依赖的节点可物化，或窗口收缩后仍新增物化」）；[v0.5 行 3650 `[PLAN-LAZY-001]`](../../design/06-架构设计总纲-v0.5.md)、[行 4503 `[SCALE-MATERIALIZE-001]`](../../design/06-架构设计总纲-v0.5.md)、§25.2.1 状态机（行 4488-4499）；[ADR-0013](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md) verify-then-commit 契约；[ADR-0016 决定 3/4](../../management/adrs/0016-task-plan-declaration-surface.md)。
>
> 状态：`PASS`（本切片范围）；G3 五条件中的依赖就绪 + Task admission 两面已强制（见 §10.7 诚实边界），六门整体评审归 W31-G。

### 10.1 设计：门协议与状态边

物化门是逻辑声明面（`plan_nodes`）到物理执行（Task/Process runtime）的桥，按 ADR-0013 拆在两个权威上：

```text
plan 权威（readiness 事实 + commit）          task 权威（admission consult，只读消费）
────────────────────────────────────────      ─────────────────────────────────────
request_materialization                        answer_plan_materialization
  ├ 依赖就绪校验：节点 pinned revision 的        ├ enforce_task_node_admission
  │ 声明依赖逐个须 COMPLETED                     │   （其余声明节点数 +1 ≤ max_task_nodes）
  │  不满足 → typed DependenciesNotReady        ├ enforce_working_set_admission
  │  （DECLARED 节点先落 BLOCKED_DEPENDENCY 凭证）│   （未决 CommitPermit 数 +1 ≤ max_active_working_set）
  ├ 状态推进（合法边、稠密凭证、单事务）：        └ 返回 admission facts 或 typed denial
  │   DECLARED/BLOCKED_DEPENDENCY → ELIGIBLE
  │   → WAITING_RESOURCE（已 WAITING/REHYDRATING resolve_materialization(verdict)
  │   则不再重复推进）                             ├ APPROVED：请求行翻 APPROVED（记 admission facts）
  └ 落一行 durable PENDING 请求（幂等键）          │   + WAITING_* → MATERIALIZING 凭证【同一事务】
                                                  └ REJECTED：请求行翻 REJECTED（记 typed 原因）
resolve 侧提交内再核：declared-revision CAS          节点停在 WAITING_*——窗口收缩，plan 不失败
（StaleNodeRevision）+ 依赖就绪复核 + 节点态 CAS
```

- **状态边**：门驱动的合法边严格取 §25.2.1 `transition_is_legal` 既有子集（`DECLARED→BLOCKED_DEPENDENCY|ELIGIBLE`、`BLOCKED_DEPENDENCY→ELIGIBLE`、`ELIGIBLE→WAITING_RESOURCE`、`WAITING_AUTHORIZATION|WAITING_RESOURCE|REHYDRATING→MATERIALIZING`）；批准凭证即一张普通 `plan_node_transitions` 凭证（复用既有 voucher 表/稠密序列/CAS 语义），不引入第二状态机方言。
- **存储层门禁（G3 证伪 #1 的第三层）**：`plan_node_transitions_materializing_gated` trigger 拒绝一切无 APPROVED 请求行的 `to_state = MATERIALIZING` 凭证插入——裸 `record_node_transition` 面从此对该边关闭（W28-A 骨架该边开放，属「本 lane 加 GATE 逻辑」的显式语义升级；既有 4 个测试文件的该边走查改为过门获取批准，断言零弱化，见 §10.6）。
- **单飞行请求**：`plan_materialization_requests_one_pending` partial unique index（每节点至多一行 PENDING）；`resolve_once` / `resolved_shape` / `pending_shape` / `no_delete` trigger 承载「PENDING 是唯一可更新状态、身份列冻结、解析形状列匹配」。
- **窗口收缩可观测**：拒绝是 durable 的——请求行携带 typed 原因（`WorkingSetFull{profile, active_count, max}` / `TaskNodeCapExceeded{profile, task_count, max}`）+ `resolved_at_ms`，节点停留 `WAITING_RESOURCE`，重试须新键新请求（历史保留为审计轨迹）；批准行携带 admission facts（profile + 两维投影计数）+ `approved_voucher_id` 链接到翻转凭证。
- **checkpoint/evict 组合面**（`[SCALE-MATERIALIZE-001]` 收缩响应的 W31-A 范围）：拒绝后既有生命周期的 `ACTIVE→CHECKPOINTED→EVICTED` 与 W31-E residency `HOT→WARM→COLD` 逐级下走可组合收敛，再入场（`EVICTED→REHYDRATING→` 过门 `MATERIALIZING`）必须走新门轮（G3 测试 #4 钉死）；窗口整形的控制器策略归 W31-F。

### 10.2 TDD 红→绿记录（G3 red first）

1. **红**：`tests/materialization_gate.rs` 先写就（含 6 个用例）后首跑，编译面即红——`error[E0432] unresolved imports nlos_plan::{MaterializationRequest, MaterializationResolution, …}`、`error[E0599] no method named answer_plan_materialization / request_materialization / resolve_materialization / inspect_materialization_request / inspect_declared_task_node_count`：门 API 面不存在。
2. **更实质的系统级红（lane 前事实）**：W28-A 骨架上裸 `record_node_transition(ELIGIBLE→WAITING_RESOURCE→MATERIALIZING)` 对依赖未满足节点直接成功——改造前 `tests/authority.rs` §25.2.1 全边走查测试正是（合法地）这样裸走该边的；即 G3 证伪条件 #1 在 lane 前的系统上成立。
3. **绿**：schema v4 + `materialization.rs` + nlos-task 接线落地后 6 用例全绿（含裸边存储层 ABORT、伪造 resolution typed 拒绝两条 bypass 面断言）；既有 4 文件过门改造后全量零回归。

### 10.3 实现事实

- **schema v4**（additive，单事务）：`plan_materialization_requests`（16 列 STRICT：request_id PK / 幂等键 UNIQUE / (plan_id, task_node_id) FK plan_nodes / `observed_declared_revision`（请求钉住的声明 revision，resolve CAS 依据）/ status(1,2,3) / 批准列组 admission_profile·admitted_task_nodes·admitted_active_working_set·approved_voucher_id / 拒绝列组 rejection_kind(0/1/2)·rejection_profile·rejection_observed·rejection_cap / requested_at_ms·resolved_at_ms）+ §10.1 所列 5 trigger + 1 partial unique index；`migrate_v4` 沿线性链（0/1/2/3 → 4），v3 表在场校验、部分态拒绝。
- **`crates/nlos-plan/src/materialization.rs`**：`request_materialization` / `resolve_materialization` / `inspect_materialization_request` / `inspect_node_materialization_requests`（历史，requested_at+rowid 序）/ `inspect_declared_task_node_count`（store-wide `plan_nodes` 计数，决定 4 维度读面）；域分隔派生：request_id、drive 凭证幂等键（step 域分隔）、approval 凭证幂等键各自独立域，杜绝跨表键碰撞。typed 错误新增四枚：`DependenciesNotReady{node_id, unresolved}` / `MaterializationRequestNotFound(key)` / `MaterializationRequestAlreadyPending{node_id, pending_key}` / `NodeNotAwaitingMaterialization{node_id, current}`。
- **Task 侧接线（`crates/nlos-task/src/materialization.rs`，最小 additive）**：`MaterializationAdmissionFacts{profile_id, projected_task_nodes, projected_active_working_set}` + 自由函数 `admit_plan_materialization(profile, other_declared_task_nodes, active_working_set)`（先 task-node 维后 working-set 维，denial 复用既有 typed `TaskNodeAdmissionDenied` / `WorkingSetAdmissionDenied`，即拒绝的 typed 原因本体）+ `SqliteTaskAuthority::answer_plan_materialization(other_declared_task_nodes)`（内部读自身未决 permit 数，`scale_profile()` 为新增 pub(crate) 读面）。**只读消费：Task 侧零 schema 变更、零新 durable 行**（组合事实按 ADR-0013 嵌套回执姿态落在 plan 侧请求行上）。**新增 nlos-task 公共 API 两枚 + pub(crate) 读面一枚——按派工单显式 REPORT**。
- **nlos-plan dev-dependency `nlos-task`**：G3 证伪测试需真实 Task 权威走消费路径；无环（nlos-task 不依赖 nlos-plan），公共面不新增编译依赖。
- **consult 语义（决定 4 口径注记）**：task-node 维度按「其余声明节点数 +1（候选自身）≤ max_task_nodes」投影——物化边界确认「含候选的声明集适配 tier」；working-set 维度按「未决 CommitPermit +1 ≤ max_active_working_set」投影。声明面（apply 时）的 admission consult 仍是 W30-D 登记缺口，本 lane 关闭的是其物化半边（W29-A §12 缺口 #1 的边界接线）。

### 10.4 G3 证伪测试（`tests/materialization_gate.rs`，6 passed）

| 用例 | 覆盖 | 结果 |
|---|---|---|
| `g3_unmet_dependency_cannot_materialize_through_any_face` | 证伪 #1 三层闭合：门 typed 拒绝（`DependenciesNotReady`，节点落 `BLOCKED_DEPENDENCY`）；伪造 resolution → `MaterializationRequestNotFound`；裸 `WAITING_RESOURCE→MATERIALIZING` → 存储层 trigger ABORT（Sqlite typed），节点停留、全 plan 恰 1 节点越界 | PASS |
| `g3_admission_denial_shrinks_window_with_typed_durable_reason` | 证伪 #2：真实消费路径（`G3_NODE_CAP_ONE` tier）下拒绝不可绕——durable typed 原因（`TaskNodeCapExceeded{task-10k 档位名, 2, 1}`）经读回逐位相等；节点停 `WAITING_RESOURCE`、窗口 0、B 节点不受牵连（plan 不失败）；拒绝后裸边仍 ABORT；重放 `ReplayedRejected`、异 verdict `IdempotencyConflict`；宽 tier 下新键重试收敛（历史 rejected+approved 两行） | PASS |
| `g3_working_set_dimension_denies_and_window_stops_growing` | working-set 维度（零上界 tier）：typed `WorkingSetFull{.., active_count:1, max:0}` + 窗口 0 | PASS |
| `g3_window_shrink_composes_with_checkpoint_evict_and_residency_eviction` | 收缩响应组合：拒绝后 `ACTIVE→CHECKPOINTED→EVICTED` + residency `HOT→WARM→COLD`（W28-A/W31-E 既有面）收敛到窗口 0；`EVICTED→REHYDRATING` 后再入场必须过新门轮 | PASS |
| `gate_request_drives_legal_edges_is_idempotent_and_single_pending` | 门权威语义：`DECLARED→ELIGIBLE→WAITING_RESOURCE` 双稠密凭证、幂等重放、同节点第二 PENDING typed 拒绝、越界节点再请求 typed 拒绝 | PASS |
| `gate_resolve_is_fenced_by_declared_revision_cas` | G4 一致 fence：reshape 后 resolve `StaleNodeRevision{expected:1, current:2}`、节点不越界 | PASS |

### 10.5 物化门崩溃窗口故障矩阵（`tests/materialization_fault_matrix.rs`，F1–F4 + helper，5 passed）

故障模型与 harness 与 §4/`plan_fault_matrix.rs` 逐字同源（kill-9 子进程 + piped READY、`FAULT_LOCK` 串行、`nlos-store-fault` VFS、逐行 `integrity_check` ok；断电语义 disclaimer 同 §4）：

| 行 | 崩溃窗口 | 收敛断言 | 结果 |
|---|---|---|---|
| F1 `fault_kill9_mid_request_tx_rolls_back_and_real_request_converges` | request 事务中 kill-9（幻影 PENDING 行携真实幂等键） | 回滚零行、真实 request `Requested`（双驱动凭证）、同键 `Replayed` 恒单行 | PASS |
| F2 `fault_kill9_between_request_and_approval_converges_uniquely` | request 提交后、approval 前 kill-9 | PENDING 单行 + `WAITING_RESOURCE` + 双凭证逐位存续；旧视图重放由持久行应答；重启后 approval 收敛 `MATERIALIZING`（行翻 APPROVED 不加行、第三张凭证同事务）；resolution 重放 `ReplayedApproved` 凭证恒三张 | PASS |
| F3 `fault_io_error_on_approval_fails_closed_and_retry_succeeds` | approval 事务硬 I/O 错误（`FailWritesAfter{0, IoErr}`） | typed `Sqlite` 失败（错误链含 I/O 条件）不假成功；请求仍 PENDING、节点仍 `WAITING_RESOURCE`、凭证恒两张；disarm 后同一 resolution 成功收敛 | PASS |
| F4 `fault_silent_write_loss_on_approval_redo_resolves_once_and_converges` | approval 静默丢写（`PowerLossAfter{0}`，幻影「成功」） | 重开只见落盘前缀（PENDING/`WAITING_RESOURCE`/两凭证/integrity ok）——**不存在 approved-但未翻状态的窗口**（verdict 与状态翻转同一事务，即「crash after approval before node state flip」行的答案：该窗口在存储上不可达）；redo 恰解析一次；二次重开验证真持久 + 重放幂等 | PASS |

### 10.6 既有测试过门改造（断言零弱化清单）

- `tests/authority.rs` 全边走查：两处 `→ MATERIALIZING`（0xa3/0xa8）改为 `gate_into_materializing` 过门（请求+批准各贡献恰一张凭证），凭证总数/断言不变。
- `tests/g1_revision_immutability.rs` `drive_to_materializing`：第三步改过门；G1 三用例全部断言原样（冻结语义不受影响——G1 测试零弱化）。
- `tests/residency.rs` `evicted_to_cold_node_preserves_metadata_facts`：越界改过门 + 显式断言 gated 状态/冻结；residency 断言原样（零弱化）。
- `tests/restart_replay.rs`：walk 拆为两裸步 + 门请求/门批准两个 effect，保持「每 effect 间重启、每键重放恰一次」结构；`transition_count == 3` 与 rebound 键断言不变。
- `tests/residency.rs` / `tests/resolver.rs` migration 版本期望 3→4（随链头推进；语义不变，同 W31-E §8.6 先例）。

### 10.7 已知限制与 deferred minors（如实登记）

- **G3 五条件的诚实边界**：仅依赖就绪 + Task admission（working-set / max_task_nodes 两维）已强制；Namespace / ResourceContract / fanout gate 仍是声明 digest（对应权威未落，W28-A 姿态沿用）。授权面（WAITING_AUTHORIZATION 的解除）无权威承载——门接受该态节点进入并按同一 consult 批准（§25.2.1 该边合法），授权 enforcement 属后续车道。
- **approval 真实性边界（ADR-0013 分工的直接推论）**：plan 侧验证 verdict 的绑定/形状/revision fence/依赖就绪，admission 真理由 Task 权威所有；「admission 说否仍获批准」的组合不可能性经消费路径成立（G3 测试即证），与 Task 嵌套 owner 回执同构。持有 plan 库写权限的调用者手工构造 Approved verdict 属同级的越权前提（本地单进程无跨权威签名面），与既有全部权威面（含 `record_node_transition` 本身）信任模型一致，不额外声明防护。
- **窗口整形策略 = W31-F**（priority/deadline/locality/pressure 的 window shaping 与调度决策 inspect）；checkpoint/evict 控制器自动化 = W31-C/W31-F（本 lane 提供门 + §10.4 #4 的可组合面）。
- **物化窗口计数的观测面未做成 API**：测试经 `list_plan_nodes` 过滤 {MATERIALIZING/ACTIVE/CHECKPOINTED/REHYDRATING} 计窗（EVICTED 已释放席位）；如 W31-F 需要专用 inspect 计数 API 按需补。
- **working-set 维度计数口径**：nlos-task 未决 `CommitPermit` 数（现成 durable 事实），非 plan 侧物化窗口数——两权威词汇有意不混写（ADR-0016 决定 4 分维纪律）。
- **故障矩阵 verdict 为手工构造**：F1–F4 测 plan 侧崩溃窗口（请求/批准事务），真实 consult 路径由 G3 测试覆盖；Task 侧零 durable 写，无 Task 侧崩溃窗口可注入（ADR-0013 该半边无两阶段）。
- **nlos-plan dev-dep nlos-task**：仅测试接线；若后续需要生产期组合器（verdict 映射 wiring），落位归 W31-F/slice-k 组装器（本 lane 的映射 wiring 在 G3 测试内，5 行）。

### 10.8 验证门（W31-A 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt -p nlos-plan -p nlos-task` | PASS |
| 全量测试 | `cargo test -p nlos-plan -p nlos-task` | PASS（nlos-plan 13 target 54 passed / 0 failed / 2 ignored（探针）；nlos-task 47 target 355 passed / 0 failed——G6 零回归） |
| clippy | `cargo clippy -p nlos-plan -p nlos-task --all-features --all-targets` | PASS（0 warning；未使用 `chunks_exact`，CI clippy 1.98 纪律） |

### 10.9 未运行项（W31-A，显式列出）

- **push 与 PR：未执行**——派工单 MUST NOT；由控制器统一执行。
- **`cargo test --workspace`：未运行**——派工单 MUST NOT（波次屏障由控制器收口；W30-A 并行车道持 nlos-task 测试文件写集）。
- **三平台 CI / MSRV：未运行**——待 push 后 CI 触发。

## 11. W31-F：分层 Scheduler 最小版（B4-6）

> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W31-F 车道行（验收门：窗口收缩联动 W31-A；调度决策可 inspect）；[v0.5 §25.2.2「Global → Cell → Worker 分层调度」](../../design/06-架构设计总纲-v0.5.md)（行 4511-4538，阶段 B 取「单机分层 Scheduler」两层最小形态）、[行 4503 `[SCALE-MATERIALIZE-001]`](../../design/06-架构设计总纲-v0.5.md)（Materialization Controller 窗口语义）、[行 4538 `[SCHED-BACKPRESSURE-001]`](../../design/06-架构设计总纲-v0.5.md)、§28.2 交付项「TaskPlan/TaskNode、Dependency Resolver、惰性物化、Context residency 和单机分层 Scheduler」；语义链 [议题 28 定案 3](../../discussions/28-海量Agent执行与多层手动调度.md)；[ADR-0016 决定 2](../../management/adrs/0016-task-plan-declaration-surface.md)（落点 nlos-plan）。
>
> 状态：`PASS`（本切片范围）；形态为「足以支撑 benchmark 的两层」，非完整 OS 调度器。派发决策（dispatch）留控制器——本调度器只做物化选择与门的驱动。

### 11.1 设计：两层形态、选择策略与窗口语义

调度器是 W31-A 门之上的**纯组合层**：无第二状态机、无第二写面、零 schema 变更（内存策略态 + 既有权威 durable 面）。

```text
Global 层 select（纯读，durable plan 状态扫描）          Worker 层 drive（把选择映射到 W31-A 门）
───────────────────────────────────────────            ──────────────────────────────────────
候选 = 状态可等待物化（DECLARED..WAITING_*/             每个选择：
  REHYDRATING，无 PENDING 门轮）且声明依赖               ├ AdoptPendingGateRound：采纳崩溃遗留的
全部 COMPLETED                                            │  durable PENDING 轮（原幂等键续解析）
排序 = ready-FIFO：(first_declared_at_ms,                ├ NewGateRound：按 (node_id, 重试轮次)
  TaskNodeId 字节)——确定性、零 wall-clock                 │  域分隔派生幂等键开新轮
预算 = window − 占用席位                                   ├ consult（AdmissionConsult trait 边界）
  席位 = PENDING 门轮 + 窗口带节点                          │  ├ Admitted → resolve APPROVED
  （MATERIALIZING/ACTIVE/CHECKPOINTED/                     │  ├ Denied   → resolve REJECTED → 窗口收缩
  REHYDRATING；EVICTED/终态已释放）                        │  └ Err      → ConsultFailed，轮留 PENDING
PENDING 轮总是入选（已持席位，解析即收敛）                  └ 拒绝 −1 席/次（下界 1）；批准不增长
```

- **选择策略（定案）**：ready-FIFO——依赖就绪候选按 `(first_declared_at_ms, TaskNodeId)` 字节序。理由：确定性可测、零 wall-clock 启发、不预支 priority/deadline/locality 词汇（`[SCHED-QUEUE-001]` 的策略维度属控制器后续策略面）；且候选集本身依赖就绪才入围，到达序即最小拓扑就绪优先。文档化于 `src/scheduler.rs` 模块头。
- **窗口收缩联动 W31-A（车道门 #1）**：窗口是物化并发上界（席位口径与 §10.4 窗口计数约定一致）；每次 admission 拒绝收缩 1 席，下界 1——持续压力下每 pass 恰一条 durable typed 拒绝探针（压力可见），完全停新物化是控制器决定（`set_window(0)`）；批准永不自动增长，唯一增长路径是控制器杠杆 `set_window`。拒绝的 durable 事实本体即 W31-A 请求行（typed 原因 + `resolved_at_ms`），调度器不复制。
- **崩溃窗口联动（车道门 #4 前半）**：门轮中断（request 已提交、verdict 未解析——§10.5 F2 窗口）由下一 pass 采纳收敛：Global 层把 PENDING 轮分类为采纳选择，Worker 层用原幂等键 consult+resolve；新轮幂等键 `digest(llmos/plan/scheduler-request-key/v1, node_id, 重试轮次)`，重试轮次读自节点 durable 请求历史——重启后同轮同键、解析后新轮新键。存储级 kill 窗口（事务中 kill-9、I/O 错误、静默丢写）已由 §10.5 F1–F4 故障矩阵钉死，本层不重复注入。
- **调度决策可 inspect（车道门 #2）**：typed 决策轨迹 `SchedulerDecision`（Selected{kind}/Skipped{typed reason}/Approved{voucher}/Rejected{typed reason}/ConsultFailed/DriveRefused{typed error}）+ 每轮 `SchedulerPassSummary`（window_before/after、selected/skipped/approved/rejected）。**诚实选择：内存有界 ring（默认 1024 条，可配）**——durable 审计轨迹就是 W31-A 的请求行/凭证（`inspect_materialization_request` / `inspect_node_materialization_requests`），再做 durable 决策日志即复制权威；内存轨迹重启即失，如实声明。
- **不可绕门（车道门 #3）**：调度器对 MATERIALIZING 无任何写路径——一切效果经 `request_materialization`/`resolve_materialization`；裸边仍被 §10.1 存储层 trigger 拒绝（测试复验）。consult 经 `AdmissionConsult` trait 注入（`Ok(Admitted|Denied)` 为裁决、`Err` 为 consult 自身故障留 PENDING 待采纳），nlos-task 保持 dev-dep 不进公共面；Task 权威到 trait 的 1:1 映射为组装器接线（slice-k；G3 与本测试各自内联携带）。

### 11.2 TDD 红→绿记录

1. **红**：`tests/scheduler.rs` 先写就（6 用例）后首跑，编译面红——`error[E0432] unresolved imports nlos_plan::{AdmissionConsult, AdmissionConsultOutcome, MaterializationScheduler, SchedulerDecision, SelectionKind, SelectionSkipReason, …}`：调度器 API 面不存在。
2. **绿**：`src/scheduler.rs`（+ `model.rs` 域分隔常量、lib.rs 导出、四枚 crate 内 helper 提升 pub(crate)：`unresolved_dependencies`/`REQUEST_COLUMNS`/`raw_request_row`/`decode_request_row`（materialization.rs）、`raw_node_row`/`decode_node_row`（store.rs）——零语义变更）落地后 6 用例全绿；其间一次断言修正（崩溃采纳用例误assume a/b FIFO 末位序，改为存在性断言——节点 id 为派生摘要，序不硬编码）。

### 11.3 实现事实

- **`crates/nlos-plan/src/scheduler.rs`**（新模块，~650 行含文档）：`AdmissionConsult`/`AdmissionConsultOutcome`（Worker 层 consult 边界）、`MaterializationScheduler`（`new`/`with_decision_log_capacity`/`window`/`set_window`/`decisions` + 两层显式公共面 `select`（Global）与 `drive`（Worker）+ `run_pass` 组合）、typed 读回 `SelectionReport`/`SelectionEntry`/`SelectionKind`/`SkipEntry`/`SelectionSkipReason`/`SchedulerDecision`/`SchedulerDecisionRecord`/`SchedulerPassSummary`。Global 层一条 SQL 扫描（`ORDER BY first_declared_at_ms, task_node_id`）+ 一条 PENDING 轮查询 + 一条窗口带计数；依赖就绪复用 W31-A 的 `unresolved_dependencies`（同一就绪谓词，无双写）。存储错误（`PlanStoreError::Sqlite`）中止 pass；typed 门拒绝逐节点落 `DriveRefused` 决策。
- **`model.rs`**：`SCHEDULER_REQUEST_KEY_DOMAIN` 域分隔常量（沿 v1 命名链）。
- **lib.rs**：`mod scheduler` + 公共导出；crate 头「out of scope」段随 W29-B/W31-A 已落地事实刷新（现为：生产期 consult 映射接线归 slice-k 组装器、派发决策留控制器、IPC/CLI 面）。
- **nlos-task：零触碰**（派工单边界遵守；消费路径 API 经 dev-dep 只读使用）。
- **零 schema 变更**：SCHEMA_VERSION 保持 4；调度器无 durable 行（§11.1 诚实选择）。

### 11.4 测试（`tests/scheduler.rs`，6 passed）

| 用例 | 覆盖 | 结果 |
|---|---|---|
| `selection_is_deterministic_ready_fifo_bounded_by_window` | 车道门·确定性：混合就绪/未就绪/已取消 + 双 revision 时间分层；选择序独立于 `list_plan_nodes` 复算逐位相等（`(first_declared_at_ms, node_id)`）；窗口上界截断（第三就绪节点 `WindowExhausted`）；`DependenciesNotReady{unresolved:[dep]}` / `NotAwaitingMaterialization{Cancelled}` typed skip；全新调度器实例重放 byte-equal；未知 plan typed `PlanNotFound` | PASS |
| `window_shrinks_on_admission_rejection_and_is_inspectable` | 车道门·窗口收缩联动：真实 Task 消费路径（零 working-set 档）下 4 拒绝 → `window 4→1`（下界）summary/`window()` 双面可 inspect；pass2 恰 1 选择 3 skip；0 节点越界；每节点 1 行 durable typed `WorkingSetFull` 拒绝；决策轨迹 4 条 Rejected | PASS |
| `scheduler_cannot_materialize_past_admission_gate` | 车道门·不可绕门：拒绝 consult 三 pass 后 0 节点 MATERIALIZING；裸 `WAITING_RESOURCE→MATERIALIZING` 仍存储层 ABORT；换宽档后同面 2 批准、各恰 1 行 APPROVED 请求行（只经门） | PASS |
| `crashed_gate_round_converges_via_adoption_on_next_pass` | 车道门·崩溃窗口：手工遗留 PENDING 轮（§10.5 F2 窗口语义）→ 重启调度器分类 `AdoptPendingGateRound{原键}`（`seats_in_use=1` 计席位）、原键解析 APPROVED、节点历史恰 1 行；并行节点走派生新键 | PASS |
| `consult_failure_leaves_pending_round_and_next_pass_adopts` | consult 自身故障（trait `Err` 路径）：`ConsultFailed` 决策可 inspect、轮留 durable PENDING、窗口不收缩（无裁决）；恢复后同轮采纳解析、历史仍 1 行 | PASS |
| `controller_lever_and_seat_release_drive_progress` | 控制器杠杆与席位释放：window 1 下第二节点 `WindowExhausted` 双 pass；`MATERIALIZING→ACTIVE→COMPLETED` 释放席位后第三 pass 批准；`set_window` 为唯一增长路径（无自动增长） | PASS |

### 11.5 已知限制与 deferred minors（如实登记）

- **cancelled 节点的遗留 PENDING 轮泄漏席位**：节点在门轮 PENDING 期间被取消（`→ CANCELLED` 合法裸边）后，该死轮仍计席位且 Global 层按 `NotAwaitingMaterialization` 跳过（不采纳）。cancel-vs-in-flight-round 对账归控制器/W31-C（reclaim 车道）；最小版如实登记。
- **重放拒绝也收缩窗口**：`ReplayedRejected`（并发行为者已解析同键拒绝后的重驱动）保守计一次收缩——窗口下界 1 保证不因重放放大而熄火；并发双调度器场景仅由单飞行约束（§10.1 partial unique index）+ typed `MaterializationRequestAlreadyPending` 拒绝兜底，无跨进程互斥。
- **多 plan 公平性未做**：pass 以单 plan 为域；跨 plan 公平/priority/deadline/locality 整形属 `[SCHED-QUEUE-001]` 策略面与控制器（MUST NOT 边界内如实不做）。
- **决策轨迹内存态**：重启即失（§11.1 诚实选择）；durable 事实以 W31-A inspect 面为准。
- **Global 层扫描为逐节点依赖校验**（N 次 `unresolved_dependencies` 查询）：100K 量级扫描吞吐未做 benchmark 门（G2/G5 归 W31-D/G 评审口径）；调度器自身规模探针未运行，如实登记。
- **生产期 consult 映射接线（Task 权威→`AdmissionConsult`）未落**：按 W31-A §10.7 既定分工归 slice-k 组装器；两处测试内联携带同一 1:1 映射。

### 11.6 验证门（W31-F 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt -p nlos-plan -- --check` | PASS |
| 全量测试 | `cargo test -p nlos-plan` | PASS（14 target 全 ok，60 passed / 0 failed / 2 ignored 即两探针；W31-A 54 基线零回归 + 6 新调度器用例） |
| clippy | `cargo clippy -p nlos-plan --all-features --all-targets` | PASS（0 warning；未使用 `chunks_exact`，CI clippy 纪律） |

### 11.7 未运行项（W31-F，显式列出）

- **push 与 PR：未执行**——派工单 MUST NOT；由控制器统一执行。
- **`cargo test --workspace`：未运行**——派工单 MUST NOT（波次屏障由控制器收口）。
- **三平台 CI / MSRV：未运行**——待 push 后 CI 触发。
- **调度器规模/吞吐探针：未运行**——本切片验收门为收缩联动 + 决策 inspect 两语义门；benchmark 矩阵归 W31-B/G 口径。

## 12. W36-P7：G4 生态 selector 半边 + G3 三条件结构化（C-SELECTOR，移交#7 前半）

> 对应：[进度单 §C.3.2 `C-SELECTOR` 行](../../management/stage-c-progress.md)（移交#7；`NOT_STARTED`（W31-G §8.2.2/§8.2.3；U-7/U-8）→ 本 lane 落 typed selector→generation handle 解析 + 负路径 + 三条件结构化）、[§C.5.1 移交#7](../../management/stage-c-progress.md)、[§C.5.3 W36 波](../../management/stage-c-progress.md)；[W31-G §8.2.2/§8.2.3 residuals](reviews/w31g-road-b004-gates.md)（G3 三条件仍为声明 digest；G4 生态 selector 半边未落）；[v0.5 行 3654 `[PLAN-DEPENDENCY-001]`](../../design/06-架构设计总纲-v0.5.md)（"Package、Skill、Tool、Model、Artifact、Topic 和外部服务依赖 MUST 在计划中以 typed selector 声明，并在执行前解析为带版本/generation 的 handle"）、[行 3650 `[PLAN-LAZY-001]`](../../design/06-架构设计总纲-v0.5.md)（G3 五条件）；[ADR-0016 决定 5](../../management/adrs/0016-task-plan-declaration-surface.md)（第二半：生态实体同享 durable receipt 语义）。
>
> 状态：`PASS`（本切片范围）；实现分支 `feat/w36-p7`（worktree `llmos-w36-p7`），三实现提交 + 本 evidence 提交，未 push。

### 12.1 设计

W29-B 的 resolver 只解析 plan revision；本 lane 把同一套 G4 pinned-handle 语义平移到**生态实体**（存在于仓库且有 generation 载荷读回的实体），并把 G3 三条件从「约定 ride 在 digest 槽位」升为 typed 验证声明面：

```text
typed EcosystemSelector ──resolve_ecosystem_selector──▶ durable EcosystemResolutionHandle
（kind + 名义 id +              （BEGIN IMMEDIATE 内：replay 优先 → kinds() 门 →
 Current|At(generation)）         source consult → 钉 (generation, content_digest) →
                                  写一次 ecosystem_resolution_receipts，commit）
                                        │
EcosystemSelectorSource trait（kinds() 自声明注册面 + 关联 Error + 静态派发，零 dyn，
镜像 W31-F AdmissionConsult 姿态）      │ verify_ecosystem_resolution_current
  ├─ TestSource（crate 内测试源）       │（显式新鲜度栅栏：source 现值 ≠ 柄上代际 ⇒
  ├─ ArtifactSelectorSource            │  StaleEcosystemGeneration typed fail-closed，
  │   （nlos-plan 特性 artifact-source，│  实体消失 ⇒ EntityNotFound）
  │    可选 dep，默认依赖图不变）        │
  └─ ApplicationSelectorSource
      （nlos-application 薄扩展，沿既有
       application→plan 依赖方向，零环）
```

- **kind 闭集以「权威有 generation 载荷读回」为准入**：Application（`inspect_application` → `current_installation_generation` + manifest digest）、Artifact（`resolve_head` → head revision + content digest）落地；Topic/Skill/Tool/Model/外部服务**不发明**（§12.5 递延台账具名）。
- **负路径全 typed**：未注册 kind ⇒ `EcosystemSourceUnavailable`（fail-closed 不 panic）；实体未知 ⇒ `EcosystemEntityNotFound`；代际失配（双向）⇒ `StaleEcosystemGeneration`；source 失败 ⇒ `Source(e)` 传播且零 durable 行；存储行 kind 域外 ⇒ `EcosystemKindUnknown`；id 不再派生 ⇒ `CorruptRecord`。
- **G3 三条件**：`NodeConditions{namespace: NamespaceCondition（非空/去重/上界 256/序不入身份）, resource_contract: ResourceContractCondition（cpu_shares/memory_mib/io_weight 三维镜像 nlos-resource `ResourceDemand` 词汇，至少一维非零）, fanout: FanoutCondition（下游扇出上界 ≥ 1）}`；digest-only 形态完全兼容（`node_digest` None 分支零追加——v1..v5 行与旧公式逐位相等，测试内复刻旧公式钉死）。

### 12.2 红→绿记录（如实）

1. **红（版本头迁移断言，实测）**：v5 落地时 `schema_v2_migration_paths` / `schema_v3_migration_paths` 实测红（`user_version` 4 ≠ 5 断言失败）；v6 落地时同一对断言再红（5 ≠ 6）——按波次惯例（v3→v4 同型维护）更新为当前头后转绿。生态 API 面以编译红起步（`E0432 unresolved imports`，类型不存在）。
2. **开发期测试捕获的两处缺陷（测试先行价值的如实记录）**：(a) rusqlite 对 STRICT 表 NULL 列的类型推断把 `Option<Vec<u8>>` 读成 `Vec<u8>`（digest-only 行读回 `InvalidColumnType(Null)` 实测红）——修为闭包内显式 `get::<_, Option<Vec<u8>>>`；(b) order-free 身份测试首版误用不同 `node_key` 对比（node_key 本就入 digest，断言红）——修正测试逻辑为同 key 跨 plan 对照。
3. **绿**：三提交全绿（§12.6 门）；既有 G1/G3/G4/residency/materialization 断言零弱化（§12.3 机械适配仅加 `conditions: None`，无断言改动）。

### 12.3 实现事实

- **`crates/nlos-plan/src/selector.rs`**（新模块）：`EcosystemSelectorSource` trait + `EcosystemResolutionError<E>{Plan|Source}` + `resolve_ecosystem_selector` / `inspect_ecosystem_resolution` / `verify_ecosystem_resolution_current`（`SqlitePlanAuthority` impl 块，replay-优先、`kinds()` 门先于 consult、source 失败零 durable）；receipt id 域分隔派生 `digest16(llmos/plan/ecosystem-resolution-id/v1, key‖kind‖entity‖generation)`，读回重派生校验（不匹配 ⇒ `CorruptRecord`）。
- **`model.rs`**：`EcosystemEntityKind`（Application=1/Artifact=2，闭集 + `EcosystemKindUnknown` decode 守卫）、`GenerationExpectation{Current|At}`、`EcosystemSelector`、`EcosystemEntityState`、`EcosystemSourceLookup{Found|NotFound}`、`ResolveEcosystemRequest`、`EcosystemResolutionHandle`/`Decision`；G3 侧 `NodeConditions` 三件套 + `validate`/`canonical_bytes`/`decode`（decode 重验 + 非 canonical ⇒ `CorruptRecord`）+ `MAX_CONDITION_NAMESPACES=256`；`PlanNodeDeclaration.conditions: Option<NodeConditions>` additive 字段。
- **`schema.rs` v5/v6**：v5 `ecosystem_resolution_receipts`（写一次双 trigger；`entity_kind` 有意不带 IN-list CHECK——decode 面保持可证伪，§12.4 #10 以 raw INSERT 99 实测）；v6 `plan_revision_nodes.conditions_body BLOB` 可空列（NULL=digest-only 旧形态）。
- **`store.rs`**：`validate_declaration` 挂接条件验证（⇒ `InvalidNodeConditions` typed）；`node_digest` additive 折叠（None 零追加）；`persist_revision_shape` 写 conditions_body；新读回面 `inspect_node_conditions(plan, revision, node)`。
- **适配器**：nlos-plan `[features] artifact-source = ["dep:nlos-artifact"]` + `src/artifact_source.rs`（`resolve_head` 映射：零修订/未知 id ⇒ NotFound typed miss，保留期外/存储错 ⇒ Store(err)）；nlos-application `src/selector_source.rs`（薄扩展，读回 status 无关）。
- **机械适配**：`conditions: None` 共 17 处（nlos-plan 12 测试文件 + nlos-application `task_templates.rs`/`manifest_task_templates.rs` + nlos-system-control `layer_inspector_authorities.rs` 1 处——后两者为 additive 字段的编译驱动适配，无断言改动）。

### 12.4 证伪测试

生态 selector 半边（`tests/ecosystem_selector.rs`，10 passed + 特性门 1）：

| 用例 | 覆盖 | 结果 |
|---|---|---|
| `ecosystem_current_selector_pins_once_and_receipt_never_floats` | G4 平移：Current 钉一次；source 推进后同键 replay 由原回执应答（不再咨询 source）、新键钉新代际、双回执并存可审计 | PASS |
| `ecosystem_at_expectation_resolves_and_stale_generation_fences_typed` | At 精确代际解析；推进后 At(旧) 与 At(超前) 双向 ⇒ `StaleEcosystemGeneration{expected,current}` 逐字段；At(0) ⇒ `InvalidRequest` | PASS |
| `ecosystem_unknown_entity_fails_typed_notfound` | 未知实体 id ⇒ `EcosystemEntityNotFound{kind,entity_id}` typed miss，零 durable 行 | PASS |
| `ecosystem_kind_without_registered_source_fails_typed_unavailable` | 源未声明该 kind ⇒ `EcosystemSourceUnavailable`（fail-closed 不 panic，不咨询源，零行） | PASS |
| `ecosystem_source_failure_propagates_and_writes_nothing_durable` | source `Err` ⇒ `Source(e)` 传播 + 计数 0；恢复后同键正常解析 | PASS |
| `ecosystem_idempotent_replay_answers_original_and_rebind_conflicts` | 同键同 selector ⇒ Replayed 原回执；At(原代际) 形态 replay 等价；换实体/换 At 目标/换时间戳 ⇒ `IdempotencyConflict` | PASS |
| `ecosystem_receipts_survive_restart_and_reopen_stays_at_head` | 文件库 drop+reopen 回执逐位存活；user_version=6 头；人为降版本戳重迁移幂等收敛回 6 | PASS |
| `ecosystem_two_kinds_resolve_through_one_multi_kind_source` | 单一多 kind 源双实体独立解析（kind 回读正确、id 不同） | PASS |
| `ecosystem_verify_current_face_detects_stale_generation` | 显式新鲜度栅栏：未动 ⇒ Ok(原柄)；推进 ⇒ Stale{expected=柄,current=现}；实体消失 ⇒ `EcosystemEntityNotFound` | PASS |
| `ecosystem_unknown_kind_and_tampered_rows_fail_typed_closed` | raw INSERT kind=99 行 ⇒ 读回 `EcosystemKindUnknown(99)`；id 不再派生行 ⇒ `CorruptRecord`；写一次 trigger 拒 raw UPDATE/DELETE；合法行照常读 | PASS |
| `artifact_adapter::artifact_source_resolves_real_head_and_fences_on_advance`（`--features artifact-source`） | 真实面：真 `put_revision` CAS 推进 head，At(2) 解析钉 v2 digest；推进 v3 后 resolve+verify 双面 Stale；未知 id typed miss | PASS |

G3 三条件结构化（`tests/structured_conditions.rs`，5 passed）：

| 用例 | 覆盖 | 结果 |
|---|---|---|
| `structured_conditions_apply_validate_and_read_back_canonically` | 与 digest-only 同 revision 共存；读回 canonical（序规范化）；重启存活；revision 链仍可验证 | PASS |
| `conditions_digest_fold_is_bit_compatible_and_order_free` | 测试内复刻 pre-v6 公式：None 形态逐位相等（bit-compat 钉死）；Some 扩展 digest；同集异序 + 跨 plan 同 digest（序与 plan 身份均不入）；异界变 digest | PASS |
| `structured_conditions_invalid_forms_fail_typed` | 空集/重复/超界(257)/全零资源/零扇出 五形态 ⇒ `InvalidNodeConditions`，零 shape 行 | PASS |
| `frozen_node_conditions_rewrite_is_refused_typed` | 执行冻结后同集（异序）重声明保原 revision/digest；异集 ⇒ `FrozenNodeShapeRewrite`（G1 fence 覆盖条件维度） | PASS |
| `tampered_condition_bodies_fail_closed_on_decode` | 错 tag/截断/非 canonical 三体 ⇒ `CorruptRecord` 读回拒绝 | PASS |

真实面适配器（`crates/nlos-application/tests/selector_source.rs`，1 passed）：

| 用例 | 覆盖 | 结果 |
|---|---|---|
| `application_source_pins_install_generations_and_fences_on_reinstall` | 真验签包安装链（support fixture）：gen1 钉 manifest digest；重装 gen2 ⇒ 旧柄 verify 栅栏 + At(1) 栅栏 + 新 Current 钉 gen2；未知包 typed miss；未知回执 typed | PASS |

### 12.5 已知限制与 deferred minors（如实登记）

- **G3 三条件 enforcement 仍开放**（W31-G §8.2.2 后半，如实带入）：本 lane 交付 typed 声明面 + 结构验证；物化门对三条件的**消费/强制**（Namespace/Capability 权威裁决、Resource 权威 admission、fanout payer/grant 预留）随对应权威落地逐面接测试——「对应权威未落」的事实未变，变的是三条件从不可证伪的 digest 变为可验证的 typed 声明。
- **`[PLAN-DEPENDENCY-001]` kind 闭集递延台账（具名）**：**Topic**——实体在（`nlos-topic` TopicAuthority/TopicRecord）但无推进的 topic 级 generation（仅创建时 `channel_generation` 快照，不可推进 ⇒ fencing 空转；subscription/pattern generation 粒度不同），待 topic 头部 revision/generation 落地后入集；**Skill/Tool**——仓库无任何实体（全仓 grep 零符号；`nlos-capability` 仅 NamespaceId 前缀树目标，无 skill/tool 注册面）；**Model/ModelConfig**——无 crate 无类型；**外部服务**——仅 `nlos-service-directory` 内存 SABI PoC 快照（candidate generation 为调用方快照数据非权威代际；对快照解析即违反「不得把搜索结果当已授权依赖」），待 durable 外部服务权威落地。
- **At 仅栅栏形态**：`At(g)` 只匹配当前代际（失配双向 stale fail-closed）；**历史代际钉取递延**（artifact `inspect_revision`/application `list_installations` 具备按代读回，接入后 At 可钉历史）。
- **声明侧结构化绑定递延**：节点声明对生态依赖仍以 `input_selectors_digest` 摘要绑定；把 resolution handle 以 typed 输入选择器清单写入声明（替代摘要绑定）属物化门后续车道（本 lane 落解析半边 + 负路径）。
- **ResourceContractCondition 为声明合法子集**：固定三维镜像 nlos-resource W22-R `ResourceDemand` 词汇；总纲 ResourceContract 全字段（scheduling class/deadline policy/guarantee/overcommit 等）不入声明面——声明表达「请求上界」，裁决与 effective 面归 Resource 权威。
- **nlos-application/nlos-system-control 写集说明**：分别为薄扩展模块（沿既有依赖方向的新公共面 + 1 测试）与 1 行编译驱动机械适配（`conditions: None`），均为本 additive 字段的必要涟漪，非越权改写。
- **验证域**：macOS 单平台 debug/test profile；`nlos-system-control` 仅跑全量测试（129/0）未动其源码；kill-9/断电外推边界沿 §4 口径（本 lane 无新崩溃窗口面——解析事务为单事务原子，restart 测试覆盖 reopen 收敛）。

### 12.6 验证门（W36-P7 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt -p nlos-plan -p nlos-application -p nlos-system-control` | PASS |
| 全量测试（默认特性） | `cargo test -p nlos-plan` | PASS（16 target 全 ok，75 passed / 0 failed / 2 ignored；W31-F 60 基线零回归 + 15 新用例） |
| 全量测试（特性门） | `cargo test -p nlos-plan --features artifact-source` | PASS（76 passed / 0 failed / 2 ignored——含真实 ArtifactStore 适配器用例） |
| 依赖面测试 | `cargo test -p nlos-application` / `cargo test -p nlos-system-control` | PASS（86 / 0（W31-G 口径 60 之上为后续 lane 增量 + 本 lane 1 适配器用例）；129 / 0） |
| clippy | `cargo clippy -p nlos-plan -p nlos-application -p nlos-system-control --all-features --all-targets -- -D warnings` | PASS（0 warning；未使用 `chunks_exact`，CI 纪律） |
| workspace 编译 | `cargo check --workspace --all-targets` | PASS（可选特性不改变默认依赖图） |

### 12.7 未运行项（W36-P7，显式列出）

- **push 与 PR：未执行**——派工单 MUST NOT；由控制器统一执行（lane #8 `C-PLAN-HARDEN` 在本 lane 之后排队）。
- **`cargo test --workspace`：未运行**——派工单 MUST NOT（波次屏障由控制器收口；并行车道持 nlos-task/nlos-process 写集）。
- **三平台 CI / MSRV：未运行**——待 push 后 CI 触发。
- **release profile 复测：未运行**——本 lane 无 benchmark 声明，沿 §9 口径不涉及。

## 13. W36-P8：apply 侧 admission consult + PINNED overlay + 调度器规模探针（C-PLAN-HARDEN 移交#8）

> 对应：W31-G §8.2.4 / §8.2.6 / §8.2.7（调度器半边）；实现分支 `feat/w36-p8`（worktree `llmos-w36-p8`）。reclaim×residency 与 100K@50% 矩阵见 [B-TASK-SCALE-001](b-task-scale-001.md) §15。
>
> 状态：`PASS`（本切片范围，带 PINNED 台账边界）；未 push。

### 13.1 apply 侧 TaskNode 维 admission consult（§8.2.4）

生产声明面 `SqlitePlanAuthority::apply_plan_revision` 缺 consult 时 typed `DeclarationConsultUnavailable`（零 durable 行，禁止 silent pass）。consult 路径 `apply_plan_revision_with_admission` 在写入前 consult 店面级 projected `plan_nodes` 人口（既有行 + 本修订新 key）。Task 侧 `SqliteTaskAuthority::answer_plan_declaration` 只读回答 `max_task_nodes` 维。超档 typed `DeclarationAdmissionDenied` 且零 durable 行；consult 故障同样 fail-closed；幂等重放与无增长 reshape 绕过 consult。咨询无关旁路具名为 `apply_plan_revision_ungated`（测试/夹具专用）。

测试：`crates/nlos-plan/tests/apply_admission.rs` — 7 passed（默认 apply 无 consult fail-closed、deny 零副作用、replay 绕过、店面级累积、跨 plan 累积、consult 故障 fail-closed、gated reshape/growth）。

### 13.2 PINNED overlay 最小档（§8.2.6）

schema v7 additive：`plan_nodes.pinned` / `pin_transition_count` + 写一次 `plan_node_pin_transitions`。PINNED 是 5 级 residency 轴上的 overlay，**不是**第六 `NodeResidencyTier` discriminant（写集外 `ContextResidencyTier` 映射保持可编译）。驱逐方向 residency 走迁对已 pin 节点 typed `PinnedNodeNotEvictable`；unpin 是降级路径，之后 HOT→WARM 恢复。

测试：`crates/nlos-plan/tests/pinned.rs` — 4 passed。

**本切片明确不做的 `[SCALE-PIN-001]` 台账（不得声称已落）：**

- ResourceAllocation / owner 绑定
- pin-reason
- resident-bytes
- rebuild-cost
- expiry / renewal
- release / fence procedure
- SABI `ContextResidencyTier::Pinned` 线值
- 独立 bulkhead / 规模等级降级

### 13.3 调度器自身规模探针（§8.2.7 / 原 §11.5 缺口）

`tests/scheduler_scale_probe.rs`：独立节点 ready-FIFO select + 全窗 drive。默认套件 48 节点 smoke 每跑；10K `#[ignore]` 可复跑。

平台：macOS arm64，debug/test profile，`CARGO_TARGET_DIR=/tmp/nlos-w36-p8-target`。

```text
cargo test -p nlos-plan --test scheduler_scale_probe --offline -- --ignored --nocapture
W36-P8 scheduler scale probe (single platform): nodes=10000 apply_total=433.194542ms select_total=72.26775ms drive_total=11.815111291s selected=10000 approved=10000
```

未跑 100K 调度器扫描（§11.5 原登记的 N 次 `unresolved_dependencies` 吞吐仍以 10K 为可复跑上限）。release / 多平台未跑。
