# B-TASK-008C2F：Semantic admission-policy 声明与 owner 绑定

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：补齐 `[TASK-WRITE-003]` 要求的 Semantic staging `expected admission policy` 声明；不等同于 Semantic publication receipt 或最终 `TaskCommitReceipt` 接线。

## 结论

`TaskWriteSetSemanticAppendRequest` 现在必须携带
`expected_admission_policy_digest`。TaskAuthority seal 从 SemanticAuthority
回读 `AdmissionReceipt.authz_policy_digest`，只有与 caller 声明逐位一致时才
将 policy digest 写入 owner-derived `TaskWriteSetSemanticAppend`；不一致会
fail closed，caller 不能把任意 policy ID 当成 authority fact。

schema v23 为 Semantic append child 增加 nullable
`admission_policy_digest`。新 seal 使用 append-root v3，把每个 append 的
policy presence/bytes 纳入 root；v1/v2 历史行继续保留 `NULL`，迁移不补造
policy 事实。load、replay、permit issuance 和 Semantic-aware finalize 均按
同一 root/owner policy 规则复核。

## 验证

- participant fixture 覆盖 owner policy digest 回读、policy mismatch fail-closed、
  append-root/permit binding、Semantic-aware finalize 和 terminal replay。
- `cargo test -p nlos-task --test participant_registry --quiet`
- `cargo test -p nlos-task --quiet`
- `cargo test --workspace --all-targets --quiet`
- `cargo fmt --all -- --check`
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`

## 明确缺口

SemanticAuthority 仍没有 checkpoint producer、publication transaction/finalize
或 public SemanticPublicationReceipt API；不能把 outbox acknowledgement、
AdmissionReceipt 或 DurabilityReceipt 推断为已发布。下一门仍需决定
publication receipt 的生产/消费权威，再接入
`TaskCommitReceipt.semantic_publications`；Artifact/Resource/Operation/Channel
与完整 TaskWriteSet 仍未完成。
