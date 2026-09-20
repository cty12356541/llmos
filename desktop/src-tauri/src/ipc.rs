//! 认证 SystemControl IPC 客户端接线(W32-A 只读半)。
//!
//! 每个只读命令都通过 [`nlos_system_control::auth::dispatch_over_authenticated_socket`]
//! ——ADR-0011 challenge-response 认证入口——到达真实 SystemControl 服务,
//! 没有 plain socket 捷径。principal 私钥不入仓库:会话配置来自环境变量或
//! GUI 会话内设置,签名密钥始终从 operator 提供的 `0600` 密钥文件在派发时读取。
//!
//! 一致性自检(`parity_check`)把同一只读命令再经真实 `system-control-cli`
//! 二进制(plain 入口,本地信任域)派发一次,比对两侧
//! `ControlReceipt::to_bytes` hex——与 B-TASK-006L 已固化的三入口字节一致
//! 契约同源。写入/控制动作是 W32-B,本模块不提供任何 mutation 派发。

use std::sync::Mutex;

use ed25519_dalek::{Signer, SigningKey};
use nlos_system_control::control::{ControlCommand, parse_hex_id};
use nlos_types::PrincipalId;
use serde::Deserialize;

use crate::dto::{ConfigDto, ConfigSourceDto, ParityDto, ReceiptDto, receipt_dto};
use crate::error::{DesktopError, from_control_error};

/// 环境变量名(README 记录;GUI 内可会话级覆盖)。
pub const ENV_SOCKET: &str = "LLMOS_DESKTOP_SOCKET";
pub const ENV_PRINCIPAL: &str = "LLMOS_DESKTOP_PRINCIPAL";
pub const ENV_KEY_FILE: &str = "LLMOS_DESKTOP_KEY_FILE";
pub const ENV_CLI_SOCKET: &str = "LLMOS_DESKTOP_CLI_SOCKET";
pub const ENV_CLI: &str = "LLMOS_DESKTOP_CLI";

/// `cargo run`/`tauri dev` 的 cwd 是 `src-tauri`,仓库 target 目录在此相对路径下。
const DEFAULT_CLI_PATH: &str = "../../target/debug/system-control-cli";

/// 会话连接配置(内存态;不落盘)。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionConfig {
    pub socket_path: Option<String>,
    pub principal_hex: Option<String>,
    pub key_file: Option<String>,
    pub cli_socket: Option<String>,
    pub cli_path: Option<String>,
    pub source: ConfigSourceDto,
}

/// Tauri 管理态:仅持有一份会话配置;派发路径无共享可变状态。
pub struct AppState {
    config: Mutex<SessionConfig>,
}

impl AppState {
    #[must_use]
    pub fn from_env() -> Self {
        let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let any_set = [
            ENV_SOCKET,
            ENV_PRINCIPAL,
            ENV_KEY_FILE,
            ENV_CLI_SOCKET,
            ENV_CLI,
        ]
        .iter()
        .any(|name| env(name).is_some());
        let source = if any_set {
            ConfigSourceDto::Env
        } else {
            ConfigSourceDto::Unset
        };
        Self {
            config: Mutex::new(SessionConfig {
                socket_path: env(ENV_SOCKET),
                principal_hex: env(ENV_PRINCIPAL),
                key_file: env(ENV_KEY_FILE),
                cli_socket: env(ENV_CLI_SOCKET),
                cli_path: env(ENV_CLI),
                source,
            }),
        }
    }

    fn snapshot(&self) -> Result<SessionConfig, DesktopError> {
        self.config
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| DesktopError::internal("会话配置锁中毒"))
    }

    fn replace(&self, next: SessionConfig) -> Result<(), DesktopError> {
        self.config
            .lock()
            .map(|mut guard| *guard = next)
            .map_err(|_| DesktopError::internal("会话配置锁中毒"))
    }
}

fn config_dto(config: &SessionConfig) -> ConfigDto {
    ConfigDto {
        socket_path: config.socket_path.clone(),
        principal_hex: config.principal_hex.clone(),
        key_file: config.key_file.clone(),
        cli_socket: config.cli_socket.clone(),
        cli_path: config.cli_path.clone(),
        source: config.source,
        platform_supported: cfg!(unix),
    }
}

/// 32 hex 字符 → 16 字节(复用控制面 fail-closed 解析器,不做第二套规则)。
fn principal_bytes(principal_hex: &str) -> Result<[u8; 16], DesktopError> {
    parse_hex_id(principal_hex).map_err(|error| from_control_error(&error))
}

/// 64 hex 字符 Ed25519 种子文件 → 32 字节种子。
fn key_seed(key_file: &str) -> Result<[u8; 32], DesktopError> {
    let invalid = || DesktopError::config("密钥文件必须是 64 个 hex 字符的 Ed25519 种子");
    let content = std::fs::read_to_string(key_file)
        .map_err(|error| DesktopError::config(format!("读取密钥文件失败: {error}")))?;
    let trimmed = content.trim();
    let raw = trimmed.as_bytes();
    if raw.len() != 64 || !raw.iter().all(u8::is_ascii_hexdigit) {
        return Err(invalid());
    }
    let mut seed = [0u8; 32];
    for (index, byte) in seed.iter_mut().enumerate() {
        let hi = (raw[2 * index] as char).to_digit(16).ok_or_else(invalid)?;
        let lo = (raw[2 * index + 1] as char)
            .to_digit(16)
            .ok_or_else(invalid)?;
        *byte = u8::try_from(hi * 16 + lo).map_err(|_| invalid())?;
    }
    Ok(seed)
}

fn required(config: &SessionConfig) -> Result<(String, String, String), DesktopError> {
    let missing = |field: &str| {
        DesktopError::config(format!("缺少 {field};先在「连接配置」页或环境变量提供"))
    };
    Ok((
        config
            .socket_path
            .clone()
            .ok_or_else(|| missing("认证 socket 路径"))?,
        config
            .principal_hex
            .clone()
            .ok_or_else(|| missing("principal(32 hex)"))?,
        config
            .key_file
            .clone()
            .ok_or_else(|| missing("key_file 路径"))?,
    ))
}

/// 经 ADR-0011 认证入口派发一条只读命令并投影 Receipt(纯函数核心,
/// 由 Tauri 命令与集成测试共用)。
#[cfg(unix)]
pub async fn dispatch_read(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    command: ControlCommand,
) -> Result<ReceiptDto, DesktopError> {
    use nlos_system_control::auth::dispatch_over_authenticated_socket;

    let principal = PrincipalId::from_bytes(principal_bytes(principal_hex)?);
    let seed = key_seed(key_file)?;
    let key = SigningKey::from_bytes(&seed);
    let receipt = dispatch_over_authenticated_socket(
        socket,
        principal,
        |digest| Ok(key.sign(digest).to_bytes()),
        &command,
        None,
        None,
    )
    .await
    .map_err(|error| from_control_error(&error))?;
    Ok(receipt_dto(&receipt))
}

#[cfg(not(unix))]
pub async fn dispatch_read(
    _socket: &str,
    _principal_hex: &str,
    _key_file: &str,
    _command: ControlCommand,
) -> Result<ReceiptDto, DesktopError> {
    Err(DesktopError::unsupported_platform())
}

/// 同步阻塞执行一次认证 dispatch。`dispatch_over_authenticated_socket` 的
/// future 持有上游 `&dyn ProcessInspector/&dyn ResourceInspector` 参数(非
/// `Sync`),不能作为 Tauri async 命令的 `Send` future;同步命令运行在
/// 独立阻塞线程,`block_on` 是正确收口。
fn dispatch_configured(
    state: &tauri::State<'_, AppState>,
    command: ControlCommand,
) -> Result<ReceiptDto, DesktopError> {
    let config = state.snapshot()?;
    let (socket, principal, key_file) = required(&config)?;
    tauri::async_runtime::block_on(dispatch_read(&socket, &principal, &key_file, command))
}

#[tauri::command]
pub fn get_config(state: tauri::State<'_, AppState>) -> Result<ConfigDto, DesktopError> {
    Ok(config_dto(&state.snapshot()?))
}

/// 会话级全量替换(null 字段即清空);principal 立即校验,失败 fail-closed。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigInput {
    pub socket_path: Option<String>,
    pub principal_hex: Option<String>,
    pub key_file: Option<String>,
    pub cli_socket: Option<String>,
    pub cli_path: Option<String>,
}

#[tauri::command]
pub fn set_config(
    state: tauri::State<'_, AppState>,
    input: SetConfigInput,
) -> Result<ConfigDto, DesktopError> {
    let cleaned = |value: &Option<String>| {
        value
            .as_ref()
            .map(|raw| raw.trim().to_owned())
            .filter(|trimmed| !trimmed.is_empty())
    };
    let principal_hex = cleaned(&input.principal_hex);
    if let Some(principal) = principal_hex.as_ref() {
        principal_bytes(principal)?;
    }
    let next = SessionConfig {
        socket_path: cleaned(&input.socket_path),
        principal_hex,
        key_file: cleaned(&input.key_file),
        cli_socket: cleaned(&input.cli_socket),
        cli_path: cleaned(&input.cli_path),
        source: ConfigSourceDto::Session,
    };
    state.replace(next)?;
    Ok(config_dto(&state.snapshot()?))
}

#[tauri::command]
pub fn inspect_health(state: tauri::State<'_, AppState>) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::InspectHealth)
}

#[tauri::command]
pub fn inspect_semantic_health(
    state: tauri::State<'_, AppState>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::InspectSemanticHealth)
}

#[tauri::command]
pub fn export_metrics(state: tauri::State<'_, AppState>) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::ExportMetrics)
}

#[tauri::command]
pub fn export_semantic_metrics(
    state: tauri::State<'_, AppState>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::ExportSemanticMetrics)
}

#[tauri::command]
pub fn inspect_task(
    state: tauri::State<'_, AppState>,
    plan_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let plan_id = principal_bytes(&plan_id_hex)?;
    dispatch_configured(&state, ControlCommand::InspectTask { plan_id })
}

#[tauri::command]
pub fn inspect_process(
    state: tauri::State<'_, AppState>,
    process_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let process_id = principal_bytes(&process_id_hex)?;
    dispatch_configured(&state, ControlCommand::InspectProcess { process_id })
}

#[tauri::command]
pub fn inspect_resource(
    state: tauri::State<'_, AppState>,
    reservation_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let reservation_id = principal_bytes(&reservation_id_hex)?;
    dispatch_configured(&state, ControlCommand::InspectResource { reservation_id })
}

/// 只读 operation → (ControlCommand, CLI 参数)。mutation 一律拒绝。
fn parity_command(
    operation: &str,
    target_hex: Option<&str>,
) -> Result<(ControlCommand, Vec<String>), DesktopError> {
    let target = || -> Result<[u8; 16], DesktopError> {
        let value = target_hex.ok_or_else(|| {
            DesktopError::config("该 operation 需要 32 hex 目标 id(inspect-task/process/resource)")
        })?;
        principal_bytes(value)
    };
    match operation {
        "inspect-health" => Ok((ControlCommand::InspectHealth, vec!["inspect-health".into()])),
        "inspect-semantic-health" => Ok((
            ControlCommand::InspectSemanticHealth,
            vec!["inspect-semantic-health".into()],
        )),
        "export-metrics" => Ok((ControlCommand::ExportMetrics, vec!["export-metrics".into()])),
        "export-semantic-metrics" => Ok((
            ControlCommand::ExportSemanticMetrics,
            vec!["export-semantic-metrics".into()],
        )),
        "inspect-task" => {
            let plan_id = target()?;
            Ok((
                ControlCommand::InspectTask { plan_id },
                vec![
                    "inspect-task".into(),
                    target_hex.unwrap_or_default().to_owned(),
                ],
            ))
        }
        "inspect-process" => {
            let process_id = target()?;
            Ok((
                ControlCommand::InspectProcess { process_id },
                vec![
                    "inspect-process".into(),
                    target_hex.unwrap_or_default().to_owned(),
                ],
            ))
        }
        "inspect-resource" => {
            let reservation_id = target()?;
            Ok((
                ControlCommand::InspectResource { reservation_id },
                vec![
                    "inspect-resource".into(),
                    target_hex.unwrap_or_default().to_owned(),
                ],
            ))
        }
        _ => Err(DesktopError::config(format!(
            "未知或非只读 operation: {operation}(mutation 是 W32-B)"
        ))),
    }
}

/// 一致性自检:同一只读命令,GUI 经认证入口派发一次,真实
/// `system-control-cli` 经 plain socket 派发一次,比对 receipt hex。
#[tauri::command]
pub fn parity_check(
    state: tauri::State<'_, AppState>,
    operation: String,
    target_hex: Option<String>,
) -> Result<ParityDto, DesktopError> {
    let config = state.snapshot()?;
    let (command, cli_args) = parity_command(&operation, target_hex.as_deref())?;
    let (socket, principal, key_file) = required(&config)?;
    let gui =
        tauri::async_runtime::block_on(dispatch_read(&socket, &principal, &key_file, command))?;

    let cli_socket = config.cli_socket.clone().ok_or_else(|| {
        DesktopError::config("一致性自检需要 cli_socket(plain 入口;开发夹具提供)")
    })?;
    let cli_path = config
        .cli_path
        .clone()
        .unwrap_or_else(|| DEFAULT_CLI_PATH.to_owned());
    let output = std::process::Command::new(&cli_path)
        .arg(&cli_socket)
        .args(&cli_args)
        .output()
        .map_err(|error| {
            DesktopError::ipc(format!("启动 system-control-cli({cli_path})失败: {error}"))
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let cli_receipt_hex = stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("RECEIPT "))
        .map(str::to_owned);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(ParityDto {
        operation,
        matched: cli_receipt_hex.as_deref() == Some(gui.receipt_hex.as_str()),
        gui_receipt_hex: gui.receipt_hex,
        cli_receipt_hex,
        cli_exit_code: output.status.code(),
        cli_stderr: (!stderr.trim().is_empty()).then(|| stderr.trim().to_owned()),
    })
}

/// 仅供测试与文档引用:默认 CLI 探测路径。
#[must_use]
pub fn default_cli_path() -> &'static str {
    DEFAULT_CLI_PATH
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn principal_hex_rejects_non_hex_shapes() {
        assert!(principal_bytes("31").is_err());
        assert!(principal_bytes(&"zz".repeat(16)).is_err());
        assert!(principal_bytes(&"31".repeat(16)).is_ok());
    }

    #[test]
    fn key_seed_rejects_malformed_files() {
        let dir = std::env::temp_dir().join(format!("llmosdt-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("seed.key");
        std::fs::write(&path, "xyz").ok();
        assert!(key_seed(path.to_str().unwrap_or_default()).is_err());
        std::fs::write(&path, "31".repeat(32)).ok();
        assert!(key_seed(path.to_str().unwrap_or_default()).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parity_command_rejects_mutations() {
        assert_eq!(
            parity_command("ack-recovery-alert", None).unwrap_err().code,
            ErrorCode::Config
        );
        let (command, args) = parity_command("inspect-health", None).unwrap();
        assert_eq!(command, ControlCommand::InspectHealth);
        assert_eq!(args, vec!["inspect-health".to_owned()]);
    }
}
