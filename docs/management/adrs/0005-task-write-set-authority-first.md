# ADR-0005：TaskWriteSet 采用 authority-first 实施顺序

- 状态：ACCEPTED
- 日期：2026-08-09
- Owner：TaskAuthority / ProcessAuthority / SemanticAuthority / Resource reference monitors
- 关联 Requirement：`MODEL-ID-001`、`TASK-SNAPSHOT-002`、`TASK-WRITE-002`、`TASK-COMMIT-001`、`DIST-TASK-001`
- 关联工作包：`B-TYPES`、`B-PROCESS`、`B-TASK`
- 决策来源：用户在 2026-08-09 明确选择候选 2
- 复审触发器：Authority 前置链无法形成可测试纵切；新增 authority 被证明只复制 handle 而没有独立事实；Slice K 因依赖链无法取得端到端证据

## 上下文

schema v10 已建立 durable `TaskSnapshotReceipt` 与 attempt binding。下一验收门的完整 `TaskWriteSet` 按 v0.5 必须绑定 AgentInstance/incarnation、IsolationDomain generation、IntentSpec、producer ControlDomain、Semantic append、Driver/Reservation、participant registry、精确 read/write set 和 snapshot receipt。

当前仓库只有部分 nominal ID 和 Task/Artifact/Operation authority；Process/IsolationDomain、Semantic、Resource/Driver binding 与 Task participant registry 尚未形成 durable authority。若立即把所有字段作为 caller-supplied ID/digest 写入 TaskAuthority，schema 会先冻结“未验证引用”，之后容易把结构完整误报为 authority 已验证。

## 候选

| 候选 | 优点 | 主要代价 |
|---|---|---|
| 先落完整 typed envelope，缺失 authority 暂存未验证 binding | 最快形成 TaskWriteSet 外形；可尽早计算 root | durable schema 会包含尚无验证来源的字段，容易形成第二事实源；后续 authority 接线需要迁移语义 |
| **先实现依赖 authority，再落 TaskWriteSet** | 每个 binding 都能由其 owner 查询、CAS/replay 或消费 Receipt；最终 schema 直接表达权威关系 | 前置工作更多，TaskWriteSet 与 Slice K 时间后移 |
| 继续 artifact-only write set，跳到 Slice K | 复用现有路径，短期演示快 | 无法满足 `TASK-WRITE-002`，纵切会建立在已知缺口上 |

## 决定

采用 **authority-first** 顺序，不把 caller-supplied opaque binding 当作完整 `TaskWriteSet`。

实施链固定为：

1. 把 v0.5 明确要求的共享 nominal identity 收敛到 `nlos-types`，消除 TaskGroup/Effect 等 crate-local 同名类型。
2. 建立 Process/AgentInstance/IsolationDomain 的 durable binding authority，至少具备 generation/fence、幂等注册、查询、重启恢复和冲突拒绝。
3. 建立 Semantic target/event 与 Resource/Driver/Reservation 的 authority-owned binding/readback；未激活或未验证记录不得进入可签发 permit 的 write set。
4. 在 TaskAuthority 建立 participant registry generation/root，seal 后冻结 participant 集；permit issuance 必须消费同一 generation/root。
5. 最后持久化完整 `TaskWriteSet`，由 TaskAuthority 从上述权威记录构造/校验 canonical root，再与 snapshot receipt、group binding、effect set 和 CommitPermit 逐位绑定。

“实现 authority”在本 ADR 中不等于生产级完整服务，但至少必须拥有独立 durable fact、typed state/generation、冲突与 replay 语义、重启测试及明确 Evidence；仅增加字段、trait stub 或 caller-provided digest 不算完成。

## 后果

- `complete TaskWriteSet` 保持 `READY`，不能因 Rust struct 字段齐全提前晋升。
- `B-PROCESS`、Semantic/Resource binding 和 `DIST-TASK-001` participant registry 成为 B-TASK 的显式前置链。
- 新 authority 可以先以单节点 SQLite reference implementation 取得 H3 证据，但不得外推为跨 Cell、签名验证、宿主强制执行或生产 HA。
- `IntentSpecId` 按 v0.5 类型词典使用受限 `SemanticEventId/SpecId` 语义，不新造与 `IntentId` 混淆的同名 ID。

## 退出与迁移策略

若复审证明某 binding 没有独立 authority 事实，应删除该伪 authority，并在 TaskWriteSet 中降级为明确的 immutable reference/digest；这种降级必须更新 ADR、GuaranteeTier 和 Evidence，不能静默改回 caller-supplied trusted field。已经发布的 durable schema 不得通过重写历史行迁移。

## 当前证据与缺口

`B-TASK-008C2G-RES` 增加了 Resource owner-aware permit issuance：对 sealed Reservation 在 participant registry freeze 前逐项回读 RESERVED binding，错误 authority、已激活记录或 Driver fence 漂移均 fail closed；该 opt-in guard 不激活/消费 Reservation，也不生成 Resource publication/finalization receipt。详见 [Evidence](../../evidence/stage-b/b-task-008c2g-resource-permit-owner-revalidation.md)。Semantic publication receipt producer/consumer 仍是下一主门，完整 TaskWriteSet 仍保持 `READY`。

[B-TASK-006O](../../evidence/stage-b/b-task-006o-durable-task-snapshot-receipt.md) 已证明 snapshot receipt 的本地持久化、replay 和 attempt binding；[B-PROCESS-001](../../evidence/stage-b/b-process-001-durable-execution-binding-authority.md)、[B-RESOURCE-001](../../evidence/stage-b/b-resource-001-driver-reservation-binding-authority.md)、[B-SEMANTIC-001/002B](../../evidence/stage-b/b-semantic-002b-durable-spec-event-admission.md) 已建立本地 reference authority facts。[B-TASK-007A–007D1](../../evidence/stage-b/b-task-007d1-participant-binding-propagation.md) 已建立 authority-assigned participant registry、verified endpoint registration、permit freeze，以及 EffectPermit/Task Receipt generation/root 传播与在线重验。

这些证据仍是单节点 H3 partial proof：真实 snapshot builder/checkpoint 验签、Channel endpoint、operation prepare→activate、TaskAuthority term/takeover coverage 与完整 planned endpoint seal 仍缺失。`B-TASK-008A` 已开始步骤 5，实现 authority-verified snapshot/read-set seal 和 Artifact head readback；`B-TASK-008B1` 又把 Process/AgentInstance/IsolationDomain active binding 与 owner-derived endpoint proof 接入 seal；`B-TASK-008B2` 再加入 Semantic event 与 RESERVED Resource Reservation owner readback；`B-TASK-008C1` 进一步把 planned effect descriptor 持久化为 schema v17 immutable child，并在命中 sealed root 的 `CommitPermit` issuance 时执行 exact effect/root replay 校验；`B-TASK-008C2A` 再把 Artifact/Semantic/Process/Driver/Resource owner endpoint proof 持久化为 schema v18 immutable child，并在 permit 前确认其 frozen participant membership；`B-TASK-008C2B` 新增 schema v19 Artifact proposed-write child，并以 schema v20 migration 分离 permit-bound TaskWriteSet root 与含 staging identity 的 Artifact publication-plan root，plan 只接受与 sealed Artifact declaration 逐位匹配的 owner-independent staging identity；`B-TASK-008C2C` 新增 schema v21 Semantic append child/root，要求 target scope 与 event envelope 一致，并由 SemanticAuthority 直接回读 Durable AdmissionReceipt 后才允许进入 sealed write set；`B-TASK-008C2D` 以 schema v22 增加可选 owner-verified DurabilityReceipt ID，并为带二级收据的 append 使用 append-root v2，同时保留 direct-Durable AdmissionReceipt 的历史 v1 公式；`B-TASK-008C2E` 再把这些 owner proofs 接入 Issued permit 的 Semantic-aware finalize 前复核，并保持 terminal replay 不重复访问 owner；`B-TASK-008C2F` 以 schema v23 将 expected admission-policy digest owner binding 纳入 append-root v3，同时为历史 rows 保留 NULL。该切片仍保留 B-TASK-002/003 的 legacy 无 sealed row planned-effect permit 兼容路径，且不消费 semantic outbox publication acknowledgement、Semantic publication/finalization、Artifact publication receipt、Resource activation/finalize 或 operation/Channel linkage，因此没有把 caller-supplied opaque fields 晋升为完整权威事实。Semantic/Artifact final publication、Resource activation/finalize、per-effect operation linkage 和 complete TaskWriteSet 仍缺失；下一步 `B-TASK-008C2G` 处理 Semantic publication receipt producer/consumer 与 TaskCommitReceipt 接线，完成 TaskWriteSet 仍保持 `READY`。
