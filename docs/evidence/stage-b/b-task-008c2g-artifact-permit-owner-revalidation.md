# B-TASK-008C2G-ART：Artifact head permit 前 owner 复核

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：在 `CommitPermit` 发放前重新读取已封存 `TaskWriteSet` 中的
  Artifact write head；不等同于 Artifact staging/publication receipt 消费或
  完整 `TaskCommitReceipt` 接线。

## 结论

`SqliteTaskAuthority::request_commit_permit_with_artifact_authority` 为已有
sealed write set 提供显式 Artifact owner 复核边界。对每个 declared write，
TaskAuthority 通过 `ArtifactStore::resolve_head` 要求当前 head revision 仍与
封存的 expected revision 一致，且 proposed revision 仍是下一个连续 revision；
Artifact 缺失、head 已推进或声明的目标 revision 漂移都会在 participant
registry freeze 前 fail closed。

该 API 只复核 head，不 stage/publish bytes，也不生成 Artifact publication
receipt。普通 `request_commit_permit` 的历史兼容行为保持不变；相同 permit 请求
的 replay 返回原 durable decision，不重复 owner readback。

## 验证

- `artifact_write_declaration_binds_post_permit_publication_plan` 先用错误
  `ArtifactStore` 验证 owner readback 失败且不发放 permit，再用正确 authority
  验证发放与 replay，随后继续既有 publication-plan binding 测试。
- `cargo test -p nlos-task --test participant_registry artifact_write_declaration_binds_post_permit_publication_plan --quiet`
- `cargo test -p nlos-task --quiet`

## 明确缺口

Artifact staging/publication receipt consumption、TaskCommitReceipt 的统一
Artifact/Resource/Semantic publication 嵌套、跨 authority prepare→activate/complete
以及完整 TaskWriteSet 仍未实现；该切片不替代 Semantic publication receipt 的
authority ownership 决策。
