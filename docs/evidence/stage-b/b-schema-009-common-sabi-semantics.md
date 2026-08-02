# B-SCHEMA-009：common SABI 元数据与安全重试语义初始证据

> 状态：PARTIAL PASS（三平台 CI 与 fuzz 已通过；durable dedup/result 的后续证据见 B-SCHEMA-010，其余生产语义仍未完成）
>
> 日期：2026-08-02
>
> 对应：`SABI-COMMON-001`、`SABI-RECEIPT-001`、`SABI-AUTH-001`、`KABI-ERR-001`、`KABI-UNCERTAIN-001`、`KABI-IDEM-002`

## 1. 本切片完成的边界

`nlos.sabi.Envelope` 以 additive minor=1 candidate 增加 mutually-exclusive request/response common context；原 minor=0 frame 仍可解码，公共 ABI 仍未冻结。

请求 context 包含：

- 128-bit Principal/Application/Process ID 与非零 Process generation；
- opaque bounded activity context；
- 可选 TaskAttempt authority/control/cancel/permit/isolation fence；
- 独立的 128-bit correlation ID 和 128-bit IdempotencyKey；
- host monotonic deadline；
- 有界、带 slot+generation 的 Capability/Reservation handle；
- 可选 32-byte proposal/input SHA-256 digest。

响应 context 包含：

- correlation ID；
- generation-fenced Operation reference；
- 不可混淆的 Receipt reference；
- 19 类 common error 与显式 retry directive。

`request_id` 继续只负责一次 IPC exchange 的请求/响应关联；它没有被重解释为幂等键。这样相同 IdempotencyKey 可以在重新连接后的新 exchange 中继续使用，而每次 exchange 仍有独立 request ID。

## 2. Fail-closed 规则

Rust、TypeScript、Python validators 当前共同拒绝：

- 缺失 caller 或长度不是 16 bytes 的 nominal ID/correlation/idempotency/Operation/Receipt；
- side-effecting method 缺失 IdempotencyKey；
- long-running method 缺失 deadline，或 deadline 已在同一 monotonic clock domain 过期；
- 零 generation、重复/超量 Capability、重复/超量 Receipt、错误长度 proposal digest；
- unknown/unspecified common error 或 retry directive；
- `E_UNCERTAIN`/`E_EFFECT_UNKNOWN` 没有 Operation，或没有要求“查询 Operation/使用原 IdempotencyKey 重试”；
- `E_RETRY` 没有要求使用原 IdempotencyKey；
- side-effecting response 同时缺少 Operation 和 Receipt，无法证明即时或最终 effect evidence；
- `E_PARTIAL` 没有任何 Receipt；
- 过长或含 NUL 的 safe error message。

这些规则防止 SDK 在“服务可能已经产生副作用”时换 key 重做，也防止把 partial/uncertain 压扁为普通网络失败。

## 3. 跨语言与真实 IPC 证据

新增两个完整 Envelope golden：

- common long-running mutation request；
- `E_UNCERTAIN` + Operation/Receipt + safe retry directive response。

Rust/TypeScript/Python 均逐字节解码、校验并重新编码同一 vectors。两个 vectors 同时进入 bounded Protobuf fuzz seed 集，防止新 nested/oneof 路径只被确定性测试覆盖。

B-SCHEMA-008 的目录两跳 fixture 也已提升：

```text
TS/Python
  → trusted bootstrap
  → ServiceDirectory negotiate Envelope minor=1
  → business request with caller/idempotency/deadline/capability
  → Rust validates LONG_RUNNING_MUTATION before dispatch
  → response with correlated Operation + Receipt
  → TS/Python validate common response
```

因此当前证据不只证明 schema generation，还证明 common metadata 穿过 Unix socket/Windows named pipe 的现有 ServiceDirectory 两跳路径，并在 Rust 服务入口执行验证。

本地 macOS 已通过：

```sh
npm run schema:lint
npm run schema:generate
npm run schema:typecheck
npm run schema:test:typescript
npm run directory:test:typescript
python tests/conformance/schema/envelope.py
python tests/conformance/ipc/client.py
python tests/conformance/ipc/directory_chain.py
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
NLOS_FUZZ_RUNS=2000 NLOS_FUZZ_TOOLCHAIN=nightly-2026-08-01 scripts/run-fuzz-smoke.sh
```

三个 fuzz target 各完成 2,000 次本地 bounded run，无 crash、timeout、OOM 或断言反例。

远程复验：

- [Rust cross-platform verification run 30737782776](https://github.com/cty12356541/llmos/actions/runs/30737782776) 在 Linux、macOS、Windows 全部成功；三平台均通过 Rust/TypeScript/Python golden/反例和目录两跳 common-context IPC，Windows 实际使用 named pipe；
- [Schema fuzz smoke run 30737782772](https://github.com/cty12356541/llmos/actions/runs/30737782772) 成功，新 common request/uncertain seeds 进入 Protobuf fuzz corpus；
- [GitHub Pages run 30737782777](https://github.com/cty12356541/llmos/actions/runs/30737782777) 成功。

## 4. 当前不能证明什么

- 本切片提交时 IdempotencyKey 只有线协议和入口校验；后续 [B-SCHEMA-010](./b-schema-010-durable-idempotency-result.md) 已补本地 durable dedup/result authority，但真实 IPC reconnect 仍未验证；
- deadline 当前只验证同宿主 monotonic 值，尚未实现排队/dispatch/callback 全链路 deadline fence，也未定义远程 clock-domain 映射；
- cancel epoch 被携带但尚未接入 Operation registry、Process supervisor 或 service handler 的真实取消传播；
- Operation/Receipt 当前只是 typed reference，fixture 返回固定测试引用；尚无正式 Operation SABI payload、Receipt canonical body/signature/attestation；
- Capability handle 只验证 slot+generation 形状；没有通过 Namespace/authority 查权，也没有 peer auth；
- safe message 只有长度/NUL 限制，生产错误脱敏、localized description 和 service-specific bounded detail 尚未实现；
- 没有真实 server-side `E_UNCERTAIN`/`E_PARTIAL` 故障注入、stale generation、reconnect same-key retry 或持久化恢复矩阵。

因此 B-SCHEMA-009 只把“common wire metadata + 三语言安全校验 + 两跳传输”记为 `PARTIAL PASS`，不把 TS/Python 升级为完整 `SDK-3`。durable same-key dedup/result 的三平台 authority 已由 B-SCHEMA-010 推进，三平台真实 SABI 重连由 B-SCHEMA-011 推进；下一验收门是 server restart 组合与 deadline/cancel/uncertain 服务端状态机。
