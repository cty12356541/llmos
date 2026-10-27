//! Stage D (`[DUI-INPUT-001]`, `[DUI-WINDOW-001]`, `[DUI-SESSION-001]`) 焦点管理与防伪输入路由器。
//!
//! 权威契约对齐 `06-架构设计总纲-v0.5.md` §25.4 与 `ADR-0025` 决定 1：
//! 1. **来源标签与系统级防伪**：不可信 `Application` `Surface` 严禁注入 `Z3` 系统快捷键、
//!    `Z4` 安全授权确认/会话解锁或跨表面伪造事件（`UntrustedSystemSpoof`）；
//! 2. **会话代际围栏**：锁屏、挂起、切换用户或关闭会话推进 `session_generation` 后，一切携带旧代际
//!    或在非 `ACTIVE` 会话到达的事件（除 `LOCKED` 下可信 `Z4SessionUnlock` / `Escape` 取消外）一律拒绝
//!    （`LockedOrStaleSession`）；
//! 3. **`Z4` 模态切断与 `350ms` 防劫持武装门**：`z4_modal_lock == true` 时物理切断发往 `Z2` 的全部
//!    五类硬件/自动化输入（`Keyboard` / `Pointer` / `Voice` / `DragDrop` / `Automation`），并在挂载后
//!    前 `350ms` 内拒绝确认提交（`Z4ArmingWindowActive`），同时保持 `0ms` 即时 `Escape` 拒绝通道畅通；
//! 4. **表面安装代际围栏**：应用升级推进 `current_installation_generation` 或窗口隐藏/最小化/关闭中时，
//!    立即拒绝旧代际或非 `PRESENTED` 表面的一切输入（`StaleOrUnpresentedSurface`）。

use std::fmt;

use crate::dto::hex;
use crate::session::DesktopSessionState;
use crate::window_lifecycle::{CoupledWindowSurface, SurfaceLifecycle, WindowLifecycle};

/// `Z4` 系统安全确认框挂载后的强制防点击劫持武装时间窗（毫秒）。
pub const Z4_ARMING_WINDOW_MS: u64 = 350;

/// 五类受管输入通道（`[DUI-INPUT-001]`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InputSource {
    Keyboard,
    Pointer,
    Voice,
    DragDrop,
    Automation,
}

impl InputSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keyboard => "KEYBOARD",
            Self::Pointer => "POINTER",
            Self::Voice => "VOICE",
            Self::DragDrop => "DRAG_DROP",
            Self::Automation => "AUTOMATION",
        }
    }

    /// 解析输入通道名称。
    ///
    /// # Errors
    ///
    /// 未知通道名返回错误文本。
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "keyboard" => Ok(Self::Keyboard),
            "pointer" | "mouse" | "touch" => Ok(Self::Pointer),
            "voice" => Ok(Self::Voice),
            "drag_drop" | "drag-drop" | "dragdrop" => Ok(Self::DragDrop),
            "automation" => Ok(Self::Automation),
            other => Err(format!("unknown input source: {other}")),
        }
    }
}

/// 输入事件的信任域起点（宿主特权壳 vs 不可信应用表面）。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputOrigin {
    TrustedShell,
    UntrustedApplicationSurface {
        surface_id: [u8; 16],
        application_id: [u8; 16],
    },
}

/// 输入事件请求投递的目标平面与实体。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputTarget {
    Z2Surface {
        window_id: [u8; 16],
        surface_id: [u8; 16],
        target_surface_generation: u64,
    },
    Z3SystemShortcut {
        shortcut: String,
    },
    Z4SecurityAction {
        confirm: bool,
        key: Option<String>,
    },
    Z4SessionUnlock,
}

/// 准备经过 `FocusAndInputRoute` 裁决的一帧输入事件信封。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputEventEnvelope {
    pub source: InputSource,
    pub origin: InputOrigin,
    pub target: InputTarget,
    pub event_session_generation: u64,
    pub now_mono_ms: u64,
}

/// 输入路由器可观测防伪审计计数器。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputAuditCounters {
    pub accepted_events: u64,
    pub rejected_stale_surface_events: u64,
    pub rejected_locked_session_events: u64,
    pub rejected_z4_severed_events: u64,
    pub rejected_z4_arming_guard_events: u64,
    pub rejected_untrusted_spoof_events: u64,
}

/// 输入被拒绝的稳定类型化原因码。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InputRejectionReason {
    UntrustedSystemSpoof,
    LockedOrStaleSession,
    Z4ModalSevered,
    Z4ArmingWindowActive,
    StaleOrUnpresentedSurface,
}

impl InputRejectionReason {
    #[must_use]
    pub const fn as_code_str(self) -> &'static str {
        match self {
            Self::UntrustedSystemSpoof => "UNTRUSTED_SYSTEM_SPOOF",
            Self::LockedOrStaleSession => "LOCKED_OR_STALE_SESSION",
            Self::Z4ModalSevered => "Z4_MODAL_SEVERED",
            Self::Z4ArmingWindowActive => "Z4_ARMING_WINDOW_ACTIVE",
            Self::StaleOrUnpresentedSurface => "STALE_OR_UNPRESENTED_SURFACE",
        }
    }
}

/// 类型化输入拒绝回执。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputRejection {
    pub reason: InputRejectionReason,
    pub message: String,
}

impl fmt::Display for InputRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.reason.as_code_str(), self.message)
    }
}

impl std::error::Error for InputRejection {}

/// 输入通过围栏后的投递结论。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoutedInputOutcome {
    pub routed_plane: &'static str,
    pub target_window_id: Option<[u8; 16]>,
    pub target_surface_id: Option<[u8; 16]>,
}

/// 供输入路由器执行只读代际与可见性校验的窗口门禁投影。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowInputGateView {
    pub window_id: [u8; 16],
    pub surface_id: [u8; 16],
    pub owner_application_id: [u8; 16],
    pub surface_generation: u64,
    pub current_installation_generation: u64,
    pub stale_generation: bool,
    pub lifecycle: CoupledWindowSurface,
}

fn is_escape_key(key: Option<&str>) -> bool {
    key.is_some_and(|raw| {
        let trimmed = raw.trim();
        trimmed.eq_ignore_ascii_case("escape") || trimmed.eq_ignore_ascii_case("esc")
    })
}

/// 宿主权威焦点与输入路由器（`FocusAndInputRoute`，`[DUI-INPUT-001]`）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FocusAndInputRoute {
    pub focused_window_id: Option<[u8; 16]>,
    pub focused_surface_id: Option<[u8; 16]>,
    pub focused_surface_generation: Option<u64>,
    pub session_generation: u64,
    pub z4_modal_lock: bool,
    pub z4_armed_at_mono_ms: Option<u64>,
    pub counters: InputAuditCounters,
}

impl FocusAndInputRoute {
    #[must_use]
    pub fn new(initial_session_generation: u64) -> Self {
        Self {
            focused_window_id: None,
            focused_surface_id: None,
            focused_surface_generation: None,
            session_generation: initial_session_generation.max(1),
            z4_modal_lock: false,
            z4_armed_at_mono_ms: None,
            counters: InputAuditCounters::default(),
        }
    }

    /// 同步最新会话代际；一旦会话代际推进，立即清空当前应用表面焦点。
    pub fn sync_session_generation(&mut self, next_session_generation: u64) {
        if self.session_generation != next_session_generation {
            self.session_generation = next_session_generation;
            self.clear_focus();
        }
    }

    pub fn set_focus(
        &mut self,
        window_id: [u8; 16],
        surface_id: [u8; 16],
        surface_generation: u64,
    ) {
        self.focused_window_id = Some(window_id);
        self.focused_surface_id = Some(surface_id);
        self.focused_surface_generation = Some(surface_generation);
    }

    pub fn clear_focus(&mut self) {
        self.focused_window_id = None;
        self.focused_surface_id = None;
        self.focused_surface_generation = None;
    }

    /// 开启或关闭 `Z4` 不可遮挡安全确认模态锁，并记录 `350ms` 武装起点。
    pub fn set_z4_modal_lock(&mut self, active: bool, now_mono_ms: u64) {
        self.z4_modal_lock = active;
        self.z4_armed_at_mono_ms = active.then_some(now_mono_ms);
    }

    /// 对一帧输入执行确定性四级防伪与代际围栏校验。
    ///
    /// # Errors
    ///
    /// 被任一防伪或代际规则拦截时递增对应审计计数并返回 [`InputRejection`]。
    #[allow(clippy::too_many_lines)]
    pub fn route_input<F>(
        &mut self,
        event: &InputEventEnvelope,
        session_state: DesktopSessionState,
        lookup_window: F,
    ) -> Result<RoutedInputOutcome, InputRejection>
    where
        F: Fn([u8; 16]) -> Option<WindowInputGateView>,
    {
        // 规则 1：不可信应用表面严禁伪造 Z3/Z4 系统交互或跨表面/跨应用注入。
        if let InputOrigin::UntrustedApplicationSurface {
            surface_id: origin_surface_id,
            application_id: origin_app_id,
        } = event.origin
        {
            match &event.target {
                InputTarget::Z3SystemShortcut { .. }
                | InputTarget::Z4SecurityAction { .. }
                | InputTarget::Z4SessionUnlock => {
                    self.counters.rejected_untrusted_spoof_events = self
                        .counters
                        .rejected_untrusted_spoof_events
                        .saturating_add(1);
                    return Err(InputRejection {
                        reason: InputRejectionReason::UntrustedSystemSpoof,
                        message: format!(
                            "untrusted surface {} (app {}) attempted to spoof system plane input",
                            hex(&origin_surface_id),
                            hex(&origin_app_id)
                        ),
                    });
                }
                InputTarget::Z2Surface {
                    window_id,
                    surface_id: target_surface_id,
                    ..
                } => {
                    if *target_surface_id != origin_surface_id {
                        self.counters.rejected_untrusted_spoof_events = self
                            .counters
                            .rejected_untrusted_spoof_events
                            .saturating_add(1);
                        return Err(InputRejection {
                            reason: InputRejectionReason::UntrustedSystemSpoof,
                            message: format!(
                                "untrusted surface {} attempted cross-surface injection into {}",
                                hex(&origin_surface_id),
                                hex(target_surface_id)
                            ),
                        });
                    }
                    if let Some(win) = lookup_window(*window_id)
                        && (win.owner_application_id != origin_app_id
                            || win.surface_id != origin_surface_id)
                    {
                        self.counters.rejected_untrusted_spoof_events = self
                            .counters
                            .rejected_untrusted_spoof_events
                            .saturating_add(1);
                        return Err(InputRejection {
                            reason: InputRejectionReason::UntrustedSystemSpoof,
                            message: format!(
                                "untrusted app {} attempted cross-window injection into window {}",
                                hex(&origin_app_id),
                                hex(window_id)
                            ),
                        });
                    }
                }
            }
        }

        // 规则 2：会话代际与会话活跃态围栏。
        if event.event_session_generation != self.session_generation {
            self.counters.rejected_locked_session_events = self
                .counters
                .rejected_locked_session_events
                .saturating_add(1);
            return Err(InputRejection {
                reason: InputRejectionReason::LockedOrStaleSession,
                message: format!(
                    "stale event session generation {} != current session generation {}",
                    event.event_session_generation, self.session_generation
                ),
            });
        }

        if session_state != DesktopSessionState::Active {
            let trusted_locked_escape_or_unlock = session_state == DesktopSessionState::Locked
                && matches!(event.origin, InputOrigin::TrustedShell)
                && match &event.target {
                    InputTarget::Z4SessionUnlock => true,
                    InputTarget::Z4SecurityAction { confirm, key } => {
                        !*confirm || is_escape_key(key.as_deref())
                    }
                    _ => false,
                };
            if !trusted_locked_escape_or_unlock {
                self.counters.rejected_locked_session_events = self
                    .counters
                    .rejected_locked_session_events
                    .saturating_add(1);
                return Err(InputRejection {
                    reason: InputRejectionReason::LockedOrStaleSession,
                    message: format!(
                        "desktop session is {} and rejects {} input",
                        session_state.as_str(),
                        event.source.as_str()
                    ),
                });
            }
        }

        // 规则 3：Z4 安全模态切断与 350ms 防点击劫持武装时间窗。
        if self.z4_modal_lock {
            match &event.target {
                InputTarget::Z2Surface { window_id, .. } => {
                    self.counters.rejected_z4_severed_events =
                        self.counters.rejected_z4_severed_events.saturating_add(1);
                    return Err(InputRejection {
                        reason: InputRejectionReason::Z4ModalSevered,
                        message: format!(
                            "Z4 modal lock severed {} input targeting Z2 window {}",
                            event.source.as_str(),
                            hex(window_id)
                        ),
                    });
                }
                InputTarget::Z3SystemShortcut { shortcut }
                    if !is_escape_key(Some(shortcut.as_str())) =>
                {
                    self.counters.rejected_z4_severed_events =
                        self.counters.rejected_z4_severed_events.saturating_add(1);
                    return Err(InputRejection {
                        reason: InputRejectionReason::Z4ModalSevered,
                        message: format!(
                            "Z4 modal lock blocked non-Escape Z3 shortcut '{shortcut}'"
                        ),
                    });
                }
                _ => {}
            }
        }

        match &event.target {
            InputTarget::Z4SecurityAction { confirm, key } => {
                let is_cancel = !*confirm || is_escape_key(key.as_deref());
                if is_cancel {
                    self.counters.accepted_events = self.counters.accepted_events.saturating_add(1);
                    return Ok(RoutedInputOutcome {
                        routed_plane: "Z4",
                        target_window_id: None,
                        target_surface_id: None,
                    });
                }
                let Some(armed_at) = self.z4_armed_at_mono_ms.filter(|_| self.z4_modal_lock) else {
                    self.counters.rejected_z4_arming_guard_events = self
                        .counters
                        .rejected_z4_arming_guard_events
                        .saturating_add(1);
                    return Err(InputRejection {
                        reason: InputRejectionReason::Z4ArmingWindowActive,
                        message: "Z4 security confirm rejected because Z4 modal is not armed"
                            .to_owned(),
                    });
                };
                if event.now_mono_ms < armed_at
                    || event.now_mono_ms.saturating_sub(armed_at) < Z4_ARMING_WINDOW_MS
                {
                    let elapsed = event.now_mono_ms.saturating_sub(armed_at);
                    self.counters.rejected_z4_arming_guard_events = self
                        .counters
                        .rejected_z4_arming_guard_events
                        .saturating_add(1);
                    return Err(InputRejection {
                        reason: InputRejectionReason::Z4ArmingWindowActive,
                        message: format!(
                            "Z4 350ms clickjacking arming guard active ({elapsed}ms < {Z4_ARMING_WINDOW_MS}ms)"
                        ),
                    });
                }
                self.counters.accepted_events = self.counters.accepted_events.saturating_add(1);
                Ok(RoutedInputOutcome {
                    routed_plane: "Z4",
                    target_window_id: None,
                    target_surface_id: None,
                })
            }
            InputTarget::Z4SessionUnlock => {
                self.counters.accepted_events = self.counters.accepted_events.saturating_add(1);
                Ok(RoutedInputOutcome {
                    routed_plane: "Z4",
                    target_window_id: None,
                    target_surface_id: None,
                })
            }
            InputTarget::Z3SystemShortcut { .. } => {
                self.counters.accepted_events = self.counters.accepted_events.saturating_add(1);
                Ok(RoutedInputOutcome {
                    routed_plane: "Z3",
                    target_window_id: None,
                    target_surface_id: None,
                })
            }
            // 规则 4：Z2 窗口表面安装代际与 PRESENTED 状态围栏。
            InputTarget::Z2Surface {
                window_id,
                surface_id,
                target_surface_generation,
            } => {
                let Some(win) = lookup_window(*window_id) else {
                    self.counters.rejected_stale_surface_events = self
                        .counters
                        .rejected_stale_surface_events
                        .saturating_add(1);
                    return Err(InputRejection {
                        reason: InputRejectionReason::StaleOrUnpresentedSurface,
                        message: format!("target window {} does not exist", hex(window_id)),
                    });
                };
                if win.surface_id != *surface_id
                    || *target_surface_generation != win.current_installation_generation
                    || win.surface_generation != win.current_installation_generation
                    || win.stale_generation
                    || win.lifecycle.surface != SurfaceLifecycle::Presented
                    || win.lifecycle.window != WindowLifecycle::Visible
                {
                    self.counters.rejected_stale_surface_events = self
                        .counters
                        .rejected_stale_surface_events
                        .saturating_add(1);
                    return Err(InputRejection {
                        reason: InputRejectionReason::StaleOrUnpresentedSurface,
                        message: format!(
                            "surface {} of window {} rejected input (event_gen={target_surface_generation}, surface_gen={}, current_gen={}, stale={}, surface_state={})",
                            hex(surface_id),
                            hex(window_id),
                            win.surface_generation,
                            win.current_installation_generation,
                            win.stale_generation,
                            win.lifecycle.surface.as_str()
                        ),
                    });
                }
                self.set_focus(*window_id, *surface_id, win.surface_generation);
                self.counters.accepted_events = self.counters.accepted_events.saturating_add(1);
                Ok(RoutedInputOutcome {
                    routed_plane: "Z2",
                    target_window_id: Some(*window_id),
                    target_surface_id: Some(*surface_id),
                })
            }
        }
    }
}
