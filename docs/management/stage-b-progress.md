# 阶段 B 权威进度单

> 状态：`ACTIVE / POC ACCEPTANCE PENDING`
>
> 最后更新：2026-08-02
>
> 权威用途：这是阶段 B 工作项、实现事实、验证证据和下一验收门的唯一汇总入口。它不替代 v0.5 架构规范、ADR 或 Evidence；每一项状态都必须能下钻到这些权威对象。

## 1. 阶段目标

阶段 B 要把 NLOS 从设计地基推进到可运行的单机通用应用平台，至少贯通：

```text
Application
  → Task / TaskPlan
  → Process / AgentInstance / ExecutionFiber
  → async Operation
  → durable Receipt / Artifact
  → cancel / crash recovery
  → CLI / GUI / NL 共用 ControlCommand
```

阶段 B 不是“已经完成的产品版本”。在 `ROAD-B-001` 至 `ROAD-B-006` 的退出门全部取得足够 Evidence 前，不得声称已经具备 Windows/macOS 级完整系统能力，也不得把局部 PoC 外推成 PID 级 Agent 容量。

## 2. 状态语义

| 状态 | 含义 |
|---|---|
| `DONE` | 工作项实现、验证、Evidence 和文档同步完成；不等于阶段退出 |
| `PARTIAL_PASS` | 局部原型和指定测试通过，但仍有明确验收缺口 |
| `IN_PROGRESS` | 已开始实现，尚未形成可独立验收结果 |
| `READY` | 边界、依赖和验收条件已具备，可以开始实现 |
| `BLOCKED` | 有明确阻塞原因，需要外部决定或前置能力 |
| `NOT_STARTED` | 尚未开始 |

证据等级沿用 v0.5 的 H0–H8；本表不把状态名当作生产保证。`PARTIAL_PASS` 只能声称 Evidence 覆盖的局部范围。

## 3. 当前工作包总览

| ID | 工作包 | 当前状态 | 实现/证据 | 主要未决项 |
|---|---|---|---|---|
| `B-MGMT` | 最高目标、v0.5 规范、渐进式披露、CRUD、原子提交规则 | `DONE` | [项目管理机制](./README.md)、[知识规则](./project-knowledge-progressive-disclosure.md) | claims/risk/evidence 机器台账仍待建立 |
| `B-TYPES` | Rust workspace 与稳定 nominal ID / Generation / CancelEpoch | `DONE` | `crates/nlos-types`；`ADR-0001` | public schema 与生成约束未冻结 |
| `B-RUNTIME` | RuntimeAdapter 与 Tokio 有界 Fiber runtime | `PARTIAL_PASS` | [ADR-0001](./adrs/0001-stage-b-core-language-and-runtime.md)、[PoC-0001](../evidence/stage-b/poc-0001-tokio-fiber-runtime.md)；提交 `a211088` | wake latency/fairness、structured join/detach、CPU 分维计量、Process crash、跨平台 |
| `B-OP-FENCE` | Operation 状态机、callback identity、cancel/generation fence | `PARTIAL_PASS` | [PoC-0002](../evidence/stage-b/poc-0002-operation-callback-fence.md)；提交 `8b9ffe1` | Driver authentication、EffectPermit、progress/stream callback；Tokio wake 集成已随 `B-OUTBOX`（PoC-0004）补齐 |
| `B-STORE` | SQLite WAL/FULL Operation authority、恢复、Outbox | `PARTIAL_PASS` | [ADR-0002](./adrs/0002-stage-b-sqlite-operation-authority.md)、[PoC-0003](../evidence/stage-b/poc-0003-sqlite-operation-authority.md)；F1–F7 已通过，包括三平台 CI | 100K 逐条生产写入、真实硬件掉电/更多文件系统仍超出当前证据 |
| `B-OUTBOX` | Durable Outbox → Tokio Fiber wake/reconcile consumer | `DONE` | [PoC-0004](../evidence/stage-b/poc-0004-outbox-wake-consumer.md)；本提交及评审后 remediation 提交（hash 见 git log 与 commit receipt） | durable wait registry/fiber rehydration 归 `B-PROCESS`/Slice K；此前移交 `B-STORE-FAULT` 的 F1–F7 已全部通过。2026-08-01 remediation：评审指出的 pump 错误路径可观测性（失败计数/根因/有上限退避/Faulted 终态）、drain panic 防护、shutdown 终态语义与 wake 重缓冲已补齐并各有测试。2026-08-01 复验残余（非阻塞，详见 PoC-0004 §8.4）：持久 apply 失败（`stopped_at` 路径）暂无 health 信号 → 后续 observability 项；`Faulted` 恢复依赖外部监督 → `B-PROCESS`；`PumpHealth.last_error` 跨 IPC 边界需脱敏 → `B-CONTROL`/`B-SCHEMA`；`Buffered` 驻留仅随 fiber 终态清理 → `B-PROCESS`/Slice K |
| `B-STORE-FAULT` | SQLite fault-injection：kill-9、torn-write、disk-full、checkpoint/backup、migration、长读事务、100K metadata、跨平台 | `DONE` | [PoC-0003 F1–F7 增量证据](../evidence/stage-b/poc-0003-sqlite-operation-authority.md)；[三平台 CI run 30714584445](https://github.com/cty12356541/llmos/actions/runs/30714584445) | 100K 逐条生产写入、真实硬件掉电/更多文件系统保留为扩展 Evidence，不阻塞本工作包 |
| `B-SCHEMA` | Protobuf/CBOR、golden vector、版本演进和本地 typed IPC | `IN_PROGRESS` | [ADR-0003](./adrs/0003-stage-b-idl-and-canonical-encoding.md)、[B-SCHEMA-001](../evidence/stage-b/b-schema-001-protobuf-envelope.md)、[B-SCHEMA-002](../evidence/stage-b/b-schema-002-cross-language-generation.md)、[B-SCHEMA-003](../evidence/stage-b/b-schema-003-deterministic-cbor.md)、[B-SCHEMA-004](../evidence/stage-b/b-schema-004-schema-fuzz-smoke.md)、[Linux fuzz run 30717749638](https://github.com/cty12356541/llmos/actions/runs/30717749638)、[三平台 CI run 30717749643](https://github.com/cty12356541/llmos/actions/runs/30717749643)；`schema/`、`gen/`、`crates/nlos-schema`、`crates/nlos-canonical`、`fuzz/` | RPC service/client、CBOR 跨语言、长期 fuzz、actual signing、transport adapter |
| `B-SANDBOX` | Wasmtime/WASI 与独立 host Process 隔离对比 | `READY` | [技术选型第 5 节](./stage-b-technology-selection.md) | capability import、fuel/epoch、memory、host crash、GuaranteeTier |
| `B-PROCESS` | native Process supervisor 与平台资源/生命周期 adapter | `READY` | [v0.5 Process 规范](../design/06-架构设计总纲-v0.5.md) | macOS/Windows/Linux suspend/kill、host incarnation、resource mapping |
| `B-TASK` | TaskPlan/TaskNode、lazy materialization、TaskSnapshot、双 Attempt 唯一提交 | `READY` | [v0.5 Task 规范](../design/06-架构设计总纲-v0.5.md) | TaskAuthority、CommitPermit、EffectPermit、snapshot drift、reconcile |
| `B-CONTROL` | CLI/API/NL/GUI 共用 ControlCommand 与 Receipt | `READY` | [v0.5 控制面规范](../design/06-架构设计总纲-v0.5.md) | SystemControl client、权限 UI、多层手动调度、等价路径证明 |
| `B-ARTIFACT` | 内容寻址 Artifact、metadata、reconcile、GC | `READY` | [技术选型第 7 节](./stage-b-technology-selection.md) | fsync/rename、blob/metadata 恢复、retention 和 GC |
| `B-SLICE-K` | Slice K：Package → Application → Task → Fiber → Operation → Receipt → 控制 | `NOT_STARTED` | [v0.5 Slice K](../design/06-架构设计总纲-v0.5.md) | 需要前述执行、持久化、Process、权限和控制能力贯通 |

## 4. 已验证的当前事实

### 4.1 Runtime

- 两个 Tokio worker 可承载并取消 100K 极简 waiting Fiber；当前证据最大 RSS 约 128.39 MiB。
- 10K 测试默认进入 workspace 测试；100K 作为显式规模探针运行。
- 这只证明当前 Apple Silicon/macOS workload 的局部 ScaleProfile，不证明所有 PC 的 PID 级 Agent 容量。

### 4.2 Operation fence

- `REGISTERED → DISPATCHED → terminal` 与 cancel 分支已实现。
- late callback 不得唤醒旧 Fiber，但会进入 reconciliation。
- duplicate callback 幂等；CallbackId substitution、stale Operation/Fiber generation 会被拒绝。
- cancel/completion 竞态只产生两个合法线性化结果。

### 4.3 Durable authority

- Operation state、Receipt identity 和 `WakeFiber | ReconcileEffect` Outbox 在同一个 SQLite `BEGIN IMMEDIATE` 事务中提交。
- 使用 WAL/FULL、单 writer admission、revision CAS、固定宽度 ID/epoch 编码和 schema version fail-closed。
- 数据库重开、Outbox ACK 重放、异常退出和 durable cancel/completion race 已有测试。
- Outbox 仍是 at-least-once：consumer 必须 generation-aware、幂等，不能把重复投递误判为新 effect。

### 4.4 Outbox → Tokio wake consumer

- pump 在专用 OS 线程驱动（blocking 不进 Tokio worker）；writer commit 后有界 hint + 兜底轮询，writer 路径不被 consumer 阻塞。
- commit 前无 wake；崩溃重放不丢失、不制造旧 generation wake；duplicate 不产生第二次逻辑唤醒/reconciliation。
- runtime 重启后 fiber record 不恢复，重投 wake 分类为 `FiberGone` 并 ACK；durable wait registry/fiber rehydration 属 `B-PROCESS`/Slice K。

### 4.5 Store fault-injection F1–F4

- kill-9 覆盖事务中断、commit 后崩溃和 consumer apply 后/ACK 前崩溃；已提交事务保留，未提交事务不产生 Receipt/Outbox，未 ACK 条目可重投。
- 测试专用 VFS 覆盖硬 I/O error、disk-full 与静默丢写；WAL 半帧截断/后续帧破坏保持合法前缀，未知或不完整尾部不冒充提交。
- 当前 macOS 上只读介质两种 WAL side-file 场景与真实 RAM volume ENOSPC 探针通过；错误向调用者显式传播，解除故障后 authority 可继续。
- checkpoint 模式、长读事务、online backup、WAL triplet 与仅复制主文件的正反恢复语义已有测试；这些仍是单机局部证据，不替代真实掉电和跨平台验证。

### 4.6 Store schema migration F5

- durable schema 已从 v1 演进到 v2，新增按 Operation/generation/sequence 的 Outbox 恢复索引，不改变公共 API。
- golden v1 中的 Operation、Callback fence、Receipt 与未 ACK Outbox 可无损迁移，升级后继续读写。
- 升级前在线备份保持 v1 rollback anchor，可独立恢复并迁移；逐写入点故障只留下完整 v1 或完整 v2。

### 4.7 Store 100K metadata F6

- 100K terminal Operation + 100K pending Outbox 的约 28.95 MB 数据库可快速重开、分页 pending、执行 durable ACK 并再次恢复。
- 当前 macOS dev profile：打开约 0.34 ms、pending 512 条约 0.56 ms、512 次 ACK 约 26.56 ms、再次打开约 0.30 ms。
- fixture 批量生成不计生产写入吞吐；该证据仅覆盖 100K 既有 metadata 下的恢复与队列热路径。

### 4.8 Store cross-platform F7

- Ubuntu、Windows、macOS GitHub Actions 均通过 workspace 测试与 Clippy；Linux 通过 rustfmt。
- Windows 强制终止、fault VFS、authority、Outbox 和 migration 路径已执行；Unix chmod 在 Linux/macOS 执行，真实 ENOSPC 探针仍为 macOS 专属。

### 4.9 Schema envelope 首切片

- `nlos.sabi.Envelope` v1 由 `.proto` 唯一源在构建期生成 Rust 类型，并登记 schema name、major/minor 和 critical extension support。
- unknown major/critical extension fail-closed；更高 minor/non-critical extension 可接受；frame、request ID、service/method 具有公共边界检查。
- forwarding API 保存原始 wire frame，避免 decode/re-encode 静默丢失当前生成器未知的 protobuf field。
- 首个 checked-in golden vector和 7 项 compatibility 测试通过；vendored `protoc` 与 Rust 生成链路经 Ubuntu/Windows/macOS CI 复验，不等于三语言、CBOR、fuzz 或 typed IPC 完成。

### 4.10 Schema 跨语言生成与 breaking gate

- Buf 以固定版本从同一 `.proto` 生成 TypeScript/Python type bindings；Rust 仍由 Cargo build 生成，三语言读取同一 golden vector。
- TypeScript/Python conformance 覆盖主次版本、critical/non-critical 扩展与 unknown protobuf field；生成物 checked in，并由 CI 重生成后检查 tracked/untracked drift。
- Buf STANDARD lint + FILE breaking policy 已接入；临时删除 `Envelope.method` field 4 的反例被明确拒绝。
- 当前 IDL 尚无 RPC service，因此不能声称已有三语言 service client；type generation/drift/conformance 已在 Ubuntu/Windows/macOS 通过。

### 4.11 Deterministic CBOR 与签名域

- `nlos-canonical` 实现 RFC 8949 core deterministic CBOR 严格子集；schema/major/minor、`sha-256`、object ID、payload digest 和 critical/noncritical opaque extensions 进入 canonical body。
- signing preimage 使用两个 `u32_be` 长度前缀绑定 ASCII domain 与 CBOR body；verifier 必须提供 expected domain，Protobuf bytes 不进入此路径。
- 禁止 duplicate/乱序 key、indefinite、非最短整数、tag、float/NaN、算法替换、额外嵌套与超界输入；decoder re-encode 后逐字节比较。
- CDDL、CBOR body golden、preimage golden 和 13 项正反测试经 Ubuntu/Windows/macOS 通过；实际 SHA-256/signature、完整业务 schema 和跨语言 CBOR 尚未实现。

### 4.12 Protobuf / CBOR sanitizer fuzz smoke

- 独立 `fuzz/` package 提供 Protobuf envelope、canonical body、signing preimage 三个有界 target；生产 workspace 不依赖 libFuzzer。
- checked-in corpus 覆盖 unknown Protobuf field，以及 duplicate/order/non-shortest/indefinite/tag/float/length/major/minor/extensions/domain 篡改。
- CBOR/preimage 任意成功解码都必须重新编码后逐字节等于输入；critical support set 与 expected domain 在 target 内显式校验。
- macOS arm64 AddressSanitizer 本地 33 秒共执行 15,499,860 次，无 crash/timeout/OOM/断言反例；这只是 smoke，不是长期 fuzz 或 production claim。

## 5. 当前下一验收门

`B-SCHEMA` 继续处于 `IN_PROGRESS`。B-SCHEMA-001/002 已完成 Protobuf envelope 与跨语言 compat gate，B-SCHEMA-003 已经三平台完成 deterministic CBOR/profile/golden，B-SCHEMA-004 已建立可重复 sanitizer fuzz smoke；当前验收门推进到本地 typed IPC adapter：

```text
Protobuf envelope + Rust generation + registry + first golden       DONE
  → TypeScript / Python generation + checked-in drift check         DONE
  → Buf lint / breaking + cross-language compatibility              DONE
  → deterministic CBOR profile + canonical golden                   DONE
  → protobuf / CBOR sanitizer fuzz smoke                            DONE
  → Unix socket / Windows named pipe typed IPC adapter              NEXT
```

下一切片验收条件：

1. 用同一 Protobuf schema 定义最小 request/response service，生成 Rust/TypeScript/Python client surface；
2. transport-neutral typed adapter 必须在 Unix domain socket 和 Windows named pipe 后端之间保持同一 schema/compat 行为；
3. frame length、connect/read/write timeout、peer identity/authorization hook 和 backpressure 必须显式有界；
4. unknown field forwarding、unknown major/critical extension、断连、半帧、超界与重连必须 fail-closed 或产生明确可恢复错误；
5. 不把 socket path、named-pipe name、OS credential 或 Protobuf bytes 泄漏进 `nlos-types`，并保留后续替换 transport 的边界。

`B-OUTBOX` 的已验收条件（供追溯）：commit 前无 wake；崩溃重放不丢失、不制造旧 generation wake；duplicate 无第二次逻辑唤醒/reconciliation；bounded queue 不阻塞 writer/cancel；测试覆盖 current/late/cancel-before-dispatch/crash-restart 场景；Evidence 已同步三 PoC 集成缺口并保持 `PARTIAL_PASS` 直到故障注入通过。

## 6. 阶段退出门映射

| Exit gate | 当前结论 |
|---|---|
| `ROAD-B-001` 第三方 Application 安装/更新/卸载 | 未开始 |
| `ROAD-B-002` Application 多 Process、后台 Task、UI Surface | 未开始 |
| `ROAD-B-003` 双 Attempt、cancel/commit、handle 泄漏、snapshot、provider cache、effect fence | 未完成；当前仅有 Operation 局部 fence |
| `ROAD-B-004` 10K/100K logical TaskNode、working-set、pressure/reclaim、rehydrate | 未开始；waiting Fiber 不能替代 TaskNode benchmark |
| `ROAD-B-005` Task Manager 多层手动控制与 NL/GUI/CLI 同路 | 未开始 |
| `ROAD-B-006` 100K dormant Fiber、阻塞隔离、crash propagation、Activation meter | 局部通过；仍为 `PARTIAL_PASS` |

阶段 B 当前总体状态：`IN_PROGRESS / NOT EXITED`。

## 7. 进度更新协议

以后每个实现或验证工作包完成时，必须在同一个 canonical commit 中同步：

1. 更新本表的状态、日期、commit、Evidence 和未决项；
2. 更新对应 ADR/PoC 或当前规范的实现缺口；
3. 更新“当前下一验收门”，确保只有一个主线 `IN_PROGRESS` 工作包；
4. 重新运行与风险相称的测试，并记录命令和结果；
5. 若状态从 `PARTIAL_PASS` 晋升为 `DONE`，必须补齐 Evidence 范围和退出条件，不能只改状态文字；
6. 若发现反例，降低状态或 Claim，保留反例 Evidence，不得删除旧事实。

本表的 `最后更新`、状态、commit 和 Evidence 是一个 CAS 检查点；提交前必须确认 HEAD 没有漂移。任何只更新聊天、不更新本表的进度不视为项目 canonical 进度。

## 8. 关联权威入口

- 当前规范：[架构设计总纲 v0.5](../design/06-架构设计总纲-v0.5.md)
- 阶段管理：[项目管理机制](./README.md)
- 技术选型：[阶段 B 技术选型](./stage-b-technology-selection.md)
- 规则：[项目知识渐进式披露与自动 CRUD](./project-knowledge-progressive-disclosure.md)
- 阶段证据：[stage-b evidence](../evidence/stage-b)
