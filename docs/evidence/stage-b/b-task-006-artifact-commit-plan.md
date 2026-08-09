# B-TASK-006A：Artifact publication plan 与 CommitPermit 持久绑定

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`[TASK-WRITE-001]`、`[TASK-COMMIT-001]` write-set binding 子集、`[TASK-COMMIT-002]` Artifact publication receipts 前置、`[TASK-TXN-001]` durable prepare 前置
>
> 实现：`crates/nlos-task` schema v6

## 1. 本切片目标

为 B-ARTIFACT-002 之后的跨 authority 可恢复提交建立 TaskAuthority 侧第一块持久基线：在任何 Artifact canonical publication 之前，把 issued CommitPermit 与预期 staged Artifact 集合固化为不可变、可重启查询、可幂等重放的 publication plan。

本切片刻意只产生 `PLANNED` 状态。它不授权 Artifact publish、不消费 publication receipt、不关闭 permit、不推进 TaskHead；因此不能被误用来声明跨 authority commit 已完成。

## 2. 已实现事实

1. **schema v5 → v6 纯增量迁移**：新增 `task_artifact_commit_plans` 与 `task_artifact_publication_expectations`；旧 Task/Attempt/Permit/Receipt 逐位保留。
2. **canonical plan root**：`artifact_publication_plan_root` 对 expectation 按 `(ArtifactId, target_revision, staging_id)` 排序后，以固定 domain、count、ordinal、staging/artifact/revision/digest/size 计算 SHA-256；调用方输入顺序不影响 root。
3. **消除歧义**：空集合、revision=0、重复 staging identity、重复 `(ArtifactId, target_revision)` fail-closed。
4. **permit 逐位绑定**：planning 在一个 `BEGIN IMMEDIATE` 中复验 TaskHead/history/fence、permit holder/generation/state 和当前 group binding；canonical plan root 必须精确等于 permit `write_set_root`。当前明确限定为 artifact-only TaskWriteSet。
5. **无提前可见性**：planning 只写 plan/expectation 表，不写 Task receipt、不关闭 permit、不推进 TaskHead，也不接触 ArtifactAuthority。
6. **幂等与不可变**：plan ID 由 PermitId 确定性派生；同 key/同 canonical 集合跨重启返回原 record，改写集合返回 `IdempotencyConflict`。DDL 阻止 identity/expectation UPDATE 与全部 DELETE。
7. **未来状态不冒充事实**：schema 预留 `PUBLISHING/READY/FINALIZED` code，但 v6 API 只可产生 `PLANNED`；后续必须通过独立校验事务晋级。

## 3. 验收测试

新增 `crates/nlos-task/tests/artifact_commit_plan.rs` 六项：

- root 与输入顺序无关，空/重复集合拒绝；
- issued permit 成功绑定且 TaskHead/permit 状态不变；
- 重启后 exact replay 与 inspect 一致，异集合 key reuse 拒绝；
- plan root 与 permit write-set root 不同 fail-closed；
- plan identity/expectation DDL 不可变；
- 结构等价 v5 数据库无损迁移到 v6。

验证命令：

```text
cargo fmt --all -- --check
cargo test -p nlos-task
cargo clippy -p nlos-task --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

最终结果以本 canonical commit 的验证记录为准。

## 4. 证据等级与限制

单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- `PLANNED` 已授权发布；ArtifactAuthority 不能把 plan row 当成 publication permit。
- 已验证 effect slots/required satisfaction/finalize proof 并把 plan 晋级 `READY`。
- 已消费或验证 B-ARTIFACT-002 publication receipt，已形成 nested TaskCommitReceipt。
- 已冻结 prepared grouped commit 的 membership、完成跨库 prepare/finalize、partial/uncertain 查询或重启收敛。
- 已覆盖 schema v6 VFS 故障注入、三平台 CI、完整 TaskWriteSet/read-set/phantom/write-skew。

## 5. 下一步

下一原子切片在此不可变 plan 上增加：finalize-readiness 证明与 `READY` 晋级、group membership 漂移冻结、Artifact publication receipt 逐项消费、partial/uncertain 可查询状态，以及只有 receipt 集完整时才允许 Task finalize 并把 nested receipts 绑定进 TaskCommitReceipt。
