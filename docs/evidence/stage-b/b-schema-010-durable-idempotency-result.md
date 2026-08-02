# B-SCHEMA-010：durable same-key dedup/result 初始证据

> 状态：PARTIAL PASS（本地与远程三平台单节点 authority 测试通过；真实 SABI 接线的后续证据见 B-SCHEMA-011）
>
> 日期：2026-08-02
>
> 对应：`KABI-IDEM-001`、`KABI-IDEM-002`、`KABI-UNCERTAIN-001`、`KABI-QUERY-001`、`SABI-COMMON-001`

## 1. 本切片完成的边界

`nlos-store` 的 SQLite durable format 从 v2 演进到 v3，新增 `idempotent_calls` authority。幂等身份按以下 tuple 隔离：

```text
(ApplicationId, service, method, 128-bit IdempotencyKey)
```

每条记录同时绑定：

- 32-byte canonical request SHA-256 digest；
- generation-fenced Operation handle；
- terminal ReceiptId；
- 最多 1 MiB 的 transport-independent service result bytes。

service/method 必须非空、单项不超过 128 bytes 且不含 NUL。数据库使用 `STRICT` table、长度约束、唯一 Operation 绑定和 completed-result immutable trigger；未知 schema version 继续 fail-closed。

本切片没有让 store 信任任意 Application 自报的 digest。`begin_idempotent_operation` 是 authority 内部 API，参数必须由完成 SABI validation 的可信 service adapter 从 canonical request bytes 计算；把该 API 直接暴露给不可信 client 不在契约内。

## 2. 原子状态语义

首次调用在一个 `BEGIN IMMEDIATE` 事务内同时完成：

```text
claim scoped IdempotencyKey + request digest
  + register Operation
COMMIT
  → Created(Operation)  // 唯一一次 dispatch authority
```

同一 key 的后续调用只有三种结果：

1. request digest 不同：返回 `IdempotencyConflict`，不注册或派发新 Operation；
2. digest 相同但结果未提交：返回 `PendingOrUncertain(original Operation)`，不得重新派发副作用；
3. digest 相同且结果已提交：返回原 Operation、ReceiptId 和逐字节相同的 `result_wire`。

terminal callback 使用 `complete_idempotent_operation` 在同一事务提交：

```text
Operation terminal transition + revision CAS
  + Receipt identity
  + immutable response bytes
  + WakeFiber | ReconcileEffect Outbox
COMMIT
```

因此不存在“Operation 已对外完成，但 dedup result 尚未提交”的正常成功窗口。exact duplicate callback 返回既有结果且不产生第二条 Outbox；相同 callback/Receipt 对应不同 response bytes 会 fail-closed 为 conflict。

## 3. 恢复与迁移证据

新增测试覆盖：

- 首次 claim 返回 `Created`，相同 tuple/digest 只返回 `PendingOrUncertain`；
- 相同 tuple/key 配不同 request digest 被拒绝；
- 同一个 key 可在不同 service/method scope 独立使用；
- completion 后关闭并重开数据库，重复原调用返回逐字节相同结果，不重新授予 dispatch；
- dispatch 后、result 前模拟进程退出，重开后保持原 Operation `DISPATCHED` 并返回 `PendingOrUncertain`；
- duplicate callback 的 exact result 可重放，不同 result bytes 被拒绝且 Outbox 仍只有一条；
- 空 scope 和超过 1 MiB 的 result 在写入前被拒绝，Operation 状态不被错误推进；
- golden v1 数据库可事务化前向迁移到 v3；迁移逐写入点中断只留下完整 v1、v2 或 v3；online backup 保留 schema version。

本地通过：

```sh
cargo test -p nlos-store
cargo clippy -p nlos-store --all-targets -- -D warnings
cargo fmt --all -- --check
```

远程 [Rust cross-platform verification run 30738888761](https://github.com/cty12356541/llmos/actions/runs/30738888761) 已在 Ubuntu、macOS、Windows 全部成功；三平台均执行 workspace test 与 Clippy，因而 SQLite v3 migration、same-key replay、conflict、uncertain recovery 和 immutable result 测试均实际运行。对应 [GitHub Pages run 30738888747](https://github.com/cty12356541/llmos/actions/runs/30738888747) 也成功。

## 4. 当前不能证明什么

- 本切片提交时尚未接入 ServiceDirectory 两跳 server；后续 [B-SCHEMA-011](./b-schema-011-durable-idempotency-ipc.md) 已补 TS/Python reconnect + same-key 真实 IPC 并通过三平台，进程重启组合也已在本地通过、远程三平台待验证；
- 尚未实现排队、dispatch、callback 全链路 deadline fence、cancel propagation 和真实 server-side `E_UNCERTAIN` 映射；
- request digest 的 canonicalization/计算仍由可信 service adapter 负责，本切片只持久化并比较固定 32-byte identity；
- Receipt 仍只有 nominal ID，没有 canonical body、签名、attestation 或正式查询 API；
- 没有 retention/GC policy；在该策略形成前记录不会主动删除，不能据此宣称已满足所有 retry/lease/recovery/audit window；
- 当前证明的是单节点 SQLite authority 的 at-most-once dispatch grant 与 durable result replay，不是跨节点 exactly-once，也不能证明外部 provider 自身严格幂等。

因此本 Evidence 记为 `PARTIAL PASS`。下一验收门是把 durable authority 接入真实 SABI server，并实现 deadline/cancel/uncertain 服务端状态机与 reconnect fault matrix。
