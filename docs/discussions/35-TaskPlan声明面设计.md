# 35. TaskPlan/TaskNode 声明面设计（ROAD-B-004 剩余核心）

> 状态：讨论中（设计候选，未定案）
> 日期：2026-09-01
> 关联：v0.5 行 304-305、360-362、374、437、991（对象模型与 Task Space 职责）、行 3588-3658（§24.1.1 TaskPlan/TaskNode 原文与六条 PLAN-*）、行 4460-4509（§25.2 惰性物化与驻留分级）、行 4814（[PERF-SCALE-001]）、行 4881（[ROAD-B-004]）、行 3441-3506（§23.2 最小 Package Manifest）；[B-TASK-SCALE-001](../evidence/stage-b/b-task-scale-001.md)（ROAD-B-004 前片与缺口清单）；[B-SLICE-K-001](../evidence/stage-b/b-slice-k-001-end-to-end.md) 缺口 1（TaskSpec 无自由字段）；[B-APPLICATION-001](../evidence/stage-b/b-application-001-installation-authority.md)（manifest 最小子集）；[B-SCHEMA-015](../evidence/stage-b/b-schema-015-registry-freeze-marker.md) 与 [ADR-0014](../management/adrs/0014-schema-channel-freeze-v1-beta.md)（冻结纪律）；[ADR-0013](../management/adrs/0013-cross-authority-verify-then-commit-contract.md)（跨权威提交契约）；[议题 34](./34-fiber执行状态重建设计.md)（讨论格式与「ADR 定案前不实现」先例）

## 1. 问题定义

三源合流，构成本议题要解决的问题：

1. **ROAD-B-004 剩余**：v0.5 行 4881 要求单机发布 ScaleProfile 并完成 10K/100K logical TaskNode、不同 active working-set 比例、pressure/reclaim 与 checkpoint/rehydrate 基准。前片 [B-TASK-SCALE-001](../evidence/stage-b/b-task-scale-001.md) 只交付了 ScaleProfile 骨架与 10K 实跑，其 §4 缺口清单明确登记：TaskPlan/TaskNode 持久声明面、Dependency Resolver 未落地，`max_task_nodes` 以持久 Task 注册近似承载（行 19、42），ROAD-B-004 整体不可宣称达成。
2. **Slice K Task 模板桥缺口**：[B-SLICE-K-001](../evidence/stage-b/b-slice-k-001-end-to-end.md) 行 64 登记：`TaskSpec` 当前只有 `task_id/task_generation/registered_at_ms`，无可引用 `application_id` 或 manifest digest 的权威字段，Application↔Task 关联停留在 slice 编排层。[B-APPLICATION-001](../evidence/stage-b/b-application-001-installation-authority.md) 行 67-68 同步登记「安装不创建 Task」，行 78 把「Task 创建接线（消费 installation receipt 作为 Task 引导事实）」列为 Slice K 后半。
3. **规范已定、落点未定**：v0.5 行 3605-3643 已给出 TaskPlan/TaskNode schema 原文，行 3646-3658 给出六条 PLAN-* 不变量，行 4465-4499 给出六层规模区分与 TaskNode 状态机，行 991 给出 TaskPlan 服务操作面（propose/validate/revise/authorize/seal/get_graph/watch_node/request_materialize/request_evict）。行 5000-5002 状态表如实承认：规格化已完成，Planner/Dependency Resolver/惰性物化尚无实现。

与议题 31 §6 的关系需如实说明：Slice K 十二条证据门（议题 31 行 212-227）没有单列 plan 条目，与计划面最接近的是条 1（真实用户 Task 与冻结输入的 TaskSnapshot，行 216）；plan 相关缺口的登记位置是 B-SLICE-K-001 缺口 1 与进度单「Task 模板桥」后续项（第六十一增量，行 11）。因此本议题不是补 Slice K 门，而是为 ROAD-B-004 剩余核心开路，顺带关闭模板桥缺口。

**问题陈述**：为 TaskPlan proposal/revision 与 TaskNode durable metadata 选定权威落点，使 ScaleProfile 维度映射正规化、Dependency Resolver 的解析结果有处可挂、惰性物化门有 durable 事实可查；同时不破坏 ADR-0014 冻结纪律、不越既有权威写集边界、不改 v0.5 行文。

## 2. 已落地事实（候选对照锚点）

| # | 事实 | 出处 |
|---|---|---|
| F1 | `TaskSpec` 无自由字段，只有 `task_id/task_generation/registered_at_ms`；application↔task 关联停在 slice 编排层 | B-SLICE-K-001 行 64 |
| F2 | application 权威为最小子集：两表 + trigger 只做 installed/disabled 状态、代际 CAS 与七式 digest 绑定；只存 `package_manifest_digest`，不解析 manifest 内容；无 Task 创建接线 | B-APPLICATION-001 行 29-31、67-68、78 |
| F3 | scale 临时维度映射：`max_task_nodes` 以 Task 注册承载、`max_active_working_set` 以未结 CommitPermit 承载；名义 `TaskPlanId`/`TaskNodeId` 已存在但未绑定持久面 | B-TASK-SCALE-001 行 17-19、42 |
| F4 | permit 侧 key-scoped 惰性面已实证：`tasks.task_id` 主键、attempts/permits 的 `UNIQUE(task_id, idempotency_key)`、`commit_permits_single_active` 部分唯一索引，无全表扫描；10K 库 permit p95 391.291µs 不退化于 100 库基线 | B-TASK-SCALE-001 行 20、28、34 |
| F5 | 冻结纪律：nlos-schema REGISTRY 前 6 项 frozen、第 7 项（PrincipalHandshake）开放；golden 管字节、frozen 标记管注册表语义的双层防线 | B-SCHEMA-015 行 9、23-24 |
| F6 | 跨权威提交已有正式契约：verify-then-commit + 有界收敛（ADR-0013），slice-k/plan/converge 均按此组装 | 进度单第五十八增量；B-SLICE-K-001 行 23-29 |

## 3. 候选

| 候选 | 一句话 | 状态面落点 | 声明来源 |
|---|---|---|---|
| **A. 声明面入 TaskSpec 扩展** | TaskAuthority 扩为 task+plan 一体权威，plan/plan_node 表挂 `tasks.sqlite3` | nlos-task | 运行期 API/NL，同 A |
| **B. 独立 TaskPlan authority** | 新权威 `nlos-plan`（自有 SQLite + immutable revision + trigger），Task 侧仅持最小关联引用 | 新 crate | 运行期 API/NL，模板来源另接 C |
| **C. plan 存于 application manifest** | §23.2 manifest 扩 tasks 模板段，随包签名不可变，实例化出 TaskPlan proposal（Task 模板桥合并讨论） | 无（模板面）；运行期状态须叠加 A 或 B | signed manifest 模板 |
| **D. 声明面暂缓，仅落 Dependency Resolver 骨架** | 只做 resolver 组件层（typed selector 解析），不落持久声明面 | 无 | 无 |

### 3.1 候选 A：声明面入 TaskSpec 扩展

- **架构**：`TaskSpec` 增加计划关联与计划字段（至少 application 引用、plan digest/revision），plans、plan_nodes、依赖边表挂进 `tasks.sqlite3`；Dependency Resolver 作为 nlos-task 内部组件。
- **durable 面**：task schema additive 迁移续版；plan revision 事实与 Task/Attempt/Permit 同库同事务。
- **与 permit/惰性面的交互**：同库免跨权威，物化 gate 对 permit/attempt 状态的一致性检查是本地读；F4 的 key-scoped 索引模式可平移为 `plan_nodes(task_plan_id, task_node_id)` 主键加依赖索引。
- **迁移成本**：task schema 再迁移加注册路径触碰（F1 表明 TaskSpec 形状当前冻结为最小三字段，扩展即改既有绑定与 trigger 面）；既有 258 个测试的回归面在 nlos-task 全量。
- **被否风险**：
  1. v0.5 行 360-362 把 Task 与 TaskPlan 定为 `planned-as` 分离对象，行 374 [MODEL-PLAN-001] 强调计划不是已存在的进程。一体权威容易重演「Task 即计划」的旧耦合，计划修订（行 3648 要求新 revision/digest）与 10K 任务注册争同一单写者；B-TASK-SCALE-001 行 28 的 1.769ms/次注册数字是当前形状的实测，加 plan 表后需重测且写放大方向明确。
  2. B-TASK-SCALE-001 行 42 把「TaskPlan/TaskNode 持久声明面」登记为独立缺口，暗示其应为可独立演进的对象，而非 TaskSpec 的字段堆叠。

### 3.2 候选 B：独立 TaskPlan authority

- **架构**：新 crate `nlos-plan`，镜像 clock/topic/wait/application 的既成模式：自有 SQLite、域分隔 Id 派生、immutable plan revision receipt、trigger 守卫、幂等键。Dependency Resolver 是其授权消费组件；TaskAuthority 只持久最小关联引用，或完全经 ADR-0013 verify-then-commit 以 plan revision id 跨权威引用（见 §7.3）。
- **durable 面**：`plan_revisions`（immutable、digest 链，对齐行 3648「修订产生新 revision/digest、已执行节点保留原 revision」）、`plan_nodes`（durable metadata，对齐行 3642 状态机与行 4501 [SCALE-LOGICAL-001] 有界 metadata）、物化门事件（ELIGIBLE/WAITING_*/MATERIALIZING 迁移凭证）。
- **与 permit/惰性面的交互**：物化请求走 ADR-0013 契约（plan 权威出 readiness 事实，task/resource 权威出 permit 与 admission），与 slice-k 已验证的 verify-then-commit 组装方式同构（F6）；F4 证明 SQLite key-scoped 查询在 10K 规模不退化，plan 侧惰性查询面同构可得。
- **迁移成本**：新 crate 加跨权威提交接线；`SliceKRuntime` 组装器增一权威；ScaleProfile 维度改绑 plan_nodes 计数（正规化 F3）。
- **被否风险**：
  1. 过早固化：行 5002 承认 Planner/Resolver 闭环尚无实验，先立权威可能锁错形状；议题 34 先例是「ADR 定案前不进入实现」，本候选同样受此约束。
  2. fence 面扩大：物化涉及 plan/task/resource 三方权威，fence 顺序与故障矩阵成本高于同库方案 A。

### 3.3 候选 C：plan 声明存于 application manifest（Task 模板桥合并讨论）

- **架构**：§23.2 manifest（行 3443-3506）扩展 tasks 模板段（声明式 node、依赖、资源上界模板），随 package 签名不可变；安装或启动时模板实例化为 TaskPlan proposal。模板来源即 manifest digest，一并桥接 F1 缺的 application 关联。
- **durable 面**：模板面免费 durable（signed package 即事实，B-ARTIFACT-003 验签链已落地）；但运行期 revision、node 状态推进、resolver 解析结果仍需状态权威。C 只回答「声明从哪来」，不回答「状态放哪」，单独不成立，必须与 A 或 B 的状态面组合。
- **与 permit/惰性面的交互**：模板是静态声明；行 3652 [PLAN-DEPENDENCY-001] 要求 typed selector 在执行前解析为带版本/generation 的 handle，解析与授权动作仍在运行期。manifest 扩段即签名覆盖面变化：`llmos.package` 不在 ADR-0014 冻结 REGISTRY 内，但旧验签器遇到新段的行为需要定义，additive 纪律是否延伸到 package schema 是决策点（§7.1）。
- **迁移成本**：manifest schema 加验签 golden 扩展；B-APPLICATION-001 现状只存 digest（F2），解析面与 Task 创建接线需新增（该接线本就登记在其行 78）。
- **被否风险**：
  1. 行 3658 [PLAN-OVERRIDE-001]：自动与手工规划必须收敛同一 TaskPlan/TaskNode schema。模板若自成第二声明方言即造出绕行路径；模板必须编译为与 A/B 相同的 schema，只是来源不同。
  2. 静态模板覆盖不了 NL Intent 自动分解（行 3592-3602 管线入口是 TypedIntent，行 5000 承认尚无 Planner），宣称「模板即计划」会违反设计与事实分级。
  3. wire 面纪律风险见 §7.1，需显式决策而非默认扩展。

### 3.4 候选 D：声明面暂缓，仅落 Dependency Resolver 骨架

- **架构**：只实现 resolver 组件层（typed selector 输入、候选解析、结果类型），不落持久声明面；解析结果交给调用方。
- **durable 面**：无。
- **与 permit/惰性面的交互**：结果无权威可挂，重演 F1 的「关联停留在 slice 编排层」模式；F3 临时映射继续以代码注释存续。
- **迁移成本**：最小。
- **被否风险**：
  1. 行 4881 的 100K logical TaskNode 基准对象永远缺席，B-TASK-SCALE-001 行 42 缺口不关闭，ROAD-B-004 无法推进。
  2. 临时映射长期化会放大「把 Task 注册数字误读为 TaskNode 容量」的声明风险，与「没有 Evidence 不得把 DESIGN 改写为已实现」的纪律相抵触。
  3. 行 3652 要求解析结果成为「带版本/generation 的 handle」并可执行前核验；不 durable 则无审计事实，resolver 只剩纯函数价值。

## 4. 写集边界分析

| 权威/文件 | A | B | C | D | 备注 |
|---|---|---|---|---|---|
| `nlos-task`（schema/scale.rs/注册路径） | 重度 | 轻度（最小关联字段或零触碰；ScaleProfile 维度重绑） | 零 | 零 | tasks.sqlite3 schema 版本由单一 integrator 串行推进 |
| 新 crate `nlos-plan` | 无 | 独占 | 无 | 无 | 独立 Task/Attempt/写集 |
| `nlos-application` | 零 | 零 | 触碰（manifest 解析 + Task 创建接线） | 零 | 接线本登记于 B-APPLICATION-001 行 78 |
| `nlos-artifact` | 零 | 零 | 触碰（manifest 扩段 golden/验签） | 零 | 旧包兼容负路径必须测试 |
| `nlos-schema` REGISTRY | 共同注意项 | 同左 | 同左 | 同左 | plan 控制面若走 SABI 通道，additive 新条目以 `frozen: false` 起（B-SCHEMA-015 行 9 先例） |
| v0.5 / ADR / 进度单 | 不改 | 不改 | 不改 | 不改 | 本议题只做落点选择，规范已定；晋升时另开 ADR |
| docs/evidence | 实现车道落档 | 同左 | 同左 | 同左 | 数字指标在 ADR/工作包定，本议题不定 |

约束：任何候选不得修改 v0.5 行文；跨候选共享的 tasks.sqlite3 schema 迁移按 revision/CAS 思维更新，禁止 last-writer-wins。

## 5. 倾向与理由

**倾向 B 为主、C 作为模板来源的组合候选（两半各自可否决）；A 否决；D 仅作里程碑失败后的收缩路径。**

1. B 与仓库既成权威模式同构（clock/topic/wait/application 均为独立 authority + 域分隔 Id + immutable receipt + trigger 守卫），而声明面在规范中本来就是独立对象（行 360-362）。
2. F3 缺口的正规化要求 TaskNode 计数成为一等 durable 对象，B 直接满足；A 用字段堆叠近似，写放大与耦合风险明确。
3. C 解决「声明从哪来」并顺带关闭 Task 模板桥缺口（F1、F2），但其状态面仍由 B 承担；合并讨论合理，决策可分离。
4. D 无法推进 ROAD-B-004，只配作收缩态，不作为正选。

以上是倾向不是决策。议题 34 先例适用：任一候选落地前须先出 ADR（跨模块、难撤销），本议题状态停在讨论中。

## 6. 验收门草案（可证伪）

每门给出证伪条件；数字指标与命令在 ADR/工作包定案后由实现车道落 evidence，本议题只锁语义门。

| 门 | 语义 | 规范依据 | 证伪条件 |
|---|---|---|---|
| G1 revision 不可变 | 对已授权/已执行 plan 应用新 revision，旧节点 revision/digest 保持原值 | 行 3648 [PLAN-DAG-001] | 存在改写已执行节点 revision 的路径 |
| G2 惰性有界 | 100K METADATA_ONLY 节点登记后每节点 durable metadata 有上界、RSS 增量有界、零进程/模型会话/连接预占 | 行 4501 [SCALE-LOGICAL-001]、行 4814 [PERF-SCALE-001] | metadata 随节点数超线性，或未物化节点预占执行资源 |
| G3 物化门与窗口 | 仅依赖+授权+Namespace+ResourceContract+fanout gate 全满足的节点进入 MATERIALIZING；窗口收缩停止新物化并 checkpoint/evict | 行 3650 [PLAN-LAZY-001]、行 4503 [SCALE-MATERIALIZE-001] | 未满足依赖的节点可物化，或窗口收缩后仍新增物化 |
| G4 resolver 负路径 | `latest`/搜索结果/未解析 selector 不得被当作已授权依赖 | 行 3652 [PLAN-DEPENDENCY-001] | 存在绕过版本解析直达授权的路径 |
| G5 维度正规化 | ScaleProfile `max_task_nodes` 绑定 TaskNode 持久计数；10K 复跑对齐 B-TASK-SCALE-001 基线量级、100K 档 probe 可跑；Task 注册近似映射退役并在证据中显式注明 | B-TASK-SCALE-001 行 42 | 维度仍以 Task 注册近似，或新旧行为混写不注明 |
| G6 兼容与回归 | 既有 nlos-task 全量测试零回归；若采纳 C，旧签名包仍可验装、新段为 additive golden；若走 SABI，冻结条目 wire 零 diff | B-SCHEMA-015 行 19 硬门 | 任一回归或 wire diff |

## 7. 需用户决策清单

1. **manifest 是否扩 task 模板段**（候选 C 前提）：这是签名覆盖的 wire 面变化。`llmos.package` 不在 ADR-0014 冻结 REGISTRY 内，但需显式确认 additive 纪律是否延伸到 package schema 与验签 golden，即扩段是否触碰 schema 冻结纪律。不扩则 C 退化为「模板存于安装后 Application 数据」，缺口桥接方式改变。
2. **状态权威落点**：B（独立 `nlos-plan`）还是 A（并入 nlos-task）。若 B，TaskAuthority 是否仍加最小关联字段（`application_id`/plan revision 引用），这本身是一次 task schema additive 迁移。
3. **关联方式**：TaskSpec 加字段，还是完全经 ADR-0013 verify-then-commit 以 plan revision id 跨权威引用（零 TaskSpec 变更，代价是每次核验跨库）。
4. **ScaleProfile 语义**：`max_task_nodes` 正规化为 TaskNode 计数后，Task 注册是否保留为第二独立维度；TASK_PROFILE_10K 已发布数字的可比性如何处理（重发布新档或注明口径切换）。
5. **resolver 结果是否 durable**：依赖解析落 immutable receipt（可审计、可 fence）还是纯查询返回（轻、但 G4 的审计面弱化）。
6. **晋升路径**：确认组合（或其他）后是否直接开 ADR；ADR 定案前不进入实现（34 先例）。

## 8. 结论

不定案。规范面（v0.5 行 3588-3658、4460-4509）已定，本议题只解决落点：四候选已对照已落地事实（F1-F6）列明架构、durable 面、交互、迁移成本与被否风险；倾向 B 为主、C 为模板来源的组合。待 §7 六项决策后晋升 ADR，ADR 定案前 ROAD-B-004 剩余核心不进入实现。
