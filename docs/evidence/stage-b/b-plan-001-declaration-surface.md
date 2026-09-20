# B-PLAN-001：nlos-plan 声明面状态权威骨架（TaskPlan/TaskNode state face）+ Dependency Resolver + Context Residency 分级

状态：`PARTIAL_PASS`（**W28-A 状态面骨架 + W29-B Dependency Resolver + W31-E Context residency 分级最小版**，2026-09-20）

> 对应：[ADR-0016 决定 2](../../management/adrs/0016-task-plan-declaration-surface.md)（独立 `nlos-plan` authority）与 [决定 5](../../management/adrs/0016-task-plan-declaration-surface.md)（Resolver 结果 durable）；[议题 35 §6](../../discussions/35-TaskPlan声明面设计.md) 验收门 G1（§2–§6，W28-A）与 G4（§7，W29-B）；[进度单 §6.5.3](../../management/stage-b-progress.md) W28-A / W29-B / W31-E 车道行
>
> 实现：crate `crates/nlos-plan`（schema v1：`plans` / `plan_revisions` / `plan_nodes` / `plan_node_transitions` 四表 + 12 trigger；schema v2 additive：`plan_revision_nodes` / `plan_revision_edges` / `plan_resolution_receipts` 三表 + 8 trigger；schema v3 additive：`plan_node_residency_transitions` 一表 + `plan_nodes` 两列 + 4 trigger）
>
> 范围纪律：W28-A 只落**状态权威落点**（§1–§6）；W29-B 只落 **Dependency Resolver**（§7，B4-3）；W31-E 只落 **Context residency 分级最小版**（§8，B4-5）。manifest 模板面（W28-B）、TaskSpec 关联字段与 ScaleProfile 维度重绑（W29-A）、物化门 ADR-0013 接线、100K benchmark（W31/G2）均不在本 evidence 声明范围。

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
- **结构性上界非规模声明**：100K/256 仅为拒绝无界声明；G2（惰性有界、RSS/metadata 上界、100K probe）属 W31，本 evidence 不作任何数字声明。
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
- **G2/G5 数字指标、100K benchmark、working-set 比例矩阵：未运行**——W31-B/W31-D 车道。
