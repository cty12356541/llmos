# W36-P10 / C-RUNTIME-PROC：kill receipt×meter + B6-4 supervisor + B6-5 BirthDecision

- 状态：`PARTIAL_PASS`（三子项合同/最小前缀已接线并有行为测试；非跨 authority 完整 BirthDecision，非 ROAD-B-006 整体达成；Windows supervisor 实杀臂未在本机执行）
- 日期：2026-09-21
- Owner：`nlos-runtime` / `nlos-runtime-tokio` / `nlos-process`
- 设计依据：W34-A §6 residual 3；阶段 C 移交 #10；v0.5 §8.2 合同层 `BirthDecision` disposition 切片（非跨域 durable 对象）
- 关联：`b-runtime-002` §6.16.2（kill receipt 消费 residual）；`b-process-003` §6–§10（platform kill / supervisor pid registry residual）；B6-4 / B6-5
- 分支：`feat/w36-p10`；base `7d3ad53`；合入本地 main `c45bd10`（2026-09-22）。本文件为新 Evidence 小节，不改 `b-runtime-002` / `b-process-003` 正文（避免与既有追加节冲突）

## 1. 实现事实

### 1.1 kill receipt 消费 × Activation meter（`bd972e6`）

- 入口：`TokioRuntimeAdapter::consume_platform_kill`（`crates/nlos-runtime-tokio/src/kill_receipt.rs`）。
- 门序 fail-closed：① `inspect_platform_kill_receipt` 无回执 → `ChannelWaitError::PlatformKillReceiptAbsent`，零 runtime 副作用；② 有回执后走既有 W27-C `cancel_process_fibers`（非终态 binding / stale fence 仍由其 durable 门拒绝）；③ 成功消费递增 lock-free `RuntimeHealth::platform_kills_consumed_total`，sweep 计数走 `process_cancel_sweeps_total` / `process_fiber_cancel_matched_total`；被杀 fiber 的 `ActivationUsage` 经既有终态 finalize 缝闭合。
- 幂等：durable 回执不可变、传播可 replay、scope cancel 幂等；每次成功消费再计量一次（计量的是联动路径，不是 ledger）。

### 1.2 B6-4 跨平台 supervisor（`28567e1`）

- `ProcessSupervisor` 复用 `SupervisorPidRegistry` + 平台 adapter；kill 绑到围栏解析的 `SupervisorPidEntry`（`from_fenced_entry`），不另起 pid 账。Durable `request_platform_kill` 调用方可仍喂 `registry.pid_map()`。
- spawn 观察 OS pid 后登记；登记拒绝则杀掉并回收刚 spawn 的 child（`[PROC-SPAWN-003]` supervisor 侧类比）。
- Unix：真实 spawn / `SIGSTOP`+`SIGCONT` suspend-resume / SIGTERM kill（ESRCH → `AlreadyTerminated`）。
- Windows：真实 spawn + `taskkill /F /T` kill；suspend/resume typed `UnsupportedOnPlatform`（工作区 `unsafe_code = forbid`，不发明能力）。
- 本链不写 durable；权威 kill 仍走 `request_platform_kill`，adapter 可由 `registry.pid_map()` 喂入。

### 1.3 B6-5 合同层 BirthDecision（`7f03771`）

- `nlos-runtime` 合同：`BirthDecision::{Admitted, Rejected}` + `BirthRejection` 六维（Capacity / Scope / Budget / Identity / GenerationFence / Unavailable），`From<RuntimeError>` 对合同错误族全覆盖、不发明维度。
- 明确声明：这是 runtime 准入切片（对应 §8.2 `COMMIT | ABORT`），不是跨 authority durable `BirthDecision`（launch grant / resource / capability prepares 仍在域外）。
- `RuntimeAdapter::birth_fiber` 默认路径驱动 `spawn_fiber` 并分类；`TokioRuntimeAdapter` 不覆盖，走默认接线。

## 2. 验证（2026-09-21，macOS arm64，`CARGO_TARGET_DIR=/tmp/nlos-w36-p10-target`）

```text
cargo test -p nlos-runtime -p nlos-runtime-tokio -p nlos-process --offline
  → 185 passed / 0 failed / 14 ignored
    nlos-process 41；nlos-runtime 2；nlos-runtime-tokio 142 + 14 ignored（既有规模探针 ignore，与本车道无关）
cargo fmt -p nlos-runtime -p nlos-runtime-tokio -p nlos-process -- --check → 通过

定向：
  --test kill_receipt     → 5 passed（含 POSIX 真子进程 e2e）
  --test birth_decision   → 7 passed
  --test supervisor       → 5 passed（Unix 臂：spawn+kill / suspend-resume / stale fence / spawn 拒绝回收）
  nlos-runtime --lib      → 2 passed（From<RuntimeError> 全覆盖 + handle 仅 Admitted）
```

未运行：Windows `#[cfg(windows)]` supervisor 真杀 / `UnsupportedOnPlatform` 臂（本机 macOS）；`cargo clippy --workspace`；未 merge `origin/main`（写集不相交，编测不依赖 `8b91419`）。

## 3. 边界

- 不发明跨 Cell / 分布式语义。
- 不重做移交 #4 Windows live-child 实杀、#6 100K cancel。
- Claim 维持既有 `PARTIAL_PASS`：三子项 residual 收到本前缀，不等于 ROAD-B-006 整体达成，也不等于跨 authority BirthDecision。
