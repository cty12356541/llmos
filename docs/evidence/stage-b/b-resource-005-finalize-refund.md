# B-RESOURCE-005：Reservation finalize/refund 双重记账结算

- 状态：`PARTIAL_PASS`
- 日期：2026-08-17
- 范围：单节点、单一整数 credit 的 strict reference profile；为 ACTIVE Reservation 提供 effect-closed 证明下的双重记账 finalize/refund 结算。不是完整 reconciliation、UNKNOWN_USAGE/BOUNDED_RISK ledger、late rebate、endpoint 签名证明或跨 authority settlement。

## 1. 结论

`ResourceAuthority::finalize_reservation` 只接受已激活且仍受当前 Driver fence 保护的 Reservation，逐位校验 Operation、activation receipt 与当前 usage high-water，并要求 caller 提供 `effect_closed_proof_digest`（本 reference profile 中为 opaque caller-asserted digest，真实 enforcement-gateway 签名属未来 reconciliation authority）。它在**同一个事务**内完成：(a) 写入不可变 `FinalizationReceipt`（含 `refund_credit = upper_bound - final_usage`）；(b) 以 overlay 方式把 Reservation 置为 `FINALIZED`（`state` 保持 1，v5 新增 `finalize_receipt_id`/`finalized_at_ms` 列）；(c) 把 refund 记回账户 `available_credit`（双重记账：reserve 时的 hold 与 refund 原子释放）。结算后迟到的 `consume`/`quarantine` 一律拒绝；精确重放返回原 Receipt，异 bytes fail closed；FINAlIZED overlay 绑定列不可变；重启后逐位回读同一 Receipt。

## 2. 已实现事实

- schema v5 为既有 Reservation 增加 `finalize_receipt_id`/`finalized_at_ms` 可空列（overlay，不触碰 v1 `state` CHECK），新增 immutable `reservation_finalize_receipts` 表、`reservations_finalize_receipt_unique` 唯一索引、binding insert/update 触发器与 overlay-binding immutable 触发器（`OLD.finalize_receipt_id IS NOT NULL` 时禁止改写/清空），以及 v1/v2/v3/v4→v5 迁移（幂等重放 + partial schema fail-closed，沿用 v4 模式）。
- `ReservationState::Finalized`：解码优先级为 quarantine overlay → finalize overlay → Active；两者同时出现判 `CorruptRecord`。
- `finalize_reservation` 校验链：未知 Reservation → `ReservationNotFound`；非 ACTIVE → `ReservationNotActive`/`ReservationQuarantined`/`ReservationFinalized`；operation/activation 绑定不符 → `ReservationBindingMismatch`；时间戳早于 activation → `InvalidFinalizeTimestamp`；Driver fence 过期 → `StaleDriver`；`final_seq < high_water_seq` → `FinalizeSequenceConflict`；`final_usage < high_water` → `UsageNotMonotonic`；`final_usage > upper_bound` → `UsageExceedsUpperBound`。
- 确定性 receipt id 由 `(reservation_id, activation_receipt_id, proof_digest, final_seq, final_usage)` 派生；精确重放逐字节返回原 Receipt，异 proof/usage fail closed（`IdempotencyConflict`）。
- 双重记账：`available_credit` 在 finalize 前始终保留完整 `upper_bound` 的 hold；finalize 后 `available_credit += upper_bound - final_usage` 与 receipt 同事务提交；no-effect 结算（final_usage=0）全额退还 hold。
- 新增只读 `inspect_reservation`（durable Reservation 全字段含 terminal overlay）与 `inspect_finalize_receipt`（owner 回读 + 与 Reservation 逐位核对）。
- 既有 reserve/activate/consume/quarantine、Driver rotation、endpoint proof 与 v1–v4 migration 语义保持（consume/quarantine/activate 对 `Finalized` 显式 `ReservationFinalized` fail-closed）。

## 3. Evidence

- `cargo test -p nlos-resource --test finalize_refund`：5 项通过——`finalize_settles_double_entry_refund_and_is_immutable`（consume 40/100 → refund 60、available 900→960、FINALIZED overlay、exact replay、异 bytes IdempotencyConflict、迟到 consume/quarantine 拒绝、重启回读、receipt 与 overlay 绑定不可变）；`finalize_no_effect_refunds_full_upper_bound`（refund=100、available 回到 1000）；`finalize_fails_closed_on_invalid_inputs`（upper-bound/monotonic/sequence/timestamp/binding/unknown 全类型化拒绝）；`finalize_rejects_reserved_and_quarantined_reservations`（RESERVED→NotActive、QUARANTINED 保持冻结）；`finalize_v5_migration_reapplies_idempotently_and_partial_schema_fails_closed`（v4 形状 DB 重开重放 v5、legacy ACTIVE 行不伪造 overlay 且可正常结算、partial schema 拒绝）。
- `cargo test -p nlos-resource --quiet`：13 项全过（含既有 resource_authority 8 项回归）。
- `cargo clippy -p nlos-resource --all-targets --all-features -- -D warnings`：通过。
- 三平台 CI 待运行（本地 macOS/arm64 证据）。

## 4. 明确限制

- proof digest 为 caller-asserted opaque 摘要；endpoint/enforcement-gateway 签名、UNKNOWN_USAGE/BOUNDED_RISK/OBSERVED risk ledger、late rebate、自动 reconciliation、QUARANTINED→FINALIZED 的证明后解冻、跨 authority resource/cost receipt 与 TaskCommitReceipt resource consumption 接线仍未实现。
- `finalize_reservation` 要求当前 Driver fence（与 consume/quarantine 一致）；Driver 轮换后的结算解冻路径未提供。
- 单机 strict reference profile：非多维资源、无真实 Driver enforcement、无三平台 CI/真实 ENOSPC 证据。
