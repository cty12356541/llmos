# B-PLAN-001：nlos-plan 声明面状态权威骨架（TaskPlan/TaskNode state face）

状态：`PARTIAL_PASS`（**SKELETON——仅状态面**，2026-09-20，W28-A）

> 对应：[ADR-0016 决定 2](../../management/adrs/0016-task-plan-declaration-surface.md)（独立 `nlos-plan` authority）；[议题 35 §6](../../discussions/35-TaskPlan声明面设计.md) 验收门 G1；[进度单 §6.5.3](../../management/stage-b-progress.md) W28-A 车道行
>
> 实现：新 crate `crates/nlos-plan`（schema v1：`plans` / `plan_revisions` / `plan_nodes` / `plan_node_transitions` 四表 + 12 trigger）；workspace member 增补
>
> 范围纪律：本 lane 只落**状态权威落点**。manifest 模板面（W28-B）、Dependency Resolver（W29-B）、TaskSpec 关联字段与 ScaleProfile 维度重绑（W29-A）、物化门 ADR-0013 接线、100K benchmark（W31/G2）均不在本 evidence 声明范围。

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

- **SKELETON 范围**：仅状态面。无 manifest 模板编译入口（W28-B）、无 Resolver/解析回执（W29-B，决定 5）、无 TaskSpec 关联字段与 ScaleProfile 维度重绑（W29-A，决定 3/4——`max_task_nodes` 目前仍以 Task 注册近似承载，口径切换未发生）、无物化门/Resource/Operation 接线、无 IPC/CLI/SABI 面。
- **G1 fence 边界 = 物化边界**：「已授权」半边（`WAITING_AUTHORIZATION` 及之前）不在冻结集——授权事实属其他权威，经 ADR-0013 在物化门核验；若后续接线发现需要把授权回执本身落本权威，按新 revision 演进 schema，不回改 v1 语义。
- **历史 revision 节点集不可从行重建**：`plan_nodes` 是每节点单行（有界 metadata 姿态），历史 revision 的完整节点集仅由 `nodes_root`/digest 链见证（apply 时校验，之后由不可变 trigger 保真）；`verify_revision_chain` 验证链结构与存储 digest 公式，不重推导历史 nodes_root。完整逐 revision 形状审计（如取证需要）为后续 additive 表的 deferred 项。
- **pre-execution 节点同形重声明仍推进 `declared_revision`**：revision 是全量声明、节点随当前 revision 走（已测试钉死该语义）；副作用是飞行中迁移以 `StaleNodeRevision` 被拒（typed，非静默）。若未来要求「未变形不推进」，属行为决策变更，需同移测试钉死。
- **apply 无调用方 revision CAS**：单写者 + `BEGIN IMMEDIATE` 串行化下并发 revision 为「最后合法全量声明胜」（链式衔接，无静默改写）；`expected_current_revision` CAS 登记为 deferred minor。
- **结构性上界非规模声明**：100K/256 仅为拒绝无界声明；G2（惰性有界、RSS/metadata 上界、100K probe）属 W31，本 evidence 不作任何数字声明。
- 其余 deferred：无 golden DDL 文件（nlos-task golden_v*.sql 先例，schema 首次 churn 时补）；`plan_nodes` 无 `(plan_id, node_state)` 索引（读面仅按 node id，规模车道再评估）；`plan_digest` UNIQUE 依赖密码学抗碰撞性质。

## 6. 未运行项（显式列出）

- **push 与 PR：未执行**——按 W28-A 派工纪律，push 由控制器统一执行。
- **`cargo test --workspace`：未运行**——派工单 MUST NOT 条款（波次屏障由控制器收口）；本 crate 全量 + workspace `cargo metadata` 成员解析已验证。
- **三平台 CI / MSRV：未运行**——待 push 后 CI 触发。
- **TS/Py conformance、schema 生成物检查：未运行**——零 `schema/`/`gen/` 写集触碰，无触发面。
- **G2/G5 数字指标：未运行**——W31 车道。
