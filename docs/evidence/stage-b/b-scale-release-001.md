# B-SCALE-RELEASE-001：release-profile 多平台规模重测管线 + 首批同机对照数字（移交#5 / C-SCALE-RELEASE 前片）

> 对应：[stage-c-progress §C.5.2 移交#5](../../management/stage-c-progress.md)（release-profile 多平台规模复测 + CI 化，W36 lane C）；[RISK-B-07](../../management/exit-unknown-risks.md)（scale-impersonation）退役证据行 / U-1（release-profile 与多平台规模行为未知——现有数字全部 debug/test、单平台 macOS）；gate 文本禁令（「PID 级 Agent 容量」「coroutine 级大规模并发」生产量级声明以 release-profile + 多平台复测为前置）。**本文件只建测量管线 + 记首批数字——不翻转任何生产量级声明**；U-1 的多平台半边待首 CI run 回填，在此之前 RISK-B-07 口径限定不变。
>
> 状态：`PIPELINE ESTABLISHED`（CI job 落地 + 本地首批 release 数字 + 校准裁决零调整）；首 CI run `PENDING`（控制器合并后 dispatch，本 lane MUST NOT 触发）。

## 1. 范围与诚实规则（先行声明）

1. **探针家族五二进制**（与派工单一致）：`nlos-task` `scale_profile`（默认套件）+ `scale_profile_probe`（10K/100K `#[ignore]`）；`nlos-plan` `tasknode_scale_probe`（默认 smoke + 10K/100K）；`nlos-runtime-tokio` `lifecycle_scale` / `activation_meter_scale` / `batch_cancel_scale`（各 10K/100K）。探针代码与断言**零改动**——本 lane 换的是 profile 与平台矩阵，不是测量方法。
2. **显式不含**（后续车道按需扩，非本 lane 缺口）：`working_set_ratio_probe`、`reclaim_execution_probe`、`checkpoint_rehydrate_scale`、`durable_wait_scale`、`outbox_wake_latency`、`nlos-store` `store_scale` 的 release 化。
3. **同机对照纪律**：debug 与 release 同日同机背靠背实跑（macOS arm64 M5 10 核，stable 1.97.1），非跨 run 拼接。既有登记 debug 数字（b-runtime-002 §6.18.2、b-task-scale-001 §12、b-plan-001 §9.3 等）与本处同机同类但负载相不同——release↔debug delta 以本文件 §4 同日对表为准。
4. **release = cargo 默认 release profile**：workspace `Cargo.toml` 无 `[profile.release]` 自定义段——opt-level 3、无 debug assertions、未开 LTO。任何后续 profile 调优（LTO/codegen-units）都改变被测对象，须另立口径。
5. **不发布声明**：本文件全部数字仅证明「release 下机制与形状断言成立、量级记录在案」；生产量级声明（PID 级/coroutine 级）维持禁令至多平台 CI 数字回填且通过独立评审。

## 2. 管线（CI job，deliverable 1）

`.github/workflows/rust-cross-platform.yml` 新增 job `scale-probe-release`：

- **门控**：`if: github.event_name == 'schedule' || github.event_name == 'workflow_dispatch'`——与既有 debug `scale-probe` job 同门，push/PR 一律跳过；夜间 cron 与手动 dispatch 触发（dispatch 由控制器 post-merge 执行）。
- **矩阵**：`ubuntu-latest` / `windows-latest` / `macos-latest`，`fail-fast: false`。
- **成本纪律**：`-p <crate> --test <probe>` 精确钉探针 test 目标（连带其 lib 依赖链编译），**不做全 workspace release 构建**——三步分别覆盖 task / plan / runtime 三家族。
- **可观测性**：cargo 级 `--no-fail-fast`（单二进制失败不掩盖后续二进制——§6.18.2 教训沿用）+ `--nocapture`（探针 profile 行是本管线的测量数字，必须进 run 日志；debug scale-probe job 无 `--nocapture`，数字仅失败时可见，测量管线反转该默认）。
- **家族隔离**：后两步 `if: ${{ !cancelled() }}`——前序家族失败不截断后续家族的结果落账，job 整体仍红。
- **缓存**：`Swatinem/rust-cache@v2` 带 `key: scale-release-${{ runner.os }}` 独立前缀（msrv job 同款纪律），避免 release 工件与同 workflow debug 工件混写缓存键。
- **RSS/线程读数口径（跨平台如实）**：探针读数 macOS 走 `ps`、Linux 走 `/proc/self/status`、**其余平台（windows）cfg 兜底返回 0/None**——windows 腿的 RSS 种群比例界与线程界形同空转（vacuous），吞吐/比值/终态唯一/reclaim 硬界仍真实；与 debug 管线同口径，非本 lane 引入。

## 3. 校准裁决（deliverable 3）：零常量调整

逐常量映射（§6.18 纪律在 release 下的复核）：

| 探针 | 常量 | 语义 | release 敏感性 | 裁决 |
|---|---|---|---|---|
| `lifecycle_scale` | `MIN_BACKPRESSURE_WAIT`/`MIN_SUSPENDED`=40ms | 每纤下限 | 无——由 50ms phase sleep 锚定，与优化级别无关 | 不动 |
| `lifecycle_scale` | `SATURATION_ACTIVE_PER_FIBER`=40ms 饱和门 | 亚阈才断言「等待主导」比值 | release 收缩 spawn 窗（本机 91.7ms→16.1ms）→ avg_active 落亚阈 → **比值断言更常执行（更强）**，本机实证 §4 | 不动 |
| `lifecycle_scale` | 30s 相位预算 / `THREAD_BOUND`=10 | 墙钟上限/线程界 | release 严格更快；线程数与优化无关 | 不动 |
| `activation_meter_scale` | `MIN_ACTIVE_CPU`=10ms + `COMPUTE_TARGET`=25ms | active_cpu 下限 | 无——busy-loop 按墙钟锚定；本机两 profile 相位墙钟均为 ~12.8s（≈1000×25ms÷2 workers，逐字一致） | 不动 |
| `activation_meter_scale` | `MIN_EXTERNAL_WAIT`=40ms | external_wait 下限 | 无——50ms sleep 锚定 | 不动 |
| `batch_cancel_scale` | `SUPERLINEAR_RATIO_BOUND`=3.0 / `RSS_PER_FIBER_KIB`=8 / 线程增长 +4 | 种群比例界 | 相对面（100K vs 10K 同 profile 内比值），与优化无关；本机 release 比值 1.12× | 不动 |
| `scale_profile_probe` | 惰性比值 ≤16× 且 p95<100ms | 比值+绝对上限 | release 双面（基线/规模）同向变快，比值落 0.90×/0.95× | 不动 |
| `tasknode_scale_probe` | `LAZY_RATIO`=16 / 100ms 上限 / 每节点 ≤4096B | 同上 + durable 界 | durable 字节界与 profile 无关（本机每节点 508B/554B 两 profile 逐字一致） | 不动 |

**结论**：全部下限由 sleep/墙钟目标锚定（profile 无关），全部形状断言为比值或种群比例界（两面同 profile 相消），全部上限为宽裕墙钟预算（release 严格更快）。release 的制度迁移方向 = spawn/计算窗收缩 → 饱和门更容易开着 → 比值断言从「gate 跳过」变为「执行并通过」（本机 lifecycle 10K 实证，§4）——**没有任何界需要放宽，也没有发现需要收紧的假阳性面**。若首 CI run 在 2-vCPU runner 上出现倒挂类假失败，按 §6.18 先例修测量免疫力（如 lifecycle 加 `PROBE_SERIALIZE` 槽），不放宽界（§7 deferred #4）。

## 4. 本地首批数字（deliverable 2，原样誊录）

命令族（同日同机背靠背，macOS arm64 M5 10 核，stable 1.97.1；debug 先、release 后）：

```text
cargo test -p nlos-task --test scale_profile --test scale_profile_probe -- --include-ignored --nocapture
cargo test -p nlos-plan --test tasknode_scale_probe -- --include-ignored --nocapture
cargo test -p nlos-runtime-tokio --test lifecycle_scale --test activation_meter_scale --test batch_cancel_scale -- --include-ignored --nocapture
（release 组同三命令，前缀 cargo test --release）
```

### 4.1 release profile 行（原样誊录）

```
10K task profile (single platform): registrations=10000 register_total=2.341244375s permit_p50_100=230.875µs permit_p95_100=536.75µs permit_p50_10k=344.916µs permit_p95_10k=508.041µs permit_max_10k=2.222584ms working_set=512 working_set_total=148.588709ms working_set_p95=434.708µs inspect4=128.541µs database_bytes=7704576
100K task profile (single platform): registrations=100000 register_total=36.906445834s permit_p50_100=251.125µs permit_p95_100=464.042µs permit_p50_100k=229.209µs permit_p95_100k=419.958µs permit_max_100k=3.333167ms working_set=5120 working_set_total=2.255397708s working_set_p95=703.5µs database_bytes=70062080
10K logical TaskNode profile (single platform): nodes=10000 apply_total=671.5925ms resolve_total=19.3335ms resolved_view_total=8.461625ms inspect_p95_scale=14.25µs residency_p95_scale=10.75µs per_node_after_apply=514 per_node_after_activity=560
100K logical TaskNode profile (single platform): nodes=100000 apply_total=4.897469625s resolve_total=201.310167ms resolved_view_total=133.543958ms inspect_p95_scale=16.916µs residency_p95_scale=20µs per_node_after_apply=508 per_node_after_activity=554
10000-fiber activation-meter profile: spawn_issue=15.274667ms park_settle=24.435709ms external_wait_sleep=50ms rss_kib=114320 threads=7 total=100.639416ms（active_cpu_phase=12.806154625s）
100000-fiber activation-meter profile: spawn_issue=153.952ms park_settle=115.413667ms rss_kib=206736 threads=4 total=326.492667ms（active_cpu_phase=12.824211375s）
10000-fiber batch-cancel profile: spawn_issue=10.982125ms linkage=2.481999ms settle=17.542333ms join_reap=2.205917ms per_fiber_cancel_phase=2.761µs（rss 增长 16208 ≤ 80000；threads 5→5）
100000-fiber batch-cancel profile: spawn_issue=139.432209ms linkage=29.95925ms settle=233.517959ms join_reap=39.00325ms per_fiber_cancel_phase=3.098µs（rss 增长 136944 ≤ 800000；threads 5→5）
batch-cancel O-family ratio: per-fiber cancel phase 2.761µs @10K vs 3.098µs @100K → 1.12x (linear-or-better bound 3x)
10000-fiber backpressure_wait profile: spawn_issue=16.102084ms enter_backpressure=2.053459ms phase_sleep=50ms rss_kib=111296 threads=7 total=80.896709ms（无 SKIP 行——比值断言执行并通过）
10000-fiber suspended profile: spawn_issue=15.699833ms total=75.819875ms
100000-fiber backpressure_wait profile: spawn_issue=128.956917ms park_settle=10.491834ms rss_kib=193072 threads=4 total=257.905958ms
100000-fiber suspended profile: spawn_issue=136.95775ms total=254.169042ms
```

### 4.2 同日同机 release↔debug 对表

| 面（档位） | debug（同日） | release（同日） | δ（release/debug） |
|---|---|---|---|
| task 注册 100K register_total | 61.568s | 36.906s | 0.60× |
| task permit p95_100k（惰性面） | 1.915ms | 420µs | 0.22× |
| task permit 惰性比值 p95_100k÷p95_100 | 2.03× | 0.90× | 断言 ≤16× 两 profile 均绿 |
| task working-set 100K p95 | 2.102ms | 703.5µs | 0.33× |
| plan 100K apply_total | 15.178s | 4.897s | 0.32× |
| plan 100K resolve_total | 1.304s | 201.3ms | 0.15× |
| plan inspect p95_scale（100K 库） | 54.5µs | 16.9µs | 0.31× |
| plan 每节点 durable 字节（100K 终态） | 554B | 554B | 1.00×（durable 界与 profile 无关的实证） |
| activation external 相位总墙钟（100K） | 1.206s | 326.5ms | 0.27× |
| activation active_cpu 相位 | 12.86s | 12.82s | ≈1.00×（墙钟目标锚定，by design） |
| batch-cancel per_fiber（100K） | 28.408µs | 3.098µs | 0.11× |
| batch-cancel O 比值（100K÷10K） | 1.42× | 1.12× | 界 3× 两 profile 均绿 |
| lifecycle 10K spawn_issue | 91.69ms | 16.10ms | 0.18× |
| lifecycle 100K backpressure 相位总墙钟 | 1.093s | 257.9ms | 0.24× |
| lifecycle 10K「等待主导」比值断言 | **gate 跳过**（avg active 88.98ms ≥ 40ms 饱和门；同二进制 100K spawn 风暴并行污染） | **断言执行并通过**（avg active < 40ms） | 断言面更强 |
| RSS 面（记录） | lifecycle 100K 163.7MiB；batch-cancel 100K 增长 136928KiB | 193.1MiB；136944KiB | release 略高/持平（内联体积）；比例界两 profile 均绿 |

要点：

1. **全部断言族两 profile 同绿**：下限（sleep/墙钟锚定）、比值（惰性 ≤16×、O 族 ≤3×、等待主导）、种群比例界（RSS 8KiB/fiber、线程 ≤+4）、durable 界（每节点 ≤4096B 实测 508–560B）。零失败、零跳过新增（debug lifecycle 10K 的 gate 跳过是 §6.18.2.1 既有合法路径）。
2. **release 加速集中在 CPU 面**（per-fiber cancel 0.11×、resolve 0.15×、spawn 0.18×、permit 惰性面 0.22×）；**fsync 主导面收敛到 ~0.6×**（task 注册 100K：61.6s→36.9s——APFS fsync 地板，与 §5 已知限制同源）；**墙钟锚定面不动**（active_cpu 相位、50ms phase sleep）。该分布本身即「哪些面是算法成本、哪些是 IO 成本」的机制性证据。
3. **lifecycle 10K release 下比值断言执行**：spawn 窗收缩 5.7× 使零功人口的 avg active（排队时延）落回 40ms 饱和门之下——§6.18 家族第三例裁决的「有主机余量时形状断言」在 release 下更常真实执行，CI 2-vCPU 上仍可能走 gate 跳过（合法路径，非失败）。

## 5. 验证门（本 lane 实跑）

| 门 | 命令 | 结果 |
|---| --- | --- |
| debug 基线（task） | `cargo test -p nlos-task --test scale_profile --test scale_profile_probe -- --include-ignored --nocapture` | PASS（14+2 passed；probe 二进制 74.62s，串行槽生效：100K 档先完） |
| debug 基线（plan） | `cargo test -p nlos-plan --test tasknode_scale_probe -- --include-ignored --nocapture` | PASS（4 passed，17.18s） |
| debug 基线（runtime 三族） | `cargo test -p nlos-runtime-tokio --test lifecycle_scale --test activation_meter_scale --test batch_cancel_scale -- --include-ignored --nocapture` | PASS（2+2+2 passed；lifecycle 10K 比值走饱和门跳过， Floors/线程/RSS/聚合下限全断言） |
| release（task） | 同命令前缀 `--release` | PASS（14+2 passed；probe 二进制 44.02s） |
| release（plan） | 同上 | PASS（4 passed，5.38s） |
| release（runtime 三族） | 同上 | PASS（2+2+2 passed；lifecycle 10K 比值断言执行） |
| workflow yaml | `python3 -c "yaml.safe_load(...)"` | 解析通过（jobs/if/matrix/steps 结构核对） |
| evidence index | `python3 scripts/lint_claims.py` | PASS（本文件 + 索引行双向一致） |
| fmt/clippy | — | 本 lane 零 Rust 代码改动，无适用面（探针与 src 逐字节未动，`git diff` 仅 yaml/md） |

## 6. PENDING：首 CI run 必须呈现什么（dispatch 由控制器执行）

1. 三 OS 矩阵腿（ubuntu/windows/macos）`Scale probe release (...)` job 全绿；同 run 内 debug `scale-probe` 与既有 job 不受影响。
2. 各腿日志含三家族 release profile 行（`--nocapture`）：task 注册/permit/working-set 行 ×2 档、plan declare/profile 行 ×2 档、runtime 10K/100K 各相位行 + batch-cancel O 族比值行。
3. lifecycle 10K 在安静窗下比值断言执行；若 2-vCPU 饱和仍走 gate 跳过（打印 SKIP 理由行），属合法路径——区分「跳过」与「失败」。
4. windows 腿：RSS/线程读数为 0/None（cfg 兜底）→ 内存/线程比例界空转；吞吐/比值/终态唯一/reclaim 硬界仍真实——与 §2 口径一致，非失败。
5. ubuntu 腿 `scale_profile_probe` 100K 档 fsync 主导，预计与 debug 先例（二进制 ~725s）同量级或更快；若假失败按 §6.18 纪律排查测量窗，不放宽界。
6. 回填：run 链接 + 三平台关键数字（对 §4.2 表补 Linux/Windows 列）落本文件 §6.1 后，U-1 的「多平台」半边才可评估；RISK-B-07 退役评审另行发起，本 lane 不代裁。

## 7. 仍属缺口 / deferred minors（如实登记）

1. **未 release 化家族**：§1.2 所列六个探针二进制不在本管线——每扩一族须先过同机 release 对照（同 §4 纪律），再向 job 加一步（一行成本）。
2. **windows RSS/线程读数空转**：cfg 兜底 0/None；补 windows 真读数（如 `GetProcessMemoryInfo`）归后续车道——本 lane 不改探针代码。
3. **首批数字单机**：多平台数字 PENDING 首 CI run（§6）；生产量级声明禁令维持。
4. **`lifecycle_scale` 二进制内两档并行**（无 `PROBE_SERIALIZE` 槽，与 debug 夜间同口径）：release 下 spawn 窗收缩使 10K 更可能落亚阈；若 CI release 出现倒挂类假失败，按 §6.18 同法加槽（届时登记），本 lane 不预改——保持与 debug 管线同形状的诚实对照。
5. **无 profile 调优基线**：默认 release（无 LTO 等）；若未来引入 `[profile.release]` 自定义，本文件全部 release 数字即失效，须复测。
6. **debug 对照相的负载披露**：本地同日跑受同机并行负载影响（house 常态），绝对值有噪声、比值与断言结论不受影响；CI 数字落地后以 CI 画像为准。

## 8. 未运行项（显式列出）

- **push / PR / workflow dispatch**：MUST NOT（派工单明令；控制器 post-merge 统一执行）。
- **`cargo test --workspace`（任何 profile）**：MUST NOT（波次屏障；本 lane 以三 crate 探针目标为验证面）。
- **CI 多平台实跑**：PENDING（§6）。
- **探针/ src Rust 代码改动**：无——校准裁决零调整（§3），`cargo fmt`/clippy 无适用面。

## 9. Base HEAD / 写集

- 开工 `7d3ad53`（分支 `feat/w36-p5`，W36 lane C）。
- 写集：`.github/workflows/rust-cross-platform.yml`（新增 `scale-probe-release` job）、本文件、`docs/management/evidence-index.yaml`（本文件行）。零 Rust 代码改动。
