# C-NET-TRANSPORT / W69-C-TRANSPORT：`nlos-net` 跨宿主 TLS 1.3 mTLS 传输层、五轴 TLS Exporter 栅栏互认握手、双优先级多路复用背压、确定性套接字失败分类与 3 节点 `ConsensusCore` PoC 验证（PoC-XHOST-001 & ADR-0026）

- 状态：`PASS`（`PoC-XHOST-001` 门禁套件与 `W69-C-XHOST-REVIEW` 独立对抗审计加固全部通过：`ClusterProfile::SingleHostDesktop` `N = 1` 零套接字默认保证、`EndpointResolver` 单调路由水位线 + `CellIdentity <-> PrincipalId` 绑定与名义 `CellIdentity` 解耦、纯 Rust Ed25519 X.509v3 + `tokio-rustls`/`ring` TLS 1.3 mTLS + `EXPORTER-nlos-cell-fence-v1` 五轴挑战-响应互认握手与签名前零状态毒化门禁、4B 大端长度前缀 `ControlRpc`/`BulkChunk <= 64 KiB` 双优先级多路复用与 `64 MiB` 信用窗口背压、`BeforeFirstPayloadByte`/`AfterFirstPayloadByte` 确定性 `RemoteCallOutcome` 映射与盲重试对账栅栏、以及纯无 I/O 3 节点 `ConsensusCore` 状态机种子全部落地并通过验证）
- 日期：2026-10-27
- Owner：`C-NET-TRANSPORT`（`W69-C-TRANSPORT` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §1.1 `[NLOS-GOAL-001]` + §6.1 `[TCB-ENDPOINT-001]` + §25.1 `[TASK-SCALE-002]` + §26.1 `[DIST-NAME-001]` & `[DIST-FAIL-001]` + [ADR-0026: 跨物理机网络传输与控制平面共识扩展](../../management/adrs/0026-cross-physical-host-transport-and-consensus-expansion.md) §2.1–§2.4、§3.1–§3.2、§6.2–§6.3、§7.1（`PoC-XHOST-001`）
- Base HEAD：`a8bbfa219d37a2e7032a57e390be82533c6fdd5a`

## 1. 实现事实

新增独立 crate `crates/nlos-net`（`edition = "2024"`, `rust-version = "1.97"`, `unsafe_code = "forbid"`, `clippy::all = "warn"`, `clippy::pedantic = "warn"`）：

1. **传输中立特征、有界套接字生命周期核算与 `N = 1` 单机桌面零网络开销保证（`ADR-0026` §3.1, §6.3, `[NLOS-GOAL-001]`, `[DIST-NAME-001]`）**
   - `crates/nlos-net/src/profile.rs`：`ClusterProfile` 默认值为 `ClusterProfile::SingleHostDesktop`（`N = 1`），绑定 **0** 个 TCP/UDP 监听端口、建立 **0** 条跨机连接、不初始化任何 TLS 状态，任何跨宿主监听或拨号调用均在触碰 OS 套接字之前以 `SingleHostProfileViolation` fail-closed 拒绝；`SocketActivityAccounting` 的 `ConnectionSocketGuard` 仅由底层活跃读写 I/O 协程持有（不滞留于外部 `MultiplexedSession` 克隆句柄），配合 `watch` 关闭广播确保对端断开或显式关闭时双端 `active_connection_count()` 立即归零。
   - `crates/nlos-net/src/resolver.rs`：`EndpointResolver` 与 `MonotonicEndpointResolver` 严格将 16B 不透明名义标识 `CellIdentity` 与易失性 `SocketAddr` 路由提示解耦；所有更新受单调水位线 `(node_boot_generation, epoch, heartbeat_seq)` 与可选 `expected_principal: Option<PrincipalId>` 钉扎保护，乱序/回放旧版本、同版本异址冲突或篡改已钉扎 `PrincipalId` 一律以 `EndpointStaleError` fail-closed 拒绝，`invalidate_if_version_at_most` 防止慢速故障探测器误删已完成 DHCP/端口重绑的新路由记录。
   - `crates/nlos-net/src/transport.rs`：`CellRpcTransport` 与 `BulkStreamTransport` 仅接受名义 `CellIdentity`（绝不直接接受 `SocketAddr`），由 `MtlsCellTransport` 内部查询 `EndpointResolver` 并在路由提示旋转时自动关闭旧会话并切换至新 TLS 1.3 会话；监听器拒绝审计与已接受会话表分别由 `MAX_REJECTED_HANDSHAKE_AUDIT_ENTRIES = 256`（辅以不回绕的 `total_rejected_handshakes()` 单调总计数）与 `MAX_RETAINED_ACCEPTED_SESSIONS = 256` 严格界定内存上界。

2. **零私钥导出的 `IdentityAuthority` + `SigningKeyCustodyVault` 锚定与 TLS 1.3 mTLS + 五轴 TLS Exporter 互认握手（`ADR-0026` §2.2, §6.2, `[TCB-ENDPOINT-001]`）**
   - `crates/nlos-net/src/identity.rs`：`CustodyPeerSigner` 直接调用 `SigningKeyCustodyVault::sign_digest_under_custody` 在托管库内部完成 RFC 8032 Ed25519 摘要签名并持久化不可变 `CustodySignAuditReceipt`，节点长期私钥种子永不离开托管边界；`AuthorityCustodyPeerVerifier` 联合校验 `IdentityAuthority` 当前绑定与 `SigningKeyCustodyVault` 泄露墓碑（`CompromisedRevoked`）。
   - `crates/nlos-net/src/cert.rs`：纯 Rust 严格规范 ASN.1 DER 编解码生成与验真带 `NLOSCERT` 托管签名扩展（`OID 1.3.6.1.4.1.59999.26.1`）的每启动周期临时 Ed25519 X.509v3 节点证书（零 OpenSSL、零外部 ASN.1 解析器依赖，按 `RFC 5280` §4.2 对重复 `NLOSCERT` 扩展 fail-closed 拒绝）；`MtlsNodeCertVerifier` 强制仅允许 `rustls::version::TLS13`（TLS 1.2 签名路径无条件 fail-closed）且禁用 `0-RTT` early data（`max_early_data_size = 0`, `enable_early_data = false`）。
   - `crates/nlos-net/src/handshake.rs`：TLS 1.3 握手完成后、任何业务 `ControlRpc`/`BulkChunk` 帧收发前，双端通过 `ConnectionCommon::export_keying_material` 提取 32 字节通道绑定材料（标签 `"EXPORTER-nlos-cell-fence-v1"`，上下文 `"llmos/cell-mtls-exporter-context/v1"`），服务端挑战随机数同时混入 `exporter` 与 `client_nonce`，配合有界防重放 `FenceNonceRegistry` 完成双向域分隔 Ed25519 挑战-响应，密码学绑定全部五轴 `(CellIdentity, node_boot_generation, PrincipalId, CellEpoch, CellFencingToken)` 为不可变 `AuthenticatedPeerSession`。`PeerFencePolicy` 严格拆分为签名前纯只读预检 `check_peer_fence` 与签名验真通过后的原子提交 `commit_verified_peer_fence`（杜绝未验签 `ClientHelloFence(u64::MAX)` 毒化单调水位线导致永久拒绝服务），并强制校验 `CellIdentity <-> PrincipalId` 绑定（`HandshakeRejectCode::CellPrincipalBindingMismatch`），阻断同 `IdentityAuthority` 内其他合法节点跨 IP 重绑冒充目标 `CellIdentity`。

3. **4B 大端长度前缀多路复用帧与双优先级流隔离背压（`ADR-0026` §3.1）**
   - `crates/nlos-net/src/frame.rs`：4B 大端外层长度前缀 + 显式协议版本与类型标签，强制 `MAX_BULK_CHUNK_BYTES = 64 KiB`（`65_536 B`），在分配缓冲区前校验长度上界防止畸形头部触发 OOM。
   - `crates/nlos-net/src/mux.rs`：`MultiplexedSession` 写协程采用 `tokio::select! { biased; ... }` 严格优先排空高优先级队列（`ControlRpcRequest` / `ControlRpcResponse` / `BulkWindowCredit` / `BulkStreamCompleteAck`），随后才调度有界低优先级 `BulkChunk` 队列；`MuxConfig::max_concurrent_bulk_streams`（默认 `16`）在分配流状态前限制单连接并发入站大块流数量；接收端 `BulkStreamReader` 仅在业务消费者实际拉取并排空分块（`next_chunk()`）后才向发送端回补 `BulkWindowCredit`，并在流结束时执行端到端 `SHA-256` 完整性校验。

4. **确定性套接字/TLS 失败 -> `RemoteCallOutcome` 映射与对账门禁（`ADR-0026` §2.4, `[DIST-FAIL-001]`）**
   - `crates/nlos-net/src/failure.rs` 与 `transport.rs`：按精确 `WriteProgressPhase` 分类传输故障：
     - `BeforeFirstPayloadByte`（`SingleHostDesktop` 拒绝、路由未解析、`ECONNREFUSED`/连接超时、mTLS 拒绝、五轴 Exporter 握手拒绝、以及大块流首块为空 `BulkChunkEmpty` / 超 `64 KiB` / `total_bytes` 不匹配预检拒绝）→ 网络连接前故障确定性映射为 `RemoteCallOutcome::Unavailable`（预算内 `SafeToRetryWithSameKey`），不污染对账门禁；
     - `AfterFirstPayloadByte`（首字节或 `BulkStreamOpen` 进入 TLS/TCP 写路径后的 `ConnectionReset`/TCP `RST`、无 `close_notify` 的 TLS 截断 `UnexpectedEof`、半开静默黑洞 `ReadTimeout`/`WriteTimeout`、以及大块流中途第 `k >= 1` 块生产者故障）：中途故障立即终止半写 `MultiplexedSession` 以释放远端入站流表；当 `side_effecting == false` 时映射为 `DeadlineExceededUncertain { side_effecting: false, retry_directive: SafeToRetryWithSameKey }`；当 `side_effecting == true` 时强制映射为 `DeadlineExceededUncertain { side_effecting: true, retry_directive: ReconcileBeforeRetry }`，并由 `PendingReconciliationTracker` 在客户端传输入口拦截同一 `IdempotencyKey` 的盲重试（返回 `RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation`），直至显式 `mark_reconciled`；
     - `aggregate_fanout_outcomes` 对多分片结果按 `CellIdentity` 规范排序后调用 `RemoteCallContract::classify_partial`，产出与分片返回到达顺序无关的确定性 `SHA-256` `uncertainty_digest`。

5. **纯无 I/O 确定性 3 节点 `ConsensusCore` 状态机种子（`ADR-0026` §2.1, §3.2, §6.3, §7.1）**
   - `crates/nlos-net/src/poc_consensus_core_3node.rs`：
     - 通过 `ControlPlaneLogPayload` 结构化限制仅允许 4 类低频控制平面元数据进入共识日志（`CellDirectoryUpdate`、`RootLeaseEpochAdvance`、`TaskAuthorityAssignment`、`ReconciliationAnchor`），遵守 `[CONS-SCOPE-001]`；
     - `VoterRoster::new` 强制 `N > 1` 投票节点拥有互不相同的 `ControlDomainId`（违反时以 `VoterRosterError::DuplicateControlDomain` fail-closed 拒绝），遵守 `[TASK-SCALE-002]`；
     - 支持 `N = 1` 退化单节点零选举流量直接提交模式，以及 `N = 3` Raft 多数派选举、当前任期多数派（`2/3`）推进 `consensus_commit_index`、更高任期消息对旧任期少数派 Leader 的即时降级栅栏（含基于活多数派心跳轮次的线性一致 `ReadIndex` 确认门禁）与分区恢复后的未提交冲突后缀截断收敛。

## 2. `RISK-B-09` 新增依赖供应链与许可证审计

`crates/nlos-net` 显式关闭 `rustls` 与 `tokio-rustls` 默认特性（避免引入需要 CMake/NASM 构建的 `aws-lc-rs` 及遗留 TLS 1.2 协议栈），仅启用 `ring` 后端与 TLS 1.3：

| Crate | 解析版本 | License | `rust-version` (MSRV) | 角色与安全边界审计 |
| --- | --- | --- | --- | --- |
| `rustls` | `0.23.46` | `Apache-2.0 OR ISC OR MIT` | `1.71` | 纯 Rust TLS 1.3 状态机（`default-features = false, features = ["ring", "std", "logging"]`；禁用 TLS 1.2 与 0-RTT） |
| `tokio-rustls` | `0.26.6` | `MIT OR Apache-2.0` | `1.71` | `tokio` 异步流与 `rustls` 连接桥接（`default-features = false, features = ["ring", "logging"]`） |
| `ring` | `0.17.14` | `Apache-2.0 AND ISC` | `1.66.0` | 经审计 AEAD / ECDHE / Ed25519 TLS 1.3 密码学原语（无 OpenSSL、无 CMake/NASM 外部构建器依赖） |
| `rustls-pki-types` | `1.15.1` | `MIT OR Apache-2.0` | `1.60` | 零逻辑 DER 证书与私钥切片类型包装 |
| `rustls-webpki` | `0.103.16` | `ISC` | `1.71` | `rustls` 内部传输依赖（节点身份最终由 `MtlsNodeCertVerifier` + `IdentityAuthority` 裁决） |
| `untrusted` | `0.9.0` | `ISC` | `<= 1.66` | `ring` 零拷贝输入切片边界读取器 |
| `zeroize` | `1.9.1` | `Apache-2.0 OR MIT` | `1.85` | 敏感密钥缓冲归零擦除 |
| `subtle` | `2.6.1` | `BSD-3-Clause` | `<= 1.85` | 常量时间比较原语（工作区既有 `ed25519-dalek` 共享依赖） |

全部解析版本均满足工作区 `rust-version = "1.97"` 要求，许可证均为宽松开源许可证（`MIT` / `Apache-2.0` / `ISC` / `BSD-3-Clause`），且 `crates/nlos-net` 自身保持 `unsafe_code = "forbid"`。

## 3. 验证

在车道隔离模式（不触碰根 `Cargo.toml` / `Cargo.lock`，待 `W69-INT` 波次屏障合入工作区成员）下执行全套验证：

```text
cargo fmt --manifest-path crates/nlos-net/Cargo.toml -p nlos-net -- --check            → 退出码 0
cargo clippy --manifest-path crates/nlos-net/Cargo.toml -p nlos-net --all-targets -- -D warnings → 退出码 0，零警告
cargo test --manifest-path crates/nlos-net/Cargo.toml -p nlos-net --no-fail-fast       → 退出码 0（3 unit + 8 PoC-XHOST-001 integration + 0 doc = 11 passed / 0 failed）
```

### 3.1 单元测试（`crates/nlos-net/src/**`，3 passed / 0 failed）

1. `cert::tests::x509_ed25519_cert_roundtrips_and_rejects_expiry_or_tampering`：验证纯 Rust Ed25519 X.509v3 `NLOSCERT` 证书签发、有效窗口验真、未生效/过期拒绝、篡改签名位拒绝，以及重复 `OID_NLOS_PRINCIPAL_BINDING` X.509 扩展按 `RFC 5280` §4.2 fail-closed 拒绝。
2. `frame::tests::mux_frame_roundtrips_all_variants_and_enforces_64kib_ceiling`：验证全部 7 类 `MuxFrame` 编解码往返一致，以及 `65_537 B`（`> 64 KiB`）分块以 `FrameCodecError::BulkChunkOutOfBounds` fail-closed 拒绝。
3. `resolver::tests::conditional_invalidation_protects_freshly_rotated_routing_hints`：验证 `expected_principal` 在版本推进中持续保留且拒绝异主冲突覆盖，并验证 `invalidate_if_version_at_most` 保护已推进至 `epoch = 2` 的新路由提示不被滞后的 `epoch = 1` 失效回调误删。

### 3.2 `PoC-XHOST-001` 集成与门禁测试（`crates/nlos-net/tests/poc_xhost_001.rs`，8 passed / 0 failed）

1. `poc_01_valid_5axis_mtls_and_tls13_exporter_handshake_pins_authenticated_peer_session`：基于真实持久 `IdentityAuthority`（`WAL+FULL`）与双独立 `SigningKeyCustodyVault`（`WAL+FULL`）在 loopback TCP + TLS 1.3 mTLS 上完成五轴 Exporter 互认握手，验证双端钉住完全一致的 32B `tls_exporter_binding`、`session_transcript_digest`、对端 `ControlDomainId` 与五轴声明，成功完成 `ControlRpc` 往返，并在两端托管库各自留存 2 条持久 `CustodySignAuditReceipt`（证书签发 + Exporter 握手）。
2. `poc_02_negative_security_matrix_fails_closed_before_any_rpc_payload`：全覆盖安全负向与对抗矩阵：
   - (2a) 通过 `revoke_compromised_key_with_identity` 撤销/标记泄露后的节点证书在 TLS 1.3 握手阶段即 fail-closed 拒绝，服务端业务 RPC 计数为 `0`；
   - (2b) 外部未知 `IdentityAuthority` 签发的陌生 `PrincipalId` 被拒绝；
   - (2c.0) 未验签 Step 1 `ClientHelloFence(u64::MAX)` 配合伪造 Step 3 签名被 `ExporterSignatureInvalid` 拒绝，且绝不毒化 `MonotonicPeerFenceGate` 水位线（紧随其后的正常 `boot_gen = 3` 会话成功建立）；
   - (2c.1 / 2f) 同 `IdentityAuthority` 下合法兄弟节点 `Principal_Imposter` 分别在入站与出站（模拟 DHCP/IP 重绑）冒充目标 `CellIdentity`，均被 `CellPrincipalBindingMismatch` fail-closed 阻断；
   - (2c.2) 捕获第 1 条真实 TLS 1.3 会话签名的 `ClientHelloFence` + `ClientFenceAttestation` 并在第 2 条独立 TLS 1.3 会话（不同 ECDHE `exporter`）上原样重放，被服务端以 `HandshakeRejectCode::ExporterSignatureInvalid` 确定性拒绝；
   - (2d) 回退的 `node_boot_generation`（`2 < 3`）与回退的 `CellEpoch`（`4 < 5`）分别被服务端以 `StaleBootGeneration` 与 `StaleCellEpoch` fail-closed 拒绝；
   - (2e) 非 TLS 1.3 明文/降级字节流连接被 `TlsAcceptor` 拒绝并记入有界拒绝审计环。
3. `poc_03_endpoint_resolver_monotonic_cas_and_live_socket_rotation_by_cell_identity`：验证 `MonotonicEndpointResolver` 放行幂等重放、拒绝旧 `heartbeat_seq` 与同版本异址冲突；模拟节点重启与端口重绑（`addr_1 -> addr_2`, `boot_gen: 1 -> 2`, `epoch: 1 -> 2`）而 `CellIdentity` 保持不变，`MtlsCellTransport` 仅凭 `CellIdentity` 自动切换至 `addr_2` 完成新纪元握手与 RPC。
4. `poc_04_deterministic_socket_failure_taxonomy_and_reconciliation_fence`：
   - (4a) 连接已关闭端口（`ECONNREFUSED`, `BeforeFirstPayloadByte`）映射为 `RemoteCallOutcome::Unavailable { retry_directive: SafeToRetryWithSameKey }`，不触发对账锁；
   - (4b) 服务端完成五轴握手并读取业务 `ControlRpcRequest` 后立即以 `SO_LINGER = 0` 发送硬 TCP `RST`（`AfterFirstPayloadByte`, `side_effecting = true`），精确映射为 `DeadlineExceededUncertain { side_effecting: true, retry_directive: ReconcileBeforeRetry }`，且 `RemoteCallContract::next_retry_for_outcome` 与 `MtlsCellTransport::call_cell_rpc` 双重拦截同幂等键盲重试（`UncertainSideEffectRequiresReconciliation`），直至显式 `mark_reconciled` 后重试成功；
   - (4c) 半开超时 `ReadTimeout` 在只读请求（`side_effecting = false`）下映射为 `SafeToRetryWithSameKey`；
   - (4d) 3 分片并发结果（1 成功 + 1 `Unavailable` + 1 `DeadlineExceededUncertain`）经 `aggregate_fanout_outcomes` 聚合为 `RemoteCallOutcome::Partial { completed_shards: 1, failed_shards: 2, retry_directive: ReconcileBeforeRetry }`，且乱序输入产出逐字节一致的 `SHA-256` `uncertainty_digest`。
5. `poc_05_64mib_chunked_bulk_stream_backpressure_and_priority_rpc_isolation`：首先验证 `65_537 B` 超界分块被 `BulkChunkExceedsMax` 拒绝；随后在**单条共享 TLS 1.3 连接**上以 `256 KiB`（`4 × 64 KiB`）信用窗口执行完整 **`64 MiB`（`67_108_864 B` = `1024 × 64 KiB`）** 大块流式传输，并在接收端消费者完全暂停（窗口打满背压）期间及恢复流式排空期间并发发送 **20 次高优先级 `ControlRpc` ping**：20 次 ping 100% 零饥饿成功返回，`64 MiB` 流完成端到端 `SHA-256` 校验，且接收端峰值缓冲严格满足 `max_inbound_bulk_buffered_bytes() <= 256 KiB`（仅为总传输量的 `1/256`）。
6. `poc_06_single_host_desktop_n1_default_binds_zero_sockets_and_fails_closed`：验证默认 `ClusterProfile::SingleHostDesktop`（`N = 1`）下监听绑定、远端 RPC 与大块流全部 fail-closed 拒绝，且 `peak_bound_listener_count() == 0`、`peak_active_connection_count() == 0`。
7. `poc_07_consensus_core_3node_election_quorum_commit_fencing_and_heal_convergence`：验证 `[TASK-SCALE-002]` 拒绝共享 `ControlDomainId` 的投票配置、验证 `N = 1` 零网络消息直接提交、以及 `N = 3` 集群确定性选主、单 Follower 隔离下 `2/3` 多数派推进 `consensus_commit_index`、非对称分区下旧任期少数派 Leader 无法推进提交亦无法确认线性一致读、多数派选出更高任期新 Leader 提交新条目并确认线性一致读、以及分区愈合后旧 Leader 自动降级截断冲突未提交后缀并达成 3 节点日志与 `commit_index` 完全一致。
8. `poc_08_post_first_byte_bulk_producer_reconciliation_and_bounded_resource_accounting`：验证大块流首字节前空分块（`BulkChunkEmpty`）与 `total_bytes` 不匹配在发包前 fail-closed；验证 `BulkStreamOpen` + 第 0 块写出后（`AfterFirstPayloadByte`, `side_effecting = true`）第 1 块触发超界故障时精确映射为 `DeadlineExceededUncertain { retry_directive: ReconcileBeforeRetry }`、激活 `PendingReconciliationTracker` 盲重试门禁并关闭底层会话使服务端入站流表归零；验证 `close_all_sessions()` 后客户端与服务端 `active_connection_count()` 双双立即归零；并验证 `max_concurrent_bulk_streams = 2` 对第 3 条并发入站大块流 fail-closed 阻断。

## 4. 边界与限制（诚实域）

- 本证据完成 `ADR-0026` §7.1 定义的 `PoC-XHOST-001` 门禁套件与 `C-NET-TRANSPORT` 核心 crate `crates/nlos-net` 实现：套接字与 TLS 1.3 mTLS 集成测试运行于单机多端口真实 TCP loopback 套接字，3 节点 `ConsensusCore` 运行于确定性内存网络模拟器。
- 严格遵循 `ADR-0026` §1.2 与 `Claim <= Evidence` 规则：本证据**不声称**后续阶段任务——混合时钟与分布式租约（`C-NET-CLOCK-LEASE`）、独立持久 WAL 控制平面共识 crate `crates/nlos-consensus`（`C-CONSENSUS-CP`）、跨机快照流式迁移与 `CellHost` 联邦生产接线（`C-NET-SYNC-MIGRATE`）或真实多物理宿主机/独立 VM 跨网段硬件故障注入门禁（`C-XHOST-GATE` / `ROAD-C-001-XHOST` 完整结项判据）已落地；`ADR-0026` 从 `CANDIDATE` 晋升至 `ACCEPTED` 及根 `Cargo.toml` 工作区成员合入由波次集成器（`W69-INT`）与独立复核执行。

## 5. 复现

```text
# 波次合入前（独立 manifest 模式）：
cargo test --manifest-path crates/nlos-net/Cargo.toml --no-fail-fast -- --nocapture

# W69-INT 合入根工作区后：
cargo test -p nlos-net --no-fail-fast -- --nocapture
```
