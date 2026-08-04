# B-TASK-003：schema-v3 表组（quarantine / adoption / reconcile / effect-history / finalize-proofs）故障注入增量证据

> 状态：PARTIAL PASS 候选（本地复验通过；尚待 integrator 审议）
>
> 日期：2026-08-05
>
> 对应：`b-task-003-reconcile-effect-history.md` 标题状态行与 `b-task-003-crash-windows.md` §6 中明确 deferred 的"v3 表组 fault-injection"条目——对 schema v3 六表（`task_quarantine_receipts` / `task_adoption_receipts` / `task_reconcile_receipts` / `effect_history` / `task_effect_sequences` / `task_finalize_proofs`，743c88c 落地）在 `nlos-store-fault` VFS 下的耐久行为补齐 F1–F4 对齐矩阵的测试层证据
>
> 前置：[b-task-003-reconcile-effect-history.md](./b-task-003-reconcile-effect-history.md)（v3 流逻辑层证据，其 §3 占位语义即本文断言对象）、[b-task-003-crash-windows.md](./b-task-003-crash-windows.md)（崩溃窗口 + effect 表组矩阵与本文的范式来源）、[b-task-001-fault-injection.md](./b-task-001-fault-injection.md)（F1–F4 矩阵原型）

## 1. 本切片完成的边界

写集恰好两个新文件：`crates/nlos-task/tests/reconcile_fault_injection.rs`（8 个测试 = 7 个矩阵行 + 1 个 kill-9 子进程 helper）与本文档。**未触碰任何 `src/**`、既有测试、`Cargo.toml`、其他 crate 或既有证据文件**；只驱动公开 API（`finalize_commit_v3` / `adopt_permit` / `reconcile_effect` / `close_permit` / `inspect_quarantine_receipt` / `inspect_adoption_receipt` / `inspect_reconcile_receipt` / `list_effect_history` / `compute_effect_history_root` 及 B-TASK-001/002 既有 API）+ 裸 rusqlite 做表级断言。每条断言都是类型化的（精确错误变体或精确耐久状态），无"没 panic 就算过"。

范式完全复用既有 harness：唯一 VFS 名 `nlos-task-reconcile-fault`、进程级 `FAULT_LOCK` 串行、`TestDatabase` 临时库清理、kill-9 子进程以管道 `READY` 标记同步（无 sleep）、`wal_commit_frames` 定位 commit 帧做文件级 WAL 撕裂、错误链内容断言（`error_chain`）、每场景末尾独立连接 `PRAGMA integrity_check = ok` 复核。

**与两条并行 lane 的关系**：复验期间 lane A（`crates/nlos-task/src/**` TaskGroup / schema v4）两次处于 mid-flight 不可编译状态（`mod group` 未接线；`todo!()` 占位导致非穷尽 match），lane B（`crates/nlos-artifact` 新 crate）同在工作区推进。按任务书以 `cargo test -p nlos-task --test reconcile_fault_injection` 限定范围重试，lane 推进后全部恢复。**schema v4（TaskGroup 表）在测试编写期间落地，本文全部 v3 语义断言在 v4 落地后的快照上保持绿色，未删除任何断言，无 counter-evidence。** lane B 的 `nlos-artifact` 文件存在 rustfmt 差异（见 §4），不属本写集。

## 2. 故障矩阵（对齐 PoC-0003 F1–F4 与 b-task-001/003 既有行）

环境：Apple Silicon / arm64，macOS，workspace toolchain（rustc 1.97.x），rusqlite 0.40 bundled SQLite。

| # | 场景 | 注入点 | 预期不变量 | 结果 |
|---|---|---|---|---|
| 1 | kill-9 中断 v3 表组写事务 | 子进程在 slot0 `EFFECT_UNKNOWN` 已提交后，于 `BEGIN IMMEDIATE` 未提交时（已弄脏 `commit_permits.permit_state` CAS、写入幻影 quarantine tombstone / 幻影 history 条目 / 幻影 sequence 行）被 SIGKILL | 重开后中断事务完全回滚：v3 六表全空、permit 回到已提交 `ISSUED`、slot 保持 `EFFECT_UNKNOWN`（state_seq=3）、无半截 quarantine/adoption/reconcile/history 状态；同一 finalize 重做后真实产生 tombstone，重放观察到同一类型化 `Quarantined` 拒绝（确定性派生 ID 一致） | PASS（`fault_kill9_mid_v3_transaction_leaves_no_half_state`） |
| 2 | commit 后崩溃（quarantine/adoption/reconcile 闭合 + COMMITTED finalize） | 子进程在 v3 全生命周期（quarantine tombstone → adoption → slot0 reconcile `EFFECT_CLOSED` + slot1 reconcile `CONFIRMED_NO_EFFECT`，各含同事务 history 追加 → proved `COMMITTED` finalize 含 finalize-proof 行）全部提交返回后被 SIGKILL | 重开后逐位保留：quarantine 1 行（unknown_slots=[0,1]、fenced digest 一致）、adoption 1 行（epoch=1）、reconcile 2 行（proof digest 逐位一致）、history 2 行（seq 1/2 无洞、outcome `EffectClosed`/`ConfirmedNoEffect`）、finalize-proof 1 行、commit receipt `Committed`（head=1）；finalize/adoption/reconcile 重放全部返回原结果，异 proof `HistoryConflict`；重放不双重追加 history | PASS（`fault_kill9_after_v3_commit_preserves_everything`） |
| 2b | commit 后崩溃（`PARTIAL_EFFECT` finalize） | 子进程在 required slot0 证明满足、required slot1 跳过、可选 slot2 闭合的 `PARTIAL_EFFECT` finalize（fence 0→1、`PARTIAL_EFFECT` history 条目追加、finalize-proof 行）提交后被 SIGKILL | 重开后 receipt `PartialEffect`（prior_fence=0/new_fence=1/head=1）、attempt `Committed`、head root == 重算 root；history 3 行 seq 1..=3 无洞、第 3 条 `PartialEffect` 且 fence=1、归属 slot1 的 `LogicalEffectId`；同 bytes finalize 重放返回原 receipt，**fence 不再 +1、history 不双重追加** | PASS（`fault_kill9_after_partial_effect_finalize_preserves_fence_and_history`） |
| 3 | 写入硬 I/O 错误 | `FailWritesAfter { 0, IoErr }` 分别拦截（a）产生 tombstone 的 `finalize_commit_v3` 与（b）reconcile 事务（slot CAS + 闭合 receipt + reconcile receipt + history 追加同事务）的首次 xWrite | 均以 `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件），不返回假成功；无半截状态（无 quarantine/reconcile/history 行、permit 分别保持 `ISSUED`/`QUARANTINED`、slot 保持 `EFFECT_UNKNOWN`、`control_epoch` 不动）；disarm 后同一操作成功 | PASS（`fault_io_error_on_quarantine_and_reconcile_writes_fails_closed`） |
| 4 | disk-full（ENOSPC） | `FailWritesAfter { 0, Full }` 分别拦截（a）adoption 写事务（receipt + sequence epoch 推进 + `control_epoch` 同事务）与（b）proved `COMMITTED` finalize 写事务（commit receipt + finalize-proof + permit 关闭 + attempt 终态 + head 推进同事务） | 均以 `SQLITE_FULL` 显式失败（错误链含 full）；无半截状态（无 adoption/finalize-proof/receipt 行、`task_effect_sequences` 不推进、head 不前进、permit 保持 `ISSUED`、attempt 保持 `COMMIT_PERMITTED`）；disarm 后同一操作成功 | PASS（`fault_enospc_on_adoption_and_finalize_proof_writes_fails_closed`） |
| 5 | 静默丢写 / WAL 撕裂 | (a) `PowerLossAfter { 0 }`：reconcile 事务"报告成功"但写入从未落盘；(b) kill-9 后文件级 WAL 撕裂：截断到最后一个 commit 帧（adoption 事务）的一半并删 `-shm` | (a) 杀连接重开后幻影 reconcile/history 不得冒充已提交事实：slot 回到 `EFFECT_UNKNOWN`、无 reconcile 行、无 history 行、permit 保持 `QUARANTINED`、`control_epoch` 不前进；同一请求重做且确定性派生的 reconcile receipt id 逐位相同、重开后真实持久；(b) 撕裂尾部整体隐藏（幻影 adoption 不可见、`ReceiptNotFound`），合法前缀（quarantine tombstone、permit `QUARANTINED`、slot `EFFECT_UNKNOWN`）完整保留；同一幂等 key 重做 adoption 且确定性派生 receipt id 逐位相同（epoch=1） | PASS（`fault_silent_write_loss_and_torn_tail_hide_phantom_v3_facts`） |
| 6 | 故障解除后从已提交前缀继续 | `FailWritesAfter { 0, Full }` 注入一次失败的 reconcile 写后 disarm，**同一 authority 实例**继续读写 | 已提交前缀与故障前逐位一致（slot `EFFECT_UNKNOWN`、permit `QUARANTINED`、tombstone/adoption 在库、`control_epoch` 不动）；reconcile 重试最终闭合（`EFFECT_CLOSED` + history 追加 + tombstone 解除回 `ISSUED`），proved `COMMITTED` finalize 成功；新竞争（第二张 permit 绑定推进后的 head/root/fence，`permit_epoch=2`，声明真正新业务 slot）再走完整 v3 finalize 成功；完整重开后 head=2、双 slot `EFFECT_CLOSED`、history 2 行无洞、finalize-proof 2 行 | PASS（`fault_after_disarm_reconcile_retry_closes_from_committed_prefix`） |

每行场景末尾均以独立连接跑 `PRAGMA integrity_check = ok`（行 5 两个 phase 各一次）。

## 3. 崩溃/重放下幂等性的精确断言（对应 §3 占位语义）

- **quarantined finalize 重放**：tombstone 已提交后，同 bytes finalize 重放返回同一类型化 `TaskStoreError::Quarantined`（行 1、2 均断言；行 2 为跨 kill-9 重开后的重放）。
- **adoption 重放**：同幂等 key + 同 bytes → `AdoptionReplay::Replayed`（行 2）；`task_effect_sequences` 的 adoption epoch 不因失败/重放而推进或回退（行 4、5b）。
- **reconcile 重放**：同 (slot, adoption, outcome, proof) → `ReconcileReplay::Replayed`；异 proof → `HistoryConflict`（行 2，跨 kill-9 重开后断言）。
- **`PARTIAL_EFFECT` finalize 重放**：返回原 receipt；fence 保持 1 不再 +1；history 保持 3 行不双重追加、seq 无洞（行 2b）。
- **重做确定性派生 ID 一致**：幻影 reconcile 重做后 receipt id 逐位相同（行 5a）；撕裂隐藏的 adoption 重做后 receipt id 逐位相同（行 5b）；kill-9 回滚后的 quarantine 重做产生同派生 ID 的 tombstone（行 1）。

## 4. 复验命令与结果

```sh
cargo test -p nlos-task --test reconcile_fault_injection   # 8 passed; 0 failed（7 矩阵行 + 1 child helper no-op）
cargo test -p nlos-task                                    # 全部套件通过（74 = 既有 66 + 本切片 8；task_authority 14 / effect_permit 13 / fault_injection 7 / effect_fault_injection 11 / effect_reconcile 11 / effect_history 10 / reconcile_fault_injection 8）
cargo test --workspace                                     # 52 个 test result: ok，0 failed
cargo clippy --workspace --all-targets -- -D warnings      # 通过（exit 0）
cargo fmt -p nlos-task -- --check                          # 通过（exit 0）
```

说明：上述为同一工作区快照（lane A schema v4 已落地、lane B `nlos-artifact` 推进中）的最终复验结果。`cargo fmt --all -- --check` 当前仅在 lane B 的 `crates/nlos-artifact/**` 文件上有差异（其 lane 的 in-flight 状态，不属本写集）；本切片写集文件自身通过 rustfmt 与 `cargo clippy -p nlos-task --test reconcile_fault_injection -- -D warnings`。复验期间 lane A 两次短暂使 lib 不可编译（见 §1），按任务书限定范围重试，未触碰其文件。

## 5. 当前不能证明什么（限制与非声明）

- **kill-9 ≠ 机器断电**：kill-9 模拟进程崩溃（OS page cache 存活）；"内核已接受但盘未见"的语义由行 5a 的 `PowerLossAfter` 与行 5b 的 WAL 撕裂覆盖；真实断电下的介质行为不在证据内。
- **macOS 本地 VFS**：注入经 `nlos-store-fault` shim 拦截 `xWrite`/`xSync`/`xTruncate`；纯读路径无法直接注入失败（与 b-task-001/003 相同边界），行 3/4 对读路径断言的是 fail-closed 契约。真实 ENOSPC RAM-volume 探针未在 v3 表组重做，disk-full 以注入 `SQLITE_FULL` 为准。APFS 以外文件系统、`-shm`/mmap 损坏组合不在证据内。
- **不声称 F4 全集**：checkpoint/backup/长 reader 矩阵未对 v3 表组重做。
- **无 TaskGroup 表注入**：lane A 的 schema v4（TaskGroup）表组在并行推进，其故障矩阵不在本切片；本文断言的是 v3 语义在 v4 落地后的保持（全部绿色，无 counter-evidence）。
- 单 authority、单进程 SQLite；不证明跨节点 consensus、跨 term takeover/adoption 或分布式 exactly-once。closure proof / participant fence / condition-false proof 均为调用方供给的 digest 占位，authority 不验证其外部真实性（沿用 b-task-003-reconcile-effect-history §5 限制）。
- 不声称 Slice K 或 `B-TASK` 包完成；B-TASK-001/002/003 既有证据文档的限制条目继续有效。

因此本增量为单节点原型的 H3 级耐久性证据，状态 PARTIAL PASS 候选，不得据此声称 `B-TASK` 包完成或真实断电/多平台耐久性已证明。
