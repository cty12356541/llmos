# B-TASK-006H：TaskAuthority-owned commit recovery worker

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`ADR-0004`、`[TASK-COMMIT-001]`、`[TASK-COMMIT-002]`、`[TASK-TXN-001]`、`[TASK-CONFLICT-001]`
>
> 实现：`nlos-commit-coordinator::TaskAuthorityCommitRecoveryWorker`

## 1. 本切片目标

把 B-TASK-006E–006G 的手动 pending scan 接入由 TaskAuthority service 拥有的真实生命周期：worker 启动后立即恢复未完成 plan，之后周期扫描；故障时有界指数退避并保留 plan/authority 来源；达到阈值后进入可监督的 terminal `Faulted`，显式停止可及时 join。

## 2. 已实现事实

1. `TaskAuthorityCommitRecoveryWorker::start` 接收共享的 `SqliteTaskAuthority` 与 `ArtifactStore`，在专用线程立即执行首轮 bounded best-effort scan，不等待第一个 poll interval。
2. worker 不打开第三个数据库，不持久化派生进度；canonical truth 仍只在 TaskAuthority 与 ArtifactAuthority，崩溃后新 worker 从 durable prefix 重放。
3. 成功/空扫描按 `poll_interval` 周期运行；包含 scan failure 或逐 plan failure 的 cycle 使用 `poll_interval × 2^(n-1)` 指数退避，并由 `max_backoff` 封顶。
4. `RecoveryWorkerHealth` 提供 `Starting / Running / BackingOff / Faulted / Stopped`、累计 cycle/inspect/finalize、连续失败数、当前 retry delay 和最后失败集。
5. `RecoveryWorkerFailure` 保留可选 `ArtifactCommitPlanId` 与 `Task / Artifact / Coordinator / Worker` typed source；同批健康 plan 即使另一个 plan 失败，其 inspected/finalized 统计仍保留。
6. 连续失败达到 `failure_threshold` 后 worker 进入 terminal `Faulted`，不删除或改写 pending plan；TaskAuthority service 可据此升级并创建新 worker。
7. `stop()` 使用有界非阻塞信号唤醒等待并 join；重复停止与 `Drop` 均安全，不必等待长 poll interval。
8. 配置对零 scan limit、零 poll interval、倒置 backoff 上限和零 failure threshold fail-fast。

## 3. 验收测试

`nlos-commit-coordinator` integration tests 从 3 项增至 6 项：

- `task_authority_worker_scans_immediately_and_stops_promptly`：把 poll interval 设为 10 秒，验证 worker 仍立即把启动前 pending plan 推进到 `FINALIZED`，随后停止在 1 秒内 join 并报告 `Stopped`。
- `task_authority_worker_backs_off_reports_source_and_recovers`：对一个 plan 的 terminal CAS 注入持续 TaskAuthority abort，验证至少两个失败 cycle、`BackingOff`、非空 retry delay、准确 plan ID/Task source；解除故障后自动完成并清零失败健康状态。
- `persistent_failure_faults_worker_without_losing_durable_plan`：阈值设为 2，验证精确两个失败 cycle 后进入 `Faulted`，plan 仍为 durable `READY`，没有伪 terminal fact。
- 此前 restart prefix、三 authority 写故障点和 best-effort 双 plan 隔离测试保持通过。

验证命令：

```text
cargo test -p nlos-commit-coordinator
cargo clippy -p nlos-commit-coordinator --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## 4. 证据边界与下一步

> 后续更新：B-TASK-006J 已用 per-plan durable escalation 取代本切片的纯进程内“持续 plan failure → worker Faulted”策略；`Faulted` 现仅用于控制环基础设施连续失败。启动/停止/health 基础仍有效。

本证据是单节点本地 H3 / `PARTIAL PASS`，不声称独立服务、分布式事务或 production 运维能力。尚未实现：

- jitter 与持久 retry/escalation ledger；进程崩溃会从 durable plan 恢复，但连续失败计数从零开始；
- ServiceDirectory/IPC 运维 health、metrics、告警确认与错误文本脱敏；
- 真实 kill-9、ENOSPC、I/O/torn-write 的 worker 组合矩阵与三平台 CI；
- 多 worker 单活 lease/fence（ADR-0004 当前边界是一个 TaskAuthority service owner）。

下一独立验收门应先把失败升级决定持久化到 TaskAuthority，而不是依赖进程内计数；随后再把只读健康摘要接入统一运维接口。
