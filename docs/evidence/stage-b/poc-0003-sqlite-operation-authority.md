# PoC-0003：SQLite Operation Authority 初始证据

> 状态：PARTIAL PASS
>
> 日期：2026-07-29（F1–F4 fault-injection 切片于 2026-08-01 复验）
>
> 对应：`MODEL-OP-001`、`IO-ASYNC-001`、`IO-CANCEL-001`、`DUR-ACK-001`、`DUR-RECOVER-001`

## 实现

新增 `nlos-store`：

- SQLite WAL；
- `synchronous=FULL`；
- 进程内单写者 admission + `BEGIN IMMEDIATE`；
- schema v1、`STRICT` tables、固定宽度 ID/epoch 编码；
- Operation revision CAS；
- exact `OperationSpec` registration 幂等；
- dispatched CallbackId/CancelEpoch durable binding；
- Operation terminal state + Receipt + Outbox 同事务；
- `WakeFiber` 与 `ReconcileEffect` 分流；
- outbox at-least-once delivery + 幂等 ACK；
- 未知 schema version fail-closed。

`nlos-operation` 同时抽取出可恢复的 `OperationMachine`，使内存 Registry 与 SQLite authority 共用同一状态转换语义。dispatch 后的票据替换现在会被拒绝。

## 事务边界

```text
BEGIN IMMEDIATE
  load + invariant validation
  apply OperationMachine transition
  revision compare-and-swap
  append WakeFiber | ReconcileEffect
COMMIT
  → 才向调用者返回成功
```

Outbox ACK 是后续独立事务。Consumer 崩溃可造成重复投递，所以 wake/reconcile consumer 仍必须按 Operation/Fiber generation 幂等；但 Operation 提交与待投递事件不会出现一个成功、另一个缺失。

## 测试

复现：

```sh
cargo test -p nlos-store
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

当前覆盖：

1. 相同 OperationSpec registration 重放返回同一 handle；
2. 同 OperationId 的不同 spec 被拒绝；
3. completion、Receipt 和 Wake Outbox 在关闭并重开数据库后仍存在；
4. duplicate callback 不产生第二条 Outbox；
5. Outbox ACK 可幂等重放且重开后不再列出；
6. cancel 后迟到 callback durable 路由到 reconciliation；
7. dispatch 前 cancel 同事务产生 `CANCELLED_BEFORE_EFFECT` 和 wake；
8. forged CallbackId 整笔回滚，不改变 `DISPATCHED` 状态；
9. SQLite authority 上 cancel/completion 竞态重复 64 次，只出现两个合法串行化结果；
10. completion 已返回 durable ACK 后，测试子进程以 `exit(97)` 跳过析构退出，父进程重开仍恢复终态与未 ACK Outbox。

## B-STORE-FAULT F1–F4 增量证据（2026-08-01）

本切片新增测试专用 `nlos-store-fault` crate，把 `unsafe` 限定在一个有逐处 `SAFETY` 说明的 SQLite VFS shim 审计单元。shim 复制默认 VFS 并按文件包装 `xWrite`、`xSync`、`xTruncate` 与 `xClose`，支持硬 `SQLITE_IOERR`/`SQLITE_FULL` 和“报告成功但不落盘”的断电模型；`SqliteOperationStore::open_with_vfs` 允许测试选择命名 VFS，正常生产入口仍使用默认 VFS。打开 authority 时会回读 `journal_mode=WAL` 与 `synchronous=FULL`，不接受 SQLite 静默回退。

环境：Apple Silicon / arm64，macOS 26.5.2（Build 25F84），rustc 1.97.1，cargo 1.97.1，rusqlite 0.40.1 bundled SQLite。

新增并通过 25 项 F1–F4 验收测试：

1. **F1 kill-9**：事务中途 `SIGKILL` 后只有 register+dispatch 可恢复，Receipt/Outbox 不出现半提交；commit 返回后 `SIGKILL` 保留终态、Receipt 与 Outbox；consumer apply 后、ACK 前被杀，条目可重投。
2. **F2 torn-write / power loss**：扫描 commit 写路径的每个注入点，未完成写入不能冒充 durable commit；静默丢写后重开回到先前 durable 状态且允许重新提交；WAL 半帧截断与后续帧损坏只隐藏不完整尾部，保留此前合法提交；删除可重建的 `-shm` 后恢复正常。
3. **F3 disk-full / read-only / I/O error**：注入 `SQLITE_FULL` 与 `SQLITE_IOERR` 时 complete/ACK fail-closed，状态和 Outbox 保持故障前一致，解除故障后可继续；只读主文件在有/无 WAL side files 两种情况下可读、写入明确失败且恢复权限后继续；macOS RAM volume 的真实 ENOSPC 探针实际执行并通过，进程保持存活、既有提交可恢复。
4. **F4 checkpoint / backup / long reader**：PASSIVE/FULL/RESTART/TRUNCATE 的返回值与 WAL 文件行为一致；长读事务不阻塞 writer，但会阻止 checkpoint 越过 reader end-mark，释放后可 TRUNCATE；SQLite online backup 产生完整可打开副本；复制 `db+wal+shm` 保留未 checkpoint 提交，而只复制主文件会丢失这些提交但不产生伪造状态；并发 writer 下 backup 只允许一致快照或显式错误。

复验命令：

```sh
cargo test -p nlos-store --tests
cargo test -p nlos-store --test fault_io real_full_disk_returns_full_error_and_process_survives -- --nocapture
cargo test -p nlos-store --test fault_crash -- --nocapture
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

上述命令全部通过；workspace 的 100K waiting Fiber 显式规模探针仍为 ignored，且它不等价于本工作包要求的 100K Operation metadata。

## B-STORE-FAULT F5 schema migration 增量证据（2026-08-02）

durable format 从 v1 前向演进到 v2。v2 新增 `operation_outbox_by_operation(operation_id, operation_generation, sequence)` 索引，为后续按 Operation 恢复等待与协调状态提供有界查询入口，避免依赖全 Outbox 扫描；公共 Rust API 和既有行编码不变。

迁移采用独立 `BEGIN IMMEDIATE` 事务：新数据库先完整创建 v1，再迁移到 v2；旧 v1 数据库直接迁移；未知版本继续 fail-closed。新增可审计的 `authority-v1.sql` golden fixture，固定一个已完成 Operation、Callback fence、Receipt 与未 ACK Outbox，用于验证旧格式兼容。

新增 3 项验收测试并通过：

1. golden v1 打开后无损升级到 v2，Operation/Receipt/Outbox 保持一致，v2 索引只出现一次，升级后可继续写入；
2. 升级前 SQLite online backup 保持完整 v1 rollback anchor；源库升级后，备份仍可独立恢复并迁移到 v2；
3. fault VFS 逐写入点中断迁移，磁盘上只允许完整 v1 或完整 v2，不暴露混合 schema；最终成功点保留全部 golden 数据。

全 workspace 测试、rustfmt、Clippy `-D warnings` 与 `git diff --check` 通过。

## B-STORE-FAULT F6 100K Operation metadata 增量证据（2026-08-02）

新增默认 ignored 的显式规模探针，物化 100,000 条 terminal Operation 和 100,000 条未 ACK Outbox（数据库约 28.95 MB），随后通过正常 WAL/FULL authority 执行恢复打开、pending 分页、512 次逐条 durable ACK 和再次恢复。

当前 Apple Silicon/macOS dev profile 实测：fixture 批量构造 491.01 ms、authority 打开 0.34 ms、读取前 512 条 pending 0.56 ms、512 次 durable ACK 26.56 ms、ACK 后再次打开 0.30 ms；重开后下一条 sequence 精确为 513。探针命令：

```sh
cargo test -p nlos-store --test store_scale -- --ignored --nocapture
```

fixture 构造在单个可重建测试事务中使用 `synchronous=OFF`，只用于快速生成 100K 规模状态；计入验收的恢复、pending 与 ACK 均通过正常 `SqliteOperationStore` 的 WAL/FULL 路径。这份证据证明当前数据规模下 metadata 恢复与队列热路径没有退化到不可用，但不代表 100K 次逐条生产写入吞吐，也不外推到 Windows/Linux、慢盘或完整 Task/Artifact 负载。

## B-STORE-FAULT F7 跨平台增量证据（2026-08-02）

新增 GitHub Actions `Rust cross-platform verification`，在 `ubuntu-latest`、`windows-latest`、`macos-latest` 上运行同一份 `cargo test --workspace` 与 Clippy `-D warnings`，Linux 额外执行 rustfmt。远程 run [30714584445](https://github.com/cty12356541/llmos/actions/runs/30714584445) 三个平台全部通过。

Windows 首轮测试已通过但 Clippy 暴露 Unix-only chmod 测试仍参与 Windows 编译的问题；修复为编译期平台分流，并把强制子进程终止准确表述为 Unix `SIGKILL` / Windows `TerminateProcess` 后，第二轮全绿。fault VFS、Operation authority、crash recovery、Outbox、migration 和默认规模回归均在三平台执行；Unix chmod 只在 Linux/macOS 执行，真实 RAM-volume ENOSPC 仍只在 macOS 执行。

## 当前不能证明

- 当前 fault-injection VFS、WAL 文件破坏和 kill-9 测试已覆盖单机进程崩溃、commit 中断、写损坏与模拟掉电，但不能替代真实硬件掉电、控制器缓存或不同文件系统上的 torn-sector 验证；
- disk-full、只读文件系统、I/O error 与 fail-closed 行为已在当前 macOS 环境覆盖，尚未跨平台复验；
- WAL checkpoint、长读事务、备份/恢复与 100K Operation metadata 恢复/pending/ACK 已覆盖；尚未测量 100K 次逐条生产写入和完整 Task/Artifact 负载；
- schema v1→v2 前向迁移、升级前备份、失败恢复与 golden database 已覆盖；更复杂的破坏性迁移和发布级 rollback 编排尚未验证；
- Windows/Linux/macOS CI 已复验核心 authority/fault/recovery/migration；真实 Windows 只读 ACL、Linux loop-device ENOSPC、真实掉电和更多文件系统仍未覆盖；
- ~~Outbox 尚未连接 Tokio Fiber wake/reconciliation consumer~~ **已由 [PoC-0004](./poc-0004-outbox-wake-consumer.md) 补齐（2026-08-01）**：consumer 经专用 OS 线程 pump 驱动，崩溃重放、幂等去重、stale generation fencing 与 backpressure 均有集成测试（`PARTIAL PASS`，单节点局部证据）；
- 尚无 Driver authentication、Capability、Reservation 或 EffectPermit；
- Receipt 仍只有 nominal ID，没有 durable Receipt body、签名和 provenance；
- 当前只证明单进程 SQLite authority，不证明跨节点 consensus 或分布式 exactly-once。

因此证据等级仍为单节点原型的局部 H3，不能声称完整 durable ledger、外部副作用 exactly-once 或阶段 B 已退出。
