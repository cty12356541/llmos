# C-SHARD 工作包计划

> **状态：已采纳（附条件），2026-10-08。** 维护者四点裁断原文要点：「接受两阶段与分域退役原则，先修订 ADR 消除循环并明确验证范围；保留所有未证残域。接受保守命名映射及离线 Wave 1 节奏。先完成文档登记，仍不派发实施，实施按既定授权边界另行确认。」裁定逐条登记于 §0；配套 [ADR-0019 修订 2](./adrs/0019-cross-cell-commit-semantics-extension.md)（Phase 0 证据车道豁免 + VERIFIED 范围界定）与本文件同步落档。
> 性质：阶段 C 工作包 `C-SHARD` 的执行计划（[stage-c-progress §C.3.1](./stage-c-progress.md) 行入口）。Owner：integrator（本计划采纳会话）；Phase 0/Phase 1 各车道派发时另立 Task/Attempt。
> 基线：main = `f443e3c`（2026-10-06）；2026-10-08 登记时 fetch 复核 origin/main = 本地 = `f443e3c`，**零漂移**。
> 起草信息前置（全部已核验原文）：ADR-0019 修订 1 `ACCEPTED`（`f443e3c`）；机制面三项在 main（`federation.rs` + `CellHost` 接线 + 双进程证据测试 ×2）；v27–v38 迁移基线、§26.1 三 Record、`DIST-TASK-001..004` 全文、RISK-B-11/U-5 退役定义、H6 门类目。

---

## 0. 呈批决策点与裁定登记（2026-10-08 附条件通过）

### CS-0（核心）：VERIFIED 派发门死锁的解锁 —— **裁定：接受两阶段，但先最小修订 ADR（已落地）**

- 原死锁：ADR-0019 决定 2「`C-SHARD` 不得派发，直到本 ADR `VERIFIED`」vs R1.3「`VERIFIED` 留给实现证据」——循环前置；且「验证与证据」节的显式时间限制（「一律在本 ADR `VERIFIED` **之后**由实现车道落档」）无法靠车道命名消除。
- **裁定后执行路径**：[ADR-0019 修订 2 R2.1](./adrs/0019-cross-cell-commit-semantics-extension.md) 已显式替代该时间限制句——允许**有界 Phase 0 证据车道**（登记名「ADR-0019 VERIFIED 证据面」）在 `VERIFIED` 前产出证据；豁免仅此，`C-SHARD` 本体派发门（决定 2）与触发器 1（独立审查 + 定向门）保留。
- Phase 0 不含任何 C-SHARD 本体切片；[stage-c-progress §C.3.1](./stage-c-progress.md) `C-SHARD` 行维持 `NOT_STARTED` 至 `VERIFIED` 门开。

### CS-1：RISK-B-11 / U-5 分域退役口径 —— **裁定：接受分域原则，partial ≠ 自动满足 VERIFIED（计划勘误一处，已修）**

- `VERIFIED` 的拓扑 / 协议覆盖 / 注入类目 / 验收条件已由 ADR 修订 2 R2.2 界定（拓扑域 = ADR-0018 单机双进程；协议覆盖 = R1.2.2/2.3/2.4；验收 = 四条件整体）。
- **勘误（原草案错误，裁定指出）**：原草案把「旧 Cell 效果拒绝 / QUARANTINE」当作「远端物理 cleanup 证明」——两者不等同。拒绝/QUARANTINE 只证明**拒绝语义**；**物理 cleanup 残域保留**至存在实际清理证据（ADR 修订 2 R2.3 第 2 条）。
- Phase 0 证据覆盖的 participant endpoint **逐 endpoint 声明**；未覆盖者不得宣称闭环（R2.3 第 3 条）。跨机残域挂 R1.4 触发器 7（R2.3 第 4 条）。

### CS-2：三 Record 命名/映射策略 —— **裁定：接受保守映射，终裁留 CS1-A（CS1-A 已终裁并入册 §4.1 映射表）**

保留现有 `Authority*` / `ParticipantRegistry*` 类型与 SQLite 表名，并提供 §26.1 规范名类型别名（`TaskAuthorityAssignment`、`AuthorityAssignmentStatus`、`TaskAuthorityParticipantRegistry`、`ParticipantRegistrySnapshot`、`TaskAuthorityTakeoverReceipt`）；**映射表义务**：已于 §4.1 逐字段、逐状态、逐不变量列出对应关系**与语义缺口**——名称对齐不代替语义实现（ADR 修订 2 R2.4）。

### 验收修正（裁定追加，适用于 CS0-D）—— **分区双主必须双存活注入**

kill child 只证明**崩溃处理**；分区与分区双主类目必须在**两进程保持存活**的条件下阻断通信 / 续约来注入（ADR 修订 2 R2.2 第 3 条方法学约束）。

### 节奏 —— **裁定：原则上允许离线先做 Wave 1，本地原子提交**

恢复后先 fetch、审查漂移、复验再 push；缺少远端验证时不宣称屏障闭合。**网络状态更新**：2026-10-08 登记时 fetch 已恢复并复核零漂移（维护者侧与会话侧双重核实）；开工前正常 fetch 复核即可，离线纪律保留为后备。

---

## 1. 代码级现状核验结论（教训六义务；勘察基线 `f443e3c`）

| 事实 | 锚点 | 对计划的含义 |
|---|---|---|
| v27–v38 接管脊椎完整：lease→fence receipt→assignment→pending receipt→barrier observation→fence manifest→signer 列→completion→cross-term adoption | `nlos-task/src/migrations.rs`（v27 `:800`…v38 `:1270`）、`lease.rs`、`participant.rs:65` | C-SHARD = 对齐 §26.1 语义 + 补跨 Cell 编排，**不是从零造表** |
| 本地接管执行 = 三个单事务门：`prepare_authority_takeover_fence`（store.rs:2101，一事务完成 freeze+receipt+exact roots+manifest+assignment→TakeoverPending）、`record_authority_takeover_barrier_receipt(_signed)`（:2324/:2409）、`complete_authority_takeover`（:2548，全员签名覆盖+successor lease 活跃） | `nlos-task/src/store.rs` | 跨 Cell 驱动器的**本地半边已在**，缺的是跨进程协调循环 |
| 跨进程签名 barrier IPC 已有：`nlos-takeover-control`（test infrastructure, 非 production daemon；无人依赖，独立 crate） | `crates/nlos-takeover-control/` | Phase 0 直接复用，不新造 IPC；升格/替换留 CS1-D 裁定 |
| reconciliation executor 真空位，接口边界被文档钉死：checkpoint digest 是 ≤256B 自由文本（`federation.rs:68/:811`）、`MigrationObject` 是 opaque 16B（`:118`）、验证/执行显式 deferred（`:38-40`） | `nlos-cell/src/federation.rs` | executor 须自建 digest 语义与 object↔DIST-TASK 绑定（CS1-D） |
| 唯一 epoch 推进入口 = quota 族 `advance_epoch_and_quarantine`（`nlos-lease/src/lib.rs:559`），`cell_host.rs:38-44` 文档+测试 pin | `nlos-slice-k/src/cell_host.rs` | 驱动器走 TaskAuthority lease/term/fencing 机器，**不得**新增第二 epoch 入口 |
| SCHEMA_VERSION = 46（v39–v46 已被其他车道占用） | `nlos-task/src/store.rs:75` | C-SHARD 迁移从 **v47+** 起排，不占已用号 |
| slice-k 已依赖 nlos-task（Cargo.toml:36）+ nlos-cell + nlos-lease | `crates/nlos-slice-k/Cargo.toml` | 驱动器落 slice-k 装配层**零新增依赖边** |
| H6 注入四件套可复用：dual-cell `#[ignore]` child 惯例（`dual_cell_federation.rs:112` 等）、`nlos-store-fault` VFS（FailWritesAfter/PowerLossAfter）、kill-9+READY 管道+`FAULT_LOCK` 矩阵惯例（`takeover_fault_injection.rs` 为 v27+ 表组族先例）、`epoch_fencing.rs` stale 拒绝断言 | 各 tests | H6 证据面是**净新增测试面**，但工具全有先例 |
| `docs/evidence/stage-c/` 不存在；`lint_claims.py` 已支持 v2 任意 stage 目录（`scripts/lint_claims.py:28`） | evidence 侧 | 首个 Evidence 由 CS0-E 落档时创建目录 + index 收录 |
| DIST-TASK-001..004 代码零存在（唯一命中 federation.rs:115 一句 doc） | 全仓 | 全语义从规范文本落地，无半成品可续 |

---

## 2. 结构总览

```text
Phase 0（证据车道族，登记名「ADR-0019 VERIFIED 证据面」，C-SHARD 行不动；豁免依据 = ADR 修订 2 R2.1）
  Wave P0-1 ── CS0-A 跨 Cell 接管驱动器 + 双进程 PoC（成功路径）   ┐ 并行，写集不相交
              CS0-B checkpoint digest 钉面与验证器（nlos-cell）     ┘
  [屏障]
  Wave P0-2 ── CS0-C reconciliation 收敛核（最小 executor）→ CS0-D H6 故障注入矩阵
              （同域 slice-k + 真依赖，串行）
  [屏障]
  Wave P0-3 ── CS0-E Evidence 落档 + ADR-0019 VERIFIED 晋升 + RISK-B-11/U-5 分域退役
              （integrator 单车道，共享 canonical 全在此合并）
  ═══ VERIFIED 门开（验收条件 = ADR 修订 2 R2.2 第 4 条）═══
Phase 1（C-SHARD 本体，§C.3.1 行翻 IN_PROGRESS）
  CS1-A 三 Record 迁移族（schema v47+，含 CS-2 命名终裁门 + 映射表义务）
  CS1-B DIST-TASK-001 admission/permit-freeze 全语义
  CS1-C DIST-TASK-002/004 对齐缺口（fence 集并集、六类 endpoint barrier 映射、closure→新 OPEN registry）
  CS1-D 驱动面生产化 + MigrationObject↔DIST-TASK record 绑定（federation.rs:115 遗留义务）
  CS1-E 台账/claims/L0 收尾 + C-MIGRATE 解锁评估（依赖链 §C.5.2：C-SHARD+C-LEASE→C-MIGRATE）
```

Phase 1 各车道派发时**必须重做代码级核验**（教训六；本计划只冻结框架与验收门，不冻结车道内设计）。Phase 1 依赖图初判：A→B→C 串行（同写集 nlos-task 表族 + 真依赖递进）；D 依赖 A（Record 名落定）可与 B/C 并行（写集 slice-k/nlos-cell 侧）；E 屏障后单车道。

---

## 3. Phase 0 车道明细

所有 Phase 0 证据测试的边界声明纪律：**单机双进程**（ADR-0018 拓扑；共享内核/时钟/文件系统），非跨机、非网络分区、非跨时钟域——引证为跨机证据即违 RISK-B-11（沿 W50-L2/W51 测试边界声明原文惯例）。

### Wave P0-1

| 车道 | 事项 | 写集 | 依赖 | 验收门 |
|---|---|---|---|---|
| **CS0-A** 接管驱动器 + PoC | slice-k 新模块（暂名 `takeover_drive.rs`）：在 ADR-0018 双进程拓扑上驱动既有本地门走完 DIST-TASK-002 CAS 链——旧 Cell 进程持 authority（TaskAuthority lease 机器，**不碰** cell epoch 推进入口），新 Cell 进程作为 successor 经 `nlos-takeover-control` IPC 提交签名 barrier observation，驱动器编排 prepare→逐 endpoint barrier→complete→新 assignment 激活 + DIST-TASK-004 baseline 初始化与 generation 递增。双进程证据测试（新文件 `tests/dual_cell_takeover.rs`，沿 `dual_cell_federation.rs` 惯例：`#[ignore]` child + env/临时文件信号 + 单父 `#[test]`） | `crates/nlos-slice-k/src/`（新文件）、`crates/nlos-slice-k/tests/dual_cell_takeover.rs` | 无 | 成功路径端到端：两 OS 进程完成一次跨 Cell 接管，全部 durable 表组（v27–v38 族）落痕可断言；旧 Cell fence 后拒绝新提交有断言；**不声称跨机**；fmt/clippy `--all-targets` 退出码直检绿；slice-k 全测试绿 |
| **CS0-B** checkpoint digest 钉面 | nlos-cell：为 `CheckpointFact` 的自由摘要轴定义**结构化 digest 语义**（确定性序列化 + SHA-256，域分隔字符串如 `llmos/reconciliation-checkpoint/v1`）与**验证器 API**（对声称的 durable 前缀重算并比对）——不改存储格式（仍是 ≤256B 单行），只终结「登记不验证」的语义真空中 Phase 0 需要的那半边 | `crates/nlos-cell/src/federation.rs`（additive）、`crates/nlos-cell/tests/federation.rs` | 无 | digest 确定性（同前缀同摘要、异前缀异摘要）、损坏/篡改 fail-closed 有测试；既有 9 个 federation 测试不回归 |

### Wave P0-2（屏障后）

| 车道 | 事项 | 写集 | 依赖 | 验收门 |
|---|---|---|---|---|
| **CS0-C** reconciliation 收敛核 | 最小 executor（slice-k，CS0-A 模块邻接）：消费 per-Cell checkpoint trail（CS0-B 验证器核对摘要）+ 各 authority durable prefix，把「接管完成点与旧权威最后 checkpoint 之间的提交前缀缺口」收敛到唯一终态。契约不变量**照搬** ADR-0013 决定 1 的跨 Cell 推广（R1.2.4）：崩溃窗口收敛性、无幻影行、无双重提交、replay 逐字节幂等 | `crates/nlos-slice-k/src/`（CS0-A 邻接）、对应测试 | CS0-A、CS0-B | 收敛场景测试族（缺口覆盖、幂等重放、双 prefix 一致终态）；不变量逐条有断言；不新增 epoch 入口 |
| **CS0-D** H6 故障注入矩阵 | 对 CS0-A PoC 注入六类目（ADR 修订 2 R2.2 第 3 条子集）：**分区**（**双进程存活**下阻断通信/续约——kill 不算分区证据）、**重启**（Cell 进程 mid-chain 崩溃重启，从 durable prefix 续走；kill 型注入归此类目）、**迁移**（authority 接管本身）、**重复消息**（barrier observation / receipt 重放幂等）、**stale epoch**（fence 后旧 Cell 以旧 token 提交被拒）、**分区双主**（**双进程存活**的分区窗口内双活声称 → lease/fencing 仲裁单胜者，anchored fail-closed 语义）。复用 `nlos-store-fault`、`FAULT_LOCK`、kill-9+READY 管道惯例（仅限崩溃/重启类目）。注：fanout storm / provider stall / bulkhead 三类目属 C-FANOUT/C-BENCH，不在本计划 | 新测试文件族（`crates/nlos-slice-k/tests/dual_cell_takeover_fault_*.rs`）+ 最小注入钩子（如驱动器测试门） | CS0-C（重启/双主场景断言收敛） | 六类目逐条至少一个具名证据测试，断言语义来自 DIST-TASK-002 尾句与 R1.2.4（不抢先接管、QUARANTINE、拒绝旧 Cell 六类动作：result/message/permit/callback/effect/commit）；分区与双主类目的测试代码可证明两进程全程存活；全仓测试绿 |

### Wave P0-3（integrator 单车道）

| 车道 | 事项 | 写集 | 依赖 | 验收门 |
|---|---|---|---|---|
| **CS0-E** Evidence + VERIFIED 晋升 + 风险分域退役 | ① 创建 `docs/evidence/stage-c/` 首批 Evidence 文件（H6 六类目逐条 + 成功路径 + 收敛核，`EVID-SCOPE-001` 单 claim 单证据范围，负面/边界显式持久化，**endpoint 覆盖逐个声明**）+ `evidence-index.yaml` 收录 + `lint_claims.py` 绿；② ADR-0019 `ACCEPTED→VERIFIED` 晋升包：**独立审查门**（另派审查代理，非本车道自审；沿 ADR-0019 两段式先例）+ 定向门（Phase 0 测试套件复跑全绿）+ 状态行记录裁断与授权链——验收条件 = ADR 修订 2 R2.2 第 4 条**整体**（partial 退役登记不自动满足）；③ RISK-B-11 `open→partial`（risks.yaml，已证域=单机双进程域；**物理 cleanup 残域 + 跨机残域显式保留**，复审点挂 R1.4 触发器 7）+ U-5 分域口径（exit-unknown-risks.md）；④ 台账 `stage-c-progress.md` 登记（单一时间线）+ L0（management/README ADR 状态行）同步 | docs（evidence/、evidence-index.yaml、risks.yaml、exit-unknown-risks.md、adrs/0019 状态行、stage-c-progress.md、README） | P0-2 屏障 | R2.2 第 4 条四条件逐项满足留痕；独立审查 pass（或 pass-with-fixes 修复后复验）；lint 绿；**至此 C-SHARD 派发门开**，Phase 1 派发前再向用户确认一次（授权边界原文） |

---

## 4. Phase 1 车道框架（门开后细化，此处只冻结验收门）

| 车道 | 义务 | 规范锚点 | 验收门（框架级） |
|---|---|---|---|
| CS1-A 三 Record 迁移族（**DONE**，schema v47） | schema v47：registry 全 Record 化（`authority` / `signature` / `prior_participant_registry_root` / `participant_registry_root` 字段面）；assignment 状态机对齐五态（`Active = 1` / `TakeoverPending = 2` / `Fenced = 3` / `Expired = 4` / `Quarantined = 5`）；receipt 字段对齐（`fence_barrier_receipts` 子表映射、`outstanding_effect_quarantine_root` 可空、`consensus_commit_index` 被动槽——ADR-0018 决定 5 & ADR-0020：零共识协议语义）；CS-2 终裁 + **§4.1 映射表**（字段/状态/不变量对应与缺口，命名≠语义） | §26.1 三 Record | `migrate_v47` 幂等 + 半迁移 fail-closed；`c_shard_cs1a_schema_v47` 全绿；映射表入册 §4.1 |
| CS1-B admission 全语义（**DONE**） | participant 注册先于 operation admission（`verify_dist_task_operation_admission` / `admit_verified_participant_operation` / `admit_operation_binding_with_authority` 同 authority transaction 原子注册准入，或未激活 `OperationBinding` 在 seal 门以 `OperationDispatchNotPrepared`/`NotActivated` fail-closed）；非 `Active` assignment（`TakeoverPending`/`Fenced`/`Expired`/`Quarantined`）全路径拒绝注册/准入/seal/permit；`CommitPermit` 发放（含 `request_commit_permit_with_authorities_and_lease`）同 CAS 置 `FROZEN_FOR_PERMIT`、存续期禁新增、`EffectPermit` 绑定逐位相同 generation/root；permit 背后禁扩集合 | DIST-TASK-001 | `c_shard_cs1bc_conformance` 全绿；[c-shard-005](../evidence/stage-c/c-shard-005-dist-task-conformance.md) 在册 |
| CS1-C 002/004 缺口对齐（**DONE**） | exact fence set = frozen registry 当前全集 ∪ 旧 control epoch durable outstanding participants（全六域 `TaskWriteSet` 并集解析显式化）；六类 `ParticipantType::CANONICAL_SECTION_26_1`（`TaskStore`/`ArtifactHead`/`SemanticAdmission`/`ChannelTopic`/`DriverGateway`/`ResourceLedger`）barrier 覆盖审计与门控（`inspect_takeover_endpoint_barrier_audit` / `verify_six_endpoint_takeover_barrier_coverage`）；`QuarantineReceipt` 自动联动填入 `TaskAuthorityTakeoverReceipt.outstanding_effect_quarantine_root`；`TaskPermitClosureReceipt` 同事务置旧代 `Superseded` 并推进新 `OPEN` registry generation `generation + 1` 及旋转 `Active` assignment（SQLite 触发器禁 `Superseded` 回退解冻） | DIST-TASK-002/004 | `c_shard_cs1bc_conformance` 全绿；[c-shard-005](../evidence/stage-c/c-shard-005-dist-task-conformance.md) 在册 |
| CS1-D 驱动面生产化（**DONE**） | Phase 0 驱动器从证据面升生产语义（`drive_cross_cell_takeover_with_options` + `ProductionTakeoverOptions`）；`MigrationObject`（opaque 16B）↔ 真实 `DIST-TASK` record 绑定（`DistTaskRecordBinding` / `DistTaskMigrationKind` / `verify_dist_task_binding`，`federation.rs:115` 遗留义务闭合）；checkpoint digest 绑定升级（`CheckpointPrefix::from_dist_task_bindings` + `CheckpointFact::verify_dist_task_binding`）；快照签名核验（`record_verified_participant_registry_signature` / `verify_frozen_registry_signature`）；assignment `Expired`/`Quarantined` 编排与 barrier 失败自动隔离；`nlos-takeover-control` SABI handler 裁定为传输中立 `BarrierObservationSource` 的本地/IPC 参考实现 | R1.2.2/R1.2.5 | `nlos-cell` 35/0 + `dual_cell_takeover` 3/0；[c-shard-006](../evidence/stage-c/c-shard-006-production-takeover-binding.md) 在册 |
| CS1-E 收尾（**DONE**） | 台账 `C-SHARD` 行状态升格（Phase 0 + Phase 1 `CS1-A..E` 全部完成，限定 ADR-0018 单机双进程域；跨机/真网络分区与远端物理 cleanup 残域按 `RISK-B-11` 保留）、`evidence-index.yaml` 收录 `C-SHARD-005..006`、L0 `README.md` 同步、`C-MIGRATE` 依赖解锁确认（`W63-CMIGRATE-PROC` 同波落地 `C-MIGRATE-001`） | §C.5.2 | `lint_claims.py` PASS；`C-SHARD-001..006` 全证据在册 |

### 4.1 CS-2 三 Record 字段 / 状态 / 不变量对应与缺口映射表（CS1-A 终裁入册）

> **边界声明（ADR-0019 修订 2 R2.4）**：保留现有 `nlos-task` SQLite 表名与 `Authority*` / `ParticipantRegistry*` 主结构名，同时导出 §26.1 规范名类型别名；下表逐项区分「已实现并由 SQLite 触发器 / CAS 门强制」与「仍属后续车道（CS1-B..D）或长期残域的语义缺口」，绝不以名称或列存在冒充协议闭环。

#### (A) `TaskAuthorityAssignment` ↔ `AuthorityAssignmentRecord`（表 `task_authority_assignments`，schema v31 创建、v47 扩五态）

| §26.1 规范字段 / 状态 / 不变量 | SQLite 列 / 约束（v47） | Rust 结构 / API 映射 | 当前已证不变量（v47） | 残余缺口与归属车道 |
|---|---|---|---|---|
| `assignment_id` / `task_id` / `task_generation` | `assignment_id BLOB(16) PK`、`task_id BLOB(16) FK`、`task_generation BLOB(8)` | `AuthorityAssignmentRecord::{assignment_id, task_id, task_generation}`（别名 `TaskAuthorityAssignment`） | `derive_assignment_id` 确定性派生；`task_authority_assignment_identity_immutable` 触发器锁死身份列不可变、`no_delete` 禁删 | 无 |
| `authority_id` / `authority_term` / `lease_until` / `fencing_token` | `authority_id BLOB(16)`、`authority_lease_holder_id BLOB(16)`、`authority_lease_term BLOB(8)`、`authority_lease_epoch BLOB(8)`、`authority_lease_fencing_token BLOB(32)`、`authority_lease_expires_at_ms INTEGER` | `authority_lease_binding: AuthorityLeaseBinding` | 同 term 续约可经 `refresh_active_assignment` 更新 epoch/expires_at_ms；跨 term 只能经 takeover completion 旋转新行 | 跨机时钟与网络租约仲裁不在单机双进程域（留 R1.4 触发器 7） |
| `control_epoch` / `participant_registry_generation` / `participant_registry_root` | `control_epoch BLOB(8)`、`participant_registry_generation BLOB(8)`、`participant_registry_root BLOB(32)` | `control_epoch: u64`、`participant_registry_binding: ParticipantRegistryBinding` | 触发器锁死 `participant_registry_generation` 与 `participant_registry_root` 不可变；registry 演进必须旋转新 assignment 行；`TaskPermitClosureReceipt` 同事务自动将旧 `Active` assignment 置 `Fenced` 并旋转到新 `OPEN` generation（CS1-C 已闭合） | 无 |
| `status: ACTIVE \| TAKEOVER_PENDING \| FENCED \| EXPIRED \| QUARANTINED` | `assignment_state INTEGER NOT NULL CHECK(assignment_state IN (1, 2, 3, 4, 5))` | `AuthorityAssignmentState` / `AuthorityAssignmentStatus`：`Active(1)`、`TakeoverPending(2)`、`Fenced(3)`、`Expired(4)`、`Quarantined(5)`；`expire_authority_assignment`、`quarantine_authority_assignment` | 终态 `(3, 4, 5)` 由 SQLite 触发器与 Rust CAS 双重锁死不可逆、不可回退；`TakeoverPending(2) -> Active(1)` 触发器禁止；相同目标终态重放逐字节幂等；`takeover_drive` 已提供跨进程 `expire_authority_assignment` / `quarantine_authority_assignment` 与 `quarantine_on_barrier_failure` 自动隔离编排（CS1-D 已闭合） | 无 |
| `authority` / `signature`（Record 尾签名） | 未单列（由 `authority_id` + `fencing_token` 承载本地权威绑定） | `authority_lease_binding.authority_id` / `fencing_token` | 本地单库事务与 `nlos-identity` 签名 barrier observation 在 takeover 门核验 | assignment 行级独立签名信封在单机双进程域非必需；跨机控制面签名信封留 R1.4 |

#### (B) `TaskAuthorityParticipantRegistry` ↔ `ParticipantRegistryRecord`（表 `task_participant_registries` + `task_participants`，schema v11 创建、v15/v24/v40 扩枚举、v47 补齐字段面）

| §26.1 规范字段 / 状态 / 不变量 | SQLite 列 / 约束（v47） | Rust 结构 / API 映射 | 当前已证不变量（v47） | 残余缺口与归属车道 |
|---|---|---|---|---|
| `registry_id` / `task_id` / `task_generation` / `participant_registry_generation` | `registry_id BLOB(16) PK`、`task_id BLOB(16) FK`、`task_generation BLOB(8)`、`registry_generation BLOB(8)`，`UNIQUE(task_id, registry_generation)` | `ParticipantRegistryRecord::{registry_id, task_id, task_generation, generation}`（别名 `TaskAuthorityParticipantRegistry` / `ParticipantRegistrySnapshot`） | 每代一行、单调递增、触发器锁死身份列与历史行不可删改 | `assignment_id` / `authority_term` / `control_epoch` 通过当前 `task_authority_assignments.participant_registry_binding` 外键/绑定关联，未在 registry 表内冗余存储 |
| `prior_participant_registry_root` / `participant_registry_root` | `prior_registry_root BLOB(32)`（哈希链非空根）+ v47 新增可空 `prior_participant_registry_root BLOB CHECK(IS NULL OR length = 32)`、`participant_registry_root BLOB(32)` | `prior_root: [u8; 32]`、`prior_participant_registry_root: Option<[u8; 32]>`（gen 1 为 `None`，gen > 1 及 successor reopen 为 `Some(prior_root)`）、`root: [u8; 32]` | `validate_registry` 每次读取重算 SHA-256 根并校验 `prior_participant_registry_root` 与 `prior_root` 一致；触发器锁死不可变 | 无 |
| `participants:[...]`（§26.1 六类 + 扩展进程/操作端点） | 子表 `task_participants`：`participant_type BETWEEN 1 AND 8`（1=`TaskStore`, 2=`ArtifactHead`, 3=`SemanticAdmission`, 4=`ChannelTopic`, 5=`DriverGateway`, 6=`ResourceLedger`, 7=`ProcessBinding`, 8=`OperationBinding`） | `participants: Vec<ParticipantRecord>`、`ParticipantType::CANONICAL_SECTION_26_1`、`admit_verified_participant_operation`、`verify_six_endpoint_takeover_barrier_coverage` | 子表触发器完全不可变；注册入口强制先向各 owner authority 读取真实 endpoint proof；`DIST-TASK-001` 同事务准入/未激活 prepare 门控与六类 canonical endpoint 全覆盖 barrier 门控已在 CS1-B / CS1-C 闭合 | 无（远端物理副作用清理仍属 `RISK-B-11` 残域） |
| `state: OPEN \| FROZEN_FOR_PERMIT \| FROZEN_FOR_TAKEOVER \| SUPERSEDED` | `registry_state INTEGER NOT NULL CHECK(registry_state BETWEEN 1 AND 4)` | `ParticipantRegistryState::{Open, FrozenForPermit, FrozenForTakeover, Superseded}`；`reopen_participant_registry_after_permit_closure` | permit 发放冻结为 `FrozenForPermit`；takeover 预门冻结为 `FrozenForTakeover`；`TaskPermitClosureReceipt` 同事务将 `FrozenForPermit` 置 `Superseded(4)`（SQLite 触发器锁死 `OLD.registry_state = 4` 不可回退）并创建 `generation + 1` 的新 `Open(1)` 代（CS1-B / CS1-C 已闭合） | 无 |
| `authority` / `signature` | v47 新增可空 `authority_id BLOB(16)`、`signature BLOB(64)` | `authority: Option<[u8; 16]>`、`signature: Option<[u8; 64]>`；`record_participant_registry_signature`、`record_verified_participant_registry_signature`、`verify_frozen_registry_signature` | `participant_registry_signature_coupled_insert` 与 `task_participant_registry_identity_immutable` 触发器强制两列成对出现（同 NULL 或同 NOT NULL）且写后不可变；`takeover_drive` 已接入 `nlos-identity` Ed25519 快照签名与 successor 验签门控（CS1-D 已闭合） | 无 |

#### (C) `TaskAuthorityTakeoverReceipt` ↔ `AuthorityTakeoverReceiptRecord`（表 `task_authority_takeover_receipts` + 子表 `task_authority_takeover_barrier_receipts` / `task_authority_takeover_fence_members`，schema v32–v37 创建、v47 补齐字段面）

| §26.1 规范字段 / 状态 / 不变量 | SQLite 列 / 约束（v47） | Rust 结构 / API 映射 | 当前已证不变量（v47） | 残余缺口与归属车道 |
|---|---|---|---|---|
| `receipt_id` / `task_id` / `task_generation` / `old_assignment_id` / `new_assignment_id` | `receipt_id BLOB(16) PK`、`task_id BLOB(16)`、`task_generation BLOB(8)`、`old_assignment_id BLOB(16) FK`、`new_assignment_id BLOB(16) NULL` | `AuthorityTakeoverReceiptRecord`（别名 `TaskAuthorityTakeoverReceipt`）对应字段 | `Pending(1)` 时 `new_assignment_id IS NULL`；仅 `complete_authority_takeover` 可经窄化触发器做唯一一次 `Pending(1) -> Complete(2)` 并填入 `new_assignment_id` | 无 |
| `frozen_old_authority_term` / `frozen_old_control_epoch` / `new_authority_term` / `new_control_epoch` | `frozen_old_authority_term`、`frozen_old_control_epoch`、`new_authority_*`（含 holder/term/epoch/fencing_token/expires_at_ms）、`new_control_epoch` | `frozen_old_authority_term`、`frozen_old_control_epoch`、`new_authority_lease_binding`、`new_control_epoch` | 预门原子递增 `control_epoch` 并要求 `new_term > old_term`；触发器锁死不可变 | 无 |
| `frozen_participant_registry_generation` / `frozen_participant_registry_root` / `exact_fence_set_root` | `frozen_registry_generation BLOB(8)`、`frozen_registry_root BLOB(32)`、`exact_fence_set_root BLOB(32) NULL`、`outstanding_operation_participant_root BLOB(32) NULL` | `frozen_registry_binding: ParticipantRegistryBinding`、`exact_fence_set_root: Option<[u8; 32]>`、`outstanding_operation_participant_root: Option<[u8; 32]>` | `load_outstanding_operation_participants` 解析未决 permit 写集全六域 durable participants 并由 `takeover_fence_roots` 计算 `frozen registry ∪ durable outstanding participants` 的确定性摘要落库 `task_authority_takeover_fence_members`（CS1-C 已闭合） | 无 |
| `fence_barrier_receipts:[...]` | 子表 `task_authority_takeover_barrier_receipts`（v33 创建、v35 补 digest、v36 补 5 个 signer 列与成对触发器） | `inspect_authority_takeover_barrier_receipts`、`record_authority_takeover_barrier_receipt_signed`、`inspect_authority_takeover_barrier_coverage`、`verify_six_endpoint_takeover_barrier_coverage` | `complete_authority_takeover` 与 `verify_six_endpoint_takeover_barrier_coverage` 强制校验 manifest 非空、§26.1 六类 canonical endpoint 与并集全成员覆盖、且每条 observation 均带 `nlos-identity` 验证通过的签名（CS1-C 已闭合） | 远端物理副作用 cleanup 残域归 **RISK-B-11 残域** |
| `outstanding_effect_quarantine_root` | v47 新增可空 `outstanding_effect_quarantine_root BLOB CHECK(IS NULL OR length = 32)` | `outstanding_effect_quarantine_root: Option<[u8; 32]>`；`prepare_authority_takeover_fence_with_receipt_extensions` / `record_authority_takeover_receipt_extensions` / `ProductionTakeoverOptions` | `persist_takeover_pending_receipt` 在存在 `Quarantined` permit 时自动从 `task_quarantine_receipts` 派生并填入 `outstanding_effect_quarantine_root`，冲突值 fail-closed（CS1-C 已闭合）；跨进程驱动器 `ProductionTakeoverOptions` 透传与核验已在 CS1-D 闭合；`Pending` 期写一次即锁死，`Complete` 后完全不可变 | 无 |
| `consensus_commit_index` | v47 新增可空 `consensus_commit_index INTEGER CHECK(IS NULL OR >= 0)` | `consensus_commit_index: Option<u64>` | 写一次即锁死，支持落库与读取；**显式声明（ADR-0018 决定 5 & ADR-0020）：仅作被动元数据槽，零 Raft/多节点共识协议语义** | 共识协议基底属 ADR-0020 `CANDIDATE`（未激活，非本计划目标） |

---

## 5. 派发节奏与纪律

1. **并发上限**：单波在飞车道 ≤3（§C.5.4 决策点 2）；子代理并发 ≤2（基础设施 ENOBUFS/EPIPE 教训）；每车道独立 Task/Attempt/写集/验收条件（PD-ORCH-001..006）。
2. **brief 注入六条集成缝教训**（交接 §3 原文）：新枚举变体查下游穷尽 match；schema 升级查只读版本白名单/schema 钉子；feature 门控按矩阵验证；文档注释改动过 `clippy --all-targets`；验证管道后显式查退出码；设计裁断前代码级核验。
3. **共享 canonical 串行**：`stage-c-progress.md`、`evidence-index.yaml`、`risks.yaml`、README ADR 列表、ADR-0019 文件本身——全部经 CS0-E/CS1-E 单一 integrator 合并，车道间禁止并行写。
4. **网络纪律**（裁定）：原则上允许离线先做 Wave 1、本地原子提交（PD-COMMIT-001..005 全套）；恢复后先 fetch、审查漂移、复验再 push；**缺少远端验证时不宣称屏障闭合**。当前状态：2026-10-08 登记时网络已恢复且复核零漂移；每次派发前正常 fetch 复核。
5. **波号**：内部用 P0-x/P1-x；台账登记时取下一顺位 W 号并避开并行流已用号段（W61+，登记前核对）。
6. **Phase 1 派发前**：再次向用户确认（2026-10-08 授权只覆盖「立项起草 + 文档登记」；VERIFIED 门开 ≠ 授权自动延续）。

---

## 6. 风险与开放问题

| # | 风险/问题 | 处置 |
|---|---|---|
| R1 | origin/main 漂移 | **2026-10-08 登记时已复核零漂移**（fetch 成功，origin=本地=`f443e3c`）；此后每次派发前正常 fetch 复核，漂移则本计划按 CAS 重读修订 |
| R2 | `nlos-takeover-control` 文档自称 test infrastructure——驱动器复用它是否构成「生产依赖测试件」 | CS0-A 设计段裁定：PoC 期复用（证据面本就是测试），CS1-D 生产化时决定升格或替换，计划不预冻结 |
| R3 | dual-cell 测试吞吐惯例（单父 `#[test]` 独占进程级 Cell）约束 P0-2 矩阵时长 | 矩阵按类目分文件（各自独占），可两文件并行跑；CI 时长在 CS0-E 登记 |
| R4 | store 本地门 API 若非公开（勘察只证存在，未证 pub 面） | CS0-A 首步核验 pub 性；需暴露则属 nlos-task 写集扩展，显式进 lane brief |
| R5 | Phase 1 与并行流在 nlos-task 表族的写集竞争 | Phase 1 派发时重核验 + 写集锁纪律（§C.4 (4)-(2)） |
| R6 | 「迁移」类目语义口径：C-SHARD 域 = authority 接管迁移；Process 迁移属 C-MIGRATE（DIST-MIGRATE-001/002） | 已在 CS0-D 事项句中显式限定，防范围蠕变 |
| R7 | 分区/双主注入的「双存活 + 阻断通信」实现面：dual-cell 惯例目前用 env/临时文件信号，无进程间通信通道可掐 | CS0-D 设计段先落注入通道（如 child 侧拒答/挂起开关或 IPC 关断），并证明两进程全程存活；这是裁定的方法学硬约束 |

---

## 7. 非目标（显式排除）

- DIST-TASK-003 层级 fanout（C-FANOUT 主写）；fanout storm/provider stall/bulkhead H6 类目（C-BENCH）。
- Process 迁移与 migration intent 的**执行**（C-MIGRATE；federation.rs:9 显式归属）。
- 跨机/跨时钟域任何声明（ADR-0018 决定 2 + R1.4 触发器 7；留跨机证据再评估门）。
- 共识基底引入（ADR-0020 `CANDIDATE` 不解锁；`consensus_commit_index` 保持无协议语义）。
- 远端物理 cleanup 声明（残域保留至清理证据存在；ADR 修订 2 R2.3 第 2 条）。
- 全球联邦与跨组织清算（§29.2 `NLOS-DEFER-001`）。
- 议题 10 L1/L2、capability 组织信封模板等交接 §6 显式后置项。

---

## 8. 登记与 CRUD 义务

**2026-10-08 采纳登记已完成**（单一 integrator，同一原子提交）：① ADR-0019 修订 2（R2.1 豁免替代 + R2.2 范围界定 + R2.3 退役口径）；② 本计划入册 `docs/management/c-shard-plan.md`（本文件）；③ [stage-c-progress](./stage-c-progress.md) §C.3.1 C-SHARD 行更新 + §C.8 单一时间线登记（含授权链）；④ L0 [README](./README.md) §11.5 ADR-0019 行 + 本计划入口。

**后续义务**（各波执行）：Phase 0 各车道完成时台账增量登记；CS0-E 履行其车道写集内全部登记；VERIFIED 晋升走 ADR-0019 触发器 1（独立审查 + 定向门），届时另提交翻状态行；Phase 1 派发前用户确认。

---

## 9. 验收门汇总

- **Phase 0 门** = ADR-0019 `VERIFIED`：验收条件 = [ADR 修订 2 R2.2 第 4 条](./adrs/0019-cross-cell-commit-semantics-extension.md)整体（独立审查 + 定向复跑全绿 + Evidence/lint 绿 + 分域退役登记且未证残域全部在册）。partial 退役登记**不自动满足**。
- **Phase 1 门** = DIST-TASK-001/002/004 逐句 conformance 绿 + 三 Record 迁移（v47+）完成且幂等可回滚 + 驱动面生产化 + Claim ≤ Evidence lint 绿 + C-SHARD 行终态如实登记（DONE 或余量显式）。
- 全程：任何跨 Cell 能力声明在 VERIFIED 前不得出现；任何跨机声明与物理 cleanup 声明不出本计划（残域口径为准）。
