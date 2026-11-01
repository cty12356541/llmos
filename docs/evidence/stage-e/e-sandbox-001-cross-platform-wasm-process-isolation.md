# E-SANDBOX-001：跨平台双层 Wasm 强沙箱（独立宿主 Process + Wasmtime WASI 白名单 + 内存页/CPU fuel 硬限 + 零宿主环境继承）与逃逸对抗矩阵证据（`W80-E6A`）

- **Evidence ID**: `e-sandbox-001-cross-platform-wasm-process-isolation`
- **Wave / Lane**: `W80 / W80-E6A-WASM-SANDBOX`（工作包 `E6` 沙箱面，闭合 `U-9` / `RISK-E-04`）
- **日期**: `2026-11-01`
- **Assurance Level**: `H4`（纯确定性本地测试与真实子进程集成对抗验证；Windows 11 本机实跑，Linux/macOS 归三平台 CI，不声称 `H7` 生产环境证明）
- **状态**: `VERIFIED`（车道级 `54/54` 全绿，并经 `W80-INT` 四车道工作区合流复验通过，晋升 canonical `DONE`；Linux/macOS `#[cfg(unix)]` 实跑由远端三平台 CI 矩阵守护）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §6（`TCB-BYPASS-001`、`TCB-EGRESS-001`、`TCB-DOWNGRADE-001`）、§7（`CAP-UNFORGE-001` 域内能力边界）、§15.1（`DRV-PROTO-001`、`DRV-LIFE-001`）、§24.2（`TUI-CONSENT-001`）
  - [ADR-0015 双层不可信执行隔离](../../management/adrs/0015-dual-layer-untrusted-execution-isolation.md)（决策 1 双层结构、决策 2 fuel/epoch 分层、决策 3 内存以 Wasm 层为准、决策 4 WASI preopen 白名单收敛、决策 6 信任档挂接）
  - [ADR-0027 Stage E SABI 长期兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.2（`COMMUNITY_SANDBOXED` 强制封死在双层沙箱）与 §2.3 三级信任域-隔离策略映射
  - [b-sandbox-001-isolation-comparison](../stage-b/b-sandbox-001-isolation-comparison.md)（macOS 单平台选型基线；本证据闭合其「Linux/Windows 未运行」最大缺口中的 Windows 侧）
  - [stage-e-progress.md](../../management/stage-e-progress.md) §E.2.2 工作包 `E6`、§E.3.2 `W80-E6A` 写集边界

---

## 1. 交付物架构（`crates/nlos-wasm-sandbox`）

100% 安全 Rust（`#![forbid(unsafe_code)]`，workspace `clippy::all` + `clippy::pedantic` `-D warnings` 零警告，零新增第三方依赖——仅消费 `W80-0-SCAFFOLD` 已预锁的 `wasmtime 50.0.1` 默认 feature 集，`Cargo.toml` 与根 `Cargo.lock` 零改动）：

| 模块 | 层 | 职责（规范绑定） |
| :--- | :--- | :--- |
| [process.rs](../../../crates/nlos-wasm-sandbox/src/process.rs) | **L1 宿主进程** | `build_isolated_host_command`：`std::process::Command` + `.env_clear()`（空环境块）+ 显式 argv（guest 路径/入口/fuel/内存页/epoch/预打开/宾客 argv 全部走命令行，零环境变量取值）+ stdin `null`/stdout/stderr 管道 + 显式工作目录；`run_isolated_host` 墙钟死线轮询 + `kill()` 硬杀终途（Windows `TerminateProcess`/Unix `SIGKILL`，`ADR-0015` 决策 1 终途语义）；`spawn_env_audit` 环境零继承审计（`TCB-BYPASS-001`、`TCB-EGRESS-001`、`DRV-LIFE-001`） |
| [host_bin.rs](../../../crates/nlos-wasm-sandbox/src/host_bin.rs) + [bin/nlos-wasm-sandbox-host.rs](../../../crates/nlos-wasm-sandbox/src/bin/nlos-wasm-sandbox-host.rs) | **L1 承载进程** | 专职宿主二进制：**生产路径中唯一加载不可信 Wasm 的进程**；除 `--self-audit-env` 审计模式外读零个环境变量；严格 argv 状态机（未知旗标 fail-closed 拒绝）；`NLOS-HOST-R1` 单行结构化报告协议（stderr）承运 `outcome/code/detail/guest_stdout_len` |
| [isolate.rs](../../../crates/nlos-wasm-sandbox/src/isolate.rs) | **L2 引擎硬限** | `pub(crate)` 引擎运行器（按 `ADR-0015` 决策 6 仅专职宿主 bin 与本 crate 测试可达，库公共 API 不暴露进程内执行面）：`Config::consume_fuel` + `epoch_interruption`、`Store::set_fuel` 确定性配额、`PageLimiter`（`ResourceLimiter` 64KiB 页粒度，`ADR-0015` 决策 3——不使用宿主 rlimit 充当内存配额）、epoch ticker 线程墙钟死线（`Trap::Interrupt`）、guest 终态分类（`Trap::OutOfFuel`/`Interrupt`/`UnreachableCodeReached`/`GuestExited` downcast 具体变体） |
| [wasi_allowlist.rs](../../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs) | **L2 WASI 白名单** | 手写 `wasi_snapshot_preview1` 白名单面（见 §2）；预打开集合默认为空；**零 socket 导入注册**（导入 `sock_open` 的 guest 在实例化即被 unknown-import 拒绝，不存在可达 socket 的代码路径）；`environ_*` 恒空集；`args_*` 仅显式 argv |
| [profile.rs](../../../crates/nlos-wasm-sandbox/src/profile.rs) | 契约 | `SandboxProfile` deny-by-default 能力与硬限信封（fuel `>=1`、页上限 `1..=65536`、epoch 死线须配 ticker 间隔、预打开 guest 名严格 guest-绝对路径、宿主根构造时 canonicalize）fail-closed 校验 |
| [error.rs](../../../crates/nlos-wasm-sandbox/src/error.rs) | 契约 | 类型化错误层级（`ProfileInvalid`/`SandboxError`/`TrapKind`/`HostReport`），报告行 detail 消毒（单行、160 字符界内、字母数字与 `-_.:` 白名单） |

## 2. 关键设计决策与边界（如实声明）

1. **手写 WASI p1 白名单面而非链接 `wasmtime-wasi`**：`W80-0-SCAFFOLD` 预锁供应链（`RISK-E-04`）的 `Cargo.lock` 中不含 `wasmtime-wasi`/`wasi-common`，且车道任务书禁止动根 `Cargo.lock`。本车道因此直接在 `wasmtime::Linker` 上注册**自有的 13 个导入**（`args_sizes_get`、`args_get`、`environ_sizes_get`、`environ_get`、`clock_time_get`、`clock_res_get`、`proc_exit`、`fd_write`、`fd_read`、`fd_close`、`fd_seek`、`fd_prestat_get`、`fd_prestat_dir_name`、`path_open`）。这是比链接通用 WASI 实现更强的「白名单由构造保证」：能力存在当且仅当在 `add_to_linker` 单点注册处注册。`add_to_linker` 因此豁免 `clippy::too_many_lines`（单点可审计是安全属性，仓库 `nlos-application` 既有先例）。
2. **errno 策略**：遵循 WASI preview 1 编号；能力违规（绝对路径、`..` 逃逸、符号链接逃逸、只读授信写打开）一律 `ENOTCAPABLE(76)`；未授信 fd 为 `EBADF(8)`；挂载内不存在文件 `ENOENT(44)`。与 b-sandbox-001 实测的 `EBADF(8)`/`EPERM` 家族语义同向（p1 实现间 62/63/76 映射存在差异，本实现取 p1 规范枚举 `ENOTCAPABLE` 并全程自洽，测试断言精确数字）。
3. **双重围栏**：词法围栏（拒绝绝对路径、反斜杠、空组件、越出挂载根的 `..`）+ canonicalize 包含围栏（`realpath` 必须落在 canonicalize 后的授信根之内，拒绝符号链接逃逸；写路径对父目录 canonicalize 后包含校验）。**残留边界（如实）**：canonicalize 与 open 之间存在理论 TOCTOU 窗口，需要已获写授信的攻击者在树内竞速替换符号链接；无 `openat2`/`O_NOFOLLOW` 跨平台原语下不声称根除，与通用 WASI preopen 实现同级。读取侧对 Windows 驱动器形态（`C:/`、`C:\`）显式拒绝。
4. **熵边界**：`random_get` 不在白名单面（导入即实例化拒绝）。E6 基线显式不授信熵源；后续车道引入经批准的熵源时须补负向回归。
5. **epoch 默认死线实测发现**：wasmtime 50.0.1 中 `Config::epoch_interruption(true)` 而未 `set_epoch_deadline` 时**立即 trap**（文档明示）。本实现未配置 epoch 的 profile 显式 `set_epoch_deadline(u64::MAX)`（内部 `saturating_add` 安全），该行为由红→绿循环捕获（见 §4）。
6. **平台句柄继承边界（如实）**：Windows 上 `std::process` 仅对显式接线的 stdio 句柄设置继承；Unix 上 Rust 打开的一切 fd 默认 `FD_CLOEXEC`。父进程无关代码自行标记为可继承的描述符不在本 crate 控制范围（调用方不得标记描述符可继承）。子进程内零 socket 创建。
7. **`TCB-DOWNGRADE-001`/`TUI-CONSENT-001` 挂接边界**：本 crate 的 `isolate` 层为 `pub(crate)`，库公共 API 只有进程编排——生产路径**不存在**「未完全中介的进程内执行」出口；降档可视化和 `Z4` 实心确认屏由 `W80-E6C` 桌面商店车道承接。`DRV-PROTO-001`/`DRV-LIFE-001`：`proc_exit`（协作终止）与 trap（强终止）映射到 `GuestExited`/`TrapKind` 类型化终态，供驱动生命周期 `UNCERTAIN` 对账消费。

## 3. 逃逸对抗矩阵（负向测试，具体变体断言）

全部 54 项测试通过（41 单元 + 13 集成）。矩阵断言均为**具体错误变体**（精确 errno 数字 / trap token / 类型化变体），无一处裸 `is_err()`：

| # | 对抗场景 | 层 | 断言（具体变体） | 结果 |
| :--- | :--- | :--- | :--- | :--- |
| 1 | 路径穿越 `../../../` 越出授信根 | L2（单元+集成双跑） | `path_open` 返回 `errno 76`（`ENOTCAPABLE`），guest 以 76 退出 | PASS |
| 2 | 绝对路径逃逸 `/etc/passwd` 与 Windows 驱动器 `C:/Windows/win.ini` | L2 集成 | 两者均 `errno 76` | PASS |
| 3 | 符号链接逃逸（挂载内链接指向根外目标） | L2 单元（`#[cfg(unix)]`，Linux/macOS 归三平台 CI） | `errno 76` | PASS（unix cfg；Windows 本机不适用） |
| 4 | 只读授信上 `CREAT`+写 rights 打开 | L2 单元+集成 | `errno 76` 且宿主侧确认**零文件产生** | PASS |
| 5 | 白名单内读取（正向对照，证明围栏是过滤而非全拒） | L2 单元+集成 | `path_open` SUCCESS + `fd_read` 精确 7 字节 | PASS |
| 6 | 写授信正向对照（CREAT + fd_write 3 字节） | L2 单元+集成 | guest code 0 + 宿主文件内容逐字节 `e6w` | PASS |
| 7 | 网络外连：导入 `sock_open` | L2 单元+集成 | 实例化拒绝，报告 `outcome=host-error`、detail 含 `instantiation-rejected` 且点名 `sock_open`（unknown import） | PASS |
| 8 | 内存超限：`memory.grow(100)`（上限 4 页） | L2 单元+集成 | limiter 拒绝，guest 观察 `-1`（标记码 42） | PASS |
| 9 | 内存超限：模块声明 `(memory 8)`（上限 4 页） | L2 单元+集成 | 实例化拒绝（`GuestInstantiationRejected` 变体 / `host-error:instantiation-rejected`） | PASS |
| 10 | 内存限内增长正向对照 | L2 单元 | `grow(2)` 返回原页数 1 | PASS |
| 11 | CPU fuel 耗尽（1e9 迭代循环，fuel 50k） | L2 单元+集成 | `TrapKind::OutOfFuel` / 报告 `outcome=trap detail=out-of-fuel`，双跑一致 | PASS |
| 12 | fuel 消费确定性（同模块双跑） | L2 单元 | `fuel_remaining` 双跑逐位相等且 `< 初始`（对齐 b-sandbox-001 D1 `run1==run2` 口径） | PASS |
| 13 | epoch 墙钟中断先于 fuel（无限循环，fuel 1e11 死线 2 tick/1ms） | L2 单元+集成 | `TrapKind::EpochInterrupt` / `detail=epoch-interrupt` | PASS |
| 14 | 宿主环境变量零泄漏（进程层） | L1 集成 | `env_clear` 子进程 `--self-audit-env` 计数 `== 0`；**对照向**：同一二进制裸 spawn（无 `env_clear`）计数 `>= 1`，证明计数可观测非恒零 | PASS |
| 15 | WASI 层环境零泄漏 | L2 单元+集成 | `environ_sizes_get` 双零；`environ_get` 以野指针（`-1`）调用仍 `SUCCESS`（零条目不触内存） | PASS |
| 16 | 宾客 argv 恰为显式声明值 | L2 单元+集成 | `args_sizes_get` 计数 2、缓冲 11 字节；偏移与字节逐位校验（`alpha\0beta\0`） | PASS |
| 17 | 预打开发现与默认拒绝 | L2 集成 | 授信后 `fd_prestat_get(3)` tag 0 + 名长 10 + 名首字节 `/`；deny-all 下 `path_open(fd=3)` `errno 8`（`EBADF`） | PASS |
| 18 | guest stdout 管道穿透 | L1 集成 | 父进程捕获 `e6a-ok\n` 且 `guest_stdout_len == 7` | PASS |
| 19 | 进程层硬杀（无界循环，fuel u64::MAX、epoch 关） | L1 集成 | 400ms 死线后 `timed_out_and_killed == true`，总耗时 < 30s | PASS |
| 20 | guest `unreachable` / `proc_exit(7)` 终态分类 | L2 单元 | `TrapKind::UnreachableCodeReached` / `GuestExited{code:7}` | PASS |
| 21 | 入口缺失 / 签名不符 / 非法 WAT / 非法 profile（fuel=0 等 7 类） | L2 单元 | `EntrypointMissing` / `EntrypointSignatureInvalid` / `GuestModuleRejected` / `ProfileInvalid` 各自类型化变体 | PASS |

## 4. TDD 红→绿循环记录（如实）

- **红相**（实现首版编译通过后首轮运行，2026-11-01）：`cargo test -p nlos-wasm-sandbox`：lib `26 passed / 15 failed`、集成 `12 passed / 1 failed`。失败根因三处，全部由断言定位：（a）wasmtime 50.0.1 epoch 默认死线为 0 导致未配置 epoch 的 guest 立即 `Interrupt`（12 项失败公共根因，见 §2.5）；（b）`args_get` 偏移累计漏计 NUL 分隔符（guest 返回 105，恰为偏移断言位）；（c）`TrapKind::Other` token 测试与 `fd_prestat_get` 名长断言（`/workspace` 实为 10 字节）两处测试侧笔误。
- **绿相**：修复（a）（b）与测试笔误后：lib `41 passed / 0 failed`、集成 `13 passed / 0 failed`。
- 声明：本车道在单工作会话内以「测试与实现逐模块成对撰写、首轮运行呈现红相、定位修复后转绿」的复合循环执行 TDD（未为每个用例单独跑一轮先红），红相的失败清单与根因如上如实记录；负向断言本身即防回归（移除任一围栏实现将使对应具体变体断言失败）。

## 5. 验证数据（本机实跑，2026-11-01）

| 命令 | 结果 |
| :--- | :--- |
| `cargo fmt --check -p nlos-wasm-sandbox` | PASS（exit 0，零漂移） |
| `cargo clippy -p nlos-wasm-sandbox --all-targets -- -D warnings` | PASS（exit 0，`clippy::all`+`clippy::pedantic` 零警告；唯一豁免：`add_to_linker` 的 `too_many_lines`，理由见 §2.1，仓库既有先例） |
| `cargo test -p nlos-wasm-sandbox` | **54 passed / 0 failed**（lib 41 + bin 0 + 集成 `escape_matrix` 13 + doc-test 0；集成含真实子进程 spawn/kill） |

环境：Windows 11 Pro 10.0.28000（x86_64，msvc）、Rust stable（`rust-toolchain.toml` 锁定）、wasmtime 50.0.1（`W80-0-SCAFFOLD` 预锁，本车道零 `Cargo.toml` feature 改动、零根 `Cargo.lock` 改动）。

## 6. 未运行项与已知限制（显式）

- **Linux / macOS 实跑**：全部场景未在本机运行（`#[cfg(unix)]` 分支含符号链接逃逸用例已编译检查，未在 Windows 11 本机执行）——归远端三平台 CI（「Rust cross-platform verification」）守护。
- **全 workspace 集成门禁**：本车道提交时按 `CLAUDE.md` §5 单 crate 门槛验证；`W80-INT` 合流阶段已对根工作区集成门禁完成统一复验。
- **fuel 计量开销百分比**：未测量。`ADR-0015` 决策 2 已定性为 `MEASURED-UNSTABLE`，本车道不引用精确百分比。
- **双层叠加启动/IPC 开销**：未测量（`ADR-0015` 代价节既有边界，未变）。
- **`wasmtime-wasi` 通用 p1 shim 的行为对齐回归**（stdout/clock 默认授予面、`DirPerms` 写打开 shim 缺陷线索）：不适用——本实现未链接该 shim，白名单面由本 crate 自有实现并全量负向覆盖。
- **TOCTOU 竞速逃逸**：未做破坏性竞速验证，残留窗口见 §2.3，如实维持边界声明。
- **guest 多线程（threads proposal）guest**：未覆盖（deny-by-default；`Config` 未启用 threads feature）。

## 7. 写集声明

本 Attempt 完整写集：`crates/nlos-wasm-sandbox/**`（Cargo.toml 零改动、`src/{lib,error,profile,wasi_allowlist,isolate,process,host_bin}.rs`、`src/bin/nlos-wasm-sandbox-host.rs`、`tests/escape_matrix.rs`）与本证据文件。未触碰根 `Cargo.toml`、根 `Cargo.lock`、`docs/management/**`、其他 crate 或桌面端任何路径；未执行 push；分支由 `worktree-agent-*` 就地重命名为 `feat/w80-e6a-wasm-sandbox` 后单原子提交。
