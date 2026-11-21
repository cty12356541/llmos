# NLOS 多语言 SDK 支持评估计划

> Work Item：`B-SDK-LANG-EVAL`（由阶段 E `E1` / `W82-D-PY-TS-SDK-WAIST` 持续演进，并由 `DEL-BASE` / `DEL-SDK` 承接官方发行门）
>
> 状态：`IN_PROGRESS / PARTIAL_PASS`（2026-11-21 依 `W82-R2` 实测与 [del-base-coverage-audit.md §5 表 4](./del-base-coverage-audit.md#5-表-4多语言-sdk-探针-vs-正式支持等级矩阵) 校准；注：2026-08-04 曾为 `BLOCKED / GATE B DEFERRED`，随后 Gate B Go/C# 黄金探针已于 `W29-G`/`W77`/`W82-B1` 解封并入三平台 CI；当前五语言均**未达到 `SDK-4 OFFICIAL SDK`**）
>
> 日期：2026-11-21（初立于 2026-08-04）
>
> 适用基线：架构总纲 v0.5、ADR-0003/0014/0024/0027、`B-SCHEMA`、`SABI v1.1`（12 通道）

## 1. 目标与边界

本计划回答两个不同问题：

1. 哪些语言应当拥有官方 NLOS Application SDK；
2. 哪些语言只需要能生成 schema、通过兼容测试或作为 WASM guest 使用。

语言支持不得与内核实现语言混淆：

```text
Rust NLOS kernel / authority services
                ↑
       versioned SABI + typed IPC
                ↑
Rust / TypeScript / Python / Go / C# / ... SDK
```

Rust 继续作为阶段 B 核心实现语言。增加 Go、C# 或其他 SDK 不代表把这些语言放入 Safety TCB，也不允许它们绕过 ServiceDirectory、Capability、deadline/cancel、Operation、Receipt 和版本协商直接调用内部 Rust ABI。

## 2. “支持一种语言”的等级

| 等级 | 含义 | 可以声称什么 |
|---|---|---|
| `SDK-0 SCHEMA-CAPABLE` | 官方/受审 generator 能从同一 `.proto` 产生消息类型 | 只能读写 schema；不能声称可调用 NLOS |
| `SDK-1 COMPAT-VERIFIED` | golden、major/minor、critical extension、unknown field 和 malformed/oversized conformance 通过 | 与当前 wire schema 的受测兼容 |
| `SDK-2 IPC-CAPABLE` | 在声明平台上实现 Unix socket/Windows named pipe framing、timeout、断连和 backpressure | 可建立受测本地 typed IPC |
| `SDK-3 SABI-CAPABLE` | ServiceDirectory/negotiation、typed error、deadline/cancel、idempotency、Operation/Receipt 和 uncertain retry 语义通过 | 可调用声明范围内的 NLOS 服务 |
| `SDK-4 OFFICIAL SDK` | 固定工具链、包发布、升级策略、文档、示例、三平台 CI、安全审查和兼容承诺齐备 | NLOS 正式维护该语言/平台 profile |

任何语言不得从 `SDK-0` 直接标为“官方支持”。官方 SDK 必须声明具体 OS、runtime 和 feature profile；例如 Node.js TypeScript SDK 不自动等于浏览器/WebView 可以直接访问本地 pipe。

## 3. 评估维度

每个候选使用同一组问题评估，不以语言流行度直接决定：

1. **产品覆盖**：是否覆盖 NLOS 预期的桌面、AI、企业、系统工具或远程节点开发者；
2. **Schema 工具链**：是否有可固定版本、可重复生成、可审查的 Protobuf runtime/generator；
3. **本地 IPC**：macOS/Linux Unix socket 与 Windows named pipe 是否有成熟异步 API，能否取得足够 peer identity；
4. **SABI 语义**：能否无损表达 nominal ID、`u32/u64`、bytes、deadline、cancel、stream、partial/uncertain error 和 Receipt；
5. **安全边界**：是否容易误用远程 pipe、默认宽权限、隐式字符串转换、反序列化动态类型或不受控 FFI；
6. **分发升级**：包管理、runtime 最低版本、签名、离线生成物和多架构发布成本；
7. **维护成本**：CI 时间、依赖数量、长期兼容策略、文档和示例负担；
8. **独立验证价值**：是否能发现当前 Rust/Node/Python 都未暴露的协议假设。

截至 2026-08-02，Protobuf 官方文档列有 C++、C#、Dart、Go、Java、Kotlin、Objective-C、Python、Rust、Ruby 等生成支持；这只证明候选入口存在，不证明 NLOS transport 或 SABI 语义成立。[Protobuf language reference](https://protobuf.dev/reference/)、[Protobuf editions generation guide](https://protobuf.dev/programming-guides/editions/)

## 4. 当前候选分层

> 2026-11-21 校准口径（[del-base-coverage-audit.md §5 表 4](./del-base-coverage-audit.md#5-表-4多语言-sdk-探针-vs-正式支持等级矩阵)）：严格禁止宣称任何语言达到 `SDK-4 OFFICIAL SDK`。

| 优先级 | 语言 | 当前证据级别 | 当前定位 | 主要价值 | 主要待证风险 |
|---|---|---|---|---|---|
| P0 | Rust | **仓内 `SDK-3 SABI-CAPABLE`（非 `SDK-4`）** | 核心实现 + 仓内首个完整 SABI 消费面 | 内核、系统服务、CLI、可信 adapter、全 12 通道编解码与多路 IPC | 不能让内部 Rust type/layout 泄漏成公共 ABI；缺仓外独立 SDK 门面 crate、semver 策略与独立消费验收（归 `DEL-SDK`） |
| P0 | TypeScript | **`SDK-2 PASS` + `SDK-3 PARTIAL_PASS`（仅限 Node.js `>=20`，非 WebView，非 `SDK-4`）** | 阶段 B/E 官方 SDK 候选（[W82-D-TS-SDK](../evidence/stage-e/w82-d-ts-sdk-20261121.md) macOS 本地已验收） | Tauri/Node 控制工具、桌面 UI 配套生态；已补齐 `SABI_V1_CHANNELS` 12 通道、`node:crypto` Ed25519 持久认证会话、`Wait`/`Operation`/`LlmGateway`/`ToolSandbox`/`BlobReference` 客户端与 `12/12` 跨进程一致性 | live daemon 尚未全 8 通道实时挂载（阻塞于 `GAP-1/2/3` 与 [ADR-0028](./adrs/0028-w82-int-service-authorization-and-wire-binding.md) 签认）；Windows Named Pipe 仅代码对齐未在 Windows 实测；`sdk/typescript/` 无独立 `package.json`、未发布 npm、缺文档与三平台 CI 复跑（归 `W82-R4` / `DEL-SDK`） |
| P0 | Python | **`SDK-2 PASS` + `SDK-3 PARTIAL_PASS`（非 `SDK-4`）** | 阶段 B/E 官方 SDK 候选（[W82-D-PY-SDK](../evidence/stage-e/w82-d-py-sdk-20261121.md) macOS 本地已验收） | AI、模型策略、数据与自动化生态；已补齐 `SABI_V1_CHANNELS` 12 通道、`Ed25519Signer` 持久认证会话、`Wait`/`Operation`/`LlmGateway`/`ToolSandbox`/`BlobReference` 客户端、跨平台 `verify_wheel.py`（源码树外剥离 `PYTHONPATH` 校验 12 个 `*_pb2.py` 零漂移）与 `6/6` 守护进程一致性 | live daemon 尚未全 8 通道实时挂载（阻塞于 `GAP-1/2/3` 与 [ADR-0028](./adrs/0028-w82-int-service-authorization-and-wire-binding.md) 签认）；三平台 CI 待复跑；`verify_wheel.py` 仅校验本地 wheel，绝不等于已发布 PyPI，缺独立开发者消费文档（归 `W82-R4` / `DEL-SDK`） |
| P1 | C#/.NET | **`SDK-0 + SDK-1 PARTIAL`（纯黄金探针，非可用 SDK）** | 线协议兼容性独立探针（`.NET 8.0` 零依赖，已入三平台 CI） | Windows 桌面、企业、`.NET` 黄金验证；已同步 `SabiRegistry.SabiV1Channels` 12 通道身份校验与 `v1.0` 9 通道黄金向量（`30/30`） | 无 `.proto` 代码生成链（手写 codec）；缺 `v1.1` 三通道黄金向量；完全缺失 `SDK-2`（Named Pipe / Unix socket）、`SDK-3` 与 `SDK-4`（NuGet 发布）（`DEL-SDK` 有真实消费方与 owner 时再立项） |
| P1 | Go | **`SDK-0 + SDK-1 PARTIAL`（纯黄金探针，非可用 SDK）** | 线协议兼容性独立探针（Go `1.27.0` 零依赖，已入三平台 CI） | 线协议独立跨编译器探针；已同步 `SabiV1Channels` 12 通道身份校验、`TestCompatVer002SchemaIdentityValidation` 与 `v1.0` 9 通道黄金向量（`go test ./... -count=1`） | 无 `.proto` 代码生成链（手写 struct/codec）；缺 `v1.1` 三通道黄金向量；完全缺失 `SDK-2`（UnixConn / Named Pipe）、`SDK-3` 与 `SDK-4`（`DEL-SDK` 有真实消费方与 owner 时再立项） |
| P2 | Java/Kotlin | `UNASSESSED` | 需求驱动评估 | 企业服务、JVM 工具、Android/Kotlin 生态 | Windows named pipe 缺少同等直接标准路径；JVM/runtime 体量 |
| P2 | Swift | `UNASSESSED` | Apple profile 候选 | macOS 原生 Application/UI 与 Apple 开发生态 | Windows/Linux 覆盖弱；跨平台官方 SDK 维护收益不足 |
| P3 | C/C++ | `UNASSESSED` | 低层 bridge/兼容探针，不优先做完整 SDK | 既有 native 软件、Driver/游戏/高性能库 | 内存安全、ABI/编译器矩阵、ownership/cancel/error 表达成本高 |
| WATCH | Dart、Ruby、PHP 等 | `UNASSESSED` | 保持 schema 可生成性观察 | 特定 UI、脚本或 Web 后端生态 | 当前缺乏足以承担正式 SDK 维护成本的 NLOS 用例 |

进展演进与约束（历史注记与现行边界）：

- **语言支持现状（2026-11-21）**：Rust 达仓内 `SDK-3`（非 `SDK-4`）；Python 与 TypeScript（Node.js）在 macOS 本地达成 `SDK-2 PASS` 与 `SDK-3 PARTIAL_PASS`（非 `SDK-4`），其全 8 通道实时挂载仍待 [ADR-0028](./adrs/0028-w82-int-service-authorization-and-wire-binding.md) 签认与 `W82-R4` 合流；Go 与 C# 均仅停留在 `SDK-0 + SDK-1 PARTIAL` 纯黄金探针，不是可用 SDK；
- **Go 与 C# 探针历史与当前定位**：`2026-08-04`（议题 31/32）曾将 Gate B 后移至 `B-TASK`/EffectPermit 纵切面之后；随后 Gate B 已于 `W29-G`、`W77`（[E-SABI-002](../evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md)）及 `W82-B1`（`4fc70be1`）解封并在三平台 CI 常跑，目前维持低成本线协议黄金探针定位（无真实外部消费方与维护 owner 前不进入 `SDK-2..4`）；
- **C# 可优先进行 Windows transport PoC**，因为 .NET 官方提供 `NamedPipeClientStream/NamedPipeServerStream`，且 NLOS 最高目标明确包含 Windows 级桌面系统；[Microsoft .NET pipe operations](https://learn.microsoft.com/en-us/dotnet/standard/io/pipe-operations)
- **Go 可优先进行 Unix/服务工具 PoC**，标准库直接提供 `UnixConn`；Windows named-pipe 依赖和身份语义必须单独选型，不能假设与 Unix 等价；[Go `net.UnixConn`](https://pkg.go.dev/net#UnixConn)
- Java/Kotlin、Swift 不进入阶段 B 必交矩阵，但 schema 设计不得主动制造其无法表达的语义；Java 标准库已有 Unix-domain socket API，可作为后续 Unix profile 起点。[Java `UnixDomainSocketAddress`](https://docs.oracle.com/en/java/javase/21/docs/api/java.base/java/net/UnixDomainSocketAddress.html)
- WASM guest language 支持单独按 WIT/Component Model 能力评估，不因为某语言拥有 native IPC SDK 就自动宣称拥有 sandbox guest SDK。[WebAssembly Component Model language support](https://component-model.bytecodealliance.org/language-support.html)

## 5. Stage B 执行顺序

### Gate A：完成现有三语言闭环

1. TypeScript Node client 完成 framing、Unix/Windows transport、timeout 和 fail-closed；
2. Python asyncio client 完成同一能力；
3. 两者接入 ServiceDirectory/negotiation 和最小 common SABI semantics；
4. Rust/TypeScript/Python 运行同一跨语言服务端/客户端矩阵，而不只读取同一 golden 文件。

### Gate B：Go/C# 低成本独立兼容探针

> 2026-08-04 起本 Gate 后移至 `B-TASK`/EffectPermit 纵切面通过之后（议题 31/32 顺序变更）；下列内容保持为启动时的执行定义。
> **2026-11-21 状态同步**：`W77` [E-SABI-002](../evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md) 已将 Go / C# 纳入五语言 `v1.0` 黄金探针（含 12 通道身份校验与 `v1.0` 线级兼容），目前维持 **`SDK-0 + SDK-1 PARTIAL`**（缺 `SABI v1.1` 增量黄金向量及 `SDK-2/3/4`）；参见 [DEL-BASE §5 表 4](./del-base-coverage-audit.md)。

1. 固定 Go 与 C# generator/runtime 版本，生成物 checked in 或提供可验证的离线恢复方式；
2. 读取 Rust golden，覆盖 major/critical、unknown field、malformed 和 frame bound；
3. 验证 bytes/unsigned integer/enum/optional/unknown-field 的语言映射；
4. 记录 package namespace、runtime 最低版本、license、供应链和生成漂移风险。

Buf 支持固定 remote plugin 并从同一配置生成 Go 等语言；引入时仍必须固定完整版本并保留 drift gate，不能使用浮动 latest。[Buf remote plugins](https://buf.build/docs/bsr/remote-plugins/)

### Gate C：transport 与 common semantics

对晋级候选执行：

- Unix socket 与 Windows named pipe 真实往返；
- connect/read/write timeout、半帧、超界、服务重启、失联和连接 poison；
- ServiceDirectory resolve/negotiate，不写死 endpoint；
- cancel/deadline、同 IdempotencyKey retry、`E_UNCERTAIN`、partial failure 和 Receipt；
- peer identity/authorization hook 与日志脱敏；
- 与 Rust server 交叉组合，不允许仅做同语言 client/server 自测。

### Gate D：是否晋级官方 SDK

满足 `SDK-3` 后，再根据真实消费方、维护 owner、发布渠道、CI 成本和安全审查决定是否进入 `SDK-4`。若没有实际 Application/工具消费方，可保留为 conformance probe，不承担永久官方 SDK 承诺。

## 6. 计划产物与决策点

| 产物 | 状态 | 归属 |
|---|---|---|
| Rust/TS/Python transport | `PARTIAL PASS`，三平台 run 30734744799 成功 | `B-SCHEMA`、B-SCHEMA-006 |
| ServiceDirectory schema + Rust negotiation core | `PARTIAL PASS`，三平台 run 30735589673 成功 | `B-SCHEMA`、B-SCHEMA-007 |
| TS/Python directory negotiate-and-connect | `PARTIAL PASS`，三平台 run 30736741324 成功 | `B-SCHEMA`、B-SCHEMA-008 |
| SABI common metadata/error/safe-retry validation | `PARTIAL PASS`，三平台 run 30737782776 成功 | `B-SCHEMA`、B-SCHEMA-009 |
| Durable idempotency authority + reconnect IPC | `PARTIAL PASS`，SQLite v3 与 TS/Python 真实断线重连均通过三平台 run 30740180511 | `B-SCHEMA`、`B-STORE`、B-SCHEMA-010/011 |
| Deadline/cancel/uncertain + OperationControl/timer worker | `PARTIAL PASS`，B-SCHEMA-012/013 均已通过三平台，最新 run 30743421174 | `B-SCHEMA`、`B-STORE`、`B-PROCESS` |
| Python/TS SABI 最小完备窄腰（`L-008` ~ `L-015`：单连接多轮复用、`WaitClient`/`OperationClient`/`--auth-socket`、`nlos-sdk` 独立 wheel 打包 `gen/python`、受控 LLM 五维预算网关、`ToolSandboxClient` 与 `BlobReferenceClient` 大载荷引用客户端） | `SDK-2 PASS + SDK-3 PARTIAL_PASS`（macOS 本地已验收：[W82-D-PY-SDK](../evidence/stage-e/w82-d-py-sdk-20261121.md)、[W82-D-TS-SDK](../evidence/stage-e/w82-d-ts-sdk-20261121.md)，覆盖 12 通道表、Ed25519 持久认证会话、5 类新客户端与源码树外 `verify_wheel.py` 12 个 `*_pb2.py` 零漂移；因 live daemon 尚未全 8 通道实时挂载且三平台 CI / PyPI / npm 发布未完成，维持 `SDK-3 PARTIAL_PASS`，非 `SDK-4`） | [stage-e-progress.md](./stage-e-progress.md) `§E.6.1.8` / `W82-D-SDK` |
| Go generation/golden probe | `PARTIAL_PASS`（`SDK-0 + SDK-1 PARTIAL`：已覆盖 12 通道身份校验与 `v1.0` 9 通道 11 向量黄金校验，三平台 CI `go test ./... -count=1` 常跑；缺 `.proto` 代码生成链、`v1.1` 向量及 `SDK-2/3/4`） | `B-SDK-LANG-EVAL` / `E-SABI-002` |
| C# generation/golden probe | `PARTIAL_PASS`（`SDK-0 + SDK-1 PARTIAL`：已覆盖 12 通道身份校验与 `v1.0` 9 通道 11 向量黄金校验，三平台 CI `30/30` 常跑；缺 `.proto` 代码生成链、`v1.1` 向量及 `SDK-2/3/4`） | `B-SDK-LANG-EVAL` / `E-SABI-002` |
| Go/C# transport 对比 Evidence | `DEFERRED`（仅在出现真实外部消费方与维护 owner 触发 Gate C/D 时再立项） | `DEL-SDK` |
| 官方 SDK 语言集合 ADR 与 `SDK-4` 发布流水线 | `PLANNED`（当前无任何语言达到 `SDK-4 OFFICIAL SDK`；须完成独立包发布、三平台 CI 与仓外临时项目验收） | `DEL-SDK` |
| Java/Kotlin、Swift、C/C++ 复审 | `DEFERRED / DEMAND-DRIVEN` | `DEL-SDK` 或具体消费者触发 |

复审触发器：

- 出现明确的 Go、.NET、JVM、Swift 或 native 第三方 Application；
- SABI schema 加入某语言难以无损表达的类型或状态；
- 某 runtime 无法满足 peer authorization、cancel/deadline、unknown forwarding 或 uncertain retry；
- SDK CI/供应链维护成本明显超过实际覆盖收益；
- WASI Component Model 为某候选提供比 native IPC 更合适的路径。

## 7. 当前结论

当前依据实测证据核定的分语言能力状态如下（详见 [del-base-coverage-audit.md §5 表 4](./del-base-coverage-audit.md#5-表-4多语言-sdk-探针-vs-正式支持等级矩阵)）：

```text
仓内 SDK-3 SABI-CAPABLE（非 SDK-4）：Rust
本地 SDK-2 PASS + SDK-3 PARTIAL_PASS（仅 Node.js / asyncio macOS 实测，非 SDK-4）：TypeScript + Python
SDK-0 + SDK-1 PARTIAL 纯黄金探针（已入三平台 CI，缺 v1.1 向量与 SDK-2/3/4）：Go + C#
需求驱动候选（UNASSESSED）：Java/Kotlin + Swift
低层桥接候选（UNASSESSED）：C/C++
观察池（UNASSESSED）：Dart/Ruby/PHP 等
```

**严正边界**：当前五语言均**未达到 `SDK-4 OFFICIAL SDK`**；Go/C# 纯黄金探针不等于可用或官方支持 SDK；Python/TypeScript 在 live daemon 全 8 通道实时挂载、三平台 CI 复跑、独立包元数据（TS `package.json`）及 PyPI/npm 正式发布与独立仓外消费验收完成前，一律维持 `SDK-3 PARTIAL_PASS`。
