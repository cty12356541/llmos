# B-SEMANTIC-007：Declassification receipt 最小前缀

## 1. 验收对象

本切片实现 v0.5 `[SEM-DECLASS-001]` 最小子集：adjudicator 签发不可变 declassification receipt；Assertion admission 在 lineage/taint union 之后按 receipt 移除 bounded labels；fail-closed 绑定校验。Trust View、batch DAG、完整 declassification policy 解引用不在本切片。

## 2. 接管与续建说明

接续波次 11 [W11-S] 中止候选：遗留 `declassification.rs` / `tests/declassification.rs` 实现体，本 Attempt 完成 lib/model/canonical/schema v6 接线、公开 API 与 Evidence。

## 3. 实现事实

- **Canonical**：Assertion 信封 field 12 从固定 `null` 改为 optional `declassification_receipt_id`（ReceiptId）；encode/decode 严格 round-trip，旧 bytes（field 12 = null）仍解码为 `None`。
- **Receipt 签发**（`issue_declassification_receipt`）：域分隔 `llmos/declassification-issue/v1` 授权 EventId + adjudicator `semantic_signature_message` 验签；`SEMANTIC_ADJUDICATE` capability 授权；source events 必须已 commit（`admission_receipts`）；nonce 16..=32 字节幂等（相同请求 `Replayed`，冲突请求 `DeclassificationNonceReplayConflict`）；store signer 对 receipt core digest 签名并验绑。
- **Receipt 存储**（schema v6）：append-only `declassification_receipts` + `declassification_source_events`；`user_version` 5→6 迁移链接入 `open()`；v0/1/2/3/4/5 路径均可达 v6。
- **Admission apply**（`append_assertion`）：lineage union 得 effective taint 后调用 `apply_declassification`；校验 holder/scope/purpose/lineage（declared+captured）/expiry/removed labels present；通过后 `effective_taint.without(removed_labels)`；无 receipt id 时 no-op。
- **公开 API**：`issue_declassification_receipt`、`inspect_declassification_receipt`、`declassification_issue_authorization_id`；`DeclassificationReceipt` / `IssueDeclassificationReceiptRequest` / `IssueDeclassificationDecision` 类型导出。

## 4. 验证

```text
cargo test -p nlos-semantic
cargo test -p nlos-semantic --test declassification
cargo clippy -p nlos-semantic --all-targets -- -D warnings
cargo fmt -p nlos-semantic --check
```

结果：**全部通过**（26 passed / 0 failed）。test bins：declassification 4 passed（无 receipt 继承 union taint、有效 receipt 降 taint、issue+append 幂等、not-found/expired/source/label 负路径）、semantic_authority 12 passed（含 v1→v6 迁移）、spec_canonical 5、typed_events 4、lib schema 单元 1；doc-tests 0。Clippy 零警告，fmt clean。

## 5. 证据等级与已知限制

结论：`PARTIAL_PASS / H3 local durable authority（最小子集）`（与 B-SEMANTIC-001/006 基线一致）。

- holder mismatch / scope mismatch / purpose mismatch 负路径由 typed error 覆盖，集成测试矩阵未逐项独立断言（apply 逻辑 fail-closed 已实现）。
- declassification 仅 Assertion admission 路径；Spec/Judgment/Verification/Retraction typed admission 不支持 field 12 receipt（canonical 固定 null）。
- Trust View / authority-aware view 派生态、declassification policy 解引用、captured-input trap 自动检测未实现。
- 未运行 workspace 级测试；下游 crate 联动编译未验证。

## 6. 未运行项

- nightly clippy/fmt 双工具链（任务验收仅 stable）。
- 故障注入 VFS matrix、并发多写者压测未新增。
