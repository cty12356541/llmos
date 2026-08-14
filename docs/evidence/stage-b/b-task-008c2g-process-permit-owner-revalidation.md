# B-TASK-008C2G-PROCESS：Process binding permit 前 owner 复核

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：在 `CommitPermit` 发放前重新读取已封存 `TaskWriteSet` 中的
  Process/AgentInstance/IsolationDomain binding 与 endpoint proof；不等同于
  Process spawn/rotation、跨 authority prepare→activate 或完整 TaskWriteSet。

## 结论

`SqliteTaskAuthority::request_commit_permit_with_process_authority` 为已有
sealed write set 提供显式 Process owner 复核边界。TaskAuthority 通过
`ProcessAuthority::verify_active_process_binding` 要求完整 Process、AgentInstance、
IsolationDomain generation/fencing token 仍一致，并重新读取 endpoint proof，
同时确认 owner 仍绑定同一 TaskAttempt；任何 stale binding、domain fence 漂移、
错误 attempt 或 proof 漂移都会在 participant registry freeze 前 fail closed。

该 API 不修改 ProcessAuthority 状态；普通 `request_commit_permit` 的历史兼容
行为保持不变；相同 permit 请求 replay 返回原 durable decision，不重复 owner
readback。

## 验证

- `verified_write_set_seal_binds_receipted_snapshot_and_artifact_reads` 先用错误
  `ProcessAuthority` 验证 owner readback 失败且不发放 permit，再用正确 authority
  验证发放与 replay。
- `cargo test -p nlos-task --test participant_registry verified_write_set_seal_binds_receipted_snapshot_and_artifact_reads --quiet`
- `cargo test -p nlos-task --quiet`

## 明确缺口

Process spawn/rotation、TaskAuthority 与 ProcessAuthority 的跨 authority 原子
prepare→activate、Operation/Channel linkage、Semantic publication receipt 及完整
TaskWriteSet 仍未实现；该切片不替代 Semantic publication 的 authority ownership
决策。
