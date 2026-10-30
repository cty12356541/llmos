# Evidence e-sabi-002：v1.0 九通道黄金不可变语料库、五语言一致性门禁与黄金篡改拦截验证

- **Evidence ID**：`e-sabi-002`
- **所属波次与车道**：`Stage E` / `W77` / `W77-E1B-GOLDEN-SDKS`（工作包 `E1` `E-SABI-POLICY-CORPUS` 右半幅，里程碑 `M-E1` 候选证据）
- **日期**：`2026-10-30`
- **基线提交（Base HEAD）**：`44fd32e04120bc8e2e0475b5eb859cb23505b439`
- **验证等级**：`H4`（宿主实测五语言 wire 一致性、跨语言 `[COMPAT-VER-002]` 对抗矩阵与黄金不可变篡改拦截单测；不主张 `H5`–`H7`）
- **规范与决策依据**：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §9（SABI 契约与跨语言 wire 确定性）、§28（演进兼容与 `[COMPAT-VER-002]`）
  - [ADR-0027：SABI 版本策略、世代生命周期与跨大版本兼容垫片架构](../../management/adrs/0027-sabi-versioning-deprecation-and-compat-shim.md) §2.1、§3.3（黄金不可变语料库）、§6.1
  - [ADR-0014：Schema 通道冻结 v1-beta](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)
  - 历史探针基线：[b-sdk-go-001-golden-probe.md](../stage-b/b-sdk-go-001-golden-probe.md)、[b-sdk-csharp-001-golden-probe.md](../stage-b/b-sdk-csharp-001-golden-probe.md)

---

## 1. v1.0 世代九通道黄金语料库与根级历史黄金零漂移清单

### 1.1 `schema/golden/epochs/v1.0/` 黄金向量矩阵（9/9 通道 · 11 条确定性向量）

依据 `crates/nlos-schema/src/lib.rs` 中 `REGISTRY` 冻结登记的全部 **9 条 SABI 通道**，在 `schema/golden/epochs/v1.0/` 下建立只增不改（immutable）的 `v1.0` 世代语料库及指纹清单 `schema/golden/epochs/v1.0/manifest.json`（其中 `nlos.sabi.Envelope` 含标准请求与携带未知字段/非关键扩展两条向量，`nlos.sabi.PrincipalHandshake` 覆盖 `Challenge` 与 `Attestation` 双消息）：

| 序号 | 通道名（`SchemaIdentity.name`） | 黄金向量文件（`schema/golden/epochs/v1.0/`） | 消息类型 | 版本 | 字节数 | `sha256_wire` | `sha256_file` |
|---|---|---|---|---:|---:|---|---|
| 1 | `nlos.sabi.Envelope` | `nlos.sabi.Envelope-v1.0.hex` | `nlos.sabi.v1.Envelope` | `1.1` | 280 | `1fcef924ca415e1d44c825755e15ccc9eaaff8480f93d9eb3146fd8cae0c7724` | `bc5192239103c945521ab7d736c19a7b134d2e377520b8088ce7a6f313584b03` |
| 2 | `nlos.sabi.Envelope` | `nlos.sabi.Envelope-unknown-noncritical-v1.0.hex` | `nlos.sabi.v1.Envelope`（含 `non_critical=[42,7001,9999]` 与尾部未知字段 `a00607`） | `1.1` | 83 | `61508a0f778c4c34fcba6b8b63390381937515718188cd5dbfa40ae03b5e3382` | `46aecff05503c4ba8181b4aeec9c2bfa7245328a5e05dcb95cc4d75b98abd09c` |
| 3 | `nlos.sabi.ServiceDirectory` | `nlos.sabi.ServiceDirectory.ResolveServiceRequest-v1.0.hex` | `nlos.sabi.v1.ResolveServiceRequest` | `1.0` | 48 | `f8387d10fc8a72a8f72fcf3f6b0af21358785e505f99d9b19ca2b7aacc58ec70` | `a157960cefdffc71f5bb8cffc910c6681ba7e7e118a11130e986cdc971f75b7d` |
| 4 | `nlos.sabi.OperationControl` | `nlos.sabi.OperationControl.QueryOperationRequest-v1.0.hex` | `nlos.sabi.v1.QueryOperationRequest` | `1.0` | 54 | `866a3b0811a681f06992b21ff4bfff290966289fc348bbd7ff25148037d378a4` | `ba84512e68ceb8a05de17e6103f67c18227bcda8df4f82de5398fd27513b4cfe` |
| 5 | `nlos.sabi.SystemControl` | `nlos.sabi.SystemControl.GetSystemControlRequest-v1.0.hex` | `nlos.sabi.v1.GetSystemControlRequest` | `1.7` | 73 | `fb1b2a923e60a6fa22444c853cd516eb8bad11ddd51c73b9256833794f9ca0ac` | `61f0bad2e998db13f7615e6b24d99ca45370ca3e492d7e8935854a077f4b07f9` |
| 6 | `nlos.sabi.TakeoverControl` | `nlos.sabi.TakeoverControl.SubmitBarrierObservationRequest-v1.0.hex` | `nlos.sabi.v1.SubmitBarrierObservationRequest` | `1.0` | 274 | `988fa2aeab19e3d759a9bd27956da8772a8766a557bb8349efc28975489be892` | `db0d7616f085bd2d2adde43bc7bf0e33cbcc4c3426672f8da85b12fb09914742` |
| 7 | `nlos.sabi.WaitControl` | `nlos.sabi.WaitControl.InspectWaitRequest-v1.0.hex` | `nlos.sabi.v1.InspectWaitRequest` | `1.0` | 45 | `a90443d446e5e32c284babdae53428f3e078635d2d427bc10aa6611377b8ff10` | `37c7304ada52053ddd1320d568b94ecba10a299ef24a9562dde78d87a31a63b5` |
| 8 | `nlos.sabi.PrincipalHandshake` | `nlos.sabi.PrincipalHandshake.Challenge-v1.0.hex` | `nlos.sabi.v1.PrincipalHandshakeChallenge` | `1.0` | 68 | `9bae5953652b07d507e1569222ae2b91a7d3a24aa7d120a0975234861f4bce0a` | `e372ea1e0c0f8fc97ebce37b1e9893ac16ef28922d43adacaf0fa6b8cf9699e5` |
| 9 | `nlos.sabi.PrincipalHandshake` | `nlos.sabi.PrincipalHandshake.Attestation-v1.0.hex` | `nlos.sabi.v1.PrincipalHandshakeAttestation` | `1.0` | 193 | `06e4bc1042401e2dea8b554ec9172c1ba1a7e9c68d82e20090026c2df9111569` | `568de3e68cd95541f6445d88ecb2f30cee7729bd32b8041167bc7b98c1acf69e` |
| 10 | `nlos.sabi.DriverProvider` | `nlos.sabi.DriverProvider.Request-v1.0.hex` | `nlos.sabi.v1.DriverProviderRequest` | `1.0` | 92 | `ab8bebab35db75c4f14be3e8f6ba9dc8ca59f300457100b00a315c5ad81395ac` | `27efa0d0d864eeec5fb4c711d7839472e145fcc377ebe0578ea515f4fddf2a83` |
| 11 | `nlos.sabi.ChannelControl` | `nlos.sabi.ChannelControl.CreateChannelRequestWire-v1.0.hex` | `nlos.sabi.v1.CreateChannelRequestWire` | `1.0` | 93 | `8723f39ba6eec4552a9511774f4b83305bcaa3f2b686d55d459aa138ee81303d` | `6f80757f9d31a5dce0f60aca81abc1a3b6043fd251f3fe00f638921a7a39b1fb` |

### 1.2 根级历史冻结黄金向量（7 条）零漂移锁定表

根级 `schema/golden/*.hex` 共 7 个历史冻结文件只读零改动，并由 `schema/golden/epochs/v1.0/manifest.json` 的 `legacy_root_goldens` 字段同时锁定 wire SHA-256 与文件级 SHA-256：

| 历史黄金路径 | 字节数 | `sha256_wire` | `sha256_file` | 状态 |
|---|---:|---|---|---|
| `schema/golden/nlos.canonical.DigestEnvelope-preimage-v1.hex` | 162 | `d4bd951a18385a72dee85cdd48544aa56e783a896b6612268d881e66eab81923` | `4bb7cbf370dca47dc706a938eb8bbdbaccefb76c70fcdc25d6ce8f09cadd175d` | 零改动锁定 |
| `schema/golden/nlos.canonical.DigestEnvelope-v1.hex` | 128 | `6fffe21750702156f4ff059818a917b8537e2c8f4494aae2f24e738e5873009a` | `ce93787d1093e48a2eebf347abfbd7f311806acc54a595063ce9461e9d355ecb` | 零改动锁定 |
| `schema/golden/nlos.sabi.Envelope-common-request-v1.hex` | 252 | `fff671f95f99b4d7afd9a06b5f1276e433fae325cee7dc47323086c3a13faa3a` | `e2bb07a2d742bd51396e7dad96ff5dc2d59465235a8c5319e88d6e20ca6d5bb5` | 零改动锁定 |
| `schema/golden/nlos.sabi.Envelope-common-uncertain-v1.hex` | 164 | `b4338c436f72c8577340ff89918ded254a29f488e3856b01c3abe5e3703adfa4` | `dfcc54f32cd7da1e9a80230259a21a4d3be9825343cff904669c78f60b175def` | 零改动锁定 |
| `schema/golden/nlos.sabi.Envelope-v1.hex` | 66 | `ce270a09badebf5a6f9c6c16b6107c5820d72a011363ffa004fd1bbdaad0174d` | `670f664fb25a0f2c1e5cb34dc7f427e3187cec535b9a1031dff221f4d92debb0` | 零改动锁定 |
| `schema/golden/nlos.sabi.PrincipalHandshake-v1.hex` | 185 | `db5ad666c6b0b31401df06932bcea0a2202854bbbc8bb9f258a1ac33305bb8d5` | `c7259dffaf0bd168425c77963a0ae421560cd288bd4381ec2e58e8d25d791963` | 零改动锁定 |
| `schema/golden/nlos.sabi.ServiceDirectory.ResolveRequest-v1.hex` | 43 | `062a8627a5104161cdd0ed0e4d1724f3c8ce06cb968fc5ff2cca926dbbc8d65e` | `5d2a1570d03018db58ab397238821cc07f680055d73b0266658bce594615afb7` | 零改动锁定 |

---

## 2. 五语言一致性与 `[COMPAT-VER-002]` 对抗验证矩阵

### 2.1 五语言核验能力对齐表

| 语言栈 | 实现/校验入口 | 工具链版本与供给来源 | 11 条 `v1.0` 正向编码 = Golden | 11 条解码字段断言 + 重编码 = Golden | 九通道未知字段 `a00607` 保持 | `[COMPAT-VER-002]`（`major!=1` / 未知 `critical` 拒绝，未知 `non_critical` 放行） |
|---|---|---|---|---|---|---|
| **Rust** | `crates/nlos-schema`（`prost 0.14.4`，只读引用既有权威实现） | `rustc` / `cargo` 离线校验（`nlos-schema` 41 项单测 + 11 向量交叉核验） | 11/11 PASS | 11/11 PASS（`ValidatedFrame::wire_bytes()` 原样保留未知字段帧） | PASS（`Envelope` 帧级原样透传） | PASS（`CompatibilityError::UnsupportedMajor` / `UnsupportedCriticalExtension`） |
| **TypeScript** | `sdk/typescript/src/common.ts` + `tests/conformance/schema/envelope.ts` | Node.js + `@bufbuild/protobuf`（`gen/ts`） | 11/11 PASS | 11/11 PASS | 10/10 消息类型（9/9 通道）PASS | 9/9 通道对抗矩阵 PASS（`E_ABI_VERSION` / `UNSUPPORTED_CRITICAL_EXTENSION`） |
| **Python** | `sdk/python/nlos_sdk/common.py` + `tests/conformance/schema/envelope.py` | Python 3 + `google.protobuf`（`gen/python`） | 11/11 PASS | 11/11 PASS | 10/10 消息类型（9/9 通道）PASS | 9/9 通道对抗矩阵 PASS（`E_ABI_VERSION` / `UNSUPPORTED_CRITICAL_EXTENSION`） |
| **Go** | `sdk/go/sabi/{messages,marshal,unmarshal,golden_test}.go` | `go1.27.0 darwin/arm64`（仓库外临时目录供给，`go1.27.0.darwin-arm64.tar.gz` SHA-256 `90493b3bbd5e10f91d12153198bf1994fd756399b4fec93b49b0c6e2acdeeb3e`） | 11/11 PASS | 11/11 PASS | 10/10 消息类型（9/9 通道）PASS | 9/9 通道对抗矩阵 PASS（`ErrAbiVersion` / `ErrUnsupportedCriticalExtension`） |
| **C#** | `sdk/csharp/llmos-sabi-probe/{Messages,Codec,Program}.cs` | `.NET SDK 8.0.424`（`osx-arm64` 仓库外临时目录供给，`dotnet-sdk-8.0.424-osx-arm64.tar.gz` SHA-512 `975b686a2c6a5d62b20d95d04a233325670f38acc2ab5815d65126f05338783eb5988b16f504c9de516a63a550bcef0c061fc20fd25f128cc5b46be957065999`） | 11/11 PASS | 11/11 PASS | 10/10 消息类型（9/9 通道）PASS | 9/9 通道对抗矩阵 PASS（`E_ABI_VERSION` / `UNSUPPORTED_CRITICAL_EXTENSION`） |

### 2.2 关键跨语言新增对齐细节

1. **SDK 兼容校验收口**：
   - TypeScript 新增 `SABI_V1_CHANNELS`、`SabiChannelSpec` 与 `validateSchemaCompatibility`（`sdk/typescript/src/common.ts`），对空/含 NUL `name`、未注册通道、`major != 1`（返回 `code = "E_ABI_VERSION"`）及未知或零值 `critical_extension_ids`（返回 `code = "UNSUPPORTED_CRITICAL_EXTENSION"`）严格 fail-closed，同时显式放行更高 `minor` 与任意未知 `non_critical_extension_ids`。
   - Python 新增 `SABI_V1_CHANNELS`、`SabiChannelSpec` 与 `validate_schema_compatibility`（`sdk/python/nlos_sdk/common.py` 并由 `__init__.py` 导出），错误语义与 TypeScript、Rust `nlos-schema` 完全一致。
   - Go（`sdk/go/sabi`）与 C#（`sdk/csharp/llmos-sabi-probe`）同步补齐 `SabiV1Channels`、`ValidateSchemaIdentity` 以及其余 6 条通道（`OperationControl`、`SystemControl`、`TakeoverControl`、`WaitControl`、`DriverProvider`、`ChannelControl`）加 `PrincipalHandshakeChallenge` 的手写确定性 `Marshal`/`Unmarshal` 与 `UnknownFields` 透传，从 Stage B 的 3/9 通道扩面至 **9/9 全通道覆盖**，同时完整保留全部历史根级黄金测试零回退。

---

## 3. 黄金不可变门禁（`tools/check_golden_immutability.py`）与篡改拦截单测

### 3.1 门禁检查项

`tools/check_golden_immutability.py` 仅使用 Python 3 标准库实现，执行五重静态与 Git 历史防篡改检查：

1. **根级历史黄金完整性**：核对 `schema/golden/*.hex` 磁盘实际集合与 `v1.0/manifest.json` 中 `legacy_root_goldens` 完全一致（7 个文件无缺失、无未登记新增），逐一校验规范小写十六进制格式、`byte_length`、`sha256_wire` 与 `sha256_file`。
2. **世代目录规范性**：要求 `schema/golden/epochs/<epoch>/manifest.json` 必须存在、`immutable == true`、`.hex` 文件全部满足单行小写十六进制加末尾 `\n`（`^[0-9a-f]+\n\Z`）且无孤儿 `.hex` 文件。
3. **九通道全覆盖强制**：对 `sabi_major == 1` 的世代强制校核 `required_channels` 与实际向量覆盖集合等于全部 9 条 SABI v1 通道。
4. **双哈希锁死**：同时校验解码后二进制载荷 `sha256_wire` 与原始文本字节 `sha256_file`。
5. **Git 历史不可变对比（`--check-git-diff <base_ref>`）**：对比 `<base_ref>` 与当前工作区/索引，禁止删除或修改 `<base_ref>` 中已存在的任何 `schema/golden/**/*.hex` 或既有世代 `manifest.json`（仅允许新增 `schema/golden/epochs/vX.Y/` 新世代目录）。即使攻击者同时篡改 `.hex` 与 `manifest.json` 中的哈希值，`--check-git-diff` 仍能基于基线提交直接拦截。

### 3.2 正反向单测结果（`tests/conformance/schema/test_golden_immutability.py`）

通过 `python3 -m unittest discover -s tests/conformance/schema -p "test_*.py" -v` 执行 7 项测试全部 PASS：

1. `test_clean_corpus_passes`：干净语料库返回码 `0`。
2. `test_tamper_single_byte_in_hex_fails`：分别篡改 `v1.0` 世代 `Envelope-v1.0.hex` 与根级历史 `nlos.sabi.Envelope-v1.hex` 单字节（`0x00 <-> 0xff`），均返回非零并命中 `sha256` 不匹配拦截。
3. `test_noncanonical_uppercase_hex_fails`：将十六进制改为大写（wire 解码字节不变但文件格式非规范）返回非零拦截。
4. `test_delete_golden_file_fails`：删除既有 `v1.0/*.hex` 或删除根级历史 `.hex` 均返回非零拦截。
5. `test_unregistered_stray_hex_file_fails`：在 `v1.0/` 或根级 `schema/golden/` 混入未登记 `stray.hex` 均返回非零拦截。
6. `test_missing_one_of_nine_channels_fails`：移除 9 条通道中任一通道（如 `ChannelControl`）向量返回非零拦截。
7. `test_git_diff_rejects_mutation_of_existing_golden_and_allows_new_epoch`：构造独立临时 Git 仓，验证新增 `schema/golden/epochs/v2.0/` 在 `--check-git-diff HEAD` 下合法放行；而修改既有 `v1.0/*.hex` 并同步伪造更新 `v1.0/manifest.json` 指纹时被 `--check-git-diff HEAD` 精确拦截。

---

## 4. 实测验证门命令与结果摘要

以下全部门禁均在独立车道工作树中实测通过（退出码均为 `0`）：

1. **黄金不可变门禁（含基线 Git diff 核验）**：
   ```console
   $ python3 tools/check_golden_immutability.py --check-git-diff 44fd32e04120bc8e2e0475b5eb859cb23505b439
   [GOLDEN-IMMUTABILITY-OK] legacy_root=7 files; epochs=(v1.0: 9 channels / 11 vectors), git-diff base=44fd32e04120bc8e2e0475b5eb859cb23505b439 OK
   ```
2. **黄金不可变正反向单测**：
   ```console
   $ python3 -m unittest discover -s tests/conformance/schema -p "test_*.py" -v
   Ran 7 tests in 0.188s
   OK
   ```
3. **Python SABI 九通道一致性与兼容门禁**：
   ```console
   $ python3 tests/conformance/schema/envelope.py
   （退出码 0：覆盖 5 条根级 SABI golden + 2 条 canonical golden + 11 条 v1.0 golden + 九通道未知字段保持 + 九通道 COMPAT-VER-002 矩阵）
   ```
4. **TypeScript Schema Lint / 生成物零漂移 / 类型检查 / 九通道一致性门禁**：
   ```console
   $ npm run schema:lint && npm run schema:check-generated && npm run schema:typecheck && npm run schema:test:typescript
   （4 项脚本退出码全部为 0）
   ```
5. **Rust `nlos-schema` 权威实现离线交叉校验**：
   ```console
   $ cargo test --offline --locked -p nlos-schema
   test result: ok. 41 passed; 0 failed
   （独立校验器验证 11/11 条 v1.0 黄金向量与 nlos-schema encode/decode 逐字节相等且通过全部语义不变量检查）
   ```
6. **Go 1.27.0 探针门禁（`wire` + `sabi`，共 30 项测试）**：
   ```console
   $ cd sdk/go && GOTOOLCHAIN=local go vet ./... && GOTOOLCHAIN=local go test ./... -v -count=1
   ok  	example.com/llmos/sdk-go/sabi	1.355s   # 24/24 测试函数 PASS（0 FAIL）
   ok  	example.com/llmos/sdk-go/wire	0.678s   # 6/6 测试函数 PASS（0 FAIL）
   ```
7. **C# .NET 8.0.424 探针门禁（`llmos-sabi-probe`，共 30 项测试）**：
   ```console
   $ DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1 dotnet run --project sdk/csharp/llmos-sabi-probe/llmos-sabi-probe.csproj
   cs golden probe: 30 passed, 0 failed (total 30)
   ```

---

## 5. 诚实非主张声明（Honest Non-Claims）

1. **验证等级边界（`H4` != `H7`）**：本证据仅证明宿主单机环境下的确定性 wire 语料库一致性、五语言编解码往返、`[COMPAT-VER-002]` 版本/扩展位 fail-closed 行为与静态/Git 黄金防篡改门禁（`H4`），不主张跨进程压力故障注入、多节点升级演练或长稳生产运行（`H5`–`H7`）。
2. **Go 与 C# 探针范围边界**：`sdk/go` 与 `sdk/csharp/llmos-sabi-probe` 延续零第三方依赖的手写确定性 wire 探针定位，已扩面覆盖 `v1.0` 全部 9 条 SABI 通道的代表性消息与 `SchemaIdentity` 兼容校验，但仍不包含 `LocalRpcService` IPC 传输客户端、流式会话管理或完整 `.proto` 全量消息代码生成产物。
3. **跨大版本兼容垫片边界**：本车道仅落地 `E1` 右半幅（`v1.0` 九通道黄金不可变语料库与五语言一致性门禁）；跨大版本协议转换垫片 `nlos-compat-shim` 及其性能/降级审计属于后续波次 `W78`（工作包 `E2`）职责，本证据不做提前主张。
