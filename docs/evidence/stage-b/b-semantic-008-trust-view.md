# B-SEMANTIC-008：Trust View 最小前缀

## 1. 验收对象

本切片在 `nlos-semantic` 实现只读 owner-local Trust View 最小前缀：基于已提交 Assertion/Judgment/Verification/Retraction/declassification receipt 与 durable `AdmissionReceipt`，派生单 event 的可见 taint/labels 与 verification 状态；lineage 父节点未 commit 时 fail-closed（`DanglingLineage`）。完整 Trust View engine、TrustPolicy/SemanticCheckpoint、Gate/CRDT、batch DAG、跨进程与 vector checkpoint 不在本切片。

## 2. 实现事实

- **公开 API**：`SemanticAuthority::inspect_trust_view(event_id) -> TrustViewSnapshot`；导出 `TrustViewSnapshot`、`TrustViewVerificationStatus`、`TrustViewVerificationFact`、`TrustViewJudgmentFact`、`TrustViewJudgmentRole`。
- **Taint/labels**：读取 durable `AdmissionReceipt.effective_taint`（已含 admission 时 declassification apply 结果）；Assertion 另暴露 canonical `declassification_receipt_id` 只读字段。
- **Lineage fail-closed**：读路径重验 declared lineage edges + admission receipt 的 captured inputs，任一 parent 不在 `admission_receipts` 则 `DanglingLineage`。
- **Verification 派生**（`[SEM-VERIFY-002]`）：扫描已 commit 的 Event-target VerificationEvent，收集 `TrustViewVerificationFact`；无适用 verification 时为 `Unverified`（不写事件）；最小前缀以最高 `log_seq` 派生 `verification_status`（非 TrustPolicy 聚合）。
- **Judgment 观察**：收集 subject 作为 source/target 端点的 committed JudgmentEvent 事实行。
- **Retraction 观察**：附带 `inspect_event_retraction` 同等 durable 撤回行（不删改目标行、不推导 Gate disposition）。
- **存储**：纯 SQLite 读路径，无 schema 变更（沿用 schema v6）。

## 3. 验证

```text
cargo test -p nlos-semantic
cargo clippy -p nlos-semantic --all-targets -- -D warnings
cargo fmt -p nlos-semantic --check
```

## 4. 证据等级与已知限制

结论：`PARTIAL_PASS / H3 local durable authority（最小子集）`。

- TrustPolicy、SemanticCheckpoint、AuthorityViewReceipt、resolver/aggregation digest 绑定未实现。
- verification 状态未做 policy-aware quorum/独立性/independence 聚合；Criterion-target verification 未纳入 subject event 视图。
- 多 Cell vector checkpoint、跨 authority batch DAG、Gate disposition、CRDT ConflictReceipt 未实现。
- 未运行 workspace 级测试；下游 crate 联动编译未验证。

## 5. 未运行项

- nightly clippy/fmt 双工具链（任务验收仅 stable）。
- 故障注入 VFS matrix、并发多写者压测未新增。
