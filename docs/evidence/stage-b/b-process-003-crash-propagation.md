# B-PROCESS-003：process crash propagation / terminal lifecycle 最小前缀

- 状态：`PARTIAL_PASS`（ROAD-B-006 process crash propagation 合同层最小前缀；runtime 侧 join/cancel 联动、平台 kill adapter、Activation meter 未做）
- 日期：2026-09-05
- 设计依据：v0.5 §28.2 ROAD-B-006「Process crash propagation」；`[FIBER-FAIL-001]` Fiber 共享 Process 故障域
- 关联：`B-PROCESS-001` durable binding authority；W12-P 波次 13 车道

## 1. 实现事实

- **schema v3**（`nlos-process`）：`process_heads.lifecycle_state`（0=Active/1=Terminated/2=Crashed）+ 不可变 `process_terminal_markers`（按 `(process_id, process_generation)` 主键、idempotency key 唯一）。
- **入口**：`mark_process_terminated`（干净终止）与 `propagate_crash`（宿主 crash 传播）；CAS 对当前 generation/fence，`inspect_process_terminal` 读回。
- **fail-closed 门**：terminal 后 `register_fiber_incarnation`、`write_fiber_entry_snapshot`（resume 路径）、`inspect_active_process_binding` 均返回 `ProcessBindingTerminal` 零副作用；terminal 前已登记 incarnation 的 exact idempotency replay 仍合法。
- **restore**：`restore_process` 推进 generation 时重置 `lifecycle_state=Active`，不删历史 terminal marker 行（按 generation 归档）。
- **未做**：真实 host spawn/suspend/kill、runtime fiber 批量取消传播、跨 authority Task 收敛、三平台 fault matrix。

## 2. 验证

```text
cargo test -p nlos-process
  → 22 passed / 0 failed（+5 process_crash_propagation；含 fiber_incarnation 11 + process_authority 6）
cargo clippy -p nlos-process --all-targets -- -D warnings → 0 warning
cargo fmt -p nlos-process -- --check → 通过
```

覆盖：crash/terminated 标记与 reopen 幂等 replay；terminal 后 fiber 注册/快照/active readback fail-closed；stale fence 与异键二次标记拒绝；terminal 前 incarnation replay 仍合法。

## 3. 边界

单节点 SQLite H3 reference authority；不等同真实 kill-9 注入、Slice K 端到端 crash 收敛或 ROAD-B-006 整体达成。Claim 维持 `PARTIAL_PASS`。
