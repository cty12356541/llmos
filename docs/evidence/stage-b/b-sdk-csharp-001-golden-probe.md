# Evidence B-SDK-CSHARP-001：C# 最小 golden 探针（冻结 wire v1-beta 逐字节比对）

- 状态：**PARTIAL_PASS**（golden 门 14/14 全 PASS；探针边界见 §5，与 Go 先例同口径）
- 日期：2026-08-30
- 仓库 HEAD：`bb93e2fbd53cafde0933ebc07f83b4c70f7072e1`（任务开始时基线，验证时无漂移）
- 工作包：`B-SDK-LANG-EVAL`（Go 先例 [b-sdk-go-001](b-sdk-go-001-golden-probe.md) 的第二语言镜像车道）
- 冻结纪律依据：[ADR-0014](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)
- 覆盖的 registry 条目：`nlos.sabi.Envelope`（v1.1）、`nlos.sabi.PrincipalHandshake` 家族（ADR-0011 additive；golden 实际编码的是 `PrincipalHandshakeAttestation`，schema name 载荷为家族名 `nlos.sabi.PrincipalHandshake`）

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

任务规定的 golden 门为上述两条（任务书明确镜像范围），解码后字段断言与 `tests/conformance/schema/envelope.py` 对齐（oneof 未置位、payload 等）。`Envelope-common-request-v1.hex`/`Envelope-common-uncertain-v1.hex` 未纳入本车道 golden 门；codec 已实现其全部消息面（SabiRequestContext/SabiResponseContext 及嵌套），但未做逐字节断言（见 §6 未运行项）。

### 3.3 roundtrip（解码 → 重编码 byte-equal）

两条 golden 全部 `Unmarshal` 后 `Marshal` 与原字节相等：**2/2 PASS**。另有未知字段保持用例：Envelope golden 尾部追加 `a0 06 07`（field 100 varint 7），解码捕获 `UnknownFields` 精确为 3 字节、重编码与扩展输入 byte-equal（镜像 envelope.py 与 Go 车道同款用例）。

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
3. **覆盖面 2/7 registry 条目**：ServiceDirectory、OperationControl、SystemControl、TakeoverControl、WaitControl 未覆盖。
4. **golden 门 2/4**：Go 车道断言过 4 条 golden；本车道按任务书只断言 Envelope-v1 与 PrincipalHandshake-v1 两条，common-request/uncertain 两条 golden 的 byte-equal 未运行（codec 面已实现，补跑成本低）。
5. .NET 工具链为临时下载供给，未进入机器 PATH 与 CI；与 `Google.Protobuf`/protobuf-net 生成代码的交叉比对未做（无 protoc，且探针刻意零依赖）。
6. 解码不校验重复 oneof 臂（后到覆盖），解码枚举不做未知值 fail-closed（proto3 保留未知值）；`Environment.Exit` 码为唯一失败信号，无 xunit/NUnit 报告格式。
7. 测试以 console harness 形式交付（无测试框架依赖）；若后续车道引入 xunit，需迁移 14 个用例的断言形式。

## 6. 未运行项（显式标注）

- `nlos.sabi.Envelope-common-request-v1.hex` / `nlos.sabi.Envelope-common-uncertain-v1.hex` 的 byte-equal 断言（任务书范围外，codec 面已备）
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
