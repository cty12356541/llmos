# B-RESOURCE-006：Owner 侧 Resource cost-receipt 聚合回读

- 状态：`PARTIAL_PASS`
- 日期：2026-08-24（增量验收，Attempt `RESOURCE-COST-EVIDENCE-01`）
- 范围：单节点、单一整数 credit 的 strict reference profile；为**已 FINALIZED** 的 Reservation 提供 owner-derived 只读聚合 `ResourceCostReceipt`（activation + 全部有序 consumption + finalization receipts）。这是 Resource owner 侧的局部安全门，不是 TaskAuthority 消费接线、跨 authority 事务、endpoint 签名效应证明或统一 TaskWriteSet。

## 1. 结论

`ResourceAuthority::inspect_cost_receipt` 是对已结算 Reservation 的纯读聚合：FINALIZED 门（缺失 → `ReservationNotFound`；任何非 FINALIZED 状态 → `ReservationNotActive` typed fail-closed）之后，从既有 immutable 表加载 activation receipt、按 `sequence` 排序的全部 consumption receipts 与 finalization receipt，并**逐位**与同一 durable Reservation 交叉核对（activation/finalization 绑定七项等式），且要求有序 consumptions 恰好闭合 owner high-water（末条 `(sequence, cumulative_usage)` == `(usage_high_water_seq, usage_high_water)`，空集对应 `(0,0)`；每条 consumption 还须与 reservation 的 `operation_id` 和 activation `receipt_id` 绑定一致）。任何不一致一律 `CorruptRecord` fail-closed。聚合不接受任何 caller 提供的成本事实（owner-derived read model），重启后重放逐字节相等。守恒链 `upper_bound − final_usage = refund_credit`（测试中 100 − 37 = 63）由聚合内 immutable finalization receipt 字段承载（账户行的双重记账移动属 B-RESOURCE-005 同事务语义，聚合本身不重读账户）。

## 2. 已实现事实

- 新增 `ResourceCostReceipt` 值类型：`reservation_id`/`account_id`/`quote_id`/`call_id`/`operation_id`/`upper_bound` + `activation: ActivationReceipt` + `consumptions: Vec<ConsumptionReceipt>` + `finalization: FinalizationReceipt`，全部取自 durable 表，按值返回（immutable-by-construction）。
- 新增 `inspect_cost_receipt(reservation_id)` 只读方法与私有查询 `consumption_receipts`（`SELECT ... FROM reservation_consumption_receipts WHERE reservation_id=? ORDER BY sequence`）。
- **FINALIZED 门**：状态解码以 overlay 为准（quarantine → finalize → Active），故直接 finalize 与 QUARANTINED→FINALIZED reconciliation 解冻后的 Reservation 均可聚合；本 attempt 测试仅覆盖直接 finalize 路径。
- **activation/finalization 绑定检查**（任一不符 → `CorruptRecord("Resource cost receipt disagrees with finalized Reservation")`）：`reservation.activation_receipt_id == activation.receipt_id`、`reservation.finalize_receipt_id == finalization.receipt_id`、`activation.operation_id == reservation.operation_id`、`finalization.activation_receipt_id == activation.receipt_id`、`finalization.operation_id == reservation.operation_id`、`finalization.high_water_seq == reservation.usage_high_water_seq`、`finalization.high_water == reservation.usage_high_water`。
- **有序 consumptions 闭合 high-water**（不符 → `CorruptRecord("Resource consumption receipts do not close the high-water")`）：末条 consumption 的 `(sequence, cumulative_usage)` 必须等于 Reservation 当前 `(usage_high_water_seq, usage_high_water)`；每条 consumption 的 `operation_id`/`activation_receipt_id` 绑定不一致 → `CorruptRecord("Resource consumption receipt binding disagrees with activation")`；activation 或 finalization receipt 行缺失 → `CorruptRecord`。
- **无 schema 变更、无新写路径**：复用 v3/v5 既有 immutable receipt 表组与 v1–v5 迁移；diff 仅新增聚合结构体、只读方法与一个私有 SELECT helper（+125 行），既有 reserve/activate/consume/quarantine/finalize 语义不变。

## 3. Evidence

- `cargo test -p nlos-resource --test cost_receipt`：2 项通过——
  - `cost_receipt_is_owner_derived_and_replays_after_restart`：完整生命周期（driver/account 1000/quote upper_bound 100/reserve/activate/consume sequence 1 cumulative 37/finalize final_seq 2 final_usage 37 → refund 63）；聚合全字段 owner 断言（reservation/account/quote/call/operation/upper_bound、activation 逐位相等、consumptions 长度 1 且 sequence 1 usage 37、finalization 逐位相等、`ReservationState::Finalized`）；关闭并重开 authority 后 `inspect_cost_receipt` 返回**逐字节相等**的聚合（"owner aggregate must replay exactly after restart"），refund 63 保持。
  - `cost_receipt_requires_terminal_owner_state`：RESERVED（非 FINALIZED）Reservation → `Err(ReservationNotActive)` fail-closed。
- `cargo test -p nlos-resource --quiet`：**25 项全过、0 失败**（unit 0；`activation_consume_finalize_restart` 1；`cost_receipt` 2；`finalize_fault_injection` 7；`finalize_refund` 6；`resource_authority` 9；doc 0）。
- `cargo clippy -p nlos-resource --all-targets --all-features -- -D warnings`：通过（0 warning）。
- `cargo fmt --all -- --check`：通过。
- 本地 macOS/arm64；基线 HEAD `6b7285e`；候选为工作区未提交变更（`crates/nlos-resource/src/lib.rs` +125 行、新增 `crates/nlos-resource/tests/cost_receipt.rs`）。本 attempt 只写本 Evidence 文件，不提交、不更新进度表；提交与 `stage-b-progress.md` 更新由后续单一 integrator 负责。本 attempt 无 CI 结果。

## 4. 明确限制

- **Owner-side only**：`ResourceCostReceipt` 尚未被 TaskAuthority/`TaskCommitReceipt` 消费；本证据不声称跨 authority resource/cost receipt 事务、跨 authority 原子提交或统一 TaskWriteSet（complete TaskWriteSet 仍未完成）。
- effect-closed proof digest 仍为 caller-asserted opaque 摘要（沿 B-RESOURCE-005 限制）；无 endpoint/enforcement-gateway 签名的效应证明。
- 聚合无专属故障注入矩阵：`inspect_cost_receipt` 为纯读路径，F1–F6 只覆盖 finalize 表组写入；未验证真实断电（power-loss）下的聚合读，kill-9/掉电模拟不属本 attempt。
- 聚合不重读账户行：`upper_bound − final_usage = refund_credit` 守恒由 finalize 同事务双重记账（B-RESOURCE-005）与聚合内 receipt 字段承载，`available_credit` 数值不在聚合输出中复核。
- 非 FINALIZED 一律复用 `ReservationNotActive` typed 拒绝（无专用 NotFinalized 错误变体）；QUARANTINED→FINALIZED reconciliation 后的聚合路径无专属测试。
- 单机 strict reference profile：非多维资源、无真实 Driver enforcement；无本 attempt CI 结果，不得据此外推 DONE 或 H4+。
