# C-NET-SYNC-MIGRATE / W71-C-SYNC-MIGRATE：跨网络流式 `ArtifactTransferPackage` 分块验签、签名向量检查点增量拉取与双主机八步进程迁移/三节点 Quorum 六端点接管屏障（ADR-0026 §2.3/§3.2/§6.1 & ADR-0019 R2.3 #3）

- 状态（Verdict）：`PARTIAL_PASS`（`crates/nlos-semantic::net_sync` 跨网络 TLS 1.3 mTLS 五轴 Exporter 签名向量检查点增量拉取与三要素溯源原子收敛、`crates/nlos-artifact::net_transfer` 分块可断点续传 `ArtifactTransferPackage` 64 MiB 窗口背压流式传输与 staging SHA-256 逐块/整包双验签、`crates/nlos-slice-k::{net_cell_host,net_migration_drive}` 跨套接字真实双/三独立 OS 子进程零共享文件系统八步 `CellHost` 进程迁移、断网防双活幂等对账、`N = 3` Raft 多数派 `CommittedControlIndexGate` 驱动的 §26.1 六端点接管屏障与 `IPC-D7` `CellTopologyInspector`/`CellLeaseInspector` 生产注入已全部落库并通过验证；跨机 `ChaosTcpProxy` 故障代理与 `ROAD-C-001-XHOST` / `ROAD-C-002-XHOST` 复合门禁留待 `W72-C-XHOST-GATE` 验收后晋升 `PASS`）
- Assurance Level：`H6`（跨独立 OS 子进程 + 各自完全隔离的私有 `TempDir` SQLite 存储 + 零共享文件系统路径 + 真实 Loopback TCP + TLS 1.3 mTLS + 五轴 Exporter `CellFence` 套接字实测：覆盖中途 TCP 断连保留分块游标与禁盲目重发、对称分叉冲突双保留、八步进程迁移与固定会话策略、`SourceFenced` 后断网防双活与重连幂等对账、以及 `N = 3` 独立 `ControlDomainId` Raft 多数派提交索引绑定下 §26.1 六类 canonical endpoint 签名接管屏障与存活旧主机 `AuthorityLeaseFenced` 拒止验证）
- 日期：2026-10-27
- Owner：`C-NET-SYNC-MIGRATE`（`W71-C-SYNC-MIGRATE` 车道，Attempt 1 `25371007` + Attempt 2 `ca859fe2` + `W71-INT` 收口）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §1.1 `[NLOS-GOAL-001]`、§6.1 `[TCB-ENDPOINT-001]`、§12 `[LEASE-MIGRATE-001]` / `[LEASE-PROTO-001]`、§18 `[SEM-CHECKPOINT-001]`、§25.1 `[TASK-SCALE-002]`、§25.3–§25.4 `[UI-PARITY-001]`（`IPC-D7`）、§26.1 `[DIST-NAME-001]` / `[DIST-FAIL-001]` / `[DIST-TASK-001/002/004]`、§26.2 `[DIST-EVENT-001]` / `[DIST-ART-001]` / `[DIST-AUDIT-001]`、§26.3 `[DIST-MIGRATE-001/002]`
  - [ADR-0026: 跨物理机网络传输与控制平面共识扩展](../../management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md) §2.3、§3.2、§6.1、§6.3
  - [ADR-0018: 单机多进程双 Cell 拓扑与跨机升级边界](../../management/adrs/0018-single-host-multiprocess-dual-cell-topology.md) 退出策略 3 / 触发器 2 & 4
  - [ADR-0019: 跨 Cell 提交语义扩展点](../../management/adrs/0019-cross-cell-commit-semantics-extension.md) Trigger 7 & R2.3 #3
  - [ADR-0020: 控制面单写者与共识基底时序](../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md) `S-B -> S-A` 升级路径
  - 前置基线证据：[c-xhost-001-mtls-transport-and-fence-handshake.md](./c-xhost-001-mtls-transport-and-fence-handshake.md)、[c-xhost-002-conservative-clock-skew-and-remote-lease.md](./c-xhost-002-conservative-clock-skew-and-remote-lease.md)、[c-xhost-003-embedded-raft-control-plane-consensus.md](./c-xhost-003-embedded-raft-control-plane-consensus.md)、[d-ipc-001-twelve-domain-enum-and-symmetry.md](../stage-d/d-ipc-001-twelve-domain-enum-and-symmetry.md)
- Base HEAD：`e9087fedae09d950a181fd888c9a3e9f416035fa`

---

## 1. 实现事实与交付物清单

### 1.1 跨网络签名向量检查点增量拉取与三要素溯源收敛（`crates/nlos-semantic`）

| 路径 | 变更类型 | 核心职责 |
|---|---|---|
| [crates/nlos-semantic/src/net_sync.rs](../../../crates/nlos-semantic/src/net_sync.rs) | 新增 | 实现 `SemanticNetSyncServer` 与 `SemanticNetSyncClient`（线魔数 `"NLOSSY01"`，跑在 `nlos-net::MtlsCellTransport` `ControlRpc` 通道上）：① `ExchangeVectorCheckpoint` 交换双方 `SignedSemanticVectorCheckpoint`，强制校验 mTLS 五轴 Exporter 对端 `peer_claims.cell_identity == declared_caller_cell` 与 `verify_signed_vector_checkpoint`（实时查询 `IdentityAuthority` 阻断已撤销 store 密钥），通过 `diff_vector_checkpoints` 计算无全局标量 `log_seq` 的因果偏序（`Equal` / `LocalAhead` / `RemoteAhead` / `Concurrent`，`[SEM-CHECKPOINT-001]`）；② `PullDomainEventsBatch` 按 `(authority_id, event_id)` 字典序游标有界分页拉取缺失的 `CrossDomainEventItem`；③ `pull_and_merge_from_peer` 在全部批次拉取完毕前绝不写盘，收齐后统一验证三要素跨域溯源信封（原始生产者签名 + 源域 `AdmissionReceipt` 签名 + 非零 `trust_snapshot_digest`，`[DIST-AUDIT-001]`）并在单个 SQLite `Immediate` 事务中调用 `SemanticAuthority::converge_cross_domain_event_sets` 原子落盘（`[DIST-EVENT-001]`） |
| [crates/nlos-semantic/src/lib.rs](../../../crates/nlos-semantic/src/lib.rs) | 修改 | 导出 `pub mod net_sync;` 及跨网络检查点同步核心类型 |
| [crates/nlos-semantic/tests/net_vector_checkpoint_sync.rs](../../../crates/nlos-semantic/tests/net_vector_checkpoint_sync.rs) | 新增 | 4 项跨双独立 TLS 1.3 mTLS 主机与独立私有目录的端到端集成测试，覆盖多批次双向并发增量收敛、幂等零写重放、五轴 `CellIdentity` 冒充拦截、篡改检查点/缺失三要素溯源/同 `SemanticEventId` 冲突负载在批次 2 触发时的零部分 SQLite 写入回滚、已撤销签名密钥实时拒止、`LocalAhead` 因果非回退以及对端 `CellEpoch` 倒退拦截 |

### 1.2 跨网络分块流式 `ArtifactTransferPackage` 验签传输与断点续传（`crates/nlos-artifact`）

| 路径 | 变更类型 | 核心职责 |
|---|---|---|
| [crates/nlos-artifact/src/net_transfer.rs](../../../crates/nlos-artifact/src/net_transfer.rs) | 新增 | 实现 `ArtifactNetTransferServer`（同时实现 `RpcRequestHandler` 与 `BulkStreamHandler`）与 `ArtifactNetTransferClient`（控制面魔数 `"NLOSAT01"`，数据面分块魔数 `"NLOSAB01"`）：① `BeginTransferSession` 在 staging 前强制校验 `ArtifactSyncPolicyRegistry` 已注册 MIME 策略与 `package_manifest_digest`（`[DIST-ART-001]`）；② `BulkStream` 分块流对每帧 `ArtifactBulkChunkFrame` 校验 `chunk_sha256`、单调偏移与 `total_blob_bytes`，写入隔离的 `<store>/artifacts/tmp/net-stage/<pkg_digest_hex>/<blob_digest_hex>.part` 并原子更新带 `SHA-256` 的 `.cursor` 水位文件，遵循 `nlos-net` 64 MiB 窗口背压且绝不阻塞并发高优 `ControlRpc`；③ 中途 TCP 断开返回 `RemoteCallOutcome::DeadlineExceededUncertain { retry_directive: ReconcileBeforeRetry }`，`PendingReconciliationTracker` 物理拦截未对账盲目重试（`[DIST-FAIL-001]`），`QueryTransferCursor` 对账后 `transfer_package_resumable` 仅重传 `verified_chunks` 之后的剩余分块；④ `FinalizeStagedTransfer` 在提升入正式 `ArtifactStore` 前重算全量 `.part` `ContentDigest` 与 `package_manifest_digest`，并联动 `ReplicaSyncRetentionSet` 保护对称分叉隔离双头免遭孤儿 GC 回收 |
| [crates/nlos-artifact/src/lib.rs](../../../crates/nlos-artifact/src/lib.rs) | 修改 | 导出 `pub mod net_transfer;` 及流式制品传输 API |
| [crates/nlos-artifact/tests/net_chunked_artifact_transfer.rs](../../../crates/nlos-artifact/tests/net_chunked_artifact_transfer.rs) | 新增 | 4 项跨双独立 TLS 1.3 mTLS 套接字与私有 `ArtifactStore` 的集成测试，覆盖多版本 `600 KiB` 19+ 分块窗口流式传输与并发 `ControlRpc` 零饥饿、中途 TCP 强断保留 staging 游标 + 阻断盲目重试 + 精确偏移续传、篡改 `chunk_sha256`/篡改清单/未注册策略/伪造 `CellIdentity` fail-closed 拦截、以及对称并发分叉头 `QuarantineDivergentBranch` 双保留与孤儿 GC 存活验证 |

### 1.3 `NetCellHost` 跨机编排、`IPC-D7` 生产检视注入、八步进程迁移与 `N = 3` Quorum 六端点接管（`crates/nlos-slice-k`）

| 路径 | 变更类型 | 核心职责 |
|---|---|---|
| [crates/nlos-slice-k/src/net_cell_host.rs](../../../crates/nlos-slice-k/src/net_cell_host.rs) | 新增 | 封装网络化 `NetCellHost`：① `open_single_host_desktop` 在默认 `N = 1` 模式下绑定 `0` 个监听套接字、启动 `0` 个后台选举定时器、要求 `0` 张 TLS 证书（`[NLOS-GOAL-001]`、`ADR-0026` §6.3）；② `bind_networked` 在 `N >= 2` 模式下集成 `MtlsEndpointListener`、`MtlsCellTransport`、保守时钟偏斜 `ClockSkewPolicy`、`RemoteLeaseClient`/`RemoteLeaseServer`（`[LEASE-PROTO-001]`）、`SemanticNetSyncServer`、`ArtifactNetTransferServer` 与 `MultiHostQuorumNode` / `SingleHostPassthroughNode`；③ 为 `NetCellHost` 实现 `nlos-system-control::cell_inspector::{CellTopologyInspector, CellLeaseInspector}`，将阶段 C 真实跨机拓扑、三族租约水位与受控 `RequestProcessMigration` 无缝注入阶段 D `IPC-D7` 桌面控制面 |
| [crates/nlos-slice-k/src/net_migration_drive.rs](../../../crates/nlos-slice-k/src/net_migration_drive.rs) | 新增 | 实现两大跨网络生产编排器：① `drive_cross_socket_process_migration` 与 `reconcile_post_fence_interrupted_migration`（线魔数 `"NLOSPM01"`），在零共享文件系统的双主机间通过 mTLS 完整驱动 §26.3 八步进程迁移（`Initiated -> Quiesced -> Checkpointed -> SourceFenced -> CapabilitiesRebound -> LeasesRegranted -> TargetRestored -> Completed`）、强制 `[DIST-MIGRATE-002]` 固定会话 `RefuseMigration` / `DowngradeToRestartFromArtifact`、跨网络三族租约先关后授守恒（`[LEASE-MIGRATE-001]`）、Ed25519 Capability 无放大衰减重绑与源端撤销、以及 Step 4 `SourceFenced` 后网络切断防双活与重连幂等恢复；② `drive_quorum_backed_six_endpoint_takeover`（线魔数 `"NLOSTB01"`），在 `N = 3` 独立 `ControlDomainId` Raft 集群（`[TASK-SCALE-002]`）上通过 `propose_control_command_over_quorum` + `collect_quorum_commit_attestation_over_mtls` + `CommittedControlIndexGate` 驱动跨机 `DIST-TASK-002/004` 接管，经 `MtlsBarrierObservationClient` 收集全部 6 类 canonical §26.1 端点签名屏障并在存活旧主机上实证旧任期 `lease_one` 被 `AuthorityLeaseFenced` 持久拒止 |
| [crates/nlos-slice-k/src/lib.rs](../../../crates/nlos-slice-k/src/lib.rs) | 修改 | 导出 `pub mod net_cell_host;` 与 `pub mod net_migration_drive;` 及公共编排接口 |
| [crates/nlos-slice-k/tests/cross_host_sync_and_migration.rs](../../../crates/nlos-slice-k/tests/cross_host_sync_and_migration.rs) | 新增 | 4 项顶层多进程集成测试 + 1 项 `#[ignore]` 子进程入口 `child_net_cell_host_worker`（含裸跑无环境变量安全跳过守卫与 `SpawnedHostWorker` `Drop` 自动清理守卫），每个 `NetCellHost` 运行在独立子 OS 进程与独立私有 `TempDir` 中，全部跨节点数据与控制流 100% 走真实 Loopback TCP + TLS 1.3 mTLS + 五轴 Exporter 套接字 |

---

## 2. `RISK-B-09` 供应链与依赖审计

本次工作包**未引入任何新的外部第三方 crate**，仅在内部工作区 crate 之间建立显式路径依赖（`nlos-semantic -> nlos-net`、`nlos-artifact -> {nlos-cell, nlos-net}`、`nlos-slice-k -> {nlos-consensus, nlos-net, nlos-system-control, nlos-takeover-control}`），无循环依赖，100% 符合 `edition = "2024"`、`rust-version = "1.97"` 与 `unsafe_code = "forbid"`。

---

## 3. 验证命令与实测结果（Windows x86_64）

全部验证命令在隔离构建目录 `$env:CARGO_TARGET_DIR='d:\llmos\target\w71-sync'` 下执行并通过（退出码均为 `0`）：

```text
1. cargo fmt -p nlos-semantic -p nlos-artifact -p nlos-slice-k -- --check
   → 退出码 0（0 格式化差异）

2. cargo clippy -p nlos-semantic -p nlos-artifact -p nlos-slice-k --all-targets -- -D warnings
   → 退出码 0（0 警告）

3. cargo test -p nlos-semantic -p nlos-artifact -p nlos-slice-k --no-fail-fast -- --test-threads=4
   → 退出码 0：
     - nlos-artifact: 110 passed; 0 failed; 0 ignored（含 net_chunked_artifact_transfer 4/0）
     - nlos-semantic: 76 passed; 0 failed; 0 ignored（含 net_vector_checkpoint_sync 4/0）
     - nlos-slice-k: 123 passed; 0 failed; 15 ignored（含 cross_host_sync_and_migration 4/0/1）

4. cargo test -p nlos-semantic -p nlos-artifact -p nlos-slice-k --no-fail-fast -- --ignored --test-threads=1
   → 退出码 0（15 passed; 0 failed，含 child_net_cell_host_worker 裸跑常设门守卫验证）
```

### 3.1 新增核心跨网络测试套件逐项核验明细

1. **`crates/nlos-semantic/tests/net_vector_checkpoint_sync.rs`（4 passed / 0 failed）**
   - `multi_batch_remote_ahead_and_bidirectional_concurrent_convergence_over_mtls`：双主机各自独立写入重叠本地 `log_seq` 的断言事件，以 `max_batch_events = 2` 分页互拉并原子收敛至相同 `covered_event_ids_root`，重放拉取 `0` 批次、`0` 写入；
   - `forged_caller_cell_identity_is_rejected_by_mtls_five_axis_pinning`：mTLS 会话认证为 `cell_id(1)` 但载荷声称 `caller_cell = cell_id(99)` 时，被服务端以 `CallerCellIdentityMismatch` 确定性拒绝；
   - `tampered_checkpoint_missing_provenance_and_conflicting_payload_fail_closed_with_zero_mutation`：注入篡改检查点签名、第 2 批全零 `trust_snapshot_digest`、篡改 `AdmissionReceipt` 签名、篡改生产者签名及同 `SemanticEventId` 冲突负载，全部 fail-closed 且第 1 批合法事件绝不发生部分落盘；
   - `revoked_signer_key_causal_local_ahead_and_epoch_regression_fail_closed_over_mtls`：验证 `LocalAhead` 零回退、撤销生产者/检查点签名密钥实时阻断、以及对端 `CellEpoch` 落后于 `EndpointResolver` 期望纪元时在传输层 fail-closed。

2. **`crates/nlos-artifact/tests/net_chunked_artifact_transfer.rs`（4 passed / 0 failed）**
   - `large_multi_chunk_artifact_transfer_with_concurrent_control_rpc_non_starvation`：`600 KiB` 双修订包按 `32 KiB` 分块（19+ 帧）在 `64 KiB` 窗口背压下流式传输，同时并发执行 8 次 `50ms` 预算 `ControlRpc` 游标查询零饥饿通过，finalize 后自动清理 staging 目录；
   - `mid_blob_tcp_disconnect_preserves_staged_cursor_blocks_blind_retry_and_resumes_exact_offset`：`384 KiB`（12 块）传输中途经单次代理在 `~190 KiB` 处断开 TCP，验证返回 `DeadlineExceededUncertain(ReconcileBeforeRetry)`、正式 `ArtifactStore` 零半成品污染、未对账盲目重试返回 `UncertainSideEffectRequiresReconciliation`、提前 finalize 返回 `IncompleteStagedBlobs`，经 `reconcile_interrupted_stream` 对账后仅重传剩余未验证分块并完整合入；
   - `tampered_chunk_sha256_tampered_manifest_unregistered_policy_and_forged_cell_fail_closed`：验证伪造 `caller_cell`、未注册 `content_type`、篡改 `package_manifest_digest` 与篡改块内字节导致 `chunk_sha256` 失配全部 fail-closed；
   - `divergent_replica_heads_over_mtls_quarantine_both_heads_and_survive_orphan_gc`：双主机基于共同 `rev1` 并发产生分叉 `rev2`，双向同步后产出对称一致的 `ArtifactConflictReceipt`（`QuarantineDivergentBranch`），并在执行 `collect_orphan_blobs_with_sync_roots` 后验证双头 blob 均完整保留可读。

3. **`crates/nlos-slice-k/tests/cross_host_sync_and_migration.rs`（4 passed / 0 failed + 1 ignored child worker passed）**
   - `single_host_n1_net_cell_host_zero_sockets_and_cell_inspector_projection`：独立子进程验证 `N = 1` `SingleHostDesktop` 模式 `active_listener_sockets == 0`、`active_outbound_connections == 0`、`requires_tls_certificates == false`，同时验证 `CellTopologyInspector` / `CellLeaseInspector` 的三族租约快照投影与单机拒绝 `request_process_migration`；
   - `cross_socket_eight_step_migration_and_pinned_session_policies_over_mtls`：两个完全隔离的子 OS 进程（`Source Host 1` 与 `Target Host 2`，零共享目录）通过真实 TLS 1.3 mTLS 套接字验证：① `PinnedNonMigratable + RefuseMigration` 提前拒绝且源进程保持 `Runnable`；② 透明八步迁移全链路（含语义向量检查点同步、`ArtifactTransferPackage` 分块流式传输、源进程 `Terminated` 围栏、Ed25519 Capability 无放大衰减重绑與源端撤销、三族租约先关后授守恒、目标进程 `generation = 2` 恢复及 `PMIGRCPT` 联邦检查点锚定）；③ `PinnedNonMigratable + DowngradeToRestartFromArtifact` 显式降级重启；④ 放大权限重绑在源已 fence 后被目标拒绝并将源端三族未决租约转入 `Quarantined`；
   - `post_source_fenced_network_cut_prevents_dual_live_and_reconciles_idempotently`：在 Step 4 `SourceFenced` 刚完成、Step 5 `CapabilitiesRebound` 发起前切断目标 mTLS 链路，实证断网窗口内源端进程已 `Terminated`、源端三族租约处于 `Quarantined`、目标端无提前恢复进程（零双活执行），链路恢复后调用 `reconcile_post_fence_interrupted_migration` 幂等推进至 `Completed`；
   - `three_host_quorum_consensus_backed_six_endpoint_takeover_fences_old_term_host`：三个完全隔离的子 OS 进程（3 个互异 `ControlDomainId` 组成 `N = 3` Raft 多数派集群）通过 mTLS 完成 `AssignInitialAuthority` 多数派提交与 `CommittedControlIndexGate` 验签准入、分块导出任务分片、采集 §26.1 全部 6 类 canonical endpoint 签名屏障、`RecordCompletedTakeover` 多数派提交与 `CommittedControlIndexGate` 准入、`DIST-TASK-004` 后继注册表重开、联邦 `MigrationIntent` + `CheckpointFact` 绑定与 Raft 审计锚定，并向仍存活的 `Host 1` 发起 mTLS 探针实证旧任期 `lease_one` 的 prepare 与 permit 请求双双以 `AuthorityLeaseFenced` 被拒。

---

## 4. 诚实范围边界说明

1. **已完成范围**：跨独立 OS 进程与零共享文件系统路径下的 TLS 1.3 mTLS 签名向量检查点增量同步、分块可断点续传 `ArtifactTransferPackage` 流式传输、跨套接字八步 `CellHost` 进程迁移、`N = 3` Raft 多数派 `CommittedControlIndexGate` 驱动的六端点接管屏障及 `IPC-D7` `CellTopologyInspector`/`CellLeaseInspector` 实现已全部验证通过。
2. **留归 `W72-C-XHOST-GATE` 的范围**：可编程套接字级混沌故障代理 `ChaosTcpProxy`（随机丢包、RTT 抖动、非对称半开黑洞、TCP 帧中段 `RST`、偏斜越界注入）及 `ROAD-C-001-XHOST` / `ROAD-C-002-XHOST` 复合规模与混沌门禁在 `W72-C-XHOST-GATE` 车道验收。
