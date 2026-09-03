# B-ARTIFACT-005：retention policy 最小前缀——per-artifact 时间上界、fail-closed 过期拒绝与「只标记不删」的 GC 协同

> 状态：`PASS`（本地复验通过；候选，尚待 integrator 审议）
>
> 日期：2026-09-03
>
> 对应：B-ARTIFACT 进度单未决项「retention policy」最小前缀；B-ARTIFACT-004 §6「无 retention/TTL 策略」限制的显式收窄；实现 `crates/nlos-artifact` schema v6
>
> 基线：HEAD `498161b`（GC 于 `832c58a` 落地）

## 1. 本切片完成的边界

在 B-ARTIFACT-001..004 的内容寻址 store 之上，落地 retention policy 的**最小前缀**：artifact 级时间上界（`retention_ms`）的显式设置/检查面——到期 artifact 的 poll/read **fail-closed 拒绝**（typed，与 not-found 可区分），以及新内容 admission 的对称拒绝；**物理清理不在本切片**（过期不进入任何删除路径，登记为后续专用 retention-GC）。与 001（创建/恢复协议）、002（staging/publication）、003（Package 签名）、004（孤儿 GC）主题相互独立，故新建本证据文件。

### 1.1 公开 API（`src/retention.rs`，全部英文 rustdoc + typed error）

- `ArtifactStore::set_retention(SetRetentionRequest{artifact_id, retention_ms}) -> SetRetentionDecision::{Updated, Replayed}`：per-artifact、durable、幂等。同值重放返回 `Replayed`（存储值即去重后的 durable 状态，无需额外 idempotency key）；不同值立即生效——延长可复活已过期数据、收缩可使可读数据立即过期（「当前上界」的诚实语义）。未知 artifact → typed `ArtifactNotFound`；超出 SQLite INTEGER → typed `InvalidSpec`。
- `ArtifactStore::inspect_retention(artifact_id) -> RetentionRecord{artifact_id, retention_ms: Option<u64>, expires_at_ms: Option<u64>, created_at_ms}`：策略状态读回（含派生的绝对 deadline 与锚点），**永不被过期门控**（审计面）。
- 读取/poll 面签名变更（见 §8.1）：`get_revision(artifact_id, revision, now_ms)`、`resolve_head(artifact_id, now_ms)`。
- typed error 新增 `ArtifactError::RetentionExpired { artifact_id, expires_at_ms }`：过期与 not-found 是两个可区分的 typed 状态，Display 明示「reads fail closed (no data was deleted)」。
- `ArtifactRecord` 增加 `retention_ms: Option<u64>`（`None` = 无界，未设置策略的 artifact 的默认态）。

### 1.2 语义核心论证：retention 与 GC 哲学的关系（本切片的决策点）

任务书给出的两个方向：「过期拒绝读 + 显式清理入口可选」vs「只标记」；并建议过期**不进** GC 删除集。本切片的落地与论证：

**选定语义：过期拒绝读（fail-closed）+ 物理不删（GC 引用集零改动）。** 理由分三层：

1. **`retention_ms` 是时间上界（keep-at-most），不是保留承诺（keep-at-least）**。任务书钉死「时间上界」；故过期后的数据已超出其声明预算：继续可读会静默违反该声明。fail-closed typed 拒绝使边界显式，且调用方有权知道「数据还在但已过期」（`RetentionExpired`）而非被 `ArtifactNotFound` 撒谎。这是「拒绝读」相对「只标记只查询」被选中的原因：标记若无检查面，预算声明无任何执行语义。
2. **删除方向与 GC 的保守哲学（b-artifact-004）正交且更危险**。GC 的 fail-safe 方向是「宁可保留不可误删」：只删可证明孤儿。过期 artifact 的 revision 行仍是已提交的不可变元数据（DDL trigger 禁删），其 blob 机械上**不是**孤儿——引用集无需任何 retention 感知，`collect_orphan_blobs` 代码零改动即保持正确。若让过期触发删除，则需要：(a) 打破 revision 行不可变性或引入 soft-delete 状态面（正是后续 retention-GC 切片的工作）；(b) 接受一个不可逆的、静默发生的策略动作——而拒绝读是可逆的（延长上界即复活，测试钉死此性质）。**保留字节永远安全，拒绝读可逆，删除不可逆**：最小诚实前缀取前两者，第三者显式登记。
3. **元数据面（audit plane）刻意不门控**：`inspect_*`/`list_revisions`/`recover`/package 验证只读行不读字节，过期 artifact 仍可被运维完整审计——这正是「只标记」的正确部分：retention 以列值标记状态，为后续专用 retention-GC（或运维显式决策）提供完整的可见性。把元数据也藏掉反而使清理决策不可能。

结论：本前缀 = 「过期拒绝读 + 只标记不删」的组合；`collect_orphan_blobs` 的引用集、候选集、崩溃窗口、回执语义**零改动**（gc.rs 仅范围声明文档更新）。

### 1.3 过期判定与门控放置

- **锚点**：artifact durable `created_at_ms`（既有列，无新增时间戳）。deadline = `created_at_ms.saturating_add(retention_ms)`；判定 `now_ms > deadline` → 过期（**半开区间**：`now_ms == deadline` 仍可读，严格大于才过期——边界测试钉死）。无续期、无写活动延长（无 TTL 续期策略引擎，登记 §9）。
- **时间源**：全部调用方传入（`now_ms` / request 内既有 `created_at_ms`/`published_at_ms`），不引入 `AuthorityClock` 或任何环境时钟。
- **门控放置**（`retention::ensure_readable` 单一实现，5 个调用点）：
  - `get_revision`：artifact 行加载后、任何 revision/blob 状态触及前；
  - `resolve_head`（poll 面）：artifact 行加载后；**无 revision 的过期 artifact 同样拒绝**（门在 artifact 上，不在 head 上），poll 永远无法观察到指向过期数据的活指针；
  - `put_revision`（新插入路径）：幂等 replay 分支**之后**、head CAS 之前，观察时钟 = `request.created_at_ms`；
  - `stage_revision`（新插入路径）：幂等/冲突分支之后，观察时钟 = `request.created_at_ms`；
  - `publish_staged_revision`（fresh 路径）：published-replay 分支返回之后、head CAS 之前，观察时钟 = `request.published_at_ms`。
- **replay 永不门控**：`PutRevisionDecision::Replayed` / `StageRevisionDecision::Replayed` / `PublishStagedRevisionDecision::Replayed` 返回已提交 durable 事实、不创建状态，沿用 B-ARTIFACT-003/004「replay 即 durable 权威、不重判」先例（测试钉死：过期后 re-put/re-stage/publish-replay 仍精确重放）。

### 1.4 schema v6

`ALTER TABLE artifacts ADD COLUMN retention_ms INTEGER`（可空，`NULL` = 无界）。`migrate_v6` 带 pragma_table_info 防御性幂等（migrate_v3 先例）；迁移链 `migrate_v1..v6` 纯增量，未知 `user_version` 继续 fail-closed；负值由 decode 期 `CorruptRecord` 拒绝（与其余 u64 列同构）。STRICT 表 ALTER ADD COLUMN 合法性已由迁移测试（含手搭 v1 库）覆盖。

## 2. 写集

- `crates/nlos-artifact/src/retention.rs`（新增：模块文档含 GC 关系论证、`SetRetentionRequest/Decision`、`RetentionRecord`、`set_retention`、`inspect_retention`、`ensure_readable`）
- `crates/nlos-artifact/src/schema.rs`（`migrate_v6`）
- `crates/nlos-artifact/src/store.rs`（`SCHEMA_VERSION` 5→6、迁移链各 arm 追加 v6、`put_revision` admission 门、`create_artifact` 记录构造补字段）
- `crates/nlos-artifact/src/query.rs`（`ArtifactRecord` SELECT/decode 增列、`optional_u64` 解码器、`get_revision`/`resolve_head` 签名 + 门 + rustdoc）
- `crates/nlos-artifact/src/publication.rs`（`stage_revision`/`publish_staged_revision` admission 门）
- `crates/nlos-artifact/src/model.rs`（`ArtifactRecord.retention_ms` 字段）
- `crates/nlos-artifact/src/lib.rs`（`mod retention` + 导出、`RetentionExpired` 变体 + Display、crate rustdoc 新增 retention 段落并收窄「无 retention」范围声明）
- `crates/nlos-artifact/src/gc.rs`（范围声明文档更新：retention 过期不进引用集，retention-GC 属后续）
- `crates/nlos-artifact/tests/retention.rs`（新增，5 测试）
- `crates/nlos-artifact/tests/{happy_path,immutable_head,recovery,gc,fault_injection,staged_publication}.rs`（`get_revision`/`resolve_head` 调用点追加 `READ_NOW_MS`、`user_version` 断言 5→6 两处）
- `crates/nlos-artifact/tests/support/mod.rs`（`READ_NOW_MS = u64::MAX` 常量：无界 artifact 在任意观测时刻可读）
- 本文档（新增）

未触碰并行车道改动（topic/capability/.github 等）。禁 git 操作未违反；未使用 `--workspace`；未自动删除任何过期数据。

## 3. 测试矩阵与结果（`tests/retention.rs` 新增 5 项）

| # | 场景 | 关键断言 | 结果 |
|---|---|---|---|
| 1 | 设置/幂等/变更/校验/重启（`set_retention_is_durable_idempotent_and_changeable`） | 初始无界 `None`；首设 `Updated` 且 deadline=锚点+retention 派生正确；同值 `Replayed` 且逐字段等于原记录；延长/收缩均 `Updated`；未知 artifact `ArtifactNotFound`；超 INTEGER `InvalidSpec`；drop+reopen 后策略状态持久 | PASS |
| 2 | 半开边界与 fail-closed 读（`expiry_boundary_is_half_open_and_reads_fail_closed`） | `now==deadline` 读/poll 均可读；`deadline+1` 双面 typed `RetentionExpired`（逐字段含 deadline）；过期≠not-found：`inspect_*`/`list_revisions` 元数据面可审计、blob 文件仍在盘（拒绝不删除）；无 revision 的过期 artifact poll 同样拒绝（门在 artifact 不在 head）；无界 artifact 在 `u64::MAX` 观测时刻仍可读 | PASS |
| 3 | admission 拒绝与 replay 不门控（`expired_artifact_refuses_fresh_admission_but_replays_durable_facts`） | 到期前 stage、恰在 deadline publish 成功（rev2）；deadline 后 fresh put/stage/publish 三路径 typed `RetentionExpired`（观察时钟=请求自带时间戳）；**replay 三连**：过期后精确 re-put rev1、re-stage、publication replay 全部返回 durable 事实不创建状态；head 未越过 deadline 前已提交的 rev2 | PASS |
| 4 | GC 协同（只标记不删）+ 重启 + 可逆性 + 零长上界（`gc_never_collects_expired_references_and_policy_survives_restart`） | 过期后显式 GC：`collected_digests` 为空、`scanned_blob_count=1`、blob 文件存活（过期但被引用 ≠ 孤儿）；reopen 后策略与过期行为持久；延长上界后在先前已过期的观测时刻复活可读；零长上界在锚点时刻恰可读、锚点+1ms 过期 | PASS |
| 5 | 门不掩盖元数据面契约（`retention_does_not_mask_create_contract`） | 同 artifact_id 异 key 仍 typed `IdempotencyConflict`；未到期的 retained artifact poll 正常 | PASS |

既有测试回归：fault_injection（10）、gc（4）、happy_path（6）、immutable_head（4）、package_signature（7）、recovery（5）、staged_publication（7，含手搭 v1 库经 v1→v6 迁移链与 `user_version=6` 断言）全部通过；lib 单测 4 项通过。

## 4. 验证门（全部实际运行）

环境：Apple Silicon / arm64，macOS；rustc 1.97.1（stable，rust-toolchain.toml 钉住）；rustc 1.99.0-nightly（2026-08-01）；rusqlite 0.40 bundled。

| 命令 | 结果 |
|---|---|
| `cargo test -p nlos-artifact` | **52 passed, 0 failed**（4 lib + 48 集成：fault 10 / gc 4 / happy 6 / immutable 4 / package 7 / recovery 5 / **retention 5** / staged 7；doctest 0） |
| `cargo clippy -p nlos-artifact --all-targets -- -D warnings`（stable 1.97.1） | PASS（exit 0；期间修复 lib rustdoc `SQLite` 反引号两处） |
| `cargo +nightly-2026-08-01 clippy -p nlos-artifact --all-targets -- -D warnings` | PASS（exit 0） |
| `cargo fmt -p nlos-artifact -- --check`（stable） | PASS（首次 diff 后 format，复检 exit 0） |
| `cargo +nightly-2026-08-01 fmt -p nlos-artifact -- --check` | PASS（exit 0） |

## 5. 规范解释决策（spec-interpretation decisions）

1. **读取面签名变更为显式 `now_ms` 参数**（`get_revision`/`resolve_head` 各加一个 `u64` 尾参）。任务书钉死「时间源调用方传入（ms，与既有 created_at_ms 模式一致）」；读路径的过期判定无法在无时间参量下诚实实现，而环境时钟/AuthorityClock 被明确排除。选择改签名而非平行 API（`*_at(now_ms)` 双面）：单一读取面使「读必带时间」成为编译期强制，调用方无法绕过时间维度；既有调用点机械适配（本切片内全部完成）。put/stage/publish 无需签名变更——其 request 结构本就携带调用方时间戳，admission 门以请求自身时间戳判定（语义：内容在自身时间戳处进入一个未过期的 artifact）。
2. **admission 门覆盖 put/stage/publish 三路径而非仅 stage**。任务书点名为 get/resolve/stage；但仅门 stage 会被 `put_revision` 平凡绕过，且会在过期 artifact 上制造永不可读的 revision（违背 fail-closed 精神）。publish 属 fresh 路径同理：deadline 前 stage、deadline 后 publish 会产出无人可读的 revision。故三路径对称门控；replay 分支一律不门控（§1.3）。
3. **`set_retention` 无 idempotency key**。与 create/stage/gc 的 exactly-once 事件语义不同，set 是「当前策略状态赋值」：存储值本身就是去重后的 durable 状态，同值重复设置天然幂等（`Replayed`），无需引入一次性键；策略变更（延长/收缩）是合法操作而非 `IdempotencyConflict`。
4. **过期判定半开区间**（`now_ms <= deadline` 可读）。使「deadline 时刻恰可读、deadline+1 过期」有确定唯一的判定面，边界测试逐点钉死；`saturating_add` 保证锚点+上界不溢出 panic。
5. **错误优先序**：`get_revision`/`resolve_head` 为 ArtifactNotFound → RetentionExpired →（revision/head 状态）；admission 路径为 幂等 replay → RetentionExpired → head/slot 冲突。过期 artifact 上的 stale-head 调用方得到 `RetentionExpired` 而非 `HeadConflict`——重解析（poll）同样被门，故调用方无法也未需要先解决 head 冲突。

## 6. 当前不能证明什么（限制与非声明）

- **无物理清理**：过期数据永不删除（本切片核心语义）；专用 retention-GC（独立于 `collect_orphan_blobs` 孤儿语义、带自身不可变回执与 replay）未实现，登记为后续切片。当前唯一的显式删除路径仍是可证明孤儿 GC。
- **无 TTL 续期策略引擎**：锚点固定为 `created_at_ms`，写活动不延长窗口；延长/收缩只能经显式 `set_retention`。
- **时间源调用方传入**：本 crate 不引入 AuthorityClock/环境时钟；调用方时钟正确性（单调性、真实墙钟）不在本 crate 职责内，调用方传入倒退的 `now_ms` 会让过期数据重新可读——与 crate 既有 `created_at_ms` 纪律同一信任模型。
- **per-artifact 粒度**：无 per-revision 上界；artifact 内所有 revision 共享同一 deadline。
- **元数据面不门控**（§1.2 论证的刻意选择）：`inspect_*`/`list_revisions`/`recover`/package 验证对过期 artifact 照常工作；「过期即不可读」仅指字节与 head poll。
- **无自动触发**：无 schedule、无 open 时 sweep、无 recover 联动；过期效果只在显式读/写调用时判定。
- **Windows/CI**：本证据仅 macOS 本地复验；`sync_dir` Windows 限制沿用 B-ARTIFACT-001 §5；CI/其它平台未在本切片运行。故障注入矩阵（VFS 崩溃窗口）未针对 `set_retention` 单独构造——其为单行 UPDATE + 提交，与既有元数据事务窗口同构，由 `fault_injection`/`gc` 既有行覆盖事务模型本身。
- **未运行项**：`cargo test --workspace`、`cargo clippy --workspace` 未运行（任务书禁 `--workspace`；并行车道存在未提交改动）；真实断电、三平台 CI 未测。

## 7. 复验命令汇总

```sh
cargo test -p nlos-artifact                                          # 52 passed, 0 failed
cargo clippy -p nlos-artifact --all-targets -- -D warnings           # PASS（stable 1.97.1）
cargo +nightly-2026-08-01 clippy -p nlos-artifact --all-targets -- -D warnings  # PASS
cargo fmt -p nlos-artifact -- --check                                # PASS
cargo +nightly-2026-08-01 fmt -p nlos-artifact -- --check            # PASS
```

因此本切片为单节点原机 H3 级证据，状态 **PASS 候选**；仅声称 §1 边界内的 retention 时间上界（设置/检查/拒绝语义）已实现并验证，不得据此声称 retention 物理清理、自动 GC、TTL 续期、per-revision 上界或分布式场景已证明。
