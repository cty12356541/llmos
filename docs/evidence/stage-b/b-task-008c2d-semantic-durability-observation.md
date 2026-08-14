# B-TASK-008C2D：Semantic DurabilityReceipt 观察与可选绑定

- 状态：`PARTIAL_PASS`
- 日期：2026-08-14
- 范围：在 C2C 的直接 Durable AdmissionReceipt 路径上增加可选、owner-verified `DurabilityReceipt` 观察与绑定；不等同于 Semantic publication/finalization 或完整 `TaskWriteSet`。

## 结论

`SemanticAuthority::inspect_durability_receipt` 现在可以按 `(event_id, receipt_id)` 精确读取 immutable durability proof，并拒绝把仅有的 Semantic outbox identity当作 DurabilityReceipt。`TaskWriteSetSemanticAppendRequest` 可声明一个可选 `durability_receipt_id`；TaskAuthority 不信任 caller 的字段，而是从 SemanticAuthority 精确回读、校验 event identity 后才把 receipt ID 写进 sealed record。

schema v22 为 `task_write_set_semantic_appends` 增加 nullable `durability_receipt_id`。没有该字段的历史 v21 rows 保留 C2C 的 Semantic append root v1 公式；至少一个 append 携带 durability receipt 时使用 append-root v2，并把每个 append 的 presence/ID 纳入 root。load、replay、permit issuance 继续重算并校验 child/root，迁移不补造 receipt。

## 验证

- `verified_write_set_seal_binds_semantic_event_readback_and_append` 使用 raw immutable Semantic durability row，覆盖 optional receipt owner readback、root/permit binding、重放、重启回读以及缺失 receipt 的 fail-closed 路径。
- `cargo test -p nlos-task --test participant_registry --quiet`：12 项通过。
- `cargo test -p nlos-task --quiet`、workspace 全目标测试、fmt 与 Clippy 作为本提交验收门执行。

## 明确缺口

该切片只提供一个可被 TaskWriteSet 引用的 durability observation；当前 SemanticAuthority 仍没有完整的 checkpoint producer、跨 authority publication transaction 或 finalization API。没有 `DurabilityReceipt` 的直接 `AdmissionReceipt(durability = Durable)` 仍是合法局部路径，但 outbox row、DurabilityReceipt 和 publication receipt 都不能被相互推断。TaskCommitReceipt、Semantic final publication、Artifact receipt consumption、Resource activation/consume/finalize、per-effect Operation/Channel linkage、phantom/range validation、跨 authority prepare→activate、term takeover 与宿主 enforcement 仍未完成。
