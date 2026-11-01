//! 桌面商店透明目录桥(`W80-E6C-DESKTOP-STORE-UI` 车道实现面,`ADR-0025`
//! Graphite/Paper 实心工业工作站规范、零毛玻璃、零 Emoji)。
//!
//! 权威约束:
//! - 目录快照、发布者证书链与权限透明报告 100% 投影自 `nlos-store-registry`
//!   数据面(`SqliteStoreRegistry`),绝不在桌面侧自造第二目录状态
//!   (`[PKG-OCI-001]`、`[PKG-MULTI-001]`、`[TUI-CONSENT-001]`);
//! - 发布者证书链透明 = 查看时以发布记录重算域分离提交摘要,经真实
//!   `nlos-publisher-trust` 三级 PKI 在线复验(叶 -> 中介 -> 根 + 签名 +
//!   CRL 新鲜度,`[TCB-EGRESS-001]`、`[CAP-REVOKE-001]`);信任库缺失时
//!   目录浏览按 `[DUI-OFFLINE-001]` 明确降级标注,敏感动作入口严格
//!   fail-closed;
//! - 安装 / 不可逆升级 / 降档沙箱三类敏感动作只产出既有 `Z4`
//!   `Z4ConsentChallengeRequest` 入口投影(`ADR-0011` 认证控制脊柱与
//!   `trusted_ui` 既有确认屏),本模块零自绘确认 UI、零旁路动词;
//! - 全部阻塞型 `SQLite` I/O 经后台线程池脱离 UI 主线程;错误消息零
//!   绝对路径、零密钥材料泄漏;`L1/L2` 白话文案 `zh-CN / en-US` 双语
//!   零内部架构行话、零 Emoji(对齐 `a11y_i18n` 既有纪律)。
//!
//! `run()` 壳 `invoke_handler` 挂接与前端 `main.ts` 路由注册归 `W80-INT`。

#![forbid(unsafe_code)]

use std::path::PathBuf;

use nlos_publisher_trust::{
    PublisherTrustCheckError, PublisherTrustTier, SqlitePublisherTrustAuthority, TrustFreshnessMode,
};
use nlos_store_registry::{
    DeclaredPermissionEntry, EffectClass, PermissionEscalationDiff, PermissionTransparencyReport,
    RecallSeverity, SqliteStoreRegistry, StoreCatalogTrack, StoreReleaseRecord,
    UnsignedStoreReleaseSubmission, compute_release_submission_digest, compute_sha256,
};
use nlos_types::PackageId;
use serde::{Deserialize, Serialize};

use crate::dto::hex;
use crate::error::{DesktopError, ErrorCode};
use crate::ipc::{AppState, ENV_RESOURCE_ROOT};
use crate::trusted_ui::{
    ConsentGrantScope, ReversibilityClass, Z4ActionScopeSpec, Z4BlastRadiusRowDto,
    Z4ConsentChallengeRequest, Z4RequestingAppSpec, Z4ResourceCeilingSpec, Z4ReversibilityRiskSpec,
};

/// 目录快照数据来源:真实治理目录在线投影。
pub const STORE_SOURCE_LIVE_REGISTRY: &str = "LIVE_REGISTRY";
/// 目录快照数据来源:治理目录不可用时的确定性离线降级(`[DUI-OFFLINE-001]`)。
pub const STORE_SOURCE_OFFLINE_UNAVAILABLE: &str = "OFFLINE_UNAVAILABLE";
/// 证书链状态:查看时复验全部通过。
pub const STORE_CHAIN_STATUS_VERIFIED: &str = "VERIFIED";
/// 证书链状态:本地信任库缺失或不可读(浏览降级,敏感动作 fail-closed)。
pub const STORE_CHAIN_STATUS_TRUST_DB_UNAVAILABLE: &str = "TRUST_DB_UNAVAILABLE";
/// 证书链状态:复验失败(签名/证书链/撤回/新鲜度任一不通过)。
pub const STORE_CHAIN_STATUS_VERIFICATION_FAILED: &str = "VERIFICATION_FAILED";
/// 敏感动作种类:安装(商店目录内首次安装)。
pub const STORE_ACTION_INSTALL: &str = "INSTALL";
/// 敏感动作种类:不可逆升级(跨版本且透明报告存在提权差异)。
pub const STORE_ACTION_IRREVERSIBLE_UPGRADE: &str = "IRREVERSIBLE_UPGRADE";
/// 敏感动作种类:降档沙箱(社区未认证包强制双层强沙箱执行姿态)。
pub const STORE_ACTION_DOWNGRADE_SANDBOX: &str = "DOWNGRADE_SANDBOX";
/// 商店治理目录 `SQLite` 文件所在目录的环境变量兜底(生产接线归 `W80-INT`)。
pub const ENV_STORE_ROOT: &str = "LLMOS_DESKTOP_STORE_ROOT";
/// 发布者信任库文件路径的环境变量兜底(生产接线归 `W80-INT`)。
pub const ENV_PUBLISHER_TRUST_DB: &str = "LLMOS_DESKTOP_PUBLISHER_TRUST_DB";

/// 目录条目投影(`L1` 高密度表格行,零内部行话)。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreCatalogEntryDto {
    pub package_id_hex: String,
    pub package_version: u64,
    pub application_ids_hex: Vec<String>,
    pub publisher_id_hex: String,
    pub publisher_tier: String,
    pub track: String,
    pub channel: String,
    pub active_recall_severity: Option<String>,
    pub component_count: u32,
    pub permission_count: u32,
    pub any_network_egress: bool,
    pub any_irreversible_effect: bool,
    pub publisher_key_fingerprint_hex: String,
    pub submitted_at_ms: u64,
}

/// 商店目录快照投影(浏览面;离线确定性降级,绝不崩溃)。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreCatalogSnapshotDto {
    pub source: String,
    pub offline_notice_zh: Option<String>,
    pub offline_notice_en: Option<String>,
    pub store_revocation_epoch: u64,
    pub official_stable_releases: Vec<StoreCatalogEntryDto>,
    pub community_candidates: Vec<StoreCatalogEntryDto>,
}

/// 单条声明权限投影(`L2` 检视;`human_purpose` 为发布者原文)。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredPermissionDto {
    pub capability_scope: String,
    pub human_purpose: String,
    pub effect_class: String,
    pub network_egress_declared: bool,
    pub sandbox_profile: String,
}

/// 副作用等级双语图例行(`L2` 零行话)。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectClassLabelDto {
    pub effect_class: String,
    pub label_zh: String,
    pub label_en: String,
}

/// 发布者证书链透明投影(查看时复验状态 + 撤回纪元/CRL 新鲜度)。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublisherTrustChainDto {
    pub publisher_id_hex: String,
    pub trust_tier: String,
    pub tier_label_zh: String,
    pub tier_label_en: String,
    pub chain_status: String,
    pub chain_status_notice_zh: String,
    pub chain_status_notice_en: String,
    pub leaf_key_fingerprint_hex: String,
    pub revocation_epoch: u64,
    pub crl_fresh_until_ms: u64,
    pub signature_reverified: bool,
}

/// 单发布物权限透明报告投影(浏览详情/安装前检视)。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreReleaseTransparencyDto {
    pub package_id_hex: String,
    pub package_version: u64,
    pub application_ids_hex: Vec<String>,
    pub publisher_id_hex: String,
    pub publisher_tier: String,
    pub track: String,
    pub channel: String,
    pub permissions: Vec<DeclaredPermissionDto>,
    pub any_network_egress: bool,
    pub any_irreversible_effect: bool,
    pub effect_legend: Vec<EffectClassLabelDto>,
    pub trust_chain: PublisherTrustChainDto,
}

/// 新增能力差异行。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddedCapabilityDto {
    pub capability_scope: String,
    pub human_purpose: String,
    pub effect_class: String,
    pub network_egress_declared: bool,
}

/// 副作用等级升级行。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectEscalationDto {
    pub capability_scope: String,
    pub old_effect: String,
    pub new_effect: String,
}

/// 用途变更行。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurposeChangeDto {
    pub capability_scope: String,
    pub old_purpose: String,
    pub new_purpose: String,
}

/// 沙箱姿态变更行。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxProfileChangeDto {
    pub capability_scope: String,
    pub old_profile: String,
    pub new_profile: String,
}

/// 跨版本权限提权差异投影(`requiresZ4Reconsent` 阻断静默升级)。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorePermissionDiffDto {
    pub package_id_hex: String,
    pub old_version: u64,
    pub new_version: u64,
    pub added_capabilities: Vec<AddedCapabilityDto>,
    pub removed_capability_scopes: Vec<String>,
    pub newly_enabled_network_egress: Vec<String>,
    pub escalated_effects: Vec<EffectEscalationDto>,
    pub escalated_to_irreversible: Vec<EffectEscalationDto>,
    pub purpose_changes: Vec<PurposeChangeDto>,
    pub sandbox_profile_changes: Vec<SandboxProfileChangeDto>,
    pub requires_z4_reconsent: bool,
    pub plain_summary_zh: String,
    pub plain_summary_en: String,
}

/// 敏感动作入口输入(仅三类法定动作;`nowMs` 供确定性测试覆盖)。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreSensitiveActionInput {
    pub action_kind: String,
    pub package_id_hex: String,
    pub package_version: u64,
    pub old_version: Option<u64>,
    pub now_ms: Option<u64>,
}

/// 敏感动作入口投影:白话动作说明 + 既有 `Z4` 确认屏请求(确认屏本体复用)。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreSensitiveActionEntryDto {
    pub action_kind: String,
    pub plain_action_zh: String,
    pub plain_action_en: String,
    pub reversibility_class: ReversibilityClass,
    pub z4_request: Z4ConsentChallengeRequest,
}

/// 桥接执行上下文(供 Tauri 命令与无全局环境变量污染的 Rust 集成测试共用)。
#[derive(Clone, Debug, Default)]
pub struct StoreCatalogContext {
    /// `store-registry.db` 所在目录(如 `<root>/store-db`)。
    pub store_root: Option<PathBuf>,
    /// `publisher-trust.db` 精确文件路径。
    pub trust_db_path: Option<PathBuf>,
    /// 是否允许回退读取进程环境变量。生产为 `true`;隔离单测为 `false`。
    pub allow_host_env_discovery: bool,
}

impl StoreCatalogContext {
    /// 构建严格隔离的测试桥接上下文(绝不读取宿主机环境变量)。
    #[must_use]
    pub fn isolated(store_root: Option<PathBuf>, trust_db_path: Option<PathBuf>) -> Self {
        Self {
            store_root,
            trust_db_path,
            allow_host_env_discovery: false,
        }
    }

    fn from_tauri_state(
        state: tauri::State<'_, AppState>,
        store_root_override: Option<String>,
        trust_db_override: Option<String>,
    ) -> Result<Self, DesktopError> {
        let cfg = crate::ipc::get_config(state)?;
        let cleaned = |raw: Option<String>| {
            raw.map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        };
        let resource_root = cfg
            .resource_root
            .or_else(|| std::env::var(ENV_RESOURCE_ROOT).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let store_root = cleaned(store_root_override)
            .or_else(|| cleaned(std::env::var(ENV_STORE_ROOT).ok()))
            // 资源根的约定布局兜底:`<resource_root>/store-db/store-registry.db`。
            .or_else(|| resource_root.as_ref().map(|root| root.join("store-db")));
        let trust_db_path = cleaned(trust_db_override)
            .or_else(|| cleaned(std::env::var(ENV_PUBLISHER_TRUST_DB).ok()));
        Ok(Self {
            store_root,
            trust_db_path,
            allow_host_env_discovery: true,
        })
    }
}

/// 将阻塞型 `SQLite` I/O 放到后台工作线程池,UI 主线程永不因磁盘锁等待卡顿。
async fn run_store_off_main<F, R>(work: F) -> Result<R, DesktopError>
where
    F: FnOnce() -> Result<R, DesktopError> + Send + 'static,
    R: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|_| DesktopError::internal("商店目录后台线程 join 失败"))?
}

fn wall_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn sanitize_registry_error(error: &nlos_store_registry::StoreRegistryError) -> DesktopError {
    use nlos_store_registry::StoreRegistryError as E;
    match error {
        E::ReleaseNotFound { .. } => {
            DesktopError::new(ErrorCode::NotFound, "商店目录中不存在该发布物")
        }
        E::ReleaseRecalled { severity, .. } => DesktopError::new(
            ErrorCode::Control,
            format!(
                "该发布物已被安全撤回({}),入口已停用",
                recall_severity_label_zh(*severity)
            ),
        ),
        E::GovernanceUninitialized => DesktopError::ipc("商店治理目录尚未初始化,当前不可用"),
        E::SchemaVersionUnsupported(_)
        | E::Sqlite(_)
        | E::Io(_)
        | E::DurabilityUnavailable { .. }
        | E::CorruptRecord(_)
        | E::LockPoisoned => DesktopError::ipc("商店治理目录存储读取失败"),
        E::InvalidInput(reason) => DesktopError::config(format!("非法商店目录输入: {reason}")),
        other => DesktopError::ipc(format!("商店目录读取失败 ({other:?})")),
    }
}

fn recall_severity_label_zh(severity: RecallSeverity) -> &'static str {
    match severity {
        RecallSeverity::WarnNewInstall => "新安装警告",
        RecallSeverity::BlockLaunchAndRevokeCap => "阻断启动并撤回权限",
        RecallSeverity::QuarantineInstallation => "隔离安装",
    }
}

fn sanitize_trust_error(error: &PublisherTrustCheckError) -> DesktopError {
    let kind = match error {
        PublisherTrustCheckError::UnknownPublisher(_) => "unknown publisher",
        PublisherTrustCheckError::UntrustedCertificateChain(_) => "untrusted certificate chain",
        PublisherTrustCheckError::ExpiredCertificate { .. } => "expired certificate",
        PublisherTrustCheckError::RevokedCertificate { .. } => "revoked certificate",
        PublisherTrustCheckError::RevokedPublisher { .. } => "revoked publisher",
        PublisherTrustCheckError::StaleOrRollbackRevocationEpoch { .. } => {
            "stale or rolled-back revocation epoch"
        }
        PublisherTrustCheckError::FrozenRevocationList { .. } => "frozen revocation list",
        PublisherTrustCheckError::SignatureInvalid => "invalid signature",
        PublisherTrustCheckError::ApplicationIdHijackAttempt { .. } => {
            "application identity hijack attempt"
        }
        _ => "publisher trust verification failure",
    };
    DesktopError::new(
        ErrorCode::Control,
        format!("发布者证书链核验未通过({kind}),敏感动作入口已拒绝"),
    )
}

fn parse_package_id_hex(raw: &str) -> Result<PackageId, DesktopError> {
    let trimmed = raw.trim();
    if trimmed.len() != 32 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DesktopError::config(
            "packageIdHex 必须是 32 位十六进制发布包标识",
        ));
    }
    let mut bytes = [0_u8; 16];
    for (idx, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&trimmed[2 * idx..2 * idx + 2], 16)
            .map_err(|_| DesktopError::config("packageIdHex 解析失败"))?;
    }
    Ok(PackageId::from_bytes(bytes))
}

fn fingerprint16(bytes: &[u8; 32]) -> String {
    hex(&compute_sha256(bytes)[..8])
}

fn track_label(track: StoreCatalogTrack) -> &'static str {
    match track {
        StoreCatalogTrack::OfficialTrack => "OFFICIAL",
        StoreCatalogTrack::CommunityTrack => "COMMUNITY",
    }
}

fn tier_labels(tier: PublisherTrustTier) -> (&'static str, &'static str) {
    match tier {
        PublisherTrustTier::PlatformCore => ("平台核心发布方", "Platform core publisher"),
        PublisherTrustTier::CertifiedPublisher => ("已认证发布方", "Certified publisher"),
        PublisherTrustTier::CommunitySandboxed => {
            ("社区沙箱发布方", "Community sandboxed publisher")
        }
    }
}

fn effect_labels(effect: EffectClass) -> (&'static str, &'static str) {
    match effect {
        EffectClass::ReadOnly => ("只读", "Read-only"),
        EffectClass::Reversible => ("可逆", "Reversible"),
        EffectClass::Compensatable => ("可补偿", "Compensatable"),
        EffectClass::Irreversible => ("不可逆", "Irreversible"),
    }
}

fn effect_class_str(effect: EffectClass) -> &'static str {
    match effect {
        EffectClass::ReadOnly => "READ_ONLY",
        EffectClass::Reversible => "REVERSIBLE",
        EffectClass::Compensatable => "COMPENSATABLE",
        EffectClass::Irreversible => "IRREVERSIBLE",
    }
}

fn entry_dto(record: &StoreReleaseRecord) -> StoreCatalogEntryDto {
    StoreCatalogEntryDto {
        package_id_hex: hex(record.package_id.as_bytes()),
        package_version: record.package_version,
        application_ids_hex: record
            .application_ids
            .iter()
            .map(|app| hex(app.as_bytes()))
            .collect(),
        publisher_id_hex: hex(record.publisher_id.as_bytes()),
        publisher_tier: record.publisher_trust_tier.as_str().to_owned(),
        track: track_label(record.track).to_owned(),
        channel: record.channel.as_str().to_owned(),
        active_recall_severity: record.active_recall_severity.map(|s| s.as_str().to_owned()),
        component_count: record.components.len() as u32,
        permission_count: record.declared_permissions.len() as u32,
        any_network_egress: record
            .declared_permissions
            .iter()
            .any(|p| p.network_egress_declared),
        any_irreversible_effect: record
            .declared_permissions
            .iter()
            .any(|p| p.effect_class == EffectClass::Irreversible),
        publisher_key_fingerprint_hex: fingerprint16(&record.publisher_signing_key),
        submitted_at_ms: record.submitted_at_ms,
    }
}

fn offline_snapshot() -> StoreCatalogSnapshotDto {
    StoreCatalogSnapshotDto {
        source: STORE_SOURCE_OFFLINE_UNAVAILABLE.to_owned(),
        offline_notice_zh: Some(
            "本地商店目录暂不可用(未初始化或处于离线状态),当前仅显示空白目录;安装与升级入口已停用。"
                .to_owned(),
        ),
        offline_notice_en: Some(
            "Local store catalog is unavailable (not initialized or offline); showing an empty catalog, and install or upgrade entries are paused."
                .to_owned(),
        ),
        store_revocation_epoch: 0,
        official_stable_releases: Vec::new(),
        community_candidates: Vec::new(),
    }
}

fn open_registry(ctx: &StoreCatalogContext) -> Result<Option<SqliteStoreRegistry>, DesktopError> {
    let Some(root) = ctx.store_root.as_deref() else {
        return Ok(None);
    };
    match SqliteStoreRegistry::reopen(root) {
        Ok(registry) => Ok(Some(registry)),
        Err(error) => {
            let mapped = sanitize_registry_error(&error);
            match mapped.code {
                ErrorCode::Ipc => Ok(None),
                other => Err(DesktopError::new(other, mapped.message)),
            }
        }
    }
}

/// 查看时复验发布者证书链:以发布记录重算域分离提交摘要并走真实
/// `PublisherTrustVerifier` 在线窗口(防冻结 CRL)。
fn verify_chain_at_view(
    trust: &SqlitePublisherTrustAuthority,
    record: &StoreReleaseRecord,
    now_ms: u64,
) -> Result<nlos_publisher_trust::VerifiedPublisherIdentity, PublisherTrustCheckError> {
    let digest = compute_release_submission_digest(&UnsignedStoreReleaseSubmission {
        package_id: record.package_id,
        package_version: record.package_version,
        manifest_digest: record.manifest_digest,
        oci_manifest_digest: record.oci_manifest_digest,
        application_ids: &record.application_ids,
        components: &record.components,
        declared_permissions: &record.declared_permissions,
        publisher_id: record.publisher_id,
    });
    trust.verify_publisher_signature_with_mode(
        record.publisher_id,
        &record.publisher_signing_key,
        &digest,
        &record.publisher_signature,
        now_ms,
        TrustFreshnessMode::OnlineInstallOrUpgrade,
    )
}

fn build_trust_chain_dto(
    record: &StoreReleaseRecord,
    trust: Option<&SqlitePublisherTrustAuthority>,
    now_ms: u64,
) -> PublisherTrustChainDto {
    let (tier_label_zh, tier_label_en) = tier_labels(record.publisher_trust_tier);
    let mut chain = PublisherTrustChainDto {
        publisher_id_hex: hex(record.publisher_id.as_bytes()),
        trust_tier: record.publisher_trust_tier.as_str().to_owned(),
        tier_label_zh: tier_label_zh.to_owned(),
        tier_label_en: tier_label_en.to_owned(),
        chain_status: STORE_CHAIN_STATUS_TRUST_DB_UNAVAILABLE.to_owned(),
        chain_status_notice_zh: String::new(),
        chain_status_notice_en: String::new(),
        leaf_key_fingerprint_hex: fingerprint16(&record.publisher_signing_key),
        revocation_epoch: 0,
        crl_fresh_until_ms: 0,
        signature_reverified: false,
    };
    let Some(trust) = trust else {
        chain.chain_status_notice_zh =
            "本地发布者信任库不可用,暂无法在查看时复验证书链;仅展示目录登记信息。".to_owned();
        chain.chain_status_notice_en = "Local publisher trust store is unavailable; the certificate chain cannot be re-verified for this view. Showing catalog registration only.".to_owned();
        return chain;
    };
    if let Ok(snapshot) = trust.inspect_revocation_state() {
        chain.revocation_epoch = snapshot.revocation_epoch.get();
        chain.crl_fresh_until_ms = snapshot.fresh_until_ms;
    }
    match verify_chain_at_view(trust, record, now_ms) {
        Ok(_) => {
            chain.chain_status = STORE_CHAIN_STATUS_VERIFIED.to_owned();
            chain.chain_status_notice_zh = "证书链与发布签名已在本机复验通过。".to_owned();
            chain.chain_status_notice_en =
                "Certificate chain and publisher signature re-verified on this machine.".to_owned();
            chain.signature_reverified = true;
        }
        Err(_) => {
            chain.chain_status = STORE_CHAIN_STATUS_VERIFICATION_FAILED.to_owned();
            chain.chain_status_notice_zh =
                "证书链复验未通过(可能已被撤回或失效),请勿安装或升级该发布物。".to_owned();
            chain.chain_status_notice_en = "Certificate chain re-verification failed (possibly revoked or expired). Do not install or upgrade this release.".to_owned();
        }
    }
    chain
}

fn permission_dto(entry: &DeclaredPermissionEntry) -> DeclaredPermissionDto {
    DeclaredPermissionDto {
        capability_scope: entry.capability_scope.clone(),
        human_purpose: entry.human_purpose.clone(),
        effect_class: effect_class_str(entry.effect_class).to_owned(),
        network_egress_declared: entry.network_egress_declared,
        sandbox_profile: entry.sandbox_profile.clone(),
    }
}

fn effect_legend() -> Vec<EffectClassLabelDto> {
    [
        EffectClass::ReadOnly,
        EffectClass::Reversible,
        EffectClass::Compensatable,
        EffectClass::Irreversible,
    ]
    .into_iter()
    .map(|effect| {
        let (zh, en) = effect_labels(effect);
        EffectClassLabelDto {
            effect_class: effect_class_str(effect).to_owned(),
            label_zh: zh.to_owned(),
            label_en: en.to_owned(),
        }
    })
    .collect()
}

fn transparency_dto(
    record: &StoreReleaseRecord,
    report: &PermissionTransparencyReport,
    trust: Option<&SqlitePublisherTrustAuthority>,
    now_ms: u64,
) -> StoreReleaseTransparencyDto {
    StoreReleaseTransparencyDto {
        package_id_hex: hex(record.package_id.as_bytes()),
        package_version: record.package_version,
        application_ids_hex: record
            .application_ids
            .iter()
            .map(|app| hex(app.as_bytes()))
            .collect(),
        publisher_id_hex: hex(record.publisher_id.as_bytes()),
        publisher_tier: record.publisher_trust_tier.as_str().to_owned(),
        track: track_label(record.track).to_owned(),
        channel: record.channel.as_str().to_owned(),
        permissions: report.permissions.iter().map(permission_dto).collect(),
        any_network_egress: report.any_network_egress,
        any_irreversible_effect: report.any_irreversible_effect,
        effect_legend: effect_legend(),
        trust_chain: build_trust_chain_dto(record, trust, now_ms),
    }
}

fn diff_plain_summaries(diff: &PermissionEscalationDiff) -> (String, String) {
    if !diff.requires_tui_reconsent {
        return (
            "两个版本的权限声明完全一致,无需重新确认。".to_owned(),
            "Permission declarations are identical across both versions; no re-confirmation needed.".to_owned(),
        );
    }
    let irreversible = diff.escalated_to_irreversible.len();
    let egress = diff.newly_enabled_network_egress.len();
    let added = diff.added_capabilities.len();
    (
        format!(
            "新版本请求更强权限:{added} 项新增能力、{egress} 项新开网络出站、{irreversible} 项不可逆变更;必须经系统安全确认屏重新授权后才能升级。"
        ),
        format!(
            "The new version requests stronger permissions: {added} added capabilities, {egress} newly enabled network egress, {irreversible} irreversible changes. Re-authorization on the system security confirmation screen is required before upgrading."
        ),
    )
}

fn diff_dto(
    package_id: PackageId,
    old_version: u64,
    new_version: u64,
    diff: &PermissionEscalationDiff,
) -> StorePermissionDiffDto {
    let (plain_summary_zh, plain_summary_en) = diff_plain_summaries(diff);
    StorePermissionDiffDto {
        package_id_hex: hex(package_id.as_bytes()),
        old_version,
        new_version,
        added_capabilities: diff
            .added_capabilities
            .iter()
            .map(|entry| AddedCapabilityDto {
                capability_scope: entry.capability_scope.clone(),
                human_purpose: entry.human_purpose.clone(),
                effect_class: effect_class_str(entry.effect_class).to_owned(),
                network_egress_declared: entry.network_egress_declared,
            })
            .collect(),
        removed_capability_scopes: diff.removed_capability_scopes.clone(),
        newly_enabled_network_egress: diff.newly_enabled_network_egress.clone(),
        escalated_effects: diff
            .escalated_effects
            .iter()
            .map(|row| EffectEscalationDto {
                capability_scope: row.capability_scope.clone(),
                old_effect: effect_class_str(row.old_effect).to_owned(),
                new_effect: effect_class_str(row.new_effect).to_owned(),
            })
            .collect(),
        escalated_to_irreversible: diff
            .escalated_to_irreversible
            .iter()
            .map(|row| EffectEscalationDto {
                capability_scope: row.capability_scope.clone(),
                old_effect: effect_class_str(row.old_effect).to_owned(),
                new_effect: effect_class_str(row.new_effect).to_owned(),
            })
            .collect(),
        purpose_changes: diff
            .purpose_changes
            .iter()
            .map(|row| PurposeChangeDto {
                capability_scope: row.capability_scope.clone(),
                old_purpose: row.old_purpose.clone(),
                new_purpose: row.new_purpose.clone(),
            })
            .collect(),
        sandbox_profile_changes: diff
            .sandbox_profile_changes
            .iter()
            .map(|row| SandboxProfileChangeDto {
                capability_scope: row.capability_scope.clone(),
                old_profile: row.old_sandbox_profile.clone(),
                new_profile: row.new_sandbox_profile.clone(),
            })
            .collect(),
        requires_z4_reconsent: diff.requires_tui_reconsent,
        plain_summary_zh,
        plain_summary_en,
    }
}

fn permission_blast_rows(record: &StoreReleaseRecord) -> Vec<Z4BlastRadiusRowDto> {
    let mut rows = Vec::new();
    for entry in &record.declared_permissions {
        rows.push(Z4BlastRadiusRowDto {
            symbol: "+".to_owned(),
            category: "能力授权".to_owned(),
            scope_label: entry.capability_scope.clone(),
            target_label: entry.capability_scope.clone(),
            reason: entry.human_purpose.clone(),
            target_id_hex: None,
        });
        if entry.network_egress_declared {
            rows.push(Z4BlastRadiusRowDto {
                symbol: "~".to_owned(),
                category: "网络出站".to_owned(),
                scope_label: entry.capability_scope.clone(),
                target_label: entry.capability_scope.clone(),
                reason: entry.human_purpose.clone(),
                target_id_hex: None,
            });
        }
    }
    rows
}

fn diff_blast_rows(diff: &PermissionEscalationDiff) -> Vec<Z4BlastRadiusRowDto> {
    let mut rows = Vec::new();
    for entry in &diff.added_capabilities {
        rows.push(Z4BlastRadiusRowDto {
            symbol: "+".to_owned(),
            category: "新增能力授权".to_owned(),
            scope_label: entry.capability_scope.clone(),
            target_label: entry.capability_scope.clone(),
            reason: entry.human_purpose.clone(),
            target_id_hex: None,
        });
        if entry.network_egress_declared {
            rows.push(Z4BlastRadiusRowDto {
                symbol: "~".to_owned(),
                category: "新开网络出站".to_owned(),
                scope_label: entry.capability_scope.clone(),
                target_label: entry.capability_scope.clone(),
                reason: entry.human_purpose.clone(),
                target_id_hex: None,
            });
        }
    }
    for row in &diff.escalated_to_irreversible {
        rows.push(Z4BlastRadiusRowDto {
            symbol: "~".to_owned(),
            category: "升级为不可逆".to_owned(),
            scope_label: row.capability_scope.clone(),
            target_label: row.capability_scope.clone(),
            reason: format!(
                "{} -> {}",
                effect_class_str(row.old_effect),
                effect_class_str(row.new_effect)
            ),
            target_id_hex: None,
        });
    }
    rows
}

#[allow(clippy::too_many_lines)]
fn build_sensitive_entry(
    action_kind: &str,
    record: &StoreReleaseRecord,
    diff: Option<&PermissionEscalationDiff>,
    chain_verified: bool,
) -> StoreSensitiveActionEntryDto {
    let (tier_label_zh, _) = tier_labels(record.publisher_trust_tier);
    let pkg_short = hex(&record.package_id.as_bytes()[..4]);
    let version = record.package_version;
    let any_irreversible = record
        .declared_permissions
        .iter()
        .any(|p| p.effect_class == EffectClass::Irreversible);
    let any_egress = record
        .declared_permissions
        .iter()
        .any(|p| p.network_egress_declared);
    let permission_count = record.declared_permissions.len();

    let (reversibility, plain_action_zh, plain_action_en, consequence_zh, consequence_en) =
        match action_kind {
            STORE_ACTION_IRREVERSIBLE_UPGRADE => (
                ReversibilityClass::Irreversible,
                format!("升级应用包至版本 {version};新版权限声明存在差异,须逐项核对后确认。"),
                format!("Upgrade the application package to version {version}; the new permission declaration differs and each change must be reviewed."),
                "该升级包含不可逆变更,执行后无法自动回退到原版本。".to_owned(),
                "This upgrade contains irreversible changes and cannot be automatically rolled back to the previous version.".to_owned(),
            ),
            STORE_ACTION_DOWNGRADE_SANDBOX => (
                ReversibilityClass::Irreversible,
                format!("将社区应用包 {pkg_short} 降档为强沙箱运行:仅在隔离环境内执行,禁用直连宿主能力。"),
                format!("Downgrade community package {pkg_short} to strict sandboxed execution: it runs only inside the isolation layer with no direct host access."),
                "降档将改变该应用的运行姿态,部分能力可能不可用,恢复需重新确认。".to_owned(),
                "Downgrading changes the runtime posture of this application; some capabilities may become unavailable, and restoring them requires re-confirmation.".to_owned(),
            ),
            _ => (
                if any_irreversible {
                    ReversibilityClass::Irreversible
                } else {
                    ReversibilityClass::CompensatableRollback
                },
                format!(
                    "从受控商店目录安装应用包 {pkg_short}(版本 {version}),将授予 {permission_count} 项能力权限;安装前请核对用途与出站声明。"
                ),
                format!(
                    "Install application package {pkg_short} (version {version}) from the governed store catalog; {permission_count} capability permissions will be granted. Review purposes and egress declarations before installing."
                ),
                if any_irreversible {
                    "该发布物声明了不可逆操作能力,安装后其部分操作将无法自动撤销。".to_owned()
                } else {
                    "安装本身可通过卸载补偿;卸载会保留用户数据目录。".to_owned()
                },
                if any_irreversible {
                    "This release declares irreversible operation capabilities; some of its actions cannot be automatically undone after installation.".to_owned()
                } else {
                    "Installation itself is compensatable via uninstall; user data directories are preserved.".to_owned()
                },
            ),
        };

    let egress_label_zh = if any_egress { "允许" } else { "禁止" };
    let egress_label_en = if any_egress { "allowed" } else { "denied" };
    let size_mib = record
        .components
        .iter()
        .map(|slot| slot.size_bytes)
        .sum::<u64>()
        / (1024 * 1024);

    let blast_rows = match diff {
        Some(diff) if diff.requires_tui_reconsent => {
            let mut rows = diff_blast_rows(diff);
            rows.extend(permission_blast_rows(record));
            rows
        }
        _ => permission_blast_rows(record),
    };

    let request = Z4ConsentChallengeRequest {
        requesting_app: Z4RequestingAppSpec {
            display_name: format!("应用包 {pkg_short} v{version}"),
            publisher_name: format!("{tier_label_zh} {pkg_short}"),
            signature_verified: chain_verified,
            application_id_hex: record
                .application_ids
                .first()
                .map(|app| hex(app.as_bytes()))
                .unwrap_or_else(|| "0".repeat(32)),
            installation_id_hex: "0".repeat(32),
            principal_id_hex: "0".repeat(32),
            installation_generation: 0,
            manifest_digest_hex: hex(&record.manifest_digest),
        },
        action_scope: Z4ActionScopeSpec {
            plain_summary_zh: plain_action_zh.clone(),
            plain_summary_en: plain_action_en.clone(),
            capability_rights: record
                .declared_permissions
                .iter()
                .map(|p| p.capability_scope.clone())
                .collect(),
            control_scope: "StoreCatalog".to_owned(),
            plan_digest_hex: hex(&record.oci_manifest_digest),
        },
        resource_ceiling: Z4ResourceCeilingSpec {
            plain_ceiling_zh: format!(
                "组件总大小上限约 {size_mib} MiB;网络出站:{egress_label_zh};令牌预算:未声明"
            ),
            plain_ceiling_en: format!(
                "Component size ceiling about {size_mib} MiB; network egress {egress_label_en}; token budget: undeclared"
            ),
            memory_ceiling_mib: size_mib,
            token_budget_ceiling: 0,
            network_egress_allowed: any_egress,
            max_duration_ms: 0,
        },
        reversibility_and_risk: Z4ReversibilityRiskSpec {
            reversibility_class: reversibility,
            plain_consequence_zh: consequence_zh,
            plain_consequence_en: consequence_en,
            multi_target_count: permission_count as u32,
            partial_failure_disclosure_zh: None,
            partial_failure_disclosure_en: None,
            blast_radius_rows: blast_rows,
            requires_data_loss_receipt: false,
        },
        allowed_grant_scopes: vec![ConsentGrantScope::SingleAction],
    };

    StoreSensitiveActionEntryDto {
        action_kind: action_kind.to_owned(),
        plain_action_zh,
        plain_action_en,
        reversibility_class: reversibility,
        z4_request: request,
    }
}

// ---------------------------------------------------------------------------
// 无 Tauri 句柄核心(集成测试与 Tauri 命令共用)
// ---------------------------------------------------------------------------

/// 读取商店目录快照(浏览面;治理目录不可用时确定性降级,不返回错误)。
///
/// # Errors
///
/// 仅在治理目录存在但读取以非降级形态失败时返回错误。
pub async fn store_catalog_snapshot_with_context(
    ctx: StoreCatalogContext,
    now_ms: u64,
) -> Result<StoreCatalogSnapshotDto, DesktopError> {
    run_store_off_main(move || {
        let Some(registry) = open_registry(&ctx)? else {
            return Ok(offline_snapshot());
        };
        let store_revocation_epoch = registry
            .current_store_revocation_epoch()
            .map(|epoch| epoch.get())
            .unwrap_or(0);
        let official = registry
            .list_official_stable_releases()
            .map_err(|e| sanitize_registry_error(&e))?;
        let community = registry
            .list_community_sandbox_candidates()
            .map_err(|e| sanitize_registry_error(&e))?;
        let _ = now_ms;
        Ok(StoreCatalogSnapshotDto {
            source: STORE_SOURCE_LIVE_REGISTRY.to_owned(),
            offline_notice_zh: None,
            offline_notice_en: None,
            store_revocation_epoch,
            official_stable_releases: official.iter().map(entry_dto).collect(),
            community_candidates: community.iter().map(entry_dto).collect(),
        })
    })
    .await
}

/// 读取单发布物权限透明报告与发布者证书链复验状态(详情检视面)。
///
/// # Errors
///
/// 发布物不存在返回 `NotFound`;治理目录不可用按 `Ipc` 降级错误。
pub async fn store_release_transparency_with_context(
    ctx: StoreCatalogContext,
    package_id_hex: &str,
    package_version: u64,
    now_ms: u64,
) -> Result<StoreReleaseTransparencyDto, DesktopError> {
    let package_id = parse_package_id_hex(package_id_hex)?;
    let ctx = ctx.clone();
    run_store_off_main(move || {
        let Some(registry) = open_registry(&ctx)? else {
            // 离线态不伪造明细:单发布物检视 fail-closed。
            return Err(DesktopError::new(
                ErrorCode::NotFound,
                "商店目录当前不可用,无法读取该发布物的透明报告",
            ));
        };
        let report = registry
            .inspect_permission_transparency(package_id, package_version)
            .map_err(|e| sanitize_registry_error(&e))?;
        let record = registry
            .inspect_release(package_id, package_version)
            .map_err(|e| sanitize_registry_error(&e))?
            .ok_or_else(|| DesktopError::new(ErrorCode::NotFound, "商店目录中不存在该发布物"))?;
        let trust = ctx
            .trust_db_path
            .as_deref()
            .and_then(|path| SqlitePublisherTrustAuthority::open(path).ok());
        Ok(transparency_dto(&record, &report, trust.as_ref(), now_ms))
    })
    .await
}

/// 计算跨版本权限提权差异(`requiresZ4Reconsent` 阻断静默升级)。
///
/// # Errors
///
/// 任一版本不存在返回 `NotFound`。
pub async fn store_permission_diff_with_context(
    ctx: StoreCatalogContext,
    package_id_hex: &str,
    old_version: u64,
    new_version: u64,
) -> Result<StorePermissionDiffDto, DesktopError> {
    let package_id = parse_package_id_hex(package_id_hex)?;
    run_store_off_main(move || {
        let Some(registry) = open_registry(&ctx)? else {
            return Err(DesktopError::new(
                ErrorCode::NotFound,
                "商店目录当前不可用,无法比对权限差异",
            ));
        };
        let diff = registry
            .diff_release_permission_transparency(package_id, old_version, new_version)
            .map_err(|e| sanitize_registry_error(&e))?;
        Ok(diff_dto(package_id, old_version, new_version, &diff))
    })
    .await
}

/// 构造敏感动作入口(安装 / 不可逆升级 / 降档沙箱):仅产出既有 `Z4`
/// 确认屏请求投影,链路未核验、发布物被撤回或输入非法一律 fail-closed。
///
/// # Errors
///
/// 未知动作种类/非法标识返回 `Config`;发布物不存在返回 `NotFound`;
/// 链路未核验或发布物被撤回返回 `Control`。
pub async fn store_prepare_sensitive_action_with_context(
    ctx: StoreCatalogContext,
    input: StoreSensitiveActionInput,
) -> Result<StoreSensitiveActionEntryDto, DesktopError> {
    match input.action_kind.as_str() {
        STORE_ACTION_INSTALL
        | STORE_ACTION_IRREVERSIBLE_UPGRADE
        | STORE_ACTION_DOWNGRADE_SANDBOX => {}
        other => {
            return Err(DesktopError::config(format!(
                "未知的商店敏感动作种类: '{other}'"
            )));
        }
    }
    if input.action_kind == STORE_ACTION_IRREVERSIBLE_UPGRADE && input.old_version.is_none() {
        return Err(DesktopError::config(
            "不可逆升级必须提供原版本号以比对权限差异",
        ));
    }
    let package_id = parse_package_id_hex(&input.package_id_hex)?;
    let now_ms = input.now_ms.unwrap_or_else(wall_now_ms);
    run_store_off_main(move || {
        let Some(registry) = open_registry(&ctx)? else {
            return Err(DesktopError::new(
                ErrorCode::Control,
                "商店治理目录不可用,敏感动作入口已停用",
            ));
        };
        // 新安装门禁:撤回包在此 fail-closed(ReleaseRecalled -> Control)。
        let record = registry
            .evaluate_new_install_gate(package_id, input.package_version)
            .map_err(|e| sanitize_registry_error(&e))?;
        let Some(trust_path) = ctx.trust_db_path.as_deref() else {
            return Err(DesktopError::new(
                ErrorCode::Control,
                "本地发布者信任库未配置,敏感动作入口已停用(链路未核验绝不放行)",
            ));
        };
        let trust = SqlitePublisherTrustAuthority::open(trust_path).map_err(|_| {
            DesktopError::new(
                ErrorCode::Control,
                "本地发布者信任库不可读,敏感动作入口已停用",
            )
        })?;
        let verified =
            verify_chain_at_view(&trust, &record, now_ms).map_err(|e| sanitize_trust_error(&e))?;
        let _ = verified;

        let diff = if input.action_kind == STORE_ACTION_IRREVERSIBLE_UPGRADE {
            let old_version = input.old_version.unwrap_or(input.package_version);
            let diff = registry
                .diff_release_permission_transparency(
                    package_id,
                    old_version,
                    input.package_version,
                )
                .map_err(|e| sanitize_registry_error(&e))?;
            Some(diff)
        } else {
            None
        };

        Ok(build_sensitive_entry(
            &input.action_kind,
            &record,
            diff.as_ref(),
            true,
        ))
    })
    .await
}

// ---------------------------------------------------------------------------
// Tauri 命令薄壳(invoke_handler 挂接归 W80-INT)
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn store_catalog_snapshot(
    state: tauri::State<'_, AppState>,
    store_root_override: Option<String>,
    trust_db_override: Option<String>,
    now_ms: Option<u64>,
) -> Result<StoreCatalogSnapshotDto, DesktopError> {
    let ctx = StoreCatalogContext::from_tauri_state(state, store_root_override, trust_db_override)?;
    store_catalog_snapshot_with_context(ctx, now_ms.unwrap_or_else(wall_now_ms)).await
}

#[tauri::command]
pub async fn store_release_transparency(
    state: tauri::State<'_, AppState>,
    package_id_hex: String,
    package_version: u64,
    now_ms: Option<u64>,
    store_root_override: Option<String>,
    trust_db_override: Option<String>,
) -> Result<StoreReleaseTransparencyDto, DesktopError> {
    let ctx = StoreCatalogContext::from_tauri_state(state, store_root_override, trust_db_override)?;
    store_release_transparency_with_context(
        ctx,
        &package_id_hex,
        package_version,
        now_ms.unwrap_or_else(wall_now_ms),
    )
    .await
}

#[tauri::command]
pub async fn store_permission_diff(
    state: tauri::State<'_, AppState>,
    package_id_hex: String,
    old_version: u64,
    new_version: u64,
    store_root_override: Option<String>,
    trust_db_override: Option<String>,
) -> Result<StorePermissionDiffDto, DesktopError> {
    let ctx = StoreCatalogContext::from_tauri_state(state, store_root_override, trust_db_override)?;
    store_permission_diff_with_context(ctx, &package_id_hex, old_version, new_version).await
}

#[tauri::command]
pub async fn store_prepare_sensitive_action(
    state: tauri::State<'_, AppState>,
    input: StoreSensitiveActionInput,
    store_root_override: Option<String>,
    trust_db_override: Option<String>,
) -> Result<StoreSensitiveActionEntryDto, DesktopError> {
    let ctx = StoreCatalogContext::from_tauri_state(state, store_root_override, trust_db_override)?;
    store_prepare_sensitive_action_with_context(ctx, input).await
}

// ---------------------------------------------------------------------------
// 模块内验证:双语白话文案零内部行话、零 Emoji 静态纪律
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a11y_i18n::{contains_forbidden_l0_to_l2_jargon, contains_pictographic_emoji};
    use nlos_publisher_trust::PublisherId;
    use nlos_store_registry::StoreChannel;

    fn assert_plain(label: &str, text: &str) {
        assert!(
            contains_forbidden_l0_to_l2_jargon(text).is_none(),
            "{label}: {text}"
        );
        assert!(!contains_pictographic_emoji(text), "{label}: {text}");
    }

    #[test]
    fn bilingual_plain_copy_is_jargon_free_and_emoji_free() {
        let offline = offline_snapshot();
        assert_plain(
            "offline zh",
            offline.offline_notice_zh.as_deref().unwrap_or(""),
        );
        assert_plain(
            "offline en",
            offline.offline_notice_en.as_deref().unwrap_or(""),
        );

        for legend in effect_legend() {
            assert_plain("legend zh", &legend.label_zh);
            assert_plain("legend en", &legend.label_en);
        }

        for tier in [
            PublisherTrustTier::PlatformCore,
            PublisherTrustTier::CertifiedPublisher,
            PublisherTrustTier::CommunitySandboxed,
        ] {
            let (zh, en) = tier_labels(tier);
            assert_plain("tier zh", zh);
            assert_plain("tier en", en);
        }

        let record = StoreReleaseRecord {
            package_id: PackageId::from_bytes([0xA1; 16]),
            package_version: 1,
            manifest_digest: [0x5E; 32],
            oci_manifest_digest: [0x6E; 32],
            publisher_id: PublisherId::from_bytes([0x41; 16]),
            publisher_trust_tier: PublisherTrustTier::CertifiedPublisher,
            publisher_signing_key: [0x04; 32],
            publisher_signature: [0u8; 64],
            channel: StoreChannel::Stable,
            track: StoreCatalogTrack::OfficialTrack,
            active_recall_severity: None,
            application_ids: vec![nlos_types::ApplicationId::from_bytes([0x33; 16])],
            components: vec![nlos_store_registry::StoreComponentSlot {
                slot_name: "main-cli".to_owned(),
                kind: nlos_store_registry::StoreComponentKind::Executable,
                entrypoint: "bin/main.wasm".to_owned(),
                content_sha256: [0u8; 32],
                size_bytes: 2 * 1024 * 1024,
            }],
            declared_permissions: vec![
                DeclaredPermissionEntry {
                    capability_scope: "sabi.artifact.read".to_owned(),
                    human_purpose: "读取用户笔记".to_owned(),
                    effect_class: EffectClass::ReadOnly,
                    network_egress_declared: false,
                    sandbox_profile: "wasm-strict-v1".to_owned(),
                },
                DeclaredPermissionEntry {
                    capability_scope: "sabi.sync.push".to_owned(),
                    human_purpose: "推送同步数据".to_owned(),
                    effect_class: EffectClass::Irreversible,
                    network_egress_declared: true,
                    sandbox_profile: "wasm-strict-v2".to_owned(),
                },
            ],
            submitted_at_ms: 2_900,
            updated_at_ms: 2_900,
        };

        for kind in [
            STORE_ACTION_INSTALL,
            STORE_ACTION_IRREVERSIBLE_UPGRADE,
            STORE_ACTION_DOWNGRADE_SANDBOX,
        ] {
            let entry = build_sensitive_entry(kind, &record, None, true);
            assert_plain("entry zh", &entry.plain_action_zh);
            assert_plain("entry en", &entry.plain_action_en);
            assert_plain(
                "z4 summary zh",
                &entry.z4_request.action_scope.plain_summary_zh,
            );
            assert_plain(
                "z4 summary en",
                &entry.z4_request.action_scope.plain_summary_en,
            );
            assert_plain(
                "ceiling zh",
                &entry.z4_request.resource_ceiling.plain_ceiling_zh,
            );
            assert_plain(
                "ceiling en",
                &entry.z4_request.resource_ceiling.plain_ceiling_en,
            );
            assert_plain(
                "consequence zh",
                &entry.z4_request.reversibility_and_risk.plain_consequence_zh,
            );
            assert_plain(
                "consequence en",
                &entry.z4_request.reversibility_and_risk.plain_consequence_en,
            );
        }

        // 提权差异白话摘要(旧=单权限,新=全量 -> 存在新增能力)。
        let escalated = nlos_store_registry::diff_permission_transparency(
            &record.declared_permissions[..1],
            &record.declared_permissions,
        );
        let (zh, en) = diff_plain_summaries(&escalated);
        assert_plain("diff zh", &zh);
        assert_plain("diff en", &en);
        let identical = nlos_store_registry::diff_permission_transparency(
            &record.declared_permissions,
            &record.declared_permissions,
        );
        let (zh, en) = diff_plain_summaries(&identical);
        assert_plain("identical diff zh", &zh);
        assert_plain("identical diff en", &en);

        // 撤回等级标签与错误消息同样零行话。
        for severity in [
            RecallSeverity::WarnNewInstall,
            RecallSeverity::BlockLaunchAndRevokeCap,
            RecallSeverity::QuarantineInstallation,
        ] {
            assert_plain("recall zh", recall_severity_label_zh(severity));
        }
        assert_plain(
            "release not found",
            &sanitize_registry_error(&nlos_store_registry::StoreRegistryError::ReleaseNotFound {
                package_id: PackageId::from_bytes([0; 16]),
                package_version: 0,
            })
            .message,
        );
    }

    #[test]
    fn package_id_hex_parsing_rejects_malformed_input() {
        assert!(parse_package_id_hex("ab").is_err());
        assert!(parse_package_id_hex(&"a".repeat(33)).is_err());
        assert!(parse_package_id_hex(&"g".repeat(32)).is_err());
        let parsed = parse_package_id_hex(&"a1".repeat(16)).expect("valid hex id");
        assert_eq!(*parsed.as_bytes(), [0xA1; 16]);
    }

    #[test]
    fn fingerprint_and_entry_projection_shapes() {
        let fingerprint = fingerprint16(&[0x04; 32]);
        assert_eq!(fingerprint.len(), 16);
        assert!(fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
