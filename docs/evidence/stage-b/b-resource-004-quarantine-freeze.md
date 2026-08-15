# B-RESOURCE-004：Resource quarantine freeze

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：单节点、单一整数 credit 的 strict reference profile；补齐缺少 effect-closed final usage 证明时的保守冻结边界。不是完整 Resource finalize/退款、reconciliation 或跨 authority settlement。

## 已实现事实

`ResourceAuthority::quarantine` 只接受已激活且仍受当前 Driver fence 保护的 Reservation，并逐位校验 Operation、activation receipt 与当前 usage high-water。它在同一事务内写入不可变 `QuarantineReceipt`，记录触发原因摘要和冻结时的 `(high_water_seq, high_water)`，再以 CAS 将 Reservation 置为 `QUARANTINED` overlay。

QUARANTINED 不移动账户余额、不生成 final settlement，也不把 caller 的 reason digest 当成 effect-closed proof。冻结后迟到的 `consume` 一律拒绝；activation/quarantine receipt 仍可由 owner 回读用于后续 reconciliation。相同 reservation/activation/reason 重试返回原 Receipt，改变绑定或 reason fail closed；重启后回读同一 Receipt。

schema v4 为既有 Reservation 增加 quarantine receipt 绑定列，新增 immutable quarantine receipt 表、绑定触发器和 v1/v2/v3→v4 迁移。采用 overlay 而不是重写 v1 的 state CHECK，避免改写既有 Reservation 历史。

## 验证

- `quarantine_freezes_high_water_rejects_late_consume_and_replays` 覆盖冻结 high-water、迟到 consume 拒绝、activation proof 保留、冲突/重放、重启回读及 DDL immutable trigger。
- 既有 Resource reserve/activate/consume、Driver rotation、endpoint proof 与 v1 migration 测试仍通过。
- `cargo test -p nlos-resource --all-targets --quiet`
- `cargo clippy -p nlos-resource --all-targets --all-features -- -D warnings`

## 明确缺口

仍未实现 endpoint/enforcement gateway 签名的 `effect_closed + final_usage + final_seq`、CLOSING/FINALIZED 状态、双重记账 finalize/refund/risk account、UNKNOWN_USAGE/BOUNDED_RISK/OBSERVED、late rebate 和自动 reconciliation。QUARANTINED 只能冻结并保留证据，不能被描述为结算完成或 TaskCommitReceipt resource/cost receipt。
