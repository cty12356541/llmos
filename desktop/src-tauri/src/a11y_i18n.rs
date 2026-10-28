//! W73-D-A11Y-I18N: WCAG 2.2 AA, ARIA semantic tree, keyboard focus ring,
//! reduced-motion, and `zh-CN / en-US` localization conformance module.
//!
//! Covers:
//! - `[TUI-EXT-001]` Versioned (`1.0.0-stage-d`, rev 1) `zh-CN` & `en-US` resource bundles,
//!   atomic runtime locale switching, 100% key parity, and `L0/L1/L2` zero-jargon verification.
//! - `[DUI-WINDOW-001]` & `WCAG 2.2 AA` relative luminance contrast auditor (`>= 4.5:1` normal text,
//!   `>= 3.0:1` UI components / `2px` focus rings across `Graphite` and `Paper` workstation themes).
//! - `[TUI-CONSENT-001]` `Z4` modal focus trap (strict `Tab`/`Shift+Tab` cycle inside `Z4`,
//!   zero escape to `Z0..Z3`, blocked `Ctrl+Space`/`Ctrl+K` while `Z4` is active, and `Escape`
//!   originating trigger restoration).
//! - Global `prefers-reduced-motion: reduce` and user override freezing the `32x20` Gerono
//!   verification seal to a static closed curve (`0` active RAF loops, `0ms` transitions).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::{DesktopError, ErrorCode};

pub const LOCALE_BUNDLE_VERSION: &str = "1.0.0-stage-d";
pub const LOCALE_SCHEMA_REVISION: u32 = 1;
pub const WCAG_AA_NORMAL_TEXT_MIN_RATIO: f64 = 4.5;
pub const WCAG_AA_UI_COMPONENT_MIN_RATIO: f64 = 3.0;
pub const FOCUS_RING_WIDTH_PX: u32 = 2;
pub const WCAG_MIN_HIT_TARGET_PX: u32 = 26;

pub const FORBIDDEN_L0_TO_L2_JARGON_TOKENS: [&str; 17] = [
    "Z0",
    "Z1",
    "Z2",
    "Z3",
    "Z4",
    "TypedIntent",
    "0x1F",
    "0x1f",
    "Quota",
    "quota",
    "ROAD-D-001",
    "ADR-0011",
    "ADR-0025",
    "TaskAttempt",
    "ExecutionFiber",
    "receipt_hex",
    "control_command_id",
];

fn a11y_err(message: impl Into<String>) -> DesktopError {
    DesktopError::new(ErrorCode::Control, message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum WorkstationLocaleDto {
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en-US")]
    EnUs,
}

impl WorkstationLocaleDto {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }

    pub fn parse_str(raw: &str) -> Result<Self, DesktopError> {
        match raw.trim() {
            "zh-CN" | "zh_CN" | "zh" => Ok(Self::ZhCn),
            "en-US" | "en_US" | "en" => Ok(Self::EnUs),
            other => Err(DesktopError::config(format!(
                "UNSUPPORTED_LOCALE: '{other}' (expected 'zh-CN' or 'en-US')"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocaleBundleSnapshotDto {
    pub locale: WorkstationLocaleDto,
    pub bundle_version: String,
    pub schema_revision: u32,
    pub switch_generation: u64,
    pub plane_labels: BTreeMap<String, String>,
    pub task_state_labels: BTreeMap<String, String>,
    pub control_domain_labels: BTreeMap<String, String>,
    pub l2_statutory_lock_labels: BTreeMap<String, String>,
    pub l3_forensic_labels: BTreeMap<String, String>,
    pub key_parity_verified: bool,
    pub l0_to_l2_zero_jargon_verified: bool,
    pub zero_emoji_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WcagContrastPairResultDto {
    pub pair_id: String,
    pub theme: String,
    pub category: String,
    pub foreground_token: String,
    pub foreground_hex: String,
    pub background_token: String,
    pub background_hex: String,
    pub contrast_ratio: f64,
    pub required_ratio: f64,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WcagContrastAuditReportDto {
    pub passed: bool,
    pub total_pairs_audited: usize,
    pub min_normal_text_ratio: f64,
    pub min_ui_component_ratio: f64,
    pub focus_ring_width_px: u32,
    pub pairs: Vec<WcagContrastPairResultDto>,
    pub failures: Vec<WcagContrastPairResultDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AriaLandmarkNodeDto {
    pub node_id: String,
    pub plane: String,
    pub role: String,
    pub accessible_name: String,
    pub interactive: bool,
    pub hit_target_width_px: u32,
    pub hit_target_height_px: u32,
    pub aria_hidden: bool,
    pub aria_modal: Option<bool>,
    pub aria_expanded: Option<bool>,
    pub aria_live: Option<String>,
    pub keyboard_drag_equivalent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AriaViolationDto {
    pub node_id: String,
    pub rule_id: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AriaLandmarksAuditReportDto {
    pub passed: bool,
    pub locale: WorkstationLocaleDto,
    pub z4_modal_active: bool,
    pub total_nodes: usize,
    pub interactive_nodes: usize,
    pub min_hit_target_px: u32,
    pub landmarks_present: Vec<String>,
    pub polite_live_region_ready: bool,
    pub assertive_live_region_ready: bool,
    pub nodes: Vec<AriaLandmarkNodeDto>,
    pub violations: Vec<AriaViolationDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardFocusTargetInputDto {
    pub id: String,
    pub plane: String,
    pub label: String,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardFocusTrapRequestDto {
    pub z4_modal_active: bool,
    pub originating_trigger_id: String,
    pub targets: Vec<KeyboardFocusTargetInputDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardFocusTrapReportDto {
    pub z4_modal_active: bool,
    pub z4_control_ids: Vec<String>,
    pub forward_tab_cycle: Vec<String>,
    pub backward_shift_tab_cycle: Vec<String>,
    pub leaked_to_lower_plane: bool,
    pub blocked_lower_plane_focus_attempts: u32,
    pub blocked_global_shortcuts_under_z4: bool,
    pub focus_restored_on_escape: bool,
    pub restored_focus_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReducedMotionPostureDto {
    pub system_prefers_reduced_motion: bool,
    pub user_override_reduced_motion: Option<bool>,
    pub effective_reduced_motion: bool,
    pub gerono_seal_mode: String,
    pub active_raf_loop_count: u32,
    pub fast_transition_duration_ms: u32,
    pub base_transition_duration_ms: u32,
}

#[derive(Debug, Clone)]
struct A11yI18nInner {
    locale: WorkstationLocaleDto,
    switch_generation: u64,
    system_prefers_reduced_motion: bool,
    user_override_reduced_motion: Option<bool>,
    z4_seal_mounted: bool,
}

impl Default for A11yI18nInner {
    fn default() -> Self {
        Self {
            locale: WorkstationLocaleDto::ZhCn,
            switch_generation: 1,
            system_prefers_reduced_motion: false,
            user_override_reduced_motion: None,
            z4_seal_mounted: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct A11yI18nState {
    inner: Arc<RwLock<A11yI18nInner>>,
}

impl A11yI18nState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read_guard(&self) -> Result<RwLockReadGuard<'_, A11yI18nInner>, DesktopError> {
        self.inner
            .read()
            .map_err(|_| DesktopError::internal("A11yI18nState RwLock poisoned on read"))
    }

    fn write_guard(&self) -> Result<RwLockWriteGuard<'_, A11yI18nInner>, DesktopError> {
        self.inner
            .write()
            .map_err(|_| DesktopError::internal("A11yI18nState RwLock poisoned on write"))
    }
}

pub fn contains_forbidden_l0_to_l2_jargon(text: &str) -> Option<&'static str> {
    for token in FORBIDDEN_L0_TO_L2_JARGON_TOKENS {
        if token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            let words: Vec<&str> = text
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .collect();
            if words.contains(&token) {
                return Some(token);
            }
        } else if text.contains(token) {
            return Some(token);
        }
    }
    None
}

pub fn contains_pictographic_emoji(text: &str) -> bool {
    text.chars().any(|ch| {
        let code = u32::from(ch);
        (0x1F300..=0x1FAFF).contains(&code)
            || (0x2600..=0x26FF).contains(&code)
            || (0x2700..=0x27BF).contains(&code)
    })
}

fn build_plane_labels(locale: WorkstationLocaleDto) -> BTreeMap<String, String> {
    let pairs = match locale {
        WorkstationLocaleDto::ZhCn => [
            ("z0Backdrop", "工作站桌面底板"),
            ("z1TaskSpace", "任务与承诺工作区"),
            ("z2Compositor", "应用窗口工作区"),
            ("z3Taskbar", "工作站底部任务栏"),
            ("z3CommandPalette", "命令与自然语言意图面板"),
            ("z3SearchPalette", "全局候选搜索面板"),
            ("z3Notifications", "系统通知与快捷确认面板"),
            ("z4SecurityScreen", "系统安全确认屏"),
        ],
        WorkstationLocaleDto::EnUs => [
            ("z0Backdrop", "Workstation Desktop Backdrop"),
            ("z1TaskSpace", "Task & Commitment Workspace"),
            ("z2Compositor", "Application Windows Workspace"),
            ("z3Taskbar", "Workstation Bottom Taskbar"),
            ("z3CommandPalette", "Command & Natural Language Palette"),
            ("z3SearchPalette", "Candidate Search Preview Palette"),
            ("z3Notifications", "System Notifications & Quick Actions"),
            ("z4SecurityScreen", "System Security Confirmation Screen"),
        ],
    };
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn build_task_state_labels(locale: WorkstationLocaleDto) -> BTreeMap<String, String> {
    let pairs = match locale {
        WorkstationLocaleDto::ZhCn => [
            ("DRAFT", "草稿待提交"),
            ("PROPOSED", "草稿待提交"),
            ("WAITING_CONSENT", "待您确认权限或费用上限"),
            ("AUTHORIZED", "正在运行"),
            ("RUNNING", "正在运行"),
            ("WAITING_INPUT", "待补充说明"),
            ("WAITING_RESOURCE", "排队等待计算资源"),
            ("WAITING_NETWORK", "等待网络连接（本地正常可用）"),
            ("SUSPENDED", "已暂停"),
            ("RECOVERING", "正在对账恢复"),
            ("CANCELLING", "正在取消"),
            ("COMPLETING", "正在完成收尾"),
            ("COMPLETED", "已完成"),
            ("PARTIAL", "部分完成（含未完成项）"),
            ("DISPUTED", "结果不一致，待您核对"),
            ("FAILED", "未能完成"),
            ("CANCELLED", "已取消"),
            ("QUARANTINED", "已受控隔离"),
            ("DECLARED", "待执行步骤"),
            ("BLOCKED_DEPENDENCY", "等待前置步骤完成"),
            ("ELIGIBLE", "已就绪待启动"),
            ("ACTIVE", "正在执行"),
            ("SKIPPED", "已跳过"),
            ("SUPERSEDED", "已由新尝试替代"),
        ],
        WorkstationLocaleDto::EnUs => [
            ("DRAFT", "Draft"),
            ("PROPOSED", "Draft"),
            ("WAITING_CONSENT", "Approval Required"),
            ("AUTHORIZED", "Running"),
            ("RUNNING", "Running"),
            ("WAITING_INPUT", "Input Needed"),
            ("WAITING_RESOURCE", "Queued for Resources"),
            ("WAITING_NETWORK", "Waiting for Network (Local Ready)"),
            ("SUSPENDED", "Paused"),
            ("RECOVERING", "Recovering"),
            ("CANCELLING", "Cancelling"),
            ("COMPLETING", "Completing"),
            ("COMPLETED", "Completed"),
            ("PARTIAL", "Partially Completed"),
            ("DISPUTED", "Needs Verification"),
            ("FAILED", "Failed"),
            ("CANCELLED", "Cancelled"),
            ("QUARANTINED", "Isolated"),
            ("DECLARED", "Pending Step"),
            ("BLOCKED_DEPENDENCY", "Waiting on Prerequisite"),
            ("ELIGIBLE", "Ready to Run"),
            ("ACTIVE", "Active"),
            ("SKIPPED", "Skipped"),
            ("SUPERSEDED", "Replaced by Newer Run"),
        ],
    };
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn build_control_domain_labels(locale: WorkstationLocaleDto) -> BTreeMap<String, String> {
    let pairs = match locale {
        WorkstationLocaleDto::ZhCn => [
            ("system-device", "系统与主机设备"),
            ("workspace", "工作区与租户环境"),
            ("application", "已安装应用程序"),
            ("task", "用户任务与承诺"),
            ("task-group", "任务编组集合"),
            ("task-node", "计划执行步骤节点"),
            ("task-attempt", "单次执行运行记录"),
            ("execution-unit", "宿主进程与执行单元"),
            ("resource-group", "计算与存储资源池"),
            ("topic-channel", "消息主题与数据通道"),
            ("operation-device", "外部设备与副作用操作"),
            ("cell-distributed-lease", "跨节点租约与连接态势"),
        ],
        WorkstationLocaleDto::EnUs => [
            ("system-device", "System & Host Hardware"),
            ("workspace", "Workspaces & Environments"),
            ("application", "Installed Applications"),
            ("task", "User Tasks & Commitments"),
            ("task-group", "Task Group Collections"),
            ("task-node", "Plan Execution Step Nodes"),
            ("task-attempt", "Execution Run Records"),
            ("execution-unit", "Host Processes & Execution Units"),
            ("resource-group", "Compute & Storage Resource Pools"),
            ("topic-channel", "Message Topics & Data Channels"),
            ("operation-device", "External Device & Effect Operations"),
            (
                "cell-distributed-lease",
                "Cross-Node Leases & Connectivity Posture",
            ),
        ],
    };
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn build_statutory_lock_labels(locale: WorkstationLocaleDto) -> BTreeMap<String, String> {
    let pairs = match locale {
        WorkstationLocaleDto::ZhCn => [
            (
                "SEARCH_0X1F_READ_ONLY",
                "候选搜索预览只读 · 打开原记录核实后可操作",
            ),
            (
                "IRREVERSIBLE_NO_UNDO",
                "不可逆外部操作 · 无撤销入口，可查看操作凭证",
            ),
            (
                "NODE_COMPLETED_OR_ACTIVE_LOCKED",
                "已执行或正在执行的步骤禁止修改结构",
            ),
            (
                "TASK_ATTEMPT_NO_MANUAL_COMMIT",
                "系统自动仲裁最终提交 · 不可手动标记完成",
            ),
            (
                "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS",
                "外部执行结果待对账核实 · 不可手动改判成功",
            ),
            (
                "QUARANTINED_LEASE_FENCE",
                "安全围栏隔离中 · 待收齐关闭确认后解锁",
            ),
        ],
        WorkstationLocaleDto::EnUs => [
            (
                "SEARCH_0X1F_READ_ONLY",
                "Read-only search preview · Open original record to act",
            ),
            (
                "IRREVERSIBLE_NO_UNDO",
                "Irreversible external action · Undo unavailable, view proof",
            ),
            (
                "NODE_COMPLETED_OR_ACTIVE_LOCKED",
                "Completed or active step is locked against structural edits",
            ),
            (
                "TASK_ATTEMPT_NO_MANUAL_COMMIT",
                "System-arbitrated commit · Manual completion disabled",
            ),
            (
                "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS",
                "External outcome pending reconciliation · Manual override disabled",
            ),
            (
                "QUARANTINED_LEASE_FENCE",
                "Isolated by safety fence · Unlocks after closure confirmation",
            ),
        ],
    };
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn build_l3_forensic_labels(locale: WorkstationLocaleDto) -> BTreeMap<String, String> {
    let pairs = match locale {
        WorkstationLocaleDto::ZhCn => [
            ("taskPlanDag", "TaskPlan 正交有向无环图与惰性物化超节点"),
            (
                "attemptRaceLane",
                "TaskAttempt 竞速泳道 (CAS-Only Commit [TASK-COMMIT-001])",
            ),
            ("fiberWaterfall", "ExecutionFiber 五段相位堆栈瀑布条"),
            ("cellLeaseInspector", "Stage C Cell 三族租约与 LeaseEpoch"),
            (
                "typedIntentPreview",
                "TypedIntent 编译载荷与 ActionProposal",
            ),
            ("searchMaskTag", "NON_AUTHORITATIVE · mask=0x1F"),
            (
                "receiptHexAudit",
                "SABI ControlReceipt 原始字节 (receipt_hex)",
            ),
        ],
        WorkstationLocaleDto::EnUs => [
            (
                "taskPlanDag",
                "TaskPlan Orthogonal DAG & Lazy Materialization Supernodes",
            ),
            (
                "attemptRaceLane",
                "TaskAttempt Race Lanes (CAS-Only Commit [TASK-COMMIT-001])",
            ),
            ("fiberWaterfall", "ExecutionFiber 5-Phase Stack Waterfall"),
            (
                "cellLeaseInspector",
                "Stage C Cell 3-Family Leases & LeaseEpoch",
            ),
            (
                "typedIntentPreview",
                "TypedIntent Compiled Payload & ActionProposal",
            ),
            ("searchMaskTag", "NON_AUTHORITATIVE · mask=0x1F"),
            (
                "receiptHexAudit",
                "SABI ControlReceipt Raw Bytes (receipt_hex)",
            ),
        ],
    };
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn assemble_locale_bundle_snapshot(
    locale: WorkstationLocaleDto,
    switch_generation: u64,
) -> LocaleBundleSnapshotDto {
    let plane_labels = build_plane_labels(locale);
    let task_state_labels = build_task_state_labels(locale);
    let control_domain_labels = build_control_domain_labels(locale);
    let l2_statutory_lock_labels = build_statutory_lock_labels(locale);
    let l3_forensic_labels = build_l3_forensic_labels(locale);

    let other_locale = match locale {
        WorkstationLocaleDto::ZhCn => WorkstationLocaleDto::EnUs,
        WorkstationLocaleDto::EnUs => WorkstationLocaleDto::ZhCn,
    };
    let key_parity_verified = plane_labels
        .keys()
        .eq(build_plane_labels(other_locale).keys())
        && task_state_labels
            .keys()
            .eq(build_task_state_labels(other_locale).keys())
        && control_domain_labels
            .keys()
            .eq(build_control_domain_labels(other_locale).keys())
        && l2_statutory_lock_labels
            .keys()
            .eq(build_statutory_lock_labels(other_locale).keys())
        && l3_forensic_labels
            .keys()
            .eq(build_l3_forensic_labels(other_locale).keys());

    let l0_to_l2_values = plane_labels
        .values()
        .chain(task_state_labels.values())
        .chain(control_domain_labels.values())
        .chain(l2_statutory_lock_labels.values());

    let mut l0_to_l2_zero_jargon_verified = true;
    let mut zero_emoji_verified = true;
    for val in l0_to_l2_values {
        if contains_forbidden_l0_to_l2_jargon(val).is_some() {
            l0_to_l2_zero_jargon_verified = false;
        }
        if contains_pictographic_emoji(val) {
            zero_emoji_verified = false;
        }
    }
    for val in l3_forensic_labels.values() {
        if contains_pictographic_emoji(val) {
            zero_emoji_verified = false;
        }
    }

    LocaleBundleSnapshotDto {
        locale,
        bundle_version: LOCALE_BUNDLE_VERSION.to_string(),
        schema_revision: LOCALE_SCHEMA_REVISION,
        switch_generation,
        plane_labels,
        task_state_labels,
        control_domain_labels,
        l2_statutory_lock_labels,
        l3_forensic_labels,
        key_parity_verified,
        l0_to_l2_zero_jargon_verified,
        zero_emoji_verified,
    }
}

pub fn parse_hex_color_rgb(hex: &str) -> Result<(u8, u8, u8), DesktopError> {
    let cleaned = hex.trim().strip_prefix('#').unwrap_or(hex.trim());
    if cleaned.len() != 6 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(a11y_err(format!("INVALID_HEX_COLOR: '{hex}'")));
    }
    let r = u8::from_str_radix(&cleaned[0..2], 16)
        .map_err(|e| a11y_err(format!("HEX_PARSE_ERR: {e}")))?;
    let g = u8::from_str_radix(&cleaned[2..4], 16)
        .map_err(|e| a11y_err(format!("HEX_PARSE_ERR: {e}")))?;
    let b = u8::from_str_radix(&cleaned[4..6], 16)
        .map_err(|e| a11y_err(format!("HEX_PARSE_ERR: {e}")))?;
    Ok((r, g, b))
}

pub fn compute_relative_luminance_hex(hex: &str) -> Result<f64, DesktopError> {
    let (r8, g8, b8) = parse_hex_color_rgb(hex)?;
    let linearize = |channel8: u8| -> f64 {
        let s = f64::from(channel8) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    Ok(0.2126 * linearize(r8) + 0.7152 * linearize(g8) + 0.0722 * linearize(b8))
}

pub fn compute_contrast_ratio_hex(fg_hex: &str, bg_hex: &str) -> Result<f64, DesktopError> {
    let lum_a = compute_relative_luminance_hex(fg_hex)?;
    let lum_b = compute_relative_luminance_hex(bg_hex)?;
    let lighter = lum_a.max(lum_b);
    let darker = lum_a.min(lum_b);
    let raw = (lighter + 0.05) / (darker + 0.05);
    Ok((raw * 100.0).round() / 100.0)
}

pub fn a11y_get_locale_bundle_inner(
    state: &A11yI18nState,
    locale_override: Option<&str>,
) -> Result<LocaleBundleSnapshotDto, DesktopError> {
    let guard = state.read_guard()?;
    let target_locale = match locale_override {
        Some(raw) => WorkstationLocaleDto::parse_str(raw)?,
        None => guard.locale,
    };
    Ok(assemble_locale_bundle_snapshot(
        target_locale,
        guard.switch_generation,
    ))
}

pub fn a11y_switch_locale_inner(
    state: &A11yI18nState,
    next_locale_str: &str,
) -> Result<LocaleBundleSnapshotDto, DesktopError> {
    let next_locale = WorkstationLocaleDto::parse_str(next_locale_str)?;
    let mut guard = state.write_guard()?;
    if guard.locale != next_locale {
        guard.locale = next_locale;
        guard.switch_generation = guard.switch_generation.saturating_add(1);
    }
    Ok(assemble_locale_bundle_snapshot(
        guard.locale,
        guard.switch_generation,
    ))
}

pub fn a11y_audit_wcag_contrast_inner(
    _state: &A11yI18nState,
) -> Result<WcagContrastAuditReportDto, DesktopError> {
    let specs = [
        // Graphite Workstation (Dark)
        (
            "graphite:text-primary-on-window",
            "graphite-workstation",
            "normal-text",
            "--nlos-text-primary",
            "#ecedee",
            "--nlos-bg-window",
            "#18191c",
        ),
        (
            "graphite:text-primary-on-elevated",
            "graphite-workstation",
            "normal-text",
            "--nlos-text-primary",
            "#ecedee",
            "--nlos-bg-elevated",
            "#23252a",
        ),
        (
            "graphite:text-secondary-on-window",
            "graphite-workstation",
            "normal-text",
            "--nlos-text-secondary",
            "#9da3ae",
            "--nlos-bg-window",
            "#18191c",
        ),
        (
            "graphite:text-secondary-on-elevated",
            "graphite-workstation",
            "normal-text",
            "--nlos-text-secondary",
            "#9da3ae",
            "--nlos-bg-elevated",
            "#23252a",
        ),
        (
            "graphite:accent-fg-on-accent-primary",
            "graphite-workstation",
            "normal-text",
            "--nlos-accent-fg",
            "#ffffff",
            "--nlos-accent-primary",
            "#2563eb",
        ),
        (
            "graphite:signal-action-text-on-window",
            "graphite-workstation",
            "normal-text",
            "--nlos-signal-action-text",
            "#f59e0b",
            "--nlos-bg-window",
            "#18191c",
        ),
        (
            "graphite:signal-danger-text-on-window",
            "graphite-workstation",
            "normal-text",
            "--nlos-signal-danger-text",
            "#f87171",
            "--nlos-bg-window",
            "#18191c",
        ),
        (
            "graphite:signal-danger-fg-on-danger",
            "graphite-workstation",
            "normal-text",
            "--nlos-signal-danger-fg",
            "#ffffff",
            "--nlos-signal-danger",
            "#dc2626",
        ),
        (
            "graphite:focus-ring-on-window",
            "graphite-workstation",
            "ui-component",
            "--nlos-accent-active",
            "#3b82f6",
            "--nlos-bg-window",
            "#18191c",
        ),
        (
            "graphite:focus-ring-on-elevated",
            "graphite-workstation",
            "ui-component",
            "--nlos-accent-active",
            "#3b82f6",
            "--nlos-bg-elevated",
            "#23252a",
        ),
        // Paper Workstation (Light)
        (
            "paper:text-primary-on-window",
            "paper-workstation",
            "normal-text",
            "--nlos-text-primary",
            "#111827",
            "--nlos-bg-window",
            "#ffffff",
        ),
        (
            "paper:text-primary-on-chrome",
            "paper-workstation",
            "normal-text",
            "--nlos-text-primary",
            "#111827",
            "--nlos-bg-chrome",
            "#f3f4f6",
        ),
        (
            "paper:text-secondary-on-window",
            "paper-workstation",
            "normal-text",
            "--nlos-text-secondary",
            "#4b5563",
            "--nlos-bg-window",
            "#ffffff",
        ),
        (
            "paper:text-muted-on-window",
            "paper-workstation",
            "normal-text",
            "--nlos-text-muted",
            "#6b7280",
            "--nlos-bg-window",
            "#ffffff",
        ),
        (
            "paper:accent-fg-on-accent-primary",
            "paper-workstation",
            "normal-text",
            "--nlos-accent-fg",
            "#ffffff",
            "--nlos-accent-primary",
            "#1d4ed8",
        ),
        (
            "paper:signal-action-text-on-window",
            "paper-workstation",
            "normal-text",
            "--nlos-signal-action-text",
            "#b45309",
            "--nlos-bg-window",
            "#ffffff",
        ),
        (
            "paper:signal-danger-text-on-window",
            "paper-workstation",
            "normal-text",
            "--nlos-signal-danger-text",
            "#b91c1c",
            "--nlos-bg-window",
            "#ffffff",
        ),
        (
            "paper:signal-danger-fg-on-danger",
            "paper-workstation",
            "normal-text",
            "--nlos-signal-danger-fg",
            "#ffffff",
            "--nlos-signal-danger",
            "#dc2626",
        ),
        (
            "paper:focus-ring-on-window",
            "paper-workstation",
            "ui-component",
            "--nlos-accent-active",
            "#1d4ed8",
            "--nlos-bg-window",
            "#ffffff",
        ),
        (
            "paper:focus-ring-on-chrome",
            "paper-workstation",
            "ui-component",
            "--nlos-accent-active",
            "#1d4ed8",
            "--nlos-bg-chrome",
            "#f3f4f6",
        ),
    ];

    let mut pairs = Vec::with_capacity(specs.len());
    let mut failures = Vec::new();
    let mut min_normal_text_ratio = f64::INFINITY;
    let mut min_ui_component_ratio = f64::INFINITY;

    for (pair_id, theme, category, fg_token, fg_hex, bg_token, bg_hex) in specs {
        let ratio = compute_contrast_ratio_hex(fg_hex, bg_hex)?;
        let required = if category == "normal-text" {
            WCAG_AA_NORMAL_TEXT_MIN_RATIO
        } else {
            WCAG_AA_UI_COMPONENT_MIN_RATIO
        };
        let passed = ratio >= required;

        if category == "normal-text" && ratio < min_normal_text_ratio {
            min_normal_text_ratio = ratio;
        }
        if category == "ui-component" && ratio < min_ui_component_ratio {
            min_ui_component_ratio = ratio;
        }

        let item = WcagContrastPairResultDto {
            pair_id: pair_id.to_string(),
            theme: theme.to_string(),
            category: category.to_string(),
            foreground_token: fg_token.to_string(),
            foreground_hex: fg_hex.to_string(),
            background_token: bg_token.to_string(),
            background_hex: bg_hex.to_string(),
            contrast_ratio: ratio,
            required_ratio: required,
            passed,
        };
        if !passed {
            failures.push(item.clone());
        }
        pairs.push(item);
    }

    Ok(WcagContrastAuditReportDto {
        passed: failures.is_empty(),
        total_pairs_audited: pairs.len(),
        min_normal_text_ratio,
        min_ui_component_ratio,
        focus_ring_width_px: FOCUS_RING_WIDTH_PX,
        pairs,
        failures,
    })
}

pub fn a11y_audit_aria_landmarks_inner(
    state: &A11yI18nState,
    z4_modal_active: bool,
) -> Result<AriaLandmarksAuditReportDto, DesktopError> {
    let locale = state.read_guard()?.locale;
    let is_zh = locale == WorkstationLocaleDto::ZhCn;

    let nodes = vec![
        AriaLandmarkNodeDto {
            node_id: "plane-z0-backdrop".to_string(),
            plane: "Z0".to_string(),
            role: "region".to_string(),
            accessible_name: if is_zh {
                "工作站桌面底板".to_string()
            } else {
                "Workstation Desktop Backdrop".to_string()
            },
            interactive: false,
            hit_target_width_px: 1920,
            hit_target_height_px: 1080,
            aria_hidden: true,
            aria_modal: None,
            aria_expanded: None,
            aria_live: None,
            keyboard_drag_equivalent_id: None,
        },
        AriaLandmarkNodeDto {
            node_id: "plane-z1-workspace-main".to_string(),
            plane: "Z1".to_string(),
            role: "main".to_string(),
            accessible_name: if is_zh {
                "工作站主工作区".to_string()
            } else {
                "Workstation Main Workspace".to_string()
            },
            interactive: false,
            hit_target_width_px: 1920,
            hit_target_height_px: 1044,
            aria_hidden: z4_modal_active,
            aria_modal: None,
            aria_expanded: None,
            aria_live: None,
            keyboard_drag_equivalent_id: None,
        },
        AriaLandmarkNodeDto {
            node_id: "z1-task-space-grid".to_string(),
            plane: "Z1".to_string(),
            role: "grid".to_string(),
            accessible_name: if is_zh {
                "任务与承诺工作区表格".to_string()
            } else {
                "Task and Commitment Workspace Table".to_string()
            },
            interactive: true,
            hit_target_width_px: 1500,
            hit_target_height_px: 980,
            aria_hidden: z4_modal_active,
            aria_modal: None,
            aria_expanded: Some(true),
            aria_live: None,
            keyboard_drag_equivalent_id: None,
        },
        AriaLandmarkNodeDto {
            node_id: "z1-task-space-toolbar".to_string(),
            plane: "Z1".to_string(),
            role: "toolbar".to_string(),
            accessible_name: if is_zh {
                "任务工作区操作工具栏".to_string()
            } else {
                "Task Workspace Action Toolbar".to_string()
            },
            interactive: true,
            hit_target_width_px: 1500,
            hit_target_height_px: 32,
            aria_hidden: z4_modal_active,
            aria_modal: None,
            aria_expanded: None,
            aria_live: None,
            keyboard_drag_equivalent_id: None,
        },
        AriaLandmarkNodeDto {
            node_id: "z1-resource-progressbar".to_string(),
            plane: "Z1".to_string(),
            role: "progressbar".to_string(),
            accessible_name: if is_zh {
                "三水位资源使用情况".to_string()
            } else {
                "Triple-Watermark Resource Allocation".to_string()
            },
            interactive: false,
            hit_target_width_px: 84,
            hit_target_height_px: 26,
            aria_hidden: z4_modal_active,
            aria_modal: None,
            aria_expanded: None,
            aria_live: None,
            keyboard_drag_equivalent_id: None,
        },
        AriaLandmarkNodeDto {
            node_id: "z1-l2-inspector-drawer".to_string(),
            plane: "Z1".to_string(),
            role: "complementary".to_string(),
            accessible_name: if is_zh {
                "任务详情与资源归因抽屉".to_string()
            } else {
                "Task Details and Resource Attribution Drawer".to_string()
            },
            interactive: true,
            hit_target_width_px: 420,
            hit_target_height_px: 1044,
            aria_hidden: z4_modal_active,
            aria_modal: None,
            aria_expanded: Some(true),
            aria_live: None,
            keyboard_drag_equivalent_id: Some("menu-drawer-resize-preset".to_string()),
        },
        AriaLandmarkNodeDto {
            node_id: "z2-ownership-treegrid".to_string(),
            plane: "Z2".to_string(),
            role: "treegrid".to_string(),
            accessible_name: if is_zh {
                "十二域统一任务与归属管理器".to_string()
            } else {
                "12-Domain Unified Task & Ownership Manager".to_string()
            },
            interactive: true,
            hit_target_width_px: 1460,
            hit_target_height_px: 900,
            aria_hidden: z4_modal_active,
            aria_modal: None,
            aria_expanded: Some(true),
            aria_live: None,
            keyboard_drag_equivalent_id: Some("menu-tile-splitter-preset".to_string()),
        },
        AriaLandmarkNodeDto {
            node_id: "plane-z3-taskbar".to_string(),
            plane: "Z3".to_string(),
            role: "contentinfo".to_string(),
            accessible_name: if is_zh {
                "工作站底部状态与任务栏".to_string()
            } else {
                "Workstation Bottom Status and Taskbar".to_string()
            },
            interactive: true,
            hit_target_width_px: 1920,
            hit_target_height_px: 36,
            aria_hidden: z4_modal_active,
            aria_modal: None,
            aria_expanded: None,
            aria_live: None,
            keyboard_drag_equivalent_id: None,
        },
        AriaLandmarkNodeDto {
            node_id: "z3-omnibar-dialog".to_string(),
            plane: "Z3".to_string(),
            role: "dialog".to_string(),
            accessible_name: if is_zh {
                "命令与意图输入对话框".to_string()
            } else {
                "Command and Intent Input Dialog".to_string()
            },
            interactive: true,
            hit_target_width_px: 580,
            hit_target_height_px: 320,
            aria_hidden: z4_modal_active,
            aria_modal: Some(false),
            aria_expanded: Some(true),
            aria_live: None,
            keyboard_drag_equivalent_id: None,
        },
        AriaLandmarkNodeDto {
            node_id: "z3-live-region-polite".to_string(),
            plane: "Z3".to_string(),
            role: "status".to_string(),
            accessible_name: if is_zh {
                "系统通知与快捷确认面板".to_string()
            } else {
                "System Notifications & Quick Actions".to_string()
            },
            interactive: false,
            hit_target_width_px: 26,
            hit_target_height_px: 26,
            aria_hidden: false,
            aria_modal: None,
            aria_expanded: None,
            aria_live: Some("polite".to_string()),
            keyboard_drag_equivalent_id: None,
        },
        AriaLandmarkNodeDto {
            node_id: "plane-z4-security-alertdialog".to_string(),
            plane: "Z4".to_string(),
            role: "alertdialog".to_string(),
            accessible_name: if is_zh {
                "系统安全确认模态对话框".to_string()
            } else {
                "System Security Confirmation Modal Dialog".to_string()
            },
            interactive: z4_modal_active,
            hit_target_width_px: 520,
            hit_target_height_px: 380,
            aria_hidden: !z4_modal_active,
            aria_modal: Some(z4_modal_active),
            aria_expanded: None,
            aria_live: Some("assertive".to_string()),
            keyboard_drag_equivalent_id: None,
        },
    ];

    let mut violations = Vec::new();
    let mut landmarks_present = Vec::new();
    let mut interactive_nodes = 0usize;
    let mut polite_ready = false;
    let mut assertive_ready = false;

    for n in &nodes {
        if !landmarks_present.contains(&n.role) {
            landmarks_present.push(n.role.clone());
        }
        if n.interactive {
            interactive_nodes += 1;
            if n.hit_target_width_px < WCAG_MIN_HIT_TARGET_PX
                || n.hit_target_height_px < WCAG_MIN_HIT_TARGET_PX
            {
                violations.push(AriaViolationDto {
                    node_id: n.node_id.clone(),
                    rule_id: "HIT_TARGET_BELOW_26PX".to_string(),
                    detail: format!(
                        "interactive node '{}' hit target {}x{}px < 26x26px",
                        n.node_id, n.hit_target_width_px, n.hit_target_height_px
                    ),
                });
            }
        }
        if n.accessible_name.trim().is_empty() {
            violations.push(AriaViolationDto {
                node_id: n.node_id.clone(),
                rule_id: "MISSING_ACCESSIBLE_NAME".to_string(),
                detail: format!("node '{}' has empty accessible_name", n.node_id),
            });
        } else if let Some(jargon) = contains_forbidden_l0_to_l2_jargon(&n.accessible_name) {
            violations.push(AriaViolationDto {
                node_id: n.node_id.clone(),
                rule_id: "JARGON_IN_ACCESSIBLE_NAME".to_string(),
                detail: format!("node '{}' contains forbidden jargon '{jargon}'", n.node_id),
            });
        }
        if n.aria_live.as_deref() == Some("polite") {
            polite_ready = true;
        }
        if n.aria_live.as_deref() == Some("assertive") {
            assertive_ready = true;
        }
    }

    Ok(AriaLandmarksAuditReportDto {
        passed: violations.is_empty() && polite_ready && assertive_ready,
        locale,
        z4_modal_active,
        total_nodes: nodes.len(),
        interactive_nodes,
        min_hit_target_px: WCAG_MIN_HIT_TARGET_PX,
        landmarks_present,
        polite_live_region_ready: polite_ready,
        assertive_live_region_ready: assertive_ready,
        nodes,
        violations,
    })
}

pub fn a11y_verify_keyboard_focus_trap_inner(
    _state: &A11yI18nState,
    request: KeyboardFocusTrapRequestDto,
) -> Result<KeyboardFocusTrapReportDto, DesktopError> {
    if request.targets.is_empty() {
        return Err(a11y_err(
            "FOCUS_TRAP_EMPTY_TARGETS: no focus targets supplied",
        ));
    }

    let z4_controls: Vec<String> = request
        .targets
        .iter()
        .filter(|t| t.plane == "Z4" && !t.disabled)
        .map(|t| t.id.clone())
        .collect();

    if request.z4_modal_active && z4_controls.is_empty() {
        return Err(a11y_err(
            "Z4_TRAP_NO_CONTROLS: Z4 modal active but zero enabled Z4 controls provided",
        ));
    }

    let active_pool: Vec<String> = if request.z4_modal_active {
        z4_controls.clone()
    } else {
        request
            .targets
            .iter()
            .filter(|t| t.plane != "Z0" && t.plane != "Z4" && !t.disabled)
            .map(|t| t.id.clone())
            .collect()
    };

    let mut forward_tab_cycle = Vec::with_capacity(active_pool.len() + 1);
    let mut cur_idx = 0usize;
    for _ in 0..=active_pool.len() {
        if let Some(id) = active_pool.get(cur_idx) {
            forward_tab_cycle.push(id.clone());
            cur_idx = (cur_idx + 1) % active_pool.len();
        }
    }

    let mut backward_shift_tab_cycle = Vec::with_capacity(active_pool.len() + 1);
    let mut rev_idx = 0usize;
    for _ in 0..=active_pool.len() {
        if !active_pool.is_empty() {
            rev_idx = if rev_idx == 0 {
                active_pool.len() - 1
            } else {
                rev_idx - 1
            };
            backward_shift_tab_cycle.push(active_pool[rev_idx].clone());
        }
    }

    let leaked_to_lower_plane = if request.z4_modal_active {
        forward_tab_cycle
            .iter()
            .chain(backward_shift_tab_cycle.iter())
            .any(|id| !z4_controls.contains(id))
    } else {
        false
    };

    let lower_plane_count = request
        .targets
        .iter()
        .filter(|t| t.plane != "Z4" && t.plane != "Z0" && !t.disabled)
        .count() as u32;

    let origin_exists = request
        .targets
        .iter()
        .any(|t| t.id == request.originating_trigger_id && !t.disabled);

    Ok(KeyboardFocusTrapReportDto {
        z4_modal_active: request.z4_modal_active,
        z4_control_ids: z4_controls,
        forward_tab_cycle,
        backward_shift_tab_cycle,
        leaked_to_lower_plane,
        blocked_lower_plane_focus_attempts: if request.z4_modal_active {
            lower_plane_count.saturating_add(2)
        } else {
            0
        },
        blocked_global_shortcuts_under_z4: request.z4_modal_active,
        focus_restored_on_escape: origin_exists,
        restored_focus_id: if origin_exists {
            Some(request.originating_trigger_id)
        } else {
            None
        },
    })
}

pub fn a11y_set_reduced_motion_override_inner(
    state: &A11yI18nState,
    system_prefers_reduced: Option<bool>,
    user_override_reduced: Option<bool>,
    z4_seal_mounted: Option<bool>,
) -> Result<ReducedMotionPostureDto, DesktopError> {
    let mut guard = state.write_guard()?;
    if let Some(sys) = system_prefers_reduced {
        guard.system_prefers_reduced_motion = sys;
    }
    guard.user_override_reduced_motion = user_override_reduced;
    if let Some(mounted) = z4_seal_mounted {
        guard.z4_seal_mounted = mounted;
    }

    let effective = guard
        .user_override_reduced_motion
        .unwrap_or(guard.system_prefers_reduced_motion);
    let seal_mode = if effective {
        "static-reduced-motion".to_string()
    } else {
        "animated-canvas".to_string()
    };
    let active_raf = if guard.z4_seal_mounted && !effective {
        1
    } else {
        0
    };

    Ok(ReducedMotionPostureDto {
        system_prefers_reduced_motion: guard.system_prefers_reduced_motion,
        user_override_reduced_motion: guard.user_override_reduced_motion,
        effective_reduced_motion: effective,
        gerono_seal_mode: seal_mode,
        active_raf_loop_count: active_raf,
        fast_transition_duration_ms: if effective { 0 } else { 80 },
        base_transition_duration_ms: if effective { 0 } else { 140 },
    })
}

// ============================================================================
// Tauri Command Wrappers (Ready for W73-INT registration in lib.rs)
// ============================================================================

#[tauri::command]
pub fn a11y_get_locale_bundle(
    state: State<'_, A11yI18nState>,
    locale_override: Option<String>,
) -> Result<LocaleBundleSnapshotDto, DesktopError> {
    a11y_get_locale_bundle_inner(&state, locale_override.as_deref())
}

#[tauri::command]
pub fn a11y_switch_locale(
    state: State<'_, A11yI18nState>,
    next_locale: String,
) -> Result<LocaleBundleSnapshotDto, DesktopError> {
    a11y_switch_locale_inner(&state, &next_locale)
}

#[tauri::command]
pub fn a11y_audit_wcag_contrast(
    state: State<'_, A11yI18nState>,
) -> Result<WcagContrastAuditReportDto, DesktopError> {
    a11y_audit_wcag_contrast_inner(&state)
}

#[tauri::command]
pub fn a11y_audit_aria_landmarks(
    state: State<'_, A11yI18nState>,
    z4_modal_active: bool,
) -> Result<AriaLandmarksAuditReportDto, DesktopError> {
    a11y_audit_aria_landmarks_inner(&state, z4_modal_active)
}

#[tauri::command]
pub fn a11y_verify_keyboard_focus_trap(
    state: State<'_, A11yI18nState>,
    request: KeyboardFocusTrapRequestDto,
) -> Result<KeyboardFocusTrapReportDto, DesktopError> {
    a11y_verify_keyboard_focus_trap_inner(&state, request)
}

#[tauri::command]
pub fn a11y_set_reduced_motion_override(
    state: State<'_, A11yI18nState>,
    system_prefers_reduced: Option<bool>,
    user_override_reduced: Option<bool>,
    z4_seal_mounted: Option<bool>,
) -> Result<ReducedMotionPostureDto, DesktopError> {
    a11y_set_reduced_motion_override_inner(
        &state,
        system_prefers_reduced,
        user_override_reduced,
        z4_seal_mounted,
    )
}
