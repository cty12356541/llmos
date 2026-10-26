# ROAD-C-001 / W66：阶段 C H6 分区 + 硬崩溃重启 + 权威接管/迁移 + 重连去重收敛联合故障注入门禁与 `nlos-slice-k` 服务面装配

- 状态：`PASS`（在 `nlos-slice-k` 生产装配面完整接入 `WaitAuthority`、`NotificationService` 与只读非权威 `SearchService`，扩展 `CellHost` 持久租约打开入口 `open_durable` / `open_federated_durable` 与 `slice-k-demo` 第 15 步收据演示，并通过跨真实子 OS 进程的 H6 联合分布式故障注入门禁 `c_road_001_h6_partition_restart_migration_gate` 全部 5 项场景）
- Assurance：`H6`（多 OS 进程并发 + 控制面分区 + `child.kill()` 硬崩溃冷重启 + 跨 Cell 权威接管与 8 步进程迁移 + 重连幂等回放故障注入门禁；严格限定于单主机多 OS 进程 `ADR-0018` 拓扑，见 §4 诚实边界）
- 日期：2026-10-26
- Owner：`C-MULTICELL` 轨道 `#15`（Task `W66-UNFREEZE-SLICEK-ROADC001`，Attempt 2 晋升；Attempt 1 `4f1e494c` 在完成生产源码装配与测试骨架落盘后因上游 TCP 连接中断未完成编译对齐与取证，Attempt 2 `1f7cac34` 审计复用其写集草稿、修复测试签名时间窗/幂等信封/迁移时间戳/收据 ID 碰撞与 Clippy 严格门禁并完成全套验证）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §28.4 `[ROAD-C-001]`；§26 `[DIST-LOCAL-001]`、`[DIST-FAIL-001]`、`[DIST-TASK-001]`、`[DIST-TASK-002]`、`[DIST-TASK-004]`、`[DIST-MIGRATE-001]`、`[DIST-MIGRATE-002]`、`[DIST-EVENT-001]`、`[DIST-ART-001]`；§11.2 `[LEASE-TTL-001]`、`[LEASE-LOSS-001]`、`[LEASE-RECON-001]`、`[LEASE-WAL-001]`、`[LEASE-MIGRATE-001]`；§25 `[TASK-NOTIFY-001]`；§24 `[DUI-NOTIFY-001]`、`[DUI-OFFLINE-001]`；§24.1 `[CONS-DECL-001]`、`[CONS-CTRL-001]`；§18 `[SEM-CHECKPOINT-001]`、`[TASK-SNAPSHOT-003]`；[ADR-0018](../../management/adrs/0018-single-host-multiprocess-dual-cell-topology.md)；[ADR-0019](../../management/adrs/0019-cross-cell-commit-semantics-extension.md)
- 前置证据：[c-unfreeze-001-search-notify-production-wiring.md](./c-unfreeze-001-search-notify-production-wiring.md)、[c-search-001-non-authoritative-checkpoint-search.md](./c-search-001-non-authoritative-checkpoint-search.md)、[c-notify-001-task-lifecycle-action-token.md](./c-notify-001-task-lifecycle-action-token.md)
- 基线：`8f16d57bc5bd09b41eee6227c9f58b5eafaf446f`

---

## 1. 覆盖需求矩阵

| 需求 / 规则 ID | 权威契约摘要 | 生产落点与 H6 验证锚点 |
|---|---|---|
| `[ROAD-C-001]` | 阶段 C 联合退出门禁：多进程控制面分区、进程硬崩溃重启、跨 Cell 接管/迁移、重连收敛与重复消息回放同场闭环 | `crates/nlos-slice-k/tests/c_road_001_h6_partition_restart_migration_gate.rs`（5 项顶层测试 + 13 次真实子 OS 进程孵化） |
| `[DIST-LOCAL-001]` / `[DIST-FAIL-001]` | 分区期间本地已缓存能力与有效 `QuotaLease` 维持本地自治；跨 Cell 控制面动作 fail-closed 拒绝；远端超时/不可达显式分类为 `RemoteCallOutcome` | H6 测试 1（分区本地自治扣费 + 4 类控制面动作拒绝 + 联邦搜索远端分片超时分类）与测试 2 |
| `[LEASE-TTL-001]` / `[LEASE-LOSS-001]` / `[LEASE-RECON-001]` | 租约到期或 epoch 推进将未结算额度冻结为 `Quarantined`（零静默退回 `AVAILABLE`）；仅凭匹配 `FenceBarrierAck` 的对账收据结算有界损失并幂等回放 | `CellHost::open_federated_durable`（`<root>/leases.sqlite3` `WAL + FULL`）+ H6 测试 1、测试 2 |
| `[LEASE-WAL-001]` | 本地 WAL 丢失不得猜测剩余额度，必须拒绝猜测提交并按 `node_boot_generation` 隔离，直至显式对账 | H6 测试 2（`refuse_guessed_remaining_on_wal_loss` + `fence_boot_generation_on_wal_loss` + `BoundedLossViolation` 拒绝） |
| `[DIST-EVENT-001]` / `[DIST-ART-001]` | 分区恢复 4 阶段协议：重连门禁 → 3 族租约隔离对账 → 跨控制域语义事件双向幂等收敛与同值 `SignedSemanticVectorCheckpoint` → 工件快进同步与分叉隔离收据 `PARTRECV` | `partition_recovery.rs` + H6 测试 1（`converged_event_count == 2`、双端 checkpoint 摘要一致、`ff_art_id` 快进同步、`conflict_art_id` 分叉隔离收据） |
| `[DIST-TASK-001]` / `[DIST-TASK-002]` / `[DIST-TASK-004]` | 旧 `TaskAuthority` 租约过期后由继任 Cell 驱动带签名屏障观测的跨 Cell 接管（`term 1 -> 2`、参与者注册表重开 `gen 2`），旧任期 `prepare`/`permit` 一律以 `AuthorityLeaseFenced` 拒绝，重复接管回放 fail-closed | H6 测试 1 第 5 阶段（跨子进程邮箱 RPC 采集签名屏障观测 + `drive_takeover_with_federation_binding`） |
| `[DIST-MIGRATE-001]` / `[DIST-MIGRATE-002]` / `[LEASE-MIGRATE-001]` | 跨 Cell 8 步进程迁移（透明检查点模式与固定会话降级 `restart-from-artifact` 模式）、能力衰减重绑与源端吊销、3 族租约守恒转移，以及 `SourceFenced` 后崩溃隔离（零双活进程） | H6 测试 1 第 6 阶段（`Cell A -> Cell B` 透明检查点 + `Cell B -> Cell A` 工件重启镜像双向 8 步迁移）与测试 3（`SourceFenced` 后注入故障触发 `QuarantinedPostFence`） |
| `[TASK-NOTIFY-001]` / `[DUI-NOTIFY-001]` / `[DUI-OFFLINE-001]` | 任务迁移通知幂等发布、崩溃重连后离线历史单调分页回放、一次性 `ActionToken` 签发/核销及二次回放/跨应用/跨任务/篡改凭证/过期 5 类 fail-closed 拒绝 | `service_faces.rs`、`SliceKRuntime`、`CellHost`、`slice-k-demo` 第 15 步 + H6 测试 1、测试 4、测试 5 |
| `[SEM-CHECKPOINT-001]` / `[TASK-SNAPSHOT-003]` / `[CONS-DECL-001]` / `[CONS-CTRL-001]` | `SearchService` 严格只读非权威（`NON_AUTHORITATIVE`），禁止直接驱动五大控制门禁；支持签名向量检查点有界候选发现、回权威库二次核验与跨 Cell 分片显式降级聚合 | `service_faces.rs`、`SliceKRuntime`、`CellHost`、`slice-k-demo` 第 15 步 + H6 测试 1、测试 5 |

---

## 2. 生产实现与装配事实（`crates/nlos-slice-k`）

1. **`crates/nlos-slice-k/src/service_faces.rs`（新增）**：
   - 定义确定性通知通道/主题配置 `NotificationTopicConfig`（支持应用级派生与命名派生）、`NotificationTopicBinding` 与 `SubscribedNotificationTopic`；
   - 封装任务生命周期通知发布/轮询/确认、离线历史分页查询（`query_offline_notification_history`）与一次性 `ActionToken` 签发/核销辅助面；
   - 封装只读非权威搜索辅助面：`query_runtime_at_checkpoint_with_verifier`、`verify_runtime_candidate_against_checkpoint`、`export_cell_search_shard`（支持检查点水位线截断与无界本地分片导出）、`remote_call_outcome_to_search_shard`（将 `[DIST-FAIL-001]` `RemoteCallOutcome` 映射为 `CellSearchShardOutcome::Failed`）与 `aggregate_federated_cell_candidates`（封装 `nlos_search::aggregate_federated_shards`）。
2. **`crates/nlos-slice-k/src/runtime.rs`、`error.rs`、`lib.rs`**：
   - `SliceKRuntime::open` 在同一 `<root>` 下持久装配 `<root>/waits`（`WaitAuthority`）、`<root>/notify`（`NotificationService` v2，`SQLite WAL + FULL`）以及绑定 `<root>/semantic/semantic-authority.db` 的只读 `Mutex<SearchService>`（暴露 `waits()`、`notify()`、`search() -> SearchServiceGuard<'_>`）；
   - 将原测试侧局部 `WaitAuthority` 统一收敛到 `runtime.waits()`，使 `NotificationService::register_delivery_wait` 与运行期投递唤醒共享同一持久等待权威；
   - `SliceKError` 新增类型化 `Wait(WaitError)`、`Notify(NotifyError)`、`Search(SearchError)` 与 `SemanticVectorCheckpoint(SemanticVectorCheckpointError)` 变体（零 `anyhow`、零字符串吞错）。
3. **`crates/nlos-slice-k/src/cell_host.rs`**：
   - 新增 `CellHost::open_durable` 与 `CellHost::open_federated_durable`，将 `QuotaLeaseGrantor`、`CapacityLeaseGrantor` 与 `ExclusiveDeviceLeaseGrantor` 统一挂载于 `<root>/leases.sqlite3`（`SqliteLeaseStore`，`WAL + FULL`），保证子 OS 进程被 `child.kill()` 强杀并冷重启后所有已提交租约状态与守恒池精确重水化；
   - 暴露 `CellHost` 级 `waits()`、`notify()`、`search()`、通知主题订阅/发布/轮询/离线历史/一次性 `ActionToken` 方法，以及 `export_signed_vector_checkpoint`、`query_at_signed_checkpoint`、`verify_candidate_against_checkpoint` 与 `export_search_shard`。
4. **`crates/nlos-slice-k/src/bin/slice-k-demo.rs`**：
   - 新增 `STEP 15 cell-host-services`（拆分为 `demo_cell_host_notify` 与 `demo_cell_host_search`）：在同一持久运行根上打开 `CellHost`，演示 `WAITING_CONSENT` 任务通知发布与轮询、一次性 `ActionToken` 核销与二次重放拒绝、离线通知历史分页读取、语义操作收据桥接写入、签名向量检查点导出、检查点有界非权威候选发现（验证 `TaskHeadCommit` 控制门禁拒绝）及回权威库二次核验，输出 4 类可 grep 结构化 `RECEIPT` 行。
5. **全仓冻结/退役声明静态清零审计**：
   - 对 `crates/**/*.rs` 执行 `冻结声明|退役声明|冻结状态|退役状态` 正则扫描，匹配数为 `0`。

---

## 3. H6 联合故障注入门禁场景（`c_road_001_h6_partition_restart_migration_gate.rs`）

所有多 Cell 场景均严格遵循 `ADR-0018`（单 OS 进程单 `CellAuthority`）：父测试进程通过 `std::process::Command` 孵化独立子 OS 进程运行 `road_c001_cell_worker`，各子进程独占 `<cell_root>` 下的 `SQLite WAL + FULL` 权威库与 `cell.lock`（记录真实 OS PID 与递增 `node_boot_generation`），通过共享文件系统 `<federation_root>`（`CellDirectory`）与原子临时文件重命名邮箱帧通信，并以真实 `Child::kill()` + `Child::wait()` 注入非优雅硬崩溃。

1. **`road_c001_compound_partition_crash_restart_migration_and_duplicate_replay_converges`（全链路 6 阶段联合场景）**：
   - **阶段 1–2（双活 Cell 孵化与控制面分区）**：孵化 `worker_a`（`boot_gen = 1`）与 `worker_b`（`boot_gen = 1`），预置 3 族租约、共同基线工件 `rev 1` 及崩溃前通知/`ActionToken`；双端进入 `Partitioned` 后验证本地自治 `QuotaLease` 扣费成功（Cell A 扣 120、Cell B 扣 90）、4 类控制面动作全部 fail-closed 拒绝、两侧独立追加语义事件与分叉工件修订，且联邦搜索在严格模式下因远端分片超时以 `RemoteShardFailed` 拒绝、在显式降级模式下返回 `FederatedPartialEventual` 与 `failed_shards.len() == 1`；
   - **阶段 3（Cell A 硬崩溃与冷重启重水化）**：对 `worker_a` 执行 `hard_kill_and_wait()`（验证非零退出状态且 `worker_b` 保持存活），在同一 `cell_a_root` 上冷重启 `worker_a2`（验证新 OS PID、`initial_boot_generation == 2`、持久 `leases.sqlite3` 零静默退款）；
   - **阶段 4（跨进程 4 阶段分区重连收敛）**：驱动 `drive_partition_reconnection_convergence` 完成 epoch 推进与 3 族租约有界损失结算（Cell A 永久隔离未证实窗口损失 `30`、退回 `250`；Cell B 永久隔离 `10`、退回 `200`）、双向语义事件幂等收敛（二次回放零重复导入，双端 `post_sync_checkpoint` 摘要完全一致）、快进工件 `ff_art_id` 同步至 Cell B、分叉工件 `conflict_art_id` 生成对称 `ArtifactConflictReceipt` 及联邦目录 `PARTRECV` 检查点；验证重连后联邦检查点搜索收敛为 `CheckpointBoundedSnapshot`（去重后 `2` 条唯一候选）且崩溃前通知幂等回放与一次性 `ActionToken` 首次核销成功、二次重放返回 `ActionTokenAlreadyRedeemed`；
   - **阶段 5（跨 Cell `TaskAuthority` 权威接管）**：在 `worker_a2` 上播种 `term = 1` 任务租约并等待到期，由 `worker_b` 经 IPC 采集签名屏障观测并执行 `drive_takeover_with_federation_binding` 晋升至 `term = 2`（参与者注册表重开 `gen = 2`），验证重复接管回放被拒绝，且 `worker_a2` 持旧 `term = 1` 租约发起 `prepare` 与 `permit` 均以 `AuthorityLeaseFenced` 拒绝；
   - **阶段 6（双向 8 步跨 Cell 进程迁移）**：先驱动 `Cell A -> Cell B` 透明检查点模式迁移（含 3 族租约守恒转移 `300 - 80 = 220`、飞行中异步操作取消排空、源端监管表注销与名称缓存失效、目标端恢复至 `process_generation == 2`），再驱动 `Cell B -> Cell A` 固定会话降级 `restart-from-artifact` 模式完成反向 8 步迁移。
2. **`road_c001_quota_freeze_and_exact_bounded_loss_under_partition_and_crash`**：
   - 验证分区内短 TTL `QuotaLease` 到期冻结不退回 `AVAILABLE`、`hard_kill` 重启后 `[LEASE-WAL-001]` WAL 丢失猜测提交拒绝与启动代隔离、旧代扣费拒绝、伪造 `FenceBarrierAck` 拒绝、超窗口损失 `BoundedLossViolation`（`60 > 40`）拒绝，以及合法 `QuotaReconciliationReceipt` 重复回放零二次退款。
3. **`road_c001_migration_crash_after_source_fenced_quarantines_without_dual_active_process`**：
   - 在源端完成第 4 步 `SourceFenced` 与第 5 步能力重绑后、第 6 步源租约关闭前注入崩溃/断连故障，随后强杀并冷重启源进程 `worker_a2`，证明 `abort_post_fence` 将协调器推进至 `QuarantinedPostFence`，源端与目标端均无活跃进程绑定（零双活进程），且源端 3 族未结算租约持久保持 `Quarantined`。
4. **`road_c001_duplicate_messages_and_action_tokens_across_reconnection_are_idempotent_and_one_shot`**：
   - 验证相同幂等键与相同信封的通知重复发布返回 `Replayed`（`seq == 2` 不膨胀）、`issue_notification_action_token` 幂等回放返回同一令牌、硬崩溃重启后离线历史分页精确恢复 3 条记录，以及 `ActionToken` 对篡改凭证、跨应用伪造、跨任务伪造、过期时间窗与二次重放共 5 类攻击全部 fail-closed 拒绝，且 `prune_notification_history_before` 水位线持久生效。
5. **`slice_k_runtime_and_cell_host_wire_live_search_and_notify_surfaces`**：
   - 直接验证 `SliceKRuntime::open` 与子进程 `CellHost` 的持久目录装配（`<root>/waits`、`<root>/notify`、`<root>/semantic`）、`WaitAuthority` 投递等待由 `Pending` 唤醒至 `Woken`、`SearchService` 非权威候选发现与签名向量检查点二次核验在关闭重开后完整保持。

---

## 4. 边界与限制（诚实域，`Claim <= Evidence` / `ADR-0018` / `RISK-B-11`）

1. **单主机多 OS 进程拓扑边界（`ADR-0018`）**：本门禁通过 `std::process::Command` 在单台 Windows x86_64 主机上孵化多个独立子 OS 进程（每个子进程严格持有一个 `CellHost` / `CellAuthority`），跨进程协调经由本地 NTFS 文件系统上的共享 `CellDirectory` 与原子文件重命名邮箱 IPC 完成。本证据**不声称**已覆盖跨物理机器的真实 TCP/RDMA 网络套接字传输、内核级丢包/乱序网卡故障注入或硬件集群时钟漂移。
2. **进程级强杀与存储层边界（`RISK-B-11`）**：本门禁中的硬崩溃采用真实 OS 进程级 `Child::kill()`（非优雅终止进程句柄，无析构器清理）并在同一数据目录上冷重启验证 `SQLite WAL + FULL` 与原子重命名恢复语义；这属于 Layer-2 进程硬崩溃故障注入证据，**不等同于**掉电测试台或块设备写撕裂模拟器下的 Layer-3 物理断电介质证明（`RISK-B-11` 物理掉电边界保持不变）。
3. **搜索面非权威只读边界（`[CONS-CTRL-001]`）**：在 `SliceKRuntime` 与 `CellHost` 中解冻装配的 `SearchService` 仅持有 `SQLite_OPEN_READ_ONLY` 连接与 `SearchUseRestrictions::NON_AUTHORITATIVE` 标记；任何控制面提交仍必须回源 `SemanticAuthority`、`TaskAuthority`、`CapabilityAuthority` 与 `ResourceLedger` 权威校验。

---

## 5. 验证结果与复现命令（2026-10-26，Windows x86_64）

```powershell
# 1. 全仓冻结/退役声明静态扫描（0 匹配）
$m = @(Get-ChildItem -Path crates -Recurse -Filter *.rs | Select-String -Pattern "冻结声明|退役声明|冻结状态|退役状态"); Write-Output "FREEZE_MATCH_COUNT=$($m.Count)"
# -> 退出码 0，FREEZE_MATCH_COUNT=0

# 2. 格式门禁（nlos-slice-k 与全工作区检查）
cargo fmt -p nlos-slice-k
cargo fmt --all -- --check
# -> 退出码 0，零差异

# 3. Clippy 严格零警告门禁（含 lib、slice-k-demo 二进制与全部集成测试目标）
cargo clippy -p nlos-slice-k --all-targets -- -D warnings
# -> 退出码 0，零错误、零警告

# 4. 阶段 C H6 联合分布式故障注入门禁套件
cargo test -p nlos-slice-k --test c_road_001_h6_partition_restart_migration_gate -- --nocapture
# -> 退出码 0：5 passed; 0 failed; 1 ignored（含 13 次真实子 OS 进程 road_c001_cell_worker 运行全部 ok）

# 5. nlos-slice-k 全量测试套件（单元 + 26 个集成测试目标 + 文档测试）
cargo test -p nlos-slice-k
# -> 退出码 0：合计 119 passed; 0 failed; 14 ignored（所有 ignored 项均为父测试孵化的子进程 worker 入口）

# 6. 纵向切片与 CellHost 服务面端到端演示二进制
cargo run -p nlos-slice-k --bin slice-k-demo
# -> 退出码 0，输出 STEP 15 cell-host-services begin/done 与 4 条结构化 RECEIPT 行，最终输出 [slice-k] DONE
```
