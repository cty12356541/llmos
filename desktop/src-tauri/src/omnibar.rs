//! Stage D `D-OMNIBAR-MULTIMODAL` (`W71-D-OMNIBAR`) 按需唤起实心 `Z3` 命令与意图面板、
//! 渐进式自然语言意图编译器、`[NL-AMBIG-001]` 强制澄清硬门与多模态权限状态机。
//!
//! 权威契约对齐：
//! - `docs/design/06-架构设计总纲-v0.5.md` §24.1 (`[NL-COMPILE-001]`, `[NL-PROPOSE-001]`,
//!   `[NL-AMBIG-001]`, `[NL-BIND-001]`), §24.2 (`[TUI-CHOOSER-001]`, `[TUI-EXT-001]`),
//!   §25.4 (`[DUI-LAUNCH-001]`, `[DUI-INPUT-001]`, `[DUI-CHOOSER-001]`, `[DUI-SESSION-001]`), §28.4
//! - `docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md`
//! - `docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md` §0.1, §2.4, §2A, §3.3

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Mutex, OnceLock};

use nlos_application::ApplicationAuthority;
use nlos_schema::SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL;
use nlos_system_control::control::parse_hex_id;
use nlos_system_control::nl::parse_nl_command;
use nlos_types::{ApplicationId, InstallationId};
use serde::{Deserialize, Serialize};

use crate::compositor::{RectPx, WorkstationHostShell, Z3_PALETTE_Z};
use crate::dto::hex;
use crate::error::{DesktopError, ErrorCode};
use crate::input_router::{
    InputEventEnvelope, InputOrigin, InputRejectionReason, InputSource, InputTarget,
};
use crate::ipc::AppState;
use crate::session::DesktopSessionState;
use crate::surfaces::{AuthorityLaunchResolutionError, resolve_launch_from_authority};

/// `Z3` 全局命令与意图面板唯一叠层 ID。
pub const OMNIBAR_OVERLAY_ID: &str = "z3-omnibar";
/// `Z3` 命令面板固定平面层级（`460`）。
pub const OMNIBAR_Z_INDEX: u16 = Z3_PALETTE_Z;
/// 命令面板与多模态附件芯片固定圆角（`4px`）。
pub const OMNIBAR_CONTROL_RADIUS_PX: u8 = 4;
/// 底部 `36px` 任务栏内紧凑命令触发槽高度（`26px`）。
pub const OMNIBAR_TASKBAR_TRIGGER_HEIGHT_PX: u32 = 26;
/// 屏幕顶部常驻像素占用（严格为 `0px`，绝非常驻大输入框）。
pub const OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX: u32 = 0;

/// 版本化结构意图 Schema 标识（`[NL-COMPILE-001]`）。
pub const TYPED_INTENT_SCHEMA_ID: &str = "nlos.intent.v1";
/// 版本化结构意图 Schema 版本。
pub const TYPED_INTENT_SCHEMA_VERSION: &str = "1.0";
/// 结构化行动提案预览 Schema 标识（`[NL-PROPOSE-001]`）。
pub const ACTION_PROPOSAL_SCHEMA_ID: &str = "nlos.action-proposal.v1";

/// 桌面工作站双语区域标识。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum OmnibarLocale {
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en-US")]
    EnUs,
}

impl OmnibarLocale {
    #[must_use]
    pub fn parse_lossy(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("en-US" | "en_US" | "en") => Self::EnUs,
            _ => Self::ZhCn,
        }
    }
}

/// 命令面板唤起入口。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OmnibarOpenSource {
    KeyboardShortcut,
    TaskbarTrigger,
    Programmatic,
}

/// 命令面板收起原因（`Escape` 与面板外点击均完整保留草稿）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OmnibarCloseReason {
    Escape,
    OutsideClick,
    Submitted,
    SecurityGate,
    Programmatic,
}

/// 可逆性等级（`[NL-PROPOSE-001]`, `[RCPT-UNDO-001]`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProposalUndoClass {
    Lossless,
    Compensatable,
    Irreversible,
}

impl ProposalUndoClass {
    #[must_use]
    pub const fn badge_label(self, locale: OmnibarLocale) -> &'static str {
        match (self, locale) {
            (Self::Lossless, OmnibarLocale::ZhCn) => "可无损撤销",
            (Self::Compensatable, OmnibarLocale::ZhCn) => "可补偿回滚",
            (Self::Irreversible, OmnibarLocale::ZhCn) => "不可逆",
            (Self::Lossless, OmnibarLocale::EnUs) => "Losslessly Reversible",
            (Self::Compensatable, OmnibarLocale::EnUs) => "Compensatable Rollback",
            (Self::Irreversible, OmnibarLocale::EnUs) => "Irreversible",
        }
    }

    #[must_use]
    pub const fn compensation_note(self, locale: OmnibarLocale) -> &'static str {
        match (self, locale) {
            (Self::Lossless, OmnibarLocale::ZhCn) => {
                "执行后可完整恢复至原工作区状态，不留残余改动。"
            }
            (Self::Compensatable, OmnibarLocale::ZhCn) => {
                "涉及跨应用或外部变更，回退时将自动执行补偿恢复步骤。"
            }
            (Self::Irreversible, OmnibarLocale::ZhCn) => {
                "一旦执行无法撤回，提交后必须经过系统安全确认屏二次核准。"
            }
            (Self::Lossless, OmnibarLocale::EnUs) => {
                "Can be reverted cleanly without leaving residual workspace changes."
            }
            (Self::Compensatable, OmnibarLocale::EnUs) => {
                "Produces external or cross-app changes recoverable via a compensating step."
            }
            (Self::Irreversible, OmnibarLocale::EnUs) => {
                "Cannot be undone once executed; requires explicit system security confirmation."
            }
        }
    }
}

/// 附件与数据流污染标记（`[TUI-EXT-001]`, `[DUI-CLIP-001]`）。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaintFlag {
    ExternalOrigin,
    CrossApplication,
    ConfidentialSecret,
    UnverifiedInput,
}

impl TaintFlag {
    #[must_use]
    pub const fn plain_label(self, locale: OmnibarLocale) -> &'static str {
        match (self, locale) {
            (Self::ExternalOrigin, OmnibarLocale::ZhCn) => "外部来源",
            (Self::CrossApplication, OmnibarLocale::ZhCn) => "跨应用数据",
            (Self::ConfidentialSecret, OmnibarLocale::ZhCn) => "敏感内容",
            (Self::UnverifiedInput, OmnibarLocale::ZhCn) => "待核验输入",
            (Self::ExternalOrigin, OmnibarLocale::EnUs) => "External source",
            (Self::CrossApplication, OmnibarLocale::EnUs) => "Cross-app data",
            (Self::ConfidentialSecret, OmnibarLocale::EnUs) => "Sensitive content",
            (Self::UnverifiedInput, OmnibarLocale::EnUs) => "Unverified input",
        }
    }
}

/// 多模态附件芯片类别（`4px` 圆角实心芯片）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttachmentKind {
    Screenshot,
    ChooserFile,
    Artifact,
}

/// 已校验的多模态附件芯片 DTO（不泄露未授权宿主文件系统原始路径）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentChipDto {
    pub attachment_id: String,
    pub kind: AttachmentKind,
    pub display_label: String,
    pub mime_type: String,
    pub byte_length: u64,
    pub chip_border_radius_px: u8,
    pub content_digest_hex: String,
    pub chooser_grant_id_hex: Option<String>,
    pub scoped_capability_id_hex: Option<String>,
    pub artifact_id_hex: Option<String>,
    pub taint_flags: Vec<TaintFlag>,
    pub explicit_host_path_authorized: bool,
    pub authorized_host_path: Option<String>,
}

/// 添加多模态附件的请求输入。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentChipInput {
    pub attachment_id: Option<String>,
    pub kind: AttachmentKind,
    pub label_or_path: String,
    pub mime_type: Option<String>,
    pub byte_length: u64,
    pub content_digest_hex: String,
    pub chooser_grant_id_hex: Option<String>,
    pub scoped_capability_id_hex: Option<String>,
    pub artifact_id_hex: Option<String>,
    pub taint_flags: Vec<TaintFlag>,
    #[serde(default)]
    pub explicit_host_path_authorized: bool,
}

/// 硬件采集权限授权等级。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HardwarePermissionGrant {
    #[default]
    PromptRequired,
    Granted,
    Denied,
}

/// 流式语音输入状态机相位（`Idle -> RequestingMicPermission -> Capturing -> Transcribing -> Committed | Cancelled | Denied`）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum VoiceStreamPhase {
    #[default]
    Idle,
    RequestingMicPermission,
    Capturing,
    Transcribing,
    Committed,
    Cancelled,
    Denied,
}

/// 语音采集被迫切断原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VoiceSeveranceReason {
    SessionNotActive,
    Z4ModalActive,
    MicPermissionRevoked,
    UntrustedOriginSpoof,
}

/// `36px` `L0` 底部任务栏托盘硬件指示器源状态（`DevicePermissionState`）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevicePermissionStateDto {
    pub mic_permission: HardwarePermissionGrant,
    pub camera_permission: HardwarePermissionGrant,
    pub screen_capture_permission: HardwarePermissionGrant,
    pub mic_active: bool,
    pub camera_active: bool,
    pub screen_capture_active: bool,
}

impl DevicePermissionStateDto {
    /// 投影到底部 `36px` `L0` 任务栏右侧硬件采集指示灯的布尔三元组。
    #[must_use]
    pub const fn taskbar_tray_projection(self) -> (bool, bool, bool) {
        (
            matches!(self.mic_permission, HardwarePermissionGrant::Granted) && self.mic_active,
            matches!(self.camera_permission, HardwarePermissionGrant::Granted)
                && self.camera_active,
            matches!(
                self.screen_capture_permission,
                HardwarePermissionGrant::Granted
            ) && self.screen_capture_active,
        )
    }
}

/// 语音流快照 DTO。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceStreamSnapshotDto {
    pub phase: VoiceStreamPhase,
    pub streaming_transcript: String,
    pub committed_transcript: String,
    pub device_permissions: DevicePermissionStateDto,
    pub last_severance_reason: Option<VoiceSeveranceReason>,
    pub severed_count: u64,
    pub rejected_attempt_count: u64,
}

/// 编译后的结构化意图大类。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CompiledIntentKind {
    LaunchApplication,
    BackgroundTask,
    ControlOperation,
    InspectReadonly,
}

/// 未消解歧义类别（`[NL-AMBIG-001]`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UnresolvedAmbiguityKind {
    HighRiskIrreversibleEffect,
    CrossApplicationTransfer,
    MultipleMatchingTargets,
    BudgetEscalation,
}

/// 待用户显式澄清的歧义条目。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnresolvedAmbiguityDto {
    pub kind: UnresolvedAmbiguityKind,
    pub plain_reason: String,
}

/// 候选解释对比表行（`CandidateInterpretationRow`，`[NL-AMBIG-001]`）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateInterpretationDto {
    pub candidate_id: String,
    pub target_app_name: String,
    pub target_application_id_hex: String,
    pub target_installation_id_hex: String,
    pub target_generation: u64,
    pub affected_scope_summary: String,
    pub side_effect_label: String,
    pub undo_class: ProposalUndoClass,
    pub undo_badge_label: String,
    pub budget_ceiling_units: u64,
    pub budget_impact_summary: String,
    pub candidate_digest_hex: String,
}

/// 已核验的应用安装依赖句柄（`[NL-BIND-001]` / `[DUI-LAUNCH-001]`）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedApplicationHandleDto {
    pub application_id_hex: String,
    pub installation_id_hex: String,
    pub generation: u64,
    pub display_name: String,
}

/// 已核验的内容寻址结果引用句柄（`ArtifactId + ContentDigest`，`[NL-BIND-001]`）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedArtifactHandleDto {
    pub artifact_id_hex: String,
    pub content_digest_hex: String,
    pub generation: u64,
    pub display_label: String,
}

/// `L3` 完整版本化 `TypedIntent` 信封投影（`[NL-COMPILE-001]`）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypedIntentDto {
    pub schema_id: String,
    pub schema_version: String,
    pub intent_id_hex: String,
    pub intent_kind: CompiledIntentKind,
    pub normalized_utterance: String,
    pub workspace_id_hex: String,
    pub principal_id_hex: String,
    pub session_generation: u64,
    pub control_scope: String,
    pub idempotency_key_hex: String,
    pub resolved_application: Option<ResolvedApplicationHandleDto>,
    pub resolved_artifacts: Vec<ResolvedArtifactHandleDto>,
    pub compiled_control_command_preview: String,
}

/// `L1/L2` 零行话提案预览 + `L3` 摘要与澄清绑定（`[NL-PROPOSE-001]`, `[NL-AMBIG-001]`）。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionProposalDto {
    pub proposal_schema_id: String,
    pub proposal_digest_hex: String,
    pub target_application_label: String,
    pub affected_scope_label: String,
    pub resource_ceiling_label: String,
    pub undo_class: ProposalUndoClass,
    pub undo_badge_label: String,
    pub compensation_note: String,
    pub dependency_summary_label: String,
    pub effective_taint_flags: Vec<TaintFlag>,
    pub plain_taint_summary: Option<String>,
    pub actual_usage_units: u64,
    pub effective_cap_units: u64,
    pub requested_ceiling_units: u64,
    pub requires_z4_consent: bool,
    pub unresolved_ambiguities: Vec<UnresolvedAmbiguityDto>,
    pub candidates: Vec<CandidateInterpretationDto>,
    pub selected_candidate_id: Option<String>,
    pub clarification_digest_hex: Option<String>,
    pub can_submit: bool,
}

/// 编译输出绑定包。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompiledIntentBundleDto {
    pub typed_intent: TypedIntentDto,
    pub proposal: ActionProposalDto,
}

/// 通过澄清门后的正式提案提交回执。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmittedProposalDto {
    pub typed_intent: TypedIntentDto,
    pub proposal: ActionProposalDto,
    pub selected_candidate_id: Option<String>,
    pub clarification_digest_hex: Option<String>,
    pub handoff_target: String,
}

/// 显式依赖钉死与来源标记输入（用于验证 `[NL-BIND-001]` 与 `U-7/U-8`）。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplicitDependencyPin {
    pub raw_identifier_hex: Option<String>,
    pub application_id_hex: Option<String>,
    pub installation_id_hex: Option<String>,
    pub expected_generation: Option<u64>,
    pub version_tag: Option<String>,
    #[serde(default)]
    pub from_non_authoritative_search_hit: bool,
    #[serde(default)]
    pub search_restriction_bits: u32,
    pub artifact_id_hex: Option<String>,
    pub artifact_content_digest_hex: Option<String>,
    pub artifact_generation: Option<u64>,
}

/// 已同步到 Omnibar 编译目录的应用条目。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OmnibarCatalogEntry {
    pub application_id: [u8; 16],
    pub installation_id: [u8; 16],
    pub package_id: Option<[u8; 16]>,
    pub known_process_ids: Vec<[u8; 16]>,
    pub current_generation: u64,
    pub display_name: String,
    pub keywords: Vec<String>,
}

/// Omnibar 完整内存态快照 DTO。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OmnibarSnapshotDto {
    pub open: bool,
    pub z_index: u16,
    pub border_radius_px: u8,
    pub taskbar_trigger_height_px: u32,
    pub top_permanent_footprint_px: u32,
    pub last_open_source: Option<OmnibarOpenSource>,
    pub last_close_reason: Option<OmnibarCloseReason>,
    pub locale: OmnibarLocale,
    pub draft_utterance: String,
    pub attachments: Vec<AttachmentChipDto>,
    pub selected_candidate_id: Option<String>,
    pub compiled_bundle: Option<CompiledIntentBundleDto>,
    pub voice: VoiceStreamSnapshotDto,
    pub taskbar_tray_indicators: (bool, bool, bool),
}

/// Omnibar 类型化领域错误（在 Tauri 命令边界映射为 `DesktopError`）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OmnibarError {
    EmptyUtterance,
    MutableLatestTagForbidden {
        offending_tag: String,
    },
    NonAuthoritativeSearchHitForbidden {
        restriction_bits: u32,
    },
    PackageIdNotApplicationIdentity {
        offending_id: [u8; 16],
    },
    ProcessIdNotApplicationIdentity {
        offending_id: [u8; 16],
    },
    StaleTargetGeneration {
        application_id: [u8; 16],
        requested_generation: u64,
        current_generation: u64,
    },
    UnverifiedDependencyHandle {
        detail: String,
    },
    ClarificationRequired {
        ambiguity_kinds: Vec<UnresolvedAmbiguityKind>,
        candidate_ids: Vec<String>,
    },
    MissingChooserGrant,
    RawHostPathForbidden,
    InvalidAttachmentDigest,
    InvalidHexIdentity {
        field: &'static str,
    },
    InputRejected {
        reason: InputRejectionReason,
        detail: String,
    },
    SessionNotActive {
        state: String,
    },
}

impl fmt::Display for OmnibarError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUtterance => write!(formatter, "EMPTY_UTTERANCE: utterance cannot be empty"),
            Self::MutableLatestTagForbidden { offending_tag } => write!(
                formatter,
                "MUTABLE_LATEST_TAG_FORBIDDEN: symbolic version tag '{offending_tag}' cannot be authorized directly"
            ),
            Self::NonAuthoritativeSearchHitForbidden { restriction_bits } => write!(
                formatter,
                "NON_AUTHORITATIVE_SEARCH_HIT_FORBIDDEN: search hit (restrictions=0x{restriction_bits:02x}) cannot act as direct execution authority"
            ),
            Self::PackageIdNotApplicationIdentity { offending_id } => write!(
                formatter,
                "PACKAGE_ID_NOT_APPLICATION_IDENTITY: {} is a PackageId",
                hex(offending_id)
            ),
            Self::ProcessIdNotApplicationIdentity { offending_id } => write!(
                formatter,
                "PROCESS_ID_NOT_APPLICATION_IDENTITY: {} is a transient ProcessId",
                hex(offending_id)
            ),
            Self::StaleTargetGeneration {
                application_id,
                requested_generation,
                current_generation,
            } => write!(
                formatter,
                "STALE_TARGET_GENERATION: app {} requested gen {} < current gen {}",
                hex(application_id),
                requested_generation,
                current_generation
            ),
            Self::UnverifiedDependencyHandle { detail } => {
                write!(formatter, "UNVERIFIED_DEPENDENCY_HANDLE: {detail}")
            }
            Self::ClarificationRequired {
                ambiguity_kinds,
                candidate_ids,
            } => write!(
                formatter,
                "CLARIFICATION_REQUIRED: {:?} requires explicit selection among {:?}",
                ambiguity_kinds, candidate_ids
            ),
            Self::MissingChooserGrant => write!(
                formatter,
                "MISSING_CHOOSER_GRANT: local file requires chooserGrantId and scopedCapabilityId"
            ),
            Self::RawHostPathForbidden => write!(
                formatter,
                "RAW_HOST_PATH_FORBIDDEN: raw host filesystem path cannot be attached without a Trusted UI ChooserGrant"
            ),
            Self::InvalidAttachmentDigest => write!(
                formatter,
                "INVALID_CONTENT_DIGEST: attachment requires a non-zero 64-character hex digest"
            ),
            Self::InvalidHexIdentity { field } => write!(
                formatter,
                "INVALID_HEX_IDENTITY: {field} must be a non-zero 32-character hex identifier"
            ),
            Self::InputRejected { reason, detail } => {
                write!(formatter, "{}: {detail}", reason.as_code_str())
            }
            Self::SessionNotActive { state } => {
                write!(formatter, "SESSION_NOT_ACTIVE: desktop session is {state}")
            }
        }
    }
}

impl std::error::Error for OmnibarError {}

impl From<OmnibarError> for DesktopError {
    fn from(value: OmnibarError) -> Self {
        match value {
            OmnibarError::EmptyUtterance
            | OmnibarError::MissingChooserGrant
            | OmnibarError::RawHostPathForbidden
            | OmnibarError::InvalidAttachmentDigest
            | OmnibarError::InvalidHexIdentity { .. } => Self::config(value.to_string()),
            OmnibarError::ClarificationRequired { .. }
            | OmnibarError::MutableLatestTagForbidden { .. }
            | OmnibarError::NonAuthoritativeSearchHitForbidden { .. }
            | OmnibarError::PackageIdNotApplicationIdentity { .. }
            | OmnibarError::ProcessIdNotApplicationIdentity { .. }
            | OmnibarError::StaleTargetGeneration { .. }
            | OmnibarError::UnverifiedDependencyHandle { .. }
            | OmnibarError::InputRejected { .. }
            | OmnibarError::SessionNotActive { .. } => {
                Self::new(ErrorCode::Control, value.to_string())
            }
        }
    }
}

fn fnv1a_u32(bytes: &[u8], seed: u32) -> u32 {
    let mut hash = seed;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn derive_deterministic_hex_16(domain: &str, payload: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    for idx in 0..4_u32 {
        let seed = 0x811c_9dc5_u32 ^ (idx + 1).wrapping_mul(0x9e37_79b9);
        let combined = format!("{domain}|{idx}|{payload}");
        let word = fnv1a_u32(combined.as_bytes(), seed);
        out[(idx as usize) * 4..(idx as usize) * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn derive_deterministic_hex_32(domain: &str, payload: &str) -> String {
    let mut buf = String::with_capacity(64);
    for idx in 0..8_u32 {
        let seed = 0x811c_9dc5_u32 ^ (idx + 1).wrapping_mul(0x9e37_79b9);
        let combined = format!("{domain}|{idx}|{payload}");
        let word = fnv1a_u32(combined.as_bytes(), seed);
        buf.push_str(&format!("{word:08x}"));
    }
    buf
}

fn parse_non_zero_hex_16(raw: &str, field: &'static str) -> Result<[u8; 16], OmnibarError> {
    let bytes = parse_hex_id(raw.trim()).map_err(|_| OmnibarError::InvalidHexIdentity { field })?;
    if bytes == [0u8; 16] {
        return Err(OmnibarError::InvalidHexIdentity { field });
    }
    Ok(bytes)
}

fn normalize_non_zero_digest_64(raw: &str) -> Result<String, OmnibarError> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(OmnibarError::InvalidAttachmentDigest);
    }
    if trimmed.bytes().all(|b| b == b'0') {
        return Err(OmnibarError::InvalidAttachmentDigest);
    }
    Ok(trimmed)
}

fn looks_like_raw_host_path(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    let bytes = trimmed.as_bytes();
    let windows_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let unc_path = trimmed.starts_with("\\\\");
    let file_uri = trimmed.to_ascii_lowercase().starts_with("file://");
    let posix_host = [
        "/Users/",
        "/home/",
        "/etc/",
        "/var/",
        "/opt/",
        "/private/",
        "/root/",
        "/tmp/",
        "/mnt/",
        "/Volumes/",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix));

    windows_drive || unc_path || file_uri || posix_host
}

fn extract_safe_basename(raw: &str) -> String {
    let mut cleaned = raw.trim();
    if let Some(rest) = cleaned.strip_prefix("file:///") {
        cleaned = rest;
    } else if let Some(rest) = cleaned.strip_prefix("file://") {
        cleaned = rest;
    }
    let normalized = cleaned.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .unwrap_or(trimmed)
        .trim()
        .to_owned()
}

/// 校验并构造 `4px` 圆角多模态附件芯片（`[DUI-CHOOSER-001]`, `[TUI-EXT-001]`）。
///
/// # Errors
///
/// 缺失 `ChooserGrant`、尝试直接挂载原始宿主路径或哈希非法时返回 [`OmnibarError`]。
pub fn validate_attachment_chip(
    input: AttachmentChipInput,
) -> Result<AttachmentChipDto, OmnibarError> {
    let digest = normalize_non_zero_digest_64(&input.content_digest_hex)?;
    let mut taint_set: BTreeSet<TaintFlag> = input.taint_flags.into_iter().collect();

    match input.kind {
        AttachmentKind::Screenshot => {
            let safe_label = {
                let base = extract_safe_basename(&input.label_or_path);
                if base.is_empty() {
                    "screen-capture.png".to_owned()
                } else {
                    base
                }
            };
            Ok(AttachmentChipDto {
                attachment_id: input
                    .attachment_id
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| format!("att-shot-{}", &digest[..12])),
                kind: AttachmentKind::Screenshot,
                display_label: safe_label,
                mime_type: input
                    .mime_type
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "image/png".to_owned()),
                byte_length: input.byte_length,
                chip_border_radius_px: OMNIBAR_CONTROL_RADIUS_PX,
                content_digest_hex: digest,
                chooser_grant_id_hex: None,
                scoped_capability_id_hex: None,
                artifact_id_hex: None,
                taint_flags: taint_set.into_iter().collect(),
                explicit_host_path_authorized: false,
                authorized_host_path: None,
            })
        }
        AttachmentKind::ChooserFile => {
            let raw_grant = input
                .chooser_grant_id_hex
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            let raw_cap = input
                .scoped_capability_id_hex
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            if raw_grant.is_empty() || raw_cap.is_empty() {
                if looks_like_raw_host_path(&input.label_or_path) {
                    return Err(OmnibarError::RawHostPathForbidden);
                }
                return Err(OmnibarError::MissingChooserGrant);
            }
            let grant_bytes = parse_non_zero_hex_16(raw_grant, "chooser_grant_id_hex")?;
            let cap_bytes = parse_non_zero_hex_16(raw_cap, "scoped_capability_id_hex")?;
            let grant_hex = hex(&grant_bytes);
            let cap_hex = hex(&cap_bytes);
            let safe_name = extract_safe_basename(&input.label_or_path);
            if safe_name.is_empty() {
                return Err(OmnibarError::InvalidHexIdentity {
                    field: "label_or_path",
                });
            }
            let authorized_path = if input.explicit_host_path_authorized {
                Some(input.label_or_path.trim().to_owned())
            } else {
                None
            };
            Ok(AttachmentChipDto {
                attachment_id: input
                    .attachment_id
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| format!("att-file-{}", &grant_hex[..12])),
                kind: AttachmentKind::ChooserFile,
                display_label: safe_name,
                mime_type: input
                    .mime_type
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                byte_length: input.byte_length,
                chip_border_radius_px: OMNIBAR_CONTROL_RADIUS_PX,
                content_digest_hex: digest,
                chooser_grant_id_hex: Some(grant_hex),
                scoped_capability_id_hex: Some(cap_hex),
                artifact_id_hex: None,
                taint_flags: taint_set.into_iter().collect(),
                explicit_host_path_authorized: input.explicit_host_path_authorized,
                authorized_host_path: authorized_path,
            })
        }
        AttachmentKind::Artifact => {
            let raw_art = input
                .artifact_id_hex
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            let art_bytes = parse_non_zero_hex_16(raw_art, "artifact_id_hex")?;
            let art_hex = hex(&art_bytes);
            let safe_name = {
                let base = extract_safe_basename(&input.label_or_path);
                if base.is_empty() {
                    format!("output-{}", &art_hex[..8])
                } else {
                    base
                }
            };
            taint_set.insert(TaintFlag::CrossApplication);
            Ok(AttachmentChipDto {
                attachment_id: input
                    .attachment_id
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| format!("att-obj-{}", &art_hex[..12])),
                kind: AttachmentKind::Artifact,
                display_label: safe_name,
                mime_type: input
                    .mime_type
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                byte_length: input.byte_length,
                chip_border_radius_px: OMNIBAR_CONTROL_RADIUS_PX,
                content_digest_hex: digest,
                chooser_grant_id_hex: None,
                scoped_capability_id_hex: None,
                artifact_id_hex: Some(art_hex),
                taint_flags: taint_set.into_iter().collect(),
                explicit_host_path_authorized: false,
                authorized_host_path: None,
            })
        }
    }
}

fn default_palette_rect(viewport_w: u32, viewport_h: u32) -> RectPx {
    let width = 640_u32.min(viewport_w.saturating_sub(32)).max(240);
    let height = 320_u32.min(viewport_h.saturating_sub(72)).max(120);
    let x = i64::from(viewport_w.saturating_sub(width) / 2) as i32;
    let y = i64::from((viewport_h / 10).max(24)) as i32;
    RectPx::new(x, y, width, height)
}

fn contains_any_case_insensitive(haystack: &str, needles: &[&str]) -> bool {
    let lower = haystack.to_lowercase();
    needles
        .iter()
        .any(|needle| lower.contains(&needle.to_lowercase()))
}

fn has_mutable_latest_reference(
    utterance: &str,
    pin: Option<&ExplicitDependencyPin>,
) -> Option<String> {
    if let Some(tag) = pin.and_then(|p| p.version_tag.as_deref()) {
        let trimmed = tag.trim();
        if matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "latest" | "*" | "head"
        ) {
            return Some(trimmed.to_owned());
        }
    }
    let lower = utterance.to_ascii_lowercase();
    for marker in [
        "@latest",
        ":latest",
        "use latest",
        "使用latest",
        "使用 latest",
    ] {
        if lower.contains(marker) {
            return Some("latest".to_owned());
        }
    }
    None
}

/// Stage D `OmnibarEngine`：维护按需 `Z3` 命令面板草稿、渐进式意图预览、
/// `[NL-AMBIG-001]` 候选消解门与 `InputSource::Voice` / `DevicePermissionState` 状态机。
#[derive(Clone, Debug)]
pub struct OmnibarEngine {
    open: bool,
    last_open_source: Option<OmnibarOpenSource>,
    last_close_reason: Option<OmnibarCloseReason>,
    locale: OmnibarLocale,
    draft_utterance: String,
    attachments: Vec<AttachmentChipDto>,
    explicit_pin: Option<ExplicitDependencyPin>,
    selected_candidate_id: Option<String>,
    compiled_bundle: Option<CompiledIntentBundleDto>,
    catalog: BTreeMap<[u8; 16], OmnibarCatalogEntry>,
    known_package_ids: BTreeSet<[u8; 16]>,
    known_process_ids: BTreeSet<[u8; 16]>,
    effective_budget_cap_units: u64,
    actual_budget_usage_units: u64,
    requested_budget_override_units: Option<u64>,
    voice_phase: VoiceStreamPhase,
    streaming_transcript: String,
    committed_transcript: String,
    device_permissions: DevicePermissionStateDto,
    last_severance_reason: Option<VoiceSeveranceReason>,
    severed_count: u64,
    rejected_voice_attempts: u64,
    bound_session_generation: u64,
}

impl Default for OmnibarEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl OmnibarEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: false,
            last_open_source: None,
            last_close_reason: None,
            locale: OmnibarLocale::ZhCn,
            draft_utterance: String::new(),
            attachments: Vec::new(),
            explicit_pin: None,
            selected_candidate_id: None,
            compiled_bundle: None,
            catalog: BTreeMap::new(),
            known_package_ids: BTreeSet::new(),
            known_process_ids: BTreeSet::new(),
            effective_budget_cap_units: 100,
            actual_budget_usage_units: 18,
            requested_budget_override_units: None,
            voice_phase: VoiceStreamPhase::Idle,
            streaming_transcript: String::new(),
            committed_transcript: String::new(),
            device_permissions: DevicePermissionStateDto::default(),
            last_severance_reason: None,
            severed_count: 0,
            rejected_voice_attempts: 0,
            bound_session_generation: 1,
        }
    }

    pub fn set_locale(&mut self, locale: OmnibarLocale) {
        self.locale = locale;
    }

    pub fn set_budget_context(
        &mut self,
        actual_usage_units: u64,
        effective_cap_units: u64,
        requested_override_units: Option<u64>,
    ) {
        self.actual_budget_usage_units = actual_usage_units;
        self.effective_budget_cap_units = effective_cap_units.max(10);
        self.requested_budget_override_units = requested_override_units;
    }

    /// 登记已核验应用目录项，同时记录其 `PackageId` 与瞬态 `ProcessId` 以防身份混用（`[DUI-LAUNCH-001]`）。
    pub fn register_catalog_entry(&mut self, entry: OmnibarCatalogEntry) {
        if let Some(pkg) = entry.package_id {
            self.known_package_ids.insert(pkg);
        }
        for &pid in &entry.known_process_ids {
            self.known_process_ids.insert(pid);
        }
        self.catalog.insert(entry.application_id, entry);
    }

    pub fn register_known_package_id(&mut self, package_id: [u8; 16]) {
        self.known_package_ids.insert(package_id);
    }

    pub fn register_known_process_id(&mut self, process_id: [u8; 16]) {
        self.known_process_ids.insert(process_id);
    }

    const fn is_live_voice_phase(phase: VoiceStreamPhase) -> bool {
        matches!(
            phase,
            VoiceStreamPhase::RequestingMicPermission
                | VoiceStreamPhase::Capturing
                | VoiceStreamPhase::Transcribing
        )
    }

    fn sever_live_hardware(&mut self, reason: VoiceSeveranceReason) {
        let had_live = Self::is_live_voice_phase(self.voice_phase)
            || self.device_permissions.mic_active
            || self.device_permissions.camera_active
            || self.device_permissions.screen_capture_active;
        if !had_live {
            return;
        }
        self.device_permissions.mic_active = false;
        if matches!(
            reason,
            VoiceSeveranceReason::SessionNotActive | VoiceSeveranceReason::Z4ModalActive
        ) {
            self.device_permissions.camera_active = false;
            self.device_permissions.screen_capture_active = false;
        }
        self.streaming_transcript.clear();
        if Self::is_live_voice_phase(self.voice_phase) {
            self.voice_phase = VoiceStreamPhase::Cancelled;
        }
        self.last_severance_reason = Some(reason);
        self.severed_count = self.severed_count.saturating_add(1);
    }

    /// 与宿主 `WorkstationHostShell` 同步安全门状态：
    /// 若会话非 `ACTIVE`、会话代际变化或 `Z4` 模态锁激活，立即切断语音采集并收起 `Z3` 面板（保留草稿）。
    pub fn sync_with_host_shell(&mut self, host_shell: &mut WorkstationHostShell) {
        let current_gen = host_shell.session.session_generation();
        let session_state = host_shell.session.state();
        let z4_locked = host_shell.route.z4_modal_lock;

        if z4_locked {
            self.sever_live_hardware(VoiceSeveranceReason::Z4ModalActive);
        } else if session_state != DesktopSessionState::Active
            || current_gen != self.bound_session_generation
        {
            self.sever_live_hardware(VoiceSeveranceReason::SessionNotActive);
        }
        self.bound_session_generation = current_gen;

        if (z4_locked || session_state != DesktopSessionState::Active) && self.open {
            self.open = false;
            self.last_close_reason = Some(OmnibarCloseReason::SecurityGate);
            let _ = host_shell.set_z3_overlay(OMNIBAR_OVERLAY_ID, "palette", false, None);
        }
    }

    /// 经由 `FocusAndInputRoute` 以 `Ctrl+Space`（或 `Cmd+Space`）按需唤起/收起 `Z3` 命令面板。
    ///
    /// # Errors
    ///
    /// 来自不可信表面伪造、会话锁定或 `Z4` 模态锁期间返回 [`OmnibarError`]。
    pub fn toggle_via_shortcut(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        origin: InputOrigin,
        now_mono_ms: u64,
    ) -> Result<bool, OmnibarError> {
        self.sync_with_host_shell(host_shell);
        let envelope = InputEventEnvelope {
            source: InputSource::Keyboard,
            origin,
            target: InputTarget::Z3SystemShortcut {
                shortcut: "Ctrl+Space".to_owned(),
            },
            event_session_generation: host_shell.session.session_generation(),
            now_mono_ms,
        };
        host_shell
            .dispatch_input(&envelope)
            .map_err(|rej| OmnibarError::InputRejected {
                reason: rej.reason,
                detail: rej.message,
            })?;

        if self.open {
            self.close_palette(host_shell, OmnibarCloseReason::Programmatic);
            Ok(false)
        } else {
            self.open_palette_internal(host_shell, OmnibarOpenSource::KeyboardShortcut)?;
            Ok(true)
        }
    }

    /// 通过底部 `36px` 任务栏中的 `26px` 紧凑触发槽唤起面板。
    ///
    /// # Errors
    ///
    /// 来自不可信表面、会话非 `ACTIVE` 或 `Z4` 模态锁期间返回 [`OmnibarError`]。
    pub fn open_via_taskbar_slot(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        origin: InputOrigin,
        now_mono_ms: u64,
    ) -> Result<bool, OmnibarError> {
        self.sync_with_host_shell(host_shell);
        let envelope = InputEventEnvelope {
            source: InputSource::Pointer,
            origin,
            target: InputTarget::Z3SystemShortcut {
                shortcut: "taskbar-omnibar-slot".to_owned(),
            },
            event_session_generation: host_shell.session.session_generation(),
            now_mono_ms,
        };
        host_shell
            .dispatch_input(&envelope)
            .map_err(|rej| OmnibarError::InputRejected {
                reason: rej.reason,
                detail: rej.message,
            })?;

        self.open_palette_internal(host_shell, OmnibarOpenSource::TaskbarTrigger)?;
        Ok(true)
    }

    fn open_palette_internal(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        source: OmnibarOpenSource,
    ) -> Result<(), OmnibarError> {
        if host_shell.session.state() != DesktopSessionState::Active {
            return Err(OmnibarError::SessionNotActive {
                state: host_shell.session.state().as_str().to_owned(),
            });
        }
        let vp = host_shell.compositor.viewport_rect();
        let rect = default_palette_rect(vp.width, vp.height);
        host_shell
            .set_z3_overlay(OMNIBAR_OVERLAY_ID, "palette", true, Some(rect))
            .map_err(|err| OmnibarError::UnverifiedDependencyHandle {
                detail: err.to_string(),
            })?;
        self.open = true;
        self.last_open_source = Some(source);
        Ok(())
    }

    /// 收起面板（`Escape` 或点击外部），严格保留草稿与附件不丢失。
    pub fn close_palette(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        reason: OmnibarCloseReason,
    ) {
        self.open = false;
        self.last_close_reason = Some(reason);
        let _ = host_shell.set_z3_overlay(OMNIBAR_OVERLAY_ID, "palette", false, None);
    }

    /// 显式清空草稿、附件与候选选择。
    pub fn clear_draft(&mut self) {
        self.draft_utterance.clear();
        self.attachments.clear();
        self.explicit_pin = None;
        self.requested_budget_override_units = None;
        self.selected_candidate_id = None;
        self.compiled_bundle = None;
    }

    pub fn update_draft(
        &mut self,
        utterance: impl Into<String>,
        explicit_pin: Option<ExplicitDependencyPin>,
    ) {
        self.draft_utterance = utterance.into();
        self.explicit_pin = explicit_pin;
        self.selected_candidate_id = None;
        self.compiled_bundle = None;
    }

    /// 挂载经过安全核验的 `4px` 多模态附件芯片。
    ///
    /// # Errors
    ///
    /// 附件违反 `ChooserGrant` 或哈希契约时返回 [`OmnibarError`]。
    pub fn attach_item(
        &mut self,
        input: AttachmentChipInput,
    ) -> Result<AttachmentChipDto, OmnibarError> {
        let chip = validate_attachment_chip(input)?;
        if let Some(pos) = self
            .attachments
            .iter()
            .position(|item| item.attachment_id == chip.attachment_id)
        {
            self.attachments[pos] = chip.clone();
        } else {
            self.attachments.push(chip.clone());
        }
        self.selected_candidate_id = None;
        self.compiled_bundle = None;
        Ok(chip)
    }

    pub fn remove_attachment(&mut self, attachment_id: &str) -> bool {
        let before = self.attachments.len();
        self.attachments
            .retain(|item| item.attachment_id != attachment_id);
        let removed = self.attachments.len() != before;
        if removed {
            self.selected_candidate_id = None;
            self.compiled_bundle = None;
        }
        removed
    }

    /// 更新麦克风/摄像头/屏幕采集硬件授权态，并在权限收回时立即切断活跃采集。
    pub fn set_device_permissions(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        mic_permission: Option<HardwarePermissionGrant>,
        camera_permission: Option<HardwarePermissionGrant>,
        screen_capture_permission: Option<HardwarePermissionGrant>,
        camera_active: Option<bool>,
        screen_capture_active: Option<bool>,
    ) -> DevicePermissionStateDto {
        self.sync_with_host_shell(host_shell);
        if let Some(mic) = mic_permission {
            let was_live =
                Self::is_live_voice_phase(self.voice_phase) || self.device_permissions.mic_active;
            self.device_permissions.mic_permission = mic;
            if mic == HardwarePermissionGrant::Denied {
                self.device_permissions.mic_active = false;
                self.streaming_transcript.clear();
                self.voice_phase = VoiceStreamPhase::Denied;
                if was_live {
                    self.last_severance_reason = Some(VoiceSeveranceReason::MicPermissionRevoked);
                    self.severed_count = self.severed_count.saturating_add(1);
                }
            } else if mic == HardwarePermissionGrant::Granted
                && self.voice_phase == VoiceStreamPhase::RequestingMicPermission
                && host_shell.session.state() == DesktopSessionState::Active
                && !host_shell.route.z4_modal_lock
            {
                self.voice_phase = VoiceStreamPhase::Capturing;
                self.device_permissions.mic_active = true;
            }
        }
        if let Some(cam) = camera_permission {
            self.device_permissions.camera_permission = cam;
        }
        if let Some(scr) = screen_capture_permission {
            self.device_permissions.screen_capture_permission = scr;
        }
        let gate_ok = host_shell.session.state() == DesktopSessionState::Active
            && !host_shell.route.z4_modal_lock;
        if let Some(cam_on) = camera_active {
            self.device_permissions.camera_active = cam_on
                && gate_ok
                && self.device_permissions.camera_permission == HardwarePermissionGrant::Granted;
        }
        if let Some(scr_on) = screen_capture_active {
            self.device_permissions.screen_capture_active = scr_on
                && gate_ok
                && self.device_permissions.screen_capture_permission
                    == HardwarePermissionGrant::Granted;
        }
        self.device_permissions
    }

    fn verify_voice_route(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        origin: InputOrigin,
        now_mono_ms: u64,
    ) -> Result<(), OmnibarError> {
        self.sync_with_host_shell(host_shell);
        let envelope = InputEventEnvelope {
            source: InputSource::Voice,
            origin,
            target: InputTarget::Z3SystemShortcut {
                shortcut: "voice-stream".to_owned(),
            },
            event_session_generation: host_shell.session.session_generation(),
            now_mono_ms,
        };
        if let Err(rej) = host_shell.dispatch_input(&envelope) {
            self.rejected_voice_attempts = self.rejected_voice_attempts.saturating_add(1);
            let sev = match rej.reason {
                InputRejectionReason::UntrustedSystemSpoof => {
                    VoiceSeveranceReason::UntrustedOriginSpoof
                }
                InputRejectionReason::Z4ModalSevered
                | InputRejectionReason::Z4ArmingWindowActive => VoiceSeveranceReason::Z4ModalActive,
                InputRejectionReason::LockedOrStaleSession
                | InputRejectionReason::StaleOrUnpresentedSurface => {
                    VoiceSeveranceReason::SessionNotActive
                }
            };
            if sev != VoiceSeveranceReason::UntrustedOriginSpoof {
                self.sever_live_hardware(sev);
            } else {
                self.last_severance_reason = Some(sev);
            }
            return Err(OmnibarError::InputRejected {
                reason: rej.reason,
                detail: rej.message,
            });
        }
        Ok(())
    }

    /// 启动按住说话/切换语音流（严格经 `FocusAndInputRoute` 裁决 `InputSource::Voice`）。
    ///
    /// # Errors
    ///
    /// 当会话锁定、`Z4` 模态激活、来源伪造或麦克风权限被拒时返回 [`OmnibarError`]。
    pub fn start_voice_capture(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        origin: InputOrigin,
        now_mono_ms: u64,
    ) -> Result<VoiceStreamSnapshotDto, OmnibarError> {
        self.verify_voice_route(host_shell, origin, now_mono_ms)?;
        self.streaming_transcript.clear();
        self.last_severance_reason = None;

        match self.device_permissions.mic_permission {
            HardwarePermissionGrant::Denied => {
                self.voice_phase = VoiceStreamPhase::Denied;
                self.device_permissions.mic_active = false;
                self.rejected_voice_attempts = self.rejected_voice_attempts.saturating_add(1);
                Err(OmnibarError::UnverifiedDependencyHandle {
                    detail: "microphone permission denied".to_owned(),
                })
            }
            HardwarePermissionGrant::PromptRequired => {
                self.voice_phase = VoiceStreamPhase::RequestingMicPermission;
                self.device_permissions.mic_active = false;
                Ok(self.voice_snapshot())
            }
            HardwarePermissionGrant::Granted => {
                self.voice_phase = VoiceStreamPhase::Capturing;
                self.device_permissions.mic_active = true;
                Ok(self.voice_snapshot())
            }
        }
    }

    /// 追加流式语音转写分片。
    ///
    /// # Errors
    ///
    /// 会话锁定、`Z4` 激活或当前不在采集中时返回 [`OmnibarError`]。
    pub fn append_voice_transcript(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        origin: InputOrigin,
        chunk: &str,
        now_mono_ms: u64,
    ) -> Result<VoiceStreamSnapshotDto, OmnibarError> {
        self.verify_voice_route(host_shell, origin, now_mono_ms)?;
        if !matches!(
            self.voice_phase,
            VoiceStreamPhase::Capturing | VoiceStreamPhase::Transcribing
        ) {
            return Err(OmnibarError::UnverifiedDependencyHandle {
                detail: "voice stream is not in Capturing/Transcribing state".to_owned(),
            });
        }
        let cleaned = chunk.trim();
        if !cleaned.is_empty() {
            if !self.streaming_transcript.is_empty() {
                self.streaming_transcript.push(' ');
            }
            self.streaming_transcript.push_str(cleaned);
        }
        Ok(self.voice_snapshot())
    }

    /// 结束麦克风硬件占用并进入 `Transcribing` 阶段。
    ///
    /// # Errors
    ///
    /// 会话锁定或 `Z4` 激活时返回 [`OmnibarError`]。
    pub fn finish_voice_to_transcribing(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        now_mono_ms: u64,
    ) -> Result<VoiceStreamSnapshotDto, OmnibarError> {
        self.verify_voice_route(host_shell, InputOrigin::TrustedShell, now_mono_ms)?;
        self.device_permissions.mic_active = false;
        self.voice_phase = VoiceStreamPhase::Transcribing;
        Ok(self.voice_snapshot())
    }

    /// 提交转写草稿进入 Omnibar 输入框。
    ///
    /// # Errors
    ///
    /// 会话锁定或 `Z4` 激活时返回 [`OmnibarError`]。
    pub fn commit_voice_transcript(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        override_text: Option<&str>,
        now_mono_ms: u64,
    ) -> Result<VoiceStreamSnapshotDto, OmnibarError> {
        self.verify_voice_route(host_shell, InputOrigin::TrustedShell, now_mono_ms)?;
        self.device_permissions.mic_active = false;
        let final_text = override_text
            .unwrap_or(&self.streaming_transcript)
            .trim()
            .to_owned();
        self.streaming_transcript.clear();
        if final_text.is_empty() {
            self.voice_phase = VoiceStreamPhase::Cancelled;
            return Ok(self.voice_snapshot());
        }
        if !self.draft_utterance.trim().is_empty() {
            self.draft_utterance.push(' ');
        }
        self.draft_utterance.push_str(&final_text);
        self.committed_transcript = final_text;
        self.voice_phase = VoiceStreamPhase::Committed;
        self.selected_candidate_id = None;
        self.compiled_bundle = None;
        Ok(self.voice_snapshot())
    }

    pub fn cancel_voice_capture(&mut self) -> VoiceStreamSnapshotDto {
        self.device_permissions.mic_active = false;
        self.streaming_transcript.clear();
        self.last_severance_reason = None;
        self.voice_phase = VoiceStreamPhase::Cancelled;
        self.voice_snapshot()
    }

    #[must_use]
    pub fn voice_snapshot(&self) -> VoiceStreamSnapshotDto {
        VoiceStreamSnapshotDto {
            phase: self.voice_phase,
            streaming_transcript: self.streaming_transcript.clone(),
            committed_transcript: self.committed_transcript.clone(),
            device_permissions: self.device_permissions,
            last_severance_reason: self.last_severance_reason,
            severed_count: self.severed_count,
            rejected_attempt_count: self.rejected_voice_attempts,
        }
    }

    fn resolve_explicit_dependency(
        &self,
        utterance: &str,
        authority: Option<&ApplicationAuthority>,
    ) -> Result<Option<ResolvedApplicationHandleDto>, OmnibarError> {
        if let Some(offending_tag) =
            has_mutable_latest_reference(utterance, self.explicit_pin.as_ref())
        {
            return Err(OmnibarError::MutableLatestTagForbidden { offending_tag });
        }

        let Some(pin) = self.explicit_pin.as_ref() else {
            return Ok(None);
        };

        if pin.from_non_authoritative_search_hit || pin.search_restriction_bits != 0 {
            let bits = if pin.search_restriction_bits != 0 {
                pin.search_restriction_bits
            } else {
                SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
            };
            return Err(OmnibarError::NonAuthoritativeSearchHitForbidden {
                restriction_bits: bits,
            });
        }

        if let Some(raw_id) = pin
            .raw_identifier_hex
            .as_deref()
            .or(pin.application_id_hex.as_deref())
            && let Ok(candidate_bytes) = parse_hex_id(raw_id.trim())
        {
            if self.known_package_ids.contains(&candidate_bytes) {
                return Err(OmnibarError::PackageIdNotApplicationIdentity {
                    offending_id: candidate_bytes,
                });
            }
            if self.known_process_ids.contains(&candidate_bytes) {
                return Err(OmnibarError::ProcessIdNotApplicationIdentity {
                    offending_id: candidate_bytes,
                });
            }
        }

        if pin.application_id_hex.is_none() && pin.installation_id_hex.is_none() {
            return Ok(None);
        }

        let app_bytes = parse_non_zero_hex_16(
            pin.application_id_hex.as_deref().unwrap_or(""),
            "application_id_hex",
        )?;
        let inst_bytes = parse_non_zero_hex_16(
            pin.installation_id_hex.as_deref().unwrap_or(""),
            "installation_id_hex",
        )?;

        if let Some(auth) = authority {
            let resolved = resolve_launch_from_authority(
                auth,
                ApplicationId::from_bytes(app_bytes),
                InstallationId::from_bytes(inst_bytes),
            )
            .map_err(|err| match err {
                AuthorityLaunchResolutionError::PackageIdNotApplicationIdentity {
                    offending_id,
                } => OmnibarError::PackageIdNotApplicationIdentity { offending_id },
                AuthorityLaunchResolutionError::ProcessIdNotApplicationIdentity {
                    offending_id,
                } => OmnibarError::ProcessIdNotApplicationIdentity { offending_id },
                AuthorityLaunchResolutionError::StaleInstallationGeneration {
                    application_id,
                    requested_generation,
                    current_generation,
                    ..
                } => OmnibarError::StaleTargetGeneration {
                    application_id,
                    requested_generation,
                    current_generation,
                },
                other => OmnibarError::UnverifiedDependencyHandle {
                    detail: other.to_string(),
                },
            })?;

            if let Some(expected_gen) = pin.expected_generation
                && expected_gen < resolved.current_installation_generation
            {
                return Err(OmnibarError::StaleTargetGeneration {
                    application_id: app_bytes,
                    requested_generation: expected_gen,
                    current_generation: resolved.current_installation_generation,
                });
            }

            let display_name = resolved
                .presentation
                .presentable_surfaces
                .first()
                .map_or_else(
                    || format!("Application {}", &hex(&app_bytes)[..8]),
                    |s| s.title.clone(),
                );
            return Ok(Some(ResolvedApplicationHandleDto {
                application_id_hex: hex(&app_bytes),
                installation_id_hex: hex(&inst_bytes),
                generation: resolved.current_installation_generation,
                display_name,
            }));
        }

        let entry = self.catalog.get(&app_bytes).ok_or_else(|| {
            OmnibarError::UnverifiedDependencyHandle {
                detail: format!(
                    "application {} is not registered in catalog",
                    hex(&app_bytes)
                ),
            }
        })?;
        if entry.installation_id != inst_bytes {
            return Err(OmnibarError::UnverifiedDependencyHandle {
                detail: format!(
                    "installation {} does not match current installation {} for {}",
                    hex(&inst_bytes),
                    hex(&entry.installation_id),
                    hex(&app_bytes)
                ),
            });
        }
        if let Some(expected_gen) = pin.expected_generation
            && expected_gen < entry.current_generation
        {
            return Err(OmnibarError::StaleTargetGeneration {
                application_id: app_bytes,
                requested_generation: expected_gen,
                current_generation: entry.current_generation,
            });
        }

        Ok(Some(ResolvedApplicationHandleDto {
            application_id_hex: hex(&entry.application_id),
            installation_id_hex: hex(&entry.installation_id),
            generation: entry.current_generation,
            display_name: entry.display_name.clone(),
        }))
    }

    fn collect_verified_artifacts(&self) -> Result<Vec<ResolvedArtifactHandleDto>, OmnibarError> {
        let mut out = Vec::new();
        for chip in &self.attachments {
            if let Some(art_hex) = &chip.artifact_id_hex {
                out.push(ResolvedArtifactHandleDto {
                    artifact_id_hex: art_hex.clone(),
                    content_digest_hex: chip.content_digest_hex.clone(),
                    generation: 1,
                    display_label: chip.display_label.clone(),
                });
            }
        }
        if let Some(pin) = &self.explicit_pin
            && let Some(raw_art) = &pin.artifact_id_hex
        {
            let art_bytes = parse_non_zero_hex_16(raw_art, "artifact_id_hex")?;
            let digest = normalize_non_zero_digest_64(
                pin.artifact_content_digest_hex.as_deref().unwrap_or(""),
            )?;
            out.push(ResolvedArtifactHandleDto {
                artifact_id_hex: hex(&art_bytes),
                content_digest_hex: digest,
                generation: pin.artifact_generation.unwrap_or(1),
                display_label: format!("ref-{}", &hex(&art_bytes)[..8]),
            });
        }
        Ok(out)
    }

    /// 渐进式编译当前草稿与附件为 `TypedIntentDto` + `ActionProposalDto`（`[NL-COMPILE-001]`, `[NL-PROPOSE-001]`）。
    ///
    /// # Errors
    ///
    /// 空草稿或违反 `[NL-BIND-001]` 依赖绑定规则时返回 [`OmnibarError`]。
    #[allow(clippy::too_many_lines)]
    pub fn compile_preview(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        authority: Option<&ApplicationAuthority>,
    ) -> Result<CompiledIntentBundleDto, OmnibarError> {
        self.sync_with_host_shell(host_shell);
        let normalized = self
            .draft_utterance
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if normalized.is_empty() {
            return Err(OmnibarError::EmptyUtterance);
        }

        let explicit_app = self.resolve_explicit_dependency(&normalized, authority)?;
        let resolved_artifacts = self.collect_verified_artifacts()?;

        let workspace = host_shell.session.active_workspace().map_err(|err| {
            OmnibarError::SessionNotActive {
                state: err.to_string(),
            }
        })?;
        let workspace_id_hex = workspace.workspace_id_hex();
        let workspace_name = workspace.name.clone();
        let principal_id_hex = hex(&host_shell.session.active_principal_id());
        let session_gen = host_shell.session.session_generation();

        let matched_catalog: Vec<&OmnibarCatalogEntry> = if explicit_app.is_some() {
            Vec::new()
        } else {
            self.catalog
                .values()
                .filter(|entry| {
                    let mut tokens = vec![entry.display_name.as_str()];
                    for kw in &entry.keywords {
                        tokens.push(kw.as_str());
                    }
                    contains_any_case_insensitive(&normalized, &tokens)
                })
                .collect()
        };

        let is_destructive = contains_any_case_insensitive(
            &normalized,
            &[
                "delete",
                "remove",
                "purge",
                "wipe",
                "overwrite",
                "uninstall",
                "send external",
                "publish",
                "删除",
                "清空",
                "永久移除",
                "覆盖",
                "卸载",
                "外发",
                "公开导出",
            ],
        );

        let mut taint_set: BTreeSet<TaintFlag> = self
            .attachments
            .iter()
            .flat_map(|a| a.taint_flags.iter().copied())
            .collect();

        let is_cross_app = contains_any_case_insensitive(
            &normalized,
            &[
                "forward to",
                "transfer to",
                "export to",
                "share with",
                "send to",
                "sync to",
                "转发到",
                "同步到",
                "发送到",
                "导入到",
                "跨应用",
            ],
        ) || taint_set.contains(&TaintFlag::CrossApplication)
            || matched_catalog.len() > 1;

        if is_cross_app {
            taint_set.insert(TaintFlag::CrossApplication);
        }

        let effective_cap = self.effective_budget_cap_units.max(10);
        let actual_usage = self.actual_budget_usage_units;
        let requested_ceiling = self.requested_budget_override_units.unwrap_or_else(|| {
            if contains_any_case_insensitive(
                &normalized,
                &[
                    "unlimited",
                    "escalate budget",
                    "raise limit",
                    "high compute",
                    "扩容预算",
                    "提升上限",
                    "高算力",
                    "全量批处理",
                ],
            ) {
                effective_cap.saturating_add(50)
            } else {
                effective_cap.min(48).max(actual_usage)
            }
        });
        let is_budget_escalated = requested_ceiling > effective_cap;

        let mut ambiguities = Vec::new();
        if matched_catalog.len() > 1 {
            ambiguities.push(UnresolvedAmbiguityDto {
                kind: UnresolvedAmbiguityKind::MultipleMatchingTargets,
                plain_reason: match self.locale {
                    OmnibarLocale::ZhCn => {
                        "有多个已安装应用匹配该指令，请明确点选目标应用。".to_owned()
                    }
                    OmnibarLocale::EnUs => {
                        "Multiple installed applications match this request; select one target."
                            .to_owned()
                    }
                },
            });
        }
        if is_destructive {
            ambiguities.push(UnresolvedAmbiguityDto {
                kind: UnresolvedAmbiguityKind::HighRiskIrreversibleEffect,
                plain_reason: match self.locale {
                    OmnibarLocale::ZhCn => {
                        "该操作可能造成不可撤回的数据变更，请选择可恢复归档或确认永久执行。"
                            .to_owned()
                    }
                    OmnibarLocale::EnUs => {
                        "This action may permanently modify or delete data; choose reversible archive or confirm permanent execution."
                            .to_owned()
                    }
                },
            });
        }
        if is_cross_app {
            ambiguities.push(UnresolvedAmbiguityDto {
                kind: UnresolvedAmbiguityKind::CrossApplicationTransfer,
                plain_reason: match self.locale {
                    OmnibarLocale::ZhCn => {
                        "数据涉及跨应用流转，请明确在当前应用内处理或跨应用传递。".to_owned()
                    }
                    OmnibarLocale::EnUs => {
                        "Data crosses application boundaries; confirm local processing or cross-app transfer."
                            .to_owned()
                    }
                },
            });
        }
        if is_budget_escalated {
            ambiguities.push(UnresolvedAmbiguityDto {
                kind: UnresolvedAmbiguityKind::BudgetEscalation,
                plain_reason: match self.locale {
                    OmnibarLocale::ZhCn => {
                        "预计资源上限超出当前工作区常规额度，请选择常规限额或申请提额。"
                            .to_owned()
                    }
                    OmnibarLocale::EnUs => {
                        "Requested ceiling exceeds current workspace allowance; select bounded mode or elevated allowance."
                            .to_owned()
                    }
                },
            });
        }

        let fallback_app = explicit_app
            .clone()
            .or_else(|| {
                matched_catalog
                    .first()
                    .map(|entry| ResolvedApplicationHandleDto {
                        application_id_hex: hex(&entry.application_id),
                        installation_id_hex: hex(&entry.installation_id),
                        generation: entry.current_generation,
                        display_name: entry.display_name.clone(),
                    })
            })
            .or_else(|| {
                self.catalog
                    .values()
                    .next()
                    .map(|entry| ResolvedApplicationHandleDto {
                        application_id_hex: hex(&entry.application_id),
                        installation_id_hex: hex(&entry.installation_id),
                        generation: entry.current_generation,
                        display_name: entry.display_name.clone(),
                    })
            })
            .unwrap_or_else(|| ResolvedApplicationHandleDto {
                application_id_hex: "11".repeat(16),
                installation_id_hex: "22".repeat(16),
                generation: 1,
                display_name: match self.locale {
                    OmnibarLocale::ZhCn => "工作区台面工具".to_owned(),
                    OmnibarLocale::EnUs => "Workspace Assistant".to_owned(),
                },
            });

        let candidates = self.build_candidates(
            &normalized,
            &ambiguities,
            &matched_catalog,
            &fallback_app,
            effective_cap,
            requested_ceiling,
            &workspace_name,
            is_destructive,
        );

        let selected_row = self
            .selected_candidate_id
            .as_deref()
            .and_then(|cid| candidates.iter().find(|row| row.candidate_id == cid))
            .cloned();

        let chosen_app = if let Some(ref row) = selected_row {
            ResolvedApplicationHandleDto {
                application_id_hex: row.target_application_id_hex.clone(),
                installation_id_hex: row.target_installation_id_hex.clone(),
                generation: row.target_generation,
                display_name: row.target_app_name.clone(),
            }
        } else {
            fallback_app
        };

        let final_undo = if let Some(ref row) = selected_row {
            row.undo_class
        } else if is_destructive {
            ProposalUndoClass::Irreversible
        } else if is_cross_app {
            ProposalUndoClass::Compensatable
        } else {
            ProposalUndoClass::Lossless
        };

        let final_ceiling = selected_row
            .as_ref()
            .map_or(requested_ceiling, |row| row.budget_ceiling_units);

        let parsed_nl_cmd = parse_nl_command(&normalized).ok();
        let intent_kind = if parsed_nl_cmd.is_some() {
            CompiledIntentKind::ControlOperation
        } else if contains_any_case_insensitive(
            &normalized,
            &["inspect", "check", "show", "查看", "检查", "预览"],
        ) && !is_destructive
            && !is_cross_app
        {
            CompiledIntentKind::InspectReadonly
        } else if contains_any_case_insensitive(&normalized, &["open", "launch", "打开", "启动"])
            && !is_destructive
            && !is_cross_app
        {
            CompiledIntentKind::LaunchApplication
        } else {
            CompiledIntentKind::BackgroundTask
        };

        let seed = format!(
            "{normalized}|{workspace_id_hex}|{principal_id_hex}|{session_gen}|{}|{}|{}",
            chosen_app.application_id_hex, chosen_app.installation_id_hex, chosen_app.generation
        );
        let intent_id_hex = hex(&derive_deterministic_hex_16(
            "llmos/stage-d/omnibar/intent-id/v1",
            &seed,
        ));
        let idempotency_key_hex = hex(&derive_deterministic_hex_16(
            "llmos/stage-d/omnibar/idempotency/v1",
            &format!(
                "{intent_id_hex}|{}",
                selected_row
                    .as_ref()
                    .map_or("default", |r| r.candidate_id.as_str())
            ),
        ));
        let effective_taint_flags: Vec<TaintFlag> = taint_set.into_iter().collect();
        let proposal_digest_hex = derive_deterministic_hex_32(
            "llmos/stage-d/omnibar/proposal-digest/v1",
            &format!("{seed}|{final_undo:?}|{final_ceiling}|{effective_taint_flags:?}"),
        );
        let clarification_digest_hex = selected_row.as_ref().map(|row| {
            derive_deterministic_hex_32(
                "llmos/stage-d/omnibar/clarification-digest/v1",
                &format!(
                    "{proposal_digest_hex}|{}|{}|{session_gen}",
                    row.candidate_id, row.candidate_digest_hex
                ),
            )
        });

        let compiled_control_command_preview = if let Some(cmd) = parsed_nl_cmd {
            format!("parse_nl_command -> {cmd:?}")
        } else {
            match intent_kind {
                CompiledIntentKind::LaunchApplication => format!(
                    "launch_application(app={}, inst={}, gen={}, idem={idempotency_key_hex})",
                    chosen_app.application_id_hex,
                    chosen_app.installation_id_hex,
                    chosen_app.generation
                ),
                CompiledIntentKind::ControlOperation => format!(
                    "submit_scoped_control(ws={workspace_id_hex}, idem={idempotency_key_hex})"
                ),
                CompiledIntentKind::InspectReadonly => format!(
                    "inspect_workspace_projection(ws={workspace_id_hex}, app={})",
                    chosen_app.application_id_hex
                ),
                CompiledIntentKind::BackgroundTask => format!(
                    "propose_task_plan(ws={workspace_id_hex}, app={}, gen={}, idem={idempotency_key_hex})",
                    chosen_app.application_id_hex, chosen_app.generation
                ),
            }
        };

        let affected_scope_label = if let Some(ref row) = selected_row {
            row.affected_scope_summary.clone()
        } else if !self.attachments.is_empty() {
            self.attachments
                .iter()
                .map(|a| a.display_label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            match self.locale {
                OmnibarLocale::ZhCn => format!("{workspace_name} 当前上下文"),
                OmnibarLocale::EnUs => format!("Active context in {workspace_name}"),
            }
        };

        let unit_word = match self.locale {
            OmnibarLocale::ZhCn => "单位",
            OmnibarLocale::EnUs => "units",
        };
        let resource_ceiling_label = match self.locale {
            OmnibarLocale::ZhCn => {
                format!(
                    "上限 {final_ceiling} {unit_word}（当前可用上限 {effective_cap} {unit_word}）"
                )
            }
            OmnibarLocale::EnUs => {
                format!("Up to {final_ceiling} {unit_word} (Allowance {effective_cap} {unit_word})")
            }
        };
        let dependency_summary_label = match self.locale {
            OmnibarLocale::ZhCn => format!(
                "{}（已核验安装修订 #{}）",
                chosen_app.display_name, chosen_app.generation
            ),
            OmnibarLocale::EnUs => format!(
                "{} (Verified install rev #{})",
                chosen_app.display_name, chosen_app.generation
            ),
        };

        let plain_taint_summary = (!effective_taint_flags.is_empty()).then(|| {
            effective_taint_flags
                .iter()
                .map(|f| f.plain_label(self.locale))
                .collect::<Vec<_>>()
                .join(" / ")
        });

        let requires_z4_consent = final_undo == ProposalUndoClass::Irreversible
            || final_ceiling > effective_cap
            || effective_taint_flags.iter().any(|f| {
                matches!(
                    f,
                    TaintFlag::ExternalOrigin
                        | TaintFlag::ConfidentialSecret
                        | TaintFlag::CrossApplication
                )
            });

        let can_submit = ambiguities.is_empty() || selected_row.is_some();

        let control_scope = match intent_kind {
            CompiledIntentKind::LaunchApplication => "APPLICATION",
            CompiledIntentKind::ControlOperation => "OPERATION",
            CompiledIntentKind::InspectReadonly => "WORKSPACE",
            CompiledIntentKind::BackgroundTask => "TASK",
        }
        .to_owned();

        let bundle = CompiledIntentBundleDto {
            typed_intent: TypedIntentDto {
                schema_id: TYPED_INTENT_SCHEMA_ID.to_owned(),
                schema_version: TYPED_INTENT_SCHEMA_VERSION.to_owned(),
                intent_id_hex,
                intent_kind,
                normalized_utterance: normalized,
                workspace_id_hex,
                principal_id_hex,
                session_generation: session_gen,
                control_scope,
                idempotency_key_hex,
                resolved_application: Some(chosen_app.clone()),
                resolved_artifacts,
                compiled_control_command_preview,
            },
            proposal: ActionProposalDto {
                proposal_schema_id: ACTION_PROPOSAL_SCHEMA_ID.to_owned(),
                proposal_digest_hex,
                target_application_label: chosen_app.display_name,
                affected_scope_label,
                resource_ceiling_label,
                undo_class: final_undo,
                undo_badge_label: final_undo.badge_label(self.locale).to_owned(),
                compensation_note: final_undo.compensation_note(self.locale).to_owned(),
                dependency_summary_label,
                effective_taint_flags,
                plain_taint_summary,
                actual_usage_units: actual_usage,
                effective_cap_units: effective_cap,
                requested_ceiling_units: final_ceiling,
                requires_z4_consent,
                unresolved_ambiguities: ambiguities,
                candidates,
                selected_candidate_id: selected_row.map(|r| r.candidate_id),
                clarification_digest_hex,
                can_submit,
            },
        };

        self.compiled_bundle = Some(bundle.clone());
        Ok(bundle)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_candidates(
        &self,
        utterance: &str,
        ambiguities: &[UnresolvedAmbiguityDto],
        matched_catalog: &[&OmnibarCatalogEntry],
        fallback_app: &ResolvedApplicationHandleDto,
        effective_cap: u64,
        requested_ceiling: u64,
        workspace_name: &str,
        is_destructive: bool,
    ) -> Vec<CandidateInterpretationDto> {
        if ambiguities.is_empty() {
            return Vec::new();
        }
        let unit_word = match self.locale {
            OmnibarLocale::ZhCn => "单位",
            OmnibarLocale::EnUs => "units",
        };
        if matched_catalog.len() >= 2 {
            return matched_catalog
                .iter()
                .take(4)
                .enumerate()
                .map(|(idx, entry)| {
                    let cid = format!("cand-target-{}", idx + 1);
                    let undo = if is_destructive {
                        ProposalUndoClass::Irreversible
                    } else {
                        ProposalUndoClass::Lossless
                    };
                    let ceiling = requested_ceiling.min(effective_cap);
                    let app_hex = hex(&entry.application_id);
                    let inst_hex = hex(&entry.installation_id);
                    CandidateInterpretationDto {
                        candidate_id: cid.clone(),
                        target_app_name: entry.display_name.clone(),
                        target_application_id_hex: app_hex.clone(),
                        target_installation_id_hex: inst_hex,
                        target_generation: entry.current_generation,
                        affected_scope_summary: format!(
                            "{workspace_name} · {}",
                            entry.display_name
                        ),
                        side_effect_label: undo.badge_label(self.locale).to_owned(),
                        undo_class: undo,
                        undo_badge_label: undo.badge_label(self.locale).to_owned(),
                        budget_ceiling_units: ceiling,
                        budget_impact_summary: format!("{ceiling} / {effective_cap} {unit_word}"),
                        candidate_digest_hex: derive_deterministic_hex_32(
                            "llmos/stage-d/omnibar/candidate/v1",
                            &format!("{cid}|{app_hex}|{}|{utterance}", entry.current_generation),
                        ),
                    }
                })
                .collect();
        }

        let safe_ceiling = (effective_cap * 3 / 5).max(20).min(effective_cap);
        let direct_undo = if is_destructive {
            ProposalUndoClass::Irreversible
        } else {
            ProposalUndoClass::Compensatable
        };

        let safe_scope = match self.locale {
            OmnibarLocale::ZhCn => format!("{workspace_name} 内可恢复暂存副本"),
            OmnibarLocale::EnUs => format!("Reversible staged copy in {workspace_name}"),
        };
        let direct_scope = match self.locale {
            OmnibarLocale::ZhCn => format!("直接作用于 {workspace_name} 目标内容"),
            OmnibarLocale::EnUs => format!("Direct execution on {workspace_name} targets"),
        };

        vec![
            CandidateInterpretationDto {
                candidate_id: "cand-conservative-safe".to_owned(),
                target_app_name: fallback_app.display_name.clone(),
                target_application_id_hex: fallback_app.application_id_hex.clone(),
                target_installation_id_hex: fallback_app.installation_id_hex.clone(),
                target_generation: fallback_app.generation,
                affected_scope_summary: safe_scope,
                side_effect_label: ProposalUndoClass::Lossless
                    .badge_label(self.locale)
                    .to_owned(),
                undo_class: ProposalUndoClass::Lossless,
                undo_badge_label: ProposalUndoClass::Lossless
                    .badge_label(self.locale)
                    .to_owned(),
                budget_ceiling_units: safe_ceiling,
                budget_impact_summary: format!("{safe_ceiling} / {effective_cap} {unit_word}"),
                candidate_digest_hex: derive_deterministic_hex_32(
                    "llmos/stage-d/omnibar/candidate/v1",
                    &format!(
                        "cand-conservative-safe|{}|LOSSLESS|{safe_ceiling}|{utterance}",
                        fallback_app.application_id_hex
                    ),
                ),
            },
            CandidateInterpretationDto {
                candidate_id: "cand-direct-elevated".to_owned(),
                target_app_name: fallback_app.display_name.clone(),
                target_application_id_hex: fallback_app.application_id_hex.clone(),
                target_installation_id_hex: fallback_app.installation_id_hex.clone(),
                target_generation: fallback_app.generation,
                affected_scope_summary: direct_scope,
                side_effect_label: direct_undo.badge_label(self.locale).to_owned(),
                undo_class: direct_undo,
                undo_badge_label: direct_undo.badge_label(self.locale).to_owned(),
                budget_ceiling_units: requested_ceiling,
                budget_impact_summary: format!("{requested_ceiling} / {effective_cap} {unit_word}"),
                candidate_digest_hex: derive_deterministic_hex_32(
                    "llmos/stage-d/omnibar/candidate/v1",
                    &format!(
                        "cand-direct-elevated|{}|{direct_undo:?}|{requested_ceiling}|{utterance}",
                        fallback_app.application_id_hex
                    ),
                ),
            },
        ]
    }

    /// 显式选定某一行候选解释，生成确定性 `clarification_digest_hex` 解锁硬门（`[NL-AMBIG-001]`）。
    ///
    /// # Errors
    ///
    /// 未知 `candidate_id` 或编译校验失败时返回 [`OmnibarError`]。
    pub fn select_candidate(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        authority: Option<&ApplicationAuthority>,
        candidate_id: &str,
    ) -> Result<CompiledIntentBundleDto, OmnibarError> {
        self.selected_candidate_id = Some(candidate_id.trim().to_owned());
        let bundle = self.compile_preview(host_shell, authority)?;
        if !bundle.proposal.unresolved_ambiguities.is_empty()
            && bundle.proposal.selected_candidate_id.as_deref() != Some(candidate_id.trim())
        {
            self.selected_candidate_id = None;
            return Err(OmnibarError::UnverifiedDependencyHandle {
                detail: format!("unknown candidate_id '{candidate_id}'"),
            });
        }
        Ok(bundle)
    }

    /// 提交行动提案：若存在未澄清歧义且未显式选定候选行，物理拦截并返回 `OmnibarError::ClarificationRequired`。
    ///
    /// # Errors
    ///
    /// 未澄清歧义、依赖绑定不合法或会话非 `ACTIVE` 时返回 [`OmnibarError`]。
    pub fn submit_proposal(
        &mut self,
        host_shell: &mut WorkstationHostShell,
        authority: Option<&ApplicationAuthority>,
        arm_z4_if_required: bool,
        now_mono_ms: u64,
    ) -> Result<SubmittedProposalDto, OmnibarError> {
        self.sync_with_host_shell(host_shell);
        if host_shell.session.state() != DesktopSessionState::Active {
            return Err(OmnibarError::SessionNotActive {
                state: host_shell.session.state().as_str().to_owned(),
            });
        }
        let bundle = self.compile_preview(host_shell, authority)?;
        if !bundle.proposal.unresolved_ambiguities.is_empty()
            && (bundle.proposal.selected_candidate_id.is_none()
                || bundle.proposal.clarification_digest_hex.is_none())
        {
            return Err(OmnibarError::ClarificationRequired {
                ambiguity_kinds: bundle
                    .proposal
                    .unresolved_ambiguities
                    .iter()
                    .map(|a| a.kind)
                    .collect(),
                candidate_ids: bundle
                    .proposal
                    .candidates
                    .iter()
                    .map(|c| c.candidate_id.clone())
                    .collect(),
            });
        }

        self.close_palette(host_shell, OmnibarCloseReason::Submitted);
        let handoff_target = if bundle.proposal.requires_z4_consent {
            if arm_z4_if_required {
                host_shell.set_z4_modal(true, now_mono_ms);
                self.sync_with_host_shell(host_shell);
            }
            "Z4_TRUSTED_CONSENT".to_owned()
        } else {
            "DIRECT_BACKGROUND_TASK".to_owned()
        };

        Ok(SubmittedProposalDto {
            selected_candidate_id: bundle.proposal.selected_candidate_id.clone(),
            clarification_digest_hex: bundle.proposal.clarification_digest_hex.clone(),
            typed_intent: bundle.typed_intent,
            proposal: bundle.proposal,
            handoff_target,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> OmnibarSnapshotDto {
        OmnibarSnapshotDto {
            open: self.open,
            z_index: OMNIBAR_Z_INDEX,
            border_radius_px: OMNIBAR_CONTROL_RADIUS_PX,
            taskbar_trigger_height_px: OMNIBAR_TASKBAR_TRIGGER_HEIGHT_PX,
            top_permanent_footprint_px: OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX,
            last_open_source: self.last_open_source,
            last_close_reason: self.last_close_reason,
            locale: self.locale,
            draft_utterance: self.draft_utterance.clone(),
            attachments: self.attachments.clone(),
            selected_candidate_id: self.selected_candidate_id.clone(),
            compiled_bundle: self.compiled_bundle.clone(),
            voice: self.voice_snapshot(),
            taskbar_tray_indicators: self.device_permissions.taskbar_tray_projection(),
        }
    }
}

fn engine_registry() -> &'static Mutex<BTreeMap<usize, OmnibarEngine>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<usize, OmnibarEngine>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// 在 `AppState` 互斥保护下对绑定的 [`OmnibarEngine`] 与 [`WorkstationHostShell`] 执行原子操作。
///
/// # Errors
///
/// 互斥锁中毒或闭包返回错误时返回 [`DesktopError`]。
pub fn with_omnibar_engine_mut<R>(
    state: &AppState,
    op: impl FnOnce(&mut OmnibarEngine, &mut WorkstationHostShell) -> Result<R, DesktopError>,
) -> Result<R, DesktopError> {
    let key = std::ptr::from_ref(state) as usize;
    state.with_host_shell_mut(|host_shell| {
        let mut reg = engine_registry()
            .lock()
            .map_err(|_| DesktopError::internal("Omnibar 引擎注册表锁中毒"))?;
        let engine = reg.entry(key).or_default();
        engine.sync_with_host_shell(host_shell);
        op(engine, host_shell)
    })
}

pub fn omnibar_get_snapshot_core(state: &AppState) -> Result<OmnibarSnapshotDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, _shell| Ok(engine.snapshot()))
}

pub fn omnibar_toggle_palette_core(
    state: &AppState,
    action: &str,
    now_mono_ms: u64,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, shell| {
        match action.trim().to_ascii_lowercase().as_str() {
            "shortcut" | "toggle" | "ctrl+space" => {
                engine.toggle_via_shortcut(shell, InputOrigin::TrustedShell, now_mono_ms)?;
            }
            "taskbar" | "taskbar-slot" => {
                engine.open_via_taskbar_slot(shell, InputOrigin::TrustedShell, now_mono_ms)?;
            }
            "escape" | "dismiss-escape" => {
                engine.close_palette(shell, OmnibarCloseReason::Escape);
            }
            "outside" | "outside-click" => {
                engine.close_palette(shell, OmnibarCloseReason::OutsideClick);
            }
            other => {
                return Err(DesktopError::config(format!(
                    "unsupported omnibar palette action: {other}"
                )));
            }
        }
        Ok(engine.snapshot())
    })
}

pub fn omnibar_update_draft_core(
    state: &AppState,
    utterance: String,
    locale: Option<String>,
    explicit_pin: Option<ExplicitDependencyPin>,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, _shell| {
        if locale.is_some() {
            engine.set_locale(OmnibarLocale::parse_lossy(locale.as_deref()));
        }
        engine.update_draft(utterance, explicit_pin);
        Ok(engine.snapshot())
    })
}

pub fn omnibar_clear_draft_core(state: &AppState) -> Result<OmnibarSnapshotDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, _shell| {
        engine.clear_draft();
        Ok(engine.snapshot())
    })
}

pub fn omnibar_attach_item_core(
    state: &AppState,
    input: AttachmentChipInput,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, _shell| {
        engine.attach_item(input)?;
        Ok(engine.snapshot())
    })
}

pub fn omnibar_remove_attachment_core(
    state: &AppState,
    attachment_id: &str,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, _shell| {
        engine.remove_attachment(attachment_id);
        Ok(engine.snapshot())
    })
}

pub fn omnibar_compile_preview_core(
    state: &AppState,
) -> Result<CompiledIntentBundleDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, shell| {
        engine
            .compile_preview(shell, None)
            .map_err(DesktopError::from)
    })
}

pub fn omnibar_select_candidate_core(
    state: &AppState,
    candidate_id: &str,
) -> Result<CompiledIntentBundleDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, shell| {
        engine
            .select_candidate(shell, None, candidate_id)
            .map_err(DesktopError::from)
    })
}

pub fn omnibar_submit_proposal_core(
    state: &AppState,
    arm_z4_if_required: bool,
    now_mono_ms: u64,
) -> Result<SubmittedProposalDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, shell| {
        engine
            .submit_proposal(shell, None, arm_z4_if_required, now_mono_ms)
            .map_err(DesktopError::from)
    })
}

pub fn omnibar_voice_action_core(
    state: &AppState,
    action: &str,
    transcript_chunk: Option<String>,
    now_mono_ms: u64,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, shell| {
        match action.trim().to_ascii_lowercase().as_str() {
            "start" => {
                engine.start_voice_capture(shell, InputOrigin::TrustedShell, now_mono_ms)?;
            }
            "append" => {
                engine.append_voice_transcript(
                    shell,
                    InputOrigin::TrustedShell,
                    transcript_chunk.as_deref().unwrap_or(""),
                    now_mono_ms,
                )?;
            }
            "transcribe" => {
                engine.finish_voice_to_transcribing(shell, now_mono_ms)?;
            }
            "commit" => {
                engine.commit_voice_transcript(shell, transcript_chunk.as_deref(), now_mono_ms)?;
            }
            "cancel" => {
                engine.cancel_voice_capture();
            }
            other => {
                return Err(DesktopError::config(format!(
                    "unsupported omnibar voice action: {other}"
                )));
            }
        }
        Ok(engine.snapshot())
    })
}

pub fn omnibar_set_device_permission_core(
    state: &AppState,
    mic_permission: Option<HardwarePermissionGrant>,
    camera_permission: Option<HardwarePermissionGrant>,
    screen_capture_permission: Option<HardwarePermissionGrant>,
    camera_active: Option<bool>,
    screen_capture_active: Option<bool>,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    with_omnibar_engine_mut(state, |engine, shell| {
        engine.set_device_permissions(
            shell,
            mic_permission,
            camera_permission,
            screen_capture_permission,
            camera_active,
            screen_capture_active,
        );
        Ok(engine.snapshot())
    })
}

#[tauri::command]
pub fn omnibar_get_snapshot(
    state: tauri::State<'_, AppState>,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    omnibar_get_snapshot_core(&state)
}

#[tauri::command]
pub fn omnibar_toggle_palette(
    state: tauri::State<'_, AppState>,
    action: String,
    now_mono_ms: Option<u64>,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    omnibar_toggle_palette_core(&state, &action, now_mono_ms.unwrap_or(0))
}

#[tauri::command]
pub fn omnibar_update_draft(
    state: tauri::State<'_, AppState>,
    utterance: String,
    locale: Option<String>,
    explicit_pin: Option<ExplicitDependencyPin>,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    omnibar_update_draft_core(&state, utterance, locale, explicit_pin)
}

#[tauri::command]
pub fn omnibar_clear_draft(
    state: tauri::State<'_, AppState>,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    omnibar_clear_draft_core(&state)
}

#[tauri::command]
pub fn omnibar_attach_item(
    state: tauri::State<'_, AppState>,
    input: AttachmentChipInput,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    omnibar_attach_item_core(&state, input)
}

#[tauri::command]
pub fn omnibar_remove_attachment(
    state: tauri::State<'_, AppState>,
    attachment_id: String,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    omnibar_remove_attachment_core(&state, &attachment_id)
}

#[tauri::command]
pub fn omnibar_compile_preview(
    state: tauri::State<'_, AppState>,
) -> Result<CompiledIntentBundleDto, DesktopError> {
    omnibar_compile_preview_core(&state)
}

#[tauri::command]
pub fn omnibar_select_candidate(
    state: tauri::State<'_, AppState>,
    candidate_id: String,
) -> Result<CompiledIntentBundleDto, DesktopError> {
    omnibar_select_candidate_core(&state, &candidate_id)
}

#[tauri::command]
pub fn omnibar_submit_proposal(
    state: tauri::State<'_, AppState>,
    arm_z4_if_required: Option<bool>,
    now_mono_ms: Option<u64>,
) -> Result<SubmittedProposalDto, DesktopError> {
    omnibar_submit_proposal_core(
        &state,
        arm_z4_if_required.unwrap_or(true),
        now_mono_ms.unwrap_or(0),
    )
}

#[tauri::command]
pub fn omnibar_voice_action(
    state: tauri::State<'_, AppState>,
    action: String,
    transcript_chunk: Option<String>,
    now_mono_ms: Option<u64>,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    omnibar_voice_action_core(&state, &action, transcript_chunk, now_mono_ms.unwrap_or(0))
}

#[tauri::command]
pub fn omnibar_set_device_permission(
    state: tauri::State<'_, AppState>,
    mic_permission: Option<HardwarePermissionGrant>,
    camera_permission: Option<HardwarePermissionGrant>,
    screen_capture_permission: Option<HardwarePermissionGrant>,
    camera_active: Option<bool>,
    screen_capture_active: Option<bool>,
) -> Result<OmnibarSnapshotDto, DesktopError> {
    omnibar_set_device_permission_core(
        &state,
        mic_permission,
        camera_permission,
        screen_capture_permission,
        camera_active,
        screen_capture_active,
    )
}
