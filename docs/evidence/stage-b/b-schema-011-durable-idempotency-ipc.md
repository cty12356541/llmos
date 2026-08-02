# B-SCHEMA-011：durable idempotency 真实 SABI 重连初始证据

> 状态：PARTIAL PASS（本地与远程三平台 Rust↔TypeScript/Python 真实 IPC 通过；server restart/deadline/cancel 待完成）
>
> 日期：2026-08-02
>
> 对应：`KABI-IDEM-001`、`KABI-IDEM-002`、`KABI-UNCERTAIN-001`、`KABI-QUERY-001`、`SABI-COMMON-001`、`SABI-TRANSPORT-001`

## 1. 本切片完成的边界

B-SCHEMA-010 的 SQLite v3 authority 已接入 `nlos-directory-chain` Rust business endpoint。真实调用路径现在是：

```text
TypeScript / Python
  → trusted bootstrap
  → ServiceDirectory negotiate
  → business endpoint
  → validate common SABI request
  → trusted adapter computes SHA-256(payload)
  → atomic key claim + Operation register
  → dispatch exactly once
  → atomic terminal Operation + Receipt + result + Outbox
  → response or reconnect replay
```

conformance server 从已验证 caller/application/idempotency 字节构造 nominal types，并按 `(ApplicationId, service, method, IdempotencyKey)` 查询 authority。OperationId 由该稳定 scope 计算，exchange request ID、correlation ID 和 deadline 不进入 Operation identity。

## 2. 接线暴露并修正的结果边界

首次尝试接线时确认：authority 不能把整个 `ExchangeResponse` 当作幂等结果原样回放。重连后的 exchange 有新的 `request_id`；若返回旧 envelope，Rust/TS/Python client 都必须把它视为 response 串线。

因此 `DurableCallResult` 已把公共语义从 `response_wire` 收紧为 `result_wire`：

- durable：稳定 service result bytes、Operation、Receipt；
- 每次 exchange 重新生成：request ID、correlation ID 和 transport envelope；
- SQLite v3 的内部历史列名仍为 `response_wire`，但代码注释明确其只保存 transport-independent service result，避免无必要的 durable migration；
- retry 可以使用新的 exchange request ID/correlation，同时仍返回同一 Operation、Receipt 和稳定结果。

## 3. 不确定断线、重连与冲突证据

fixture 的第一次 business call 故意执行以下顺序：

```text
commit durable result
drop connection before response write
```

TS/Python client 都收到 typed read failure 并 poison 原连接。随后客户端直接重连已协商 business endpoint，使用：

- 原 Application/service/method/IdempotencyKey；
- 原业务 payload；
- 新 exchange request ID；
- 新 correlation ID。

Rust authority 返回原 Operation、Receipt 和 `result_wire`，再使用当前 exchange metadata 封装响应。server 结束前断言该 `cancel` handler 的 dispatch count 精确为 1，防止测试只比较相同输出却漏掉重复副作用。

同一链路还覆盖：

- 相同 scoped key 配不同 payload digest：返回 `E_CONFLICT + DO_NOT_RETRY`，并关联原 Operation；
- 新 key 的 Operation 保持 `DISPATCHED`：返回 `E_UNCERTAIN + QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY`，并关联该 Operation；
- TS/Python common response validator 对上述 typed failure 执行 fail-closed 校验。

## 4. 本地验证

以下命令通过：

```sh
cargo test -p nlos-store -p nlos-ipc --all-features
npm run schema:typecheck
npm run directory:test:typescript
python tests/conformance/ipc/directory_chain.py
```

新增 `sha2 0.11.0` 仅进入 feature-gated conformance server 的可信 request adapter，用于实际计算 SHA-256；其 MSRV 1.85，许可证为 MIT OR Apache-2.0。它不是新的 wire/durable ABI。

远程验证：

- [Rust cross-platform verification run 30740180511](https://github.com/cty12356541/llmos/actions/runs/30740180511) 在 Ubuntu、macOS、Windows 全部成功；三平台均运行 Rust workspace/Clippy、TS/Python directory-chain，Windows 实际使用 named pipe；
- [Schema fuzz smoke run 30740180497](https://github.com/cty12356541/llmos/actions/runs/30740180497) 成功；
- [GitHub Pages run 30740180477](https://github.com/cty12356541/llmos/actions/runs/30740180477) 成功。

## 5. 当前不能证明什么

- 当前覆盖连接在 commit 后断开，但 conformance server 进程本身没有在两次 IPC 之间重启；进程重开恢复仍由 `nlos-store` 测试证明，二者尚未组合；
- request digest 当前只覆盖 fixture 的完整业务 payload；正式 service schema 必须定义哪些 canonical effect fields 进入 digest，不能对任意 envelope 盲目哈希；
- `pending` 是受控 fixture 分支，尚无真实异步 worker 完成、deadline timer 或 cancel propagation；
- 尚未实现排队前、dispatch 前和 callback 时的 deadline fence，也未把 cancel epoch 接到真实 SABI handler；
- Receipt 仍只有 nominal ID；Capability 和 peer authorization 仍是 conformance hook/allow fixture；
- 三平台已通过现有 CI workload，但尚无网络文件系统、跨主机 transport 或多 authority 证明。

因此本 Evidence 记为 `PARTIAL PASS`。下一验收门是实现 deadline/cancel/uncertain 的真实服务端状态机与进程重启重放组合测试。
