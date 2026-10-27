//! Stage D / §25.4 窗口（`Window`）与表面（`Surface`）双轨耦合生命周期状态机。
//!
//! 权威链（`06-架构设计总纲-v0.5.md` §25.4 & `ADR-0025` 决定 1）：
//! - `Surface`: `REGISTERED → CREATED → PRESENTED ↔ HIDDEN → CLOSED`
//! - `Window`:  `CREATED → VISIBLE ↔ MINIMIZED → CLOSING → CLOSED`
//!
//! 窗口开合与最小化确定性驱动底层受控沙箱 `Surface`：处于 `MINIMIZED`、`CLOSING` 或
//! `CLOSED` 的窗口绝不维持 `SurfaceLifecycle::Presented`，从状态机层面阻断隐藏/关闭中表面接收输入。

use std::fmt;

/// Spec surface lifecycle states (`REGISTERED` … `CLOSED`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SurfaceLifecycle {
    Registered,
    Created,
    Presented,
    Hidden,
    Closed,
}

/// Illegal transition on the surface lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceLifecycleError {
    /// Terminal `CLOSED` rejects further create/open/hide/close.
    AlreadyClosed,
    /// `create` is only legal from `REGISTERED` (or an idempotent `CREATED`).
    NotRegistered,
    /// `hide` is only legal from `PRESENTED` (or an idempotent `HIDDEN`).
    NotPresented,
}

impl fmt::Display for SurfaceLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyClosed => formatter.write_str("surface is already CLOSED"),
            Self::NotRegistered => {
                formatter.write_str("surface create requires REGISTERED or CREATED state")
            }
            Self::NotPresented => {
                formatter.write_str("surface hide requires PRESENTED or HIDDEN state")
            }
        }
    }
}

impl std::error::Error for SurfaceLifecycleError {}

impl SurfaceLifecycle {
    /// Fresh surface starts at the spec initial state `REGISTERED`.
    #[must_use]
    pub const fn registered() -> Self {
        Self::Registered
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "REGISTERED",
            Self::Created => "CREATED",
            Self::Presented => "PRESENTED",
            Self::Hidden => "HIDDEN",
            Self::Closed => "CLOSED",
        }
    }

    /// `REGISTERED → CREATED`. A second call on `CREATED` stays there.
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` on `CLOSED`. `NotRegistered` from `PRESENTED` or `HIDDEN`.
    pub fn create(self) -> Result<Self, SurfaceLifecycleError> {
        match self {
            Self::Registered | Self::Created => Ok(Self::Created),
            Self::Closed => Err(SurfaceLifecycleError::AlreadyClosed),
            Self::Presented | Self::Hidden => Err(SurfaceLifecycleError::NotRegistered),
        }
    }

    /// Open advances toward presentation (`REGISTERED`/`CREATED`/`HIDDEN` → `PRESENTED`).
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` when the surface is already `CLOSED`.
    pub fn open(self) -> Result<Self, SurfaceLifecycleError> {
        match self {
            Self::Registered | Self::Created | Self::Hidden => Ok(Self::Presented),
            Self::Presented => Ok(Self::Presented),
            Self::Closed => Err(SurfaceLifecycleError::AlreadyClosed),
        }
    }

    /// `PRESENTED → HIDDEN`. A second call on `HIDDEN` stays there.
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` on `CLOSED`. `NotPresented` from `REGISTERED` or `CREATED`.
    pub fn hide(self) -> Result<Self, SurfaceLifecycleError> {
        match self {
            Self::Presented | Self::Hidden => Ok(Self::Hidden),
            Self::Closed => Err(SurfaceLifecycleError::AlreadyClosed),
            Self::Registered | Self::Created => Err(SurfaceLifecycleError::NotPresented),
        }
    }

    /// Close reaches the spec terminal `CLOSED`.
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` when the surface is already `CLOSED`.
    pub fn close(self) -> Result<Self, SurfaceLifecycleError> {
        match self {
            Self::Closed => Err(SurfaceLifecycleError::AlreadyClosed),
            Self::Registered | Self::Created | Self::Presented | Self::Hidden => Ok(Self::Closed),
        }
    }
}

/// Spec window lifecycle states (`CREATED → VISIBLE ↔ MINIMIZED → CLOSING → CLOSED`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WindowLifecycle {
    Created,
    Visible,
    Minimized,
    Closing,
    Closed,
}

/// Illegal transition on the window or coupled window/surface lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowLifecycleError {
    /// Terminal `CLOSED` rejects all further transitions.
    AlreadyClosed,
    /// A window in `CLOSING` may only proceed to `CLOSED`, never back to `VISIBLE` or `MINIMIZED`.
    ClosingInProgress,
    /// `minimize` is only legal from `VISIBLE` (or idempotent `MINIMIZED`).
    NotVisible,
    /// Coupled `SurfaceLifecycle` transition failed.
    Surface(SurfaceLifecycleError),
}

impl fmt::Display for WindowLifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyClosed => formatter.write_str("window is already CLOSED"),
            Self::ClosingInProgress => {
                formatter.write_str("window is CLOSING and cannot be shown or minimized")
            }
            Self::NotVisible => {
                formatter.write_str("window minimize requires VISIBLE or MINIMIZED state")
            }
            Self::Surface(source) => write!(formatter, "coupled surface error: {source}"),
        }
    }
}

impl std::error::Error for WindowLifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Surface(error) => Some(error),
            _ => None,
        }
    }
}

impl WindowLifecycle {
    #[must_use]
    pub const fn created() -> Self {
        Self::Created
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Visible => "VISIBLE",
            Self::Minimized => "MINIMIZED",
            Self::Closing => "CLOSING",
            Self::Closed => "CLOSED",
        }
    }

    /// `CREATED | MINIMIZED | VISIBLE → VISIBLE`.
    ///
    /// # Errors
    ///
    /// `ClosingInProgress` from `CLOSING`; `AlreadyClosed` from `CLOSED`.
    pub fn show(self) -> Result<Self, WindowLifecycleError> {
        match self {
            Self::Created | Self::Minimized | Self::Visible => Ok(Self::Visible),
            Self::Closing => Err(WindowLifecycleError::ClosingInProgress),
            Self::Closed => Err(WindowLifecycleError::AlreadyClosed),
        }
    }

    /// Alias of [`Self::show`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::show`].
    pub fn present(self) -> Result<Self, WindowLifecycleError> {
        self.show()
    }

    /// `VISIBLE | MINIMIZED → MINIMIZED`.
    ///
    /// # Errors
    ///
    /// `NotVisible` from `CREATED`; `ClosingInProgress` from `CLOSING`; `AlreadyClosed` from `CLOSED`.
    pub fn minimize(self) -> Result<Self, WindowLifecycleError> {
        match self {
            Self::Visible | Self::Minimized => Ok(Self::Minimized),
            Self::Created => Err(WindowLifecycleError::NotVisible),
            Self::Closing => Err(WindowLifecycleError::ClosingInProgress),
            Self::Closed => Err(WindowLifecycleError::AlreadyClosed),
        }
    }

    /// `CREATED | VISIBLE | MINIMIZED | CLOSING → CLOSING`.
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` from `CLOSED`.
    pub fn begin_close(self) -> Result<Self, WindowLifecycleError> {
        match self {
            Self::Created | Self::Visible | Self::Minimized | Self::Closing => Ok(Self::Closing),
            Self::Closed => Err(WindowLifecycleError::AlreadyClosed),
        }
    }

    /// `CREATED | VISIBLE | MINIMIZED | CLOSING → CLOSED`.
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` from `CLOSED`.
    pub fn finish_close(self) -> Result<Self, WindowLifecycleError> {
        match self {
            Self::Created | Self::Visible | Self::Minimized | Self::Closing => Ok(Self::Closed),
            Self::Closed => Err(WindowLifecycleError::AlreadyClosed),
        }
    }

    /// Immediate close (`→ CLOSED`).
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` from `CLOSED`.
    pub fn close_immediately(self) -> Result<Self, WindowLifecycleError> {
        self.begin_close()?.finish_close()
    }
}

/// Coupled `(WindowLifecycle, SurfaceLifecycle)` pair enforcing `[DUI-WINDOW-001]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoupledWindowSurface {
    pub window: WindowLifecycle,
    pub surface: SurfaceLifecycle,
}

impl CoupledWindowSurface {
    /// Initializes `WindowLifecycle::Created` paired with `SurfaceLifecycle::Created`
    /// (`REGISTERED → CREATED`).
    #[must_use]
    pub const fn new_created() -> Self {
        Self {
            window: WindowLifecycle::Created,
            surface: SurfaceLifecycle::Created,
        }
    }

    #[must_use]
    pub const fn can_accept_input(self) -> bool {
        matches!(self.window, WindowLifecycle::Visible)
            && matches!(self.surface, SurfaceLifecycle::Presented)
    }

    /// `Created | Minimized | Visible → Visible` coupled with `surface.open() → Presented`.
    ///
    /// # Errors
    ///
    /// Fails closed when closing or already closed.
    pub fn show(self) -> Result<Self, WindowLifecycleError> {
        let next_window = self.window.show()?;
        let next_surface = self.surface.open().map_err(WindowLifecycleError::Surface)?;
        Ok(Self {
            window: next_window,
            surface: next_surface,
        })
    }

    /// Alias of [`Self::show`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::show`].
    pub fn present(self) -> Result<Self, WindowLifecycleError> {
        self.show()
    }

    /// `Visible | Minimized → Minimized` coupled with `surface.hide() → Hidden`.
    ///
    /// # Errors
    ///
    /// Fails closed when not visible, closing, or closed.
    pub fn minimize(self) -> Result<Self, WindowLifecycleError> {
        let next_window = self.window.minimize()?;
        let next_surface = self.surface.hide().map_err(WindowLifecycleError::Surface)?;
        Ok(Self {
            window: next_window,
            surface: next_surface,
        })
    }

    /// `Created | Visible | Minimized | Closing → Closing`.
    ///
    /// If `surface == Presented`, immediately transitions `surface → Hidden` so a
    /// closing window never remains presentable for input.
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` when already closed.
    pub fn begin_close(self) -> Result<Self, WindowLifecycleError> {
        let next_window = self.window.begin_close()?;
        let next_surface = match self.surface {
            SurfaceLifecycle::Presented => {
                self.surface.hide().map_err(WindowLifecycleError::Surface)?
            }
            SurfaceLifecycle::Closed => return Err(WindowLifecycleError::AlreadyClosed),
            other => other,
        };
        Ok(Self {
            window: next_window,
            surface: next_surface,
        })
    }

    /// Completes closing (`→ Closed`) coupled with `surface.close() → Closed`.
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` when already closed.
    pub fn finish_close(self) -> Result<Self, WindowLifecycleError> {
        let next_window = self.window.finish_close()?;
        let next_surface = self
            .surface
            .close()
            .map_err(WindowLifecycleError::Surface)?;
        Ok(Self {
            window: next_window,
            surface: next_surface,
        })
    }

    /// Single-step close (`begin_close` followed by `finish_close`).
    ///
    /// # Errors
    ///
    /// `AlreadyClosed` when already closed.
    pub fn close_immediately(self) -> Result<Self, WindowLifecycleError> {
        self.begin_close()?.finish_close()
    }
}
