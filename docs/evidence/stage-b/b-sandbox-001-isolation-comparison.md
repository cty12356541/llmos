# b-sandbox-001-isolation-comparison：Wasmtime/WASI 与独立 host Process 隔离对比实验

- **Evidence ID**：b-sandbox-001-isolation-comparison
- **工作项**：B-SANDBOX（2026-08-29「Slice K 立即全量并行」决策的沙箱车道）
- **结论等级**：**PARTIAL_PASS**（macOS 单平台实测完成；Linux/Windows、Seatbelt、fuel 开销精确值等未运行项见下）
- **决策状态**：**candidate**（推荐进入 ADR 流程；不构成已采纳决策，不改变任何规范词状态）
- **关联规范**：stage-b-technology-selection.md §5（PoC 必须对比 in-process Wasmtime / 独立 host Process / native Process adapter）；06-架构设计总纲 v0.5 §6（Safety TCB；`[TCB-BYPASS-001]`、`[TCB-EGRESS-001]`、`[TCB-DOWNGRADE-001]`）
- **日期**：2026-08-29（HEAD b0badd5）
- **实验产物**：`experiments/exp8-sandbox-comparison/`（独立 Cargo.toml + 空 `[workspace]` 声明，未加入根 workspace members，未修改根 Cargo.toml）

## 1. 环境与复现

| 项 | 值 |
|---|---|
| 平台 | macOS 26.5.2 (BuildVersion 25F84)，arm64 |
| 工具链 | rustc/cargo 1.97.1 |
| 依赖 | wasmtime 36.0.14、wasmtime-wasi 36.0.14、nix 0.31.3、anyhow 1（crates.io 下载成功，未启用降级路线） |
| 命令 | `cd experiments/exp8-sandbox-comparison && cargo build --release && cargo run --release` |
| 输出 | stdout 77 行结果 + `results/exp8-results.md` + `results/exp8-results.json` |
| 复现性 | 全部场景 exit 0；D1 计时类指标受机器负载影响，资源类/errno 类指标跨次稳定 |

## 2. 实测结果摘要（四维）

### D1 CPU 限制：生效延迟与精度

- **fuel 确定性精确**：fuel=1000/10⁴/10⁵ 恰好执行 67/667/6667 次迭代（≈15 fuel/迭代），双次运行逐一相等（run1==run2=true ×3）
- **epoch 异步中断**：deadline 10/50/100ms → 实测 15.06/75.20/149.73ms；校准出 macOS 名义 1ms tick 的有效周期 1.488ms（timer coalescing），按有效 tick 折算超调仅 0.18–0.93ms
- **进程 SIGKILL（1ms 轮询）**：CPU 超调中位 0–1ms，范围 0–4ms（≈1 个轮询周期 + 调度噪声）
- **RLIMIT_CPU(soft=2s)**：实际 CPU 2.006s 终止；量子 1s，无法表达毫秒级配额
- **fuel 计量开销**：MEASURED-UNSTABLE——本机并行车道负载下样本方差大于效应（metered 6.5→62ms vs unmetered 4.7→9.6ms），不能给出可靠百分比，仅可确认同数量级

### D2 内存限制 enforcement

- **Wasmtime limiter**：宿主施加的页级上限精确生效——max=10 页时 grow(100)→-1；max=6 页时 grow(5) 成功、再 grow(1)→-1；64KiB 粒度、memory.grow 调用点内联确定性拒绝
- **macOS RLIMIT_AS 不可用（决定性实测）**：`setrlimit(RLIMIT_AS, …)` 无论 64MB 还是 2GB 均返回 **EINVAL: Invalid argument**；子进程随后无限制分配至 256MB/4GB 目标全部达成
- 结论：macOS(arm64) 上 host-process 侧没有可用的内核级内存配额原语（RLIMIT_RSS 亦为遗留 no-op）

### D3 capability 权限面收敛

| 操作 | deny-all（无 preopen/无显式 env） | grant（stdout+只读 preopen） | host Process 默认 spawn |
|---|---|---|---|
| 文件 path_open | 8(EBADF) 拒绝 | preopen 内可达；路径逃逸 `../../etc/hosts`→**63(EPERM)** | /etc/hosts 与 $HOME 可读（ambient 全通） |
| environ | 空集 0 变量 | 空集 | 继承 46 个变量 |
| 继承 FD / socket / 写临时目录 | — | — | 4 FD / socket 可建 / TCP connect 调用可达 / 写 OK |

- **诚实标注**：wasmtime-wasi 36 p1 shim 默认即授予 stdout 与 clock（fd_write(1)、clock_time_get 在 deny-all 配置返回 SUCCESS），「默认全拒」的朴素预期不成立
- **发现（shim 缺陷线索）**：`DirPerms::READ` 只读 preopen 未拦截对 preopen 内文件的写打开（返回 SUCCESS）；capability 收敛完整性依赖 shim 实现质量，须逐版本回归

### D4 host 故障隔离

- Wasmtime：guest `unreachable` → Trap(UnreachableCodeReached)，host 捕获 Err 后继续；guest 乱写自身线性内存 0..256，host 哨兵数据完整
- 进程：子进程 abort()（SIG6）后父进程存活并 reap（检出 52ms）；SIGTERM 免疫子进程 TERM 后 200ms 存活、SIGKILL 即刻生效（reap 0ms）
- 设计注记（DESIGN 级，未破坏性验证）：in-process Wasmtime 与宿主共享进程 fault domain——engine 侧原生崩溃/OOM 会连带全部 guest

## 3. 推荐（candidate，建议后续 ADR）

1. 互不信任或需独立强杀的组件采用**「独立 Process + Wasmtime」双层隔离**：进程层提供 fault domain 与 SIGKILL 终途（D4），Wasm 层提供细粒度资源配额与 capability 收敛（D1/D2/D3）；与选型文档 §5 默认建议一致
2. in-process Wasmtime 仅限同信任域插件，并按 `[TCB-DOWNGRADE-001]` 声明 GuaranteeTier
3. macOS 上禁止以 rlimit 充当内存限制（D2 EINVAL 实测）
4. native Process adapter 若引入必须补 capability 收敛层（D3 ambient 全通不可接受）
5. ADR 需包含：双层边界与 GuaranteeTier 映射、wasmtime-wasi p1 shim capability 行为版本锁定与回归项、Linux rlimit 补测、p2/component 升级时的 capability 面重审

## 4. 未运行项与已知限制（显式）

- **平台**：Linux/Windows 全部场景未运行（rlimit 行为平台差异是最大缺口）
- **未实测**：macOS Seatbelt/App Sandbox 的进程 capability 收敛；Mach task 内存限制；Linux RLIMIT_AS/CPU；wasmtime p2 与 Component Model；fuel 开销可靠百分比（MEASURED-UNSTABLE）；「in-process engine 被原生崩溃连带」破坏性验证（保留 DESIGN 论证）
- **测量口径**：fuel 精度按特定 spin 循环（≈15 fuel/迭代）标定，不同 guest 字节码比例不同；epoch 中断仅作用于 loop back-edge/调用点；D1 计时受机器并行负载影响
- **证据等级**：本文为 TESTED（macOS 单平台）/ 部分结论 DESIGN（Linux rlimit 行为、in-process fault domain 连带），未达 CONFORMANT

## 5. 写集声明

本证据与 `experiments/exp8-sandbox-comparison/**` 为同一 Attempt 的完整写集；未触碰 `crates/**`、`docs/management/**`、根 `Cargo.toml`、`experiments/README.md`；未执行任何 git 操作（分支并行车道纪律）。
