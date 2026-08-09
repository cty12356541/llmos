# 阶段 B 权威进度单

> 状态：`ACTIVE / POC ACCEPTANCE PENDING`
>
> 最后更新：2026-08-09（已纳入 `B-TASK-006N` dual-authority VFS/process crash fault matrix、`B-TASK-006M` backend-neutral recovery metrics export、`B-TASK-006L` SystemControl handler、`B-SCHEMA-014` typed contract 与 `B-TASK-006K` durable alert acknowledgement；此前 `B-TASK-003/004/005/006A/006B/006C/006D/006E/006F/006G/006H/006I/006J`、`B-ARTIFACT-001/002` 及对应故障注入证据保持有效；2026-08-04 采纳[议题 31](../discussions/31-重复建设评估与继续投入边界.md)/[议题 32](../discussions/32-核心设计理念撞车风险评估.md) 顺序变更：主线由 `B-SCHEMA` 剩余横向门切换为 `B-TASK` 纵切面，Go/C# 探针后移）
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
| `B-STORE` | SQLite WAL/FULL Operation authority、恢复、Outbox、durable dedup/result | `PARTIAL_PASS` | [ADR-0002](./adrs/0002-stage-b-sqlite-operation-authority.md)、[PoC-0003](../evidence/stage-b/poc-0003-sqlite-operation-authority.md)、[B-SCHEMA-010](../evidence/stage-b/b-schema-010-durable-idempotency-result.md)、[B-SCHEMA-011](../evidence/stage-b/b-schema-011-durable-idempotency-ipc.md)、[B-SCHEMA-012](../evidence/stage-b/b-schema-012-deadline-cancel-state-machine.md)、[B-SCHEMA-013](../evidence/stage-b/b-schema-013-operation-control-timer-worker.md)；F1–F7、authority、真实重连、server restart、durable no-effect 与 cancel epoch CAS 已验证 | 100K 逐条生产写入、真实硬件掉电/更多文件系统仍超出当前证据 |
| `B-OUTBOX` | Durable Outbox → Tokio Fiber wake/reconcile consumer | `DONE` | [PoC-0004](../evidence/stage-b/poc-0004-outbox-wake-consumer.md)；本提交及评审后 remediation 提交（hash 见 git log 与 commit receipt） | durable wait registry/fiber rehydration 归 `B-PROCESS`/Slice K；此前移交 `B-STORE-FAULT` 的 F1–F7 已全部通过。2026-08-01 remediation：评审指出的 pump 错误路径可观测性（失败计数/根因/有上限退避/Faulted 终态）、drain panic 防护、shutdown 终态语义与 wake 重缓冲已补齐并各有测试。2026-08-01 复验残余（非阻塞，详见 PoC-0004 §8.4）：持久 apply 失败（`stopped_at` 路径）暂无 health 信号 → 后续 observability 项；`Faulted` 恢复依赖外部监督 → `B-PROCESS`；`PumpHealth.last_error` 跨 IPC 边界需脱敏 → `B-CONTROL`/`B-SCHEMA`；`Buffered` 驻留仅随 fiber 终态清理 → `B-PROCESS`/Slice K |
| `B-STORE-FAULT` | SQLite fault-injection：kill-9、torn-write、disk-full、checkpoint/backup、migration、长读事务、100K metadata、跨平台 | `DONE` | [PoC-0003 F1–F7 增量证据](../evidence/stage-b/poc-0003-sqlite-operation-authority.md)；[三平台 CI run 30714584445](https://github.com/cty12356541/llmos/actions/runs/30714584445) | 100K 逐条生产写入、真实硬件掉电/更多文件系统保留为扩展 Evidence，不阻塞本工作包 |
| `B-SCHEMA` | Protobuf/CBOR、golden vector、版本演进和本地 typed IPC | `IN_PROGRESS` | [ADR-0003](./adrs/0003-stage-b-idl-and-canonical-encoding.md)、[B-SCHEMA-001](../evidence/stage-b/b-schema-001-protobuf-envelope.md)、[B-SCHEMA-002](../evidence/stage-b/b-schema-002-cross-language-generation.md)、[B-SCHEMA-003](../evidence/stage-b/b-schema-003-deterministic-cbor.md)、[B-SCHEMA-004](../evidence/stage-b/b-schema-004-schema-fuzz-smoke.md)、[B-SCHEMA-005](../evidence/stage-b/b-schema-005-local-typed-ipc.md)、[B-SCHEMA-006](../evidence/stage-b/b-schema-006-typescript-python-ipc-clients.md)、[B-SCHEMA-007](../evidence/stage-b/b-schema-007-service-directory-negotiation.md)、[B-SCHEMA-008](../evidence/stage-b/b-schema-008-cross-language-directory-chain.md)、[B-SCHEMA-009](../evidence/stage-b/b-schema-009-common-sabi-semantics.md)、[B-SCHEMA-010](../evidence/stage-b/b-schema-010-durable-idempotency-result.md)、[B-SCHEMA-011](../evidence/stage-b/b-schema-011-durable-idempotency-ipc.md)、[B-SCHEMA-012](../evidence/stage-b/b-schema-012-deadline-cancel-state-machine.md)、[B-SCHEMA-013](../evidence/stage-b/b-schema-013-operation-control-timer-worker.md)、[三平台 reconnect run 30740180511](https://github.com/cty12356541/llmos/actions/runs/30740180511)、[三平台 restart run 30741046472](https://github.com/cty12356541/llmos/actions/runs/30741046472)、[三平台 deadline/cancel run 30741733804](https://github.com/cty12356541/llmos/actions/runs/30741733804)、[三平台 OperationControl run 30743421174](https://github.com/cty12356541/llmos/actions/runs/30743421174)、[fuzz run 30743421200](https://github.com/cty12356541/llmos/actions/runs/30743421200)、[B-SCHEMA-014](../evidence/stage-b/b-schema-014-system-control-recovery-contract.md)：typed/sanitized SystemControl recovery contract PARTIAL PASS；`schema/`、`gen/`、`sdk/`、`crates/nlos-schema`、`crates/nlos-service-directory`、`crates/nlos-canonical`、`crates/nlos-ipc`、`fuzz/` | Namespace bootstrap authority、生产目录 watch/lease/rebind、持久 deadline queue/restart recovery、Receipt authority、双向 peer auth、Python Proactor 稳定 profile、CBOR 跨语言、长期 fuzz、actual signing |
| `B-SDK-LANG-EVAL` | 官方 SDK 语言集合与 Go/C# 优先兼容评估 | `BLOCKED` | [多语言 SDK 支持评估计划](./language-sdk-support-plan.md)；OperationControl 前置切片见 [B-SCHEMA-013](../evidence/stage-b/b-schema-013-operation-control-timer-worker.md) | 2026-08-04 起 Go/C# generation/golden 探针与独立 IPC PoC 后移至 `B-TASK`/EffectPermit 纵切面之后（议题 31/32：第四种语言不能证明核心成立，且不应推动 SABI 在 Task/Effect 语义稳定前过早冻结）；Java/Kotlin、Swift、C/C++ 需求驱动复审 |
| `B-SANDBOX` | Wasmtime/WASI 与独立 host Process 隔离对比 | `READY` | [技术选型第 5 节](./stage-b-technology-selection.md) | capability import、fuel/epoch、memory、host crash、GuaranteeTier |
| `B-PROCESS` | native Process supervisor 与平台资源/生命周期 adapter | `READY` | [v0.5 Process 规范](../design/06-架构设计总纲-v0.5.md) | macOS/Windows/Linux suspend/kill、host incarnation、resource mapping |
| `B-TASK` | TaskPlan/TaskNode、lazy materialization、TaskSnapshot、双 Attempt 唯一提交 | `IN_PROGRESS` | [v0.5 Task 规范](../design/06-架构设计总纲-v0.5.md)；2026-08-04 起为唯一主线工作包（议题 31/32 顺序变更采纳）；[B-TASK-001](../evidence/stage-b/b-task-001-task-authority-commit-permit.md)：durable TaskAuthority + 双 Attempt 竞争 CommitPermit 六条门 PARTIAL PASS（`nlos-task`，14 测试，三平台 CI [run 30905979180](https://github.com/cty12356541/llmos/actions/runs/30905979180)）；[B-TASK-002](../evidence/stage-b/b-task-002-effect-permit-dispatch.md)：EffectPermit 签发 + 逐槽 EffectSlot 状态机（schema v2，13 测试）PARTIAL PASS 候选；[B-TASK-001 fault-injection](../evidence/stage-b/b-task-001-fault-injection.md)：F1–F4 对齐故障矩阵 6 行全 PASS（kill-9 中断/commit 后崩溃/硬 I/O 错误/ENOSPC/静默丢写+WAL 撕裂/故障解除恢复，7 测试）PARTIAL PASS；[B-TASK-003](../evidence/stage-b/b-task-003-reconcile-effect-history.md)：quarantine/reconcile + 跨 Attempt effect history + retry fence + required 成功语义（schema v3，21 测试）PARTIAL PASS；[B-TASK-003 crash windows](../evidence/stage-b/b-task-003-crash-windows.md)：三点崩溃窗口 + effect 表组故障矩阵（11 测试）PARTIAL PASS；[B-TASK-004](../evidence/stage-b/b-task-004-task-group.md)：TaskGroup membership generation/root CAS + Admission Receipt + 树状取消 + ALL/ANY 聚合 + quarantine 父组降级（schema v4，13 测试）PARTIAL PASS 候选；[B-TASK-003 fault](../evidence/stage-b/b-task-003-fault-injection.md)：v3 表组故障矩阵 7 行全 PASS（8 测试）PARTIAL PASS 候选；[B-TASK-005](../evidence/stage-b/b-task-005-commit-group-binding.md)：WriteSet/CommitPermit/TaskCommitReceipt 组绑定 + pre-dispatch/finalize 漂移围栏 + v4→v5 迁移（90 项 integration tests）PARTIAL PASS；[B-TASK-006A](../evidence/stage-b/b-task-006-artifact-commit-plan.md)：Artifact publication plan canonical root + CommitPermit artifact-only write-set 绑定 + v5→v6 迁移（96 项 integration tests）PARTIAL PASS；[B-TASK-006B](../evidence/stage-b/b-task-006b-artifact-publication-progress.md)：nested publication receipts 强绑定 + PUBLISHING/READY 重启状态 + v6→v7 迁移（100 项 integration tests）PARTIAL PASS；[B-TASK-006C](../evidence/stage-b/b-task-006c-artifact-publication-authorization.md)：发布前 Task/permit/head/group/effect 复验 + durable authorization + group membership freeze（103 项 integration tests）PARTIAL PASS；[B-TASK-006D](../evidence/stage-b/b-task-006d-artifact-prepared-finalize.md)：READY-only TaskAuthority terminal transaction + nested Task receipt + restart replay/rollback + membership unfreeze（105 项 integration tests）PARTIAL PASS；[B-TASK-006E](../evidence/stage-b/b-task-006e-cross-authority-coordinator.md)：独立薄 coordinator + bounded durable steps + pending scan + publish-before-record 重启收敛（真实双 authority integration）PARTIAL PASS；[B-TASK-006F](../evidence/stage-b/b-task-006f-coordinator-fault-matrix.md)：Artifact publish / Task nested receipt / Task finalize 三点 transaction-abort 故障隔离与修复后收敛（coordinator integration 2 项）PARTIAL PASS；[B-TASK-006G](../evidence/stage-b/b-task-006g-pending-scan-isolation.md)：best-effort pending scan + 逐 plan typed failure report + 单坏 plan 不阻塞健康 plan（coordinator integration 3 项）PARTIAL PASS；[ADR-0004](./adrs/0004-task-authority-commit-recovery-owner.md)：TaskAuthority-owned recovery worker `ACCEPTED`；[B-TASK-006H](../evidence/stage-b/b-task-006h-task-authority-recovery-worker.md)：启动扫描 + 周期/有界指数退避 + typed health + faulted/stopped 生命周期（coordinator integration 6 项）PARTIAL PASS；[B-TASK-006I](../evidence/stage-b/b-task-006i-durable-recovery-ledger.md)：schema v8 failure CAS + durable due/escalation/resolution + deterministic jitter（`nlos-task` 107 项 integration tests）PARTIAL PASS；[B-TASK-006J](../evidence/stage-b/b-task-006j-worker-durable-scheduling-health.md)：worker durable scheduling + local operations health（coordinator integration 6 项）PARTIAL PASS；[B-TASK-006K](../evidence/stage-b/b-task-006k-durable-recovery-alert-acknowledgement.md)：schema v9 immutable alert acknowledgement Receipt + failure CAS + unacknowledged gauge PARTIAL PASS；[B-TASK-006L](../evidence/stage-b/b-task-006l-system-control-recovery-handler.md)：typed handler + ServiceDirectory + Unix IPC PARTIAL PASS；[B-TASK-006M](../evidence/stage-b/b-task-006m-recovery-metrics-export.md)：backend-neutral recovery metrics catalog + live TaskAuthority gauge PARTIAL PASS；[B-TASK-006N](../evidence/stage-b/b-task-006n-dual-authority-vfs-process-fault-matrix.md)：双 authority 真实 VFS + process crash recovery PARTIAL PASS | QUORUM/REDUCE 执行语义、AGENT_INSTANCE 成员、DETACH 执行、LOST/quiescence、完整 TaskWriteSet/TaskSnapshotReceipt、旧 membership result 的 aggregate 过滤、TaskPlan/TaskNode 惰性物化、Process/Operation 绑定、跨 authority term adoption、真实 gateway/driver 集成、compensation 执行；真实 Capability/peer authority、bounded rejection Receipt 与 Windows handler IPC 延后至 B-CONTROL/Receipt authority |
| `B-CONTROL` | CLI/API/NL/GUI 共用 ControlCommand 与 Receipt | `READY` | [v0.5 控制面规范](../design/06-架构设计总纲-v0.5.md) | SystemControl client、权限 UI、多层手动调度、等价路径证明 |
| `B-ARTIFACT` | 内容寻址 Artifact、metadata、reconcile、GC | `IN_PROGRESS` | [B-ARTIFACT-001](../evidence/stage-b/b-artifact-001-content-addressed-store.md)：内容寻址 blob 五步写入协议 + SQLite metadata + 崩溃窗口/reconcile + cache 分域（26 测试含 VFS 故障注入）PARTIAL PASS；[B-ARTIFACT-002](../evidence/stage-b/b-artifact-002-staged-publication.md)：staged revision + Artifact 域内原子 publish + immutable publication receipt + v1→v2 迁移（33 测试）PARTIAL PASS | TaskAuthority prepare/finalize 与 nested Receipt、GC 执行、retention policy、加密/provenance/legal hold、Package 签名验证、sync/对象存储后端、Windows 目录 fsync 等价物、真实 ENOSPC 探针 |
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

### 4.6 Store schema migration F5 与 durable dedup/result

- durable schema 已从 v1 演进到 v3：v2 增加按 Operation/generation/sequence 的 Outbox 恢复索引；v3 增加按 Application/service/method/key 隔离的 dedup/result authority。
- 首次 key claim 与 Operation 注册同事务；terminal Operation、Receipt、稳定 service result 和 Outbox 同事务。相同 key/digest 只返回原 Operation 或原结果，不重新授予 dispatch；不同 digest fail-closed。
- golden v1 中的 Operation、Callback fence、Receipt 与未 ACK Outbox 可无损迁移，升级后继续读写；逐写入点故障只留下完整 v1、v2 或 v3。

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

### 4.13 本地 typed IPC 初始切片

- `LocalRpcService.Exchange` 以独立 request/response wrapper 进入同一 Protobuf IDL；Rust 生成 transport-neutral client trait，TypeScript/Python 生成 service descriptor。
- `nlos-ipc` 实现统一 4-byte length framing、1 MiB 硬上限、connect/read/write timeout、authorization-before-read、单 in-flight backpressure、request ID correlation、原始 response forwarding，以及不确定 exchange 后连接 fail-closed。
- macOS 真实 Unix socket 往返、owner-only `0600`、peer credential hook，以及超界、半帧、断连、未授权、串线、并发积压等 8 项 IPC 测试通过。
- Windows named pipe adapter 已实现 local-only/first-instance/有界实例和 buffer、identification QoS、有界 busy retry；Windows-only 往返/timeout 测试和整仓 Clippy 已由 [三平台 run 30730221706](https://github.com/cty12356541/llmos/actions/runs/30730221706) 通过。
- B-SCHEMA-005 当时尚无 TypeScript/Python transport runtime client；该缺口已由 4.14/B-SCHEMA-006 补齐。ServiceDirectory runtime、Capability、deadline/cancel、Operation/Receipt、自动重连和 Windows token/ACL 在该切片中仍未实现。

### 4.14 TypeScript/Python IPC client 初始切片

- Node `net.Socket` 和 Python asyncio client 已实现与 Rust 相同的 4-byte framing、1 MiB bound、connect/read/write timeout、单 in-flight backpressure、compatibility gate、request ID correlation 和失败后 connection poison。
- 两种 client 均通过真实 macOS Unix socket 调用 feature-gated Rust conformance server；测试同时覆盖 unknown major preflight、并发 backpressure 和 unavailable endpoint。
- [三平台 run 30734744799](https://github.com/cty12356541/llmos/actions/runs/30734744799) 已通过 Rust server ↔ TypeScript/Python client 真实组合；Windows Node named pipe 与 Python Proactor 路径均成功。
- ServiceDirectory/common SABI、双向 peer auth、自动重连与 SDK 发布尚未完成，因此当前只记 `SDK-2 CANDIDATE / PARTIAL`。

### 4.15 ServiceDirectory schema 与协商内核初始切片

- `nlos.sabi.ServiceDirectory` v1.0 已定义 resolve/negotiate、candidate/binding、local transport 和 typed compatibility error，并进入 schema registry；payload 独立限制为 64 KiB。
- resolve candidate 不携带 endpoint address；只有 negotiate 满足 schema major/minor、required feature 和 supported transport 后才返回 binding。
- Rust `SnapshotDirectory` 在注册时拒绝 malformed/duplicate binding，并按更高 minor、更高 generation、更小 binding ID 确定性选择；Rust/TS/Python 已通过同一 resolve request golden。
- [三平台 run 30735589673](https://github.com/cty12356541/llmos/actions/runs/30735589673) 与 [fuzz regression 30735589675](https://github.com/cty12356541/llmos/actions/runs/30735589675) 已成功；当前仍只记协议与 Rust core `PARTIAL PASS`。真实目录 IPC server、TS/Python resolver、watch/describe_error、lease/撤销和 common SABI 仍未完成。

### 4.16 TypeScript/Python 目录两跳链路

- feature-gated Rust fixture 同时提供 directory 与 business endpoint；directory payload 通过现有 bounded IPC 承载，协商结果来自 `SnapshotDirectory`。
- TypeScript/Python SDK 只接收 trusted bootstrap endpoint，校验 directory response/binding 后关闭目录连接，再自动连接返回的 business endpoint。
- [三平台 run 30736741324](https://github.com/cty12356541/llmos/actions/runs/30736741324) 已通过两种语言的 `bootstrap → negotiate → business exchange` 组合，覆盖 Linux/macOS Unix socket 与 Windows named pipe。
- bootstrap 仍是 raw endpoint，业务 schema 暂复用 Envelope；Namespace handle、生产目录、peer auth 和 common SABI 未完成，因此状态保持 `SDK-2 CANDIDATE / PARTIAL`。

### 4.17 common SABI 元数据与安全重试

- Envelope additive minor=1 candidate 已区分 exchange request ID、correlation ID 与 IdempotencyKey，并承载 caller/task fence、deadline、Capability/Reservation、proposal digest、Operation/Receipt reference 和 typed common failure。
- Rust/TypeScript/Python validators 按 method semantics 要求 mutation idempotency 与 long-running deadline；`E_UNCERTAIN`/`E_EFFECT_UNKNOWN` 只能指示查询 Operation 或使用原 key 重试，`E_PARTIAL` 必须关联 Receipt。
- 两个跨语言 golden 和 fail-closed 反例已在本地通过；目录两跳 fixture 的业务请求也真实携带 common metadata，由 Rust 服务入口校验后返回 Operation/Receipt，再由 TS/Python 校验。
- [三平台 run 30737782776](https://github.com/cty12356541/llmos/actions/runs/30737782776) 与 [fuzz run 30737782772](https://github.com/cty12356541/llmos/actions/runs/30737782772) 已成功，覆盖 Linux/macOS Unix socket 与 Windows named pipe。
- 本切片当时尚无 durable dedup/result；该缺口的三平台 authority 已由 4.18/B-SCHEMA-010 补齐，真实 IPC 与 server restart 三平台组合由 4.19/B-SCHEMA-011 补齐，初始 deadline/cancel durable 路由由 4.20/B-SCHEMA-012 推进，OperationControl/timer 初始切片由 4.21/B-SCHEMA-013 补齐。正式 SDK facade、Receipt authority、Capability authorization 和完整 server fault matrix 仍未完成，因此仍是 `SDK-2 CANDIDATE / PARTIAL`。

### 4.18 durable same-key dedup/result authority

- SQLite v3 已持久绑定 `(ApplicationId, service, method, IdempotencyKey)`、canonical request digest、Operation、Receipt 与有界稳定 service result。
- 首次 claim 才返回 `Created`；重连/重开后的相同 key/digest 返回 `PendingOrUncertain` 或逐字节原结果；不同 digest 和结果篡改被拒绝。
- crash-window、exact replay、冲突、immutable result、migration 和 online backup 已由本地与[三平台 run 30738888761](https://github.com/cty12356541/llmos/actions/runs/30738888761) 验证；详见 [B-SCHEMA-010](../evidence/stage-b/b-schema-010-durable-idempotency-result.md)。
- 真实 SABI server 接线与 server restart 组合已由 4.19/B-SCHEMA-011 补齐并通过三平台，deadline/cancel/uncertain 初始状态机由 4.20/B-SCHEMA-012 推进并通过三平台，独立 OperationControl 与 async timer worker 由 4.21/B-SCHEMA-013 推进。正式 SDK facade、完整 fault matrix、Receipt body 和 retention/GC 仍未完成，因此不升级 SDK 等级。

### 4.19 durable idempotency 真实 SABI 重连

- Rust directory-chain business handler 已接入 SQLite authority，并由可信 adapter 实际计算 payload SHA-256。
- fixture 在 durable commit 后、response write 前主动断线；TS/Python poison 原连接，以原 key 和新 exchange ID/correlation 重连，回放原 Operation/Receipt/result，server 断言 mutation dispatch 精确一次。
- 同 key/不同 payload 返回 `E_CONFLICT + DO_NOT_RETRY`；保持 `DISPATCHED` 的新 Operation 返回 `E_UNCERTAIN + QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY`。
- 接线明确了 durable `result_wire` 与每次重建的 transport envelope 必须分层；同进程重连已由本地与[三平台 run 30740180511](https://github.com/cty12356541/llmos/actions/runs/30740180511) 验证。新增组合把 commit/recovery 拆为两个独立 Rust 进程，恢复进程重绑目录和业务 endpoint、重开同一 SQLite authority，TS/Python 重新协商后回放原结果且恢复进程零次新 dispatch；[三平台 run 30741046472](https://github.com/cty12356541/llmos/actions/runs/30741046472) 已成功。详见 [B-SCHEMA-011](../evidence/stage-b/b-schema-011-durable-idempotency-ipc.md)。

### 4.20 deadline/cancel durable 服务端状态机

- `cancel_idempotent_before_dispatch` 原子提交 `CancelledBeforeEffect + Receipt + stable result + WakeFiber`，已 dispatch 的 Operation 无法被该路径改写为 no-effect；重开、精确回放和冲突已有测试。
- Rust SABI handler 用同宿主确定性 monotonic 检查点覆盖 dispatch 前 deadline/cancel，以及 dispatch 后 cancel/deadline；后两者先推进 cancel epoch，再把迟到 callback durable 分类为 `PartialEffect`/`EffectUnknown` 和 `ReconcileEffect`。
- TS/Python 真实 IPC 校验 `E_DEADLINE`、`E_CANCELLED`、`E_PARTIAL`、`E_EFFECT_UNKNOWN` 及其 Operation/Receipt/retry 组合；[三平台 run 30741733804](https://github.com/cty12356541/llmos/actions/runs/30741733804) 已成功。详见 [B-SCHEMA-012](../evidence/stage-b/b-schema-012-deadline-cancel-state-machine.md)。

### 4.21 OperationControl 与异步 deadline worker

- 新增 `nlos.sabi.OperationControl` v1.0 的 Query/Cancel/Status payload、Rust/TypeScript/Python 生成物、registry 与独立 64 KiB bound；八种 durable lifecycle 状态保持可区分。
- `request_cancel_idempotent` 以 Operation generation 和 `expected_cancel_epoch` 做 SQLite 事务 CAS：首次取消只推进一次，精确重试不重复推进/发 Outbox，completion 先提交时只返回既有终态。
- TS/Python 真实 IPC 已验证 `DISPATCHED(0) → CANCEL_REQUESTED(1)`、cancel replay 与 query；独立 Tokio timer task 又验证 `REGISTERED(0) → CANCELLED_BEFORE_EFFECT(1)`，worker 精确一次成功、零失败。[三平台 run 30743421174](https://github.com/cty12356541/llmos/actions/runs/30743421174) 与 [fuzz run 30743421200](https://github.com/cty12356541/llmos/actions/runs/30743421200) 已成功，保持 `PARTIAL PASS`。详见 [B-SCHEMA-013](../evidence/stage-b/b-schema-013-operation-control-timer-worker.md)。

### 4.22 durable TaskAuthority 与双 Attempt 唯一 CommitPermit（B-TASK-001）

- `nlos-task` crate（schema v1，WAL/FULL 回读 fail-closed、单写者 `BEGIN IMMEDIATE`、未知 `user_version` 拒绝）实现 Task 注册、冻结输入 digest 包快照绑定、TaskHead revision CAS；初始 head 为 `commit_seq=0` + domain-separated 空 effect-history root。
- 双 TaskAttempt 独立 generation/取消域绑定同一 snapshot；snapshot 行 immutable trigger，同 ID 异 bytes fail-closed `SnapshotConflict`。
- CommitPermit 线性化 CAS：无 outstanding permit 才签发；snapshot 绑定与当前 head 逐位不等 → `CONFLICTED`；他人持有 → `SUPERSEDED`（带 winner 身份）；同 key 同 bytes 重放原 permit，异 bytes fail-closed；磁盘部分唯一索引使第二个 outstanding permit 不可表示；CLOSED 后可再竞争。
- cancel-first 线性化：cancel_epoch 恰好递增一次后新 permit 拒发并写 closure receipt（head 不变）；permit-first：permit 不被 cancel 清除，holder 可 finalize 推进 head；effect 级 fencing 推迟到 EffectPermit 切片。
- 重启恢复完整、重放一致、幽灵 permit 不可表示（permit/receipt ID 确定性派生）；14 项测试与 workspace clippy/fmt 本地通过，三平台 CI 已通过（[run 30905979180](https://github.com/cty12356541/llmos/actions/runs/30905979180)），详见 [B-TASK-001](../evidence/stage-b/b-task-001-task-authority-commit-permit.md)。
- 证据等级为单节点局部 H3 + 三平台复验、PARTIAL PASS：EffectPermit/effect history/PermitAdoption/惰性物化/Process 绑定未实现，digest 为占位公式，未接 fault-injection。

### 4.23 EffectPermit 签发与逐槽 EffectSlot 状态机（B-TASK-002）

- `nlos-task` schema v1→v2 纯增量迁移（`effect_slots`/`effect_permits`/`effect_receipts`/`permit_effect_sets` 四表），golden-v1 无损迁移与失败回滚测试通过。
- `LogicalEffectId`/`idempotency_identity_digest` 按固定 domain-separated 公式派生；descriptor 按构造排除 AttemptId/ActionId/OperationId/incarnation/nonce（TASK-EFFECT-ID-001 禁止项不可表示）；跨 attempt 同一 descriptor 同一身份。
- 只有 CommitPermit 持有者可签发 EffectPermit（TASK-RACE-001）：loser/陈旧 generation/错误 epoch/未声明 slot 全类型化拒绝；签发 CAS PLANNED→PERMITTED + 一次性 dispatch token；双线程竞态恰好一胜一拒（DispatchTokenConsumed）。
- cancel 线性化：cancel 后迟到 dispatch 类型化 `CancellationCommitted` 且 slot 保持 PERMITTED；未消费 token 收口 NO_EFFECT；已消费 token 不得伪装未执行、cancel 后仍可登记真实结局。
- finalize 收紧：任何 PLANNED/PERMITTED/DISPATCHED/EFFECT_UNKNOWN slot 禁止关闭 permit；EFFECT_UNKNOWN 跨重启持久阻塞；无声明 effect 的旧流程行为不变（14 项旧测试回归）。
- 13 项新测试 + workspace clippy/fmt 通过，详见 [B-TASK-002](../evidence/stage-b/b-task-002-effect-permit-dispatch.md)；证据等级单节点 H3 候选：TASK-EFFECT-003 quarantine/reconcile、跨 Attempt effect history、required 成功语义、三点崩溃注入均未实现。

### 4.24 TaskAuthority fault-injection（B-TASK-001 增量）

- `nlos-store-fault` VFS 对齐 PoC-0003 F1–F4 矩阵移植至 B-TASK-001 表组，6 行全 PASS：kill-9 中断事务完全回滚无痕迹；commit 后崩溃全部已提交事实（head/receipt/closure/cancel_epoch）逐位保留且重放一致；硬 I/O 错误与 ENOSPC 显式 typed 传播无半截状态；静默丢写幻影 permit 重开不可见、WAL 撕裂尾部整体隐藏且合法前缀保留、重做确定派生同 ID 真实持久；故障解除后从已提交前缀完整继续（head=2 双 attempt 收口）。
- `PRAGMA integrity_check` 独立复核每行通过；`nlos-task` 零 `src/` 改动（`open_with_vfs` 与 `nlos-store` 同构）。
- 详见 [B-TASK-001 fault-injection](../evidence/stage-b/b-task-001-fault-injection.md)；限制：macOS VFS 模拟 ≠ 真实断电、三平台 CI 待运行本文件、effect 表组（B-TASK-002 新增）与 F4/F5（checkpoint/backup/migration）矩阵未覆盖。

### 4.25 EFFECT_UNKNOWN quarantine/reconcile、跨 Attempt history 与 required 语义（B-TASK-003）

- schema v2→v3 纯增量迁移（`effect_history`/`task_quarantine_receipts`/`task_adoption_receipts`/`task_reconcile_receipts`/`task_effect_sequences`/`task_finalize_proofs` 六表），golden-v2 无损迁移与失败回滚测试通过。
- EFFECT_UNKNOWN 全生命周期：finalize/close 遇 unknown → permit 不可复用 `QUARANTINED` tombstone（TaskHead 冻结、禁发新 winner、attempt SUPERSEDED）；`adopt_permit` 限权 `RECONCILE_CLOSE_OR_QUARANTINE_ONLY`（禁新 EffectPermit/dispatch，`AdoptionScopeViolation`）；`reconcile_effect` 单事务 `UNKNOWN → RECONCILING → EFFECT_CLOSED | CONFIRMED_NO_EFFECT | 回 QUARANTINED`，重放逐字节一致、异 proof `HistoryConflict` 无双重 reconcile。
- 跨 Attempt effect history：`EFFECT_CLOSED`/`CONFIRMED_NO_EFFECT` 与 slot 闭合同事务追加（seq 无洞、root 重算、空 root 与 B-TASK-001 初始 head 逐位兼容）；required 未满足且已有 effect → `PARTIAL_EFFECT`（有 required 满足）/`FAILED_AFTER_EFFECT`（零满足），fence 严格 +1、head/root/fence 同 CAS，stale-fence snapshot `CONFLICTED`；`lookup_effect_history` 跨 attempt 回读；已闭合 LogicalEffectId 再 dispatch 被拒 `EffectAlreadyClosed`。
- required 成功语义完整化：`COMMITTED` 要求 required 槽 `EFFECT_CLOSED`+断言 或 `NO_EFFECT`+CNA+snapshot 绑定证明；普通 NO_EFFECT 与 `CONFIRMED_NO_EFFECT` 永不满足 required；skip 绝不写成 COMMITTED；legacy `finalize_commit` 通道冻结 B-TASK-002 语义（兼容层，见 B-TASK-003 §3.5）。
- 21 项新测试 + 仅 1 处旧测试适配（golden v1 版本戳 2→3），详见 [B-TASK-003](../evidence/stage-b/b-task-003-reconcile-effect-history.md)；证据等级单节点 H3：跨 term adoption、真实 gateway proof、compensation 执行、TaskGroup 均未实现。

### 4.26 三点崩溃窗口与 effect 表组故障矩阵（B-TASK-003 增量）

- 三窗口 kill-9 注入全 PASS：窗口1（token 未消费）重启后 PERMITTED 可证明、NO_EFFECT 收口合法、登记 outcome/伪造 token 类型化拒绝；窗口2（DISPATCHED 未闭合）不冒充成败、finalize 阻塞、拒绝 NO_EFFECT 改名、EFFECT_UNKNOWN 跨二次重开持久阻塞且 permit 保持 ISSUED 不冒充失败；窗口3（Receipt 前）登记 EFFECT_CLOSED 后 required 槽 COMMITTED。
- effect 表组 F1–F4 对齐矩阵 6 行全 PASS：kill-9 中断无半截 slot；commit 后崩溃 slot/permit/token/receipt/summary 逐位保留且重放一致（迟到登记 PermitNotIssued）；IoErr/ENOSPC typed 传播无假成功；静默丢写幻影 EffectPermit 不可见、WAL 撕裂尾部隐藏幻影 DISPATCHED；故障解除后从已提交前缀继续至 head=2。
- 四态区分断言：`blocks_finalization` 映射（DISPATCHED/EFFECT_UNKNOWN 阻塞、NO_EFFECT/EFFECT_CLOSED 放行）经公开 API 直接断言；PARTIAL 如实映射为 v2 观测形态（DISPATCHED 未闭合），提交语义归 B-TASK-003 主线。
- 详见 [B-TASK-003 crash windows](../evidence/stage-b/b-task-003-crash-windows.md)；限制：kill-9 ≠ 真实断电、macOS 本地、未对 v3 reconcile/history 表组注入、F4 全集未覆盖。

### 4.27 TaskGroup 组织层（B-TASK-004）

- schema v3→v4 纯增量五表迁移（golden-v3 无损 + 失败回滚测试）：`register_group` 无环父子树（父先存在使环构造上不可产生 + 祖先链防御检查）、每 task 单 root、max_depth 沿祖先链与 max_children fail-closed、QUORUM/REDUCE/BEST_EFFORT 保留拒绝。
- membership content-addressed root + 单调 generation CAS；Admission/Removal Receipt immutable、确定性派生 ID、重放安全；OPEN-only admission（SEALED 冻结）。
- `register_attempt_in_group` 纯增量 API：期望 membership generation/root/policy 逐位校验，漂移 fail-closed（`StaleMembershipGeneration`/`MembershipConflict`）；未绑组 attempt 行为逐位不变（v1–v3 API 零变化，旧测试仅 2 处版本戳适配）。
- 树状取消单事务结构化传播：parent cancel_epoch 恰递增一次 → 全部非终态后代（child group + pre-permit member attempt closure receipt、head 不变）；permit 持有 attempt 不动（permit-first 线性化）；终态/未绑组不动。
- 聚合状态为显式 refresh 派生视图（child 状态是真相权威）：ALL 全成功才 COMPLETED、ANY 任一成功；failure_mode 占位语义 FAIL_FAST（同事务传播取消剩余）/COLLECT_ALL/ISOLATE；quarantine 子树证据使父组降级 PARTIAL 不得 COMPLETED、携带者拒绝移除（`GroupQuarantinedChild`）。
- 详见 [B-TASK-004](../evidence/stage-b/b-task-004-task-group.md)；证据等级单节点 H3 候选：QUORUM/REDUCE、AGENT_INSTANCE、DETACH、LOST/quiescence、WriteSet/Permit/Receipt 组绑定均未实现。

### 4.28 内容寻址 Artifact 存储（B-ARTIFACT-001）

- 新 crate `nlos-artifact`：`ArtifactId + revision + ContentDigest → SQLite metadata`（WAL/FULL 回读 fail-closed、未知 user_version 拒绝），`ContentDigest → 本地内容寻址字节`；目录布局 artifacts/blobs/tmp 与 cache 分域。
- 安全关键写入协议：tmp → fsync → **重读按 SHA-256 校验** → atomic rename → 父目录 fsync → metadata 同事务提交（**blob 持久化永远先于引用事务**）；跨设备 rename typed fail-closed。
- immutable revision（DDL trigger 禁 UPDATE/DELETE）+ mutable head CAS（竞争恰好一胜、`HeadConflict` typed）；读后 digest 重验（撕裂 blob `DigestMismatch` 绝不静默返回错字节）。
- `recover()` reconcile：committed revision 缺 blob typed 列出、孤儿 tmp 清理、孤儿 blob 仅列出供 GC（本切片不删除）；cache 行 blob 缺失降级 miss 自愈；eviction 无任何触及 artifacts/ 的代码路径（同字节双域测试验证）。
- 崩溃窗口全覆盖：rename 前（tmp 孤儿清理）/ rename 后 commit 前（无幻影 revision、孤儿列出）/ commit 后（完全可用）；kill-9 中断完全回滚；ENOSPC/静默丢写 typed 无假成功。
- 26 测试全绿，详见 [B-ARTIFACT-001](../evidence/stage-b/b-artifact-001-content-addressed-store.md)；限制：仅 LOCAL_SINGLE_NODE、无 GC/retention 执行、无加密/Package 验证、recover 只核 presence、Windows 目录 fsync 无 std 等价物（NTFS 日志依赖）、真实 ENOSPC 未测。

### 4.29 v3 表组故障注入（B-TASK-003 增量）

- quarantine/adoption/reconcile/effect-history/finalize-proofs 六表的 F1–F4 对齐矩阵 7 行全 PASS：kill-9 中断 v3 事务完全回滚（幻影 tombstone/history/sequence 不可见，重做确定性派生 ID 一致）；commit 后崩溃 tombstone/adoption/reconcile/history/finalize-proof 逐位保留（异 proof HistoryConflict、重放不双重追加）；PARTIAL_EFFECT finalize 重放 fence 不再 +1、history seq 无洞不双增；IoErr/ENOSPC typed 无假成功；静默丢写幻影 reconcile/adoption 不可见、WAL 撕裂尾部隐藏合法前缀保留；故障解除后 reconcile 重试闭合 + 新竞争完整收口至 head=2。
- v3 语义断言在 v4（TaskGroup）落地后全部保持绿色，无 counter-evidence。
- 详见 [B-TASK-003 fault](../evidence/stage-b/b-task-003-fault-injection.md)；限制：kill-9 ≠ 真实断电、macOS VFS、F4 全集未覆盖、TaskGroup 表组未注入。

### 4.30 WriteSet / CommitPermit / TaskCommitReceipt 组绑定（B-TASK-005）

- `nlos-task` schema v4→v5 纯增量迁移：CommitPermit 与 task receipt 新增可空 `group_id + membership_generation + membership_root + group_policy_digest`；旧 v1–v4 行显式解释为 ungrouped，不推断 membership。
- grouped Attempt 竞争 permit 时从当前 TaskGroup 权威行捕获 binding，并与 `write_set_root` 同事务持久化；新 EffectPermit、dispatch 和 terminal receipt 前逐位复验，membership/policy 漂移 typed `MembershipConflict`，不消费 token、不推进 TaskHead、不关闭 permit。
- TaskCommitReceipt-shaped record 原样复制 permit binding，permit/receipt 跨重启回读一致；结构等价 v4 的旧 ungrouped permit 升级后仍可完成提交且 binding 保持 `None`。
- `nlos-task` 90 项 integration tests、workspace rustfmt 与 crate Clippy 通过，详见 [B-TASK-005](../evidence/stage-b/b-task-005-commit-group-binding.md)；限制：完整 TaskWriteSet/TaskSnapshotReceipt、sealed membership rebase、旧 root aggregate 过滤、Artifact/Semantic publication receipts、fault-injection 与三平台 CI 尚未完成。

### 4.31 Artifact staged publication（B-ARTIFACT-002）

- `nlos-artifact` schema v1→v2 纯增量迁移，新增 durable staged revision 与 immutable publication receipt；stage 先持久化 blob 但不插 revision、不推进 canonical head。
- staged record 绑定 task/permit/write-set root，完全重放返回原记录，key 重绑 fail-closed；publish 前逐位复验 binding、blob 与 expected head。
- publish 在 ArtifactAuthority 单个 `BEGIN IMMEDIATE` 内完成 immutable revision + head CAS + receipt + staged state transition；同 head 多候选恰好一胜，败者保持 staged；跨重启 publish/replay 保持同一 receipt。
- `recover()` 将 staged digest 视为权威引用，缺失 staged blob 进入独立报告并阻止发布；`nlos-artifact` 33 项测试通过，详见 [B-ARTIFACT-002](../evidence/stage-b/b-artifact-002-staged-publication.md)。限制：TaskAuthority 跨库 prepare/finalize、nested Receipt、跨 authority 崩溃收敛、v2 VFS 故障矩阵和三平台 CI 尚未完成。

### 4.32 Artifact publication plan（B-TASK-006A）

- `nlos-task` schema v5→v6 纯增量迁移；新增 immutable Artifact commit plan/expectation 表，旧 Task/Attempt/Permit/Receipt 逐位保留。
- canonical expectation set 以固定 domain + 稳定排序计算 root，拒绝空集合、revision=0、重复 staging identity 与重复 Artifact revision；root 必须逐位等于 issued permit 的 artifact-only `write_set_root`。
- planning 复验 holder/generation、TaskHead/history/fence 与 group binding，但只产生 `PLANNED`：不授权 Artifact publish、不关闭 permit、不推进 TaskHead；跨重启 exact replay/inspect 与 DDL 不可变已验证。
- `nlos-task` 96 项 integration tests 通过，详见 [B-TASK-006A](../evidence/stage-b/b-task-006-artifact-commit-plan.md)。限制：READY 晋级、publication receipt 消费、membership freeze、nested TaskCommitReceipt 与跨库收敛仍是当前门槛。

### 4.33 Artifact publication progress（B-TASK-006B）

- `nlos-task` schema v6→v7 新增 immutable nested Artifact publication receipt；task/permit/write-set/staging/artifact/revision/digest/size/head transition 与 plan 逐项强绑定。
- receipt batch 单事务消费：任一冲突全批回滚；1..N-1 项显式 `PUBLISHING`，N/N 项 `READY`，exact replay 不重写时间戳；重启后 partial 集可查询并继续补齐。
- `PUBLISHING/READY` 均保持 permit `ISSUED`、TaskHead 不变，不冒充 finalized；100 项 `nlos-task` integration tests 通过，详见 [B-TASK-006B](../evidence/stage-b/b-task-006b-artifact-publication-progress.md)。限制：publication authorization、prepared finalize、TaskCommitReceipt nested link、自动跨库收敛仍未完成。

### 4.34 Artifact publication authorization（B-TASK-006C）

- `authorize_artifact_publication` 在发布前单事务复验 holder/generation、permit state/write-set、TaskHead/history/fence、group binding，并只允许无 Effect slot 的 artifact-only permit；成功 CAS `PLANNED → PUBLISHING`，零 receipt 状态也可跨重启查询和精确重放。
- 未授权 plan 不再接受 nested receipt；授权后的 grouped plan 在 `PUBLISHING/READY` 期间冻结 Admission/Removal，避免 Artifact canonical publication 后因 membership 漂移阻断 Task finalize。
- permit 仍为 `ISSUED`、TaskHead 仍未推进；103 项 `nlos-task` integration tests 通过，详见 [B-TASK-006C](../evidence/stage-b/b-task-006c-artifact-publication-authorization.md)。限制：prepared finalize、TaskCommitReceipt nested link、ArtifactAuthority 在线验签与自动跨库收敛仍未完成。

### 4.35 Artifact prepared finalize（B-TASK-006D）

- 只有完整 `READY` plan 可进入 `finalize_artifact_commit`；Task receipt、permit closure、Attempt terminal state、TaskHead/control epoch、plan `FINALIZED + task_receipt_id` 在同一个 TaskAuthority transaction 提交。
- `ArtifactTaskCommitReceipt` 返回 Task receipt 与 canonical nested Artifact receipts；重启 exact replay 不重写时间戳、不重复推进 head。terminal CAS 注入失败已验证所有 terminal fact 一起回滚，解除故障后可重试收敛。
- finalize 后 grouped membership freeze 自动解除；105 项 `nlos-task` integration tests 通过，详见 [B-TASK-006D](../evidence/stage-b/b-task-006d-artifact-prepared-finalize.md)。限制：跨 authority coordinator/outbox、自动重启收敛、在线 authorization 验证与新路径完整 VFS/三平台矩阵仍未完成。

### 4.36 Cross-authority commit coordinator（B-TASK-006E）

- 新增无独立持久状态的 `nlos-commit-coordinator` 薄适配层，以一步一个 durable boundary 驱动 `PLANNED → PUBLISHING → READY → FINALIZED`；Task/Artifact crate 保持互不反向依赖。
- TaskAuthority 提供 bounded incomplete-plan scan；coordinator 可在启动后收敛 pending plan。真实双 authority 测试覆盖 authorize 后重启、Artifact publish 后/Task record 前重启、partial/ready 重启、finalize 与 finalized replay。
- `staging_id_for` 公开既有确定性公式，允许 permit/write-set 在 stage 前绑定未来 staging identity。详见 [B-TASK-006E](../evidence/stage-b/b-task-006e-cross-authority-coordinator.md)。限制：完整 VFS fault matrix、Process supervisor 托管、在线 authorization token/签名、冲突处置与混合 Effect write set 尚未完成。

### 4.37 Coordinator write fault matrix（B-TASK-006F）

- 精确注入 Artifact publication receipt、Task nested receipt、Task terminal CAS 三类 SQLite abort；分别验证 no-publication、published-but-unrecorded、READY-but-unfinalized 的真实 durable 状态，均未返回伪成功。
- 每类故障解除后都从原 durable prefix 收敛至同一完整 Task/Artifact receipt；typed `CoordinatorError::Artifact/Task` 保留错误来源。详见 [B-TASK-006F](../evidence/stage-b/b-task-006f-coordinator-fault-matrix.md)。
- 限制：真实 kill-9/ENOSPC/I/O/torn-write 组合矩阵、supervisor 托管、online authorization token/签名和 conflict/compensation 运维路径尚未完成。

### 4.38 Pending scan isolation（B-TASK-006G）

- 新增 `converge_pending_best_effort` 与 `PendingConvergenceReport`：scan 查询失败才整体失败；单 plan authority failure 绑定 plan ID 与 typed source 后继续处理同批其他 plan。
- 条件故障测试验证两个 pending plan 中一个停在 `READY` 时，另一个仍能 `FINALIZED`；故障修复后下一轮只扫描并完成剩余 plan。详见 [B-TASK-006G](../evidence/stage-b/b-task-006g-pending-scan-isolation.md)。
- [ADR-0004](./adrs/0004-task-authority-commit-recovery-owner.md) 已接受 TaskAuthority-owned worker；周期调度、退避和 lifecycle health 已由 B-TASK-006H 推进，jitter、持久升级策略、metrics/运维 API 仍待实现与验证。

### 4.39 TaskAuthority-owned recovery worker（B-TASK-006H）

- `TaskAuthorityCommitRecoveryWorker` 在专用线程启动即执行 bounded pending scan，成功后周期轮询；失败 cycle 使用封顶指数退避，单 plan typed failure 不抹掉同 cycle 已完成计划的统计。
- health 区分 `Starting / Running / BackingOff / Faulted / Stopped`，并报告 cycle、inspect/finalize、连续失败、retry delay 和 plan/authority failure；显式停止可中断长 poll 并 join。
- 三项新增生命周期测试覆盖立即启动恢复、故障退避后修复、阈值 faulted 且 durable plan 不丢失；coordinator integration tests 共 6 项，详见 [B-TASK-006H](../evidence/stage-b/b-task-006h-task-authority-recovery-worker.md)。持久 retry/escalation ledger 与 jitter 已由 B-TASK-006I 建立，worker 接线、运维 API、真实进程/VFS 故障与三平台 CI 尚未完成。

### 4.40 Durable recovery ledger（B-TASK-006I）

- TaskAuthority schema v8 新增 per-plan `RETRYING / ESCALATED / RESOLVED` ledger；failure total CAS、连续/总失败、authority source、next due 与升级/解决时间跨重启保持。
- due scan 排除未到期和 escalated plan；指数 delay 加 plan/ordinal 确定性 ±20% jitter。显式 resume 以 total failures CAS，terminal Task transaction 同时把 ledger 置 `RESOLVED`。
- v5/v6/v7→v8 migration 与 recovery 生命周期测试通过；`nlos-task` 107 项 integration tests 全通过，详见 [B-TASK-006I](../evidence/stage-b/b-task-006i-durable-recovery-ledger.md)。worker 接线与本地 operations health 已由 B-TASK-006J 完成；统一外部接口与真实故障矩阵仍待完成。

### 4.41 Worker durable scheduling/health（B-TASK-006J）

- worker 改用 TaskAuthority due scan，并把 plan failure 以 total-failure CAS 写入 schema v8；等待采用 durable deterministic-jitter due。
- 单 plan 达阈值后持久 `ESCALATED` 并退出自动 scan，worker 继续服务健康 plan；只有控制环基础设施连续失败才 fault worker。
- health 汇总 durable retrying/escalated/resolved；6 项 coordinator integration tests 通过，详见 [B-TASK-006J](../evidence/stage-b/b-task-006j-worker-durable-scheduling-health.md)。限制：统一 IPC/metrics/告警面、错误脱敏与真实故障矩阵尚未完成。

### 4.42 Durable recovery alert acknowledgement（B-TASK-006K）

- TaskAuthority schema v9 新增 immutable acknowledgement Receipt，绑定 exact plan/failure escalation、Principal、IdempotencyKey 和确认时间；failure-count CAS 阻止 stale UI 确认新告警。
- acknowledgement 不隐式 resume/retry/finalize；exact retry 跨重启返回原 Receipt，冲突 key fail-closed。bounded alert list 与 `unacknowledged_escalated` gauge 不暴露本地错误字符串。
- migration、重启 replay、CAS/idempotency、DDL immutability 和不触发 resume 均有 integration test，详见 [B-TASK-006K](../evidence/stage-b/b-task-006k-durable-recovery-alert-acknowledgement.md)。限制：SystemControl protobuf/ServiceDirectory/真实 IPC、Capability enforcement、metrics exporter 与真实故障矩阵尚未完成。

### 4.43 SystemControl recovery contract（B-SCHEMA-014）

- 新增 registry-backed `nlos.sabi.SystemControl` v1；`get` 返回有界 metrics/typed failure/alert snapshot，`submit` 接收统一 ControlCommand acknowledgement，并以 Receipt-shaped result 返回。
- schema 不包含 worker 本地 diagnostic string；Rust validator 对 identity、枚举、CAS、数量、时间、reason 和 Receipt fail-closed，同源 TypeScript/Python bindings 已生成。
- schema tests、Buf lint/format 与 TypeScript typecheck 通过，详见 [B-SCHEMA-014](../evidence/stage-b/b-schema-014-system-control-recovery-contract.md)。限制：TaskAuthority handler、common context/Capability enforcement、ServiceDirectory binding 和真实 local IPC 尚未完成。

### 4.44 SystemControl recovery handler（B-TASK-006L）

- 新增 transport-neutral handler：`get` 合并 worker lifecycle 与即时 TaskAuthority durable gauge/alerts；raw diagnostic string 不跨边界。
- `submit` 强制 caller=issuer、ControlCommandId=IdempotencyKey，并在 pluggable Capability policy 放行后调用 TaskAuthority acknowledgement CAS；payload/common context 返回同一 Receipt，确认不触发 resume。
- 5 项 integration tests 覆盖拒绝路径、framed mutation/replay、ServiceDirectory negotiate 和真实 Unix endpoint typed get，详见 [B-TASK-006L](../evidence/stage-b/b-task-006l-system-control-recovery-handler.md)。限制：真实 Capability/peer authority、bounded SABI failure 映射、Windows handler round-trip、外部 metrics exporter、多入口 parity 与三平台 CI 尚未完成。

### 4.45 Recovery metrics export（B-TASK-006M）

- 新增 backend-neutral typed metrics sink 与稳定 catalog；counter/gauge/worker state 不借助 diagnostic string 或 per-plan identity 表达。
- export 每次以 live TaskAuthority summary 覆盖 worker cache 的 durable gauge；故意注入 stale cache 的测试验证 escalated/unacknowledged 均返回权威值。
- `nlos-system-control` 共 6 项 integration tests 通过，详见 [B-TASK-006M](../evidence/stage-b/b-task-006m-recovery-metrics-export.md)。限制：具体 OpenMetrics/ETW/signpost adapter、scrape auth/retention/alert rule 和三平台验证尚未完成。

### 4.46 Dual-authority VFS / process crash fault matrix（B-TASK-006N）

- TaskAuthority 与 ArtifactAuthority 可分别接入真实 SQLite fault VFS；Artifact `IOERR`、Task `SQLITE_FULL` 均返回对应 typed authority failure，且只保留已提交前缀。
- Task 侧 `PowerLossAfter` 的表面成功在连接死亡/重开后不会制造 durable nested Receipt；Artifact 已发布前缀可被重放并 finalize。
- 独立子进程在 Artifact 已发布、Task 尚未记账时被强制终止；重开后的 TaskAuthority-owned worker startup scan 收敛到唯一 terminal receipt。
- `restart_convergence` 共 11 项 integration tests 通过，详见 [B-TASK-006N](../evidence/stage-b/b-task-006n-dual-authority-vfs-process-fault-matrix.md)。限制：不是实际硬件掉电/跨机器原子性/三平台证据；coordinator 仍依赖各 authority 的 durability contract。

## 5. 当前下一验收门

`B-TASK` 自 2026-08-04 起为唯一主线工作包（采纳议题 31/32 顺序变更）。`B-SCHEMA` 保持 `IN_PROGRESS` 完成态收尾但不再持有主线；其剩余横向项（Go/C# 探针、Namespace bootstrap authority、生产目录 watch/lease/rebind、持久 deadline queue/restart recovery、Receipt authority、双向 peer auth、Python Proactor 稳定 profile、CBOR 跨语言、长期 fuzz、actual signing）在 `B-TASK` 纵切面成立前不推动 SABI 冻结。

已完成的 B-SCHEMA 验收链（保持有效）：

```text
Protobuf envelope + Rust generation + registry + first golden       DONE
  → TypeScript / Python generation + checked-in drift check         DONE
  → Buf lint / breaking + cross-language compatibility              DONE
  → deterministic CBOR profile + canonical golden                   DONE
  → protobuf / CBOR sanitizer fuzz smoke                            DONE
  → Rust typed framing + Unix/Windows platform adapters             PARTIAL PASS
  → TS/Python transport clients                                     PARTIAL PASS
  → ServiceDirectory schema + Rust negotiation core                 PARTIAL PASS
  → TS/Python directory negotiate-and-connect                       PARTIAL PASS
  → common SABI metadata/error/safe-retry validation                PARTIAL PASS
  → durable idempotency authority                                  PARTIAL PASS
  → durable idempotency SABI reconnect integration                 PARTIAL PASS
  → deadline/cancel/uncertain state machine                        PARTIAL PASS
  → Operation query/cancel payload + async timer/worker            PARTIAL PASS
  → Go/C# generation/golden probes + one independent IPC PoC       DEFERRED（B-TASK 纵切面之后）
```

`B-TASK` 首个切片验收门（议题 31 证据门条 1–3 与条 7 的子集）：

```text
durable TaskAuthority：Task 注册、TaskSnapshot 冻结输入 digest、TaskHead revision CAS   PARTIAL PASS（本地，B-TASK-001）
  → 双 TaskAttempt 注册：独立 generation、独立取消域，均绑定同一 TaskSnapshot          PARTIAL PASS（本地，B-TASK-001）
  → CommitPermit 唯一发放：只有一个 Attempt 获得 permit 并推进 TaskHead                PARTIAL PASS（本地，B-TASK-001）
  → losing/cancelled/stale Attempt 不得推进 TaskHead、不得覆盖 winner Receipt          PARTIAL PASS（本地，B-TASK-001）
  → cancel 与 permit 竞态只有规范允许的线性化结果（cancel-first / permit-first）        PARTIAL PASS（本地，B-TASK-001）
  → authority 重启后 TaskHead/Attempt/Permit 状态可恢复，无幽灵 permit                 PARTIAL PASS（本地，B-TASK-001）
```

六条均经本地 macOS/arm64 复验 + 双线程竞态 + 三平台 CI（run 30905979180）验证；`nlos-store-fault` 故障注入接入后才考虑晋升。

`B-TASK` 第二个切片验收门（议题 31 证据门条 4 与条 7 的 permit 维度，B-TASK-002）：

```text
planned slot 集承诺 + LogicalEffectId 确定性公式（descriptor 无禁止字段）            PARTIAL PASS 候选（本地，13 测试）
  → 只有 CommitPermit 持有者可签发 EffectPermit（TASK-RACE-001）                     PARTIAL PASS 候选
  → 签发 CAS PLANNED→PERMITTED + 一次性 dispatch token（二次消费 fail-closed）        PARTIAL PASS 候选
  → cancel 后迟到 dispatch 类型化拒绝；已消费 token 不得伪装未执行                    PARTIAL PASS 候选
  → slot 闭合 EFFECT_CLOSED/EFFECT_UNKNOWN/NO_EFFECT；UNKNOWN 跨重启阻塞              PARTIAL PASS 候选
  → finalize 收紧：任何 open/unknown slot 禁止关闭 permit；schema v1→v2 无损迁移      PARTIAL PASS 候选
```

`B-TASK` 第三个切片（B-TASK-003，议题 31 条 4 完整化与条 5–6 语义层 + 条 7 effect 维度）：

```text
EFFECT_UNKNOWN → QUARANTINED tombstone（head 冻结、禁新 winner、重放同 lifecycle）   PARTIAL PASS（本地，21 测试）
  → adoption 限权 RECONCILE_CLOSE_OR_QUARANTINE_ONLY（禁新 permit/dispatch）          PARTIAL PASS
  → reconcile：UNKNOWN → EFFECT_CLOSED | CONFIRMED_NO_EFFECT | 回 QUARANTINED          PARTIAL PASS
  → 跨 Attempt effect history（同事务追加、seq 无洞、fence 严格推进、回读）             PARTIAL PASS
  → required 成功语义完整（EFFECT_CLOSED+断言 | CNA+绑定证明；skip 绝不 COMMITTED）     PARTIAL PASS
  → 三点崩溃窗口 + effect 表组故障矩阵（议题 31 条 5–6 测试层）                        PARTIAL PASS 候选（并行切片，11 测试）
```

`B-TASK` 组织与提交绑定切片（B-TASK-004/005，议题 31 条 8 前置）：

```text
TaskGroup membership generation/root CAS + Admission/Removal Receipt                  PARTIAL PASS 候选（B-TASK-004）
  → 树状取消 + ALL/ANY 聚合 + quarantine 父组降级                                    PARTIAL PASS 候选
  → WriteSet/CommitPermit 捕获当前 membership generation/root/policy                  PARTIAL PASS（B-TASK-005）
  → EffectPermit/dispatch/finalize 前 membership 漂移 fail-closed                     PARTIAL PASS
  → TaskCommitReceipt-shaped record 逐位继承 binding + v4→v5 无损迁移                 PARTIAL PASS
  → Artifact staged revision + Artifact 域内 publication receipt                    PARTIAL PASS（B-ARTIFACT-002）
  → TaskAuthority immutable Artifact publication plan + permit write-set binding     PARTIAL PASS（B-TASK-006A）
  → publication receipt 强绑定消费 + PUBLISHING/READY 重启状态                       PARTIAL PASS（B-TASK-006B）
  → 发布前 TaskAuthority 授权 + grouped membership freeze                            PARTIAL PASS（B-TASK-006C）
  → READY-only prepared finalize + nested TaskCommitReceipt + restart replay          PARTIAL PASS（B-TASK-006D）
  → cross-authority coordinator + crash/restart automatic convergence                 PARTIAL PASS（B-TASK-006E）
  → coordinator transaction-abort fault isolation + repair convergence                PARTIAL PASS（B-TASK-006F）
  → best-effort pending scan + per-plan failure isolation/reporting                    PARTIAL PASS（B-TASK-006G）
  → TaskAuthority-owned recovery worker decision                                      ACCEPTED（ADR-0004）
  → startup scan + periodic retry/backoff + lifecycle health                           PARTIAL PASS（B-TASK-006H）
  → durable retry/escalation ledger + deterministic jitter                             PARTIAL PASS（B-TASK-006I）
  → worker durable scheduling integration + operations health summary                  PARTIAL PASS（B-TASK-006J）
  → durable alert acknowledgement Receipt + unacknowledged gauge                        PARTIAL PASS（B-TASK-006K）
  → typed/sanitized SystemControl recovery schema                                        PARTIAL PASS（B-SCHEMA-014）
  → TaskAuthority handler + ServiceDirectory + Unix local IPC                            PARTIAL PASS（B-TASK-006L）
  → backend-neutral metrics exporter                                                      PARTIAL PASS（B-TASK-006M）
  → real Capability/peer authority + bounded rejection Receipt + Windows handler IPC      DEFERRED（B-CONTROL/Receipt authority）
  → worker/dual-authority real VFS + process crash fault matrix                            PARTIAL PASS（B-TASK-006N）
  → complete TaskWriteSet + TaskSnapshotReceipt                                            READY
```

议题 31 证据门条 1–7 的 Task/Effect 核心语义至此全部具有至少 H3 级本地证据；TaskGroup membership generation/root 已由 B-TASK-004 实现，WriteSet/CommitPermit/TaskCommitReceipt 的 permit-time 组绑定与漂移围栏已由 B-TASK-005 取得局部 H3 证据，Artifact staged revision 与 Artifact 域内 publication receipt 已由 B-ARTIFACT-002 取得局部 H3 证据。条 8 的本地双 authority 路径已经具备 TaskAuthority publication authorization、Artifact publication nested Receipt、READY-only terminal transaction、重启收敛和逐 plan 故障隔离（B-TASK-006A–006G）；[ADR-0004](./adrs/0004-task-authority-commit-recovery-owner.md) 确定 TaskAuthority 是恢复生命周期 owner，[B-TASK-006H](../evidence/stage-b/b-task-006h-task-authority-recovery-worker.md) 补上 worker lifecycle，[B-TASK-006I](../evidence/stage-b/b-task-006i-durable-recovery-ledger.md) 补上 durable retry truth，[B-TASK-006J](../evidence/stage-b/b-task-006j-worker-durable-scheduling-health.md) 完成 worker 接线与本地 health 汇总，[B-TASK-006K](../evidence/stage-b/b-task-006k-durable-recovery-alert-acknowledgement.md) 建立不会误触发恢复的 durable alert Receipt 和未确认 gauge，[B-SCHEMA-014](../evidence/stage-b/b-schema-014-system-control-recovery-contract.md) 建立统一 typed/sanitized recovery schema，[B-TASK-006L](../evidence/stage-b/b-task-006l-system-control-recovery-handler.md) 接通 TaskAuthority handler、ServiceDirectory 与 Unix local IPC，[B-TASK-006M](../evidence/stage-b/b-task-006m-recovery-metrics-export.md) 建立 backend-neutral 指标 catalog 并以 TaskAuthority live summary 覆盖过期 worker cache，[B-TASK-006N](../evidence/stage-b/b-task-006n-dual-authority-vfs-process-fault-matrix.md) 以真实 VFS 和强制终止子进程验证跨 authority 已提交前缀可由新 worker 收敛。当前下一验收门是 **complete TaskWriteSet + TaskSnapshotReceipt**，随后补齐 sealed membership rebase，再进入 Slice K 首次端到端纵切。真实 Capability/peer authority、bounded rejection Receipt 与 Windows handler IPC 因缺少 durable rejection Receipt authority 而明确延后至 B-CONTROL/Receipt authority；这不是已实现事实。现有证据仍只覆盖单节点两个本地 SQLite authority 之间的可恢复提交协议，不得外推为真实硬件掉电、分布式原子事务、跨 term 接管或 Slice K 完成。

多语言 SDK 扩展按 [`B-SDK-LANG-EVAL`](./language-sdk-support-plan.md) 单独晋级：Go 与 C# 的 generation/golden 探针与独立 IPC PoC 自 2026-08-04 起后移至 `B-TASK`/EffectPermit 纵切面通过之后（议题 31/32 顺序变更），不在只有 generated types 时宣称“已支持”；Rust/TypeScript/Python 三语言现有 PARTIAL PASS 证据保持有效。

`B-OUTBOX` 的已验收条件（供追溯）：commit 前无 wake；崩溃重放不丢失、不制造旧 generation wake；duplicate 无第二次逻辑唤醒/reconciliation；bounded queue 不阻塞 writer/cancel；测试覆盖 current/late/cancel-before-dispatch/crash-restart 场景；Evidence 已同步三 PoC 集成缺口并保持 `PARTIAL_PASS` 直到故障注入通过。

## 6. 阶段退出门映射

| Exit gate | 当前结论 |
|---|---|
| `ROAD-B-001` 第三方 Application 安装/更新/卸载 | 未开始 |
| `ROAD-B-002` Application 多 Process、后台 Task、UI Surface | 未开始 |
| `ROAD-B-003` 双 Attempt、cancel/commit、handle 泄漏、snapshot、provider cache、effect fence | 局部推进：双 Attempt 唯一 CommitPermit、cancel/commit 线性化、effect fence 四态与 quarantine/reconcile、树状取消、permit-time 组绑定/漂移围栏及 Artifact staged publication 已有单节点 H3，B-TASK-001 已有三平台证据（B-TASK-001~005、B-ARTIFACT-002）；跨 authority prepare/finalize、handle 泄漏、完整 TaskSnapshot/TaskWriteSet、provider cache、Process 绑定与新增切片三平台复验未完成 |
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
