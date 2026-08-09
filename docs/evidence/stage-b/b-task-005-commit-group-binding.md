# B-TASK-005：WriteSet / CommitPermit / TaskCommitReceipt 组绑定证据

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`[TASK-GROUP-002]`（TaskWriteSet/CommitPermit/TaskCommitReceipt membership generation/root/policy 绑定子集）、`[TASK-COMMIT-002]`（finalize membership 复验子集）、`[TASK-EFFECT-001]`（真实 dispatch 前 membership 漂移围栏子集）
>
> 实现：`crates/nlos-task` schema v5

## 1. 本切片目标

补齐 B-TASK-004 明确保留的组绑定缺口：一个 grouped Attempt 的 staged `write_set_root` 获得 CommitPermit 时，authority 必须把当时的 TaskGroup `group_id + membership_generation + membership_root + group_policy_digest` 固化进 permit；真实 effect dispatch 和 terminal receipt 前再次逐位验证，最终 TaskCommitReceipt-shaped record 原样携带同一绑定。membership 漂移不得被旧 permit 或 receipt 静默掩盖。

本切片不实现 Artifact publication、完整 TaskWriteSet 对象、TaskSnapshotReceipt、ParticipantRegistry、签名或跨 authority term adoption。

## 2. 已实现事实

1. **schema v4 → v5 纯增量迁移**：`commit_permits` 与 `task_receipts` 各增加四个 nullable 绑定列；旧 v1–v4 行全部解释为显式 ungrouped `None`，不会推断或伪造 membership。
2. **permit 签发时权威捕获**：grouped Attempt 必须仍是 active member；authority 从当前 TaskGroup 行读取 generation/root/policy，与 `write_set_root` 在同一个 permit 事务中持久化。ungrouped B-TASK-001 路径保持 `None`。
3. **副作用前漂移围栏**：`request_effect_permit` 与 `consume_dispatch_token` 在新 EffectPermit/真实 dispatch 前重新计算当前 binding；membership 或 policy 漂移返回 typed `MembershipConflict`，不签发、不消费 token。
4. **terminalization 漂移围栏**：legacy finalize、v3 finalize 和 no-effect permit closure 均在写 TaskCommitReceipt/TaskPermitClosureReceipt 前复验同一 binding；漂移时 TaskHead、permit 和 attempt 状态保持不变。
5. **Receipt 逐位继承**：成功提交/闭合的 task receipt 直接复制 permit binding；数据库重启后 permit 与 receipt 回读完全相同。
6. **迁移兼容**：结构等价 v4 的既有 ungrouped permit 升级 v5 后仍可完成提交，receipt 继续携带 `None`，`user_version=5`。

## 3. 验收测试

`crates/nlos-task/tests/task_group.rs` 新增：

- `permit_and_commit_receipt_bind_group_membership_across_restart`
- `membership_drift_after_permit_fails_terminalization_closed`
- `schema_v4_upgrades_to_v5_without_inventing_group_bindings`

验证命令：

```text
cargo fmt --all -- --check
cargo test -p nlos-task
cargo clippy -p nlos-task --all-targets -- -D warnings
```

本地 macOS/arm64：90 项 integration tests 全部通过；Clippy 与 rustfmt 通过。

## 4. 证据等级与限制

证据等级：单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- AttemptContract/TaskSnapshotReceipt 已与最终 sealed membership root 完整闭环；当前 Attempt admission binding 与 permit-time binding 是两个可追溯位置，完整 contract rebase 尚未实现。
- 已实现 Artifact/Semantic publication receipt、nested Receipt、read-set/phantom/write-skew validation、Capability/Budget/ParticipantRegistry 或签名。
- 已实现跨 authority term 的 membership/permit adoption。
- 已实现 membership 漂移后 live permit 的自动 rebase/closure；当前行为是保持 `ISSUED` 并 fail-closed，需后续显式 sealed-membership rebase 或管理性收口协议恢复活性。
- 已验证 schema v5 的 kill-9、ENOSPC、torn-write 或三平台 CI；本轮只覆盖正常迁移、漂移拒绝和重启回读。
- 旧 generation/root 的已提交 child result 已在 TaskGroup aggregate 层自动排除；该 aggregate 过滤仍是下一组绑定切片的一部分。

## 5. 下一步

1. `B-ARTIFACT-002` 已完成 Artifact staged revision 与 Artifact 域内 publication receipt；下一步由 TaskAuthority durable prepare/finalize 消费 nested receipt，避免先推进 Artifact canonical head 后 Task finalize 失败被误报为完整提交。
2. 建立完整 TaskWriteSet/TaskSnapshotReceipt/read-set validation，并把 sealed membership rebase 变为显式 Receipt。
3. 为 schema v5 表组补齐 fault-injection 与三平台 CI Evidence。
4. 进入 Slice K 最小端到端骨架：Package → Application → Task → Fiber → Operation → Artifact/Receipt → CLI。
