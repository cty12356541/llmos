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

## 5. Wall 域增量（2026-08-30，ADR-0011 validity 接入，base HEAD `baf86fa`）

ADR-0011 决定 3 的 validity 接入第一步：AuthorityClock 增补**持久化墙钟高水位** API（additive，schema v1→v2），供消费方以权威单调时间判定 validity，取代「上层传入时间」占位。§1–§4 描述的逻辑 tick 域语义不变；本节是**第二个严格分离的域**——逻辑 tick 与 wall 读数是两个域，不得混用。

### 5.1 实现事实

- **`wall_now(NowRequest) -> WallNowDecision`**：读系统时钟（单位 **ms since Unix epoch**，类型 `WallReading`）但强制单调——fresh key 读数 = `max(durable wall 水位, 系统时钟)`；**首次调用以系统时钟初始化 durable 水位（bootstrap 语义，本节登记）**；整个推进（replay 检查 → 系统时钟读 → 水位读 → CAS 写 → wall 回执插入 → commit）在一个 `Immediate` 事务内，与 `now()` 的 tick 纪律完全同构。任何重启/系统时钟回拨后读数不小于上次 durable 值。`Advanced`/`Replayed` 两分支镜像 `NowDecision`；`inspect_wall()` 为零副作用重启侧验证读。
- **幂等键回执完全镜像 `now()` 模式**：同 key 重放返回 durable 原读数（不双跳）且**不咨询系统时钟**——时钟坏了重放仍然可服务。
- **表独立（非 tick 表加 kind 列），论证**：(a) 单行水位表把单调不变量内嵌于行本身，一行只能承载一个域——共用会让 ~1.8e12 的 wall ms 读数 vault 逻辑计数器，直接破坏 `now()` 的稠密 +1 语义，而 no-insert trigger 又禁止第二行；(b) `tick_receipts` 行被 DDL 声明为 immutable（`BEFORE UPDATE` abort），改造列等于事后重释已 durable 的行，独立表让 tick 域 durable 面字节不动；(c) trigger SQL 绑定表名，域各表使两套守卫自洽、两套故障矩阵可独立断言。
- **域语义差异（显式登记）**：wall 读数**非稠密**——同一毫秒内的多个 key 共享一个读数（水位是系统时钟的高水位，不是每 key 计数器）；tick 读数稠密 +1。
- **时钟源注入**：公开 `WallSource` trait（`now_ms() -> Result<u64, _>`）+ 生产实现 `SystemWallSource`（`SystemTime`；epoch 之前/不可表示 → `WallClockUnavailable`）；`open_with_wall_source` 注入构造器。方案论证：回拨模拟必须跨重启（reopen 出的新实例也要注入），`#[cfg(test)]` 类方案对集成测试的 reopen 路径不可达，构造器级 seam 是最小侵入方案；`open()` 生产行为与签名不变。
- **fail-closed**：系统时钟不可用 → 新 typed error `WallClockUnavailable`（不猜时间），事务回滚零 durable 状态变更。
- **schema v2 additive 迁移**：`wall_watermark`（单行，种子 0=「尚未签发」）+ `wall_receipts`，六条 DDL 守卫逐条镜像 tick 域（不可减/不可插/不可删/singleton 冻结/回执不可改不可删/回执读数 ≤ 水位；`reading_ms >= 1`——恰好 epoch 的系统时钟与「尚未签发」不可区分，fail-closed）。v1 存量库重开自动升级，tick 域状态字节不动（有测试锁定）。

### 5.2 验证（wall 增量部分，base HEAD `baf86fa` 工作区未提交写集）

```text
cargo test -p nlos-clock -p nlos-identity
  → clock_authority 4 passed（原有，含 v2 迁移透明生效）
  → clock_fault_injection 11 passed（原 6 场景 + 新 W1–W4 wall 写窗口）
  → clock_wall_authority 6 passed（新增）
cargo clippy -p nlos-clock -p nlos-identity --all-targets -- -D warnings → 0 warning
cargo +nightly-2026-08-01 clippy（同前）→ 0 warning
cargo fmt 双工具链 -- -p nlos-clock -p nlos-identity --check → 干净
```

- `clock_wall_authority`（6）：bootstrap=源值；`max(durable, system)` 推进；源回拨（2_500→2_400）被吸收、fresh key 恒 ≥ durable 水位；同 ms 非稠密共享读数；同 key 重放字节相等且水位不动；**重启后**源回拨到 1 仍不回退；`SystemWallSource` 读数落在观测窗口内；源故障 `WallClockUnavailable` 零状态且重放仍服务；v1→v2 additive 升级保留 tick 状态；wall 域 DDL 守卫 raw 篡改全数 abort；tick/wall 双向域隔离（wall 推进不动 tick 水位，tick 推进不动 wall 水位）。
- 故障矩阵 W1–W4（镜像 C1–C4 精简为 wall 单入口写窗口）：W1 pre-commit IOERR（bootstrap/advance 两相位）typed 失败、零部分状态、disarm 重做**单调**收敛——与 tick 的逐字节确定性收敛不同（源是系统时钟），此域差异显式登记；W2 ENOSPC 同收敛；W3 PowerLoss 双向（不可见方向幻影消失整体、同 key 重做推进水位；kill-9 可见方向已提交读数整体存活、重放字节相等、fresh key ≥ 水位）；W4 torn WAL tail（≥6 代表截断点：水位恒为旧值或新值、integrity ok、**水位 == 幸存最后一条回执读数**（推进与回执同事务同生同灭）、恒 ≥ 更深幸存提交、幸存回执与控制前缀逐字节相等、缺失 key 重做单调收敛、完整恢复对照相等）。

### 5.3 本节新增已知限制

- wall 校准无外部时间源对齐：wall 读数只锚定本地系统时钟，这是 ADR-0011 复审触发器，非本切片缺陷；
- wall 同 key 重做是单调收敛而非逐字节确定性（tick 域是确定性的）——源非确定性所致，durability 不变量（不回退）不受影响；
- §4 中「无 wall-clock 校准」「现公开 API 仅 now/inspect」的表述由本节取代；三服务 IPC 接线仍未消费（见 b-identity-001 §6）。
