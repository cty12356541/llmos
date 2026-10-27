//! Stage D (`[DUI-WINDOW-001]`, `[DUI-LAUNCH-001]`, `ADR-0025` 决定 1) 桌面合成器、
//! 五平面 Z 序模型、确定性平铺/浮动几何引擎、子表面裁剪/遮挡适配器与应用启动器。
//!
//! 核心不变量：
//! - 底部 `36px` 固定为可信 `Z3` 系统任务栏，任何 `Z1` 任务空间或 `Z2` 应用窗口（含最大化与越界浮动）
//!   的 `y_max` 严格限制在 `<= viewport_h - 36`；
//! - 可信 `WindowFrame` 顶部 `30px` 标题栏与 `1px` 四周边框由宿主独占绘制，`SandboxedSurfaceHost`
//!   将不可信子表面裁剪在标题栏下方安全区（`y = outer.y + 30`, `height = outer.height - 31`,
//!   `x = outer.x + 1`, `width = outer.width - 2`）；
//! - 当 `Z4` 安全确认屏锁定或任意活动 `Z3` 浮层/命令面板矩形与窗口外框相交时，标记
//!   `occluded_by_system_plane = true`；
//! - `ApplicationLauncher` 严格按 `(ApplicationId, InstallationId)` 激活应用表面，类型化拒绝
//!   将 `PackageId` 或瞬态 `ProcessId` 冒充为应用身份的请求。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nlos_application::ApplicationAuthority;
use nlos_types::{ApplicationId, InstallationId, PackageId, ProcessId};

use crate::dto::hex;
use crate::input_router::{
    FocusAndInputRoute, InputEventEnvelope, InputRejection, RoutedInputOutcome, WindowInputGateView,
};
use crate::session::{
    DesktopSessionManager, DesktopSessionState, SessionError, derive_scoped_id_16,
};
use crate::surfaces::{
    AuthorityLaunchResolutionError, inspect_current_generation_by_package,
    resolve_launch_from_authority,
};
use crate::window_lifecycle::{
    CoupledWindowSurface, SurfaceLifecycle, WindowLifecycle, WindowLifecycleError,
};

/// 底部 `Z3` 系统任务栏固定像素高度。
pub const TASKBAR_HEIGHT_PX: u32 = 36;
/// 可信 `Z2` 窗口外框标题栏固定像素高度。
pub const WINDOW_TITLEBAR_HEIGHT_PX: u32 = 30;
/// 平铺工作站窗格间锐利间隙像素宽度。
pub const TILE_GAP_PX: u32 = 4;
/// 平铺模式窗口圆角（`0px` 直角）。
pub const TILED_BORDER_RADIUS_PX: u8 = 0;
/// 浮动模式窗口圆角（`6px` 克制工业圆角）。
pub const FLOATING_BORDER_RADIUS_PX: u8 = 6;

/// `Z0` 工作站底板平面 Z-index。
pub const Z0_BACKDROP_Z: u16 = 0;
/// `Z1` 停靠自然语言任务空间平面 Z-index。
pub const Z1_TASK_SPACE_Z: u16 = 100;
/// `Z2` 应用窗口平面最小 Z-index。
pub const Z2_WINDOW_Z_MIN: u16 = 200;
/// `Z2` 应用窗口平面最大 Z-index（严禁越界至 `Z3`/`Z4`）。
pub const Z2_WINDOW_Z_MAX: u16 = 299;
/// `Z3` 系统任务栏固定 Z-index。
pub const Z3_TASKBAR_Z: u16 = 400;
/// `Z3` 系统快捷浮层固定 Z-index。
pub const Z3_FLYOUT_Z: u16 = 430;
/// `Z3` 全局命令面板固定 Z-index。
pub const Z3_PALETTE_Z: u16 = 460;
/// `Z4` 不可遮挡系统安全确认屏固定 Z-index。
pub const Z4_SECURITY_SCREEN_Z: u16 = 900;

/// 默认桌面视口宽度。
pub const DEFAULT_VIEWPORT_WIDTH_PX: u32 = 1440;
/// 默认桌面视口高度。
pub const DEFAULT_VIEWPORT_HEIGHT_PX: u32 = 900;
/// 三窗格工作站默认顶部主工作区高度千分比（`68%`）。
pub const DEFAULT_TOP_HEIGHT_RATIO_PERMILLE: u16 = 680;
/// 默认左右分栏宽度千分比（`50%`）。
pub const DEFAULT_SPLIT_WIDTH_RATIO_PERMILLE: u16 = 500;
/// 可调分栏最小千分比（`20%`）。
pub const MIN_SPLIT_RATIO_PERMILLE: u16 = 200;
/// 可调分栏最大千分比（`80%`）。
pub const MAX_SPLIT_RATIO_PERMILLE: u16 = 800;

/// 整数像素矩形（确定性无浮点舍入）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RectPx {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl RectPx {
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub const fn right(&self) -> i32 {
        self.x.saturating_add_unsigned(self.width)
    }

    #[must_use]
    pub const fn bottom(&self) -> i32 {
        self.y.saturating_add_unsigned(self.height)
    }

    /// 判断两个非空像素矩形是否存在面积重叠。
    #[must_use]
    pub const fn intersects(&self, other: &Self) -> bool {
        if self.width == 0 || self.height == 0 || other.width == 0 || other.height == 0 {
            return false;
        }
        let self_left = self.x as i64;
        let self_right = self_left + self.width as i64;
        let self_top = self.y as i64;
        let self_bottom = self_top + self.height as i64;

        let other_left = other.x as i64;
        let other_right = other_left + other.width as i64;
        let other_top = other.y as i64;
        let other_bottom = other_top + other.height as i64;

        self_left < other_right
            && other_left < self_right
            && self_top < other_bottom
            && other_top < self_bottom
    }
}

/// 计算扣除底部 `36px` 系统任务栏后的工作舞台矩形 `StageRect`。
#[must_use]
pub const fn compute_stage_rect(viewport_w: u32, viewport_h: u32) -> RectPx {
    RectPx {
        x: 0,
        y: 0,
        width: viewport_w,
        height: viewport_h.saturating_sub(TASKBAR_HEIGHT_PX),
    }
}

/// 计算底部 `36px` `Z3` 系统任务栏矩形。
#[must_use]
pub const fn compute_taskbar_rect(viewport_w: u32, viewport_h: u32) -> RectPx {
    let raw_y = viewport_h.saturating_sub(TASKBAR_HEIGHT_PX);
    let y = if raw_y > i32::MAX as u32 {
        i32::MAX
    } else {
        raw_y as i32
    };
    let height = if viewport_h < TASKBAR_HEIGHT_PX {
        viewport_h
    } else {
        TASKBAR_HEIGHT_PX
    };
    RectPx {
        x: 0,
        y,
        width: viewport_w,
        height,
    }
}

/// 钳制浮动窗口矩形，确保标题栏始终可触达且绝不侵入底部 `36px` 任务栏。
#[must_use]
pub fn clamp_floating_rect(desired: RectPx, viewport_w: u32, viewport_h: u32) -> RectPx {
    let stage = compute_stage_rect(viewport_w, viewport_h);
    if stage.width == 0 || stage.height == 0 {
        return RectPx::new(0, 0, 0, 0);
    }
    let min_w = 240_u32.min(stage.width);
    let min_h = WINDOW_TITLEBAR_HEIGHT_PX
        .saturating_add(32)
        .min(stage.height);
    let width = desired.width.clamp(min_w, stage.width);
    let height = desired.height.clamp(min_h, stage.height);
    let max_x = i64::from(stage.width.saturating_sub(width));
    let max_y = i64::from(stage.height.saturating_sub(height));
    let x = i64::from(desired.x).clamp(0, max_x) as i32;
    let y = i64::from(desired.y).clamp(0, max_y) as i32;
    RectPx {
        x,
        y,
        width,
        height,
    }
}

/// 五种工业工作站窗口布局模式（`ADR-0025` 决定 3）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum WorkstationLayoutMode {
    #[default]
    #[serde(rename = "three-pane-workstation")]
    ThreePaneWorkstation,
    #[serde(rename = "two-column-split")]
    TwoColumnSplit,
    #[serde(rename = "quadrants")]
    Quadrants,
    #[serde(rename = "workspace-maximize")]
    WorkspaceMaximize,
    #[serde(rename = "floating")]
    Floating,
}

impl WorkstationLayoutMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThreePaneWorkstation => "three-pane-workstation",
            Self::TwoColumnSplit => "two-column-split",
            Self::Quadrants => "quadrants",
            Self::WorkspaceMaximize => "workspace-maximize",
            Self::Floating => "floating",
        }
    }

    /// 解析布局模式标识。
    ///
    /// # Errors
    ///
    /// 未知布局模式返回 [`CompositorError::InvalidLayoutMode`]。
    pub fn parse(raw: &str) -> Result<Self, CompositorError> {
        match raw.trim() {
            "three-pane-workstation" | "three_pane_workstation" | "ThreePaneWorkstation" => {
                Ok(Self::ThreePaneWorkstation)
            }
            "two-column-split" | "two_column_split" | "TwoColumnSplit" => Ok(Self::TwoColumnSplit),
            "quadrants" | "Quadrants" => Ok(Self::Quadrants),
            "workspace-maximize" | "workspace_maximize" | "WorkspaceMaximize" => {
                Ok(Self::WorkspaceMaximize)
            }
            "floating" | "Floating" => Ok(Self::Floating),
            other => Err(CompositorError::InvalidLayoutMode(other.to_owned())),
        }
    }
}

/// `Z3` 系统浮层类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Z3OverlayKind {
    Flyout,
    Palette,
}

impl Z3OverlayKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Flyout => "flyout",
            Self::Palette => "palette",
        }
    }

    #[must_use]
    pub const fn z_index(self) -> u16 {
        match self {
            Self::Flyout => Z3_FLYOUT_Z,
            Self::Palette => Z3_PALETTE_Z,
        }
    }

    /// 解析 `Z3` 浮层种类。
    ///
    /// # Errors
    ///
    /// 非法类别返回 [`CompositorError::InvalidOverlayKind`]。
    pub fn parse(raw: &str) -> Result<Self, CompositorError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "flyout" => Ok(Self::Flyout),
            "palette" => Ok(Self::Palette),
            other => Err(CompositorError::InvalidOverlayKind(other.to_owned())),
        }
    }
}

/// 活动中的 `Z3` 系统浮层或全局命令面板。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositorZ3Overlay {
    pub overlay_id: String,
    pub kind: Z3OverlayKind,
    pub rect: RectPx,
    pub z_index: u16,
}

/// 子表面裁剪与系统平面遮挡计算结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceHostPlacement {
    pub inner_content_clip_rect: RectPx,
    pub occluded_by_system_plane: bool,
}

/// 受控子表面（受限 WebView / 原生子视图）视口裁剪与系统平面防伪遮挡适配器契约。
pub trait SurfaceHostAdapter: Send + Sync {
    fn compute_surface_placement(
        &self,
        outer_frame_rect: RectPx,
        lifecycle: CoupledWindowSurface,
        viewport_w: u32,
        viewport_h: u32,
        z4_modal_lock: bool,
        active_z3_overlays: &[CompositorZ3Overlay],
    ) -> SurfaceHostPlacement;
}

/// 默认沙箱表面宿主实现：强制保留顶部 `30px` 可信标题栏、`1px` 边框与底部 `36px` 任务栏净空。
#[derive(Clone, Copy, Debug, Default)]
pub struct SandboxedSurfaceHost;

impl SurfaceHostAdapter for SandboxedSurfaceHost {
    fn compute_surface_placement(
        &self,
        outer_frame_rect: RectPx,
        lifecycle: CoupledWindowSurface,
        viewport_w: u32,
        viewport_h: u32,
        z4_modal_lock: bool,
        active_z3_overlays: &[CompositorZ3Overlay],
    ) -> SurfaceHostPlacement {
        let stage = compute_stage_rect(viewport_w, viewport_h);
        let occluded_by_system_plane = z4_modal_lock
            || active_z3_overlays
                .iter()
                .any(|overlay| overlay.rect.intersects(&outer_frame_rect));

        if lifecycle.surface != SurfaceLifecycle::Presented
            || lifecycle.window != WindowLifecycle::Visible
            || stage.width == 0
            || stage.height == 0
        {
            let clamped_x = outer_frame_rect.x.saturating_add(1).clamp(0, stage.right());
            let clamped_y = outer_frame_rect
                .y
                .saturating_add_unsigned(WINDOW_TITLEBAR_HEIGHT_PX)
                .clamp(0, stage.bottom());
            return SurfaceHostPlacement {
                inner_content_clip_rect: RectPx::new(clamped_x, clamped_y, 0, 0),
                occluded_by_system_plane,
            };
        }

        let raw_x = i64::from(outer_frame_rect.x) + 1;
        let raw_y = i64::from(outer_frame_rect.y) + i64::from(WINDOW_TITLEBAR_HEIGHT_PX);
        let raw_w = i64::from(outer_frame_rect.width.saturating_sub(2));
        let raw_h = i64::from(
            outer_frame_rect
                .height
                .saturating_sub(WINDOW_TITLEBAR_HEIGHT_PX + 1),
        );

        let stage_w = i64::from(stage.width);
        let stage_h = i64::from(stage.height);

        let left = raw_x.clamp(0, stage_w);
        let top = raw_y.clamp(0, stage_h);
        let right = (raw_x + raw_w).clamp(left, stage_w);
        let bottom = (raw_y + raw_h).clamp(top, stage_h);

        SurfaceHostPlacement {
            inner_content_clip_rect: RectPx {
                x: left as i32,
                y: top as i32,
                width: (right - left) as u32,
                height: (bottom - top) as u32,
            },
            occluded_by_system_plane,
        }
    }
}

/// 窗口无障碍树元数据节点（对齐 `ADR-0025` 与视觉设计规范无障碍门禁）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowAccessibilityNode {
    pub a11y_node_id: String,
    pub a11y_role: String,
    pub a11y_label: String,
    pub owner_application_id: [u8; 16],
    pub installation_generation: u64,
    pub stale_readonly: bool,
    pub hidden: bool,
}

impl WindowAccessibilityNode {
    #[must_use]
    pub fn new(
        window_id: [u8; 16],
        title: &str,
        owner_application_id: [u8; 16],
        installation_generation: u64,
        stale_readonly: bool,
        hidden: bool,
    ) -> Self {
        Self {
            a11y_node_id: format!("llmos-win-{}", hex(&window_id)),
            a11y_role: "region".to_owned(),
            a11y_label: format!("Window: {title}"),
            owner_application_id,
            installation_generation,
            stale_readonly,
            hidden,
        }
    }
}

/// 合成器受管 `Z2` 应用窗口实体。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositorWindow {
    pub window_id: [u8; 16],
    pub surface_id: [u8; 16],
    pub title: String,
    pub entry_name: Option<String>,
    pub owner_application_id: [u8; 16],
    pub installation_id: [u8; 16],
    pub package_id: [u8; 16],
    pub workspace_id: [u8; 16],
    pub session_generation: u64,
    pub surface_generation: u64,
    pub current_installation_generation: u64,
    pub stale_generation: bool,
    pub lifecycle: CoupledWindowSurface,
    pub layout_slot: String,
    pub is_floating: bool,
    pub floating_desired_rect: RectPx,
    pub z_index: u16,
    pub border_radius_px: u8,
    pub outer_frame_rect: RectPx,
    pub inner_content_clip_rect: RectPx,
    pub occluded_by_system_plane: bool,
    pub a11y: WindowAccessibilityNode,
}

impl CompositorWindow {
    #[must_use]
    pub const fn input_gate_view(&self) -> WindowInputGateView {
        WindowInputGateView {
            window_id: self.window_id,
            surface_id: self.surface_id,
            owner_application_id: self.owner_application_id,
            surface_generation: self.surface_generation,
            current_installation_generation: self.current_installation_generation,
            stale_generation: self.stale_generation,
            lifecycle: self.lifecycle,
        }
    }
}

/// 合成器类型化错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompositorError {
    InvalidLayoutMode(String),
    InvalidOverlayKind(String),
    InvalidWindowAction(String),
    WindowNotFound {
        window_id: [u8; 16],
    },
    StaleWindowRequiresReloadOrClose {
        window_id: [u8; 16],
        surface_generation: u64,
        current_installation_generation: u64,
    },
    WindowLifecycle(WindowLifecycleError),
    Session(SessionError),
    Launcher(LauncherError),
}

impl fmt::Display for CompositorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLayoutMode(mode) => {
                write!(formatter, "unsupported workstation layout mode: {mode}")
            }
            Self::InvalidOverlayKind(kind) => {
                write!(formatter, "unsupported Z3 overlay kind: {kind}")
            }
            Self::InvalidWindowAction(action) => {
                write!(formatter, "unsupported compositor window action: {action}")
            }
            Self::WindowNotFound { window_id } => {
                write!(formatter, "compositor window {} not found", hex(window_id))
            }
            Self::StaleWindowRequiresReloadOrClose {
                window_id,
                surface_generation,
                current_installation_generation,
            } => write!(
                formatter,
                "stale window {} (surface gen {surface_generation} < current gen {current_installation_generation}) must be reloaded or closed before focusing",
                hex(window_id)
            ),
            Self::WindowLifecycle(err) => write!(formatter, "{err}"),
            Self::Session(err) => write!(formatter, "{err}"),
            Self::Launcher(err) => write!(formatter, "{err}"),
        }
    }
}

impl std::error::Error for CompositorError {}

impl From<WindowLifecycleError> for CompositorError {
    fn from(value: WindowLifecycleError) -> Self {
        Self::WindowLifecycle(value)
    }
}

impl From<SessionError> for CompositorError {
    fn from(value: SessionError) -> Self {
        Self::Session(value)
    }
}

impl From<LauncherError> for CompositorError {
    fn from(value: LauncherError) -> Self {
        Self::Launcher(value)
    }
}

/// 桌面五平面几何合成器。
pub struct DesktopCompositor {
    viewport_width: u32,
    viewport_height: u32,
    layout_mode: WorkstationLayoutMode,
    top_height_ratio_permille: u16,
    split_width_ratio_permille: u16,
    z1_task_space_visible: bool,
    windows: Vec<CompositorWindow>,
    mru_order: Vec<[u8; 16]>,
    active_z3_overlays: BTreeMap<String, CompositorZ3Overlay>,
    surface_host: Box<dyn SurfaceHostAdapter>,
}

impl fmt::Debug for DesktopCompositor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DesktopCompositor")
            .field("viewport_width", &self.viewport_width)
            .field("viewport_height", &self.viewport_height)
            .field("layout_mode", &self.layout_mode)
            .field("top_height_ratio_permille", &self.top_height_ratio_permille)
            .field(
                "split_width_ratio_permille",
                &self.split_width_ratio_permille,
            )
            .field("z1_task_space_visible", &self.z1_task_space_visible)
            .field("windows_len", &self.windows.len())
            .field("active_z3_overlays_len", &self.active_z3_overlays.len())
            .finish_non_exhaustive()
    }
}

impl Default for DesktopCompositor {
    fn default() -> Self {
        Self::new(DEFAULT_VIEWPORT_WIDTH_PX, DEFAULT_VIEWPORT_HEIGHT_PX)
    }
}

impl DesktopCompositor {
    #[must_use]
    pub fn new(viewport_width: u32, viewport_height: u32) -> Self {
        Self::with_surface_host(
            viewport_width,
            viewport_height,
            Box::new(SandboxedSurfaceHost),
        )
    }

    #[must_use]
    pub fn with_surface_host(
        viewport_width: u32,
        viewport_height: u32,
        surface_host: Box<dyn SurfaceHostAdapter>,
    ) -> Self {
        Self {
            viewport_width: viewport_width.max(320),
            viewport_height: viewport_height.max(240),
            layout_mode: WorkstationLayoutMode::ThreePaneWorkstation,
            top_height_ratio_permille: DEFAULT_TOP_HEIGHT_RATIO_PERMILLE,
            split_width_ratio_permille: DEFAULT_SPLIT_WIDTH_RATIO_PERMILLE,
            z1_task_space_visible: true,
            windows: Vec::new(),
            mru_order: Vec::new(),
            active_z3_overlays: BTreeMap::new(),
            surface_host,
        }
    }

    #[must_use]
    pub const fn viewport_rect(&self) -> RectPx {
        RectPx::new(0, 0, self.viewport_width, self.viewport_height)
    }

    #[must_use]
    pub const fn stage_rect(&self) -> RectPx {
        compute_stage_rect(self.viewport_width, self.viewport_height)
    }

    #[must_use]
    pub const fn taskbar_rect(&self) -> RectPx {
        compute_taskbar_rect(self.viewport_width, self.viewport_height)
    }

    #[must_use]
    pub const fn layout_mode(&self) -> WorkstationLayoutMode {
        self.layout_mode
    }

    #[must_use]
    pub const fn top_height_ratio_permille(&self) -> u16 {
        self.top_height_ratio_permille
    }

    #[must_use]
    pub const fn split_width_ratio_permille(&self) -> u16 {
        self.split_width_ratio_permille
    }

    #[must_use]
    pub const fn z1_task_space_visible(&self) -> bool {
        self.z1_task_space_visible
    }

    /// 计算停靠式 `Z1` 任务空间矩形（仅在 `ThreePaneWorkstation` 且 `z1_task_space_visible` 时占据底部 `32%`）。
    #[must_use]
    pub fn z1_task_space_rect(&self) -> Option<RectPx> {
        if self.layout_mode != WorkstationLayoutMode::ThreePaneWorkstation
            || !self.z1_task_space_visible
        {
            return None;
        }
        let stage = self.stage_rect();
        if stage.height == 0 {
            return Some(RectPx::new(0, 0, stage.width, 0));
        }
        let usable_h = stage.height.saturating_sub(TILE_GAP_PX);
        let top_h =
            ((u64::from(usable_h) * u64::from(self.top_height_ratio_permille)) / 1000) as u32;
        let bottom_y = top_h.saturating_add(TILE_GAP_PX).min(stage.height) as i32;
        let bottom_h = usable_h.saturating_sub(top_h);
        Some(RectPx::new(0, bottom_y, stage.width, bottom_h))
    }

    #[must_use]
    pub fn active_z3_overlays(&self) -> Vec<CompositorZ3Overlay> {
        self.active_z3_overlays.values().cloned().collect()
    }

    #[must_use]
    pub fn windows_for_workspace(&self, workspace_id: [u8; 16]) -> Vec<&CompositorWindow> {
        self.windows
            .iter()
            .filter(|win| win.workspace_id == workspace_id)
            .collect()
    }

    #[must_use]
    pub fn window(&self, window_id: [u8; 16]) -> Option<&CompositorWindow> {
        self.windows.iter().find(|win| win.window_id == window_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_layout_config(
        &mut self,
        layout_mode: WorkstationLayoutMode,
        viewport_width: Option<u32>,
        viewport_height: Option<u32>,
        top_height_ratio_permille: Option<u16>,
        split_width_ratio_permille: Option<u16>,
        z1_visible: Option<bool>,
        active_workspace_id: [u8; 16],
        z4_modal_lock: bool,
    ) {
        self.layout_mode = layout_mode;
        if let Some(vw) = viewport_width {
            self.viewport_width = vw.max(320);
        }
        if let Some(vh) = viewport_height {
            self.viewport_height = vh.max(240);
        }
        if let Some(top_ratio) = top_height_ratio_permille {
            self.top_height_ratio_permille =
                top_ratio.clamp(MIN_SPLIT_RATIO_PERMILLE, MAX_SPLIT_RATIO_PERMILLE);
        }
        if let Some(split_ratio) = split_width_ratio_permille {
            self.split_width_ratio_permille =
                split_ratio.clamp(MIN_SPLIT_RATIO_PERMILLE, MAX_SPLIT_RATIO_PERMILLE);
        }
        if let Some(visible) = z1_visible {
            self.z1_task_space_visible = visible;
        }
        self.recompute_workspace_geometry(active_workspace_id, z4_modal_lock);
    }

    pub fn set_z3_overlay(
        &mut self,
        overlay_id: impl Into<String>,
        kind: Z3OverlayKind,
        active: bool,
        rect: Option<RectPx>,
        active_workspace_id: [u8; 16],
        z4_modal_lock: bool,
    ) {
        let id = overlay_id.into();
        if active {
            let stage = self.stage_rect();
            let default_rect = match kind {
                Z3OverlayKind::Palette => {
                    let w = 640_u32.min(stage.width);
                    let h = 360_u32.min(stage.height);
                    let x = ((stage.width.saturating_sub(w)) / 2) as i32;
                    let y = 72_u32.min(stage.height.saturating_sub(h)) as i32;
                    RectPx::new(x, y, w, h)
                }
                Z3OverlayKind::Flyout => {
                    let w = 360_u32.min(stage.width);
                    let h = 280_u32.min(stage.height);
                    let x = stage.width.saturating_sub(w.saturating_add(12)) as i32;
                    let y = stage.height.saturating_sub(h.saturating_add(8)) as i32;
                    RectPx::new(x, y, w, h)
                }
            };
            let resolved_rect = rect.unwrap_or(default_rect);
            self.active_z3_overlays.insert(
                id.clone(),
                CompositorZ3Overlay {
                    overlay_id: id,
                    kind,
                    rect: resolved_rect,
                    z_index: kind.z_index(),
                },
            );
        } else {
            self.active_z3_overlays.remove(&id);
        }
        self.recompute_workspace_geometry(active_workspace_id, z4_modal_lock);
    }

    fn promote_mru(&mut self, window_id: [u8; 16]) {
        self.mru_order.retain(|existing| *existing != window_id);
        self.mru_order.push(window_id);
    }

    fn compute_tiled_slot(&self, slot_index: usize) -> (String, RectPx) {
        let stage = self.stage_rect();
        let usable_w = stage.width.saturating_sub(TILE_GAP_PX);
        let left_w =
            ((u64::from(usable_w) * u64::from(self.split_width_ratio_permille)) / 1000) as u32;
        let right_x = left_w.saturating_add(TILE_GAP_PX).min(stage.width) as i32;
        let right_w = usable_w.saturating_sub(left_w);

        match self.layout_mode {
            WorkstationLayoutMode::ThreePaneWorkstation => {
                let top_h = if self.z1_task_space_visible {
                    let usable_h = stage.height.saturating_sub(TILE_GAP_PX);
                    ((u64::from(usable_h) * u64::from(self.top_height_ratio_permille)) / 1000)
                        as u32
                } else {
                    stage.height
                };
                if slot_index == 0 {
                    ("pane-top-left".to_owned(), RectPx::new(0, 0, left_w, top_h))
                } else {
                    (
                        "pane-top-right".to_owned(),
                        RectPx::new(right_x, 0, right_w, top_h),
                    )
                }
            }
            WorkstationLayoutMode::TwoColumnSplit => {
                if slot_index == 0 {
                    (
                        "column-left".to_owned(),
                        RectPx::new(0, 0, left_w, stage.height),
                    )
                } else {
                    (
                        "column-right".to_owned(),
                        RectPx::new(right_x, 0, right_w, stage.height),
                    )
                }
            }
            WorkstationLayoutMode::Quadrants => {
                let usable_h = stage.height.saturating_sub(TILE_GAP_PX);
                let top_h = usable_h / 2;
                let bottom_y = top_h.saturating_add(TILE_GAP_PX).min(stage.height) as i32;
                let bottom_h = usable_h.saturating_sub(top_h);
                match slot_index % 4 {
                    0 => ("quadrant-tl".to_owned(), RectPx::new(0, 0, left_w, top_h)),
                    1 => (
                        "quadrant-tr".to_owned(),
                        RectPx::new(right_x, 0, right_w, top_h),
                    ),
                    2 => (
                        "quadrant-bl".to_owned(),
                        RectPx::new(0, bottom_y, left_w, bottom_h),
                    ),
                    _ => (
                        "quadrant-br".to_owned(),
                        RectPx::new(right_x, bottom_y, right_w, bottom_h),
                    ),
                }
            }
            WorkstationLayoutMode::WorkspaceMaximize => ("maximized".to_owned(), stage),
            WorkstationLayoutMode::Floating => {
                let offset = ((slot_index % 6) as i32) * 28;
                let desired = RectPx::new(64 + offset, 48 + offset, 760, 520);
                (
                    "floating".to_owned(),
                    clamp_floating_rect(desired, self.viewport_width, self.viewport_height),
                )
            }
        }
    }

    /// 重新计算指定工作区内全部窗口的外框几何、Z 序、标题栏下裁剪区与系统平面遮挡标记。
    pub fn recompute_workspace_geometry(&mut self, workspace_id: [u8; 16], z4_modal_lock: bool) {
        let overlays: Vec<CompositorZ3Overlay> =
            self.active_z3_overlays.values().cloned().collect();

        // 计算该工作区内可见非关闭窗口的 MRU Z-index 分配（200..=299）。
        let mut ordered_ids: Vec<[u8; 16]> = Vec::new();
        for win in &self.windows {
            if win.workspace_id == workspace_id && win.lifecycle.window != WindowLifecycle::Closed {
                ordered_ids.push(win.window_id);
            }
        }
        ordered_ids.sort_by_key(|id| {
            self.mru_order
                .iter()
                .position(|candidate| candidate == id)
                .unwrap_or(0)
        });
        let mut z_by_window: BTreeMap<[u8; 16], u16> = BTreeMap::new();
        for (idx, id) in ordered_ids.iter().enumerate() {
            let z = Z2_WINDOW_Z_MIN
                .saturating_add(idx.min(usize::from(Z2_WINDOW_Z_MAX - Z2_WINDOW_Z_MIN)) as u16)
                .min(Z2_WINDOW_Z_MAX);
            z_by_window.insert(*id, z);
        }

        let mut tiled_index = 0_usize;
        for idx in 0..self.windows.len() {
            if self.windows[idx].workspace_id != workspace_id {
                continue;
            }
            let effective_floating = self.layout_mode == WorkstationLayoutMode::Floating
                || self.windows[idx].is_floating;
            let is_visible = self.windows[idx].lifecycle.window == WindowLifecycle::Visible;

            let (slot_name, outer_rect, radius) = if effective_floating {
                let clamped = clamp_floating_rect(
                    self.windows[idx].floating_desired_rect,
                    self.viewport_width,
                    self.viewport_height,
                );
                ("floating".to_owned(), clamped, FLOATING_BORDER_RADIUS_PX)
            } else if is_visible {
                let (slot, rect) = self.compute_tiled_slot(tiled_index);
                tiled_index = tiled_index.saturating_add(1);
                (slot, rect, TILED_BORDER_RADIUS_PX)
            } else {
                (
                    self.windows[idx].layout_slot.clone(),
                    self.windows[idx].outer_frame_rect,
                    TILED_BORDER_RADIUS_PX,
                )
            };

            let lifecycle = self.windows[idx].lifecycle;
            let placement = self.surface_host.compute_surface_placement(
                outer_rect,
                lifecycle,
                self.viewport_width,
                self.viewport_height,
                z4_modal_lock,
                &overlays,
            );
            let assigned_z = z_by_window
                .get(&self.windows[idx].window_id)
                .copied()
                .unwrap_or(Z2_WINDOW_Z_MIN);

            let win = &mut self.windows[idx];
            win.layout_slot = slot_name;
            win.outer_frame_rect = outer_rect;
            win.border_radius_px = radius;
            win.z_index = assigned_z;
            win.inner_content_clip_rect = placement.inner_content_clip_rect;
            win.occluded_by_system_plane = placement.occluded_by_system_plane;
            win.a11y.hidden = !lifecycle.can_accept_input();
            win.a11y.stale_readonly = win.stale_generation;
            win.a11y.installation_generation = win.surface_generation;
        }
    }
}

/// 已安装应用目录中的可呈现窗口表面规格。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredWindowSurfaceSpec {
    pub surface_id: [u8; 16],
    pub title: String,
    pub entry_name: Option<String>,
}

/// 同步自 `ListInstalledApplications` / `ApplicationAuthority` 的应用启动目录项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledApplicationCatalogEntry {
    pub application_id: ApplicationId,
    pub installation_id: InstallationId,
    pub package_id: PackageId,
    pub current_installation_generation: u64,
    pub surfaces: Vec<DeclaredWindowSurfaceSpec>,
    pub known_process_ids: Vec<ProcessId>,
}

/// 应用启动身份选择器（用于类型化执行 `[DUI-LAUNCH-001]`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchIdentity {
    ApplicationInstallation {
        application_id: ApplicationId,
        installation_id: InstallationId,
    },
    PackageId(PackageId),
    ProcessId(ProcessId),
}

/// `[DUI-LAUNCH-001]` 启动器类型化错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LauncherError {
    PackageIdNotApplicationIdentity {
        offending_id: [u8; 16],
    },
    ProcessIdNotApplicationIdentity {
        offending_id: [u8; 16],
    },
    InstallationNotFound {
        installation_id: [u8; 16],
    },
    ApplicationInstallationMismatch {
        requested_application_id: [u8; 16],
        receipt_application_id: [u8; 16],
        installation_id: [u8; 16],
    },
    ApplicationNotInstalled {
        application_id: [u8; 16],
        status: String,
    },
    StaleInstallationGeneration {
        application_id: [u8; 16],
        installation_id: [u8; 16],
        requested_generation: u64,
        current_generation: u64,
    },
    NoPresentableSurfaces {
        application_id: [u8; 16],
        generation: u64,
    },
    SessionOrWorkspaceLocked(String),
    Storage(String),
}

impl fmt::Display for LauncherError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PackageIdNotApplicationIdentity { offending_id } => write!(
                formatter,
                "PACKAGE_ID_NOT_APPLICATION_IDENTITY: {} is a PackageId, not an ApplicationId/InstallationId ([DUI-LAUNCH-001])",
                hex(offending_id)
            ),
            Self::ProcessIdNotApplicationIdentity { offending_id } => write!(
                formatter,
                "PROCESS_ID_NOT_APPLICATION_IDENTITY: {} is a transient ProcessId, not an ApplicationId/InstallationId ([DUI-LAUNCH-001])",
                hex(offending_id)
            ),
            Self::InstallationNotFound { installation_id } => write!(
                formatter,
                "INSTALLATION_NOT_FOUND: installation {} not found",
                hex(installation_id)
            ),
            Self::ApplicationInstallationMismatch {
                requested_application_id,
                receipt_application_id,
                installation_id,
            } => write!(
                formatter,
                "APPLICATION_INSTALLATION_MISMATCH: installation {} belongs to application {}, not {}",
                hex(installation_id),
                hex(receipt_application_id),
                hex(requested_application_id)
            ),
            Self::ApplicationNotInstalled {
                application_id,
                status,
            } => write!(
                formatter,
                "APPLICATION_NOT_INSTALLED: application {} has status '{status}'",
                hex(application_id)
            ),
            Self::StaleInstallationGeneration {
                application_id,
                installation_id,
                requested_generation,
                current_generation,
            } => write!(
                formatter,
                "STALE_INSTALLATION_GENERATION: installation {} of application {} has generation {requested_generation}, current is {current_generation}",
                hex(installation_id),
                hex(application_id)
            ),
            Self::NoPresentableSurfaces {
                application_id,
                generation,
            } => write!(
                formatter,
                "NO_PRESENTABLE_SURFACES: application {} generation {generation} has no presentable surfaces",
                hex(application_id)
            ),
            Self::SessionOrWorkspaceLocked(detail) => {
                write!(formatter, "SESSION_OR_WORKSPACE_LOCKED: {detail}")
            }
            Self::Storage(detail) => write!(formatter, "LAUNCHER_STORAGE_ERROR: {detail}"),
        }
    }
}

impl std::error::Error for LauncherError {}

impl From<AuthorityLaunchResolutionError> for LauncherError {
    fn from(value: AuthorityLaunchResolutionError) -> Self {
        match value {
            AuthorityLaunchResolutionError::PackageIdNotApplicationIdentity { offending_id } => {
                Self::PackageIdNotApplicationIdentity { offending_id }
            }
            AuthorityLaunchResolutionError::ProcessIdNotApplicationIdentity { offending_id } => {
                Self::ProcessIdNotApplicationIdentity { offending_id }
            }
            AuthorityLaunchResolutionError::InstallationNotFound { installation_id } => {
                Self::InstallationNotFound { installation_id }
            }
            AuthorityLaunchResolutionError::ApplicationInstallationMismatch {
                requested_application_id,
                receipt_application_id,
                installation_id,
            } => Self::ApplicationInstallationMismatch {
                requested_application_id,
                receipt_application_id,
                installation_id,
            },
            AuthorityLaunchResolutionError::ApplicationNotInstalled {
                application_id,
                status,
            } => Self::ApplicationNotInstalled {
                application_id,
                status,
            },
            AuthorityLaunchResolutionError::StaleInstallationGeneration {
                application_id,
                installation_id,
                requested_generation,
                current_generation,
                ..
            } => Self::StaleInstallationGeneration {
                application_id,
                installation_id,
                requested_generation,
                current_generation,
            },
            AuthorityLaunchResolutionError::NoPresentableSurfaces {
                application_id,
                generation,
            } => Self::NoPresentableSurfaces {
                application_id,
                generation,
            },
            AuthorityLaunchResolutionError::Storage(detail) => Self::Storage(detail),
        }
    }
}

fn decode_hex_16_lossless(hex_str: &str) -> Option<[u8; 16]> {
    let trimmed = hex_str.trim();
    if trimmed.len() != 32 {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut out = [0u8; 16];
    let mut idx = 0;
    while idx < 16 {
        let hi = match bytes[idx * 2] {
            b'0'..=b'9' => bytes[idx * 2] - b'0',
            b'a'..=b'f' => bytes[idx * 2] - b'a' + 10,
            b'A'..=b'F' => bytes[idx * 2] - b'A' + 10,
            _ => return None,
        };
        let lo = match bytes[idx * 2 + 1] {
            b'0'..=b'9' => bytes[idx * 2 + 1] - b'0',
            b'a'..=b'f' => bytes[idx * 2 + 1] - b'a' + 10,
            b'A'..=b'F' => bytes[idx * 2 + 1] - b'A' + 10,
            _ => return None,
        };
        out[idx] = (hi << 4) | lo;
        idx += 1;
    }
    Some(out)
}

/// `[DUI-LAUNCH-001]` 应用启动与代际推进协调器。
#[derive(Clone, Debug, Default)]
pub struct ApplicationLauncher {
    catalog: BTreeMap<[u8; 16], InstalledApplicationCatalogEntry>,
    known_package_ids: BTreeSet<[u8; 16]>,
    known_process_ids: BTreeSet<[u8; 16]>,
    launch_sequence: u64,
}

impl ApplicationLauncher {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册或更新同步自 `ListInstalledApplications` 的已安装应用目录项。
    pub fn register_catalog_entry(&mut self, entry: InstalledApplicationCatalogEntry) {
        self.known_package_ids.insert(*entry.package_id.as_bytes());
        for pid in &entry.known_process_ids {
            self.known_process_ids.insert(*pid.as_bytes());
        }
        self.catalog.insert(*entry.application_id.as_bytes(), entry);
    }

    /// 登记已知瞬态 `ProcessId`（用于拦截将 `ProcessId` 伪装为 `ApplicationId` / `InstallationId`）。
    pub fn register_known_process_id(&mut self, process_id: ProcessId) {
        self.known_process_ids.insert(*process_id.as_bytes());
    }

    /// 登记已知 `PackageId`（用于拦截将 `PackageId` 伪装为 `ApplicationId` / `InstallationId`）。
    pub fn register_known_package_id(&mut self, package_id: PackageId) {
        self.known_package_ids.insert(*package_id.as_bytes());
    }

    /// 显式拒绝以 `PackageId` 启动（`[DUI-LAUNCH-001]`）。
    ///
    /// # Errors
    ///
    /// 始终返回 [`LauncherError::PackageIdNotApplicationIdentity`]。
    pub const fn launch_by_package_id_rejected(
        &self,
        package_id: PackageId,
    ) -> Result<Vec<[u8; 16]>, LauncherError> {
        Err(LauncherError::PackageIdNotApplicationIdentity {
            offending_id: *package_id.as_bytes(),
        })
    }

    /// 显式拒绝以瞬态 `ProcessId` 启动（`[DUI-LAUNCH-001]`）。
    ///
    /// # Errors
    ///
    /// 始终返回 [`LauncherError::ProcessIdNotApplicationIdentity`]。
    pub const fn launch_by_process_id_rejected(
        &self,
        process_id: ProcessId,
    ) -> Result<Vec<[u8; 16]>, LauncherError> {
        Err(LauncherError::ProcessIdNotApplicationIdentity {
            offending_id: *process_id.as_bytes(),
        })
    }

    fn check_known_non_app_identities(
        &self,
        candidate_app: [u8; 16],
        candidate_inst: [u8; 16],
    ) -> Result<(), LauncherError> {
        if self.known_package_ids.contains(&candidate_app) {
            return Err(LauncherError::PackageIdNotApplicationIdentity {
                offending_id: candidate_app,
            });
        }
        if self.known_package_ids.contains(&candidate_inst) {
            return Err(LauncherError::PackageIdNotApplicationIdentity {
                offending_id: candidate_inst,
            });
        }
        if self.known_process_ids.contains(&candidate_app) {
            return Err(LauncherError::ProcessIdNotApplicationIdentity {
                offending_id: candidate_app,
            });
        }
        if self.known_process_ids.contains(&candidate_inst) {
            return Err(LauncherError::ProcessIdNotApplicationIdentity {
                offending_id: candidate_inst,
            });
        }
        Ok(())
    }

    /// 按类型化 [`LaunchIdentity`] 启动或激活已登记目录应用。
    ///
    /// # Errors
    ///
    /// 当传入 `PackageId`、`ProcessId`、未知/过期安装或会话锁定时返回 [`LauncherError`]。
    pub fn launch_by_identity(
        &mut self,
        identity: LaunchIdentity,
        session: &DesktopSessionManager,
        compositor: &mut DesktopCompositor,
        route: &mut FocusAndInputRoute,
    ) -> Result<Vec<[u8; 16]>, LauncherError> {
        match identity {
            LaunchIdentity::PackageId(package_id) => self.launch_by_package_id_rejected(package_id),
            LaunchIdentity::ProcessId(process_id) => self.launch_by_process_id_rejected(process_id),
            LaunchIdentity::ApplicationInstallation {
                application_id,
                installation_id,
            } => self.launch_from_catalog(
                application_id,
                installation_id,
                session,
                compositor,
                route,
            ),
        }
    }

    /// 严格按 `(ApplicationId, InstallationId)` 从本地 `ApplicationAuthority` 核验并启动窗口。
    ///
    /// # Errors
    ///
    /// 违反 `[DUI-LAUNCH-001]`、安装过期或会话未激活时返回 [`LauncherError`]。
    pub fn launch_from_authority(
        &mut self,
        authority: &ApplicationAuthority,
        application_id: ApplicationId,
        installation_id: InstallationId,
        session: &DesktopSessionManager,
        compositor: &mut DesktopCompositor,
        route: &mut FocusAndInputRoute,
    ) -> Result<Vec<[u8; 16]>, LauncherError> {
        let app_bytes = *application_id.as_bytes();
        let inst_bytes = *installation_id.as_bytes();
        self.check_known_non_app_identities(app_bytes, inst_bytes)?;

        let resolved = resolve_launch_from_authority(authority, application_id, installation_id)?;
        let mut surfaces = Vec::with_capacity(resolved.presentation.presentable_surfaces.len());
        for dto in &resolved.presentation.presentable_surfaces {
            let surface_id = decode_hex_16_lossless(&dto.surface_id_hex).unwrap_or_else(|| {
                derive_scoped_id_16(
                    b"llmos/desktop/surface/fallback-id/v1",
                    dto.surface_id_hex.as_bytes(),
                    &app_bytes,
                )
            });
            surfaces.push(DeclaredWindowSurfaceSpec {
                surface_id,
                title: dto.title.clone(),
                entry_name: dto.entry_name.clone(),
            });
        }
        let known_pids = resolved
            .known_process_ids
            .iter()
            .copied()
            .map(ProcessId::from_bytes)
            .collect();

        let catalog_entry = InstalledApplicationCatalogEntry {
            application_id: resolved.application_id,
            installation_id: resolved.installation_id,
            package_id: resolved.package_id,
            current_installation_generation: resolved.current_installation_generation,
            surfaces,
            known_process_ids: known_pids,
        };
        self.register_catalog_entry(catalog_entry);
        self.launch_from_catalog(application_id, installation_id, session, compositor, route)
    }

    /// 严格按 `(ApplicationId, InstallationId)` 从已同步目录启动或激活窗口。
    ///
    /// # Errors
    ///
    /// 违反 `[DUI-LAUNCH-001]`、安装不存在、代际不匹配或会话/工作区锁定时返回 [`LauncherError`]。
    pub fn launch_from_catalog(
        &mut self,
        application_id: ApplicationId,
        installation_id: InstallationId,
        session: &DesktopSessionManager,
        compositor: &mut DesktopCompositor,
        route: &mut FocusAndInputRoute,
    ) -> Result<Vec<[u8; 16]>, LauncherError> {
        let app_bytes = *application_id.as_bytes();
        let inst_bytes = *installation_id.as_bytes();
        self.check_known_non_app_identities(app_bytes, inst_bytes)?;

        if session.state() != DesktopSessionState::Active {
            return Err(LauncherError::SessionOrWorkspaceLocked(format!(
                "desktop session is {}",
                session.state().as_str()
            )));
        }
        let workspace = session
            .active_workspace()
            .map_err(|err| LauncherError::SessionOrWorkspaceLocked(err.to_string()))?;
        if !workspace.state.can_admit_work() {
            return Err(LauncherError::SessionOrWorkspaceLocked(format!(
                "workspace {} is {}",
                workspace.workspace_id_hex(),
                workspace.state.as_str()
            )));
        }
        let workspace_id = workspace.workspace_id;
        let session_gen = session.session_generation();

        // 若通过已知 installation_id 能找到另一个应用条目，说明 ApplicationId 与 InstallationId 错配。
        if !self.catalog.contains_key(&app_bytes) {
            for entry in self.catalog.values() {
                if entry.installation_id == installation_id {
                    return Err(LauncherError::ApplicationInstallationMismatch {
                        requested_application_id: app_bytes,
                        receipt_application_id: *entry.application_id.as_bytes(),
                        installation_id: inst_bytes,
                    });
                }
            }
            return Err(LauncherError::InstallationNotFound {
                installation_id: inst_bytes,
            });
        }

        let entry =
            self.catalog
                .get(&app_bytes)
                .cloned()
                .ok_or(LauncherError::InstallationNotFound {
                    installation_id: inst_bytes,
                })?;

        if entry.installation_id != installation_id {
            return Err(LauncherError::InstallationNotFound {
                installation_id: inst_bytes,
            });
        }
        if entry.surfaces.is_empty() {
            return Err(LauncherError::NoPresentableSurfaces {
                application_id: app_bytes,
                generation: entry.current_installation_generation,
            });
        }

        let mut activated_windows = Vec::with_capacity(entry.surfaces.len());
        for spec in &entry.surfaces {
            if let Some(existing_idx) = compositor.windows.iter().position(|win| {
                win.workspace_id == workspace_id
                    && win.owner_application_id == app_bytes
                    && win.installation_id == inst_bytes
                    && win.surface_id == spec.surface_id
                    && !win.stale_generation
                    && win.lifecycle.window != WindowLifecycle::Closed
                    && win.lifecycle.window != WindowLifecycle::Closing
            }) {
                let shown = compositor.windows[existing_idx]
                    .lifecycle
                    .show()
                    .map_err(|err| LauncherError::Storage(err.to_string()))?;
                compositor.windows[existing_idx].lifecycle = shown;
                compositor.windows[existing_idx].session_generation = session_gen;
                let win_id = compositor.windows[existing_idx].window_id;
                compositor.promote_mru(win_id);
                activated_windows.push(win_id);
                continue;
            }

            self.launch_sequence = self.launch_sequence.saturating_add(1);
            let mut seed = [0u8; 24];
            seed[..16].copy_from_slice(&workspace_id);
            seed[16..].copy_from_slice(&self.launch_sequence.to_be_bytes());
            let window_id =
                derive_scoped_id_16(b"llmos/desktop/compositor/window-id/v1", &inst_bytes, &seed);
            let created = CoupledWindowSurface::new_created();
            let shown = created
                .show()
                .map_err(|err| LauncherError::Storage(err.to_string()))?;
            let seq_offset = ((self.launch_sequence % 6) as i32) * 24;
            let floating_desired = RectPx::new(72 + seq_offset, 54 + seq_offset, 760, 520);
            let a11y = WindowAccessibilityNode::new(
                window_id,
                &spec.title,
                app_bytes,
                entry.current_installation_generation,
                false,
                false,
            );
            compositor.windows.push(CompositorWindow {
                window_id,
                surface_id: spec.surface_id,
                title: spec.title.clone(),
                entry_name: spec.entry_name.clone(),
                owner_application_id: app_bytes,
                installation_id: inst_bytes,
                package_id: *entry.package_id.as_bytes(),
                workspace_id,
                session_generation: session_gen,
                surface_generation: entry.current_installation_generation,
                current_installation_generation: entry.current_installation_generation,
                stale_generation: false,
                lifecycle: shown,
                layout_slot: "pending".to_owned(),
                is_floating: compositor.layout_mode == WorkstationLayoutMode::Floating,
                floating_desired_rect: floating_desired,
                z_index: Z2_WINDOW_Z_MIN,
                border_radius_px: TILED_BORDER_RADIUS_PX,
                outer_frame_rect: RectPx::default(),
                inner_content_clip_rect: RectPx::default(),
                occluded_by_system_plane: false,
                a11y,
            });
            compositor.promote_mru(window_id);
            activated_windows.push(window_id);
        }

        compositor.recompute_workspace_geometry(workspace_id, route.z4_modal_lock);

        if let Some(&last_win_id) = activated_windows.last()
            && !route.z4_modal_lock
            && let Some(win) = compositor.window(last_win_id)
        {
            route.set_focus(win.window_id, win.surface_id, win.surface_generation);
        }

        Ok(activated_windows)
    }

    /// 推进应用当前安装代际（`[DUI-WINDOW-001]` / `[DUI-LAUNCH-001]`）：
    /// 将所有 `surface_generation < new_generation` 的已打开窗口标记为 `stale_generation = true`，
    /// 立即剥夺其焦点并锁定后续输入投递，直至显式重载或关闭。
    pub fn advance_application_generation(
        &mut self,
        application_id: ApplicationId,
        new_generation: u64,
        new_installation_id: Option<InstallationId>,
        compositor: &mut DesktopCompositor,
        route: &mut FocusAndInputRoute,
    ) -> usize {
        let app_bytes = *application_id.as_bytes();
        if let Some(entry) = self.catalog.get_mut(&app_bytes) {
            if new_generation > entry.current_installation_generation {
                entry.current_installation_generation = new_generation;
            }
            if let Some(inst_id) = new_installation_id {
                entry.installation_id = inst_id;
            }
        }

        let mut marked_stale = 0_usize;
        for win in &mut compositor.windows {
            if win.owner_application_id == app_bytes
                && win.lifecycle.window != WindowLifecycle::Closed
            {
                if new_generation > win.current_installation_generation {
                    win.current_installation_generation = new_generation;
                }
                if win.surface_generation < win.current_installation_generation {
                    win.stale_generation = true;
                    win.a11y.stale_readonly = true;
                    marked_stale = marked_stale.saturating_add(1);
                    if route.focused_window_id == Some(win.window_id) {
                        route.clear_focus();
                    }
                }
            }
        }
        marked_stale
    }

    /// 将过期代际窗口显式重载至最新安装代际并恢复焦点资格。
    ///
    /// # Errors
    ///
    /// 窗口不存在或已关闭时返回 [`CompositorError`]。
    pub fn reload_stale_window(
        &mut self,
        window_id: [u8; 16],
        new_installation_id: Option<InstallationId>,
        compositor: &mut DesktopCompositor,
        route: &mut FocusAndInputRoute,
    ) -> Result<(), CompositorError> {
        let idx = compositor
            .windows
            .iter()
            .position(|win| win.window_id == window_id)
            .ok_or(CompositorError::WindowNotFound { window_id })?;

        let app_bytes = compositor.windows[idx].owner_application_id;
        let resolved_inst = new_installation_id
            .map(|id| *id.as_bytes())
            .or_else(|| {
                self.catalog
                    .get(&app_bytes)
                    .map(|entry| *entry.installation_id.as_bytes())
            })
            .unwrap_or(compositor.windows[idx].installation_id);

        let target_gen = self.catalog.get(&app_bytes).map_or(
            compositor.windows[idx].current_installation_generation,
            |e| {
                e.current_installation_generation
                    .max(compositor.windows[idx].current_installation_generation)
            },
        );

        let shown = compositor.windows[idx].lifecycle.show()?;
        let workspace_id = compositor.windows[idx].workspace_id;
        let surface_id = compositor.windows[idx].surface_id;
        compositor.windows[idx].lifecycle = shown;
        compositor.windows[idx].installation_id = resolved_inst;
        compositor.windows[idx].current_installation_generation = target_gen;
        compositor.windows[idx].surface_generation = target_gen;
        compositor.windows[idx].stale_generation = false;
        compositor.windows[idx].a11y.stale_readonly = false;
        compositor.windows[idx].a11y.installation_generation = target_gen;
        compositor.promote_mru(window_id);
        compositor.recompute_workspace_geometry(workspace_id, route.z4_modal_lock);
        if !route.z4_modal_lock {
            route.set_focus(window_id, surface_id, target_gen);
        }
        Ok(())
    }
}

/// 阶段 D 统一桌面工作站壳聚合体（`Session` + `Compositor` + `ApplicationLauncher` + `FocusAndInputRoute`）。
#[derive(Debug)]
pub struct WorkstationHostShell {
    pub session: DesktopSessionManager,
    pub compositor: DesktopCompositor,
    pub launcher: ApplicationLauncher,
    pub route: FocusAndInputRoute,
}

impl Default for WorkstationHostShell {
    fn default() -> Self {
        Self::new_default_workstation()
    }
}

impl WorkstationHostShell {
    #[must_use]
    pub fn new(session: DesktopSessionManager, compositor: DesktopCompositor) -> Self {
        let initial_gen = session.session_generation();
        Self {
            session,
            compositor,
            launcher: ApplicationLauncher::new(),
            route: FocusAndInputRoute::new(initial_gen),
        }
    }

    #[must_use]
    pub fn new_default_workstation() -> Self {
        let session = DesktopSessionManager::default_workstation();
        let compositor = DesktopCompositor::default();
        Self::new(session, compositor)
    }

    /// 执行会话状态转换并自动同步 `FocusAndInputRoute` 的 `session_generation` 与工作区几何。
    ///
    /// # Errors
    ///
    /// 非法会话转换或未知用户返回 [`CompositorError`]。
    pub fn transition_session(
        &mut self,
        transition: &str,
        target_principal_id: Option<[u8; 16]>,
        _now_mono_ms: Option<u64>,
    ) -> Result<u64, CompositorError> {
        let next_gen = match transition.trim().to_ascii_lowercase().as_str() {
            "lock" => self.session.lock()?,
            "unlock" => self.session.unlock()?,
            "suspend" => self.session.suspend()?,
            "resume" => self.session.resume()?,
            "switch_user" | "switch-user" => {
                let principal = target_principal_id.ok_or(CompositorError::Session(
                    SessionError::UserProfileNotFound {
                        principal_id: [0u8; 16],
                    },
                ))?;
                self.session.switch_user(principal)?
            }
            "close" => self.session.close()?,
            other => {
                return Err(CompositorError::InvalidWindowAction(format!(
                    "unsupported session transition: {other}"
                )));
            }
        };
        self.route.sync_session_generation(next_gen);
        if self.session.state() != DesktopSessionState::Active {
            self.route.clear_focus();
        } else if let Ok(ws) = self.session.active_workspace() {
            let ws_id = ws.workspace_id;
            if let Ok(mode) = WorkstationLayoutMode::parse(&ws.layout_mode) {
                self.compositor.layout_mode = mode;
            }
            self.compositor
                .recompute_workspace_geometry(ws_id, self.route.z4_modal_lock);
        }
        Ok(next_gen)
    }

    /// 执行工作区容器生命周期动作（创建、激活、归档、关闭）。
    ///
    /// # Errors
    ///
    /// 跨用户访问或状态机冲突返回 [`CompositorError`]。
    pub fn workspace_action(
        &mut self,
        action: &str,
        workspace_id: [u8; 16],
    ) -> Result<(), CompositorError> {
        match action.trim().to_ascii_lowercase().as_str() {
            "create" => {
                let label = format!("工作区 {}", &hex(&workspace_id)[..8]);
                self.session.create_workspace(workspace_id, label, false)?;
            }
            "create_and_activate" | "create-and-activate" => {
                let label = format!("工作区 {}", &hex(&workspace_id)[..8]);
                self.session.create_workspace(workspace_id, label, true)?;
                self.route.clear_focus();
                self.compositor
                    .recompute_workspace_geometry(workspace_id, self.route.z4_modal_lock);
            }
            "activate" | "switch" => {
                let ws = self.session.activate_workspace(workspace_id)?;
                let mode = WorkstationLayoutMode::parse(&ws.layout_mode)
                    .unwrap_or(WorkstationLayoutMode::ThreePaneWorkstation);
                self.compositor.layout_mode = mode;
                if let Some(focused_id) = self.route.focused_window_id
                    && self
                        .compositor
                        .window(focused_id)
                        .is_none_or(|win| win.workspace_id != workspace_id)
                {
                    self.route.clear_focus();
                }
                self.compositor
                    .recompute_workspace_geometry(workspace_id, self.route.z4_modal_lock);
            }
            "archive" => {
                self.session.archive_workspace(workspace_id)?;
                if let Some(focused_id) = self.route.focused_window_id
                    && self
                        .compositor
                        .window(focused_id)
                        .is_some_and(|win| win.workspace_id == workspace_id)
                {
                    self.route.clear_focus();
                }
            }
            "close" => {
                self.session.close_workspace(workspace_id)?;
                for win in &mut self.compositor.windows {
                    if win.workspace_id == workspace_id
                        && win.lifecycle.window != WindowLifecycle::Closed
                        && let Ok(closed) = win.lifecycle.close_immediately()
                    {
                        win.lifecycle = closed;
                        win.a11y.hidden = true;
                    }
                }
                if let Some(focused_id) = self.route.focused_window_id
                    && self
                        .compositor
                        .window(focused_id)
                        .is_some_and(|win| win.workspace_id == workspace_id)
                {
                    self.route.clear_focus();
                }
            }
            other => {
                return Err(CompositorError::InvalidWindowAction(format!(
                    "unsupported workspace action: {other}"
                )));
            }
        }
        Ok(())
    }

    /// 更新当前工作区布局与视口参数。
    ///
    /// # Errors
    ///
    /// 非法布局名称或无有效活动工作区时返回 [`CompositorError`]。
    pub fn set_layout(
        &mut self,
        layout_mode_raw: &str,
        viewport_width: Option<u32>,
        viewport_height: Option<u32>,
        top_height_ratio_permille: Option<u16>,
        split_width_ratio_permille: Option<u16>,
        z1_visible: Option<bool>,
    ) -> Result<(), CompositorError> {
        let mode = WorkstationLayoutMode::parse(layout_mode_raw)?;
        let active_ws_id = self.session.active_workspace()?.workspace_id;
        self.session
            .sync_active_workspace_layout_mode(mode.as_str());
        self.compositor.set_layout_config(
            mode,
            viewport_width,
            viewport_height,
            top_height_ratio_permille,
            split_width_ratio_permille,
            z1_visible,
            active_ws_id,
            self.route.z4_modal_lock,
        );
        Ok(())
    }

    /// 从真实 `ApplicationAuthority` 同步所有已打开窗口的最新安装代际。
    ///
    /// 若发现后台已执行升级（`current_gen > window.current_installation_generation`）或停用/卸载，
    /// 立即将该应用旧窗口标记为 `stale_generation = true` 并剥夺焦点。
    ///
    /// # Errors
    ///
    /// 底层存储读取失败时返回错误描述。
    pub fn sync_generations_from_authority(
        &mut self,
        authority: &ApplicationAuthority,
    ) -> Result<usize, String> {
        let mut unique_packages = BTreeSet::new();
        for win in &self.compositor.windows {
            if win.lifecycle.window != WindowLifecycle::Closed {
                unique_packages.insert(win.package_id);
            }
        }
        let mut total_marked = 0_usize;
        for pkg_bytes in unique_packages {
            let pkg_id = PackageId::from_bytes(pkg_bytes);
            if let Some((app_id, current_gen, status)) =
                inspect_current_generation_by_package(authority, pkg_id)
                    .map_err(|err| err.message)?
            {
                if status != nlos_application::ApplicationStatus::Installed {
                    // 已禁用或已卸载应用的旧表面同样必须进入 stale 围栏，禁止继续接收输入。
                    let bumped = current_gen.saturating_add(1);
                    total_marked =
                        total_marked.saturating_add(self.launcher.advance_application_generation(
                            app_id,
                            bumped,
                            None,
                            &mut self.compositor,
                            &mut self.route,
                        ));
                } else {
                    total_marked =
                        total_marked.saturating_add(self.launcher.advance_application_generation(
                            app_id,
                            current_gen,
                            None,
                            &mut self.compositor,
                            &mut self.route,
                        ));
                }
            }
        }
        Ok(total_marked)
    }

    /// 执行窗口动作（`focus` / `show` / `minimize` / `begin_close` / `close` / `float` / `tile` / `reload`）。
    ///
    /// # Errors
    ///
    /// 窗口不存在、已关闭、过期未重载或动作未知时返回 [`CompositorError`]。
    #[allow(clippy::too_many_lines)]
    pub fn window_action(
        &mut self,
        window_id: [u8; 16],
        action: &str,
        rect: Option<RectPx>,
        new_installation_id: Option<InstallationId>,
        authority: Option<&ApplicationAuthority>,
    ) -> Result<(), CompositorError> {
        let active_ws_id = self.session.active_workspace()?.workspace_id;
        let idx = self
            .compositor
            .windows
            .iter()
            .position(|win| win.window_id == window_id && win.workspace_id == active_ws_id)
            .ok_or(CompositorError::WindowNotFound { window_id })?;

        match action.trim().to_ascii_lowercase().as_str() {
            "focus" | "activate" | "show" | "restore" | "present" => {
                let win = &self.compositor.windows[idx];
                if win.stale_generation
                    || win.surface_generation < win.current_installation_generation
                {
                    return Err(CompositorError::StaleWindowRequiresReloadOrClose {
                        window_id,
                        surface_generation: win.surface_generation,
                        current_installation_generation: win.current_installation_generation,
                    });
                }
                let shown = win.lifecycle.show()?;
                let surface_id = win.surface_id;
                let surface_gen = win.surface_generation;
                self.compositor.windows[idx].lifecycle = shown;
                self.compositor.promote_mru(window_id);
                self.compositor
                    .recompute_workspace_geometry(active_ws_id, self.route.z4_modal_lock);
                if !self.route.z4_modal_lock {
                    self.route.set_focus(window_id, surface_id, surface_gen);
                }
            }
            "minimize" | "hide" => {
                let minimized = self.compositor.windows[idx].lifecycle.minimize()?;
                self.compositor.windows[idx].lifecycle = minimized;
                if self.route.focused_window_id == Some(window_id) {
                    self.route.clear_focus();
                }
                self.compositor
                    .recompute_workspace_geometry(active_ws_id, self.route.z4_modal_lock);
            }
            "begin_close" | "begin-close" => {
                let closing = self.compositor.windows[idx].lifecycle.begin_close()?;
                self.compositor.windows[idx].lifecycle = closing;
                if self.route.focused_window_id == Some(window_id) {
                    self.route.clear_focus();
                }
                self.compositor
                    .recompute_workspace_geometry(active_ws_id, self.route.z4_modal_lock);
            }
            "close" | "finish_close" | "finish-close" => {
                let current_lc = self.compositor.windows[idx].lifecycle;
                let closed = if current_lc.window == WindowLifecycle::Closing {
                    current_lc.finish_close()?
                } else {
                    current_lc.close_immediately()?
                };
                self.compositor.windows[idx].lifecycle = closed;
                if self.route.focused_window_id == Some(window_id) {
                    self.route.clear_focus();
                }
                self.compositor
                    .recompute_workspace_geometry(active_ws_id, self.route.z4_modal_lock);
            }
            "float" | "set_floating" | "move_floating" | "move-floating" | "move_resize"
            | "move-resize" => {
                if let Some(desired) = rect {
                    self.compositor.windows[idx].floating_desired_rect = desired;
                }
                self.compositor.windows[idx].is_floating = true;
                self.compositor.promote_mru(window_id);
                self.compositor
                    .recompute_workspace_geometry(active_ws_id, self.route.z4_modal_lock);
            }
            "toggle_floating" | "toggle-floating" => {
                if let Some(desired) = rect {
                    self.compositor.windows[idx].floating_desired_rect = desired;
                }
                let next_floating = !self.compositor.windows[idx].is_floating;
                self.compositor.windows[idx].is_floating = next_floating;
                if next_floating {
                    self.compositor.promote_mru(window_id);
                }
                self.compositor
                    .recompute_workspace_geometry(active_ws_id, self.route.z4_modal_lock);
            }
            "tile" | "dock" | "unfloat" => {
                self.compositor.windows[idx].is_floating = false;
                self.compositor
                    .recompute_workspace_geometry(active_ws_id, self.route.z4_modal_lock);
            }
            "reload" | "reload_stale" | "reload-stale" => {
                if let Some(auth) = authority
                    && let Some(inst_id) = new_installation_id
                {
                    let app_id = ApplicationId::from_bytes(
                        self.compositor.windows[idx].owner_application_id,
                    );
                    let resolved = resolve_launch_from_authority(auth, app_id, inst_id)
                        .map_err(LauncherError::from)?;
                    let mut surfaces =
                        Vec::with_capacity(resolved.presentation.presentable_surfaces.len());
                    for dto in &resolved.presentation.presentable_surfaces {
                        let surface_id = decode_hex_16_lossless(&dto.surface_id_hex)
                            .unwrap_or(self.compositor.windows[idx].surface_id);
                        surfaces.push(DeclaredWindowSurfaceSpec {
                            surface_id,
                            title: dto.title.clone(),
                            entry_name: dto.entry_name.clone(),
                        });
                    }
                    self.launcher
                        .register_catalog_entry(InstalledApplicationCatalogEntry {
                            application_id: resolved.application_id,
                            installation_id: resolved.installation_id,
                            package_id: resolved.package_id,
                            current_installation_generation: resolved
                                .current_installation_generation,
                            surfaces,
                            known_process_ids: resolved
                                .known_process_ids
                                .iter()
                                .copied()
                                .map(ProcessId::from_bytes)
                                .collect(),
                        });
                }
                self.launcher.reload_stale_window(
                    window_id,
                    new_installation_id,
                    &mut self.compositor,
                    &mut self.route,
                )?;
            }
            other => return Err(CompositorError::InvalidWindowAction(other.to_owned())),
        }
        Ok(())
    }

    /// 切换 `Z3` 系统浮层或全局命令面板，并更新子表面系统平面遮挡态。
    ///
    /// # Errors
    ///
    /// 未知浮层类别或无活动工作区时返回 [`CompositorError`]。
    pub fn set_z3_overlay(
        &mut self,
        overlay_id: impl Into<String>,
        kind_raw: &str,
        active: bool,
        rect: Option<RectPx>,
    ) -> Result<(), CompositorError> {
        let kind = Z3OverlayKind::parse(kind_raw)?;
        let active_ws_id = self
            .session
            .active_workspace()
            .map_or([0u8; 16], |ws| ws.workspace_id);
        self.compositor.set_z3_overlay(
            overlay_id,
            kind,
            active,
            rect,
            active_ws_id,
            self.route.z4_modal_lock,
        );
        Ok(())
    }

    /// 切换 `Z4` 不可遮挡安全确认模态锁，记录 `350ms` 武装时间戳并立即重算 `Z2` 遮挡标记。
    pub fn set_z4_modal(&mut self, active: bool, now_mono_ms: u64) {
        self.route.set_z4_modal_lock(active, now_mono_ms);
        if let Ok(ws) = self.session.active_workspace() {
            let ws_id = ws.workspace_id;
            self.compositor
                .recompute_workspace_geometry(ws_id, self.route.z4_modal_lock);
        }
    }

    /// 对当前会话活动工作区执行确定性输入防伪与代际路由裁决。
    ///
    /// # Errors
    ///
    /// 被任一防伪/会话/Z4/表面代际规则拒绝时返回 [`InputRejection`]。
    pub fn dispatch_input(
        &mut self,
        event: &InputEventEnvelope,
    ) -> Result<RoutedInputOutcome, InputRejection> {
        let session_state = self.session.state();
        let active_ws_id = self
            .session
            .active_workspace()
            .ok()
            .map(|ws| ws.workspace_id);
        let compositor_ref = &self.compositor;
        self.route.route_input(event, session_state, |window_id| {
            let ws_id = active_ws_id?;
            compositor_ref
                .window(window_id)
                .filter(|win| win.workspace_id == ws_id)
                .map(CompositorWindow::input_gate_view)
        })
    }
}
