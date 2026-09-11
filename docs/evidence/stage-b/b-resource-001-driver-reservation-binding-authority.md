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

## （2026-09-11，W22-R）Reservation 多维 demand 谓词最小前缀

基线 HEAD：`f81e57fa`（`feat(runtime-tokio): add lifecycle meter openmetrics text prefix (W22-006)`）。本车道把 §边界 中登记的「未实现多维 ResourceDemand」推进一个最小前缀：demand 从单一数值扩为固定三维集合，binding（AVAILABLE→RESERVED）时逐维 fail-closed 校验；单维 credit 路径（`upper_bound`/consume high-water/finalize refund）逐字节保持不变。

### 维度清单

Quote 声明每维 capacity（`CreateQuoteRequest.demand_capacity`），Reservation 声明每维 demand（`ReserveRequest.demand`）；admission 按 `DemandDimension::ALL` 固定顺序比较，取首个违例维：

| 维度 | 语义 | capacity 列（quotes） | demand 列（reservations） |
|---|---|---|---|
| `cpu_shares` | 相对 CPU 份额 | `capacity_cpu_shares` | `demand_cpu_shares` |
| `memory_mib` | 内存 MiB | `capacity_memory_mib` | `demand_memory_mib` |
| `io_weight` | I/O 权重 | `capacity_io_weight` | `demand_io_weight` |

与总纲 `[BUD-ALG-001]`/`[RSM-TYPE-001]` 的 CPU/memory/I/O 分维一致（完整 tagged-variant ResourceDemand 与 unit registry 仍属后续车道，本前缀只做固定三维 capacity 谓词）。

### 写集

- `crates/nlos-resource/src/lib.rs`：`ResourceDemand`（零值 = 旧单维兼容档）+ `DemandDimension`（exhaustive `match` 取数）+ typed error `DemandExceedsCapacity { dimension, demand, capacity }`；`CreateQuoteRequest`/`QuoteRecord` 增 `demand_capacity`，`ReserveRequest`/`ReservationRecord` 增 `demand`（additive 字段，旧构造以 `Default` 补齐）；`reserve()` 绑定时逐维校验（任一维 `demand > capacity` 即拒绝，不扣 credit、不写行）；replay 一致性比较纳入 demand（同 key 不同 demand → `IdempotencyConflict`）；`quote_matches` 纳入 capacity。
- `crates/nlos-resource/src/schema.rs`：`migrate_v6`（schema v5→v6），同构 v3/v5 追加列风格：quotes/reservations 各 `ALTER TABLE ADD COLUMN … INTEGER NOT NULL DEFAULT 0 CHECK(>= 0)` ×3（物理列尾追加，INSERT/SELECT 按物理序）；已完整则幂等 bump `user_version=6`，部分存在则 `CorruptRecord("partial resource demand schema")` fail-closed。
- `crates/nlos-resource/tests/demand_admission.rs`（新增，5 测试）+ 既有 6 个测试文件的构造点默认值补齐 + `resource_authority.rs` 迁移链断言 v5→v6。

### 验证门（2026-09-11 实跑，独立 CARGO_TARGET_DIR，`--test-threads=1`）

- `cargo test -p nlos-resource`：**35 passed / 0 failed**（基线 30 + 新增 5；0 单元 + 1 activation + 3 cost_receipt + 4 cost_receipt_fault_injection + 5 demand_admission + 7 finalize_fault_injection + 6 finalize_refund + 9 resource_authority，doctest 0）。
- `cargo clippy -p nlos-resource --all-targets -- -D warnings`：exit 0，clean。
- `cargo fmt -p nlos-resource -- --check`：exit 0，clean。

新增测试覆盖：三维逐维超限各返回 `DemandExceedsCapacity` 且 credit 不动；多维 demand 全过绑定 + 精确回放 + 重启回读一致 + demand==capacity 边界可绑定；同 key 不同 demand 回放 `IdempotencyConflict` 且不改变 durable 行；v5 行剥离 v6 列后重开迁移幂等、旧行回读零 demand、旧零 demand 构造照常绑定；部分 v6 schema（缺一列）重开 `CorruptRecord` fail-closed。

### 边界

demand 仅在 binding（reserve）时作为 admission 谓词校验；consume/finalize 的计量与退款仍只走单维 credit（多维 metering/refund 是后续车道）。capacity 与 demand 均为 caller 声明值（无 host 容量证明）；零 demand 恒过零 capacity。迁移幂等与 torn-schema fail-closed 已测，真实掉电矩阵未在本前缀重跑。测试文件 352 纯 LOC 超出 250 指引，遵循本仓库按主题单文件 integration-test 惯例（同目录既有文件 314–1010 行）未拆分。
