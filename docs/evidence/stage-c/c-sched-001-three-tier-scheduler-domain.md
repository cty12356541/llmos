# C-SCHED-001 / W64-CSCHED：三层 `Global -> Cell -> Worker` `SchedulerDomain`、有界 WFQ/老化队列、安全工作窃取、`Context` 亲和放置与跨 `Cell` 反向背压

- 状态：`PASS`（`[SCHED-HIER-001]` 三层本地热路径调度、`[SCHED-QUEUE-001]` 有界 `ResourceGroup` WFQ/DRR + 饥饿老化 + 取消纪元围栏 + 队列自省、`[SCHED-STEAL-001]` 零复制原子工作窃取与安全门禁、`Context` 热缓存亲和路由、以及 `[SCHED-BACKPRESSURE-001]` `Worker -> Cell -> Global` 反向背压传播与跨 `Cell` 负载卸载的库级与 Tokio 多线程集成证据；**非**跨物理机 RPC 调度传输或 `ScaleProfile` 百万级压测声明）
- 日期：2026-10-25
- Owner：`C-SCHED` 轨道（`W64-CSCHED` Attempt 1）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §25.2.2 `[SCHED-HIER-001]`、`[SCHED-QUEUE-001]`、`[SCHED-STEAL-001]`、`[SCHED-BACKPRESSURE-001]`；§25.3 `[CTRL-PRIORITY-001]`
- 基线：`3a91098a7557a987128b29a4eaf8490d1277a65b`

## 1. 实现事实

- `crates/nlos-runtime/src/scheduler_domain.rs`（并在 `crates/nlos-runtime/src/lib.rs` 公开导出，同时保持 `FiberSpec::scheduler_domain_id` 对 `nlos_types::SchedulerDomainId` 的零破坏兼容）：
  - **三层分层身份与血统契约（`[SCHED-HIER-001]`）**：定义 `SchedulerTier::{Global, Cell, Worker}` 与 `SchedulerDomainId::{Global { domain_id }, Cell { global_id, cell_id }, Worker { cell_id, worker_id }}`，提供 `tier()`、`belongs_to_cell()`、`belongs_to_global()`、`belongs_to_global_via()` 与 `leaf_nominal_id()`；
  - **有界 `ResourceGroup` 队列与自省元数据契约（`[SCHED-QUEUE-001]`）**：定义 `DeadlineClass::{Immediate, Interactive, Standard, BackgroundBatch}`、`ThrottleReason`、`ResourceGroupQueuePolicy`（含 `max_priority_cap` 截断与 `starvation_aging_step_ms` 饥饿老化推进算法）以及完整公开 `requested_priority`、`effective_priority`、`deadline_class`、`weight`、`queue_position_range`、`throttle_reason`、`starvation_age_ms`、`resource_group_id`、`cancel_epoch` 的 `QueueItemInspection`；
  - **跨 `Worker` 窃取资格与亲和性契约（`[SCHED-STEAL-001]`）**：定义 `ContextAffinitySpec`、`WorkStealEnvelope`、`WorkerStealCapabilities`、`StealRejectionReason`、`StealEligibility` 及纯确定性校验函数 `evaluate_steal_eligibility(item, target_worker_capabilities)`，严格拒绝 `migratable == false`、被围栏或未授权 `IsolationDomain`、不匹配 `Namespace` / `secret_scope_id` / `provider_tenant_id` / `device_affinity` / `TaskAuthority` 任期，并**绝对禁止**窃取或复制持有活跃 `EffectPermit`、`Reservation` 或 `ExclusiveDeviceLease` 的工作项；
  - **反向背压信号与接纳指令契约（`[SCHED-BACKPRESSURE-001]`）**：定义 `DomainPressureLevel::{Nominal, Elevated, Saturated, Critical}`、`SaturationSource::{ModelSlot, TopicBacklog, ContextResidency, StorageIo, DriverPool}`、`AdmissionDirective::{Accept, ThrottleBackground, WaitingResource}`、`BackpressureSignal`（含单调纪元与 `recommended_materialization_window_limit` 收缩计算）以及 `SchedulerAdmission`。

- `crates/nlos-runtime-tokio/src/distributed_scheduler.rs`（并在 `crates/nlos-runtime-tokio/src/lib.rs` 公开导出）：
  - **`ThreeTierScheduler`（`GlobalSchedulerDomain` / `CellSchedulerDomain` / `WorkerSchedulerDomain`）（`[SCHED-HIER-001]`）**：
    - `WorkerSchedulerDomain` 仅持有每 `Worker` 本地 `Mutex<WorkerLocalState>`、`tokio::sync::Notify` 与原子计数器，不持有全局 `AgentInstance` 表或 `Global` 锁引用；正常 `enqueue`、`wake_item`、`dequeue_next` 与 `complete_in_flight` 热路径在稳态下**零次**获取 `Global` 控制锁与 `Cell` 放置锁；
  - **有界 WFQ/DRR 调度、父级硬配额与取消围栏（`[SCHED-QUEUE-001]`）**：
    - `Worker` 内跨 `ResourceGroup` 采用按 `weight` 量子的 Deficit Round Robin（DRR）公平调度，组内按 `(effective_priority, deadline_class.urgency_rank(), starvation_age_ms)` 及 FIFO 序号确定性出队；
    - 同一 `Cell` 下各 `Worker` 通过无锁 `ResourceGroupQuotaState`（`AtomicUsize::try_update` CAS 与 `AtomicU64::fetch_max`）共享父级 `parent_hard_quota_in_flight` 硬上限与 `fenced_cancel_epoch` 取消围栏；
  - **`Context` 亲和放置与安全工作窃取（`[SCHED-STEAL-001]`）**：
    - `CellSchedulerDomain::place_runnable` 优先将携带 `preferred_context_digest` 的工作项路由至已驻留该热 `Context` 且未饱和的 `Worker`（`context_affinity_hit = true`），当热缓存 `Worker` 饱和时自动回退至同 `Cell` 健康兄弟 `Worker`；
    - `WorkerSchedulerDomain::steal_from_peer_worker` 与 `CellSchedulerDomain::steal_from_overloaded_peers` 按 `worker_id` 严格字典序加锁，消除对称并发窃取的 AB-BA 死锁；在同一临界区内原子移除受害者队列项并迁入窃取者队列（零复制、零双执行窗口），对所有被 `evaluate_steal_eligibility` 拒绝的项记录 `skipped_ineligible` 并原地保留在受害者队列中，同时拒绝跨 `Cell` 直接窃取（`StealError::CrossCellStealForbidden`）；
  - **反向背压传播与跨 `Cell` 负载卸载（`[SCHED-BACKPRESSURE-001]`）**：
    - 当 `Worker` 队列利用率跨越水位或报告下游 `ModelSlot` / `DriverPool` / `StorageIo` 等饱和源时，背压信号沿 `Worker -> Cell -> Global` 反向传播，收缩 `Cell` 与 `Global` 的 `effective_materialization_window`，使新非关键接纳立即返回 `SchedulerAdmission::WaitingResource` 而非无界堆积等待队列；
    - `GlobalSchedulerDomain::place_across_cells` 汇总各 `Cell` 背压摘要，当首选 `Cell` 饱和时自动将新工作项卸载至健康的对等 `Cell`，所有兼容 `Cell` 均饱和时返回 `SchedulerAdmission::WaitingResource`。

## 2. 验证（2026-10-25，Windows x86_64）

```text
cargo fmt -p nlos-runtime -p nlos-runtime-tokio -- --check                      → 退出码 0
cargo clippy -p nlos-runtime -p nlos-runtime-tokio --all-targets -- -D warnings → 退出码 0，零警告
cargo test -p nlos-runtime-tokio --test c_sched_three_tier_domain               → 12 passed / 0 failed
cargo test -p nlos-runtime -p nlos-runtime-tokio --no-fail-fast                 → 176 passed / 0 failed / 14 ignored（35 测试目标，退出码 0）
```

证据测试 `crates/nlos-runtime-tokio/tests/c_sched_three_tier_domain.rs`（12 项测试）断言面：

1. **三层拓扑契约与无全局锁热路径并发派发（`[SCHED-HIER-001]`）**：
   - `sched_hier_001_domain_id_lineage_and_tier_contracts`：验证 `Global / Cell / Worker` 三层 `SchedulerDomainId`、血统判定与 `leaf_nominal_id` 转换；
   - `sched_hier_001_concurrent_multi_worker_tokio_dispatch_without_global_lock_contention`：4 线程 Tokio 运行时下 4 个 `WorkerSchedulerDomain` 并发执行 160 次本地 `enqueue`、挂起态 `wake_item`、`wait_for_runnable`、`dequeue_next` 与 `complete_dispatched`，断言全部 160 项唯一完成且期间 `Global` 控制锁获取增量为 `0`、`Cell` 放置锁获取增量为 `0`；
   - `sched_hier_001_cross_cell_raw_worker_steal_is_forbidden_fail_closed`：断言跨 `Cell` 直接调用 `steal_from_peer_worker` 返回 `Err(StealError::CrossCellStealForbidden)`，自窃取返回 `Err(StealError::SelfSteal)`。
2. **有界 `ResourceGroup` WFQ/DRR、饥饿老化、父级硬配额与取消围栏（`[SCHED-QUEUE-001]`）**：
   - `sched_queue_001_bounded_resource_group_queue_and_drr_weighted_fair_share`：断言超出 `max_queue_depth` 时以 `RejectedQueueFull` 拒绝，且权重 `3:1` 的双 `ResourceGroup` 在 8 次派发中精确呈现 `6:2` 公平份额；
   - `sched_queue_001_priority_clamping_starvation_aging_and_full_queue_inspection`：断言超限 `requested_priority` 入队时按 `max_priority_cap` 截断，`starvation_age_ms` 推进后低优先级 `BackgroundBatch` 项老化晋升超越新入队 `Standard` 项，且 `QueueItemInspection` 全部字段精确匹配；
   - `sched_queue_001_parent_hard_quota_and_cancellation_epoch_fence_across_cell_workers`：断言跨双 `Worker` 共享的 `parent_hard_quota_in_flight = 2` 严格阻塞第 3 个并发派发并报告 `ThrottleReason::ParentHardQuotaReached`，`advance_cancel_epoch` 跨 `Cell` 清除旧 `cancel_epoch` 队列项并以 `RejectedCancelled` 拦截过期入队。
3. **零复制原子工作窃取与安全门禁（`[SCHED-STEAL-001]`）及 `Context` 亲和放置**：
   - `sched_steal_001_work_stealing_transfers_ownership_atomically_with_zero_duplication`：多线程并发屏障起步下 4 个 `Worker` 对称窃取与执行 40 个可迁移项，断言 `40` 项唯一执行、零重复、零丢失、零死锁；
   - `sched_steal_001_strictly_refuses_active_effect_permit_reservation_and_device_lease`：断言持有活跃 `EffectPermit`、`Reservation`、`ExclusiveDeviceLease` 或 `migratable == false` 的 4 个工作项全部被拒绝窃取并原地保留在受害者队列中，仅唯一干净可迁移项被原子迁出；
   - `sched_steal_001_enforces_fenced_isolation_namespace_secret_tenant_authority_and_device_affinity`：逐一验证 `IsolationDomainFenced`（含动态 `fence_isolation_domain`）、`NamespaceMismatch`、`SecretScopeMismatch`、`DeviceAffinityUnsatisfied`、`ProviderTenantMismatch` 与过期 `authority_term` 的 `TaskAuthorityMismatch` 门禁拒绝；
   - `context_affinity_placement_hits_warm_worker_cache_until_saturated_then_falls_back`：断言携带 `preferred_context_digest` 的工作项持续命中热缓存 `Worker`（`context_affinity_hit = true`），直至该 `Worker` 饱和后自动回退至健康兄弟 `Worker`（`context_affinity_hit = false`），且窃取路径同样优先命中热缓存（`context_locality_hit = true`）。
4. **反向背压传播与跨 `Cell` 压力感知卸载（`[SCHED-BACKPRESSURE-001]`）**：
   - `sched_backpressure_001_reverse_propagation_worker_to_cell_to_global_shrinks_window_and_returns_waiting_resource`：断言 `Worker` 侧 `ModelSlot` 饱和与 `Critical` 过载沿 `Worker -> Cell -> Global` 反向传播，将 `materialization_window` 从 `16` 收缩至 `4` 乃至 `0`，令非关键接纳返回 `SchedulerAdmission::WaitingResource`（零队列堆积），解除饱和后自动恢复至 `16`；
   - `sched_backpressure_001_cross_cell_pressure_aware_placement_sheds_overloaded_cell_to_healthy_cell`：断言双 `Cell` 拓扑中首选 `Cell A` 因 `DriverPool` 饱和后，`Global` 自动将新请求卸载至健康的 `Cell B`，当 `Cell B` 的 `StorageIo` 也饱和时全局返回 `SchedulerAdmission::WaitingResource`。

## 3. 边界与限制（诚实域，`EVID-SCOPE-001`）

- **单声明范围**：本证据覆盖 `C-SCHED` 轨道在 `nlos-runtime` 与 `nlos-runtime-tokio` 中的三层 `Global -> Cell -> Worker` `SchedulerDomain` 契约、有界 `ResourceGroup` WFQ/DRR + 饥饿老化队列、`[SCHED-STEAL-001]` 安全工作窃取、`Context` 亲和放置与 `[SCHED-BACKPRESSURE-001]` 反向背压及跨 `Cell` 卸载算法。
- **非跨物理主机网络传输证据**：`Global -> Cell -> Worker` 放置与背压信号在单进程多线程 Tokio 运行时内跨独立域状态验证；跨物理主机 RPC 传输、持久租约故障接管与网络分区仲裁属于 `C-SHARD` / `nlos-cell` / `nlos-lease` 轨道职责。
- **非 `[SCHED-BATCH-001]` 推理批处理或 `[SCALE-PROFILE-001]` 规模声明**：模型/工具调用物理批处理与 `10K_LOGICAL` / `100K_LOGICAL` / `1M_LOGICAL` 基准压测不在本证据声明范围内。

## 4. 复现

```text
cargo test -p nlos-runtime-tokio --test c_sched_three_tier_domain -- --nocapture
```
