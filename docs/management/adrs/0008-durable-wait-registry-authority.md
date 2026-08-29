# ADR-0008：Durable wait registry 权威归属（新 crate nlos-wait）

- 状态：ACCEPTED
- 日期：2026-08-29
- Owner：WaitAuthority / ChannelAuthority
- 关联 Requirement：总纲 v0.5 行 325（Channel 为有序、可取消、有背压的通信原语，最小权威为 commit + wakeup）、行 219（异步执行行：callback 必须绑定 generation、cancel epoch 和 OperationId，Fiber WAITING/READY）
- 关联工作包：`B-WAIT-001`（新增，B-PROCESS 家族的 wait registry 前缀切片）、`B-CHANNEL-001`（commit 锚点）
- 决策来源：用户在 2026-08-29 明确选择方向 commit + wakeup（B-PROCESS 侧承接 B-OUTBOX 移交），并在 registry 归属上选择「新 crate `nlos-wait`」，同时确认 wake 触发采用「enqueue/commit 后显式幂等 notify + 重启 replay，不引入轮询」
- 复审触发器：fiber rehydration 落地；runtime-tokio 消费 wait-authority wake 的泛化接线落地；跨进程等待

## 上下文

总纲行 325 把 Channel 的最小权威钉在 commit + wakeup：有序、可取消、有背压的队列事实已由 `B-CHANNEL-001` 以 durable endpoint authority 与 queue delivery 前缀落地，但「commit 之后谁在等、等到哪个 sequence、如何被唤醒」至今没有 durable 事实源。行 219 要求异步执行的 callback 必须绑定 generation、cancel epoch 和 OperationId，Fiber 有 WAITING/READY 状态——等待方身份与等待条件必须是可持久的权威记录，而非内存约定。

现状是三段割裂：`B-OUTBOX`（DONE）已实现「outbox apply → Tokio fiber wake」的 transport observation 侧消费，但其 wake sink（runtime-tokio 的 TokioWakeSink）是纯内存侧——runtime 重启后 fiber record 不恢复，重投 wake 只能分类 `FiberGone` 并 ACK，durable wait registry 与 fiber rehydration 已被 B-OUTBOX 显式移交 `B-PROCESS`/Slice K；`B-PROCESS-001` 已建立 Process/AgentInstance/IsolationDomain 的 durable generation/fence 权威，但其对象身份是 process/agent binding，不含 channel/topic cursor 等待语义；`B-CHANNEL-001` 的未决清单中 commit+wakeup 接线一直标注「依赖 B-PROCESS wait registry」。缺的正是中间一环：一个持久的「谁在等哪个 channel 的哪个 sequence」登记处，使得 commit 锚点一旦成立（包括重启 replay 后），等待方能被恰好一次地唤醒。

本 ADR 要回答：durable wait registry 归属哪个权威对象（不动谁、新建什么）、wake 如何被触发（并明确拒绝轮询/scanner）、等待方 binding 表达到什么程度——同时不污染 process/agent binding 语义与 outbox 的 transport observation 语义。

## 候选

| 候选 | 优点 | 主要代价 |
|---|---|---|
| **1. 新建独立 crate `nlos-wait`：durable wait registry 为独立 authority** | 等待登记是独立的调度事实，不依附任何既有对象；process/agent binding 与 transport observation 语义零污染；面积最小（wait 行只有 channel_id、sequence 上界、opaque binding 与状态） | 新增一个 authority 即新增一份 schema、迁移与运维责任；跨 authority 的 commit→notify 无法单事务，需要幂等 replay 收敛窗口 |
| 2. 扩展 `nlos-process`（在 process/agent binding authority 上加 channel/topic cursor 等待） | 少一个 crate；等待方与 process 对象天然同库 | 对象身份混入：process/agent binding 的语义是「谁在哪个 incarnation 中可调度」，channel/topic cursor 等待与之正交；混入后 rotate/fence 语义互相牵连，B-PROCESS-001 已验收边界被迫重开 |
| 3. 扩展 `nlos-outbox`（在 outbox 侧登记等待并触发唤醒） | wake 消费已在 outbox 落地，顺路 | transport 语义混入调度状态：outbox 的权威是「已发生事实的可靠投递观测」，不是「谁在等待」；且唤醒若由 outbox 侧扫描驱动，同步唤醒退化为轮询延迟，直接违反本次确认的「不引入轮询」约束 |
| 4. 不做改变（wakeup 继续留在 `B-CHANNEL-001` 未决清单） | 零新增面积 | 行 325 的 wakeup 锚点持续无权威承接；`B-OUTBOX` 的移交悬空；`B-TOPIC-001` 的 publish/subscribe 热路径与后续 consumer 无法接入唤醒 |

## 评价标准

安全（等待登记 fail-closed、不产生未授权唤醒面）、正确性（每次 commit 至多触发每个等待行一次逻辑唤醒，PENDING→WOKEN CAS 恰好一次，重启 replay 双向收敛）、性能（唤醒为 commit 后显式调用，无 scanner/轮询常驻成本）、跨平台（纯用户态 + 自有 SQLite authority，不依赖宿主特有机制）、可维护性（crate 边界与总纲分层一致，process binding 与 outbox 语义不被改写）、兼容性（不改 `nlos-process`/`nlos-outbox`/`nlos-channel` 既有 schema/KABI 与测试基线）、退出成本（wait 行为独立 additive 事实，可整体导出，回滚即移除 crate）。

## 决定

采用 **候选 1：新建独立 crate `nlos-wait`，durable wait registry 为独立 authority**，边界如下。

1. **归属**：durable wait registry 是独立 authority，落在新 crate `nlos-wait`，自有 SQLite schema 持久化 wait 行。`nlos-process` 不动——其 process/agent binding 语义保持 `B-PROCESS-001` 验收边界；`nlos-outbox` 不动——其 transport observation 语义保持。runtime-tokio 的 TokioWakeSink 保持内存侧不动；其消费 wait-authority wake 的泛化接线登记为已登记后续工作，在本 ADR 范围之外（owner-side first 先例：先把权威与语义立住，运行时接线随后接入）。
2. **wake 触发**：显式幂等 `notify_commits(channel_id, up_to_sequence)`——由 enqueue/commit 侧（调用方或后续 consumer）紧随 commit 调用，把「该 channel 已 commit 到 sequence N」通知 wait registry；registry 对比各 wait 行登记的等待上界，命中者以 PENDING→WOKEN CAS 恰好一次翻转并派发唤醒。无 scanner、无轮询。重启 replay 以同一幂等语义重放 notify，「恰好一次唤醒」性质跨重启一致。跨 authority 的「commit → notify」无法单事务原子完成，崩溃窗口由幂等重放收敛（与 ADR-0007 的 PENDING_ENQUEUE 同族问题、同解法）。
3. **binding opaque**：wait 行的 binding 为 16B opaque id，表达等待方 fiber/process 的绑定引用；本前缀不解释其内部结构，也不承诺其与 `ExecutionFiberId`/`OperationId` 的映射关系。fiber rehydration（等待方崩溃后由权威记录重建等待）显式列为后续工作，本 ADR 不声称具备。
4. **与 Channel 锚点的关系**：Channel 的权威仍是 commit 锚点本身（`B-CHANNEL-001` 范围不变）；wait registry 只消费 commit 事实（channel_id + sequence 上界），不复制队列条目，不参与 capacity/fencing/compaction。

适用范围：单 Cell 单机（与 `B-CHANNEL-001`/`B-TOPIC-001` 当前范围一致）。跨进程等待（等待方与 commit 方不同进程）在本 ADR 边界之外，触发复审。

## 后果与退出策略

- 新增 crate `nlos-wait` 与其 SQLite schema（wait 行：channel_id、up_to_sequence、16B opaque binding、状态机 PENDING→WOKEN 及 generation/fence 字段）；这是新的运维责任（独立数据库文件、迁移、备份、kill-window 故障矩阵）。
- 「不引入轮询」的代价是 notify 依赖调用方纪律：enqueue/commit 侧遗漏 notify 时，等待在重启 replay 前不可见。因此 commit 侧接线（`B-CHANNEL-001` 锚点与后续 consumer）必须把 `notify_commits` 作为 commit 路径的固定步骤；漏调属接线缺陷，不是 registry 容忍项。
- TokioWakeSink 保持内存侧意味着：在泛化接线落地前，durable 唤醒事实由 `nlos-wait` 权威记录，宿主 runtime 的即时 fiber 唤醒仍走既有 PoC-0004 路径；两条路径的接缝（wake 消费如何从 wait-authority 读出 WOKEN 行并转为 READY）是已登记的后续工作，属技术债。
- fiber rehydration 未落地前，等待方崩溃后其 wait 行仍会命中并翻转为 WOKEN，但无 fiber 可唤醒；该场景按「绑定引用失效」处理，重建等待属 `B-PROCESS`/Slice K 后续。
- 退出策略：`nlos-wait` 为纯 additive authority，不改 `nlos-process`/`nlos-outbox`/`nlos-channel` 任何既有 schema/KABI。若归属被取代，wait 行可整体导出为审计事实；回滚即停用并移除 crate，预估成本为代码删除与进度单/Evidence 回写，无数据回迁。被取代时新增 ADR，不重写本 ADR 历史。

## 验证与证据

本 ADR 决策于 2026-08-29 由用户选择；owner-side 最小前缀与本 ADR 同日实现（第四十九增量，沿用 B-TOPIC-001 的「决策与实现同批」模式），由并行车道落地、integrator 合并验收。Evidence 位置：[B-WAIT-001](../../evidence/stage-b/b-wait-001-durable-wait-registry.md)（integrator 落地）。在该 evidence 存在并记录验收结果之前，本 ADR 不声明任何实现事实为已完成。既有相关证据：[B-CHANNEL-001](../../evidence/stage-b/b-channel-001-endpoint-authority.md)（commit 锚点）、[PoC-0004 / B-OUTBOX](../../evidence/stage-b/poc-0004-outbox-wake-consumer.md)（transport observation 侧 wake 消费与移交记录）、[B-PROCESS-001](../../evidence/stage-b/b-process-001-durable-execution-binding-authority.md)（process/agent binding authority 边界）。
