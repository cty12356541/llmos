# W34-A：六门证据矩阵与退出门 readiness 评审（review 记录）

> 状态：`REVIEW`（2026-09-21，lane W34-A；评审时 HEAD `a01fd57`（工作区 clean 起步）；本文件为 docs-only 评审记录——不引入新测试/新实现，**未提交**，由控制器集成并组装 W34-D 退出门包）
>
> 任务书对位：[进度单 §6.5.3](../../../management/stage-b-progress.md) W34-A 车道「六门证据矩阵补漏与 Evidence review（B1–B6 全门）——每门 review 记录齐；无缺门」+ §6.5.5 五条件逐一核对的诚实基线。
>
> 既有评审记录（本矩阵引用、不重复展开）：
> - [W31-G](./w31g-road-b004-gates.md)：ROAD-B-004 G1–G6 逐门（评审基 HEAD `14f42ab`）。
> - [W33-H](./w33h-road-b001-b002.md)：ROAD-B-001/002 逐条（评审基 HEAD `14f42ab`）。
>
> 本文件新增评审（评审时点无既有记录的三门，NO-REVIEW-YET → 本文件补齐）：**ROAD-B-003（§3）、ROAD-B-005（§5）、ROAD-B-006（§6）**；并对 W33-H 之后落地的 W32-F（ROAD-B-002 唯一硬缺口的闭合方）做增补评审（§2.2）。
>
> 评审纪律（沿 W31-G/W33-H）：一门仅当**具名测试/证据工件**支撑才可裁 SATISFIED；证据自身登记的边界一律带入裁决；Claim ≤ Evidence、无慷慨膨胀；已登记 residual 一律显式出现在矩阵中，不得因收官波完成而蒸发。裁决取值：SATISFIED / SATISFIED-WITH-BOUNDARIES / PARTIAL-residuals / OPEN。

## 0. 门原文（v0.5 §28.2 逐字誊录，行号对齐 [06-架构设计总纲-v0.5](../../../design/06-架构设计总纲-v0.5.md)）

| 门 | 行 | 原文 |
|---|---|---|
| ROAD-B-001 | 4875 | 一个不了解内核实现的第三方能开发、安装、更新和卸载未预设 Application。 |
| ROAD-B-002 | 4877 | Application 能拥有 Artifact、后台 Task、UI Surface 和多个 Process，而非只能运行单 Agent chat。 |
| ROAD-B-003 | 4879 | 至少完成双 Attempt 唯一提交、cancel/commit 竞态、跨 Task handle 泄漏、snapshot 漂移、共享 provider cache 降级与投机副作用 fence 测试。 |
| ROAD-B-004 | 4881 | 单机实现 MUST 发布至少一个 ScaleProfile，并完成 10K/100K logical TaskNode、不同 active working-set 比例、pressure/reclaim 与 checkpoint/rehydrate benchmark；未完成前不得声称 PID 级 Agent 容量。 |
| ROAD-B-005 | 4883 | 用户 MUST 能从可信 Task Manager 在 Application、Task、TaskGroup/TaskNode、Process/AgentInstance、ExecutionFiber、ResourceGroup 和 Topic/Operation 层执行授权范围内的 inspect/pause/resume/cancel/kill/throttle/reclaim，并证明其与 NL Shell 走同一 ControlCommand/Receipt。 |
| ROAD-B-006 | 4885 | 单机 runtime MUST 证明有限宿主线程可承载至少 100K dormant/waiting Fiber，阻塞 I/O 不线性占用宿主线程；并通过 cancel/late-callback、structured join/detach、Process crash propagation 与分维 Activation metering 测试。未完成前不得声称 coroutine 级大规模并发。 |

---

## 1. ROAD-B-001（第三方 Application 安装/更新/卸载）

**§6 行当前声明**（[进度单 §6 行 1289](../../../management/stage-b-progress.md)）：五命令全生命周期 + 生产活动门 + 兼容窗 + 迁移引擎 + GC 三态 + teardown 链 + W33-A/B/C 第三方公共面；「载荷执行面与 `nlos package install` CLI、slice-k-demo 09d 登记 defect 待处置」。

**覆盖评审**：[W33-H §1–§3](./w33h-road-b001-b002.md)（15 条逐项 + 构造性证据评估 + 抽样复跑：application_migration 11 passed、third_party_sample_lifecycle 1 passed、slice-k-demo STEP 09d panic HEAD 复现）。本文件无增量代码证据，维持该记录裁决。

**裁决提案**：**PARTIAL-residuals**（近收口——W33-H 原判「PARTIAL：距门闭合差波次屏障验证 + 登记 defects 处置」，两阻塞项状态更新如下）。

W34-D 必须称量的 residuals：

1. **三平台 CI 收口未完成**：W33-H 登记的波次屏障——workspace 全仓门已由第九十一增量收口（275 测试二进制 / 1536 passed / 0 failed / 20 ignored；fmt/clippy `--all-targets --all-features` 双 0；schema check-generated 0；desktop 全量构建 0），但三平台 CI/MSRV 由 push `7dc9e57` 触发的链路因后续 docs push 连续取代（run `35531386660` cancelled，被取代非失败），**当前有效载体为 HEAD `a01fd57` 的 run [35531478487](https://github.com/cty12356541/llmos/actions/runs/35531478487)，2026-09-21 评审时 in_progress**（MSRV 1.97 腿已 success；ubuntu/windows/macos 三腿进行中；同 push Pages run `35531478300` success）。CI 绿 + run 链接回填是本门从 PARTIAL 晋升的最后屏障动作。
2. **slice-k-demo STEP 09d 登记 defect 未处置**（`collected_digests==[]` panic；W28-E auto-GC blast radius 嫌疑；[B-SLICE-K-001 §15.3/§16.4](../b-slice-k-001-end-to-end.md)；W33-H §0.1 在 HEAD 复现）——W34-D 裁量：修复或显式退役（B-001 最早的 GC 演示载体；测试面不受影响，W33-B `lifecycle.sh` 为现行构造性演示且绿）。
3. **构造性证明的四具名边界**（W33-H §2，沿引不删）：载荷执行面未做（manifest `executable` 字节不被真实执行）；Os 服务替身为 `sleep` 子进程（非内核托管服务进程）；消费端接线库驱动（`nlos package install` CLI 未做）；模板编译只到 proposal（未接 `apply_plan_revision`）。
4. **deferred minors 移交裁量**（W33-H §3.2）：迁移面 kill-9 矩阵/abandon 命令/跨进程并发注入、跨进程 uninstall 审批、capability revoke、uninstall 不解除 artifact 引用、patch-downgrade 拒绝、GC 跨进程并发 tick 未证明。

## 2. ROAD-B-002（多 Process、后台 Task、UI Surface）

**§6 行当前声明**（行 1290）：后台 Task 登记 + 多 Process binding + inspect 接线 + W29-F spawn→kill 全链 + W30-D teardown/关联下沉 + **W32-F UI Surface 唯一硬缺口闭合**；「Windows 实杀（B6-2 同源）、生命周期 NL 动词、supervisor unregister 登记」。

### 2.1 既有评审

[W33-H §4](./w33h-road-b001-b002.md)：四能力维度中三维度 SATISFIED（Artifact / 后台 Task / 多 Process 纵切面级），**UI Surface 当时 OPEN（全仓无 W32-F commit）**→ 总判「PARTIAL——单缺口（UI Surface）+ 平台/语法登记项」。

### 2.2 增补评审：W32-F 闭合该唯一硬缺口（本文件评审时点已落库）

W33-H 评审（HEAD `14f42ab`）之后，W32-F 以 merge `e5d7115`（实现 commit `f2e53e4`）落地。证据：[B-GUI-001 §W32-F](../b-gui-001-task-manager-shell.md)：

- **声明面**：manifest additive `surfaces` 段（`PackageSurfaceDeclaration` + `validate_surface_declarations` 同款纪律）+ schema v8 `application_surface_registrations`（代际 + manifest digest 内容绑定，陈旧声明 typed `SurfaceManifestMismatch` 拒绝）+ `inspect_surfaces` 读回（`crates/nlos-application/tests/surface_registration.rs` 8 新增；nlos-application 85 passed / 0 failed，既有 g6 golden 零改动）。
- **呈现面**：desktop `surfaces.rs` + `present_surfaces`（本地权威直读，WAL 多进程读安全）；stale 代际不呈现（`stale_generation_and_unknown_package_present_honestly`）+ 未知包 typed `NOT_FOUND`；`surface_presentation_side.rs` 2 项（验签→install→登记→第二句柄呈现全链断言；desktop 25 项全过）。
- **边界（§W32-F.3 四缺口登记，沿引）**：entry 载荷内容渲染未做（渲染声明元数据 + 占位）；表面生命周期状态机/焦点/输入路由/几何未建模；呈现走本地权威直读而非 SystemControl IPC（Surface 域 ControlCommand 不在波次）；登记入口是库 API。

**裁决提案**：**SATISFIED-WITH-BOUNDARIES**（对 W33-H PARTIAL 的唯一增量即 W32-F 闭合单缺口；四能力维度均有具名测试，W32-F 最小链边界如上逐条带入）。

W34-D 必须称量的 residuals：**Windows 实杀**（W33-H §4 行 4 登记，B6-2 同源——Unix 实证 + 非 Unix noop 合同道；见 §6.4 本文件对 run `35511878717` 的精确核对）；生命周期 NL 动词（uninstall/disable application 不在白名单，W30-D 登记）；supervisor 自动 pid 发现/unregister；`restore_process` 复活链、干净退出终态路径、teardown 无并发竞争面（W33-H 行 4/5 沿引）；W32-F 四呈现边界。

## 3. ROAD-B-003（双 Attempt / cancel-commit / handle 泄漏 / snapshot 漂移 / provider cache 降级 / 投机副作用 fence）

**NO-REVIEW-YET → 本节即评审记录**（评审基 HEAD `a01fd57`；未复跑测试，数字系原样誊录自下列证据文件，复现命令见各节）。

门为**测试完成门**（「至少完成……测试」）：六项逐一对照具名测试。

| # | 门条款 | 证据（文件 + 节 + 具名测试 + commit） | 实测结果 |
|---|---|---|---|
| 1 | 双 Attempt 唯一提交 | [B-SLICE-K-001 §7](../b-slice-k-001-end-to-end.md)（W5-D，commit `3500c9c`）+ [B-TASK-001](../b-task-001-task-authority-commit-permit.md)（§4.22 双 Attempt 唯一 CommitPermit） | 两种请求顺序均恰一个 CommitPermit Issued、败者 typed `Superseded` 终态且重试 `InvalidAttemptState` fail-closed、胜者唯一提交 head=1、二次 converge 幂等空 |
| 2 | cancel/commit 竞态 | 同上 §竞争追加（W5-D）：permit-first 线性化（cancel 不清 permit 收敛仍唯一提交）；cancel 先发（双 Attempt 闭 + `CancelledBeforeEffect` 回执重放） | 按 TaskAuthority 实测语义断言，双向竞态终态唯一 |
| 3 | 跨 Task handle 泄漏 | [B-TASK-ROAD-B003-GAPS §2](../b-task-road-b003-gaps.md)（W6-R，commit `b4de16e`，`tests/road_b003_gaps.rs` 4 测试）：`foreign_task_permit_identity_is_rejected_in_all_task_scoped_paths`（T1 permit 在 T2 全部 task-scoped 路径 `PermitNotFound`、T1 仍 Issued、T2 可独立进展）+ `foreign_attempt_identity_cannot_hijack_holder_permit_context`（换 attempt 身份五路径 `AttemptNotFound`，holder check 先于副作用） | 4/4 passed；nlos-task 253 passed / 0 failed（当轮基线） |
| 4 | snapshot 漂移 | 同文件：`stale_effect_history_root_conflicts_fences_and_cannot_revive`（root-only 漂移 → `Conflicted{StaleEffectHistoryRoot}` 终态、新 key 不可复活）+ `stale_snapshot_after_head_advance_conflicts_fences_and_cannot_revive`（head 前进 → `Conflicted{StaleTaskHead}`、新 attempt 绑旧 snapshot 同样 Conflicted） | 同上；head 单调 0→1→2 恰两次提交、stale 流量贡献为零 |
| 5 | 共享 provider cache 降级 | [B-DRIVER-001 §W30-C](../b-driver-001-mock-provider-plane.md)（W30-C，`tests/degradation.rs` 5 项 + `tests/ipc_degradation.rs` 1 项）：`degraded_provider_fails_typed_and_preserves_durable_rows`（三面 typed `Unreachable`、`rpcs_observed` 精确计数、durable 行保持、零 outbox）、`recovery_after_provider_returns_replays_and_converges`、`degradation_window_survives_provider_restart_and_converges`、`shared_cache_degradation_is_visible_to_every_consumer`、`typed_authority_rejection_does_not_degrade_the_cache`；IPC 面 `HostLost`+`RetrySameIdempotencyKey` 有界失败 | `cargo test -p nlos-driver-mock` 23 passed / 0 failed（W30-B 14 项保持绿） |
| 6 | 投机副作用 fence | 同文件 §W30-C（`tests/effect_fence.rs` 3 项）：`stale_generation_speculative_dispatch_is_typed_rejected_without_effect_commit`（陈旧 generation 零 durable 痕迹、正确 generation 恰一次 dispatch）、`cancel_epoch_wins_over_late_speculative_completion`（outbox 仅 `ReconcileEffect` 永不 `WakeFiber`、换 seed `CallbackIdentityConflict`）、`fence_window_restart_commits_exactly_once`（两重启单次提交） | 同上 |

支撑面（超出门条款、B3-1..B3-5 收官项，均有落档）：W26 统一恢复面（[B-TASK-008C2G-UNIFIED-RECOVERY](../b-task-008c2g-unified-recovery.md)，F1–F4 四行全 PASS）；ADR-0017 定案并实现（Resource coordinator schema v43 + 三域 worker，[B-TASK-008C2G-RESOURCE-PREPARE-FINALIZE](../b-task-008c2g-resource-prepare-finalize.md)）；[W30-A 六域混合写集闭环](../b-task-008c2g-complete-writeset.md)（e2e 3/3 + 故障矩阵 9/9，nlos-task 367 passed）；W27-A 三域恢复运维面 IPC/CLI/metrics 全可达。

**裁决提案**：**SATISFIED-WITH-BOUNDARIES**——门六条款全部有具名测试闭合，无一 NOT-SATISFIED。

W34-D 必须称量的 residuals（证据自身登记，沿引）：

1. **provider 面载体是确定性 mock**（B3-4 计划行即「确定性 fake provider + 真实 IPC」——W30-B 按计划交付；真实 driver、生产级 cache 策略（熔断迟滞/探测退避）显式不在切片）；认证 transport 仅 Unix（Windows named-pipe 面未接）；payload codec 未进 `nlos-schema` 冻结 protobuf 通道（ADR-0014 冻结纪律下需显式决策）。
2. **三平台复验（B3-5/W30-E）随当前 CI run**：§6 行「handle 泄漏/snapshot 漂移三平台复验随本增量 CI run」——run `35531478487` in_progress，绿后回填方可收口。
3. **kill-9 = 页缓存存活的进程崩溃建模**；真实断电/跨 Cell/三权威原子性不外推（W30-A §6、W/WE 系列 disclaimer 沿引）。
4. 六域闭环口径：seal/permit 层六域全闭；终结回执层 Artifact 混合终结门缺失（结构性登记，矩阵行 5/6 钉死诚实暴露）+ Channel 面到 endpoint binding 为止（W30-A §3）。

## 4. ROAD-B-004（10K/100K TaskNode、working-set、pressure/reclaim、rehydrate）

**§6 行当前声明**（行 1292）：W28–W31 声明面全链 + W31-G 评审「G1–G6 全 SATISFIED-WITH-BOUNDARIES」。

**覆盖评审**：[W31-G](./w31g-road-b004-gates.md)（G1–G6 逐门证伪条件对照，G1/G3/G4 均有红→绿实证）。本文件仅更新其 G6 边界 1 的状态：workspace 级全量门已由第九十一增量收口（275/1536/0），三平台 CI 由当前 run `35531478487` 承载（in_progress）。

**裁决提案**：**SATISFIED-WITH-BOUNDARIES**（维持 W31-G 六门裁决；G6 边界 1 的 CI 半边待当前 run 绿后解除）。

W34-D 必须称量的 residuals（W31-G §8.2 逐条沿引，不得蒸发）：**release-profile 复测 + 多平台（Linux/Windows）+ CI 化**（全部规模数字为 debug/test 单平台 macOS 口径）；G3 五条件仅依赖+admission 已强制（Namespace/ResourceContract/fanout 仍声明 digest；授权面无权威承载）；G4 生态 selector 半边（Package/Skill/Tool/Model/Artifact/Topic/外部服务）未落；apply 侧 TaskNode 维 admission consult 仍缺（仅物化半边接线）；Task 面 reclaim 与 plan 侧 residency 轴未互连；PINNED tier 未落；**矩阵登记项（无 100K@50% cell、occupancy 只单调填充无回收再入场、调度器自身规模探针未跑）**；故障矩阵外推边界（kill-9 页缓存）；「未完成前不得声称 PID 级 Agent 容量」——生产量级声明以 release/多平台复测为前置。

## 5. ROAD-B-005（多层手动控制 + NL/GUI/CLI 同路）

**NO-REVIEW-YET → 本节即评审记录**（评审基 HEAD `a01fd57`）。

门为**层 × 动作矩阵 + 同路证明**。逐层逐动作对照：

### 5.1 层覆盖（inspect 面）

| 层 | 证据 | 状态 |
|---|---|---|
| Task | `InspectTask`（[B-CONTROL-003 §W17-005](../b-control-003-nl-prefix.md) socket parity；W32-C scoped 家族） | ✓ |
| TaskGroup/TaskNode | W32-G 五 typed Receipt 视图（SABI v1.5 additive，[B-CONTROL-003 §W32-G](../b-control-003-nl-prefix.md)：`cargo test -p nlos-schema -p nlos-system-control` 148 passed + `--features plan,runtime,topic,store` 121 passed） | ✓ |
| Process | `InspectProcess`（§W16-005）；**AgentInstance 无独立层**（未与 Process 分面建模） | ✓/缺 |
| ExecutionFiber | W32-G `EXECUTION_FIBER` 视图（runtime 无 fiber 枚举 API、store 行缺失与 stale generation 不可区分——W32-G/W33-F ③④ 登记） | ✓（带枚举面边界） |
| ResourceGroup | `InspectResource`（§W17-005）+ `InspectResourceHealth`/`ExportResourceMetrics`（W27-A/G8）；粒度为 reservation/账户级，无 ResourceGroup 分组面 | ✓（粒度边界） |
| Topic/Operation | W32-G `TOPIC` + `OPERATION`（DurableOperationSnapshot 8 态）两互补视图 | ✓ |
| **Application** | **无 SystemControl 层 inspect/控制命令**（无 ControlCommand arm；生命周期动词 uninstall/disable application 不在 NL 白名单——W30-D §16.5 登记） | **缺** |

### 5.2 动作覆盖（授权范围内的控制动作）

| 动作 | 命令面/回执 | 真实执行 | 证据 |
|---|---|---|---|
| inspect | ✓ | ✓（只读派发真达权威） | §5.1 全部 |
| pause/resume/cancel | ✓（SABI v1.2 三臂 + typed Receipt tag 8/9/10 + NL 双语 + CLI 子命令） | **执行器未接线**（`UnwiredOperationCommandExecutor` typed NOT_FOUND；宿主执行器递延——W28-D §340、W29-D §385 登记） | [B-CONTROL-003 §W28-D](../b-control-003-nl-prefix.md)（97 passed） |
| kill/throttle/reclaim | ✓（SABI v1.3 三臂 + Receipt tag 11/12/13 + NL/CLI） | ✓ 三条真实 authority 路径：kill→`request_platform_kill` durable 回执先落库再信号 OS；throttle→`ResourceDemand` 权威计算（**不落 durable**，§383 登记）；reclaim→`plan/execute_working_set_reclaim_execution`（`evicted_units` 为合成可重建缓存计数，§384 登记） | [B-CONTROL-003 §W29-D](../b-control-003-nl-prefix.md)（`--features process,resource` 90 passed，`operation_executor_authorities` 9 测试） |
| GUI 授权派发 | ✓（11 动作单一编译点 `build_control_command` + 认证入口；恢复域 ack 真实 CAS mutation） | 夹具上 operation 家族为确定性 typed NOT_FOUND（与 CLI 同形态） | [B-GUI-001 §W32-B](../b-gui-001-task-manager-shell.md)（desktop 14 项全过，`ack_recovery_alert_mutates_authority_and_echoes_receipt_reference`） |

### 5.3 同路证明（与 NL Shell 走同一 ControlCommand/Receipt）

- **W32-C 四路径 parity 钉死**（[B-GUI-001 §W32-C](../b-gui-001-task-manager-shell.md)：`crates/nlos-system-control/tests/triple_path_receipt_parity.rs` 6 项，SABI v1.4 全部 20 命令家族 × direct/NL/CLI/GUI 四路 receipt 逐字节相等；成功/NotFound/Rights 三形态 + mutation 幂等重放同字节；连续 8+ 轮零 flake）。
- NL 句编译结果 == 直接构造命令先断言再派发（受限语法编译器非特权路径，B-CONTROL-003 已实现事实节）。
- 前驱等价门：W28-D `operation_control_commands_are_byte_identical_across_nl_cli_and_direct_paths`、W29-D `kill_throttle_reclaim_commands_...`、desktop 侧认证 vs plain 字节一致集成测试。

### 5.4 可信 Task Manager（GUI 壳）

[B-GUI-001](../b-gui-001-task-manager-shell.md)：Tauri 壳经 ADR-0011 认证入口为唯一 IPC 路径（无明文捷径）；读侧六视图 + 写侧 11 动作 + 权限/预算/成本可见（W32-D 与权威 `inspect_cost_receipt` 逐字段钉死）+ 三域 OpenMetrics 消费（W32-E）+ kill 两步确认；desktop 累计 25 项集成/单元测试全过、`npm run tauri build` EXIT=0。

**裁决提案**：**PARTIAL-residuals**——七层中 Application 层控制面完全缺位、AgentInstance 未独立分面；七动作中 pause/resume/cancel 真实执行器未接线（命令/回执/parity 面完备）；同路证明已由 W32-C 全家族钉死（门的证明条款成立）。

W34-D 必须称量的 residuals：**GUI 真机战役（computer-use 交互清单）未跑**——GUI 证据形态为命令层集成测试 + 窗口注册 + CLI 活体冒烟（会话无 Accessibility/Screen Recording 权限，B-GUI-001 §2 逐波次登记），交互式点按验证是登记在案的收尾动作（§6 行原文「GUI 真机战役（computer-use 清单）……登记」）；Application 层控制面（ControlCommand arm + NL 动词）；pause/resume/cancel 宿主执行器（pause→Process suspend 等）；semantic 域 NL 语法空缺（W32-C §137.1 钉为 typed 拒绝）；throttle 不落 durable / reclaim 合成计数（W29-D §383/384）；desktop 测试不在根 workspace CI（独立 workspace）；Windows 认证入口上游仅 Unix→GUI 无 Windows 面；五层 inspect 的 desktop 命令层接线未做（W33-F ③，渲染器已就绪，今日操作者路径走 CLI）。

## 6. ROAD-B-006（100K dormant Fiber、阻塞隔离、crash propagation、Activation meter）

**NO-REVIEW-YET → 本节即评审记录**（评审基 HEAD `a01fd57`）。

门条款逐一对着具名测试：

| # | 条款 | 证据（文件 + 节 + 具名测试） | 实测结果 |
|---|---|---|---|
| 1 | 有限宿主线程承载 ≥100K dormant/waiting Fiber | [B-RUNTIME-002 §1–§4](../b-runtime-002-fiber-scale.md)：`one_hundred_thousand_durable_wait_fibers_on_two_workers`（`#[ignore]` 探针，真实 durable wait 注册非内存桩） | 100K 探针两次独立运行绿：threads 4→4 恒定、RSS Δ≈196MB（≈2.0KiB/fiber）、唤醒子集精确（99K 未目标 fiber 全员仍 WaitingIo 零误唤醒）、durable 行逐行复核 |
| 2 | 阻塞 I/O 不线性占用宿主线程 | 同文件 §6.7（W15-B，`blocking_io_negative`）：32→256 fiber 线程次线性 + 绝对上界 | 2 passed / 1 ignored（10K 档）；runtime-tokio 75 passed（当轮） |
| 3 | cancel/late-callback 测试 | 同文件 §6（W9-C 功能矩阵：durable wait 挂起中取消、双序竞态终态唯一、取消后重 spawn 代次守卫、late-callback 不复活不 panic）+ §6.16（W27-C `batch_cancel.rs` 6 项：混合 cohort 唯一终态、代次围栏、wake-then-cancel、late callback 缓冲不复活、respawn fence、authority 门 fail-closed 零 runtime 副作用） | 矩阵绿；batch_cancel 6/6 连续 5 轮复跑绿；全 crate 116 passed / 0 failed / 8 ignored |
| 4 | structured join/detach | 同文件 §6.5（W12-J `join_detach` 6 项：`FiberExit` 终态 Condvar、generation fence、implicit detach 回收）+ W25 生命周期回收（第八十八增量 `lifecycle_reap` 18 用例：join 一次性消费、墓碑环、scope 引用计数、orphan 缓冲上界） | runtime 双 crate 定向门 101 passed（W25） |
| 5 | Process crash propagation | [B-PROCESS-003](../b-process-003-crash-propagation.md)：§2 合同层（crash/terminal 标记、reopen 幂等 replay、terminal 后注册/快照/active readback fail-closed、stale fence）+ §5 process 域批量 cancel 传播（schema v4 不可变 receipt）+ §4 runtime terminal 门（`process_crash_propagation` 3 passed）+ W27-C 双域联动（§6.16：durable 拒绝 → runtime 零副作用） | 各节定向门逐轮全绿（22→25→35 passed 演进） |
| 6 | 分维 Activation metering | 同文件 §6.6（W14-M `activation_meter`：`external_wait`/`active_cpu` 边界累计）+ §6.9.3–§6.9.4（10K/100K `--include-ignored` 实跑：100K 1 passed ~13.4s，rss≈203MB）+ §6.10–§6.11（lifecycle backpressure/suspended 10K/100K 探针实跑）+ §6.12（aggregate inspect）+ §6.14（OpenMetrics exposition） | 逐节定向门绿；§6.13 终态竞态已根因修复（`c84c91a`，旧代码 6/6 红→新代码 10/10 绿） |

**裁决提案**：**SATISFIED-WITH-BOUNDARIES**——门六条款全部有具名测试/探针实跑闭合。

W34-D 必须称量的 residuals（证据自身登记，沿引不删）：

1. **单平台 macOS arm64、debug/test profile 数字**——多平台（Linux/Windows）与 release-profile 复测未做（§5 已知限制；100K 探针为 `#[ignore]`，常规 CI 不跑，夜间 scale-probe job 承载）；「coroutine 级大规模并发」的生产量级声明以此为前置。
2. **100K 规模级 cancel/batch-cancel 探针未纳入**（O(n²)/O(n) 终态 purge 特征家族，§5/§6.16.2 登记）；wake fairness 为 `current_thread` 确定性口径（§6.17.2：多 worker 交错与端到端墙钟分布不外推）。
3. **runtime kill receipt 消费与 Activation meter 联动未接线**（§6.16.2「如实保留」）；B6-4 跨平台 supervisor spawn/suspend/kill、B6-5 完整 BirthDecision 登记。
4. **Windows 实杀**：W28-F CI 步骤「Test platform kill contract on Windows」在 run [35511878717](https://github.com/cty12356541/llmos/actions/runs/35511878717) windows-latest 腿 **step 级 success**（含 Test workspace 亦 success）；**该 run 的 windows job 整体 conclusion 为 failure——失败步为后续 Clippy 步**（Windows cfg dead_code，根因已修，见第九十一增量「4 处 CI 根因修复留痕」；run 级 conclusion 勿误判为 kill 步失败——b-process-003 §11 预警的在案实例）。且按 §11 登记边界：即使步骤绿也不含 live-child 实杀测试（`taskkill /F /T` 成功路径无真实子进程断言）——「Windows 真 OS kill 集成测试」仅收窄「CI 复验已跑」半维。当前 HEAD run `35531478487` 重验中。

---

## 7. 横切 X-1..X-6（§6.5.1 横切清单一行态）

| 项 | 当前状态 | 证据 | verdict | 登记 residuals |
|---|---|---|---|---|
| X-1 Notification/Search 最小服务 | W33-D/W33-E 已落 | [B-NOTIFY-001](../b-notify-001-minimal-service.md)（H3：订阅/投递/ack 全经 Topic 权威 + 无影子 fanout 负证）、[B-SEARCH-001](../b-search-001-minimal-service.md)（H3：只读查询 + 零 semantic 写入负证） | SATISFIED-WITH-BOUNDARIES | 最小口径（§6.5.4 决策点 2：薄层/只读）；超出最小面的能力归后续阶段 |
| X-2 最小 Task Space | W33-F 已落（merge `7dc9e57`） | [B-GUI-001 §W33-F](../b-gui-001-task-manager-shell.md)（零新后端命令，复用 Task Manager 读侧；CLI 活体冒烟逐项对上） | SATISFIED-WITH-BOUNDARIES | 五层 inspect 的 desktop 派发接线未做（W33-F.3 ③）；列表=escalated 行+follow，无全量枚举 IPC 面（①）；完整桌面归阶段 D |
| X-3 SDK/debugger/conformance | W29-G 双语言解封复验 + W33-C kit + W33-G debugger 已落 | [B-SDK-GO-001 §8](../b-sdk-go-001-golden-probe.md)（复验零漂移 + ResolveRequest 扩面）、[B-SDK-CSHARP-001 §9](../b-sdk-csharp-001-golden-probe.md)（PASS 16/16 + 4/4 + §9 复验）、[B-ARTIFACT-007 §6](../b-artifact-007-package-sdk.md)（PKG-CONF-001..042 + `nlos-package conformance`，30 测试 + 114 passed）、[B-DEBUG-001](../b-debug-001-minimal-face.md)（三命令只读检查面 + 零 mutation 纪律） | SATISFIED-WITH-BOUNDARIES | TS/Python conformance 未钉 W28-D/W29-D/W32-G 新臂/新视图 golden（三处 Deferred minor 登记）；driver payload codec 未进冻结通道 |
| X-4 机器台账 | evidence-index 143/143 + lint PASS（2026-09-21 实测；本文件落盘并补索引后 144/144） | `python3 scripts/lint_claims.py` → PASS（0 ERROR 0 INFO）；claims 9 行（DONE 3/PARTIAL_PASS 6）、risks 9 行（P1/open 1、P1/partial 6、P2/open 1、P2/partial 1） | PARTIAL（待 W34-B） | claims/risks 六门全量回填与 Claim≤Evidence 复核是 W34-B 车道写集 |
| X-5 三平台 CI/Pages 复验 | HEAD `a01fd57` run [35531478487](https://github.com/cty12356541/llmos/actions/runs/35531478487) **in_progress**（MSRV success；三平台腿进行中）；Pages `35531478300` success；`7dc9e57` 的 run `35531386660` cancelled（被后续 push 取代） | `gh run list/view` 只读采集（2026-09-21）；W26 链补登见 [W27-F](../w27-f-ci-run-links.md) | PENDING | 当前 run 绿 + 链接回填后本项与 B1/B3/B4 的 CI 半边同步收口 |
| X-6 真实掉电分层证据 | 层 2 dm-flakey workflow 已实跑 | [B-STORE-FS-SEMANTICS-001 §8.5](../b-store-fs-semantics-001-apfs-calibration.md)：workflow_dispatch run [33895972272](https://github.com/cty12356541/llmos/actions/runs/33895972272) success（44/44 baseline+power-cut） | PARTIAL（登记态） | M4/M6/M8 模型校准仍无专项数据；真实物理掉电不外推（沿登记） |

## 8. 裁决汇总表（W34-D 组装用）

| 门 | 覆盖评审记录 | 裁决提案 | 主 residuals（W34-D 必须称量） |
|---|---|---|---|
| ROAD-B-001 | W33-H + 本文件 §1 状态更新 | **PARTIAL-residuals**（近收口） | CI run `35531478487` 收口；slice-k-demo 09d defect 处置；四构造性边界；deferred minors 移交裁量 |
| ROAD-B-002 | W33-H + 本文件 §2.2（W32-F 增补） | **SATISFIED-WITH-BOUNDARIES** | Windows 实杀；生命周期 NL 动词；supervisor unregister；W32-F 四呈现边界 |
| ROAD-B-003 | **本文件 §3（新评审）** | **SATISFIED-WITH-BOUNDARIES** | provider 载体为确定性 mock + Unix-only transport + codec 未冻结；三平台复验随当前 CI run；kill-9 页缓存建模；Artifact 混合终结门结构性登记 |
| ROAD-B-004 | W31-G + 本文件 §4 状态更新 | **SATISFIED-WITH-BOUNDARIES** | release-profile/多平台/CI 化复测；G3 三条件 digest；G4 生态 selector；apply 侧 admission consult；矩阵登记项（100K@50% cell 等）；PID 级容量声明前置 |
| ROAD-B-005 | **本文件 §5（新评审）** | **PARTIAL-residuals** | Application 层控制面缺位；AgentInstance 未分面；pause/resume/cancel 执行器；**GUI 真机战役未跑**；semantic NL 语法；desktop 不在根 CI；Windows GUI |
| ROAD-B-006 | **本文件 §6（新评审）** | **SATISFIED-WITH-BOUNDARIES** | 单平台 debug-profile 数字（多平台/release 复测）；100K 级 cancel 探针；kill receipt 消费/meter 联动；B6-4/B6-5；Windows live-child 实杀（step 绿 + run 级 Clippy 失败的精确口径） |

横切：X-1/X-2/X-3 SATISFIED-WITH-BOUNDARIES；X-4 PARTIAL（待 W34-B）；X-5 PENDING（当前 run）；X-6 PARTIAL（登记态）。

## 9. 底线：§6.5.5 五条件 readiness（诚实口径）

| # | 条件 | 状态 | 待什么 |
|---|---|---|---|
| 1 | 六门各有 Evidence review 记录，无一 P0 未决 | **review 半边：本文件交付后 MET**（B-001/002/004 有 W33-H/W31-G + 本文件状态更新；B-003/005/006 由本文件 §3/§5/§6 补齐——「每门 review 记录齐；无缺门」达成）。P0 半边：**PENDING W34-C**（W33-H §3.2 明示 P0 清零确认属 W34-C；当前 risks.yaml 无 P0 条目，但正式确认未做） | W34-C 出未知风险清单 + P0 清零确认 |
| 2 | claims/risks/evidence 全量回填，lint 通过 | **PENDING W34-B**（lint 今日 PASS 143/143、本文件补索引后 144/144——但六门 claims 全量回填是 W34-B 写集；claims 现仅 9 行，六门裁决尚未入账） | W34-B 回填 + lint 复跑全绿 |
| 3 | 未知风险清单显式，各项有 owner 与缓解 | **PENDING W34-C** | W34-C |
| 4 | 管理机制 §7 三件套（Evidence review + 未知风险清单 + 明确批准） | review ✓（三份记录）；清单 PENDING W34-C；**明确批准 PENDING W34-D（唯一保留用户门）** | W34-C → W34-D |
| 5 | 本节车道全 DONE 或批准移交并登记 | W27–W33 车道 DONE（第九十一增量）；W34-A = 本文件（交付、未提交——控制器集成）；**W34-B/C/D 未做**；移交阶段 C 项清单（载荷执行面、`nlos package install` CLI、GUI 真机战役、Windows 实杀、multi-cell/release 复测等）待 W34-D 显式登记 | W34-B/C/D；另：**当前 CI run `35531478487` 收口**是全波次（B-001/B-003/B-004/G6 边界/X-5）共同的最后一道验证 |

**诚实总结论**：阶段 B **今日不可翻门**——五条件中 0 项完全满足（条件 1 的 review 半边随本文件闭环）；阻塞链为 CI run 收口 → W34-B（claims 回填 + lint）→ W34-C（风险清单 + P0 确认）→ W34-D（用户明确批准 + §6 翻转 + 移交登记）。六门中四门（B-002/003/004/006）已达 SATISFIED-WITH-BOUNDARIES、两门（B-001/005）PARTIAL-residuals 且 residual 清单具名可溯源；无 OPEN 门。生产量级声明（PID 级 Agent 容量、coroutine 级大规模并发）按各门原文禁令，以 release-profile/多平台复测为前置，未做前不声称。

## 10. 本评审自身的边界与登记项

1. **docs-only 评审**：除 §0.1 式只读采集（`gh run list/view`、`python3 scripts/lint_claims.py`、`git log`）外未复跑任何测试；数字与测试名系原样誊录自上列证据文件（复现命令在各证据节）。评审基 HEAD `a01fd57`，写集仅本文件 + evidence-index.yaml 一行追加，均未提交（控制器集成，MUST NOT 边界）。
2. CI 状态为评审时点快照（2026-09-21）：run `35531478487` in_progress——W34-D 组装时须以最终 conclusion 复核；`35511878717` 的 job/step 级区分（§6 residual 4）已按 b-process-003 §11 预警口径精确核对。
3. §6 行、§6.5.3 W34-A 行、claims/risks 的更新归控制器/W34-B（本 lane 对这些文件只读）；本文件裁决为**提案**（verdict proposal），晋入 §6 行以控制器集成为准。
4. 进度单 §6 B-006 行「windows leg 首跑绿 run `35511878717`」表述与 run 级 conclusion（failure，Clippy 步）不符——精确口径为「platform kill 步骤 step 级 success、job 级因无关 Clippy 失败」（§6 residual 4）；建议控制器在 §6 行回填时按此口径微调措辞，避免「run 绿」误读。
