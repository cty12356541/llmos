# C-NOTIFY-001 / W65-NOTIFY-EXT：超最小通知面扩展——后台任务生命周期通知、一次性绑定动作令牌与离线历史分页（TASK-NOTIFY-001, TASK-STATE-001, DUI-NOTIFY-001 & DUI-OFFLINE-001）

- 状态：`PASS`（`[TASK-NOTIFY-001]` / `[TASK-STATE-001]` 16 态后台任务迁移定长二进制载荷与零影子扇出发布/唤醒/轮询、`[DUI-NOTIFY-001]` 绑定 `NotificationId / ApplicationId / TaskId / action_code` 的一次性动作令牌与防回滚 `SQLite` 触发器、`[DUI-OFFLINE-001]` 有界游标分页本地通知历史与单调裁剪水位线，以及 `notify-service.db` `SCHEMA_VERSION 1 -> 2` 原地无损迁移已落地并验证）
- 日期：2026-10-25
- Owner：`C-MULTICELL` #15（`W65-NOTIFY-EXT` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §25 `[TASK-STATE-001]`、`[TASK-NOTIFY-001]` + §24 `[DUI-NOTIFY-001]`、`[DUI-OFFLINE-001]`
- Base HEAD：`5ac73b6333515efbd28b9fa94a4c9eb27df2cf18`

## 1. 实现事实

- `crates/nlos-notify/src/task_notify.rs`（新增）与 `crates/nlos-notify/src/lib.rs`（additive 重导出与扩展面，100% 保持既有 Stage-B 最小订阅面公共 API 与 `#![forbid(unsafe_code)]` 兼容）：
  - **后台 `Task` 生命周期迁移通知与零影子扇出投递（`[TASK-NOTIFY-001]`，`[TASK-STATE-001]`）**：
    1. `TaskTransitionKind`：完整覆盖 `[TASK-STATE-001]` 全部 16 个可观察任务迁移态（`Proposed`、`WaitingConsent`、`Authorized`、`Running`、`WaitingInput`、`WaitingResource`、`WaitingNetwork`、`Suspended`、`Recovering`、`Cancelling`、`Completing`、`Completed`、`Partial`、`Disputed`、`Failed`、`Cancelled`），提供显式 1 字节编码标签（`1..=16`）与终态判定（`is_terminal`）。
    2. `TaskNotificationEnvelope` 与定长规范二进制编解码（`encode_task_notification_envelope` / `decode_task_notification_envelope`）：采用 `TASK_NOTIFICATION_WIRE_MAGIC = b"NLOSTNTF\x01"`（9 字节）与固定总长 `TASK_NOTIFICATION_WIRE_LEN = 91` 字节（绑定 `task_id`、`application_id`、非零 `task_generation`、`transition`、`occurred_at_ms`、`summary_digest`、`requires_user_action`）。对截断/超长字节流、错误魔数、零代数（`ZeroTaskGeneration`）、未知状态标签（`UnknownTransitionTag`）与非布尔动作标志（`InvalidUserActionFlag`）严格 fail-closed。
    3. `NotificationService::publish_task_transition` 与 `NotificationService::poll_task_notifications`：严格复用单一 `TopicAuthority::publish`（`ChannelAuthority` 单日志入队）+ `WaitAuthority::notify_commits` 唤醒接线，不建立任何第二消息队列或每订阅者影子扇出副本，使后台 `Task` 关键状态变化在原聊天窗口关闭时仍可被系统面或通知托盘订阅、唤醒、轮询与确认。
  - **绑定 `Notification / Application / Task` 的一次性动作令牌（`[DUI-NOTIFY-001]`）**：
    1. `crates/nlos-notify/src/schema.rs` 将 `SCHEMA_VERSION` 从 `1` 演进至 `2`（`WAL + FULL` 持久化）：新建空白库初始化 `v1` 薄引用表 `notify_subscriptions` 以维持最小面零额外表开销，重开既有 `v1` 库或首次调用任何 `v2` 能力时通过 `Immediate` 事务原地无损迁移至 `v2`（4 张 `STRICT` 表 + 6 条不可变/防回滚 `SQLite` 触发器）。
    2. `issue_action_token`：先校验 `notification_id` 在 `notify_subscriptions` 中真实存在，再按 `ACTION_TOKEN_DOMAIN = b"llmos/notify-action-token/v1"` 对 `(token_id, notification_id, application_id, task_id, delivery_sequence, action_code, secret_nonce)` 计算域分隔 `SHA-256` 派生 `secret_proof: [u8; 32]`。`notify_action_tokens` 表仅落盘 `secret_proof` 的 `SHA-256` 验证摘要 `secret_commitment`（原始 `secret_nonce` 与 `secret_proof` 永不落盘），并按 `idempotency_key` 幂等重放（冲突重绑返回 `ActionIdempotencyConflict`）。
    3. `redeem_action_token`（及 `revoke_action_token` / `inspect_action_token`）：在 `Immediate` 事务内执行常量时间验证与严格一次性状态跃迁（`ISSUED -> REDEEMED`），对 6 类违规分别以独立强类型 `NotifyError` fail-closed 拒绝：
       - 未知令牌：`ActionTokenNotFound`；
       - 跨 `Application` 伪造响应（即便攻击者持有合法 `token_id` 与 `presented_proof`）：`ActionCrossApplicationSpoof { expected, actual }`，且保持原令牌未消费，防止跨应用伪造与恶意抢占 DoS；
       - 错误 `NotificationId`、`TaskId` 或 `action_code`：`ActionBindingMismatch`；
       - 错误证明摘要：`ActionTokenProofInvalid`；
       - 过期令牌（`redeemed_at_ms > expires_at_ms`）：`ActionTokenExpired`；
       - 已核销令牌二次重放：`ActionTokenAlreadyRedeemed { first_redeemed_at_ms, receipt_id }`（严格 one-shot，绝不二次放行；已吊销令牌返回 `ActionTokenRevoked`）。
    4. 数据库引擎级防篡改触发器：`trg_notify_action_tokens_immutable_bindings` 禁止篡改任何绑定列与 `secret_commitment`；`trg_notify_action_tokens_terminal_once` 禁止 `REDEEMED` / `REVOKED` 终态回滚复活；`trg_notify_action_tokens_no_delete` 禁止删除一次性核销墓碑。
  - **本地离线通知历史与单调裁剪分页（`[DUI-OFFLINE-001]`）**：
    1. `notify_history_refs` 与 `notify_history_prune_watermarks`（`STRICT` 表 + `trg_notify_history_refs_immutable` / `trg_notify_history_prune_monotonic` / `trg_notify_history_prune_no_delete`）：`publish_task_transition` 自动记录（且支持 `record_delivery_history` 显式记录）按 `(topic_id, channel_sequence)` 索引的紧凑摘要引用元数据。
    2. `query_notification_history_page(topic_id, after_sequence, limit)`：按 `channel_sequence ASC` 提供有界确定性游标分页（`limit == 0` 或 `limit > MAX_HISTORY_PAGE_LIMIT (1024)` 立即以 `InvalidHistoryPagination` fail-closed 拒绝）；`prune_history_before(topic_id, retain_from_sequence)` 单调推进裁剪水位线并拒绝水位线回退或向已裁剪区间补写。

## 2. 验证

```text
cargo fmt -p nlos-notify -- --check                                         → 退出码 0
cargo clippy -p nlos-notify --all-targets -- -D warnings                    → 退出码 0，零警告
RUSTDOCFLAGS="-D warnings" cargo doc -p nlos-notify --no-deps               → 退出码 0，零警告
cargo test -p nlos-notify --no-fail-fast                                    → 退出码 0（27 passed / 0 failed）
```

测试构成明细（共 `27` 个测试全部通过）：

- 模块内单元测试 `crates/nlos-notify/src/task_notify.rs`（`2 passed`）：
  1. `all_sixteen_task_transition_tags_roundtrip_and_classify_terminality`：验证全部 16 个 `[TASK-STATE-001]` 迁移态编码标签往返与终态分类。
  2. `task_envelope_fixed_wire_encoding_is_deterministic`：验证 91 字节 `NLOSTNTF\x01` 规范编码往返。
- 新增集成测试 `crates/nlos-notify/tests/c_notify_task_action_token.rs`（`13 passed`）：
  1. `background_task_transitions_publish_wake_and_poll_without_shadow_fanout`：验证多状态任务迁移发布、`WaitAuthority` 唤醒、双订阅者独立游标轮询/确认、幂等重放，以及 `ChannelAuthority` 单日志零影子扇出。
  2. `malformed_and_truncated_task_notification_payloads_fail_closed`：验证截断、超长、坏魔数、零代数、未知状态标签、非法布尔标志及非任务载荷轮询的 fail-closed 拒绝。
  3. `action_token_happy_path_persists_across_reopen_without_storing_raw_secret`：验证动作令牌签发、磁盘字节扫描确认原始 `secret_nonce` / `secret_proof` 零落盘、跨重启持久化核销与二次重启状态保持。
  4. `issue_action_token_verifies_subscription_bounds_and_idempotency`：验证未知 `NotificationId` 拒绝、参数边界拒绝、相同 `idempotency_key` 幂等重放及冲突参数拒绝。
  5. `cross_application_spoof_is_rejected_and_leaves_token_unconsumed_for_owner`：验证 `App B` 持真实令牌与证明冒用 `App A` 响应时以 `ActionCrossApplicationSpoof` 确定性拒绝，且原令牌保持未消费供 `App A` 正常核销。
  6. `wrong_bindings_invalid_proof_and_unknown_token_fail_closed`：逐项验证未知 `token_id`、错误 `NotificationId`、错误 `TaskId`、错误 `action_code` 与篡改 `presented_proof` 的独立类型化拒绝。
  7. `one_shot_token_blocks_second_redeem_and_sqlite_triggers_block_rollback`：验证二次核销返回 `ActionTokenAlreadyRedeemed`，且直接执行原始 SQL 回滚状态、篡改绑定列或删除墓碑均被 `SQLite` 触发器拦截。
  8. `revoked_action_token_cannot_be_redeemed_or_resurrected`：验证行政吊销幂等性、`ActionTokenRevoked` 拒绝核销及触发器阻止复活。
  9. `expired_action_token_is_rejected_and_exact_deadline_succeeds`：验证超期返回 `ActionTokenExpired` 与精确截止时刻边界通过。
  10. `schema_v1_migrates_in_place_to_v2_preserving_existing_subscriptions`：验证 `v1 -> v2` 原地升级无损保留既有 `v1` 订阅与待投递消息，以及未知版本号 `99` 以 `SchemaVersionUnsupported(99)` 拒绝。
  11. `offline_history_cursor_pagination_and_conflict_bounds`：验证自动与显式通知历史写入、幂等重放、同序列摘要冲突拒绝、`limit == 0` / 超上限拒绝及多页游标遍历。
  12. `offline_history_monotonic_retention_pruning_and_sqlite_triggers`：验证 `prune_history_before` 裁剪计数、水位线回退拒绝、已裁剪区间补写拒绝及历史表/水位线表触发器保护。
  13. `pruned_offline_history_survives_restart_and_replayed_publish_converges`：验证裁剪水位线跨重启持久化与已裁剪历史序列上的幂等重放收敛。
- 既有回归测试 `crates/nlos-notify/tests/notify_service.rs`（`12 passed`）：Stage-B 全部 12 项契约测试零修改通过。

## 3. 边界与限制（诚实域）

- 本证据验证 `nlos-notify` 库内 `[TASK-NOTIFY-001]` 后台任务迁移通知编解码与单日志发布/轮询、`[DUI-NOTIFY-001]` 一次性加密绑定动作令牌（`SQLite` `WAL + FULL` 持久化与防回滚触发器）以及 `[DUI-OFFLINE-001]` 本地离线通知历史游标分页与单调裁剪。
- 当前 workspace 内生产消费方（`nlos-task` / `nlos-slice-k` / 系统控制与桌面壳层）的跨 crate 在线装配接线按计划随 `W66` 紧接落地；本切片不声称图形化通知弹窗渲染或跨物理机推送网关已完成。

## 4. 复现

```text
cargo fmt -p nlos-notify -- --check
cargo clippy -p nlos-notify --all-targets -- -D warnings
cargo test -p nlos-notify --no-fail-fast -- --nocapture
```
