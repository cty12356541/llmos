# W82-C 托管约定 C 全量验收证据（D7 权威绑定与父退出重启）

- **证据 ID**：`w82-c-acceptance-20261121`
- **任务 / 波次 / 车道**：`W82-C-ACCEPT` / Wave `W82-R2`（`feat/w82-c-accept-r1`，基点 `ad0222566`）
- **日期**：2026-11-21
- **状态**：`LOCAL_ACCEPTANCE_PASS`（macOS 实机本地验收门全绿；L0/阶段状态登记归 Wave integrator，运行期吊销实时回收归 `W82-INT`）
- **依据**：[stage-e-progress.md §E.6.1.8](../../management/stage-e-progress.md#e618-w82-后续生态接入)、[w82-b-design-draft.md §7 (D7)](../../management/w82-b-design-draft.md#7-dllmos_root_capability_slot-值格式w82-cb-隐性契约本文钉死)、[w82-c-diag-20261116.md](./w82-c-diag-20261116.md)

## 1. 实现摘要（写集内增量）

1. **D7 根能力槽位格式（`^[0-9a-f]{32}$`）落地与 fail-closed 校验**
   - [managed_workers.rs](../../../crates/nlos-package/src/managed_workers.rs) 与 [lib.rs](../../../crates/nlos-package/src/lib.rs)：新增并导出 `is_valid_root_capability_slot(&str) -> bool`；解析 `slot = <name> <token>` 时严格校验 32 位小写 ASCII 十六进制（16B `CapabilityId`，不含 slot 序号、generation 或分隔符），未写 `slot` 行保持空串向后兼容；将单测示例占位符更新为合法 32-hex 并补齐正负例。
   - [managed.rs](../../../crates/nlos-process/src/managed.rs)、[supervisor.rs](../../../crates/nlos-process/src/supervisor.rs) 与 [lib.rs](../../../crates/nlos-process/src/lib.rs)：移除旧 `TODO 对表 W82-B 设计` 注释，新增并导出 `ManagedWorkerEnvError`、`parse_root_capability_slot(&str) -> Result<CapabilityId, ManagedWorkerEnvError>` 与 `validate_managed_worker_env(&ManagedWorkerEnv) -> Result<(), ManagedWorkerEnvError>`；`spawn_managed_worker` 与 `SupervisorError::InvalidManagedEnv` 在派生/拉起前同步 fail-closed 拒收非法环境变量。
2. **真实权威绑定校验与父退出同 `--root` 重启（`crates/nlos-up`）**
   - [lib.rs](../../../crates/nlos-up/src/lib.rs)：`bootstrap_trial_root` 按 ADR-0022 同源确定性参数签发/重放 operator 根能力并立即释放 SQLite 句柄，`BootstrapReceipt` 暴露 `operator_root_capability_id`、`operator_root_capability_hex` 与 `operator_root_replayed`；新增 `validate_worker_capability_slot`，对非空槽位校验 D7 格式、调用 `daemon.capabilities.inspect_active(CapabilityHandle { capability_id, generation: Generation::INITIAL }, at_ms)` 验证权威存在性/当前代际/未吊销/未过期/祖先有效，并校验 `record.holder == operator_principal`。
   - [main.rs](../../../crates/nlos-up/src/main.rs)：`print_bootstrap_summary` 与 `print_daemon_ready` 输出真实 `operator_root_capability=<32-hex>`；`run_workers` 在挂载信号监听 `ShutdownSignals::install()` 后、拉起任何受管单元前对全清单槽位执行权威预检（`spawn_worker_unit` 内亦保留同型纵深校验），任一伪造/未知/已吊销/holder 不匹配槽位均 fail-closed 拒绝拉起、清停内嵌守护与 Unix socket 后非零退出；`ShutdownSignals` 在整个 `run_workers` 周期保持常驻监听，避免 200ms 轮询间隙丢信号，确保父退出后立即使用同一 `--root` 重启可无冲突再次 `READY service=system_control workers=2`。

## 2. 验证环境与真实命令输出

- **实机环境**：`Darwin 25.6.0` / `macOS 26.6.2 (25G83)` arm64、`rustc 1.99.0 (b940084d7 2026-09-28)`、`Python 3.13.12`。
- **执行目录**：`/Users/lipunima/.gemini/antigravity/brain/875ab7ba-845f-40de-8ce8-81186117c749/scratch/wt-w82-c-accept`

| 验证命令 | 退出码 | 实测计数 / 结果 |
|---|---:|---|
| `cargo test -p nlos-package -p nlos-process -p nlos-up` | `0` | **26 个可执行测试集共 184 passed, 0 failed, 0 ignored**（另 3 个 doc-test 集 0 tests）；分 crate：`nlos-package` 61/61（9 集）、`nlos-process` 96/96（12 集）、`nlos-up` 27/27（5 集，含 `bootstrap_smoke` 7/7、`workers_e2e` 9/9、`workers_readiness` 2/2） |
| `cargo clippy -p nlos-package -p nlos-process -p nlos-up --all-targets -- -D warnings` | `0` | 零警告、零错误通过 |
| `cargo fmt --all -- --check` | `0` | 全工作区格式检查通过 |
| `git diff --check` | `0` | 零空白/冲突标记问题 |

## 3. 正负例与边界覆盖核验

- **D7 格式校验正负例**（[managed_workers.rs](../../../crates/nlos-package/src/managed_workers.rs)、[managed.rs](../../../crates/nlos-process/src/managed.rs)、[managed_env.rs](../../../crates/nlos-process/tests/managed_env.rs)）：
  - 正例：32 位小写十六进制 `^[0-9a-f]{32}$` 精确解析为 16 字节 `CapabilityId`；清单省略 `slot` 行时保持 `""` 向后兼容。
  - 负例：拒绝大写十六进制、混合大小写、31/33 位长度错误、非十六进制字符（如 `g`）、`-`/`:` 分隔符、`0x` 前缀及旧占位符文本；`spawn_managed_worker` 收到非法非空 `root_capability_slot` 时在 spawn 前 fail-closed 返回 `SupervisorError::InvalidManagedEnv`（零子进程拉起）。
- **权威绑定校验与部分失败回收**（[bootstrap_smoke.rs](../../../crates/nlos-up/tests/bootstrap_smoke.rs)、[workers_e2e.rs](../../../crates/nlos-up/tests/workers_e2e.rs)）：
  - `workers_launch_env_whitelist_and_sigterm_reclaims_tree`：从 `bootstrap_trial_root` 获取真实根 `CapabilityId`（并核对 `READY` 行 `operator_root_capability=<32-hex>`），以双单元（`adapter` + `brain`）拉起，实测子进程仅注入三项 `LLMOS_*` 且 `PATH`/`HOME` 等父环境零泄漏；`SIGTERM` 主进程后直子与 `sleep 300` 孙进程全部退出（`pid_gone`）。
  - `workers_python_unit_env_whitelist_gate_letter`：保留无 `slot` 行向后兼容行为并验证 `python3` 子进程环境变量最小化。
  - `workers_rejects_forged_and_malformed_slots_fail_closed_zero_residual`：伪造 32-hex `deadbeefdeadbeefdeadbeefdeadbeef` 及畸形 `slot` 在权威预检处 fail-closed 退出非零、守护干净 `STOPPED`、零进程残留（`.pid`/`.env` 未生成）。
  - `workers_rejects_revoked_capability_slot_fail_closed` 与 `workers_rejects_holder_mismatch_capability_slot_fail_closed`：已通过真实签名 `revoke_signed` 吊销的根能力、以及委派给其他主体 (`holder != operator_principal`) 的有效能力均在 `nlos-up workers` 启动阶段 fail-closed 拒绝，零进程残留；`bootstrap_smoke.rs` 另覆盖跨平台库级权威委派/祖先吊销/holder 检验。
  - `workers_partial_spawn_failure_still_reclaims_launched_trees`：首单元启动后次单元失败时先同步升级组回收首单元（`KILL ... group_drained=true`）并关停守护，再非零退出。
- **父退出后同 `--root` 重启与安全回收**（[workers_e2e.rs](../../../crates/nlos-up/tests/workers_e2e.rs) `workers_restart_on_same_root_after_sigterm_reclaims_and_restarts_cleanly`）：
  - Run 1 以真实根 `CapabilityId` 拉起双单元（含孙进程），验证三变量与 `READY service=system_control workers=2`，向父进程发 `SIGTERM` 后验证 `KILL ... group_drained=true`、`STOPPED` 与全部直子/孙进程 `pid_gone`。
  - Run 2 紧接以同一 `--root` 与同一 `workers.manifest` 立即重启 `nlos-up workers`，无 socket/SQLite 锁/进程冲突，正常产出新进程组、新 PID 与 `READY service=system_control workers=2`，再次 `SIGTERM` 后全树回收干净。

## 4. 诚实保留边界（不越界宣称）

1. **跨权限 / breakaway 边界仍归 `U-2`**：本车道使用 `process_group(0)` + `killpg`（Unix）实现同权限用户态受管进程树回收，不覆盖跨特权/OS sandbox breakaway 逃逸；Windows 侧 `taskkill /F /T` 保持单级强杀无宽限二级，且本次实机未在 Windows/Linux 运行 `cfg(unix)` 的 `workers_e2e`（跨平台单元逻辑与 `bootstrap_smoke` 已编译通过并在 macOS arm64 实测）。
2. **令牌吊销触发正在运行进程树的实时回收联动归 `W82-INT`**：本车道已闭合 D7 格式校验、启动准入时的能力权威存在性/未吊销/holder 绑定校验（含吊销后拒绝拉起）以及组级回收原语与 `KillScope::ProcessGroup` 事实记录；**运行中**能力吊销信号驱动 `ProcessAuthorityKillExecutor` 触发实时树回收的端到端接线验收仍归 `W82-INT`。
3. **父进程被 `SIGKILL` 瞬时孤儿窗口**：因工作区 `unsafe_code = "forbid"` 不使用 `PDEATHSIG`，主进程若遭遇不可捕获的 `SIGKILL`，残存进程组只能由外部基于稳定 `pgid` 清理；正常退出与 `SIGTERM`/`SIGINT` 均经 `ShutdownSignals` 完整回收。
