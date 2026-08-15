# B-TASK-008C2G-OP：Operation endpoint 的 TaskWriteSet / participant registry 接线

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：schema v24 将 owner-derived Operation endpoint 纳入
  `TaskWriteSet` per-effect endpoint 与 Task participant registry，并在 seal
  和 permit freeze 前做精确 `OperationId + Generation` owner readback。

## 结论

`SqliteTaskAuthority` 新增 Operation participant registration、Operation-aware
TaskWriteSet seal 以及 Operation-aware CommitPermit API。TaskAuthority 不接受
caller-supplied participant tuple：它通过 `SqliteOperationStore::inspect_endpoint_proof`
读取 durable registration row，派生并复核 `TaskParticipantId`、generation 和
admission receipt，再确认该 participant 已存在于 OPEN registry。旧 generation
在 proof 生成前被拒绝；permit replay 返回原决定，不重复 owner readback。

schema v24 只扩大既有 immutable `participant_type`（1..8）和
`endpoint_kind`（1..6）约束，迁移复制历史行并保留 immutable triggers；没有给
历史数据补造 Operation 事实。

## 验证

- `verified_operation_endpoint_is_rechecked_during_seal_and_permit`：Operation
  participant registration、stale generation seal 拒绝、owner-aware seal、无
  authority permit 拒绝、正确 authority permit/replay 与 Operation/Task 重启回读。
- `cargo test -p nlos-task --test participant_registry verified_operation_endpoint_is_rechecked_during_seal_and_permit -- --nocapture`
- `cargo test -p nlos-task --test artifact_commit_plan --quiet`（旧版本迁移兼容）

## 明确缺口

该切片不实现 Operation prepare→activate/dispatch、跨进程签名/租约/attestation、
Channel endpoint、Operation effect completion，也不消费 Semantic/Artifact/Resource
publication receipt；因此不等同于完整 TaskWriteSet 或统一 `TaskCommitReceipt`。
