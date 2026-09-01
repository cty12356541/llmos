# B-SEMANTIC-006：Judgment/Verification/Retraction typed 事件 admission

## 1. 验收对象

本切片把 v0.5 §17.2 JudgmentEvent、§17.3 VerificationEvent、§17.4 RetractionEvent 接入与 Assertion/Spec（B-SEMANTIC-001/002）同构的 durable admission 路径：typed 请求、域分隔签名验签经 IdentityAuthority、durable 行 + 幂等回执、重启 replay。**最小子集**，超集不做。

## 2. 接管与续建说明

前代理（停滞超时后接管）遗留：`model.rs` 已有完整三事件 Unsigned 信封/枚举/请求/RetractionRecord 设计（保留）；`canonical.rs` 辅助函数已 `pub(crate)` 化（保留，供 typed 模块复用）。接管期间检测到同工作区并发写入者复活并自愈式推进（`typed.rs` 模块、schema v5、lib.rs admission core）；本车道与其做了显式合并：以 `typed.rs` 为唯一 canonical 实现（删除了本车道在 canonical.rs 的重复实现与重复 migrate_v5），保留本车道的 schema 接线（open() 迁移链、`SCHEMA_VERSION=5`）、`SemanticPayloadIdentity::Structural`、`TypedSemanticEvent` 公开判别视图、错误变体与补齐的 `[SEM-TTL-003]` 校验，并修复接管时的两处合并损伤。

## 3. 实现事实

- 事件类型编号沿用 §16 信封枚举序 ASSERTION=1、**JUDGMENT=2、VERIFICATION=3、RETRACTION=4**、SPEC=5；三事件复用 17-field deterministic-CBOR 信封，完整 payload 位于 field 15，`EventId` 覆盖全部 payload 事实（`SemanticPayloadIdentity::Structural`，无 detach digest 对象）。
- Judgment（§17.2 最小域）：`relation`(EQUIVALENT/CONTRADICTS/ENTAILS/SUPPORTS/REFINES)、`source`/`target` EventId、`context?`、`evaluator_evidence`(LocalReceiptRef→ReceiptId 必填)、`confidence_bp?`(≤10000)。`[SEM-JUDGE-003]`：对称关系 source/target MUST 按 EventId 字节序规范化，未规范化输入 typed 拒绝，保证同一判断唯一编码。
- Verification（§17.3 最小域）：target 为 tagged union（Event=tag1 / Criterion=tag2，恰好一个分支，空/混合不可表达）；Criterion 最小域含 spec_id/criterion_id/artifact_set_digest/procedure_digest/evaluation_id/producer_control_domains（1..=64、sorted-unique），**`settlement_binding` 因 Escrow hold authority 未落地而显式缺席**（拒绝该依赖，不接收不可验证绑定）。`outcome`(PASS/FAIL/INCONCLUSIVE/ERROR)、`evaluator_kind`(MODEL/DETERMINISTIC_TOOL/HUMAN)、`procedure_ref`(ImmutableArtifactRef/AuthorityPolicyRef)、`evaluator_evidence` 必填、`evidence:[EventId]` sorted-unique。
- Retraction（§17.4 最小域）：`target_event_id`、`mode`(WITHDRAW/INVALIDATE)、`reason?`、`authority_evidence` 必填；`[SEM-TTL-003]` 撤回声明任何 `valid_until` 在 encode/decode 两端 typed 拒绝。
- admission（`append_judgment`/`append_verification`/`append_retraction` → 共享 `append_typed_event` core）：单 SQLite IMMEDIATE 事务内依序 canonical decode→EventId 绑定→幂等 replay（bytes+signature 相同返回原 Receipt，不同 bytes typed collision）→Identity 域分隔 Ed25519 验签→Process generation fence→typed 引用门→Capability（Judgment/Verification 需 `SEMANTIC_APPEND`；WITHDRAW 需 `SEMANTIC_RETRACT` 且 `[SEM-RETRACT-001]` 本切片 issuer-only；INVALIDATE 需 `SEMANTIC_ADJUDICATE` 即 `[SEM-RETRACT-002]`，scope 必须与目标一致）→lineage/taint→行写入→signed DURABLE AdmissionReceipt+outbox。
- typed 引用门：Judgment source/target、Verification Event 目标与 Criterion spec_id 均必须为本 authority 已 commit 事件（typed `EventNotFound`）；Criterion 还要求 spec_id 为 SPEC 事件且 `criterion_id` 经 `criterion_id()` 验证属于该 spec body acceptance（typed `InvalidVerificationTarget`）。Retraction：目标不存在 `EventNotFound`；已撤回 `EventAlreadyRetracted`；第三方撤回他人事件 `RetractionSignerUnauthorized`（`[SEM-RETRACT-003]`）。
- schema v5：重建 `semantic_events` 放宽 CHECK 为 `event_type IN (1..5)` 且 2/3/4 行 content/spec digest 双 NULL（沿用 v1→v2 受控 rename 窗口 + `foreign_key_check`）；新增 append-only `event_retractions`（target_event_id 主键 → 一目标一撤回行，不可 UPDATE/DELETE trigger），既有 authority 行原样保留。v0/1/2/3/4 → 5 迁移链全路径接线。
- 撤回是 append-only 观察行：不删改目标事件行、不级联；`inspect_event_retraction` 提供事实读回。撤回 RetractionEvent 结构上不可能使原事件复活（`[SEM-RETRACT-004]`，行不可变）。

## 4. 验证

```text
cargo test -p nlos-semantic
cargo clippy -p nlos-semantic --all-targets -- -D warnings            # stable
cargo +nightly clippy -p nlos-semantic --all-targets -- -D warnings   # nightly
cargo fmt -p nlos-semantic --check                                    # stable
cargo +nightly fmt -p nlos-semantic --check                           # nightly
```

结果：**全部通过**（总计 22 passed / 0 failed）。test bins：semantic_authority 12 passed（含 v1→v5 无损迁移既有用例）、spec_canonical 5 passed、typed_events 4 passed（本切片新增：判断 admit/重启 replay/篡改签名/未注册 principal、Verification Event+Criterion 目标正路径与 未知 spec/非 SPEC/非成员 criterion 负路径、撤回全负路径矩阵 + WITHDRAW issuer 正路径 + INVALIDATE adjudicator 正路径 + restart replay + 撤回事实读回、canonical round-trip 与对称规范化）、lib 内 schema 单元 1 passed；doc-tests 0。Clippy 双工具链零警告，fmt 双工具链 clean。

## 5. 证据等级与已知限制

结论：`PARTIAL_PASS / H3 local durable authority（最小子集）`（与 B-SEMANTIC-001/002B 评级基线一致）。

- WITHDRAW 的"预先委托主体"（`[SEM-RETRACT-001]` 后半）未建模，本切片严格 issuer-only；INVALIDATE 的 adjudication capability 仅校验 scope+right，adjudication policy 解引用未做。
- Criterion target 的 `settlement_binding` 缺席属依赖裁剪（Escrow hold authority 未落地），`[SEM-VERIFY-004]` 的 Gate 侧 settlement 校验随之不在本切片；`[SEM-VERIFY-005]` 独立性计算属 Gate/Trust View，未实现（本切片只保证 producer_control_domains 完整进入 canonical bytes）。
- 撤回 RetractionEvent 自身被撤回在 admission 层未单独禁止；因撤回账本 append-only 且非可见性过滤器，结构上不产生复活效果。
- Trust View/Authority-aware View、UNVERIFIED 派生态、CRDT ConflictReceipt（`[SEM-JUDGE-002]` 侧）均不在本切片。
- EvidenceRef 目前仅实现 LocalReceiptRef 最小分支；ExternalAttestation/HumanSession 分支未实现（与 B-SEMANTIC-001 时点一致）。

## 6. 未运行项

- 未运行 workspace 级测试/lint（任务禁 `--workspace`）；下游 crate（task/coordinator 等）对新增 API 的联动编译未验证。
- 故障注入 VFS matrix、并发多写者压测未在本切片重复（复用 B-SEMANTIC-001/002 既有覆盖思路，未新增）。
