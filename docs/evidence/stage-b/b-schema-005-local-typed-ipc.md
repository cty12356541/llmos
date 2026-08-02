# B-SCHEMA-005：本地 typed IPC 与平台适配初始证据

> 状态：PARTIAL PASS
>
> 日期：2026-08-02
>
> 对应：`SABI-DISC-001`、`SABI-COMMON-001`、`SABI-TRANSPORT-001`、`COMPAT-VER-001`、`TYPE-GEN-001`

## 1. 实现范围

本切片在既有 `nlos.sabi.v1.Envelope` 外增加 transport-neutral unary service：

```text
LocalRpcService.Exchange(ExchangeRequest) -> ExchangeResponse
```

request/response wrapper 都只持有一个现有 Envelope；schema 不包含 socket path、named-pipe name、OS credential、gRPC 或 HTTP/2 绑定。Rust 由 `prost-build` 的 service generator 生成 `local_rpc::Client` trait 和稳定的 service/method full name；TypeScript 生成 `GenService` 描述，Python 生成 service/method descriptor。后两者当前是可供后续 adapter 消费的生成表面，不是已经可连接本地端点的 runtime client stub。

新增 `nlos-ipc` crate，负责：

- 统一的 `u32_be length || protobuf wrapper` framing；
- wrapper 与内层 Envelope 的 schema/major/critical/request ID/service/method 检查；
- Unix domain socket 和 Windows named pipe 平台 adapter；
- connect/read/write/accept timeout、frame 上限、单连接单 in-flight backpressure；
- OS peer identity 到同步 authorization hook 的边界；
- response request ID correlation；
- 已验证 response 的原始 wire forwarding，避免 unknown Protobuf field 被重编码丢失。

endpoint 由调用者提供，预留给后续 ServiceDirectory/resolver；本切片没有用固定 endpoint 冒充服务发现。

## 2. 有界与失败语义

| 风险 | 当前行为 |
|---|---|
| 超界发送/接收 | 配置上限必须位于 `1..=1 MiB`；接收先读 4-byte 长度并在分配 body 前拒绝超界声明 |
| 慢连接/半帧 | connect/accept/read/write 各有非零 timeout；半帧或 EOF 返回带 operation 的显式 I/O error |
| 并发积压 | 每个 client connection 只允许一个 in-flight call；第二个并发调用立即返回 `Backpressure`，不进入无界队列 |
| 未授权 peer | `PeerAuthorizer` 在读取任何 frame 前执行；拒绝返回 `AuthorizationDenied` |
| 响应串线 | response 的 16-byte request ID 必须等于 request，否则 `RequestIdMismatch` fail-closed |
| unknown field | validated wrapper 保留精确输入 bytes；forwarding response 不 decode/re-encode |
| endpoint 暂不可用 | Unix 返回显式 connect I/O error；Windows 对 busy/not-found 做 10 ms 有界重试，耗尽 connect window 后返回 timeout |
| exchange 结果不确定 | send/read timeout、I/O、malformed response 或 request ID mismatch 后连接被标记为 unusable；后续调用明确要求新建连接，禁止在可能错帧的 stream 上盲重试 |

Unix socket 创建后设置 owner-only `0600`，连接双方读取 Tokio `peer_cred` 暴露的 PID/UID/GID。adapter 从不自动删除既有 socket path，避免误删非本任务对象；生命周期 owner 必须显式清理自己创建的 endpoint。

Windows named pipe 使用 `first_pipe_instance` 防止静默附着既有 namespace，拒绝 remote client，限制 2–254 个实例并限制内外 buffer；client 显式使用 `SECURITY_IDENTIFICATION`，防止服务端通过该连接冒充客户端。当前安全 Tokio API 没有直接提供 named-pipe peer PID，本切片将 `process_id` 明确设为 `None`；需要 PID/Token/ACL 的 policy 必须 fail-closed，不能把缺失身份当作可信。

## 3. 测试与当前结果

当前本地 macOS arm64 已通过：

```sh
npm run schema:generate
cargo test -p nlos-schema
cargo test -p nlos-ipc
cargo clippy -p nlos-ipc --all-targets -- -D warnings
```

- `nlos-schema`：9 项 compatibility/golden/service surface 测试；
- `nlos-ipc`：6 项 transport-neutral framing/client 测试；
- Unix 平台：真实 socket 往返、`0600` mode、peer credential 类型和不存在 endpoint 的显式 connect error，共 2 项；
- 失败路径覆盖 authorization-before-read、oversized declared frame、half-frame EOF、request ID mismatch、并发 backpressure/read timeout 和 unknown wrapper field 原字节 forwarding。

Windows 标准库交叉目标在本地下载阶段持续无输出，已主动终止，不能记作本地 Windows 编译通过。首次远程 [run 30730117157](https://github.com/cty12356541/llmos/actions/runs/30730117157) 的 Windows workspace test 已编译并通过真实 named-pipe 往返与 unavailable-pipe timeout；随后 Clippy 以 `cast_possible_wrap` 拒绝 `ERROR_PIPE_BUSY as i32`，因此整次 run 正确记为失败。remediation 改用显式 `cast_signed()` 后，[三平台 run 30730221706](https://github.com/cty12356541/llmos/actions/runs/30730221706) 全部成功：Ubuntu 47s、macOS 1m15s、Windows 2m5s，均通过 schema gate、跨语言 conformance、workspace test 和 Clippy；实现提交对应的 [fuzz run 30730117174](https://github.com/cty12356541/llmos/actions/runs/30730117174) 也成功。

## 4. 当前能证明什么

- 同一个 Protobuf service schema 可以驱动 Rust client trait、TypeScript service descriptor 和 Python service descriptor；
- Rust client/server 可通过 transport-neutral framing 完成 typed request/response，并保持既有兼容检查和 unknown-field forwarding 语义；
- macOS 上真实 Unix socket 后端通过 owner-only endpoint 与 peer credential hook 工作；
- Windows runner 上真实 named-pipe 往返、unavailable-pipe timeout、workspace test 和 Clippy 已通过；
- frame、timeout 和单连接并发积压有显式上限，常见断连/半帧/超界/串线不会被误判为成功；
- transport/credential/Protobuf 依赖均留在 schema/IPC adapter，没有进入 `nlos-types`。

## 5. 当前不能证明什么

- TypeScript/Python 当前只有生成 service descriptor，没有本地 socket/pipe runtime client、跨语言真实往返或取消/重连状态机；
- ServiceDirectory、version negotiation、Capability、deadline/cancel、Operation、partial failure 和 Receipt 尚未进入这个最小 service；
- Windows peer PID、token SID 和显式 pipe ACL 尚未提取/固化；默认 token DACL 不能外推为完整 NLOS authorization；
- 当前 server primitive 一次处理一个请求；没有多连接 supervisor、公平调度、streaming、连接池或自动重连。失败连接会 fail-closed，调用者重建连接的策略与幂等语义仍待实现；
- 1 MiB 是公共硬上限，不代表每个未来 service payload 已有更严格的独立限额。

因此该切片保持 `PARTIAL PASS`，`B-SCHEMA` 与 ADR-0003 继续为 `IN_PROGRESS/POC`。下一步实现 TypeScript/Python transport client、ServiceDirectory negotiation 与可测试的 reconnect/cancel/deadline 语义。
