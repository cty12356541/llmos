# B-TASK-008C2G-RES：Resource Reservation permit 前 owner 复核

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：在 `CommitPermit` 发放前重新读取已封存 `TaskWriteSet` 中的
  `ResourceAuthority` Reservation；不等同于 Reservation activation/consume、
  Resource publication receipt 或完整 `TaskCommitReceipt` 接线。

## 结论

`SqliteTaskAuthority::request_commit_permit_with_resource_authority` 为已有
sealed write set 提供了一个显式的 Resource owner 复核边界。对每个声明的
Reservation，TaskAuthority 通过 `inspect_permit_binding` 要求 owner 仍返回
`RESERVED` 状态以及相同的 account、quote、call、operation、Driver/device、
generation/fencing token 和 upper-bound；缺失、已激活、Driver 已轮换或任一
字段漂移都会在 participant registry freeze 前 fail closed。

普通 `request_commit_permit` 的历史兼容行为保持不变；该切片不把 caller 的
Reservation bytes 晋升为新事实，也不消费 activation token 或伪造 Resource
publication/finalization receipt。相同 permit 请求的 replay 仍返回原 durable
decision，不重复 owner readback。

## 验证

- `verified_write_set_seal_binds_reserved_resource_owner_facts` 先用错误
  `ResourceAuthority` 验证 owner readback 失败且不发放 permit，再用正确 authority
  验证发放与 replay。
- `cargo test -p nlos-task --test participant_registry verified_write_set_seal_binds_reserved_resource_owner_facts --quiet`
- `cargo test -p nlos-task --quiet`

## 明确缺口

Resource activation/consume/finalize、Resource publication receipt、跨 authority
prepare→activate/complete，以及 `TaskCommitReceipt` 对 Resource publication 的
嵌套仍未实现。Semantic publication receipt producer/consumer 仍是主线下一门，
需要单独确定其 authority ownership；本切片不替代该决策。
