# B-TASK-004：TaskGroup 组织层——membership generation/root CAS、Admission/Removal Receipt、树状取消与聚合状态初始证据

> 状态：PARTIAL PASS 候选（本地复验通过；尚待 integrator 审议与 CI 确认）
>
> 日期：2026-08-05
>
> 对应：`[TASK-GROUP-001]`（无环树 + fanout/depth 绑定 + failure policy 预先决定）、`[TASK-GROUP-002]`（membership CAS + Receipt + OPEN-only admission + quarantine 末句，单 authority 子集）、`[TASK-CANCEL-001]` / `[TASK-CANCEL-002]`（结构化树状取消子集）、`[TASK-STATE-002]`（group 聚合状态子集）、`[TASK-DETACH-001]`（仅保留占位，拒绝产生）
>
> 前置：[B-TASK-001](./b-task-001-task-authority-commit-permit.md)、[B-TASK-002](./b-task-002-effect-permit-dispatch.md)、[B-TASK-003](./b-task-003-reconcile-effect-history.md)

## 1. 本切片完成的边界

在 `nlos-task`（schema v3→v4，纯增量五表 + 索引 + immutable triggers）上扩展 TaskGroup 组织层：

1. **TaskGroup 注册（无环父子树 + 边界强制）**：`register_group` 幂等（同 ID 同 bytes 返回 `Existing`，异 bytes fail-closed `DuplicateGroup`）；每 task 恰好一个 root（预检查 + 部分唯一索引双保险）；child group 的出生与加入父 membership 在**同一事务**（`[TASK-GROUP-002]` BirthDecision 子集）。父必须已存在且 parent 绑定不可变——环在构造上不可产生（A→B→A 需要 B 先于 A 存在，而 B 绑定 A 时 A 已存在则违反父必须先注册）；自父显式拒绝 `GroupCycle`，注册时仍沿祖先链防御性检查。`max_depth` 沿**整条祖先链**强制（child.depth − ancestor.depth ≤ ancestor.max_depth），`max_children` 按 ACTIVE member 计数强制，均 fail-closed（`GroupDepthExceeded` / `GroupFanoutExceeded`）。`group_policy_digest` 注册时绑定、不可变。`QUORUM`/`REDUCE`/`BEST_EFFORT` 保留、注册拒绝（`UnsupportedGroupMode`）。
2. **membership content-addressed root + 单调 generation CAS**：`membership_root = H("llmos/task-group-membership/v1" || 按 (member_type, member_id) 规范排序的 ACTIVE member 定长编码)`；每次变更 generation 严格 +1、root 重算、revision CAS（changed≠1 → CorruptRecord fail-closed）。Admission/Removal Receipt 逐 member 记录 type/id/generation/ControlDomain 占位 + `membership_generation_after`/`root_after`，表级 immutable trigger，receipt ID 由 (group, generation_after, kind, member) 确定性派生。已移除 member 的再 admission 与同 (type,id) 冲突拒绝（`MembershipConflict`）。
3. **OPEN-only admission**：只有 OPEN 可新增/移除 child；SEALED 拒绝（`GroupSealed`），其他非 OPEN 状态拒绝（`GroupNotOpen`）。`seal_group` 幂等（已 SEALED 返回当前记录）。
4. **Attempt 组绑定（逐位校验，漂移 fail-closed）**：新增纯增量 API `register_attempt_in_group(spec, GroupBinding)`——期望 membership generation/root/policy digest 与 group 当前持久值**逐位比较**；generation 漂移 → `StaleMembershipGeneration{expected,current}`，root/policy 漂移 → `MembershipConflict`。持久 binding 记录**准入后**的 generation/root（attempt 所属的 membership 位置），存于独立 `task_attempt_group_bindings` 表（immutable trigger）。重放同 key 同 bytes 返回原 admission（重放不做漂移校验）。未绑组 attempt 走 B-TASK-001 `register_attempt`，行为逐位不变（现有 74 项测试零语义适配，仅 2 处版本戳断言，见 §4）。
5. **树状取消（结构化，非消息）**：`cancel_group` 单事务完成——group `cancel_epoch` 恰递增一次 → 递归传播至全部非终态后代：child group 各自 `cancel_epoch`+1 并落 `CANCELLED`（其子树有 quarantine 则落 `PARTIAL`），open pre-permit member attempt 按 B-TASK-001 语义关闭（`CANCELLED_BEFORE_EFFECT` closure receipt、TaskHead 不变）；持有 permit 的 attempt 不动（permit-first 线性化 `[TASK-CANCEL-003]`，其 finalize 在 group cancel 后仍可进行）；终态 child 不动；detached member（保留类型）按设计跳过；未绑组 attempt 不受影响；task 本身不取消、task cancel_epoch 不动。幂等 key 重放 `Replayed`，异 key `AlreadyCancelled`，均不再递增。整个子树在**同一事务**内处理（fanout 受 max_children/max_depth 策略约束，单写者事务天然线性化）。
6. **聚合状态（派生视图）**：`refresh_group_aggregate` 从 ACTIVE child 持久状态重算派生状态，变化时以 `state_seq`+1 持久（可观测性），child 状态仍是真相权威；终态 group refresh 幂等。child 分类：attempt Committed→成功、Failed→失败、Cancelled/Conflicted/Superseded→取消类、其余→非终态；child group Completed/Failed/Cancelled/Partial 对应映射。ALL：全部成功→COMPLETED，有失败按 failure_mode，无失败但有取消类/Partial→PARTIAL；ANY：任一成功→COMPLETED，全终态无成功→FAILED/PARTIAL/CANCELLED。failure_mode 占位语义：`FAIL_FAST`=任一失败即 FAILED 并同事务传播取消剩余非终态后代；`COLLECT_ALL`=等全部终态后判 FAILED；`ISOLATE`=隔舱——有成功则 PARTIAL、无成功则 FAILED。空 member 集 ALL 真空成立→COMPLETED（已记录）。
7. **quarantine 末句（`[TASK-GROUP-002]`）**：子树任一 quarantine 证据（member attempt 有 QUARANTINED permit tombstone，或 child group 子树递归）使父组**不得 COMPLETED**——派生 COMPLETED 降级为 PARTIAL；group 取消落 PARTIAL 而非 CANCELLED；携带 quarantine 证据的 member 拒绝移除（`GroupQuarantinedChild`，防止移除洗白证据）。

**schema v3→v4 迁移**：纯增量（`task_groups`、`task_group_members`、`task_group_admission_receipts`、`task_group_cancels`、`task_attempt_group_bindings` 五表 + 索引 + immutable triggers），单事务。golden-v3 无损迁移测试 + 失败回滚测试（完整 v3 或完整 v4）。

## 2. 线性化事务边界

沿用 B-TASK-001/002/003 模式：进程内单写者 admission + 每个变更 API 恰好一个 `BEGIN IMMEDIATE`；决策、membership CAS、receipt 写入、epoch 前进、子树传播同事务提交。membership generation 经 group 行 revision CAS 单调推进；receipt/binding 表均有 immutable trigger。

## 3. 规范解释决定（本切片记录）

1. **Attempt 绑定 = 准入后期代**：`[TASK-GROUP-002]` 要求 AttemptContract/Snapshot/WriteSet/Permit/Receipt 绑定同一 membership generation/root。本切片只覆盖 **Attempt 注册**绑定：调用方提供期望的**当前**（准入前）generation/root/policy 做漂移围栏，持久 binding 记录**准入后**（自身加入后的）generation/root——这是 attempt 唯一可稳定引用的 membership 位置。TaskWriteSet/CommitPermit/TaskCommitReceipt 的组绑定**不在本切片**（见 §5 非声明）。
2. **SEALED 冻结语义**：SEALED 后 membership 冻结（拒绝新增/移除），聚合只计 SEALED 时刻的 member 集——"SEALED 后旧 generation/root 的结果不得计入"解释为：旧 generation 的**新产物**无法进入 membership（准入被拒），已准入 member 的绑定保持其准入时位置继续有效。
3. **聚合分类占位**：取消类 = attempt Cancelled/Conflicted/Superseded 与 group CANCELLED（既非成功亦非失败）；ALL 模式下取消类 child 使组最多 PARTIAL。ANY 全终态无成功：有失败→FAILED，有 Partial→PARTIAL，否则→CANCELLED。空 member ALL 真空 COMPLETED（与空 effect set 的语义先例一致）。
4. **quarantine 检测为派生**：不做持久标记，refresh/cancel 时沿子树查询 member attempt 的 QUARANTINED permit + 递归 child group。无环树保证终止；与"child 状态是真相权威"一致。行政终结 `EFFECT_UNKNOWN` 以 `GroupState::Partial` 表示（`EffectUnknown` 变体保留不产生）。
5. **中间态折叠**：CREATED→OPEN、CANCEL_REQUESTED/CANCELLING 在单事务内折叠（与 B-TASK-001 attempt CREATED→READY_TO_COMMIT 折叠先例一致）；QUIESCING/UNCERTAIN/RECOVERING/QUARANTINED 保留不产生。
6. **max_depth 语义**：每个 group 的 `max_depth` 约束**其自身子树**的相对深度（自身为 0），沿祖先链全部强制——root 的 max_depth 因式约束整棵树。
7. **取消传播不触碰 task**：group cancel 不动 task 的 cancel_epoch/state/head；closure receipt 的确定性 ID 复用 `llmos/task-closure-receipt/v1`（task, attempt, epoch），group 传播用各 group 的新 cancel_epoch——attempt 至多属一个 group（binding 表 PK），无冲突。FAIL_FAST 传播同样 bump group cancel_epoch 但不写 cancels 表（非用户取消请求）。
8. **re-admission 拒绝**：同一 (type, id) 移除后再加入被拒（`MembershipConflict`）——member 行的移除证据必须归属单一 membership 谱系；要重加入需新 id（attempt 本就要新 id/generation）。
9. **digest/ID 占位约定**：group membership root、receipt ID 均为 domain-separated SHA-256 派生（`llmos/task-group-membership/v1`、`llmos/task-group-admission/v1`、`llmos/task-group-removal/v1`）；member 定长编码 = type(1)‖id(16)‖generation(8)‖control_domain flag+bytes(1|17)‖detached(1)；空集 root = 纯 domain 哈希。ControlDomain/ResourceGroup/ResourceAccount 均为占位绑定字段，authority 不验证。时间戳全部由调用方供给。
10. **TaskGroupId 为 crate-local 类型**：`nlos-types` 由并行 lane 所有，本切片在 `nlos-task` 内定义同形 nominal ID（与 `EffectSlotId`/`EffectPermitId` 先例一致）。
11. **文件规模**：`group.rs` 约 2.2k 行，沿用本 crate `store.rs`/`effect.rs`/`reconcile.rs` 的有意识内聚先例（单写者存储平面按设计内聚），非疏漏。

## 4. 测试矩阵与命令

环境：Apple Silicon / arm64，macOS，workspace toolchain（rustc 1.97.x），rusqlite 0.40 bundled SQLite。

```sh
cargo test -p nlos-task            # 87 passed; 0 failed（9 个套件全绿）
#   task_authority.rs        14（B-TASK-001 原套件，未改）
#   effect_permit.rs         13（仅 1 处版本戳适配，见下）
#   fault_injection.rs        7（未触碰）
#   effect_reconcile.rs      11（未改）
#   effect_history.rs        10（仅 1 处版本戳适配，见下）
#   effect_fault_injection.rs 11（并行 lane 文件，未触碰）
#   reconcile_fault_injection.rs 8（并行 lane C 文件，未触碰）
#   task_group.rs            13（本切片）
cargo test --workspace           # 57 个套件全部 test result: ok
cargo clippy --workspace --all-targets -- -D warnings   # 通过
cargo fmt --all -- --check       # 通过
```

测试与验收点映射（`tests/task_group.rs`）：

| 测试 | 验收点 |
|---|---|
| `group_registration_enforces_acyclic_tree_and_single_root` | 自父拒绝（GroupCycle）；缺父拒绝（GroupNotFound，A→B→A 构造上不可产生）；跨 task 父拒绝；单 root；幂等重放/异 bytes fail-closed；TaskNotFound/InvalidGeneration/TaskCancelled；QUORUM/REDUCE/BEST_EFFORT 保留拒绝 |
| `depth_and_fanout_bounds_fail_closed` | max_depth 沿祖先链强制（root 界约束 grandchild）；max_children fail-closed；移除释放 fanout 槽 |
| `membership_generation_cas_is_monotonic_and_root_recomputes` | generation 0→1→2→3→4 严格递增；每步 root == `membership_root_of(ACTIVE)` 重算；receipt 钉住 `generation_after`/`root_after` |
| `admission_and_removal_receipts_are_immutable_and_replay_safe` | receipt 表 immutable trigger；admission 重放返回原 receipt（重放不漂移校验）；同 key 异 bytes IdempotencyConflict；removal 重放返回原 receipt；member 逐字段（type/id/generation/ControlDomain 占位/detached）；generation 漂移 MembershipConflict；未知 member GroupMemberNotFound |
| `sealed_group_rejects_new_children_and_removals` | SEALED 拒绝 child group/attempt member/移除；seal 幂等；membership 冻结 |
| `attempt_group_binding_drift_fails_closed` | stale generation → StaleMembershipGeneration{0,1}；root/policy 漂移 → MembershipConflict；外部 group → GroupNotFound；binding 记录准入后位置；未绑组 attempt 无 binding（B-TASK-001 行为不变） |
| `group_cancel_propagates_to_non_terminal_descendants` | 单事务传播：pre-permit member closure receipt + head 不变；终态 child 不动；permit 持有 attempt 不动；未绑组 attempt 不动；双 group Cancelled + epoch=1；task Active/cancel_epoch=0 |
| `group_cancel_replay_and_permit_first_finalization` | 重放不再递增；异 key AlreadyCancelled；group cancel 后 outstanding permit 仍可 finalize（permit-first）；终态 group 取消 InvalidGroupState |
| `aggregate_all_and_any_modes` | ALL 全部成功才 COMPLETED（含中途 Open 不变、混合取消类→PARTIAL、终态稳定、空组真空 COMPLETED）；ANY 任一成功即 COMPLETED、其余 child 不动 |
| `failure_modes_fail_fast_collect_all_isolate` | FAIL_FAST：失败即 FAILED + 同事务取消剩余 pre-permit member（closure receipt）；COLLECT_ALL：等待全部终态（中途 Open）后 FAILED；ISOLATE：失败+成功→PARTIAL |
| `quarantined_descendant_caps_parent_at_partial` | ANY 成功 + quarantine → PARTIAL 不得 COMPLETED；quarantined member 移除拒绝 GroupQuarantinedChild；quarantine 子树取消落 PARTIAL（child + parent） |
| `golden_v3_database_migrates_losslessly_to_v4` | golden-v3 无损迁移：v3 数据逐位完整、v3 流（close_slot+finalize_v3 proved COMMITTED）不变、v4 平面从空起步全流程可用、v2/v3/v4 trigger 均强制 |
| `failed_v4_migration_rolls_back_to_complete_v3` | 预置冲突表 → open 失败 → user_version 仍为 3、v3 数据完好、不留半个 v4 表 |

**对既有测试的适配（仅 2 处，均为版本戳）**：
1. `effect_permit.rs::golden_v1_database_migrates_losslessly`：`user_version` 断言 3→4（迁移链 v1→…→v4 的最终戳记），其余断言逐位不变。
2. `effect_history.rs::golden_v2_database_migrates_losslessly_to_v3`：同上 3→4。

`task_authority.rs`、`fault_injection.rs`、`effect_reconcile.rs`、`effect_fault_injection.rs`、`reconcile_fault_injection.rs` 零改动；v1–v3 公开 API 签名与语义零变化（组绑定为纯增量新 API）。

## 5. 当前不能证明什么（限制与非声明）

- **无 QUORUM/REDUCE 执行语义**（`[TASK-GROUP-003]`）：变体保留、注册拒绝；不声称任何 quorum 认识论。
- **无 AGENT_INSTANCE member**（B-PROCESS 前置）：变体保留、不可产生；聚合遇到保留 member 按非终态处理（防御分支，实际不可达）。
- **无 DETACH 执行**（`[TASK-DETACH-001]`）：member `detached` 标志保留、准入拒绝产生；取消传播按设计跳过 detached member 的分支为防御性保留。
- **无 ResourceGroup/ResourceAccount 强制**、无 Namespace delegation、无 ControlDomain 权威：均为占位绑定字段。
- **无 TaskWriteSet/CommitPermit/TaskCommitReceipt 的组绑定**：仅 Attempt 注册绑定 membership generation/root/policy（见 §3.1）；`[TASK-GROUP-002]` 的全 artifact 绑定链未闭合。
- **无 LOST/quiescence 语义**：`[TASK-GROUP-002]` 的 LOST→UNCERTAIN/RECOVERING 与 fence barrier + closed-or-quarantined quiescence 不在本切片（相应 group 状态变体保留不产生）。
- **聚合为显式 refresh 派生**：attempt 状态迁移（finalize/close/reconcile）不自动触发父组重算——authority of truth 是 child 状态，持久 group 状态是最近 refresh 的可观测快照；FAIL_FAST 的"即时"语义以 refresh 调用点线性化。
- **单 authority、单 task 域**：无跨 authority 联邦、无跨 task 依赖；签名、deterministic-CBOR 完整编码不在本切片（digest 均为定长占位编码）。
- **取消传播单事务子树**：fanout/depth 由策略约束下的整子树同事务处理；未实现超大规模子树的分段 saga（线性化论证见 §2：单写者 `BEGIN IMMEDIATE`）。
- 不声称 Slice K、B-TASK 包完成或 TaskGroup quorum 认识论。
