# B-TASK-006F：cross-authority coordinator 写故障矩阵

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`[TASK-TXN-001]` partial/uncertain 可见性与可恢复收敛、`[TASK-CONFLICT-001]` typed failure
>
> 实现：`nlos-commit-coordinator` 真实双 authority integration tests

## 1. 本切片目标

证明 coordinator 不会在任一 authority 写失败时伪造“提交完成”，并且解除故障后能从两个数据库中已经提交的真实前缀继续收敛。测试通过 SQLite abort trigger 把故障精确注入 Artifact publish receipt、Task nested receipt 和 Task finalize terminal CAS 三个不同事务位置。

## 2. 故障矩阵

| 故障点 | 故障后 ArtifactAuthority | 故障后 TaskAuthority | 解除故障后的结果 |
|---|---|---|---|
| Artifact publication receipt insert | head 未推进、stage 保持 STAGED | plan 保持 PUBLISHING、TaskHead=0 | publish + record + finalize 收敛 |
| Task nested receipt insert | Artifact head 已推进且 publication receipt durable | plan 保持 PUBLISHING、nested 集为空、TaskHead=0 | Artifact exact replay → record → finalize 收敛 |
| Task plan terminal CAS | Artifact head/receipt durable | plan 保持 READY、permit ISSUED、TaskHead=0；本事务先前 Task receipt/permit/Attempt 写全部回滚 | finalize retry 原子完成，TaskHead=1 |

三类失败分别以 `CoordinatorError::Artifact` / `CoordinatorError::Task` 保留原 authority typed source；测试没有把跨库 partial 状态折叠成成功或笼统重试。

## 3. 验收结果

`nlos-commit-coordinator` integration tests 共 2 项：

- `every_cross_authority_prefix_converges_after_restart`：多重启前缀与 publish-before-record 窗口；
- `authority_write_failures_remain_partial_and_converge_after_repair`：上述三类写故障、事实检查与修复后收敛。

本地 crate tests、Clippy 和 rustfmt 通过；全仓结果以本 canonical commit 最终验证为准。

## 4. 证据等级与限制

单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- 已覆盖真实 kill-9、ENOSPC、I/O error、静默丢写/WAL torn tail 的组合矩阵；本切片是精确 transaction abort fault；
- coordinator 已由生产 supervisor 托管、周期调度或暴露运维状态；
- 无法自动收敛的 Artifact head conflict 已具备人工处置/compensation Receipt；
- 已完成三平台 CI 或实际掉电验证；
- online authorization token/签名与跨 authority term adoption 已实现。

## 5. 下一步与决策点

下一实现需要确定 coordinator 的长期所有者与启动顺序：由现有/未来 Process supervisor 托管，还是先建立独立本机 commit-recovery service。这个选择会决定生命周期、ServiceDirectory 身份、健康状态、重试退避和权限边界，属于架构归属而非纯实现细节，应在继续接入前取得项目负责人意见。
