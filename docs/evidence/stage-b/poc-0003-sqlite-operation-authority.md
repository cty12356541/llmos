# PoC-0003：SQLite Operation Authority 初始证据

> 状态：PARTIAL PASS
>
> 日期：2026-07-29
>
> 对应：`MODEL-OP-001`、`IO-ASYNC-001`、`IO-CANCEL-001`、`DUR-ACK-001`、`DUR-RECOVER-001`

## 实现

新增 `nlos-store`：

- SQLite WAL；
- `synchronous=FULL`；
- 进程内单写者 admission + `BEGIN IMMEDIATE`；
- schema v1、`STRICT` tables、固定宽度 ID/epoch 编码；
- Operation revision CAS；
- exact `OperationSpec` registration 幂等；
- dispatched CallbackId/CancelEpoch durable binding；
- Operation terminal state + Receipt + Outbox 同事务；
- `WakeFiber` 与 `ReconcileEffect` 分流；
- outbox at-least-once delivery + 幂等 ACK；
- 未知 schema version fail-closed。

`nlos-operation` 同时抽取出可恢复的 `OperationMachine`，使内存 Registry 与 SQLite authority 共用同一状态转换语义。dispatch 后的票据替换现在会被拒绝。

## 事务边界

```text
BEGIN IMMEDIATE
  load + invariant validation
  apply OperationMachine transition
  revision compare-and-swap
  append WakeFiber | ReconcileEffect
COMMIT
  → 才向调用者返回成功
```

Outbox ACK 是后续独立事务。Consumer 崩溃可造成重复投递，所以 wake/reconcile consumer 仍必须按 Operation/Fiber generation 幂等；但 Operation 提交与待投递事件不会出现一个成功、另一个缺失。

## 测试

复现：

```sh
cargo test -p nlos-store
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

当前覆盖：

1. 相同 OperationSpec registration 重放返回同一 handle；
2. 同 OperationId 的不同 spec 被拒绝；
3. completion、Receipt 和 Wake Outbox 在关闭并重开数据库后仍存在；
4. duplicate callback 不产生第二条 Outbox；
5. Outbox ACK 可幂等重放且重开后不再列出；
6. cancel 后迟到 callback durable 路由到 reconciliation；
7. dispatch 前 cancel 同事务产生 `CANCELLED_BEFORE_EFFECT` 和 wake；
8. forged CallbackId 整笔回滚，不改变 `DISPATCHED` 状态；
9. SQLite authority 上 cancel/completion 竞态重复 64 次，只出现两个合法串行化结果；
10. completion 已返回 durable ACK 后，测试子进程以 `exit(97)` 跳过析构退出，父进程重开仍恢复终态与未 ACK Outbox。

## 当前不能证明

- `exit(97)` 不是 kill -9、掉电或 torn sector；
- 尚未使用 SQLite fault-injection VFS 验证 commit 中断和写损坏；
- 尚未验证 disk-full、只读文件系统、I/O error 和 fail-closed 行为；
- 尚未测量 WAL checkpoint、长读事务、备份/恢复与 100K Operation metadata；
- schema v1 尚无 v2 migration/rollback golden database；
- 尚未在 Windows/Linux 和不同文件系统复验；
- ~~Outbox 尚未连接 Tokio Fiber wake/reconciliation consumer~~ **已由 [PoC-0004](./poc-0004-outbox-wake-consumer.md) 补齐（2026-08-01）**：consumer 经专用 OS 线程 pump 驱动，崩溃重放、幂等去重、stale generation fencing 与 backpressure 均有集成测试（`PARTIAL PASS`，单节点局部证据）；
- 尚无 Driver authentication、Capability、Reservation 或 EffectPermit；
- Receipt 仍只有 nominal ID，没有 durable Receipt body、签名和 provenance；
- 当前只证明单进程 SQLite authority，不证明跨节点 consensus 或分布式 exactly-once。

因此证据等级仍为单节点原型的局部 H3，不能声称完整 durable ledger、外部副作用 exactly-once 或阶段 B 已退出。
