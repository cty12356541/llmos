# B-TASK-003：EFFECT_UNKNOWN quarantine/reconcile、跨 Attempt effect history 与 retry fence、required slot 成功语义初始证据

> 状态：PARTIAL PASS（本地复验 + 三平台 CI 通过；尚待 v3 表组 fault-injection 与 integrator 审议）
>
> 日期：2026-08-05（三平台 CI 于同日通过：[run 30931085056](https://github.com/cty12356541/llmos/actions/runs/30931085056)）
>
> 对应：`[TASK-EFFECT-003]`、`[TASK-COMMIT-003]`（单 authority 子集）、`[TASK-COMMIT-002]`（required slot 成功语义完整子集）、`[TASK-EFFECT-ID-001]`（history 表/root/重算子集）、`[TASK-RETRY-EFFECT-001]`（PARTIAL_EFFECT/FAILED_AFTER_EFFECT + fence 推进 + 回读子集）
>
> 前置：[B-TASK-001](./b-task-001-task-authority-commit-permit.md)、[B-TASK-002](./b-task-002-effect-permit-dispatch.md)

## 1. 本切片完成的边界

在 `nlos-task`（schema v2→v3）上扩展三块紧耦合能力：

1. **EFFECT_UNKNOWN 全生命周期（quarantine & reconcile）**：
   - `finalize_commit_v3` / `close_permit` 在任一 slot 为 `EFFECT_UNKNOWN` 时，把 active permit CAS 为不可复用 `QUARANTINED` tombstone，并写入 `TaskEffectQuarantineReceipt` 形持久记录（outstanding slot 列表、conflicting target 占位 digest、participant fence 占位 digest、已知 receipts）；TaskHead **不前进**；同一 task 在 tombstone 存在期间不得签发新 winner（permit CAS 返回 `PermitDecision::Quarantined`，请求 attempt 落 `SUPERSEDED`）。重放同一 finalize/close 得到同一生命周期状态（finalize 返回类型化 `TaskStoreError::Quarantined`，close 返回 `ReplayedQuarantine`）；异 fence digest fail-closed `HistoryConflict`。
   - Reconcile 流（单 authority 子集）：`adopt_permit` 仅对 QUARANTINED permit 签发 `PermitAdoptionReceipt` 形记录（绑定原 permit/epochs，scope 固定为 `RECONCILE_CLOSE_OR_QUARANTINE_ONLY`，幂等 key 重放/冲突）；`reconcile_effect` 单事务完成 slot `EFFECT_UNKNOWN → RECONCILING → EFFECT_CLOSED | CONFIRMED_NO_EFFECT | EFFECT_UNKNOWN`，消费调用方提供的 closure proof digest 占位，写 `TaskEffectReconciliationReceipt` 形记录；最后一个 unknown slot 解开后 tombstone 解除（permit 回到 `Issued`），仍 unknown 则保持 `QUARANTINED`。reconcile 重放按 (slot, adoption, outcome, proof) 逐字节判定：同 bytes 返回原 receipt，异 bytes `HistoryConflict`——无双重 reconcile。
   - 已 adoption 的 permit 禁止新 `EffectPermit` 与新 dispatch（`AdoptionScopeViolation`）；`CONFIRMED_NO_EFFECT` 不等于 `TaskNoEffectReceipt`（外部权威证明未发生，而非 token 未消费），永远不满足 required slot，但可作为 pre-effect closure 的有效缺席证明。
2. **跨 Attempt effect history + retry fence**：durable `effect_history` 表（seq 从 1 严格递增无洞、logical_effect_id、retry_fence_epoch、action_proposal/idempotency digests、operation_id 占位 NULL、outcome、authoritative_effect_receipt_id、compensation_receipt_id 占位 NULL）。`EFFECT_CLOSED`（dispatch 路径或 reconcile）与 `CONFIRMED_NO_EFFECT`（reconcile）在与 slot 闭合**同一事务**内追加；`TaskEffectHistoryRoot = H("llmos/task-effect-history/v1" || 定长 canonical(entries by seq))` 每次重算，空 entries 与 B-TASK-001 初始 head 公式逐位兼容（`0x80` 空数组占位）。`[TASK-RETRY-EFFECT-001]` 路径：required 未满足且已有 effect 时，finalize 写 `PARTIAL_EFFECT`/`FAILED_AFTER_EFFECT` 的 TaskCommitReceipt，对每个未满足 required slot 追加 `PARTIAL_EFFECT` history 条目，fence 严格 +1，head/root/fence 在同一 CAS 前进；旧 fence/root 的 snapshot 落 `CONFLICTED`，新 attempt 逐位继承。`lookup_effect_history` 返回条目 + 原 effect receipt；对已 `EFFECT_CLOSED` 的 LogicalEffectId 签发 `EffectPermit` fail-closed `EffectAlreadyClosed`（显式重新授权不在本切片）。
3. **required slot 成功语义**（`[TASK-COMMIT-002]` 完整子集）：`COMMITTED` 要求全部 slot 终态，且每个 required slot 或 (a) `EFFECT_CLOSED` + 调用方 `EffectClosedSuccess` 断言 digest（authority 不验证内容），或 (b) `NO_EFFECT` 且 reason 为 `CONDITION_NOT_APPLICABLE` + 绑定原 snapshot digest 与预绑 `required_condition_digest` 的 condition-false proof 占位（`H("llmos/task-condition-false-proof/v1" || snapshot_digest || condition_digest)` 占位绑定）。普通 `NO_EFFECT`（NOT_SELECTED/CANCELLED/EXPIRED/POLICY_SKIPPED）与 `CONFIRMED_NO_EFFECT` 永不满足 required；满足数逐槽重算；skip 绝不写成 `COMMITTED`。`close_permit`（`FAILED_BEFORE_EFFECT`/`CANCELLED_BEFORE_EFFECT`，head 不变）仍要求全部 effect 可证明未发生——`NO_EFFECT` 或 `CONFIRMED_NO_EFFECT`。

**schema v2→v3 迁移**：纯增量（`effect_history`、`task_quarantine_receipts`、`task_adoption_receipts`、`task_reconcile_receipts`、`task_effect_sequences`、`task_finalize_proofs` 六表 + 索引 + immutable triggers），单事务完成。golden-v2 测试：冻结 v2 DDL + 种子数据建库 → 迁移 → v2 数据逐位完整、B-TASK-002 行为不变、v3 平面从空 history 起步且全流程可用；失败回滚测试：预置冲突表 → open 失败 → `user_version` 仍为 2、v2 数据完好、不留半个 v3 表。

## 2. 线性化事务边界

沿用 B-TASK-001/002 模式：进程内单写者 admission + 每个变更 API 恰好一个 `BEGIN IMMEDIATE`；决策、状态 CAS、roots/history 追加、receipt 写入与 epoch 前进同事务提交。history seq 与 adoption epoch 经 `task_effect_sequences` 行单调推进（CAS，changed!=1 即 CorruptRecord fail-closed），保证 seq 无洞；history/quarantine/adoption/reconcile 表均有 immutable trigger。

## 3. 规范解释决定（本切片记录）

1. **PARTIAL_EFFECT vs FAILED_AFTER_EFFECT 规则**：required 未满足且已有 effect 时——至少一个 required slot 已满足（EFFECT_CLOSED+断言 或 CNA+证明）则为 `PARTIAL_EFFECT`（commit 部分可用）；零个 required 满足则为 `FAILED_AFTER_EFFECT`（attempt 目标失败，attempt 落 `Failed`）。两路径都为每个未满足 required slot 追加 `PARTIAL_EFFECT` history 条目并令 fence 严格 +1、head `commit_seq+1`、root 重算，同一 CAS。
2. **adoption scope 子集（单 authority）**：`PermitAdoptionReceipt` 由同一 authority 在重启/不确定后对 QUARANTINED permit 签发；scope 固定 `RECONCILE_CLOSE_OR_QUARANTINE_ONLY`——存在任一 adoption 记录后，`request_effect_permit` 与 `consume_dispatch_token` 拒绝（`AdoptionScopeViolation`），但 `record_effect_outcome`/`record_no_effect`（真相补写）与 reconcile/finalize/close 不受影响。跨 term takeover、registry coverage proof 不在本切片；registry 绑定字段以原 permit epochs + effect roots 占位。
3. **RECONCILING 为事务内状态**：单 authority 下「CAS 为 RECONCILING → 消费 closure proof → 写 reconcile receipt → 终态」在同一 `BEGIN IMMEDIATE` 内原子完成；`RECONCILING` 不会跨 API 持久。reconcile receipt 记录 prior_state= EFFECT_UNKNOWN、outcome 与 `effect_slot_state_root_after`。
4. **quarantine 上报形状**：`FinalizeDecision` 保持 B-TASK-001 两变体形状（`fault_injection.rs` 不可触碰，其 exhaustive match 不能破坏）；quarantine tombstone 以类型化 `TaskStoreError::Quarantined` 上报——tombstone 已提交、head 未前进，重放同一 finalize 观察到同一拒绝。`close_permit` 为新 API，使用完整四变体 `ClosePermitDecision`（含 `Quarantined`/`ReplayedQuarantine`）。
5. **legacy finalize 通道冻结**：`finalize_commit`（B-TASK-001/002 请求形状）保留 B-TASK-002 语义逐位不变——非终态（含 `EFFECT_UNKNOWN`）以 `OutstandingEffectSlots` 阻塞、permit 保持 `Issued`、全终态后以调用方 roots 提交 `COMMITTED`、fence 只校验不回退。严格 required 语义、quarantine、fence 推进只在 `finalize_commit_v3`/`close_permit` 上生效。理由：不可触碰的 `fault_injection.rs` 与并行 agent 的 crash-window 测试锁定 legacy 行为；这是有记录的兼容层而非不变量削弱（B-TASK-002 语义本就如此）。
6. **CONFIRMED_NO_EFFECT ≠ TaskNoEffectReceipt**：前者由 reconcile 路径以外部权威 closure proof 写入（`ReceiptKind::ConfirmedNoEffect`），并在同事务追加 `CONFIRMED_NO_EFFECT` history 条目；后者仅证明 token 未消费。required slot 只接受 `EFFECT_CLOSED`+断言 或 `NO_EFFECT`+CNA+绑定证明；`CONFIRMED_NO_EFFECT` 在 required 上视为未满足，配合 `close_permit` 视为有效缺席证明。
7. **PARTIAL 条目的 history 归属**：未满足 required slot 的 `PARTIAL_EFFECT` 条目在 finalize 事务内以 **新 fence**（+1 后）记录；dispatch/reconcile 闭合条目以闭合时 head 的 fence 记录。`TaskEffectHistoryEntry.operation_id`/`compensation_receipt_id` 恒为 NULL 占位（无 Operation 绑定、无 compensation 执行）；`COMPENSATED` outcome 在类型中保留但本切片无产生路径。
8. **digest/ID 占位约定**：新 receipt ID 均为 domain-separated SHA-256 派生（`llmos/task-effect-quarantine/v1`、`llmos/task-permit-adoption/v1`（含 adoption_epoch）、`llmos/task-effect-reconciliation/v1`（含 slot 终态 seq）、`llmos/task-permit-closure-receipt/v1`）；conflicting target 占位为最小 effect_seq 的 unknown slot 的 `H("llmos/task-quarantine-target/v1" || slot || logical)`；finalize 重放以 `task_finalize_proofs` 存证明 digest（`H("llmos/task-finalize-proofs/v1" || 逐条 seq||variant||digest)`）做逐字节比较，v3 前旧 receipt 走 legacy roots 比较。时间戳全部由调用方供给。
9. **EffectAlreadyClosed 置于 EffectPermit 签发点**：CommitPermit 可声明已闭合 logical effect（回读路径），但 `request_effect_permit` 对其 fail-closed；显式重新授权与 gateway 派生 key 不在本切片。
10. **文件规模**：`reconcile.rs` 约 1.5k 行，沿用本 crate `store.rs`/`effect.rs` 的有意识偏差先例（单写者存储平面按设计内聚），非疏漏。

## 4. 测试矩阵与命令

环境：Apple Silicon / arm64，macOS，workspace toolchain（rustc 1.97.x），rusqlite 0.40 bundled SQLite。

```sh
cargo test -p nlos-task            # 66 passed; 0 failed
#   task_authority.rs        14（B-TASK-001 原套件，未改）
#   effect_permit.rs         13（B-TASK-002 原套件，仅 1 处适配，见下）
#   fault_injection.rs        7（B-TASK-001 并行切片，未触碰）
#   effect_reconcile.rs      11（本切片）
#   effect_history.rs        10（本切片）
#   effect_fault_injection.rs 11（并行 agent 切片，同工作区复验）
cargo test --workspace           # 全部通过（49 个套件 test result: ok）
cargo clippy --workspace --all-targets -- -D warnings   # 通过
cargo fmt --all -- --check       # 通过
```

测试与验收点映射：

| 测试 | 验收点 |
|---|---|
| `unknown_at_finalize_quarantines_permit_and_freezes_head` | tombstone 持久化、head 冻结、无新 winner（attempt SUPERSEDED + receipt id）、重放同 lifecycle、异 digest fail-closed |
| `close_permit_quarantines_on_unknown_and_replays_tombstone` | close 路径同一 tombstone、重放返回原 tombstone |
| `adoption_scope_forbids_new_permits_dispatches_and_effects` | adoption 仅限 QUARANTINED、epoch 校验、幂等重放/冲突、scope 禁止新 EffectPermit/dispatch |
| `reconcile_to_effect_closed_unblocks_committed_finalize` | reconcile 闭合 + history 条目 + tombstone 解除 + proved COMMITTED + head root == 重算 root |
| `reconcile_replay_is_byte_exact_and_proof_conflicts_fail_closed` | 无双重 reconcile；异 proof HistoryConflict |
| `confirmed_no_effect_on_required_slot_never_commits` | required 上 CONFIRMED_NO_EFFECT 永不 COMMITTED、permit 保持 open、`close_permit` 以其为有效缺席证明且 head 不变 |
| `still_unknown_reconcile_returns_to_quarantine` | 仍 unknown → 回 EFFECT_UNKNOWN + 保持 QUARANTINED + 无 history；二次 adoption+reconcile 可解开 |
| `quarantine_adoption_reconcile_replay_consistent_across_restart` / `reconcile_and_finalize_replay_consistent_across_restart` | 全部新流的重启后重放一致性 |
| `close_permit_requires_all_effects_provably_absent` / `close_permit_closes_pure_no_effect_permit_with_head_unchanged` | EFFECT_CLOSED 禁止 pre-effect closure（`PermitHasEffects`）；纯 no-effect 关闭 head 不变、重放/异 outcome fail-closed、CAS gate 释放 |
| `empty_history_root_is_bit_compatible_with_initial_head` | 空 history root == B-TASK-001 固定初始公式 |
| `history_entries_append_atomically_gapless_with_recomputed_root` | 同事务追加、seq 1..n 无洞、root 逐次重算、重启持久、immutable trigger |
| `partial_effect_advances_head_root_and_fence_and_new_attempts_inherit` | PARTIAL_EFFECT：fence 严格 +1、head/root/fence 同 CAS、stale-fence snapshot CONFLICTED、新 attempt 继承 |
| `failed_after_effect_and_no_effect_unsatisfied_rules` | FAILED_AFTER_EFFECT 规则；零 effect 时 RequiredEffectUnsatisfied + permit open + close 收口 |
| `lookup_effect_history_readback_and_re_dispatch_refused` | 跨 attempt 回读原 receipt；EffectAlreadyClosed 拒再 dispatch |
| `required_slot_satisfaction_matrix` / `required_condition_not_applicable_satisfies_with_snapshot_bound_proof` / `plain_no_effect_and_foreign_proofs_never_satisfy_required` | required 矩阵全格：EFFECT_CLOSED±proof、CNA±绑定、普通 NO_EFFECT、外源 proof |
| `golden_v2_database_migrates_losslessly_to_v3` | golden-v2 无损迁移 + 旧行为不变 + v3 全流程可用 + 新旧 trigger 均强制 |
| `failed_v3_migration_rolls_back_to_complete_v2` | 迁移失败回滚为完整 v2，不留半迁移态 |

**对既有测试的适配（仅 1 处）**：`effect_permit.rs::golden_v1_database_migrates_losslessly` 的版本断言由 2 改为 3（迁移链 v1→v2→v3 的最终戳记），其余断言逐位不变。`task_authority.rs`、`fault_injection.rs` 零改动。

**并行 agent 文件说明**：`tests/effect_fault_injection.rs`（并行 agent 所有）在工作区中一度有 1 项失败（`fault_after_disarm_effect_flow_continues_from_committed_prefix` 对已 EFFECT_CLOSED 的 logical effect 二次 dispatch，命中本切片 `[TASK-RETRY-EFFECT-001]` 的 `EffectAlreadyClosed` 收紧）与 2 项 clippy `too_many_lines`；当前工作区版本已全绿，未由本切片修改。

## 5. 当前不能证明什么（限制与非声明）

- **单 authority**：不声称跨 authority term 的 takeover/adoption、registry coverage proof、分布式 exactly-once；adoption 仅为同一 authority 在重启/不确定后的收口。
- **占位证明**：`authoritative closure proof`、condition-false proof、participant fence 均为调用方供给的 digest 占位，authority 不验证其外部真实性；无签名、无 deterministic-CBOR 完整编码。
- **无 gateway/driver/IPC 集成**：reconcile 的 closure proof 不由 gateway/provider 实际产生；`lookup_effect_history` 的 gateway 侧消费不在本切片。
- **无 compensation 执行**：`COMPENSATED` outcome 仅类型保留，无产生路径，更无执行。
- **无 TaskGroup 语义**、无 TaskPlan/materialization、无 Operation 绑定（`operation_id` 恒 NULL 占位）。
- 不声称 `[TASK-EFFECT-003]` 的跨 term adoption、分布式语义、Slice K 或 B-TASK 包完成；B-TASK-001/002 证据文档的限制条目继续有效。
- legacy `finalize_commit` 通道保留 B-TASK-002 语义（§3.5）：严格 required 语义仅经 `finalize_commit_v3`/`close_permit` 强制。

因此证据等级为单节点原型的 H3 加三平台构建/测试复验（run 30931085056），PARTIAL PASS，不得据此声称 `B-TASK` 包完成或 TaskAttempt effect 语义完整。
