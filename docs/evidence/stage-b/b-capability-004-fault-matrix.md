# B-CAPABILITY-004：Kill-window 故障矩阵（最小前缀）

> 状态：`PARTIAL_PASS`
>
> 日期：2026-09-05
>
> 基线：HEAD `8f08c1f`；前序证据 `b-capability-001`（§4）、`b-capability-002`（§6）、`b-capability-003`（§4）

## 1. 验收目标

关闭 B-CAPABILITY 未决项「fault matrix」的最小前缀：在 `nlos-capability` 接入
`nlos-store-fault` VFS 故障注入 harness，覆盖 `issue_root`、`delegate` 与
`consume`（call-limit 账本）三条既有 durable 写路径，至少四类故障场景
（IOERR、ENOSPC、PowerLoss invisible、replay storm）。

不在本前缀范围内：kill-9 子进程可见方向、WAL 尾部截断穷举、revoke 写路径、
三平台 CI、硬件掉电实测。

## 2. Harness 约束

`CapabilityAuthority` 无 `open_with_vfs` 且 workspace `forbid(unsafe_code)`，
故与 channel/identity 矩阵相同：经 SQLite URI filename
（`file:<db>?vfs=<shim>&tail=`）将 authority 连接路由至 fault VFS；Identity
保持 plain VFS。`FAULT_LOCK` 进程级串行；`SandboxCwd` RAII 隔离 URI 路径
`create_dir_all` 副作用。

## 3. 验证证据

新增 `crates/nlos-capability/tests/capability_fault_injection.rs` 4 项 integration
tests（8 子场景）：

| 测试 | 写路径 | 故障类 | 断言要点 |
| --- | --- | --- | --- |
| `capability_precommit_ioerr_fails_typed_zero_phantom_converges` | issue_root + consume | IOERR | typed Sqlite 链含 ioerr；零幻影行；disarm 后同 key 收敛 |
| `capability_precommit_enospc_fails_typed_zero_phantom_converges` | issue_root + delegate | ENOSPC | typed 链含 full；前缀不变；redo byte-equal |
| `capability_power_loss_invisible_commit_converges_for_issue_and_consume` | issue_root + consume | PowerLossAfter | 幻影成功但 reopen 无行；redo 与 phantom receipt 相等 |
| `capability_replay_storm_is_byte_equal_idempotent` | issue_root + consume | replay storm | 3+ 次重放 + reopen 后仍 byte-equal；行数恰 1 |

行计数覆盖五表：`capability_heads`、`capability_versions`、
`capability_issue_receipts`、`capability_revocation_receipts`、
`capability_consumption_rows`；每场景 `PRAGMA integrity_check = ok`。

追加 `nlos-store-fault` dev-dependency（`Cargo.toml`）。

本地验收命令与结果（全部实跑）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p nlos-capability` | PASS：36 passed / 0 failed（fault 4 + hierarchy 5 + unit 2 + ledger 7 + authority 8 + signed 10） |
| `cargo clippy -p nlos-capability --all-targets -- -D warnings` | PASS |
| `cargo fmt -p nlos-capability -- --check` | PASS |

写集：`crates/nlos-capability/Cargo.toml`、
`crates/nlos-capability/tests/capability_fault_injection.rs`、本证据文件。

## 4. 已知限制

- **无 kill-9 / torn-WAL 可见方向**：PowerLoss 仅 invisible（静默丢写）模型；
- **revoke 写路径未覆盖**：最小前缀聚焦 issue/delegate/consume；
- **URI VFS 偏差**：raw reader / reopen 走 plain VFS，与 channel/identity 先例一致；
- 不得外推为跨平台或硬件级 durability 保证。

## 5. B-CAPABILITY 未决项状态增量

- 「fault matrix」：**最小前缀已关闭**（本证据 `PARTIAL_PASS`）；
- Namespace hierarchy narrowing：已由 `b-capability-003` 关闭（不变）；
- call-limit 消耗账本：已由 `b-capability-002` 关闭（不变）；
- 跨进程认证入口、AuthorityClock：仍开放。
