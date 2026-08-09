# B-SEMANTIC-001：Durable Assertion admission authority

> 状态：`PARTIAL_PASS`
>
> 日期：2026-08-09
>
> 范围：`LOCAL_SINGLE_NODE + APPLICATION_PLATFORM` 的 AssertionEvent admission vertical slice

## 1. 验收目标

把 strict authority-first 的 Identity/Key/Capability 前置链接入第一个真实 Semantic append：

- `[SEM-ID-001/002/003]`：ContentDigest 与 EventId 分离，EventId 覆盖完整 unsigned envelope；
- `[SEM-CANON-001]`：unsigned Assertion 使用 strict deterministic CBOR，拒绝 indefinite/trailing/noncanonical 表示；
- `[SEM-IDEM-001]`：exact canonical event replay 原 AdmissionReceipt，冲突 bytes fail-closed；
- `[SEM-SIGN-001]`：issuer 实签 `SHA-256("llmos/semantic-signature/v1" || EventId)`；
- `[SEM-LINEAGE-001]`：只接受已提交父/捕获输入，拒绝 dangling/self，当前单事件事务天然禁止未来边与环；
- `[SEM-APPEND-001/SEM-TXN-002]`：content/event/signature/lineage/log/AdmissionReceipt/outbox 原子 append-only；
- `[SEM-DURABLE-001]`：本切片只签发直接跨过 WAL/FULL barrier 的 DURABLE AdmissionReceipt，不原地升级 BUFFERED Receipt。

## 2. 实现事实

`crates/nlos-semantic` 新增 SQLite schema v1 authority：

- v1 unsigned Assertion envelope 固定 17 个有序字段，包含 schema/event type、`NamespaceId | TaskId` scope、issuer、LocalProcessRef、ControlDomain、Unix-ns producer time、16–32 byte nonce、sorted unique parents、validity、purpose、完整 Assertion payload 与 KeyId；decode 后必须逐字节 re-encode 相等；
- `ContentDigest = SHA-256("llmos/content/v1" || deterministic_cbor([media_type, exact_bytes]))`，`EventId = SHA-256("llmos/semantic-event/v1" || canonical_unsigned_event)`；content bytes 和 event payload digest 在 admission 前逐位复核；
- admission 先用 `B-IDENTITY-001` 实际验签并检查 key validity/revocation，再读取 `B-PROCESS-001` current Process generation，再由 `B-CAPABILITY-001` 检查不可外部构造 signer proof、holder ControlDomain、scope、append right、purpose、validity 与完整 ancestor fence；
- declared parents 与 captured inputs 必须 sorted/unique 且已有 DURABLE AdmissionReceipt；effective taint 是 ingress + parent + captured taint 的单调并集；
- effective validity 取 declared validity、key validity、Capability validity 与 admission limit 的最小值；已过期拒绝；
- content object、semantic event、event signature、gapless local event log、两类 lineage edge、signed DURABLE AdmissionReceipt 和 semantic outbox 在同一个 `BEGIN IMMEDIATE` 事务提交；任一 store signing/验签失败全部回滚；
- AdmissionReceipt core 绑定 EventId、log sequence、admission/effective validity、captured inputs、taint、authz policy、DURABLE 与 store Principal/ControlDomain/Key；store signature 使用独立 domain，随后由 Identity authority 再验证；
- committed exact event 的 replay 在当前 key/Capability 后续 revoke 后仍返回原 Receipt；这不重新授权新事件，也不改写历史事实。

## 3. 验证证据

`crates/nlos-semantic/tests/semantic_authority.rs` 的 6 项 integration tests：

1. deterministic CBOR round-trip、stable EventId、trailing bytes、nonce 与 FACT_FROM_TOOL evidence 反例；
2. WAL/FULL durable append、actual issuer/store signatures、exact replay、restart、readback、effective validity/taint；
3. issuer signature、store signer 与 stale Process generation 的跨 authority失败均不留下半事件；
4. dangling lineage 拒绝，已提交 declared/captured parent 的 log ordering 与 taint inheritance；
5. committed event 在 Capability revoke 后 replay，新 Event 被 generation fence；
6. event/signature/AdmissionReceipt 的 DDL append-only 与 durable outbox 同事务存在。

本地验收命令：

```text
cargo test -p nlos-semantic
cargo clippy -p nlos-semantic --all-targets -- -D warnings
```

结果：6/6 integration tests PASS；crate Clippy PASS。

## 4. 证据等级与未覆盖范围

当前为单节点 SQLite 重启级 `H3 / PARTIAL_PASS`，只覆盖 Assertion admission：

- Judgment、Verification、Retraction 与 SpecEvent payload/canonical rules 尚未实现；因此还不能为 TaskWriteSet 提供真实 IntentSpec/SpecEvent binding；
- 单事件事务只接受已提交 parent，尚未实现同批 earlier-event DAG append；
- `declassification_receipt_id` 在 v1 Assertion slice 固定为 null；未实现 declassification authority、captured-input trap 或 private-context 自动检测；captured inputs 由可信 admission caller 传入；
- Process generation 已验证，但 `B-PROCESS-001` 尚未把 Process 绑定 Principal；FACT_FROM_TOOL 只要求 evidence Receipt ID 存在于签名 payload，尚未验证 Driver execution Evidence；
- store signer 是 Keychain/HSM adapter trait，测试使用内存 Ed25519 key；未实现生产 Keychain custody；
- scope policy 只有 caller 提供的 admission limit，未实现 Namespace policy authority；
- 未实现 TrustPolicy/View/checkpoint、Retraction 视图、GC/crypto-shredding、batch、fuzz、fault VFS、kill-9/ENOSPC 或三平台 CI。

下一验收门：`B-SEMANTIC-002` canonical SpecEvent admission，然后建立 Task participant registry 并把真实 Semantic target/event Receipt 绑定进完整 TaskWriteSet。
