# B-TASK-007B1：Artifact/Semantic authority endpoint proofs

## 1. 验收对象

本切片为 v0.5 `[DIST-TASK-001]` 的外部 participant registration 建立前置事实：Artifact head 与 Semantic admission endpoint 必须先拥有由各自 authority 分配、持久化并可精确回读的 identity/generation/admission Receipt，TaskAuthority 后续不得接受 caller 自报 tuple。

## 2. 实现事实

- Artifact schema v3 为每个 Artifact head 建立 immutable endpoint proof：`artifact_id + TaskParticipantId + Generation + ReceiptId`。新 Artifact 创建与 proof 写入同一 transaction；v2 既有 Artifact 在迁移事务中逐项回填 authority-assigned identity/Receipt。
- Semantic schema v3 建立单例 immutable admission endpoint proof：`TaskParticipantId + Generation + ReceiptId`，首次 authority 建库或 v2→v3 迁移时由 SQLite `randomblob(16)` 分配。
- 两个 authority 都提供 typed inspect API；identity、generation 与 Receipt 跨重启逐位稳定，底层 UPDATE/DELETE trigger 拒绝历史改写。
- 迁移器识别测试使用的“完整新 schema + 旧 user_version”结构并只重盖版本号；任何只存在部分 endpoint table/trigger/identity coverage 的结构拒绝打开。
- proof value 本身是可运输值，不被当作 bearer credential。后续 Task registration 必须在线向具体 owning authority 精确回读验证，不能仅信任 struct 字段。

## 3. 验证

```text
cargo test -p nlos-artifact -p nlos-semantic
cargo clippy -p nlos-artifact -p nlos-semantic --all-targets -- -D warnings
```

结果：Artifact 全套测试、Semantic 全套测试及 Doc tests 全部通过，Clippy 零警告。新增测试覆盖 authority assignment、restart replay、DDL immutability，以及 v1/v2 迁移后 endpoint proof coverage。

## 4. 证据等级与限制

结论：`PARTIAL PASS / H3 local endpoint-proof baseline`。

- 当前只覆盖 Artifact head 与 Semantic admission；Driver gateway、Resource/Ledger、Channel/Topic 尚无 endpoint proof。
- endpoint proof 尚未由 TaskAuthority 消费，因此还没有 OPEN registry generation CAS、duplicate/conflict replay 或 frozen-registry rejection 证据。
- proof 未带跨进程签名、route generation、authority term/lease 或远程 attestation；当前保证来自同进程直接 authority readback + durable SQLite immutability，不外推为跨 Cell 身份认证。
- Artifact 创建 API 仍消费上游分配的 `ArtifactId`；本切片只保证 participant endpoint identity 由 ArtifactAuthority 自身分配。

下一验收门：`B-TASK-007B2` 让 TaskAuthority 通过具体 Artifact/Semantic authority 在线精确回读 proof，以 OPEN registry generation/root CAS 注册 participant，并验证 replay/conflict/frozen/restart 语义。
