# B-APPLICATION-007：第三方样板应用全生命周期（W33-B / B1-5）

> 状态：`PARTIAL PASS`
>
> 日期：2026-09-21
>
> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W33-B 车道行（B1-5，验收门「第三方样板应用全生命周期：非内核视角开发 → sign → install → run → update → uninstall」）；ROAD-B-001 生态门的 constructive exit 证据；§7 为阶段 C 移交项 #2 前片（W35-P2 / `C-APP-PAYLOAD`）增量
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

- **载荷执行面未做**（W33-B 时点措辞）：manifest 的 `executable` entry 字节不被真实执行——run 阶段驱动的是公共任务/操作/纤维面，载荷以 artifact 形态可消费（verify 时已物化进 artifact store）。样例的「driver entry」是驱动二进制本身（消费公共 driver/runtime 面），不是载荷字节的运行时。**内核侧最小执行车道已由 §7 前片收口（W35-P2）；样例自身 run 阶段接线该车道仍属后续切片。**
- **Os 服务替身是 `sleep` 子进程**：真实子进程、真实 SIGTERM、真实死亡，但不是内核托管的服务进程；supervisor registry 是内存态（W29-F 既定语义），跨驱动进程的 pid 由演练脚本携带并按重启模式重注册。
- **`compile_task_templates` 只到 proposal**：未接 `apply_plan_revision`（plan authority 接线是后续车道，`nlos-application/src/task_templates.rs` 文档已声明）。
- **`nlos package install` CLI 子命令仍未做**（W33-B 时点措辞）：消费端接线由 `sample-app-driver` 以库 API 承担（W33-A §4 已登记的递延项，本切片补上的是消费端事实而非 CLI 面）。**该子命令已由 §7 前片收口（W35-P2）；run/update/uninstall 的 CLI 消费端仍属后续车道。**
- **演练一次性状态根**：`lifecycle.sh` 每次跑用全新 `$STATE`；各阶段单次执行的幂等语义由底层权威保证，但「同一 root 上重复整条脚本」不在声明面（run 阶段的 fiber/permit 键按单次执行设计）。
- **非 Unix 契约道未本机实测**：macOS 只跑 unix 道；noop 适配器路径与三平台 CI 属波次屏障统一项。

## 6. 未运行项（显式列出）

未运行 `cargo test --workspace` / `cargo clippy --workspace`（任务边界禁）、三平台 CI/MSRV/Pages（波次屏障统一跑）、真实断电矩阵（无新 durable 协议——全部复用既有权威提交路径）。

## 7. 移交#2 前片（W35-P2 / `C-APP-PAYLOAD`）：载荷执行车道 + `nlos-package install` CLI

> 2026-09-21 增量；阶段 C [进度单 §甲组 移交项 #2](../../management/stage-c-progress.md) 前片。收口对象：W33-H §2 具名边界 1（载荷执行面）与边界 3（消费端库驱动）的**前片范围**——install CLI 收口、内核侧最小执行车道收口；run/update/uninstall 的 CLI 消费端与样例自身接线仍属后续切片。

### 7.1 执行语义的解释（显式钉死，非发明）

「execute」在前片里锚定在两件既有事实上：

1. **样例声明了什么**：`entry = sample-driver executable app.bin`——executable 载荷是一个具名字节 entry，其 artifact 身份由公共 `derive_artifact_id(package_id, version, name)` 确定性派生，verify 时已物化进 artifact 权威。
2. **W30-B driver 契约定义了「在 provider 面上执行」是什么**：一个 operation 走 `register → dispatch → complete`；`complete` 的终局 outcome 由 `(operation, callback, seed)` 确定性派生，每个边界落真实持久回执。

因此前片的执行车道 = 解析已安装应用**当前代际**的 executable entry → 从 artifact 权威读回物化字节 → 以 `SHA-256("llmos/slice-k/payload-seed/v1" ‖ 载荷字节)` 为完成种子，经 `MockProvider`（绑定与 fiber 车道同一持久 `SqliteOperationStore`）驱动完整三边界。**载荷字节被 driver operation 消费**：终局 outcome 的类别与 receipt 是字节内容的纯函数（测试钉死：不同字节 ⇒ 不同终局；同身份下字节突变 ⇒ `CallbackIdentityConflict` typed 拒绝，绝不静默重派生）。不发明任何超出该面的运行时：无进程 spawn、无脚本解释器、无 wasm 引擎。

### 7.2 写集清单

- `crates/nlos-slice-k/src/payload.rs`（新模块：`execute_application_payload` / `PayloadExecution` / `payload_execution_seed` + 两个域分隔常量）；`src/runtime.rs`（`operations` 改 `Arc<SqliteOperationStore>`——车道把同一持久权威绑进 provider 面，不开第二连接，既有 fiber 车道调用点零改动）；`src/error.rs`（`Driver(ProviderError)` / `PayloadState(&'static str)`）；`Cargo.toml`（+`nlos-driver-mock` 依赖）
- `crates/nlos-slice-k/tests/payload_execution.rs`（车道无头测试 ×6）
- `crates/nlos-package/`（**新 crate**：`nlos-package` 二进制自 `nlos-artifact/src/bin/` 整体 `git mv` 而来——install 需链接 `nlos-application` 安装权威，后者依赖 `nlos-artifact`，原址即依赖环（W33-A 时点根 Cargo.toml 禁改才落 src/bin，见 B-ARTIFACT-007 §4；`#[cfg(test)]` 单测随 bin 原样迁移）；`tests/package_sdk_cli.rs` / `tests/package_conformance.rs` 随迁**逐字节不变**（W33-A/C 门零弱化，`CARGO_BIN_EXE` 随包可用）；`src/main.rs` 新增 `install` 子命令）；根 `Cargo.toml` workspace +1 member
- `crates/nlos-package/tests/install_payload_execution.rs`（端到端无头测试 ×2）
- `examples/sample-app/lifecycle.sh` + `README.md`（构建行 `cargo build -p nlos-package`；诚实范围首条改指本 §）、`examples/sample-app/Cargo.lock`（经 slice-k 传递 `nlos-driver-mock` 依赖边，cargo 自动落锁）
- `docs/developers/packaging.md`（§1/§6.1/退出码表/§7：install 用法与幂等纪律）、本 §、`docs/management/evidence-index.yaml`（本文件行 scope 增量）

### 7.3 CLI 面（install）

```text
nlos-package install <PKGFILE> --root <DIR>
```

decode → 在 root 上打开 slice-k 权威集（子路径与 `sample-app-driver` 同一落点）→ bootstrap 签名者 → 逐 entry 物化 → 权威 verify（`VERIFIED`/`REPLAYED`，与 verify 子命令同一条管线）→ W22-001 install-scoped 孤儿 GC → `install_application`（receipt digest-binding）。幂等/时钟键由 manifest digest / receipt id 域分隔派生：同包重装 `decision replayed`（代际不推进），异包同 root 并行安装互不冲突（slice-k 种子带助手做不到的多包同 root 语义）。typed 退出码：1-6 沿 W33-A 契约，**7 = 安装权威拒绝**。

### 7.4 验证证据（2026-09-21，macOS/arm64，HEAD `d1e0058`）

| 门 | 命令 | 结果 |
|---|---|---|
| 车道无头测试 | `cargo test -p nlos-slice-k --test payload_execution` | PASS：6 passed / 0 failed（回执真实可查、精确重放逐回执相同、载荷字节选择终局、已终局回调下突变 typed 拒绝、新代际执行新载荷、未安装态 typed 拒绝） |
| 端到端无头测试 | `cargo test -p nlos-package --test install_payload_execution` | PASS：2 passed / 0 failed（真 CLI keygen→build→install→车道执行→outcome 与公共 `derive_provider_outcome` 重算逐位相等→operation store 终局可查→重执行全 replay；负门：缺 `--root` exit 1、篡改载荷 exit 4、拒后零 durable 应用行） |
| slice-k 全量 | `cargo test -p nlos-slice-k` | PASS：28 passed / 0 failed（既有 22 原样绿 + 新 6） |
| nlos-package 全量 | `cargo test -p nlos-package` | PASS：34 passed / 0 failed（5 unit + 20 conformance + 7 sdk-cli 迁移原样绿 + 2 e2e） |
| nlos-artifact 全量（迁移回归） | `cargo test -p nlos-artifact` | PASS：82 passed / 0 failed |
| driver-mock 全量 | `cargo test -p nlos-driver-mock` | PASS：23 passed / 0 failed |
| clippy | `cargo clippy -p nlos-slice-k -p nlos-package -p nlos-artifact --all-targets --all-features -- -D warnings` | PASS：exit 0 |
| fmt | `cargo fmt -p nlos-slice-k -p nlos-package -p nlos-artifact -- --check` | PASS：exit 0 |
| 真机 CLI 冒烟 | `nlos-package install <sample-app 包> --root $STATE` | exit 0：`INSTALL 185f9e22…` `decision installed` `executables sample-driver`；同包二跑 `decision replayed` 且 INSTALL id 逐位相同（跨全新 root 亦逐位相同——receipt 派生的确定性） |
| 真机全生命周期回归 | `sh examples/sample-app/lifecycle.sh` | PASS：exit 0（`LIFECYCLE OK`，九步含真实 kill 链，迁移后 CLI 构建行生效） |

提交：`48d8b5e`（slice-k 车道）、`11bb214`（CLI 迁 crate + install + e2e）、`d1e0058`（clippy 清理 + 样板锁同步）、docs 提交（本 § + 索引 + packaging.md）。未 push（任务边界）。

### 7.5 前片诚实边界

- **「执行」= driver 面上的确定性执行**，不是把字节当机器码/脚本/wasm 跑起来——那是超出 W30-B 既有面的新运行时，明示不在前片发明范围。字节的消费点是完成种子：outcome 是字节的纯函数，回执链真实持久。
- **entry 的 role 声明不随 verify 持久化**（installation receipt 绑 manifest digest，不存解析后的 manifest）：车道按 manifest 声明的 entry name 经公共确定性派生解析 artifact，调用方负责命名其签名 manifest 声明为 `executable` 的 entry；「读回的是 head revision」——安装后被外写的 head（若发生）即被执行并在同身份下触发 typed 拒绝（负门钉死），跨代际换新 operation 身份执行新字节。
- **无 ambient fiber/task**：前片车道的 driver operation 身份（owner fiber/scope 同域派生）自持，不经 Task/Attempt/Process 绑定——那属于样例接线/后续切片的关联下沉面。
- **消费端 CLI 只收口 install**：`run`/`update`/`uninstall` 子命令未做（W33-H §3.2 既有登记项，移交 #2 后片/其它移交项）；样例 `sample-app-driver` 的 run 阶段未改接执行车道（README 诚实范围已同步改指本 §）。
- 幂等键域 `llmos/package-file/install-*` 与 `llmos/slice-k/payload-*` 为本切片新增，均与既有域（W33-A 的 `llmos/package-file/*`、样板的 `llmos/sample-app/*`）分隔。
- 未运行：`cargo test --workspace` / `cargo clippy --workspace`（波次屏障统一项）、三平台 CI（非 Unix 道未本机实测——车道本体无平台分支，真机链为 Unix）。
