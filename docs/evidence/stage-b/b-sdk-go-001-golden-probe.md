# Evidence B-SDK-GO-001：Go 最小 golden 探针（冻结 wire v1-beta 逐字节比对）

- 状态：**PARTIAL_PASS**
- 日期：2026-08-29
- 仓库 HEAD：`b0badd5892e30eb46f060f53f902616712339085`（任务开始时基线，无漂移）
- 工作包：`B-SDK-LANG-EVAL`（2026-08-29「解封末位排队」首片，先例 B-SCHEMA-002/006）
- 冻结纪律依据：[ADR-0014](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)
- 覆盖的 registry 条目：`nlos.sabi.Envelope`（v1.1）、`nlos.sabi.PrincipalHandshake` 家族（第 7 项，ADR-0011 additive；golden 实际编码的是 `PrincipalHandshakeAttestation`，schema name 载荷为家族名 `nlos.sabi.PrincipalHandshake`）

## 1. 写集清单

| 路径 | 操作 | 说明 |
|---|---|---|
| `sdk/go/go.mod` | 新增 | 独立模块 `example.com/llmos/sdk-go`，`go 1.27`，零第三方依赖 |
| `sdk/go/wire/wire.go` | 新增 | 手写 varint / tag / length-delimited 原语（路线 B） |
| `sdk/go/wire/wire_test.go` | 新增 | varint/tag 边界与非 minimal、截断、group 拒绝用例 |
| `sdk/go/sabi/messages.go` | 新增 | Envelope 家族 + Attestation 结构与冻结枚举常量 |
| `sdk/go/sabi/marshal.go` | 新增 | 确定性编码器（升序 field number、proto3 implicit presence、packed repeated、未知字段尾部追加） |
| `sdk/go/sabi/unmarshal.go` | 新增 | 手写解码器（last-wins、未知字段捕获、group fail-closed、uint32 溢出拒绝） |
| `sdk/go/sabi/golden_test.go` | 新增 | golden byte-equal / roundtrip / 边界 / 失败路径用例 |
| `docs/evidence/stage-b/b-sdk-go-001-golden-probe.md` | 新增 | 本文件 |

未触碰（写集外验证）：`buf.gen.yaml`、`gen/`、`schema/**`、`crates/**` 均零 diff（`git diff --stat` 为空；工作区内 crates/** 与其他 untracked 改动属并行车到既有工作，未读取写、未提交、未回退）。

## 2. 工具链探测结果（如实记录）

| 工具 | PATH 探测 | 常见安装位置探测 | 结论 |
|---|---|---|---|
| `go` | 缺失 | `/usr/local/go`、Homebrew、mise、asdf、MacPorts 均无 | **本机未安装**；为本探针临时下载 `go1.27.0.darwin-arm64.tar.gz`（镜像 golang.google.cn，`go.dev` 不可达；SHA256 `90493b3bbd5e10f91d12153198bf1994fd756399b4fec93b49b0c6e2acdeeb3e` 与官方发布清单一致）解压至仓库外临时目录运行，`go version go1.27.0 darwin/arm64` |
| `protoc` | 缺失 | 无 | 缺失 |
| `protoc-gen-go` | 缺失（`~/go/bin` 无） | 无 | 缺失 |
| `buf` | 缺失 | 无 | 缺失 |

路线判定：protoc/protoc-gen-go/buf 全缺 → **路线 B**（手写 wire format 最小编码器）。`buf.gen.yaml` 完全未动：无法证明 gen/ 零 diff（无 buf 可复现生成），故不添加 Go plugin 条目。go 工具链为一次性临时供给，机器与 CI 均无持久 Go 安装。

## 3. 交付物与比对用例

### 3.1 结构

`sdk/go/wire`（varint/tag/length 原语，fixed32/64 仅可跳过不生成，group 拒绝）＋ `sdk/go/sabi`（11 个手写结构 + 2 个冻结枚举 + 确定性 Marshal/Unmarshal）。确定性契约与 protobuf-go deterministic serialization 对齐：字段升序、零值省略、消息字段指针 presence、repeated uint32 packed、未知字段捕获后于尾部原样追加。

### 3.2 golden byte-equal（编码方向）

固定输入 → `Marshal()` → 与 `schema/golden/*.hex` 逐字节比对：

| golden | 字节数 | 结果 |
|---|---|---|
| `nlos.sabi.Envelope-v1.hex` | 66 | **byte-equal PASS** |
| `nlos.sabi.Envelope-common-request-v1.hex` | 252 | **byte-equal PASS** |
| `nlos.sabi.Envelope-common-uncertain-v1.hex` | 164 | **byte-equal PASS** |
| `nlos.sabi.PrincipalHandshake-v1.hex` | 185 | **byte-equal PASS** |

四条覆盖共 4 golden / 2 registry 条目；字段断言镜像 `tests/conformance/schema/envelope.py`（schema name/major/minor/non_critical_extension_ids=[42]、process_generation=7、idempotency_key=06×16、capability/reservation handle、failure code=13 UNCERTAIN、retry=3 QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY 等）。

### 3.3 roundtrip（解码 → 重编码 byte-equal）

四条 golden 全部 `Unmarshal` 后 `Marshal` 与原字节相等：**4/4 PASS**。另有未知字段保持用例：golden 尾部追加 `a0 06 07`（field 100 varint 7），解码捕获 `UnknownFields` 精确为 3 字节、重编码与扩展输入 byte-equal（镜像 envelope.py 第 73–78 行）。

### 3.4 边界值与失败路径

- varint 编码边界：0/1/127/128/300/16383/16384/MaxInt32/MaxUint32/MaxInt64/MaxUint64（10 字节）共 11 档，编码与解码 roundtrip 全 PASS
- 非 minimal varint 接受（`80 00`=0，protobuf 解码语义）；截断报 `ErrTruncated`；11 字节与第 10 字节带值位溢出报 `ErrOverflow`
- tag 边界：field 15（单字节键 `7a`）→ field 16（`82 01`）→ 2047/2048 → 2^29−1 上限 roundtrip
- 长度前缀边界：128 字节 payload 触发两字节长度前缀 `80 01`，roundtrip 保持
- group wire type（tag `0x0b`）fail-closed 拒绝；uint32 溢出值（varint 2^32 装入 Major）显式拒绝不截断；golden 在字段中部截断（cut=1/28/63）fail-closed，字段边界完整前缀（cut=27）可解码；oneof 双臂置位在 Marshal 时 panic（探针级不变式）

## 4. 验证门命令与结果

```console
$ cd sdk/go && GOTOOLCHAIN=local PATH=<tmp>/go/bin:$PATH go build ./...   # BUILD_OK
$ cd sdk/go && GOTOOLCHAIN=local PATH=<tmp>/go/bin:$PATH go test ./... -count=1 -v
ok  example.com/llmos/sdk-go/sabi  0.171s   # 10/10 PASS
ok  example.com/llmos/sdk-go/wire  0.296s   # 6/6 PASS
```

**数字：16/16 测试函数 PASS（0 FAIL），go build 成功，`gofmt -l` 无输出，`go vet` 无告警。** 复现需仓库外临时 Go 1.27.0（见 §2）。

## 5. 已知限制（探针边界，非完整 SDK）

1. **非完整 SDK**：仅手写 2 条目 frozen wire 面；无 options/map/zigzag/fixed 编码、无反射/描述符、无 UTF-8 校验、无 service 桩、无 oneof 类型化 API（双臂用 panic 而非 typed error）、无 conformance 框架集成。
2. **无 IPC 客户端**：不含 LocalRpcService 传输层（先例 TS/Python 的 ipc 客户端能力未实现）。
3. **C# 未开**：第四/五语言中仅 Go 探针，C# 排队未启动。
4. **覆盖面 2/7 registry 条目**：ServiceDirectory、OperationControl、SystemControl、TakeoverControl、WaitControl 未覆盖。
5. Go 工具链为临时下载供给，未进入机器 PATH 与 CI；与 protobuf-go 生成代码的交叉比对未做（无 protoc-gen-go）。
6. 解码不校验重复 oneof 臂（后到覆盖），解码枚举不做未知值 fail-closed（proto3 保留未知值，语义校验归 SDK 校验层，本探针无该层）。

## 6. 未运行项（显式标注）

- `buf generate` 及 buf 驱动的 Go 代码生成（buf 缺失）；`buf.gen.yaml` 因此零改动
- protoc/protoc-gen-go 生成路径与路线 B 产物的一致性交叉验证（两者均缺失）
- `go test -race`、模糊测试、性能基线（超出探针门）
- 本车道全部 `cargo`/Rust、TS/Python conformance 重跑（并行车道进行中，非本探针职责）
- 提交与推送（任务明令禁 git）

## 7. Evidence 交叉引用

- 冻结 golden（只读消费）：`schema/golden/nlos.sabi.Envelope-v1.hex`、`...Envelope-common-request-v1.hex`、`...Envelope-common-uncertain-v1.hex`、`...PrincipalHandshake-v1.hex`
- 三语言先例：`tests/conformance/schema/envelope.py`、`envelope.ts`；[b-schema-002](b-schema-002-cross-language-generation.md)、[b-schema-006](b-schema-006-typescript-python-ipc-clients.md)
