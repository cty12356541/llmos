//! F3-c2:可替换的本地 IPC 传输面(Unix domain socket ↔ Windows named pipe)。
//!
//! `ipc.rs` 的认证派发不再直接绑死 Unix transport:`dispatch_authenticated`
//! 是唯一传输入口,按 cfg 选择实现——
//!
//! - **Unix**:整体委托 `nlos_system_control::auth::dispatch_over_authenticated_socket`
//!   (W32 起的既有路径,字节与行为零改动);
//! - **Windows**:用 `nlos-ipc` 既有的 named pipe 传输原语
//!   (`nlos_ipc::windows`:`reject_remote_clients` 服务端、
//!   `SECURITY_IDENTIFICATION` 客户端——仓库根 workspace CI 的
//!   windows-latest 腿已实测编译)加**平台中立**的 ADR-0011 握手原语
//!   (`nlos_ipc::handshake`:challenge/attestation 编解码、
//!   `HandshakeNonceRegistry`、`verify_attestation`——上游仅
//!   `handshake::transport`(Unix 接线层)是 cfg(unix),原语本身无平台分支)
//!   在 desktop 侧装配出同一语义:connect → challenge → sign → attest →
//!   exchange → `ControlReceipt::compose`。
//!
//! 通道绑定:上游 Unix 路径按端点路径字节派生
//! (`handshake::transport::endpoint_channel_binding`,域分隔
//! `llmos/ipc-channel-binding/v1`);Windows 路径以本模块
//! [`endpoint_channel_binding`] 按规整后的管道名派生,同一域分隔符,
//! 客户端与 dev-fixture 服务端共用本函数(单一派生点,不会漂移)。
//!
//! ## 边界(F3-c3,不做假凭据桩)
//!
//! 本模块只做**传输通道**,不做 OS 凭据校验:
//!
//! - Unix 服务端凭据门(0600 权限位 + `peer_cred` uid/gid)住在
//!   `nlos-system-control` daemon,不在本 crate;
//! - Windows 等价物(named pipe 安全描述符 / 客户端进程令牌解析,
//!   `PeerIdentity::WindowsNamedPipe.process_id` 目前恒 `None`——
//!   `nlos_ipc::windows` 尚未解析对端 pid)属 F3-c3;在落地前,
//!   dev-fixture 服务端以 `AllowPeer` 放行——与 Unix 夹具现状一致
//!   (Unix 夹具同样用 `AllowPeer`,生产凭据门在 daemon);
//! - principal 级认证(ADR-0011 challenge-response)是协议层,平台中立,
//!   两平台都真实执行:没有 plain socket 捷径。
//!
//! TODO(F3-c3):Windows 生产服务端凭据面(`GetNamedPipeClientProcessId`
//! /进程令牌 ACL 门)+ 与真实 Windows SystemControl daemon 的通道绑定
//! 约定对齐(当前只有 dev-fixture 双端共读本模块派生)。

use nlos_system_control::control::{
    ApplicationInspector, ControlCommand, ControlReceipt, ProcessInspector, ResourceInspector,
};
use nlos_types::PrincipalId;

#[cfg(windows)]
use crate::error::ErrorCode;
use crate::error::{DesktopError, from_control_error};

/// 经 ADR-0011 认证传输面派发一条命令并投影 Receipt(平台分派点;
/// `ipc.rs` 的唯一传输入口)。
///
/// # Errors
///
/// Unix:上游 `ControlError` 原样经 `from_control_error` 映射。
/// Windows:握手拒绝 → `ErrorCode::Handshake`,传输失败 → `ErrorCode::Ipc`,
/// 命令/回执契约违反 → `from_control_error` 同口径。
#[cfg(unix)]
pub async fn dispatch_authenticated<S>(
    endpoint: &str,
    principal: PrincipalId,
    sign: S,
    command: &ControlCommand,
    process: Option<&dyn ProcessInspector>,
    resource: Option<&dyn ResourceInspector>,
    application: Option<&dyn ApplicationInspector>,
) -> Result<ControlReceipt, DesktopError>
where
    S: Fn(&[u8; 32]) -> Result<[u8; 64], nlos_ipc::handshake::HandshakeError>,
{
    let receipt = nlos_system_control::auth::dispatch_over_authenticated_socket(
        endpoint,
        principal,
        sign,
        command,
        process,
        resource,
        application,
    )
    .await
    .map_err(|error| from_control_error(&error))?;
    Ok(receipt)
}

/// [`dispatch_authenticated`] 的 Windows named pipe 实现(语义镜像上游
/// `dispatch_over_authenticated_socket`:同一 envelope 构造、同一
/// `TransportConfig::default()` 预算、同一 `ControlReceipt::compose`)。
#[cfg(windows)]
pub async fn dispatch_authenticated<S>(
    endpoint: &str,
    principal: PrincipalId,
    sign: S,
    command: &ControlCommand,
    process: Option<&dyn ProcessInspector>,
    resource: Option<&dyn ResourceInspector>,
    application: Option<&dyn ApplicationInspector>,
) -> Result<ControlReceipt, DesktopError>
where
    S: Fn(&[u8; 32]) -> Result<[u8; 64], nlos_ipc::handshake::HandshakeError>,
{
    use nlos_ipc::{LocalRpcClient, TransportConfig};
    use nlos_schema::sabi::v1::ExchangeRequest;
    use nlos_system_control::control::build_request_envelope;

    let name = normalize_endpoint(endpoint)?;
    let request = build_request_envelope(command).map_err(|error| from_control_error(&error))?;
    let config = TransportConfig::default();
    let framed = authenticated_connect(&name, config, principal, sign)
        .await
        .map_err(handshake_error)?;
    let response = LocalRpcClient::new(framed.into_inner(), config)
        .exchange_validated(ExchangeRequest {
            envelope: Some(request),
        })
        .await
        .map_err(|error| DesktopError::ipc(format!("control transport: {error}")))?;
    ControlReceipt::compose(command, response.envelope(), process, resource, application)
        .map_err(|error| from_control_error(&error))
}

/// 其余平台没有认证传输面可用(fail-closed,不以桩糊绿)。
#[cfg(not(any(unix, windows)))]
pub async fn dispatch_authenticated<S>(
    _endpoint: &str,
    _principal: PrincipalId,
    _sign: S,
    _command: &ControlCommand,
    _process: Option<&dyn ProcessInspector>,
    _resource: Option<&dyn ResourceInspector>,
    _application: Option<&dyn ApplicationInspector>,
) -> Result<ControlReceipt, DesktopError>
where
    S: Fn(&[u8; 32]) -> Result<[u8; 64], nlos_ipc::handshake::HandshakeError>,
{
    Err(DesktopError::unsupported_platform())
}

// ---------------------------------------------------------------------------
// Windows named pipe 传输细节
// ---------------------------------------------------------------------------

/// Windows named pipe 端点规整形态:`\\.\pipe\<name>`。
pub const WINDOWS_PIPE_PREFIX: &str = r"\\.\pipe\";

/// 规整会话配置里的 Windows 端点地址。
///
/// 接受三种形态并归一到 `\\.\pipe\<name>`(ASCII 小写——named pipe 命名空间
/// 大小写不敏感,通道绑定按规整形态派生,避免大小写漂移造成假性绑定失配):
/// 裸名 `name`、本地全名 `\\.\pipe\name` / `\\?\pipe\name`。其余形态
/// (远端 `\\server\pipe\...`、文件系统路径等)是配置错误——本传输面只收
/// **本地** named pipe 端点(与服务端 `reject_remote_clients` 同姿态)。
///
/// # Errors
///
/// 空白地址或非本地 named pipe 形态 → `ErrorCode::Config`。
#[cfg(windows)]
pub fn normalize_endpoint(raw: &str) -> Result<String, DesktopError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DesktopError::config(
            "Windows 认证端点为空;expected \\\\.\\pipe\\<name> named pipe 形态",
        ));
    }
    let name = if let Some(name) = trimmed.strip_prefix(WINDOWS_PIPE_PREFIX) {
        name.to_owned()
    } else if let Some(name) = trimmed.strip_prefix(r"\\?\pipe\") {
        name.to_owned()
    } else if trimmed.contains('\\') {
        return Err(DesktopError::config(format!(
            "Windows 传输面只接受本地 named pipe 端点(\\\\.\\pipe\\<name>);got: {trimmed}"
        )));
    } else {
        trimmed.to_owned()
    };
    if name.is_empty() {
        return Err(DesktopError::config(
            "Windows named pipe 端点缺少管道名(\\\\.\\pipe\\<name>)",
        ));
    }
    Ok(format!(
        "{WINDOWS_PIPE_PREFIX}{}",
        name.to_ascii_lowercase()
    ))
}

/// 通道绑定派生域(镜像上游 `nlos_ipc::handshake::transport` 的
/// `CHANNEL_BINDING_DOMAIN`;上游函数整体 cfg(unix),Windows 无法复用,
/// 常量逐字相同以保持协议同源)。
#[cfg(windows)]
const CHANNEL_BINDING_DOMAIN: &[u8] = b"llmos/ipc-channel-binding/v1";

/// 按(规整后的)Windows 端点名派生 32 字节通道绑定:与上游 Unix 路径的
/// `endpoint_channel_binding` 同构(域分隔 SHA-256,末域为端点名字节)。
/// 客户端([`authenticated_connect`])与 dev-fixture 服务端共用本函数。
#[cfg(windows)]
pub fn endpoint_channel_binding(name: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(CHANNEL_BINDING_DOMAIN);
    hasher.update(name.as_bytes());
    hasher.finalize().into()
}

/// Windows 客户端握手:连接 named pipe 并应答服务端 challenge
/// (镜像上游 `nlos_ipc::handshake::transport::authenticated_connect`,
/// 仅传输原语由 Unix socket 换成 `nlos_ipc::windows::connect`;
/// `SECURITY_IDENTIFICATION` 防服务端经由不可信端点名冒充本客户端)。
///
/// # Errors
///
/// 传输失败(含 connect/read/write 超时)与 schema 违反都是类型化
/// `HandshakeError`(fail-closed,零字节请求上网)。
#[cfg(windows)]
async fn authenticated_connect<S>(
    name: &str,
    config: nlos_ipc::TransportConfig,
    principal: PrincipalId,
    sign: S,
) -> Result<
    nlos_ipc::FramedIo<tokio::net::windows::named_pipe::NamedPipeClient>,
    nlos_ipc::handshake::HandshakeError,
>
where
    S: Fn(&[u8; 32]) -> Result<[u8; 64], nlos_ipc::handshake::HandshakeError>,
{
    use nlos_ipc::handshake::{
        HandshakeError, client_attestation, decode_challenge_wire, encode_attestation_wire,
        principal_handshake_message,
    };
    use nlos_ipc::{FramedIo, windows};
    use nlos_schema::HANDSHAKE_NONCE_BYTES;

    let channel_binding = endpoint_channel_binding(name);
    let (stream, _peer) = windows::connect(name, config)
        .await
        .map_err(HandshakeError::Transport)?;
    let mut framed = FramedIo::new(stream, config);

    let challenge_wire = framed.receive().await.map_err(HandshakeError::Transport)?;
    let challenge = decode_challenge_wire(&challenge_wire)?;
    // 上游 `attestation_nonce` 为私有助手;此处同口径手工提升
    // (定长 try_into,失配为同一类型化 MalformedAttestation)。
    let nonce: [u8; HANDSHAKE_NONCE_BYTES] = challenge
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| HandshakeError::MalformedAttestation("nonce length"))?;
    let digest = principal_handshake_message(&nonce, principal, &channel_binding);
    let signature = sign(&digest)?;
    let attestation = client_attestation(principal, &challenge, &channel_binding, signature)?;
    framed
        .send(&encode_attestation_wire(&attestation)?)
        .await
        .map_err(HandshakeError::Transport)?;
    Ok(framed)
}

/// Windows 握手拒绝 → 稳定错误码(与 `from_control_error` 的 Unix
/// `ControlError::Handshake` 臂同文案;上游变体仅 Unix 存在,此处直接映射)。
#[cfg(windows)]
fn handshake_error(error: nlos_ipc::handshake::HandshakeError) -> DesktopError {
    DesktopError::new(
        ErrorCode::Handshake,
        format!("control handshake refused: {error}"),
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn normalize_endpoint_accepts_local_forms() {
        assert_eq!(
            normalize_endpoint(r"\\.\pipe\llmos-sc").as_deref(),
            Ok(r"\\.\pipe\llmos-sc")
        );
        assert_eq!(
            normalize_endpoint(r"\\?\pipe\llmos-sc").as_deref(),
            Ok(r"\\.\pipe\llmos-sc")
        );
        assert_eq!(
            normalize_endpoint("llmos-sc").as_deref(),
            Ok(r"\\.\pipe\llmos-sc")
        );
        // 命名空间大小写不敏感:规整到小写,绑定派生不会因大小写漂移失配。
        assert_eq!(
            normalize_endpoint(r"\\.\pipe\LLMOS-SC").as_deref(),
            Ok(r"\\.\pipe\llmos-sc")
        );
    }

    #[test]
    fn normalize_endpoint_rejects_empty_remote_and_path_shapes() {
        assert_eq!(
            normalize_endpoint("   ").unwrap_err().code,
            ErrorCode::Config
        );
        assert_eq!(
            normalize_endpoint(r"\\.\pipe\").unwrap_err().code,
            ErrorCode::Config
        );
        // 远端 pipe 与文件系统路径都不是本地 named pipe 端点。
        assert_eq!(
            normalize_endpoint(r"\\server\pipe\llmos-sc")
                .unwrap_err()
                .code,
            ErrorCode::Config
        );
        assert_eq!(
            normalize_endpoint(r"C:\llmos\run.sock").unwrap_err().code,
            ErrorCode::Config
        );
    }

    #[test]
    fn channel_binding_is_deterministic_and_endpoint_sensitive() {
        let first = endpoint_channel_binding(r"\\.\pipe\llmos-sc");
        assert_eq!(first, endpoint_channel_binding(r"\\.\pipe\llmos-sc"));
        assert_ne!(first, endpoint_channel_binding(r"\\.\pipe\llmos-other"));
        // 与裸名规整后的同一端点派生一致(规整是绑定前的单一规整点)。
        assert_eq!(
            first,
            endpoint_channel_binding(&normalize_endpoint("llmos-sc").unwrap())
        );
    }
}
