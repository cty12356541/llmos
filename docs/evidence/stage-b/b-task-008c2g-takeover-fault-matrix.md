# B-TASK-008C2G-FAULT：takeover fence 表组故障注入矩阵

状态：`PARTIAL_PASS`（2026-08-17）

## 1. 结论

本切片把 `b-task-008c2g-semantic-coordinator.md` 明确保留的"完整故障矩阵仍未接入"缺口对 schema v27–v35 的 lease/takeover 表组收口：以 PoC-0003 对齐的 F1–F4 故障矩阵（kill-9 中断事务 / commit 后崩溃 / 硬 I/O 错误 / disk-full / 静默丢写与 WAL 撕裂尾部 / 故障解除后继续）验证 2026-08-16 落地的 authority lease、takeover fence、assignment、pending takeover receipt、barrier observation 与 exact fence member manifest 的 durable 前缀语义。测试只走公开 API（`acquire_authority_lease`、`request_commit_permit_with_authority_lease`、`finalize_commit_v3_with_authority_lease`、`prepare_authority_takeover_fence`、`record_authority_takeover_barrier_receipt` 及只读 inspect），复用 `fault_injection.rs` / `effect_fault_injection.rs` / `reconcile_fault_injection.rs` 的 `nlos-store-fault` VFS 与 kill-9 子进程范式；`nlos-task` 零 `src/` 改动。

## 2. 已实现事实

- **F1 kill-9 中断事务**：子进程在 `BEGIN IMMEDIATE` 未提交（已写入幻影 fence receipt（真实 registry binding，若存活将与真实 receipt 撞 UNIQUE）、幻影 fence member、幻影 Active assignment、幻影 pending takeover receipt、幻影 barrier observation、幻影 term-2 lease history，并弄脏 `commit_permits.permit_state` CAS）时被强杀；重开后中断事务完全回滚——takeover 六表无幻影行、lease history 恰好 2 行（term-1 acquire + term-2 takeover）、assignment 恰好 1 行 Active、permit 回到已提交 `Closed`、registry 保持 `FrozenForPermit`、`control_epoch` 不动；随后同一 takeover fence 重做成功且确定性派生 receipt id 一致，重放不再推进 `control_epoch`。
- **F2 commit 后崩溃**：子进程在完整 takeover 链（term-2 lease、fence receipt 含 exact roots、member manifest、assignment `TakeoverPending`、pending takeover receipt、v35 barrier observation 含 digest）全部提交返回后被强杀；重开后全部逐位保留（registry `FrozenForTakeover`、`control_epoch` 恰好 +1、barrier digest 持久、coverage `LocallyCovered`）；fence 与 barrier 重放返回原结果、`control_epoch` 不再推进、observation 不重复。
- **F3 硬 I/O 错误**：`FailWritesAfter { 0, IoErr }` 下（a）takeover fence 事务与（b）barrier observation 事务都以 `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件）、`writes_observed() > 0`，无半截状态（无 fence/member/takeover/barrier 行、registry 保持 `FrozenForPermit`、assignment 保持 `Active`、`control_epoch` 不动）；disarm 后同一操作成功，coverage 升至 `LocallyCovered`。
- **F4 disk-full**：`FailWritesAfter { 0, Full }` 下同一对事务以 `SQLITE_FULL`（错误链含 full）显式失败且无半截状态；disarm 后同一操作成功。
- **F5 静默丢写/撕裂尾部**：`PowerLossAfter { 0 }` 下 barrier observation "报告成功"但写入从未落盘，重开后幻影 observation 不可见（无 barrier 行、coverage `Partial`/0 observed）、同一请求重做且确定性派生的 barrier receipt id 逐位相同、重开后真实持久；WAL 截断到最后一个 commit 帧一半时，隐藏整个 fence 事务（registry 回到 `FrozenForPermit`、六表无行、`control_epoch` 不前进，重做 fence 且 receipt id 一致、`control_epoch` 恰好 +1）或只隐藏 barrier 事务（fence 前缀完整、barrier 重做 receipt id 逐位一致）。
- **F6 故障解除后继续**：fence 写事务在 `FailWritesAfter { 0, Full }` 下失败后 disarm，同一 authority 实例的已提交前缀与故障前逐位一致，fence 重试成功、barrier 成功、coverage `LocallyCovered`；完整重开后全部状态可恢复。

每个场景结束时都通过独立 rusqlite 连接执行 `PRAGMA integrity_check` 复核为 `ok`。

## 3. Evidence

- `cargo test -p nlos-task --test takeover_fault_injection`：7 项通过（`crash_child_helper` 无环境变量时为空操作；6 个矩阵行全绿）。子进程经 `current_exe + --exact crash_child_helper + 环境变量 + piped READY` 同步，kill-9 后断言非正常退出。
- `cargo clippy -p nlos-task --all-targets -- -D warnings`：通过（对单测试覆盖完整矩阵行的长函数显式 `#[allow(clippy::too_many_lines)]`，与既有 fault 测试一致）。
- `cargo test -p nlos-task --quiet`：TaskAuthority 全部测试通过（含既有 lease/takeover 验收、reconcile/effect fault 矩阵与全部迁移链）。
- `cargo test --workspace --quiet`：workspace 383 项测试全过（本切片新增 7 项）。
- 三平台 CI 尚未运行本文件（本地 macOS/arm64 证据）。

## 4. 明确限制

- kill-9 是强制终止子进程的进程崩溃模型，不是真实断电；磁盘层写入由 `PowerLossAfter` 与 WAL 撕裂尾部覆盖，仍无真实硬件掉电证据。
- 覆盖表组：`task_authority_leases`/`task_authority_lease_history`（v27）、`task_authority_takeover_fence_receipts`（v30）、`task_authority_assignments`（v31）、`task_authority_takeover_receipts`（v32）、`task_authority_takeover_barrier_receipts`（v33/v35）、`task_authority_takeover_fence_members`（v34），以及 registry 冻结 CAS（`task_participant_registries.registry_state`）与 `tasks.control_epoch` 推进。v28/v29 在 `commit_permits` / `task_adoption_receipts` 的 lease-binding 列不在本矩阵逐列注入（其中 permit 行以 mid-tx CAS 弄脏覆盖），adoption/reconcile 表组的既有矩阵见 `reconcile_fault_injection.rs`。
- F4 全集（checkpoint / backup / migration 对 v27–v35 表组的变体）未覆盖；本矩阵聚焦写路径原子性。
- 本地 macOS 单机证据；三平台 CI、真实 ENOSPC 探针与更多文件系统未运行。
- 本矩阵只证明本地 durable 前缀的原子性/可恢复性，不证明 IPC peer 认证、远端 barrier 验证/完成、successor assignment 激活或跨 term adoption（这些仍为下一验收门）。
