# B-SCHEMA-002：跨语言生成、漂移与 breaking gate 证据

> 状态：PARTIAL PASS
>
> 日期：2026-08-02
>
> 对应：`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`、`TYPE-GEN-001`

## 1. 实现范围

本切片把 B-SCHEMA-001 的单一 `.proto` 扩展为可重复的三语言生成和 conformance 链路：

- Rust：继续由 `prost-build` + vendored `protoc` 在 Cargo build 中生成；
- TypeScript：Buf remote plugin `buf.build/bufbuild/es:v2.13.0` 生成到 `gen/typescript`，runtime 固定 `@bufbuild/protobuf 2.13.0`；
- Python：Buf remote plugin `buf.build/protocolbuffers/python:v33.4` 生成到 `gen/python`，runtime 固定 `protobuf 6.33.4`；
- `buf.yaml` 使用 `STANDARD` lint 和 `FILE` breaking policy；`buf.gen.yaml` 固定 plugin 版本；
- checked-in 生成物由 `schema:check-generated` 重生成后检查 `git status --porcelain -- gen`，同时捕获已跟踪差异与新增未跟踪文件；
- TypeScript/Python conformance 程序读取同一 golden vector，验证 major/critical fail-closed、higher minor/non-critical 接受和 unknown protobuf field round-trip；
- GitHub Actions 三平台安装固定 Buf/Node/Python/runtime，执行 lint、generation drift、跨语言测试；PR 在 Linux 额外对 `origin/<base>` 执行 `buf breaking`。

当前 `.proto` 只有 message，没有 RPC service，因此生成的是三语言 type bindings，不虚称已生成不存在的 service client stub。

## 2. 工具链选择事实

```text
Buf CLI: 1.72.0
protobuf-es generator/runtime: 2.13.0
Python generator: protobuf v33.4
Python runtime: protobuf 6.33.4
TypeScript: 5.9.3
CI Node: 24
CI Python: 3.13
```

验证中发现并拒绝了 `protocolbuffers/python:v35.1`：其生成代码要求 Python protobuf runtime `7.35.1`，而 2026-08-02 PyPI 当前最高可安装版本为 `6.33.6`。最终固定 `v33.4`，生成代码声明 runtime `6.33.4`，并通过真实 import/parse/serialize 测试。这个反例说明 generator version 与语言 runtime 必须作为一组验证，不能只取上游最新 tag。

remote plugin 的版本已固定，生成物也 checked in；正常 Rust build 和 SDK consumer 不依赖 BSR 在线可用。重新生成仍依赖 Buf Schema Registry，属于后续 supply-chain mirror/provenance 工作。

## 3. 测试与复现

```sh
buf lint
buf format -d --exit-code
buf generate
npm ci --ignore-scripts
npm run schema:typecheck
npm run schema:test:typescript
python -m pip install -r requirements-schema.txt
python tests/conformance/schema/envelope.py
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

本地结果：Buf lint/format/generate、TypeScript typecheck/conformance、Python conformance、Rust workspace test/Clippy/rustfmt 全部通过。

breaking 反例在临时目录复制 schema，删除 `Envelope.method` field 4 后执行：

```sh
buf breaking <mutated-schema> --against '.git#ref=HEAD,subdir=schema'
```

Buf 以失败退出并报告：此前存在的 `Envelope` field 4 `method` 被删除。临时反例未修改 canonical schema。

## 4. 当前能证明什么

- 同一 `.proto` 可生成 Rust、TypeScript、Python type bindings；
- TypeScript 与 Python 能读取并精确重编码 Rust 使用的同一 golden vector；
- 三语言 conformance 对主次版本、critical/non-critical 和 unknown field 的当前测试行为一致；
- checked-in 生成物的修改、缺失或新增文件可由 CI drift gate 阻止；
- Buf STANDARD lint 与 FILE breaking policy 已配置，删除既有字段的反例会失败；
- generator/runtime 版本不匹配能在实际 runtime import 阶段暴露，而不是只检查生成命令退出码。

## 5. 当前不能证明什么

- 尚无 RPC service 定义，因此没有 Rust/TypeScript/Python service client stub；
- breaking gate 只证明当前 Buf `FILE` policy 和删除字段反例，不覆盖所有应用级语义破坏；
- remote plugin 重新生成依赖 BSR，尚未建立内部 mirror、签名/provenance 验证或离线恢复包；
- deterministic CBOR、签名域、fuzz/property corpus、parser 深度限制和 typed IPC 尚未完成；
- 本地只在 macOS 执行，三平台 CI 结果必须在本提交推送后补记。

因此 `B-SCHEMA` 继续保持 `IN_PROGRESS`，ADR-0003 继续保持 `POC`。
