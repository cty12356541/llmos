# B-RUNTIME-002：100K dormant/waiting Fiber 承载证明（ROAD-B-006 前片）

- 状态：`PARTIAL_PASS`（ROAD-B-006 的承载量 + 唤醒正确性 + 线程有界性前片；cancel/late-callback、structured join/detach、Process crash propagation、分维 Activation metering 未做，ROAD-B-006 整体不达成）
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
- **未做（ROAD-B-006 其余退出门，登记后续）**：cancel/late-callback 矩阵、structured join/detach、Process crash propagation、分维 Activation metering、真实阻塞 I/O（本片为 durable wait 挂起路径）；100K `cancel_scope` 收尾因 O(n²) 终态 purge 未纳入探针， teardown 走 drop。
- probe 为 `#[ignore]`，常规 CI（push/PR）不运行；夜间 scale-probe job 覆盖。
