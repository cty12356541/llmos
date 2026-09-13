# 统一恢复面设计(Semantic 恢复台账 + worker 双域驱动 + 统一 TaskCommitReceipt)

- **日期**:2026-09-13
- **状态**:已获维护者批准的方向(chat 批准,本文为定稿 spec)
- **波次**:W26
- **背景依据**:[ADR-0013 跨 authority verify-then-commit 契约](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md)、[ADR-0005 authority-first 顺序](../../management/adrs/0005-task-write-set-authority-first.md)、`b-task-006h`–`006k` 恢复台账/worker 证据、`b-task-008c2g-coord` 系列收敛证据

## 1. 目标

把跨 authority 提交契约([ADR-0013](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md))的**有界收敛半边**从"仅 Artifact 自动、Semantic 靠调用方"补齐为**两域均自动**:Semantic 获得与 Artifact 同构的 durable 恢复台账与常驻 worker 驱动,重启后无人调用也能从 durable prefix 收敛到唯一终态;并以**读侧聚合类型**统一 TaskCommitReceipt 的表达。

**非目标**:不给 Resource/Operation/Process 建 prepare/finalize 入口(W27+ 候选);不做跨机/跨 Cell 语义(ADR-0013 已登记为 Stage C 扩展点);不改 Artifact 既有台账 schema 与已验证路径;不做 compensation 执行与跨进程 attestation。

## 2. 现状缺口(设计动机)

- `TaskAuthorityCommitRecoveryWorker` 的 `durable_cycle` 只扫 `list_due_artifact_commit_plans`;Semantic 重启后依赖调用方显式调 `list_incomplete_semantic_commit_plans` + `SemanticCommitCoordinator::converge_pending`(semantic_pending_restart_scan 测试即此形态)——**无人调用则不收敛**,契约的自动性只兑现一半。
- Semantic 无失败台账:无 CAS 重试计数、无 backoff、无 escalation、无告警 acknowledge 面。
- TaskCommitReceipt 无统一类型:plain `TaskReceiptRecord` 与 `ArtifactTaskCommitReceipt`/`SemanticTaskCommitReceipt`/`ResourceTaskCommitReceipt`/`SemanticResourceTaskCommitReceipt` 并立,读侧无单一收口表达。

## 3. 语义契约(规范语句)

### 3.1 Semantic 恢复台账(schema v42)

- `[SEM-RECOV-001]` 新表组 `semantic_commit_recovery_*` 镜像 Artifact 台账语义:plan 身份、`total_failures`(CAS 更新)、`next_retry_at_ms`(指数退避、封顶)、`Escalated` 终态(需显式 `resume` 才重回 `Retrying`)、告警行与 `acknowledge`。
- `[SEM-RECOV-002]` `record_semantic_recovery_failure` 以 `total_failures` 做 CAS;并发双写者至多一次自增,无双记。
- `[SEM-RECOV-003]` `list_due_semantic_commit_plans` 只返回非 Finalized 且 `Retrying` 状态、`next_retry_at_ms` 已到期的 plan;`Escalated` 不进扫描。
- `[SEM-RECOV-004]` Semantic finalize 成功路径按与 Artifact 的 `resolve_recovery` 同款语义把台账置 `Resolved`;幂等重放不二次记录失败。
- `[SEM-RECOV-005]` 台账写入自身失败视为 infrastructure 失败,进 worker 退避;plan 本身仍是 durable 事实,台账只是恢复调度状态——台账行丢失不产生幻影收敛(重扫 durable plan 即可重建调度)。
- `[SEM-RECOV-006]` 迁移幂等、历史行零改写(既有 schema 规范)。
- `[SEM-RECOV-007]` 告警面:`list_semantic_recovery_alerts` / `acknowledge_semantic_recovery_alert` / `summarize_semantic_recovery` 与 Artifact 告警语义逐位同构。

### 3.2 Worker 双域驱动

- `[UNIFIED-WORKER-001]` `durable_cycle` 每轮顺序执行:先 Artifact 域(扫描→converge→失败记台账),后 Semantic 域(同构)。两域互不嵌套事务。
- `[UNIFIED-WORKER-002]` **故障粒度按域独立**:单域连续 infrastructure 失败达阈值只把该域置 `Faulted`,不牵连另一域;`RecoveryWorkerHealth` 按域分列(artifact/semantic 各自 state、连续失败数、最后错误)。
- `[UNIFIED-WORKER-003]` 阈值沿用单一 `failure_threshold` 数值,按域独立累计(不新增每域独立配置项)。
- `[UNIFIED-WORKER-004]` `stop`/首轮立即 bounded scan/轮询间隔语义与现 worker 一致,不因双域改变。

### 3.3 统一 TaskCommitReceipt(读侧聚合)

- `[RECEIPT-UNIFY-001]` 新增 `enum TaskCommitReceipt { Plain(TaskReceiptRecord), Artifact(ArtifactTaskCommitReceipt), Semantic(SemanticTaskCommitReceipt), Resource(ResourceTaskCommitReceipt), SemanticResource(SemanticResourceTaskCommitReceipt) }`,穷举、non_exhaustive 不适用(五变体即全集,新增权威域时再扩展)。
- `[RECEIPT-UNIFY-002]` `commit_receipt_digest()` 对五变体给出确定性摘要:canonical 编码 + 既有各 receipt 的身份字段,同 receipt 跨重启摘要一致。
- `[RECEIPT-UNIFY-003]` **纯读侧聚合**:不建持久化统一表、不改既有 receipt 表与写入路径;各权威 receipt 仍是唯一事实源(ADR-0005:不提前冻结 schema 形状)。
- `[RECEIPT-UNIFY-004]` Display/Debug 穷举;digest 公式在类型文档钉死,变体扩展时必须同步。

## 4. 错误处理

- `SemanticRecoveryError` 家族镜像 Artifact 恢复错误语义:CAS 冲突、`Escalated` 拒扫、告警身份不存在、acknowledge 幂等重放。
- `CoordinatorError` 既有 typed 三权威错误不变;worker 把域内错误归 infrastructure / plan 级失败两类,分别走退避与台账。

## 5. 验收门(测试矩阵)

| 组 | 证明 |
|---|---|
| semantic 台账 F1–F4 故障注入 | kill-9 中断 / commit 后崩溃 / IoErr / 静默丢写下台账一致、CAS 无双记、重放幂等 |
| 统一 worker 双域驱动 | 两域各有 pending 时单轮全收敛;一域注入连续失败置 Faulted 后另一域继续收敛 |
| 无 caller 重启收敛(核心) | 进程重启、无任何调用方,worker 自动把 pending semantic plan 收敛到唯一终态 |
| 台账损坏自愈 | 删除台账行后重扫 durable plan 重建调度,收敛结果不变 |
| 统一 receipt digest | 五变体摘要确定性、穷举、跨重启一致 |

各车道独立"失败测试→最小实现→审查"闭环;全量门 `cargo test --workspace --no-fail-fast` + clippy/fmt 双 0;三平台 CI。

## 6. 车道编排(写集不相交,可并行)

| 车道 | 写集 | 内容 |
|---|---|---|
| W26-001 | nlos-task semantic_commit.rs/recovery.rs/migrations.rs/schema v42 | Semantic 台账 + 扫描 + 告警面 |
| W26-002 | nlos-commit-coordinator worker.rs(+tests) | durable_cycle 双域 + 按域 Faulted + health 分列;**semantic 半边依赖 W26-001 的扫描/台账 API——可先行开发 Artifact 半边与测试 seam,集成验收在 W26-001 落地后的屏障点** |
| W26-003 | nlos-task model.rs(+新 receipt.rs 若拆) | 统一 TaskCommitReceipt + digest |
| W26-004(串行收尾) | docs/management/stage-b-progress.md、evidence、本 spec 状态 | 登记与证据同步 |

## 7. 兼容性清单

1. Artifact 台账、worker 既有行为对外部观察者保持不变(health 增列属增量)。
2. 既有 `list_incomplete_semantic_commit_plans` + `converge_pending` 手动路径保留——worker 是新增自动驱动,不剥夺显式调用。
3. schema v42 只加表,不改既有列;降级路径 = 忽略新表(旧版本读不到新表数据即视作无台账,plan 事实不受损)。

## 8. 已知限制(如实登记)

- 仍是单机本地 SQLite H3 证据;不外推跨 Cell、真实断电、远端 attestation。
- Resource/Operation 的 prepare/finalize 与其恢复接入留待 W27+ 届时评估(届时若有三域样本,再评估台账泛化)。
- 统一 receipt 不改变任何持久化事实,complete TaskWriteSet 的完成度评估不因本 spec 晋升。
