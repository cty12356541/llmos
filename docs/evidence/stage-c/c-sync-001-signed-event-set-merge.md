# C-SYNC-001 / W62-CSYNC-SEM + W63-CSYNC-APPLY：签名 Semantic 事件集合存储导出、跨域持久化收敛与 SEM-CHECKPOINT-001 签名向量检查点

- 状态：`PASS`（`[DIST-EVENT-001]` 签名 Semantic 事件集合导出/合并 + `[DIST-AUDIT-001]` 跨域审计锚点绑定与持久化写回 + `[SEM-CHECKPOINT-001]` 签名多域向量检查点与因果水位比较的本地双存储库级证据；**非**跨机网络传输证据，**非** `[DIST-ART-001]` Artifact 同步证据）
- 日期：2026-10-24
- Owner：`C-SYNC` 轨道（`W45-D1` 基础原语 `4826b6c8` + `W62-CSYNC-SEM` 存储导出与 `DIST-AUDIT-001` 跨域审计锚点闭环 + `W63-CSYNC-APPLY` 跨域持久化收敛与 `SEM-CHECKPOINT-001` 签名向量检查点）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §26.2 `[DIST-EVENT-001]`、`[DIST-AUDIT-001]`；§17.6 `[SEM-CHECKPOINT-001]`；§26.1 `[CONS-SCOPE-001]`
- 基线：`236ff8278711eef4db837ec7b972115af54a5811`（前置 `W62-CSYNC-SEM` `5a04be6d`、`W45-D1` `4826b6c8`）

## 1. 实现事实

- `crates/nlos-semantic/src/lib.rs`：
  - 新增只读存储导出门 `SemanticAuthority::export_signed_event_set(&self) -> Result<Vec<SignedEventSetItem>, SemanticAuthorityError>`，按本地 `event_log.log_seq ASC` 确定性读取全部已提交 `SemanticEventRecord`（覆盖 Assertion、`IntentSpec`、Judgment/Verification/Retraction 全事件类型）与对应 `AdmissionReceipt`，并在导出时核验 `event_signatures` 生产者签名行、`event_id` 与 `log_seq` 内部一致性；
  - 新增 `SemanticAuthority::inspect_event_signature(&self, event_id)` 与 `SemanticAuthority::export_cross_domain_event_set(&self, trust_snapshot_digest)`，从 SQLite `event_signatures` + `admission_receipts` + `semantic_events` 直接构造带完整 `[DIST-AUDIT-001]` 三元锚点的 `CrossDomainEventItem` 集合，保持既有 SQLite schema（`SCHEMA_VERSION = 9`）不变；
  - 新增 `[SEM-CHECKPOINT-001]` 向量检查点导出与签名门 `SemanticAuthority::export_vector_checkpoint(&self, trust_snapshot_digest)` 与 `SemanticAuthority::export_signed_vector_checkpoint(&self, identity, store_signer, trust_snapshot_digest, signed_at_ms)`，在导出前通过 `IdentityAuthority` 自验签；
  - 新增跨域持久化收敛门 `SemanticAuthority::import_cross_domain_event_set` 与 `SemanticAuthority::converge_cross_domain_event_sets`：
    1. 导出接收端当前已提交的跨域事件集，并与远端输入集合统一调用 `verify_and_merge_cross_domain_events` 执行 `[DIST-EVENT-001]` + `[DIST-AUDIT-001]` 验签与冲突检测，任何拒绝或冲突在开启写事务前即以 `SemanticAuthorityError::CrossDomainMergeRejected` fail-closed 中止；
    2. 在单个 SQLite `Immediate` 原子事务内对尚未入库的远端事件按拓扑依赖顺序（`declared_parents`、`captured_inputs`、Judgment 端点、Verification 目标、Retraction 目标优先，以 `(admitted_at_ms, event_id)` 确定性消歧）校验血统与引用目标后持久化 canonical 事件、源生产者签名、接收域签名的 `AdmissionReceipt` 以及绑定 `[DIST-AUDIT-001]` 溯源承诺摘要（`compute_cross_domain_audit_commitment_digest`）的接收域 `DurabilityReceipt`，已存在事件经 canonical 字节与生产者签名比对后幂等跳过；任何中途血统悬空或载荷冲突均原子回滚整个 SQLite 事务。
- `crates/nlos-semantic/src/merge.rs`：
  - 在 `W45-D1` 纯集合并集原语 `merge_signed_event_sets` 基础上，提供 `[DIST-AUDIT-001]` 跨域审计载体 `CrossDomainEventItem`、`CrossDomainAuditRecord`、`MergedCrossDomainEvent`、`CrossDomainEventSetMergeReport`、`CrossDomainConvergenceReport` 以及 `merge_cross_domain_event_sets` / `verify_and_merge_cross_domain_events`；
  - 新增 `[SEM-CHECKPOINT-001]` 多域向量检查点类型与算法：
    - `DomainSemanticWaterMark`（别名 `CellSemanticCursor`）：按源 `authority_id`（`ControlDomainId`）严格升序排列，记录 `(authority_id, event_count, event_id_merkle_root, latest_admission_receipt_digest, trust_snapshot_digest)`，完全不依赖任何跨存储或存储本地标量 `log_seq`；
    - `SemanticVectorCheckpoint` 与 `SignedSemanticVectorCheckpoint`：包含 `checkpoint_id`、`domain_watermarks`、`covered_event_ids_root`、域分离 SHA-256 `checkpoint_digest`（`"llmos/semantic-vector-checkpoint/v1"`）及存储权威 Ed25519 签名元数据（`"llmos/semantic-vector-checkpoint-sig/v1"`）；
    - `verify_vector_checkpoint` 与 `verify_signed_vector_checkpoint`：对全零 `trust_snapshot_digest`、非升序/重复 `authority_id`、零计数、空 Merkle 根、零接纳摘要、`covered_event_ids_root` 失配、`checkpoint_digest`/`checkpoint_id` 篡改、签名者身份失配与 Ed25519 签名篡改执行全量 fail-closed 拒绝；
    - `diff_vector_checkpoints(local, remote) -> VectorCheckpointDiff`：在不依赖任何标量 `log_seq` 的前提下计算跨源域与同源域因果关系 `VectorCheckpointRelation::{Equal, LocalAhead, RemoteAhead, Concurrent}`；
  - 严格执行 `[DIST-AUDIT-001]` 与 fail-closed 冲突隔离：
    - 全零 `trust_snapshot_digest` 拒绝为 `SignedEventSetRejection::MissingTrustSnapshot`；
    - 全零或 `IdentityAuthority` 验签失败的 `producer_signature` 拒绝为 `SignedEventSetRejection::InvalidProducerSignature`；
    - 同一 `event_id` 跨接收域出现不一致 `producer_signature` 时，记录 `SignedEventSetConflict::ProducerSignatureMismatch` 与 `SignedEventSetRejection::ProducerSignatureConflict` 并整条排除；
    - 同一 `receipt_id` 携带冲突 `trust_snapshot_digest` 时记录 `SignedEventSetConflict::TrustSnapshotConflict` 并排除对应观测。

## 2. 验证（2026-10-24，Windows x86_64）

```text
cargo fmt -p nlos-semantic -- --check                         → 退出码 0
cargo clippy -p nlos-semantic --all-targets -- -D warnings    → 退出码 0，零警告
cargo test -p nlos-semantic --test signed_event_set_merge     → 23 passed / 0 failed
cargo test -p nlos-semantic --no-fail-fast                    → 72 passed / 0 failed（8 测试目标）
```

证据测试 `crates/nlos-semantic/tests/signed_event_set_merge.rs`（23 项测试）断言面：

1. **双 Cell 跨域持久化收敛与顺序无关签名向量检查点（`[SEM-CHECKPOINT-001]` + `[DIST-EVENT-001]` + `[DIST-AUDIT-001]`）**：
   - `sem_checkpoint_001_two_cell_durable_convergence_and_order_independent_vector_checkpoint`：两个独立 SQLite `SemanticAuthority` 存储（Cell A 与 Cell B）分别接纳跨两个源 `ControlDomainId` 的 Assertion、`IntentSpec`、Judgment 与带血统子事件，以相反方向执行 `import_cross_domain_event_set` / `converge_cross_domain_event_sets` 持久化收敛；断言收敛后两存储虽具有不同的本地 `log_seq` 顺序，但导出的 `SignedSemanticVectorCheckpoint` 具有完全一致的 `domain_watermarks`、`covered_event_ids_root`、`checkpoint_digest` 与 `checkpoint_id`，每条新导入事件生成的接收域 `DurabilityReceipt` 均通过 `verify_durability_receipt` 验签并精确绑定 `compute_cross_domain_audit_commitment_digest`，且重复导入 100% 幂等；
   - `sem_checkpoint_001_verify_signed_vector_checkpoint_rejects_watermark_tampering_fail_closed`：断言全零 `trust_snapshot_digest`、逆序 `domain_watermarks`、零 `event_count`、空 `event_id_merkle_root`、零 `latest_admission_receipt_digest` 以及篡改水位后重算摘要的伪造检查点全部被 fail-closed 拒绝；
   - `sem_checkpoint_001_verify_signed_vector_checkpoint_rejects_root_digest_and_signer_tampering`：断言篡改 `covered_event_ids_root`、`checkpoint_digest`、`checkpoint_id`、全零签名者公钥、伪造 `signer_authority_id` 或篡改 Ed25519 `signature` 全部被 fail-closed 拒绝；
   - `sem_checkpoint_001_diff_vector_checkpoints_causal_relations_without_scalar_log_seq`：断言 `diff_vector_checkpoints` 在不同本地 `log_seq` 下精确判定 `Equal`、`LocalAhead`、`RemoteAhead` 与 `Concurrent`（含同源域等长分叉检测），并验证 `CellSemanticCursor` 类型别名等价性；
   - `c_sync_import_rejects_corrupt_dist_audit_001_envelope_without_polluting_store`：断言批次内含全零 `trust_snapshot_digest` 或伪造 `producer_signature` 时，`import_cross_domain_event_set` 在写库前以 `CrossDomainMergeRejected` fail-closed 拒绝，接收端 SQLite 保持零污染；
   - `c_sync_import_atomic_rollback_on_mid_batch_dangling_lineage_or_missing_target`：断言批次内合法事件与悬空血统事件混合导入时触发 `DanglingLineage` 并完整回滚 SQLite 事务，不残留任何半提交事件。
2. **双域存储导出与全事件类型集合并集（`[DIST-EVENT-001]` + `[SEM-CHECKPOINT-001]`）**：
   - `store_backed_export_signed_event_set_across_event_types_and_idempotent_merge`、`two_domains_same_event_merges_with_both_admissions`、`disjoint_events_union_sorted_by_event_id`、`identical_items_across_sets_dedup_idempotently`、`same_content_different_nonce_events_merge_separately`、`empty_inputs_and_argument_order_independence`。
3. **跨域审计锚点保留与密码学验签（`[DIST-AUDIT-001]`）**：
   - `dist_audit_001_cross_domain_export_and_verified_merge_preserves_provenance`、`dist_audit_001_rejects_missing_or_zero_trust_snapshot_fail_closed`、`dist_audit_001_rejects_invalid_or_zero_producer_signature_fail_closed`、`dist_audit_001_rejects_conflicting_producer_signatures_across_domains_fail_closed`、`dist_audit_001_rejects_conflicting_trust_snapshots_for_same_receipt_id`。
4. **单条内禀完整性拒绝与接收域冲突隔离**：
   - `rejects_event_id_canonical_mismatch`、`rejects_admission_event_binding_mismatch`、`rejects_admission_log_seq_binding_mismatch`、`rejects_admission_receipt_id_derivation_mismatch`、`receipt_content_conflict_excludes_event_without_other_admission`、`duplicate_store_admission_conflict_keeps_other_stores`。

## 3. 边界与限制（诚实域，`EVID-SCOPE-001`）

- **单声明范围**：本证据覆盖 `C-SYNC` 轨道下的 `[DIST-EVENT-001]` 签名 Semantic 事件集合存储导出/合并、`[DIST-AUDIT-001]` 跨域审计三元锚点校验与持久化写回、以及 `[SEM-CHECKPOINT-001]` 签名多域向量检查点导出/验签/因果水位差集比较。
- **非跨机网络传输证据**：测试在同一宿主机上的多个独立 SQLite `SemanticAuthority` 存储目录间执行跨域事件集合导出、持久化导入收敛与签名向量检查点交互；不包含跨进程/跨主机网络传输会话或在线增量流式拉取守护进程。
- **非 `[DIST-ART-001]` 证据**：Artifact 同步的按内容类型 merge / conflict / authoritative replica / deletion 语义由独立的 `C-SYNC` Artifact 同步车道（`nlos-artifact`）承载，不在本证据声明范围内。
- **不自动创造语义判断**：合并与导入层遵循 `[DIST-EVENT-001]`，不消解 `CONTRADICTS` Judgment、不过滤已撤回（Retracted）或带 Taint 的事件，所有信任裁决仍归属上游 `TrustView` / `AuthorityView`。

## 4. 复现

```text
cargo test -p nlos-semantic --test signed_event_set_merge -- --nocapture
```
