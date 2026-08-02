# B-SCHEMA-012：deadline/cancel durable 服务端状态机初始证据

> 状态：PARTIAL PASS（本地 Rust↔TypeScript/Python 真实 IPC 通过；远程三平台待验证）
>
> 日期：2026-08-02
>
> 对应：`MODEL-TIME-001`、`MODEL-OP-001`、`FIBER-AWAIT-001`、`KABI-ERR-001`、`SABI-COMMON-001`、`IO-CANCEL-001`

## 1. 本切片完成的边界

`nlos-directory-chain` 的业务入口不再只检查 deadline 字段形状或返回受控 `E_UNCERTAIN`，而是把四个 effect 窗口接入现有 SQLite Operation authority：

```text
admission validates host-monotonic deadline
  → atomic idempotency claim + Operation REGISTERED
  → queue/dispatch fence
      ├─ deadline/cancel before dispatch
      │    → CANCELLED_BEFORE_EFFECT + Receipt + stable result + Wake Outbox
      └─ dispatch callback ticket
           → deadline/cancel advances cancel epoch
           → late callback becomes PARTIAL_EFFECT or EFFECT_UNKNOWN
           → ReconcileEffect Outbox; cancelled Fiber is not woken
```

新增 `SqliteOperationStore::cancel_idempotent_before_dispatch`，在同一个 `BEGIN IMMEDIATE` 事务中提交：

- `REGISTERED → CANCELLED_BEFORE_EFFECT`；
- no-effect Receipt identity；
- transport-independent stable result；
- `WakeFiber` Outbox。

该 API 在 Operation 已经 dispatch 后拒绝执行，不能把已消费 dispatch ticket 的操作伪装成 no-effect。相同 durable result 可在数据库重开后精确回放；不同 result bytes 冲突。

## 2. 四个真实 SABI 场景

TypeScript 与 Python 客户端通过 ServiceDirectory 协商后的真实 Unix socket/Windows named-pipe business endpoint 发起以下调用：

| 窗口 | Durable Operation 结果 | SABI 结果 | retry |
|---|---|---|---|
| deadline 在 dispatch 前到期 | `CANCELLED_BEFORE_EFFECT` | `E_DEADLINE` + Operation + no-effect Receipt | `DO_NOT_RETRY` |
| cancel 在 dispatch 前生效 | `CANCELLED_BEFORE_EFFECT` | `E_CANCELLED` + Operation + no-effect Receipt | `DO_NOT_RETRY` |
| cancel 在 dispatch 后到达，callback 证明部分 effect | `PARTIAL_EFFECT` | `E_PARTIAL` + Operation + Receipt | `DO_NOT_RETRY` |
| deadline 在 dispatch 后到期，effect 无法确认 | `EFFECT_UNKNOWN` | `E_EFFECT_UNKNOWN` + Operation + Receipt | `QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY` |

deadline-before-dispatch 还以相同 IdempotencyKey、相同 payload、新 exchange request ID/correlation 重试，返回原 Operation/Receipt 与新的 transport metadata，证明 no-effect 终态同样进入 durable result authority。

fixture 结束前检查全部 pending Outbox：总数精确为 5，其中 3 个 `WakeFiber`、2 个 `ReconcileEffect`；两个 dispatch 前场景是 `CancelledBeforeEffect`，两个 dispatch 后迟到 callback 分别是 `PartialEffect` 与 `EffectUnknown`。这避免只验证错误码、却没有证明 cancel epoch 改变了唤醒路由。

## 3. 本地验证

以下命令通过：

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
npm run schema:typecheck
npm run directory:test:typescript
python tests/conformance/ipc/directory_chain.py
```

新增 store 测试还验证 no-effect result 的重开回放、精确重复、结果冲突、单一 Wake Outbox，以及 terminal 后禁止 dispatch。

## 4. 当前不能证明什么

- 当前由 conformance fixture 的确定性同宿主 monotonic 检查点触发 deadline，不是生产 timer wheel、scheduler queue 或真实慢 worker；
- cancel 场景进入真实 durable Operation 状态机，但触发器仍是 fixture method；尚无独立、版本化、幂等的 Operation query/cancel SABI payload；
- 尚未把 TaskExecutionBinding.cancel_epoch、Process supervisor、Driver cancel acknowledgement 和真实 provider callback 串成端到端传播链；
- `E_PARTIAL`/`E_EFFECT_UNKNOWN` Receipt 仍只有 nominal ID，没有 canonical body、effect summary、签名或 attestation；
- 尚未覆盖 cancel/complete/deadline 三方并发的跨进程 IPC fault matrix、异常 server crash 或 authority retention/GC；
- 本证据只证明单节点受控服务入口，不构成跨节点 exactly-once 或远程 clock-domain 保证。

因此本 Evidence 记为 `PARTIAL PASS`。下一验收门是远程三平台复验，并设计/实现独立 Operation query/cancel payload 与 timer-driven async worker，使取消不依赖 fixture method。
