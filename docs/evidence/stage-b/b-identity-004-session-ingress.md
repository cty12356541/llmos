# B-IDENTITY-004：Trusted-local session ingress 最小前缀

> 状态：`PARTIAL_PASS`
>
> 日期：2026-09-05
>
> 范围：`LOCAL_SINGLE_NODE + APPLICATION_PLATFORM` 的 trusted-local software-only reference slice；在 `B-IDENTITY-001`/`002`/`003` 之上追加认证 session ingress 登记与只读回读

## 1. 验收目标

trusted-local session ingress 记录最小前缀：

- 新类型 `TrustedLocalSessionRecord` / `RegisterSessionRequest` / `SessionRegistrationDecision`；
- additive API `register_session(RegisterSessionRequest)`：按 current key generation fence 绑定 principal/control-domain/session token digest，immutable 落库并分配 `receipt_id`；
- 只读回读 `inspect_session(session_id)`；
- fail-closed：未知 session、stale generation fence、revoked key generation、idempotency 重绑、无效 validity window；
- 登记 HSM/跨机 session、HumanSession attestation、token 校验/crypto 未做。

## 2. 实现事实

`crates/nlos-identity` schema v4（自 v3 在线迁移）：

- 新增 `trusted_local_sessions` 表与 immutability trigger；`UNIQUE(session_id)` 保证每 session 至多一条 ingress receipt；
- `receipt_id` 由 authority 自 `session_id`/`key_id`/`generation`/`idempotency_key` 确定性派生；
- 仅存 `session_token_digest`（32 字节），不持久化 raw token material；
- `register_session`：`BEGIN IMMEDIATE` 内 idempotency 查重 → 已有 session 查重 → current binding generation CAS → revoked generation 拒绝 → INSERT immutable session row；
- `inspect_session`：按 `session_id` 回读后在 `key_versions.revoked_at_ms` 与 `key_revocations.expected_key_generation` 上复查 bound generation，revoked 则 `KeyRevoked`；
- 不修改 bootstrap/rotation/revocation/custody/验签判定链——session ingress 为并列权威记录，非 IPC 认证协议。

## 3. 验证证据

`crates/nlos-identity/tests/identity_authority.rs` 新增 3 项集成测试：

1. `session_registers_replays_inspects_and_survives_restart`：bootstrap 后 `SessionNotFound` → register/replay/inspect → 重启回读 → idempotency 重绑与 stale fence 拒绝；
2. `session_inspect_fails_closed_on_revoked_key_generation`：register 后 revoke bound generation → `inspect_session` → `KeyRevoked`；
3. `session_register_rejects_invalid_validity_window`：`registered_at_ms > expires_at_ms` → `InvalidKeyValidity`；
4. 既有 immutability 测试追加 `trusted_local_sessions` DELETE 拒绝。

本地验收命令：

```text
cargo test -p nlos-identity
cargo clippy -p nlos-identity --all-targets -- -D warnings
cargo fmt -p nlos-identity --check
```

## 4. 证据等级与未覆盖范围

当前为单节点 SQLite 重启级 `H3 / PARTIAL_PASS`，只证明 software-only session ingress 记录前缀：

- session ingress 是 trusted local TCB 元数据；未接 token 校验、HumanSession attestation、跨机 session federation 或 HSM 绑定的 session 密钥；
- 未实现 session 过期 enforcement（仅存 `expires_at_ms`）、session 撤销 receipt 或 rotation 后 session 自动 re-bind；
- 未执行 session register kill-window 故障矩阵；
- 三平台 CI 仍未完成。

关联：`b-identity-003-custody.md` §4 开放项「session」中 trusted-local session ingress 最小前缀由此 Evidence 关闭；HSM/跨机 session、attestation ingress 仍开放。
