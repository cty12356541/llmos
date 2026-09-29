# Cell 骨架设计（本地七件套 + 控制面最小面 + 单机双进程 identity/epoch/fencing）

- **日期**: 2026-09-22
- **状态**: `DESIGN` only（无实现、无 Evidence、不改写当前规范）。**C-CELL 代码车道入度不为零，直至 W37-A 拓扑 ADR 为 VERIFIED。**
- **波次**: W37-C
- **写集**: 本文件 only
- **背景依据**（只誊录/索引，不发明 Requirement）:
  - [架构设计总纲 v0.5 §26.1 蜂窝式权威](../../design/06-架构设计总纲-v0.5.md)（本地七件套、控制面职责、三 Record、`DIST-LOCAL/FAIL/NAME-001`、`DIST-TASK-001..004`）
  - [v0.5 §28.3 阶段 C](../../design/06-架构设计总纲-v0.5.md) 必须交付首条「多节点 identity/epoch/fencing」
  - [v0.5 §3 `MODEL-ID-002/003`](../../design/06-架构设计总纲-v0.5.md)、[§7 `ID-AUTH-001`/`ID-DOMAIN-001/002`](../../design/06-架构设计总纲-v0.5.md)、[§12 lease/fencing](../../design/06-架构设计总纲-v0.5.md)、[`TCB-ENDPOINT-001`](../../design/06-架构设计总纲-v0.5.md)、[`CONS-SCOPE-001`](../../design/06-架构设计总纲-v0.5.md)、[`SEM-CHECKPOINT-001`](../../design/06-架构设计总纲-v0.5.md)、[`RSM-DIST-001`](../../design/06-架构设计总纲-v0.5.md)
  - [阶段 C 进度单](../../management/stage-c-progress.md) §C.3.1 `C-CELL`、§C.5.2 依赖链、§C.5.4 决策点 1（单机多进程 Cell×2）
  - [ADR-0013](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md) 决定 3：跨 Cell 提交语义是 Stage C 扩展点，不在单机契约内
  - [RISK-B-11](../../management/risks.yaml)：单机结论不得外推跨机/多 Cell

## 0. 等待门（本文件不授权实现）

`[PD-READ-003]` / 阶段 C 派发纪律：入度以 **VERIFIED** 计零；决策类信息依赖未关闭不得提前派发实现。

| 门 | 权威对象 | 本骨架关系 |
|---|---|---|
| W37-A 拓扑 ADR（决策点 1 晋升） | 计划中的 `docs/management/adrs/0018*` / 车道 `feat/w37-topo-adr` | **硬前置**。未 VERIFIED 前禁止 C-CELL 产品代码、新 crate、schema、测试桩。 |
| 跨 Cell 提交 ADR（决策点 4） | ADR-0013 扩展点 / 计划中的 `0019*` | **不在本骨架实现范围**。`DIST-TASK-001..004` 与三 Record 只作不得违背的对象模型，落地归 `C-SHARD`。 |
| 共识基底 | 决策点 1 子议题：先控制面单写者 + lease/fencing，ADR 定是否/何时引入共识 | 本文件**不选定**共识算法或复制日志。 |

本文件是实现车道的**读集草稿**：把 v0.5 已写明的 Cell 骨架面收成一张可审查清单，并标出必须由 W37-A 回答的空位。W37-A 若收窄/改写下列「保守读法」，以 ADR 为准，本文件不得覆盖 ADR。

**禁止**：把本 DESIGN 改写成「已实现」；用本文件替代 ADR；把单机权威结论外推为双进程/跨 Cell 能力（RISK-B-11）。

## 1. 目标与非目标

### 1.1 目标（C-CELL 前片，ADR 关闭后）

在 **单机两个 OS 进程 = 两个 Cell**（决策点 1 已裁 (a)，跨机后置）上，交付可验证的最小骨架：

1. 每个 Cell 进程持有 §26.1 **本地七件套**的最小面（§2）；
2. 控制面只开 §3 所列 **最小面**（identity / lease-epoch-fencing / 分区下本地可证明额度）；
3. 两进程之间的 identity、`node_boot_generation`、lease epoch、fencing token 满足 §4，使 `DIST-LOCAL-001` / `DIST-FAIL-001` / `DIST-NAME-001` 与 `TCB-ENDPOINT-001` 的分布式校验条款可测。

这对应 §28.3 必须交付的第一条「多节点 identity/epoch/fencing」，PoC 拓扑把「节点」读成「同机 OS 进程」，**不**把「同机」读成「同一 Cell」或「可省略 fencing」。

### 1.2 非目标（本骨架与后续工作包的边界）

下列已有规范锚点与工作包，**不得**由本骨架发明实现或提前开码：

| 排除 | 权威去向 |
|---|---|
| 跨机 / 多机拓扑 | 决策点 1：有单机双 Cell 证据后再置 |
| 跨 Cell 提交原子性 / 2PC | ADR-0013 决定 3；决策点 4；`C-SHARD` |
| TaskAuthority 分片与接管全语义 | `DIST-TASK-001/002/004` → `C-SHARD` |
| 层级 fanout / reducer / bulkhead 全语义 | `DIST-TASK-003` / `DIST-FANOUT-001` / `DIST-BULKHEAD-001` → `C-FANOUT` |
| Process 迁移与 placement 执行 | `DIST-MIGRATE-001/002` → `C-MIGRATE` |
| Artifact/event sync 与审计 checkpoint | §26.2、`SEM-CHECKPOINT-001` → `C-SYNC` |
| 三族 lease 全状态机 + reconciliation | §12 全部 `LEASE-*` → `C-LEASE` |
| Global/Cell/Worker 调度完整面 | §25.2.2 / §28.3 → `C-SCHED` |
| 分区恢复与 ROAD-C-001/002 退出门 | `C-PARTITION` / `C-BENCH` |
| 真实 provider / payload codec 冻结 | 移交 #9；codec 仍为 RISK-B-05 `review_point` |
| 全球联邦与跨组织清算 | §29.2；决策点 5：C 只交机制面 |

## 2. Cell 本地七件套（§26.1 逐字誊录 + 骨架读法）

§26.1：「每个 Cell 具有本地」下列七项。骨架必须把它们做成**每进程一份、互不共享写者**的本地件；共享 SQLite 文件、共享 supervisor 表、共享 lease WAL 是否允许，由 **W37-A 定**，本文件默认保守读法 = **两进程两份 durable 根**（与「一 authority 一 store 一写者」及 ADR-0013 否决 ATTACH 共享事务同构，但**不是**新 ADR）。

| # | §26.1 原文 | 骨架最小面（不发明新语义） | 明确不做 |
|---|---|---|---|
| 1 | Process supervisor | 本 Cell 进程内的 Process 出生/登记/fencing 观察面；child 进入 `RUNNABLE` 仍受 `PROC-SPAWN-001` / `PROC-BIRTH-001`。跨进程不得把对方 pid 当本 Cell 本地事实。 | supervisor 自动跨进程 pid 发现/unregister（移交 #11 后片 / `nlos-process` 锁） |
| 2 | capability/name cache | 本 Cell 可本地证明的 Capability / 名称绑定缓存；分区期间只服务未过期、可本地证明的条目（`DIST-LOCAL-001`）。稳定对象身份不编码位置（`DIST-NAME-001`、`MODEL-ID-002`）。 | 跨 Cell 名称与服务发现权威（控制面第 5 条，见 §3.2） |
| 3 | Resource lease 子账本 | 本 Cell 持有的已签发 lease 本地 WAL/子账：至少能校验 `node_boot_generation`、epoch、fencing token（`LEASE-PROTO-001`、`LEASE-ENFORCE-001`）。WAL 丢失 → 停准入并 fence 旧 boot generation（`LEASE-WAL-001`）。 | 控制面扣除 `face_value`、三族 reconciliation、跨 Cell 重发（`C-LEASE`） |
| 4 | Driver gateway | 本 Cell 侧真实成本/副作用端点：`TCB-ENDPOINT-001` + `DRV-BOUND-001`；`ENFORCED_DISTRIBUTED` 时 lease epoch/fencing token 不得降 optional。 | 真实 provider 替换 mock、codec 入 ADR-0014 通道（移交 #9） |
| 5 | durable event/outbox | 本 Cell 出站事件/命令的 durable outbox，供分区后 replay；不在此发明跨 Cell merge。 | `DIST-EVENT-001` 签名事件集合同步（`C-SYNC`） |
| 6 | Artifact cache | 本 Cell 内容寻址缓存；权威 replica/conflict/deletion 语义未在骨架宣布。 | `DIST-ART-001` 按类型声明 sync（`C-SYNC`） |
| 7 | failure detector | 对**对端 Cell 进程**与本地 child 的不可用/超时观察，供 `DIST-FAIL-001` 暴露 `unavailable`/`uncertain`，不得用「同机」省略。 | 跨机网络分区注入矩阵（后置） |

七件套是 Cell **本地**权威，不是控制面副本。控制面建议不替代节点本地 admission、boot generation 与 fencing 校验（`RSM-DIST-001`）。

## 3. 控制面最小面

§26.1 控制面职责原文六条（进度单称「控制面七职责」时把「quota lease、epoch 和 fencing」与 identity 面拆列；规范正文为下列六条）。**全表有效**；骨架只实现「最小面」，其余按已有工作包延后。最小面划分是**保守读法**，W37-A 可收窄或改切，不得在 ADR 前当已裁决。

### 3.1 骨架最小面（待 W37-A 确认）

1. **Principal/key 与 policy**
   - `ID-AUTH-001`：Principal、ControlDomain、Process、Driver、Verifier 身份来自认证会话、受信启动、capability 或 attestation，**禁止**对端进程用自由字符串自报身份。
   - `ID-KEY-001`：签名 key 绑定 Principal、用途、有效期、generation、撤销状态。
   - 决策点 1 子议题默认：控制面**单写者**；哪一个进程持有控制面写权限、另一进程是否只读副本，由 W37-A 写明。

2. **quota lease、epoch 和 fencing**（与 §12 同一对象，不做全状态机）
   - Quota lease = 控制面 `AVAILABLE` → 节点 `LEASE` 的预付转移，不是乐观透支（§12 开篇）。
   - 骨架必须能：**签发到指定 Cell 的 lease 带 `node_id` + `node_boot_generation` + `epoch` + `fencing_token` + `fence_scope` + `AuthorityExpiry`**（`QuotaLease` 字段誊录）；节点侧校验这些字段后才准入。
   - `LEASE-GRANT-001`：控制面先 durable 扣除 `face_value` 再签发——若骨架尚无完整 Ledger，W37-A 须声明「最小面」是真扣除还是显式 stub；本文件**不发明 stub 语义**。
   - `LEASE-FENCE-001`：fencing token 在 `fence_scope` 内全序单调；重发未对清额度前必须取得该 scope 内真实成本 gateway 的 durable fence-barrier ACK，否则额度保持 `QUARANTINED`。
   - `DIST-LOCAL-001`：控制面分区期间，Cell **只能**使用已授权、未过期、**可本地证明**的能力与额度；不得凭对端内存里的旧快照超卖（`RSM-DIST-001`）。

3. **identity snapshot 与 ControlDomain（§7，支撑「多节点 identity」）**
   - `ID-DOMAIN-001`：`ControlDomainId` 表达共同控制关系，阻止同一控制者通过多 key、**多进程**或多事件伪造独立 quorum。
   - `ID-DOMAIN-002`：ControlDomain 由 Identity authority 在版本化 identity snapshot 中创建/维护；merge/split/revoke 出新 snapshot + effective time。
   - 单机双进程的默认读法（待 ADR 确认）：两 Cell 进程若同属一个维护者控制关系，**计同一个 ControlDomain**，不得因「两个进程」变成两个独立 quorum 投票者（`SEM-VIEW-003` / `TASK-SCALE-002` 同款去重）。

### 3.2 控制面其余职责（骨架登记、不实现）

| §26.1 原文 | 延后 |
|---|---|
| Package/Application trust | 单机权威已在阶段 B；跨域接受须保存生产者签名 + AdmissionReceipt + trust snapshot（`DIST-AUDIT-001`）→ `C-SYNC` |
| placement 与 migration intent | `C-MIGRATE`；`DIST-MIGRATE-001` 的源 fencing / 新 generation 不得在骨架里用「同机拷贝目录」代替 |
| 跨 Cell 名称与服务发现 | 骨架只要求两 Cell 有**不编码位置**的稳定身份（§4）；发现协议/目录由 ADR + 后续包定 |
| reconciliation 和审计 checkpoint | `C-LEASE` + `C-SYNC`；分布式 View 用签名 vector/checkpoint，不得假设跨 Cell 全局标量 `log_seq`（`SEM-CHECKPOINT-001`） |

## 4. 单机双进程的 identity / epoch / fencing

决策点 1：PoC = **一台机器、两个 OS 进程、两个 Cell**。下列条款在「同机」条件下仍然全文有效；「同机」只降低部署成本，不降低失败语义。

### 4.1 身份（identity）

| 条款 | 对双进程的约束 |
|---|---|
| `MODEL-ID-002` | Cell / Process / Node 稳定 ID **不得**编码 `localhost`、本机路径、pid、端口或当前路由。 |
| `MODEL-ID-003` | 可迁移运行对象另带 `generation/epoch`；旧 generation 的 handle 与回调 MUST 被拒绝。进程重启 ⇒ 新 `node_boot_generation`，旧 boot 的 handle 不得复活。 |
| `DIST-NAME-001` | 稳定对象身份不编码位置；路由绑定 MAY 变化（进程 A/B 谁监听哪条 IPC 由 ADR 定），靠 generation/epoch 防止旧实例复活。 |
| `ID-AUTH-001` | 对端 Cell 不得凭自报字符串进入本 Cell 的 identity 事实。 |
| `ID-DOMAIN-001/002` | 两进程的 quorum 独立性以 identity snapshot 为准，不以进程数计（见 §3.1.3）。 |
| `PROFILE-LOCAL-001` | 单节点离线面仍然成立：远程对端不可用时必须显式降级，同步不得成为本地数据存在的前提。 |

§33 未单列 `CellId`。骨架可用的已有名义类型：`SchedulerDomainId`（含 Cell 层）、`ControlDomainId`、`IsolationDomainId`（必须另带 generation/fencing token）。是否新增 `CellId` 名义类型 = **W37-A / 规范修订**，本文件不发明。

### 4.2 Epoch

至少四条互相不可替换的 epoch 轴（总纲已分开，骨架不得合成一个整数）：

1. **`node_boot_generation`**：每个 Cell 进程一次启动一个值；lease、`LEASE-PROTO-001` 命令、`TCB-ENDPOINT-001` 分布式校验都绑定它。
2. **lease `epoch` / `capacity_epoch` / `exclusivity_epoch`**：随签发与 fence 前进；旧 epoch 的 spend/invoke 拒绝。
3. **TaskAuthority `authority_term` + `control_epoch`**：对象模型见 §5；骨架不实现接管，但**不得**让两进程共享一个可双方推进的 term 计数器（除非 W37-A 把控制面单写者写死到其中一个进程）。
4. **`MonotonicDeadline.boot_epoch`**（`MODEL-TIME-001`）：本机 cancel/deadline 用单调时钟；持久 lease/capability expiry 用 `AuthorityExpiry`，不是墙钟默契。

`CONS-SCOPE-001`：v0.5 **不建立跨 Cell 全局总序**。双进程不得靠「同一台机器的 SQLite 序列」冒充跨 Cell 线性化。

### 4.3 Fencing

| 条款 | 对双进程的约束 |
|---|---|
| `TCB-ENDPOINT-001` | 真实成本 / canonical write / 受保护副作用：分布式场景再校验 node boot generation、lease epoch、fencing token。两进程互调即分布式场景。 |
| `LEASE-FENCE-001` / `LEASE-ENFORCE-001` | token 在 `fence_scope` 内全序单调；invoke/extend/新计费窗口携带并校验，不得 optional。 |
| `LEASE-WAL-001` | 任一 Cell 丢失本地 lease WAL：停准入、fence 旧 boot generation、等控制面 reconcile/quarantine；不得从 `face_value` 猜 `remaining` 后上线。 |
| `RSM-DIST-001` | 分区时不得凭中央旧快照超卖；全局/对端建议不替代本地 admission。 |
| `DIST-FAIL-001` | 进程间调用 MUST 暴露 deadline、unavailable、partial、uncertain、retry、idempotency；位置透明（「都在 localhost」）不得隐藏失败。 |
| `DIST-LOCAL-001` | 对端不可达 = 控制面分区（若对端持有控制面写者）或对等 Cell 分区；本 Cell 退回本地可证明额度。 |

决策点 1 子议题：先用控制面单写者 + lease/fencing 覆盖语义。**是否引入共识基底由 W37-A（及并行共识 ADR）决定**；本骨架的可测命题在 ADR 前只陈述为：

- 旧 boot generation / 旧 fencing token 的 endpoint 动作被拒绝；
- 未完成 fence-barrier 的额度保持 `QUARANTINED`；
- 不得出现两个进程同时以同一 `(lease_id, epoch, fencing_token)` 作为互不感知的双主 spender（如何线性化该拒绝 = ADR）。

### 4.4 失败形态（PoC 可注入，仍等 ADR 定注入面）

单机双进程的价值（决策点 1 原文）：迭代快、**分区可注入**、回退成本低。骨架验收应能表达——但注入装置由 ADR 指定，本文件不发明 socket/kill 协议：

- 停掉 Cell B 进程：Cell A 对 B 的调用呈 `unavailable`/`uncertain`，A 不超卖（`DIST-FAIL-001` + `DIST-LOCAL-001`）。
- Cell B 以新 `node_boot_generation` 重启：旧 generation 的 handle/callback/lease spend 被拒绝（`MODEL-ID-003`）。
- 控制面写者进程停掉：另一 Cell 不得自行晋升为第二写者，除非 ADR 给出接管协议（默认：无协议则只读本地可证明状态）。

## 5. §26.1 三 Record：骨架只遵守、不实现

下列对象模型是蜂窝式权威的规范形状。`home_cell`、`authority_shard`、takeover fence 集已预设「Cell 是可命名的家」。**实现归 `C-SHARD`，且前置决策点 4 VERIFIED。** 骨架不得另造一套 Task 分片表去「先跑起来」。

誊录字段以 v0.5 §26.1 为准（此处不复制全文以免双源）：

- `TaskAuthorityParticipantRegistry`：`authority_term` + `control_epoch` + participant 全集 + `OPEN | FROZEN_FOR_PERMIT | FROZEN_FOR_TAKEOVER | SUPERSEDED`
- `TaskAuthorityAssignment`：`home_cell` + `authority_shard` + `authority_lease_id` + `ACTIVE | TAKEOVER_PENDING | FENCED | EXPIRED | QUARANTINED`
- `TaskAuthorityTakeoverReceipt`：冻结旧 term/epoch、`exact_fence_set_root`、逐 endpoint barrier Receipt

与骨架直接相关的已有语句（实现仍后置）：

- `DIST-TASK-001`：worker MAY 跨 Cell，但 canonical `TaskControlRecord` / `TaskHead` / `CommitPermit` 只能由该 term 的 authority 提交。
- `DIST-TASK-002`：旧 Cell 在接管后必须拒绝 result/message/permit/callback/effect/commit；不可 fence 则等待过期并 `QUARANTINED`，不能抢先接管。
- `DIST-TASK-004`：禁止「已启用但未进入 fence 集」的参与者。

ADR-0013 在单机范围内仍是 verify-then-commit + 有界收敛；跨 Cell 提交不在该契约内。

## 6. 必须由 W37-A 回答的空位（本文件保持开放）

1. 两 Cell 进程的进程模型：如何启动、如何命名（不违反 `MODEL-ID-002`）、IPC 载体（UDS / 其它）、谁先谁后。
2. 控制面单写者落在哪个进程；只读进程如何取 identity snapshot / lease 视图；写者死亡时是否冻结。
3. 两进程是否允许共享任何 durable 文件；若共享，如何保持一写者。
4. `Cell` 稳定身份的名义类型（沿用 `SchedulerDomainId` 还是新增，须改 v0.5 / schema 通道）。
5. 共识基底：无 / 何时 / 何种；在此之前 fencing 的线性化点落在哪一个权威。
6. `LEASE-GRANT-001` 在骨架是否完整扣除 `face_value`，或显式声明最小面不含 grant（避免静默 stub）。
7. 分区注入的规范手段（杀进程 / 切 IPC / 时钟），以便后续 Evidence 对上 `DIST-FAIL-001`。

未回答前，C-CELL 代码入度保持非零。

## 7. 实现车道验收门（仅在 W37-A VERIFIED 之后适用）

下列是骨架**将来**的测试命题，不是本文件的已证事实。TDD 与写集由届时 C-CELL 车道声明。

| 命题 | 规范锚 |
|---|---|
| 每进程一份本地七件套最小面，对端不可写本 Cell supervisor / lease WAL / outbox | §26.1 本地表 |
| 稳定 ID 不含位置；重启后旧 boot generation handle 被拒 | `MODEL-ID-002/003`、`DIST-NAME-001` |
| 对端进程停 = typed unavailable/uncertain，本 Cell 不超卖 | `DIST-FAIL-001`、`DIST-LOCAL-001` |
| spend/invoke 缺或旧 fencing token / epoch / boot generation → 拒绝 | `TCB-ENDPOINT-001`、`LEASE-ENFORCE-001` |
| 两进程不得靠双计票伪造 ControlDomain quorum | `ID-DOMAIN-001` |
| 无跨 Cell 全局标量 log_seq / 全局锁作为 commit 前提 | `CONS-SCOPE-001`、`SEM-CHECKPOINT-001`、`SCHED-HIER-001` |
| 不声明跨机、不声明 `DIST-TASK-*` 接管、不声明 ROAD-C-001/002 | RISK-B-11、§1.2 |

证据等级预期：单机双进程 H3/H6 注入的**局部** Evidence；任何「多 Cell 已具备」生产声明仍受 RISK-B-11 `review_point` 约束。

## 8. 兼容与诚实边界

- 阶段 B 单机权威（TaskAuthority term/fencing、ADR-0013 单机收敛、ADR-0017 三域 coordinator）仍是迁移起点，**不是**双进程证据。
- 本文件不修改 v0.5、不修改 `stage-c-progress.md`、不开 ADR、不改 `nlos-process` / `nlos-runtime*` / `nlos-plan` / `nlos-task` / `nlos-system-control`。
- 状态保持 `DESIGN`。W37-A VERIFIED 且实现车道提交 Evidence 之后，由 integrator 把对应 claim 从设计晋升为已实现；本文件不得自行改写。
