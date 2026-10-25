# C-SEARCH-001 / W65-SEARCH-EXT：非权威候选发现信封、签名向量检查点有界快照查询、游标分页与多 Cell 联邦候选归并

- 状态：`PASS`（`[CONS-DECL-001]` 一致性声明与显式降级门禁 + `[CONS-CTRL-001]` 五门控制权威禁用守卫 + `[SEM-CHECKPOINT-001]` / `[TASK-SNAPSHOT-001/003]` 签名向量检查点有界快照发现与权威重验 + `[PERF-CONTROL-001]` 有界窗口游标分页 + `[DIST-FAIL-001]` 多 Cell 联邦候选去重与显式部分失败暴露的单机多 SQLite 持久库证据；**非**跨机网络传输证据，生产消费方接线随 `W66` 紧接落地）
- 日期：2026-10-25
- Owner：`C-MULTICELL` 轨道 `#15`（前置 `B-SEARCH-001` / `W33-E` 最小只读面 + 本次 `W65-SEARCH-EXT` 超最小面扩展）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §24.1 `[CONS-DECL-001]`、`[CONS-CTRL-001]`；§24.2 `[TASK-SNAPSHOT-001]`、`[TASK-SNAPSHOT-003]`；§17.4 `[SEM-RETRACT-004]`；§18 `[SEM-CHECKPOINT-001]`；§26.1 `[DIST-FAIL-001]`；§27 `[PERF-CONTROL-001]`
- 基线：`5ac73b6333515efbd28b9fa94a4c9eb27df2cf18`

## 1. 实现事实

- `crates/nlos-search/src/lib.rs`：
  - 更新顶部模块声明，完整保留 `2026-10-05 F1` 历史记录并登记 `2026-10-25 W65：超最小面扩展落地（非权威候选发现、向量检查点有界快照查询、游标分页与多 Cell 联邦候选归并）；生产消费方接线随 W66 紧接落地`，保持 `#![forbid(unsafe_code)]`（workspace 级强制）以及与既有 `B-SEARCH-001` 公共类型/方法的 100% 向后兼容；
  - **一致性声明与非权威用途限制信封（`[CONS-DECL-001]`、`[CONS-CTRL-001]`）**：
    - 定义 `SearchRequestedConsistency::{EventualOrDerived, CheckpointBoundedSnapshot}`（含 `verify_achieved(achieved, allow_downgrade, accepted_lower_modes)` 显式降级校验）与 `SearchAchievedConsistency::{EventualLiveScan, DerivedInMemoryIndex, CheckpointBoundedSnapshot, FederatedPartialEventual}`；
    - 定义 `ControlConsumerKind` 与 `SearchUseRestrictions`（常量 `SearchUseRestrictions::NON_AUTHORITATIVE` 将 `forbid_capability_decision`、`forbid_resource_ledger_commit`、`forbid_task_head_commit`、`forbid_package_update`、`forbid_irreversible_action` 全部置 `true`），并提供 `enforce_not_control_authoritative(purpose)` fail-closed 守卫：仅放行非权威 `CandidateDiscovery`（且若任一 `forbid_*` 标志被篡改清除则连 `CandidateDiscovery` 一并拒绝），任何试图将搜索/索引结果直接送入 `Capability`、`ResourceAccount`、`TaskHead`、`Package` 或 `IrreversibleAction` 控制门禁的调用一律返回 `SearchError::ControlAuthoritativeUseForbidden`；
  - **非权威候选包装与检查点权威重验（`[TASK-SNAPSHOT-003]`、`[SEM-RETRACT-004]`）**：
    - 定义 `NonAuthoritativeCandidateHit`（包装 `AssertionHit`、源 `domain_id: [u8; 16]`、`achieved_consistency` 与 `SearchUseRestrictions::NON_AUTHORITATIVE`）、`SemanticIndex::query_candidates`、`CheckpointVerifier` trait（同时支持固定 32 字节 `Ed25519PublicKey` 与完整 `IdentityAuthority` 句柄验签）以及 `VerifiedCheckpointCandidate` 证明回执；
    - 实现 `verify_candidate_against_checkpoint(service, candidate, checkpoint, verifying_key)`（及同名 `SearchService` 方法）：先校验候选信封限制位与 `SignedSemanticVectorCheckpoint` 签名/摘要/Merkle 根，定位源域 `DomainSemanticWaterMark` 并核验本地只读 `event_log` 前缀的 `event_count` 与 `event_id_merkle_root`，再回读实时 `SemanticAuthority`（`inspect_event`、`inspect_admission_receipt`、`inspect_event_retraction` 与 `content_objects`）逐字段比对；对索引构建后新增撤回（`CandidateRetracted`）、检查点水位之后接纳的事件（`CandidateOutsideCheckpoint`）或任何字段篡改（`CandidateAuthorityMismatch`）全部 fail-closed 拒绝；
  - **向量检查点有界快照发现与游标分页（`[SEM-CHECKPOINT-001]`、`[PERF-CONTROL-001]`）**：
    - 新增 `PaginatedAssertionSelector { selector, after_log_seq, max_log_seq_inclusive }` 与 `SearchPage<T> { items, next_after_log_seq, exhausted, achieved_consistency, use_restrictions }`；
    - 新增 `SearchService::search_assertions_paginated`：在 `SQLITE_OPEN_READ_ONLY` 连接上以唯一单调 `event_log.log_seq` 执行 keyset 游标扫描（无后置过滤时 SQL 窗口精确限制为 `limit + 1`，含后置过滤时限制为 `limit * 4 + 64`），严格保证 `exhausted == next_after_log_seq.is_none()`、跨页零重零漏及上界截断后撤回事件不越界泄漏；
    - 新增 `SearchService::search_candidates_at_checkpoint`（及带显式 `require_domain_present` 策略的 `search_candidates_at_checkpoint_with_options`）：先对 `SignedSemanticVectorCheckpoint` 执行 `Ed25519` 严格验签，再将本地只读 SQL 扫描严格约束在 `e.control_domain_id = local_domain_id AND l.log_seq <= domain_watermark`（同时将 `event_retractions` 子查询约束在同一水位内），排除检查点之后接纳的全部断言。
- `crates/nlos-search/src/federated.rs`：
  - 定义 `RemoteSearchFailureReason::{DeadlineExceeded { budget_ms }, Unavailable, Uncertain}`、`CellSearchShardOutcome::{Available, Failed}`、`FailedShardSummary`、`FederatedCandidateHit`（刻意不暴露任何跨 Cell 标量 `log_seq`）与 `FederatedSearchReport`；
  - 实现 `aggregate_federated_candidates(shards, limit, allow_partial_on_remote_failure)`：
    - 完全不假设跨 Cell 全局标量 `log_seq`（`[SEM-CHECKPOINT-001]`）：按内容寻址 `SemanticEventId` 去重合并跨分片命中，核验同一 `EventId` 的不可变 canonical 字段一致性（冲突时以 `SearchError::CorruptRecord` fail-closed 拒绝），保留字典序去重的观测 `observed_by_cells` 与检查点摘要溯源列表，对 `admitted_at_ms` 取交换律最小值，并以单调或（`[SEM-RETRACT-004]`）传播跨分片撤回状态，最终按 `(admitted_at_ms, event_id.into_bytes())` 确定性排序，实现分片到达顺序无关性；
    - 严格执行 `[DIST-FAIL-001]`：存在任何失败分片且 `allow_partial_on_remote_failure == false` 时以 `SearchError::RemoteShardFailed(failed_shards)` fail-closed 拒绝；显式设置 `allow_partial_on_remote_failure == true` 时将 `achieved_consistency` 降级为 `SearchAchievedConsistency::FederatedPartialEventual`，完整暴露排序后的 `failed_shards`，且所有候选与报告均携带 `SearchUseRestrictions::NON_AUTHORITATIVE`。

## 2. 验证（2026-10-25，Windows x86_64）

```text
cargo fmt -p nlos-search -- --check                         → 退出码 0
cargo clippy -p nlos-search --all-targets -- -D warnings    → 退出码 0，零警告
cargo test -p nlos-search --no-fail-fast                    → 24 passed / 0 failed（3 测试目标：lib 2 + c_search_non_authoritative_checkpoint 14 + search_service 8）
```

证据测试 `crates/nlos-search/tests/c_search_non_authoritative_checkpoint.rs`（14 项集成测试）断言面：

1. **一致性声明与非权威用途门禁（`[CONS-DECL-001]` + `[CONS-CTRL-001]`）**：
   - `cons_ctrl_001_use_restrictions_reject_all_five_control_gates_and_allow_candidate_discovery`：断言 `CandidateDiscovery` 放行，而 `CapabilityDecision`、`ResourceAccountCommit`、`TaskHeadCommit`、`PackageUpdate`、`IrreversibleAction` 五类控制门禁及篡改清除任一 `forbid_*` 位的信封全部 fail-closed 拒绝；
   - `cons_decl_001_requested_vs_achieved_consistency_downgrade_gate`：断言未开启 `allow_downgrade` 或未列入 `accepted_lower_modes` 时一致性降级以 `ConsistencyUnsatisfied` fail-closed 拒绝。
2. **签名向量检查点有界快照发现（`[SEM-CHECKPOINT-001]` + `[TASK-SNAPSHOT-001]`）**：
   - `sem_checkpoint_001_search_candidates_at_checkpoint_excludes_post_checkpoint_admissions`：在真实 `SemanticAuthority` 上导出签名检查点后继续追加断言，验证实时查询可见全量 5 条断言，而检查点快照查询严格排除检查点后的 2 条断言并标记 `CheckpointBoundedSnapshot` + `NON_AUTHORITATIVE`；
   - `sem_checkpoint_001_checkpoint_search_paginates_within_signed_watermark_bound`：验证检查点有界快照下的多页游标遍历不越过签名水位；
   - `sem_checkpoint_001_checkpoint_search_rejects_tampered_signature_digest_and_wrong_key`：验证篡改 `Ed25519` 签名、篡改 `checkpoint_digest` 与错误公钥全部 fail-closed 拒绝；
   - `sem_checkpoint_001_checkpoint_search_enforces_domain_presence_and_merkle_root_match`：验证缺失源域水位默认以 `MissingDomainWatermark` 拒绝（显式 `require_domain_present = false` 时返回空穷尽页），且异源分叉检查点因 `event_id_merkle_root` 失配以 `CheckpointWatermarkMismatch` 拒绝。
3. **候选重验与后置撤回/越界拦截（`[TASK-SNAPSHOT-003]` + `[SEM-RETRACT-004]`）**：
   - `task_snapshot_003_verify_candidate_against_checkpoint_succeeds_for_valid_unretracted_fact`：验证对检查点内未撤回合法候选同时通过固定公钥与 `IdentityAuthority` 生成 `VerifiedCheckpointCandidate` 证明；
   - `task_snapshot_003_verify_candidate_fails_closed_on_post_index_retraction_and_post_checkpoint_drift`：验证陈旧 `SemanticIndex` 命中在权威撤回后以 `CandidateRetracted` 拒绝、检查点后接纳候选以 `CandidateOutsideCheckpoint` 拒绝、篡改载荷摘要候选以 `CandidateAuthorityMismatch` 拒绝。
4. **有界窗口游标分页（`[PERF-CONTROL-001]`）**：
   - `perf_control_001_cursor_paginated_scan_has_zero_duplicates_and_zero_gaps_across_filters`：在 35 条混合模式与周期撤回断言集上遍历多种页大小（`1/4/5/7/50`）、过滤谓词及 `max_log_seq_inclusive` 边界，验证跨页零重零漏及撤回水位精确隔离；
   - `perf_control_001_paginated_scan_bounds_per_page_sql_window_without_scanning_tail`：验证小 `limit` 分页在首窗口收集满后立即返回游标而不遍历 80 行未验证尾部。
5. **多 Cell 联邦候选归并与远程失败显式暴露（`[SEM-CHECKPOINT-001]` + `[DIST-FAIL-001]` + `[SEM-RETRACT-004]`）**：
   - `sem_checkpoint_001_federated_aggregator_deduplicates_across_cells_with_arrival_independence`：双独立 Cell `SemanticAuthority` 持久库 + 异构本地 `log_seq`/`admitted_at_ms` 副本场景下，验证跨分片按 `SemanticEventId` 去重、单调撤回传播及正序/逆序/重放输入逐字节一致；
   - `dist_fail_001_federated_remote_failures_fail_closed_by_default_and_surface_explicit_partial_downgrade`：验证 `DeadlineExceeded`、`Unavailable`、`Uncertain` 在默认模式下以 `RemoteShardFailed` fail-closed 拒绝，显式允许部分结果时降级为 `FederatedPartialEventual` 并完整保留 `failed_shards`；
   - `federated_aggregator_rejects_zero_limit_and_conflicting_canonical_payload_for_same_event_id`：验证 `limit == 0` 与同 `EventId` canonical 字段冲突 fail-closed 拒绝。
6. **严格零写入不变量**：
   - `strict_zero_write_invariant_across_checkpoint_paginated_verification_and_federated_workloads`：在执行分页、检查点快照查询、权威重验、派生索引构建与联邦归并全套负载前后核对 `PRAGMA data_version`、全表行数与目录文件集，确认 `SQLITE_OPEN_READ_ONLY` 零权威写入且零私有持久文件生成。

## 3. 边界与限制（诚实域，`EVID-SCOPE-001`）

- **生产消费方待 `W66` 接线**：本波（`W65`）聚焦 `nlos-search` crate 自身超最小面契约与测试闭环，未修改其他业务 crate 的 `Cargo.toml` 依赖图；上层 `TaskSnapshot` 候选发现与系统控制面只读分页消费方接线随 `W66` 紧接落地。
- **非跨机网络传输证据**：`federated::aggregate_federated_candidates` 验证确定性归并、去重、单调撤回传播与 `[DIST-FAIL-001]` 失败语义透明暴露，跨进程/跨主机分片扇出 RPC 传输由上层分布式传输车道承载。
- **非向量嵌入/ANN 近似检索引擎**：当前候选发现仍基于确定性结构化谓词、`TrustView` 校验与检查点水位裁剪，不包含模糊全文分词或向量近似近邻（ANN）索引。

## 4. 复现

```text
cargo fmt -p nlos-search -- --check
cargo clippy -p nlos-search --all-targets -- -D warnings
cargo test -p nlos-search --no-fail-fast
```
