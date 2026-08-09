# B-ARTIFACT-002：Artifact staged revision 与 publication receipt

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`[TASK-WRITE-001]`（permit 前输出只可进入 staging）、`[TASK-COMMIT-002]`（Artifact publication receipt 子集）、`[TASK-TXN-001]`（跨 authority 可恢复提交的 Artifact 侧前置）
>
> 实现：`crates/nlos-artifact` schema v2

## 1. 本切片目标

在 B-ARTIFACT-001 的内容寻址 blob 与 immutable revision 之上，增加一条不会提前改变 canonical head 的发布路径：Attempt 输出可先以 task/permit/write-set binding 持久进入 staging；只有显式 publish 才能在 ArtifactAuthority 内原子写入 revision、CAS head 并生成不可变 publication receipt。

本切片不声称已经完成 `nlos-task` 与 `nlos-artifact` 两个 SQLite authority 之间的原子事务。TaskAuthority 的 durable prepare/finalize、nested Receipt 与完整 TaskCommitReceipt 消费协议是下一集成切片。

## 2. 已实现事实

1. **schema v1 → v2 纯增量迁移**：新增 `artifact_staged_revisions` 与 `artifact_publication_receipts`；旧 artifact/revision 数据保持不变，未知版本继续 fail-closed。
2. **stage 不推进 canonical head**：`stage_revision` 先按 B-ARTIFACT-001 协议持久化 blob，再写 staged metadata；不插入 revision，不修改 artifact head。staging identity 由 `ArtifactId + IdempotencyKey` 确定性派生。
3. **staging 幂等与绑定**：完全相同的 artifact/head/digest/size/task/permit/write-set 请求返回原 staged record；同 key 改 bytes 或 binding 返回 typed `IdempotencyConflict`。调用方时间戳不参与重放等价性， durable record 原样返回。
4. **Artifact 域内原子发布**：`publish_staged_revision` 在一个 `BEGIN IMMEDIATE` 中重验 blob、task/permit/write-set binding 与 expected head，随后写 immutable revision、CAS head、写 publication receipt、把 stage 标为 `Published`。任一步失败均不留下半发布 metadata。
5. **竞争恰好一胜**：同一旧 head 可存在多个 staged 候选；首个 publish 推进 head，败者得到 typed `HeadConflict` 并保持 `Staged`，不得伪装成已发布或覆盖 winner。
6. **publication receipt 不可变且可重放**：receipt ID 由 staging ID 确定性派生；重复 publish 跨重启返回原 receipt。DDL trigger 拒绝直接 UPDATE/DELETE。
7. **恢复不把 staging 当孤儿**：`recover()` 把 staged digest 纳入 artifact 引用集合；缺失 staged blob 单独进入 `missing_staged_blobs`，publish 返回 typed `StagedBlobMissing`，head 保持不变。

## 3. 验收测试

新增 `crates/nlos-artifact/tests/staged_publication.rs`，覆盖：

- stage 持久但 head/revision 不变，recover 不误报 orphan；
- stage 完全重放、bytes/binding 改写冲突；
- publish binding 拒绝、revision/head/receipt/stage-state 同事务结果；
- receipt 重放与 DDL 不可变约束；
- 同 head 双 staged 竞争恰好一胜，败者保持 staged；
- stage 后重启再发布、发布后再重启重放；
- staged blob 丢失恢复可见且阻止发布；
- 结构等价 v1 store 无损迁移到 v2。

本地验证命令：

```text
cargo fmt --all -- --check
cargo test -p nlos-artifact
cargo clippy -p nlos-artifact --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

最终结果以本 canonical commit 的验证记录为准。

## 4. 证据等级与限制

证据等级：单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- 已实现 TaskAuthority 与 ArtifactAuthority 的跨库 atomic commit；当前只保证 Artifact metadata 域内原子发布。直接调用 Artifact publish 后，Task finalize 仍可能失败。
- 已实现 TaskAuthority durable prepare/outbox/saga、nested Receipt、完整 TaskCommitReceipt 或崩溃后的跨 authority 自动收敛。
- 已实现 read-set/phantom/write-skew validation、完整 TaskWriteSet/TaskSnapshotReceipt、sealed membership rebase、签名或 ParticipantRegistry。
- 已完成 schema v2 的 VFS 写点故障矩阵、真实断电、三平台 CI 或生产级并发性能验证。
- 已改变 B-ARTIFACT-001 的 LOCAL_SINGLE_NODE、无 GC/retention/encryption/provenance/legal hold/sync backend 等限制。

## 5. 下一步

在 `nlos-task` 增加可恢复的 commit prepare/finalize 状态：TaskAuthority 先 durable prepare，ArtifactAuthority 仅按该 binding 发布并返回 publication receipt，TaskAuthority 再消费 nested receipt 完成 TaskCommitReceipt；重启后必须能查询 partial/uncertain 并幂等收敛，避免“Artifact head 已推进但 Task receipt 丢失”被误报为完整提交。
