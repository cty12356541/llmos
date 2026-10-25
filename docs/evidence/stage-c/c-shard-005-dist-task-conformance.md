# C-SHARD-005 / W63-CS1A + W63-CS1BC：§26.1 三 Record Schema v47 迁移与 DIST-TASK-001 / 002 / 004 全语义对齐

- 状态：`PASS`（`C-SHARD` Phase 1 `CS1-A`、`CS1-B`、`CS1-C` 单库权威核与六域写集集成证据；跨进程驱动器生产化与 `MigrationObject` 绑定属并行车道 `CS1-D`，**非**跨机证据）
- 日期：2026-10-24
- Owner：`C-SHARD` 计划 Phase 1（[c-shard-plan.md](../../management/c-shard-plan.md) `CS1-A` / `CS1-B` / `CS1-C` 车道）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §26.1（`TaskAuthorityAssignment` / `TaskAuthorityParticipantRegistry` / `TaskAuthorityTakeoverReceipt` 三 Record，`[DIST-TASK-001]`、`[DIST-TASK-002]`、`[DIST-TASK-004]`）
  - [ADR-0018](../../management/adrs/0018-stage-c-Configured-Single-Machine-Multi-Cell-Topology.md) 决定 5 & [ADR-0020](../../management/adrs/0020-consensus-group-activation-gate.md)（`consensus_commit_index` 仅作被动元数据槽，零 Raft / 多节点共识协议语义）
  - [ADR-0019](../../management/adrs/0019-cross-cell-commit-semantics-extension.md) R1.2.2 / R2.4（CS-2 命名终裁与三 Record 字段/状态/不变量映射表）

## 1. 实现事实

### 1.1 CS1-A：Schema v47 三 Record 字段与状态机对齐（`W63-CS1A`）

- **`crates/nlos-task/src/migrations.rs` & `store.rs`（`SCHEMA_VERSION = 47`）**：
  - `task_authority_assignments`：`assignment_state` 扩展为 §26.1 五态枚举（`Active = 1`、`TakeoverPending = 2`、`Fenced = 3`、`Expired = 4`、`Quarantined = 5`）；SQLite 触发器 `task_authority_assignment_identity_immutable` 锁死终态 `(3, 4, 5)` 不可逆、不可回退，并禁止 `TakeoverPending(2) -> Active(1)` 倒退；新增公开转换入口 `SqliteTaskAuthority::expire_authority_assignment` 与 `quarantine_authority_assignment`。
  - `task_participant_registries`：补齐可空 `prior_participant_registry_root BLOB(32)`（generation 1 为 `NULL`，`generation > 1` 为前代 `participant_registry_root`）以及成对出现的 `authority_id BLOB(16)` / `signature BLOB(64)`（由 `participant_registry_signature_coupled_insert` 与 `task_participant_registry_identity_immutable` 触发器强制同 `NULL` 或同 `NOT NULL` 且写后不可变）；新增 `record_participant_registry_signature`。
  - `task_authority_takeover_receipts`：补齐可空 `outstanding_effect_quarantine_root BLOB(32)` 与 `consensus_commit_index INTEGER CHECK(IS NULL OR >= 0)`；触发器保证 `Pending(1)` 期写一次即锁死、`Complete(2)` 后完全不可变。
  - **CS-2 命名终裁**：保留现有 SQLite 表名与 `Authority*` / `ParticipantRegistry*` 主结构名，在 `nlos-task` 根导出 §26.1 规范类型别名 `TaskAuthorityAssignment`、`AuthorityAssignmentStatus`、`TaskAuthorityParticipantRegistry`、`ParticipantRegistrySnapshot`、`TaskAuthorityTakeoverReceipt`，并在 `c-shard-plan.md` §4.1 建立完整字段/状态/不变量映射表。

### 1.2 CS1-B：`[DIST-TASK-001]` Admission 与 Permit-Freeze 全语义闭环（`W63-CS1BC`）

- **Participant 注册先于 Operation Admission / 同事务原子准入**：
  - 在 `crates/nlos-task/src/store.rs` 与 `lease.rs` 新增 `DistTaskOperationAdmissionRequest`、`DistTaskOperationAdmissionRecord`、`verify_dist_task_operation_admission`、`admit_verified_participant_operation`、`admit_operation_binding_with_authority`。
  - 每次准入在单一 `BEGIN IMMEDIATE` 事务内强制校验：当前 `TaskAuthorityAssignment` 处于 `Active` 态（任何 `TakeoverPending` / `Fenced` / `Expired` / `Quarantined` 均以 `TaskStoreError::AuthorityLeaseFenced` 拒绝）、`(authority_id, authority_term, control_epoch, fencing_token)` 与活租约逐位匹配、`TaskAuthorityParticipantRegistry` 处于 `Open` 态且 CAS 绑定匹配。
  - 未注册 participant 调 `verify_dist_task_operation_admission` 立即以 `TaskStoreError::TaskWriteSetConflict` fail-closed；调 `admit_verified_participant_operation` 或 `admit_operation_binding_with_authority` 则在同一 SQLite 事务内原子完成 owner proof 核验、registry generation 递增与 active assignment 旋转，且对未 prepare 或未 activate 的 `OperationBinding` 在 `seal_task_write_set` 门控以 `OperationDispatchNotPrepared` / `OperationDispatchNotActivated` fail-closed（零半激活残留）。
- **Active Assignment 门控全覆盖**：
  - `register_verified_participant` 与 `seal_task_write_set_inner` 现显式核验任务若存在 `TaskAuthorityAssignment` 则必须处于 `Active` 态，闭合了非 `Active` 赋值态下仍可推进 registry 或 seal 写集的缺口。
- **CommitPermit 同 CAS 置 `FROZEN_FOR_PERMIT` 与 `EffectPermit` 逐位绑定**：
  - 新增 `request_commit_permit_with_authorities_and_lease`，支持在携带六域 `Authorities` 复核（含 `OperationBinding` / `ChannelTopic` 效果端点）的同时绑定 `AuthorityLeaseRecord`。
  - `CommitPermit` 发放同一事务将 `TaskAuthorityParticipantRegistry` 置为 `FrozenForPermit`；冻结期间任何新增 participant 注册或同事务 operation admission 均以 `TaskStoreError::ParticipantRegistryFrozen { state: FrozenForPermit }` 拒绝。
  - 每个 `EffectPermit` 逐位拷贝并校验父 `CommitPermit` 的 `(participant_registry_generation, participant_registry_root)`，任何 generation 或 root 漂移均以 `ParticipantRegistryBindingMismatch` 拒绝。

### 1.3 CS1-C：`[DIST-TASK-002]` 与 `[DIST-TASK-004]` 缺口对齐（`W63-CS1BC`）

- **Takeover Fence 并集语义（`frozen registry ∪ durable outstanding participants`）全六域解析**：
  - 更新 `crates/nlos-task/src/store.rs` 中的 `load_outstanding_operation_participants`：当任务存在未决（`Issued` 或 `Quarantined`）permit 且其 `TaskWriteSet` 包含 `artifact_writes`、`semantic_reads`、`semantic_appends`、`resource_reservations`、`artifact_reads`（已注册 `ArtifactHead`）、`process_binding` 或 `effect_endpoints` 时，自动从写集绑定的 registry generation 与 permit 冻结 generation 解析全部 durable participant 并并入 `takeover_fence_members` 与 `takeover_fence_roots`，消除了此前非 `effect_endpoints` 写集返回 `None` 导致 manifest 缺失的缺口。
- **§26.1 六类 Canonical Endpoint Barrier 覆盖审计与门控**：
  - 在 `crates/nlos-task/src/participant.rs` 定义 `ParticipantType::CANONICAL_SECTION_26_1`（`TaskStore`、`ArtifactHead`、`SemanticAdmission`、`ChannelTopic`、`DriverGateway`、`ResourceLedger`）与 `is_canonical_section_26_1`。
  - 在 `crates/nlos-task/src/store.rs` 新增 `inspect_takeover_endpoint_barrier_audit` 与 `verify_six_endpoint_takeover_barrier_coverage`（返回 `TakeoverEndpointBarrierAuditReport`，缺任一 canonical 类型或任一并集成员未完成签名 observation 时返回 `TaskStoreError::BarrierCoverageIncomplete { expected, observed }`）。
- **`QuarantineReceipt` 与 `TaskAuthorityTakeoverReceipt.outstanding_effect_quarantine_root` 自动联动**：
  - `persist_takeover_pending_receipt` 在任务存在 `Quarantined` permit 时自动读取 `task_quarantine_receipts.outstanding_effect_quarantine_root` 填入 `TaskAuthorityTakeoverReceipt`；若调用方显式传入的 root 与 durable quarantine receipt 不一致，则以 `TaskStoreError::AuthorityLeaseBindingMismatch` fail-closed。
- **`TaskPermitClosureReceipt` 强制推进新 `OPEN` Registry Generation（禁止原地解冻）**：
  - `crates/nlos-task/src/reconcile.rs` 的 `write_closure_receipt`（`close_permit` / `close_permit_with_authority_lease`）在写入 `FAILED_BEFORE_EFFECT` 或 `CANCELLED_BEFORE_EFFECT`（`TaskHead` 不变）回执的同一事务内调用 `reopen_registry_and_rotate_assignment_on_permit_closure`：将原 `FrozenForPermit` 代置为 `Superseded(4)`（SQLite 触发器锁死 `OLD.registry_state = 4` 永不可回退为 `Open(1)`），插入 `generation + 1` 的新 `Open(1)` 代（`prior_participant_registry_root = Some(old_frozen_root)`），并在存在 `Active` assignment 时自动将旧 assignment 置 `Fenced(3)`、插入绑定新 registry generation 的 `Active(1)` assignment；同时提供显式核验/幂等重放入口 `reopen_participant_registry_after_permit_closure`。

## 2. 验证（2026-10-24，Windows 11 x86_64）

```text
cargo fmt -p nlos-task -- --check                                                                                                                        → 退出码 0
cargo clippy -p nlos-task --all-targets -- -D warnings                                                                                                   → 退出码 0，零警告
cargo test -p nlos-task --test c_shard_cs1a_schema_v47 --test c_shard_cs1bc_conformance --no-fail-fast                                                   → 10 passed / 0 failed（4 + 6）
cargo test -p nlos-task --lib --test authority_lease --test takeover_completion --test cross_term_adoption --test effect_reconcile --test participant_registry --no-fail-fast → 75 passed / 0 failed
```

测试矩阵明细：

1. `crates/nlos-task/tests/c_shard_cs1a_schema_v47.rs`（4 项测试）：
   - `v46_schema_migrates_to_v47_preserving_all_three_authority_records`
   - `assignment_five_state_expired_and_quarantined_transitions_and_triggers`
   - `participant_registry_and_takeover_receipt_v47_fields_persist_and_lock`
   - `partial_v47_schema_fails_closed_with_corrupt_record`
2. `crates/nlos-task/tests/c_shard_cs1bc_conformance.rs`（6 项测试）：
   - `cs1b_dist_task_001_operation_admission_and_same_transaction_registration`
   - `cs1b_dist_task_001_permit_freeze_and_bitwise_effect_permit_binding`
   - `cs1b_cs1c_dist_task_004_non_active_assignment_states_refuse_admission_and_permits`
   - `cs1c_dist_task_004_permit_closure_receipt_forbids_in_place_unfreeze_and_rotates_to_new_open_generation`
   - `cs1c_dist_task_002_six_canonical_endpoint_types_and_union_fence_barrier_verification`
   - `cs1c_dist_task_002_quarantine_receipt_auto_links_outstanding_effect_quarantine_root_into_takeover_receipt`

## 3. 边界与限制（诚实域）

- **单机单库 / 多权威组件集成证据**：本证据验证 `nlos-task` 与六类本地权威存储（`TaskStore`、`ArtifactStore`、`SemanticAuthority`、`ChannelAuthority`、`ResourceAuthority`、`SqliteOperationStore`）及 `IdentityAuthority` 的真实持久化、触发器与事务门控；**不声称**跨机网络传输或跨时钟域仲裁（留 R1.4 触发器 7）。
- **跨进程驱动器生产化与 `MigrationObject` 绑定**：属并行车道 `CS1-D`（`crates/nlos-cell` / `crates/nlos-slice-k` 写集）。
- **共识协议基底**：`consensus_commit_index` 保持为被动元数据槽，零 Raft / 多节点共识协议语义（ADR-0020 `CANDIDATE` 未激活）。
- **远端物理 cleanup 残域**：签名 barrier observation 证明主体背书与精确 fence root 绑定，远端物理副作用清理仍按 `RISK-B-11` 残域口径保留。

## 4. 复现

```text
cargo test -p nlos-task --test c_shard_cs1a_schema_v47 --test c_shard_cs1bc_conformance -- --nocapture
```
