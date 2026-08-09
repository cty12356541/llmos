# B-TASK-006D：Artifact prepared finalize 与 nested Task receipt

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`[TASK-COMMIT-001]` / `[TASK-COMMIT-002]` TaskCommitReceipt 子集、`[TASK-TXN-001]` recoverable finalize 子集
>
> 实现：`crates/nlos-task` schema v7，承接 B-TASK-006A/006B/006C

## 1. 本切片目标

把完整 Artifact publication evidence 晋级为一个不可分割的 TaskAuthority terminal decision：只有 `READY` plan 才能在同一 SQLite 事务中写 TaskCommitReceipt、链接 nested Artifact receipts、关闭 CommitPermit、推进 TaskHead、提交 Attempt 终态并将 plan 标为 `FINALIZED`。重启重放必须返回同一份完整 receipt，不得重复推进 TaskHead。

## 2. 已实现事实

1. **typed prepared finalize**：`finalize_artifact_commit` 只接受 authority-derived `plan_id + finalized_at_ms`；TaskHead 新序号、effect-history root 与 retry fence 均从 durable Task/Permit 事实派生，不接受调用方伪造 root。
2. **READY 门槛**：`PLANNED`/`PUBLISHING` 返回 typed `ArtifactCommitPlanNotReady`；只有 expectation 数量与逐项绑定均完整的 `READY` 可终态化。
3. **terminal transaction**：Task receipt insert、permit `ISSUED → CLOSED`、Attempt `COMMITTED`、TaskHead/control epoch 更新、plan `READY → FINALIZED` 与 `task_receipt_id` 链接位于同一 `BEGIN IMMEDIATE`。
4. **nested receipt view**：`ArtifactTaskCommitReceipt` 同时返回 `TaskReceiptRecord` 和 canonical-order nested `NestedArtifactPublicationReceipt` 集；plan 公开 `task_receipt_id` 以支持重启后追溯。
5. **幂等重放**：已 `FINALIZED` plan 从 durable link 读取原 Task receipt 与 nested receipts，返回 `Replayed`，不会使用新时间戳或再次推进 TaskHead。
6. **membership 解冻**：group freeze 只覆盖 `PUBLISHING/READY`；terminal transaction 提交 `FINALIZED` 后 Admission/Removal 恢复，且新的 group binding 从最新 membership 重新生成。
7. **失败原子性**：在 plan terminal CAS 注入 SQLite trigger failure 时，先前已执行的 Task receipt、permit closure、Attempt/TaskHead 更新全部回滚；解除故障后同一 `READY` plan 可正常收敛。

## 3. 验收测试

新增或扩展覆盖：

- `PUBLISHING` finalize typed 拒绝且没有 terminal side effect；
- `READY` finalize 同时产生 Task receipt、nested publications、closed permit、TaskHead `0 → 1` 与 `FINALIZED + task_receipt_id`；
- authority restart 后 finalize exact replay 返回逐位相同完整 receipt；
- terminal plan update 故障导致全事务回滚，解除故障后可重试收敛；
- grouped plan finalize 后 membership freeze 解除并允许新 Attempt Admission。

本地验证：`nlos-task` 105 项 integration tests、crate Clippy 与 rustfmt 通过；全仓结果以本 canonical commit 最终验证为准。

## 4. 证据等级与限制

单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- 两个独立 SQLite authority 已形成原子事务；Artifact head 可能先于 Task finalize 推进，依靠后续 coordinator 重试收敛；
- 已实现 durable coordinator/outbox、启动扫描、自动 publication/finalize 或 compensation；
- ArtifactAuthority 会验证 TaskAuthority authorization signature/token；
- 混合 Artifact + Effect write set 已获支持；
- 已覆盖此新路径的 VFS kill-9/ENOSPC/I/O/torn-write 矩阵或三平台 CI。

## 5. 下一步

增加单机 cross-authority coordinator：按 durable plan 状态执行 authorize → Artifact publish/replay → Task receipt consume → finalize；进程在每个窗口崩溃后，重启扫描必须从 `PLANNED/PUBLISHING/READY/FINALIZED` 的真实前缀幂等收敛，并显式暴露无法自动推进的冲突。
