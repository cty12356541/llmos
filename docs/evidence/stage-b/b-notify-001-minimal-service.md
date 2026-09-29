# B-NOTIFY-001：Notification 最小服务（Topic/Channel/Wait 薄服务层）

> 状态：`PARTIAL_PASS`（单机 SQLite 重启级 `H3`）
>
> 日期：2026-09-21
>
> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W33-D 车道行（X-1 前半，验收门「订阅/投递/ack 全部经既有 Topic 权威」）；[§6.5.4 决策点 2](../../management/stage-b-progress.md#654-决策点状态2026-09-20-更新)已确认裁剪口径：X-1 Notification = Topic 薄层
>
> 实现：`crates/nlos-notify/`（新 crate，`NotificationService` 薄服务层 + 自有 `notify-service.db` schema v1 引用表）+ 根 `Cargo.toml` members 追加（本轮唯一写该文件的车道）
>
> 依赖前序：B-TOPIC-001（被包裹的 Topic 权威）、B-CHANNEL-001（Channel 权威）、B-WAIT-001（Wait 权威）；[ADR-0007](../../management/adrs/0007-topic-service-single-log-fanout.md)（fanout 归属——本切片零 fanout 机制）、[ADR-0008](../../management/adrs/0008-durable-wait-registry-authority.md)（wait registry 语义 + commit 侧 notify 纪律）

## 1. 本切片目标与边界

决策点 2 钉死的边界：Notification 是既有 Topic 权威之上的**薄层**——不新增 fanout 机制（单 log + per-subscriber cursor 属 ADR-0007/B-TOPIC-001，本切片零触碰）、不复制任何权威事实。每个 canonical 事实都在权威库里：订阅/游标/投递状态是 `nlos-topic` 行，消息 log 是 `nlos-channel` 队列，「等到序列 N 唤醒我」是 `nlos-wait` 行。`nlos-notify` 只持久化**薄层引用**：单表 `(notification_id, topic_id, subscriber_key, consume_token, registered_at_ms)`，无状态、无游标、无队列、无载荷列。

## 2. 写集清单

- `crates/nlos-notify/**`（新 crate：`src/lib.rs` + `src/schema.rs` + `tests/notify_service.rs` + `Cargo.toml`）
- 根 `Cargo.toml`（仅 members 追加 `crates/nlos-notify`）
- `docs/evidence/stage-b/b-notify-001-minimal-service.md`（本文件）+ `docs/management/evidence-index.yaml`（追加收录）

其余文件零改动；被包裹的三个权威 crate 源码零改动（回归见 §5）。

## 3. 实现事实

- **订阅面（create/list/cancel 全经 Topic 权威）**：`subscribe` 先走 `TopicAuthority::subscribe`（准入、订阅点、代际、签发 token 均为权威决策），再把派生引用行 upsert 进 `notify-service.db`；`NotificationId = SHA-256("nlos/notify/id/v1" ‖ topic_id ‖ subscriber_key)[..16]`（确定性派生，重启/重订同 id）。`list_subscriptions` 逐行经 `inspect_subscription` 读**活**权威状态（out-of-band 退订以 `authority.active == false` 透出，非缓存快照）；权威行已不可解析的引用显式回报 `Dangling`（不静默丢弃、不伪造状态）。`cancel` 经 `unsubscribe_with_token`（权威复核存储凭证对当前代际），成功后仅删本层引用行——权威的退订审计行是唯一 durable 事实，删除不丢事实。
- **投递观察（零写读路径）**：`poll(notification_id, limit)` 是 `TopicAuthority::poll` 的逐字转发（订阅游标过滤共享单 log 窗口）；本层无任何缓冲可重放。
- **ack（既有 ack 机制，零新 canonical 状态）**：`ack` 经 `advance_with_token`（既有 Topic ack：每订阅者游标 CAS + 归账 ledger 同事务）；存储凭证被越权重订掏旧时权威 `ConsumptionTokenMismatch` fail-closed，face 重订阅后自愈刷新。
- **wait 接线（ADR-0008 §2 纪律的 commit 侧落位）**：`register_delivery_wait` 经 `WaitAuthority::register_wait`（channel 由 `inspect_topic` 活读；wait 行/幂等/状态机全在权威，本层零 wait 记录）。`publish` 面 = `TopicAuthority::publish`（唯一 enqueue，ADR-0007 单 log）+ 紧随的显式幂等 `notify_commits`（notify key = `SHA-256("nlos/notify/commit-notify/v1" ‖ publication key)[..16]`，确定性 ⇒ publish 重放重报原 wake 集、无二次 enqueue、无二次翻转）。
- **跨权威纪律**：authority-first + 幂等重放收敛——subscribe/cancel/publish→notify 的本层步骤都在权威事实之后，两步窗口崩溃由同请求重放收敛（publish 的 enqueue 已提交而 notify 失败时，Err 随真实 enqueue 状态返回，同 key 重放收敛；与 B-TOPIC-001 PENDING_ENQUEUE 同族同解法）。读路径读时重 derive 引用 id，不合一律 `CorruptRecord` fail-closed。
- **门内证伪**：`no_shadow_fanout_delivery_reads_only_authority_state` 双断言——(a) 两订阅者投递结果为**同一条**共享 log 条目（逐字段相等），channel `max_sequence` 恰 +1、活条目恰 1 条（无 per-recipient 拷贝）；(b) 结构性负证：`notify-service.db` 的 `sqlite_master` 仅含 `notify_subscriptions` 一张表（无任何 queue/message 存储可在本层影子投递）。

## 4. crate 面（公开 API）

```text
NotificationService::open(root, Arc<TopicAuthority>, Arc<WaitAuthority>)
  subscribe(SubscribeNotificationRequest) -> NotificationSubscribeDecision
  list_subscriptions() -> Vec<NotificationSubscriptionView>   // Live{引用+活权威行} | Dangling{引用}
  cancel(CancelNotificationRequest)  -> NotificationCancelDecision
  poll(notification_id, limit)       -> Vec<QueueEntryRecord> // 权威单 log 窗口逐字转发
  ack(AckNotificationRequest)        -> NotificationAckDecision
  register_delivery_wait(RegisterDeliveryWaitRequest) -> nlos_wait::RegisterDecision
  publish(PublishNotificationRequest)-> NotificationPublishDecision // 权威 enqueue + wait notify 回执
```

类型失败面 `NotifyError`：`Topic(TopicAuthorityError)` / `Wait(WaitAuthorityError)` 原样透传 + 本层 `NotificationNotFound(NotificationId)`（未注册或已 cancel）+ 存储/Corrupt/LockPoisoned 家族。

## 5. 验证（TDD：测试先写，编译红→实现→绿）

```text
cargo test -p nlos-notify
  → notify_service 12 passed / 0 failed
cargo test -p nlos-channel -p nlos-topic -p nlos-wait   # 被包裹权威零回归
  → channel 31 passed（3+13+4+11）；topic 85 passed（12+8+9+9+9+14+11+13+10）；wait 26 passed（13+13）
cargo clippy -p nlos-notify --all-targets --all-features -- -D warnings → 0 warning / 0 error
cargo fmt -p nlos-notify --check → 通过
```

链路测试名（W33-D 验收门映射）：

| 测试 | 覆盖 |
|---|---|
| `subscribe_wait_publish_deliver_ack_full_chain` | 订阅→durable wait→publish（权威 enqueue+notify）→投递→ack 全链；再 poll 空（无本层缓冲） |
| `publish_replay_reenqueues_nothing_and_rereports_the_original_wake` | 同 key 重放：publication/wake 逐字段原样，`max_sequence` 不动 |
| `subscription_reference_records_replay_across_restart` | 权威+face 全体重启：引用逐字段 replay、poll/ack 继续路由、订阅重放幂等 |
| `no_shadow_fanout_delivery_reads_only_authority_state` | 门内负证：双订阅者共享单条 + face 库结构性无 queue 表 |
| `cancel_routes_through_topic_authority_and_removes_the_reference` | cancel 经权威 token 退订；引用删除；重订同 id 新代际新 token |
| `list_reports_live_authority_state_through_the_thin_reference` | out-of-band 权威退订经同一引用透出 `active=false`（无第二事实源） |
| `list_surfaces_dangling_references_when_the_authority_diverged` | 权威失配引用显式 `Dangling` + 路由 typed 失败 |
| `typed_failures_for_unknown_topic_and_unknown_notification` | 未知 topic 经 `Topic(TopicNotFound)`；未知 id 全路由 `NotificationNotFound`；拒绝零残留 |
| `typed_failures_for_ack_sequence_bounds` | 越界 ack `InvalidSequence` 零 durable 移动；回退 ack 游标保持 |
| `stale_stored_token_fails_closed_then_self_heals_on_resubscribe` | 陈旧凭证 `ConsumptionTokenMismatch` fail-closed；face 重订阅自愈后 ack 成功 |
| `typed_failures_for_delivery_wait_registration` | target=0 / 零 binding 经 `Wait(…)` typed 透传 |
| `publish_notify_flips_only_covered_waits` | notify 按各 wait 的 target 精确翻转（target-3 wait 在 seq 2 后仍 PENDING） |

## 6. 明确未完成（PARTIAL_PASS 保持）

- **无本层信号级 kill-window 矩阵**：本层打开的跨权威窗口（subscribe/cancel 的权威后本层步、publish→notify）按 §3 幂等重放收敛语义以逻辑级 replay 测试覆盖；dm-flakey/kill-9 级矩阵如波屏障要求再补（权威侧矩阵已覆盖各自窗口）。
- **接线假设由 host 承担**：`WaitAuthority` 必须与 `TopicAuthority` 绑定同一 `ChannelAuthority`（face 无法观测该绑定，doc 注明）。
- **未上浮的权威面**：pattern 订阅、reinstate、republish、credit、compact 未在本层开面（权威直接使用仍可用；按最小版裁剪）。
- IPC/GUI 面、跨进程、真实掉电、CI 三平台 run 链接补登均为波屏障固定动作，不在本车道写集内。
