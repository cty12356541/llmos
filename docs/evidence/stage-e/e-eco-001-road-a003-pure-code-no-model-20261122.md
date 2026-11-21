# E-ECO-001：`ROAD-A-003` / `NLOS-GEN-001` 无模型纯代码应用端到端纵切面证据（`DEL-ECO-PURE-CODE`）

- **Evidence ID**: `E-ECO-001`（文件：`e-eco-001-road-a003-pure-code-no-model-20261122`）
- **Wave / Lane**: `W82-R4-IND` Wave 1 / `DEL-ECO-PURE-CODE`（分支 `feat/del-eco-pure-code`）
- **日期**: `2026-11-22`
- **基线提交**: `05f967451feaab9f99f84e397445ec3bffe03a6c`
- **Assurance Level**: `H4`（单机本地多 OS 子进程隔离 + 真实 `SIGKILL` / `SIGTERM` 强杀恢复 + 持久权威提交与卸载门禁集成闭环；`H8 = 0`）
- **状态**: `LOCAL_CONSTRUCTIVE_PASS`（macOS arm64 实测全绿；共享台账 `evidence-index.yaml` / `claims.yaml` 索引登记按多轨写集纪律移交 Wave Integrator）
- **权威规范与计划依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §1.1（`[NLOS-GEN-001]`）、§23.1（`[PKG-UNINSTALL-001]`）、§27（`Fast` 工作负载路径）、§28.1（`[ROAD-A-003]`）
  - [del-base-coverage-audit.md](../../management/del-base-coverage-audit.md) §2.1 #1、`ROAD-A-003` 行
  - [nlos-goal-delivery-plan.md](../../management/nlos-goal-delivery-plan.md) §5 `DEL-ECO`（无模型 pure-code 专项覆盖要求）、§6 `DEL-ACCEPT`
- **独占写集交付物**:
  1. [crates/nlos-slice-k/tests/road_a003_pure_code_e2e.rs](../../../crates/nlos-slice-k/tests/road_a003_pure_code_e2e.rs)
  2. [docs/evidence/stage-e/e-eco-001-road-a003-pure-code-no-model-20261122.md](./e-eco-001-road-a003-pure-code-no-model-20261122.md)（本文件）

---

## 1. 零模型 / 零 HTTP 客户端依赖实查（`cargo tree -p nlos-slice-k`）

在测试工作树基线 `05f967451feaab9f99f84e397445ec3bffe03a6c` 执行 `cargo tree -p nlos-slice-k` 与传递依赖全量检索：

```bash
cargo tree -p nlos-slice-k | grep -i -E "llm|http|reqwest|ureq|hyper|tonic|curl" || echo "NO_LLM_HTTP_MATCH"
# 输出: NO_LLM_HTTP_MATCH
```

核查结论（结合 [crates/nlos-slice-k/Cargo.toml](../../../crates/nlos-slice-k/Cargo.toml) 与 `cargo tree -p nlos-slice-k -e normal --prefix none | sort -u`）：

1. `nlos-slice-k` 的全部直接与传递依赖中**零包含** `nlos-driver-llm-http`、`reqwest`、`ureq`、`hyper`、`tonic`、`curl` 或任何外部 LLM / HTTP 客户端 crate；
2. 密码学与持久存储仅依赖本地库（`ed25519-dalek v3.0.0`、`sha2 v0.11.1`、`rusqlite v0.40.2`、`tokio v1.54.1`、`nix v0.31.3` 等）；
3. `nlos-slice-k` 因保留既有 payload 样例测试（[payload.rs](../../../crates/nlos-slice-k/src/payload.rs)）而在依赖树中含有 `nlos-driver-mock`，但本专项端到端测试 [road_a003_pure_code_e2e.rs](../../../crates/nlos-slice-k/tests/road_a003_pure_code_e2e.rs) 全程**不调用任何 `nlos-driver-mock`、不引用任何模型驱动接口**，仅通过独立 OS 纯代码子进程完成确定性计算并通过 `WriteFiberJob`（`SqliteOperationStore` + `ArtifactStore` + `SqliteTaskAuthority` + `ArtifactCommitCoordinator`）直接收口。

---

## 2. 隔离执行设计（多 OS 子进程 + `.env_clear()` + 超时/RAII 护卫）

- **单一子进程入口**：`#[ignore] fn road_a003_pure_code_child_worker()`；未设置 `NLOS_PURE_CODE_WORKER` 时立即 `return`（`--include-ignored` 下零副作用通过）。
- **干净环境构造**：父测试通过 `Command::new(std::env::current_exe().expect("current_exe"))` 传入 `["--exact", "road_a003_pure_code_child_worker", "--ignored", "--nocapture"]` 并在注入参数前先调用 `.env_clear()`，仅注入非敏感确定性工作参数（`NLOS_PURE_CODE_WORKER`、`NLOS_PURE_CODE_MODE`、`NLOS_PURE_CODE_INPUT`、`NLOS_PURE_CODE_OUTPUT`、`NLOS_PURE_CODE_REPORT`、`NLOS_PURE_CODE_READY`、`NLOS_PURE_CODE_STOP`）。
- **子进程启动首步自证**：调用 `audit_child_env_strict()` 读取全部 `std::env::vars()`，把键列表与全量 `key=value` 审计行写入 `NLOS_PURE_CODE_REPORT`，并严格断言：
  - 不存在任何 `OPENAI_API_KEY`、`ANTHROPIC_API_KEY`、`GLM_API_KEY`、`ZHIPU_API_KEY`、`DEEPSEEK_API_KEY`、`GEMINI_API_KEY`、`AZURE_OPENAI_API_KEY`、`LLMOS_KEYPOOL_CONFIG`、`LLMOS_LLM_BASE_URL`、`LLMOS_LLM_MODEL`、`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY`（含子串匹配）；
  - 全部可见变量严格满足 `NLOS_PURE_CODE_*` 白名单前缀（零宿主机 `PATH`/`HOME`/`CARGO_*` 泄漏）。
- **防挂死护卫**：父侧封装 `ChildGuard`（20s `CHILD_WAIT_TIMEOUT` 超时轮询 + `Drop` 强制 `kill()` 与 `wait()` 回收），子进程持驻留模式受 `CHILD_HOLD_CAP`（30s）与停止标记兜底，确保失败或超时场景均无孤儿进程泄漏。

---

## 3. 端到端覆盖场景与不变量核验

### 3.1 用例 1：`road_a003_pure_code_zero_model_install_fast_write_crash_recover_uninstall`

| 阶段 | 执行路径 | 实测不变量与权威状态核验 |
|---|---|---|
| **1. 纯代码清单开发/签名/验签/安装** | 构造仅含 2 个 `PackageTaskKind::Executable` 节点（零 `AgentRole`）的 `SignedPackageWithTasks`，Ed25519 签名 | `verify_package_with_tasks` 签发不可变收据；`compile_task_templates` 产出 2 个 `PlanNodeKind::Executable` 节点（0 `AgentRole`）；`install_verified_package` 落盘代际 `1`、`ApplicationStatus::Installed` |
| **2. Fast 无模型执行与持久化（Rev 1）** | 派生隔离纯代码子进程 `worker_1`（`mode=compute-and-hold`，输入 `records=alpha:10,beta:25,gamma:65`）计算确定性账本（`checksum_sum=100`）与 SHA-256 自证报告；登记真实 OS PID 至 `SupervisorPidRegistry`；经 `Task`/`Attempt` + `CommitPermit` + `WriteFiberJob` + `converge_pending` 提交用户 `Artifact` | 零模型驱动/HTTP 调用；`env_clear` 审计通过（`forbidden_llm_or_proxy_keys=0`）；产生 `TaskCommitReceipt`（`new_head_commit_seq=1`），`AttemptState::Committed`、`PermitState::Closed`；`resolve_head == 1`，`get_revision(1)` 字节与 SHA-256 逐位一致 |
| **3. 真实 `SIGKILL` 崩溃注入与无损恢复（Rev 2）** | 派生 `worker_2`（`mode=barrier-before-write`）停在写入前屏障后由父进程执行 `child.kill()` + `child.wait()`；记录 `propagate_crash` 与 `cancel_task`；随后在 `task_1` 上登记 `attempt_rev2`（锚定 `expected_head_commit_seq=1`）并派生 `worker_3`（`mode=recover-compute`，串联 `prev_sha256`、`checksum_sum=200`）完成提交 | `worker_2` 非零信号退出；崩溃与取消后 `Artifact` head 稳停在 Revision `1`、`list_revisions == 1`、`list_incomplete_artifact_commit_plans == 0`，被取消 Attempt 申请 `CommitPermit` 返回 `CancelledBeforeEffect`；`worker_3` 恢复提交产生 `TaskCommitReceipt`（`prior_head_commit_seq=1 -> new_head_commit_seq=2`），Rev 1 与 Rev 2 均可读且 SHA-256 完整 |
| **4. 活动门控卸载与 `PKG-UNINSTALL-001` 用户数据保护** | 后台 `task_1` 活跃时探针调用 `uninstall_application_gated_by_task_activity`；随后执行 `run_application_teardown` 并重放一次；验证卸载后重装拒绝、执行 `collect_orphan_blobs` 并 drop + reopen `SliceKRuntime` | 卸载门控在活跃任务未终结前返回 `ApplicationActiveTasksRunning { active_task_count: 1 }`；`teardown` 通过 `PosixPlatformKillAdapter` 向 `worker_1` 发送 `SIGTERM`（退出非零）、`ProcessLifecycleState::Crashed`、`TaskState::Cancelled`、`ApplicationStatus::Uninstalled`，重放回执逐字节一致（`PlatformKillDecision::Replayed` / `CancelDecision::Replayed` / 相同 `UninstallReceipt`）；孤儿 GC 与重启重开后用户 `Artifact` 的 Rev 1 与 Rev 2 字节及 SHA-256 依然完好可读 |

### 3.2 用例 2：`road_a003_pure_code_negative_tampered_package_and_env_guard`

1. **篡改清单验签失败与零权威状态写入**：篡改已签名纯代码包的 `manifest.version` 以及篡改 `tasks[0].kind`（`Executable -> AgentRole`）均在 `verify_package_with_tasks` 返回 `ArtifactError::PackageSignatureInvalid`，随后核验 `applications.inspect_application == None` 且 `plans.list_plan_ids` 为空。
2. **未解绑/未清停前卸载拒绝**：安装有效包并登记活跃任务与进程绑定后，活跃任务阻断门控卸载（`ApplicationActiveTasksRunning`）；在 Unix 上向 `run_application_teardown` 传入空 `SupervisorPidRegistry` 触发 `ProcessAuthorityError::PlatformKillAdapter(_)` 拒绝且 `ApplicationStatus` 保持 `Installed`；补齐绑定后完成清理卸载。
3. **跨投毒父进程的双跳 `.env_clear()` 隔离证明**：父测试先派生带 9 个模拟密钥与代理变量（`OPENAI_API_KEY`、`ANTHROPIC_API_KEY`、`GLM_API_KEY`、`ZHIPU_API_KEY`、`LLMOS_KEYPOOL_CONFIG`、`LLMOS_LLM_BASE_URL`、`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`）的中间子进程（确认其自身确实观测到 `poisoned_keys_seen=9`），再由该中间子进程以 `.env_clear()` 派生纯代码子进程，审计证实孙子进程仅见 `NLOS_PURE_CODE_*` 且 `forbidden_llm_or_proxy_keys=0`。

---

## 4. 实机验证记录与测试计数

- **实测环境**：`Darwin 25.6.0` / `macOS` arm64、`rustc 1.99.0 (b940084d7 2026-09-28)`、`cargo 1.99.0 (5f94df478 2026-08-27)`
- **工作树目录**：`/Users/lipunima/.gemini/antigravity/brain/875ab7ba-845f-40de-8ce8-81186117c749/scratch/wt-del-eco-pure-code`

| 验证命令 | 退出码 | 实测结果与计数 |
|---|---:|---|
| `cargo tree -p nlos-slice-k \| grep -i -E "llm\|http\|reqwest\|ureq\|hyper\|tonic\|curl"` | `0`（匹配为空） | 零 LLM/HTTP 客户端依赖（`NO_LLM_HTTP_MATCH`） |
| `cargo fmt -p nlos-slice-k -- --check` | `0` | 零格式差异 |
| `cargo clippy -p nlos-slice-k --all-targets -- -D warnings` | `0` | 0 error / 0 warning（含 `clippy::pedantic` 与 `#![forbid(unsafe_code)]`） |
| `cargo test -p nlos-slice-k --test road_a003_pure_code_e2e` | `0` | **2 passed; 0 failed; 1 ignored**（`road_a003_pure_code_child_worker` 由父测试通过 `current_exe()` 实际派生运行） |
| `cargo test -p nlos-slice-k --test road_a003_pure_code_e2e -- --include-ignored` | `0` | **3 passed; 0 failed; 0 ignored**（验证无控制环境变量时子进程入口立即安全返回） |
| `cargo test -p nlos-slice-k --test third_party_sample_lifecycle` | `0` | **1 passed; 0 failed; 0 ignored** |
| `cargo test -p nlos-slice-k` | `0` | 共 `39` 个测试目标（`lib` 单测 `1`、`bin` `1`、集成测试 `36`、`doc-test` `1`）：**129 passed; 0 failed; 18 ignored**（全部 `18` 个忽略项均为由父测试通过 `current_exe()` 调起的子进程/Cell worker 入口） |

---

## 5. 诚实边界与未完成外部验收项（严禁越界宣称）

1. **仓内无头夹具构造性闭环（`H4`）≠ 外部独立第三方正式交付生态验收（`H8 = 0`）**：
   本闭环在仓库内部以无头 Rust 集成测试夹具及同二进制多 OS 子进程在**单机 macOS arm64** 上完成，严格证明了内核纵切面、包验签、持久 `Task`/`Attempt`/`CommitPermit`/`Artifact`、崩溃恢复与卸载保护链路**不依赖任何模型 provider/凭据或 HTTP 客户端即可端到端闭环**；但它**尚未构成**「与本仓作者无关的外部独立第三方团队基于正式发布安装器与独立 SDK 在干净宿主上自行构建、安装并运行纯代码应用」的最终生态验收（该项仍归 `DEL-DIST`、`DEL-SDK`、`DEL-ECO` 与 `DEL-ACCEPT`，全仓 `H8 = 0`）。
2. **宿主平台与 CI 矩阵边界**：
   本次实测仅在单机 `Darwin` arm64 运行；[road_a003_pure_code_e2e.rs](../../../crates/nlos-slice-k/tests/road_a003_pure_code_e2e.rs) 对非 Unix 宿主的差异分支（`NoopPlatformKillAdapter` 与 stop-flag 退出路径）已通过交叉可读代码结构约束，但尚未在 Linux 与 Windows 实机或三平台 CI 上实跑。
3. **无模型环境强隔离边界**：
   本测试通过依赖图审查（零 LLM/HTTP 客户端 crate）与子进程 `.env_clear()`（零模型密钥、零代理变量）构造性保证纯代码工作负载不依赖模型与外连；测试内未在操作系统防火墙层额外挂接内核级网络策略拦截器（系统级的双层沙箱网络拒绝见 [E-SANDBOX-001](./e-sandbox-001-cross-platform-wasm-process-isolation.md)）。
