# C-BENCH-READINESS-20261116：分片基准的预置屏障与计时边界

- 状态：预置屏障及早退回收回归通过，修后原 Windows 吞吐目标通过。Ubuntu 主作业的吞吐失败根因仍待取证。
- 日期：2026-11-16；基线：`6415600db5c165a828108709a3dc57602169abaf`。
- 范围：[C-BENCH-001 §3](./c-bench-001-road-c002-distributed-benchmark.md) 的单机多进程基准；仅验证测试工具的同步契约，未新增性能或生产能力结论。规范依据见 [v0.5 §27、§28.3](../../design/06-架构设计总纲-v0.5.md#27-性能与工作负载类别)。

## 观察和修复

Windows CI [Run 43649840304 / job 132615620831](https://github.com/cty12356541/llmos/actions/runs/43649840304/job/132615620831) 的三轮 Regime B 都未满足 `ops_8 > 1.15 × ops_2`。其中两轮也未满足 `ops_4 > 1.15 × ops_2`，但三轮均满足 `ops_8 >= 0.80 × ops_4`。当时没有逐分片预置和工作时序，无法确定这次吞吐失败的原因。

旧 worker 建好 SQLite 与租约后立即写 `ready`，随后才落盘预置 16 个控制 Task 和 16 个控制 Group。父进程仅等待各 `ready` 文件出现便可发送 `go`，所以快分片可能已开始计时，慢分片仍在提交预置事务。更新后的[分片基准](../../../crates/nlos-task/tests/c_bench_task_sharded_scale.rs) 在最后一次预置事务成功返回后才写 `ready`。父进程仍等待所有分片的 `ready`，worker 仍等待 `go` 才执行计时工作。工作负载、SQLite WAL/FULL 持久性、吞吐和延迟阈值、三轮扫掠策略均未变更。

父进程等待就绪时也检查子进程状态。如果任一分片在 `go` 前退出，父进程报告异常分片和退出状态。清理对象从启动首个分片前就管理子进程，父进程提前退出时会先终止尚在运行的分片，再等待回收，避免已就绪分片持续等待。

## 计时诊断

`[c-bench-task-timing]` 按轮次、分片数和分片编号记录四个分片局部时长，统一使用纳秒：

- `preload_and_ready_ns`：开始控制数据预置，到 `ready` 文件写入调用返回。数据库初始化与租约建立发生在此区间之前。
- `ready_to_go_ns`：`ready` 写入调用返回，到 worker 观察到 `go`。
- `go_to_work_ns`：worker 观察到 `go`，到工作循环开始计时。
- `local_work_ns`：该分片工作循环的计时。WAL/FULL 复查、关闭 SQLite 和报告写入发生在局部计时之后。

每档额外打印 `active_span_lower_ns`（最长局部工作时长）、`parent_go_to_exit_ns`（父进程写 `go` 前到等待所有子进程退出后）与两者之差 `bound_gap_ns`。从最早工作开始到最晚工作完成的实际聚合跨度，至少覆盖最长局部工作时间，且在父进程的 go 到退出区间内。区间差也包含唤醒、调度和收尾开销，无法单独判定其中多少来自启动偏斜。各进程只比较自身单调时钟上的时点，因此日志提供上下界而无法给出精确跨进程跨度。原 `window_ms` 现明确打印门内使用的最长局部时长（至少 1ns），吞吐门继续使用这一分母。

## 验证

- 在 Windows、Rust 1.99.0 上新增 `shard_readiness_waits_for_last_durable_control_group`。分片准备函数使用真实 SQLite，在注册最后一个控制 Group 前通过通道暂停，测试在它暂停时检查父进程实际使用的 `ready` 文件。旧顺序实测失败（父侧提前看到了 `ready`）；修正后通过。测试通过通道确定阶段边界。
- 编译后运行单进程协议检查。看到真实子进程的 `ready` 后、写 `go` 前，独立只读 SQLite 连接读到 16 个已提交 Task 和 16 个已提交 Group；发 `go` 后，子进程完成一次工作循环，验证报告标记、样本和计时字段齐全。此验证未采样规模吞吐。
- `shard_readiness_failure_reaps_waiting_peer` 启动两个真实分片。先确认健康分片已就绪，再用缺失的父目录让第二个分片在写 `ready` 时退出。清理前，父进程已报告早退但健康分片仍在运行，外层回归测试失败。修正后，父进程报告同一早退且未发 `go`，两个原进程句柄都可立即读到退出状态。
- `nlos-task` 库单测 28 项通过。同测试文件的 6 项非性能测试通过（包括三类原有故障注入、原门校准单测、预置屏障回归和早退回收回归）。内部子进程入口保留 `ignored`，吞吐门从该次定向测试显式排除。该目标编译、Clippy 严格检查和包内格式检查通过。

需在空闲 Windows runner 定向复跑规模门并保留真实诊断；在原报告没有分片时序的情况下，不能将此次修复与原失败建立因果关系。若分片吞吐仍平坦，下一步需检查宿主磁盘、调度及代码访问串行化的证据。

## 2026-11-16 集成代码的 Ubuntu 主作业反例

准备及回收修正以 `862b5ca9` 合入。代码 SHA `4fc70be15a1ee848ebd99f9dad8771a328998ba7` 的 [Run 43744122670 / Ubuntu job 132903604897](https://github.com/cty12356541/llmos/actions/runs/43744122670/job/132903604897) 执行默认 profile 的 `cargo test --workspace` 时，`c_bench_task_sharded_scale` 得到 6 通过、1 失败、1 忽略；失败项为 `road_c002_sharded_spawn_message_commit_scales_without_global_lock_or_total_order`。预置屏障与早退回收回归均通过。

四个可见 CPU 上，三轮 Regime C 的 4→8 分片吞吐比分别为 `0.59x / 0.51x / 0.41x`（门限为至少 `0.80x`）。8 分片档最长局部工作约 `1.63 / 1.21 / 1.48 s`，比父进程 go 至回收的跨度仅短约 `13 / 13 / 50 ms`；第三轮三类操作的最大延迟约 `599–603 ms`。这些数据将调查集中到计时工作内的延迟长尾。宿主 CPU 调度、持久化 I/O 或 SQLite checkpoint 的具体影响尚未证实，也没有证据确认生产实现回归。原 Windows Regime B 的失败因果和精确跨进程活动跨度仍独立留界。

同 SHA 的 Ubuntu `cargo test --workspace --no-fail-fast -- --include-ignored` 执行了同一分片吞吐目标，外层该测试通过，套件 8/8 通过。成功时输出被捕获，现存日志未提供该样本的 Regime、吞吐或时序数字。Ubuntu 主作业的失败仍保留，两次结果差异尚无可验证归因。[include-ignored 作业 132903604849](https://github.com/cty12356541/llmos/actions/runs/43744122670/job/132903604849)

## 2026-11-16 修后 Windows 目标复验

同一集成代码 SHA `4fc70be15a1ee848ebd99f9dad8771a328998ba7` 在 Windows Server 2025 的主作业运行 `cargo test --workspace`。原分片吞吐目标通过，`c_bench_task_sharded_scale` 外层 7 通过、0 失败、1 忽略（内部子进程入口），预置屏障与早退回收回归均通过。成功输出未展示 Regime、吞吐或逐分片时序，历史 Regime B 失败的精确因果仍待取证。[Windows 主作业 132903604864](https://github.com/cty12356541/llmos/actions/runs/43744122670/job/132903604864)

## 2026-11-16 Ubuntu 独立诊断

诊断 workflow 提交 `ff5951b26b3ba5e8713bcba7eb8887944c041931` 在独立分支检出原源码 `4fc70be15a1ee848ebd99f9dad8771a328998ba7`，执行同一 debug 吞吐用例（`--exact --nocapture --test-threads=1`）。Ubuntu 26.04.1 镜像 `20261111.196.1`、Rust 1.99.0、4 个逻辑 CPU 上，首轮 Regime C 通过。1/2/4/8 分片为 `663.7 / 1363.6 / 1797.8 / 1849.5 ops/s`，4→8 为 `1.03x`，8 分片最大操作延迟为 `33.5 ms`。主作业约 `300–600 ms` 的长尾未复现。[诊断作业 132912095707](https://github.com/cty12356541/llmos/actions/runs/43746899436/job/132912095707) · [原始采样制品](https://github.com/cty12356541/llmos/actions/runs/43746899436/artifacts/14174922375)

观察器 200 ms 间隔取得 15 个快照，无漏采，8 分片档约 2 个点，观察器 CPU 成本约为一个核的 1.45%。该成功样本同时出现 CPU 等待与 I/O 等待。`/tmp` 所在 ext4 的实际挂载参数含 `journal_async_commit,nobarrier,commit=30,data=writeback`。原 SQLite WAL/FULL 断言保持启用，采样范围限该 runner 与测试条件，硬件掉电持久性未测。原失败台缺少可对比遥测，精确因果未知。后续在真实失败时保存同类 CPU、I/O、挂载与前序测试数据，或在固定宿主配对测量有无完整前序测试的条件。
