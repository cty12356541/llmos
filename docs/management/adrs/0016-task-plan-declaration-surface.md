# ADR-0016：TaskPlan/TaskNode 声明面落点——独立 nlos-plan authority（候选 B）+ application manifest 模板段（候选 C）组合

- 状态：ACCEPTED
- 日期：2026-09-20
- Owner：TaskAuthority / `B-TASK`（ROAD-B-004 主线）；模板面协同 `B-APPLICATION`
- 关联 Requirement：总纲 v0.5 行 360-362（Task 与 TaskPlan `planned-as` 分离）、行 3646-3658（六条 `PLAN-*` 不变量）、行 4460-4509（§25.2 惰性物化与驻留分级）、行 4881（`[ROAD-B-004]`）
- 关联工作包：`B-TASK`（声明面与维度正规化）；`B-APPLICATION`（manifest 模板段与 Task 创建接线）；`B-SLICE-K`（缺口 1 关闭）；`B-TASK-SCALE-001`（维度重绑与 benchmark）
- 决策来源：[议题 35](../../discussions/35-TaskPlan声明面设计.md) §5 推荐倾向；2026-09-20 用户授权「按编排建议直接采纳讨论推荐倾向」（先例表述同 ADR-0009/0015 决策批次）
- 证据：设计级论证见议题 35 全文（候选对照锚点 F1–F6）；[B-TASK-SCALE-001](../../evidence/stage-b/b-task-scale-001.md)（ScaleProfile 前片与缺口清单）、[B-SLICE-K-001](../../evidence/stage-b/b-slice-k-001-end-to-end.md) 缺口 1、[B-APPLICATION-001](../../evidence/stage-b/b-application-001-installation-authority.md)
- 复审触发器：见文末五项

## 上下文

ROAD-B-004 剩余核心需要 TaskPlan proposal/revision 与 TaskNode durable metadata 的权威落点，使 ScaleProfile 维度映射正规化、Dependency Resolver 解析结果有处可挂、惰性物化门有 durable 事实可查；同时 Slice K 缺口 1（TaskSpec 无自由字段，application↔task 关联停留在 slice 编排层）需要模板桥。规范面已定（v0.5 行 3588-3658、4460-4509），本 ADR 只解决落点选择与六项参数（议题 35 §7），不修改 v0.5 行文。

## 候选与比较

完整四候选对照（A：并入 TaskSpec 扩展；B：独立 TaskPlan authority；C：manifest 模板段；D：暂缓仅落 Resolver 骨架）、写集边界分析与被否理由见[议题 35](../../discussions/35-TaskPlan声明面设计.md) §3–§5。结论：**B 为主、C 作模板来源的组合（两半各自可否决）；A 否决；D 仅作里程碑失败后的收缩路径。**

## 决定

以下六项对应议题 35 §7 决策清单，逐项定案：

1. **manifest 扩 tasks 模板段（候选 C 前提）：采纳。** package manifest（§23.2）additive 扩 tasks 模板段（声明式 node、依赖、资源上界模板），随包签名不可变；安装/启动时实例化为 TaskPlan proposal。additive 纪律显式延伸到 package schema 与验签 golden：`llmos.package` 不在 ADR-0014 冻结 REGISTRY 内，扩段按 additive golden 处理，旧签名包仍须可验装（负路径测试硬门）。模板编译为与 B 相同的 TaskPlan/TaskNode schema（`[PLAN-OVERRIDE-001]`：模板不得自成第二声明方言），只解决「声明从哪来」。
2. **状态权威落点：候选 B（独立 `nlos-plan` crate）。** 镜像 clock/topic/wait/application 既成模式：自有 SQLite、域分隔 PlanId/TaskNodeId 派生、immutable plan revision receipt（digest 链，对齐行 3648「修订产生新 revision/digest、已执行节点保留原 revision」）、trigger 守卫、幂等键。TaskAuthority 不并入 plan 表（A 否决理由：`planned-as` 分离对象不得字段堆叠耦合，写放大方向明确）。
3. **关联方式：TaskSpec 加最小关联字段。** 一次 additive 迁移为 TaskSpec 增加 `application_id` 与 plan revision 引用；运行期核验仍在物化/permit 边界按 ADR-0013 verify-then-commit 执行（字段存引用、边界做核验），不采用纯跨库引用（每次核验跨库成本高）也不采用运行期仅 slice 编排层维系（F1 缺口即由此而来）。
4. **ScaleProfile 语义：`max_task_nodes` 正规化为 TaskNode（plan_nodes）持久计数；Task 注册上限保留为独立第二维度。** TASK_PROFILE_10K 已发布数字在 evidence 中注明口径切换并按验收门 G5 重跑对齐基线量级；禁止新旧行为混写不注明。
5. **Resolver 结果 durable：采纳。** 依赖解析落 immutable 解析 receipt（typed selector → 带版本/generation 的 handle，`[PLAN-DEPENDENCY-001]`），可审计、可 fence；不采用纯查询返回（G4 审计面弱化）。
6. **晋升路径：本 ADR 即定案载体，ACCEPTED 即进入实现。** 实现车道按[进度单 §6.5.3](../stage-b-progress.md) 波次编排执行（W28-A 起声明面车道）；验收语义门沿用议题 35 §6 的 G1–G6（证伪条件逐门落测试）。

## 后果与退出策略

- 影响：新 crate `nlos-plan` 进入 workspace；`SliceKRuntime` 组装器增一权威；`nlos-task` 一次 additive schema 迁移 + ScaleProfile 维度重绑；`nlos-application`/`nlos-artifact` 触碰 manifest 解析与验签 golden（旧包兼容负路径必须测试）。
- 代价：物化涉及 plan/task/resource 三方权威，fence 顺序与故障矩阵成本高于同库方案（A）；显式接受。若 Planner/Resolver 闭环实验（行 5002 承认尚无）暴露 schema 形状错误，按「新 revision 演进」纠偏，不回迁 A。
- 退出策略：组合两半各自可否决——若 manifest 模板段在 W28-B 实现中证伪（验签/兼容不可行），C 半边降级为「模板存于安装后 Application 数据」，B 半边不受影响；若 nlos-plan authority 在 W28-A/W31 验收中证伪 G1/G2/G5，收缩路径为议题 35 候选 D 并重开 ADR。
- 证据边界：本 ADR 为设计定案（DESIGN 级），无实现证据；G1–G6 语义门与数字指标由实现车道在 evidence 落档后方可声称。

## 复审触发器

1. Planner/TypedIntent 自动分解管线启动时（行 5000 承认尚无 Planner），复审模板段与 proposal schema 是否覆盖 NL 分解产物。
2. 100K TaskNode benchmark（G2/G5）若惰性有界不成立，重开落点决策。
3. manifest 验签 golden 扩段若造成旧包兼容破坏且不可 additive 修复，触发 C 半边降级评审。
4. plan/task/resource 三方 fence 故障矩阵若发现不可收敛窗口，复审物化门权威归属。
5. 阶段 B 退出评审时随六门 Evidence review 一并复审本 ADR 的证据边界。
