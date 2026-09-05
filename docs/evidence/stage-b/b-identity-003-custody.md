# B-IDENTITY-003：Private-key custody 最小前缀

> 状态：`PARTIAL_PASS`
>
> 日期：2026-09-05
>
> 范围：`LOCAL_SINGLE_NODE + APPLICATION_PLATFORM` 的 trusted-local software-only reference slice；在 `B-IDENTITY-001`/`002` 之上追加 key generation ↔ custody domain 绑定

## 1. 验收目标

trusted-local private-key custody 记录最小前缀：

- 新类型 `KeyCustodyRecord` / `CustodyProfile::TrustedLocalSoftware`（software-only reference profile）；
- additive API `register_custody_binding(RegisterCustodyBindingRequest)`：按 key generation fence 绑定 principal/control-domain/custody profile，immutable 落库；
- 只读回读 `inspect_custody(key_id, generation)` 与 `inspect_current_custody(key_id)`；
- fail-closed：未知 key、stale generation fence、缺失 binding、idempotency 重绑；
- 登记 HSM/Keychain 真实集成、签名服务、session 未做。

## 2. 实现事实

`crates/nlos-identity` schema v3（自 v2 在线迁移）：

- 新增 `key_custody_bindings` 表与 immutability trigger；`UNIQUE(key_id, key_generation)` 保证每代 key 至多一条 custody 记录；
- `CustodyProfile` 仅 admit `TrustedLocalSoftware=1`；DDL CHECK 与 Rust decode 双 fail-closed；
- `register_custody_binding`：`BEGIN IMMEDIATE` 内 idempotency 查重 → 已有 generation 绑定查重 → current binding generation CAS → 从 binding 复制 principal/control-domain → INSERT immutable custody row；
- rotation 后新 generation 需单独 register；旧 generation custody 仍可按 `(key_id, generation)` 精确回读；
- 不修改 bootstrap/rotation/revocation/验签判定链——custody 为并列权威记录，非 HSM adapter。

## 3. 验证证据

`crates/nlos-identity/tests/identity_authority.rs` 新增 1 项集成测试 `custody_binding_registers_per_generation_replays_and_survives_restart`：

1. bootstrap 后 `inspect_custody` → `CustodyBindingNotFound`；
2. register gen1 → `Registered`、字段与 binding 一致、幂等 `Replayed`、`inspect_current_custody` 回读；
3. rotate 至 gen2 后 current custody 缺失；重启后 gen1 历史 custody 可读；
4. register gen2 → current custody 可见；
5. 同 generation 不同 idempotency/registered_at → `IdempotencyConflict`；stale generation fence → `KeyGenerationFenceConflict`；
6. 既有 immutability 测试追加 `key_custody_bindings` DELETE 拒绝。

本地验收命令：

```text
cargo test -p nlos-identity
cargo clippy -p nlos-identity --all-targets -- -D warnings
cargo fmt -p nlos-identity --check
```

## 4. 证据等级与未覆盖范围

当前为单节点 SQLite 重启级 `H3 / PARTIAL_PASS`，只证明 software-only custody 记录前缀：

- custody 是 trusted local TCB 元数据；未接 Keychain/HSM/Secure Enclave、私钥 material 存储或签名委托；
- 未实现 custody 变更 receipt、跨 generation 自动 propagate 或 attestation 绑定的 custody upgrade；
- 未执行 custody register kill-window 故障矩阵（rotation 矩阵已独立覆盖 store 纪律，custody 单行 INSERT 未单独扩测）；
- 三平台 CI 仍未完成。

关联：`b-identity-002-key-rotation.md` §4 开放项「HSM/custody/session」中 custody 最小前缀由此 Evidence 关闭；HSM/Keychain/session 仍开放。
