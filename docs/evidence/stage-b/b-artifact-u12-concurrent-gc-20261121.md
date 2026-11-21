# B-ARTIFACT-U12：ArtifactStore 跨进程并发孤儿 GC 竞态修复与实证

> 证据 ID：`b-artifact-u12-concurrent-gc-20261121`
>
> 状态：`PASS`（单机 macOS 真实多 OS 进程并发对抗与全量 crate 门禁通过；待 Wave `W82-R3` integrator 合流）
>
> 日期：`2026-11-21`
>
> 保障等级：`H4`（单机 macOS 真实多 OS 进程并发对抗实测）
>
> 对应问题：`U-12` / `DEL-DATA`（跨 OS 进程并发孤儿 GC 删失在途 blob 竞态）
>
> 基线 SHA：`9414000f9b19179ca0922d95bbf81e21eff137ab`（分支 `feat/w82-r3-u12-gc`，保持 SQLite `SCHEMA_VERSION = 8` 不变）

---

## 1. 修前根因与跨进程语义审查

### 1.1 `put_revision` 与 `stage_revision` 跨进程 TOCTOU 窗口

修复前，`ArtifactStore` 的两个面向 `artifacts/` 域写盘入口存在相同的跨进程写锁时序漏洞：

1. `crates/nlos-artifact/src/store.rs`（`ArtifactStore::put_revision`，原第 288–291 行）：
   先调用 `self.lock_connection()?` 获取**进程内** `Mutex<Connection>`，随即调用 `blob::commit_blob(&self.paths.artifacts, digest, request.bytes)?` 将内容寻址 blob 原子 rename 至 `artifacts/blobs/<2-hex>/<digest>`，**之后**才调用 `connection.transaction_with_behavior(TransactionBehavior::Immediate)?`。
2. `crates/nlos-artifact/src/publication.rs`（`ArtifactStore::stage_revision`，原第 37–39 行）：
   同样在持有进程内 `Mutex<Connection>` 后、开启 `TransactionBehavior::Immediate` **之前**调用 `blob::commit_blob(&self.paths().artifacts, digest, request.bytes)?`。

该互斥锁仅序列化同一 `ArtifactStore` 实例内的线程。一旦两个独立 OS 进程（或同进程内两个独立 `ArtifactStore::open` 句柄）同时运行：

```text
进程 A (put_revision / stage_revision)        进程 B (collect_orphan_blobs / tick_auto_gc)
--------------------------------------        ----------------------------------------------
1. lock_connection() [仅进程 A 本地 Mutex]
2. blob::commit_blob() -> 落盘 <digest>
   (此时 SQLite 尚无任何引用行，亦无写锁)  -> 3. lock_connection() [进程 B 本地 Mutex]
                                              4. BEGIN IMMEDIATE [成功抢占 SQLite 写锁]
                                              5. referenced_digests() 未见 <digest>
                                              6. scan_blobs() 发现 <digest>
                                              7. blob::remove_blob() 删除 <digest> 并提交 GcReceipt
8. BEGIN IMMEDIATE [等待后获得写锁]
9. 插入 revision / staged 行并提交
=> 已提交 revision / staged 永久损坏为 ArtifactError::BlobMissing / StagedBlobMissing
```

### 1.2 `gc.rs` 与 `auto_gc.rs` 跨进程收敛核查

对 `crates/nlos-artifact/src/gc.rs` 与 `crates/nlos-artifact/src/auto_gc.rs` 逐行核查并经真实多进程并发锤击实测，确认无需修改其代码：

- `collect_orphan_blobs`（`gc.rs:143–184`）从引用扫描到 `GcReceipt` 提交全程持有 `BEGIN IMMEDIATE`（配合 `open_with_vfs` 设置的 `busy_timeout(5s)`，直接获取写锁而非 `DEFERRED` 升级，避免升级死锁型 `SQLITE_BUSY`）。
- `tick_auto_gc`（`auto_gc.rs:198–347`）使用基于当前 `k = passes_completed` 的确定性派生幂等键 `auto_gc_pass_key(k)` 调用 `collect_orphan_blobs`，随后在独立 `BEGIN IMMEDIATE` 事务中执行 `WHERE singleton = 0 AND passes_completed = ?1` CAS 推进：
  - 两进程同时读取 `passes_completed = k` 时，先到达 `collect_orphan_blobs` 者返回 `Collected(receipt)`，后到达者由不可变 `artifact_gc_receipts` 重放路径返回 `Replayed(receipt)`（不再扫描或删文件）；
  - 双方进入 `advance_auto_gc_state` 后，仅 CAS 胜出者推进 `passes_completed = k + 1` 并累加 `orphans_collected_total += receipt.collected_count`，败者影响 0 行、不重复计数。
- 当竞争进程以较晚时间戳推进 `last_pass_at_ms` 后，时间戳落后的另一进程调用 `tick_auto_gc`（即使 `interval_ms = Some(0)`）正确返回 `AutoGcTickDecision::Skipped(AutoGcSkipReason::IntervalNotElapsed)`，不触发越界推进或错误计数。

---

## 2. 最小修复

严格保留原有错误返回顺序、重放分支、retention 检查及 `nlos-store-fault` VFS 故障注入语义，仅将 `BEGIN IMMEDIATE` 提前到 `blob::commit_blob` 之前：

1. `crates/nlos-artifact/src/store.rs`（`ArtifactStore::put_revision`）：
   - 调整为 `let mut connection = self.lock_connection()?;` -> `let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;` -> `blob::commit_blob(&self.paths.artifacts, digest, request.bytes)?;`。
   - 同步更新函数 docstring 与 phase 1 注释，明确进程内 `Mutex` + 跨进程 SQLite `BEGIN IMMEDIATE` 联合构成完整写入临界区。
2. `crates/nlos-artifact/src/publication.rs`（`ArtifactStore::stage_revision`）：
   - 调整为 `let mut connection = self.lock_connection()?;` -> `let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;` -> `blob::commit_blob(&self.paths().artifacts, digest, request.bytes)?;`。
   - 同步更新阶段注释。

兼容性说明：`BEGIN IMMEDIATE` 仅获取 SQLite WAL 写锁而不触发 `xWrite`；`nlos-store-fault` 的 `FailWritesAfter` / `PowerLossAfter` 仍在 SQLite 页写出/提交时触发，因此 `fault_injection.rs`（10/10）与 `gc.rs`（5/5）原有崩溃窗口语义完整保留；`blob::commit_blob` 失败时（如只读 `tmp/` 目录或已有损坏 blob 的 `DigestMismatch`）`transaction` 随 drop 自动回滚并释放写锁，不留元数据。

---

## 3. 新增跨进程对抗测试与写集清单

### 3.1 `crates/nlos-artifact/tests/u12_cross_process_gc.rs`

使用 `std::process::Command::new(std::env::current_exe().unwrap())` 配合 `--exact u12_cross_process_child_helper --ignored --nocapture` 与环境变量派生真实 OS 子进程，带 30 秒步骤超时与 RAII `ManagedChild` 清理护卫：

| 用例 | 测试函数 | 核心构造与断言 |
|---|---|---|
| 用例 1（确定性屏障对抗） | `u12_barrier_serializes_write_commit_window_against_cross_process_gc` | ① 进程 B 持 `BEGIN IMMEDIATE` 250ms 期间，进程 A 的 `put_revision` 与 `stage_revision` 均阻塞在 SQLite 锁上且磁盘 `blobs/` 与 `tmp/` 零写入（修前窗口中此时已落盘并可被删）；② 反向屏障：进程 A 持 `BEGIN IMMEDIATE` 写完 blob 未提交元数据期间，进程 B 的 `collect_orphan_blobs` 阻塞且不删 blob，待提交后扫描返回 `collected_count = 0`；③ 真实 `ArtifactStore` 写者与 GC 进程锁步齐步发枪竞速 20 轮，全过程 0 孤儿、0 `BlobMissing`、`recover` 全空、`integrity_check = ok` |
| 用例 2（多 OS 进程并发锤击矩阵） | `u12_multi_process_hammer_matrix_preserves_all_revisions_and_auto_gc_accounting` | 7 个独立 OS 子进程并发运行：2 个 `put_revision` 写进程（各 16 个 revision）、1 个 `stage_revision + publish_staged_revision` 进程（12 个已发布 + 1 个未发布滞留 stage）、1 个持续校验字节与 SHA-256 的读进程、1 个 `collect_orphan_blobs` 手动 GC 进程、2 个竞争调用 `tick_auto_gc` 的自动 GC 进程，叠加战前 8 个 + 战中 8 个原子植入真孤儿；断言：① 0 次 `BlobMissing` / SHA-256 失败；② 44 个已提交/已发布 revision 与未发布 staged blob 100% 字节完整；③ 16 个预埋孤儿 100% 清理、`recover` 全空；④ `passes_completed` 单调且 `0..passes_completed` 与 `artifact_gc_receipts` 一一对应、`failure_count = 0`、`orphans_collected_total > 0` 且与自动回执计数之和精确相等、`手动 + 自动 = 16`（无重复/漏收） |
| 用例 3（强杀崩溃残留跨进程清理） | `u12_killed_writer_crash_residue_collected_with_immutable_receipt_and_zero_false_deletes` | 子进程 A 提交 rev 2 后在 `BEGIN IMMEDIATE` 中写入未入库孤儿 blob、残留 `.tmp` 及未提交 WAL 脏帧并被 `SIGKILL` 强杀，再叠加 1 个孤儿 blob；独立 OS 子进程 B 调用 `collect_orphan_blobs` 精确回收 2 个孤儿并生成不可变 `GcReceipt`（验证 replay 一致与 `UPDATE`/`DELETE` 触发器拒绝），已提交 revision 1/2 与未发布 staged blob 100% 完好、`recover` 清理残留 `.tmp`、`integrity_check = ok` |

### 3.2 本次交付写集（严格限于任务授权范围）

- `crates/nlos-artifact/src/store.rs`
- `crates/nlos-artifact/src/publication.rs`
- `crates/nlos-artifact/tests/u12_cross_process_gc.rs`（新增）
- `docs/evidence/stage-b/b-artifact-u12-concurrent-gc-20261121.md`（本文件）

未触碰 `Cargo.toml`、`Cargo.lock`、`schema/**`、`.agentdash/`、`gc.rs`、`auto_gc.rs` 或主工作区。

---

## 4. 实测验证门与结果输出

### 4.1 实测环境

- 主机 / 内核：`Darwin lipunimadeMacBook-Air.local 25.6.0 ... arm64`（macOS Apple Silicon）
- 编译器：`rustc 1.99.0 (b940084d7 2026-09-28)`

### 4.2 全量门禁与重复并发实测记录（全部在最终编辑后实跑）

| 命令 | 结果 | 关键计数 |
|---|---|---|
| `cargo fmt -p nlos-artifact -- --check` | PASS（exit 0） | 0 diff |
| `cargo clippy -p nlos-artifact --all-targets -- -D warnings` | PASS（exit 0） | 0 warnings |
| `cargo test -p nlos-artifact` | PASS（exit 0） | **113 passed; 0 failed; 1 ignored**（`lib` 22、`artifact_sync_policy` 17、`auto_gc` 7、`fault_injection` 10、`gc` 5、`happy_path` 7、`immutable_head` 4、`net_chunked_artifact_transfer` 4、`package_signature` 7、`package_task_templates` 4、`provenance` 6、`recovery` 5、`retention` 5、`staged_publication` 7、`u12_cross_process_gc` **3 passed; 0 failed; 1 ignored**，`doctests` 0；其中 `u12_cross_process_child_helper` 为父进程派发使用的 `#[ignore]` 子进程入口） |
| `cargo test -p nlos-artifact --test u12_cross_process_gc`（连续复跑第 2、3 次，累计 5 次全通过） | PASS（exit 0） | 第 2 次 1.29s、第 3 次 1.34s，每次均 `3 passed; 0 failed; 1 ignored`，临时目录全清零 |
| `git diff --check` | PASS（exit 0） | 0 空白/冲突标记错误 |

---

## 5. 保留边界与不宣称事项

1. **三平台 CI 未在本分支复跑**：本次验证为单机 macOS (`arm64`) 本地真实多 OS 进程实测（`H4`），Windows（`FILE_FLAG_BACKUP_SEMANTICS` 目录 sync 与文件锁语义）及 Linux CI 尚未在该分支复跑。
2. **跨仓同步冲突固定集与跨 crate 边界仍归后续**：`crates/nlos-artifact/src/sync.rs:2453`（`isolate_conflicting_package_blobs` 中在 `lock_connection` 保护下写入冲突包 blob 并登记至内存 `ReplicaSyncRetentionSet`，跨进程视角仍无 `BEGIN IMMEDIATE` 持久化引用表兜底）及 `nlos-store-registry` 安装/卸载与 GC 的跨 crate 交错仍归 `DEL-DATA` / `U-12` 后续范围。
3. **`H8 = 0`**：本证据不声称 `H8` 级真实断电/多主机/长稳老化验证。
