# B-PROCESS-003：process crash propagation / terminal lifecycle 最小前缀

- 状态：`PARTIAL_PASS`（ROAD-B-006 process crash propagation 合同层最小前缀；runtime 侧 join/cancel 联动、macOS/Windows 真 OS kill、Activation meter 未做）
- 日期：2026-09-05
- 设计依据：v0.5 §28.2 ROAD-B-006「Process crash propagation」；`[FIBER-FAIL-001]` Fiber 共享 Process 故障域
- 关联：`B-PROCESS-001` durable binding authority；W12-P 波次 13 车道

## 1. 实现事实

- **schema v5**（`nlos-process`）：v4 基础上增不可变 `platform_kill_receipts`（W17-P platform kill receipt 行）。
- **schema v4**（`nlos-process`）：v3 基础上增不可变 `fiber_incarnation_cancel_receipts`（W16-P batch cancel 传播 receipt 行）。
- **schema v3**（`nlos-process`）：`process_heads.lifecycle_state`（0=Active/1=Terminated/2=Crashed）+ 不可变 `process_terminal_markers`（按 `(process_id, process_generation)` 主键、idempotency key 唯一）。
- **入口**：`mark_process_terminated`（干净终止）与 `propagate_crash`（宿主 crash 传播）；`propagate_cancel_to_fibers`（batch invalidate + immutable receipt，crash/terminal 路径自动联动）；CAS 对当前 generation/fence，`inspect_process_terminal` / `inspect_fiber_incarnation_cancel_receipt` 读回。
- **fail-closed 门**：terminal 后 `register_fiber_incarnation`、`write_fiber_entry_snapshot`（resume 路径）、`inspect_active_process_binding` 均返回 `ProcessBindingTerminal` 零副作用；batch cancel 后 `inspect_fiber_incarnation` 返回 `FiberIncarnationCancelled`（绕过 terminal 读回亦 fail-closed）；terminal 前已登记 incarnation 的 exact idempotency replay 仍合法。
- **restore**：`restore_process` 推进 generation 时重置 `lifecycle_state=Active`，不删历史 terminal marker 行（按 generation 归档）。
- **未做**：真实 host spawn/suspend/kill、runtime 消费 cancel receipt 联动、跨 authority Task 收敛、三平台 fault matrix。

## 5. Process 域 fiber incarnation 批量 cancel 传播（2026-09-05 追加，W16-P）

- Owner：`nlos-process`（schema v4 + `propagate_cancel_to_fibers`）
- **实现**：`mark_process_terminated` / `propagate_crash` 同事务调用 `propagate_cancel_to_fibers_in_tx`；不可变 `fiber_incarnation_cancel_receipts`（按 `(process_id, process_generation, binding_id)` 主键、batch idempotency key 索引）；对每个 `fiber_incarnation_heads` 行 CAS 校验后写入 receipt；`inspect_fiber_incarnation` / `write_fiber_entry_snapshot` / 同代次 `register_fiber_incarnation` 经 `FiberIncarnationCancelled` fail-closed（不等同 platform kill）。
- **验证**：

```text
cargo test -p nlos-process
  → 25 passed / 0 failed（+3 fiber_cancel_propagation；2026-09-05 W16-P）
cargo clippy -p nlos-process --all-targets -- -D warnings → 0 warning
cargo fmt -p nlos-process -- --check → 通过
```

- **仍 PARTIAL_PASS**：runtime 侧 cancel receipt 消费接线、平台 kill adapter、Activation meter 联动未做。

## 6. Platform kill 合同层最小前缀（2026-09-06 追加，W17-P）

- Owner：`nlos-process`（schema v5 + `request_platform_kill`）
- **实现**：`PlatformKillAdapter` trait + `StubPlatformKillAdapter` / `NoopPlatformKillAdapter`；`ProcessAuthority::request_platform_kill` 校验 active binding → 不可变 `platform_kill_receipts`（按 `(process_id, process_generation)` 主键、idempotency key 唯一）→ 调用 adapter；terminal binding `ProcessBindingTerminal` fail-closed；exact idempotency replay 不重复调用 adapter；`inspect_platform_kill_receipt` 读回。
- **验证**：

```text
cargo test -p nlos-process
  → 20 passed / 0 failed（+3 platform_kill；2026-09-06 W17-P）
cargo clippy -p nlos-process --all-targets -- -D warnings → 0 warning
cargo fmt -p nlos-process -- --check → 通过
```

- **仍 PARTIAL_PASS**：非 macOS/Windows 真 OS kill-9/spawn；runtime 侧 kill receipt 消费、Activation meter 联动、跨平台 fault matrix 未做；不等同 ROAD-B-006 整体达成。

## 7. Platform kill AlreadyTerminated + inspect 读回（2026-09-08 追加，W19-P）

- Owner：`nlos-process`（`PlatformKillAdapterOutcome::AlreadyTerminated` + `inspect_platform_kill_receipt` 集成测试）
- **实现**：adapter 返回 `AlreadyTerminated` 时 durable receipt 已提交、`PlatformKillDecision::AlreadyTerminated` 读回；exact idempotency replay 不重复调用 adapter；`inspect_platform_kill_receipt` 按 `(process_id, process_generation)` 作用域读回，异 generation 返回 `None`、异 idempotency key 二次请求 `PlatformKillAlreadySignaled` fail-closed。
- **验证**：

```text
cargo test -p nlos-process
  → 22 passed / 0 failed（+2 platform_kill；2026-09-08 W19-P）
cargo clippy -p nlos-process --all-targets -- -D warnings → 0 warning
cargo fmt -p nlos-process -- --check → 通过
```

- **仍 PARTIAL_PASS**：非 macOS/Windows 真 OS kill-9/spawn；runtime 侧 kill receipt 消费、Activation meter 联动、跨平台 fault matrix 未做；不等同 ROAD-B-006 整体达成。

## 8. POSIX platform kill adapter 最小前缀（2026-09-09 追加，W20-P）

- Owner：`nlos-process`（`PosixPlatformKillAdapter` + Unix 集成测试）
- **实现**：`PosixPlatformKillAdapter` 接受 caller 注入的 `HashMap<ProcessId, u32>`（NLOS ProcessId ≠ OS pid）；Unix 上 `kill(2)` SIGTERM（`nix::sys::signal::kill`；工作区 forbid unsafe 故不用直接 `libc::kill`），ESRCH → `AlreadyTerminated`、成功 → `Signaled`；Windows 返回 `Platform` 错误（cfg 分派）。
- **验证**：

```text
cargo test -p nlos-process
  → 23 passed / 0 failed（+1 platform_kill posix 集成；2026-09-09 W20-P）
cargo clippy -p nlos-process --all-targets -- -D warnings → 0 warning
cargo fmt -p nlos-process -- --check → 通过
```

- **仍 PARTIAL_PASS**：Windows 真 OS kill、supervisor 自动 pid 映射、runtime kill receipt 消费、Activation meter 联动、跨平台 fault matrix 未做；不等同 ROAD-B-006 整体达成。

## 9. Windows platform kill adapter 最小前缀（2026-09-10 追加，W21-P）

- Owner：`nlos-process`（`WindowsPlatformKillAdapter` + 非 Windows stub 测试）
- **实现**：`WindowsPlatformKillAdapter` 接受 caller 注入的 `HashMap<ProcessId, u32>`（NLOS ProcessId ≠ OS pid）；Windows 上经 `taskkill /F /T` 发出 `TerminateProcess` 等价信号（工作区 forbid unsafe 故不直接绑定 `OpenProcess`/`TerminateProcess`），进程不存在 → `AlreadyTerminated`、成功 → `Signaled`；缺失 map 项 → `Platform` 错误；非 Windows cfg 返回 `Platform` 错误（镜像 `PosixPlatformKillAdapter` 在 Windows 上的 stub 分派）。
- **验证**：

```text
cargo test -p nlos-process
  → 25 passed / 0 failed（+2 platform_kill Windows stub；2026-09-10 W21-P）
cargo clippy -p nlos-process --all-targets -- -D warnings → 0 warning
cargo fmt -p nlos-process -- --check → 通过
```

- **仍 PARTIAL_PASS**：Windows 真 OS kill 集成测试（需 Windows host）、supervisor 自动 pid 映射、runtime kill receipt 消费、Activation meter 联动、跨平台 fault matrix 未做；不等同 ROAD-B-006 整体达成。

## 4. Runtime 侧 terminal 门（2026-09-05 追加，W15-P）

- Owner：`nlos-runtime-tokio`（`src/replay.rs`、`src/snapshot.rs` + `tests/process_crash_propagation.rs`）
- **实现**：`resume_binding` 与 `resume_from_snapshot` / `snapshot_handler_entry` 在代次 gate 或 durable 写之前调用 `inspect_active_process_binding`，使 terminal/crashed process 以 `ProcessBindingTerminal` fail-closed（`inspect_fiber_incarnation` 本身不拒绝 terminal head）。
- **验证**：

```text
cargo test -p nlos-runtime-tokio --test process_crash_propagation
  → 3 passed / 0 failed（2026-09-05 W15-P）
```

- **仍 PARTIAL_PASS**：fiber 批量 cancel 传播（process 域最小前缀见 §5）、平台 kill adapter、Activation meter 联动未做；不等同 ROAD-B-006 整体达成。

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
