# B-TASK-008C2E：Semantic 收据终结前权威复核

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：在 TaskAuthority 的 v3 permit finalize 入口增加 Semantic owner-proof re-read guard；不等同于 Semantic publication receipt、checkpoint producer 或完整 `TaskCommitReceipt`。

## 结论

`SqliteTaskAuthority::finalize_commit_v3_with_semantic_authority` 对仍处于
`ISSUED` 的 permit 按 `write_set_root` 重新读取 sealed `TaskWriteSet`，并在
Task CAS 前由 `SemanticAuthority` 逐项回读并比对：

- admitted event 的 target scope 与 sealed declaration；
- event log sequence、`AdmissionReceipt` identity 和 `Durable` durability；
- 若声明存在，则对应 event 的 immutable `DurabilityReceipt` identity。

任一 owner fact 缺失或不一致都会 fail closed，不会推进 `TaskHead`。已经
`CLOSED`/`QUARANTINED` 的 permit 不重复访问 owner authority，继续沿用原有
幂等 replay/tombstone 语义。

该 guard 只复核已有 authority facts，不确认 `semantic_outbox` acknowledgement，
不创建 `SemanticCheckpoint` 或 publication receipt，也不把 Task receipt 扩展成
“已发布”声明。direct `AdmissionReceipt(durability = Durable)` 与已绑定的可选
`DurabilityReceipt` 两条局部路径均保留。

## 验证

- `verified_write_set_seal_binds_semantic_event_readback_and_append` 覆盖带
  `DurabilityReceipt` 的 sealed append 通过 Semantic-aware finalize、Task receipt
  为 `Committed`，以及相同 finalize 的精确 replay。
- `cargo test -p nlos-task --test participant_registry --quiet`
- `cargo test -p nlos-task --quiet`
- `cargo test --workspace --all-targets --quiet`
- `cargo fmt --all -- --check`
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`

## 明确缺口

SemanticAuthority 仍没有 checkpoint producer、跨 authority publication
transaction/finalization 或 public publication receipt API；outbox intent、
AdmissionReceipt、DurabilityReceipt 不能互相推断为最终发布。Artifact
publication receipt consumption、Resource activation/consume/finalize、
per-effect Operation/Channel linkage、phantom/range validation、跨 authority
prepare→activate、term takeover 与完整 TaskWriteSet 仍未完成。下一门需要先
确定 Semantic publication receipt 的生产/消费边界，再把它接入
`TaskCommitReceipt.semantic_publications`。
