# 阶段 B 技术选型：单机通用 NLOS

> 状态：`DISCUSSION COMPLETE / POC ACCEPTANCE PENDING`
>
> 日期：2026-07-29
>
> 目标：为阶段 B 的 PoC 和第一个纵切面确定可执行基线。本文不是冻结 KABI/SABI，也不代表候选已经通过生产验证。

## 1. 选型约束

阶段 B 技术必须优先满足：

1. 单机 macOS、Windows、Linux 可运行；
2. 支持 100K logical nodes 和大量 dormant/waiting Fiber，而非一 Agent 一线程；
3. Process/IsolationUnit、Capability、ResourceGroup、Operation 和 Receipt 可被系统强制；
4. durable state 可做 kill/torn-write/recovery 测试；
5. 第三方 Application 可以跨语言、可沙箱、可升级；
6. UI 不是私有控制旁路；
7. 核心协议与具体 runtime/database/UI 解耦；
8. 能从阶段 B 演进到阶段 C，而不提前引入分布式复杂度。

## 2. 推荐基线总览

| 层 | 推荐 | 状态 | 主要理由 |
|---|---|---|---|
| 核心语言 | Rust stable | **推荐进入 ADR** | 内存安全、强类型、跨平台、适合可信 runtime/ABI |
| 异步/Fiber runtime | Tokio + NLOS scheduler facade | **PoC 必须验证** | async/await、多线程 work stealing；不能直接等同 NLOS Scheduler |
| native Process supervisor | Rust `std::process` + 平台 adapter | **PoC** | 先保留 Windows Job/macOS sandbox/Linux cgroup 后端差异 |
| 不可信应用沙箱 | Wasmtime + WASI Component Model | **首选候选** | capability-oriented imports、跨语言组件、可嵌入与资源限制 |
| 本地权威数据库 | SQLite WAL（单 writer service） | **首选候选** | 单机成熟、事务简单、易备份；必须实测 durability/checkpoint |
| 大对象/Artifact | 内容寻址文件存储 + SQLite metadata | **首选候选** | 避免大对象塞入控制数据库；便于版本、校验和 GC |
| 内部 schema/IDL | Protobuf v3 + Buf；canonical 对象另用 deterministic CBOR | **待对比 PoC** | RPC/生成生态成熟；签名/哈希对象不能依赖普通 protobuf 序列化 |
| 本地 IPC | Unix domain socket / Windows named pipe，上层 typed RPC | **PoC** | 本地权限与 Process 边界清晰；协议不绑定 transport |
| observability | tracing + OpenTelemetry，metrics/log 分离 | **推荐** | Rust 生态成熟，可关联 Task/Process/Fiber/Operation |
| 桌面壳 | Tauri 2 + Web UI | **首选候选** | 跨平台、Rust 后端、capability/permission 模型；仍需验证可信 UI 边界 |
| Web UI | TypeScript + React（暂定） | **可替换候选** | 生态与复杂系统 UI 组件充分；不进入核心系统契约 |
| CLI | Rust CLI，共用 SystemControl client | **推荐** | 最早验证 typed API 和手动控制，不等待桌面完成 |
| 构建/CI | Cargo workspace + nextest/clippy/rustfmt + 跨平台 CI | **推荐** | 统一模块与测试入口；后续增加 fuzz/Miri/loom |

## 3. 核心语言：Rust

### 决定建议

阶段 B 的微内核原型、Process supervisor、Resource Manager、Ledger、Driver host 和 CLI 使用 Rust。Python 保留为研究、模型策略、数据实验和外部 Application SDK，不进入 Safety TCB 的首选实现。

### 原因

- nominal types、ownership 和 `Result` 适合实现 ID、handle、generation 和状态机；
- 无 GC pause，更容易进行资源和延迟计量；
- 可同时嵌入 Tokio、Wasmtime、SQLite；
- Windows/macOS/Linux 支持成熟；
- FFI 和生成代码能支撑未来多语言 SDK。

### 风险与约束

- Rust 不自动保证协议、权限或并发正确；
- `unsafe` 必须集中、标注不变量并单独审计；
- 不允许让 Rust 类型布局直接成为稳定 KABI；
- public schema、durable format 与 crate internal type 必须分离。

## 4. Fiber/异步运行时：Tokio，但不外泄

Tokio 官方提供多线程 work-stealing async runtime，适合承载大量 I/O wait 工作；它是阶段 B 的执行引擎候选，不是 NLOS 的 SchedulerDomain、Task、AgentInstance 或 Resource policy。[Tokio 官方说明](https://tokio.rs/)

采用：

```text
NLOS ExecutionFiber / Operation API
              ↓
Runtime Adapter trait
              ↓
Tokio implementation
```

强制边界：

- 不把 `tokio::task::Id` 作为 ExecutionFiberId；
- 不允许业务模块任意 `tokio::spawn` 绕过 CancellationScope/ResourceGroup；
- 所有 spawn/await/timer/channel 经过 runtime facade；
- blocking/CPU-bound 工作进入受控 pool 或独立 Process；
- queue、semaphore、channel 必须有界；
- Activation metering 和 callback fencing 由 NLOS wrapper 注入。

必须 PoC：

- 100K waiting Fiber RSS、wake latency 和取消延迟；
- 1K/10K runnable Fiber 的公平性和 starvation；
- blocking task 隔离；
- parent cancel、late callback、Process crash；
- tracing span 与 ResourceGroup 归集成本。

## 5. 不可信应用执行：Wasmtime/WASI

Wasmtime 是可嵌入的 WebAssembly、WASI 和 Component Model runtime；WASI 文件能力采用 capability-oriented access，Wasmtime 也提供 async 执行、fuel/epoch interruption 和 resource limiter。[Wasmtime 概览](https://docs.wasmtime.dev/)、[安全模型](https://docs.wasmtime.dev/security.html)、[异步嵌入 API](https://docs.wasmtime.dev/api/wasmtime/)

建议用途：

- 第三方 pure-code Application/Driver plugin；
- 跨语言 package component；
- 可控文件、网络、时钟和随机数导入；
- CPU fuel/epoch、memory/table/instance limit；
- capability handle 映射。

不能假设：

- Wasm sandbox 等于完整 Process/tenant 隔离；
- `ResourceLimiter` 覆盖宿主全部内存或 CPU；
- Component Model 已完全冻结；官方稳定性说明仍标注部分提案/实现缺口。[Wasmtime proposal 状态](https://docs.wasmtime.dev/stability-wasm-proposals.html)

PoC 必须对比：

1. in-process Wasmtime；
2. Wasmtime 独立 host Process；
3. native Process adapter。

默认安全建议：互不信任或要求独立强杀的组件采用“独立 Process + Wasmtime”双层隔离；in-process Wasmtime 只在对应 GuaranteeTier 下使用。

## 6. 本地权威状态：SQLite WAL

阶段 B 优先选择 SQLite，采用单一 authority service 写入 Task、Control、Receipt、Package metadata 和索引；Artifact bytes 独立存储。

原因：

- 单机嵌入、事务和恢复模型成熟；
- 便于快照、迁移和本地备份；
- 减少阶段 B 过早运维一个数据库集群。

SQLite WAL 支持并发读与持久 journal mode；WAL、主数据库和 checkpoint 是同一持久状态的一部分，不能把复制单个主文件当成可靠备份。[SQLite WAL 文档](https://www.sqlite.org/wal.html)

约束：

- authority write path 默认单 writer service；
- 明确 `synchronous`、fsync、checkpoint 和备份策略；
- 禁止放在不支持所需共享内存语义的网络文件系统；
- ledger/canonical event 使用 append-oriented schema 和 idempotency table；
- schema migration 必须可回滚或可恢复；
- 高吞吐时先测量，再决定是否引入专用 KV/日志引擎。

PoC：

- kill -9/torn-write/disk-full；
- WAL checkpoint 与长读事务；
- 10K/100K TaskNode metadata；
- dedup/idempotency；
- snapshot/restore 和 schema migration。

阶段 B 暂不选 PostgreSQL、etcd 或分布式 NewSQL 作为本机硬依赖；它们在阶段 C 重新评估。

## 7. Artifact 存储

采用：

```text
ArtifactId + revision + ContentDigest  → SQLite metadata
ContentDigest                          → local content-addressed bytes
```

要求：

- 临时写入、内容校验、fsync、atomic rename、父目录 fsync；
- metadata commit 与 blob presence 的 recovery/reconcile；
- Package、Artifact、Context cache 分目录和 retention domain；
- Context/KV/embedding 是可回收派生缓存，不是 Artifact 本体；
- 后续可用同一接口替换为对象存储。

## 8. IDL、编码和 IPC

候选基线：

- service/RPC schema：Protobuf + Buf；
- signed/canonical object：deterministic CBOR；
- local transport：Unix domain socket 与 Windows named pipe；
- streaming：基于同一 Operation/Channel envelope 的 bounded stream。

选择原因：RPC 生成生态与 canonical signing 需求不同，不强求“一种编码解决全部问题”。

PoC 对比：

| 候选 | 重点 |
|---|---|
| Protobuf + Buf | 多语言生成、breaking check、unknown field |
| Cap'n Proto | 零拷贝收益、schema evolution、生态 |
| FlatBuffers | 大型只读对象、复杂度和 mutation 成本 |
| JSON/CBOR | 调试性、canonical、安全解析、性能 |

冻结前必须生成 Rust + TypeScript + Python client，并通过 unknown field、major version、golden vector 和 fuzz test。

## 9. Desktop 与可信控制面

Tauri 2 作为阶段 B Task Manager/Resource Monitor 的首选候选。其 runtime authority 能按窗口/origin 检查 capability、permission 和 scope，但 NLOS 权限最终仍由 SystemControl service 校验，不能把 Tauri ACL 当成 NLOS Capability。[Tauri Runtime Authority](https://v2.tauri.app/security/runtime-authority/)

边界：

```text
WebView（不可信展示）
  → Tauri command allowlist
    → local typed SystemControl client
      → NLOS Capability + generation CAS + Receipt
```

第一版 UI 只做：

- Application/Task/Process/AgentInstance/Fiber 树；
- ResourceGroup 与 pressure；
- Operation 和 effect state；
- inspect/pause/resume/cancel/kill/throttle；
- ControlCommand 状态和 Receipt。

不在第一版实现窗口管理器、应用商店或完整桌面 compositor。

## 10. 可观测性与测试

### 可观测性

- Rust `tracing` 作为内部 structured instrumentation；
- OpenTelemetry 作为可替换导出协议；
- metrics、logs、traces、audit ledger 分离；
- span 至少关联 Application、TaskAttempt、Process、AgentInstance、Fiber、Activation、Operation 和 ResourceGroup。

### 测试工具

| 风险 | 建议工具/方法 |
|---|---|
| 状态机与守恒 | property-based testing |
| 并发竞态 | Loom 或确定性 scheduler test |
| 解析与 ABI | cargo-fuzz、golden vectors |
| unsafe/UB | Miri、sanitizer |
| durable recovery | 子进程 kill、fault-injection VFS、磁盘满 |
| 性能 | Criterion + 独立 workload harness |
| 跨平台 | macOS/Windows/Linux CI matrix |

## 11. 暂不选择

- **Kubernetes 作为阶段 B 核心**：它属于部署环境，不是本机 NLOS 执行模型。
- **Actor framework 作为内核对象模型**：可借鉴 mailbox/supervision，但不能让框架 identity 取代 NLOS ID、Capability 和 Receipt。
- **一 Agent 一 OS Process/Thread**：无法满足逻辑 PID 级目标。
- **Electron 作为默认结论**：保留为 Tauri PoC 失败后的对照，不先承担更大资源基线。
- **分布式数据库**：阶段 B 不为阶段 C 预付全部一致性和运维复杂度。
- **Python 作为 Safety TCB 主实现**：保留 SDK/策略/实验用途。

## 12. 第一批 ADR 与 PoC

| 顺序 | ADR/PoC | 退出条件 |
|---:|---|---|
| 1 | ADR-0001 Rust workspace 与模块边界 | 编译、lint、test、最小 ID/schema crate |
| 2 | PoC-0001 Tokio Fiber runtime facade | 100K waiting Fiber + cancel/fence/meter 报告 |
| 3 | ADR-0002 SQLite authority store | durability、dedup、migration、100K metadata 通过 |
| 4 | PoC-0004 Wasmtime isolation | capability imports、fuel/epoch、memory limit、host crash 测试 |
| 5 | ADR-0003 IDL/canonical encoding | 三语言生成、compat check、golden/fuzz vectors |
| 6 | PoC-0005 Process supervisor | macOS/Windows/Linux suspend/kill/resource mapping 能力表 |
| 7 | PoC-0006 Tauri SystemControl UI | GUI 与 CLI 产生等价 ControlCommand/Receipt |

## 13. 首个代码仓库布局建议

```text
Cargo.toml
crates/
  nlos-types
  nlos-schema
  nlos-runtime
  nlos-kernel
  nlos-store
  nlos-resource
  nlos-supervisor
  nlos-driver-host
  nlos-system-control
  nlos-cli
apps/
  task-manager
tests/
  conformance
  fault-injection
  scale
docs/
  management/adrs
```

模块依赖方向必须从 adapter 指向稳定 contract；`nlos-types` 不依赖 Tokio、SQLite、Wasmtime 或 Tauri。

## 14. 下一步

当前已完成 ADR-0001、PoC-0001、PoC-0002、ADR-0002/PoC-0003 的初版和 PoC-0004；`B-STORE-FAULT` 的 F1–F7 已通过 fault/recovery/migration/100K metadata 与 Ubuntu/Windows/macOS CI，工作包 `DONE`。下一主线以[阶段 B 权威进度单](./stage-b-progress.md)的 `B-SCHEMA` 为准：

1. Protobuf/CBOR IDL 与 schema registry；
2. Rust/TypeScript/Python 生成和版本兼容；
3. golden vector、fuzz 与本地 typed IPC adapter；
4. 100K 逐条生产写入、真实掉电和更多文件系统保留为 Store 扩展 Evidence。

技术栈讨论已于[议题 30](../discussions/30-阶段B技术栈讨论.md)收束。编码立即从 Cargo workspace、`nlos-types`、`nlos-runtime` 与 Tokio Fiber scale PoC 开始；带 `PoC` 标记的组件在证据出来前不得升级为 `ACCEPTED` 或冻结公共 ABI。

PoC 进度：[PoC-0001 Tokio Fiber Runtime 初始证据](../evidence/stage-b/poc-0001-tokio-fiber-runtime.md)已取得 `PARTIAL PASS`：2 个 worker 上 100K waiting Fiber 测试通过，最大 RSS 约 128.39 MiB；Operation callback fence、fairness、结构化 join/detach、record GC、CPU 分维计量和跨平台验证仍待完成。

[PoC-0002 Operation Callback Fence](../evidence/stage-b/poc-0002-operation-callback-fence.md)也已取得 `PARTIAL PASS`：cancel epoch、迟到/重复 callback、dispatch ticket identity、generation fence 和 cancel/completion 竞态在线程安全内存 Registry 中通过。

[ADR-0002 / PoC-0003 SQLite Operation Authority](./adrs/0002-stage-b-sqlite-operation-authority.md)已完成首个持久化切片：Operation 转换、Receipt identity 和 Wake/Reconcile Outbox 在 WAL/FULL 单写者事务中提交，并通过重开与无析构进程退出恢复测试；~~Tokio consumer 集成仍待完成~~ **已由 [PoC-0004](../evidence/stage-b/poc-0004-outbox-wake-consumer.md) 补齐（2026-08-01）**。F1–F7 已依次补齐 fault/recovery、v1→v2 migration、100K metadata 与 Ubuntu/Windows/macOS CI；真实硬件掉电、更多文件系统和 100K 逐条生产写入保留为扩展 Evidence。
