# E-SECURITY-001：供应链与吊销 H7 对抗回归六类攻击矩阵证据（`W80-E6D`）

- **Evidence ID**: `e-security-001-h7-supply-chain-and-revocation-adversarial`
- **Wave / Lane**: `W80 / W80-E6D-SUPPLYCHAIN-H7`（工作包 `E6` 第 4 条：`H7` 供应链与沙箱对抗回归套件中的供应链/吊销/防抢注/回滚/卸载残留半边）
- **日期**: `2026-11-01`
- **Assurance Level**: `H7`（安全/兼容 conformance 阶梯下的**确定性本地宿主对抗性负向集成测试**：六类攻击输入 100% 拦截、每条负路径断言具体错误变体；不声称生产环境渗透测试、外部红队实证或物理硬件台架）
- **状态**: `VERIFIED`（本车道独占写集内三道验证门全绿；三平台 CI 与 `W80-INT` 合流待波次集成后统一回执）
- **验证平台**: Windows 11 Pro（10.0.28000）、Rust stable（msvc）、仓库独立 git worktree 基线 `0436b789`
- **权威规范与治理依据**:
  - [stage-e-progress.md](../../management/stage-e-progress.md) §E.2.2 `E6` 第 4 条（"执行 `H7` 供应链与沙箱对抗回归套件（覆盖篡改包层、伪造叶证书、重放过期 `RevocationEpoch`、抢注 `ApplicationId`、恶意升级崩溃、卸载权限残留与 Wasm 路径/网络逃逸）"——其中 Wasm 路径/网络逃逸归 `W80-E6A` 车道，本证据只覆盖前六类）与 §E.5.5 退出门禁 2（"伪造发布者证书/篡改 OCI 层/重放旧 `RevocationEpoch`/抢注 `ApplicationId` 100% 拦截"）
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §23.1–§23.3（`PKG-OCI-001`、`PKG-APPID-001`、`PKG-UPDATE-001`、`PKG-UNINSTALL-001`）、§6/§7/§24.2（`CAP-REVOKE-001`）、§13
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.3–§3
  - 前序基座: [E-TRUST-001](e-trust-001-publisher-pki-revocation-and-selectors.md)、[E-STORE-001](e-store-001-oci-index-and-entitlement-ledger.md)、[E-CERTIFY-001](e-certify-001-automated-conformance-and-uninstall-gate.md)（`W79` `H4` 实现）

---

## 1. 交付物与写集边界

本车道单一交付物为新增集成测试文件（`W80` 四车道写集正交编排下独占 `crates/nlos-certify/tests/h7_*` 子树）：

1. [crates/nlos-certify/tests/h7_supply_chain_adversarial.rs](../../../crates/nlos-certify/tests/h7_supply_chain_adversarial.rs)（6 个测试函数 + 真实三 crate 权威夹具）
2. `docs/evidence/stage-e/e-security-001-h7-supply-chain-and-revocation-adversarial.md`（本文件）

零新增依赖；被测 crate 源码（`nlos-certify/src/**`、`nlos-publisher-trust/**`、`nlos-store-registry/**`）、根 `Cargo.toml`/`Cargo.lock`、`docs/management/**` 在本车道零改动——全部六类攻击均在**不改被测源码**的前提下 100% 拦截，无需豁免或补丁。

测试夹具完全对齐 [e2e_trust_store_certify_pipeline.rs](../../../crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs) 惯例：`SigningKeyCustodyVault` 三级 PKI（`OfflinePlatformRootCa -> StoreIntermediateCertificate -> CertifiedPublisher` 双叶证书）落盘 `SqlitePublisherTrustAuthority`、`RevocationEpoch(1)` 初始 `SignedRevocationList`、`LocalOciCasStore` 内容寻址层与 `SqliteStoreRegistry` 治理门，全部为真实权威实现（无 stub/无 mock）。

## 2. 六类攻击矩阵与逐条拦截证据

| # | 攻击类别（§E.5.5 门禁 2 / §E.2.2 `E6` 第 4 条原文） | 测试函数 | 攻击注入方式 | 断言的具体拦截点（错误变体/消息） |
|---|---|---|---|---|
| 1 | 篡改 `OCI` blob（层字节/长度/`SHA-256` 失配） | `h7_attack1_tampered_oci_blob_rejected_by_store_cas_and_certification_gate` | (a) 等长字节替换 CAS 层 blob；(b) 截断 9 字节；(c) 伪造组件槽位摘要引用不存在 blob；(d) 谎报槽位长度 | `LocalOciCasStore::read_verified_blob`/`verify_stored_layout` 返回 `OciCasError::LayerDigestMismatch { declared, computed }`（逐字节比对 computed == `compute_sha256(篡改字节)`）与 `OciCasError::LayerSizeMismatch { declared: 原长, actual: 截断长 }`；`SqliteStoreRegistry::submit_release` 事务前即返回 `StoreRegistryError::OciCas(LayerDigestMismatch / LayerSizeMismatch / BlobMissing { digest })`，且每次拒绝后 `inspect_release` 均为 `None`（零脏行）；对照组合法提交仍为 `Candidate` |
| 2 | 伪造发布者叶证书（非信任链内签发的 Ed25519 证书） | `h7_attack2_forged_publisher_leaf_certificate_rejected_by_publisher_trust_verifier` | (a) 自签证书伪称 `CERTIFIED_PUBLISHER` 层级（自签摘要+有效自签名）；(b) 攻击者自建"中间 CA"签发叶证书；(c) 未注册签名密钥冒充合法 `PublisherId`（密钥替换）；(d) 全未知发布者；(e)/(f) 以伪造身份走商店上架门与认证收据签发门 | `register_leaf_certificate` 精确相等断言 `UntrustedCertificateChain("self-signed leaf certificate is strictly confined to COMMUNITY_SANDBOXED")` 与 `UntrustedCertificateChain("leaf certificate issuer_serial_id is not a registered StoreIntermediateCertificate")`；`PublisherTrustVerifier::verify_publisher_signature` 精确相等断言 `UntrustedCertificateChain("signing public key is not bound to any registered certificate of publisher")` 与 `UnknownPublisher(id)`；`submit_release` 返回 `StoreRegistryError::PublisherTrust(UnknownPublisher)`（零目录行）；`issue_store_certification_receipt` 返回 `CertifyError::PublisherTrust(UnknownPublisher)` |
| 3 | 重放过期/回滚 `RevocationEpoch`（单调纪元回退，SQLite `no_rollback` 触发器） | `h7_attack3_revocation_epoch_rollback_replay_and_sqlite_tamper_rejected` | (a) 纪元升至 2 后重放纪元 1 CRL；(b) 同纪元不同摘要 CRL（equivocation）；(c) 第二个裸 `rusqlite` 连接直写 `UPDATE trust_revocation_state SET revocation_epoch = 1`；(d) 裸 `DELETE FROM revoked_certificates` 反吊销攻击；(e) 商店撤回公告纪元不前进 | (a)/(b) 精确相等断言 `StaleOrRollbackRevocationEpoch { current_epoch: 2, candidate_epoch: 1/2 }`；(c) 触发器 `RAISE(ABORT)` 消息片段 `"revocation_epoch rollback is forbidden"` 且 `inspect_revocation_state` 纪元仍为 2；(d) `"revoked_certificates rows cannot be deleted"` 且被吊销发布者签名验证仍精确返回 `RevokedPublisher { publisher_id, revoked_at_epoch: 2 }`；(e) `StoreRegistryError::RecallEpochRollback { current_epoch: 1, candidate_epoch: 1 }` |
| 4 | 跨发布者抢注已归属 `(PublisherId, ApplicationId)` 应用名 | `h7_attack4_cross_publisher_application_id_squatting_blocked_at_all_gates` | 合法发布者先注册归属后，持有同层级（`CertifiedPublisher`）合法证书的恶意发布者以不同 `PackageId` 抢注同一 `ApplicationId` | 信任权威 `verify_or_bind_application_owner` 精确相等断言 `ApplicationIdHijackAttempt { application_id, registered_owner: 合法, attempted_publisher: 恶意 }`；商店门 `submit_release` 返回 `StoreRegistryError::PublisherTrust(ApplicationIdHijackAttempt{...})`（同构体全等）且恶意包 `inspect_release == None`、合法发布仍为 `Candidate` 不受扰动；认证收据签发门 `issue_store_certification_receipt` 返回 `CertifyError::PublisherTrust(ApplicationIdHijackAttempt{...})`（三字段逐项守卫匹配） |
| 5 | 恶意升级中途崩溃（健康探针失败/半途崩溃） | `h7_attack5_malicious_upgrade_health_fail_and_mid_crash_restore_bitwise_baseline` | 真实样板包 `starter-assistant-app` `v1.0.0 -> v1.1.0` 上运行 `nlos-certify` Gate 3：注入 `HealthCheckFailed` 探针失败与 `MidStepFault` 半写崩溃+`.tmp` 残留；(b) 反向注入"恶意回滚处理器留漂移" | (a) `GateStatus::Pass`、零 findings、健康升级代数 `Some(2)`，且 `health_fail_restored_tree_digest` 与 `mid_step_fault_restored_tree_digest` 均与 `baseline_container_tree_digest` **逐位全等**（`UserDataContainer` SHA-256 树摘要字节级原子回滚）；(b) 漂移注入使门禁 `GateStatus::Fail`，两条 `BLOCKER` 规则均为 `PKG-UPDATE-001`（两个故障子演练各自捕获漂移），回滚摘要与基线**必须不等**证明确实注入成功 |
| 6 | 卸载后权限/后台/缓存残留 | `h7_attack6_uninstall_zero_capability_background_and_cache_residue` | 真实样板包全生命周期 `Install -> Grant -> Start Background Tasks -> Populate Cache -> Uninstall`；(b)/(c) 反向注入"恶意卸载器"跳过能力吊销/保留私有缓存 | (a) 干净卸载：`GateStatus::Pass`、零 findings、活跃任务门控成立、`revoked_capabilities_verified == 2`（卸载后消费全部返回 `CapabilityRevoked`/`GenerationFenceConflict`）、`residual_private_cache_files == 0`、共享 Artifact 完好、用户数据默认 100% 保留且 `Z4` 门控；(b) 缓存残留注入被捕获为 `subject == "gate4/private-cache"` 的 `PKG-UNINSTALL-001` `BLOCKER` 且残差数 > 0；(c) 未吊销能力注入产生恰两条 `CAP-REVOKE-001` `BLOCKER`（每个存活句柄一条）且 `revoked_capabilities_verified == 0` |

全部 6 个测试断言均为**具体错误变体精确匹配**（`PublisherTrustCheckError` 全等比较）或**变体+字段守卫/触发器 `RAISE(ABORT)` 消息片段**（`StoreRegistryError`/`OciCasError`/`CertifyError` 因不派生 `PartialEq` 采用 `matches!` 字段守卫），不存在任何裸 `is_err()` 断言。

## 3. 验证门禁结果与复现命令

环境：Windows 11 Pro、bash、Rust stable（msvc）、隔离 `CARGO_TARGET_DIR`。基线提交 `0436b789`（`W80-0-SCAFFOLD`）。

| 门禁 | 命令 | 结果 |
|---|---|---|
| 格式检查 | `cargo fmt --check -p nlos-certify` | 退出码 `0` |
| Clippy 严格门禁 | `cargo clippy -p nlos-certify --all-targets -- -D warnings` | 退出码 `0`（零警告） |
| 集成测试套件 | `cargo test -p nlos-certify` | `17/17` 通过、`0` 失败：既有 `certify_pipeline_gate.rs` `10/10` + 既有 `e2e_trust_store_certify_pipeline.rs` `1/1` + **新增 `h7_supply_chain_adversarial.rs` `6/6`**（本套件 `finished in 2.60s`） |

开发过程中实测并修正过 4 处 clippy 违规（2 处 `similar_names`、2 处 `assert_is_empty`），修正后三道门禁全绿；最终提交版本为本节所列数字。

## 4. 未覆盖项与诚实边界（`Claim <= Evidence`）

1. **Wasm 路径/网络/配额逃逸对抗矩阵不在本车道**：§E.2.2 `E6` 第 4 条中"Wasm 路径/网络逃逸"归 `W80-E6A-WASM-SANDBOX` 车道（`crates/nlos-wasm-sandbox`，闭合 `U-9`），其写集与本车道正交，本文件不声称任何沙箱逃逸拦截证据。
2. **本车道验证平台仅为 Windows 本机**：三平台（Windows/macOS/Linux）CI 全绿是合并前置门（CLAUDE.md §5），须待本分支推送后由 "Rust cross-platform verification" CI 与 `W80-INT` 合流统一回执；裸 `rusqlite` 触发器攻击子项（攻击 3c/3d）为平台无关的 SQLite DDL 行为，但 macOS/Linux 实跑数字尚无。
3. **整库文件级替换攻击不在本测试范围**：攻击 3c 证明的是**同库内**直写 `UPDATE`/`DELETE` 被 `trust_revocation_state_no_rollback` 等 `RAISE(ABORT)` 触发器拒绝；对持有文件系统级写权限、直接整体替换 trust DB 文件为旧副本的攻击者，防御依赖发布同步对账与更高层 `store_revocation_epoch` 水位（`EntitlementStoreEpochTooStale`）而非该触发器，本文件不声称覆盖。
4. **认证收据劫持子项走 `issue_store_certification_receipt` 直连而非整条四门流水线重跑**：四门全流程 + 收据拒发的端到端劫持路径已由既有 [e2e_trust_store_certify_pipeline.rs](../../../crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs)（`11/11` 基线内）覆盖，本车道攻击 2f/4c 在同一信任检查点上以更小半径复验，不重复 heavyweight 全流水线；两处断言点等价（`run_certification_pipeline` 尾部即调用 `issue_store_certification_receipt`）。
5. **`H7` 定性边界**：本套件是确定性本地对抗性集成回归（给定攻击输入 → 断言拦截输出），非模糊测试、非渗透测试、非红队实证；不声称对未枚举攻击面的覆盖。
