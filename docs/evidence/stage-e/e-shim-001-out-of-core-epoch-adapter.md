# E-SHIM-001：核外无特权兼容垫片（`nlos-compat-shim`）、`N-1 / N-2 <-> N` 线协议转换与 `[DRV-SHIM-001]` 四层零特权隔离验证证据（`W78-E2`）

- **Evidence ID**: `e-shim-001-out-of-core-epoch-adapter`
- **Wave / Lane**: `W78 / W78-E2-COMPAT-SHIM`（里程碑 `M-E2` / 工作包 `E2` 左半片核外兼容垫片实现）
- **日期**: `2026-10-31`
- **Assurance Level**: `H4`（纯确定性本地宿主单元测试、真实 SQLite 多权威端到端集成与对抗性旁路隔离验证；不声称 `H7` 生产环境证明）
- **状态**: `CANDIDATE_VERIFIED`（车道内全部静态与动态门禁通过，待 `W78` 屏障合流晋升 canonical `DONE`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §9（`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`、`SABI-DISC-001`、`SABI-TRANSPORT-001`、`KABI-IDEM-001`、`KABI-UNCERTAIN-001`）、§21（`DRV-SHIM-001`、`DRV-CRED-001`、`DRV-BOUND-001`、`DRV-ACTIVE-001`、`DRV-LIFE-001`、`DRV-NEGO-001`、`DRV-PROTO-001`）、§28（阶段 E 路线图 `E2` / `M-E2`）
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.2（支柱二：核外兼容垫片与 `[DRV-SHIM-001]` 四层双边界切分）
  - [ADR-0013 跨权威先验后提契约](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md)
  - [ADR-0014 SABI Schema 通道 v1-beta 冻结规则](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)
  - [ADR-0024 DriverProvider 载荷编解码冻结契约](../../management/adrs/0024-driver-provider-payload-codec-freeze.md)
  - [stage-e-progress.md](../../management/stage-e-progress.md) §E.2.2（工作包 `E2` / 里程碑 `M-E2`）
  - [E-SABI-001 9/9 SABI v1 通道全冻结与版本兼容矩阵证据](./e-sabi-001-version-matrix-and-deprecation.md)

---

## 1. 架构落点与核心控制面零污染验证

依据 `ADR-0027` §2.2.1“核心控制面零历史分支污染铁律”，在独立核外用户态兼容包 `crates/nlos-compat-shim` 中完整实现历史纪元动态挂载裁决、SABI 信封跨纪元双向转换、Stage B `minicbor` 与冻结第 8 通道 `nlos.sabi.DriverProvider` 双向桥接，以及四层双边界特权隔离门禁验证：

1. **核心权威零反向依赖与零历史分支污染**：
   - `nlos-cell`、`nlos-task`、`nlos-capability`、`nlos-resource`、`nlos-consensus`、`nlos-system-control`、`nlos-identity`、`nlos-operation` 八个核心 TCB crate 的 `Cargo.toml` 均保持 `0` 处对 `nlos-compat-shim` 的依赖引用，核心权威状态机仅接纳当前宿主纪元（`host_epoch == N`）的 canonical 冻结线帧。
   - 本次实现未向工作区新增任何第三方外部依赖，未修改 `Cargo.lock` 或其他既有 crate，且全 crate 顶层声明 `#![forbid(unsafe_code)]`。
2. **模块职责切分**：
   - [lib.rs](../../../crates/nlos-compat-shim/src/lib.rs)：`#![forbid(unsafe_code)]`、有界遥测上限常量 `MAX_SHIM_DEPRECATION_WARNINGS = 256` 及公共契约导出。
   - [mount.rs](../../../crates/nlos-compat-shim/src/mount.rs)：`CompatMountPlanner`、`PackageCompatibilitySpec`、`DriverWireEpoch`、`CompatMountDecision`（`NativePassthrough` 零跳直通 vs `MountOutOfCoreShim` 按需挂载 1 跳或 2 跳核外栈），以及与 `nlos-service-directory::SnapshotDirectory` 联动的 `[SABI-DISC-001]` 服务协商与弃用遥测收据汇总。
   - [proto_unknown.rs](../../../crates/nlos-compat-shim/src/proto_unknown.rs)：纯安全 Rust 有界 Protobuf 未知 tag 扫描与无损回注编解码器（`extract_envelope_unknown_fields`、`encode_envelope_with_unknown_fields`），覆盖 `Envelope`、`SchemaIdentity`、`SabiRequestContext` 与 `SabiResponseContext` 四级嵌套消息。
   - [sabi_adapter.rs](../../../crates/nlos-compat-shim/src/sabi_adapter.rs)：`SabiEpochAdapterCell`（`N-1 / N-2 <-> N` 双向信封纪元升级/降级、128-bit `request_id` 与 `IdempotencyKey` 原样透传、`[COMPAT-VER-002]` 非关键扩展与未知字段 round-trip 保留、禁止折叠 `EffectUnknown` / `Uncertain` / `Partial` 的安全错误码降级、256 条 FIFO 弃用告警环形缓冲）与传输中立桥接器 `SabiShimBridge`（`InMemoryFrameDuplex` 与 `ByteStreamFrameTransport`）。
   - [driver_shim.rs](../../../crates/nlos-compat-shim/src/driver_shim.rs)：`DriverProtocolShim`、`CancelOperationWire` / `CancelOperationResultWire`（补齐 Stage B `minicbor` 取消线帧契约），实现 `register`、`dispatch`、`complete`、`cancel` 在历史 `minicbor` / 历史 `DriverProvider` 纪元与当前冻结 `nlos.sabi.DriverProvider` Protobuf 之间的 1:1 双向转换，并强制执行 `[TASK-PROVIDER-001]` `PROVIDER_ISOLATION_UNVERIFIED` 诚实标注与防提权守卫。
   - [boundary.rs](../../../crates/nlos-compat-shim/src/boundary.rs)：`ADR-0027` §2.2.2 四层特权隔离参考模型：
     - **Layer 1（核心权威）**：真实 SQLite `IdentityAuthority`、`CapabilityAuthority`、`ResourceAuthority` 与 `OperationRegistry`，仅接受 `host_epoch` canonical 帧。
     - **Layer 2（TCB 边缘 `TrustedEnforcementGate`）**：独占托管脱敏且 `Drop` 自动清零的 `SecretCredential`（`[DRV-CRED-001]`），执行 `ADR-0013` 先验后提（`verify-then-commit`）跨权威准入、`Capability` 持有者 Ed25519 签名与权限/作用域/调用次数核验（`[DRV-SHIM-001]`）、`Operation`-bound `Reservation` 一次性激活令牌与 TCB 计量主体签名的 `SignedMeteredUsageProof` 决算（`[DRV-ACTIVE-001]`）、`deadline` / `ActivityContext` / `Process incarnation` / `Driver generation + fencing_token` 围栏（`[DRV-BOUND-001]`），以及崩溃转 `EffectUnknown` + `Quarantined` 与同 128-bit `IdempotencyKey` 对账闭环（`[DRV-LIFE-001]`、`[KABI-UNCERTAIN-001]`）。
     - **Layer 3（核外无特权 `OutOfCoreCompatShimRuntime`）**：零密钥、零原始凭据、零权威写句柄的纯协议翻译层，内置 6 队列全向线帧捕获器 `WireCaptureTranscript` 供字节级防泄漏扫描审计。
     - **Layer 4（历史纪元调用方与外部不可信 ProviderAdapter）**：发送 `N-1` / `N-2` SABI 帧与历史 `minicbor` / 历史纪元驱动响应。

---

## 2. 架构不变量与测试覆盖追溯矩阵

| 不变量 / 架构条款 | 核心保证语义 | 实现模块 | 验证用例落点 |
| :--- | :--- | :--- | :--- |
| `[SABI-DISC-001]` / `ADR-0027` §2.2.1 | 当前纪元 `N` 零垫片直通（0-hop）；`N-1`/`N-2` 或历史驱动协议动态挂载核外垫片；`0`、`> N`、`<= N-3` fail-closed 返回 `E_ABI_VERSION` + `DoNotRetry` | `src/mount.rs` | `mount_native_epoch_n_uses_zero_hop_passthrough`、`mount_n_minus_1_and_n_minus_2_dynamically_mounts_sabi_adapter_and_emits_deprecation_warnings`、`mount_native_epoch_n_with_legacy_stage_b_minicbor_driver_mounts_only_driver_shim`、`mount_n_minus_1_with_historical_frozen_driver_epoch_mounts_two_hop_stack`、`mount_fails_closed_with_e_abi_version_on_zero_future_and_sunset_epochs` |
| `[COMPAT-VER-002]` / `[COMPAT-DEPRECATE-001]` | 未知 `critical_extensions` 与 `SUNSET` / `RESERVED` 符号或字段号立即拒绝；未知 `non_critical_extension_ids` 与未知原始 Protobuf wire tag 双向 round-trip 无损保留；`DEPRECATED` 契约发出有界 256-FIFO `DeprecationWarningReceipt` | `src/mount.rs`、`src/sabi_adapter.rs`、`src/proto_unknown.rs` | `mount_fails_closed_with_e_abi_version_on_unknown_critical_extension_and_sunset_or_reserved_symbols_and_fields`、`sabi_adapter_preserves_noncritical_extension_ids_and_raw_unknown_protobuf_wire_tags_round_trip`、`sabi_adapter_fails_closed_with_e_abi_version_on_unknown_critical_extension_and_sunset_wire_field`、`sabi_adapter_caps_deprecation_warning_ring_at_256_fifo` |
| `[KABI-IDEM-001]` / `[SABI-TRANSPORT-001]` | 128-bit `request_id` 与 128-bit `IdempotencyKey` 双向零变异透传；错误码安全降级且严禁将 `EffectUnknown`、`Uncertain`、`Partial` 折叠为成功或普通可盲目重试错误；内存双工与字节流传输结果一致 | `src/sabi_adapter.rs` | `sabi_adapter_bidirectional_n_minus_1_and_n_minus_2_preserves_128bit_request_id_and_idempotency_key`、`sabi_adapter_downgrades_modern_error_codes_safely_and_forbids_collapsing_uncertain_or_partial`、`sabi_shim_bridge_operates_over_in_memory_duplex_and_byte_stream_transport_neutrally` |
| `[DRV-NEGO-001]` / `[DRV-PROTO-001]` | Stage B `minicbor` 与冻结 `nlos.sabi.DriverProvider` Protobuf 四类方法（`register` / `dispatch` / `complete` 五态 / `cancel`）1:1 双向无损转换；非法取消后状态 fail-closed；历史冻结纪元驱动帧无损升级 | `src/driver_shim.rs` | `driver_shim_round_trips_register_dispatch_complete_and_cancel_between_minicbor_and_frozen_proto_with_1_to_1_fidelity`、`driver_shim_bridges_historical_frozen_driver_provider_epochs_losslessly` |
| `[TASK-PROVIDER-001]` | 历史 `minicbor` 驱动固定合成 `PROVIDER_ISOLATION_UNVERIFIED` 且全零证明摘要；核外垫片企图伪造 `PROVIDER_ISOLATION_VERIFIED` 在垫片层与 TCB 门禁层双重拦截 | `src/driver_shim.rs`、`src/boundary.rs` | `driver_shim_synthesizes_unverified_isolation_for_legacy_minicbor_and_blocks_forged_elevation`、`task_provider_001_malicious_shim_forging_verified_isolation_on_legacy_driver_completion_fails_closed` |
| `[DRV-CRED-001]` | 外部原始凭据仅驻留 TCB `TrustedEnforcementGate`（`Debug`/`Display` 强制脱敏、`Drop` 清零）；核外垫片全部 6 个方向共 7 帧原始线字节实现 0 字节秘密泄漏；空凭据与未知凭据句柄 fail-closed | `src/boundary.rs` | `drv_cred_001_zero_secret_bytes_appear_in_any_sabi_or_driver_shim_wire_frame`、`drv_cred_001_secret_credential_redacts_debug_display_and_rejects_empty_or_unknown_handles` |
| `[DRV-SHIM-001]` | 核外垫片中途篡改 `CapabilityId` 或 `Generation`、伪造未知 `Capability`、重放已吊销 `Capability`、使用已吊销调用方密钥、非持有者盗用句柄、越权放大衰减委派权限/命名空间、耗尽 `call_limit` 全部被真实 `IdentityAuthority` + `CapabilityAuthority` 拒绝，且 `verify-then-commit` 不提前污染 `Reservation` | `src/boundary.rs` | `drv_shim_001_mid_transit_shim_tampering_of_capability_id_fails_closed`、`drv_shim_001_mid_transit_shim_tampering_of_capability_generation_fails_closed`、`drv_shim_001_forged_unknown_capability_handle_signed_by_caller_fails_closed`、`drv_shim_001_revoked_capability_handle_replay_fails_closed`、`drv_shim_001_revoked_caller_signing_key_fails_closed`、`drv_shim_001_wrong_holder_principal_stealing_capability_handle_fails_closed`、`drv_shim_001_attenuated_delegated_capability_missing_required_right_or_wrong_target_fails_closed`、`drv_shim_001_exhausted_capability_call_limit_blocks_second_dispatch_without_activating_reservation` |
| `[DRV-ACTIVE-001]` / `[DRV-BOUND-001]` | 缺失/跨 `Operation` 劫持 `ReservationId`、错误 `activation_token`、重复激活 `Active` 预留、非 TCB 计量主体伪造或篡改 `SignedMeteredUsageProof`、超额 `UsageExceedsUpperBound`、过期 `deadline`、空 `ActivityContext`、过期 `process_generation`、轮换后旧 `driver_generation` / `fencing_token` 全部 fail-closed 拦截 | `src/boundary.rs` | `drv_active_001_missing_or_unknown_reservation_fails_closed_without_consuming_capability`、`drv_active_001_cross_operation_hijacked_reservation_or_wrong_activation_token_fails_closed`、`drv_active_001_duplicate_activation_of_already_active_reservation_with_new_idempotency_key_fails_closed`、`drv_active_001_forged_wrong_signer_or_tampered_usage_receipt_fails_closed`、`drv_active_001_usage_exceeding_reservation_upper_bound_fails_closed_with_budget_error`、`drv_bound_001_expired_deadline_or_missing_activity_context_fails_closed`、`drv_bound_001_stale_process_incarnation_generation_fenced_at_tcb_gate`、`drv_bound_001_stale_driver_generation_and_rotated_fencing_token_fenced_by_resource_authority` |
| `[DRV-LIFE-001]` / `[KABI-UNCERTAIN-001]` | 核外垫片或驱动在途崩溃将未决 `Operation` 置为 `EffectUnknown`、关联 `Reservation` 置为 `Quarantined`（返回 `QueryOperationOrRetrySameIdempotencyKey`），物理阻断换新 `IdempotencyKey` 的盲目重试；携带原 128-bit `IdempotencyKey` 对账后精确解除隔离、退还差额额度并支持幂等重放 | `src/boundary.rs` | `drv_life_001_and_kabi_uncertain_001_mid_flight_shim_crash_transitions_to_effect_unknown_and_quarantined_and_blocks_blind_retry`、`kabi_uncertain_001_reconciliation_with_exact_original_128bit_idempotency_key_settles_quarantined_reservation_idempotently` |
| `ADR-0027` §2.2.1 核心零污染 | TCB 门禁拒绝未经垫片转换的非 canonical 纪元线帧直连；静态扫描 8 个核心 TCB crate `Cargo.toml` 零 `nlos-compat-shim` 依赖 | `src/boundary.rs`、`tests/drv_shim_adversarial_isolation.rs` | `four_layer_e2e_n_minus_1_legacy_minicbor_dispatch_and_metered_completion_succeeds`、`tcb_enforcement_gate_rejects_non_canonical_client_epoch_frames_directly`、`zero_core_crate_dependency_pollution_static_cargo_toml_audit` |

---

## 3. 独占写集、可复现验证命令与门禁结果

### 3.1 本车道独占写集清单

1. `crates/nlos-compat-shim/src/lib.rs`
2. `crates/nlos-compat-shim/src/proto_unknown.rs`（新增）
3. `crates/nlos-compat-shim/src/sabi_adapter.rs`（新增）
4. `crates/nlos-compat-shim/src/driver_shim.rs`（新增）
5. `crates/nlos-compat-shim/src/mount.rs`（新增）
6. `crates/nlos-compat-shim/src/boundary.rs`（新增）
7. `crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs`（新增，`15` 项集成测试）
8. `crates/nlos-compat-shim/tests/drv_shim_adversarial_isolation.rs`（新增，`24` 项真实多权威集成、对抗隔离与静态审计测试）
9. `docs/evidence/stage-e/e-shim-001-out-of-core-epoch-adapter.md`（新增，本证据文档）

注：`crates/nlos-compat-shim/Cargo.toml`、工作区根 `Cargo.toml` 与 `Cargo.lock` 保持零改动。

### 3.2 可复现验证命令与测试计数

在仓库根目录执行以下命令复现全部验证门禁：

```powershell
cargo fmt -p nlos-compat-shim -- --check
cargo clippy --offline -p nlos-compat-shim --all-targets -- -D warnings
cargo test --offline -p nlos-compat-shim
```

实测门禁结果：

1. **格式门禁**：`cargo fmt -p nlos-compat-shim -- --check` 退出码 `0`（零格式差异）。
2. **静态与安全门禁**：`cargo clippy --offline -p nlos-compat-shim --all-targets -- -D warnings` 退出码 `0`（在工作区 `#![forbid(unsafe_code)]`、`clippy::all`、`clippy::pedantic` 规则下 `0` 警告、`0` 错误）。
3. **单元、集成与对抗测试门禁**：`cargo test --offline -p nlos-compat-shim` 退出码 `0`，合计 **`40` 项测试全部通过（`0` 失败、`0` 忽略）**：
   - `src/lib.rs` 单元测试：`1` 项通过（`tests::crate_name_matches_cargo_pkg_name`）。
   - [mount_and_protocol_translation.rs](../../../crates/nlos-compat-shim/tests/mount_and_protocol_translation.rs)：`15` 项通过（覆盖 `CompatMountPlanner`、`SabiEpochAdapterCell`、未知 Protobuf wire tag round-trip、`SabiShimBridge` 双传输通道及 `DriverProtocolShim` 1:1 双向转换）。
   - [drv_shim_adversarial_isolation.rs](../../../crates/nlos-compat-shim/tests/drv_shim_adversarial_isolation.rs)：`24` 项通过（覆盖四层端到端基线、TCB 非 canonical 纪元直连拦截、`[DRV-CRED-001]` 零秘密泄漏、`[DRV-SHIM-001]` 8 类伪造/篡改/吊销/越权攻击、`[DRV-BOUND-001]` 与 `[DRV-ACTIVE-001]` 8 类预留/签名用量回执/化身代际围栏攻击、`[TASK-PROVIDER-001]` 隔离证明防提权、`[DRV-LIFE-001]` 与 `[KABI-UNCERTAIN-001]` 崩溃隔离与原幂等键对账，以及核心 crate `Cargo.toml` 零反向依赖静态审计）。

---

## 4. 诚实非声明边界（Honest Non-Claims，`Claim <= Evidence`）

1. **保证等级边界（`H4 != H7`）**：本证据包所有结论均建立在单机确定性宿主进程内多权威 SQLite 集成与故障注入测试（`H4`）之上，不声称已完成跨物理节点生产集群或真实操作系统独立沙箱容器（如 Linux `seccomp`/`namespaces`、Windows AppContainer、macOS Seatbelt）下的长稳压测（`H6` / `H7`）。
2. **工作包 `E2` 右半片边界（`nlos-migrate` 独立车道）**：声明式契约扫描、弃用升级建议与 `PKG-UPDATE-001` 数据容器迁移分类校验 CLI（`crates/nlos-migrate`）归属 `W78` 并行车道实现，不在本证据包声称范围内；里程碑 `M-E2` 须待 `W78` 全部并行车道经合流复验后方可整体闭合。
3. **`SABI v1.0` 正式长期冻结门禁边界**：尽管 `9/9` SABI 通道已在 `W77` 完成线级入冻且本车道落地了核外 `N-1 / N-2 <-> N` 垫片转换，SABI 全表面仍维持 `ADR-0014` / `ADR-0027` §2.1.5 规定的候选护栏态，须待 `E3` 一致性认证套件、`E4` 五语言 SDK 与 `W81` 决策点 `E2`（`[ROAD-E-001]` 跨主系统零重编译升级门禁与维护者签字）验收通过后，方可正式宣告 `SABI v1.0` 长期冻结并闭合 `RISK-B-05`。
