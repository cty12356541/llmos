# C-SYNC-002 / W62-CSYNC-ART + W64-CART-REPLICA：Artifact 按内容类型同步策略、跨 Store 传输包闭环与同步感知 GC 保留守卫（DIST-ART-001）

- 状态：`PASS`（`[DIST-ART-001]` 内容类型四轴同步策略注册表、确定性评估器 `evaluate_artifact_sync`、跨 `ArtifactStore` 传输包 `ArtifactTransferPackage` 导出/导入闭环及同步感知 GC 根保护 `collect_orphan_blobs_with_sync_roots` 已落地并验证）
- 日期：2026-10-25（初始 `W62-CSYNC-ART`：2026-10-24；跨店闭环补齐 `W64-CART-REPLICA`：2026-10-25）
- Owner：`C-SYNC`（`W62-CSYNC-ART` + `W64-CART-REPLICA` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §14.1（`[ART-VERSION-001]` 不可变 revision/digest 绑定、`[ART-LOCAL-001]` 显式冲突与失败语义）+ §26.2 `[DIST-ART-001]`（Artifact 同步 MUST 为每种内容类型声明 merge、conflict、authoritative replica 和 deletion semantics，跨副本传输校验 digest、隔离冲突保留双端 blob、墓碑与保留窗口约束 GC）
- Base HEAD：`3a91098a7557a987128b29a4eaf8490d1277a65b`（初始 `W62-CSYNC-ART` Base：`5a04be6d319143ce7280a64630cce2045bbf60d8`）

## 1. 实现事实

- `crates/nlos-artifact/src/sync.rs` 与 `crates/nlos-artifact/src/lib.rs`（纯增量类型与方法导出，`SCHEMA_VERSION = 8` 及既有 SQLite 表结构零改动）：
  - **四轴内容类型策略模型（`[DIST-ART-001]`，`W62-CSYNC-ART`）**：
    1. `ArtifactMergeStrategy`：`ImmutableContentAddressed`（按不可变 revision 槽位与内容寻址摘要校验，仅允许幂等重放或紧邻 `local_rev + 1` 且父摘要匹配的推进）、`FastForwardLineageOnly`（仅当远端祖先谱系包含本地 head `(revision, digest)` 时允许 fast-forward，并发分支禁止自动合并）、`AuthoritativeReplicaWins`（接受声明的唯一权威 Cell 签发/起源的 head）。
    2. `ArtifactConflictStrategy`：`FailClosedConflict`（拒绝推进 head 并产出确定性 `ArtifactConflictReceipt`）、`QuarantineDivergentBranch`（将本地与远端分叉 head 记录入隔离冲突回执，等待显式人工/控制器裁决，不推进 canonical head）、`DeferToAuthoritativeReplica`（由唯一权威副本裁决；仅允许与 `SingleAuthoritativeCell` 组合，与 `SymmetricPeer` 组合在注册期即 fail-closed 拒绝）。
    3. `ArtifactReplicaAuthority`：`SymmetricPeer`、`SingleAuthoritativeCell { authoritative_cell_id: [u8; 16] }`（全零 cell ID 在注册期 fail-closed 拒绝）。
    4. `ArtifactDeletionSemantics`：`Forbidden`（任何删除同步一律 fail-closed 拒绝）、`TombstoneRevisionRequired`（删除必须表现为推进 `head_revision + 1` 的显式 tombstone revision，禁止静默无版本文件删除或直接物理 GC）、`GcAfterRetentionBarrier`（仅在 `retention_expired == true && active_ref_count == 0` 屏障满足后允许物理回收）。
  - **确定性冲突回执（`ArtifactConflictReceipt` / `compute_conflict_digest`）**：
    - 采用域分隔 SHA-256（`ARTIFACT_CONFLICT_RECEIPT_DOMAIN = b"llmos/artifact-conflict-receipt/v1"`），覆盖 `(artifact_id, be64(content_type.len()) || content_type, canonical_min_max((local_rev, local_digest), (remote_rev, remote_digest)))`，保证两端副本以任意顺序观测同一并发分叉时生成逐字节一致的 `conflict_digest`。
  - **纯确定性评估器与单跳双店同步原语（`W62-CSYNC-ART`）**：
    - `evaluate_artifact_sync`：对未注册 `content_type`、同 revision 不同摘要损坏（`CorruptRevisionDigestMismatch`）、非紧邻跳号（`NonContiguousRevision`）、非权威副本越权写入（`NonAuthoritativeReplicaRejected`）及违反删除策略的操作全部显式 fail-closed。
    - `export_replica_revision_bundle` + `sync_artifact_replica_head`：在两个 `ArtifactStore` 实例之间导出已验证单跳 revision 及其祖先谱系并原子 staged/published 落盘。
  - **跨 Store 内容寻址传输包与完整谱系导入/导出闭环（`W64-CART-REPLICA`）**：
    - `ArtifactBlobPayload { digest, bytes }`、`TransferRevisionEntry { head, provenance_source_kind }`、`ArtifactTombstoneMetadata { deleted_at_ms, retention_until_ms }` 与 `ArtifactTransferPackage`：采用域分隔 SHA-256（`ARTIFACT_TRANSFER_PACKAGE_DOMAIN = b"llmos/artifact-transfer-package/v1"`，`compute_transfer_package_digest`）对 `bundle`、`producer_epoch`、`artifact_created_at_ms`、连续 revision 列表、全部内容寻址 blob 负载及可选墓碑元数据计算确定性 `package_digest: [u8; 32]`。
    - `export_artifact_transfer_package`（及 `ArtifactStore::export_artifact_transfer_package`）：从源 `ArtifactStore` 验证并导出 `1..=target_revision` 完整前缀谱系与去重排序的 `ArtifactBlobPayload` 列表，支持常规 head（`ExportArtifactTransferRequest::head`）与显式墓碑 head（`ExportArtifactTransferRequest::tombstone_head`）。
    - `import_artifact_transfer_package`（及 `ArtifactStore::import_artifact_transfer_package`）返回 `CrossStoreArtifactSyncReport`：
      - **预物化全内存验签（Fail-Closed Zero-Write）**：在任何磁盘 `.tmp` / blob 写入或 SQLite 事务开始前，先逐一校验每个 `ArtifactBlobPayload` 的内容寻址 SHA-256（篡改即返回 `ArtifactSyncError::TamperedBlobDigest`）与域分隔 `package_digest`（元数据或负载替换篡改即返回 `ArtifactSyncError::TamperedPackageDigest`），被篡改包在目标 Store 上产生零磁盘残留、零 SQLite 行写入。
      - **多跳 Fast-Forward 与幂等收敛**：支持可选 `expected_local_state` CAS 检查（漂移时返回 `ArtifactSyncError::LocalHeadStateDrift`）；缺失的连续前缀 revisions 经 `stage_revision` + `publish_staged_revision` 顺序原子物化并绑定 `OwnerDerived` provenance 回执，返回 `ImportedFastForward`；重复导入同一已收敛传输包返回 `NoOpConverged`。
      - **分叉隔离双端 Blob 保留**：当策略产出 `QuarantinedConflict` 或 `RefusedConflict` 时，本地 canonical head 保持不变，同时将远端竞争分支 blob 经摘要校验写入内容寻址存储并登记到 `ReplicaSyncRetentionSet`，返回 `ConflictIsolated { conflict_receipt, preserved_both_blobs: true, .. }`，可通过 `read_retained_sync_blob` 安全读取待裁决远端负载。
      - **显式墓碑与保留窗口传播**：携带 `ArtifactTombstoneMetadata` 的传输包先经策略删除语义门禁（如 `Forbidden` 直接拒绝为 `DeletionForbidden`），通过后物化 tombstone revision、在目标 Store 持久化 `set_retention` 策略并将关联摘要钉入墓碑保留集，返回 `TombstoneApplied`。
  - **同步感知 GC 保留根保护（`ReplicaSyncRetentionSet` + `collect_orphan_blobs_with_sync_roots`，`W64-CART-REPLICA`）**：
    - `ReplicaSyncRetentionSet`：显式维护未裁决冲突钉住集（`QuarantinedConflictPin`）与墓碑保留窗口钉住集（`TombstoneRetentionPin`），支持 `resolve_conflict`、`retire_conflict_into_tombstone_retention`、`evaluate_tombstone_reclamation` 与 `prune_expired_tombstones`。
    - `collect_orphan_blobs_with_sync_roots`（及 `ArtifactStore::collect_orphan_blobs_with_sync_roots`）：在不修改 SQLite schema 的前提下复用 `artifact_gc_receipts` 幂等回执事务，将 `artifact_revisions` + `staged_revisions` 引用集与 `ReplicaSyncRetentionSet::pinned_digests_at(collected_at_ms)` 并集作为存活根——仅清理真正无引用的孤儿 blob，确保隔离冲突双端 blob 在冲突解除前不被 GC 回收，且墓碑关联 blob 在 `retention_until_ms` 到期前保持存活、到期后可被正常回收。

## 2. 验证

```text
cargo fmt -p nlos-artifact -- --check                              → 退出码 0
cargo clippy -p nlos-artifact --no-deps --all-targets -- -D warnings → 退出码 0，零警告
cargo test -p nlos-artifact --no-fail-fast                         → 退出码 0（23 unit + 83 integration = 106 passed / 0 failed）
```

集成测试套件 `crates/nlos-artifact/tests/artifact_sync_policy.rs`（共 17 个具名测试，17 passed / 0 failed）：

1. `undeclared_content_type_and_invalid_policy_combinations_fail_closed`
2. `immutable_content_addressed_happy_path_replay_and_corrupt_digest_mismatch`
3. `fast_forward_lineage_happy_path_replay_and_corrupt_digest_mismatch`
4. `divergent_branches_emit_order_invariant_fail_closed_conflict_receipts`
5. `divergent_branches_emit_order_invariant_quarantine_conflict_receipts`
6. `single_authoritative_cell_accepts_authoritative_and_rejects_non_authoritative_divergent_heads`
7. `deletion_semantics_forbidden_and_tombstone_revision_required_enforced`
8. `deletion_semantics_gc_after_retention_barrier_enforced`
9. `end_to_end_two_store_replica_sync_stages_and_publishes_verified_revisions`
10. `end_to_end_two_store_divergent_heads_preserve_local_canonical_head`
11. `transfer_package_multi_revision_export_import_and_idempotent_reimport`（`W64-CART-REPLICA`：跨双独立 Store 的 `v1..=v3` 完整谱系与多跳增量 `v4` 导出/导入及幂等重放 `NoOpConverged`）
12. `transfer_package_tampered_blob_and_package_digest_fail_closed_with_zero_writes`（`W64-CART-REPLICA`：blob 字节篡改 `TamperedBlobDigest` 与包级元数据篡改 `TamperedPackageDigest` 均在落盘前 fail-closed，验证零文件/零表行污染）
13. `transfer_package_quarantine_conflict_preserves_both_blobs_across_sync_aware_gc`（`W64-CART-REPLICA`：`QuarantineDivergentBranch` 分叉导入产出 `ConflictIsolated` 并保留双端 blob，同步感知 GC 仅回收真实孤儿 blob，冲突裁决后释放远端未采纳分支 blob）
14. `transfer_package_fail_closed_conflict_receipt_also_guards_competing_blobs_until_resolved`（`W64-CART-REPLICA`：`FailClosedConflict` 双向导入产出一致 `conflict_digest` 并在同步感知 GC 下保护竞争分支 blob）
15. `transfer_package_tombstone_with_retention_propagates_and_enforces_expiry_window`（`W64-CART-REPLICA`：显式墓碑 revision 与 `retention_until_ms` 跨 Store 传播、窗口内可读且拒绝提前回收、窗口过期后读取 fail-closed 且允许 `ReclaimEligible`）
16. `tombstone_retention_gc_guard_retains_superseded_sync_blobs_until_window_expires`（`W64-CART-REPLICA`：转入墓碑保留窗口的旧同步分支 blob 在到期前受 `collect_orphan_blobs_with_sync_roots` 保护，到期后物理回收）
17. `transfer_package_single_authoritative_cell_and_expected_local_state_cas_enforced`（`W64-CART-REPLICA`：`SingleAuthoritativeCell` 拒绝非权威来源传输包、`expected_local_state` CAS 拦截本地 head 漂移、`Forbidden` 删除策略拒绝墓碑传输包）

## 3. 边界与限制（诚实域）

- 本证据验证两个独立本地文件系统目录上的 `ArtifactStore` 实例之间通过 `ArtifactTransferPackage` 完成 `[DIST-ART-001]` 内容类型策略校验、多 revision 内容寻址导出/导入、篡改零写入拒绝、冲突双端 blob 隔离保留、墓碑保留期传播及同步感知 GC 根保护闭环。
- `ReplicaSyncRetentionSet` 当前为显式传入 `import_artifact_transfer_package` 与 `collect_orphan_blobs_with_sync_roots` 的内存态同步保留根集合（保持 `nlos-artifact` `SCHEMA_VERSION = 8` 零 schema 迁移）。
- 不声称跨主机网络传输层（RPC/QUIC 复制通道）、跨机器拜占庭容错或自动后台反熵守护进程已落地（遵循 ADR-0018 与 `RISK-B-11` 诚实边界）。

## 4. 复现

```text
cargo test -p nlos-artifact --test artifact_sync_policy -- --nocapture
```
