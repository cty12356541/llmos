# C-MIGRATE-001 / W63-CMIGRATE-PROC：跨 Cell Process 8 步迁移状态机与 Pinned Driver Session 策略（DIST-MIGRATE-001 & DIST-MIGRATE-002）

- 状态：`PASS`（`[DIST-MIGRATE-001]` 8 步严格有序 Process 迁移状态机、`ProcessMigrationReceipt` 域分隔 SHA-256 回执与 `[DIST-MIGRATE-002]` pinned/non-migratable Driver session 显式拒绝或降级 `restart-from-artifact` 策略已落地并验证）
- 日期：2026-10-24
- Owner：`C-MIGRATE`（`W63-CMIGRATE-PROC` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §26.3 `[DIST-MIGRATE-001]`（Process 迁移至少要求：`quiesce/cancel`、`checkpoint`、源 `fencing`、`Capability rebind`、`Lease close/regrant`、目标 `restore`、新 `generation` 和 `Receipt`）+ `[DIST-MIGRATE-002]`（`pinned/non-migratable Driver session` MUST 导致明确拒绝或降级为 `restart-from-artifact`，不得假装透明迁移）+ §12 `[LEASE-MIGRATE-001]`
- Base HEAD：`236ff8278711eef4db837ec7b972115af54a5811`

## 1. 实现事实

- `crates/nlos-process/src/migration.rs`（新增）与 `crates/nlos-process/src/lib.rs`（additive `pub mod migration;` 与导出，既有 SQLite 表结构、类型与函数签名零改动）：
  - **Session 可迁移性与准入策略（`[DIST-MIGRATE-002]`）**：
    1. `DriverSessionAttachment`：区分可序列化状态进入进程检查点的 `Migratable { session_id, state_digest }` 与绑定宿主硬件/局部句柄的 `PinnedNonMigratable { session_id, pin_reason, artifact_restart_image_digest: Option<[u8; 32]> }`。
    2. `PinnedSessionPolicy`：`RefuseMigration`（一旦存在任何 `PinnedNonMigratable` 会话立即以 `ProcessMigrationError::PinnedSessionNonMigratable { session_id, pin_reason }` fail-closed 拒绝迁移）与 `DowngradeToRestartFromArtifact`（显式降级为从制品重启）。
    3. `MigrationMode`：`TransparentCheckpointRestore` 与 `DowngradedRestartFromArtifact { artifact_image_digest: [u8; 32], dropped_pinned_sessions: Vec<[u8; 16]> }`。当策略为 `DowngradeToRestartFromArtifact` 时，若任一 pinned 会话缺失 `artifact_restart_image_digest`（`MissingArtifactRestartDigest`）或多个会话声明冲突摘要（`ConflictingArtifactRestartDigests`），均在 `Initiated` 阶段 fail-closed 拒绝；降级准入后若在 Step 3 试图提交 `FullStateCheckpoint` 伪装透明迁移，立即以 `CheckpointModeMismatch` fail-closed 拒绝。
  - **8 步严格有序 Process 迁移状态机（`[DIST-MIGRATE-001]`，`ProcessMigrationCoordinator` / `ProcessMigrationPlan`）**：
    1. `Initiated`：绑定 `migration_id`、`process_id`、`source_cell_id`、`target_cell_id`（禁止与源 Cell 相同）、`source_generation`、`target_generation`（强制 `target_generation == source_generation + 1`）与解析后的 `MigrationMode`。
    2. `Quiesced`（`record_quiesce`）：记录并校验 `QuiesceProof`（强制 `inflight_effects_drained == true`，否则 `SourceNotQuiesced`）。
    3. `Checkpointed`（`record_checkpoint`）：记录并校验 `ProcessCheckpointProof`（`FullStateCheckpoint` 或 `ArtifactRestartImage` 必须与 `MigrationMode` 及可迁移会话摘要严格匹配）。
    4. `SourceFenced`（`record_source_fence`）：记录并校验 `SourceProcessFenceProof`（强制 `source_cell_epoch > 0`、非零 `source_fencing_token` 且 `supervisor_unregistered == true`，确保源 Cell 旧世代不可恢复）。
    5. `CapabilitiesRebound`（`record_capability_rebind`）：记录并校验 `CapabilityRebindProof`，验证每条 `ReboundCapabilityEntry` 均从 `(source_cell_id, source_generation)` 重绑至 `(target_cell_id, target_generation)` 且附带非零 `attenuation_digest`。
    6. `LeasesRegranted`（`record_lease_close_regrant`）：记录并校验 `LeaseCloseRegrantProof`（强制每条 `MigratedLeaseEntry` 满足 `source_closed_or_fenced == true`、非零 `source_close_receipt_digest` 且新旧 Lease ID 相异，遵循 `[LEASE-MIGRATE-001]`）。
    7. `TargetRestored`（`record_target_restore`）：记录并校验 `TargetRestoreProof`（支持通过 `TargetRestoreProof::from_restored_binding` 从 `ProcessAuthority::restore_process` 产出的 `ProcessBindingRecord` 直接构造；强制 `restored_generation == target_generation`、目标 fencing token 与源 fencing token 严格相异、且恢复摘要与 Step 3 检查点/制品摘要逐字节一致）。
    8. `Completed`（`complete`）：封存全部步骤证明并签发不可变 `ProcessMigrationReceipt`，计算域分隔 SHA-256 摘要 `receipt_digest`（`PROCESS_MIGRATION_RECEIPT_DOMAIN = b"llmos/process-migration-receipt/v1"`），支持 `verify_digest()` 独立验真。
  - **幂等重放与中断/隔离语义（`abort_before_target_restore`）**：
    - 对已完成步骤重放完全一致的证明返回 `Ok(MigrationStepDecision::Replayed(step))`（`complete` 返回既有回执），重放冲突证明返回 `Err(ProcessMigrationError::ConflictingStepProof { step })`。
    - 在 `TargetRestored` 之前中止迁移时：若尚未执行 `SourceFenced`（处于 `Initiated` / `Quiesced` / `Checkpointed`），转移至 `ProcessMigrationStep::Aborted`（`MigrationAbortOutcome::AbortedPreFence`）；若已执行 `SourceFenced`（处于 `SourceFenced` / `CapabilitiesRebound` / `LeasesRegranted`），因源世代已被 fence 且目标尚未恢复，转移至 `ProcessMigrationStep::Quarantined`（`MigrationAbortOutcome::QuarantinedPostFence`）；在 `TargetRestored` 或 `Completed` 之后调用则以 `CannotAbortAfterTargetRestore` fail-closed 拒绝。

## 2. 验证

```text
cargo fmt -p nlos-process -- --check                     → 退出码 0
cargo clippy -p nlos-process --all-targets -- -D warnings → 退出码 0，零警告
cargo test -p nlos-process --no-fail-fast                → 退出码 0（2 unit + 77 integration = 79 passed / 0 failed）
```

新增集成测试 `crates/nlos-process/tests/process_migration.rs`（12 个具名测试）：

1. `transparent_checkpoint_restore_full_8_step_happy_path_and_deterministic_receipt`
2. `pinned_session_refuse_migration_policy_fails_closed_dist_migrate_002`
3. `pinned_session_downgrade_to_restart_from_artifact_requires_digest_and_forbids_transparent_checkpoint`
4. `out_of_order_step_bypass_attempts_fail_closed`
5. `invalid_initiation_parameters_and_wrong_target_generation_fail_closed`
6. `quiesce_and_source_fence_incomplete_proofs_fail_closed`
7. `capability_rebind_wrong_cell_or_generation_fails_closed`
8. `lease_regrant_without_source_lease_close_fails_closed_lease_migrate_001`
9. `target_restore_digest_mismatch_and_stale_fence_reuse_fail_closed`
10. `idempotent_step_replay_succeeds_and_conflicting_proof_replay_fails_closed`
11. `pre_fence_abort_transitions_to_aborted_and_blocks_further_steps`
12. `post_fence_abort_transitions_to_quarantined_and_abort_after_target_restore_is_rejected`

## 3. 边界与限制（诚实域）

- 本证据验证 `nlos-process` 内 `[DIST-MIGRATE-001]` 8 步迁移状态机、`ProcessAuthority::restore_process` 世代推进接线、`ProcessMigrationReceipt` 确定性摘要与 `[DIST-MIGRATE-002]` pinned Driver session 拒绝/降级规则。
- 不声称跨主机物理进程内存页实时热迁移（CRIU 级 OS 进程镜像传输）、跨进程 RPC 协调通道或与控制面 placement/federation 调度器的自动触发闭环已落地（遵循 ADR-0018 与 `RISK-B-11` 诚实边界）。

## 4. 复现

```text
cargo test -p nlos-process --test process_migration -- --nocapture
```
