# B-SEMANTIC-003：Semantic outbox owner 回读

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：公开已提交 Semantic admission outbox item 的 owner-consistent transport readback；不等同于 outbox ACK、Semantic checkpoint、publication receipt 或 `TaskCommitReceipt` 消费。

## 已实现事实

`SemanticAuthority::inspect_outbox` 按 `event_id` 回读 durable outbox row，并同时校验 event log sequence、`AdmissionReceipt` identity 与 outbox 的 `log_seq`/`receipt_id` 一致。未知 event、缺失 outbox row、负值/错误宽度字段或三者绑定不一致均 fail closed。

返回的 `acknowledged_at_ms` 只是 transport observation；该 API 不修改状态，也不把 outbox intent 或 ACK 解释为 Semantic publication proof。authority 重启后可回读相同 outbox identity 与 transport 状态。

## 验证

- `admission_is_durable_signed_atomic_and_exactly_replayable` 覆盖首次 admission、当前进程回读及 authority 重启后的回读。
- `cargo test -p nlos-semantic --quiet`
- `cargo clippy -p nlos-semantic --all-targets --all-features -- -D warnings`

## 明确缺口

尚未实现 outbox consumer/ACK writer、Semantic checkpoint producer、immutable `SemanticPublicationReceipt`、跨 authority publication/finalize 或 `TaskCommitReceipt.semantic_publications` 消费；这些仍需明确 Semantic publication ownership 后才能推进。
