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

## 5. runtime-tokio 消费接线（2026-08-29 增量，commit `3b005e6`）

- `TokioRuntimeAdapter::wait_for_channel`：durable 注册（nlos-wait）与内存等待在同一入口完成，gate 顺序与错误语义镜像 `wait_for_operation`；durable WOKEN/CANCELLED 立即解析（at-least-once）；PENDING 且 channel 高水位已满足 → fresh key `notify_commits` 自翻转后立即 Woken（显式 notify 模型，无轮询）；否则内存 Pending（同 key 取代者 Cancelled）。`TokioChannelWakeSink::deliver(report)`：commit 侧 notify 后非阻塞投递，Pending 消费发信号/Vacant 缓冲/重复幂等。purge 与 shutdown 覆盖新注册表；runtime 取消不清 durable 行（doc 契约钉住）。
- **commit + wakeup 契约就此闭环**（单机内）：channel enqueue（commit）→ `notify_commits`（durable 翻转）→ `deliver`（内存唤醒）→ fiber resume；崩溃后 durable 行由下次 notify/注册触发翻转，等待方重建（fiber rehydration）仍为 ADR-0008 登记后续。
- nlos-wait additive：`channel_high_water` helper；nlos-runtime-tokio 追加 nlos-wait 依赖与 nlos-channel dev-dependency。
- 验证：nlos-runtime-tokio 37 passed / 1 ignored（既有 scale probe；新增 channel_wait 9 项）；nlos-wait 26、nlos-channel 27 无回归；workspace clippy -D warnings 零警告；fmt 通过。
- 已知限制：channel_waits 为本 runtime 内存注册表（跨进程等待不做）；同 wait_id 多 fiber 共享注册为 degenerate 允许用法；自翻转 notify key 采用 WaitId 派生的 domain-reserved 变换（producer 不得使用，doc 注明）。

## 6. wait 侧 rehydration（2026-08-29 增量，commit `d3cb9a5`）

- 语义权威：[ADR-0008 补记](../../management/adrs/0008-durable-wait-registry-authority.md)：`rearm_channel_waits` 把重启后仍 PENDING 的 durable waits 重挂到新 fiber 内存等待——已满足者自 notify 翻转计 satisfied（future 立即 Woken）、未满足者注册 Pending 等后续 deliver；placeholder 早到缓冲被同 wait_id 重挂消费；同 key 二次 rearm 取代前者；durable 零副作用（自 notify 除外）。
- `WaitAuthority::list_waits(filter)`：全状态枚举 helper（占位缓冲只属 WOKEN 行，故不能只列 PENDING）。
- **边界**：fiber 执行状态重建（B-PROCESS 检查点域）与跨进程等待（blocked-by B-TASK-006L 真实 Capability/Principal 认证权威，未决）均不实现，登记于 ADR 复审触发器。
- 验证：runtime-tokio 45 passed / 1 ignored（新增 channel_rehydration 8 项）；nlos-wait 26 无回归；workspace clippy/fmt 全绿。

## 7. 跨进程等待传输前缀（2026-08-29 增量，commit `f5fed22`）

- 原 blocked-by B-TASK-006L 的「跨进程等待」按项目自身先例（SystemControl `handle_for_ipc`）解除到**本地信任域传输前缀**粒度：新 crate `nlos-wait-control` 将 WaitAuthority 五操作（register/notify/cancel/list/inspect）经 SABI envelope 暴露给独立进程，authorizer 注入沿用既有 posture；**跨 Principal 认证仍继承上游未决**（真实 Capability/Principal 模型未定，本前缀不解释 CapabilityHandle 字节）——分级解阻塞，非全量完成。
- 验证：nlos-wait-control 13 passed / 0 failed（含 tokio duplex 与真实 Unix socket roundtrip、失败映射 envelope 形状）；nlos-wait 26 无回归；workspace clippy/fmt 全绿。
- 已知限制：payload 为 crate 局部 prost 描述符（入 nlos-schema 共享 REGISTRY 为后续）；无 conformance server bin 与 TS/Python 客户端（对照 takeover-control 后续）。
