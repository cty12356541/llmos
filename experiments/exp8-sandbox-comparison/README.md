# exp8-sandbox-comparison：Wasmtime/WASI 与独立 host Process 隔离对比（B-SANDBOX）

> 目的：为「不可信应用执行」选型提供实测依据——对比 in-process Wasmtime/WASI 与独立 host Process 两条隔离路线在四个维度上的真实能力边界
> 规范依据：[阶段 B 技术选型 §5](../../docs/management/stage-b-technology-selection.md)（要求 PoC 对比 in-process Wasmtime / 独立 host Process / native Process adapter）；[架构总纲 §6](../../docs/design/06-架构设计总纲-v0.5.md)（Safety TCB、TCB-BYPASS-001、TCB-EGRESS-001、TCB-DOWNGRADE-001）
> 日期：2026-08-29；状态：**实测完成（macOS 单平台），结论为 candidate，非已采纳决策**
> 独立编译：本目录自带空 `[workspace]` 的 Cargo.toml，未加入根 workspace members（未修改根 Cargo.toml）

## 判定标准（先于代码定义）

| 维度 | Wasmtime 侧 | host Process 侧 | 数据口径 |
|---|---|---|---|
| D1 CPU 限制 | fuel（指令级确定性）与 epoch（tick 级异步中断）的生效精度/延迟 | 轮询 + SIGKILL 的 CPU 超调；RLIMIT_CPU 粒度 | fuel→迭代数映射 + 确定性；epoch/kill 超调 ms |
| D2 内存限制 | Store ResourceLimiter 对 memory.grow 的拒绝粒度 | RLIMIT_AS 是否可设置/生效 | 拒绝发生点、粒度、是否平台可用 |
| D3 capability 面 | WASI 默认拒绝面：无 preopen/env 时敏感调用 errno；显式授予后的收敛 | 默认 spawn 的 ambient authority（env/FD/文件/socket） | 敏感操作成功/失败计数 |
| D4 故障隔离 | guest trap 可恢复性、host 状态完整性 | 子进程 crash 后父存活；SIGTERM 免疫子进程的 SIGKILL 升级 | trap 类型、父存活、升级延迟 |

## 构建

- Rust + `wasmtime`/`wasmtime-wasi`（36.0.14，crates.io 依赖下载成功，未走降级路线）+ `nix`（rlimit/signal/rusage 安全封装）
- WAT guest 手写内嵌（`src/guest.rs`），免 wasm 工具链依赖；子进程用 `current_exe()` 自 spawn，免外部 stress 工具
- 网络依赖实测：一次成功（首次 `cargo check` 拉取并编译 wasmtime 全家桶约 10 分钟）

```bash
cd experiments/exp8-sandbox-comparison
cargo build --release
cargo run --release          # 或 ./target/release/exp8-sandbox-comparison [d1|d2|d3|d4|all]
```

本实验唯一 unsafe 点：`child.rs` 的 `ignore_term()`（sigaction 注册 SIG_IGN），已在源码注明；crate 级 lint 为 `deny`（非仓库根的 `forbid`），因实验需直接操作 OS 原语。

## 结果（2026-08-29，macOS 26.5.2 / arm64 / rustc 1.97.1 / wasmtime 36.0.14，77 行原始数据见 results/）

### D1 CPU 限制：生效延迟与精度

| 场景 | 关键数据 | 判定 |
|---|---|---|
| Wasmtime fuel=1000 | 恰好执行 **67** 次迭代后 OutOfFuel；run1==run2 | ✅ 确定性、指令级精度 |
| Wasmtime fuel=10⁴ / 10⁵ | 667 / 6667 次迭代（严格 10× 比例，≈15 fuel/迭代） | ✅ 线性可预期 |
| Wasmtime epoch（名义 1ms tick） | deadline 10/50/100ms → 实测 15.06/75.20/149.73ms；**有效 tick=1.488ms**（macOS timer coalescing）；按有效 tick 折算超调仅 0.18–0.93ms | ✅ 可靠中断，精度=tick 粒度+相位抖动 |
| 进程 SIGKILL（1ms 轮询） | deadline 10/50/100ms → CPU 超调中位 1/0/1ms（范围 0–4ms） | ✅ 超调 ≤1 个轮询周期 + 调度噪声 |
| 进程 RLIMIT_CPU（soft=2s） | 实际 CPU 2.006s 终止 | ⚠️ 量子=1s，无法表达毫秒级配额 |
| fuel 计量开销 | 本机受并行负载污染：metered [6.5→62ms] vs unmetered [4.7→9.6ms]，**MEASURED-UNSTABLE**，仅 min 口径 +37% 量级参考 | ⚠️ 本机噪声下无法给出可靠百分比 |

### D2 内存限制 enforcement

| 场景 | 关键数据 | 判定 |
|---|---|---|
| Wasmtime limiter（max=10 页） | grow_to(100) → -1 拒绝 | ✅ 宿主施加、内联确定性 |
| Wasmtime limiter（max=6 页） | grow(5) 成功、再 grow(1) 拒绝 | ✅ **64KiB 页粒度**、精确到页 |
| 进程 setrlimit(RLIMIT_AS, 64MB) | **EINVAL：Invalid argument** | ❌ macOS 根本不允许设置 |
| 进程 setrlimit(RLIMIT_AS, 2GB) | **EINVAL**；子进程随后无限制分配到 4GB 目标 | ❌ 同上 |
| 基线（无限制） | 256MB/4GB 目标全部达成 | 对照成立 |

**macOS(arm64) 上 host-process 侧不存在可用的内核级内存配额原语**（RLIMIT_AS 无法设置、RLIMIT_RSS 为历史遗留 no-op）；Linux 的 RLIMIT_AS 生效于 mmap 路径（DESIGN 引用，本实验未实测 Linux）。

### D3 capability 权限面收敛

| 操作 | Wasmtime deny-all（无 preopen/无显式 env） | Wasmtime grant（stdout+只读 preopen） | host Process（默认 spawn） |
|---|---|---|---|
| clock_time_get | 0(SUCCESS)* | 0(SUCCESS) | 可用（隐含） |
| fd_write(stdout) | 0(SUCCESS)* | 0(SUCCESS) | 可用 |
| path_open（文件访问） | **8(EBADF)** 拒绝 | preopen 内可达；路径逃逸 `../../etc/hosts` → **63(EPERM)** | /etc/hosts、$HOME 全部可读 |
| environ | 空集（0 变量） | 空集 | **继承 46 个环境变量** |
| 继承 FD / socket / 临时目录写 | 0 | 0 | 4 个 FD、socket 可建、TCP connect 调用可达、写 OK |

\* 诚实记录：wasmtime-wasi 36 的 p1 shim **默认授予 stdout 与 clock**（与「默认全拒」的朴素预期不符）；env 默认空集、文件默认全拒。
另发现：`DirPerms::READ` 的只读 preopen **未能拦截**对 preopen 内文件的写打开（0 SUCCESS）——p1 shim 对目录权限降权的执行不完整，capability 收敛依赖 runtime 实现质量。

### D4 host 故障隔离

| 场景 | 关键数据 | 判定 |
|---|---|---|
| Wasmtime guest `unreachable` | Trap=UnreachableCodeReached，host 捕获为 Err 后继续运行 | ✅ trap 可恢复 |
| guest 乱写自身 memory 0..256 | host 侧哨兵数据完整 | ✅ 线性内存与宿主隔离 |
| 子进程 abort()（SIG6） | 父进程存活并 reap（检出 52ms） | ✅ 进程边界隔离 |
| SIGTERM 免疫子进程 | TERM 后 200ms 仍存活 → SIGKILL 即刻生效（reap 0ms） | ✅ OS 提供 SIGKILL 不可屏蔽终途 |

## 结论（candidate）

1. **CPU**：fuel 提供指令级确定性配额（1000 fuel → 精确 67 迭代、可复现），epoch 提供毫秒级异步中断（有效精度 = tick 粒度 + 亚毫秒抖动）；进程侧 SIGKILL 轮询超调 ≈ 轮询周期（≤4ms），RLIMIT_CPU 量子 1s 完全不够用。**配额语义上 Wasmtime 显著更精细**。
2. **内存**：macOS 上 host-process 侧没有可用配额原语（RLIMIT_AS 直接 EINVAL）；Wasmtime limiter 64KiB 粒度内联拒绝。**本维度 Wasmtime 决定性胜出（macOS）**；Linux 待实测。
3. **capability**：WASM 默认拒绝文件/env，授予即收敛；但 v36 p1 shim 默认放行 stdout+clock、只读 preopen 未拦写打开——**默认面优于进程，但收敛完整性依赖 shim 实现质量，须逐版本回归**。进程默认继承全部 ambient authority（46 env、文件/socket 全通），denial 需外部机制（macOS 上为 Seatbelt/App Sandbox，本实验未测）。
4. **故障隔离**：方向相反——guest 侧 trap 可恢复且无法伤害宿主内存，但 in-process 模型共享进程 fault domain（engine 原生崩溃/OOM 连带全部 guest，DESIGN 论证）；进程侧有 SIGKILL 这条 OS 级强杀终途。**强隔离与强杀语义在进程侧，细粒度资源控制在 Wasmtime 侧**。

### 推荐（→ 建议后续 ADR，非已采纳决策）

与选型文档 §5 默认建议一致的实证支持：**互不信任或要求独立强杀的组件采用「独立 Process + Wasmtime」双层隔离**——进程层给 fault domain 与强杀终途（D4），Wasm 层给资源配额与 capability 收敛（D1/D2/D3）；in-process Wasmtime 仅用于同信任域插件并按 GuaranteeTier 降级声明。**在 macOS 上绝不能用 rlimit 作为内存限制的替代**（D2 EINVAL）。原生命令适配器（native Process adapter）若引入，必须补 capability 收敛层，否则 ambient authority 全通（D3）不可接受。

建议 ADR 要点：① 双层隔离的默认边界与 GuaranteeTier 映射；② Wasmtime p1 shim capability 行为的版本锁定与回归项（默认 stdout/clock、preopen 权限）；③ Linux RLIMIT_AS/RLIMIT_CPU 补测；④ wasmtime-wasi 升级到 p2/component 时的 capability 面重审。

## 已知限制与未运行项

- **单平台**：全部数据来自 macOS 26.5.2/arm64；Linux/Windows 未实测（rlimit 行为平台差异大，是主要缺口）
- **未实测**：Linux RLIMIT_AS/CPU、macOS Seatbelt/App Sandbox 进程 capability 收敛、Mach task 内存限制、wasmtime p2/component、fuel 开销的可靠百分比（本机并行负载下 MEASURED-UNSTABLE）、「in-process engine 被原生崩溃连带」的破坏性验证（DESIGN 论证）
- epoch 中断只在 loop back-edge/调用点检查；fuel 精度实测口径为该特定 spin 循环（≈15 fuel/迭代），不同 guest 字节码比例不同
- D3 的进程侧 maxrss（6.4MB）与 D2 的 child maxrss 为 macOS 字节口径换算 KB
- 本 README 数据为单次完整运行摘录；机器负载波动会使 D1 计时类指标漂移，复现时以 results/ 当次输出为准
