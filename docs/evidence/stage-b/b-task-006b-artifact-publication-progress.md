# B-TASK-006B：Artifact publication receipt 消费与 partial/ready 状态

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`[TASK-COMMIT-002]` Artifact publication receipts 子集、`[TASK-TXN-001]` PARTIAL/UNCERTAIN 可见性子集
>
> 实现：`crates/nlos-task` schema v7，依赖 B-TASK-006A immutable plan 与 B-ARTIFACT-002 publication receipt

## 1. 本切片目标

把 ArtifactAuthority 已产生的 publication receipt 作为 nested durable evidence 逐项消费进 TaskAuthority；多 Artifact 只完成一部分时必须显式显示 `PUBLISHING`，完整集合才进入 `READY`，不能把部分发布隐藏成失败、未开始或完整 Task commit。

`READY` 仍不等于 Task finalized。本切片不关闭 CommitPermit、不推进 TaskHead；下一切片再把 finalize-readiness proof 与 nested receipts 同一个 TaskAuthority 事务绑定到 TaskCommitReceipt。

## 2. 已实现事实

1. **schema v6 → v7 纯增量迁移**：新增 immutable `task_artifact_publication_receipts`，旧 plan 与全部 Task 数据保留。
2. **实现解耦的 receipt envelope**：`NestedArtifactPublicationReceipt` 镜像 Artifact authority 输出，不让 `nlos-task` 依赖具体 `nlos-artifact` crate。
3. **逐项强绑定**：消费前验证 task/permit/write-set root、staging identity、Artifact/revision/digest/size 与 immutable expectation 逐位一致；同时验证 prior/new head revision/digest 自洽。
4. **批次原子性**：一个 receipt batch 中任一项冲突，整个 TaskAuthority 事务回滚，不留下半批 nested evidence。
5. **部分状态真实可见**：收到 1..N-1 项为 `PUBLISHING`，N/N 为 `READY`；两者均保持 permit `ISSUED`、TaskHead 不变。重启后可查询完整 partial 集并继续补齐。
6. **幂等与冲突**：同 staging/同 receipt exact replay 不重复写且不改时间戳；同 staging 异 receipt、同 receipt ID 重绑、非计划 staging 或字段不一致返回 typed `ArtifactPublicationConflict`。
7. **nested evidence 不可变**：DDL 禁止 UPDATE/DELETE；plan inspect 同时重算 expectation root/count，progress inspect 复验每个 stored receipt 与状态/count 一致。

## 3. 验收测试

`artifact_commit_plan.rs` 从 6 项增至 10 项，新增：

- partial receipt → 重启 → 补齐 READY → exact replay；
- 冲突 batch 全量回滚，plan 保持 PLANNED；
- nested receipt DDL UPDATE/DELETE 拒绝；
- 结构等价 v6 plan 无损迁移 v7 并继续查询。

本地验证：`nlos-task` 100 项 integration tests、crate Clippy 与 rustfmt 通过；全仓结果以本 canonical commit 最终验证记录为准。

## 4. 证据等级与限制

单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- TaskAuthority 已签发可被 ArtifactAuthority 在线验证的 publication authorization；当前 receipt 由调用方转交，TaskAuthority 只做强绑定消费。
- `READY` 已完成 Task finalize、关闭 permit或推进 TaskHead。
- TaskCommitReceipt 已列出 nested Artifact receipts；当前通过 `ArtifactCommitProgress` 查询。
- 已实现跨库 coordinator/outbox、crash 后自动收敛、compensation Receipt 或多 Artifact transaction-domain SERIALIZABLE。
- 已冻结 grouped membership 或覆盖 v7 VFS 故障矩阵/三平台 CI。

## 5. 下一步

增加 prepared finalize：在 TaskAuthority 内复验原 finalize proof、effect terminal/required satisfaction、当前 TaskHead 与 membership；只允许 `READY` plan 进入 finalize，并在同一个事务中写 TaskCommitReceipt、nested receipt link、关闭 permit、推进 TaskHead、把 plan 标为 `FINALIZED`。重放必须返回同一完整 receipt。
