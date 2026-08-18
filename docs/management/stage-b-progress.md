# 阶段 B 权威进度单

> 本轮（2026-08-18）主线推进：`B-TASK-008C2G` 声明的下一门第一项落地——takeover barrier observation 接入 NLOS principal 签名验证（[B-TASK-008C2G-BARRIER-SIG](../evidence/stage-b/b-task-008c2g-barrier-principal-signature.md)）：`nlos-identity` 新增 `KeyPurpose::BarrierObservationSigning` 与 `verify_barrier_observation_signature`（八步链对齐 semantic 先例、零新增错误变体）；`nlos-task` schema v36 为 observation 表增加五个可空 signer 列（coupled 触发器强制同现同缺，旧行 NULL 不伪造），新公开 API `record_authority_takeover_barrier_receipt_signed` 在共享观察校验（manifest membership + root 复算）后对含服务端 `fence_set_root` 的 domain-separated preimage 验签，signer 列写入取 verified proof 而非 caller 自报；coverage 判定、parent completion、successor 激活语义不变。workspace 415 测试全过（404 基线 + 11 新增），clippy/fmt 清洁；三平台 CI + MSRV 1.97 job 已通过（[run 32099012698](https://github.com/cty12356541/llmos/actions/runs/32099012698)，MSRV job 首次实跑）。另本轮早前完成全项目评估后的四项工程风险消除（pump 类型化错误、store-fault clippy 恢复、CI 缓存/scale-probe/MSRV/dependabot、TaskStore 迁移链拆分），见下方注记与提交 `ca29756`/`9098c53`/`80f2cef`/`458bd57`。

> 本轮（2026-08-18）工程风险消除：全项目深度评估后落地四项加固——`nlos-runtime-tokio` outbox pump 线程启动 `.expect` 改为类型化 `OutboxPumpStartError`（提交 `ca29756`）；`nlos-store-fault` 显式恢复 clippy all/pedantic 约束、消除 lint 盲区（`9098c53`）；CI 接入 rust-cache、新增夜间 scale-probe job（`--include-ignored` 首次将两个 10 万级规模探针纳入 CI）与 MSRV 1.97 校验 job、dependabot（`80f2cef`）；`nlos-task` 7373 行 store.rs 的 35 个迁移函数机械拆分至 `migrations.rs`、零行为变化（`458bd57`）。workspace 404 测试全过、fmt/clippy `-D warnings` 全绿；三平台 CI + MSRV job 已通过（run 32099012698）。

> 近期增量（`B-RESOURCE-005` + `B-TASK-008C2G-FAULT` 故障矩阵系列，2026-08-16~17，明细见第 3 节表格与对应 Evidence）：`finalize_fault_injection.rs`（7 测试）验证 kill-9/commit 后崩溃/IoErr/ENOSPC/PowerLossAfter 下 finalize 双重记账无幻影 receipt/overlay/退款且重做 receipt id 确定性一致；`lease_binding_fault_injection.rs`（7 测试）覆盖 v28/v29 lease-bound permit 签发/finalize/adoption 三写事务同型 F1–F4 矩阵；schema v27–v35 lease/takeover 表组 F1–F4 矩阵（7 测试）；schema v35 barrier observation endpoint-supplied digest 持久化（v33/v34 历史行保留 `NULL`）、barrier 写入与 coverage 路径 canonical `exact_fence_set_root` fail-closed 复算、fence member manifest inspect root/presence 一致性校验。takeover/lease-binding 两轮矩阵已通过三平台 CI（run 31962738904 / 31963113968）；resource 侧矩阵已随 run 32099012698 通过三平台复验。

> 本轮新增：`B-RESOURCE-004` 已补上缺少 effect-closed final usage 证明时的 Resource QUARANTINED freeze、immutable QuarantineReceipt 与迟到 consume 拒绝；该分支不移动余额、不冒充最终结算。
>
> 本轮继续新增：`B-SEMANTIC-004` 已补上 Semantic outbox owner-bound 单调 ACK writer；ACK 只表示 transport observation，不提升为 checkpoint/publication proof。`B-SEMANTIC-005` 再由 SemanticAuthority 生成 owner-derived publication receipt 与 local log-prefix checkpoint（schema v4）。`B-OP-FENCE-002` 又补上 durable Operation registration row 的 owner-derived endpoint proof/readback；旧 generation 在 proof 生成前拒绝。`B-TASK-008C2G-OP` 再将该 proof 接入 TaskWriteSet per-effect endpoint、participant registry 与 permit 前复核；schema v24 只扩约束并保留历史行。

> 状态：`ACTIVE / POC ACCEPTANCE PENDING`
>
> 最后更新：2026-08-18（本轮两段增量：①主线 `B-TASK-008C2G-BARRIER-SIG` takeover barrier observation principal 签名验证——identity 新 purpose/验签方法 + task schema v36 signed record，workspace 415 测试全过（404 基线 + 11 新增），clippy/fmt 清洁；②工程风险消除四项（pump 类型化错误 `ca29756`、store-fault clippy 恢复 `9098c53`、CI 缓存/scale-probe/MSRV/dependabot `80f2cef`、TaskStore 迁移链拆分 `458bd57`）——两段增量连同其前的 `8a7264e`（resource finalize 矩阵）已推送 origin/main 并通过三平台 + MSRV 1.97 CI（[run 32099012698](https://github.com/cty12356541/llmos/actions/runs/32099012698)），Pages 部署成功（run 32099012849）；此前 `B-RESOURCE-005` finalize/refund 双重记账与 `B-TASK-008C2G-FAULT` 系列故障矩阵保持有效，其中 takeover/lease-binding 两轮矩阵三平台 CI（run 31962738904 / 31963113968）已通过；schema v35 barrier digest 至 v27 起的 lease/takeover 表组、`B-TASK-008C2G-SEM` Semantic publication consumer、schema v25、nested receipt、本地混合 Effect + Semantic v3 终结 hook、slot/Receipt-bound success proof，以及 `B-TASK-008C2G-COORD` Semantic-only 跨 authority restart coordinator 增量保持有效）
>
> 本次增量（历史摘要）：`B-RESOURCE-003` 已补上 strict ACTIVE Reservation consume high-water 与 immutable ConsumptionReceipt；`B-SEMANTIC-003` 已补上 Semantic admission outbox 的 owner-consistent transport 回读；`B-RESOURCE-002` 已补上 ACTIVE Reservation activation receipt 的 owner 回读与重启 replay；`B-TASK-008C2G-RES`、`B-TASK-008C2G-ART` 与 `B-TASK-008C2G-PROCESS` 已补上 Resource Reservation / Artifact head / Process binding 的 permit 前 owner 复核；`B-TASK-008C2G-OP` 已补上 Operation endpoint 的 TaskWriteSet/participant registry 接线、schema v24 迁移与 permit 前 owner 复核；其中 Semantic publication receipt ownership 已由后续 ADR-0006 取代。
>
> 状态更正（2026-08-16）：用户已选择 ADR-0006 候选 1；`B-SEMANTIC-005` 已完成 SemanticAuthority publication receipt producer。当前下一门改为 TaskAuthority consumer 与 nested `TaskCommitReceipt.semantic_publications` 接线。上方“仍待 authority ownership 决策”仅是 2026-08-15 的历史摘要，现由 ADR-0006 取代。
>
> 状态更正（2026-08-16，Task consumer）：`B-TASK-008C2G-SEM` 已完成 TaskAuthority Semantic publication plan/owner receipt consumer、schema v25、READY/finalize 与 nested `SemanticTaskCommitReceipt.semantic_publications`；随后补上含 Effect slot 的 v3 同事务混合终结 hook。跨 authority coordinator/recovery 与完整 TaskWriteSet 仍保持未完成。
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
| `B-TYPES` | Rust workspace 与稳定 nominal ID / Generation / CancelEpoch | `DONE` | `crates/nlos-types`；`ADR-0001`；[B-TASK-006P](../evidence/stage-b/b-task-006p-shared-nominal-identity-spine.md) 扩展 TaskWriteSet 共享 identity spine | public schema 与生成约束未冻结 |
| `B-IDENTITY` | Principal、ControlDomain、版本化 identity snapshot 与 signing key authority | `IN_PROGRESS` | [B-IDENTITY-001](../evidence/stage-b/b-identity-001-principal-key-authority.md)：authority-assigned identity、Ed25519 key validity/revocation、Semantic signature verification PARTIAL PASS | 多 Principal domain merge/split、key rotation/custody、认证 session/attestation ingress、可信时钟与 fault matrix |
| `B-CAPABILITY` | Capability issue、attenuation、delegation、revoke 与 reference monitor | `IN_PROGRESS` | [B-CAPABILITY-001](../evidence/stage-b/b-capability-001-durable-attenuation-authority.md)：durable root issue/delegation Receipt、全维衰减、generation/ancestor fence、verified-signer Semantic authorization PARTIAL PASS | Namespace hierarchy narrowing、call-limit 消耗账本、跨进程认证入口、通用 object/right registry、fault matrix |
| `B-SEMANTIC` | canonical SemanticEvent、签名、lineage、Admission/Durability 与 authority view | `IN_PROGRESS` | [B-SEMANTIC-001](../evidence/stage-b/b-semantic-001-durable-assertion-admission.md)：durable Assertion；[B-SEMANTIC-002A](../evidence/stage-b/b-semantic-002a-canonical-intent-spec-body.md)：canonical IntentSpec identity；[B-SEMANTIC-002B](../evidence/stage-b/b-semantic-002b-durable-spec-event-admission.md)：durable signed SpecEvent admission/migration；[B-SEMANTIC-003](../evidence/stage-b/b-semantic-003-outbox-owner-readback.md)：admission outbox owner-consistent transport readback；[B-SEMANTIC-004](../evidence/stage-b/b-semantic-004-outbox-ack-writer.md)：owner-bound monotonic transport ACK；[B-TASK-007B1](../evidence/stage-b/b-task-007b1-authority-endpoint-proofs.md)：authority-assigned admission endpoint proof PARTIAL PASS | Judgment/Verification/Retraction、declassification、batch DAG、Trust View/checkpoint、跨进程 endpoint 签名/attestation、fault matrix |
| `B-PARTICIPANT` | TaskAuthority participant registry generation/root/freeze 与 endpoint proof | `IN_PROGRESS` | [B-TASK-007A](../evidence/stage-b/b-task-007a-self-participant-registry.md)：TaskStore self/freeze；[B-TASK-007B1/007B2](../evidence/stage-b/b-task-007b2-verified-participant-registration.md)：Artifact/Semantic proof + registration；[B-TASK-007C1/007C2](../evidence/stage-b/b-task-007c2-verified-resource-registration.md)：Driver/Resource proof + registration；[B-TASK-007D1](../evidence/stage-b/b-task-007d1-participant-binding-propagation.md)：EffectPermit/Task Receipt binding + online revalidation；[B-TASK-008C2G-OP](../evidence/stage-b/b-task-008c2g-operation-endpoint-binding.md)：Operation proof registration + per-effect TaskWriteSet binding PARTIAL PASS | Channel endpoint、operation prepare→activate、takeover/adoption coverage、fault matrix |
| `B-RUNTIME` | RuntimeAdapter 与 Tokio 有界 Fiber runtime | `PARTIAL_PASS` | [ADR-0001](./adrs/0001-stage-b-core-language-and-runtime.md)、[PoC-0001](../evidence/stage-b/poc-0001-tokio-fiber-runtime.md)；提交 `a211088` | wake latency/fairness、structured join/detach、CPU 分维计量、Process crash、跨平台 |
| `B-OP-FENCE` | Operation 状态机、callback identity、cancel/generation fence | `PARTIAL_PASS` | [PoC-0002](../evidence/stage-b/poc-0002-operation-callback-fence.md)；[B-OP-FENCE-002](../evidence/stage-b/b-op-fence-002-operation-endpoint-proof.md)：owner-derived Operation endpoint proof/readback、generation fence 与重启回读；[B-TASK-008C2G-OP](../evidence/stage-b/b-task-008c2g-operation-endpoint-binding.md)：TaskWriteSet/participant registry 接线 PARTIAL PASS | Driver authentication、Operation prepare→activate、EffectPermit、progress/stream callback、Channel endpoint；Tokio wake 集成已随 `B-OUTBOX`（PoC-0004）补齐 |
| `B-STORE` | SQLite WAL/FULL Operation authority、恢复、Outbox、durable dedup/result | `PARTIAL_PASS` | [ADR-0002](./adrs/0002-stage-b-sqlite-operation-authority.md)、[PoC-0003](../evidence/stage-b/poc-0003-sqlite-operation-authority.md)、[B-SCHEMA-010](../evidence/stage-b/b-schema-010-durable-idempotency-result.md)、[B-SCHEMA-011](../evidence/stage-b/b-schema-011-durable-idempotency-ipc.md)、[B-SCHEMA-012](../evidence/stage-b/b-schema-012-deadline-cancel-state-machine.md)、[B-SCHEMA-013](../evidence/stage-b/b-schema-013-operation-control-timer-worker.md)；F1–F7、authority、真实重连、server restart、durable no-effect 与 cancel epoch CAS 已验证 | 100K 逐条生产写入、真实硬件掉电/更多文件系统仍超出当前证据 |
| `B-OUTBOX` | Durable Outbox → Tokio Fiber wake/reconcile consumer | `DONE` | [PoC-0004](../evidence/stage-b/poc-0004-outbox-wake-consumer.md)；本提交及评审后 remediation 提交（hash 见 git log 与 commit receipt） | durable wait registry/fiber rehydration 归 `B-PROCESS`/Slice K；此前移交 `B-STORE-FAULT` 的 F1–F7 已全部通过。2026-08-01 remediation：评审指出的 pump 错误路径可观测性（失败计数/根因/有上限退避/Faulted 终态）、drain panic 防护、shutdown 终态语义与 wake 重缓冲已补齐并各有测试。2026-08-01 复验残余（非阻塞，详见 PoC-0004 §8.4）：持久 apply 失败（`stopped_at` 路径）暂无 health 信号 → 后续 observability 项；`Faulted` 恢复依赖外部监督 → `B-PROCESS`；`PumpHealth.last_error` 跨 IPC 边界需脱敏 → `B-CONTROL`/`B-SCHEMA`；`Buffered` 驻留仅随 fiber 终态清理 → `B-PROCESS`/Slice K |
| `B-STORE-FAULT` | SQLite fault-injection：kill-9、torn-write、disk-full、checkpoint/backup、migration、长读事务、100K metadata、跨平台 | `DONE` | [PoC-0003 F1–F7 增量证据](../evidence/stage-b/poc-0003-sqlite-operation-authority.md)；[三平台 CI run 30714584445](https://github.com/cty12356541/llmos/actions/runs/30714584445) | 100K 逐条生产写入、真实硬件掉电/更多文件系统保留为扩展 Evidence，不阻塞本工作包 |
| `B-SCHEMA` | Protobuf/CBOR、golden vector、版本演进和本地 typed IPC | `IN_PROGRESS` | [ADR-0003](./adrs/0003-stage-b-idl-and-canonical-encoding.md)、[B-SCHEMA-001](../evidence/stage-b/b-schema-001-protobuf-envelope.md)、[B-SCHEMA-002](../evidence/stage-b/b-schema-002-cross-language-generation.md)、[B-SCHEMA-003](../evidence/stage-b/b-schema-003-deterministic-cbor.md)、[B-SCHEMA-004](../evidence/stage-b/b-schema-004-schema-fuzz-smoke.md)、[B-SCHEMA-005](../evidence/stage-b/b-schema-005-local-typed-ipc.md)、[B-SCHEMA-006](../evidence/stage-b/b-schema-006-typescript-python-ipc-clients.md)、[B-SCHEMA-007](../evidence/stage-b/b-schema-007-service-directory-negotiation.md)、[B-SCHEMA-008](../evidence/stage-b/b-schema-008-cross-language-directory-chain.md)、[B-SCHEMA-009](../evidence/stage-b/b-schema-009-common-sabi-semantics.md)、[B-SCHEMA-010](../evidence/stage-b/b-schema-010-durable-idempotency-result.md)、[B-SCHEMA-011](../evidence/stage-b/b-schema-011-durable-idempotency-ipc.md)、[B-SCHEMA-012](../evidence/stage-b/b-schema-012-deadline-cancel-state-machine.md)、[B-SCHEMA-013](../evidence/stage-b/b-schema-013-operation-control-timer-worker.md)、[三平台 reconnect run 30740180511](https://github.com/cty12356541/llmos/actions/runs/30740180511)、[三平台 restart run 30741046472](https://github.com/cty12356541/llmos/actions/runs/30741046472)、[三平台 deadline/cancel run 30741733804](https://github.com/cty12356541/llmos/actions/runs/30741733804)、[三平台 OperationControl run 30743421174](https://github.com/cty12356541/llmos/actions/runs/30743421174)、[fuzz run 30743421200](https://github.com/cty12356541/llmos/actions/runs/30743421200)、[B-SCHEMA-014](../evidence/stage-b/b-schema-014-system-control-recovery-contract.md)：typed/sanitized SystemControl recovery contract PARTIAL PASS；`schema/`、`gen/`、`sdk/`、`crates/nlos-schema`、`crates/nlos-service-directory`、`crates/nlos-canonical`、`crates/nlos-ipc`、`fuzz/` | Namespace bootstrap authority、生产目录 watch/lease/rebind、持久 deadline queue/restart recovery、Receipt authority、双向 peer auth、Python Proactor 稳定 profile、CBOR 跨语言、长期 fuzz、actual signing |
| `B-SDK-LANG-EVAL` | 官方 SDK 语言集合与 Go/C# 优先兼容评估 | `BLOCKED` | [多语言 SDK 支持评估计划](./language-sdk-support-plan.md)；OperationControl 前置切片见 [B-SCHEMA-013](../evidence/stage-b/b-schema-013-operation-control-timer-worker.md) | 2026-08-04 起 Go/C# generation/golden 探针与独立 IPC PoC 后移至 `B-TASK`/EffectPermit 纵切面之后（议题 31/32：第四种语言不能证明核心成立，且不应推动 SABI 在 Task/Effect 语义稳定前过早冻结）；Java/Kotlin、Swift、C/C++ 需求驱动复审 |
| `B-SANDBOX` | Wasmtime/WASI 与独立 host Process 隔离对比 | `READY` | [技术选型第 5 节](./stage-b-technology-selection.md) | capability import、fuel/epoch、memory、host crash、GuaranteeTier |
| `B-PROCESS` | native Process supervisor 与平台资源/生命周期 adapter | `IN_PROGRESS` | [v0.5 Process 规范](../design/06-架构设计总纲-v0.5.md)；[B-PROCESS-001](../evidence/stage-b/b-process-001-durable-execution-binding-authority.md)：Process/AgentInstance/IsolationDomain durable generation/fence authority PARTIAL PASS | 完整 BirthDecision、多 authority prepare/activate、macOS/Windows/Linux spawn/suspend/kill、checkpoint、IsolationUnit 与 resource mapping |
| `B-RESOURCE` | ResourceAccount、Driver/Device、Reservation 与 activation reference monitor | `IN_PROGRESS` | [B-RESOURCE-001](../evidence/stage-b/b-resource-001-driver-reservation-binding-authority.md)：Driver generation/fence + AVAILABLE→RESERVED→ACTIVE binding；[B-RESOURCE-003](../evidence/stage-b/b-resource-003-consumption-high-water.md)：strict ACTIVE consume high-water + immutable ConsumptionReceipt；[B-RESOURCE-004](../evidence/stage-b/b-resource-004-quarantine-freeze.md)：缺少 effect-closed 证明时 QUARANTINED freeze + immutable QuarantineReceipt；[B-RESOURCE-005](../evidence/stage-b/b-resource-005-finalize-refund.md)：effect-closed 证明下双重记账 finalize/refund 结算 + immutable FinalizationReceipt；[B-TASK-007C1](../evidence/stage-b/b-task-007c1-resource-endpoint-proofs.md)：Driver gateway + Resource/Ledger authority endpoint proofs PARTIAL PASS | Task registry/operation admission 接线、多维 demand、完整 Ledger/risk/rebate、AdmissionPlan、ControllerBinding、真实 enforcement shim 与平台 Device adapter |
| `B-TASK` | TaskPlan/TaskNode、lazy materialization、TaskSnapshot、双 Attempt 唯一提交 | `IN_PROGRESS` | [v0.5 Task 规范](../design/06-架构设计总纲-v0.5.md)；2026-08-04 起为唯一主线工作包（议题 31/32 顺序变更采纳）；既有 B-TASK-001～007D1 证据保持有效；[B-TASK-008A](../evidence/stage-b/b-task-008a-verified-write-set-seal.md)：TaskAuthority 绑定 receipted snapshot、current group/participant registry，并由 ArtifactAuthority 回读 exact artifact heads，持久化 canonical artifact-read/write-set roots；[B-TASK-008B1](../evidence/stage-b/b-task-008b1-process-write-set-binding.md)：ProcessAuthority owner-derived endpoint proof、active Process/AgentInstance/IsolationDomain binding readback 与 participant-registry pre-registration；[B-TASK-008B2](../evidence/stage-b/b-task-008b2-semantic-resource-write-set-binding.md)：Semantic event log/canonical readback、RESERVED Resource Reservation owner readback、endpoint pre-registration 与 schema v16 immutable read children；[B-TASK-008C1](../evidence/stage-b/b-task-008c1-planned-effect-write-set-binding.md)：schema v17 immutable planned-effect children、canonical effect root、sealed-root permit exact binding/replay；[B-TASK-008C2A](../evidence/stage-b/b-task-008c2a-effect-endpoint-binding.md)：schema v18 immutable per-effect owner endpoint proofs、v4 write-set root、OPEN registry pre-registration 与 permit-time frozen membership；[B-TASK-008C2B](../evidence/stage-b/b-task-008c2b-artifact-write-publication-binding.md)：schema v19 Artifact proposed-write declaration、schema v20 separate permit/publication roots、post-permit staging and plan expectation binding；[B-TASK-008C2C](../evidence/stage-b/b-task-008c2c-semantic-append-binding.md)：schema v21 Semantic append declaration、target scope 与直接 Durable AdmissionReceipt owner readback、append root/permit binding；[B-TASK-008C2D](../evidence/stage-b/b-task-008c2d-semantic-durability-observation.md)：schema v22 可选 DurabilityReceipt owner readback、append-root v2 与 direct-Durable 兼容 PARTIAL PASS；[B-TASK-008C2E](../evidence/stage-b/b-task-008c2e-semantic-finalization-guard.md)：Issued permit 终结前重新读取 Semantic event/Admission/Durability owner proofs，并保持 terminal replay 不访问 owner；[B-TASK-008C2F](../evidence/stage-b/b-task-008c2f-semantic-admission-policy-binding.md)：schema v23 expected admission-policy owner readback、append-root v3 与历史 NULL 迁移；[B-TASK-008C2G-OP](../evidence/stage-b/b-task-008c2g-operation-endpoint-binding.md)：schema v24 Operation endpoint/participant registry 接线、seal/permit owner revalidation PARTIAL PASS | Semantic checkpoint producer/publication receipt/final TaskCommitReceipt、Artifact publication receipt consumption、Resource activation/consume/finalize、Operation prepare→activate、Channel endpoint、legacy no-seal planned-effect path 收敛与完整 TaskWriteSet 仍未实现；本切片不等同于 complete TaskWriteSet |
| `B-TASK-008C2G-RES` | Resource Reservation permit 前 owner 复核 | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-task-008c2g-resource-permit-owner-revalidation.md) | Resource activation/consume/finalize、publication receipt 与跨 authority complete 仍未实现 |
| `B-TASK-008C2G-ART` | Artifact head permit 前 owner 复核 | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-task-008c2g-artifact-permit-owner-revalidation.md) | Artifact staging/publication receipt consumption 与统一 TaskCommitReceipt 接线仍未实现 |
| `B-TASK-008C2G-PROCESS` | Process binding permit 前 owner 复核 | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-task-008c2g-process-permit-owner-revalidation.md) | Process rotation/跨 authority prepare→activate 与 Operation/Channel linkage 仍未实现 |
| `B-TASK-008C2G-OP` | Operation endpoint TaskWriteSet / participant registry 接线 | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-task-008c2g-operation-endpoint-binding.md)：schema v24、owner proof registration、per-effect seal 与 permit 前复核 | Operation prepare→activate/dispatch、跨进程认证、Channel endpoint、统一 TaskCommitReceipt 与完整 TaskWriteSet 仍未实现 |
| `B-RESOURCE-002` | Resource activation receipt owner 回读 | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-resource-002-activation-receipt-readback.md) | Task 消费/统一 receipt、CLOSING/UNCERTAIN 与跨 authority lifecycle 仍未实现；strict consume high-water 见 B-RESOURCE-003，finalize/refund 见 B-RESOURCE-005 |
| `B-SEMANTIC-003` | Semantic admission outbox owner 回读 | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-semantic-003-outbox-owner-readback.md) | 跨进程 consumer/ACK、checkpoint/publication receipt 与 TaskCommitReceipt 接线仍未实现；owner ACK writer 见 B-SEMANTIC-004 |
| `B-SEMANTIC-004` | Semantic outbox owner ACK writer | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-semantic-004-outbox-ack-writer.md) | 跨进程 consumer 认证/租约、checkpoint/publication receipt 与 TaskCommitReceipt 接线仍未实现 |
| `B-SEMANTIC-005` | Semantic publication receipt producer | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-semantic-005-publication-receipt-producer.md)；[ADR-0006](./adrs/0006-semantic-publication-receipt-owner.md) | Task consumer 已由 B-TASK-008C2G-SEM 接线；跨 authority prepare/consume/recovery、Trust View/vector checkpoint 与多 Cell 仍未实现 |
| `B-TASK-008C2G-SEM` | TaskAuthority Semantic publication consumer 与混合终结 | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-task-008c2g-semantic-publication-consumer.md) | 单节点 TaskAuthority 内混合 Effect + Semantic v3 终结与 slot/Receipt-bound success proof 已接线；跨 authority coordinator/recovery、外部 provider proof/attestation、Trust View/vector checkpoint、多 Cell 与完整 TaskWriteSet 仍未实现 |
| `B-TASK-008C2G-COORD` | Semantic publication cross-authority restart coordinator | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-task-008c2g-semantic-coordinator.md)；[B-TASK-008C2G-FAULT](../evidence/stage-b/b-task-008c2g-takeover-fault-matrix.md)：schema v27–v35 lease/takeover 表组 F1–F4 故障注入矩阵（kill-9 中断/commit 后崩溃/IoErr/ENOSPC/静默丢写与 WAL 撕裂尾部/解除后继续） | schema v26 mixed finalize envelope、单机 bounded convergence、本地 slot/Receipt-bound proof binding、schema v27 durable lease/term/fencing、schema v28 opt-in CommitPermit/terminal binding、schema v29 same-term adoption/reconcile lease guard、schema v30 immutable local `FROZEN_FOR_TAKEOVER` fence receipt/exact local root、schema v31 lease-bound local assignment baseline、schema v32 pending takeover receipt prefix、schema v33 per-endpoint barrier observation、schema v34 canonical exact-fence member manifest、schema v35 barrier digest 与本地 takeover fault matrix 已接线；NLOS principal/签名 peer auth、远端 barrier 验证/完成、successor assignment 激活、跨 term adoption、lease-binding fault matrix 三平台复验、外部 provider proof/attestation、Trust View/vector checkpoint、多 Cell 与完整 TaskWriteSet 仍未实现 |
| `B-RESOURCE-003` | Resource strict consume high-water | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-resource-003-consumption-high-water.md) | CLOSING/UNCERTAIN、risk/unknown usage、late rebate 与跨 authority resource/cost receipt 仍未实现；finalize/refund 见 B-RESOURCE-005 |
| `B-RESOURCE-004` | Resource QUARANTINED freeze | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-resource-004-quarantine-freeze.md) | endpoint 签名 final usage proof 与跨 authority resource/cost receipt 仍未实现；QUARANTINED→FINALIZED 证明后解冻与 ACTIVE finalize/refund 均见 B-RESOURCE-005 |
| `B-RESOURCE-005` | Reservation finalize/refund 双重记账结算 | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-resource-005-finalize-refund.md)：schema v5 overlay、immutable FinalizationReceipt、`upper_bound - final_usage` 同事务退款、FINALIZED overlay 绑定不可变、QUARANTINED→FINALIZED reconciliation 解冻、v4→v5 幂等迁移、finalize 表组 F1–F4 故障矩阵（`nlos-resource` 接入 fault VFS） | endpoint 签名 proof、risk/rebate ledger、跨 authority resource/cost receipt 与 TaskCommitReceipt resource consumption 接线仍未实现 |
| `B-OP-FENCE-002` | Operation owner endpoint proof/readback | `PARTIAL_PASS` | [Evidence](../evidence/stage-b/b-op-fence-002-operation-endpoint-proof.md)；TaskWriteSet 接线见 [B-TASK-008C2G-OP](../evidence/stage-b/b-task-008c2g-operation-endpoint-binding.md) | Operation prepare→activate、跨进程签名/租约/attestation、Channel endpoint 与 Operation completion 仍未实现 |
| `B-CONTROL` | CLI/API/NL/GUI 共用 ControlCommand 与 Receipt | `READY` | [v0.5 控制面规范](../design/06-架构设计总纲-v0.5.md) | SystemControl client、权限 UI、多层手动调度、等价路径证明 |
| `B-ARTIFACT` | 内容寻址 Artifact、metadata、reconcile、GC | `IN_PROGRESS` | [B-ARTIFACT-001](../evidence/stage-b/b-artifact-001-content-addressed-store.md)：内容寻址 blob 五步写入协议 + SQLite metadata + 崩溃窗口/reconcile + cache 分域（26 测试含 VFS 故障注入）PARTIAL PASS；[B-ARTIFACT-002](../evidence/stage-b/b-artifact-002-staged-publication.md)：staged revision + Artifact 域内原子 publish + immutable publication receipt + v1→v2 迁移（33 测试）；[B-TASK-007B1](../evidence/stage-b/b-task-007b1-authority-endpoint-proofs.md)：per-head authority endpoint proof PARTIAL PASS | Task registry/operation admission 接线、GC 执行、retention policy、加密/provenance/legal hold、Package 签名验证、sync/对象存储后端、Windows 目录 fsync 等价物、真实 ENOSPC 探针 |
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
- 新增 `PeerCredentialBinding` / `ExactPeerAuthorizer`：服务可把一次观察到的 Unix PID/UID/GID 或 Windows peer identity 逐字段绑定，任何 tuple/platform 漂移在读 frame 前拒绝；这是 OS credential pre-gate，不是 NLOS Principal、签名 attestation 或 authority lease proof。
- macOS 真实 Unix socket 往返、owner-only `0600`、peer credential hook，以及超界、半帧、断连、未授权、串线、并发积压和 exact credential drift 等 9 项 IPC 测试通过。
- Windows named pipe adapter 已实现 local-only/first-instance/有界实例和 buffer、identification QoS、有界 busy retry；Windows-only 往返/timeout 测试和整仓 Clippy 已由 [三平台 run 30730221706](https://github.com/cty12356541/llmos/actions/runs/30730221706) 通过。
- B-SCHEMA-005 当时尚无 TypeScript/Python transport runtime client；该缺口已由 4.14/B-SCHEMA-006 补齐。ServiceDirectory runtime、Capability、deadline/cancel、Operation/Receipt、自动重连、Windows token/ACL、NLOS principal mapping、签名 peer attestation 和 lease/adoption 仍未实现。

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
- 详见 [B-TASK-001 fault-injection](../evidence/stage-b/b-task-001-fault-injection.md)；限制：macOS VFS 模拟 ≠ 真实断电、三平台 CI 已随 workspace run 32099012698 覆盖本文件、effect 表组（B-TASK-002 新增）与 F4/F5（checkpoint/backup/migration）矩阵未覆盖。

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
- `nlos-task` 90 项 integration tests、workspace rustfmt 与 crate Clippy 通过，详见 [B-TASK-005](../evidence/stage-b/b-task-005-commit-group-binding.md)；限制：完整 TaskWriteSet、sealed membership rebase、旧 root aggregate 过滤、Artifact/Semantic publication receipts、fault-injection 与三平台 CI 尚未完成。

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

### 4.47 Durable TaskSnapshotReceipt（B-TASK-006O）

- schema v10 持久化 immutable snapshot receipt 与 ordered/unique authority checkpoint Receipt 集，并绑定 head/history/fence、builder/version、dependency closure、resolver/iteration、consistency、authority/key/signature bytes。
- 新 receipted attempt 入口将 receipt ID 写入 attempt；重启可精确回读，legacy/receipted replay 不可互换，迁移不会给旧 attempt 发明证明。
- `MIXED_NON_SETTLEABLE` 可保存但不能授权 attempt；3 项新 integration tests 与既有 v1–v9 迁移链通过，详见 [B-TASK-006O](../evidence/stage-b/b-task-006o-durable-task-snapshot-receipt.md)。限制：真实 snapshot builder/checkpoint 查询、验签、canonical CBOR preimage 与完整 causal content 尚未完成。

### 4.48 Shared nominal identity spine（B-TASK-006P）

- `nlos-types` 补齐 TaskWriteSet 前置链所需的 typed ID：对象身份使用 16-byte nominal ID；`SemanticEventId` 按 v0.5 §16.1 使用 32-byte SHA-256 event identity。
- `TaskGroupId`、`EffectSlotId`、`EffectPermitId` 不再由 `nlos-task` 重复定义；crate root 继续兼容重导出共享类型。
- workspace check、`nlos-types`/`nlos-task` tests、workspace clippy 与 fmt 通过，详见 [B-TASK-006P](../evidence/stage-b/b-task-006p-shared-nominal-identity-spine.md)。限制：这不是 authority 实现，SABI/多语言生成和完整 TaskWriteSet 仍未完成。

### 4.49 Durable execution binding authority（B-PROCESS-001）

- 新增 `nlos-process` SQLite reference authority，由 authority 分配 Process/AgentInstance/IsolationDomain identity 与 fencing token；不接受调用者自报这些身份。
- Domain rotate 与 Process restore 都执行 generation CAS；restore 同步推进 Process/AgentInstance generation，active readback 逐位验证 Process 和 Domain 当前 fence。
- 5 项 integration tests 覆盖 replay/conflict、重启、Domain rotation、restore 双 generation 围栏和 DDL immutability，详见 [B-PROCESS-001](../evidence/stage-b/b-process-001-durable-execution-binding-authority.md)。限制：不是完整 BirthDecision 或 host supervisor，未接 Resource/Capability/Task prepare、真实平台进程和 fault VFS。

### 4.50 Driver / Reservation binding authority（B-RESOURCE-001）

- 新增 `nlos-resource` SQLite reference authority，由 authority 分配 Driver/Device/Quote/Reservation identity、driver fence 与 activation token。
- reserve 在同一事务完成 AVAILABLE 扣减和 immutable Operation binding；exact replay 不重复扣款，余额不足不留下半状态。
- permit readback 只接受 RESERVED + current Driver fence；activate 一次性写 immutable Receipt，Driver rotation 围栏旧 binding。5 项 integration tests 通过，详见 [B-RESOURCE-001](../evidence/stage-b/b-resource-001-driver-reservation-binding-authority.md)。限制：不是完整多维 Resource Manager/Ledger 或真实 Driver enforcement。

### 4.51 Principal / ControlDomain / signing-key authority（B-IDENTITY-001）

- 新增 `nlos-identity` SQLite WAL/FULL reference authority；可信本地 bootstrap 原子分配 Principal、单成员 ControlDomain、identity snapshot 与 Ed25519 public-key identity，不接受调用者自报这些稳定 ID。
- signing key 持久绑定 Principal、ControlDomain、用途、有效期、generation 与撤销状态；撤销同时执行 key generation 与 identity snapshot 双 CAS，旧 snapshot/key version 保持不可变且可按精确 snapshot 回读。
- Semantic admission 验签接口按 `SHA-256("llmos/semantic-signature/v1" || EventId)` 执行 strict Ed25519 verification，并 fail-closed 区分 binding、purpose、validity、revocation 与 signature failure。
- 5 项 integration tests 覆盖 bootstrap/restart/replay、真实签名与反例、双 fence 撤销/restart/history、idempotency/validity conflict 和 DDL immutability，详见 [B-IDENTITY-001](../evidence/stage-b/b-identity-001-principal-key-authority.md)。限制：这是本地 trusted-bootstrap reference slice，不包含认证 session/attestation、私钥 Keychain custody、多成员 ControlDomain merge/split、key rotation、可信时钟、Capability authority 或故障注入。

### 4.52 Durable Capability attenuation authority（B-CAPABILITY-001）

- 新增 `nlos-capability` SQLite WAL/FULL reference authority；root issue 通过 Identity authority 解析 issuer/holder，authority 分配 Capability identity，调用者不能靠自报 `CapabilityId` 建立权力。
- delegation 机械校验 parent holder、current generation/完整 ancestor chain、`DELEGATE` right，并要求 rights、target scope、purpose、validity、call-limit 和 remaining depth 单调衰减；每次 issue/delegate 产生 immutable Receipt。
- revoke 由 issuer/holder 驱动 generation CAS；child 持久绑定 parent generation，因此任一祖先撤销或换代都会使整条后代链 fail-closed。
- Semantic reference monitor 只接受 `IdentityAuthority::verify_semantic_signature` 返回的不可外部构造 signer proof，再逐位校验 holder ControlDomain、target、right、purpose、time 与 ancestor fences。6 项 integration tests 通过，详见 [B-CAPABILITY-001](../evidence/stage-b/b-capability-001-durable-attenuation-authority.md)。限制：call-limit 当前只证明委托不放大，尚无消费账本；target narrowing 仅支持 exact Namespace/Task，没有 Namespace hierarchy authority。

### 4.53 Durable Assertion admission authority（B-SEMANTIC-001）

- 新增 `nlos-semantic` SQLite WAL/FULL append-only authority；严格 decode + re-encode v1 Assertion deterministic CBOR，按 `SHA-256("llmos/semantic-event/v1" || canonical_unsigned_event)` 重算 32-byte EventId，并独立验证 content digest。
- append 顺序校验 Identity actual signature/key validity、Process current generation、Capability holder/scope/right/purpose/time、committed lineage 与 effective taint；同 EventId exact bytes 跨重启 replay 原 Receipt，已提交事件在后续 revoke 后仍可 replay，新事件 fail-closed。
- content/event/signature/log/lineage、store-key 实签的 DURABLE AdmissionReceipt 与 durable outbox 在同一事务提交；store signer signature 再经 Identity authority 验证，失败回滚全部行。
- 6 项 integration tests 覆盖 canonical 反例、durable admission/replay/store signature、跨 authority 失败回滚、dangling lineage/taint、revoke replay boundary 与 DDL append-only，详见 [B-SEMANTIC-001](../evidence/stage-b/b-semantic-001-durable-assertion-admission.md)。限制：当前只支持 Assertion；Process binding 尚未关联 Principal，FACT_FROM_TOOL evidence 仅检查存在字段而未接 Driver Receipt authority。

### 4.54 Canonical IntentSpec body identity（B-SEMANTIC-002A）

- 新增 bounded deterministic-CBOR `IntentSpecBody` profile；goal、criteria、constraints、criticality、settlement 与 extensions 全部进入 `SpecBodyDigest`，criterion 按重算 `CriterionId` 规范排序。
- AUTOMATIC settlement 必须绑定完整、非空 HARD set 的 `HardCriteriaDigest`；quorum、HARD MODEL/HUMAN policy、capability allow/forbid 集和 extension criticality 均 fail-closed。
- 5 项新增 tests 与原 6 项 Semantic tests 通过，详见 [B-SEMANTIC-002A](../evidence/stage-b/b-semantic-002a-canonical-intent-spec-body.md)。限制：ResourceVector/ArtifactSelector/policy 尚为 immutable digest reference；SPEC envelope 与 durable admission 尚未接通。

### 4.55 Durable SpecEvent admission（B-SEMANTIC-002B）

- SPEC envelope 绑定完整 canonical `IntentSpecBody` 与重算 `SpecBodyDigest`，EventId 同时覆盖 issuer/scope/nonce/time/lineage/execution/domain/key；不能用旁表漂移 body。
- `append_spec` 复用 Identity/Process/Capability/lineage/taint gates，并把 spec body/event/signature/log/edges、signed DURABLE AdmissionReceipt 与 outbox 原子提交；精确 replay 跨重启返回原 Receipt。
- schema v2 用 tagged XOR 区分 Assertion ContentDigest 与 SpecBodyDigest，`spec_bodies` append-only；真实 v1 Assertion/Receipt/lineage store 无损迁移并通过 FK 检查。15 项 Semantic tests 通过，详见 [B-SEMANTIC-002B](../evidence/stage-b/b-semantic-002b-durable-spec-event-admission.md)。

### 4.56 TaskStore self-participant registry（B-TASK-007A）

- schema v11 由 TaskAuthority 分配 durable TaskStore participant identity；Task 注册同事务建立 generation 1 OPEN registry、完整 root 与 immutable create Receipt，不接受调用者自报 self endpoint。
- CommitPermit issuance 同一 transaction CAS freeze registry 并逐位绑定 generation/root；exact replay 保留原 binding，旧 permit 闭合后的新竞争创建 successor generation/prior-root chain，不原地解冻历史 registry。
- 3 项新增 integration tests 与原 `nlos-task` 全套测试通过，详见 [B-TASK-007A](../evidence/stage-b/b-task-007a-self-participant-registry.md)。限制：当前只覆盖 TaskStore self participant；外部 endpoint proof、registration CAS、EffectPermit/Receipt binding 与 takeover fence 尚未实现。

### 4.57 Artifact/Semantic authority endpoint proofs（B-TASK-007B1）

- Artifact schema v3 为每个 head 建立 authority-assigned immutable participant identity/generation/Receipt，新建同事务写入、既有 Artifact 迁移回填；Semantic schema v3 建立 durable singleton admission endpoint proof。
- 两个 authority 提供 typed readback，proof 跨重启逐位稳定且 storage trigger 禁止 UPDATE/DELETE；完整旧结构可安全重盖 migration version，部分结构 fail closed。
- Artifact/Semantic 全套测试与 Clippy 通过，详见 [B-TASK-007B1](../evidence/stage-b/b-task-007b1-authority-endpoint-proofs.md)。限制：TaskAuthority 尚未在线验证/注册这些 proof；跨进程签名/attestation 与其余 endpoint 类型未实现。

### 4.58 TaskAuthority verified participant registration CAS（B-TASK-007B2）

- TaskAuthority 的 Artifact/Semantic 注册 API 直接回读具体 owning authority，不接收 caller-supplied participant tuple；proof 失败在 Task transaction 前返回 typed source error且零变更。
- 新 endpoint 以 expected generation/root + OPEN state 做 `BEGIN IMMEDIATE` CAS，旧 generation 进入 SUPERSEDED，新 generation/root 覆盖稳定排序全集；duplicate 收敛 replay、stale/frozen 均 fail closed。
- `nlos-task` participant integration tests 增至 5 项并通过，详见 [B-TASK-007B2](../evidence/stage-b/b-task-007b2-verified-participant-registration.md)。限制：Driver/Resource/Channel endpoint、operation prepare→activate、EffectPermit/Receipt binding 与 takeover fence 未实现。

### 4.59 Driver gateway / Resource-Ledger endpoint proofs（B-TASK-007C1）

- Resource schema v2 为 Driver gateway 建立稳定 participant identity + 逐 Driver generation Receipt，为 ResourceAccount 建立 Resource/Ledger identity/generation/Receipt；注册/rotation/account 创建与 proof 同事务。
- v1 migration 为全部既有 generation/account 回填 proof，partial coverage fail closed；typed readback、跨重启稳定与 DDL immutability 已验证。
- `nlos-resource` integration tests 从 5 项增至 7 项并通过，详见 [B-TASK-007C1](../evidence/stage-b/b-task-007c1-resource-endpoint-proofs.md)。限制：Task registry 接线、Channel endpoint、跨进程 attestation 与 operation prepare→activate 未实现。

### 4.60 TaskAuthority verified Driver/Resource registration（B-TASK-007C2）

- TaskAuthority 直接回读 ResourceAuthority，只接收 Driver/Account stable ID 与 expected generation；proof/计划 generation 不匹配在 Task transaction 前 typed fail closed、零 registry 变更。
- 同一 Driver participant identity 的严格更高 generation/new Receipt 以 OPEN registry CAS 替换旧 tuple而不增加 cardinality；旧/相同 generation 冲突、duplicate replay 与 restart 已验证。
- participant integration tests 增至 6 项并通过，详见 [B-TASK-007C2](../evidence/stage-b/b-task-007c2-verified-resource-registration.md)。限制：operation prepare→activate、planned endpoint seal、EffectPermit/Receipt binding、Channel endpoint 与 takeover fence 未实现。

### 4.61 EffectPermit / Task Receipt participant binding（B-TASK-007D1）

- Task schema v12 将 CommitPermit 冻结的 participant registry generation/root 逐位复制到新 EffectPermit 与 permit-backed Task commit/closure Receipt；历史记录不伪造 proof，unbound legacy permit 的新权威 mutation typed fail closed。
- EffectPermit issuance、dispatch、legacy/v3 finalize、permit closure 与 Artifact-only finalize 均在线回读当前 frozen registry；dispatch 还校验 EffectPermit→CommitPermit copy，Receipt/effect replay 拒绝 parent/copy 漂移。
- participant integration tests 增至 7 项，`nlos-task` 全测试与 Clippy 通过，详见 [B-TASK-007D1](../evidence/stage-b/b-task-007d1-participant-binding-propagation.md)。限制：complete TaskWriteSet/planned endpoint seal、term/takeover coverage、完整 CommitReceipt evidence 与跨进程 attestation 未实现。

### 4.62 authority-verified snapshot/read-set seal（B-TASK-008A）

- TaskAuthority 只接受已持有 `TaskSnapshotReceipt` 的 attempt，并在同一事务中复验 snapshot、TaskHead、当前 TaskGroup binding 与 OPEN participant registry；未收据 attempt、混合不可结算 snapshot 或漂移 binding 均 fail closed。
- Artifact read set 不接受 caller-supplied digest 作为事实：seal 前逐项回读 ArtifactAuthority 当前 head，按 artifact ID 排序计算 canonical artifact-read root，再与 snapshot/head/fence/group/participant binding 计算 canonical write-set root；schema v13 的 immutable parent/child tables 与 migration partial-schema fail-closed 已验证。
- 同一 idempotency key + root 可逐位 replay；不同 root、重复 artifact、revision/digest 漂移均 typed reject。CommitPermit 若携带该 root，会自动消费同一 sealed record 并复验 registry binding 后冻结 participant registry。
- `nlos-task` 全测试通过（本地 macOS/arm64）。这是 authority-verified snapshot/read-set 的 PARTIAL PASS，不声称 complete TaskWriteSet：Process/Agent/Isolation、Semantic append/control、Resource/Reservation、planned endpoint、effect set 与 write publication 尚未进入该 seal。

### 4.63 Process/AgentInstance/IsolationDomain binding（B-TASK-008B1）

- `nlos-process` 为当前 active Process binding 提供 owner-derived endpoint proof：participant identity 稳定绑定 ProcessId，participant generation 与 admission Receipt 随 Process generation/fence 变化；旧 IsolationDomain fence 或旧 Process generation 的 proof readback fail closed。
- TaskAuthority 新增 Process participant registration：直接回读 ProcessAuthority 的 task/attempt/generation、active Process/AgentInstance/IsolationDomain 全字段和 endpoint proof，拒绝把其他 TaskAttempt 的 Process 注册进当前 registry；registry 必须保持 OPEN，seal 不会在背后扩展 participant 集。
- schema v14 新增 immutable `task_write_set_process_bindings` child，schema v15 原子重建 participant type check（1–6 → 1–7）以纳入 ProcessBinding；历史 registry/TaskWriteSet rows 逐字复制，不伪造旧 Process 事实。
- 带 Process binding 的 seal 将 owner-read binding、endpoint proof、snapshot/head/group/participant/artifact roots 一并纳入 canonical write-set root；重启回读、owner mismatch、stale domain/process fence、未预注册 endpoint 和 idempotent replay 均有 typed coverage。`nlos-task`/`nlos-process` 测试与 workspace Clippy 通过。
- 仍为 PARTIAL PASS：ProcessAuthority 尚非完整 BirthDecision/跨 authority prepare→activate，也未接入 Semantic/Resource planned bindings、Channel、effect/publication 或跨 term takeover。

### 4.64 Semantic/Resource owner readback（B-TASK-008B2）

- Semantic read dependencies 逐项由 `SemanticAuthority::inspect_event` 回读，并校验 immutable event log sequence 与 canonical unsigned-event digest；重复 event、log/digest 漂移或缺少 Semantic admission endpoint pre-registration 均 typed fail closed。
- Resource Reservation dependencies 逐项由 `ResourceAuthority::inspect_permit_binding` 回读，只接受当前 Driver fence 下的 `RESERVED` binding，并校验 caller 期待的 Call/Operation/Quote；owner 返回的 account/quote/call/operation/driver/device/generation/fence/upper-bound 进入 immutable child 与 canonical resource root，activation token 不进入 write-set。
- seal 要求 Semantic admission participant、Driver gateway participant、Resource/Ledger participant 均已存在于同一 OPEN registry；schema v16 新增 immutable Semantic/Resource child tables 与 root columns，旧 schema v0–v15 迁移保留历史事实且 partial schema fail closed。
- `nlos-task` integration tests 覆盖 Semantic event readback、Resource RESERVED readback、owner mismatch、duplicate/read conflict、registry pre-registration、restart replay；workspace Clippy 与 `nlos-task` tests 通过。
- 仍为 PARTIAL PASS：Semantic publication Admission/Durability Receipt、Resource activation/consume/finalize、跨 authority prepare→activate、planned effect/publication、phantom/range serializability 和完整 TaskWriteSet 尚未完成。

### 4.65 sealed TaskWriteSet planned-effect binding（B-TASK-008C1）

- `TaskWriteSetRequest.planned_effects` 现在由 TaskAuthority 在 seal 时校验并持久化到 schema v17 immutable child；父行保存 canonical `effect_set_root`，child 记录有序 effect descriptor、derived identity 和成功/条件/action digests，update/delete 由 immutable triggers 拒绝。
- 带 planned effect 的 sealed write set 使用 v3 write-set root，将 effect root 纳入 canonical extended domain；无 effect 的 v0–v16 历史行继续保留 v1/v2 root domain 与零 effect root，迁移的 parent/child/trigger partial schema fail closed。
- `CommitPermit` 若命中 sealed write-set root，必须逐位匹配 stored planned-effect vector、effect root、task generation 与 write-set root；不同 effect、篡改 child、重启后漂移和 replay substitution 均 typed reject。`nlos-task` participant 11 项、全 crate 测试与 Clippy 通过，详见 [B-TASK-008C1](../evidence/stage-b/b-task-008c1-planned-effect-write-set-binding.md)。
- 仍为 PARTIAL PASS：没有 per-effect operation/driver/channel endpoint 或 reservation linkage，也没有 Semantic/Artifact publication plan、Semantic durability、Resource activation/consume/finalize、跨 authority prepare→activate、phantom/range serializability 或完整 TaskWriteSet；为兼容 B-TASK-002/003，未命中 sealed row 的 legacy planned-effect permit path 仍存在。

### 4.66 planned effect owner endpoint binding（B-TASK-008C2A）

- `TaskWriteSetEffectEndpointRequest` 将 planned slot 的 endpoint 限定为 Artifact head、Semantic admission、Process binding、Driver gateway 或 Resource ledger；TaskAuthority 直接从对应 owner authority 回读 participant ID、generation 和 admission Receipt，caller 不得注入 proof tuple。
- schema v18 新增 immutable `task_write_set_effect_endpoints` child 与 `effect_endpoint_set_root`；endpoint root 纳入 v4 write-set root。迁移保留历史 v1/v2/v3 roots，parent/child/trigger partial schema fail closed。
- seal 要求 endpoint 已存在于同一 OPEN participant registry；permit issuance 重算 endpoint root，并确认所有 endpoint proof 仍在待冻结 registry 中。`nlos-task` participant 11 项、全 crate 测试与 Clippy 通过，详见 [B-TASK-008C2A](../evidence/stage-b/b-task-008c2a-effect-endpoint-binding.md)。
- 仍为 PARTIAL PASS：没有 per-effect Action/Operation/Driver invocation、Channel/Topic、Semantic target/publication receipt、Artifact publication plan/receipt 或 Resource activation/finalize；当前不强制每个 planned effect 至少声明一个 endpoint，legacy 无 sealed row planned-effect path 仍存在。

### 4.67 Artifact proposed-write 与 publication plan binding（B-TASK-008C2B）

- `TaskWriteSetRequest.artifact_writes` 现在由 TaskAuthority 在 seal 时逐项回读 ArtifactAuthority 当前 head，校验 `expected_head_revision`、`proposed_revision = expected + 1`、Artifact head endpoint proof 与 OPEN participant registry membership；声明内容摘要/大小只作为待发布 proposal，不冒充已发布事实。
- schema v19 新增 immutable `task_write_set_artifact_writes` child 与 `artifact_write_set_root`；schema v20 重建历史 Artifact plan parent，解除旧的 `artifact_plan_root = write_set_root` 约束，使 permit-bound TaskWriteSet root 与含 staging identity 的 publication-plan root 可分别持久化。两类 root 都在 load/permit-time 重算校验，child update/delete 继续 fail closed。
- `plan_artifact_commit` 对命中 sealed root 的请求严格比对 ArtifactId、目标 revision、摘要和大小，忽略 permit 后才确定的 staging ID；durable plan 的 `write_set_root` 仍绑定 permit root，ArtifactAuthority staging 使用同一 root。命中 sealed Artifact write 的 effectful permit 可进入 Artifact publication authorization，但 terminal finalize 仍保留 effectful-plan guard。
- 新 participant integration test 覆盖 seal → effectful permit → mismatch reject → matching plan → post-permit stage → authorization；nlos-task 全测试、workspace 全目标测试、fmt 与 Clippy 通过，详见 [B-TASK-008C2B](../evidence/stage-b/b-task-008c2b-artifact-write-publication-binding.md)。
- 仍为 PARTIAL PASS：Semantic publication Admission/Durability receipt、Artifact publication receipt consumption、Resource activation/consume/finalize、per-effect operation/Channel linkage、legacy 无 sealed row 收敛、跨 authority prepare→activate 与 complete TaskWriteSet 尚未完成；下一门为 `B-TASK-008C2C` Semantic append declaration，再后续进入 `B-TASK-008C2D` durable publication/finalization。

### 4.68 Semantic 追加声明与直接耐久收据绑定（B-TASK-008C2C）

- `TaskWriteSetRequest.semantic_appends` 现在由 SemanticAuthority owner readback：逐项校验 event log 存在、caller target scope 与 event envelope scope 逐位一致，并只接受 `required_durability = Durable` 与直接 `AdmissionReceipt.durability = Durable`；receipt ID 由 owner 返回后持久化，caller 不能注入。
- Semantic read/append 均复用 authority endpoint proof 与 OPEN participant pre-registration。schema v21 新增 immutable `task_write_set_semantic_appends` child、`semantic_append_set_root` 与 update/delete triggers；含追加声明的 seal 使用 v6 write-set root，load/replay/permit issuance 重算并校验 child/root，历史无追加声明 rows 保留 v1–v5 root 语义。
- Participant integration test 覆盖 direct durable receipt readback、target mismatch fail-closed、append root、重放/permit root binding；新增 [B-TASK-008C2C](../evidence/stage-b/b-task-008c2c-semantic-append-binding.md)。
- 仍为 PARTIAL PASS：本切片没有消费 `DurabilityReceipt`、semantic outbox acknowledgement 或最终 publication receipt，因此不能把 required Semantic event 视为已 `COMMITTED`；Semantic/Artifact final publication、Resource activation/consume/finalize、per-effect operation/Channel linkage、legacy 无 sealed row 收敛、跨 authority prepare→activate 与 complete TaskWriteSet 尚未完成；下一门拆为 `B-TASK-008C2D` durable publication/finalization 与剩余字段。

### 4.69 Semantic DurabilityReceipt 观察与可选绑定（B-TASK-008C2D）

- `SemanticAuthority::inspect_durability_receipt(event_id, receipt_id)` 新增精确 owner readback；TaskWriteSet append 可声明可选 `durability_receipt_id`，TaskAuthority 必须从 SemanticAuthority 回读匹配 event 的 immutable receipt，不能把 caller ID 当作事实。
- schema v22 为 Semantic append child 增加 nullable durability receipt ID。无该字段的历史 v21 rows 保留 append-root v1；至少一个 append 带 receipt 时切换 append-root v2，并把每项 receipt presence/ID 纳入 permit-bound write-set root。迁移、load、replay、permit issuance 均 fail closed 校验。
- participant test 覆盖 raw durability proof、owner mismatch/缺失拒绝、重放与重启回读；详见 [B-TASK-008C2D](../evidence/stage-b/b-task-008c2d-semantic-durability-observation.md)。
- 仍为 PARTIAL PASS：本切片没有实现 checkpoint producer、Semantic publication transaction/finalize 或把 outbox/receipt 自动推进为 `COMMITTED`；Artifact publication receipt consumption、Resource activation/consume/finalize、per-effect operation/Channel linkage、legacy 无 sealed row 收敛、跨 authority prepare→activate 与 complete TaskWriteSet 仍未完成；下一门为 `B-TASK-008C2E` publication receipt/finalization boundary。

### 4.70 Semantic 收据终结前权威复核（B-TASK-008C2E）

- 新增 `SqliteTaskAuthority::finalize_commit_v3_with_semantic_authority`：Issued permit 按 sealed `write_set_root` 取回 `TaskWriteSet`，在 Task CAS 前由 SemanticAuthority 逐项复核 event scope、`AdmissionReceipt` identity/log sequence/durability，以及已声明的 immutable `DurabilityReceipt` identity；缺失或不一致均 fail closed。
- Closed/Quarantined permit 不重复访问 Semantic owner，继续走既有 terminal replay/tombstone 语义；针对带 Semantic append + DurabilityReceipt 的 participant fixture 已验证 guarded commit 与精确 replay。
- 明确 guard 只复核已有 authority facts，不 ack `semantic_outbox`、不生成 checkpoint/publication receipt、不扩展 `TaskCommitReceipt` 为已发布声明；direct-Durable AdmissionReceipt 与可选 DurabilityReceipt 两条局部路径继续保持。
- 详见 [B-TASK-008C2E](../evidence/stage-b/b-task-008c2e-semantic-finalization-guard.md)。仍为 `PARTIAL_PASS`：下一门是 Semantic publication receipt producer/consumer 与 `TaskCommitReceipt.semantic_publications` 接线，Artifact/Resource/Operation/Channel 及完整 TaskWriteSet 仍未完成。

### 4.71 Semantic admission-policy 声明与 owner 绑定（B-TASK-008C2F）

- `TaskWriteSetSemanticAppendRequest` 现在必须声明 `expected_admission_policy_digest`；seal 逐项回读 Semantic `AdmissionReceipt.authz_policy_digest` 并做 exact match，policy mismatch fail closed。
- schema v23 为 Semantic append child 增加 nullable `admission_policy_digest`。新 seal 使用 append-root v3，将每项 policy presence/bytes 纳入 root；v1/v2 历史行保持 `NULL`，迁移不补造旧 policy 事实。
- load、replay、permit issuance、Semantic-aware finalize 与 participant fixture 均覆盖 root/owner policy 复核；详见 [B-TASK-008C2F](../evidence/stage-b/b-task-008c2f-semantic-admission-policy-binding.md)。
- 仍为 `PARTIAL_PASS`：Semantic checkpoint/publication receipt producer/consumer 与 `TaskCommitReceipt.semantic_publications` 仍需要架构决定，Artifact/Resource/Operation/Channel 及完整 TaskWriteSet 仍未完成。

### 4.72 Semantic admission outbox owner 回读（B-SEMANTIC-003）

- 新增 `SemanticAuthority::inspect_outbox`；按 event owner 回读 outbox transport 状态，并校验 event log、`AdmissionReceipt` 与 outbox 的 `log_seq`/`receipt_id` 逐位一致。
- `acknowledged_at_ms` 只表示 transport observation；该 API 不修改 outbox，也不把 ACK/intent 当作 checkpoint 或 publication proof。详见 [B-SEMANTIC-003](../evidence/stage-b/b-semantic-003-outbox-owner-readback.md)。
- 仍为 `PARTIAL_PASS`：outbox consumer/ACK writer、Semantic checkpoint/publication receipt、跨 authority finalize、`TaskCommitReceipt.semantic_publications` 及完整 TaskWriteSet 仍未完成。

### 4.73 Resource Reservation permit 前 owner 复核（B-TASK-008C2G-RES）

- 新增 `request_commit_permit_with_resource_authority`；对 sealed `TaskWriteSet` 的每个 Reservation 回读 owner 的 `RESERVED` 状态及 account、quote、call、operation、Driver/device、generation/fencing token、upper-bound，并在 participant registry freeze 前 fail closed。
- 错误 Resource authority 不会发放 permit；正确 authority 支持发放与相同请求 replay。该 API 不激活 Reservation、不消费 token，也不生成 Resource publication/finalization receipt。详见 [B-TASK-008C2G-RES](../evidence/stage-b/b-task-008c2g-resource-permit-owner-revalidation.md)。
- 仍为 `PARTIAL_PASS`：Resource activation/consume/finalize、publication receipt、跨 authority prepare→activate/complete、Semantic publication receipt producer/consumer、Artifact/Operation/Channel 及完整 TaskWriteSet 仍未完成；Semantic publication 主线仍需要明确 authority ownership。

### 4.74 Artifact head permit 前 owner 复核（B-TASK-008C2G-ART）

- 新增 `request_commit_permit_with_artifact_authority`；对 sealed `TaskWriteSet` 的每个 Artifact write 回读当前 head revision，要求仍等于 expected head 且 proposed revision 为连续下一版，并在 participant registry freeze 前 fail closed。
- 错误 Artifact authority 不会发放 permit；正确 authority 支持发放与相同请求 replay。该 API 不 stage/publish bytes，也不生成 Artifact publication receipt。详见 [B-TASK-008C2G-ART](../evidence/stage-b/b-task-008c2g-artifact-permit-owner-revalidation.md)。
- 仍为 `PARTIAL_PASS`：Artifact staging/publication receipt consumption、统一 TaskCommitReceipt publication 嵌套、跨 authority prepare→activate/complete、Semantic/Resource/Operation/Channel 及完整 TaskWriteSet 仍未完成。

### 4.75 Process binding permit 前 owner 复核（B-TASK-008C2G-PROCESS）

- 新增 `request_commit_permit_with_process_authority`；对 sealed `TaskWriteSet` 的 Process/AgentInstance/IsolationDomain binding 逐字段回读，并复核 owner endpoint proof 与 TaskAttempt 归属，在 participant registry freeze 前 fail closed。
- 错误 Process authority 不会发放 permit；正确 authority 支持发放与相同请求 replay。该 API 不修改 ProcessAuthority 状态。详见 [B-TASK-008C2G-PROCESS](../evidence/stage-b/b-task-008c2g-process-permit-owner-revalidation.md)。
- 仍为 `PARTIAL_PASS`：Process rotation/跨 authority prepare→activate、Semantic publication receipt、Artifact/Resource/Operation/Channel 及完整 TaskWriteSet 仍未完成。

### 4.76 Resource activation receipt owner 回读（B-RESOURCE-002）

- 新增 `ResourceAuthority::inspect_activation_receipt`；仅允许 ACTIVE Reservation 回读 immutable activation receipt，并逐项校验 Reservation 与 receipt 的 `activation_receipt_id`、`operation_id` 绑定。
- 未知、未激活、缺失 receipt 或绑定不一致均 fail closed；authority 重启后仍回读同一 receipt。详见 [B-RESOURCE-002](../evidence/stage-b/b-resource-002-activation-receipt-readback.md)。
- 仍为 `PARTIAL_PASS`：Task 消费/统一 `TaskCommitReceipt`、Resource CLOSING/finalize/refund、跨 authority prepare→activate、Operation/Channel 与完整 TaskWriteSet 仍未完成；strict consume high-water 见 B-RESOURCE-003。

### 4.77 Resource strict consume high-water（B-RESOURCE-003）

- schema v3 为 Reservation 增加 `usage_high_water_seq`/`usage_high_water`，并新增 immutable `reservation_consumption_receipts`；`consume` 只接受 ACTIVE、当前 Driver fence 和匹配 activation receipt。
- strict reference profile 拒绝零序列、同序列改写、usage 回退和超过 upper bound 的报告；相同报告重试回放原 `ConsumptionReceipt`，重启后可回读。详见 [B-RESOURCE-003](../evidence/stage-b/b-resource-003-consumption-high-water.md)。
- 仍为 `PARTIAL_PASS`：CLOSING/UNCERTAIN、effect-closed finalize/refund/risk ledger、late consume/rebate、多维资源及 TaskCommitReceipt resource/cost receipt 仍未完成；QUARANTINED freeze 已由 B-RESOURCE-004 补齐。

### 4.78 Resource QUARANTINED freeze（B-RESOURCE-004）

- schema v4 为 Reservation 增加 quarantine overlay 与 immutable `reservation_quarantine_receipts`；`quarantine` 在当前 Driver fence 和 activation binding 下原子记录冻结时 high-water，并阻止后续 consume。
- 缺少 endpoint/enforcement gateway 的 `effect_closed + final_usage + final_seq` 证明时，只能进入 QUARANTINED；余额不移动、reason digest 不被提升为 final proof。相同请求重放原 Receipt，冲突绑定/reason、重启回读和 DDL immutable 均有测试。详见 [B-RESOURCE-004](../evidence/stage-b/b-resource-004-quarantine-freeze.md)。
- 仍为 `PARTIAL_PASS`：CLOSING/FINALIZED、endpoint-signed final usage、reconciliation、双重记账 finalize/refund/risk ledger、late rebate、多维资源及 TaskCommitReceipt resource/cost receipt 仍未完成。

### 4.79 Semantic outbox owner ACK writer（B-SEMANTIC-004）

- `SemanticAuthority::acknowledge_outbox` 在 owner 事务内重新校验 Event/AdmissionReceipt/outbox 三元绑定，拒绝 admission 前 ACK、身份漂移和 ACK 时间回退；相同时间回放，更晚时间推进 transport high-water。
- `inspect_outbox` 补上 outbox 自身 `event_id` 一致性检查。ACK writer 不修改 event log、AdmissionReceipt 或 DurabilityReceipt，也不生成 checkpoint/publication proof。详见 [B-SEMANTIC-004](../evidence/stage-b/b-semantic-004-outbox-ack-writer.md)。
- 仍为 `PARTIAL_PASS`：跨进程 consumer 认证/租约、Semantic checkpoint/publication receipt、跨 authority finalize 与 `TaskCommitReceipt.semantic_publications` 仍未完成。

### 4.80 Operation owner endpoint proof/readback（B-OP-FENCE-002）

- `SqliteOperationStore::inspect_endpoint_proof` 先按 `OperationId + Generation` 回读 durable registration row，再派生 owner-bound `TaskParticipantId`、`participant_generation` 与 `admission_receipt_id`，并携带 owner Fiber 与 cancellation scope/generation。
- 旧 generation 或未知 Operation 在 proof 生成前 fail closed；authority 重启后对同一 handle 逐字段回读相同 proof。详见 [B-OP-FENCE-002](../evidence/stage-b/b-op-fence-002-operation-endpoint-proof.md)。
- 仍为 `PARTIAL_PASS`：Operation prepare→activate、跨进程签名/租约/attestation、Operation completion 与 Channel endpoint 仍未完成；TaskWriteSet/participant registry 接线见 `B-TASK-008C2G-OP`。

### 4.81 Operation endpoint 接入 TaskWriteSet 与 participant registry（B-TASK-008C2G-OP）

- schema v24 将 `OperationBinding` 加入 per-effect endpoint kind（1..6）与 participant type（1..8），迁移通过新表复制保留历史行和 immutable triggers；已存在的 v24 表在旧版本回迁测试中可安全识别，不重复重写。
- 新增 Operation owner proof participant registration、Operation-aware TaskWriteSet seal，以及 permit freeze 前的 Operation proof revalidation；proof 必须来自 durable `OperationId + Generation` readback，且 participant 已在 OPEN registry 中。无 Operation authority、旧 generation、错误 proof 或 registry 缺失均 fail closed；相同请求 replay 保持原 durable decision。
- 组合 API 可同时复核 Artifact/Process/Resource/Operation；该切片不启动 Operation、不 dispatch/complete、不实现 Channel，也不消费任何 publication receipt。详见 [B-TASK-008C2G-OP](../evidence/stage-b/b-task-008c2g-operation-endpoint-binding.md)。
- 仍为 `PARTIAL_PASS`：Operation prepare→activate/dispatch、跨进程认证、Channel endpoint、Semantic/Artifact/Resource publication receipt 与统一 `TaskCommitReceipt` 仍未完成。

### 4.82 Semantic publication receipt producer（B-SEMANTIC-005）

- Semantic schema v4 新增 immutable `semantic_publication_receipts`，以唯一 `(TaskId, CommitPermitId, EventId)` 绑定 owner-derived receipt；update/delete 与 partial schema 均 fail closed。
- `SemanticAuthority::publish_semantic_publication` 在同一 owner transaction 重新读取 Event scope、Durable AdmissionReceipt 和可选 DurabilityReceipt；不 ACK outbox、不修改既有 Semantic facts。
- `semantic_checkpoint_after` 是当前 append-only `event_log` 前缀的确定性本地 digest；receipt identity、exact replay、错误 target/receipt 拒绝与 authority restart readback 已覆盖。详见 [B-SEMANTIC-005](../evidence/stage-b/b-semantic-005-publication-receipt-producer.md) 与 [ADR-0006](./adrs/0006-semantic-publication-receipt-owner.md)。
- 仍为 `PARTIAL_PASS`：TaskAuthority consumer 已由 `B-TASK-008C2G-SEM` 接线，单 authority lease 已覆盖 mixed、Semantic-only high-level 与 same-term adoption/reconcile terminal path，并增加新 term 的 local `FROZEN_FOR_TAKEOVER` fence pre-gate、schema v30 immutable local fence receipt/exact local fence-set root、schema v31 lease-bound local assignment baseline、schema v32 pending takeover receipt prefix、schema v33 per-endpoint barrier observation 与 schema v34 canonical exact-fence member manifest；跨 authority prepare/consume/recovery、NLOS principal/attestation、远端 barrier 验证/完成、successor assignment 激活、跨 term adoption、Trust View/vector checkpoint 与多 Cell 仍未完成。

### 4.83 TaskAuthority Semantic publication consumer（B-TASK-008C2G-SEM）

- schema v25 新增 immutable Semantic commit plan/receipt 表；`plan → authorize → partial/READY consume` 只接受 sealed `TaskWriteSet.semantic_appends`，并在消费时重新读取 `SemanticAuthority` owner receipt。
- Semantic-only permit 的完整 receipt set 在一个 TaskAuthority transaction 内关闭 permit、推进 TaskHead/attempt、写入既有 Task receipt，并返回 nested `SemanticTaskCommitReceipt.semantic_publications`；重启后精确回放不重写 receipt。
- v3 required `EffectClosedSuccess` 现在必须提供 slot/Receipt-bound success assertion digest；TaskAuthority 重读闭合 EffectReceipt 并校验 task/permit/slot/logical-effect 与 `success_criteria_digest`，错误摘要 fail closed。
- 新增 `finalize_commit_v3_with_semantic_publications`：复用既有 Effect slot terminal evaluation/history append，并在同一 TaskAuthority transaction 内 CAS `READY → FINALIZED`、关闭 permit、推进 TaskHead/attempt；Effect 未闭合时不写入任何 terminal 子事实。Semantic-only API 对含 Effect slot 的 permit 仍 fail closed。
- 新增 `finalize_semantic_commit_with_authority_lease`：Semantic-only high-level 首次终结对 lease-bound permit 强制校验同一 live authority lease；缺少 lease 或 binding 漂移 fail closed，已 `FINALIZED` 的计划仍可在重启后精确 replay。
- Task group publication-in-flight fence 已同时覆盖 Artifact/Semantic plan；错误 owner binding、checkpoint、target、Admission/Durability receipt 与 immutable child UPDATE 均 fail closed。详见 [B-TASK-008C2G-SEM](../evidence/stage-b/b-task-008c2g-semantic-publication-consumer.md)。
- 仍为 `PARTIAL_PASS`：跨 authority prepare/finalize 的完整故障矩阵、owner publication crash recovery、外部 provider proof/attestation、Trust View/vector checkpoint、多 Cell 与完整 TaskWriteSet 仍未完成。

### 4.84 Semantic publication cross-authority restart coordinator（B-TASK-008C2G-COORD）

- schema v35 修复 barrier observation 的审计缺口：新行持久化 `barrier_receipt_digest` 并在重启后精确读回；旧 schema 行不伪造 digest，仍保持 `NULL`。

- `SemanticCommitCoordinator` 已把 Semantic-only plan 的 `PLANNED → PUBLISHING → READY → FINALIZED` durable prefix 串起来；每个步骤只调用对应 owner authority，TaskAuthority 仍是 plan/receipt 的唯一 durable consumer。
- `inspect_semantic_commit_expectations` 从 sealed `TaskWriteSet` 派生发布声明，`list_incomplete_semantic_commit_plans` 提供稳定 bounded scan；重启后可从 `PUBLISHING`/`READY` 继续，显式 replay 返回同一 nested receipt。
- TaskAuthority 写失败后的 owner publication exact replay 与 prefix 收敛已有 SQLite abort/VFS 局部证据；schema v26 又持久化 mixed finalize envelope，使 coordinator 可在重启后重建 v3 request；EffectClosedSuccess 也已有本地 slot/Receipt-bound proof binding；schema v27 增加单 authority durable lease/term/fencing primitive，schema v28 再把 opt-in binding 接入 CommitPermit 签发、plain v3 finalize、pre-effect close、mixed Effect + Semantic persisted-envelope finalize/replay 与 Semantic-only high-level finalize，schema v29 再把同一 live lease 接入 adoption/reconcile，并增加 local `FROZEN_FOR_TAKEOVER` takeover-fence pre-gate；schema v30 持久化 immutable local fence receipt，并在 durable participant mapping 完整时固定 frozen registry ∪ locally durable outstanding-operation participant 的 exact local roots，映射不完整则保留 NULL；schema v31 为 lease-bound permit 建立 immutable local assignment baseline，schema v32 再把旧 assignment 置 `TakeoverPending` 并持久化 pending takeover receipt prefix，schema v33 再记录绑定 exact local root 的逐 endpoint barrier observation，schema v34 再持久化 canonical exact-fence member manifest，并提供只读 coverage view，但不激活 successor；以测试证明缺少 lease 时 fail closed、binding 持久化/不可变、resolved replay 可重放、新 term fence CAS 只推进一次 control epoch、assignment/receipt/barrier observation/member manifest/coverage view 进入 durable prefix 并在重启后保持。详见 [B-TASK-008C2G-COORD](../evidence/stage-b/b-task-008c2g-semantic-coordinator.md)。仍为 `PARTIAL_PASS`：NLOS principal/attestation、远端 barrier 验证/完成、successor assignment 激活、跨 term adoption、故障矩阵三平台复验、外部 provider proof/attestation、Trust View/vector checkpoint、多 Cell 与完整 TaskWriteSet 仍未完成。

### 4.85 takeover fence 表组故障注入矩阵（B-TASK-008C2G-FAULT）

- schema v27–v35 的 lease/takeover 表组（`task_authority_leases`/`task_authority_lease_history`、`task_authority_takeover_fence_receipts`、`task_authority_assignments`、`task_authority_takeover_receipts`、`task_authority_takeover_barrier_receipts`（v35 digest）、`task_authority_takeover_fence_members`）接入 PoC-0003 对齐的 F1–F4 矩阵，7 项测试全绿，`nlos-task` 零 `src/` 改动。
- F1 kill-9 中断：mid-tx 幻影 fence receipt（真实 registry binding，若存活撞 UNIQUE）/member/assignment/takeover receipt/barrier/term-2 lease history 与 `commit_permits.permit_state` CAS 弄脏全部回滚——takeover 六表无幻影行、lease history 恰好 2 行、assignment 1 行 Active、registry 保持 `FrozenForPermit`、`control_epoch` 不动，重做 fence 成功且重放不二次推进。
- F2 commit 后崩溃：完整 takeover 链（fence receipt + exact roots + member manifest + assignment `TakeoverPending` + pending takeover receipt + v35 barrier observation）逐位保留，fence/barrier 重放返回原结果、observation 不重复。
- F3/F4 IoErr 与 ENOSPC：fence 与 barrier 事务均 typed fail-closed（错误链含 i/o/full）、无半截状态（registry 不冻结、assignment 保持 Active、`control_epoch` 不动），disarm 后同一操作成功。
- F5 静默丢写/撕裂尾部：`PowerLossAfter` 幻影 barrier 重开不可见且重做确定性 receipt id 一致；WAL 截断隐藏整个 fence 事务（registry 回 `FrozenForPermit`、重做 receipt id 一致、`control_epoch` 恰好 +1）或只隐藏 barrier 事务（fence 前缀完整、重做 receipt id 逐位一致）。
- F6 故障解除后：同一 authority 实例从已提交前缀继续，fence + barrier 完整收口、coverage `LocallyCovered`，完整重开可恢复。
- 第二轮增量（v28/v29 lease-binding 写路径，`lease_binding_fault_injection.rs` 7 测试）：lease-bound permit 签发（v28 binding 列 + v31 assignment + registry freeze 同事务）与 lease-bound finalize 在 IoErr 下 typed fail-closed 无半截状态；lease-bound adoption（v29 binding 列 + sequence 推进同事务）在 ENOSPC 下 typed fail-closed 无半截状态；kill-9 中断 mid-tx 幻影 v29 receipt（带 binding 列）与 permit CAS 弄脏全部回滚；commit 后崩溃 lease-bound adoption 逐位保留、重放不重复、v29 binding 列 UPDATE 被 immutable trigger 拒绝；PowerLossAfter 幻影 finalize 重开不可见且重做确定性 receipt id 一致；WAL 撕裂尾部隐藏 finalize 事务且重做收敛。
- takeover 与 lease-binding 两个矩阵文件均已通过三平台 CI（run 31962738904 / 31963113968）。
- 详见 [B-TASK-008C2G-FAULT](../evidence/stage-b/b-task-008c2g-takeover-fault-matrix.md)；限制：kill-9 ≠ 真实断电、macOS 本地、F4 全集（checkpoint/backup/migration 变体）未覆盖、真实 ENOSPC 探针未运行。

### 4.86 Reservation finalize/refund 双重记账结算（B-RESOURCE-005）

- schema v5 为既有 Reservation 增加 `finalize_receipt_id`/`finalized_at_ms` overlay 列（不触碰 v1 `state` CHECK），新增 immutable `reservation_finalize_receipts` 表、唯一索引、binding insert/update 触发器与 overlay-binding immutable 触发器；v1–v4→v5 幂等迁移，partial schema fail-closed。
- `finalize_reservation` 只接受 ACTIVE + 当前 Driver fence 的 Reservation，逐位校验 operation/activation 绑定、时间戳单调与 final usage 的 high-water/upper-bound 约束；同一事务内写入 immutable `FinalizationReceipt`（`refund_credit = upper_bound - final_usage`）、置 `FINALIZED` overlay、并把 refund 记回账户 `available_credit`（双重记账 hold 原子释放）。
- 结算后迟到 `consume`/`quarantine` 显式 `ReservationFinalized`；精确重放逐字节返回原 Receipt，异 proof/usage `IdempotencyConflict`；overlay 绑定一旦设置不可改写/清空；no-effect 结算（final_usage=0）全额退还 hold；重启后逐位回读同一 Receipt。
- 新增只读 `inspect_reservation`（全字段含 terminal overlay）与 `inspect_finalize_receipt`（owner 回读 + 逐位核对）；既有 reserve/activate/consume/quarantine 与 v1–v4 migration 语义保持（15 项 resource 测试全过）。
- 增量：QUARANTINED→FINALIZED reconciliation 解冻——caller 随后提供 effect-closed proof 时，校验 quarantine receipt 与 reservation 的绑定后同一事务清除 QUARANTINED overlay 指针、写 FINALIZED overlay/receipt 并退款（冻结 high-water 为基线）；immutable quarantine receipt 行保留为审计证据（`inspect_quarantine_receipt` 在解冻后返回 `CorruptRecord`，文档化）。
- 增量（故障矩阵）：`nlos-resource` 新增 `open_with_vfs` 并接入 `nlos-store-fault`，finalize 表组对齐 F1–F4 矩阵（kill-9 中断/commit 后崩溃/IoErr/ENOSPC/静默丢写与撕裂尾部/解除后继续，7 测试）；authority 代码零改动。
- 详见 [B-RESOURCE-005](../evidence/stage-b/b-resource-005-finalize-refund.md)；限制：proof digest 为 caller-asserted opaque 摘要、无 endpoint 签名、risk/rebate ledger、late rebate、多维资源与跨 authority resource/cost receipt 及 TaskCommitReceipt resource consumption 接线仍未实现；三平台 CI 已随 run 32099012698 通过。

### 4.87 Takeover barrier observation principal 签名验证（B-TASK-008C2G-BARRIER-SIG）

- `nlos-identity` `KeyPurpose` 新增 `BarrierObservationSigning = 2`（encode/decode 接受 {1,2}，schema CHECK 放宽为 `purpose IN (1,2)`，存量行全为 1 不受影响）；新增 `verify_barrier_observation_signature`，八步验证链（binding/purpose/revocation/validity/InvalidPublicKey/`verify_strict`）与 `verify_semantic_authority_signature` 逐位对齐，零新增错误变体；proof 类型沿私有字段 + const getter 模式。
- `nlos-task` schema v36 为 `task_authority_takeover_barrier_receipts` 增加五个可空 signer 列（principal/control domain/key 各 16B、key generation INTEGER>=1、signature 64B，各带 NULL/长度 CHECK），`signer_coupled` BEFORE INSERT 触发器强制五列同现同缺；v35→v36 幂等迁移，partial schema fail-closed，golden 旧行回读 signer=None 不伪造身份事实。
- 新公开 API `record_authority_takeover_barrier_receipt_signed`：单 `BEGIN IMMEDIATE` 事务内先跑与 unsigned 路径共享的观察核心校验（takeover Pending、exact_fence_set_root、FrozenForTakeover binding、manifest membership + root 复算），再对 `barrier_observation_signature_message`（domain `llmos/takeover-barrier-observation/v1`，覆盖 takeover/participant/remote_receipt/barrier_digest/服务端权威 fence_set_root）做 identity 验签，失败整体回滚零写入；signer 列取 verified proof 值而非 caller 自报；unsigned→signed 或异签名重放 `CorruptRecord` fail-closed；unsigned 路径行为逐位不变。
- coverage 判定、parent takeover receipt 状态、successor assignment 激活语义均不变；跨进程 IPC transport、防重放窗口与真实远端 barrier 语义验证仍是后续切片。详见 [B-TASK-008C2G-BARRIER-SIG](../evidence/stage-b/b-task-008c2g-barrier-principal-signature.md)；限制：签名写路径 kill-9/ENOSPC/torn-WAL 注入矩阵与三平台 CI 未运行，barrier 签名 key 尚无 capability 授权策略约束。


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
  → durable TaskSnapshotReceipt + attempt binding                                          PARTIAL PASS（B-TASK-006O）
  → TaskWriteSet authority-first decision                                                  ACCEPTED（ADR-0005）
  → shared nominal identity spine                                                          PARTIAL PASS（B-TASK-006P）
  → Process/AgentInstance/IsolationDomain durable binding                                  PARTIAL PASS（B-PROCESS-001）
  → Resource/Driver/Reservation durable binding                                            PARTIAL PASS（B-RESOURCE-001）
  → Principal/ControlDomain/signing-key durable authority                                  PARTIAL PASS（B-IDENTITY-001）
  → Capability issue/attenuate/revoke authority                                            PARTIAL PASS（B-CAPABILITY-001）
  → Semantic Assertion target/event admission authority                                    PARTIAL PASS（B-SEMANTIC-001）
  → Semantic canonical IntentSpec body identity                                            PARTIAL PASS（B-SEMANTIC-002A）
  → Semantic SpecEvent durable admission                                                   PARTIAL PASS（B-SEMANTIC-002B）
  → TaskStore self-participant registry                                                    PARTIAL PASS（B-TASK-007A）
  → Artifact/Semantic authority endpoint proofs                                             PARTIAL PASS（B-TASK-007B1）
  → TaskAuthority verified participant registration CAS                                    PARTIAL PASS（B-TASK-007B2）
  → Driver gateway + Resource/Ledger endpoint proofs                                       PARTIAL PASS（B-TASK-007C1）
  → TaskAuthority verified Driver/Resource registration CAS                               PARTIAL PASS（B-TASK-007C2）
  → EffectPermit + Task Receipt participant binding/revalidation                          PARTIAL PASS（B-TASK-007D1）
  → authority-verified snapshot/read-set seal + artifact head readback                      PARTIAL PASS（B-TASK-008A）
  → Process/AgentInstance/IsolationDomain owner binding + endpoint proof                    PARTIAL PASS（B-TASK-008B1）
  → Semantic event + Resource Reservation owner readback                                    PARTIAL PASS（B-TASK-008B2）
  → planned effect durable seal + permit exact binding                                 PARTIAL PASS（B-TASK-008C1）
  → planned effect owner endpoint proof + frozen registry membership                   PARTIAL PASS（B-TASK-008C2A）
  → Artifact proposed-write + publication plan binding                                 PARTIAL PASS（B-TASK-008C2B）
  → Semantic append declaration + direct durable AdmissionReceipt binding              PARTIAL PASS（B-TASK-008C2C）
  → Optional Semantic DurabilityReceipt owner binding                                  PARTIAL PASS（B-TASK-008C2D）
  → Semantic append owner-proof revalidation at Task finalize                         PARTIAL PASS（B-TASK-008C2E）
  → Semantic expected admission-policy owner binding                                  PARTIAL PASS（B-TASK-008C2F）
  → Resource Reservation owner revalidation before permit freeze                       PARTIAL PASS（B-TASK-008C2G-RES）
  → Artifact head owner revalidation before permit freeze                              PARTIAL PASS（B-TASK-008C2G-ART）
  → Process binding owner revalidation before permit freeze                            PARTIAL PASS（B-TASK-008C2G-PROCESS）
  → Resource activation receipt owner readback + restart replay                       PARTIAL PASS（B-RESOURCE-002）
  → Resource strict consume high-water + immutable receipt                           PARTIAL PASS（B-RESOURCE-003）
  → Resource QUARANTINED freeze + immutable receipt                                  PARTIAL PASS（B-RESOURCE-004）
  → Effect-closed 证明下双重记账 finalize/refund 结算 + immutable receipt             PARTIAL PASS（B-RESOURCE-005：QUARANTINED→FINALIZED reconciliation 解冻 + finalize 表组 F1–F4 故障矩阵）
  → Semantic admission outbox owner-consistent transport readback                     PARTIAL PASS（B-SEMANTIC-003）
  → Semantic outbox owner-bound monotonic ACK writer                                  PARTIAL PASS（B-SEMANTIC-004）
  → Operation owner-derived endpoint proof/readback                                   PARTIAL PASS（B-OP-FENCE-002）
  → Operation endpoint TaskWriteSet/participant registry binding                       PARTIAL PASS（B-TASK-008C2G-OP）
  → Semantic publication receipt producer                                             PARTIAL PASS（B-SEMANTIC-005）
  → TaskAuthority Semantic publication consumer + nested TaskCommitReceipt             PARTIAL PASS（B-TASK-008C2G-SEM）
  → Mixed Effect + Semantic unified TaskAuthority finalize hook                         PARTIAL PASS（B-TASK-008C2G-SEM）
  → Semantic-only cross-authority restart coordinator                                   PARTIAL PASS（B-TASK-008C2G-COORD）
  → Mixed Effect envelope + cross-authority recovery                                    PARTIAL PASS（B-TASK-008C2G-COORD）
  → Durable local authority lease/term/fencing primitive                                 PARTIAL PASS（B-TASK-008C2G-COORD，schema v27）
  → Opt-in CommitPermit/terminal lease binding                                          PARTIAL PASS（B-TASK-008C2G-COORD，schema v28）
  → Semantic-only high-level lease path                                          PARTIAL PASS（B-TASK-008C2G-COORD，schema v28）
  → Exact local OS peer credential pre-gate                                      PARTIAL PASS（B-SCHEMA-005 增量）
  → Same-term lease-bound adoption/reconcile guard                                PARTIAL PASS（B-TASK-008C2G-COORD，schema v29）
  → New-term local FROZEN_FOR_TAKEOVER fence + exact local root             PARTIAL PASS（B-TASK-008C2G-COORD，schema v30）
  → Lease-bound local TaskAuthorityAssignment baseline                         PARTIAL PASS（B-TASK-008C2G-COORD，schema v31）
  → Pending local TakeoverReceipt + old assignment fence                         PARTIAL PASS（B-TASK-008C2G-COORD，schema v32）
  → Per-endpoint barrier receipt observation contract                            PARTIAL PASS（B-TASK-008C2G-COORD，schema v33）
  → Canonical exact-fence member manifest                                      PARTIAL PASS（B-TASK-008C2G-COORD，schema v34）
  → Read-only local barrier coverage view                                       PARTIAL PASS（B-TASK-008C2G-COORD，schema v34）
  → Lease/takeover 表组 F1–F4 故障注入矩阵（kill-9 中断/commit 后崩溃/IoErr/ENOSPC/静默丢写/撕裂尾部/解除后继续） PARTIAL PASS（B-TASK-008C2G-FAULT，7 测试，三平台 CI 已过 run 31962738904）
  → v28/v29 lease-binding 写路径故障矩阵（签发/finalize/adoption 三写事务） PARTIAL PASS（B-TASK-008C2G-FAULT 增量，7 测试，三平台 CI 已过 run 31963113968）
  → Barrier observation principal 签名验证（KeyPurpose=2 + schema v36 signed record + coupled signer 列） PARTIAL PASS（B-TASK-008C2G-BARRIER-SIG，7+4 测试，本地全绿）
  → Cross-process IPC transport/防重放 + cross-term adoption + 故障矩阵三平台复验 NEXT（B-TASK-008C2G）
```

议题 31 证据门条 1–7 的 Task/Effect 核心语义至此全部具有至少 H3 级本地证据；TaskGroup membership generation/root 已由 B-TASK-004 实现，WriteSet/CommitPermit/TaskCommitReceipt 的 permit-time 组绑定与漂移围栏已由 B-TASK-005 取得局部 H3 证据，Artifact staged revision 与 Artifact 域内 publication receipt 已由 B-ARTIFACT-002 取得局部 H3 证据。条 8 的本地双 authority 路径已经具备 TaskAuthority publication authorization、Artifact publication nested Receipt、READY-only terminal transaction、重启收敛和逐 plan 故障隔离（B-TASK-006A–006G）；B-TASK-006H–006O 继续证明 recovery lifecycle、durable ledger/alert、typed control/metrics、dual-authority crash recovery 与 TaskSnapshotReceipt；[B-TASK-006P](../evidence/stage-b/b-task-006p-shared-nominal-identity-spine.md) 收敛共享 typed identity，[B-PROCESS-001](../evidence/stage-b/b-process-001-durable-execution-binding-authority.md) 建立 Process/AgentInstance/IsolationDomain generation/fence authority，[B-RESOURCE-001](../evidence/stage-b/b-resource-001-driver-reservation-binding-authority.md) 建立 Driver/Device/Quote/Reservation pre-dispatch binding authority，[B-IDENTITY-001](../evidence/stage-b/b-identity-001-principal-key-authority.md) 建立 Principal/ControlDomain/signing-key validity、撤销与真实 Semantic signature verification，[B-CAPABILITY-001](../evidence/stage-b/b-capability-001-durable-attenuation-authority.md) 建立 issue/delegate/revoke 与 verified-signer reference monitor，[B-SEMANTIC-001](../evidence/stage-b/b-semantic-001-durable-assertion-admission.md) 将前置权威接入 canonical Assertion append，[B-SEMANTIC-002A/002B](../evidence/stage-b/b-semantic-002b-durable-spec-event-admission.md) 冻结 IntentSpec identity 并建立 signed durable SpecEvent admission。[ADR-0005](./adrs/0005-task-write-set-authority-first.md) 的前置 authority/participant registry 链已取得局部 H3 证据；B-TASK-008A 完成 snapshot/read-set seal，B-TASK-008B1 完成 Process owner binding，B-TASK-008B2 完成 Semantic/Resource owner readback，B-TASK-008C1 完成 planned effect 的 durable seal 与 sealed-root permit exact binding，B-TASK-008C2A 完成 owner endpoint proof 与 permit 前 frozen registry membership，B-TASK-008C2B 完成 Artifact proposed-write 与 publication plan 的局部 binding，B-TASK-008C2C 完成 Semantic append declaration 与直接 Durable AdmissionReceipt 的局部 binding，B-TASK-008C2D 完成可选 Semantic DurabilityReceipt owner binding，B-TASK-008C2E 完成终结前 owner-proof re-read guard，B-TASK-008C2F 完成 expected admission-policy owner binding 的局部证据，B-TASK-008C2G-OP 完成 Operation endpoint 的 schema v24、registry 接线与 permit 前 owner 复核，B-TASK-008C2G-SEM 完成 Semantic publication consumer、nested receipt 与本地混合 v3 终结 hook，但 **complete TaskWriteSet 仍未完成**，下一验收门转为跨 authority prepare/finalize coordinator 与 recovery。现有证据仍只覆盖单节点本地 reference authority，不得外推为完整 Identity/Keychain、通用 Capability 系统、五类完整 Semantic store、Trust View、Ledger、真实 Driver enforcement、硬件掉电、分布式原子事务、跨 term 接管或 Slice K 完成。

`B-TASK-008C2G-RES` 已把 Resource Reservation owner readback 推进到 permit freeze 前；它仍是局部安全门，不代表 Resource activation/finalize 或完整 TaskWriteSet 已完成。
`B-TASK-008C2G-ART` 又把 Artifact write head readback 推进到 permit freeze 前；它仍是局部安全门，不代表 Artifact publication receipt consumption 或完整 TaskWriteSet 已完成。
`B-TASK-008C2G-PROCESS` 又把 Process/AgentInstance/IsolationDomain owner binding 与 endpoint proof readback 推进到 permit freeze 前；它仍是局部安全门，不代表跨 authority Process lifecycle 或完整 TaskWriteSet 已完成。
`B-RESOURCE-002` 又为 ACTIVE Reservation 提供 immutable activation receipt 的 owner 回读与重启 replay；它仍是局部安全门，不代表 Task 消费、Resource finalize 或完整 TaskWriteSet 已完成。
`B-SEMANTIC-003` 又为 admission outbox 提供 owner-consistent transport 回读；它仍是局部安全门，不代表 ACK、Semantic checkpoint/publication 或完整 TaskWriteSet 已完成。
`B-SEMANTIC-004` 又为 admission outbox 提供 owner-bound 单调 ACK writer；它仍是 transport observation，不代表 checkpoint/publication 或完整 TaskWriteSet 已完成。
`B-RESOURCE-003` 又为 strict ACTIVE Reservation 提供单调 consume high-water 与 immutable receipt；它仍是局部安全门，不代表 Resource finalize/refund 或完整 TaskWriteSet 已完成。
`B-RESOURCE-004` 又为缺少 effect-closed final usage 证明的 ACTIVE Reservation 提供 QUARANTINED freeze 与 immutable receipt；它仍是保守冻结边界，不代表 Resource finalize/refund 或完整 TaskWriteSet 已完成。
`B-OP-FENCE-002` 又为 durable Operation registration row 提供 owner-derived endpoint proof/readback；它仍是局部 owner 证据，不代表 Operation prepare→activate、TaskWriteSet 接线或 Channel endpoint 已完成。

`B-TASK-008C2G-OP` 又把该 Operation proof 接入 TaskWriteSet per-effect endpoint、participant registry 和 permit 前 owner 复核；它仍不代表 Operation prepare→activate/dispatch、Channel endpoint、publication receipt 或完整 TaskCommitReceipt 已完成。

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
