# C-SHARD-006 / W63-CS1D：生产级接管推进 + `MigrationObject` ↔ `DIST-TASK` 强绑定 + Checkpoint Digest 绑定升级

- 状态：`PASS`（`C-SHARD` Phase 1 `CS1-D` 收官：`MigrationObject` ↔ `DIST-TASK` 记录密码学绑定 + `ReconciliationCheckpoint` `DTBINDV1` 前缀验证 + 生产级 `takeover_drive` 消费 schema `v47` 扩展字段、签名 `ParticipantRegistryRecord` 快照验证与 Assignment 5 态推进）
- 日期：2026-10-24
- Owner：`C-SHARD` 计划 Phase 1（[c-shard-plan.md](../../management/c-shard-plan.md) `CS1-D` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md §26.1](../../design/06-架构设计总纲-v0.5.md)、[ADR-0018](../../management/adrs/0018-stage-c-single-host-multi-cell-architecture.md)（单机多进程 Cell 拓扑 + Decision 5 无共识边界）、[ADR-0019](../../management/adrs/0019-cross-cell-commit-semantics-extension.md) R1.2.2 / R1.2.5、[ADR-0020](../../management/adrs/0020-raft-and-consensus-exclusion-boundary.md)
- 基线：`236ff8278711eef4db837ec7b972115af54a5811`（Wave `W63`）

## 1. 实现事实

1. **`crates/nlos-cell/src/federation.rs` & `crates/nlos-cell/src/lib.rs`（`MigrationObject` ↔ `DIST-TASK` 密码学记录绑定 + Checkpoint Digest 绑定升级）**：
   - 新增域分隔常量 `DIST_TASK_BINDING_DOMAIN`（`b"llmos/dist-task-migration-binding/v1"`）、记录类别枚举 `DistTaskMigrationKind`（`AuthorityAssignment = 1` / `TakeoverReceipt = 2` / `ParticipantRegistry = 3`）、绑定描述符 `DistTaskRecordBinding`（绑定 `kind`、`task_id`、`task_generation`、`record_id`、`authority_term`、`control_epoch`、`participant_registry_generation`、`participant_registry_root` 与可选 `exact_fence_set_root`）以及 fail-closed 验证结果枚举 `DistTaskBindingVerification`（`Verified` / `Mismatch` / `CheckpointDigestMismatch` / `Unbound` / `Corrupt`）。
   - `DistTaskRecordBinding::to_checkpoint_entry` / `from_checkpoint_entry` 定义确定性二进制文法 `DTBINDV1 (8B) || kind (1B) || task_id (16B) || be64(task_generation) || record_id (16B) || be64(authority_term) || be64(control_epoch) || be64(participant_registry_generation) || participant_registry_root (32B) || (0u8 | 1u8 || exact_fence_set_root (32B))`（106B 或 138B），并在零代次/任期、未知类别、长度不符或非规范标志位时严格返回 `None`。
   - `DistTaskRecordBinding::binding_digest` 计算域分隔 `SHA-256("llmos/dist-task-migration-binding/v1" || to_checkpoint_entry())`；`MigrationObject::from_dist_task_binding` / `DistTaskRecordBinding::migration_object` 将前 16 字节确定性投影为 `MigrationObject`，同时保持磁盘 16 字节格式对非 Task `C-MIGRATE` 调用方兼容；`DistTaskRecordBinding::to_intent_reason` 编码为单行 `v1:dist-task-binding:<64 小写 hex>`（85B，位于 `<= 256B` 合同内，零磁盘格式破坏）。
   - 提供 `MigrationIntent::from_dist_task_binding`、`MigrationIntent::verify_dist_task_binding`、`CheckpointPrefix::from_dist_task_bindings`、`CheckpointPrefix::decode_dist_task_bindings`、`CheckpointFact::verify_dist_task_binding` 与顶层 `verify_dist_task_binding`，实现 `MigrationIntent` 与 `ReconciliationCheckpoint` 对 `DIST-TASK` 权威记录的端到端 fail-closed 验证。
   - 清理 `federation.rs` 模块头与 `MigrationObject` 文档注释中已过期的 Phase 0 deferred 措辞，反映 `CS0-B` + `CS1-D` 已落地的验证能力。

2. **`crates/nlos-slice-k/src/takeover_drive.rs` & `crates/nlos-slice-k/src/lib.rs`（生产级跨 Cell 接管推进器）**：
   - **生产传输边界裁决（[c-shard-plan.md](../../management/c-shard-plan.md) R2）**：明确 `BarrierObservationSource` + `drive_cross_cell_takeover` / `drive_cross_cell_takeover_with_options` 为跨进程接管的规范生产编排缝隙（transport-neutral production orchestrator seam），端点侧 `nlos-takeover-control` SABI handler 为同一签名存储门禁（`record_authority_takeover_barrier_receipt_signed` + `IdentityAuthority::verify_barrier_observation_signature`）上的本地/IPC 参考处理器；保持 `CrossCellTakeoverRequest` 与 `drive_cross_cell_takeover` 100% 向后兼容。
   - **签名 `ParticipantRegistryRecord` 快照验证**：新增 `PARTICIPANT_REGISTRY_SNAPSHOT_SIGNATURE_DOMAIN`（`b"llmos/participant-registry-snapshot-signature/v1"`）、`participant_registry_snapshot_signature_message`、`RegistrySnapshotSignature`、`verify_frozen_registry_signature` 与 `record_verified_participant_registry_signature`，通过 `nlos-identity` 验证 `BarrierObservationSigning` 密钥对冻结注册表快照的 Ed25519 签名并持久化 `v47` `(authority, signature)` 列，验证证明随 `CrossCellTakeoverOutcome::verified_registry_signer` 返回。
   - **`v47` `TaskAuthorityTakeoverReceipt` 扩展字段传递**：新增 `ReceiptExtensions`（`outstanding_effect_quarantine_root` + 被动 `consensus_commit_index`）与 `ProductionTakeoverOptions`（支持显式 `receipt_extensions` 或通过 `quarantined_permit_id` 从 `inspect_quarantine_receipt` 自动解析并交叉校验 `outstanding_effect_quarantine_root`），在 `prepare_authority_takeover_fence_with_receipt_extensions` 与完成态回读 `CrossCellTakeoverOutcome::takeover_receipt_completed` 上执行 fail-closed 一致性断言。
   - **Assignment 5 态推进（`Active` / `TakeoverPending` / `Fenced` / `Expired` / `Quarantined`）**：新增 `expire_authority_assignment`（执行锚定挂钟租约过期检查 `min(expired_at_ms, wall_now_ms) >= expires_at_ms`，`wall_now_ms == 0` fail-closed 视为仍存活）与 `quarantine_authority_assignment`；并在 `ProductionTakeoverOptions::quarantine_on_barrier_failure` 开启时落实 `[DIST-TASK-002]` 尾句——当 Leg 2 屏障观测收集或覆盖门禁失败时，自动将 `TakeoverPending` 旧权威 assignment 置为 `Quarantined`，且不激活任何新 assignment。
   - **Federation `MigrationIntent` + `ReconciliationCheckpoint` 绑定编排**：新增 `dist_task_binding_for_assignment`、`dist_task_binding_for_takeover_receipt`、`dist_task_binding_for_registry`、`TakeoverFederationContext`、`TakeoverFederationBindingOutcome`、`bind_takeover_to_federation` 与 `drive_takeover_with_federation_binding`，在接管完成后自动登记绑定 `MigrationIntent` 与 `CheckpointFact` 并立即执行 `verify_dist_task_binding` fail-closed 自检。

## 2. 验证

- `cargo fmt -p nlos-cell -p nlos-slice-k -- --check` → 退出码 `0`
- `cargo clippy -p nlos-cell -p nlos-slice-k --no-deps --all-targets -- -D warnings` → 退出码 `0`（零警告）
- `cargo test -p nlos-cell --no-fail-fast` → 退出码 `0`：
  - `tests/federation_digest.rs`：`8 passed; 0 failed`（含新增 `cs1d_dist_task_record_binding_is_deterministic_and_round_trips_checkpoint_entries`、`cs1d_migration_intent_and_checkpoint_binding_round_trip`、`cs1d_migration_intent_and_checkpoint_binding_fail_closed`）
  - `tests/federation.rs`：`9 passed; 0 failed`
  - `nlos-cell` 全套集成测试通过
- `cargo test -p nlos-slice-k --test dual_cell_takeover --test dual_cell_reconciliation --no-fail-fast` → 退出码 `0`：
  - `tests/dual_cell_takeover.rs`：`3 passed; 0 failed; 1 ignored`（含既有双进程 `successor_cell_drives_full_takeover_and_old_cell_is_fenced` 以及新增 `cs1d_production_takeover_verifies_signed_registry_receipt_extensions_and_federation_binding`、`cs1d_assignment_expired_and_quarantined_transitions_and_barrier_failure_quarantine`）
  - `tests/dual_cell_reconciliation.rs`：`8 passed; 0 failed; 1 ignored`

## 3. 边界与限制（诚实域）

- **单机多进程语义域（ADR-0018 & `RISK-B-11`）**：本证据基于同机共享内核、共享挂钟域与共享文件系统（SQLite WAL + `CellDirectory` 目录）；禁止将本证据引证为跨机网络、跨机时钟漂移或网络分区证据。
- **零共识协议边界（ADR-0018 Decision 5 & ADR-0020）**：`AuthorityTakeoverReceiptRecord.consensus_commit_index` 与 `ReceiptExtensions.consensus_commit_index` 仅为被动元数据槽位（passive metadata slot），携带**零** Raft 或多节点共识协议语义。
- **跨进程传输边界**：`BarrierObservationSource` 定义传输无关的观察请求/签名响应契约；单机双进程测试通过原子文件信令跨越 OS 进程边界，本地/IPC 控制面通过 `nlos-takeover-control` SABI handler 暴露同一套签名存储门禁。

## 4. 复现

```text
cargo test -p nlos-cell --test federation_digest -- --nocapture
cargo test -p nlos-slice-k --test dual_cell_takeover --test dual_cell_reconciliation -- --nocapture
```
