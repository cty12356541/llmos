# C-BENCH-001 / W66-CBENCH-ROADC002：分片无全局锁扩展基准、`10K_LOGICAL` / `100K_LOGICAL` 多工作集实盘压测与阶段 C 退出门 `[ROAD-C-002]` 五类故障注入验证

- 状态：`PASS`（`Assurance: H6`；基于真实磁盘 `SQLite WAL + synchronous=FULL` 的 `Fast / Streaming / Slow` 工作负载基线、`1 -> 2 -> 4 -> 8` 独立子进程 `Cell` 分片无全局锁/无全局总序 `spawn / message / commit` 扩展、8-`Cell` 分片 `10K_LOGICAL` 与 `100K_LOGICAL` 在 `1% / 5% / 10%` 活跃工作集比例下的全链路实测、`[PERF-CONTROL-001]` 大规模手动控制响应度、以及 `[ROAD-C-002]` 全部五类分布式/隔舱故障注入验证；**单机多进程/多分片域**——不声称 `1M_LOGICAL` 或跨物理主机网卡网络基准）
- 日期：2026-10-26
- Owner：`C-BENCH` / `[ROAD-C-002]` 轨道（`W66-CBENCH-ROADC002` Attempt 1）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §25.2 `[SCALE-PROFILE-001]`、`[SCALE-DEGRADE-001]`；§25.3；§26.1 `[DIST-LOCAL-001]`、`[DIST-TASK-001..004]`、`[DIST-FANOUT-001]`、`[DIST-BULKHEAD-001]`；§27 `[PERF-CLASS-001]`、`[PERF-BENCH-001]`、`[PERF-SCALE-001]`、`[PERF-SCALE-002]`、`[PERF-CONTROL-001]`；§28.3 `[ROAD-C-002]`；§30 `[EVID-SCOPE-001]`、`[EVID-NONLINEAR-001]`、`[EVID-NEG-001]`、`[EVID-REPRO-001]`
  - [ADR-0018：阶段 C 分布式拓扑基线](../../management/adrs/0018-single-host-multiprocess-dual-cell-topology.md)
  - [ADR-0019：跨 Cell 提交语义扩展](../../management/adrs/0019-cross-cell-commit-semantics-extension.md)
- 基线：`8f16d57bc5bd09b41eee6227c9f58b5eafaf446f`

---

## 1. 测试环境与持久性基线（`[PERF-BENCH-001]`、`[EVID-REPRO-001]`）

| 维度 | 实测配置与核验事实 |
|---|---|
| CPU | `AMD Ryzen 5 9600X 6-Core Processor`（`6` 物理核 / `12` 逻辑处理器） |
| 操作系统 | `Microsoft Windows 11 专业版`（`10.0.28000`，`x86_64`） |
| 可见物理内存 | `15,787,384 KB`（`~15.06 GiB`） |
| 权威存储持久性 | 全部权威库（`SqlitePlanAuthority`、`SqliteTaskAuthority`、`ProcessAuthority`、`ChannelAuthority`、`WaitAuthority`、`SqliteOperationStore`、`SqliteLeaseStore`）均落盘至独立临时目录文件，并在测试内通过 `PRAGMA journal_mode` 与 `PRAGMA synchronous` 强制断言 `journal_mode = WAL` 且 `synchronous = FULL (2)`；零 `:memory:`、零 `synchronous = NORMAL` 降级 |
| 代码边界 | 新增 2 个集成基准测试文件与本证据文档；两受测 crate 的 `src/` 生产源码零改动（`Cargo.toml` 仅由集成器补充测试所需 `[dev-dependencies]`） |

新增验证载体：

1. [c_bench_task_sharded_scale.rs](../../../crates/nlos-task/tests/c_bench_task_sharded_scale.rs)：多 OS 子进程 `CellAuthority` + `SqliteLeaseStore` + `SqliteTaskAuthority` + `HierarchicalFanoutTree` + `FanoutUsageLedger` + `BulkheadController` 的 `1 -> 2 -> 4 -> 8` 分片扩展基准，以及 `stale TaskAuthority epoch`、`dual-primary partition`、`fanout storm` 三项任务面并发故障注入测试。
2. [c_bench_road_c002_scale_and_faults.rs](../../../crates/nlos-runtime-tokio/tests/c_bench_road_c002_scale_and_faults.rs)：`[PERF-CLASS-001]` 三类工作负载基准、8-`Cell` 分片 `10K_LOGICAL` 与 `100K_LOGICAL`（`#[ignore]` release 规模探针）跨 `1% / 5% / 10%` 活跃工作集比例全链路压测、`[PERF-CONTROL-001]` 手动控制延迟与有界恢复扫描、以及 `provider stall` 与 `Cell bulkhead` 两项运行时/驱动面并发故障注入测试。

---

## 2. `[PERF-CLASS-001]` 三类工作负载基准实测（`Fast` / `Streaming` / `Slow`）

测试函数：`perf_class_001_fast_streaming_and_slow_workload_classes_benchmark`（`--release --test-threads=1` 隔离实测与 `dev` 对照）：

| Workload Class | 样本规模 | 持久性屏障 (`WAL + FULL`) | 构建档位 | 吞吐量 | `p50` (µs) | `p95` (µs) | `p99` (µs) | `max` (µs) | 关键契约核验 |
|---|---|---|---:|---:|---:|---:|---:|---:|---|
| `Fast` | `256` 次本地调度 + `ExecutionFiber` oneshot 往返 + `join_fiber` 回收 | `0` 次同步落盘屏障 / op | `release` | `38,385.4 ops/s` | `24` | `42` | `95` | `135` | 零残留 `ExecutionFiber`（`registered_fibers() == 0`），微秒级热路径不被慢路径 I/O 污染 |
| `Fast` | 同上 | `0` 次同步落盘屏障 / op | `dev` | `18,317.8 ops/s` | `49` | `83` | `113` | `424` | `Fast p95 (83 µs) << Slow p95 (2,848 µs)` |
| `Streaming`（逐 chunk） | `48` 条流 / 共 `672` 个增量 token chunk（有界 `mpsc(4)`） | `0` 次同步落盘屏障 / chunk | `release` | — | `< 1` (`0`) | `14` | `20` | `28` | 触发 `126` 次真实有界背压挂起；`8/8` 条中途取消流在 chunk 4 精确触发 `cancel_scope` 围栏且零后续泄漏 |
| `Streaming`（整窗闭合） | `48` 条流（`40` 条完成流各含 `1` 次 `ChannelAuthority::enqueue` + `WaitAuthority::notify_commits`） | `1` 次 `WAL + FULL` 窗口屏障 / 完成流 | `release` | `1,623.9 streams/s` | `665` | `935` | `1,026` | `1,026` | 增量消费零每 token `fsync` 放大，窗口闭合具备可恢复持久水位 |
| `Slow` | `48` 次 `MockProvider` + `SqliteOperationStore` 全生命周期 | `5` 次 `WAL + FULL` 屏障 / op（`register -> prepare -> activate -> complete -> ack_outbox`） | `release` | `614.9 ops/s` | `1,616` | `1,965` | `2,101` | `2,101` | 关闭并重开 `SqliteOperationStore` 后 `pending_outbox == 0`，重复 `complete` 命中幂等重放（`replayed == true`） |
| `Slow` | 同上 | `5` 次 `WAL + FULL` 屏障 / op | `dev` | `410.2 ops/s` | `2,290` | `2,848` | `4,813` | `4,813` | 真实 `FlushFileBuffers` 持久性成本显式可见 |

---

## 3. `[ROAD-C-002]` 无全局锁 / 无全局总序多 `Cell` 分片水平扩展（`1 -> 2 -> 4 -> 8` 分片）

测试函数：`road_c002_sharded_spawn_message_commit_scales_without_global_lock_or_total_order`。
架构证明：每个分片运行在**独立 OS 子进程**与**独立磁盘目录**中，拥有各自独立的 `CellAuthority`、本地 `QuotaLease` 预算切片、独立 `SqliteTaskAuthority`（`WAL + FULL`）与本地 `HierarchicalFanoutTree` 增量 Merkle 归约器；跨分片**零共享互斥锁、零全局序号发生器、零全局总序协调**，各分片内每个任务独立完成 `TaskHead` `0 -> 1` 线性化提交（每分片 `32` 次 `spawn` + `32` 次子组消息/Merkle 增量归约 + `32` 次租约绑定 `CommitPermit` + `finalize_commit_v3` = `96` 生命周期操作/分片，另加 `16` 次并发手动控制操作/分片）。

### 3.1 `--release` 实盘分片扩展结果（`6` 物理核 / `12` 逻辑线程宿主机）

| 独立 `Cell` 分片数 | 并发窗口 (`ms`) | 总生命周期操作数 | 聚合吞吐 (`ops/s`) | 相对 `1` 分片加速比 | `spawn` `p50 / p95 / p99` (µs) | `message+merkle` `p50 / p95 / p99` (µs) | `commit` `p50 / p95 / p99` (µs) | 并发手动控制 `p50 / p95 / p99` (µs) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `1` | `96.54` | `96` | `994.4` | `1.00x` | `1,155 / 1,700 / 1,750` | `333 / 587 / 624` | `1,059 / 2,556 / 4,555` | `125 / 451 / 451` |
| `2` | `116.21` | `192` | `1,652.1` | `1.66x` | `1,404 / 1,964 / 3,747` | `348 / 482 / 2,378` | `1,356 / 2,702 / 5,326` | `118 / 460 / 515` |
| `4` | `127.93` | `384` | `3,001.6` | `3.02x` | `1,651 / 2,006 / 2,207` | `464 / 780 / 1,015` | `1,450 / 3,505 / 5,558` | `103 / 580 / 736` |
| `8` | `173.86` | `768` | `4,417.3` | `4.44x` | `2,233 / 2,977 / 3,431` | `642 / 875 / 1,096` | `1,909 / 4,879 / 8,247` | `124 / 825 / 873` |

- 对照：`dev` 构建下同一 `1 -> 2 -> 4 -> 8` 分片测试测得聚合吞吐 `652.2 -> 1,191.6 -> 1,932.2 -> 3,769.9 ops/s`（相对 `1` 分片加速比 `1.00x -> 1.83x -> 2.96x -> 5.78x`）。在 `6` 物理核与单一 NVMe 控制器（8 路并发 `synchronous = FULL` 写屏障）硬件约束下，吞吐随分片数单调近似线性增长，证实不存在全局锁或全局总序串行瓶颈。
- 层级归约局部性（`[DIST-TASK-003]`）：每个分片树（`32` 叶子、`4` 子组）每次增量 `report_leaf_candidate` 仅重算受影响叶子至根路径上的 `2` 个子组节点（远小于全量重算的 `49` 次哈希折叠），且叶子到根 `verify_merkle_proof` 100% 验证通过。

---

## 4. `[PERF-SCALE-001]`、`[PERF-SCALE-002]` 与 `[SCALE-PROFILE-001]`：8-`Cell` 分片 `10K_LOGICAL` 与 `100K_LOGICAL` 规模实测

测试函数：
- `perf_scale_001_002_sharded_10k_logical_multi_working_set_benchmark`
- `perf_scale_001_002_sharded_100k_logical_multi_working_set_benchmark`

拓扑配置：`8` 个独立 `Cell` 分片，`ThreeTierScheduler`（`1 Global -> 8 Cells -> 16 Workers`，每 `Cell` 2 个 `WorkerSchedulerDomain` 执行真实同 `Cell` 跨 `Worker` 原子工作窃取），M:N `TokioRuntimeAdapter` 固定绑定 **`8` 个宿主工作线程**（活跃协程批次上限 `2,048`），严格区分**逻辑声明节点（cold metadata）**、**已物化节点（materialized）**、**可运行队列峰值（runnable）**、**活跃 `ExecutionFiber` 峰值（active fibers）**与**宿主进程常驻内存（`WorkingSet64`）**。

### 4.1 逻辑节点登记、DAG 依赖解析、磁盘元数据密度与恢复扫描基线（`--release --test-threads=1` 隔离实测）

| 规模档位 (`ScaleProfile`) | 分片布局 | 登记总耗时 (`ms`) / 吞吐 (`nodes/s`) | 分片 `apply_plan_revision` `p50 / p95` (µs) | 分片 DAG `resolve_plan` `p50 / p95` (µs) | 登记期每节点持久化字节 (`.db + -wal`) | 关闭归并后每节点持久化字节 (`.db`) | 8 分片全套权威库总磁盘字节 | 8 分片重启恢复扫描总墙钟 (`ms`) / 分片 `p95` (µs) | 进程 `WorkingSet64`（初始 -> 声明后 -> 关闭后） |
|---|---|---|---|---|---:|---:|---:|---|---|
| `10K_LOGICAL` | `8 × 1,250 = 10,000` | `3,107.17 ms` / `3,218.4`（并发跑 `4,311.5`） | `55,087 / 57,224` | `4,597 / 5,562` | `770 B/node` (`7.70 MB`) | `766 B/node` (`7.66 MB`) | `18,214,912 B` (`17.37 MiB`) | `33.74 ms` / `27,270 µs` | `12.34 -> 44.81 -> 14.69 MiB` |
| `100K_LOGICAL` | `8 × 12,500 = 100,000` | `1,887.95 ms` / `52,967.5` | `642,921 / 646,534` | `47,841 / 50,595` | `1,041 B/node` (`104.19 MB`) | `634 B/node` (`63.39 MB`) | `74,010,624 B` (`70.58 MiB`) | `38.07 ms` / `29,482 µs` | `6.81 -> 54.46 -> 17.63 MiB` |

### 4.2 多活跃工作集比例（`1%` / `5%` / `10%`）分层计数与调度/执行延迟表（`[SCALE-PROFILE-001]`）

| 档位 | 工作集比例 | `logical` 总数 | `unmaterialized` (仅冷元数据) | `materialized` (活跃窗口) | `runnable` 队列峰值 | `active_fibers` 峰值 (`host_threads=8`) | 跨 `Worker` 窃取成功数 | 增量物化数 / 墙钟 (`ms`) | `ThreeTierScheduler` 派发墙钟 (`ms`) / 入队 `p99` (µs) | M:N `Fiber` 执行墙钟 (`ms`) / 端到端 `p50 / p95 / p99` (µs) | 状态迁移吞吐 (`transitions/s`) | 宿主进程常驻 `WorkingSet64` (`MiB`) |
|---|---:|---:|---:|---:|---:|---:|---:|---|---|---|---:|---:|
| `10K_LOGICAL` | `1%` | `10,000` | `9,900` | `100` | `100` | `100` | `24` | `+100` / `76.19 ms` | `0.05 ms` / `1 µs` | `0.24 ms` / `161 / 193 / 195` | `5,230.5` | `49.01` |
| `10K_LOGICAL` | `5%` | `10,000` | `9,500` | `500` | `500` | `500` | `120` | `+400` / `147.34 ms` | `0.25 ms` / `<1 µs` | `0.62 ms` / `295 / 443 / 454` | `12,144.8` | `47.48` |
| `10K_LOGICAL` | `10%` | `10,000` | `9,000` | `1,000` | `1,000` | `1,000` | `248` | `+500` / `157.75 ms` | `0.72 ms` / `1 µs` | `1.52 ms` / `784 / 1,194 / 1,237` | `18,751.5` | `48.30` |
| `100K_LOGICAL` | `1%` | `100,000` | `99,000` | `1,000` | `1,000` | `1,000` | `248` | `+1,000` / `952.88 ms` | `0.80 ms` / `3 µs` | `1.33 ms` / `706 / 982 / 1,014` | `4,188.4` | `64.19` |
| `100K_LOGICAL` | `5%` | `100,000` | `95,000` | `5,000` | `5,000` | `2,048`（累计派发 `5,000`） | `1,248` | `+4,000` / `2,322.26 ms` (注 1) | `12.90–15.47 ms` / `<1 µs` | `7.01–7.40 ms` / `1,449 / 2,551 / 2,738` | `7,685.2` (`924.8` 注 1) | `67.51` |
| `100K_LOGICAL` | `10%` | `100,000` | `90,000` | `10,000` | `10,000` | `2,048`（累计派发 `10,000`） | `2,496` | `+5,000` / `2,292.08 ms` (注 1) | `46.70–47.74 ms` / `<1 µs` | `13.23–13.70 ms` / `1,320 / 2,104 / 2,435` | `12,746.9` (`1,230.7` 注 1) | `69.97` |

> **注 1（诚实波动记录，`[EVID-NEG-001]`）**：在无外部进程磁盘争用的 `--release` 运行中，`100K_LOGICAL` 的 `5%`（`+4,000` 节点）与 `10%`（`+5,000` 节点）持久化物化耗时分别为 `2,322.26 ms`（`7,685.2 transitions/s`）与 `2,292.08 ms`（`12,746.9 transitions/s`）；当与并行工作区其他构建任务争用同一 NVMe `FlushFileBuffers` 写缓存时，单次运行曾测得 `19,441.73 ms`（`924.8 transitions/s`）与 `24,316.59 ms`（`1,230.7 transitions/s`），而内存态 `ThreeTierScheduler`（`15.47 ms` / `46.70 ms`）与 M:N `Fiber` 调度（`7.40 ms` / `13.23 ms`）完全不受影响。两类数据均完整保留备查。

### 4.3 Checkpoint / Evict / Rehydrate 与 Channel 订阅唤醒实测（`[PERF-SCALE-001]`）

涵盖完整 `ProcessAuthority` `snapshot_handler_entry` + `PlanAuthority` `Hot -> Warm -> Cold` 驱逐 + 运行时丢弃 + 新一代 `FiberIncarnation` 注册 + `Cold -> Warm -> Hot` `resume_from_snapshot` 重水合 + `ChannelAuthority::enqueue` / `WaitAuthority::notify_commits` / `TokioChannelWakeSink::deliver` 唤醒闭合：

| 档位 | 驱逐/重水合节点数 | Checkpoint + Cold 驱逐 `p50 / p95 / p99` (µs) | Rehydrate + Wait 重挂 `p50 / p95 / p99` (µs) | Channel 提交匹配与唤醒 `p50 / p95 / p99` (µs) |
|---|---:|---:|---:|---:|
| `10K_LOGICAL` | `64` (`8 × 8`) | `3,346 / 4,163 / 18,757` | `1,438 / 1,857 / 2,014` | `86 / 123 / 123` |
| `100K_LOGICAL`（常规跑 / NVMe 争用跑） | `128` (`8 × 16`) | `3,320 / 6,702 / 8,149`（争用跑 `p50=3,257, p99=205,036`） | `1,498 / 2,098 / 5,890`（争用跑 `p50=1,644, p99=220,385`） | `51 / 79 / 79`（争用跑 `p50=73, p99=10,053`） |

---

## 5. `[ROAD-C-002]` 五大故障注入场景并发验证矩阵

| 故障类目 (`[ROAD-C-002]`) | 具名测试函数 | 故障注入与并发负载 | 核心不变量与实测指标 | 结果 |
|---|---|---|---|---|
| 1. `stale TaskAuthority epoch` | `road_c002_fault_stale_task_authority_epoch_fences_old_term_workers_under_load` | 4 个并行分片满载提交期间，Shard 0 租约推进至 `Term 2`；旧 `Term 1` worker 持接管前已签发 permit 尝试 `finalize_commit_v3`，并继续发起 `15` 次旧 term `request_commit_permit_with_authority_lease` | `16/16` 次旧 term 操作全部以 `TaskStoreError::AuthorityLeaseFenced` 拒绝（零静默提交）；Shard 0 的 `16` 个新 `Term 2` 任务与 Shards `1..3` 的 `48` 个健康任务（共 `64/64`）100% 提交成功，`release` 下健康提交 `p50 = 3,119 µs, p95 = 6,903 µs, p99 = 7,836 µs` | `PASS` |
| 2. `分区双主 (dual-primary partition)` | `road_c002_fault_dual_primary_partition_arbitrates_single_canonical_winner` | `PartitionAutonomyGate` 处于 `ControlPlaneLinkPhase::Partitioned`；验证未过期租约不可被抢先接管（`AuthorityLeaseHeld`），随后两个竞争主节点对 `24` 个任务在并发屏障下同时发起竞争 `CommitPermit` + `finalize_commit_v3` | 本地已授权 `QuotaLease` 扣减放行（`verify_digest() == true`），跨 `Cell` 未授权 canonical 发布被 `CrossCellCanonicalPublishWithoutLocalAuthority` 围栏拒绝；`24` 个竞争任务产生精确 `24` 个唯一赢家与 `24` 个被围栏输家（`Superseded` / `StaleTaskHead`），所有任务最终 `head_commit_seq == 1`，**双主脑裂数为 `0`** | `PASS` |
| 3. `fanout storm` | `road_c002_fault_fanout_storm_triggers_waiting_resource_backpressure_and_preserves_siblings` | 恶意/失控递归组在 Cell 0 上并发喷发 `48` 个超额预留与超深（`depth > max_group_depth`）spawn 请求，并尝试绕过内存账本直接向 `SqliteTaskAuthority` 超上限塞入子任务；同时健康兄弟组与 Cell 1 并发执行 `24` 个正常提交 | 仅精确放行预算内 `4` 个预留，`16` 个转入结构化 `WaitingResourceBackpressure`（报告 `ActiveAgents` / `MessageBytes` 饱和轴），`20` 个超出等待队列上限以 `BackpressureQueueFull` 丢弃，`8` 个超深请求以 `MaxGroupDepthExceeded` 硬拒绝，存储层旁路写入被 `GroupFanoutExceeded` 拦截；`24/24` 健康兄弟提交零受阻，`SafetyTcbSystemReserve` `4/4` 槽位 100% 完好 | `PASS` |
| 4. `provider stall` | `road_c002_fault_provider_stall_isolates_bulkhead_fences_timeout_and_preserves_fast_path` | `provider_stalled` 注入挂起与 `ProviderFaultMode::FailProviderRpc`（并发上限 `4`，队列上限 `4`），突发打入 `16` 个慢调用；挂起期间并发执行 `128` 个 `Fast` 协程与 `24` 个 `provider_healthy` 持久化慢操作；随后触发取消/超时围栏并注入迟到完成回调 | `provider_stalled` 精确隔离为 `4 active + 4 queued + 8 RejectedQueueFull`；`Fast` 路径 `release` 下 `p50 = 7 µs, p95 = 20 µs, p99 = 31 µs`，`provider_healthy` `24/24` 正常完成（`p50 = 1,372 µs, p95 = 1,952 µs`）；取消围栏后全部 `4/4` 个迟到回调被路由至 `OutboxKind::ReconcileEffect`（绝不误唤醒已取消协程），残留泄漏协程数为 **`0`**，`SafetyTcb` `4/4` 槽位完好 | `PASS` |
| 5. `Cell bulkhead` | `road_c002_fault_cell_bulkhead_preserves_safety_tcb_drr_fairness_and_offloads_healthy_cells` | Cell 0 上 `group_flood` 遭遇 `200` 次突发洪泛；同 `Cell` 兄弟 `group_fair`（权重 `3`）与 `group_flood`（权重 `1`）同优先级竞争 `Worker`；随后 Cell 0 标记 `SaturationSource::ModelSlot` 饱和并向 `Global` 发起 `64` 次首选 Cell 0 的跨 `Cell` 放置 | `group_flood` 被精确限制在 `4 acquired + 4 queued + 192 shed`；`SafetyTcbSystemReserve` 全部 **`6/6` 个保留槽位立即可获**；同 `Cell` `Worker` DRR 调度前 `12` 次出队精确呈现 **`9 : 3`**（符合 `3:1` 权重，零饥饿）；反向背压将 `Global` 物化窗口从 `128` 收缩至 `116`，且 **`64/64` 次跨 `Cell` 放置 100% 自动卸载至健康 `Cells 1..7`** | `PASS` |

---

## 6. `[PERF-CONTROL-001]` 大规模与高并发下的手动控制面响应度

所有手动控制命令均直接作用于主键索引的权威控制记录（`inspect_node`、`inspect_node_residency`、`cancel_group`、`cancel_task`、`inspect_working_set_pressure` + `drive_working_set_reclaim` + 有界 `pending_outbox(8)` 探针），**零全量 Agent 枚举阻塞**：

| 负载场景 | 采样操作数 | `p50` (µs) | `p95` (µs) | `p99` (µs) | `max` (µs) |
|---|---:|---:|---:|---:|---:|
| 8-`Cell` 并发 `spawn/message/commit` 满载 (`release`) | `128` | `124` | `825` | `873` | `901` |
| `10K_LOGICAL` 8-`Cell` 分片负载 (`release`) | `80` | `93` | `448` | `542` | `542` |
| `100K_LOGICAL` 8-`Cell` 分片负载（`release` 常规跑） | `128` | `85` | `419` | `484` | `499` |
| `100K_LOGICAL` 8-`Cell` 分片负载（`release` NVMe 写缓存争用跑） | `128` | `84` | `450` | `155,285` | `226,489` |

即使在 `100,000` 个逻辑节点已落盘且存在并发磁盘 `FlushFileBuffers` 尖峰的场景下，手动控制面中位数延迟仍保持 **`84–124 µs`**，`p95` 保持 **`< 1 ms`（`419–825 µs`）**，最坏单次同步刷盘尖峰亦保持在亚秒级交互预算内（`< 230 ms`）。

---

## 7. 显式复杂度暴露、边界与诚实非声明（`[PERF-SCALE-002]`、`[EVID-NEG-001]`、`[EVID-SCOPE-001]`）

1. **显式暴露的分片内扫描与 `synchronous = FULL` 尾延迟事实（`[PERF-SCALE-002]`、`[EVID-NEG-001]`）**：
   - `nlos-plan` 当前的 `MaterializationScheduler::select` 在单个 `SqlitePlanAuthority` 分片内部需扫描该分片的 `plan_nodes` 表以筛选 `Eligible / Declared` 节点，复杂度为按分片的 `O(N / S)`（8 分片 `100K_LOGICAL` 下每分片 `12,500` 行，单次增量物化轮耗时约 `0.95–2.32 s`）。通过 `Cell` 分片水平切分避免了单库 `O(N)` 全局大锁，但未来若推进单分片百万节点档位，需在 `nlos-plan` 内引入就绪队列二级索引以将 `select` 降至严格 `O(W)`（窗口大小阶）。
   - Windows NTFS + NVMe 下 `PRAGMA synchronous = FULL` 触发真实硬件写缓存屏障（`FlushFileBuffers`）：当宿主存在并行进程重度磁盘写入时，单次事务 `p99/max` 可从常规 `2–8 ms` 跳升至 `60–226 ms`，本证据未通过关闭 `FULL` 同步或剔除异常点来掩盖该物理持久性尾延迟。
2. **`ScaleProfile` 声称边界（`[SCALE-PROFILE-001]`）**：
   - 本证据正式验证并支撑 8-`Cell` 分片拓扑下的 **`10K_LOGICAL`** 与 **`100K_LOGICAL`**（活跃工作集 `1% / 5% / 10%`，即最高 `10,000` 已物化节点、`2,048` 并发活跃 `ExecutionFiber`、`8` 宿主工作线程）；
   - **`1M_LOGICAL` 未在当前 `6C/12T, 15.06 GiB RAM` 单机工作站上声称**，留待更高内存/多机硬件档位及 `O(W)` 物化索引优化后单独取证；绝不用 `100K` 冷元数据宣称“同时运行十万个大模型推理”。
3. **分布式拓扑域边界（[ADR-0018](../../management/adrs/0018-single-host-multiprocess-dual-cell-topology.md)）**：
   - 多 `Cell` 扩展与分区/接管故障注入运行在单机多 OS 子进程与独立磁盘目录 `SQLite` 拓扑上，不冒充跨物理主机真实网卡 RTT、交换机丢包或外部商业 LLM API 端到端网络延迟。

---

## 8. 复现命令与核验结果（`[EVID-REPRO-001]`）

```powershell
# 1. 格式与严格 Clippy 门禁（workspace 启用 unsafe_code = forbid, clippy::all + clippy::pedantic）
cargo fmt -p nlos-task -p nlos-runtime-tokio -- --check
# -> 退出码 0

cargo clippy -p nlos-task -p nlos-runtime-tokio --all-targets -- -D warnings
# -> 退出码 0（零警告）

# 2. 默认套件快速回归（含 1->2->4->8 多进程分片扩展、10K_LOGICAL 1%/5%/10%、PERF-CLASS-001 与全部 5 类 ROAD-C-002 故障注入）
cargo test -p nlos-task --test c_bench_task_sharded_scale -- --nocapture
# -> 退出码 0（4 passed; 0 failed; 1 ignored）

cargo test -p nlos-runtime-tokio --test c_bench_road_c002_scale_and_faults -- --nocapture
# -> 退出码 0（4 passed; 0 failed; 1 ignored）

# 3. Release 全量实盘基准（含 100K_LOGICAL 8-Cell 分片 1%/5%/10% 规模压测，单线程隔离采样）
cargo test -p nlos-task --release --test c_bench_task_sharded_scale -- --test-threads=1 --nocapture
# -> 退出码 0（4 passed; 0 failed; 1 ignored）

cargo test -p nlos-runtime-tokio --release --test c_bench_road_c002_scale_and_faults -- --include-ignored --test-threads=1 --nocapture
# -> 退出码 0（5 passed; 0 failed; 0 ignored）
```
