# B-SCHEMA-008：跨语言 ServiceDirectory 两跳 IPC 初始证据

> 状态：PARTIAL PASS（三平台 CI 待本提交推送后复验）
>
> 日期：2026-08-02
>
> 对应：`SABI-DISC-001`、`SABI-TRANSPORT-001`、`COMPAT-VER-001`

## 1. 实现范围

B-SCHEMA-007 已有目录 schema 和 Rust snapshot negotiation，但 SDK 尚未实际调用目录。本切片把真实调用链贯通为：

```text
trusted bootstrap endpoint（测试注入）
  → Rust ServiceDirectory IPC
  → negotiate(service/schema/version/features/platform transport)
  → validated ServiceBinding + business endpoint
  → SDK 关闭 directory connection
  → SDK 连接 negotiated business endpoint
  → Rust business service typed round-trip
```

新增：

- feature-gated `nlos-directory-chain` Rust conformance binary，同时绑定 directory 和 business 两个 Unix socket/Windows named pipe；
- TypeScript `ServiceDirectoryClient.negotiateAndConnect`；
- Python `ServiceDirectoryClient.negotiate_and_connect`；
- 两种 SDK 对 directory identity、typed error、binding ID/generation、service/schema/version、required feature、transport kind 和 endpoint bound 的 fail-closed 校验；
- TypeScript/Python 两套独立 conformance 程序；
- 三平台 workflow 中的真实 `bootstrap → negotiate → service` 组合。

测试代码知道两个 endpoint，以便启动 Rust fixture；但 SDK API 只接收 directory bootstrap endpoint。business endpoint 没有传入 resolver，必须来自 Rust negotiation response，测试随后断言 binding 中的 endpoint 与 fixture 一致。

## 2. 当前协议行为

- directory request 使用外层 `nlos.sabi.Envelope`，service/method 为 `service_directory/negotiate`，payload 是有独立 identity 和 64 KiB bound 的 `NegotiateServiceRequest`；
- server 通过 `SnapshotDirectory` 选择 binding，将 `NegotiateServiceResponse` 放入同 request ID 的 response envelope；
- SDK 只声明当前平台实际支持的 transport：Unix 使用 socket，Windows 使用 named pipe；
- response 为 typed directory error 时不连接业务 endpoint；unknown/unspecified error code、缺失 result 或不相容 binding 按 compatibility failure 拒绝；
- 成功后 directory connection 被关闭，再由现有有界 `LocalRpcClient` 连接 negotiated endpoint；
- conformance business service echo request ID/payload，证明第二跳不是只解析 binding 而未使用。

当前 conformance registration 暂以 `service=operation`、`schema=nlos.sabi.Envelope v1.0` 表示尚未进入 registry 的最小业务服务。它只能证明 discovery/transport chain，不能把 Envelope 冒充正式 Operation service schema。

## 3. 本地验证

macOS arm64 已通过：

```sh
npm run schema:typecheck
npm run schema:test:typescript
npm run ipc:test:typescript
npm run directory:test:typescript
python tests/conformance/schema/envelope.py
python tests/conformance/ipc/client.py
python tests/conformance/ipc/directory_chain.py
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

TypeScript 和 Python 均完成：

1. 启动两个本地 endpoint 并等待 Rust 明确 `READY`；
2. 只用 bootstrap endpoint 构造 ServiceDirectory client；
3. negotiate `operation` + Envelope v1 + 当前平台 transport；
4. 校验 16-byte binding ID、generation=7 和返回 endpoint；
5. 自动连接 business endpoint；
6. 发出独立 request 并取得相同 request ID/payload；
7. 两个连接关闭，Rust fixture 正常退出。

## 4. 当前能证明什么

- TypeScript/Python 已实际消费 Rust ServiceDirectory negotiation response，不再由应用直接把业务 endpoint 传给 `LocalRpcClient`；
- 同一目录和 SDK 代码路径可在 Unix socket 与 Windows named pipe 之间按平台 transport 协商；
- 不相容或畸形 binding 在连接业务 endpoint 前 fail-closed；
- directory schema、framing 和 business framing 保持 transport-neutral，OS endpoint 只存在于 negotiated binding；
- 现有 IPC timeout、backpressure、request correlation 和 connection poison 继续作用于两跳中的每条连接。

## 5. 当前不能证明什么

- bootstrap 仍由 conformance test 以 raw endpoint 注入；Namespace typed handle、Process birth inheritance 和 ServiceDirectory 自身身份认证尚未实现；
- conformance binary 是单次请求 fixture，不是 durable/动态 ServiceDirectory daemon；
- SDK 尚未实现 `resolve` candidate browsing、`watch`、`describe_error`、binding lease/撤销、generation change、健康检查或自动重新协商；
- client 尚未通过 OS peer credential/token 验证 directory 和 business server 身份；
- 当前业务 service 复用 Envelope v1 身份，没有正式 Operation payload schema、Capability 或 common SABI header；
- 未覆盖 typed negotiation error 的真实 IPC 矩阵、恶意 directory、stale endpoint、directory restart、binding replacement 或 uncertain retry；
- common SABI 的 Principal/Application/Process、deadline/cancel、IdempotencyKey、Operation/Receipt、partial/uncertain error 仍未实现。

因此本 Evidence 只把 `Rust directory fixture + TypeScript/Python negotiate-and-connect` 记为 `PARTIAL PASS`。下一验收门转入最小 common SABI header 与错误/幂等语义；生产目录生命周期和 bootstrap authority 仍作为 ServiceDirectory 后续工作保留。
