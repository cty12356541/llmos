# B-TASK-006L：SystemControl recovery handler

> 状态：`PARTIAL PASS`　　日期：2026-08-23（第二十二增量）
>
> 对应：`B-TASK-006K`、`B-SCHEMA-014`、`[SABI-AUTH-001]`、`[CTRL-PARITY-001]`、`[CTRL-RECOVERY-001]`

## 已实现事实

1. 新增无 canonical state 的 `nlos-system-control` handler；它只读取 worker health/TaskAuthority，并把 mutation 委托给 TaskAuthority schema v9 acknowledgement transaction。
2. `get` 每次从 TaskAuthority 重新读取 durable retrying/escalated/unacknowledged/resolved gauge；worker 本地 failure message 被丢弃，只映射 bounded plan ID 与 typed authority。
3. `submit` 先验证 common SABI mutation context，再要求 authenticated caller Principal 等于 command issuer、ControlCommandId 等于 envelope IdempotencyKey，之后才进入 pluggable Capability authorizer。
4. acknowledgement 使用 command target + expected revision 驱动 TaskAuthority failure-count CAS；响应 payload 与 common response context 引用同一个 immutable Receipt，exact replay 不重复确认，且不会隐式 resume。
5. ServiceDirectory 可按 `nlos.sabi.SystemControl` v1 协商 binding；本地 Unix socket 测试从协商 endpoint 建连并完成 typed `get`，另一个 framed LocalRpc 测试完成 `submit` 与 Receipt replay。

6. `SystemControlError::to_sabi_failure` 将本地拒绝映射为有界 common SABI `SabiFailure`：契约/期限/权限/冲突/状态/NotFound/Durability/Driver/Fenced/NotSupported 均有固定 code、retry directive 和短安全消息；不传播 SQLite、authority reason 或 corrupt-record 诊断。
7. `failure_envelope` 保留 request/correlation identity，清空 payload、Operation 与 Receipt evidence；malformed correlation 只回退到合法 request ID 或有界全零 correlation。映射、SQLite 同 key 重试安全和 envelope 脱敏测试已加入 `nlos-system-control`。
8. 新增 Windows-only named-pipe round-trip：真实 `NamedPipeListenerAdapter`/`connect`、`ExactPeerAuthorizer` OS credential pre-gate、typed `submit`、durable Receipt 和第二连接 exact replay；测试 authorizer 仍是固定 stub，不代表真实 Capability authority。
9. `RecoverySystemControl::handle_for_ipc` 为 IPC caller 提供统一 typed response 适配：成功保持原 Envelope，任一 handler error 走 bounded `failure_envelope`，不再把拒绝压成裸 `IpcError::ServiceFailure`。
10. common SABI Rust/TypeScript/Python validator 现在允许已知终止 `SabiFailure` 在无 Operation/Receipt 时返回；无 failure 的 mutation 仍要求 effect evidence，`UNCERTAIN`/`EFFECT_UNKNOWN`/`PARTIAL` 约束不变。真实 Unix framed IPC 拒绝测试已验证 `RIGHTS + DO_NOT_RETRY`、空 payload/Operation/Receipt 和无 durable acknowledgement。

## 2026-08-23 增量验证

- `cargo fmt --all -- --check`：通过。
- `cargo test -p nlos-system-control --quiet`：通过（`recovery_control` 7 项、`system_control_failure_mapping` 5 项；Windows-only test 在 macOS 目标下为 0 tests；metrics contract 另见 B-TASK-006M）。
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过。
- `cargo test --workspace --quiet`：通过。
- `npm run schema:test:typescript`、`npm run schema:typecheck`、`python tests/conformance/schema/envelope.py`：通过；三语言均覆盖 terminal rejection without effect evidence。
- 三平台 + MSRV Rust CI [32588868965](https://github.com/cty12356541/llmos/actions/runs/32588868965) 的 Ubuntu/macOS/Windows/MSRV jobs 均成功；Pages [32588868943](https://github.com/cty12356541/llmos/actions/runs/32588868943) 成功。Windows named-pipe 的编译/执行结论以该 CI 为准。
- 本轮三平台 + MSRV Rust CI [32614979438](https://github.com/cty12356541/llmos/actions/runs/32614979438) 的 Ubuntu/macOS/Windows/MSRV jobs 均成功；Pages [32614979403](https://github.com/cty12356541/llmos/actions/runs/32614979403) 与 Schema fuzz smoke [32614979395](https://github.com/cty12356541/llmos/actions/runs/32614979395) 成功。该轮覆盖 `handle_for_ipc`、terminal rejection common semantics、metrics contract 与 coordinator restart-scan 的 workspace/conformance 回归。

## 验证与边界

既有 5 项 integration tests 覆盖脱敏 health、Capability 拒绝、caller/issuer 与 command/key 防替换、真实 framed submit/replay、ServiceDirectory negotiate 和 macOS/Unix endpoint round-trip；新增 bounded-failure mapping tests 与真实 Unix IPC denied-response test，覆盖分类、同 key durability 重试安全、相关性保留、脱敏 envelope 和无 effect evidence 的终止失败；Windows-only named-pipe round-trip 已由三平台 + MSRV CI 执行通过；Rust/TypeScript/Python schema conformance、workspace、Clippy `-D warnings` 与 fmt 通过。

本证据为单节点本地 H3 / `PARTIAL PASS`。`handle` 仍保留 transport-neutral `Result` 供调用方诊断，IPC caller 应使用 `handle_for_ipc` 以返回 typed rejection；Windows 测试验证成功/重放路径，Unix 测试已验证拒绝响应。测试 policy/peer authorizer 仍是受控 stub，尚未接真实 Capability authority、Principal-level peer attestation 或双向 peer policy；trusted-clock anti-replay、外部 metrics exporter、GUI/NL/CLI 多入口等价证明和批量控制仍未完成。

## 2026-08-29 增量验证：IPC challenge-response 握手最小前缀（ADR-0011）

> 状态：`PARTIAL PASS`（连接级 principal 认证设施落地，服务接线为后续切片）

### 已实现事实

1. **握手协议最小前缀（`crates/nlos-ipc/src/handshake.rs`）**：server 经 `HandshakeNonceRegistry::register` 登记一次性 32 字节 nonce 并下发 typed challenge；client 以 principal Ed25519 私钥对域分隔摘要签名应答——`principal_handshake_message` = `SHA-256("llmos/principal-handshake/v1" ‖ nonce ‖ principal_id ‖ channel_binding)`（镜像 `nlos-capability` 的 `COMMAND_MESSAGE_DOMAIN` 域分隔构造；channel binding 为双方约定的连接上下文字节，如 resolved endpoint 名）。server 端 `verify_attestation` 经 `IdentityAuthority::verify_capability_command_signature` 按 principal 当前 key binding 验签并核对当前 key generation；返回 `VerifiedPrincipalHandshake`（principal/control_domain/key_id/key_generation）。
2. **fail-closed 语义**：`PrincipalUnknown` / `KeyRevoked` / `KeyNotYetValid` / `KeyExpired` / `SignatureInvalid` / `NonceRejected` / `ChannelBindingMismatch` / `MalformedAttestation` / `Schema` 全部 typed、零 panic、零会话副作用；nonce 在第一次验证尝试即消费（单次有效，防重放与 retry oracle）；channel binding 以 server 本地期望为准、attestation 携带值不一致即拒（mismatch 在 nonce 消费之前拒绝，不烧毁诚实 client 的 nonce）。设施传输无关（`FramedIo::into_inner` 为 additive accessor，握手后把流交接给 `serve_one`/`LocalRpcClient`），服务接线由调用方完成。
3. **schema additive 登记第 7 项**：新增 `schema/nlos/sabi/v1/principal_handshake.proto`（`PrincipalHandshakeChallenge` / `PrincipalHandshakeAttestation`，package `nlos.sabi.v1`）；`nlos-schema` REGISTRY 追加 `nlos.sabi.PrincipalHandshake` v1.0（第 7 项）+ `SABI_PRINCIPAL_HANDSHAKE_SCHEMA`、`HANDSHAKE_NONCE_BYTES=32`、`HANDSHAKE_SIGNATURE_BYTES=64`、`MAX_HANDSHAKE_CHANNEL_BINDING_BYTES=256`、`MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES=4KiB` bound 与 4 个 additive `CompatibilityError` 变体；encode/decode 全部经 REGISTRY 校验 fail-closed（nonce/principal/signature 精确长度、binding 非空且 ≤256）。已冻结 6 项零字节改动：6 个既有 proto/golden/生成物文件零修改（gen/ 仅有 2 个新增 untracked 文件，无任何 `M`）。
4. **golden 与三语言**：新增 `schema/golden/nlos.sabi.PrincipalHandshake-v1.hex`（attestation canonical wire，185 字节）；Rust golden test 与 Python（`gen/python` protobuf 6.33.4）双向逐字节一致（encode == golden 且 golden decode 还原相等）；`buf generate` 重新生成 `gen/python`、`gen/typescript` 增量文件，重跑 deterministic（二次生成无 diff）。
5. **定向测试（`crates/nlos-ipc/tests/handshake.rs`，12 项全过）**：nonce registry bounded/one-time、正常握手回传当前 binding 四元组、unknown principal、revoked key、stale generation（非当前 key material 签名 → `SignatureInvalid`）、key validity 过期、错 nonce、篡改签名（且后续重放原签名仍 `NonceRejected`）、同 nonce 重放幂等拒绝、channel binding mismatch（不烧 nonce）、wire codec wrapper schema 拒绝；**真实 Unix socket roundtrip**：`UnixListenerAdapter` 真实 bind/accept → challenge/attestation framed 交换 → `IdentityAuthority` 验签 → 同一连接 `serve_one` 完成 authenticated Exchange echo。

### 验证（全部本机运行，macOS/darwin）

- `cargo test -p nlos-ipc`：通过（framing 7 + handshake 12 + doc 2）。
- `cargo test -p nlos-schema`：通过（20，含 registry=7 断言与 golden）。
- `cargo clippy -p nlos-ipc -p nlos-schema --all-targets -- -D warnings`：通过（0 error）。
- `cargo fmt -p nlos-ipc -p nlos-schema -- --check`：通过。
- `cargo check -p nlos-ipc --features conformance-server`：通过（feature 列表调整后 bins 仍编译）。
- `npm run schema:lint`（buf lint + buf format -d --exit-code）：通过。
- `npm run schema:typecheck`（tsc，覆盖 gen/typescript + sdk + conformance）：通过。
- `npm run schema:test:typescript`：通过；`python3 tests/conformance/schema/envelope.py`：通过（既有 TS/Python conformance 零回归）。
- `npm run schema:generate` 重跑 deterministic；Python 侧对新消息做 golden 逐字节断言通过。
- `npm run schema:check-generated`：**失败，且仅为预期内的 baseline 断言失败**——该脚本断言 `git status --porcelain -- gen` 为空，而本车道禁止 git 写操作、无法提交 2 个新增 untracked 生成物（`principal_handshake_pb2.py` / `principal_handshake_pb.ts`）；diff 显示 gen/ 仅有这 2 个 `??` 新文件、0 个已跟踪文件被修改，即冻结通道生成物零漂移。integrator 提交本车道变更后该门即通过。
- 未运行：TS/Python 侧针对新握手消息的 conformance 用例扩展（`tests/conformance/**` 不在本车道写集）；Windows named-pipe 上的握手 roundtrip（无 Windows 环境，既有平台先例由三平台 CI 背书）。

### 已知限制

1. **服务接线为后续切片**：SystemControl / TakeoverControl / WaitControl 三个服务的连接入口仍使用现有 `PeerAuthorizer` 占位，未替换为真实 challenge-response 握手 authorizer；本切片只交付可复用握手设施与 wire 契约。
2. **AuthorityClock 未接入**：validity 时间语义仍由调用方传入 `verified_at_ms`（`nlos-identity` 现行 API 形态）；AuthorityClock 由并行车道独立实现，其落地后再统一接入。
3. **stale generation 的覆盖形态**：`IdentityAuthority` 当前为 bootstrap 单 key 模型（无独立 rotate-to-new-key API），key generation 只经 revocation 前进；「stale generation」以 revoked（generation 前进后旧 key 一律 `KeyRevoked`）与非当前 key material 签名（`SignatureInvalid`）两类测试覆盖。
4. **握手复用 SemanticSigning key purpose**：`verify_capability_command_signature` 校验 `KeyPurpose::SemanticSigning`；专用 handshake key purpose 需改 `nlos-identity`（本车道写集外），暂复用 principal 语义签名密钥。
5. **channel binding 推导为调用方接线**：设施只强制「非空、有界、双端一致、入签」。各传输的 binding 推导策略（如 Unix peer credential 元组编码）由接线切片补充；测试以 resolved endpoint 路径为 binding。

## 2026-08-30 增量验证：握手传输接线设施（ADR-0011 决定 1 的传输闭环）

> 状态：`PARTIAL PASS`（可复用「认证后服务/认证后连接」封装落地；三服务接线为波次 3）

### 已实现事实

1. **API（`crates/nlos-ipc/src/handshake/transport.rs`，全部 `#[cfg(unix)]`）**：`endpoint_channel_binding(path)` 以域分隔 `SHA-256("llmos/ipc-channel-binding/v1" ‖ endpoint path bytes)` 派生定长 32 字节 channel binding（任意路径长度均满足 binding 边界，双端传同一 resolved path 即一致）；`ServerHandshakeContext::new(endpoint, nonce_capacity)` 持有派生 binding + `HandshakeNonceRegistry` 的服务端薄封装；`authenticated_serve_one(listener, config, identity, nonces, endpoint_binding, authorizer, handler, next_nonce, verified_at_ms)` = accept → OS 凭证 pre-gate → 发 challenge wire → 收 attestation wire → `verify_attestation` → 通过后进既有 `serve_one` 语义；`authenticated_connect(path, config, principal, sign)` = connect → 收 challenge → `principal_handshake_message` 摘要经 caller 注入 `sign` 闭包签名 → 发 attestation → 返回 `FramedIo`（可直接接 `LocalRpcClient`/`serve_one`）。签名走回调而非新增 ed25519 主依赖（`nlos-ipc` 现有 deps 无 ed25519；测试经既有 dev-dep `ed25519_dalek` 提供 signer）。
2. **语义决定**：(a) **nonce 消耗时机**——`verify_attestation` 在验签之前消费一次性 nonce，消费后不返还 registry：握手失败的 nonce 按设计烧毁，被捕获的 attestation 无法对同一 nonce 重放（channel-binding mismatch 是唯一在消耗前拒绝的路径，不烧诚实 client 的 nonce，行为与原语层一致）；(b) **绑定派生**——两端各自从 endpoint 路径派生，server 只核对其本地期望值，attestation 携带值不一致即 `ChannelBindingMismatch`；(c) **pre-gate 双跑**——authorizer 在握手前先跑一次（错 peer 零副作用拒绝、challenge 未发出）且 `serve_one` 内部原语义不变（第二次幂等 authorize）；(d) **握手期越界帧**——任何非合法 attestation 帧走 `decode_attestation_wire` → typed `HandshakeError::Schema` 拒绝；(e) **`HandshakeError` 新增 2 个平台无关变体** `PeerAuthorization(String)` 与 `Transport(IpcError)`（framing/I/O/超时在握手期的 typed 包装），非 unix 编译面零新增 item。
3. **测试矩阵（`crates/nlos-ipc/tests/handshake_transport.rs`，11 项全过，`#![cfg(unix)]`）**：binding 派生确定性/有界；ServerHandshakeContext 派生与 registry 容量；**真实 Unix socket 全链**（真 bind/accept → `authenticated_connect` 挑战应答 → `IdentityAuthority` 真验签返回当前 key generation 四元组 → 同连接 authenticated Exchange echo + nonce 已消费断言）；坏签名（全零签名 → `SignatureInvalid` + server 关闭连接 client 观测 EOF + nonce 已烧毁）；**attestation 重放**（conn1 捕获 attestation 字节 → conn2 fresh nonce 下原样重放 → `NonceRejected`，client 观测连接关闭）；channel-binding 不匹配（`ChannelBindingMismatch` 且 nonce 未烧毁、仍可消费）；未知 principal（`PrincipalUnknown`）；握手期越界帧（合法 `ExchangeRequest` 提前注入 → `Schema`）；pre-gate 拒绝（`PeerAuthorization` + 挑战从未发出：nonce 从未登记）；静默 client 超时（`Transport(Timeout(Read))` fail-closed）；served 阶段 handler 失败（`AuthenticatedServeOutcome` 同帧携带 verified 身份与 `Err(ServiceFailure)`，client 按 `serve_one` 既有语义观测 EOF）。
4. **Windows/cfg 纪律**：传输集成整体 `#[cfg(unix)]`（模块声明级）；测试文件整文件 `#![cfg(unix)]`；非 unix 编译面唯一新增是 `HandshakeError` 两个变体（`Display`/`source` 无条件臂引用，无 dead-code/unused 风险）。

### 验证（全部本机运行，macOS/darwin，stable 1.97.0）

- `cargo test -p nlos-ipc`：**32 passed / 0 failed**（lib 0 + framing 7 + handshake 12 + handshake_transport 11 + platform 2 + doc 0）。
- `cargo clippy -p nlos-ipc --all-targets -- -D warnings`：通过（0 error）。
- `cargo +nightly-2026-08-01 clippy -p nlos-ipc --all-targets -- -D warnings`：通过（0 error，防新 lint）。
- `cargo fmt -p nlos-ipc` 后 `cargo fmt -p nlos-ipc -- --check`（stable）：通过；`cargo +nightly-2026-08-01 fmt -p nlos-ipc -- --check`：通过。
- 未运行：`cargo check -p nlos-ipc --target x86_64-pc-windows-msvc`——本机交叉编译在 `libsqlite3-sys`（nlos-identity 依赖链）build script 失败，属 macOS host 交叉环境限制，非本车道 cfg 缺陷；Windows 编译面按先例由三平台 + MSRV CI 背书。named-pipe 握手传输（Windows 面）本切片显式不做。

### 已知限制

1. **三服务接线为波次 3**：SystemControl / TakeoverControl / WaitControl 的连接入口尚未替换为 `authenticated_serve_one`/`authenticated_connect`；本切片只交付可复用传输封装（ADR-0011 决定 1 的传输闭环，不含决定 2 命令级签名贯穿的接线消费）。
2. **AuthorityClock 未接入**：`verified_at_ms` 仍由调用方传入；接入由 W2-E 车道（nlos-clock）落地后统一收口。
3. **nonce 熵为调用方注入**：`authenticated_serve_one` 的 `next_nonce: FnMut() -> [u8; 32]` 刻意不带随机数依赖，生产接线必须注入 OS 级 RNG；测试以确定性序列生成器验证协议语义。registry 只防「同一 nonce 值 outstanding 重复」与「单次消费」，nonce 值复用（随机性不足）不在设施防御面。
4. **签名回调不可失败语义以 typed error 表达**：`sign: Fn(&[u8; 32]) -> Result<[u8; 64], HandshakeError>`；HSM 类可失败 signer 可直接映射，无需 panic。
5. **served 阶段失败无 failure response**：镜像既有 `serve_one` 语义（handler 错误直接返回、不向 peer 发送 `SabiFailure`），认证后连接的失败响应策略仍属服务接线切片。

## 2026-08-31 增量验证：多入口 parity 收口与全量生产 IPC caller 勘察

> 状态：`PASS`（B-TASK-006L 进度单遗留三项之「多入口 parity」「全量生产 IPC caller 迁移」收口；「metrics exporter」已于 16c0fc0 完成）

### 勘察清单（全仓 `serve_one`/`handle`/`handle_for_ipc` 使用点分类）

**A. 生产服务端点（lib/auth 生产路径）——全部已走 typed `handle_for_ipc` 失败面，零迁移必要：**

1. `nlos-system-control`：`RecoverySystemControl::handle_for_ipc`（lib.rs）；`dispatch_in_process`（control.rs，in-process 入口 → `handle_for_ipc`）；`dispatch_over_socket`（cli 客户端，服务侧同一 handler）；`authenticated_serve_one_control` → `serve_validated` → `control.handle_for_ipc`（auth.rs，认证入口，失败前置契约违规亦走 `failure_envelope`）。
2. `nlos-wait-control`：`WaitControlService::handle_for_ipc`（lib.rs）；`AuthenticatedWaitControlServer::serve_one` → `service.handle_for_ipc`（authenticated.rs）；`AuthenticatedWaitControl::handle_for_ipc` 显式复用 plain service 的 typed 拒绝词表（「never invents a second rejection vocabulary」）。
3. `nlos-takeover-control`：`TakeoverControl::handle_for_ipc`（lib.rs）；`AuthenticatedTakeoverControlServer::serve_one` → `control.handle_for_ipc`（authenticated.rs）。

**B. conformance 测试专用（零改动，CI 依赖）：**

1. `nlos-wait-control/src/bin/wait-control-conformance.rs`：bare `serve_one`，但 handler 内部已走 `WaitControlService::handle_for_ipc`（等价合规；按纪律不动）。
2. `nlos-takeover-control/src/bin/takeover-control-conformance.rs`：bare `serve_one` + 裸 `.handle` + 手工 `failure_envelope`（与 `handle_for_ipc` 语义等价：`handle_for_ipc` 即 `handle` + `failure_envelope`；含 crash-injection 钩子故不迁移；按纪律不动）。

**C. 集成测试 harness（不动）：** 各 `tests/` 内 `serve_one`/`serve_forever`（control_command_cli、recovery_control、wait_control、takeover_control、authenticated_*、windows_named_pipe、nlos-ipc platform/framing/handshake*）均为测试自建 harness。

**D. 写集外发现（如实登记、不修改）：**

1. `nlos-ipc/src/bin/nlos-ipc-echo.rs`、`nlos-directory-chain.rs`：IPC 设施 demo bin（bare `serve_one`），非控制面生产端点，且 `nlos-ipc` 为本任务禁改 crate。
2. 本地工作区存在他车道未提交变更（`crates/nlos-artifact/**` 8 文件、`crates/nlos-schema/**` 2 文件等）；本地 stable 1.97.1 pedantic clippy 会对其报 `manual_is_multiple_of` 警告（HEAD 4a1cb2a CI 绿，属本地工具链/在途变更漂移，非本车道写集，未触碰）。

### 迁移与 parity 变更摘要

1. **生产 caller 迁移**：勘察结论为**已完成态、零迁移改动**——三控制服务的全部生产入口（in-process / plain-IPC 服务侧 / authenticated 服务侧）在既有提交（d005f15/8d3da50/885ef23 等）中已收敛到 `handle_for_ipc` 单一 typed 失败面；本切片不发明新语义、不重构。
2. **多入口 parity 测试补缺**（`crates/nlos-system-control/tests/control_ipc_auth.rs`，+195 行，唯一写集变更）：
   - 既有覆盖：in-process ↔ plain-socket ↔ CLI 成功 receipt 字节一致 + denied 失败一致（`control_command_cli.rs::cli_and_in_process_paths_produce_byte_identical_receipts`）；authenticated 仅独立 roundtrip/失败形态，**无跨入口同请求比对——此为补的缺口**。
   - 新增 `spawn_plain_entry`（plain 入口 3 连接 harness，`handle_for_ipc` 投影）与 `dispatch_all_entries`（同一命令依次经 in-process / plain-IPC / authenticated 三入口）；
   - 新增 `same_command_receipts_are_identical_across_in_process_plain_and_authenticated_entries`：三命令 × 三入口矩阵——InspectHealth（成功）、InspectTask 缺失目标（typed `NotFound` + `DO_NOT_RETRY`）、AcknowledgeRecoveryAlert policy 拒绝（typed `Rights` + `DO_NOT_RETRY` + 脱敏 safe_message）——断言同命令跨入口 receipt 字节级一致；并断言 denied ack 三入口零 durable 副作用。`ControlReceipt` 不含 wall 时间戳（故障在策略/CAS 之前返回），故 authenticated 入口的 clock-issued wall 与 caller-supplied wall 不影响字节一致。

### 验证（全部本机运行，macOS/darwin；stable 1.97.1 + nightly-2026-08-01）

- `cargo test -p nlos-system-control`：**52 passed / 0 failed**（lib 16 + cli 4 + control_ipc_auth **9**（含新增 parity）+ metrics_export 3 + openmetrics 7 + recovery_control 7 + failure_mapping 5 + windows_named_pipe 0（macOS）+ doc 1）。
- `cargo test -p nlos-wait-control`：13 passed / 0 failed；`cargo test -p nlos-takeover-control`：10 passed / 0 failed；`cargo test -p nlos-ipc`：32 passed / 0 failed（勘察涉及 crate 定向回归，零改动零回归）。
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过；`cargo +nightly-2026-08-01 clippy … -D warnings`：通过（双工具链 0 warning）。
- `cargo fmt -p nlos-system-control -- --check`：通过；`cargo +nightly-2026-08-01 fmt -p nlos-system-control -- --check`：通过。
- 未运行：Windows named-pipe 面（本机 macOS，按先例由三平台 + MSRV CI 背书；`dispatch_over_socket` 已按既有 `#[cfg(all(unix, feature = "cli"))]` 模式门控）；`--no-default-features` 编译面（新测试与既有 socket 测试同样依赖 cli feature 门控）；`--workspace` 全量（本车道禁用，定向覆盖如上）。

### 遗留项更新

B-TASK-006L 进度单遗留原文「metrics exporter、多入口 parity、全量生产 IPC caller 迁移」三项现状：

1. **metrics exporter**：已完成（16c0fc0，见 metrics evidence）。
2. **多入口 parity**：本切片完成——三入口（in-process/plain-IPC/authenticated）同请求 receipt 字节级一致已由测试固化。
3. **全量生产 IPC caller 迁移**：勘察确认**生产面收敛已完成**（先于本切片的提交已把三服务全部生产入口收敛到 `handle_for_ipc`）；bare `serve/handle` 残留仅在 conformance bin、IPC 设施 demo bin 与测试 harness（均为设施/测试专用，按 CI 冻结纪律不动）。**结论：生产面收敛完成，无生产 caller 遗留**；NL 前缀已接（b-control-003），GUI/批量控制等非本遗留项内容仍按进度单原口径另行跟踪。
