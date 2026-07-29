# ADR-0001：阶段 B 核心语言与异步运行时

- 状态：POC
- 日期：2026-07-29
- Owner：待指定
- 关联 Requirement：`MODEL-EXEC-HIER-001`、`FIBER-MN-001`、`FIBER-AWAIT-001`、`FIBER-CANCEL-001`、`FIBER-PREEMPT-001`、`FIBER-METER-001`、`ROAD-B-006`
- 复审触发器：PoC-0001 未达到规模/取消门槛；Tokio 引入无法封装的调度语义；跨平台或维护成本不可接受

## 上下文

阶段 B 需要实现单机 NLOS 的 Process/AgentInstance/ExecutionFiber/Activation 执行层级，并以有限宿主线程承载大量等待工作。核心 runtime 属于高风险基础选择，但 NLOS 的身份、取消、资源和调度语义不能与具体 async 库绑定。

## 候选

| 候选 | 优点 | 主要风险 |
|---|---|---|
| Rust + Tokio | 生态成熟、跨平台、work stealing、I/O 驱动完整 | Tokio task 语义泄漏；公平性与精确计量需自行补齐 |
| Rust + 自研 executor | 完全控制调度和计量 | 研发、安全与维护成本过高 |
| Go runtime | goroutine 与调度成熟、开发效率高 | 精细宿主控制、嵌入 Wasmtime/系统 ABI 和 TCB 语言统一较弱 |
| C++ + Asio | 系统控制强、生态成熟 | 内存安全和并发缺陷扩大 TCB 风险 |
| Python asyncio | 原型快 | 不适合 Safety TCB 和目标规模的核心基线 |

## 当前决定

采用 **Rust + 可替换 RuntimeAdapter + Tokio PoC**。

这不是最终接受 Tokio 为稳定系统契约。NLOS 对外只暴露自己的 ExecutionFiberId、OperationId、CancellationScope、ResourceGroup 和 Activation；Tokio ID、task handle、channel 和错误不得进入 KABI/SABI、durable state 或 Receipt。

## 约束

- 所有 spawn 必须经过 NLOS runtime facade；
- 所有外部等待必须绑定 OperationId 和 callback fence；
- blocking/CPU-bound 代码不得运行在普通 async worker；
- channel/queue/semaphore 默认有界；
- unsafe 必须集中并单独审计；
- runtime adapter 必须可被 deterministic test executor 替换。

## PoC 验收

目标硬件和环境必须完整记录。

1. 10K、100K waiting Fiber 的 RSS、创建/取消时间和 wake p50/p95/p99；
2. 固定数量宿主线程，不随 waiting Fiber 线性增长；
3. parent cancellation 不遗留未归属 child；
4. late/duplicate callback 不能复活旧 generation；
5. CPU-heavy task 不得无限阻塞 control/cancel path；
6. Activation 能区分 active CPU、runtime queue wait 和 external wait；
7. 运行时重启后，durable identity 与 runtime-local task identity不混淆。

任何数据只证明测试环境内的 ScaleProfile，不直接外推到所有 PC。

## 退出策略

`RuntimeAdapter`、NLOS nominal ID 和 schema crate 不依赖 Tokio。若 PoC 失败，可替换为自研 executor、其他 Rust runtime 或隔离 Process pool，不改变上层 Task/SystemControl/Receipt 契约。

## 当前证据

尚无仓库内 PoC 结果。外部项目说明只能支持候选合理性，不能替代本项目 benchmark。
