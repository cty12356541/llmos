# PoC-0002：Operation Callback Fence 初始证据

> 状态：PARTIAL PASS
>
> 日期：2026-07-29
>
> 对应：`FIBER-AWAIT-001`、`IO-ASYNC-001`、`IO-CANCEL-001`

## 实现

新增 `nlos-operation`：

- `OperationId + Generation`；
- `CallbackId`；
- `CancelEpoch`；
- Operation stable handle；
- dispatch-time `CallbackTicket`；
- `REGISTERED → DISPATCHED → terminal`；
- `DISPATCHED → CANCEL_REQUESTED → terminal`；
- `REGISTERED → CANCELLED_BEFORE_EFFECT`；
- duplicate callback 幂等；
- callback ID substitution 检测；
- stale Operation/Fiber generation 拒绝；
- cancel/completion 竞态线性化。

## 最重要的语义

取消后的旧 callback 被分成两个结果：

```text
旧 cancel epoch
  → 禁止唤醒原 Fiber
  → 但 final effect Receipt 仍进入 canonical reconciliation
```

如果完全丢弃迟到 callback，系统可能把已经发生的外部副作用错误记为“未发生”。因此 `CompletionDecision` 区分：

- `CanonicalizedAndWake`
- `CanonicalizedForReconciliation`
- `Duplicate`

## 测试

```sh
cargo test -p nlos-operation
cargo clippy -p nlos-operation --all-targets -- -D warnings
```

8 项集成测试通过，覆盖以下 9 个断言：

1. 当前 callback canonicalize 后允许 wake；
2. duplicate callback 幂等返回原终态；
3. cancel epoch 前进后迟到 callback 只进入 reconciliation；
4. dispatch 前 cancel 形成 `CANCELLED_BEFORE_EFFECT` 并阻止 dispatch；
5. stale Operation/Fiber generation 被拒绝；
6. callback ID 不得对应不同 outcome；
7. dispatch-time CallbackId/CancelEpoch 被绑定，不能在 dispatch 后替换 callback ticket；
8. durable restore 会拒绝不可能的 callback/state/cancel epoch 组合；
9. cancel/completion race 重复 256 次，只出现两个合法线性化结果。

最后一项中的两个合法结果：

```text
cancel 先线性化
  → CANCEL_REQUESTED
  → callback canonicalized for reconciliation

completion 先线性化
  → callback canonicalized and may wake
  → cancel 被拒绝为 terminal InvalidState
```

## 当前不能证明

- 内存 Registry 本身仍不持久化；独立的 [PoC-0003 SQLite authority](./poc-0003-sqlite-operation-authority.md) 已验证初始 durable adapter，但尚未达到完整 fault-injection 验收；
- ~~尚未与 Tokio Fiber wake channel 集成~~ **已由 [PoC-0004](./poc-0004-outbox-wake-consumer.md) 补齐（2026-08-01）**：当前/迟到/cancel-before-dispatch callback 经 durable Outbox 正确路由到 Tokio Fiber wake 或 reconciliation（`PARTIAL PASS`，单节点局部证据）；
- Receipt 目前只有 nominal ID，尚未验证签名和内容；
- 尚无 Driver callback authentication；
- 尚无 progress/stream callback sequence；
- 尚无 deadline、Reservation、EffectPermit；
- terminal record retention/GC 未实现；
- host-loss takeover 和跨进程 callback 未实现。

因此该 PoC 只证明内存内机械 fencing 状态机，不能声称外部副作用已经具备 durable exactly-once。
