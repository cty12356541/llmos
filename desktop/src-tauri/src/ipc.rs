//! 认证 SystemControl IPC 客户端接线(W32-A 只读半 + W32-B 写入半)。
//!
//! 每条命令——inspect 读或授权控制写——都通过
//! [`nlos_system_control::auth::dispatch_over_authenticated_socket`]
//! ——ADR-0011 challenge-response 认证入口——到达真实 SystemControl 服务,
//! 没有 plain socket 捷径。principal 私钥不入仓库:会话配置来自环境变量或
//! GUI 会话内设置,签名密钥始终从 operator 提供的 `0600` 密钥文件在派发时读取。
//!
//! 写入半(W32-B):`submit_control` 把 GUI 的授权动作编译为真实
//! `ControlCommand`(§25.3 idempotency 身份在派发时新生成;CAS 预期由
//! 前端从 inspect 状态带入),经同一认证入口派发并投影 Receipt。
//!
//! 一致性自检(`parity_check`)把同一只读命令再经真实 `system-control-cli`
//! 二进制(plain 入口,本地信任域)派发一次,比对两侧
//! `ControlReceipt::to_bytes` hex——与 B-TASK-006L 已固化的三入口字节一致
//! 契约同源。`parity_check_write` 把同一自检扩展到一条写路径命令
//! (pause-operation 探针);完整写路径 parity 矩阵钉死是 W32-C。

use std::sync::Mutex;

#[cfg(unix)]
use ed25519_dalek::{Signer, SigningKey};
use nlos_application::ApplicationAuthority;
use nlos_resource::ResourceAuthority;
use nlos_system_control::application_inspector::ApplicationAuthorityInspector;
use nlos_system_control::control::{
    ApplicationInspector, CONTROL_CAPABILITY_GENERATION, CONTROL_CAPABILITY_SLOT, ControlCommand,
    ResourceInspector, parse_hex_id,
};
use nlos_system_control::resource_inspector::ResourceAuthorityInspector;
#[cfg(unix)]
use nlos_types::PrincipalId;
use serde::Deserialize;

#[cfg(unix)]
use crate::dto::receipt_dto;
use crate::dto::{
    ConfigDto, ConfigSourceDto, ControlPlaneFactsDto, FactCheckDto, ParityDto, ReceiptDto,
    fact_check_dto,
};
use crate::error::{DesktopError, from_control_error};

/// 环境变量名(README 记录;GUI 内可会话级覆盖)。
pub const ENV_SOCKET: &str = "LLMOS_DESKTOP_SOCKET";
pub const ENV_PRINCIPAL: &str = "LLMOS_DESKTOP_PRINCIPAL";
pub const ENV_KEY_FILE: &str = "LLMOS_DESKTOP_KEY_FILE";
pub const ENV_CLI_SOCKET: &str = "LLMOS_DESKTOP_CLI_SOCKET";
pub const ENV_CLI: &str = "LLMOS_DESKTOP_CLI";
/// W32-D:本地资源权威根目录(预算/成本可见性的 ResourceAuthority 接线)。
pub const ENV_RESOURCE_ROOT: &str = "LLMOS_DESKTOP_RESOURCE_ROOT";
/// W32-F:本地应用权威根目录(UI Surface 呈现的 ApplicationAuthority 接线)。
pub const ENV_APPLICATION_ROOT: &str = "LLMOS_DESKTOP_APPLICATION_ROOT";

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
    pub resource_root: Option<String>,
    pub application_root: Option<String>,
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
            ENV_RESOURCE_ROOT,
            ENV_APPLICATION_ROOT,
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
                resource_root: env(ENV_RESOURCE_ROOT),
                application_root: env(ENV_APPLICATION_ROOT),
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
        resource_root: config.resource_root.clone(),
        application_root: config.application_root.clone(),
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

/// 经 ADR-0011 认证入口派发一条命令(inspect 读与 W32-B 授权写共用)并
/// 投影 Receipt(纯函数核心,由 Tauri 命令与集成测试共用)。
#[cfg(unix)]
pub async fn dispatch_control(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    command: ControlCommand,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_control_with_resource(socket, principal_hex, key_file, command, None).await
}

/// [`dispatch_control`] 的 W32-D 扩展核心:额外接受可选资源 inspector。
/// `InspectResource` 的有界成本事实在回执投影时由客户端侧 inspector 组装
/// (上游 `ControlReceipt::compose` 契约);`None` 保持未接线形态,回执为
/// 类型化 `NOT_FOUND`,与 CLI(同样未接线)字节一致——CLI parity 比对
/// 恒走 [`dispatch_control`],不受本参数影响。
#[cfg(unix)]
pub async fn dispatch_control_with_resource(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    command: ControlCommand,
    resource: Option<&dyn ResourceInspector>,
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
        resource,
        None,
    )
    .await
    .map_err(|error| from_control_error(&error))?;
    Ok(receipt_dto(&receipt))
}

#[cfg(not(unix))]
pub async fn dispatch_control(
    _socket: &str,
    _principal_hex: &str,
    _key_file: &str,
    _command: ControlCommand,
) -> Result<ReceiptDto, DesktopError> {
    Err(DesktopError::unsupported_platform())
}

#[cfg(not(unix))]
pub async fn dispatch_control_with_resource(
    _socket: &str,
    _principal_hex: &str,
    _key_file: &str,
    _command: ControlCommand,
    _resource: Option<&dyn ResourceInspector>,
) -> Result<ReceiptDto, DesktopError> {
    Err(DesktopError::unsupported_platform())
}

/// [`dispatch_control`] 的 Application inspect 扩展:可选
/// [`ApplicationAuthorityInspector`]。`None` 保持
/// [`nlos_system_control::control::UnwiredApplicationInspector`] 的类型化
/// `NOT_FOUND`(`application inspection backend is not wired`)。
#[cfg(unix)]
pub async fn dispatch_control_with_application(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    command: ControlCommand,
    application: Option<&dyn ApplicationInspector>,
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
        application,
    )
    .await
    .map_err(|error| from_control_error(&error))?;
    Ok(receipt_dto(&receipt))
}

#[cfg(not(unix))]
pub async fn dispatch_control_with_application(
    _socket: &str,
    _principal_hex: &str,
    _key_file: &str,
    _command: ControlCommand,
    _application: Option<&dyn ApplicationInspector>,
) -> Result<ReceiptDto, DesktopError> {
    Err(DesktopError::unsupported_platform())
}

/// `InspectApplication` 经认证入口派发。配置了 `application_root` 时以
/// [`ApplicationAuthorityInspector`] 组装应用头事实;未配置时 inspector
/// 为 `None`,回执为既有未接线 `NOT_FOUND`。
pub async fn dispatch_application_inspect(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    authority: Option<&ApplicationAuthority>,
    package_id: [u8; 16],
) -> Result<ReceiptDto, DesktopError> {
    let command = ControlCommand::InspectApplication { package_id };
    match authority {
        Some(authority) => {
            let inspector = ApplicationAuthorityInspector::new(authority);
            dispatch_control_with_application(
                socket,
                principal_hex,
                key_file,
                command,
                Some(&inspector),
            )
            .await
        }
        None => {
            dispatch_control_with_application(socket, principal_hex, key_file, command, None).await
        }
    }
}

fn open_application_authority(
    application_root: Option<&str>,
) -> Result<Option<ApplicationAuthority>, DesktopError> {
    let Some(root) = application_root
        .map(str::trim)
        .filter(|root| !root.is_empty())
    else {
        return Ok(None);
    };
    ApplicationAuthority::open(root)
        .map(Some)
        .map_err(|error| DesktopError::config(format!("打开本地应用权威失败({root}):{error}")))
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
    tauri::async_runtime::block_on(dispatch_control(&socket, &principal, &key_file, command))
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
    pub resource_root: Option<String>,
    pub application_root: Option<String>,
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
        resource_root: cleaned(&input.resource_root),
        application_root: cleaned(&input.application_root),
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

/// 资源域恢复巡检(W32-B 补接线:G8 资源恢复动作的 CAS 预期来源)。
#[tauri::command]
pub fn inspect_resource_health(
    state: tauri::State<'_, AppState>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::InspectResourceHealth)
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

/// W32-E 资源监控:resource 域(G8)指标导出——既有只读命令
/// `ControlCommand::ExportResourceMetrics` 的 GUI 接线(W32-A 只接了
/// artifact/semantic 两域)。与另外两条导出命令同路经认证入口,回执携带
/// OpenMetrics 文本;无任何新控制路径。
#[tauri::command]
pub fn export_resource_metrics(
    state: tauri::State<'_, AppState>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::ExportResourceMetrics)
}

#[tauri::command]
pub fn inspect_task(
    state: tauri::State<'_, AppState>,
    plan_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let plan_id = principal_bytes(&plan_id_hex)?;
    dispatch_configured(&state, ControlCommand::InspectTask { plan_id })
}

/// W39-D / §28.4:Task Space 五层 inspect 命令构造器(hex → ControlCommand)。
/// Tauri 命令与集成测试共用;generation 必须非零(与上游
/// `validate_layer_generation` 同纪律,在派发前于 GUI 壳拒绝)。
pub fn layer_inspect_task_group(group_id_hex: &str) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::InspectTaskGroup {
        group_id: principal_bytes(group_id_hex)?,
    })
}

pub fn layer_inspect_task_node(
    plan_id_hex: &str,
    node_id_hex: &str,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::InspectTaskNode {
        plan_id: principal_bytes(plan_id_hex)?,
        node_id: principal_bytes(node_id_hex)?,
    })
}

pub fn layer_inspect_execution_fiber(
    fiber_id_hex: &str,
    generation: u64,
) -> Result<ControlCommand, DesktopError> {
    reject_zero_generation(generation)?;
    Ok(ControlCommand::InspectExecutionFiber {
        fiber_id: principal_bytes(fiber_id_hex)?,
        generation,
    })
}

pub fn layer_inspect_topic(topic_id_hex: &str) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::InspectTopic {
        topic_id: principal_bytes(topic_id_hex)?,
    })
}

pub fn layer_inspect_operation(
    operation_id_hex: &str,
    generation: u64,
) -> Result<ControlCommand, DesktopError> {
    reject_zero_generation(generation)?;
    Ok(ControlCommand::InspectOperation {
        operation_id: principal_bytes(operation_id_hex)?,
        generation,
    })
}

fn reject_zero_generation(generation: u64) -> Result<(), DesktopError> {
    if generation == 0 {
        Err(DesktopError::config(
            "handle generation must be a non-zero generation",
        ))
    } else {
        Ok(())
    }
}

/// W39-D:InspectTaskGroup 的 GUI 接线(既有只读 ControlCommand)。
#[tauri::command]
pub fn inspect_task_group(
    state: tauri::State<'_, AppState>,
    group_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, layer_inspect_task_group(&group_id_hex)?)
}

/// W39-D:InspectTaskNode 的 GUI 接线。
#[tauri::command]
pub fn inspect_task_node(
    state: tauri::State<'_, AppState>,
    plan_id_hex: String,
    node_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, layer_inspect_task_node(&plan_id_hex, &node_id_hex)?)
}

/// W39-D:InspectExecutionFiber 的 GUI 接线。
#[tauri::command]
pub fn inspect_execution_fiber(
    state: tauri::State<'_, AppState>,
    fiber_id_hex: String,
    generation: u64,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(
        &state,
        layer_inspect_execution_fiber(&fiber_id_hex, generation)?,
    )
}

/// W39-D:InspectTopic 的 GUI 接线。
#[tauri::command]
pub fn inspect_topic(
    state: tauri::State<'_, AppState>,
    topic_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, layer_inspect_topic(&topic_id_hex)?)
}

/// W39-D:InspectOperation 的 GUI 接线。
#[tauri::command]
pub fn inspect_operation(
    state: tauri::State<'_, AppState>,
    operation_id_hex: String,
    generation: u64,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(
        &state,
        layer_inspect_operation(&operation_id_hex, generation)?,
    )
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

/// W32-D:打开会话配置指向的本地资源权威(未配置/空白 → `None`,保持
/// 未接线形态)。每次派发即时打开(WAL 多进程读安全),进程内不缓存句柄。
fn open_resource_authority(
    resource_root: Option<&str>,
) -> Result<Option<ResourceAuthority>, DesktopError> {
    let Some(root) = resource_root.map(str::trim).filter(|root| !root.is_empty()) else {
        return Ok(None);
    };
    ResourceAuthority::open(root)
        .map(Some)
        .map_err(|error| DesktopError::config(format!("打开本地资源权威失败({root}): {error}")))
}

/// W32-D 预算/成本查询核心:InspectResource 经认证入口派发;配置了
/// resource_root 时以真实 `ResourceAuthorityInspector` 组装有界成本事实,
/// 未配置时 inspector 传 `None`,回执为诚实的类型化 `NOT_FOUND`。
pub async fn dispatch_cost_inspect(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    authority: Option<&ResourceAuthority>,
    reservation_id: [u8; 16],
) -> Result<ReceiptDto, DesktopError> {
    let command = ControlCommand::InspectResource { reservation_id };
    match authority {
        Some(authority) => {
            let inspector = ResourceAuthorityInspector::new(authority);
            dispatch_control_with_resource(
                socket,
                principal_hex,
                key_file,
                command,
                Some(&inspector),
            )
            .await
        }
        None => {
            dispatch_control_with_resource(socket, principal_hex, key_file, command, None).await
        }
    }
}

/// W32-D 预算/成本可见性命令:「权限/预算」视图的资源成本查询入口。
#[tauri::command]
pub fn inspect_resource_cost(
    state: tauri::State<'_, AppState>,
    reservation_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let config = state.snapshot()?;
    let reservation_id = principal_bytes(&reservation_id_hex)?;
    let (socket, principal, key_file) = required(&config)?;
    let authority = open_resource_authority(config.resource_root.as_deref())?;
    tauri::async_runtime::block_on(dispatch_cost_inspect(
        &socket,
        &principal,
        &key_file,
        authority.as_ref(),
        reservation_id,
    ))
}

/// W32-D 一致性自检(渲染事实 vs 直接复检):同一 reservation 两次独立
/// 认证派发 InspectResource,逐字段比较有界成本事实并比对 receipt hex。
/// 已结清(FINALIZED)预留的事实不可变,两次必须一致;未接线形态两侧
/// 同为类型化 NOT_FOUND,字节同样可比。
#[tauri::command]
pub fn cost_fact_check(
    state: tauri::State<'_, AppState>,
    reservation_id_hex: String,
) -> Result<FactCheckDto, DesktopError> {
    let config = state.snapshot()?;
    let reservation_id = principal_bytes(&reservation_id_hex)?;
    let (socket, principal, key_file) = required(&config)?;
    let authority = open_resource_authority(config.resource_root.as_deref())?;
    let first = tauri::async_runtime::block_on(dispatch_cost_inspect(
        &socket,
        &principal,
        &key_file,
        authority.as_ref(),
        reservation_id,
    ))?;
    let second = tauri::async_runtime::block_on(dispatch_cost_inspect(
        &socket,
        &principal,
        &key_file,
        authority.as_ref(),
        reservation_id,
    ))?;
    Ok(fact_check_dto(
        &reservation_id_hex.trim().to_lowercase(),
        &first,
        &second,
    ))
}

/// `ControlCommand::InspectApplication` 的 GUI 接线。未配置
/// `application_root` 时走既有未接线 inspector,回执为类型化 `NOT_FOUND`。
#[tauri::command]
pub fn inspect_application(
    state: tauri::State<'_, AppState>,
    package_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let config = state.snapshot()?;
    let package_id = principal_bytes(&package_id_hex)?;
    let (socket, principal, key_file) = required(&config)?;
    let authority = open_application_authority(config.application_root.as_deref())?;
    tauri::async_runtime::block_on(dispatch_application_inspect(
        &socket,
        &principal,
        &key_file,
        authority.as_ref(),
        package_id,
    ))
}

/// W32-F 表面呈现命令:按包身份读回应用声明的可呈现表面(本地应用
/// 权威直读视图,与权限/预算视图的 cost 查询同机制,非 CLI parity 面)。
#[tauri::command]
pub fn present_surfaces(
    state: tauri::State<'_, AppState>,
    package_id_hex: String,
) -> Result<crate::surfaces::SurfacesPresentationDto, DesktopError> {
    let config = state.snapshot()?;
    let package_id = nlos_types::PackageId::from_bytes(principal_bytes(&package_id_hex)?);
    let authority =
        crate::surfaces::open_configured_application_authority(config.application_root.as_deref())?;
    crate::surfaces::present_surfaces_core(&authority, package_id)
}

/// W32-D 控制面授权事实(客户端路径常量,非 inspect 数据):每条派发
/// 携带的固定控制能力句柄与服务名。逐 principal 的能力签发/衰减/撤销
/// 账本无 IPC inspect 面,由「权限/预算」视图的缺口登记列出。
#[tauri::command]
pub fn control_plane_facts() -> ControlPlaneFactsDto {
    ControlPlaneFactsDto {
        service: nlos_system_control::SYSTEM_CONTROL_SERVICE.to_owned(),
        capability_slot: CONTROL_CAPABILITY_SLOT,
        capability_generation: CONTROL_CAPABILITY_GENERATION,
    }
}

/// §25.3 idempotency 身份:每次提交新生成 16 字节(/dev/urandom;认证入口
/// 仅 Unix,非 Unix 面在派发前就以类型化 UNSUPPORTED_PLATFORM 拒绝)。
#[cfg(unix)]
fn fresh_command_id() -> Result<[u8; 16], DesktopError> {
    use std::io::Read;

    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|error| DesktopError::internal(format!("打开 /dev/urandom 失败: {error}")))?;
    let mut id = [0u8; 16];
    file.read_exact(&mut id)
        .map_err(|error| DesktopError::internal(format!("读取命令 id 失败: {error}")))?;
    Ok(id)
}

#[cfg(not(unix))]
fn fresh_command_id() -> Result<[u8; 16], DesktopError> {
    Err(DesktopError::unsupported_platform())
}

/// GUI 授权动作(SABI v1.4 命令面;serde tag 与 CLI operation 名一致)。
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ControlAction {
    AckRecoveryAlert {
        plan_id_hex: String,
        expected_total_failures: u64,
        reason: String,
    },
    AckSemanticRecoveryAlert {
        plan_id_hex: String,
        expected_total_failures: u64,
        reason: String,
    },
    ResumeSemanticRecovery {
        plan_id_hex: String,
        expected_total_failures: u64,
        reason: String,
    },
    AckResourceRecoveryAlert {
        plan_id_hex: String,
        expected_total_failures: u64,
        reason: String,
    },
    ResumeResourceRecovery {
        plan_id_hex: String,
        expected_total_failures: u64,
        reason: String,
    },
    PauseOperation {
        target_id_hex: String,
        expected_revision: u64,
        reason: String,
    },
    ResumeOperation {
        target_id_hex: String,
        expected_revision: u64,
        reason: String,
    },
    CancelOperation {
        target_id_hex: String,
        expected_revision: u64,
        reason: String,
    },
    KillOperation {
        target_id_hex: String,
        expected_revision: u64,
        reason: String,
    },
    ThrottleOperation {
        target_id_hex: String,
        expected_revision: u64,
        throttle_percent: u64,
        reason: String,
    },
    ReclaimOperation {
        target_id_hex: String,
        expected_revision: u64,
        reason: String,
    },
}

/// 动作 → [`ControlCommand`] 的单一编译点(派发前类型化校验:32 hex 目标、
/// 非空 reason;throttle 百分比镜像上游 1..=100 wire 前拒绝规则)。
/// Tauri `submit_control` 命令与集成测试共用(测试注入确定性命令 id)。
pub fn build_control_command(
    action: ControlAction,
    control_command_id: [u8; 16],
) -> Result<ControlCommand, DesktopError> {
    let reason = |raw: &str| -> Result<String, DesktopError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            Err(DesktopError::config(
                "控制动作需要非空 reason(有界操作理由)",
            ))
        } else {
            Ok(trimmed.to_owned())
        }
    };
    match action {
        ControlAction::AckRecoveryAlert {
            plan_id_hex,
            expected_total_failures,
            reason: raw,
        } => Ok(ControlCommand::AcknowledgeRecoveryAlert {
            control_command_id,
            plan_id: principal_bytes(&plan_id_hex)?,
            expected_total_failures,
            reason: reason(&raw)?,
        }),
        ControlAction::AckSemanticRecoveryAlert {
            plan_id_hex,
            expected_total_failures,
            reason: raw,
        } => Ok(ControlCommand::AcknowledgeSemanticRecoveryAlert {
            control_command_id,
            plan_id: principal_bytes(&plan_id_hex)?,
            expected_total_failures,
            reason: reason(&raw)?,
        }),
        ControlAction::ResumeSemanticRecovery {
            plan_id_hex,
            expected_total_failures,
            reason: raw,
        } => Ok(ControlCommand::ResumeSemanticRecovery {
            control_command_id,
            plan_id: principal_bytes(&plan_id_hex)?,
            expected_total_failures,
            reason: reason(&raw)?,
        }),
        ControlAction::AckResourceRecoveryAlert {
            plan_id_hex,
            expected_total_failures,
            reason: raw,
        } => Ok(ControlCommand::AcknowledgeResourceRecoveryAlert {
            control_command_id,
            plan_id: principal_bytes(&plan_id_hex)?,
            expected_total_failures,
            reason: reason(&raw)?,
        }),
        ControlAction::ResumeResourceRecovery {
            plan_id_hex,
            expected_total_failures,
            reason: raw,
        } => Ok(ControlCommand::ResumeResourceRecovery {
            control_command_id,
            plan_id: principal_bytes(&plan_id_hex)?,
            expected_total_failures,
            reason: reason(&raw)?,
        }),
        ControlAction::PauseOperation {
            target_id_hex,
            expected_revision,
            reason: raw,
        } => Ok(ControlCommand::PauseOperation {
            control_command_id,
            target_id: principal_bytes(&target_id_hex)?,
            expected_generation_or_revision: expected_revision,
            reason: reason(&raw)?,
        }),
        ControlAction::ResumeOperation {
            target_id_hex,
            expected_revision,
            reason: raw,
        } => Ok(ControlCommand::ResumeOperation {
            control_command_id,
            target_id: principal_bytes(&target_id_hex)?,
            expected_generation_or_revision: expected_revision,
            reason: reason(&raw)?,
        }),
        ControlAction::CancelOperation {
            target_id_hex,
            expected_revision,
            reason: raw,
        } => Ok(ControlCommand::CancelOperation {
            control_command_id,
            target_id: principal_bytes(&target_id_hex)?,
            expected_generation_or_revision: expected_revision,
            reason: reason(&raw)?,
        }),
        ControlAction::KillOperation {
            target_id_hex,
            expected_revision,
            reason: raw,
        } => Ok(ControlCommand::KillOperation {
            control_command_id,
            target_id: principal_bytes(&target_id_hex)?,
            expected_generation_or_revision: expected_revision,
            reason: reason(&raw)?,
        }),
        ControlAction::ThrottleOperation {
            target_id_hex,
            expected_revision,
            throttle_percent,
            reason: raw,
        } => {
            if !(1..=100).contains(&throttle_percent) {
                return Err(DesktopError::config(
                    "throttle_percent 必须是 1..=100 的整数百分比(与上游 wire 前拒绝同规)",
                ));
            }
            Ok(ControlCommand::ThrottleOperation {
                control_command_id,
                target_id: principal_bytes(&target_id_hex)?,
                expected_generation_or_revision: expected_revision,
                throttle_percent,
                reason: reason(&raw)?,
            })
        }
        ControlAction::ReclaimOperation {
            target_id_hex,
            expected_revision,
            reason: raw,
        } => Ok(ControlCommand::ReclaimOperation {
            control_command_id,
            target_id: principal_bytes(&target_id_hex)?,
            expected_generation_or_revision: expected_revision,
            reason: reason(&raw)?,
        }),
    }
}

/// W32-B 写入半唯一入口:授权动作 → 真实 ControlCommand → 认证 IPC。
/// 回执(含类型化失败)完整返回前端渲染,后端不改写失败。
#[tauri::command]
pub fn submit_control(
    state: tauri::State<'_, AppState>,
    action: ControlAction,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_control_command(action, fresh_command_id()?)?;
    dispatch_configured(&state, command)
}

/// 只读 operation → (ControlCommand, CLI 参数)。mutation 一律拒绝。
fn parity_command(
    operation: &str,
    target_hex: Option<&str>,
) -> Result<(ControlCommand, Vec<String>), DesktopError> {
    let target = || -> Result<[u8; 16], DesktopError> {
        let value = target_hex.ok_or_else(|| {
            DesktopError::config(
                "该 operation 需要 32 hex 目标 id(inspect-task/process/resource/application)",
            )
        })?;
        principal_bytes(value)
    };
    match operation {
        "inspect-health" => Ok((ControlCommand::InspectHealth, vec!["inspect-health".into()])),
        "inspect-semantic-health" => Ok((
            ControlCommand::InspectSemanticHealth,
            vec!["inspect-semantic-health".into()],
        )),
        "inspect-resource-health" => Ok((
            ControlCommand::InspectResourceHealth,
            vec!["inspect-resource-health".into()],
        )),
        "export-metrics" => Ok((ControlCommand::ExportMetrics, vec!["export-metrics".into()])),
        "export-semantic-metrics" => Ok((
            ControlCommand::ExportSemanticMetrics,
            vec!["export-semantic-metrics".into()],
        )),
        "export-resource-metrics" => Ok((
            ControlCommand::ExportResourceMetrics,
            vec!["export-resource-metrics".into()],
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
        "inspect-application" => {
            let package_id = target()?;
            Ok((
                ControlCommand::InspectApplication { package_id },
                vec![
                    "inspect-application".into(),
                    target_hex.unwrap_or_default().to_owned(),
                ],
            ))
        }
        _ => Err(DesktopError::config(format!(
            "未知或非只读 operation: {operation}(读路径自检只收 inspect/export;写路径探针见 parity_check_write)"
        ))),
    }
}

/// 一致性自检(读路径):同一只读命令,GUI 经认证入口派发一次,真实
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
        tauri::async_runtime::block_on(dispatch_control(&socket, &principal, &key_file, command))?;
    let cli = run_cli(&config, &cli_args)?;
    Ok(ParityDto {
        operation,
        matched: cli.receipt_hex.as_deref() == Some(gui.receipt_hex.as_str()),
        gui_receipt_hex: gui.receipt_hex,
        cli_receipt_hex: cli.receipt_hex,
        cli_exit_code: cli.exit_code,
        cli_stderr: cli.stderr,
    })
}

/// 一次真实 `system-control-cli` 子进程运行的首行 `RECEIPT <hex>` 投影。
struct CliRun {
    receipt_hex: Option<String>,
    exit_code: Option<i32>,
    stderr: Option<String>,
}

fn run_cli(config: &SessionConfig, cli_args: &[String]) -> Result<CliRun, DesktopError> {
    let cli_socket = config.cli_socket.clone().ok_or_else(|| {
        DesktopError::config("一致性自检需要 cli_socket(plain 入口;开发夹具提供)")
    })?;
    let cli_path = config
        .cli_path
        .clone()
        .unwrap_or_else(|| DEFAULT_CLI_PATH.to_owned());
    let output = std::process::Command::new(&cli_path)
        .arg(&cli_socket)
        .args(cli_args)
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
    Ok(CliRun {
        receipt_hex: cli_receipt_hex,
        exit_code: output.status.code(),
        stderr: (!stderr.trim().is_empty()).then(|| stderr.trim().to_owned()),
    })
}

/// W32-B 写路径一致性探针:同一条 pause-operation 命令(同 command id、
/// 目标、CAS、reason——两侧字节同一),GUI 经认证入口、真实 CLI 经 plain
/// 入口各派发一次,比对 receipt hex。开发夹具未接线操作执行器时两侧同为
/// 确定性的类型化 `NOT_FOUND`(executor 未接线)失败回执,字节可比;在
/// 已接线执行器的宿主上,第一次派发可能真实暂停目标、第二次按 idempotency/
/// CAS 纪律回 CONFLICT——matched=false 即如实显示。完整写路径 parity 矩阵
/// (成功/NotFound/Rights 三形态)钉死属 W32-C。
#[tauri::command]
pub fn parity_check_write(
    state: tauri::State<'_, AppState>,
    command_id_hex: String,
    target_hex: String,
    expected_revision: u64,
    reason: String,
) -> Result<ParityDto, DesktopError> {
    let config = state.snapshot()?;
    let trimmed_reason = reason.trim();
    if trimmed_reason.is_empty() {
        return Err(DesktopError::config("写路径自检需要非空 reason"));
    }
    let command = ControlCommand::PauseOperation {
        control_command_id: principal_bytes(&command_id_hex)?,
        target_id: principal_bytes(&target_hex)?,
        expected_generation_or_revision: expected_revision,
        reason: trimmed_reason.to_owned(),
    };
    let (socket, principal, key_file) = required(&config)?;
    let gui =
        tauri::async_runtime::block_on(dispatch_control(&socket, &principal, &key_file, command))?;
    let cli_args = vec![
        "pause-operation".to_owned(),
        command_id_hex.trim().to_owned(),
        target_hex.trim().to_owned(),
        expected_revision.to_string(),
        trimmed_reason.to_owned(),
    ];
    let cli = run_cli(&config, &cli_args)?;
    Ok(ParityDto {
        operation: "pause-operation".to_owned(),
        matched: cli.receipt_hex.as_deref() == Some(gui.receipt_hex.as_str()),
        gui_receipt_hex: gui.receipt_hex,
        cli_receipt_hex: cli.receipt_hex,
        cli_exit_code: cli.exit_code,
        cli_stderr: cli.stderr,
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

    #[test]
    fn parity_command_covers_resource_domain_reads() {
        let (command, args) = parity_command("inspect-resource-health", None).unwrap();
        assert_eq!(command, ControlCommand::InspectResourceHealth);
        assert_eq!(args, vec!["inspect-resource-health".to_owned()]);
        let (command, args) = parity_command("export-resource-metrics", None).unwrap();
        assert_eq!(command, ControlCommand::ExportResourceMetrics);
        assert_eq!(args, vec!["export-resource-metrics".to_owned()]);
    }

    #[test]
    fn layer_inspect_builders_reject_malformed_hex_and_zero_generation() {
        assert!(layer_inspect_task_group("31").is_err());
        assert!(layer_inspect_task_node(&"31".repeat(16), &"zz".repeat(16)).is_err());
        assert_eq!(
            layer_inspect_execution_fiber(&"31".repeat(16), 0)
                .unwrap_err()
                .code,
            ErrorCode::Config
        );
        assert_eq!(
            layer_inspect_operation(&"31".repeat(16), 0)
                .unwrap_err()
                .code,
            ErrorCode::Config
        );
        let fiber = layer_inspect_execution_fiber(&"b1".repeat(16), 2).unwrap();
        assert_eq!(
            fiber,
            ControlCommand::InspectExecutionFiber {
                fiber_id: [0xb1; 16],
                generation: 2,
            }
        );
    }

    fn control_action(action: ControlAction) -> Result<ControlCommand, DesktopError> {
        build_control_command(action, [0xC9; 16])
    }

    #[test]
    fn build_control_command_rejects_empty_reason_and_bad_inputs() {
        let ack = ControlAction::AckRecoveryAlert {
            plan_id_hex: "31".repeat(16),
            expected_total_failures: 1,
            reason: "   ".to_owned(),
        };
        assert_eq!(control_action(ack).unwrap_err().code, ErrorCode::Config);

        let bad_target = ControlAction::KillOperation {
            target_id_hex: "zz".repeat(16),
            expected_revision: 3,
            reason: "operator kill".to_owned(),
        };
        assert_eq!(
            control_action(bad_target).unwrap_err().code,
            ErrorCode::Config
        );

        for percent in [0u64, 101] {
            let throttle = ControlAction::ThrottleOperation {
                target_id_hex: "31".repeat(16),
                expected_revision: 7,
                throttle_percent: percent,
                reason: "throttle".to_owned(),
            };
            assert_eq!(
                control_action(throttle).unwrap_err().code,
                ErrorCode::Config,
                "percent {percent} must be rejected before the wire"
            );
        }
    }

    #[test]
    fn build_control_command_trims_reason_and_binds_command_identity() {
        let command = control_action(ControlAction::PauseOperation {
            target_id_hex: "31".repeat(16),
            expected_revision: 5,
            reason: "  pause for maintenance  ".to_owned(),
        })
        .unwrap();
        let ControlCommand::PauseOperation {
            control_command_id,
            target_id,
            expected_generation_or_revision,
            reason,
        } = command
        else {
            panic!("expected pause command");
        };
        assert_eq!(control_command_id, [0xC9; 16]);
        assert_eq!(target_id, [0x31; 16]);
        assert_eq!(expected_generation_or_revision, 5);
        assert_eq!(reason, "pause for maintenance");

        let throttle = control_action(ControlAction::ThrottleOperation {
            target_id_hex: "41".repeat(16),
            expected_revision: 2,
            throttle_percent: 50,
            reason: "cap demand".to_owned(),
        })
        .unwrap();
        let ControlCommand::ThrottleOperation {
            throttle_percent, ..
        } = throttle
        else {
            panic!("expected throttle command");
        };
        assert_eq!(throttle_percent, 50);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_command_id_is_unique_per_call() {
        let first = fresh_command_id().unwrap();
        let second = fresh_command_id().unwrap();
        assert_ne!(first, second);
    }
}
