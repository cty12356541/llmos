# ADR-0019：跨 Cell 提交语义——ADR-0013 扩展点开档

- 状态：`ACCEPTED`（DESIGN 级定案，2026-10-06：修订 1 经独立审查门 pass-with-fixes、必修 F1/F3 与建议 F2/F4 已修；授权链=用户 2026-10-05"按推荐裁定/不中途请示"+2026-10-06"开始裁断/能继续一直继续"；`VERIFIED` 仍留 C-SHARD 实现证据，派发门不变）
- 日期：2026-09-22
- Owner：TaskAuthority / `C-SHARD`（编排入口：[stage-c-progress](../stage-c-progress.md) §C.3.1 / §C.5.2）
- 关联 Requirement：总纲 v0.5 §26.1 `[DIST-TASK-001]`/`[DIST-TASK-002]`/`[DIST-TASK-003]`/`[DIST-TASK-004]`；`[CONS-SCOPE-001]`（不建立跨 Cell 全局总序）；`[NLOS-DEFER-001]` / §29.2（全球联邦与跨组织清算延后但保留接口）；`[SEM-CHECKPOINT-001]`（分布式 View 用签名 vector/checkpoint，不得假设跨 Cell 全局标量 `log_seq`） 本节只定「读已提交前缀」这一可见性通道；`[CONS-SCOPE-001]` 同句允许的 durable outbox/saga 等跨 shard 传播机制不在本节排除或收窄范围，其跨 Cell 形状同属 `C-SHARD`/后续定案。
- 关联工作包：`C-SHARD`（本 ADR 的 VERIFIED 是其派发前置）；`C-FANOUT`（`DIST-TASK-003` 层级 fanout 主写）；`C-MIGRATE`（federation 机制面验收边界，决策点 5）；`C-CELL`（决策点 1 拓扑首片）；拓扑 / 共识基底 ADR（W37 并行开档，写集不相交，本文件不占用 `0018`）
- 决策来源：
  - [ADR-0013](./0013-cross-authority-verify-then-commit-contract.md) 决定 3 与复审触发器「Stage C 跨机提交语义定案」——本文件是该扩展点的开档载体，**不改写** ADR-0013 已 ACCEPTED 的单机契约
  - [stage-c-progress §C.5.4](../stage-c-progress.md) 决策点 1 / 4 / 5（2026-09-22 裁定生效；否决窗口留至 W36 屏障）
  - [stage-c-progress §C.5.2](../stage-c-progress.md) 依赖链与 §C.5.3 W39 槽位
- 证据：无新实现证据。单机半边仍是阶段 B 既有 Evidence（schema v27–v38 表组族；[RISK-B-11](../exit-unknown-risks.md) / U-5 跨机半边显式未证）。本 ADR 为 DESIGN 级开档，不得把任何跨 Cell 提交能力写成已实现
- 复审触发器：见文末

## 上下文

[ADR-0013](./0013-cross-authority-verify-then-commit-contract.md) 把 verify-then-commit + 有界收敛升格为**单机**跨 authority 提交的正式契约，并在决定 3 登记 Stage C 扩展点：跨机（跨 Cell）提交语义不在该契约范围内；扩展点登记为 reconciliation 协调（蜂窝式权威 §26.1 偏向调和而非全局共识），届时经新 ADR 定案。复审触发器之一即「Stage C 跨机提交语义定案」。

阶段 C 把该扩展点落到工作包 `C-SHARD`：[stage-c-progress §C.3.1](../stage-c-progress.md) 将其定义为「TaskAuthority 分片与跨 Cell 接管（单机表组族向 DIST-TASK-001..004 全语义迁移）」，状态 `NOT_STARTED`，前置为「跨 Cell 提交语义 ADR，决策点 4」。§C.5.2 依赖链写明：`C-SHARD` 的信息前置是「跨 Cell 提交语义 ADR = 决策点 4 前置，ADR-0013 复审触发器」。§C.5.3 原排 W39 才「ADR-0013 扩展点定案 → `C-SHARD`」；§C.5.4 决策点 4 裁 (b) **W37 随拓扑 ADR 并行开档**，目的是早启降低 W39 串行等待，**纪律不变**：`C-SHARD` 派发仍以本 ADR 的 `VERIFIED` 为前置。

本文件只做开档：把已裁定约束与规范锚点写入独立 ADR 身份，供后续定案 / VERIFIED 与 `C-SHARD` 引用。不实现 `C-SHARD` / DIST-TASK 表，不发明提交协议，不把开档写成定案。

单机迁移起点已在阶段 B 落地、构成 C 的迁移基础（非重做）。[stage-c-progress §C.1.1](../stage-c-progress.md) 逐项列举：TaskAuthority lease/term/fencing、`FROZEN_FOR_TAKEOVER`、per-endpoint barrier、exact-fence manifest、successor registry、cross-term adoption（stage-b-progress §4.85–§4.102 表组族，schema v27–v38）；统一恢复面双域自动收敛；ADR-0017 三域 coordinator。跨机 / 跨 Cell 半边 = RISK-B-11 显式未证域。§C.1.2 第 6 条：单机结论不外推——跨机 / 多 Cell 原子提交、跨 Cell barrier、远端物理 cleanup 在 C 证据落地前不可声称。

## 已裁定约束（只记录，不发明）

以下条目均有权威出处。本开档把它们收成 ADR 身份，不增加新规范句。

### 1. 派发门：开档 ≠ 解锁 `C-SHARD`

[§C.5.4 决策点 4](../stage-c-progress.md)：裁 (b) W37 随拓扑 ADR 并行开档（早启降低 W39 串行等待；**`C-SHARD` 派发仍以其 VERIFIED 为前置，纪律不变**）。

[§C.4](../stage-c-progress.md) 派发纪律 (1)：入度以 **VERIFIED**（定向门通过 + 审查 clean）计零，仅 completed 不触发解锁。同节 (4)：依赖未决结论的车道不得提前。

因此：本文件进入仓库、状态为 `CANDIDATE`、甚至日后写为 `ACCEPTED`，都**不足以**派发 `C-SHARD`。`C-SHARD` 仍保持 `NOT_STARTED`，直到本 ADR 达到 `VERIFIED`。W37 开档不改变 W39 的实现门槛。

### 2. 首片拓扑：跨机语义后置

[§C.5.4 决策点 1](../stage-c-progress.md)：裁 (a) **单机多进程 Cell×2 起步**（迭代最快、分区可注入、回退成本最低；**跨机语义后置到有单机双 Cell 证据后**）。子议题：共识基底 ADR 于 W37 拓扑 ADR 并行开档（先以控制面单写者 + lease/fencing 覆盖语义，由该 ADR 定是否 / 何时引入共识基底）。

本 ADR 可以、且只应当**指定扩展点**（跨 Cell 提交如何从 ADR-0013 单机契约伸出），**不要求**第一实现切片具备多机。多机 / 跨机器原子性仍属 RISK-B-11 / U-5，不得因本开档而声称。共识基底不在本文件决定范围（与拓扑 ADR 写集不相交）。

### 3. federation 读法：C 交付机制面，不是全球组织清算

[§C.5.4 决策点 5](../stage-c-progress.md)：确认草案读法——**C 交付机制面**（跨 Cell 名称 / 服务发现、迁移 intent、审计 checkpoint），**全球跨组织清算延后**（v0.5 §29.2 / `[NLOS-DEFER-001]`）；`C-MIGRATE` 验收边界据此。

[§C.1.2 第 2 条](../stage-c-progress.md) 已指出：§28.3 的「federation」交付项与 §29.2 延后项存在规范内部张力，由决策点 5 收口。本 ADR 引用该读法，避免把跨 Cell 提交语义写成全球联邦或跨组织清算。`[NLOS-DEFER-001]`：deferred 不等于 forbidden，不得从对象模型删除接口。

控制面七职责中与本读法对齐的三项（v0.5 §26.1）：placement 与 migration intent；跨 Cell 名称与服务发现；reconciliation 和审计 checkpoint。这是机制面，不是清算面。

### 4. 迁移基线：单机 schema v27–v38，不是重做

[§C.5.3 W39](../stage-c-progress.md)：`C-SHARD`（DIST-TASK-001..004 迁移；**单机 schema v27–v38 表组族为基础**）。

阶段 B 已落地、作为迁移起点的表组族（stage-b-progress §4.85–§4.102，只列已有事实）：

| schema | 已落地事实（单机） |
|---|---|
| v27 | durable lease / term / fencing primitive |
| v28 | opt-in CommitPermit / terminal lease binding |
| v29 | same-term adoption / reconcile lease guard |
| v30 | 本地 immutable `FROZEN_FOR_TAKEOVER` fence receipt + exact local root |
| v31 | lease-bound 本地 TaskAuthorityAssignment baseline |
| v32 | pending TakeoverReceipt prefix；旧 assignment 置 `TakeoverPending` |
| v33 | per-endpoint barrier observation |
| v34 | canonical exact-fence member manifest |
| v35 | barrier observation digest 持久化 |
| v36 | barrier observation principal 签名列 |
| v37 | takeover 完成 + successor assignment 激活 |
| v38 | 独立 immutable `task_cross_term_adoption_receipts` |

`C-SHARD` 的工作是把上述单机表组族迁向 §26.1 对象模型 + `DIST-TASK-001..004` 全语义，而不是另起一套提交表。本开档不设计、不冻结下一顺位 schema，也不改写 v27–v38。

### 5. 规范锚点：`DIST-TASK-001..004`（誊录范围，不改写）

权威正文在 [v0.5 §26.1](../../design/06-架构设计总纲-v0.5.md)。本 ADR 只固定引用，不重写不变量。对象模型三 Record 已在规范中给出：`TaskAuthorityParticipantRegistry`、`TaskAuthorityAssignment`、`TaskAuthorityTakeoverReceipt`。

工作包切分按进度单原文并列记录，避免把 fanout 并进本 ADR：

| Requirement | 进度单归属 | 与跨 Cell 提交的关系（规范已写明的部分） |
|---|---|---|
| `DIST-TASK-001` | `C-SHARD`（§C.3.1 列 001/002/004） | 每 Task generation 唯一 durable Assignment + ParticipantRegistry；worker MAY 跨 Cell，但 canonical `TaskControlRecord` / `TaskHead` / `CommitPermit` 只能由该 term 的 authority 提交 |
| `DIST-TASK-002` | `C-SHARD` | 迁移先同一线性化 CAS：旧 assignment → `TAKEOVER_PENDING`，registry → `FROZEN_FOR_TAKEOVER`，截取 generation/root；`exact_fence_set_root`；逐 endpoint barrier Receipt 覆盖后才能激活新 assignment；不可 fence 则等待旧权威过期并 QUARANTINE，不能抢先接管 |
| `DIST-TASK-004` | `C-SHARD` | 新 assignment 从已验证 takeover Receipt 初始化 registry baseline 并递增 generation；禁止原地解冻旧 root |
| `DIST-TASK-003` | `C-FANOUT` 主写（§C.3.1）；W39 行把 001..004 一并写在 `C-SHARD` 迁移句 | 大 TaskGroup 用层级子组 / 局部 reducer / Merkle result root；本开档不把 fanout 协议定为提交语义 |

跨 shard / 跨 Cell 一致性底线已由 `[CONS-SCOPE-001]` 写明：LINEARIZABLE / SERIALIZABLE 必须声明 authority object / shard / transaction domain；**v0.5 不建立跨 Cell 的全局总序**；跨 shard 使用 causal / vector checkpoint、durable outbox / saga 和显式 `PARTIAL` / `UNCERTAIN`。这与 ADR-0013 决定 3「reconciliation 而非全局共识」同向，本开档不改口。

## 候选

本开档**不新增**协议候选。下列三项均已出现在权威对象中，此处只做索引，避免把开档误读成一次协议选型。

| 候选 | 出处 | 本开档立场 |
|---|---|---|
| A. 单机 verify-then-commit + 有界收敛 | ADR-0013 决定 1（已 ACCEPTED） | 单机范围内继续有效；本 ADR 不 SUPERSEDE |
| B. 扩展点 = reconciliation 协调（蜂窝式权威，非全局共识） | ADR-0013 决定 3；§26.1 控制面「reconciliation 和审计 checkpoint」；`[CONS-SCOPE-001]` | **本开档继承该扩展点登记**；具体跨 Cell 消息 / checkpoint / saga 形状留待定案，不在此发明 |
| C. 协议级 2PC / 跨权威强原子 | ADR-0013 候选 C（阶段 B 否决）；ADR-0013 复审触发器「出现必须跨权威强原子的新需求时，经新 ADR 评估 2PC/reconciliation 混合」 | 本开档不采纳、也不关闭该复审门；未出现新的强原子需求陈述 |

否决（已有记录，不重开）：单机 SQLite ATTACH 跨库共享事务（ADR-0013 候选 B）；把全球联邦 / 跨组织清算当作阶段 C 提交语义（决策点 5 + §29.2）。

## 决定

本开档可写下的只有已裁定条目。未完成定案、无 PoC、无 VERIFIED，故状态保持 `CANDIDATE`（ADR 模板：未完成 PoC 时不得写为 `ACCEPTED`）。

1. **身份**：本文件是 ADR-0013 决定 3 所登记「跨机（跨 Cell）提交语义」扩展点的独立 ADR，编号 `0019`（`0018` 留给并行拓扑 / 共识基底 ADR，本车道不占用、不编辑）。不改写 ADR-0013 正文，不把单机契约标 SUPERSEDED。
2. **派发**：`C-SHARD` 仍不得派发，直到本 ADR `VERIFIED`。W37 开档只降低 W39 等待，不改变 §C.4 入度规则。
3. **首片范围**：本 ADR 指定扩展点即可；第一实现切片不要求多机。跨机语义后置于单机双 Cell 证据之后（决策点 1）。
4. **语义对象**：跨 Cell 提交语义的规范对象是 `DIST-TASK-001..004` + §26.1 三 Record；实现迁移基线是单机 schema v27–v38 表组族。本文件不设计新表、不实现 `C-SHARD`。
5. **federation 边界**：阶段 C 只交付机制面（名称 / 发现、migration intent、审计 checkpoint），不交付全球跨组织清算（决策点 5）。该边界由 `C-MIGRATE` 验收；本 ADR 引用以免提交语义越界。
6. **诚实声明**：开档不等于定案。ADR-0013 的复审触发器「Stage C 跨机提交语义定案」与 ADR-0017 复审触发器 4「Stage C 跨机 reconciliation 经新 ADR 定案」均**尚未触发关闭**。RISK-B-11 / U-5 仍为 open。

## 后果与退出策略

- **正面**：扩展点获得独立 ADR 身份，可与拓扑 ADR 并行推进；`C-SHARD` 的信息依赖从「决策点 4 未开档」变为「ADR-0019 待 VERIFIED」，W39 不再从零起草。
- **负面 / 债务**：开档期间不得把 `CANDIDATE` 当 `VERIFIED` 解锁实现；L0 索引（[README](../README.md) ADR 列表、stage-c-progress §C.7）属共享 canonical，本车道写集不含它们，由 integrator 在屏障合并，避免 last-writer-wins。
- **非目标（本文件）**：不实现 DIST-TASK 表或 `C-SHARD` 代码；不定共识基底；不改 ADR-0013 / 0017 已接受决定；不声称跨 Cell 原子提交、跨 Cell barrier 或远端物理 cleanup。
- **退出策略**：§C.5.4 否决窗口内决策点 4 被单点否决 → 本 ADR 标 `REJECTED` 或回退 `QUESTION`，`C-SHARD` 继续 `NOT_STARTED`。定案证伪（出现必须跨权威强原子、或 reconciliation 无法从 durable prefix 有界收敛）→ 按 ADR-0013 复审门重开，评估 2PC / reconciliation 混合；单机 ADR-0013 继续有效，除非另文显式 SUPERSEDED。

## 验证与证据

本开档无实现、无测试、无 schema 迁移。验证范围仅限文档自身：

- 编号未与现网 `0000`–`0017` 冲突；不创建 / 编辑 `0018`
- 正文只引用 ADR-0013、v0.5 §26.1 / `[CONS-SCOPE-001]` / §29.2、stage-c-progress §C.5.2 / §C.5.4、schema v27–v38 已有事实
- 未改 `stage-*-progress.md`、SDD `progress.md`、crates

`C-SHARD` 的实现证据、H6 分布式故障注入与 RISK-B-11 / U-5 退役，一律在本 ADR `VERIFIED` 之后由实现车道落档，开档阶段不得预写为已满足。

## 复审触发器

1. 本 ADR 申请从 `CANDIDATE` 升 `ACCEPTED` / `VERIFIED`（W39 定案或更早，若证据先到）——必须另有审查 + 定向门，不得用本开档提交冒充。
2. 决策点 4 在 W36 否决窗口被否决 → 本 ADR 状态回退，受影响波次重排（§C.5.4 裁定方式原文）。
3. 单机双 Cell 证据齐备、需要把跨机语义从「后置」推进到本 ADR 正文定案时，additive 修订或后继 ADR，禁止 last-writer-wins 改写已记录约束。
4. 出现必须跨权威强原子的新需求 → 联动 ADR-0013 复审，评估 2PC / reconciliation 混合。
5. 跨机 reconciliation 定案时 → 联动 ADR-0017 复审触发器 4，复审三域 coordinator 边界；本开档本身不是该定案。
6. 发现跨 Cell 崩溃窗口无法从 durable prefix 收敛到唯一终态 → 登记 conflict、降级相关 Claim，并联动重开 ADR-0013。

---

## 修订 1（定案草案）：跨 Cell 提交语义 = reconciliation 接管（W52-draft，未生效）

- 日期：2026-10-06
- 性质：**additive 修订**。本节只追加，上文正文（含状态行）一字不改；上文与本节冲突时以上文为准，并按上文复审触发器重开。本节是上文复审触发器 1 / 3 所指的「定案申请稿」：按触发器 1，晋升必须另有独立审查 + 定向门——本草案自身不生效、不翻状态行。
- 决策来源：[stage-c-progress](../stage-c-progress.md) 2026-10-05 设计门裁断 ①（「ADR-0019/C-SHARD：维持 CANDIDATE，证据先行……证据到再定案」）→ 2026-10-06 W50-L2 段（发现 + intent 两面落地，登记「ADR-0019 的定案前置（单机双 Cell 证据）自此具备评估基础……建议：补 reconciliation checkpoint 面后再评」）→ W51 段（第三面落地，登记「§26.1 federation 机制面三项（发现/intent/checkpoint）自此齐备——ADR-0019 定案评估进入可调度状态」）
- 证据：全部为既有实现证据（R1.1 逐条引用，含测试名与提交号）；本修订为 DESIGN 级定案草案，不引入新实现、测试或 schema 义务

### R1.1 证据基础与边界

上文决定 3 与复审触发器 3 把跨 Cell 语义定案前置到「单机双 Cell 证据」（承 [§C.5.4 决策点 1](../stage-c-progress.md)「跨机语义后置到有单机双 Cell 证据后」）。该前置由两波机制面落地满足：

| §26.1 控制面职责（上文决定 5 的机制面读法） | 实现（`crates/nlos-cell/src/federation.rs`） | 接线（`crates/nlos-slice-k/src/cell_host.rs`，opt-in、缺省零侵入） | 双进程实证 |
|---|---|---|---|
| 跨 Cell 名称与服务发现 | `CellDirectory`：`<root>/cells/<hex>.cell` 原子替换发布 + `(boot, heartbeat)` 单调守卫；`snapshot` / `find` / `live_cells` 读面 | `CellHost::open_federated`（开即注册，heartbeat 1）/ `refresh_cell_registration`（心跳） | `dual_cell_hosts_discover_each_other_and_enumerate_one_shared_migration_intent`：两 OS 进程各持一 CellHost，互相发现、按名查找、活窗内双活 |
| placement 与 migration intent | `MigrationIntent` 不可变文件追加（source→target、object、fenced generation、reason） | `record_migration_intent`（非联邦 host `Ok(None)`） | 同一测试：双侧枚举同一 intent，全轴一致 |
| reconciliation 和审计 checkpoint | `CheckpointFact` / `CheckpointRecord`（fence 三轴 + 摘要；按 Cell 文件名前缀枚举，`(recorded_ms, pid, sequence)` 排序） | `record_reconciliation_checkpoint`（当前 fence 快照 + 摘要） | `dual_cell_checkpoint_registered_by_child_enumerated_by_parent`：child 登记、parent 枚举全轴一致、双侧 trail 互不串 |

提交：W50-L2 `9b15fda`（nlos-cell 机制面）+ `d4a54c0`（slice-k 接线与双进程测试；merge `7a5e0be`；台账登记 `b895de5`）；W51-L1 `489e06f` + `2e856d1`（merge `0067832`；台账登记 `0338f36`，即本修订基线）。

**证据边界（显式声明，不因定案放松）**：

1. 上述全部证据是**单机双进程**（共享文件系统、共享内核、共享时钟域；[ADR-0018](./0018-single-host-multiprocess-dual-cell-topology.md) 拓扑），**非跨机、非网络分区、非跨时钟域**——两个测试的边界声明原文均写明引证为跨机证据即违 RISK-B-11。[RISK-B-11](../exit-unknown-risks.md) / U-5 不因本修订退役。
2. 证据止于**登记与枚举**：intent 登记不执行（执行归 `C-MIGRATE`）、checkpoint 登记不验证（`federation.rs` 模块文档显式：核对摘要属延后的 reconciliation 执行者）。没有任何跨 Cell 接管、跨 Cell barrier 或跨 Cell 提交被实现或被测试——上文「验证与证据」节的诚实声明继续有效。
3. 因此本修订与 [ADR-0017](./0017-resource-operation-cross-authority-prepare-finalize.md) 同型：**基于既有证据的设计定案（DESIGN 级）**，不把任何跨 Cell 提交能力写成已实现；`C-SHARD` 派发门（上文决定 2）不变。

### R1.2 跨 Cell 提交语义定案

在上方「已裁定约束」全部条目内，把 [ADR-0013](./0013-cross-authority-verify-then-commit-contract.md) 决定 3 登记的扩展点写出落地形状。本节不新增 Requirement、不发明协议对象、不设计新表、不改写 v27–v38。

#### R1.2.1 定性：跨 Cell 提交 = reconciliation 协调，非全局共识、非 2PC

跨 Cell 提交**不是**跨 Cell 原子提交。ADR-0013 决定 3 已把扩展点登记为 reconciliation 协调（「蜂窝式权威 §26.1 偏向调和而非全局共识」），`[CONS-SCOPE-001]`（[v0.5](../../design/06-架构设计总纲-v0.5.md) 行 4338）已定不建立跨 Cell 的全局总序；本修订给出的落地形状是：

**一个 Task 的 canonical 提交路径永远只在一个 Cell 的一个 authority term 内闭合；「跨 Cell」改变的是哪个 Cell 持有该 term（reconciliation 接管），以及他 Cell 如何被仲裁地观察到已提交前缀（checkpoint trail 可见性）——不是把提交本身分布化。**依据：`[DIST-TASK-001]`（v0.5 行 4774）「worker MAY 跨 Cell，但 canonical `TaskControlRecord` / `TaskHead` / `CommitPermit` 只能由该 term 的 authority 提交」。

#### R1.2.2 权威归属与传递：接管请求权 ≠ 接管权

**归属**：canonical authority 恒在单 Cell——每 Task generation 唯一 durable Assignment（`[DIST-TASK-001]`）。他 Cell 持有的至多是发现快照与审计事实（R1.2.5 三面），一律不是权威。他 Cell 的 worker 至多按 `[DIST-TASK-002]`（行 4776）尾句在未过期 TaskSnapshot/lease 内做纯计算，无有效 assignment/CommitPermit 时不得发布 canonical output 或启动新不可逆 effect。

**传递**：他 Cell 对该 Task 的全部合法跨 Cell 权威动作是**接管请求权**——本修订对既有机制面组合的命名，不是新增权威通道（DIST-TASK-002 尾句允许的未过期 lease 内纯计算不在此列，因其不产生任何跨 Cell 可见状态）：

1. 经发现面定位对象与其当前权威 Cell（directory：`find` / `live_cells`）；
2. 在登记面登记接管请求（intent：source→target、对象、其 fenced generation、理由摘要；不可变追加，登记不执行）；
3. 在观察面读旧权威的 checkpoint trail，确定接管后的收敛可从哪个 durable 前缀起步。

凭以上登记与观察不产生任何权威：`[DIST-LOCAL-001]`（行 4768）仍绑定 Cell 只能使用已授权、未过期、可本地证明的能力；发现面的活窗读与注册快照均被实现显式声明为发现启发式 / discovery snapshot，非 fencing oracle。

**接管执行**只有一条通道：既有 `[DIST-TASK-002]` 同一线性化 CAS 链——旧 assignment → `TAKEOVER_PENDING`、registry → `FROZEN_FOR_TAKEOVER` 并截取 generation/root、`exact_fence_set_root` 固定 fence 集、逐 endpoint barrier Receipt 覆盖该 exact root 后，才能提交 TakeoverReceipt 并激活新 assignment；新 assignment 从已验证 Receipt 初始化 registry baseline 并递增 generation（`[DIST-TASK-004]`，行 4778）。该链与 registry 更新、permit freeze、operation admission 共享 TaskControlRecord 的线性化顺序（`[DIST-TASK-004]`），其 home 即该 term 的权威；跨 Cell 驱动这条 CAS 的传输形状（消息 / 共享存储）属 `C-SHARD` 实现细节，本修订不发明。语义基线即上文「已裁定约束 4」表列的单机 v27–v38 表组族（v30 冻结回执与 exact local root / v32 pending TakeoverReceipt 前缀与旧 assignment 置 `TakeoverPending` / v33–v36 逐 endpoint barrier + 摘要 + principal 签名列 / v37 successor 激活 / v38 跨 term adoption receipt）：跨 Cell 语义**借用**该族 takeover/fence/barrier 语义，`C-SHARD` 负责向 §26.1 三 Record 迁移，不另起一套提交表（约束 4 原文）。

#### R1.2.3 可见性规则：跨 Cell 读走 checkpoint trail

- 他 Cell 读另一 Cell 的已提交状态，走该 Cell 的 **checkpoint trail**（按 Cell 前缀、`(recorded_ms, pid, sequence)` 排序的不可变追加日志）。每 Cell 一条 trail，结构上即不存在跨 Cell 全局标量序——与 `[SEM-CHECKPOINT-001]`（行 2808：「分布式 View MUST 使用签名 vector/checkpoint，不得假设存在跨 Cell 的全局标量 `log_seq`」）同构。
- **签名 / 摘要素后置于 `C-SHARD` 实现**：现行 `CheckpointFact` 携带无签名的自由摘要轴（机制面显式「登记不验证」）；§26.1 三 Record 的 `authority` / `signature` 字段与 v36 barrier principal 签名列是 `C-SHARD` 的迁移义务，不是本修订的新发明。
- **未覆盖窗口的诚实语义**：trail 未覆盖的最新提交前缀对他 Cell 不可声称——跨 Cell 读结果必须可声明 `PARTIAL` / `UNCERTAIN`（`[CONS-SCOPE-001]` 行 4338 原文），不得把「trail 末端」当作「全局现在时」。

#### R1.2.4 失败模式：不抢先 + 有界收敛

- **不可 fence 不抢先**：任一 endpoint 不可 fence 时，只能等待其旧 authority/permit/lease 权威过期并保持 outstanding effect QUARANTINED，不能抢先接管；旧 Cell 的 result、message、permit、callback、effect 和 commit 必须拒绝（`[DIST-TASK-002]` 行 4776 原文誊引——跨 Cell 场景按原文执行，本修订不设例外条款）。
- **checkpoint 缺口的有界收敛**：接管完成点与旧权威最后一个 checkpoint 之间的提交前缀缺口，由 reconciliation 从各 authority 的 durable prefix 收敛到唯一终态——这是 ADR-0013 决定 1 有界收敛（「崩溃窗口由公开 `converge_pending` 从 durable prefix replay 收敛」）的跨 Cell 推广；契约不变量照搬（崩溃窗口收敛性、无幻影行、无双重提交、replay 逐字节幂等），不新增、不放松。若出现无法从 durable prefix 收敛到唯一终态的跨 Cell 崩溃窗口，上文复审触发器 6 已显式登记处置路径。

#### R1.2.5 与机制面三项的接口：发现面 / 登记面 / 观察面，皆非 fencing oracle

| 机制面 | 在本语义中的角色 | 显式不是什么（实现已声明的边界） |
|---|---|---|
| `CellDirectory` | 接管请求的**发现面**：定位对象、旧权威 Cell、存活启发式 | 非 fencing oracle——`live_cells` 新鲜度不授予也不撤销任何权威 |
| `MigrationIntent` | 接管请求的**登记面**：谁向谁、对哪个对象、哪个 fenced generation 提出请求 | 非执行面——登记不触发 DIST-TASK-002 CAS（执行归 `C-MIGRATE` / `C-SHARD` 车道） |
| `ReconciliationCheckpoint` | 审计与收敛的**观察面**：旧权威声称已收敛到的 durable 前缀；审计与接管后收敛从此起步 | 非验证面——登记不核对摘要（核对属延后的 reconciliation 执行者） |

三面皆不铸 fencing token：**fence 只在 Cell 内的 `CellAuthority`**（epoch/fencing token 的唯一推进入口是 quota 族的 `advance_epoch_and_quarantine`——此为 `cell_host.rs` 装配声明，该名非 v0.5 规范句；隔离语义背景见 `[LEASE-LOSS-001]`（v0.5 行 2327）；见 `cell_host.rs` 装配声明）。接管请求权不等于接管权，权威迁移只有 R1.2.2 的 CAS 链一条。

### R1.3 状态建议

- **`CANDIDATE` → `ACCEPTED`（定案）**。理由：上文复审触发器 3 的前置「单机双 Cell 证据齐备」的**证据事实**已由台账登记（2026-10-06 W51 段原文：「§26.1 federation 机制面三项（发现/intent/checkpoint）自此齐备——ADR-0019 定案评估进入可调度状态」；台账显式把「是否齐备到可定案」留给下一次裁断）；该裁断即本次独立审查 + 定向门，本修订是触发器 3 预期的 additive 修订申请路径。晋升提交须记录裁断与授权来源（同 ADR-0017/0018 状态行先例），并同步 L0 索引（management/README.md ADR-0019 状态行、stage-c-progress 台账登记），沿上文后果节的 integrator 屏障合并纪律。先例与边界同 ADR-0017：既有证据上的 DESIGN 级 ACCEPTED，无新实现声称。按上文触发器 1，晋升必须另有独立审查 + 定向门，不得以本草案提交冒充；审查通过后由 canonical 提交把状态行翻为 `ACCEPTED`。
- **`VERIFIED` 不预写**。留给 `C-SHARD` 实现证据：H6 分布式故障注入（[§C.5.5](../stage-c-progress.md) 门 1 类目：分区/重启/迁移/重复消息、stale epoch/分区双主等）+ RISK-B-11 / U-5 退役。**`ACCEPTED` 不解锁 `C-SHARD`**——上文决定 2 的派发门以 VERIFIED 为前置，本修订不触碰。
- **生效即联动**：本修订升 `ACCEPTED` 即构成上文触发器 5 的「跨机 reconciliation 定案」，联动 ADR-0017 复审触发器 4（三域 coordinator 边界复审）与 ADR-0013 复审触发器「Stage C 跨机提交语义定案」。本修订选择的形状是 reconciliation（非 2PC），两处复审的落档由各自复审留痕，不由本修订代答。

### R1.4 复审触发器（增量）

上文文末 6 条全部沿用、一字不改。新增：

7. **跨机证据再评估门**：R1.2 全部语义在单机双 Cell 拓扑（ADR-0018）上定案。首个真跨机证据（跨机双 Cell 拓扑、或真网络分区 / 跨时钟域注入）出现时必须再评估：发现面的墙钟新鲜度启发式（`heartbeat_ms` / `recorded_ms` 在跨时钟域的语义）、checkpoint trail 的 `(recorded_ms, pid, sequence)` 轴与 `[SEM-CHECKPOINT-001]` 签名 vector 的对齐、「等待旧权威过期」在真分区下的时限语义；若届时出现必须跨权威强原子的新需求，按上文触发器 4 / ADR-0013 复审门评估 2PC / reconciliation 混合。

---

## 修订 2（授权裁定落地）：Phase 0 证据车道豁免与 VERIFIED 范围界定（2026-10-08，生效）

- 日期：2026-10-08
- 性质：**授权修订（非纯 additive）**。本节由维护者 2026-10-08 立项裁断直接授权，对上文**指名条款做显式替代**（先例：[ADR-0023](./0023-semantic-write-side-driver-bridge.md) 修订 1 前提更正显式作废原表述）。未指名的上文条款一字不改、继续有效；修订 1 的 R1.x 全部有效，本节在其上追加界定。本修订**不翻状态行**（仍 `ACCEPTED`）。
- 动机：上文决定 2（「`C-SHARD` 不得派发，直到本 ADR `VERIFIED`」）与 R1.3（「`VERIFIED` 留给实现证据」）构成循环前置；仅靠车道命名（「证据车道」而非「`C-SHARD` 车道」）无法消除上文「验证与证据」节的显式时间限制。维护者裁断：两阶段方向通过，但必须先修订本 ADR 消除循环并界定 VERIFIED 范围，未证残域全部保留。

### R2.1 循环消除：Phase 0 证据车道豁免（显式替代）

上文「验证与证据」节末段原句：

> 「`C-SHARD` 的实现证据、H6 分布式故障注入与 RISK-B-11 / U-5 退役，一律在本 ADR `VERIFIED` **之后**由实现车道落档，开档阶段不得预写为已满足。」

其中的**时间限制**（「在本 ADR `VERIFIED` 之后」）由本节替代为：

> 允许**有界的 Phase 0 证据车道**（登记名「ADR-0019 VERIFIED 证据面」，沿 W50-L2/W51 台账先例）在本 ADR `VERIFIED` 之前，为满足本 ADR 的 `VERIFIED` 晋升而产出 H6 故障注入证据、跨 Cell 接管 PoC 与收敛核证据并落档。豁免范围**仅此**。

原句的诚实条款**保留并强化**：上述落档证据不得预写为「已满足 `VERIFIED`」；`VERIFIED` 晋升仍必须走上文触发器 1（独立审查 + 定向门），晋升前一切证据只是候选事实。

同时**保留不改**（一字不动）：上文决定 2 与 R1.3 的 `C-SHARD` 工作包本体派发门——`C-SHARD`（[stage-c-progress §C.3.1](../stage-c-progress.md) 行）在本 ADR `VERIFIED` 前保持 `NOT_STARTED`；Phase 1 迁移义务（§26.1 三 Record + `DIST-TASK-001..004` 全语义）仍以 `VERIFIED` 为前置。

### R2.2 VERIFIED 范围界定（拓扑、协议覆盖、注入类目、验收条件）

`VERIFIED` 晋升的效力域显式界定如下；超出域的证据不自动满足任何条件：

1. **拓扑域**：[ADR-0018](./0018-single-host-multiprocess-dual-cell-topology.md) 单机多进程 Cell×2（共享内核、时钟与文件系统）。跨机、跨时钟域、真网络分区**不在**效力域内——该残域显式保留，挂 R1.4 触发器 7 再评估门。
2. **协议覆盖域**：R1.2 已定案语义中，`VERIFIED` 验收覆盖——R1.2.2 接管执行唯一通道（`DIST-TASK-002` CAS 链跨 Cell 驱动 + `DIST-TASK-004` baseline 初始化与 generation 递增）；R1.2.3 checkpoint trail 可见性（未覆盖窗口 `PARTIAL` / `UNCERTAIN`）；R1.2.4 失败语义（不抢先 + QUARANTINE + 从 durable prefix 有界收敛，ADR-0013 决定 1 契约不变量照搬）。
3. **H6 注入类目**（C-SHARD 域子集，§C.5.5 门 1 全类目的其余三项属 C-FANOUT/C-BENCH）：分区、重启、迁移（= authority 接管）、重复消息、stale epoch、分区双主。**方法学约束**：分区与分区双主类目必须在**两进程保持存活**的条件下阻断通信 / 续约来注入——kill 子进程只证明崩溃处理，不构成分区语义证据。
4. **验收条件**（全部满足方可晋升）：① 独立审查门 pass（触发器 1 路径）；② Phase 0 测试套件定向复跑全绿；③ Evidence 落档 stage-c 域 + `evidence-index.yaml` 收录 + `lint_claims.py` 绿（`EVID-SCOPE-001` 单 claim 单证据范围）；④ RISK-B-11 / U-5 **分域退役登记**完成且全部未证残域显式在册（口径见 R2.3）。

### R2.3 风险分域退役口径

1. **`partial` 退役不自动满足 `VERIFIED`**：RISK-B-11 在 risks.yaml 的状态调整只是退役登记动作；`VERIFIED` 验收看 R2.2 第 4 条整体，其中风险处置 = 已证域证据齐 **且** 未证残域全部显式保留在册。
2. **「物理 cleanup」界定（勘误性澄清）**：「旧 Cell 效果拒绝 / QUARANTINE」只证明**拒绝语义**（`DIST-TASK-002` 尾句），**不构成** U-5 所指「远端物理 cleanup 证明」。物理 cleanup 残域保留至存在实际清理证据（旧权威 outstanding effect 的物理清除 / 对账完成证明）为止，不随 Phase 0 一并退役。
3. **endpoint 覆盖逐个声明**：Phase 0 证据覆盖的 participant endpoint 集以 Evidence 为准**逐 endpoint 声明**；未覆盖的 endpoint 不得宣称闭环，留 Phase 1（CS1-C 六类映射）补证。
4. **跨机残域**：U-5 跨机器原子性半边按裁定挂 R1.4 触发器 7，终局属后续跨机拓扑决策（ADR-0018 复审触发器 2/4 路径），不在 Phase 0/Phase 1 范围。

### R2.4 授权链与关联

- 授权链：维护者 2026-10-08 立项裁断——「接受两阶段与分域退役原则，先修订 ADR 消除循环并明确验证范围；保留所有未证残域。接受保守命名映射及离线 Wave 1 节奏」+ CS-0/CS-1 补条件（CS-0：证据车道命名不替代显式条款；CS-1：partial 不自动满足 VERIFIED、「拒绝旧效果/QUARANTINE」不等同「物理 cleanup 证明」）+ 分区双主验收修正（双进程存活注入）+「先完成文档登记，仍不派发实施，实施按既定授权边界另行确认」。
- 执行计划：Phase 0（证据车道族）/ Phase 1（C-SHARD 本体）车道框架见 [c-shard-plan.md](../c-shard-plan.md)（2026-10-08 附条件采纳；其裁定登记与本节同步落档）。
- 三 Record 命名：保守映射层（CS-2 裁定）——保留现有 `Authority*` 类型与表名，映射表须列字段 / 状态 / 不变量对应**与缺口**；名称对齐不代替语义实现。终裁留 Phase 1 CS1-A 设计门。

### R2.5 复审触发器（增量）

上文文末 6 条与 R1.4 第 7 条全部沿用、一字不改。新增：

8. **Phase 0 证伪门**：Phase 0 证据出现 R2.2 范围无法容纳的语义缺口（如 CAS 链在双进程拓扑上不可驱动、或收敛不变量失败、或分区双主在双存活注入下无法仲裁单胜者）→ 停止 `VERIFIED` 晋升，按上文触发器 6 处置并重开本修订。
