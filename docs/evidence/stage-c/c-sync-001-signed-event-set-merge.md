# C-SYNC-001 / W62-CSYNC-SEM：签名 Semantic 事件集合存储导出合并与 DIST-AUDIT-001 跨域审计锚点保留

- 状态：`PASS`（`[DIST-EVENT-001]` 签名 Semantic 事件集合导出/合并 + `[DIST-AUDIT-001]` 跨域生产者签名 / 接收域 `AdmissionReceipt` / `trust_snapshot_digest` 绑定与 fail-closed 验签的本地双存储库级证据；**非**跨机网络传输证据，**非** `[DIST-ART-001]` Artifact 同步证据）
- 日期：2026-10-24
- Owner：`C-SYNC` 轨道（`W45-D1` 基础原语 `4826b6c8` + `W62-CSYNC-SEM` 存储导出与 `DIST-AUDIT-001` 跨域审计锚点闭环）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §26.2 `[DIST-EVENT-001]`、`[DIST-AUDIT-001]`；§17.6 `[SEM-CHECKPOINT-001]`
- 基线：`5a04be6d319143ce7280a64630cce2045bbf60d8`（`W45-D1` 前置提交 `4826b6c8`）

## 1. 实现事实

- `crates/nlos-semantic/src/lib.rs`：
  - 新增只读存储导出门 `SemanticAuthority::export_signed_event_set(&self) -> Result<Vec<SignedEventSetItem>, SemanticAuthorityError>`，按本地 `event_log.log_seq ASC` 确定性读取全部已提交 `SemanticEventRecord`（覆盖 Assertion、`IntentSpec`、Judgment/Verification/Retraction 全事件类型）与对应 `AdmissionReceipt`，并在导出时核验 `event_signatures` 生产者签名行、`event_id` 与 `log_seq` 内部一致性；
  - 新增 `SemanticAuthority::inspect_event_signature(&self, event_id)` 与 `SemanticAuthority::export_cross_domain_event_set(&self, trust_snapshot_digest)`，从 SQLite `event_signatures` + `admission_receipts` + `semantic_events` 直接构造带完整 `[DIST-AUDIT-001]` 三元锚点的 `CrossDomainEventItem` 集合，保持既有 SQLite schema 不变。
- `crates/nlos-semantic/src/merge.rs`：
  - 在 `W45-D1` 纯集合并集原语 `merge_signed_event_sets` 基础上，新增 `[DIST-AUDIT-001]` 跨域审计载体 `CrossDomainEventItem`、`CrossDomainAuditRecord`、`MergedCrossDomainEvent`、`CrossDomainEventSetMergeReport` 以及 `merge_cross_domain_event_sets` / `verify_and_merge_cross_domain_events`；
  - 严格执行 `[DIST-AUDIT-001]`：每条合并存活的跨域观测都在 `MergedCrossDomainEvent::audit_records` 中保留源生产者签名（`producer_key_id`、`producer_signature`）、接收域 `AdmissionReceipt` 与适用非零 `trust_snapshot_digest`；
  - 严格执行 fail-closed 拒绝与冲突隔离：
    - 全零 `trust_snapshot_digest` 拒绝为 `SignedEventSetRejection::MissingTrustSnapshot`；
    - 全零或 `IdentityAuthority` 验签失败的 `producer_signature` 拒绝为 `SignedEventSetRejection::InvalidProducerSignature`；
    - 同一 `event_id` 跨接收域出现不一致 `producer_signature` 时，不偏袒任一观测，记录 `SignedEventSetConflict::ProducerSignatureMismatch` 与 `SignedEventSetRejection::ProducerSignatureConflict` 并整条排除；
    - 同一 `receipt_id` 携带冲突 `trust_snapshot_digest` 时记录 `SignedEventSetConflict::TrustSnapshotConflict` 并排除对应观测；
  - 遵守 `[SEM-CHECKPOINT-001]`：`MergedSignedEvent` 与 `MergedCrossDomainEvent` 均不暴露跨存储全局标量 `log_seq`，各接收域 `AdmissionReceipt` 保留各自本地 `log_seq` 与存储签名三元组。

## 2. 验证（2026-10-24，Windows x86_64）

```text
cargo fmt --all -- --check                                    → 退出码 0
cargo clippy -p nlos-semantic --all-targets -- -D warnings    → 退出码 0，零警告
cargo test -p nlos-semantic --test signed_event_set_merge     → 17 passed / 0 failed
cargo test -p nlos-semantic --no-fail-fast                    → 66 passed / 0 failed（8 测试目标）
```

证据测试 `crates/nlos-semantic/tests/signed_event_set_merge.rs`（17 项测试）断言面：

1. **双域存储导出与全事件类型集合并集（`[DIST-EVENT-001]` + `[SEM-CHECKPOINT-001]`）**：
   - `store_backed_export_signed_event_set_across_event_types_and_idempotent_merge`：两个独立 `SemanticAuthority` SQLite 存储分别接纳 Assertion、`IntentSpec`、Judgment 事件（且有意以不同接纳顺序制造不同的本地 `log_seq`），经 `export_signed_event_set()` 导出后由 `merge_signed_event_sets()` 合并，共享事件精确收敛为单条 `MergedSignedEvent` 并保留双域 `AdmissionReceipt`，单域事件保留 1 条 `AdmissionReceipt`，自合并幂等去重；
   - `two_domains_same_event_merges_with_both_admissions`、`disjoint_events_union_sorted_by_event_id`、`identical_items_across_sets_dedup_idempotently`、`same_content_different_nonce_events_merge_separately`、`empty_inputs_and_argument_order_independence`：验证纯集合并集交换律、幂等律与“同内容不同 nonce 事件不发生 CRDT 语义折叠”。
2. **跨域审计锚点保留与密码学验签（`[DIST-AUDIT-001]`）**：
   - `dist_audit_001_cross_domain_export_and_verified_merge_preserves_provenance`：双域通过 `export_cross_domain_event_set` 导出各自携带非零 `trust_snapshot_digest` 的跨域观测，经 `verify_and_merge_cross_domain_events` 对源生产者签名做 Ed25519 验签并合并，断言 `MergedCrossDomainEvent` 完整保留 `producer_signature`、双接收域 `AdmissionReceipt` 与按 `receipt_id` 排序的 `CrossDomainAuditRecord`；
   - `dist_audit_001_rejects_missing_or_zero_trust_snapshot_fail_closed`：断言全零 `trust_snapshot_digest` 触发 `MissingTrustSnapshot` fail-closed 拒绝；
   - `dist_audit_001_rejects_invalid_or_zero_producer_signature_fail_closed`：断言篡改或全零 `producer_signature` 触发 `InvalidProducerSignature` fail-closed 拒绝；
   - `dist_audit_001_rejects_conflicting_producer_signatures_across_domains_fail_closed`：断言同一 `event_id` 跨域出现不一致 `producer_signature` 时触发 `ProducerSignatureMismatch` + `ProducerSignatureConflict` 并整条拒绝；
   - `dist_audit_001_rejects_conflicting_trust_snapshots_for_same_receipt_id`：断言同一 `receipt_id` 出现不同 `trust_snapshot_digest` 时触发 `TrustSnapshotConflict` fail-closed 排除。
3. **单条内禀完整性拒绝与接收域冲突隔离**：
   - `rejects_event_id_canonical_mismatch`、`rejects_admission_event_binding_mismatch`、`rejects_admission_log_seq_binding_mismatch`、`rejects_admission_receipt_id_derivation_mismatch`、`receipt_content_conflict_excludes_event_without_other_admission`、`duplicate_store_admission_conflict_keeps_other_stores`。

## 3. 边界与限制（诚实域，`EVID-SCOPE-001`）

- **单声明范围**：本证据仅覆盖 `C-SYNC` 轨道下的 `[DIST-EVENT-001]` 签名 Semantic 事件集合存储导出/合并与 `[DIST-AUDIT-001]` 跨域审计三元锚点保留及 fail-closed 校验。
- **非跨机网络传输证据**：测试在同一宿主机上的多个独立 SQLite `SemanticAuthority` 存储目录间执行内存切片导出与合并；不包含跨进程 RPC、网络分片重传或分布式 `SemanticCheckpoint` 向量同步协议。
- **非 `[DIST-ART-001]` 证据**：Artifact 同步的按内容类型 merge / conflict / authoritative replica / deletion 语义由独立的 `C-SYNC` Artifact 同步车道承载，不在本证据声明范围内。
- **不自动创造语义判断**：合并层遵循 `[DIST-EVENT-001]`，不消解 `CONTRADICTS` Judgment、不过滤已撤回（Retracted）或带 Taint 的事件，所有信任裁决仍归属上游 `TrustView` / `AuthorityView`。

## 4. 复现

```text
cargo test -p nlos-semantic --test signed_event_set_merge -- --nocapture
```
