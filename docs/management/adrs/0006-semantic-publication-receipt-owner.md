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
- TaskAuthority 增加 publication plan/consumer 与 nested `TaskCommitReceipt.semantic_publications`；含 Effect slot 的 v3 本地终结 hook 复用同一 TaskAuthority transaction，schema v26 又持久化 typed mixed-finalize envelope，Semantic-only 与 envelope-backed mixed plan 均可由 bounded coordinator 跨 authority 驱动；required `EffectClosedSuccess` 另有 slot/Receipt-bound 本地 proof binding；schema v27 增加单 authority durable lease/term/fencing primitive，schema v28 再为 opt-in CommitPermit、plain v3 finalize、pre-effect close、mixed Effect + Semantic persisted-envelope finalize/replay 和 Semantic-only high-level finalize 增加 immutable lease binding，schema v29 再为同一 live lease 的 adoption/reconcile 安全子集增加 immutable binding，并提供新 term `FROZEN_FOR_TAKEOVER` local fence pre-gate；schema v30 持久化 immutable local fence receipt，并在 durable participant mapping 完整时计算 frozen registry ∪ locally durable outstanding-operation participant 的 exact local roots；schema v31 为 lease-bound permit 建立 immutable local assignment baseline；schema v32 再把旧 assignment 置为 `TakeoverPending` 并持久化不含 successor/远端 barrier 完成声明的 pending `TaskAuthorityTakeoverReceipt` local prefix；schema v33 再持久化逐 endpoint 的 immutable barrier receipt observation，schema v34 再持久化 canonical exact-fence member manifest 并提供只读 local coverage view，但不校验远端签名、不完成 parent takeover。跨 authority crash/restart 仍保持 PARTIAL/UNCERTAIN 语义，租约尚未覆盖 IPC peer auth、远端 barrier 验证/完成、successor assignment 激活、跨 term adoption 或完整故障矩阵。
- schema v35 为 takeover barrier observation 持久化 endpoint-supplied `barrier_receipt_digest`；v33/v34 旧行保留 `NULL`，不把无法重建的 digest 伪造成审计事实。该修复仍不校验远端签名、不完成 parent takeover。
- 2026-08-18 后续切片 `B-TASK-008C2G-SUCCESSOR-REGISTRY` 已在 schema v37 之上补齐
  takeover 完成后的本地 successor registry generation/assignment rotation 与新
  lease-bound permit 接线；它只复制冻结 participant tuple，不改变本 ADR 对远端
  attestation、跨 authority adoption 和 Trust View 的边界。详见
  [Evidence](../../evidence/stage-b/b-task-008c2g-successor-registry.md)。
- 当 TrustPolicy、vector checkpoint、跨 Cell assignment 或独立 Semantic broker 具备完整证据时，新增 ADR/迁移不得重写历史 receipt；可通过 successor receipt/version 扩展。

## 当前证据与缺口

`B-SEMANTIC-005` 已验证 schema v4、owner target/Admission/Durability readback、local log-prefix checkpoint、immutable receipt、exact replay、restart readback 和错误绑定拒绝；`B-TASK-008C2G-SEM` 已验证 schema v25、TaskAuthority consumer、nested TaskCommitReceipt、本地混合 Effect + Semantic v3 终结/重放与 slot/Receipt-bound success proof；`B-TASK-008C2G-COORD` 已验证 schema v26 typed mixed-finalize envelope、Semantic-only 与 envelope-backed mixed plan 的授权、owner publication、Task receipt consumption、重启继续、Task 写失败后的 owner replay 与 durable prefix 收敛，并以 schema v27/v28/v29/v30/v31/v32/v33/v34、`authority_lease`、`semantic_commit` 和 `effect_reconcile` 测试验证单 authority 续租、过期接管、旧 token 拒绝、lease-bound permit 签发/终结、local assignment baseline、pending takeover receipt、逐 endpoint barrier observation、exact fence member manifest、mixed publication finalize/replay、Semantic-only high-level finalize、same-term adoption/reconcile binding、new-term local takeover-fence CAS、immutable fence receipt、exact local fence-set root 和重启回读；`B-TASK-008C2G-SUCCESSOR-REGISTRY` 再验证 schema v37 完成后 successor registry generation/assignment rotation、新 permit 接线和重启 replay；`B-SCHEMA-005` 增量再验证 exact OS peer credential pre-gate。跨 authority 完整 prepare/consume/recovery、NLOS principal/签名 peer attestation、远端 barrier 验证/完成、旧 permit cross-term adoption、外部 provider proof/attestation、Trust View/vector checkpoint 与多 Cell 仍未完成。
