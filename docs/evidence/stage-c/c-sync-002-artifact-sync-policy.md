# C-SYNC-002 / W62-CSYNC-ART：Artifact 按内容类型声明 merge/conflict/authoritative replica/deletion 同步策略与原语（DIST-ART-001）

- 状态：`PASS`（`[DIST-ART-001]` 内容类型四轴同步策略注册表、确定性评估器 `evaluate_artifact_sync` 与双 `ArtifactStore` 副本同步原语 `sync_artifact_replica_head` 已落地并验证）
- 日期：2026-10-24
- Owner：`C-SYNC`（`W62-CSYNC-ART` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §14.1（`[ART-VERSION-001]` 不可变 revision/digest 绑定、`[ART-LOCAL-001]` 显式冲突与失败语义）+ §26.2 `[DIST-ART-001]`（Artifact 同步 MUST 为每种内容类型声明 merge、conflict、authoritative replica 和 deletion semantics）
- Base HEAD：`5a04be6d319143ce7280a64630cce2045bbf60d8`

## 1. 实现事实

- `crates/nlos-artifact/src/sync.rs`（新增）与 `crates/nlos-artifact/src/lib.rs`（additive `pub mod sync;` 与导出，既有表结构、类型与函数签名零改动）：
  - **四轴内容类型策略模型（`[DIST-ART-001]`）**：
    1. `ArtifactMergeStrategy`：`ImmutableContentAddressed`（按不可变 revision 槽位与内容寻址摘要校验，仅允许幂等重放或紧邻 `local_rev + 1` 且父摘要匹配的推进）、`FastForwardLineageOnly`（仅当远端祖先谱系包含本地 head `(revision, digest)` 时允许 fast-forward，并发分支禁止自动合并）、`AuthoritativeReplicaWins`（接受声明的唯一权威 Cell 签发/起源的 head）。
    2. `ArtifactConflictStrategy`：`FailClosedConflict`（拒绝推进 head 并产出确定性 `ArtifactConflictReceipt`）、`QuarantineDivergentBranch`（将本地与远端分叉 head 记录入隔离冲突回执，等待显式人工/控制器裁决，不推进 canonical head）、`DeferToAuthoritativeReplica`（由唯一权威副本裁决；仅允许与 `SingleAuthoritativeCell` 组合，与 `SymmetricPeer` 组合在注册期即 fail-closed 拒绝）。
    3. `ArtifactReplicaAuthority`：`SymmetricPeer`、`SingleAuthoritativeCell { authoritative_cell_id: [u8; 16] }`（全零 cell ID 在注册期 fail-closed 拒绝）。
    4. `ArtifactDeletionSemantics`：`Forbidden`（任何删除同步一律 fail-closed 拒绝）、`TombstoneRevisionRequired`（删除必须表现为推进 `head_revision + 1` 的显式 tombstone revision，禁止静默无版本文件删除或直接物理 GC）、`GcAfterRetentionBarrier`（仅在 `retention_expired == true && active_ref_count == 0` 屏障满足后允许物理回收）。
  - **确定性冲突回执（`ArtifactConflictReceipt` / `compute_conflict_digest`）**：
    - 采用域分隔 SHA-256（`ARTIFACT_CONFLICT_RECEIPT_DOMAIN = b"llmos/artifact-conflict-receipt/v1"`），覆盖 `(artifact_id, be64(content_type.len()) || content_type, canonical_min_max((local_rev, local_digest), (remote_rev, remote_digest)))`，保证两端副本以任意顺序观测同一并发分叉时生成逐字节一致的 `conflict_digest`。
  - **纯确定性评估器与双店同步原语**：
    - `evaluate_artifact_sync`：对未注册 `content_type`、同 revision 不同摘要损坏（`CorruptRevisionDigestMismatch`）、非紧邻跳号（`NonContiguousRevision`）、非权威副本越权写入（`NonAuthoritativeReplicaRejected`）及违反删除策略的操作全部显式 fail-closed。
    - `export_replica_revision_bundle` + `sync_artifact_replica_head`：在两个 `ArtifactStore` 实例之间导出已验证 revision 及其祖先谱系，先校验 `content_type` 策略注册与远端字节 SHA-256 摘要，再通过 `stage_revision` + `publish_staged_revision` 原子落盘并生成 `OwnerDerived` provenance 回执；分叉冲突时返回 `RefusedConflict` / `QuarantinedConflict` 且本地 canonical head 与字节保持零污染。

## 2. 验证

```text
cargo fmt -p nlos-artifact -- --check                     → 退出码 0
cargo clippy -p nlos-artifact --all-targets -- -D warnings → 退出码 0，零警告
cargo test -p nlos-artifact --no-fail-fast                → 退出码 0（23 unit + 76 integration = 99 passed / 0 failed）
```

新增集成测试 `crates/nlos-artifact/tests/artifact_sync_policy.rs`（10 个具名测试）：

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

## 3. 边界与限制（诚实域）

- 本证据验证单进程内 `[DIST-ART-001]` 四轴内容类型同步策略注册表、确定性评估器及双 `ArtifactStore` 实例间的 staged/published 副本同步闭环。
- 不声称跨主机网络传输层（RPC/QUIC 复制通道）、跨机器拜占庭容错或自动后台反熵守护进程已落地（遵循 ADR-0018 与 `RISK-B-11` 诚实边界）。

## 4. 复现

```text
cargo test -p nlos-artifact --test artifact_sync_policy -- --nocapture
```
