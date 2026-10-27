//! 认证 SystemControl IPC 客户端接线(W32-A 只读半 + W32-B 写入半 + Stage D 十二域枚举与对称控制)。
//!
//! 每条命令——inspect 读、Stage D 有界分页枚举或授权控制写——都通过
//! [`nlos_system_control::auth::dispatch_over_authenticated_socket_with_services`]
//! ——ADR-0011 challenge-response 认证入口——到达真实 SystemControl 服务,
//! 没有 plain socket 捷径。端点路径按平台解释:Unix 下为 Unix socket
//! 路径,Windows 下为命名管道路径,字符串自会话配置透传。principal 私钥
//! 不入仓库:会话配置来自环境变量或 GUI 会话内设置,签名密钥始终从
//! operator 提供的密钥文件(64 hex Ed25519 种子;Unix 侧 0600 权限位)
//! 在派发时读取。
//!
//! 写入半(W32-B + Stage D `IPC-D8`):`submit_control` 把 GUI 的授权动作编译为真实
//! `ControlCommand`(§25.3 idempotency 身份在派发时新生成;CAS 预期由
//! 前端从 inspect/list 状态带入),经同一认证入口派发并投影 Receipt。
//!
//! 一致性自检(`parity_check`)把同一只读命令再经真实 `system-control-cli`
//! 二进制(plain 入口,本地信任域)派发一次,比对两侧
//! `ControlReceipt::to_bytes` hex——与 B-TASK-006L 已固化的三入口字节一致
//! 契约同源。`parity_check_write` 把同一自检扩展到一条写路径命令
//! (pause-operation 探针);完整写路径 parity 矩阵钉死是 W32-C。

use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::ApplicationAuthority;
use nlos_resource::ResourceAuthority;
use nlos_schema::sabi::v1::{ControlScope, SettingsDomainKind};
use nlos_schema::{
    MAX_DESKTOP_WATERMARK_EVENT_LIMIT, MAX_SYSTEM_CONTROL_ENUM_LIMIT,
    SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
};
use nlos_system_control::application_inspector::ApplicationAuthorityInspector;
use nlos_system_control::control::{
    ApplicationInspector, CONTROL_CAPABILITY_GENERATION, CONTROL_CAPABILITY_SLOT, ControlCommand,
    NotificationInspector, ProcessInspector, ResourceInspector, ScopedControlAction,
    SearchInspector, parse_hex_32, parse_hex_id,
};
use nlos_system_control::desktop_enum::InMemoryDesktopControlSurface;
use nlos_system_control::nl::{
    parse_control_scope, parse_plan_node_state, parse_preview_action_code,
    parse_settings_domain_kind, parse_undo_class, parse_unit_scoped_action,
};
use nlos_system_control::resource_inspector::ResourceAuthorityInspector;
use nlos_types::PrincipalId;
use serde::Deserialize;

use crate::dto::receipt_dto;
use crate::dto::{
    ConfigDto, ConfigSourceDto, ControlPlaneFactsDto, FactCheckDto, OutcomeDto, ParityDto,
    ReceiptDto, fact_check_dto,
};
use crate::error::{DesktopError, ErrorCode, from_control_error};

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

/// Settings CBOR value payload upper bound enforced before the wire.
const MAX_SETTINGS_CBOR_BYTES: usize = 4096;

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

/// Tauri 管理态:持有一份会话配置及可选的桌面只读组合 inspector 面。
pub struct AppState {
    config: Mutex<SessionConfig>,
    desktop_surface: Option<Arc<InMemoryDesktopControlSurface>>,
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
            desktop_surface: None,
        }
    }

    /// 供集成测试显式注入会话配置(无环境变量竞态)。
    #[must_use]
    pub fn from_session(config: SessionConfig) -> Self {
        Self {
            config: Mutex::new(config),
            desktop_surface: None,
        }
    }

    /// 供集成测试与开发夹具接线客户端侧五域只读 inspector
    /// (`Process`/`Resource`/`Application`/`Search`/`Notification`)。
    #[must_use]
    pub fn with_desktop_surface(
        config: SessionConfig,
        desktop_surface: Arc<InMemoryDesktopControlSurface>,
    ) -> Self {
        Self {
            config: Mutex::new(config),
            desktop_surface: Some(desktop_surface),
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
        // 认证派发核心已跨平台(Unix socket / Windows 命名管道)。
        platform_supported: cfg!(any(unix, windows)),
    }
}

/// 32 hex 字符 → 16 字节(复用控制面 fail-closed 解析器,不做第二套规则)。
fn principal_bytes(principal_hex: &str) -> Result<[u8; 16], DesktopError> {
    parse_hex_id(principal_hex.trim()).map_err(|error| from_control_error(&error))
}

/// 64 hex 字符 → 32 字节(Search scope_id 等 32 字节身份解析)。
fn scope_32_bytes(scope_hex: &str) -> Result<[u8; 32], DesktopError> {
    parse_hex_32(scope_hex.trim()).map_err(|error| from_control_error(&error))
}

fn optional_id_bytes(raw: Option<&str>) -> Result<Option<[u8; 16]>, DesktopError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(hex) => principal_bytes(hex).map(Some),
        None => Ok(None),
    }
}

fn decode_hex_bytes(raw: &str, max_bytes: usize) -> Result<Vec<u8>, DesktopError> {
    let trimmed = raw.trim();
    if !trimmed.len().is_multiple_of(2) || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DesktopError::config(
            "十六进制负载必须是偶数个合法 ASCII hex 字符",
        ));
    }
    let byte_len = trimmed.len() / 2;
    if byte_len > max_bytes {
        return Err(DesktopError::config(format!(
            "十六进制负载超过 {max_bytes} 字节上限"
        )));
    }
    let mut out = Vec::with_capacity(byte_len);
    let raw_bytes = trimmed.as_bytes();
    for idx in 0..byte_len {
        let hi = (raw_bytes[2 * idx] as char)
            .to_digit(16)
            .ok_or_else(|| DesktopError::config("非法 hex 字符"))?;
        let lo = (raw_bytes[2 * idx + 1] as char)
            .to_digit(16)
            .ok_or_else(|| DesktopError::config("非法 hex 字符"))?;
        let byte = u8::try_from(hi * 16 + lo).map_err(|_| DesktopError::config("非法 hex 字节"))?;
        out.push(byte);
    }
    Ok(out)
}

fn validate_enum_limit_bound(limit: u32) -> Result<u32, DesktopError> {
    if limit == 0 || usize::try_from(limit).unwrap_or(usize::MAX) > MAX_SYSTEM_CONTROL_ENUM_LIMIT {
        Err(DesktopError::config(
            "enumeration limit must be between 1 and 64",
        ))
    } else {
        Ok(limit)
    }
}

fn validate_watermark_limit_bound(limit: u32) -> Result<u32, DesktopError> {
    if limit == 0
        || usize::try_from(limit).unwrap_or(usize::MAX) > MAX_DESKTOP_WATERMARK_EVENT_LIMIT
    {
        Err(DesktopError::config(
            "watermark event limit must be between 1 and 128",
        ))
    } else {
        Ok(limit)
    }
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
/// 投影 Receipt(纯函数核心,由 Tauri 命令与集成测试共用)。端点路径按
/// 平台解释——Unix 下为 Unix socket 路径,Windows 下为命名管道路径,
/// 字符串透传给跨平台认证传输(nlos-ipc handshake transport)。
pub async fn dispatch_control(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    command: ControlCommand,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_control_with_services(
        socket,
        principal_hex,
        key_file,
        command,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

/// [`dispatch_control`] 的全量五域客户端 inspector 扩展核心
/// (`Process` / `Resource` / `Application` / `Search` / `Notification`)。
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_control_with_services(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    command: ControlCommand,
    process: Option<&dyn ProcessInspector>,
    resource: Option<&dyn ResourceInspector>,
    application: Option<&dyn ApplicationInspector>,
    search: Option<&dyn SearchInspector>,
    notify: Option<&dyn NotificationInspector>,
) -> Result<ReceiptDto, DesktopError> {
    use nlos_system_control::auth::dispatch_over_authenticated_socket_with_services;

    let principal = PrincipalId::from_bytes(principal_bytes(principal_hex)?);
    let seed = key_seed(key_file)?;
    let key = SigningKey::from_bytes(&seed);
    let receipt = dispatch_over_authenticated_socket_with_services(
        socket,
        principal,
        |digest| Ok(key.sign(digest).to_bytes()),
        &command,
        process,
        resource,
        application,
        search,
        notify,
    )
    .await
    .map_err(|error| from_control_error(&error))?;
    Ok(receipt_dto(&receipt))
}

/// [`dispatch_control`] 的 W32-D 扩展核心:额外接受可选资源 inspector。
/// `InspectResource` 的有界成本事实在回执投影时由客户端侧 inspector 组装
/// (上游 `ControlReceipt::compose` 契约);`None` 保持未接线形态,回执为
/// 类型化 `NOT_FOUND`,与 CLI(同样未接线)字节一致——CLI parity 比对
/// 恒走 [`dispatch_control`],不受本参数影响。
pub async fn dispatch_control_with_resource(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    command: ControlCommand,
    resource: Option<&dyn ResourceInspector>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_control_with_services(
        socket,
        principal_hex,
        key_file,
        command,
        None,
        resource,
        None,
        None,
        None,
    )
    .await
}

/// [`dispatch_control`] 的 Application inspect 扩展:可选
/// [`ApplicationAuthorityInspector`]。`None` 保持
/// [`nlos_system_control::control::UnwiredApplicationInspector`] 的类型化
/// `NOT_FOUND`(`application inspection backend is not wired`)。
pub async fn dispatch_control_with_application(
    socket: &str,
    principal_hex: &str,
    key_file: &str,
    command: ControlCommand,
    application: Option<&dyn ApplicationInspector>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_control_with_services(
        socket,
        principal_hex,
        key_file,
        command,
        None,
        None,
        application,
        None,
        None,
    )
    .await
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

/// 把一次阻塞派发移出调用线程:H10(深审计 42 D1)。macOS 上 WKWebView
/// 的脚本消息回调发生在**主线程**,同步 `#[tauri::command]` 内联
/// `block_on` 会把 UI 线程按传输超时预算(connect/read/write 各 5s,
/// 叠加 challenge-response 多轮)整体冻结。改法:命令一律 `async fn`,
/// 非 `Send` 的派发 future(`dispatch_over_authenticated_socket` 持有
/// 上游 `&dyn ProcessInspector/&dyn ResourceInspector` 参数,非 `Sync`,
/// 不能直接作 Tauri async 命令的 `Send` future)封装进
/// [`tauri::async_runtime::spawn_blocking`] 闭包,在 runtime 的阻塞
/// 线程池上 `block_on` 驱动完成——等待发生在阻塞线程,主线程立即返回。
async fn dispatch_off_main<F, R>(work: F) -> Result<R, DesktopError>
where
    F: FnOnce() -> Result<R, DesktopError> + Send + 'static,
    R: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|_| DesktopError::internal("认证派发阻塞线程 join 失败"))?
}

/// 面向 [`AppState`] 的无 Tauri 句柄认证派发核心(Tauri 命令与集成测试共用)。
pub async fn dispatch_app_state(
    state: &AppState,
    command: ControlCommand,
) -> Result<ReceiptDto, DesktopError> {
    let config = state.snapshot()?;
    let (socket, principal, key_file) = required(&config)?;
    let surface = state.desktop_surface.clone();
    dispatch_off_main(move || {
        tauri::async_runtime::block_on(async move {
            match surface.as_deref() {
                Some(surface) => {
                    dispatch_control_with_services(
                        &socket,
                        &principal,
                        &key_file,
                        command,
                        Some(surface),
                        Some(surface),
                        Some(surface),
                        Some(surface),
                        Some(surface),
                    )
                    .await
                }
                None => dispatch_control(&socket, &principal, &key_file, command).await,
            }
        })
    })
    .await
}

/// 会话配置快照 + 必需凭据,随后在阻塞线程池上执行一次认证 dispatch。
async fn dispatch_configured(
    state: &tauri::State<'_, AppState>,
    command: ControlCommand,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_app_state(state.inner(), command).await
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
pub async fn inspect_health(state: tauri::State<'_, AppState>) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::InspectHealth).await
}

#[tauri::command]
pub async fn inspect_semantic_health(
    state: tauri::State<'_, AppState>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::InspectSemanticHealth).await
}

/// 资源域恢复巡检(W32-B 补接线:G8 资源恢复动作的 CAS 预期来源)。
#[tauri::command]
pub async fn inspect_resource_health(
    state: tauri::State<'_, AppState>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::InspectResourceHealth).await
}

#[tauri::command]
pub async fn export_metrics(state: tauri::State<'_, AppState>) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::ExportMetrics).await
}

#[tauri::command]
pub async fn export_semantic_metrics(
    state: tauri::State<'_, AppState>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::ExportSemanticMetrics).await
}

/// W32-E 资源监控:resource 域(G8)指标导出——既有只读命令
/// `ControlCommand::ExportResourceMetrics` 的 GUI 接线(W32-A 只接了
/// artifact/semantic 两域)。与另外两条导出命令同路经认证入口,回执携带
/// OpenMetrics 文本;无任何新控制路径。
#[tauri::command]
pub async fn export_resource_metrics(
    state: tauri::State<'_, AppState>,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, ControlCommand::ExportResourceMetrics).await
}

#[tauri::command]
pub async fn inspect_task(
    state: tauri::State<'_, AppState>,
    plan_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let plan_id = principal_bytes(&plan_id_hex)?;
    dispatch_configured(&state, ControlCommand::InspectTask { plan_id }).await
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
pub async fn inspect_task_group(
    state: tauri::State<'_, AppState>,
    group_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, layer_inspect_task_group(&group_id_hex)?).await
}

/// W39-D:InspectTaskNode 的 GUI 接线。
#[tauri::command]
pub async fn inspect_task_node(
    state: tauri::State<'_, AppState>,
    plan_id_hex: String,
    node_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, layer_inspect_task_node(&plan_id_hex, &node_id_hex)?).await
}

/// W39-D:InspectExecutionFiber 的 GUI 接线。
#[tauri::command]
pub async fn inspect_execution_fiber(
    state: tauri::State<'_, AppState>,
    fiber_id_hex: String,
    generation: u64,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(
        &state,
        layer_inspect_execution_fiber(&fiber_id_hex, generation)?,
    )
    .await
}

/// W39-D:InspectTopic 的 GUI 接线。
#[tauri::command]
pub async fn inspect_topic(
    state: tauri::State<'_, AppState>,
    topic_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(&state, layer_inspect_topic(&topic_id_hex)?).await
}

/// W39-D:InspectOperation 的 GUI 接线。
#[tauri::command]
pub async fn inspect_operation(
    state: tauri::State<'_, AppState>,
    operation_id_hex: String,
    generation: u64,
) -> Result<ReceiptDto, DesktopError> {
    dispatch_configured(
        &state,
        layer_inspect_operation(&operation_id_hex, generation)?,
    )
    .await
}

#[tauri::command]
pub async fn inspect_process(
    state: tauri::State<'_, AppState>,
    process_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let process_id = principal_bytes(&process_id_hex)?;
    dispatch_configured(&state, ControlCommand::InspectProcess { process_id }).await
}

#[tauri::command]
pub async fn inspect_resource(
    state: tauri::State<'_, AppState>,
    reservation_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let reservation_id = principal_bytes(&reservation_id_hex)?;
    dispatch_configured(&state, ControlCommand::InspectResource { reservation_id }).await
}

/// W66 (`C-UNFREEZE-001`) + Stage D (`IPC-D4`):`InspectSearch` 命令构造器。
pub fn build_inspect_search(scope_id_hex: &str) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::InspectSearch {
        scope_id: scope_32_bytes(scope_id_hex)?,
    })
}

fn verify_search_inspected_restrictions(receipt: &ReceiptDto) -> Result<(), DesktopError> {
    if let OutcomeDto::SearchInspected {
        non_authoritative_restrictions_bits,
        ..
    } = &receipt.outcome
        && u32::from(*non_authoritative_restrictions_bits)
            != SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
    {
        return Err(DesktopError::new(
            ErrorCode::Control,
            format!(
                "search inspection violated 0x1F non-authoritative restriction bits: {non_authoritative_restrictions_bits:#x}"
            ),
        ));
    }
    Ok(())
}

/// `InspectSearch` 无 Tauri 句柄核心(强制 `0x1F` 非权威五禁掩码 fail-closed)。
pub async fn inspect_search_core(
    state: &AppState,
    scope_id_hex: &str,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_inspect_search(scope_id_hex)?;
    let receipt = dispatch_app_state(state, command).await?;
    verify_search_inspected_restrictions(&receipt)?;
    Ok(receipt)
}

#[tauri::command]
pub async fn inspect_search(
    state: tauri::State<'_, AppState>,
    scope_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    inspect_search_core(state.inner(), &scope_id_hex).await
}

/// W66 (`C-UNFREEZE-001`) + Stage D (`IPC-D6`):`InspectNotification` 命令构造器。
pub fn build_inspect_notification(
    notification_id_hex: &str,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::InspectNotification {
        notification_id: principal_bytes(notification_id_hex)?,
    })
}

pub async fn inspect_notification_core(
    state: &AppState,
    notification_id_hex: &str,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_inspect_notification(notification_id_hex)?;
    dispatch_app_state(state, command).await
}

#[tauri::command]
pub async fn inspect_notification(
    state: tauri::State<'_, AppState>,
    notification_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    inspect_notification_core(state.inner(), &notification_id_hex).await
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
pub async fn inspect_resource_cost(
    state: tauri::State<'_, AppState>,
    reservation_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let config = state.snapshot()?;
    let reservation_id = principal_bytes(&reservation_id_hex)?;
    dispatch_off_main(move || {
        let (socket, principal, key_file) = required(&config)?;
        let authority = open_resource_authority(config.resource_root.as_deref())?;
        tauri::async_runtime::block_on(dispatch_cost_inspect(
            &socket,
            &principal,
            &key_file,
            authority.as_ref(),
            reservation_id,
        ))
    })
    .await
}

/// W32-D 一致性自检(渲染事实 vs 直接复检):同一 reservation 两次独立
/// 认证派发 InspectResource,逐字段比较有界成本事实并比对 receipt hex。
/// 已结清(FINALIZED)预留的事实不可变,两次必须一致;未接线形态两侧
/// 同为类型化 NOT_FOUND,字节同样可比。
#[tauri::command]
pub async fn cost_fact_check(
    state: tauri::State<'_, AppState>,
    reservation_id_hex: String,
) -> Result<FactCheckDto, DesktopError> {
    let config = state.snapshot()?;
    let reservation_id = principal_bytes(&reservation_id_hex)?;
    dispatch_off_main(move || {
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
    })
    .await
}

/// `ControlCommand::InspectApplication` 的 GUI 接线。未配置
/// `application_root` 时,若会话未注入 `desktop_surface`,走既有未接线 inspector,
/// 回执为类型化 `NOT_FOUND`。
#[tauri::command]
pub async fn inspect_application(
    state: tauri::State<'_, AppState>,
    package_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let config = state.snapshot()?;
    let package_id = principal_bytes(&package_id_hex)?;
    if config
        .application_root
        .as_deref()
        .map(str::trim)
        .is_none_or(str::is_empty)
        && state.desktop_surface.is_some()
    {
        return dispatch_configured(&state, ControlCommand::InspectApplication { package_id })
            .await;
    }
    dispatch_off_main(move || {
        let (socket, principal, key_file) = required(&config)?;
        let authority = open_application_authority(config.application_root.as_deref())?;
        tauri::async_runtime::block_on(dispatch_application_inspect(
            &socket,
            &principal,
            &key_file,
            authority.as_ref(),
            package_id,
        ))
    })
    .await
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

// =========================================================================
// Stage D (`IPC-D1..D8` & `[UI-PARITY-001]`) 有界枚举、水位线、预演与对账命令
// =========================================================================

pub fn build_list_tasks(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListTasks {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_tasks(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_tasks(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_task_groups(
    task_id_hex: Option<&str>,
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListTaskGroups {
        task_id: optional_id_bytes(task_id_hex)?,
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_task_groups(
    state: tauri::State<'_, AppState>,
    task_id_hex: Option<String>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_task_groups(task_id_hex.as_deref(), cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_task_nodes(
    plan_id_hex: &str,
    expected_plan_revision: u64,
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListTaskNodes {
        plan_id: principal_bytes(plan_id_hex)?,
        expected_plan_revision,
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_task_nodes(
    state: tauri::State<'_, AppState>,
    plan_id_hex: String,
    expected_plan_revision: u64,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_task_nodes(
        &plan_id_hex,
        expected_plan_revision,
        cursor_hex.as_deref(),
        limit,
    )?;
    dispatch_configured(&state, command).await
}

pub fn build_list_task_attempts(
    scope_filter_id_hex: Option<&str>,
    expected_generation: u64,
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListTaskAttempts {
        scope_filter_id: optional_id_bytes(scope_filter_id_hex)?,
        expected_generation,
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_task_attempts(
    state: tauri::State<'_, AppState>,
    scope_filter_id_hex: Option<String>,
    expected_generation: u64,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_task_attempts(
        scope_filter_id_hex.as_deref(),
        expected_generation,
        cursor_hex.as_deref(),
        limit,
    )?;
    dispatch_configured(&state, command).await
}

pub fn build_list_execution_fibers(
    process_id_hex: Option<&str>,
    expected_generation: u64,
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListExecutionFibers {
        process_id: optional_id_bytes(process_id_hex)?,
        expected_generation,
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_execution_fibers(
    state: tauri::State<'_, AppState>,
    process_id_hex: Option<String>,
    expected_generation: u64,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_execution_fibers(
        process_id_hex.as_deref(),
        expected_generation,
        cursor_hex.as_deref(),
        limit,
    )?;
    dispatch_configured(&state, command).await
}

pub fn build_list_operations(
    fiber_id_hex: Option<&str>,
    expected_generation: u64,
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListOperations {
        fiber_id: optional_id_bytes(fiber_id_hex)?,
        expected_generation,
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_operations(
    state: tauri::State<'_, AppState>,
    fiber_id_hex: Option<String>,
    expected_generation: u64,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_operations(
        fiber_id_hex.as_deref(),
        expected_generation,
        cursor_hex.as_deref(),
        limit,
    )?;
    dispatch_configured(&state, command).await
}

pub fn build_list_installed_applications(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListInstalledApplications {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_installed_applications(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_installed_applications(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_query_search_candidates(
    scope_id_hex: &str,
    query: &str,
    cursor_offset: u64,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    if query.len() > 256 {
        return Err(DesktopError::config(
            "search query exceeds bounded 256-byte limit",
        ));
    }
    Ok(ControlCommand::QuerySearchCandidates {
        scope_id: scope_32_bytes(scope_id_hex)?,
        query: query.to_owned(),
        cursor_offset,
        limit: validate_enum_limit_bound(limit)?,
    })
}

fn verify_search_candidates_restrictions(receipt: &ReceiptDto) -> Result<(), DesktopError> {
    if let OutcomeDto::SearchCandidatesQueried {
        non_authoritative_restrictions_bits,
        candidates,
        ..
    } = &receipt.outcome
    {
        if *non_authoritative_restrictions_bits != SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL {
            return Err(DesktopError::new(
                ErrorCode::Control,
                format!(
                    "search candidate summary violated 0x1F non-authoritative restriction bits: {non_authoritative_restrictions_bits:#x}"
                ),
            ));
        }
        if let Some(violating) = candidates.iter().find(|candidate| {
            candidate.non_authoritative_restrictions_bits
                != SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
        }) {
            return Err(DesktopError::new(
                ErrorCode::Control,
                format!(
                    "search candidate {} violated 0x1F non-authoritative restriction bits: {:#x}",
                    violating.candidate_id_hex, violating.non_authoritative_restrictions_bits
                ),
            ));
        }
    }
    Ok(())
}

pub async fn query_search_candidates_core(
    state: &AppState,
    scope_id_hex: &str,
    query: &str,
    cursor_offset: u64,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_query_search_candidates(scope_id_hex, query, cursor_offset, limit)?;
    let receipt = dispatch_app_state(state, command).await?;
    verify_search_candidates_restrictions(&receipt)?;
    Ok(receipt)
}

#[tauri::command]
pub async fn query_search_candidates(
    state: tauri::State<'_, AppState>,
    scope_id_hex: String,
    query: String,
    cursor_offset: u64,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    query_search_candidates_core(state.inner(), &scope_id_hex, &query, cursor_offset, limit).await
}

pub fn build_poll_desktop_watermark_events(
    since_watermark: u64,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::PollDesktopWatermarkEvents {
        since_watermark,
        limit: validate_watermark_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn poll_desktop_watermark_events(
    state: tauri::State<'_, AppState>,
    since_watermark: u64,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_poll_desktop_watermark_events(since_watermark, limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_inspect_settings_domain(
    domain_kind: &str,
    scope_target_id_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::InspectSettingsDomain {
        domain_kind: parse_settings_domain_kind(domain_kind.trim())
            .map_err(|error| from_control_error(&error))?,
        scope_target_id: optional_id_bytes(scope_target_id_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn inspect_settings_domain(
    state: tauri::State<'_, AppState>,
    domain_kind: String,
    scope_target_id_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command =
        build_inspect_settings_domain(&domain_kind, scope_target_id_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_content_type_associations(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListContentTypeAssociations {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_content_type_associations(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_content_type_associations(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_keychain_references(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListKeychainReferences {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_keychain_references(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_keychain_references(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_chooser_grants(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListChooserGrants {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_chooser_grants(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_chooser_grants(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_workspaces(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListWorkspaces {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_workspaces(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_workspaces(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_inspect_workspace(
    workspace_id_hex: &str,
    expected_revision: u64,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::InspectWorkspace {
        workspace_id: principal_bytes(workspace_id_hex)?,
        expected_revision,
    })
}

#[tauri::command]
pub async fn inspect_workspace(
    state: tauri::State<'_, AppState>,
    workspace_id_hex: String,
    expected_revision: u64,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_inspect_workspace(&workspace_id_hex, expected_revision)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_processes(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListProcesses {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_processes(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_processes(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_resource_reservations(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListResourceReservations {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_resource_reservations(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_resource_reservations(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_topics(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListTopics {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_topics(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_topics(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_notifications(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListNotifications {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_notifications(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_notifications(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_list_cells(
    cursor_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::ListCells {
        cursor: optional_id_bytes(cursor_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn list_cells(
    state: tauri::State<'_, AppState>,
    cursor_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_list_cells(cursor_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_inspect_cell_leases(
    cell_id_hex: Option<&str>,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::InspectCellLeases {
        cell_id: optional_id_bytes(cell_id_hex)?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn inspect_cell_leases(
    state: tauri::State<'_, AppState>,
    cell_id_hex: Option<String>,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_inspect_cell_leases(cell_id_hex.as_deref(), limit)?;
    dispatch_configured(&state, command).await
}

pub fn build_preview_control_blast_radius(
    target_scope: &str,
    target_id_hex: &str,
    secondary_id_hex: Option<&str>,
    preview_action: &str,
    limit: u32,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::PreviewControlBlastRadius {
        target_scope: parse_control_scope(target_scope.trim())
            .map_err(|error| from_control_error(&error))?,
        target_id: principal_bytes(target_id_hex)?,
        secondary_id: optional_id_bytes(secondary_id_hex)?,
        preview_action: parse_preview_action_code(preview_action.trim())
            .map_err(|error| from_control_error(&error))?,
        limit: validate_enum_limit_bound(limit)?,
    })
}

#[tauri::command]
pub async fn preview_control_blast_radius(
    state: tauri::State<'_, AppState>,
    target_scope: String,
    target_id_hex: String,
    secondary_id_hex: Option<String>,
    preview_action: String,
    limit: u32,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_preview_control_blast_radius(
        &target_scope,
        &target_id_hex,
        secondary_id_hex.as_deref(),
        &preview_action,
        limit,
    )?;
    dispatch_configured(&state, command).await
}

pub fn build_query_control_command_status(
    target_command_id_hex: &str,
) -> Result<ControlCommand, DesktopError> {
    Ok(ControlCommand::QueryControlCommandStatus {
        target_command_id: principal_bytes(target_command_id_hex)?,
    })
}

#[tauri::command]
pub async fn query_control_command_status(
    state: tauri::State<'_, AppState>,
    target_command_id_hex: String,
) -> Result<ReceiptDto, DesktopError> {
    let command = build_query_control_command_status(&target_command_id_hex)?;
    dispatch_configured(&state, command).await
}

/// §25.3 idempotency 身份:每次提交新生成 16 字节 OS 随机(Unix 读
/// `/dev/urandom`;Windows 经 `getrandom` 走 BCrypt CSPRNG)。认证派发
/// 已跨平台,命令 id 的生成不再有平台桩。
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

#[cfg(windows)]
fn fresh_command_id() -> Result<[u8; 16], DesktopError> {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id)
        .map_err(|error| DesktopError::internal(format!("读取命令 id 失败: {error}")))?;
    Ok(id)
}

/// GUI 授权动作(SABI v1.4 + Stage D 对称控制面;serde tag 与 CLI operation 名一致)。
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ControlAction {
    AckRecoveryAlert {
        #[serde(alias = "planIdHex")]
        plan_id_hex: String,
        #[serde(alias = "expectedTotalFailures")]
        expected_total_failures: u64,
        reason: String,
    },
    AckSemanticRecoveryAlert {
        #[serde(alias = "planIdHex")]
        plan_id_hex: String,
        #[serde(alias = "expectedTotalFailures")]
        expected_total_failures: u64,
        reason: String,
    },
    ResumeSemanticRecovery {
        #[serde(alias = "planIdHex")]
        plan_id_hex: String,
        #[serde(alias = "expectedTotalFailures")]
        expected_total_failures: u64,
        reason: String,
    },
    AckResourceRecoveryAlert {
        #[serde(alias = "planIdHex")]
        plan_id_hex: String,
        #[serde(alias = "expectedTotalFailures")]
        expected_total_failures: u64,
        reason: String,
    },
    ResumeResourceRecovery {
        #[serde(alias = "planIdHex")]
        plan_id_hex: String,
        #[serde(alias = "expectedTotalFailures")]
        expected_total_failures: u64,
        reason: String,
    },
    PauseOperation {
        #[serde(alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    ResumeOperation {
        #[serde(alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    CancelOperation {
        #[serde(alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    KillOperation {
        #[serde(alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    ThrottleOperation {
        #[serde(alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        #[serde(alias = "throttlePercent")]
        throttle_percent: u64,
        reason: String,
    },
    ReclaimOperation {
        #[serde(alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    DisableApplication {
        #[serde(alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        #[serde(default)]
        scope: Option<String>,
        reason: String,
    },
    UninstallApplication {
        #[serde(alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        #[serde(default)]
        scope: Option<String>,
        reason: String,
    },
    ScopedOperation {
        scope: String,
        verb: String,
        #[serde(alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        #[serde(default, alias = "throttlePercent")]
        throttle_percent: Option<u64>,
        reason: String,
    },
    RequestProcessMigration {
        #[serde(alias = "processIdHex")]
        process_id_hex: String,
        #[serde(alias = "destinationCellIdHex")]
        destination_cell_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    PurgeTopic {
        #[serde(alias = "topicIdHex")]
        topic_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        #[serde(default = "default_true", alias = "acknowledgeDataLoss")]
        acknowledge_data_loss: bool,
        reason: String,
    },
    UpdateSettingsDomain {
        #[serde(alias = "domainKind")]
        domain_kind: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default, alias = "scopeTargetIdHex")]
        scope_target_id_hex: Option<String>,
        #[serde(alias = "expectedDomainRevision")]
        expected_domain_revision: u64,
        key: String,
        #[serde(alias = "valueCborHex")]
        value_cbor_hex: String,
        reason: String,
    },
    SetContentTypeAssociation {
        #[serde(alias = "associationIdHex")]
        association_id_hex: String,
        #[serde(alias = "contentType")]
        content_type: String,
        #[serde(alias = "applicationIdHex")]
        application_id_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    IssueChooserGrant {
        #[serde(alias = "grantIdHex")]
        grant_id_hex: String,
        #[serde(alias = "applicationIdHex")]
        application_id_hex: String,
        #[serde(alias = "chooserKind")]
        chooser_kind: u32,
        #[serde(alias = "resourceHandleHex")]
        resource_handle_hex: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    UndoReceipt {
        #[serde(default)]
        scope: Option<String>,
        #[serde(alias = "targetReceiptIdHex", alias = "targetIdHex")]
        target_id_hex: String,
        #[serde(alias = "undoClass")]
        undo_class: String,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    MutateSearchCandidate {
        #[serde(alias = "candidateIdHex")]
        candidate_id_hex: String,
        #[serde(alias = "scopeIdHex")]
        scope_id_hex: String,
        #[serde(
            default = "default_search_restrictions_bits",
            alias = "restrictionsBits"
        )]
        restrictions_bits: u32,
        #[serde(alias = "expectedRevision")]
        expected_revision: u64,
        reason: String,
    },
    MutateTaskNodeTopology {
        #[serde(alias = "planIdHex")]
        plan_id_hex: String,
        #[serde(alias = "nodeIdHex")]
        node_id_hex: String,
        #[serde(alias = "observedState")]
        observed_state: String,
        #[serde(alias = "expectedPlanRevision")]
        expected_plan_revision: u64,
        reason: String,
    },
    OverrideTaskAttemptCommit {
        #[serde(alias = "attemptIdHex")]
        attempt_id_hex: String,
        #[serde(alias = "expectedGeneration")]
        expected_generation: u64,
        reason: String,
    },
    OverrideEffectUnknownOperation {
        #[serde(alias = "operationIdHex")]
        operation_id_hex: String,
        #[serde(default = "default_true", alias = "markSucceeded")]
        mark_succeeded: bool,
        #[serde(alias = "expectedGeneration")]
        expected_generation: u64,
        reason: String,
    },
}

const fn default_true() -> bool {
    true
}

const fn default_search_restrictions_bits() -> u32 {
    SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
}

fn compile_scoped_verb(
    verb: &str,
    throttle_percent: Option<u64>,
) -> Result<ScopedControlAction, DesktopError> {
    let trimmed = verb.trim();
    if trimmed.eq_ignore_ascii_case("throttle")
        || trimmed.eq_ignore_ascii_case("throttle-operation")
        || trimmed == "限流"
    {
        let percent = throttle_percent.ok_or_else(|| {
            DesktopError::config("scoped throttle 动作必须提供 throttle_percent(1..=100)")
        })?;
        if !(1..=100).contains(&percent) {
            return Err(DesktopError::config(
                "throttle_percent 必须是 1..=100 的整数百分比(与上游 wire 前拒绝同规)",
            ));
        }
        return Ok(ScopedControlAction::Throttle {
            throttle_percent: percent,
        });
    }
    if throttle_percent.is_some() {
        return Err(DesktopError::config(
            "非 throttle 作用域动作不得携带 throttle_percent",
        ));
    }
    parse_unit_scoped_action(trimmed)
        .map(|(action, _)| action)
        .map_err(|error| from_control_error(&error))
}

/// 动作 → [`ControlCommand`] 的单一编译点(派发前类型化校验:32 hex 目标、
/// 非空 reason;throttle 百分比镜像上游 1..=100 wire 前拒绝规则)。
/// Tauri `submit_control` 命令与集成测试共用(测试注入确定性命令 id)。
#[allow(clippy::too_many_lines)]
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
        ControlAction::DisableApplication {
            target_id_hex,
            expected_revision,
            scope,
            reason: raw,
        } => {
            let target_id = principal_bytes(&target_id_hex)?;
            let bounded_reason = reason(&raw)?;
            if let Some(raw_scope) = scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let parsed_scope =
                    parse_control_scope(raw_scope).map_err(|error| from_control_error(&error))?;
                Ok(ControlCommand::ScopedOperation {
                    scope: parsed_scope,
                    action: ScopedControlAction::DisableApplication,
                    control_command_id,
                    target_id,
                    expected_generation_or_revision: expected_revision,
                    reason: bounded_reason,
                })
            } else {
                Ok(ControlCommand::DisableApplication {
                    control_command_id,
                    package_id: target_id,
                    expected_generation_or_revision: expected_revision,
                    reason: bounded_reason,
                })
            }
        }
        ControlAction::UninstallApplication {
            target_id_hex,
            expected_revision,
            scope,
            reason: raw,
        } => {
            let target_id = principal_bytes(&target_id_hex)?;
            let bounded_reason = reason(&raw)?;
            if let Some(raw_scope) = scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let parsed_scope =
                    parse_control_scope(raw_scope).map_err(|error| from_control_error(&error))?;
                Ok(ControlCommand::ScopedOperation {
                    scope: parsed_scope,
                    action: ScopedControlAction::UninstallApplication,
                    control_command_id,
                    target_id,
                    expected_generation_or_revision: expected_revision,
                    reason: bounded_reason,
                })
            } else {
                Ok(ControlCommand::UninstallApplication {
                    control_command_id,
                    package_id: target_id,
                    expected_generation_or_revision: expected_revision,
                    reason: bounded_reason,
                })
            }
        }
        ControlAction::ScopedOperation {
            scope,
            verb,
            target_id_hex,
            expected_revision,
            throttle_percent,
            reason: raw,
        } => Ok(ControlCommand::ScopedOperation {
            scope: parse_control_scope(scope.trim()).map_err(|error| from_control_error(&error))?,
            action: compile_scoped_verb(&verb, throttle_percent)?,
            control_command_id,
            target_id: principal_bytes(&target_id_hex)?,
            expected_generation_or_revision: expected_revision,
            reason: reason(&raw)?,
        }),
        ControlAction::RequestProcessMigration {
            process_id_hex,
            destination_cell_id_hex,
            expected_revision,
            reason: raw,
        } => Ok(ControlCommand::RequestProcessMigration {
            control_command_id,
            process_id: principal_bytes(&process_id_hex)?,
            destination_cell_id: principal_bytes(&destination_cell_id_hex)?,
            expected_generation_or_revision: expected_revision,
            reason: reason(&raw)?,
        }),
        ControlAction::PurgeTopic {
            topic_id_hex,
            expected_revision,
            acknowledge_data_loss,
            reason: raw,
        } => Ok(ControlCommand::PurgeTopic {
            control_command_id,
            topic_id: principal_bytes(&topic_id_hex)?,
            expected_generation_or_revision: expected_revision,
            acknowledge_data_loss,
            reason: reason(&raw)?,
        }),
        ControlAction::UpdateSettingsDomain {
            domain_kind,
            scope,
            scope_target_id_hex,
            expected_domain_revision,
            key,
            value_cbor_hex,
            reason: raw,
        } => {
            let trimmed_key = key.trim();
            if trimmed_key.is_empty() || trimmed_key.len() > 128 {
                return Err(DesktopError::config(
                    "settings key 必须非空且不超过 128 字节",
                ));
            }
            let parsed_kind = parse_settings_domain_kind(domain_kind.trim())
                .map_err(|error| from_control_error(&error))?;
            let resolved_scope = match scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(raw_scope) => {
                    parse_control_scope(raw_scope).map_err(|error| from_control_error(&error))?
                }
                None => match parsed_kind {
                    SettingsDomainKind::Workspace => ControlScope::Workspace,
                    SettingsDomainKind::Application => ControlScope::Application,
                    SettingsDomainKind::System
                    | SettingsDomainKind::User
                    | SettingsDomainKind::Unspecified => ControlScope::System,
                },
            };
            Ok(ControlCommand::UpdateSettingsDomain {
                control_command_id,
                scope: resolved_scope,
                target_id: optional_id_bytes(scope_target_id_hex.as_deref())?.unwrap_or([0u8; 16]),
                expected_generation_or_revision: expected_domain_revision,
                domain_kind: parsed_kind,
                key: trimmed_key.to_owned(),
                value_cbor: decode_hex_bytes(&value_cbor_hex, MAX_SETTINGS_CBOR_BYTES)?,
                reason: reason(&raw)?,
            })
        }
        ControlAction::SetContentTypeAssociation {
            association_id_hex,
            content_type,
            application_id_hex,
            expected_revision,
            reason: raw,
        } => {
            let trimmed_ct = content_type.trim();
            if trimmed_ct.is_empty() || trimmed_ct.len() > 128 {
                return Err(DesktopError::config(
                    "content_type 必须非空且不超过 128 字节",
                ));
            }
            Ok(ControlCommand::SetContentTypeAssociation {
                control_command_id,
                association_id: principal_bytes(&association_id_hex)?,
                content_type: trimmed_ct.to_owned(),
                application_id: principal_bytes(&application_id_hex)?,
                expected_generation_or_revision: expected_revision,
                reason: reason(&raw)?,
            })
        }
        ControlAction::IssueChooserGrant {
            grant_id_hex,
            application_id_hex,
            chooser_kind,
            resource_handle_hex,
            expected_revision,
            reason: raw,
        } => Ok(ControlCommand::IssueChooserGrant {
            control_command_id,
            grant_id: principal_bytes(&grant_id_hex)?,
            application_id: principal_bytes(&application_id_hex)?,
            chooser_kind,
            resource_handle: principal_bytes(&resource_handle_hex)?,
            expected_generation_or_revision: expected_revision,
            reason: reason(&raw)?,
        }),
        ControlAction::UndoReceipt {
            scope,
            target_id_hex,
            undo_class,
            expected_revision,
            reason: raw,
        } => {
            let resolved_scope = match scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(raw_scope) => {
                    parse_control_scope(raw_scope).map_err(|error| from_control_error(&error))?
                }
                None => ControlScope::Task,
            };
            Ok(ControlCommand::UndoReceipt {
                control_command_id,
                scope: resolved_scope,
                target_id: principal_bytes(&target_id_hex)?,
                receipt_undo_class: parse_undo_class(undo_class.trim())
                    .map_err(|error| from_control_error(&error))?,
                expected_generation_or_revision: expected_revision,
                reason: reason(&raw)?,
            })
        }
        ControlAction::MutateSearchCandidate {
            candidate_id_hex,
            scope_id_hex,
            restrictions_bits,
            expected_revision,
            reason: raw,
        } => Ok(ControlCommand::MutateSearchCandidate {
            control_command_id,
            candidate_id: principal_bytes(&candidate_id_hex)?,
            scope_id: scope_32_bytes(&scope_id_hex)?,
            non_authoritative_restrictions_bits: restrictions_bits,
            expected_generation_or_revision: expected_revision,
            reason: reason(&raw)?,
        }),
        ControlAction::MutateTaskNodeTopology {
            plan_id_hex,
            node_id_hex,
            observed_state,
            expected_plan_revision,
            reason: raw,
        } => Ok(ControlCommand::MutateTaskNodeTopology {
            control_command_id,
            plan_id: principal_bytes(&plan_id_hex)?,
            node_id: principal_bytes(&node_id_hex)?,
            observed_node_state: parse_plan_node_state(observed_state.trim())
                .map_err(|error| from_control_error(&error))?,
            expected_generation_or_revision: expected_plan_revision,
            reason: reason(&raw)?,
        }),
        ControlAction::OverrideTaskAttemptCommit {
            attempt_id_hex,
            expected_generation,
            reason: raw,
        } => Ok(ControlCommand::OverrideTaskAttemptCommit {
            control_command_id,
            attempt_id: principal_bytes(&attempt_id_hex)?,
            expected_generation_or_revision: expected_generation,
            reason: reason(&raw)?,
        }),
        ControlAction::OverrideEffectUnknownOperation {
            operation_id_hex,
            mark_succeeded,
            expected_generation,
            reason: raw,
        } => Ok(ControlCommand::OverrideEffectUnknownOperation {
            control_command_id,
            operation_id: principal_bytes(&operation_id_hex)?,
            mark_succeeded,
            expected_generation_or_revision: expected_generation,
            reason: reason(&raw)?,
        }),
    }
}

/// 确定性命令身份提交核心(供四入口字节等价性测试调用,与 `submit_control` 同路径)。
pub async fn submit_control_with_id_core(
    state: &AppState,
    action: ControlAction,
    control_command_id: [u8; 16],
) -> Result<ReceiptDto, DesktopError> {
    let command = build_control_command(action, control_command_id)?;
    dispatch_app_state(state, command).await
}

/// 随机命令身份提交核心(`submit_control` 的无 Tauri 句柄等价入口)。
pub async fn submit_control_core(
    state: &AppState,
    action: ControlAction,
) -> Result<ReceiptDto, DesktopError> {
    submit_control_with_id_core(state, action, fresh_command_id()?).await
}

/// W32-B 写入半唯一入口:授权动作 → 真实 ControlCommand → 认证 IPC。
/// 回执(含类型化失败)完整返回前端渲染,后端不改写失败。
#[tauri::command]
pub async fn submit_control(
    state: tauri::State<'_, AppState>,
    action: ControlAction,
) -> Result<ReceiptDto, DesktopError> {
    submit_control_core(state.inner(), action).await
}

/// 只读 operation → (ControlCommand, CLI 参数)。mutation 一律拒绝。
#[allow(clippy::too_many_lines)]
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
        "inspect-search" => {
            let raw = target_hex
                .ok_or_else(|| DesktopError::config("inspect-search 需要 64 hex scope_id"))?;
            let scope_id = scope_32_bytes(raw)?;
            Ok((
                ControlCommand::InspectSearch { scope_id },
                vec!["inspect-search".into(), raw.trim().to_owned()],
            ))
        }
        "inspect-notification" => {
            let notification_id = target()?;
            Ok((
                ControlCommand::InspectNotification { notification_id },
                vec![
                    "inspect-notification".into(),
                    target_hex.unwrap_or_default().trim().to_owned(),
                ],
            ))
        }
        "list-tasks" => Ok((
            ControlCommand::ListTasks {
                cursor: None,
                limit: 16,
            },
            vec!["list-tasks".into(), "16".into()],
        )),
        "list-task-groups" => Ok((
            ControlCommand::ListTaskGroups {
                task_id: None,
                cursor: None,
                limit: 16,
            },
            vec!["list-task-groups".into(), "16".into()],
        )),
        "list-installed-applications" => Ok((
            ControlCommand::ListInstalledApplications {
                cursor: None,
                limit: 16,
            },
            vec!["list-installed-applications".into(), "16".into()],
        )),
        "list-workspaces" => Ok((
            ControlCommand::ListWorkspaces {
                cursor: None,
                limit: 16,
            },
            vec!["list-workspaces".into(), "16".into()],
        )),
        "list-processes" => Ok((
            ControlCommand::ListProcesses {
                cursor: None,
                limit: 16,
            },
            vec!["list-processes".into(), "16".into()],
        )),
        "list-resource-reservations" => Ok((
            ControlCommand::ListResourceReservations {
                cursor: None,
                limit: 16,
            },
            vec!["list-resource-reservations".into(), "16".into()],
        )),
        "list-topics" => Ok((
            ControlCommand::ListTopics {
                cursor: None,
                limit: 16,
            },
            vec!["list-topics".into(), "16".into()],
        )),
        "list-notifications" => Ok((
            ControlCommand::ListNotifications {
                cursor: None,
                limit: 16,
            },
            vec!["list-notifications".into(), "16".into()],
        )),
        "list-cells" => Ok((
            ControlCommand::ListCells {
                cursor: None,
                limit: 16,
            },
            vec!["list-cells".into(), "16".into()],
        )),
        "inspect-cell-leases" => Ok((
            ControlCommand::InspectCellLeases {
                cell_id: None,
                limit: 16,
            },
            vec!["inspect-cell-leases".into(), "16".into()],
        )),
        "query-control-command-status" => {
            let target_command_id = target()?;
            Ok((
                ControlCommand::QueryControlCommandStatus { target_command_id },
                vec![
                    "query-control-command-status".into(),
                    target_hex.unwrap_or_default().trim().to_owned(),
                ],
            ))
        }
        _ => Err(DesktopError::config(format!(
            "未知或非只读 operation: {operation}(读路径自检只收 inspect/export/list;写路径探针见 parity_check_write)"
        ))),
    }
}

/// 一致性自检(读路径):同一只读命令,GUI 经认证入口派发一次,真实
/// `system-control-cli` 经 plain socket 派发一次,比对 receipt hex。
#[tauri::command]
pub async fn parity_check(
    state: tauri::State<'_, AppState>,
    operation: String,
    target_hex: Option<String>,
) -> Result<ParityDto, DesktopError> {
    let config = state.snapshot()?;
    let (command, cli_args) = parity_command(&operation, target_hex.as_deref())?;
    dispatch_off_main(move || {
        let (socket, principal, key_file) = required(&config)?;
        let gui = tauri::async_runtime::block_on(dispatch_control(
            &socket, &principal, &key_file, command,
        ))?;
        let cli = run_cli(&config, &cli_args)?;
        Ok(ParityDto {
            matched: cli.receipt_hex.as_deref() == Some(gui.receipt_hex.as_str()),
            gui_receipt_hex: gui.receipt_hex,
            cli_receipt_hex: cli.receipt_hex,
            cli_exit_code: cli.exit_code,
            cli_stderr: cli.stderr,
            operation,
        })
    })
    .await
}

/// 一次真实 `system-control-cli` 子进程运行的首行 `RECEIPT <hex>` 投影。
#[derive(Debug)]
struct CliRun {
    receipt_hex: Option<String>,
    exit_code: Option<i32>,
    stderr: Option<String>,
}

/// CLI 子进程墙钟预算:plain 入口一次交换的传输预算为 connect+read+write
/// 各 5s(上游 TransportConfig 默认),叠加 CLI 自身启动与指标文本量,
/// 30s 覆盖慢启动仍有界(深审计 42 D2:无超时的 `output()` 可把调用线程
/// 无限期挂住)。
const CLI_RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn run_cli(config: &SessionConfig, cli_args: &[String]) -> Result<CliRun, DesktopError> {
    run_cli_with_timeout(config, cli_args, CLI_RUN_TIMEOUT)
}

fn run_cli_with_timeout(
    config: &SessionConfig,
    cli_args: &[String],
    timeout_budget: std::time::Duration,
) -> Result<CliRun, DesktopError> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let cli_socket = config.cli_socket.clone().ok_or_else(|| {
        DesktopError::config("一致性自检需要 cli_socket(plain 入口;开发夹具提供)")
    })?;
    let cli_path = config
        .cli_path
        .clone()
        .unwrap_or_else(|| DEFAULT_CLI_PATH.to_owned());
    // ETXTBSY(26) 是"写后即执"的内核侧竞态: 脚本已 fs::write 关闭, 但慢速
    // runner 的写回/扫描仍短暂持有写打开。有界重试吸收, 其余错误照常上抛。
    let mut spawn_attempts = 0;
    let mut child = loop {
        match Command::new(&cli_path)
            .arg(&cli_socket)
            .args(cli_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => break child,
            Err(error) if error.raw_os_error() == Some(26) && spawn_attempts < 5 => {
                spawn_attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(error) => {
                return Err(DesktopError::ipc(format!(
                    "启动 system-control-cli({cli_path})失败: {error}"
                )));
            }
        }
    };
    // 读线程排空两路管道:否则子进程写满管道缓冲后会阻塞在 write 上,
    // 等待方永远等不到退出(经典 output() 死锁面)。
    fn drain_pipe<R>(pipe: Option<R>) -> Option<std::thread::JoinHandle<Vec<u8>>>
    where
        R: Read + Send + 'static,
    {
        pipe.map(|mut reader| {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = reader.read_to_end(&mut buffer);
                buffer
            })
        })
    }
    let stdout_reader = drain_pipe(child.stdout.take());
    let stderr_reader = drain_pipe(child.stderr.take());

    let deadline = std::time::Instant::now() + timeout_budget;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // 超时即终止并回收,不让挂起的 CLI 把调用方挂住。
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(DesktopError::ipc(format!(
                        "运行 system-control-cli({cli_path})超过 {}ms 墙钟预算,已终止子进程",
                        timeout_budget.as_millis()
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => {
                return Err(DesktopError::ipc(format!(
                    "等待 system-control-cli({cli_path})退出失败: {error}"
                )));
            }
        }
    };

    let stdout = stdout_reader
        .map(|reader| reader.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_reader
        .map(|reader| reader.join().unwrap_or_default())
        .unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout);
    let cli_receipt_hex = stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("RECEIPT "))
        .map(str::to_owned);
    let stderr = String::from_utf8_lossy(&stderr);
    Ok(CliRun {
        receipt_hex: cli_receipt_hex,
        exit_code: status.code(),
        stderr: (!stderr.trim().is_empty()).then(|| stderr.trim().to_owned()),
    })
}

/// 写路径自检的宿主保护开关(深审计 42 D9):探针执行真实
/// pause-operation mutation 且 GUI 侧先行,任意已配置 socket 上盲跑
/// 属未防护面。仅在显式设置 `LLMOS_DESKTOP_ALLOW_WRITE_PARITY=1`
/// (预期为 dev:server 开发夹具宿主)后可用。
pub const ENV_ALLOW_WRITE_PARITY: &str = "LLMOS_DESKTOP_ALLOW_WRITE_PARITY";

fn write_parity_allowed(flag: Option<String>) -> bool {
    matches!(flag.as_deref(), Some("1"))
}

/// GUI 侧 mutation 已派发而 CLI 侧运行失败时的错误面:必须披露 GUI
/// 回执,操作者才能感知"已写入"(深审计 42 D9 的感知缺失面)。
fn write_parity_cli_failure(gui_receipt_hex: &str, error: &DesktopError) -> DesktopError {
    DesktopError::ipc(format!(
        "CLI 侧运行失败,但 GUI 侧 pause-operation 已真实派发(RECEIPT {gui_receipt_hex});CLI 错误: {error}"
    ))
}

/// W32-B 写路径一致性探针:同一条 pause-operation 命令(同 command id、
/// 目标、CAS、reason——两侧字节同一),GUI 经认证入口、真实 CLI 经 plain
/// 入口各派发一次,比对 receipt hex。开发夹具未接线操作执行器时两侧同为
/// 确定性的类型化 `NOT_FOUND`(executor 未接线)失败回执,字节可比;在
/// 已接线执行器的宿主上,第一次派发可能真实暂停目标、第二次按 idempotency/
/// CAS 纪律回 CONFLICT——matched=false 即如实显示。探针受
/// [`ENV_ALLOW_WRITE_PARITY`] 宿主保护门约束。完整写路径 parity 矩阵
/// (成功/NotFound/Rights 三形态)钉死属 W32-C。
#[tauri::command]
pub async fn parity_check_write(
    state: tauri::State<'_, AppState>,
    command_id_hex: String,
    target_hex: String,
    expected_revision: u64,
    reason: String,
) -> Result<ParityDto, DesktopError> {
    if !write_parity_allowed(std::env::var(ENV_ALLOW_WRITE_PARITY).ok()) {
        return Err(DesktopError::config(
            "写路径自检会执行真实 pause-operation mutation:仅在开发夹具宿主显式设置 LLMOS_DESKTOP_ALLOW_WRITE_PARITY=1 后可用",
        ));
    }
    let config = state.snapshot()?;
    let trimmed_reason = reason.trim().to_owned();
    if trimmed_reason.is_empty() {
        return Err(DesktopError::config("写路径自检需要非空 reason"));
    }
    let command = ControlCommand::PauseOperation {
        control_command_id: principal_bytes(&command_id_hex)?,
        target_id: principal_bytes(&target_hex)?,
        expected_generation_or_revision: expected_revision,
        reason: trimmed_reason.clone(),
    };
    dispatch_off_main(move || {
        let (socket, principal, key_file) = required(&config)?;
        let gui = tauri::async_runtime::block_on(dispatch_control(
            &socket, &principal, &key_file, command,
        ))?;
        let cli_args = vec![
            "pause-operation".to_owned(),
            command_id_hex.trim().to_owned(),
            target_hex.trim().to_owned(),
            expected_revision.to_string(),
            trimmed_reason,
        ];
        let cli = run_cli(&config, &cli_args)
            .map_err(|error| write_parity_cli_failure(&gui.receipt_hex, &error))?;
        Ok(ParityDto {
            matched: cli.receipt_hex.as_deref() == Some(gui.receipt_hex.as_str()),
            gui_receipt_hex: gui.receipt_hex,
            cli_receipt_hex: cli.receipt_hex,
            cli_exit_code: cli.exit_code,
            cli_stderr: cli.stderr,
            operation: "pause-operation".to_owned(),
        })
    })
    .await
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

    #[test]
    fn stage_d_builders_enforce_pagination_and_scope_contracts() {
        assert_eq!(
            build_list_tasks(None, 0).unwrap_err().code,
            ErrorCode::Config
        );
        assert_eq!(
            build_list_tasks(None, 65).unwrap_err().code,
            ErrorCode::Config
        );
        assert!(build_list_tasks(None, 64).is_ok());

        assert_eq!(
            build_poll_desktop_watermark_events(0, 129)
                .unwrap_err()
                .code,
            ErrorCode::Config
        );
        assert!(build_poll_desktop_watermark_events(0, 128).is_ok());

        let disable_app = control_action(ControlAction::DisableApplication {
            target_id_hex: "51".repeat(16),
            expected_revision: 3,
            scope: None,
            reason: "disable app".to_owned(),
        })
        .unwrap();
        assert!(matches!(
            disable_app,
            ControlCommand::DisableApplication { .. }
        ));

        let scoped_pause = control_action(ControlAction::ScopedOperation {
            scope: "task-group".to_owned(),
            verb: "pause".to_owned(),
            target_id_hex: "61".repeat(16),
            expected_revision: 4,
            throttle_percent: None,
            reason: "pause group".to_owned(),
        })
        .unwrap();
        let ControlCommand::ScopedOperation { scope, action, .. } = scoped_pause else {
            panic!("expected scoped operation");
        };
        assert_eq!(scope, ControlScope::TaskGroup);
        assert_eq!(action, ScopedControlAction::Pause);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_command_id_is_unique_per_call() {
        let first = fresh_command_id().unwrap();
        let second = fresh_command_id().unwrap();
        assert_ne!(first, second);
    }

    /// H10 回归:阻塞派发必须离开调用线程(spawn_blocking),而不是把
    /// 调用线程(生产中即主线程)按 block_on 挂住。
    #[test]
    fn dispatch_off_main_runs_work_off_the_calling_thread() {
        let caller = std::thread::current().id();
        let ran_on_caller = tauri::async_runtime::block_on(async {
            dispatch_off_main(move || Ok(std::thread::current().id() == caller)).await
        })
        .unwrap();
        assert!(
            !ran_on_caller,
            "阻塞派发必须在调用线程之外的阻塞池线程上执行"
        );
    }

    /// run_cli 超时/成功/启动失败三面回归(深审计 42 D2)。脚本夹具避免
    /// 依赖真实 system-control-cli 二进制。
    #[cfg(unix)]
    fn write_cli_script(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        path.to_str().unwrap().to_owned()
    }

    #[cfg(unix)]
    fn cli_script_config(cli_path: &str) -> SessionConfig {
        SessionConfig {
            cli_socket: Some("ignored-socket".to_owned()),
            cli_path: Some(cli_path.to_owned()),
            ..SessionConfig::default()
        }
    }

    #[cfg(unix)]
    fn cli_script_dir(tag: &str) -> std::path::PathBuf {
        // 每个测试独立目录:并行测试共享同一临时目录会在清理时互相删除。
        std::env::temp_dir().join(format!("llmosdt-unit-{}-{tag}", std::process::id()))
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_projects_receipt_line_and_exit_code() {
        let dir = cli_script_dir("cli-ok");
        std::fs::create_dir_all(&dir).ok();
        let script = write_cli_script(
            &dir,
            "cli-ok.sh",
            "#!/bin/sh\necho 'RECEIPT 0123456789abcdef'\n",
        );
        let run = run_cli_with_timeout(&cli_script_config(&script), &[], CLI_RUN_TIMEOUT).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(run.receipt_hex.as_deref(), Some("0123456789abcdef"));
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.stderr, None);
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_kills_hung_subprocess_after_the_wall_clock_budget() {
        let dir = cli_script_dir("cli-hang");
        std::fs::create_dir_all(&dir).ok();
        let script = write_cli_script(&dir, "cli-hang.sh", "#!/bin/sh\nsleep 30\n");
        let started = std::time::Instant::now();
        let error = run_cli_with_timeout(
            &cli_script_config(&script),
            &[],
            std::time::Duration::from_millis(150),
        )
        .unwrap_err();
        let elapsed = started.elapsed();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(error.code, crate::error::ErrorCode::Ipc);
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "超时必须及时返回"
        );
        assert!(error.message.contains("已终止子进程"));
    }

    #[test]
    fn run_cli_reports_spawn_failure_as_ipc_error() {
        let config = SessionConfig {
            cli_socket: Some("ignored-socket".to_owned()),
            cli_path: Some("/nonexistent/llmos-cli-probe".to_owned()),
            ..SessionConfig::default()
        };
        let error = run_cli_with_timeout(&config, &[], CLI_RUN_TIMEOUT).unwrap_err();
        assert_eq!(error.code, crate::error::ErrorCode::Ipc);
        assert!(error.message.contains("启动 system-control-cli"));
    }

    /// 写路径自检宿主保护门:仅显式 "1" 放行(深审计 42 D9)。
    #[test]
    fn write_parity_guard_requires_explicit_opt_in() {
        assert!(write_parity_allowed(Some("1".to_owned())));
        assert!(!write_parity_allowed(None));
        assert!(!write_parity_allowed(Some("0".to_owned())));
        assert!(!write_parity_allowed(Some("yes".to_owned())));
        assert!(!write_parity_allowed(Some("".to_owned())));
    }

    /// CLI 失败错误面必须披露 GUI 侧已派发的回执(深审计 42 D9)。
    #[test]
    fn write_parity_cli_failure_discloses_the_dispatched_gui_receipt() {
        let cli_error = DesktopError::ipc("启动 system-control-cli(x)失败: no such file");
        let error = write_parity_cli_failure("deadbeef", &cli_error);
        assert_eq!(error.code, crate::error::ErrorCode::Ipc);
        assert!(error.message.contains("deadbeef"));
        assert!(error.message.contains("已真实派发"));
    }
}
