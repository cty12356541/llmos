# B-OP-FENCE-002：Operation owner endpoint proof/readback

- 状态：`PARTIAL_PASS`
- 日期：2026-08-23（增量验收）
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

本轮在同一 `SqliteOperationStore` owner authority 上补齐一个受限的
`prepare → activate` durable boundary：`prepare_dispatch` 将 callback、owner
Fiber、cancellation scope/generation 与 cancel epoch 写入 immutable preparation
receipt；`activate_dispatch` 在同一事务中重新读取 Operation owner facts，消费
preparation 并写入 immutable activation receipt，随后才把状态推进到
`DISPATCHED`。旧的直接 `dispatch` 路径在存在未激活 preparation 时被围栏，取消
或 callback/generation 漂移均 fail closed；重启后 prepare/activation exact replay
返回原 receipt/ticket，不重复派发。

## 验证

- `crates/nlos-store/tests/operation_store.rs` 覆盖首次 readback、非零 participant/receipt、旧 generation 拒绝，以及关闭并重开 authority 后的逐字段相等回读。
- `crates/nlos-store/tests/operation_prepare_activate.rs` 的 2 项测试覆盖首次 prepare/activate、重启 replay、callback 冲突、伪造 preparation、旧 generation 与 cancel fence。
- `cargo test -p nlos-store --all-targets --quiet` 通过。
- `cargo clippy -p nlos-store --all-targets --all-features -- -D warnings` 通过。
- `cargo fmt --all -- --check` 与 `git diff --check` 通过；workspace 测试通过。
- 代码提交 `f6530fc`；其并入 `a9f5afe` 后的 Rust cross-platform/MSRV run [32629828391](https://github.com/cty12356541/llmos/actions/runs/32629828391) 与 Pages run [32629828373](https://github.com/cty12356541/llmos/actions/runs/32629828373) 均成功。

## 明确未完成

这只证明单机 SQLite owner 的 prepare/activate 前缀，不是跨 authority 的
TaskWriteSet prepare/finalize、Driver authentication、Channel endpoint、
EffectPermit completion 或统一 `TaskCommitReceipt` 接线。它也不等于跨进程签名/
attestation、lease/takeover fence 或真实掉电；后续 TaskAuthority 接线仍必须把
activation receipt 纳入 participant/effect binding 并在 permit/finalize 前重新回读。
