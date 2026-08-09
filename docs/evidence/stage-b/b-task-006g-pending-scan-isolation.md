# B-TASK-006G：pending scan 逐 plan 隔离与健康报告

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`[TASK-TXN-001]` recoverable coordinator、`[TASK-CONFLICT-001]` typed conflict visibility
>
> 实现：`nlos-commit-coordinator::converge_pending_best_effort`

## 1. 本切片目标

消除启动恢复队列的 head-of-line blocking：一个长期冲突或暂时故障的 Artifact plan 不得阻止同批其他 Task 提交完成；运行方必须获得逐 plan、保留 authority 错误来源的报告，而不是只有整批成功/失败。

## 2. 已实现事实

1. `PendingConvergenceReport` 记录扫描快照的 `inspected`、已完成的完整 receipts 和逐 plan failures。
2. `PendingConvergenceFailure` 绑定稳定 `ArtifactCommitPlanId` 与原 `CoordinatorError::Task/Artifact`，不丢失错误 authority。
3. `converge_pending_best_effort` 只在 pending-list 查询本身失败时整体返回 `Err`；单 plan 失败写入报告并继续处理后续 plan。
4. 扫描仍是 bounded stable snapshot；本轮新出现的 plan 留到下一轮，已 `FINALIZED` plan 自动退出后续扫描。
5. 故障修复后再次扫描只处理仍未完成的 plan，不重放已经 finalized 的健康 plan。

## 3. 验收测试

在同一 TaskAuthority/ArtifactAuthority 中建立两个独立 Task plan，只对其中一个 plan 的 terminal CAS 注入条件 trigger：

- 首轮 best-effort scan：`inspected=2`、一个完整 finalized receipt、一个绑定准确 plan ID 的 typed Task failure；
- 故障 plan 保持 `READY`，健康 plan 为 `FINALIZED`；
- 删除 trigger 后第二轮：`inspected=1`、该 plan 完成、failures 为空。

`nlos-commit-coordinator` integration tests 由 2 项增至 3 项；crate tests、Clippy 与 rustfmt 通过。

## 4. 限制与下一步

证据等级仍为单节点局部 H3 / `PARTIAL PASS`。本切片不决定 worker 所在进程，也不实现周期调度、指数退避/jitter、最大尝试/升级策略、运维 API 或 metrics。下一步仍需项目负责人确定 coordinator 长期归属，再把本报告映射为该服务的健康状态与告警语义。
