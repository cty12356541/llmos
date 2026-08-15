# ADR-0006：SemanticAuthority 拥有 Semantic publication receipt

- 状态：ACCEPTED
- 日期：2026-08-16
- Owner：SemanticAuthority / TaskAuthority
- 关联 Requirement：`TASK-WRITE-003`、`TASK-COMMIT-002`、`SEM-ADMIT-001`、`SEM-DURABLE-001`
- 关联工作包：`B-TASK-008C2G`
- 决策来源：用户在 2026-08-16 明确选择候选 1
- 复审触发器：Semantic publication 跨 Cell、Trust View/vector checkpoint 正式落地，或 publication 需要独立 broker/租约

## 上下文

此前 SemanticAuthority 已拥有 canonical SemanticEvent、AdmissionReceipt、可选 DurabilityReceipt 以及 admission outbox transport observation；TaskAuthority 只在 finalize 前重读这些 owner facts。outbox ACK 明确不是 publication/checkpoint proof，但 v0.5 的 `TaskCommitReceipt.semantic_publications` 仍需要一个 canonical receipt producer。

如果由 TaskAuthority 或 coordinator 生成 Semantic publication receipt，会把 Semantic 域事实复制到第二个权威；如果直接把 outbox ACK 当作 publication，则违反 `[TASK-WRITE-003]` 与 `[SEM-TXN-002]` 的 durable fact 边界。

## 候选

| 候选 | 优点 | 主要代价 |
|---|---|---|
| **SemanticAuthority 生成 receipt/checkpoint** | publication 事实与 Event/Admission/Durability 同一 owner；Task 只消费并嵌套证明 | 需要跨 authority 的 durable prepare/consume/recovery；当前 checkpoint 先限定为单 authority 本地前缀 |
| TaskAuthority/coordinator 生成消费 receipt | Task 侧接线较快 | 会创建第二个 Semantic publication 事实源，且无法自行证明 Semantic log/checkpoint |

## 决定

采用 **SemanticAuthority 生成 canonical SemanticPublicationReceipt**，TaskAuthority 只负责：

1. 在 publication request 中绑定 `TaskId`、`CommitPermitId`、sealed `write_set_root` 和 Semantic event；
2. 通过 SemanticAuthority owner readback 验证 target、AdmissionReceipt、可选 DurabilityReceipt；
3. 在自身 durable Task commit receipt 中嵌套已消费的 immutable publication receipt；
4. 在跨 authority 事务无法原子完成时，保持 `FINALIZING/PARTIAL/PENDING_PROJECTION`，不得把 intent 或 outbox ACK 当成 `COMMITTED`。

当前阶段的 `semantic_checkpoint_after` 是 SemanticAuthority 内部 append-only log prefix 的确定性 digest。它不是跨 Cell 的全局标量或 vector checkpoint，不能外推为分布式 Trust View 保证。

## 后果与退出策略

- SemanticAuthority schema 增加 immutable `semantic_publication_receipts`；receipt identity 绑定 Task/Permit/WriteSet/Event/Admission/Durability 和 owner-derived local checkpoint。
- publication receipt 不修改 Event、AdmissionReceipt、DurabilityReceipt 或 outbox ACK；重复请求逐字节回放，绑定漂移 fail closed。
- TaskAuthority 后续增加 publication plan/consumer 与 nested `TaskCommitReceipt.semantic_publications`，并保留跨 authority crash/restart 的 PARTIAL/UNCERTAIN 语义。
- 当 TrustPolicy、vector checkpoint、跨 Cell assignment 或独立 Semantic broker 具备完整证据时，新增 ADR/迁移不得重写历史 receipt；可通过 successor receipt/version 扩展。

## 当前证据与缺口

`B-SEMANTIC-005` 已验证 schema v4、owner target/Admission/Durability readback、local log-prefix checkpoint、immutable receipt、exact replay、restart readback 和错误绑定拒绝。TaskAuthority consumer、nested TaskCommitReceipt、跨 authority prepare/consume/recovery、Trust View/vector checkpoint、跨进程认证与多 Cell 仍未完成。
