# B-ARTIFACT-001：内容寻址 Artifact 存储（blob + SQLite 元数据 + 崩溃安全提交/恢复）

> 状态：PARTIAL PASS（候选，本地复验通过；尚待 integrator 审议）
>
> 日期：2026-08-05
>
> 对应：工作包 `B-ARTIFACT` 首个切片；技术选型 §7（[stage-b-technology-selection.md](../management/stage-b-technology-selection.md)）；v0.5 §14.1–14.2（`[ART-OWNER-001]` / `[ART-VERSION-001]` / `[ART-LOCAL-001]` / `[CTX-NOTDATA-001]`）

## 1. 本切片完成的边界

新增 crate `crates/nlos-artifact`，提供：

```text
ArtifactId + revision + ContentDigest  → SQLite metadata（WAL/FULL，沿用 nlos-store/nlos-task 权威模式）
ContentDigest                          → 本地内容寻址字节存储
```

- **目录布局**：`<root>/metadata.db`、`<root>/artifacts/blobs/<2-hex>/<digest>`、`<root>/artifacts/tmp/`、`<root>/cache/blobs/`、`<root>/cache/tmp/`。tmp 与 blobs 同 root 同设备，跨设备 rename 以 typed `CrossDeviceRename` fail-closed。
- **写入协议**（安全关键不变量，见 `src/blob.rs` 模块文档）：临时文件写入 → fsync 文件 → **重读 tmp 并按 SHA-256 校验** → atomic rename → 父目录 fsync；**blob 持久化永远先于引用该 digest 的元数据事务提交**（`BEGIN IMMEDIATE` 内插入不可变 revision 行 + CAS 推进 head）。
- **恢复/reconcile**：`recover()` 显式调用（open 不自动执行，见 §4 决策 3）：committed revision 缺 blob → typed `missing_blobs` 列表（不修复）；孤儿 tmp → 清理；孤儿 blob（无元数据引用）→ 仅列出供后续 GC，**本切片不删除**；cache 行 blob 缺失 → 降级为 miss 并删行自愈。
- **retention domain 分离**：`artifacts/`（本体）与 `cache/`（可回收派生）分目录分表；cache eviction 无任何触及 `artifacts/` 的代码路径（`[CTX-NOTDATA-001]` spirit，测试用**相同字节**同时放入两域验证 eviction 后 artifact blob 完好）。
- **immutable revision + mutable head**：revision 行由 DDL trigger 禁止 UPDATE/DELETE；head 经 CAS 前进；revision 号 = `expected_head + 1`，由权威派生（确定性、调用方无法伪造）。
- **durability gating**：与 nlos-store 同构——busy_timeout 5s、`journal_mode=WAL` 与 `synchronous=FULL` **回读校验** fail-closed（`DurabilityUnavailable`）、`user_version` v1 未知版本 fail-closed（`SchemaVersionUnsupported`）、`open_with_vfs` 支持命名 VFS 注入。

公开 API（英文 rustdoc、typed error enum、无 anyhow、库路径无 unwrap/todo）：`ArtifactStore::{open, open_with_vfs, create_artifact, put_revision, get_revision, resolve_head, put_cache_blob, get_cache_blob, evict_cache_blob, recover, inspect_artifact, inspect_revision, list_revisions}`；decision enum `CreateArtifactDecision::{Created, Existing}` / `PutRevisionDecision::{Committed, Replayed}`；typed error 含 `HeadConflict`、`RevisionConflict`、`BlobMissing`（含 reconcile 提示）、`DigestMismatch`、`BlobNoSpace`、`CrossDeviceRename`、`DurabilityUnavailable`、`SchemaVersionUnsupported`、`IdempotencyConflict` 等。

## 2. 不变量映射

| 规范条目 | 本切片的落实 | 验证 |
|---|---|---|
| 技术选型 §7 写入协议 | `blob::commit_blob` 五步协议 | fault 行 1–4 + happy path |
| 技术选型 §7 recovery/reconcile | `recover()` 双向核对 | recovery 测试全集 |
| 技术选型 §7 分目录 retention domain | `artifacts/` vs `cache/` 分目录分表 | `cache_eviction_never_touches_artifact_blobs` |
| `[ART-VERSION-001]` spirit（revision 不可变、绑定 digest） | 不可变 revision 行（trigger 强制）+ 派生 revision 号 + 读后 digest 重验 | `immutable_head.rs` 全集 |
| `[ART-LOCAL-001]` | 本地存储为权威；仅 `LOCAL_SINGLE_NODE` | 范围声明（§5） |
| `[CTX-NOTDATA-001]` spirit | cache eviction 不触 artifact blob | 同字节双域测试 |
| `[ART-OWNER-001]`（Package 与用户数据分离） | `application_id`/`owner` 仅为占位字段，语义属后续切片 | 范围声明（§5） |

## 3. 测试矩阵与结果

环境：Apple Silicon / arm64，macOS，rustc 1.97.x workspace toolchain，rusqlite 0.40 bundled SQLite。共 26 个测试（3 lib 单测 + 23 集成），全部通过。

| 组 | 场景 | 结果 |
|---|---|---|
| happy path | create/put/resolve/get 全链路（读后 digest 重验、reopen 持久化、WAL/FULL/user_version 回读、未知 schema 版本 fail-closed、create 幂等 replay/conflict、未知 artifact/revision typed 错误） | PASS（5） |
| immutable revision | 同 revision 同字节 → `Replayed`（head 不前进）；同 revision 不同字节 → typed conflict fail-closed 且原 revision 逐字节不变；DDL trigger 拒绝直接 UPDATE/DELETE revision 行 | PASS |
| head CAS | 双 store 竞争同一 head=0：恰好一者 `Committed`，败者 typed `HeadConflict{expected:0,current:1}`；未来（gap）期望同样 `HeadConflict`；败者重新 resolve 后可合法推进 head=2 | PASS |
| 崩溃窗口：rename 前 | kill-9 子进程遗留 tmp → recover 清理（`removed_tmp_files=1`） | PASS |
| 崩溃窗口：rename 后、metadata commit 前 | (a) kill-9 子进程遗留已 rename blob 无元数据 → 重开无幻影 revision、recover 列孤儿不删除；(b) VFS `FailWritesAfter{0, IoErr}` 使元数据事务失败 → typed Sqlite 错误、blob 持久但 head/revision 不可见、recover 列孤儿、disarm 后重放成功 | PASS |
| 崩溃窗口：metadata commit 后 | kill-9 子进程 → 重开后 revision+blob 完全可用，recover 零发现 | PASS |
| kill-9 元数据事务中断 | 子进程 `BEGIN IMMEDIATE` 弄脏 `artifacts` 行未提交被强杀 → 完全回滚（`created_at_ms` 回到已提交值）、`integrity_check=ok` | PASS |
| ENOSPC | 元数据期 `FailWritesAfter{0, Full}` → typed 错误（错误链含 full）、无半截元数据、disarm 后成功；blob 期 ENOSPC 由 OS 错误码分类单测覆盖（`BlobNoSpace`） | PASS（含限制，见 §5） |
| 静默丢写（断电模型） | `PowerLossAfter{0}`：put “成功”但元数据未落盘 → 杀连接重开后幻影 revision 不可见（head 无、表无行）、blob 孤儿可识别；同一请求可重做且重开后真实持久 | PASS |
| 撕裂 blob | 截断 blob 文件 → 读后 digest 校验 typed `DigestMismatch`，绝不静默返回错字节 | PASS |
| recovery reconcile | 手删 blob → recover 列 `missing_blobs` + get typed `BlobMissing`（含 digest 提示）；孤儿 tmp 清理；孤儿 blob 仅列出；recover 幂等 | PASS |
| cache 分离 | 同字节双域 eviction 不触 artifact；cache blob 丢失降级为 miss 且 recover 删行自愈；孤儿 cache blob 仅列出 | PASS |
| blob 写入失败排序 | tmp 目录只读 → typed I/O 错误、无元数据提交、tmp 无残留（Unix 门控） | PASS |

## 4. 规范解释决策（spec-interpretation decisions）

1. **`HeadConflict` vs `RevisionConflict` 的判定顺序**。任务书同时要求“两个竞争 put 败者 `HeadConflict`”与“同 revision id 不同 digest → `RevisionConflict`”；在 revision 号 = `expected_head+1` 的派生模型下，这两种描述对同一请求形状不可区分。本实现的事务内判定顺序为：(a) 目标槽位存在且 digest 相同 → `Replayed`（幂等重放，不看 head）；(b) head ≠ expected → `HeadConflict`（竞争败者/未来 gap 均落此）；(c) head 匹配但槽位被不同 digest 占用 → `RevisionConflict`（fail-closed 守卫，公开 API 正常路径不可达，防御元数据不一致）。后果：任务书 head-CAS 测试逐字通过；“同 revision 不同字节”得到 typed `HeadConflict` fail-closed（与任务书点名的 `RevisionConflict` 不同名，但语义同为 fail-closed typed conflict），在此显式登记偏差。
2. **“rename 后、metadata commit 前”窗口的构造**。库内无中途暂停钩子；该窗口用两种独立方式等效构造：kill-9 子进程直接建立“blob 已 rename、无元数据”的文件系统终态（public API + fs 级操作），以及 VFS 注入使元数据事务在 blob 持久化之后失败。两者断言相同不变量。
3. **`recover()` 显式而非 open 自动**。open 延迟可预测、恢复报告是运维决策；调用方可在 open 后立即调用。已在 crate 级 rustdoc 记录。
4. **create 幂等比较字段**。同 key replay 比较语义字段（artifact_id、content_type、application_id、owner），忽略 `created_at_ms` 并返回已存记录（崩溃重试可能带新时间戳）。同 artifact_id 不同 key → `IdempotencyConflict`（artifact 身份由上游权威签发，冲突复用拒绝）。
5. **digest 算法**。SHA-256 作为阶段 B 占位；`ContentDigest` 以不透明 32 字节存储，算法敏捷性留待后续切片。
6. **cache eviction 的别名处理**。两个 cache key 可指向同一 digest；eviction 仅在无其他行引用该 digest 时删除 blob 文件，否则只删行（孤儿 blob 留给 recover 列出）。

## 5. 当前不能证明什么（限制与非声明）

- **仅 `LOCAL_SINGLE_NODE`**：无 sync/分布式/对象存储后端；blob 层已隔离在内部 `blob` 模块以便后续抽 trait，但本切片未提供该 trait。
- **无 GC 执行**：孤儿 blob 仅列出，永不删除；无 retention policy 执行。
- **无加密、provenance 链、legal hold、删除语义**；`owner`/`application_id` 为占位字段；无 Package 签名验证（属 B-ARTIFACT 后续切片）；无 Context builder（仅原始 cache 域分离）；无 nlos-task 集成（TaskCommitReceipt 绑定属后续切片）。
- **真实整盘 ENOSPC 未测**：macOS 无可写 `/dev/full`，blob 写期 ENOSPC 以 OS 错误码分类单测（真实 `raw_os_error` 映射）+ tmp 只读集成测试近似；元数据期 ENOSPC 以 `SQLITE_FULL` 注入覆盖。RAM-volume 真实满盘探针留待后续。
- **kill-9 = 进程崩溃**（OS page cache 存活），非机器断电；断电语义由 `PowerLossAfter` 行覆盖。VFS shim 只拦截 SQLite I/O，blob 写入为普通文件系统 I/O，不在 shim 覆盖内。
- **Windows 目录 fsync**：std 无法 fsync 目录句柄；`#[cfg(not(unix))]` 下目录项持久性依赖文件系统（NTFS 元数据日志），已在 `blob.rs` 记录为平台限制；本证据仅 macOS 本地复验，CI/其他平台未在本切片声明。
- **recover 只核 presence 不核 digest**：全量 blob 重哈希留给 GC/审计切片；撕裂 blob 由读后重验覆盖。
- 单 writer（进程内 Mutex + `BEGIN IMMEDIATE` 存储栅栏）；不声称跨节点一致性。

因此本切片为单节点原机的 H3 级耐久性证据，状态 **PARTIAL PASS 候选**，不得据此声称 `B-ARTIFACT` 包完成或上述非声明项已证明。

## 6. 复验命令与结果

```sh
cargo test -p nlos-artifact
# lib 3 passed；fault_injection 9 passed；happy_path 5 passed；
# immutable_head 4 passed；recovery 5 passed；共 26 passed, 0 failed

cargo clippy -p nlos-artifact -p nlos-types --all-targets -- -D warnings
# 通过（exit 0）

cargo fmt -p nlos-artifact -p nlos-types -- --check
# 通过（exit 0）

cargo test --workspace
# 42 个 test binary ok；唯一失败为并行 lane 的 nlos-task --test task_group
# （其未提交 mid-flight 文件，不在本切片写集，见下）
```

并行 lane 说明：复验时点 `crates/nlos-task/**`（TaskGroup schema v4 lane）存在未提交改动，其 `task_group` 测试 1 项失败、`cargo fmt --all -- --check` 仅对其 `task_group.rs` 报 diff、`cargo clippy --workspace` 的错误亦全部位于该 lane 文件。本切片写集（`crates/nlos-artifact/**`、根 `Cargo.toml`、`Cargo.lock`、`crates/nlos-types/src/lib.rs` 一行 `nominal_id!(ArtifactId)`）自身全部绿色；按任务书约定未触碰该 crate。

## 7. 写集

- `crates/nlos-artifact/**`（新增 crate：`Cargo.toml` + `src/{lib,model,blob,store,query,cache,recover,schema}.rs` + `tests/{happy_path,immutable_head,recovery,fault_injection}.rs` + `tests/support/mod.rs`）
- 根 `Cargo.toml`（members 增加一行）、`Cargo.lock`（cargo 自动更新）
- `crates/nlos-types/src/lib.rs`：**最小附加**一行 `nominal_id!(ArtifactId);`，严格沿用既有宏（按任务书授权，已在此报告）
- 本文档（新增）
