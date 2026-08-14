# B-TASK-008C2B：Artifact proposed-write 与 publication plan binding

- 状态：`PARTIAL_PASS`
- 日期：2026-08-14
- 范围：把 Artifact proposed write 从 authority-verified seal 绑定到 permit 与 post-permit publication plan；不等同于 Artifact publication receipt consumption、Semantic publication 或 complete TaskWriteSet。

## 结论

`TaskWriteSetRequest.artifact_writes` 现在由 TaskAuthority 在 seal 期间逐项回读 ArtifactAuthority 当前 head。每个 declaration 校验 ArtifactId 对应的当前 head revision、`proposed_revision = expected_head_revision + 1`、内容 digest/size proposal，以及 Artifact head endpoint proof 和同一 OPEN participant registry membership；重复 Artifact slot、revision 溢出、owner proof 缺失或 registry 漂移均 fail closed。proposal 只表达未来写入意图，不把 bytes 或 head 变更冒充为已发布事实。

schema v19 新增 immutable `task_write_set_artifact_writes` child 和 `artifact_write_set_root` parent root。schema v20 对历史 Artifact plan parent 做一次事务性 rebuild，移除旧的 `artifact_plan_root = write_set_root` equality constraint：permit-bound TaskWriteSet root 与含 staging identity 的 publication-plan root 现在是两个独立 commitment；两者都在 durable load/permit-time 重新计算，Artifact plan identity 与 child update/delete 仍由 trigger 保护。历史 v0–v19 数据迁移保留原 rows，不凭空添加 Artifact write declaration。

`plan_artifact_commit` 命中 sealed `write_set_root` 时，canonical expectations 必须逐位匹配 sealed Artifact write 的 ArtifactId、目标 revision、内容 digest 和 size；staging ID 被有意忽略，因为它由 post-permit Artifact staging idempotency key 确定。新 plan 的 `write_set_root` 仍等于 permit root，`artifact_plan_root` 单独保存 expectation root，ArtifactAuthority `stage_revision` 使用 permit root。命中 sealed Artifact write 的 effectful permit 可以进入 Artifact publication authorization；terminal `finalize_artifact_commit` 仍拒绝 effectful permit，避免把局部授权误报为完整 Task commit。

## 验证

- `cargo test -p nlos-task --test participant_registry artifact_write_declaration_binds_post_permit_publication_plan -- --nocapture`：通过。
- `cargo test -p nlos-task --quiet`：通过，包含 19 个测试目标、历史 schema v5/v7/v8 migration 与 legacy Artifact plan compatibility。
- `cargo fmt --all -- --check`：通过。
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings`：通过。
- `cargo test --workspace --all-targets --quiet`：通过。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。

新增 participant integration test 覆盖 seal → effectful permit → mismatch expectation rejection → matching plan → post-permit staging → publication authorization；同时验证 plan root 与 TaskWriteSet root 的分离、staging identity 的确定性和 Artifact stage 的 permit-root binding。

## 明确缺口

本切片没有消费 Artifact publication receipt，也没有把 stage/publish receipt 纳入最终 TaskCommitReceipt；Semantic publication Admission/Durability receipt、Resource activation/consume/finalize、per-effect Operation/Action/Driver/Channel linkage、phantom/range serializability、跨 authority prepare→activate、term takeover、attestation 与宿主 enforcement 仍未接入。没有 sealed row 的 legacy planned-effect permit path 仍保留兼容行为。`complete TaskWriteSet` 继续保持 `READY`，下一切片为 `B-TASK-008C2C` Semantic publication / remaining write fields。
