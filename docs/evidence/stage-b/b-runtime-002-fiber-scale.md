# B-RUNTIME-002：100K dormant/waiting Fiber 承载证明（ROAD-B-006 前片）

- 状态：`PARTIAL_PASS`（ROAD-B-006 的承载量 + 唤醒正确性 + 线程有界性前片 + cancel/late-callback 功能矩阵（§6，2026-09-02 追加）；structured join/detach、Process crash propagation、分维 Activation metering、阻塞 I/O 负向证明未做，ROAD-B-006 整体不达成）
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
- **未做（ROAD-B-006 其余退出门，登记后续）**：~~cancel/late-callback 矩阵~~（功能级矩阵已落地，见 §6；100K 规模级 cancel 探针仍因 O(n²) 终态 purge 未纳入）、structured join/detach（API 不存在，如实登记缺口，见 §6.4）、Process crash propagation、分维 Activation metering、真实阻塞 I/O（本片为 durable wait 挂起路径）；100K `cancel_scope` 收尾因 O(n²) 终态 purge 未纳入探针， teardown 走 drop。
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
- **如实保留（ROAD-B-006 剩余，整体不达成）**：阻塞 I/O 负向证明（不线性占用宿主线程的负向证据）、Process crash propagation、分维 Activation metering。

### 6.4 structured join/detach：API 缺口登记（不发明）

勘察结论：`nlos_runtime::RuntimeAdapter` trait 表面为 `spawn_fiber` / `cancel_scope` / `inspect` / `activation_usage`，`TokioRuntimeAdapter` 及全仓（grep `join|detach` 于 runtime 合同两侧）**不存在任何 structured join/detach API**。按本车道纪律不发明 API、不用 `tokio::JoinHandle` 冒充（adapter 内部 spawn 的 task handle 未外泄，语义上即隐式 detach）。缺口维持登记：structured join/detach 属 ROAD-B-006 退出门的**合同层缺口**（需先在 nlos-runtime 合同层定义），非测试面缺口；本次仅能证明 detach 是当前唯一隐式语义。
