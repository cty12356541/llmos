# C-CONSENSUS-CP / W70-C-CONSENSUS：`nlos-consensus` 嵌入式确定性 Raft 控制面多数派日志、`SQLite WAL + FULL` 持久层、`N = 1` 零配置直通模式与控制面权威索引门禁（ADR-0026 §2.1/§3.2/§6.1/§6.3 & ADR-0019 R2.3 #3）

- 状态（Verdict）：`PARTIAL_PASS`（`crates/nlos-consensus` 嵌入式确定性 Raft 控制面多数派复制引擎、`SQLite WAL + FULL` 持久日志与快照恢复、`N = 1` 零套接字/零定时器/零证书直通模式、真实 `nlos-net` TLS 1.3 mTLS + 五轴 Exporter 栅栏复制驱动、真实 OS 子进程 `Child::kill()` 硬崩溃故障转移与分块 `InstallSnapshot` 追赶、以及 `crates/nlos-takeover-control` 已提交权威索引门禁 `CommittedControlIndexGate` 已全部落地并通过验证，根工作区成员已由 `W70-INT` 合入；`CellHost` 生产联邦编排接线与跨机 `ChaosTcpProxy` 混沌门禁留待 `W71/W72`（`C-NET-SYNC-MIGRATE` / `C-XHOST-GATE`）验收后晋升 `PASS`）
- Assurance Level：`H6`（纯无 I/O 确定性 3/5 节点分区/乱序/丢包/Figure 8 模拟 + `SQLite WAL + FULL` 持久层磁盘损坏/不完整快照注入 fail-closed 验证 + 真实 TLS 1.3 mTLS 多端口套接字与真实 OS 进程 `std::process::Child::kill()` 硬杀故障转移、恢复追赶及旧任期围栏验证；尚未声称跨独立物理宿主机/独立 VM 网段的 `C-XHOST-GATE` 完成）
- 日期：2026-10-27
- Owner：`C-CONSENSUS-CP`（`W70-C-CONSENSUS` 车道，Attempt 1）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §1.1 `[NLOS-GOAL-001]`、§6.1 `[TCB-ENDPOINT-001]`、§15.2 `[DUR-ACK-002]` / `[DUR-SNAP-001]` / `[DUR-FAILCLOSED-001]`、§25.1 `[TASK-SCALE-002]`、§26.1 `[DIST-NAME-001]` / `[DIST-FAIL-001]`、§26.2 `[CONS-SCOPE-001]`
  - [ADR-0026: 跨物理机网络传输与控制平面共识扩展](../../management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md) §2.1、§3.2、§6.1、§6.3
  - [ADR-0020: 控制面单写者与共识基底时序](../../management/adrs/0020-control-plane-single-writer-and-consensus-timing.md) `S-B -> S-A` 升级路径
  - [ADR-0019: 跨 Cell 提交语义扩展点](../../management/adrs/0019-cross-cell-commit-semantics-extension.md) Trigger 7 #4 & R2.3 #3
  - [ADR-0011: 跨进程 Principal 认证签名贯穿](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md) 与 `crates/nlos-clock` 本地单调时钟权威 `AuthorityClock`
- Base HEAD：`c09c54b3b62b41c81ef55a793abe6afd0bc2ea41`

---

## 1. 实现事实

### 1.1 新增独立控制平面共识 crate `crates/nlos-consensus`

`crates/nlos-consensus` 遵守 `edition = "2024"`、`rust-version = "1.97"`、`unsafe_code = "forbid"`、`clippy::all = "warn"`、`clippy::pedantic = "warn"`，完全基于纯 Rust 自研确定性状态机与工作区既有依赖构建：

1. **`[CONS-SCOPE-001]` 四族控制平面结构化边界与双层拒绝守卫（`crates/nlos-consensus/src/state_machine.rs`）**
   - `ControlPlaneCommand` 在类型系统与线协议解码器双层严格锁定仅允许 4 类低频控制平面元数据进入共识状态机：
     1. `CellDirectoryUpdate`（标签 `0x01`，单调 `directory_revision`）
     2. `RootLeaseEpochAdvance`（标签 `0x02`，覆盖 `TokenPool` / `DeviceSlot` / `BackgroundQuota` 三族根纪元单调推进）
     3. `TaskAuthorityAssignment`（标签 `0x03`，单调 `authority_epoch` 绑定 `CellIdentity` 与 `PrincipalId`）
     4. `ReconciliationAnchor`（标签 `0x04`，单调 `anchor_seq` + 32B `anchor_digest`）
   - 线协议解码器 `ControlPlaneProposal::decode_wire` 在反序列化入口即拦截高频数据面帧标签（`TopicMessagePublish = 0xD0`、`BulkCheckpointChunk = 0xD1`、`HighFrequencyLeaseTick = 0xD2`、`SearchOrNotifyHint = 0xD3`）及一切未知标签，确定性返回 `ScopeViolationError::RejectDataPlanePayload(ForbiddenDataPlaneKind)`，绝不推进 `consensus_commit_index` 或污染持久日志。
   - `ControlPlaneStateMachine` 维护幂等去重环（按 `IdempotencyKey` 拒绝冲突负载重放、放行逐字节幂等重试）、执行四族单调世代推进，并产出跨节点逐字节确定性的快照字节流与 RFC 6234 `SHA-256` `state_root_digest`。

2. **纯无 I/O 确定性 Raft 多数派复制引擎与 `[TASK-SCALE-002]` 独立控制域投票名册（`crates/nlos-consensus/src/core.rs`）**
   - `VoterRoster::new_multi_host` 强制 `N >= 3` 且奇数规模（最大 `MAX_VOTER_ROSTER_SIZE = 7`），并强制每个投票节点同时具备唯一的 `CellIdentity`、`PrincipalId` 与**互不相同的 `ControlDomainId`**；任何共享 `ControlDomainId` 的同宿主多进程或同控制域子视图一律以 `VoterRosterError::NonIndependentControlDomain` fail-closed 拒绝，杜绝伪多数派（`[TASK-SCALE-002]`）。
   - `ConsensusCore` 完全不触碰套接字、系统墙钟或磁盘，仅通过 `on_tick()`、入站 `RaftMessage` 与本地提案驱动并产出显式 `ConsensusEffect` 序列：
     - **Ongaro §9.6 Pre-Vote 预投票**：隔离少数派节点仅递增候选试探任期发起 `RequestPreVote`，未获存活多数派独立 `ControlDomainId` 预投票前绝不递增持久 `current_term`，网络愈合后零任期膨胀、零 Leader 抖动；
     - **Raft §5.4.2 Figure 8 提交安全性**：`try_advance_leader_commit` 仅当候选 `LogIndex` 的条目任期严格等于 `self.current_term` 且已在 `>= floor(N/2) + 1` 个不同 `ControlDomainId`（含 Leader 本地已完成 `WAL + FULL` 刷盘的自身域）确认复制后才推进 `commit_index`；旧任期遗留条目仅随当前任期条目达成多数派而间接提交；
     - **单调时间有界 Leader 租约与线性一致 `ReadIndex`**：`TermBoundLeaderLease` 完全由单调 `on_tick()` 递减（零 NTP/墙钟依赖，`[ADR-0011]`），每次收到存活多数派独立 `ControlDomainId` 对当前心跳轮次的确认后刷新租约并唤醒挂起的线性一致读请求；遇到更高 `term` 时立即在 `step_down_to_higher_term` 中清零租约并以 `ProposalRejectReason::StaleLeaderTerm` fail-closed 拒绝旧任期写提案与挂起读请求；
     - **幂等乱序容忍的分块 `InstallSnapshot` 状态转移**：Leader 对落后于已压缩前缀（`next_index <= snapshot.included_index`）的 Follower 按有界 `max_snapshot_chunk_bytes` 发送带全量 `SHA-256` 摘要的 `InstallSnapshotChunk`；Follower Staging 对周期性心跳重传或乱序分块保持已缓冲连续前缀幂等（仅在无匹配 Staging 且 `offset > 0` 或摘要/长度校验失败时重置回 `0`），Leader 侧单调推进 `snapshot_send_offset` 避免迟到 ACK 回退游标。

3. **`SQLite WAL + FULL` 持久层、原子快照安装与 `[DUR-FAILCLOSED-001]` 启动门禁（`crates/nlos-consensus/src/store.rs`）**
   - `DurableConsensusStore::open` 强制设置 `PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;`（`[DUR-ACK-002]`），并安装数据库触发器 `trg_consensus_log_no_update` 禁止对已写入日志行做原位篡改。
   - 每条持久化日志与快照均写入域分隔 RFC 6234 `SHA-256` 完整性校验和（`entry_sha256` / `snapshot_sha256`）。启动恢复执行四重 fail-closed 校验：SQLite `PRAGMA quick_check`、不完整快照事务标记 `snapshot_install_in_progress != 0` 检测、快照 `SHA-256` 与状态机反序列化 `state_root_digest` 对账、以及紧接 `included_index` 的无间隙连续日志索引与逐条 `entry_sha256` 校验；任何损坏、截断或半写标记均返回 `ConsensusStoreError::FailClosedCorruption`，严禁静默跳过或截断恢复（`[DUR-FAILCLOSED-001]`）。
   - `[DUR-SNAP-001]` 快照与截断解耦：`persist_snapshot_only` 先独立持久化完整快照及 `SHA-256`，仅在快照持久落盘验证后才允许 `compact_log_up_to(index <= min(snapshot.included_index, commit_index))` 裁剪旧日志前缀；Follower 接收远端快照通过单 `IMMEDIATE` 事务 `install_remote_snapshot_and_compact` 原子替换快照并按 Raft §7 规则保留或清空后缀。

4. **`N = 1` 单机桌面零配置直通模式（`crates/nlos-consensus/src/single_node.rs`，`ADR-0026` §6.3 & `[NLOS-GOAL-001]`）**
   - `SingleHostPassthroughNode` 与统一门面 `ConsensusNode::open_single_host_passthrough` 专为默认 `ClusterProfile::SingleHostDesktop`（`N = 1`）设计：
     - `active_socket_count() == 0`（绑定 **0** 个 TCP/UDP 端口）；
     - `active_background_timer_count() == 0`（启动 **0** 个选举/心跳后台定时器协程）；
     - `requires_tls_certificates() == false`（不要求签发或配置任何 mTLS 节点证书）。
   - 控制面提案经同一 `[CONS-SCOPE-001]` 守卫校验后直接进入本地 `DurableConsensusStore`（`SQLite WAL + FULL`）单事务原子落盘并立即推进 `consensus_commit_index` 产出 `ControlCommitReceipt`。

5. **基于 `nlos-net` TLS 1.3 mTLS + 五轴 Exporter `CellFence` 的多宿主复制驱动（`crates/nlos-consensus/src/net_driver.rs`）**
   - `MultiHostQuorumNode` 复用 `nlos-net::MtlsCellTransport`、`EndpointResolver`、`IdentityAuthority`、`CustodyPeerSigner` 与 `PeerFencePolicy`，所有 Raft RPC（`RequestPreVote`、`RequestVote`、`AppendEntries`、`InstallSnapshotChunk`）均跑在高优先级 `ControlRpc` 通道上并仅按名义 `CellIdentity` 寻址（绝不暴露裸 `SocketAddr`，`[DIST-NAME-001]`）。
   - 入站调度器联合校验 `AuthenticatedPeerSession.peer_fence.cell_identity` 与 `VoterRoster` 钉扎的 `PrincipalId` 及对端证书 `ControlDomainId`，任何冒充或跨身份重绑在触达 `ConsensusCore` 之前即被静默丢弃拒绝。
   - **严格先刷盘后应答/提交顺序（`[DUR-ACK-002]`）**：`execute_effects` 严格在向网络派发任何 `RequestVoteResponse`、`AppendEntriesResponse` 或推进状态机 `CommitAdvance` 之前，先在本地 `DurableConsensusStore` 完成 `PersistHardState` / `TruncateLogSuffixFrom` / `AppendLogEntries` / `InstallSnapshot` 的 `WAL + FULL` 持久提交；Leader 本地提案同样在完成本地 `WAL + FULL` 落盘后才计入自身 `ControlDomainId` 票数。

### 1.2 控制平面已提交权威索引门禁（`crates/nlos-takeover-control/src/consensus_index_gate.rs`）

在不修改 `crates/nlos-takeover-control/Cargo.toml`（保持零跨车道工作区清单冲突）的前提下，于 `nlos-takeover-control` 新增并导出 `consensus_index_gate` 模块：

- 定义跨模式权威门禁策略 `ConsensusBackingMode`：
  - `SingleHostPassthrough { local_cell, local_principal, local_control_domain }`（对应 `S-B` 单机 `N = 1` 直通模式）；
  - `MultiHostQuorum { voters: BTreeMap<CellIdentity, AuthorizedConsensusVoter> }`（对应 `S-A` 跨机 `N >= 3` 奇数独立 `ControlDomainId` 多数派模式，构造时即强制校验 `[TASK-SCALE-002]` 互异控制域不变量）。
- `CommittedControlIndexGate` 对每次控制面权威演进或跨机 `TaskAuthorityTakeoverReceipt` 执行四重门禁裁决：
  1. 严格单调递增校验：`commit_index > last_applied_commit_index` 且 `term >= last_committed_term`（阻断回滚或重放旧 `consensus_commit_index`）；
  2. 接管回执精确绑定校验：`verify_and_advance_takeover_receipt` 强制 `receipt.consensus_commit_index == Some(attestation.subject.commit_index)` 且 `subject.payload_digest` 与回执规范摘要逐字节一致（缺失或错配一律以 `MissingReceiptCommitIndex` / `ReceiptCommitIndexMismatch` 拒绝）；
  3. 实时 `IdentityAuthority` 密码学验签与密钥撤销阻断：逐一调用 `IdentityAuthority::verify_semantic_authority_signature` 校验投票节点的域分隔 Ed25519 签名（前缀 `"nlos/consensus-commit-attestation/v1"`）；已轮换失效或被撤销的签名密钥立即以 `VoterSignatureRejected` 拒绝，不计入有效票；
  4. `[TASK-SCALE-002]` 独立控制域多数派计数：在 `MultiHostQuorum` 模式下按通过验签的投票节点所属 `ControlDomainId` 去重计数，仅当不同控制域数 `>= floor(N/2) + 1` 时放行推进权威水位线，否则返回 `InsufficientDistinctDomainQuorum`。

---

## 2. `RISK-B-09` 供应链与依赖审计

本次工作包**未引入任何新的外部第三方 crate**。`crates/nlos-consensus` 仅复用仓库既有内部 crate（`nlos-capability`、`nlos-cell`、`nlos-clock`、`nlos-identity`、`nlos-net`、`nlos-types`）及工作区已审计基础依赖（`rusqlite` `bundled`、`tokio`、`rustls`/`tokio-rustls` `ring`、`ed25519-dalek`、`sha2`）：

| 依赖项 | 来源 / 版本策略 | 新增外部供应链面 | 用途 |
| --- | --- | --- | --- |
| `nlos-capability` / `nlos-cell` / `nlos-clock` / `nlos-identity` / `nlos-net` / `nlos-types` | 仓库内部路径依赖 | **0 新增** | 四轴/五轴身份、托管签名、本地单调时钟、TLS 1.3 mTLS + Exporter `ControlRpc` |
| `rusqlite` (`0.40`, `bundled`) | 工作区既有基线依赖（`MIT`） | **0 新增** | `SQLite WAL + FULL` 共识日志、硬状态与快照持久化 |
| `tokio` (`1.48`) / `rustls` (`0.23`, `ring`) / `tokio-rustls` (`0.26`, `ring`) | 工作区既有基线依赖（`MIT` / `Apache-2.0`） | **0 新增** | 仅在 `MultiHostQuorumNode` 模式下驱动异步 mTLS 复制循环 |
| `ed25519-dalek` (`3.0.0`) / `sha2` (`0.11.0`) 及 `nlos-takeover-control` 内置 RFC 6234 `SHA-256` | 工作区既有基线依赖 + 纯安全 Rust 实现 | **0 新增** | 域分隔投票签名、日志条目、快照传输与权威提交摘要校验（`unsafe_code = "forbid"`） |

---

## 3. 验证与证据矩阵

全部验证命令均在隔离构建目录 `$env:CARGO_TARGET_DIR='d:\llmos\target\w70-consensus'` 下执行并通过：

```text
1. cargo fmt --manifest-path crates/nlos-consensus/Cargo.toml --all -- --check              → 退出码 0
2. cargo clippy --manifest-path crates/nlos-consensus/Cargo.toml --all-targets -- -D warnings → 退出码 0（0 警告）
3. cargo test --manifest-path crates/nlos-consensus/Cargo.toml --no-fail-fast               → 退出码 0（2 unit + 8 sim + 4 durable = 14 passed / 0 failed / 2 ignored）
4. cargo test --manifest-path crates/nlos-consensus/Cargo.toml --no-fail-fast -- --ignored  → 退出码 0（2 passed / 0 failed）
5. cargo fmt -p nlos-takeover-control -- --check                                            → 退出码 0
6. cargo clippy -p nlos-takeover-control --all-features --all-targets -- -D warnings        → 退出码 0（0 警告）
7. cargo test -p nlos-takeover-control --all-features --no-fail-fast                        → 退出码 0（1 unit + 1 win_ipc + 4 consensus_gate + 9 takeover = 15 passed / 0 failed）
```

### 3.1 `nlos-consensus` 单元测试（`src/state_machine.rs`，2 passed / 0 failed）

1. `state_machine::tests::all_four_control_plane_families_roundtrip_and_snapshot_deterministically`：验证全部 4 类控制平面命令线协议编解码往返、状态机应用、快照序列化恢复与 `state_root_digest` 逐字节一致性。
2. `state_machine::tests::wire_guard_rejects_all_forbidden_data_plane_families`：验证全部 4 类高频数据面负载在线协议边界被 `RejectDataPlanePayload` 确定性拦截。

### 3.2 `nlos-consensus` 确定性 Raft 故障模拟套件（`tests/deterministic_raft_simulation.rs`，8 passed / 0 failed）

1. `three_and_five_node_quorums_converge_across_all_four_families_under_reorder_and_drop`：在存在报文乱序与周期性丢包的内存确定性网络中验证 `N = 3` 与 `N = 5` 集群选主、跨全部 4 类控制平面命令推进 `consensus_commit_index`，并达成全集群 `state_root_digest` 逐字节一致。
2. `isolated_minority_uses_pre_vote_without_inflating_term_or_disrupting_active_leader`：验证非对称/对称少数派隔离节点反复超时仅停留在 `PreCandidate` 阶段，重连后不膨胀 `current_term`、不打断存活多数派 Leader 租约与提交。
3. `deposed_leader_steps_down_on_heal_and_rejects_stale_proposals_and_reads`：验证旧 Leader 被分区隔离后单调 `TermBoundLeaderLease` 超时自动阻断本地陈旧读，分区愈合收到更高任期消息后立即降级并以 `StaleLeaderTerm` 围栏拒绝旧任期提案与挂起线性一致读。
4. `conflicting_uncommitted_tail_truncates_and_overwrites_without_touching_committed_prefix`：验证已提交前缀不可回退，旧 Leader 隔离期间写入的未提交分叉后缀在重连新任期 Leader 后被精确截断并收敛至权威日志。
5. `figure_8_prior_term_entry_is_not_committed_until_current_term_entry_reaches_quorum`：在 5 节点集群复现 Raft §5.4.2 Figure 8 经典场景，验证新任期 Leader 绝不因旧任期条目已复制至多数节点而直接提交该旧条目，直至当前任期新条目亦达成多数派复制后才一并提交。
6. `chunked_install_snapshot_catches_up_lagging_follower_after_leader_compaction`：验证 Leader 执行快照与日志前缀裁剪后，长期失联 Follower 通过多块（`max_snapshot_chunk_bytes = 128`）`InstallSnapshotChunk` 完成 `SHA-256` 验真安装与后续增量日志追赶。
7. `cons_scope_001_wire_guard_rejects_all_forbidden_data_plane_payloads`：验证 Leader 对全部禁止进入共识的数据面帧返回 `RejectDataPlanePayload` 且 `commit_index == 0`。
8. `task_scale_002_voter_roster_rejects_shared_control_domains_and_invalid_sizes`：验证偶数节点（`N = 2, 4`）、超界节点及共享同一 `ControlDomainId` 的投票名册在构造期即以 `NonIndependentControlDomain` fail-closed 拒绝。

### 3.3 `nlos-consensus` 持久层、`N = 1` 直通与真实 OS 进程硬杀 mTLS 套件（`tests/durable_quorum_and_crash_recovery.rs`，默认 4 passed + `--ignored` 2 passed = 6 passed / 0 failed）

1. `single_host_passthrough_zero_overhead_and_restart_durability`：验证 `SingleHostPassthroughNode`（`N = 1`）满足 `active_socket_count() == 0`、`active_background_timer_count() == 0`、`!requires_tls_certificates()`，拦截数据面帧，完成 4 族本地 `WAL + FULL` 提交、`[DUR-SNAP-001]` 先快照后裁剪与重启恢复 `state_root_digest` 一致性。
2. `dur_failclosed_001_corrupted_log_wal_and_snapshot_refuse_startup_without_silent_truncation`：分别注入（2a）`consensus_log` 负载字节篡改破坏 `entry_sha256`、（2b）`snapshot_install_in_progress = 1` 半写快照崩溃标记与 `snapshot_bytes` 篡改、（2c）底层 `.db-wal` 文件头损坏，验证 `DurableConsensusStore::open` 100% 以 `FailClosedCorruption` 拒绝启动，零静默截断。
3. `dur_snap_001_mtls_snapshot_compaction_catches_up_offline_follower`：在真实 3 节点 TLS 1.3 mTLS 集群上隔离 Follower 3、在 2/3 多数派提交 8 条目并执行快照压缩裁剪日志前缀后重连 Follower 3，验证跨真实 mTLS 连接的多分块（`192 B/chunk`）`InstallSnapshot` 幂等流式传输与 `state_root_digest` 收敛。
4. `three_node_mtls_quorum_survives_leader_hard_kill_and_fences_rejoined_old_leader`：将初始 Leader 运行于独立真实 OS 子进程（`consensus_replica_child_worker`），完成跨 mTLS 3 节点四族提交后调用真实 **`std::process::Child::kill()`** 硬杀 Leader 进程；验证存活 2/3 节点自动选出更高任期新 Leader 并继续提交新控制面条目，随后在 `node_boot_generation = 2` 重启原 Leader 子进程，验证其从 `SQLite WAL + FULL` 恢复并追赶至最新提交高度，且对其注入旧任期写请求被 `StaleLeaderTerm` 确定性围栏拒绝。
5. `ignored_leader_hard_kill_with_compacted_snapshot_rejoin_soak`（`-- --ignored`）：在真实 `Child::kill()` 硬杀原 Leader 后，由存活 2/3 新 Leader 提交新条目并压缩裁剪日志前缀，再重启原 Leader 子进程，端到端联合验证硬崩溃恢复 + 跨进程 mTLS 多分块（`192 B/chunk`）`InstallSnapshot` 状态同步 + 旧任期围栏拒绝。
6. `consensus_replica_child_worker`（`-- --ignored`）：子进程副本工作入口；无 `NLOS_CONSENSUS_CHILD_WORKER` 环境变量时安全零副作用退出。

### 3.4 `nlos-takeover-control` 已提交控制索引门禁测试（`src/consensus_index_gate.rs` + `tests/consensus_index_gate.rs`，1 unit + 4 integration + 10 existing = 15 passed / 0 failed）

1. `consensus_index_gate::tests::rfc6234_sha256_vectors_match`：校验内置纯 Rust RFC 6234 `SHA-256` 标准测试向量。
2. `single_host_passthrough_mode_verifies_local_commit_and_rejects_foreign_principal`：验证 `S-B` 单机直通模式验签通过推进索引，并拒绝异主 `PrincipalId` 伪造提交证明。
3. `multi_host_quorum_enforces_majority_distinct_domains_and_monotonic_commit_index`：验证共享 `ControlDomainId` 名册被拒绝、单节点（`1/3`）票数不足以 `InsufficientDistinctDomainQuorum` 拒绝、`2/3` 独立控制域验签通过推进水位线，以及回退/重复 `commit_index` 被 `StaleOrDuplicateCommitIndex` 阻断。
4. `multi_host_quorum_enforces_takeover_commit_index_monotonicity_and_key_revocation`：验证 `TaskAuthorityTakeoverReceipt.consensus_commit_index` 缺失、错配或回退全部 fail-closed 拒绝，仅在匹配更高已提交多数派证明时放行接管权威演进。
5. `multi_host_quorum_rejects_revoked_voter_key_via_identity_authority`：通过 `IdentityAuthority::revoke_key` 撤销其中一名投票节点签名密钥后，原 `2/3` 证明因有效独立域跌至 `1/3` 被 `VoterSignatureRejected` / `InsufficientDistinctDomainQuorum` 立即阻断，直至其余健康第 3 域节点补齐 `2/3` 多数派后恢复推进。

---

## 4. 边界与限制（诚实域，`Claim <= Evidence`）

1. **工作区合入记录**：`W70-C-CONSENSUS` 车道实施期间使用独立 `[workspace]` 验证，并已由波屏障单一集成器 `W70-INT` 移除临时 `[workspace]` 表、合入根 `Cargo.toml` 工作区成员列表与全仓门禁矩阵。
2. **下游生产编排与跨物理机混沌门禁边界**：本工作包交付 `crates/nlos-consensus` 核心复制/持久/mTLS 驱动与 `crates/nlos-takeover-control` 已提交索引验真门禁。将 `ConsensusNode` 接入 `CellHost` 联邦主管循环及跨机快照流式迁移，属于下游 `C-NET-SYNC-MIGRATE`（`W71`）职责；跨独立宿主机/VM 网络与 `ChaosTcpProxy` 对称/非对称分区混沌门禁属于 `C-XHOST-GATE`（`W72`）职责，故本证据诚实标记为 `PARTIAL_PASS`。

---

## 5. 复现命令

```powershell
cargo fmt -p nlos-consensus -p nlos-takeover-control -- --check
cargo clippy -p nlos-consensus -p nlos-takeover-control --all-targets -- -D warnings
cargo test -p nlos-consensus --no-fail-fast
cargo test -p nlos-consensus --no-fail-fast -- --ignored
cargo test -p nlos-takeover-control --all-features --no-fail-fast
```
