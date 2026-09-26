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

**常设门（standing gate，逐波屏障复查）**：RISK-B-12 升级条款已于 2026-09-21 裁决关闭（根因 = W22-001 install-scoped 前缀收集预埋孤儿，回执级证据排除误收在册 blob，**不升 P0**；处置 merge `87ef288`，[B-SLICE-K-001 §17](../evidence/stage-b/b-slice-k-001-end-to-end.md)）——条款文本存档于 risks.yaml/exit-unknown-risks 作为同类条款范本。存续常设门：夜间 scale-probe **schedule** 绿（移交 #14 的 workflow_dispatch 复证已闭合——dispatch run [35560466719](https://github.com/cty12356541/llmos/actions/runs/35560466719)，与 §6.5.6 #14 / [B-RUNTIME-002 §6.18.2.2](../evidence/stage-b/b-runtime-002-fiber-scale.md) 一致；**不得以 dispatch 绿冒充 schedule 绿**，schedule 常设门仍开）与 P0=0 维持。

## C.3 当前工作包总览

种子 = §6.5.6 移交项 #1–17 + v0.5 §28.3 C-core。状态以起草基线 HEAD `ebbeddc` 为起点；其后合入以本表各行 + [§C.8](#c8-增量日志w36w37-本地合入) 为准（禁止只改聊天）。

### C.3.1 C-core 工作包（阶段 C 本体）

| ID | 工作包 | 规范锚点 | 状态 |
|---|---|---|---|
| `C-CELL` | Cell 本地七件套 + 控制面最小面 + 多节点 identity/epoch/fencing | §26.1（Cell/控制面职责表 + `DIST-LOCAL/FAIL/NAME-001`） | **`PARTIAL_PASS` / 前片**（2026-09-22：identity/epoch/fencing 最小面合入 `4fe5854`，crate `nlos-cell`；W38-E 测试加固 `e9c252c`/`5c93394`——身份与 pid 解耦、双进程存活窗口；W38-CELL boot generation 持久化合入 `12d89e1`/`bb9b7cb`；clippy `too_many_lines` 拆分 `f3e83f6`/`1ad35d4`；[ADR-0018](./adrs/0018-single-host-multiprocess-dual-cell-topology.md) `ACCEPTED` ≠ `VERIFIED`。**七件套与其余控制面未做**、信息门仍开。不声称 C-CELL 已实现） |
| `C-LEASE` | Quota/Capacity/ExclusiveDevice lease 三族 + reconciliation | §12 全部 `LEASE-*` 不变量 | **`PARTIAL_PASS` / 前片**（2026-09-25 本地 HEAD：单 Cell 三族授予与过期 epoch 拒绝——Quota `ac898ec`/`7fa1fec` + admit `fc5d81b`/`6ec9528`；Capacity `9be119c`/`6225f4b`；ExclusiveDevice `ac2116c`/`9219bd8`；crate `nlos-lease`。2026-09-26 续片：Quota 单调高水位 / `ACTIVE→CLOSING→CLOSED` / `ISSUED` 取消 `df8abd6`；Capacity `GLOBAL_RESERVED→RETURNING→RETURNED` `43f2600`。**无独立 Evidence 文件**——Claim 止于单 Cell 三族授予加 Quota 消耗/关闭与 Capacity 预激活归还。**reconciliation / 全 §12 `LEASE-*` / Device reset 未做**。不声称 C-LEASE 已实现） |
| `C-SYNC` | Artifact/event sync + 跨域审计 checkpoint | §26.2；`SEM-CHECKPOINT-001`（行 2808） | `NOT_STARTED` |
| `C-SHARD` | TaskAuthority 分片与跨 Cell 接管（单机表组族向 DIST-TASK-001..004 全语义迁移） | §26.1 `DIST-TASK-001/002/004`；对象模型三 Record | `NOT_STARTED`（[ADR-0019](./adrs/0019-cross-cell-commit-semantics-extension.md) 已开档 `CANDIDATE` `b512fa7`；派发仍以其 `VERIFIED` 为前置，本 integrator 未实现任何 C-SHARD 切片） |
| `C-FANOUT` | 层级子组/局部 reducer/Merkle result root + fanout admission + bulkhead | `DIST-TASK-003`、`DIST-FANOUT-001`、`DIST-BULKHEAD-001` | `NOT_STARTED` |
| `C-MIGRATE` | placement、Process 迁移、federation 机制面 | §26.3 `DIST-MIGRATE-001/002`；§26.1 控制面 placement/migration intent | `NOT_STARTED` |
| `C-SCHED` | Global/Cell/Worker SchedulerDomain、work stealing、context affinity、跨 Cell pressure/backpressure | §28.3 第 6 条；SchedulerDomainId 类型（§33） | `NOT_STARTED` |
| `C-PARTITION` | 分区与恢复语义（一致性、额度冻结、损失上界） | `DIST-LOCAL-001`、`DIST-FAIL-001`；ROAD-C-001 | `NOT_STARTED` |
| `C-BENCH` | 分布式海量 Agent benchmark（全局锁/总序反证 + 五类故障验证） | ROAD-C-002；§27 `PERF-*` | `NOT_STARTED` |

### C.3.2 移交项工作包（§6.5.6 #1–17）

| ID | 移交# | 事项（§6.5.6 原文要点） | 当前状态与证据 | 主要未决项 |
|---|---|---|---|---|
| `C-REG-12` | #1 | slice-k-demo STEP 09d defect 根因处置（collected_digests==[] panic） | **`DONE`**（2026-09-21：merge `87ef288`——根因 = W22-001 install-scoped 前缀在 fail-closed 拒绝前收集预埋孤儿（登记嫌疑 W28-E 归因修正）；回执级证据排除误收在册 blob，RISK-B-12 裁决不升 P0 并 closed；demo 拒绝探针改 `AutoOrphanGc::Disabled` + 回归测试；demo e2e 绿 + 136 passed；[B-SLICE-K-001 §17](../evidence/stage-b/b-slice-k-001-end-to-end.md)） | 无 |
| `C-APP-PAYLOAD` | #2 | 载荷执行面 + `nlos package install` CLI | **`PARTIAL_PASS` / 前片**（install：2026-09-21 merge `953c8af` + [B-APPLICATION-007 §7](../evidence/stage-b/b-application-007-third-party-sample.md) VERIFIED；run/update/uninstall CLI：2026-09-25 merge `4c67ede`/`000436d`/`e406fdb`（`c362668`/`874385f`/`5bb60fc`）+ crate 测试；clippy `too_many_lines` 拆分 `e782c35`/`dcfc1d3`；**无独立 Evidence 文件**——Claim 止于 CLI 子命令前片） | 样例自身接线该车道；无 W38 Evidence 文件。owner：后续 C-APP-PAYLOAD 切片 |
| `C-GUI-CAMPAIGN` | #3 | GUI 真机战役（computer-use 清单）+ Windows GUI | `NOT_STARTED`（W34-A §5；RISK-B-06/U-3） | 归置（决策点 3：D 邻接但为 B 交付物的可用性验证） |
| `C-WIN-KILL` | #4 | Windows live-child 实杀（taskkill /F/T 成功路径无真实子进程断言） | `NOT_STARTED`（W34-A §6 residual 4 精确口径；RISK-B-10/U-2） | Windows 实机 kill 矩阵 + B2-1 双活三层场景 |
| `C-SCALE-RELEASE` | #5 | release-profile + Linux/Windows 规模复测与 CI 化 | **`PARTIAL_PASS` / 管线前片 VERIFIED**（2026-09-21：merge `09c0832`；[B-SCALE-RELEASE-001](../evidence/stage-b/b-scale-release-001.md)——`scale-probe-release` job + 同日同机首批 release↔debug 数字）。生产量级声明禁令维持 | 首 CI run `PENDING`（owner：控制器 post-merge dispatch / 夜间 schedule）。U-1 多平台半边待该 run 回填 |
| `C-RUNTIME-SCALE` | #6 | 100K 级 cancel/batch-cancel 探针、多 worker wake fairness、端到端墙钟分布 | **`PARTIAL_PASS` / 三探针前片 VERIFIED**（2026-09-21：merge `4df55d6`；[B-RUNTIME-002 §6.19](../evidence/stage-b/b-runtime-002-fiber-scale.md)——100K batch-cancel 比值 + 多 worker fairness + 墙钟分布首割） | durable-wait 全量挂起下 O(n²) 终态 purge 家族本身；100K wake 风暴。owner：后续 C-RUNTIME-SCALE |
| `C-SELECTOR` | #7 | G4 生态 selector 半边（Package/Skill/Tool/Model/Artifact/Topic/外部服务）；G3 Namespace/ResourceContract/fanout 从 digest 升结构化 | **`PARTIAL_PASS` / 前半 VERIFIED**（2026-09-21：merge `8b91419`；[B-PLAN-001 §12](../evidence/stage-b/b-plan-001-declaration-surface.md)——Application/Artifact typed selector + G3 三条件结构化） | Topic/Skill/Tool/Model/外部服务未发明（§12.5）；G3 enforcement 与节点声明对 resolution handle 的结构化绑定。owner：后续 C-SELECTOR 切片 |
| `C-PLAN-HARDEN` | #8 | apply 侧 TaskNode admission consult；Task-reclaim × plan-residency 互连；PINNED tier；调度器自身规模探针；100K@50% cell 与回收再入场 | **`PARTIAL_PASS` / 四 residual VERIFIED**（2026-09-22：merge `8ed592a`；终审 Ready；[B-PLAN-001 §13](../evidence/stage-b/b-plan-001-declaration-surface.md) + [B-TASK-SCALE-001 §15](../evidence/stage-b/b-task-scale-001.md)）。本 integrator 定向复跑：nlos-plan+nlos-task skip-10K/100K 名 **462 passed / 0 failed / 2 ignored / 8 filtered** | `nlos-system-control` 夹具仍调 `apply_plan_revision`（park，owner：后续 write-set / assembler）；PINNED 台账/SABI 字段与调度器 100K 为 brief 边界；`apply_plan_revision_ungated` / `UnlinkedReclaimResidency` 旁路 |
| `C-PROVIDER-REAL` | #9 | provider 真实载体替换 mock + transport 跨平台 + payload codec 冻结通道 | `NOT_STARTED`（U-10；codec 决策沿 RISK-B-05 review_point） | 首个真实 provider 全链接入；codec 入 ADR-0014 通道显式决策 |
| `C-RUNTIME-PROC` | #10 | kill receipt 消费 × Activation meter 联动；B6-4 跨平台 supervisor；B6-5 完整 BirthDecision | **`PARTIAL_PASS` / 三子项 VERIFIED**（2026-09-22：merge `c45bd10`；终审 Ready；[W36-P10](../evidence/stage-b/w36-p10-c-runtime-proc.md)）。本 integrator 定向复跑：runtime 三 crate skip-10K/100K 名 **185 passed / 0 failed / 4 ignored / 11 filtered**；kill_receipt 5 / birth_decision 7 / supervisor 6 / runtime-lib 2 | Windows `#[cfg(windows)]` supervisor 实杀臂未在本机跑（owner：C-WIN-KILL / 三平台 CI）；5 parked minors（test/API/docs polish） |
| `C-APP-CONTROL` | #11 | Application 层控制面与生命周期 NL 动词；supervisor 自动 pid 发现/unregister | **`PARTIAL_PASS` / 前片**（disable/uninstall：2026-09-21 merge `f49c5a8` + [B-CONTROL-003 W35-P11](../evidence/stage-b/b-control-003-nl-prefix.md) VERIFIED；inspect GET：2026-09-25 merge `ce06746`/`e8a3c4b`；supervisor 按代次 unregister：`7e975d7`/`1a8a073`——后两片 **无独立 Evidence 文件**，Claim 止于 inspect+unregister 前片） | supervisor 自动 pid **发现**仍开；不声称 Application 层控制面全包 DONE。owner：后续 C-APP-CONTROL 切片 |
| `C-LIFECYCLE` | #12 | `restore_process` 复活链、干净退出终态、teardown 并发竞争面、teardown/NL kill 幂等键同源 | **`PARTIAL_PASS` / 前片**（2026-09-25 本地 HEAD：`clean_shutdown`→`FiberExit::Completed` 合入 `cb51acb`/`6da024d`；`restore_process` 过期代次拒绝测试合入 `d391ba7`/`362fc11`；W38 spawn-refusal 观察 `6547039` 仍不解锁全包。**无独立 Evidence 文件**——Claim 止于干净退出 + stale-generation 测试前片。teardown 竞争面 / 幂等键同源 / 完整复活链未做） | 后两子项 + 完整复活链。owner：后续 C-LIFECYCLE |
| `C-CONFORMANCE-GOLDEN` | #13 | TS/Python conformance 对 SABI v1.2–v1.5 新臂/新视图 golden 钉死 | **`DONE`**（2026-09-21：merge `5c6f4c7`，[B-SCHEMA-002 §6](../evidence/stage-b/b-schema-002-cross-language-generation.md) 收口证据；U-14 退役） | 无（后续新增 SABI 臂随波屏障钉死即可） |
| `C-SCALE-PROBE` | #14 | 夜间 scale-probe 既有失败专项排查 | **根因修复已合入 main**（merge `ebbeddc`：测量窗口串行化 + 基线相对界；[B-RUNTIME-002 §6.18](../evidence/stage-b/b-runtime-002-fiber-scale.md)：测量方法学缺陷 + 10K 档 `+2` 标定错误，本地双模式 3/3 绿）——**dispatch 复证已闭合**（2026-09-22：与 §6.5.6 #14「复证全绿闭合」及 [B-RUNTIME-002 §6.18.2.2](../evidence/stage-b/b-runtime-002-fiber-scale.md) 对齐；dispatch run [35560466719](https://github.com/cty12356541/llmos/actions/runs/35560466719) 五 job 首全绿含 scale-probe；不再引用已过时的 PENDING 行原文） | **夜间 schedule 常设门仍开**（仅 workflow_dispatch 绿不得关 schedule 门；W35-B 仍要 schedule run 绿 + 链接回填） |
| `C-MULTICELL` | #15 | 多 Cell / 分布式（阶段 C 本体）；Notification/Search 超最小面扩展；完整桌面 | 前半 = §C.3.1 C-core 全族；Notification/Search 超最小面仍 backlog；完整桌面按决策点 3→**D 册**。D 前片已在 origin/main：五层只读 inspect `ee649d3`/`86fd713`；Surface open/close→`CLOSED` `d059ac2`/`49d3e14`；Surface `create`/`hide` `6165bc4`——**`PARTIAL_PASS` / D 前片**，**不声称 Stage D 余量 DONE** | C-core 仍开；D 余量未做 |
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
| W35-B | 移交#14：夜间 scale-probe 复证收口 | docs（B-RUNTIME-002 §6.18 回填 + §6.5.6 #14 行关闭） | **dispatch 半边已闭合**（run [35560466719](https://github.com/cty12356541/llmos/actions/runs/35560466719) / §6.18.2.2 / §6.5.6 #14「复证全绿闭合」）；**schedule 常设门验收仍开**：下一个 schedule run 绿 + run 链接回填；否则按失败如实登记并回 W35-A 同级处置 |
| W35-C | 本计划批准与治理半边：决策点 0–5 收口（docs）；evidence-index schema v2 扩域 `docs/evidence/stage-c/` + lint 适配 + 移交项 #1–17 状态逐项入册（含 #13 已收口复核） | docs/management（本文件、evidence-index.yaml、risks.yaml 如需） | 决策点各有裁定并留痕（晋升 ADR 者开档）；lint 全绿；本表 §C.3 与 §6.5.6 对账一致 |

W35 晋升候选池（依派发纪律 (4)，决策点关闭 + 写集空闲即可进 W35，增量日志留痕）：移交#4 Windows 实杀（RISK-B-10 退役证据）、移交#11 Application 层控制面前片、移交#2 载荷执行面前片——三者与决策点 1–5 无信息依赖、与 W35-A/B/C 写集不相交。

**W36（单 Cell 加固波，依决策点 2 排程）**：四车道任务门+终审已齐并**已合入 origin/main**（#5 `09c0832` / #7 `8b91419` / #10 `c45bd10` / #8 `8ed592a`；#6 已于 W35-P6 `4df55d6` 提前）。**W36 屏障未关**：三平台 CI / Pages / MSRV / nightly schedule 未关——不得声称屏障闭合。否决窗口仍开。

**W37（Cell 骨架波，依决策点 1 + 拓扑 ADR）**：A–E 文档/前片已合入 origin/main（ADR-0018 `c433d6f` / ADR-0019 `b512fa7` / cell spec `e485450` / ADR-0020 `705d4d1` / nlos-cell `4fe5854`）。移交#9 仍信息门（codec 未冻结）；#12 生命周期全包未做（W38 仅合入 spawn-refusal 观察测试 `6547039`，不解锁 #12）。七件套切片与 C-SHARD **未实现**（信息门：0018/0019 均非 VERIFIED）。

**W38（lease/quota + 移交后片）**：本地 main 与 origin/main 对齐（ahead/behind 0；2026-09-25）；范围 `ee649d3..194b014` 已由 bound SSH ls-remote 核实在 origin；台账提交 `3c5cc9a` 亦在 origin；权威 tip 以 `git ls-remote origin refs/heads/main` 为准，本文件不冻结 tip SHA。已合入 origin 的前片：`C-LEASE` 单 Cell Quota+Capacity+ExclusiveDevice（`ac898ec`/`9be119c`/`ac2116c`）；boot generation（`12d89e1`/`bb9b7cb`）；#12 干净退出 + restore 过期代次测试（`cb51acb`/`d391ba7`）；#2 run/update/uninstall CLI（`4c67ede`/`000436d`/`e406fdb`）+ clippy 行数拆分（`e782c35`/`f3e83f6`）；#11 inspect + supervisor unregister（`ce06746`/`7e975d7`，**发现仍开**）。**未做 / 仍开**：C-SHARD、C-CELL **七件套全量**、Stage D 余量、#12 全生命周期余量、三平台 CI、夜间 schedule、#17 custody/gateway；各前片无独立 Evidence 文件——Claim=`PARTIAL_PASS`。本地 fmt+clippy+workspace test 于 `e782c35` 绿（1628 passed）**≠** 三平台 CI；不声称 CI 绿。

**W39（TaskAuthority 跨 Cell 波 + D 邻接前片）**：`C-SHARD` / `C-FANOUT` **未实现**（ADR-0019 仍非 VERIFIED）。D 前片（决策点 3 归 D 册）：五层只读 inspect `ee649d3`/`86fd713`；窗口开合终态 `d059ac2`/`49d3e14`；Surface `create`/`hide` `6165bc4`——**`PARTIAL_PASS` / D 前片**，**不声称 Stage D 余量 / C-SHARD DONE**。

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

2026-09-22 落档（现已在 origin/main）：[ADR-0018](./adrs/0018-single-host-multiprocess-dual-cell-topology.md) `ACCEPTED` `c433d6f`；[ADR-0019](./adrs/0019-cross-cell-commit-semantics-extension.md) `CANDIDATE` `b512fa7`；[ADR-0020](./adrs/0020-control-plane-single-writer-and-consensus-timing.md) `CANDIDATE` `705d4d1`。**否决窗口仍至 W36 屏障**（三平台 CI / nightly schedule 未关）。ACCEPTED/CANDIDATE ≠ VERIFIED。

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
- 多 Cell 拓扑（决策点 1）：[ADR-0018](./adrs/0018-single-host-multiprocess-dual-cell-topology.md)（`ACCEPTED` ≠ `VERIFIED`）
- 跨 Cell 提交语义扩展点（决策点 4）：[ADR-0019](./adrs/0019-cross-cell-commit-semantics-extension.md)（`CANDIDATE`；C-SHARD 派发前置）
- 控制面单写者与共识基底时序：[ADR-0020](./adrs/0020-control-plane-single-writer-and-consensus-timing.md)（`CANDIDATE`；不解锁 C-CELL / C-SHARD）
- Cell 骨架 DESIGN spec：[2026-09-22-cell-skeleton-design.md](../superpowers/specs/2026-09-22-cell-skeleton-design.md)（不授权七件套实现）
- 机器台账：[claims.yaml](./claims.yaml)、[risks.yaml](./risks.yaml)、[evidence-index.yaml](./evidence-index.yaml)
- 风险与未知项：[exit-unknown-risks.md](./exit-unknown-risks.md)（U-1..U-14；§4 已决定递延项）
- 评审基线：[W31-G](../evidence/stage-b/reviews/w31g-road-b004-gates.md)、[W33-H](../evidence/stage-b/reviews/w33h-road-b001-b002.md)、[W34-A](../evidence/stage-b/reviews/w34a-six-gate-matrix.md)

## C.8 增量日志（W36/W37/W38 合入与推送）

2026-09-22 单一 integrator：把已完成未合入车道合入**本地** `main`（基点 `8b91419` → 代码 HEAD `4fe5854`，本登记提交另计），并 CAS 对齐 §C.3 / L0 / evidence-index / §6.5.6。当时未 push、未开 PR、未跑波次屏障 CI。

| 车道 | 分支 | merge SHA | 写集要点 |
|---|---|---|---|
| W37-A | `feat/w37-topo-adr` | `c433d6f` | ADR-0018 新文件 |
| W37-B | `feat/w37-xcell-adr` | `b512fa7` | ADR-0019 新文件 |
| W37-D | `feat/w37-consensus-adr` | `705d4d1` | ADR-0020 新文件 |
| W37-C | `feat/w37-cell-spec` | `e485450` | Cell 骨架 DESIGN spec |
| W36 T2 / #10 | `feat/w36-p10` | `c45bd10` | nlos-runtime / nlos-runtime-tokio / nlos-process + W36-P10 Evidence |
| W36 T4 / #8 | `feat/w36-p8` | `8ed592a` | nlos-plan / nlos-task + B-PLAN-001 §13 / B-TASK-SCALE-001 §15 |
| W37-E | `feat/w37-cell-skeleton` | `4fe5854` | 新 crate `nlos-cell`（Cargo.toml/lock 无冲突） |

已在 main、本波补登（先前合入未写 §C.3）：#2 `953c8af`、#5 `09c0832`、#6 `4df55d6`、#7 `8b91419`、#11 `f49c5a8`。

**未运行项（不得用本登记冒充屏障）**：三平台 CI / Pages / MSRV；`scale-probe-release` 首 CI run；夜间 schedule；Windows supervisor 实杀臂；T4 100K@50% / 调度器 10K ignore 探针本 integrator 未复跑（车道报告已有数字）。

2026-09-22：本地屏障于 HEAD `8397488`（plan-fmt merge）记为 workspace test 1614 passed / 0 failed / 25 ignored、`cargo fmt --check`、clippy 均绿；三平台 CI 未跑。

2026-09-25：`git push origin main` 成功——`8b91419..e406fdb`（含 W36/W37 合入 + W38 前片）。推送后 origin/main = `e406fdbc526b84193112085b48dc74dae9ef21a8`；本地与远程对齐（ahead/behind 0）。§C.3 同步：`C-LEASE`/`C-APP-PAYLOAD`/`C-APP-CONTROL`/`C-CELL` 前片改为 `PARTIAL_PASS`（无 W38 Evidence 文件处不升 DONE）。**仍未声称**：C-SHARD、C-CELL 七件套、#12 全生命周期、三平台 CI、夜间 schedule。

| 车道 | 分支 | merge SHA | 写集要点 |
|---|---|---|---|
| W38-E cell tests | `fix/w38-cell-tests` | `e9c252c` | nlos-cell 双进程窗口 / 去空跑 child |
| W38-L12 observe | `feat/w38-lifecycle` | `6547039` | spawn 拒绝后子进程已死观察（**不**解锁 #12） |
| W38 plan-evict | `test/w38-plan-unpin-evict` | `84114c5` | 未 pin 节点驱逐正路径 |
| W38 ticker | `test/w38-supervisor-ticker` | `ed93d9c` | supervisor ticker 文件缓冲 flake |
| W38-A11 inspect | `feat/w38-app-control` | `ce06746` | InspectApplication GET + NL/CLI |
| W38 unregister | `feat/w38-supervisor-unregister` | `7e975d7` | supervisor 按代次 unregister（发现仍开） |
| W38 cell-id | `test/w38-cell-identity` | `5c93394` | 身份与 pid 解耦断言 |
| W38-L QuotaLease | `feat/w38-quota-lease` | `ac898ec` | `nlos-lease` 单 Cell QuotaLease |
| W38-P2 run | `feat/w38-payload` | `4c67ede` | `nlos package run` |
| W38 lease-admit | `fix/w38-lease-admit` | `fc5d81b` | QuotaLease → `CellAuthority::admit` |
| W38-P2 update | `feat/w38-package-update` | `000436d` | `nlos package update` |
| W38-P2 uninstall | `feat/w38-package-uninstall` | `e406fdb` | `nlos package uninstall` |

**仍开、具名 parked（owner）**：见 §C.3.1/§C.3.2 各行未决项；另 `feat/dash-plugin` 为工具链旁支、不入 C 册（owner：dash 插件车道）。

2026-09-25（历史）：代码 tip `e782c35` 时相对当时 origin/main `ee649d3` 曾 **ahead 8**；一次 unbound `git push` 挂起失败后台账登记至 `4a98e67`/`194b014`。**现已核实**：范围 `ee649d3..194b014` 已由 bound SSH `ls-remote` 核实在 origin；台账提交 `3c5cc9a` 与其后的 invariant 提交亦在 origin；本地 main 与 origin 对齐（ahead/behind 0）；权威 tip 以 `git ls-remote origin refs/heads/main` 为准，本文件不冻结 tip SHA。§C.3 同步：`C-LEASE` 三族单 Cell 前片、`C-CELL` boot generation、`C-LIFECYCLE` 干净退出+stale-generation 测试、`C-APP-PAYLOAD` clippy 拆分、D 前片（五层 inspect + Surface CLOSED）均为 `PARTIAL_PASS`。**仍未声称**：C-SHARD、C-CELL 七件套全量、Stage D 余量、#12 全生命周期余量、三平台 CI、夜间 schedule。本地验证（据前次会话）：fmt+clippy+workspace test 于 `e782c35` 绿 1628 passed —— **≠** 三平台 CI。

| 车道 | 分支 | merge SHA | 写集要点 |
|---|---|---|---|
| W38-L CapacityLease | `feat/w38-capacity-lease` | `9be119c` | 单 Cell CapacityLease（`6225f4b`） |
| W38-CELL boot gen | `feat/w38-cell-boot` | `12d89e1` | boot generation 持久化（`bb9b7cb`） |
| W38-L12 clean exit | `feat/w38-clean-exit` | `cb51acb` | `clean_shutdown`→`FiberExit::Completed`（`6da024d`） |
| W38-L ExclusiveDevice | `feat/w38-device-lease` | `ac2116c` | 单 Cell ExclusiveDevice（`9219bd8`） |
| W39-D five-layer | `feat/w39-stage-d` | `ee649d3` | Task Space 五层只读 inspect（`86fd713`）；已在 origin |
| W38-L12 restore-proc | `feat/w38-restore-proc` | `d391ba7` | restore 过期代次拒绝测试（`362fc11`）；已在 origin |
| W39-D window | `feat/w39-window-lifecycle` | `d059ac2` | Surface open/close→CLOSED（`49d3e14`）；已在 origin |
| W38 cell lines | `fix/w38-cell-too-many-lines` | `f3e83f6` | clippy `too_many_lines` 拆分 process_scope（`1ad35d4`）；已在 origin |
| W38 pkg lines | `fix/w38-package-too-many-lines` | `e782c35` | clippy `too_many_lines` 拆分 run_command（`dcfc1d3`）；已在 origin |
| W38/W39 台账 | （docs） | `4a98e67` | 登记上述前片为 `PARTIAL_PASS`；已在 origin |
| HEAD/ahead 对账 | （docs） | `194b014` | 纠正 `e782c35`/ahead 与 L0；已在 origin（origin 上已核实点，非当前 tip） |

2026-09-26：三车道本地提交，均为 `PARTIAL_PASS` / 前片。不声称 C-LEASE、Stage D、C-SHARD、七件套或 CI 绿。推送结果不在本段预写。

| 车道 | 提交 | 写集要点 |
|---|---|---|
| Quota 高水位/关闭/取消 | `df8abd6141e567b5f2e618b948ed2a7189fbec73` | `nlos-lease` 单 Cell `LEASE-SPEND-001` / `LEASE-REPORT-001` / `LEASE-CLOSE-001` / `LEASE-PREACTIVE-001` 前片 |
| Surface create/hide | `6165bc4d2b0b9f6b34317b7e9f2740bba3061848` | `REGISTERED→CREATED` 与 `PRESENTED↔HIDDEN`；无窗口管理器 |
| Capacity 预激活归还 | `43f260054c23261ceb663e8d6c1ff7698c7f72fb` | `GLOBAL_RESERVED→RETURNING→RETURNED`；ACK 前不退回源池 |

定向验证（本机）：`nlos-lease` 5 passed / 0 failed；`window_lifecycle_side` 3 passed / 0 failed；两 crate `cargo fmt --check` 与 `clippy -D warnings` exit 0。**≠** 三平台 CI / 夜间 schedule。Device reset、C-SHARD、七件套、#12 全量 teardown 仍开。
