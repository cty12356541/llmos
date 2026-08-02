# NLOS 多语言 SDK 支持评估计划

> Work Item：`B-SDK-LANG-EVAL`
>
> 状态：`READY / EVALUATION PLANNED`
>
> 日期：2026-08-02
>
> 适用基线：架构总纲 v0.5、ADR-0003、`B-SCHEMA`

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

| 优先级 | 语言 | 当前证据级别 | 当前定位 | 主要价值 | 主要待证风险 |
|---|---|---|---|---|---|
| P0 | Rust | `SDK-2 PARTIAL` | 核心实现 + 首个 SDK | 内核、系统服务、CLI、可信 adapter | 不能让 Rust type/layout 泄漏成公共 ABI |
| P0 | TypeScript | `SDK-2 CANDIDATE / PARTIAL` | 阶段 B 官方 SDK 候选 | Tauri/Node 控制工具、桌面 UI 配套生态 | WebView 与 Node 权限边界；discovery/common semantics 尚未完成 |
| P0 | Python | `SDK-2 CANDIDATE / PARTIAL` | 阶段 B 官方 SDK 候选 | AI、模型策略、数据与自动化生态 | Windows Proactor 稳定边界、打包与 common semantics 尚未完成 |
| P1 | C#/.NET | `PLANNED / UNASSESSED` | 优先评估，官方 SDK 候选 | Windows 桌面、企业、Unity/.NET；原生 `System.IO.Pipes` | macOS/Linux profile、peer identity、版本最低线和 NuGet 发布 |
| P1 | Go | `PLANNED / UNASSESSED` | 优先评估，官方 SDK 候选 | CLI、系统代理、基础设施和后续远程节点；部署简单 | Windows named pipe 通常需额外 adapter；context cancel 与 uncertain retry 映射 |
| P2 | Java/Kotlin | `UNASSESSED` | 需求驱动评估 | 企业服务、JVM 工具、Android/Kotlin 生态 | Windows named pipe 缺少同等直接标准路径；JVM/runtime 体量 |
| P2 | Swift | `UNASSESSED` | Apple profile 候选 | macOS 原生 Application/UI 与 Apple 开发生态 | Windows/Linux 覆盖弱；跨平台官方 SDK 维护收益不足 |
| P3 | C/C++ | `UNASSESSED` | 低层 bridge/兼容探针，不优先做完整 SDK | 既有 native 软件、Driver/游戏/高性能库 | 内存安全、ABI/编译器矩阵、ownership/cancel/error 表达成本高 |
| WATCH | Dart、Ruby、PHP 等 | `UNASSESSED` | 保持 schema 可生成性观察 | 特定 UI、脚本或 Web 后端生态 | 当前缺乏足以承担正式 SDK 维护成本的 NLOS 用例 |

初步建议不是永久决定：

- **阶段 B 当前主线不变**：先完成 Rust/TypeScript/Python 的 `SDK-3`；
- **Go 与 C# 正式进入 P1 评估计划**：两者都做 `SDK-0–SDK-1` 生成/golden 探针；至少一个在 SABI 冻结前完成 `SDK-2` 跨平台 IPC 探针；
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
| TS/Python directory resolver + SABI common semantics | `NEXT` | `B-SCHEMA` |
| Go generation/golden probe | `READY`，Gate A 后实施 | `B-SDK-LANG-EVAL` |
| C# generation/golden probe | `READY`，Gate A 后实施 | `B-SDK-LANG-EVAL` |
| Go/C# transport 对比 Evidence | `PLANNED` | 新 Evidence，不提前编号 |
| 官方 SDK 语言集合 ADR | `PLANNED` | 至少一个 P1 transport PoC 后创建 |
| Java/Kotlin、Swift、C/C++ 复审 | `DEFERRED / DEMAND-DRIVEN` | Stage B 后段或具体消费者触发 |

复审触发器：

- 出现明确的 Go、.NET、JVM、Swift 或 native 第三方 Application；
- SABI schema 加入某语言难以无损表达的类型或状态；
- 某 runtime 无法满足 peer authorization、cancel/deadline、unknown forwarding 或 uncertain retry；
- SDK CI/供应链维护成本明显超过实际覆盖收益；
- WASI Component Model 为某候选提供比 native IPC 更合适的路径。

## 7. 当前结论

当前纳入计划的不是“立刻正式支持所有语言”，而是：

```text
阶段 B 官方候选：Rust + TypeScript + Python
优先兼容评估：Go + C#
需求驱动候选：Java/Kotlin + Swift
低层桥接候选：C/C++
观察池：Dart/Ruby/PHP 等
```

在 Go/C# 探针与官方 SDK ADR 完成前，项目只能声称它们是 `PLANNED`，不能声称 NLOS 已支持这些语言。
