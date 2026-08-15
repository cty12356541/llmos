# B-TASK-008C2G-COORD：Semantic publication cross-authority coordinator

状态：`PARTIAL_PASS`（2026-08-16）

## 1. 结论

本切片把 ADR-0006 选择 1 的本地两 authority 前缀接成可重启收敛的 coordinator：它只驱动已由 `TaskAuthority` 持久化的 Semantic plan，不拥有新的事实源；每次跨 authority 调用都从上一次 durable prefix 继续。schema v26 又把混合 Effect finalize 所需的 typed proof envelope 持久化，使 coordinator 能在重启后重建 v3 request；同时，`EffectClosedSuccess` 已收紧为绑定 slot contract 与权威闭合 Receipt 的本地 proof。

## 2. 已实现事实

- `SemanticCommitCoordinator` 复用 `TaskAuthority` 的 `PLANNED → PUBLISHING → READY → FINALIZED` 状态机：先授权，再按 sealed `TaskWriteSet` 声明调用 `SemanticAuthority::publish_semantic_publication`，随后消费 owner receipt，最后由 TaskAuthority 原子写入 nested Task receipt。
- `prepare_semantic_finalize` 在 schema v26 写入 immutable mixed-finalize envelope 与 typed required-satisfaction child rows；重复请求逐字节 replay，缺失/重复/非 required slot proof fail closed。
- `TaskAuthority::inspect_semantic_commit_expectations` 只读 sealed Semantic append declarations；`list_incomplete_semantic_commit_plans` 提供稳定的 bounded restart scan，不允许 coordinator 注入新的 event/target/receipt binding。
- owner publication 重试使用 SemanticAuthority 的 exact replay；Task-side receipt consumption 继续逐字段校验 owner readback、write-set root、target、Admission/Durability receipt 和 plan identity。
- `converge_pending` 在重启后扫描未完成计划；检测到 v26 envelope 时调用 persisted v3 finalize，已 `FINALIZED` 的计划不会再次进入 pending scan，但显式 replay 返回原 nested receipt。
- `expected_success_assertion_digest` 以 slot identity、`success_criteria_digest`、EffectReceipt identity/proof 和闭合 kind 计算 domain-separated digest；v3 finalize 逐位校验 Receipt 与 slot 绑定，错误或跨 slot 复制的摘要 fail closed。

## 3. Evidence

- `cargo test -p nlos-commit-coordinator --test semantic_convergence -- --nocapture`：4 项通过；覆盖授权→发布→READY→重启后 finalize、显式 replay、pending scan、负时间戳在任何 mutation 前拒绝、TaskAuthority 写失败后 owner publication exact replay 与 durable prefix 收敛，以及 mixed v3 envelope 重启恢复。
- `cargo test -p nlos-commit-coordinator --quiet`：coordinator 既有 Artifact recovery 测试与 Semantic coordinator 测试全绿。
- `cargo test -p nlos-task --test effect_history -- --nocapture`、`cargo test -p nlos-task --test effect_reconcile -- --nocapture`、`cargo test -p nlos-task --test reconcile_fault_injection -- --nocapture`：29 项通过；覆盖错误 success assertion、闭合 Receipt/slot 绑定、reconcile/replay 与 v3 VFS 故障前缀。
- `cargo clippy -p nlos-task -p nlos-commit-coordinator --all-targets -- -D warnings`：通过。
- `cargo clippy -p nlos-commit-coordinator --all-targets --all-features -- -D warnings`：通过。

## 4. 明确限制

- 只覆盖单机 SemanticAuthority/TaskAuthority、Semantic-only coordinator；新增的故障证据是 TaskAuthority SQLite abort/VFS 写失败，不等于完整 kill-9/ENOSPC/torn-write 组合矩阵，也没有跨进程认证、租约、term takeover 或多 Cell 传播证据。
- mixed v3 envelope 必须在 publication 前由 permit holder 准备；当前 proof 只在 TaskAuthority 内绑定本地 slot contract 与已持久化 EffectReceipt，仍不验证外部 provider 的语义成功内容、签名、attestation 或跨进程 authority lease。
- 不把 outbox ACK、local log-prefix digest 或 coordinator observation 晋升为 Trust View/vector checkpoint，也不声称分布式原子提交。
