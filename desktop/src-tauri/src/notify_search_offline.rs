//! Stage D (`D-NOTIFY-SEARCH-OFFLINE` / `W72-D-NOTIFY-SEARCH`) 通知中心、一次性 `ActionToken` 核销、
//! `Ctrl+K` 全局搜索与 `0x1F` 只读非权威硬件锁、以及离线自治与集群态势托盘 (`[DUI-OFFLINE-001]`)。
//!
//! 权威契约对齐：
//! - `06-架构设计总纲-v0.5.md` §16.3.1、§16.3.4 (`IPC-D4` `QueryIndex` / `InspectSearchScope`、
//!   `IPC-D7` `ListCells` / `InspectCellLeases`、`IPC-D8` `ListNotifications` / `InspectNotification`)、
//!   §26.1–§26.4、§27.1–§27.3；
//! - `ADR-0025` 决定 1（`Z3 = 400` 系统浮层与命令面板）、决定 2（`L0..L3` 渐进披露与动作空间裁剪）、
//!   决定 3（Nomad 三水位线实心微型条、零毛玻璃实心材质）、决定 4（三大法定不可篡改锁：
//!   搜索 `0x1F` 只读锁、`QUARANTINED` 租约围栏锁）；
//! - `2026-10-26-stage-d-desktop-visual-and-ux-design.md` §0.1 (`ANTI-AI-01..07`)、§1.1–§1.3、
//!   §2A.1–§2A.4、§3.5（通知中心与离线自治态势）、§3.6（`Ctrl+K` 全局搜索与 `0x1F` 只读边界）、
//!   §4.1–§4.3。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use crate::dto::hex;
use crate::error::{DesktopError, ErrorCode};
use crate::trusted_ui::{parse_hex_id_16, sha256_domain_digest};

/// `Z3` 系统通知与离线态势浮层固定 Z-index。
pub const Z3_NOTIFICATION_FLYOUT_Z_INDEX: u16 = 400;

/// `list_notification_feed` 单页最大条数上限。
pub const MAX_NOTIFICATION_PAGE_LIMIT: u16 = 64;

/// `query_spotlight_candidates` 单次最大返回候选数上限。
pub const MAX_SPOTLIGHT_CANDIDATE_LIMIT: u16 = 32;

/// 法定不可篡改锁 #1：禁止从搜索候选直接授予能力 (`1 << 0`)。
pub const SPOTLIGHT_FORBID_CAPABILITY_GRANT: u8 = 1 << 0;
/// 法定不可篡改锁 #1：禁止从搜索候选直接扣减资源账本 (`1 << 1`)。
pub const SPOTLIGHT_FORBID_LEDGER_DEDUCTION: u8 = 1 << 1;
/// 法定不可篡改锁 #1：禁止从搜索候选直接提交任务状态跃迁 (`1 << 2`)。
pub const SPOTLIGHT_FORBID_TASK_COMMIT: u8 = 1 << 2;
/// 法定不可篡改锁 #1：禁止从搜索候选直接触发应用包更新 (`1 << 3`)。
pub const SPOTLIGHT_FORBID_APP_UPDATE: u8 = 1 << 3;
/// 法定不可篡改锁 #1：禁止从搜索候选直接执行不可逆动作 (`1 << 4`)。
pub const SPOTLIGHT_FORBID_IRREVERSIBLE_ACTION: u8 = 1 << 4;
/// 法定不可篡改锁 #1：`0x1F` 五位全掩码（搜索结果严禁直接驱动任何控制面写操作）。
pub const SPOTLIGHT_STATUTORY_MASK_0X1F: u8 = SPOTLIGHT_FORBID_CAPABILITY_GRANT
    | SPOTLIGHT_FORBID_LEDGER_DEDUCTION
    | SPOTLIGHT_FORBID_TASK_COMMIT
    | SPOTLIGHT_FORBID_APP_UPDATE
    | SPOTLIGHT_FORBID_IRREVERSIBLE_ACTION;

const DOMAIN_ACTION_TOKEN_ID: &[u8] = b"LLMOS_STAGE_D_NOTIFY_ACTION_TOKEN_ID_V1";
const DOMAIN_ACTION_TOKEN_COMMITMENT: &[u8] = b"LLMOS_STAGE_D_NOTIFY_ACTION_TOKEN_COMMIT_V1";
const DOMAIN_ACTION_TOKEN_RECEIPT: &[u8] = b"LLMOS_STAGE_D_NOTIFY_ACTION_TOKEN_RECEIPT_V1";
const DOMAIN_SEARCH_CHECKPOINT: &[u8] = b"LLMOS_STAGE_D_SPOTLIGHT_CHECKPOINT_V1";
const DOMAIN_SEARCH_VERIFY_RECEIPT: &[u8] = b"LLMOS_STAGE_D_SPOTLIGHT_VERIFY_RECEIPT_V1";
const DOMAIN_LOCAL_AUTONOMY_PROOF: &[u8] = b"LLMOS_STAGE_D_OFFLINE_AUTONOMY_PROOF_V1";

/// 通知、搜索与离线自治子系统确定性错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotifySearchOfflineErrorCode {
    /// 一次性 `ActionToken` 已被消费（重放拦截）。
    ActionTokenAlreadyConsumed,
    /// 一次性 `ActionToken` 已过期。
    ActionTokenExpired,
    /// 调用方 `ApplicationId` 与令牌绑定不一致。
    ActionTokenAppMismatch,
    /// 调用方 `PrincipalId` 与令牌绑定不一致。
    ActionTokenPrincipalMismatch,
    /// 调用方 `TaskId` 与令牌绑定不一致。
    ActionTokenTaskMismatch,
    /// 一次性 `ActionToken` 密码学承诺校验失败。
    ActionTokenInvalidProof,
    /// 目标通知或动作令牌不存在。
    ActionTokenNotFound,
    /// 法定锁 #1：搜索结果为 `0x1F` 非权威只读视图，严禁直接发起控制面变更。
    SearchResultNonAuthoritative0x1f,
    /// 搜索候选条目不存在。
    SearchCandidateNotFound,
    /// 权威存储记录已被撤回 (`retracted`)，重核验拒绝解锁写动作。
    AuthorityRecordRetracted,
    /// 搜索快照代数落后于权威存储代数 (`stale generation`)，需要重新拉取权威状态。
    StaleGeneration,
    /// 法定锁 #3：`QUARANTINED` 节点租约处于隔离围栏锁定态，严禁仅凭本地 TTL 解锁。
    QuarantinedLeaseFenceLocked,
    /// 离线或分区状态下请求了强依赖不可达远端 `Provider` / `Cell` 的操作。
    RemoteDependencyOffline,
    /// 参数格式或边界非法。
    InvalidArgument,
    /// 会话未处于激活态。
    SessionNotActive,
}

impl NotifySearchOfflineErrorCode {
    /// 返回稳定机器可读错误码字符串。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActionTokenAlreadyConsumed => "ACTION_TOKEN_ALREADY_CONSUMED",
            Self::ActionTokenExpired => "ACTION_TOKEN_EXPIRED",
            Self::ActionTokenAppMismatch => "ACTION_TOKEN_APP_MISMATCH",
            Self::ActionTokenPrincipalMismatch => "ACTION_TOKEN_PRINCIPAL_MISMATCH",
            Self::ActionTokenTaskMismatch => "ACTION_TOKEN_TASK_MISMATCH",
            Self::ActionTokenInvalidProof => "ACTION_TOKEN_INVALID_PROOF",
            Self::ActionTokenNotFound => "ACTION_TOKEN_NOT_FOUND",
            Self::SearchResultNonAuthoritative0x1f => "SEARCH_RESULT_NON_AUTHORITATIVE_0X1F",
            Self::SearchCandidateNotFound => "SEARCH_CANDIDATE_NOT_FOUND",
            Self::AuthorityRecordRetracted => "AUTHORITY_RECORD_RETRACTED",
            Self::StaleGeneration => "STALE_GENERATION",
            Self::QuarantinedLeaseFenceLocked => "QUARANTINED_LEASE_FENCE_LOCKED",
            Self::RemoteDependencyOffline => "REMOTE_DEPENDENCY_OFFLINE",
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::SessionNotActive => "SESSION_NOT_ACTIVE",
        }
    }

    #[must_use]
    const fn to_desktop_error_code(self) -> ErrorCode {
        match self {
            Self::ActionTokenAlreadyConsumed
            | Self::ActionTokenExpired
            | Self::StaleGeneration
            | Self::ActionTokenAppMismatch
            | Self::ActionTokenPrincipalMismatch
            | Self::ActionTokenTaskMismatch
            | Self::ActionTokenInvalidProof
            | Self::SearchResultNonAuthoritative0x1f
            | Self::AuthorityRecordRetracted
            | Self::QuarantinedLeaseFenceLocked => ErrorCode::Control,
            Self::ActionTokenNotFound | Self::SearchCandidateNotFound => ErrorCode::NotFound,
            Self::RemoteDependencyOffline => ErrorCode::Ipc,
            Self::InvalidArgument => ErrorCode::Config,
            Self::SessionNotActive => ErrorCode::Handshake,
        }
    }
}

impl fmt::Display for NotifySearchOfflineErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 通知、搜索与离线自治结构化错误。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotifySearchOfflineError {
    /// 细粒度错误码。
    pub code: NotifySearchOfflineErrorCode,
    /// 面向日志与诊断的可读说明。
    pub message: String,
}

impl NotifySearchOfflineError {
    /// 构造结构化错误。
    #[must_use]
    pub fn new(code: NotifySearchOfflineErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 转换为通用 [`DesktopError`]（在 message 前缀保留细粒度错误码以便前端精确识别）。
    #[must_use]
    pub fn into_desktop_error(self) -> DesktopError {
        DesktopError::new(
            self.code.to_desktop_error_code(),
            format!("[{}] {}", self.code.as_str(), self.message),
        )
    }
}

impl fmt::Display for NotifySearchOfflineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for NotifySearchOfflineError {}

/// 16 态权威任务状态枚举（与前端 `CANONICAL_TASK_STATES` 一一对应）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CanonicalTaskState {
    Draft,
    Planning,
    AwaitingApproval,
    Scheduled,
    Running,
    Paused,
    WaitingDependency,
    WaitingHuman,
    Succeeded,
    PartialSucceeded,
    Degraded,
    Failed,
    Cancelled,
    Compensating,
    Compensated,
    RolledBack,
}

impl CanonicalTaskState {
    /// 全部 16 个权威任务状态常量数组。
    pub const ALL: [Self; 16] = [
        Self::Draft,
        Self::Planning,
        Self::AwaitingApproval,
        Self::Scheduled,
        Self::Running,
        Self::Paused,
        Self::WaitingDependency,
        Self::WaitingHuman,
        Self::Succeeded,
        Self::PartialSucceeded,
        Self::Degraded,
        Self::Failed,
        Self::Cancelled,
        Self::Compensating,
        Self::Compensated,
        Self::RolledBack,
    ];

    /// 权威大写下划线标识。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "DRAFT",
            Self::Planning => "PLANNING",
            Self::AwaitingApproval => "AWAITING_APPROVAL",
            Self::Scheduled => "SCHEDULED",
            Self::Running => "RUNNING",
            Self::Paused => "PAUSED",
            Self::WaitingDependency => "WAITING_DEPENDENCY",
            Self::WaitingHuman => "WAITING_HUMAN",
            Self::Succeeded => "SUCCEEDED",
            Self::PartialSucceeded => "PARTIAL_SUCCEEDED",
            Self::Degraded => "DEGRADED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
            Self::Compensating => "COMPENSATING",
            Self::Compensated => "COMPENSATED",
            Self::RolledBack => "ROLLED_BACK",
        }
    }

    /// `L0/L1/L2` 零行话中文状态标签。
    #[must_use]
    pub const fn label_zh(self) -> &'static str {
        match self {
            Self::Draft => "草稿就绪",
            Self::Planning => "正在规划步骤",
            Self::AwaitingApproval => "等待您的确认",
            Self::Scheduled => "已排入队列",
            Self::Running => "正在执行",
            Self::Paused => "已暂停",
            Self::WaitingDependency => "等待前置步骤",
            Self::WaitingHuman => "需要补充输入",
            Self::Succeeded => "已完成",
            Self::PartialSucceeded => "部分完成",
            Self::Degraded => "降级运行",
            Self::Failed => "执行受阻",
            Self::Cancelled => "已取消",
            Self::Compensating => "正在回退清理",
            Self::Compensated => "已完成补偿清理",
            Self::RolledBack => "已安全回滚",
        }
    }

    /// `L0/L1/L2` 零行话英文状态标签。
    #[must_use]
    pub const fn label_en(self) -> &'static str {
        match self {
            Self::Draft => "Draft ready",
            Self::Planning => "Planning steps",
            Self::AwaitingApproval => "Awaiting your confirmation",
            Self::Scheduled => "Queued",
            Self::Running => "Running",
            Self::Paused => "Paused",
            Self::WaitingDependency => "Waiting for prerequisite",
            Self::WaitingHuman => "Needs your input",
            Self::Succeeded => "Completed",
            Self::PartialSucceeded => "Partially completed",
            Self::Degraded => "Running in fallback mode",
            Self::Failed => "Blocked by error",
            Self::Cancelled => "Cancelled",
            Self::Compensating => "Cleaning up changes",
            Self::Compensated => "Cleanup completed",
            Self::RolledBack => "Safely rolled back",
        }
    }

    /// 对应的通知默认分类。
    #[must_use]
    pub const fn default_category(self) -> NotificationCategory {
        match self {
            Self::AwaitingApproval | Self::WaitingHuman => NotificationCategory::ApprovalRequired,
            Self::Failed | Self::Degraded | Self::Compensating => {
                NotificationCategory::AnomalyAlert
            }
            Self::Succeeded | Self::PartialSucceeded | Self::Compensated | Self::RolledBack => {
                NotificationCategory::TaskMilestone
            }
            Self::Draft
            | Self::Planning
            | Self::Scheduled
            | Self::Running
            | Self::Paused
            | Self::WaitingDependency
            | Self::Cancelled => NotificationCategory::TaskTransition,
        }
    }

    /// 对应的通知默认严重度。
    #[must_use]
    pub const fn default_severity(self) -> NotificationSeverity {
        match self {
            Self::Failed => NotificationSeverity::Critical,
            Self::AwaitingApproval
            | Self::WaitingHuman
            | Self::Degraded
            | Self::PartialSucceeded
            | Self::Compensating => NotificationSeverity::Warning,
            Self::Draft
            | Self::Planning
            | Self::Scheduled
            | Self::Running
            | Self::Paused
            | Self::WaitingDependency
            | Self::Succeeded
            | Self::Cancelled
            | Self::Compensated
            | Self::RolledBack => NotificationSeverity::Info,
        }
    }
}

/// 通知分类枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotificationCategory {
    /// 任务状态跃迁通知。
    TaskTransition,
    /// 需要用户确认或输入。
    ApprovalRequired,
    /// 任务阶段里程碑完成。
    TaskMilestone,
    /// 异常、降级或回退警告。
    AnomalyAlert,
    /// 系统或离线自治态势变更。
    SystemPosture,
}

/// 通知严重度枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotificationSeverity {
    Info,
    Warning,
    Critical,
}

/// 一次性通知内联动作令牌视图（绑定 `(NotificationId, ApplicationId, TaskId, PrincipalId, expiry_ms)`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotificationActionTokenView {
    /// 令牌 16 字节定长十六进制标识（32 hex chars）。
    pub token_id_hex: String,
    /// 动作唯一标识（例如 `"approve_task_step"`, `"retry_failed_step"`, `"open_task_drawer"`）。
    pub action_id: String,
    /// `L1/L2` 中文按钮文案（无行话）。
    pub label_zh: String,
    /// `L1/L2` 英文按钮文案（无行话）。
    pub label_en: String,
    /// 绑定的通知 ID（32 hex chars）。
    pub bound_notification_id_hex: String,
    /// 绑定的应用 ID（32 hex chars）。
    pub bound_app_id_hex: String,
    /// 绑定的任务 ID（32 hex chars）。
    pub bound_task_id_hex: String,
    /// 绑定的主体 Principal ID（32 hex chars）。
    pub bound_principal_id_hex: String,
    /// 令牌过期单调时钟毫秒数。
    pub expiry_mono_ms: u64,
    /// 是否已被消费（一次性核销后立即变为 `true`）。
    pub consumed: bool,
    /// 核销时间（若已消费）。
    pub consumed_at_mono_ms: Option<u64>,
    /// 核销回执摘要（若已消费）。
    pub redemption_receipt_digest_hex: Option<String>,
    /// `L3` 诊断專用：SHA-256 校验承诺摘要（64 hex chars）。
    pub verifier_commitment_hex: String,
}

/// 通知中心单条通知的 `L3` 工程诊断元数据。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotificationL3Diagnostics {
    /// 订阅主题标识（32 hex chars）。
    pub topic_id_hex: String,
    /// 订阅者通道标识（32 hex chars）。
    pub subscriber_key_hex: String,
    /// 令牌校验承诺（64 hex chars）。
    pub verifier_commitment_hex: String,
    /// 投递序号。
    pub sequence: u64,
    /// 当前裁剪水位线序号。
    pub prune_watermark_sequence: u64,
    /// 已撤回历史事件计数。
    pub retracted_event_count: u32,
    /// 投递尝试次数。
    pub delivery_attempt: u8,
}

/// 通知中心单条通知视图（支持 `L1/L2` 零行话呈现与 `L3` 工程诊断）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotificationFeedItem {
    /// 通知 16 字节十六进制标识（32 hex chars）。
    pub notification_id_hex: String,
    /// 严格递增投递序号（用于游标分页）。
    pub sequence: u64,
    /// 关联任务 ID（32 hex chars）。
    pub task_id_hex: String,
    /// 关联应用 ID（32 hex chars）。
    pub app_id_hex: String,
    /// 关联应用显示名称（`L1/L2` 友好名称，如 `"工作台构建助手"` / `"Workspace Builder"`）。
    pub app_display_name_zh: String,
    pub app_display_name_en: String,
    /// 权威 16 态任务状态。
    pub task_state: CanonicalTaskState,
    /// 通知分类。
    pub category: NotificationCategory,
    /// 通知严重度。
    pub severity: NotificationSeverity,
    /// `L1/L2` 零行话中文标题。
    pub title_zh: String,
    /// `L1/L2` 零行话英文标题。
    pub title_en: String,
    /// `L1/L2` 零行话中文摘要。
    pub summary_zh: String,
    /// `L1/L2` 零行话英文摘要。
    pub summary_en: String,
    /// `L1/L2` 零行话中文状态文案。
    pub state_label_zh: String,
    /// `L1/L2` 零行话英文状态文案。
    pub state_label_en: String,
    /// 创建时的单调时钟毫秒数。
    pub created_at_mono_ms: u64,
    /// 是否未读。
    pub unread: bool,
    /// 是否已归档/忽略。
    pub dismissed: bool,
    /// 绑定的一次性内联 `ActionToken`（若存在）。
    pub action_token: Option<NotificationActionTokenView>,
    /// 当前允许执行的动作动词集合（由 `action-pruner` 规则同步裁剪）。
    pub allowed_verbs: Vec<String>,
    /// `L3` 工程诊断字段（仅在 `L3` 披露层级渲染）。
    pub l3_diagnostics: NotificationL3Diagnostics,
}

/// `list_notification_feed` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListNotificationFeedRequest {
    /// 分页游标：仅返回 `sequence < cursor_sequence` 的较早通知（`None` 表示从最新一条开始）。
    pub cursor_sequence: Option<u64>,
    /// 每页返回上限（自动钳制在 `1..=64`）。
    pub limit: u16,
    /// 是否仅过滤未读项。
    pub unread_only: bool,
    /// 可选分类过滤。
    pub category_filter: Option<NotificationCategory>,
    /// 是否包含已归档/忽略的通知。
    pub include_dismissed: bool,
}

impl Default for ListNotificationFeedRequest {
    fn default() -> Self {
        Self {
            cursor_sequence: None,
            limit: 20,
            unread_only: false,
            category_filter: None,
            include_dismissed: false,
        }
    }
}

/// `list_notification_feed` 响应快照。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotificationFeedPage {
    /// 当前页通知条目（按 `sequence` 降序排列，最新在前）。
    pub items: Vec<NotificationFeedItem>,
    /// 下一页游标（若还有更多条目）。
    pub next_cursor_sequence: Option<u64>,
    /// 是否还有更多未加载条目。
    pub has_more: bool,
    /// 当前未读通知总数。
    pub total_unread_count: u32,
    /// 当前活动通知总数（不含已忽略）。
    pub total_active_count: u32,
    /// 当前已确认水位线序号。
    pub ack_watermark_sequence: u64,
    /// `L3` 裁剪水位线序号。
    pub prune_watermark_sequence: u64,
    /// `L3` 已撤回事件累计计数。
    pub retracted_event_count: u32,
}

/// `redeem_notification_action_token` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RedeemNotificationActionTokenRequest {
    /// 目标通知 ID（32 hex chars）。
    pub notification_id_hex: String,
    /// 待核销的一次性令牌 ID（32 hex chars）。
    pub token_id_hex: String,
    /// 预期执行的动作标识（如 `"approve_task_step"`）。
    pub action_id: String,
    /// 发起核销的调用方应用 ID（32 hex chars）。
    pub caller_app_id_hex: String,
    /// 发起核销的调用方主体 Principal ID（32 hex chars）。
    pub caller_principal_id_hex: String,
    /// 关联任务 ID（32 hex chars）。
    pub task_id_hex: String,
    /// 当前单调时钟毫秒数（用于有效期校验）。
    pub now_mono_ms: u64,
    /// 可选调用方显式提交的承诺摘要（64 hex chars，若提供则额外比对）。
    pub presented_commitment_hex: Option<String>,
}

/// `redeem_notification_action_token` 成功核销回执。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RedeemNotificationActionTokenReceipt {
    /// 通知 ID（32 hex chars）。
    pub notification_id_hex: String,
    /// 已消费令牌 ID（32 hex chars）。
    pub token_id_hex: String,
    /// 执行的动作标识。
    pub action_id: String,
    /// 关联任务 ID（32 hex chars）。
    pub task_id_hex: String,
    /// 核销后的任务状态。
    pub resulting_task_state: CanonicalTaskState,
    /// 核销回执 SHA-256 摘要（64 hex chars）。
    pub redemption_receipt_digest_hex: String,
    /// 核销单调时钟毫秒数。
    pub redeemed_at_mono_ms: u64,
    /// 核销后该通知剩余允许动词（`redeem_action_token` 已被移除）。
    pub remaining_allowed_verbs: Vec<String>,
    /// 更新后的未读通知总数。
    pub updated_unread_count: u32,
}

/// `dismiss_notification_item` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DismissNotificationItemRequest {
    /// 目标通知 ID（32 hex chars）。
    pub notification_id_hex: String,
    /// 当前单调时钟毫秒数。
    pub now_mono_ms: u64,
}

/// `dismiss_notification_item` 响应回执。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DismissNotificationItemReceipt {
    /// 已归档通知 ID（32 hex chars）。
    pub notification_id_hex: String,
    /// 是否已归档。
    pub dismissed: bool,
    /// 更新后的确认水位线序号。
    pub ack_watermark_sequence: u64,
    /// 更新后的未读通知总数。
    pub updated_unread_count: u32,
}

/// `Ctrl+K` 搜索候选分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SpotlightCandidateCategory {
    Task,
    Artifact,
    App,
    Setting,
    Notification,
}

impl SpotlightCandidateCategory {
    /// `L1/L2` 中文分类标签。
    #[must_use]
    pub const fn label_zh(self) -> &'static str {
        match self {
            Self::Task => "任务",
            Self::Artifact => "本地制品与文档",
            Self::App => "受管应用",
            Self::Setting => "系统设置",
            Self::Notification => "通知历史",
        }
    }

    /// `L1/L2` 英文分类标签。
    #[must_use]
    pub const fn label_en(self) -> &'static str {
        match self {
            Self::Task => "Tasks",
            Self::Artifact => "Local Artifacts",
            Self::App => "Managed Apps",
            Self::Setting => "System Settings",
            Self::Notification => "Notification History",
        }
    }

    /// 对应的权威存储域名称。
    #[must_use]
    pub const fn authoritative_store_kind(self) -> &'static str {
        match self {
            Self::Task => "AUTHORITATIVE_TASK_STORE",
            Self::Artifact => "AUTHORITATIVE_ARTIFACT_STORE",
            Self::App => "AUTHORITATIVE_APP_REGISTRY",
            Self::Setting => "AUTHORITATIVE_SETTINGS_STORE",
            Self::Notification => "AUTHORITATIVE_NOTIFICATION_STORE",
        }
    }

    /// 权威跳转目标视图 ID。
    #[must_use]
    pub const fn target_view_id(self) -> &'static str {
        match self {
            Self::Task => "task-control-drawer",
            Self::Artifact => "artifact-inspector-view",
            Self::App => "launcher-workspace",
            Self::Setting => "settings-governance-view",
            Self::Notification => "notification-center-flyout",
        }
    }
}

/// 搜索索引分片健康度摘要。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpotlightShardStatus {
    /// 所属节点 `CellId`（32 hex chars）。
    pub cell_id_hex: String,
    /// `L1/L2` 友好节点名称。
    pub node_label_zh: String,
    pub node_label_en: String,
    /// 是否为本机主节点分片。
    pub is_local_primary: bool,
    /// 分片是否可达。
    pub reachable: bool,
    /// 分片快照代数。
    pub checkpoint_generation: u64,
}

/// `Ctrl+K` 搜索单条候选的 `L3` 诊断信息。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpotlightCandidateL3Diagnostics {
    /// 固定为 `"NON_AUTHORITATIVE · mask=0x1F"`。
    pub authority_tag: String,
    /// 限制位掩码十六进制表示（固定 `"0x1F"`）。
    pub restrictions_bitmask_hex: String,
    /// 来源分片节点 `CellId`（32 hex chars）。
    pub source_cell_id_hex: String,
    /// 语义向量检查点 ID（32 hex chars）。
    pub checkpoint_id_hex: String,
    /// 语义向量检查点签名摘要（64 hex chars）。
    pub checkpoint_digest_hex: String,
    /// 快照捕获时的权威代数。
    pub snapshot_generation: u64,
    /// 相关性得分（千分比整数 `0..=1000`）。
    pub score_permille: u16,
}

/// `Ctrl+K` 搜索单条候选条目（强制携带 `0x1F` 只读非权威标记）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpotlightCandidateItem {
    /// 搜索候选 ID（32 hex chars）。
    pub candidate_id_hex: String,
    /// 指向的权威对象 ID（32 hex chars）。
    pub target_id_hex: String,
    /// 候选分类。
    pub category: SpotlightCandidateCategory,
    /// `L1/L2` 零行话中文标题。
    pub title_zh: String,
    /// `L1/L2` 零行话英文标题。
    pub title_en: String,
    /// `L1/L2` 零行话中文副标题/摘要。
    pub snippet_zh: String,
    /// `L1/L2` 零行话英文副标题/摘要。
    pub snippet_en: String,
    /// `L1/L2` 零行话只读候选提示（固定为 `"搜索候选（点击跳转核验）"`）。
    pub candidate_hint_zh: String,
    /// `L1/L2` 零行话只读候选提示（固定为 `"Search candidate — click to verify source"`）。
    pub candidate_hint_en: String,
    /// 关联任务状态（若为任务或通知类候选）。
    pub task_state: Option<CanonicalTaskState>,
    /// 权威等级（法定锁 #1：固定为 `"NON_AUTHORITATIVE"`）。
    pub authority_grade: String,
    /// 法定限制位掩码（固定为 `0x1F` = `31`）。
    pub restrictions_bitmask: u8,
    /// 搜索命中所允许的操作动词（严格限制为只读与跳转：`["inspect", "jump_to_authority"]`）。
    pub allowed_verbs: Vec<String>,
    /// 快照捕获时的代数（用于跳转时与权威存储比对）。
    pub snapshot_generation: u64,
    /// `L3` 工程诊断元数据。
    pub l3_diagnostics: SpotlightCandidateL3Diagnostics,
}

/// `query_spotlight_candidates` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QuerySpotlightCandidatesRequest {
    /// 搜索关键词（空字符串返回最近高优候选）。
    pub query_text: String,
    /// 可选分类过滤。
    pub category_filter: Option<SpotlightCandidateCategory>,
    /// 返回条数上限（自动钳制在 `1..=32`）。
    pub limit: u16,
}

impl Default for QuerySpotlightCandidatesRequest {
    fn default() -> Self {
        Self {
            query_text: String::new(),
            category_filter: None,
            limit: 12,
        }
    }
}

/// `query_spotlight_candidates` 响应信封。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpotlightQueryResponse {
    /// 归一化后的查询词。
    pub normalized_query: String,
    /// 候选列表（全部强制携带 `0x1F` 非权威只读锁）。
    pub candidates: Vec<SpotlightCandidateItem>,
    /// 信封级权威等级（固定为 `"NON_AUTHORITATIVE"`）。
    pub envelope_authority_grade: String,
    /// 信封级限制位掩码（固定为 `0x1F`）。
    pub envelope_restrictions_bitmask: u8,
    /// 是否存在不可达的远端联邦索引分片。
    pub has_degraded_shards: bool,
    /// `L1/L2` 零行话分片状态说明。
    pub scope_notice_zh: String,
    /// `L1/L2` 零行话分片状态说明（英文）。
    pub scope_notice_en: String,
    /// 分片状态明细。
    pub shard_statuses: Vec<SpotlightShardStatus>,
    /// `L3` 一致性级别描述。
    pub l3_consistency_level: String,
    /// `L3` 检查点签名摘要（64 hex chars）。
    pub l3_checkpoint_digest_hex: String,
    /// `L3` 已过滤的撤回条目数。
    pub l3_retracted_filtered_count: u32,
}

/// 法定锁 #1 (`0x1F`) 禁止直接由搜索命中触发的五大控制面门禁。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SearchForbiddenControlGate {
    /// `0x01`: 能力授权 (`CapabilityGrant`)。
    CapabilityGrant,
    /// `0x02`: 资源账本扣减 (`ResourceLedgerDeduction`)。
    ResourceLedgerDeduction,
    /// `0x04`: 任务状态提交 (`TaskCommit`)。
    TaskCommit,
    /// `0x08`: 应用更新 (`ApplicationUpdate`)。
    ApplicationUpdate,
    /// `0x10`: 不可逆动作 (`IrreversibleAction`)。
    IrreversibleAction,
}

impl SearchForbiddenControlGate {
    /// 对应的掩码位。
    #[must_use]
    pub const fn mask_bit(self) -> u8 {
        match self {
            Self::CapabilityGrant => SPOTLIGHT_FORBID_CAPABILITY_GRANT,
            Self::ResourceLedgerDeduction => SPOTLIGHT_FORBID_LEDGER_DEDUCTION,
            Self::TaskCommit => SPOTLIGHT_FORBID_TASK_COMMIT,
            Self::ApplicationUpdate => SPOTLIGHT_FORBID_APP_UPDATE,
            Self::IrreversibleAction => SPOTLIGHT_FORBID_IRREVERSIBLE_ACTION,
        }
    }

    /// 门禁名称。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityGrant => "CAPABILITY_GRANT",
            Self::ResourceLedgerDeduction => "RESOURCE_LEDGER_DEDUCTION",
            Self::TaskCommit => "TASK_COMMIT",
            Self::ApplicationUpdate => "APPLICATION_UPDATE",
            Self::IrreversibleAction => "IRREVERSIBLE_ACTION",
        }
    }
}

/// `attempt_mutation_from_search_hit` 请求参数（测试与运行时法定锁 #1 拦截入口）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttemptMutationFromSearchHitRequest {
    /// 搜索候选 ID（32 hex chars）。
    pub candidate_id_hex: String,
    /// 试图越权调用的控制面门禁。
    pub attempted_gate: SearchForbiddenControlGate,
    /// 调用方试图伪造或覆盖的限制位掩码（即使传入 `Some(0x00)` 也必须被后端强制拒绝）。
    pub caller_supplied_mask: Option<u8>,
}

/// `verify_spotlight_candidate_authority` 请求参数（点击搜索候选跳转至权威视图前的重核验）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VerifySpotlightCandidateAuthorityRequest {
    /// 搜索候选 ID（32 hex chars）。
    pub candidate_id_hex: String,
    /// 预期跳转的目标权威对象 ID（32 hex chars）。
    pub expected_target_id_hex: String,
    /// 搜索候选捕获时的快照代数。
    pub snapshot_generation: u64,
}

/// `verify_spotlight_candidate_authority` 权威重核验回执。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpotlightAuthorityVerificationReceipt {
    /// 候选 ID（32 hex chars）。
    pub candidate_id_hex: String,
    /// 权威对象 ID（32 hex chars）。
    pub target_id_hex: String,
    /// 候选分类。
    pub category: SpotlightCandidateCategory,
    /// 是否通过权威存储核验。
    pub verified: bool,
    /// 权威存储类型标识（如 `"AUTHORITATIVE_TASK_STORE"`）。
    pub authoritative_store_kind: String,
    /// 跳转的目标工作台视图标识（如 `"task-control-drawer"`）。
    pub target_view_id: String,
    /// 权威存储当前最新代数。
    pub authoritative_generation: u64,
    /// 权威核验回执 SHA-256 摘要（64 hex chars）。
    pub verification_digest_hex: String,
    /// 跳转进入权威视图后由权威对象解锁的操作动词集合。
    pub unlocked_authoritative_verbs: Vec<String>,
    /// `L1/L2` 零行话核验成功说明（中文）。
    pub message_zh: String,
    /// `L1/L2` 零行话核验成功说明（英文）。
    pub message_en: String,
}

/// 集群连接与离线自治模式（对应 `PartitionAutonomyGate` 四态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClusterConnectivityMode {
    /// 集群全连接正常。
    Connected,
    /// 网络分区，本机进入离线自治模式。
    Partitioned,
    /// 网络恢复中，正在重新同步租约与分片。
    Reconnecting,
    /// 纯单机自治模式（无外部节点依赖）。
    LocalSingleNode,
}

impl ClusterConnectivityMode {
    /// 模式常量字符串。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "CONNECTED",
            Self::Partitioned => "PARTITIONED",
            Self::Reconnecting => "RECONNECTING",
            Self::LocalSingleNode => "LOCAL_SINGLE_NODE",
        }
    }

    /// `L0/L1/L2` 零行话简短状态标签（中文）。
    #[must_use]
    pub const fn badge_label_zh(self) -> &'static str {
        match self {
            Self::Connected => "在线协同",
            Self::Partitioned => "离线自治",
            Self::Reconnecting => "正在重连同步",
            Self::LocalSingleNode => "本机独立运行",
        }
    }

    /// `L0/L1/L2` 零行话简短状态标签（英文）。
    #[must_use]
    pub const fn badge_label_en(self) -> &'static str {
        match self {
            Self::Connected => "Online Synced",
            Self::Partitioned => "Offline Autonomy",
            Self::Reconnecting => "Reconnecting",
            Self::LocalSingleNode => "Local Standalone",
        }
    }

    /// `L1/L2` 零行话态势说明（中文）。
    #[must_use]
    pub const fn summary_copy_zh(self) -> &'static str {
        match self {
            Self::Connected => "本机与远端协同节点连接正常，所有服务均可使用。",
            Self::Partitioned => {
                "远端网络暂时中断 · 本机应用启动、本地制品、任务历史、系统设置、本机搜索与通知中心 100% 正常可用。"
            }
            Self::Reconnecting => {
                "正在重新连接远端协同节点 · 本机功能保持连续可用，云端协同操作将在同步完成后恢复。"
            }
            Self::LocalSingleNode => {
                "当前以本机独立模式运行 · 所有本机工作台功能、历史记录与本地检索均完全可用。"
            }
        }
    }

    /// `L1/L2` 零行话态势说明（英文）。
    #[must_use]
    pub const fn summary_copy_en(self) -> &'static str {
        match self {
            Self::Connected => {
                "Local workstation and remote nodes are connected; all services are available."
            }
            Self::Partitioned => {
                "Remote network interrupted · Launcher, local artifacts, task history, settings, local search, and notifications remain 100% available."
            }
            Self::Reconnecting => {
                "Reconnecting to remote nodes · Local workstation functions remain continuously available while cloud actions resume after sync."
            }
            Self::LocalSingleNode => {
                "Running in local standalone mode · All local workstation views, history, and local search are 100% available."
            }
        }
    }
}

/// 节点态势枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CellNodePosture {
    /// 本机主控自治节点（始终在线可用）。
    ActiveLocalPrimary,
    /// 可达的远端工作节点。
    ReachableRemoteWorker,
    /// 因网络分区暂时不可达的远端工作节点。
    PartitionedRemoteWorker,
    /// 处于 `QUARANTINED` 租约隔离围栏锁定态的节点（法定锁 #3）。
    QuarantinedFenced,
}

impl CellNodePosture {
    /// `L1/L2` 零行话节点状态文案（中文）。
    #[must_use]
    pub const fn label_zh(self) -> &'static str {
        match self {
            Self::ActiveLocalPrimary => "本机工作站（自治就绪）",
            Self::ReachableRemoteWorker => "远端协同节点（在线）",
            Self::PartitionedRemoteWorker => "远端协同节点（离线等待网络）",
            Self::QuarantinedFenced => "受保护隔离节点（安全锁定）",
        }
    }

    /// `L1/L2` 零行话节点状态文案（英文）。
    #[must_use]
    pub const fn label_en(self) -> &'static str {
        match self {
            Self::ActiveLocalPrimary => "Local Workstation (Autonomy Ready)",
            Self::ReachableRemoteWorker => "Remote Node (Online)",
            Self::PartitionedRemoteWorker => "Remote Node (Offline — Waiting for Network)",
            Self::QuarantinedFenced => "Isolated Node (Safety Fence Locked)",
        }
    }
}

/// 本机自治证明徽章 (`LocalAutonomyProof`)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalAutonomyProofBadge {
    /// 证明标识（32 hex chars）。
    pub proof_id_hex: String,
    /// 本机主节点 `CellId`（32 hex chars）。
    pub local_cell_id_hex: String,
    /// 当前自治纪元 (`epoch`)。
    pub autonomy_epoch: u64,
    /// 证明覆盖的 6 大本机无网 100% 可交互功能域。
    pub offline_capable_surfaces: Vec<String>,
    /// `L1/L2` 中文徽章标签。
    pub label_zh: String,
    /// `L1/L2` 英文徽章标签。
    pub label_en: String,
    /// `L3` SHA-256 自治证明签名摘要（64 hex chars）。
    pub signature_digest_hex: String,
}

/// 单个节点的 `L3` 工程诊断元数据（仅在 `L3` 披露层级显示）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusterCellL3Diagnostics {
    /// 节点 `CellId`（32 hex chars）。
    pub cell_id_hex: String,
    /// 权威租约 ID（32 hex chars）。
    pub lease_id_hex: String,
    /// 节点纪元 (`epoch`)。
    pub epoch: u64,
    /// 时钟偏斜毫秒数 (`clock_skew_ms`)。
    pub clock_skew_ms: i64,
    /// 最近一次心跳单调时钟毫秒数。
    pub last_heartbeat_mono_ms: u64,
    /// 围栏令牌摘要（64 hex chars）。
    pub fencing_token_hex: String,
}

/// 集群节点态势与 Nomad 三水位线租约摘要。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusterCellSummary {
    /// 节点 `CellId`（32 hex chars）。
    pub cell_id_hex: String,
    /// `L1/L2` 友好节点名称（中文）。
    pub display_name_zh: String,
    /// `L1/L2` 友好节点名称（英文）。
    pub display_name_en: String,
    /// 节点态势。
    pub posture: CellNodePosture,
    /// `L1/L2` 零行话状态标签（中文）。
    pub status_label_zh: String,
    /// `L1/L2` 零行话状态标签（英文）。
    pub status_label_en: String,
    /// 是否为本机主节点。
    pub is_local_primary: bool,
    /// 是否当前可交互/可达。
    pub reachable: bool,
    /// Nomad 三水位线微型条：当前已用租约毫秒数 / 负载水位。
    pub lease_elapsed_ms: u64,
    /// Nomad 三水位线微型条：软警告水位毫秒数。
    pub lease_soft_watermark_ms: u64,
    /// Nomad 三水位线微型条：硬上限毫秒数。
    pub lease_hard_ceiling_ms: u64,
    /// 是否受法定锁 #3 (`QUARANTINED_LEASE_FENCE`) 硬件围栏锁定。
    pub quarantined_fence_locked: bool,
    /// `L3` 工程诊断元数据。
    pub l3_diagnostics: ClusterCellL3Diagnostics,
}

/// 集群整体 `L3` 工程诊断摘要。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusterL3Diagnostics {
    /// `PartitionAutonomyGate` 原始状态名。
    pub partition_autonomy_gate_state: String,
    /// 集群协调纪元 (`cluster_epoch`)。
    pub cluster_epoch: u64,
    /// 活跃租约数量。
    pub active_lease_count: u32,
    /// 隔离围栏锁定节点数量 (`QUARANTINED`)。
    pub quarantined_cell_count: u32,
    /// 离线远端动作拦截计数。
    pub offline_remote_action_blocks: u64,
    /// `QUARANTINED` 本地 TTL 非法解锁拦截计数。
    pub quarantined_ttl_unlock_rejections: u64,
}

/// `inspect_offline_cluster_posture` 完整响应快照 (`IPC-D7`)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OfflineClusterPostureSnapshot {
    /// 当前连接与自治模式。
    pub connectivity_mode: ClusterConnectivityMode,
    /// 本机自治是否就绪（`[DUI-OFFLINE-001]`：本机工作台始终为 `true`）。
    pub local_autonomy_active: bool,
    /// 本机自治证明徽章 (`LocalAutonomyProof`)。
    pub local_autonomy_proof: LocalAutonomyProofBadge,
    /// `L0/L1/L2` 简短状态标签（中文）。
    pub badge_label_zh: String,
    /// `L0/L1/L2` 简短状态标签（英文）。
    pub badge_label_en: String,
    /// `L1/L2` 零行话状态说明（中文）。
    pub status_copy_zh: String,
    /// `L1/L2` 零行话状态说明（英文）。
    pub status_copy_en: String,
    /// 节点列表（含本机主节点、远端协同节点与隔离围栏节点）。
    pub cells: Vec<ClusterCellSummary>,
    /// `L3` 工程诊断元数据。
    pub l3_diagnostics: ClusterL3Diagnostics,
}

/// `[DUI-OFFLINE-001]` 工作台动作所属功能面分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OfflineSurfaceKind {
    /// 应用启动台 (`Launcher`)。
    Launcher,
    /// 本地制品与文件检视 (`LocalArtifacts`)。
    LocalArtifacts,
    /// 任务历史浏览与本机任务控制 (`TaskHistoryAndLocalTasks`)。
    TaskHistoryAndLocalTasks,
    /// 系统设置 (`Settings`)。
    Settings,
    /// 本机 `Ctrl+K` 搜索 (`LocalSearch`)。
    LocalSearch,
    /// 通知中心与历史记录 (`NotificationHistory`)。
    NotificationHistory,
    /// 强依赖远端云模型或跨节点协同动作 (`RemoteClusterDispatch`)。
    RemoteClusterDispatch,
}

impl OfflineSurfaceKind {
    /// 功能面稳定标识。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Launcher => "launcher",
            Self::LocalArtifacts => "artifacts",
            Self::TaskHistoryAndLocalTasks => "tasks",
            Self::Settings => "settings",
            Self::LocalSearch => "search",
            Self::NotificationHistory => "notifications",
            Self::RemoteClusterDispatch => "remote_cluster_dispatch",
        }
    }
}

/// 离线动作可用性评估结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OfflineActionAvailabilityStatus {
    /// 在线全功能可用。
    AvailableOnline,
    /// 离线/单机模式下由 `LocalAutonomyProof` 担保本机 100% 可交互 (`[DUI-OFFLINE-001]`)。
    AvailableLocalAutonomy,
    /// 强依赖远端 `Provider` / `Cell`，当前处于离线等待网络恢复状态 (`WAITING_NETWORK`)。
    WaitingNetwork,
    /// 远端服务在当前单机配置下不可用 (`UNAVAILABLE`)。
    UnavailableRemoteOffline,
    /// 目标节点处于 `QUARANTINED` 租约围栏锁定态，法定锁 #3 强制拦截 (`BLOCKED_QUARANTINED_FENCE`)。
    BlockedQuarantinedFence,
}

/// `evaluate_offline_action_availability` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvaluateOfflineActionAvailabilityRequest {
    /// 目标工作台功能面。
    pub surface: OfflineSurfaceKind,
    /// 请求执行的动作动词（如 `"open"`, `"inspect"`, `"invoke_remote_llm"`, `"dispatch_remote_cell"`）。
    pub action_verb: String,
    /// 该动作是否强依赖远端模型 `Provider` 或外部节点网络。
    pub requires_remote_provider: bool,
    /// 可选目标节点 `CellId`（32 hex chars；若不填则默认路由至本机主节点）。
    pub target_cell_id_hex: Option<String>,
}

/// `evaluate_offline_action_availability` 评估结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OfflineActionAvailabilityDecision {
    /// 目标功能面。
    pub surface: OfflineSurfaceKind,
    /// 动作动词。
    pub action_verb: String,
    /// 可用性状态结论。
    pub status: OfflineActionAvailabilityStatus,
    /// 前端是否允许交互执行（本机 6 大功能面在离线时恒为 `true`）。
    pub interactive: bool,
    /// 若允许本机离线执行，附带 `LocalAutonomyProof` 徽章证明。
    pub autonomy_proof: Option<LocalAutonomyProofBadge>,
    /// 若触发法定不可篡改锁（如 `QUARANTINED_LEASE_FENCE`），返回对应锁标识。
    pub statutory_lock_kind: Option<String>,
    /// `L1/L2` 零行话原因说明（中文）。
    pub reason_zh: String,
    /// `L1/L2` 零行话原因说明（英文）。
    pub reason_en: String,
    /// `L3` 工程诊断标签（含 `CellId`、`epoch`、租约状态）。
    pub l3_diagnostic_tag: String,
}

/// `set_cluster_connectivity_mode` 请求参数。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetClusterConnectivityModeRequest {
    /// 目标连接模式。
    pub mode: ClusterConnectivityMode,
    /// 当前单调时钟毫秒数。
    pub now_mono_ms: u64,
}

/// 安全与不变式拦截审计计数器。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotifySearchOfflineAuditCounters {
    /// `ActionToken` 成功核销次数。
    pub action_token_redemptions_succeeded: u64,
    /// `ActionToken` 重放攻击拦截次数 (`ACTION_TOKEN_ALREADY_CONSUMED`)。
    pub action_token_replay_rejections: u64,
    /// `ActionToken` 过期拦截次数 (`ACTION_TOKEN_EXPIRED`)。
    pub action_token_expired_rejections: u64,
    /// `ActionToken` 应用 ID 不匹配拦截次数 (`ACTION_TOKEN_APP_MISMATCH`)。
    pub action_token_app_mismatch_rejections: u64,
    /// `ActionToken` 主体 Principal ID 不匹配拦截次数 (`ACTION_TOKEN_PRINCIPAL_MISMATCH`)。
    pub action_token_principal_mismatch_rejections: u64,
    /// `ActionToken` 任务 ID 不匹配拦截次数 (`ACTION_TOKEN_TASK_MISMATCH`)。
    pub action_token_task_mismatch_rejections: u64,
    /// `ActionToken` 承诺校验失败拦截次数 (`ACTION_TOKEN_INVALID_PROOF`)。
    pub action_token_invalid_proof_rejections: u64,
    /// 搜索命中直接驱动控制面写操作拦截总次数 (`SEARCH_RESULT_NON_AUTHORITATIVE_0X1F`)。
    pub search_0x1f_mutation_rejections: u64,
    /// 搜索命中试图授予能力拦截次数 (`0x01`)。
    pub search_forbid_capability_grant_hits: u64,
    /// 搜索命中试图扣减账本拦截次数 (`0x02`)。
    pub search_forbid_ledger_deduction_hits: u64,
    /// 搜索命中试图提交任务拦截次数 (`0x04`)。
    pub search_forbid_task_commit_hits: u64,
    /// 搜索命中试图更新应用拦截次数 (`0x08`)。
    pub search_forbid_app_update_hits: u64,
    /// 搜索命中试图执行不可逆动作拦截次数 (`0x10`)。
    pub search_forbid_irreversible_action_hits: u64,
    /// 搜索候选跳转时因权威记录已撤回被拒次数 (`AUTHORITY_RECORD_RETRACTED`)。
    pub search_authority_retracted_rejections: u64,
    /// 搜索候选跳转时因快照代数过期被拒次数 (`STALE_GENERATION`)。
    pub search_stale_generation_rejections: u64,
    /// 搜索候选成功完成权威跳转核验次数。
    pub search_authority_verifications_succeeded: u64,
    /// `QUARANTINED` 节点仅凭本地 TTL 试图解锁被拒次数 (`QUARANTINED_LEASE_FENCE_LOCKED`)。
    pub quarantined_lease_ttl_unlock_rejections: u64,
    /// 离线状态下远端依赖操作降级/阻断次数。
    pub offline_remote_action_blocks: u64,
}

#[derive(Debug, Clone)]
struct AuthoritativeObjectRecord {
    category: SpotlightCandidateCategory,
    authoritative_generation: u64,
    retracted: bool,
    unlocked_verbs: Vec<String>,
}

#[derive(Debug)]
struct NotifySearchOfflineInner {
    notifications: Vec<NotificationFeedItem>,
    ack_watermark_sequence: u64,
    prune_watermark_sequence: u64,
    retracted_event_count: u32,
    spotlight_candidates: Vec<SpotlightCandidateItem>,
    authoritative_records: BTreeMap<String, AuthoritativeObjectRecord>,
    connectivity_mode: ClusterConnectivityMode,
    cluster_epoch: u64,
    local_autonomy_proof: LocalAutonomyProofBadge,
    cells: Vec<ClusterCellSummary>,
    audit: NotifySearchOfflineAuditCounters,
}

/// 通知中心、`Ctrl+K` 全局搜索与离线自治态势宿主状态管理器。
#[derive(Debug)]
pub struct NotifySearchOfflineState {
    inner: Mutex<NotifySearchOfflineInner>,
}

impl Default for NotifySearchOfflineState {
    fn default() -> Self {
        Self::with_default_fixture()
    }
}

#[must_use]
fn deterministic_hex_id_16(tag: &str, index: u32) -> String {
    let digest = sha256_domain_digest(
        b"LLMOS_STAGE_D_W72_FIXTURE_ID16",
        &[tag.as_bytes(), &index.to_be_bytes()],
    );
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    hex(&id)
}

#[must_use]
fn compute_action_token_commitment_hex(
    notification_id_hex: &str,
    token_id_hex: &str,
    action_id: &str,
    app_id_hex: &str,
    task_id_hex: &str,
    principal_id_hex: &str,
    expiry_mono_ms: u64,
) -> String {
    let digest = sha256_domain_digest(
        DOMAIN_ACTION_TOKEN_COMMITMENT,
        &[
            notification_id_hex.as_bytes(),
            token_id_hex.as_bytes(),
            action_id.as_bytes(),
            app_id_hex.as_bytes(),
            task_id_hex.as_bytes(),
            principal_id_hex.as_bytes(),
            &expiry_mono_ms.to_be_bytes(),
        ],
    );
    hex(&digest)
}

#[must_use]
fn constant_time_eq_str(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl NotifySearchOfflineState {
    /// 创建搭载完整 16 态通知夹具、`0x1F` 搜索候选夹具与多节点离线自治态势夹具的状态实例。
    #[must_use]
    pub fn with_default_fixture() -> Self {
        let default_principal_id_hex = "a1a2a3a4a5a6a7a8b1b2b3b4b5b6b7b8".to_string();
        let default_app_id_hex = "c1c2c3c4c5c6c7c8d1d2d3d4d5d6d7d8".to_string();
        let subscriber_key_hex = deterministic_hex_id_16("subscriber", 1);

        type StateTitleSeed<'a> = (
            CanonicalTaskState,
            &'a str,
            &'a str,
            &'a str,
            &'a str,
            Option<(&'a str, &'a str, &'a str)>,
        );
        let state_titles: [StateTitleSeed<'_>; 16] = [
            (
                CanonicalTaskState::Draft,
                "季度财务报表整理草稿已创建",
                "Quarterly financial report draft created",
                "已为您准备好报表汇总草稿，可随时查看或继续编辑。",
                "Your report aggregation draft is ready to review or continue editing.",
                Some(("open_draft_task", "查看草稿", "Open Draft")),
            ),
            (
                CanonicalTaskState::Planning,
                "代码仓依赖升级方案规划中",
                "Planning repository dependency upgrade",
                "正在分析模块依赖关系并生成安全的分步升级计划。",
                "Analyzing module dependencies and generating a safe step-by-step upgrade plan.",
                Some(("inspect_plan_steps", "查看规划进度", "View Plan Progress")),
            ),
            (
                CanonicalTaskState::AwaitingApproval,
                "生产环境配置导出需要您的确认",
                "Production config export awaits your confirmation",
                "导出操作将生成包含环境参数的归档文件，请确认是否继续执行。",
                "Exporting will generate an archive with environment parameters; please confirm to proceed.",
                Some(("approve_task_step", "批准并继续", "Approve & Continue")),
            ),
            (
                CanonicalTaskState::Scheduled,
                "夜间全量索引构建已排入队列",
                "Nightly full index build queued",
                "任务已按计划排入本机空闲队列，将在预定时间自动启动。",
                "Task is queued for local idle execution and will start automatically as scheduled.",
                Some(("run_scheduled_now", "立即启动", "Start Now")),
            ),
            (
                CanonicalTaskState::Running,
                "正在执行多语言文档校验",
                "Running multilingual documentation verification",
                "已完成 68% 的文档章节交叉核对，预计剩余 20 秒。",
                "Completed 68% of document section cross-checks; about 20 seconds remaining.",
                Some(("inspect_running_task", "查看实时进度", "View Live Progress")),
            ),
            (
                CanonicalTaskState::Paused,
                "大文件归档压缩已暂停",
                "Large archive compression paused",
                "任务已按您的指令安全暂停，保留当前处理进度。",
                "Task is safely paused at your request with current progress preserved.",
                Some(("resume_paused_task", "继续执行", "Resume Task")),
            ),
            (
                CanonicalTaskState::WaitingDependency,
                "等待前置数据清洗步骤完成",
                "Waiting for prerequisite data cleaning step",
                "当前分析任务将在上游数据清洗完成后自动接续运行。",
                "This analysis task will automatically resume once upstream data cleaning finishes.",
                None,
            ),
            (
                CanonicalTaskState::WaitingHuman,
                "报表导出路径需要补充选择",
                "Report export destination needs your selection",
                "请选择保存导出文件的本地目标文件夹以继续完成任务。",
                "Please choose a local destination folder for the exported file to continue.",
                Some(("provide_human_input", "选择目标文件夹", "Choose Folder")),
            ),
            (
                CanonicalTaskState::Succeeded,
                "桌面视觉规范一致性巡检已完成",
                "Desktop visual specification audit completed",
                "全部检查项均已通过，已生成可随时查阅的本地核验报告。",
                "All checks passed and a local verification report is ready for inspection.",
                Some(("open_succeeded_artifact", "打开核验报告", "Open Report")),
            ),
            (
                CanonicalTaskState::PartialSucceeded,
                "跨目录批量整理已部分完成",
                "Batch folder organization partially completed",
                "已完成 14 个目录的归档整理，有 2 个受保护文件被安全跳过。",
                "Archived 14 folders; 2 protected files were safely skipped.",
                Some(("review_partial_items", "查看跳过项", "Review Skipped Items")),
            ),
            (
                CanonicalTaskState::Degraded,
                "智能摘要服务已切换至本机离线模型",
                "Smart summary switched to local offline model",
                "由于远端网络暂不可达，已自动使用本机轻量模型继续为您提供服务。",
                "Remote network is unreachable; automatically continuing with the local lightweight model.",
                Some((
                    "acknowledge_degraded_mode",
                    "知悉并保持本机运行",
                    "Keep Local Mode",
                )),
            ),
            (
                CanonicalTaskState::Failed,
                "外部数据源同步连接受阻",
                "External data source sync blocked",
                "无法连接远端数据接口，本机已有数据未受影响，可稍后重试。",
                "Could not reach remote data endpoint; local data is intact and ready to retry later.",
                Some(("retry_failed_step", "重试同步", "Retry Sync")),
            ),
            (
                CanonicalTaskState::Cancelled,
                "临时日志导出任务已取消",
                "Temporary log export cancelled",
                "任务已按请求停止，未产生任何残留临时文件。",
                "Task stopped as requested without leaving any temporary files.",
                None,
            ),
            (
                CanonicalTaskState::Compensating,
                "正在回退未完成的批量重命名",
                "Cleaning up incomplete batch rename",
                "检测到中途取消，正在将已修改的文件名安全还原至初始状态。",
                "Cancellation detected; safely restoring modified filenames to their original state.",
                None,
            ),
            (
                CanonicalTaskState::Compensated,
                "批量重命名已完成补偿清理",
                "Batch rename cleanup completed",
                "所有已修改的文件名均已恢复原状，目录保持一致。",
                "All modified filenames have been restored and the directory is consistent.",
                None,
            ),
            (
                CanonicalTaskState::RolledBack,
                "工作台偏好设置更新已安全回滚",
                "Workspace preference update safely rolled back",
                "已还原至更新前的稳定配置快照，当前工作台状态完好。",
                "Restored to the stable configuration snapshot prior to the update.",
                Some((
                    "inspect_rollback_summary",
                    "查看回滚摘要",
                    "View Rollback Summary",
                )),
            ),
        ];

        let mut notifications = Vec::with_capacity(16);
        let mut authoritative_records = BTreeMap::new();

        for (idx, (task_state, title_zh, title_en, summary_zh, summary_en, action_spec)) in
            state_titles.into_iter().enumerate()
        {
            let sequence = (idx as u64) + 1;
            let notification_id_hex = deterministic_hex_id_16("notification", sequence as u32);
            let task_id_hex = deterministic_hex_id_16("task", sequence as u32);
            let topic_id_hex = deterministic_hex_id_16("topic", sequence as u32);
            let created_at_mono_ms = 1_700_000_000_000u64 + sequence * 15_000;
            let expiry_mono_ms = created_at_mono_ms + 3_600_000;

            let (action_token, verifier_commitment_hex, allowed_verbs) =
                if let Some((action_id, label_zh, label_en)) = action_spec {
                    let token_digest = sha256_domain_digest(
                        DOMAIN_ACTION_TOKEN_ID,
                        &[
                            notification_id_hex.as_bytes(),
                            task_id_hex.as_bytes(),
                            action_id.as_bytes(),
                        ],
                    );
                    let mut token_bytes = [0u8; 16];
                    token_bytes.copy_from_slice(&token_digest[..16]);
                    let token_id_hex = hex(&token_bytes);
                    let commitment_hex = compute_action_token_commitment_hex(
                        &notification_id_hex,
                        &token_id_hex,
                        action_id,
                        &default_app_id_hex,
                        &task_id_hex,
                        &default_principal_id_hex,
                        expiry_mono_ms,
                    );
                    let token_view = NotificationActionTokenView {
                        token_id_hex,
                        action_id: action_id.to_string(),
                        label_zh: label_zh.to_string(),
                        label_en: label_en.to_string(),
                        bound_notification_id_hex: notification_id_hex.clone(),
                        bound_app_id_hex: default_app_id_hex.clone(),
                        bound_task_id_hex: task_id_hex.clone(),
                        bound_principal_id_hex: default_principal_id_hex.clone(),
                        expiry_mono_ms,
                        consumed: false,
                        consumed_at_mono_ms: None,
                        redemption_receipt_digest_hex: None,
                        verifier_commitment_hex: commitment_hex.clone(),
                    };
                    (
                        Some(token_view),
                        commitment_hex,
                        vec![
                            "inspect".to_string(),
                            "redeem_action_token".to_string(),
                            "dismiss".to_string(),
                        ],
                    )
                } else {
                    let fallback_commitment = compute_action_token_commitment_hex(
                        &notification_id_hex,
                        "00000000000000000000000000000000",
                        "inspect_only",
                        &default_app_id_hex,
                        &task_id_hex,
                        &default_principal_id_hex,
                        expiry_mono_ms,
                    );
                    (
                        None,
                        fallback_commitment,
                        vec!["inspect".to_string(), "dismiss".to_string()],
                    )
                };

            authoritative_records.insert(
                task_id_hex.clone(),
                AuthoritativeObjectRecord {
                    category: SpotlightCandidateCategory::Task,
                    authoritative_generation: 1,
                    retracted: false,
                    unlocked_verbs: vec![
                        "inspect".to_string(),
                        "open_task_drawer".to_string(),
                        "export_audit_bundle".to_string(),
                    ],
                },
            );

            authoritative_records.insert(
                notification_id_hex.clone(),
                AuthoritativeObjectRecord {
                    category: SpotlightCandidateCategory::Notification,
                    authoritative_generation: 1,
                    retracted: false,
                    unlocked_verbs: allowed_verbs.clone(),
                },
            );

            notifications.push(NotificationFeedItem {
                notification_id_hex,
                sequence,
                task_id_hex,
                app_id_hex: default_app_id_hex.clone(),
                app_display_name_zh: "工作台协同中心".to_string(),
                app_display_name_en: "Workspace Coordinator".to_string(),
                task_state,
                category: task_state.default_category(),
                severity: task_state.default_severity(),
                title_zh: title_zh.to_string(),
                title_en: title_en.to_string(),
                summary_zh: summary_zh.to_string(),
                summary_en: summary_en.to_string(),
                state_label_zh: task_state.label_zh().to_string(),
                state_label_en: task_state.label_en().to_string(),
                created_at_mono_ms,
                unread: sequence >= 11,
                dismissed: false,
                action_token,
                allowed_verbs,
                l3_diagnostics: NotificationL3Diagnostics {
                    topic_id_hex,
                    subscriber_key_hex: subscriber_key_hex.clone(),
                    verifier_commitment_hex,
                    sequence,
                    prune_watermark_sequence: 0,
                    retracted_event_count: 2,
                    delivery_attempt: 1,
                },
            });
        }

        let local_cell_id_hex = "11111111111111112222222222222222".to_string();
        let remote_worker_cell_id_hex = "33333333333333334444444444444444".to_string();
        let quarantined_cell_id_hex = "55555555555555556666666666666666".to_string();

        let checkpoint_id_hex = deterministic_hex_id_16("search_checkpoint", 1);
        let checkpoint_digest_hex = hex(&sha256_domain_digest(
            DOMAIN_SEARCH_CHECKPOINT,
            &[checkpoint_id_hex.as_bytes(), local_cell_id_hex.as_bytes()],
        ));

        type SpotlightSeed<'a> = (
            u32,
            SpotlightCandidateCategory,
            String,
            &'a str,
            &'a str,
            &'a str,
            &'a str,
            Option<CanonicalTaskState>,
            &'a str,
            u16,
        );
        let spotlight_specs: [SpotlightSeed<'_>; 6] = [
            (
                1,
                SpotlightCandidateCategory::Task,
                notifications[2].task_id_hex.clone(),
                "生产环境配置导出任务",
                "Production Config Export Task",
                "状态：等待您的确认 · 包含环境参数归档校验",
                "Status: Awaiting your confirmation · Includes environment archive check",
                Some(CanonicalTaskState::AwaitingApproval),
                &local_cell_id_hex,
                980,
            ),
            (
                2,
                SpotlightCandidateCategory::Artifact,
                deterministic_hex_id_16("artifact", 1),
                "Stage D 桌面视觉一致性核验报告.md",
                "Stage D Desktop Visual Audit Report.md",
                "本地制品 · Graphite 与 Paper 双主题对比度及零毛玻璃核验记录",
                "Local artifact · Graphite & Paper contrast and zero-glass verification log",
                Some(CanonicalTaskState::Succeeded),
                &local_cell_id_hex,
                955,
            ),
            (
                3,
                SpotlightCandidateCategory::App,
                default_app_id_hex.clone(),
                "工作台制品检视器",
                "Workspace Artifact Inspector",
                "受管桌面应用 · 支持离线查阅本地证据包、日志与制品快照",
                "Managed desktop app · Offline inspection of local evidence bundles and snapshots",
                None,
                &local_cell_id_hex,
                920,
            ),
            (
                4,
                SpotlightCandidateCategory::Setting,
                deterministic_hex_id_16("setting", 1),
                "主题外观与高对比度护眼设置",
                "Theme Appearance & High-Contrast Settings",
                "系统设置 · 切换 Graphite 暗色、Paper 亮色与高对比度模式",
                "System settings · Switch between Graphite dark, Paper light, and high-contrast modes",
                None,
                &local_cell_id_hex,
                890,
            ),
            (
                5,
                SpotlightCandidateCategory::Notification,
                notifications[10].notification_id_hex.clone(),
                "智能摘要服务已切换至本机离线模型",
                "Smart Summary Switched to Local Offline Model",
                "通知历史 · 远端网络不可达时由本机模型连续提供服务",
                "Notification history · Local model provides continuous service when network is offline",
                Some(CanonicalTaskState::Degraded),
                &local_cell_id_hex,
                860,
            ),
            (
                6,
                SpotlightCandidateCategory::Artifact,
                deterministic_hex_id_16("artifact", 2),
                "跨节点联邦构建缓存清单",
                "Federated Node Build Cache Manifest",
                "协同节点索引快照 · 点击后将向来源存储重核验最新版本",
                "Federated node index snapshot · Clicking re-verifies latest version with source store",
                Some(CanonicalTaskState::Running),
                &remote_worker_cell_id_hex,
                830,
            ),
        ];

        let mut spotlight_candidates = Vec::with_capacity(spotlight_specs.len());
        for (
            idx,
            category,
            target_id_hex,
            title_zh,
            title_en,
            snippet_zh,
            snippet_en,
            task_state,
            source_cell_id,
            score_permille,
        ) in spotlight_specs
        {
            let candidate_id_hex = deterministic_hex_id_16("spotlight_candidate", idx);
            authoritative_records
                .entry(target_id_hex.clone())
                .or_insert_with(|| AuthoritativeObjectRecord {
                    category,
                    authoritative_generation: 1,
                    retracted: false,
                    unlocked_verbs: vec![
                        "inspect".to_string(),
                        "open".to_string(),
                        "export".to_string(),
                    ],
                });

            spotlight_candidates.push(SpotlightCandidateItem {
                candidate_id_hex,
                target_id_hex,
                category,
                title_zh: title_zh.to_string(),
                title_en: title_en.to_string(),
                snippet_zh: snippet_zh.to_string(),
                snippet_en: snippet_en.to_string(),
                candidate_hint_zh: "搜索候选（点击跳转核验）".to_string(),
                candidate_hint_en: "Search candidate — click to verify source".to_string(),
                task_state,
                authority_grade: "NON_AUTHORITATIVE".to_string(),
                restrictions_bitmask: SPOTLIGHT_STATUTORY_MASK_0X1F,
                allowed_verbs: vec!["inspect".to_string(), "jump_to_authority".to_string()],
                snapshot_generation: 1,
                l3_diagnostics: SpotlightCandidateL3Diagnostics {
                    authority_tag: "NON_AUTHORITATIVE · mask=0x1F".to_string(),
                    restrictions_bitmask_hex: "0x1F".to_string(),
                    source_cell_id_hex: source_cell_id.to_string(),
                    checkpoint_id_hex: checkpoint_id_hex.clone(),
                    checkpoint_digest_hex: checkpoint_digest_hex.clone(),
                    snapshot_generation: 1,
                    score_permille,
                },
            });
        }

        let proof_id_hex = deterministic_hex_id_16("autonomy_proof", 1);
        let autonomy_epoch = 42u64;
        let proof_sig = hex(&sha256_domain_digest(
            DOMAIN_LOCAL_AUTONOMY_PROOF,
            &[
                proof_id_hex.as_bytes(),
                local_cell_id_hex.as_bytes(),
                &autonomy_epoch.to_be_bytes(),
            ],
        ));

        let local_autonomy_proof = LocalAutonomyProofBadge {
            proof_id_hex,
            local_cell_id_hex: local_cell_id_hex.clone(),
            autonomy_epoch,
            offline_capable_surfaces: vec![
                "launcher".to_string(),
                "artifacts".to_string(),
                "tasks".to_string(),
                "settings".to_string(),
                "search".to_string(),
                "notifications".to_string(),
            ],
            label_zh: "本机自治已就绪".to_string(),
            label_en: "Local Autonomy Ready".to_string(),
            signature_digest_hex: proof_sig,
        };

        let cells = vec![
            ClusterCellSummary {
                cell_id_hex: local_cell_id_hex.clone(),
                display_name_zh: "本机主工作站".to_string(),
                display_name_en: "Primary Local Workstation".to_string(),
                posture: CellNodePosture::ActiveLocalPrimary,
                status_label_zh: CellNodePosture::ActiveLocalPrimary.label_zh().to_string(),
                status_label_en: CellNodePosture::ActiveLocalPrimary.label_en().to_string(),
                is_local_primary: true,
                reachable: true,
                lease_elapsed_ms: 12_000,
                lease_soft_watermark_ms: 45_000,
                lease_hard_ceiling_ms: 60_000,
                quarantined_fence_locked: false,
                l3_diagnostics: ClusterCellL3Diagnostics {
                    cell_id_hex: local_cell_id_hex,
                    lease_id_hex: deterministic_hex_id_16("lease", 1),
                    epoch: autonomy_epoch,
                    clock_skew_ms: 0,
                    last_heartbeat_mono_ms: 1_700_000_240_000,
                    fencing_token_hex: hex(&sha256_domain_digest(b"FENCE", &[b"local_primary"])),
                },
            },
            ClusterCellSummary {
                cell_id_hex: remote_worker_cell_id_hex.clone(),
                display_name_zh: "远端构建加速节点 A".to_string(),
                display_name_en: "Remote Build Accelerator A".to_string(),
                posture: CellNodePosture::ReachableRemoteWorker,
                status_label_zh: CellNodePosture::ReachableRemoteWorker
                    .label_zh()
                    .to_string(),
                status_label_en: CellNodePosture::ReachableRemoteWorker
                    .label_en()
                    .to_string(),
                is_local_primary: false,
                reachable: true,
                lease_elapsed_ms: 28_000,
                lease_soft_watermark_ms: 45_000,
                lease_hard_ceiling_ms: 60_000,
                quarantined_fence_locked: false,
                l3_diagnostics: ClusterCellL3Diagnostics {
                    cell_id_hex: remote_worker_cell_id_hex,
                    lease_id_hex: deterministic_hex_id_16("lease", 2),
                    epoch: autonomy_epoch,
                    clock_skew_ms: 14,
                    last_heartbeat_mono_ms: 1_700_000_238_000,
                    fencing_token_hex: hex(&sha256_domain_digest(b"FENCE", &[b"remote_worker_a"])),
                },
            },
            ClusterCellSummary {
                cell_id_hex: quarantined_cell_id_hex.clone(),
                display_name_zh: "受保护隔离归档节点 B".to_string(),
                display_name_en: "Isolated Archive Node B".to_string(),
                posture: CellNodePosture::QuarantinedFenced,
                status_label_zh: CellNodePosture::QuarantinedFenced.label_zh().to_string(),
                status_label_en: CellNodePosture::QuarantinedFenced.label_en().to_string(),
                is_local_primary: false,
                reachable: false,
                lease_elapsed_ms: 60_000,
                lease_soft_watermark_ms: 45_000,
                lease_hard_ceiling_ms: 60_000,
                quarantined_fence_locked: true,
                l3_diagnostics: ClusterCellL3Diagnostics {
                    cell_id_hex: quarantined_cell_id_hex,
                    lease_id_hex: deterministic_hex_id_16("lease", 3),
                    epoch: autonomy_epoch - 1,
                    clock_skew_ms: 480,
                    last_heartbeat_mono_ms: 1_700_000_120_000,
                    fencing_token_hex: hex(&sha256_domain_digest(b"FENCE", &[b"quarantined_b"])),
                },
            },
        ];

        Self {
            inner: Mutex::new(NotifySearchOfflineInner {
                notifications,
                ack_watermark_sequence: 10,
                prune_watermark_sequence: 0,
                retracted_event_count: 2,
                spotlight_candidates,
                authoritative_records,
                connectivity_mode: ClusterConnectivityMode::Connected,
                cluster_epoch: autonomy_epoch,
                local_autonomy_proof,
                cells,
                audit: NotifySearchOfflineAuditCounters::default(),
            }),
        }
    }

    /// 分页查询通知流 (`IPC-D8` `ListNotifications`)。
    ///
    /// # Errors
    ///
    /// 当参数不合法时返回 [`NotifySearchOfflineError`]。
    pub fn list_notification_feed(
        &self,
        request: ListNotificationFeedRequest,
    ) -> Result<NotificationFeedPage, NotifySearchOfflineError> {
        if request.limit == 0 {
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::InvalidArgument,
                "limit must be >= 1",
            ));
        }
        let clamped_limit = request.limit.min(MAX_NOTIFICATION_PAGE_LIMIT) as usize;
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let total_unread_count = guard
            .notifications
            .iter()
            .filter(|item| item.unread && !item.dismissed)
            .count() as u32;
        let total_active_count = guard
            .notifications
            .iter()
            .filter(|item| !item.dismissed)
            .count() as u32;

        let mut filtered: Vec<NotificationFeedItem> = guard
            .notifications
            .iter()
            .filter(|item| {
                if !request.include_dismissed && item.dismissed {
                    return false;
                }
                if request.unread_only && !item.unread {
                    return false;
                }
                if let Some(cat) = request.category_filter
                    && item.category != cat
                {
                    return false;
                }
                if let Some(cursor) = request.cursor_sequence
                    && item.sequence >= cursor
                {
                    return false;
                }
                true
            })
            .cloned()
            .collect();

        // 按序号降序排列（最新通知在前）
        filtered.sort_by_key(|a| std::cmp::Reverse(a.sequence));

        let has_more = filtered.len() > clamped_limit;
        if filtered.len() > clamped_limit {
            filtered.truncate(clamped_limit);
        }
        let next_cursor_sequence = if has_more {
            filtered.last().map(|last| last.sequence)
        } else {
            None
        };

        Ok(NotificationFeedPage {
            items: filtered,
            next_cursor_sequence,
            has_more,
            total_unread_count,
            total_active_count,
            ack_watermark_sequence: guard.ack_watermark_sequence,
            prune_watermark_sequence: guard.prune_watermark_sequence,
            retracted_event_count: guard.retracted_event_count,
        })
    }

    /// 检视单条通知详情 (`IPC-D8` `InspectNotification`)。
    ///
    /// # Errors
    ///
    /// 当 `notification_id_hex` 格式非法或不存在时返回 [`NotifySearchOfflineError`]。
    pub fn inspect_notification_item(
        &self,
        notification_id_hex: &str,
    ) -> Result<NotificationFeedItem, NotifySearchOfflineError> {
        let _ = parse_hex_id_16("notification_id_hex", notification_id_hex).map_err(|e| {
            NotifySearchOfflineError::new(NotifySearchOfflineErrorCode::InvalidArgument, e.message)
        })?;
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .notifications
            .iter()
            .find(|n| {
                n.notification_id_hex
                    .eq_ignore_ascii_case(notification_id_hex.trim())
            })
            .cloned()
            .ok_or_else(|| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::ActionTokenNotFound,
                    format!("notification {notification_id_hex} not found"),
                )
            })
    }

    /// 核销通知内联一次性 `ActionToken`（原子消费，拒绝重放、过期、跨应用/跨主体伪造）。
    ///
    /// # Errors
    ///
    /// 令牌已消费、过期、绑定字段不匹配或承诺摘要不匹配时 fail-closed 返回 [`NotifySearchOfflineError`]。
    pub fn redeem_notification_action_token(
        &self,
        request: RedeemNotificationActionTokenRequest,
    ) -> Result<RedeemNotificationActionTokenReceipt, NotifySearchOfflineError> {
        let _ =
            parse_hex_id_16("notification_id_hex", &request.notification_id_hex).map_err(|e| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::InvalidArgument,
                    e.message,
                )
            })?;
        let _ = parse_hex_id_16("token_id_hex", &request.token_id_hex).map_err(|e| {
            NotifySearchOfflineError::new(NotifySearchOfflineErrorCode::InvalidArgument, e.message)
        })?;
        let _ = parse_hex_id_16("caller_app_id_hex", &request.caller_app_id_hex).map_err(|e| {
            NotifySearchOfflineError::new(NotifySearchOfflineErrorCode::InvalidArgument, e.message)
        })?;
        let _ = parse_hex_id_16("caller_principal_id_hex", &request.caller_principal_id_hex)
            .map_err(|e| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::InvalidArgument,
                    e.message,
                )
            })?;
        let _ = parse_hex_id_16("task_id_hex", &request.task_id_hex).map_err(|e| {
            NotifySearchOfflineError::new(NotifySearchOfflineErrorCode::InvalidArgument, e.message)
        })?;

        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let item_idx = guard
            .notifications
            .iter()
            .position(|n| {
                n.notification_id_hex
                    .eq_ignore_ascii_case(request.notification_id_hex.trim())
            })
            .ok_or_else(|| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::ActionTokenNotFound,
                    format!("notification {} not found", request.notification_id_hex),
                )
            })?;

        let token_clone = guard.notifications[item_idx]
            .action_token
            .clone()
            .ok_or_else(|| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::ActionTokenNotFound,
                    format!(
                        "notification {} does not carry an inline ActionToken",
                        request.notification_id_hex
                    ),
                )
            })?;

        // 1. 校验 token_id 与 action_id 是否一致
        if !token_clone
            .token_id_hex
            .eq_ignore_ascii_case(request.token_id_hex.trim())
            || token_clone.action_id != request.action_id.trim()
        {
            guard.audit.action_token_invalid_proof_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::ActionTokenInvalidProof,
                "token_id_hex or action_id does not match bound ActionToken",
            ));
        }

        // 2. 校验是否已被消费（重放拦截）
        if token_clone.consumed {
            guard.audit.action_token_replay_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::ActionTokenAlreadyConsumed,
                format!(
                    "ActionToken {} has already been redeemed (one-shot replay rejected)",
                    token_clone.token_id_hex
                ),
            ));
        }

        // 3. 校验 ApplicationId 绑定
        if !token_clone
            .bound_app_id_hex
            .eq_ignore_ascii_case(request.caller_app_id_hex.trim())
        {
            guard.audit.action_token_app_mismatch_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::ActionTokenAppMismatch,
                format!(
                    "caller_app_id {} mismatches token bound_app_id {}",
                    request.caller_app_id_hex, token_clone.bound_app_id_hex
                ),
            ));
        }

        // 4. 校验 PrincipalId 绑定
        if !token_clone
            .bound_principal_id_hex
            .eq_ignore_ascii_case(request.caller_principal_id_hex.trim())
        {
            guard.audit.action_token_principal_mismatch_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::ActionTokenPrincipalMismatch,
                format!(
                    "caller_principal_id {} mismatches token bound_principal_id {}",
                    request.caller_principal_id_hex, token_clone.bound_principal_id_hex
                ),
            ));
        }

        // 5. 校验 TaskId 绑定
        if !token_clone
            .bound_task_id_hex
            .eq_ignore_ascii_case(request.task_id_hex.trim())
        {
            guard.audit.action_token_task_mismatch_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::ActionTokenTaskMismatch,
                format!(
                    "task_id {} mismatches token bound_task_id {}",
                    request.task_id_hex, token_clone.bound_task_id_hex
                ),
            ));
        }

        // 6. 校验有效期
        if request.now_mono_ms > token_clone.expiry_mono_ms {
            guard.audit.action_token_expired_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::ActionTokenExpired,
                format!(
                    "ActionToken {} expired at {} ms (now = {} ms)",
                    token_clone.token_id_hex, token_clone.expiry_mono_ms, request.now_mono_ms
                ),
            ));
        }

        // 7. 重算 SHA-256 密码学承诺并做常时间比对
        let recomputed_commitment = compute_action_token_commitment_hex(
            &token_clone.bound_notification_id_hex,
            &token_clone.token_id_hex,
            &token_clone.action_id,
            &token_clone.bound_app_id_hex,
            &token_clone.bound_task_id_hex,
            &token_clone.bound_principal_id_hex,
            token_clone.expiry_mono_ms,
        );
        if !constant_time_eq_str(&recomputed_commitment, &token_clone.verifier_commitment_hex) {
            guard.audit.action_token_invalid_proof_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::ActionTokenInvalidProof,
                "ActionToken cryptographic commitment verification failed",
            ));
        }
        if let Some(presented) = &request.presented_commitment_hex
            && !constant_time_eq_str(&recomputed_commitment, presented.trim())
        {
            guard.audit.action_token_invalid_proof_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::ActionTokenInvalidProof,
                "presented_commitment_hex does not match authoritative ActionToken commitment",
            ));
        }

        // 8. 原子核销并生成回执
        let receipt_digest = hex(&sha256_domain_digest(
            DOMAIN_ACTION_TOKEN_RECEIPT,
            &[
                token_clone.bound_notification_id_hex.as_bytes(),
                token_clone.token_id_hex.as_bytes(),
                token_clone.action_id.as_bytes(),
                &request.now_mono_ms.to_be_bytes(),
            ],
        ));

        let item = &mut guard.notifications[item_idx];
        item.unread = false;
        item.allowed_verbs = vec!["inspect".to_string(), "dismiss".to_string()];
        let resulting_state = match item.task_state {
            CanonicalTaskState::AwaitingApproval | CanonicalTaskState::WaitingHuman => {
                CanonicalTaskState::Running
            }
            CanonicalTaskState::Paused | CanonicalTaskState::Scheduled => {
                CanonicalTaskState::Running
            }
            CanonicalTaskState::Failed => CanonicalTaskState::Scheduled,
            other => other,
        };
        item.task_state = resulting_state;
        item.state_label_zh = resulting_state.label_zh().to_string();
        item.state_label_en = resulting_state.label_en().to_string();

        if let Some(tok) = &mut item.action_token {
            tok.consumed = true;
            tok.consumed_at_mono_ms = Some(request.now_mono_ms);
            tok.redemption_receipt_digest_hex = Some(receipt_digest.clone());
        }

        let remaining_verbs = item.allowed_verbs.clone();
        let notification_id_hex = item.notification_id_hex.clone();
        let task_id_hex = item.task_id_hex.clone();

        if let Some(auth_rec) = guard.authoritative_records.get_mut(&notification_id_hex) {
            auth_rec.authoritative_generation += 1;
            auth_rec.unlocked_verbs = remaining_verbs.clone();
        }

        guard.audit.action_token_redemptions_succeeded += 1;

        let updated_unread_count = guard
            .notifications
            .iter()
            .filter(|n| n.unread && !n.dismissed)
            .count() as u32;

        Ok(RedeemNotificationActionTokenReceipt {
            notification_id_hex,
            token_id_hex: token_clone.token_id_hex,
            action_id: token_clone.action_id,
            task_id_hex,
            resulting_task_state: resulting_state,
            redemption_receipt_digest_hex: receipt_digest,
            redeemed_at_mono_ms: request.now_mono_ms,
            remaining_allowed_verbs: remaining_verbs,
            updated_unread_count,
        })
    }

    /// 忽略/归档单条通知并推进已读水位线。
    ///
    /// # Errors
    ///
    /// 当通知不存在或 ID 格式非法时返回 [`NotifySearchOfflineError`]。
    pub fn dismiss_notification_item(
        &self,
        request: DismissNotificationItemRequest,
    ) -> Result<DismissNotificationItemReceipt, NotifySearchOfflineError> {
        let _ =
            parse_hex_id_16("notification_id_hex", &request.notification_id_hex).map_err(|e| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::InvalidArgument,
                    e.message,
                )
            })?;
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let item_idx = guard
            .notifications
            .iter()
            .position(|n| {
                n.notification_id_hex
                    .eq_ignore_ascii_case(request.notification_id_hex.trim())
            })
            .ok_or_else(|| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::ActionTokenNotFound,
                    format!("notification {} not found", request.notification_id_hex),
                )
            })?;

        let seq = guard.notifications[item_idx].sequence;
        let norm_id = guard.notifications[item_idx].notification_id_hex.clone();
        guard.notifications[item_idx].unread = false;
        guard.notifications[item_idx].dismissed = true;
        if seq > guard.ack_watermark_sequence {
            guard.ack_watermark_sequence = seq;
        }
        let ack_watermark_sequence = guard.ack_watermark_sequence;
        let updated_unread_count = guard
            .notifications
            .iter()
            .filter(|n| n.unread && !n.dismissed)
            .count() as u32;

        Ok(DismissNotificationItemReceipt {
            notification_id_hex: norm_id,
            dismissed: true,
            ack_watermark_sequence,
            updated_unread_count,
        })
    }

    /// 执行 `Ctrl+K` 全局搜索候选查询 (`IPC-D4` `QueryIndex` + `InspectSearchScope`)。
    ///
    /// 法定锁 #1：所有返回的候选与响应信封均强制打上 `NON_AUTHORITATIVE` 与 `0x1F` 只读位掩码，
    /// `allowed_verbs` 严格收敛为 `["inspect", "jump_to_authority"]`。
    ///
    /// # Errors
    ///
    /// 当 `limit == 0` 时返回 [`NotifySearchOfflineError`]。
    pub fn query_spotlight_candidates(
        &self,
        request: QuerySpotlightCandidatesRequest,
    ) -> Result<SpotlightQueryResponse, NotifySearchOfflineError> {
        if request.limit == 0 {
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::InvalidArgument,
                "Spotlight query limit must be >= 1",
            ));
        }
        let clamped_limit = request.limit.min(MAX_SPOTLIGHT_CANDIDATE_LIMIT) as usize;
        let normalized_query = request.query_text.trim().to_lowercase();

        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let shard_statuses: Vec<SpotlightShardStatus> = guard
            .cells
            .iter()
            .map(|cell| SpotlightShardStatus {
                cell_id_hex: cell.cell_id_hex.clone(),
                node_label_zh: cell.display_name_zh.clone(),
                node_label_en: cell.display_name_en.clone(),
                is_local_primary: cell.is_local_primary,
                reachable: cell.reachable && !cell.quarantined_fence_locked,
                checkpoint_generation: cell.l3_diagnostics.epoch,
            })
            .collect();

        let reachable_cell_ids: Vec<&str> = shard_statuses
            .iter()
            .filter(|s| s.reachable)
            .map(|s| s.cell_id_hex.as_str())
            .collect();

        let has_degraded_shards = shard_statuses.iter().any(|s| !s.reachable);

        let mut candidates: Vec<SpotlightCandidateItem> = guard
            .spotlight_candidates
            .iter()
            .filter(|cand| {
                // 只返回可达分片的索引快照（本机分片始终可达）
                if !reachable_cell_ids
                    .iter()
                    .any(|id| id.eq_ignore_ascii_case(&cand.l3_diagnostics.source_cell_id_hex))
                {
                    return false;
                }
                if let Some(cat) = request.category_filter
                    && cand.category != cat
                {
                    return false;
                }
                if !normalized_query.is_empty() {
                    let hay_zh = format!("{} {}", cand.title_zh, cand.snippet_zh).to_lowercase();
                    let hay_en = format!("{} {}", cand.title_en, cand.snippet_en).to_lowercase();
                    if !hay_zh.contains(&normalized_query) && !hay_en.contains(&normalized_query) {
                        return false;
                    }
                }
                true
            })
            .map(|cand| {
                let mut enforced = cand.clone();
                // 硬件级强制覆写 `0x1F` 非权威只读锁
                enforced.authority_grade = "NON_AUTHORITATIVE".to_string();
                enforced.restrictions_bitmask = SPOTLIGHT_STATUTORY_MASK_0X1F;
                enforced.allowed_verbs =
                    vec!["inspect".to_string(), "jump_to_authority".to_string()];
                enforced.l3_diagnostics.authority_tag = "NON_AUTHORITATIVE · mask=0x1F".to_string();
                enforced.l3_diagnostics.restrictions_bitmask_hex = "0x1F".to_string();
                enforced
            })
            .collect();

        candidates.truncate(clamped_limit);

        let (scope_notice_zh, scope_notice_en, l3_consistency_level) = if has_degraded_shards {
            (
                "部分远端或隔离节点未参与检索，当前显示本机与在线节点候选（点击候选将核验最新状态）。"
                    .to_string(),
                "Some remote or isolated nodes are excluded; showing local and online candidates (click to verify latest status)."
                    .to_string(),
                "BOUNDED_STALENESS_LOCAL_AUTONOMY".to_string(),
            )
        } else {
            (
                "本机与协同节点索引已同步（搜索结果仅供预览，点击后将跳转并核验最新状态）。"
                    .to_string(),
                "Local and federated index synced (search hits are preview-only; clicking jumps and verifies latest status)."
                    .to_string(),
                "FEDERATED_CHECKPOINT_SYNCED".to_string(),
            )
        };

        let l3_checkpoint_digest_hex = guard.spotlight_candidates.first().map_or_else(
            || "0".repeat(64),
            |c| c.l3_diagnostics.checkpoint_digest_hex.clone(),
        );

        Ok(SpotlightQueryResponse {
            normalized_query,
            candidates,
            envelope_authority_grade: "NON_AUTHORITATIVE".to_string(),
            envelope_restrictions_bitmask: SPOTLIGHT_STATUTORY_MASK_0X1F,
            has_degraded_shards,
            scope_notice_zh,
            scope_notice_en,
            shard_statuses,
            l3_consistency_level,
            l3_checkpoint_digest_hex,
            l3_retracted_filtered_count: guard.retracted_event_count,
        })
    }

    /// 法定锁 #1 (`0x1F`) 强门禁：任何试图从搜索命中直接触发控制面写操作的调用一律 fail-closed 拒绝。
    ///
    /// 即使调用方伪造 `caller_supplied_mask = Some(0x00)`，后端也无条件强制执行 `0x1F` 掩码并记录审计计数。
    ///
    /// # Errors
    ///
    /// 恒返回 `NotifySearchOfflineErrorCode::SearchResultNonAuthoritative0x1f`。
    pub fn attempt_mutation_from_search_hit(
        &self,
        request: AttemptMutationFromSearchHitRequest,
    ) -> Result<(), NotifySearchOfflineError> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.audit.search_0x1f_mutation_rejections += 1;
        match request.attempted_gate {
            SearchForbiddenControlGate::CapabilityGrant => {
                guard.audit.search_forbid_capability_grant_hits += 1;
            }
            SearchForbiddenControlGate::ResourceLedgerDeduction => {
                guard.audit.search_forbid_ledger_deduction_hits += 1;
            }
            SearchForbiddenControlGate::TaskCommit => {
                guard.audit.search_forbid_task_commit_hits += 1;
            }
            SearchForbiddenControlGate::ApplicationUpdate => {
                guard.audit.search_forbid_app_update_hits += 1;
            }
            SearchForbiddenControlGate::IrreversibleAction => {
                guard.audit.search_forbid_irreversible_action_hits += 1;
            }
        }

        Err(NotifySearchOfflineError::new(
            NotifySearchOfflineErrorCode::SearchResultNonAuthoritative0x1f,
            format!(
                "Statutory Lock #1 violation: search candidate '{}' carries NON_AUTHORITATIVE 0x1F lock (gate={} mask_bit=0x{:02X}, caller_supplied_mask={:?}); direct mutation is forbidden — jump to authoritative store first",
                request.candidate_id_hex,
                request.attempted_gate.as_str(),
                request.attempted_gate.mask_bit(),
                request.caller_supplied_mask
            ),
        ))
    }

    /// 点击 `Ctrl+K` 搜索候选后跳转至权威视图前的权威重核验 (`verify_spotlight_candidate_authority`)。
    ///
    /// # Errors
    ///
    /// 候选不存在、目标权威记录已被撤回 (`AUTHORITY_RECORD_RETRACTED`) 或快照代数落后于权威存储
    /// (`STALE_GENERATION`) 时 fail-closed 返回 [`NotifySearchOfflineError`]。
    pub fn verify_spotlight_candidate_authority(
        &self,
        request: VerifySpotlightCandidateAuthorityRequest,
    ) -> Result<SpotlightAuthorityVerificationReceipt, NotifySearchOfflineError> {
        let _ = parse_hex_id_16("candidate_id_hex", &request.candidate_id_hex).map_err(|e| {
            NotifySearchOfflineError::new(NotifySearchOfflineErrorCode::InvalidArgument, e.message)
        })?;
        let _ = parse_hex_id_16("expected_target_id_hex", &request.expected_target_id_hex)
            .map_err(|e| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::InvalidArgument,
                    e.message,
                )
            })?;

        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let candidate = guard
            .spotlight_candidates
            .iter()
            .find(|c| {
                c.candidate_id_hex
                    .eq_ignore_ascii_case(request.candidate_id_hex.trim())
            })
            .cloned()
            .ok_or_else(|| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::SearchCandidateNotFound,
                    format!("Spotlight candidate {} not found", request.candidate_id_hex),
                )
            })?;

        if !candidate
            .target_id_hex
            .eq_ignore_ascii_case(request.expected_target_id_hex.trim())
        {
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::InvalidArgument,
                format!(
                    "expected_target_id_hex {} does not match candidate target_id_hex {}",
                    request.expected_target_id_hex, candidate.target_id_hex
                ),
            ));
        }

        let auth_record = guard
            .authoritative_records
            .get(&candidate.target_id_hex)
            .cloned()
            .ok_or_else(|| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::SearchCandidateNotFound,
                    format!(
                        "authoritative object {} not found in store",
                        candidate.target_id_hex
                    ),
                )
            })?;

        if auth_record.retracted {
            guard.audit.search_authority_retracted_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::AuthorityRecordRetracted,
                format!(
                    "authoritative record {} has been retracted since search index snapshot",
                    candidate.target_id_hex
                ),
            ));
        }

        if request.snapshot_generation < auth_record.authoritative_generation {
            guard.audit.search_stale_generation_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::StaleGeneration,
                format!(
                    "search candidate snapshot_generation {} is older than authoritative_generation {}",
                    request.snapshot_generation, auth_record.authoritative_generation
                ),
            ));
        }

        guard.audit.search_authority_verifications_succeeded += 1;

        let verification_digest_hex = hex(&sha256_domain_digest(
            DOMAIN_SEARCH_VERIFY_RECEIPT,
            &[
                candidate.candidate_id_hex.as_bytes(),
                candidate.target_id_hex.as_bytes(),
                auth_record.category.authoritative_store_kind().as_bytes(),
                &auth_record.authoritative_generation.to_be_bytes(),
            ],
        ));

        Ok(SpotlightAuthorityVerificationReceipt {
            candidate_id_hex: candidate.candidate_id_hex,
            target_id_hex: candidate.target_id_hex,
            category: candidate.category,
            verified: true,
            authoritative_store_kind: candidate.category.authoritative_store_kind().to_string(),
            target_view_id: candidate.category.target_view_id().to_string(),
            authoritative_generation: auth_record.authoritative_generation,
            verification_digest_hex,
            unlocked_authoritative_verbs: auth_record.unlocked_verbs,
            message_zh: "已通过权威存储核验，正在打开最新目标视图。".to_string(),
            message_en: "Verified against authoritative store; opening latest target view."
                .to_string(),
        })
    }

    /// 检视离线自治与集群态势 (`IPC-D7` `ListCells` + `InspectCellLeases`)。
    #[must_use]
    pub fn inspect_offline_cluster_posture(&self) -> OfflineClusterPostureSnapshot {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::build_posture_snapshot(&guard)
    }

    fn build_posture_snapshot(guard: &NotifySearchOfflineInner) -> OfflineClusterPostureSnapshot {
        let active_lease_count = guard
            .cells
            .iter()
            .filter(|c| c.reachable && !c.quarantined_fence_locked)
            .count() as u32;
        let quarantined_cell_count = guard
            .cells
            .iter()
            .filter(|c| c.quarantined_fence_locked)
            .count() as u32;

        OfflineClusterPostureSnapshot {
            connectivity_mode: guard.connectivity_mode,
            local_autonomy_active: true,
            local_autonomy_proof: guard.local_autonomy_proof.clone(),
            badge_label_zh: guard.connectivity_mode.badge_label_zh().to_string(),
            badge_label_en: guard.connectivity_mode.badge_label_en().to_string(),
            status_copy_zh: guard.connectivity_mode.summary_copy_zh().to_string(),
            status_copy_en: guard.connectivity_mode.summary_copy_en().to_string(),
            cells: guard.cells.clone(),
            l3_diagnostics: ClusterL3Diagnostics {
                partition_autonomy_gate_state: guard.connectivity_mode.as_str().to_string(),
                cluster_epoch: guard.cluster_epoch,
                active_lease_count,
                quarantined_cell_count,
                offline_remote_action_blocks: guard.audit.offline_remote_action_blocks,
                quarantined_ttl_unlock_rejections: guard
                    .audit
                    .quarantined_lease_ttl_unlock_rejections,
            },
        }
    }

    /// 切换集群连接与离线自治模式（用于离线自治演练、测试与托盘态势刷新）。
    ///
    /// # Errors
    ///
    /// 参数非法时返回 [`NotifySearchOfflineError`]。
    pub fn set_cluster_connectivity_mode(
        &self,
        request: SetClusterConnectivityModeRequest,
    ) -> Result<OfflineClusterPostureSnapshot, NotifySearchOfflineError> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.connectivity_mode = request.mode;
        guard.cluster_epoch += 1;
        let new_epoch = guard.cluster_epoch;

        for cell in &mut guard.cells {
            if cell.is_local_primary {
                cell.posture = CellNodePosture::ActiveLocalPrimary;
                cell.reachable = true;
                cell.status_label_zh = CellNodePosture::ActiveLocalPrimary.label_zh().to_string();
                cell.status_label_en = CellNodePosture::ActiveLocalPrimary.label_en().to_string();
                cell.l3_diagnostics.epoch = new_epoch;
                cell.l3_diagnostics.last_heartbeat_mono_ms = request.now_mono_ms;
            } else if cell.quarantined_fence_locked {
                // 法定锁 #3：QUARANTINED 节点即使切换网络模式也保持硬件隔离围栏锁定
                cell.posture = CellNodePosture::QuarantinedFenced;
                cell.reachable = false;
                cell.status_label_zh = CellNodePosture::QuarantinedFenced.label_zh().to_string();
                cell.status_label_en = CellNodePosture::QuarantinedFenced.label_en().to_string();
            } else {
                match request.mode {
                    ClusterConnectivityMode::Connected => {
                        cell.posture = CellNodePosture::ReachableRemoteWorker;
                        cell.reachable = true;
                        cell.status_label_zh = CellNodePosture::ReachableRemoteWorker
                            .label_zh()
                            .to_string();
                        cell.status_label_en = CellNodePosture::ReachableRemoteWorker
                            .label_en()
                            .to_string();
                        cell.l3_diagnostics.epoch = new_epoch;
                        cell.l3_diagnostics.last_heartbeat_mono_ms = request.now_mono_ms;
                    }
                    ClusterConnectivityMode::Partitioned
                    | ClusterConnectivityMode::Reconnecting
                    | ClusterConnectivityMode::LocalSingleNode => {
                        cell.posture = CellNodePosture::PartitionedRemoteWorker;
                        cell.reachable = false;
                        cell.status_label_zh = CellNodePosture::PartitionedRemoteWorker
                            .label_zh()
                            .to_string();
                        cell.status_label_en = CellNodePosture::PartitionedRemoteWorker
                            .label_en()
                            .to_string();
                    }
                }
            }
        }

        Ok(Self::build_posture_snapshot(&guard))
    }

    /// 评估离线/分区态势下工作台某项动作的可用性 (`[DUI-OFFLINE-001]`)。
    ///
    /// - 本机 6 大核心功能面（Launcher、本地制品、任务浏览、设置、本机搜索、通知历史）在无网或
    ///   `LocalSingleNode` / `Partitioned` 下恒为 100% 可交互，并附带 `LocalAutonomyProof`。
    /// - 目标指向 `QUARANTINED` 隔离围栏节点时，强制返回 `BlockedQuarantinedFence`。
    /// - 强依赖远端不可达 `Provider` 或离线远端节点时，确定性返回 `WaitingNetwork` / `UnavailableRemoteOffline`。
    ///
    /// # Errors
    ///
    /// 当 `target_cell_id_hex` 格式非法时返回 [`NotifySearchOfflineError`]。
    pub fn evaluate_offline_action_availability(
        &self,
        request: EvaluateOfflineActionAvailabilityRequest,
    ) -> Result<OfflineActionAvailabilityDecision, NotifySearchOfflineError> {
        if let Some(cell_hex) = &request.target_cell_id_hex {
            let _ = parse_hex_id_16("target_cell_id_hex", cell_hex).map_err(|e| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::InvalidArgument,
                    e.message,
                )
            })?;
        }

        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // 1. 若指定了 target_cell_id_hex，先检查该节点是否处于 QUARANTINED 围栏锁定或离线
        if let Some(target_hex) = &request.target_cell_id_hex
            && let Some(cell) = guard
                .cells
                .iter()
                .find(|c| c.cell_id_hex.eq_ignore_ascii_case(target_hex.trim()))
                .cloned()
        {
            if cell.quarantined_fence_locked {
                guard.audit.offline_remote_action_blocks += 1;
                return Ok(OfflineActionAvailabilityDecision {
                    surface: request.surface,
                    action_verb: request.action_verb,
                    status: OfflineActionAvailabilityStatus::BlockedQuarantinedFence,
                    interactive: false,
                    autonomy_proof: None,
                    statutory_lock_kind: Some("QUARANTINED_LEASE_FENCE".to_string()),
                    reason_zh: "目标节点处于安全隔离保护态，为防止数据冲突已暂停向该节点派发操作。"
                        .to_string(),
                    reason_en: "Target node is under safety isolation; operations to this node are paused to prevent data conflict."
                        .to_string(),
                    l3_diagnostic_tag: format!(
                        "QUARANTINED_LEASE_FENCE · cell={} · epoch={} · lease={}",
                        cell.cell_id_hex,
                        cell.l3_diagnostics.epoch,
                        cell.l3_diagnostics.lease_id_hex
                    ),
                });
            }

            if !cell.is_local_primary && !cell.reachable {
                guard.audit.offline_remote_action_blocks += 1;
                let status = if guard.connectivity_mode == ClusterConnectivityMode::LocalSingleNode
                {
                    OfflineActionAvailabilityStatus::UnavailableRemoteOffline
                } else {
                    OfflineActionAvailabilityStatus::WaitingNetwork
                };
                return Ok(OfflineActionAvailabilityDecision {
                    surface: request.surface,
                    action_verb: request.action_verb,
                    status,
                    interactive: false,
                    autonomy_proof: None,
                    statutory_lock_kind: None,
                    reason_zh: "此操作需要连接远端协同节点（当前离线，等待网络恢复后可继续）。"
                        .to_string(),
                    reason_en: "This action requires a remote node connection (currently offline; will resume when network returns)."
                        .to_string(),
                    l3_diagnostic_tag: format!(
                        "REMOTE_CELL_UNREACHABLE · mode={} · cell={} · epoch={}",
                        guard.connectivity_mode.as_str(),
                        cell.cell_id_hex,
                        cell.l3_diagnostics.epoch
                    ),
                });
            }
        }

        // 2. 检查是否强依赖远端 Provider 或属于跨节点派发面
        if request.requires_remote_provider
            || request.surface == OfflineSurfaceKind::RemoteClusterDispatch
        {
            if guard.connectivity_mode != ClusterConnectivityMode::Connected {
                guard.audit.offline_remote_action_blocks += 1;
                let status = if guard.connectivity_mode == ClusterConnectivityMode::LocalSingleNode
                {
                    OfflineActionAvailabilityStatus::UnavailableRemoteOffline
                } else {
                    OfflineActionAvailabilityStatus::WaitingNetwork
                };
                return Ok(OfflineActionAvailabilityDecision {
                    surface: request.surface,
                    action_verb: request.action_verb,
                    status,
                    interactive: false,
                    autonomy_proof: None,
                    statutory_lock_kind: None,
                    reason_zh: "此操作需要连接云端模型或远端服务（当前处于本机离线模式，本机功能不受影响）。"
                        .to_string(),
                    reason_en: "This action requires a remote model or cloud service (currently in local offline mode; local workstation features remain unaffected)."
                        .to_string(),
                    l3_diagnostic_tag: format!(
                        "REMOTE_PROVIDER_OFFLINE · gate={} · cluster_epoch={}",
                        guard.connectivity_mode.as_str(),
                        guard.cluster_epoch
                    ),
                });
            }
            return Ok(OfflineActionAvailabilityDecision {
                surface: request.surface,
                action_verb: request.action_verb,
                status: OfflineActionAvailabilityStatus::AvailableOnline,
                interactive: true,
                autonomy_proof: Some(guard.local_autonomy_proof.clone()),
                statutory_lock_kind: None,
                reason_zh: "远端协同服务在线可用。".to_string(),
                reason_en: "Remote collaborative service is online and available.".to_string(),
                l3_diagnostic_tag: format!(
                    "ONLINE_CONNECTED · cluster_epoch={}",
                    guard.cluster_epoch
                ),
            });
        }

        // 3. 本机 6 大核心功能面：在任何网络模式（含 Partitioned 与 LocalSingleNode）下 100% 可交互
        let status = if guard.connectivity_mode == ClusterConnectivityMode::Connected {
            OfflineActionAvailabilityStatus::AvailableOnline
        } else {
            OfflineActionAvailabilityStatus::AvailableLocalAutonomy
        };

        Ok(OfflineActionAvailabilityDecision {
            surface: request.surface,
            action_verb: request.action_verb,
            status,
            interactive: true,
            autonomy_proof: Some(guard.local_autonomy_proof.clone()),
            statutory_lock_kind: None,
            reason_zh: "本机自治保障：无需外部网络即可完整使用此功能。".to_string(),
            reason_en:
                "Local autonomy guaranteed: full functionality available without external network."
                    .to_string(),
            l3_diagnostic_tag: format!(
                "LOCAL_AUTONOMY_PROOF · proof={} · cell={} · epoch={}",
                guard.local_autonomy_proof.proof_id_hex,
                guard.local_autonomy_proof.local_cell_id_hex,
                guard.local_autonomy_proof.autonomy_epoch
            ),
        })
    }

    /// 法定锁 #3 (`QUARANTINED` 租约围栏)：拦截仅凭本地 TTL 到期试图解锁隔离节点的操作。
    ///
    /// # Errors
    ///
    /// 当目标节点处于 `QUARANTINED` 围栏锁定态时，恒返回 `QuarantinedLeaseFenceLocked`。
    pub fn attempt_unlock_quarantined_lease_by_ttl(
        &self,
        cell_id_hex: &str,
        elapsed_local_ttl_ms: u64,
    ) -> Result<(), NotifySearchOfflineError> {
        let _ = parse_hex_id_16("cell_id_hex", cell_id_hex).map_err(|e| {
            NotifySearchOfflineError::new(NotifySearchOfflineErrorCode::InvalidArgument, e.message)
        })?;
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let cell = guard
            .cells
            .iter()
            .find(|c| c.cell_id_hex.eq_ignore_ascii_case(cell_id_hex.trim()))
            .cloned()
            .ok_or_else(|| {
                NotifySearchOfflineError::new(
                    NotifySearchOfflineErrorCode::InvalidArgument,
                    format!("cell {cell_id_hex} not found"),
                )
            })?;

        if cell.quarantined_fence_locked {
            guard.audit.quarantined_lease_ttl_unlock_rejections += 1;
            return Err(NotifySearchOfflineError::new(
                NotifySearchOfflineErrorCode::QuarantinedLeaseFenceLocked,
                format!(
                    "Statutory Lock #3 violation: cell {} is fenced in QUARANTINED state (epoch={}, lease={}); local TTL expiration ({} ms >= ceiling {} ms) cannot unlock fence without authoritative epoch progression",
                    cell.cell_id_hex,
                    cell.l3_diagnostics.epoch,
                    cell.l3_diagnostics.lease_id_hex,
                    elapsed_local_ttl_ms,
                    cell.lease_hard_ceiling_ms
                ),
            ));
        }

        Ok(())
    }

    /// 测试与治理辅助方法：将指定权威对象标记为已撤回 (`retracted = true`)，用于验证搜索跳转重核验拦截。
    pub fn retract_authority_record(&self, target_id_hex: &str) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rec) = guard.authoritative_records.get_mut(target_id_hex.trim()) {
            rec.retracted = true;
            guard.retracted_event_count += 1;
            true
        } else {
            false
        }
    }

    /// 测试与治理辅助方法：推进指定权威对象的 `authoritative_generation`，用于验证过期搜索快照拦截。
    pub fn advance_authority_generation(&self, target_id_hex: &str) -> Option<u64> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rec) = guard.authoritative_records.get_mut(target_id_hex.trim()) {
            rec.authoritative_generation += 1;
            Some(rec.authoritative_generation)
        } else {
            None
        }
    }

    /// 读取安全审计与法定锁拦截计数器快照。
    #[must_use]
    pub fn get_security_audit(&self) -> NotifySearchOfflineAuditCounters {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.audit.clone()
    }
}

// ============================================================================
// Tauri Command 包装层（供 `W72-INT` 在 `lib.rs` 注册 `invoke_handler`）
// ============================================================================

/// Tauri 命令：分页查询通知中心事件流 (`IPC-D8`)。
#[tauri::command]
pub fn list_notification_feed(
    state: tauri::State<'_, NotifySearchOfflineState>,
    request: ListNotificationFeedRequest,
) -> Result<NotificationFeedPage, DesktopError> {
    state
        .list_notification_feed(request)
        .map_err(NotifySearchOfflineError::into_desktop_error)
}

/// Tauri 命令：核销通知内联一次性 `ActionToken` (`IPC-D8`)。
#[tauri::command]
pub fn redeem_notification_action_token(
    state: tauri::State<'_, NotifySearchOfflineState>,
    request: RedeemNotificationActionTokenRequest,
) -> Result<RedeemNotificationActionTokenReceipt, DesktopError> {
    state
        .redeem_notification_action_token(request)
        .map_err(NotifySearchOfflineError::into_desktop_error)
}

/// Tauri 命令：忽略/归档单条通知项 (`IPC-D8`)。
#[tauri::command]
pub fn dismiss_notification_item(
    state: tauri::State<'_, NotifySearchOfflineState>,
    request: DismissNotificationItemRequest,
) -> Result<DismissNotificationItemReceipt, DesktopError> {
    state
        .dismiss_notification_item(request)
        .map_err(NotifySearchOfflineError::into_desktop_error)
}

/// Tauri 命令：执行 `Ctrl+K` 全局搜索候选查询 (`IPC-D4` + `IPC-D8`)。
#[tauri::command]
pub fn query_spotlight_candidates(
    state: tauri::State<'_, NotifySearchOfflineState>,
    request: QuerySpotlightCandidatesRequest,
) -> Result<SpotlightQueryResponse, DesktopError> {
    state
        .query_spotlight_candidates(request)
        .map_err(NotifySearchOfflineError::into_desktop_error)
}

/// Tauri 命令：点击搜索候选跳转前向权威存储重核验 (`IPC-D4`)。
#[tauri::command]
pub fn verify_spotlight_candidate_authority(
    state: tauri::State<'_, NotifySearchOfflineState>,
    request: VerifySpotlightCandidateAuthorityRequest,
) -> Result<SpotlightAuthorityVerificationReceipt, DesktopError> {
    state
        .verify_spotlight_candidate_authority(request)
        .map_err(NotifySearchOfflineError::into_desktop_error)
}

/// Tauri 命令：法定锁 #1 (`0x1F`) 搜索结果直接写操作拦截器。
#[tauri::command]
pub fn attempt_mutation_from_search_hit(
    state: tauri::State<'_, NotifySearchOfflineState>,
    request: AttemptMutationFromSearchHitRequest,
) -> Result<(), DesktopError> {
    state
        .attempt_mutation_from_search_hit(request)
        .map_err(NotifySearchOfflineError::into_desktop_error)
}

/// Tauri 命令：检视离线自治与集群节点租约态势 (`IPC-D7`)。
#[tauri::command]
pub fn inspect_offline_cluster_posture(
    state: tauri::State<'_, NotifySearchOfflineState>,
) -> Result<OfflineClusterPostureSnapshot, DesktopError> {
    Ok(state.inspect_offline_cluster_posture())
}

/// Tauri 命令：评估离线或分区态势下某项动作的可用性 (`[DUI-OFFLINE-001]`)。
#[tauri::command]
pub fn evaluate_offline_action_availability(
    state: tauri::State<'_, NotifySearchOfflineState>,
    request: EvaluateOfflineActionAvailabilityRequest,
) -> Result<OfflineActionAvailabilityDecision, DesktopError> {
    state
        .evaluate_offline_action_availability(request)
        .map_err(NotifySearchOfflineError::into_desktop_error)
}

/// Tauri 命令：切换集群连接与离线自治模式 (`IPC-D7`)。
#[tauri::command]
pub fn set_cluster_connectivity_mode(
    state: tauri::State<'_, NotifySearchOfflineState>,
    request: SetClusterConnectivityModeRequest,
) -> Result<OfflineClusterPostureSnapshot, DesktopError> {
    state
        .set_cluster_connectivity_mode(request)
        .map_err(NotifySearchOfflineError::into_desktop_error)
}
