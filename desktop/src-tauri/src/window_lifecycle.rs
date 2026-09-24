//! Stage D / §25 最小窗口表面生命周期(规范已点名的状态机切片)。
//!
//! 权威链(v0.5 桌面契约最小生命周期):
//! `Surface: REGISTERED → CREATED → PRESENTED ↔ HIDDEN → CLOSED`
//!
//! 本模块只落地规范已列状态上的 open/close 终态动作,不引入窗口管理器、
//! 合成器、焦点/几何或多模态 UI。

/// Spec surface lifecycle states (`REGISTERED` … `CLOSED`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceLifecycle {
    Registered,
    Created,
    Presented,
    Hidden,
    Closed,
}

/// Illegal open/close transition on the surface lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceLifecycleError {
    /// Terminal `CLOSED` rejects further open/close.
    AlreadyClosed,
}

impl SurfaceLifecycle {
    /// Fresh surface starts at the spec initial state `REGISTERED`.
    #[must_use]
    pub const fn registered() -> Self {
        Self::Registered
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
