# B-RESOURCE-003：Resource consume high-water

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：单节点、单一整数 credit 的 strict reference profile；为 ACTIVE Reservation 增加单调累计 usage receipt。不是完整 Resource finalize/退款或跨 authority ledger。

## 已实现事实

`ResourceAuthority::consume` 只接受 ACTIVE Reservation，并逐位校验 Operation、activation receipt 与当前 Driver fence。它以 `(reservation_id, sequence)` 幂等记录 immutable `ConsumptionReceipt`，拒绝 sequence=0、同序列不同内容、回退的 cumulative usage 以及超过 Reservation upper bound 的报告；同一报告重试返回原 receipt，不重复推进 high-water。

schema v3 为 Reservation 增加 durable `usage_high_water_seq`/`usage_high_water`，新增 immutable `reservation_consumption_receipts`，并提供事务化 v1/v2→v3 migration。`inspect_consumption_receipt` 会再次校验 receipt 与 Reservation high-water/activation binding 的一致性。

## 验证

- `consumption_records_strict_monotonic_high_water_and_replays` 覆盖 inactive rejection、sequence/content conflict、monotonicity、strict upper bound、replay、重启回读及 DDL immutable trigger。
- `cargo test -p nlos-resource --quiet`
- `cargo clippy -p nlos-resource --all-targets --all-features -- -D warnings`

## 明确缺口

尚未实现 CLOSING/UNCERTAIN/QUARANTINED、effect-closed final usage receipt、双重记账 finalize/refund/risk account、late consume/rebate、BOUNDED_RISK/OBSERVED/UNKNOWN_USAGE、多维 ResourceDemand、跨 authority prepare→activate 或 TaskCommitReceipt resource/cost receipt；完整 Resource Manager 仍为 `PARTIAL_PASS`。
