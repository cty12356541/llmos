# B-RUNTIME-U11：终态 O(k) Wait Purge、O(1) 唤醒索引与 100K 同时唤醒风暴实证

> 证据 ID：`b-runtime-u11-o1-purge-and-wake-storm-20261122`
>
> 状态：`PASS`（单机 macOS `arm64` 常规全套件、clippy/fmt、`5K` 混合回归及 `--release` `10K`/`100K` 规模与同时唤醒风暴探针全通过；待 Wave `W82-R4-IND` integrator 合流）
>
> 日期：`2026-11-22`
>
> 保障等级：`H4`（单机 macOS `--release` 真实 `WaitAuthority`/`ChannelAuthority` 持久化等待与 100K 级同时唤醒风暴实测）
>
> 对应问题：`U-11` / `DEL-CAP-WAKE-O1`（`nlos-runtime-tokio` 终态 wait 清理 `O(n²)` 退化与 `100K` 同时唤醒风暴覆盖）
>
> 基线 SHA：`05f967451feaab9f99f84e397445ec3bffe03a6c`（分支 `feat/del-cap-wake-o1`）

---

## 1. 修前根因审查

1. **终态 wait 清理呈 `O(n²)`（`src/lib.rs` / `src/wake.rs` / `src/channel_wait.rs`）**：
   - 修复前，`Inner.waits` 为裸 `Mutex<HashMap<WaitKey, WaitEntry>>`，`ChannelWaitRegistry` 仅有 `map` + `orphan_order` 队列而无按 fiber 或按 `WaitId` 的二级索引。
   - `run_fiber` 终态退出路径（原 `crates/nlos-runtime-tokio/src/lib.rs:1306-1307`）对每个终止的 fiber 调用全表 `waits.retain(|key, _| !key.for_fiber(...))` 与 `channel_waits.retain(|key, _| !key.for_fiber(...))`。当 $n$ 个 fiber 挂起并集中终结时，每次终态清理扫描整张活跃哈希表，总体复杂度退化为 $O(n^2)$。
2. **Channel wait 注册与 `deliver` 按 `WaitId` 线性扫描呈 `O(n²)`（`src/channel_wait.rs`）**：
   - 修复前，`wait_for_channel`、`arm_durable_row`、`consume_channel_buffer` 与 `TokioChannelWakeSink::deliver` 在未按 `WaitId` 建索引的 `ChannelWaitRegistry` 上反复调用 `channel_waits.iter().find(|(key, _)| key.wait_id == ...)`。
   - 在 `100K` 级全量唤醒风暴下，单次 `deliver` 携带 `100,000` 条 `WaitRecord`；若每次逐桶扫描整张 `map`（且 Rust `HashMap::remove` 不缩容桶数组），`deliver` 与 `100K` 注册阶段均出现严重 $O(n^2)$ 阻塞（历史基线 `b-runtime-002-fiber-scale.md` §3 中 `100K` 注册 `211.50s`、仅 `1,000` 唤醒 `deliver = 1.086s`）。
3. **零 fiber ID 与 orphan 占位键潜在碰撞边界（`src/channel_wait.rs`）**：
   - 修复前，`ChannelWaitKey::orphaned(wait_id)` 使用 `ExecutionFiberId::from_bytes([0; 16])` 与 `Generation::INITIAL` 作为占位标识，未带独立判别位；而 `durable_wait_scale.rs` 等探针从 `index = 0` 起构造 `ExecutionFiberId::from_bytes(id_bytes(0)) = [0; 16]`，两者结构上不可区分。

---

## 2. 最小修复实现（严格限于 5 文件独占写集）

### 2.1 `crates/nlos-runtime-tokio/src/wake.rs`

- 引入 `pub(crate) struct OperationWaitRegistry`（`mod wake` 对 crate 外私有），封装主映射 `map: HashMap<WaitKey, WaitEntry>` 与二级索引 `by_fiber: HashMap<(ExecutionFiberId, Generation), HashSet<WaitKey>>`（以 `HashSet` 防止重复键）。
- 提供兼容访问与生命周期方法：`new`、`get`、`len`、`insert`、`remove`、`clear`、`deliver_or_buffer`、`purge_fiber`（以及 `#[cfg(test)]` 下的 `is_empty`、`contains_key`）。
- 保证 `insert`、`remove`、`clear` 与 `TokioWakeSink::wake` 通过 `deliver_or_buffer` 进行的 `entry` 消费 / 丢接收端重缓冲 / 早到缓冲迁移全程同步维护 `by_fiber`，并在桶清空时立即移除 `by_fiber` 外层键。
- `purge_fiber(&mut self, fiber_id: ExecutionFiberId, generation: Generation)` 直接从 `by_fiber` 弹出该 fiber 代次的键集并在 $O(k)$ 时间内从 `map` 删除（空桶同时移除，零内存泄漏）。

### 2.2 `crates/nlos-runtime-tokio/src/channel_wait.rs`

- 为 `ChannelWaitKey` 显式增加 `orphaned: bool` 标志位：真实 fiber（含 `ExecutionFiberId([0; 16])`）注册一律为 `orphaned: false`；`ChannelWaitKey::orphaned(wait_id)` 一律为 `orphaned: true`，从结构上杜绝零号 fiber 与 orphan 占位键碰撞。
- 为 `ChannelWaitRegistry` 增配：
  - `by_fiber: HashMap<(ExecutionFiberId, Generation), HashSet<ChannelWaitKey>>`：仅索引非孤儿键；`orphaned` 占位键绝不进入 `by_fiber`，继续严格由 `orphan_order: VecDeque<WaitId>` 与 `orphan_capacity` FIFO 驱逐治理（`ORPHAN-001` / `ORPHAN-002`）。
  - `by_wait_id: HashMap<WaitId, WaitKeyBucket>`（单键 `One(ChannelWaitKey)` 零额外堆分配，多键自动升级 `Many(HashSet<ChannelWaitKey>)` 并在缩回单键时降回 `One`），将 `wait_for_channel`、`arm_durable_row`、`consume_channel_buffer` 与 `TokioChannelWakeSink::deliver` 的按 `WaitId` 查找由 $O(n)$ 降为 $O(1)$。
- 保证 `insert`、`remove`、`retain`、`clear`、`buffer_orphan` 及 orphan 消费 / rebuffer 迁移严格同步 `by_fiber`、`by_wait_id` 与 `orphan_order`。
- 实现 `purge_fiber(&mut self, fiber_id: ExecutionFiberId, generation: Generation)`：在 $O(k)$ 时间内仅移除该 fiber 代次的非孤儿键并清理 `by_fiber` / `by_wait_id`，**绝不动 `orphan_order`**。

### 2.3 `crates/nlos-runtime-tokio/src/lib.rs`

- 将 `Inner.waits` 替换为 `Mutex<OperationWaitRegistry>`。
- 在 `TokioRuntimeAdapter` 暴露只读观测面 `wait_count(&self) -> usize` 与 `channel_wait_count(&self) -> usize`（对齐 `registered_fibers()` 与 `registered_scopes()`）。
- 在 `run_fiber` 终态临界区将全表 `retain` 替换为 `waits.purge_fiber(spec.fiber_id, spec.fiber_generation)` 与 `channel_waits.purge_fiber(spec.fiber_id, spec.fiber_generation)`，严格保持原有锁序（先 `waits`，再 `channel_waits`，同一临界区内调用 `record.finish_terminal`，在释放 `waits`/`channel_waits` 前已完成双表 purge）。

### 2.4 `crates/nlos-runtime-tokio/tests/durable_wait_scale.rs`

- 为探针二进制补充 `PROBE_SERIALIZE: Mutex<()>` 串行槽与 `bounded_runtime()`（2 Tokio workers），防止 `--include-ignored` 并发执行同进程探针相互污染进程级 RSS / 线程数 / 时延读数。
- 新增 **非 `#[ignore]`** 快速回归测试 `five_thousand_mixed_waits_partial_wake_storm_and_orphan_boundary_zero_leak`：
  - `5,000` 个 fiber（含 `index = 0` 零 fiber ID）真实挂起于独立 durable Channel wait，同时挂载 `5,000` 个混合状态 Operation wait（`250` 枚已投递 re-buffered + `4,750` 枚 `Pending`）、`250` 枚额外未唤醒 Channel wait（半数 `Pending`、半数 re-buffered）及 `64` 枚超容 orphan 投递（`orphan_buffer_capacity = 32`，FIFO 丢最老 `32` 枚、保留 `32` 枚）。
  - 先同时唤醒并 join 前 `2,500` 个 fiber，验证中点精确收口：`wait_count() == 2,500`、`channel_wait_count() == 2,532`、`registered_fibers() == 2,500`、被淘汰 orphan `0` 保持 pending、存活 orphan `32..64` 全数消费 `Woken`；再唤醒剩余 `2,500` 个 fiber 并 join，验证 `wait_count() == 0`、`channel_wait_count() == 0`、`registered_fibers() == 0`、`registered_scopes() == 0`。
- 新增 **`#[ignore]` 门控 `--release` `100K` 同时唤醒风暴与全量终态清理探针** `one_hundred_thousand_simultaneous_wake_storm_and_terminal_purge`：
  - 在 2-worker Tokio runtime 上生成 `FULL_COUNT = 100,000` 个 parked durable-wait fiber，挂满 `100,000` 枚 `OperationWait`，通过一次 `notify_commits(up_to_sequence = 100_000)` + `deliver` 同时唤醒全量 `100,000` 个休眠 fiber 并全部完成、join 回收、校验全部 `OperationWait` 转为 `Cancelled`，断言 `host_threads <= THREAD_BOUND (10)` 且 `wait_count == 0`、`channel_wait_count == 0`、`registered_fibers == 0`、`registered_scopes == 0`，逐行复核 durable 行全为 `Woken`。

---

## 3. 实测验证门与原始数据

### 3.1 实测环境

- 主机 / 内核：`Darwin lipunimadeMacBook-Air.local 25.6.0 Darwin Kernel Version 25.6.0: Fri Jul 31 19:16:20 PDT 2026; root:xnu-12377.161.14~5/RELEASE_ARM64_T8142 arm64`（单机 macOS Apple Silicon）
- 编译器：`rustc 1.99.0 (b940084d7 2026-09-28)`

### 3.2 代码门与常规测试套件（最终代码后实跑）

| 命令 | 结果 | 关键指标 / 摘要 |
|---|---|---|
| `cargo fmt -p nlos-runtime-tokio -- --check` | PASS（exit 0） | 0 diff |
| `cargo clippy -p nlos-runtime-tokio --all-targets -- -D warnings` | PASS（exit 0） | 0 warnings / 0 errors |
| `cargo test -p nlos-runtime-tokio` | PASS（exit 0） | **35 targets：181 passed; 0 failed; 16 ignored**（含单元测试 `lib` 7 passed、`wake` 10 passed、`channel_wait` 11 passed、`shutdown_semantics` 4 passed、`durable_wait_scale` 1 passed / 3 ignored 等全绿） |
| Debug 模式 `durable_wait_scale` 非 ignore 回归（`five_thousand_mixed_waits_partial_wake_storm_and_orphan_boundary_zero_leak`） | PASS（exit 0） | `spawn_issue=13.8975ms register_settle=578.411292ms wake_half=43.010583ms wake_rest=49.367625ms total=828.707084ms`（`0.83s`） |
| `git diff --check` | PASS（exit 0） | 0 空白/冲突错误 |

### 3.3 `--release` `durable_wait_scale` 实测（`--include-ignored --nocapture` 与独立 `--exact` 对照）

#### A. 全二进制联合运行：`cargo test --release -p nlos-runtime-tokio --test durable_wait_scale -- --include-ignored --nocapture`（4 passed; 0 failed，总墙钟 `17.78s`）

| 用例 | 关键实测指标（原样输出） |
|---|---|
| `five_thousand_mixed_waits_partial_wake_storm_and_orphan_boundary_zero_leak` | `spawn_issue=3.942333ms`、`register_settle=323.471541ms`、`wake_half=13.506083ms`、`wake_rest=16.910958ms`、`total=447.14ms` |
| `one_hundred_thousand_durable_wait_fibers_on_two_workers` | `spawn_issue=37.012958ms`、`register_settle=7.736525167s`、`wake_notify=13.908959ms`、`wake_deliver=2.006458ms`、`wake_settle=424.625µs`、`durable_readback=80.665541ms`、`rss_before=28256KiB`、`rss_after=312208KiB`、`threads=6 -> 6`（`<= 10`） |
| `one_hundred_thousand_simultaneous_wake_storm_and_terminal_purge` | `spawn_issue=36.769291ms`、`register_settle=7.822025458s`、`op_attach=38.059084ms`、`wake_notify=293.489125ms`、`wake_deliver=179.800625ms`、`wake_settle=91.560333ms`、`join_reap=28.332708ms`、`durable_readback=78.109666ms`、`rss: before=290512KiB -> parked=386048KiB -> after_storm=414960KiB -> after_reap=410416KiB`、`threads: 5 -> 5 -> 5 -> 5`（`<= 10`） |
| `ten_thousand_durable_wait_fibers_on_two_workers` | `spawn_issue=3.379083ms`、`register_settle=688.109208ms`、`wake_notify=3.5155ms`、`wake_deliver=1.594583ms`、`wake_settle=755.958µs`、`durable_readback=5.980958ms`、`rss_before=410368KiB`、`rss_after=410432KiB`、`threads=4 -> 4`（同进程前序探针释放后的分配器页复用，独立基线见下表 B） |

#### B. 独立进程 `--exact` `--release` 基线（消除同进程前序探针分配器页驻留影响）

| 用例 | 独立墙钟 | 实测时间 / 内存 / 线程 |
|---|---|---|
| `ten_thousand_durable_wait_fibers_on_two_workers` | `0.66s` | `spawn_issue=3.228166ms`、`register_settle=626.429333ms`、`wake_notify=3.20075ms`、`wake_deliver=1.470833ms`、`wake_settle=604.875µs`、`durable_readback=5.529625ms`、`rss_before=4848KiB -> rss_after=38880KiB`（`Δ=34032KiB`）、`threads=4 -> 4` |
| `one_hundred_thousand_durable_wait_fibers_on_two_workers` | `7.68s` | `spawn_issue=34.073584ms`、`register_settle=7.427917333s`、`wake_notify=12.792875ms`、`wake_deliver=1.839958ms`、`wake_settle=476.417µs`、`durable_readback=74.373625ms`、`rss_before=4848KiB -> rss_after=309840KiB`（`Δ=304992KiB`）、`threads=4 -> 4` |
| `one_hundred_thousand_simultaneous_wake_storm_and_terminal_purge` | `8.16s` | `spawn_issue=35.054958ms`、`register_settle=7.401082542s`、`op_attach=34.510416ms`、`wake_notify=282.270208ms`、`wake_deliver=187.7195ms`、`wake_settle=85.971ms`、`join_reap=22.802583ms`、`durable_readback=76.910708ms`、`rss: before=4832KiB -> parked=374736KiB (Δ=369904KiB) -> after_storm=394800KiB -> after_reap=395376KiB`、`threads=4 -> 4 -> 4 -> 4` |

#### C. 辅助对照：`batch_cancel_scale` `--release`（`--include-ignored --nocapture`，`0.31s`）

- `10K`：`per_fiber_cancel_phase=1.954µs`（`settle=7.2535ms`、`join_reap=2.711792ms`、`threads 5 -> 5`）
- `100K`：`per_fiber_cancel_phase=1.125µs`（`settle=69.019084ms`、`join_reap=13.419167ms`、`threads 5 -> 5`）
- 比例：`100K / 10K = 0.58x`（线性门界 `<= 3.0x`）。

---

## 4. 诚实边界与未宣称事项

1. **单机 macOS 实测**：上述全套与 `--release` 探针均为单机 macOS (`arm64`) 实跑结果；Linux 与 Windows CI 尚未在本分支复跑。
2. **`1M_LOGICAL` 仍待跑**：本增量仅实证 `10K` 与 `100K` 档位，`1M_LOGICAL` 档位仍待后续波次与 CI 矩阵推进。
3. **`H8 = 0`**：本证据不声称 `H8` 级真实断电、多主机 WAN 或长期老化验证。
4. **未跨写集修改**：未修改 `.agentdash/`、`Cargo.toml`、`Cargo.lock`、`schema/**`、`nlos-wait` 或其他 crate，亦未触碰主工作区或执行 `git push`；本证据中的 `claims.yaml` / `del-base-coverage-audit.md` 等管理索引留待 wave integrator 统一同步。
