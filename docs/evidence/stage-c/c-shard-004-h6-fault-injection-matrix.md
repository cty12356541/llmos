# C-SHARD-004 / W55-CS0D：H6 六类目故障注入矩阵（跨 Cell 接管域）

- 状态：`PASS`（ADR-0019 修订 2 R2.2 第 3 条六类目子集，对 C-SHARD-001 驱动器与 C-SHARD-003 收敛核注入；**单机双进程域**——非跨机/真网络分区/跨时钟域）
- 日期：2026-10-08
- Owner：`C-SHARD` 计划 Phase 0（[c-shard-plan.md](../../management/c-shard-plan.md) CS0-D 车道；attempt-2 收尾——attempt-1 因会话中断留未提交候选，沿革在 lane commit message）
- 设计依据：[ADR-0019](../../management/adrs/0019-cross-cell-commit-semantics-extension.md) 修订 2 R2.2 第 3 条（类目与方法学硬约束：分区/双主须双存活注入，kill 只证崩溃处理）+ R1.2.4（不抢先 + QUARANTINE + 有界收敛）+ `DIST-TASK-002` 尾句（旧 Cell 六类动作拒绝）
- 分支：`feat/cs0d-h6-fault-matrix`；lane commit `ed23ac7b6`；base `70aff0f8d`；合入 main `3dc6c391f`（W55-CS0D）

## 1. 实现事实

- 8 测试文件 4639 行（`tests/common/mod.rs` 通道与场景基建 + 七类目文件），**src/ 与 Cargo.toml 零改动**——注入纯走 `BarrierObservationSource` 公开测试可见缝（对 CS0-A 驱动器的既有 trait 缝组合故障：拒答/挂起/重放）。
- **R7 注入通道**（先行落成并单独测试）：harness 通道层投递拒绝（掐通道≠进程死）；通道文件信号位 + child 拒答开关；`wait_heartbeat_at_least` 内含 try_wait 存活检查——双存活可证明。诚实声明：通道层投递拒绝，非 OS 级分区（测试文档自 declare）。

## 2. 六类目 → 具名测试与断言语义（全部退出码 0，各 1 passed / 1 ignored=child）

| 类目 | 具名测试 | 断言语义 |
|---|---|---|
| 通道（R7） | `cut_channel_blocks_delivery_while_both_cells_stay_alive` | 掐通道期间两进程存活可证明 |
| 分区 | `partition_cut_blocks_takeover_until_channel_recovers` | 双存活掐通道：接管不能凭缺口完成（**不抢先**），通道恢复后可完成 |
| 重启 | `restart_continues_from_durable_prefix_and_converges` | kill-9 mid-chain → durable prefix 续走 → 收敛核四不变量全真 |
| 迁移（mid-fault） | `migration_with_mid_chain_fault_reaches_the_fault_free_terminal_state` | mid-chain 故障后终态与无故障迁移一致 |
| 重复消息 | `duplicate_message_replays_are_idempotent` | barrier/receipt 重放无双重提交 |
| stale epoch | `stale_epoch_submissions_are_all_refused` | 拒绝三形：`Err(AuthorityLeaseFenced)`/`Err(ParticipantRegistryFrozen)`/`Ok(Superseded{winner})`；第二面=无 outstanding permit 的新任务以旧 lease 请求必须 `Err(AuthorityLeaseFenced)`（Superseded 契约先证：model.rs:1325「请求 attempt 被 durable fence 为终态」+ compete CAS store.rs:3926 先于 issue_permit 内 lease 门 :4146——非弱化，反而补强） |
| 分区双主 | `dual_primary_partition_arbitrates_a_single_winner` | 双存活分区窗口双活声称 → lease/fencing 仲裁单胜者；输家提交全拒；不抢先（`AuthorityLeaseHeld` + 零 durable 效果） |

方法学硬约束核验：kill 仅 `fault_restart.rs`（15 处）；`migration_midfault` 为双存活瞬态通道故障（零 kill，文件头自声明 both-alive）；partition/dual_primary 每个 cut 窗口有通道外心跳前进断言 + 显式存活断言 + demand 计数为 0。旧 Cell 六类动作（result/message/permit/callback/effect/commit）中本场景可注入子集：fence/permit/effect/result/commit（stale 类目逐条注释对应；message/callback 无本 slice 注入面，未声明——CS1-C）。

## 3. 验证（2026-10-08，macOS arm64 darwin 25.6.0）

```text
cargo test -p nlos-slice-k（含七类目文件全套）        → 退出码 0
cargo fmt --check                                      → 退出码 0
cargo clippy -p nlos-slice-k --all-targets             → 退出码 0 零警告
integrator 全仓（merge 后）：fmt=0 / clippy --workspace --all-targets=0 / test --workspace 1922/0/38（348 二进制）
```

（全仓数字含全部 Phase 0 证据测试；`EVID-SCOPE-001`：本文件 claim 范围=六类目注入语义，与 C-SHARD-001/002/003 各自单列。）

## 4. 边界与限制（诚实域）

- **单机双进程域**（ADR-0019 修订 2 R2.2 第 1 条）：注入「掐通道」是 harness 通道层投递拒绝，非 OS 级网络分区、非跨机、非跨时钟域——引证为跨机证据即违 RISK-B-11。
- endpoint 覆盖仅声明 TaskStore 参与端；物理 cleanup 残域按 R2.3 第 2 条保留（拒绝/QUARANTINE ≠ cleanup 证明）。
- H6 其余三类目（fanout storm/provider stall/bulkhead）属 C-FANOUT/C-BENCH，不在 C-SHARD 范围。
- 时序类断言用 store 锚定时钟/短 TTL 注入，无 sleep 拖长。

## 5. 复现

```text
cargo test -p nlos-slice-k --test dual_cell_takeover_fault_channel --test dual_cell_takeover_fault_partition \
  --test dual_cell_takeover_fault_restart --test dual_cell_takeover_fault_migration_midfault \
  --test dual_cell_takeover_fault_duplicate_messages --test dual_cell_takeover_fault_stale_epoch \
  --test dual_cell_takeover_fault_dual_primary -- --nocapture
```
