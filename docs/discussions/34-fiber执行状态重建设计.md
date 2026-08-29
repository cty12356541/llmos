# 34. Fiber 执行状态重建设计（B-PROCESS 检查点域）

> 状态：讨论中（设计候选，未定案）
> 日期：2026-08-29
> 关联：[ADR-0008](../management/adrs/0008-durable-wait-registry-authority.md)（wait 侧 rehydration 已落地、fiber 执行状态重建显式出界）、[B-PROCESS-001](../evidence/stage-b/b-process-001-durable-execution-binding-authority.md)、[B-TASK-006O](../evidence/stage-b/b-task-006o-durable-task-snapshot-receipt.md)（task 级快照先例）、总纲行 219/740（Fiber WAITING/READY、M:N 复用）

## 1. 问题定义

`B-WAIT-001` 已闭环单机 commit+wakeup：durable wait registry + runtime 内存等待 + rearm 重挂。缺的最后一块是**等待方自身的重建**：进程崩溃后，durable 层知道「binding B 在等 channel C 的 sequence ≥ N」，但 fiber F 的执行上下文（局部变量、调用栈、await 点位）只存在于内存。要让「重启后 fiber 从等待点继续」成立，必须把执行状态本身变成 durable 事实。

原生 Rust future 的栈不可泛型快照——这是与 Erlang（进程堆可复制）、Go（goroutine 栈可复制）的本质差异，也是本项目选 Rust/Tokio（议题 30）后必须显式回答的设计债。

## 2. 候选

| 候选 | 机制 | 优点 | 主要代价 |
|---|---|---|---|
| **A. 事件溯源续跑（event-sourced resume）** | fiber 的每一步外部交互（effect 请求、channel 收发、wait 注册）已经是 durable 事实（EffectPermit、queue entries、wait rows）；重启后按 binding 重放 durable 事件流，在**新的 fiber incarnation** 里把代码重新执行到等待点 | 零语言魔法、与既有 authority-first 架构同构（task reconcile 已是同思路）、durable 事实已被各切片强制 | 要求 fiber 代码写成可重入再驱动形态（框架约束）；「纯内部计算」段无事件、需 epoch 边界 |
| B. 受控快照（snapshot incarnation） | 仅对框架调度的 handler 级 fiber 提供「输入快照 + 幂等重执行」（镜像 B-TASK-006O task snapshot receipt 到 fiber 粒度） | 实现小、与 task snapshot 先例同构 | 只能恢复到 handler 入口，粒度粗；长Running fiber 的中间进度丢失 |
| C. Wasm 化 fiber（进 Wasmtime 沙箱） | fiber 主体编译为 Wasm 组件，用引擎的 checkpoint/restore 能力快照执行态 | 真·任意点快照恢复 | 议题 30 已定 Process 层用 Wasmtime，但把**所有** fiber 拉进 Wasm 与 native 性能路径冲突；引擎快照格式与版本耦合 |
| D. 不做（应用层自行持久化） | 框架只提供 wait registry（已做），恢复策略归应用 | 零框架成本 | 每个应用重造恢复逻辑；「通用 OS」承诺打折 |

## 3. 倾向与未决

**倾向 A 为主、B 为退化路径**：事件溯源续跑与项目 authority-first 哲学一致（durable 事实在 authority、fiber 是事实的消费者），且 wait/effect/queue 三类事实已 durable 化，缺的只是「replay 到等待点」的框架设施（binding → 事件流重放器 → 新 incarnation → 重新 wait_for_channel）。B 作为未改造遗留 fiber 的兜底。

**未决问题**：
1. 事件流的边界：哪些 authority 事实进入 per-binding replay 流（需要 binding 在 effect/queue/wait 各层的投影）；
2. 非幂等内部计算的 epoch 划分与 exactly-once 边界；
3. 与 `B-PROCESS-001` binding generation 的关系（replay 到哪一代）；
4. 跨进程等待（blocked-by B-TASK-006L）落地后 replay 流是否跨机器。

## 4. 结论

不定案。本议题作为 fiber 执行状态重建的 L2 讨论入口；任一候选落地前须先出 ADR（跨模块、难撤销）。当前工程边界：wait 侧 rehydration 已交付（`d3cb9a5`），执行状态重建在 ADR 定案前不进入实现。
