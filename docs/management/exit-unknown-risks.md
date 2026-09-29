# 阶段 B 退出未知/残余风险清单（W34-C）

> 状态：`W34-C` 交付（2026-09-21，base `a01fd57`；工作区未提交，由控制器集成）。
>
> 对应 [stage-b-progress §6.5.5](./stage-b-progress.md) 条件 3（「未知风险清单显式列出，各项有 owner 与缓解措施」）与条件 4 的清单半边；[管理机制 §7](./README.md#7-评审与决策机制)「Stage 退出或 production claim」三件套之一。
>
> 机器台账权威：[risks.yaml](./risks.yaml)（本清单为其人读视图 + 未知项扩展，两者同源同步，W34-B 同波维护）；claims 侧口径限制见 [claims.yaml](./claims.yaml) 各 limitations。
>
> 评审输入：[W31-G](../evidence/stage-b/reviews/w31g-road-b004-gates.md)（ROAD-B-004 G1–G6）、[W33-H](../evidence/stage-b/reviews/w33h-road-b001-b002.md)（ROAD-B-001/002）、[W34-A](../evidence/stage-b/reviews/w34a-six-gate-matrix.md)（六门矩阵 + X-1..X-6）。

## 0. P0 声明（§6.5.5 条件 1 的 P0 半边 + 条件 3 的清零确认）

**截至 2026-09-21（base `a01fd57`）：P0 未决风险 = 0。**

依据：

1. risks.yaml 无 P0 条目（12 条：P1×10、P2×2）；lint 对「P0 未闭环」的 INFO 探针零触发。
2. 三份评审记录均无 P0 级证据反例：W33-H §3.2 明示「本评审范围内未见 P0 级证据反例」；W31-G 无 NOT-SATISFIED 门；W34-A §8 两 PARTIAL-residuals 门（B-001/B-005）的 residual 清单均为登记级（具名、可溯源、有处置路径），非证据反例。
3. 全部生产量级声明（PID 级 Agent 容量、coroutine 级大规模并发）按门原文禁令未发布（见 U-1）。

**升级条款**：RISK-B-12（slice-k-demo STEP 09d defect）若根因定位证明为生产 GC 误收在册 blob，即刻升级 P0（durable-state-loss）并阻止阶段退出——该升级判定先于 W34-D 批准生效。

## 1. Owner 总则

当前项目为单人维护（全部 W27–W33 收官车道由单一控制器串行集成）——该集中度本身即 RISK-B-08（P1/open）。因此：

- 下表默认 owner 为**维护者**（单人）；不构成缓解，只构成登记事实。
- **W34-D 用户门须显式称量**：(a) 是否接受 RISK-B-08 维持 open 退出；(b) 阶段 C 是否引入第二维护者及本清单各项 owner 重新分配。
- 知识外化缓解已全库化：143+ 证据文件、ADR-0001..0017、机器台账（claims/risks/evidence-index + lint 机械门）、§6.5.3 车道编排与增量日志、本清单。

## 2. 已登记风险视图（risks.yaml 摘要，含 owner/缓解/复查点）

| ID | 类别 | 级别/状态 | owner | 缓解现状 | 退役/复查证据 |
|---|---|---|---|---|---|
| RISK-B-01 | security-bypass | P1/partial | 维护者 | attenuation/fence + kill-window 矩阵 + IPC 认证（ADR-0011）+ GUI 认证唯一路径 + 强制签名收敛 | 真实 Capability authorizer + principal peer attestation 接线并过 threat review/bypass test |
| RISK-B-02 | durable-state-loss | P1/partial | 维护者 | 双 Attempt 唯一 permit + 全表组故障矩阵 + 统一恢复面 + 六域闭环验收 + 分层掉电阶梯（层 1/2） | 真实硬件掉电证据（X-6 推进）+ Artifact 混合终结门补齐 |
| RISK-B-03 | resource-oversell | P1/partial | 维护者 | high-water/quarantine/finalize-refund + cost-receipt 桥接 + ADR-0017 coordinator | 真实 enforcement shim 与平台 Device adapter |
| RISK-B-04 | cancel-effect-unknown | P1/partial | 维护者 | EFFECT_UNKNOWN reconcile + 统一恢复 + provider fence（W30-C）+ teardown 链 | pause/resume/cancel 宿主执行器接线 |
| RISK-B-05 | premature-format-freeze | P1/partial | 维护者 | ADR-0014 additive-only 冻结 + SABI v1.2→v1.5 全程 additive + 平行签名模板面 | SABI 正式冻结决策 + driver codec 入通道决策（均须复审） |
| RISK-B-06 | runtime-ui-lockin | P2/partial | 维护者 | ADR-0001/0015 显式决策 + GUI 单控制路径架构约束（认证 IPC + 四路径 parity） | GUI 真机战役收尾（见 U-3）+ 新重依赖选型门 |
| RISK-B-07 | scale-impersonation | P1/partial | 维护者 | G1–G6 SATISFIED-WITH-BOUNDARIES + 逻辑 TaskNode 口径钉死 + claims 按口径限定 | release-profile 复测 + 多平台数字（见 U-1） |
| RISK-B-08 | single-person-dependency | P1/open | 维护者（兼唯一知识载体） | 知识外化全库化（证据/ADR/台账/编排留痕）——登记态非闭环 | 阶段 C 第二维护者实际接入并独立复现一次全仓门 |
| RISK-B-09 | third-party-supply-chain | P2/open | 维护者 | 无（open；desktop Tauri/Node 面为新暴露） | 依赖树专项复审 + license/维护性检查记录 |
| RISK-B-10 | cancel-effect-unknown | P1/open | 维护者 | Windows kill CI 契约道 step 级绿（精确口径见 risks.yaml）+ claims 口径限定 | Windows 实机 live-child 实杀 + B2-1 双活三层场景证据（见 U-2） |
| RISK-B-11 | durable-state-loss | P1/open | 维护者 | claims 显式限定单机范围；多 Cell 属阶段 C | 阶段 C 多 Cell PoC + 跨 Cell barrier/物理 cleanup 证明（见 U-5） |
| RISK-B-12 | durable-state-loss | P1/open | 维护者（处置决定归 W34-D） | demo 级定级 + 登记 + W33-B 现行演示替代 | 根因定位 + 修复测试或 demo 显式退役记录（见 U-6；含 P0 升级条款） |

## 3. 未知项清单（unknowns：当前无证据、不可现在判定；§6.5.5 条件 3「未知风险」本体）

每项：未知内容 / owner / 当前缓解（防外推）/ 何种证据可退役。

- **U-1 release-profile 与多平台规模行为**：release 构建 + Linux/Windows 上的 10K/100K（TaskNode/注册维/fiber/working-set/rehydrate/reclaim）行为未知；现有数字全部 debug/test、单平台 macOS。owner：维护者。缓解：ROAD-B-004/006 claims 与 RISK-B-07 按口径限定；PID 级/coroutine 级生产量级声明未发布。退役证据：B-PLAN-001 §9 / B-TASK-SCALE-001 §13/§14 / B-RUNTIME-002 追加 release+双平台数字（含 100K cancel/batch-cancel 探针与 wake fairness 多 worker 口径）。
- **U-2 Windows 实机终止语义**：taskkill /F/T 在真实进程树上的行为（live-child、权限边界、孤儿）未知；CI 契约道 step 绿不含实杀断言（W34-A §6 residual 4）。owner：维护者。缓解：RISK-B-10 登记 + claims 口径限定。退役证据：Windows 实机 kill 矩阵（B6-2）+ W29-F 双活三层场景 Windows 实杀（B2-1）。
- **U-3 GUI 真机可用性**：Tauri 壳在真实用户机的交互可用性（点按/权限/多窗口/中文输入/长期运行）未知；现有证据为命令层集成测试 + 窗口注册 + CLI 活体冒烟。owner：维护者。缓解：computer-use 交互清单已登记为收尾动作（§6 B-005 行）；五层 inspect desktop 派发接线显式未做（操作者路径走 CLI）。退役证据：computer-use 清单全量跑完并落 evidence。
- **U-4 真实硬件掉电**：kill-9=页缓存存活建模；层 1 APFS 校准与层 2 dm-flakey 已有，M4/M6/M8 模型校准无专项数据，真实物理掉电未做。owner：维护者。缓解：分层阶梯策略 + 各 evidence disclaimer 不外推。退役证据：真机掉电设备证据，或 M4/M6/M8 校准完成（B-STORE-FS-SEMANTICS-001 推进）。
- **U-5 多 Cell/跨机原子提交**：跨 Cell barrier、远端物理 cleanup 证明、跨机器原子性——未设计未实现，行为不可判定。owner：维护者。缓解：claims 全部限定单机；RISK-B-11。退役证据：阶段 C 多 Cell PoC + cross-cell 一致性证明。
- **U-6 slice-k-demo 09d 根因**：auto-GC blast radius 嫌疑未定位；若为生产 GC 误收在册 blob 则升级 P0。owner：维护者（W34-D 裁量处置）。缓解：RISK-B-12 登记；测试面不受影响；W33-B 为现行演示。退役证据：根因定位报告 + 修复测试（或 demo 退役决定记录）。
- **U-7 G3 未证伪三条件与授权面**：Namespace/ResourceContract/fanout gate 对应权威未落，门语义三条件不可证伪；WAITING_AUTHORIZATION 解除无 enforcement。owner：维护者。缓解：W31-G 边界带入 ROAD-B-004 limitations。退役证据：对应权威落地后的门测试（物化门负路径扩展）。
- **U-8 G4 生态 selector 半边**：Package/Skill/Tool/Model/Artifact/Topic/外部服务 typed selector→generation handle 解析未落，其上的「latest 不得当已授权依赖」不可判定。owner：维护者。缓解：现以 input_selectors_digest 摘要绑定。退役证据：生态 selector 解析车道落地 + 负路径测试。
- **U-9 载荷执行面**：manifest `executable` 字节不被真实执行——第三方应用「运行」的证据对象是公共任务/操作/纤维面。owner：维护者（移交阶段 C 裁量）。缓解：构造性证明边界具名（W33-H §2.1）。退役证据：内核侧载荷执行车道 evidence。
- **U-10 真实 provider 行为**：driver plane 载体为确定性 mock（含真实 IPC）；真实 provider 的降级谱系/认证/attestation 行为未知。owner：维护者。缓解：W30-C 语义门（降级可见/typed 拒绝/fence）已在 mock 面钉死。退役证据：首个真实 provider 经 driver plane 全链接入 evidence。
- **U-11 矩阵与探针缺口**：100K@50% working-set cell、occupancy 回收再入场动态、调度器自身规模探针、PINNED tier、apply 侧 TaskNode admission consult、100K 级 cancel 探针。owner：维护者。缓解：逐项登记（W31-G §8.2 / W34-A §6）；不据此宣称门正式达成之外的能力。退役证据：各缺口补跑/落地后追加 evidence 节。
- **U-12 跨进程并发 GC tick**：单写者前提下键不相交为构造性论证，跨进程并发 tick 未证明。owner：维护者。缓解：B-ARTIFACT-004 §8.7 登记。退役证据：并发 tick 测试或显式单写者契约文档化。
- **U-13 当前 CI run 结论**：HEAD `a01fd57` run `35531478487`（+ Pages `35531478300`）为 B-001/B-003/B-004/G6 边界/X-5 共同收口载体，W34-A 评审时 in_progress——最终 conclusion 未知。owner：维护者。缓解：workspace 全仓门已本地收口（275 二进制/1536 passed/0 failed/20 ignored）。退役证据：run 绿 + 链接回填 §6 对应行。
- **U-14 TS/Python conformance 覆盖漂移**：W28-D/W29-D/W32-G 新增 SABI 臂/视图未钉 TS/Python golden——跨语言面是否零漂移未证。owner：维护者。缓解：Deferred minor 登记（W34-A §7 X-3 行）。退役证据：三语言 golden 同步扩展。

## 4. 已决定递延项（非未知：登记在案、有明确处置路径，W34-D 裁量豁免/移交阶段 C）

载荷执行面（U-9 同体）；`nlos package install` CLI；迁移面 kill-9 矩阵/abandon 命令/跨进程并发注入；patch-downgrade 拒绝；跨进程 uninstall 审批；capability revoke；uninstall 不解除 artifact 引用；生命周期 NL 动词（uninstall/disable application）；supervisor 自动 pid 发现/unregister；`restore_process` 复活链与干净退出终态路径；teardown 并发竞争面；pause/resume/cancel 宿主执行器；throttle durable 落账与 reclaim 真实计数；semantic 域 NL 语法；desktop 测试并入根 CI；Windows GUI 认证面；W32-F 四呈现边界（载荷渲染/生命周期状态机/呈现走本地直读/库 API 入口）；AgentInstance 独立分面；driver payload codec 入冻结通道；X-1/X-2 最小口径之外的能力面。

（出处：W33-H §3.2/§4、W31-G §8.2、W34-A 各门 residual 段；移交登记由 W34-D 在 §6.5.5 条件 5 下完成。）

## 5. 维护

- 本清单与 risks.yaml / claims.yaml 同波更新（W34-B/C 交付）；风险状态变化（尤其 RISK-B-12 重定级）必须同步两处。
- lint（`python3 scripts/lint_claims.py`）覆盖机器台账一致性；本清单为 docs，不入 evidence-index（schema v1 索引域限定 `docs/evidence/stage-b/`）。
