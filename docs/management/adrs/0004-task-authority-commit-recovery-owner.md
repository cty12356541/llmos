# ADR-0004：TaskAuthority 拥有跨权威提交恢复 worker

- 状态：ACCEPTED
- 日期：2026-08-09
- Owner：TaskAuthority
- 关联 Requirement：`TASK-COMMIT-001`、`TASK-COMMIT-002`、`TASK-TXN-001`、`TASK-CONFLICT-001`
- 关联工作包：`B-TASK-006E`、`B-TASK-006F`、`B-TASK-006G`
- 复审触发器：恢复吞吐需要独立扩缩容；TaskAuthority 故障域阻碍恢复；跨 Cell authority 已引入；恢复 worker 需要独立权限或发布节奏

## 上下文

Artifact-only Task commit 已由无独立持久状态的 coordinator 驱动
`PLANNED → PUBLISHING → READY → FINALIZED`，并验证了重启收敛、三个写入故障点和逐 plan 故障隔离。下一步需要确定谁启动、停止、观察和升级这个 worker；否则周期调度、退避、健康状态和运维接口没有稳定 owner。

这个选择只决定恢复控制环的归属，不改变事实权威：Task commit plan、CommitPermit、TaskHead 与 terminal Task receipt 仍由 TaskAuthority 持久化，Artifact revision/head 与 publication receipt 仍由 ArtifactAuthority 持久化，coordinator 不建立第三份 canonical 状态。

## 候选

| 候选 | 优点 | 主要代价 |
|---|---|---|
| TaskAuthority 内部恢复 worker | 计划与最终 TaskHead 的 owner 同时拥有恢复生命周期；一致性与告警归属清晰；阶段 B 不增加服务 | TaskAuthority 运行时职责增加；必须用 composition 层避免 crate 循环依赖 |
| 独立 commit-recovery service | 可独立扩缩容、发布和隔离故障 | 立即引入服务身份、发现、权限、HA 与运维面，当前证据不足以证明必要性 |
| Process supervisor 托管 | 进程生命周期入口集中 | 把 Task canonical commit 恢复耦合到普通 Process 生命周期，语义 owner 不自然 |

## 决定

采用 **TaskAuthority 内部恢复 worker**。

具体边界如下：

1. TaskAuthority service/runtime 是 worker 的生命周期 owner，负责启动扫描、周期唤醒、停止、健康与故障升级。
2. `nlos-task` 核心 crate 继续只包含 TaskAuthority 持久语义，不反向依赖 ArtifactAuthority。
3. 运行时 composition 层可持有 `SqliteTaskAuthority`、`ArtifactStore` 与无状态 coordinator；这仍属于 TaskAuthority service 的内部实现，不形成第三 authority 或独立服务身份。
4. worker 每次只处理 TaskAuthority 给出的有界 pending snapshot；单 plan 失败不得阻塞同批其他 plan。
5. worker 停止或崩溃不得回滚、删除或伪造任何已提交 prefix；重启从两个 authority 的 durable truth 幂等收敛。
6. 健康状态必须区分正常运行、退避中的可恢复故障、达到阈值后的 faulted 终态和显式停止；错误至少保留 plan identity 与 Task/Artifact authority 来源。

## 后果

- 当前阶段不创建独立 `commit-recovery` 服务，也不为 worker 分配 ServiceDirectory identity。
- Process supervisor 后续只监督 TaskAuthority service 进程，不拥有 commit 恢复业务语义。
- 第一个实现切片应提供启动即扫描、周期扫描、有界指数退避、可 join 停止与只读健康快照；持久 retry ledger、跨进程运维 API 和 metrics 可在后续独立验收门推进。
- composition 层的 public 名称必须表达 TaskAuthority ownership，避免把现有 `nlos-commit-coordinator` crate 的物理位置误解成独立服务。

## 退出与迁移策略

若复审触发器成立，可把同一个无状态协调算法迁移到独立服务。迁移前必须增加明确的 service identity、Capability/authorization、单活或租约 fence、健康/升级协议，并证明不会出现两个 unfenced worker 争用或扩大权限。因为 canonical progress 全在两个 authority 中，迁移不得复制或搬迁第三份恢复数据库。

## 当前证据与缺口

`B-TASK-006E/F/G` 已证明无状态 coordinator 的重启收敛、事务故障保真和逐 plan 隔离，足以支持 owner 决策。[B-TASK-006H](../../evidence/stage-b/b-task-006h-task-authority-recovery-worker.md) 已进一步验证启动扫描、周期调度、有界指数退避、生命周期健康、故障阈值和及时 join；持久 retry/escalation ledger、jitter、外部运维接口与真实进程/VFS 故障仍未完成。
