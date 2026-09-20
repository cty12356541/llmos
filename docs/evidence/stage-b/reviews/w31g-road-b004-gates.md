# W31-G：ROAD-B-004 门证据评审记录（G1–G6 逐门）

> 状态：`REVIEW`（2026-09-21，lane W31-G；评审时 HEAD `14f42ab`；本文件为 docs-only 评审记录——不引入新测试/新实现，未提交，由控制器集成）
>
> 评审对象：[ADR-0016](../../../management/adrs/0016-task-plan-declaration-surface.md) 决定 6 所沿用的 [议题 35 §6](../../../discussions/35-TaskPlan声明面设计.md) 验收语义门 G1–G6（证伪条件逐门落测试）。
>
> 证据基（全部只读引用，本评审未复跑任何测试；数字系原样誊录自下列证据文件，复现命令见各节）：
> - [B-PLAN-001](../b-plan-001-declaration-surface.md)：§1–§6（W28-A 状态面，G1）、§7（W29-B Dependency Resolver，G4）、§8（W31-E residency）、§9（W31-D 10K/100K 逻辑 TaskNode benchmark，G2/G5）、§10（W31-A 惰性物化门，G3）、§11（W31-F 分层 Scheduler）
> - [B-PLAN-002](../b-plan-002-manifest-template.md)：全文（W28-B manifest tasks 模板段，G6 模板半边）
> - [B-TASK-SCALE-001](../b-task-scale-001.md)：§3/§3.1（前片已发布数字）、§12（W29-A TaskSpec 关联 + ScaleProfile 维度正规化，G5 前半）、§13（W31-B working-set 比例矩阵）、§14（W31-C reclaim 闭环 + checkpoint/rehydrate 基准）
>
> 评审纪律：一门仅当**具名测试/证据工件**证明其证伪条件不成立才可裁 SATISFIED；证据自身登记的边界一律带入裁决（SATISFIED-WITH-BOUNDARIES）；无慷慨膨胀。裁决取值：SATISFIED / SATISFIED-WITH-BOUNDARIES / NOT-SATISFIED。

## 0. 门定义（议题 35 §6 逐字誊录）

| 门 | 语义 | 规范依据 | 证伪条件 |
|---|---|---|---|
| G1 revision 不可变 | 对已授权/已执行 plan 应用新 revision，旧节点 revision/digest 保持原值 | 行 3648 [PLAN-DAG-001] | 存在改写已执行节点 revision 的路径 |
| G2 惰性有界 | 100K METADATA_ONLY 节点登记后每节点 durable metadata 有上界、RSS 增量有界、零进程/模型会话/连接预占 | 行 4501 [SCALE-LOGICAL-001]、行 4814 [PERF-SCALE-001] | metadata 随节点数超线性，或未物化节点预占执行资源 |
| G3 物化门与窗口 | 仅依赖+授权+Namespace+ResourceContract+fanout gate 全满足的节点进入 MATERIALIZING；窗口收缩停止新物化并 checkpoint/evict | 行 3650 [PLAN-LAZY-001]、行 4503 [SCALE-MATERIALIZE-001] | 未满足依赖的节点可物化，或窗口收缩后仍新增物化 |
| G4 resolver 负路径 | `latest`/搜索结果/未解析 selector 不得被当作已授权依赖 | 行 3652 [PLAN-DEPENDENCY-001] | 存在绕过版本解析直达授权的路径 |
| G5 维度正规化 | ScaleProfile `max_task_nodes` 绑定 TaskNode 持久计数；10K 复跑对齐 B-TASK-SCALE-001 基线量级、100K 档 probe 可跑；Task 注册近似映射退役并在证据中显式注明 | B-TASK-SCALE-001 行 42 | 维度仍以 Task 注册近似，或新旧行为混写不注明 |
| G6 兼容与回归 | 既有 nlos-task 全量测试零回归；若采纳 C，旧签名包仍可验装、新段为 additive golden；若走 SABI，冻结条目 wire 零 diff | B-SCHEMA-015 行 19 硬门 | 任一回归或 wire diff |

## 1. G1 revision 不可变

**证伪条件**：「存在改写已执行节点 revision 的路径」。

**证据**（B-PLAN-001 §2.4 红→绿记录、§3 验证证据；`crates/nlos-plan/tests/g1_revision_immutability.rs`）：

- 红（真实证伪面）：`g1_executed_node_revision_cannot_be_rewritten_by_new_revision` 先写就后，`apply_plan_revision` 对「已执行节点 + 不同形状」的新 revision 静默返回 `Ok(Applied(revision 2))`——证伪条件在公共 API 面实测成立（红）。
- 绿：加 typed 写前拒绝 `FrozenNodeShapeRewrite` 后同测试绿——已执行节点 revision/digest/state 逐位不变、链仍验证、零 durable 损伤。
- `g1_frozen_node_redeclared_bit_identical_keeps_original_revision_and_digest`：同形重声明合法且原行不动；pre-execution 节点可自由改形（revision 为全量声明语义）。
- `g1_storage_triggers_block_raw_rewrites`：裸 SQL 改 frozen 节点 `declared_revision`/`node_digest`、改已提交回执，均 trigger ABORT（第二层）；`plan_revisions` 禁 UPDATE/DELETE + 链衔接 trigger（第三层）。
- 支撑面：`tests/restart_replay.rs`（效果间重启，frozen 节点保持 revision 1、`Replayed` 不推进状态）；`tests/plan_fault_matrix.rs` F1–F4（kill-9/IoErr/静默丢写下已提交前缀逐位保留、redo 恰一次）；`tests/residency.rs::evicted_to_cold_node_preserves_metadata_facts`（evict 不触碰 shape/digest/revision）；`tests/materialization_gate.rs` 过门改造后 G1 断言零弱化（§10.6 清单）。

**裁决**：**SATISFIED-WITH-BOUNDARIES**。

**边界**（证据自身登记，B-PLAN-001 §5/§10.7）：

1. **fence 边界 = 物化边界，非授权边界**：门语义的「已授权」半边（`WAITING_AUTHORIZATION` 及之前）不在冻结集——授权事实属其他权威，按 ADR-0013 在物化门核验；授权回执本身未落本权威（如需落，按新 revision 演进 schema）。
2. **pre-execution 节点同形重声明仍推进 `declared_revision`**（测试钉死的设计语义，非静默分歧）；飞行中迁移以 `StaleNodeRevision` typed 拒绝。
3. `apply_plan_revision` 无调用方 revision CAS（单写者 + `BEGIN IMMEDIATE` 串行化下「最后合法全量声明胜」）；`expected_current_revision` CAS 为登记的 deferred minor。
4. 故障矩阵 disclaimer：kill-9 模拟进程崩溃（页缓存存活）；不外推真实断电/跨 Cell/plan-task-resource 三权威原子性（§4）。

## 2. G2 惰性有界

**证伪条件**：「metadata 随节点数超线性，或未物化节点预占执行资源」。

**证据**（B-PLAN-001 §9 W31-D；`crates/nlos-plan/tests/tasknode_scale_probe.rs`，两档 `#[ignore]` 探针实跑 `--ignored --nocapture`）：

- `ten_thousand_logical_task_nodes_stay_lazy_and_bounded` / `one_hundred_thousand_logical_task_nodes_stay_lazy_and_bounded`：经公共 API `apply_plan_revision` 声明真实 `plan_nodes` 行（逻辑 TaskNode = `plan_nodes` 持久计数，waiting Fiber 不作代理，§9.1 规则 1）。
- 逐证伪对表（§9.4）：每节点 durable 字节 10K=512B(声明)/557B(终态)、100K=508B/553B——两量级逐位一致（线性），远低于 4096B 硬断言上界；全图解析后进程 RSS 增量 ≈46.9MiB@10K / 91.8MiB@100K ≪ 256MiB/768MiB 硬断言上界，零进程/线程/会话预占面；惰性点读 `inspect_node` p95 100K 库 14.167µs vs 同 run 100 节点基线 11.584µs（~1.22x，≤16x 断言限），`inspect_node_residency` p95 10.875µs < 基线 12.791µs。
- evict 对比（G2 姿态）：COLD/WARM/HOT tier 不改变 per-node durable 地板；跨 evict metadata 事实逐位保留 + revision 链仍 verify（断言）。
- 全量套件随跑：`cargo test -p nlos-plan` PASS（§9.5：43 passed / 0 failed / 2 ignored 即两探针）。

**裁决**：**SATISFIED-WITH-BOUNDARIES**。

**边界**（证据自身登记，B-PLAN-001 §9.6）：

1. **debug/test profile、单平台 macOS arm64 数字**：release profile 复测、多平台（Linux/Windows）与 CI 化未做；RSS 读数仅 macOS 可移植（`ps`），其余 target 如实 `None`。
2. 应用形状为合成混合图（链段+hub 叶+独立 root，9,099/90,999 边）：真实 workload 形状（深链/宽扇出矩阵）未扫。
3. `inspect_resolution` 按 id 读回为 O(population)（回执内联 BLOB 解码），分页未做——W29-B §7.6 已登记的写放大同源。
4. checkpoint/rehydrate 与 working-set 比例矩阵数字在姊妹车道（W31-C/W31-B，见 B-TASK-SCALE-001 §13/§14），同为 debug/test 单平台口径。

## 3. G3 物化门与窗口

**证伪条件**：「未满足依赖的节点可物化，或窗口收缩后仍新增物化」。

**证据**（B-PLAN-001 §10 W31-A；`crates/nlos-plan/tests/materialization_gate.rs` 6 passed + `tests/materialization_fault_matrix.rs` F1–F4 + helper 5 passed）：

- 红（系统级，lane 前事实）：W28-A 骨架上裸 `record_node_transition(…→MATERIALIZING)` 对依赖未满足节点直接成功——证伪条件 #1 在改造前的系统上成立（§10.2 红 #2）。
- `g3_unmet_dependency_cannot_materialize_through_any_face`：证伪 #1 三层闭合——门 typed 拒绝（`DependenciesNotReady`，节点落 `BLOCKED_DEPENDENCY`）；伪造 resolution → `MaterializationRequestNotFound`；裸 `WAITING_RESOURCE→MATERIALIZING` → 存储层 trigger `plan_node_transitions_materializing_gated` ABORT。
- `g3_admission_denial_shrinks_window_with_typed_durable_reason`：证伪 #2——真实 Task 消费路径下拒绝不可绕，durable typed 原因（`TaskNodeCapExceeded{…}`）读回逐位相等，节点停 `WAITING_RESOURCE`、窗口 0、拒绝后裸边仍 ABORT、重放 `ReplayedRejected`。
- `g3_working_set_dimension_denies_and_window_stops_growing`：working-set 维 typed `WorkingSetFull` + 窗口 0。
- `g3_window_shrink_composes_with_checkpoint_evict_and_residency_eviction`：收缩响应组合——拒绝后 `ACTIVE→CHECKPOINTED→EVICTED` + residency `HOT→WARM→COLD` 收敛，再入场必走新门轮。
- 崩溃窗口矩阵：`fault_kill9_mid_request_tx_rolls_back_and_real_request_converges`、`fault_kill9_between_request_and_approval_converges_uniquely`、`fault_io_error_on_approval_fails_closed_and_retry_succeeds`、`fault_silent_write_loss_on_approval_redo_resolves_once_and_converges`（verdict 与状态翻转同一事务，「approved-但未翻状态」窗口存储上不可达）。
- 既有测试过门改造断言零弱化（§10.6 清单：G1 三用例断言原样、凭证总数不变）。

**裁决**：**SATISFIED-WITH-BOUNDARIES**。

**边界**（证据自身登记，B-PLAN-001 §10.7「G3 五条件的诚实边界」）：

1. **五条件仅两条件已强制**：依赖就绪 + Task admission（`max_task_nodes`/working-set 两维）已落；**Namespace / ResourceContract / fanout gate 仍是声明 digest**（对应权威未落，W28-A 姿态沿用）——即门语义的前半句在当前系统上由这两条件承载，其余三条件未到可证伪面。
2. **授权面无权威承载**：`WAITING_AUTHORIZATION` 的解除无 enforcement——门接受该态节点进入并按同一 consult 批准（§25.2.1 该边合法），授权 enforcement 属后续车道。
3. approval 真实性边界：admission 真理由 Task 权威所有；持有 plan 库写权限的调用者手工构造 Approved verdict 属同级越权前提（本地单进程无跨权威签名面），与既有权威面信任模型一致。
4. 故障矩阵 verdict 为手工构造（测 plan 侧崩溃窗口）；Task 侧零 durable 写、无崩溃窗口可注入。

## 4. G4 resolver 负路径

**证伪条件**：「存在绕过版本解析直达授权的路径」。

**证据**（B-PLAN-001 §7 W29-B；`crates/nlos-plan/tests/g4_resolution_fencing.rs` 3 + `tests/resolver.rs` 9 + `tests/resolver_restart_replay.rs` 2）：

- 红（真实证伪面）：实现初版 `inspect_resolved_nodes` 从可变 `plan_nodes` 当前行取形状——`g4_resolution_pins_shapes_against_later_revision_reshape` 实测红：revision 1 上 resolve 后 revision 2 重塑未解析节点，旧 resolution 读形状**静默返回 revision 2 digest**（handle 未经版本解析漂移到 latest，即证伪面）。绿：形状读取切到 `plan_revision_nodes` write-once pinned view；同测试绿（旧回执逐位不变、重放 byte-equal、双回执并存可审计）。
- `g4_current_selector_pins_once_and_receipt_never_floats`：Current 只钉一次；头推进后重放由原回执应答（receipt 不浮）。
- `g4_stale_resolution_revision_cannot_drive_transitions_past_fence`：持旧 resolution revision 推进节点被 `StaleNodeRevision` CAS typed 拒绝（任务书「typed stale/fence error or pinned old view」二选一，两半都有测试）。
- 未解析 selector 面：`selector_and_receipt_negatives_fail_typed`（未知 plan/revision/回执 typed）、`missing_revision_shape_fails_typed_never_rederived`（形状行缺失 → `ResolutionShapeUnavailable` fail-closed，绝不从当前行静默重推导）、`tampered_non_cyclic_shape_fails_closed_on_root_verification`（篡改 → `CorruptRecord`，不解析被篡改内容）。
- 幂等/重启：`resolution_idempotency_matrix`、`restart_between_resolver_effects_replays_once_and_converges`、`restart_preserves_pinned_view_across_post_crash_reshape`。

**裁决**：**SATISFIED-WITH-BOUNDARIES**。

**边界**（证据自身登记，B-PLAN-001 §7.6）：

1. **[PLAN-DEPENDENCY-001] 生态半边不在本 lane**：本 lane 的「typed selector」实例化为 **plan revision 选择器**（`Current | At`），handle 即 durable receipt（revision + plan_digest 双锚）；Package/Skill/Tool/Model/Artifact/Topic/外部服务的 typed selector → generation handle 解析未落（其声明现以 `input_selectors_digest` 摘要绑定）——「`latest`/搜索结果不得当已授权依赖」在那些面上的硬门随物化门/后续车道逐面落测试。
2. 无显式 freshness/stale 探测 API（以 pinned view + 既有 CAS 满足门措辞的二选一）。
3. `plan_revision_nodes`/`edges` 允许 raw INSERT（apply 本身走 INSERT）；post-commit 注入由 resolve 时 root 复核拒绝，非 INSERT 期阻断。

## 5. G5 维度正规化

**证伪条件**：「维度仍以 Task 注册近似，或新旧行为混写不注明」。

**证据**（B-TASK-SCALE-001 §12 W29-A + B-PLAN-001 §9 W31-D）：

- **维度重绑**（§12 已实现事实 2）：`ScaleProfile` 新增 `max_task_registrations` 第二显式维度（两档常量补齐）；`register_task` admission 切换为 `enforce_task_registration_admission` → `TaskRegistrationAdmissionDenied`（replay bypass 不变）；`max_task_nodes` 自此正规化为声明 TaskNode（`plan_nodes` 持久计数）维度。口径切换在 code（scale.rs/pressure.rs/probe doc）与 §12 显式声明，**新旧行为不混写**。
- **10K 复跑对齐基线量级**（§12 probe 重跑，原样誊录）：注册维度 10K 复跑 permit p95_10k = 492.625µs，与已发布 §3 数字（391.291µs）同量级（同 run 基线 549.917µs 的 ~0.9x，绝对面 <100ms 限）；10K 注册 2.078s。
- **100K 档 probe 可跑**（§12）：注册维度 100K probe **首次实跑** PASS（补 §4 缺口 #3 / §3.1 待实跑项）：100_000 注册 28.34s，permit p95_100k = 393.958µs ≤ 同 run 100 基线 553.417µs（~0.71x）；5_120 活跃工作集 2.275s。逻辑 TaskNode 维 100K 档由 W31-D 承载（`one_hundred_thousand_logical_task_nodes_stay_lazy_and_bounded`，§9.3 数字），两维度数字分列于 §12 与 §9、互不混写（§9.1 规则 2）。
- **近似映射退役注明**：B-PLAN-001 §9.1 规则 2 显式注明「Task 注册近似退役」（G5 条款原文），并回指 B-TASK-SCALE-001 §3/§3.1/§12。
- 维度语义接线面：TaskNode 维 consult 在物化门真实消费（`g3_admission_denial_shrinks_window_with_typed_durable_reason` 以 `G3_NODE_CAP_ONE` 档实测 `TaskNodeCapExceeded`；W31-A §10.3 consult 口径注记）。

**裁决**：**SATISFIED-WITH-BOUNDARIES**。

**边界**（证据自身登记）：

1. **声明面（apply 时）的 TaskNode 维 admission consult 仍缺**（W29-A §12 仍属缺口 #1 / W30-D 接线位；B-PLAN-001 §9.6 复述「本 benchmark 钉死逻辑计数与上界事实，不声称 admission 已强制」）——当前强制点仅在物化边界（W31-A 半边）。
2. **debug/test profile 单平台数字**（§12 要点 3、§9.6、§14 要点 4 同口径）：release 复测与多平台未做，证据原文明确「不据此宣称 G2/G5 正式达成」的正式面以本评审口径收束为「带边界满足」。
3. 关联字段仅存引用（决定 3 的物化/permit 边界 ADR-0013 核验未接线，W29-C/W30-D；§12 仍属缺口 #2）——不属 G5 证伪面，如实带入。

## 6. G6 兼容与回归

**证伪条件**：「任一回归或 wire diff」。

**证据**：

- **旧签名包仍可验装（负路径硬门）**（B-PLAN-002 §3，nlos-application 集成）：`g6_old_shape_package_verifies_and_installs_byte_identically`——无段旧包经 legacy 面验签（receipt 摘要 == `package_manifest_message` 逐字节）→ 安装 → 同键 replay 逐字节相等、代际不双跳；`templated_package_installs_through_the_same_receipt_path`（templated receipt 经 `install_application` 零改动消费）；`verify_with_tasks_fails_closed_on_segment_tamper_and_strip`（剥段/注段/改模板三向 tamper 全 `PackageSignatureInvalid` 且零 durable 行）；六类 malformed 段 typed `PackageManifestInvalid`。
- **新段 additive golden**（B-PLAN-002 §2–§3）：`templated_message_framing_is_canonical`（域分离/链式复用 legacy 摘要/每字段参与）；legacy `verify_package` 与 `PackageManifest`/`SignedPackage` 类型零改动（方法体抽共享核逐语句保持原序）；既有 golden 零改动零回归（`package_signature.rs` 7 用例、`manifest_message_framing_is_canonical`、application_authority 43 + fault 7 全绿原样）。additive 落为**平行签名面** `SignedPackageWithTasks`（不内嵌第二枚 legacy 签名，杜绝静默丢段降级面）——实现选择及理由（slice-k 穷举 struct literal + rustc default field values 未稳定）登记于 §2.1。
- **nlos-task 全量零回归**（沿 ADR 时间线的具名运行）：W29-A §12 验证门 347 passed / 0 failed（基线 W28-C 340 全保持绿）；W31-A §10.8 `cargo test -p nlos-task` 355 passed / 0 failed；W31-B §13 369 passed / 0 failed / 4 ignored；W31-C §14 371 passed / 0 failed / 4 ignored。nlos-plan 侧 17（W28-A）→31（W29-B）→41（W31-E）→43（W31-D）→54（W31-A）→60（W31-F）passed / 0 failed 递增全绿；nlos-artifact 72/0、nlos-application 60/0（W28-B §3）；`cargo check -p nlos-slice-k` PASS（平行面零跨界破坏）。
- **PLAN-OVERRIDE-001 编译等价**（同 schema 纪律的模板半边）：`plan_override_001_templated_segment_compiles_to_the_direct_plan_proposal`——三模板段编译产物与手工直写 `ApplyPlanRevisionRequest` 结构相等（逐字节）+ canonical 摘要相等；kind 全映射 match（plan 侧加变体即编译失败，方言漂移=构建失败）。
- **SABI 条款**：不适用（by scope）——声明面/模板面车道均未触碰 SABI 通道（B-PLAN-001 §5「无 IPC/CLI/SABI 面」；B-PLAN-002 §5 跨进程序列化/Envelope 通道不在切片），冻结条目 wire 面零接触、无 diff 可生。

**裁决**：**SATISFIED-WITH-BOUNDARIES**。

**边界**（证据自身登记）：

1. **`cargo test --workspace` 与三平台 CI / MSRV 未运行**（各 lane「未运行项」一致登记：波次屏障由控制器收口，CI 待 push 后触发）——零回归结论的覆盖域是逐 crate 全量（nlos-plan/nlos-task/nlos-artifact/nlos-application + slice-k check），非 workspace 级单遍。
2. schema v2 admission 收紧（同一节点重复依赖键由「重复计入 digest」改为 typed 拒绝）为登记的行为差异（B-PLAN-001 §7.1：W28-A 测试未覆盖该语义，零回归）——非回归，如实带入。
3. 同 signer 双面签名共存语义的专门 golden 未做（域分离下语义自然成立，B-PLAN-002 §4）。
4. additive 经平行签名面而非 `PackageManifest` 加字段实现（§2.1 理由 + 回退路径：删依赖行与 task_templates 模块）；若编排方不认可该解释属设计复审事项，不属回归。

## 7. 裁决汇总

| 门 | 证伪条件是否被触发 | 裁决 | 关键测试（证据文件§节） | 主边界 |
|---|---|---|---|---|
| G1 | 否（红→绿实证闭合） | **SATISFIED-WITH-BOUNDARIES** | `g1_executed_node_revision_cannot_be_rewritten_by_new_revision` / `g1_frozen_node_redeclared_bit_identical_keeps_original_revision_and_digest` / `g1_storage_triggers_block_raw_rewrites`（B-PLAN-001 §2.4/§3）+ F1–F4（§4） | fence=物化边界非授权边界；pre-execution 全量声明语义；apply 无调用方 CAS |
| G2 | 否（线性+有界+零预占） | **SATISFIED-WITH-BOUNDARIES** | `ten_thousand_logical_task_nodes_stay_lazy_and_bounded` / `one_hundred_thousand_logical_task_nodes_stay_lazy_and_bounded`（B-PLAN-001 §9） | debug/test profile 单平台；合成图形状；O(population) 读回 |
| G3 | 否（两证伪条件均闭合） | **SATISFIED-WITH-BOUNDARIES** | `g3_unmet_dependency_cannot_materialize_through_any_face` / `g3_admission_denial_shrinks_window_with_typed_durable_reason` / `g3_working_set_dimension_denies_and_window_stops_growing` / `g3_window_shrink_composes_with_checkpoint_evict_and_residency_eviction` + F1–F4（B-PLAN-001 §10.4/§10.5） | 五条件仅依赖+admission 已强制；Namespace/ResourceContract/fanout 仍为 digest；授权面无承载 |
| G4 | 否（漂移红→pinned 绿） | **SATISFIED-WITH-BOUNDARIES** | `g4_resolution_pins_shapes_against_later_revision_reshape` / `g4_current_selector_pins_once_and_receipt_never_floats` / `g4_stale_resolution_revision_cannot_drive_transitions_past_fence`（B-PLAN-001 §7.3–§7.4） | selector 域=plan revision；生态 selector（Package/Model/…）半边递延 |
| G5 | 否（重绑+注明+双档实跑） | **SATISFIED-WITH-BOUNDARIES** | W29-A probe 重跑 10K/100K（B-TASK-SCALE-001 §12）+ `one_hundred_thousand_logical_task_nodes_stay_lazy_and_bounded`（B-PLAN-001 §9）+ `g3_admission_denial_shrinks_window_with_typed_durable_reason`（物化半边 consult） | apply 侧 admission consult 仍缺（仅物化半边接线）；debug/test 单平台 |
| G6 | 否（零回归+旧包硬门绿） | **SATISFIED-WITH-BOUNDARIES** | `g6_old_shape_package_verifies_and_installs_byte_identically` / `verify_with_tasks_fails_closed_on_segment_tamper_and_strip` / `plan_override_001_templated_segment_compiles_to_the_direct_plan_proposal`（B-PLAN-002 §3）+ nlos-task 347→355→369→371 全 0 failed（§12/§13/§14、B-PLAN-001 §10.8） | workspace 全量+三平台 CI 待控制器收口；additive 为平行签名面；SABI 条款 by-scope 不适用 |

## 8. 底线：ROAD-B-004 现在可声称什么 / 仍开放什么

### 8.1 现在可声称（有具名证据支撑）

1. **六门证伪条件全部未被触发**：G1–G6 每门的证伪面都有红→绿记录或具名测试闭合（G1/G3/G4 均先在真实系统上观测到证伪面成立、再以 typed/存储层守卫闭合；G2/G5/G6 由具名探针/负路径测试证明）。台账可记：六门 **SATISFIED-WITH-BOUNDARIES**，无一 NOT-SATISFIED。
2. **声明面落点成立**：ADR-0016 组合候选（B 独立 `nlos-plan` authority + C manifest 模板来源）两侧均有落地产物——nlos-plan schema v1–v4（revision 链/节点状态机/解析回执/residency 轴/物化门）+ 平行签名模板面（旧包兼容硬门绿、编译等价逐字节）。
3. **规模数字（ROAD-B-004 量纲面）**：100K 逻辑 TaskNode（每节点 durable 508–553B、RSS 增量 ≪ 界、惰性点读平坦）；100K 注册维 probe 首跑；working-set 比例矩阵 5 cell（10K×{1,10,50}% + 100K×{1,10}%，admission 精确断言零退化）；checkpoint/rehydrate 24/500/5_000 节点实测（rehydrate 1_811–2_166 nodes/s）；reclaim 实执行闭环（2_032–2_758 evictions/s，readback 三面证明）。**全部为 debug/test profile、单平台 macOS 数字**。
4. **B4-1..B4-9 车道均有落档 evidence**（W28-A/B、W29-A/B、W31-A–F），进度单 §6.5.3 W31 行除本评审（W31-G）外均已交付。

### 8.2 仍开放（证据自身登记的缺口，逐条可溯源）

1. **release-profile 复测 + 多平台（Linux/Windows）+ CI 化**：全部规模数字的口径边界（B-PLAN-001 §9.6/§9.7、B-TASK-SCALE-001 §13.5/§14）。
2. **G3 三条件未到证伪面**：Namespace / ResourceContract / fanout gate 仍是声明 digest（对应权威未落）；授权面（`WAITING_AUTHORIZATION` 解除）无权威承载（B-PLAN-001 §10.7）。
3. **G4 生态 selector 半边**：Package/Skill/Tool/Model/Artifact/Topic/外部服务 typed selector → generation handle 解析未落（B-PLAN-001 §7.6）。
4. **声明面（apply 时）TaskNode 维 admission consult 仍缺**：仅物化半边接线（W29-A §12 缺口 #1 / W30-D 接线位 / B-PLAN-001 §9.6）。
5. **Task 面 reclaim 闭环与 plan 侧 residency 轴未互连**：本波驱逐在 Task 工作集，不驱动 `record_residency_transition`（B-TASK-SCALE-001 §14 缺口 #1）。
6. **PINNED tier 未落**（[SCALE-PIN-001] 全语义随 Resource/物化后续车道）；resident-bytes/pin-reason/rebuild-cost 台账未落（B-PLAN-001 §8.6）。
7. **矩阵登记项**：比例矩阵无 100K@50% cell（档位选择非测量缺口）且 occupancy 只单调填充、无回收再入场动态（B-TASK-SCALE-001 §13）；调度器自身规模探针未跑（B-PLAN-001 §11.5）。
8. **workspace 级全量门 + 三平台 CI/MSRV + push/PR**：各 lane 一致登记为控制器收口动作；完成后 G6 边界 1 方可解除。
9. **故障矩阵外推边界**：kill-9=页缓存存活的进程崩溃；真实断电/跨 Cell/三权威原子性未测（B-PLAN-001 §4）。

### 8.3 ADR-0016 证据边界对照（复审触发器 #5）

- ADR §证据边界（「本 ADR 为设计定案（DESIGN 级），无实现证据」）已由 W28-A 起各 lane evidence 解除；本评审即触发器 #5 所要求的六门 Evidence review。
- 退出策略未触发：G1/G2/G5 未证伪（候选 D 收缩路径不需要）；manifest 模板段未证伪（C 半边无需降级）。
- 触发器 #2（100K benchmark 若惰性有界不成立重开落点）未触发：100K 惰性有界实测成立（带 §8.2.1 口径边界）。

## 9. 本评审自身的边界与登记项

1. **docs-only 评审**：未复跑任何测试/探针；所有数字与测试名系原样引用自上列证据文件（复现命令在各证据节）。评审基 HEAD `14f42ab`，工作区仅新增本文件与 evidence-index.yaml 一行追加，均未提交（控制器集成）。
2. **索引交互（供控制器）**：`scripts/lint_claims.py` check (d) 的目录扫描为**非递归** `glob("*.md")`（仅 `docs/evidence/stage-b/` 顶层）；本文件位于 `reviews/` 子目录且已按任务书追加索引条目，lint 将报「索引收录了目录外/不存在文件」直到该 glob 改为递归（`**/*.md`）或条目迁移——修 lint 不在本 lane 写集，特此登记。追加前基线：`lint: PASS（0 项 ERROR）`，index 139/139。
3. 进度单 §6 ROAD-B-004 行与 §6.5.3 W31-G 行的更新归控制器（本 lane 对该文件只读）。
