# B-SEMANTIC-002B：Durable SpecEvent admission

## 1. 验收对象

本切片把 [B-SEMANTIC-002A](./b-semantic-002a-canonical-intent-spec-body.md) 的 canonical `IntentSpecBody` 接入 v0.5 `[SEM-ID-003]`、`[SEM-TXN-001/002]`、`[SPEC-ID-001/002]` 与 `[SPEC-IMM-001]` 的 SPEC event durable admission。

## 2. 实现事实

- `UnsignedSpecEvent` 使用与 Assertion 相同的 17-field deterministic-CBOR envelope，`event_type=SPEC`；payload 同时包含重算的 `SpecBodyDigest` 与完整 canonical body，不允许把完整结构降格为可漂移旁表。
- `SpecId` 直接等于 SPEC envelope 的 `EventId`，因此除 body 外还覆盖 issuer、scope、nonce、producer time、lineage、validity、purpose、execution binding、ControlDomain 与 key。
- `append_spec` 在一个 SQLite IMMEDIATE transaction 中依次验证 canonical/body digest/EventId、真实 Ed25519 producer signature、current Process generation、Capability holder/scope/right/purpose/time、committed lineage 与 taint，再提交 immutable spec body、event、signature、log、edges、signed DURABLE AdmissionReceipt 与 outbox。
- 重试先按 EventId 读取 canonical bytes；相同 bytes/signature/body 返回原 Receipt，不同 bytes 触发 typed collision，避免 `INSERT OR IGNORE`。
- semantic schema v2 为 Assertion content identity 与 SpecBody identity 建立严格 tagged XOR；`spec_bodies` append-only。v1→v2 rebuild 在受控窗口关闭 FK enforcement，复制 authority rows后执行 `foreign_key_check`，再恢复 enforcement。
- `SemanticEventRecord` 返回 tagged `SemanticPayloadIdentity`，不会把 `SpecBodyDigest` 误称为 `ContentDigest`。

## 3. 验证

```text
cargo test -p nlos-semantic
cargo clippy -p nlos-semantic --all-targets -- -D warnings
```

结果：15 项 Semantic tests 全部通过，Clippy 零警告。其中新增覆盖：SPEC canonical/type/body digest 反例；Assertion parent→Spec lineage/taint、store signature、durable replay/restart、append-only body；真实含 Assertion/Receipt/lineage 的 v1 store 无损迁移与 FK 检查；最小 v1 schema rebuild 单元测试。

## 4. 证据等级与限制

结论：`PARTIAL PASS / H3 local durable authority`。

- 已形成可供 TaskWriteSet 引用的真实、durable、signed SpecEvent/AdmissionReceipt identity，但 `ResourceVector`、`ArtifactSelector`、authority/independence/risk policy 仍是 immutable digest reference，尚未解引用相应 authority。
- Process binding 仍未绑定 Principal；当前只有 local process issuer execution，ExternalAttestation/HumanSession ingress 未实现。
- Judgment/Verification/Retraction、CriterionEvaluationWindow、ResolvedArtifactSet、Trust View/Gate/Escrow settlement、declassification、batch append 和 fault VFS matrix 尚未实现。

下一验收门：在 TaskAuthority 建立 durable participant registry generation/root 与 freeze CAS，把 Semantic admission endpoint 纳入 permit 前完整 participant 集。
