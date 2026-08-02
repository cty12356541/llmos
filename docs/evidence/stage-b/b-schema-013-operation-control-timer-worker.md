# B-SCHEMA-013：OperationControl 与异步 deadline worker 初始证据

> 状态：PARTIAL PASS（Rust↔TypeScript/Python 真实 IPC 本地通过；三平台 CI 待回执）
>
> 日期：2026-08-02
>
> 对应：`MODEL-TIME-001`、`MODEL-OP-001`、`FIBER-AWAIT-001`、`KABI-QUERY-001`、`KABI-UNCERTAIN-001`、`SABI-COMMON-001`、`IO-CANCEL-001`

## 1. 本切片完成的边界

新增独立 `nlos.sabi.OperationControl` v1.0 Protobuf payload，不再以业务 fixture method 充当查询或取消命令：

```text
QueryOperationRequest(OperationId, generation)
  → SQLite authority inspect
  → OperationStatus(state, cancel_epoch, optional Receipt)

CancelOperationRequest(OperationId, generation, expected_cancel_epoch)
  → BEGIN IMMEDIATE + generation/epoch CAS
  → Applied | Replayed | AlreadyTerminal
  → OperationStatus
```

schema、Rust 生成、TypeScript/Python checked-in 生成物和 64 KiB 独立 payload bound 同步加入 registry。`OperationStatus` 保留八种 durable lifecycle 状态，不把 `CANCEL_REQUESTED`、`PARTIAL_EFFECT` 或 `EFFECT_UNKNOWN` 压扁成普通失败。

`SqliteOperationStore::request_cancel_idempotent` 以 `expected_cancel_epoch` 线性化取消：首次请求只推进一次 epoch；精确重试读取已经推进的状态，不产生第二个 Outbox；若 completion 先提交，取消返回既有终态且不改写历史；stale handle 或不兼容 epoch fail-closed。

## 2. timer-driven worker 与真实 IPC

Rust conformance server 为一个保持 `REGISTERED` 的排队 Operation 启动独立 Tokio timer task。客户端先通过 OperationControl 查询到：

```text
REGISTERED(cancel_epoch=0)
  → Tokio sleep 到期
  → durable cancel-before-dispatch
  → CANCELLED_BEFORE_EFFECT(cancel_epoch=1, Receipt)
  → WakeFiber Outbox
```

worker 的成功/失败都有显式计数；fixture 结束时要求精确为一次成功、零次失败。结合 B-SCHEMA-012 的既有场景，最终 pending Outbox 精确为 6：4 个 `WakeFiber`、2 个 `ReconcileEffect`，其中 3 个 `CancelledBeforeEffect`、1 个 `PartialEffect`、1 个 `EffectUnknown`。

TypeScript 与 Python 客户端通过真实 Unix socket（Windows CI 使用 named pipe）验证：

1. 查询既有 pending Operation 得到 `DISPATCHED(cancel_epoch=0)`；
2. 独立 cancel payload 得到 `CANCEL_REQUESTED(cancel_epoch=1)`；
3. 以相同 expected epoch 重试，仍为 epoch 1；
4. 再次 query 读取相同 durable 状态；
5. 查询 timer worker 的 Operation，观察 `REGISTERED → CANCELLED_BEFORE_EFFECT` 及 Receipt。

## 3. 验证

本地定向与完整验证已通过：

```sh
npm run schema:generate
npm run schema:typecheck
cargo test -p nlos-schema
cargo test -p nlos-store --test operation_store
npm run directory:test:typescript
python tests/conformance/ipc/directory_chain.py
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
npm run schema:lint
npm run schema:test:typescript
python tests/conformance/schema/envelope.py
git diff --check
```

三平台 CI 在提交后继续执行并回填。

## 4. 当前不能证明什么

- timer worker 是真实异步 task，但当前 deadline 队列仍驻留 conformance server 内存；没有持久 deadline registration、进程重启后的 timer 恢复、timer wheel/优先队列或生产 scheduler 集成；
- worker 使用受控相对延时证明异步触发，不构成跨进程 clock-domain 或真实客户端 monotonic epoch 互操作保证；
- OperationControl schema 仍是 PoC candidate，尚无正式 SDK client facade、typed `NOT_FOUND/FENCED/CONFLICT` payload、双向 peer auth 或 ABI 冻结；
- 取消只到 Operation authority；尚未贯通 Task CancellationScope 树、Process supervisor、Driver cancel acknowledgement 和 provider callback；
- Receipt 仍是 nominal reference，没有 canonical body、签名、effect summary 或 attestation；
- 未覆盖 timer/cancel/complete 三方跨进程 crash matrix、deadline retention/GC 和高基数 timer 压测。

因此本 Evidence 记为 `PARTIAL PASS`。它关闭“取消依赖业务 method、deadline 只能同步检查点触发”的初始缺口，但不声称已形成 production deadline scheduler。按照阶段路线，下一主线转入 Go/C# generation/golden 探针，并至少选择一种语言完成独立 IPC PoC；生产 deadline queue/restart recovery 继续作为 B-PROCESS/Slice K 缺口保留。
