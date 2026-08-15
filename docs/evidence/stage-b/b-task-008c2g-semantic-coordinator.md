# B-TASK-008C2G-COORD：Semantic publication cross-authority coordinator

状态：`PARTIAL_PASS`（2026-08-16）

## 1. 结论

本切片把 ADR-0006 选择 1 的本地两 authority 前缀接成可重启收敛的 coordinator：它只驱动已由 `TaskAuthority` 持久化的 Semantic plan，不拥有新的事实源；每次跨 authority 调用都从上一次 durable prefix 继续。schema v26 又把混合 Effect finalize 所需的 typed proof envelope 持久化，使 coordinator 能在重启后重建 v3 request。

## 2. 已实现事实

- `SemanticCommitCoordinator` 复用 `TaskAuthority` 的 `PLANNED → PUBLISHING → READY → FINALIZED` 状态机：先授权，再按 sealed `TaskWriteSet` 声明调用 `SemanticAuthority::publish_semantic_publication`，随后消费 owner receipt，最后由 TaskAuthority 原子写入 nested Task receipt。
- `prepare_semantic_finalize` 在 schema v26 写入 immutable mixed-finalize envelope 与 typed required-satisfaction child rows；重复请求逐字节 replay，缺失/重复/非 required slot proof fail closed。
- `TaskAuthority::inspect_semantic_commit_expectations` 只读 sealed Semantic append declarations；`list_incomplete_semantic_commit_plans` 提供稳定的 bounded restart scan，不允许 coordinator 注入新的 event/target/receipt binding。
- owner publication 重试使用 SemanticAuthority 的 exact replay；Task-side receipt consumption 继续逐字段校验 owner readback、write-set root、target、Admission/Durability receipt 和 plan identity。
- `converge_pending` 在重启后扫描未完成计划；检测到 v26 envelope 时调用 persisted v3 finalize，已 `FINALIZED` 的计划不会再次进入 pending scan，但显式 replay 返回原 nested receipt。

## 3. Evidence

- `cargo test -p nlos-commit-coordinator --test semantic_convergence -- --nocapture`：4 项通过；覆盖授权→发布→READY→重启后 finalize、显式 replay、pending scan、负时间戳在任何 mutation 前拒绝、TaskAuthority 写失败后 owner publication exact replay 与 durable prefix 收敛，以及 mixed v3 envelope 重启恢复。
- `cargo test -p nlos-commit-coordinator --quiet`：coordinator 既有 Artifact recovery 测试与 Semantic coordinator 测试全绿。
- `cargo clippy -p nlos-commit-coordinator --all-targets --all-features -- -D warnings`：通过。

## 4. 明确限制

- 只覆盖单机 SemanticAuthority/TaskAuthority、Semantic-only coordinator；新增的故障证据是 TaskAuthority SQLite abort/VFS 写失败，不等于完整 kill-9/ENOSPC/torn-write 组合矩阵，也没有跨进程认证、租约、term takeover 或多 Cell 传播证据。
- mixed v3 envelope 必须在 publication 前由 permit holder 准备；required-effect proof 的内容仍沿用当前 v0.5 placeholder digest 语义，coordinator 不把它提升为外部 authority 的真实执行证明。
- 不把 outbox ACK、local log-prefix digest 或 coordinator observation 晋升为 Trust View/vector checkpoint，也不声称分布式原子提交。
