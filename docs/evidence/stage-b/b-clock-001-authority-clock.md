# B-CLOCK-001：AuthorityClock 本地单调时钟权威（最小前缀）

- 状态：`PARTIAL_PASS`（单机 SQLite 重启级 `H3`）
- 日期：2026-08-29
- Owner：`AuthorityClock`（`crates/nlos-clock`，SQLite schema v1）
- 设计依据：[ADR-0011](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md) 决定 3（AuthorityClock 纳入签名贯穿同批范围：本地单调时钟权威，为 validity/anti-replay 提供时间语义，取代「上层传入时间」占位；「crate 归属在实现切片定」由本 evidence 登记为 `crates/nlos-clock`）；先例：[ADR-0008](../../management/adrs/0008-durable-wait-registry-authority.md)（schema vN、WAL/FULL 硬校验、replay 回执模式、kill-window 矩阵样式）
- 关联工作包：`B-TASK-006L`（AuthorityClock 归属定案切片）
- base HEAD：`4f90511`

## 1. 实现事实

- **归属与分层**：独立 authority crate，自有 `clock-authority.db`（WAL+FULL 硬校验、STRICT、trigger 守卫、单写者 Mutex）；依赖仅 `rusqlite`（workspace 同版 0.40）与 `nlos-types`（`IdempotencyKey`）。权威语义是**纯逻辑计数器**：不读系统时钟、不依赖任何上层传入时间——被取代的占位语义不进入实现。
- **schema v1 设计定案（任务书二选一）**：**single-row watermark**，非 append-only tick 行。论证：事务原子性保证崩溃后水位只有旧值/新值两个合法终点（无中间态）；单调性内嵌于行本身——`STRICT CHECK(reading>=0)`、no-insert trigger（防第二行与重播子）、no-delete、singleton 冻结、`BEFORE UPDATE` 拒绝任何 `NEW.reading < OLD.reading`（连 raw SQL 都无法回退）。append-only 方案的「不减」只能靠 max 扫描 + 前驱守卫在读取路径派生，且 durable 面无界增长——最小前缀不取。
- **`now(NowRequest{ idempotency_key })`**：单 `Immediate` 事务内完成 replay 检查 → 读水位 → CAS 写（`UPDATE ... WHERE reading=<观察值>`，`changed != 1` → `CorruptRecord`）→ tick 回执插入（AFTER INSERT trigger 校验回执读数 ≤ 水位，回执不可改不可删）→ commit。首次调用初始化读数 1（迁移种子行 `reading=0` 表示「尚未签发」）；每个 distinct key 恰好 +1；同 key 重放返回 durable 原读数且水位不动——**同值重放不倒退、不双跳**。读数空间耗尽（`i64::MAX`）fail-closed（`CorruptRecord`，零状态变更）。
- **`inspect()`**：零副作用读取 durable 高水位，作为重启侧验证入口：任何重启路径后 `now()` ≥ 该值。
- **typed error**：`AuthorityClockError`（`Sqlite`/`Io`/`DurabilityUnavailable`/`SchemaVersionUnsupported`/`CorruptRecord`/`LockPoisoned`）全 fail-closed；`open()` 硬校验 WAL+FULL，未知 schema 版本拒绝打开。
- **显式不做**：不接 wall-clock 校准、无跨进程 IPC、无 validity/anti-replay 签发 API（均为后续切片）；未持久化 epoch/offset 事实（本前缀不需要；若后续需要，按 ADR-0011 在所属切片与 Evidence 登记，不私加 authority）。

## 2. 验证

```text
cargo test -p nlos-clock（base 4f90511 工作区，未提交写集）
  → clock_authority 4 passed；clock_fault_injection 7 passed（6 场景 + kill-9 helper）
  → 合计 11 passed / 0 failed

cargo clippy -p nlos-clock --all-targets -- -D warnings → 0 warning / 0 error
cargo fmt -p nlos-clock → 通过（-- --check 干净）
```

kill-window 故障矩阵（镜像 nlos-wait 13 项矩阵样式与 harness：kill-9 子进程 READY 管道同步、FAULT_LOCK 串行化、URI 路由 fault VFS、WAL tail 截断扫描、typed 错误链断言、raw 行计数、逐场景 integrity_check；按本域单入口 `now` 裁剪为 6 场景）：

- **C1 pre-commit IOERR**（init 与 advance 两相位）：typed `Sqlite` 失败、错误链含注入条件；水位整体保持旧值、零新回执（零部分状态）；disarm 后同 key 重做收敛到确定性读数（时钟是确定性计数器：幻影应得读数可精确重现）；重放幂等；integrity ok。
- **C2 pre-commit ENOSPC**（`SQLITE_FULL`）：init/advance 两相位同一收敛。
- **C3 commit 点 PowerLoss 双向**：Phase A（丢写方向，`PowerLossAfter{0}`）tick「报告成功」→ 重开后回到旧高水位 0（旧值本身 ≥ 更早一切值——不回退）、零回执，同 key 重做逐字节等于幻影读数；Phase B（提交后 kill-9 可见方向）→ 3 个读数整体存活，fresh key 下一读数恰为 4（= 高水位 + 1，绝不回退到更早）。
- **C4 torn WAL tail**：5-tick 子进程快照，末段 1–2 个事务帧组的每个截断点（≥6 代表点，含帧界/半帧/末字节）逐一恢复重开：水位恒为旧值或新值（3 或 4，绝不撕裂）、integrity ok，且**水位 == 回执数 == 已签发最大读数**在每一点成立（推进与回执同事务、同生同灭）、恒 ≥ 幸存的更深提交（不回退）；同 key 重做缺失 tick 逐字节收敛；完整恢复对照相等。
- **C5 replay storm**：同 key 连放 3 次 + 重开后再放，逐字节相等，水位每 key 恰推进一次（不双跳）；fresh key 从 durable 高水位稠密续推（不回退）。
- **C6 注入后守卫仍有效**：一次断电崩溃 + 恢复 + 重做之后，raw SQL 篡改仍被 DDL 守卫 abort——水位不可减、不可插第二行、不可删、singleton 冻结、回执不可改不可删、回执读数不可越过水位；守卫下的权威读路径照常服务、durable 读数未被扰动。

happy-path 与 fail-closed 门：首调初始化 1；逐 key 恰 +1；重启持久；重放零推进；未知 schema 版本 `SchemaVersionUnsupported(7)`；raw 回退基线拒绝；读数耗尽 `CorruptRecord` 零状态变更。

## 3. Canonical commits

- 本 Attempt 按任务约束禁止 git 写操作；写集（`crates/nlos-clock/**`、根 `Cargo.toml` members 一行、本 evidence）按原子提交规范留待 integrator 基于上述验证结果落库。base HEAD `4f90511`。

## 4. 明确未完成（PARTIAL_PASS 保持）

- **无 IPC 接线**：AuthorityClock 未暴露跨进程传输（对照 ADR-0008 的 nlos-wait-control 先例，属后续切片）；
- **无外部时间源对齐**：显式不做——这是 ADR-0011 的复审触发器，不是本前缀缺陷；当前读数是纯逻辑计数，与真实时间无任何承诺关系；
- **无 validity/anti-replay 签发 API**：消费方（validity 签发、anti-replay 窗口）接线为后续切片；现公开 API 仅 `now`/`inspect`；
- 无 wall-clock 校准、无 epoch/offset 持久化（本前缀未发现需要）；
- kill-9 仅模拟进程崩溃（OS 页缓存存活）；真实掉电由 `PowerLossAfter` 与 WAL tail 截断**模型化**覆盖——与全部既有矩阵同一免责；
- 未运行项：`cargo --workspace` 级测试/clippy/fmt（任务约束仅允许 `-p nlos-clock` 定向命令）；CI 接入未做；Windows 平台未交叉验证。
