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

1. `B-STORE-FAULT`（SQLite fault-injection）：kill -9、torn-write VFS、disk-full、checkpoint/backup、100K metadata——与 ADR-0002 PoC 验收第 7 条对齐；通过后本 PoC 方可考虑晋升；
2. durable wait registry 与 fiber rehydration（`B-PROCESS`/Slice K）：使 crash-restart 场景的重投 wake 能送达重建 fiber 而非 `FiberGone`；
3. reconcile sink 接入真实 Task/Artifact 权威并验证 Driver authentication/EffectPermit；
4. 跨平台复验与真实速率 backpressure 计量。
