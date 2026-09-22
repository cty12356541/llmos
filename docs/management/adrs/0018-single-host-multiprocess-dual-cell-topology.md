# ADR-0018：多 Cell 拓扑 PoC 采用单机多进程 Cell×2 起步

- 状态：ACCEPTED（控制器按 2026-09-22 §C.5.4 决策点 1 代定，否决窗口留至 W36 屏障；维护者可单点否决回滚本条并重排受影响波次。本文为 DESIGN 级定案草案，供维护者复审；ACCEPTED ≠ VERIFIED，不得据此声称 `C-CELL` 已实现）
- 日期：2026-09-22
- Owner：`C-CELL`（阶段 C 蜂窝式权威前片）
- 关联 Requirement：总纲 v0.5 §26.1 蜂窝式权威（Cell 本地七件套 + 控制面职责；`[DIST-LOCAL-001]`、`[DIST-FAIL-001]`、`[DIST-NAME-001]`、`[DIST-TASK-001..004]`）；§28.3 阶段 C「多节点 identity/epoch/fencing」；`[PHIL-TRANSFER-002]`（使用 consensus 名称不得冒充已获共识保证）
- 关联工作包：`C-CELL`（本 ADR 解锁其实现派发的信息依赖）；下游只读引用：`C-LEASE`、`C-SHARD`、`C-SCHED`、`C-PARTITION`、`C-BENCH`（依赖链见 [stage-c-progress §C.5.2](../stage-c-progress.md)）
- 决策来源：[stage-c-progress §C.5.4](../stage-c-progress.md) 决策点 1（2026-09-22 裁定生效）。草案候选原文见 `053f523` §C.5.4 第 1 条；裁定读法为控制器按草案默认 / 最低风险代定（ADR-0017 模式）
- 证据：无新实现证据。本 ADR 只把已裁定的 PoC 拓扑顺序与最小拓扑落档为 DESIGN；`C-CELL` 代码与 H6 分区证据由后续实现车道落档后方可声称
- 复审日期：W36 屏障（否决窗口关闭点）；其后随首份单机双 Cell Evidence 与并行共识基底 ADR 复审
- 复审触发器：见文末五项

## 上下文

[v0.5 §26.1](../../design/06-架构设计总纲-v0.5.md) 已定蜂窝式权威的**职责分工**，未定阶段 C 的 **PoC 拓扑顺序与最小拓扑**。规范留白处不得发明新对象模型，只允许在编排决策点给候选（[stage-c-progress](../stage-c-progress.md) 开篇纪律）。本 ADR 只记录决策点 1 的拓扑半边，不改写 §26.1 行文。

Cell 本地七件套（§26.1 原文）：Process supervisor；capability/name cache；Resource lease 子账本；Driver gateway；durable event/outbox；Artifact cache；failure detector。控制面职责（§26.1 原文）：Principal/key 与 policy；Package/Application trust；quota lease、epoch 和 fencing；placement 与 migration intent；跨 Cell 名称与服务发现；reconciliation 和审计 checkpoint。

不变量本 ADR 不得破坏：

- `[DIST-LOCAL-001]`：Cell 在控制面分区期间只能使用已授权、未过期、可本地证明的能力和额度。
- `[DIST-FAIL-001]`：远程调用 MUST 显式暴露 deadline、unavailable、partial、uncertain、retry 和 idempotency；位置透明不得隐藏失败语义。
- `[DIST-NAME-001]`：稳定对象身份不编码位置；路由绑定 MAY 变化并由 generation/epoch 防止旧实例复活。
- `[DIST-TASK-001..004]`：TaskAuthority 分片 / 接管 / registry freeze 对象模型已在规范中；实现归属 `C-SHARD`，不在本 ADR 展开。

已有契约只解释 rationale，不覆盖本决策：

- [ADR-0013](./0013-cross-authority-verify-then-commit-contract.md) 把 verify-then-commit + 有界收敛定为**单机**跨 authority 正式契约；决定 3 把跨机 / 跨 Cell 提交登记为 reconciliation 扩展点，须经新 ADR 定案。本 ADR **不是**该扩展点的定案。
- [ADR-0017](./0017-resource-operation-cross-authority-prepare-finalize.md) 是单机三域 coordinator 定案；其复审触发器 4 要求 Stage C 跨机 reconciliation 经新 ADR 定案后再复审三域边界。本 ADR 不提前改写该边界。
- [议题 15](../../discussions/15-分布式内核.md) 已定「蜂窝式 = 本地数据面 + 逻辑集中控制面」为内部分布拓扑。本 ADR 不重开该哲学选择，只裁定 **C 期第一份可注入证据的落地形态**。
- RISK-B-11 / [exit-unknown-risks U-5](../exit-unknown-risks.md)：单机有界收敛不得外推为跨机 / 多 Cell 原子提交、跨 Cell barrier 或远端物理 cleanup。

草案（`053f523` §C.5.4 第 1 条）同时挂出子议题：`TaskAuthorityAssignment.consensus_commit_index` 隐含共识 / 日志基底，但 v0.5 未定共识算法。该子议题的开档义务由决策点 1 裁定并行启动；**是否 / 何时引入共识基底不由本文件裁决**，归并行的共识基底 / 跨 Cell 邻接 ADR。

## 约束

1. 本 ADR 只定 PoC 拓扑顺序与最小拓扑。禁止在此发明 Cell 运行时协议、crate 切分、IPC 形状或 schema 迁移。
2. `C-CELL` 实现车道须待本 ADR **VERIFIED**（定向门 + 审查 clean）后方可派发；仅 ACCEPTED 或仅 completed 不解锁代码。
3. 不得把单机双进程上的分区注入证据写成跨机 / 跨主机网络分区、时钟域或真实多机故障的证据（§C.1.2 第 6 条；RISK-B-11）。
4. 跨 Cell 提交语义（决策点 4 → ADR-0013 扩展点）与共识基底是否引入，均不在本文件决定面。
5. `[PHIL-TRANSFER-002]`：对象模型中的 `consensus_commit_index` 字段不得被本 ADR 解释为已采纳共识协议。

## 候选

草案原文三候选（`053f523` §C.5.4 第 1 条）。「维持阶段 B 单 Cell、不做拓扑」不是阶段 C 选项：§28.3 必须交付多节点 identity/epoch/fencing。

| 候选 | 内容 | 优点 | 主要代价 |
|---|---|---|---|
| **(a) 单机多进程 Cell×2 起步（采纳）** | 同一主机上两个独立 OS 进程各承载一个 Cell；控制面最小面与 Cell 本地七件套按 §26.1 分工，先在进程边界上跑通 identity/epoch/fencing 与可注入分区 | 迭代最快；failure detector / 分区可在本机注入（停进程、掐 IPC、拒续约）；回退成本最低（停第二进程即回到单 Cell）；跨机网络 / 时钟语义后置 | 不覆盖真实网卡分区、跨主机时钟与传输；双进程仍共享主机故障域 |
| (b) 跨机双节点起步（否决，本阶段后置） | 起步即两台主机各一 Cell | 网络分区 / 时钟语义一步到位 | 迭代慢；回退与排障成本高；在尚无单机双 Cell 证据时把 RISK-B-11 未证域与 Cell 骨架同时摊开 |
| (c) 控制面先行（否决） | quota/epoch/fencing 先于 Cell 本地七件套落地 | 控制面对象可单独推进 | 无可对等的第二 Cell，分区 / failure detector / `[DIST-LOCAL-001]` 本地可证能力缺对端；把 §26.1 职责表拆成「先控制面、后数据面」的未裁定规范 |

子议题（并行开档，**不在本表裁断**）：是否 / 何时为 `consensus_commit_index` 引入共识基底。裁定原文要求：W37 与本拓扑 ADR 并行开档；先以控制面单写者 + lease/fencing 覆盖语义，由那份 ADR 定是否 / 何时引入共识基底。

## 评价标准

按决策点 1 裁定给出的最低风险读法，本阶段只比较：

| 维度 | 权重 | (a) | (b) | (c) |
|---|---|---|---|---|
| 迭代速度（到第一份双 Cell 骨架） | 高 | 最快 | 慢 | 中（无双 Cell） |
| 分区可注入（failure detector / `[DIST-FAIL-001]` 显式失败） | 高 | 进程边界可注入 | 需真实网络 | 缺对端 Cell |
| 回退成本 | 高 | 最低 | 高 | 中（已铺控制面、无数据面对证） |
| 跨机网络 / 时钟语义覆盖 | 后置 | 不声称 | 一步到位但过早 | 无 |
| 与 §26.1 职责表同时起步 | 高 | Cell 本地 + 控制面最小面可同波 | 同左但叠加主机面 | 只控制面 |

## 决定

1. **采纳候选 (a)：阶段 C 多 Cell 拓扑 PoC 以单机多进程 Cell×2 起步。** 最小拓扑 = 同一主机上两个独立 OS 进程，各承载一个 Cell；控制面最小面按 §26.1 控制面职责裁最小可证子集（identity / epoch / fencing 与单写者 lease），与 Cell 本地七件套最小面同属 `C-CELL` 前片，不拆成「先控制面、后本地件套」。
2. **跨机语义后置。** 网络分区、跨主机时钟、跨机传输与远端物理 cleanup 推迟到**已有单机双 Cell 证据之后**再开；在该证据落地前，引用本 ADR 的 Claim 不得声称跨机能力（RISK-B-11 仍 open）。候选 (b) 否决为起步形态，不是永久否决跨机。
3. **否决候选 (c) 作为起步顺序。** 控制面对象不先于可对等的第二 Cell 单独充当多 Cell PoC。
4. **共识基底不在本 ADR 决定。** 按裁定原文：共识基底 ADR 于 W37 与本文件并行开档；在该并行 ADR VERIFIED 之前，控制面语义覆盖为**单写者 + lease/fencing**，不引入共识协议，也不把 `consensus_commit_index` 解释为已提交的共识日志。本文件只登记该并行开档义务，不撰写那份 ADR。
5. **范围与实现门。** 本 ADR 是 DESIGN 定案，不实现 Cell 运行时、不改 crate、不改 schema。`C-CELL` 代码车道的信息依赖以本 ADR **VERIFIED** 计零；跨 Cell 提交语义仍以决策点 4 的并行 ADR VERIFIED 为 `C-SHARD` 前置（纪律不变）。
6. **不改写既有契约。** ADR-0013 单机 verify-then-commit 继续有效；ADR-0017 单机三域边界继续有效。二者的 Stage C 扩展点仍待各自的新 ADR，不被本拓扑选择提前关闭或扩大。

## 后果

- **正面**：`C-CELL` 获得可执行的最小拓扑（双进程、可注入分区、可回退）；W37 骨架波可按 §C.5.3 在本 ADR VERIFIED 后细化车道，而不必等待跨机实验床；与议题 15 蜂窝式哲学及 §26.1 职责表兼容，且不把 Raft / 共识名称提前落实为协议。
- **负面 / 债务**：双进程共享主机内核、时钟与存储故障域，故本拓扑上的「分区」是进程 / IPC 故障，不是网卡或机房分区；后续若直接用本 ADR 的 PoC 数字外推 ROAD-C-001/002 的跨机条款，即构成 Claim > Evidence。`consensus_commit_index` 在并行 ADR 定案前保持规范字段、无协议语义。
- **运维责任**：PoC 须能独立启停两个 Cell 进程并注入进程级不可达；不得把第二 Cell 做成同进程线程或 in-proc 双实例冒充多 Cell（那会取消 (a) 的分区可注入优点，滑向未裁定形态）。
- **并行写集**：共识基底 / 跨 Cell 邻接 ADR 由兄弟车道撰写；本文件不预占其编号结论，也不复制其决定面。

## 退出策略

裁定原文已把 (a) 标为「回退成本最低」。退出路径：

1. **否决窗口内回滚（W36 屏障前）**：维护者单点否决本条 → 本 ADR 标 SUPERSEDED / REJECTED，`C-CELL` 未派发则无需代码回滚；已派发则停在 DESIGN，不得把部分实现晋升 canonical。
2. **收缩回单 Cell**：停止第二进程即回到阶段 B 单权威形态；身份不编码位置（`[DIST-NAME-001]`），无强制数据迁移。
3. **升级到跨机双节点**：在单机双 Cell Evidence 齐备后另开 ADR 或修订本 ADR 适用范围，把 (b) 从「后置」升为下一拓扑；不改写 §26.1 对象身份。
4. **若进程边界被证伪**（无法注入分区、无法表达 `[DIST-FAIL-001]`、或双进程实际共享写者破坏单写者前提）：按复审触发器 5 重开，评估是否必须改采 (b) 或补运输面，而不是在本文件上静默扩大拓扑。

预估退出成本：文档级回滚为低；实现后回滚限于停进程与丢弃未晋升 Evidence，无跨权威 schema 强制迁移（本 ADR 未引入迁移）。

## 验证与证据

本 ADR 为设计定案，无 PoC 实现、无测试、无 benchmark。裁决依据：

- [stage-c-progress §C.5.4](../stage-c-progress.md) 决策点 1 裁定行（2026-09-22；`7d3ad53` 落库）
- 草案候选原文：`053f523` §C.5.4 第 1 条
- 规范锚点：v0.5 §26.1 / §28.3
- 契约边界：[ADR-0013](./0013-cross-authority-verify-then-commit-contract.md) 决定 3；[ADR-0017](./0017-resource-operation-cross-authority-prepare-finalize.md) 复审触发器 4
- 风险边界：RISK-B-11、U-5

`C-CELL` 前片（Cell 本地七件套最小面 + 控制面最小面 + 多节点 identity/epoch/fencing）的实现证据、以及 ROAD-C-001 所需 H6 分区注入，均在本 ADR VERIFIED 之后的实现车道落档；落档前不得把本文件引用为「多 Cell 已实现」。

## 复审触发器

1. **W36 屏障否决窗口**：维护者否决决策点 1 → 本 ADR 回滚，受影响波次重排。
2. **首份单机双 Cell Evidence 落地**：复审是否解除「跨机语义后置」，以及 (b) 应否升为下一拓扑。
3. **并行共识基底 ADR VERIFIED 或被否**：复审控制面单写者 + lease/fencing 覆盖是否仍然足够，或 `consensus_commit_index` 是否获得协议语义（本 ADR 不抢先改写）。
4. **ROAD-C-001 评审要求真实网络分区 / 跨主机时钟**：本 PoC 拓扑的证据范围不够时，必须另开跨机拓扑，不得扩写本 ADR 的 Claim 范围。
5. **`C-CELL` 实现证伪 (a)**：进程边界无法注入分区、无法表达 `[DIST-FAIL-001]`、或双进程破坏控制面单写者前提 → 重开本 ADR。
