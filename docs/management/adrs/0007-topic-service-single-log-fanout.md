# ADR-0007：Topic 服务层单 log fanout 模型

- 状态：ACCEPTED
- 日期：2026-08-28
- Owner：TopicService / ChannelAuthority
- 关联 Requirement：`LAYER-SVC-001`、总纲 v0.5 行 222（迁移矩阵 IPC 行：Topic 匹配和语义 reducer 属服务层，内核只保留通用 I/O 锚点）、行 325（Channel 为有序、可取消、有背压的通信原语，锚点 commit + wakeup）、行 1073（资源治理矩阵「消息 / fanout」行）、`RSM-ADMIT-001`、`RSM-FANOUT-001`、`RSM-METER-002`
- 关联工作包：`B-CHANNEL-001`（扩展）、`B-TOPIC-001`（新增）
- 决策来源：用户在 2026-08-28 明确选择「Topic routing/fanout 服务层」方向，并确认「单 log + per-subscriber cursor」fanout 模型，否决 per-recipient 拷贝
- 复审触发器：跨 Cell/分布式 Topic broker；真实 payer 计量与 AttributionPolicy（`RSM-METER-002`）落地；retention 策略执行正式化

## 上下文

`[LAYER-SVC-001]` 要求 Topic 是一等 NLOS 系统服务，且可由更小机制实现的部分不得因「一等」进入微内核；迁移矩阵 IPC 行（行 222）进一步把边界钉死为「Topic 匹配和语义 reducer 属于服务层；内核只保留通用 I/O 锚点」。Channel 是有序、可取消、有背压的通信原语，其内核锚点只有 commit + wakeup（行 325）。资源治理矩阵「消息 / fanout」行（行 1073）要求 Channel/Topic queue、retention、fanout 上界与 payer 预留、慢消费者隔离，并禁止无人承担的无限广播。

`[RSM-ADMIT-001]` 把 Topic fanout 列为必须在不可逆工作开始前完成 admission/reserve/grant 的操作；`[RSM-FANOUT-001]` 要求 publish 在首次入队前绑定 max recipients、delivery attempts、cascade depth、retained bytes/time、payer 和 idempotency scope，每级再发布消耗父级 cascade budget，慢消费者必须隔离，Topic 环不得形成无限成本。`[RSM-METER-002]` 要求共享 fanout 成本使用版本化 AttributionPolicy，且各归属项加未分配系统开销精确等于可信总 meter。

当前 `B-CHANNEL-001` 已落地单机 durable Channel endpoint authority（identity、generation/fencing CAS rotate、participant proof）与 queue delivery 最小前缀（schema v2 immutable entries、consume/trim 双 high-water、bytes 簿记、`enqueue`/`receive`/`ack`/`compact`，compact 钳制 min(trim_to, consume)），并有 kill-window 故障矩阵。未决项为：Topic routing/fanout、payer accounting、commit+wakeup 接线（依赖 B-PROCESS wait registry）、ack 消费身份绑定、跨进程/多机语义、真实掉电。

本 ADR 要回答：Topic broker 归属哪一层、持久层权威如何划分、采用什么 fanout 模型，以及 RSM 准入/fanout 约束在什么时点绑定——同时不复制 Channel 的队列事实、不污染内核 I/O 锚点。

## 候选

| 候选 | 优点 | 主要代价 |
|---|---|---|
| **1. 服务层 Topic broker：单 log + per-subscriber cursor** | publish 恰好入队一次，无写放大；慢订阅者只拖慢自身，天然隔离；Channel 保持唯一队列/日志事实源；Topic 持久层不含消息体，面积小 | 需要 Topic 侧持久化 per-subscriber 游标并钳制 Channel compaction；离线订阅者的保留窗口必须由 retention 上界约束，否则 log 无界增长 |
| 2. per-recipient copies（publish 时为每个订阅者复制一份队列条目） | 接近 actor mailbox 直觉，每接收方投递状态独立、简单 | 写放大 N 倍；订阅者越多成本越高且在 payer 计量未落地时无人承担，直接违反行 1073「禁止无人承担的无限广播」与 `RSM-FANOUT-001` 精神；慢消费者仍需独立隔离机制，原问题并未消失 |
| 3. 不做改变（暂不建 Topic 服务，fanout 留在 `B-CHANNEL-001` 未决清单） | 零新增代码面积 | `RSM-ADMIT-001`/`RSM-FANOUT-001` 对 Topic 的要求无 owner；`LAYER-SVC-001` 的一等系统服务承诺持续落空；上层 Semantic/Task 的扇出需求继续无规范承接 |

## 评价标准

安全（缺失/越界策略 fail-closed、无未授权广播成本）、正确性（每 publication 恰好一次入队、游标单调、cascade 预算不可透支）、性能（写放大倍数、慢消费者影响面、compaction 可推进性）、跨平台（纯用户态服务 + 自有 SQLite authority，不依赖宿主特有机制）、可维护性（队列事实源唯一、crate 边界与总纲分层一致）、兼容性（不改变 `nlos-channel` 既有 schema/KABI 语义与既有测试基线）、退出成本（Topic 数据可整体导出/迁移，内核与 Channel 零耦合残留）。

## 决定

采用 **候选 1：服务层 Topic broker，单 log + per-subscriber cursor**，具体边界如下。

1. **归属与分层**：Topic broker 为一等系统服务，新建独立 crate `nlos-topic`（服务层 authority），不进 `nlos-channel`——后者作为内核通用 I/O 锚点保持通用（行 222、行 325、`LAYER-SVC-001`）。Topic 服务持久层为自有 SQLite authority；Channel authority 保留唯一队列/日志事实源。Topic 不复制消息体，只持久化 topic 身份、订阅、发布策略绑定与 per-subscriber 游标。
2. **fanout 模型**：单 log——每 topic 绑定一个 channel，publish 恰好 enqueue 一次；per-subscriber cursor——订阅者各自维护消费进度，慢消费者只拖慢自身、天然隔离，无 per-recipient 拷贝放大。被否候选 2 的写放大与无人承担的广播成本是主要否决理由。
3. **`RSM-FANOUT-001` 绑定时点**：publish 在首次 enqueue 前持久化完整策略绑定——max_recipients、delivery_attempts、cascade_depth、retained bytes/time 声明、payer typed binding、idempotency scope；缺失或越界一律 fail-closed，不产生部分入队。订阅 admission 受 max_recipients 上界约束，超限订阅不得静默接纳。
4. **payer 处理（最小前缀）**：payer 为 opaque typed binding——durable 记录 + admission 时绑定存在性校验；计量/扣费与 ResourceAccount 集成显式延后，归属未来 `RSM-METER-002` AttributionPolicy ADR。本 ADR 不声称已实现 payer 预留执行。
5. **cascade 预算**：republish/forward 必须消耗父 publication 的 cascade budget（owner 侧 CAS 递减，耗尽 fail-closed）；深度越界 fail-closed。Topic 环不得形成无限成本。
6. **compaction 边界**：channel 日志 compact 的 trim 上界由 Topic 层 min-live-subscriber-cursor 钳制（服务层策略，取代/收紧 Channel 侧仅按 consume high-water 的钳制）；Channel 内核 compact 保持纯语义不变——未消费条目永不删除的不变量继续成立。

适用范围：单 Cell 单机 Topic 服务（与 `B-CHANNEL-001` 当前范围一致）。跨 Cell/分布式 broker、真实 payer 计量、retention 策略执行均在本 ADR 边界之外，触发复审。

## 后果与退出策略

- 新增 crate `nlos-topic` 与其 SQLite schema（topic 身份、订阅、publication 策略绑定、per-subscriber 游标、cascade 预算 CAS 记录）；这是新的运维责任（独立数据库文件、迁移、备份、故障矩阵）。
- Topic 侧持久层不含消息体，体量与订阅者规模成正比而非与消息数×订阅者数成正比；代价是离线订阅者的保留窗口把 channel log 尾部拖住，必须靠 retained bytes/time 上界执行强制截断（retention 执行属 `B-TOPIC-001`，越界订阅者按策略断开或游标跳变，具体策略另行验证）。
- per-subscriber 游标推进是新的失败点：游标单调 CAS、重启 replay、与 `ack` 消费身份绑定的关系在 `B-TOPIC-001` 定义；Channel 侧 `ack` 语义不变。
- 对 `B-CHANNEL-001` 各未决项的影响：
  - Topic routing/fanout：归属移交本 ADR 与 `B-TOPIC-001`，`B-CHANNEL-001` 保留 queue/delivery/proof 内核侧核心；
  - payer accounting：`B-TOPIC-001` 只落地 opaque typed binding 与存在性校验，真实计量/扣费继续未决，挂 `RSM-METER-002` 后续 ADR；
  - commit+wakeup 接线（依赖 B-PROCESS wait registry）：不受本 ADR 影响，仍留在 `B-CHANNEL-001`；Topic 的 publish/subscribe 热路径后续作为 wakeup 消费方接入；
  - ack 消费身份绑定：单 log 模型下订阅者进度由 Topic 侧游标表达，Channel 侧 ack 仍是单队列 trim high-water；消费身份必须绑定到游标推进者身份（`B-TOPIC-001` 验收项）；
  - 跨进程/多机语义：本 ADR 明确不覆盖，保持未决并列为复审触发器；
  - 真实掉电：仍在 `B-CHANNEL-001` 故障矩阵范围；`B-TOPIC-001` 需要自己的 kill-window 矩阵（publish 绑定与 enqueue 的跨 authority 窗口、游标 CAS 窗口）。
- 负面与技术债：跨 authority 的「策略绑定持久化 → Channel enqueue」无法单事务原子完成，崩溃窗口需要幂等重放收敛（publish idempotency scope 承担）；min-live-cursor 钳制使 compaction 依赖服务层查询，接口需在 `B-TOPIC-001` 定义；payer 只做存在性校验意味着「有人承担」目前是记账承诺而非强制。
- 退出策略：`nlos-topic` 为纯 additive 服务，不触碰内核与 `nlos-channel` 既有 schema/KABI。若 fanout 模型被取代（如转向分布式 broker 或 per-recipient），订阅与游标记录可整体导出为审计事实，channel log 因 Topic 从未复制消息体而无迁移负担；回滚即停用 Topic 服务并移除 crate，预估成本主要是代码删除与进度单/Evidence 回写，无数据回迁。被取代时新增 ADR，不重写本 ADR 历史。

## 补记：delivery attempts 执行语义（2026-08-28，第四十七增量）

`[RSM-FANOUT-001]` 要求 publish 绑定 delivery attempts；单 log + cursor 模型下「一次投递」的 owner 可强制语义补记如下（用户未即时应答，按推荐语义执行，复审触发器同本 ADR）：

- **计费点**：某订阅者处于滞后态（其 cursor 落后于既有 max sequence）时，每有一条**新** publication 成功 enqueue 到该 topic，该订阅者的 durable 重投计数 +1（与 enqueue 同事务的 owner 计数，非运行时观测）；cursor 追平后计数**不清零**——预算按声明 attempts 一次性给足，耗尽即隔离，避免「追平重置」被慢消费者用作无限续期。
- **耗尽语义**：计数达到策略声明 delivery_attempts → 订阅翻转 `QUARANTINED`（慢消费者隔离，RSM-FANOUT-001）；poll 对隔离订阅者返回 typed `DeliveryQuarantined`（fail-closed 读，与 poll 无鉴权不冲突：拒绝服务而非鉴权）；advance/unsubscribe 仍可用（不扣留消费权与退出权）；其余订阅者不受影响；**零消息删除**——channel log 与游标不动，隔离只停投递。
- **恢复**：显式 `reinstate`（须出示 consumption token）清零计数翻转回 ACTIVE，cursor 保持原位；同 key 幂等。
- **被否候选**：耗尽即删游标/踢出重订（丢位置，违背可审计性）；阻断 topic 新 publish（把单个慢消费者的成本转嫁给全部发布者）；追平即重置计数（无限续期漏洞）。

## 验证与证据

本 ADR 决策于 2026-08-28 由用户选择；同日 `B-TOPIC-001` 最小前缀已实现并验证（canonical commits `89f966e` / `345a959` / `05ff1ff`），上节所列验收项全部覆盖：策略绑定 fail-closed、恰好一次 enqueue 与 replay 幂等、游标单调/重启 replay、min-live-cursor compact 钳制、cascade 预算 CAS 与耗尽/深度越界 fail-closed、payer 绑定存在性校验、订阅 max_recipients admission，以及 Topic 侧 14 项 kill-window 故障矩阵（含 PENDING_ENQUEUE 跨权威双向收敛与悬空 ENQUEUED 检测路径）。实现事实与明确未决项（delivery attempts 执行、运行时自动 republish、真实 payer 计量、匹配谓词、跨进程/多机、wakeup、真实掉电）见 [B-TOPIC-001 evidence](../../evidence/stage-b/b-topic-001-topic-service-single-log-fanout.md)。仍为单机 `H3 / PARTIAL_PASS`，不声明分布式 broker 或 payer 扣费。Channel 侧既有证据见 [B-CHANNEL-001](../../evidence/stage-b/b-channel-001-endpoint-authority.md)。
