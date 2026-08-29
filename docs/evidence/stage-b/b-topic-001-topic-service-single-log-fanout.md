# B-TOPIC-001：Topic 服务层单 log fanout 最小前缀

- 状态：`PARTIAL_PASS`（单机 SQLite 重启级 `H3`）
- 日期：2026-08-28
- Owner：`TopicAuthority`（`crates/nlos-topic`，SQLite schema v2）
- 设计依据：[ADR-0007](../../management/adrs/0007-topic-service-single-log-fanout.md)（用户 2026-08-28 选择单 log + per-subscriber cursor 模型）
- 关联工作包：`B-TOPIC-001`（本切片）；`B-CHANNEL-001`（Channel 为队列事实源）

## 1. 实现事实

- **分层**：Topic 服务层 authority 自有 SQLite 持久层（WAL+FULL、STRICT、immutable trigger、单写者），不复制消息体；Channel authority 保持唯一队列/日志事实源。TopicId/SubscriptionId 均为 authority 域分隔派生。
- **create_topic**：经 `ChannelAuthority::inspect_channel` owner 回读绑定 channel 与 generation/fence 快照；RSM-FANOUT-001 策略（max_recipients/delivery_attempts/cascade_depth/retained_bytes/retention_ms/payer opaque typed binding/idempotency scope）缺失或越界 pre-write fail-closed；幂等 replay/漂移 `IdempotencyConflict`。
- **subscribe/unsubscribe**：活跃订阅 < max_recipients 否则 `SubscriberLimitReached`；游标初始化于订阅点（不回放历史）；min-live-cursor 排除非活跃订阅。
- **publish**：先持久化 publication 行（策略 digest/payer binding/cascade 预算/idempotency）再 `channel.enqueue`（fence 实时回读，`StaleChannel` typed 传播无静默重试）；恰好一次 enqueue；`PENDING_ENQUEUE` 崩溃窗口同 key 重放补投收敛（verify-then-commit，不声称跨 authority 原子性）。
- **poll/advance**：poll 零写、`sequence > subscriber_cursor` 过滤；advance per-subscriber 单调 CAS（回退/越界 `InvalidSequence`）；慢消费者隔离（A 滞后不影响 B）。
- **compact_bound/compact**：effective = min(trim_to, min_live_subscriber_cursor, channel consume high-water)；Channel 内核 compact 语义未被修改。
- **republish（cascade，schema v2）**：单 Immediate 事务内 owner 回读父行 → `verify_parent_chain`（断链/环/深度非严格递减 `CorruptRecord`）→ 深度上界 pre-write `CascadeDepthExceeded` → 预算 guarded UPDATE CAS（耗尽 `CascadeBudgetExhausted` 零部分状态）→ 子 publication 入链；跨 authority enqueue 复用 publish 语义。幂等：绑定三元组漂移冲突；ENQUEUED replay 预算不双扣不双投；PENDING_ENQUEUE 窗口跳过预算 CAS 只补投。预算审计不变量：预算 = 初始 cascade_depth − durable 子行数。
- **CorruptRecord 交叉校验**：游标/计数重推导、publication↔channel 序列绑定、父链完整性；悬空 `ENQUEUED` 绑定（仅跨文件磁盘级 torn-write 模型可达）`CorruptRecord` 硬失败。

## 2. 验证

```text
cargo test -p nlos-topic（05ff1ff 后）
  → topic_service 10 passed；topic_cascade 8 passed；topic_fault_injection 14 passed
  → 合计 32 passed / 0 failed / 0 ignored

cargo clippy --workspace --all-targets -- -D warnings → 0 warning / 0 error
cargo fmt --all --check → 通过
```

kill-window 矩阵覆盖：pre-commit IOERR/ENOSPC（create/subscribe/advance/publish）typed fail-closed 零幻影行；publish `PENDING_ENQUEUE` 跨权威 PowerLoss 双向收敛（不可见方向 channel key 域幂等补投；kill-9 可见方向 `Replayed` 不重复）；torn WAL tail 双侧代表点扫描（topic 9+9、channel 12）；replay storm 幂等；advance CAS kill-window 无越界/回退中间态；compact 崩溃窗口 min-live-cursor 钳制不破坏。

## 3. Canonical commits / push 状态

- `89f966e` feat: add topic service single-log fanout prefix
- `345a959` feat: bind topic republish to parent cascade budget
- `05ff1ff` test: cover topic authority kill-window fault matrix
- 文档同步提交随本节落地后统一 push；各级状态以 commit receipt 为准，不互相冒充。

## 4. 明确未完成（PARTIAL_PASS 保持）

- delivery attempts 仅声明未执行；运行时自动 republish 触发未做（republish 为 owner 显式调用）；
- payer 为 opaque typed binding + 存在性校验，计量/扣费与 ResourceAccount 集成延后（挂 `RSM-METER-002` AttributionPolicy 后续 ADR）；
- Topic 匹配谓词/兴趣订阅、retention 策略执行、commit+wakeup 运行时接线（依赖 B-PROCESS wait registry）、跨进程/多写者/多机、真实掉电、CI/部署均未决；
- kill-9 后页面缓存存活，PowerLossAfter/WAL 截断为模型化丢写，非 fsync 级断电复现。

## 5. 消费身份绑定（2026-08-28 增量，commit `3d42dc6`）

- **缺口**：advance/unsubscribe 原只凭调用者报名 `subscriber_key` 即可推进游标/取消订阅；`SubscriptionId` 为公开派生值，不构成身份证明。
- **修复**：subscribe 签发 authority 派生 consumption token（domain-separated SHA-256：`subscription_id ‖ subscription_generation`，镜像 channel fencing token 风格）；新增 `advance_with_token` / `unsubscribe_with_token` additive 入口，token 不匹配 `ConsumptionTokenMismatch` 零写入 fail-closed。代次语义：首订 generation=1，重订 +1 换发，旧 token fail-closed（测试钉住）。
- **取舍**：token-free 旧入口保留（镜像 channel「新变体强制、legacy 保留」先例）；token-free 入口的弃用为已登记后续项。schema v3 幂等预检迁移，既有行确定性重推导 token。poll 保持零写无鉴权（声明边界）。
- **已知限制**：token 为单机对称证明，非加密签名/跨进程认证；`inspect_subscription` 返回含 token 记录（单 owner 语义成立，多租户暴露需收口）。
- **验证**：nlos-topic 39 passed / 0 failed（新增 7 项 consumer_binding，既有 32 项零修改）；workspace clippy -D warnings 零警告；fmt 通过。

## 6. Token-free 旧入口弃用（2026-08-28 增量，commit `a356f97`）

- `advance` / `unsubscribe` 加 `#[deprecated]`（note 指引 `*_with_token` 并说明冒充风险）；全 workspace 无 crate 外使用方、生产代码零内部调用（薄封装委托非弃用 inner）；既有测试文件机械 `#![allow(deprecated)]` 零逻辑改动。
- 新增 2 项双向行为等价抽测（弃用入口 ↔ token 入口同请求 replay 收敛，receipt 逐字节相等）。
- 验证：nlos-topic 41 passed / 0 failed；workspace clippy -D warnings 零警告；fmt 通过。
- Push 状态说明：增量 45/46 共 3 个提交（`178f25e` 之前为 `3d42dc6`/`178f25e`，本增量 `a356f97` + 本文档提交）在 push 时遇 github.com 443 不可达（两次重试失败），先落地本地，恢复后统一 fast-forward 推送，各级状态以实际 push 结果为准。

## 7. delivery attempts 执行（2026-08-29 增量，commit `c74e5f4`）

- 语义权威：[ADR-0007 补记](../../management/adrs/0007-topic-service-single-log-fanout.md)（2026-08-28 用户未即时应答，按推荐语义执行并登记复审触发器）：滞后订阅者在每条新 publication 落地时计费 +1（首条无积压免费、追平不清零），达到策略 delivery_attempts 同事务翻转 `QUARANTINED`。
- 隔离语义：poll 对隔离订阅者 `DeliveryQuarantined` fail-closed；advance/unsubscribe 权利保留；peer 订阅者不受影响；发布者永不受阻；隔离行停止计费；零消息删除。恢复：`reinstate_with_token` 清零计数、cursor 保持、幂等重放、错 token 零写入。
- schema v4 幂等迁移（既有行回填 0/ACTIVE）；`topic_service` 3 项既有测试按计费语义最小修正（订阅重放断言改用当前存储记录 / A 追平后再发布 / compact 重放改用已生效水位），意图原样保留。
- 验证：nlos-topic 50 passed / 0 failed（新增 topic_delivery_attempts 9 项）；workspace clippy -D warnings 零警告；fmt 通过。
- 已知限制：计数为 owner 单机确定性语义，无运行时投递循环参与；恢复为显式动作非自动；poll 无鉴权边界不变。
