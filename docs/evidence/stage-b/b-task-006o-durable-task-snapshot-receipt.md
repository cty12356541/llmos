# B-TASK-006O：durable TaskSnapshotReceipt

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`[TASK-SNAPSHOT-002]`、`[TASK-COMMIT-001]`

## 已实现事实

1. `nlos-task` schema v10 新增 immutable `task_snapshot_receipts` 与 ordered/unique per-authority checkpoint Receipt 集；记录 snapshot/head、durable effect-history root、retry fence、builder/version、dependency closure、semantic resolver、canonical iteration、achieved consistency、authority/key/signature bytes。
2. 注册时 TaskAuthority 逐位核对当前 Task head/history/fence，拒绝 stale snapshot、空/重复/超界 checkpoint 集、负时间、ReceiptId 或 SnapshotId rebinding。
3. 新 `register_attempt_with_snapshot_receipt` 路径把 receipt ID 持久写入 attempt；重启后精确回读，同一 idempotency key 不能在 receipted/legacy 路径之间切换。
4. `MIXED_NON_SETTLEABLE` receipt 可以作为失败/降级事实保存，但不能授权 attempt；旧 schema attempt 迁移后明确保持 `snapshot_receipt_id = NULL`，不会被伪造为已有证明。
5. header 与 checkpoint child rows 都有 DDL immutability trigger；v1–v9 数据库沿既有迁移链进入 v10。

## 验证与边界

3 项新 integration tests 覆盖 durable replay/attempt binding、stale/incomplete/conflicting/MIXED 拒绝和 DDL immutability；既有 v1/v2/v3/v4 与 v5–v8 migration fixtures 均继续迁移至当前 schema。

本证据为单节点本地 H3 / `PARTIAL PASS`。本切片持久化并执行 receipt 结构与 TaskAuthority binding，不包含真实多 authority snapshot builder、checkpoint authority 查询、Identity/key trust 验签、canonical deterministic-CBOR preimage、有效期或完整 TaskSnapshot causal-dependency 内容采集；这些未完成前不得把 caller-supplied signature bytes 宣称为已验证签名。
