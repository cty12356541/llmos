# B-WAIT-001：Durable wait registry（commit + wakeup 前缀）

- 状态：`PARTIAL_PASS`（单机 SQLite 重启级 `H3`）
- 日期：2026-08-29
- Owner：`WaitAuthority`（`crates/nlos-wait`，SQLite schema v1）
- 设计依据：[ADR-0008](../../management/adrs/0008-durable-wait-registry-authority.md)（用户 2026-08-29 选择独立 crate 归属 + 显式幂等 notify 触发）
- 关联工作包：`B-WAIT-001`（B-PROCESS 家族前缀切片）；`B-CHANNEL-001`（commit 锚点）

## 1. 实现事实

- **归属与分层**：独立 durable wait authority（自有 `wait-authority.db`，WAL+FULL 硬校验、STRICT、状态机 trigger 守卫、单写者）；`nlos-process`/`nlos-outbox` 语义边界保持不动；`TokioWakeSink` 保持内存侧，其消费 wait wake 的泛化接线为已登记后续（owner-side first）。
- **register_wait**：binding（16B opaque，仅拒全零）+ channel_id + target_sequence；经 `ChannelAuthority::inspect_channel` owner 回读绑定 generation/fence 快照；`WaitId = SHA-256("nlos/wait/id/v1" ‖ binding ‖ channel ‖ target(be) ‖ key)`；门序全部先于写入（target=0 `InvalidSequence`、零 binding `InvalidBinding`、漂移 `IdempotencyConflict`）。
- **notify_commits**：单 Immediate 事务批量 CAS `PENDING→WOKEN`（`target <= up_to`，快照 woken_at_ms/woken_up_to）；notify 回执行（含 woken id 列表）与翻转同生同灭，同 key 重放精确还原原 report 不重复翻转；空集合法；`up_to=0` pre-write 拒绝。
- **cancel_wait**（身份 = WaitId）：`PENDING→CANCELLED` CAS + 回执重放；对 WOKEN/CANCELLED `WaitNotPending` fail-closed。
- **不可变性**：`binding_digest` trigger 冻结 + 读时重derive（篡改必现 `CorruptRecord`）；非法状态翻转与全部 DELETE 被 trigger abort。
- **重启 replay**：全行逐字段相等；PENDING 行重启后仍可被 notify 唤醒。
- **语义事实**（矩阵登记，非缺陷）：register 同 key 重放返回当前 durable 行（翻转后携带现态，注册身份字段冻结）；fresh-key 空唤醒 notify 亦持久化自己的回执（回执数按 distinct key 增长）。

## 2. 验证

```text
cargo test -p nlos-wait（7acded5 后）
  → wait_registry 13 passed；wait_fault_injection 13 passed（12 场景 + helper）
  → 合计 26 passed / 0 failed

cargo clippy --workspace --all-targets -- -D warnings → 0 warning / 0 error
cargo fmt --all --check → 通过
```

kill-window 矩阵：三入口 pre-commit IOERR/ENOSPC typed fail-closed 零幻影行；register PowerLoss 双向；**notify 跨权威窗口双向证明**（CAS 翻转与回执同生同灭、无部分翻转、kill-9 同 key 重放原 report）；register/notify 各 ≥6 torn WAL 截断点（可见行恒控制组前缀）；replay storm 零重复；cancel CAS 窗口无中间态；注入恢复后 trigger 守卫仍有效。

## 3. Canonical commits

- `401c817` docs: accept durable wait registry ADR
- `04839cc` feat: add durable wait registry authority
- `7acded5` test: cover wait authority kill-window fault matrix

## 4. 明确未完成（PARTIAL_PASS 保持）

- runtime-tokio 接线（WakeSink 消费 wait wake、`wait_for` 泛化）与 fiber rehydration（等待方崩溃后重建等待）为 ADR-0008 登记后续；
- binding 为 opaque，本前缀不解释 fiber/process 绑定结构；无 per-channel notify 水位摘要（乱序/回退 notify 安全但无「已通知至 X」聚合）；
- 跨进程等待、真实掉电（kill-9 页缓存存活、模型化丢写）、CI/部署均未决。
