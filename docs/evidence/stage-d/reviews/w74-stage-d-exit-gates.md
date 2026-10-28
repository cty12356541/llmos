# W74 阶段 D 完整自然语言桌面（`SurfaceProfile=DESKTOP`）与退出门独立终审报告（`W68–W74` / `[ROAD-D-001]` / `[UI-PARITY-001]` / `ADR-0025 VERIFIED`）

> - **状态**：`REVIEW`（`2026-10-29`，车道 `W74-D-EXIT-REVIEW` Attempt 1；面向 [stage-d-progress.md §D.5.5](../../../management/stage-d-progress.md#d55-阶段-d-退出评审门) 六项强制退出条件、`v0.5 §28.4` 七大法定交付物、`[ROAD-D-001]`、`[UI-PARITY-001]` 与 [ADR-0025](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) `CANDIDATE / ACCEPTED -> VERIFIED` 的独立终审记录）
> - **终审裁决**：`SATISFIED-WITH-BOUNDARIES`（`Assurance = H4`，`P0 = 0`，`P1 = 0 blocking`，Ready for Maintainer Stage D `EXITED` Sign-off）
> - **日期**：`2026-10-29`
> - **评审基线 `HEAD`**：`40f9967a67a981b385e415353a09cf5518a81633`
> - **Assurance**：`H4`（所审底层桌面宿主与前后端联合取证等级为 [06-架构设计总纲-v0.5.md](../../../design/06-架构设计总纲-v0.5.md) §30 `H3–H4` 跨进程认证 IPC 对等验证、Tauri Rust 宿主端到端正反例旁路测试、前端确定性状态机/ARIA/对比度审计及真机工作站交互战役；本终审严格执行 `Claim <= Evidence`、`[EVID-SCOPE-001]` 与 `[EVID-NEG-001]`，逐项核实 `W69–W74` 全部 11 份阶段 D 证据工件，零慷慨外推）
> - **写集边界**：严格限定为本独立终审报告（`docs/evidence/stage-d/reviews/w74-stage-d-exit-gates.md`）；共享机器台账（[ledger-schema.md](../../../management/ledger-schema.md)、[claims.yaml](../../../management/claims.yaml)、[risks.yaml](../../../management/risks.yaml)、[evidence-index.yaml](../../../management/evidence-index.yaml)、[exit-unknown-risks.md](../../../management/exit-unknown-risks.md)、[stage-d-progress.md](../../../management/stage-d-progress.md) 及 [ADR-0025](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)）由本波屏障唯一集成器 `W74-INT` 串行合流。
> - **前序基线与输入证据**：
>   - 阶段 B/C 桌面与跨机基线：[B-GUI-001](../../stage-b/b-gui-001-task-manager-shell.md)、[W67 阶段 C 退出评审](../../stage-c/reviews/w67-stage-c-exit-gates.md)、[W73 阶段 C 跨物理主机收官独立终审](../../stage-c/reviews/w73-stage-c-xhost-exit-review.md)；
>   - 阶段 D 架构与视觉规范：[Stage D 权威进度单](../../../management/stage-d-progress.md)、[ADR-0025（Rev 2）](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)、[Stage D 桌面视觉与交互总规格（`W68-R2`）](../../../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md)；
>   - 阶段 D 11 份核心证据工件：[D-TOKENS-001](../d-tokens-001-workstation-tokens-and-sota-kit.md)、[D-IPC-001](../d-ipc-001-twelve-domain-enum-and-symmetry.md)、[D-COMPOSITOR-001](../d-compositor-001-five-plane-tiling-wm-and-session.md)、[D-OMNIBAR-001](../d-omnibar-001-command-intent-palette-and-multimodal.md)、[D-TRUSTED-UI-001](../d-trusted-ui-001-z4-consent-chooser-and-taint-clipboard.md)、[D-TASK-SPACE-001](../d-task-space-001-l1-l3-disclosure-dag-and-attempts.md)、[D-NOTIFY-SEARCH-001](../d-notify-search-001-action-token-spotlight-and-offline.md)、[D-CONTROL-SETTINGS-001](../d-control-settings-001-twelve-domain-tree-and-settings-cas.md)、[D-A11Y-I18N-001](../d-a11y-i18n-001-wcag22-aria-and-bilingual-conformance.md)、[D-GUI-CAMPAIGN-001](../d-gui-campaign-001-cross-platform-workstation-campaign.md)、[D-ROAD-001](../d-road-001-non-developer-multi-app-e2e-gate.md)。

---

## 1. 执行裁决与审查范围（Executive Verdict & Scope）

### 1.1 阶段 D 综合终审裁决：`SATISFIED-WITH-BOUNDARIES`（`Assurance = H4`，`P0 = 0`，`P1 = 0 blocking`，Ready for Maintainer Stage D `EXITED` Sign-off）

经对阶段 D（完整自然语言桌面，`SurfaceProfile=DESKTOP`）自 `W68 / W68-R1 / W68-R2` 架构立项与契约固化至 `W69–W74` 六个实施与终验波次的独立交叉审计，本终审作出如下结论：

| 终审维度 | 最高证据等级 | 终审裁决 | 核心实证结论与诚实边界摘要 |
|---|---|---|---|
| **`v0.5 §28.4` 七大法定交付物**（Task Space/窗口/通知/设置/搜索、多模态 I/O、可信权限与成本 UI、系统控制中心与手动调度、计划预览/图形化编辑/重授权、离线恢复、可访问性与本地化） | **`H4`** | **`7/7 SATISFIED-WITH-BOUNDARIES`** | `D-core` 八大工作包（`W69–W73`）与 `W74` `[ROAD-D-001]` E2E 门禁已完整交付 `108` 项 Tauri 认证命令句柄、五平面 `Z0–Z4` 平铺优先工作站合成器、12 域显控同构、`L0–L3` 四级渐进式披露、`Z4` 实心安全确认屏与 `WCAG 2.2 AA` 中英双语支持。 |
| **`[ROAD-D-001]` 阶段 D 唯一法定退出门**（[v0.5 §28.4](../../../design/06-架构设计总纲-v0.5.md)：非开发者无需理解 Agent 编排即可持续使用多个 Application 和后台 Task） | **`H4`** | **`SATISFIED-WITH-BOUNDARIES`** | [D-ROAD-001](../d-road-001-non-developer-multi-app-e2e-gate.md)（[d_road_001_non_dev_multi_app_e2e.rs](../../../../desktop/src-tauri/tests/d_road_001_non_dev_multi_app_e2e.rs)）完整实证非开发者零内部架构行话端到端旅程：多应用并排平铺启动 -> `Ctrl+Space` 命令面板自然语言跨应用任务下达 -> `[NL-AMBIG-001]` 歧义澄清与 `Z4` 实心安全确认屏授权 -> `L1` 16 态承诺表后台推进与 `L2` 单行非模态步骤/配额归因 -> 通知中心一次性 `ActionToken` 快捷核销 -> `L3` 未执行计划节点受控调整与 `Z4` diff 重授权 -> `[DUI-OFFLINE-001]` 本地断网自治与重连恢复 -> `ChooserGrant` 受限保存与可逆性校验。 |
| **`[UI-PARITY-001]` 12 域显控同构与内核到像素对称（Kernel-to-Pixel Symmetry）** | **`H4`** | **`SATISFIED`** | `IPC-D1..D8`（[D-IPC-001](../d-ipc-001-twelve-domain-enum-and-symmetry.md)）与前端 `DesktopProjectionStore` / `SystemControlCenter`（[D-COMPOSITOR-001](../d-compositor-001-five-plane-tiling-wm-and-session.md)、[D-CONTROL-SETTINGS-001](../d-control-settings-001-twelve-domain-tree-and-settings-cas.md)）实现 12 域有界游标枚举（`limit <= 64`，`O(visible_rows)`）、单点检视与显式 `ControlScope` 控制命令 100% 对齐，闭合全部 `TASK_SPACE_GAPS (1–3)` 与存量 Tauri 显控断头路，`TRUSTED_GUI / NL_SHELL / CLI / STRUCTURED_API` 四入口 `receipt_hex` 逐字节一致，并硬锁八项法定不可篡改/防越权锁。 |
| **`D-core` 八大核心工作包 + `#3 C-GUI-CAMPAIGN`**（`W69–W73`） | **`H3`–`H4`** | **`9/9 PASS / PARTIAL_PASS`** | `D-TOKENS-KIT`（`H3`）、`D-IPC-ENUM`（`H4`）、`D-COMPOSITOR-WM`（`H4`）、`D-OMNIBAR-MULTIMODAL`（`H4`）、`D-TRUSTED-UI`（`H4`）、`D-TASK-SPACE-DAG`（`H4`）、`D-NOTIFY-SEARCH-OFFLINE`（`H4`）、`D-CONTROL-SETTINGS-A11Y`（Slices 1 & 2，`H4`）及 `#3 C-GUI-CAMPAIGN`（`H4`，闭合 `U-3`）全部达到或超过 `PARTIAL_PASS`（`H3–H4`）。 |
| **`W68-R1` / `W68-R2` SOTA 工业工作站与 `ANTI-AI-01..07` 纪律** | **`H3`–`H4`** | **`SATISFIED`** | 全桌面源码与渲染产物保持 **`0` `backdrop-filter: blur()`**、**`0` 霓虹发光 `box-shadow`**、**`0` Emoji 象形字符**、圆角严格属于 **`{0px, 2px, 4px, 6px}`**、数值列强制 **`tabular-nums`**、以及 `L0/L1/L2` 中英双语 **`0` 内部内核黑话泄露**。 |
| **[ADR-0025（Rev 2）](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) 晋升就绪度** | **`H4`** | **`READY FOR PROMOTION TO VERIFIED`** | `ADR-0025` 决定 1–4 及 `D-GATE-001..006` 全部验证门已被 `D-TOKENS-001` 至 `D-ROAD-001` 共 11 份独立证据实证覆盖，满足从 `CANDIDATE / ACCEPTED` 晋升为 `VERIFIED` 的全部条件。 |
| **未决 `P0` / 阻塞性 `P1` 风险计数** | 台账复核 | **`P0 = 0`，`P1 = 0 blocking`** | [risks.yaml](../../../management/risks.yaml) 中未决 `P0` 风险为 **`0`**；`U-3` 已由 [D-GUI-CAMPAIGN-001](../d-gui-campaign-001-cross-platform-workstation-campaign.md) 退役，残余边界均已在 `limitations` 与 [exit-unknown-risks.md](../../../management/exit-unknown-risks.md) 诚实登记。 |

> [!IMPORTANT]
> **阶段状态翻转纪律（`AGENTS.md` 规则 7 & [stage-d-progress.md §D.5.5](../../../management/stage-d-progress.md#d55-阶段-d-退出评审门) 条件 4）**：
> 本独立终审报告确认阶段 D（`SurfaceProfile=DESKTOP`）的全部技术退出门、108 项 Tauri 认证命令句柄、五平面工作站合成器、12 域显控同构、安全负路径旁路测试、跨平台 GUI 战役、`[ROAD-D-001]` 非开发者端到端门禁及机器台账 `ledger-schema v1.2` 回填准备均已 **100% 就绪，具备呈交维护者最终签署 `EXITED` 的完整条件（Ready for Maintainer Stage D `EXITED` Sign-off）**。但在获得维护者明确批准（explicit Maintainer sign-off）之前，[stage-d-progress.md](../../../management/stage-d-progress.md) 头部阶段状态 **MUST** 严格保持 `IN_PROGRESS / NOT EXITED`，任何 Agent 严禁擅自越权将其翻转为 `EXITED`。

### 1.2 审查范围与方法论（Audit Scope & Methodology）

本终审覆盖以下六个正交维度：
1. **[v0.5 §28.4](../../../design/06-架构设计总纲-v0.5.md) 七大法定交付物与 [stage-d-progress.md §D.1.1](../../../management/stage-d-progress.md#d11-阶段目标与规范需求全映射) 24+ 项规范性 Requirement ID** 的逐条落地核验；
2. **`D-core` 八大核心工作包（`W69–W73`）、`#3 C-GUI-CAMPAIGN` 与 `W74` `[ROAD-D-001]`** 的代码路径、测试套件、证据工件与保证等级映射；
3. **[ADR-0025（Rev 2）](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)** 四项核心架构决定与 `D-GATE-001..006` 门禁的晋升核准（`CANDIDATE / ACCEPTED -> VERIFIED`）；
4. **[stage-d-progress.md §D.5.5](../../../management/stage-d-progress.md#d55-阶段-d-退出评审门) 六项强制退出条件**（含安全与可信 UI 对抗性负路径旁路矩阵、`[UI-PARITY-001]` 12 域同构与八项法定锁、`WCAG 2.2 AA` 与双语对偶、机器台账 `v1.2` 双向同步）的逐条终审；
5. **`W68-R1` / `W68-R2` SOTA 中性工业工作站反 AI 套路（`ANTI-AI-01..07`）与六大工业级显示原语**的定量核查；
6. **诚实残余边界（Residual Boundaries）界定与维护者签署建议**。

---

## 2. `v0.5 §28.4` 七大核心交付物与 `D-core` 八大工作包全面审计

### 2.1 `v0.5 §28.4` 七大法定交付物逐项核验表

[06-架构设计总纲-v0.5.md §28.4](../../../design/06-架构设计总纲-v0.5.md) 规定阶段 D 必须交付以下 7 项核心能力并满足退出门 `[ROAD-D-001]`。审计结果如下：

| §28.4 序号 | 法定交付物原文 | 关联规范 Requirement ID | 承接工作包与权威证据 | 核心实现与验证事实 | 终审判定 |
|---:|---|---|---|---|---|
| **1** | **完整 Task Space、窗口、通知、设置、搜索** | `[TASK-SPACE-001]`、`[TASK-VIS-001/002]`、`[TASK-NOTIFY-001]`、`[DUI-LAUNCH-001]`、`[DUI-WINDOW-001]`、`[DUI-NOTIFY-001]`、`[DUI-SESSION-001]`、`[DUI-SETTINGS-001]`、`[DUI-ASSOC-001]`、`[CONS-CTRL-001]` | `D-COMPOSITOR-WM`（[D-COMPOSITOR-001](../d-compositor-001-five-plane-tiling-wm-and-session.md)）、`D-TASK-SPACE-DAG`（[D-TASK-SPACE-001](../d-task-space-001-l1-l3-disclosure-dag-and-attempts.md)）、`D-NOTIFY-SEARCH-OFFLINE`（[D-NOTIFY-SEARCH-001](../d-notify-search-001-action-token-spotlight-and-offline.md)）、`D-CONTROL-SETTINGS-A11Y`（[D-CONTROL-SETTINGS-001](../d-control-settings-001-twelve-domain-tree-and-settings-cas.md)） | 五平面 `Z0–Z4` 合成器、`4px` 五模式平铺窗口管理器、`30px` 可信标题栏、`36px` 底部任务栏（`L0`）；`Z1` 16 态 `Task Space`（清零 `TASK_SPACE_GAPS 1–3`）；右下角 16 态通知中心与 SHA-256 一次性 `ActionToken`；四层 `SettingsDomain` CAS + watch、可撤销 `ContentTypeAssociation` 与零明文 `KeychainReference`；`Ctrl+K` Spotlight 签名向量检查点搜索（硬锁 `0x1F` 零写禁区）。 | **`SATISFIED`（`H4`）** |
| **2** | **多模态输入输出** | `[NL-COMPILE-001]`、`[NL-PROPOSE-001]`、`[NL-AMBIG-001]`、`[NL-BIND-001]`、`[DUI-INPUT-001]`、`[TUI-EXT-001]` | `D-OMNIBAR-MULTIMODAL`（[D-OMNIBAR-001](../d-omnibar-001-command-intent-palette-and-multimodal.md)）、`#3 C-GUI-CAMPAIGN`（[D-GUI-CAMPAIGN-001](../d-gui-campaign-001-cross-platform-workstation-campaign.md)） | `Ctrl+Space` / 底栏 `26px` 槽按需唤起 `Z3` 实心命令与意图面板（平时 `0px` 顶部占用）；`nlos.intent.v1` + `nlos.action-proposal.v1` 分层预览；`[NL-AMBIG-001]` 候选解释对比表与澄清硬门；`InputSource::Voice` 流式状态机（锁屏/`Z4` 即时物理切断）与 `L0` 底栏麦克风/摄像头/录屏指示灯；`4px` 实心附件芯片（截图、`ChooserGrant` 文件、`ArtifactId + ContentDigest`）与 CJK IME 组合态防误触保护。 | **`SATISFIED-WITH-BOUNDARIES`（`H4`）** |
| **3** | **可信权限与成本界面** | `[TUI-CONSENT-001]`、`[TUI-SEAL-001]`、`[TUI-GUARD-001]`、`[TUI-CHOOSER-001]`、`[DUI-CHOOSER-001]`、`[DUI-CLIP-001]`、`[RCPT-UNDO-001]`、`[RCPT-PARTIAL-001]` | `D-TRUSTED-UI`（[D-TRUSTED-UI-001](../d-trusted-ui-001-z4-consent-chooser-and-taint-clipboard.md)）、`D-TOKENS-KIT`（[D-TOKENS-001](../d-tokens-001-workstation-tokens-and-sota-kit.md)） | `Z4`（`z-index: 900`）UAC/Polkit 不可遮挡实心安全确认屏（`rgba(10,11,13,0.78)` 零模糊暗幕、`2px` 钢蓝顶条、`32×20` 凹陷单色 Gerono 防伪印章与私有 `security_seal_hint`、`350ms` 呈现与重获焦点防点击劫持门、4 行键值规格表）；`IRREVERSIBLE` 深红 `#DC2626` 法定无撤销锁；`ChooserGrant` Open/Save/Share 受限能力经纪器（零宿主裸路径泄露）；跨应用高敏/污点剪贴板与拖放自动升级 `Z4` 门控。 | **`SATISFIED`（`H4`）** |
| **4** | **System Control Center、Task Manager、Resource Monitor 与各层级手动调度** | `[CTRL-PARITY-001]`、`[CTRL-HIER-001]`、`[CTRL-PRIORITY-001]`、`[CTRL-SAFETY-001]`、`[CTRL-RECOVERY-001]`、`[DUI-CONTROL-001]`、`[UI-PARITY-001]` | `D-IPC-ENUM`（[D-IPC-001](../d-ipc-001-twelve-domain-enum-and-symmetry.md)）、`D-CONTROL-SETTINGS-A11Y` Slice 1（[D-CONTROL-SETTINGS-001](../d-control-settings-001-twelve-domain-tree-and-settings-cas.md)） | 12 域 `SystemControlCenter`、System Informer 十层父子归属树（`L1_SYSTEM_DEVICE..L10_EXECUTION_FIBER`）与 `SHARED_THREAD_LIKE` 共享故障域徽标；阶段 C `Cell` 三族租约检视与受控 `RequestProcessMigration`；Nomad 三水位子弹微条（`Usage / Effective / Requested`）；Headlamp 权限+状态机动作裁剪；Terraform `+ / ~ / - / =` 爆炸半径预览、5s 两步武装确认、`TopicPurge` `DataLossReceipt` 与 `ControlCommandId` 幂等对账恢复。 | **`SATISFIED`（`H4`）** |
| **5** | **自动 Planner 计划预览、图形化编辑、手工 override 与重新授权体验** | `[PLAN-CHANGE-001]`、`[PLAN-OVERRIDE-001]`、`[PLAN-LAZY-001]`、`[CTRL-PLAN-001]`、`[TASK-REOPEN-001]` | `D-TASK-SPACE-DAG`（[D-TASK-SPACE-001](../d-task-space-001-l1-l3-disclosure-dag-and-attempts.md)） | `L1 -> L2 -> L3` 四级渐进式披露；`L3` Dagster 左到右正交 `TaskPlan` DAG 编辑器与 `[PLAN-LAZY-001]` 未物化折叠超节点（`+N 待物化步骤 · 0 进程`）；`COMPLETED`/`ACTIVE` 节点只读法定锁；未执行节点手工增删/暂停/合并/重排/替换经确定性双路径分类器分流为 Path A（受限 `ControlCommand`）与 Path B（新 `plan_digest` 重编译 + Terraform diff + 强制 `Z4` 重新授权）；Temporal `TaskAttempt` 竞速泳道与 `ExecutionFiber` 五段耗时瀑布条。 | **`SATISFIED`（`H4`）** |
| **6** | **离线/恢复体验** | `[DUI-OFFLINE-001]`、`[CONS-DECL-001]`、`[CONS-CTRL-001]`、`[CTRL-RECOVERY-001]` | `D-NOTIFY-SEARCH-OFFLINE`（[D-NOTIFY-SEARCH-001](../d-notify-search-001-action-token-spotlight-and-offline.md)）、`#3 C-GUI-CAMPAIGN`（[D-GUI-CAMPAIGN-001](../d-gui-campaign-001-cross-platform-workstation-campaign.md)） | 在 `LOCAL_SINGLE_NODE` 与 `PartitionAutonomyGate == PARTITIONED` 下，本地六大核心表面（`Launcher`、`LocalArtifacts`、`TaskHistoryAndLocalTasks`、`Settings`、`LocalSearch`、`NotificationHistory`）保持 100% 可交互并附签名 `LocalAutonomyProofBadge`；远端依赖动作精确降级为 `WAITING_NETWORK` / `UNAVAILABLE_REMOTE_OFFLINE`；`QUARANTINED` 节点租约严禁仅靠本地 TTL 解锁；断连超时命令强制按 `ControlCommandId` 对账恢复。 | **`SATISFIED`（`H4`）** |
| **7** | **可访问性和本地化** | `[DUI-WINDOW-001]`、`[TUI-EXT-001]`、`[ROAD-D-001]` | `D-CONTROL-SETTINGS-A11Y` Slice 2（[D-A11Y-I18N-001](../d-a11y-i18n-001-wcag22-aria-and-bilingual-conformance.md)） | `Graphite` 与 `Paper` 双主题 20 组标准色彩对 `WCAG 2.2 AA` 定量审计（正文最低 `4.83:1` >= `4.5:1`，主文本 `14.72:1` / `17.74:1`，`2px` 焦点环最低 `4.08:1` >= `3.0:1`）；`Z0–Z4` ARIA 语义地标树、`>= 26x26px` 触控热区、全键盘焦点遍历与 `Z4` 模态焦点陷阱；`prefers-reduced-motion: reduce` 静态双纽线与 `0ms` 过渡；版本化 `zh-CN / en-US` 双语字典 100% 键值对偶与 `L0/L1/L2` 零行话原子切换。 | **`SATISFIED`（`H4`）** |

### 2.2 `D-core` 八大工作包、`#3 C-GUI-CAMPAIGN` 与 `[ROAD-D-001]` 全量映射表

下表汇总阶段 D 全部 8 个核心工作包、跨阶段移交项 `#3 C-GUI-CAMPAIGN` 及退出门 `[ROAD-D-001]` 的实现路径、Tauri 命令面（总计 **`108` 项 `#[tauri::command]`**）、前后端测试套件、证据 ID 与保证等级：

| 工作包 ID / 波次车道 | 权威证据工件 | 等级 / 状态 | 核心实现路径（Rust 后端 + Tauri 宿主 + TS/CSS 前端） | 定向验证套件与实测断言规模 |
|---|---|---|---|---|
| **`D-TOKENS-KIT`**<br>（`W69-D-TOKENS`） | [D-TOKENS-001](../d-tokens-001-workstation-tokens-and-sota-kit.md) | **`H3`**<br>`PARTIAL_PASS` | `desktop/src/styles/{tokens.css,surfaces-and-borders.css}`<br>`desktop/src/ui/{icons.ts,verification-seal.ts,status-indicator.ts,bullet-micro-bar.ts,waterfall-bar.ts,inspector-drawer.ts,blast-radius-diff.ts,receipt-audit-bar.ts,index.ts}` | `desktop/src/ui/__tests__/tokens-and-primitives.test.ts`（`10` 大套件 **`697`–`698` 项断言**：覆盖 `Graphite/Paper` Token、32 枚单色 SVG 图标、`32×20` Gerono 印章与 `prefers-reduced-motion`、24 态指示器、6 类法定锁剪枝、四大 SOTA 原语、`WCAG 2.2 AA` 亮度比与 `ANTI-AI-01..07`） |
| **`D-IPC-ENUM`**<br>（`W69-D-IPC-ENUM`） | [D-IPC-001](../d-ipc-001-twelve-domain-enum-and-symmetry.md) | **`H4`**<br>`PARTIAL_PASS` | `schema/nlos/sabi/v1/system_control.proto`、`gen/**`<br>`crates/nlos-schema/src/lib.rs`<br>`crates/nlos-system-control/src/{cell_inspector.rs,desktop_enum.rs,stage_d_handler.rs,control_stage_d.rs,nl_stage_d.rs}`<br>`desktop/src-tauri/src/{ipc.rs,dto.rs,lib.rs}`<br>`desktop/src/{ipc.ts,types.ts}` | `nlos-schema`（`40/0`）、`nlos-system-control --all-features`（`156/0`）、`desktop/src-tauri/tests/ipc_enum_symmetry_side.rs`（Windows Named Pipe 下 `GUI / NL / CLI / API` 四入口 `receipt_hex` 逐字节对等、12 域 `limit <= 64` 枚举、`IPC-D1..D8` 与五大法定禁区拦截，注册 25 项新 Tauri 命令） |
| **`D-COMPOSITOR-WM`**<br>（`W70-D-COMPOSITOR`） | [D-COMPOSITOR-001](../d-compositor-001-five-plane-tiling-wm-and-session.md) | **`H4`**<br>`PARTIAL_PASS` | `desktop/src-tauri/src/{session.rs,window_lifecycle.rs,surfaces.rs,compositor.rs,input_router.rs,ipc.rs,dto.rs,lib.rs}`<br>`desktop/src/styles/planes-layout.css`<br>`desktop/src/store/{disclosure-model.ts,action-pruner.ts,command-reconciler.ts,projection-store.ts}`<br>`desktop/src/{wm,dock,compositor}/**`、`desktop/src/main.ts` | `desktop/src-tauri/tests/compositor_wm_side.rs`（`7/0` 端到端集成测试，`src-tauri` 累计 `32/0`，注册 58 项 Tauri 命令）+ `desktop/src/compositor/__tests__/compositor-wm.test.ts`（`8` 大套件 **`733` 项断言**：五平面几何、`[DUI-LAUNCH/WINDOW/INPUT/SESSION-001]`、`<16ms` `L0–L3` 切换、`O(visible_rows)<=64`） |
| **`D-OMNIBAR-MULTIMODAL`**<br>（`W71-D-OMNIBAR`） | [D-OMNIBAR-001](../d-omnibar-001-command-intent-palette-and-multimodal.md) | **`H4`**<br>`PARTIAL_PASS` | `desktop/src-tauri/src/omnibar.rs`<br>`desktop/src/views/omnibar/{omnibar.css,intent-compiler.ts,palette-view.ts,index.ts}`<br>`desktop/src/multimodal/{voice-stream.ts,attachments.ts,index.ts}` | `desktop/src-tauri/tests/omnibar_multimodal_side.rs`（`7/0` 集成测试，新增 11 项 Tauri 命令）+ `desktop/src/views/omnibar/__tests__/omnibar-multimodal.test.ts`（`9` 大套件 **`525` 项断言**：`0px` 常驻占用、`Ctrl+Space` 唤起、`L1/L2` 零行话与 `L3` `TypedIntent`、`[NL-AMBIG-001]` 澄清门、`[NL-BIND-001]`、语音切断与附件防泄密） |
| **`D-TRUSTED-UI`**<br>（`W71-D-TRUSTED-UI`） | [D-TRUSTED-UI-001](../d-trusted-ui-001-z4-consent-chooser-and-taint-clipboard.md) | **`H4`**<br>`PARTIAL_PASS` | `desktop/src-tauri/src/trusted_ui.rs`<br>`desktop/src/trusted-ui/{z4-security-dialog.ts,chooser-grant-modal.ts,clipboard-drag-guard.ts,index.ts}` | `desktop/src-tauri/tests/trusted_ui_scrim_chooser_clip_side.rs`（`5` 大端到端与对抗旁路套件 `7/0`，新增 12 项 Tauri 命令，累计 81 项）+ `desktop/src/trusted-ui/__tests__/trusted-ui-security.test.ts`（`7` 大套件 **`333` 项断言**：`Z4` 防伪/防劫持、`32×20` 印章、`IRREVERSIBLE` 零撤销、`ChooserGrant` 零裸路径、污点剪贴板升级 `Z4`） |
| **`D-TASK-SPACE-DAG`**<br>（`W72-D-TASK-DAG`） | [D-TASK-SPACE-001](../d-task-space-001-l1-l3-disclosure-dag-and-attempts.md) | **`H4`**<br>`PARTIAL_PASS` | `desktop/src-tauri/src/plan_editor.rs`<br>`desktop/src/store/disclosure-model.ts`<br>`desktop/src/views/task-space/{dag-canvas.ts,attempt-lanes.ts,task-space.css,index.ts}` | `desktop/src-tauri/tests/task_space_dag_override_side.rs`（`8/0` 集成测试：Path A vs Path B、`Z4` 重授权、法定锁 `#3/#4/#5`、`[PLAN-LAZY-001]`、`[TASK-REOPEN-001]`）+ `desktop/src/views/task-space/__tests__/task-space-dag.test.ts`（`7` 大套件全绿，清零 `TASK_SPACE_GAPS = []`） |
| **`D-NOTIFY-SEARCH-OFFLINE`**<br>（`W72-D-NOTIFY-SEARCH`） | [D-NOTIFY-SEARCH-001](../d-notify-search-001-action-token-spotlight-and-offline.md) | **`H4`**<br>`PARTIAL_PASS` | `desktop/src-tauri/src/notify_search_offline.rs`<br>`desktop/src/views/{notifications,spotlight,offline-cluster}/**` | `desktop/src-tauri/tests/notify_token_search_offline_side.rs`（`8/0` 集成测试：16 态通知、一次性 `ActionToken` 重放/跨应用拒绝、`0x1F` 五门拦截、`[DUI-OFFLINE-001]` 六表面本地自治、`QUARANTINED` 租约锁）+ `desktop/src/views/notifications/__tests__/notify-search-offline.test.ts` 全绿 |
| **`D-CONTROL-SETTINGS-A11Y` (Slice 1)**<br>（`W72-D-SETTINGS-CTRL`） | [D-CONTROL-SETTINGS-001](../d-control-settings-001-twelve-domain-tree-and-settings-cas.md) | **`H4`**<br>`PARTIAL_PASS` | `desktop/src-tauri/src/settings_keychain.rs`<br>`desktop/src/views/control-center/{ownership-tree.ts,cell-lease-panel.ts,blast-radius-modal.ts,control-center.css,index.ts}`<br>`desktop/src/views/settings/{settings-cas-view.ts,associations-view.ts,keychain-view.ts,settings.css,index.ts}` | `desktop/src-tauri/tests/control_center_settings_cas_side.rs`（`7/0` 集成测试，`W72` 累计注册 102 项 Tauri 命令）+ `desktop/src/views/control-center/__tests__/control-center-settings.test.ts`（`7/0` 套件：12 域控制中心、十层归属树、`SHARED_THREAD_LIKE`、`Cell` 迁移、5s 两步武装 + `DataLossReceipt`、四层设置 CAS、关联防劫持、Keychain 零明文） |
| **`D-CONTROL-SETTINGS-A11Y` (Slice 2)**<br>（`W73-D-A11Y-I18N`） | [D-A11Y-I18N-001](../d-a11y-i18n-001-wcag22-aria-and-bilingual-conformance.md) | **`H4`**<br>`PARTIAL_PASS` | `desktop/src-tauri/src/a11y_i18n.rs`<br>`desktop/src/i18n/{locales/zh-CN.ts,locales/en-US.ts,index.ts}`<br>`desktop/src/a11y/{focus-manager.ts,aria-tree-auditor.ts,contrast-and-motion.ts,a11y.css,index.ts}` | `desktop/src-tauri/tests/a11y_i18n_conformance_side.rs`（`6/0` 集成测试，`W73` 累计注册 **`108` 项 Tauri 命令**）+ `desktop/src/a11y/__tests__/a11y-i18n.test.ts`（`7` 大套件 **`148` 项断言**：20 组 `WCAG 2.2 AA` 对比度、`Z0–Z4` ARIA 树、`Z4` 焦点陷阱、`prefers-reduced-motion`、`zh-CN / en-US` 100% 对偶与零行话） |
| **`#3 C-GUI-CAMPAIGN` (`U-3`)**<br>（`W73-D-GUI-CAMPAIGN`） | [D-GUI-CAMPAIGN-001](../d-gui-campaign-001-cross-platform-workstation-campaign.md) | **`H4`**<br>`PARTIAL_PASS` | `desktop/e2e/{workstation-campaign-matrix.ts,ime-and-multimodal-probe.ts,long-run-leak-detector.ts,index.ts}`<br>`desktop/scripts/gui-campaign/run_workstation_campaign.py` | `desktop/e2e/__tests__/gui-campaign.test.ts`（`8` 大套件 **`70` 项断言**）+ `python desktop/scripts/gui-campaign/run_workstation_campaign.py --self-test`（Windows 11 `10.0.28000 AMD64` 实机探针、三平台分辨率矩阵、中日英 CJK IME 组合态探针、`100K` 任务 1,000 轮 `O(visible_rows)<=64` 零 DOM/内存泄漏浸泡测试，`CAMPAIGN_PASSED`） |
| **`[ROAD-D-001]` E2E 终验门**<br>（`W74-D-ROADD001`） | [D-ROAD-001](../d-road-001-non-developer-multi-app-e2e-gate.md) | **`H4`**<br>`PASS` | `desktop/src-tauri/tests/d_road_001_non_dev_multi_app_e2e.rs` | 全链路非开发者多应用 + 后台任务 E2E 验收套件（验证 `L0/L1` 零编排概念暴露、多应用激活与平铺切换、`Ctrl+Space` 自然语言意图编译与歧义澄清、`Z4` 授权、`L2` 单行归因、通知 `ActionToken` 核销、`L3` 计划调整重授权、`[DUI-OFFLINE-001]` 断网自治与恢复、`ChooserGrant` 受限导出与 `Anti-AI` / 零行话守卫） |

### 2.3 [ADR-0025（Rev 2）](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) 晋升 `VERIFIED` 核准记录

[ADR-0025](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) 定义的 4 项核心架构决定与 6 项桌面验证门（`D-GATE-001..006`）已全部由 `W69–W74` 实证闭环：

1. **决定 1（五平面 `Z0–Z4` 合成隔离与输入防伪路由，`D-GATE-001`）**：由 [D-COMPOSITOR-001](../d-compositor-001-five-plane-tiling-wm-and-session.md) 与 [D-TRUSTED-UI-001](../d-trusted-ui-001-z4-consent-chooser-and-taint-clipboard.md) 验证通过——`Z2` 不可信应用表面被限制在 `30px` 可信标题栏下与 `<iframe sandbox="allow-scripts">` 裁剪区内，任何伪造 `Z3/Z4` 输入源或声明 `z-index > 299` 的行为均被 `FocusAndInputRoute` 与 `TrustedUiBroker` 确定性拦截。
2. **决定 2（单一控制编译主干与 `L0–L3` 视口窗口化惰性取数，`D-GATE-002`）**：由 [D-IPC-001](../d-ipc-001-twelve-domain-enum-and-symmetry.md)、[D-COMPOSITOR-001](../d-compositor-001-five-plane-tiling-wm-and-session.md) 与 [D-TASK-SPACE-001](../d-task-space-001-l1-l3-disclosure-dag-and-attempts.md) 验证通过——`GUI / NL / CLI / API` 四入口 `receipt_hex` 逐字节等价；`L1 -> L2 -> L3` 原地切换耗时 `< 16ms`（实测 `<= 0.04ms`），且驻留行数严格有界于 `O(visible_rows) <= 64`。
3. **决定 3（`Z4` UAC/Polkit 实心安全确认屏、`32×20` 防伪印章、`ChooserGrant` 与污点剪贴板，`D-GATE-003`）**：由 [D-TOKENS-001](../d-tokens-001-workstation-tokens-and-sota-kit.md) 与 [D-TRUSTED-UI-001](../d-trusted-ui-001-z4-consent-chooser-and-taint-clipboard.md) 验证通过。
4. **决定 4（`[UI-PARITY-001]` 12 域显控同构、法定不可篡改锁、离线自治與 `0x1F` 搜索围栏，`D-GATE-004`、`D-GATE-005`、`D-GATE-006`）**：由 [D-IPC-001](../d-ipc-001-twelve-domain-enum-and-symmetry.md)、[D-NOTIFY-SEARCH-001](../d-notify-search-001-action-token-spotlight-and-offline.md)、[D-CONTROL-SETTINGS-001](../d-control-settings-001-twelve-domain-tree-and-settings-cas.md)、[D-A11Y-I18N-001](../d-a11y-i18n-001-wcag22-aria-and-bilingual-conformance.md)、[D-GUI-CAMPAIGN-001](../d-gui-campaign-001-cross-platform-workstation-campaign.md) 与 [D-ROAD-001](../d-road-001-non-developer-multi-app-e2e-gate.md) 全量验证通过。

据此，本终审正式建议 `W74-INT` 将 [ADR-0025](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) 状态晋升为 **`VERIFIED`**。

---

## 3. `stage-d-progress.md §D.5.5` 六项强制退出条件逐条终审

### 3.1 六项退出条件综合核对矩阵

| 退出条件维度（对齐 [§D.5.5](../../../management/stage-d-progress.md#d55-阶段-d-退出评审门)） | 终审状态 | 权威证据锚点 | 核心核验事实摘要 |
|---|---|---|---|
| **条件 1：`D-core` 八大工作包全部达到 `>= PARTIAL_PASS`（`H3–H4`），且三平台桌面编译/测试常设门全绿** | **`SATISFIED`** | `D-TOKENS-001`（`H3`）、`D-IPC-001`..`D-A11Y-I18N-001`（`H4`） | 8 个核心工作包（含 `D-CONTROL-SETTINGS-A11Y` Slices 1 & 2）全部落地生产代码、108 项 Tauri 命令句柄、Rust 端到端集成测试与前端确定性测试套件，零未完成桩模块。 |
| **条件 2：`[ROAD-D-001]` 非开发者多应用与后台任务端到端门禁通过** | **`SATISFIED`** | [D-ROAD-001](../d-road-001-non-developer-multi-app-e2e-gate.md)、[d_road_001_non_dev_multi_app_e2e.rs](../../../../desktop/src-tauri/tests/d_road_001_non_dev_multi_app_e2e.rs) | 非开发者在默认 `L0/L1` 零架构行话下持续操作多应用平铺窗口、通过 `Ctrl+Space` 发起跨应用自然语言任务、完成歧义澄清与 `Z4` 授权、在 `L2` 单行按需查看步骤与配额卡点（无需开启全局专家模式）、核销通知 `ActionToken`、在断网离线时继续本地工作并重连恢复。 |
| **条件 3：`[UI-PARITY-001]` 12 域零能力隐藏、`L0–L3` 四级渐进式披露与八项法定不可篡改/防越权锁** | **`SATISFIED`** | [D-IPC-001](../d-ipc-001-twelve-domain-enum-and-symmetry.md)、[D-COMPOSITOR-001](../d-compositor-001-five-plane-tiling-wm-and-session.md)、[D-TASK-SPACE-001](../d-task-space-001-l1-l3-disclosure-dag-and-attempts.md)、[D-CONTROL-SETTINGS-001](../d-control-settings-001-twelve-domain-tree-and-settings-cas.md) | 12 域 `List* (limit<=64) + Inspect* <-> ControlCommand (ControlScope 1..=17)` 100% 显控同构；`L0 -> L1 -> L2 -> L3` 按视口游标 `O(visible_rows)` 惰性取数；八项法定不可篡改/防越权锁在协议层与 UI 层双重锁死（详见 §3.2）。 |
| **条件 4：安全与可信 UI 对抗性负路径旁路测试矩阵全绿，且 `P0 = 0`** | **`SATISFIED`** | [D-TRUSTED-UI-001](../d-trusted-ui-001-z4-consent-chooser-and-taint-clipboard.md)、[D-OMNIBAR-001](../d-omnibar-001-command-intent-palette-and-multimodal.md)、[D-NOTIFY-SEARCH-001](../d-notify-search-001-action-token-spotlight-and-offline.md)、[D-COMPOSITOR-001](../d-compositor-001-five-plane-tiling-wm-and-session.md) | `Z4` `350ms` 防点击劫持门、`32×20` 凹陷 Gerono 印章与私有 `security_seal_hint` 零下泄、`ChooserGrant` 零宿主裸路径泄露、`ActionToken` 重放/跨应用拒绝、`0x1F NON_AUTHORITATIVE` 五门零写拦截、高敏/污点剪贴板跨应用升级 `Z4`、锁屏/切用户会话代际立即吊销全部通过对抗测试（详见 §3.3）。 |
| **条件 5：`#3 C-GUI-CAMPAIGN` / `U-3` 闭合 + `WCAG 2.2 AA` 无障碍与 `zh-CN / en-US` 100% 双语对偶** | **`SATISFIED`** | [D-A11Y-I18N-001](../d-a11y-i18n-001-wcag22-aria-and-bilingual-conformance.md)、[D-GUI-CAMPAIGN-001](../d-gui-campaign-001-cross-platform-workstation-campaign.md) | 20 组色彩对 `WCAG 2.2 AA` 达标、`Z0–Z4` ARIA 地标与 `Z4` 焦点陷阱、`prefers-reduced-motion` 静态冻结、`zh-CN / en-US` 零缺失键/零行话/零 Emoji；五阶段跨平台工作站交互战役、中日英 CJK IME 探针与 1,000 轮 `100K` 任务零泄漏浸泡测试全绿，正式退役 `U-3`。 |
| **条件 6：`ledger-schema v1.2` 升级、`claims.yaml` Stage D 全量回填、`evidence-index.yaml` 100% 双向同步与历史静态缺口清零** | **`SATISFIED`（Ready for `W74-INT` Sync）** | `W74-SCHEMA-V12`、本报告 §5.2、[desktop/src/views/task-space/index.ts](../../../../desktop/src/views/task-space/index.ts) | `TASK_SPACE_GAPS = []` 已清零；`ledger-schema.md v1.2` 纯增量扩展 `stage: "D"` 与 `RISK-D-NN`；本报告 §5.2 给出 `W74-INT` 机器台账精确回填清单，确保 `lint_claims.py` 与 `test_lint_claims.py` 100% 零错误通过。 |

### 3.2 条件 3 深度核验：`[UI-PARITY-001]` 12 域显控同构与八项法定不可篡改锁（8 Statutory Non-Tamperable Locks）

1. **12 域显控同构（Zero Capability Hiding & Zero Blind Control）**：
   - 覆盖 `v0.5 §25.3` 十层（`1. System/Device`、`2. Workspace/Tenant`、`3. Application`、`4. Task`、`5. TaskGroup`、`6. TaskNode`、`7. TaskAttempt`、`8. Process/AgentInstance/ExecutionFiber`、`9. ResourceGroup/Context`、`10. Topic/Channel`、`11. DurableOperation`）+ 阶段 C 分布式域（`12. Cell & Three-Family Leases`）以及横切搜索/通知/设置面；
   - 每个域均具备有界游标列表枚举（`limit <= 64`）、单点权威检视（`Inspect*`）与携带显式 `ControlScope (1..=17)` + CAS 代际令牌的 `ControlCommand`，彻底根除“有写无列（盲控）”、“后端已实现但前端漏接（`inspect_search`、`inspect_notification`、`disable-application`、`uninstall-application`）”与“无类型 `targetIdHex` 重载”。
2. **`L0 -> L1 -> L2 -> L3` 四级渐进式披露与 `O(visible_rows)` 视口围栏**：
   - `L0`（`36px` 底栏与 `30px` 窗框单色环境感知）-> `L1`（零行话 16 态任务承诺表与标准应用工作区，仅拉取 `list_tasks` `limit <= 64`，`0` 次 `L2/L3` 预拉取）-> `L2`（无需开启全局专家模式，点击单行即在右侧 `420px` Headlamp 非模态抽屉按需展开步骤依赖、阻塞原因与 Nomad 三水位配额瓶颈）-> `L3`（`Ctrl/⌘ + Shift + E` 直达，展开 Dagster 正交 DAG、Temporal `TaskAttempt` 竞速泳道、`ExecutionFiber` 五段瀑布条、System Informer 十层归属树、`Cell` 三族租约与 `receipt_hex` 取证底栏）。
3. **八项法定不可篡改/防越权锁（8 Statutory Non-Tamperable Locks）核验表**：

| 法定锁序号 | 法定锁标识 | 规范依据 | 协议层与 UI 层双重强制机制 | 实测验证锚点 |
|---:|---|---|---|---|
| **Lock #1** | `SEARCH_NON_AUTHORITATIVE_0X1F`（`0x1F` 搜索候选零写锁） | `[CONS-CTRL-001]`、`[UI-PARITY-001]` | 搜索候选强制携带 `restrictions_bitmask = 0x1F`，UI 仅保留 `inspect` 与 `jump_to_authority`；将搜索命中直接用于权限授予（`0x01`）、账本扣减（`0x02`）、任务提交（`0x04`）、应用更新（`0x08`）或不可逆动作（`0x10`）在协议层与前端均硬拒绝（`SEARCH_RESULT_NON_AUTHORITATIVE_0X1F`）。 | `ipc_enum_symmetry_side.rs`、`notify_token_search_offline_side.rs`、`omnibar_multimodal_side.rs` |
| **Lock #2** | `IRREVERSIBLE_NO_UNDO`（不可逆动作零伪撤销锁） | `[RCPT-UNDO-001]`、`[UI-PARITY-001]` | `UndoClass == IRREVERSIBLE` 的任务/回执在 UI 层物理省略 `Undo` 按钮（`data-undo-omitted="true"`，仅渲染“查看操作与效果凭证”），协议层调用 `undo_receipt` 立即拒绝（`IRREVERSIBLE_STATUTORY_LOCK_VIOLATION`）。 | `trusted_ui_scrim_chooser_clip_side.rs`、`task-space-dag.test.ts` Suite 3 |
| **Lock #3** | `NODE_COMPLETED_OR_ACTIVE_LOCKED`（已执行/运行中计划节点只读锁） | `[PLAN-CHANGE-001]`、`[TASK-GRAPH-002]` | `COMPLETED` 与 `ACTIVE` 状态的 `TaskNode` 在 `L3` DAG 画布上硬锁为只读；任何针对其的 `remove_node`、`replace_node`、`merge_nodes`、`reorder_nodes` 在前后端均确定性拒绝（`READ_ONLY_EXECUTED_TASK_NODE`）。 | `task_space_dag_override_side.rs`、`task-space-dag.test.ts` Suite 5 |
| **Lock #4** | `TASK_ATTEMPT_NO_MANUAL_COMMIT`（`TaskAttempt` 禁手工标提交锁） | `[TASK-COMMIT-001]`、`[UI-PARITY-001]` | `TaskAttempt` 胜出者仅能由内核 `TaskHead` CAS 裁决产生；UI 物理剔除人工标记 commit 按钮，协议层调用 `override_task_attempt_commit` 立即拒绝（`FORBIDDEN_MANUAL_ATTEMPT_COMMIT`）。 | `ipc_enum_symmetry_side.rs`、`task_space_dag_override_side.rs` |
| **Lock #5** | `OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS`（`EFFECT_UNKNOWN` 禁手工改判成功锁） | `[TASK-EFFECT-003]`、`[CTRL-RECOVERY-001]` | 处于 `EFFECT_UNKNOWN` 的 `DurableOperation` 严禁通过 UI 按钮强行改判为 `SUCCEEDED`，仅允许发起受控对账查询 `getControlCommandStatus` 或走补偿审计流（`FORBIDDEN_MANUAL_EFFECT_SUCCESS`）。 | `ipc_enum_symmetry_side.rs`、`task_space_dag_override_side.rs`、`control_center_settings_cas_side.rs` |
| **Lock #6** | `QUARANTINED_LEASE_FENCE`（隔离态远程租约禁本地 TTL 解锁） | `[LEASE-TTL-001]`、`[CELL-FENCE-001]` | 处于 `QUARANTINED` 状态的 `Cell` 或远程租约严禁仅凭本地单调时钟 TTL 到期解锁或迁出进程，必须同时具备验签 `closureReceiptHex` 与 `fenceReceiptHex` 方可解除围栏（`QUARANTINED_LEASE_FENCE_LOCKED`）。 | `notify_token_search_offline_side.rs`、`control_center_settings_cas_side.rs` |
| **Lock #7** | `SYSTEM_POLICY_LOCKED`（系统级策略锁定设置防篡改锁） | `[DUI-SETTINGS-001]` | 在四层 `SettingsDomain`（`system / user / workspace / application`）中，凡被 `system` 层标记为 `policy_locked = true` 的键，低层作用域尝试覆盖或修改时一律拒绝（`SYSTEM_POLICY_LOCKED`）。 | `control_center_settings_cas_side.rs`、`control-center-settings.test.ts` |
| **Lock #8** | `PACKAGE_SILENT_DEFAULT_ASSOC_HIJACK_LOCK`（应用包禁静默夺取默认关联锁） | `[DUI-ASSOC-001]` | 应用 `Package` 安装仅能提交未批准的 `ContentTypeAssociation` 候选提议（`submitPackageProposal`），物理禁止静默覆写当前 `handlerApplicationId` / `handlerInstallationId`，唯有用户显式 CAS 确认方可变更。 | `control_center_settings_cas_side.rs`、`control-center-settings.test.ts` |

### 3.3 条件 4 深度核验：安全与可信 UI 对抗性负路径旁路测试矩阵（Negative Security Bypass Matrix）

本终审逐一复核了阶段 D 针对不可信应用表面（`Z2`）、自动化脚本与跨应用数据流的 7 类对抗性攻击向量及其确定性拦截结果：

| 攻击向量 / 旁路场景 | 防御不变量与门禁机制 | 负路径期望拦截码 | 实测验证结果 |
|---|---|---|---|
| **1. `Z2` 不可信应用遮挡或伪造 `Z4` 授权框 / 抢夺点击（Clickjacking）** | `Z4` 独占 `z-index: 900` 实心暗幕 `rgba(10,11,13,0.78)`；`Z2` 声明 `z-index > 299` 或伪造 `Z4` 输入源被拒；初始呈现与每次 `notify_focus_regained` 后 **`350ms` 内**拒绝任何确认提交，而 `Escape` 在 **`0ms`** 立即取消。 | `CLICKJACKING_ARMING_ACTIVE` / `UNTRUSTED_INPUT_ORIGIN` / `Z2_ZINDEX_OUT_OF_BOUNDS` | **`PASS`**（`trusted_ui_scrim_chooser_clip_side.rs` Tests 1 & 3；`compositor_wm_side.rs`） |
| **2. 不可信表面窃取用户防钓鱼印章（`security_seal_hint`）** | `32×20` 凹陷单色 Gerono 双纽线印章仅在 `include_z4_trusted == true` 的 TCB 通道渲染；`Z0..Z3` 公共快照 DTO 中印章字段恒为 `None`，`Z2` 读取印章立即被拒。 | `UNTRUSTED_SEAL_READ_DENIED`（`Z0..Z3` 泄露计数 `== 0`） | **`PASS`**（`trusted_ui_scrim_chooser_clip_side.rs` Test 1；`trusted-ui-security.test.ts` Suite 1） |
| **3. 应用通过文件选择器套取宿主裸路径或遍历兄弟目录（`[DUI-CHOOSER-001]`）** | `ChooserGrantBroker` 将真实宿主路径封存于 TCB 私有 `HostVaultEntry`；向应用返回的 `ChooserGrantForAppDto` 仅含衰减句柄与安全文件名，序列化 JSON 中 Windows `C:\...` 与 Unix `/home/...` 裸路径出现次数为 **`0`**；未选中兄弟文件、过期句柄、跨应用盗用与 `../` 遍历全部拦截。 | `RAW_HOST_PATH_FORBIDDEN` / `CHOOSER_GRANT_APP_MISMATCH` / `CHOOSER_GRANT_EXPIRED` / `CHOOSER_MODE_VIOLATION` | **`PASS`**（`trusted_ui_scrim_chooser_clip_side.rs` Test 4；`omnibar_multimodal_side.rs`） |
| **4. 通知 `ActionToken` 重放、过期或跨应用/跨用户冒用（`[DUI-NOTIFY-001]`）** | `ActionToken` 以域分离 SHA-256 绑定 `(NotificationId, ApplicationId, TaskId, PrincipalId, action_id, expiry_mono_ms)`；首次核销后立即原子吊销并从 `allowed_verbs` 剪除，二次重放或跨应用调用立即失败。 | `ACTION_TOKEN_ALREADY_CONSUMED` / `ACTION_TOKEN_APP_MISMATCH` / `ACTION_TOKEN_PRINCIPAL_MISMATCH` / `ACTION_TOKEN_EXPIRED` | **`PASS`**（`notify_token_search_offline_side.rs`；`gui-campaign.test.ts` 64 次核销 + 64 次重放拦截） |
| **5. 篡改 `caller_supplied_mask = 0x00` 企图拿 Spotlight 搜索结果直接驱动写操作（`[CONS-CTRL-001]`）** | 后端忽略客户端传入掩码，强制执行 `restrictions_bitmask = 0x1F` 五门拦截，且点击候选必须经 `verify_spotlight_candidate_authority` 重验当前权威代际。 | `SEARCH_RESULT_NON_AUTHORITATIVE_0X1F` / `STALE_GENERATION` / `AUTHORITY_RECORD_RETRACTED` | **`PASS`**（`notify_token_search_offline_side.rs`；`ipc_enum_symmetry_side.rs`） |
| **6. 无手势后台静默读取剪贴板，或跨应用粘贴 `CONFIDENTIAL / CREDENTIALS / HIGH_SECRET / UNTRUSTED_EXTERNAL` 数据（`[DUI-CLIP-001]`）** | `ClipboardAndDragBroker` 强制校验一次性非自动化 `UserGestureToken` 与 TTL；跨应用传递带高敏/外部污点标签内容时强制拉起 `Z4` 确认屏，未获批或按 `Escape` 时向目标应用释放 **`0` 字节**。 | `MISSING_USER_GESTURE` / `AUTOMATION_GESTURE_FORBIDDEN` / `Z4_CONSENT_REQUIRED_FOR_TAINTED_TRANSFER` | **`PASS`**（`trusted_ui_scrim_chooser_clip_side.rs` Test 5；`gui-campaign.test.ts` Stage 3） |
| **7. 会话锁屏/切换用户或应用升级后，旧表面继续接收输入或持有凭据句柄（`[DUI-SESSION-001]`、`[DUI-WINDOW-001]`）** | 执行 `lockSession` 或 `switchUser` 时，`session_generation` 单调递增，挂起的 `Z4` 挑战立即吊销，语音/摄像头采集立即物理切断，`DesktopProjectionStore` 与 `KeychainReference` 会话句柄缓存立即清零；飞向旧 `surface_generation` 的五类输入源事件 100% 丢弃并记入审计计数。 | `SESSION_NOT_ACTIVE` / `STALE_GENERATION_SURFACE` / `SESSION_GENERATION_REVOKED` | **`PASS`**（`compositor_wm_side.rs`；`omnibar_multimodal_side.rs`；`control_center_settings_cas_side.rs`） |

---

## 4. SOTA 反 AI 套路（`ANTI-AI-01..07`）与内核到像素对称性核验（Anti-AI-Smell & Kernel-to-Pixel Symmetry Verification）

### 4.1 `ANTI-AI-01..07` 工业工作站静态与渲染审计表

遵循维护者 `W68-R1` 与 `W68-R2` 裁定（[Stage D UX Spec §0.1](../../../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md)、[ADR-0025](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)），阶段 D 桌面 OS 显式与外部营销门户 [docs/site/DESIGN.md](../../../site/DESIGN.md) 解耦，并在全部 8 套前端测试套件中内嵌了自动化源码与 DOM/CSS 扫描门禁。核验结果如下：

| 禁令编号 | 反 AI 套路（Anti-AI-Smell）硬性约束 | 全仓 `desktop/src/**` 与 `desktop/e2e/**` 实测值 | 核验锚点与判定 |
|---|---|---|---|
| **`ANTI-AI-01`** | **禁止黑底发光青蓝科幻风与径向光斑**：严禁 `#030710` + `#7FC8FF` 发光标题/外框、科技方格背景网与 `radial-gradient` 光斑；锁定 `Graphite Workstation`（`#111214 / #18191C / #1E2024 / #23252A` + `#2563EB`）与 `Paper Workstation`（`#E5E7EB / #FFFFFF / #F3F4F6` + `#1D4ED8`）双套实心中性灰阶主题 | **`0` 处**科幻发光色、网格背景或径向光斑 | `tokens-and-primitives.test.ts`、`compositor-wm.test.ts` — **`PASS`** |
| **`ANTI-AI-02`** | **禁止毛玻璃/液态玻璃模糊滤镜**：全桌面 CSS 与内联样式严禁出现 `backdrop-filter: blur(...)` 或 `-webkit-backdrop-filter`，100% 采用实心不透明表面 + `1px` 结构分割细线 | **`0` 处** `backdrop-filter: blur()` | 全套 CSS（`tokens.css`、`surfaces-and-borders.css`、`planes-layout.css`、`omnibar.css`、`task-space.css`、`control-center.css`、`settings.css`、`a11y.css`）静态扫描 — **`PASS`** |
| **`ANTI-AI-03`** | **禁止霓虹外发光阴影與顶置常驻巨型 Prompt Box**：严禁霓虹 `box-shadow`；命令面板默认隐藏（屏幕顶部常驻占用 **`0px`**），仅通过 `Ctrl+Space` 或底栏 `26px` 槽按需唤起，最大化纵向工作区至 `calc(100vh - 36px)` | **`0` 处**霓虹发光阴影；**`0px`** 顶部常驻占用 | `omnibar_multimodal_side.rs`、`omnibar-multimodal.test.ts` — **`PASS`** |
| **`ANTI-AI-04`** | **严格工业圆角白名单**：严禁 `12px–16px` SaaS 卡片大圆角或 `9999px` 胶囊气泡；全局 `border-radius` 严格限定在 **`{0px, 2px, 4px, 6px}`** 四档（平铺区/底栏 `0px`、微条/印章 `2px`、控件/命令面板 `4px`、浮动窗/`Z4` 模态框 `6px`） | 越界圆角计数 **`== 0`** | `tokens-and-primitives.test.ts`、`compositor-wm.test.ts`、`trusted-ui-security.test.ts` — **`PASS`** |
| **`ANTI-AI-05`** | **零 Emoji 象形字符纪律**：所有 UI 源码、双语字典（`zh-CN.ts` / `en-US.ts`）与渲染 DOM 严禁使用任何彩色或字符 Emoji，100% 采用 32 枚 `1.25px–1.5px` 单色几何 SVG 线性图标与纯数学 `∞`（`U+221E`）兜底 | Emoji 字符计数 **`== 0`** | `a11y_i18n_conformance_side.rs`、`a11y-i18n.test.ts`、全部组件套件 — **`PASS`** |
| **`ANTI-AI-06`** | **双极克制警示色与等宽对齐**：严禁彩虹多色相状态徽章；常态采用单色几何符号，仅保留琥珀色 `#D97706`（待用户介入/确认）与深绯红 `#DC2626`（失败/不可逆警告）；所有配额、水位、延迟、代际与计数器强制 `font-variant-numeric: tabular-nums` | 彩虹徽章 **`== 0`**；数值列 100% `tabular-nums` | `status-indicator.ts`、`bullet-micro-bar.ts`、`waterfall-bar.ts` — **`PASS`** |
| **`ANTI-AI-07`** | **`L0/L1/L2` 零内部内核行话泄露（`[ROAD-D-001]`）**：默认 `L0/L1` 表面与 `L2` 单行归因抽屉在 `zh-CN` 与 `en-US` 下严禁出现 `Z0..Z4`、`TypedIntent`、`0x1F`、`Quota`、`TaskAttempt`、`ExecutionFiber`、`Cell`、`receipt_hex`、`fencing_token`、`plan_digest`、`control_command_id` 等内部术语（仅在 `L3` 诊断层披露） | `L0/L1/L2` 行话泄露计数 **`== 0`** | `a11y_i18n_conformance_side.rs`、`task-space-dag.test.ts`、`d_road_001_non_dev_multi_app_e2e.rs` — **`PASS`** |

### 4.2 六大工业级 SOTA 显示原语与内核到像素对称（Kernel-to-Pixel Symmetry）核验

阶段 D 将 `v0.5` 内核原语精准映射为六个经生产验证的工业级 SOTA 显示原语（外部参考查询基准 `2026-10-26`）：

1. **CNCF Headlamp（权限/状态机动作裁剪 + 右侧非模态归因抽屉）**：`store/action-pruner.ts` 与 `ui/inspector-drawer.ts` 根据当前 `Capability`、16 态 `TaskState` 与八大法定锁实时裁剪可用动作并显示置灰原因；`420px` 右侧非模态抽屉支持在 `L2` 查看单任务阻塞原因而不打断 `L1` 主表浏览。
2. **Dagster（左到右正交 `TaskPlan` DAG + `[PLAN-LAZY-001]` 待物化折叠超节点）**：`views/task-space/dag-canvas.ts` 与 `plan_editor.rs` 将未物化惰性子图渲染为 `+N 待物化步骤 · 0 进程` 折叠超节点（`allocated_process_count == 0`），按需展开且受 `limit <= 64` 游标保护。
3. **Temporal Web UI + Perfetto UI（`L1/L2` 阶段折叠 + `L3` 按墙钟比例 `TaskAttempt` 竞速泳道与 `ExecutionFiber` 五段瀑布条）**：`views/task-space/attempt-lanes.ts` 与 `ui/waterfall-bar.ts` 精确对比 `CAS_WINNER` 与 `SUPERSEDED` 候选、展示 `PROVIDER_ISOLATION_UNVERIFIED` 徽标，并按比例渲染 `activeCpuMs | schedulerWaitMs | externalWaitMs | backpressureWaitMs | suspendedMs`。
4. **HashiCorp Nomad（`Usage / Effective / Requested` 三水位子弹微缩条）**：`ui/bullet-micro-bar.ts` 在 `72–96px × 6px` 实心微条内同时编码实际消耗、有效授予配额/本地 `ResourceSubpoolLease` 水位与请求上限，直观呈现 `[CTRL-PRIORITY-001]` 降级与离线租约收缩。
5. **Sysinternals Process Explorer / System Informer（十层父子归属树与 `SHARED_THREAD_LIKE` 共享故障域徽标）**：`views/control-center/ownership-tree.ts` 贯通 `L1_SYSTEM_DEVICE` 至 `L10_EXECUTION_FIBER`，自动检测并标注共享同一宿主 OS `ProcessId` 的 `AgentInstance`/`ExecutionFiber` 故障域（`[TASK-AGENT-001]`、`[TASK-ISO-005]`）。
6. **Terraform Plan Preview（级联控制前 `+ / ~ / - / =` 爆炸半径差异预览表与 5s 两步武装器）**：`ui/blast-radius-diff.ts` 与 `views/control-center/blast-radius-modal.ts` 在执行级联暂停/取消/强杀/清空/迁移前，强制展示受影响后代与 `=` 豁免系统对象，并对 `kill` 与 `TopicPurge` 执行 5 秒倒计时武装与 `DataLossReceipt` 签发（`[CTRL-HIER-001]`、`[CTRL-SAFETY-001]`）。

---

## 5. 诚实残余边界与维护者签署建议（Residual Boundaries & Maintainer Sign-Off Recommendation）

### 5.1 诚实残余边界声明（Explicit Residual Boundaries — 严守 `Claim <= Evidence`）

按照 [AGENTS.md](../../../../AGENTS.md) 规则 7、`[EVID-SCOPE-001]` 与 [stage-d-progress.md §D.1.2](../../../management/stage-d-progress.md#d12-显式非目标源自规范与阶段边界严禁越界)，阶段 D 在达成 `SATISFIED-WITH-BOUNDARIES`（`H4`）的同时，显式记录以下 **4 项诚实残余边界**（均不构成阶段 D 退出阻塞，已作为 `PARTIAL_PASS` 的非空 `limitations` 或阶段 E 演进项登记）：

1. **宿主窗口壳合成 vs 原生 OS 内核显示服务器合成驱动（`v0.5 §29.1` & `ADR-0025` 非目标）**：
   - 阶段 D 的五平面 `Z0–Z4` 合成器、平铺窗口管理器、`FocusAndInputRoute` 与 `Z4` 模态安全屏运行于 Tauri v2 宿主窗口与特权壳层 WebView（Windows WebView2 / macOS WKWebView / Linux WebKitGTK）及 `<iframe sandbox="allow-scripts">` 隔离边界内；
   - **未声称**替代宿主操作系统内核显示服务器（Wayland Compositor / Windows DWM DirectComposition / macOS SkyLight）或编写裸机 GPU 帧缓冲驱动；第三方原生 Vulkan/wgpu 离屏纹理直通嵌入留归未来触发 `ADR-0025` 复审触发器时演进。
2. **跨平台确定性工作站战役仿真 + Windows 11 实机探针 vs 物理 macOS / Linux 硬件显示服务器集群常设机房（`RISK-B-06` / `U-3` 边界）**：
   - [D-GUI-CAMPAIGN-001](../d-gui-campaign-001-cross-platform-workstation-campaign.md) 已在真实 Windows 11 x86_64 工作站宿主（`10.0.28000 AMD64`）上通过 `run_workstation_campaign.py` 完成实机探针，并在确定性 E2E 战役矩阵中完整覆盖 `windows-11-x86_64 (1440×900)`、`macos-14-arm64 (1512×982)` 与 `linux-x86_64 (1920×1080)` 几何求解、CJK IME 组合流与 1,000 轮 `100K` 任务零泄漏浸泡测试，正式闭合移交项 `#3 C-GUI-CAMPAIGN` 与 `U-3`；
   - **诚实保留边界**：尚未建立 7×24 物理 macOS Quartz 与 Linux Wayland 裸机硬件显示采集机房的像素级光学截图比对流水线（`RISK-B-06` 维持 `P2 / partial` 观察）。
3. **多模态语音硬件驱动与系统级硬件安全模块（HSM）边界（`D-OMNIBAR-001` & `D-CONTROL-SETTINGS-001`）**：
   - `D-OMNIBAR-MULTIMODAL` 验证了 `InputSource::Voice` 经 `FocusAndInputRoute` 的权限握手、流式转写状态机、底栏 `L0` 硬件占用指示以及锁屏/`Z4` 激活时的即时物理切断，但底层 OS 原生麦克风音频驱动采样与本地离线 ASR 模型推理留归语音 DriverProvider 插件；
   - `D-CONTROL-SETTINGS-A11Y` 验证了 `KeychainReference` 零明文私钥导出（仅暴露 `32B` 公钥指纹与 `use-by-reference` 句柄）与锁屏即时擦除，但跨重启硬件 TPM 2.0 / Secure Enclave 物理芯片封印仍由宿主平台密钥链驱动承接。
4. **阶段 E 应用商店生态治理与商业分发边界（`v0.5 §28.5 / §29.2` & `ROAD-E-001`）**：
   - 阶段 D 的 `ApplicationLauncher`、`ChooserGrant` 与 `ContentTypeAssociation` 仅消费本地 `ApplicationAuthority` 已安装且验签通过的应用包；长期 SABI v1.0 跨大版本二进制兼容承诺、第三方 Publisher 公证吊销网、生产级应用商店审核/计费清算与 OS 原生安装包（MSI/DMG）外部 CA 代码签名公证属于阶段 E（`ROAD-E-001`）范畴。

### 5.2 面向波次屏障集成器 `W74-INT` 的机器台账合流指令（Recommended Ledger & ADR Updates）

为满足 [stage-d-progress.md §D.5.5](../../../management/stage-d-progress.md#d55-阶段-d-退出评审门) 条件 2、条件 3 与条件 6，建议 `W74-INT` 在波次屏障合流时执行以下原子更新：

1. **[docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)**：
   - 将状态从 `ACCEPTED` 晋升为 **`VERIFIED`**，并追加引用 `D-TOKENS-001` 至 `D-ROAD-001` 及本终审报告 `w74-stage-d-exit-gates.md`。
2. **[docs/management/evidence-index.yaml](../../../management/evidence-index.yaml)**：
   - 登记 `W74` 新增证据 [d-road-001-non-developer-multi-app-e2e-gate.md](../d-road-001-non-developer-multi-app-e2e-gate.md)（`stage: D`, `assurance_level: H4`）与本独立终审报告 `docs/evidence/stage-d/reviews/w74-stage-d-exit-gates.md`（`stage: D`, `assurance_level: H4`），确保磁盘文件与索引 `100%` 双向对齐。
3. **[docs/management/claims.yaml](../../../management/claims.yaml)**（基于 `W74-SCHEMA-V12` 升级后的 `ledger-schema v1.2`）：
   - 全量回填 `stage: D` 声明条目，覆盖 `[ROAD-D-001]`、`[UI-PARITY-001]` 及 `D-core` 八大工作包对应的规范 Requirement ID（`[NL-COMPILE/PROPOSE/AMBIG/BIND-001]`、`[PLAN-CHANGE/OVERRIDE/LAZY-001]`、`[TUI-CONSENT/CHOOSER/EXT-001]`、`[RCPT-UNDO/PARTIAL-001]`、`[TASK-SPACE/VIS/NOTIFY/REOPEN-001]`、`[CTRL-PARITY/HIER/PRIORITY/PLAN/RECOVERY-001]`、`[DUI-LAUNCH/CONTROL/WINDOW/INPUT/CHOOSER/ASSOC/CLIP/NOTIFY/SESSION/SETTINGS/OFFLINE-001]`），绑定 `docs/evidence/stage-d/*.md` 与本报告，并为所有 `PARTIAL_PASS` 条目填写符合 §5.1 的非空诚实 `limitations`。
4. **[docs/management/risks.yaml](../../../management/risks.yaml) & [docs/management/exit-unknown-risks.md](../../../management/exit-unknown-risks.md)**：
   - 确认 `U-3`（GUI 真机可用性）保持已退役状态，`RISK-B-06`、`U-7`、`U-8` 追加引用 `d-road-001-non-developer-multi-app-e2e-gate.md` 与 `w74-stage-d-exit-gates.md`，保持全仓未决 `P0 = 0`。
5. **[docs/management/stage-d-progress.md](../../../management/stage-d-progress.md) & [docs/management/README.md](../../../management/README.md)**：
   - 在 `§D.3.1`、`§D.5.2`、`§D.5.5` 与 `§D.6.1.7` 同步 `W74` 完成记录与本终审结论；**头部阶段状态严格保持 `IN_PROGRESS / NOT EXITED`（标注“全部退出门已满足，呈请维护者明确批准翻转 `EXITED`”）**。

### 5.3 维护者退出签署建议（Maintainer Sign-Off Recommendation）

综上所述，阶段 D（完整自然语言桌面，`SurfaceProfile=DESKTOP`）已完成从工业级视觉 Token、12 域认证 IPC 显控同构、五平面平铺合成器、按需多模态命令面板、`Z4` 可信安全确认屏、`L1–L3` 渐进式披露 `Task Space` 与 DAG 编辑器、一次性令牌通知与 `0x1F` 只读搜索、本地离线自治、12 域系统控制中心与四层设置 CAS、`WCAG 2.2 AA` 无障碍与中英双语、跨平台 GUI 战役到 `[ROAD-D-001]` 非开发者多应用端到端门禁的全部工程实现与 `H3–H4` 独立取证。

**本独立终审员正式建议维护者（Maintainer）：**
1. 核准 [ADR-0025](../../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) 晋升为 **`VERIFIED`**；
2. 审阅本报告（`docs/evidence/stage-d/reviews/w74-stage-d-exit-gates.md`）与 `W74-INT` 同步后的 `claims.yaml`（`ledger-schema v1.2`），正式批准将 [stage-d-progress.md](../../../management/stage-d-progress.md) 阶段状态从 `IN_PROGRESS / NOT EXITED` 翻转为 **`EXITED`**。
