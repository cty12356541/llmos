# llmos 项目管理机制

> 最高目标：构建达到 Windows/macOS 级别的通用、现代、系统级 NLOS。
>
> 本文件管理“如何把目标持续变成可验证系统”，不改变 [v0.5 架构规范](../design/06-架构设计总纲-v0.5.md)。

## 1. 管理原则

1. **目标、规范、实现、证据分离**：愿景不能冒充能力，代码通过 happy path 不能冒充 conformance。
2. **纵切面优先**：每个里程碑必须贯通身份、权限、资源、执行、持久化、恢复和用户控制，避免只堆独立组件。
3. **需求可追踪**：规范 Requirement ID → 工作项 → 测试 → Evidence → Release Claim 必须可机械追踪。
4. **架构决策显式化**：不可逆或跨模块技术选择必须进入 ADR，注明替代方案、退出成本和复审日期。
5. **默认小批量集成**：短分支、原子提交、持续测试；长期分叉和“大爆炸合并”需要明确批准。
6. **风险先验证**：隔离、durability、取消、资源守恒、ABI 和规模假设先做 PoC/故障注入。
7. **用户控制不后补**：自然语言、GUI、CLI、API 必须从同一 ControlCommand/Receipt 路径生长。

## 2. 权威文档及优先级

冲突时按下列顺序处理：

1. 项目最高目标与安全/权限边界；
2. 当前规范性架构总纲；
3. 已接受 ADR；
4. 阶段计划与验收门；
5. 实现和测试；
6. 讨论记录、原型和站点说明。

代码与规范冲突时，默认登记实现缺口；不能仅因代码已经存在就反向修改规范。

## 3. 工作分解模型

```text
North Star
  → Stage
    → Capability
      → Vertical Slice
        → Work Package
          → Issue / Change
            → Test / Evidence
              → Release Claim
```

| 层级 | 必须包含 |
|---|---|
| Stage | 目标、范围、非目标、进入/退出门、风险预算 |
| Capability | 用户价值、系统对象、Requirement ID、owner |
| Vertical Slice | 端到端路径、故障路径、观测点、演示场景 |
| Work Package | 输入、输出、依赖、验收、预计风险；建议 1–5 个工作日 |
| Issue/Change | 单一结果、关联 ID、测试方案 |
| Evidence | commit、环境、命令、原始输出、断言、复现方法 |
| Release Claim | profile、assurance、适用范围、证据引用和已知限制 |

## 4. 状态模型

### 4.1 工作项

```text
PROPOSED → TRIAGED → READY → IN_PROGRESS → REVIEW
                                      ↘ BLOCKED
REVIEW → VERIFIED → DONE
```

- `READY`：边界、依赖、验收条件和责任人完整。
- `VERIFIED`：测试和证据已完成，不等于已经发布。
- `DONE`：文档、代码、证据和发布说明均已同步。
- `BLOCKED`：必须记录阻塞原因、已尝试方案、需要的决定和复查时间。

### 4.2 技术决策

```text
QUESTION → CANDIDATE → POC → ACCEPTED
                         ↘ REJECTED
ACCEPTED → SUPERSEDED
```

“推荐”不等于 `ACCEPTED`；没有 ADR 和规定证据的选择不得冻结为稳定契约。

## 5. Definition of Ready

工作项进入实施前必须具备：

- 对应 Stage/Capability 和 Requirement ID；
- 明确范围、非目标、依赖和兼容影响；
- happy path、failure path、安全路径的验收条件；
- 是否影响 KABI/SABI/schema/durable format；
- 测试层级与 Evidence 产物；
- 对破坏性迁移、数据丢失和回滚的说明。

## 6. Definition of Done

工作项只有同时满足以下条件才能完成：

- 实现与规范一致；
- unit、integration、conformance 或 fault-injection 测试按风险通过；
- 无未解释的 capability/resource/durability 绕过；
- metrics、trace、typed error 和 Receipt 足以诊断；
- 相关设计、ADR、运行说明和已知限制同步；
- Evidence 可从干净环境复现；
- 兼容性或数据格式变化具有迁移/回滚路径。

## 7. 评审与决策机制

| 变化 | 最低要求 |
|---|---|
| 局部实现，不改变契约 | 普通代码评审 + 自动测试 |
| 新依赖或可替换组件 | 技术选型记录 + license/security/maintenance 检查 |
| KABI/SABI/schema/durable format | ADR + compatibility review + golden vector |
| TCB、Capability、secret、sandbox | threat review + bypass test |
| Resource/Budget/durability | property test + crash/fault injection |
| Stage 退出或 production claim | Evidence review + 未知风险清单 + 明确批准 |

ADR 至少包含：上下文、约束、候选、比较、决定、后果、退出策略、证据、复审触发器。

## 8. 需求与证据台账

建议建立机器可读台账：

```text
management/
  README.md
  stage-b-technology-selection.md
  stages/
  adrs/
  claims.yaml
  risks.yaml
  evidence-index.yaml
```

`claims.yaml` 的最小字段：

```yaml
- requirement_id: FIBER-MN-001
  stage: B
  status: planned
  implementation_refs: []
  test_refs: []
  evidence_refs: []
  assurance: DESIGN
  limitations: []
```

在 schema 确定前不批量生成空台账；首先用阶段 B 的第一个纵切面验证字段是否足够。

## 9. 风险管理

风险按 `影响 × 发生概率 × 不可逆性` 排序，至少维护：

- 安全与权限旁路；
- durable 状态丢失或双重提交；
- Resource/Budget 超卖；
- cancellation/effect unknown；
- ABI/数据格式过早冻结；
- runtime 或 UI 技术锁定；
- 规模目标被冷 metadata 数量冒充；
- 单人关键知识和维护依赖；
- 第三方依赖 license、供应链和停止维护。

P0 风险阻止 Stage 退出；P1 必须有 owner、缓解措施和明确复查点。

## 10. 分支、提交与发布

- 默认从 `main` 创建短生命周期 `codex/<topic>` 或功能分支；紧急修复例外需记录。
- 一个 canonical 结果对应一个可解释、可独立验证和回退的原子提交；提交信息说明“做了什么、为什么、证据是什么”。
- 开始任务时记录 base HEAD；提交前检查 HEAD 漂移，漂移后重新审查 read-set/write-set 并复验。
- 只显式暂存当前 Task/Attempt 的 write-set；共享脏工作区禁止无差别 `git add -A`/`git add .`，不得顺带提交其他 Agent 或用户改动。
- 暂存后检查 staged name/diff、`git diff --cached --check`、适用测试和 Evidence；未运行项目必须明确记录。
- 禁止把密钥、真实凭证、用户数据和不可再生大型输出提交到仓库。
- 未达到 DoD 的候选不得冒充完成；确需 checkpoint commit 时必须标记 `WIP/POC/PARTIAL`。
- 多 Agent 的同一 canonical 对象由单一 integrator 提交；禁止擅自 amend/rebase/reset/force-push 或改写他人历史。
- 合并前运行与变更风险相称的测试，保留命令和结果。
- 发布 Claim 只来自已提交 Evidence；设计文档中的未来时态不得出现在能力声明中。
- 破坏性 schema/数据迁移先备份、演练恢复，再允许发布。
- push 前核对 branch/upstream/HEAD，只允许 fast-forward；push、CI、部署分别确认，任何一级成功都不能冒充后一级成功。
- 详细规范见[项目知识渐进式披露与自动 CRUD 规则第 8 节](./project-knowledge-progressive-disclosure.md#8-控制与提交协议)。

## 11. 节奏与看板

建议使用滚动两周节奏，但以验收门而非日期驱动：

- 周期开始：选择一个纵切面和最多两个主要风险；
- 每日：更新工作项状态、阻塞和最新 Evidence；
- 周中：运行集成/故障测试，避免结束前集中发现架构问题；
- 周期结束：演示端到端路径、复盘偏差、更新 ADR/风险/证据；
- 每四个周期：重新检查阶段范围、技术选型和性能基线。

限制在制品：每位实现者最多一个主要 `IN_PROGRESS` Work Package；紧急故障除外。

## 11.3 阶段 E 管理入口（兼容制度与生态规模化，当前活跃主线 IN_PROGRESS / NOT EXITED）

- 阶段 E 权威进度单与依赖感知互斥波次编排：[stage-e-progress.md](./stage-e-progress.md)（2026-10-29 `W76` 经维护者批复阶段 C/D 正式退出后立项开启，2026-10-30 `W76.1` 增量纳管契约冻结收口与开箱试用先行波，2026-10-30 `W77-PRE` 完成先行开箱试用工作包 **`E0`（`E-TRIAL-BOOTSTRAP`） / 里程碑 `M-E0.5`（`DONE`，保障等级：`H4`；证据 [E-TRIAL-001](../evidence/stage-e/e-trial-001-llm-http-driver.md)、[E-TRIAL-002](../evidence/stage-e/e-trial-002-quickstart-and-starter-app.md)）**，同日 `W77` 完成兼容治理首包 **`E1`（`E-SABI-POLICY-CORPUS`） / 里程碑 `M-E1`（`DONE`，保障等级：`H4`；证据 [E-SABI-001](../evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md)、[E-SABI-002](../evidence/stage-e/e-sabi-002-five-lang-golden-corpus.md)——达成 9/9 SABI 通道全量入冻、`N / N-1 / N-2` 兼容矩阵、`deprecation-manifest.yaml` 弃用遥测与五语言 `v1.0` 黄金不可变语料库）**，**2026-10-31 `W78` 完成核外兼容垫片 `E2`（`E-COMPAT-SHIM`）与迁移工具链 `E3`（`E-MIGRATE-KIT`） / 里程碑 `M-E2`（`DONE`，保障等级：`H4`；证据 [E-SHIM-001](../evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md)、[E-MIGRATE-001](../evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md)）**，当前阶段总体状态 **`IN_PROGRESS / NOT EXITED`**；后续四个工作包 **`E4`..`E7` 仍严格保持 `NOT_STARTED` / `DESIGN`**——含待 `W81` 决策点 `E2`（`[ROAD-E-001]`）将 `SABI v1-beta` 晋升为 `SABI v1.0` 长期冻结闭合 `RISK-B-05`、`W79` 闭合 `U-7`/`U-8`、`W80` 闭合 `U-9`、`W79/W81` 闭合 `RISK-B-09` CI 门，并诚实对比列明 `E-deferred` 4 项外部物理硬件/商业资质/组织边界、关键决策护栏 `E0–E4` 与 §E.5.5 六项退出门禁）
- 阶段 E 总体架构决策记录（长期兼容制度、核外兼容垫片与生态商店治理）：[ADR-0027](./adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md)（**`ACCEPTED`** 2026-10-31 `W78` 晋升，`ACCEPTED != VERIFIED`；2026-10-29 `W76` 立项 / 2026-10-30 `W76.1` 增补，`E0` 开箱试用边界、`E1` `9/9` SABI 通道冻结/版本兼容/弃用矩阵与五语言 `v1.0` 黄金语料库、`E2` 核外无特权兼容垫片 `nlos-compat-shim` 及 `E3` `nlos-migrate` 静态检查器/三态迁移沙箱已于 `W77-PRE` / `W77` / `W78` 达成 `H4` 验证，后续工作包 `E4`..`E7` 仍为 `DESIGN`）
- 需求/风险/证据机器台账 Schema：[ledger-schema.md](./ledger-schema.md) **`v1.3`**（2026-10-29 `W76-SCHEMA-V13` 提交 `ed64ae3` 按 `FROZEN-additive` 纪律完成 `v1.2 -> v1.3` 纯增量升级；`2026-10-31 W78` 同步收录 `E-SHIM-001` 与 `E-MIGRATE-001`（连同前序 `E-TRIAL-001`、`E-TRIAL-002`、`E-SABI-001`、`E-SABI-002`）后 [evidence-index.yaml](./evidence-index.yaml) 共 **`194`** 条证据索引，`lint_claims.py` `0 ERROR`、`test_lint_claims` `44/44 OK`；严守 `Claim <= Evidence` 纪律，`E4`..`E7` 未验证前零 `stage: "E"` 空产声明录入）
- 规范路线：[v0.5 第 28.5 节](../design/06-架构设计总纲-v0.5.md)（`[ROAD-E-001]`）

## 11.4 阶段 D 管理入口（完整自然语言桌面，已 EXITED（2026-10-29 W76 维护者显式批复退出））

- 阶段 D 权威进度单与双轨并行编排：[stage-d-progress.md](./stage-d-progress.md)（2026-10-26 `W68` 开立，并经 `W68-R1` 去 AI 味工业工作站修订与 `W68-R2` 12 域显控同构 + `L0–L3` 渐进式披露/SOTA 显示升级；2026-10-27 至 2026-10-29 `W69–W74` 已 100% 完成 `D-core` 全部八大工作包——`D-TOKENS-KIT` [D-TOKENS-001](../evidence/stage-d/d-tokens-001-workstation-tokens-and-sota-kit.md)、`D-IPC-ENUM` [D-IPC-001](../evidence/stage-d/d-ipc-001-twelve-domain-enum-and-symmetry.md)、`D-COMPOSITOR-WM` [D-COMPOSITOR-001](../evidence/stage-d/d-compositor-001-five-plane-tiling-wm-and-session.md)、`D-OMNIBAR-MULTIMODAL` [D-OMNIBAR-001](../evidence/stage-d/d-omnibar-001-command-intent-palette-and-multimodal.md)、`D-TRUSTED-UI` [D-TRUSTED-UI-001](../evidence/stage-d/d-trusted-ui-001-z4-consent-chooser-and-taint-clipboard.md)、`D-TASK-SPACE-DAG` [D-TASK-SPACE-001](../evidence/stage-d/d-task-space-001-l1-l3-disclosure-dag-and-attempts.md)、`D-NOTIFY-SEARCH-OFFLINE` [D-NOTIFY-SEARCH-001](../evidence/stage-d/d-notify-search-001-action-token-spotlight-and-offline.md)、`D-CONTROL-SETTINGS-A11Y` [D-CONTROL-SETTINGS-001](../evidence/stage-d/d-control-settings-001-twelve-domain-tree-and-settings-cas.md) / [D-A11Y-I18N-001](../evidence/stage-d/d-a11y-i18n-001-wcag22-aria-and-bilingual-conformance.md) / [D-GUI-CAMPAIGN-001](../evidence/stage-d/d-gui-campaign-001-cross-platform-workstation-campaign.md)、退出门 `[ROAD-D-001]` 端到端实证 [D-ROAD-001](../evidence/stage-d/d-road-001-non-developer-multi-app-e2e-gate.md)、独立退出门终审 [w74-stage-d-exit-gates.md](../evidence/stage-d/reviews/w74-stage-d-exit-gates.md) [`SATISFIED-WITH-BOUNDARIES`, `H4`, `P0=0`] 及 `W75`/`W76` 远端全矩阵 CI 13/13 全绿；**当前状态：已 `EXITED`（2026-10-29 `W76` 维护者显式批复退出）**，生态延续边界已移交 [stage-e-progress.md](./stage-e-progress.md) §E.1.3）
- 工业工作站桌面合成层、`L0–L3` 渐进式披露、12 域显控同构与可信 UI 防伪边界：[ADR-0025](./adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)（**`VERIFIED`** 2026-10-29 `W74`：2026-10-26 `W68` 定案并经 `W68-R1/R2` 修订 1–2，由 `W69–W74` `D-TOKENS-001..D-ROAD-001` 与独立退出门终审 [w74-stage-d-exit-gates.md](../evidence/stage-d/reviews/w74-stage-d-exit-gates.md) 实证闭环 `Z0–Z4` 五平面合成隔离、窗口平铺优先 + `36px` 底部紧凑任务栏 + `Ctrl+Space` 按需命令面板、UAC/Polkit 级实心 `Z4` 系统安全确认屏、`ADR-0011` 单一控制脊柱、`[UI-PARITY-001]` 12 域显控同构及 `L0→L1→L2→L3` 四级按需渐进式披露）
- 阶段 D 完整自然语言桌面视觉、渐进披露与交互设计规格：[2026-10-26-stage-d-desktop-visual-and-ux-design.md](../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md)（`W68-R1/R2` 修订版并已于 `W69–W74` 全部落地验证：采用实心中性 `Graphite (#111214/#18191C)` 与 `Paper (#E5E7EB/#FFFFFF)` 双主题、`0/4/6px` 锐利边角、零毛玻璃零发光零 Emoji 纪律；融合六大工业级 SOTA 可视化原语——Headlamp 权限/状态动作裁剪与作用域面包屑、Dagster 待物化折叠超节点、Temporal/Perfetto 阶段耗时瀑布与游标虚拟化、Nomad 三水位配额子弹条、System Informer 十层父子归属树与 Terraform 式级联爆炸半径预览，配套 `IPC-D1..D8` 全量补面契约）
- 规范路线：[v0.5 第 28.4 节](../design/06-架构设计总纲-v0.5.md)（`[ROAD-D-001]`）

## 11.5 阶段 C 管理入口（分布式现代系统，已 EXITED（2026-10-29 W76 维护者显式批复退出））

- 阶段 C 权威进度单与编排：[stage-c-progress.md](./stage-c-progress.md)（2026-09-22 决策点 0 批准生效；经 `W35–W67` 完成单机多进程域 C-core 九包与 `ROAD-C-001`/`ROAD-C-002` H6 评审 [W67](../evidence/stage-c/reviews/w67-stage-c-exit-gates.md)，并经 `W68–W73` 完成跨物理主机 `C-XHOST` 五包与联合退出终审 [W73](../evidence/stage-c/reviews/w73-stage-c-xhost-exit-review.md) [`SATISFIED-WITH-BOUNDARIES`, `H6`, `P0=0`] 及 `W75`/`W76` 远端全矩阵 CI 13/13 全绿、`B-SCALE-RELEASE-001` `e77c094` 三平台 `--release` 规模数字回填；**当前状态：已 `EXITED`（2026-10-29 `W76` 维护者显式批复退出）**，延续硬件与生态边界已移交 [stage-e-progress.md](./stage-e-progress.md) §E.1.3）
- 阶段 B 已 EXITED：[stage-b-progress.md](./stage-b-progress.md)（§6.5.6 十七项移交清单为 C 册输入）
- 单机双进程 Cell×2 拓扑：[ADR-0018](./adrs/0018-single-host-multiprocess-dual-cell-topology.md)（`ACCEPTED`，否决窗口至 W36 屏障；`ACCEPTED` ≠ `VERIFIED`）
- 跨 Cell 提交语义扩展点：[ADR-0019](./adrs/0019-cross-cell-commit-semantics-extension.md)（**`VERIFIED`** 2026-10-08：Phase 0 证据车道族满足修订 2 R2.2 四条件（独立审查 pass-with-fixes）；效力域=ADR-0018 单机双进程拓扑（R1.2.2/2.3/2.4 + H6 六类目双存活注入），endpoint 仅 TaskStore；跨机/真分区/跨时钟域与物理 cleanup 残域保留（R1.4 触发器 7 / R2.3）；修订 2（2026-10-08）消除循环前置并界定 VERIFIED 范围；**C-SHARD 本体（Phase 1）派发门自此打开，派发前按计划 §5.6 另行确认**）
- 控制面单写者与共识基底时序：[ADR-0020](./adrs/0020-control-plane-single-writer-and-consensus-timing.md)（**`VERIFIED`** 2026-10-28 `W73`：单机多进程域 `S-B` 覆盖语义与跨主机 `S-B -> S-A` 嵌入式确定性 Raft 控制面多数派升级路径已由 [ADR-0026](./adrs/0026-cross-physical-host-transport-and-consensus-expansion.md)、[C-XHOST-003](../evidence/stage-c/c-xhost-003-embedded-raft-control-plane-consensus.md) 与 [W73 独立终审](../evidence/stage-c/reviews/w73-stage-c-xhost-exit-review.md) 实证闭环，`S-B` 保留为单机 `N=1` 零开销直通特例）
- lease grantor 共享 `CellAuthority` 所有权（capacity/device 进 CellHost 装配）：[ADR-0021](./adrs/0021-lease-grantor-shared-cell-authority.md)
- capability 签发闭环 operator 密钥文件路径：[ADR-0022](./adrs/0022-operator-root-capability-issuance.md)
- 语义写入端 driver 回调桥（解除 D6 后置）：[ADR-0023](./adrs/0023-semantic-write-side-driver-bridge.md)
- DriverProvider 载荷编解码通道冻结（第 8 个 SABI 通道 `nlos.sabi.DriverProvider` v1.0，闭合 `RISK-B-05 review_point` 与 `U-10`）：[ADR-0024](./adrs/0024-driver-provider-payload-codec-freeze.md)
- `qq-agent` Dogfood 全量 7 项上游问题闭环（`L-001` ~ `L-007`：第 9 个 SABI 通道 `nlos.sabi.ChannelControl` v1.0 + `SystemControlDaemon` 挂载 + Python `ChannelClient`、`nlos-resource` v8 LLM Token/API 调用预算账本、`directory_chain.py` Windows 粗时钟防碰撞、`sdk/python/pyproject.toml` 打包与 `ProactorEventLoop` 诊断、Conformance/Channel 测试轨迹输出）：[W73-DOGFOOD-001](../evidence/stage-c/w73-qq-agent-dogfood-closure.md)
- 跨物理主机蜂窝网络传输、有界时钟偏斜与控制面复制基底扩展（阶段 C `C-XHOST` 收官立项）：[ADR-0026](./adrs/0026-cross-physical-host-transport-and-consensus-expansion.md)（**`VERIFIED`** 2026-10-28 `W73`：`W69–W72` 完成 `C-XHOST-001..005` 全部五工作包，并经 `W73` 独立退出终审 [w73-stage-c-xhost-exit-review.md](../evidence/stage-c/reviews/w73-stage-c-xhost-exit-review.md) 核准晋升 `VERIFIED`，退役 `RISK-B-11` / `U-5` 跨机网络/时钟/共识半边，支撑阶段 C 于 `2026-10-29` `W76` 正式获批 `EXITED`）
- C-SHARD 工作包计划：[c-shard-plan.md](./c-shard-plan.md)（2026-10-08 附条件采纳；Phase 0 证据车道族 `C-SHARD-001..004` 与 Phase 1 本体迁移车道族 `CS1-A..E` `C-SHARD-005..006` 已于 2026-10-24 `W62/W63` 全部完成；效力域限定 ADR-0018 单机双进程拓扑，跨机与远端物理 cleanup 残域按 `RISK-B-11` 保留）
- Cell 骨架 DESIGN spec：[2026-09-22-cell-skeleton-design.md](../superpowers/specs/2026-09-22-cell-skeleton-design.md)（不授权七件套实现）

## 12. 阶段 B 管理入口

阶段 B 当前管理入口：

- 项目知识规则：[project-knowledge-progressive-disclosure.md](./project-knowledge-progressive-disclosure.md)
- Rust 导读：[rust-for-nlos.md](./rust-for-nlos.md)
- 技术选型：[stage-b-technology-selection.md](./stage-b-technology-selection.md)
- 阶段 B 权威进度单：[stage-b-progress.md](./stage-b-progress.md)
- TaskAuthority 提交恢复归属：[ADR-0004](./adrs/0004-task-authority-commit-recovery-owner.md)
- TaskWriteSet authority-first 顺序：[ADR-0005](./adrs/0005-task-write-set-authority-first.md)
- Semantic publication receipt 权威归属：[ADR-0006](./adrs/0006-semantic-publication-receipt-owner.md)
- Topic 服务层单 log fanout 模型：[ADR-0007](./adrs/0007-topic-service-single-log-fanout.md)
- Durable wait registry 权威归属：[ADR-0008](./adrs/0008-durable-wait-registry-authority.md)
- Fiber 事件溯源续跑为主、受控快照兜底：[ADR-0009](./adrs/0009-fiber-event-sourced-resume.md)
- 跨进程 Principal 认证签名贯穿：[ADR-0011](./adrs/0011-ipc-principal-auth-signature-passthrough.md)
- Fiber replay 登记式投影与入口快照：[ADR-0012](./adrs/0012-fiber-projection-registration-and-entry-snapshot.md)
- 跨 authority verify-then-commit 契约：[ADR-0013](./adrs/0013-cross-authority-verify-then-commit-contract.md)
- Schema 注册表通道冻结 v1-beta：[ADR-0014](./adrs/0014-schema-channel-freeze-v1-beta.md)
- 不可信代码执行双层隔离（独立 host Process + 进程内 Wasmtime 按信任档分层）：[ADR-0015](./adrs/0015-dual-layer-untrusted-execution-isolation.md)
- TaskPlan/TaskNode 声明面落点（独立 nlos-plan authority + manifest 模板来源组合）：[ADR-0016](./adrs/0016-task-plan-declaration-surface.md)
- Resource/Operation 跨 authority prepare/finalize 入口：[ADR-0017](./adrs/0017-resource-operation-cross-authority-prepare-finalize.md)
- 多语言 SDK 支持评估：[language-sdk-support-plan.md](./language-sdk-support-plan.md)
- 规范路线：[v0.5 第 28.2 节](../design/06-架构设计总纲-v0.5.md#282-阶段-b单机通用应用平台)
- 执行层级决策：[议题 29](../discussions/29-现代系统执行层级与机制迁移.md)

第一个建议纵切面：

```text
signed Package
  → install Application
  → create Task/TaskPlan
  → materialize Process/AgentInstance/Fiber
  → async Driver Operation
  → Artifact + Receipt
  → cancel/crash recovery
  → CLI/NL inspect and control
```

在该纵切面通过前，不并行冻结完整桌面框架、分布式数据库或公共插件 ABI。

阶段 B 的实现状态、当前主线工作包、提交和 Evidence 以[权威进度单](./stage-b-progress.md)为准。任何完成的实现或验证工作都必须在对应 canonical commit 中同步更新该进度单。
