# ADR-0020：控制面单写者覆盖与共识基底引入时序

- 状态：`CANDIDATE`（W37 开档；无 PoC，不得写 `ACCEPTED` / `VERIFIED`；见决定 1）
- 日期：2026-09-22
- Owner：控制面单写者 / 共识基底时序（编排入口：[stage-c-progress](../stage-c-progress.md) §C.5.4 决策点 1 子议题）
- 关联 Requirement：总纲 v0.5 §26.1 控制面职责与 `TaskAuthorityAssignment.consensus_commit_index` / `TaskAuthorityTakeoverReceipt.consensus_commit_index`；`[PHIL-TRANSFER-002]`（使用 consensus 名称不得冒充已获共识保证）；`[CONS-SCOPE-001]`（不建立跨 Cell 全局总序）；`[TASK-SCALE-002]`（禁止用同源 Agent 复制伪造共识）；§12 `LEASE-*`（尤其 `[LEASE-FENCE-001]` / `[LEASE-ENFORCE-001]` / `[LEASE-LOSS-001]`）；§28.3 多节点 identity/epoch/fencing
- 关联工作包：**不解锁** `C-CELL`（实现派发仍以并行 [ADR-0018](./0018-single-host-multiprocess-dual-cell-topology.md) `VERIFIED` 为前置）；**不解锁** `C-SHARD`（仍以并行 [ADR-0019](./0019-cross-cell-commit-semantics-extension.md) `VERIFIED` 为前置）；`C-LEASE` 仍按 §12 / §C.5.3 W38，本文件不实现三族 lease
- 决策来源：
  - [stage-c-progress §C.5.4](../stage-c-progress.md) 决策点 1 子议题（2026-09-22 裁定生效；否决窗口留至 W36 屏障）
  - 草案原文 `053f523` §C.5.4 第 1 条附带：`consensus_commit_index` 隐含共识 / 日志基底，但 v0.5 未定共识算法
  - [ADR-0013](./0013-cross-authority-verify-then-commit-contract.md) 决定 3（跨 Cell 扩展点 = reconciliation，非全局共识）
  - 拓扑半边只读引用 ADR-0018（本文件不重写）；跨 Cell 提交半边只读引用 ADR-0019（本文件不定提交协议）
- 证据：无新实现证据、无共识库 / Raft PoC。本 ADR 为 DESIGN 级开档，只记录已裁定覆盖语义与「是否 / 何时」时序，不得把任何共识协议写成已实现
- 复审触发器：见文末

## 上下文

[v0.5 §26.1](../../design/06-架构设计总纲-v0.5.md) 在 `TaskAuthorityAssignment` 与 `TaskAuthorityTakeoverReceipt` 上给出字段 `consensus_commit_index`，但**未定共识算法、未定日志实现、未定复制组**。控制面职责原文是 Principal/key 与 policy、Package/Application trust、quota lease / epoch / fencing、placement 与 migration intent、跨 Cell 名称与服务发现、reconciliation 和审计 checkpoint——其中没有「必须落地 Raft」。

草案（`053f523` §C.5.4 第 1 条）把该字段标为子议题：对象模型隐含共识 / 日志基底，v0.5 未定算法；**是否需要 ADR 定基底，或先以控制面单写者 + lease/fencing 覆盖语义，必须随决策点 1 裁定**。2026-09-22 裁定把拓扑半边裁为 (a) 单机多进程 Cell×2（归 ADR-0018），并把本子议题写成：**共识基底 ADR 于 W37 与拓扑 ADR 并行开档；先以控制面单写者 + lease/fencing 覆盖语义，由本 ADR 定是否 / 何时引入共识基底**。

本文件只开拓扑之上的**控制面写者模型与共识基底时序**。不重写 ADR-0018 的最小拓扑，不定 ADR-0019 的跨 Cell 提交协议，不实现 Raft / 共识库，不派发 `C-CELL` / `C-SHARD`。

历史 rationale 不得覆盖当前规范：

- [议题 15](../../discussions/15-分布式内核.md) 曾推荐蜂窝式控制面「Raft 式复制」、否决 Raft-per-blackboard 与完全去中心化 P2P 共识。该文是 L2 讨论，只能解释为何对象模型留了 `consensus_commit_index` 这个名称；[AGENTS.md](../../../AGENTS.md) 规则 3：历史文档不能覆盖 v0.5。`[PHIL-TRANSFER-002]` 禁止因使用 consensus 名称就声称已获共识保证。
- [ADR-0002](./0002-stage-b-sqlite-operation-authority.md) 已写明：进程内 Mutex 是单 authority 的 writer admission gate，**不能冒充跨主机共识或分布式权威**。阶段 B 单写者纪律只覆盖本 authority store。
- [ADR-0013](./0013-cross-authority-verify-then-commit-contract.md) 决定 3：跨机 / 跨 Cell 提交扩展点是 reconciliation，不是全局共识。`[CONS-SCOPE-001]`：v0.5 不建立跨 Cell 全局总序。即使未来引入控制面共识基底，也不得把该基底解释成跨 Cell 全局总序。

## 已裁定约束（只记录，不发明）

以下条目均有权威出处。本开档把它们收成独立 ADR 身份，不增加新规范句。

### 1. 覆盖语义先于共识库

[§C.5.4 决策点 1](../stage-c-progress.md) 子议题原文：**先以控制面单写者 + lease/fencing 覆盖语义，ADR 定是否 / 何时引入共识基底。**

「覆盖」的含义按草案 `053f523` 与裁定行合读：`consensus_commit_index` 在引入共识协议之前，由**唯一控制面写者**在 lease / epoch / fencing 下给出单调游标语义，而不是先选 Raft（或任何共识库）再实现字段。本 ADR 开档本身不是实现该字段。

### 2. 跨机语义后置

同一决策点 1：跨机语义后置到**已有单机双 Cell 证据之后**。拓扑选择在 ADR-0018（单机多进程 Cell×2）。本 ADR 不得把控制面单写者写成跨主机 quorum，也不得把本机 lease/fencing 证据写成跨机共识证据（RISK-B-11 / [U-5](../exit-unknown-risks.md) 仍 open）。

### 3. 本 ADR 不解锁实现车道

- `C-CELL`：§C.3.1 / ADR-0018 决定 5——实现派发以 **ADR-0018 `VERIFIED`** 计零，不是本文件。
- `C-SHARD`：§C.5.4 决策点 4 / ADR-0019——派发以 **ADR-0019 `VERIFIED`** 为前置，纪律不变。
- `C-LEASE`：§C.5.3 W38，权威仍是 §12 三族 `LEASE-*`。本文件引用 lease/fencing 只作为控制面覆盖语义，不提前实现或改写 `C-LEASE`。

因此：本文件进入仓库、状态为 `CANDIDATE`、甚至日后升 `ACCEPTED`，都**不足以**派发 `C-CELL` 或 `C-SHARD`。

### 4. 拓扑与提交语义各有主人

- 最小拓扑、进程边界、分区可注入：ADR-0018。本文件不重开 (a)/(b)/(c)，不改写 Cell 本地七件套。
- 跨 Cell 提交扩展点（ADR-0013 决定 3）：ADR-0019。本文件不定 2PC / saga / checkpoint 形状。
- 共识基底是否 / 何时引入：本文件。ADR-0018 决定 4 已显式把该决定面留给本 ADR。

## 约束

1. 禁止在此发明共识算法、复制组、crate 切分、IPC 形状或 schema 迁移。`consensus_commit_index` 保持规范字段，本开档不赋予协议语义。
2. 禁止把议题 15 的「Raft 复制」提升为当前义务；禁止把阶段 B 单 authority Mutex 外推为控制面共识。
3. 禁止改写 ADR-0013 / 0017 / 0018 / 0019 已记录决定。
4. 跨机 / 多控制面写者 / 真实网络分区不在本开档的声称范围。
5. `[TASK-SCALE-002]`：不得通过复制海量同源 Agent 伪造共识或 quorum。

## 候选

草案子议题给出的是**覆盖顺序**，不是算法选型。本开档不新增协议候选，只索引已有出处，避免把开档误读成一次 Raft 选型。

| 候选 | 内容 | 出处 | 本开档立场 |
|---|---|---|---|
| **S-B（现阶段采纳）** | 先以控制面单写者 + lease/fencing 覆盖 `consensus_commit_index` 语义；不引入共识协议 / 库 | `053f523` 子议题另一臂；§C.5.4 决策点 1 裁定原文 | **现阶段覆盖**。字段解释为单写者控制面下的单调游标 / fencing 序号，不是已提交的共识日志下标 |
| S-A（现阶段否决） | 开档即选定并引入共识基底（Raft 或其他库） | 议题 15 Q1 落地形态曾写「Raft 复制」；草案「是否需要 ADR 定基底」一臂 | **不作为现阶段引入**。议题 15 不能覆盖 v0.5；无 PoC；`[PHIL-TRANSFER-002]` 禁止名称冒充保证。本否决不是对议题 15 哲学的永久推翻 |
| S-C（不采纳为永久否决） | 永远不引入共识基底，字段永远只是本地计数器 | 无权威出处要求永久关闭 | 不写成永不。跨机证据或单写者被证伪后按复审门重开 |

已有记录、不重开：

- Raft-per-blackboard（议题 15 已否：v1 不需要每个黑板一个共识组）。
- 完全去中心化 P2P 内核共识（议题 15 候选 C，未采纳）。
- 用 Agent / Process 数量冒充 ControlDomain quorum（`[TASK-SCALE-002]`）。
- 把 ADR-0013 单机 verify-then-commit 换成跨权威 2PC（那是 ADR-0019 / ADR-0013 复审门，不是本文件）。

## 评价标准

按决策点 1 的最低风险读法，本阶段只比较「现在是否引入基底」，不比较具体算法：

| 维度 | 权重 | S-B（先覆盖） | S-A（现在引入） |
|---|---|---|---|
| 与裁定原文一致 | 高 | 即裁定子议题 | 把「是否 / 何时」提前做成实现选型 |
| 迭代 / 回退 | 高 | 不引入新协议面；回退 = 停在单写者 | 在尚无单机双 Cell 证据时摊开共识库 |
| `[PHIL-TRANSFER-002]` 诚实性 | 高 | 名称保留、保证不冒充 | 易把字段名写成已获共识 |
| 跨机 / 分区双主（ROAD-C-002） | 后置 | 本机用 epoch/lease/fencing 表达双主拒绝；跨机后置 | 过早用 quorum 语言覆盖未证域 |
| 议题 15 控制面可复制的历史意图 | 后置 | 保留复审门，不删除字段 | 把 L2 推荐当成当前实现义务 |

## 决定

未完成定案、无 PoC、无 VERIFIED，故状态保持 `CANDIDATE`（ADR 模板：未完成 PoC 时不得写为 `ACCEPTED`）。下列条目只记录已裁定覆盖与时序，不选定算法。

1. **身份。** 本文件是 §C.5.4 决策点 1 子议题「共识基底是否 / 何时引入」的独立 ADR，编号 `0020`（`0018` = 拓扑，`0019` = 跨 Cell 提交；本车道不创建、不编辑二者）。不重写 ADR-0018，不定 ADR-0019。
2. **现阶段覆盖（是否 = 现在不引入）。** 采纳 S-B：控制面权威对象（§26.1 控制面职责所列）在引入共识基底之前，以**单写者**提交；写者身份由 **lease + epoch + fencing token** 约束（`[LEASE-FENCE-001]` / `[LEASE-ENFORCE-001]`：fencing 在声明的 `fence_scope` 内全序单调；失联额度走 `[LEASE-LOSS-001]` QUARANTINED，不得并行双发）。`consensus_commit_index` 在此覆盖下只表示该单写者控制面的单调提交游标，**不是**共识协议日志的 commit index，也不是跨 Cell 全局序号（`[CONS-SCOPE-001]`）。
3. **何时。** 不在本波实现共识库或复制组。引入共识基底的最早复审点是：**已有单机双 Cell 证据之后**（决策点 1 跨机后置），或复审触发器成立（单写者被证伪、fencing 无法单调、或 ROAD-C-002 的分区双主无法用 epoch/lease 表达）。届时经本 ADR 升格或后继 ADR 选型；**现在不定算法、不定 crate、不定实现波次槽位**。
4. **派发。** 本 ADR 不解锁 `C-CELL`，不解锁 `C-SHARD`。`C-CELL` 仍等 ADR-0018 `VERIFIED`；`C-SHARD` 仍等 ADR-0019 `VERIFIED`。本文件即使日后 `ACCEPTED`，入度仍不以本 ADR 计零去派发那两条实现车道。
5. **范围。** 本 ADR 不实现 Cell 运行时、不改 crate、不改 schema、不实现 `C-LEASE`。lease/fencing 在此是覆盖语言，实现归属仍按进度单原车道。
6. **诚实声明。** 开档不等于定案。议题 15 的「控制面 Raft 复制」仍是未兑现的历史推荐，不是本文件的实现承诺。RISK-B-11 / U-5 仍为 open。

## 后果

- **正面：** `consensus_commit_index` 不再处于「名称像共识、规范未定算法」的无主状态；W37 可与 ADR-0018 并行推进控制面语义，而不把 Raft 选型塞进拓扑 ADR，也不把提交协议塞进本文件。
- **负面 / 债务：** 开档期间不得把 `CANDIDATE` 当 `VERIFIED` 解锁实现；单写者在 ADR-0018 的双进程拓扑上仍共享主机故障域，本覆盖不能冒充跨机 HA。L0 索引（[README](../README.md) ADR 列表、stage-c-progress §C.7）属共享 canonical，本车道写集不含它们，由 integrator 在屏障合并（[PD-CREATE-001](../project-knowledge-progressive-disclosure.md)）。
- **运维责任：** 在共识基底引入前，控制面必须能指出当前唯一写者及其 lease/epoch；旧写者复活必须被 fencing 拒绝。不得用第二 Cell 进程各写一份控制面权威状态冒充「已分布」。
- **非目标（本文件）：** 不实现 Raft/共识库；不重写 ADR-0018 拓扑；不定 ADR-0019 提交语义；不声称跨机 quorum、跨 Cell 全局总序或远端物理 cleanup。

## 退出策略

1. **否决窗口内回滚（W36 屏障前）：** 维护者单点否决决策点 1 子议题 → 本 ADR 标 `REJECTED` 或回退 `QUESTION`；因本开档无代码，无 crate 回滚。
2. **收缩覆盖：** 若单写者 + lease/fencing 被证伪，按复审触发器重开，评估是否必须提前引入共识基底；在后继 ADR 定案前，不得继续引用本文件的覆盖解释去声称控制面线性化。
3. **升级引入基底：** 单机双 Cell Evidence 齐备且复审认为控制面需要复制时，另开或升格本 ADR，届时才选型（Raft 只是议题 15 的历史候选之一，不是预绑定）。字段名保持，协议语义另文赋予。
4. **预估成本：** 文档级回滚为低；本 ADR 未引入 schema 或依赖，退出无数据迁移。

## 验证与证据

本开档无实现、无测试、无共识 PoC、无 schema 迁移。验证范围仅限文档自身：

- 编号：现网 `0000`–`0017` 已占用；并行拓扑车道占用 `0018`；并行跨 Cell 车道占用 `0019`。本文件用 `0020`
- 正文只引用 §C.5.4 决策点 1、`053f523` 草案子议题、v0.5 §26.1 / `[PHIL-TRANSFER-002]` / `[CONS-SCOPE-001]` / §12 `LEASE-*`、ADR-0013 决定 3、ADR-0002 单写者诚实边界、议题 15（rationale only）
- 未改 `0013` / `0018` / `0019`、`stage-*-progress.md`、SDD `progress.md`、crates

`C-CELL` / `C-LEASE` / 共识库的实现证据一律在各自权威对象 `VERIFIED` 且本 ADR 若升格之后由实现车道落档；开档阶段不得预写为已满足。

## 复审触发器

1. 本 ADR 申请从 `CANDIDATE` 升 `ACCEPTED` / `VERIFIED`——必须另有审查；不得用本开档提交冒充。无 PoC 不得升 `ACCEPTED`。
2. 决策点 1 在 W36 否决窗口被否决 → 本 ADR 状态回退，受影响波次重排。
3. 首份单机双 Cell Evidence 落地 → 复审「是否 / 何时」引入共识基底；跨机语义仍不得从本覆盖外推。
4. 控制面单写者被证伪（双进程同时写入权威控制面、fencing 无法在 `fence_scope` 内单调、旧写者复活被接受）→ 重开，评估 S-A。
5. ROAD-C-001 / ROAD-C-002 评审要求真实多机控制面 HA 或把 `consensus_commit_index` 解释为协议日志 → 必须另文选型，不得扩写本开档的 Claim 范围。
6. ADR-0018 或 ADR-0019 被否 / 改写决定面 → 复审本文件与二者的边界，禁止 last-writer-wins 抢写拓扑或提交语义。
