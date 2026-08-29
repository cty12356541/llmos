# B-CAPABILITY-001：Durable Capability attenuation authority

> 状态：`PARTIAL_PASS`
>
> 日期：2026-08-09
>
> 范围：`LOCAL_SINGLE_NODE + APPLICATION_PLATFORM` 的 Semantic admission 前置 reference monitor

## 1. 验收目标

在 Semantic store 接受事件前建立可机械验证、不可由 caller-supplied ID 冒充的最小 Capability authority，对齐 v0.5：

- `[CAP-UNFORGE-001]`：Capability 是 authority-owned record，并绑定 target object/scope、rights、generation、issuer、holder 与 validity；
- `[CAP-ATTEN-001]`：delegate 只能衰减 rights、scope、purpose、validity、call limit 与再委托深度；
- `[CAP-REVOKE-001]`：generation advance 使旧 handle 及绑定旧 parent generation 的全部 descendants 失效；
- `[CAP-DELEGATE-001]`：issue/delegate 产生 immutable Receipt，记录 parent、holder 与衰减结果；
- 为 `[SEM-TXN-002]` 第 3 门提供接受真实 verified signer 的 target/right/purpose authorization，不提前声明完整 Namespace 或 Semantic authority 已完成。

## 2. 实现事实

`crates/nlos-capability` 新增 SQLite schema v1 authority：

- trusted root issue 通过 `nlos-identity` 当前 key binding 解析 issuer/holder Principal 与 ControlDomain，再由 authority 派生 `CapabilityId`；稳定身份不是调用者提交字段；
- Capability record 绑定 `NamespaceId | TaskId` target、closed rights bitset、可选 purpose digest、validity、可选 call-limit、delegation depth、issuer/holder 与 parent handle；
- delegate 必须由 parent holder 发起，parent 具有 `DELEGATE` right，且 child rights 为子集、target 精确相同、purpose 不放宽、validity 不扩张、call-limit 不增加、remaining depth 严格下降；
- root issue 和 delegate 均在一个事务写 authority record、generation v1 与 immutable issue/delegation Receipt；相同 idempotency key + 相同 request digest 跨重启 replay 原 record/Receipt，异 bytes fail-closed；
- revoke 只允许 issuer 或 holder，经 current generation CAS 追加 revoked generation 与 immutable revocation Receipt；child 保存 parent generation，使用时遍历完整祖先链，祖先换代立即围栏 descendants；
- Semantic authorize 接口要求 `VerifiedSemanticSigner`。该类型字段已收为 crate-private，只能由 `IdentityAuthority::verify_semantic_signature` 成功后产生；reference monitor 再验证 holder/ControlDomain、target、right、purpose、time 与 ancestor chain；
- authority 要求 SQLite `WAL + FULL + foreign_keys`，Capability descriptor、version 和 Receipt 由 DDL trigger 防 UPDATE/DELETE。

## 3. 验证证据

`crates/nlos-capability/tests/capability_authority.rs` 的 6 项 integration tests：

1. root issue 的 authority identity binding、exact replay、restart 与 active readback；
2. 合法 delegate 以及 rights/scope/validity/call-limit/depth amplification 的独立拒绝；
3. 真实 Ed25519 verified signer 的 Semantic authorization，以及 holder/right/purpose 反例；
4. issuer/holder revocation、Receipt replay、generation fence；
5. parent revoke 后 descendant 跨重启 fail-closed；
6. Capability descriptor/version/issue Receipt 的 DDL immutability。

本地验收命令：

```text
cargo test -p nlos-capability
cargo clippy -p nlos-capability --all-targets -- -D warnings
```

结果：6/6 integration tests PASS；crate Clippy PASS。

## 4. 证据等级与未覆盖范围

当前为单节点 SQLite 重启级 `H3 / PARTIAL_PASS`：

- root issue/delegate/revoke 是可信 TCB API；尚未接 IPC peer authentication、签名命令或 bounded rejection Receipt；
- target narrowing 目前只接受 exact `NamespaceId | TaskId`，尚无 Namespace hierarchy authority 来证明子 scope；
- call-limit 只证明委托值不增加；未实现消费计数、并发 reservation 或 Ledger credit，不能把它当实际配额执行证据；
- rights registry 只覆盖 Semantic append/retract/adjudicate 与 delegate，不是通用 NLOS object/operation registry；
- validity 使用上层传入的 authority time，未接可信 AuthorityClock；
- 未执行 kill-9、ENOSPC、VFS/torn-write 或三平台 CI；不得外推为分布式 Capability、生产 MAC/ACL 或硬件掉电保证。

下一验收门：将 `B-IDENTITY-001 + B-CAPABILITY-001` 接入 `B-SEMANTIC-001` 的 canonical EventId、signature、lineage、atomic durable AdmissionReceipt。

## 5. 签名命令（2026-08-29 增量，commit `2ccc694`，ADR-0010）

- §4 开放项「root issue/delegate/revoke 是可信 TCB API」部分关闭：三命令新增签名变体（行为方 principal 公钥验签，域分隔命令消息，require_signed_by 交叉校验防归属攻击），无签名入口弃用保留；replay 不重验签。
- signer 持久化于既有 issuer/revoker principal id 列（零 schema 迁移）；nlos-identity additive `verify_capability_command_signature`。
- 验证：capability 18 / identity 11 / semantic 18 全绿；workspace clippy/fmt 全绿。
- 其余开放项不变：call-limit 消费计数、Namespace hierarchy、AuthorityClock、IPC peer auth（blocked-by B-TASK-006L）、故障矩阵。
