# ADR-0003：阶段 B IDL、兼容信封与 canonical encoding

- 状态：POC
- 日期：2026-08-02
- Owner：待指定
- 关联 Requirement：`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`、`SABI-DISC-001`、`SABI-TRANSPORT-001`、`TYPE-GEN-001`
- 复审触发器：任一官方 SDK 候选的生成/运行语义出现漂移；unknown field 无法安全转发；Protobuf/CBOR 无法表达 required compatibility；解析资源上限或 fuzz 出现反例；本地 IPC 迫使 schema 绑定单一 transport

## 上下文

阶段 B 已有 Rust nominal types、Operation authority 和 durable Outbox，但它们仍是进程内 Rust 契约。后续 Process、Task、Control 和第三方 Application 需要跨语言、跨进程且可演进的 SABI。若 schema 没有显式 identity/version/extension policy，旧客户端可能错误接受未知强制语义；若中间转发层 decode 后重新 encode，生成器未保留的 unknown field 又可能静默丢失。

签名、授权和 Receipt 的 canonical object 还有更严格的“同一语义只有一种字节表示”要求，不能把普通 RPC 编码的便利性冒充 deterministic signing format。

## 候选

| 候选 | 优点 | 主要风险 |
|---|---|---|
| Protobuf + Buf | 三语言生态成熟，字段编号和 breaking check 工具清晰 | canonical signing 不自然；不同生成器的 unknown-field round-trip 行为不同 |
| Cap'n Proto | 可减少部分复制，schema evolution 能力明确 | 多语言生态、构建与安全解析成本需额外验证 |
| FlatBuffers | 大型只读对象访问高效 | mutation、普通 RPC 和演进复杂度不适合当前首切片 |
| JSON/CBOR | 可读性或紧凑性好；CBOR 可用于 canonical object | 单独承担 RPC 生成生态较弱；canonical profile 必须严格限定 |

## 当前决定

维持技术选型中的双轨候选，不冻结公共 ABI：

1. service/RPC schema 采用 **Protobuf + Buf 候选**；首个 PoC 由 `schema/nlos/sabi/v1/envelope.proto` 作为唯一 IDL 源，`nlos-schema` 在构建期使用 `prost-build` 生成 Rust 类型；
2. signed/canonical object 采用 **RFC 8949 core deterministic CBOR 候选**；`nlos-canonical` 已实现首个严格 digest envelope/profile，任何 Protobuf bytes 都不得被当作签名 canonical bytes；
3. 每个公共 frame 显式携带 schema name、major、minor、critical extension IDs 和 non-critical extension IDs；
4. unknown major 与 unknown critical extension fail-closed；更高 minor 和 unknown non-critical extension可被当前 consumer 接受；
5. `ValidatedFrame` 同时保留 typed view 和原始 wire bytes。转发 hop 必须转发原始 frame，禁止通过 decode/re-encode 假装保留生成器未知字段；
6. envelope minor=0 只承载 128-bit exchange request ID、service、method 与 opaque payload；B-SCHEMA-009 以 additive minor=1 candidate 加入 request/response common context，区分 exchange request ID、correlation ID 与 IdempotencyKey，并承载 caller/fence、deadline、Capability、Operation/Receipt reference 和 typed failure。service-specific payload 仍保持 opaque；common context 尚未冻结为稳定 ABI。
7. Buf 1.72.0 负责 lint、breaking 与跨语言生成编排；TypeScript 固定 protobuf-es 2.13.0，Python 固定 generator v33.4/runtime 6.33.4。生成物 checked in，`.gitattributes` 强制跨平台 LF，CI 必须重生成并拒绝 drift。
8. canonical signing preimage 固定为 `u32_be(domain_len) || ASCII domain || u32_be(cbor_len) || deterministic_cbor(body)`；v1 digest algorithm 固定 `SHA-256`。CBOR map 使用最短 unsigned integer key 严格升序，禁止 duplicate、indefinite、tag、float/NaN、simple value、negative integer和自由 Unicode text；decoder 必须重编码逐字节比对。
9. 首个 local RPC 使用 `LocalRpcService.Exchange` 和独立 request/response wrapper；wire framing 为 `u32_be length || protobuf wrapper`，平台层候选为 Unix socket/Windows named pipe。service schema 不绑定 gRPC/HTTP2 或 endpoint，Rust generated client trait 与平台 stream adapter 分离。

## 约束

- schema registry 是兼容检查入口；不得按固定网络地址、实现语言或 Rust 类型名发现服务；
- frame 在解析前执行 1 MiB 上限，公共 request ID 固定 16 bytes；后续每种 service payload 还需更严格的独立上限；
- non-critical 可忽略不等于可丢弃：透明 forwarding 必须保持输入 wire bytes；
- critical extension ID 只能在实现、测试和协商支持同时存在时加入 registry；
- 三语言 Protobuf generation/compat、首轮 sanitizer fuzz smoke、Unix/Windows typed IPC client、ServiceDirectory 两跳调用与 common metadata/safe-retry 校验已通过局部验证；durable dedup/result、真实 IPC 重连与 server restart 已有三平台证据，deadline/cancel durable 状态机本地通过。但独立 Operation control payload、Receipt authority、CBOR 跨语言和长期 fuzz 未完成前，ADR 保持 `POC`；
- `nlos-types` 继续不依赖 Protobuf；wire adapter 负责在 nominal ID 与生成类型之间显式转换，避免 wire bytes 侵入内核对象身份。
- SDK 语言按[多语言 SDK 支持评估计划](../language-sdk-support-plan.md)逐级晋升；Go/C# 当前只是 P1 评估候选，生成类型或 descriptor 不构成正式支持声明。

## 依赖审查

截至 2026-08-02：

- `prost 0.14.4` 与 `prost-build 0.14.4` 为 Apache-2.0，最低 Rust 1.85；本仓库以 Cargo.lock 固定版本；
- `protoc-bin-vendored 3.2.0` 为 MIT，用于让 macOS/Windows/Linux 构建不依赖宿主预装 `protoc`；代价是引入各目标平台的编译器二进制包并扩大供应链面；
- Buf CLI 1.72.0 为 Apache-2.0；`@bufbuild/protobuf 2.13.0` 为 Apache-2.0/BSD-3-Clause，TypeScript/Node 工具链由 `package-lock.json` 固定；Python runtime `protobuf 6.33.4` 为 BSD-3-Clause；
- TypeScript/Python remote plugin 固定完整版本，但重新生成仍依赖 BSR 可用性；checked-in 生成物让普通 consumer/build 不依赖在线生成，后续仍需评估 mirror 与 provenance；
- `minicbor 2.3.0` 为 BlueOak-1.0.0，只作为可替换的 CBOR primitive codec；NLOS profile 自行执行 map/type/order/size/domain/compat 与 re-encode byte equality 检查；
- `cargo-fuzz 0.13.2` + `libfuzzer-sys 0.4.13` 只进入独立 `fuzz/` package；CI 固定 nightly `2026-08-01` 并执行 Linux AddressSanitizer smoke，不进入普通 workspace 或生产依赖；
- `tokio 1.53.1` 提供 Unix socket、peer credential 与 Windows named-pipe async stream；`windows-sys 0.61.2` 只在 Windows IPC adapter 使用固定错误码和 identification QoS 常量，不进入 schema 或 `nlos-types`；
- `sha2 0.11.0`（MIT OR Apache-2.0，MSRV 1.85）只进入 feature-gated conformance server 的可信 request adapter，用于计算实际 SHA-256 payload identity；它不进入 wire/durable ABI，正式 service 仍必须定义 canonical effect fields；
- 上述依赖只进入可替换 schema/build adapter，不进入 Safety TCB、KABI 或 `nlos-types`；升级必须重跑 golden、compat、三平台 CI 和后续 fuzz corpus。

## 首切片验收

1. Rust 类型从 `.proto` 构建生成，schema 源变化会触发重新生成；
2. canonical golden vector 的 encode/decode 精确匹配；
3. unknown major 和 unknown critical extension 被拒绝；
4. 更高 minor 与 unknown non-critical extension被接受；
5. unknown Protobuf field 可由 forwarding API 原字节保留；
6. 缺失 identity、错误 request ID、空 service/method、malformed/oversized frame fail-closed；
7. workspace test、rustfmt 与 Clippy `-D warnings` 通过。

## 迁移与退出策略

`nlos.sabi.Envelope` 与 `nlos.canonical.DigestEnvelope` 当前均为 v1 PoC，未对外冻结。若 Protobuf/Prost 不能满足三语言或 unknown-field 需求，可保留 `SchemaIdentity`、兼容规则与 golden 语义，替换生成器或 RPC encoding；若 CBOR library 被替换，必须保持 canonical body/preimage golden 与全部反例逐字节一致。任何字段/排序/类型/domain/hash 不兼容变化不得复用 major=1 或旧 domain tag。

## 当前证据

[B-SCHEMA-001](../../evidence/stage-b/b-schema-001-protobuf-envelope.md) 已通过 Rust generation、7 项 compatibility/golden 测试、本地 workspace 回归，以及 [GitHub Actions run 30715148293](https://github.com/cty12356541/llmos/actions/runs/30715148293) 的 Ubuntu/Windows/macOS 复验。它只支持首个公共 envelope，不证明 TypeScript/Python client、Buf breaking check、deterministic CBOR、fuzz 或本地 typed IPC 已完成。

[B-SCHEMA-002](../../evidence/stage-b/b-schema-002-cross-language-generation.md) 已通过 TypeScript/Python generation、golden conformance、生成物 drift gate、Buf lint/format、删除字段 breaking 反例，以及 [GitHub Actions run 30715954413](https://github.com/cty12356541/llmos/actions/runs/30715954413) 的 Ubuntu/Windows/macOS 复验。由于当前 IDL 没有 RPC service，本证据只声称 type bindings，不声称 service client 已生成。

[B-SCHEMA-003](../../evidence/stage-b/b-schema-003-deterministic-cbor.md) 已通过 deterministic CBOR body、domain-separated preimage、两个 golden vectors、13 项严格反例测试，以及 [GitHub Actions run 30716908874](https://github.com/cty12356541/llmos/actions/runs/30716908874) 的 Ubuntu/Windows/macOS 复验。该证据不包含实际 SHA-256、签名、key management 或完整 Receipt/Event/Escrow schema。

[B-SCHEMA-004](../../evidence/stage-b/b-schema-004-schema-fuzz-smoke.md) 已建立 Protobuf envelope、canonical CBOR body 和 signing preimage 三个有界 sanitizer fuzz target。本地 33 秒共执行 15,499,860 次，无 crash/timeout/OOM/断言反例；[Linux fuzz run 30717749638](https://github.com/cty12356541/llmos/actions/runs/30717749638) 与[三平台回归 run 30717749643](https://github.com/cty12356541/llmos/actions/runs/30717749643) 均成功。该短跑不替代长期 fuzz，也不构成 production parser claim。

[B-SCHEMA-005](../../evidence/stage-b/b-schema-005-local-typed-ipc.md) 已加入最小 unary service、Rust generated client trait、TS/Python service descriptor、transport-neutral bounded framing/client/server、Unix socket 和 Windows named-pipe adapter。[三平台 run 30730221706](https://github.com/cty12356541/llmos/actions/runs/30730221706) 已通过真实平台测试与整仓 gate；TS/Python transport client、ServiceDirectory/negotiation、完整 common semantics、Windows token/ACL 和生产压力仍未完成。

[B-SCHEMA-006](../../evidence/stage-b/b-schema-006-typescript-python-ipc-clients.md) 已加入 TypeScript/Node 与 Python asyncio candidate client；[三平台 run 30734744799](https://github.com/cty12356541/llmos/actions/runs/30734744799) 通过两种语言调用 Rust conformance server 的真实 Unix socket/Windows named-pipe 往返、compatibility preflight、backpressure 和 unavailable endpoint 测试。ServiceDirectory/common semantics、双向 peer auth、Python Proactor 稳定 profile 和 SDK 发布仍未完成。

[B-SCHEMA-007](../../evidence/stage-b/b-schema-007-service-directory-negotiation.md) 已加入 `nlos.sabi.ServiceDirectory` v1.0 schema、三语言 generation/golden、独立 64 KiB bound，以及 Rust `SnapshotDirectory` 的确定性 resolve/negotiate 与 typed failure；[三平台 run 30735589673](https://github.com/cty12356541/llmos/actions/runs/30735589673) 和 [fuzz regression 30735589675](https://github.com/cty12356541/llmos/actions/runs/30735589675) 已成功。真实目录 IPC server、TS/Python resolver、watch/lease、Capability/common SABI 和 peer auth 仍未完成。

[B-SCHEMA-008](../../evidence/stage-b/b-schema-008-cross-language-directory-chain.md) 已加入 feature-gated Rust directory/business 双 endpoint fixture，以及 TypeScript/Python `bootstrap → negotiate → connect service → typed exchange` 两跳链路；[三平台 run 30736741324](https://github.com/cty12356541/llmos/actions/runs/30736741324) 已通过 Unix socket/Windows named-pipe 组合。Namespace bootstrap authority、生产目录生命周期、peer auth 和 common SABI 仍未完成。

[B-SCHEMA-009](../../evidence/stage-b/b-schema-009-common-sabi-semantics.md) 已以 Envelope minor=1 candidate 加入 caller/task fence、独立 correlation/idempotency、deadline、Capability、Operation/Receipt reference、19 类 common error 与 safe retry directive；[三平台 run 30737782776](https://github.com/cty12356541/llmos/actions/runs/30737782776) 与 [fuzz run 30737782772](https://github.com/cty12356541/llmos/actions/runs/30737782772) 已成功。

[B-SCHEMA-010](../../evidence/stage-b/b-schema-010-durable-idempotency-result.md) 已为 common IdempotencyKey 增加 SQLite v3 authority：首次 scoped key claim 与 Operation 注册原子提交，terminal result 与 Receipt/Outbox 原子提交，相同 key/digest 可在重启后回放稳定 service result，不同 digest 冲突；[三平台 run 30738888761](https://github.com/cty12356541/llmos/actions/runs/30738888761) 已成功。

[B-SCHEMA-011](../../evidence/stage-b/b-schema-011-durable-idempotency-ipc.md) 已完成真实 SABI 接线，并明确 durable `result_wire` 与每次 exchange 重建的 request ID/correlation/envelope 分层；TS/Python commit 后断线重连、conflict 和 `E_UNCERTAIN` 已由[三平台 run 30740180511](https://github.com/cty12356541/llmos/actions/runs/30740180511) 验证。server process restart + directory renegotiation + SQLite reopen 组合又由[三平台 run 30741046472](https://github.com/cty12356541/llmos/actions/runs/30741046472) 验证；完整 deadline/cancel 生产链、Receipt authority 和 peer auth 仍未完成。

[B-SCHEMA-012](../../evidence/stage-b/b-schema-012-deadline-cancel-state-machine.md) 已把 deterministic host-monotonic deadline checkpoint 和 cancel fence 接入 durable Operation：dispatch 前返回 no-effect `E_DEADLINE/E_CANCELLED`，dispatch 后迟到 callback 返回 `E_PARTIAL/E_EFFECT_UNKNOWN` 并进入 reconcile；本地 TS/Python IPC 通过，远程三平台待验证。fixture method 尚不能替代正式 Operation query/cancel schema。
