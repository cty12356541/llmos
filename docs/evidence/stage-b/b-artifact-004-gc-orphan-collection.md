# B-ARTIFACT-004：显式保守 GC——孤儿 blob 清理与不可变 GC 回执（最小前缀）

> 状态：`PASS`（本地复验通过；候选，尚待 integrator 审议）
>
> 日期：2026-08-30
>
> 对应：B-ARTIFACT 进度单未决项「GC 执行」最小前缀；B-ARTIFACT-001 §5「无 GC 执行」限制的显式收窄；实现 `crates/nlos-artifact` schema v5
>
> 基线：HEAD `4a1cb2a`

## 1. 本切片完成的边界

在 B-ARTIFACT-001/002 的内容寻址 store 之上，落地 GC 执行的最小前缀：**只删除可证明孤儿**（无任何 revision/metadata 引用）的 blob，经显式入口执行，durable、幂等、重启可 replay。主题与 001（创建/恢复协议）、002（staging/publication）、003（Package 签名）相互独立，故新建本证据文件。

### 1.1 公开 API（`src/gc.rs`，全部英文 rustdoc + typed error）

- `ArtifactStore::collect_orphan_blobs(CollectOrphanBlobsRequest) -> CollectOrphanBlobsDecision::{Collected, Replayed}`：唯一删除 `artifacts/blobs/` 下 blob 的代码路径。
- `ArtifactStore::inspect_gc_receipt(ReceiptId) -> GcReceipt`；不存在时新增 typed `ArtifactError::GcReceiptNotFound`。
- `GcReceipt`：`receipt_id`（确定性派生）、`collected_digests`（升序）、`collected_count`、`scanned_blob_count`、`created_at_ms`。

### 1.2 GC 语义与保守判据

**引用集合（任何一行引用即保留）**：

1. `artifact_revisions` 全部已提交 revision 的 digest（revision 行由 DDL trigger 不可变、永不删除）；
2. `artifact_staged_revisions` **全部状态**的 digest——未发布（`stage_state=0`）的 staged blob 是 durable 权威状态、后续 publish 必需（B-ARTIFACT-002 §2.7 先例：recover 已把 staged digest 纳入引用集）；已发布（`stage_state=1`）行的 digest 虽已被其 revision 行覆盖，仍一并计入，判定刻意过包含（over-inclusive）；
3. `artifacts.head_digest`（构造上已被 revision 集覆盖， paranoia 计入，保守零成本）。

**候选集合**：`blob::scan_blobs(artifacts/blobs).present` − 引用集合，升序排序（回执确定性）。foreign 文件（非 `<2-hex>/<64-hex>` 布局）与 cache 域（`cache/blobs/`，独立 retention domain）**永不扫描、永不触碰**。

**fail-safe 方向**：不可证明为孤儿的一律保留（宁可保留不可误删）。

### 1.3 原子性与崩溃窗口（删除与回执的固定顺序）

一次运行全程持有进程内 writer mutex + 一个 `BEGIN IMMEDIATE` 事务（引用扫描→回执提交），提交中的引用集合不可能在其下变化（与其它全部写 API 相同的单写者纪律）。blob 文件无法加入 SQLite 事务，故顺序固定为：

1. 事务内计算孤儿差集；
2. 逐个删除 blob 文件（`blob::remove_blob` 对 shard 目录 fsync，删除 durable）；
3. 插入不可变 `artifact_gc_receipts` 回执并提交。

- **步骤 3 前崩溃**：删除已 durable 但**无回执**——该状态构造上一致（被删 digest 均为可证明孤儿，`recover` 不再列出，下一次显式运行从零重算差集，幂等收敛）。
- **步骤 3 后崩溃**：回执与删除集逐字节一致。
- 推论：**已提交回执永不失实，回执缺席不掩盖任何已提交状态**。（注：文件删除非事务性，`deleted ⇒ receipt exists` 在「fs 删除真实落盘、SQLite 回执可静默丢失」的不对称下不可实现；本切片以「状态一致 + 幂等重放」为窗口不变量，见 §3 行 3/4。）

**幂等 replay**：同 idempotency key 的回执是 durable 权威，原样返回、**绝不重跑**扫描或删除（B-ARTIFACT-003 / ADR-0010 replay 先例）；新 key 才触发新扫描。crash 后重试：差集重算（已删者不再出现），补交空/增量回执。

### 1.4 schema v5

`artifact_gc_receipts`（STRICT）：`receipt_id` PK、`idempotency_key` UNIQUE、`collected_digests` BLOB（packed 32B/digest，`% 32 = 0` CHECK）、`collected_count`（与 packing 一致性 CHECK）、`scanned_blob_count`、`created_at_ms`；UPDATE/DELETE 由 DDL trigger RAISE(ABORT) 拒绝（与前三级 receipt 同构的不可变模式）。迁移链 `migrate_v1..v5` 纯增量，未知 `user_version` 继续 fail-closed。

## 2. 写集

- `crates/nlos-artifact/src/gc.rs`（新增：GC 入口、回执、查询/解码）
- `crates/nlos-artifact/src/schema.rs`（`migrate_v5`）
- `crates/nlos-artifact/src/store.rs`（`SCHEMA_VERSION` 4→5、迁移链各 arm 追加 v5）
- `crates/nlos-artifact/src/lib.rs`（`mod gc` + 导出、`GcReceiptNotFound` 变体、crate 级 rustdoc 收窄「无 GC 执行」范围声明、新增 GC 段落）
- `crates/nlos-artifact/src/blob.rs`（`remove_blob` 文档更正：调用点=cache eviction 与显式 GC，无其它删除路径——原「artifact blob 永不删除」声明随本切片失效，必须更正）
- `crates/nlos-artifact/src/recover.rs`、`src/model.rs`（文档同步：recover 仍只列不删，删除权归 `collect_orphan_blobs`）
- `crates/nlos-artifact/tests/gc.rs`（新增，4 测试）
- `crates/nlos-artifact/tests/happy_path.rs`、`tests/staged_publication.rs`（`user_version` 断言 4→5，随 schema v5 的既有门）
- 本文档（新增）

未触碰并行车道改动（`crates/nlos-schema/**`、`crates/nlos-system-control/**`、`b-schema-015`）。禁 git 操作未违反；未使用 `--workspace`。

## 3. 测试矩阵与结果（`tests/gc.rs`）

| # | 场景 | 关键断言 | 结果 |
|---|---|---|---|
| 1 | 正常 GC（`gc_collects_provable_orphans_and_retains_every_referenced_blob`） | 2 孤儿删除；2 个 revision blob + **未发布 staged blob** + 第二 artifact revision blob 全部保留（不删活跃/在册引用的负路径）；同 digest 双域时仅删 artifacts 域拷贝、cache 拷贝与读路径完好；foreign 文件不动；`recover` 零发现；回执 readback 逐字段一致 | PASS |
| 2 | 幂等重放（`gc_replay_is_durable_authoritative_and_never_reruns`） | 同 key replay 逐字节等于原回执且**不重跑**（运行后新植入的孤儿存活）；新 key 触发新扫描；drop+reopen 后同 key 仍精确 replay；`inspect_gc_receipt` 跨重启一致 | PASS |
| 3 | 崩溃窗口 pre-commit IOERR（`gc_io_error_during_receipt_commit_leaves_consistent_state`） | `FailWritesAfter{0, IoErr}` 使回执写入 typed `Sqlite` 失败（`writes_observed>0`）；文件删除（纯 fs、不经 shim）已 durable；重开无幻影回执行、孤儿已消失且 `recover` 零 missing、活跃 revision 完好；重试重算空差集并补交回执，再 replay 精确；`integrity_check=ok` | PASS |
| 4 | 崩溃窗口 pre-commit PowerLoss（`gc_power_loss_mid_commit_phantom_receipt_invisible_after_reopen`） | `PowerLossAfter{0}` 下 GC「报告成功」但幻影回执重开不可见（typed `GcReceiptNotFound`）；杀掉 wal-index 持有连接后重开：删除存活、被引用 blob 完好、`recover` 零发现；重跑幂等补全；`integrity_check=ok` | PASS |

既有测试回归：`fault_injection`（10，含孤儿清单先例）、`happy_path`（6）、`immutable_head`（4）、`package_signature`（7）、`recovery`（5，孤儿「仅列出不删除」语义不变）、`staged_publication`（7）全部通过——孤儿列出/保留语义与 v1→v5 迁移链无回归。

## 4. 验证门（全部实际运行）

环境：Apple Silicon / arm64，macOS；rustc 1.97.1（stable，rust-toolchain.toml 钉住）；rustc 1.99.0-nightly (2026-08-01)；rusqlite 0.40 bundled。

| 命令 | 结果 |
|---|---|
| `cargo test -p nlos-artifact` | **47 passed, 0 failed**（4 lib + 43 集成：fault 10 / **gc 4** / happy 6 / immutable 4 / package 7 / recovery 5 / staged 7） |
| `cargo clippy -p nlos-artifact --all-targets -- -D warnings`（stable 1.97.1） | PASS（exit 0；期间修复 `manual_is_multiple_of` 一处） |
| `cargo +nightly-2026-08-01 clippy -p nlos-artifact --all-targets -- -D warnings` | PASS（exit 0；期间按提示改用 `as_chunks::<32>`） |
| `cargo fmt -p nlos-artifact -- --check`（stable） | PASS（format 后复检 exit 0） |
| `cargo +nightly-2026-08-01 fmt -p nlos-artifact -- --check` | PASS |

## 5. 规范解释决策（spec-interpretation decisions）

1. **回执与删除的顺序 = 先删后回执**。「删除与回执同事务」在文件系统对象上不可字面实现；若先提交回执再删文件，崩溃窗口会产生「回执声称已删而文件仍在」的失实回执（更差）。故固定为删（含 fsync）→插回执→提交，并以「已提交回执永不失实、无回执不掩盖已提交状态、重试幂等收敛」为窗口不变量（§1.3）。
2. **staged 未发布 blob 计为引用**（按已落地 staged 语义判定，非保守猜测）：B-ARTIFACT-002 中 staged 行是 durable 权威状态、publish 需读该 blob 验证；`stage_state=0` 行的 digest 必须保留。已发布 staged 行按 revision 覆盖即可，但仍计入引用集（over-inclusive），零成本换取判据简单。
3. **replay 语义 = 不重跑**：与 package verification receipt 同构——同 key 回执是 durable 权威，即使运行后出现新孤儿也不补删（测试行 2 钉死）；新孤儿属下一次显式运行。GC 无请求形状参数，故不存在 `IdempotencyConflict` 分支。
4. **并发前提显式化为单写者纪律**：`put_revision`/`stage_revision` 的 blob 第一阶段在 mutex 之外落盘；若与 GC 并发，其「已写 blob、未提交元数据」与崩溃孤儿不可区分，可能被 GC 判删后元数据提交成 `BlobMissing`。本切片不改变写入协议（如 pending-blob 登记表属后续切片），在 `collect_orphan_blobs` rustdoc 与 §6 明示该前提；真实孤儿主来源（写入方已死）恒安全。

## 6. 当前不能证明什么（限制与非声明）

- **无自动触发**：GC 仅显式调用；无 schedule、无 open 时自动 sweep、无 `recover` 联动触发。
- **无 retention/TTL 策略**：不判龄、不判冷热；任何后台驱逐/保留期策略均未实现（登记为后续项）。
- **无跨 artifact 引用追踪**：引用集只含本 store 的 SQLite 行；仅被 store 外部（其它 authority、外部系统）引用的 blob 对本 store 构成孤儿，调用方须自行保证无外部引用后方可调用 GC。
- **单写者并发前提**（§5.4）：与运行中 put/stage 的字节级竞态由调用纪律排除，未做并发 GC 正确性证明。
- **presence-based**：GC 只判存在性不做全量重哈希；损坏但在册的 blob 不在其职责内（撕裂检测仍在读路径）。
- **无 killed-child（kill-9）行**：崩溃窗口由 VFS 双注入覆盖（精简矩阵）；「GC 进程被 kill-9 中途强杀」与 pre-commit 注入行等价性未单独构造。
- **Windows/CI**：本证据仅 macOS 本地复验；`sync_dir` 的 Windows 目录 fsync 限制沿用 B-ARTIFACT-001 §5 声明；CI/其它平台未在本切片运行。
- **未运行项**：`cargo test --workspace`、`cargo clippy --workspace` 未运行（任务书禁 `--workspace`；并行车道存在未提交改动）；真实断电、三平台 CI 未测。

## 7. 复验命令汇总

```sh
cargo test -p nlos-artifact                                          # 47 passed, 0 failed
cargo clippy -p nlos-artifact --all-targets -- -D warnings           # PASS（stable 1.97.1）
cargo +nightly-2026-08-01 clippy -p nlos-artifact --all-targets -- -D warnings  # PASS
cargo fmt -p nlos-artifact -- --check                                # PASS
cargo +nightly-2026-08-01 fmt -p nlos-artifact -- --check            # PASS
```

 因此本切片为单节点原机 H3 级证据，状态 **PASS 候选**；仅声称 §1 边界内的显式保守孤儿 GC 已实现并验证，不得据此声称 retention、自动 GC、跨 authority 引用或并发 GC 已证明。

---

## 8. W28-E 增量：caller-driven 周期/阈值自动 GC tick、独立幂等键空间与健康计数器

> 日期：2026-09-20
>
> 对应：W28-E / B1-2（提前入 W27 波次，派发纪律 §6.5）；收窄本文件 §6「无自动触发」项
>
> 基线：HEAD `3d81a90`（分支 `feat/w28-e`）

### 8.1 触发设计裁定（spec-interpretation decision）

**裁定：显式 tick API（`ArtifactStore::tick_auto_gc`），不在 open 时评估、不建后台线程。** 理由：

1. 本 crate 无任何后台线程先例；自建线程等于引入 crate 自有的调度器与生命周期，超出最小诚实边界。
2. open 延迟可预测是既有纪律（`recover` 显式化的同一先例，lib.rs 明文）；open 时评估会把磁盘扫描代价藏进 open。
3. 库形态 + caller-driven：调用方自持时钟与节律（open 后、自家定时循环、安装流程前），时间戳全程 caller 供给（crate 级时间源纪律）。

触发条件（满足其一即 fire 一次 pass，`AutoOrphanGcPolicy::Enabled`）：

- **周期**：`now_ms ≥ last_pass_at_ms + interval_ms`（从未运行过的 store 立即 eligible）；
- **阈值**：周期未到时可选 `orphan_threshold`——只读计数当前孤儿候选（与核心同一 present−referenced 差集，`count_orphan_candidates`），`candidates ≥ threshold` 即 fire。计数只决定是否运行；随后的 pass 在自己的事务里全量重算，竞态计数最坏引发一次多余的保守 pass，**不可能引发误删**。

fail-closed 形状：`Disabled` 全旁路（不扫描、不运行、不耗键、不动计数器，W22-001 `AutoOrphanGc` 先例）；`Enabled` 双 `None` 永不 fire（显式 `NeverEligible`）；`Some(0)` 阈值即「扫描后恒 fire」的退化形状，`InvalidSpec` 拒绝。

### 8.2 独立幂等键空间（21/22 先例的推广）

触发 pass **不收调用方键**。第 `k` 次 pass（`k` = 持久 `passes_completed`，零基）固定使用域分离派生键 `SHA-256("llmos/artifact-auto-gc-key/v1" ‖ k_be)[0..16]`——128 位派生空间，与 `derive_receipt_id` 的 `llmos/artifact-gc-receipt/v1` 域、与一切调用方键族（含 nlos-slice-k 手动 19/20、安装 21/22 offsets）按构造不相交。pass 协议：

1. 读持久状态；eligible 则 `k = passes_completed`，派生键。索引**不预先预约**：单写者 mutex + 步骤 3 的 CAS 使推进恰一次；
2. 以该键调用**未改动的** `collect_orphan_blobs`（同键先前尝试遗留的回执原样 replay，durable-authoritative 规则）；
3. 单事务 CAS 推进 `passes_completed = k → k+1`（`orphans_collected_total += receipt.collected_count`、`last_pass_at_ms = now`）；CAS 0 行 = 并发 tick 已推进，报告回执不重复计数。

崩溃窗口：2 与 3 之间崩溃 = 回执在、状态旧——下一次 eligible tick 重派生同键 → replay → 恰一次推进（测试 5 以 raw SQL 手术重建该状态钉死）；步骤 2 内崩溃 = §1.3 既有 pre-receipt 窗口，同键幂等重试，**失败不消耗 pass 索引**。

### 8.3 健康计数器（schema v8）

`artifact_auto_gc_state`（STRICT，单行 `singleton=0`，迁移即插入零值行）：`passes_completed`（亦即下一 pass 键索引）、`orphans_collected_total`（只计回执在册的收集——未交回执尝试的删除归属完成重试的那次）、`failure_count`（失败不因后续成功重置）、`last_pass_at_ms`、`last_failure_at_ms`。**刻意不加不可变 trigger**：聚合健康计数不是 authority 记录；逐 pass 回执（`artifact_gc_receipts`）保持不可变。读回 API `inspect_auto_gc_health() -> AutoGcHealth`（typed）。失败语义：pass 失败计 `failure_count+1`、不计 pass、不重置周期窗口（下一 tick 同键重试）；触发评估错误（状态读/阈值计数）不计为 pass 失败。

### 8.4 写集（本增量）

- `crates/nlos-artifact/src/auto_gc.rs`（新增：tick、policy、decision/skip/health 类型、状态行读写、键派生）
- `crates/nlos-artifact/src/gc.rs`（抽取共享 `referenced_digests`；新增 `pub(crate) count_orphan_candidates`；模块文档「无自动触发」收窄为指向 auto_gc；**判据与删除顺序零改动**）
- `crates/nlos-artifact/src/schema.rs`（`migrate_v8`）、`src/store.rs`（SCHEMA_VERSION 7→8、迁移链各 arm 追加 v8）
- `crates/nlos-artifact/src/lib.rs`（`mod auto_gc` + 导出；crate 级 GC 段落与 honesty boundaries 增补「caller-driven tick」）
- `crates/nlos-artifact/tests/auto_gc.rs`（新增，7 测试）；`tests/happy_path.rs`、`tests/staged_publication.rs`（`user_version` 门 7→8）
- 本文档（本节）

### 8.5 测试矩阵与结果（`tests/auto_gc.rs`）

| # | 场景 | 关键断言 | 结果 |
|---|---|---|---|
| 1 | `tick_disabled_is_full_bypass` | `Disabled` → `Skipped(Disabled)`；孤儿文件在、回执表 0 行、健康全零；随后 Enabled tick 正常收集（旁路未耗键空间） | PASS |
| 2 | `tick_interval_double_fire_runs_single_pass` | 首 tick `Collected`；窗口内双发 → `Skipped(IntervalNotElapsed{last, eligible})` 且新植入孤儿存活、回执仍 1 行；边界 tick 新键新 pass（回执 2 行）；健康累计 `passes=2/orphans=3/last=1500`；重启后健康一致且窗口仍生效 | PASS |
| 3 | `tick_threshold_counts_true_orphans_and_never_fires_on_referenced_blobs` | 候选计数只含真孤儿（已提交 revision×2、未发布 staged、活跃 head 均不计）；低于阈值 `Skipped(BelowThreshold)`；达标 fire 仅删 2 孤儿，全部在册引用 blob 存活、可读、`recover` 零发现（触发路径 false-orphan 回归） | PASS |
| 4 | `tick_never_eligible_fail_closed` | 双 `None` → `Skipped(NeverEligible)`；零消耗 | PASS |
| 5 | `tick_replays_durable_receipt_and_advances_once_after_state_loss` | raw SQL 手术重建「回执在、状态丢」崩溃窗口 → 下一 tick `Replayed` 原回执、健康恰一次推进（`passes=1/orphans=1`）、回执仍 1 行；`Some(0)` 阈值 `InvalidSpec` 拒绝 | PASS |
| 6 | `tick_failure_counts_health_and_retries_same_pass_index` | digest 命名目录（扫描为候选、`remove_file` 必败，免 shim 的确定性中途失败）→ tick `Err(Io)`；健康 `failure=1/last_failure=5000/passes=0/last_pass=None`、回执 0 行；修复后重试同 pass 索引收敛 1 回执、`failure` 不遗忘、被引用 blob 存活 | PASS |
| 7 | `tick_key_space_stays_independent_of_manual_keys` | 手动键 GC 与 auto pass 各 1 回执（2 行、receipt_id 不同）；手动 replay 不受 auto pass 影响 | PASS |

既有测试回归：fault_injection 10、gc 4（含 kill-window 双行）、happy 6、immutable_head 4、package 7、provenance 6、recovery 5、retention 5、staged 7 全绿——v7→v8 迁移链与孤儿保守语义零回归。

### 8.6 验证门（全部实际运行，2026-09-20）

| 命令 | 结果 |
|---|---|
| `cargo test -p nlos-artifact` | **66 passed, 0 failed**（5 lib + 61 集成，含 auto_gc 7） |
| `cargo clippy -p nlos-artifact --all-targets -- -D warnings`（stable） | PASS |
| `cargo +nightly-2026-08-01 clippy -p nlos-artifact --all-targets -- -D warnings` | PASS |
| `cargo fmt -p nlos-artifact -- --check`（stable / nightly-2026-08-01） | PASS |

### 8.7 限制与非声明（本增量）

- **键不相交是构造性论证**（域分离 128 位）而非穷举证明；测试 7 只钉「与手动键共存不混淆」。
- 测试 5 的崩溃窗口经 raw SQL 手术重建（VFS 写计数对两事务窗口不可预测），非真实断电行；与 §3 行 3/4 同一 H3 级别声明。
- `AutoGcHealth` 单行可变，多实例并发 tick 的恰一次性由进程内 mutex + SQLite CAS 保证（单机单写者前提不变），未做跨进程并发 tick 证明。
- 后台调度、open 时评估、retention-GC、按 pass 失败退避（backoff）均明确不做，登记为后续项。
- `cargo test --workspace` 未运行（任务书禁令）；Windows/CI 未测（沿用 §6 声明）。
