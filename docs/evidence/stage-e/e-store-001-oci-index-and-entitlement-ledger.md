# E-STORE-001：内容寻址 OCI 工件校验层、持久化应用商店治理目录、不透明许可令牌验证缝与三级恶意包撤回联动账本证据（`W79-E5A`）

- **Evidence ID**: `e-store-001-oci-index-and-entitlement-ledger`
- **Wave / Lane**: `W79 / W79-E5A-STORE-REGISTRY`（里程碑 `M-E3` 中车道，工作包 `E5` 左半部分）
- **日期**: `2026-10-31`
- **Assurance Level**: `H4`（纯确定性本地宿主单元测试、真实多权威 `SQLite` `WAL + FULL` 跨库集成验证、重启持久性恢复测试与 DDL 触发器防篡改验证；不声称 `H7` 生产环境证明）
- **状态**: `VERIFIED`（经 `W79-INT` 三车道合流与跨 crate 端到端集成门禁 [e2e_trust_store_certify_pipeline.rs](../../../crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs) 联合复验通过，晋升 canonical `DONE`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §11（`CAP-REVOKE-001`）、§16 / §22（`TUI-CONSENT-001`、`DUI-OFFLINE-001`、`TCB-EGRESS-001`）、§23.1–§23.3（`PKG-MULTI-001`、`PKG-IMM-001`、`PKG-SEED-001`、`PKG-APPID-001`、`PKG-OCI-001`）、§29.1（`NLOS-DEFER-001`）
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.3、§3、§4
  - [stage-e-progress.md](../../management/stage-e-progress.md)（工作包 `E5`、里程碑 `M-E3`、风险项 `RISK-E-03`、`RISK-E-04`）
  - [e-migrate-001-static-checker-and-rollbacks.md](e-migrate-001-static-checker-and-rollbacks.md)（`W78` 多组件包清单与副作用分级基线）

---

## 1. 内容寻址 OCI 工件层校验器与商店治理层彻底解耦（`[PKG-OCI-001]`）

在 [crates/nlos-store-registry/src/oci_cas.rs](../../../crates/nlos-store-registry/src/oci_cas.rs) 与 [error.rs](../../../crates/nlos-store-registry/src/error.rs) 中以 100% 安全 Rust（`#![forbid(unsafe_code)]`）实现本地内容寻址存储 `LocalOciCasStore`：

1. **原始字节流 `SHA-256` 与字节大小双重逐位门禁**：
   - `OciLayerDescriptor` 严格绑定 `(media_type, sha256_digest, size_bytes)`，拒绝空串、含空白或控制字符的非法 `media_type`（`OciCasError::InvalidMediaType`）、长度不匹配（`OciCasError::LayerSizeMismatch`）及单比特翻转篡改（`OciCasError::LayerDigestMismatch`）。
   - `OciPackageLayout::from_verified_streams` 对 `config` 与非空有序 `layers` 全量复算校验后，采用域分离标签 `nlos/oci-cas/package-manifest/v1` 计算确定性 `oci_manifest_digest: [u8; 32]`，并通过临时文件原子重命名落盘至 `<cas_root>/blobs/sha256/<hex>` 与 `<cas_root>/manifests/sha256/<hex>`。
   - `LocalOciCasStore::verify_stored_layout` 在读取时重新加载磁盘全部层字节流并复算 `oci_manifest_digest`，对落盘后的磁盘静默比特损坏或截断立即 fail-closed 拦截。
2. **传输存储与治理授权物理隔离（`[PKG-OCI-001]`）**：
   - `LocalOciCasStore` 不持有任何数据库连接、渠道状态或许可记录；仅将 OCI 层与清单写入 `CAS` 目录绝不自动产生任何商店目录上架行、渠道晋升或安装授权。
   - 反向门禁上，`SqliteStoreRegistry::submit_release` 强制调用 `cas.verify_stored_layout` 并核对每个组件槽位的 `(content_sha256, size_bytes)` 均存在于已验证的 OCI 层集合中，缺失或篡改时零脏写拒绝入库。

---

## 2. 多组件复合包目录、`ApplicationId` 防劫持、双轨渠道隔离与权限透明度差异（`[PKG-MULTI-001]`、`[PKG-APPID-001]`、`[TUI-CONSENT-001]`）

在 [catalog.rs](../../../crates/nlos-store-registry/src/catalog.rs)、[transparency.rs](../../../crates/nlos-store-registry/src/transparency.rs)、[schema.rs](../../../crates/nlos-store-registry/src/schema.rs) 与 [registry.rs](../../../crates/nlos-store-registry/src/registry.rs) 中构建 `SQLite`（`WAL + FULL`、`STRICT` 表与不可变 DDL 触发器）持久化治理目录：

1. **五类组件槽位复合应用建模（`[PKG-MULTI-001]`）**：
   - `StoreComponentKind` 完整覆盖 `Executable`、`Driver`、`UiSurface`、`BackgroundService`、`ReadOnlySeedTemplate` 五类组件角色，支持单个 `(PackageId, package_version)` 复合包在单笔 `Immediate` 事务内原子登记多个组件槽位与一个或多个 `ApplicationId`。
2. **`PublisherTrustVerifier` Trait 对接与 `ApplicationId` 跨发布者防劫持（`[PKG-APPID-001]`、`RISK-E-03`）**：
   - 通过跨 crate 冻结接口 `&dyn nlos_publisher_trust::PublisherTrustVerifier`（参见 [crates/nlos-publisher-trust/src/types.rs](../../../crates/nlos-publisher-trust/src/types.rs)）验证发布者对域分离摘要 `nlos/store-registry/release-submission/v1` 的 `Ed25519` 签名与信任等级。
   - 在任何目录表写入前先完成全部声明 `ApplicationId` 的本地归属核查与 `verify_or_bind_application_owner` 绑定：跨发布者抢注既有 `ApplicationId` 立即返回 `PublisherTrustCheckError::ApplicationIdHijackAttempt`，事务自动回滚，确保磁盘上零残留 `store_releases` 行与零半截归属绑定；同时由 `trg_store_app_owners_no_update` / `trg_store_app_owners_no_delete` 在数据库层永久冻结首次注册归属。
3. **双轨目录隔离与四门禁签名晋升（`ADR-0027` §2.3.1–§2.3.4）**：
   - `PublisherTrustTier::CommunitySandboxed` 发布物强制路由至 `StoreCatalogTrack::CommunityTrack` 的 `Candidate` 渠道；`SQLite` `CHECK (NOT (publisher_trust_tier = 3 AND channel = 2))` 与 `promote_candidate_to_stable` 双重拦截其进入官方 `Stable` 目录（`StoreRegistryError::CommunityPublisherForbiddenInStable`）。
   - `PlatformCore` 与 `CertifiedPublisher` 候选包仅在出示由受信任 `certifier_public_key` 严格 `Ed25519` 签名、处于有效期内、全部四道认证门禁位掩码通过（`passed_gates_mask == 0b1111`）且 `(package_id, application_id, package_version, manifest_digest, oci_manifest_digest, publisher_id)` 逐项绑定的 `SignedCertificationAttestation` 时，方可原子晋升为 `StoreChannel::Stable` 并落盘不可变 `StoreCertificationPromotionReceipt`（支持 `IdempotencyKey` 精确一次重放）。
4. **权限与用途透明度账本及跨版本提权检测（`[TUI-CONSENT-001]`、`[TCB-EGRESS-001]`）**：
   - 持久化每个版本的 `(capability_scope, human_purpose, effect_class, network_egress_declared, sandbox_profile)` 并导出 `PermissionTransparencyReport`。
   - `diff_permission_transparency` / `diff_release_permission_transparency` 确定性比对前后版本：凡出现新增 capability 作用域、既有或新增作用域开启外部网络出站（`newly_enabled_network_egress`）、副作用等级上升（尤其升级为 `EffectClass::Irreversible`）、`human_purpose` 变更或 `sandbox_profile` 变更，一律置 `requires_tui_reconsent = true` 以阻断静默升级并触发 `Z4` Trusted UI 重新确认；纯权限缩减或幂等不变版本则置 `requires_tui_reconsent = false`。

---

## 3. 不透明 `EntitlementToken` 本地离线验签与席位核销缝（`NLOS-DEFER-001`）

在 [entitlement.rs](../../../crates/nlos-store-registry/src/entitlement.rs) 与 [registry.rs](../../../crates/nlos-store-registry/src/registry.rs) 中严格遵循 `v0.5` §29.1 与 `ADR-0027` §4 边界实现 `SignedEntitlementToken` 与 `verify_and_redeem_entitlement`：

- **纯密码学与席位账本边界**：严格校验商店签发密钥 `KeyId`、域分离（`nlos/store-registry/entitlement-token/v1`）`Ed25519` 严格签名、`[issued_at_ms, expires_at_ms]` 有效期窗口、调用者 `PrincipalId` 与目标 `ApplicationId` 绑定、本地 `store_revocation_epoch >= min_revocation_epoch` 新鲜度下界、目标发布物非撤回状态、渠道准入级别（`Stable` 令牌拒绝未认证 `Candidate` 包）以及 `max_seat_or_install_count` 席位上限。
- **精确一次幂等核销**：在单笔 `Immediate` 事务内递增 `store_entitlement_usage`（受 `trg_store_entitlement_usage_monotonic` 防回退触发器保护）并写入不可变 `EntitlementGrantReceipt`；相同 `IdempotencyKey` 重放原样返回既有收据且绝不重复扣减席位，篡改参数重用幂等键则以 `IdempotencyConflict` 拒绝。
- **零商业计费越界**：全模块零法币定价、零支付网关 SDK、零订阅账单与税务逻辑。

---

## 4. 三级恶意包撤回（`RecallAdvisory`）与 `CapabilityAuthority` / `ApplicationAuthority` 跨权威联动（`[CAP-REVOKE-001]`、`[DUI-OFFLINE-001]`）

在 [recall.rs](../../../crates/nlos-store-registry/src/recall.rs) 与 [registry.rs](../../../crates/nlos-store-registry/src/registry.rs) 中实现安全权威 `Ed25519` 签名 `SignedRecallAdvisory`、单调递增 `store_revocation_epoch` 防重放/防回滚与防降级状态机：

| 撤回等级 (`RecallSeverity`) | 商店目录与新安装门禁 | 本地已安装离线实例与跨权威执行动作 |
| :--- | :--- | :--- |
| `WarnNewInstall`（Level 1） | 记录激活告警，`evaluate_new_install_gate` 与 `verify_and_redeem_entitlement` 立即阻断新下载与新席位核销 | `evaluate_local_launch_gate` 返回 `AllowedWithRiskWarning`，既有 Capability 句柄与本地应用保持离线可用（`[DUI-OFFLINE-001]`） |
| `BlockLaunchAndRevokeCap`（Level 2） | 目标版本（或全包所有版本）原子切换至 `StoreChannel::Recalled` 并从官方 `Stable` 列表剔除 | 阻断本地新启动，并通过 `RecallEnforcementAuthorities` 驱动真实 `nlos_capability::CapabilityAuthority::revoke_signed` 立即吊销已登记的全部 `CapabilityId`（世代推进并栅栏化：当前世代 `consume` 立即返回 `CapabilityRevoked`，旧世代句柄返回 `GenerationFenceConflict`，派生子句柄返回 `AncestorRevokedOrFenced`） |
| `QuarantineInstallation`（Level 3） | 包含 Level 2 全部目录撤回与阻断语义 | 在完成 Level 2 全量 Capability 吊销的基础上，调用真实 `nlos_application::ApplicationAuthority::disable_application` 将本地安装状态原子隔离为 `ApplicationStatus::Disabled` 并落盘 `DisableReceipt`，同时 100% 字节级保留用户数据目录不做静默销毁 |

---

## 5. 验证门禁结果与复现命令

### 5.1 本车道独占写集清单

1. [crates/nlos-store-registry/src/lib.rs](../../../crates/nlos-store-registry/src/lib.rs)、[error.rs](../../../crates/nlos-store-registry/src/error.rs)、[oci_cas.rs](../../../crates/nlos-store-registry/src/oci_cas.rs)、[transparency.rs](../../../crates/nlos-store-registry/src/transparency.rs)、[catalog.rs](../../../crates/nlos-store-registry/src/catalog.rs)、[entitlement.rs](../../../crates/nlos-store-registry/src/entitlement.rs)、[recall.rs](../../../crates/nlos-store-registry/src/recall.rs)、[schema.rs](../../../crates/nlos-store-registry/src/schema.rs)、[registry.rs](../../../crates/nlos-store-registry/src/registry.rs)
2. [crates/nlos-store-registry/tests/store_registry_gate.rs](../../../crates/nlos-store-registry/tests/store_registry_gate.rs)
3. `docs/evidence/stage-e/e-store-001-oci-index-and-entitlement-ledger.md`（本文件）

> 注：根 `Cargo.toml`、`Cargo.lock`、[crates/nlos-store-registry/Cargo.toml](../../../crates/nlos-store-registry/Cargo.toml) 以及兄弟车道 `crates/nlos-publisher-trust`、`crates/nlos-certify` 在本车道保持零改动。

### 5.2 门禁执行结果

1. **格式检查门禁**：
   `cargo fmt -p nlos-store-registry -- --check` -> 退出码 `0`。
2. **Clippy 严格门禁**：
   `cargo clippy --offline -p nlos-store-registry -p nlos-publisher-trust -p nlos-certify --all-targets -- -D warnings` -> 退出码 `0`（`#![forbid(unsafe_code)]`、`clippy::all`、`clippy::pedantic` 零警告）。
3. **Rust 单元与跨权威集成测试门禁**：
   `cargo test --offline -p nlos-store-registry` -> 退出码 `0`：
   - 库内单元测试（[src/lib.rs](../../../crates/nlos-store-registry/src/lib.rs)）：`4` 项通过，`0` 失败（覆盖 schema 主版本常量、全部封闭枚举编解码往返与非法值拒绝、OCI 清单摘要顺序/内容敏感性、纯权限衰减免重弹 `Z4` 确认）；
   - 端到端集成测试（[tests/store_registry_gate.rs](../../../crates/nlos-store-registry/tests/store_registry_gate.rs)）：`6` 项通过，`0` 失败：
     - `test_01_oci_cas_layer_tamper_rejection_and_store_governance_decoupling`
     - `test_02_multi_component_catalog_appid_anti_hijack_community_isolation_and_four_gate_promotion`
     - `test_03_permission_transparency_ledger_and_cross_version_escalation_diff`
     - `test_04_opaque_signed_entitlement_token_offline_verification_and_seat_accounting`
     - `test_05_three_tier_recall_advisory_capability_revocation_application_quarantine_and_restart_recovery`
     - `test_06_governance_anchor_mismatch_and_sqlite_ddl_immutability_triggers`
   - 文档测试（doc-tests）：`0` 失败；
   - 合计 `10` 项 Rust 测试全部通过。

---

## 6. 诚实非声明边界（Honest Non-Claims）

1. **保证等级边界（`H4 != H7`）**：本文档证据均来自单机确定性宿主测试与真实本地 `SQLite` 权威组件联动验证（`H4`），不声称覆盖跨广域网 CDN 分发、硬件 HSM 根密钥仪式或大规模在线商店运营的 `H7` 生产证明。
2. **并行车道职责边界**：发布者根/中级/叶三级离线/在线 PKI 证书链签发器与 CRL 权威库由并行左车道 `W79-E4-PUBLISHER-TRUST` 在 `crates/nlos-publisher-trust` 中独立交付（本车道仅消费其冻结 `PublisherTrustVerifier` trait）；实际驱动四道自动化认证流水线并签发四门禁认证报告的执行引擎由并行右车道 `W79-E5B-CERTIFY` 在 `crates/nlos-certify` 中独立交付（本车道负责独立 `Ed25519` 验签四门禁证明并门控 `Candidate -> Stable` 渠道跃迁）。
3. **法币计费与商业运营延后边界（`NLOS-DEFER-001`）**：`SignedEntitlementToken` 仅提供不透明签名许可的本地密码学验签、吊销纪元新鲜度校验与有限席位幂等核销缝，不包含任何定价、支付结算、发票或订阅续费系统。
