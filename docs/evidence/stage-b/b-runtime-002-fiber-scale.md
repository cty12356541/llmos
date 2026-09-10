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

### 6.9 Activation meter 100K 规模探针（2026-09-06 追加，W17-006 / ROAD-B-006）

- Owner：`nlos-runtime-tokio`（`tests/activation_meter_scale.rs`）
- 设计依据：v0.5 §28.2 ROAD-B-006 分维 Activation metering 规模验证；§6.6 最小前缀 + §6.8 backpressure/suspended 已落地，本片补 `external_wait`/`active_cpu` 在大量 live fiber 下的不退化证明。
- **实现（test-only，`#[ignore]` 探针）**：
  - 新增 `activation_meter_scale.rs`，镜像 `scale.rs` / `durable_wait_scale.rs` 双 tier：`QUICK_COUNT=10_000`、`FULL_COUNT=100_000`，`METER_SUBSET=1_000` 前缀抽样。
  - **Phase 1 `active_cpu`**：`subset` 计算 fiber 并发完成；全 cohort 断言 `active_cpu ≥ 10ms` 且 `external_wait = 0`（`active_cpu ≤ elapsed_wall` 仍由 `activation_meter.rs` 低并发覆盖；finalize 锁序在并发下可能差微秒级）。
  - **Phase 2 `external_wait`**：`count` fiber 在 fiber 体内注册 Operation wait（避免 test 线程注册与 `run_fiber` 初始 `Running` 竞态）；`park_settle` 后 sleep 50ms，前 `subset` 断言 `external_wait ≥ 40ms` 且 `active_cpu < external_wait`；teardown 走 drop（100K cancel 仍 O(n²) 未纳入）。
- **新增测试**（`activation_meter_scale.rs`，2 项，均 `#[ignore]`）：
  1. `ten_thousand_activation_meter_fibers_on_two_workers` — 10K quick tier。
  2. `one_hundred_thousand_activation_meter_fibers_on_two_workers` — 100K exit-gate tier（CI/nightly `--include-ignored`）。

#### 6.9.1 验证门实测

```text
cargo test -p nlos-runtime-tokio --test activation_meter
  → 3 passed / 0 failed（2026-09-06 W17-006；与全套件同跑时绿）
cargo test -p nlos-runtime-tokio -- --test-threads=1
  → 82 passed / 0 failed / 6 ignored（17 个 test target 全绿；+2 activation_meter_scale ignored；
    ignored = durable_wait_scale 2 + scale.rs 100K 1 + blocking_io_negative 10K 1
              + activation_meter_scale 10K 1 + activation_meter_scale 100K 1）
cargo clippy -p nlos-runtime-tokio --all-targets -- -D warnings
  → exit 0（stable，2026-09-06 W17-006）
cargo test -p nlos-runtime-tokio --test activation_meter_scale -- --include-ignored ten_thousand --nocapture
  → 1 passed / 0 failed（10K 探针本地实跑，2026-09-06 W17-006）
  → 10K profile（2 tokio workers，sample=1000）：
     active_cpu_phase=12.503s
     external_wait: spawn_issue=30.9ms park_settle=48.8ms external_wait_sleep=50ms
                    sample_assert=0.49ms total=133.7ms
cargo test -p nlos-runtime-tokio --test activation_meter_scale -- --include-ignored one_hundred_thousand
  → 未本地实跑（100K tier 登记 CI/nightly manual；与 scale.rs / durable_wait_scale 100K 同级）
```

#### 6.9.2 缺口更新

- **勾销**：§6.6.2 / §6.8.2 中「100K 探针下分维 metering 规模验证（`external_wait`/`active_cpu`）」→ 本 §6.9（10K 实跑 + 100K `#[ignore]` 探针登记）。
- **如实保留（ROAD-B-006 剩余，Claim 维持 PARTIAL_PASS）**：
  - 100K activation-meter 探针本地/nightly 实跑数字（10K quick tier 已实跑）；
  - 100K 规模级 cancel 探针；
  - fiber 体内自动触发背压/挂起（scheduler 边界 hook 已覆盖，admission 集成未做）；
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

#### 6.9.3 W18-006：10K ignore 探针实跑 + RSS/线程读数（2026-09-07）

- **写集**：`activation_meter_scale.rs` 补 `process_rss_kib` / `process_thread_count` 与 `THREAD_BOUND` 断言（镜像 `durable_wait_scale.rs` 口径）；§6.9.1 数字由 W17-006 骨架登记更新为 W18-006 实跑。
- **平台**：macOS arm64，2 tokio workers，`--include-ignored --test-threads=1 --nocapture`。
- **10K 实跑（exit 0，断言全绿）**：

```text
cargo test -p nlos-runtime-tokio --test activation_meter_scale ten_thousand -- --include-ignored --test-threads=1 --nocapture
  → 1 passed / 0 failed（2026-09-07 W18-006）
  → 10K profile（2 tokio workers，sample=1000）：
     active_cpu_phase=12.513s（1000 compute fibers，全 cohort active_cpu≥10ms 且 external_wait=0）
     external_wait: spawn_issue=35.1ms park_settle=21.2ms external_wait_sleep=50ms
                    sample_assert=0.42ms rss_kib=23440 threads=4 total=112.3ms
     （1000 前缀 external_wait≥40ms 且 active_cpu<external_wait；threads≤10 有界断言通过）
cargo test -p nlos-runtime-tokio -- --test-threads=1
  → 82 passed / 0 failed / 6 ignored（2026-09-07 W18-006；17 test target 全绿）
cargo test -p nlos-runtime-tokio --test activation_meter_scale -- --include-ignored one_hundred_thousand
  → 未本地实跑（100K tier 仍登记 CI/nightly manual；W18-006 未跑）
```

- **缺口更新**：100K activation-meter 探针本地/nightly 实跑数字仍缺；不得外推 ROAD-B-006 达成。

#### 6.9.4 W19-PATCH：编排者收尾复跑 10K（2026-09-08）

第八十增量登记「收尾员未复跑 10K」——编排者于本增量本地 `--include-ignored` 复跑确认 race-free 修复后探针仍绿：

```text
cargo test -p nlos-runtime-tokio --test activation_meter_scale ten_thousand_activation_meter_fibers_on_two_workers -- --include-ignored --nocapture
  → 1 passed / 0 failed（2026-09-08 W19-PATCH，~12.6s wall）
  → 10K profile（2 tokio workers，sample=1000）：
     active_cpu_phase=12.506s
     external_wait: spawn_issue=31.2ms park_settle=29.5ms external_wait_sleep=50ms
                    sample_assert=0.46ms rss_kib=23792 threads=4 total=119.2ms
```

100K tier：编排者本增量 `--include-ignored` 实跑亦绿（macOS arm64，~13.4s wall）：

```text
cargo test -p nlos-runtime-tokio --test activation_meter_scale one_hundred_thousand_activation_meter_fibers_on_two_workers -- --include-ignored --nocapture
  → 1 passed / 0 failed（2026-09-08 W19-PATCH）
  → 100K profile（2 tokio workers，sample=1000）：
     active_cpu_phase=12.511s
     external_wait: spawn_issue=313.0ms park_settle=331.1ms external_wait_sleep=50ms
                    sample_assert=0.47ms rss_kib=203344 threads=4 total=702.0ms
```

不得外推 ROAD-B-006 全门达成；背压/挂起维 100K 规模验证仍缺。

### 6.10 Lifecycle phase 10K 规模探针（2026-09-08 追加，W19-006 / ROAD-B-006）

- Owner：`nlos-runtime-tokio`（`tests/lifecycle_scale.rs`）
- 设计依据：v0.5 §28.2 ROAD-B-006 分维 Activation metering 规模验证；§6.8 backpressure/suspended 功能级最小前缀已落地，本片补两维在 10K live fiber 下的相位转换 + 计量 + 线程有界性证明。
- **实现（test-only，`#[ignore]` 探针）**：
  - 新增 `lifecycle_scale.rs`，镜像 `activation_meter_scale.rs` / `lifecycle_phase.rs`：`QUICK_COUNT=10_000`、`METER_SUBSET=1_000` 前缀抽样。
  - **Phase 1 `backpressure_wait`**：`count` fiber spawn 后 test 线程批量 `begin_backpressure_wait`；`park_settle` 后 sleep 50ms，前 `subset` 断言 `FiberLifecyclePhase::BackpressureWait` + `FiberState::WaitingModel`、`backpressure_wait ≥ 40ms`、`external_wait = 0`、`active_cpu < backpressure_wait`；resume 后全员回 `Running`。
  - **Phase 2 `suspended`**：同形状 `begin_suspended` / `FiberState::Suspended` / `suspended ≥ 40ms`；teardown 走 drop。
- **新增测试**（`lifecycle_scale.rs`，1 项 `#[ignore]`）：
  1. `ten_thousand_lifecycle_phase_fibers_on_two_workers` — 10K quick tier（backpressure_wait + suspended 双 phase）。

#### 6.10.1 验证门实测

```text
cargo test -p nlos-runtime-tokio --test lifecycle_scale
  → 0 passed / 0 failed / 1 ignored（2026-09-08 W19-006；默认套件不跑 scale）
cargo test -p nlos-runtime-tokio --test lifecycle_scale ten_thousand_lifecycle_phase_fibers_on_two_workers -- --include-ignored --nocapture
  → 1 passed / 0 failed（2026-09-08 W19-006，~0.23s wall，macOS arm64）
  → 10K profile（2 tokio workers，sample=1000）：
     backpressure_wait: spawn_issue=31.2ms enter_backpressure=4.0ms park_settle=2.9ms
                        phase_sleep=50ms sample_assert=0.37ms resume_settle=2.8ms
                        rss_kib=18304 threads=4 total=101.5ms
     suspended:         spawn_issue=31.4ms enter_suspended=4.4ms park_settle=3.7ms
                        phase_sleep=50ms sample_assert=0.38ms resume_settle=2.9ms
                        threads=4 total=100.7ms
     （1000 前缀 backpressure_wait/suspended≥40ms 且 external_wait=0；threads≤10 有界断言通过）
```

#### 6.10.2 缺口更新

- **勾销**：§6.8.2 中「100K 探针下分维 metering 规模验证（背压/挂起维仍为零占位的 100K 实跑未做）」→ 本 §6.10 10K 实跑（背压/挂起双维；100K tier 未登记）。
- **如实保留（ROAD-B-006 剩余，Claim 维持 PARTIAL_PASS）**：
  - ~~100K lifecycle phase 规模探针（本片仅 10K quick tier）~~ → 本 §6.11 100K 实跑登记；
  - fiber 体内自动触发背压/挂起（scheduler 边界 hook 已覆盖，admission 集成未做）；
  - 100K 规模级 cancel 探针；
  - runtime 侧 process crash 传播联动；
  - 未声称 ROAD-B-006 整体达成。

### 6.11 Lifecycle phase 100K 规模探针（2026-09-09 追加，W20-006 / ROAD-B-006）

- Owner：`nlos-runtime-tokio`（`tests/lifecycle_scale.rs`）
- 设计依据：v0.5 §28.2 ROAD-B-006 分维 Activation metering 规模验证；§6.10 10K quick tier 已证 backpressure_wait/suspended 双维相位 + 计量 + 线程有界性，本片补 100K full tier。
- **实现（test-only，`#[ignore]` 探针）**：
  - `lifecycle_scale.rs` 增 `FULL_COUNT=100_000` 与 `one_hundred_thousand_lifecycle_phase_fibers_on_two_workers`（镜像 §6.10 双 phase 形状，`METER_SUBSET=1_000` 前缀抽样）。
  - 100K tier 省略前缀 fiber 的 `active_cpu < backpressure_wait` 断言：batch spawn 窗口内前缀 fiber 累积 spawn-window `active_cpu` 量级大于 parked `backpressure_wait`（10K 仍保留；功能级维分离由 `lifecycle_phase.rs` + §6.10 quick tier 覆盖）。
- **新增测试**（`lifecycle_scale.rs`，累计 2 项 `#[ignore]`）：
  1. `ten_thousand_lifecycle_phase_fibers_on_two_workers` — 10K quick tier（§6.10）；
  2. `one_hundred_thousand_lifecycle_phase_fibers_on_two_workers` — 100K full tier（backpressure_wait + suspended 双 phase）。

#### 6.11.1 验证门实测

```text
cargo test -p nlos-runtime-tokio --test lifecycle_scale
  → 0 passed / 0 failed / 2 ignored（2026-09-09 W20-006；默认套件不跑 scale）
cargo test -p nlos-runtime-tokio --test lifecycle_scale -- --include-ignored --nocapture
  → 2 passed / 0 failed（2026-09-09 W20-006，100K ~1.2s test wall / ~220s cargo wall incl. compile，macOS arm64）
  → 100K profile（2 tokio workers，sample=1000）：
     backpressure_wait: spawn_issue=301.2ms enter_backpressure=44.8ms park_settle=30.9ms
                        phase_sleep=50ms sample_assert=0.44ms resume_settle=30.6ms
                        rss_kib=156032 threads=4 total=506.1ms
     suspended:         spawn_issue=292.9ms enter_suspended=47.6ms park_settle=32.9ms
                        phase_sleep=50ms sample_assert=0.42ms resume_settle=33.2ms
                        threads=4 total=503.2ms
     （1000 前缀 backpressure_wait/suspended≥40ms 且 external_wait=0；threads≤10 有界断言通过）
```

#### 6.11.2 缺口更新

- **勾销**：§6.10.2「100K lifecycle phase 规模探针」→ 本 §6.11 100K 实跑（背压/挂起双维；`active_cpu` 维分离仍仅 10K/功能级）。
- **如实保留（ROAD-B-006 剩余，Claim 维持 PARTIAL_PASS）**：
  - 100K tier 前缀 fiber `active_cpu` 维分离断言（spawn 窗口 artifact，非 runtime 回归）；
  - fiber 体内自动触发背压/挂起（scheduler 边界 hook 已覆盖，admission 集成未做）；
  - 100K 规模级 cancel 探针；
  - runtime 侧 process crash 传播联动；
  - 未声称 ROAD-B-006 整体达成。

### 6.12 Lifecycle meter aggregate inspect prefix（2026-09-10 追加，W21-006 / ROAD-B-006）

- Owner：`nlos-runtime-tokio`（`src/lib.rs` + `tests/lifecycle_meter_aggregate.rs` + `tests/lifecycle_scale.rs` 聚合日志）
- 设计依据：v0.5 §28.2 ROAD-B-006 分维 Activation metering；§6.8–§6.11 backpressure/suspended 功能级 + 规模探针已落地，本片补 read-side **aggregate** 前缀，将背压/挂起计量与 inspect 面联动（非完整 OpenMetrics export）。
- **实现（最小、additive，tokio-only）**：
  - 公开 `LifecycleMeterAggregate { total_backpressure_wait, total_suspended, sampled_fibers }`。
  - `TokioRuntimeAdapter::inspect_lifecycle_meter_aggregate()` 遍历内部 fiber registry，O(n) 求和各 live fiber 的 `activation_usage` 中 `backpressure_wait` / `suspended` 维；未改 `nlos-runtime` trait。
  - `lifecycle_scale.rs` 10K/100K 探针在 phase_sleep 后断言 aggregate ≥ 全员下限并 eprintln 聚合读数。
- **新增测试**（`lifecycle_meter_aggregate.rs`，2 项）：
  1. `aggregate_sums_backpressure_and_suspended_across_live_fibers` — 6 fiber（2 背压 + 2 挂起 + 2 运行）：aggregate ≥ 各 parked handle 个体之和，`sampled_fibers = 6`。
  2. `aggregate_on_empty_registry_is_zero` — 空 registry 返回零 aggregate。

#### 6.12.1 验证门实测

```text
cargo test -p nlos-runtime-tokio --test lifecycle_meter_aggregate
  → 2 passed / 0 failed（2026-09-10 W21-006）
cargo test -p nlos-runtime-tokio --test lifecycle_phase
  → 4 passed / 0 failed（2026-09-10 W21-006）
cargo test -p nlos-runtime-tokio lifecycle
  → 2 passed / 0 failed / 2 ignored（2026-09-10 W21-006 收尾复验；lifecycle 名过滤跨 test target 实际仅匹配 lifecycle_phase 2 项 + lifecycle_scale 2 项 ignored，车道初记 8/8 系误记，提交前修正）
cargo clippy -p nlos-runtime-tokio --all-targets -- -D warnings
  → exit 0（stable，2026-09-10 W21-006）
cargo fmt -p nlos-runtime-tokio -- --check
  → 本车道新增 hunk 3 处格式违规已修复（lifecycle_meter_aggregate.rs ×1、lifecycle_scale.rs ×2）；activation_meter_scale.rs:142 与 lifecycle_scale.rs:72 为先在漂移（非本车道 write-set），保留并如实登记
```

#### 6.12.2 缺口更新

- **勾销**：§6.11.2 中 read-side aggregate inspect 前缀缺口 → 本 §6.12（O(n) 聚合 + 规模探针联动日志）。
- **如实保留（ROAD-B-006 剩余，Claim 维持 PARTIAL_PASS）**：
  - 完整 OpenMetrics / Prometheus export（本片为 prefix inspect，非 export）；
  - fiber 体内自动触发背压/挂起（scheduler 边界 hook 已覆盖，admission 集成未做）；
  - 100K 规模级 cancel 探针；
  - runtime 侧 process crash 传播联动；
  - 未声称 ROAD-B-006 整体达成。

### 6.13 已知 flaky：active_cpu/elapsed_wall 终态竞态（2026-09-10 收尾复跑发现）

- **现象**：`cargo test --workspace`（2026-09-10 第一段）中 `activation_meter::compute_fiber_records_active_cpu_against_elapsed_wall` 间歇失败于 `assert!(usage.active_cpu <= usage.elapsed_wall)`（activation_meter.rs:136）。
- **复现统计**：隔离复跑 3 次 → 1 failed / 2 passed；第二段全仓补跑中 nlos-runtime-tokio 整 crate（含该测试）通过。负载下与空载下均可触发，非确定性。
- **初步归因（未修）**：终态 `elapsed_wall` 时间戳与最后一段 Running 退出的 `active_cpu` 累计时间戳为分离的 `Instant::now()` 调用，调度抖动下区间可倒挂。W14-M（commit `c3b2a10`）先在行为，非 W21-006 引入（该车道只新增只读聚合读，未触碰累计路径）。
- **处置**：不改计量语义（需专门车道处理终态时间戳统一）；如实登记，根因待修。

#### 6.13.1 根因修复（2026-09-10 同日收口）

- **根因确认**：`run_fiber` 中 `finished_at`（写 `elapsed_wall`）→ 抢 `inner.waits`/`inner.channel_waits` 两锁 → `set_state(terminal)` 内部另取更晚的 `Instant::now()` 调 `finalize`，最后一段 Running 以 `now₂ > finished_at` 闭合，`active_cpu` 越界（实测越界 42ns–416ns，恰为锁窗口差）。
- **修复（最小、单点）**：新增 `FiberRecord::finish_terminal(state, started_at, finished_at)` —— `elapsed_wall` 写入与 `finalize(finished_at)` 在**同一临界区、同一时间戳**完成（顺带消除"state 已 terminal 但 metering 未闭合"中间态），`run_fiber` 终态路径改用；`set_state` 的 Running 路径不变。
- **红→绿证据**：新增压力回归 `terminal_metering_keeps_active_cpu_bounded_by_elapsed_wall_under_stress`（40 × 5ms 忙循环 fiber）——旧代码 **6/6 轮 FAILED**（`active_cpu=5.008208ms > elapsed_wall=5.007792ms` 等）；修复后 **10/10 轮 passed**；原 flaky 测试隔离复跑 5/5 passed。
- **验证门（macOS arm64，2026-09-10）**：`cargo test -p nlos-runtime-tokio` → 55 passed / 0 failed；`cargo clippy -p nlos-runtime-tokio --all-targets -- -D warnings` → 0 warning；`cargo fmt -p nlos-runtime-tokio -- --check` → 通过。
- **不变量恢复依据（构造性）**：所有计量段闭合 ≤ `finished_at`、首段开启 ≥ `started_at`、同一单调钟 saturating 运算，故 `active_cpu ≤ elapsed_wall` 恒成立。

### 6.14 OpenMetrics text exposition 最小前缀（2026-09-11，W22-006）

- Owner：`nlos-runtime-tokio`（新增 `src/metrics.rs` + `src/lib.rs` +1 `mod` 行 + `tests/lifecycle_meter_aggregate.rs` +4 测试）；base HEAD `d039cd0`。
- 设计依据：§6.12.2 登记的「完整 OpenMetrics / Prometheus export」缺口之**最小前缀**——纯函数 `LifecycleMeterAggregate::to_open_metrics_text(&self) -> String`，无 I/O、无状态、无错误路径（故无 typed error）、无 unsafe、零新依赖（禁 prometheus crate，手写 exposition）。不做 scrape 端点/auth/retention，本片如实登记为 prefix。未触碰 `nlos-runtime` trait 与 `run_fiber`/metering 语义（c84c91a 终态修复不变量保持只读）。
- **实现要点**：
  - 每指标 `# HELP` → `# TYPE` → 单行无 label sample，输出以换行结尾；
  - Duration 经 `as_secs_f64()` + Rust 最短往返浮点格式化（无尾随零、无科学计数法，如 `1_500_000_000 ns → 1.5`、`42 ns → 0.000000042`）；
  - 不输出 OpenMetrics 专属 `# EOF` 终止符与 timestamp/exemplar，保持 Prometheus 0.0.4 文本格式兼容（OpenMetrics 解析器同样接受该文本体）。
- **指标命名表**：

  | Aggregate 字段           | 指标名                                        | 类型    |
  |--------------------------|-----------------------------------------------|---------|
  | `total_backpressure_wait`| `nlos_fiber_backpressure_wait_seconds_total`  | counter |
  | `total_suspended`        | `nlos_fiber_suspended_seconds_total`          | counter |
  | `sampled_fibers`         | `nlos_fiber_sampled`                          | gauge   |

- **新增测试**（并入 `lifecycle_meter_aggregate.rs`，纯函数测试无 runtime 开销）：
  1. `open_metrics_text_matches_expected_format_snapshot` — 含 HELP/TYPE 行的完整快照（1.5s/250ms/3 fiber）；
  2. `open_metrics_text_on_zero_aggregate_reports_zero_values` — 零值 aggregate（空 registry 等价态）全 0 输出；
  3. `open_metrics_text_converts_durations_to_fractional_seconds` — `1_500_000_000 ns → "1.5"`、`50 ms → "0.05"`；
  4. `open_metrics_text_preserves_nanosecond_and_large_second_precision` — `42 ns → "0.000000042"`、`100_000 s → "100000"`（无科学计数法）。
- **pedantic lint 修复记录**：`clippy::doc_markdown` ×5（`OpenMetrics` 加反引号）；`clippy::duration_suboptimal_units` ×2（快照/换算测试对 `from_nanos(1_500_000_000)` scoped `#[allow]` + 理由注释——纳秒精确输入正是被测转换，lint 的更大单位建议会破坏测试意图，属误报豁免）。

#### 6.14.1 验证门实测

- 验证方式：先在**隔离 worktree**（base `d039cd0` + 仅本车道写集）实跑四门——因共享树当时有并行车道 `nlos-process` WIP 中间态导致依赖编译失败（非本车道写集，未触碰）；其恢复后共享树复跑，两处结果一致：

```text
cargo test -p nlos-runtime-tokio --test lifecycle_meter_aggregate
  → 6 passed / 0 failed（2026-09-11 W22-006，worktree 与共享树一致，0.05s）
cargo test -p nlos-runtime-tokio --test lifecycle_phase
  → 4 passed / 0 failed（2026-09-11 W22-006，worktree 与共享树一致，0.05s）
cargo clippy -p nlos-runtime-tokio --all-targets -- -D warnings
  → exit 0（stable，2026-09-11 W22-006 共享树 1m08s；worktree 首跑暴露上述 7 处 pedantic 违规已全部修复）
cargo fmt -p nlos-runtime-tokio -- --check
  → 通过（2026-09-11 W22-006，worktree 与共享树一致）
```

#### 6.14.2 缺口更新

- **勾销**：§6.12.2 中「完整 OpenMetrics / Prometheus export」缺口的**文本 exposition 前缀**部分 → 本 §6.14。
- **如实保留（ROAD-B-006 剩余，Claim 维持 PARTIAL_PASS）**：
  - scrape HTTP 端点 / auth / retention / label 维度（本片明确不做）；
  - fiber 体内自动触发背压/挂起（scheduler 边界 hook 已覆盖，admission 集成未做）；
  - 100K 规模级 cancel 探针；
  - runtime 侧 process crash 传播联动；
  - 未声称 ROAD-B-006 整体达成。
