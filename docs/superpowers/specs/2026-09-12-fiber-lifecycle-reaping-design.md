# fiber/scopes 生命周期回收(join 即回收 + 有界墓碑)设计

- **日期**:2026-09-12
- **状态**:已获维护者批准的方向(chat 批准,本文为定稿 spec)
- **波次**:W25
- **背景依据**:深度审查第二轮对抗核查 RT-A1/A3/B2(证实项)、`b-runtime-002` §6、`cancel_late_callback_matrix` 场景 5

## 1. 目标

把 `nlos-runtime-tokio` 的两个只增不减注册表(`fibers`、`scopes`)与 orphaned `channel_waits` 缓冲从**无界增长**改为**有界**,同时保留窗口内的防重语义。修掉 `scope_for` 的 O(N²) 扫描。

**非目标**:不改 `join_fiber` 为 async(RT-A2 另立波次);不改准入信号量;不动 wake 三态协议与锁序。

## 2. 语义契约(规范语句)

### 2.1 fiber 记录

- `[FIBER-REAP-001]` `join_fiber` 成功返回终态 `FiberExit` 的**同一临界区内**,记录从 `fibers` 移除,墓碑 `(fiber_id, fiber_generation)` 入环。join 语义从"可无限重放"变为"一次性消费"。
- `[FIBER-REAP-002]` 对已回收 fiber 再次 join → `RuntimeError::FiberReaped { fiber_id, generation }`(新错误变体;契约层 `nlos-runtime` 同步定义并穷举 Display)。
- `[FIBER-REAP-003]` `detach_fiber` 语义升级:已终态 → 立即回收(同 001 临界区);未终态 → 标记 `reap_on_terminal`,终态转换临界区内回收。既有校验(存在性、generation)不变。
- `[FIBER-REAP-004]` spawn 防重:墓碑环内命中同 `(id, generation)` → 维持 `DuplicateFiber`;环容量满时 FIFO 挤出,被挤出后同 id 视为新 fiber(允许)。
- `[FIBER-REAP-005]` 进程崩溃/重启等价性:墓碑为内存态,不持久化(与现 fibers map 一致)。

### 2.2 scope 注册表

- `[SCOPE-IDX-001]` 新增 `by_id: HashMap<ScopeId, Vec<generation>>` 二级索引;`scope_for` 从持锁线性 `any()` 改为索引查询(O(1));`cancel_scope` 与 spawn 注册同步维护索引。
- `[SCOPE-IDX-002]` scope 条目引用计数 = 引用它的未回收 fiber 记录数;归零即移除条目(含索引),scope id 入 scope 墓碑环。
- `[SCOPE-IDX-003]` scope 墓碑环内同 id 再注册 → 维持现有 `InvalidGeneration` 拒绝;出环后允许以任意 generation 新建 scope。
  - 注记(实现口径,经 W25 审查裁定):现有语义即只拒异代;环内存 `(id, generation)`,窗口内同 id 异代再注册才拒(`InvalidGeneration`),被回收的同 `(id, generation)` 可立即重建为未取消的新实例;已取消 scope 的取消态不越窗口存活。
- `[SCOPE-IDX-004]` 既有"同 id 锁定首个 generation"语义在窗口内不变(现测试钉死的 `InvalidGeneration` 行为保留)。

### 2.3 orphaned channel_waits

- `[ORPHAN-001]` 早到且无人认领的 `channel_waits` 缓冲条目加容量上界(默认 1024,`TokioRuntimeConfig::orphan_buffer_capacity`);超限丢最老,单调递增丢弃计数器加入 `health()` 快照。
- `[ORPHAN-002]` 正常路径(fiber 绑定的等待)不受影响——终态 purge 行为不变。

### 2.4 配置

- `TokioRuntimeConfig` 新增:`tombstone_capacity: usize`(fiber 墓碑,默认 65_536)、`scope_tombstone_capacity: usize`(默认 65_536)、`orphan_buffer_capacity: usize`(默认 1_024)。零值语义:`0` = 容量为零的环(即无窗口保护,纯消费语义)——显式合法,非"禁用功能"歧义。

## 3. 内存账(设计目标兑现)

稳态上界:活 fiber ≤ 准入上限(10k × ~176B ≈ 1.8MB)+ fiber 墓碑 ≤ 65_536 × ~32B ≈ 2MB + scope 条目/墓碑同量级 + orphan ≤ 1_024 条。**总上界 ~10MB 量级**(配置可调),替代原先的无界。

## 4. 兼容性清单(显式)

1. `join` 从可重放到一次性消费——生产代码无多join调用方(审查证实);测试中 join 重放断言更新为 `FiberReaped`。
2. `cancel_late_callback_matrix` 场景 5:终态后同 id 同代 spawn 期许 `DuplicateFiber` → 改为"墓碑窗口内仍 `DuplicateFiber`"(默认容量下测试值远小于窗口,断言不变即可通过;另补一条环容量=1 的挤出用例)。
3. `b-runtime-002` §6 "终态占位"表述:W25-D 增量中登记语义更新(不删历史证据,追加说明)。
4. `detach_fiber` 从 no-op 到回收动作:调用方(测试)行为不变,仅多回收效果。

## 5. 测试计划

新套件 `crates/nlos-runtime-tokio/tests/lifecycle_reap.rs`:
1. join 消费:`registered_fibers()` 计数在 join 后下降;重 join → `FiberReaped`
2. detach 已终态 → 立即回收;detach 未终态 → 终态后回收
3. 墓碑窗口:同 id 同代 spawn → `DuplicateFiber`(窗口内);`tombstone_capacity = 1` 挤出后同 id 新 spawn 允许
4. scope:O(1) 索引正确性(乱序 id 注册/查询/取消);最后 fiber 回收后 scope 条目移除、窗口内同 id `InvalidGeneration`、出环后允许
5. orphan 上界:灌入 > 容量条目,断言最老被丢、`health()` 计数器递增
6. 回归:`cancel_late_callback_matrix`、`join_detach`、既有 92+ 测试全绿

## 6. 写集与文件

- `crates/nlos-runtime/src/lib.rs`:`FiberReaped` 变体 + 契约文档
- `crates/nlos-runtime-tokio/src/lib.rs`:fibers 回收、detach、墓碑、TokioRuntimeConfig
- `crates/nlos-runtime-tokio/src/channel_wait.rs`:orphan 上界 + 计数器
- `crates/nlos-runtime-tokio/tests/lifecycle_reap.rs`:新套件
- W25-D:stage-b-progress 第八十八增量

## 7. 验收门

`-p nlos-runtime -p nlos-runtime-tokio` 全绿 + workspace clippy/fmt 双 0 + 全量 `--no-fail-fast` 1104+ 通过 0 失败(新增测试计入)+ 三平台 CI 绿。
