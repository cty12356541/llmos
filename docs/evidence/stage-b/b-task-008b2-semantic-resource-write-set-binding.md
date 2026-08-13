# B-TASK-008B2：Semantic / Resource owner readback

- 状态：`PARTIAL_PASS`
- 日期：2026-08-13
- 范围：在 B-TASK-008A/008B1 seal 上加入 Semantic event read dependencies 与 RESERVED Resource Reservation owner facts；不等同于 Semantic publication、Resource activation 或 complete TaskWriteSet。

## 结论

`TaskWriteSetRequest.semantic_reads` 的每一项由 `SemanticAuthority::inspect_event` 直接回读。TaskAuthority 校验 event ID、immutable `event_log.log_seq` 和 canonical unsigned-event digest；重复 event、log sequence/digest 漂移或缺少已预注册的 Semantic admission endpoint 都 fail closed。Canonical Semantic read rows 按 event ID 排序形成独立 root，并进入 extended write-set root。

`TaskWriteSetRequest.resource_reservations` 的每一项由 `ResourceAuthority::inspect_permit_binding` 直接回读，只接受当前 Driver generation/fence 下的 `RESERVED` Reservation，并校验 caller 期待的 CallId、OperationId、QuoteId。TaskAuthority 同时回读 Driver gateway 与 Resource/Ledger endpoint proofs，要求三类 participant 都已存在于同一 OPEN registry；owner 返回的 account/quote/call/operation/driver/device/generation/fence/upper-bound 进入 immutable resource child 和 canonical root。一次性 activation token 不复制进 write set。

schema v16 新增：

- `semantic_read_set_root` 与 `resource_reservation_set_root` parent columns；
- `task_write_set_semantic_reads` immutable child；
- `task_write_set_resource_reservations` immutable child；
- 四个 immutable update/delete triggers。

v0–v15 历史数据迁移时，空 Semantic/Resource roots 使用零值兼容旧写集 root；已有 rows 不被重写，也不凭空添加 owner facts。partial table/column/trigger schema 在迁移时 fail closed。

## 验证

- `cargo test -p nlos-task --quiet`：通过，包含旧 schema fixture migration。
- participant integration tests：Semantic durable event readback、Resource RESERVED readback、owner mismatch、duplicate/read conflict、participant pre-registration、restart replay。
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。

## 明确缺口

本切片没有把 Semantic append 的 AdmissionReceipt/DurabilityReceipt、Resource `RESERVED → ACTIVE → consume/finalize`、planned Channel/Driver endpoint、EffectSlot/Operation、Artifact/Semantic publication 或 phantom/range SERIALIZABLE token 接入 CommitPermit；也没有跨 authority prepare→activate、term takeover、attestation 或宿主 enforcement 证据。下一切片为 `B-TASK-008C` planned endpoint/effect/publication binding。
