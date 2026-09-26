//! Stage D / §25 最小窗口表面生命周期(规范已点名的状态机切片)。
//!
//! 权威链(v0.5 桌面契约最小生命周期):
//! `Surface: REGISTERED → CREATED → PRESENTED ↔ HIDDEN → CLOSED`
//!
//! 本模块只落地规范已列状态上的 create/open/hide/close,不引入窗口管理器、
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

impl SurfaceLifecycle {
    /// Fresh surface starts at the spec initial state `REGISTERED`.
    #[must_use]
    pub const fn registered() -> Self {
        Self::Registered
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
