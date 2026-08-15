# B-SEMANTIC-005：Semantic publication receipt producer

- 日期：2026-08-16
- 状态：`PARTIAL_PASS`
- Owner：SemanticAuthority
- 关联：`B-TASK-008C2G`、`ADR-0006`、`TASK-WRITE-003`、`TASK-COMMIT-002`
- 证据范围：单节点 SQLite WAL/FULL reference authority；不是跨 Cell 或生产级 Semantic View

## 目标

由 `SemanticAuthority` 生成 canonical `SemanticPublicationReceipt`，让 publication 事实继续留在 Semantic 域；`semantic_outbox.acknowledged_at_ms` 保持 transport observation，不被提升为 publication proof。

## 实现事实

- Semantic schema 从 v3 增至 v4，新增 immutable `semantic_publication_receipts` 表、唯一 `(task_id, permit_id, event_id)` 约束和 update/delete 拒绝 triggers。
- `PublishSemanticPublicationRequest` 只携带 Task/Permit/WriteSet 绑定、EventId、目标和 owner receipt IDs；producer 重新读取 Event、scope、durable AdmissionReceipt 与可选 DurabilityReceipt，caller 不能注入事实字段。
- `semantic_checkpoint_after` 由 owner 对当前 append-only `event_log` 前缀计算确定性 digest；它是本地 log-prefix checkpoint，不是跨 Cell 全局标量或 vector checkpoint。
- receipt ID 由 Task/Permit/WriteSet/Event/Admission/Durability/checkpoint 绑定字段确定性派生；相同请求重放原 receipt，绑定漂移 fail closed。
- publication receipt 不修改 Event、AdmissionReceipt、DurabilityReceipt 或 outbox ACK；authority 重启后按 receipt ID 逐字段回读。

## 验证

本地命令：

```text
cargo test -p nlos-semantic --quiet
  1 unit test + 12 semantic authority tests + 5 spec tests passed

cargo clippy -p nlos-semantic --all-targets --all-features -- -D warnings
  passed
```

新增集成覆盖：

1. owner target/AdmissionReceipt 绑定与 direct durable admission；
2. local checkpoint 非零、receipt identity 和 `created_at_ms` exact replay；
3. wrong target / wrong AdmissionReceipt typed rejection；
4. authority restart readback；
5. storage-level immutable update/delete rejection。

## 未完成与限制

- TaskAuthority 尚未消费该 receipt，也未把它嵌入 nested `TaskCommitReceipt.semantic_publications`。
- 尚无 Semantic publication plan 的跨 authority prepare/consume、PENDING_PROJECTION/UNCERTAIN recovery、故障注入或多 receipt 批次原子收敛。
- 当前 checkpoint 只证明单 authority 的连续事件日志前缀；TrustPolicy、签名 vector checkpoint、跨 Cell shard coverage、跨进程认证/租约和独立 Semantic broker 均未实现。
- 本证据不能把 Semantic publication 或完整 TaskWriteSet 晋升为 `DONE`，仅证明 producer 局部 `PARTIAL_PASS`。
