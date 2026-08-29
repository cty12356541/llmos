# B-PROCESS-002：Fiber replay 最小前缀（事件溯源续跑）

- 状态：`PARTIAL_PASS`（单机 `H3`）
- 日期：2026-08-29
- Owner：TokioRuntimeAdapter / WaitAuthority
- 设计依据：[ADR-0009](../../management/adrs/0009-fiber-event-sourced-resume.md)（用户 2026-08-29 定案：事件溯源续跑为主 + 受控快照兜底）
- 关联工作包：`B-PROCESS-002`（本切片）；`B-WAIT-001`（durable 事实源）

## 1. 实现事实

- **投影**：`BindingEventProjection::project` 按 binding 从 wait registry 投影事件流（typed、注册时间序、跨 channel 保序；空流合法）。事实源暂为 wait registry 唯一含 binding 列的 authority；effect/queue 投影待各 authority 增 binding 关联列（ADR「不私加 authority」原则，登记后续切片）。
- **契约**：`ResumableBinding::resume(&BindingReplay) -> ResumePlan`——计划只 gate arming；框架在任何 arming（含 self-flip 这一唯一 durable 写）之前 fail-closed 校验（`ResumePlanMismatch`）。
- **`resume_binding`**：gate 镜像 rearm（shutdown/stale/terminal 零副作用）；PENDING 事件复用 `arm_durable_row`（高水位覆盖 → self-notify 自翻转 satisfied；否则同 key 重挂，取代语义与 rearm 一致）；WOKEN → `already_woken` 纯报告（at-least-once 保住，不消费 placeholder）；CANCELLED → 零动作；二次 resume = 幂等重放（前者 Cancelled，durable 行字段级不变）。
- **ADR-0009 第 3 条落地**：重放只覆盖 durable 交互边界，纯内部计算段不进事件流（显式语义损失，复审触发器）；重放消费幂等性由既有 durable 去重承担，replay 设施对 durable 面只读。
- **B 路径占位**：`SnapshotResumable` marker（语义镜像 B-TASK-006O），不接线；快照保留策略留实现切片。
- nlos-wait additive：`list_waits_for_binding`（owner readback、完整行校验、零 durable 副作用）。

## 2. 验证

```text
cargo test -p nlos-runtime-tokio（c5144a8 后）
  → 55 passed / 1 ignored / 0 failed（基线 45+1 无回归，新增 fiber_replay 10 项）
cargo test -p nlos-wait → 26 passed / 0 failed
cargo clippy --workspace --all-targets -- -D warnings → 0 warning / 0 error
cargo fmt --all --check → 通过
```

覆盖：投影正确性/空流、resume 满链路（重挂→notify+deliver→Woken）、已满足自翻转、WOKEN/CANCELLED 分桶、二次 resume 取代 + durable 零副作用、stale/shutdown、rearm 互操作、契约拒绝（re-drive/外部计划）零状态泄漏。

## 3. Canonical commit

- `c5144a8` feat: add fiber replay projection and resume contract

## 4. 明确未完成（PARTIAL_PASS 保持）

- effect/queue 投影（需 authority binding 关联列）；B 快照路径实现与保留策略；fiber 代次与 binding 的 durable 关联；跨进程/跨机 replay（blocked-by B-TASK-006L）。

---

## 5. 增补切片：ADR-0012 登记式投影 + 入口快照 + 代次关联（2026-08-29 接管续建）

- 设计依据：[ADR-0012](../../management/adrs/0012-fiber-projection-registration-and-entry-snapshot.md)（三子决策全部镜像先例：`register_wait` / B-TASK-006O / B-PROCESS-001）；本节关闭上节前三个未决，只余跨进程/跨机 replay。
- 状态：增量实现 PASS（验证门全绿）；canonical commit 待编排者以本工作树原子提交（未提交，不冒充已发布）。
- 接管说明：前代理 30+ 分钟超时中断，其未提交成果（channel v3 / task v41 / process v2 权威增列与登记入口、扩展投影、SnapshotResumable 转正）符合 ADR-0012，全部保留续建；本 Attempt 补齐其残留缺口（见下）。

### 5.1 实现事实

- **channel v3**（`nlos-channel`）：`channel_queue_entries` 增列 `binding_id`/`fiber_generation`（additive，旧行解码为 `None`，从不伪造归属）；不可变 `channel_queue_consumptions` 登记表（trigger-guarded UPDATE/DELETE ABORT）。入口：`enqueue_registered`（生产者身份随 enqueue 同事务写入；零 binding / 身份重绑 fail-closed）、`register_queue_consumption`（消费前登记，`register_wait` 镜像：零 binding → sequence=0 → channel 存在 → 幂等键重放 → entry durable 存在 → 身份唯一，全部门在任一 durable 写之前）、`list_consumptions_for_binding`（投影只读视图，零副作用）。
- **task v41**（`nlos-task`）：`effect_slots` 增列 `fiber_binding`/`fiber_generation`；不可变 `effect_fiber_registrations` 收据表 + by-binding 索引。入口：`register_effect_binding`（slot 列与登记收据单事务提交；门序：零 binding → 幂等重放/重绑冲突 → CommitPermit 持有者（`[TASK-RACE-001]`）→ 登记窗口（仅 `Planned`/`Permitted`）→ 身份 CAS（他者身份或 stale incarnation → `EffectBindingConflict` 零副作用））、`list_effect_registrations_for_binding`（join slot 实时状态与收据 id = effect 完成事实）。
- **扩展投影**（`nlos-runtime-tokio::replay`）：`BindingEventProjection::project` 接受 `ReplayAuthorities`（channel/task/process 三个只读事实源，缺省仅 wait），产出 `BindingReplayEvent::{Wait,Effect,QueueConsumed}` 合一流（注册时间序 + authority tie-break wait→effect→queue）；`ResumePlan`/`resume_binding` 只 arm wait 事件，effect/queue 事件进 `ResumeReport::{effect_events,queue_events}` report-only 桶（框架从不 re-drive）。
- **SnapshotResumable 转正**（`nlos-runtime-tokio::snapshot`）：B 路径 = handler 入口输入快照 + 幂等重执行（B-TASK-006O 语义，中间进度如实丢失）。`snapshot_handler_entry` / `resume_from_snapshot` / `gc_handler_entry_snapshot`；durable 面为 process 权威 `fiber_entry_snapshots`（不新建 authority）。保留策略 latest-only per invocation + 终态 GC，无 TTL/过期窗口；恢复侧 `FiberSnapshotNotFound` 映射为 `ChannelWaitError::SnapshotUnavailable`（fail-closed 指回 A 路径）。
- **代次关联**（`nlos-process` v2）：`register_fiber_incarnation` 复用 B-PROCESS-001 durable generation/fence 权威（CAS + fence；对 process head 的 stale fence fail-closed 零副作用；incarnation 逐次 +1，immutable 行 + CAS head 行）。`resume_binding` 代次门：resumable 声明 `expected_incarnation`+`process_id` 时先校验当前登记，stale incarnation → `StaleFiberIncarnation` 零副作用（与既有 resume gate 同构）。快照写入以 `expected_incarnation_generation` CAS。
- **快照保留语义的关键定案**：新 incarnation 登记不触碰快照槽——latest-only 槽跨 incarnation 共享，崩溃恢复正是「恢复方消费前一个 invocation 的快照」；只有终态 GC 移除它（无第三条消失路径）。
- **登记写窗口故障矩阵**（ADR-0012「由幂等重放收敛，不产生新矩阵」的验收面）：每次登记/快照写 = 单 `Immediate` 事务，durable 状态恰为 `[absent | complete]`；矩阵以 reopen 模拟进程崩溃：absent→redo=`Registered`（fresh）；complete→reopen→同 key redo=`Replayed` 字节相等、异身份 redo=`IdempotencyConflict`/`EffectBindingConflict` 零副作用（task 侧 `effect_binding_write_window_converges_by_idempotent_replay` 显式 W1/W2/W3；channel/process 侧 reopen+replay 用例、runtime 侧快照 crash-window 用例覆盖同构窗口）。
- **公开入口 typed error fail-closed**：`ChannelAuthorityError::InvalidBindingRegistration`、`TaskStoreError::{InvalidFiberBinding,EffectBindingConflict}`、`ProcessAuthorityError::{InvalidFiberBinding,FiberIncarnationNotFound,StaleFiberIncarnation,FiberSnapshotNotFound,InvalidFiberSnapshot}`、`ChannelWaitError::{ChannelAuthority,TaskAuthority,ProcessAuthority,SnapshotUnavailable,StaleFiberIncarnation}`。
- **迁移幂等可重放**：channel v1→2→3、task →41、process 1→2 线性链；各 vN 幂等分支检测部分 schema 即 `CorruptRecord` fail-closed。

### 5.2 验证（全部于本工作树实跑）

```text
cargo test -p nlos-channel -p nlos-process
  → 40 passed / 0 failed（含 channel queue_consumption_registration 4 项、process fiber_incarnation 3 项）
cargo test -p nlos-task
  → 249 passed / 0 failed（含新增 effect_fiber_registration 5 项：登记先于 effect、门 fail-closed、
     身份重绑、写窗口矩阵 W1/W2/W3、投影读随 slot 生命周期）
cargo test -p nlos-runtime-tokio
  → 58 passed / 1 ignored / 0 failed（基线 55+1 无回归；新增 fiber_replay_registration 3 项：
     三 authority 合并投影序、代次门+新事件 report 桶、快照 crash-window 恢复+幂等重执行+终态 GC）
cargo clippy -p nlos-channel -p nlos-task -p nlos-process -p nlos-runtime-tokio
     --all-targets -- -D warnings → 0 warning / 0 error
cargo fmt -p nlos-channel -p nlos-task -p nlos-process -p nlos-runtime-tokio -- --check → 通过
```

- 既有测试零语义改动（机械适配）：`fiber_replay.rs`（`resume_binding`/`project` 新签名 + `events[i].as_wait()` 投影）、`queue_delivery.rs`（v1 回滚脚本补删 v3 对象、user_version 断言 3）。

### 5.3 本 Attempt 修复的前代理残留

- `nlos-task/src/effect.rs`：`register_effect_binding` 的 slot CAS 将 `state_seq`（INTEGER 存储）误传 `encode_u64` BLOB → 恒 0 行命中 `CorruptRecord`；改回 `count_to_i64` 对齐既有 `cas_slot`。
- `nlos-runtime-tokio/src/snapshot.rs`：`RuntimeError` 未导入（编译失败）；`resume_from_snapshot` 未把 `FiberSnapshotNotFound` 接线为 `SnapshotUnavailable`（变体已声明但无产出点）。
- `nlos-process` v41 同族：确认新 incarnation 登记不删除旧快照（超时中断前半截编辑留下的 supersede sweep 痕迹与既有单测意图相悖，按单测+ADR-0012 收敛为「仅终态 GC」）。
- clippy 全量清零（wildcard match、`too_many_lines` allow 属性、冗余闭包、doc backticks）；`cargo fmt` 全量。

### 5.4 明确未完成（增量后）

- 跨进程/跨机 replay（blocked-by B-TASK-006L，与本增量无关）。
- 登记写放大 benchmark（ADR-0012 复审触发器 1）未运行——实现已含登记路径，benchmark 属后续切片。
- 本节验证基于未提交工作树；canonical commit 与 stage-b 进度单同步由编排者执行。

## 5. ADR-0012 剩余前缀实现事实（本切片，DESIGN→IMPLEMENTED）

- **nlos-channel schema v3（幂等迁移）**：`channel_queue_entries` 增 `binding_id`/`fiber_generation` 可空列 + 新增不可变 `channel_queue_consumptions` 登记表（`UNIQUE(channel_id, sequence, binding_id)`、trigger 防改删）。既有行重推导策略对齐先例（nlos-task v5 式）：pre-v3/未登记行如实解码 `None`，不发明事实。生产侧登记入口 `enqueue_registered`（authority 行写入时携带生产方 binding+代次；同 key 重放时登记身份不一致 fail-closed `IdempotencyConflict`）；消费侧登记入口 `register_queue_consumption` 镜像 `register_wait`（框架消费前登记、authority 派生 `registration_id`、entry 必须在场、重放/换 key/换代次全部 fail-closed）；`list_consumptions_for_binding` 纯只读投影读（registration-time 序，channel owner readback 校验）。`EnqueueRequest` 未改动——nlos-topic（他车道）以结构体字面量构造该类型，改为 additive 新入口保持其零改动。
- **nlos-task schema v41（幂等迁移）**：`effect_slots`（effect plane 的 planned-effect 行，write-set 声明行 immutable 且 seal 时无 fiber 身份，故登记列落在本表）增 `fiber_binding`/`fiber_generation` 可空列 + 新增不可变 `effect_fiber_registrations` 回执表（`UNIQUE(permit_id, effect_seq, binding_id)`）。effect 发起登记入口 `register_effect_binding` authority-first：调用方只提供 opaque binding 与自身代次，registration receipt id、slot 关联、logical effect 全部 owner 派生；gate 序 = 零 binding → key 重放（joined slot 状态一并返回）→ `check_holder`（`[TASK-RACE-001]`）→ slot 必须仍 `Planned`/`Permitted`（登记先于 effect 发起）→ slot 已带登记时仅接受完全相同 `(binding, generation)`（幂等重登记），其余——含 stale 代次——`EffectBindingConflict` 零副作用；slot 列回写为受 CAS 保护的独立 UPDATE，不动 `state_seq`（receipt id 派生不受扰）。`list_effect_registrations_for_binding` JOIN slot 当前状态，即「effect 完成」投影读（终态 + receipt id，receipt 本体经既有 `inspect_effect_receipt` 读）。
- **投影扩展（严守 ADR-0009 不新建 authority）**：`BindingEventProjection::project(waits, sources, binding)` 经 `ReplayAuthorities{channel, task, process}` 可选并入三类事实，`BindingReplay.events` 为 `BindingReplayEvent::{Wait, Effect, QueueConsumed}` 合并流（registration-time 序 + authority 确定性 tie-break）；effect 完成/queue 消费事件为 report-only 事实，框架绝不 re-drive。投影 = 三个 authority 只读列表的纯 join（测试断言逐 authority 计数等价 + 跨重启投影相等）。
- **nlos-process schema v2（幂等迁移，借道 B-PROCESS-001）**：新增 `fiber_incarnations`（immutable，`(process_id, binding)` 内 1→prior+1 线性 CHECK）+ `fiber_incarnation_heads`（CAS head）+ `fiber_entry_snapshots`（latest-only 单 slot，digest 完整性回读）。`register_fiber_incarnation` 复用既有 process generation/fence 权威做 CAS gate（stale process fence → `StaleProcessBinding` 零副作用），代次派生 fence 同族机制；`write_fiber_entry_snapshot` 以 `expected_incarnation_generation` CAS 当前登记（stale → `StaleFiberIncarnation` 零副作用）；`gc_fiber_entry_snapshot` 为终态 GC。设计澄清：incarnation 递增**不**清扫 snapshot slot——latest-only slot 跨 incarnation 共享正是 crash-window 恢复（新 incarnation 消费上一 invocation 快照）的前提；只有终态 GC 与下一次覆写删除它。
- **nlos-runtime-tokio 接线**：`SnapshotResumable` 从占位转正（`binding/process_id/expected_incarnation/handler_input/resume_from_entry`）；`snapshot_handler_entry`（latest-only per invocation 覆写，terminal fiber 零副作用返回 `None`）、`resume_from_snapshot`（shutdown→stale handle→terminal no-op→代次 gate→`SnapshotUnavailable`→入口幂等重执行）、`gc_handler_entry_snapshot`（不 gate live——GC 恰是终态所叫）。`resume_binding` 增 `ReplayAuthorities` bundle 与 ADR-0012 代次 gate（`ResumableBinding::expected_incarnation/process_id` 提供即校验，stale → `StaleFiberIncarnation` 在投影前零副作用）；effect/queue 事件进 report-only 桶。既有 `fiber_replay.rs` 10 项测试仅机械适配新签名（`ReplayAuthorities::default()`/`as_wait()`），零语义改动。
- nlos-wait 零改动：`list_waits_for_binding` 既有 API 足以支撑投影（本切片先证明后复用，未动该 crate）。

## 6. 验证（全部运行，2026-08-29，HEAD b0badd5 工作区）

```text
cargo test -p nlos-channel        → 31 passed / 0 failed（含新 queue_consumption_registration 4 项：登记镜像 register_wait 幂等/门、binding 列 None 策略、v3 幂等迁移+半截 schema fail-closed）
cargo test -p nlos-task           → 249 passed / 0 failed（含 effect_fiber_registration：登记先于 effect+重放、holder/窗口/冲突/stale fail-closed、投影读隔离；既有测试仅 user_version 40→41 机械适配）
cargo test -p nlos-process        → 9 passed / 0 failed（含新 fiber_incarnation 3 项：CAS 递增+stale fence、latest-only 快照+stale CAS+GC、重启等价）
cargo test -p nlos-runtime-tokio  → 58 passed / 1 ignored / 0 failed（fiber_replay 10 项机械适配零回归；新 fiber_replay_registration 3 项：三 authority 投影合并/重启等价/直查等价、代次 gate+新事件类型+登记写窗口收敛、B 路径快照 crash-window 恢复+幂等重执行+终态 GC）
cargo clippy -p nlos-channel -p nlos-task -p nlos-process -p nlos-runtime-tokio --all-targets -- -D warnings → 0 error
cargo fmt -p nlos-channel -p nlos-task -p nlos-process -p nlos-runtime-tokio（--check 通过）
```

## 7. 登记写窗口故障矩阵（与既有 kill-window 矩阵同构，全部由幂等重放收敛）

| 写窗口 | durable 状态 | 恢复语义 | 验证 |
|---|---|---|---|
| 消费登记已提交，ack 未执行 | 登记行在场、entry 未消费 | 同 key 重登记 → `Replayed`，投影恰好一条 QueueConsumed；直接查询等价 | `consume_registration_mirrors_register_wait_and_replays` |
| effect 登记已提交，permit 未请求 | slot 仍 `Planned` | resume 重投影（initiated-not-completed 事实），同 key 登记重放 | `resume_binding_gates_incarnation_and_reports_new_events` |
| 快照已提交，handler 未到等待点（「写一半」在 SQLite 单事务 WAL/FULL 下不可持久化为半行；durable 状态 = [absent \| complete]） | 快照行在场 | 新 incarnation 恢复 → handler 入口幂等重执行 → 同 wait key 重登记（恰好一行、字段不变）→ 驱回等待点 | `snapshot_path_restores_crash_window_and_gcs_on_terminal` |
| 快照后写入新 invocation | latest-only 覆写 | 恢复取最新输入 | `entry_snapshot_is_latest_only_with_stale_cas_and_gc` |
| stale incarnation 的任何登记/快照写/恢复 | 零副作用 | authority 级（`EffectBindingConflict`/`IdempotencyConflict`/`StaleFiberIncarnation`）+ runtime 级（`StaleFiberIncarnation` gate）双闸 | 各 crate 对应断言 |

## 8. 已知限制

- 跨进程/跨机 replay 未接（认证前提 ADR-0011 实现线并行中，blocked-by B-TASK-006L 不变）——本切片全部为单机 `H3`。
- 快照仅 handler 粒度：B 路径恢复到 handler 入口，handler 内部进度如实声明丢失（B-TASK-006O 语义镜像）；纯内部计算段仍不进事件流（ADR-0009 显式接受，复审触发器 2）。
- 登记写放大（每次 effect 发起/queue 消费多一次列级 durable 写）为 ADR-0012 显式接受成本；登记 benchmark 不可接受时走复审触发器 1。
- producer binding 列由 additive `enqueue_registered` 写入；经 `enqueue`（含 nlos-topic 路径）入队的 entry 如实解码 `None`——producer 归属对该路径不可推导，不发明。
- 工作区并发说明：本切片与同任务另一 Attempt 共享工作区并行执行，期间发生两次显式合并——其一为 `register_fiber_incarnation` 的 supersede sweep 曾被并行方引入后又移除（该 sweep 破坏 crash-window 恢复前提，以本节 §5 澄清为准，双方测试现一致）；其二为 nlos-task/nlos-runtime-tokio 重复测试文件合并（保留 `effect_fiber_registration.rs`，我的同覆盖文件被并行方收敛删除）。最终 HEAD b0badd5 工作区上全部验证门复跑通过。
- 未运行项：`cargo test --workspace` 及四 crate 之外的任何构建/测试（按任务纪律禁用 --workspace）；nlos-wait/nlos-topic/nlos-artifact 等他车道 crate 未触碰亦未单独验证。
