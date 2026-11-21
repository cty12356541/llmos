# C-SCHED-002 / W82-R4-IND：`plan_nodes_state_order` 覆盖索引与有界 `select_bounded` 零 Temp B-Tree 归并调度扩展证据

- 状态：`PASS-WITH-BOUNDARIES`
- 等级：`H4`（`H8 = 0`；单机确定性持久化集成验证与 `--release` `10K`/`100K` 对照探针）
- 日期：`2026-11-22`
- Owner：`DEL-CAP-SCHED-INDEX`（Wave `W82-R4-IND`）
- 基线：`05f967451`（`05f967451feaab9f99f84e397445ec3bffe03a6c`）
- 关联设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §25.2.2 `[SCALE-MATERIALIZE-001]` / `[SCHED-BACKPRESSURE-001]`、§27 `[PERF-SCALE-002]`、§30 `[EVID-SCOPE-001]` / `[EVID-REPRO-001]`；[c-bench-001-road-c002-distributed-benchmark.md](./c-bench-001-road-c002-distributed-benchmark.md) §7.1（`select` $O(N/S)$ 分片扫描显式暴露项）

---

## 1. 变更与实现事实

1. **零模式版本升级的幂等覆盖索引（`crates/nlos-plan/src/store.rs`）**：
   - `SCHEMA_VERSION = 9` 与 `PRAGMA user_version = 9` **100% 保持不变**，现有 `migrate_v1..v9` 线性迁移链零改动。
   - 在 `SqlitePlanAuthority::open_with_vfs` 的版本匹配迁移之后幂等执行：
     ```sql
     CREATE INDEX IF NOT EXISTS plan_nodes_state_order
         ON plan_nodes(plan_id, node_state, first_declared_at_ms, task_node_id);
     ```
2. **有界索引归并挑选路径（`crates/nlos-plan/src/scheduler.rs`、`crates/nlos-plan/src/lib.rs`）**：
   - 完整保留既有 `MaterializationScheduler::select` 与 `MaterializationScheduler::run_pass` 的 100% 原语义（含全量 `NotAwaitingMaterialization` / `WindowExhausted` `skips` 诊断枚举）。
   - 新增 `MaterializationScheduler::select_bounded` 与 `MaterializationScheduler::run_pass_bounded`：
     - `BOUNDED_BAND_SEATS_SQL` 通过 `plan_nodes_state_order` 覆盖索引统计 `node_state IN (6, 7, 8, 10)`（`MATERIALIZING` / `ACTIVE` / `CHECKPOINTED` / `REHYDRATING`）占位；
     - 查取 `plan_materialization_requests WHERE plan_id = ?1 AND status = 1` 在途请求；
     - `budget == 0` 时直接跳过游标扫描，仅对剩余 `pending` 节点按 `(plan_id, task_node_id)` 主键定点核查并在 Rust 中按 `(first_declared_at_ms, task_node_id)` 排序追加可采纳项；
     - `budget > 0` 时对精确匹配 `state_can_await_materialization` 的 6 个状态码 `AWAITING_MATERIALIZATION_STATES = [1, 2, 3, 4, 5, 10]`（`Declared`、`BlockedDependency`、`Eligible`、`WaitingAuthorization`、`WaitingResource`、`Rehydrating`）各自开启单值游标 `BOUNDED_AWAITING_CURSOR_SQL`（固定 `plan_id = ?1 AND node_state = ?2`，按 `first_declared_at_ms, task_node_id` 升序流式读取），在 Rust 侧执行 6 路有序归并（避免 SQLite 对 `IN (...) + ORDER BY` 生成 `TEMP B-TREE FOR ORDER BY`）；
     - 一旦 `budget == 0` 立即早停游标扫描，并将未覆盖的剩余 `pending` 节点通过主键定点核查补齐。`skips` 仅记录有界扫描与 pending 核查期间观察到的跳过项（全量跳过清单仍由 `select()` 提供）。
3. **验证与规模探针（`crates/nlos-plan/tests/scheduler_scale_probe.rs`）**：
   - 保留既有 `scheduler_scale_smoke_selects_and_drives_ready_fifo` 与 `ten_thousand_nodes_scheduler_select_and_drive`；
   - 新增常规测试：`explain_query_plan_uses_covering_index_without_temp_btree`、`differential_equivalence_across_complex_plan_topologies`（覆盖全部 13 种 `PlanNodeState`、跨 revision `first_declared_at_ms`、pending 挂起/取消/Rehydrating、DAG 阻塞、`PINNED` 叠加及 `window` 全区间与 `run_pass_bounded`）、`differential_equivalence_all_blocked_and_completed`、`fast_scale_regression_five_thousand_tail_bounded_early_stop`；
   - 新增 `#[ignore]` 门控 `--release` 对照探针：`scale_probe_10k_select_vs_bounded`、`scale_probe_100k_select_vs_bounded`。

---

## 2. `EXPLAIN QUERY PLAN` 原文核验

由 `explain_query_plan_uses_covering_index_without_temp_btree` 在真实落盘库（`WAL + synchronous = FULL (2)`，`user_version = 9`）上直接采集：

```text
EQP cursor (node_state=1): SEARCH plan_nodes USING INDEX plan_nodes_state_order (plan_id=? AND node_state=?)
EQP band_seats: SEARCH plan_nodes USING COVERING INDEX plan_nodes_state_order (plan_id=? AND node_state=?)
EQP single-IN contrast: SEARCH plan_nodes USING INDEX plan_nodes_state_order (plan_id=? AND node_state=?) | USE TEMP B-TREE FOR ORDER BY
EQP legacy select: SEARCH plan_nodes USING INDEX sqlite_autoindex_plan_nodes_2 (plan_id=?) | USE TEMP B-TREE FOR ORDER BY
```

核验结论：
- 全部 6 个待物化状态游标（`node_state IN [1, 2, 3, 4, 5, 10]`）均稳定命中 `SEARCH plan_nodes USING INDEX plan_nodes_state_order (plan_id=? AND node_state=?)`，**零 `SCAN plan_nodes`、零 `USE TEMP B-TREE FOR ORDER BY`**；
- `band_seats` 聚合查询命中 `SEARCH plan_nodes USING COVERING INDEX plan_nodes_state_order (plan_id=? AND node_state=?)`；
- 对照实验证实：若直接在 SQL 中写 `node_state IN (1, 2, 3, 4, 5, 10) ORDER BY first_declared_at_ms, task_node_id`，SQLite 仍会触发 `USE TEMP B-TREE FOR ORDER BY`——6 路单值游标 + Rust 归并彻底消除了该 Temp B-Tree 排序。

---

## 3. `--release` `select` vs `select_bounded` 实测对比延迟表

### 3.1 测试环境（单机 macOS）

| 维度 | 实测配置 |
|---|---|
| 宿主机 | Apple Silicon (`Apple M5`, `10` 物理核 / `10` 逻辑核, `16 GiB` RAM) |
| 操作系统 | `macOS 26.6.2` (`Build 25G83`) |
| Rust 工具链 | `rustc 1.99.0 (b940084d7 2026-09-28)` |
| SQLite | `rusqlite 0.40.2` / `libsqlite3-sys 0.38.2`（bundled） |
| 持久性策略 | 独立磁盘目录 `plan.sqlite3`，强制断言 `journal_mode = WAL`、`synchronous = FULL (2)`、`user_version = 9`、`PRAGMA integrity_check = ok` |
| 采样口径 | 每格 `2` 次预热 + `25` 次交替测量（`select` 与 `select_bounded` 同池对比），每次均先校验 `selections` 与 `seats_in_use` **100% 全等**；夹具包含 `20%` 活跃 band（`6/7/8/10`）、`80%` 待处理（`1/2/3/5`）、真实 `PINNED` 叠加及 `PENDING` 门控轮，窗口设为 `band_seats + pending_seats + 64`（每次挑选 `68` 项） |

### 3.2 命令 A：并行默认线程（`cargo test --release -p nlos-plan --test scheduler_scale_probe -- --include-ignored --nocapture`）

| 节点规模 | 待处理/活跃比例 | `window` / `selected` | `select` `p50` (µs) | `select` `p95` (µs) | `select_bounded` `p50` (µs) | `select_bounded` `p95` (µs) | `p50` 加速比 | `p95` 加速比 | 语义一致性 |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `10,000` | `1%` | `88 / 68` | `6,216` | `7,186` | `213` | `347` | `29.18x` | `20.71x` | `selections` / `seats_in_use` 全等 |
| `10,000` | `5%` | `168 / 68` | `12,027` | `13,239` | `292` | `809` | `41.19x` | `16.36x` | `selections` / `seats_in_use` 全等 |
| `10,000` | `50%` | `1068 / 68` | `70,163` | `128,232` | `751` | `1,182` | `93.43x` | `108.49x` | `selections` / `seats_in_use` 全等 |
| `100,000` | `1%` | `268 / 68` | `107,297` | `114,693` | `496` | `655` | `216.32x` | `175.10x` | `selections` / `seats_in_use` 全等 |
| `100,000` | `5%` | `1068 / 68` | `116,137` | `134,076` | `329` | `516` | `353.00x` | `259.84x` | `selections` / `seats_in_use` 全等 |
| `100,000` | `50%` | `10068 / 68` | `296,197` | `327,638` | `395` | `577` | `749.87x` | `567.83x` | `selections` / `seats_in_use` 全等 |

> 注：默认并发执行时 `ten_thousand_nodes_scheduler_select_and_drive`（`10,000` 节点全量 `drive()` 触发 ~`20,000` 次 `WAL+FULL` 刷盘：`apply_total=315.74875ms`, `select_total=137.114959ms`, `drive_total=14.868273334s`）与对照探针并行运行，产生可观磁盘/CPU 竞争；下表 3.3 给出单线程隔离跑测数据。

### 3.3 命令 B：单线程隔离采样（`cargo test --release -p nlos-plan --test scheduler_scale_probe -- --include-ignored --test-threads=1 --nocapture`）

| 节点规模 | 待处理/活跃比例 | `window` / `selected` | `select` `p50` (µs) | `select` `p95` (µs) | `select_bounded` `p50` (µs) | `select_bounded` `p95` (µs) | `p50` 加速比 | `p95` 加速比 | 语义一致性 |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `10,000` | `1%` | `88 / 68` | `5,900` | `6,234` | `93` | `103` | `63.44x` | `60.52x` | `selections` / `seats_in_use` 全等 |
| `10,000` | `5%` | `168 / 68` | `7,742` | `8,325` | `104` | `127` | `74.44x` | `65.55x` | `selections` / `seats_in_use` 全等 |
| `10,000` | `50%` | `1068 / 68` | `27,694` | `51,742` | `162` | `197` | `170.95x` | `262.65x` | `selections` / `seats_in_use` 全等 |
| `100,000` | `1%` | `268 / 68` | `84,410` | `96,394` | `163` | `182` | `517.85x` | `529.64x` | `selections` / `seats_in_use` 全等 |
| `100,000` | `5%` | `1068 / 68` | `86,961` | `91,710` | `164` | `189` | `530.25x` | `485.24x` | `selections` / `seats_in_use` 全等 |
| `100,000` | `50%` | `10068 / 68` | `358,165` | `764,645` | `481` | `997` | `744.63x` | `766.95x` | `selections` / `seats_in_use` 全等 |

在 `100,000` 逻辑节点规模下，`select_bounded` 在单线程隔离跑测中将选择延迟从 `84.4–358.2 ms`（`p95` `91.7–764.6 ms`）降至 **`163–481 µs`**（`p95` **`182–997 µs`**），达成 **`517.85x–744.63x`（`p50`）** 与 **`485.24x–766.95x`（`p95`）** 的实测加速。

---

## 4. 全量门禁与回归结果

```text
cargo fmt -p nlos-plan -- --check                                                              -> 退出码 0
cargo clippy -p nlos-plan --all-targets -- -D warnings                                         -> 退出码 0（零警告）
cargo test -p nlos-plan                                                                        -> 21 个测试目标（1 unit + 20 integration）：125 passed, 0 failed, 5 ignored
cargo test --release -p nlos-plan --test scheduler_scale_probe -- --include-ignored --nocapture -> 8 passed, 0 failed, 0 ignored（26.62s）
cargo test --release -p nlos-plan --test scheduler_scale_probe -- --include-ignored --test-threads=1 --nocapture -> 8 passed, 0 failed, 0 ignored（44.33s）
```

含 `plan_fault_matrix`（5 passed）、`materialization_fault_matrix`（5 passed）、`pinned`（4 passed）、`residency`（10 passed）、`ecosystem_selector_gate`（7 passed）、`schema_edge_declaration`（7 passed）、`scheduler`（9 passed）及新增 `scheduler_scale_probe` 常规测试（5 passed）100% 通过。

---

## 5. 诚实边界与残留说明（`H4`，`H8 = 0`）

1. **单机 macOS 平台边界**：上述数据全部测自单台 `macOS 26.6.2`（`Apple M5`）开发机；**Linux 与 Windows 8-`Cell` 多进程分布式基准、`1M_LOGICAL` 档位与夜间 CI 回归门禁保留开放**，不在本单卡 `H4` 证据声明范围内。
2. **生产声明等级（`H8 = 0`）**：本证据不构成全系统端到端生产就绪（`H8`）或跨物理机集群调度延迟证明。
3. **语义与夹具边界**：
   - `select_bounded` 为有界挑选路径，仅记录扫描与 pending 核查期间观察到的跳过项，不枚举未访问行的 `NotAwaitingMaterialization` / `WindowExhausted`；完整诊断仍使用原 `select()`（原 `select()` 与 `run_pass()` 语义 100% 未改）；
   - `10K`/`100K` 规模对照夹具通过公开 `apply_plan_revision_ungated` 声明全量节点，并在单个 `WAL+FULL` 事务中批量写入历史终端/活跃状态后以公开 API 写入 `PINNED` 叠加与 `PENDING` 门控请求，用以隔离测度 `select` 与 `select_bounded` 只读查询路径本身的扩展性，不冒充 `100,000` 次顺序独立 fsync 写入吞吐。
