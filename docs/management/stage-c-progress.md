# 阶段 C 权威进度单（草案）

> **状态：`IN_PROGRESS / NOT EXITED`（2026-09-22 批准生效）。决策点 0：维护者在退出包与草案呈批后两次回复「继续」——按本会话 W34-D 先例（「继续」= 批准 + 透明记录）批准本计划；决策点 1–5 由控制器按草案默认/最低风险读法代定，**否决窗口留至 W36 屏障**（任何一条可单点否决回滚，见 §C.5.4 裁定行）。** 本文件为 stage-b-progress §6.5 的后继编排草案：由 17 项移交清单（[stage-b-progress §6.5.6](./stage-b-progress.md)）与 [v0.5 §28.3 阶段 C 定义](../design/06-架构设计总纲-v0.5.md) 构造。起草基线 HEAD `ebbeddc`（2026-09-21）；控制器评审修订后于 `053f523` 落库（起草期"未提交"边界已由控制器集成收口）。
>
> 评审输入（只读引用）：三份门评审记录 [W31-G](../evidence/stage-b/reviews/w31g-road-b004-gates.md)、[W33-H](../evidence/stage-b/reviews/w33h-road-b001-b002.md)、[W34-A](../evidence/stage-b/reviews/w34a-six-gate-matrix.md) + 退出未知风险清单 [exit-unknown-risks.md](./exit-unknown-risks.md)（W34-C，评审包第四输入；`reviews/` 目录现存三份记录，W34-C 清单为其随附）。ADR-0013/0016/0017；[risks.yaml](./risks.yaml)（P0 条款）；[claims.yaml](./claims.yaml)。
>
> 规范留白处的处理纪律：凡 v0.5 未规定而本计划必须排程的主题（PoC 拓扑、优先序、移交项归置等），一律进入 §C.5.4 维护者决策点并给候选，**不发明规范**；决策点关闭前对应车道不派发（信息依赖，沿 §C.4 派发纪律 (4)）。

## C.1 阶段目标与非目标（源自规范）

### C.1.1 目标

阶段 C 把 NLOS 从单机通用应用平台（阶段 B 已 `EXITED`，2026-09-21 W34-D）推进到分布式现代系统。[v0.5 §28.3](../design/06-架构设计总纲-v0.5.md) 必须交付（逐字誊录）：

- 多节点 identity/epoch/fencing；
- quota lease 与 reconciliation；
- Artifact/event sync；
- placement、migration、federation；
- TaskAuthority 分片、层级 fanout/reducer、Capacity/Device lease 与 bulkhead；
- Global/Cell/Worker SchedulerDomain、work stealing、context affinity、跨 Cell pressure/backpressure；
- 分区与恢复语义。

退出门（§28.3 逐字誊录）：

- `[ROAD-C-001]` 分区、重启、迁移和重复消息下有明确一致性、额度冻结和损失上界。
- `[ROAD-C-002]` 海量 Agent benchmark MUST 证明 spawn/message/commit 不依赖全局锁或全局总序，并验证 stale TaskAuthority epoch、分区双主、fanout storm、provider stall 与 Cell bulkhead。

规范性锚点（实现车道据此落 Requirement 级测试，本计划不复制全文）：

| 锚点 | 内容 | 服务于 |
|---|---|---|
| v0.5 §26.1 蜂窝式权威 | Cell 本地七件套（supervisor / capability-name cache / Resource lease 子账本 / Driver gateway / durable event-outbox / Artifact cache / failure detector）与控制面七职责（Principal/key 与 policy、Package/Application trust、quota lease/epoch/fencing、placement/migration intent、跨 Cell 名称与服务发现、reconciliation 与审计 checkpoint）；`DIST-LOCAL-001`、`DIST-FAIL-001`、`DIST-NAME-001`、`DIST-TASK-001..004`、`DIST-FANOUT-001`、`DIST-BULKHEAD-001`；TaskAuthorityParticipantRegistry / TaskAuthorityAssignment / TaskAuthorityTakeoverReceipt 对象模型 | C-CELL、C-SHARD |
| v0.5 §26.2 Artifact 与事件同步 | `DIST-EVENT-001`（只合并签名事件集合，不 CRDT 造语义）、`DIST-ART-001`（按内容类型声明 merge/conflict/authoritative replica/deletion）、`DIST-AUDIT-001`（跨域保存生产者 signature + AdmissionReceipt + trust snapshot） | C-SYNC |
| v0.5 §26.3 Process 迁移 | `DIST-MIGRATE-001`（quiesce/cancel → checkpoint → 源 fencing → Capability rebind → Lease close/regrant → 目标 restore → 新 generation → Receipt）、`DIST-MIGRATE-002`（pinned session 明确拒绝或降级 restart-from-artifact） | C-MIGRATE |
| v0.5 §12 | QuotaLease / CapacityLease / ExclusiveDeviceLease 三族状态机与 `LEASE-GRANT-001`、`LEASE-SPEND-001`、`LEASE-REPORT-001`、`LEASE-TTL-001`、`LEASE-FENCE-001`、`LEASE-CLOSE-001`、`LEASE-LOSS-001`、`LEASE-MIGRATE-001` | C-LEASE |
| v0.5 §27 性能与工作负载类别 | `PERF-CLASS-001`、`PERF-BENCH-001`（durable barrier 不可缺）、`PERF-SCALE-001/002`、`PERF-CONTROL-001`（大规模手动控制面可用性） | C-BENCH |
| v0.5 §30 证据分级 | H6 = 分布式故障注入（阶段 C 核心证据形态）；`EVID-SCOPE-001` 单 claim 单证据范围 | §C.5.5 退出门 |
| v0.5 行 2808 `SEM-CHECKPOINT-001` | 分布式 View MUST 使用签名 vector/checkpoint，不得假设跨 Cell 全局标量 log_seq | C-SYNC |

单机基础已在阶段 B 落地、构成 C 的迁移起点（非重做）：TaskAuthority lease/term/fencing、FROZEN_FOR_TAKEOVER、per-endpoint barrier、exact-fence manifest、successor registry、cross-term adoption（stage-b-progress §4.85–§4.102 表组族，schema v27–v38）；统一恢复面双域自动收敛（[B-TASK-008C2G-UNIFIED-RECOVERY](../evidence/stage-b/b-task-008c2g-unified-recovery.md)）；ADR-0017 三域 coordinator。跨机/跨 Cell 半边 = RISK-B-11 显式未证域。

路线位置（v0.5 §28）：C 与 D 在 B 之后**并行**，非串行瀑布；安全、兼容、证据贯穿全部阶段。

### C.1.2 非目标（同样源自规范，不是本计划的裁剪）

1. **永久非目标**（v0.5 §29.1 逐条有效）：不替代硬件内核；不绑定单一模型/provider/harness/MCP/A2A；内核不判断自然语言真假；不保证 Agent 正确/Task 成功；raw NL 不是唯一编程接口；不隐藏远程调用的部分失败/信任边界/成本；治理与定价不入内核；不要求所有 Application 是聊天界面。
2. **v0.5 延后但保留接口**（§29.2，`NLOS-DEFER-001`）：完整消费级桌面、**全球联邦与跨组织清算**、生产级 Application Store、完整硬件 Driver 生态、长期二进制兼容承诺、裸机运行——C 不得把这些实现掉，也不得从对象模型中删除。与 §28.3「federation」交付项的关系属规范内部张力 → **维护者决策点 5**（见 §C.5.4）。
3. **阶段 D 本体不并入**（v0.5 §28.4）：完整 Task Space/窗口/多模态/System Control Center 等。移交 #15 中「完整桌面（Task Space 全量枚举 IPC 面、五层 desktop 派发）」按规范属 D 追踪面 → 归置确认在决策点 3。
4. **阶段 E 本体不并入**（v0.5 §28.5）：长期兼容政策、Store 治理、市场。
5. **生产量级声明与 C 退出解耦**（承 B 各门原文禁令）：「PID 级 Agent 容量」「coroutine 级大规模并发」以 release-profile + 多平台复测为前置（移交 #5 / RISK-B-07 / U-1）；该复测预计在 C 时间窗执行，但其闭环是**声明前置条件**，不是 C 退出门条款（除非决策点 3 另有裁定）。
6. 单机结论不外推：跨机/多 Cell 原子提交、跨 Cell barrier、远端物理 cleanup 在 C 证据落地前不可声称（RISK-B-11；ADR-0013 决定 3 登记的扩展点）。

## C.2 状态语义

沿 stage-b-progress §2 全套（`DONE` / `PARTIAL_PASS` / `IN_PROGRESS` / `READY` / `BLOCKED` / `NOT_STARTED`；证据等级 H0–H8；`PARTIAL_PASS` 只能声称 Evidence 覆盖的局部范围），不重复定义。新增阶段级两态：

| 阶段级状态 | 含义 |
|---|---|
| `IN_PROGRESS / NOT EXITED` | **当前态**（2026-09-22 决策点 0 批准生效）；至 §C.5.5 全条件满足并获明确批准前不得翻 `EXITED` |

**P0 条款（承 risks.yaml 头部纪律，逐字有效）**：P0 阻止阶段退出；P1 须有缓解措施与复查点；status 只反映已有 Evidence 支撑的缓解现状，无证据支撑的缓解不得写 mitigated。当前 P0=0（W34-C 确认）。

**常设门（standing gate，逐波屏障复查）**：RISK-B-12 升级条款已于 2026-09-21 裁决关闭（根因 = W22-001 install-scoped 前缀收集预埋孤儿，回执级证据排除误收在册 blob，**不升 P0**；处置 merge `87ef288`，[B-SLICE-K-001 §17](../evidence/stage-b/b-slice-k-001-end-to-end.md)）——条款文本存档于 risks.yaml/exit-unknown-risks 作为同类条款范本。存续常设门：夜间 scale-probe 绿（移交 #14 复证 PENDING）与 P0=0 维持。

## C.3 当前工作包总览

种子 = §6.5.6 移交项 #1–17 + v0.5 §28.3 C-core。状态以起草基线 HEAD `ebbeddc` 为准（#13/#14 已有退出后进展，如实登记）。

### C.3.1 C-core 工作包（阶段 C 本体）

| ID | 工作包 | 规范锚点 | 状态 |
|---|---|---|---|
| `C-CELL` | Cell 本地七件套 + 控制面最小面 + 多节点 identity/epoch/fencing | §26.1（Cell/控制面职责表 + `DIST-LOCAL/FAIL/NAME-001`） | `NOT_STARTED`（决策点 1 前置） |
| `C-LEASE` | Quota/Capacity/ExclusiveDevice lease 三族 + reconciliation | §12 全部 `LEASE-*` 不变量 | `NOT_STARTED` |
| `C-SYNC` | Artifact/event sync + 跨域审计 checkpoint | §26.2；`SEM-CHECKPOINT-001`（行 2808） | `NOT_STARTED` |
| `C-SHARD` | TaskAuthority 分片与跨 Cell 接管（单机表组族向 DIST-TASK-001..004 全语义迁移） | §26.1 `DIST-TASK-001/002/004`；对象模型三 Record | `NOT_STARTED`（跨 Cell 提交语义 ADR 前置，决策点 4） |
| `C-FANOUT` | 层级子组/局部 reducer/Merkle result root + fanout admission + bulkhead | `DIST-TASK-003`、`DIST-FANOUT-001`、`DIST-BULKHEAD-001` | `NOT_STARTED` |
| `C-MIGRATE` | placement、Process 迁移、federation 机制面 | §26.3 `DIST-MIGRATE-001/002`；§26.1 控制面 placement/migration intent | `NOT_STARTED` |
| `C-SCHED` | Global/Cell/Worker SchedulerDomain、work stealing、context affinity、跨 Cell pressure/backpressure | §28.3 第 6 条；SchedulerDomainId 类型（§33） | `NOT_STARTED` |
| `C-PARTITION` | 分区与恢复语义（一致性、额度冻结、损失上界） | `DIST-LOCAL-001`、`DIST-FAIL-001`；ROAD-C-001 | `NOT_STARTED` |
| `C-BENCH` | 分布式海量 Agent benchmark（全局锁/总序反证 + 五类故障验证） | ROAD-C-002；§27 `PERF-*` | `NOT_STARTED` |

### C.3.2 移交项工作包（§6.5.6 #1–17）

| ID | 移交# | 事项（§6.5.6 原文要点） | 当前状态与证据 | 主要未决项 |
|---|---|---|---|---|
| `C-REG-12` | #1 | slice-k-demo STEP 09d defect 根因处置（collected_digests==[] panic） | **`DONE`**（2026-09-21：merge `87ef288`——根因 = W22-001 install-scoped 前缀在 fail-closed 拒绝前收集预埋孤儿（登记嫌疑 W28-E 归因修正）；回执级证据排除误收在册 blob，RISK-B-12 裁决不升 P0 并 closed；demo 拒绝探针改 `AutoOrphanGc::Disabled` + 回归测试；demo e2e 绿 + 136 passed；[B-SLICE-K-001 §17](../evidence/stage-b/b-slice-k-001-end-to-end.md)） | 无 |
| `C-APP-PAYLOAD` | #2 | 载荷执行面 + `nlos package install` CLI | `NOT_STARTED`（W33-H §2 边界 1/3：manifest executable 字节不被真实执行；消费端为库驱动） | 内核侧载荷执行车道 + CLI 子命令 |
| `C-GUI-CAMPAIGN` | #3 | GUI 真机战役（computer-use 清单）+ Windows GUI | `NOT_STARTED`（W34-A §5；RISK-B-06/U-3） | 归置（决策点 3：D 邻接但为 B 交付物的可用性验证） |
| `C-WIN-KILL` | #4 | Windows live-child 实杀（taskkill /F/T 成功路径无真实子进程断言） | `NOT_STARTED`（W34-A §6 residual 4 精确口径；RISK-B-10/U-2） | Windows 实机 kill 矩阵 + B2-1 双活三层场景 |
| `C-SCALE-RELEASE` | #5 | release-profile + Linux/Windows 规模复测与 CI 化 | `NOT_STARTED`（RISK-B-07/U-1；全部规模数字为 debug/test 单平台 macOS） | 生产量级声明前置（PID 级/coroutine 级） |
| `C-RUNTIME-SCALE` | #6 | 100K 级 cancel/batch-cancel 探针、多 worker wake fairness、端到端墙钟分布 | `NOT_STARTED`（W34-A §6 residual 2） | O(n²)/O(n) 终态 purge 特征家族探针 |
| `C-SELECTOR` | #7 | G4 生态 selector 半边（Package/Skill/Tool/Model/Artifact/Topic/外部服务）；G3 Namespace/ResourceContract/fanout 从 digest 升结构化 | `NOT_STARTED`（W31-G §8.2.2/§8.2.3；U-7/U-8） | 各面 typed selector → generation handle 解析 + 负路径；三条件权威落地 |
| `C-PLAN-HARDEN` | #8 | apply 侧 TaskNode admission consult；Task-reclaim × plan-residency 互连；PINNED tier；调度器自身规模探针；100K@50% cell 与回收再入场 | `NOT_STARTED`（W31-G §8.2.4–§8.2.7；U-11） | 逐项补跑/接线 |
| `C-PROVIDER-REAL` | #9 | provider 真实载体替换 mock + transport 跨平台 + payload codec 冻结通道 | `NOT_STARTED`（U-10；codec 决策沿 RISK-B-05 review_point） | 首个真实 provider 全链接入；codec 入 ADR-0014 通道显式决策 |
| `C-RUNTIME-PROC` | #10 | kill receipt 消费 × Activation meter 联动；B6-4 跨平台 supervisor；B6-5 完整 BirthDecision | `NOT_STARTED`（W34-A §6 residual 3） | 三子项 |
| `C-APP-CONTROL` | #11 | Application 层控制面与生命周期 NL 动词；supervisor 自动 pid 发现/unregister | `NOT_STARTED`（W34-A §5：七层中 Application 层缺位） | ControlCommand arm + NL 白名单 |
| `C-LIFECYCLE` | #12 | `restore_process` 复活链、干净退出终态、teardown 并发竞争面、teardown/NL kill 幂等键同源 | `NOT_STARTED`（W33-H §4 行 4/5 沿引） | 四子项 |
| `C-CONFORMANCE-GOLDEN` | #13 | TS/Python conformance 对 SABI v1.2–v1.5 新臂/新视图 golden 钉死 | **`DONE`**（2026-09-21：merge `5c6f4c7`，[B-SCHEMA-002 §6](../evidence/stage-b/b-schema-002-cross-language-generation.md) 收口证据；U-14 退役） | 无（后续新增 SABI 臂随波屏障钉死即可） |
| `C-SCALE-PROBE` | #14 | 夜间 scale-probe 既有失败专项排查 | **根因修复已合入 main**（merge `ebbeddc`：测量窗口串行化 + 基线相对界；[B-RUNTIME-002 §6.18](../evidence/stage-b/b-runtime-002-fiber-scale.md)：测量方法学缺陷 + 10K 档 `+2` 标定错误，本地双模式 3/3 绿）——**PENDING 下一个 schedule run 复证**（§6.5.6 #14 行原文） | 夜间 schedule run 绿后回填关闭（W35 常设门槽位） |
| `C-MULTICELL` | #15 | 多 Cell / 分布式（阶段 C 本体）；Notification/Search 超最小面扩展；完整桌面 | 前半 = §C.3.1 C-core 全族；后半两件**归置未定**（决策点 3：Notification/Search 超最小面规范未指派阶段；完整桌面按 §28.4 属 D） | 决策点 3 |
| `C-POWER-LOSS` | #16 | 真实硬件掉电与 M4/M6/M8 模型校准（层 3+） | `NOT_STARTED`（U-4；层 1 APFS 校准/层 2 dm-flakey run 33895972272 已有） | 真机掉电设备或校准数据；时序归决策点 3 |
| `C-CUSTODY-GATEWAY` | #17 | 生产 signing key custody / enforcement-gateway reconciliation authority | `NOT_STARTED`（ADR-0017 约束 4 的终态承载；复审触发器 2 联动） | owner 侧自动结算（R-B 受限复活）评估随 C-LEASE reconciliation |

## C.4 诚实纪律与派发纪律（承阶段 B，逐条有效）

1. **Claim ≤ Evidence**：能力声明只来自已提交 Evidence；设计文档未来时态不得进入能力声明（README §10）；`EVID-SCOPE-001` 单 claim 单证据范围，H4/H6 某项通过不自动晋升其他 claim；负面结果与适用限制同等持久化（`EVID-NEG-001`）。分布式声明额外受 §C.1.2 第 6 条约束（单机不外推）。
2. **机器台账**：claims/risks/evidence-index + `scripts/lint_claims.py` 机械门逐波运行。**登记项**：evidence-index schema v1 索引域限定 `docs/evidence/stage-b/`（[exit-unknown-risks.md §5](./exit-unknown-risks.md)）——首个 stage-c evidence 落档前必须扩域（schema v2：`docs/evidence/stage-c/`）并过 lint，登记为 W35-C 半边；risks.yaml 新增条目沿其头部纪律（九类、保守评级、P0 阻退）。
3. **VERIFIED 就绪才解锁依赖**：入度以 **VERIFIED**（定向门通过 + 审查 clean）计零，仅 completed 不触发解锁。
4. **派发纪律（[stage-b-progress §6.5 派发纪律 2026-09-20 补充](./stage-b-progress.md) 全文沿用，mutatis mutandis）**：车道派发按真依赖就绪集推进、不按波次批次——(1) 入度以 VERIFIED 计零；(2) 写集锁：写集与任一 in-flight 车道相交即不可派，共享文件（含本进度单）按渐进式披露 §6.1 单一 integrator 合并、禁止并行双写；(3) 就绪集内关键路径优先（最宽铺开不保证最短 makespan）；(4) 跨波晋升：依赖全部满足（含已 ACCEPTED 的决策类信息依赖）且写集空闲可晋升进当前波，须增量日志留痕；依赖未决结论（如本计划决策点 1–5）的车道不得提前；(5) 波次屏障保留为验证/记账/重规划点，不因提前解锁豁免；(6) 唯一主线 `IN_PROGRESS` 工作包约束不变——晋升车道并入当前波记账。决策类信息依赖在本阶段的实例即 §C.5.4 决策点 0–5。
5. **波次屏障固定动作（PD-ORCH-004 同款）**：全部车道定向门通过 → 全仓 `cargo test --workspace --no-fail-fast` + fmt/clippy（`--all-targets --all-features`）双 0 → 本表与增量日志同步 → 三平台 CI/Pages/MSRV 复验并回填 run 链接 → **常设门复查**（RISK-B-12 升级条款未触发确认 + 夜间 scale-probe 绿 + P0=0）。
6. **原子提交与写集纪律**：AGENTS.md 第 8–10 条 + README §10 + 渐进式披露 §8 全文有效（单一 canonical 结果对应可解释原子提交；只暂存本 Task/Attempt 写集；禁 `git add -A`；HEAD 漂移复查；禁 amend/rebase/force-push 他人历史；push/CI/发布分别确认）。
7. **子 Agent 编排**（AGENTS.md 第 11 条 + 渐进式披露 §6.1）：独立 Task/Attempt、读集/写集/验收条件显式；写集不相交车道并行波次；共享 canonical 文件串行或单一 integrator 合并；波间屏障；单车道失败只阻塞其依赖者。

## C.5 阶段 C 编排（W35 起 → 阶段 C 退出门）

> 编号沿用 W 序列（B 收官于 W34，C 自 W35 起保持连续）；工具链波次沿用 T 序列互不占用。
>
> 滚动细化纪律（承 B §6.5 同款）：W36+ 现为**主题级占位**——决策点 1–5 关闭后细化到车道级（B 的先例：议题 35 经 ADR-0016 定案后解除 W28+ 占位）；每波屏障后允许重排后续波车道，已登记车道的变更/删除必须在增量日志留痕。本节只承诺编排不承诺完成。

### C.5.1 剩余工作总清单

**甲组：移交项 #1–17（来源：§6.5.6；评审 residual 溯源：W31-G §8.2 / W33-H §3.2/§4 / W34-A §8 / exit-unknown-risks U-1..U-14 + §4 已决定递延项）**

| # | 事项 | 归置（建议，决策点 3 终裁） | 波次槽位（建议） |
|---|---|---|---|
| 1 | 09d defect 根因处置（RISK-B-12 升级条款随行） | **已收口**（`87ef288`，RISK-B-12 closed） | 无（W35 复核登记即可） |
| 2 | 载荷执行面 + `nlos package install` CLI | C 册 | W36+ 晋升候选 |
| 3 | GUI 真机战役 + Windows GUI | 待归置（D 邻接） | 决策点 3 定 |
| 4 | Windows live-child 实杀 | C 册（RISK-B-10 退役证据） | W36+ 晋升候选 |
| 5 | release-profile 多平台规模复测 + CI 化 | C 册（生产声明前置） | W36 |
| 6 | 100K cancel/batch-cancel、wake fairness、墙钟分布 | C 册 | W36 |
| 7 | G4 生态 selector；G3 三条件结构化 | C 册（单机加固，与 C-core 写集多数不相交） | W36–W37 |
| 8 | apply 侧 consult、reclaim×residency、PINNED、调度器探针、100K@50% | C 册 | W36–W37 |
| 9 | provider 真实载体 + transport 跨平台 + codec 冻结决策 | C 册（codec 决策沿 RISK-B-05 review_point 晋升 ADR） | W37+ |
| 10 | kill receipt×meter 联动、B6-4 supervisor、B6-5 BirthDecision | C 册 | W36+ |
| 11 | Application 层控制面 + NL 动词 + supervisor unregister | C 册 | W36+ 晋升候选 |
| 12 | restore_process 复活链、干净退出、teardown 竞争、幂等键同源 | C 册 | W37+ |
| 13 | TS/Python golden 钉死 | **已收口**（`5c6f4c7`） | 无（W35 复核登记即可） |
| 14 | 夜间 scale-probe 复证 | C 册（常设门槽位） | **W35-B（必选）** |
| 15 | 多 Cell/分布式（C 本体）/ Notification-Search 扩展 / 完整桌面 | 前者 = C-core；后两者待归置 | 决策点 3 定 |
| 16 | 真实掉电层 3+ 与 M4/M6/M8 | C 册（时序待定） | 决策点 3 定 |
| 17 | 生产 custody / enforcement-gateway authority | C 册（与 C-LEASE reconciliation 联动） | W38+ |

**乙组：C-core（来源：v0.5 §28.3 必须交付 + §26/§12/§27 锚点，见 §C.3.1）**

| ID | 事项 | 规范锚点 |
|---|---|---|
| C-CELL | Cell 七件套 + 控制面最小面 + 多节点 identity/epoch/fencing | §26.1 |
| C-LEASE | 三族 lease + reconciliation | §12 |
| C-SYNC | artifact/event sync + 审计 checkpoint | §26.2、行 2808 |
| C-SHARD | TaskAuthority 分片/跨 Cell 接管全语义 | §26.1 DIST-TASK-001..004 |
| C-FANOUT | 层级 fanout/reducer + admission + bulkhead | DIST-TASK-003/FANOUT-001/BULKHEAD-001 |
| C-MIGRATE | placement/migration/federation 机制 | §26.3 + §26.1 控制面 |
| C-SCHED | 三层 SchedulerDomain + 跨 Cell pressure/backpressure | §28.3 |
| C-PARTITION | 分区恢复语义（ROAD-C-001 本体） | DIST-LOCAL/FAIL-001 |
| C-BENCH | 分布式 benchmark（ROAD-C-002 本体） | §27 |

### C.5.2 关键依赖链

```text
决策点 0（本计划批准）→ §C.2 翻转 IN_PROGRESS / NOT EXITED
  → W35 承接收口波（#1 09d 处置 + #14 夜间复证 + 决策点 1–5 收口 + evidence-index 扩域）
决策点 1（多 Cell 拓扑 PoC 顺序，晋升 ADR）
  → C-CELL 骨架（Cell 本地件套 + 控制面最小 + identity/epoch/fencing）
  → { C-LEASE（quota/capacity/device + reconciliation）、
      C-SHARD（DIST-TASK-001..004；跨 Cell 提交语义 ADR = 决策点 4 前置，ADR-0013 复审触发器）、
      C-SCHED（三层 SchedulerDomain） }
C-SHARD + C-LEASE → C-MIGRATE（placement/migration/federation，DIST-MIGRATE-001/002）
C-CELL + C-SYNC（DIST-EVENT/ART/AUDIT + SEM-CHECKPOINT-001）
  → C-PARTITION（分区一致性/额度冻结/损失上界）
  → ROAD-C-001 证据
C-CELL + C-SHARD + C-SCHED + C-FANOUT（bulkhead/storm 面）
  → C-BENCH（全局锁/总序反证 + stale epoch/分区双主/fanout storm/provider stall/bulkhead 五类）
  → ROAD-C-002 证据
单 Cell 加固族（#4/#5/#6/#7/#8/#10 等，W36 起与 C-core 按写集不相交并行）
#17 custody/gateway → 与 C-LEASE reconciliation 联动（ADR-0017 复审触发器 2）
全门 Evidence 齐（H6 维度）→ 台账 stage-c 域回填 → 未知风险清单 + P0=0 → 阶段 C 退出评审（§C.5.5）
```

### C.5.3 波次编排（W35 起；W36+ 主题级占位）

**W35（承接收口波）**——三固定车道写集两两不相交：

| 车道 | 事项 | 主要写集 | 验收门 |
|---|---|---|---|
| W35-A | 移交#1 复核登记（处置已于 2026-09-21 完成：merge `87ef288`，RISK-B-12 裁决不升 P0 并 closed） | docs（本表 + §6.5.6 对账） | §C.3 与 §6.5.6 #1 状态一致；常设门条款存档登记 |
| W35-B | 移交#14：夜间 scale-probe 复证收口 | docs（B-RUNTIME-002 §6.18 回填 + §6.5.6 #14 行关闭） | 下一个 schedule run 绿 + run 链接回填；否则按失败如实登记并回 W35-A 同级处置 |
| W35-C | 本计划批准与治理半边：决策点 0–5 收口（docs）；evidence-index schema v2 扩域 `docs/evidence/stage-c/` + lint 适配 + 移交项 #1–17 状态逐项入册（含 #13 已收口复核） | docs/management（本文件、evidence-index.yaml、risks.yaml 如需） | 决策点各有裁定并留痕（晋升 ADR 者开档）；lint 全绿；本表 §C.3 与 §6.5.6 对账一致 |

W35 晋升候选池（依派发纪律 (4)，决策点关闭 + 写集空闲即可进 W35，增量日志留痕）：移交#4 Windows 实杀（RISK-B-10 退役证据）、移交#11 Application 层控制面前片、移交#2 载荷执行面前片——三者与决策点 1–5 无信息依赖、与 W35-A/B/C 写集不相交。

**W36（单 Cell 加固波，依决策点 2 排程）**：移交#5 release/多平台复测 + CI 化（`C-SCALE-RELEASE`）∥ 移交#6 100K cancel/fairness/墙钟（`C-RUNTIME-SCALE`）∥ 移交#7/#8 plan 生态与矩阵缺口（`C-SELECTOR`/`C-PLAN-HARDEN`）∥ 移交#10（`C-RUNTIME-PROC`）——车道级细化待 W35 屏障后。

**W37（Cell 骨架波，依决策点 1 + 拓扑 ADR）**：`C-CELL` 前片（Cell 本地件套最小面 + 控制面最小面 + 多节点 identity/epoch/fencing）∥ 移交#9 provider 真实载体前片（若 codec 决策关闭）∥ 移交#12 lifecycle 族。

**W38（lease/quota 波）**：`C-LEASE`（§12 三族 + reconciliation）∥ 移交#17 custody/gateway 前片。

**W39（TaskAuthority 跨 Cell 波）**：跨 Cell 提交语义 ADR（决策点 4 → ADR-0013 扩展点定案）→ `C-SHARD`（DIST-TASK-001..004 迁移；单机 schema v27–v38 表组族为基础）∥ `C-FANOUT`。

**W40（sync 与分区恢复波）**：`C-SYNC` → `C-PARTITION` → **ROAD-C-001 证据评审**。

**W41（调度与迁移波）**：`C-SCHED` 完整面 + `C-MIGRATE`。

**W42（分布式 benchmark 波）**：`C-BENCH`（`PERF-BENCH-001` durable barrier 语义；`PERF-CONTROL-001` 大规模控制面）→ **ROAD-C-002 证据评审**。

**W43（退出评审波）**：两门证据矩阵补漏 → claims/risks 全量回填 + lint → 未知风险清单 + P0 清零确认 → §C.5.5 用户门 + §C.2 翻转 + 移交阶段 D/E 登记。

### C.5.4 决策点状态（2026-09-22 裁定生效；否决窗口至 W36 屏障）

裁定方式：决策点 0 = 维护者「继续」×2（W34-D 先例）；1–5 = 控制器按草案默认/最低风险代定（ADR-0017 模式：否决窗口留至下波屏障，任何一条单点否决即回滚该条并重排受影响波次）。

0. **本计划批准：已批准（2026-09-22）**——§C.2 已翻转；L0/README 入口同步随本裁定提交。
1. **多 Cell 拓扑：裁 (a) 单机多进程 Cell×2 起步**（迭代最快、分区可注入、回退成本最低；跨机语义后置到有单机双 Cell 证据后）。子议题：**共识基底 ADR 于 W37 拓扑 ADR 并行开档**（先以控制面单写者 + lease/fencing 覆盖语义，ADR 定是否/何时引入共识基底）。
2. **优先序：裁 (c) 并行双线，附保守波幅**——按写集并行（W36 加固波与 W37+ C-core 写集天然不相交），但**单波在飞车道 ≤3**（单人可承载的保守执行；原草案「须确认可承载」以该上限形式落实，否决窗口内可调）。
3. **归置裁定**：(i) 完整桌面→**D 册**（C 册只挂引用）；Notification/Search 超最小面→**显式 backlog**（规范未指派，不排入 C 波次；出现规范依据再入册）；(ii) GUI 真机战役→**C 册 W36 起的常设观察项、不占车道**（computer-use 战役需维护者本机，控制器只备清单）；(iii) 掉电层 3+→**持续项**（遇真机设备即执行，不设 C 期硬承诺）；(iv) 顺位确认——W35 已按写集完成 #2/#11 前片，后片随波次。
4. **跨 Cell 提交 ADR：裁 (b) W37 随拓扑 ADR 并行开档**（早启降低 W39 串行等待；C-SHARD 派发仍以其 VERIFIED 为前置，纪律不变）。
5. **federation 解释：确认草案读法**——C 交付机制面（跨 Cell 名称/服务发现、迁移 intent、审计 checkpoint），全球跨组织清算延后（§29.2）；C-MIGRATE 验收边界据此。

### C.5.5 阶段 C 退出评审门

§C.2 阶段级状态翻转为 `EXITED`，必须同时满足（结构承 §6.5.5，条款从 v0.5 推导）：

1. `ROAD-C-001`/`ROAD-C-002` 两门各有 Evidence review 记录（证据等级按 v0.5 H0–H8 分级；分布式故障注入 = H6 维度须实质性覆盖门原文故障类目——分区/重启/迁移/重复消息、stale epoch/分区双主/fanout storm/provider stall/bulkhead），无一 P0 未决风险；
2. claims/risks/evidence 机器台账 stage-c 域全量回填，Claim≤Evidence lint 通过；
3. 未知风险清单显式列出（含 B 继承项 RISK-B-11 等的退役/重定级判定），各项有 owner 与缓解措施；
4. [管理机制 §7](./README.md#7-评审与决策机制)「Stage 退出或 production claim」三件套（Evidence review + 未知风险清单 + 明确批准）完成；
5. §C.5.3 全部车道 DONE，或经批准显式移交（阶段 D/E 或持续项）并在本表登记；
6. 移交项处置终局登记：§6.5.6 #1–17 逐项有关闭证据或显式再移交——含 RISK-B-12 终局（常设门关闭）、#14 夜间绿、#5 生产量级声明前置闭环或维持限定口径（并按 §C.1.2 第 5 条明示其与 C 退出的关系裁定）。

## C.6 进度更新协议

承 stage-b-progress §7 全部条款（同一 canonical commit 同步本表状态/日期/commit/Evidence/未决项 + ADR/规范实现缺口 + 唯一主线 `IN_PROGRESS` + 相称测试复跑 + PARTIAL→DONE 补证据范围 + 反例降级保留），增加两条：

1. 常设门状态（RISK-B-12、夜间 scale-probe、P0 计数）随每波屏障在增量日志留痕；
2. §6.5.6 移交项的状态变化必须同步 stage-b-progress §6.5.6 对应行（#13/#14 先例：`98b6b60`/`5c6f4c7` 的 L0 同步模式）。

## C.7 关联权威入口

- 当前规范：[架构设计总纲 v0.5](../design/06-架构设计总纲-v0.5.md)（§26 分布式现代 NLOS、§12 分布式 lease、§27 性能、§28.3 阶段 C、§29 非目标）
- 阶段 B 权威进度单（移交清单 §6.5.6 + 派发纪律原文）：[stage-b-progress.md](./stage-b-progress.md)
- 管理机制：[README.md](./README.md)；知识规则：[project-knowledge-progressive-disclosure.md](./project-knowledge-progressive-disclosure.md)
- 跨 authority 契约与 Stage C 扩展点：[ADR-0013](./adrs/0013-cross-authority-verify-then-commit-contract.md)；声明面：[ADR-0016](./adrs/0016-task-plan-declaration-surface.md)；三域 coordinator 与 enforcement-gateway 终态：[ADR-0017](./adrs/0017-resource-operation-cross-authority-prepare-finalize.md)
- 机器台账：[claims.yaml](./claims.yaml)、[risks.yaml](./risks.yaml)、[evidence-index.yaml](./evidence-index.yaml)
- 风险与未知项：[exit-unknown-risks.md](./exit-unknown-risks.md)（U-1..U-14；§4 已决定递延项）
- 评审基线：[W31-G](../evidence/stage-b/reviews/w31g-road-b004-gates.md)、[W33-H](../evidence/stage-b/reviews/w33h-road-b001-b002.md)、[W34-A](../evidence/stage-b/reviews/w34a-six-gate-matrix.md)
