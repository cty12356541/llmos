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
- ~~未执行 kill-9、ENOSPC、VFS/torn-write 或三平台 CI~~ **kill-window 故障矩阵已追加（2026-09-05，lane W13-I）**；三平台 CI 仍未完成。

关联：`b-identity-001-principal-key-authority.md` §5 开放项「key rotation 入口」由此 Evidence 关闭最小前缀；HSM/custody/session 仍开放。

## 5. Signing-key rotation kill-window 故障矩阵（2026-09-05 增量，lane W13-I）

`crates/nlos-identity/tests/identity_rotation_fault_injection.rs`：6 项测试（5 主动场景 + crash 子进程 helper），镜像 `nlos-channel`/`nlos-wait` kill-window harness（kill-9 子进程 READY 管道同步、`FAULT_LOCK` 串行化、URI 路由 fault VFS、WAL tail 截断、typed 错误链断言、raw 行计数、逐场景 `integrity_check`）。追加 `nlos-store-fault` dev-dependency。

| 窗口 | 测试 | 断言摘要 | 状态 |
|---|---|---|---|
| W1 pre-commit IOERR | `identity_fault_rotate_precommit_ioerr_fails_typed_and_converges` | `FailWritesAfter{0,IoErr}` → typed `Sqlite`（链含 i/o/ioerr）；`key_rotations=0`、bootstrap 前缀保持；disarm 后 redo → `Rotated` gen2 | PASS |
| W2 pre-commit ENOSPC | `identity_fault_rotate_precommit_enospc_fails_typed_and_converges` | `FailWritesAfter{0,Full}` → 链含 full；零幻影 rotation 行；redo 收敛 | PASS |
| W3 commit 点 PowerLossAfter | `identity_fault_rotate_power_loss_commit_point_converges_both_ways` | Phase A（`PowerLossAfter{0}`）：head 仍 gen1、零 rotation 行、redo byte-equal 幻影 receipt；Phase B（kill-9 after commit）：rotation 全可见、同 key `Replayed` byte-equal | PASS |
| W4 torn WAL tail | `identity_fault_rotate_torn_wal_tail_discards_and_redo_converges` | 最后 rotation commit 帧半截断 → head 回 bootstrap、无半套 rotation；redo/replay byte-equal | PASS |
| W5 replay storm | `identity_fault_rotate_replay_storm_is_idempotent` | 同请求 3+ 次 + 重开后 1 次 → 全 `Replayed` byte-equal；`key_versions` 恒 `[1,2]`、同 key 不双 rotate | PASS |

本地验收（2026-09-05）：

```text
cargo test -p nlos-identity          # 20 passed
cargo clippy -p nlos-identity --all-targets -- -D warnings
cargo fmt -p nlos-identity --check
```

证据等级仍为 `H3 / PARTIAL_PASS`：VFS/process-crash 为模型化注入，非真实掉电或三平台 CI。
