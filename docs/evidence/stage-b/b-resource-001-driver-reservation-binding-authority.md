# B-RESOURCE-001：Driver / Reservation binding authority

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`[BUD-RES-001]`、`[BUD-RES-002]`、`[BUD-BIND-001]`、`[DRV-ACTIVE-001]`、[ADR-0005](../../management/adrs/0005-task-write-set-authority-first.md)

## 已实现事实

1. 新增 `nlos-resource` WAL/FULL SQLite reference authority；DriverId、DeviceId、QuoteId、ReservationId、driver fencing token 与一次性 activation token 均由 authority 从 typed idempotency input 派生，不接受调用者自报这些权威身份。
2. Driver registration/rotation 保存 immutable generation history；rotation 以 generation/token CAS 推进，旧 quote 或 RESERVED binding 的 readback 随即 fail closed。
3. reference account 的 `AVAILABLE → RESERVATION` 在一个 `BEGIN IMMEDIATE` 事务内完成：余额不足不写 Reservation、不改变余额；exact replay 不重复扣减。
4. 每个 Reservation 唯一绑定 CallId、OperationId、QuoteId、DeviceId、DriverId 及 driver generation/token；数据库唯一约束阻止 Call/Operation 跨 Reservation 复用。
5. `inspect_permit_binding` 只返回仍为 RESERVED 且 Driver fence 当前的记录；`activate` 逐位核对 binding 后一次性执行 `RESERVED → ACTIVE` 并写 immutable activation Receipt，重复激活返回原 Receipt。

## 验证

`cargo test -p nlos-resource` 的 5 项 integration tests 覆盖：reserve/replay/restart 与余额守恒、余额不足/重绑定拒绝、一次性 activation、Driver rotation 围栏，以及 Quote/Reservation/Receipt 的 DDL 防改写。完整 workspace 的 fmt、check、test 与 clippy 同样通过。

## 边界

这是单节点、单一整数 credit 维度的 pre-dispatch reference slice，不是完整 Resource Manager/Ledger。bootstrap `initial_credit` 仅用于本地 profile，尚无 Mint/双重记账来源证明；未实现多维 ResourceDemand、risk reservation、consume/high-water、closing/finalize/refund、UNKNOWN/QUARANTINED、AdmissionPlan、多 participant prepare、ControllerBinding、真实 enforcement shim、provider credential、签名 quote 或三平台 Device adapter。OperationId/CallId 是外部预分配引用，尚未与 OperationAuthority/TaskAuthority 做原子跨 authority 注册；完整 EffectPermit 在线验证仍未接通。
