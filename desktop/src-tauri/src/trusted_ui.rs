//! Stage D (`D-TRUSTED-UI` / `W71-D-TRUSTED-UI`) 可信系统安全屏 (`Z4`)、受限能力选择器 (`ChooserGrant`)
//! 与污点感知跨应用剪贴板/拖放守卫 (`ClipboardAndDragBroker`)。
//!
//! 权威契约对齐：
//! - `06-架构设计总纲-v0.5.md` §24.1–§24.3、§25.1–§25.4 (`[TUI-CONSENT-001]`, `[TUI-SEAL-001]`,
//!   `[TUI-GUARD-001]`, `[TUI-CHOOSER-001]`, `[DUI-CHOOSER-001]`, `[DUI-CLIP-001]`, `[RCPT-UNDO-001]`,
//!   `[RCPT-PARTIAL-001]`, `[CTRL-HIER-001]`)；
//! - `ADR-0025` 决定 1（五平面 Z 序、`Z4 = 900` 不可遮挡安全屏、`350ms` 防点击劫持武装门、私密安全印章隔离）、
//!   决定 3（零毛玻璃实心遮罩 `rgba(10, 11, 13, 0.78)`、`6px` 实心对话框、`2px` 顶部钢蓝安全条、`32×20` 凹陷硬件核验印章）、
//!   决定 4（`IRREVERSIBLE` 法定不可篡改锁与爆炸半径预览）；
//! - `2026-10-26-stage-d-desktop-visual-and-ux-design.md` §1.3、§2A.1、§2A.3、§3.5。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use crate::compositor::{WorkstationHostShell, Z2_WINDOW_Z_MAX, Z4_SECURITY_SCREEN_Z};
use crate::dto::hex;
use crate::error::{DesktopError, ErrorCode};
use crate::input_router::{InputOrigin, InputSource, Z4_ARMING_WINDOW_MS};
use crate::session::{DesktopSessionManager, DesktopSessionState};

/// `Z4` 系统安全确认屏固定 Z-index（全屏不可遮挡顶层）。
pub const Z4_SECURITY_Z_INDEX: u16 = Z4_SECURITY_SCREEN_Z;
/// 不可信 `Z2` 应用窗口允许声明的最高 Z-index 上限。
pub const Z2_SURFACE_Z_CEILING: u16 = Z2_WINDOW_Z_MAX;
/// `Z4` 实心遮罩颜色（严禁任何模糊滤镜）。
pub const Z4_SCRIM_RGBA: &str = "rgba(10, 11, 13, 0.78)";
/// `Z4` 安全对话框圆角（像素）。
pub const Z4_DIALOG_RADIUS_PX: u8 = 6;
/// `Z4` 顶部钢蓝安全横条高度（像素）。
pub const Z4_TOP_SECURITY_BAR_HEIGHT_PX: u8 = 2;
/// 凹陷硬件核验印章宽度（像素）。
pub const Z4_SEAL_WIDTH_PX: u8 = 32;
/// 凹陷硬件核验印章高度（像素）。
pub const Z4_SEAL_HEIGHT_PX: u8 = 20;
/// `Z4` 呈现与焦点重获后的强制防点击劫持武装时间窗（毫秒）。
pub const Z4_CLICKJACK_ARMING_MS: u64 = Z4_ARMING_WINDOW_MS;
/// `IRREVERSIBLE` 动作高对比警示红（双极高信号色之一）。
pub const Z4_IRREVERSIBLE_CRIMSON_HEX: &str = "#DC2626";

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 自包含零外部依赖 FIPS 180-4 SHA-256 摘要实现。
#[must_use]
pub fn sha256_bytes(input: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(input.len() + 72);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word_bytes) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word_bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut out = [0u8; 32];
    for (idx, word) in state.iter().enumerate() {
        out[idx * 4..(idx + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// 长度前缀域分离 SHA-256 摘要，消除字段拼接歧义。
#[must_use]
pub fn sha256_domain_digest(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let total_len = 8
        + domain.len()
        + parts
            .iter()
            .fold(0usize, |acc, part| acc.saturating_add(8 + part.len()));
    let mut buf = Vec::with_capacity(total_len);
    buf.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    buf.extend_from_slice(domain);
    for part in parts {
        buf.extend_from_slice(&(part.len() as u64).to_be_bytes());
        buf.extend_from_slice(part);
    }
    sha256_bytes(&buf)
}

#[must_use]
fn derive_id_16(domain: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let digest = sha256_domain_digest(domain, parts);
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    id
}

/// 解析 32 字符十六进制为 16 字节定长标识。
///
/// # Errors
///
/// 长度不为 32 或包含非十六进制字符时返回 [`TrustedUiError`]。
pub fn parse_hex_id_16(field: &str, raw: &str) -> Result<[u8; 16], TrustedUiError> {
    let trimmed = raw.trim();
    if trimmed.len() != 32 {
        return Err(TrustedUiError::new(
            TrustedUiErrorCode::InvalidArgument,
            format!("{field} must be 32 hex chars, got length {}", trimmed.len()),
        ));
    }
    let mut out = [0u8; 16];
    for (idx, pair) in trimmed.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = decode_nibble(pair[0]).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!("{field} contains invalid hex byte at offset {}", idx * 2),
            )
        })?;
        let lo = decode_nibble(pair[1]).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!(
                    "{field} contains invalid hex byte at offset {}",
                    idx * 2 + 1
                ),
            )
        })?;
        out[idx] = (hi << 4) | lo;
    }
    Ok(out)
}

/// 解析 64 字符十六进制为 32 字节摘要。
///
/// # Errors
///
/// 长度不为 64 或包含非十六进制字符时返回 [`TrustedUiError`]。
pub fn parse_hex_digest_32(field: &str, raw: &str) -> Result<[u8; 32], TrustedUiError> {
    let trimmed = raw.trim();
    if trimmed.len() != 64 {
        return Err(TrustedUiError::new(
            TrustedUiErrorCode::InvalidArgument,
            format!("{field} must be 64 hex chars, got length {}", trimmed.len()),
        ));
    }
    let mut out = [0u8; 32];
    for (idx, pair) in trimmed.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = decode_nibble(pair[0]).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!("{field} contains invalid hex byte at offset {}", idx * 2),
            )
        })?;
        let lo = decode_nibble(pair[1]).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!(
                    "{field} contains invalid hex byte at offset {}",
                    idx * 2 + 1
                ),
            )
        })?;
        out[idx] = (hi << 4) | lo;
    }
    Ok(out)
}

const fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 可信安全层稳定错误码（供前端和测试按枚举码断言，无需解析自然语言文本）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrustedUiErrorCode {
    UntrustedZ4ForgeAttempt,
    Z4PlaneOcclusionRejected,
    UntrustedSealReadDenied,
    ClickjackingArmingActive,
    UntrustedInputOrigin,
    SessionNotActiveOrStaleGeneration,
    ChallengeNotFoundOrExpired,
    ChallengeAlreadyResolved,
    IrreversibleHighContrastAckRequired,
    IrreversibleStatutoryLockViolation,
    ChooserSessionNotFound,
    ChooserItemNotInVault,
    ChooserRightsExceedModeCeiling,
    ChooserInvalidExportName,
    ChooserGrantNotFound,
    ChooserGrantRevoked,
    ChooserGrantExpired,
    ChooserGrantApplicationMismatch,
    ChooserGrantHandleOutOfScope,
    ChooserGrantInsufficientRights,
    MissingUserGesture,
    InvalidOrExpiredUserGesture,
    AutomationGestureForbidden,
    TransferNotFound,
    TransferExpired,
    TransferRequiresZ4Escalation,
    TransferConsentMismatchOrDenied,
    InvalidArgument,
}

impl TrustedUiErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UntrustedZ4ForgeAttempt => "UNTRUSTED_Z4_FORGE_ATTEMPT",
            Self::Z4PlaneOcclusionRejected => "Z4_PLANE_OCCLUSION_REJECTED",
            Self::UntrustedSealReadDenied => "UNTRUSTED_SEAL_READ_DENIED",
            Self::ClickjackingArmingActive => "CLICKJACKING_ARMING_ACTIVE",
            Self::UntrustedInputOrigin => "UNTRUSTED_INPUT_ORIGIN",
            Self::SessionNotActiveOrStaleGeneration => "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
            Self::ChallengeNotFoundOrExpired => "CHALLENGE_NOT_FOUND_OR_EXPIRED",
            Self::ChallengeAlreadyResolved => "CHALLENGE_ALREADY_RESOLVED",
            Self::IrreversibleHighContrastAckRequired => "IRREVERSIBLE_HIGH_CONTRAST_ACK_REQUIRED",
            Self::IrreversibleStatutoryLockViolation => "IRREVERSIBLE_STATUTORY_LOCK_VIOLATION",
            Self::ChooserSessionNotFound => "CHOOSER_SESSION_NOT_FOUND",
            Self::ChooserItemNotInVault => "CHOOSER_ITEM_NOT_IN_VAULT",
            Self::ChooserRightsExceedModeCeiling => "CHOOSER_RIGHTS_EXCEED_MODE_CEILING",
            Self::ChooserInvalidExportName => "CHOOSER_INVALID_EXPORT_NAME",
            Self::ChooserGrantNotFound => "CHOOSER_GRANT_NOT_FOUND",
            Self::ChooserGrantRevoked => "CHOOSER_GRANT_REVOKED",
            Self::ChooserGrantExpired => "CHOOSER_GRANT_EXPIRED",
            Self::ChooserGrantApplicationMismatch => "CHOOSER_GRANT_APPLICATION_MISMATCH",
            Self::ChooserGrantHandleOutOfScope => "CHOOSER_GRANT_HANDLE_OUT_OF_SCOPE",
            Self::ChooserGrantInsufficientRights => "CHOOSER_GRANT_INSUFFICIENT_RIGHTS",
            Self::MissingUserGesture => "MISSING_USER_GESTURE",
            Self::InvalidOrExpiredUserGesture => "INVALID_OR_EXPIRED_USER_GESTURE",
            Self::AutomationGestureForbidden => "AUTOMATION_GESTURE_FORBIDDEN",
            Self::TransferNotFound => "TRANSFER_NOT_FOUND",
            Self::TransferExpired => "TRANSFER_EXPIRED",
            Self::TransferRequiresZ4Escalation => "TRANSFER_REQUIRES_Z4_ESCALATION",
            Self::TransferConsentMismatchOrDenied => "TRANSFER_CONSENT_MISMATCH_OR_DENIED",
            Self::InvalidArgument => "INVALID_ARGUMENT",
        }
    }
}

/// 可信安全交互层类型化错误。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedUiError {
    pub code: TrustedUiErrorCode,
    pub message: String,
}

impl TrustedUiError {
    #[must_use]
    pub fn new(code: TrustedUiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for TrustedUiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[{}] {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for TrustedUiError {}

impl From<TrustedUiError> for DesktopError {
    fn from(err: TrustedUiError) -> Self {
        let mapped = match err.code {
            TrustedUiErrorCode::InvalidArgument | TrustedUiErrorCode::ChooserInvalidExportName => {
                ErrorCode::Config
            }
            TrustedUiErrorCode::ChallengeNotFoundOrExpired
            | TrustedUiErrorCode::ChooserSessionNotFound
            | TrustedUiErrorCode::ChooserItemNotInVault
            | TrustedUiErrorCode::ChooserGrantNotFound
            | TrustedUiErrorCode::TransferNotFound => ErrorCode::NotFound,
            _ => ErrorCode::Control,
        };
        DesktopError::new(mapped, err.to_string())
    }
}

/// 请求进入 `TrustedUiBroker` 或 `ChooserGrantBroker` 的调用者来源。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrustedUiCallerOrigin {
    /// 宿主 TCB `TrustedUiBroker` 内部可信系统调用。
    TcbTrustedBroker,
    /// 不可信 `Z2` 应用窗口表面发起的请求（禁止直接锻造或遮挡 `Z4`、禁止读取安全印章）。
    UntrustedZ2Surface {
        surface_id: [u8; 16],
        application_id: [u8; 16],
        requested_z_index: Option<u16>,
    },
}

/// 安全审计计数器：记录所有防欺骗、防遮挡、防点击劫持、法定锁、Chooser 与剪贴板拦截事件。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedUiSecurityAuditCounters {
    pub z4_challenges_opened: u64,
    pub z4_challenges_approved: u64,
    pub z4_challenges_cancelled: u64,
    pub z4_challenges_invalidated_by_session: u64,
    pub rejected_untrusted_z4_forge_attempts: u64,
    pub rejected_z4_occlusion_attempts: u64,
    pub rejected_untrusted_seal_reads: u64,
    pub rejected_clickjacking_arming_attempts: u64,
    pub rejected_untrusted_confirm_spoofs: u64,
    pub rejected_irreversible_ack_missing: u64,
    pub rejected_irreversible_undo_attempts: u64,
    pub chooser_grants_issued: u64,
    pub chooser_grants_revoked: u64,
    pub rejected_chooser_boundary_violations: u64,
    pub transfers_published: u64,
    pub rejected_missing_gesture_reads: u64,
    pub rejected_expired_transfer_reads: u64,
    pub escalated_tainted_transfers: u64,
}

/// 动作可逆性等级（对齐 `[RCPT-UNDO-001]` 与 `ADR-0025` 决定 4）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReversibilityClass {
    LosslessUndo,
    CompensatableRollback,
    Irreversible,
}

impl ReversibilityClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LosslessUndo => "LOSSLESS_UNDO",
            Self::CompensatableRollback => "COMPENSATABLE_ROLLBACK",
            Self::Irreversible => "IRREVERSIBLE",
        }
    }

    /// 解析可逆性等级文本。
    ///
    /// # Errors
    ///
    /// 非法标识返回 [`TrustedUiError`]。
    pub fn parse(raw: &str) -> Result<Self, TrustedUiError> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "LOSSLESS_UNDO" | "LOSSLESS" => Ok(Self::LosslessUndo),
            "COMPENSATABLE_ROLLBACK" | "COMPENSATABLE" => Ok(Self::CompensatableRollback),
            "IRREVERSIBLE" => Ok(Self::Irreversible),
            other => Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!("unknown reversibility class: {other}"),
            )),
        }
    }

    /// 法定锁契约：`IRREVERSIBLE` 动作严禁暴露任何撤销入口。
    #[must_use]
    pub const fn undo_affordance_allowed(self) -> bool {
        match self {
            Self::LosslessUndo | Self::CompensatableRollback => true,
            Self::Irreversible => false,
        }
    }

    /// 是否要求高对比度显式二次确认勾选。
    #[must_use]
    pub const fn requires_high_contrast_ack(self) -> bool {
        matches!(self, Self::Irreversible)
    }

    /// 对应法定锁标识（与前端 `status-indicator.ts` `IRREVERSIBLE_NO_UNDO` 对齐）。
    #[must_use]
    pub const fn statutory_lock_kind(self) -> Option<&'static str> {
        match self {
            Self::Irreversible => Some("IRREVERSIBLE_NO_UNDO"),
            Self::LosslessUndo | Self::CompensatableRollback => None,
        }
    }
}

/// 授权有效期范围（单次动作 / 当前任务内 / 当前会话内）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConsentGrantScope {
    #[default]
    SingleAction,
    CurrentTask,
    CurrentSession,
}

impl ConsentGrantScope {
    /// 解析授权范围文本。
    ///
    /// # Errors
    ///
    /// 非法标识返回 [`TrustedUiError`]。
    pub fn parse(raw: &str) -> Result<Self, TrustedUiError> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "SINGLE_ACTION" | "ONCE" => Ok(Self::SingleAction),
            "CURRENT_TASK" | "TASK" => Ok(Self::CurrentTask),
            "CURRENT_SESSION" | "SESSION" => Ok(Self::CurrentSession),
            other => Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!("unknown consent grant scope: {other}"),
            )),
        }
    }
}

/// 级联爆炸半径预览行（对齐前端 `BlastRadiusRow` `+ / ~ / - / =`）。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Z4BlastRadiusRowDto {
    pub symbol: String,
    pub category: String,
    pub scope_label: String,
    pub target_label: String,
    pub reason: String,
    pub target_id_hex: Option<String>,
}

/// `Z4` 4 行结构化规格表第 1 行：请求应用身份与签名核验信息。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Z4RequestingAppSpec {
    pub display_name: String,
    pub publisher_name: String,
    pub signature_verified: bool,
    pub application_id_hex: String,
    pub installation_id_hex: String,
    pub principal_id_hex: String,
    pub installation_generation: u64,
    pub manifest_digest_hex: String,
}

/// `Z4` 4 行结构化规格表第 2 行：请求能力与动作边界（`L1/L2` 日常直白语义 + `L3` 原始协议字段）。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Z4ActionScopeSpec {
    pub plain_summary_zh: String,
    pub plain_summary_en: String,
    pub capability_rights: Vec<String>,
    pub control_scope: String,
    pub plan_digest_hex: String,
}

/// `Z4` 4 行结构化规格表第 3 行：资源与预算上限。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Z4ResourceCeilingSpec {
    pub plain_ceiling_zh: String,
    pub plain_ceiling_en: String,
    pub memory_ceiling_mib: u64,
    pub token_budget_ceiling: u64,
    pub network_egress_allowed: bool,
    pub max_duration_ms: u64,
}

/// `Z4` 4 行结构化规格表第 4 行：可逆性等级、风险与级联爆炸半径。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Z4ReversibilityRiskSpec {
    pub reversibility_class: ReversibilityClass,
    pub plain_consequence_zh: String,
    pub plain_consequence_en: String,
    pub multi_target_count: u32,
    pub partial_failure_disclosure_zh: Option<String>,
    pub partial_failure_disclosure_en: Option<String>,
    pub blast_radius_rows: Vec<Z4BlastRadiusRowDto>,
    pub requires_data_loss_receipt: bool,
}

/// 打开 `Z4` 安全确认屏的完整规格输入。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Z4ConsentChallengeRequest {
    pub requesting_app: Z4RequestingAppSpec,
    pub action_scope: Z4ActionScopeSpec,
    pub resource_ceiling: Z4ResourceCeilingSpec,
    pub reversibility_and_risk: Z4ReversibilityRiskSpec,
    pub allowed_grant_scopes: Vec<ConsentGrantScope>,
}

/// `Z4` 授权挑战生命周期状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Z4ChallengeState {
    Pending,
    Approved,
    CancelledByUser,
    InvalidatedBySessionFence,
}

/// 面向宿主可信 `Z4` 渲染层的完整安全屏投影（唯一合法包含 `security_seal_hint` 的投影）。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Z4TrustedChallengeDto {
    pub challenge_id_hex: String,
    pub state: Z4ChallengeState,
    pub z_index: u16,
    pub scrim_rgba: String,
    pub zero_blur_filter: bool,
    pub dialog_radius_px: u8,
    pub top_security_bar_px: u8,
    pub seal_width_px: u8,
    pub seal_height_px: u8,
    pub security_seal_hint: String,
    pub session_generation: u64,
    pub presented_at_mono_ms: u64,
    pub armed_from_mono_ms: u64,
    pub arming_window_ms: u64,
    pub requesting_app: Z4RequestingAppSpec,
    pub action_scope: Z4ActionScopeSpec,
    pub resource_ceiling: Z4ResourceCeilingSpec,
    pub reversibility_and_risk: Z4ReversibilityRiskSpec,
    pub undo_affordance_allowed: bool,
    pub requires_high_contrast_ack: bool,
    pub statutory_lock_kind: Option<String>,
    pub signal_color_hex: Option<String>,
    pub allowed_grant_scopes: Vec<ConsentGrantScope>,
    pub approved_receipt_id_hex: Option<String>,
}

/// 面向 `Z0..Z3` 低信任平面的只读安全态势摘要（严格不含 `security_seal_hint`）。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LowerPlaneTrustedPostureDto {
    pub z4_modal_active: bool,
    pub z4_z_index: u16,
    pub z2_z_ceiling: u16,
    pub active_challenge_id_hex: Option<String>,
    pub active_chooser_session_id_hex: Option<String>,
    pub session_generation: u64,
}

/// `Z4` 用户确认或拒绝后的权威回执。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Z4ConsentDecisionReceiptDto {
    pub receipt_id_hex: String,
    pub challenge_id_hex: String,
    pub state: Z4ChallengeState,
    pub chosen_grant_scope: Option<ConsentGrantScope>,
    pub reversibility_class: ReversibilityClass,
    pub undo_affordance_allowed: bool,
    pub requires_data_loss_receipt: bool,
    pub decided_at_mono_ms: u64,
    pub session_generation: u64,
    pub decision_digest_hex: String,
}

#[derive(Clone, Debug)]
struct Z4ChallengeRecord {
    challenge_id: [u8; 16],
    state: Z4ChallengeState,
    session_generation: u64,
    presented_at_mono_ms: u64,
    armed_from_mono_ms: u64,
    requesting_app: Z4RequestingAppSpec,
    action_scope: Z4ActionScopeSpec,
    resource_ceiling: Z4ResourceCeilingSpec,
    reversibility_and_risk: Z4ReversibilityRiskSpec,
    allowed_grant_scopes: Vec<ConsentGrantScope>,
    decision_receipt: Option<Z4ConsentDecisionReceiptDto>,
}

/// 宿主 TCB `Z4` 可信安全确认屏代理 (`[TUI-CONSENT-001]`, `[TUI-SEAL-001]`, `[TUI-GUARD-001]`)。
#[derive(Debug, Default)]
pub struct TrustedUiBroker {
    challenges: BTreeMap<[u8; 16], Z4ChallengeRecord>,
    receipts: BTreeMap<[u8; 16], Z4ConsentDecisionReceiptDto>,
    active_challenge_id: Option<[u8; 16]>,
    next_nonce: u64,
}

impl TrustedUiBroker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn active_challenge_id(&self) -> Option<[u8; 16]> {
        self.active_challenge_id
    }

    /// 校验平面 Z-index 申请：任何不可信 `Z2` 表面试图越过 `Z2_SURFACE_Z_CEILING (299)` 或遮挡 `Z4 (900)` 一律拒绝。
    ///
    /// # Errors
    ///
    /// 越界请求返回 [`TrustedUiErrorCode::Z4PlaneOcclusionRejected`]。
    pub fn guard_plane_z_index(
        &self,
        caller: TrustedUiCallerOrigin,
        requested_z_index: u16,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<u16, TrustedUiError> {
        match caller {
            TrustedUiCallerOrigin::TcbTrustedBroker => Ok(requested_z_index),
            TrustedUiCallerOrigin::UntrustedZ2Surface {
                surface_id,
                application_id,
                ..
            } => {
                if requested_z_index > Z2_SURFACE_Z_CEILING {
                    audit.rejected_z4_occlusion_attempts =
                        audit.rejected_z4_occlusion_attempts.saturating_add(1);
                    return Err(TrustedUiError::new(
                        TrustedUiErrorCode::Z4PlaneOcclusionRejected,
                        format!(
                            "untrusted Z2 surface {} (app {}) requested z-index {}, exceeding Z2 ceiling {} / attempting Z4 ({}) occlusion",
                            hex(&surface_id),
                            hex(&application_id),
                            requested_z_index,
                            Z2_SURFACE_Z_CEILING,
                            Z4_SECURITY_Z_INDEX
                        ),
                    ));
                }
                Ok(requested_z_index)
            }
        }
    }

    /// 会话锁屏、挂起、关闭或切换用户时立即吊销一切挂起的 `Z4` 授权挑战。
    pub fn sync_with_session(
        &mut self,
        session_state: DesktopSessionState,
        session_generation: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) {
        let mut invalidated_active = false;
        for record in self.challenges.values_mut() {
            if record.state == Z4ChallengeState::Pending
                && (session_state != DesktopSessionState::Active
                    || record.session_generation != session_generation)
            {
                record.state = Z4ChallengeState::InvalidatedBySessionFence;
                audit.z4_challenges_invalidated_by_session =
                    audit.z4_challenges_invalidated_by_session.saturating_add(1);
                if self.active_challenge_id == Some(record.challenge_id) {
                    invalidated_active = true;
                }
            }
        }
        if invalidated_active {
            self.active_challenge_id = None;
        }
    }

    /// 由宿主 TCB 打开不可遮挡 `Z4` 安全确认屏。
    ///
    /// # Errors
    ///
    /// - 不可信 `Z2` 表面伪造 `Z4` 请求返回 `UntrustedZ4ForgeAttempt`；
    /// - 附带越界 `requested_z_index` 返回 `Z4PlaneOcclusionRejected`；
    /// - 非 `ACTIVE` 会话返回 `SessionNotActiveOrStaleGeneration`。
    pub fn open_consent_challenge(
        &mut self,
        caller: TrustedUiCallerOrigin,
        session: &DesktopSessionManager,
        mut request: Z4ConsentChallengeRequest,
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<Z4TrustedChallengeDto, TrustedUiError> {
        match caller {
            TrustedUiCallerOrigin::TcbTrustedBroker => {}
            TrustedUiCallerOrigin::UntrustedZ2Surface {
                surface_id,
                application_id,
                requested_z_index,
            } => {
                if let Some(req_z) = requested_z_index
                    && req_z > Z2_SURFACE_Z_CEILING
                {
                    audit.rejected_z4_occlusion_attempts =
                        audit.rejected_z4_occlusion_attempts.saturating_add(1);
                    return Err(TrustedUiError::new(
                        TrustedUiErrorCode::Z4PlaneOcclusionRejected,
                        format!(
                            "untrusted Z2 surface {} (app {}) attempted z-index {} occlusion of Z4 ({})",
                            hex(&surface_id),
                            hex(&application_id),
                            req_z,
                            Z4_SECURITY_Z_INDEX
                        ),
                    ));
                }
                audit.rejected_untrusted_z4_forge_attempts =
                    audit.rejected_untrusted_z4_forge_attempts.saturating_add(1);
                return Err(TrustedUiError::new(
                    TrustedUiErrorCode::UntrustedZ4ForgeAttempt,
                    format!(
                        "untrusted Z2 surface {} (app {}) forbidden from forging Z4 consent screen",
                        hex(&surface_id),
                        hex(&application_id)
                    ),
                ));
            }
        }

        if session.state() != DesktopSessionState::Active {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                format!(
                    "cannot open Z4 consent challenge while session is {}",
                    session.state().as_str()
                ),
            ));
        }

        // 校验必填 4 行字段非空，并为多目标动作自动补齐 [RCPT-PARTIAL-001] 局部失败风险披露。
        if request.requesting_app.display_name.trim().is_empty()
            || request.action_scope.plain_summary_zh.trim().is_empty()
            || request.action_scope.plain_summary_en.trim().is_empty()
            || request.resource_ceiling.plain_ceiling_zh.trim().is_empty()
            || request.resource_ceiling.plain_ceiling_en.trim().is_empty()
            || request
                .reversibility_and_risk
                .plain_consequence_zh
                .trim()
                .is_empty()
            || request
                .reversibility_and_risk
                .plain_consequence_en
                .trim()
                .is_empty()
        {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                "all 4 structured KV rows in Z4 consent screen must provide non-empty bilingual disclosures",
            ));
        }
        let _ = parse_hex_id_16(
            "requesting_app.application_id_hex",
            &request.requesting_app.application_id_hex,
        )?;
        let _ = parse_hex_id_16(
            "requesting_app.installation_id_hex",
            &request.requesting_app.installation_id_hex,
        )?;
        let _ = parse_hex_id_16(
            "requesting_app.principal_id_hex",
            &request.requesting_app.principal_id_hex,
        )?;
        let _ = parse_hex_digest_32(
            "requesting_app.manifest_digest_hex",
            &request.requesting_app.manifest_digest_hex,
        )?;
        let _ = parse_hex_digest_32(
            "action_scope.plan_digest_hex",
            &request.action_scope.plan_digest_hex,
        )?;

        if request.reversibility_and_risk.multi_target_count > 1 {
            if request
                .reversibility_and_risk
                .partial_failure_disclosure_zh
                .as_ref()
                .is_none_or(|s| s.trim().is_empty())
            {
                request.reversibility_and_risk.partial_failure_disclosure_zh = Some(format!(
                    "此操作涉及 {} 个目标；若中途部分失败，已完成项与未完成项将分别出具明细并需逐项核查。",
                    request.reversibility_and_risk.multi_target_count
                ));
            }
            if request
                .reversibility_and_risk
                .partial_failure_disclosure_en
                .as_ref()
                .is_none_or(|s| s.trim().is_empty())
            {
                request.reversibility_and_risk.partial_failure_disclosure_en = Some(format!(
                    "Affects {} targets; if partial failure occurs, succeeded and failed targets are reported separately for review.",
                    request.reversibility_and_risk.multi_target_count
                ));
            }
        }

        if request
            .reversibility_and_risk
            .blast_radius_rows
            .iter()
            .any(|row| row.category.eq_ignore_ascii_case("purged"))
        {
            request.reversibility_and_risk.requires_data_loss_receipt = true;
        }

        let allowed_grant_scopes = if request.allowed_grant_scopes.is_empty() {
            vec![ConsentGrantScope::SingleAction]
        } else {
            request.allowed_grant_scopes
        };

        self.next_nonce = self.next_nonce.saturating_add(1);
        let session_gen = session.session_generation();
        let challenge_id = derive_id_16(
            b"llmos/stage-d/z4-challenge/v1",
            &[
                &self.next_nonce.to_be_bytes(),
                &session_gen.to_be_bytes(),
                &now_mono_ms.to_be_bytes(),
                request.requesting_app.application_id_hex.as_bytes(),
                request.action_scope.plan_digest_hex.as_bytes(),
            ],
        );

        // 若此前仍有挂起挑战，替换前将其关闭为取消态，保证单时刻单顶层 Z4 焦点仲裁。
        if let Some(prev_id) = self.active_challenge_id.take()
            && let Some(prev) = self.challenges.get_mut(&prev_id)
            && prev.state == Z4ChallengeState::Pending
        {
            prev.state = Z4ChallengeState::CancelledByUser;
        }

        let record = Z4ChallengeRecord {
            challenge_id,
            state: Z4ChallengeState::Pending,
            session_generation: session_gen,
            presented_at_mono_ms: now_mono_ms,
            armed_from_mono_ms: now_mono_ms,
            requesting_app: request.requesting_app,
            action_scope: request.action_scope,
            resource_ceiling: request.resource_ceiling,
            reversibility_and_risk: request.reversibility_and_risk,
            allowed_grant_scopes,
            decision_receipt: None,
        };
        self.challenges.insert(challenge_id, record);
        self.active_challenge_id = Some(challenge_id);
        audit.z4_challenges_opened = audit.z4_challenges_opened.saturating_add(1);

        self.project_trusted_challenge(caller, session, challenge_id, audit)
    }

    /// 记录 `Z4` 窗口焦点失而复得事件：重置 `350ms` 防点击劫持武装计时起点。
    ///
    /// # Errors
    ///
    /// 非 TCB 调用或挑战不存在/已结束时返回 [`TrustedUiError`]。
    pub fn notify_focus_regained(
        &mut self,
        caller: TrustedUiCallerOrigin,
        challenge_id: [u8; 16],
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<u64, TrustedUiError> {
        if !matches!(caller, TrustedUiCallerOrigin::TcbTrustedBroker) {
            audit.rejected_untrusted_z4_forge_attempts =
                audit.rejected_untrusted_z4_forge_attempts.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::UntrustedZ4ForgeAttempt,
                "only TCB TrustedUiBroker may reset Z4 focus-regain arming timer",
            ));
        }
        let record = self.challenges.get_mut(&challenge_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::ChallengeNotFoundOrExpired,
                format!("Z4 challenge {} not found", hex(&challenge_id)),
            )
        })?;
        if record.state != Z4ChallengeState::Pending {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ChallengeAlreadyResolved,
                format!(
                    "Z4 challenge {} is already {:?}",
                    hex(&challenge_id),
                    record.state
                ),
            ));
        }
        record.armed_from_mono_ms = now_mono_ms.max(record.armed_from_mono_ms);
        Ok(record.armed_from_mono_ms)
    }

    /// 裁决 `Z4` 安全确认挑战：
    /// - `confirm == false` (`Escape` / 取消)：`0ms` 立即可达（但仍要求 `InputOrigin::TrustedShell`，防止恶意应用静默代用户取消或试探）；
    /// - `confirm == true`：要求 `TrustedShell`、非 `Automation`、活动会话同代际、距离呈现及最近一次焦点重获 `>= 350ms`，
    ///   且 `IRREVERSIBLE` 动作必须携带 `irreversible_high_contrast_ack == true`。
    ///
    /// # Errors
    ///
    /// 违反防欺骗、武装时间窗、会话代际或不可逆显式确认规则时返回对应类型化 [`TrustedUiError`]。
    #[allow(clippy::too_many_arguments)]
    pub fn decide_consent_challenge(
        &mut self,
        origin: InputOrigin,
        source: InputSource,
        session: &DesktopSessionManager,
        challenge_id: [u8; 16],
        confirm: bool,
        chosen_grant_scope: Option<ConsentGrantScope>,
        irreversible_high_contrast_ack: bool,
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<Z4ConsentDecisionReceiptDto, TrustedUiError> {
        self.sync_with_session(session.state(), session.session_generation(), audit);

        if !matches!(origin, InputOrigin::TrustedShell)
            || (confirm && source == InputSource::Automation)
        {
            audit.rejected_untrusted_confirm_spoofs =
                audit.rejected_untrusted_confirm_spoofs.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::UntrustedInputOrigin,
                "untrusted surface or automated script forbidden from deciding Z4 security screen",
            ));
        }

        let record = self.challenges.get_mut(&challenge_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::ChallengeNotFoundOrExpired,
                format!("Z4 challenge {} not found", hex(&challenge_id)),
            )
        })?;

        if record.state == Z4ChallengeState::InvalidatedBySessionFence {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                format!(
                    "Z4 challenge {} invalidated by session lock/switch",
                    hex(&challenge_id)
                ),
            ));
        }
        if record.state != Z4ChallengeState::Pending {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ChallengeAlreadyResolved,
                format!(
                    "Z4 challenge {} already resolved as {:?}",
                    hex(&challenge_id),
                    record.state
                ),
            ));
        }

        let rev_class = record.reversibility_and_risk.reversibility_class;

        if !confirm {
            record.state = Z4ChallengeState::CancelledByUser;
            if self.active_challenge_id == Some(challenge_id) {
                self.active_challenge_id = None;
            }
            audit.z4_challenges_cancelled = audit.z4_challenges_cancelled.saturating_add(1);
            let receipt = build_decision_receipt(
                challenge_id,
                Z4ChallengeState::CancelledByUser,
                None,
                rev_class,
                record.reversibility_and_risk.requires_data_loss_receipt,
                now_mono_ms,
                record.session_generation,
            );
            let receipt_id = parse_hex_id_16("receipt_id_hex", &receipt.receipt_id_hex)?;
            record.decision_receipt = Some(receipt.clone());
            self.receipts.insert(receipt_id, receipt.clone());
            return Ok(receipt);
        }

        if session.state() != DesktopSessionState::Active
            || session.session_generation() != record.session_generation
        {
            record.state = Z4ChallengeState::InvalidatedBySessionFence;
            if self.active_challenge_id == Some(challenge_id) {
                self.active_challenge_id = None;
            }
            audit.z4_challenges_invalidated_by_session =
                audit.z4_challenges_invalidated_by_session.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                "session state or generation changed before Z4 confirmation",
            ));
        }

        let elapsed = now_mono_ms.saturating_sub(record.armed_from_mono_ms);
        if elapsed < Z4_CLICKJACK_ARMING_MS {
            audit.rejected_clickjacking_arming_attempts = audit
                .rejected_clickjacking_arming_attempts
                .saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ClickjackingArmingActive,
                format!(
                    "Z4 confirm rejected during {Z4_CLICKJACK_ARMING_MS}ms clickjacking arming window (elapsed {elapsed}ms)"
                ),
            ));
        }

        if rev_class.requires_high_contrast_ack() && !irreversible_high_contrast_ack {
            audit.rejected_irreversible_ack_missing =
                audit.rejected_irreversible_ack_missing.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::IrreversibleHighContrastAckRequired,
                "IRREVERSIBLE action requires explicit high-contrast user acknowledgement",
            ));
        }

        let scope = chosen_grant_scope.unwrap_or(ConsentGrantScope::SingleAction);
        if !record.allowed_grant_scopes.contains(&scope) {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!("grant scope {scope:?} is not permitted for this challenge"),
            ));
        }

        record.state = Z4ChallengeState::Approved;
        if self.active_challenge_id == Some(challenge_id) {
            self.active_challenge_id = None;
        }
        audit.z4_challenges_approved = audit.z4_challenges_approved.saturating_add(1);

        let receipt = build_decision_receipt(
            challenge_id,
            Z4ChallengeState::Approved,
            Some(scope),
            rev_class,
            record.reversibility_and_risk.requires_data_loss_receipt,
            now_mono_ms,
            record.session_generation,
        );
        let receipt_id = parse_hex_id_16("receipt_id_hex", &receipt.receipt_id_hex)?;
        record.decision_receipt = Some(receipt.clone());
        self.receipts.insert(receipt_id, receipt.clone());
        Ok(receipt)
    }

    /// 对已签发的动作授权回执发起撤销/回滚请求：法定锁强制拒绝任何针对 `IRREVERSIBLE` 回执的伪撤销调用。
    ///
    /// # Errors
    ///
    /// `IRREVERSIBLE` 回执返回 [`TrustedUiErrorCode::IrreversibleStatutoryLockViolation`]。
    pub fn request_undo_for_receipt(
        &self,
        receipt_id: [u8; 16],
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<&'static str, TrustedUiError> {
        let receipt = self.receipts.get(&receipt_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::ChallengeNotFoundOrExpired,
                format!("Z4 decision receipt {} not found", hex(&receipt_id)),
            )
        })?;
        match receipt.reversibility_class {
            ReversibilityClass::LosslessUndo => Ok("LOSSLESS_UNDO_DISPATCHED"),
            ReversibilityClass::CompensatableRollback => Ok("COMPENSATABLE_ROLLBACK_DISPATCHED"),
            ReversibilityClass::Irreversible => {
                audit.rejected_irreversible_undo_attempts =
                    audit.rejected_irreversible_undo_attempts.saturating_add(1);
                Err(TrustedUiError::new(
                    TrustedUiErrorCode::IrreversibleStatutoryLockViolation,
                    "statutory lock [RCPT-UNDO-001]: IRREVERSIBLE receipt forbids Undo affordance and execution",
                ))
            }
        }
    }

    /// 仅供宿主 TCB `Z4` 投影读取指定挑战（携带当前活动用户的私密防钓鱼 `security_seal_hint`）。
    ///
    /// # Errors
    ///
    /// 不可信 `Z2` 表面试图读取时返回 [`TrustedUiErrorCode::UntrustedSealReadDenied`]。
    pub fn project_trusted_challenge(
        &self,
        caller: TrustedUiCallerOrigin,
        session: &DesktopSessionManager,
        challenge_id: [u8; 16],
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<Z4TrustedChallengeDto, TrustedUiError> {
        if !matches!(caller, TrustedUiCallerOrigin::TcbTrustedBroker) {
            audit.rejected_untrusted_seal_reads =
                audit.rejected_untrusted_seal_reads.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::UntrustedSealReadDenied,
                "security_seal_hint and Z4 trusted projection are strictly confined to TCB Z4 plane",
            ));
        }

        let record = self.challenges.get(&challenge_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::ChallengeNotFoundOrExpired,
                format!("Z4 challenge {} not found", hex(&challenge_id)),
            )
        })?;
        let rev_class = record.reversibility_and_risk.reversibility_class;
        let active_user = session.active_user().map_err(|err| {
            TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                err.to_string(),
            )
        })?;
        let seal_hint = active_user.z4_trusted_seal_hint().to_owned();

        Ok(Z4TrustedChallengeDto {
            challenge_id_hex: hex(&record.challenge_id),
            state: record.state,
            z_index: Z4_SECURITY_Z_INDEX,
            scrim_rgba: Z4_SCRIM_RGBA.to_owned(),
            zero_blur_filter: true,
            dialog_radius_px: Z4_DIALOG_RADIUS_PX,
            top_security_bar_px: Z4_TOP_SECURITY_BAR_HEIGHT_PX,
            seal_width_px: Z4_SEAL_WIDTH_PX,
            seal_height_px: Z4_SEAL_HEIGHT_PX,
            security_seal_hint: seal_hint,
            session_generation: record.session_generation,
            presented_at_mono_ms: record.presented_at_mono_ms,
            armed_from_mono_ms: record.armed_from_mono_ms,
            arming_window_ms: Z4_CLICKJACK_ARMING_MS,
            requesting_app: record.requesting_app.clone(),
            action_scope: record.action_scope.clone(),
            resource_ceiling: record.resource_ceiling.clone(),
            reversibility_and_risk: record.reversibility_and_risk.clone(),
            undo_affordance_allowed: rev_class.undo_affordance_allowed(),
            requires_high_contrast_ack: rev_class.requires_high_contrast_ack(),
            statutory_lock_kind: rev_class.statutory_lock_kind().map(str::to_owned),
            signal_color_hex: if rev_class == ReversibilityClass::Irreversible {
                Some(Z4_IRREVERSIBLE_CRIMSON_HEX.to_owned())
            } else {
                None
            },
            allowed_grant_scopes: record.allowed_grant_scopes.clone(),
            approved_receipt_id_hex: record
                .decision_receipt
                .as_ref()
                .map(|r| r.receipt_id_hex.clone()),
        })
    }

    #[must_use]
    pub fn challenge_state(&self, challenge_id: [u8; 16]) -> Option<Z4ChallengeState> {
        self.challenges.get(&challenge_id).map(|c| c.state)
    }
}

fn build_decision_receipt(
    challenge_id: [u8; 16],
    state: Z4ChallengeState,
    chosen_grant_scope: Option<ConsentGrantScope>,
    reversibility_class: ReversibilityClass,
    requires_data_loss_receipt: bool,
    decided_at_mono_ms: u64,
    session_generation: u64,
) -> Z4ConsentDecisionReceiptDto {
    let state_bytes = match state {
        Z4ChallengeState::Pending => b"PENDING".as_slice(),
        Z4ChallengeState::Approved => b"APPROVED".as_slice(),
        Z4ChallengeState::CancelledByUser => b"CANCELLED_BY_USER".as_slice(),
        Z4ChallengeState::InvalidatedBySessionFence => b"INVALIDATED_BY_SESSION_FENCE".as_slice(),
    };
    let scope_bytes = match chosen_grant_scope {
        Some(ConsentGrantScope::SingleAction) => b"SINGLE_ACTION".as_slice(),
        Some(ConsentGrantScope::CurrentTask) => b"CURRENT_TASK".as_slice(),
        Some(ConsentGrantScope::CurrentSession) => b"CURRENT_SESSION".as_slice(),
        None => b"NONE".as_slice(),
    };
    let digest = sha256_domain_digest(
        b"llmos/stage-d/z4-consent-receipt/v1",
        &[
            &challenge_id,
            state_bytes,
            scope_bytes,
            reversibility_class.as_str().as_bytes(),
            &decided_at_mono_ms.to_be_bytes(),
            &session_generation.to_be_bytes(),
        ],
    );
    let mut receipt_id = [0u8; 16];
    receipt_id.copy_from_slice(&digest[..16]);

    Z4ConsentDecisionReceiptDto {
        receipt_id_hex: hex(&receipt_id),
        challenge_id_hex: hex(&challenge_id),
        state,
        chosen_grant_scope,
        reversibility_class,
        undo_affordance_allowed: reversibility_class.undo_affordance_allowed(),
        requires_data_loss_receipt,
        decided_at_mono_ms,
        session_generation,
        decision_digest_hex: hex(&digest),
    }
}

// ============================================================================
// 2. System ChooserGrant Scoped Capability Broker ([TUI-CHOOSER-001], [DUI-CHOOSER-001])
// ============================================================================

/// 系统文件/对象选择器模式（`Open` / `Save` / `Share`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChooserMode {
    Open,
    Save,
    Share,
}

impl ChooserMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Save => "SAVE",
            Self::Share => "SHARE",
        }
    }

    /// 解析选择器模式。
    ///
    /// # Errors
    ///
    /// 未知模式返回 [`TrustedUiError`]。
    pub fn parse(raw: &str) -> Result<Self, TrustedUiError> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "OPEN" => Ok(Self::Open),
            "SAVE" => Ok(Self::Save),
            "SHARE" => Ok(Self::Share),
            other => Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!("unknown chooser mode: {other}"),
            )),
        }
    }

    #[must_use]
    pub const fn allows_rights(self, rights: ChooserAttenuatedRights) -> bool {
        matches!(
            (self, rights),
            (
                Self::Open,
                ChooserAttenuatedRights::ReadOnly | ChooserAttenuatedRights::ReadWriteSingle
            ) | (
                Self::Save,
                ChooserAttenuatedRights::ExportSave | ChooserAttenuatedRights::ReadWriteSingle
            ) | (Self::Share, ChooserAttenuatedRights::ReadOnly)
        )
    }
}

/// 经 `ChooserGrantBroker` 衰减后的最小权限集合。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChooserAttenuatedRights {
    ReadOnly,
    ReadWriteSingle,
    ExportSave,
}

impl ChooserAttenuatedRights {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "READ_ONLY",
            Self::ReadWriteSingle => "READ_WRITE_SINGLE",
            Self::ExportSave => "EXPORT_SAVE",
        }
    }

    /// 解析衰减权限枚举。
    ///
    /// # Errors
    ///
    /// 未知权限返回 [`TrustedUiError`]。
    pub fn parse(raw: &str) -> Result<Self, TrustedUiError> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "READ_ONLY" => Ok(Self::ReadOnly),
            "READ_WRITE_SINGLE" => Ok(Self::ReadWriteSingle),
            "EXPORT_SAVE" => Ok(Self::ExportSave),
            other => Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!("unknown chooser attenuated rights: {other}"),
            )),
        }
    }

    #[must_use]
    pub const fn satisfies(self, required: Self) -> bool {
        matches!(
            (self, required),
            (Self::ReadOnly, Self::ReadOnly)
                | (
                    Self::ReadWriteSingle,
                    Self::ReadOnly | Self::ReadWriteSingle
                )
                | (Self::ExportSave, Self::ExportSave)
        )
    }
}

/// TCB 内部宿主保管条目（`host_backing_path` 保持私有，绝不向任何 `Application` DTO 暴露）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostVaultEntry {
    pub vault_item_id: [u8; 16],
    pub artifact_id: [u8; 32],
    host_backing_path: String,
    pub logical_collection_label: String,
    pub leaf_display_name: String,
    pub content_type: String,
    pub size_bytes: u64,
}

impl HostVaultEntry {
    /// 构造受托管宿主条目（自动校验 `leaf_display_name` 不含任何路径分隔符或盘符）。
    ///
    /// # Errors
    ///
    /// 非法叶子显示名返回 [`TrustedUiError`]。
    pub fn new(
        vault_item_id: [u8; 16],
        artifact_id: [u8; 32],
        host_backing_path: impl Into<String>,
        logical_collection_label: impl Into<String>,
        leaf_display_name: impl Into<String>,
        content_type: impl Into<String>,
        size_bytes: u64,
    ) -> Result<Self, TrustedUiError> {
        let leaf = sanitize_leaf_display_name(&leaf_display_name.into())?;
        Ok(Self {
            vault_item_id,
            artifact_id,
            host_backing_path: host_backing_path.into(),
            logical_collection_label: logical_collection_label.into(),
            leaf_display_name: leaf,
            content_type: content_type.into(),
            size_bytes,
        })
    }

    /// 仅供宿主 TCB 内核/测试验证底层宿主映射，严禁用于构造应用层消息。
    #[must_use]
    pub fn tcb_only_host_backing_path(&self) -> &str {
        &self.host_backing_path
    }
}

fn sanitize_leaf_display_name(raw: &str) -> Result<String, TrustedUiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains("..")
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains(':')
        || trimmed.contains('\0')
    {
        return Err(TrustedUiError::new(
            TrustedUiErrorCode::ChooserInvalidExportName,
            format!(
                "chooser display/export leaf name must not contain host path separators or traversal: {trimmed}"
            ),
        ));
    }
    Ok(trimmed.to_owned())
}

/// 面向宿主 `Z4` 系统选择器界面的可浏览条目摘要（不含裸宿主路径，且不可信 `Z2` 禁止直接拉取目录枚举）。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TcbChooserCatalogItemDto {
    pub vault_item_id_hex: String,
    pub logical_collection_label: String,
    pub leaf_display_name: String,
    pub content_type: String,
    pub size_bytes: u64,
}

/// 面向宿主 `Z4` 选择器模态层的会话投影。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TcbChooserSessionDto {
    pub chooser_session_id_hex: String,
    pub requesting_application_id_hex: String,
    pub requesting_app_display_name: String,
    pub mode: ChooserMode,
    pub requested_rights: ChooserAttenuatedRights,
    pub z_index: u16,
    pub security_seal_hint: String,
    pub session_generation: u64,
    pub presented_at_mono_ms: u64,
    pub armed_from_mono_ms: u64,
    pub arming_window_ms: u64,
    pub catalog_items: Vec<TcbChooserCatalogItemDto>,
}

/// 签发给请求方 `Application` 的单个不透明受限对象句柄（零宿主裸路径、零兄弟目录信息）。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedChooserHandleDto {
    pub scoped_handle_id_hex: String,
    pub artifact_id_hex: String,
    pub display_name: String,
    pub content_type: String,
    pub size_bytes: u64,
}

/// 唯一暴露给请求方 `Application` 的受限 `ChooserGrant` 凭证（`[TUI-CHOOSER-001]`, `[DUI-CHOOSER-001]`）。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChooserGrantForAppDto {
    pub grant_id_hex: String,
    pub requesting_application_id_hex: String,
    pub mode: ChooserMode,
    pub attenuated_rights: ChooserAttenuatedRights,
    pub scoped_handles: Vec<ScopedChooserHandleDto>,
    pub session_generation: u64,
    pub issued_at_mono_ms: u64,
    pub expires_at_mono_ms: u64,
    pub grant_digest_hex: String,
}

/// `Application` 凭 `ChooserGrant` 执行受限句柄访问时的核验结果。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExercisedChooserHandleDto {
    pub grant_id_hex: String,
    pub scoped_handle_id_hex: String,
    pub artifact_id_hex: String,
    pub granted_right: ChooserAttenuatedRights,
    pub display_name: String,
    pub content_type: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug)]
struct ChooserSessionRecord {
    chooser_session_id: [u8; 16],
    requesting_application_id: [u8; 16],
    mode: ChooserMode,
    requested_rights: ChooserAttenuatedRights,
    session_generation: u64,
    armed_from_mono_ms: u64,
    ttl_ms: u64,
    resolved: bool,
}

#[derive(Clone, Debug)]
struct IssuedChooserGrantRecord {
    dto: ChooserGrantForAppDto,
    requesting_application_id: [u8; 16],
    revoked: bool,
    /// TCB 内部句柄 -> 宿主文件保管条目映射（仅留在内核内存，绝不序列化给应用）。
    handle_to_vault_item: BTreeMap<[u8; 16], Option<[u8; 16]>>,
}

/// 系统受限文件/对象能力经纪器 (`ChooserGrantBroker`)。
#[derive(Debug)]
pub struct ChooserGrantBroker {
    vault: BTreeMap<[u8; 16], HostVaultEntry>,
    sessions: BTreeMap<[u8; 16], ChooserSessionRecord>,
    grants: BTreeMap<[u8; 16], IssuedChooserGrantRecord>,
    active_chooser_session_id: Option<[u8; 16]>,
    next_nonce: u64,
}

impl Default for ChooserGrantBroker {
    fn default() -> Self {
        Self::with_default_vault()
    }
}

impl ChooserGrantBroker {
    /// 构造带工作站默认受管条目夹具的 `ChooserGrantBroker`（同时覆盖 Windows 与 Unix 宿主路径负例边界）。
    #[must_use]
    pub fn with_default_vault() -> Self {
        let mut broker = Self {
            vault: BTreeMap::new(),
            sessions: BTreeMap::new(),
            grants: BTreeMap::new(),
            active_chooser_session_id: None,
            next_nonce: 0,
        };
        let defaults = [
            (
                [0x11; 16],
                sha256_bytes(b"artifact:q3-financial-ledger"),
                r"C:\Users\operator\Documents\Finance\q3-financial-ledger.csv",
                "工作文档",
                "q3-financial-ledger.csv",
                "text/csv",
                48_120_u64,
            ),
            (
                [0x22; 16],
                sha256_bytes(b"artifact:design-review-brief"),
                r"C:\Users\operator\Documents\Specs\design-review-brief.md",
                "工作文档",
                "design-review-brief.md",
                "text/markdown",
                12_480_u64,
            ),
            (
                [0x33; 16],
                sha256_bytes(b"artifact:unselected-ssh-private-key"),
                "/home/operator/.ssh/id_ed25519",
                "私密凭据库",
                "id_ed25519",
                "application/octet-stream",
                411_u64,
            ),
            (
                [0x44; 16],
                sha256_bytes(b"artifact:unselected-recovery-seed"),
                r"C:\Users\operator\AppData\Roaming\Secrets\master-recovery.seed",
                "私密凭据库",
                "master-recovery.seed",
                "application/octet-stream",
                256_u64,
            ),
        ];
        for (item_id, artifact_id, host_path, collection, leaf, mime, size) in defaults {
            if let Ok(entry) = HostVaultEntry::new(
                item_id,
                artifact_id,
                host_path,
                collection,
                leaf,
                mime,
                size,
            ) {
                broker.vault.insert(item_id, entry);
            }
        }
        broker
    }

    #[must_use]
    pub const fn active_chooser_session_id(&self) -> Option<[u8; 16]> {
        self.active_chooser_session_id
    }

    /// 向 TCB 保管库注册测试或运行时宿主条目。
    pub fn register_vault_entry(&mut self, entry: HostVaultEntry) {
        self.vault.insert(entry.vault_item_id, entry);
    }

    /// 仅供 TCB 内部解析受限句柄对应的宿主条目（绝不通过 IPC 返回给应用）。
    #[must_use]
    pub fn tcb_resolve_backing_entry(
        &self,
        grant_id: [u8; 16],
        scoped_handle_id: [u8; 16],
    ) -> Option<&HostVaultEntry> {
        let grant = self.grants.get(&grant_id)?;
        let vault_id = (*grant.handle_to_vault_item.get(&scoped_handle_id)?)?;
        self.vault.get(&vault_id)
    }

    /// 会话锁屏、挂起、切换用户或关闭时吊销挂起选择器与旧代际会话级 `ChooserGrant`。
    pub fn sync_with_session(
        &mut self,
        session_state: DesktopSessionState,
        session_generation: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) {
        for session_rec in self.sessions.values_mut() {
            if !session_rec.resolved
                && (session_state != DesktopSessionState::Active
                    || session_rec.session_generation != session_generation)
            {
                session_rec.resolved = true;
                if self.active_chooser_session_id == Some(session_rec.chooser_session_id) {
                    self.active_chooser_session_id = None;
                }
            }
        }
        for grant in self.grants.values_mut() {
            if !grant.revoked
                && (session_state != DesktopSessionState::Active
                    || grant.dto.session_generation != session_generation)
            {
                grant.revoked = true;
                audit.chooser_grants_revoked = audit.chooser_grants_revoked.saturating_add(1);
            }
        }
    }

    /// 在 `Z4` 平面打开系统级 `Open / Save / Share` 选择器会话。
    ///
    /// # Errors
    ///
    /// 不可信来源直接调用、非活动会话或请求权限超出模式上限时返回 [`TrustedUiError`]。
    #[allow(clippy::too_many_arguments)]
    pub fn open_chooser_session(
        &mut self,
        caller: TrustedUiCallerOrigin,
        session: &DesktopSessionManager,
        requesting_application_id: [u8; 16],
        requesting_app_display_name: impl Into<String>,
        mode: ChooserMode,
        requested_rights: ChooserAttenuatedRights,
        ttl_ms: u64,
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<TcbChooserSessionDto, TrustedUiError> {
        if !matches!(caller, TrustedUiCallerOrigin::TcbTrustedBroker) {
            audit.rejected_chooser_boundary_violations =
                audit.rejected_chooser_boundary_violations.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::UntrustedZ4ForgeAttempt,
                "untrusted Z2 surface cannot directly instantiate TCB chooser catalog; must broker via SystemControl/TCB",
            ));
        }
        if session.state() != DesktopSessionState::Active {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                "cannot open system chooser while desktop session is not ACTIVE",
            ));
        }
        if !mode.allows_rights(requested_rights) {
            audit.rejected_chooser_boundary_violations =
                audit.rejected_chooser_boundary_violations.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ChooserRightsExceedModeCeiling,
                format!(
                    "chooser mode {} forbids attenuated rights {}",
                    mode.as_str(),
                    requested_rights.as_str()
                ),
            ));
        }
        let app_label = requesting_app_display_name.into().trim().to_owned();
        if app_label.is_empty() {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                "requesting_app_display_name must not be empty",
            ));
        }

        self.next_nonce = self.next_nonce.saturating_add(1);
        let session_gen = session.session_generation();
        let chooser_session_id = derive_id_16(
            b"llmos/stage-d/chooser-session/v1",
            &[
                &self.next_nonce.to_be_bytes(),
                &session_gen.to_be_bytes(),
                &now_mono_ms.to_be_bytes(),
                &requesting_application_id,
            ],
        );

        if let Some(prev_id) = self.active_chooser_session_id.take()
            && let Some(prev) = self.sessions.get_mut(&prev_id)
        {
            prev.resolved = true;
        }

        let bounded_ttl = ttl_ms.clamp(1_000, 86_400_000);
        let rec = ChooserSessionRecord {
            chooser_session_id,
            requesting_application_id,
            mode,
            requested_rights,
            session_generation: session_gen,
            armed_from_mono_ms: now_mono_ms,
            ttl_ms: bounded_ttl,
            resolved: false,
        };
        self.sessions.insert(chooser_session_id, rec);
        self.active_chooser_session_id = Some(chooser_session_id);

        let catalog_items = self
            .vault
            .values()
            .map(|entry| TcbChooserCatalogItemDto {
                vault_item_id_hex: hex(&entry.vault_item_id),
                logical_collection_label: entry.logical_collection_label.clone(),
                leaf_display_name: entry.leaf_display_name.clone(),
                content_type: entry.content_type.clone(),
                size_bytes: entry.size_bytes,
            })
            .collect();

        let active_user = session.active_user().map_err(|err| {
            TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                err.to_string(),
            )
        })?;

        Ok(TcbChooserSessionDto {
            chooser_session_id_hex: hex(&chooser_session_id),
            requesting_application_id_hex: hex(&requesting_application_id),
            requesting_app_display_name: app_label,
            mode,
            requested_rights,
            z_index: Z4_SECURITY_Z_INDEX,
            security_seal_hint: active_user.z4_trusted_seal_hint().to_owned(),
            session_generation: session_gen,
            presented_at_mono_ms: now_mono_ms,
            armed_from_mono_ms: now_mono_ms,
            arming_window_ms: Z4_CLICKJACK_ARMING_MS,
            catalog_items,
        })
    }

    /// 取消系统选择器会话（`Escape` `0ms` 即时生效，向应用返回零句柄）。
    ///
    /// # Errors
    ///
    /// 非可信壳来源或会话不存在时返回 [`TrustedUiError`]。
    pub fn cancel_chooser_session(
        &mut self,
        origin: InputOrigin,
        chooser_session_id: [u8; 16],
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<(), TrustedUiError> {
        if !matches!(origin, InputOrigin::TrustedShell) {
            audit.rejected_untrusted_confirm_spoofs =
                audit.rejected_untrusted_confirm_spoofs.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::UntrustedInputOrigin,
                "untrusted origin cannot close system chooser session",
            ));
        }
        let rec = self.sessions.get_mut(&chooser_session_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::ChooserSessionNotFound,
                format!("chooser session {} not found", hex(&chooser_session_id)),
            )
        })?;
        rec.resolved = true;
        if self.active_chooser_session_id == Some(chooser_session_id) {
            self.active_chooser_session_id = None;
        }
        Ok(())
    }

    /// 用户在 `Z4` 系统选择器中确认选定对象，签发衰减后的 `ChooserGrantForAppDto`（零宿主裸路径泄露）。
    ///
    /// # Errors
    ///
    /// 伪造来源、`< 350ms` 武装窗内点击、未选有效条目或非法导出文件名时返回 [`TrustedUiError`]。
    #[allow(clippy::too_many_arguments)]
    pub fn confirm_chooser_grant(
        &mut self,
        origin: InputOrigin,
        source: InputSource,
        session: &DesktopSessionManager,
        chooser_session_id: [u8; 16],
        selected_vault_item_ids: &[[u8; 16]],
        export_save_leaf_name: Option<&str>,
        export_content_type: Option<&str>,
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<ChooserGrantForAppDto, TrustedUiError> {
        self.sync_with_session(session.state(), session.session_generation(), audit);

        if !matches!(origin, InputOrigin::TrustedShell) || source == InputSource::Automation {
            audit.rejected_untrusted_confirm_spoofs =
                audit.rejected_untrusted_confirm_spoofs.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::UntrustedInputOrigin,
                "untrusted surface or automated script forbidden from confirming ChooserGrant",
            ));
        }

        let rec = self.sessions.get(&chooser_session_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::ChooserSessionNotFound,
                format!("chooser session {} not found", hex(&chooser_session_id)),
            )
        })?;
        if rec.resolved {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ChallengeAlreadyResolved,
                format!(
                    "chooser session {} already resolved or invalidated",
                    hex(&chooser_session_id)
                ),
            ));
        }
        if session.state() != DesktopSessionState::Active
            || session.session_generation() != rec.session_generation
        {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                "session changed before chooser grant confirmation",
            ));
        }

        let elapsed = now_mono_ms.saturating_sub(rec.armed_from_mono_ms);
        if elapsed < Z4_CLICKJACK_ARMING_MS {
            audit.rejected_clickjacking_arming_attempts = audit
                .rejected_clickjacking_arming_attempts
                .saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ClickjackingArmingActive,
                format!(
                    "chooser confirm rejected during {Z4_CLICKJACK_ARMING_MS}ms arming window (elapsed {elapsed}ms)"
                ),
            ));
        }

        let requesting_app_id = rec.requesting_application_id;
        let mode = rec.mode;
        let rights = rec.requested_rights;
        let session_gen = rec.session_generation;
        let expires_at = now_mono_ms.saturating_add(rec.ttl_ms);

        self.next_nonce = self.next_nonce.saturating_add(1);
        let grant_id = derive_id_16(
            b"llmos/stage-d/chooser-grant-id/v1",
            &[
                &self.next_nonce.to_be_bytes(),
                &chooser_session_id,
                &requesting_app_id,
                &now_mono_ms.to_be_bytes(),
            ],
        );

        let mut scoped_handles = Vec::new();
        let mut handle_to_vault_item = BTreeMap::new();

        match mode {
            ChooserMode::Save => {
                let raw_name = export_save_leaf_name.ok_or_else(|| {
                    TrustedUiError::new(
                        TrustedUiErrorCode::ChooserInvalidExportName,
                        "SAVE chooser requires export_save_leaf_name",
                    )
                })?;
                let safe_leaf = sanitize_leaf_display_name(raw_name)?;
                let mime = export_content_type
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                let artifact_id = sha256_domain_digest(
                    b"llmos/stage-d/chooser-save-artifact/v1",
                    &[&grant_id, safe_leaf.as_bytes()],
                );
                let scoped_handle_id = derive_id_16(
                    b"llmos/stage-d/chooser-scoped-handle/v1",
                    &[&grant_id, &artifact_id, &[0u8]],
                );
                handle_to_vault_item.insert(scoped_handle_id, None);
                scoped_handles.push(ScopedChooserHandleDto {
                    scoped_handle_id_hex: hex(&scoped_handle_id),
                    artifact_id_hex: hex(&artifact_id),
                    display_name: safe_leaf,
                    content_type: mime,
                    size_bytes: 0,
                });
            }
            ChooserMode::Open | ChooserMode::Share => {
                if selected_vault_item_ids.is_empty() {
                    return Err(TrustedUiError::new(
                        TrustedUiErrorCode::InvalidArgument,
                        "OPEN/SHARE chooser confirmation requires at least one selected item",
                    ));
                }
                for (idx, item_id) in selected_vault_item_ids.iter().enumerate() {
                    let entry = self.vault.get(item_id).ok_or_else(|| {
                        audit.rejected_chooser_boundary_violations =
                            audit.rejected_chooser_boundary_violations.saturating_add(1);
                        TrustedUiError::new(
                            TrustedUiErrorCode::ChooserItemNotInVault,
                            format!("selected item {} is not in TCB vault", hex(item_id)),
                        )
                    })?;
                    let scoped_handle_id = derive_id_16(
                        b"llmos/stage-d/chooser-scoped-handle/v1",
                        &[&grant_id, &entry.artifact_id, &(idx as u64).to_be_bytes()],
                    );
                    handle_to_vault_item.insert(scoped_handle_id, Some(*item_id));
                    scoped_handles.push(ScopedChooserHandleDto {
                        scoped_handle_id_hex: hex(&scoped_handle_id),
                        artifact_id_hex: hex(&entry.artifact_id),
                        display_name: entry.leaf_display_name.clone(),
                        content_type: entry.content_type.clone(),
                        size_bytes: entry.size_bytes,
                    });
                }
            }
        }

        // 计算覆盖全部授权字段的域分离 SHA-256 摘要。
        let mut handle_concat = Vec::new();
        for h in &scoped_handles {
            handle_concat.extend_from_slice(h.scoped_handle_id_hex.as_bytes());
            handle_concat.push(b':');
            handle_concat.extend_from_slice(h.artifact_id_hex.as_bytes());
            handle_concat.push(b';');
        }
        let grant_digest = sha256_domain_digest(
            b"llmos/stage-d/chooser-grant/v1",
            &[
                &grant_id,
                &requesting_app_id,
                mode.as_str().as_bytes(),
                rights.as_str().as_bytes(),
                &handle_concat,
                &session_gen.to_be_bytes(),
                &now_mono_ms.to_be_bytes(),
                &expires_at.to_be_bytes(),
            ],
        );

        let dto = ChooserGrantForAppDto {
            grant_id_hex: hex(&grant_id),
            requesting_application_id_hex: hex(&requesting_app_id),
            mode,
            attenuated_rights: rights,
            scoped_handles,
            session_generation: session_gen,
            issued_at_mono_ms: now_mono_ms,
            expires_at_mono_ms: expires_at,
            grant_digest_hex: hex(&grant_digest),
        };

        if let Some(session_mut) = self.sessions.get_mut(&chooser_session_id) {
            session_mut.resolved = true;
        }
        if self.active_chooser_session_id == Some(chooser_session_id) {
            self.active_chooser_session_id = None;
        }

        self.grants.insert(
            grant_id,
            IssuedChooserGrantRecord {
                dto: dto.clone(),
                requesting_application_id: requesting_app_id,
                revoked: false,
                handle_to_vault_item,
            },
        );
        audit.chooser_grants_issued = audit.chooser_grants_issued.saturating_add(1);
        Ok(dto)
    }

    /// 校验并消费受限 `ChooserGrant`：验证会话代际、未吊销、未过期、应用身份绑定、受限句柄归属与权限上限。
    ///
    /// # Errors
    ///
    /// 任一边界不满足时返回对应类型化 [`TrustedUiError`]。
    #[allow(clippy::too_many_arguments)]
    pub fn exercise_chooser_grant(
        &mut self,
        grant_id: [u8; 16],
        caller_application_id: [u8; 16],
        scoped_handle_id: [u8; 16],
        required_right: ChooserAttenuatedRights,
        session: &DesktopSessionManager,
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<ExercisedChooserHandleDto, TrustedUiError> {
        self.sync_with_session(session.state(), session.session_generation(), audit);

        let record = self.grants.get(&grant_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::ChooserGrantNotFound,
                format!("ChooserGrant {} not found", hex(&grant_id)),
            )
        })?;
        if record.revoked {
            audit.rejected_chooser_boundary_violations =
                audit.rejected_chooser_boundary_violations.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ChooserGrantRevoked,
                format!("ChooserGrant {} has been revoked", hex(&grant_id)),
            ));
        }
        if now_mono_ms >= record.dto.expires_at_mono_ms {
            audit.rejected_chooser_boundary_violations =
                audit.rejected_chooser_boundary_violations.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ChooserGrantExpired,
                format!(
                    "ChooserGrant {} expired at {}ms",
                    hex(&grant_id),
                    record.dto.expires_at_mono_ms
                ),
            ));
        }
        if record.requesting_application_id != caller_application_id {
            audit.rejected_chooser_boundary_violations =
                audit.rejected_chooser_boundary_violations.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ChooserGrantApplicationMismatch,
                format!(
                    "ChooserGrant {} belongs to app {}, rejected caller app {}",
                    hex(&grant_id),
                    hex(&record.requesting_application_id),
                    hex(&caller_application_id)
                ),
            ));
        }
        if !record.dto.attenuated_rights.satisfies(required_right) {
            audit.rejected_chooser_boundary_violations =
                audit.rejected_chooser_boundary_violations.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::ChooserGrantInsufficientRights,
                format!(
                    "ChooserGrant {} holds {}, insufficient for {}",
                    hex(&grant_id),
                    record.dto.attenuated_rights.as_str(),
                    required_right.as_str()
                ),
            ));
        }

        let target_hex = hex(&scoped_handle_id);
        let handle = record
            .dto
            .scoped_handles
            .iter()
            .find(|h| h.scoped_handle_id_hex == target_hex)
            .ok_or_else(|| {
                audit.rejected_chooser_boundary_violations =
                    audit.rejected_chooser_boundary_violations.saturating_add(1);
                TrustedUiError::new(
                    TrustedUiErrorCode::ChooserGrantHandleOutOfScope,
                    format!(
                        "handle {} is outside ChooserGrant {} scope",
                        target_hex,
                        hex(&grant_id)
                    ),
                )
            })?;

        Ok(ExercisedChooserHandleDto {
            grant_id_hex: record.dto.grant_id_hex.clone(),
            scoped_handle_id_hex: handle.scoped_handle_id_hex.clone(),
            artifact_id_hex: handle.artifact_id_hex.clone(),
            granted_right: required_right,
            display_name: handle.display_name.clone(),
            content_type: handle.content_type.clone(),
            size_bytes: handle.size_bytes,
        })
    }

    /// 显式吊销已签发的 `ChooserGrant`。
    ///
    /// # Errors
    ///
    /// 未知 `grant_id` 返回 [`TrustedUiErrorCode::ChooserGrantNotFound`]。
    pub fn revoke_chooser_grant(
        &mut self,
        grant_id: [u8; 16],
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<(), TrustedUiError> {
        let record = self.grants.get_mut(&grant_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::ChooserGrantNotFound,
                format!("ChooserGrant {} not found", hex(&grant_id)),
            )
        })?;
        if !record.revoked {
            record.revoked = true;
            audit.chooser_grants_revoked = audit.chooser_grants_revoked.saturating_add(1);
        }
        Ok(())
    }
}

// ============================================================================
// 3. Taint-Aware Cross-App Clipboard & Drag-Drop Guard ([DUI-CLIP-001])
// ============================================================================

/// 跨应用传输通道类型（剪贴板 vs 拖放）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransferChannelKind {
    Clipboard,
    DragDrop,
}

impl TransferChannelKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clipboard => "CLIPBOARD",
            Self::DragDrop => "DRAG_DROP",
        }
    }

    /// 解析通道名称。
    ///
    /// # Errors
    ///
    /// 未知通道返回 [`TrustedUiError`]。
    pub fn parse(raw: &str) -> Result<Self, TrustedUiError> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "CLIPBOARD" => Ok(Self::Clipboard),
            "DRAG_DROP" | "DRAG-DROP" | "DRAGDROP" => Ok(Self::DragDrop),
            other => Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!("unknown transfer channel: {other}"),
            )),
        }
    }
}

/// 数据污点与机密分级标签（`[DUI-CLIP-001]`）。
#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaintFlag {
    Public,
    WorkspaceInternal,
    Sensitive,
    Confidential,
    Credentials,
    UntrustedExternal,
}

impl TaintFlag {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "PUBLIC",
            Self::WorkspaceInternal => "WORKSPACE_INTERNAL",
            Self::Sensitive => "SENSITIVE",
            Self::Confidential => "CONFIDENTIAL",
            Self::Credentials => "CREDENTIALS",
            Self::UntrustedExternal => "UNTRUSTED_EXTERNAL",
        }
    }

    /// 解析污点标签。
    ///
    /// # Errors
    ///
    /// 未知标签返回 [`TrustedUiError`]。
    pub fn parse(raw: &str) -> Result<Self, TrustedUiError> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "PUBLIC" => Ok(Self::Public),
            "WORKSPACE_INTERNAL" => Ok(Self::WorkspaceInternal),
            "SENSITIVE" => Ok(Self::Sensitive),
            "CONFIDENTIAL" => Ok(Self::Confidential),
            "CREDENTIALS" => Ok(Self::Credentials),
            "UNTRUSTED_EXTERNAL" => Ok(Self::UntrustedExternal),
            other => Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                format!("unknown taint flag: {other}"),
            )),
        }
    }

    /// 跨应用粘贴或拖放时是否必须升级至 `Z4` 系统安全确认屏。
    #[must_use]
    pub const fn requires_cross_app_z4_escalation(self) -> bool {
        matches!(
            self,
            Self::Sensitive | Self::Confidential | Self::Credentials | Self::UntrustedExternal
        )
    }

    /// 是否属于高危机密/凭据泄露（一旦释放给外部应用即不可逆）。
    #[must_use]
    pub const fn is_irreversible_disclosure(self) -> bool {
        matches!(self, Self::Confidential | Self::Credentials)
    }

    #[must_use]
    pub const fn plain_label_zh(self) -> &'static str {
        match self {
            Self::Public => "公开内容",
            Self::WorkspaceInternal => "工作区内部内容",
            Self::Sensitive => "敏感业务内容",
            Self::Confidential => "机密受控内容",
            Self::Credentials => "账号密钥与凭据",
            Self::UntrustedExternal => "外部未核验来源内容",
        }
    }

    #[must_use]
    pub const fn plain_label_en(self) -> &'static str {
        match self {
            Self::Public => "Public content",
            Self::WorkspaceInternal => "Workspace internal content",
            Self::Sensitive => "Sensitive business content",
            Self::Confidential => "Confidential controlled content",
            Self::Credentials => "Account keys and credentials",
            Self::UntrustedExternal => "Untrusted external content",
        }
    }
}

/// 一次性硬件用户手势令牌（防后台脚本静默读取剪贴板或伪造拖放）。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserGestureTokenDto {
    pub gesture_token_id_hex: String,
    pub actor_application_id_hex: String,
    pub channel: TransferChannelKind,
    pub input_source: InputSource,
    pub session_generation: u64,
    pub issued_at_mono_ms: u64,
    pub expires_at_mono_ms: u64,
}

#[derive(Clone, Debug)]
struct UserGestureTokenRecord {
    actor_application_id: [u8; 16],
    channel: TransferChannelKind,
    session_generation: u64,
    expires_at_mono_ms: u64,
    consumed: bool,
}

/// 已登记的剪贴板条目或拖放传输元数据回执（不含原始载荷字节）。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferMetadataDto {
    pub transfer_id_hex: String,
    pub channel: TransferChannelKind,
    pub content_type: String,
    pub payload_digest_hex: String,
    pub byte_length: usize,
    pub source_application_id_hex: String,
    pub source_app_display_name: String,
    pub taint_flags: Vec<TaintFlag>,
    pub session_generation: u64,
    pub created_at_mono_ms: u64,
    pub expires_at_mono_ms: u64,
    pub user_gesture_token_id_hex: String,
}

/// 跨应用剪贴板/拖放读取裁决结果。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TransferReadDecisionDto {
    /// 同源或非敏感公开内容：校验有效用户手势后直接释放字节。
    #[serde(rename_all = "camelCase")]
    Released {
        transfer_id_hex: String,
        channel: TransferChannelKind,
        content_type: String,
        payload_digest_hex: String,
        payload_utf8_or_hex: String,
        byte_length: usize,
        taint_flags: Vec<TaintFlag>,
    },
    /// 跨应用污点内容：已阻断字节释放并自动升级打开 `Z4` 安全确认挑战。
    #[serde(rename_all = "camelCase")]
    EscalatedToZ4Consent {
        transfer_id_hex: String,
        challenge_id_hex: String,
        channel: TransferChannelKind,
        source_application_id_hex: String,
        target_application_id_hex: String,
        taint_flags: Vec<TaintFlag>,
        requires_high_contrast_ack: bool,
    },
}

#[derive(Clone, Debug)]
struct TransferEnvelopeRecord {
    transfer_id: [u8; 16],
    channel: TransferChannelKind,
    content_type: String,
    payload_digest: [u8; 32],
    payload_bytes: Vec<u8>,
    source_application_id: [u8; 16],
    source_app_display_name: String,
    taint_flags: Vec<TaintFlag>,
    session_generation: u64,
    expires_at_mono_ms: u64,
}

#[derive(Clone, Debug)]
struct PendingTransferEscalation {
    transfer_id: [u8; 16],
    target_application_id: [u8; 16],
    session_generation: u64,
}

/// 污点感知剪贴板与拖放守卫 (`ClipboardAndDragBroker`)。
#[derive(Debug, Default)]
pub struct ClipboardAndDragBroker {
    gestures: BTreeMap<[u8; 16], UserGestureTokenRecord>,
    transfers: BTreeMap<[u8; 16], TransferEnvelopeRecord>,
    pending_escalations: BTreeMap<[u8; 16], PendingTransferEscalation>,
    latest_clipboard_transfer_id: Option<[u8; 16]>,
    next_nonce: u64,
}

impl ClipboardAndDragBroker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn latest_clipboard_transfer_id(&self) -> Option<[u8; 16]> {
        self.latest_clipboard_transfer_id
    }

    /// 会话代际变化或非 `ACTIVE` 时清理旧代际手势令牌与待决跨应用升级记录。
    pub fn sync_with_session(
        &mut self,
        session_state: DesktopSessionState,
        session_generation: u64,
    ) {
        if session_state != DesktopSessionState::Active {
            self.pending_escalations.clear();
            for gesture in self.gestures.values_mut() {
                gesture.consumed = true;
            }
            return;
        }
        self.pending_escalations
            .retain(|_, esc| esc.session_generation == session_generation);
        for gesture in self.gestures.values_mut() {
            if gesture.session_generation != session_generation {
                gesture.consumed = true;
            }
        }
    }

    /// 由宿主可信输入路由签发一次性用户手势令牌（严禁 `Automation` 脚本通道铸造）。
    ///
    /// # Errors
    ///
    /// 非 `TrustedShell`、`Automation` 来源或非活动会话返回 [`TrustedUiError`]。
    #[allow(clippy::too_many_arguments)]
    pub fn mint_user_gesture_token(
        &mut self,
        origin: InputOrigin,
        source: InputSource,
        actor_application_id: [u8; 16],
        channel: TransferChannelKind,
        session: &DesktopSessionManager,
        now_mono_ms: u64,
        ttl_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<UserGestureTokenDto, TrustedUiError> {
        if !matches!(origin, InputOrigin::TrustedShell) {
            audit.rejected_missing_gesture_reads =
                audit.rejected_missing_gesture_reads.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::UntrustedInputOrigin,
                "untrusted surface cannot self-mint user_gesture_token",
            ));
        }
        if source == InputSource::Automation {
            audit.rejected_missing_gesture_reads =
                audit.rejected_missing_gesture_reads.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::AutomationGestureForbidden,
                "automated input channel cannot mint human clipboard/drag-drop gesture tokens",
            ));
        }
        if session.state() != DesktopSessionState::Active {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                "cannot mint gesture token while session is not ACTIVE",
            ));
        }

        self.next_nonce = self.next_nonce.saturating_add(1);
        let session_gen = session.session_generation();
        let bounded_ttl = ttl_ms.clamp(250, 30_000);
        let expires_at = now_mono_ms.saturating_add(bounded_ttl);
        let token_id = derive_id_16(
            b"llmos/stage-d/user-gesture-token/v1",
            &[
                &self.next_nonce.to_be_bytes(),
                &actor_application_id,
                channel.as_str().as_bytes(),
                &session_gen.to_be_bytes(),
                &now_mono_ms.to_be_bytes(),
            ],
        );

        let record = UserGestureTokenRecord {
            actor_application_id,
            channel,
            session_generation: session_gen,
            expires_at_mono_ms: expires_at,
            consumed: false,
        };
        self.gestures.insert(token_id, record);

        Ok(UserGestureTokenDto {
            gesture_token_id_hex: hex(&token_id),
            actor_application_id_hex: hex(&actor_application_id),
            channel,
            input_source: source,
            session_generation: session_gen,
            issued_at_mono_ms: now_mono_ms,
            expires_at_mono_ms: expires_at,
        })
    }

    fn consume_gesture_token(
        &mut self,
        gesture_token_id: Option<[u8; 16]>,
        expected_actor_app_id: [u8; 16],
        expected_channel: TransferChannelKind,
        session_generation: u64,
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<[u8; 16], TrustedUiError> {
        let token_id = gesture_token_id.ok_or_else(|| {
            audit.rejected_missing_gesture_reads =
                audit.rejected_missing_gesture_reads.saturating_add(1);
            TrustedUiError::new(
                TrustedUiErrorCode::MissingUserGesture,
                "clipboard/drag-drop operation rejected: missing fresh user_gesture_token",
            )
        })?;

        let token = self.gestures.get_mut(&token_id).ok_or_else(|| {
            audit.rejected_missing_gesture_reads =
                audit.rejected_missing_gesture_reads.saturating_add(1);
            TrustedUiError::new(
                TrustedUiErrorCode::InvalidOrExpiredUserGesture,
                format!("user_gesture_token {} is unknown", hex(&token_id)),
            )
        })?;

        if token.consumed
            || token.actor_application_id != expected_actor_app_id
            || token.channel != expected_channel
            || token.session_generation != session_generation
            || now_mono_ms >= token.expires_at_mono_ms
        {
            audit.rejected_missing_gesture_reads =
                audit.rejected_missing_gesture_reads.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidOrExpiredUserGesture,
                format!(
                    "user_gesture_token {} is consumed, expired, or bound to a different application/channel",
                    hex(&token_id)
                ),
            ));
        }

        token.consumed = true;
        Ok(token_id)
    }

    /// 登记一条剪贴板复制或拖放启动传输，强绑定源应用、污点集合、摘要、TTL 与用户手势。
    ///
    /// # Errors
    ///
    /// 缺少有效用户手势、非活动会话或参数为空时返回 [`TrustedUiError`]。
    #[allow(clippy::too_many_arguments)]
    pub fn publish_transfer(
        &mut self,
        session: &DesktopSessionManager,
        channel: TransferChannelKind,
        content_type: impl Into<String>,
        payload_bytes: Vec<u8>,
        source_application_id: [u8; 16],
        source_app_display_name: impl Into<String>,
        mut taint_flags: Vec<TaintFlag>,
        gesture_token_id: Option<[u8; 16]>,
        ttl_ms: u64,
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<TransferMetadataDto, TrustedUiError> {
        if session.state() != DesktopSessionState::Active {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                "cannot publish clipboard/drag-drop transfer while session is not ACTIVE",
            ));
        }
        let content_type = content_type.into().trim().to_owned();
        let source_app_display_name = source_app_display_name.into().trim().to_owned();
        if content_type.is_empty() || source_app_display_name.is_empty() {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                "content_type and source_app_display_name must not be empty",
            ));
        }

        let session_gen = session.session_generation();
        let consumed_token = self.consume_gesture_token(
            gesture_token_id,
            source_application_id,
            channel,
            session_gen,
            now_mono_ms,
            audit,
        )?;

        if taint_flags.is_empty() {
            taint_flags.push(TaintFlag::WorkspaceInternal);
        } else {
            taint_flags.sort();
            taint_flags.dedup();
        }

        let payload_digest =
            sha256_domain_digest(b"llmos/stage-d/transfer-payload/v1", &[&payload_bytes]);
        self.next_nonce = self.next_nonce.saturating_add(1);
        let transfer_id = derive_id_16(
            b"llmos/stage-d/transfer-id/v1",
            &[
                &self.next_nonce.to_be_bytes(),
                &source_application_id,
                &payload_digest,
                &now_mono_ms.to_be_bytes(),
            ],
        );
        let bounded_ttl = ttl_ms.clamp(500, 3_600_000);
        let expires_at = now_mono_ms.saturating_add(bounded_ttl);
        let byte_length = payload_bytes.len();

        let record = TransferEnvelopeRecord {
            transfer_id,
            channel,
            content_type: content_type.clone(),
            payload_digest,
            payload_bytes,
            source_application_id,
            source_app_display_name: source_app_display_name.clone(),
            taint_flags: taint_flags.clone(),
            session_generation: session_gen,
            expires_at_mono_ms: expires_at,
        };
        self.transfers.insert(transfer_id, record);
        if channel == TransferChannelKind::Clipboard {
            self.latest_clipboard_transfer_id = Some(transfer_id);
        }
        audit.transfers_published = audit.transfers_published.saturating_add(1);

        Ok(TransferMetadataDto {
            transfer_id_hex: hex(&transfer_id),
            channel,
            content_type,
            payload_digest_hex: hex(&payload_digest),
            byte_length,
            source_application_id_hex: hex(&source_application_id),
            source_app_display_name,
            taint_flags,
            session_generation: session_gen,
            created_at_mono_ms: now_mono_ms,
            expires_at_mono_ms: expires_at,
            user_gesture_token_id_hex: hex(&consumed_token),
        })
    }

    /// 请求读取剪贴板条目或接收拖放载荷：
    /// 1. 后台无手势读取一律拒绝 (`MissingUserGesture`)；
    /// 2. 已过期或跨会话代际条目一律拒绝 (`TransferExpired`)；
    /// 3. 跨应用且带 `SENSITIVE | CONFIDENTIAL | CREDENTIALS | UNTRUSTED_EXTERNAL` 污点时，
    ///    自动在 `TrustedUiBroker` 上弹出 `Z4` 安全确认挑战，在获批前释放 **0 字节**。
    ///
    /// # Errors
    ///
    /// 无手势、手势伪造/重放、传输不存在或已过期时返回对应 [`TrustedUiError`]。
    #[allow(clippy::too_many_arguments)]
    pub fn request_transfer_read(
        &mut self,
        z4_broker: &mut TrustedUiBroker,
        session: &DesktopSessionManager,
        transfer_id: [u8; 16],
        target_application_id: [u8; 16],
        target_installation_id: [u8; 16],
        target_app_display_name: impl Into<String>,
        gesture_token_id: Option<[u8; 16]>,
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<TransferReadDecisionDto, TrustedUiError> {
        self.sync_with_session(session.state(), session.session_generation());
        if session.state() != DesktopSessionState::Active {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                "cannot read clipboard/drag-drop transfer while session is not ACTIVE",
            ));
        }

        let target_app_display_name = target_app_display_name.into().trim().to_owned();
        if target_app_display_name.is_empty() {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::InvalidArgument,
                "target_app_display_name must not be empty",
            ));
        }

        let transfer_channel = self
            .transfers
            .get(&transfer_id)
            .map(|t| t.channel)
            .ok_or_else(|| {
                TrustedUiError::new(
                    TrustedUiErrorCode::TransferNotFound,
                    format!("transfer {} not found", hex(&transfer_id)),
                )
            })?;

        let session_gen = session.session_generation();
        let _ = self.consume_gesture_token(
            gesture_token_id,
            target_application_id,
            transfer_channel,
            session_gen,
            now_mono_ms,
            audit,
        )?;

        let transfer = self.transfers.get(&transfer_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::TransferNotFound,
                format!("transfer {} not found", hex(&transfer_id)),
            )
        })?;

        if transfer.session_generation != session_gen || now_mono_ms >= transfer.expires_at_mono_ms
        {
            audit.rejected_expired_transfer_reads =
                audit.rejected_expired_transfer_reads.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::TransferExpired,
                format!(
                    "transfer {} expired at {}ms (now {}ms) or belongs to stale session",
                    hex(&transfer_id),
                    transfer.expires_at_mono_ms,
                    now_mono_ms
                ),
            ));
        }

        let is_cross_app = target_application_id != transfer.source_application_id;
        let requires_escalation = is_cross_app
            && transfer
                .taint_flags
                .iter()
                .any(|flag| flag.requires_cross_app_z4_escalation());

        if !requires_escalation {
            return Ok(build_released_decision(transfer));
        }

        let is_irreversible = transfer
            .taint_flags
            .iter()
            .any(|flag| flag.is_irreversible_disclosure());
        let reversibility_class = if is_irreversible {
            ReversibilityClass::Irreversible
        } else {
            ReversibilityClass::CompensatableRollback
        };

        let taint_zh: Vec<&str> = transfer
            .taint_flags
            .iter()
            .map(|f| f.plain_label_zh())
            .collect();
        let taint_en: Vec<&str> = transfer
            .taint_flags
            .iter()
            .map(|f| f.plain_label_en())
            .collect();
        let channel_zh = match transfer.channel {
            TransferChannelKind::Clipboard => "剪贴板粘贴",
            TransferChannelKind::DragDrop => "拖放传入",
        };
        let channel_en = match transfer.channel {
            TransferChannelKind::Clipboard => "clipboard paste",
            TransferChannelKind::DragDrop => "drag-and-drop",
        };

        let principal_hex = session
            .active_user()
            .map_err(|err| {
                TrustedUiError::new(
                    TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                    err.to_string(),
                )
            })?
            .principal_id_hex();
        let manifest_digest = sha256_domain_digest(
            b"llmos/stage-d/transfer-escalation-manifest/v1",
            &[&target_application_id, &target_installation_id],
        );

        let challenge_req = Z4ConsentChallengeRequest {
            requesting_app: Z4RequestingAppSpec {
                display_name: target_app_display_name.clone(),
                publisher_name: "已核验本地安装应用".to_owned(),
                signature_verified: true,
                application_id_hex: hex(&target_application_id),
                installation_id_hex: hex(&target_installation_id),
                principal_id_hex: principal_hex,
                installation_generation: 1,
                manifest_digest_hex: hex(&manifest_digest),
            },
            action_scope: Z4ActionScopeSpec {
                plain_summary_zh: format!(
                    "允许「{}」通过{}读取来自「{}」的受控内容（{}）",
                    target_app_display_name,
                    channel_zh,
                    transfer.source_app_display_name,
                    taint_zh.join("、")
                ),
                plain_summary_en: format!(
                    "Allow \"{}\" to read {} from \"{}\" ({})",
                    target_app_display_name,
                    channel_en,
                    transfer.source_app_display_name,
                    taint_en.join(", ")
                ),
                capability_rights: vec![
                    format!("CROSS_APP_{}_READ", transfer.channel.as_str()),
                    format!(
                        "TAINT_RELEASE({})",
                        transfer
                            .taint_flags
                            .iter()
                            .map(|f| f.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                ],
                control_scope: "CROSS_APPLICATION_TRANSFER".to_owned(),
                plan_digest_hex: hex(&transfer.payload_digest),
            },
            resource_ceiling: Z4ResourceCeilingSpec {
                plain_ceiling_zh: format!(
                    "单次传入 {} 字节（类型 {}），不授予后台持续读取权限",
                    transfer.payload_bytes.len(),
                    transfer.content_type
                ),
                plain_ceiling_en: format!(
                    "One-time transfer of {} bytes ({}), zero background polling access",
                    transfer.payload_bytes.len(),
                    transfer.content_type
                ),
                memory_ceiling_mib: 1,
                token_budget_ceiling: 0,
                network_egress_allowed: false,
                max_duration_ms: 5_000,
            },
            reversibility_and_risk: Z4ReversibilityRiskSpec {
                reversibility_class,
                plain_consequence_zh: if is_irreversible {
                    "机密或凭据内容一旦传递给目标应用即无法收回，也无法事后撤销已读取的数据。"
                        .to_owned()
                } else {
                    "受控敏感或外部内容将进入目标应用，系统会记录此次跨应用流转审计记录。"
                        .to_owned()
                },
                plain_consequence_en: if is_irreversible {
                    "Once confidential or credential bytes are delivered to the target app, disclosure cannot be undone."
                        .to_owned()
                } else {
                    "Controlled sensitive or external content will enter the target app and be recorded in the audit log."
                        .to_owned()
                },
                multi_target_count: 1,
                partial_failure_disclosure_zh: None,
                partial_failure_disclosure_en: None,
                blast_radius_rows: vec![Z4BlastRadiusRowDto {
                    symbol: "~".to_owned(),
                    category: "reverified".to_owned(),
                    scope_label: channel_zh.to_owned(),
                    target_label: target_app_display_name,
                    reason: taint_zh.join(" / "),
                    target_id_hex: Some(hex(&target_application_id)),
                }],
                requires_data_loss_receipt: false,
            },
            allowed_grant_scopes: vec![ConsentGrantScope::SingleAction],
        };

        let challenge_dto = z4_broker.open_consent_challenge(
            TrustedUiCallerOrigin::TcbTrustedBroker,
            session,
            challenge_req,
            now_mono_ms,
            audit,
        )?;
        let challenge_id = parse_hex_id_16("challenge_id_hex", &challenge_dto.challenge_id_hex)?;

        self.pending_escalations.insert(
            challenge_id,
            PendingTransferEscalation {
                transfer_id,
                target_application_id,
                session_generation: session_gen,
            },
        );
        audit.escalated_tainted_transfers = audit.escalated_tainted_transfers.saturating_add(1);

        Ok(TransferReadDecisionDto::EscalatedToZ4Consent {
            transfer_id_hex: hex(&transfer_id),
            challenge_id_hex: challenge_dto.challenge_id_hex,
            channel: transfer.channel,
            source_application_id_hex: hex(&transfer.source_application_id),
            target_application_id_hex: hex(&target_application_id),
            taint_flags: transfer.taint_flags.clone(),
            requires_high_contrast_ack: challenge_dto.requires_high_contrast_ack,
        })
    }

    /// 在用户在 `Z4` 确认放行后，向目标应用完成受控污点数据的单次释放。
    ///
    /// # Errors
    ///
    /// 对应 `Z4` 挑战未批准、已被 `Escape` 取消、会话锁屏失效或传输过期时返回 [`TrustedUiError`]。
    #[allow(clippy::too_many_arguments)]
    pub fn complete_escalated_transfer_after_z4(
        &mut self,
        z4_broker: &TrustedUiBroker,
        session: &DesktopSessionManager,
        transfer_id: [u8; 16],
        target_application_id: [u8; 16],
        challenge_id: [u8; 16],
        now_mono_ms: u64,
        audit: &mut TrustedUiSecurityAuditCounters,
    ) -> Result<TransferReadDecisionDto, TrustedUiError> {
        self.sync_with_session(session.state(), session.session_generation());
        if session.state() != DesktopSessionState::Active {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::SessionNotActiveOrStaleGeneration,
                "session is not ACTIVE; escalated transfer blocked",
            ));
        }

        let esc = self.pending_escalations.get(&challenge_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::TransferConsentMismatchOrDenied,
                format!(
                    "no pending transfer escalation bound to Z4 challenge {}",
                    hex(&challenge_id)
                ),
            )
        })?;

        if esc.transfer_id != transfer_id
            || esc.target_application_id != target_application_id
            || esc.session_generation != session.session_generation()
        {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::TransferConsentMismatchOrDenied,
                "escalated transfer target/transfer/session binding mismatch",
            ));
        }

        let state = z4_broker.challenge_state(challenge_id);
        if state != Some(Z4ChallengeState::Approved) {
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::TransferConsentMismatchOrDenied,
                format!(
                    "Z4 challenge {} is not APPROVED (current state: {:?}); zero bytes released",
                    hex(&challenge_id),
                    state
                ),
            ));
        }

        let transfer = self.transfers.get(&transfer_id).ok_or_else(|| {
            TrustedUiError::new(
                TrustedUiErrorCode::TransferNotFound,
                format!("transfer {} not found", hex(&transfer_id)),
            )
        })?;
        if now_mono_ms >= transfer.expires_at_mono_ms {
            self.pending_escalations.remove(&challenge_id);
            audit.rejected_expired_transfer_reads =
                audit.rejected_expired_transfer_reads.saturating_add(1);
            return Err(TrustedUiError::new(
                TrustedUiErrorCode::TransferExpired,
                format!("transfer {} expired before Z4 release", hex(&transfer_id)),
            ));
        }

        self.pending_escalations.remove(&challenge_id);
        Ok(build_released_decision(transfer))
    }
}

fn build_released_decision(transfer: &TransferEnvelopeRecord) -> TransferReadDecisionDto {
    let payload_utf8_or_hex = match std::str::from_utf8(&transfer.payload_bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => hex(&transfer.payload_bytes),
    };
    TransferReadDecisionDto::Released {
        transfer_id_hex: hex(&transfer.transfer_id),
        channel: transfer.channel,
        content_type: transfer.content_type.clone(),
        payload_digest_hex: hex(&transfer.payload_digest),
        payload_utf8_or_hex,
        byte_length: transfer.payload_bytes.len(),
        taint_flags: transfer.taint_flags.clone(),
    }
}

// ============================================================================
// 4. Unified TrustedUiAuthority & Tauri Command Surface (Ready for W71-INT)
// ============================================================================

/// 阶段 D 可信 UI 聚合权威（`Z4` 确认屏 + `ChooserGrant` + 污点剪贴板/拖放 + 审计计数）。
#[derive(Debug, Default)]
pub struct TrustedUiAuthority {
    pub z4_broker: TrustedUiBroker,
    pub chooser_broker: ChooserGrantBroker,
    pub clipboard_drag_broker: ClipboardAndDragBroker,
    pub audit: TrustedUiSecurityAuditCounters,
}

impl TrustedUiAuthority {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn has_active_z4_surface(&self) -> bool {
        self.z4_broker.active_challenge_id().is_some()
            || self.chooser_broker.active_chooser_session_id().is_some()
    }

    /// 与宿主 `DesktopSessionManager` 对齐会话状态与代际，并同步合成壳 `z4_modal_lock`。
    pub fn sync_with_host_shell(&mut self, shell: &mut WorkstationHostShell, now_mono_ms: u64) {
        let state = shell.session.state();
        let generation = shell.session.session_generation();
        self.z4_broker
            .sync_with_session(state, generation, &mut self.audit);
        self.chooser_broker
            .sync_with_session(state, generation, &mut self.audit);
        self.clipboard_drag_broker
            .sync_with_session(state, generation);

        let should_lock_z4 = self.has_active_z4_surface();
        if shell.route.z4_modal_lock != should_lock_z4 {
            shell.set_z4_modal(should_lock_z4, now_mono_ms);
        }
    }

    /// 导出供 `Z0..Z3` 低信任层读取的公共态势（绝对不含 `security_seal_hint`）。
    #[must_use]
    pub fn project_lower_plane_posture(
        &self,
        session: &DesktopSessionManager,
    ) -> LowerPlaneTrustedPostureDto {
        LowerPlaneTrustedPostureDto {
            z4_modal_active: self.has_active_z4_surface(),
            z4_z_index: Z4_SECURITY_Z_INDEX,
            z2_z_ceiling: Z2_SURFACE_Z_CEILING,
            active_challenge_id_hex: self.z4_broker.active_challenge_id().map(|id| hex(&id)),
            active_chooser_session_id_hex: self
                .chooser_broker
                .active_chooser_session_id()
                .map(|id| hex(&id)),
            session_generation: session.session_generation(),
        }
    }
}

/// Tauri 托管线程安全 `TrustedUiState`（供 `W71-INT` 在 `lib.rs` 中 `.manage(TrustedUiState::default())`）。
#[derive(Debug, Default)]
pub struct TrustedUiState {
    inner: Mutex<TrustedUiAuthority>,
}

impl TrustedUiState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 在互斥保护下访问 `TrustedUiAuthority`（零 `unwrap`）。
    ///
    /// # Errors
    ///
    /// 锁中毒或闭包返回错误时返回 [`DesktopError`]。
    pub fn with_authority_mut<R>(
        &self,
        op: impl FnOnce(&mut TrustedUiAuthority) -> Result<R, DesktopError>,
    ) -> Result<R, DesktopError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| DesktopError::internal("TrustedUiState 锁中毒"))?;
        op(&mut guard)
    }
}

/// `_core`: 打开 `Z4` 授权挑战并自动锁闭合成器 `z4_modal_lock`。
///
/// # Errors
///
/// 欺骗拦截或参数错误返回 [`DesktopError`]。
pub fn trusted_ui_open_z4_consent_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    caller: TrustedUiCallerOrigin,
    request: Z4ConsentChallengeRequest,
    now_mono_ms: u64,
) -> Result<Z4TrustedChallengeDto, DesktopError> {
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            let dto = auth.z4_broker.open_consent_challenge(
                caller,
                &shell.session,
                request,
                now_mono_ms,
                &mut auth.audit,
            )?;
            shell.set_z4_modal(auth.has_active_z4_surface(), now_mono_ms);
            Ok(dto)
        })
    })
}

/// `_core`: 通知 `Z4` 焦点重获并重置 `350ms` 武装计时器。
///
/// # Errors
///
/// 挑战不存在或来源非法返回 [`DesktopError`]。
pub fn trusted_ui_notify_z4_focus_regained_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    caller: TrustedUiCallerOrigin,
    challenge_id_hex: &str,
    now_mono_ms: u64,
) -> Result<u64, DesktopError> {
    let challenge_id = parse_hex_id_16("challenge_id_hex", challenge_id_hex)?;
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            let armed_from = auth.z4_broker.notify_focus_regained(
                caller,
                challenge_id,
                now_mono_ms,
                &mut auth.audit,
            )?;
            shell.set_z4_modal(true, now_mono_ms);
            Ok(armed_from)
        })
    })
}

/// `_core`: 裁决 `Z4` 授权挑战并同步释放/维持 `z4_modal_lock`。
///
/// # Errors
///
/// 违反武装窗、欺骗防护或法定不可逆确认要求时返回 [`DesktopError`]。
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_decide_z4_consent_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    origin: InputOrigin,
    source: InputSource,
    challenge_id_hex: &str,
    confirm: bool,
    chosen_grant_scope: Option<ConsentGrantScope>,
    irreversible_high_contrast_ack: bool,
    now_mono_ms: u64,
) -> Result<Z4ConsentDecisionReceiptDto, DesktopError> {
    let challenge_id = parse_hex_id_16("challenge_id_hex", challenge_id_hex)?;
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            let receipt = auth.z4_broker.decide_consent_challenge(
                origin,
                source,
                &shell.session,
                challenge_id,
                confirm,
                chosen_grant_scope,
                irreversible_high_contrast_ack,
                now_mono_ms,
                &mut auth.audit,
            )?;
            shell.set_z4_modal(auth.has_active_z4_surface(), now_mono_ms);
            Ok(receipt)
        })
    })
}

/// `_core`: 读取 `Z0..Z3` 公共安全屏态势（零 `security_seal_hint`）。
///
/// # Errors
///
/// 锁中毒时返回 [`DesktopError`]。
pub fn trusted_ui_get_public_posture_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    now_mono_ms: u64,
) -> Result<LowerPlaneTrustedPostureDto, DesktopError> {
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            Ok(auth.project_lower_plane_posture(&shell.session))
        })
    })
}

/// `_core`: 打开系统 `ChooserGrant` 选择器。
///
/// # Errors
///
/// 权限越界或非活动会话返回 [`DesktopError`]。
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_open_chooser_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    caller: TrustedUiCallerOrigin,
    requesting_application_id_hex: &str,
    requesting_app_display_name: &str,
    mode_raw: &str,
    requested_rights_raw: &str,
    ttl_ms: u64,
    now_mono_ms: u64,
) -> Result<TcbChooserSessionDto, DesktopError> {
    let app_id = parse_hex_id_16(
        "requesting_application_id_hex",
        requesting_application_id_hex,
    )?;
    let mode = ChooserMode::parse(mode_raw)?;
    let rights = ChooserAttenuatedRights::parse(requested_rights_raw)?;
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            let dto = auth.chooser_broker.open_chooser_session(
                caller,
                &shell.session,
                app_id,
                requesting_app_display_name,
                mode,
                rights,
                ttl_ms,
                now_mono_ms,
                &mut auth.audit,
            )?;
            shell.set_z4_modal(auth.has_active_z4_surface(), now_mono_ms);
            Ok(dto)
        })
    })
}

/// `_core`: 确认系统 `ChooserGrant` 并返回仅含不透明句柄与摘要的 `ChooserGrantForAppDto`。
///
/// # Errors
///
/// 武装时间窗未满、条目越界或导出名非法返回 [`DesktopError`]。
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_confirm_chooser_grant_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    origin: InputOrigin,
    source: InputSource,
    chooser_session_id_hex: &str,
    selected_vault_item_hexes: &[String],
    export_save_leaf_name: Option<&str>,
    export_content_type: Option<&str>,
    now_mono_ms: u64,
) -> Result<ChooserGrantForAppDto, DesktopError> {
    let chooser_id = parse_hex_id_16("chooser_session_id_hex", chooser_session_id_hex)?;
    let mut item_ids = Vec::with_capacity(selected_vault_item_hexes.len());
    for raw in selected_vault_item_hexes {
        item_ids.push(parse_hex_id_16("selected_vault_item_id", raw)?);
    }
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            let grant = auth.chooser_broker.confirm_chooser_grant(
                origin,
                source,
                &shell.session,
                chooser_id,
                &item_ids,
                export_save_leaf_name,
                export_content_type,
                now_mono_ms,
                &mut auth.audit,
            )?;
            shell.set_z4_modal(auth.has_active_z4_surface(), now_mono_ms);
            Ok(grant)
        })
    })
}

/// `_core`: 吊销指定 `ChooserGrant`。
///
/// # Errors
///
/// 未知 `grant_id` 返回 [`DesktopError`]。
pub fn trusted_ui_revoke_chooser_grant_core(
    trusted: &TrustedUiState,
    grant_id_hex: &str,
) -> Result<(), DesktopError> {
    let grant_id = parse_hex_id_16("grant_id_hex", grant_id_hex)?;
    trusted.with_authority_mut(|auth| {
        auth.chooser_broker
            .revoke_chooser_grant(grant_id, &mut auth.audit)
            .map_err(DesktopError::from)
    })
}

/// `_core`: 签发一次性用户手势令牌（供剪贴板/拖放操作消费）。
///
/// # Errors
///
/// 自动化通道或非可信来源返回 [`DesktopError`]。
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_mint_gesture_token_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    origin: InputOrigin,
    source: InputSource,
    actor_application_id_hex: &str,
    channel_raw: &str,
    now_mono_ms: u64,
    ttl_ms: u64,
) -> Result<UserGestureTokenDto, DesktopError> {
    let actor_app = parse_hex_id_16("actor_application_id_hex", actor_application_id_hex)?;
    let channel = TransferChannelKind::parse(channel_raw)?;
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            auth.clipboard_drag_broker
                .mint_user_gesture_token(
                    origin,
                    source,
                    actor_app,
                    channel,
                    &shell.session,
                    now_mono_ms,
                    ttl_ms,
                    &mut auth.audit,
                )
                .map_err(DesktopError::from)
        })
    })
}

/// `_core`: 登记剪贴板或拖放传输信封。
///
/// # Errors
///
/// 缺失有效用户手势或参数非法返回 [`DesktopError`]。
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_publish_transfer_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    channel_raw: &str,
    content_type: &str,
    payload_bytes: Vec<u8>,
    source_application_id_hex: &str,
    source_app_display_name: &str,
    taint_flag_raws: &[String],
    gesture_token_id_hex: Option<&str>,
    ttl_ms: u64,
    now_mono_ms: u64,
) -> Result<TransferMetadataDto, DesktopError> {
    let channel = TransferChannelKind::parse(channel_raw)?;
    let src_app = parse_hex_id_16("source_application_id_hex", source_application_id_hex)?;
    let gesture_id = match gesture_token_id_hex {
        Some(raw) => Some(parse_hex_id_16("gesture_token_id_hex", raw)?),
        None => None,
    };
    let mut taints = Vec::with_capacity(taint_flag_raws.len());
    for raw in taint_flag_raws {
        taints.push(TaintFlag::parse(raw)?);
    }
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            auth.clipboard_drag_broker
                .publish_transfer(
                    &shell.session,
                    channel,
                    content_type,
                    payload_bytes,
                    src_app,
                    source_app_display_name,
                    taints,
                    gesture_id,
                    ttl_ms,
                    now_mono_ms,
                    &mut auth.audit,
                )
                .map_err(DesktopError::from)
        })
    })
}

/// `_core`: 请求跨应用剪贴板/拖放读取；若命中高敏/未核验污点则自动拉起 `Z4` 并锁闭 `z4_modal_lock`。
///
/// # Errors
///
/// 无手势、传输已过期或会话非活动返回 [`DesktopError`]。
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_request_transfer_read_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    transfer_id_hex: &str,
    target_application_id_hex: &str,
    target_installation_id_hex: &str,
    target_app_display_name: &str,
    gesture_token_id_hex: Option<&str>,
    now_mono_ms: u64,
) -> Result<TransferReadDecisionDto, DesktopError> {
    let transfer_id = parse_hex_id_16("transfer_id_hex", transfer_id_hex)?;
    let target_app = parse_hex_id_16("target_application_id_hex", target_application_id_hex)?;
    let target_inst = parse_hex_id_16("target_installation_id_hex", target_installation_id_hex)?;
    let gesture_id = match gesture_token_id_hex {
        Some(raw) => Some(parse_hex_id_16("gesture_token_id_hex", raw)?),
        None => None,
    };
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            let decision = auth.clipboard_drag_broker.request_transfer_read(
                &mut auth.z4_broker,
                &shell.session,
                transfer_id,
                target_app,
                target_inst,
                target_app_display_name,
                gesture_id,
                now_mono_ms,
                &mut auth.audit,
            )?;
            shell.set_z4_modal(auth.has_active_z4_surface(), now_mono_ms);
            Ok(decision)
        })
    })
}

/// `_core`: 在 `Z4` 获批后完成升级污点传输的最终释放。
///
/// # Errors
///
/// `Z4` 未批准或已取消/过期返回 [`DesktopError`]。
pub fn trusted_ui_complete_escalated_transfer_core(
    trusted: &TrustedUiState,
    app: &crate::ipc::AppState,
    transfer_id_hex: &str,
    target_application_id_hex: &str,
    challenge_id_hex: &str,
    now_mono_ms: u64,
) -> Result<TransferReadDecisionDto, DesktopError> {
    let transfer_id = parse_hex_id_16("transfer_id_hex", transfer_id_hex)?;
    let target_app = parse_hex_id_16("target_application_id_hex", target_application_id_hex)?;
    let challenge_id = parse_hex_id_16("challenge_id_hex", challenge_id_hex)?;
    app.with_host_shell_mut(|shell| {
        trusted.with_authority_mut(|auth| {
            auth.sync_with_host_shell(shell, now_mono_ms);
            auth.clipboard_drag_broker
                .complete_escalated_transfer_after_z4(
                    &auth.z4_broker,
                    &shell.session,
                    transfer_id,
                    target_app,
                    challenge_id,
                    now_mono_ms,
                    &mut auth.audit,
                )
                .map_err(DesktopError::from)
        })
    })
}

/// `_core`: 获取可信 UI 防伪与边界拦截审计计数器快照。
///
/// # Errors
///
/// 锁中毒返回 [`DesktopError`]。
pub fn trusted_ui_get_security_audit_core(
    trusted: &TrustedUiState,
) -> Result<TrustedUiSecurityAuditCounters, DesktopError> {
    trusted.with_authority_mut(|auth| Ok(auth.audit))
}

// ============================================================================
// Tauri #[command] Wrappers (Ready for W71-INT `lib.rs` `generate_handler![]`)
// ============================================================================

#[tauri::command]
pub fn trusted_ui_open_z4_consent(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    request: Z4ConsentChallengeRequest,
    now_mono_ms: u64,
) -> Result<Z4TrustedChallengeDto, DesktopError> {
    trusted_ui_open_z4_consent_core(
        &trusted,
        &app,
        TrustedUiCallerOrigin::TcbTrustedBroker,
        request,
        now_mono_ms,
    )
}

#[tauri::command]
pub fn trusted_ui_notify_z4_focus_regained(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    challenge_id_hex: String,
    now_mono_ms: u64,
) -> Result<u64, DesktopError> {
    trusted_ui_notify_z4_focus_regained_core(
        &trusted,
        &app,
        TrustedUiCallerOrigin::TcbTrustedBroker,
        &challenge_id_hex,
        now_mono_ms,
    )
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_decide_z4_consent(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    source: String,
    challenge_id_hex: String,
    confirm: bool,
    chosen_grant_scope: Option<String>,
    irreversible_high_contrast_ack: bool,
    now_mono_ms: u64,
) -> Result<Z4ConsentDecisionReceiptDto, DesktopError> {
    let parsed_source = InputSource::parse(&source).map_err(DesktopError::config)?;
    let parsed_scope = match chosen_grant_scope {
        Some(raw) => Some(ConsentGrantScope::parse(&raw)?),
        None => None,
    };
    trusted_ui_decide_z4_consent_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        parsed_source,
        &challenge_id_hex,
        confirm,
        parsed_scope,
        irreversible_high_contrast_ack,
        now_mono_ms,
    )
}

#[tauri::command]
pub fn trusted_ui_get_public_posture(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    now_mono_ms: u64,
) -> Result<LowerPlaneTrustedPostureDto, DesktopError> {
    trusted_ui_get_public_posture_core(&trusted, &app, now_mono_ms)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_open_chooser(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    requesting_application_id_hex: String,
    requesting_app_display_name: String,
    mode: String,
    requested_rights: String,
    ttl_ms: u64,
    now_mono_ms: u64,
) -> Result<TcbChooserSessionDto, DesktopError> {
    trusted_ui_open_chooser_core(
        &trusted,
        &app,
        TrustedUiCallerOrigin::TcbTrustedBroker,
        &requesting_application_id_hex,
        &requesting_app_display_name,
        &mode,
        &requested_rights,
        ttl_ms,
        now_mono_ms,
    )
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_confirm_chooser_grant(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    source: String,
    chooser_session_id_hex: String,
    selected_vault_item_hexes: Vec<String>,
    export_save_leaf_name: Option<String>,
    export_content_type: Option<String>,
    now_mono_ms: u64,
) -> Result<ChooserGrantForAppDto, DesktopError> {
    let parsed_source = InputSource::parse(&source).map_err(DesktopError::config)?;
    trusted_ui_confirm_chooser_grant_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        parsed_source,
        &chooser_session_id_hex,
        &selected_vault_item_hexes,
        export_save_leaf_name.as_deref(),
        export_content_type.as_deref(),
        now_mono_ms,
    )
}

#[tauri::command]
pub fn trusted_ui_revoke_chooser_grant(
    trusted: tauri::State<'_, TrustedUiState>,
    grant_id_hex: String,
) -> Result<(), DesktopError> {
    trusted_ui_revoke_chooser_grant_core(&trusted, &grant_id_hex)
}

#[tauri::command]
pub fn trusted_ui_mint_gesture_token(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    source: String,
    actor_application_id_hex: String,
    channel: String,
    now_mono_ms: u64,
    ttl_ms: u64,
) -> Result<UserGestureTokenDto, DesktopError> {
    let parsed_source = InputSource::parse(&source).map_err(DesktopError::config)?;
    trusted_ui_mint_gesture_token_core(
        &trusted,
        &app,
        InputOrigin::TrustedShell,
        parsed_source,
        &actor_application_id_hex,
        &channel,
        now_mono_ms,
        ttl_ms,
    )
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_publish_transfer(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    channel: String,
    content_type: String,
    payload_utf8: String,
    source_application_id_hex: String,
    source_app_display_name: String,
    taint_flags: Vec<String>,
    gesture_token_id_hex: Option<String>,
    ttl_ms: u64,
    now_mono_ms: u64,
) -> Result<TransferMetadataDto, DesktopError> {
    trusted_ui_publish_transfer_core(
        &trusted,
        &app,
        &channel,
        &content_type,
        payload_utf8.into_bytes(),
        &source_application_id_hex,
        &source_app_display_name,
        &taint_flags,
        gesture_token_id_hex.as_deref(),
        ttl_ms,
        now_mono_ms,
    )
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn trusted_ui_request_transfer_read(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    transfer_id_hex: String,
    target_application_id_hex: String,
    target_installation_id_hex: String,
    target_app_display_name: String,
    gesture_token_id_hex: Option<String>,
    now_mono_ms: u64,
) -> Result<TransferReadDecisionDto, DesktopError> {
    trusted_ui_request_transfer_read_core(
        &trusted,
        &app,
        &transfer_id_hex,
        &target_application_id_hex,
        &target_installation_id_hex,
        &target_app_display_name,
        gesture_token_id_hex.as_deref(),
        now_mono_ms,
    )
}

#[tauri::command]
pub fn trusted_ui_complete_escalated_transfer(
    trusted: tauri::State<'_, TrustedUiState>,
    app: tauri::State<'_, crate::ipc::AppState>,
    transfer_id_hex: String,
    target_application_id_hex: String,
    challenge_id_hex: String,
    now_mono_ms: u64,
) -> Result<TransferReadDecisionDto, DesktopError> {
    trusted_ui_complete_escalated_transfer_core(
        &trusted,
        &app,
        &transfer_id_hex,
        &target_application_id_hex,
        &challenge_id_hex,
        now_mono_ms,
    )
}

#[tauri::command]
pub fn trusted_ui_get_security_audit(
    trusted: tauri::State<'_, TrustedUiState>,
) -> Result<TrustedUiSecurityAuditCounters, DesktopError> {
    trusted_ui_get_security_audit_core(&trusted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fips_180_4_sha256_vectors_match_nist() {
        assert_eq!(
            hex(&sha256_bytes(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256_bytes(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256_bytes(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn leaf_name_sanitizer_blocks_host_paths_and_traversal() {
        assert!(sanitize_leaf_display_name("report-2026.pdf").is_ok());
        assert!(sanitize_leaf_display_name("../secret.key").is_err());
        assert!(sanitize_leaf_display_name(r"C:\Users\alice\secret.pdf").is_err());
        assert!(sanitize_leaf_display_name("/home/alice/id_rsa").is_err());
    }
}
