# W67 阶段 C 退出门综合证据评审记录（ROAD-C-001 / ROAD-C-002 / C-core 九包 / 移交 #1–#17 / 全仓零冻结审计）

> - **状态**：`REVIEW`（`2026-10-26`，车道 `W67-EXIT-REVIEW` Attempt 1；面向 [stage-c-progress §C.5.5](../../../management/stage-c-progress.md#c55-阶段-c-退出评审门) 条件 1/5/6 的综合证据审查与条件 2/3/4 readiness 核对）
> - **日期**：`2026-10-26`
> - **评审基线 `HEAD`**：`9e5e0edc33cbcf89f2be9dd25f369cd81eba5237`
> - **Assurance**：`H6`（所审底层最高证据等级为 `v0.5 §30` `H6` 分布式故障注入；本评审严格遵循 `Claim <= Evidence`、`[EVID-SCOPE-001]` 与 `[EVID-NEG-001]`，将每份底层 Evidence 的拓扑/持久性/平台边界原样带入裁决，零慷慨外推）
> - **任务边界**：写集严格限定为本文件（`docs/evidence/stage-c/reviews/w67-stage-c-exit-gates.md`），不改动任何生产代码、测试代码或共享机器台账；并在基线 `HEAD` 上只读复跑了全仓零冻结静态扫描、`cargo tree -i` 反向依赖图审计，以及 `[ROAD-C-001]` / `[ROAD-C-002]` 三套核心门禁测试二进制。
> - **前序评审对照基线**：[W34-A（阶段 B 六门证据矩阵与退出门 readiness 评审）](../../stage-b/reviews/w34a-six-gate-matrix.md)、[W31-G](../../stage-b/reviews/w31g-road-b004-gates.md)、[W33-H](../../stage-b/reviews/w33h-road-b001-b002.md)、[exit-unknown-risks.md](../../../management/exit-unknown-risks.md)、[risks.yaml](../../../management/risks.yaml)、[claims.yaml](../../../management/claims.yaml)。

## 0. 阶段 C 退出门与核心交付规范原文（逐字誊录自 [06-架构设计总纲-v0.5.md](../../../design/06-架构设计总纲-v0.5.md) §28.3）

[06-架构设计总纲-v0.5.md](../../../design/06-架构设计总纲-v0.5.md) `§28.3 阶段 C：分布式现代系统`（行 `4887–4904`）规定了阶段 C 的 7 项必须交付与 2 道退出门：

| 类别 | 规范行号 | 规范原文 |
|---|---:|---|
| 必须交付 1–7 | `4891–4897` | 多节点 identity/epoch/fencing；quota lease 与 reconciliation；Artifact/event sync；placement、migration、federation；TaskAuthority 分片、层级 fanout/reducer、Capacity/Device lease 与 bulkhead；Global/Cell/Worker SchedulerDomain、work stealing、context affinity、跨 Cell pressure/backpressure；分区与恢复语义。 |
| **`[ROAD-C-001]`** | `4901` | **分区、重启、迁移和重复消息下有明确一致性、额度冻结和损失上界。** |
| **`[ROAD-C-002]`** | `4903` | **海量 Agent benchmark MUST 证明 spawn/message/commit 不依赖全局锁或全局总序，并验证 stale TaskAuthority epoch、分区双主、fanout storm、provider stall 与 Cell bulkhead。** |

---

## 1. 执行裁决总表（Executive Verdict Table）

沿承 [W34-A](../../stage-b/reviews/w34a-six-gate-matrix.md) 评审口径，仅当具名测试与落盘 Evidence 完整覆盖规范条款且显式登记适用域边界时，裁定为 `SATISFIED-WITH-BOUNDARIES`；局部验证的工作包维持 `PARTIAL_PASS`。

| 评审维度 | 证据最高等级 | 综合裁决提案 | 证据覆盖与边界核心摘要 |
|---|---|---|---|
| **`[ROAD-C-001]`**（§28.3 行 `4901`） | **`H6`** | **`SATISFIED-WITH-BOUNDARIES`** | 由 [C-ROAD-001](../c-road-001-partition-restart-migration-gate.md)（5 顶层测试 / 13 真实子 OS 进程孵化，本评审现场复跑 `5 passed; 0 failed`）联同 [C-PARTITION-001](../c-partition-001-partition-autonomy-bounded-loss.md)、[C-LEASE-001](../c-lease-001-durable-three-family-reconciliation.md)、[C-SHARD-001](../c-shard-001-cross-cell-takeover-drive.md)..[006](../c-shard-006-production-takeover-binding.md)、[C-SYNC-001](../c-sync-001-signed-event-set-merge.md)..[002](../c-sync-002-artifact-sync-policy.md)、[C-MIGRATE-001](../c-migrate-001-process-migration-protocol.md)..[002](../c-migrate-002-cell-host-process-migration.md)、[C-NOTIFY-001](../c-notify-001-task-lifecycle-action-token.md)、[C-SEARCH-001](../c-search-001-non-authoritative-checkpoint-search.md) 逐条闭合。**显式边界**：限定 [ADR-0018](../../../management/adrs/0018-single-host-multiprocess-dual-cell-topology.md) 单机多 OS 进程 + 本地持久 `SQLite WAL + FULL` 与 Layer-2 `Child::kill()` 强杀；跨物理主机网卡网络与 Layer-3 物理断电不外推（`RISK-B-11` / `U-4`）。 |
| **`[ROAD-C-002]`**（§28.3 行 `4903` + §27 `PERF-*`） | **`H6`** | **`SATISFIED-WITH-BOUNDARIES`** | 由 [C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md) 联同 [C-FANOUT-001](../c-fanout-001-hierarchical-fanout-bulkhead.md)、[C-SCHED-001](../c-sched-001-three-tier-scheduler-domain.md) 闭合：`1 -> 2 -> 4 -> 8` 独立子 OS 进程 `Cell` 分片无全局锁/无全局总序扩展（`--release` `4.44x`、`dev` `5.78x`），全权威库强制 `SQLite WAL + synchronous=FULL`（`[PERF-BENCH-001]`），8-`Cell` `10K_LOGICAL` 与 `100K_LOGICAL`（`1%/5%/10%` 工作集，`100K` 控制面 `p50=84–85 µs`），以及全部五类并发故障注入 100% 通过（本评审现场复跑两套二进制均 `4 passed; 0 failed; 1 ignored`）。**显式边界**：`1M_LOGICAL` 档位、多平台 CI release 生产容量声明（`RISK-B-07` / `U-1`）与跨物理主机网络 RTT 显式留界。 |
| **`C-core` 九大工作包**（`C-CELL`、`C-LEASE`、`C-SYNC`、`C-SHARD`、`C-FANOUT`、`C-MIGRATE`、`C-SCHED`、`C-PARTITION`、`C-BENCH`） | `H4`–`H6` | **`9/9 PARTIAL_PASS`** | `ADR-0018` 单机多 OS 进程拓扑与 `SQLite WAL + FULL` 持久层下的全部规范条款均已实现并落档 `docs/evidence/stage-c/`（共 22 份 Stage C Evidence）；跨物理主机网络传输/共识基底（`RISK-B-11` / [ADR-0020](../../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md)）与 Layer-3 硬件掉电显式设界，不虚标全域 `DONE`。 |
| **阶段 B → C 移交项 `#1–#17`**（[stage-c-progress §C.3.2](../../../management/stage-c-progress.md#c32-移交项工作包656-117)） | `H3`–`H6` | **`2 DONE`（`#1`, `#13`）+ `12 PARTIAL_PASS`（`#2`, `#4`–`#12`, `#15`, `#17`）+ `3 Explicitly Governed / Re-scoped per §C.5.4 Decision Point 3`（`#3`, `#14`, `#16`）** | `#1`（`slice-k-demo` 09d / `RISK-B-12` 关闭）与 `#13`（TS/Python SABI golden）已 `DONE`；`#2/#4..#12/#15/#17` 共 12 项已完成单机/多进程核心闭环并保留具名部署/规模残域；`#3`（GUI 真机交互战役，D 邻接常设观察项）、`#14`（根因修复 + dispatch `35560466719` 及 `2026-09-30` 夜间 `36641210416` 已绿，夜间 `schedule` 作为逐波常设门持续生效）、`#16`（Layer-3 物理掉电持续项）按 `§C.5.4` 决策点 3 显式治理归置，零遗漏悬空项。 |
| **全仓零冻结 / 零退役模块审计** | 静态扫描 + `cargo tree -i` + `H4/H6` | **`PASS (0 frozen / 0 retired across all 35 workspace crates + desktop + sample-app)`** | 全仓 35 个 workspace crate、`desktop` 与 `examples/sample-app` 源码正则扫描匹配数为 **`0`**；`2026-10-05 F1`（`8a37943`）冻结的四 crate（`nlos-cell`、`nlos-lease`、`nlos-search`、`nlos-notify`）与退役二进制（`slice-k-demo`）已全部完成真实生产消费接线与端到端验真。 |
| **未决 `P0` 风险计数**（[risks.yaml](../../../management/risks.yaml)） | 台账复核 | **`0`** | 当前 `risks.yaml` 共 12 条（`RISK-B-01..12`），`severity: P0` 条目数为 **`0`**；`RISK-B-12` 常设升级门已于 `87ef288` 凭回执级证据裁决不升 `P0` 并 `closed`。 |

---

## 2. 全仓零冻结 / 零退役审计与生产消费反向依赖图（Zero-Frozen / Zero-Retired Audit）

### 2.1 只读静态扫描与反向依赖实测结果（基线 `HEAD = 9e5e0edc`）

本评审在 `d:\llmos` 工作区直接执行只读扫描与 `cargo tree` 反向依赖核验，全部退出码为 `0`：

1. **全仓源码冻结/退役声明正则扫描**：
   ```powershell
   $m = @(Get-ChildItem -Path crates,desktop,examples -Recurse -Filter *.rs |
     Select-String -Pattern "冻结声明|退役声明|冻结状态|退役状态|冻结保留")
   Write-Output "WORKSPACE_FREEZE_OR_RETIRE_MATCH_COUNT=$($m.Count)"
   # 实测输出：WORKSPACE_FREEZE_OR_RETIRE_MATCH_COUNT=0（涵盖全部 35 个 workspace crate + desktop/src-tauri + examples/sample-app）
   ```
2. **解冻四 crate 的工作区生产反向依赖图（`cargo tree --workspace -i <crate> --all-features`）**：
   ```text
   $ cargo tree --workspace -i nlos-search --all-features
   nlos-search v0.1.0 (D:\llmos\crates\nlos-search)
   ├── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k)
   │   └── nlos-package v0.1.0 (D:\llmos\crates\nlos-package)
   └── nlos-system-control v0.1.0 (D:\llmos\crates\nlos-system-control)
       └── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k) (*)

   $ cargo tree --workspace -i nlos-notify --all-features
   nlos-notify v0.1.0 (D:\llmos\crates\nlos-notify)
   ├── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k)
   │   └── nlos-package v0.1.0 (D:\llmos\crates\nlos-package)
   └── nlos-system-control v0.1.0 (D:\llmos\crates\nlos-system-control)
       └── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k) (*)

   $ cargo tree --workspace -i nlos-cell --all-features
   nlos-cell v0.1.0 (D:\llmos\crates\nlos-cell)
   ├── nlos-lease v0.1.0 (D:\llmos\crates\nlos-lease)
   │   └── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k)
   │       └── nlos-package v0.1.0 (D:\llmos\crates\nlos-package)
   └── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k) (*)

   $ cargo tree --workspace -i nlos-lease --all-features
   nlos-lease v0.1.0 (D:\llmos\crates\nlos-lease)
   └── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k)
       └── nlos-package v0.1.0 (D:\llmos\crates\nlos-package)
   ```
   （此外桌面壳 `desktop/src-tauri` 独立工作区通过 `nlos-system-control` 消费 `OutcomeDto::SearchInspected` / `OutcomeDto::NotificationInspected` 全分支穷尽投影。）

### 2.2 从 `F1` 冻结到全量生产解冻的完整演进链

| 时间 / 波次 | 演进节点与提交锚点 | 对象与解冻条件兑现事实 | 支撑证据 |
|---|---|---|---|
| `2026-10-05 F1` | `8a37943`（收口 `de68ee3`） | 经 `cargo tree -i` 审计对当时零生产消费的 `nlos-lease`、`nlos-cell`、`nlos-notify`、`nlos-search` 写入显式**冻结保留声明**并登记各自解冻前提；对 `slice-k-demo` 写入退役声明；保留 `nlos-debug` 运维面。 | [stage-c-progress §C.8 F1 段](../../../management/stage-c-progress.md#c8-增量日志w36w37w38-合入与推送) |
| `W47 -> W48 -> W49 -> W65` | `3116129` / `366f0e6` / [ADR-0021](../../../management/adrs/0021-lease-grantor-shared-cell-authority.md) / `W65-CPARTITION` | **`nlos-cell` + `nlos-lease` 解冻闭环**：`CellHost` 全量装配本地七件套与单持有者 epoch 广播链，三族 Lease grantor（`Quota` / `Capacity` / `ExclusiveDevice`）共享 `Arc<CellAuthority>` 并挂载 `SqliteLeaseStore`（`<root>/leases.sqlite3` `WAL + FULL`），清理全部历史冻结注释头。 | [C-LEASE-001](../c-lease-001-durable-three-family-reconciliation.md)、[C-PARTITION-001](../c-partition-001-partition-autonomy-bounded-loss.md) |
| `W65` | `W65-SEARCH-EXT` / `W65-NOTIFY-EXT` | **`nlos-search` + `nlos-notify` 超最小面能力完备**：`nlos-search` 实现 `SearchEnvelope<T>`、`NON_AUTHORITATIVE` 五门禁用守卫、`SignedSemanticVectorCheckpoint` 快照查询与多 Cell 无标量 `log_seq` 联邦归并；`nlos-notify` 升级至 schema `v2`（`WAL + FULL`），实现 91B `TaskNotificationEnvelope`、一次性防跨应用伪造 `ActionToken` 与离线游标分页历史。 | [C-SEARCH-001](../c-search-001-non-authoritative-checkpoint-search.md)、[C-NOTIFY-001](../c-notify-001-task-lifecycle-action-token.md) |
| `W66` | `W66-UNFREEZE-SC` + `W66-UNFREEZE-SLICEK-ROADC001` | **`nlos-search`、`nlos-notify` 与 `slice-k-demo` 终局生产接线（清零最后 3 处声明）**：①控制面线：`SearchService`（`Send + Sync` 只读 `Mutex<Connection>`）与 `NotificationService`（主键 `O(1)` `inspect_subscription`）接入 `SystemControlDaemon`（常驻 `<root>/{waits,notify,semantic}`）、`SearchServiceInspector`、`NotificationServiceInspector`、`system-control-cli`、中英 `nl`、`ADR-0011` 认证命名管道/Unix 套接字 IPC 及 `desktop` Rust/TS 全分支视图；②运行时与纵向切片线：接入 `SliceKRuntime::open`、`CellHost::open_durable` / `open_federated_durable`、`service_faces.rs` 及 `slice-k-demo` `STEP 15 cell-host-services`（实跑输出 4 类结构化 `RECEIPT` 行并 `[slice-k] DONE`）。 | [C-UNFREEZE-001](../c-unfreeze-001-search-notify-production-wiring.md)、[C-ROAD-001](../c-road-001-partition-restart-migration-gate.md) |

---

## 3. `[ROAD-C-001]` 逐条款证据审查（分区、重启、迁移与重复消息）

> **门原文（v0.5 §28.3 行 `4901`）**：「分区、重启、迁移和重复消息下有明确一致性、额度冻结和损失上界。」

### 3.1 规范条款 × 生产实现 × 证据工件逐项核对矩阵

| 门子句与规范不变量 | 一致性 / 冻结 / 损失上界契约验证事实 | 主证据与专项证据工件 | 具名测试锚点 |
|---|---|---|---|
| **1. 分区局部自治与远端失败显式化**（`[DIST-LOCAL-001]`、`[DIST-FAIL-001]`、`[CONS-DECL-001]`） | 控制面进入 `ControlPlaneLinkPhase::Partitioned` 期间，持有有效缓存 `Capability` 与未过期本地 `QuotaLease` 的 Cell 独立完成本地扣费并产出 `LocalAutonomyProof`；跨 Cell 控制面扩容/发布等 4 类动作一律 fail-closed 拒绝；远端超时/不可达显式分类为 `RemoteCallOutcome`（禁止不确定副作用盲目自动重试）；联邦搜索在严格模式下以 `RemoteShardFailed` 拒绝，在显式降级模式下返回 `FederatedPartialEventual` 与失败分片列表。 | [C-ROAD-001](../c-road-001-partition-restart-migration-gate.md)、[C-PARTITION-001](../c-partition-001-partition-autonomy-bounded-loss.md)、[C-SEARCH-001](../c-search-001-non-authoritative-checkpoint-search.md) | `road_c001_compound_...`（`L4850–L4938`）；`dual_cell_partition_recovery`（6/6）；`partition_cut_blocks_takeover_until_channel_recovers`（[C-SHARD-004](../c-shard-004-h6-fault-injection-matrix.md)） |
| **2. 硬崩溃重启、持久重水化与 WAL 丢失防猜测**（`boot_generation`、`SQLite WAL+FULL`、`[LEASE-WAL-001]`） | 真实子 OS 进程经 `Child::kill()` + `Child::wait()` 非优雅强杀后，同目录冷重启的新 OS PID 自动递增持久化 `node_boot_generation`（`1 -> 2`），从 `<root>/leases.sqlite3`（11 张 `STRICT` 表 + 22 条触发器）精确重水化已提交租约，零静默退款；若发生本地 WAL 丢失，`refuse_guessed_remaining_on_wal_loss` 严拒从 `face_value` 猜测剩余额度，并由 `fence_boot_generation_on_wal_loss(1 -> 2)` 将旧世代全部未决租约持久转入 `QUARANTINED`。 | [C-ROAD-001](../c-road-001-partition-restart-migration-gate.md)、[C-LEASE-001](../c-lease-001-durable-three-family-reconciliation.md)、[C-SHARD-004](../c-shard-004-h6-fault-injection-matrix.md) | `road_c001_compound_...`（`L4940–L4965`）；`road_c001_quota_freeze_and_exact_bounded_loss_under_partition_and_crash`（`L5504–L5741`）；`restart_continues_from_durable_prefix_and_converges` |
| **3. 跨 Cell 权威接管与双向 8 步进程迁移一致性**（`[DIST-TASK-001/002/004]`、`[DIST-MIGRATE-001/002]`、`[LEASE-MIGRATE-001]`） | ①`TaskAuthority` 接管：旧 `term 1` 租约到期后，继任 Cell 采集 Ed25519 签名六类 canonical endpoint 屏障观测并驱动 `drive_takeover_with_federation_binding` 晋升至 `term 2`、重开 `participant_registry_generation = 2`（SQLite 触发器禁原地解冻），重复接管回放被拒，存活旧 Cell 持 `term 1` 发起 `prepare` 与 `permit` 均以 `AuthorityLeaseFenced` 拒绝；②`Process` 8 步迁移（`Initiated -> ... -> TargetRestored -> Completed`）：覆盖 `Cell A -> Cell B` 透明检查点模式与 `Cell B -> Cell A` 固定会话降级 `restart-from-artifact` 模式，三族租约严格先关/fence 源再授目标（守恒 `300 - 80 == 220`），Capability 衰减重绑并吊销源端，目标 `process_generation == 2`；③`SourceFenced` 后崩溃注入触发 `abort_post_fence -> QuarantinedPostFence`，源/目标双侧均无活跃进程绑定（**零双活进程**），源端未决三族租约持久保持 `Quarantined`。 | [C-ROAD-001](../c-road-001-partition-restart-migration-gate.md)、[C-SHARD-001](../c-shard-001-cross-cell-takeover-drive.md)..[006](../c-shard-006-production-takeover-binding.md)、[C-MIGRATE-001](../c-migrate-001-process-migration-protocol.md)..[002](../c-migrate-002-cell-host-process-migration.md) | `road_c001_compound_...`（`L5300–L5492`）；`road_c001_migration_crash_after_source_fenced_quarantines_without_dual_active_process`（`L5749–L5874`）；`dual_cell_process_migration`（7/7） |
| **4. 重复消息、跨库重连收敛与一次性 `ActionToken`**（`[DIST-EVENT-001]`、`[DIST-AUDIT-001]`、`[SEM-CHECKPOINT-001]`、`[DIST-ART-001]`、`[TASK-NOTIFY-001]`、`[DUI-NOTIFY-001]`、`[DUI-OFFLINE-001]`、`[CONS-CTRL-001]`） | ①重连语义同步：仅合并带生产者签名 + `AdmissionReceipt` + `trust_snapshot_digest` 的事件集合（零 CRDT 语义臆造、零跨 Cell 标量 `log_seq`），二次回放零重复导入，双端收敛至同值 `SignedSemanticVectorCheckpoint` 摘要；②制品同步：按四轴 MIME 策略快进同步 `ff_art_id`，对分叉修改生成确定性对称 `ArtifactConflictReceipt` 并由同步感知 GC 保护冲突头与墓碑；③通知与交互令牌：重复通知发布返回 `Replayed`（序号不膨胀），强杀重连后离线历史按游标单调分页完整恢复；一次性 `ActionToken` 对跨应用伪造、绑定不匹配、篡改凭证、过期窗口均 fail-closed 拒绝且不误烧毁有效令牌，合法首次核销 `RESP_OK` 后二次重放必报 `NOTIFY_ERR_ALREADY_REDEEMED`；④重连后联邦检查点搜索收敛为 `CheckpointBoundedSnapshot`（按 `SemanticEventId` 去重），且 `NON_AUTHORITATIVE` 持续禁止直接充当控制门禁权威输入。 | [C-ROAD-001](../c-road-001-partition-restart-migration-gate.md)、[C-SYNC-001](../c-sync-001-signed-event-set-merge.md)、[C-SYNC-002](../c-sync-002-artifact-sync-policy.md)、[C-NOTIFY-001](../c-notify-001-task-lifecycle-action-token.md)、[C-SEARCH-001](../c-search-001-non-authoritative-checkpoint-search.md) | `road_c001_compound_...`（`L5049–L5297`）；`road_c001_duplicate_messages_and_action_tokens_across_reconnection_are_idempotent_and_one_shot`（`L5882–L6109`）；`slice_k_runtime_and_cell_host_wire_live_search_and_notify_surfaces`（`L6117–L6304`） |
| **5. 额度冻结与有界损失数学证明**（`[LEASE-TTL-001]`、`[LEASE-FENCE-001]`、`[LEASE-LOSS-001]`、`[LEASE-RECON-001]`） | 短 TTL 到期自动 `sweep_expired -> FENCED`，epoch/boot 推进将未对清租约冻结为 `Quarantined`（严禁在收到匹配 `FenceBarrierAck` 的验真对账回执前退回 `AVAILABLE`）；伪造 `FenceBarrierAck` 以 `InvalidFenceBarrierAck` 拒绝；超窗口未证实损耗（`60 > 40`）以 `BoundedLossViolation` 硬拒绝；重连四阶段收敛产出可验签摘要的 `PartitionBoundedLossProof`，严格满足三项守恒等式 `reconciled_spent + refunded_available + permanently_quarantined_loss == total_granted_face_value` 且 `permanently_quarantined_loss <= max_unreported_inflight_window`（测试 1 Cell A：`120 + 250 + 30 == 400`, `30 <= 50`；Cell B：`90 + 200 + 10 == 300`, `10 <= 25`），且重复回放对账回执零额外退款。 | [C-ROAD-001](../c-road-001-partition-restart-migration-gate.md)、[C-PARTITION-001](../c-partition-001-partition-autonomy-bounded-loss.md)、[C-LEASE-001](../c-lease-001-durable-three-family-reconciliation.md)、[C-CUSTODY-001](../c-custody-001-key-custody-gateway-reconciliation.md) | `road_c001_compound_...`（`L5049–L5190`）；`road_c001_quota_freeze_and_exact_bounded_loss_under_partition_and_crash`（`L5504–L5741`） |

### 3.2 行级源码断言抽审与本评审实跑核验

本评审直接抽查了 [c_road_001_h6_partition_restart_migration_gate.rs](../../../../crates/nlos-slice-k/tests/c_road_001_h6_partition_restart_migration_gate.rs)（共 `6,304` 行）：
- 子进程入口 `road_c001_cell_worker`（`L4363`）在独立子 OS 进程中独占打开 `CellHost::open_federated_durable`（`cell.lock` 持久化 OS PID 与 `node_boot_generation`），绝非同进程双实例模拟；
- 顶层 5 个测试函数（`L4697`、`L5504`、`L5749`、`L5882`、`L6117`）在分区阶段均验证存活心跳与 fail-closed 门禁，在崩溃阶段均调用真实 `Child::kill()` 并校验 `!kill_status.success()` 与重启后 `os_pid != pid_a1`，在对账阶段逐字段校验 `receipt.verify_digest()`、守恒和式、`PARTRECV` / `PMIGRCPT` 联邦检查点摘要与幂等二次重放。

**本评审现场实跑结果（`HEAD = 9e5e0edc33cbcf89f2be9dd25f369cd81eba5237`，Windows 11 x86_64）**：
```text
$ cargo test -p nlos-slice-k --test c_road_001_h6_partition_restart_migration_gate
running 6 tests
test road_c001_cell_worker ... ignored
...（孵化 13 个真实子 OS 进程 road_c001_cell_worker 全部 ok）...
test slice_k_runtime_and_cell_host_wire_live_search_and_notify_surfaces ... ok
test road_c001_migration_crash_after_source_fenced_quarantines_without_dual_active_process ... ok
test road_c001_duplicate_messages_and_action_tokens_across_reconnection_are_idempotent_and_one_shot ... ok
test road_c001_quota_freeze_and_exact_bounded_loss_under_partition_and_crash ... ok
test road_c001_compound_partition_crash_restart_migration_and_duplicate_replay_converges ... ok

test result: ok. 5 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 2.15s (exit code 0)
```

---

## 4. `[ROAD-C-002]` 逐条款证据审查（无全局锁分片规模基准与五类故障验证）

> **门原文（v0.5 §28.3 行 `4903`）**：「海量 Agent benchmark MUST 证明 spawn/message/commit 不依赖全局锁或全局总序，并验证 stale TaskAuthority epoch、分区双主、fanout storm、provider stall 与 Cell bulkhead。」
> **配套性能规范（v0.5 §27 行 `4800–4819` + §25.2 `[SCALE-PROFILE-001]`）**：`[PERF-CLASS-001]`、`[PERF-BENCH-001]`、`[PERF-SCALE-001]`、`[PERF-SCALE-002]`、`[PERF-CONTROL-001]`。

### 4.1 规模与持久性基准条款核对表（主证据：[C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md)）

| 规范锚点 | 具名测试与代码位置 | 核心实测数据与架构证明 | 审查结论 |
|---|---|---|---|
| **无全局锁 / 无全局总序 `spawn/message/commit` 扩展**（`[ROAD-C-002]` 前半句 + `[DIST-TASK-003]`） | [c_bench_task_sharded_scale.rs](../../../../crates/nlos-task/tests/c_bench_task_sharded_scale.rs) `L966` `road_c002_sharded_spawn_message_commit_scales_without_global_lock_or_total_order` | `1 -> 2 -> 4 -> 8` 个**独立 OS 子进程** `Cell` 分片（各自独占磁盘目录、`CellAuthority`、`QuotaLease` 切片、`SqliteTaskAuthority` `WAL+FULL` 与 `HierarchicalFanoutTree`），跨分片零共享互斥锁、零全局序号发生器、零全局总序协调；`--release`（6C/12T Ryzen 5 9600X）聚合吞吐 `994.4 -> 1,652.1 -> 3,001.6 -> 4,417.3 ops/s`（**`4.44x`** 加速比），`dev` 构建 `652.2 -> 3,769.9 ops/s`（**`5.78x`** 加速比）；每次叶子更新仅增量重算路径上 `2` 个子组节点（远小于全量 `49` 次折叠）且 Merkle 包含证明 100% 验真。 | `SATISFIED`（单机多进程实盘域） |
| **`[PERF-BENCH-001]` 持久屏障铁律** + **`[PERF-CLASS-001]` 三类工作负载** | [c_bench_road_c002_scale_and_faults.rs](../../../../crates/nlos-runtime-tokio/tests/c_bench_road_c002_scale_and_faults.rs) `L343` `perf_class_001_fast_streaming_and_slow_workload_classes_benchmark` | 全部 7 类 SQLite 权威库均在测试内强制断言 `PRAGMA journal_mode = WAL` 与 `PRAGMA synchronous = FULL (2)`（零 `:memory:`、零 `NORMAL` 降级）。`--release` 实测：`Fast`（`0` 落盘屏障/op）`38,385.4 ops/s`、`p50=24 µs / p95=42 µs` 且零残留 Fiber；`Streaming` 逐 chunk `p50 < 1 µs / p95=14 µs`（触发 `126` 次有界背压挂起，`8/8` 条中途取消流精确围栏零泄漏）、整窗 `1×WAL+FULL` 闭合 `1,623.9 streams/s`（`p50=665 µs`）；`Slow`（`5×WAL+FULL` 屏障/op）`614.9 ops/s`（`p50=1,616 µs / p95=1,965 µs`），重开后 `pending_outbox == 0` 且重复 `complete` 幂等重放。 | `SATISFIED` |
| **`[PERF-SCALE-001/002]`** 与 **`[SCALE-PROFILE-001]`**（8-Cell `10K_LOGICAL` & `100K_LOGICAL`） | 同上 `L1772` `perf_scale_001_002_sharded_10k_...`、`L1778` `perf_scale_001_002_sharded_100k_...` | 严格分离 `logical`（冷元数据）、`materialized`（`1%/5%/10%` 工作集）、`runnable` 与 `active_fibers`（`host_threads = 8`，活跃协程上限 `2,048`）：`100K_LOGICAL`（`8 × 12,500`）登记吞吐 `52,967.5 nodes/s`，归并后每节点落盘 **`634 B/node`**（8 分片全套库总计 `70.58 MiB`），重启恢复扫描总墙钟 **`38.07 ms`**（`10K` 为 `33.74 ms`），宿主常驻 `WorkingSet64` 仅 `64.19–69.97 MiB`；同 Cell 跨 Worker 原子窃取在 `100K@10%` 成功 `2,496` 次；Checkpoint+Cold 驱逐 `p50=3,320 µs`、Rehydrate `p50=1,498 µs`、Channel 唤醒 `p50=51 µs`。按 `[EVID-NEG-001]` 如实暴露 `MaterializationScheduler::select` 分片内 `O(N/S)` 扫描成本与并行 NVMe `FlushFileBuffers` 争用下的 `p99` 尖峰。 | `SATISFIED-WITH-BOUNDARIES`（`1M_LOGICAL` 与多平台 CI release 留界） |
| **`[PERF-CONTROL-001]` 大规模手动控制面响应度** | 两套基准测试内并发采样（`128` 次主键控制/有界探针） | 所有控制动作（`inspect_node`、`inspect_node_residency`、`cancel_group`、`cancel_task`、`inspect_working_set_pressure`、`drive_working_set_reclaim`、有界 `pending_outbox(8)`）均按主键或有界窗口执行，零全量 Agent 扫描：8-Cell 并发提交满载下 `p50 = 124 µs / p95 = 825 µs`；`100K_LOGICAL` 常规跑 `p50 = 85 µs / p95 = 419 µs`，即便遭遇 NVMe 写缓存争用 `p50` 仍为 **`84 µs`**、`p95 = 450 µs`、最坏单次 `< 227 ms`。 | `SATISFIED` |

### 4.2 `[ROAD-C-002]` 五大并发故障类目逐条核验矩阵

| 门原文故障类目 | 具名测试函数与行号 | 并发负载与故障注入手段 | 关键断言与实测结果 |
|---|---|---|---|
| **1. `stale TaskAuthority epoch`** | [c_bench_task_sharded_scale.rs](../../../../crates/nlos-task/tests/c_bench_task_sharded_scale.rs) `L1058` `road_c002_fault_stale_task_authority_epoch_fences_old_term_workers_under_load` | 4 个分片并发满载提交中将 Shard 0 租约推进至 `Term 2`；旧 `Term 1` worker 持接管前已签发 permit 调 `finalize_commit_v3` 并继续打入 `15` 次旧 term permit 请求。 | **`16/16`** 次旧 term 操作全部以 `TaskStoreError::AuthorityLeaseFenced` 拒绝（零静默提交）；Shard 0 新 `Term 2`（`16/16`）与 Shards `1..3`（`48/48`）共 **`64/64`** 健康任务 100% 提交成功（`release` `p50=3,119 µs`）。 |
| **2. `分区双主`（dual-primary partition）** | 同上 `L1349` `road_c002_fault_dual_primary_partition_arbitrates_single_canonical_winner` | `ControlPlaneLinkPhase::Partitioned` 下验证未过期租约不可抢先接管（`AuthorityLeaseHeld`）；随后双竞争主节点在并发屏障下对 `24` 个任务同时竞争 `CommitPermit` + `finalize_commit_v3`。 | 本地已授权 `QuotaLease` 扣减放行且跨 Cell 未授权 canonical 发布被 `CrossCellCanonicalPublishWithoutLocalAuthority` 围栏拒绝；`24` 个竞争任务精确产生 **`24` 个唯一赢家**与 `24` 个被围栏输家（`Superseded` / `StaleTaskHead`），全部 `head_commit_seq == 1`，**脑裂数为 `0`**。 |
| **3. `fanout storm`** | 同上 `L1570` `road_c002_fault_fanout_storm_triggers_waiting_resource_backpressure_and_preserves_siblings` | 失控组在 Cell 0 并发喷发 `48` 个超额预留与超深（`depth > max_group_depth`）spawn 请求，并尝试绕过内存账本直接向 `SqliteTaskAuthority` 超限塞入子任务；健康兄弟组与 Cell 1 并发执行 `24` 个提交（关联 [C-FANOUT-001](../c-fanout-001-hierarchical-fanout-bulkhead.md)）。 | 精确放行预算内 `4` 个预留，`16` 个进入 `WaitingResourceBackpressure`（标注饱和轴），`20` 个 `BackpressureQueueFull` 丢弃，`8` 个超深 `MaxGroupDepthExceeded` 硬拒绝，存储旁路写入被 `GroupFanoutExceeded` 拦截；`24/24` 健康兄弟提交零受阻，`SafetyTcbSystemReserve` **`4/4` 完好**。 |
| **4. `provider stall`** | [c_bench_road_c002_scale_and_faults.rs](../../../../crates/nlos-runtime-tokio/tests/c_bench_road_c002_scale_and_faults.rs) `L1787` `road_c002_fault_provider_stall_isolates_bulkhead_fences_timeout_and_preserves_fast_path` | `provider_stalled`（并发上限 `4`、队列上限 `4`）注入挂起与 `FailProviderRpc` 并打入 `16` 个慢调用；同期并发跑 `128` 个 `Fast` 协程与 `24` 个 `provider_healthy` 持久慢操作；随后触发取消/超时围栏并注入迟到回调。 | `provider_stalled` 精确限制为 `4 active + 4 queued + 8 RejectedQueueFull`；`Fast` 路径保持微秒级（`release` `p50=7 µs / p95=20 µs`），`provider_healthy` `24/24` 完成；取消后全部 `4/4` 迟到回调路由至 `OutboxKind::ReconcileEffect`（零误唤醒），残留泄漏 Fiber 数为 **`0`**，`SafetyTcb` `4/4` 完好。 |
| **5. `Cell bulkhead`** | 同上 `L2112` `road_c002_fault_cell_bulkhead_preserves_safety_tcb_drr_fairness_and_offloads_healthy_cells` | Cell 0 上 `group_flood` 突发 `200` 次洪泛，同 Cell 兄弟 `group_fair`（权重 `3`）与 `group_flood`（权重 `1`）同优先级竞争 `Worker`；Cell 0 标记 `ModelSlot` 饱和后向 `Global` 发起 `64` 次首选 Cell 0 的跨 Cell 放置（关联 [C-SCHED-001](../c-sched-001-three-tier-scheduler-domain.md)）。 | `group_flood` 被隔离在 `4 acquired + 4 queued + 192 shed`；`SafetyTcbSystemReserve` **`6/6` 槽位立即可获**；同 Cell `Worker` DRR 前 `12` 次出队精确呈现 **`9 : 3`**（零饥饿）；反向背压将 `Global` 窗口 `128 -> 116` 收缩，且 **`64/64` 次跨 Cell 放置 100% 自动卸载至健康 `Cells 1..7`**。 |

### 4.3 本评审现场实跑核验（`HEAD = 9e5e0edc33cbcf89f2be9dd25f369cd81eba5237`）

```text
$ cargo test -p nlos-task --test c_bench_task_sharded_scale
running 5 tests
test c_bench_cell_shard_worker_entry ... ignored
test road_c002_fault_dual_primary_partition_arbitrates_single_canonical_winner ... ok
test road_c002_fault_fanout_storm_triggers_waiting_resource_backpressure_and_preserves_siblings ... ok
test road_c002_fault_stale_task_authority_epoch_fences_old_term_workers_under_load ... ok
test road_c002_sharded_spawn_message_commit_scales_without_global_lock_or_total_order ... ok
test result: ok. 4 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.81s (exit code 0)

$ cargo test -p nlos-runtime-tokio --test c_bench_road_c002_scale_and_faults
running 5 tests
test perf_scale_001_002_sharded_100k_logical_multi_working_set_benchmark ... ignored
test road_c002_fault_cell_bulkhead_preserves_safety_tcb_drr_fairness_and_offloads_healthy_cells ... ok
test road_c002_fault_provider_stall_isolates_bulkhead_fences_timeout_and_preserves_fast_path ... ok
test perf_class_001_fast_streaming_and_slow_workload_classes_benchmark ... ok
test perf_scale_001_002_sharded_10k_logical_multi_working_set_benchmark ... ok
test result: ok. 4 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 3.37s (exit code 0)
```
（注：`100K_LOGICAL` `#[ignore]` 规模探针的 `--release -- --include-ignored --test-threads=1` 隔离实盘采样结果已完整固化于 [C-BENCH-001 §4/§8](../c-bench-001-road-c002-distributed-benchmark.md)。）

---

## 5. `C-core` 九包与阶段 B → C 移交 `#1–#17` 全量终局处置表

### 5.1 `C-core` 九大工作包终局核对表（[stage-c-progress §C.3.1](../../../management/stage-c-progress.md#c31-c-core-工作包阶段-c-本体)）

| ID | 规范锚点 | 落点 Crate / 模块 | 支撑 Evidence | 状态 | 已证范围与显式残余边界 |
|---|---|---|---|---|---|
| `C-CELL` | §26.1、`DIST-LOCAL/FAIL/NAME-001` | `nlos-cell`、`nlos-slice-k::cell_host` | [C-SHARD-002](../c-shard-002-checkpoint-digest-verification.md)、[C-SHARD-006](../c-shard-006-production-takeover-binding.md)、[C-PARTITION-001](../c-partition-001-partition-autonomy-bounded-loss.md)、[C-ROAD-001](../c-road-001-partition-restart-migration-gate.md) | **`PARTIAL_PASS`** | 本地七件套全量装配、持久化 `boot_generation`、单持有者 epoch 广播、联邦三机制（`CellDirectory`/`MigrationIntent`/`ReconciliationCheckpoint`）与 `PartitionAutonomyGate`/`RemoteCallOutcome` 已证；跨物理主机网络传输与共识基底留界（`RISK-B-11` / [ADR-0020](../../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md)）。 |
| `C-LEASE` | §12 全部 `LEASE-*`、[ADR-0021](../../../management/adrs/0021-lease-grantor-shared-cell-authority.md) | `nlos-lease`（`SqliteLeaseStore`）、`nlos-slice-k` | [C-LEASE-001](../c-lease-001-durable-three-family-reconciliation.md)、[C-CUSTODY-001](../c-custody-001-key-custody-gateway-reconciliation.md)、[C-PARTITION-001](../c-partition-001-partition-autonomy-bounded-loss.md)、[C-ROAD-001](../c-road-001-partition-restart-migration-gate.md) | **`PARTIAL_PASS`** | `Quota`/`Capacity`/`ExclusiveDevice` 三族 `WAL+FULL` 持久态（11 表 + 22 触发器）、`FenceBarrierAck` 门控、三族非互换对账回执、跨节点先关后授迁移与 `LEASE-WAL-001` 丢失隔离已证；跨物理主机共识留界（`RISK-B-11`）。 |
| `C-SYNC` | §26.2 `DIST-EVENT/ART/AUDIT-001`、`SEM-CHECKPOINT-001` | `nlos-semantic`、`nlos-artifact::sync` | [C-SYNC-001](../c-sync-001-signed-event-set-merge.md)、[C-SYNC-002](../c-sync-002-artifact-sync-policy.md)、[C-PARTITION-001](../c-partition-001-partition-autonomy-bounded-loss.md)、[C-ROAD-001](../c-road-001-partition-restart-migration-gate.md) | **`PARTIAL_PASS`** | 跨域三要素验签合并、SQLite `Immediate` 拓扑序原子写回、`SignedSemanticVectorCheckpoint`、四轴制品同步策略、`ArtifactTransferPackage` 逐字节 SHA-256 传输与同步感知孤儿 GC 已证；跨物理主机网络传输留界。 |
| `C-SHARD` | §26.1 `DIST-TASK-001/002/004`、[ADR-0019](../../../management/adrs/0019-cross-cell-commit-semantics-extension.md) | `nlos-task`（schema `v47`）、`nlos-cell`、`nlos-slice-k::takeover_drive` | [C-SHARD-001](../c-shard-001-cross-cell-takeover-drive.md)、[002](../c-shard-002-checkpoint-digest-verification.md)、[003](../c-shard-003-reconciliation-convergence-core.md)、[004](../c-shard-004-h6-fault-injection-matrix.md)、[005](../c-shard-005-dist-task-conformance.md)、[006](../c-shard-006-production-takeover-binding.md) | **`PARTIAL_PASS`** | Phase 0 + Phase 1（`CS1-A..E`，[c-shard-plan.md](../../../management/c-shard-plan.md)）全绿：三 Record 对齐 `v47`、六类 canonical endpoint 屏障全覆盖、`TaskPermitClosureReceipt` 代际推进、联邦绑定与 H6 注入矩阵已证；跨物理主机与远端物理 cleanup 留界（`RISK-B-11`）。 |
| `C-FANOUT` | `DIST-TASK-003`、`DIST-FANOUT-001`、`DIST-BULKHEAD-001` | `nlos-task::fanout` | [C-FANOUT-001](../c-fanout-001-hierarchical-fanout-bulkhead.md)、[C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md) | **`PARTIAL_PASS`** | 有界分支因子层级树、顺序无关局部归约、SHA-256 `MerkleResultRoot` + `O(B*depth)` 增量检查点、六维 `FanoutUsageLedger` + `WaitingResourceBackpressure`、四域舱壁与 `SafetyTcbSystemReserve` 已证；限定单机多进程/多分片域。 |
| `C-MIGRATE` | §26.3 `DIST-MIGRATE-001/002`、§26.1 控制面 | `nlos-process::migration`、`nlos-slice-k::process_migration_drive` | [C-MIGRATE-001](../c-migrate-001-process-migration-protocol.md)、[C-MIGRATE-002](../c-migrate-002-cell-host-process-migration.md)、[C-ROAD-001](../c-road-001-partition-restart-migration-gate.md) | **`PARTIAL_PASS`** | 八步严格有序状态机、固定会话显式拒绝/降级 `restart-from-artifact`、跨双真实子 OS 进程 `CellHost` 三族租约守恒重授、Capability 衰减重绑与 post-fence 故障自动隔离已证；跨物理主机网络传输与物理内存热迁移留界（`RISK-B-11`）。 |
| `C-SCHED` | §28.3 第 6 条、`SCHED-HIER/QUEUE/STEAL/BACKPRESSURE-001` | `nlos-runtime::scheduler_domain`、`nlos-runtime-tokio::distributed_scheduler` | [C-SCHED-001](../c-sched-001-three-tier-scheduler-domain.md)、[C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md) | **`PARTIAL_PASS`** | `Global -> Cell -> Worker` 稳态零全局锁本地热路径、有界 DRR/老化/取消围栏、字典序安全窃取（严拒持活跃 permit/reservation/device 项）、热 `Context` 亲和及反向背压跨 Cell 卸载已证；跨物理主机调度 RPC 留界（`RISK-B-11`）。 |
| `C-PARTITION` | `DIST-LOCAL-001`、`DIST-FAIL-001`、`ROAD-C-001` | `nlos-cell::partition`、`nlos-slice-k::partition_recovery` | [C-PARTITION-001](../c-partition-001-partition-autonomy-bounded-loss.md)、[C-ROAD-001](../c-road-001-partition-restart-migration-gate.md) | **`PARTIAL_PASS`** | 三态断连自治门控、远端四态失败契约、双存活子 OS 进程四阶段重连收敛、`PartitionBoundedLossProof` 数学守恒证明及复合强杀/接管/迁移重放门禁已证；跨物理主机网卡分区留界（`RISK-B-11`）。 |
| `C-BENCH` | `ROAD-C-002`、§27 `PERF-*`、`SCALE-PROFILE-001` | `nlos-task`、`nlos-runtime-tokio` | [C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md) | **`PARTIAL_PASS`** | `Fast/Streaming/Slow` 基准、`1->2->4->8` 子进程分片无全局锁扩展、8-Cell `10K/100K_LOGICAL` `WAL+FULL` 压测、`100K` 手动控制 `p50=84–85 µs` 及五类并发故障注入已证；`1M_LOGICAL` 与跨物理主机网络基准留界。 |

### 5.2 阶段 B → C 移交清单 `#1–#17` 全量终局处置表（[stage-c-progress §C.3.2](../../../management/stage-c-progress.md#c32-移交项工作包656-117) / [stage-b-progress §6.5.6](../../../management/stage-b-progress.md)）

| # | 工作包 ID | 移交事项要点 | 终局处置状态 | 落盘证据 / 提交锚点 | 残余边界与后续归置（Owner / 风险号） |
|---:|---|---|---|---|---|
| `#1` | `C-REG-12` | `slice-k-demo` STEP 09d defect 根因处置与 `RISK-B-12` 升级门裁决 | **`DONE`** | merge `87ef288`、[B-SLICE-K-001 §17](../../stage-b/b-slice-k-001-end-to-end.md) | 无；`RISK-B-12` 凭回执级证据排除误收在册 blob，裁决不升 `P0` 并 `closed`。 |
| `#2` | `C-APP-PAYLOAD` | 第三方样板应用载荷执行面 + `nlos package` CLI 全周期 | **`PARTIAL_PASS`** | [C-APP-PAYLOAD-001](../c-app-payload-001-sample-cli-execution.md)、[B-APPLICATION-007](../../stage-b/b-application-007-third-party-sample.md) | `examples/sample-app` 已走通 `execute_application_payload` 与 `keygen..uninstall` CLI 全链；客户机 Wasm/ELF 指令级沙箱执行面为独立隔离边界（[ADR-0015](../../../management/adrs/0015-dual-layer-untrusted-execution-isolation.md)）。 |
| `#3` | `C-GUI-CAMPAIGN` | GUI 真机交互战役（computer-use 清单）+ Windows GUI | **`Explicitly Governed / Re-scoped`（§C.5.4 决策点 3）** | 桌面 Windows 认证管道与三平台 CI 前端打包已由 `W52`（`f41549f`）/ `W44-F2` 落地；交互点按战役维持常设观察项（[W34-A §5.4](../../stage-b/reviews/w34a-six-gate-matrix.md)） | 按 `§C.5.4` 决策点 3 (ii) 归置为**阶段 D 邻接常设观察项**（需维护者本机 GUI 会话执行，`RISK-B-06` / `U-3`），不阻塞 C 退出技术门。 |
| `#4` | `C-WIN-KILL` | Windows live-child 进程树实杀（`taskkill /F /T`） | **`PARTIAL_PASS`** | `W62-CWINKILL`（`728a765b`）、[B-PROCESS-003 §13](../../stage-b/b-process-003-crash-propagation.md) | 已在真实 Windows 宿主实跑杀死父+子双活进程树且兄弟树 100% 存活，`RISK-B-10` 降为 `partial`；残余跨权限/UAC 子进程与 Job Object breakaway 边界保留于 `RISK-B-10` / `U-2`。 |
| `#5` | `C-SCALE-RELEASE` | `release-profile` + Linux/Windows 规模复测与 CI 化 | **`PARTIAL_PASS`** | merge `09c0832`、[B-SCALE-RELEASE-001](../../stage-b/b-scale-release-001.md)、夜间 run `36641210416` 三平台 `Scale probe release` 实跑 + 本机 Windows `--release` [C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md) | 按 `§C.1.2 (5)` 与 C 退出门显式解耦；多平台 CI release 规模数字回填前，持续遵守「不对外声称无限定生产级 PID/coroutine 容量」禁令（`RISK-B-07` / `U-1`）。 |
| `#6` | `C-RUNTIME-SCALE` | `100K` cancel/batch-cancel、多 worker wake fairness、墙钟分布 | **`PARTIAL_PASS`** | merge `4df55d6`、[B-RUNTIME-002 §6.19](../../stage-b/b-runtime-002-fiber-scale.md) | 三探针前片已验证；全量 durable-wait 挂起下 `O(n^2)` 终态 purge 家族与 `100K` 唤醒风暴留作后续运行时优化项（`U-6`）。 |
| `#7` | `C-SELECTOR` | G4 生态 selector 半边；G3 三条件从 digest 升结构化 | **`PARTIAL_PASS`** | merge `8b91419`、[B-PLAN-001 §12](../../stage-b/b-plan-001-declaration-surface.md) | `Application`/`Artifact` typed selector 与 G3 三条件结构化已验证；规范未定稿的 `Topic/Skill/Tool/Model/外部服务` selector 不发明（`RISK-B-09` / `U-8`）。 |
| `#8` | `C-PLAN-HARDEN` | apply 侧 admission consult、reclaim×residency、PINNED、调度器探针、`100K@50%` | **`PARTIAL_PASS`** | merge `8ed592a`、[B-PLAN-001 §13](../../stage-b/b-plan-001-declaration-surface.md)、[B-TASK-SCALE-001 §15](../../stage-b/b-task-scale-001.md) | 四项核心加固已验证；`PINNED` SABI 字段扩展、分片内 `O(W)` 就绪索引（见 [C-BENCH-001 §7](../c-bench-001-road-c002-distributed-benchmark.md)）与 `apply_plan_revision` 旧夹具迁移显式登记保留（`U-9`）。 |
| `#9` | `C-PROVIDER-REAL` | Provider 真实载体替换 mock + 跨平台认证传输 + 载荷 codec 冻结 | **`PARTIAL_PASS`** | [ADR-0024](../../../management/adrs/0024-driver-provider-payload-codec-freeze.md)、[C-PROVIDER-001](../c-provider-001-real-process-provider-codec.md) | 第 8 个 SABI 通道 `DriverProvider` v1.0 已冻结并同步 TS/Python 生成物；独立子进程 `nlos-provider-worker` + 跨平台 Unix Socket / Windows Named Pipe `ADR-0011` 认证 IPC + `PROVIDER_ISOLATION_UNVERIFIED` 强制姿态已证；公有云商用 LLM HTTP SDK 与硬件 TEE 在线根证书验真归部署适配层（`RISK-B-05` / `U-10`）。 |
| `#10` | `C-RUNTIME-PROC` | kill receipt × Activation meter 联动、B6-4 supervisor、B6-5 `BirthDecision` | **`PARTIAL_PASS`** | merge `c45bd10`、[W36-P10](../../stage-b/w36-p10-c-runtime-proc.md) | 三子项均已实现并落档验证；5 项细粒度文档/API 打磨小项具名保留（`U-11`）。 |
| `#11` | `C-APP-CONTROL` | Application 层控制面与生命周期 NL 动词、supervisor 自动 pid 发现/unregister | **`PARTIAL_PASS`** | merge `f49c5a8`（[B-CONTROL-003](../../stage-b/b-control-003-nl-prefix.md)）、`ce06746`、`7e975d7`、`3560800`、`W47-L1` `67678bd` | `disable/uninstall`、`InspectApplication`（含 desktop 接线）、按代次 `unregister` 与 `scan_pids` 自动发现均已合入测试；OS 全进程表主动枚举型 resolver 与后片独立汇编留界（`RISK-B-08` / `U-12`）。 |
| `#12` | `C-LIFECYCLE` | `restore_process` 复活链、干净退出终态、teardown 竞争、NL kill 幂等键同源 | **`PARTIAL_PASS`** | `cb51acb`、`d391ba7`、`3d8da80`、`W46-B` `4272cdf`（`bbcd431` + `05b67ca`） | 干净退出、过期代次拒绝、拆卸采纳 platform-kill 回执、反向 NL 时间戳同源与 `run_process_revival` 完整复活链均已在 `nlos-slice-k` 落地全绿；因未单独拆出专项 Stage C Evidence 文件，按 `Claim <= Evidence` 保守维持 `PARTIAL_PASS`（`U-13`）。 |
| `#13` | `C-CONFORMANCE-GOLDEN` | TS/Python conformance 对 SABI v1.2–v1.5 新臂/新视图 golden 钉死 | **`DONE`** | merge `5c6f4c7`、[B-SCHEMA-002 §6](../../stage-b/b-schema-002-cross-language-generation.md)（且 `W65` `ADR-0024` 同步零漂移） | 无；`U-14` 已退役。 |
| `#14` | `C-SCALE-PROBE` | 夜间 `scale-probe` 既有失败专项排查与常设门复核 | **`Explicitly Governed / Re-scoped`（根因闭合 + 常设门持续生效）** | merge `ebbeddc`、[B-RUNTIME-002 §6.18](../../stage-b/b-runtime-002-fiber-scale.md)、dispatch `35560466719`、`W43-A` `1e60707`（修复 child harness 裸跑）、schedule run `36641210416`（`2026-09-30` 10/10 全绿） | 既有失败与 09-24 连红均已根因修复并在真实夜间 `schedule` run `36641210416` 验证全绿；按 `§C.2` 与 `§C.5.5 (6)`，夜间 `schedule` 作为**跨阶段常设门（standing gate）**持续监控，不以旧 run 冻结未来 tip。 |
| `#15` | `C-MULTICELL` | 多 Cell / 分布式（阶段 C 本体）+ Notification/Search 扩展与解冻 + 完整桌面 | **`PARTIAL_PASS`**（完整桌面按决策点 3 归 **D 册**） | `C-core` 九包 Evidence + [C-SEARCH-001](../c-search-001-non-authoritative-checkpoint-search.md) + [C-NOTIFY-001](../c-notify-001-task-lifecycle-action-token.md) + [C-UNFREEZE-001](../c-unfreeze-001-search-notify-production-wiring.md) + [C-ROAD-001](../c-road-001-partition-restart-migration-gate.md) + [C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md) | 多 Cell 单机多进程域与 Search/Notify 双生产消费线全量解冻闭合；跨物理主机网络归 `RISK-B-11`，完整消费级桌面（`§28.4` / `§29.2`）按 `§C.5.4` 决策点 3 (i) 显式移交**阶段 D 册**。 |
| `#16` | `C-POWER-LOSS` | 真实硬件掉电（Layer 3+）与 M4/M6/M8 模型校准 | **`Explicitly Governed / Re-scoped`（§C.5.4 决策点 3）** | Layer 1 APFS 校准与 Layer 2 `dm-flakey`（run `33895972272`、`36506566252`）已落档 [B-STORE-FS-SEMANTICS-001](../../stage-b/b-store-fs-semantics-001-apfs-calibration.md) | 按 `§C.5.4` 决策点 3 (iii) 裁定为**跨阶段持续项**（遇真机掉电工装即执行，不作软件仓库 C 期硬阻塞，`U-4`）。 |
| `#17` | `C-CUSTODY-GATEWAY` | 生产 signing key custody / enforcement-gateway reconciliation authority | **`PARTIAL_PASS`** | [C-CUSTODY-001](../c-custody-001-key-custody-gateway-reconciliation.md)（关联 [ADR-0017](../../../management/adrs/0017-resource-operation-cross-authority-prepare-finalize.md)、[ADR-0022](../../../management/adrs/0022-operator-root-capability-issuance.md)） | `SigningKeyCustodyVault`（`WAL+FULL`，KEK 封装零明文种子导出、轮换与吊销墓碑）与 `EnforcementGatewayReconciliationAuthority`（`Provisional` / `FinalNonSupersedeable` 有界 `R-C -> R-B` 双域收敛）已证；硬件 HSM / OS Enclave 后端为生产部署边界。 |

---

## 6. 显式残余边界与阶段 C 退出门 Readiness 核对（§C.5.5 六条件）

### 6.1 严禁外推的技术与部署边界汇总（`Claim <= Evidence` / `[EVID-SCOPE-001]` / `[EVID-NEG-001]`）

以下 8 条边界从底层 22 份 Stage C Evidence 与 Stage B 继承台账逐条汇总，任何阶段 C 退出声明、文档摘要与对外口径均**不得抹平或外推**：

1. **单机多 OS 进程拓扑 vs 跨物理主机网络与共识（[ADR-0018](../../../management/adrs/0018-single-host-multiprocess-dual-cell-topology.md) / [ADR-0020](../../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md) / `RISK-B-11` / `U-5`）**：阶段 C 全部多 `Cell` 证据均在单台宿主机上通过多个独立子 OS 进程、独立持久目录 `SQLite WAL + FULL` 与共享文件系统 `CellDirectory` / 原子重命名邮箱 IPC 完成；**不声称**已验证跨物理主机的 TCP/RDMA 传输、真实网卡丢包/乱序、跨机时钟漂移、Raft/Paxos 控制面多副本共识或远端不可达宿主的物理副作用清理证明。
2. **Layer-2 进程强杀/`dm-flakey` vs Layer-3 物理断电硬件介质（`U-4` / `#16`）**：`Child::kill()` 硬崩溃冷重启与 Linux `dm-flakey` 块层故障注入属于操作系统页缓存存活或软件块层模拟的 Layer-2 证据，**不等同于**掉电测试台下的真实硬件断电与控制器写撕裂证明。
3. **`10K/100K_LOGICAL` 单机实盘基准 vs `1M_LOGICAL` 及多平台生产容量声明（`RISK-B-07` / `U-1` / `#5`）**：[C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md) 已在 Windows 11（6C/12T, 15.06 GiB RAM）`--release` 下实证 8-`Cell` `10K_LOGICAL` 与 `100K_LOGICAL`（最高 `10,000` 物化节点、`2,048` 活跃 Fiber）及分片内 `O(N/S)` 物化扫描成本，但 **`1M_LOGICAL` 未在当前单机档位声称**，且绝不将 `100K` 冷元数据宣称为“同时运行十万个大模型推理”；跨三平台 CI release 规模数字回填前维持生产容量声明前置禁令。
4. **独立子进程认证 Provider vs 外部商用云 LLM HTTP SDK 与硬件 TEE 远程证明（`RISK-B-05` / `U-10` / `#9`）**：`OutOfProcessProviderDriver` + `nlos-provider-worker` 已闭合跨进程边界、跨平台认证 IPC 与 `ADR-0024` SABI 通道冻结，但公有云商用模型 HTTP/SSE 客户端适配与 Intel TDX / AMD SEV-SNP 硬件根证书在线验真属于外部部署适配层（缺失硬件证明时内核强制打标 `PROVIDER_ISOLATION_UNVERIFIED`）。
5. **软件 KEK 持久密钥托管库 vs 物理 HSM / OS Enclave（`#17` / [C-CUSTODY-001](../c-custody-001-key-custody-gateway-reconciliation.md)）**：`SigningKeyCustodyVault` 已在软件层实现种子加密落盘与零明文导出 API，但未声称硬件防篡改芯片（PKCS#11 HSM / KMS / Secure Enclave）物理防护。
6. **确定性载荷执行桥 vs 客户机指令级强沙箱（`#2` / [ADR-0015](../../../management/adrs/0015-dual-layer-untrusted-execution-isolation.md)）**：`execute_application_payload` 走确定性 Driver Operation + 语义回执桥，不冒充不可信原生 ELF/PE 或 Wasm 字节码的微虚拟机/指令级沙箱（`RISK-B-03`）。
7. **Windows `taskkill /F /T` 进程树实杀 vs 跨权限/Job Object Breakaway（`RISK-B-10` / `U-2` / `#4`）**：同权限父子进程树强杀与兄弟隔离已在 Windows 实机闭合；跨 UAC 提权子进程或显式脱离父进程树的进程仍需未来 Windows Job Object 句柄级收口。
8. **阶段 D 完整桌面与全球联邦延后边界（`v0.5 §28.4` / `§29.2 [NLOS-DEFER-001]` / `#3` / `#15`）**：完整消费级桌面（全量窗口管理、多模态 Surface 渲染、GUI 真机交互点按战役）、全球跨组织联邦清算与生产级 Application Store 按规范分别归属阶段 D / 阶段 E / v0.5 延后项，阶段 C 仅保留机制接口与 Stage D 前片引用。

### 6.2 [stage-c-progress §C.5.5](../../../management/stage-c-progress.md#c55-阶段-c-退出评审门) 六条件 Readiness 逐项核对

| 条件 # | §C.5.5 退出评审门条款 | 当前核对结论 | 说明与收口责任方 |
|---:|---|---|---|
| **1** | `ROAD-C-001` / `ROAD-C-002` 两门各有 Evidence review 记录（`H6` 维度实质性覆盖门原文全部故障类目），无一 `P0` 未决风险 | **`MET`（本文件交付即闭合）** | 本文件 §3 与 §4 逐条款审查并现场复跑核验了 `ROAD-C-001`（分区/重启/迁移/重复消息 + 一致性/额度冻结/损失上界）与 `ROAD-C-002`（无全局锁/总序扩展 + 五类并发故障）的 `H6` 证据链；`risks.yaml` 未决 `P0 = 0` 已核实。 |
| **2** | `claims` / `risks` / `evidence` 机器台账 `stage-c` 域全量回填，`Claim <= Evidence` lint 通过 | **`READY FOR W67 LEDGER SYNC`** | 截至 W66 屏障 `evidence-index.yaml`（`168/168`）与 `scripts/lint_claims.py` 均 `PASS 0 ERROR`；本波 `W67` 兄弟台账车道与单一 integrator 同步将本评审记录及 Stage C 正式 claims/risks 状态写入 [claims.yaml](../../../management/claims.yaml)、[risks.yaml](../../../management/risks.yaml)、[evidence-index.yaml](../../../management/evidence-index.yaml) 并复跑 `lint_claims.py` 全绿后闭合。 |
| **3** | 未知风险清单显式列出（含 `RISK-B-11` 等分域退役/重定级判定），各项有 owner 与缓解措施 | **`READY FOR W67 RISK-DOC SYNC`** | 本文件 §5.2 与 §6.1 已完整给出 `RISK-B-11`（单机多进程已证 vs 跨机/远端物理 cleanup 保留）、`RISK-B-10`、`RISK-B-05..09` 及 `U-1..U-14` 的精确分域状态；由 `W67` 未知风险清单车道同步刷新 [exit-unknown-risks.md](../../../management/exit-unknown-risks.md) Stage C 章节后闭合。 |
| **4** | [管理机制 §7](../../../management/README.md#7-评审与决策机制)「Stage 退出或 production claim」三件套（Evidence review + 未知风险清单 + **维护者明确批准**）完成 | **`READY FOR MAINTAINER APPROVAL`（唯一保留人工门）** | 技术证据链（22 份 Stage C Evidence + 全仓 `2128+` 测试通过 + 全仓零冻结/零退役）、本综合评审记录与未知风险/机器台账均已就绪呈批。**严格纪律：在获得维护者明确批准前，阶段 C 权威状态必须保持 `IN_PROGRESS / NOT EXITED`，任何 Agent 不得自行将其翻转为 `EXITED`。** |
| **5** | §C.5.3 全部车道 `DONE`（或 `PARTIAL_PASS` 边界内闭合），或经批准显式移交（阶段 D/E 或持续项）并登记 | **`MET`** | `C-core` 九包全部在 `ADR-0018` 域达到 `PARTIAL_PASS` 并落档 Evidence；阶段 D 完整桌面、跨物理主机网络（`RISK-B-11`）与持续观察项均已按 `§C.5.4` 决策点 1–5 显式归置登记。 |
| **6** | 移交项 `#1–#17` 处置终局逐项登记（含 `RISK-B-12` 终局关闭、`#14` 夜间常设门、`#5` 生产量级声明前置关系明示） | **`MET`** | 见本文件 §5.2：`#1`（含 `RISK-B-12` 关闭）与 `#13` 为 `DONE`，12 项为有 Evidence/测试支撑的 `PARTIAL_PASS`，`#3`/`#14`/`#16` 按决策点 3 与常设门纪律显式治理，且 `#5` 与阶段 C 退出的解耦及声明前置禁令已按 `§C.1.2 (5)` 明示。 |

**综合评审结论**：阶段 C 在 [ADR-0018](../../../management/adrs/0018-single-host-multiprocess-dual-cell-topology.md) 单机多 OS 进程拓扑与真实磁盘 `SQLite WAL + synchronous=FULL` 持久层下的两道规范退出门（`[ROAD-C-001]`、`[ROAD-C-002]`）均达到 **`SATISFIED-WITH-BOUNDARIES`（`H6`）**，九大 `C-core` 工作包（`9/9 PARTIAL_PASS`）与 17 项阶段 B 移交项全部完成取证或显式治理归置，全仓 35 个 crate 实现**零冻结、零退役**，未决 `P0` 风险为 **`0`**。待本波 `W67` 集成器合流机器台账与未知风险清单并通过 `lint_claims.py` 后，阶段 C 退出评审包即具备完整呈批条件（**Ready for Maintainer Exit Approval**）；在维护者明确批准前，阶段级状态严格保持 **`IN_PROGRESS / NOT EXITED`**。
