# C-FANOUT-001 / W64-CFANOUT：层级子组扇出、局部确定性归约与 Merkle 结果根、多轴扇出准入背压与多域舱壁隔离（DIST-TASK-003, DIST-FANOUT-001 & DIST-BULKHEAD-001）

- 状态：`PASS`（`[DIST-TASK-003]` 层级子组树、到达顺序无关 `DeterministicLocalReducer`、二进制 SHA-256 `MerkleResultRoot` + `MerkleInclusionProof` 与 `O(B * depth)` 增量检查点，`[DIST-FANOUT-001]` 六维配额预留与 `WaitingResourceBackpressure` 确定性排空，以及 `[DIST-BULKHEAD-001]` `Tenant / Workspace / TaskGroup / Provider` 独立舱壁与不可侵占 `SafetyTcbSystemReserve` 已落地并验证）
- 日期：2026-10-25
- Owner：`C-FANOUT`（`W64-CFANOUT` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §26.1 `[DIST-TASK-003]`、`[DIST-FANOUT-001]`、`[DIST-BULKHEAD-001]` + §25.1 `[TASK-GROUP-001..003]`、`[TASK-SCALE-001..002]`
- Base HEAD：`3a91098a7557a987128b29a4eaf8490d1277a65b`

## 1. 实现事实

- `crates/nlos-task/src/fanout.rs`（新增）与 `crates/nlos-task/src/lib.rs`（additive `pub mod fanout;` 与公共类型重导出，`SqliteTaskAuthority` 持久化表结构与 `SCHEMA_VERSION = 47` 保持零改动）：
  - **层级子组拓扑、确定性局部归约、Merkle 结果根与增量检查点（`[DIST-TASK-003]`，`[TASK-GROUP-003]`）**：
    1. `SubgroupNode` 与 `HierarchicalFanoutTree`：支持平衡自动分片构造（`build_balanced`，按 `FanoutLeafId` 规范化排序，与输入顺序无关）、增量注册（`register_subgroup` / `register_leaf`）与任意拓扑校验构造（`from_topology`）。强制三色 DFS 有向环检测（`CycleDetected`）、严格单父树校验（拒绝菱形多父 `MultipleParents` 与孤岛子组 `DisconnectedSubgroup`）、有界深度（`max_depth`）及有界局部扇出度（`max_branching_factor`）。通过 `TopologyComplexityMetrics` 机械证明任意子组与根协调器的局部连接度 `<= B`，总边数为 `O(N)`，完全消除 `N * (N - 1) / 2` 的 `O(N^2)` 全连接与全状态广播。
    2. `DeterministicLocalReducer`：强制绑定非零不可变 `reducer_digest: [u8; 32]`（全零摘要以 `ZeroReducerDigest` fail-closed 拒绝，`verify_reducer_binding` 拒绝不匹配摘要），对 `TaskAttempt` / `AgentInstance` 叶子成员按规范主键排序归约，完全独立于候选回执到达顺序。同一叶子或跨成员出现分歧候选回执时，以规范有序集合在 `DivergentLeafRecord` 与 `distinct_result_digests` 中显式完整保留，禁止静默 last-writer-wins 覆盖；同时按非分歧叶子去重统计独立 `ControlDomainId` 数（`[TASK-GROUP-003]` / `[TASK-SCALE-002]`）。
    3. 二进制 SHA-256 `MerkleResultRoot` 与 `MerkleInclusionProof`（`verify_merkle_proof`）：使用域分隔常量 `FANOUT_MERKLE_LEAF_DOMAIN = b"llmos/fanout-merkle-leaf/v1"` 与 `FANOUT_MERKLE_NODE_DOMAIN = b"llmos/fanout-merkle-node/v1"`（奇数尾节点按 RFC 6962 原样提升以避免二阶原像歧义），支持单子组归约器及多层 `HierarchicalFanoutTree` 从任意已上报叶子直达根 `MerkleResultRoot` 的统一包含证明与全字段防篡改校验。
    4. `IncrementalSubgroupCheckpoint`：维护子组级脏标记与缓存摘要（`SubgroupCheckpointCache`）。当单个叶子上报或更新候选回执时，仅沿 `owning_subgroup -> ... -> root` 祖先链标记脏路径，`checkpoint()` 自底向上仅重算脏叶子与脏祖先子组（`O(B * depth)` 哈希节点数），完全复用未触碰兄弟子树的缓存摘要；`recompute_full_reference()` 提供无副作用的 `O(N)` 全树重算基线以机械校验逐字节一致性。
  - **六维多轴扇出准入预留与结构化背压门控（`[DIST-FANOUT-001]`）**：
    1. `FanoutQuotaContract` 与 `FanoutUsageLedger`：显式预留并限制全部 6 个规范维度：
       - `max_group_depth: u32`
       - `max_total_children: u32` 与 `max_active_agents: u32`
       - `max_message_count: u64` 与 `max_message_bytes: u64`
       - `max_context_read_bytes: u64`
       - `max_write_bytes: u64`
       - `max_external_effects: u32`
    2. `FanoutAdmissionDecision`：区分 `Admitted { reservation_id, remaining_headroom }`、`WaitingResourceBackpressure { wait_ticket_id, saturated_axes, retry_after_hint_ms }` 与 `HardRefused { reason }`（超 `max_group_depth`、`GroupSealed`、`GroupCancelled`、`GroupPoisoned` 或有界背压等待队列满 `BackpressureQueueFull`）。资源轴饱和时平稳转入 FIFO 背压队列而非递归失控；调用 `settle_reservation`（强制校验 `actual <= reserved`，超限以 `ActualUsageExceedsReservation` fail-closed 拒绝）或 `cancel_reservation` / `extend_quota_contract` 时自动释放活跃槽位、退还未消费预留预算，并按确定性 FIFO 顺序唤醒等待请求。
  - **四域独立舱壁隔离与不可侵占 `SafetyTcbSystemReserve`（`[DIST-BULKHEAD-001]`）**：
    1. `BulkheadRegistry` 与 `BulkheadController`：覆盖 `Tenant`、`Workspace`、`TaskGroup`、`Provider` 四个规范隔离域，强制每个舱壁绑定全局唯一 `ResourceGroupId`（与任何既有舱壁或 `SafetyTcbSystemReserve` 共享 `ResourceGroupId` 立即以 `DuplicateBulkheadResourceGroup` 拒绝）、独立并发池（`max_concurrent`）、有界等待队列（`max_queue_depth`）及故障预算（`FailureBudget { max_consecutive_failures, max_total_failures }`）。
    2. 复合路由全有或全无准入（`WorkloadBulkheadRoute`）：当局部 `TaskGroup` 风暴或 `Provider` 停滞打满其并发池时，后续请求仅排队或限流于该瓶颈舱壁，绝不提前锁死上层健康 `Tenant` / `Workspace` 并发槽位，确保兄弟 `TaskGroup` 与健康 `Provider` 零退化运行；归因故障超出 `FailureBudget` 时仅将故障舱壁置为 `PoisonedCircuitOpen` 并驱逐其挂起等待者，支持单调递增 `reset_epoch` 行政重置。
    3. `SafetyTcbSystemReserve`：将系统总槽位严格划分为不可借用的 `reserved_tcb_slots` 与非 TCB 上限 `non_tcb_system_ceiling = total_system_slots - reserved_tcb_slots`；即便全部租户非 TCB 负载 100% 耗尽全局非 TCB 配额，`SafetyTcb` 关键路径槽位仍保持 100% 立即可得。

## 2. 验证

```text
cargo fmt -p nlos-task -- --check                                          → 退出码 0
cargo clippy -p nlos-task --all-targets -- -D warnings                     → 退出码 0，零警告
cargo test -p nlos-task --test c_fanout_hierarchical_bulkhead --no-fail-fast → 退出码 0（12 passed / 0 failed）
cargo test -p nlos-task --lib --no-fail-fast                               → 退出码 0（28 passed / 0 failed）
```

新增集成测试 `crates/nlos-task/tests/c_fanout_hierarchical_bulkhead.rs`（12 个具名测试）与模块内单元测试（2 个具名测试）：

1. `hierarchical_fanout_tree_partitions_1200_leaves_with_bounded_degree_and_no_all_to_all`：验证 1,200 叶子（600 `TaskAttempt` + 600 `AgentInstance`）平衡构建（`B=10, max_depth=3`），证明局部度 `<= 10`、树边数 `1,334` 对比避免的 `719,400` 条 `O(N^2)` 全连接边，且拓扑与输入顺序无关。
2. `hierarchical_fanout_tree_rejects_cycles_multiple_parents_depth_and_branching_overflow_fail_closed`：验证有向环、指向根回边、菱形多父、断连孤岛子组、超最大深度、超分支因子与重复叶子的 fail-closed 拒绝。
3. `deterministic_local_reducer_is_arrival_order_independent_and_preserves_divergent_candidates`：验证正向、逆向与互质步长置换到达顺序下 `MerkleResultRoot` 与 `reduced_digest` 逐位一致，多候选冲突叶子显式保留全部候选回执，且错误 `reducer_digest` fail-closed。
4. `merkle_result_root_and_inclusion_proof_verify_and_reject_tampering_across_all_fields`：验证奇数叶子（13 叶）与冲突叶子的 `MerkleInclusionProof` 验真，以及篡改结果摘要、溯源摘要、候选排序、空候选、归约器摘要、左右兄弟位置与路径长度时的全面拒绝。
5. `incremental_subgroup_checkpoint_recomputes_only_dirty_path_sublinear_in_1000_leaf_tree`：验证 1,000 叶子、111 子组树在单叶更新时仅重算 3 个脏祖先子组、复用 108 个缓存子组，总哈希节点数仅 `28`（对比全树重算 `1,999` 个节点），且增量根与全量参考根逐字节相等、跨层 Merkle 证明直达根验真通过。
6. `fanout_admission_enforces_all_six_normative_quota_dimensions_and_multi_axis_saturation`：逐一及组合验证全部 6 个规范准入维度（`GroupDepth`、`TotalChildren`、`ActiveAgents`、`MessageCount` / `MessageBytes`、`ContextReadBytes`、`WriteBytes`、`ExternalEffects`）。
7. `fanout_backpressure_blocks_runaway_recursion_and_slot_settlement_drains_waiters_deterministically`：验证活跃容量打满转入 `WaitingResourceBackpressure`、超队上限硬拒绝、超预留结算 fail-closed、`settle_reservation` 差额退还与 `cancel_reservation` 按 FIFO 确定性唤醒等待请求，以及取消状态清空等待队列。
8. `fanout_admission_idempotent_replay_and_quota_extension_drain`：验证活跃与排队预留的幂等重放、冲突参数拒绝及 `extend_quota_contract` 扩容排空。
9. `bulkhead_isolates_agent_fanout_storm_without_degrading_sibling_task_group_or_safety_tcb`：验证单 `TaskGroup` 50 并发扇出风暴被严格限制在本组舱壁内，同租户/工作区的兄弟 `TaskGroup` 与 `SafetyTcbSystemReserve` 100% 正常准入。
10. `bulkhead_contains_provider_stall_without_pinning_tenant_workspace_or_starving_healthy_provider`：验证 `Provider A` 100% 并发与队列停滞不会锁死共享 `Tenant` / `Workspace` 槽位，切往健康 `Provider B` 零阻塞，且槽位释放后按序唤醒等待票据。
11. `bulkhead_failure_budget_poisons_faulty_task_group_and_evicts_waiters_while_preserving_peers_and_reset`：验证连续与累计故障预算触发 `PoisonedCircuitOpen` 熔断、自动驱逐中毒舱壁挂起队列、隔离保护健康同伴以及单调 `reset_epoch` 行政恢复。
12. `safety_tcb_system_reserve_is_100_percent_untouchable_under_global_non_tcb_exhaustion`：验证多租户耗尽全局 `non_tcb_system_ceiling` 时被拒绝借用任何保留槽位，`SafetyTcbSystemReserve` 全部保留槽位 100% 可获取与释放。

## 3. 边界与限制（诚实域）

- 本证据验证 `nlos-task::fanout` 内 `[DIST-TASK-003]` 层级子组树、局部确定性归约、二进制 SHA-256 Merkle 结果根/包含证明与 `O(B * depth)` 增量检查点，`[DIST-FANOUT-001]` 六维配额预留与背压排空状态机，以及 `[DIST-BULKHEAD-001]` 四域独立舱壁隔离与 `SafetyTcbSystemReserve` 保留池。
- `SqliteTaskAuthority` 的 SQLite schema 保持 `SCHEMA_VERSION = 47` 不变；本切片不声称跨物理节点的网络 RPC 扇出传输通道、持久化 SQLite 舱壁账本落盘或与全局 `nlos-resource` / Cell 调度器的在线自动接线已完成。

## 4. 复现

```text
cargo fmt -p nlos-task -- --check
cargo clippy -p nlos-task --all-targets -- -D warnings
cargo test -p nlos-task --test c_fanout_hierarchical_bulkhead --no-fail-fast -- --nocapture
cargo test -p nlos-task --lib --no-fail-fast
```
