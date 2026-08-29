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
