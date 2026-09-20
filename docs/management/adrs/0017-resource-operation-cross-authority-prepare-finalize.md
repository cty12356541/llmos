# ADR-0017：Resource/Operation 跨 authority prepare/finalize 入口——Resource 采纳有界 coordinator、Operation 只采纳 verify 半边接线、恢复台账维持按域镜像

- 状态：ACCEPTED（控制器按用户 2026-09-20 授权定案，否决窗口留至下波屏障）
- 日期：2026-09-20
- Owner：TaskAuthority / `B-TASK-008C2G` 家族（Resource 半边）；Operation 半边协同 `nlos-store` owner（ADR-0002）
- 关联 Requirement：v0.5 行 164（资源 = quote + reserve + consume + finalize）、行 1553–1555（`[BUD-LATE-001]`/`[BUD-CANCEL-001]`）、行 712（`[PROC-KILL-002]`）、行 4418（`[TASK-COMMIT-002]`）、行 4450（`[TASK-CANCEL-003]`）、行 4881（`[ROAD-B-003]`）
- 关联工作包：B3-2（[进度单 §6.5.1](../stage-b-progress.md)）；车道 W27-B/W28-C/W29-C（§6.5.3）；`B-TASK-008C2G-RES-COMMIT`、`B-RESOURCE-002..006`、`B-OP-FENCE-002`、`B-TASK-008C2G-OP`、`B-TASK-008C2G-UNIFIED-RECOVERY`
- 决策来源：控制器按用户 2026-09-20 授权在 W27-B 车道定案（否决窗口留至下波屏障）；完整候选对照与证据引用见[评估记录 2026-09-20-w27-b-prepare-finalize](../evaluations/2026-09-20-w27-b-prepare-finalize.md)
- 证据：全部为既有实现证据（下文逐条引用）；本 ADR 为设计定案（DESIGN 级），不声称任何新实现
- 复审触发器：见文末五项

## 上下文

[ADR-0013](./0013-cross-authority-verify-then-commit-contract.md) 把 verify-then-commit + 有界收敛定为单机跨 authority 提交契约。其自动收敛半边经 W25/W26 已在 **Artifact 与 Semantic 两域**兑现：schema v42 Semantic 恢复台账、worker 双域驱动（无 caller 重启收敛到唯一终态）与统一 `TaskCommitReceipt` 五变体读侧聚合（[B-TASK-008C2G-UNIFIED-RECOVERY](../../evidence/stage-b/b-task-008c2g-unified-recovery.md)，提交 `1a14ae0..2b51d28`）；该波 spec 明确把「给 Resource/Operation/Process 建 prepare/finalize 入口」列为非目标、留待 W27+ 评估（[统一恢复面设计 §1/§8](../../superpowers/specs/2026-09-13-unified-recovery-plane-design.md)）。[进度单 §5 W26 段](../stage-b-progress.md)随后把下一验收门更新为「Resource/Operation 的 prepare/finalize 入口评估（W27+ 候选，届时若有三域样本再评估台账泛化）」，即本 ADR（B3-2，车道 W27-B）。

两域现状不对称：

- **Resource**：owner 侧生命周期链已闭合——activation receipt 回读（[B-RESOURCE-002](../../evidence/stage-b/b-resource-002-activation-receipt-readback.md)）、consume high-water（[B-RESOURCE-003](../../evidence/stage-b/b-resource-003-consumption-high-water.md)）、QUARANTINED 保守冻结（[B-RESOURCE-004](../../evidence/stage-b/b-resource-004-quarantine-freeze.md)）、finalize/refund 双重记账结算 + QUARANTINED→FINALIZED reconciliation 解冻 + F1–F6 故障矩阵（[B-RESOURCE-005](../../evidence/stage-b/b-resource-005-finalize-refund.md)，重启生命周期前缀提交 `9e57694`）、owner 只读聚合 `inspect_cost_receipt`（FINALIZED 门 + 七项绑定等式 + consumption 闭合，[B-RESOURCE-006](../../evidence/stage-b/b-resource-006-cost-receipt-aggregate.md)）。Task 侧已有 resource-aware v3 finalize 与 schema v39 嵌套表、混合 Semantic+Resource rung、桥接 kill-window 矩阵 W1–W6 全绿（[B-TASK-008C2G-RES-COMMIT](../../evidence/stage-b/b-task-008c2g-resource-cost-commit.md)）。**缺口**：终结请求身份（`FinalizeRequestV3` + `finalize_proof_digest`）由调用方逐次供给、Task 侧无持久化 envelope，也无 incomplete-plan 扫描与 worker 驱动——owner 已结算而调用方进程永久消失时，Task 停在 permit Issued、head 不前进、无人收敛（与 Semantic 在 W26 前的「无人调用则不收敛」同形，见统一恢复面设计 §2 的动机描述）。
- **Operation**：owner 侧已有 durable prepare→activate 边界（schema v4 immutable preparation/activation receipt、cancel/generation fence、重启 exact replay、旧 direct dispatch 围栏，[B-OP-FENCE-002](../../evidence/stage-b/b-op-fence-002-operation-endpoint-proof.md)，提交 `f6530fc`），endpoint 已接入 TaskWriteSet per-effect endpoint / participant registry / permit 前 owner 复核（[B-TASK-008C2G-OP](../../evidence/stage-b/b-task-008c2g-operation-endpoint-binding.md)）。**缺口**：该证据「明确未完成」段自己列出——TaskAuthority 接线仍须把 activation receipt 纳入 participant/effect binding 并在 permit/finalize 前重新回读；dispatch/completion 与统一 `TaskCommitReceipt` 接线未做。Task 侧 effect 生命周期恢复已由 EffectSlot 状态机 + effect history + `EFFECT_UNKNOWN` 跨重启 quarantine/reconcile 拥有（进度单 §4.23/§4.25/§4.26）。

## 约束

1. [ADR-0013]：单机跨 authority 提交是 verify-then-commit + 有界收敛；owner 读在 Task 事务之外；崩溃窗口由 durable prefix 收敛；本 ADR 不得引入跨 authority 原子性声明。
2. [ADR-0004]：commit 恢复生命周期归 TaskAuthority 内部 worker；worker 不建立第三份 canonical 状态（owner 事实留在各 authority）。
3. [ADR-0005]：authority-first 顺序；Task 侧不提前冻结未到时机的 schema 形状。
4. `[BUD-CANCEL-001]`（v0.5 行 1555）：取消/退出必须先隔离外部执行再按已发生用量 finalize，不得直接全额退款；`[BUD-LATE-001]`（行 1553）：finalize 后迟到 consume 只走受限 rebate。⇒ **自动结算不得凭空造 effect-closed 证明或 final usage**——这两项事实当前是 caller-asserted opaque digest（[B-RESOURCE-005] §4），真实 enforcement-gateway 签名属未来 reconciliation authority。
5. `[TASK-COMMIT-002]`（行 4418）：permit 关闭要求每个 planned slot 有唯一已知终态与权威 Receipt；`[TASK-CANCEL-003]`（行 4450）：cancel/permit/slot dispatch CAS 必须在同一 control/cancel/permit epoch 上线性化。⇒ 任何「worker 自动 activate dispatch」都破坏一次性 dispatch token 的线性化语义。
6. schema 迁移只 additive（v42 即 v8 的 additive 镜像先例，[B-TASK-008C2G-UNIFIED-RECOVERY] §2.1）；已过故障矩阵的表组不得为形式统一而改写。
7. 阶段 B 收官时间线：W30-A 六域闭环验收（§6.5.3）依赖恢复面稳定；W27-A（同波、在飞）已占用恢复运维面写集（nlos-schema/nlos-system-control/nlos-task）。

## 候选

**Resource 域**（三候选 + owner 半边的独立裁剪）：

| 候选 | 内容 | 优点 | 主要代价 |
|---|---|---|---|
| R-A（否决） | 不建 coordinator：维持调用方两阶段（caller 先逐项 `finalize_reservation`，再调 resource-aware v3 finalize） | 零新 schema/API；既有 W1–W6 矩阵已证明同请求重试双向收敛 | owner 已结算 + 调用方永久消失 ⇒ Task 永停 permit Issued；自动收敛契约对第三域缺席，与两域既成模式（ADR-0004 + W26）不一致 |
| R-B（否决） | 全自动 coordinator：worker 同时驱动 owner 侧 finalize/refund（含 QUARANTINED 解冻） | 三域完全同构的自动性 | worker 无法产出 `effect_closed_proof_digest`/`final_usage`（约束 4）；自动全额退款违反 `[BUD-CANCEL-001]`；等于伪造结算证据 |
| **R-C（采纳）** | **有界 coordinator**：Task 侧持久化 resource finalize envelope + plan（镜像 v26 envelope / semantic plan 模式），`converge_pending` 重启扫描，owner 已 FINALIZED 时自动收敛 Task 侧；owner 未结算的 plan **不由 worker 触碰**（not-due 语义，零 owner 变更），仅作为 durable 事实可被运维面看见 | 把缺口收窄到「无外部证据不动作」的诚实边界；owner 半边不动、零新增 owner 语义风险；复用 v3 finalize/v39 表组全部既有矩阵 | 新增 plan/envelope 表组与第三 worker cycle；escalation 语义只覆盖 infra 失败，owner 证据缺失走人工/gateway 通道 |

**Operation 域**：

| 候选 | 内容 | 优点 | 主要代价 |
|---|---|---|---|
| O-A（否决） | 镜像 semantic 的 coordinator 入口（Task 侧 plan 状态机 + worker cycle） | 与两域同构 | Operation 的 Task 侧恢复 owner 已存在（EffectSlot/effect history/`EFFECT_UNKNOWN` quarantine-reconcile，§4.23/§4.25/§4.26）——第二恢复 owner 违反 ADR-0004 单一归属；且 owner 侧 prepare→activate 已自带 durable receipt + 重启 exact replay（`f6530fc`），无 Task 侧多步 publication 可驱动 |
| **O-B（采纳）** | **仅 verify 半边接线**：permit/finalize 前 owner 回读 activation receipt 并纳入 participant/effect binding（[B-OP-FENCE-002]「明确未完成」原文）；prepared-but-unactivated / canceled / generation 漂移 fail-closed | 补齐 `[TASK-COMMIT-002]` slot 终态证据链的 owner 复核；与 `B-TASK-008C2G-RES/ART/PROCESS` owner-revalidation 家族同模式；零新表零新 worker | 不提供「无 caller 收敛」——由既有 effect 机制拥有（本就如此） |
| O-C（否决） | 什么都不做（连 verify 接线也不做） | 零成本 | slot DISPATCHED→闭合的证据链无法对抗「prepared 未激活却被描述为已派发」；W29-C 既定车道（§6.5.3）即要求此接线 |

**台账泛化**（若 Resource 采纳，第三域进入恢复面）：

| 候选 | 内容 | 裁定 |
|---|---|---|
| L-A（否决，本阶段） | 把 v8（Artifact）/v42（Semantic）合并为单一 domain-generic 台账表组，worker 改域参数化循环 | 收尾期对已过 F1–F4 矩阵的表组做合并迁移 = 重跑两套矩阵 + golden；与 W27-A 写集（nlos-task 恢复面）冲突；收益只在第四域样本出现时兑现 |
| **L-B（采纳）** | **按域镜像**：Resource 以 v42 同构表组进入（下一顺位 schema 版本，按波内合入顺序递增），语义逐条镜像 `SEM-RECOV-001..007`（含 CAS/退避/Escalated/告警 acknowledge/行丢失自愈） | additive-only；v42 本身就是 v8 的镜像先例（存在性守卫 + partial fail-closed）；退避/阈值钉死为与 v42 同款，不扩大已知不对称面 |
| L-C（否决） | Resource 进 coordinator 但不建台账（infra 失败仅 worker 内存计数） | 重启丢失退避日程，违反 worker durable scheduling 纪律（B-TASK-006J 先例） |

## 比较

逐维度对照（证据逐条见评估记录 §2）：

- **崩溃窗口覆盖**：R-C 把「owner 已结算 + 调用方消失」窗口纳入自动收敛（对应 semantic RESTART-SCAN 形态）；R-A 留窗、R-B 不可实现（约束 4）。O-B 不新增窗口声明——owner 侧 prepare→activate 重启 replay 已证（`f6530fc`），Task 侧 slot 恢复既有。
- **schema 迁移成本**：R-C+L-B = 一次 additive 镜像迁移（v42 先例：2 表 + 1 索引 + 2 触发器 + 存在性守卫，[B-TASK-008C2G-UNIFIED-RECOVERY] §2.1 `[SEM-RECOV-006]`）+ envelope/plan 表组（v26 先例）；L-A = 两套已验证表组的合并迁移 + 全矩阵重跑；O-B ≈ 0（只读接线，可能仅 participant binding 列校验扩宽，沿 v24 additive 先例）。
- **SABI/运维面增长**：R-C+L-B 的第三域告警/resume 需要运维可见性——W27-A 正在为 semantic 接线（IPC/CLI/metrics，§6.5.3 W27-A 行）；裁定 W28-C 必须使 resource 域 Escalated/resume 至少经 W27-A 引入的同一通道家族可达（若 W27-A 落成 semantic 字面量，允许最小 additive 扩展接入并登记命名债，不得留「对运维面整体不可见」的已知缺陷重演——semantic 该缺陷的教训见 [B-TASK-008C2G-UNIFIED-RECOVERY] §6）。L-A 的域参数化面最省但被时间线否决。O-B 无运维面增量。
- **故障矩阵规模**：R-C+L-B 新增约五组（envelope F1–F4、ledger F1–F4 + 套件镜像、三域 worker 隔离、无 caller 收敛核心门、桥接矩阵 envelope 窗口扩展）——全部有 v42/RES-COMMIT 模板可抄；L-A 额外要求重跑 v8/v42 两套已绿矩阵；O-B 新增 2–3 项 owner 复核证伪测试。
- **可逆性/退出成本**：R-C+L-B 纯 additive，降级路径 = 忽略新表（旧版本读不到即视为无台账，plan 事实不受损——统一恢复面设计 §7.3 同款）；退出为删除扫描入口，owner/Task 既有事实零改写。L-A 退出需再迁移回去，代价最高。O-B 退出 = 移除复核读，回到现状。

## 决定

1. **Resource：采纳 R-C（有界 coordinator），W28-C 实现。** TaskAuthority 持久化 resource finalize envelope（终结请求身份 + 从 sealed write set 推导的 Reservation 集，镜像 v26 envelope 模式）与 plan 状态机；`converge_pending` 扫描 incomplete resource plan：owner 全部 FINALIZED（`inspect_cost_receipt` FINALIZED 门）⇒ 在既有 resource-aware v3 单事务路径收敛/幂等重放（只读 Task 行，镜像 RES-COMMIT 测试 5）；任一 owner 未结算 ⇒ **not-due：零 owner 变更、零台账失败记录**，plan 保持可被运维面 inspect 的 durable 事实。Owner 侧 finalize/refund/quarantine 仍归调用方/未来 enforcement-gateway（约束 4）。
2. **Operation：否决 coordinator 入口（O-A），采纳 verify 半边接线（O-B），W29-C 按此范围执行。** 即 §6.5.3 W29-C 车道维持「接线」定位：activation receipt 的 permit/finalize 前 owner 回读 + participant/effect binding；**负向门**：不建 Task 侧 operation plan 状态机、不进恢复台账、不加 worker cycle（恢复归属仍是 effect 机制）。
3. **台账泛化：维持按域镜像（L-B），本阶段不泛化（L-A 否决）。** Resource 台账逐条镜像 `SEM-RECOV-001..007`；退避公式与 escalation 阈值钉死为与 v42 同款（不新增配置项，沿 `[UNIFIED-WORKER-003]` 单阈值纪律），不扩大 [B-TASK-008C2G-UNIFIED-RECOVERY] §6 已登记的不对称面。第四域样本（如 Channel 需要自动收敛）出现时按复审触发器 1 重开。
4. **W28-C 实现切片定义**：见附录 A（车道行 + 类型化验收门 + 故障矩阵期望，风格沿 §6.5.3 车道行与 B-TASK-008C2G-RES-COMMIT 矩阵）。W29-C 的重定义（范围收紧确认）见附录 B。
5. 本 ADR 不改动 ADR-0013 契约语义；Resource coordinator 兑现的是其有界收敛半边在第三域的延伸，一切崩溃窗口声明仍以收敛性而非原子性为准。

## 后果与退出策略

- **正面**：ADR-0013 自动收敛半边覆盖第三域；「owner 已结算 + 调用方消失」窗口消除；Resource 结算的不可自动半边获得诚实边界（not-due + 可观测），与 `[BUD-CANCEL-001]`/`[PROC-KILL-002]`（行 712：Reservation 在 reconciliation/finalize 前保持冻结）的规范意图一致。
- **负面/债务**：第三套镜像表组与第三 worker cycle；v42 §6 登记的已知不对称（jitter/阈值/health 枚举）在第三域延续（钉死为 v42 同款以不扩大）；W28-C 与 W27-A 在 nlos-task 恢复面上存在潜在写集交叠——按 §6.5.3 派发纪律，nlos-task 在 W28 内为串行位 1，W27-A 先行合入后 W28-C 基于其末态开发。
- **运维责任**：resource Escalated 告警/resume 的可达性为 W28-C 硬门（附录 A G8）；owner 未结算 stuck plan 需要人工/gateway 处置流程（观测面提供事实，不自动动作）。
- **退出策略**：全部 additive；降级 = 旧版本忽略新表组（统一恢复面设计 §7.3 同款路径）；若 W28-C 实现证伪 G4 的 not-due 语义（如 stuck plan 积压不可运维）或 envelope 形状错误，收缩回 R-A（调用方两阶段）并按复审触发器 5 重开本 ADR——收缩不损失任何 owner/Task 既有事实。

## 验证与证据

本 ADR 为设计定案，无新实现证据。裁决依据的既有证据：[B-TASK-008C2G-UNIFIED-RECOVERY]（v42 台账/双域 worker/F1–F4/无 caller 收敛/§6 已知不对称与运维面缺口）、[B-TASK-008C2G-RES-COMMIT]（v39 嵌套表、authority-first 聚合、replay 只读 Task 行、W1–W6 桥接 kill-window 矩阵 11 项全绿）、[B-RESOURCE-005]（owner 单事务结算 + caller-asserted opaque proof 限制 + F1–F6 + `9e57694` 重启前缀）、[B-RESOURCE-006]（FINALIZED 门聚合）、[B-RESOURCE-004]（quarantine 保守冻结）、[B-OP-FENCE-002]（prepare→activate + `f6530fc` + 「明确未完成」清单）、[B-TASK-008C2G-OP]（endpoint 接线缺口）、进度单 §4.23/§4.25/§4.26（effect 恢复机制）、§6.5.1 B3-2/§6.5.3 车道行。附录 A 各门的实现证据由 W28-C 车道落档后方可声称。

## 复审触发器

1. 第四域样本：W30-A 六域闭环中 Channel（或其他域）被证明需要自动收敛 ⇒ 重开台账泛化（L-A vs L-B 再评）。
2. enforcement-gateway 签名 proof（真实 reconciliation authority）落地 ⇒ 重开 owner 侧自动结算评估（R-B 的受限复活）。
3. 发现 resource plan 不可收敛窗口（存在无法从 durable prefix 收敛到唯一终态的崩溃形态）⇒ 登记 conflict、降级相关 Claim，并联动重开 ADR-0013。
4. Stage C 跨机 reconciliation 经新 ADR 定案时，复审三域 coordinator 的边界划分。
5. W28-C 验收证伪 G1–G8 任一且不可修复 ⇒ 收缩回 R-A 并重开本 ADR。

## 附录 A：W28-C 实现切片定义（Resource 跨 authority prepare/finalize coordinator）

> 车道行风格沿 [进度单 §6.5.3](../stage-b-progress.md)；门为证伪式类型化验收门（实现车道须逐门落测试）；故障矩阵期望沿用 B-TASK-008C2G-RES-COMMIT 的窗口表式样。本附录由控制器用于派发 W28-C；实现落档前不得声称任何门已通过。

| 子车道 | 事项 | 主要写集 | 验收门 |
|---|---|---|---|
| W28-C-1 | resource finalize envelope + plan 状态机 + incomplete 扫描 API | `nlos-task`（schema 下一顺位版本 additive 表组：immutable envelope + plan；`resource_commit.rs`/`recovery.rs`/`migrations.rs`） | G1、G2 |
| W28-C-2 | resource 恢复台账域（v42 同构镜像） | `nlos-task`（`recovery.rs`/`migrations.rs`，与 W28-C-1 同事务域开发） | G5 |
| W28-C-3 | worker 第三域驱动（`resource_cycle`，artifact→semantic→resource 顺序） | `nlos-commit-coordinator`（`worker.rs` + 测试） | G3、G6 |
| W28-C-4 | 桥接 kill-window 矩阵扩展（envelope 窗口）+ 故障 fixture | `nlos-task` tests（`resource_bridge_fault_injection.rs` 扩展；`nlos-resource` 预计仅测试只读复用，无生产写路径——owner 侧无新 API 需求） | G7 |
| W28-C-5 | Evidence 落档 + 进度单/ADR 索引同步 | docs（本 ADR 附录勾稽、`docs/evidence/stage-b/`、进度单 §3/§5/§6 行） | G8 + 波次屏障 |

**类型化验收门**（全部为证伪条件；任一失败即车道未过）：

- **G1（envelope/plan 不变量）**：envelope 逐字节 exact replay；immutable 触发器拒绝 UPDATE/DELETE；sealed write set 漂移/重复 ReservationId/跨 plan 复用 envelope ⇒ typed fail-closed（零 Task 终结变更）；迁移幂等 + partial schema fail-closed（沿 v42 `[SEM-RECOV-006]` 模式）。
- **G2（既有 verify-then-commit 语义零回归）**：resource-aware v3 与 mixed rung 全部既有测试逐位不变；permit Closed 的重放只读 Task 行（对空 ResourceAuthority 重放逐字节相等，镜像 RES-COMMIT 测试 5）；envelope 只增不改终结事务结构（仍是 `insert_receipt` → nested → close → head 单事务）。
- **G3（无 caller 收敛，核心门）**：prepare(envelope) → 逐项 owner `finalize_reservation` → 丢弃并重开 TaskAuthority/worker → 仅凭 durable plan/envelope + owner 读收敛到唯一终态（plan FINALIZED、恰一套嵌套 receipt、head 前进、显式 replay 逐字节相等）；对应 W26 `worker_converges_pending_semantic_plan_without_caller` 形态。
- **G4（not-due 负向门）**：owner 任一 Reservation 仍 ACTIVE/QUARANTINED 时，converge 对 owner **零变更**（无 finalize/refund/consume/quarantine 调用，以 owner 侧表行数与余额断言），plan 不记失败、不退避、不 escalate；stuck plan 可经 inspect/告警面读出。
- **G5（台账镜像）**：`SEM-RECOV-001..007` 逐条镜像（CAS 单增、due scan 过滤、Escalated 显式 resume、finalize 置 Resolved、infra 失败入退避、迁移幂等、告警 acknowledge 面）；F1–F4 故障注入（kill-9 中断/commit 后崩溃/IoErr/静默丢写）下台账一致、无双记、重放幂等、行丢失自愈重扫。
- **G6（三域 worker 隔离）**：三域各有 pending 时单轮全收敛；resource 域连续 infra 失败达阈值只置本域 Faulted，artifact/semantic 照常收敛（镜像 `[UNIFIED-WORKER-002]`）；`stop`/首轮立即扫描/轮询间隔语义不变；既有 coordinator 测试零回归。
- **G7（桥接 kill-window 扩展矩阵）**：在 RES-COMMIT W1–W6 基础上补两个 envelope 边界窗口——(i) prepare 后、owner finalize 前崩溃；(ii) owner 全部 FINALIZED 后、Task finalize 前崩溃（含 `PowerLossAfter` 双向与 torn WAL tail）。期望（沿用 W1–W6 断言式样）：(i) 重开后 plan 仍 PLANNED、G4 语义成立、同请求继续推进；(ii) 不可见方向零部分状态、可见方向恰一套嵌套行、两方向重放均逐字节相等、绝无部分可见。
- **G8（运维面 + 全量门）**：resource 域 Escalated 告警与 resume 至少经 W27-A 引入的运维通道家族可达（若其为 semantic 字面量，允许最小 additive 扩展并登记命名债；禁止重演「对运维面整体不可见」缺陷）；`cargo test --workspace --no-fail-fast` 零回归 + fmt/clippy 双 0 + 三平台 CI/Pages run 回填。

## 附录 B：W29-C 范围确认（Operation 半边）

W29-C 维持 §6.5.3 既有定义并按本 ADR 决定 2 收紧为 verify 半边接线：

| 车道 | 事项 | 主要写集 | 验收门 |
|---|---|---|---|
| W29-C | Operation prepare→activate 跨 authority 接线（O-B：仅 verify 半边） | `nlos-task`、`nlos-store` 接线（nlos-task 串行位 3，波内屏障 2） | permit/finalize 前 owner 回读 activation receipt 并入 participant/effect binding；prepared-but-unactivated、canceled、旧 generation ⇒ typed fail-closed；owner 侧 prepare/activate 重启 exact replay 保持（`f6530fc` 语义零回归）；**负向门**：不新增 Task 侧 operation plan 状态机/台账表组/worker cycle（出现即违例） |
