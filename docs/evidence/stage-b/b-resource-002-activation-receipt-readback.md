# B-RESOURCE-002：Resource activation receipt owner 回读

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：为已激活 `Reservation` 提供可重启回读的 immutable activation receipt；不等同于 Resource consume/finalize、TaskCommitReceipt 嵌套或完整 TaskWriteSet。

## 已实现事实

`ResourceAuthority::inspect_activation_receipt` 只接受 durable `ACTIVE` Reservation，读取其 immutable activation receipt，并校验 `activation_receipt_id` 与 `operation_id` 同时和 Reservation 行、Receipt 行一致。未知 Reservation、仍为 `RESERVED` 的记录、缺失 Receipt 或绑定不一致均 fail closed。

该回读不创建新 Receipt、不改变 Reservation 状态，也不把 caller-supplied 字段提升为权威事实。authority 重启后从同一 SQLite 数据库回读相同 Receipt，重复激活仍返回原 Receipt。

## 验证

- `activation_consumes_exact_binding_once_and_replays_receipt` 覆盖首次激活、重复激活、当前进程回读以及 authority 重启后的回读。
- `cargo test -p nlos-resource --quiet`
- `cargo clippy -p nlos-resource --all-targets --all-features -- -D warnings`

## 明确缺口

Resource activation 仍没有被 TaskAuthority 消费，也没有 consume/high-water、closing/finalize/refund、跨 authority prepare→activate、Operation/Channel linkage 或统一 `TaskCommitReceipt` publication receipt。Semantic publication receipt producer/consumer 与 `TaskCommitReceipt.semantic_publications` 仍是下一主门，完整 TaskWriteSet 仍未完成。
