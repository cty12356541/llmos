# B-SEARCH-001：Search 最小服务（语义断言只读查询面）

> 状态：`PARTIAL_PASS`（单机 SQLite 重启级 `H3`）
>
> 日期：2026-09-21
>
> 对应：[进度单 §6.5.3](../../management/stage-b-progress.md) W33-E 车道行（X-1 后半，验收门「只读查询；零 semantic 权威写入」）；[§6.5.4 决策点 2](../../management/stage-b-progress.md#654-决策点状态2026-09-20-更新)已确认裁剪口径：Search = 只读查询面
>
> 实现：`crates/nlos-search/`（新 crate，`SearchService` 只读查询面 + `SemanticIndex` 派生内存索引，零自有 schema/零自有库文件）+ 根 `Cargo.toml` members 追加（本轮唯一写该文件的车道；`Cargo.lock` 仅为新 crate 条目的机械反映，同 W33-D 先例）
>
> 依赖前序：B-SEMANTIC-001..009（被查询的 Semantic 权威：Assertions/Judgments/Verifications/Retractions + TrustView）；[ADR-0006](../../management/adrs/0006-semantic-publication-receipt-owner.md)（Semantic 域事实单一 owner 纪律——本切片对权威库零写入是该纪律在查询面的延伸）；[ADR-0013](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md)（蜂窝式权威：读方消费 owner 事实、不复制第二事实源）

## 1. 本切片目标与边界

决策点 2 钉死的边界：Search 是 Semantic 权威之上的**只读查询面**——本切片对 Semantic 权威**零写入**（不是「经权威写入」，是根本没有写路径），也零自有持久化。每个 canonical 事实都在权威库里：断言/judgment/verification/retraction 行与 admission receipts 是 `nlos-semantic` 的表，TrustView 派生（最新 outcome 胜出等）是权威 `inspect_trust_view` 的逻辑。`nlos-search` 只提供两样东西：

1. **只读查询面**：`SQLITE_OPEN_READ_ONLY` 连接读权威 durable 行（结构谓词：scope/issuer/content digest/retraction 事实），canonical 解码复用权威公开 decoder，verification 谓词经绑定的 `Arc<SemanticAuthority>::inspect_trust_view` **逐查询活读**——派生逻辑零第二实现。
2. **可选派生内存索引**：`SemanticIndex` 纯粹由权威读构建（rebuildable、never canonical、不落盘）；陈旧方向被语义钉死：retraction 只追加不可复活（[SEM-RETRACT-004]），故陈旧索引只会**低报** retraction，绝不会伪造或否决权威事实；重建即收敛。

物理负证：只读标志由 `SQLite` 强制（经本面连接发 write 语句得到 `attempt to write a readonly database`，见 §5 单元测试），不靠自觉。schema 门：读 `user_version`，不在 `SUPPORTED_AUTHORITY_SCHEMA_VERSIONS`（当前 `&[6]`）内一律 fail-closed——权威未来迁移必须先扩该列表，本面才可读新行。

## 2. 写集清单

- `crates/nlos-search/**`（新 crate：`src/lib.rs` + `tests/search_service.rs` + `Cargo.toml`）
- 根 `Cargo.toml`（仅 members 追加 `crates/nlos-search`）+ `Cargo.lock`（仅新 crate 条目）
- `docs/evidence/stage-b/b-search-001-minimal-service.md`（本文件）+ `docs/management/evidence-index.yaml`（追加收录）

其余文件零改动；被查询的 `nlos-semantic` 权威 crate 源码零改动（回归见 §5）。

## 3. 实现事实

- **结构查询（`search_assertions`）**：谓词 AND 组合，按 `log_seq`（admission 序）稳定返回，`limit` 在全部过滤后应用。SQL 谓词（scope_kind/scope_id、issuer、content_digest、`event_retractions` EXISTS）在只读连接上求值；`assertion_mode` 谓词在 canonical 解码后求值（mode 只存在于 canonical 字节内）；每行解码后**重derive `EventId` 并与行内 event_id 比对**，不一致 `CorruptRecord` fail-closed（读路径完整性，同 nlos-notify 的 derive-verify 纪律）。
- **verification 谓词（trust-view 派生查找）**：`VerificationFilter::Status(s)` 对每个幸存候选**逐查询**调用权威自身 `inspect_trust_view`，仅保留 `verification_status == s` 者；权威 typed 失败（`EventNotFound`/`DanglingLineage` 家族）原样透传 fail-closed。`trust_view(event_id)` 是权威快照的逐字透传。
- **索引（`build_index`/`query`）**：一次全量读入 `Vec<AssertionHit>`（admission 序）；`query` 纯内存线性过滤（最小版显式不做查询引擎）；请求 verification 谓词时 typed 拒绝 `VerificationJoinUnavailable`（派生信任态只归权威 join，快照不得代言）。零 limit 在两面均 `InvalidLimit` fail-closed。
- **打开纪律（`open`）**：只读打开 `<root>/semantic-authority.db` + `busy_timeout` 5s + `user_version` 门。权威句柄须绑同一 root（face 无法观测绑定，host 接线假设，doc 注明，同 nlos-notify）。WAL 边界注明：不洁关机后 `-wal`/`-shm` 缺失时只读打开 fail-closed，先重开权威恢复 log。
- **门内负证**：`no_authority_writes_across_a_full_query_workload`——9 个 selector 活扫描 + 索引双建 + 索引查询 + 5 个 trust_view + verification join 全工作负载前后，权威库**全库逻辑 dump**（sqlite_master 对象 + 每表逐行按 rowid 序列化）逐字节相等；单测 `read_only_connection_rejects_writes` 直接证只读物理强制。

## 4. crate 面（公开 API）

```text
SearchService::open(root, Arc<SemanticAuthority>)
  search_assertions(&AssertionSelector) -> Vec<AssertionHit>   // 活读 + verification 活 join
  trust_view(event_id)            -> TrustViewSnapshot          // 权威快照逐字透传
  build_index()                   -> SemanticIndex              // 派生内存快照（可重建）
AssertionSelector { scope?, issuer?, assertion_mode?, content_digest?,
                    retraction: Any|ExcludeRetracted|OnlyRetracted,
                    verification: Any|Status(TrustViewVerificationStatus), limit }
SemanticIndex::query(&AssertionSelector) -> Vec<AssertionHit>   // 纯内存（verification 谓词 typed 拒绝）
SemanticIndex::entries() / assertion_count()
```

类型失败面 `SearchError`：`Authority(SemanticAuthorityError)` 原样透传 + `SchemaVersionUnsupported(i64)` + `InvalidLimit`（零 limit）+ `VerificationJoinUnavailable` + `Sqlite`/`CorruptRecord` 家族。`AssertionHit` 携带寻址字段（event_id/log_seq/scope/issuer/mode/content_digest+media_type/admitted_at_ms/retracted）；canonical 字节、receipts、lineage 仍在权威点读 API 之后。

## 5. 验证（TDD：测试先写，编译红→实现→绿）

```text
cargo test -p nlos-search
  → lib 单测 2 passed / 0 failed；search_service 集成 7 passed / 0 failed（合计 9）
cargo test -p nlos-semantic   # 被查询权威零回归
  → 35 passed（1+4+4+12+5+5+4；admission_fault_injection 4、declassification 4、
    semantic_authority 12、spec_canonical 5、trust_view 5、typed_events 4、lib 1）
cargo clippy -p nlos-search --all-targets --all-features -- -D warnings → 0 warning / 0 error
cargo fmt -p nlos-search --check → 通过
```

链路测试名（W33-E 验收门映射）：

| 测试 | 覆盖 |
|---|---|
| `selector_queries_match_authority_readback` | 全扫描按 log_seq 序；judgment/verification/retraction 事件不入断言面；mode/digest/scope/issuer 谓词；每 hit 经 `inspect_event`/`inspect_admission_receipt` 回读逐字段对账；limit 后置 |
| `verification_joined_search_matches_authority_trust_views` | verification join 与权威 `inspect_trust_view` 逐事件一致（最新 outcome 胜出：Fail→Pass 序列归 Pass）；`trust_view` 透传逐字相等 |
| `index_rebuild_is_deterministic_and_matches_live_scan` | 双建逐字节相等；结构 selector 下索引与活扫描结果全等（含 limit 截断） |
| `stale_index_converges_on_rebuild_and_never_overrides_the_authority` | out-of-band 权威 retraction 后：活扫描立见、陈旧索引低报、重建收敛——快照不伪造不否决权威事实 |
| `no_authority_writes_across_a_full_query_workload` | 门内负证：全查询工作负载前后权威库逻辑 dump（含 sqlite_master）逐字节相等 |
| `typed_failures_for_empty_state_and_invalid_inputs` | 空权威 typed-空结果（非错误）；零 limit 双面 `InvalidLimit`；未知事件 `Authority(EventNotFound)`；索引 verification 谓词 typed 拒绝；缺库 fail-closed |
| `schema_version_gate_fails_closed` | `user_version=9999` → `SchemaVersionUnsupported(9999)` |
| lib：`read_only_connection_rejects_writes` | 经本面连接的 write 语句被 SQLite 只读强制拒绝（`readonly` 错误） |
| lib：`open_fails_closed_without_an_authority_database` | 无权威库路径打开 fail-closed |

## 6. 明确未完成（PARTIAL_PASS 保持）

- **SPEC 事件（event_type 5）未入索引面**：最小版只索引断言（task 行「语义原子/断言」的断言半边）；SPEC/judgment/verification/retraction 仍可经权威点读 API 访问。按最小版裁剪。
- **索引为线性扫描**：`SemanticIndex` 是 admission 序 `Vec` + 线性过滤，无二级结构/查询引擎；规模假设（100K 级）如波屏障要求再benchmark。
- **快照一致性窗口未做读事务包裹**：全量读未包 `BEGIN DEFERRED` 快照（行级读各自一致）；最小版按「重建即收敛」语义覆盖，强快照需求登记为后续项。
- **刷新策略未接 outbox**：索引重建靠调用方显式 `build_index`；未订阅 semantic outbox 做增量/失效（权威侧已有 outbox 面，接线留给后续波次）。
- IPC/GUI 面、跨进程、真实掉电、CI 三平台 run 链接补登均为波屏障固定动作，不在本车道写集内。
