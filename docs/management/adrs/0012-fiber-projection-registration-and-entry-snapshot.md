# ADR-0012：Fiber replay 采用登记式投影与入口快照兜底

- 状态：ACCEPTED
- 日期：2026-08-29
- Owner：ProcessAuthority / TokioRuntimeAdapter
- 关联 Requirement：总纲 v0.5 行 219（异步执行：callback 绑定 generation/cancel epoch/OperationId）、行 740（Fiber M:N 复用与 WAITING/READY 状态）——同 [ADR-0009](0009-fiber-event-sourced-resume.md) 关联行；本 ADR 是其遗留未决项的定案，不新增 Requirement
- 关联工作包：`B-PROCESS-002`（fiber replay 切片，本决策的直接增补对象）、`B-PROCESS-001`（durable generation/fence 权威）、`B-WAIT-001`（register_wait 先例）、`B-TASK-006O`（task snapshot 先例）、`B-SLICE-K`（消费者：crash recovery 演示场景）
- 决策来源：用户于 2026-08-29 决策会话在三候选（登记式投影+入口快照/推导式投影+周期快照/最小推迟）中选择「登记式+入口快照」
- 复审触发器：登记写放大 benchmark 不可接受；Wasm Process 层与 fiber 层出现统一快照需求（ADR-0009 复审触发器 3）；effect permit 演进出 fiber 粒度绑定、使推导式投影变为可行

## 上下文

[ADR-0009](0009-fiber-event-sourced-resume.md) 已定案「事件溯源续跑为主、受控快照兜底」：fiber 重建 = 按 binding 从既有 authority 投影 durable 事件流，在新 incarnation 中再驱动到等待点。其最小前缀已落地（[B-PROCESS-002](../../evidence/stage-b/b-process-002-fiber-replay.md)，PARTIAL_PASS）：`BindingEventProjection` 仅能从 wait registry 投影 wait 事件（它是当前唯一含 binding 列的 authority），`ResumableBinding`/`resume_binding` 提供 gate 与幂等重放，`SnapshotResumable` 只是 B 路径的语义占位。

Evidence §4 登记了三个未决，全部卡在「durable 事实缺失」这一类问题上：

1. **effect/queue 投影**：EffectPermit 与 queue entry 所在的 authority 行没有 binding 列，投影器无从按 fiber 归并事件流；ADR-0009 明文「不私加 authority」，增列必须先有归属与方式的决策。
2. **B 快照实现与保留策略**：ADR-0009 后果节明文「B 路径的快照保留策略需在实现切片定」，至今悬置。
3. **fiber 代次 durable 关联**：replay 再驱动要求新 incarnation 能对旧 incarnation 的 durable 交互做代次校验，而 fiber 代次与 binding 的 durable 关联不存在。

ADR-0009 后果节为此预留了增补通道：「若实现中发现必须的缺失事实（如 fiber 代次与 binding 的 durable 关联），增补进 `B-PROCESS-002` 切片并在 Evidence 登记，不私加 authority。」本 ADR 即该增补的定案：一次关闭三个未决，且不新建任何 authority、不引入任何新语义概念。

同日 Slice K 纵切面已决策立即全量并行启动，其 crash recovery 演示场景（进程崩溃后 Package→Fiber→Receipt 全链恢复）直接消费 fiber replay 语义——半截恢复语义会立即撞上演示路径，推迟不再是无成本选项。

## 候选

| 候选 | 结论 |
|---|---|
| A. 登记式投影 + 入口快照 | **采纳**：fiber 发起 effect/消费 queue 前经框架登记 binding，authority 行天然可按 fiber 归并；快照取 handler 入口输入、幂等重执行，latest-only 保留 |
| B. 推导式投影 + 周期快照 | 否决：effect permit 绑定 task 粒度、fiber 粒度不可推导（同一 task 的多个 fiber 共用 permit，投影器无法归并归属），语义风险前置到投影层；周期快照引入阈值决策面（多久一次）与快照写入窗口的故障语义新矩阵（写到一半崩溃算什么），两处均无先例可依托 |
| C. 最小推迟（保住 wait 投影，三未决挂起） | 否决：Slice K 纵切面已同日决策立即全量并行启动，其 crash recovery 演示场景会立即撞上半截恢复语义（effect/queue 交互在重放中蒸发、B 路径无兜底），「推迟」实际等于带着已知缺口上线演示 |

## 决定

1. **effect/queue 投影采用登记式**：fiber 发起 effect / 消费 queue 之前，经框架登记 binding——完全镜像 `register_wait` 先例（[ADR-0008](0008-durable-wait-registry-authority.md)：等待登记先于等待发生，authority 行携带等待方身份）；对应 effect/queue authority 行增补 binding 列与 fiber 代次列。投影器保持各既有 authority 的只读视图，严守 ADR-0009「不新建 authority」边界——增列是对既有权威的 additive 扩展，不是新权威。
2. **快照兜底采用 handler 入口快照**：B 路径实现为入口输入快照 + 幂等重执行，语义镜像 [B-TASK-006O](../../evidence/stage-b/b-task-006o-durable-task-snapshot-receipt.md) task snapshot 先例（快照在入口一次成型，恢复回到 handler 入口，中间进度丢失如实声明）。保留策略 latest-only per invocation：每次 invocation 只保留最新一份入口快照，fiber 到达终态即 GC；无 TTL 决策面、无过期窗口——快照要么被本次恢复消费，要么随终态消失。
3. **fiber 代次 durable 关联借道 B-PROCESS-001**：不新建关联对象，fiber incarnation 递增登记复用 [B-PROCESS-001](../../evidence/stage-b/b-process-001-durable-execution-binding-authority.md) 既有 durable generation/fence 权威——fiber 代次与 process/agent binding 的 generation 同族同机制（CAS + fence）；replay 再驱动前按代次校验 binding 归属，stale incarnation 的登记行零副作用（与 `resume_binding` 既有 gate 语义一致）。

## 后果与退出策略

- 三个子决策全部落在已验证先例上：登记式投影镜像 `register_wait`（B-WAIT-001）、入口快照镜像 B-TASK-006O、代次关联镜像 B-PROCESS-001 generation/fence。零新语义概念；故障语义与既有 kill-window 矩阵同构——登记/快照写入的崩溃窗口由幂等重放收敛（同 ADR-0007/0008 先例），不产生新故障矩阵。
- 代价：登记带来写放大——每次 effect 发起/queue 消费多一次列级 durable 写（binding 与代次随行写入，非独立行）。这是显式接受的成本；登记 benchmark 不可接受时走复审触发器 1。
- ADR-0009 悬置的「B 路径快照保留策略需在实现切片定」就此关闭：latest-only per invocation + 终态 GC，无 TTL、无过期窗口、无新决策面。
- 实现归属：authority 增列、B 路径接线、代次登记的落地均属 `B-PROCESS-002` 后续切片，各自在 Evidence 登记验收结果后方可声明完成。本 ADR 为纯设计决策（DESIGN 级），不声称任何实现或测试已发生。
- 退出策略：登记列为 additive schema 扩展、投影器为只读视图，两者均可独立停用（移除列/停读视图）而不触碰既有 authority 行为；入口快照是独立 B 路径，回退即停用接线，A 路径不受影响。若 effect permit 演进出 fiber 粒度绑定、推导式变为可行（复审触发器 3），以新 ADR 取代本 ADR 对应条目，不重写历史。
