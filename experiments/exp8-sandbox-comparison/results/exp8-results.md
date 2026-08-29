# exp8 原始结果（run at 1788009355339 ms, os=macos arch=aarch64 exp8=0.1.0）

| 维度 | 场景 | 指标 | 值 |
|---|---|---|---|
| D1-CPU | wasmtime-fuel(1000) | trap=OutOfFuel | true |
| D1-CPU | wasmtime-fuel(1000) | iters_run1 | 67 |
| D1-CPU | wasmtime-fuel(1000) | deterministic(run1==run2) | true |
| D1-CPU | wasmtime-fuel(10000) | trap=OutOfFuel | true |
| D1-CPU | wasmtime-fuel(10000) | iters_run1 | 667 |
| D1-CPU | wasmtime-fuel(10000) | deterministic(run1==run2) | true |
| D1-CPU | wasmtime-fuel(100000) | trap=OutOfFuel | true |
| D1-CPU | wasmtime-fuel(100000) | iters_run1 | 6667 |
| D1-CPU | wasmtime-fuel(100000) | deterministic(run1==run2) | true |
| D1-CPU | wasmtime-fuel-overhead(20M iters,5rep) | metered_samples_ms | [6.499375, 7.017875, 7.062208, 15.923917, 30.931583] |
| D1-CPU | wasmtime-fuel-overhead(20M iters,5rep) | unmetered_samples_ms | [4.708209, 4.708792, 4.713333, 7.934500000000001, 11.851666999999999] |
| D1-CPU | wasmtime-fuel-overhead | median_metered/unmetered/overhead_pct | 7.1/4.7/49.8 (MEASURED-UNSTABLE：跨次运行样本方差大于效应本身，本机噪声下不能给出可靠 fuel 开销百分比，仅可确认量级为同数量级) |
| D1-CPU | epoch-tick-calibration(nominal=1ms,300ms window) | effective_tick_ms | 1.497 |
| D1-CPU | wasmtime-epoch(deadline=10ms,tick=1ms) | trapped_by_epoch | true |
| D1-CPU | wasmtime-epoch(deadline=10ms,tick=1ms) | trapped_by_epoch | true |
| D1-CPU | wasmtime-epoch(deadline=10ms,tick=1ms) | trapped_by_epoch | true |
| D1-CPU | wasmtime-epoch(deadline=10ms) | elapsed_ms_median/overshoot_vs_nominal_ms/overshoot_vs_eff_tick_ms | 15.08/5.08/0.11 |
| D1-CPU | wasmtime-epoch(deadline=50ms,tick=1ms) | trapped_by_epoch | true |
| D1-CPU | wasmtime-epoch(deadline=50ms,tick=1ms) | trapped_by_epoch | true |
| D1-CPU | wasmtime-epoch(deadline=50ms,tick=1ms) | trapped_by_epoch | true |
| D1-CPU | wasmtime-epoch(deadline=50ms) | elapsed_ms_median/overshoot_vs_nominal_ms/overshoot_vs_eff_tick_ms | 74.41/24.41/-0.44 |
| D1-CPU | wasmtime-epoch(deadline=100ms,tick=1ms) | trapped_by_epoch | true |
| D1-CPU | wasmtime-epoch(deadline=100ms,tick=1ms) | trapped_by_epoch | true |
| D1-CPU | wasmtime-epoch(deadline=100ms,tick=1ms) | trapped_by_epoch | true |
| D1-CPU | wasmtime-epoch(deadline=100ms) | elapsed_ms_median/overshoot_vs_nominal_ms/overshoot_vs_eff_tick_ms | 150.10/50.10/0.40 |
| D1-CPU | process-kill(deadline=10ms,poll=1ms,SIGKILL,3rep) | cpu_overshoot_ms_median(min/max见json) | 1.00 |
| D1-CPU | process-kill(deadline=10ms) | cpu_overshoot_all_ms | [0.0, 1.0, 2.0] |
| D1-CPU | process-kill(deadline=50ms,poll=1ms,SIGKILL,3rep) | cpu_overshoot_ms_median(min/max见json) | 1.00 |
| D1-CPU | process-kill(deadline=50ms) | cpu_overshoot_all_ms | [0.0, 1.0, 1.0] |
| D1-CPU | process-kill(deadline=100ms,poll=1ms,SIGKILL,3rep) | cpu_overshoot_ms_median(min/max见json) | 1.00 |
| D1-CPU | process-kill(deadline=100ms) | cpu_overshoot_all_ms | [-1.0, 1.0, 1.0] |
| D1-CPU | process-rlimit-cpu(soft=2s,quantum=1s) | actual_cpu_s/wall_s/terminated | 2.009/2.010/true |
| D1-CPU | process-rlimit-cpu | note | RLIMIT_CPU 以秒为量子，无法表达毫秒级配额；macOS/Linux 同为 1s 粒度 |
| D2-MEM | wasmtime-limiter(max=10 pages) | grow_to(100) denied(ret=-1) | true |
| D2-MEM | wasmtime-limiter(max=6 pages) | grow_to(5)=ok(1>=0) then grow_to(1)=denied | true |
| D2-MEM | wasmtime-limiter | granularity | 1 wasm page = 64 KiB，拒绝发生在 memory.grow 调用点（内联、确定性） |
| D2-MEM | process-baseline(no rlimit, target 256MB) | exit(ok/signal/abort) | ok |
| D2-MEM | process-baseline(no rlimit, target 256MB) | reached_target | true |
| D2-MEM | process-baseline(no rlimit, target 256MB) | setrlimit | setrlimit_as=absent |
| D2-MEM | process-baseline(no rlimit, target 256MB) | last_progress | chunk=16 mb=256 maxrss_kb=22768 |
| D2-MEM | process-rlimit-as-LOW(64MB, target 256MB) | exit(ok/signal/abort) | ok |
| D2-MEM | process-rlimit-as-LOW(64MB, target 256MB) | reached_target | true |
| D2-MEM | process-rlimit-as-LOW(64MB, target 256MB) | setrlimit | setrlimit_as=ERR:EINVAL: Invalid argument limit_mb=64 |
| D2-MEM | process-rlimit-as-LOW(64MB, target 256MB) | last_progress | chunk=16 mb=256 maxrss_kb=22800 |
| D2-MEM | process-rlimit-as-HIGH(2GB, target 4GB) | exit(ok/signal/abort) | ok |
| D2-MEM | process-rlimit-as-HIGH(2GB, target 4GB) | reached_target | true |
| D2-MEM | process-rlimit-as-HIGH(2GB, target 4GB) | setrlimit | setrlimit_as=ERR:EINVAL: Invalid argument limit_mb=2048 |
| D2-MEM | process-rlimit-as-HIGH(2GB, target 4GB) | last_progress | chunk=16 mb=4096 maxrss_kb=268560 |
| D2-MEM | process-rlimit-as | platform_analysis | 实测：macOS(arm64) 上 setrlimit(RLIMIT_AS) 直接返回 EINVAL（任何值都无法设置），子进程随后无限制分配至 256MB/4GB 目标——macOS 上 host-process 侧不存在可用的内核级内存配额原语（RLIMIT_RSS 亦为历史遗留 no-op），需 Mach task limit 等外部机制（未测）。对比 wasmtime limiter：64KiB 页粒度、仅约束 guest 线性内存、在 memory.grow 调用点内联确定性拒绝、跨平台一致。Linux 的 RLIMIT_AS 在 mmap 路径生效（DESIGN 引用，本实验未实测 Linux） |
| D3-CAP | wasmtime-wasi(deny-all: 无 preopen/无显式 env；v36 实测默认含 stdout+clock) | clock_time_get | 0(SUCCESS) |
| D3-CAP | wasmtime-wasi(deny-all: 无 preopen/无显式 env；v36 实测默认含 stdout+clock) | fd_write(stdout) | 0(SUCCESS) |
| D3-CAP | wasmtime-wasi(deny-all: 无 preopen/无显式 env；v36 实测默认含 stdout+clock) | path_open(无 preopen) | 8(EBADF) |
| D3-CAP | wasmtime-wasi(deny-all: 无 preopen/无显式 env；v36 实测默认含 stdout+clock) | environ_sizes_get(空集 0 变量) | 0(SUCCESS) |
| D3-CAP | wasmtime-wasi(deny-all: 无 preopen/无显式 env；v36 实测默认含 stdout+clock) | path_open(路径逃逸, 无 preopen) | 8(EBADF) |
| D3-CAP | wasmtime-wasi(grant: stdout+只读 preopen scratch) | fd_write(stdout) | 0(SUCCESS) |
| D3-CAP | wasmtime-wasi(grant: stdout+只读 preopen scratch) | path_open(写打开只读 preopen 内的 secret.txt) | 0(SUCCESS) |
| D3-CAP | wasmtime-wasi(grant: stdout+只读 preopen scratch) | path_open(路径逃逸 ../../etc/hosts) | 63(EPERM) |
| D3-CAP | wasmtime-wasi(grant: stdout+只读 preopen scratch) | clock_time_get | 0(SUCCESS) |
| D3-CAP | wasmtime-wasi(grant: stdout+只读 preopen scratch) | environ_sizes_get(空集 0 变量) | 0(SUCCESS) |
| D3-CAP | host-process(默认 spawn：完整 ambient authority) | env_count | 69 |
| D3-CAP | host-process(默认 spawn：完整 ambient authority) | fd_count | 4 |
| D3-CAP | host-process(默认 spawn：完整 ambient authority) | etc_hosts | OK(size=213) |
| D3-CAP | host-process(默认 spawn：完整 ambient authority) | home_stat | OK(size=2336) |
| D3-CAP | host-process(默认 spawn：完整 ambient authority) | socket_create | OK |
| D3-CAP | host-process(默认 spawn：完整 ambient authority) | tcp_connect | REFUSED(syscall permitted) |
| D3-CAP | host-process(默认 spawn：完整 ambient authority) | tmp_write | OK |
| D3-CAP | host-process(默认 spawn：完整 ambient authority) | maxrss_kb | 6448 |
| D3-CAP | 对比口径 | wasmtime 敏感操作成功数（默认/显式授予） | 由上方 errno 判定；process 侧 env/fd/文件/socket/tmp-write 默认全部可用 |
| D4-FAULT | wasmtime(guest unreachable) | trap | UnreachableCodeReached |
| D4-FAULT | wasmtime(trap 后复用同一 instance) | spin(1000) 正常完成 | true |
| D4-FAULT | wasmtime(guest scribble 自身 memory 0..256) | host 哨兵 Vec 完整 | true |
| D4-FAULT | wasmtime(in-process) 设计注记 | fault_domain | guest 无法 segfault host（沙箱内存受检、trap 可恢复），但与 host 同进程：engine/宿主侧原生崩溃或 OOM 会连带全部 guest——此为设计论证（DESIGN），本实验未主动崩溃 host 验证 |
| D4-FAULT | process-child(abort) | exit_signal | SIG6 |
| D4-FAULT | process-child(abort) | parent_survived_and_reaped | true(detect_ms=53.0) |
| D4-FAULT | process-child(SIGTERM 免疫) | alive_after_SIGTERM_200ms | true |
| D4-FAULT | process-child(升级 SIGKILL) | killed_and_reaped | true(reap_ms=0.0, exit_ok=false) |
| D4-FAULT | process 隔离语义 | note | OS 提供 SIGKILL 这条不可屏蔽终途（仅 D-state 例外）；Wasmtime 侧无需 kill：fuel/epoch 保证循环可中断，但不存在'强杀单个 guest 后宿主继续'的等价 OS 语义（卸载靠 host 逻辑） |
