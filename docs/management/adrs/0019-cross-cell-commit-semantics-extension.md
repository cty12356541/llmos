# ADR-0019：跨 Cell 提交语义——ADR-0013 扩展点开档

- 状态：`CANDIDATE`（W37 开档，非定案、非 VERIFIED；见决定 1）
- 日期：2026-09-22
- Owner：TaskAuthority / `C-SHARD`（编排入口：[stage-c-progress](../stage-c-progress.md) §C.3.1 / §C.5.2）
- 关联 Requirement：总纲 v0.5 §26.1 `[DIST-TASK-001]`/`[DIST-TASK-002]`/`[DIST-TASK-003]`/`[DIST-TASK-004]`；`[CONS-SCOPE-001]`（不建立跨 Cell 全局总序）；`[NLOS-DEFER-001]` / §29.2（全球联邦与跨组织清算延后但保留接口）；`[SEM-CHECKPOINT-001]`（分布式 View 用签名 vector/checkpoint，不得假设跨 Cell 全局标量 `log_seq`）
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
