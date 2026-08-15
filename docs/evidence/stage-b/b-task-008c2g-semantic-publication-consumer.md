# B-TASK-008C2G-SEM：TaskAuthority Semantic publication consumer 与混合终结

状态：`PARTIAL_PASS`（2026-08-16）

## 1. 结论

本切片完成了 ADR-0006 选择 1 的 Task 侧边界：`SemanticAuthority` 生成 canonical `SemanticPublicationReceipt`，`TaskAuthority` 只对 owner readback 做逐字段校验，并将不可变副本作为 nested `SemanticTaskCommitReceipt.semantic_publications` 的证据。Semantic outbox ACK 不参与该证明；随后将同一份 nested receipt 接入含 Effect slot 的 v3 统一终结路径。

## 2. 已实现事实

- `nlos-task` schema v25 新增 immutable Semantic commit plan 与 nested publication receipt 表；计划、事件、Task/Permit/write-set root、target、log sequence、Admission/Durability receipt 和 local checkpoint 均有长度/唯一性/FK/immutable 约束。
- `plan_semantic_commit` 只能从已签发 permit 的 authority-verified `TaskWriteSet.semantic_appends` 建立计划；permit/head/group/participant binding 与 canonical append root 逐位复核。
- `authorize_semantic_publication` 形成 `PLANNED → PUBLISHING` fence；`record_semantic_publications` 通过 `SemanticAuthority::inspect_publication_receipt` 重新读取 owner receipt，支持 partial set、exact replay 和错误绑定拒绝。
- 完整 receipt set 进入 `READY` 后，`finalize_semantic_commit` 在单个 TaskAuthority transaction 中写入原有 `TaskReceiptRecord`、关闭 permit、推进 TaskHead、提交 attempt terminal state、标记 plan `FINALIZED`，返回 `SemanticTaskCommitReceipt` 及 nested publications。
- 新增 `finalize_commit_v3_with_semantic_publications`：在同一 TaskAuthority transaction 内复用现有 v3 Effect slot evaluation/history append，再 CAS 标记 Semantic plan `FINALIZED`，使 Effect + Semantic terminal receipt 一起提交；普通 Semantic-only finalize 明确拒绝含 Effect slot 的 permit。
- Task group membership 的 publication-in-flight fence 同时覆盖 Artifact 与 Semantic plan；混合路径在 effect 尚未闭合时保持 `OutstandingEffectSlots`，事务失败后 plan 仍为 `READY`，不留下半个 terminal receipt。

## 3. Evidence

- `cargo test -p nlos-task --test semantic_commit -- --nocapture`：2 项端到端测试通过；除 owner receipt 消费、错误 checkpoint 拒绝、READY、nested Task receipt、TaskAuthority 重启 replay 与 immutable receipt update 拒绝外，新增混合 Effect + Semantic v3 终结、Semantic-only API 拒绝 Effect、未闭合 Effect 的原子失败与统一路径 replay。
- `cargo test -p nlos-task --quiet`：既有 nlos-task 集成测试全绿（包含 schema v24→v25 迁移断言）。
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings`：通过。

## 4. 明确限制

- 混合 Effect + Semantic 目前覆盖单节点 TaskAuthority 内的统一 v3 transaction；跨 authority prepare/finalize 原子性、自动 coordinator、owner publication 的 crash recovery 尚未完成。
- 没有把 Task nested copy 反向写入 SemanticAuthority，也没有把 outbox ACK、local log-prefix checkpoint 晋升为 Trust View/vector checkpoint。
- 仍缺少跨进程 auth/lease、term takeover、真实 VFS/kill-9/ENOSPC 组合矩阵、multi-Cell 传播和完整 `TaskCommitReceipt` canonical encoding/signature。
