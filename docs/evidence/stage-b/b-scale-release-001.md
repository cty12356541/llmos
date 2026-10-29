# B-SCALE-RELEASE-001：release-profile 多平台规模重测管线 + 首批同机对照数字（移交#5 / C-SCALE-RELEASE 前片）

> 对应：[stage-c-progress §C.5.2 移交#5](../../management/stage-c-progress.md)（release-profile 多平台规模复测 + CI 化，W36 lane C）；[RISK-B-07](../../management/exit-unknown-risks.md)（scale-impersonation）退役证据行 / U-1（release-profile 与多平台规模行为未知——现有数字全部 debug/test、单平台 macOS）；gate 文本禁令（「PID 级 Agent 容量」「coroutine 级大规模并发」生产量级声明以 release-profile + 多平台复测为前置）。**本文件只建测量管线 + 记首批数字——不翻转任何生产量级声明**；首 CI 多平台 `--release` 实跑数字已于 `2026-10-29` `W76` 回填至 §6.1（闭合 `U-1` 多平台 CI `--release` 探针半边），**但本文件仍不翻转任何无边界生产量级口号（`PID 级 Agent 容量` / `coroutine 级大规模并发` 禁令维持，`1M_LOGICAL` 裸金属/跨机 WAN 边界仍在，`RISK-B-07` 维持 `partial` 由管理台账独立治理）**。
>
> 状态：`PIPELINE & MULTI-PLATFORM CI BASELINE VERIFIED`（W36 CI job 落地 + 本地首批 release↔debug 对照 + `2026-10-29` `W76` `workflow_dispatch` Run [40868357139](https://github.com/cty12356541/llmos/actions/runs/40868357139) 三平台 13/13 全绿实跑回填 §6.1；校准裁决零调整；`workflow_dispatch` 不冒充 `schedule` 事件）。

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

## 6. 首 CI run 验收判据与多平台实跑回填

1. 三 OS 矩阵腿（ubuntu/windows/macos）`Scale probe release (...)` job 全绿；同 run 内 debug `scale-probe` 与既有 job 不受影响。
2. 各腿日志含三家族 release profile 行（`--nocapture`）：task 注册/permit/working-set 行 ×2 档、plan declare/profile 行 ×2 档、runtime 10K/100K 各相位行 + batch-cancel O 族比值行。
3. lifecycle 10K 在安静窗下比值断言执行；若 2-vCPU 饱和仍走 gate 跳过（打印 SKIP 理由行），属合法路径——区分「跳过」与「失败」。
4. windows 腿：RSS/线程读数为 0/None（cfg 兜底）→ 内存/线程比例界空转；吞吐/比值/终态唯一/reclaim 硬界仍真实——与 §2 口径一致，非失败。
5. ubuntu 腿 `scale_profile_probe` 100K 档 fsync 主导，预计与 debug 先例（二进制 ~725s）同量级或更快；若假失败按 §6.18 纪律排查测量窗，不放宽界。
6. 回填：run 链接 + 三平台关键数字（对 §4.2 表补 Linux/Windows 列）落本文件 §6.1 后，U-1 的「多平台」半边才可评估；RISK-B-07 退役评审另行发起，本 lane 不代裁。

### 6.1 多平台 CI release 实跑回填（2026-10-29 W76，Run 40868357139）

- **工作流与运行实例**：[`.github/workflows/rust-cross-platform.yml`](../../../.github/workflows/rust-cross-platform.yml)（`Rust cross-platform verification`）— Run [40868357139](https://github.com/cty12356541/llmos/actions/runs/40868357139)（`2026-10-29`，`W76`）。
- **触发方式与目标提交**：控制器在 `HEAD = ed64ae3` 上手动执行 `workflow_dispatch`（显式声明：本次实跑由 `workflow_dispatch` 手动触发，**不冒充夜间定时 `schedule` 事件触发**）。
- **全工作流矩阵结论**：**13/13 作业全部 `completed / success`**——其中 `Scale probe release` 三平台矩阵腿各自 6 个测试二进制（`scale_profile` 14 + `scale_profile_probe` 2 + `tasknode_scale_probe` 4 + `activation_meter_scale` 2 + `batch_cancel_scale` 2 + `lifecycle_scale` 2 = `26 passed; 0 failed; 0 ignored`）全部通过；同次 Run 内 debug `Scale probe (include-ignored)`（Job [123429146425](https://github.com/cty12356541/llmos/actions/runs/40868357139/job/123429146425)）及全部其余 9 个平台/MSRV/desktop/daemon 作业亦全部 `completed / success`（满足 §6 判据第 1 条）。
- **三平台 Runner 镜像与工具链基线**（Hosted Compute Agent `20261021.618` Commit `21505ad2541d5be6135ed84fc33697146cf75b8e`；三平台统一 `rustc 1.99.1 (58cf8f9c3 2026-10-13)`）：
  - `Scale probe release (ubuntu-latest)`：Job [123429146718](https://github.com/cty12356541/llmos/actions/runs/40868357139/job/123429146718) — Ubuntu `26.04.1` LTS，Image `ubuntu-26.04`（`20261025.179.1`），`stable-x86_64-unknown-linux-gnu` — `completed / success`
  - `Scale probe release (macos-latest)`：Job [123429146036](https://github.com/cty12356541/llmos/actions/runs/40868357139/job/123429146036) — macOS `26.6.2`（`25G83`），Image `macos-26-arm64`（`20261021.0445.8`），`stable-aarch64-apple-darwin` — `completed / success`
  - `Scale probe release (windows-latest)`：Job [123429146544](https://github.com/cty12356541/llmos/actions/runs/40868357139/job/123429146544) — Microsoft Windows Server 2025 `10.0.26100` Datacenter，Image `windows-2025-vs2026`（`20261021.278.1`），`stable-x86_64-pc-windows-msvc` — `completed / success`

#### 6.1.1 `ubuntu-latest`（Job `123429146718`）`--nocapture` 与汇总行原样誊录

```text
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.89s
10K task profile (single platform): registrations=10000 register_total=3.783161493s register_mean=3.783161ms permit_p50_100=791.138µs permit_p95_100=890.102µs permit_max_100=957.898µs permit_p50_10k=843.74µs permit_p95_10k=946.732µs permit_max_10k=1.095549ms working_set=512 working_set_total=300.361105ms working_set_p50=642.015µs working_set_p95=901.567µs inspect4=224.367µs database_bytes=7745536 rss_before=None rss_after=None
100K task profile (single platform): registrations=100000 register_total=66.998659775s register_mean=66.998659ms permit_p50_100=766.998µs permit_p95_100=895.807µs permit_max_100=6.300113ms permit_p50_100k=748.695µs permit_p95_100k=1.030158ms permit_max_100k=2.990419ms working_set=5120 working_set_total=3.826395382s working_set_p50=714.44µs working_set_p95=896.549µs inspect4=222.765µs database_bytes=70070272 rss_before=None rss_after=None
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 78.44s
10K logical TaskNode profile (single platform): nodes=10000 edges=9099 apply_total=499.861452ms resolve_total=19.109399ms resolved_view_total=9.826941ms inspect_p50_base=9.748µs inspect_p95_base=29.936µs inspect_p95_scale=22.162µs inspect_max_scale=42.87µs residency_p50_base=9.658µs residency_p95_base=14.627µs residency_p95_scale=11.702µs residency_max_scale=47.669µs eligible_sample=64 eligible_total=29.026029ms eligible_max=653.116µs evict_sample=8 evict_walk_total=17.298958ms tier_step_max=585.7µs baseline_database_bytes=412008 bytes_after_apply=5193728 bytes_after_activity=5652480 per_node_after_apply=519 per_node_after_activity=565 activity_delta=458752 rss_before=None rss_after_declare=None rss_after_activity=None
100K logical TaskNode profile (single platform): nodes=100000 edges=90999 apply_total=5.381989068s resolve_total=317.88525ms resolved_view_total=206.87526ms inspect_p50_base=18.174µs inspect_p95_base=23.364µs inspect_p95_scale=19.376µs inspect_max_scale=103.423µs residency_p50_base=17.362µs residency_p95_base=20.858µs residency_p95_scale=10.689µs residency_max_scale=11.582µs eligible_sample=64 eligible_total=16.862812ms eligible_max=687.821µs evict_sample=8 evict_walk_total=9.828775ms tier_step_max=369.648µs baseline_database_bytes=412008 bytes_after_apply=51195904 bytes_after_activity=55717888 per_node_after_apply=511 per_node_after_activity=557 activity_delta=4521984 rss_before=None rss_after_declare=None rss_after_activity=None
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.06s
10000-fiber activation-meter profile (2 tokio workers, sample=1000): spawn_issue=39.191589ms park_settle=2.071968ms external_wait_sleep=50ms sample_assert=373.986µs rss_kib=89216 threads=7 total=92.929933ms
10000-fiber activation-meter scale probe complete: active_cpu_phase=12.819162375s external_wait_phase=92.929933ms
100000-fiber activation-meter profile (2 tokio workers, sample=1000): spawn_issue=359.978291ms park_settle=22.892172ms external_wait_sleep=50ms sample_assert=392.911µs rss_kib=203440 threads=4 total=434.904864ms
100000-fiber activation-meter scale probe complete: active_cpu_phase=12.79832924s external_wait_phase=434.904864ms
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 13.36s
10000-fiber batch-cancel profile (workers=2, processes=4, scopes/process=16, waiters=1000, pre-terminal=16): spawn_issue=30.170524ms linkage=1.89054ms settle=17.47208ms join_reap=2.496959ms per_fiber_cancel_phase=2.451µs rss_kib before=5740 ready=22112 after_reap=22116 (growth 16372 ≤ bound 80000, proportional 8 KiB/fiber) threads 5 -> 5 (growth bound +4)
100000-fiber batch-cancel profile (workers=2, processes=4, scopes/process=16, waiters=1000, pre-terminal=16): spawn_issue=256.144306ms linkage=19.544078ms settle=181.101016ms join_reap=45.547503ms per_fiber_cancel_phase=2.493µs rss_kib before=21476 ready=162056 after_reap=162444 (growth 140580 ≤ bound 800000, proportional 8 KiB/fiber) threads 5 -> 5 (growth bound +4)
batch-cancel O-family ratio: per-fiber cancel phase 2.451µs @10K vs 2.493µs @100K → 1.02x (linear-or-better bound 3x); phase breakdown 10K settle=17.47208ms join_reap=2.496959ms, 100K settle=181.101016ms join_reap=45.547503ms
10000-fiber batch-cancel profile (workers=2, processes=4, scopes/process=16, waiters=1000, pre-terminal=16): spawn_issue=20.709448ms linkage=1.634764ms settle=18.847522ms join_reap=1.784262ms per_fiber_cancel_phase=2.455µs rss_kib before=10916 ready=26692 after_reap=26700 (growth 15776 ≤ bound 80000, proportional 8 KiB/fiber) threads 4 -> 4 (growth bound +4)
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.66s
10000-fiber backpressure_wait profile (2 tokio workers, sample=1000): spawn_issue=33.153056ms enter_backpressure=1.77745ms park_settle=932.456µs phase_sleep=50ms sample_assert=296.552µs resume_settle=934.95µs rss_kib=59656 threads=7 aggregate_backpressure_wait=540.588340419s aggregate_sampled_fibers=10000 total=92.087565ms
10000-fiber suspended profile (2 tokio workers, sample=1000): spawn_issue=32.903732ms enter_suspended=1.848181ms park_settle=972.48µs phase_sleep=50ms sample_assert=231.681µs resume_settle=957.553µs threads=7 aggregate_suspended=540.666270064s aggregate_sampled_fibers=10000 total=92.0202ms
10000-fiber lifecycle scale probe complete: backpressure_wait_phase=92.087565ms suspended_phase=92.0202ms
100000-fiber backpressure_wait profile (2 tokio workers, sample=1000): spawn_issue=281.724095ms enter_backpressure=14.715929ms park_settle=7.498341ms phase_sleep=50ms sample_assert=367.635µs resume_settle=8.658031ms rss_kib=159408 threads=4 aggregate_backpressure_wait=7465.063154603s aggregate_sampled_fibers=100000 total=398.241464ms
100000-fiber suspended profile (2 tokio workers, sample=1000): spawn_issue=312.783691ms enter_suspended=14.434254ms park_settle=7.274594ms phase_sleep=50ms sample_assert=223.115µs resume_settle=8.477875ms threads=4 aggregate_suspended=7374.773269652s aggregate_sampled_fibers=100000 total=426.53979ms
100000-fiber lifecycle scale probe complete: backpressure_wait_phase=398.241464ms suspended_phase=426.53979ms
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.99s
```

#### 6.1.2 `macos-latest`（Job `123429146036`）`--nocapture` 与汇总行原样誊录

```text
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.89s
100K task profile (single platform): registrations=100000 register_total=66.015230833s register_mean=66.01523ms permit_p50_100=384.417µs permit_p95_100=520.166µs permit_max_100=668.667µs permit_p50_100k=431.583µs permit_p95_100k=733µs permit_max_100k=10.4735ms working_set=5120 working_set_total=2.875909958s working_set_p50=363.458µs working_set_p95=713.459µs inspect4=131.208µs database_bytes=70045696 rss_before=Some(6979584) rss_after=Some(8470528)
10K task profile (single platform): registrations=10000 register_total=2.440456833s register_mean=2.440456ms permit_p50_100=374.958µs permit_p95_100=462.875µs permit_max_100=529.625µs permit_p50_10k=385.291µs permit_p95_10k=702.959µs permit_max_10k=8.8975ms working_set=512 working_set_total=156.239667ms working_set_p50=280.958µs working_set_p95=411.208µs inspect4=108.041µs database_bytes=7749632 rss_before=Some(8568832) rss_after=Some(8601600)
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 74.29s
10K logical TaskNode profile (single platform): nodes=10000 edges=9099 apply_total=467.350834ms resolve_total=15.925833ms resolved_view_total=8.535292ms inspect_p50_base=6.875µs inspect_p95_base=60.375µs inspect_p95_scale=29.75µs inspect_max_scale=44.208µs residency_p50_base=7.042µs residency_p95_base=9.625µs residency_p95_scale=24.375µs residency_max_scale=37.458µs eligible_sample=64 eligible_total=16.359499ms eligible_max=754.709µs evict_sample=8 evict_walk_total=9.590041ms tier_step_max=350.958µs baseline_database_bytes=412008 bytes_after_apply=5193728 bytes_after_activity=5652480 per_node_after_apply=519 per_node_after_activity=565 activity_delta=458752 rss_before=Some(6733824) rss_after_declare=Some(67141632) rss_after_activity=Some(70696960)
100K logical TaskNode profile (single platform): nodes=100000 edges=90999 apply_total=3.70083175s resolve_total=230.4325ms resolved_view_total=145.729833ms inspect_p50_base=7.083µs inspect_p95_base=8.042µs inspect_p95_scale=11.959µs inspect_max_scale=76.166µs residency_p50_base=7.125µs residency_p95_base=9.25µs residency_p95_scale=8.333µs residency_max_scale=17.917µs eligible_sample=64 eligible_total=13.036044ms eligible_max=530.083µs evict_sample=8 evict_walk_total=7.535958ms tier_step_max=425.417µs baseline_database_bytes=412008 bytes_after_apply=51195904 bytes_after_activity=55717888 per_node_after_apply=511 per_node_after_activity=557 activity_delta=4521984 rss_before=Some(6733824) rss_after_declare=Some(70647808) rss_after_activity=Some(95567872)
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.25s
10000-fiber activation-meter profile (2 tokio workers, sample=1000): spawn_issue=20.372833ms park_settle=2.619291ms external_wait_sleep=50ms sample_assert=291.209µs rss_kib=126944 threads=7 total=85.951417ms
10000-fiber activation-meter scale probe complete: active_cpu_phase=12.600115417s external_wait_phase=85.951417ms
100000-fiber activation-meter profile (2 tokio workers, sample=1000): spawn_issue=229.996416ms park_settle=129.096ms external_wait_sleep=50ms sample_assert=379.75µs rss_kib=245392 threads=4 total=417.489292ms
100000-fiber activation-meter scale probe complete: active_cpu_phase=12.591091625s external_wait_phase=417.489292ms
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 13.20s
10000-fiber batch-cancel profile (workers=2, processes=4, scopes/process=16, waiters=1000, pre-terminal=16): spawn_issue=21.23025ms linkage=3.5155ms settle=24.324625ms join_reap=4.0365ms per_fiber_cancel_phase=4.333µs rss_kib before=4560 ready=24144 after_reap=25584 (growth 19584 ≤ bound 80000, proportional 8 KiB/fiber) threads 5 -> 5 (growth bound +4)
10000-fiber batch-cancel profile (workers=2, processes=4, scopes/process=16, waiters=1000, pre-terminal=16): spawn_issue=18.022083ms linkage=3.726126ms settle=24.821375ms join_reap=2.586584ms per_fiber_cancel_phase=4.279µs rss_kib before=25760 ready=25856 after_reap=25888 (growth 96 ≤ bound 80000, proportional 8 KiB/fiber) threads 4 -> 4 (growth bound +4)
100000-fiber batch-cancel profile (workers=2, processes=4, scopes/process=16, waiters=1000, pre-terminal=16): spawn_issue=198.457917ms linkage=38.816875ms settle=253.476125ms join_reap=30.171792ms per_fiber_cancel_phase=3.350µs rss_kib before=25952 ready=192944 after_reap=203552 (growth 166992 ≤ bound 800000, proportional 8 KiB/fiber) threads 4 -> 4 (growth bound +4)
batch-cancel O-family ratio: per-fiber cancel phase 4.279µs @10K vs 3.350µs @100K → 0.78x (linear-or-better bound 3x); phase breakdown 10K settle=24.821375ms join_reap=2.586584ms, 100K settle=253.476125ms join_reap=30.171792ms
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.73s
10000-fiber backpressure_wait profile (2 tokio workers, sample=1000): spawn_issue=23.492042ms enter_backpressure=1.85025ms park_settle=616.417µs phase_sleep=50ms sample_assert=139.417µs resume_settle=643.834µs rss_kib=93008 threads=7 aggregate_backpressure_wait=643.632947783s aggregate_sampled_fibers=10000 total=91.839584ms
10000-fiber suspended profile (2 tokio workers, sample=1000): spawn_issue=26.922084ms enter_suspended=2.284875ms park_settle=668.917µs phase_sleep=50ms sample_assert=129.833µs resume_settle=533.458µs threads=7 aggregate_suspended=584.217576176s aggregate_sampled_fibers=10000 total=89.347167ms
10000-fiber lifecycle scale probe complete: backpressure_wait_phase=91.839584ms suspended_phase=89.347167ms
100000-fiber backpressure_wait profile (2 tokio workers, sample=1000): spawn_issue=225.299583ms enter_backpressure=23.095416ms park_settle=8.605833ms phase_sleep=50ms sample_assert=248.75µs resume_settle=8.11ms rss_kib=206848 threads=4 aggregate_backpressure_wait=9695.807181254s aggregate_sampled_fibers=100000 total=371.486875ms
100000-fiber suspended profile (2 tokio workers, sample=1000): spawn_issue=165.656292ms enter_suspended=17.66325ms park_settle=8.230083ms phase_sleep=50ms sample_assert=305.167µs resume_settle=6.207333ms threads=4 aggregate_suspended=8607.71016602s aggregate_sampled_fibers=100000 total=295.999167ms
100000-fiber lifecycle scale probe complete: backpressure_wait_phase=371.486875ms suspended_phase=295.999167ms
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.85s
```

#### 6.1.3 `windows-latest`（Job `123429146544`）`--nocapture` 与汇总行原样誊录

```text
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.35s
100K task profile (single platform): registrations=100000 register_total=1107.1910236s register_mean=1.107191023s permit_p50_100=3.3652ms permit_p95_100=7.4746ms permit_max_100=10.9158ms permit_p50_100k=3.9745ms permit_p95_100k=9.2292ms permit_max_100k=244.4908ms working_set=5120 working_set_total=45.7648249s working_set_p50=4.3762ms working_set_p95=8.6247ms inspect4=232.6µs database_bytes=70025216 rss_before=None rss_after=None
10K task profile (single platform): registrations=10000 register_total=64.8848535s register_mean=64.884853ms permit_p50_100=3.6755ms permit_p95_100=6.8571ms permit_max_100=39.8885ms permit_p50_10k=4.8487ms permit_p95_10k=11.3646ms permit_max_10k=150.58ms working_set=512 working_set_total=3.0513362s working_set_p50=4.8831ms working_set_p95=8.5687ms inspect4=145.6µs database_bytes=7749632 rss_before=None rss_after=None
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1249.60s
10K logical TaskNode profile (single platform): nodes=10000 edges=9099 apply_total=640.2156ms resolve_total=54.5938ms resolved_view_total=8.3837ms inspect_p50_base=6µs inspect_p95_base=9.5µs inspect_p95_scale=18.8µs inspect_max_scale=62.7µs residency_p50_base=5.9µs residency_p95_base=6.1µs residency_p95_scale=7µs residency_max_scale=7.8µs eligible_sample=64 eligible_total=320.0273ms eligible_max=19.7423ms evict_sample=8 evict_walk_total=141.4858ms tier_step_max=6.6309ms baseline_database_bytes=412008 bytes_after_apply=5193728 bytes_after_activity=5652480 per_node_after_apply=519 per_node_after_activity=565 activity_delta=458752 rss_before=None rss_after_declare=None rss_after_activity=None
100K logical TaskNode profile (single platform): nodes=100000 edges=90999 apply_total=4.9612839s resolve_total=536.0529ms resolved_view_total=346.8787ms inspect_p50_base=5.8µs inspect_p95_base=6.5µs inspect_p95_scale=29.9µs inspect_max_scale=183.6µs residency_p50_base=5.9µs residency_p95_base=6µs residency_p95_scale=9.7µs residency_max_scale=21.6µs eligible_sample=64 eligible_total=213.9143ms eligible_max=10.7751ms evict_sample=8 evict_walk_total=184.3749ms tier_step_max=26.3503ms baseline_database_bytes=412008 bytes_after_apply=51195904 bytes_after_activity=55717888 per_node_after_apply=511 per_node_after_activity=557 activity_delta=4521984 rss_before=None rss_after_declare=None rss_after_activity=None
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.00s
10000-fiber activation-meter profile (2 tokio workers, sample=1000): spawn_issue=11.0231ms park_settle=13.5911ms external_wait_sleep=50ms sample_assert=142.6µs rss_kib=0 threads=0 total=76.3025ms
10000-fiber activation-meter scale probe complete: active_cpu_phase=14.5657412s external_wait_phase=76.3025ms
100000-fiber activation-meter profile (2 tokio workers, sample=1000): spawn_issue=121.4097ms park_settle=21.1076ms external_wait_sleep=50ms sample_assert=227µs rss_kib=0 threads=0 total=193.8835ms
100000-fiber activation-meter scale probe complete: active_cpu_phase=14.655819s external_wait_phase=193.8835ms
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 14.94s
10000-fiber batch-cancel profile (workers=2, processes=4, scopes/process=16, waiters=1000, pre-terminal=16): spawn_issue=11.5174ms linkage=1.5537ms settle=1.432ms join_reap=1.6621ms per_fiber_cancel_phase=1.986µs rss_kib before=0 ready=0 after_reap=0 (growth 0 ≤ bound 80000, proportional 8 KiB/fiber) threads 0 -> 0 (growth bound +4)
10000-fiber batch-cancel profile (workers=2, processes=4, scopes/process=16, waiters=1000, pre-terminal=16): spawn_issue=10.669ms linkage=2.17ms settle=1.568ms join_reap=1.7544ms per_fiber_cancel_phase=3.455µs rss_kib before=0 ready=0 after_reap=0 (growth 0 ≤ bound 80000, proportional 8 KiB/fiber) threads 0 -> 0 (growth bound +4)
100000-fiber batch-cancel profile (workers=2, processes=4, scopes/process=16, waiters=1000, pre-terminal=16): spawn_issue=121.278ms linkage=45.2328ms settle=72.2285ms join_reap=27.7122ms per_fiber_cancel_phase=1.618µs rss_kib before=0 ready=0 after_reap=0 (growth 0 ≤ bound 800000, proportional 8 KiB/fiber) threads 0 -> 0 (growth bound +4)
batch-cancel O-family ratio: per-fiber cancel phase 3.455µs @10K vs 1.618µs @100K → 0.47x (linear-or-better bound 3x); phase breakdown 10K settle=1.568ms join_reap=1.7544ms, 100K settle=72.2285ms join_reap=27.7122ms
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.70s
10000-fiber backpressure_wait profile (2 tokio workers, sample=1000): spawn_issue=13.1476ms enter_backpressure=1.9243ms park_settle=1.0277ms phase_sleep=50ms sample_assert=281.2µs resume_settle=1.9305ms rss_kib=0 threads=0 aggregate_backpressure_wait=629.651569s aggregate_sampled_fibers=10000 total=82.8338ms
10000-fiber suspended profile (2 tokio workers, sample=1000): spawn_issue=12.2121ms enter_suspended=2.0509ms park_settle=978µs phase_sleep=50ms sample_assert=208.4µs resume_settle=1.3516ms threads=0 aggregate_suspended=557.0316208s aggregate_sampled_fibers=10000 total=73.6976ms
10000-fiber lifecycle scale probe complete: backpressure_wait_phase=82.8338ms suspended_phase=73.6976ms
100000-fiber backpressure_wait profile (2 tokio workers, sample=1000): spawn_issue=130.9054ms enter_backpressure=26.9825ms park_settle=18.738ms phase_sleep=50ms sample_assert=212µs resume_settle=16.3653ms rss_kib=0 threads=0 aggregate_backpressure_wait=9253.934083s aggregate_sampled_fibers=100000 total=285.3362ms
100000-fiber suspended profile (2 tokio workers, sample=1000): spawn_issue=119.3385ms enter_suspended=28.1646ms park_settle=17.1498ms phase_sleep=50ms sample_assert=168.7µs resume_settle=12.5614ms threads=0 aggregate_suspended=8894.1225939s aggregate_sampled_fibers=100000 total=265.3361ms
100000-fiber lifecycle scale probe complete: backpressure_wait_phase=285.3362ms suspended_phase=265.3361ms
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.65s
```

#### 6.1.4 三平台 CI `--release` 关键指标对照表（对标 §4.2）

| 指标面（档位） | ubuntu-latest (x86_64 Linux) | macos-latest (arm64 macOS) | windows-latest (x86_64 Windows) | 断言界与跨平台机制核验 |
|---|---|---|---|---|
| `task` 10K / 100K `register_total` | `3.783s` / `66.999s`（二进制 `78.44s`） | `2.440s` / `66.015s`（二进制 `74.29s`） | `64.885s` / `1107.191s`（二进制 `1249.60s`） | Linux ext4 与 macOS APFS 100K 均收敛在 `~66–67s`（远快于 §6 第 5 条 debug ~725s 先例）；Windows Server NTFS `SQLite WAL+FULL` 逐事务物理刷盘成本为 `1107.19s`（二进制总耗时 `1249.60s`），属宿主存储栈 fsync 地板而非算法退化 |
| `task permit` `p95_100` / `p95_100k` 及惰性比值 `p95_100k / p95_100` | `895.8µs` / `1.030ms`（`1.030ms / 895.8µs = 1.15x`） | `520.2µs` / `733µs`（`733µs / 520.2µs = 1.41x`） | `7.475ms` / `9.229ms`（`9.229ms / 7.475ms = 1.23x`） | 三平台均远低于 `<= 16x` 惰性比值界且绝对 `p95_100k < 100ms` 硬上限成立；100K 背景行不抬高单点 permit 延迟 |
| `task working-set` 100K `p95` 与 `database_bytes`（`working_set=5120`） | `896.5µs` / `70,070,272` B | `713.5µs` / `70,045,696` B | `8.625ms` / `70,025,216` B | 三平台 `working_set_p95` 均远低于 `< 100ms` 硬上限；100K 任务库落盘体积一致收敛在 `~70.0–70.1 MB` |
| `plan` 100K `apply_total` / `resolve_total` / `inspect_p95_scale` | `5.382s` / `317.9ms` / `19.38µs` | `3.701s` / `230.4ms` / `11.96µs` | `4.961s` / `536.1ms` / `29.9µs` | `LAZY_RATIO <= 16x` 与 `< 100ms` 上限在三平台全部成立；100K 逻辑节点库单次 `inspect` 保持在 `12–30µs` |
| `plan` 每节点 durable 字节与数据库体积（10K / 100K） | 10K `519B/565B`；100K `511B/557B`（`51,195,904` / `55,717,888` B） | 10K `519B/565B`；100K `511B/557B`（`51,195,904` / `55,717,888` B） | 10K `519B/565B`；100K `511B/557B`（`51,195,904` / `55,717,888` B） | **Linux / macOS / Windows 三平台逐字节完全一致**（100K `bytes_after_apply=51,195,904` / `bytes_after_activity=55,717,888` 分毫不差，远低于 `<= 4096B/node`），实证 durable 布局跨 OS/架构零漂移 |
| `activation-meter` 100K `external_wait_phase` / `active_cpu_phase` | `434.9ms` / `12.80s` | `417.5ms` / `12.59s` | `193.9ms` / `14.66s` | `MIN_EXTERNAL_WAIT >= 40ms`（50ms sleep 锚定）与 `MIN_ACTIVE_CPU >= 10ms`（墙钟目标锚定；Windows 调度时钟粒度使 busy-loop 为 `14.66s`）三平台全绿 |
| `batch-cancel` `per_fiber_cancel_phase`（10K -> 100K）与 O 族比值 | `2.451µs -> 2.493µs`（`1.02x`） | `4.279µs -> 3.350µs`（`0.78x`） | `3.455µs -> 1.618µs`（`0.47x`） | 三平台全部满足 `<= 3x` 线性或更优上界（独立 10K 测例分别为 `2.455µs` / `4.333µs` / `1.986µs`） |
| `lifecycle` 10K / 100K `backpressure_wait_phase` & `suspended_phase` | 10K `92.09ms` / `92.02ms`；100K `398.24ms` / `426.54ms`（无 SKIP） | 10K `91.84ms` / `89.35ms`；100K `371.49ms` / `296.00ms`（无 SKIP） | 10K `82.83ms` / `73.70ms`；100K `285.34ms` / `265.34ms`（无 SKIP） | **三平台 `lifecycle_scale` 10K 均无 SKIP 行**，「等待主导」比值断言在三平台 CI runner 上全部真实执行并通过 |
| RSS 与线程读数口径核验 | `runtime` `/proc/self/status` 真实校验（100K cancel 增长 `140,580 KiB <= 800,000`，`threads` `5->5` / `4->4`）；`task`/`plan` 可选读数为 `None` | 全族 `ps` 真实校验（100K cancel 增长 `166,992 KiB <= 800,000`，`threads` `5->5` / `4->4`；`plan` 100K RSS `95,567,872` B） | `rss_before=None` / `rss_kib=0` / `threads=0`（`cfg` 兜底空转） | Ubuntu 与 macOS 真实通过 `<= 80,000 / 800,000 KiB` 种群比例界与 `<= +4` 线程界；Windows 腿如 §2 与 §6 第 4 条预言走 `cfg` 兜底空转，吞吐/比值/durable 字节硬界全部真实生效 |

验收对照结论：§6 第 1–6 条判据在 Run [40868357139](https://github.com/cty12356541/llmos/actions/runs/40868357139) 中全部兑现（零常量放宽、零假失败重跑），正式闭合 `U-1` 的多平台 CI `--release` 五探针基线半边；但本文件仍严守 §1.5 口径，不发布任何无边界生产量级口号，`1M_LOGICAL` 裸金属/跨机 WAN 边界与 `RISK-B-07`（`partial`）维持不变。

## 7. 仍属缺口 / deferred minors（如实登记）

1. **未 release 化家族**：§1.2 所列六个探针二进制不在本管线——每扩一族须先过同机 release 对照（同 §4 纪律），再向 job 加一步（一行成本）。
2. **windows RSS/线程读数空转**：cfg 兜底 0/None；补 windows 真读数（如 `GetProcessMemoryInfo`）归后续车道——本 lane 不改探针代码。
3. **生产量级声明禁令与更高档位边界（W76 更新）**：三平台 CI `--release` 五探针家族基线已于 `2026-10-29`（`W76`，Run [40868357139](https://github.com/cty12356541/llmos/actions/runs/40868357139)）在 §6.1 回填闭合（不再是 W36 时的单机孤证），但无边界生产量级口号（「PID 级 Agent 容量」「coroutine 级大规模并发」）禁令、`1M_LOGICAL` 裸金属/跨机 WAN 边界及 `RISK-B-07`（`partial`）独立治理门槛继续维持（与 §1.5 一致）。
4. **`lifecycle_scale` 二进制内两档并行**（无 `PROBE_SERIALIZE` 槽，与 debug 夜间同口径）：release 下 spawn 窗收缩使 10K 更可能落亚阈；若 CI release 出现倒挂类假失败，按 §6.18 同法加槽（届时登记），本 lane 不预改——保持与 debug 管线同形状的诚实对照。
5. **无 profile 调优基线**：默认 release（无 LTO 等）；若未来引入 `[profile.release]` 自定义，本文件全部 release 数字即失效，须复测。
6. **debug 对照相的负载披露**：本地同日跑受同机并行负载影响（house 常态），绝对值有噪声、比值与断言结论不受影响；CI 数字落地后以 CI 画像为准。

## 8. 未运行项（显式列出）

- **push / PR / workflow dispatch**：MUST NOT（派工单明令；控制器 post-merge 统一执行）。
- **`cargo test --workspace`（任何 profile）**：MUST NOT（波次屏障；本 lane 以三 crate 探针目标为验证面）。
- **CI 多平台实跑**：PENDING（§6）。
- **探针/ src Rust 代码改动**：无——校准裁决零调整（§3），`cargo fmt`/clippy 无适用面。
- **W76 回填注记（2026-10-29）**：控制器已在 `ed64ae3` 执行 `workflow_dispatch` Run [40868357139](https://github.com/cty12356541/llmos/actions/runs/40868357139)（非夜间 `schedule` 触发）完成三平台 CI `--release` 实跑（全工作流 13/13 全绿），并由 `W76-SCALE-BACKFILL` 车道将实测结果回填至 §6.1；本回填车道零探针/Rust 代码改动，不执行 `git push`。

## 9. Base HEAD / 写集

- 开工 `7d3ad53`（分支 `feat/w36-p5`，W36 lane C）。
- 写集：`.github/workflows/rust-cross-platform.yml`（新增 `scale-probe-release` job）、本文件、`docs/management/evidence-index.yaml`（本文件行）。零 Rust 代码改动。
- **W76 回填记录（2026-10-29）**：基线 `ed64ae3`（分支 `feat/w76-scale-backfill-r1`，`W76-SCALE-BACKFILL` / Lane F）；写集仅本文件 `docs/evidence/stage-b/b-scale-release-001.md`（零 Rust / workflow / 台账文件改动）。
