# ROAD-B-003 任务层补齐证据：跨 Task handle 泄漏与 snapshot 漂移

> 状态：PASS（task 层测试面补齐；限 nlos-task crate，不含 slice-k 纵切与 provider 面）
>
> 日期：2026-08-31（基线 HEAD `afd05ae`，CI 绿；W5-D 双 Attempt 竞争证据见 `b-slice-k-001-end-to-end.md` §7，commit `3500c9c`）
>
> 对应：`[ROAD-B-003]`（`06-架构设计总纲-v0.5.md` §28.2）中「跨 Task handle 泄漏」「snapshot 漂移」两项的 task 层测试；写集仅 `crates/nlos-task/tests/road_b003_gaps.rs`（新增，4 测试）与本证据文件。

## 1. 勘察结论（防重复）

对 `crates/nlos-task/tests/` 全量勘察（2026-08-31，基线 `afd05ae`）后确认以下场景**已覆盖**，本次不重复：

| 既有覆盖 | 位置 | 断言 |
|---|---|---|
| 同 Task 跨 Attempt 持有者 fencing | `effect_permit.rs::only_commit_permit_holder_obtains_effect_permit`、`effect_fiber_registration.rs::effect_binding_gates_fail_closed_without_side_effects` | 败者持 winner 的 permit/epoch 请求 EffectPermit 与 fiber 注册 → `NotPermitHolder`/`PermitEpochMismatch`/`InvalidEffectSlotState`，零残留 |
| 同 Task 跨 Attempt finalize fence | `task_authority.rs::losing_or_stale_attempt_cannot_finalize_or_overwrite_winner_receipt` | 败者 finalize 用 winner 的 permit → `NotPermitHolder`；伪造 permit → `PermitNotFound`；winner receipt 不可覆盖 |
| stale commit_seq → Conflicted | `task_authority.rs::stale_snapshot_detected_after_head_advance`、`invalid_transitions_fail_closed` | head 前进后旧 snapshot 请求 → `Conflicted{StaleTaskHead{expected,current}}`；超前冻结 → `Conflicted` + `AttemptState::Conflicted` |
| stale retry-fence → Conflicted | `effect_history.rs::partial_effect_advances_head_root_and_fence_and_new_attempts_inherit` | `PermitConflict::StaleRetryFenceEpoch` |
| snapshot receipt 过期 | `snapshot_receipt.rs::stale_incomplete_or_conflicting_receipts_fail_closed` | receipt 与 snapshot 绑定不一致 fail-closed |
| Superseded 终态不可复活（slice-k 层） | `crates/nlos-slice-k/tests/competing_attempts.rs`（commit `3500c9c`） | 新 key 重试 → `InvalidAttemptState{Superseded}` |

**确认的缺口**（本次补齐）：

1. 无任何测试在**同一 authority 内注册两个 Task** 并互换 permit/attempt 身份——既有 handle 误用全部是单 Task 内跨 Attempt；
2. `effect_history_root`-only 漂移（commit_seq 匹配、root 过期/伪造）从未被单独断言；
3. `Conflicted` 败者的**终态 fence 与新 key 不可复活**在 nlos-task 层无断言（仅 slice-k 有 Superseded 变体）。

## 2. 新增场景矩阵与实测语义

新文件：`crates/nlos-task/tests/road_b003_gaps.rs`（fixture 复制最新惯例 `effect_fiber_registration.rs`：`TestDatabase` 临时库、nominal ID 种子、`request_commit_permit_with_authorities_struct(Authorities::default(), …)` 非弃用入口、决策解包 helper）。全部语义按 TaskAuthority 实测断言（W5-D 先例），零发明。

| 场景 | 测试 | 实测语义（typed 结果） | 零双重提交/head 单调断言 |
|---|---|---|---|
| 跨 Task 方向一：T1 的 CommitPermit 在 T2 的全部 task-scoped 路径使用 | `foreign_task_permit_identity_is_rejected_in_all_task_scoped_paths` | `request_effect_permit` / `register_effect_binding` / `finalize_commit` / `close_permit` 全部 → `Err(PermitNotFound)`（permit 按 `(task_id, permit_id)` 定位，task 绑定校验生效）；T2 `permit_epoch=0`、`active_permit=None`、head=0 不变；T1 permit 仍 `Issued` | T1 正常 finalize：head 0→1 恰一步；finalize 重放同 receipt（byte-equal）；正向对照：T2 随后独立取得自己的 permit（隔离不阻碍合法进展） |
| 跨 Task 方向二（正交）：T2 的 attempt 身份劫持 T1 的 permit 上下文 | `foreign_attempt_identity_cannot_hijack_holder_permit_context` | 同 task、同 permit、同 epoch、真 dispatch token，仅换 attempt 身份 → 五条路径（effect permit、fiber 注册、dispatch 消费、finalize、close）全部 → `Err(AttemptNotFound)`（attempt 按 `(task_id, attempt_id)` 定位；holder check 先于任何 slot/副作用） | holder 的 fiber 注册恰 1 条且 slot `Permitted` 不动；holder 事后 no-effect 闭环 + finalize：head 0→1 恰一步、重放同 receipt |
| snapshot 漂移（root-only）：snapshot 的 `effect_history_root` 指向本 Task 从未有过的 root | `stale_effect_history_root_conflicts_fences_and_cannot_revive` | 注册时不校验（漂移在 permit CAS 才显现）→ `PermitDecision::Conflicted{StaleEffectHistoryRoot}`；attempt 落 `AttemptState::Conflicted`（`is_terminal()`）；新 key 重试 → `Err(InvalidAttemptState{Conflicted})` | 期间 head=0、无 outstanding permit、`permit_epoch=0`；正确绑定的新 attempt 恰好提交一次：head 0→1、重放同 receipt |
| snapshot 漂移（head 前进后）：旧 head 绑定的 attempt 在他人提交后请求 | `stale_snapshot_after_head_advance_conflicts_fences_and_cannot_revive` | → `Conflicted{StaleTaskHead{expected:0, current:1}}`；终态 `Conflicted`；新 key 重试 → `InvalidAttemptState{Conflicted}`；绑定同一旧 snapshot 的**新** attempt 同样 `Conflicted`（漂移属于 snapshot 而非 attempt） | 全程恰两次提交：head 0→1→2 单调，两 receipt 不同，`active_permit=None` 收尾；stale 流量贡献为零 |

语义注记（实测发现，非缺陷）：Task 注册时 `control_epoch` 初始为 1（`store.rs` `register_task`），跨 Task 误用后保持 1，测试按该事实断言。

## 3. 验证门（全部实跑）

环境：macOS arm64；stable 与 `nightly-2026-08-01` 双工具链。

```sh
cargo test -p nlos-task                                 # 253 passed; 0 failed（33 个测试目标全量：31 集成二进制 + lib unittests + doc-tests，含 road_b003_gaps 4 测试）
cargo clippy -p nlos-task --all-targets -- -D warnings  # 通过（exit 0）
cargo +nightly-2026-08-01 clippy -p nlos-task --all-targets -- -D warnings  # 通过（exit 0）
cargo fmt -p nlos-task -- --check                       # 通过（exit 0）
cargo +nightly-2026-08-01 fmt -p nlos-task -- --check   # 通过（exit 0）
```

说明：`cargo test -p nlos-task` 全量因 33 个测试目标的冷编译分三段跑完（169 + 84 passed, 0 failed）；接管复验为单轮跑完同值 253 passed / 0 failed。全部测试体本身运行时间为秒级。clippy 首轮报 `too_many_lines`（两个长测试）与 `items_after_statements`（测试内嵌 fn），按仓库先例（`cross_term_adoption.rs`）以 `#[allow(clippy::too_many_lines)]` 与提升内嵌 fn 为模块级 helper 修复后全绿（stable 与 nightly-2026-08-01 双工具链复验）。

## 4. 显式不适用与已知限制（非声明）

- **ROAD-B-003 其余两项「共享 provider cache 降级」「投机副作用 fence」：本证据显式登记为不适用**——provider 面尚未落地，无被测对象；待 provider 切片实现后在 provider 层补测。
- 本证据为 **task 层（`nlos-task` crate 单 authority 单进程 SQLite）** 证据：不声称跨进程/跨节点并发、真实 kill-9/torn-write 故障注入（fault-injection VFS 系列另行覆盖）、或 slice-k 端到端纵切完成。
- 跨 Task 泄漏的 typed 拒绝依赖 `task_id`-scoped 查找与 holder check；ID 为确定性派生占位（同 B-TASK-001 §3.3），无签名 provenance。
- 「Conflicted 不可复活」断言的是本切片 `AttemptState` 状态机（`is_terminal()`）；§25.1 完整状态机（Admitted/Running/…）仍为 reserved，不在此证据范围。
- 未运行项：`--workspace` 级测试/Clippy（任务书禁用）；三平台 CI 复验（本次为本地验证，未 push）。
