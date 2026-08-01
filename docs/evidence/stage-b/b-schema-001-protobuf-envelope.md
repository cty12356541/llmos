# B-SCHEMA-001：Protobuf 公共兼容信封初始证据

> 状态：PARTIAL PASS
>
> 日期：2026-08-02
>
> 对应：`COMPAT-VER-001`、`COMPAT-VER-002`、`SABI-TRANSPORT-001`、`TYPE-GEN-001`

## 1. 实现范围

本切片建立阶段 B 的第一个 schema registry 条目：

- `schema/nlos/sabi/v1/envelope.proto`：`SchemaIdentity` 与 `Envelope` 唯一 IDL 源；
- `nlos-schema/build.rs`：通过 vendored `protoc` + `prost-build` 生成 Rust 类型，不依赖开发机预装编译器；
- `nlos-schema` registry：声明 `nlos.sabi.Envelope` v1.0 与已支持 critical extension 集合；
- compatibility validator：1 MiB frame 上限、16-byte request ID、schema/major/critical extension fail-closed；
- `ValidatedFrame`：typed view 与原始 wire frame 并存，透明转发未知 protobuf field 时不 decode/re-encode；
- `schema/golden/nlos.sabi.Envelope-v1.hex`：首个 canonical conformance vector。

当前 payload 故意保持 opaque。它证明公共 envelope 的演进边界，不声称 Operation、Task 或 Control 的 service schema 已冻结。

## 2. 环境

```text
hardware architecture: arm64 / Apple Silicon
OS: macOS 26.5.2 (Build 25F84)
rustc: 1.97.1 (8bab26f4f 2026-07-14)
cargo: 1.97.1
prost/prost-build: 0.14.4
protoc-bin-vendored: 3.2.0
build: dev profile
cross-platform CI: GitHub Actions run 30715148293
```

## 3. 兼容规则

```text
frame bytes
  → pre-parse size bound
  → protobuf decode
  → registry lookup(schema name)
  → exact major match
  → every critical extension must be supported
  → common field invariants
  → ValidatedFrame { typed envelope, original wire bytes }
```

- 更高 minor：接受，前提是没有 unsupported critical extension；
- unknown non-critical extension：接受；
- unknown protobuf field：typed view 可以不知道，但 forwarding 使用原始 frame，逐字节保持；
- unknown major / critical extension：拒绝，不尝试降级；
- malformed、oversized 或公共字段不完整：拒绝。

## 4. 测试与复现

```sh
cargo fmt --all -- --check
cargo test -p nlos-schema
cargo clippy -p nlos-schema --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

新增 7 项测试并全部通过：

1. 生成编码与 checked-in golden vector 精确一致，golden 可解回相同 typed value；
2. registry 暴露唯一受支持的 schema name 和 v1.0；
3. 更高 minor + unknown non-critical extension被接受；
4. unknown major 被拒绝；
5. unknown critical extension 被拒绝；
6. 注入 protobuf field 100 后仍可解码，forwarding bytes 与输入逐字节一致；
7. 15-byte request ID 与超过 1 MiB 的 frame fail-closed。

完整 workspace 回归通过；推送后 [GitHub Actions run 30715148293](https://github.com/cty12356541/llmos/actions/runs/30715148293) 在 Ubuntu（44s）、Windows（1m24s）和 macOS（1m10s）全部成功，三平台均执行 workspace test 与 Clippy，Ubuntu 额外执行 rustfmt。100K waiting Fiber 与 100K Store metadata 的显式 ignored 规模探针未运行，因为不属于 schema 首切片。

## 5. 当前能证明什么

- Rust wire type 确实从同一 `.proto` 构建生成；
- 当前 v1 envelope 的 golden bytes 已固定为可回归 Evidence；
- `[COMPAT-VER-001/002]` 中 schema name、major/minor、critical/non-critical 的首个可执行规则已实现；
- forwarding adapter 可以在不知道新 protobuf field 的情况下保留原始 frame；
- 不兼容或资源越界输入按 typed error fail-closed。
- vendored `protoc`、Rust generation、golden/compat 测试已在 Ubuntu、Windows、macOS 构建执行。

## 6. 当前不能证明什么

- TypeScript/Python client 尚未生成，也没有跨语言 golden decode/encode；
- 尚未接入 Buf lint/breaking check，不能阻止所有 schema source breaking change；
- deterministic CBOR canonical profile 与 signed object 未实现；
- fuzz/property corpus、递归深度和 service-specific payload limits 未完成；
- Unix domain socket / Windows named pipe transport adapter、peer authentication、Capability 与 Receipt 绑定未实现；
- 原始 frame 保留保证的是透明转发，不保证“修改 typed field 后重新编码”仍保留 unknown field；此类修改必须由版本感知 gateway 明确实现。

因此 `B-SCHEMA` 保持 `IN_PROGRESS`，ADR-0003 保持 `POC`，不得据此宣称公共 SABI 已冻结。
