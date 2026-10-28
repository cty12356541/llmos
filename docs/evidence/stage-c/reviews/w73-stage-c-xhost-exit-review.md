# W73 阶段 C 跨物理主机收官（`C-XHOST`）与退出门独立终审报告（`Single-Host W35–W67` + `Cross-Host W69–W72` / `ADR-0020` & `ADR-0026 VERIFIED`）

> - **状态**：`REVIEW`（`2026-10-28`，车道 `W73-C-EXIT-REVIEW` Attempt 1；面向 [stage-c-progress.md §C.5.5](../../../management/stage-c-progress.md#c55-阶段-c-退出评审门) 六项退出条件、[ADR-0020](../../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md) `CANDIDATE -> VERIFIED` 与 [ADR-0026](../../../management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md) `ACCEPTED -> VERIFIED` 的独立终审记录）
> - **日期**：`2026-10-28`
> - **评审基线 `HEAD`**：`5c052e53295972c2e261b0bd50de26628edfdfdc`
> - **Assurance**：`H6`（所审底层最高证据等级为 [06-架构设计总纲-v0.5.md](../../../design/06-架构设计总纲-v0.5.md) §30 `H6` 分布式混沌与跨进程/跨套接字故障注入；本终审严格执行 `Claim <= Evidence`、`[EVID-SCOPE-001]` 与 `[EVID-NEG-001]`，逐项核实 `W69–W72` 五份 `C-XHOST` 证据与 `W67` 单机多进程基线，零慷慨外推）
> - **写集边界**：严格限定为本报告（`docs/evidence/stage-c/reviews/w73-stage-c-xhost-exit-review.md`）、[ADR-0020](../../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md) 与 [ADR-0026](../../../management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md)；共享机器台账（[claims.yaml](../../../management/claims.yaml)、[risks.yaml](../../../management/risks.yaml)、[evidence-index.yaml](../../../management/evidence-index.yaml)、[exit-unknown-risks.md](../../../management/exit-unknown-risks.md)、[stage-c-progress.md](../../../management/stage-c-progress.md)）由本波屏障单一集成器 `W73-INT` 合流。
> - **前序基线与输入证据**：[W67 阶段 C 退出门综合评审](./w67-stage-c-exit-gates.md)、[C-XHOST-001](../c-xhost-001-mtls-transport-and-fence-handshake.md)、[C-XHOST-002](../c-xhost-002-conservative-clock-skew-and-remote-lease.md)、[C-XHOST-003](../c-xhost-003-embedded-raft-control-plane-consensus.md)、[C-XHOST-004](../c-xhost-004-streaming-sync-and-cross-host-migration.md)、[C-XHOST-005](../c-xhost-005-chaos-proxy-and-h6-multi-machine-gate.md)。

---

## 1. 执行裁决总览（Executive Verdict）

### 1.1 阶段 C 综合终审裁决：`SATISFIED-WITH-BOUNDARIES`（`H6`，`P0 = 0`，Ready for Maintainer Stage C `EXITED` Sign-off）

经对阶段 C **单机多 OS 进程基线（`W35–W67`，22 份证据）** 与 **跨物理主机扩展收官波次（`C-XHOST W69–W72`，5 份证据 `C-XHOST-001..005`）** 的独立交叉审计与现场复跑验证，本终审作出如下结论：

| 终审维度 | 最高证据等级 | 终审裁决 | 核心实证结论与诚实边界摘要 |
|---|---|---|---|
| **`[ROAD-C-001]`**（§28.3 行 `4901`：分区、重启、迁移与重复消息） | **`H6`** | **`SATISFIED-WITH-BOUNDARIES`** | 单机多进程门禁（[C-ROAD-001](../c-road-001-partition-restart-migration-gate.md)）+ 跨主机网络混沌门禁（[C-XHOST-004](../c-xhost-004-streaming-sync-and-cross-host-migration.md)、[C-XHOST-005](../c-xhost-005-chaos-proxy-and-h6-multi-machine-gate.md) `ROAD-C-001-XHOST`）双域合龙：在零共享目录、独立私有 `SQLite WAL+FULL`、真实 `rustls` TLS 1.3 mTLS + 五轴 Exporter 握手与 `ChaosTcpProxy`（三角形非对称分区、RTT 抖动、载荷中段 `RST`、密文位翻转、合成时钟偏斜越界、子进程 `Child::kill()` 强杀）下，完整闭合 Raft 控制面多数派重选与旧主围栏、远程租约偏斜越界自动 `QUARANTINED`、跨网八步进程迁移与固定会话拒绝、流式分块断点续传及有界损耗对账守恒。 |
| **`[ROAD-C-002]`**（§28.3 行 `4903`：无全局锁/无全局总序海量基准与五类故障） | **`H6`** | **`SATISFIED-WITH-BOUNDARIES`** | 单机 8-Cell 子进程实盘基准（[C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md) `10K/100K_LOGICAL`）+ 跨机分片基准（[C-XHOST-005](../c-xhost-005-chaos-proxy-and-h6-multi-machine-gate.md) `ROAD-C-002-XHOST` `c_xhost_bench_scale`）双域合龙：实证数据面 `spawn/message/commit` 热路径保持 **`0` Raft RPC**（严守 `[CONS-SCOPE-001]`），`1 -> 2 -> 4` 跨机分片吞吐线性扩展（`211–325 -> 843–1,039 commits/s`），且慢 I/O 分片隔离、租约过期与旧任期围栏、扇出风暴舱壁与 `SafetyTcbSystemReserve` 保护、Outbox 幂等重放、Cell 饱和跨节点卸载五类并发故障 100% 通过。 |
| **`C-XHOST` 五大收官工作包**（`W69–W72`） | **`H5`–`H6`** | **`5/5 PASS / VERIFIED`** | `C-NET-TRANSPORT`（`C-XHOST-001` `H5`）、`C-NET-CLOCK-LEASE`（`C-XHOST-002` `H5`）、`C-CONSENSUS-CP`（`C-XHOST-003` `H6`）、`C-NET-SYNC-MIGRATE`（`C-XHOST-004` `H6`）、`C-XHOST-GATE`（`C-XHOST-005` `H6`）已全部兑现 [ADR-0026](../../../management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md) §3.3 与 §7.2 规定的 DoD 验收门。 |
| **[ADR-0020](../../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md) & [ADR-0026](../../../management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md) 状态核准** | **`H6`** | **`PROMOTED TO VERIFIED`** | `ADR-0020`（`CANDIDATE -> VERIFIED`）与 `ADR-0026`（`ACCEPTED -> VERIFIED`）升级门及复审触发器全部满足；单机 `N=1` 默认保持 `0` 监听套接字、`0` 后台选举定时器、`0` TLS 证书负担（`SingleHostPassthroughNode`）。 |
| **`RISK-B-11` / `U-5` 分域退役裁决** | **`H6`** | **`SPLIT-DOMAIN RETIRED (Partial Retained)`** | **已退役半边**：跨主机 TLS 1.3 mTLS 五轴认证传输、保守时钟偏斜包络、嵌入式 Raft 控制面多数派共识、流式检查点/制品同步、跨主机八步进程迁移与 `ChaosTcpProxy` H6 故障矩阵全部实证闭合。<br>**诚实保留半边**：外部不可逆第三方 API / 远端失联物理外设的物理级副作用擦除（`QUARANTINED != 远端物理擦除`，[ADR-0019](../../../management/adrs/0019-cross-cell-commit-semantics-extension.md) R2.3 第 2 条）及跨多数据中心裸金属广域网 `1M_LOGICAL` 战役。 |
| **未决 `P0` 风险计数** | 台账复核 | **`0`** | [risks.yaml](../../../management/risks.yaml) 中 `severity: P0` 未决风险持续为 **`0`**。 |

> [!IMPORTANT]
> **阶段状态翻转纪律（`AGENTS.md` 规则 7 & [stage-c-progress.md §C.5.5](../../../management/stage-c-progress.md#c55-阶段-c-退出评审门) 条件 4）**：
> 本报告确认阶段 C（含 `W35–W67` 单机多进程域与 `W69–W72` `C-XHOST` 跨主机扩展域）的全部技术退出门、代码实现、H6 故障注入证据与 ADR 验证均已 **100% 就绪，具备呈交维护者最终签署 `EXITED` 的完整条件（Ready for Maintainer Stage C `EXITED` Sign-off）**。但在获得维护者明确批准（explicit Maintainer sign-off）之前，[stage-c-progress.md](../../../management/stage-c-progress.md) 头部阶段状态 **MUST** 严格保持 `IN_PROGRESS / NOT EXITED`，任何 Agent 严禁擅自越权将其翻转为 `EXITED`。

---

## 2. `C-XHOST` 五大收官工作包与 `ADR-0020` / `ADR-0026` 晋升审计

### 2.1 `C-XHOST` 五大工作包（`W69–W72`）逐包证据与不变量核验表

| 工作包 ID / 波次 | 权威证据工件 | 证据等级 | 核心实现 Crate 与模块 | 关键规范不变量与对抗测试核验事实 | 终审判定 |
|---|---|---|---|---|---|
| **`C-NET-TRANSPORT`**<br>（`W69-C-TRANSPORT`） | [C-XHOST-001](../c-xhost-001-mtls-transport-and-fence-handshake.md) | **`H5`** | `crates/nlos-net`<br>（`profile`, `resolver`, `identity`, `cert`, `handshake`, `frame`, `mux`, `failure`, `transport`, `poc_consensus_core_3node`） | 1. **`[NLOS-GOAL-001]` `N=1` 零开销**：`ClusterProfile::SingleHostDesktop` 默认绑定 `0` 监听端口、`0` 活动连接，越界调用立即返 `SingleHostProfileViolation`。<br>2. **`[DIST-NAME-001]` & `[TCB-ENDPOINT-001]`**：`MonotonicEndpointResolver` 将 16B `CellIdentity` 与易失 `SocketAddr` 解耦；纯 Rust Ed25519 X.509v3（`OID 1.3.6.1.4.1.59999.26.1`）+ `tokio-rustls`/`ring` TLS 1.3 mTLS（禁用 TLS 1.2 与 0-RTT）+ `EXPORTER-nlos-cell-fence-v1` 双向挑战-响应将五轴 `(CellIdentity, node_boot_generation, PrincipalId, CellEpoch, CellFencingToken)` 钉死于会话，验签前 `check_peer_fence` 纯只读防 `u64::MAX` 投毒。<br>3. **双优先级多路复用与 `[DIST-FAIL-001]`**：`ControlRpc` 优先于 `<= 64 KiB` `BulkChunk`（`64 MiB` 流在 `256 KiB` 窗口下零饥饿完成 20 次并发 ping）；精确区分 `BeforeFirstPayloadByte -> Unavailable` 与 `AfterFirstPayloadByte -> DeadlineExceededUncertain(ReconcileBeforeRetry)` 并由 `PendingReconciliationTracker` 拦截盲重试。 | **`VERIFIED`（`11/11` 测试 + `W69` 对抗审查 `F1–F4` 全闭环）** |
| **`C-NET-CLOCK-LEASE`**<br>（`W70-C-CLOCK-LEASE`） | [C-XHOST-002](../c-xhost-002-conservative-clock-skew-and-remote-lease.md) | **`H5`** | `crates/nlos-lease`<br>（`clock_horizon`, `cross_host_store`, `remote_proto`）<br>`crates/nlos-cell`<br>（`clock_liveness`） | 1. **`[MODEL-TIME-001]` 类型级禁墙钟比较**：`AuditWallTimeHint(u64)` 不实现 `Ord`/`PartialOrd`；存活探测 `CrossHostLivenessDetector` 与因果轨迹 `CrossHostCausalTrailKey` 完全基于本地单调时钟与五轴水位线（闭合 `ADR-0019` 触发器 7 第 1/2 项）。<br>2. **保守偏斜包络（`ADR-0026` §2.3）**：Holder 收缩至 $\text{TTL}_{\text{holder}} = \text{TTL}_{\text{grant}} - 2\Delta_{\text{max\_skew}} - \Delta_{\text{safety}}$ 自围栏，Grantor 延后至 $\Delta t^{\text{G}} > \text{TTL}_{\text{grant}} + \Delta_{\text{max\_skew}}$ 才扫描转 `QUARANTINED`（闭合 `ADR-0019` 触发器 7 第 3 项）；`> 15,000` 组参数穷举证明双主重叠窗口 `overlap_violations == 0`；$\lceil \text{RTT}/2 \rceil > \Delta_{\text{max\_skew}}$ 立即粘性隔离。<br>3. **`[LEASE-PROTO-001]` & `ADR-0019` R2.3 #2**：12 类 mTLS 远程租约命令支持 `SQLite WAL+FULL` 持久幂等与断连对账重放；`QuarantineDisposition` 通过 SQLite 单向触发器严格区分 `LedgerQuarantinedFenceOnly` 与带密码学回执的 `PhysicallyCleanedVerified`。 | **`VERIFIED`（`nlos-cell` `43/0`、`nlos-lease` `30/0`）** |
| **`C-CONSENSUS-CP`**<br>（`W70-C-CONSENSUS`） | [C-XHOST-003](../c-xhost-003-embedded-raft-control-plane-consensus.md) | **`H6`** | `crates/nlos-consensus`<br>（`state_machine`, `core`, `store`, `single_node`, `net_driver`）<br>`crates/nlos-takeover-control`<br>（`consensus_index_gate`） | 1. **`[CONS-SCOPE-001]` 双层四族守卫**：类型系统与线协议解码器 `decode_wire` 仅允许 `CellDirectoryUpdate`、`RootLeaseEpochAdvance`、`TaskAuthorityAssignment`、`ReconciliationAnchor` 四类低频控制面命令，高频数据面帧（`0xD0..0xD3`）在反序列化入口即以 `RejectDataPlanePayload` 拒止。<br>2. **`[TASK-SCALE-002]` & Raft 安全性**：`VoterRoster::new_multi_host` 强制奇数 `N >= 3` 且互异 `ControlDomainId`；实现 Ongaro §9.6 Pre-Vote、§5.4.2 Figure 8 当期任期提交守卫、单调 `TermBoundLeaderLease` 线性一致读与分块 `InstallSnapshotChunk`。<br>3. **`[DUR-ACK-002]` / `[DUR-SNAP-001]` / `[DUR-FAILCLOSED-001]`**：`DurableConsensusStore`（`WAL+FULL`）严格先刷盘后发包/提交，启动四重校验拒绝任何日志/快照/WAL 篡改；`SingleHostPassthroughNode`（`N=1`）保持 `0` 套接字、`0` 定时器、`0` 证书；真实子进程 `Child::kill()` 强杀 Leader 后 `2/3` 选新主并围栏旧主；`CommittedControlIndexGate` 强制校验证书签名与多数派独立控制域。 | **`VERIFIED`（`nlos-consensus` `16/0`、`nlos-takeover-control` `15/0`）** |
| **`C-NET-SYNC-MIGRATE`**<br>（`W71-C-SYNC-MIGRATE`） | [C-XHOST-004](../c-xhost-004-streaming-sync-and-cross-host-migration.md) | **`H6`** | `crates/nlos-semantic::net_sync`<br>`crates/nlos-artifact::net_transfer`<br>`crates/nlos-slice-k::{net_cell_host, net_migration_drive}` | 1. **`[SEM-CHECKPOINT-001]` & `[DIST-EVENT/AUDIT-001]`**：`SemanticNetSyncClient/Server` 跨 mTLS 交换签名向量检查点、分页拉取缺失事件并全批验真三要素溯源后在单 `Immediate` 事务原子收敛（第 2 批异常零部分写盘）。<br>2. **`[DIST-ART-001]` 可断点续传流式传输**：`ArtifactNetTransferClient/Server` 基于 `64 MiB` 窗口背压流式传输分块并逐块校验 `chunk_sha256` 落入 `.part`/`.cursor` staging；中途 TCP 断连触发 `ReconcileBeforeRetry` 拦截盲重试，`transfer_package_resumable` 精确从断点续传，对称分叉双头由 `ReplicaSyncRetentionSet` 保护免遭孤儿 GC 清理。<br>3. **`[DIST-MIGRATE-001/002]` & `[DIST-TASK-002/004]`**：零共享目录双独立子进程跨 mTLS 完成八步进程迁移、`SourceFenced` 后断网零双活与重连幂等收敛、固定会话拒绝/降级，以及 `N=3` Raft 多数派 `CommittedControlIndexGate` 驱动的 §26.1 六端点签名屏障接管与 `IPC-D7` `CellTopologyInspector`/`CellLeaseInspector` 接线。 | **`VERIFIED`（`nlos-artifact` `110/0`、`nlos-semantic` `76/0`、`nlos-slice-k` `138/0`）** |
| **`C-XHOST-GATE`**<br>（`W72-C-XHOST-GATE`） | [C-XHOST-005](../c-xhost-005-chaos-proxy-and-h6-multi-machine-gate.md) | **`H6`** | `crates/nlos-net::chaos_proxy`<br>`crates/nlos-slice-k/tests/c_xhost_h6_multi_machine_gate.rs`<br>`crates/nlos-runtime-tokio/tests/c_xhost_bench_scale.rs`<br>`scripts/xhost_cluster_harness.py` | 1. **`ChaosTcpProxy` 混沌故障原语**：支持可编程有向链路切断（含三节点三角形非对称分区 `A -> B` 断而 `A -> C -> B` 通）、确定性 Xorshift64 RTT 抖动、应用层第 $N$ 字节中段 TCP `RST`、握手后 TLS 密文位翻转（被 `rustls` AEAD 100% 拦截）及合成时钟偏斜越界控制器。<br>2. **`ROAD-C-001-XHOST` & `ROAD-C-002-XHOST` 实证**：5 项端到端跨机混沌门禁测试（含独立子 OS 进程 `CrossHostCellEndpoint` worker）+ 跨机 `1/2/4` 分片数据面 `0` Raft RPC 扩展与五故障并发矩阵 + Python 三节点拓扑/时钟包络自检脚本全部 `PASS`。 | **`VERIFIED`（现场复跑 100% 通过，见 §3.3）** |

### 2.2 `ADR-0020` 与 `ADR-0026` 晋升 `VERIFIED` 核准记录

1. **[ADR-0026（跨物理主机蜂窝网络传输、有界时钟偏斜与控制面复制基底扩展）](../../../management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md) `ACCEPTED -> VERIFIED`**：
   - §7.2 规定的四项晋升条件已全部达成：① 五个补充工作包（`C-NET-TRANSPORT`、`C-NET-CLOCK-LEASE`、`C-CONSENSUS-CP`、`C-NET-SYNC-MIGRATE`、`C-XHOST-GATE`）均已落盘 `H5/H6` 证据工件 `C-XHOST-001..005`；② `ROAD-C-001-XHOST` 与 `ROAD-C-002-XHOST` 在 `SQLite WAL+FULL` 下全绿；③ `N=1` 单机桌面零套接字、零定时器、零证书负担在 `nlos-net`、`nlos-consensus` 与 `nlos-slice-k::NetCellHost` 三层均经专门测试钉死；④ `RISK-B-09` 新增依赖（仅 `W69` 引入纯 Rust `rustls 0.23.46` / `tokio-rustls 0.26.6` / `ring 0.17.14`，关闭默认特性与 TLS 1.2/0-RTT，`W70–W72` 零新增外部依赖）已完成许可证与 MSRV `1.97` 审计。
2. **[ADR-0020（控制面单写者覆盖与共识基底引入时序）](../../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md) `CANDIDATE -> VERIFIED`**：
   - `W37` 开档确立的「先以单写者 `S-B` 覆盖单机多进程域，待单机双 Cell 证据齐备且多机控制面 HA 需求触发后再升级 `S-A`」时序纪律得到严格遵守：单机 `S-B` 覆盖期由 `W37–W67` 完整验证，跨主机 `S-B -> S-A` 升级由 `ADR-0026`、`C-XHOST-003` 与 `C-XHOST-005` 实证闭环，且 `S-B` 作为 `N=1` 单机桌面零开销直通模式永久保留，历史上下文零篡改。

---

## 3. `[ROAD-C-001]` 与 `[ROAD-C-002]` 单机 + 跨机双域退出门终审（含现场复跑记录）

### 3.1 `[ROAD-C-001]` 退出门双域审查（[C-ROAD-001](../c-road-001-partition-restart-migration-gate.md) + [C-XHOST-005](../c-xhost-005-chaos-proxy-and-h6-multi-machine-gate.md) `ROAD-C-001-XHOST`）

> **规范原文（[06-架构设计总纲-v0.5.md](../../../design/06-架构设计总纲-v0.5.md) §28.3 行 `4901`）**：「分区、重启、迁移和重复消息下有明确一致性、额度冻结和损失上界。」

| 门子句与故障维度 | 单机多进程域（`W66` [C-ROAD-001](../c-road-001-partition-restart-migration-gate.md) / `W67` 评审） | 跨物理主机网络混沌域（`W71–W72` [C-XHOST-004](../c-xhost-004-streaming-sync-and-cross-host-migration.md) & [C-XHOST-005](../c-xhost-005-chaos-proxy-and-h6-multi-machine-gate.md) `ROAD-C-001-XHOST`） | 双域合龙结论 |
|---|---|---|---|
| **1. 对称/非对称网络分区与控制面多数派围栏** | `PartitionAutonomyGate` 断网期间允许有效本地 `QuotaLease` 局部扣费并阻断 4 类跨域动作；`RemoteCallOutcome` 区分 `Unavailable` 与 `DeadlineExceededUncertain`。 | `TriangleChaosMatrix` 注入三节点有向三角形非对称分区（切断 `A -> B` 而保持 `A -> C`、`C -> B` 连通）；存活多数派 `{A, C}` 选出新任期 Leader 并推进 `consensus_commit_index`，旧 Leader `B` 被高任期与 `CellEpochFenced` 确定性围栏（`road_c001_xhost_asymmetric_triangle_partition_and_raft_quorum_fencing`）。 | **`SATISFIED`（`H6`）** |
| **2. 硬崩溃重启、子进程隔离与时钟偏斜越界冻结** | 真实子进程 `Child::kill()` 强杀后冷重启递增 `node_boot_generation (1 -> 2)`，从 `leases.sqlite3`（`WAL+FULL`）重水化，WAL 丢失严拒从 `face_value` 猜测剩余额度。 | 独立子 OS 进程既跑在 `consensus_replica_child_worker`（Leader 强杀故障转移 + 分块快照追赶，[C-XHOST-003](../c-xhost-003-embedded-raft-control-plane-consensus.md)），又跑在 `road_c001_xhost_multi_process_child_worker_chaos_gate`（经 `ChaosTcpProxy` 桥接独立子进程 `CrossHostCellEndpoint`）；注入 $|\Delta_{\text{clock}}| = 650\text{ ms} > \varepsilon_{\max} = 500\text{ ms}$ 时，`RemoteLeaseClockGuard` 立即以 `ClockSkewBoundBreached` 触发远程租约 `QUARANTINED` 与节点降级（`road_c001_xhost_clock_skew_bound_breach_and_remote_lease_quarantine`）。 | **`SATISFIED`（`H6`）** |
| **3. 跨节点八步进程迁移与六端点权威接管** | 双子进程 `CellHost` 完成八步进程迁移（透明检查点与固定会话降级）、`SourceFenced` 后崩溃零双活、以及六类 canonical endpoint 签名接管屏障。 | 零共享目录双子进程跨真实 TLS 1.3 mTLS 与 `ChaosTcpProxy` Xorshift64 RTT 抖动完成八步迁移（`road_c001_xhost_eight_step_migration_under_rtt_jitter_and_pinned_rejection` 及 [C-XHOST-004](../c-xhost-004-streaming-sync-and-cross-host-migration.md) `cross_socket_eight_step_migration_...`）；`NonMigratablePinned`（`HostLocalDevice` / `HostBoundSecret` / `ProcessBoundHandle`）在发包前 fail-closed 拒绝；`N=3` Raft 多数派 `CommittedControlIndexGate` 驱动跨机六端点接管并围栏存活旧主机 `term 1`。 | **`SATISFIED`（`H6`）** |
| **4. 重复消息、中段 `RST`/密文损坏与有界损失对账** | 签名事件集幂等合并、制品分叉双保留、一次性 `ActionToken` 重放拒止、`PartitionBoundedLossProof` 三项守恒等式 `120 + 250 + 30 == 400`（`30 <= 50`）。 | `OneShotPayloadResetRule` 在应用层第 $N$ 字节注入中段 TCP `RST` 触发 `Disconnected -> Reconnecting -> Active` 与 `advance_cell_epoch()`；`CiphertextCorruptionRule` 翻转握手后 TLS 密文位被 `rustls` AEAD 100% 拦截（`0` 伪造帧达业务层）；验证 `LossBounded { max_unflushed_entries }` 窗口内无损重放收敛与超窗口 `UnflushedLossBoundExceeded` 硬拒绝（`road_c001_xhost_mid_stream_rst_ciphertext_corrupt_and_bounded_loss_reconciliation`）。 | **`SATISFIED`（`H6`）** |

### 3.2 `[ROAD-C-002]` 退出门双域审查（[C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md) + [C-XHOST-005](../c-xhost-005-chaos-proxy-and-h6-multi-machine-gate.md) `ROAD-C-002-XHOST`）

> **规范原文（[06-架构设计总纲-v0.5.md](../../../design/06-架构设计总纲-v0.5.md) §28.3 行 `4903`）**：「海量 Agent benchmark MUST 证明 spawn/message/commit 不依赖全局锁或全局总序，并验证 stale TaskAuthority epoch、分区双主、fanout storm、provider stall 与 Cell bulkhead。」

| 基准与故障维度 | 单机多进程实盘基准（`W66` [C-BENCH-001](../c-bench-001-road-c002-distributed-benchmark.md)） | 跨主机分片与控制面隔离基准（`W72` [C-XHOST-005](../c-xhost-005-chaos-proxy-and-h6-multi-machine-gate.md) `ROAD-C-002-XHOST`） | 双域合龙结论 |
|---|---|---|---|
| **数据面无全局锁 / 无全局总序 / `0` Raft RPC 扩展**（`[CONS-SCOPE-001]`、`[PERF-BENCH-001]`） | `1 -> 2 -> 4 -> 8` 独立子 OS 进程 `Cell` 分片（全 `SQLite WAL+FULL`）吞吐从 `994.4` 扩展至 `4,417.3 ops/s`（`--release` **`4.44x`**）；8-Cell `10K_LOGICAL` 与 `100K_LOGICAL`（`634 B/node`，控制面探针 `p50 = 84–85 µs`）。 | [c_xhost_bench_scale.rs](../../../../crates/nlos-runtime-tokio/tests/c_xhost_bench_scale.rs) 在每个分片独立 `SqliteTaskAuthority`（`WAL+FULL`, `TASK_PROFILE_100K`）上实证：关键路径 `register_task -> register_attempt -> request_commit_permit -> finalize_commit` 触发 **`0` 次 Raft RPC**；`1 -> 2 -> 4` 分片吞吐从 `211.2–325.1` 线性扩展至 `843.4–1,039.6 commits/s`（`p50 = 2.5–3.9 ms`）。 | **`SATISFIED-WITH-BOUNDARIES`（`H6`）** |
| **五类并发故障负载矩阵** | 覆盖 `stale TaskAuthority epoch`（`16/16` 围栏）、`dual-primary partition`（`24` 竞争零脑裂）、`fanout storm`、`provider stall`、`Cell bulkhead`（`SafetyTcb` `6/6` + `64/64` 卸载）。 | `road_c002_xhost_five_fault_matrix_under_concurrent_load` 复现跨机并发负载下五类故障：① Shard 0 `25 ms` 慢 I/O 与 Shards 1..3 零锁争用隔离（健康分片 `p95 < 15 ms`）；② 租约到期前 `150 ms` 抢先接管报 `AuthorityLeaseHeld`、到期后 `265 ms` 旧 `term 1` 报 `AuthorityLeaseFenced`、`term 2` 零脑裂提交；③ `32` 扇出风暴确定性丢弃 `24` 超额请求且 `SafetyTcbSystemReserve` 保持 `4/4`；④ 崩溃后重复 `finalize_commit_v3` 返回 `IdempotentReplay`；⑤ `Cell 0` `Saturated (32/32)` 自动卸载至健康 `Cell 1`。 | **`SATISFIED`（`H6`）** |

### 3.3 独立终审现场验证探针实跑记录（Live Reviewer Verification Probe）

本终审员在基线 `HEAD = 5c052e53295972c2e261b0bd50de26628edfdfdc`（Windows 11 x86_64）现场执行了 `nlos-net` 单元套件与 `c_xhost_h6_multi_machine_gate` 跨机 H6 混沌门禁探针，退出码均为 `0`：

```text
$ cargo test -p nlos-net --lib; cargo test -p nlos-slice-k --test c_xhost_h6_multi_machine_gate
     Running unittests src\lib.rs (target\debug\deps\nlos_net-6c7f85574f613515.exe)
running 7 tests
test chaos_proxy::tests::seeded_prng_is_deterministic_across_runs ... ok
test resolver::tests::conditional_invalidation_protects_freshly_rotated_routing_hints ... ok
test frame::tests::mux_frame_roundtrips_all_variants_and_enforces_64kib_ceiling ... ok
test cert::tests::x509_ed25519_cert_roundtrips_and_rejects_expiry_or_tampering ... ok
test chaos_proxy::tests::chaos_proxy_nth_chunk_drop_and_latency_injection_are_deterministic ... ok
test chaos_proxy::tests::chaos_directed_mesh_enforces_triangle_asymmetric_and_one_way_partitions ... ok
test chaos_proxy::tests::chaos_proxy_mtls_before_vs_after_payload_rst_and_ciphertext_corruption ... ok
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.47s

     Running tests\c_xhost_h6_multi_machine_gate.rs (target\debug\deps\c_xhost_h6_multi_machine_gate-49c1351cf5689568.exe)
running 5 tests
test road_c001_xhost_multi_process_child_worker_chaos_gate ... ignored, spawns OS child process worker over ChaosTcpProxy; run with --ignored
test road_c001_xhost_clock_skew_bound_breach_and_remote_lease_quarantine ... ok
test road_c001_xhost_eight_step_migration_under_rtt_jitter_and_pinned_rejection ... ok
test road_c001_xhost_mid_stream_rst_ciphertext_corrupt_and_bounded_loss_reconciliation ... ok
test road_c001_xhost_asymmetric_triangle_partition_and_raft_quorum_fencing ... ok
test result: ok. 4 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 3.62s
```

---

## 4. `RISK-B-11` 与 `U-5` 分域退役裁决（Split-Domain Retirement Verdict）

遵循 `Claim <= Evidence`、`[EVID-SCOPE-001]` 与 [ADR-0019](../../../management/adrs/0019-cross-cell-commit-semantics-extension.md) R2.3 第 2 条，本终审对 [risks.yaml](../../../management/risks.yaml) `RISK-B-11` 与 [exit-unknown-risks.md](../../../management/exit-unknown-risks.md) `U-5` 作出如下**精确分域退役（Split-Domain Retirement）**裁决：

1. **正式退役半边（Retired Half — 具备 `H5/H6` 实证支撑）**：
   - **跨主机 TLS 1.3 mTLS 五轴传输与确定性失败分类（[C-XHOST-001](../c-xhost-001-mtls-transport-and-fence-handshake.md)）**：`rustls` TLS 1.3 mTLS + `EXPORTER-nlos-cell-fence-v1` 五轴对端身份钉扎、名义 `CellIdentity` 与 `SocketAddr` 解耦、双优先级 64 MiB 窗口背压，以及写出前后套接字异常到 `RemoteCallOutcome` / `PendingReconciliationTracker` 的确定性映射。
   - **跨硬件时钟域保守偏斜包络与远程三族租约协议（[C-XHOST-002](../c-xhost-002-conservative-clock-skew-and-remote-lease.md)）**：Holder 本地单调准入预算收缩 $\text{TTL}_{\text{holder}} = \text{TTL}_{\text{grant}} - 2\Delta_{\text{max\_skew}} - \Delta_{\text{safety}}$、Grantor 过期等待下界 $\Delta t^{\text{G}} > \text{TTL}_{\text{grant}} + \Delta_{\text{max\_skew}}$、`> 15,000` 组参数零双主重叠证明、接收端本地单调存活探测与 `[LEASE-PROTO-001]` mTLS 远程三族租约协议（闭合 `ADR-0019` R1.4 触发器 7 全部三项墙钟假设）。
   - **`N >= 3` 嵌入式确定性 Raft 控制面多数派共识与零数据面侵入（[C-XHOST-003](../c-xhost-003-embedded-raft-control-plane-consensus.md)）**：`crates/nlos-consensus` 四族低频控制面复制、双层 `[CONS-SCOPE-001]` 数据面零侵入守卫、`[TASK-SCALE-002]` 独立 `ControlDomainId` 校验、`SQLite WAL+FULL` 持久层、`N=1` 单机零开销退化特例与 `CommittedControlIndexGate` 多域验签门禁（闭合 [ADR-0020](../../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md) `S-B -> S-A` 升级）。
   - **流式检查点/制品同步、跨主机八步进程迁移与 `ChaosTcpProxy` H6 混沌门禁（[C-XHOST-004](../c-xhost-004-streaming-sync-and-cross-host-migration.md)、[C-XHOST-005](../c-xhost-005-chaos-proxy-and-h6-multi-machine-gate.md)）**：零共享目录跨套接字签名向量检查点增量收敛、分块可断点续传 `ArtifactTransferPackage`、双主机八步进程迁移、`N=3` 多数派六端点接管屏障，以及三角形非对称分区、中段 `RST`、密文损坏、偏斜越界与数据面 `0` Raft RPC 扩展基准。
2. **诚实保留边界（Retained Honest Boundary — `RISK-B-11` 维持 `P1 / partial`，严禁外推）**：
   - **远端失联物理外设与外部不可逆第三方 API 副作用擦除边界（`ADR-0019` R2.3 第 2 条）**：控制面推进 `epoch` 并将失联租约/权威标为 `QUARANTINED`（`QuarantineDisposition::LedgerQuarantinedFenceOnly`），仅证明权威账本与受控网关完成了围栏拒绝与账面损失封顶，**不等同于**远端断网物理主机上的孤儿外部硬件外设或已发出的不可逆第三方外部 API 副作用已物理擦除；仅当收到验签 `CleanupAttestation`、`DeviceResetReceipt` 或 `SignedSupervisorCancellationReceipt` 时方可晋升为 `PhysicallyCleanedVerified`。
   - **跨多数据中心裸金属广域网（WAN）与 `1M_LOGICAL` 超大规模集群战役**：当前 `C-XHOST` H6 混沌验证运行于单宿主机多独立 OS 子进程、隔离私有磁盘目录与真实 Loopback TCP/TLS + `ChaosTcpProxy` 确定性故障注入拓扑，尚未声称跨多数据中心裸金属广域网或 `1M_LOGICAL` 档位（联动 `RISK-B-07` / `U-1`）的生产容量数字。

---

## 5. 阶段 C 退出门六条件终审核对表（§C.5.5）与 `W73-INT` 台账合流建议

### 5.1 [stage-c-progress.md §C.5.5](../../../management/stage-c-progress.md#c55-阶段-c-退出评审门) 六项退出条件逐项终审

| 条件 # | §C.5.5 退出评审门条款 | `W73` 终审结论 | 支撑载体与收口说明 |
|---:|---|---|---|
| **1** | `ROAD-C-001` / `ROAD-C-002` 两门各有 Evidence review 记录（`H6` 维度实质性覆盖门原文全部故障类目），无一 `P0` 未决风险 | **`MET`（单机 + 跨机双域闭合）** | 单机多进程域由 [w67-stage-c-exit-gates.md](./w67-stage-c-exit-gates.md) 闭合；跨物理主机扩展域由本报告（`w73-stage-c-xhost-exit-review.md`）§2–§3 闭合；未决 `P0 = 0`。 |
| **2** | `claims` / `risks` / `evidence` 机器台账 `stage-c` 域全量回填，`Claim <= Evidence` lint 通过 | **`READY FOR W73-INT SYNC`** | 本报告 §5.2 已给出 `claims.yaml`、`risks.yaml`、`evidence-index.yaml` 的精确增量回填清单，由波次屏障唯一集成器 `W73-INT` 合并并执行 `python scripts/lint_claims.py`（`0 ERROR`）。 |
| **3** | 未知风险清单显式列出（含 `RISK-B-11` 等分域退役/重定级判定），各项有 owner 与缓解措施 | **`READY FOR W73-INT SYNC`** | 本报告 §4 已给出 `RISK-B-11` 与 `U-5` 的退役半边与保留半边精确口径，由 `W73-INT` 同步写入 [risks.yaml](../../../management/risks.yaml) 与 [exit-unknown-risks.md](../../../management/exit-unknown-risks.md)。 |
| **4** | [管理机制 §7](../../../management/README.md#7-评审与决策机制)「Stage 退出或 production claim」三件套（Evidence review + 未知风险清单 + **维护者明确批准**）完成 | **`READY FOR MAINTAINER SIGN-OFF`** | 技术实现、27 份 Stage C Evidence（22 份单机 + 5 份 `C-XHOST`）、两份综合退出评审（`W67` + `W73`）及 `ADR-0020`/`0026` `VERIFIED` 均已完备；**唯一剩余动作为维护者明确批准翻转 `EXITED`**（批准前保持 `IN_PROGRESS / NOT EXITED`）。 |
| **5** | §C.5.3 与 §C.3.3 全部车道 `DONE` / `VERIFIED`（或边界内闭合），或经批准显式移交并登记 | **`MET`** | `C-core` 九包（`W35–W67`）与 `C-XHOST` 五包（`W69–W72`）全部完成取证；阶段 D 完整桌面已在 [stage-d-progress.md](../../../management/stage-d-progress.md) 双轨并行推进至收官波次。 |
| **6** | 移交项 `#1–#17` 处置终局逐项登记（含 `RISK-B-12` 关闭、`#14` 夜间常设门、`#5` 声明前置禁令明示） | **`MET`** | 维持 [w67-stage-c-exit-gates.md §5.2](./w67-stage-c-exit-gates.md) 全部终局核定，且 `#15` 跨机网络/共识半边已随 `C-XHOST-001..005` 实证补齐，`#3` GUI 战役在阶段 D `W73-D-GUI-CAMPAIGN` 推进。 |

### 5.2 面向 `W73-INT` 的共享台账合流建议（Recommended Ledger Updates）

1. **`docs/management/evidence-index.yaml`**：确认登记 `C-XHOST-001..005` 及本评审报告 `docs/evidence/stage-c/reviews/w73-stage-c-xhost-exit-review.md`（`stage: C`, `assurance_level: H6`）。
2. **`docs/management/claims.yaml`**：将 `stage: C` 下的 `[ROAD-C-001]`、`[ROAD-C-002]`、`[TCB-ENDPOINT-001]`、`[DIST-NAME-001]`、`[DIST-FAIL-001]`、`[MODEL-TIME-001]`、`[LEASE-TTL/FENCE/LOSS/PROTO-001]`、`[CONS-SCOPE-001]`、`[TASK-SCALE-002]`、`[DUR-ACK-002]`、`[DIST-EVENT/ART/AUDIT-001]`、`[DIST-MIGRATE-001/002]`、`[DIST-TASK-001..004]` 声明追加引用 `c-xhost-001..005.md` 与 `w73-stage-c-xhost-exit-review.md`，并将其 `limitations` 从「仅限单机多进程共享目录拓扑」更新为「已覆盖单机多进程与跨主机 TLS 1.3 mTLS + 保守时钟偏斜包络 + N>=3 Raft 控制面多数派 + ChaosTcpProxy H6 混沌拓扑；保留外部不可逆物理副作用擦除边界（ADR-0019 R2.3 #2）与 1M_LOGICAL 多数据中心 WAN 战役边界」。
3. **`docs/management/risks.yaml` & `docs/management/exit-unknown-risks.md`**：更新 `RISK-B-11`（維持 `P1 / partial`）与 `U-5` 的分域描述、缓解措施及 `evidence_refs`（追加 `c-xhost-001..005.md` 与 `w73-stage-c-xhost-exit-review.md`），正式标明跨机传输/时钟偏斜/控制面 Raft 共识/流式迁移半边已退役，仅保留远端物理外设/不可逆外部副作用擦除与多数据中心裸金属 WAN 残域。
4. **`docs/management/stage-c-progress.md` & `docs/management/README.md`**：在 §C.3.3 将 `C-XHOST` 五包状态同步为 `VERIFIED` / `PASS`（引用本报告），在 §C.5.5 追加 `W73` 跨机终审就绪表，并将 `ADR-0020` 与 `ADR-0026` 索引行状态更新为 `VERIFIED`（头部阶段状态保持 `IN_PROGRESS / NOT EXITED` 呈批待签）。
