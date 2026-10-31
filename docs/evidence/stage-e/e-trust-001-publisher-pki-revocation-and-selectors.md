# E-TRUST-001：三级发布者 PKI 信任链、单调 `RevocationEpoch` 吊销账本、`ApplicationId` 防抢注归属与跨生态 7 类选择器及 G3 条件授权物化门控证据（`W79-E4`）

- **Evidence ID**: `e-trust-001-publisher-pki-revocation-and-selectors`
- **Wave / Lane**: `W79 / W79-E4-PUBLISHER-TRUST`（里程碑 `M-E3` 左车道，工作包 `E4`）
- **日期**: `2026-10-31`
- **Assurance Level**: `H4`（确定性单机宿主单元测试与真实 `SQLite WAL+FULL` 多权威跨组件集成及对抗证伪测试；不声称 `H7` 生产环境证明）
- **状态**: `VERIFIED`（经 `W79-INT` 三车道合流与跨 crate 端到端集成门禁 [e2e_trust_store_certify_pipeline.rs](../../../crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs) 联合复验通过，晋升 canonical `DONE`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §6 / §7（`TCB-DOWNGRADE-001`、`CAP-UNFORGE-001`、`CAP-ATTEN-001`、`CAP-REVOKE-001`）、§23.1–§23.2（`PKG-IMM-001`、`PKG-SEP-001`、`PKG-APPID-001`）、§24.4（`DUI-OFFLINE-001`）、§25.1–§25.2（`PLAN-DEPENDENCY-001`、`PLAN-LAZY-001`）
  - [ADR-0027 阶段 E SABI v1.0 长期兼容制度、核外兼容垫片与生态商店治理架构](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.3、§5.2（`RISK-B-09`、`RISK-E-03`、`RISK-E-05`、`U-7`、`U-8`）
  - [ADR-0022 受控操作员根能力签发与密钥托管边界](../../management/adrs/0022-operator-root-capability-issuance.md)
  - [ADR-0016 TaskPlan/TaskNode 声明式表面与物化门](../../management/adrs/0016-task-plan-declaration-surface.md)
  - [stage-e-progress.md](../../management/stage-e-progress.md)（工作包 `E4`、里程碑 `M-E3`）与 [exit-unknown-risks.md](../../management/exit-unknown-risks.md)（`U-7`、`U-8`）

---

## 1. 三级发布者信任域、分层 Ed25519 PKI 与 KEK 托管中继签发（`crates/nlos-publisher-trust`）

在保持跨车道冻结共享接口 [types.rs](../../../crates/nlos-publisher-trust/src/types.rs) 零字节改动的前提下，于 [crates/nlos-publisher-trust](../../../crates/nlos-publisher-trust/src/lib.rs) 中以 100% 安全 Rust（`#![forbid(unsafe_code)]`、`clippy::all` + `clippy::pedantic` `-D warnings` 零警告）交付完整持久权威实现（[schema.rs](../../../crates/nlos-publisher-trust/src/schema.rs)、[pki.rs](../../../crates/nlos-publisher-trust/src/pki.rs)、[revocation.rs](../../../crates/nlos-publisher-trust/src/revocation.rs)、[ownership.rs](../../../crates/nlos-publisher-trust/src/ownership.rs)、[authority.rs](../../../crates/nlos-publisher-trust/src/authority.rs)）：

1. **三层密码学证书链与信任等级物理隔离（`ADR-0027` §2.3、`[TCB-DOWNGRADE-001]`）**：
   - `Offline Platform Root CA`（[OfflinePlatformRootCa](../../../crates/nlos-publisher-trust/src/pki.rs)） -> `Store Intermediate Signing CA`（[StoreIntermediateCertificate](../../../crates/nlos-publisher-trust/src/pki.rs)，携带 `max_issuable_tier` 上界约束） -> `Publisher Leaf Certificate`（[PublisherLeafCertificate](../../../crates/nlos-publisher-trust/src/pki.rs)，绑定 `(PublisherId, key_generation, trust_tier, valid_from_ms, valid_until_ms)`）。
   - 商店中继签发私钥全程驻留于 `nlos_capability::SigningKeyCustodyVault`（KEK 封装落盘，私钥种子零明文越界导出），通过 `issue_leaf_under_custody`、`rotate_leaf_under_custody`、`sign_under_custody` 与 `co_sign_under_custody` 推进单调 `sign_seq` 审计收据。
   - **防提权硬门禁**：自签名开发者证书仅允许落入 `PublisherTrustTier::CommunitySandboxed`（对应 `SandboxProfile::DualLayerProcessWasmtime` 与 `Z4TrustPresentation::UnverifiedDowngraded`）；任何自签名证书声明 `CertifiedPublisher` 或 `PlatformCore`，或任何仅获授 `CertifiedPublisher` 上界的中继 CA 越权签发 `PlatformCore` 叶证书，均在验签入库前以 `TrustVerificationError::SelfSignedTierEscalationForbidden` / `TrustTierEscalationForbidden` fail-closed 拦截。
2. **发布者密钥代际轮换与旧钥双态处置（[PublisherKeyRotationReceipt](../../../crates/nlos-publisher-trust/src/pki.rs)）**：
   - 支持双叶自证轮换（`DualSignedByOldAndNewLeaf`，旧活跃私钥 + 新私钥联合签名）与旧钥失陷场景下的中继 CA 托管背书轮换（`IntermediateCaAttested`，经 `SigningKeyCustodyVault` 签名）。
   - 强制 `new_generation == old_generation + 1`；支持将旧叶证书置为 `RotatedKeyDisposition::RetireVerifyOnly`（保留历史已装包脱机验签能力，禁止新签发）或 `RotatedKeyDisposition::RevokeCompromised`（立即标记失陷吊销并阻断后续验证）。
3. **单调 `RevocationEpoch` 签名吊销列表、防回滚/防冻结与 `[DUI-OFFLINE-001]` 脱机自治区分（[revocation.rs](../../../crates/nlos-publisher-trust/src/revocation.rs)、[authority.rs](../../../crates/nlos-publisher-trust/src/authority.rs)）**：
   - `SignedRevocationList` 携带严格单调递增的 `RevocationEpoch`、`issued_at_ms`、`next_update_deadline_ms` 与根 CA 或未吊销中继 CA 签名；`SQLite` 表 `trust_revocation_state` 同时由事务 CAS 与 `trust_revocation_state_no_rollback` 触发器双重阻断任何 `epoch < current_epoch` 的回滚重放（`TrustVerificationError::RevocationEpochRollback`），同纪元不同载荷触发 `IdempotencyConflict`。
   - **在线安装/升级 vs 已安装应用脱机启动显式分流**：
     - 在线安装、升级与上架校验（`PublisherTrustVerifier::verify_package_release` / `TrustFreshnessMode::OnlineInstallOrUpgrade`）：当 `at_ms > next_update_deadline_ms` 时强制以 `TrustVerificationError::RevocationMetadataStale` 拦截 TUF 冻结攻击；
     - 已安装缓存应用本地脱机启动（`SqlitePublisherTrustAuthority::verify_cached_offline_launch` / `TrustFreshnessMode::OfflineInstalledLaunch`，`[DUI-OFFLINE-001]`）：不因断网导致本地 CRL 超过 `next_update_deadline_ms` 而瘫痪本地工作流，同时继续强制执行本地已知的全部叶证书/中继 CA 吊销记录、证书有效期与 `ApplicationId` 归属校验。
4. **`ApplicationId` 首次注册防抢注、三方签名归属转移与三元身份严格分离（`[PKG-APPID-001]`、`[PKG-SEP-001]`、`[PKG-IMM-001]`）**：
   - `application_ownership_bindings` 将 `ApplicationId` 持久钉死至首次合法注册的 `PublisherId`（`transfer_epoch = 0`），任何异构 `PublisherId` 试图绑定同一 `ApplicationId` 一律返回 `TrustVerificationError::ApplicationIdHijackAttempt`。
   - `SQLite` 触发器 `application_ownership_bindings_transfer_gated` 物理禁止未推进 `transfer_epoch = OLD.transfer_epoch + 1` 或缺失 `last_transfer_receipt_id` 的原地篡改；唯一合法变更路径 `transfer_application_ownership` 强制校验旧发布者叶证书、新发布者叶证书与商店中继 CA 的**三方角色分离 Ed25519 联合签名**（`SignedOwnershipTransferReceipt`）。
   - `bind_package_release_for_application` 与 `verify_installation_identity_separation` 强制校验 `PackageId != ApplicationId != InstallationId`（违反返回 `IdentitySeparationViolation`）及同 `(PackageId, version)` 内容摘要不可变性（违反返回 `ImmutableReleaseDigestConflict`）。

---

## 2. 跨生态 7 类 Typed Selector 不可变句柄解析（闭合 `U-8`）与 G3 条件授权物化门控（闭合 `U-7`）

在不修改 `crates/nlos-plan` 任何既有核心文件（`SqlitePlanAuthority` 保持 `SCHEMA_VERSION == 9` 零侵入）的前提下，于 [ecosystem_selector.rs](../../../crates/nlos-plan/src/ecosystem_selector.rs) 中实现独立 `SQLite`（`WAL + FULL`、`STRICT` 表、写一次防篡改触发器）门控引擎 `SqliteEcosystemDependencyGate`：

| 规范与遗留项 | 核心实现机制 | 负向对抗证伪拦截（Fail-Closed） |
| :--- | :--- | :--- |
| **`[PLAN-DEPENDENCY-001]` / `U-8`（7 类跨生态选择器锁定）** | 完整覆盖 `PlanDependencyEntityKind` 全部 7 类实体（`Package`、`Skill`、`Tool`、`Model`、`Artifact`、`Topic`、`ExternalService`）；`bind_node_dependency_proposals` 与 `resolve_node_dependency` 通过 `CrossEcosystemEntitySource` 解析并持久化不可变 `PinnedEcosystemDependencyHandle`（锁定 `pinned_version_tag`、`pinned_generation >= 1`、`content_digest`、`provenance` 与 `ReceiptId`） | 1. `parse_version_pin` 与绑定入口拦截 `"latest"`、`"*""`、`"head"`、`"main"`、`"canary"`、`"nightly"`、空串、通配符/范围及 `ExactGeneration(0)`，返回 `EcosystemGateError::FloatingVersionForbidden`（`0` 条持久化写入）；<br>2. 拦截 `SelectorProvenance::UnpinnedSearchResult`（`发现 != 授权`），返回 `UnpinnedSearchResultForbidden`；<br>3. 物化前实时复验句柄，拦截未绑定清单（`NodeDependenciesUnbound`）、部分未解析（`UnresolvedEcosystemDependencies`）、上游紧急撤回/吊销（`PinnedDependencyRevokedOrRecalled`）及静默代际/摘要漂移（`PinnedDependencyFenceDrifted`）。 |
| **`[PLAN-LAZY-001]` / `U-7`（G3 `WaitingAuthorization` 四门条件物化）** | `advance_and_authorize_materialization` 先校验上游 DAG 节点全部 `Completed` 与 7 类生态句柄实时有效，将就绪节点推进至 `PlanNodeState::WaitingAuthorization`，随后依次强制评估：(1) `CapabilityAuthority` 实时能力核验；(2) `NodeConditions.namespace` 命名空间作用域；(3) `NodeConditions.resource_contract`（`cpu_shares` / `memory_mib` / `io_weight`）配额上限；(4) `NodeConditions.fanout`（同时核查计划 DAG 真实下游依赖扇出数 `dag_downstream_fanout <= max_downstream_fanout` 与 `effective_downstream_fanout = max(declared, dag)` 的付费主体 `payer_principal_id`）；最后联动 `SqliteTaskAuthority` `AdmissionConsult`，仅在全部通过时原子签发 `WaitingAuthorization -> Materializing` 凭证与 `ConditionedGateOutcome::Approved` 审计收据 | 1. 上游 DAG 依赖未 `Completed` -> 节点推进至 `BlockedDependency` 并以 `PlanStoreError::DependenciesNotReady` 拒绝；<br>2. 节点缺失结构化 `NodeConditions` -> 返回 `MissingStructuredNodeConditions`；<br>3. 真实 `CapabilityAuthority` 能力缺失或已被 `revoke_signed` 吊销、`NamespaceId` 越界、`ResourceContractCondition` 超限、DAG 实际下游扇出超限或缺失付费主体、以及 `SqliteTaskAuthority` 工作集满载（`WorkingSetFull`）：节点**严格停留于 `PlanNodeState::WaitingAuthorization`**（零 `MaterializationApproval` 凭证泄漏），并在 `conditioned_authorization_audit_receipts` 落盘精确类型化拒绝收据。 |

---

## 3. 验证门禁结果与复现命令

### 3.1 本车道独占写集清单

1. [crates/nlos-publisher-trust/src/lib.rs](../../../crates/nlos-publisher-trust/src/lib.rs)、[schema.rs](../../../crates/nlos-publisher-trust/src/schema.rs)、[pki.rs](../../../crates/nlos-publisher-trust/src/pki.rs)、[revocation.rs](../../../crates/nlos-publisher-trust/src/revocation.rs)、[ownership.rs](../../../crates/nlos-publisher-trust/src/ownership.rs)、[authority.rs](../../../crates/nlos-publisher-trust/src/authority.rs)（注：[types.rs](../../../crates/nlos-publisher-trust/src/types.rs) 与 `Cargo.toml` 保持零改动）
2. [crates/nlos-publisher-trust/tests/publisher_trust_gate.rs](../../../crates/nlos-publisher-trust/tests/publisher_trust_gate.rs)
3. [crates/nlos-plan/src/ecosystem_selector.rs](../../../crates/nlos-plan/src/ecosystem_selector.rs)（注：`crates/nlos-plan` 其余既有源码与 `Cargo.toml` 保持零改动）
4. [crates/nlos-plan/tests/ecosystem_selector_gate.rs](../../../crates/nlos-plan/tests/ecosystem_selector_gate.rs)
5. `docs/evidence/stage-e/e-trust-001-publisher-pki-revocation-and-selectors.md`（本文件）

### 3.2 门禁执行结果

1. **格式检查门禁**：
   `cargo fmt -p nlos-publisher-trust -p nlos-plan -- --check` -> 退出码 `0`。
2. **Clippy 严格门禁（含下游共享契约消费方联编校验）**：
   `cargo clippy --offline -p nlos-publisher-trust -p nlos-plan -p nlos-store-registry -p nlos-certify --all-targets -- -D warnings` -> 退出码 `0`（`#![forbid(unsafe_code)]`、`clippy::all`、`clippy::pedantic` 零警告）。
3. **`nlos-publisher-trust` 单元与集成对抗测试门禁**：
   `cargo test --offline -p nlos-publisher-trust` -> 退出码 `0`：
   - 库内单元测试（`types::tests`）：`4` 项通过，`0` 失败；
   - [publisher_trust_gate.rs](../../../crates/nlos-publisher-trust/tests/publisher_trust_gate.rs)：`7` 项通过，`0` 失败（覆盖三级证书链与防提权、双模式密钥轮换、单调 `RevocationEpoch` 防回滚/防冻结与 `[DUI-OFFLINE-001]` 脱机启动、`[PKG-APPID-001]` 防抢注与三方签名归属转移、中继 CA 失陷级联失效、SQLite 重启持久性与触发器防篡改）；
   - 合计 `11` 项测试全部通过。
4. **`nlos-plan` 全量回归与跨生态选择器 / G3 条件物化门测试门禁**：
   `cargo test --offline -p nlos-plan` -> 退出码 `0`：
   - `ecosystem_selector::tests` 库内单元测试：`2` 项通过，`0` 失败；
   - [ecosystem_selector_gate.rs](../../../crates/nlos-plan/tests/ecosystem_selector_gate.rs)：`7` 项通过，`0` 失败（覆盖 7 类实体句柄解析/幂等重放/重启持久性/`SqlitePlanAuthority` v9 零侵入校验、浮动版本与未锁定搜索结果零写入拦截、物化前未绑定/未解析/紧急撤回/代际漂移拦截、上游 DAG 未完成 `BlockedDependency` 阻断、真实 `CapabilityAuthority` 吊销拦截、`Namespace` / `ResourceContract` / `Fanout` / `SqliteTaskAuthority` 四维停留 `WaitingAuthorization` 审计、以及授权恢复后 `WaitingAuthorization -> Materializing` 全绿跃迁）；
   - `nlos-plan` 全套既有与新增测试合计 `120` 项通过、`0` 失败（另 `3` 项显式大规模压力探针按默认策略 `ignored`），零回归。

---

## 4. 诚实非声明边界（Honest Non-Claims）

1. **保证等级与合流状态边界（`H4 != H7`，`VERIFIED`）**：本文档所有证据均来自单机确定性宿主测试与真实本地 `SQLite WAL+FULL` 多权威沙箱（`H4`），并已由 `W79-INT` 跨车道端到端集成门禁 [e2e_trust_store_certify_pipeline.rs](../../../crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs) 联合复验晋升 `VERIFIED`，不声称跨物理集群或大规模在线商店运营的 `H7` 生产环境证明。
2. **OCI 商店目录与四门上架认证流水线边界（并行车道 `W79-E5A` / `W79-E5B` 交付）**：本车道（`W79-E4`）提供底层 `PublisherTrustVerifier` 证书链、单调 `RevocationEpoch`、`ApplicationId` 归属账本及 `nlos-plan` 7 类生态选择器与 G3 条件授权物化门；在此之上运行的 OCI 内容寻址商店目录 `crates/nlos-store-registry` 与四门自动化认证流水线 `crates/nlos-certify` 分别由同波并行车道 `W79-E5A-STORE-REGISTRY` 与 `W79-E5B-CERTIFY-PIPELINE` 独立交付并在 `W79-INT` 合流。
3. **宿主 OS 商业安装器公证与双层 Wasm 强沙箱边界**：内部 `Ed25519` 三级发布者 PKI 用于验证 `llmos.package` 生态包身份与信任等级，不冒充外部商业 CA 硬件令牌签发的 Windows Authenticode 或 Apple Developer ID 宿主安装器公证（`E-deferred`）；针对 `CommunitySandboxed` 不可信负载的三平台 `Process + Wasmtime` 双层强沙箱运行时执行底座归属后续波次 `W80`（工作包 `E6`，`U-9`）。
