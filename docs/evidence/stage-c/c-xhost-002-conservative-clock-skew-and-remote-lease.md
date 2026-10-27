# C-NET-CLOCK-LEASE / W70-C-CLOCK-LEASE：跨硬件时钟域保守偏斜包络、单调存活窗口与基于 `nlos-net` 的远程三族租约围栏协议（ADR-0026 §2.3 & ADR-0019 R2.3 #2）

- 状态：`PARTIAL_PASS`（跨硬件时钟域保守偏斜包络、单调跨宿主存活判定器与因果审计序列、以及基于 `nlos-net` TLS 1.3 mTLS + 五轴 Exporter `CellFence` 的三族远程租约围栏协议 `[LEASE-PROTO-001]` 已全部落地并通过单机多套接字 + `SQLite WAL + FULL` 持久崩溃恢复验证；跨多物理宿主机/独立 VM 硬件分区故障注入门禁留待 `C-XHOST-GATE` 验收后晋升 `PASS`）
- Assurance Level：`H5`（持久 `SQLite WAL + FULL` 状态机 + `> 15,000` 偏斜/漂移/TTL/RTT 元组穷举包络属性证明 + 多套接字真实 TLS 1.3 mTLS 往返与提交后 TCP 断连故障注入；尚未声明跨独立硬件主机的 `H6` 门禁完成）
- 日期：2026-10-27
- Owner：`C-NET-CLOCK-LEASE`（`W70-C-CLOCK-LEASE` 车道，Attempt 1）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) `[MODEL-TIME-001]`、`[RESCROW-TIME-001]`、`[TCB-ENDPOINT-001]`、`[LEASE-TTL-001]`、`[LEASE-FENCE-001]`、`[LEASE-LOSS-001]`、`[LEASE-RENEW-001]`、`[LEASE-PROTO-001]`、`[DIST-NAME-001]`、`[DIST-FAIL-001]`
  - [ADR-0026: 跨物理机网络传输与控制平面共识扩展](../../management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md) §2.3、§3–§6
  - [ADR-0019: 跨 Cell 提交语义扩展点](../../management/adrs/0019-cross-cell-commit-semantics-extension.md) Trigger 7 #1/#2 & R2.3 #2
  - [ADR-0011: 跨进程 Principal 认证签名贯穿](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md) 与 `crates/nlos-clock` 本地单调/持久逻辑时钟权威 `AuthorityClock`
- Base HEAD：`c09c54b3b62b41c81ef55a793abe6afd0bc2ea41`

---

## 1. 跨硬件时钟域保守非重叠数学推导（`ADR-0026` §2.3）

跨物理宿主机节点不存在共享单调时钟寄存器，且严禁依赖 NTP/PTP 墙钟同步判定租约存活或围栏回收（`[MODEL-TIME-001]`、`[RESCROW-TIME-001]`）。设：

- 配置最大单向时钟偏斜/不确定度包络为 $\Delta = \text{max\_clock\_skew\_ms} \ge 1\text{ ms}$，严格正安全裕量为 $\mu = \text{safety\_margin\_ms} \ge 1\text{ ms}$；
- 名义租约时长 $L = \text{ttl\_grant\_ms}$ 强制满足严格不等式：
  $$L > 2\Delta + \mu$$
  若 $L \le 2\Delta + \mu$，Grantor 与 Holder 双端均以 `ClockHorizonError::InvalidLeaseHorizon` fail-closed 拒绝；
- 控制面往返时延测量满足 $\lceil \text{RTT} / 2 \rceil \le \Delta$（若违反则立即触发 `QuarantinedClockViolation::RttHalfExceedsMaxClockSkew` 并进入 `QUARANTINED`）；
- 记理想物理参考时间为 $T$。Holder 在**发出申请 RPC 之前**锚定自身本地单调时钟读数 $t_{\text{req},h} = M_h(T_{\text{req}})$；Grantor 在物理时刻 $T_{\text{grant}} = T_{\text{req}} + d_{\text{req}}$（其中单向请求时延 $d_{\text{req}} \ge 0$）提交租约并锚定自身本地单调时钟读数 $t_{\text{grant},g} = M_g(T_{\text{grant}})$，故物理因果上恒有 $T_{\text{req}} \le T_{\text{grant}}$。

在保守偏斜包络假设下（即租约存续期内 Holder 与 Grantor 单调时钟相对于参考物理流逝的累积偏差各自不超过包络预算，使得 Holder 端测量 $L_h = L - 2\Delta - \mu$ 所对应的最大物理存活时长满足 $T_{\text{fence},h} - T_{\text{req}} \le L - \Delta - \mu$，而 Grantor 端测量等待下界 $W_g = L + \Delta$ 所对应的最小物理等待时长满足 $T_{\text{quar},g} - T_{\text{grant}} > L$）：

1. **Holder 收缩本地存活上界**：Holder 在本地单调流逝达到 $M_h(T) - t_{\text{req},h} \ge L_h = L - 2\Delta - \mu$ 时自动转换为 `FencedByShrunkHorizon`，冻结一切新本地配额预留（`admit_local_quota_reserve`）、提交许可（`HolderCommitPermit`）与不可逆外部副作用许可（`HolderEffectPermit`）。因此 Holder 最晚自围栏物理时刻满足：
   $$T_{\text{fence},h} \le T_{\text{req}} + (L - \Delta - \mu)$$
2. **Grantor 延迟到期隔离下界**：Grantor 仅当自身单调流逝严格超过 $M_g(T) - t_{\text{grant},g} > L + \Delta$ 时才允许将失联 Holder 租约扫入 `QUARANTINED`（且即使此时也**绝不直接释放回可用池**，仍须验签关闭回执或纪元推进 + 100% 网关栅栏确认）。结合 $T_{\text{grant}} \ge T_{\text{req}}$，Grantor 最早进入到期隔离扫描的物理时刻满足：
   $$T_{\text{quar},g} > T_{\text{grant}} + L \ge T_{\text{req}} + L$$
3. **严格正物理隔离间隙**：
   $$T_{\text{quar},g} - T_{\text{fence},h} > (T_{\text{req}} + L) - (T_{\text{req}} + L - \Delta - \mu) = \Delta + \mu > 0$$

因此在任意有符号初始偏斜 $\delta_0 \in [-\Delta, +\Delta]$、包络内最坏双向反向晶振漂移（Holder 偏慢 / Grantor 偏快）与满足 $\lceil \text{RTT}/2 \rceil \le \Delta$ 的非对称网络延迟下，**Holder 必然严格先于 Grantor 到期判定时刻完成本地自围栏，双活重叠窗口恒为 `0`**。

---

## 2. 实现事实

### 2.1 保守时钟偏斜包络、三族跨宿主协调器与 `SQLite WAL + FULL` 持久扩展（`crates/nlos-lease`）

- **`crates/nlos-lease/src/clock_horizon.rs`**：
  - **强类型时域隔离**：定义节点本地单调时钟类型 `MonotonicInstantMs(u64)`、`MonotonicDurationMs(u64)` 与仅用于审计诊断的墙钟提示 `AuditWallTimeHint(u64)`；`AuditWallTimeHint` **刻意不实现 `Ord` / `PartialOrd`**，从类型系统根源杜绝跨机墙钟大小比较参与租约到期、栅栏或排序决策；
  - **本地单调 + 持久逻辑时钟守卫**：`AuthorityClockHorizonGuard` 耦合本地 `MonotonicInstantMs`、`node_boot_generation` 与 `nlos-clock::AuthorityClock`（`ADR-0011`）持久逻辑 tick 水位线，任何本地单调回退、启动代数回退或逻辑 tick 水位线回退均锁存粘性（sticky）`QuarantinedClockViolation`；
  - **Holder 端收缩预算执行器 `HolderClockBoundedLease`**：覆盖 `QuotaLease`、`CapacityLease`、`ExclusiveDeviceLease` 三族，锚定发包前 `t_req_holder_mono` 计算 `ttl_holder = ttl_grant - 2 * max_clock_skew - safety_margin`，在 `elapsed >= ttl_holder` 时自动转换为 `FencedByShrunkHorizon` 并阻断 `admit_local_quota_reserve`、`admit_commit_permit` 与 `admit_irreversible_effect_permit`；支持 `[LEASE-RENEW-001]` 安全续租重锚定 `apply_verified_renewal`（严禁隐式修改 `face_value`）与托管私钥签发 `SignedLeaseCloseReceipt`；
  - **Grantor 端协调器 `GrantorClockBoundedCoordinator` 与多端点栅栏域 `MultiEndpointFenceScope`**：
    - 强制校验 `ttl_grant > 2 * max_clock_skew + safety_margin`；
    - `evaluate_unreachable_holder_horizon` / `sweep_unreachable_horizons` 在 `elapsed_grantor_mono <= ttl_grant + max_clock_skew` 时返回 `StillWithinSkewEnvelope` 拒绝提前回收；超过下界后仅将未结清租约转入 **`QUARANTINED`**（`LedgerQuarantinedFenceOnly`），**绝不直接退回 `AVAILABLE`、`pool_remaining` 或将 `DeviceLeaseHead` 置为 `FREE`**；
    - 仅在满足以下任一排他条件时才解锁对应资源：
      - **路径 A（Holder 主动签名结清）**：验真通过绑定 Holder `PrincipalId` 的 Ed25519 `SignedLeaseCloseReceipt`（由 `IdentityPeerVerifier` 联合校验 `IdentityAuthority` 与 `SigningKeyCustodyVault` 未撤销状态，且满足 `spent + returned_unspent == face_value`）；
      - **路径 B（纪元推进 + 100% 多网关栅栏 + 家族物理清理证明）**：Cell `(epoch, fencing_token)` 已严格单调递增，且 `MultiEndpointFenceScope` 内 **100%** 必达执行网关均已提交验签 `SignedFenceBarrierAck`（部分网关确认如 `2/3` 以 `IncompleteMultiEndpointFenceBarrier` fail-closed 拒绝），并按家族绑定 `QuotaReconciliationReceipt`、`CapacityReconciliationReceipt`（含 `CleanupAttestation`）或 `DeviceReconciliationReceipt`（含队列排空 + VRAM/密钥归零擦除 + `DeviceResetReceipt`）。
- **`crates/nlos-lease/src/cross_host_store.rs`、`store.rs`、`lib.rs`**：
  - 在共享 `SqliteLeaseStore`（`WAL + FULL`, `STRICT` 表 + 防删改/单调性 SQLite 触发器）上新增 `cross_host_lease_horizons`、`cross_host_fence_barrier_acks`、`cross_host_close_receipts`、`cross_host_proto_state_seq` 与 `cross_host_proto_idempotency` 五张持久表，支持进程崩溃重启后 100% 重水化恢复包络记录、网关栅栏确认集、关闭回执与协议幂等缓存；
  - 扩展三族 Grantor（`grant_for_holder`、`QuotaLeaseGrantor::quarantine_lease`）支持绑定远端非零 `holder_cell: CellIdentity`，同时完整保留单机池守恒与 `fence_scope == holder_cell` 约束。

### 2.2 单调跨宿主存活判定器与因果审计序列（`crates/nlos-cell`）

- **`crates/nlos-cell/src/clock_liveness.rs`（及 `partition.rs`、`federation.rs`、`lib.rs` 集成）**：
  - `CrossHostLivenessDetector`：纯粹基于接收端本地 `LocalMonotonicMs` 流逝、`PeerHeartbeatLivenessSample` 五轴水位线 `(node_boot_generation, epoch, fencing_token, heartbeat_seq, authority_tick)` 与 `ceil(rtt_ms / 2) <= max_clock_skew_ms` 判定 `Healthy` / `SuspectGrace` / `PartitionedUnreachable` / `QuarantinedClockAnomaly`；远端 `AuditWallClockHint` 任意跳变对存活状态零影响；对端 `node_boot_generation` 递增须显式调用 `admit_peer_reboot_rejoin` 方可解除重启围栏；
  - `CrossHostCausalTrailKey` 与 `CausalAuditTrailEntry`：严格按 `(epoch, fencing_token, authority_tick, per_cell_seq, cell_id)` 全序排列跨宿主联邦/分区审计轨迹，彻底消除物理墙钟排序依赖（`ADR-0019` Trigger 7 #1/#2）。

### 2.3 基于 `nlos-net` 的远程三族租约协议 `[LEASE-PROTO-001]`（`crates/nlos-lease/src/remote_proto.rs`）

- 实现挂载于 `nlos_net::MtlsEndpointListener` 的 `RemoteLeaseProtocolServer`（`impl RpcRequestHandler`）与基于抽象 `Arc<dyn CellRpcTransport>`（名义 `CellIdentity` 寻址，`[DIST-NAME-001]`）的 `RemoteLeaseProtocolClient`：
  - 完整覆盖 12 类控制面命令：`GrantQuotaLease`、`RenewQuotaLease`、`ReportQuotaHighWater`、`CloseQuotaLease`、`GrantCapacityLease`、`AttachCapacityHost`、`ReturnCapacityLease`、`GrantExclusiveDeviceLease`、`SubmitDeviceResetReceipt`、`SubmitFenceBarrierAck`、`ReconcileQuarantinedLease`、`QueryLeaseReconciliationState`；
  - **端到端身份钉扎防冒充（`[TCB-ENDPOINT-001]`）**：服务端强制核验 TLS 1.3 五轴 Exporter 握手钉住的 `AuthenticatedPeerSession.peer_claims`（`cell_identity`、`node_boot_generation`、`principal_id`）与请求信封完全一致，校验域分隔 Ed25519 请求签名，钉扎 `CellIdentity <-> PrincipalId`，并在所有租约变更上校验 `caller_cell == lease_holder_cell`，100% 阻断跨 Cell 伪造信封或越权操作他人租约；
  - **确定性无 panic 二进制线协议编解码 + 持久幂等单次执行 + `[DIST-FAIL-001]` 不确定对账恢复**：每条成功变更原子递增 `durable_state_seq` 并将已签名响应持久化写入 `cross_host_proto_idempotency`（`WAL+FULL`）；相同 `(caller_cell, IdempotencyKey, command_digest)` 重放精准返回 `idempotency_replayed = true` 且零二次扣减，异参复用同一 `IdempotencyKey` 以 `IdempotencyConflict` 拒绝；针对后提交断连导致的 `RemoteCallOutcome::DeadlineExceededUncertain { retry_directive: ReconcileBeforeRetry }`，客户端 `reconcile_and_replay_uncertain` 先执行只读 `QueryLeaseReconciliationState` 探明持久提交状态，再调用 `PendingReconciliationTracker::mark_reconciled` 解锁并执行同键幂等重放完成无损收敛。

---

## 3. `ADR-0019` R2.3 #2 账本围栏与远端物理清理诚实边界

本实现通过 `QuarantineDisposition` 在内存与 `SQLite`（`cross_host_lease_horizons.disposition_code` 及单向防降级触发器 `OLD.disposition_code = 2 AND NEW.disposition_code != 2 -> ABORT`）中显式区分两类事实，绝不混淆：

1. **`QuarantineDisposition::LedgerQuarantinedFenceOnly`**：Grantor 账本已冻结失联面额/容量/设备头，且网关栅栏已拒绝旧 `(epoch, fencing_token)` 的后续请求，但**不声称**失联物理宿主机上的残留进程或硬件外设已完成物理终止；`QuotaLease` 仅凭网关栅栏对账后仍诚实保持 `LedgerQuarantinedFenceOnly`；
2. **`QuarantineDisposition::PhysicallyCleanedVerified { proof_kind, proof_digest }`**：仅当收到经密码学或结构化验真的物理清理回执时才晋升：
   - `CapacityCleanupAttestation`：`CapacityLease` 提交 `holder_processes_terminated == true` 的 `CleanupAttestation`；
   - `DeviceResetZeroization`：`ExclusiveDeviceLease` 提交包含队列排空、VRAM/密钥归零与硬件复位代数递增的 `DeviceReconciliationReceipt`；
   - `SignedSupervisorCancellation`：远端宿主机 Supervisor 签发的 Ed25519 `SignedSupervisorCancellationReceipt` 经 `IdentityPeerVerifier` 验真通过。

---

## 4. 验证证据

使用独立编译目录 `$env:CARGO_TARGET_DIR='d:\llmos\target\w70-lease'` 执行全套门禁命令：

```text
cargo fmt -p nlos-cell -p nlos-lease -- --check                                  → 退出码 0
cargo clippy -p nlos-cell -p nlos-lease -p nlos-net --all-targets -- -D warnings → 退出码 0（0 警告）
cargo test -p nlos-cell -p nlos-lease -p nlos-net --no-fail-fast                 → 退出码 0（100% 通过）
```

### 4.1 测试套件覆盖汇总

| Crate / 测试目标 | 通过数 | 覆盖要点 |
| --- | --- | --- |
| `nlos-cell`（含新增 `tests/clock_liveness.rs`） | `43 passed / 0 failed` | 单调跨宿主存活判定（`Healthy -> SuspectGrace -> PartitionedUnreachable`）、墙钟跳变免疫、`RTT/2 > max_clock_skew` / 本地单调回退 / `authority_tick` 回退隔离、重启代数围栏与 `CrossHostCausalTrailKey` 因果全序 |
| `nlos-lease` 既有回归套件（12 个测试目标） | `22 passed / 0 failed` | 单机三族租约守恒、`SQLite WAL + FULL` 持久恢复、跨纪元对账与本地 WAL 丢失隔离 100% 无回归 |
| `nlos-lease` 新增 `tests/clock_skew_horizon_properties.rs` | `4 passed / 0 failed` | **> 15,000** 组 `(max_skew, margin, ttl_grant, delta_0, drift_pair, d_req)` 包络穷举属性扫描证明 **`overlap_violations == 0`**（含每 200 组驱动完整 Grantor/Holder 状态机验证）；`ttl_grant <= 2*skew + margin` 与提前扫描拒绝；`AuditWallTimeHint` 零权限验证；`RTT/2 > skew`（含奇数向上取整 `31 -> 16 > 15`）、单调回退、`AuthorityClockHorizonGuard` 启动代数/逻辑水位线回退粘性隔离；`SQLite WAL + FULL` 三族到期转 `QUARANTINED`、两次崩溃重打开恢复、路径 A（`SignedLeaseCloseReceipt` 验签结清、幂等重放、异参拒绝、冒充拒绝）与路径 B（纪元未推进拒绝、`2/3` 网关部分覆盖拒绝、`3/3` 全覆盖解锁 + `ADR-0019` R2.3 #2 `QuarantineDisposition` 晋升） |
| `nlos-lease` 新增 `tests/remote_lease_mtls_protocol.rs` | `4 passed / 0 failed` | 真实多套接字 TLS 1.3 mTLS + 五轴 Exporter `CellFence` + 持久 `IdentityAuthority` / `SigningKeyCustodyVault`（`WAL+FULL`）：(1) 三族端到端授权、`[LEASE-RENEW-001]` 保面额续租、签名关闭回执、容量挂载归还、独占设备复位归还、以及多网关 `SubmitFenceBarrierAck` + `ReconcileQuarantinedLease`；(2) `[TCB-ENDPOINT-001]` 认证攻击节点 Cell B 伪造 `caller_cell = CellA` 与跨 Cell 越权调用三族租约 100% fail-closed 阻断；(3) `[DIST-FAIL-001]` 服务端 `SQLite` 提交后 TCP 代理切断连接触发 `DeadlineExceededUncertain(ReconcileBeforeRetry)` -> `PendingReconciliationTracker` 阻断盲重试 -> 服务端持久库重打开恢复 + `reconcile_and_replay_uncertain` 零二次扣减收敛 + 同幂等键异参 `IdempotencyConflict` 拒绝；(4) mTLS 链路 `RTT/2 > max_clock_skew` 即时隔离与 `ClusterProfile::SingleHostDesktop`（`N = 1`）峰值监听/连接数恒为 `0` |
| `nlos-net` 回归套件（单元 + `PoC-XHOST-001`） | `11 passed / 0 failed` | TLS 1.3 mTLS、五轴 Exporter 握手、`64 MiB` 流控背压、`SingleHostDesktop` 零套接字与 3 节点 `ConsensusCore` 100% 无回归 |

---

## 5. 边界与限制（`PARTIAL_PASS` 诚实域）

- **已完成边界（`Assurance: H5`）**：跨硬件时钟域保守偏斜非重叠包络数学模型、单调跨宿主存活判定与因果序列、三族远程租约围栏协议 `[LEASE-PROTO-001]`、`SQLite WAL + FULL` 崩溃恢复幂等表、以及基于真实 loopback 多套接字 TLS 1.3 mTLS 的安全负向与提交后断连对账验证均已闭环。
- **未声明完成边界（为何维持 `PARTIAL_PASS`）**：
  1. 本阶段验证运行于单物理宿主机多套接字 loopback + 受控单调时钟/TCP 代理注入环境，尚未在跨独立物理宿主机或独立内核 VM 的真实多机双网卡环境中执行 `C-XHOST-GATE`（`ROAD-C-001-XHOST`）硬件级断网、真实异构晶振长期漂移与电源级崩溃门禁；
  2. 多副本控制平面共识根租约颁发（`C-CONSENSUS-CP`）与跨机大块快照迁移/联邦 `CellHost` 生产装配接线（`C-NET-SYNC-MIGRATE`）属并行/下游车道职责，不在本工作包声称范围内。

---

## 6. 复现命令

```powershell
$env:CARGO_TARGET_DIR='d:\llmos\target\w70-lease'
cargo fmt -p nlos-cell -p nlos-lease -- --check
cargo clippy -p nlos-cell -p nlos-lease -p nlos-net --all-targets -- -D warnings
cargo test -p nlos-cell -p nlos-lease -p nlos-net --no-fail-fast
```
