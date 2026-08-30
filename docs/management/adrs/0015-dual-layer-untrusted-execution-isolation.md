# ADR-0015：不可信代码执行采用双层隔离（独立 host Process + 进程内 Wasmtime 按信任档分层）

- 状态：ACCEPTED
- 日期：2026-08-30
- Owner：ProcessAuthority / B-SANDBOX
- 关联 Requirement：总纲 v0.5 §6 `[TCB-BYPASS-001]`、`[TCB-EGRESS-001]`、`[TCB-DOWNGRADE-001]`；§10.4 外部保证档位（GuaranteeTier）
- 关联工作包：`B-SANDBOX`（本决策定案归属）；`B-PROCESS`（host Process 生命周期与 fault domain 供给侧）；`B-RESOURCE`（enforcement shim 与 Reservation 消费侧）
- 决策来源：2026-08-30 编排会话按 exp8 数据定案；用户以既有「继续」推进授权批准 exp8 结论晋升（candidate → canonical，表述先例同既有决策批次）
- 证据：[b-sandbox-001-isolation-comparison](../../evidence/stage-b/b-sandbox-001-isolation-comparison.md)（PARTIAL_PASS，提交 `3df1603`；macOS 26.5.2 arm64 单平台 TESTED，Linux/Windows、Seatbelt、fuel 开销百分比未运行）
- 复审触发器：见文末四项

## 上下文

选型文档 §5 将 Wasmtime/WASI 定为不可信应用执行基线，要求 PoC 对比 in-process Wasmtime、Wasmtime 独立 host Process、native Process adapter 三形态，并给出默认安全建议：互不信任或要求独立强杀的组件采用「独立 Process + Wasmtime」双层隔离，in-process Wasmtime 只在对应 GuaranteeTier 下使用。规范侧，总纲 §6 规定了隔离必须防守的边界：`[TCB-BYPASS-001]`（不可信 Process 内不得暴露原始 provider、工具、云账户或控制面 credential）、`[TCB-EGRESS-001]`（strict domain 网络默认拒绝，继承 FD、socket、子进程等纳入旁路分析）、`[TCB-DOWNGRADE-001]`（未被完全中介的路径 MUST 降低 GuaranteeTier 并在 Receipt 和 UI 中可见）。

exp8（b-sandbox-001，提交 `3df1603` 落地为 candidate）在 macOS 26.5.2 arm64 单平台完成上述三形态关键面的四维实测：CPU 限制精度分层、内存 enforcement、capability 面、host 故障隔离。实测确认双层默认建议成立，并排除两个单层候选。本 ADR 记录该 candidate 晋升为 canonical 决策。

## 候选

| 候选 | 结论 |
|---|---|
| A. 双层隔离：独立 host Process + 进程内 Wasmtime | **采纳** |
| B. 仅进程内 Wasmtime | 否决：与宿主共享 fault domain，engine 侧 native crash/OOM 连带全部 guest 与宿主（D4 注记，DESIGN 级）；fuel/epoch 的配额精度优势（D1）不能替代进程边界的强杀终途与故障域切分，对互不信任场景精度收益买不回单层故障域 |
| C. 仅独立 host Process（native Process adapter） | 否决：macOS(arm64) 实测无可用内核内存配额原语（`RLIMIT_AS` setrlimit 返回 EINVAL、`RLIMIT_RSS` 为遗留 no-op，D2），`RLIMIT_CPU` 量子 1s 无法表达毫秒级配额（D1）；spawn 默认环境全继承（D3），须再补一层 capability 收敛，而该层正是 WASI/Wasmtime 已供给且实测收敛有效的能力，弃用即重复造轮且更弱 |

## 决定

1. **双层结构与职责切分（候选 A）**：互不信任或需独立强杀的不可信代码，一律以「独立 host Process 内嵌 Wasmtime」执行。进程层提供 fault domain 与 SIGKILL 终途（D4：子进程 abort、SIGTERM、SIGKILL 均不连带父进程）；Wasm 层提供细粒度资源配额与 capability 收敛（D1/D2/D3）。两层各承担一半隔离职责，任何单层不承担全部。
2. **CPU 限制精度分层**：确定性配额用 fuel（实测 fuel=10³/10⁴/10⁵ 恰好执行 67/667/6667 次迭代，双次运行逐一相等）；墙钟 deadline 用 epoch（macOS 本机有效 tick 1.488ms，按有效 tick 折算超调 0.18–0.93ms）；进程层 SIGKILL 轮询（超调中位 0–1ms、范围 0–4ms）与 `RLIMIT_CPU`（量子 1s）只作终途兜底，不承担毫秒级配额语义。fuel 计量开销为 MEASURED-UNSTABLE，任何 Claim 不得引用精确百分比。
3. **内存 enforcement 以 Wasm 层为准**：上限用 Wasmtime limiter 页级配额（64KiB 粒度、`memory.grow` 调用点确定性拒绝）。macOS(arm64) 上禁止以 `RLIMIT_AS` 充当内存配额（64MB/2GB setrlimit 均 EINVAL，子进程随后无限制分配至目标达成；`RLIMIT_RSS` 为遗留 no-op）。Linux rlimit 行为未实测，平台落地前必须补测（复审触发器 4）。
4. **capability 面以 WASI preopen + import 白名单收敛**：deny-all 实测 `path_open` 返回 8(EBADF)、env 空集；preopen 内路径逃逸 `../../etc/hosts` 返回 63(EPERM)。禁止 host Process 默认 spawn 的环境全继承形态（46 个 env、/etc/hosts 与 $HOME 可读、socket 与 TCP connect 可达），该形态与 `[TCB-EGRESS-001]` 默认拒绝及旁路分析要求直接冲突。native Process adapter 若引入必须补等价 capability 收敛层（exp8 建议 4）。
5. **故障隔离**：guest `unreachable` → Trap(Err) 由宿主捕获后继续运行，guest 乱写自身线性内存不损宿主哨兵数据；进程级强杀是超出版主模型时的最终手段。in-process Wasmtime 与宿主共享 fault domain（engine 原生崩溃/OOM 连带全部 guest，DESIGN 级论证），故仅限同信任域插件。
6. **信任档分层与 GuaranteeTier 挂接**：v0.5 已有档位语义：§10.4 定义 GuaranteeTier（OBSERVED/BOUNDED_RISK/STRICT），`[TCB-DOWNGRADE-001]` 要求未被完全中介的路径降低 GuaranteeTier 并在 Receipt 和 UI 可见。挂接方式：双层全中介（import 面白名单 + 进程边界 + 双层配额）是宣称较高档位的必要条件；仅进程内 Wasmtime 属未被完全中介路径，MUST 按 `[TCB-DOWNGRADE-001]` 降档声明，仅限同信任域插件。v0.5 尚无「信任档 → 隔离层组合」的枚举映射（§10.4 档位语义目前面向外部成本保证），该映射显式登记为后续规范工作，本 ADR 不发明新枚举。
7. **范围限定**：本决策是机制选型定案，不改变 v0.5 任何规范词状态，不冻结 wire/schema 契约。wasmtime-wasi 36 p1 shim 行为（默认即授予 stdout 与 clock，「默认全拒」朴素预期不成立；`DirPerms::READ` 只读 preopen 未拦截写打开的 shim 缺陷线索）须版本锁定并逐版本回归后，capability 收敛结论方可进入实现 Claim。

## 后果与退出策略

- 影响：`B-SANDBOX` 的 candidate 结论就此关闭；进程层 spawn/suspend/kill/fault domain 实现归 `B-PROCESS`（含其未决的 IsolationUnit 与 resource mapping），Wasm 层接线与 shim 版本锁定归后续沙箱实现切片（工作包在进度单登记，不在本 ADR 内创设）。
- 代价：双层组合的启动与 IPC 开销未实测（exp8 未测双层叠加延迟）；每个不可信组件一个 host Process，进程数量与调度成本随组件数增长，显式接受；fuel 开销无可靠百分比，只能声称同数量级。
- 证据边界（Claim 限定）：本 ADR 实测依据全部来自 macOS 26.5.2 arm64 单平台（wasmtime 36.0.14/wasi p1）；Linux/Windows、Seatbelt/App Sandbox、Mach task 内存限制、p2/Component Model 均未运行；in-process fault domain 连带为 DESIGN 论证、未做破坏性验证。引用本 ADR 的 Claim 不得超出该证据范围。
- 退出：若双层在实现中被证伪（平台补测推翻实测结论、双层开销不可接受、shim 缺陷无法版本锁定），以补记 ADR 修订，不重写历史。退化路径：in-process Wasmtime 仅限同信任域并按 `[TCB-DOWNGRADE-001]` 降档声明；进程层仅在具备可证内存配额与 capability 收敛层的平台上考虑单独使用。

## 复审触发器

1. **Wasm 统一快照需求**：Wasm Process 层与 fiber 层出现统一 checkpoint/restore 需求（同 [ADR-0009](0009-fiber-event-sourced-resume.md) 复审触发器 3 的对偶面），引擎快照与进程快照的职责边界重审。
2. **跨机迁移**：Stage C Process 迁移（总纲 §26.3）要求迁移 guest 执行态时，进程边界与 Wasm 快照的切分重开。
3. **Component Model / wasmtime p2 采用**：本 ADR 的 capability 面结论基于 p1 shim（wasmtime-wasi 36.0.14）版本锁定；升级改变 shim 行为即失效，capability 面重审并重新逐版本回归。
4. **真实掉电等层 3 级强故障反例**：进程层终途不可达、Wasm 层配额被绕过等双层假设失效的实测反例出现；或 Linux/Windows rlimit、Seatbelt 补测推翻 macOS 单平台结论（当前最大证据缺口）。
