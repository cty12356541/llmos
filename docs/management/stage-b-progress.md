# 阶段 B 权威进度单

> 状态：`ACTIVE / POC ACCEPTANCE PENDING`
>
> 最后更新：2026-08-01
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
| `B-OP-FENCE` | Operation 状态机、callback identity、cancel/generation fence | `PARTIAL_PASS` | [PoC-0002](../evidence/stage-b/poc-0002-operation-callback-fence.md)；提交 `8b9ffe1` | Driver authentication、EffectPermit、progress/stream callback、Tokio wake 集成 |
| `B-STORE` | SQLite WAL/FULL Operation authority、恢复、Outbox | `PARTIAL_PASS` | [ADR-0002](./adrs/0002-stage-b-sqlite-operation-authority.md)、[PoC-0003](../evidence/stage-b/poc-0003-sqlite-operation-authority.md)；提交 `bec4c42` | kill-9/torn-write/disk-full、checkpoint/backup、migration、100K metadata、跨平台 |
| `B-OUTBOX` | Durable Outbox → Tokio Fiber wake/reconcile consumer | `DONE` | [PoC-0004](../evidence/stage-b/poc-0004-outbox-wake-consumer.md)；本提交（hash 见 git log 与 commit receipt） | durable wait registry/fiber rehydration 归 `B-PROCESS`/Slice K；kill-9/torn-write/disk-full fault-injection 归 `B-STORE-FAULT` |
| `B-STORE-FAULT` | SQLite fault-injection：kill-9、torn-write、disk-full、checkpoint/backup、100K metadata | `READY` | 依赖 `B-OUTBOX`（ADR-0002 PoC 验收第 7 条） | fault-injection VFS、断电/写损坏语义、恢复演练、规模 metadata |
| `B-SCHEMA` | Protobuf/CBOR、golden vector、版本演进和本地 typed IPC | `READY` | [技术选型第 8 节](./stage-b-technology-selection.md) | 三语言生成、compat check、fuzz、transport adapter |
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

## 5. 当前下一验收门

`B-OUTBOX` 已 `DONE`（证据：[PoC-0004](../evidence/stage-b/poc-0004-outbox-wake-consumer.md)）。当前唯一主线工作包是 `B-STORE-FAULT`（SQLite fault-injection，与 ADR-0002 PoC 验收第 7 条对齐）：

```text
kill -9 / 断电
  → torn-write / commit 中断（fault-injection VFS）
  → disk-full / 只读文件系统 / I/O error fail-closed
  → WAL checkpoint / 备份恢复演练
  → 100K Operation metadata 规模行为
```

其验收条件：

1. 注入故障后已提交事务不丢失、未提交事务不冒充已提交；
2. 损坏或未知状态一律 fail-closed，不静默降级；
3. WAL、`-shm`、主数据库与 checkpoint 作为同一持久状态恢复；
4. 100K Operation metadata 下 pending/ACK/恢复路径不退化到不可用；
5. Evidence 更新 PoC-0003/PoC-0004 的 fault-injection 缺口；通过后相关 PoC 方可从 `PARTIAL_PASS` 晋升。

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
