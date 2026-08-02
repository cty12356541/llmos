# B-SCHEMA-006：TypeScript/Python 真实本地 IPC client 初始证据

> 状态：PARTIAL PASS（三平台 CI 已通过）
>
> 日期：2026-08-02
>
> 对应：`SABI-DISC-001`、`SABI-COMMON-001`、`SABI-TRANSPORT-001`、`COMPAT-VER-001`、`TYPE-GEN-001`

## 1. 实现范围

本切片把 B-SCHEMA-005 的 TypeScript/Python generated service descriptor 接到真实本地 stream，不再只验证生成类型：

```text
TypeScript/Node LocalRpcClient ─┐
                               ├─ u32_be length + ExchangeRequest/Response
Python asyncio LocalRpcClient ─┘
                                      ↓
                         Rust nlos-ipc conformance server
```

新增：

- `sdk/typescript/src/local_rpc.ts`：Node `net.Socket` client；同一 endpoint API 映射 Unix socket path 或 Windows named-pipe path；
- `sdk/python/nlos_sdk/local_rpc.py`：asyncio client；Unix 使用 `open_unix_connection`，Windows 候选使用 CPython Proactor loop 的 named-pipe connection；
- feature-gated `nlos-ipc-echo` conformance binary：只在 `conformance-server` feature 下构建，接收一个请求并由 Rust schema/IPC 路径原样 echo typed envelope；
- TypeScript/Python conformance 程序：各自启动同一个 Rust server，完成真实跨语言往返；
- 三平台 workflow：在原 schema gate 后执行两种语言的本地 IPC conformance。

endpoint 仍由调用者提供，代表未来 ServiceDirectory resolver 的输出。本切片没有把 endpoint 写入 schema，也没有将固定 path/name 冒充服务发现。

## 2. 与 Rust transport 对齐的语义

两种 client 当前都实现：

- 4-byte big-endian length prefix；
- `1..=1 MiB` 可配置 frame bound，接收 body 前检查声明长度；
- connect/read/write 非零 timeout；
- 单连接只允许一个 in-flight exchange，第二个调用立即返回 `BACKPRESSURE`；
- schema name、major、critical extension、16-byte request ID、service/method 的 client-side compatibility gate；
- response request ID correlation；
- timeout、I/O、malformed response、超界或串线后连接永久 poison，后续必须重连；
- unavailable endpoint 返回 typed connect/timeout error。

TypeScript reader 在拼接前限制 `maximumFrameBytes + 4`，避免 peer 以小声明长度加大量 trailing bytes 诱导无界累积。Python reader 用单个 read timeout 包围 prefix + bounded body，避免分别给两段各一份完整 deadline。

## 3. 跨语言测试

本地 macOS arm64 已通过：

```sh
npm run schema:typecheck
npm run schema:test:typescript
npm run ipc:test:typescript
python tests/conformance/schema/envelope.py
python tests/conformance/ipc/client.py
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

TypeScript 与 Python conformance 都覆盖：

1. 启动 feature-gated Rust server 并等待明确 `READY`；
2. unknown major 在 client 侧 fail-closed，且不会毒化尚未使用的连接；
3. 第一个合法 request 通过真实 OS IPC 到达 Rust 并取得相同 request ID/payload；
4. 与第一个请求并发的第二个调用立即返回 `BACKPRESSURE`；
5. server 正常退出；
6. 不存在 endpoint 返回 `CONNECT` 或耗尽有界窗口后的 `TIMEOUT`。

提交 `4ca76c9` 对应的[三平台 CI run 30734744799](https://github.com/cty12356541/llmos/actions/runs/30734744799) 已全部成功。macOS 与 Ubuntu 通过 Unix socket；Windows 通过 Node named pipe 与 Python Proactor 两条真实 client 路径，并同时通过 workspace test 和 Clippy。

## 4. 当前能证明什么

- Rust、TypeScript 和 Python 已经可以通过同一 schema 与 framing 发生真实 client/server 往返，不再只是读取共同 golden；
- TS/Python 对当前 envelope 的主要 compatibility、frame、timeout、backpressure、correlation 和 fail-closed 规则与 Rust adapter 对齐；
- Node client 不需要 gRPC/HTTP2，也没有把 transport 写入 service schema；
- Python candidate 保持 asyncio API，Unix 路径不需要额外 transport dependency；
- conformance server 默认 feature 不启用，不能被普通 `nlos-ipc` build 误当作生产服务。

## 5. 当前不能证明什么

- Windows 实机已通过当前 CI profile，但 Python 使用的 `ProactorEventLoop.create_pipe_connection` 是 CPython concrete loop 能力，不在 `AbstractEventLoop` 稳定公共表面；当前成功不能替代最低 CPython profile、长期兼容承诺或受维护 adapter 的选择；
- client 尚未通过 OS API 验证 server peer identity；当前安全性依赖 endpoint 来自未来受信 ServiceDirectory 和 server 侧 authorization，不能据此宣称双向认证；
- ServiceDirectory、version/feature negotiation、Capability、deadline/cancel、IdempotencyKey、`E_UNCERTAIN`、Operation/Receipt 和 partial failure 尚未进入最小 schema；
- 没有自动重连；poison 后由调用方新建 client，何时允许以同 IdempotencyKey 重试必须由后续 common SABI 语义决定；
- 当前是源码内 candidate SDK，没有 npm/PyPI package、版本承诺、安装文档、API 稳定性或发布签名；
- 没有 streaming、多连接池、公平性、长期压力、恶意 peer 持续小块发送或 server restart 矩阵。

因此 TypeScript/Python 只晋升为 `SDK-2 CANDIDATE / PARTIAL`，不能标记 `SDK-3` 或官方 SDK。下一验收门是实现 ServiceDirectory negotiation 与最小 common SABI semantics；Python Proactor 稳定边界作为 SDK packaging/profile 风险继续跟踪。
