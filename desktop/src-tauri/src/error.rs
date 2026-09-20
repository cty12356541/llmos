//! Tauri 命令面的类型化错误。每个命令都以 `Result<_, DesktopError>` 收口,
//! 序列化为 `{ code, message }`;后端命令零 `unwrap`/零 panic。

use std::fmt;

use nlos_system_control::control::ControlError;

/// 稳定错误码(GUI 按码展示,不解析消息文本)。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// 会话配置缺失或非法(socket/principal/key 文件/CLI 参数)。
    Config,
    /// ADR-0011 challenge-response 握手被拒(`HandshakeError`)。
    Handshake,
    /// 本地 IPC 传输失败(connect/timeout/frame)。
    Ipc,
    /// 命令编译或回执投影违反控制面契约(`ControlError` 的其余形态)。
    Control,
    /// 当前平台没有认证入口可用(认证 dispatch 目前仅 Unix)。
    UnsupportedPlatform,
    /// 不应发生的一致性缺口(防御性,不承载业务语义)。
    Internal,
}

/// 类型化命令错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopError {
    pub code: ErrorCode,
    pub message: String,
}

impl DesktopError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Config, message)
    }

    #[must_use]
    pub fn ipc(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Ipc, message)
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    #[must_use]
    pub fn unsupported_platform() -> Self {
        Self::new(
            ErrorCode::UnsupportedPlatform,
            "认证 SystemControl 入口目前仅提供 Unix socket 接线;Windows named-pipe 认证入口为后续波次",
        )
    }
}

impl fmt::Display for DesktopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for DesktopError {}

impl serde::Serialize for DesktopError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("DesktopError", 2)?;
        state.serialize_field("code", &self.code)?;
        state.serialize_field("message", &self.message)?;
        state.end()
    }
}

/// 把 `nlos-system-control` 客户端侧 `ControlError` 映射为稳定错误码:
/// 握手拒绝 → `Handshake`,传输 → `Ipc`,命令/契约 → `Config`/`Control`。
/// 本 crate 对 `nlos-system-control` 恒启用 `cli` feature,故 `Ipc` 恒存在;
/// `Handshake` 变体上游仅在 Unix 存在。
pub fn from_control_error(error: &nlos_system_control::control::ControlError) -> DesktopError {
    match error {
        ControlError::InvalidCommand(reason) => {
            DesktopError::config(format!("invalid control command: {reason}"))
        }
        ControlError::Schema(source) => DesktopError::new(
            ErrorCode::Control,
            format!("control payload contract: {source}"),
        ),
        ControlError::UnexpectedResponse(reason) => DesktopError::new(
            ErrorCode::Control,
            format!("unexpected control response: {reason}"),
        ),
        ControlError::Ipc(source) => DesktopError::ipc(format!("control transport: {source}")),
        #[cfg(unix)]
        ControlError::Handshake(source) => DesktopError::new(
            ErrorCode::Handshake,
            format!("control handshake refused: {source}"),
        ),
    }
}
