# ADR-0023：语义写入端——driver 回调→语义事件桥（解除 D6 后置）

- 状态：`VERIFIED`（修订 1 追认实然实现，2026-10-06；原 ACCEPTED 2026-10-05）
- 日期：2026-10-05
- Owner：nlos-slice-k / nlos-semantic
- 裁断来源：2026-09-25 决策 D6（"Capability 最小装配+写端后置"）——后置理由"先接 Capability 实例"自 W40-D/W48 起已失效；用户授权"按推荐裁定"

## 上下文

两轮审查登记：`SemanticAuthority` 的 `append_*` 家族（assertion/spec 等）**零生产调用方**——语义权威能收事件（admission+outbox 流水线完整）但生产中无人写。D6 当时的裁定 (c) 暂缓的理由是先装配 Capability 实例；该前置已于 W40-D（slice-k capability 装配）与 W48（CellHost）落地，且 ADR-0022 将补签发闭环。后置理由不再成立。

## 候选

- (a) driver 回调→语义事件桥：操作完成（prepare/activate/complete 终态）时，由 slice-k 侧桥接层以持有 capability 的身份追加一条 assertion（provenance 绑 operation/回执摘要）
- (b) 独立 ingest 服务——多一个常驻面，当前无消费者需求，否决
- (c) 维持后置——前置已满足，理由失效，否决

## 决定

取 **(a)** 最小桥：slice-k 的操作终态路径上，若配置了语义写端（缺省**关闭**=零行为变化），以 operator/服务 principal 持有的 semantic-append capability 走 `authorize_semantic`+`append_assertion`（captured inputs 绑相关 event/operation 摘要，taint 按来源标记）。事件形状最小化：一条操作终态 assertion（outcome、receipt 摘要、taint），不发明新事件类型。capability 准入失败=fail-closed 拒绝并类型化上报，不降级为静默不写。

## 后果与退出

- 正面：语义权威获得首个生产写端，admission→outbox→消费全链有真实流量；写端行为由 capability 门控（与 ADR-0022 闭环衔接）。
- 负面：操作路径新增可选写放大（缺省关闭缓解）；桥层成为 slice-k 的一个新职责点。
- 退出：若未来出现结构化语义源（agent 输出直写），本桥收窄为系统事件源之一，不废除。

## 验证与复审

实现车道：slice-k 桥 + capability 准入 + 缺省关闭/开启双路测试 + admission→outbox 端到端。复审触发：议题 19（语义计算地基）落地结构化写端时。

---

## 修订 1（2026-10-06）：前提更正与实然追认（additive，不改写上文）

**前提更正**：本 ADR 撰写时引用的"append_* 零生产调用方"出自 2026-09-25 审查结论，在入册时点（2026-10-05）已不成立——并行工作流已于 2026-10-03/04 将语义写端全家族落入 main：`c1b90a1`（W49/D6 payload 终态桥）、`f47f5e7`（W50 semantic_outbox 生产消费者）、`1f3e39f`（W51 capability consume 账本扣减）、`8d3fef0`（W53-B 预算 raise）。该家族未登记本仓进度台账，导致本 ADR 上下文失真。

**实然追认**：既有实现即本 ADR 的决定语义，原"缺省关闭+外部供给"规格作废（explicitly superseded）——canonical 形态为：桥在 `SliceKRuntime::open` 无条件装配（always-on）；桥**自签发 per-application 根能力**（较 operator 单根更细粒度）；consume 先扣费→admission 内 authorize→append，fail-closed 已文档化并有测试；幂等由 consume 账本承载。原规格中的"缺省关闭""外部供给"两 delta 登记为已考虑并否决（与 W51/W53-B 地基冲突，改造无收益）。

**状态升 `VERIFIED`**：证据=上述四提交 + 2026-10-06 定向复跑（`semantic_writer_bridge` 6/6、`payload_execution` 6/6、`semantic_consume_ledger` 3/3、`semantic_budget_raise` 6/6、`semantic_stream` 6/6，隔离 target 全绿）。

**复审触发器（修订后）**：议题 19 结构化写端立项时；或 always-on 装配出现需要关闭语义的新场景（届时增开 Option 装配须连 W51/W53-B 一并重排）。
