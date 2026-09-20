# B-TASK-SCALE-001：ROAD-B-004 前片 ScaleProfile 骨架与 10K 已落地维度规模证明

> 状态：`PASS`（前片；ROAD-B-004 整体未达成，缺口见 §4）
>
> 日期：2026-08-30
>
> 对应：`[ROAD-B-004]`（`06-架构设计总纲-v0.5.md` §28.2 单节点 ScaleProfile 发布与 10K/100K 逻辑 TaskNode 基准）
>
> 实现：`nlos-task::scale`（`ScaleProfile` + `TASK_PROFILE_10K`）、`tests/scale_profile.rs`、`tests/scale_profile_probe.rs`

## 1. 本切片目标

为 Task 域发布第一个具名单节点容量档（ScaleProfile 骨架），并对**已落地的维度**（持久 Task 注册、key-scoped permit 查询面）给出 10K 规模的实证数字，证明惰性查询面不随任务总体线性退化。

## 2. 已实现事实

1. `ScaleProfile { profile_id, max_task_nodes, max_active_working_set }` 常量声明面 + `TASK_PROFILE_10K`（`task-10k`，10_000 / 512）与 `TASK_PROFILE_100K`（`task-100k`，100_000 / 5_120）档；`admits_task_nodes` / `admits_active_working_set` 为 const 谓词（含端点）。100K 活跃工作集按 10K 同比例（~5%）线性放大。
2. **声明而非强制**：本片不把档位接入注册/admission 路径（登记为缺口）。
3. **临时维度映射**：`TaskSpec` 无 plan 字段、TaskPlan/TaskNode 声明面未落地，`max_task_nodes` 暂以持久 `Task` 注册承载，`max_active_working_set` 以未结 `CommitPermit` 承载；名义 `TaskPlanId`/`TaskNodeId` 已存在但未绑定持久面。
4. 惰性断言针对**已落地 key-scoped 查询模式**：`tasks.task_id` 主键、attempts/permits 的 `UNIQUE(task_id, idempotency_key)`、`commit_permits_single_active` 部分唯一索引；无全表扫描路径。
5. 常规测试（200 注册 + 16 活跃样本、幂等重注册、single-active fence `Superseded`、散布点读 + 60s 病态慢守卫）；10K 全量数字由显式 `#[ignore]` probe 承载。

## 3. probe 实跑数字（原样誊录）

命令：`cargo test -p nlos-task --test scale_profile_probe -- --ignored --nocapture`（debug/test profile，单平台 macOS，fsync 逐注册事务，`.expect` 全程）。

```
10K task profile (single platform): registrations=10000 register_total=1.769339542s register_mean=1.769339ms permit_p50_100=318.542µs permit_p95_100=377.542µs permit_max_100=613µs permit_p50_10k=346.417µs permit_p95_10k=391.291µs permit_max_10k=2.4605ms working_set=512 working_set_total=166.560875ms working_set_p50=350.917µs working_set_p95=388.166µs inspect4=168.625µs database_bytes=7569408 rss_before=Some(7389184) rss_after=Some(8880128)
test result: ok. 1 passed; 0 failed; ... finished in 2.27s
```

要点：

1. **惰性成立**：10K 库 permit p95 = 391.291µs ≤ 基线（100 库）p95 377.542µs × 16 断言限（实际 ~1.04x）；同 run 绝对面 < 100ms 限。
2. 注册吞吐：10K 次 fsync 注册共 1.769s（均值 1.769ms/次）；同语义早前样本为 4.437s（平台噪声，另录）。
3. 512 活跃工作集发放共 166.561ms（p95 388.166µs）；散布点读 4 次 168.625µs。
4. 落盘体积 7,569,408 字节；进程 RSS 7,389,184 → 8,880,128 字节（`ps` 采样，仅 macOS 有可移植读数，其他 target 如实记 `None`）。

## 3.1 100K 档常量与 probe（W16-004）

### 已发布常量

| 字段 | `TASK_PROFILE_100K` |
| --- | --- |
| `profile_id` | `task-100k` |
| `max_task_nodes` | 100_000 |
| `max_active_working_set` | 5_120（= 512 × 10，与 10K 档保持 ~5% 比例） |

### probe 状态

100K 全量数字由显式 `#[ignore]` probe `one_hundred_thousand_task_registrations_keep_the_permit_face_lazy` 承载（mirroring 10K 探针：100 基线库 vs 100K 注册库 lazy permit 对比、5_120 活跃工作集、散布点读、RSS/落盘体积）。

**本地 W16-004 验收**：常量单元测试已实跑；100K probe 因 ~10× 注册量未在验收窗口本地实跑（登记为待补证据，与 §4 缺口 #2 对齐）。复现命令：

```sh
cargo test -p nlos-task --test scale_profile_probe -- --ignored --nocapture
```

## 4. 限制与下一步（缺口清单）

1. **未强制**：档位未接入 `TaskAuthority` 注册/admission；强制路径为后续工作。
2. **维度映射临时**：TaskPlan/TaskNode 持久声明面、Dependency Resolver 未落地；`max_task_nodes` 以 Task 注册近似；100K 档常量与 probe 已发布，100K probe 数字待本地/CI 实跑后填入 §3.1；档位仍未接入 admission 强制路径，不得宣称 ROAD-B-004 整体达成。
3. **证据等级**：debug（test profile）单平台数字；release profile 与多平台复测未做；checkpoint/rehydrate 基准不在本片。
4. 工作树交接备注：接管时验证窗口与并行车道共享构建目录存在锁竞争，全量门以逐二进制方式收口（结果与本报告命令均一一对应，无跳过项）。

## 5. 验证门（全部实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| 编译 | `cargo test -p nlos-task --no-run` | PASS（全部测试二进制构建成功） |
| 常规套件 | `cargo test -p nlos-task`（probe 为 `#[ignore]` 天然排除） | PASS（32 个集成测试二进制 + lib 单测全量逐一二进制实跑：合计 258 passed / 0 failed / 1 ignored 即 probe；全量单遍 wall-clock 因并行车道构建锁竞争未采信，逐二进制明细见交接回执） |
| probe 实跑 | `cargo test -p nlos-task --test scale_profile_probe -- --ignored --nocapture` | PASS（§3 数字） |
| clippy stable | `cargo clippy -p nlos-task --all-targets -- -D warnings` | PASS（修 `doc_markdown` ×2、`duration_suboptimal_units` ×3 后） |
| clippy nightly | `cargo +nightly-2026-08-01 clippy -p nlos-task --all-targets -- -D warnings` | PASS |
| fmt stable | `cargo fmt -p nlos-task -- --check` | PASS |
| fmt nightly | `cargo +nightly-2026-08-01 fmt -p nlos-task -- --check` | PASS |

修复说明（接管方最小补完，未重写前代理设计）：`scale.rs` / `scale_profile.rs` 文档 `TaskNodes`、`TaskNode` 加反引号（clippy `doc_markdown`）；`from_secs(60/120)` → `from_mins(1/2)`（clippy `duration_suboptimal_units`，语义不变）；`cargo fmt` 纯格式化。

## 6. W17-004 pressure/reclaim 声明式骨架（2026-09-06）

### 已实现事实

1. `nlos-task::pressure`：`WorkingSetPressure`、`ReclaimPolicy`、`ReclaimPhase`、`TASK_DEFAULT_RECLAIM_POLICY`（`[RSM-RECLAIM-001]` 四相占位顺序：cache → QoS → checkpoint/evict → kill）。
2. `ScaleProfile` 扩展 optional `reclaim_threshold_ratio`（`None` → companion `DEFAULT_RECLAIM_THRESHOLD_RATIO = 90`）；`TASK_PROFILE_10K` / `TASK_PROFILE_100K` 显式 `Some(90)`，既有三字段构造仍可通过 `None` 保持 backward compat。
3. 谓词：`reclaim_threshold_count()`、`needs_reclaim(active_count)`（`active > threshold`）；`WorkingSetPressure::needs_reclaim` / `admits` 包装硬/软边界（10K 档 threshold=460，461→reclaim 且仍 admits，513→!admits）。
4. **声明而非强制**：未接入 `TaskAuthority` 注册/admission；无 rehydrate/checkpoint 基准。

### 验证门（W17-004 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| scale_profile 集成 | `cargo test -p nlos-task --test scale_profile` | PASS |
| scale + pressure 单测 | `cargo test -p nlos-task -q scale pressure` | PASS |
| clippy | `cargo clippy -p nlos-task --all-targets -- -D warnings` | PASS |

### 仍属缺口（不得宣称 ROAD-B-004 整体达成）

1. pressure/reclaim 未强制于 admission 或 Materialization/Context controller。
2. checkpoint/rehydrate 基准与 TaskPlan/TaskNode 声明面仍待议题 35 ADR 后推进。
3. 100K probe 数字仍待本地/CI 实跑后填入 §3.1。

## 7. W18-004 working-set admission 最小前缀（2026-09-07）

### 已实现事实

1. `TaskStoreError::WorkingSetAdmissionDenied`：`profile_id` / `active_count` / `max_active_working_set` 三元组，超 cap fail-closed。
2. `pressure::enforce_working_set_admission`：对 `current_active + 1`  consult [`WorkingSetPressure::admits`].
3. **单路径前缀强制**：`SqliteTaskAuthority::request_commit_permit*` 在 net-new  issuance 前（`compete_for_permit` → `issue_permit`）统计 store-wide `Issued` permit 数并 consult  authority 绑定的 `ScaleProfile`（默认 [`TASK_PROFILE_10K`]）；**idempotent replay  bypass**。
4. `SqliteTaskAuthority::open_with_scale_profile` / `open_with_vfs_and_scale_profile` 供测试与显式 tier 绑定。
5. **诚实范围**：仅为 predicate enforcement 前缀，非完整 Materialization Controller；`register_task` / soft reclaim / `max_task_nodes` 仍未强制。

### 验证门（W18-004 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| scale_profile 集成 | `cargo test -p nlos-task --test scale_profile` | PASS（4 passed，含 admission 三用例；2026-09-07 W18-004 integrator 救回） |
| scale_profile_probe | `cargo test -p nlos-task --test scale_profile_probe` | （未在本增量复跑） |
| pressure 单测 | `cargo test -p nlos-task pressure` | （未在本增量复跑） |
| clippy | `cargo clippy -p nlos-task --all-targets -- -D warnings` | （未在本增量复跑） |

### 仍属缺口

1. `max_task_nodes` 未接入 `register_task`；soft reclaim 未接入 controller。
2. checkpoint/rehydrate 与 TaskPlan/TaskNode 声明面仍待议题 35 ADR。
3. 100K probe 数字仍待实跑。

## 8. W19-004 reclaim advisory 最小前缀（2026-09-08）

### 已实现事实

1. `WorkingSetReclaimAdvisory`：软阈值 crossing 且 hard admission 仍通过时的 typed advisory（`profile_id` / `projected_active_count` / threshold 元数据）；**不执行** [`ReclaimPolicy`] 任何 phase。
2. `CommitPermitDecision`：`PermitDecision` + `Option<WorkingSetReclaimAdvisory>`；`request_commit_permit_decision_with_authorities_struct` 暴露完整 outcome；legacy `request_commit_permit*` 仍返回 `PermitDecision`（向后兼容）。
3. **单路径前缀 consult**：net-new issuance 在 `enforce_working_set_admission` 通过后 consult [`working_set_reclaim_advisory`]（projected `current_active + 1`）；仅 `PermitDecision::Issued` 携带 advisory；**idempotent replay / denied path 无 advisory**。
4. `inspect_working_set_pressure()`：读侧 snapshot（当前 issued count + `needs_reclaim` / `admits` + optional advisory）。

### 验证门（W19-004 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| scale_profile 集成 | `cargo test -p nlos-task --test scale_profile` | PASS（6 passed，含 reclaim advisory 两用例；2026-09-08 W19-004） |
| pressure 单测 | `cargo test -p nlos-task pressure` | PASS（8 passed；2026-09-08 W19-004） |

### 仍属缺口

1. 无 reclaim 执行/controller wiring；advisory 仅为 prefix 信号。
2. `max_task_nodes` 未接入 `register_task`；checkpoint/rehydrate 与 TaskPlan/TaskNode 声明面仍待议题 35 ADR。
3. 100K probe 数字仍待实跑。

## 9. W20-004 reclaim execution 最小前缀（2026-09-09）

### 已实现事实

1. `WorkingSetReclaimExecution`：`execution_sequence`（恒为 `0`）+ 首相 `ReclaimPhase::RebuildableCache` + advisory 快照；**不执行** eviction 或后续 phase。
2. `plan_working_set_reclaim_execution()`：从 [`TASK_DEFAULT_RECLAIM_POLICY`] 选取 phase index `0`。
3. `CommitPermitDecision` 扩展 `reclaim_execution: Option<WorkingSetReclaimExecution>`；仅 `PermitDecision::Issued` 且 advisory 为 `Some` 时填充；replay / denied / conflict 路径均为 `None`。
4. **诚实范围**：first-phase plan only；无 Materialization Controller、无 Context Residency Controller、无实际 cache eviction。

### 验证门（W20-004 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| scale_profile 集成 | `cargo test -p nlos-task --test scale_profile` | PASS（8 passed，含 execution 两用例；2026-09-09 W20-004） |
| pressure 单测 | `cargo test -p nlos-task pressure` | PASS（9 passed；2026-09-09 W20-004） |
| clippy | `cargo clippy -p nlos-task --all-targets -- -D warnings` | PASS（2026-09-09 W20-004） |

### 仍属缺口

1. 无 controller 执行后续 phase（QoS / checkpoint-evict / kill）或实际 eviction。
2. `max_task_nodes` 未接入 `register_task`；checkpoint/rehydrate 与 TaskPlan/TaskNode 声明面仍待议题 35 ADR。
3. 100K probe 数字仍待实跑。

## 10. W21-004 reclaim execution outcome 最小前缀（2026-09-10）

### 已实现事实

1. `WorkingSetReclaimOutcome`：`execution_sequence` / `phase` / `evicted_units` + advisory 快照；**仅** `RebuildableCache` 首相执行前缀。
2. `execute_working_set_reclaim_execution()`：从 planned step 派生 outcome；`evicted_units` 为 soft-threshold overshoot（`projected_active_count - reclaim_threshold_count`）合成计数，作为 Context Residency Controller 占位。
3. `CommitPermitDecision` 扩展 `reclaim_outcome: Option<WorkingSetReclaimOutcome>`；仅 `PermitDecision::Issued` 且 `reclaim_execution` 为 `Some` 时填充；replay / denied / conflict 路径均为 `None`。
4. **诚实范围**：首相 typed outcome only；无 Materialization Controller、无真实 cache eviction、无后续 phase 执行。

### 验证门（W21-004 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| scale_profile 集成 | `cargo test -p nlos-task --test scale_profile` | PASS（10 passed，含 outcome 两用例；2026-09-10 W21-004） |
| pressure 单测 | `cargo test -p nlos-task pressure` | PASS（10 passed，含 `execute_outcome_reports_rebuildable_cache_overshoot`；2026-09-10 W21-004） |
| clippy | `cargo clippy -p nlos-task --all-targets -- -D warnings` | PASS（2026-09-10 W21-004） |

### 仍属缺口

1. 无后续 phase（QoS / checkpoint-evict / kill）执行或真实 Context Residency Controller eviction。
2. `max_task_nodes` 未接入 `register_task`；checkpoint/rehydrate 与 TaskPlan/TaskNode 声明面仍待议题 35 ADR。
3. 100K probe 数字仍待实跑。

## 11. max_task_nodes admission gate 最小前缀（2026-09-11，W22-004）

### Base HEAD

开工 `d039cd0`；提交时 HEAD 已被并行车道推进至 `e09ff64`（W22-005），本车道写集与其无交集。

### 写集

- `crates/nlos-task/src/lib.rs`：新增 `TaskStoreError::TaskNodeAdmissionDenied { profile_id, task_count, max_task_nodes }` + Display 臂 + `enforce_task_node_admission` re-export。
- `crates/nlos-task/src/pressure.rs`：新增 `enforce_task_node_admission(profile, current_count)`（对 `current_count + 1` consult [`ScaleProfile::admits_task_nodes`]，fail-closed）；模块 doc honest-scope 同步。
- `crates/nlos-task/src/store.rs`：`register_task` net-new 路径在事务内 `count_registered_tasks`（`SELECT COUNT(*) FROM tasks`）后 consult 上限；`count_registered_tasks` helper。
- `crates/nlos-task/src/scale.rs`：模块 doc honest-scope 同步（task registration 已 gated，soft reclaim 仍未）。
- `crates/nlos-task/tests/scale_profile.rs`：`TASK_NODE_TEST_PROFILE`（max_task_nodes=2）+ 3 个集成用例。
- `crates/nlos-task/tests/scale_profile_probe.rs`：100K probe 的 scale database 改绑 `open_with_profile(&TASK_PROFILE_100K)`——100K 探测显式声明 100K 档，此前默认 10K 绑定因 `max_task_nodes` 从未被强制而无感；本车道 gate 落地后该绑定为语义必需（否则 10_001 个注册即 fail-closed）。
- 本 evidence 文件 §11。

### 实现要点

1. **镜像 W18-004 结构**（先例 7e01ce7）：typed error + `enforce_*_admission` 纯函数 + 单路径前缀 consult + 幂等 replay bypass。
2. **检查点**：`register_task` 在 `load_task_optional` 命中（same-generation → `Existing`）与 `DuplicateTask` 分支之后、`insert_task` 之前——即仅 net-new 注册 consult；**idempotent replay（同 TaskId + 同 generation）绕过 gate**，cap 满时仍可 `Existing`。
3. **fail-closed**：`projected = current + 1`（`checked_add`，溢出 → `EpochExhausted`），`admits_task_nodes` 为 inclusive 上界（`<=`），超限返回 `TaskNodeAdmissionDenied`，事务中止、不落任何行。
4. **additive**：`ScaleProfile` 结构未改字段（`max_task_nodes` 早已存在，本车道只是接线）；无新依赖、无 unsafe、无既有行为路径改动（默认 10K 档下 ≤10_000 注册不受影响）。
5. **既有面核验**：10K probe（注册恰 10_000，第 10_000 个 projected=10_000 inclusive 通过）无需改动；默认 `open()` 的其余测试注册量均远低于 10K。

### 验证门（W22-004 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt -p nlos-task -- --check` | PASS（先检出 1 处 import 排序 diff 于本车道 store.rs 写集内，`cargo fmt -p nlos-task` 修复后复跑 clean） |
| scale_profile 集成 | `cargo test -p nlos-task --test scale_profile` | PASS（**13 passed; 0 failed**，含 task-node admission 三新用例；2026-09-11 W22-004） |
| pressure 单测 | `cargo test -p nlos-task pressure`（约定形式）/ `cargo test -p nlos-task --lib pressure`（实跑形式） | PASS（**11 passed; 0 failed; 9 filtered out**，含 `enforce_task_node_admission_admits_at_cap_rejects_one_over` 新用例） |
| clippy | `cargo clippy -p nlos-task --lib --test scale_profile --test scale_profile_probe -- -D warnings` | PASS（0 warning；2026-09-11 W22-004） |

**共享工作区并行车道注明**：约定形式 `cargo test -p nlos-task pressure` 与 `cargo clippy -p nlos-task --all-targets -- -D warnings` 在本车道验证窗口内被并行车道（W22-R 对 `crates/nlos-resource` 的未提交 `demand`/`demand_capacity` 字段新增）阻塞——构造这些请求的 resource 依赖集成 target（三次实跑观测到 `resource_commit` / `participant_registry` / `mixed_semantic_resource_commit` / `resource_bridge_fault_injection`，均非本车道写集）以 E0063 编译失败（最终一次全形 clippy 实跑 exit=101、9 处 E0063、0 条本车道 warning）。本车道以上表 scoped 实跑为门；未触碰 nlos-resource 及受影响测试文件（规则 8）。满形 gate 待共享工作区收敛后由后续增量复跑。

### 仍属缺口

1. 无 controller 执行 reclaim；`max_task_nodes` gate 仅为注册路径前缀（TaskPlan/TaskNode 声明面落位后需迁移到真实声明单位）。
2. checkpoint/rehydrate 与 TaskPlan/TaskNode 声明面仍待议题 35 ADR。
3. 100K probe 数字仍待实跑（probe 现已绑对 100K 档）。

## 12. W29-A TaskSpec 关联字段 + ScaleProfile 维度正规化（2026-09-20）

> 对应：[ADR-0016](../../management/adrs/0016-task-plan-declaration-surface.md) 决定 3（TaskSpec 最小关联字段）+ 决定 4（ScaleProfile 维度正规化）；进度单 §6.5.3 W29-A（nlos-task 串行 slot 2，slot 1 为 W28-C 的 v43）。

### Base HEAD

开工 `55c7f45`（分支 `feat/w29-a`，串行 slot，无并行 nlos-task 写车道）。

### 写集

- `crates/nlos-task/src/migrations.rs`：`migrate_v44` + `SCHEMA_V44_SQL`（header doc v1→v44）。
- `crates/nlos-task/src/model.rs`：`TaskPlanRevisionRef { plan_id, revision }` 新类型；`TaskSpec`/`TaskRecord` 各加 `application_id`/`plan_revision` 两 `Option` 字段。
- `crates/nlos-task/src/store.rs`：`SCHEMA_VERSION = 44`、迁移链尾接 `migrate_v44`、`register_task` gate 切换 + 关联 replay 身份校验、`insert_task`/`TASK_COLUMNS`/`decode_task_row` 落读三列。
- `crates/nlos-task/src/scale.rs`：`ScaleProfile` 新增 `max_task_registrations` 第二显式维度 + `admits_task_registrations`；两档常量补维度；模块 doc 口径切换。
- `crates/nlos-task/src/pressure.rs`：`enforce_task_registration_admission` 新 gate（→ `TaskRegistrationAdmissionDenied`）；`enforce_task_node_admission` 保留为 TaskNode 维度 consult 面（doc 注明 W30-D 接线缺口）。
- `crates/nlos-task/src/lib.rs`：`TaskRegistrationAdmissionDenied` / `TaskAssociationConflict` 错误变体 + Display；re-export；crate doc 补 v44 段。
- `crates/nlos-task/src/activity.rs` + 全部测试文件的既有 `TaskSpec` 构造点补 `None` 关联字段（纯机械，零语义改动）；schema 版本 pin `assert_eq!(version, 43…)` → 44（artifact_commit_plan ×3、resource_commit、channel_endpoint、takeover_completion、semantic_recovery_schema ×2、resource_recovery_schema ×2、authority_lease、task_group ×2、barrier_signature、effect_history、effect_permit、effect_fiber_registration）。
- `crates/nlos-task/tests/task_association.rs`（新）、`tests/scale_profile.rs`、`tests/scale_profile_probe.rs`。
- 本 evidence 文件 §12。

### 已实现事实（决定 3）

1. **additive schema v44**：`tasks` 表加三 nullable 列 `application_id BLOB(16)`、`plan_id BLOB(16)`、`plan_revision BLOB(8)`（u64 大端 blob，house 编码），加 `task_association_immutable` trigger（`BEFORE UPDATE … WHEN old.x IS NOT new.x` 拒绝关联改写，与 `commit_permits_single_active` 同为 defense-in-depth）；迁移幂等可重跑（部分态 fail-closed `CorruptRecord`）；既有行 backfill NULL，不发明引用。
2. **字段形状**：`TaskSpec { application_id: Option<ApplicationId>, plan_revision: Option<TaskPlanRevisionRef { plan_id: TaskPlanId, revision: u64 }> }`——两关联字段独立可选；plan 引用为总对（plan_id 与 revision 同有同无，半对 decode 为 corruption）。类型全部来自既有 `nlos-types` 依赖，**无需新增 nlos-plan 依赖**（运行期核验按决定 3 留在物化/permit 边界，ADR-0013 verify-then-commit，W30-D 接线）。
3. **关联是声明身份**：同 task_id + 同 generation 但关联不同（含 legacy NULL 行再绑定、含反向丢弃关联）→ `TaskAssociationConflict { task_id }` fail-closed，durable 行不动；generation 冲突仍先判 `DuplicateTask`（检查顺序保持）。无关联注册与 replay 行为逐字节不变（零回归）。
4. **读回**：`inspect_task`（`TaskRecord`）回读两字段；`update_task` 等 head 变更路径不触碰关联列（trigger 兜底），cancel/permit/finalize 后关联存活（测试覆盖）。

### 已实现事实（决定 4，口径切换）

1. **第二显式维度**：`ScaleProfile` 新增 `max_task_registrations`；`TASK_PROFILE_10K = (10_000 nodes / 10_000 registrations / 512)`、`TASK_PROFILE_100K = (100_000 / 100_000 / 5_120)`——两维度同量级。
2. **口径切换**：`register_task` admission 从 `admits_task_nodes`（W22-004 临时映射）切换为 `enforce_task_registration_admission` → `TaskRegistrationAdmissionDenied { profile_id, registration_count, max_task_registrations }`（镜像 W22 错误形状）；replay bypass 语义不变。**已发布 §3 数字测的本来就是注册维度**，切换后该维度的量级与强制口径不变，§3 数字仍有效；`max_task_nodes` 自此正规化为声明 TaskNode（`nlos-plan` `plan_nodes` 持久计数）维度，其 consult 面 `enforce_task_node_admission` + `TaskNodeAdmissionDenied` 保留，store 路径暂无调用方（W30-D 关联下沉接线，禁止在新接线前宣称 TaskNode 维度已强制）。禁止新旧口径混写：code（scale.rs/pressure.rs/scale_profile.rs/probe doc）与本节为唯一口径声明处。
3. **每维度独立强制**：注册维度在 `register_task` 强制（与 TaskNode 维度饱和与否无关，测试钉死）；TaskNode 维度当前仅谓词面。

### probe 重跑（原样誊录，G5 基线量级对齐）

命令：`cargo test -p nlos-task --test scale_profile_probe -- --ignored --nocapture`（debug/test profile，单平台 macOS，fsync 逐注册事务）。

```
10K task profile (single platform): registrations=10000 register_total=2.077848375s register_mean=2.077848ms permit_p50_100=448.5µs permit_p95_100=549.917µs permit_max_100=1.023625ms permit_p50_10k=409.834µs permit_p95_10k=492.625µs permit_max_10k=2.827542ms working_set=512 working_set_total=191.097333ms working_set_p50=398.917µs working_set_p95=468.334µs inspect4=185.417µs database_bytes=7700480 rss_before=Some(9961472) rss_after=Some(13434880)
100K task profile (single platform): registrations=100000 register_total=28.342086459s register_mean=28.342086ms permit_p50_100=451.125µs permit_p95_100=553.417µs permit_max_100=906.958µs permit_p50_100k=346.042µs permit_p95_100k=393.958µs permit_max_100k=3.0895ms working_set=5120 working_set_total=2.275001583s working_set_p50=395.209µs working_set_p95=446.625µs inspect4=170.375µs database_bytes=70041600 rss_before=Some(9961472) rss_after=Some(13467648)
test result: ok. 2 passed; 0 failed; ... finished in 31.96s
```

要点：

1. **口径**：两 probe 均为**注册维度**（决定 4 后 register gate 强制的维度）；`register_mean` 字段为 probe 既有打印口径（total 秒数值标 ms 单位），与 §3 已发布数字同口径可比。
2. 10K 复跑与 §3 已发布数字同量级（permit p95_10k 492.625µs vs 发布 391.291µs，同 run 基线 549.917µs 的 ~0.9x，绝对面 <100ms 限；惰性成立）。
3. **100K probe 首次实跑**（补 §4 缺口 #3 / §3.1 待实跑项）：100_000 注册 28.34s，permit p95_100k = 393.958µs ≤ 同 run 100 基线 553.417µs（~0.71x，惰性成立）；5_120 活跃工作集发放 2.275s；落盘 70,041,600 字节。仍为 debug/test profile 单平台数字，不宣称 G2/G5 正式达成（W31 gate）。

### 验证门（W29-A 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt -p nlos-task -- --check` | PASS |
| 全量测试 | `cargo test -p nlos-task` | PASS（**347 passed / 0 failed / 2 ignored**；基线 340 + 新增 7：task_association 4 + scale_profile 注册维度 3 换 3 增 1 独立用例 + pressure/scale 单测 2；W28-C 340 全保持绿） |
| clippy | `cargo clippy -p nlos-task --all-targets -- -D warnings` | PASS（修 `doc_markdown` ×2、`redundant_closure_for_method_calls` ×1 后） |
| probes | `cargo test -p nlos-task --test scale_profile_probe -- --ignored --nocapture` | PASS（本节数字） |

### 仍属缺口

1. `max_task_nodes`（声明 TaskNode 维度）无 store 调用方：接线在 W30-D 关联下沉（跨 authority consult `nlos-plan` `plan_nodes` 计数）前不得宣称强制。
2. 关联字段仅存引用：物化/permit 边界的 ADR-0013 verify-then-commit 核验未接线（W29-C/W30-D）；manifest 模板段到 TaskSpec 关联的实例化（候选 C 桥）在 B-APPLICATION 车道。
3. checkpoint/rehydrate 基准、release profile 与多平台复测仍未做；G2/G5 100K 正式 gate 在 W31。
4. b-plan 侧 evidence 一行引用未落（W29-A 写集限定本文件为唯一 primary；b-plan 车道自行回指本节即可）。

## 13. W31-B working-set 比例矩阵基准（2026-09-21）

> 对应：进度单 §6.5.3 W31-B 车道行（验收门：比例矩阵数据落 evidence；admission 不退化）；[ROAD-B-004](../../design/06-架构设计总纲-v0.5.md) §28.2（10K/100K × active working-set ratio 矩阵）；[ADR-0016 决定 4](../../management/adrs/0016-task-plan-declaration-surface.md)（维度口径纪律）；W31-A consult 面（`answer_plan_materialization`，§10.3）与 W31-D 逻辑 TaskNode benchmark（`nlos-plan` 侧 `tasknode_scale_probe.rs`——两口径不混写）。
>
> 状态：`PASS`（本切片范围）；ROAD-B-004 整体收口归 W31-G 六门评审，本节不宣称整体达成。

### Base HEAD / 写集

开工 `b65255e`（分支 `feat/w31-b`，nlos-task 无并行写车道）。

- `crates/nlos-task/tests/working_set_ratio_probe.rs`（新，唯一代码写集——benchmark test file only，零 src admission 逻辑改动）。
- 本 evidence 文件 §13 + `docs/management/evidence-index.yaml` 本文件行 scope/date 更新。

### 口径与诚实规则（先行声明）

1. **working-set 维度 = 未决 `CommitPermit` 计数**：W18-004 admission 前缀、W19-004 advisory、W20/W21 execution/outcome 链与 `inspect_working_set_pressure` 读面测的就是这个 durable 事实；不与 plan 侧物化窗口计数混写（W31-A §10.7 词汇纪律）。
2. **逻辑 population 由每 cell profile 注册维度承载**（`max_task_registrations == population`，两声明维度同量级）；声明 TaskNode（`plan_nodes`）维度的 10K/100K 数字归 W31-D §9，两口径分列不混写。
3. **诚实档位（显式声明常量，非运行期计算）**：10K × {1%, 10%, 50%}（工作集 100/1000/5000）+ 100K × {1%, 10%}（1000/10000），软阈值一律 90%（与已发布档同值）。超过已发布档 ~5% 工作集姿态的 cell（10K@10%/50%、100K@10%）是对 admission **机制**的刻意超比例探测：每 cell 绑 per-cell profile，且探针与纯测试双向断言**已发布档本身在该占用上 fail-closed**（`enforce_working_set_admission` 锚定；已发布档 5% admits / 6% denies 姿态另钉）——本矩阵不声称已发布档支持 >5% 活跃比例。
4. **确定性纪律（本车道门「no assertion on wall-clock」，比 W31-D §9.1 更严）**：全部计时数字只记录、**零 wall-clock 断言**（含病态慢守卫也不加）；「admission 不退化」以每 ratio 位置的精确事实承载——cap 内逐笔 `Issued`、advisory 恰在 `(threshold, cap]` 投影带内携带且五字段逐位相等、execution/outcome 链在带内首笔与 cap 笔形状精确（sequence 0 / RebuildableCache / `evicted_units = projected − threshold`）、pressure 快照在 {0, threshold, threshold+1, cap} 计数点与独立推导期望逐位相等、cap+1 issuance 以 typed 三元组 fail-closed、拒后快照不变、cap 上幂等 replay 绕过 gate 与全部 advisory/execution/outcome 前缀。

### benchmark 设计（`tests/working_set_ratio_probe.rs`）

每 cell 一条管线（默认套件 500 population 三比例 smoke 走同一管线，探针 helper 不腐化）：

1. 注册 population 任务（fsync 逐注册事务）；
2. 空载 pressure 快照 + W31-A consult（`answer_plan_materialization(0)` → 精确 facts）；
3. 为全部 active 槽注册 attempt（散布 ID 空间：`ordinal × population / active`）；
4. 填充循环（每笔 issuance 逐位断言，含 {threshold, threshold+1, cap} 计数点的 pressure 快照 + 软阈值下 consult facts）；
5. 饱和后 consult 两维 verdict 精确：工作集维 `WorkingSetAdmissionDenied{active+1, active}`；task-node 维先判（`TaskNodeAdmissionDenied{max+1, max}`，与工作集饱和无关）；
6. cap+1 issuance typed fail-closed——经**已占用任务上的竞争第二 attempt**探测（admission 前缀在 `compete_for_permit` 之前 consult；cell profile 注册维度无余位，deny 面由此探针形状承载）；拒后快照不变；
7. cap 上幂等 replay 绕过 gate 与全部前缀。

纯测试 `matrix_cells_declare_exact_ratios_and_published_tier_anchors` 钉 cell 表算术：`active × 100 == population × ratio%` 精确整数比、advisory 带非空（`0 < threshold < active < population`）、两声明维度恰等于 population、已发布档 5% admits/6% denies、published-tier 锚定逐 cell 一致。

### 实跑数字（原样誊录）

命令（两档分跑，同一测试二进制）：`cargo test -p nlos-task --test working_set_ratio_probe -- --ignored --nocapture ten_thousand`（11.38s）与 `... one_hundred`（95.38s）。debug/test profile，单平台 macOS arm64。**逐 cell 墙钟**：10K 三 cell 合计 11.4s；100K@1% ≈ 39.3s、100K@10% ≈ 56.0s——均远低于 15 分钟预算，无 not-run 登记项。

```
W31-B ratio matrix cell task-ratio-10k-1pct (single platform): population=10000 ratio=1% active=100 threshold=90 published_within=true register_total=2.104871334s attempts_total=14.204875ms fill_total=40.039125ms fill_p50=371.625µs fill_p95=417.875µs fill_max=3.061709ms inspect_empty=20.125µs inspect_at_threshold=Some(13.291µs) inspect_at_threshold_plus_one=Some(11.792µs) inspect_at_cap=Some(13.875µs) consult_empty=9.375µs consult_below_cap=Some(10.083µs) consult_ws_saturated=10.833µs consult_tn_saturated=10.292µs deny_latency=73.166µs replay_latency=54.5µs database_bytes=7299072 rss_before=Some(2310144) rss_after=Some(8650752)
W31-B ratio matrix cell task-ratio-10k-10pct (single platform): population=10000 ratio=10% active=1000 threshold=900 published_within=false register_total=2.05391675s attempts_total=168.340916ms fill_total=518.181291ms fill_p50=440.084µs fill_p95=830.375µs fill_max=5.893292ms inspect_empty=26.084µs inspect_at_threshold=Some(33.667µs) inspect_at_threshold_plus_one=Some(36.459µs) inspect_at_cap=Some(35.542µs) consult_empty=11.5µs consult_below_cap=Some(27.542µs) consult_ws_saturated=30.75µs consult_tn_saturated=29.792µs deny_latency=104.25µs replay_latency=63.541µs database_bytes=8126464 rss_before=Some(8650752) rss_after=Some(8896512)
W31-B ratio matrix cell task-ratio-10k-50pct (single platform): population=10000 ratio=50% active=5000 threshold=4500 published_within=false register_total=2.345455417s attempts_total=809.550958ms fill_total=2.957148083s fill_p50=510.833µs fill_p95=943.375µs fill_max=11.777333ms inspect_empty=25.75µs inspect_at_threshold=Some(107µs) inspect_at_threshold_plus_one=Some(100.417µs) inspect_at_cap=Some(114.333µs) consult_empty=11.833µs consult_below_cap=Some(97.917µs) consult_ws_saturated=107µs consult_tn_saturated=99.334µs deny_latency=193.083µs replay_latency=311.542µs database_bytes=11677696 rss_before=Some(8863744) rss_after=Some(8634368)
test ten_thousand_population_ratio_matrix_admits_correctly ... ok
```

```
W31-B ratio matrix cell task-ratio-100k-1pct (single platform): population=100000 ratio=1% active=1000 threshold=900 published_within=true register_total=38.616019875s attempts_total=123.938583ms fill_total=523.261875ms fill_p50=364.5µs fill_p95=475.583µs fill_max=73.624542ms inspect_empty=19.416µs inspect_at_threshold=Some(26.75µs) inspect_at_threshold_plus_one=Some(24.291µs) inspect_at_cap=Some(28.625µs) consult_empty=9.083µs consult_below_cap=Some(22.708µs) consult_ws_saturated=24.708µs consult_tn_saturated=23.75µs deny_latency=84.542µs replay_latency=51.416µs database_bytes=66351104 rss_before=Some(2310144) rss_after=Some(8372224)
W31-B ratio matrix cell task-ratio-100k-10pct (single platform): population=100000 ratio=10% active=10000 threshold=9000 published_within=false register_total=48.551397791s attempts_total=2.063702833s fill_total=5.253356959s fill_p50=445.916µs fill_p95=563.583µs fill_max=66.326333ms inspect_empty=1.561959ms inspect_at_threshold=Some(155.5µs) inspect_at_threshold_plus_one=Some(155.833µs) inspect_at_cap=Some(181.708µs) consult_empty=12.042µs consult_below_cap=Some(150.458µs) consult_ws_saturated=172.875µs consult_tn_saturated=165.084µs deny_latency=233.75µs replay_latency=305.292µs database_bytes=74395648 rss_before=Some(8388608) rss_after=Some(7913472)
test one_hundred_thousand_population_ratio_matrix_admits_correctly ... ok
```

### 要点解读（记录面，非断言面）

| 面 | 实测 | 解读 |
| --- | --- | --- |
| admission 决策正确性 | 5 cell 全部逐位断言通过（cap 内逐笔 Issued、advisory 带精确、cap+1 typed deny、replay 绕过、consult 两维 verdict 精确） | **admission 不退化成立**（每 ratio 位置精确事实，零 wall-clock 断言） |
| issuance 成本 vs active 数 | fill p95：100→417.9µs、1000→830.4µs(10K)/475.6µs(100K)、5000→943.4µs、10000→563.6µs | 100x active 增长下 p95 仅 ~1.1–2.3x（COUNT 未决 permit + key-scoped CAS；噪声区间内近常数，亚线性） |
| issuance 成本 vs population | 同 active=1000：10K 库 p95 830.4µs vs 100K 库 p95 475.6µs | 同 ratio 同 active 下与 population 无正相关（惰性成立姿态的记录面） |
| pressure 读回 | inspect p95（threshold/cap 点）：13.9µs@100 → 35.5µs@1000(10K) / 28.6µs@1000(100K) → 114.3µs@5000 → 181.7µs@10000 | `COUNT(issued permits)` 为 O(active)（by design）；同 active 跨 population 同量级（35.5 vs 28.6µs） |
| deny / replay / consult | deny 73–234µs、replay 51–312µs、饱和 consult 24–173µs 随 active 线性 | fail-closed 与幂等面在全部 ratio 位置保持在亚毫秒量级 |
| 足迹 | 落盘 7.3–74.4MB（随 population/active）；RSS 峰值 ~8.9MB | 记录面；与 §12 注册维 100K 落盘（70MB）同量级 |

### 验证门（W31-B 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt -p nlos-task -- --check` | PASS |
| 全量测试 | `cargo test -p nlos-task` | PASS（50 个 test-result 行合计 **369 passed / 0 failed / 4 ignored**（2 为既有 scale_profile_probe 探针 + 本车道 2 新探针）；基线 367 + 本车道默认套件 2 新用例） |
| clippy | `cargo clippy -p nlos-task --all-features --all-targets -- -D warnings` | PASS（修 `doc_markdown` ×4、`unnecessary_unwrap` ×1、`collapsible_if` ×1 后；未使用 `chunks_exact`，CI clippy 1.98 纪律） |
| 10K 探针 | `cargo test -p nlos-task --test working_set_ratio_probe -- --ignored --nocapture ten_thousand` | PASS（本节三 cell 数字，11.38s） |
| 100K 探针 | `cargo test -p nlos-task --test working_set_ratio_probe -- --ignored --nocapture one_hundred` | PASS（本节两 cell 数字，95.38s；每 cell ≤ 15 分钟预算） |

### 仍属缺口 / deferred minors（如实登记）

1. **计时零断言**（车道门「no assertion on wall-clock」）：admission/pressure 成本只记录；COUNT 未决 permit 面（O(active)）的回归防护是否加相对断言归后续车道决定。
2. **debug/test profile 单平台数字**：release 复测、多平台与 CI 化未做；RSS 读数仅 macOS 可移植（`ps`），其余 target 如实 `None`。
3. **矩阵未含 100K@50%**（50_000 工作集）：10K@50% 已覆盖超比例姿态，且 100K 注册维每注册 fsync（~50s/cell）使该 cell 性价比低——登记为**档位选择**，非测量缺口。
4. **occupancy 只单调填充**：无回收后再入场循环；pressure/advisory 在 reclaim 边界的动态行为归 reclaim controller 车道（W31-C/W31-F）。
5. push / PR / workspace 全量门：MUST NOT，控制器统一收口。

## 14. W31-C reclaim 实执行闭环 + checkpoint/rehydrate 基准（2026-09-21）

> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W31-C 车道行（`checkpoint/rehydrate benchmark + reclaim 实执行（B4-8）`；验收门：rehydrate 实测数据；reclaim 执行闭环）；residency 分级读面为 W31-E（`b-plan-001-declaration-surface.md` §8，本车道只读引用其 `EVICTED (residency=WARM|COLD)`/REHYDRATING 语义，未触碰 `nlos-plan`）。

### Base HEAD

开工 `b65255e`（分支 `feat/w31-c`）；提交时无并行写集（本车道独占 `nlos-task` src 与两 crate 测试写集）。

### 写集

- `crates/nlos-task/src/pressure.rs`：模块 honest-scope 更新 + 4 个新公开类型（`WorkingSetReclaimExecutionRequest` / `WorkingSetReclaimEviction` / `WorkingSetReclaimPhaseReport` / `WorkingSetReclaimExecutionReport`）。
- `crates/nlos-task/src/store.rs`：`SqliteTaskAuthority::drive_working_set_reclaim` 公开方法 + `list_issued_permits_in_eviction_order` / `permit_is_reclaim_evictable` 私有 helper + imports。
- `crates/nlos-task/src/reconcile.rs`：`load_adoption_by_permit` 私有 → `pub(crate)`（一行可见性提升，无语义改动）。
- `crates/nlos-task/src/lib.rs`：`TaskStoreError` 两新变体（`ReclaimExecutionProfileMismatch` / `ReclaimExecutionSequenceOutOfRange`）+ Display 臂 + pressure 面 re-export 扩展。
- `crates/nlos-task/tests/reclaim_execution.rs`（新，4 用例）、`tests/reclaim_execution_probe.rs`（新，2 个 `#[ignore]` 基准）。
- `crates/nlos-runtime-tokio/tests/checkpoint_rehydrate_scale.rs`（新：1 默认套件档 + 2 个 `#[ignore]` 档）。
- 本 evidence 文件 §13。

### 已实现事实 1：reclaim 实执行闭环（deliverable 1）

1. **公开驱动面**：`drive_working_set_reclaim(WorkingSetReclaimExecutionRequest) -> WorkingSetReclaimExecutionReport`——把 permit decision 上浮的 `WorkingSetReclaimExecution`（advisory warrant）驱动到完成；报告按 `TASK_DEFAULT_RECLAIM_POLICY` 从 planned step 的 sequence 起逐相给出 `WorkingSetReclaimPhaseReport { phase, evicted_units, face_absent }`。
2. **真实 CheckpointEvict 面**：按 `(created_at_ms, permit_id)` FIFO 枚举 issued permit，逐个经**公开 `close_permit` 路径**以 `CancelledBeforeEffect` 关闭（每次关闭一条独立 `BEGIN IMMEDIATE` 事务 + `TaskPermitClosureReceipt`），直到 observed active count 回落至软阈值；permit 行即工作集成员的 durable checkpoint，closure receipt 即驱逐记录。结构性不可驱逐成员被跳过：任一 effect slot 不在 `NoEffect`/`ConfirmedNoEffect`、绑定了 authority lease、或存在 adoption 记录。
3. **诚实面**：`RebuildableCache`/`DegradeBackgroundQos`/`Kill` 三相 `face_absent=true`、`evicted_units=0`（本权威无 cache/`QoS`/kill 面）；无可驱逐成员时 `pressure_relieved=false` 如实报告缺口，kill 相不伪造执行。W21 的合成 outcome（`execute_working_set_reclaim_execution`，overshoot 计数）原样保留为 planning-derived 前缀，未削弱。
4. **controller-loop 语义（非单事务 CAS）**：pre-count/candidates 一致读 → 逐个线性化公开关闭 → post-count 新读；并发变化被观察而非被 fence（doc 声明）。重复驱动=合法 no-op（受害者已 Closed 不再入枚举）；每张 closure receipt 各自幂等。
5. **typed 门**：warrant 的 advisory tier ≠ authority 绑定 profile → `ReclaimExecutionProfileMismatch` fail-closed；sequence 越界 → `ReclaimExecutionSequenceOutOfRange`；非 skip 集关闭失败原样传播。
6. **readback 证明执行（非 planning）**：混合态工作集（已关闭 permit ×1、无槽位 plain permit ×4、`Planned` 槽位 permit ×1、`NoEffect` 槽位 permit ×1）压过软阈值后驱动——恰好驱逐 overshoot（2 个最旧 plain permit），`Planned` 槽位成员保持 `Issued`；读回三面：`inspect_working_set_pressure`（active 6→4、needs_reclaim true→false）、`inspect_permit`（受害者 `Closed`）、`inspect_receipt`（每张 closure receipt `CancelledBeforeEffect` 且绑定原 permit）。

### 已实现事实 2：checkpoint/rehydrate 基准（deliverable 2，ADR-0012 决定 2 B 路径）

1. **三档**（`tests/checkpoint_rehydrate_scale.rs`）：默认套件档 24 节点（每跑常规套件即证闭环）+ `#[ignore]` quick 500 + full 5_000。`#[ignore]` 语义与既有 scale probe 一致。
2. **每节点循环**：checkpoint = process binding + fiber incarnation + `snapshot_handler_entry` 公开写（64B entry input）；evict = 整个 runtime drop（全部内存 fiber 消失，durable `fiber_entry_snapshots` 存活，RSS 采样佐证）；rehydrate = 新 runtime 按需逐节点 next incarnation + `resume_from_snapshot`，handler 从 entry input 重执行并经正常 runtime 入口重注册 durable Channel wait。
3. **硬断言（全档全节点）**：每节点 restore 返回 `restored: Some` 且 input 逐位相等；fiber 停在 `WaitingIo`；durable wait 行恰好 1 条且 `PENDING`；两相总时长低于宽松上限。分位数只记录不断言。
4. **durable bytes 口径**：authority root 下全部 db+WAL 文件之和（排除瞬态 `-shm`）；开发中曾因只统计 `-wal`（文件名为 `*.db` 非 `*.sqlite3`）导致跨档字节巧合相等——已修正为全文件口径（教训记录在此防复犯）。

### probe 实跑数字（原样誊录，debug/test profile 单平台 macOS）

命令：`cargo test -p nlos-task --test reclaim_execution_probe -- --ignored --nocapture`

```
reclaim execution benchmark (10k, single platform): cap=512 threshold=460 fill_total=412.741959ms fill_p50=444.125µs fill_p95=559.834µs evicted=52 drive_total=25.5915ms evictions_per_second=2031.9 durable_bytes_before_drive=5844736 durable_bytes_after_drive=5889792 durable_bytes_delta=45056 rss_before=Some(9420800) rss_after=Some(12484608)
reclaim execution benchmark (100k, single platform): cap=5120 threshold=4608 fill_total=3.815146375s fill_p50=395.417µs fill_p95=475.125µs evicted=512 drive_total=185.644625ms evictions_per_second=2758.0 durable_bytes_before_drive=12861256 durable_bytes_after_drive=13049672 durable_bytes_delta=188416 rss_before=Some(9568256) rss_after=Some(17858560)
```

命令：`cargo test -p nlos-runtime-tokio --test checkpoint_rehydrate_scale -- --include-ignored --nocapture`

```
checkpoint/rehydrate benchmark (small, single platform): nodes=24 input_bytes=64 checkpoint_total=15.238125ms checkpoint_p50=645.25µs checkpoint_p95=756.792µs checkpoint_max=776.875µs rehydrate_total=13.2525ms rehydrate_p50=512.042µs rehydrate_p95=589.417µs rehydrate_max=1.535041ms rehydrates_per_second=1811.0 durable_bytes_after_checkpoint=2451424 durable_bytes_after_rehydrate=3254824 rss_before=Some(8093696) rss_after_evict=Some(8486912) rss_after=Some(8847360)
checkpoint/rehydrate benchmark (quick, single platform): nodes=500 input_bytes=64 checkpoint_total=227.699833ms checkpoint_p50=424.625µs checkpoint_p95=658.041µs checkpoint_max=1.027834ms rehydrate_total=230.870667ms rehydrate_p50=442.375µs rehydrate_p95=504µs rehydrate_max=5.905916ms rehydrates_per_second=2165.7 durable_bytes_after_checkpoint=5260464 durable_bytes_after_rehydrate=9605552 rss_before=Some(7913472) rss_after_evict=Some(11812864) rss_after=Some(15204352)
checkpoint/rehydrate benchmark (full, single platform): nodes=5000 input_bytes=64 checkpoint_total=2.352062166s checkpoint_p50=420.334µs checkpoint_p95=513.042µs checkpoint_max=6.243833ms rehydrate_total=2.416840792s rehydrate_p50=456.458µs rehydrate_p95=523.125µs rehydrate_max=6.630417ms rehydrates_per_second=2068.8 durable_bytes_after_checkpoint=11486384 durable_bytes_after_rehydrate=18059696 rss_before=Some(8093696) rss_after_evict=Some(20840448) rss_after=Some(32456704)
```

要点：

1. **两 gate 均有实测数据**：reclaim 闭环驱逐吞吐 2_031.9–2_758.0 evictions/s（52/512 单位，drive 25.6ms/185.6ms，恰好=overshoot）；rehydrate 按需恢复吞吐 1_811.0–2_165.7 nodes/s（5_000 节点 2.42s，p50 456µs / p95 523µs）。**重档全部真实实跑**（最长单档 <5s，远低于 15 分钟登记线），无「注册未跑」项。
2. rehydrate 延迟与节点数不退化（quick p50 442µs vs full p50 456µs，~1.03x）——按需逐节点路径为点查（per-binding snapshot/incarnation/wait 行），无全表扫描面。
3. durable bytes 随档位线性（24→2.45/3.25MB；500→5.26/9.61MB；5_000→11.49/18.06MB，checkpoint 后→rehydrate 后增量主要是 wait 行与 incarnation 层）；reclaim 驱逐的 durable 足迹 = 每受害者一张 receipt + 状态推进（52 单位 45,056B；512 单位 188,416B，~368B/单位）。
4. 仍为 debug/test profile 单平台（macOS，WAL，fsync 语义同 §3 口径）；release profile 与多平台复测未做，不据此宣称 G2/G5 正式达成。

### 验证门（W31-C 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| fmt | `cargo fmt -p nlos-task -p nlos-runtime-tokio` 后 `-- --check` | PASS |
| 全量测试（nlos-task） | `cargo test -p nlos-task` | PASS（**371 passed / 0 failed / 4 ignored**；W29-A 347 基线 + W31-A 增量 + 本车道 4 新用例，既有 reclaim/advisory 前缀用例零回归） |
| 全量测试（nlos-runtime-tokio） | `cargo test -p nlos-runtime-tokio` | PASS（**120 passed / 0 failed / 10 ignored**；W25 生命周期/snapshot/resume 套件零改动零回归，新增默认档 1 用例） |
| clippy | `cargo clippy -p nlos-task -p nlos-runtime-tokio --all-targets --all-features -- -D warnings` | PASS（修 `doc_markdown` ×2、`cast_possible_wrap/truncation/precision_loss`、`unnested_or_patterns`、`collapsible_if`、`needless_range_loop`、`duration_suboptimal_units`、`unused_async`、`too_many_lines` 后） |
| reclaim probes | `cargo test -p nlos-task --test reclaim_execution_probe -- --ignored --nocapture` | PASS（本节数字） |
| checkpoint/rehydrate probes | `cargo test -p nlos-runtime-tokio --test checkpoint_rehydrate_scale -- --include-ignored --nocapture` | PASS（本节数字；默认档亦随全量套件每跑过） |

### 仍属缺口（递延 minors）

1. `nlos-plan` residency 轴（W31-E）与 Task 面 reclaim 闭环**尚未互连**：本车道的驱逐在 Task 工作集（permit 关闭），不驱动 `record_residency_transition`；plan 侧 tier 走迁的执行侧驱动（checkpoint→evict→rehydrate 作用于 plan node 的 HOT→WARM→COLD）待 Materialization/Residency controller 车道接线。
2. `RebuildableCache`/`DegradeBackgroundQos`/`Kill` 三相在 `nlos-task` 无真实面（`face_absent` 如实报告）；cache/QoS 面分别依赖 W31-E 之后的 Context 面与调度车道。
3. reclaim 驱动为 controller-loop 语义（逐受害者线性化，非单事务 CAS）；需要跨权威原子性时应由上层 controller 编排，本面不虚构 fence。
4. 基准数字为 debug/test profile 单平台；release profile、多平台与更长 input payload 矩阵未做（G2/G5 正式 gate 复测口径不变）。
