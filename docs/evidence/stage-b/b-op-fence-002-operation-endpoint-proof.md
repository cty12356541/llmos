# B-OP-FENCE-002：Operation owner endpoint proof/readback

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- Owner：`SqliteOperationStore`
- 关联 Requirement：`MODEL-OP-001`、`TASK-EFFECT-001`、`TCB-ENDPOINT-001`
- 关联 ADR：[ADR-0002](../../management/adrs/0002-stage-b-sqlite-operation-authority.md)

## 证明范围

本切片为已有 durable Operation registration row 增加 owner-derived
`OperationEndpointProof` readback。`SqliteOperationStore::inspect_endpoint_proof`
先按 `OperationId + Generation` 读取并恢复完整 `OperationSpec`，再派生稳定的
`TaskParticipantId`、`participant_generation` 和 `admission_receipt_id`。proof 同时
携带 owner Fiber 与 cancellation scope/generation，消费者可以据此把 Operation
端点与当前 owner/fence 绑定，而不是接受调用者自填的 opaque tuple。

proof 的派生公式使用 domain-separated SHA-256，并以 Operation registration row
作为 durable source of truth；同一数据库重启后回读相同 proof。不存在的
Operation 或旧 generation 在 proof 生成前 fail closed。

## 验证

- `crates/nlos-store/tests/operation_store.rs` 覆盖首次 readback、非零 participant/receipt、旧 generation 拒绝，以及关闭并重开 authority 后的逐字段相等回读。
- `cargo test -p nlos-store --all-targets --quiet` 通过。
- `cargo clippy -p nlos-store --all-targets --all-features -- -D warnings` 通过。

## 明确未完成

这不是 Operation `prepare → activate`、Driver authentication、Channel endpoint、
EffectPermit dispatch 或跨 authority TaskWriteSet 接线。proof 是 owner-derived
readback，不等于跨进程签名/attestation、lease/takeover fence，也不生成
`TaskCommitReceipt` 或 publication receipt。后续 TaskAuthority 接线仍必须在
participant registry 中注册该 proof，并在 permit/finalize 前重新回读。
