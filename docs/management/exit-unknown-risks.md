# 阶段 B / 阶段 C 退出未知与残余风险清单（W34-C 基线 + W67 阶段 C 退出评审同步）

> 状态：`W34-C` 交付（2026-09-21）并经 **`W67` 阶段 C 退出评审同步更新**（2026-10-26，base `9e5e0edc`；同时满足 [stage-b-progress §6.5.5](./stage-b-progress.md) 与 [stage-c-progress §C.5.5](./stage-c-progress.md) 条件 3）。
>
> 对应 [stage-b-progress §6.5.5](./stage-b-progress.md) 条件 3（「未知风险清单显式列出，各项有 owner 与缓解措施」）与条件 4 的清单半边；[管理机制 §7](./README.md#7-评审与决策机制)「Stage 退出或 production claim」三件套之一。
>
> 机器台账权威：[risks.yaml](./risks.yaml)（本清单为其人读视图 + 未知项扩展，两者同源同步，W34-B 同波维护）；claims 侧口径限制见 [claims.yaml](./claims.yaml) 各 limitations。
>
> 评审输入：阶段 B [W31-G](../evidence/stage-b/reviews/w31g-road-b004-gates.md)、[W33-H](../evidence/stage-b/reviews/w33h-road-b001-b002.md)、[W34-A](../evidence/stage-b/reviews/w34a-six-gate-matrix.md)；阶段 C [W67 退出门综合评审](../evidence/stage-c/reviews/w67-stage-c-exit-gates.md)（`ROAD-C-001` / `ROAD-C-002` / C-core 九包 / 移交 `#1–#17` / 全仓零冻结审计）。

## 0. P0 声明（§6.5.5 条件 1 的 P0 半边 + 条件 3 的清零确认）

**截至 2026-10-26（W67，base `9e5e0edc`）：阶段 B 与阶段 C 范围内 P0 未决风险 = 0。**

依据：

1. `risks.yaml` 无 P0 条目（12 条：P1 closed×1、P1 open×1、P1 partial×8、P2 open×1、P2 partial×1）；`scripts/lint_claims.py` 对「P0 未闭环」探针零触发。
2. 三份评审记录均无 P0 级证据反例：W33-H §3.2 明示「本评审范围内未见 P0 级证据反例」；W31-G 无 NOT-SATISFIED 门；W34-A §8 两 PARTIAL-residuals 门（B-001/B-005）的 residual 清单均为登记级（具名、可溯源、有处置路径），非证据反例。
3. 全部生产量级声明（PID 级 Agent 容量、coroutine 级大规模并发）按门原文禁令未发布（见 U-1）。

**历史升级条款结案存档**：`RISK-B-12`（`slice-k-demo` STEP 09d defect）已于 `87ef288`（[B-SLICE-K-001 §17](../evidence/stage-b/b-slice-k-001-end-to-end.md)）经持久化回执取证排除误收在册 blob，裁定不升 P0 并正式 `closed`。

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
| RISK-B-03 | resource-oversell | P1/partial | 维护者 | 单机高水位/隔离/退款 + ADR-0017 coordinator + 阶段 C 三族持久租约对账（C-LEASE-001）、网关验签结算（C-CUSTODY-001）与断网有界损耗守恒（C-PARTITION-001） | 宿主内核级 enforcement shim 与物理设备重置驱动 |
| RISK-B-04 | cancel-effect-unknown | P1/partial | 维护者 | EFFECT_UNKNOWN reconcile + 统一恢复 + 跨进程 provider cancel-epoch fence（C-PROVIDER-001）+ teardown 链 | pause/resume/cancel 宿主执行器深层接线 |
| RISK-B-05 | premature-format-freeze | P1/partial | 维护者 | ADR-0014 additive-only + ADR-0024 冻结第 8 通道 `DriverProvider` v1.0（闭合 driver codec 残余）+ W66 零 `.proto` 破坏解冻 | 阶段 E 发布 SABI v1.0 长期二进制兼容承诺前复审 |
| RISK-B-06 | runtime-ui-lockin | P2/partial | 维护者 | ADR-0001/0015 显式决策 + GUI 单认证控制路径 + 四入口 Receipt parity（含 Search/Notify 新探查） | 阶段 D GUI 真机战役收尾（见 U-3）+ 新重依赖选型门 |
| RISK-B-07 | scale-impersonation | P1/partial | 维护者 | B-SCALE-RELEASE-001 管线 + C-BENCH-001 Windows `--release` 8-Cell 分片 `10K/100K_LOGICAL`（`WAL+FULL`）实盘数字 + claims 严格限定 | `1M_LOGICAL` 与三平台 CI release 常设数字（见 U-1） |
| RISK-B-08 | single-person-dependency | P1/open | 维护者（兼唯一知识载体） | 169 份证据、ADR-0001..0024、台账 v1.1（101 claims）与记忆恢复手册全库化——登记态非闭环 | 第二人类维护者实际接入并独立复现全仓门 |
| RISK-B-09 | third-party-supply-chain | P2/open | 维护者 | 锁文件入库、核心 Rust 工作区零未审查新重依赖（open；desktop Node 面仍在） | 周期依赖树专项复审 + license/维护性检查记录 |
| RISK-B-10 | cancel-effect-unknown | P1/partial（2026-10-24 分域：Windows 同权限父子树实杀与 B2-1 兄弟树隔离已证；跨权限/breakaway 残域保留，见 U-2） | 维护者 | `b-process-003 §13` 实机双测试 + claims 口径限定 | 残域：Windows Job Object 禁 breakaway 或跨权限回收证据 |
| RISK-B-11 | durable-state-loss | P1/partial（2026-10-26 W67 分域：ADR-0018 单机多进程域 C-core 九包与 `ROAD-C-001/002` 全量闭合；跨物理机网络/时钟域与远端物理 cleanup 残域保留，见 U-5） | 维护者 | 全部 stage: C claims 限定单机多进程持久库拓扑；失联/fence 后一律 `QUARANTINED` + 有界损耗证明 | 跨机残域：真跨物理机网络与共识证据（ADR-0019 触发器 7 / ADR-0020）；物理 cleanup 残域：外部副作用擦除证明 |
| RISK-B-12 | durable-state-loss | P1/closed | 维护者 | 已于 `87ef288`（`B-SLICE-K-001 §17`）根因处置并钉死回归测试，排除误收在册 blob | 已闭环（GC 引用集语义变更时按 §17.2 复验） |

## 3. 未知项清单（unknowns：当前无证据、不可现在判定；§6.5.5 条件 3「未知风险」本体）

每项：未知内容 / owner / 当前缓解（防外推）/ 何种证据可退役。

- **U-1 release-profile 与多平台规模行为（分域口径 2026-10-26，W66/W67）**：**Windows `--release` 分片实盘半边已证**——[C-BENCH-001](../evidence/stage-c/c-bench-001-road-c002-distributed-benchmark.md) 已在 Windows 11 x86_64 `--release`（`SQLite WAL+FULL`）实测 `1->8` 子进程分片扩展与 8-Cell `10K_LOGICAL` / `100K_LOGICAL`（`1%/5%/10%` 工作集、checkpoint/rehydrate、手动控制 `p50=84–85µs`），且三平台 CI 管线 [B-SCALE-RELEASE-001](../evidence/stage-b/b-scale-release-001.md) 已入库；**残域显式保留**：Linux/macOS 同口径 release 常设汇总数字与 `1M_LOGICAL` 档位仍待回填。owner：维护者。缓解：`RISK-B-07`（partial）+ 维持生产量级口号禁令（§C.1.2 第 5 条）。
- **U-2 Windows 实机终止语义（分域口径 2026-10-24，W62-CWINKILL）**：**同权限父子进程树半边已证**——`taskkill /F /T` 在真实 Windows 宿主终止父+子双活进程树（B6-2）及 B2-1 双活三层场景同机兄弟进程树零误伤隔离（[b-process-003 §13](../evidence/stage-b/b-process-003-crash-propagation.md)）。**残域显式保留**：跨权限/UAC 提权子进程、Job Object breakaway 脱离父进程树的孤儿进程回收语义仍未证。owner：维护者。缓解：RISK-B-10（partial）+ claims 口径限定。残域退役证据：Windows Job Object 强制绑定（禁止 breakaway）或跨权限终止验证证据。
- **U-3 GUI 真机可用性**：Tauri 壳在真实用户机的交互可用性（点按/权限/多窗口/中文输入/长期运行）未知；现有证据为命令层集成测试 + 窗口注册 + CLI 活体冒烟。owner：维护者。缓解：computer-use 交互清单已登记为收尾动作（§6 B-005 行）；五层 inspect desktop 派发接线显式未做（操作者路径走 CLI）。退役证据：computer-use 清单全量跑完并落 evidence。
- **U-4 真实硬件掉电**：kill-9=页缓存存活建模；层 1 APFS 校准与层 2 dm-flakey 已有，M4/M6/M8 模型校准无专项数据，真实物理掉电未做。owner：维护者。缓解：分层阶梯策略 + 各 evidence disclaimer 不外推。退役证据：真机掉电设备证据，或 M4/M6/M8 校准完成（B-STORE-FS-SEMANTICS-001 推进）。
- **U-5 多 Cell/跨机原子提交与分布式收敛（分域口径 2026-10-26，W67）**：**ADR-0018 单机多进程拓扑全套机制已证**——覆盖 `C-SHARD` Phase 0+1（schema `v47` 三 Record + 六类 canonical endpoint 屏障 + Ed25519 快照签名接管，`C-SHARD-001..006`）、三族持久租约对账与迁移（`C-LEASE-001`）、签名语义向量检查点与制品传输包收敛（`C-SYNC-001/002`）、双进程 `CellHost` 八步迁移（`C-MIGRATE-001/002`）、断连局部自治与有界配额损耗守恒（`C-PARTITION-001`）及 `ROAD-C-001` / `ROAD-C-002` H6 复合故障门禁（`C-ROAD-001` / `C-BENCH-001`）。**残域显式保留**：①跨物理主机真网络套接字传输、交换机丢包分区与硬件跨时钟域共识（挂 ADR-0018 触发器 2/4、ADR-0019 触发器 7、ADR-0020）；②远端外部副作用物理级擦除证明（`QUARANTINE` 围栏 ≠ 物理擦除）。owner：维护者。缓解：全部 `stage: C` claims 限定单机多进程域；`RISK-B-11`（partial）。
- **U-6 slice-k-demo 09d 根因（已退役 closed，2026-09-21）**：已由 `87ef288`（[B-SLICE-K-001 §17](../evidence/stage-b/b-slice-k-001-end-to-end.md)）定位为 install-scoped 预埋孤儿收集并补齐回归测试，`RISK-B-12` 已 `closed`。
- **U-7 G3 未证伪三条件与授权面**：Namespace/ResourceContract/fanout gate 对应权威未落，门语义三条件不可证伪；WAITING_AUTHORIZATION 解除无 enforcement。owner：维护者。缓解：W31-G 边界带入 ROAD-B-004 limitations。退役证据：对应权威落地后的门测试（物化门负路径扩展）。
- **U-8 G4 生态 selector 半边**：Package/Skill/Tool/Model/Artifact/Topic/外部服务 typed selector→generation handle 解析未落，其上的「latest 不得当已授权依赖」不可判定。owner：维护者。缓解：现以 input_selectors_digest 摘要绑定。退役证据：生态 selector 解析车道落地 + 负路径测试。
- **U-9 载荷执行面（分域口径 2026-10-25，W65-CAPP-PAYLOAD）**：**CLI 全周期与确定性驱动回执载荷执行半边已证**——[C-APP-PAYLOAD-001](../evidence/stage-c/c-app-payload-001-sample-cli-execution.md) 已跑通 `nlos-package` 全命令与 `examples/sample-app` 跨代际 `app.bin` 载荷回执闭环；**残域保留**：客户机不可信任意 Wasm/ELF 字节码虚拟机沙箱直跑仍按 ADR-0015 分层推进。owner：维护者。
- **U-10 真实 provider 行为（分域口径 2026-10-25，W65-CPROVIDER-REAL）**：**独立子 OS 进程 Provider Worker、跨平台 ADR-0011 认证 IPC、ADR-0024 冻结通道与隔离姿态门控半边已证**——见 [C-PROVIDER-001](../evidence/stage-c/c-provider-001-real-process-provider-codec.md)；**残域保留**：外部商业云 LLM HTTP 供应商尾延迟谱系与硬件 TEE 远程证明根证书在线核验属生产适配层边界。owner：维护者。
- **U-11 矩阵与探针缺口**：100K@50% working-set cell、occupancy 回收再入场动态、调度器自身规模探针、PINNED tier、apply 侧 TaskNode admission consult、100K 级 cancel 探针。owner：维护者。缓解：逐项登记（W31-G §8.2 / W34-A §6）；不据此宣称门正式达成之外的能力。退役证据：各缺口补跑/落地后追加 evidence 节。
- **U-12 跨进程并发 GC tick**：单写者前提下键不相交为构造性论证，跨进程并发 tick 未证明。owner：维护者。缓解：B-ARTIFACT-004 §8.7 登记。退役证据：并发 tick 测试或显式单写者契约文档化。
- **U-13 当前 CI run 结论**：HEAD `a01fd57` run `35531478487`（+ Pages `35531478300`）为 B-001/B-003/B-004/G6 边界/X-5 共同收口载体，W34-A 评审时 in_progress——最终 conclusion 未知。owner：维护者。缓解：workspace 全仓门已本地收口（275 二进制/1536 passed/0 failed/20 ignored）。退役证据：run 绿 + 链接回填 §6 对应行。
- **U-14 TS/Python conformance 覆盖漂移（已退役 closed）**：已由 `5c6f4c7`（[B-SCHEMA-002 §6](../evidence/stage-b/b-schema-002-cross-language-generation.md)）及 `W65` [ADR-0024](./adrs/0024-driver-provider-payload-codec-freeze.md) 三语言黄金向量同步闭合。

## 4. 已决定递延项（非未知：登记在案、有明确处置路径，W34-D 裁量豁免/移交阶段 C）

载荷执行面（U-9 同体）；`nlos package install` CLI；迁移面 kill-9 矩阵/abandon 命令/跨进程并发注入；patch-downgrade 拒绝；跨进程 uninstall 审批；capability revoke；uninstall 不解除 artifact 引用；生命周期 NL 动词（uninstall/disable application）；supervisor 自动 pid 发现/unregister；`restore_process` 复活链与干净退出终态路径；teardown 并发竞争面；pause/resume/cancel 宿主执行器；throttle durable 落账与 reclaim 真实计数；semantic 域 NL 语法；desktop 测试并入根 CI；Windows GUI 认证面；W32-F 四呈现边界（载荷渲染/生命周期状态机/呈现走本地直读/库 API 入口）；AgentInstance 独立分面；driver payload codec 入冻结通道；X-1/X-2 最小口径之外的能力面。

（出处：W33-H §3.2/§4、W31-G §8.2、W34-A 各门 residual 段；移交登记由 W34-D 在 §6.5.5 条件 5 下完成。）

## 5. 维护

- 本清单与 risks.yaml / claims.yaml 同波更新（W34-B/C 交付）；风险状态变化（尤其 RISK-B-12 重定级）必须同步两处。
- lint（`python3 scripts/lint_claims.py`）覆盖机器台账 `v1.1` 一致性（含 `stage: B/C` 与 `docs/evidence/<stage>/` 双向索引检查）；本清单为管理文档，位于 `docs/management/`，不入 `evidence-index.yaml`。
