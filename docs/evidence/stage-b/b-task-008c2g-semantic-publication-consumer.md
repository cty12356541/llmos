# B-TASK-008C2G-SEM：TaskAuthority Semantic publication consumer

状态：`PARTIAL_PASS`（2026-08-16）

## 1. 结论

本切片完成了 ADR-0006 选择 1 的 Task 侧边界：`SemanticAuthority` 生成 canonical `SemanticPublicationReceipt`，`TaskAuthority` 只对 owner readback 做逐字段校验，并将不可变副本作为 nested `SemanticTaskCommitReceipt.semantic_publications` 的证据。Semantic outbox ACK 不参与该证明。

## 2. 已实现事实

- `nlos-task` schema v25 新增 immutable Semantic commit plan 与 nested publication receipt 表；计划、事件、Task/Permit/write-set root、target、log sequence、Admission/Durability receipt 和 local checkpoint 均有长度/唯一性/FK/immutable 约束。
- `plan_semantic_commit` 只能从已签发 permit 的 authority-verified `TaskWriteSet.semantic_appends` 建立计划；permit/head/group/participant binding 与 canonical append root 逐位复核。
- `authorize_semantic_publication` 形成 `PLANNED → PUBLISHING` fence；`record_semantic_publications` 通过 `SemanticAuthority::inspect_publication_receipt` 重新读取 owner receipt，支持 partial set、exact replay 和错误绑定拒绝。
- 完整 receipt set 进入 `READY` 后，`finalize_semantic_commit` 在单个 TaskAuthority transaction 中写入原有 `TaskReceiptRecord`、关闭 permit、推进 TaskHead、提交 attempt terminal state、标记 plan `FINALIZED`，返回 `SemanticTaskCommitReceipt` 及 nested publications。
- Task group membership 的 publication-in-flight fence 同时覆盖 Artifact 与 Semantic plan；混合 Effect + Semantic permit 仍拒绝走本切片的 Semantic-only finalize，留给后续统一 coordinator。

## 3. Evidence

- `cargo test -p nlos-task --test semantic_commit -- --nocapture`：1 项端到端测试通过；覆盖 owner receipt 消费、错误 checkpoint 拒绝、READY、nested Task receipt、TaskAuthority 重启 replay 与 immutable receipt update 拒绝。
- `cargo test -p nlos-task --quiet`：既有 nlos-task 集成测试全绿（包含 schema v24→v25 迁移断言）。
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings`：通过。

## 4. 明确限制

- 仅覆盖单节点、Semantic-only permit；混合 Effect + Semantic 的统一 terminal receipt、跨 authority prepare/finalize 原子性和自动 coordinator 尚未完成。
- 没有把 Task nested copy 反向写入 SemanticAuthority，也没有把 outbox ACK、local log-prefix checkpoint 晋升为 Trust View/vector checkpoint。
- 仍缺少跨进程 auth/lease、term takeover、真实 VFS/kill-9/ENOSPC 组合矩阵、multi-Cell 传播和完整 `TaskCommitReceipt` canonical encoding/signature。
