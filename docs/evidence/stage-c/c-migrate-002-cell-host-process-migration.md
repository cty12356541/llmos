# C-MIGRATE-002 / W64-CMIGRATE-HOST：跨 CellHost 真实权威端到端 Process 迁移编排与双 OS 进程联合验证（DIST-MIGRATE-001 & DIST-MIGRATE-002 & LEASE-MIGRATE-001）

- 状态：`PASS`（`drive_cell_host_process_migration` 跨 `CellHost` 真实权威 8 步迁移编排器、三族 Lease 守恒关闭/重授、Ed25519 签名 Capability 无放大衰减重绑、`[DIST-ART-001]` 重启镜像副本同步与双 OS 进程联邦集成矩阵已落地并验证）
- 日期：2026-10-25
- Owner：`C-MIGRATE`（`W64-CMIGRATE-HOST` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §26.3 `[DIST-MIGRATE-001]`、`[DIST-MIGRATE-002]` + §12 `[LEASE-MIGRATE-001]`、`[LEASE-CAPACITY-002]`、`[LEASE-DEVICE-001]` + §26.4 `[DIST-ART-001]` + `ADR-0018`（单 OS 进程 = 单 Cell）+ `ADR-0019`（共享文件目录 `CellDirectory` federation 机制面）
- Base HEAD：`3a91098a7557a987128b29a4eaf8490d1277a65b`

## 1. 实现事实

- `crates/nlos-slice-k/src/process_migration_drive.rs`（新增）与 `crates/nlos-slice-k/src/lib.rs`（导出 `process_migration_drive` 公共类型与驱动入口）：
  - **跨进程端点契约与本地 `CellHost` 实现**：定义 `SourceCellMigrationEndpoint` 与 `TargetCellMigrationEndpoint` 两端能力面，使得编排器既可在进程内直接驱动，也可跨独立 OS 进程经 IPC 代理驱动真实 `LocalCellHostSourceEndpoint`（源 `CellHost` + 可选 `RuntimeAdapter`）与 `LocalCellHostTargetEndpoint`（目标 `CellHost`）。
  - **8 步真实权威迁移闭环（`drive_cell_host_process_migration`）**：将 `nlos-process::ProcessMigrationCoordinator` 的每一步证明绑定到 `CellHost` 七件套与持久化 SQLite 权威：
    1. `Initiated`（Step 1）：校验源 `CellHost` 身份、源进程绑定活跃代次与目标 `CellIdentity`；先经 `ProcessMigrationCoordinator::initiate` 执行 `[DIST-MIGRATE-002]` `PinnedSessionPolicy` 门控（`RefuseMigration` 遇到 `PinnedNonMigratable` 会话在产生任何联邦副作用前立即 fail-closed 拒绝），准入通过后向联邦 `CellDirectory` 追加规范化 `MigrationIntent`（`MigrationObject::Process(process_id)`，原因标签前缀 `process-migration:v1:`）。
    2. `Quiesced`（Step 2）：若提供 `RuntimeAdapter` 则对目标 `CancellationScopeId` 执行作用域取消以排空活跃纤程，并在源 `SqliteOperationStore` 中对所有在途 `OperationHandle` 执行 `request_cancel`，构造并核验 `QuiesceProof`（`inflight_effects_drained == true`）。
    3. `Checkpointed`（Step 3）：支持两条真实路径：
       - `TransparentCheckpointRestore`：对进程状态载荷计算域分隔 SHA-256（`llmos/slice-k/process-state-checkpoint/v1`），可选写入源 `ArtifactStore` 缓存并在 Step 7 跨进程同步至目标缓存；
       - `DowngradedRestartFromArtifact`（`[DIST-MIGRATE-002]` + `[DIST-ART-001]`）：从源 `ArtifactStore` 读取已提交的重启镜像制品修订并导出 `ReplicaRevisionBundle`，在目标 `ArtifactStore` 缺失时通过 `sync_artifact_replica_head`（`SingleAuthoritativeCell` 策略）同步副本头与字节内容，随后在目标端重新计算 `ContentDigest` 与 `MigrationMode` 声明摘要逐字节比对。
    4. `SourceFenced`（Step 4）：在源 `ProcessAuthority` 执行 `mark_process_terminated` 终结旧代绑定，从源 `SupervisorPidRegistry` 注销 `(process_id, source_generation)`，驱动源 `FailureDetector::sweep` 直至 `MonitoredSubject::Process(process_id)` 升格为不可复活的 `Liveness::Dead`，在源 `CapabilityNameCache` 按 `source_generation` 推进路径失效高水位，并现场执行旧代心跳探针（`record_fenced_process_heartbeat`）与旧代纤程注册探针（`register_fiber_incarnation`）确认双双被拒后签发 `SourceProcessFenceProof`。
    5. `CapabilitiesRebound`（Step 5）：在目标 `IdentityAuthority` / `CapabilityAuthority` 上验签执行 Ed25519 委托链（`bootstrap_principal` + `issue_root_signed` + `delegate_signed`），严格依赖 `CapabilityAuthority` 拒绝任何权限放大（如 `RightsAmplification`），将新代能力写入目标 `CapabilityNameCache`（`target_generation = source_generation + 1`），并在源端执行签名撤销 `revoke_signed` 使旧句柄永久失效。
    6. `LeasesRegranted`（Step 6，`[LEASE-MIGRATE-001]` / `[LEASE-CAPACITY-002]` / `[LEASE-DEVICE-001]`）：严格先在源 `CellHost` 完成关闭/归还或栅栏对账（`QuotaLease` `begin_close -> ack_close` 或 `reconcile_quota_lease`、`CapacityLease` `begin_return -> complete_return` 或 `reconcile_capacity_lease`、`ExclusiveDeviceLease` `begin_reset -> submit_reset_receipt` 或 `reconcile_device_lease`），计算域分隔关闭回执摘要（`llmos/slice-k/lease-close-digest/v1`）；随后在目标 `CellHost` 校验额度/容量守恒上限（`requested <= available_from_source`，违者以 `LeaseConservationViolated` fail-closed 拒绝）与独占设备纪元单调推进，使用全新 Lease ID 重授并构造 `LeaseCloseRegrantProof`。
    7. `TargetRestored`（Step 7）：在目标 `ProcessAuthority` 重放先序进程创世/恢复血统后调用 `restore_process` 落地 `target_generation = source_generation + 1` 活跃绑定与全新 `process_fencing_token`，在目标 `SupervisorPidRegistry` 绑定目标 `os_process_id` 并向目标 `FailureDetector` 登记首拍心跳，由 `TargetRestoreProof::from_restored_binding` 完成恢复摘要闭环校验。
    8. `Completed`（Step 8）：封存 `ProcessMigrationReceipt` 并校验 `receipt.verify_digest()`，将回执编码为 121 字节固定宽度 `PMIGRCPT` `CheckpointPrefix` 条目，通过 `TargetCellMigrationEndpoint::record_target_reconciliation_checkpoint` 追加至联邦 `CellDirectory` 并当场校验 `CheckpointFact::verify_digest(&prefix) == CheckpointDigestVerification::Verified`。
  - **Fence 前中止与 Fence 后隔离（Quarantine）闭环**：Step 4 之前发生错误时转移至 `AbortedPreFence`；一旦源进程在 Step 4 已被 fence，若 Step 5 或 Step 6 失败（例如签名能力权限放大或租约守恒超额），编排器自动调用 `quarantine_unsettled_source_leases` 将源端尚未结清的 `QuotaLease` / `CapacityLease` / `ExclusiveDeviceLease` 置入 `Quarantined` 态（资金/容量不退回可用池、设备头保持占用），并将状态机置入 `ProcessMigrationStep::Quarantined`（返回 `ProcessMigrationDriveError::QuarantinedPostFence`），目标端保持零恢复残留。
- `crates/nlos-slice-k/src/cell_host.rs`（增量扩展，既有签名零破坏）：
  - 集成 `os_process_id: u32` 与代数门控 `SupervisorPidRegistry`（在 `spawn_monitored_process` 注册、在 `terminate_monitored_process` 注销），新增 `record_fenced_process_heartbeat` 以及 `begin_quota_close` / `ack_quota_close` / `reconcile_quota_lease` / `quarantine_capacity_lease` / `reconcile_capacity_lease` / `fence_device_lease` / `quarantine_device_lease` / `reconcile_device_lease` 与只读查询接口。

## 2. 验证

```text
cargo fmt -p nlos-slice-k -- --check                     → 退出码 0
cargo clippy -p nlos-slice-k --all-targets -- -D warnings → 退出码 0，零警告
cargo test -p nlos-slice-k --no-fail-fast                → 退出码 0（9 unit + 99 integration = 108 passed / 0 failed，13 ignored subprocess helpers）
```

新增双 OS 进程联邦集成测试套件 `crates/nlos-slice-k/tests/dual_cell_process_migration.rs`（每个顶层测试均派生两个独立子 OS 进程 `child_source_cell_host_worker` 与 `child_target_cell_host_driver`，各自持有独立 `CellHost` 与本地数据目录，共享文件系统 `CellDirectory` 并通过原子文件 IPC 驱动完整迁移协议，外加父进程独立打开 `CellDirectory` 复核）：

1. `transparent_process_migration_completes_all_8_steps_and_seals_checkpoint_across_dual_cell_hosts`：验证双 OS 进程（不同 OS PID）透明迁移完整走通全部 8 步、源端纤程作用域取消与在途 `Operation` 进入 `CancelRequested`、源进程终结与旧能力撤销、目标端 `generation = 2` 恢复与缓存落盘、`ProcessMigrationReceipt` 摘要验真以及联邦 `MigrationIntent` + `PMIGRCPT` `ReconciliationCheckpoint` 摘要 `Verified`。
2. `multi_family_lease_migration_conserves_quota_capacity_and_exclusive_device_without_double_allocation`：验证 `QuotaLease`（花费 50 后仅剩余 150 迁出）、`CapacityLease`（120 全额先归还源池再在目标分配）与 `ExclusiveDeviceLease`（先完成硬件重置回执释放源设备头再在更高 `exclusivity_epoch` 授予目标）三族联合迁移守恒且无双重分配。
3. `source_fencing_refuses_stale_heartbeat_dispatch_supervisor_and_name_cache_on_source_cell_host`：验证 Step 4 之后源 `CellHost` 上旧实例 `FailureDetector` 处于 `Liveness::Dead` 且拒绝心跳（`SubjectAlreadyDead`）、代数门控心跳与纤程派发以 `ProcessBindingTerminal(Terminated)` 拒绝、`SupervisorPidRegistry` 已注销、`CapabilityNameCache` 旧代重插以 `StaleGeneration` 拒绝，而目标 `CellHost` 新代（`generation = 2`）心跳与名称缓存正常服务。
4. `dist_migrate_002_refuse_migration_rejects_pinned_session_and_leaves_source_cell_host_untouched`：验证携带 `PinnedNonMigratable` 会话且策略为 `RefuseMigration` 时在 Step 1 以 `PinnedSessionNonMigratable` fail-closed 拒绝，联邦目录零 `MigrationIntent` 写入，源端进程/监督表/名称缓存/租约原封不动，目标端零副作用。
5. `dist_migrate_002_downgrade_to_restart_from_artifact_syncs_and_verifies_real_artifact_store_digest`：验证携带 `PinnedNonMigratable` 会话且策略为 `DowngradeToRestartFromArtifact` 时，源 `ArtifactStore` 发布的真实重启镜像修订经 `[DIST-ART-001]` `ReplicaRevisionBundle` 跨进程同步至目标 `ArtifactStore` 并通过真实摘要核验，最终签发 `DowngradedRestartFromArtifact` 模式回执与 `mode_code = 1` 检查点。
6. `post_fence_capability_amplification_failure_quarantines_migration_and_unsettled_source_leases`：验证 Step 4 源端已被 fence 后，Step 5 企图通过委托放大 `CapabilityRights` 时以 `RightsAmplification` fail-closed 拒绝，状态机转入 `ProcessMigrationStep::Quarantined`，源端未结清的 `QuotaLease` / `CapacityLease` / `ExclusiveDeviceLease` 全部自动转入 `Quarantined` 态（不退回可用池、独占设备头保持占用），目标端不执行 `restore_process`。
7. `post_fence_lease_conservation_violation_quarantines_migration_without_target_restore`：验证 Step 4 源端已被 fence 且源 `QuotaLease` 仅剩 `65` 可退回额度时，Step 6 若请求在目标端重授 `80` 额度立即以 `LeaseConservationViolated` fail-closed 拒绝并转入 `ProcessMigrationStep::Quarantined`，目标端额度池与进程表零污染。

## 3. 边界与限制（诚实域）

- 本证据闭合 `C-MIGRATE-001` 留下的 `CellHost` 跨组件编排缺口，在 `ADR-0018` 单机双真实 OS 进程拓扑（共享本地文件系统 `CellDirectory` 与文件 IPC 通道）上验证了 `CellHost` 七件套、SQLite 持久权威、三族 Lease、签名 Capability 与 `[DIST-ART-001]` 制品同步的端到端迁移闭环。
- 遵循 `ADR-0018`、`ADR-0019` 与 `RISK-B-11` 诚实边界：本切片不声称跨物理主机网络传输协议（TCP/QUIC RPC）、跨机分布式共识仲裁、内核级透明进程内存页热迁移（CRIU）或远端物理外设硬件 DMA 级强制隔离已落地。

## 4. 复现

```text
cargo test -p nlos-slice-k --test dual_cell_process_migration --test cell_assembly -- --nocapture
```
