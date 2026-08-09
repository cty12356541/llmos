# B-SEMANTIC-002A：Canonical IntentSpec body identity

## 1. 验收对象

本切片落实 v0.5 `[SPEC-ID-001]`、`[SPEC-HARD-001]`、`[SPEC-AGG-001]` 与 `[SPEC-GATE-001..004]` 的本地 deterministic-CBOR reference profile，为后续 canonical SpecEvent admission 提供不可由调用者伪造的 `CriterionId`、`SpecBodyDigest` 与 `HardCriteriaDigest` 重算入口。

## 2. 实现事实

- `nlos-semantic::spec` 定义并严格编码/解码 `IntentSpecBody`：goal、完整 acceptance、constraints、criticality、settlement 与 extensions 全部进入 canonical body identity。
- acceptance 按重算后的 `CriterionId = SHA-256("llmos/criterion/v1" || canonical_criterion)` 排序；重复 criterion、非规范顺序、trailing bytes 与非 deterministic 表示 fail-closed。
- `SpecBodyDigest = SHA-256("llmos/intent-spec-body/v1" || canonical_spec_body)`；任一 criterion/quorum/constraint/settlement/extension 变化都会改变 body identity。
- `HardCriteriaDigest` 只由规范排序后的完整 HARD criterion set 生成；`AUTOMATIC` 必须绑定非空且逐位相同的完整 hard set，`NONE` 禁止 hard digest/challenge window，`MANUAL` 如声明 hard digest 也必须精确匹配。
- pass/fail quorum 必须均大于零；HARD 的 MODEL/HUMAN criterion 必须同时声明 authority、independence、timeout 与 risk policy；capability allow/forbid 集必须有界、排序、唯一且互斥。
- 当前不支持任何 critical extension，因此出现 critical extension 明确拒绝；有界、排序、唯一的 noncritical extension 可 byte-exact round-trip。

## 3. 验证

```text
cargo test -p nlos-semantic
cargo clippy -p nlos-semantic --all-targets -- -D warnings
```

结果：Semantic 原 6 项 admission integration tests 与新增 5 项 canonical IntentSpec tests 全部通过；Clippy 零警告。

新增测试覆盖 body round-trip/顺序无关、identity 敏感性、AUTOMATIC 完整 hard set、quorum/HARD policy/critical extension、capability set 与 NONE settlement 约束。

## 4. 证据等级与限制

结论：`PARTIAL PASS / H3 local canonical identity`。

- 本切片只冻结 Stage B 的 digest-reference profile：`ResourceVector`、`ArtifactSelector`、authority/independence/risk policy 目前以不可变 digest 表示，尚未接各自 authority 解引用与语义验证。
- 尚未实现 SPEC 类型 SemanticEvent envelope、producer/store signature、SQLite 原子 admission、Spec body 投影或 v1→v2 migration；因此不能把本切片称为已接纳 SpecEvent。
- 尚未实现 CriterionEvaluationWindow、ResolvedArtifactSet、Verification/Gate 聚合或 Escrow settlement。

下一验收门：`B-SEMANTIC-002B` 将 canonical IntentSpec body 嵌入 SPEC event identity，并通过现有 Identity/Process/Capability/lineage/Receipt 事务门 durable admission。
