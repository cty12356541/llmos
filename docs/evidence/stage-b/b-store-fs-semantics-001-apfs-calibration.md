# B-STORE-FS-SEMANTICS-001：层 1 文件系统语义矩阵 — fault VFS 模型在 APFS 上的校准

> 状态：**PARTIAL PASS**（单平台本地证据：macOS/APFS 页缓存级实测；真实掉电不可在层 1 复现）
>
> 日期：2026-08-29
>
> 环境：Apple Silicon arm64，macOS 26.5.2（Build 25F84，Darwin 25.5.0），数据目录文件系统 **APFS**，rustc/cargo 1.97.1，`libsqlite3-sys` 0.38.2（bundled SQLite），HEAD `4f90511`
>
> 对应：[PoC-0003](./poc-0003-sqlite-operation-authority.md)（F1–F7 fault VFS 矩阵——被校准模型的权威来源）；2026-08-29 决策「分层证据阶梯：层 1 文件系统语义矩阵优先开道」
>
> 车道约束：纯验证车道，零生产代码改动。写集 = `crates/nlos-store-fault/tests/**`（全新增）+ 本文档。`crates/nlos-store-fault/src/`、`Cargo.toml`、`docs/management/**` 均未改动；工作区中其他并行车道（nlos-clock、nlos-identity 等）的脏文件未被读取/修改/清理。

## 1. 目的

fault-injection 验证体系的地基是 `nlos-store-fault` 的写丢失模型：`PowerLossAfter`（第 N 次 `xWrite` 后，`xWrite`/`xSync`/`xTruncate` 静默丢弃但报告成功）与隐含的 torn-write 假设（tear 点 = 写调用边界）。若该模型与真实文件系统语义不一致，其上所有 F1–F7 结论失真。本车道在真实 APFS 平台上量化校准该模型，产出逐注入点的「模型假设 / 实测 / 等价或偏差」对照。

## 2. 方法学

新增测试套件 `crates/nlos-store-fault/tests/fs_semantics/`（该 crate 首个集成测试；此前 fault 测试全部位于 `nlos-store`）：

| 文件 | 职责 |
|---|---|
| `main.rs` | 10 个探针 + kill-9 子进程入口（`current_exe` + `READY` 管道标记 + `Child::kill()`，复用 `fault_crash.rs` 范式） |
| `ffi.rs` | 裸 `libsqlite3-sys` C API 安全封装（该 crate 仅依赖 `libsqlite3-sys`、写集禁改 `Cargo.toml`，故不能用 rusqlite；每个 unsafe 块附 SAFETY 论证） |
| `wal.rs` | WAL 字节级解析（32 字节文件头 + 24+page_size 帧结构、commit 帧定位、截断点换算） |
| `harness.rs` | 隔离 tempdir、子进程同步、进程全局 `FAULT_LOCK` + 出界自动 `disarm` guard |

**校准核心断言**：每个场景同时跑「真实文件系统操作」（对磁盘字节做手术：截断/损坏/恢复快照/删除 `-shm`/kill-9）与「fault VFS 注入」（`PowerLossAfter`），断言两者在同一字节位置的恢复语义等价。

规模参数：工作负载 = `CREATE TABLE` + 行 0..=6（每行一个 auto-commit 事务），`journal_mode=WAL` + `synchronous=FULL`（回读 pragma 拒绝静默回退，沿用 poc-0003 纪律）。实测 WAL 几何：page_size=4096，frame_size=4120，全量 WAL=37112 字节；setup 后边界=12392（32 头 + 1 个 journal-mode 切换产生的非 commit 帧 + 2 个 commit 帧）。

## 3. 探针与实测结果（10 探针 / 11 测试全过，~0.1 s）

| 探针 | 场景 | 实测 |
|---|---|---|
| `commit_survives_kill9_after_return_synchronous_full` | WAL/FULL，6 个 commit 返回后 SIGKILL 写进程 | db/-wal/-shm 在盘；重开 6/6 提交可见，integrity ok，journal 仍 WAL（wal_len=32992，未 checkpoint） |
| `commit_survives_kill9_after_return_synchronous_normal` | 同上但 `synchronous=NORMAL` | 与 FULL **完全相同**的可见性（页缓存存活进程死亡） |
| `uncommitted_txn_invisible_after_kill9` | `BEGIN IMMEDIATE` 内被杀 | 未提交行不可见，先前提交可见，integrity ok |
| `power_loss_between_commits_matches_real_snapshot_restore` | fault：`PowerLossAfter{0}` 后的提交 vs 真实：快照恢复字节图 | 两者重开均只回到第 1 个提交（rows=[0]），integrity ok，**等价** |
| `torn_wal_truncation_recovers_exactly_the_committed_prefix` | 真实 WAL 在 32 个字节长度截断（0、32、每 commit 帧边界 ±1、帧内 +13） | 每个截断点重开均恢复**恰好 committed prefix**（行集合 = 帧边界算术预测），integrity ok |
| `fault_write_boundary_loss_matches_real_truncation_at_same_byte_length` | `PowerLossAfter{B}`，B=0..=7，逐点与「reference WAL 截到同一字节长度」对照 | 每个模拟 tear 点**精确落在 WAL 帧边界**（12392 + n×4120），fault 行集合 == 真实截断行集合 == 模型预测，integrity ok，**逐字节对齐等价** |
| `wal_frame_corruption_hides_corrupted_tail_only` | 真实：最后一帧 / 中间帧 checksum 翻转；WAL magic 清零 | 尾帧损坏→隐藏该提交及之后全部；中间帧损坏→该提交起全部隐藏（prefix 语义）；magic 清零→整 WAL 作废、schema 为空、**从不出现部分行** |
| `shm_rebuilds_after_crash` | kill-9 后删除 `-shm` | 重开自动重建，7 行全可见，integrity ok |
| `renamed_database_visible_after_kill9` | 创建+close+`rename` 后 SIGKILL | 新名存在、旧名消失、内容完整可开（页缓存级） |
| `power_loss_during_creation_leaves_empty_file` | fault：`xOpen` 后 `PowerLossAfter{0}` 创建期掉电 vs 真实：子进程 `File::create` 后立即 SIGKILL | 两者均得「文件存在、0 字节」；重开 0 字节文件 = 合法空库（pragma/create 全部静默成功） |

## 4. 模型校准表（核心交付）

| # | 注入点 / 模型假设 | 模型内容 | APFS 实测 | 结论 |
|---|---|---|---|---|
| M1 | `xWrite` passthrough（commit 返回 ⇒ 内容在盘） | 成功返回的提交跨进程死亡可见 | kill-9 后 6/6 提交可见（FULL 与 NORMAL 同） | **等价**（页缓存层；真掉电持久性归层 2/3） |
| M2 | `PowerLossAfter` 写边界丢弃 → 重开见 committed prefix | 丢弃点后全部写消失，之前提交完好，integrity ok | B=0..=7 全部成立，行集合与模型预测逐一相等 | **等价** |
| M3 | torn-write 字节位置 = 写调用边界 | tear 点只能落在 `xWrite` 粒度处 | 实测 tear 点精确 = WAL 帧边界（12392+n×4120）；真实任意字节截断（含帧内 +1/+13）也全部恢复 prefix | **交点等价；模型域 ⊂ 真实域**（SQLite 恢复对更细粒度破坏同样稳健，模型不欠覆盖语义、只欠粒度） |
| M4 | `xSync` 静默丢弃（FULL 同步丢失） | 同步丢失不改变字节、只改变持久性承诺 | kill-9 **无法区分** FULL/NORMAL——页缓存使同步参数在 L1 不可观测 | **L1 不可证伪**，模型该面只能由层 2/3 检验（显式标注） |
| M5 | `xTruncate` 静默丢弃 | checkpoint 截断路径丢失 | 本套件工作负载未触发 truncate 路径 | **未运行**（显式标注；poc-0003 F2 曾以 WAL 半帧截断覆盖相近语义） |
| M6 | `xOpen` 不被拦截 ⇒ 创建持久、内容丢失 | 掉电中建库得「存在但 0 字节」 | 与真实 SIGKILL 孪生探针一致（0 字节、合法空库）；但真实掉电下**目录项本身**是否持久（无 dir-fsync 时）L1 不可观测 | **页缓存层等价；目录项真掉电持久性未定 → 层 2/3** |
| M7 | `-shm` 写经 mmap，绕过 `xWrite` | 模型结构性无法注入 shm 丢失 | 删除 `-shm` 后恢复无损 | **已接受偏差（模型盲区）**；真实侧稳健 |
| M8 | 模型只能整写丢弃，无位级损坏 | 尾部不完整写 = 隐藏不完整尾部 | 真实 checksum 损坏同样只隐藏损坏帧及其尾部（prefix 语义），且支持「中间帧损坏隐藏其后全部」与「magic 作废整 WAL」两类模型外破坏 | **同类结果、真实域更细——已记录偏差**（模型不产生位级损坏，层 2 可用 dm-flakey 补齐） |

### 结论

1. **模型在层 1 可验证的全部注入点上与 APFS 实测语义等价**（M1/M2/M3 逐字节对齐，M6 页缓存层对齐）——fault VFS 的 `PowerLossAfter` tear 点恰好落在 WAL 帧边界，且每个 tear 点的恢复结果与真实字节截断逐一相同；F1–F4 在此平台上以 fault VFS 得出的 durability 结论在模型域内有效。
2. **模型的损坏域是真实损坏域的真子集**（M3/M8）：真实文件系统额外存在位级损坏、中间帧损坏、整 WAL 作废等破坏形态，SQLite 恢复对这些同样保持 committed-prefix 语义，因此现有 fault 测试的通过不是侥幸，但模型覆盖需按上述清单在层 2 补齐。
3. **两个结构性边界如实标注**：`-shm` 盲区（M7，mmap 绕过 shim）与 fsync 丢弃不可由 kill-9 证伪（M4）——两者均不推翻已有证据，但层 2（Linux dm-flakey 虚拟化掉电）与层 3（硬件抽验）必须检验，**本文档不将其标记为已验证**。

## 5. 已知限制

- 单平台（macOS 26.5.2/APFS）单盘（Apple Silicon 内置盘）本地证据；Windows NTFS / Linux ext4·xfs、外置盘、FileVault 卷均未测。
- kill-9 仅证明**页缓存可见性**，等价于进程崩溃，不等价于机器掉电（沿用 `fault_crash.rs` 免责声明）。
- 测试直接驱动裸 SQLite（WAL/FULL + 简化 schema），验证的是 VFS/WAL 层语义；`SqliteOperationStore` 的 schema v3 行编码不在本车道读集/写集内，但其全部落盘路径同样经过被校准的 VFS 语义。
- 工作区当前有其他并行车道未提交改动（`Cargo.lock`/`Cargo.toml` 的差异均来自 nlos-clock/nlos-identity 等车道）；本车道未执行任何 git 写操作。

## 6. 未运行项（显式标注）

- `xTruncate` 注入路径的直接探针（M5）；
- 层 2：Linux CI dm-flakey 虚拟化掉电（含 FULL/NORMAL 区分、目录项持久性、位级损坏域）；
- 层 3：真实硬件掉电抽验；
- `cargo test --workspace` / `cargo clippy --workspace`（任务禁止，仅 `-p nlos-store-fault`）；
- IOERR/ENOSPC 注入（不在本车道范围，poc-0003 F3 已覆盖，含 macOS RAM volume 真实 ENOSPC）。

## 7. 复现

```sh
cargo test -p nlos-store-fault            # 11 passed, 0 failed
cargo test -p nlos-store-fault -- --nocapture   # 观察逐条 CALIBRATION 结论行
cargo clippy -p nlos-store-fault --all-targets -- -D warnings
cargo fmt -p nlos-store-fault
```

上述命令于 2026-08-29 在 HEAD `4f90511` 全部通过；测试套件对仓库零写入（全部数据库文件建于每测试独立 tempdir，进程退出即清理）。

## 8. 层 2 接入：虚拟化掉电 CI — workflow 交付（2026-09-02，**未实跑**）

> 本节兑现 §6 登记的「层 2：Linux CI dm-flakey 虚拟化掉电」后续项的**交付半步**：workflow 与可行性分析已落地；实跑数据（FULL/NORMAL 区分、目录项持久性、位级损坏域——M4/M6/M8 的层 2 检验）要等首次 `workflow_dispatch` 手动触发后才有，**本节不将层 2 标记为已验证**。HEAD `498161b`。

### 8.1 交付物与写集

| 文件 | 性质 |
|---|---|
| `.github/workflows/power-loss-simulation.yml`（新增） | GitHub Actions workflow：`workflow_dispatch`（含 `drop_window_offsets`/`force_fallback` 输入）+ 夜间 `schedule`（`30 21 * * *`，与 rust-cross-platform 的 `0 19` 错峰）；不进 push/PR 常规链——掉电模拟慢且 drop_writes 属「说谎的磁盘」，可能假阳性 |
| 本文档 §8 | 设计、可行性分析、未验证声明 |

### 8.2 设计

- **代表性权威**：channel（`nlos-channel/tests/channel_fault_injection.rs`）、wait（`nlos-wait/tests/wait_fault_injection.rs`）、task（`nlos-task/tests/fault_injection.rs`）三者 + `nlos-store-fault/tests/fs_semantics`（层 1 校准套件的层 2 复验入口）。
- **DB 重定向通道（零测试代码改动）**：四个套件的全部落盘目录均取 `std::env::temp_dir()`（2026-09-02 对源码审计确认：channel L196/L234、wait L299/L333、task L56、fs_semantics harness L60；子进程经环境继承），Linux 下受 `TMPDIR` 控制 → workflow 跑测试时 `export TMPDIR=<dm 挂载点>` 即把全部 SQLite 数据库放到虚拟化掉电设备上。
- **主路径 dm-flakey**（内核目标类型 `flakey`，模块 `dm_flakey`）：sparse 1 GiB 文件 → `losetup` → dm `linear` 直通映射 → `mkfs.ext4` → 挂载。两相：
  1. *baseline 相*（阻断式）：linear 直通上跑全部套件，验证 harness 在 dm/ext4 路径成立；
  2. *掉电相*（`continue-on-error`，结果按观察值分诊）：`dmsetup suspend --noflush / load / resume` 活换表为 `flakey <loop> 0 1 60 1 drop_writes`（1s up / 60s down，写**静默丢弃**、读直通），由后台编排器按 `drop_window_offsets` 时间线把 4s 掉电窗口扎进套件运行，随后切回 linear。选 `drop_writes` 而非 dm-error/EIO：静默丢写 ≈ 掉电语义；EIO（SQLite IOERR 路径）已由 fault VFS F3 覆盖。
- **最小掉电探针**（workflow 内联 python3 + sqlite3 标准库，不进仓库写集）：写进程 WAL + `synchronous=FULL` 循环提交序号；*crash 模式*（严格断言）在时序矩阵偏移点 SIGKILL 后重开，断言 `integrity_check` ok 且行集为 **committed prefix（0..k 连续无 gap）**；*power 模式*（观察）把 SIGKILL 与 drop_writes 窗口同步包住——prefix 丢失/损坏 =「媒体说谎击穿 FULL 承诺」的预期观察值，因任何软件层（含 SQLite）都不可防御媒体说谎，归层 3 硬件抽验裁决。
- **降级路径**：`modprobe dm_flakey` / losetup / mkfs / passwordless sudo 任一探测失败 → 自动落 loop ext4 + SIGKILL 时序矩阵（crash 模式严格断言，阻断式）。仍有增量价值：Linux ext4 是 §5 登记的**未测平台**，crash 探针产出该平台首批 WAL committed-prefix 证据。可行性结论强制写入 job summary。
- **取证与清理**：卸载后 `e2fsck -fn` 对环设备做只读文件系统级一致性取证（退出码进 summary）；`always` 步骤按序 umount → `dmsetup remove -f` → `losetup -d`，失败留诊断并使 job 失败（runner 资源泄漏是真信号）。全量日志上传 artifact。
- **预期失败语义约定**（写入 workflow 注释与 summary）：baseline 失败与 crash 模式失败 = 阻断信号（harness 失效 / 进程崩溃语义破坏，需调查）；power 模式与掉电相套件失败 = 观察值（需人工分诊，不构成产品回归判定）。

### 8.3 可行性分析（dm-flakey 预期可用性）

GitHub hosted `ubuntu-latest` runner 具备 root（passwordless sudo，官方文档声明）。逐项预期：

| 假设 | 预期 | 依据 | 落空后果 |
|---|---|---|---|
| passwordless sudo | 可用 | GH 官方 runner 文档 | 降级不可行 → workflow 失败（如实报告） |
| `dm_flakey` 模块 | **大概率可用** | Ubuntu generic 内核把 DM 目标以模块形式随 `linux-modules-extra` 发布，hosted runner 镜像含该包；`dmsetup targets` 探测确认 | 自动降级，不阻塞 |
| loop device | 可用 | `/dev/loop-control` + loop 模块在 runner 镜像长期可用（容器镜像构建依赖） | 探测失败 → 如实报告 |
| e2fsprogs 预装 | 可用 | ubuntu runner 镜像标配 | 探测失败 → 如实报告 |

 dm-flakey 语义与被校准模型的对接：`drop_writes` 窗口内 `xWrite`/`xSync` 的下层介质静默丢弃但报告成功——正是 M1/M4 注入点假设的**真实介质版**；M3（tear 点 = 写调用边界）之外的更细粒度破坏（M8 偏差）由窗口切割点不受调用边界约束补齐；M6 的目录项持久性与 FULL/NORMAL 区分（M4）需实跑数据才能下结论，**本节不作任何断言**。

### 8.4 未验证项（显式声明）

- **workflow 整体未实跑**：yaml 仅通过语法级自查（`yaml.safe_load`）；actionlint（本机未安装则跳过）。dm-flakey 命令序列（`suspend --noflush`/`load`/`resume` 活换、flakey 表特征参数 `1 drop_writes`）依据内核 device-mapper 文档与 xfstests 用法编写，**未在任何 Linux 环境验证**。
- 上述 8.3 全部假设未在真实 runner 上证实。
- 层 2 检验目标（M4 FULL/NORMAL 区分、M6 目录项持久性、M8 位级损坏域、层 1 结论在 ext4 上的复验）**全部无数据**——首次 `workflow_dispatch` 实跑前，§6 第二条的「未运行」状态保持不变。
- 嵌入探针除 `python3 -m py_compile` 外，其 **crash 模式已于本机（macOS/APFS）功能实跑通过**（2026-09-02：2 轮 × 3 偏移点，6/6 trial `prefix-intact` + `integrity=ok`，exit 0——crash 模式不依赖 dm，平台无关）；**power 模式**（drop_writes 窗口编排）依赖 dm 设备，**未实跑**。flip.sh 活换序列经本地 stub `dmsetup` 端到端验证表参数正确（`flakey <loop> 0 1 60 1 drop_writes` / `linear <loop> 0`），但真实内核路径未验证。
