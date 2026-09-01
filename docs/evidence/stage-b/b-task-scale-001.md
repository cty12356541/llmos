# B-TASK-SCALE-001：ROAD-B-004 前片 ScaleProfile 骨架与 10K 已落地维度规模证明

> 状态：`PASS`（前片；ROAD-B-004 整体未达成，缺口见 §4）
>
> 日期：2026-08-30
>
> 对应：`[ROAD-B-004]`（`06-架构设计总纲-v0.5.md` §28.2 单节点 ScaleProfile 发布与 10K/100K 逻辑 TaskNode 基准）
>
> 实现：`nlos-task::scale`（`ScaleProfile` + `TASK_PROFILE_10K`）、`tests/scale_profile.rs`、`tests/scale_profile_probe.rs`

## 1. 本切片目标

为 Task 域发布第一个具名单节点容量档（ScaleProfile 骨架），并对**已落地的维度**（持久 Task 注册、key-scoped permit 查询面）给出 10K 规模的实证数字，证明惰性查询面不随任务总体线性退化。

## 2. 已实现事实

1. `ScaleProfile { profile_id, max_task_nodes, max_active_working_set }` 常量声明面 + `TASK_PROFILE_10K`（`task-10k`，10_000 / 512）档；`admits_task_nodes` / `admits_active_working_set` 为 const 谓词（含端点）。
2. **声明而非强制**：本片不把档位接入注册/admission 路径（登记为缺口）。
3. **临时维度映射**：`TaskSpec` 无 plan 字段、TaskPlan/TaskNode 声明面未落地，`max_task_nodes` 暂以持久 `Task` 注册承载，`max_active_working_set` 以未结 `CommitPermit` 承载；名义 `TaskPlanId`/`TaskNodeId` 已存在但未绑定持久面。
4. 惰性断言针对**已落地 key-scoped 查询模式**：`tasks.task_id` 主键、attempts/permits 的 `UNIQUE(task_id, idempotency_key)`、`commit_permits_single_active` 部分唯一索引；无全表扫描路径。
5. 常规测试（200 注册 + 16 活跃样本、幂等重注册、single-active fence `Superseded`、散布点读 + 60s 病态慢守卫）；10K 全量数字由显式 `#[ignore]` probe 承载。

## 3. probe 实跑数字（原样誊录）

命令：`cargo test -p nlos-task --test scale_profile_probe -- --ignored --nocapture`（debug/test profile，单平台 macOS，fsync 逐注册事务，`.expect` 全程）。

```
10K task profile (single platform): registrations=10000 register_total=1.769339542s register_mean=1.769339ms permit_p50_100=318.542µs permit_p95_100=377.542µs permit_max_100=613µs permit_p50_10k=346.417µs permit_p95_10k=391.291µs permit_max_10k=2.4605ms working_set=512 working_set_total=166.560875ms working_set_p50=350.917µs working_set_p95=388.166µs inspect4=168.625µs database_bytes=7569408 rss_before=Some(7389184) rss_after=Some(8880128)
test result: ok. 1 passed; 0 failed; ... finished in 2.27s
```

要点：

1. **惰性成立**：10K 库 permit p95 = 391.291µs ≤ 基线（100 库）p95 377.542µs × 16 断言限（实际 ~1.04x）；同 run 绝对面 < 100ms 限。
2. 注册吞吐：10K 次 fsync 注册共 1.769s（均值 1.769ms/次）；同语义早前样本为 4.437s（平台噪声，另录）。
3. 512 活跃工作集发放共 166.561ms（p95 388.166µs）；散布点读 4 次 168.625µs。
4. 落盘体积 7,569,408 字节；进程 RSS 7,389,184 → 8,880,128 字节（`ps` 采样，仅 macOS 有可移植读数，其他 target 如实记 `None`）。

## 4. 限制与下一步（缺口清单）

1. **未强制**：档位未接入 `TaskAuthority` 注册/admission；强制路径为后续工作。
2. **维度映射临时**：TaskPlan/TaskNode 持久声明面、Dependency Resolver 未落地；`max_task_nodes` 以 Task 注册近似，100K 档（§28.2 的 100K 基准）未跑，不得宣称 ROAD-B-004 整体达成。
3. **证据等级**：debug（test profile）单平台数字；release profile 与多平台复测未做；checkpoint/rehydrate 基准不在本片。
4. 工作树交接备注：接管时验证窗口与并行车道共享构建目录存在锁竞争，全量门以逐二进制方式收口（结果与本报告命令均一一对应，无跳过项）。

## 5. 验证门（全部实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| 编译 | `cargo test -p nlos-task --no-run` | PASS（全部测试二进制构建成功） |
| 常规套件 | `cargo test -p nlos-task`（probe 为 `#[ignore]` 天然排除） | PASS（32 个集成测试二进制 + lib 单测全量逐一二进制实跑：合计 258 passed / 0 failed / 1 ignored 即 probe；全量单遍 wall-clock 因并行车道构建锁竞争未采信，逐二进制明细见交接回执） |
| probe 实跑 | `cargo test -p nlos-task --test scale_profile_probe -- --ignored --nocapture` | PASS（§3 数字） |
| clippy stable | `cargo clippy -p nlos-task --all-targets -- -D warnings` | PASS（修 `doc_markdown` ×2、`duration_suboptimal_units` ×3 后） |
| clippy nightly | `cargo +nightly-2026-08-01 clippy -p nlos-task --all-targets -- -D warnings` | PASS |
| fmt stable | `cargo fmt -p nlos-task -- --check` | PASS |
| fmt nightly | `cargo +nightly-2026-08-01 fmt -p nlos-task -- --check` | PASS |

修复说明（接管方最小补完，未重写前代理设计）：`scale.rs` / `scale_profile.rs` 文档 `TaskNodes`、`TaskNode` 加反引号（clippy `doc_markdown`）；`from_secs(60/120)` → `from_mins(1/2)`（clippy `duration_suboptimal_units`，语义不变）；`cargo fmt` 纯格式化。
