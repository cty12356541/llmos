# B-IDENTITY-002：Signing-key rotation 最小前缀

> 状态：`PARTIAL_PASS`
>
> 日期：2026-09-04
>
> 范围：`LOCAL_SINGLE_NODE + APPLICATION_PLATFORM` 的 trusted-bootstrap reference slice；在 `B-IDENTITY-001` revoke 之上追加 rotation 入口

## 1. 验收目标

与 revocation 区分的最小 key rotation 前缀：

- 新 API `rotate_key(RotateKeyRequest)`：携带新 Ed25519 公钥/validity、expected key/snapshot fences、idempotency；单事务 bump `key_generation`、追加 immutable rotation receipt、更新 current binding（镜像 revoke 的 snapshot CAS 纪律）；
- rotation 后旧 generation 公钥签名的 semantic event 在 `verify_semantic_signature` fail-closed（当前 binding 公钥验签 → `InvalidSignature`）；
- 幂等 replay 与 typed generation/snapshot fence 冲突；
- 登记 HSM/custody/session 未做。

## 2. 实现事实

`crates/nlos-identity` schema v2（自 v1 在线迁移）：

- 新增 `key_rotations` 表与 immutability trigger；`identity_snapshots.change_kind` 扩展为 `(1 bootstrap, 2 revocation, 3 rotation)`；
- `RotateKeyRequest` / `KeyRotationReceipt` / `KeyRotationDecision`（`Rotated` / `Replayed`）；
- `rotate_key`：`BEGIN IMMEDIATE` 事务内 idempotency 查重 → 双 fence CAS → 插入新 `key_versions`（新公钥/validity、未撤销）→ 新 immutable snapshot（`CHANGE_KEY_ROTATION=3`）→ 更新 `key_heads` 与 `control_domains` head → 写入 rotation receipt；
- 与 revoke 共用 generation 推进与历史 snapshot 可读性；revoked key 旋转拒绝为 `KeyRevoked`；
- `verify_semantic_signature` 判定链未改：始终解析 current binding 公钥，rotation 后旧 material 自然 fail-closed。

## 3. 验证证据

`crates/nlos-identity/tests/identity_authority.rs` 新增 1 项集成测试 `rotation_advances_both_fences_rejects_old_signatures_and_survives_restart`：

1. bootstrap → rotate 双 fence 推进、receipt 字段、幂等 replay；
2. 重启后 replay 与 historical/current binding 回读；
3. 旧公钥 semantic 签名 → `InvalidSignature`；新公钥 → PASS 且 `key_generation=2`；
4. stale generation fence → `KeyGenerationFenceConflict`；
5. 既有 immutability 测试追加 `key_rotations` DELETE 拒绝。

本地验收命令：

```text
cargo test -p nlos-identity
cargo clippy -p nlos-identity --all-targets -- -D warnings
cargo fmt -- -p nlos-identity --check
```

## 4. 证据等级与未覆盖范围

当前为单节点 SQLite 重启级 `H3 / PARTIAL_PASS`，只证明最小 rotation 入口与旧 material 验签拒绝：

- rotation 是 trusted local TCB API；未接认证 session、attestation、外部 enrollment 或 HSM/Keychain private-key custody；
- 未实现 rotation 授权签名、多用途 key 协商、grace-period 双签或 algorithm rollover；
- 未实现 post-rotation capability-command 旧 key 矩阵（capability 侧按 principal 解析 current binding，行为与 semantic 一致，未在本 Evidence 单独扩测）；
- 未执行 kill-9、ENOSPC、VFS/torn-write 或三平台 CI。

关联：`b-identity-001-principal-key-authority.md` §5 开放项「key rotation 入口」由此 Evidence 关闭最小前缀；HSM/custody/session 仍开放。
