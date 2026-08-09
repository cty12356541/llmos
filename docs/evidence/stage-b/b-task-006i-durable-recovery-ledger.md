# B-TASK-006I：durable recovery ledger 与 deterministic jitter

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`ADR-0004`、`[TASK-TXN-001]`、`[TASK-CONFLICT-001]`
>
> 实现：`nlos-task` schema v8 / `task_artifact_recovery`

## 1. 本切片目标

把 Artifact commit recovery 的失败次数、下次可重试时间和升级决定从 worker 内存移入 TaskAuthority durable truth。TaskAuthority 进程重启后不得把长期故障重新当作第一次失败，也不得让尚未到期或已升级的 plan 被普通 pending scan 反复热循环。

## 2. 已实现事实

1. schema v8 新增 per-plan `task_artifact_recovery` ledger，持久记录 `RETRYING / ESCALATED / RESOLVED`、连续/累计失败、最后 failure authority、首末失败时间、next due、escalated/resolved 时间。
2. `record_artifact_recovery_failure` 使用 `expected_total_failures` CAS；并发或陈旧记录者不能静默重复增加计数。
3. 重试延迟为 `base × 2^(n-1)`、由 max 封顶，再用 `SHA-256(domain || plan_id || failure_ordinal)` 施加确定性的 ±20% jitter；同一 plan/ordinal 跨进程得到同一调度结果，不引入随机状态。
4. `list_due_artifact_commit_plans` 只返回无 ledger 或 `RETRYING && next_retry_at <= now` 的非终态 plan；未到期和 `ESCALATED` plan 均被排除，且保持原创建时间/identity 稳定顺序。
5. 达到阈值的失败原子进入 `ESCALATED`，不再给出 next due；`resume_artifact_recovery` 必须以累计失败数 CAS 显式恢复，并保留总失败历史。
6. READY plan 的 terminal Task transaction 同时把既有 ledger 置 `RESOLVED`、清除 retry/escalation 字段；因此不存在 Task receipt 已提交但 ledger 仍声称待重试的崩溃窗口。
7. v5/v6/v7 均可事务升级到 v8；v7 升级不会为旧 plan 凭空生成 recovery history，未知版本仍 fail-closed。

## 3. 验收证据

新增两项 `artifact_commit_plan` integration tests：

- durable ledger 跨 authority reopen 保持逐位一致；due 前不扫描、due 时出现；陈旧 failure/resume CAS 拒绝；第三次失败升级；显式 resume 后可继续；最终 commit 与 ledger resolution 同事务完成。
- 构造 structural v7 数据库并升级至 v8，原 plan 可查且 recovery history 为 `None`。

同时更新 v1–v6 migration fixture 的当前 schema 断言，并修复 v4/v5/v6 回退 fixture 必须先移除 v8 子表的构造顺序。`nlos-task` 当前 107 项 integration tests 全部通过。

验证命令：

```text
cargo test -p nlos-task
cargo clippy -p nlos-task --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## 4. 边界与下一步

证据等级为单节点本地 H3 / `PARTIAL PASS`。本切片只建立 TaskAuthority 权威 API；B-TASK-006H worker 尚未改用 due scan 和 durable failure CAS，因此不能声称生产控制环已经跨重启保留退避。下一步把 worker 接到该 ledger，并把 `ESCALATED` 与 unresolved counts 投影为只读 operations health；真实 VFS/kill-9 故障和三平台 CI 仍未完成。
