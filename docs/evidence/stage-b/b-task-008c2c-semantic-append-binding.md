# B-TASK-008C2C：Semantic 追加声明与直接耐久收据绑定

- 状态：`PARTIAL_PASS`
- 日期：2026-08-14
- 范围：把 `TaskWriteSet` 的 Semantic staging declaration 接入 SemanticAuthority owner readback，并把声明、target scope、required durability 与直接 `AdmissionReceipt` identity 固化到 schema v21；不等同于 Semantic publication transaction、`DurabilityReceipt` 消费或完整 `TaskWriteSet`。

## 结论

`TaskWriteSetRequest.semantic_appends` 现在必须经过 `SemanticAuthority` 回读。TaskAuthority 对每个声明校验：

1. `event_id` 在当前 Semantic event log 中存在；
2. caller 声明的 `NamespaceId | TaskId` target 与 event envelope scope 逐位一致；
3. `required_durability = Durable`，且 owner 返回的 `AdmissionReceipt.durability = Durable`；
4. owner 返回的 `receipt_id` 被写入 sealed record，而不是接受 caller 提供的 receipt identity。

Semantic read 或 append 使用同一 owner endpoint proof，并要求对应 participant 已预注册在 OPEN registry。重复 event、target scope 漂移、缺失 owner readback 或缺少直接 durable AdmissionReceipt 都 fail closed。

schema v21 新增 immutable `task_write_set_semantic_appends` child、`semantic_append_set_root` parent root 及 update/delete triggers。含追加声明的 write set 使用 v6 root domain，把 canonical append root 纳入 permit-bound `write_set_root`；load、replay 和 `CommitPermit` issuance 会重新计算并校验 child rows/root。历史无追加声明的 rows 保留原 v1–v5 root 语义，迁移不补造 Semantic 事实。

## 验证

- `verified_write_set_seal_binds_semantic_event_readback_and_append`：raw Semantic event + AdmissionReceipt owner fixture，覆盖 scope/readback、append receipt identity、非零 append root、target mismatch、permit root binding。
- `cargo test -p nlos-task --test participant_registry --quiet`：12 项通过。
- `cargo fmt --all -- --check`、`cargo test -p nlos-task --quiet`、workspace 全目标测试与 Clippy 作为本提交验收门执行。

## 明确缺口

本切片只证明直接 durable `AdmissionReceipt` 已被 TaskWriteSet 声明消费；没有把 `durability_receipts`、semantic outbox acknowledgement 或跨 authority publication receipt 当作已完成，也没有写入最终 `TaskCommitReceipt`。因此不能据此把 Task 标为 `COMMITTED`：v0.5 `[TASK-WRITE-003]` 要求每个 required event 同时取得 `AdmissionReceipt + DurabilityReceipt` 后才能发布。Semantic publication transaction/finalize、Artifact publication receipt consumption、Resource activation/consume/finalize、per-effect Operation/Channel linkage、phantom/range validation、跨 authority prepare→activate、term takeover、attestation 与宿主 enforcement 仍待后续 `B-TASK-008C2D` 及之后切片。
