# B-TASK-006C：Artifact publication 授权与 membership freeze

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`[TASK-COMMIT-001]` / `[TASK-COMMIT-002]` 发布前围栏子集、`[TASK-GROUP-002]` membership 漂移围栏、`[TASK-TXN-001]` recoverable prepare 子集
>
> 实现：`crates/nlos-task` schema v7，承接 B-TASK-006A/006B

## 1. 本切片目标

关闭“plan 已持久化但 Artifact publish 前 Task 事实已经漂移”的窗口：只有 TaskAuthority 在发布前重新验证 holder、TaskHead、permit、group binding 与 effect 集之后，plan 才能从 `PLANNED` 进入可重放的 `PUBLISHING`。授权后到 Task finalize 前，相关 TaskGroup membership 必须冻结，避免 Artifact canonical head 已推进后因 membership 漂移而无法完成 Task receipt。

本切片仍不推进 TaskHead、不关闭 CommitPermit，也不声明两个 SQLite authority 已具备原子事务。

## 2. 已实现事实

1. **显式授权边界**：`authorize_artifact_publication` 在 `BEGIN IMMEDIATE` 中重新加载 plan/task/attempt/permit，逐位验证 attempt generation、permit holder/state/write-set root、TaskHead/effect-history/retry fence 与当前 group binding。
2. **artifact-only fail-closed**：当前协议只接受没有任何 `effect_slots` 的 permit；混合 Artifact + Effect 提交在具备统一 finalize proof 前拒绝授权。
3. **durable authorization**：成功授权以 CAS 将 `PLANNED → PUBLISHING`，即使尚无 publication receipt，重启后也能准确查询；重复授权返回原记录且不改写授权时间。
4. **回执必须后置**：`record_artifact_publications` 不再接受 `PLANNED` plan，调用方无法绕过 TaskAuthority 发布前围栏直接注入 nested receipt。
5. **membership freeze**：group-bound plan 处于 `PUBLISHING` 或 `READY` 时，Admission 与 Removal 均返回 typed `GroupPublicationInFlight`；检查和成员变更位于同一 TaskAuthority writer transaction。
6. **未冒充 finalized**：授权及 receipt 消费期间 permit 保持 `ISSUED`、TaskHead 保持不变；`FINALIZED` 仍由下一切片产生。

## 3. 验收测试

新增或收紧的验收覆盖：

- 未授权 receipt 被拒绝；授权后允许消费；
- `PUBLISHING + 0 receipts` 可跨重启查询，授权 exact replay 保留原时间戳；
- 带 Effect slot 的 permit 拒绝 Artifact publication authorization；
- 授权后的 grouped plan 同时阻止成员 Removal 和新 Attempt Admission；
- 原有 partial → restart → READY、batch rollback、DDL immutable 与迁移覆盖保持通过。

本地验证：`nlos-task` 103 项 integration tests 通过；全仓测试、Clippy 与 rustfmt 结果以本 canonical commit 的最终验证为准。

## 4. 证据等级与限制

单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- `READY` 已写入完整 TaskCommitReceipt、关闭 permit 或推进 TaskHead；
- ArtifactAuthority 会在线验证 TaskAuthority authorization token/signature；当前授权是 TaskAuthority durable fence，由后续 coordinator 按顺序调用；
- 已实现跨库 coordinator/outbox、崩溃后自动收敛或 compensation；
- 已覆盖 schema v7 新路径的 VFS fault matrix 或三平台 CI；
- 混合 Artifact + Effect write set 已获支持。

## 5. 下一步

实现 prepared finalize：只允许完整 `READY` plan 在 TaskAuthority 单事务中写 TaskCommitReceipt 及 nested Artifact receipt link、关闭 permit、推进 TaskHead并将 plan 标为 `FINALIZED`；重启重放必须返回同一完整 receipt，并解除 membership freeze。
