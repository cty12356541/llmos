# B-TASK-008A：authority-verified snapshot/read-set seal

- 状态：`PARTIAL_PASS`
- 日期：2026-08-13
- 范围：TaskAuthority 的第一段 authority-first TaskWriteSet seal；不等同于 v0.5 的 complete TaskWriteSet。

## 结论

`nlos-task` schema v13 新增了 durable `TaskWriteSet` parent/child records。`TaskAuthority::seal_task_write_set` 要求 attempt 已绑定 durable `TaskSnapshotReceipt`，并在 seal 事务中复验：snapshot receipt、attempt generation、TaskHead、当前 TaskGroup commit binding，以及 OPEN participant registry。混合不可结算 snapshot、过期 generation、TaskHead 漂移和 participant/group 漂移均 fail closed。

Artifact read facts 不由 caller 提供的 digest 充当权威。seal 前 TaskAuthority 逐项回读 `ArtifactStore::resolve_head`，校验 artifact owner participant、expected revision 和 optional digest；重复 artifact、revision/digest 不一致均返回 typed conflict。规范化排序后的 read rows 形成 domain-separated `artifact_read_set_root`，再和 task/attempt/snapshot/head/fence/group/participant facts 形成 `write_set_root`。

同一 `(task_id, idempotency_key)` 与相同 canonical content 可逐位 replay；异内容会拒绝。CommitPermit 请求若携带已 seal 的 `write_set_root`，会加载并复验 seal record、当前 group binding 和 OPEN registry，再冻结相同 participant generation/root；因此 permit 不会静默接受另一个 registry snapshot。

## 持久化与迁移

- schema 从 v12 升至 v13。
- `task_write_sets` 保存 snapshot/head/fence/group/participant bindings 和 canonical roots。
- `task_write_set_artifact_reads` 保存逐 artifact revision/digest read facts。
- parent/child 均有 immutable update/delete triggers；partial schema 或不完整 trigger set 在 migration 时 fail closed。
- 既有 v0–v12 数据按既有迁移链升级到 v13，不重写历史 permit/receipt 事实。

## 验证

- `cargo test -p nlos-task --quiet`：全部测试通过（本地 macOS/arm64）。
- 新增 participant integration coverage：receipted snapshot + artifact head readback、root replay、read conflict、restart inspect、permit binding/freeze。
- 既有 schema-version、TaskGroup、EffectPermit、Artifact publication、effect-history 回归测试同步到 v13 并通过。

## 明确缺口

本切片只封存 snapshot/read-set 和 Artifact head facts。尚未把 Process/AgentInstance/IsolationDomain、Semantic IntentSpec/event/control append、Resource/Driver/Reservation、planned endpoint/Channel、完整 effect set、write publication 或完整 CommitReceipt evidence 纳入 root；也没有跨进程 attestation、term/takeover、真实硬件强制执行或分布式原子事务证据。因此 `B-TASK-008A` 只能记为局部 `PARTIAL_PASS`，下一切片为 `B-TASK-008B` external authority binding。
