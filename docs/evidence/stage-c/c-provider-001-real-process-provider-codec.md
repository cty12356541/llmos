# C-PROVIDER-001 / W65-CPROVIDER-REAL：跨进程真实 OS Provider Worker、跨平台 ADR-0011 认证 IPC、ADR-0024 冻结 Protobuf 载荷通道与 TASK-PROVIDER-001 隔离姿态核验

- 状态：`PASS`（`ADR-0024` 正式冻结第 8 个 SABI 通道 `nlos.sabi.DriverProvider` v1.0，独立 OS 子进程 `nlos-provider-worker` 与 `OutOfProcessProviderDriver` 跨平台认证 IPC、崩溃降级重放、代数/取消栅栏以及 `[TASK-PROVIDER-001]` `PROVIDER_ISOLATION_UNVERIFIED` 强制标记已落地并经真实子进程验证）
- 日期：2026-10-25
- Owner：`C-PROVIDER-REAL`（`W65-CPROVIDER-REAL` 车道）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §10.2 `[TASK-PROVIDER-001]` + §33.1 `[ROAD-A-003]` + §33.2 `[ROAD-B-003]` + [ADR-0011](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md)（IPC 主体认证与签名透传）+ [ADR-0014](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)（SABI 通道冻结纪律）+ [ADR-0024](../../management/adrs/0024-driver-provider-payload-codec-freeze.md)（DriverProvider 载荷通道冻结，关闭 `RISK-B-05 review_point` 与 `U-10`）
- Base HEAD：`5ac73b6333515efbd28b9fa94a4c9eb27df2cf18`

## 1. 实现事实

- **ADR-0024 与 `nlos.sabi.DriverProvider` v1.0 冻结 Protobuf 通道**：
  - `docs/management/adrs/0024-driver-provider-payload-codec-freeze.md`（新增，`ACCEPTED / VERIFIED`）：正式裁决跨进程/生产 Provider 通信统一采用 SABI 冻结 Protobuf 契约，将 `nlos.sabi.DriverProvider` 晋升为第 8 个 SABI 注册表冻结通道（`major: 1, minor: 0, frozen: true`，`MAX_DRIVER_PROVIDER_PAYLOAD_BYTES = 16 * 1024`，`MAX_PROVIDER_PROFILE_BYTES = 128`），同时保留 `nlos-driver-mock::codec`（确定性 `minicbor`）作为既有 Stage B 进程内/回环测试兼容层，闭合 `RISK-B-05 review_point` 与 `U-10`。
  - `schema/nlos/sabi/v1/driver_provider.proto`（新增）、`crates/nlos-schema/build.rs`、`crates/nlos-schema/src/lib.rs` 与 `crates/nlos-schema/tests/compatibility.rs`：
    - 定义 `DriverProviderRequest`（`register` / `dispatch` / `complete` / `cancel` / `query_isolation` 五分支）、`DriverProviderResponse`（`register_ack` / `dispatch_ack` / `complete_ack` / `cancel_ack` / `isolation_report` 五分支）以及携带 `[TASK-PROVIDER-001]` 四项布尔隔离承诺与 32 字节证明摘要的 `ProviderIsolationAttestationWire`。
    - 在 `nlos-schema` 注册 `SABI_DRIVER_PROVIDER_SCHEMA`（既有 7 个通道描述子与黄金字节向量 100% 字节不变），提供有界编解码与 fail-closed 校验器（`encode_driver_provider_request` / `decode_driver_provider_request` / `encode_driver_provider_response` / `decode_driver_provider_response`），并在 `compatibility.rs` 钉死确定性黄金向量、全分支往返与越界/错通道负向拒绝测试。
- **确定性 Protobuf 编解码与 `[TASK-PROVIDER-001]` 隔离姿态核验（`crates/nlos-driver-mock/src/proto_codec.rs`）**：
  - 实现领域对象与 `nlos.sabi.DriverProvider` Protobuf 之间的双向无损映射，以及子进程确定性计算摘要 `derive_worker_compute_digest`（域分隔 `nlos/driver-provider-worker/compute/v1`）。
  - 落地 `[TASK-PROVIDER-001]` 硬门控（`ProviderIsolationAttestation::evaluate_posture`）：仅当 `tenant_isolated && session_ephemeral && cache_partitioned && zero_data_retention` 全部为真且 `attestation_digest != [0u8; 32]` 时返回 `ProviderIsolationPosture::VerifiedEndToEndIsolated`（标记 `PROVIDER_ISOLATION_VERIFIED`）；只要缺失任一承诺（例如共享 KV 缓存 `cache_partitioned == false`）或证明摘要全零，执行回执一律强制降级标记为 `PROVIDER_ISOLATION_UNVERIFIED` 并枚举全部缺失项 `IsolationGuarantee`。
- **独立 OS 子进程 Provider Worker（`crates/nlos-driver-mock/src/bin/nlos-provider-worker.rs`）**：
  - 独立可执行文件，打开持久 `SqliteOperationStore`、`IdentityAuthority` 与 `AuthorityClock` 根目录，跨平台绑定本地 IPC（Unix 走 `UnixListenerAdapter` 并自动清理崩溃残留 socket，Windows 走 `NamedPipeListenerAdapter` 多实例命名管道与空闲超时重绑），在 stdout 输出 `READY endpoint=<...> pid=<os_pid>` 就绪行。
  - 每个连接强制先走 ADR-0011 主体挑战-响应握手（`ServerHandshakeContext` 单次消费随机挑战 + `IdentityAuthority` Ed25519 验签 + 主体白名单与 `CallerIdentity` 一致性核验），认证失败在进入 `serve_one` 前直接断开连接，杜绝未认证载荷触碰 `SqliteOperationStore`。
  - 在独立子进程内驱动 `prepare_dispatch -> activate_dispatch -> complete / request_cancel_idempotent` 持久状态转换，并根据 `activation.cancel_epoch < snapshot.cancel_epoch` 向客户端精确报告 `fenced_reconciliation_only`。
- **跨进程监督驱动与 `ProviderCache` 联动（`crates/nlos-driver-mock/src/process_provider.rs`、`crates/nlos-driver-mock/src/lib.rs`）**：
  - 实现 `OutOfProcessProviderDriver`（支持 `spawn`、`connect_existing`、`kill_worker`、`respawn_worker`、`with_provider_cache`、`register`、`dispatch`、`complete`、`cancel`、`query_isolation`、`exchange_raw_payload`）与消费者侧观测包装 `OutOfProcessProviderCache`。
  - 当真实子进程崩溃（`kill_worker`）导致后续 IPC 连接/收发不可达时，自动将驱动自身及关联 `ProviderCache` 同步置入 `CacheHealth::Degraded { cause: DegradationCause::ProviderUnreachable }`；当 `respawn_worker` 拉起新 OS PID 并完成重放应答后，自动将 `ProviderCache::health()` 恢复为 `CacheHealth::Healthy`（通过零持久副作用只读探针同步，无需修改 `cache.rs`）。

## 2. 验证

```text
cargo fmt -p nlos-schema -p nlos-driver-mock -- --check                → 退出码 0
cargo clippy -p nlos-schema -p nlos-driver-mock --all-targets -- -D warnings → 退出码 0，零警告
cargo test -p nlos-schema -p nlos-driver-mock --no-fail-fast           → 退出码 0（nlos-schema 39 passed + nlos-driver-mock 25 passed = 64 passed / 0 failed）
cargo test -p nlos-ipc --no-fail-fast                                  → 退出码 0（27 passed / 0 failed，下游传输回归通过）
```

分二进制明细（Windows 主机实测，命名管道 `\\.\pipe\nlos-oop-*` + 真实 `nlos-provider-worker.exe` 子进程；Unix 分支共享同一 `UnixListenerAdapter` / `authenticated_connect` 跨平台适配面）：

- `nlos-schema` `tests/compatibility.rs`：`39 passed; 0 failed`（含新增 `driver_provider_request_and_response_pin_frozen_golden_bytes`、`driver_provider_all_arms_round_trip`、`driver_provider_request_bounds_fail_closed`、`driver_provider_response_bounds_fail_closed` 及 8 项冻结注册表核验）。
- `nlos-driver-mock` `tests/out_of_process_provider.rs`（新增，`7 passed; 0 failed`）：
  1. `real_child_worker_end_to_end_lifecycle_over_authenticated_ipc_and_frozen_protobuf`：启动真实 `nlos-provider-worker` 子进程（断言 `worker_pid != std::process::id()`），经 Windows Named Pipe / Unix Socket 完成 ADR-0011 握手与 `query_isolation -> register -> dispatch -> complete` 全链路冻结 Protobuf 交互，核验确定性 `compute_digest`、`SqliteOperationStore` 终态及 `OutboxKind::WakeFiber`。
  2. `task_provider_001_isolation_posture_marks_unverified_profiles_and_zero_digests`：验证 `shared-cache-unverified`（即使携带非零摘要仍缺失 `CachePartitioned`）、`verified` 但全零证明摘要（缺失 `NonZeroAttestationDigest`）以及五项全缺失场景均被强制打上 `PROVIDER_ISOLATION_UNVERIFIED` 并精确列出缺失保障项。
  3. `unauthenticated_forged_and_unauthorized_principals_rejected_before_store`：验证跳过握手的原始未认证帧、未注册主体、伪造 Ed25519 签名以及虽已注册但不在 `--allowed-principal-hex` 白名单内的第二主体（返回 `SabiErrorCode::Rights`）全部在触碰 `SqliteOperationStore` 前被拦截，且同一 `worker_pid` 随后正常服务合法主体。
  4. `worker_crash_triggers_provider_cache_degradation_and_respawn_replays_cleanly`：在途操作已 `dispatch` 后调用 `kill_worker()` 硬杀真实子进程，验证下一次 RPC 观测到 `Unreachable` 并将 `ProviderCache` 与 `OutOfProcessProviderCache` 同步降级为 `Degraded { ProviderUnreachable }`；随后 `respawn_worker()` 拉起新 OS 进程，重放 `register` 与 `dispatch`（`replayed == true`，同一 `CallbackTicket` 与回执，零重复派发），缓存恢复 `Healthy` 并完成终态提交与幂等重放。
  5. `stale_generation_fails_closed_over_out_of_process_channel_with_zero_durable_mutation`：跨进程发送陈旧 `generation = 2` 派发请求以 `SabiErrorCode::Conflict` fail-closed 拒绝，缓存保持 `Healthy`，`SqliteOperationStore` 保持 `Registered`、无 `DispatchPreparation`、无 outbox 残留。
  6. `cancel_epoch_fence_wins_over_late_completion_across_worker_restart`：在子进程 1 完成 `register -> dispatch -> cancel`（推进 `cancel_epoch = 1`），重启拉起子进程 2 后送达取消前旧 `CallbackTicket` 的迟到 `complete`，验证返回 `fenced_reconciliation_only == true` 且持久 outbox 仅产生 `OutboxKind::ReconcileEffect`（`WakeFiber` 被栅栏阻断）；跨重启重放同种子幂等返回，异种子以 `SabiErrorCode::Conflict` 拒绝。
  7. `malformed_oversized_and_wrong_schema_payloads_fail_closed_without_crashing_worker`：通过 `exchange_raw_payload` 依次注入损坏 Protobuf 字节、超过 `MAX_DRIVER_PROVIDER_PAYLOAD_BYTES`（`16 KiB + 1`）的超限载荷、`nlos.sabi.SystemControl` 跨通道错绑载荷以及方法与消息体分支失配载荷，验证全部返回 `SabiErrorCode::InvalidArgument`、子进程不崩溃、`SqliteOperationStore` 零写入，随后合法请求在同一 PID 上立即成功。
- `nlos-driver-mock` 既有回归套件：`provider_core`（`9 passed`）、`degradation`（`5 passed`）、`effect_fence`（`3 passed`）、`ipc_authenticated_windows`（`1 passed`）全部零回归通过。

## 3. 边界与限制（诚实域）

- 本证据闭合移交项 `#9 C-PROVIDER-REAL`（真实独立 OS 进程 Provider 载体替换进程内 mock + 跨平台 Unix/Windows 认证传输 + `ADR-0024` `nlos.sabi.DriverProvider` v1.0 冻结 Protobuf 载荷通道 + `[TASK-PROVIDER-001]` 隔离证明门控）。
- 遵循总纲 `[TASK-PROVIDER-001]` 与 Stage C 诚实分级纪律：`nlos-provider-worker` 的本地执行核采用确定性种子推导计算（`derive_worker_compute_digest` + `derive_provider_outcome`）以保证跨重启重放与栅栏验证的可复现性；本切片不声称已内置外部公有云商用 LLM HTTP/gRPC SDK 客户端或远程 TEE 硬件芯片厂商根证书在线验真网络链路——任何接入真实外部云推理端点的部署适配器若未提供经核验的四项隔离承诺与非零证明摘要，按本切片已落地的 `[TASK-PROVIDER-001]` 契约一律以 `PROVIDER_ISOLATION_UNVERIFIED` 记录。

## 4. 复现

```text
cargo test -p nlos-schema --test compatibility -- --nocapture
cargo test -p nlos-driver-mock --test out_of_process_provider -- --nocapture
```
