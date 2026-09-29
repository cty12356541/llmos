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
- Buf CLI 本身由官方 npm package `@bufbuild/buf 1.72.0` + lockfile 安装，避免 CI setup action 的旧 Node runtime；
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

首次三平台 run 30715842211 在 Ubuntu/macOS 通过全部 schema 步骤，但 Windows checkout 因 CRLF 转换导致 Buf format/generated drift 失败；gate 正确阻止了平台字节差异。后续 remediation 新增 `.gitattributes`，强制 `.proto`、生成 Python/TypeScript 使用 LF；同时用 lockfile 中的官方 Buf npm CLI 替代使用 Node 20 action runtime 的 setup action，并升级 Node/Python setup action。该失败作为反例保留，不冒充通过结果。

remediation 后的 [GitHub Actions run 30715954413](https://github.com/cty12356541/llmos/actions/runs/30715954413) 全部成功：Ubuntu 53s、macOS 1m0s、Windows 2m4s。三平台均通过 Buf lint/format、remote generation、tracked/untracked drift、TypeScript typecheck/conformance、Python conformance、Rust workspace test 与 Clippy；Ubuntu 额外通过 rustfmt。

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

本地及上述三平台 CI：Buf lint/format/generate、TypeScript typecheck/conformance、Python conformance、Rust workspace test/Clippy/rustfmt 全部通过。

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

因此 `B-SCHEMA` 继续保持 `IN_PROGRESS`，ADR-0003 继续保持 `POC`。

## 6. 移交项 #13 收口：SABI v1.2–v1.5 新增面 TS/Python golden 钉死（2026-09-21）

> 对应：`docs/management/stage-b-progress.md` §6.5.6 移交清单第 13 项（W29-G/W34-A 评审标记的 deferred minor）；分支 `fix/w34-conformance-golden`。

### 6.1 范围与钉死模式

本波次新增的 SystemControl 面在此前只有 Rust 侧 golden（`crates/nlos-schema/tests/compatibility.rs` 保持字节源真理），TS/Python conformance 落后。本次按既有 B-SCHEMA-014-METRICS-GOLDEN 模式补齐（goldens 内联于 `tests/conformance/schema/envelope.ts|py`，不新增 `schema/golden/` 文件）：

- v1.2（W28-D）pause/resume/cancel 命令臂；
- v1.3（W29-D）kill/throttle/reclaim 命令臂（throttle 携带 `throttle_percent=50`）；
- v1.4（W28-C-3b）`ResourceRecoveryOperationsSnapshot` 快照 golden（v1.4 冻结点身份）+ semantic/resource acknowledge/resume 四个命令臂 round-trip + 三个 recovery 视图 GetSystemControlRequest round-trip；
- v1.5（W32-G）五层 inspect 快照 golden（TaskGroup/TaskNode/ExecutionFiber/Topic/DurableOperation，v1.5 冻结点身份）+ 五层视图 GetSystemControlRequest 加性寻址字段（`target_id`/`plan_id`/`target_generation`）round-trip + `SystemControlView` 4..=8 与 `RecoveryFailureAuthority.RESOURCE=6` 枚举值钉死。

### 6.2 发现的字段序分歧（记录为事实，不改 Rust golden）

`ControlCommand` 的 oneof 块按声明顺序位于 field 6 与 `reason`(field 8) 之间。实测（TS runtime `@bufbuild/protobuf` 2.14.1、Python runtime `protobuf` 6.33.4）：

- prost 按 proto 声明序输出：`[1..6][oneof 臂][reason]`（W28-D/W29-D Rust golden 即此序）；
- protobuf-es 与 Python upb 按字段号序输出：`[1..6][reason][oneof 臂]`（臂号 9..=18 均 > 8）。

两者是同一 message 的合法 wire 形式（protobuf 字段序不具语义），但字节不逐等。因此命令族 golden 采用双锚点钉法，两语言一致：

1. 解码锚：字面 prost 序字节（Rust 常量 hex 逐段复刻）必须可解码，字段/oneof 臂/CAS 寻址断言后重编码必须落到规范形；
2. 编码锚：镜像 Rust fixture 构造的实例编码必须逐字节等于规范形（TS 与 Python 规范形互相逐字节一致）；另设一个分歧见证断言（prost 序 ≠ 规范式），运行时升级导致两种序收敛时强制显式重钉。

无 oneof 且字段号升序的消息（全部快照族）不受影响，保持与 Rust golden 直接逐字节相等。

### 6.3 验证

```sh
npm run schema:typecheck          # tsc 通过（tests/conformance 在 include 内）
npm run schema:test:typescript    # 通过（新增 §handover-13 全部断言）
python tests/conformance/schema/envelope.py   # 通过
npm run schema:check-generated    # 通过，gen/ 无漂移
cargo test -p nlos-schema --test compatibility  # 34 passed（Rust golden 侧回归）
```

schema/ 与 gen/ 零改动（只读）；Rust golden 零改动；既有测试零弱化。

### 6.4 边界与遗留

- Rust 侧 pre-wire fail-closed 约束（throttle_percent 0/101 拒绝、告警条数上界、寻址字段合法性等）仍是 Rust 编码器策略，不在 TS/Python 生成码 conformance 范围内，维持 Rust 单侧覆盖；
- GetSystemControlRequest 与四个 recovery 命令臂在 Rust 侧本就未钉字节 golden（仅 round-trip），TS/Python 按同形 round-trip + 解码断言钉住，不单方面发明新 canonical 字节；
- §6.5.6 清单第 13 项自此闭合；清单勾销动作留待阶段 C 编排（不在本分支 write-set）。
