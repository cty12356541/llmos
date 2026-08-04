# B-TASK-001：durable TaskAuthority 与双 Attempt 唯一 CommitPermit 初始证据

> 状态：PARTIAL PASS（本地复验 + 三平台 CI 通过；尚待 fault-injection 与 integrator 审议）
>
> 日期：2026-08-04（三平台 CI 于同日通过：[run 30905979180](https://github.com/cty12356541/llmos/actions/runs/30905979180)）
>
> 对应：`[TASK-ATTEMPT-001]`、`[TASK-SNAPSHOT-001/002]`（冻结输入 digest 子集）、`[TASK-COMMIT-001]`、`[TASK-COMMIT-003]`（单 authority 子集）、`[TASK-CANCEL-002/003]`（pre-permit 子集）、`[TASK-RACE-001]`、`[TASK-EFFECT-ID-001]`（初始空 history 公式子集）

## 1. 本切片完成的边界

新增 `nlos-task` crate（schema v1，`STRICT` 表，WAL + `synchronous=FULL` 回读校验，进程内单写者 admission + `BEGIN IMMEDIATE`，未知 `user_version` fail-closed），实现 `docs/management/stage-b-progress.md` §5 的 B-TASK 首个切片验收门六条：

1. **Task 注册 + TaskHead revision CAS**：`register_task` 幂等（同 spec 返回 `Existing`，同 ID 不同 generation fail-closed `DuplicateTask`）；初始 TaskHead 为 `commit_seq=0`、domain-separated 空 effect-history root（`SHA-256("llmos/task-effect-history/v1" || 0x80)`，`0x80` 为 CBOR 空数组占位 deterministic-CBOR 空 entries）、`retry_fence_epoch=0`；每次控制面变更走 `revision` compare-and-swap。
2. **双 TaskAttempt 注册**：`register_attempt` 按 `(task_id, idempotency_key)` 幂等；每个 attempt 独立 `attempt_id`/`attempt_generation` 与独立 `cancellation_scope_id`（`[TASK-ATTEMPT-001]`），多个 attempt 可绑定同一 `SnapshotBundle`；snapshot 行一经插入不可变（trigger 强制），同 snapshot_id 不同 bytes fail-closed `SnapshotConflict`。
3. **CommitPermit 唯一发放**：`request_commit_permit` 在单个 `BEGIN IMMEDIATE` 事务内完成线性化 CAS——无 outstanding permit 才签发；attempt 的 snapshot 绑定必须与当前 TaskHead 逐位相等（commit_seq/effect-history root/retry-fence epoch），不等则 attempt 落 `CONFLICTED`；他人持有 outstanding permit 则请求者落 `SUPERSEDED` 并返回 winner 身份；同 key 同 bytes 返回原 permit，同 key 不同 bytes fail-closed。磁盘上以部分唯一索引 `commit_permits_single_active` 保证每个 task 至多一个 `ISSUED` permit。permit 关闭后（`CLOSED`、本切片无 outstanding effect）下一轮竞争可再签发（`[TASK-COMMIT-001]` 第二子句）。
4. **losing/cancelled/stale 不得推进 TaskHead、不得覆盖 winner Receipt**：只有 permit 绑定的 attempt+generation 且当前 head 仍逐位匹配 permit expected head 才能 `finalize_commit`（head `commit_seq+1`、新 roots、permit `CLOSED`、attempt `COMMITTED`、commit receipt 同事务）；loser 得到 `NotPermitHolder`/`PermitNotFound`/重放冲突，stale generation 得到 `InvalidGeneration`，均不改变任何状态；receipt 表有 immutable trigger。
5. **cancel 与 permit 竞态只有规范允许的线性化结果**：
   - cancel-first：`cancel_task` 先在 TaskControlRecord 原子递增 `cancel_epoch`（`[TASK-CANCEL-002]`），再关闭全部 open pre-permit attempt（各写一条 `CANCELLED_BEFORE_EFFECT` closure receipt，TaskHead 不变）；之后的 permit 请求被拒为 `CancelledBeforeEffect` 并返回同一 closure receipt，重放结果一致。
   - permit-first：已签发 permit 不被 cancel 清除（`[TASK-COMMIT-003]`），holder 仍可 finalize 推进 TaskHead；effect 级 fencing 明确推迟到 EffectPermit 切片。
   - cancel 幂等：同 key 重放返回 `Replayed` 不再递增；不同 key 在已取消后返回 `AlreadyCancelled`；`cancel_epoch` 全程只递增一次。
6. **重启恢复、无幽灵 permit**：重开数据库后 TaskHead/attempt/permit/receipt 状态完整；重放原 PermitId/幂等 key 返回同一 lifecycle/result；从未持久签发的 permit ID 解析为 `PermitNotFound`（permit/receipt ID 由 `SHA-256` domain-separated 公式从 task+key 确定性派生，未签发的 key 不对应任何行）。

## 2. 线性化事务边界

```text
BEGIN IMMEDIATE
  load task/attempt/snapshot/permit（含 invariant 校验）
  幂等重放判定（同 key 同 bytes → 原结果；同 key 异 bytes → fail-closed 回滚）
  CAS 决策（cancel_epoch / head 逐位等值 / outstanding permit）
  attempt 状态 + epoch 推进 + receipt 写入 + task revision CAS
COMMIT
  → 才向调用者返回 Issued/Superseded/Conflicted/CancelledBeforeEffect/Committed
```

每个公开写 API 恰好一个事务；决策与其 durable 记录不会一个成功一个缺失。

## 3. 规范解释决定（本切片记录）

1. **Snapshot 表示**：按任务书授权，`TaskSnapshot` 以调用方提供的冻结输入 digest 包（`snapshot_digest` + `expected_head_commit_seq` + `effect_history_root` + `retry_fence_epoch`）表示；per-authority checkpoint 收集与签名 `TaskSnapshotReceipt` 不在本切片。digest 公式为 domain-separated SHA-256 占位，未实现规范要求的 deterministic-CBOR 完整编码。
2. **Attempt 状态映射**：§25.1 pre-permit 状态机的 `CREATED → … → READY_TO_COMMIT` 中间态折叠进原子 CAS，durable 可见结果只有 `COMMIT_PERMITTED`/`SUPERSEDED`/`CONFLICTED`/`CANCELLED`/`COMMITTED`；post-permit `EFFECTING/FINALIZING/UNCERTAIN/RECONCILING` 以 permit 状态而非 attempt 状态表示。枚举中保留了全部规范变体并标注不可产生（reserved）。
3. **Permit/Receipt ID 派生**：authority 签发对象的 ID 由确定性公式派生（`llmos/task-commit-permit/v1`、`llmos/task-commit-receipt/v1`、`llmos/task-closure-receipt/v1`），替代签名/随机 nonce；这保证重放与重启后结果一致，同时使“幽灵 permit”不可表示。占位身份方案，待 Receipt authority 切片替换。
4. **PermitAdoption**：跨 authority term 的 permit adoption 不在本切片（单 authority）；permit expiry 仅记录不清除（`[TASK-COMMIT-003]`）。
5. **`nlos-types` 增量**：仅新增 `TaskId`、`TaskSnapshotId`、`CommitPermitId` 三个 nominal 16 字节 ID（沿用既有 `nominal_id!` 宏），未改动任何既有类型。

## 4. 测试矩阵与命令

环境：Apple Silicon / arm64，macOS，rustc 1.97.x workspace toolchain，rusqlite 0.40 bundled SQLite。

```sh
cargo test -p nlos-task          # 14 passed; 0 failed
cargo test --workspace           # 全部通过（含 nlos-types 增量后的既有套件）
cargo clippy --workspace --all-targets -- -D warnings   # 通过
cargo fmt --all -- --check       # 通过
git diff --check                 # 通过
```

三平台复验：commit `4d38721` push 后 GitHub Actions [run 30905979180](https://github.com/cty12356541/llmos/actions/runs/30905979180)（Rust cross-platform verification）于 2026-08-04 通过，覆盖 Ubuntu/Windows/macOS workspace 测试与 Clippy。

测试与验收门映射（`crates/nlos-task/tests/task_authority.rs`）：

| 测试 | 验收门条目 |
|---|---|
| `dual_attempts_register_independently_on_one_snapshot` | 条 2：双 attempt 独立 generation/取消域、同 snapshot、幂等重放与冲突 fail-closed |
| `task_registration_is_idempotent_with_fixed_initial_head` | 条 1：Task 注册幂等 + 固定初始 head 公式 |
| `permit_cas_issues_exactly_one_permit_and_supersedes_loser` | 条 3：唯一签发 + loser SUPERSEDED 带 winner 身份 + holder 推进 head |
| `concurrent_permit_requests_have_exactly_one_winner` | 条 3：双线程真实竞态下恰好一个 ISSUED 一个 SUPERSEDED |
| `permit_replay_returns_original_and_conflicting_bytes_fail_closed` | 条 3：同 key 同 bytes 重放原 permit；异 bytes fail-closed |
| `closed_permit_releases_cas_gate_for_next_competition` | 条 3（`[TASK-COMMIT-001]` 第二子句）：CLOSED 后可再签发 |
| `losing_or_stale_attempt_cannot_finalize_or_overwrite_winner_receipt` | 条 4：loser/stale finalize 全拒、head 不变、winner receipt 不动 |
| `stale_snapshot_detected_after_head_advance` | 条 4：head 前进后旧 snapshot CONFLICTED |
| `cancel_first_blocks_permit_and_closes_attempt_with_head_unchanged` | 条 5（cancel-first）：拒发 permit + closure receipt + head 不变 |
| `cancel_replay_increments_epoch_exactly_once` | 条 5：cancel_epoch 恰好递增一次 |
| `permit_first_survives_cancel_and_holder_can_finalize` | 条 5（permit-first）：permit 不清除、holder 可 finalize |
| `restart_recovers_state_without_ghost_permits` | 条 6：重开后状态/重放一致、无幽灵 permit |
| `unknown_schema_version_fails_closed` | schema 版本 fail-closed |
| `invalid_transitions_fail_closed` | 无 permit finalize、digest 不匹配、fence 回退、取消后注册等均 fail-closed |

## 5. 当前不能证明什么（限制与非声明）

- **本切片不声称 TaskAttempt 语义完整**：只覆盖 pre-permit 子集与单 winner commit；effect 期的 `EFFECTING/FINALIZING/UNCERTAIN/RECONCILING`、逐槽 `EffectSlot` 证明、`EFFECT_UNKNOWN` quarantine、`PermitAdoptionReceipt` 均未实现。
- **不声称 Slice K 或 `B-TASK` 包任何其他条目完成**：EffectPermit、跨 Attempt effect history/retry fence 推进、TaskPlan/TaskNode 惰性物化、Process/AgentInstance 绑定、IsolationDomain/ResourceGroup、TaskGroup membership 均在后续切片。
- digest/ID 为 domain-separated SHA-256 **占位公式**，无签名、无 deterministic-CBOR 完整编码、无 provenance；`write_set_root`/snapshot digest 由调用方供给，authority 不验证其内容真实性。
- 单 authority、单进程 SQLite；不证明跨节点 consensus、authority takeover 或分布式 exactly-once。
- 并发证据为单进程双线程竞态 + 单写者序列化；尚未接入 `nlos-store-fault` 的 fault-injection VFS（kill-9/torn-write/disk-full 注入）。
- 三平台 CI（Ubuntu/Windows/macOS workspace 测试与 Clippy）已通过（run 30905979180）；真实硬件掉电、更多文件系统与长期 soak 仍超出当前证据。
- 时间戳由调用方供给，仅用于观测，不构成时钟域保证。

因此证据等级为单节点原型的 H3 加三平台构建/测试复验，PARTIAL PASS，不得据此声称 `B-TASK` 包完成或 TaskAttempt 语义完整。
