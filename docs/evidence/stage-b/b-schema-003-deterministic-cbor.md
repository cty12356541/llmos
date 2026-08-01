# B-SCHEMA-003：Deterministic CBOR 与签名域初始证据

> 状态：PARTIAL PASS
>
> 日期：2026-08-02
>
> 对应：`COMPAT-VER-001`、`COMPAT-VER-002`、`TYPE-GEN-001`、`SEM-CANON-001`、`RESCROW-CANON-001`

## 1. 实现范围

新增独立 `nlos-canonical` crate 与 `schema/nlos/canonical/v1/digest-envelope.cddl`。它不依赖 Protobuf，也不允许把 RPC bytes 当作可签名字节。

首个 PoC 对象 `nlos.canonical.DigestEnvelope` 使用 RFC 8949 core deterministic CBOR 的严格子集：

| 整数 key | 字段 | 约束 |
|---:|---|---|
| 0 | schema name | 固定 ASCII `nlos.canonical.DigestEnvelope` |
| 1 | major | 固定 1；未知 major 拒绝 |
| 2 | minor | 本地产生 0；更高 minor 可保留并 round-trip |
| 3 | payload digest algorithm | 固定 `sha-256`，防算法冒充 |
| 4 | object ID | 16-byte `CanonicalObjectId`，与 digest 不可类型互换 |
| 5 | payload digest | 32-byte `Sha256Digest`；本切片不负责计算 digest |
| 6 | critical extensions | `uint → bstr`，最多 16 项，每值最多 256 bytes；unknown ID 拒绝 |
| 7 | noncritical extensions | 同样有界；unknown ID 作为 opaque bytes 保留 |

完整 CBOR body 上限 4096 bytes。所有 map 必须 definite-length、key 严格按最短 unsigned encoding 升序、无重复；禁止 tag、float/NaN、simple value、negative integer、indefinite item 和任意额外顶层字段。schema/algorithm/domain 只允许固定或受限 ASCII，因此不引入 Unicode normalization 多表示问题。

decoder 在结构和兼容检查后重新编码，并要求输出与输入逐字节相等；因此非最短整数、非确定性长度或其他“语义可解但字节不 canonical”的输入仍 fail-closed。

## 2. 签名域与 SHA-256 边界

对齐 v0.5 的 `H(domain || deterministic_cbor(body))`，签名/ID preimage 固定为：

```text
u32_be(domain_len)
  || ASCII domain
  || u32_be(cbor_body_len)
  || canonical_cbor_body
```

verifier 必须调用 `decode_signing_preimage_for_domain(expected_domain, ...)`；仅解 CBOR body 不构成有效签名验证。domain 仅允许 `[a-z0-9._/-]` 且不超过 96 bytes。CBOR body 内固定声明 `sha-256`，但本 crate 暂不执行 SHA-256、密钥解析或签名算法验证；调用方必须按 v0.5 对完整 preimage 计算 SHA-256。

## 3. 依赖与替换边界

- `minicbor 2.3.0`，BlueOak-1.0.0；只用于有界 CBOR primitive encode/decode；
- deterministic 规则、字段允许集合、duplicate/order/size/domain/compat 检查由 `nlos-canonical` 显式实现，不依赖通用 decoder 的宽松默认行为；
- crate 不进入 `nlos-types`，不处理 key store、signature、Receipt authority 或业务状态；
- 若替换 CBOR library，必须保持两个 golden vectors、全部反例和三平台结果逐字节一致；不兼容变化必须提升 schema major/domain tag。

## 4. Golden vectors

- `schema/golden/nlos.canonical.DigestEnvelope-v1.hex`：128-byte deterministic CBOR body；
- `schema/golden/nlos.canonical.DigestEnvelope-preimage-v1.hex`：带长度前缀 ASCII domain 的完整签名/哈希 preimage。

golden fixture 包含一个 critical extension 和两个 noncritical extensions，用于同时验证 fail-closed 与 opaque round-trip。

## 5. 测试与复现

```sh
cargo test -p nlos-canonical
cargo clippy -p nlos-canonical --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

新增 13 项测试：

1. CBOR body 与 signing preimage 精确匹配 checked-in golden，并可 round-trip；
2. expected domain mismatch 与非 ASCII domain 拒绝；
3. unknown critical 拒绝，unknown noncritical 原字节保留；
4. producer 不能创建重复/乱序 extension map；
5. decoder 拒绝重复/乱序顶层 key；
6. decoder 拒绝重复/乱序 extension key；
7. 非最短整数拒绝；
8. unknown major 拒绝，higher minor 精确保留；
9. `sha-256` 替换为同长 `sha-512` 被拒绝；
10. float、tag、indefinite map 拒绝；
11. trailing 与超过 4096 bytes 的输入拒绝；
12. signing preimage 长度篡改拒绝；
13. 额外嵌套 map 不属于 profile 并被拒绝。

本地完整 workspace 通过；[GitHub Actions run 30716908874](https://github.com/cty12356541/llmos/actions/runs/30716908874) 在 Ubuntu（1m0s）、macOS（1m0s）、Windows（2m4s）全部成功。三平台均执行 schema generation/conformance、workspace test 和 Clippy，Ubuntu 额外执行 rustfmt。

## 6. 当前不能证明什么

- 尚未实现 SHA-256 计算、KeyId、signature algorithm、key validity/revocation 或 Trusted UI authorization；
- `DigestEnvelope` 是 canonical mechanism PoC，不是完整 Receipt、SemanticEvent、TrustPolicy 或 Resource Escrow schema；
- `CanonicalObjectId` 只阻止 object ID 与 digest 互换；ReceiptId/EventId 等更细名义类型仍需由具体业务 schema + domain 绑定；
- CDDL 尚未接入机器 validator/drift generation；Rust 与 CDDL 之间仍靠 golden/conformance 防部分漂移；
- 尚无 TypeScript/Python CBOR 实现的跨语言 byte equivalence；
- cargo-fuzz/property corpus、深度/长度随机攻击、OOM/time bound 和 sanitizer 尚未完成；
- 本 profile 不允许 float 或自由 Unicode text；未来业务 schema若需要它们，必须先定义新的 canonical 规则和 golden，不能直接放宽 decoder。

因此 ADR-0003 与 `B-SCHEMA` 继续保持 `POC/IN_PROGRESS`，下一门是 protobuf/CBOR fuzz。
