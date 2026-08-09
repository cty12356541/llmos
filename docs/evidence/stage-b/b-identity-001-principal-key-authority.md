# B-IDENTITY-001：Principal / ControlDomain / signing-key authority

> 状态：`PARTIAL_PASS`
>
> 日期：2026-08-09
>
> 范围：`LOCAL_SINGLE_NODE + APPLICATION_PLATFORM` 的 trusted-bootstrap reference slice

## 1. 验收目标

在 Semantic append 前建立不可由事件调用者自报的最小身份与密钥事实源，对齐 v0.5：

- `[ID-AUTH-001]`：authority 分配 Principal 与 ControlDomain identity，不接收自由字符串或 caller-supplied stable ID；
- `[ID-KEY-001]`：signing key 绑定 Principal、用途、有效期、generation 与撤销状态；
- `[ID-DOMAIN-001/002]`：ControlDomain 是名义身份，变更产生新 identity snapshot，历史 snapshot 不被当前状态覆盖；
- `[SEM-SIGN-001]`：真实验证 `H("llmos/semantic-signature/v1" || EventId)` 的签名；
- 为 `[SEM-TXN-002]` 第 3 门提供可直接调用的 identity/key/signature 验证前置，不在本工作项提前声明 Capability、scope 或完整 Semantic admission 已完成。

## 2. 实现事实

`crates/nlos-identity` 新增 SQLite schema v1 authority：

- `bootstrap_principal` 在一个 `BEGIN IMMEDIATE` 事务中创建 authority-assigned `PrincipalId`、单成员 `ControlDomainId`、`IdentitySnapshotId` 与 `KeyId`；调用者只提供 trusted-bootstrap profile/policy digest、公钥、用途、有效期和幂等键；
- `principals`、`identity_snapshots`、`snapshot_principals`、`key_versions`、`snapshot_key_bindings` 与 `key_revocations` 由 DDL trigger 禁止 UPDATE/DELETE；head 表只保存当前 fence；
- Ed25519 public key 绑定 `SemanticEventSigning` purpose、`valid_from/valid_until`、key generation 与 revocation state；authority 不接收或持久化 private key；
- `revoke_key` 同时 CAS 当前 key generation 与当前 identity snapshot，追加新 key version、snapshot 和 immutable revocation Receipt；旧 snapshot 仍可按精确 ID 回读；
- `verify_semantic_signature` 校验 Principal/ControlDomain/Key current binding、purpose、有效期、撤销状态，并以 Ed25519 strict verification 检查 domain-separated semantic message digest；
- 数据库启动要求 `journal_mode=WAL`、`synchronous=FULL`、foreign keys，未知 schema version fail-closed。

实现使用 `ed25519-dalek 3.0.0`；其公开 API 提供 32-byte verifying-key、64-byte signature 与 public-key verification，版本固定在 `Cargo.lock`。参考：[ed25519-dalek 3.0.0 文档](https://docs.rs/ed25519-dalek/3.0.0/ed25519_dalek/)。

## 3. 验证证据

`crates/nlos-identity/tests/identity_authority.rs` 的 5 项 integration tests：

1. bootstrap 原子创建、同 bytes replay、重启后相同 identity/readback；
2. 真实 Ed25519 semantic signature PASS，以及错误 issuer、签名、未生效、过期的 typed fail-closed；
3. key/snapshot 双 generation 撤销、Receipt replay、重启、旧 snapshot 历史回读、当前验签拒绝和 stale generation fence；
4. bootstrap idempotency rebinding 与无效 validity 拒绝；
5. snapshot、key version 与 revocation Receipt 的存储层不可变性。

本地验收命令：

```text
cargo test -p nlos-identity
cargo clippy -p nlos-identity --all-targets -- -D warnings
```

结果：5/5 integration tests PASS；crate Clippy PASS。

## 4. 证据等级与未覆盖范围

当前为单节点 SQLite 重启级 `H3 / PARTIAL_PASS`，只证明最小 identity/key authority 与实际公钥验签：

- `bootstrap_principal` 是可信本地 TCB API 边界；尚未接认证 session、受信启动证明、attestation 或外部 enrollment protocol；
- 未实现 private-key Keychain/HSM custody、签名服务、key rotation、多用途 key 或算法协商；
- ControlDomain 当前只有单成员 bootstrap 与 key-revocation snapshot；未实现多 Principal membership、merge/split/domain revoke 和 effective-time policy；
- admission time 由上层可信 Semantic service 传入，尚未接 AuthorityClock；未实现 policy-relative 的追溯撤销判断；
- 未实现 Capability issue/attenuate/delegate/revoke、Namespace scope 或 Semantic append；因此不得据此声称 `[SEM-TXN-002]` 已整体通过；
- 未执行 kill-9、ENOSPC、VFS/torn-write 或三平台 CI，不得外推为生产 Keychain、HA 或硬件掉电保证。

下一验收门：`B-CAPABILITY-001` durable issue/attenuate/revoke authority，随后再把 Identity + Key + Capability 接入 Semantic target/event admission。
