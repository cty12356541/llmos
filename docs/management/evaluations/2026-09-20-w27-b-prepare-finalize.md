# 评估记录：Resource/Operation 跨 authority prepare/finalize 入口（W27-B / B3-2）

- 日期：2026-09-20
- 车道：W27-B（[进度单 §6.5.3](../stage-b-progress.md) W27-B 行；B3-2，§6.5.1）
- 产出：[ADR-0017](../adrs/0017-resource-operation-cross-authority-prepare-finalize.md)（ACCEPTED）+ 其附录 A（W28-C 切片定义）/附录 B（W29-C 范围确认）
- 读集：ADR-0013/0016/0004/0005、统一恢复面 spec 与 Evidence、B-TASK-008C2G-RES-COMMIT/OP/COORD/UNIFIED-RECOVERY、B-RESOURCE-002..006、B-OP-FENCE-002、进度单 §4.86–4.112/§5 W26 段/§6.5、总纲 v0.5 §28.2 及行 1553–1555/712/4418/4450
- 性质：设计级评估（无代码、无新实现声明）；全部事实断言逐条附文件/提交/运行引用

## 1. 问题分解

ADR-0013 的自动收敛半边已在 Artifact/Semantic 两域兑现（[B-TASK-008C2G-UNIFIED-RECOVERY](../../evidence/stage-b/b-task-008c2g-unified-recovery.md)：schema v42 台账、worker 双域、无 caller 重启收敛，提交 `1a14ae0..2b51d28`，全量门 218 二进制/1177 passed）。[统一恢复面设计](../../superpowers/specs/2026-09-13-unified-recovery-plane-design.md) §1/§8 与 [进度单 §5 W26 段](../stage-b-progress.md) 留下三个子问题：

1. Resource（reservation activation→consume→finalize/refund）是否需要同构 prepare/finalize coordinator 入口？
2. Operation（durable prepare→activate）是否需要？
3. 若出现第三域样本，恢复台账泛化还是按域？

## 2. Resource 域候选对照

### 2.1 现状事实（缺口界定）

| # | 事实 | 引用 |
|---|---|---|
| F1 | owner 侧生命周期链闭合：activation receipt 回读、consume high-water、QUARANTINED 冻结、finalize/refund 双重记账单事务结算 + reconciliation 解冻、F1–F6 故障矩阵、三边界重启前缀 | [B-RESOURCE-005](../../evidence/stage-b/b-resource-005-finalize-refund.md) §1/§5/§6（提交 `9e57694`，CI run 32629828391/32629828373）；B-RESOURCE-002/003/004 |
| F2 | owner 只读聚合 `inspect_cost_receipt`：FINALIZED 门 + 七项绑定等式 + consumption 闭合 high-water，重启逐字节重放 | [B-RESOURCE-006](../../evidence/stage-b/b-resource-006-cost-receipt-aggregate.md) §1 |
| F3 | Task 侧 resource-aware v3 finalize：authority-first 全量聚合（调用方不提供成本事实）、schema v39 嵌套表、单 Task 事务、replay 只读 Task 行、桥接 kill-window 矩阵 W1–W6（11 项全绿）、mixed Semantic+Resource rung | [B-TASK-008C2G-RES-COMMIT](../../evidence/stage-b/b-task-008c2g-resource-cost-commit.md) §1/§3 测试 5/§5/§6 |
| F4 | 终结请求身份（`FinalizeRequestV3` + `finalize_proof_digest`）由调用方逐次供给；Task 侧无持久化 envelope、无 incomplete-plan 扫描、无 worker 驱动 | 同上 §1（请求/幂等身份即调用方重供的请求）；对照 semantic 侧 v26 envelope 与 `list_incomplete_semantic_commit_plans`（[B-TASK-008C2G-COORD](../../evidence/stage-b/b-task-008c2g-semantic-coordinator.md) §2） |
| F5 | `effect_closed_proof_digest` 为 caller-asserted opaque 摘要；endpoint/enforcement-gateway 签名、自动 reconciliation 属未来 | [B-RESOURCE-005] §4；[B-RESOURCE-006] §5 |
| F6 | 规范约束：取消/退出不得直接全额退款、须先隔离执行再按已发生用量 finalize（`[BUD-CANCEL-001]` v0.5 行 1555）；finalize 后迟到 consume 只走受限 rebate（`[BUD-LATE-001]` 行 1553）；Process 退出时 Reservation 在 reconciliation/finalize 前保持冻结（`[PROC-KILL-002]` 行 712） | 总纲 v0.5 行 1553/1555/712 |

**缺口**（由 F4 推出）：owner 已全部 FINALIZED（退款已在 owner 侧落账，F1/F2 不可变）而调用方进程永久消失时，Task 永停 permit Issued、head 不前进——与 Semantic 在 W26 前的「无人调用则不收敛」同形（[统一恢复面设计 §2](../../superpowers/specs/2026-09-13-unified-recovery-plane-design.md) 以此为动机的原文）。

### 2.2 候选与成本对照

| 维度 | R-A 无 coordinator（调用方两阶段，现状） | R-B 全自动 coordinator（worker 驱动 owner finalize/refund） | R-C 有界 coordinator（envelope + plan + Task 侧自动收敛 + not-due 负向语义） |
|---|---|---|---|
| 崩溃窗口覆盖 | owner-已结算+调用方消失窗口留存（F4）；同请求重试收敛已证（F3 W3/W5）但需存活 caller | 覆盖全部窗口 | 覆盖 F4 窗口（Task 侧自动）；owner 未结算窗口诚实保留为可观测 stuck plan |
| 规范符合性 | 不违反 | **违反 F5/F6**：worker 无法产出 effect-closed 证明与 final usage，自动结算=伪造证据或违规全额退款 | 符合：不动作、只观测（与 B-RESOURCE-004 quarantine 的保守边界同哲学） |
| schema 迁移 | 0 | 大（owner+Task 两侧） | additive：envelope/plan 表组（v26 先例）+ 台账镜像（v42 先例：2 表+1 索引+2 触发器+存在性守卫，[UNIFIED-RECOVERY] §2.1） |
| SABI/运维面 | 0 | 大 | 第三域告警/resume 需达 W27-A 通道（ADR-0017 决定 3 硬门 G8） |
| 故障矩阵规模 | 0 新增 | 最大（owner 写路径自动化全重构） | 五组新增、全部有 v42/RES-COMMIT 模板（ADR-0017 附录 A G1–G7） |
| 可逆性/退出成本 | — | 高（owner 语义变更） | 纯 additive；降级=忽略新表（统一恢复面设计 §7.3 同款）；收缩回 R-A 零事实损失 |
| 与既有模式一致性 | 自动收敛契约第三域缺席 | 过度（超出两域同构范围） | 与 ADR-0004/ADR-0013/W26 模式一致 |

**裁定：R-C 采纳，W28-C 实现。** 决定性理由：F4 窗口与两域既成自动收敛模式不一致且修复成本低（模板齐全）；R-B 被 F5/F6 结构性否决（非成本问题）；R-A 的零成本只在「调用方永远存活」的假设下成立。

## 3. Operation 域候选对照

### 3.1 现状事实

| # | 事实 | 引用 |
|---|---|---|
| F7 | owner 侧 durable prepare→activate：immutable preparation/activation receipt、cancel/generation fence、重启 exact replay、旧 direct dispatch 围栏 | [B-OP-FENCE-002](../../evidence/stage-b/b-op-fence-002-operation-endpoint-proof.md)（提交 `f6530fc`，CI run 32629828391/32629828373）；进度单 §4.112 |
| F8 | endpoint 已入 TaskWriteSet per-effect endpoint / participant registry / permit 前 owner 复核（registration proof 层） | [B-TASK-008C2G-OP](../../evidence/stage-b/b-task-008c2g-operation-endpoint-binding.md)；进度单 §4.81 |
| F9 | 已列缺口：activation receipt 尚未纳入 participant/effect binding、未在 permit/finalize 前重新回读；dispatch/completion 与统一 TaskCommitReceipt 接线未做 | [B-OP-FENCE-002]「明确未完成」；[B-TASK-008C2G-OP]「明确缺口」 |
| F10 | Task 侧 effect 生命周期恢复 owner 已存在：EffectSlot 四态 + `EFFECT_UNKNOWN` 跨重启阻塞/quarantine/reconcile + effect history | 进度单 §4.23/§4.25/§4.26（B-TASK-002/003 及增量） |
| F11 | slot 终态证据链要求（`[TASK-COMMIT-002]` 行 4418）与 cancel/permit/dispatch 同 epoch 线性化（`[TASK-CANCEL-003]` 行 4450）；一次性 dispatch token 语义 | 总纲 v0.5 行 4418/4450；进度单 §4.23 |

### 3.2 候选与成本对照

| 维度 | O-A coordinator 入口（plan 状态机+worker cycle） | O-B 仅 verify 半边接线 | O-C 不做 |
|---|---|---|---|
| 恢复归属 | **与 F10 冲突**：effect 机制已是 Task 侧恢复 owner，第二 owner 违反 ADR-0004 单一归属 | 不新增归属 | 不变 |
| 自动 activate 的安全性 | worker 自动 activate 破坏一次性 token/取消线性化（F11；B-OP-FENCE-002 的 direct-dispatch 围栏正是反旁路设计） | 无此面 | 无 |
| 需求覆盖 | 超出需求 | 恰好覆盖 F9（activation receipt 复核） | F9 缺口留存，`[TASK-COMMIT-002]` slot 证据链不完整 |
| schema/SABI/矩阵 | 新表组+worker cycle+矩阵全套 | ≈0（只读接线，v24 additive 先例）；新增 2–3 项证伪测试 | 0 |
| 可逆性 | additive 但归属混乱难回收 | 移除复核读即回退 | — |

**裁定：O-B 采纳（W29-C 维持接线定位并加负向门）；O-A/O-C 否决。** 决定性理由：Operation 的 prepare→activate 是**派发侧**（使效果发生）而非**终结侧**（结算已承诺效果）——与 Semantic/Artifact/Resource 的 publication/finalize 协议不同构；其恢复语义已由 effect 机制拥有（F10），owner 侧自带重启 replay（F7）；唯一真实缺口是 verify 半边（F9）。

## 4. 台账泛化裁定

| 维度 | L-A 泛化（合并 v8/v42 为 domain-generic 表组+参数化 worker） | L-B 按域镜像（v42 同构第三套） | L-C 无台账（仅内存退避） |
|---|---|---|---|
| schema 迁移 | 合并两套已过 F1–F4 矩阵的表组（非 additive，需重跑 W25/W26 两套矩阵+golden） | additive 镜像（v42 即 v8 镜像先例） | 0 |
| 写集冲突 | 与 W27-A（在飞，占 nlos-task/nlos-schema/nlos-system-control 恢复运维面写集）叠加冲突 | W28 内 nlos-task 串行位 1（§6.5.3 W28-C 行），波内可解 | 同 L-B |
| 时间线风险 | 动摇 W30-A 六域闭环（§6.5.3 W30-A 行）所依赖的稳定恢复面 | 低 | 低 |
| 不对称面 | 收敛为单表单循环 | 延续 v42 §6 已登记不对称（jitter/阈值/health 枚举），钉死为 v42 同款不扩大 | 违反 durable scheduling 纪律（B-TASK-006J 先例），重启丢退避日程 |
| 收益兑现条件 | 第四域样本出现时 | 即时可用 | — |
| 退出成本 | 最高（再迁移回去） | 降级=忽略新表（统一恢复面设计 §7.3） | — |

**裁定：L-B 采纳；L-A 本阶段否决、以 ADR-0017 复审触发器 1（第四域样本，如 W30-A 中 Channel 需要自动收敛）显式重开；L-C 否决（纪律问题）。** 关键权衡：泛化的全部收益是「第四域省一套表」，而该样本目前是推测性的（W30-A 的 Channel 半边可能是 endpoint 接线形态，如 Operation 之于本 ADR）；其全部成本（已验证表组合并迁移+矩阵重跑）却落在收官关键路径上。

## 5. 决策摘要与车道门勾稽

| 裁定项 | 结论 | 实现车道 |
|---|---|---|
| Resource coordinator 入口 | **采纳（R-C 有界形态）** | W28-C（ADR-0017 附录 A：子车道 W28-C-1..5 + 门 G1–G8） |
| Operation coordinator 入口 | **否决；采纳 verify 半边接线（O-B）** | W29-C（附录 B：范围确认 + 负向门） |
| 台账泛化 | **按域镜像（L-B），本阶段不泛化** | 随 W28-C-2 落地；触发器见 ADR-0017 |

对照 W27-B 车道验收门（§6.5.3）：「采纳或否决决策 + 理由」✓（本记录 §2–§4 + ADR-0017 决定）；「采纳则给出 W28-C 实现切片定义」✓（ADR-0017 附录 A）；「三域样本是否需要台账泛化一并裁定」✓（本记录 §4 + ADR-0017 决定 3）。

## 6. 证据索引

- ADR/规范：[ADR-0013](../adrs/0013-cross-authority-verify-then-commit-contract.md)、[ADR-0004](../adrs/0004-task-authority-commit-recovery-owner.md)、[ADR-0005](../adrs/0005-task-write-set-authority-first.md)、总纲 v0.5 行 164/1553/1555/712/4418/4450/4875–4882（§28.2 与 `[ROAD-B-003]`）
- 两域机器：[统一恢复面设计](../../superpowers/specs/2026-09-13-unified-recovery-plane-design.md)（§1 非目标、§2 缺口、§7 兼容性、§8 限制）、[B-TASK-008C2G-UNIFIED-RECOVERY](../../evidence/stage-b/b-task-008c2g-unified-recovery.md)（§1 非目标、§2.1/§2.2/§5 全量门、§6 已知不对称与运维面缺口）
- Resource：[B-RESOURCE-002](../../evidence/stage-b/b-resource-002-activation-receipt-readback.md)、[003](../../evidence/stage-b/b-resource-003-consumption-high-water.md)、[004](../../evidence/stage-b/b-resource-004-quarantine-freeze.md)、[005](../../evidence/stage-b/b-resource-005-finalize-refund.md)（提交 `9e57694`；CI 32629828391/32629828373；finalize 矩阵 CI 32099012698）、[006](../../evidence/stage-b/b-resource-006-cost-receipt-aggregate.md)、[B-TASK-008C2G-RES-COMMIT](../../evidence/stage-b/b-task-008c2g-resource-cost-commit.md)（§6 矩阵 W1–W6）
- Operation：[B-OP-FENCE-002](../../evidence/stage-b/b-op-fence-002-operation-endpoint-proof.md)（提交 `f6530fc`）、[B-TASK-008C2G-OP](../../evidence/stage-b/b-task-008c2g-operation-endpoint-binding.md)、进度单 §4.80/§4.81/§4.112
- Semantic coordinator 家族（envelope/plan/扫描先例）：[B-TASK-008C2G-COORD](../../evidence/stage-b/b-task-008c2g-semantic-coordinator.md)（schema v26/v42 脉络）、进度单 §4.84/§4.103–§4.110
- 编排：[进度单](../stage-b-progress.md) §5 W26 段、§6.5.1 B3-2/B3-3、§6.5.3 W27–W30 车道行、§6.5.4 决策点
