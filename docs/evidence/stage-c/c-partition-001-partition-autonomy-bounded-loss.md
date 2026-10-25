# C-PARTITION-001 / W65-CPARTITION：控制平面断连局部自治、远端不确定失败显式分类、重连有界配额损耗对账与跨双存活 OS 进程收敛验证（DIST-LOCAL-001 & DIST-FAIL-001 & LEASE-LOSS-001 & LEASE-RECON-001 & ROAD-C-001）

- 状态：`PASS`（`PartitionAutonomyGate` 分区局部自治门控、`RemoteCallContract` / `RemoteCallOutcome` 远端失败显式四分类与盲重试拒绝、`drive_partition_reconnection_convergence` 跨双 `CellPartitionEndpoint` 四阶段重连有界损耗收敛编排、`PARTRECV` 联邦检查点摘要验真及双存活子 OS 进程 6 场景集成矩阵已落地并验证）
- 日期：2026-10-25
- Owner：`C-PARTITION`（`W65-CPARTITION` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §26.3 `[DIST-LOCAL-001]`、`[DIST-FAIL-001]` + §12 `[LEASE-LOSS-001]`、`[LEASE-FENCE-001]`、`[LEASE-RECON-001]` + §17.6 `[SEM-CHECKPOINT-001]` + §26.2 `[DIST-EVENT-001]`、`[DIST-AUDIT-001]`、`[DIST-ART-001]` + §33 Stage C `[ROAD-C-001]` + `ADR-0018`（单 OS 进程 = 单 Cell）+ `ADR-0019`（共享文件目录 `CellDirectory` federation 机制面）
- Base HEAD：`5ac73b6333515efbd28b9fa94a4c9eb27df2cf18`

## 1. 实现事实

- `crates/nlos-lease/src/lib.rs`（移除历史过期 `F1 / 2026-10-05` 注释头，代码与公开 API 零改动）：
  - 清理顶部过期的 F1 冻结注释块，与已落地的 `C-LEASE-001`（`SqliteLeaseStore` + 三族非互换对账回执 + 跨节点迁移）代码事实对齐。
- `crates/nlos-cell/src/partition.rs`（新增）与 `crates/nlos-cell/src/lib.rs`（导出分区自治与远端失败契约原语）：
  - **控制平面链路三态与分区自治门控（`[DIST-LOCAL-001]`）**：
    - `ControlPlaneLinkPhase` / `ControlPlaneLinkState`：显式建模 `Connected { verified_fence, last_contact_tick }`、`Partitioned { frozen_fence, partitioned_since_tick, reason_code }` 与 `Reconnecting { prior_frozen_fence, candidate_fence, started_tick }` 三态状态机，强制纪元/栅栏令牌单调推进与逻辑时钟不回退。
    - `LocalAutonomyProof`、`PartitionedActionKind`、`PartitionAutonomyReceipt` 与 `PartitionAutonomyGate`：在 `Partitioned` 与 `Reconnecting` 期间：
      1. 只要本地已缓存 Capability 在当前 tick 未过期、未撤销且可本地验真，且本地 `QuotaLease` 与冻结纪元匹配、未超 TTL 且剩余配额充足（或 `requested_quota_spend == 0` 纯本地计算），即放行并签发域分隔 SHA-256 `PartitionAutonomyReceipt`（`llmos/cell/partition-autonomy-receipt/v1`）；
      2. 能力过期（`CapabilityExpiredDuringPartition`）、能力撤销或无法本地验真（`CapabilityNotLocallyProvable`）、租约 TTL 到期（`QuotaLeaseExpiredDuringPartition`）、冻结剩余额度不足（`QuotaFrozenInsufficientRemaining`）或栅栏失配（`FenceMismatchDuringPartition`）一律 fail-closed 拒绝，绝不在断连期隐式宽限或垫支；
      3. 配额扩容/续租（`RequestQuotaExpansionOrRenewal`）、无本地权威的跨 Cell canonical 发布（`CrossCellCanonicalPublishWithoutLocalAuthority`）及未缓存远端权威变更（`UncachedRemoteAuthorityMutation`）在非 `Connected` 态下一律以 `ControlPlaneRequiredDuringPartition` 显式阻塞拒绝。
  - **远端失败显式四分类与不确定副作用重试门禁（`[DIST-FAIL-001]`）**：
    - `RemoteCallContract`、`RemoteCallOutcome<T>`、`RemoteRetryDirective` 与 `RemoteRetryRefusal`：将跨 Cell 调用结果显式区分为 `Succeeded`、`Unavailable`（保证零远端副作用，允许安全重试）、`DeadlineExceededUncertain`（超时但远端副作用未知）与 `Partial`（部分子项已提交且未决子项不确定），禁止把超时伪装为本地 `Unavailable`；对 `DeadlineExceededUncertain` 与 `Partial`，无幂等键重试以 `MissingIdempotencyKeyForUncertainOutcome` 拒绝，未附带对账/栅栏屏障以 `UncertainSideEffectRequiresReconciliation` 拒绝，仅在携带幂等键且具备对账屏障（`ReconcileOrRetryWithIdempotency`）时允许收敛。
- `crates/nlos-slice-k/src/cell_host.rs`（纯增量扩展，既有 `CellHost` 公开方法签名 100% 兼容）：
  - 集成 `PartitionAutonomyGate` 与 `ReplicaSyncRetentionSet`，在 `advance_epoch` 推进 `CellFence` 时同步推进分区门控纪元快照；
  - 新增 `enter_control_plane_partition`、`begin_control_plane_reconnection`、`complete_control_plane_reconnection`、`admit_partitioned_action`、`spend_quota_lease_with_local_autonomy`、`semantic_writer_bootstrap_request` 等组合入口，将 `[DIST-LOCAL-001]` 局部自治门控直接绑定至 `CellHost` 本地 `QuotaLease` 账本扣减。
- `crates/nlos-slice-k/src/partition_recovery.rs`（新增）与 `crates/nlos-slice-k/src/lib.rs`（重导出公共类型与编排器）：
  - **跨进程可传输端点抽象 `CellPartitionEndpoint` 与本地实现 `LocalCellHostPartitionEndpoint`**：全量采用公有字段快照结构，使 `drive_partition_reconnection_convergence` 既可在进程内直接驱动，也可跨独立存活子 OS 进程经 IPC 代理驱动真实 `CellHost` 权威。
  - **四阶段重连有界损耗收敛编排器（`drive_partition_reconnection_convergence`）**：
    1. **Phase 1 — 纪元推进与在途额度冻结（`[LEASE-LOSS-001]`、`[LEASE-FENCE-001]`）**：校验两端处于 `Partitioned` 态，推进双 Cell `CellEpoch` 与 `CellFencingToken`，自动将旧纪元未结清 `QuotaLease` 转入 `Quarantined`（冻结资金池，禁止提前退回 `AVAILABLE`），并将控制平面链路推进至 `Reconnecting`。
    2. **Phase 2 — 有界配额损耗证明与三族非互换租约对账（`[LEASE-RECON-001]`、`[ROAD-C-001]`）**：
       - 对每条 `Quarantined` `QuotaLease`，严格先校验 `unattested_inflight_loss <= max_unreported_inflight_window`（超界立即以 `BoundedLossViolation` fail-closed 拒绝，零状态污染）以及 `attested_spent + unattested_inflight_loss <= face_value`；通过 `FenceBarrierAck` 与 `QuotaReconciliationReceipt` 结算后，仅将可证明未消费部分 `face_value - attested_spent - unattested_inflight_loss` 退回 `AVAILABLE`，不可证明在途窗口永久保留在 `Quarantined` 损耗桶中，产出带域分隔 SHA-256 的 `PartitionBoundedLossProof`（`verify_invariants()` 同时验证守恒等式 `reconciled_spent + refunded_available + permanently_quarantined_loss == total_granted_face_value` 与损耗上界不等式 `permanently_quarantined_loss <= max_unreported_inflight_window`）；
       - 独立结算 `CapacityLease`（`CleanupAttestation` + `FenceBarrierAck` -> `CapacityReconciliationReceipt` 归还容量池）与 `ExclusiveDeviceLease`（`DeviceResetReceipt` + `FenceBarrierAck` -> `DeviceReconciliationReceipt` 释放独占设备头并推进 `reset_generation`），绝不与 `QuotaLease` 混用回执。
    3. **Phase 3 — 跨 Cell 签名语义事件收敛与向量检查点对齐（`[DIST-EVENT-001]`、`[DIST-AUDIT-001]`、`[SEM-CHECKPOINT-001]`）**：双向交换公钥身份绑定与 `SignedSemanticVectorCheckpoint`（Ed25519 验签），通过 `diff_vector_checkpoints` 判定无全局标量 `log_seq` 的因果关系，按需双向导出/导入 `CrossDomainEventItem` 集合并执行二次重放幂等性校验，最终验证双 Cell 收敛至完全一致的 `domain_watermarks`、`covered_event_ids_root` 与 `checkpoint_digest`（`VectorCheckpointRelation::Equal`）。
    4. **Phase 4 — 按内容类型策略的跨 Cell Artifact 副本收敛与联邦检查点封存（`[DIST-ART-001]`）**：双向导出/导入 `ArtifactTransferPackage`（预物化 SHA-256 校验），无冲突分支执行 fast-forward 物化，并发分叉 `SymmetricPeer` 分支保留各自本地 canonical head、将对端分支钉入 `ReplicaSyncRetentionSet` 并产出双端逐字节一致的 `ArtifactConflictReceipt::conflict_digest`；随后完成两端 `complete_reconnection -> Connected`，生成 `PartitionRecoveryReceipt` 并以固定 192 字节 `PARTRECV` `CheckpointPrefix` 条目写入共享 `CellDirectory`，现场验证 `CheckpointFact::verify_digest(&prefix) == CheckpointDigestVerification::Verified`。

## 2. 验证

```text
cargo fmt -p nlos-cell -p nlos-lease -p nlos-slice-k -- --check               → 退出码 0
cargo clippy -p nlos-cell -p nlos-lease -p nlos-slice-k --all-targets -- -D warnings → 退出码 0，零警告
cargo test -p nlos-lease --no-fail-fast                                       → 退出码 0（22 passed / 0 failed）
cargo test -p nlos-cell --no-fail-fast                                        → 退出码 0（40 passed / 0 failed，2 ignored subprocess helpers，含 partition_autonomy 5 项）
cargo test -p nlos-slice-k --no-fail-fast                                     → 退出码 0（9 unit + 105 integration = 114 passed / 0 failed，14 ignored subprocess helpers，含 dual_cell_partition_recovery 6 项双存活子进程集成测试）
```

新增单元/组件测试 `crates/nlos-cell/tests/partition_autonomy.rs`（5 passed / 0 failed）与双存活子 OS 进程集成测试套件 `crates/nlos-slice-k/tests/dual_cell_partition_recovery.rs`（6 passed / 0 failed，1 ignored worker 入口；每个顶层测试均同时拉起两个独立存活子 OS 进程 `alpha` 与 `beta`，验证不同 OS PID 且在断连前、断连期间、重连后全程存活推进独立心跳，绝不以 `kill` 冒充网络分区）：

1. `dual_live_os_processes_sustain_local_autonomy_block_control_plane_and_classify_remote_failures`：双存活 OS 进程进入 `Partitioned` 后各自凭有效本地缓存 Capability 与 `QuotaLease` 余额持续扣减本地额度并推进 `CellDirectory` 心跳；验证能力未本地验真、能力过期、租约过期、冻结余额不足全部 fail-closed 拒绝，控制平面扩容/跨 Cell 未授权发布/未缓存远端变更全部以 `ControlPlaneRequiredDuringPartition` 阻塞，且 `DeadlineExceededUncertain` 与 `Partial` 拒绝盲重试直至提供幂等键与对账指令。
2. `partition_heal_with_full_gateway_attestation_achieves_zero_loss_across_three_lease_families`：断连期间双存活进程各自本地消费配额，重连时网关完整证明已消费额度（`unattested_inflight_loss = 0`），验证 `QuotaLease` 未用余额精确退回 `AVAILABLE`（零损耗）、`CapacityLease` 经 `CleanupAttestation` 全额归还容量池、`ExclusiveDeviceLease` 经 `DeviceResetReceipt` 释放设备头，两端回到 `Connected`（`epoch = 2`）且父进程独立打开 `CellDirectory` 验真双端 `PARTRECV` `ReconciliationCheckpoint`。
3. `partition_heal_with_unattested_inflight_window_quarantines_bounded_loss_without_oversell`：模拟断连期间本地新增未上报在途消费窗口；先验证 `unattested_inflight_loss (45) > max_unreported_inflight_window (40)` 时 fail-closed 拒绝，再验证有界在途未证明窗口（Alpha `25 <= 40`、Beta `20 <= 30`）永久隔离于 `Quarantined` 态、绝不退回 `AVAILABLE` 造成超卖，`PartitionBoundedLossProof::verify_invariants()` 严格成立。
4. `partitioned_semantic_admissions_converge_to_identical_vector_checkpoint_with_idempotent_replay`：双存活 OS 进程在断连期间各自独立向本地 SQLite `SemanticAuthority` 提交签名语义断言（产生 `VectorCheckpointRelation::Concurrent` 分叉），重连收敛后双方虽然本地 `log_seq` 顺序不同，但导出的 `SignedSemanticVectorCheckpoint` 达成逐字节一致的 `domain_watermarks`、`covered_event_ids_root` 与 `checkpoint_digest`，且第 2/3 次重放 100% 幂等（0 重复入库）。
5. `partitioned_artifact_updates_fast_forward_disjoint_heads_and_isolate_divergent_branches_symmetrically`：双存活 OS 进程在断连期间同时执行不相交 Artifact 多版本发布与同一 `SymmetricPeer` Artifact 并发冲突修改；重连后不相交分支双向 fast-forward 落盘，并发冲突分支在双端各自保留本地 canonical head（零静默 last-writer-wins 覆盖）、将对端竞争 blob 钉入 `ReplicaSyncRetentionSet` 可读保留，并产出双端完全一致的 `conflict_digest`。
6. `post_reconnection_stale_epoch_messages_and_fences_fail_closed_across_dual_live_processes`：重连推进至 `epoch = 2` 后，向两个存活子 OS 进程注入断连前滞留的 `epoch = 1` 旧栅栏配额申请、容量申请、用量上报与自治门控请求，全部以 `StaleEpoch` / `FenceMismatchDuringPartition` fail-closed 拒绝且账本零篡改，同时新纪元 `epoch = 2` 授权正常生效。

## 3. 边界与限制（诚实域）

- 本证据在 `ADR-0018` 单机双存活真实 OS 进程拓扑（各自拥有独立 `CellHost` 持久目录与 SQLite 权威，通过共享本地文件系统 `CellDirectory` 与原子文件邮箱 IPC 交互）上闭合了 `[DIST-LOCAL-001]`、`[DIST-FAIL-001]`、`[LEASE-LOSS-001]`、`[LEASE-FENCE-001]`、`[LEASE-RECON-001]`、`[ROAD-C-001]`、`[DIST-EVENT-001]`、`[DIST-AUDIT-001]`、`[SEM-CHECKPOINT-001]` 与 `[DIST-ART-001]` 的断连自治与重连有界损耗收敛闭环。
- 遵循 `ADR-0018`、`ADR-0019` 与 `RISK-B-11` 诚实边界：本切片采用单宿主机双存活 OS 进程与通道层控制平面断连模拟，不声称跨物理机 TCP/QUIC 套接字传输协议、Linux `netns`/交换机硬件级网络分区注入或多机 Raft/Paxos 共识仲裁已落地。

## 4. 复现

```text
cargo test -p nlos-cell --test partition_autonomy -- --nocapture
cargo test -p nlos-slice-k --test dual_cell_partition_recovery -- --nocapture
```
