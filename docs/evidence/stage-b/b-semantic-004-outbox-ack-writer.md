# B-SEMANTIC-004：Semantic outbox owner ACK writer

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：单节点 Semantic admission outbox 的 owner-bound、单调 transport acknowledgement；不等同于 Semantic checkpoint/publication receipt 或 `TaskCommitReceipt` 消费。

## 已实现事实

`SemanticAuthority::acknowledge_outbox` 在同一 `BEGIN IMMEDIATE` 事务内重新读取并校验 Event、AdmissionReceipt 与 outbox 的 `(event_id, log_seq, receipt_id)` 三元绑定。ACK 时间不得早于 admission，也不得回退；相同时间返回 `Replayed`，更晚时间推进 `acknowledged_at_ms` high-water 并返回 `Recorded`。

`inspect_outbox` 同时校验 outbox 自身的 `event_id`，避免只凭查询键掩盖 durable row 内的身份漂移。ACK writer 不修改 Semantic event log、AdmissionReceipt 或 DurabilityReceipt，不生成 checkpoint/publication proof。

## 验证

- `outbox_ack_is_owner_bound_monotonic_and_not_publication_proof` 覆盖 admission 前 ACK 拒绝、绑定冲突、单调推进、重复回放、重启回读和 ACK 仍只是 transport observation。
- `cargo test -p nlos-semantic --all-targets --quiet`
- workspace fmt、Clippy 与全量测试在提交前复验。

## 明确缺口

仍未实现 outbox consumer 的跨进程认证/租约、Semantic checkpoint producer、不可变 `SemanticPublicationReceipt`、跨 authority publication/finalize 或 `TaskCommitReceipt.semantic_publications` 消费；这些仍需明确 Semantic publication authority ownership。
