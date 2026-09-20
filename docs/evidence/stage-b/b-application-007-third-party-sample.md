# B-APPLICATION-007：第三方样板应用全生命周期（W33-B / B1-5）

> 状态：`PARTIAL PASS`
>
> 日期：2026-09-21
>
> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W33-B 车道行（B1-5，验收门「第三方样板应用全生命周期：非内核视角开发 → sign → install → run → update → uninstall」）；ROAD-B-001 生态门的 constructive exit 证据
>
> 实现：`examples/sample-app/`（新子树：开发者目录 ×2、`sample-app-driver` 二进制、`lifecycle.sh` 演练脚本）+ `crates/nlos-slice-k/tests/third_party_sample_lifecycle.rs`（无头全链测试）
>
> 依赖前序：B-ARTIFACT-007/W33-A（`nlos-package` CLI）、B-PLAN-002/W28-B（`tasks` 模板段）、B-APPLICATION-002 §9/W29-E（迁移 runner）、B-APPLICATION-003 §W27-D（真实活动门）、B-SLICE-K-001 §W30-D（teardown 链）

## 1. 本切片目标

证明一个不触任何内核内部 API 的第三方开发者，只骑公共面就能走完应用全生命周期。样板是一个完整的应用项目（manifest 含 W28-B 模板段、载荷文件、生命周期驱动），消费端接线全部走既有公共 API/CLI：`nlos-package keygen/build/verify`（真实二进制）+ `sample-app-driver`（只 import 公共 crate：`nlos-application`/`nlos-artifact`/`nlos-process`/`nlos-runtime`/`nlos-runtime-tokio`/`nlos-slice-k`/`nlos-task`/`nlos-types` + crates.io 的 `sha2`/`tokio`；零内部模块、零私有 API、零公式复制）。

## 2. 写集清单

- `examples/sample-app/**`（新建子树：`package/` v1.0.0 开发者目录、`package-v1.1.0/` 更新快照、`src/main.rs` 驱动、`lifecycle.sh`、`README.md`、`Cargo.toml`（自带空 `[workspace]`，desktop/ 先例，根 Cargo.toml 零改动）、`Cargo.lock`（工具链生成））
- `crates/nlos-slice-k/tests/third_party_sample_lifecycle.rs`（新测试文件；slice-k src/Cargo.toml 零改动）
- `docs/evidence/stage-b/b-application-007-third-party-sample.md`（本文件）+ `docs/management/evidence-index.yaml`（追加收录本文件）

其余文件未改动；并行车道（W33-A 的 nlos-artifact、W30-A/W31-A 的 nlos-task 等）未被触碰。

## 3. 样板结构与生命周期序列

### 3.1 样板（`examples/sample-app/`）

- **`package/`**（v1.0.0）：`package.manifest`（package-id `9a1c7d3e…`；`entry`×2：`sample-driver executable` + `sample-config data`；`task`×2：`agent-role` 主节点 + 依赖它的 `executable` 服务节点——W28-B 模板段含段内依赖链）、`app.bin`、`assets/config.bin`、`tasks/` 下十个 digest 体文件。
- **`package-v1.1.0/`**：同一棵树在版本演进后的快照（同 package-id、`version = 1.1.0`、载荷与模板体实改）——manifest digest 必变，走 update 通道而非重装通道。
- **`src/main.rs`（`sample-app-driver`）**：四子命令驱动。密钥纪律：slice-k 复用助手按 phase seed 带（0x5B/0x6C/0x4E，内部键互不相交）；驱动自有幂等/时钟键按 `llmos/sample-app/driver-key/v1` 域分隔 SHA-256 派生，输出 artifact 身份按 `llmos/sample-app/output-artifact/v1` 派生（与权威派生 id 域分隔，零公式镜像）。
- **`lifecycle.sh`**：九步演练 + 退出码/输出断言（`grep` 逐行断言关键 receipt 行）。

### 3.2 命令序列（全部公共面）

| 步 | 命令 | 公共面 |
|---|---|---|
| 1 | `nlos-package keygen --seed <HEX64>` | W33-A CLI |
| 2 | `nlos-package build package --key …` | 行式 manifest 解析 + with-tasks 域分隔签名 |
| 3 | `nlos-package verify <pkg> --store $STATE/artifacts --identity $STATE/identity` | `ArtifactStore::verify_package_with_tasks`（与内核同一条权威验签路径）；receipt 落在与驱动同一 `$STATE` |
| 4 | `sample-app-driver install $STATE <receipt-id>` | receipt digest-binding → `install_application`（gen 1） |
| 5 | `sample-app-driver run $STATE <pkg-id>` | `register_task_and_attempt_for`（durable 行携带 application 关联）→ `materialize_process` → `CommitPermit` → `spawn_write_fiber`（durable driver Operation register→dispatch→complete + stage + plan）→ `converge_pending`（TaskCommitReceipt，head revision 2）→ background task / process binding 注册；Unix 下 `sleep 600` 真实子进程充当 background-service 的 Os 替身（stdio 置 null，防管道孤儿阻塞） |
| 6 | `nlos-package build package-v1.1.0 …` + `verify …` | 同 major 新 revision 的 verified receipt |
| 7 | `sample-app-driver update $STATE <pkg-id> <receipt-v2>` | W29-E：`migrate_application`（SameMajor 窗 + 冻结基线/目标）→ `record_migration_step`×2 → `run_migration_health_check`（probe 恰一次）→ `activate_package_migration`（单事务原子切换，gen 1→2） |
| 8 | `sample-app-driver uninstall $STATE <pkg-id> <service_pid>` | 先见证 W27-D 门 typed 拒绝（`ApplicationActiveTasksRunning`），再 W30-D `run_application_teardown`：platform kill（真实 SIGTERM 杀死 Os 替身）→ crash terminal → W27-C linkage → `cancel_task` → 过门卸载；supervisor pid 由脚本跨进程携带（内存 registry 的既定重启重注册模式） |
| 9 | 负门：卸载后 `run` 必须 typed 拒绝；`kill -0` 确认替身进程已消失 | fail-closed 语义 |

## 4. 验证证据

### 4.1 全生命周期演练（真机实跑，2026-09-21，macOS/arm64）

`sh examples/sample-app/lifecycle.sh` → **exit 0**（`LIFECYCLE OK`）。关键输出（摘）：

```text
KEYGEN …/sample-app.devkey
principal d834757ea027fa3517d9d2cdb9b1088d
BUILT …/sample-app-v1.0.0.nlospkg
package 9a1c7d3e5f0b2a4c6d8e0f1a2b3c4d5e version 4294967296
manifest_digest d338026518afcef392511e92b80f8aa4a9d3730504dd06bd58601e794db6e205
entries 2 tasks 2
VERIFIED 91a1a600bd291123dc09779e6eb9d345          ← 与 build 的 manifest_digest/signer 逐位一致
[sample-app] INSTALL installation=a8b24b13693fb458 application=f15f762dbcc3ad0e … generation=1 version=4294967296 entries=2 installer=d834757ea027fa35
[sample-app] RUN task=8080808080808080 … associated_application=true
[sample-app] RECEIPT kind=commit-permit id=96d9a9873f624efe …
[sample-app] RUN operation=67070273ffc84d4f fiber_state=Ok(Completed) plan=956c2d74ab5c73f5
[sample-app] RECEIPT kind=task-commit id=b614a844f6abb464 … head_commit_seq=1 publications=1 output_head_revision=2
[sample-app] RUN registered … outstanding_tasks=1
[sample-app] RUN service_os_pid=97515
VERIFIED ae76d738d740821716a5655df0a937f3          ← v1.1.0：manifest_digest a60056b3… version 4295032832
[sample-app] UPDATE drill=started from_generation=1 from_version=4294967296 target_version=4295032832
[sample-app] UPDATE step=1 recorded / step=2 recorded
[sample-app] UPDATE probe package=9a1c7d3e5f0b2a4c from_generation=1 steps=2/2
[sample-app] UPDATE health passed=true …
[sample-app] UPDATE installation=0f1783486dc44e7d generation=2 manifest=a60056b333390b8a application_status=installed
[sample-app] GATE_REFUSED active_task_count=1       ← W27-D 真实活动门
[sample-app] UNINSTALL kills=1 crashes=1 linkages=1 task_cancels=1
[sample-app] UNINSTALL kill#0 decision=signaled     ← 真实 POSIX kill 链
[sample-app] UNINSTALL cancel#0 decision=Applied { cancel_epoch: 1, closed_attempts: [] }
[sample-app] UNINSTALL done application=f15f762dbcc3ad0e status=uninstalled
[lifecycle] service stand-in pid 97515 is gone (platform kill was real)
[lifecycle] STEP 9 negative gate: run after uninstall must fail closed   ← driver exit 2（typed 拒绝）
```

注：`closed_attempts` 为空是既有语义——run 阶段的 attempt 已 `Committed`（终态），cancel 只需把 Task 行推到 `Cancelled` 使活动门计数归零。

### 4.2 无头全链测试（CI 可跑）

新增 `crates/nlos-slice-k/tests/third_party_sample_lifecycle.rs`：同一链条的进程内孪生，另覆盖脚本未断言的面——`compile_task_templates` 把签名模板段编成 plan proposal（2 节点、kind 映射、依赖序保持、`plan_id: None`，`[PLAN-OVERRIDE-001]`）；跨 major（2.0.0）目标对 `SameMajor` 窗 typed 拒绝（`UpdateCompatibilityViolation`，零 durable）；代际历史稠密 [1,2]；teardown 后 task 行 `Cancelled`、binding 终态 `Crashed`、被杀作用域拒绝新 fiber（`RuntimeError::Cancelled`）；整条 teardown 经**空** supervisor registry 重跑逐字节 replay（kill replay 在适配器调用前被 durable receipt 短路）。Unix 道以真实 `sleep` 子进程过 SIGTERM kill 链（子进程 `!status.success()` 断言）；非 Unix 道走 noop 契约适配器。

### 4.3 验证门（W33-B 实跑，2026-09-21，final fmt 后）

| 门 | 命令 | 结果 |
|---|---|---|
| 无头全链测试 | `cargo test -p nlos-slice-k --test third_party_sample_lifecycle` | PASS（unix 道 1 用例；非 unix 契约道 cfg 隔离） |
| slice-k 全量回归 | `cargo test -p nlos-slice-k` | PASS：20 passed / 0 failed（既有 19 原样绿 + 新增 1） |
| slice-k clippy | `cargo clippy -p nlos-slice-k --all-targets --all-features -- -D warnings` | PASS：exit 0 |
| slice-k fmt | `cargo fmt -p nlos-slice-k -- --check` | PASS：exit 0 |
| 样板项目 clippy | `examples/sample-app` 内 `cargo clippy --all-features -- -D warnings` | PASS：exit 0（含 `--all-features`；未用 `chunks_exact().map()` 模式） |
| 样板项目 fmt | `examples/sample-app` 内 `cargo fmt -- --check` | PASS：exit 0 |
| 真机全生命周期 | `sh examples/sample-app/lifecycle.sh` | PASS：exit 0（§4.1） |
| 台账 lint | `python3 scripts/lint_claims.py` | PASS：137/137（本文件已入 evidence-index） |

## 5. 证据等级与 deferred minors

证据等级：单节点局部 H3，`PARTIAL PASS`。诚实不声明：

- **载荷执行面未做**：manifest 的 `executable` entry 字节不被真实执行（内核侧载荷执行是后续车道）；run 阶段驱动的是公共任务/操作/纤维面，载荷以 artifact 形态可消费（verify 时已物化进 artifact store）。样例的「driver entry」是驱动二进制本身（消费公共 driver/runtime 面），不是载荷字节的运行时。
- **Os 服务替身是 `sleep` 子进程**：真实子进程、真实 SIGTERM、真实死亡，但不是内核托管的服务进程；supervisor registry 是内存态（W29-F 既定语义），跨驱动进程的 pid 由演练脚本携带并按重启模式重注册。
- **`compile_task_templates` 只到 proposal**：未接 `apply_plan_revision`（plan authority 接线是后续车道，`nlos-application/src/task_templates.rs` 文档已声明）。
- **`nlos package install` CLI 子命令仍未做**：消费端接线由 `sample-app-driver` 以库 API 承担（W33-A §4 已登记的递延项，本切片补上的是消费端事实而非 CLI 面）。
- **演练一次性状态根**：`lifecycle.sh` 每次跑用全新 `$STATE`；各阶段单次执行的幂等语义由底层权威保证，但「同一 root 上重复整条脚本」不在声明面（run 阶段的 fiber/permit 键按单次执行设计）。
- **非 Unix 契约道未本机实测**：macOS 只跑 unix 道；noop 适配器路径与三平台 CI 属波次屏障统一项。

## 6. 未运行项（显式列出）

未运行 `cargo test --workspace` / `cargo clippy --workspace`（任务边界禁）、三平台 CI/MSRV/Pages（波次屏障统一跑）、真实断电矩阵（无新 durable 协议——全部复用既有权威提交路径）。
