# ADR-0003：阶段 B IDL、兼容信封与 canonical encoding

- 状态：POC
- 日期：2026-08-02
- Owner：待指定
- 关联 Requirement：`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`、`SABI-DISC-001`、`SABI-TRANSPORT-001`、`TYPE-GEN-001`
- 复审触发器：三语言生成结果出现语义漂移；unknown field 无法安全转发；Protobuf/CBOR 无法表达 required compatibility；解析资源上限或 fuzz 出现反例；本地 IPC 迫使 schema 绑定单一 transport

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
6. envelope 先只承载 128-bit request ID、service、method 与 opaque payload。service-specific payload、Capability、deadline、Operation/Receipt 等字段只有进入 registry 并取得兼容证据后才能成为稳定表面。
7. Buf 1.72.0 负责 lint、breaking 与跨语言生成编排；TypeScript 固定 protobuf-es 2.13.0，Python 固定 generator v33.4/runtime 6.33.4。生成物 checked in，`.gitattributes` 强制跨平台 LF，CI 必须重生成并拒绝 drift。
8. canonical signing preimage 固定为 `u32_be(domain_len) || ASCII domain || u32_be(cbor_len) || deterministic_cbor(body)`；v1 digest algorithm 固定 `SHA-256`。CBOR map 使用最短 unsigned integer key 严格升序，禁止 duplicate、indefinite、tag、float/NaN、simple value、negative integer和自由 Unicode text；decoder 必须重编码逐字节比对。

## 约束

- schema registry 是兼容检查入口；不得按固定网络地址、实现语言或 Rust 类型名发现服务；
- frame 在解析前执行 1 MiB 上限，公共 request ID 固定 16 bytes；后续每种 service payload 还需更严格的独立上限；
- non-critical 可忽略不等于可丢弃：透明 forwarding 必须保持输入 wire bytes；
- critical extension ID 只能在实现、测试和协商支持同时存在时加入 registry；
- 三语言 Protobuf generation/compat 已通过，但 CBOR 跨语言、fuzz、本地 transport adapter 未完成前，ADR 保持 `POC`；
- `nlos-types` 继续不依赖 Protobuf；wire adapter 负责在 nominal ID 与生成类型之间显式转换，避免 wire bytes 侵入内核对象身份。

## 依赖审查

截至 2026-08-02：

- `prost 0.14.4` 与 `prost-build 0.14.4` 为 Apache-2.0，最低 Rust 1.85；本仓库以 Cargo.lock 固定版本；
- `protoc-bin-vendored 3.2.0` 为 MIT，用于让 macOS/Windows/Linux 构建不依赖宿主预装 `protoc`；代价是引入各目标平台的编译器二进制包并扩大供应链面；
- Buf CLI 1.72.0 为 Apache-2.0；`@bufbuild/protobuf 2.13.0` 为 Apache-2.0/BSD-3-Clause，TypeScript/Node 工具链由 `package-lock.json` 固定；Python runtime `protobuf 6.33.4` 为 BSD-3-Clause；
- TypeScript/Python remote plugin 固定完整版本，但重新生成仍依赖 BSR 可用性；checked-in 生成物让普通 consumer/build 不依赖在线生成，后续仍需评估 mirror 与 provenance；
- `minicbor 2.3.0` 为 BlueOak-1.0.0，只作为可替换的 CBOR primitive codec；NLOS profile 自行执行 map/type/order/size/domain/compat 与 re-encode byte equality 检查；
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

[B-SCHEMA-003](../../evidence/stage-b/b-schema-003-deterministic-cbor.md) 已在本地通过 deterministic CBOR body、domain-separated preimage、两个 golden vectors 和 13 项严格反例测试；workspace 与三平台 CI 待本提交后补记。该证据不包含实际 SHA-256、签名、key management 或完整 Receipt/Event/Escrow schema。
