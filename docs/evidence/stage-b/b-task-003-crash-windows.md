# B-TASK-003：三点崩溃窗口与 effect 表组故障注入增量证据

> 状态：PARTIAL PASS（本地复验 + 三平台 CI 通过；尚待 integrator 审议）
>
> 日期：2026-08-05（三平台 CI 于同日通过：[run 30931085056](https://github.com/cty12356541/llmos/actions/runs/30931085056)）
>
> 对应：议题 31 证据门条 5（三点崩溃窗口注入）与条 6（effect 表组故障矩阵 + 四态区分）的测试层证据，覆盖 `EffectPermit`/`EffectSlot` 机制（6233890，schema v2）在 `nlos-store-fault` VFS 下的耐久行为
>
> 前置：[b-task-001-fault-injection.md](./b-task-001-fault-injection.md)（F1–F4 矩阵与两套测试范式）、[b-task-002-effect-permit-dispatch.md](./b-task-002-effect-permit-dispatch.md)（effect 平面逻辑层证据，其 §5 明确把"三点崩溃注入模拟与 VFS 接入"留给本切片）

## 1. 本切片完成的边界

B-TASK-002 已在逻辑层证明 EffectSlot 状态机与崩溃窗口登记语义，但其 effect 表组（`effect_slots` / `effect_permits` / `effect_receipts` / `permit_effect_sets`）此前**未经任何故障注入验证**（b-task-001-fault-injection §4 明确把 effect 表组列为未覆盖）。本切片把 PoC-0003 F1–F4 故障矩阵与三点崩溃窗口落到 effect 表组，复用已确立的两套范式（kill-9 子进程 + 管道 `READY` 标记同步，无 sleep；armed-VFS 注入 + `FAULT_LOCK` 进程内串行 + disarm 恢复验证 + `PRAGMA integrity_check` 独立复核）。

写集恰好两个新文件：`crates/nlos-task/tests/effect_fault_injection.rs`（11 个测试 = 3 个崩溃窗口 + 7 个矩阵行 + 1 个 kill-9 子进程 helper）与本文档。**未触碰任何 `src/**`、既有测试、`Cargo.toml` 或其他证据文件**；只驱动公开 API（`register_task`/`register_attempt`/`request_commit_permit`(planned_effects)/`request_effect_permit`/`consume_dispatch_token`/`record_effect_outcome`/`record_no_effect`/`finalize_commit`/重开）+ 裸 rusqlite 做表级断言。每条故障断言都是类型化的（精确错误变体或精确耐久状态），无"没 panic 就算过"。

**与并行主线（schema v3：quarantine/reconcile/effect-history）的关系**：复验期间主线多次处于 mid-flight 不可编译/未过 clippy 的瞬时状态（`mod reconcile` 未接线、导出缺失、`effect_reconcile.rs` 宏错误、`lib.rs` Display 超行等），按任务书以 `cargo test -p nlos-task --test effect_fault_injection` 限定范围重试；主线每次推进后恢复。本测试断言的 v2 保留语义在 v3 落地后**全部保持，未删除任何断言**；唯一适配是 disarm-continue 行第二轮竞争把声明 slot 从 `stable_action_slot` 0 改为 1——v3 的跨 attempt effect-history 去重（`[TASK-EFFECT-ID-001]`）会以 `EffectAlreadyClosed` 拒绝重发已 `EFFECT_CLOSED` 的 `LogicalEffectId`，这是"真正新业务 effect 必须来自显式不同 slot"的强化语义，不是语义移除。**无 counter-evidence。**

## 2. 三点崩溃窗口（议题 31 条 5）

环境：Apple Silicon / arm64，macOS，workspace toolchain（rustc 1.97.x），rusqlite 0.40 bundled SQLite。

| 窗口 | 注入点 | 重启后不变量 | 结果 |
|---|---|---|---|
| 窗口1：token 已签发、未消费（slot `PERMITTED`） | 子进程完成 注册+attempt+permit(声明 1 个 required=false slot)+effect 签发后被 SIGKILL | slot 仍 `PERMITTED`（state_seq=1、token digest 在库、无 receipt）；签发重放返回同一 `EffectPermitId` 与同一 token（token 可证明未消费）；`PERMITTED` 上登记 outcome 类型化拒绝（`InvalidEffectSlotState{Permitted}`）、伪造 token `DispatchTokenMismatch` 且状态不动——不得冒充已执行；`PERMITTED` 阻塞 finalize（`OutstandingEffectSlots{1}`）；出示未消费 token 走 `NO_EFFECT` 收口合法；全 slot 终态后 permit 正常 `COMMITTED`（head 推进、`CLOSED`） | PASS（`crash_window1_unconsumed_token_closes_no_effect_and_permit_commits`） |
| 窗口2：token 已消费（slot `DISPATCHED`）、外部调用进行中 | 子进程完成 签发+token 消费后被 SIGKILL | slot 保持 `DISPATCHED` 未闭合（state_seq=2、`effect_receipt_id=None`、receipt 表 0 行）：不得静默视为成功也不得静默视为失败；finalize 被 `OutstandingEffectSlots{1}` 阻塞；已消费 token 拒绝 `NO_EFFECT` 改名（`InvalidEffectSlotState{Dispatched}`）；调用方不确定时登记 `EFFECT_UNKNOWN` 成功且**跨第二次重开持久**，继续阻塞关闭，permit 保持 `ISSUED`（不冒充失败）；同 digest 重放返回原 receipt、改写 `EFFECT_CLOSED` 拒绝（reconcile 属主线） | PASS（`crash_window2_dispatched_unclosed_blocks_finalize_and_unknown_stays_durable`） |
| 窗口3：外部调用已成功、effect receipt 写入前 | 与窗口2同一子进程场景（两者耐久形态相同：slot `DISPATCHED` 未闭合；差别只在调用方重启后掌握的信息，属父进程断言侧） | 重开后同样 `DISPATCHED` 未闭合、finalize 阻塞；持有真实结果的调用方登记 `EFFECT_CLOSED`（receipt `prior_slot_state=Dispatched`、proof digest 逐位一致）后 permit `COMMITTED`（required slot 以 `EFFECT_CLOSED` 收口，head=1、attempt `COMMITTED`、`satisfied_required_effect_count=1`） | PASS（`crash_window3_dispatched_then_effect_closed_commits_after_restart`） |

每个窗口测试末尾均以独立连接跑 `PRAGMA integrity_check = ok`。

## 3. 故障矩阵（议题 31 条 6，对齐 PoC-0003 F1–F4 与 b-task-001 既有六行）

| # | 场景 | 注入点 | 预期不变量 | 结果 |
|---|---|---|---|---|
| 1 | kill-9 中断 effect 表写事务 | 子进程在 `BEGIN IMMEDIATE` 未提交（已弄脏 `effect_slots.slot_state` 与 `permit_effect_sets.revision`）时被 SIGKILL | 重开后中断事务完全回滚：slot 回到已提交 `PLANNED`/state_seq=0、summary revision=0；`effect_permits`/`effect_receipts` 0 行；已提交前缀（task+attempt+permit+2 个声明 slot）完整；authority 正常重开 | PASS（`fault_kill9_mid_effect_transaction_leaves_no_half_state`） |
| 2 | commit 后崩溃 | 子进程在 effect 全生命周期（签发 → dispatch → 必填槽 `EFFECT_CLOSED` + 可选槽 `NO_EFFECT` → finalize）全部提交返回后被 SIGKILL | 重开后 slot/permit/token/receipt/summary 全部逐位保留（slot0 state_seq=3、slot1 state_seq=1、receipt kind/prior/digest 逐位一致、head=1）；commit permit / effect 签发 / finalize 重放返回原结果（签发重放同一 token）；已关闭 permit 对迟到 outcome/no-effect 登记以类型化 `PermitNotIssued` 拒绝 | PASS（`fault_kill9_after_effect_commit_preserves_everything`） |
| 3 | 写入硬 I/O 错误 | `FailWritesAfter { 0, IoErr }` 分别拦截（a）携带声明 effect 集的 permit CAS 与（b）effect 签发 CAS 的首次 xWrite | 均以 `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件），不返回假成功；无半截状态（无 permit/slot/summary/effect-permit 行、slot 保持 `PLANNED`、`control_epoch` 不动）；disarm 后同一请求成功 | PASS（`fault_io_error_on_effect_writes_fails_closed_without_half_state`） |
| 4 | disk-full（ENOSPC） | `FailWritesAfter { 0, Full }` 分别拦截（a）dispatch token 消费 CAS 与（b）effect outcome 写事务（receipt+slot CAS+roots 重算同事务） | 均以 `SQLITE_FULL` 显式失败（错误链含 full）；无半截状态（slot 分别保持 `PERMITTED`/`DISPATCHED`、token 未消费、无 receipt 行）；disarm 后同一操作成功 | PASS（`fault_enospc_on_dispatch_write_fails_closed` + `fault_enospc_on_outcome_write_fails_closed`） |
| 5 | 静默丢写/短写 | (a) `PowerLossAfter { 0 }`：effect 签发 CAS"报告成功"但写入从未落盘；(b) kill-9 后文件级 WAL 撕裂：截断到最后一个 commit 帧（dispatch 事务）的一半并删 `-shm` | (a) 杀连接重开后幻影 `EffectPermit` 不得冒充已提交事实：slot 回到 `PLANNED`、`EffectPermitNotFound`、`control_epoch` 不前进；同一请求可重做且确定性派生的 `EffectPermitId`/token 逐位相同、重开后真实持久；(b) 撕裂尾部整体隐藏（slot 回到 `PERMITTED`、签发前缀完整），幻影 `DISPATCHED` 不可见（`PERMITTED` 上登记 outcome 类型化拒绝），同一 token 重放后干净再消费；两者 `integrity_check = ok` | PASS（`fault_silent_write_loss_and_torn_tail_hide_phantom_effect_facts`） |
| 6 | 故障解除后从已提交前缀继续 | `FailWritesAfter { 0, Full }` 注入一次失败的 outcome 写后 disarm，**同一 authority 实例**继续读写 | 已提交前缀与故障前逐位一致（slot `DISPATCHED`、summary roots 相等、`control_epoch` 不动）；随后 `EFFECT_CLOSED`+`NO_EFFECT` 收口、finalize A、新竞争（第二张 permit 绑定 head=1，`permit_epoch=2`）、签发/消费/闭合/finalize B 全部成功；完整重开后 head=2、双 slot 均 `EFFECT_CLOSED`、receipt 3 行 | PASS（`fault_after_disarm_effect_flow_continues_from_committed_prefix`） |

诚实性说明（与 b-task-001 相同边界）：shim 只拦截 `xWrite`/`xSync`/`xTruncate`，纯读路径无法直接注入失败；第 3/4 行对读路径断言的是 fail-closed 契约——写入失败期间读不 panic、且不返回与故障前已提交状态不一致的数据。

## 4. 四态区分（议题 31 条 6 附加）

本切片在窗口测试中对四态做如下断言与如实映射：

- **NO_EFFECT（窗口1收口）**：`PLANNED`/`PERMITTED` + 可证明未消费 token → `NO_EFFECT`，合法终态，`blocks_finalization() == false`，permit 可 `COMMITTED`。
- **COMMITTED（EFFECT_CLOSED）**：`DISPATCHED` + 权威 closure digest → `EFFECT_CLOSED`，合法终态；窗口3 与矩阵行 2/6 以此收口 required slot 完成 `COMMITTED`。
- **PARTIAL**：本切片**如实记录当前模型映射**——v2 模型中 PARTIAL 不是独立 slot 终态；`DISPATCHED` 未闭合即其观测形态，其"不冒充任何终态"（阻塞 finalize、拒绝 `NO_EFFECT` 改名、无 receipt）已由窗口2断言。required 未满足且已有 effect 的提交语义（`PARTIAL_EFFECT`/`FAILED_AFTER_EFFECT` receipt outcome）属并行主线 schema v3 的 required 成功语义与 effect-history 工作，**本文不代其声明**，其证据归属主线的 reconcile/effect-history 切片。
- **UNKNOWN（EFFECT_UNKNOWN 持久）**：崩溃窗口不确定登记，durable、跨重启阻塞关闭；`blocks_finalization() == true`；permit 保持 `ISSUED` 不冒充失败。v2 语义下 UNKNOWN 为终态；v3 reconcile 流（`RECONCILING`/`CONFIRMED_NO_EFFECT`）落地后其解除路径属主线证据范围，本测试未对 reconcile/history 表做任何故障注入。

四态的 `blocks_finalization` 映射（`DISPATCHED`/`EFFECT_UNKNOWN` 阻塞、`NO_EFFECT`/`EFFECT_CLOSED` 放行）在窗口2 测试中以公开 API 直接断言。

## 5. 复验命令与结果

```sh
cargo test -p nlos-task --test effect_fault_injection   # 11 passed; 0 failed（3 窗口 + 7 矩阵行 + 1 child helper no-op）
cargo test -p nlos-task                                 # 66 passed; 0 failed（含主线 effect_reconcile 11 + effect_history 10 与既有套件）
cargo test --workspace                                  # 49 个 test result: ok，0 failed
cargo clippy --workspace --all-targets -- -D warnings   # 通过（exit 0）
cargo fmt --all -- --check                              # 通过（exit 0）
```

说明：上述为同一工作区快照的最终复验结果。复验期间并行主线（schema v3）多次短暂使 lib 不可编译或 clippy 未过（见 §1），按任务书限定范围构建重试，主线推进后全部恢复；本切片写集文件自身始终通过 rustfmt 与 `cargo clippy -p nlos-task --test effect_fault_injection -- -D warnings`。

## 6. 当前不能证明什么（限制与非声明）

- **kill-9 ≠ 机器断电**：kill-9 模拟进程崩溃（OS page cache 存活）；"内核已接受但盘未见"的语义由矩阵行 5 的 `PowerLossAfter` 与 WAL 撕裂覆盖；真实断电下的介质行为、APFS 以外文件系统、`-shm`/mmap 损坏组合均不在证据内。
- **macOS 本地 + 三平台 CI**：Ubuntu/Windows/macOS workspace 测试与 Clippy（含本测试文件）已通过（run 30931085056）；真实 ENOSPC RAM-volume 探针未在 effect 表组重做，disk-full 以注入 `SQLITE_FULL` 为准。
- **不声称 `[TASK-EFFECT-003]` / effect-history / required 成功语义**：quarantine tombstone、`PermitAdoption`、reconcile 流、`TaskEffectHistoryEntry`/`retry_fence` 推进、`PARTIAL_EFFECT`/`FAILED_AFTER_EFFECT` 路径均属并行主线 schema v3 切片；本文未对其表组做任何故障注入，也不对其完成度背书（含 UNKNOWN 在 reconcile 落地前的终态性——本测试断言的是 v2 保留语义：UNKNOWN 阻塞关闭、DISPATCHED 未闭合阻塞 finalize、NO_EFFECT 需可证明未消费 token）。
- **不声称 F4 全集**：checkpoint/backup/长 reader 矩阵未对 effect 表组重做。
- 单 authority、单进程 SQLite；不证明跨节点 consensus 或分布式 exactly-once。digest/ID 仍为 domain-separated SHA-256 占位公式，无签名。
- 不声称 Slice K 或 `B-TASK` 包完成。

因此本增量为单节点原型的 H3 级耐久性证据，状态 PARTIAL PASS 候选，不得据此声称 `B-TASK` 包完成或真实断电/多平台耐久性已证明。
