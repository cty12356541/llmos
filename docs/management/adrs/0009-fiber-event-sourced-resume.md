# ADR-0009：Fiber 执行状态重建采用事件溯源续跑为主、受控快照兜底

- 状态：ACCEPTED
- 日期：2026-08-29
- Owner：ProcessAuthority / TokioRuntimeAdapter
- 关联 Requirement：总纲 v0.5 行 219（异步执行：callback 绑定 generation/cancel epoch/OperationId）、行 740（Fiber M:N 复用与 WAITING/READY 状态）、`LAYER-SVC-001`
- 关联工作包：`B-PROCESS-002`（fiber replay 最小前缀，新增）；`B-WAIT-001`（durable 事实源）；`B-TASK-006O`（task snapshot 先例）
- 决策来源：用户于 2026-08-29 在讨论 [34](../../discussions/34-fiber执行状态重建设计.md) 四候选中明确选择「A 事件溯源续跑为主 + B 受控快照兜底」
- 复审触发器：跨进程/跨机 replay（blocked-by B-TASK-006L 不变）；非幂等内部计算的 epoch 语义在实现中被迫复杂化；Wasm Process 层（议题 30）与 fiber 层出现统一快照需求

## 上下文

原生 Rust future 的栈不可泛型快照——这是 Rust/Tokio 选型（讨论 30）后 fiber 重建的显式设计债。`B-WAIT-001` 已交付等待侧重建（`rearm_channel_waits`，ADR-0008 补记），但 fiber 执行上下文（局部状态、await 点位）仍是内存事实：进程崩溃后「从等待点继续」不成立。讨论 34 列出四候选并分析利弊；本 ADR 记录定案。

## 候选（详见讨论 34 §2）

| 候选 | 结论 |
|---|---|
| A. 事件溯源续跑（durable 事实重放到等待点，新 incarnation 再驱动） | **采纳，为主路径** |
| B. 受控快照（handler 级输入快照 + 幂等重执行，镜像 B-TASK-006O 到 fiber 粒度） | **采纳，为退化路径**（未改造遗留 fiber 的兜底） |
| C. Wasm 化 fiber（Wasmtime checkpoint/restore） | 否决：与 native 性能路径冲突、引擎快照格式版本耦合；若议题 30 Process 层与 fiber 层未来出现统一快照需求，经复审触发器重开 |
| D. 不做（恢复策略归应用） | 否决：每个应用重造恢复逻辑，「通用 OS」承诺打折 |

## 决定

1. **主路径 A（事件溯源续跑）**：fiber 的外部交互已 durably 事实化（EffectPermit、channel queue entries、wait rows、attribution ledger）；重建 = 按 binding 投影出 durable 事件流，在**新 fiber incarnation** 中把代码重新驱动到等待点，随后复用 wait 侧 rehydration（ADR-0008 补记）重挂等待。框架提供「binding → 事件流重放器 → incarnation 再驱动」设施；fiber 代码须满足可重入再驱动形态（框架约束，写入后续最小前缀的接口契约）。
2. **兜底 B（受控快照）**：对未改造为可重入形态的 fiber，提供 handler 级「输入快照 + 幂等重执行」（语义镜像 B-TASK-006O），恢复到 handler 入口；中间进度丢失如实声明。
3. **exactly-once 边界**：重放消费的幂等性由既有 durable 去重承担（effect idempotency、queue entry key、wait row 唯一性）；**纯内部计算段**不进入事件流——跨内部段的恢复粒度到最近 durable 交互边界，这是本 ADR 显式接受的语义损失（复审触发器 2）。
4. **范围与顺序**：最小前缀只做 A 的重放设施骨架（binding 投影 + 再驱动契约 + 与 rearm 集成）与 B 的快照语义占位；跨进程/跨机 replay 仍 blocked-by B-TASK-006L（真实 Capability/Principal 认证），不变。

## 后果与退出策略

- 新增 durable 面：binding→事件投影不需要新 authority——投影是各既有 authority 的只读视图；若实现中发现必须的缺失事实（如 fiber 代次与 binding 的 durable 关联），增补进 `B-PROCESS-002` 切片并在 Evidence 登记，不私加 authority。
- 代价：fiber 代码形态约束（可重入再驱动）；恢复粒度到 durable 边界；B 路径的快照保留策略需在实现切片定。
- 退出：若事件溯源在实现中被证伪（重放语义被迫复杂化，复审触发器 2），回退到 B 为主，本 ADR 出补记不重写历史；C 的统一快照需求走新 ADR。
