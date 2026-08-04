# B-TASK-001：TaskAuthority fault-injection 增量证据（F1–F4 对齐矩阵）

> 状态：PARTIAL PASS（候选；本地复验通过，尚待三平台 CI 与 integrator 审议）
>
> 日期：2026-08-04
>
> 对应：`B-TASK-001` 耐久性不变量增量；故障矩阵对齐 PoC-0003 F1–F4（见 [poc-0003-sqlite-operation-authority.md](./poc-0003-sqlite-operation-authority.md)）；基线证据见 [b-task-001-task-authority-commit-permit.md](./b-task-001-task-authority-commit-permit.md)

## 1. 本切片完成的边界

基线证据（B-TASK-001 首个切片）已证明 `SqliteTaskAuthority` 在正常路径下的线性化与幂等语义，但其“durable”声明此前**未经任何故障注入验证**（基线 §5 明确列为非声明）。本切片把 `nlos-store` 的 PoC-0003 F1–F4 故障矩阵移植到 TaskAuthority 的 B-TASK-001 表组（`tasks` / `task_snapshots` / `task_attempts` / `commit_permits` / `task_cancels` / `task_receipts`），复用同一测试专用 `nlos-store-fault` VFS shim（`xWrite`/`xSync`/`xTruncate` 拦截，进程全局故障状态机），并复用 `nlos-store` 已确立的两套测试范式：

- **kill-9 子进程范式**（`fault_crash.rs`）：`current_exe` + 环境变量 scenario + 管道 `READY` 标记同步（无 sleep），`Child::kill` 强杀；
- **VFS 注入范式**（`fault_io.rs` / `fault_crash.rs`）：唯一 VFS 名注册、唯一临时库路径、`FAULT_LOCK` 进程内串行、disarm 后恢复验证、`PRAGMA integrity_check` 独立复核。

`nlos-task` 侧无需任何 `src/` 改动：`SqliteTaskAuthority::open_with_vfs` 与 `nlos-store` 同构（`OpenFlags::default()` + 命名 VFS + WAL/FULL 回读校验），注入点完整可用。**无 deviation，无 counter-evidence。**

## 2. 故障矩阵与结果

环境：Apple Silicon / arm64，macOS，rustc 1.97.x workspace toolchain，rusqlite 0.40 bundled SQLite。

新增 `crates/nlos-task/tests/fault_injection.rs`，6 个 `fault_*` 测试（每行一个，对应任务书矩阵）+ 1 个 kill-9 子进程 helper，全部通过：

| # | 场景 | 注入点 | 预期不变量 | 结果 |
|---|---|---|---|---|
| 1 | kill-9 等价：事务中断 | 子进程在 `BEGIN IMMEDIATE` 未提交（已弄脏 `tasks.revision` 与 `task_attempts.attempt_state`）时被 SIGKILL | 重开后中断事务完全回滚：revision 回到已提交值、attempt 仍为 `CREATED`；`commit_permits`/`task_receipts`/`task_cancels` 均为 0 行；authority 正常重开；`integrity_check = ok` | PASS（`fault_kill9_mid_transaction_leaves_no_traces`） |
| 2 | commit 后崩溃等价 | 子进程在 task 注册、permit 签发、finalize（Task A）与 cancel（Task B）全部提交返回后被 SIGKILL | 重开后全部已提交事实完整：A 的 head=1/新 root/permit `CLOSED`/attempt `COMMITTED`/commit receipt 逐字节一致；B 的 `cancel_epoch=1`/closure receipt（`CANCELLED_BEFORE_EFFECT`）/head 不变；permit 与 finalize 重放返回原 `Replayed` 结果 | PASS（`fault_kill9_after_commit_preserves_all_decisions`） |
| 3 | 写入硬 I/O 错误 | `FailWritesAfter { 0, IoErr }` 拦截 permit CAS 事务的首次 xWrite | `request_commit_permit` 以 `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件），不返回假成功、不 panic；无半截状态（`permit_epoch=0`、无 active permit、attempt 仍 `CREATED`、permit/receipt 表 0 行）；disarm 后同一请求成功签发 | PASS（`fault_io_error_propagates_typed_and_never_fakes_success`） |
| 4 | disk-full（ENOSPC） | `FailWritesAfter { 0, Full }` 拦截 finalize 事务（同事务写 receipt + 关 permit + attempt 终态 + head 推进） | finalize 以 `SQLITE_FULL` 显式失败（错误链含 full）；authority 不产生半截状态：receipt 表 0 行、permit 仍 `ISSUED`、attempt 仍 `COMMIT_PERMITTED`、`head_commit_seq=0`；disarm 后同一 finalize 成功推进 head=1 | PASS（`fault_disk_full_enospc_fails_closed_without_partial_state`） |
| 5 | 静默丢写/短写 | (a) `PowerLossAfter { 0 }`：permit CAS “报告成功”但 xWrite/xSync/xTruncate 全部静默丢弃；(b) kill-9 后文件级 WAL 撕裂：截断到最后一个 commit 帧的一半并删除 `-shm` | (a) 杀连接重开后幻影 permit 不得冒充已提交事实：`permit_epoch=0`、`PermitNotFound`、attempt 回到 `CREATED`；同一请求可重做且确定性派生的 permit id 相同、重开后真实持久；(b) 撕裂尾部整体隐藏（permit 签发不可见），此前合法提交（task+attempt）完整保留；重放同一请求重新签发成功，无冲突残留；两者 `integrity_check = ok` | PASS（`fault_silent_write_loss_and_torn_tail_hide_uncommitted_facts`） |
| 6 | 故障解除后恢复 | `FailWritesAfter { 0, Full }` 注入一次失败的 finalize 后 disarm，**同一 authority 实例**继续读写 | 已提交前缀与故障前逐位一致（head=0、`permit_epoch=1`、active permit 不变）；随后 finalize A、注册 attempt B、签发第二张 permit（`permit_epoch=2`、绑定 head=1）、finalize B 全部成功；完整重开后 head=2、双 attempt 均 `COMMITTED`、receipt 2 行 | PASS（`fault_after_disarm_authority_continues_from_committed_prefix`） |

诚实性说明（与 `nlos-store` 相同的边界）：shim 只拦截 `xWrite`/`xSync`/`xTruncate`，纯读路径无法直接注入失败；第 3/4 行对读路径断言的是 fail-closed 契约——写入失败期间读不 panic、且不返回与故障前已提交状态不一致的数据。kill-9 模拟的是**进程**崩溃（OS page cache 存活），不等于机器断电；机器断电语义由第 5 行 `PowerLossAfter` 与 WAL 撕裂覆盖。

## 3. 复验命令与结果

```sh
cargo test -p nlos-task --test fault_injection   # 7 passed; 0 failed（6 个 fault_* + 1 个 child helper no-op）
cargo test --workspace                           # 全部通过（45 个 test result: ok，0 failed）
cargo clippy --workspace --all-targets -- -D warnings   # 通过（exit 0）
rustfmt --edition 2024 --check crates/nlos-task/tests/fault_injection.rs   # 通过
```

说明：本切片写集内文件全部通过 rustfmt；`cargo fmt --all -- --check` 在复验时点仍对**并行进行中**的 B-TASK-002 切片文件（`crates/nlos-task/src/effect.rs` / `lib.rs` / `store.rs`，不在本切片写集）报未格式化 diff，属对方切片的瞬时状态，本文不代其声明。复验期间该并行切片曾短暂使 `nlos-task` lib 不可编译（`crate::effect` 未接线等），恢复后上述命令全部跑通；适配成本仅为测试 helper 中 `PermitRequest.planned_effects: Vec::new()`（空 effect 集 = 保持 pre-effect-slice 行为），未触碰任何 `src/` 文件。

## 4. 当前不能证明什么（限制与非声明）

- **macOS 本地 VFS 模拟 ≠ 真实硬件掉电**：`PowerLossAfter` 在 VFS 层丢弃写/sync/truncate，逼近“内核已接受但盘未见”的语义，但不覆盖真实断电下的介质行为、文件系统差异（APFS 以外）、或 `-shm`/mmap 在真断电下的损坏组合；真实 ENOSPC 的 RAM-volume 探针（`nlos-store` F3 已有）未在 TaskAuthority 上重做，disk-full 行为以注入 `SQLITE_FULL` 为准。
- **平台覆盖**：当前仅本机（macOS/arm64）复验；Ubuntu/Windows CI 尚未运行本测试文件（kill-9 子进程范式在 `nlos-store` 已三平台验证过，但本文件的三平台行为待 CI 确认）。
- **未覆盖 EffectPermit/effect-slot 表组**：并行进行中的 B-TASK-002 切片（`planned_effects` 等 effect 期表）不在本矩阵内；本证据仅覆盖 B-TASK-001 表组。effect 期表接入后需要按同一矩阵补一轮注入。
- **不声称 F4 全集**：checkpoint/backup/长 reader 矩阵（PoC-0003 F4）未对 TaskAuthority 重做；schema migration 故障注入（F5）待 TaskAuthority 出现 v2 schema 后再补。
- 单 authority、单进程 SQLite；不证明跨节点 consensus 或分布式 exactly-once。

因此本增量为单节点原机的 H3 级耐久性证据，状态 PARTIAL PASS 候选，不得据此声称 `B-TASK` 包完成或真实断电/多平台耐久性已证明。
