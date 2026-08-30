# Evidence B-SDK-CSHARP-001：C# 最小 golden 探针（冻结 wire v1-beta 逐字节比对）

- 状态：**PASS**（golden 门 16/16 全 PASS、4/4 golden，与 Go 先例完全对齐；2026-08-30 扩面记录见 §8）
- 日期：2026-08-30（初版 W4-G）；2026-08-30（§8 扩面追加）
- 仓库 HEAD：`bb93e2fbd53cafde0933ebc07f83b4c70f7072e1`（初版基线）；§8 验证时 `afd05ae71b39e62eaa55487e3cb49e4527d89d36`（无漂移）
- 工作包：`B-SDK-LANG-EVAL`（Go 先例 [b-sdk-go-001](b-sdk-go-001-golden-probe.md) 的第二语言镜像车道）
- 冻结纪律依据：[ADR-0014](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)
- 覆盖的 registry 条目：`nlos.sabi.Envelope`（v1.1；§8 扩面后含 common-request/common-uncertain 两条上下文 golden）、`nlos.sabi.PrincipalHandshake` 家族（ADR-0011 additive；golden 实际编码的是 `PrincipalHandshakeAttestation`，schema name 载荷为家族名 `nlos.sabi.PrincipalHandshake`）

## 1. 写集清单

| 路径 | 操作 | 说明 |
|---|---|---|
| `sdk/csharp/llmos-sabi-probe/llmos-sabi-probe.csproj` | 新增 | net8.0 最小 console 工程，零第三方依赖（无 NuGet PackageReference） |
| `sdk/csharp/llmos-sabi-probe/Wire.cs` | 新增 | 手写 varint / tag / length-delimited 原语（镜像 `sdk/go/wire/wire.go`） |
| `sdk/csharp/llmos-sabi-probe/Messages.cs` | 新增 | Envelope 家族 + PrincipalHandshakeAttestation 结构与冻结枚举常量 |
| `sdk/csharp/llmos-sabi-probe/Codec.cs` | 新增 | 确定性编码器 + fail-closed 解码器（镜像 Go 的 marshal.go/unmarshal.go） |
| `sdk/csharp/llmos-sabi-probe/Program.cs` | 新增 | 探针驱动：golden byte-equal / roundtrip / 边界 / 失败路径用例 + PASS/FAIL 汇总、非零退出码 |
| `docs/evidence/stage-b/b-sdk-csharp-001-golden-probe.md` | 新增 | 本文件 |

未触碰（写集外验证）：`buf.gen.yaml`、`gen/`、`schema/**`、`crates/**`、根 `Cargo.toml` 均零改动（`git status` 中 crates/** 条目为并行车既有工作，本车道未读取写、未提交、未回退）。构建产物 `bin/`、`obj/` 已清理，不留在写集内。

## 2. 工具链探测结果（如实记录）

| 工具 | PATH 探测 | 常见安装位置探测 | 结论 |
|---|---|---|---|
| `dotnet` | 缺失 | `/usr/local/share/dotnet`、`~/.dotnet` 均无 | **本机未安装**；为本探针临时下载官方 `dotnet-sdk-8.0.424-osx-arm64.tar.gz`（209,904,374 字节；来源 `https://builds.dotnet.microsoft.com/dotnet/Sdk/8.0.424/`，版本与 SHA512 取自官方 release metadata `dotnet/release-metadata/8.0/releases.json`）解压至仓库外临时目录运行；SHA512 `975b686a2c6a5d62b20d95d04a233325670f38acc2ab5815d65126f05338783eb5988b16f504c9de516a63a550bcef0c061fc20fd25f128cc5b46be957065999` 与官方发布清单一致（本地 `shasum -a 512` 复算 MATCH），`dotnet --version` → `8.0.424` |
| `protoc` | 缺失 | 无 | 缺失 |
| `buf` | 缺失 | 无 | 缺失 |

路线判定：**路线 A 成立**（临时工具链可用）+ 手写 wire codec。与 Go 先例一致，选择手写而非 `Google.Protobuf` NuGet 包：零第三方依赖使逐字节行为完全可控可审计，且避免为一次性探针引入包依赖面。`dotnet` 工具链为一次性临时供给（仓库外临时目录），机器与 CI 均无持久安装；`buf.gen.yaml` 完全未动。

## 3. 交付物与比对用例

### 3.1 结构

`Wire` 静态类（varint/tag/length 原语，fixed32/64 仅可跳过不生成，group 拒绝，`WireException` 四类错误对应 Go 的哨兵 error）＋ 11 个手写消息类 + 2 个冻结枚举 ＋ `SabiCodec` 确定性 Marshal/Unmarshal。确定性契约与 Go 探针逐条对齐：字段升序、零值省略、消息字段引用 presence、repeated uint32 packed、未知字段捕获后于尾部原样追加；oneof common_context 双臂置位在 Marshal 时抛 `InvalidOperationException`（对应 Go 的 panic）。

### 3.2 golden byte-equal（编码方向）

固定输入 → `Marshal()` → 与冻结 hex 逐字节比对（输入值域反推自 `sdk/go/sabi/golden_test.go`，与三语言 conformance 同源）：

| golden | 字节数 | 结果 |
|---|---|---|
| `nlos.sabi.Envelope-v1.hex` | 66 | **byte-equal PASS** |
| `nlos.sabi.PrincipalHandshake-v1.hex` | 185 | **byte-equal PASS** |

Envelope 输入：schema name=`nlos.sabi.Envelope`、major=1、non_critical_extension_ids=[42]、request_id=00×16、service=`operation`、method=`get`、payload=`abc`。PrincipalHandshake 输入：schema name=`nlos.sabi.PrincipalHandshake`、major=1、principal_id=00×16、nonce=a5×32、channel_binding=`unix:///tmp/nlos-handshake.sock`、signature=cd×64。

任务规定的 golden 门为上述两条（任务书明确镜像范围），解码后字段断言与 `tests/conformance/schema/envelope.py` 对齐（oneof 未置位、payload 等）。`Envelope-common-request-v1.hex`/`Envelope-common-uncertain-v1.hex` 在初版（W4-G）未纳入 golden 门；codec 已实现其全部消息面（SabiRequestContext/SabiResponseContext 及嵌套）。**2026-08-30 扩面后两条 common golden 已全量断言并 PASS，见 §8。**

### 3.3 roundtrip（解码 → 重编码 byte-equal）

两条 golden 全部 `Unmarshal` 后 `Marshal` 与原字节相等：**2/2 PASS**（§8 扩面后为 4/4）。另有未知字段保持用例：Envelope golden 尾部追加 `a0 06 07`（field 100 varint 7），解码捕获 `UnknownFields` 精确为 3 字节、重编码与扩展输入 byte-equal（镜像 envelope.py 与 Go 车道同款用例）。

### 3.4 边界值与失败路径（镜像 Go wire_test.go + golden_test.go）

- varint 编码边界：0/1/127/128/300/16383/16384/MaxInt32/MaxUint32/MaxInt64/MaxUint64（10 字节）共 11 档，编码与解码 roundtrip 全 PASS
- 非 minimal varint 接受（`80 00`=0、`81 80 80 00`=1）；截断（cut 1..9）报 Truncated；11 字节与第 10 字节带值位溢出报 Overflow
- tag 边界：field 15（单字节键 `7a`）→ field 16（`82 01`）→ 2047/2048 → 2^29−1 上限 roundtrip
- 长度前缀边界：128 字节 payload 触发两字节长度前缀，总长 138，roundtrip 保持
- group wire type（tag `0x0b`）fail-closed 拒绝；uint32 溢出值（varint 2^32 装入 Major）显式拒绝不截断；golden 在字段中部截断（cut=1/28/63）fail-closed，字段边界完整前缀（cut=27）可解码；oneof 双臂置位在 Marshal 时抛异常（探针级不变式）
- SkipValue：varint/len/fixed32/fixed64 可跳过（additive 扩展捕获面），group 拒绝，截断长度前缀拒绝

## 4. 验证门命令与结果

```console
$ shasum -a 512 <tmp>/dotnet-sdk-8.0.424-osx-arm64.tar.gz   # 与官方 release metadata SHA512 一致（MATCH）
$ <tmp>/dotnet/dotnet --version                              # 8.0.424
$ dotnet build sdk/csharp/llmos-sabi-probe/llmos-sabi-probe.csproj   # BUILD_OK，0 Warning(s) / 0 Error(s)
$ dotnet run --project sdk/csharp/llmos-sabi-probe/llmos-sabi-probe.csproj --no-build
cs golden probe: 14 passed, 0 failed (total 14)              # EXIT=0
```

**数字：14/14 用例 PASS（0 FAIL），构建零告警。** 其中 wire 层 6 用例、golden/边界层 8 用例（两条 golden 各含 byte-equal + 字段断言 + roundtrip 三重门）。复现需仓库外临时 .NET SDK 8.0.424（见 §2）。

## 5. 已知限制（探针边界，非完整 SDK）

1. **非完整 SDK**：仅手写 2 条目 frozen wire 面；无 options/map/zigzag/fixed 编码、无反射/描述符、无 UTF-8 校验（C# `Encoding.UTF8` 对非法序列做替换式解码，Go 车道保留原始字节——对 ASCII golden 无差异，但对含非法 UTF-8 的 string 字段非 bit-preserving，语义校验归 SDK 校验层）、无 service 桩、无 oneof 类型化 API（双臂用异常而非 typed error）、无 conformance 框架集成。
2. **无 IPC 客户端**：不含 LocalRpcService 传输层。
3. **覆盖面 4/7 registry golden 条目**（§8 扩面后；Envelope 家族全 4 条：Envelope-v1、Envelope-common-request-v1、Envelope-common-uncertain-v1、PrincipalHandshake-v1）：ServiceDirectory.ResolveRequest、OperationControl、SystemControl、TakeoverControl、WaitControl 及 DigestEnvelope 家族未覆盖。
4. **golden 门 4/4**（§8 扩面后）：Go 车道断言过的 4 条 sabi golden 本车道已全量对齐；全仓 7 条 golden 中剩余 3 条（ServiceDirectory.ResolveRequest-v1、DigestEnvelope-v1/-preimage-v1）分属其他家族，不在本探针 registry 面。
5. .NET 工具链为临时下载供给，未进入机器 PATH 与 CI；与 `Google.Protobuf`/protobuf-net 生成代码的交叉比对未做（无 protoc，且探针刻意零依赖）。
6. 解码不校验重复 oneof 臂（后到覆盖），解码枚举不做未知值 fail-closed（proto3 保留未知值）；`Environment.Exit` 码为唯一失败信号，无 xunit/NUnit 报告格式。
7. 测试以 console harness 形式交付（无测试框架依赖）；若后续车道引入 xunit，需迁移 16 个用例的断言形式。

## 6. 未运行项（显式标注）

- ~~`nlos.sabi.Envelope-common-request-v1.hex` / `nlos.sabi.Envelope-common-uncertain-v1.hex` 的 byte-equal 断言~~（初版范围外；**已于 2026-08-30 运行并 PASS，见 §8**）
- `buf generate` 及 C# 代码生成路径（buf/protoc 缺失）；`buf.gen.yaml` 零改动
- 与 `Google.Protobuf` 的编解码交叉验证（零依赖探针未引入该包）
- 模糊测试、性能基线、`dotnet test` 框架集成（超出探针门）
- 本车道全部 `cargo`/Rust 面（无 Rust 改动）；并行车 crates/** 改动非本探针职责
- 提交与推送（任务明令禁 git）

## 7. Evidence 交叉引用

- 冻结 golden（只读消费）：`schema/golden/nlos.sabi.Envelope-v1.hex`、`schema/golden/nlos.sabi.PrincipalHandshake-v1.hex`
- Go 先例（本车道镜像模板）：[b-sdk-go-001](b-sdk-go-001-golden-probe.md)
- 三语言先例：`tests/conformance/schema/envelope.py`、`envelope.ts`；[b-schema-002](b-schema-002-cross-language-generation.md)、[b-schema-006](b-schema-006-typescript-python-ipc-clients.md)
- 冻结纪律：[ADR-0014](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)

## 8. 扩面记录：golden 门 2/4 → 4/4（2026-08-30 追加）

- 触发：W4-G 初版按任务书范围只断言 2 条 golden；本次扩至 Go 先例 [b-sdk-go-001](b-sdk-go-001-golden-probe.md) `golden_test.go` 断言的完整四条，golden 门与 Go 车道完全对齐。
- 验证时仓库 HEAD：`afd05ae71b39e62eaa55487e3cb49e4527d89d36`（运行全程无漂移）。

### 8.1 写集增量

| 路径 | 操作 | 说明 |
|---|---|---|
| `sdk/csharp/llmos-sabi-probe/Program.cs` | 修改 | 新增 2 用例：`TestEnvelopeCommonRequestGolden`（SabiRequestContext 深嵌套全字段面）、`TestEnvelopeCommonUncertainGolden`（SabiResponseContext 含枚举 failure 面）；golden 分区注释同步（门=4 条） |
| `sdk/csharp/llmos-sabi-probe/Codec.cs` | 修改 | 修复 `Unmarshal(SabiRequestContext)` field 7：解码出的 `CapabilityHandle` 未加入 `m.CapabilityHandles`（repeated message 字段解码静默丢弃缺陷；既有 14 用例未覆盖该解码路径，新用例暴露，加一行 `Add` 修复） |
| `docs/evidence/stage-b/b-sdk-csharp-001-golden-probe.md` | 修改 | 本文件：头部状态、§3.2/§3.3 指针、§5 限制 3/4/7、§6 首条、新增 §8 |

写集外零改动；冻结 golden/schema 只读消费；`bin/`、`obj/` 为 HEAD 已跟踪内容，构建验证后按原状恢复（`git checkout --`），不纳入本车道改动。禁 git 提交遵守。

### 8.2 新增用例与输入值域（反推自 sdk/go/sabi/golden_test.go，三语言同源）

- **common-request**：schema `{nlos.sabi.Envelope, major=1, minor=1}`、request_id=00×16、service=`operation`、method=`cancel`；RequestContext 全字段：Caller{principal=01×16, app=02×16, process=03×16, generation=7}、activity_context=`trace`、TaskExecutionBinding{attempt=04×16, authority_term=9, control_epoch=10, cancel_epoch=11, permit_epoch=12, isolation=13}、correlation_id=05×16、idempotency_key=06×16、deadline_monotonic_ns=123456、capability_handles=[{slot=11, gen=2}]、reservation={slot=12, gen=3}、proposal_or_input_digest_sha256=07×32；payload=`abc`。断言：encode byte-equal ＋ 解码后 oneof 臂独占/Caller generation/handles/reservation/binding 全字段 ＋ roundtrip byte-equal。
- **common-uncertain**：同 schema/request_id/service/method；ResponseContext：correlation_id=05×16、operation{08×16, gen=4}、receipts=[09×16]、failure{code=`Uncertain`(13), retry=`QueryOperationOrRetrySameIdempotencyKey`(3), safe_message=`outcome requires reconciliation`}；无 payload。断言：encode byte-equal ＋ 解码后 response 臂独占/operation generation/receipts/failure 枚举与文案/payload 为空 ＋ roundtrip byte-equal。

### 8.3 验证门命令与结果（v2）

```console
$ /var/folders/tb/3__y93_94l7gqht940y91h7c0000gn/T/opencode/dotnet/dotnet --version   # 8.0.424
$ dotnet build sdk/csharp/llmos-sabi-probe/llmos-sabi-probe.csproj                    # BUILD_OK，0 Warning(s) / 0 Error(s)
$ dotnet run --project sdk/csharp/llmos-sabi-probe/llmos-sabi-probe.csproj --no-build
cs golden probe: 16 passed, 0 failed (total 16)                                       # EXIT=0
```

**数字：16/16 用例 PASS（0 FAIL），构建零告警，exit 0。** golden 门 4/4 全部通过 byte-equal ＋ 解码字段断言 ＋ roundtrip 三重门。首次运行 15/16（EXIT=1）：common-request 用例暴露 §8.1 所列 field 7 缺陷；修复后全量重跑 16/16。

### 8.4 工具链状态

临时 .NET SDK 8.0.424（仓库外 `/var/folders/tb/3__y93_94l7gqht940y91h7c0000gn/T/opencode/dotnet/`）在本次扩面时仍存活，`dotnet --version` 复验 8.0.424，直接复用未重下载；SHA512 与官方 release metadata 的一致性校验记录见 §2。若后续车道复现时临时区已被清理，按 §2 流程重新下载并校验后再运行。

### 8.5 已知限制增量（对 §5 的覆盖更新）

- 覆盖面更新为 **4/7 registry golden 条目（Envelope 家族全 4 条）**；剩余未覆盖：`ServiceDirectory.ResolveRequest-v1`、`nlos.canonical.DigestEnvelope-v1/-preimage-v1`（分属 ServiceDirectory/Digest 家族）。
- §5.4 原记录的 common golden 缺口关闭；golden 门与 Go 先例 4/4 一致。
- field 7 缺陷的教训：探针此前从未解码过 repeated message 字段（编码面已有用例、解码面为空），该路径现由 common-request 用例锁定。
