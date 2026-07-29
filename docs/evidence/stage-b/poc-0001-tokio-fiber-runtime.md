# PoC-0001：Tokio ExecutionFiber Runtime 初始证据

> 状态：PARTIAL PASS
>
> 日期：2026-07-29
>
> 对应：`FIBER-MN-001`、`FIBER-CANCEL-001`、`FIBER-FAIL-001`、`FIBER-METER-001`、`ROAD-B-006`

## 1. 实现范围

- runtime-independent `RuntimeAdapter`；
- 独立 `nlos-runtime-tokio` adapter；
- NLOS `ExecutionFiberId` 与 Tokio task identity 分离；
- `Semaphore` 有界 live-fiber admission；
- `CancellationScopeId + Generation`；
- Fiber state inspect；
- stale Fiber generation rejection；
- panic → `FAILED`；
- scheduler-wait 与 elapsed-wall 初步计量；
- 2 个 Tokio worker 承载 10K/100K pending Fiber。

## 2. 环境

```text
hardware architecture: arm64 / Apple Silicon
OS: macOS 26.5.2 (Build 25F84)
rustc: 1.97.1 (8bab26f4f 2026-07-14)
cargo: 1.97.1
tokio: 1.53.1
runtime worker threads: 2
build: --release
```

## 3. 正确性测试

命令：

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

结果：

- 2 项 nominal ID/generation 测试通过；
- bounded admission 通过；
- structured scope cancellation 通过；
- stale generation fence 通过；
- completed Fiber state/usage 通过；
- panic 转换为 `FAILED` 通过；
- 10K waiting Fiber 通过；
- 100K 测试默认 ignored，只在显式 ScaleProfile probe 中运行；
- rustfmt 与 Clippy `-D warnings` 通过。

## 4. 规模测试

测试模型：

```text
100K NLOS Fiber records
100K Tokio tasks
2 host worker threads
each future = pending forever
one shared CancellationScope
spawn all → cancel scope → wait for all CANCELLED
```

### 10K

```text
test elapsed: 0.01 s
maximum resident set size: 15,826,944 bytes
maximum RSS: 15.09 MiB
peak memory footprint: 14,745,936 bytes
```

### 100K

```text
test elapsed: 0.20 s
maximum resident set size: 134,627,328 bytes
maximum RSS: 128.39 MiB
peak memory footprint: 133,628,384 bytes
```

10K→100K 的粗略 RSS 增量斜率约为：

```text
(134,627,328 - 15,826,944) / 90,000
= 1,320 bytes / added Fiber
```

该数值包含 HashMap record、Future、Tokio task、scope reference、test handle 和 allocator 行为，不等于单个生产 Fiber 的精确对象大小。

复现：

```sh
cargo test --release -p nlos-runtime-tokio \
  --test scale one_hundred_thousand_waiting_fibers_on_two_threads \
  -- --ignored --exact --nocapture
```

macOS 峰值 RSS 使用编译后的测试二进制直接测量，避免 Cargo/编译器污染：

```sh
/usr/bin/time -l \
  target/release/deps/<scale-test-binary> \
  one_hundred_thousand_waiting_fibers_on_two_threads \
  --ignored --exact --nocapture
```

## 5. 当前能证明什么

在上述单机环境和极简 pending workload 下：

- 100K waiting Fiber 不需要 100K host thread；
- 两个 Tokio worker 可以承载并取消这些 Fiber；
- live admission 有硬上界；
- scope cancellation 和 Fiber generation fencing 在当前测试中成立；
- 100K waiting workload 的进程最大 RSS 约 128.39 MiB。

这支持 Tokio 继续作为阶段 B runtime 候选。

## 6. 当前不能证明什么

- 100K 同时 runnable/CPU-heavy Agent；
- 模型、工具、网络和真实 Operation callback；
- late callback 对 Artifact/TaskHead 的端到端 fence；
- active CPU 与 external wait 的准确分维计量；
- deadline 与外部 effect reconciliation；
- fairness、priority、EDF、ResourceGroup quota 和 starvation；
- Process crash/restore；
- completed Fiber record 的有界 retention/GC；
- 多 CancellationScope 树和 structured join/detach；
- Linux/Windows 上的同等结果；
- 长时间 soak、内存碎片和负载波动；
- production ScaleProfile。

`ActivationUsage.active_cpu` 当前保持为零，避免用 wall time 冒充 CPU consumption。

## 7. 发现的设计约束

1. 直接用 Tokio task ID 会丢失稳定身份，必须保留 NLOS nominal ID；
2. cancellation 需要 scope + generation，不能只 drop handle；
3. panic 必须被 runtime 捕获并转成 Fiber failure，否则状态会停留在 RUNNING；
4. Fiber record 当前保留终态以支持 inspect，但必须增加 retention/GC policy；
5. Semaphore 只证明 live slot 有界，尚未实现按 ResourceGroup 的层级 queue；
6. 真实 late callback fence 必须在下一步 Operation registry 中实现。

## 8. 下一验证门

PoC-0001 仍保持 `PARTIAL PASS`。升为 ACCEPTED 前至少补充：

1. 将已取得 `PARTIAL PASS` 的 [PoC-0002 Operation callback fence](./poc-0002-operation-callback-fence.md)与 Tokio wake、Driver 和 durable store 集成；
2. parent/child CancellationScope 与 join/detach；
3. completed record retention/GC；
4. priority/fairness 与 CPU-heavy isolation；
5. scheduler queue、external wait、backpressure 分维计量；
6. 100K workload 重复/soak 和跨平台复验。
