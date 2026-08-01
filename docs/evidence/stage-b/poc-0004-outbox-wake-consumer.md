# PoC-0004：Durable Outbox → Tokio Fiber Wake Consumer 集成证据

> 状态：PARTIAL PASS
>
> 日期：2026-08-01
>
> 对应：`FIBER-AWAIT-001`、`IO-ASYNC-001`、`IO-CANCEL-001`、`DUR-ACK-001`、`DUR-RECOVER-001`

## 1. 实现范围

本次提交打通了 ADR-0002 描述的持久化 Outbox 闭环的最后一段：从 `SQLite` authority 的 durable commit 到 Tokio Fiber 的唤醒与迟到副作用 reconciliation。

- `nlos-runtime`：`WakeSink`/`WakeOutcome` 契约（`Delivered`/`FiberGone`/`NotWaiting`，generation fencing、按 `(fiber, operation)` 幂等、非阻塞 handoff）；
- `nlos-outbox`：同步 `OutboxConsumer`——有界批量、严格按 durable sequence 顺序、apply 成功才 ACK、瞬时失败停批留待重投、永久终态（`FiberGone`/`NotWaiting`）照 ACK 防毒化；
- `nlos-runtime-tokio`：`TokioWakeSink`（与 adapter 共享 fiber/wait 注册表）、`wait_for_operation`（`Woken`/`Cancelled`，early-wake 按键缓冲、fiber 终态与 wait purge 同一临界区）、`shutdown`；
- `nlos-runtime-tokio::pump` glue（本提交新增）：
  - `StoreOutboxSource`：`OutboxSource` → `SqliteOperationStore::pending_outbox`/`acknowledge_outbox` 的桥接（i64↔u64 sequence、`OperationHandle` 拆分、kind 0/1 映射）；
  - `OutboxPump`：在**专用 OS 线程**（非 Tokio worker，遵守 ADR-0001 的 blocking 隔离约束）循环驱动 `drain_once`；唤醒=容量 1 的 `SyncSender::try_send` 有界 hint（满则丢）+ 25ms 兜底轮询；writer 在 commit 返回后调用 `hint()`，writer 路径永不因 consumer 阻塞；`stop()` 信号并 join；
  - `RecordingReconcileSink`：按 `(operation, operation generation, callback)` 幂等去重并记录，供测试与未来集成复用。

## 2. 环境

```text
hardware architecture: arm64 / Apple Silicon
OS: macOS 26.5.2 (Build 25F84)
rustc: 1.97.1 (8bab26f4f 2026-07-14)
cargo: 1.97.1
tokio: 1.53.1
rusqlite: 0.40.1 (bundled SQLite)
runtime worker threads: 2（各集成测试）
build: dev profile（cargo test 默认）
```

## 3. 事务边界与投递链路

```text
writer（任意线程）
  BEGIN IMMEDIATE
    Operation 终态 + Receipt + WakeFiber|ReconcileEffect outbox 行
  COMMIT
  → complete()/request_cancel() 返回成功      ← 此前不存在任何 fiber wake
  → pump.hint()（有界、非阻塞，可丢）

pump 专用 OS 线程
  drain_once:
    pending_outbox(batch_limit)   短读事务，锁内只读
    逐条 apply（锁外）:
      WakeFiber       → TokioWakeSink.wake(generation fence + 幂等 handoff)
      ReconcileEffect → ReconcileSink.reconcile（按 (operation, callback) 幂等）
    acknowledge_outbox(sequence)  短 ACK 事务
  队列空 → 等 hint 或 25ms 兜底轮询

fiber（Tokio worker）
  wait_for_operation(handle, op, gen) → Woken | Cancelled
```

崩溃语义：apply 成功、ACK 前崩溃 → 重启后未 ACK 条目按 durable sequence 重投，由 sink 幂等吸收重复；已提交条目不会永久丢失。

## 4. 测试与复现

复现：

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

本工作包新增 `crates/nlos-runtime-tokio/tests/outbox.rs`（临时 `SQLite` 库 + 真实 store + 真实 pump + 真实 `TokioWakeSink`），8 类端到端场景全部通过（复跑 3 次稳定）：

1. **current callback wake**：register→dispatch→complete（当前 ticket）→commit 返回后 fiber 的 `wait_for_operation` resolve 为 `Woken`；
2. **late callback reconciliation**：dispatch→request_cancel→迟到 complete（旧 ticket）→`ReconcileEffect` 进入 reconcile sink，fiber 的 wait 在 150ms 探针内保持 pending，不被该条目唤醒；
3. **cancel-before-dispatch**：register→request_cancel→同事务 `CancelledBeforeEffect`+`WakeFiber`（`callback_id=None`）→fiber `Woken`；
4. **duplicate delivery**：source 装饰器使每条目的首个 ACK 失败强制重投 → wake apply 2 次但逻辑唤醒恰 1 次、reconcile 原始调用 2 次但记录恰 1 条（幂等去重生效）；
5. **stale generation**：Outbox 条目指向同 fiber id 的非 live generation → `FiberGone` 分类且被 ACK，live generation 的 wait 保持 pending；
6. **consumer crash/restart**：沿用 `crash_writer_helper` 范式——子进程 apply 成功后、ACK 前 `exit(97)`；父进程重开库 + 全新 runtime 重跑 pump → 未 ACK 条目重投、无丢失；因 runtime 重启后 fiber record 不存在（且无 fiber rehydration），重投 wake 分类为 `FiberGone` 并 ACK，同 fiber id 新 generation 的 fiber 不被误唤醒；
7. **backpressure**：`batch_limit=4`、10 条目、50ms 慢 sink → writer 的 5 笔 `complete` 在 consumer 仍在排空积压时全部及时返回（<300ms，且 writer join 时 outbox 仍有未 ACK 条目），分批处理且每批 ≤4，最终 10 条全部投递+ACK；
8. **顺序保证**：commit 返回前 150ms 探针确认无 wake；共享原子序号证明 wake 观察严格发生在 `complete()` 返回之后。

同时回归：`nlos-outbox` 11 项 consumer 单测、`nlos-runtime-tokio` 9 项 wake 测试、`nlos-store` 8 项 authority 测试及 workspace 其余测试全部通过；clippy `-D warnings` 与 rustfmt 通过。100K 规模 ignore 项未运行（不属于本工作包）。

## 5. 当前能证明什么

在上述单节点、单进程、dev profile 环境下：

- commit 成功前不发生 fiber wake（验收门 §5-1）；
- consumer 崩溃后 Outbox 可重放：不丢失、不产生第二次逻辑唤醒/reconciliation（§5-2、§5-3）；
- 有界批量 + 慢 consumer 不阻塞 authority writer（§5-4）；
- current/late/cancel-before-dispatch/crash-restart 等场景按 ADR-0002 语义路由（§5-5）；
- PoC-0001/0002/0003 各自登记的"Tokio wake consumer 集成"缺口已由本集成补齐（§5-6 的 Evidence 同步部分；故障注入除外）。

## 6. 当前不能证明什么

- **durable wait registry / fiber rehydration**：runtime 重启后 fiber record 不恢复，重投 wake 只能分类为 `FiberGone` 并 ACK；让新 runtime 把重投 wake 真正送达重建的 fiber 属于 `B-PROCESS`/Slice K 范围；
- **kill -9 / torn-write / disk-full fault-injection**：`exit(97)` 不是掉电或写损坏；commit 中断、torn sector、disk-full、只读文件系统、I/O error 的 fail-closed 行为未验证（归属下一工作包 `B-STORE-FAULT`）；
- **WAL checkpoint/备份恢复、长读事务、100K Operation metadata** 规模行为；
- **Driver authentication / EffectPermit / progress callback**：reconcile sink 只记录条目，不验证副作用来源身份或许可；
- **跨平台**：仅 Apple Silicon/macOS；Linux/Windows 未复验；
- **真实规模 backpressure 计量**：慢 sink 是人工 50ms 延迟，未测量生产速率下的排队深度、延迟分布和 writer 吞吐；
- **分布式/跨节点**：只证明单进程单节点 at-least-once + 幂等，不证明跨节点 exactly-once。

因此本 Evidence 等级仍为局部单节点集成证据，状态保持 `PARTIAL PASS`；不得外推为阶段 B 退出、durable ledger 完成或外部副作用 exactly-once。

## 7. 下一验证门

1. `B-STORE-FAULT`（SQLite fault-injection）：kill -9、torn-write VFS、disk-full、checkpoint/backup、migration（v1→v2 前向迁移、备份/恢复演练、golden database）与长读事务、100K metadata——与 ADR-0002 PoC 验收第 7 条对齐；通过后本 PoC 方可考虑晋升；
2. durable wait registry 与 fiber rehydration（`B-PROCESS`/Slice K）：使 crash-restart 场景的重投 wake 能送达重建 fiber 而非 `FiberGone`；
3. reconcile sink 接入真实 Task/Artifact 权威并验证 Driver authentication/EffectPermit；
4. 跨平台复验与真实速率 backpressure 计量。

## 8. Remediation（2026-08-01）：评审后错误路径语义修复

对本 PoC 提交（`6894240`）的评审发现 1 个 MAJOR 与 3 个同族 MEDIUM 错误路径缺陷，均在同一 remediation 提交中修复并各有测试；本 PoC 状态保持 `PARTIAL PASS`（修复收窄"不能证明什么"的边界，不扩大证据范围）。

### 8.1 评审发现 → 修复对应

| 评审发现 | 修复 |
|---|---|
| MAJOR：pump_loop `Ok(_) \| Err(_) => break` 静默吞掉全部 drain 错误，持久 source 故障 = 25ms 无限重试零信号 | pump 区分 `Err(e)` 与正常停批：失败计入 `PumpHealth.consecutive_failures` 与 `last_error`（携带根因 `Display`），按 25ms 起 ×2、cap 1600ms 的有上限指数退避重试，成功 drain 归零复位；连续失败达阈值（默认 16）转 `Faulted` 并退出线程；`OutboxPump::health()` 可无锁热路径读取 |
| MEDIUM-1：`TokioWakeSink::wake` shutdown 后返回 `Err(ShuttingDown)` 被 consumer 当瞬时错误每 25ms 无限重投，outbox 队头阻塞静默积压 | `DrainReport` 新增 `shutdown: bool`：wake 返回 `ShuttingDown` 时置位并停批返回 `Ok`；pump 见到后转 `Stopped` 退出循环（可 join），未 ACK 条目留在库中（at-least-once 保留给未来 runtime，即 ADR-0002 语义）；`WakeSink` 契约文档（不改签名）声明 `ShuttingDown` 为终态而非瞬时错误 |
| MEDIUM-2：pump 线程无 panic 防护，drain_once panic → 线程静默死亡，投递永久停滞无信号 | `catch_unwind(AssertUnwindSafe(..))` 包裹每次 drain（无 `unsafe`）；panic 按 failure 同样计数/退避（`last_error = "consumer panicked"`），达阈值同样转 `Faulted` |
| MEDIUM-3：`wake()` 中 `sender.send(())` 失败（fiber 已 drop 等待句柄）被吞：wake 被消费+ACK，同键重等永久 pending | send 失败时在同键重新插入 `WaitEntry::Buffered`（与 early-wake 同路径），仍返回 `Delivered`；后续同键 `wait_for_operation` 立即 `Woken`；`wake()`/`WaitEntry` 文档已升级为契约级声明 |
| MINOR：`DrainReport::stopped_at` 文档与 ACK 失败路径矛盾 | 文档精确化：apply 失败停批 = 该条及之后未 apply 未 ack；ACK 失败停批 = 该条已 apply 未 ack、重放靠 sink 幂等吸收 |
| MINOR：`OutboxError.detail: &'static str` 丢失根因 | 改 `String`（去掉 `Copy`），pump 的 `map_err` 携带 store 错误 `Display` |
| MINOR：`ConsumerConfig.batch_limit = 0` 合法但导致 pump 永久空转 | `drain_once` 入口 `debug_assert!(batch_limit > 0)`，文档注明 0 为非法配置 |
| MINOR：`writer_elapsed < 300ms` 墙上时钟断言慢盘 CI 有 flake 风险 | 放宽为 1s 并注明仅为"writer 不被慢 sink 拖住"的粗粒度上界 |

### 8.2 新测试

1. `nlos-runtime-tokio/tests/pump.rs::failing_source_is_observed_through_health_and_backoff`：脚本化持续失败 source → `health()` 显示 failures>0、`last_error` 含根因、drain 尝试间隔随退避拉长（时间戳断言）；source 恢复后 failures 归零；
2. `nlos-runtime-tokio/tests/pump.rs::panicking_sink_faults_the_pump_without_killing_it`：panicking sink → 线程不死亡（`stop()` 1s 内 join）、health 记录 `consumer panicked`、达阈值转 `Faulted`；
3. `nlos-runtime-tokio/tests/outbox.rs::runtime_shutdown_stops_the_pump_without_draining_the_outbox`：`runtime.shutdown()` 后有未 ACK 条目 → pump 转 `Stopped`、条目保持 pending 不被无限重投、`stop()` 快速 join；
4. `nlos-runtime-tokio/tests/wake.rs::wake_to_dropped_receiver_is_rebuffered_for_the_next_registration`：注册 wait → drop → wake（`Delivered`）→ 同键重注册 → 立即 `Woken`；
5. `nlos-outbox/tests/drain.rs::shutting_down_wake_stops_batch_as_terminal_shutdown` 与 `zero_batch_limit_is_rejected_in_debug_builds`（`#[should_panic]`，debug 构建下验证 `debug_assert`；release 构建仅文档约束）。

既有测试无回归：`nlos-outbox` drain 13 项、`nlos-runtime-tokio` wake 10 项、outbox 10 项及 workspace 其余测试全绿；`cargo fmt --all -- --check` 与 `cargo clippy --workspace --all-targets -- -D warnings` 通过。

### 8.3 语义声明

- **pump 存活/健康语义**：pump 线程任何失败路径（source 错误、consumer panic）都计入 `PumpHealth` 并按有上限退避重试，不允许静默无限重试或静默死亡；`Faulted`/`Stopped` 均为可观察终态，`stop()` 在两态下都能快速 join。
- **shutdown 终态语义**：`DrainReport::shutdown = true` 是终态停批（区别于瞬时停批的 25ms 重试）；pump 见到后立即 `Stopped` 退出，未 ACK 条目保留在 durable Outbox，由未来 runtime 的 pump 按 at-least-once 重投。
