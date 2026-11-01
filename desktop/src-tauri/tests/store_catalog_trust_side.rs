//! `W80-E6C` (`Desktop Store Transparency UI`) 桌面商店透明目录桥集成测试。
//!
//! 覆盖权威不变量：
//! - 目录快照 / 权限透明报告 / 跨版本提权差异 100% 投影自真实
//!   `nlos-store-registry` 数据面(`SqliteStoreRegistry` `WAL + FULL`),
//!   绝不自造第二目录状态;
//! - 发布者证书链透明:桥用发布记录重算域分离提交摘要,经真实
//!   `nlos-publisher-trust` 三级 PKI(`SqlitePublisherTrustAuthority`)
//!   在查看时复验叶证书链、签名与 CRL 新鲜度;信任库缺失时目录可读但
//!   链路明确降级标注(`[DUI-OFFLINE-001]`),敏感动作入口则严格 fail-closed;
//!   叶证书被 CRL 撤回后复验必须转为失败并拒绝敏感动作入口;
//! - 不可逆升级 / 降档沙箱 / 安装三类敏感动作只产出既有 `Z4`
//!   `Z4ConsentChallengeRequest` 入口投影(确认屏本体复用既有组件),
//!   撤回包与链路未核验一律拒绝入口;
//! - `L1/L2` 白话文案 `zh-CN / en-US` 双语零内部架构行话、零 Emoji;
//!   DTO 序列化 camelCase 且不泄露临时目录绝对路径。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use llmos_desktop_lib::a11y_i18n::{
    contains_forbidden_l0_to_l2_jargon, contains_pictographic_emoji,
};
use llmos_desktop_lib::error::ErrorCode;
use llmos_desktop_lib::store_catalog::{
    STORE_ACTION_DOWNGRADE_SANDBOX, STORE_ACTION_INSTALL, STORE_ACTION_IRREVERSIBLE_UPGRADE,
    STORE_CHAIN_STATUS_TRUST_DB_UNAVAILABLE, STORE_CHAIN_STATUS_VERIFICATION_FAILED,
    STORE_CHAIN_STATUS_VERIFIED, STORE_SOURCE_LIVE_REGISTRY, STORE_SOURCE_OFFLINE_UNAVAILABLE,
    StoreCatalogContext, StoreSensitiveActionInput, store_catalog_snapshot_with_context,
    store_permission_diff_with_context, store_prepare_sensitive_action_with_context,
    store_release_transparency_with_context,
};
use llmos_desktop_lib::trusted_ui::ReversibilityClass;
use nlos_application::pack_package_version;
use nlos_publisher_trust::{
    CrlImportDecision, IntermediateCertificateIssueSpec, LeafCertificateIssueSpec,
    OfflinePlatformRootCa, PublisherId, PublisherLeafCertificate, PublisherTrustTier,
    RevocationEpoch, RevocationListSpec, RevocationReason, RevokedCertificateEntry,
    SignedRevocationList, SqlitePublisherTrustAuthority, ed25519_public_key_from_seed,
};
use nlos_store_registry::{
    DeclaredPermissionEntry, EffectClass, LocalOciCasStore, NLOS_OCI_CONFIG_MEDIA_TYPE,
    OciLayerDescriptor, OciPackageLayout, SignedCertificationAttestation, SqliteStoreRegistry,
    StoreComponentKind, StoreComponentSlot, StoreGovernanceAnchors, StorePromotionDecision,
    StoreReleaseRecord, StoreReleaseSubmission, UnsignedStoreReleaseSubmission,
    compute_release_submission_digest,
};
use nlos_types::{ApplicationId, IdempotencyKey, KeyId, PackageId};

/// 桥验证用的确定性"当前时间"(处于全部证书窗口与 CRL 新鲜窗口内)。
const NOW_MS: u64 = 1_700_000_000_000;
/// 全部证书 / CRL 统一远未来过期边界(约 2223 年)。
const FAR_FUTURE_MS: u64 = 8_000_000_000_000;

static NEXT_TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

fn hex16(bytes: [u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct TempStoreDir(PathBuf);

impl TempStoreDir {
    fn new(tag: &str) -> Self {
        let seq = NEXT_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "nlos-desktop-store-{tag}-{}-{seq}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp store test dir");
        Self(dir)
    }
}

impl Drop for TempStoreDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// 真实三级 PKI + 治理目录 + OCI CAS 桥夹具
// ---------------------------------------------------------------------------

struct StoreGovernanceSigners {
    certifier_key: SigningKey,
    anchors: StoreGovernanceAnchors,
}

impl StoreGovernanceSigners {
    fn new() -> Self {
        let certifier_key = SigningKey::from_bytes(&[0xA1; 32]);
        let entitlement_key = SigningKey::from_bytes(&[0xB3; 32]);
        let recall_key = SigningKey::from_bytes(&[0xC4; 32]);
        let anchors = StoreGovernanceAnchors {
            certifier_public_key: certifier_key.verifying_key().to_bytes(),
            entitlement_issuer_key_id: KeyId::from_bytes([0xB2; 16]),
            entitlement_issuer_public_key: entitlement_key.verifying_key().to_bytes(),
            recall_authority_public_key: recall_key.verifying_key().to_bytes(),
            initial_revocation_epoch: RevocationEpoch::new(1),
            initialized_at_ms: 1_500,
        };
        Self {
            certifier_key,
            anchors,
        }
    }

    fn sign_attestation(&self, release: &StoreReleaseRecord) -> SignedCertificationAttestation {
        let mut att = SignedCertificationAttestation {
            package_id: release.package_id,
            application_id: release.application_ids[0],
            package_version: release.package_version,
            manifest_digest: release.manifest_digest,
            oci_manifest_digest: release.oci_manifest_digest,
            publisher_id: release.publisher_id,
            passed_gates_mask: 0b1111,
            issued_at_ms: 2_800,
            expires_at_ms: FAR_FUTURE_MS,
            certifier_public_key: self.certifier_key.verifying_key().to_bytes(),
            ed25519_signature: [0u8; 64],
        };
        let digest = att.signing_digest();
        att.ed25519_signature = self.certifier_key.sign(&digest).to_bytes();
        att
    }
}

struct DesktopStoreFixture {
    _dir_guard: TempStoreDir,
    trust_db_path: PathBuf,
    pkg_a: PackageId,
    pkg_a_v1: u64,
    pkg_a_v2: u64,
    pkg_b: PackageId,
    pkg_b_v1: u64,
    app_a: ApplicationId,
    app_b: ApplicationId,
    publisher_a: PublisherId,
    publisher_b: PublisherId,
    certified_seed: [u8; 32],
    certified_ca: nlos_publisher_trust::StoreIntermediateCertificate,
    leaf_a_serial: [u8; 16],
}

fn ingest_oci_layout(
    cas: &LocalOciCasStore,
    tag_seed: u8,
) -> (OciPackageLayout, Vec<StoreComponentSlot>) {
    let config_bytes = format!("{{\"package_seed\":{tag_seed},\"sabi\":\"1.0\"}}").into_bytes();
    let config_desc =
        OciLayerDescriptor::from_layer_bytes(NLOS_OCI_CONFIG_MEDIA_TYPE, &config_bytes)
            .expect("valid config descriptor");
    let payloads: [(&str, StoreComponentKind, &str, &str, Vec<u8>); 3] = [
        (
            "main-cli",
            StoreComponentKind::Executable,
            "bin/main.wasm",
            "application/vnd.nlos.component.executable.v1+wasm",
            vec![tag_seed, 0x01, 0x02, 0x03, 0x04],
        ),
        (
            "desktop-panel",
            StoreComponentKind::UiSurface,
            "surfaces/panel.duis",
            "application/vnd.nlos.component.ui-surface.v1+cbor",
            vec![tag_seed, 0x21, 0x22, 0x23],
        ),
        (
            "starter-notebook-seed",
            StoreComponentKind::ReadOnlySeedTemplate,
            "seeds/welcome.md",
            "application/vnd.nlos.component.seed.v1+markdown",
            b"# Read-Only Seed Template\n".to_vec(),
        ),
    ];
    let mut owned_layers = Vec::with_capacity(3);
    let mut slots = Vec::with_capacity(3);
    for (slot_name, kind, entrypoint, media_type, bytes) in payloads {
        let desc =
            OciLayerDescriptor::from_layer_bytes(media_type, &bytes).expect("valid layer desc");
        slots.push(StoreComponentSlot {
            slot_name: slot_name.to_owned(),
            kind,
            entrypoint: entrypoint.to_owned(),
            content_sha256: desc.sha256_digest,
            size_bytes: desc.size_bytes,
        });
        owned_layers.push((desc, bytes));
    }
    let borrowed: Vec<(OciLayerDescriptor, &[u8])> = owned_layers
        .iter()
        .map(|(desc, bytes)| (desc.clone(), bytes.as_slice()))
        .collect();
    let layout = cas
        .write_package_layout(&config_desc, &config_bytes, &borrowed)
        .expect("write verified 3-layer OCI package layout");
    (layout, slots)
}

fn permission(
    scope: &str,
    purpose: &str,
    effect: EffectClass,
    egress: bool,
    sandbox: &str,
) -> DeclaredPermissionEntry {
    DeclaredPermissionEntry {
        capability_scope: scope.to_owned(),
        human_purpose: purpose.to_owned(),
        effect_class: effect,
        network_egress_declared: egress,
        sandbox_profile: sandbox.to_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_signed_submission(
    package_id: PackageId,
    package_version: u64,
    application_ids: Vec<ApplicationId>,
    components: Vec<StoreComponentSlot>,
    declared_permissions: Vec<DeclaredPermissionEntry>,
    publisher_id: PublisherId,
    publisher_key: &SigningKey,
    layout: &OciPackageLayout,
) -> StoreReleaseSubmission {
    let manifest_digest = [0x5Eu8; 32];
    let unsigned = UnsignedStoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest: layout.oci_manifest_digest,
        application_ids: &application_ids,
        components: &components,
        declared_permissions: &declared_permissions,
        publisher_id,
    };
    let digest = compute_release_submission_digest(&unsigned);
    StoreReleaseSubmission {
        package_id,
        package_version,
        manifest_digest,
        oci_manifest_digest: layout.oci_manifest_digest,
        application_ids,
        components,
        declared_permissions,
        publisher_id,
        publisher_signing_key: publisher_key.verifying_key().to_bytes(),
        publisher_signature: publisher_key.sign(&digest).to_bytes(),
        submitted_at_ms: 2_900,
    }
}

impl DesktopStoreFixture {
    /// 装配:真实三级 PKI(根 -> CertifiedPublisher 中介 -> 叶)、真实信任库 +
    /// 治理锚定目录 + OCI CAS,并上架:
    /// - `pkg_a` v1(CertifiedPublisher,双只读权限)→ 四门晋升 Stable;
    /// - `pkg_a` v2(CertifiedPublisher,新增网络出站 + 不可逆作用域 + 用途变更
    ///   + 副作用升级)→ 停留 Candidate(供提权差异检视);
    /// - `pkg_b` v1(CommunitySandboxed 自签叶)→ CommunityTrack Candidate。
    fn setup() -> Self {
        let dir = TempStoreDir::new("bridge");
        let cas = LocalOciCasStore::open(dir.0.join("oci-cas")).expect("open OCI CAS");
        let signers = StoreGovernanceSigners::new();
        let registry = SqliteStoreRegistry::open(dir.0.join("store-db"), signers.anchors)
            .expect("open store registry");

        // 真实三级 PKI:根种子 [0x01],CertifiedPublisher 中介种子 [0x03]。
        let root_seed = [0x01u8; 32];
        let root_ca = OfflinePlatformRootCa::issue_self_signed(
            [0x10; 16],
            KeyId::from_bytes([0x11; 16]),
            &root_seed,
            1_000,
            FAR_FUTURE_MS,
        )
        .expect("issue offline root CA");
        let certified_seed = [0x03u8; 32];
        let certified_ca = root_ca
            .issue_intermediate_certificate(
                &root_seed,
                IntermediateCertificateIssueSpec {
                    serial_id: [0x30; 16],
                    intermediate_key_id: KeyId::from_bytes([0x31; 16]),
                    intermediate_public_key: ed25519_public_key_from_seed(&certified_seed),
                    max_issuable_tier: PublisherTrustTier::CertifiedPublisher,
                    not_before_ms: 1_000,
                    expires_at_ms: FAR_FUTURE_MS,
                },
            )
            .expect("issue certified intermediate CA");

        let trust_db_path = dir.0.join("publisher-trust.db");
        let trust = SqlitePublisherTrustAuthority::open(&trust_db_path)
            .expect("open publisher trust authority");
        trust
            .anchor_offline_root_ca(&root_ca, 1_500)
            .expect("anchor root CA");
        trust
            .register_intermediate_certificate(&certified_ca, 1_600)
            .expect("register certified intermediate CA");

        // CertifiedPublisher 叶证书:发布者 A(种子 [0x04])。
        let publisher_a = PublisherId::from_bytes([0x41; 16]);
        let leaf_a_seed = [0x04u8; 32];
        let leaf_a_serial = [0x42u8; 16];
        let leaf_a = certified_ca
            .issue_leaf_certificate_with_seed(
                &certified_seed,
                LeafCertificateIssueSpec {
                    serial_id: leaf_a_serial,
                    publisher_id: publisher_a,
                    trust_tier: PublisherTrustTier::CertifiedPublisher,
                    leaf_key_id: KeyId::from_bytes([0x43; 16]),
                    leaf_public_key: ed25519_public_key_from_seed(&leaf_a_seed),
                    not_before_ms: 2_000,
                    expires_at_ms: FAR_FUTURE_MS,
                },
            )
            .expect("issue certified leaf certificate");
        trust
            .register_leaf_certificate(&leaf_a, 2_600)
            .expect("register certified leaf");

        // CommunitySandboxed 自签叶:发布者 B(种子 [0x08])。
        let publisher_b = PublisherId::from_bytes([0x71; 16]);
        let leaf_b_seed = [0x08u8; 32];
        let leaf_b = PublisherLeafCertificate::issue_self_signed_community(
            [0x72; 16],
            publisher_b,
            KeyId::from_bytes([0x73; 16]),
            &leaf_b_seed,
            2_000,
            FAR_FUTURE_MS,
        )
        .expect("issue community leaf");
        trust
            .register_leaf_certificate(&leaf_b, 2_700)
            .expect("register community leaf");

        // 初始 CRL(epoch 1,fresh 至远未来)——桥在线复验走防冻结窗口。
        let crl = SignedRevocationList::issue_with_intermediate_seed(
            &certified_ca,
            &certified_seed,
            RevocationListSpec {
                revocation_epoch: RevocationEpoch::new(1),
                issued_at_ms: 2_000,
                fresh_until_ms: FAR_FUTURE_MS,
                revoked_certificates: Vec::new(),
                revoked_publishers: Vec::new(),
            },
        )
        .expect("issue initial CRL");
        assert_eq!(
            trust
                .import_signed_revocation_list(&crl)
                .expect("import initial CRL"),
            CrlImportDecision::Advanced(RevocationEpoch::new(1))
        );

        // pkg_a v1:CertifiedPublisher,两个只读权限 -> 晋升 Stable。
        let pkg_a = PackageId::from_bytes([0xA1; 16]);
        let app_a = ApplicationId::from_bytes([0x33; 16]);
        let pkg_a_v1 = pack_package_version(1, 0, 0);
        let (layout_a1, slots_a1) = ingest_oci_layout(&cas, 0x51);
        let publisher_a_key = SigningKey::from_bytes(&leaf_a_seed);
        let submission_a1 = build_signed_submission(
            pkg_a,
            pkg_a_v1,
            vec![app_a],
            slots_a1,
            vec![
                permission(
                    "sabi.artifact.read",
                    "读取用户笔记",
                    EffectClass::ReadOnly,
                    false,
                    "wasm-strict-v1",
                ),
                permission(
                    "sabi.clipboard.read",
                    "粘贴文本片段",
                    EffectClass::ReadOnly,
                    false,
                    "wasm-strict-v1",
                ),
            ],
            publisher_a,
            &publisher_a_key,
            &layout_a1,
        );
        registry
            .submit_release(&cas, &trust, &submission_a1)
            .expect("submit pkg_a v1");
        let release_a1 = registry
            .inspect_release(pkg_a, pkg_a_v1)
            .expect("inspect a1")
            .expect("a1 present");
        let promoted = registry
            .promote_candidate_to_stable(
                &signers.sign_attestation(&release_a1),
                IdempotencyKey::from_bytes([0x9A; 16]),
                3_000,
            )
            .expect("promote pkg_a v1 to Stable");
        assert!(matches!(promoted, StorePromotionDecision::Promoted(_)));

        // pkg_a v2:新增出站 + 不可逆 + 用途变更 + 副作用升级 -> Candidate。
        let pkg_a_v2 = pack_package_version(1, 1, 0);
        let (layout_a2, slots_a2) = ingest_oci_layout(&cas, 0x52);
        let submission_a2 = build_signed_submission(
            pkg_a,
            pkg_a_v2,
            vec![app_a],
            slots_a2,
            vec![
                permission(
                    "sabi.artifact.read",
                    "读取用户笔记",
                    EffectClass::Reversible,
                    false,
                    "wasm-strict-v1",
                ),
                permission(
                    "sabi.clipboard.read",
                    "粘贴图文片段",
                    EffectClass::ReadOnly,
                    false,
                    "wasm-strict-v1",
                ),
                permission(
                    "sabi.artifact.write",
                    "写入文档存档",
                    EffectClass::Compensatable,
                    true,
                    "wasm-strict-v2",
                ),
                permission(
                    "sabi.sync.push",
                    "推送同步数据",
                    EffectClass::Irreversible,
                    true,
                    "wasm-strict-v2",
                ),
            ],
            publisher_a,
            &publisher_a_key,
            &layout_a2,
        );
        registry
            .submit_release(&cas, &trust, &submission_a2)
            .expect("submit pkg_a v2");

        // pkg_b v1:CommunitySandboxed -> CommunityTrack Candidate。
        let pkg_b = PackageId::from_bytes([0xB1; 16]);
        let app_b = ApplicationId::from_bytes([0x44; 16]);
        let pkg_b_v1 = pack_package_version(1, 0, 0);
        let (layout_b1, slots_b1) = ingest_oci_layout(&cas, 0x53);
        let publisher_b_key = SigningKey::from_bytes(&leaf_b_seed);
        let submission_b1 = build_signed_submission(
            pkg_b,
            pkg_b_v1,
            vec![app_b],
            slots_b1,
            vec![permission(
                "sabi.notes.read",
                "读取笔记内容",
                EffectClass::ReadOnly,
                false,
                "dual-layer-wasmtime-strict",
            )],
            publisher_b,
            &publisher_b_key,
            &layout_b1,
        );
        registry
            .submit_release(&cas, &trust, &submission_b1)
            .expect("submit pkg_b v1");

        Self {
            _dir_guard: dir,
            trust_db_path,
            pkg_a,
            pkg_a_v1,
            pkg_a_v2,
            pkg_b,
            pkg_b_v1,
            app_a,
            app_b,
            publisher_a,
            publisher_b,
            certified_seed,
            certified_ca,
            leaf_a_serial,
        }
    }

    /// `store-registry.db` 所在根目录(与 `SqliteStoreRegistry::open` 的 root 一致)。
    fn store_root(&self) -> PathBuf {
        self._dir_guard.0.join("store-db")
    }

    fn ctx(&self) -> StoreCatalogContext {
        StoreCatalogContext::isolated(Some(self.store_root()), Some(self.trust_db_path.clone()))
    }

    /// 导入 epoch 2 CRL 撤回发布者 A 的叶证书(模拟密钥泄露对抗路径)。
    fn revoke_publisher_a_leaf(&self) {
        let trust = SqlitePublisherTrustAuthority::open(&self.trust_db_path)
            .expect("reopen trust authority");
        let crl2 = SignedRevocationList::issue_with_intermediate_seed(
            &self.certified_ca,
            &self.certified_seed,
            RevocationListSpec {
                revocation_epoch: RevocationEpoch::new(2),
                issued_at_ms: 3_500,
                fresh_until_ms: FAR_FUTURE_MS,
                revoked_certificates: vec![RevokedCertificateEntry {
                    serial_or_key: self.leaf_a_serial,
                    reason: RevocationReason::KeyCompromise,
                }],
                revoked_publishers: Vec::new(),
            },
        )
        .expect("issue revoking CRL");
        assert_eq!(
            trust
                .import_signed_revocation_list(&crl2)
                .expect("import revoking CRL"),
            CrlImportDecision::Advanced(RevocationEpoch::new(2))
        );
    }
}

fn assert_zero_jargon_and_emoji(label: &str, text: &str) {
    assert!(
        contains_forbidden_l0_to_l2_jargon(text).is_none(),
        "{label} leaked banned jargon: '{text}'"
    );
    assert!(
        !contains_pictographic_emoji(text),
        "{label} leaked pictographic emoji: '{text}'"
    );
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_projects_live_catalog_from_store_registry_data_plane() {
    let fx = DesktopStoreFixture::setup();
    let snapshot = store_catalog_snapshot_with_context(fx.ctx(), NOW_MS)
        .await
        .expect("snapshot");

    assert_eq!(snapshot.source, STORE_SOURCE_LIVE_REGISTRY);
    assert_eq!(snapshot.store_revocation_epoch, 1);
    assert!(snapshot.offline_notice_zh.is_none());
    assert!(snapshot.offline_notice_en.is_none());
    assert_eq!(snapshot.official_stable_releases.len(), 1);
    assert_eq!(snapshot.community_candidates.len(), 1);

    let stable = &snapshot.official_stable_releases[0];
    assert_eq!(stable.package_id_hex, hex16(*fx.pkg_a.as_bytes()));
    assert_eq!(stable.package_version, fx.pkg_a_v1);
    assert_eq!(
        stable.application_ids_hex,
        vec![hex16(*fx.app_a.as_bytes())]
    );
    assert_eq!(stable.publisher_id_hex, hex16(*fx.publisher_a.as_bytes()));
    assert_eq!(stable.publisher_tier, "CERTIFIED_PUBLISHER");
    assert_eq!(stable.track, "OFFICIAL");
    assert_eq!(stable.channel, "STABLE");
    assert_eq!(stable.active_recall_severity, None);
    assert_eq!(stable.component_count, 3);
    assert_eq!(stable.permission_count, 2);
    assert!(!stable.any_network_egress);
    assert!(!stable.any_irreversible_effect);
    assert_eq!(stable.publisher_key_fingerprint_hex.len(), 16);
    assert_eq!(stable.submitted_at_ms, 2_900);

    let candidate = &snapshot.community_candidates[0];
    assert_eq!(candidate.package_id_hex, hex16(*fx.pkg_b.as_bytes()));
    assert_eq!(candidate.publisher_tier, "COMMUNITY_SANDBOXED");
    assert_eq!(candidate.track, "COMMUNITY");
    assert_eq!(candidate.channel, "CANDIDATE");

    // pkg_a v2 停留 Candidate:不进入官方 Stable 列表,也不属于社区列表。
    assert!(
        !snapshot
            .official_stable_releases
            .iter()
            .any(|entry| entry.package_version == fx.pkg_a_v2)
    );

    // camelCase 序列化 + 零临时目录绝对路径泄漏。
    let json = serde_json::to_string(&snapshot).expect("serialize snapshot");
    assert!(json.contains("\"officialStableReleases\""));
    assert!(json.contains("\"communityCandidates\""));
    assert!(json.contains("\"publisherKeyFingerprintHex\""));
    assert!(!json.contains("nlos-desktop-store"));

    let _ = (fx.pkg_b, fx.app_b, fx.publisher_b);
}

#[tokio::test]
async fn transparency_report_and_publisher_chain_reverification_with_crl_revocation_guard() {
    let fx = DesktopStoreFixture::setup();
    let pkg_hex = hex16(*fx.pkg_a.as_bytes());
    let dto = store_release_transparency_with_context(fx.ctx(), &pkg_hex, fx.pkg_a_v1, NOW_MS)
        .await
        .expect("transparency");

    assert_eq!(dto.package_id_hex, pkg_hex);
    assert_eq!(dto.publisher_tier, "CERTIFIED_PUBLISHER");
    assert_eq!(dto.channel, "STABLE");
    assert_eq!(dto.track, "OFFICIAL");
    assert_eq!(dto.permissions.len(), 2);
    assert_eq!(dto.permissions[0].capability_scope, "sabi.artifact.read");
    assert_eq!(dto.permissions[0].human_purpose, "读取用户笔记");
    assert_eq!(dto.permissions[0].effect_class, "READ_ONLY");
    assert!(!dto.permissions[0].network_egress_declared);
    assert_eq!(dto.permissions[0].sandbox_profile, "wasm-strict-v1");
    assert!(!dto.any_network_egress);
    assert!(!dto.any_irreversible_effect);

    // 效果等级双语图例(L2 零行话、零 Emoji,四级齐全)。
    assert_eq!(dto.effect_legend.len(), 4);
    for label in &dto.effect_legend {
        assert_zero_jargon_and_emoji("effect legend zh", &label.label_zh);
        assert_zero_jargon_and_emoji("effect legend en", &label.label_en);
    }

    // 证书链:查看时重验(叶 -> 中介 -> 根 + 签名 + CRL 新鲜度)全部通过。
    let chain = &dto.trust_chain;
    assert_eq!(chain.chain_status, STORE_CHAIN_STATUS_VERIFIED);
    assert!(chain.signature_reverified);
    assert_eq!(chain.publisher_id_hex, hex16(*fx.publisher_a.as_bytes()));
    assert_eq!(chain.leaf_key_fingerprint_hex.len(), 16);
    assert_eq!(chain.revocation_epoch, 1);
    assert_eq!(chain.crl_fresh_until_ms, FAR_FUTURE_MS);
    assert_zero_jargon_and_emoji("tier label zh", &chain.tier_label_zh);
    assert_zero_jargon_and_emoji("tier label en", &chain.tier_label_en);
    assert_zero_jargon_and_emoji("chain notice zh", &chain.chain_status_notice_zh);
    assert_zero_jargon_and_emoji("chain notice en", &chain.chain_status_notice_en);

    let json = serde_json::to_string(&dto).expect("serialize transparency");
    assert!(json.contains("\"trustChain\""));
    assert!(!json.contains("nlos-desktop-store"));

    // 信任库缺失:目录透明报告仍可读([DUI-OFFLINE-001]),但链路明确降级。
    let no_trust_ctx = StoreCatalogContext::isolated(Some(fx.store_root()), None);
    let degraded = store_release_transparency_with_context(
        no_trust_ctx.clone(),
        &pkg_hex,
        fx.pkg_a_v1,
        NOW_MS,
    )
    .await
    .expect("degraded transparency read");
    assert_eq!(
        degraded.trust_chain.chain_status,
        STORE_CHAIN_STATUS_TRUST_DB_UNAVAILABLE
    );
    assert!(!degraded.trust_chain.signature_reverified);
    assert!(!degraded.trust_chain.chain_status_notice_zh.is_empty());
    assert_zero_jargon_and_emoji(
        "degraded chain notice zh",
        &degraded.trust_chain.chain_status_notice_zh,
    );
    assert_zero_jargon_and_emoji(
        "degraded chain notice en",
        &degraded.trust_chain.chain_status_notice_en,
    );

    // CRL 撤回叶证书后:链路复验必须转 VERIFICATION_FAILED(伪造/失效链路拦截)。
    fx.revoke_publisher_a_leaf();
    let revoked = store_release_transparency_with_context(fx.ctx(), &pkg_hex, fx.pkg_a_v1, NOW_MS)
        .await
        .expect("revoked transparency read still renders");
    assert_eq!(
        revoked.trust_chain.chain_status,
        STORE_CHAIN_STATUS_VERIFICATION_FAILED
    );
    assert!(!revoked.trust_chain.signature_reverified);
    assert_zero_jargon_and_emoji(
        "revoked chain notice zh",
        &revoked.trust_chain.chain_status_notice_zh,
    );

    // 敏感动作入口在链路被撤回后必须 fail-closed。
    let blocked = store_prepare_sensitive_action_with_context(
        fx.ctx(),
        StoreSensitiveActionInput {
            action_kind: STORE_ACTION_INSTALL.to_owned(),
            package_id_hex: pkg_hex,
            package_version: fx.pkg_a_v1,
            old_version: None,
            now_ms: Some(NOW_MS),
        },
    )
    .await
    .expect_err("revoked publisher must not get a sensitive action entry");
    assert_eq!(blocked.code, ErrorCode::Control);
}

#[tokio::test]
async fn permission_escalation_diff_blocks_silent_upgrade_and_stays_plain_language() {
    let fx = DesktopStoreFixture::setup();
    let pkg_hex = hex16(*fx.pkg_a.as_bytes());
    let diff = store_permission_diff_with_context(fx.ctx(), &pkg_hex, fx.pkg_a_v1, fx.pkg_a_v2)
        .await
        .expect("diff");

    assert!(diff.requires_z4_reconsent);
    assert_eq!(
        diff.added_capabilities
            .iter()
            .map(|entry| entry.capability_scope.as_str())
            .collect::<Vec<_>>(),
        vec!["sabi.artifact.write", "sabi.sync.push"]
    );
    assert_eq!(
        diff.newly_enabled_network_egress,
        vec![
            "sabi.artifact.write".to_owned(),
            "sabi.sync.push".to_owned()
        ]
    );
    assert_eq!(diff.escalated_effects.len(), 1);
    assert_eq!(
        diff.escalated_effects[0].capability_scope,
        "sabi.artifact.read"
    );
    assert_eq!(diff.escalated_effects[0].old_effect, "READ_ONLY");
    assert_eq!(diff.escalated_effects[0].new_effect, "REVERSIBLE");
    assert_eq!(diff.escalated_to_irreversible.len(), 1);
    assert_eq!(
        diff.escalated_to_irreversible[0].capability_scope,
        "sabi.sync.push"
    );
    assert_eq!(diff.escalated_to_irreversible[0].new_effect, "IRREVERSIBLE");
    assert_eq!(diff.purpose_changes.len(), 1);
    assert_eq!(
        diff.purpose_changes[0].capability_scope,
        "sabi.clipboard.read"
    );
    assert_eq!(diff.purpose_changes[0].old_purpose, "粘贴文本片段");
    assert_eq!(diff.purpose_changes[0].new_purpose, "粘贴图文片段");
    assert!(diff.removed_capability_scopes.is_empty());
    assert!(diff.sandbox_profile_changes.is_empty());

    assert!(!diff.plain_summary_zh.is_empty());
    assert!(!diff.plain_summary_en.is_empty());
    assert_zero_jargon_and_emoji("diff summary zh", &diff.plain_summary_zh);
    assert_zero_jargon_and_emoji("diff summary en", &diff.plain_summary_en);

    // 同版本自比较:纯幂等,不触发重新确认。
    let same = store_permission_diff_with_context(fx.ctx(), &pkg_hex, fx.pkg_a_v1, fx.pkg_a_v1)
        .await
        .expect("same-version diff");
    assert!(!same.requires_z4_reconsent);
    assert_zero_jargon_and_emoji("same diff zh", &same.plain_summary_zh);

    // 任一版本不存在:fail-closed NotFound。
    let missing =
        store_permission_diff_with_context(fx.ctx(), &hex16([0xEE; 16]), fx.pkg_a_v1, fx.pkg_a_v2)
            .await
            .expect_err("unknown package diff must fail closed");
    assert_eq!(missing.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn sensitive_actions_project_z4_entry_and_fail_closed_on_unverified_inputs() {
    let fx = DesktopStoreFixture::setup();
    let pkg_hex = hex16(*fx.pkg_a.as_bytes());

    // 1) 安装 Stable v1:入口投影为 Z4 请求,可逆等级取自透明报告(无不可逆 -> 可补偿回滚)。
    let install = store_prepare_sensitive_action_with_context(
        fx.ctx(),
        StoreSensitiveActionInput {
            action_kind: STORE_ACTION_INSTALL.to_owned(),
            package_id_hex: pkg_hex.clone(),
            package_version: fx.pkg_a_v1,
            old_version: None,
            now_ms: Some(NOW_MS),
        },
    )
    .await
    .expect("install entry");
    assert_eq!(install.action_kind, STORE_ACTION_INSTALL);
    assert!(matches!(
        install.reversibility_class,
        ReversibilityClass::CompensatableRollback
    ));
    assert!(install.z4_request.requesting_app.signature_verified);
    assert_eq!(
        install.z4_request.requesting_app.application_id_hex,
        hex16(*fx.app_a.as_bytes())
    );
    assert!(!install.z4_request.action_scope.plain_summary_zh.is_empty());
    assert!(!install.z4_request.action_scope.plain_summary_en.is_empty());
    assert_zero_jargon_and_emoji(
        "install zh summary",
        &install.z4_request.action_scope.plain_summary_zh,
    );
    assert_zero_jargon_and_emoji(
        "install en summary",
        &install.z4_request.action_scope.plain_summary_en,
    );
    // 爆炸半径:每个声明能力一行 "+",网络出站以 "~" 标注(v1 无出站 -> 无 "~" 行)。
    assert!(
        install
            .z4_request
            .reversibility_and_risk
            .blast_radius_rows
            .iter()
            .any(|row| row.symbol == "+" && row.scope_label.contains("sabi.artifact.read"))
    );
    assert!(
        !install
            .z4_request
            .reversibility_and_risk
            .blast_radius_rows
            .iter()
            .any(|row| row.symbol == "~")
    );
    assert!(!install.z4_request.resource_ceiling.network_egress_allowed);
    assert!(matches!(
        install
            .z4_request
            .reversibility_and_risk
            .reversibility_class,
        ReversibilityClass::CompensatableRollback
    ));

    // 2) 安装 v2(含不可逆 + 出站):可逆等级升级为 IRREVERSIBLE,资源上限标注出站。
    let install_v2 = store_prepare_sensitive_action_with_context(
        fx.ctx(),
        StoreSensitiveActionInput {
            action_kind: STORE_ACTION_INSTALL.to_owned(),
            package_id_hex: pkg_hex.clone(),
            package_version: fx.pkg_a_v2,
            old_version: None,
            now_ms: Some(NOW_MS),
        },
    )
    .await
    .expect("install v2 entry");
    assert!(matches!(
        install_v2.reversibility_class,
        ReversibilityClass::Irreversible
    ));
    assert!(
        install_v2
            .z4_request
            .resource_ceiling
            .network_egress_allowed
    );
    assert!(
        install_v2
            .z4_request
            .reversibility_and_risk
            .blast_radius_rows
            .iter()
            .any(|row| row.symbol == "~")
    );
    assert!(matches!(
        install_v2
            .z4_request
            .reversibility_and_risk
            .reversibility_class,
        ReversibilityClass::Irreversible
    ));

    // 3) 不可逆升级 v1 -> v2:入口必须携带提权差异行。
    let upgrade = store_prepare_sensitive_action_with_context(
        fx.ctx(),
        StoreSensitiveActionInput {
            action_kind: STORE_ACTION_IRREVERSIBLE_UPGRADE.to_owned(),
            package_id_hex: pkg_hex.clone(),
            package_version: fx.pkg_a_v2,
            old_version: Some(fx.pkg_a_v1),
            now_ms: Some(NOW_MS),
        },
    )
    .await
    .expect("upgrade entry");
    assert!(matches!(
        upgrade.reversibility_class,
        ReversibilityClass::Irreversible
    ));
    assert!(
        upgrade
            .z4_request
            .reversibility_and_risk
            .blast_radius_rows
            .iter()
            .any(|row| row.symbol == "+" && row.target_label.contains("sabi.sync.push"))
    );
    assert_zero_jargon_and_emoji(
        "upgrade zh summary",
        &upgrade.z4_request.action_scope.plain_summary_zh,
    );

    // 4) 降档沙箱(社区包):入口为不可逆姿态变更确认。
    let downgrade = store_prepare_sensitive_action_with_context(
        fx.ctx(),
        StoreSensitiveActionInput {
            action_kind: STORE_ACTION_DOWNGRADE_SANDBOX.to_owned(),
            package_id_hex: hex16(*fx.pkg_b.as_bytes()),
            package_version: fx.pkg_b_v1,
            old_version: None,
            now_ms: Some(NOW_MS),
        },
    )
    .await
    .expect("downgrade entry");
    assert_eq!(downgrade.action_kind, STORE_ACTION_DOWNGRADE_SANDBOX);
    assert!(matches!(
        downgrade.reversibility_class,
        ReversibilityClass::Irreversible
    ));
    assert_zero_jargon_and_emoji(
        "downgrade zh summary",
        &downgrade.z4_request.action_scope.plain_summary_zh,
    );

    // 5) 信任库缺失:敏感动作入口严格 fail-closed(链路未核验绝不给入口)。
    let no_trust_ctx = StoreCatalogContext::isolated(Some(fx.store_root()), None);
    let blocked = store_prepare_sensitive_action_with_context(
        no_trust_ctx,
        StoreSensitiveActionInput {
            action_kind: STORE_ACTION_INSTALL.to_owned(),
            package_id_hex: pkg_hex.clone(),
            package_version: fx.pkg_a_v1,
            old_version: None,
            now_ms: Some(NOW_MS),
        },
    )
    .await
    .expect_err("sensitive action must fail closed without trust db");
    assert_eq!(blocked.code, ErrorCode::Control);

    // 6) 未知包:NotFound。
    let unknown = store_prepare_sensitive_action_with_context(
        fx.ctx(),
        StoreSensitiveActionInput {
            action_kind: STORE_ACTION_INSTALL.to_owned(),
            package_id_hex: hex16([0xEE; 16]),
            package_version: fx.pkg_a_v1,
            old_version: None,
            now_ms: Some(NOW_MS),
        },
    )
    .await
    .expect_err("unknown package entry must fail closed");
    assert_eq!(unknown.code, ErrorCode::NotFound);

    // 7) 非法输入:未知动作种类(Config 拒绝,绝不放行旁路动词)。
    let bad_kind = store_prepare_sensitive_action_with_context(
        fx.ctx(),
        StoreSensitiveActionInput {
            action_kind: "SIDELOAD_BYPASS".to_owned(),
            package_id_hex: pkg_hex,
            package_version: fx.pkg_a_v1,
            old_version: None,
            now_ms: Some(NOW_MS),
        },
    )
    .await
    .expect_err("unknown action kind must be rejected");
    assert_eq!(bad_kind.code, ErrorCode::Config);
}

#[tokio::test]
async fn offline_store_root_degrades_deterministically_without_jargon_or_path_leak() {
    let dir = TempStoreDir::new("offline");
    let missing_root = dir.0.join("no-such-store");
    let ctx = StoreCatalogContext::isolated(Some(missing_root), None);

    let snapshot = store_catalog_snapshot_with_context(ctx.clone(), NOW_MS)
        .await
        .expect("offline snapshot must degrade, not error");
    assert_eq!(snapshot.source, STORE_SOURCE_OFFLINE_UNAVAILABLE);
    assert!(snapshot.official_stable_releases.is_empty());
    assert!(snapshot.community_candidates.is_empty());
    assert_eq!(snapshot.store_revocation_epoch, 0);
    assert!(
        !snapshot
            .offline_notice_zh
            .as_deref()
            .unwrap_or_default()
            .is_empty()
    );
    assert!(
        !snapshot
            .offline_notice_en
            .as_deref()
            .unwrap_or_default()
            .is_empty()
    );
    assert_zero_jargon_and_emoji(
        "offline zh",
        snapshot.offline_notice_zh.as_deref().unwrap_or(""),
    );
    assert_zero_jargon_and_emoji(
        "offline en",
        snapshot.offline_notice_en.as_deref().unwrap_or(""),
    );

    let json = serde_json::to_string(&snapshot).expect("serialize offline snapshot");
    assert!(json.contains("\"source\":\"OFFLINE_UNAVAILABLE\""));
    assert!(!json.contains("no-such-store"));
    assert!(!json.contains("nlos-desktop-store"));

    // 单发布物明细在离线态 fail-closed(不伪造明细)。
    let detail_err = store_release_transparency_with_context(ctx, &hex16([0xAB; 16]), 1, NOW_MS)
        .await
        .expect_err("offline transparency detail must fail closed");
    assert_eq!(detail_err.code, ErrorCode::NotFound);
}
