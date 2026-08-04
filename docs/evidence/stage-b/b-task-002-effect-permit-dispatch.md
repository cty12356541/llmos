# B-TASK-002：EffectPermit 签发与逐槽 EffectSlot 状态机初始证据

> 状态：PARTIAL PASS 候选（本地复验通过；尚待 fault-injection 三点注入与 integrator 审议）
>
> 日期：2026-08-04
>
> 对应：`[TASK-EFFECT-001]`、`[TASK-EFFECT-ID-001]`、`[TASK-EFFECT-002]`（PLANNED/PERMITTED/DISPATCHED/NO_EFFECT/EFFECT_CLOSED/EFFECT_UNKNOWN 子集）、`[TASK-RACE-001]`（permit 维度）、`[TASK-CANCEL-003]`（cancel/dispatch 线性化子集）、`[TASK-COMMIT-002]`（finalize 侧 slot 闭合子集，无 effect-history 部分）
>
> 前置：[B-TASK-001](./b-task-001-task-authority-commit-permit.md)（durable TaskAuthority 与唯一 CommitPermit）

## 1. 本切片完成的边界

在 `nlos-task`（schema v1→v2）上扩展出 effect 平面，覆盖议题 31 证据门条 4/7 的 permit 维度验收要点：

1. **planned effect slot 集在 permit 签发时承诺**：`PermitRequest` 新增 `planned_effects: Vec<PlannedEffect>`；`effect_seq` 即 Vec 下标，稠密从 0 由构造保证（无洞不可表示）；descriptor 必须绑定本 task/generation；同一集合内 `LogicalEffectId` 唯一；违例 fail-closed `InvalidEffectSet` 且整张 permit 不签发、epoch 不前进（`[TASK-EFFECT-002]` 前置）。
2. **确定性身份公式**：`LogicalEffectId = SHA-256("llmos/task-logical-effect/v1" || canonical(descriptor))`，`idempotency_identity_digest = SHA-256("llmos/task-effect-idempotency-identity/v1" || LogicalEffectId)`；`LogicalEffectDescriptor` 结构上**没有** AttemptId/ActionId/OperationId/incarnation/nonce 字段——`[TASK-EFFECT-ID-001]` 的禁止项按构造排除，而非运行时校验。两个不同 attempt 声明同一 descriptor 得到同一身份。
3. **只有 CommitPermit 持有者能签发 EffectPermit**（`[TASK-RACE-001]`）：单事务内校验 outstanding permit（permit_id + permit_epoch + attempt + generation 逐位匹配）、当前 head/fence 与签发时逐位相等、slot 属于该 permit 已承诺的 effect set；CAS 将 slot `PLANNED → PERMITTED`，铸造一次性 dispatch token，并重算 issued/outstanding roots 与 `control_epoch+1`（`[TASK-EFFECT-001]` 前半）。按 `(task, idempotency_key)` 幂等：重放返回原 permit 与同一 token；同 key 异 bytes fail-closed。
4. **dispatch token 一次性原子消费**：`consume_dispatch_token` 单事务完成持有者/epoch/head/cancel_epoch 在线校验 + token digest 比对 + slot `PERMITTED → DISPATCHED` CAS（`[TASK-EFFECT-001]` 后半）。二次消费同一 token fail-closed `DispatchTokenConsumed`（双线程竞态实测恰好一胜一拒）；错误 token `DispatchTokenMismatch` 且状态不动；已消费（DISPATCHED）的 slot 永远进不了 NO_EFFECT——已消费 token 不得伪装未执行。
5. **cancel 线性化**（`[TASK-CANCEL-003]` 子集）：cancel 在 EffectPermit 签发后提交 → dispatch 时得到类型化 `CancellationCommitted{cancel_epoch}`，slot 保持 PERMITTED；cancel 提交后禁止签发新 EffectPermit（`[TASK-CANCEL-002]` 封锁）；cancel 路径以“出示未消费 token”为证明将 slot 写为 NO_EFFECT（CancelledBeforeDispatch）；cancel 前已消费的 token 只能按真实 effect 登记结局（cancel 后仍可记录 EFFECT_CLOSED/EFFECT_UNKNOWN），不得改名。
6. **slot 闭合**：DISPATCHED → EFFECT_CLOSED（权威 closure digest 占位 receipt）或 EFFECT_UNKNOWN（崩溃窗口不确定登记）；PLANNED/PERMITTED → NO_EFFECT（token 可证明未消费时，写 `TaskNoEffectReceipt` 形记录）。EFFECT_UNKNOWN 持久、本切片内终态、跨重启仍阻塞 permit 关闭；重放同 digest 返回原 receipt，异 digest fail-closed，改写为 CLOSED 拒绝（reconcile 属下一切片）。
7. **finalize 收紧**（`[TASK-COMMIT-002]` 无 effect-history 子集）：任何 PLANNED/PERMITTED/DISPATCHED/EFFECT_UNKNOWN slot 都以类型化 `OutstandingEffectSlots{count}` 禁止关闭 permit；仅当全部声明 slot 为 EFFECT_CLOSED 或 NO_EFFECT 才允许 COMMITTED。无声明 effect 的 permit（全部 B-TASK-001 流程）行为不变，由未改动的 14 项旧测试证明。
8. **schema v1→v2 迁移**：纯增量（新增 `effect_slots`、`effect_permits`、`effect_receipts`、`permit_effect_sets` 四表 + 索引 + immutable trigger），单事务完成，任一点失败整体回滚为完整 v1。golden-v1 测试：以冻结的 v1 DDL 与种子数据建库 → 迁移 → v1 数据（task/snapshot/attempt/permit/receipt）逐位完整、旧幂等重放与 finalize 不变、v1 immutable trigger 仍强制、新 effect 平面在迁移后的库上全流程可用；另有迁移失败回滚测试（预置冲突表 → open 失败 → `user_version` 仍为 1、v1 数据完好、半个 v2 表不留）。

## 2. 线性化事务边界

沿用 B-TASK-001 模式：进程内单写者 admission + 每个变更 API 恰好一个 `BEGIN IMMEDIATE`，决策、状态 CAS、roots 重算、receipt 写入与 `control_epoch` 前进同事务提交，崩溃不会把决策与其持久记录拆开。每次 slot 状态转换按 `[TASK-EFFECT-002]` 末句重算 `effect_slot_state_root` 与 required/satisfied/terminal 计数及 issued/dispatched/closed/outstanding roots（`permit_effect_sets` 行 revision CAS）。

```text
BEGIN IMMEDIATE
  幂等重放判定（同 key 同 bytes → 原结果；异 bytes → fail-closed）
  持有者/epoch/head/cancel_epoch 在线校验（类型化拒绝）
  slot 状态 CAS（WHERE 旧状态+旧 state_seq，changed!=1 即 CorruptRecord fail-closed）
  receipt 写入 + roots 重算 + task control_epoch 前进（revision CAS）
COMMIT → 才返回 Issued/Replayed/Dispatched/Recorded/…
```

## 3. 规范解释决定（本切片记录）

1. **canonical(descriptor) 占位编码**：固定宽度、按声明顺序、整数大端（16+8+32+8+32+4+4=104 字节），占位 deterministic CBOR；与 B-TASK-001 §3 的 digest 占位约定一致。`intent_spec_id`/`target_authority_object_id` 为 32 字节 digest 占位；`effect_class`/`idempotency_scope` 为调用方 u32 编码占位。
2. **`effect_seq` 由 Vec 下标派生**：稠密无洞由构造保证；`effect_slot_id = SHA-256("llmos/task-effect-slot/v1" || permit_id || effect_seq)` 前缀 16 字节，slot 身份锚定其所属 permit（与 B-TASK-001 permit/receipt 派生约定一致）。
3. **dispatch token**：`SHA-256("llmos/task-effect-dispatch-token/v1" || effect_permit_id || attempt_id || attempt_generation)`，库里只存 `SHA-256("llmos/task-effect-dispatch-token-digest/v1" || token)`；确定性派生使崩溃后重放同一幂等 key 拿回同一 token。dispatch 不设独立幂等 key——token 本身就是一次性凭证；二次消费是 fail-closed 错误而非重放。
4. **roots 为派生视图的持久化**：issued = 曾签发 EffectPermit 的 slot（含后续 NO_EFFECT/CLOSED/UNKNOWN）；dispatched = 已消费 token（DISPATCHED/EFFECT_CLOSED/EFFECT_UNKNOWN）；closed = EFFECT_CLOSED；outstanding = PERMITTED/DISPATCHED/EFFECT_UNKNOWN；各为 `H(domain || 按 effect_seq 排序的 seq||logical_effect_id)` 占位公式。`satisfied_required_effect_count` 占位语义 = required 且 EFFECT_CLOSED 的槽数；**required slot 的 success_criteria 满足性判定与逐槽重算等值校验不在本切片**（见 §5）。
5. **token 未消费证明**：authority 自证——PLANNED 无 token 可消费；PERMITTED 要求持币人出示与存储 digest 匹配的未消费 token。`dispatch_token_unconsumed_proof` 字段以 `H("llmos/task-no-effect-proof/v1" || slot || state_seq || token)` 占位。`CONDITION_NOT_APPLICABLE` 仅强制 slot 预绑定了 `required_condition_digest`（否则 `ConditionNotBound`）；绑定原 TaskSnapshot 的权威 false proof 属后续切片。
6. **cancel 后的读写不对称**：签发与 dispatch 校验 `cancel_epoch` 与（effect）permit 签发时相等（先 cancel 先赢）；outcome/no-effect 登记**不**校验 cancel/head——已消费 token 的真相登记与 cancel 路径的 no-effect 收口在 cancel 后必须可用（`[TASK-CANCEL-003]`）。
7. **`permit_effect_sets` 行仅在声明了非空 effect set 时插入**：缺失行即“无声明 effect”，v1 permit 无需回填，迁移纯增量。v1 permit 的幂等重放按“存储 root（缺省 = 空集 root）vs 请求派生 root”比较，空 `planned_effects` 与 v1 行为逐位兼容。
8. **`PermitRequest` 失去 `Copy`**（`Vec` 字段）：旧测试仅 helper 增字段与两处 `.clone()`，无语义变化。该字段是任务书明确要求；对并行 agent 的 `fault_injection.rs` 造成一次性字段补齐需求，集成后当前工作区双方测试均绿。`Cargo.toml` 未触碰（无新依赖需要）。
9. **post-permit attempt 状态**：EFFECTING/FINALIZING/UNCERTAIN 仍以 permit/slot 状态表示（沿用 B-TASK-001 §3.2）；`RECONCILING`/`CONFIRMED_NO_EFFECT` 在 `SlotState` 中保留为不可产生变体。
10. **文件规模**：`effect.rs` 约 1.5k 行，遵循本 crate `store.rs`（1.5k 行）权威模块先例——单写者存储平面按设计内聚；此为有意识的偏差记录，非疏漏。

## 4. 测试矩阵与命令

环境：Apple Silicon / arm64，macOS，workspace toolchain（rustc 1.97.x），rusqlite 0.40 bundled SQLite。

```sh
cargo test -p nlos-task            # 34 passed; 0 failed
#   task_authority.rs  14（B-TASK-001 原套件，未改语义）
#   effect_permit.rs   13（本切片）
#   fault_injection.rs  7（并行 agent 切片，同工作区复验）
cargo clippy --workspace --all-targets -- -D warnings   # 通过
cargo fmt --all -- --check                              # 通过
```

测试与验收要点映射（`crates/nlos-task/tests/effect_permit.rs`）：

| 测试 | 验收要点 |
|---|---|
| `logical_effect_identity_is_deterministic_and_attempt_independent` | TASK-EFFECT-ID-001：固定 domain-separated 公式、跨 attempt 稳定、descriptor 按构造无禁止字段 |
| `permit_issuance_commits_dense_unique_effect_set` | TASK-EFFECT-002 前置：effect_set_root 承诺完整、effect_seq 稠密、重复 LogicalEffectId/跨 task 绑定 fail-closed 且不签发 |
| `only_commit_permit_holder_obtains_effect_permit` | TASK-RACE-001：竞争同一 LogicalEffectId 的 loser/陈旧 generation/错误 epoch/未声明 slot 全部类型化拒绝 |
| `issuance_cas_moves_slot_to_permitted_and_replays_original` | TASK-EFFECT-001 前半：PLANNED→PERMITTED CAS、issued/outstanding/state roots 更新、control_epoch 前进、幂等重放同 token、异 bytes fail-closed、同槽二次签发拒绝 |
| `dispatch_token_is_single_use_and_fail_closed` | TASK-EFFECT-001 后半：错误 token 不动状态、正确 token 一次性消费、二次消费 fail-closed |
| `concurrent_token_consumption_has_exactly_one_winner` | 双线程真实竞态：恰好一个 DISPATCHED、一个 DispatchTokenConsumed |
| `cancel_fences_late_dispatch_and_preserves_pre_cancel_window` | TASK-CANCEL-003：cancel 后迟到 permit 类型化拒绝且 slot 保持 PERMITTED、禁发新 EffectPermit、未消费 token 收口 NO_EFFECT、已消费 token 不得伪装未执行且 cancel 后可登记真实结局 |
| `no_effect_requires_verifiably_unconsumed_token` | TASK-EFFECT-002：PLANNED/PERMITTED→NO_EFFECT 证明规则、重放/冲突、ConditionNotBound |
| `finalize_blocked_until_every_declared_slot_is_terminal` | TASK-COMMIT-002 子集：PLANNED/PERMITTED/DISPATCHED 逐级阻塞计数、全终态后 COMMITTED 推进 head |
| `effect_unknown_is_durable_and_blocks_closure_across_restart` | 崩溃窗口登记：EFFECT_UNKNOWN 跨重启持久阻塞、重放/异 digest/改写 CLOSED 全拒 |
| `restart_recovers_permitted_slot_and_token` | 重启恢复：PERMITTED 槽与 token 重放一致、重启后全流程收口 |
| `golden_v1_database_migrates_losslessly` | 迁移：v1 数据逐位完整、旧重放/finalize/trigger 不变、新 effect 平面在迁移库上可用 |
| `failed_migration_rolls_back_to_complete_v1` | 迁移 fail-closed：冲突即整体回滚，完整 v1 或完整 v2，不留半迁移态 |

## 5. 当前不能证明什么（限制与非声明）

- **不声称 `[TASK-EFFECT-003]`**：quarantine tombstone、`PermitAdoptionReceipt`、RECONCILING/CONFIRMED_NO_EFFECT reconcile 流均无 API；EFFECT_UNKNOWN 在本切片内是只记录+阻塞的终态。
- **不声称跨 Attempt effect history**：`TaskEffectHistoryEntry`/`TaskEffectHistoryRoot` 追加、`retry_fence_epoch` 严格递增推进、`[TASK-RETRY-EFFECT-001]` 的 PARTIAL_EFFECT/FAILED_AFTER_EFFECT 路径均属 B-TASK-003；同一 LogicalEffectId 的跨 permit 去重当前只在“单 permit 集合内唯一”层面强制。
- **required slot 成功语义占位**：COMMITTED 只要求全终态（EFFECT_CLOSED|NO_EFFECT），未强制 required 槽必须 EFFECT_CLOSED 或持 CONDITION_NOT_APPLICABLE 权威证明；`success_criteria_digest` 内容不由 authority 验证。
- **无三点崩溃注入模拟**（签发/dispatch/闭合窗口的 kill-9 中点恢复）与 `nlos-store-fault` VFS 接入——归属后续切片及并行 fault-injection 工作；本文仅覆盖逻辑层崩溃窗口登记与重启恢复。
- digest/ID 仍为 domain-separated SHA-256 **占位公式**：无签名、无 deterministic-CBOR 完整编码、无 gateway/driver/provider 集成；`authoritative_closure_digest` 等证明字段由调用方供给，authority 不验证其外部真实性。
- 单 authority、单进程 SQLite；不证明跨节点 consensus、authority takeover 或分布式 exactly-once。时间戳由调用方供给，仅用于观测。
- 不声称 Slice K 或 `B-TASK` 包完成；B-TASK-001 证据文档的限制条目继续有效。

因此证据等级为单节点原型的 H3（本地复验），PARTIAL PASS 候选，不得据此声称 `B-TASK` 包完成或 TaskAttempt effect 语义完整。
