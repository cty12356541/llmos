# 议题 28：PID 级逻辑 Agent 执行与多层手动调度

> 日期：2026-07-29
> 触发：用户要求确认 NLOS 是否真能在当前 PC 上管理 PID 数量级智能体，并要求除自然语言顶层调度外，提供类似 Windows/macOS 的各层级手动调度。
> 规范落点：[llmos 架构设计总纲 v0.5](../design/06-架构设计总纲-v0.5.md) 第 24.1.1、25.2、25.3、27、28、31、32 节。
> 性质：设计规格化；不代表容量、调度器或桌面控制面已经实现。

---

## 一、问题澄清

“PID 级 Agent”不能解释为：

- 每个 Agent 对应一个 Python/Node 进程；
- 每个 Agent 常驻完整模型 Context；
- 十万或百万 Agent 同时推理；
- 用注册数量冒充并发能力。

NLOS 的目标是：

> 在普通 PC 上持久管理接近 PID 空间量级的逻辑 TaskNode/AgentRole 工作单元，只把当前资源允许的有限工作集物化为 TaskAttempt、Process、AgentInstance、模型调用和工具连接。

因此必须分离：

```text
logical → materialized → runnable → active → resident
```

这与现代 OS 拥有大量进程/线程描述符、但只有少量实体同时占用 CPU 和物理内存的原则一致。

---

## 二、定案 1：TaskPlan 与 TaskNode 是物化前的逻辑实体

自然语言目标和结构化请求先编译为版本化 TaskPlan，而不是直接创建 Agent：

```text
Intent
  → TaskPlan proposal
  → dependency/resource/capability analysis
  → authorization
  → lazy TaskNode eligibility
  → admission
  → materialization
  → TaskAttempt + Process + AgentInstance
```

TaskNode 在未物化时只保留有界 metadata，不得预占：

- OS PID；
- 模型 session；
- Context pin/KV cache；
- 工具连接；
- 文件描述符；
- active concurrency slot。

Planner/Decomposer 是可替换的用户态系统服务，只能提出计划，不能自行扩权、提额或签发副作用许可。用户可以在可信 Task Space 中查看和修改计划；改变目标、权限、成本或不可逆效果时必须重新授权。

---

## 三、定案 2：驻留分级和 Context 工作集

逻辑工作采用以下驻留等级：

```text
METADATA_ONLY → COLD → WARM → HOT → RUNNING
                                  ↘ PINNED
```

- METADATA_ONLY：只有 TaskNode/AgentRole/依赖和资源声明；
- COLD：checkpoint/Artifact 位于持久存储；
- WARM：代码、索引或部分 Context 可快速恢复；
- HOT：Process/AgentInstance 已物化；
- RUNNING：当前占用 CPU/GPU/model/Driver slot；
- PINNED：因有界、可审计的不可迁移资源暂不可回收。

Artifact 是数据本体；Context、KV cache、embedding 和检索结果是可重建派生工作集。pressure 下先回收派生缓存，再 checkpoint/evict，最后才 kill。PINNED 必须有 owner、ResourceAllocation、原因、上界和 expiry，不能成为永久免回收标签。

---

## 四、定案 3：Global → Cell → Worker 分层调度

```text
Global Control Scheduler
  → Tenant/Workspace/Application
    → Cell Scheduler
      → TaskGroup/ResourceGroup queues
        → Worker Scheduler
          → Process/AgentInstance
            → CPU/GPU/model/Driver slots
```

- Global：全局公平、Application/Task 优先级、Cell placement 和 quota lease；
- Cell：本地容量、TaskAuthority、bulkhead、TaskGroup queue；
- Worker：runnable queue、Context affinity、batch 和本地背压；
- Driver/runtime：模型 batch、GPU slot、工具连接和 stream。

正常 spawn/message/dispatch 不能全局扫描全部 Agent、获取全局锁或依赖严格全局总序。下游拥塞必须反向收缩 materialization window，而不是继续创建无限等待 Agent。

---

## 五、定案 4：ScaleProfile 才能支撑容量声称

实现必须分别披露：

- logical nodes；
- materialized instances；
- runnable instances；
- active model/tool operations；
- resident Context bytes；
- metadata bytes/cold node；
- scheduler transition rate；
- recovery scan bound；
- p50/p95/p99 与过载策略。

只有通过对应 benchmark 后，才能声明 `10K_LOGICAL`、`100K_LOGICAL`、`1M_LOGICAL`。单机 PID 同量级逻辑 Agent 是正式目标，但当前仍是 requirement，不是项目已有事实。

---

## 六、定案 5：自然语言和手工控制具有同等系统地位

自然语言是首要界面，不是唯一控制方式。NLOS 必须提供类似 Windows/macOS 的：

- System Control Center；
- Task Manager；
- Resource Monitor；
- Application 设置；
- TaskPlan/依赖图编辑器；
- Process/Agent/Topic/Operation 检查器。

用户可在以下层级手工操作：

| 层级 | 典型操作 |
|---|---|
| System/Device | 性能模式、后台等级、系统保留量、关机/恢复 |
| Workspace/Tenant | quota、并发、默认 QoS、数据与通知策略 |
| Application | launch/terminate、自启动、后台、权限、资源上限 |
| Task | start/pause/resume/cancel/retry/refine、deadline、预算、优先级 |
| TaskGroup/TaskNode | fanout/depth、分支暂停、物化、驱逐、failure/reducer policy |
| TaskAttempt | 查看 snapshot/candidate/effect、cancel/supersede、重新验证 |
| Process/AgentInstance | inspect/suspend/resume/kill/checkpoint/迁移建议/affinity |
| ResourceGroup/Context | allocation、working set、pin、pressure、reclaim/evict |
| Topic/Channel | backlog、subscriber、payer、pause/throttle/drain/close/purge |
| Operation/Device | cancel、safe retry、reconcile、compensate、reset |

手工控制、NL Shell、CLI、GUI 和结构化 API 必须编译为同一 `ControlCommand`，经过同一 Capability、generation CAS、resource/effect fence 和 Receipt。手工操作不能把 `EFFECT_UNKNOWN` 改成成功，不能伪造 Task commit，也不能授予不存在的资源。

---

## 七、对 Windows/macOS 类比的最终修正

| Windows/macOS | NLOS |
|---|---|
| Application | Application |
| Job/process group | TaskGroup + ResourceGroup |
| Durable user job | Task |
| Process/sandbox | Process + IsolationUnit |
| Thread/actor invocation | AgentInstance |
| Process descriptor before running | TaskNode/TaskAttempt metadata |
| Virtual memory/working set | Context residency + derived cache |
| Task Manager/Activity Monitor | NLOS Task Manager/Resource Monitor |
| System Settings | Workspace/Application/System Control policy |
| GUI、PowerShell、CLI、API | Trusted GUI、NL Shell、CLI、structured API |

关键结论：

> NLOS 既必须能让普通用户从顶层用自然语言操作完整应用，也必须允许高级用户沿系统对象树逐层下钻、观察和手工调度。两条路径共享同一权威控制协议，不存在绕过内核不变量的“超级自然语言模式”或“管理员 UI 后门”。

---

## 八、实现与证据状态

当前完成的是规范补全，尚未完成：

- Planner/Decomposer 和 Dependency Resolver；
- TaskPlan 持久化与图编辑器；
- Materialization/Residency Controller；
- Global/Cell/Worker scheduler；
- 10K/100K/1M ScaleProfile benchmark；
- 可信 Task Manager/Resource Monitor；
- 大规模批量控制、断线恢复和逐目标 Receipt。

上述项目已进入 v0.5 已知实现缺口和阶段 B/C/D 退出门。
