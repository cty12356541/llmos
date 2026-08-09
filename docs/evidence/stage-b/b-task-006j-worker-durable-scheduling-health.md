# B-TASK-006J：worker durable scheduling 与 operations health

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`ADR-0004`、`B-TASK-006H`、`B-TASK-006I`、`[TASK-TXN-001]`

## 已实现事实

1. TaskAuthority-owned worker 改用 schema v8 due scan；未到期与 escalated plan 不进入自动 converge。
2. converge failure 以当前 `total_failures` 做 TaskAuthority CAS，持久产生下一 due 或 escalation；worker 重启不清零历史。
3. 单 plan 达阈值只持久升级该 plan，worker 继续服务健康 plan；只有 scan/ledger/clock 等基础设施连续失败才触发 worker `Faulted`。
4. `RecoveryWorkerHealth` 新增 durable retrying/escalated/resolved 汇总；retry wait 采用 TaskAuthority 给出的 deterministic-jitter due time。

## 验证与边界

6 项 coordinator integration tests 已更新并通过：立即启动恢复；持久退避后修复收敛；阈值升级后 plan 保持 `READY`、worker 保持运行且报告 `durable_escalated=1`；此前 prefix、写故障和逐 plan 隔离保持通过。全仓 test、Clippy `-D warnings` 与 rustfmt 通过。

本证据为单节点本地 H3 / `PARTIAL PASS`。health 仍是进程内快照，尚未接入统一 IPC/ServiceDirectory、metrics 或告警确认；外部接口前仍需错误分类与脱敏。真实 kill-9/VFS fault matrix 和三平台 CI 尚未完成。
