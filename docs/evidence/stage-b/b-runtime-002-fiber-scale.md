# B-RUNTIME-002：100K dormant/waiting Fiber 承载证明（ROAD-B-006 前片）

- 状态：`PARTIAL_PASS`（ROAD-B-006 的承载量 + 唤醒正确性 + 线程有界性前片 + cancel/late-callback 功能矩阵（§6，2026-09-02 追加）+ 阻塞 I/O 负向证明最小前缀（§6.7，2026-09-05 追加）；structured join/detach、Process crash propagation、分维 Activation metering 已做；100K 规模级 cancel 探针未做，ROAD-B-006 整体不达成）
- 日期：2026-08-31（macOS arm64 单平台实测）
- Owner：`nlos-runtime-tokio`（`tests/durable_wait_scale.rs`）
- 设计依据：[架构设计总纲 v0.5 §28.2 ROAD-B-006](../../design/06-架构设计总纲-v0.5.md)（「单机 runtime MUST 证明有限宿主线程可承载至少 100K dormant/waiting Fiber……未完成前不得声称 coroutine 级大规模并发」）
- 组织先例：`crates/nlos-store/tests/store_scale.rs` 与 `crates/nlos-runtime-tokio/tests/scale.rs`（`#[ignore]` 门控 + `--include-ignored` 夜间 scale-probe CI job）

## 1. Probe 组织

- **真实挂起路径**：每个 fiber 在自身任务体内经 `TokioRuntimeAdapter::wait_for_channel` 向 nlos-wait 权威注册**独立 durable wait 行**（binding/target sequence/key 逐 fiber 唯一），再 await 内存等待——不是 `pending()` 内存桩。durable 注册 = 单行 SQLite `Immediate` 事务（WAL + `synchronous=FULL` fsync）+ owner 回读 + SHA-256 派生。
- **通道拓扑**：共享单 channel，fiber i 的 target = i+1 → 选择性唤醒前 1000 目标 = 一次 `notify_commits(up_to=1000)`，durable 侧行 CAS 翻转即天然精确子集。
- **分层两档**（不造假、常规套件零影响）：
  - 10K 快探针 `ten_thousand_durable_wait_fibers_on_two_workers`（`#[ignore]`）；
  - 100K 全量探针 `one_hundred_thousand_durable_wait_fibers_on_two_workers`（`#[ignore]`）。
  两者均在固定 `multi_thread` runtime（`worker_threads = 2`，`max_live_fibers = count`）下运行。
- **测量口径**：spawn_issue（100K 次 `spawn_fiber` 调用发起）、register_settle（全员 `FiberState::WaitingIo` 可见 = 全部 durable 行注册完成）、wake_notify / wake_deliver / wake_settle（目标子集全部 `Completed`）、durable_readback（`inspect_channel_waits` 全行校验）、RSS（`ps -o rss=`，进程外读）、线程数（`ps -M`，进程外读；Linux 走 `/proc/self/status`，其余平台 stub 编译保障）。

## 2. 断言（全部通过）

1. `registered_fibers() == count`；全员可见 `WaitingIo`（超时预算 20ms/注册上界，实测远低于）。
2. `WakeReport` 恰好 1000 行、全部 `WOKEN` 且 `target ≤ 1000`；`deliver` 恰好 `delivered=1000, buffered=0`。
3. 目标子集全员 `Completed`（fiber 体内只接受 `WaitOutcome::Woken`，否则 panic → `Failed` 必现于状态断言）；**99K 未目标 fiber 全员仍 `WaitingIo`（无误唤醒）**。
4. durable 复核：`inspect_channel_waits` 恰好 count 行，`WOKEN` 前缀（target 1..=1000）/ `PENDING` 尾部（target 1001..=count）逐行一致。
5. 线程有界：`threads_after ≤ 10`（固定上界）且 `≤ threads_before + 2`（不随 fiber 数增长）。

## 3. 实测数据（macOS arm64，2 tokio worker，与其他 lane 并行负载下）

100K 全量探针（最终代码，exit 0，两次独立运行均绿）：

```text
100000-fiber profile:
  spawn_issue=496.7ms   register_settle=211.50s（≈2.1ms/durable 注册，fsync 主导）
  wake_notify=93.7ms    wake_deliver=1.086s    wake_settle=3.208s
  durable_readback=916ms
  rss_before=5232KiB    rss_after=200896KiB（Δ≈196MB ≈ 2.0KiB/fiber，含 SQLite 页缓存）
  threads_before=4      threads_after=4
  总耗时 277s（首次运行 277.34s：register_settle=269.9s、rss_after=124176KiB、threads 4→4——数量级一致，RSS 波动为并发负载下的分配差异，如实记录）
```

10K 快探针（exit 0）：

```text
10000-fiber profile:
  spawn_issue=24.2ms    register_settle=5.00s（≈0.5ms/注册）
  wake_notify=26.5ms    wake_deliver=284.7ms   wake_settle=388.2ms
  durable_readback=130.3ms
  rss_before=5232KiB    rss_after=27712KiB（Δ≈22MB）
  threads_before=4      threads_after=4
```

**结论**：10 万 dormant fiber 全部挂在独立 durable wait 上，宿主线程数恒为 4（main + 2 worker + 观测口径内常数），与 fiber 数无关；内存 ≈2KiB/fiber 量级；唤醒路径精确无误唤醒（运行时态 + durable 行双重复核）。

## 4. 验证门

```text
cargo test -p nlos-runtime-tokio
  → 58 passed / 0 failed（常规套件含 scale.rs 10K 内存探针；新增 durable_wait_scale 2 项 #[ignore] 不拖慢常规）
cargo test -p nlos-runtime-tokio --test durable_wait_scale -- --ignored ten_thousand --nocapture
  → 1 passed / 0 failed（exit 0）
cargo test -p nlos-runtime-tokio --test durable_wait_scale -- --ignored one_hundred_thousand --nocapture
  → 1 passed / 0 failed（exit 0，两次独立运行）
cargo clippy -p nlos-runtime-tokio --all-targets -- -D warnings       → exit 0（stable）
cargo +nightly clippy -p nlos-runtime-tokio --all-targets -- -D warnings → exit 0（nightly）
cargo fmt -p nlos-runtime-tokio -- --check                            → 通过（stable）
```

## 5. 已知限制与未运行项

- **单平台实测**：数字均为 macOS arm64（APFS fsync 特性敏感，register_settle 主导项）；Linux/Windows 数字待夜间 scale-probe CI job（`--include-ignored`，ubuntu）补充。
- **MSRV 双工具链未本地执行**：本地 1.97 toolchain 安装损坏（`librustc_driver` dylib 缺失 → 其 rustfmt/check 均不可用），fmt 双工具链与 MSRV check 以 CI（stable/Linux fmt 门 + ubuntu MSRV job）为准，如实标注。
- **in-memory 注册表线性扫描**：`deliver`（逐 report 行线性 find）与 fiber 终态 purge（`retain` 全表）在 100K 挂起下呈 O(n) 每事件，wake 阶段 ~4.3s 可接受但属已登记的规模特征，非本前片修改对象。
- **未做（ROAD-B-006 其余退出门，登记后续）**：~~cancel/late-callback 矩阵~~（功能级矩阵已落地，见 §6；100K 规模级 cancel 探针仍因 O(n²) 终态 purge 未纳入）、~~structured join/detach（API 不存在，如实登记缺口，见 §6.4）~~（合同层最小前缀已落地，见 §6.5）、~~Process crash propagation~~（见 `b-process-003-crash-propagation.md`）、~~分维 Activation metering 最小前缀~~（见 §6.6）、~~阻塞 I/O 负向证明~~（见 §6.7）；100K `cancel_scope` 收尾因 O(n²) 终态 purge 未纳入探针， teardown 走 drop。
- probe 为 `#[ignore]`，常规 CI（push/PR）不运行；夜间 scale-probe job 覆盖。

## 6. cancel/late-callback 功能矩阵（2026-09-02 追加，勾销 §5 中 cancel/late-callback 项的功能级部分）

- Owner：`nlos-runtime-tokio`（`tests/cancel_late_callback_matrix.rs`，新增 6 测试，独立文件零侵入既有测试）
- 设计依据：v0.5 §28.2 ROAD-B-006「通过 cancel/late-callback……测试」；断言口径 = 已落地 cancel 语义实测（runtime 侧 cancel 只解内存等待、永不动 durable 行，durable `PENDING→CANCELLED` 翻转专属显式 `WaitAuthority::cancel_wait`；fiber 终态 biased-select 下取消优先；终态转换与 wait 注册表 purge 同临界区；`resume_from_wait` 永不覆写终态）。
- 挂起路径：全部 fiber 在**自身 task 体内**经 `wait_for_channel` 注册并挂起于独立 durable wait 行（同 `durable_wait_scale.rs` 的真实挂起路径，非 `pending()` 桩 + 外部注册）。

### 6.1 既有覆盖勘察（防重复清单）

已覆盖（本车道不重复）：`runtime.rs`（plain pending fiber 取消 + 代次围栏）、`wake.rs`（scope 取消解 Operation wait；wake 至 Completed fiber → `NotWaiting`）、`channel_wait.rs`（外部注册 wait 的 scope 取消保留 durable PENDING；fiber 终止 purge；shutdown）、`channel_rehydration.rs`（rearm + scope 取消）、`fiber_replay.rs`（durable 侧 `cancel_wait` 重放事实）、`outbox.rs`（Operation 层 `request_cancel` 后晚到 callback）。

本车道新增（6 项，全部针对 fiber 体内 durable-wait 挂起场景）：

1. `cancel_while_parked_on_durable_wait_yields_cancelled_and_keeps_row_pending` —— durable wait 挂起中取消：终态唯一 `Cancelled`；durable 行处置符合契约（保留 `PENDING`，运行时侧不做 durable cancel）。
2. `wait_registration_after_scope_cancel_resolves_ready_cancelled_with_zero_durable_side_effect` —— 取消后注册：ready `Cancelled`（scope 门或终态门，两态竞态下结果同一），零 durable 副作用（`list_waits` 空 + 同 key 仍可全新 `Registered`）。
3. `wake_then_cancel_order_has_unique_cancelled_terminal_with_durable_wake_kept` —— 先唤醒后取消：wake 先被消费（fiber 回 `Running`），随后取消经 biased select 确定性胜出 → 终态唯一 `Cancelled`；durable 行保持 `WOKEN`（唤醒事实不被取消回滚）。
4. `cancel_then_wake_order_has_unique_cancelled_terminal_and_delivery_buffers` —— 先取消后唤醒：channel `deliver` 遇已 purge 注册表 → `buffered=1` 不 panic；Operation `wake` → `NotWaiting`；`wait_for_operation` ready `Cancelled`；终态唯一 `Cancelled` 双重复核（settle 后仍终态）。
5. `respawn_after_cancel_is_fenced_by_scope_and_fiber_generations` —— 取消后重 spawn 代次守卫：已取消 scope 拒新 fiber（`Cancelled`）；同 scope id 换 cancellation_generation → `InvalidGeneration`（scope 单代次绑定）；终态 fiber 身份仍占位（同 id 同代 → `DuplicateFiber`；同 id 换 fiber 代 → `InvalidGeneration`）；新身份新 scope 正常 spawn（对照组）。
6. `late_delivery_to_cancelled_fiber_buffers_without_wakeup_and_stays_consumable` —— 晚到 callback：取消后 `notify_commits` 翻转未被动过的 `PENDING` 行（durable 事实幸存）→ `deliver` `buffered=1` 不 panic 不误唤醒；settle 后状态仍 `Cancelled`；对已取消 fiber `rearm_channel_waits` → 空报告（占位 buffer 不得复活为活等待）；同 request 的新 waiter replay `WOKEN` 行立即 `Woken`（at-least-once：durable 事实仍可被合法后继消费）。

### 6.2 验证门实测

```text
cargo test -p nlos-runtime-tokio
  → 64 passed / 0 failed / 3 ignored（13 个 test target 全绿；58 既有 + 6 新增；
    ignored = durable_wait_scale 2 项 + scale.rs 100K 1 项，与既有口径一致）
cargo fmt -p nlos-runtime-tokio -- --check             → 通过（stable）
cargo +nightly fmt -p nlos-runtime-tokio -- --check    → 通过（nightly；本地补装 nightly rustfmt component）
```

clippy 双工具链 `-D warnings`：**本地三次运行均被并行车道在途代码阻塞，非本 crate 问题，如实登记**——`-p nlos-runtime-tokio` 依赖图含 `nlos-semantic`（经 nlos-task/nlos-store），该 crate 正被并行车道编辑：第一次运行为 15 项 dead_code（全部位于 `nlos-semantic/src/model.rs`）；后两次运行为硬编译错误 E0428（`schema.rs` `migrate_v5` 重复定义）及 E0432（unresolved imports）。三次日志中 **`nlos-runtime-tokio` 自身 0 warnings / 0 errors**。该门以 CI（顺序化、无并行编辑）为权威复跑。

环境注记：验证期间本机多次出现测试二进制 exec 前停滞（`codesign`/`sample` 对新编译二进制同样停滞，诊断为 macOS 安全策略守护进程卡顿，17 天 uptime），停滞自行消散后全套件一次通过；非代码问题，仅记录。

### 6.3 缺口更新

- **勾销**：§5「cancel/late-callback 矩阵」功能级部分 → 本 §6 落地（6/6 绿）。100K 规模级 cancel 探针（`cancel_scope` 收尾）仍随 O(n²) 终态 purge 一并保留于 §5 已登记限制。
- **如实保留（ROAD-B-006 剩余，整体不达成）**：~~阻塞 I/O 负向证明~~（见 §6.7 最小前缀）、~~Process crash propagation~~（见 `b-process-003-crash-propagation.md`）、~~分维 Activation metering 最小前缀~~（见 §6.6）。

### 6.4 structured join/detach：API 缺口登记（已由 §6.5 勾销）

勘察结论（2026-09-02）：`nlos_runtime::RuntimeAdapter` trait 表面为 `spawn_fiber` / `cancel_scope` / `inspect` / `activation_usage`，**不存在 structured join/detach API**。2026-09-04 W11-J 在合同层落地最小前缀，见 §6.5。

### 6.5 structured join/detach 最小前缀（2026-09-04 追加，W11-J / ROAD-B-006）

- Owner：`nlos-runtime`（合同）+ `nlos-runtime-tokio`（`src/lib.rs` + `tests/join_detach.rs`）
- 设计依据：v0.5 §28.2 ROAD-B-006「structured join/detach」；`[FIBER-CANCEL-001]` 父 scope 结束前须 join/cancel/显式 detach；不得外泄 `tokio::JoinHandle`。
- **合同层**：`RuntimeAdapter` 新增 `join_fiber(handle) -> Result<FiberExit, RuntimeError>` 与 `detach_fiber(handle) -> Result<(), RuntimeError>`。`spawn_fiber` 文档化**隐式 detach**（成功即并发运行，不强制 join）；`detach_fiber` 为显式 relinquish（校验 handle，不改变调度/admission）。
- **Tokio 实现**：`FiberRecord` 以 `Condvar` + `TerminalOutcome` 在终态转换时发布 `FiberExit`（与 cancel biased-select、终态唯一、wait 注册表 purge 同 `run_fiber` 临界区）；`join_fiber` 代次围栏经 `record_for`；已终态 join 幂等；内部仍用 `Handle::spawn`，不外泄 executor handle。
- **新增测试**（`join_detach.rs`，6 项）：
  1. `join_waits_for_fiber_completion` — join 阻塞至 fiber 完成并返回 `FiberExit::Completed`。
  2. `stale_generation_join_is_rejected` — 过期代次 → `InvalidGeneration`。
  3. `join_on_terminal_fiber_is_idempotent` — 终态后重复 join 返回同一 exit、不阻塞。
  4. `implicit_detach_recovers_admission_without_join` — 不调用 join，fiber 终态后 admission 槽回收（`max_live_fibers=1` 可再 spawn）。
  5. `join_returns_cancelled_after_scope_cancel` — cancel 竞态下 join 返回 `Cancelled`、状态唯一。
  6. `explicit_detach_is_a_noop_that_validates_handle` — 显式 detach 校验 handle；过期代次拒绝。

#### 6.5.1 验证门实测

```text
cargo test -p nlos-runtime-tokio --test join_detach
  → 6 passed / 0 failed（2026-09-04 W12-J）
cargo test -p nlos-runtime-tokio
  → 70 passed / 0 failed / 3 ignored（13 个 test target 全绿；64 既有 + 6 join_detach；
    ignored = durable_wait_scale 2 项 + scale.rs 100K 1 项）
cargo clippy -p nlos-runtime -p nlos-runtime-tokio --all-targets -- -D warnings
  → exit 0（stable，2026-09-04 W12-J）
cargo fmt -p nlos-runtime -p nlos-runtime-tokio -- --check
  → 通过（stable，2026-09-04 W12-J）
```

#### 6.5.2 缺口更新

- **勾销**：§6.4 合同层缺口 → 本 §6.5 最小前缀（join + 显式 detach + 隐式 detach 文档化）。
- **如实保留（ROAD-B-006 剩余，Claim 维持 PARTIAL_PASS）**：~~Process crash propagation~~（见 `b-process-003-crash-propagation.md`）、~~分维 Activation metering~~（见 §6.6 最小前缀）、~~阻塞 I/O 负向证明~~（见 §6.7）、100K 规模级 cancel 探针；未声称 ROAD-B-006 整体达成。

### 6.6 Activation meter 最小前缀（2026-09-05 追加，W13-M / ROAD-B-006）

- Owner：`nlos-runtime-tokio`（`src/lib.rs` + `tests/activation_meter.rs`）
- 设计依据：v0.5 §28.2 ROAD-B-006「分维 Activation metering」；`ActivationUsage` 合同已在 `nlos-runtime` 定义（`active_cpu`、`elapsed_wall`、`scheduler_wait`、`external_wait`、`backpressure_wait`、`suspended`）。
- **实现（最小、additive）**：
  - `FiberRecord` 以 `UsageAccumulator` + `UsagePhase` 在状态边界累计：`begin_wait` / `resume_from_wait`（Operation wait 与 Channel wait 共用路径）累计 `external_wait`；`Running` 执行段累计 `active_cpu`；终态 `finalize` 收口在途相位。
  - `spawn_fiber` 仅设置可见 `Running` 态（`set_state_without_metering`），CPU 计量从 `run_fiber` 首次 poll 开始，避免 scheduler 排队误计 `active_cpu`；`set_state(Running)` 不覆写 `WaitingIo`（仅 `resume_from_wait` 可离开等待态）。
  - 既有 `scheduler_wait` + `elapsed_wall` 口径不变；`backpressure_wait` / `suspended` 仍为默认零。
- **新增测试**（`activation_meter.rs`，3 项）：
  1. `operation_wait_accumulates_external_wait_not_active_cpu` — Operation wait 挂起 50ms：`external_wait ≥ 40ms` 且 `active_cpu < external_wait`。
  2. `compute_fiber_records_active_cpu_against_elapsed_wall` — 纯计算 fiber：`active_cpu` 与 `elapsed_wall` 同量级、`external_wait = 0`。
  3. `join_then_activation_usage_readback_is_stable` — join 后重复 `activation_usage` 读回一致。

#### 6.6.1 验证门实测

```text
cargo test -p nlos-runtime-tokio
  → 73 passed / 0 failed / 3 ignored（14 个 test target 全绿；70 既有 + 3 activation_meter；
    ignored = durable_wait_scale 2 项 + scale.rs 100K 1 项）
cargo test -p nlos-runtime-tokio --test activation_meter
  → 3 passed / 0 failed（2026-09-05 W13-M）
cargo clippy -p nlos-runtime-tokio --all-targets -- -D warnings
  → exit 0（stable，2026-09-05 W13-M）
cargo fmt -p nlos-runtime-tokio -- --check
  → 通过（stable，2026-09-05 W13-M）
```

#### 6.6.2 缺口更新

- **勾销**：§6.5.2 / §6.3 中「分维 Activation metering」功能级最小前缀 → 本 §6.6（`external_wait` + `active_cpu` 两维；join 后读回稳定）。
- **如实保留（ROAD-B-006 剩余，Claim 维持 PARTIAL_PASS）**：
  - 100K 探针下分维 metering 规模验证（`backpressure_wait` / `suspended` 维仍为零占位）；
  - ~~阻塞 I/O 负向证明~~（见 §6.7 最小前缀）；
  - 100K 规模级 cancel 探针；
  - 未声称 ROAD-B-006 整体达成。

### 6.7 阻塞 I/O 负向证明最小前缀（2026-09-05 追加，W15-B / ROAD-B-006）

- Owner：`nlos-runtime-tokio`（`tests/blocking_io_negative.rs`）
- 设计依据：v0.5 §28.2 ROAD-B-006「阻塞 I/O 不线性占用宿主线程」；本片为 **负向证明**——fiber 数增长时宿主线程数不得线性增长。
- **挂起路径**：与 `durable_wait_scale.rs` 相同——每个 fiber 在**自身 task 体内**经 `wait_for_channel` 注册独立 durable wait 行并挂起；阻塞 I/O 在 durable 注册**之前**执行。
- **阻塞 I/O 两种模式**（均测，互补）：
  1. **隔离模式**：`tokio::task::spawn_blocking` + 2ms 模拟阻塞 I/O；探针 runtime 显式 `max_blocking_threads = 8`，证明有界 blocking pool 不随 fiber 数线性扩线程。
  2. **误用模式**：fiber 体内直接 `std::thread::sleep`（占用 worker 但不 spawn 每 fiber 一线程）；256 fiber 全挂起后线程仍 ≤ `THREAD_BOUND`（10）。
- **负向证明方法论**：
  1. **比较 tiers**：32 vs 256 fiber（8×），全达 `WaitingIo` 后测 `ps -M` / `/proc/self/status` 线程数；断言 `threads(256) ≤ threads(32) + 15`（sub-linear headroom）。
  2. **绝对上界**：256 fiber + spawn_blocking（cap=8）≤ `BLOCKING_IO_THREAD_BOUND`（16）；误用模式 ≤ 10。
  3. **可选 `#[ignore]` 10K tier**：与 durable_wait_scale 同形状，夜间 scale-probe 可 `--include-ignored` 复跑。
- **新增测试**（`blocking_io_negative.rs`，3 项：2 常规 + 1 `#[ignore]`）：
  1. `blocking_io_on_durable_wait_path_grows_threads_sublinearly` — spawn_blocking 后 durable wait：8× fiber 线程增长 sub-linear。
  2. `misplaced_blocking_sleep_stays_thread_bounded_on_durable_wait_path` — 误用 blocking sleep：256 fiber 线程有界。
  3. `ten_thousand_blocking_io_fibers_stay_thread_bounded` — 10K `#[ignore]` 快探针。

#### 6.7.1 验证门实测

```text
cargo test -p nlos-runtime-tokio --test blocking_io_negative
  → 2 passed / 0 failed / 1 ignored（2026-09-05 W15-B）
cargo test -p nlos-runtime-tokio
  → 75 passed / 0 failed / 4 ignored（15 个 test target 全绿；73 既有 + 2 blocking_io_negative 常规项；
    ignored = durable_wait_scale 2 项 + scale.rs 100K 1 项 + blocking_io_negative 10K 1 项）
cargo clippy -p nlos-runtime-tokio --all-targets -- -D warnings
  → exit 0（stable，2026-09-05 W15-B）
cargo fmt -p nlos-runtime-tokio -- --check
  → 通过（stable，2026-09-05 W15-B）
```

#### 6.7.2 缺口更新

- **勾销**：§6.3 / §6.5.2 / §6.6.2 中「阻塞 I/O 负向证明」→ 本 §6.7 最小前缀（sub-linear + 绝对有界；test-only，零 src 侵入）。
- **如实保留（ROAD-B-006 剩余，Claim 维持 PARTIAL_PASS）**：
  - 100K 探针下 blocking I/O 负向实跑（10K `#[ignore]` tier 待夜间 job）；
  - 100K 规模级 cancel 探针；
  - 未声称 ROAD-B-006 整体达成。

### 6.8 backpressure_wait / suspended 生命周期最小前缀（2026-09-05 追加，W16-006 / ROAD-B-006）

- Owner：`nlos-runtime-tokio`（`src/lib.rs` + `tests/lifecycle_phase.rs`）
- 设计依据：v0.5 §28.2 ROAD-B-006 分维 Activation metering；`[FIBER-METER-001]` blocked-on-backpressure 与 suspended time 维。
- **实现（最小、additive）**：
  - 公开 `FiberLifecyclePhase::{Running,WaitingExternal,BackpressureWait,Suspended}`，由 `TokioRuntimeAdapter::inspect_lifecycle_phase` 读回；`begin_wait`/`resume_from_wait` 同步维护 `WaitingExternal`。
  - 调度/admission 背压边界：`begin_backpressure_wait` / `resume_from_backpressure_wait`（`FiberState::WaitingModel` 映射）；Running→BackpressureWait 收口 `active_cpu`、累计 `backpressure_wait`。
  - 协作挂起边界：`begin_suspended` / `resume_from_suspended`（`FiberState::Suspended`）；Running→Suspended 收口 `active_cpu`、累计 `suspended`。
  - `UsageAccumulator`/`UsagePhase` 扩展四相位终态 `finalize`/`snapshot`；既有 `external_wait`/`active_cpu`/`scheduler_wait`/`elapsed_wall` 口径不变。
  - `set_state(Running)`  guard 扩展：不覆写 `WaitingIo`/`WaitingModel`/`Suspended`（仅对应 resume 路径可离开）。
- **新增测试**（`lifecycle_phase.rs`，4 项）：
  1. `backpressure_wait_exposes_lifecycle_phase_and_fiber_state` — Running→BackpressureWait→Running 相位与 `WaitingModel` 可见。
  2. `backpressure_wait_accumulates_backpressure_not_external_wait` — 背压挂起 50ms：`backpressure_wait ≥ 40ms` 且 `external_wait = 0`。
  3. `suspended_exposes_lifecycle_phase_and_fiber_state` — Running→Suspended→Running 相位与 `FiberState::Suspended` 可见。
  4. `suspended_accumulates_suspended_not_external_wait` — 挂起 50ms：`suspended ≥ 40ms` 且 `external_wait`/`backpressure_wait = 0`。

#### 6.8.1 验证门实测

```text
cargo test -p nlos-runtime-tokio
  → 79 passed / 0 failed / 4 ignored（16 个 test target 全绿；75 既有 + 4 lifecycle_phase；
    ignored = durable_wait_scale 2 项 + scale.rs 100K 1 项 + blocking_io_negative 10K 1 项）
cargo test -p nlos-runtime-tokio --test lifecycle_phase
  → 4 passed / 0 failed（2026-09-05 W16-006）
cargo clippy -p nlos-runtime-tokio --all-targets -- -D warnings
  → exit 0（stable，2026-09-05 W16-006）
cargo fmt -p nlos-runtime-tokio -- --check
  → 通过（stable，2026-09-05 W16-006）
```

#### 6.8.2 缺口更新

- **勾销**：§6.6.2 中「`backpressure_wait` / `suspended` 维仍为零占位」→ 本 §6.8 功能级最小前缀（显式边界 API + 两维 metering + 相位 inspect）。
- **如实保留（ROAD-B-006 剩余，Claim 维持 PARTIAL_PASS）**：
  - 100K 探针下分维 metering 规模验证（背压/挂起维仍为零占位的 100K 实跑未做）；
  - fiber 体内自动触发背压/挂起（本片为 scheduler 边界显式 hook，非 admission 阻塞 spawn 集成）；
  - 100K 规模级 cancel 探针；
  - runtime 侧 process crash 传播联动；
  - 未声称 ROAD-B-006 整体达成。

### 6.9 W17-006：Activation meter 100K 规模探针骨架（2026-09-06）

- **写集**：`crates/nlos-runtime-tokio/tests/activation_meter_scale.rs`（新增 `#[ignore]` 10K quick tier + 100K full tier；两 worker 恒定线程；`active_cpu` 与 `external_wait` 分维采样断言；teardown 走 drop 避免 O(n²) cancel purge，与 §6.3 一致）。
- **验证门（编译 + 默认套件，探针未实跑）**：

```text
cargo test -p nlos-runtime-tokio --test activation_meter_scale -- --test-threads=1
  → 0 passed / 0 failed / 2 ignored（2026-09-06 W17-006）
cargo test -p nlos-runtime-tokio -- --test-threads=1
  → 79 passed / 0 failed / 6 ignored（+2 activation_meter_scale ignore；2026-09-06 W17-006）
```

- **缺口**：10K/100K 探针 `--include-ignored` 实跑未在本增量执行；背压/挂起维 100K 规模验证仍缺；不得外推 ROAD-B-006 达成。
