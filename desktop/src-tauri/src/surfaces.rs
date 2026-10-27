//! W32-F / B2-2 表面呈现核心:Application 声明的 UI Surface 经本地
//! 应用权威(`nlos-application` 的 `ApplicationAuthority`)读回并投影为
//! 桌面可呈现的数据(声明 → 呈现最小链,非窗口管理系统)。
//!
//! 机制镜像 W32-D 的 resource_root 接线:会话配置 `application_root`
//! 指向本地应用权威根目录(`application-authority.db` 所在目录),每次
//! 呈现即时打开(WAL 多进程读安全),不缓存句柄。投影纪律:
//!
//! - **只呈现声明过的事实**:`inspect_surfaces` 的 durable 行逐位携带
//!   应用声明的 surface(id/kind/title/entry);`inspect_process_bindings`
//!   与 `inspect_background_tasks` 的回执同样逐字段投影。DTO 不发明任何字段;
//! - **stale 代际不呈现**:只呈现登记在应用**当前**安装代际的表面
//!   (`[DUI-WINDOW-001]` 「stale Surface 不得接收新输入」的最小版),
//!   非当前代际与已卸载/停用状态如实投影为空集 + 状态行,不报错;
//! - **未知应用是类型化 NOT_FOUND**:从未安装的包是事实,不是错误。
//!
//! Stage D (`[DUI-LAUNCH-001]`) 扩展：`resolve_launch_from_authority` 面向
//! `ApplicationLauncher` 按 `(ApplicationId, InstallationId)` 执行权威安装与表面解析，
//! 并显式检测/拒绝将 `PackageId` 或瞬态 `ProcessId` 冒充为应用身份的调用。

use std::fmt;

use nlos_application::{
    ApplicationAuthority, ApplicationAuthorityError, ApplicationStatus, PackageSurfaceKind,
};
use nlos_types::{ApplicationId, InstallationId, PackageId};

use crate::dto::hex;
use crate::error::{DesktopError, ErrorCode};

/// One presentable surface: the durable declared fact, projected.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentedSurfaceDto {
    pub surface_id_hex: String,
    /// `"window" | "panel"`——声明的呈现种类。
    pub kind: String,
    pub title: String,
    /// 声明的内容引用(manifest entry 名);`None` = 元数据-only 声明。
    pub entry_name: Option<String>,
    /// 登记命令的幂等键(呈现证据的回溯锚)。
    pub registration_key_hex: String,
    pub registered_at_ms: u64,
}

/// One durable process binding projected for the surface view.
///
/// Fields are the `inspect_process_bindings` receipt, hex-encoded. Stale
/// generations are omitted the same way stale surfaces are.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentedProcessBindingDto {
    pub process_id_hex: String,
    pub registrant_principal_hex: String,
    pub application_generation: u64,
    pub registration_key_hex: String,
    pub registered_at_ms: u64,
}

/// One durable background-task registration projected for the surface view.
///
/// Fields are the `inspect_background_tasks` receipt, hex-encoded. Stale
/// generations are omitted the same way stale process bindings are.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentedBackgroundTaskDto {
    pub task_id_hex: String,
    pub registrant_principal_hex: String,
    pub application_generation: u64,
    pub registration_key_hex: String,
    pub registered_at_ms: u64,
}

/// One application's presentation facts: current durable state plus the
/// presentable surface set.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfacesPresentationDto {
    pub application_id_hex: String,
    pub package_id_hex: String,
    pub application_generation: u64,
    /// `"installed" | "disabled" | "uninstalled"`。
    pub status: String,
    pub package_manifest_digest_hex: String,
    /// 当前代际的可呈现表面(声明序);stale 代际/非 installed 状态为空。
    pub presentable_surfaces: Vec<PresentedSurfaceDto>,
    /// 当前代际的进程绑定(登记序);stale 代际/非 installed 状态为空。
    pub process_bindings: Vec<PresentedProcessBindingDto>,
    /// 当前代际的后台任务登记(登记序);stale 代际/非 installed 状态为空。
    pub background_tasks: Vec<PresentedBackgroundTaskDto>,
}

fn status_name(status: ApplicationStatus) -> &'static str {
    match status {
        ApplicationStatus::Installed => "installed",
        ApplicationStatus::Disabled => "disabled",
        ApplicationStatus::Uninstalled => "uninstalled",
    }
}

fn kind_name(kind: PackageSurfaceKind) -> &'static str {
    match kind {
        PackageSurfaceKind::Window => "window",
        PackageSurfaceKind::Panel => "panel",
    }
}

fn authority_failure(error: ApplicationAuthorityError) -> DesktopError {
    DesktopError::new(ErrorCode::Internal, format!("本地应用权威读失败:{error}"))
}

/// W32-F:打开会话配置指向的本地应用权威(未配置/空白 → 类型化 Config
/// 拒绝——表面呈现没有未接线回退形态,不配置即不呈现)。每次呈现即时
/// 打开(WAL 多进程读安全),进程内不缓存句柄(与 W32-D resource_root
/// 同款纪律)。
///
/// # Errors
///
/// Typed `CONFIG` when `application_root` is unset/blank or the authority
/// cannot be opened.
pub fn open_configured_application_authority(
    application_root: Option<&str>,
) -> Result<ApplicationAuthority, DesktopError> {
    let Some(root) = application_root
        .map(str::trim)
        .filter(|root| !root.is_empty())
    else {
        return Err(DesktopError::config(
            "表面呈现需要 application_root(本地应用权威根目录;「连接配置」页或 LLMOS_DESKTOP_APPLICATION_ROOT 提供)",
        ));
    };
    ApplicationAuthority::open(root)
        .map_err(|error| DesktopError::config(format!("打开本地应用权威失败({root}):{error}")))
}

/// The presentation core (pure over one open authority; shared by the
/// Tauri command and the integration tests).
///
/// # Errors
///
/// Typed `NOT_FOUND` for a package that was never installed; typed
/// `INTERNAL` for storage failures.
pub fn present_surfaces_core(
    authority: &ApplicationAuthority,
    package_id: PackageId,
) -> Result<SurfacesPresentationDto, DesktopError> {
    let application = authority
        .inspect_application(package_id)
        .map_err(authority_failure)?
        .ok_or_else(|| {
            DesktopError::new(
                crate::error::ErrorCode::NotFound,
                format!(
                    "包 {} 从未安装,无可呈现表面(事实读回,非错误重试面)",
                    hex(package_id.as_bytes())
                ),
            )
        })?;
    let registrations = authority
        .inspect_surfaces(package_id)
        .map_err(authority_failure)?;
    let bindings = authority
        .inspect_process_bindings(package_id)
        .map_err(authority_failure)?;
    let tasks = authority
        .inspect_background_tasks(package_id)
        .map_err(authority_failure)?;
    let current = application.status == ApplicationStatus::Installed;
    let generation = application.current_installation_generation;
    let presentable = if current {
        registrations
            .iter()
            .filter(|row| row.application_generation == generation)
            .map(|row| PresentedSurfaceDto {
                surface_id_hex: hex(&row.surface_id),
                kind: kind_name(row.kind).to_owned(),
                title: row.title.clone(),
                entry_name: row.entry_name.clone(),
                registration_key_hex: hex(row.idempotency_key.as_bytes()),
                registered_at_ms: row.registered_at_ms,
            })
            .collect()
    } else {
        Vec::new()
    };
    let process_bindings = if current {
        bindings
            .iter()
            .filter(|row| row.application_generation == generation)
            .map(|row| PresentedProcessBindingDto {
                process_id_hex: hex(row.process_id.as_bytes()),
                registrant_principal_hex: hex(row.registrant_principal.as_bytes()),
                application_generation: row.application_generation.get(),
                registration_key_hex: hex(row.idempotency_key.as_bytes()),
                registered_at_ms: row.registered_at_ms,
            })
            .collect()
    } else {
        Vec::new()
    };
    let background_tasks = if current {
        tasks
            .iter()
            .filter(|row| row.application_generation == generation)
            .map(|row| PresentedBackgroundTaskDto {
                task_id_hex: hex(row.task_id.as_bytes()),
                registrant_principal_hex: hex(row.registrant_principal.as_bytes()),
                application_generation: row.application_generation.get(),
                registration_key_hex: hex(row.idempotency_key.as_bytes()),
                registered_at_ms: row.registered_at_ms,
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(SurfacesPresentationDto {
        application_id_hex: hex(application.application_id.as_bytes()),
        package_id_hex: hex(package_id.as_bytes()),
        application_generation: generation.get(),
        status: status_name(application.status).to_owned(),
        package_manifest_digest_hex: hex(application.package_manifest_digest.as_bytes()),
        presentable_surfaces: presentable,
        process_bindings,
        background_tasks,
    })
}

/// `[DUI-LAUNCH-001]` 权威应用启动解析失败原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorityLaunchResolutionError {
    /// 调用方把 `PackageId` 冒充为 `ApplicationId` 或 `InstallationId`。
    PackageIdNotApplicationIdentity { offending_id: [u8; 16] },
    /// 调用方把瞬态 `ProcessId` 冒充为 `ApplicationId` 或 `InstallationId`。
    ProcessIdNotApplicationIdentity { offending_id: [u8; 16] },
    /// 请求的 `InstallationId` 在权威安装回执表中不存在。
    InstallationNotFound { installation_id: [u8; 16] },
    /// `InstallationId` 所属 `ApplicationId` 与调用方声明的不匹配。
    ApplicationInstallationMismatch {
        requested_application_id: [u8; 16],
        receipt_application_id: [u8; 16],
        installation_id: [u8; 16],
    },
    /// 应用处于 `disabled` 或 `uninstalled`，拒绝激活。
    ApplicationNotInstalled {
        application_id: [u8; 16],
        status: String,
    },
    /// 请求的 `InstallationId` 属于已被更新取代的旧代际。
    StaleInstallationGeneration {
        application_id: [u8; 16],
        installation_id: [u8; 16],
        package_id: [u8; 16],
        requested_generation: u64,
        current_generation: u64,
    },
    /// 当前安装代际尚未登记任何可呈现表面。
    NoPresentableSurfaces {
        application_id: [u8; 16],
        generation: u64,
    },
    /// 底层存储或读取错误。
    Storage(String),
}

impl fmt::Display for AuthorityLaunchResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PackageIdNotApplicationIdentity { offending_id } => write!(
                formatter,
                "PACKAGE_ID_NOT_APPLICATION_IDENTITY: {} is a PackageId, not ApplicationId/InstallationId ([DUI-LAUNCH-001])",
                hex(offending_id)
            ),
            Self::ProcessIdNotApplicationIdentity { offending_id } => write!(
                formatter,
                "PROCESS_ID_NOT_APPLICATION_IDENTITY: {} is a registered ProcessId, not ApplicationId/InstallationId ([DUI-LAUNCH-001])",
                hex(offending_id)
            ),
            Self::InstallationNotFound { installation_id } => write!(
                formatter,
                "INSTALLATION_NOT_FOUND: installation {} does not exist",
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
                "APPLICATION_NOT_INSTALLED: application {} status is '{status}'",
                hex(application_id)
            ),
            Self::StaleInstallationGeneration {
                application_id,
                installation_id,
                requested_generation,
                current_generation,
                ..
            } => write!(
                formatter,
                "STALE_INSTALLATION_GENERATION: installation {} of application {} is generation {requested_generation}, current is {current_generation}",
                hex(installation_id),
                hex(application_id)
            ),
            Self::NoPresentableSurfaces {
                application_id,
                generation,
            } => write!(
                formatter,
                "NO_PRESENTABLE_SURFACES: application {} generation {generation} has no registered surfaces",
                hex(application_id)
            ),
            Self::Storage(detail) => write!(formatter, "AUTHORITY_STORAGE_ERROR: {detail}"),
        }
    }
}

impl std::error::Error for AuthorityLaunchResolutionError {}

/// `[DUI-LAUNCH-001]` 经 `ApplicationAuthority` 完整核验后的应用启动事实包。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAuthorityLaunch {
    pub application_id: ApplicationId,
    pub installation_id: InstallationId,
    pub package_id: PackageId,
    pub current_installation_generation: u64,
    pub known_process_ids: Vec<[u8; 16]>,
    pub presentation: SurfacesPresentationDto,
}

fn check_process_bindings_for_spoof(
    authority: &ApplicationAuthority,
    package_id: PackageId,
    candidate_app: [u8; 16],
    candidate_inst: [u8; 16],
    collected_pids: &mut Vec<[u8; 16]>,
) -> Result<(), AuthorityLaunchResolutionError> {
    let bindings = authority
        .inspect_process_bindings(package_id)
        .map_err(|err| AuthorityLaunchResolutionError::Storage(err.to_string()))?;
    for binding in &bindings {
        let pid = *binding.process_id.as_bytes();
        if !collected_pids.contains(&pid) {
            collected_pids.push(pid);
        }
        if candidate_app == pid {
            return Err(
                AuthorityLaunchResolutionError::ProcessIdNotApplicationIdentity {
                    offending_id: candidate_app,
                },
            );
        }
        if candidate_inst == pid {
            return Err(
                AuthorityLaunchResolutionError::ProcessIdNotApplicationIdentity {
                    offending_id: candidate_inst,
                },
            );
        }
    }
    Ok(())
}

/// `[DUI-LAUNCH-001]`：严格按 `(ApplicationId, InstallationId)` 从本地 `ApplicationAuthority`
/// 解析当前安装代际的可呈现表面，并拒绝把 `PackageId` 或已登记 `ProcessId` 冒充为应用身份。
///
/// # Errors
///
/// 返回类型化 [`AuthorityLaunchResolutionError`]。
pub fn resolve_launch_from_authority(
    authority: &ApplicationAuthority,
    application_id: ApplicationId,
    installation_id: InstallationId,
) -> Result<ResolvedAuthorityLaunch, AuthorityLaunchResolutionError> {
    let app_bytes = *application_id.as_bytes();
    let inst_bytes = *installation_id.as_bytes();

    // 1. 检测调用方是否将已登记的 PackageId 传入 application_id 或 installation_id。
    if authority
        .inspect_application(PackageId::from_bytes(app_bytes))
        .map_err(|err| AuthorityLaunchResolutionError::Storage(err.to_string()))?
        .is_some()
    {
        return Err(
            AuthorityLaunchResolutionError::PackageIdNotApplicationIdentity {
                offending_id: app_bytes,
            },
        );
    }
    if authority
        .inspect_application(PackageId::from_bytes(inst_bytes))
        .map_err(|err| AuthorityLaunchResolutionError::Storage(err.to_string()))?
        .is_some()
    {
        return Err(
            AuthorityLaunchResolutionError::PackageIdNotApplicationIdentity {
                offending_id: inst_bytes,
            },
        );
    }

    let mut known_process_ids = Vec::new();

    // 2. 若 application_id 存在安装历史，交叉检查其包与已登记 ProcessId（拦截 `(valid_app_id, process_id)`）。
    let history = authority
        .list_installations(application_id)
        .map_err(|err| AuthorityLaunchResolutionError::Storage(err.to_string()))?;
    for entry in &history {
        let pkg_bytes = *entry.package_id.as_bytes();
        if app_bytes == pkg_bytes || inst_bytes == pkg_bytes {
            return Err(
                AuthorityLaunchResolutionError::PackageIdNotApplicationIdentity {
                    offending_id: pkg_bytes,
                },
            );
        }
        check_process_bindings_for_spoof(
            authority,
            entry.package_id,
            app_bytes,
            inst_bytes,
            &mut known_process_ids,
        )?;
    }

    // 3. 按 InstallationId 读取不可变安装回执。
    let receipt = authority
        .inspect_installation(installation_id)
        .map_err(|err| match err {
            ApplicationAuthorityError::InstallationNotFound(_) => {
                AuthorityLaunchResolutionError::InstallationNotFound {
                    installation_id: inst_bytes,
                }
            }
            other => AuthorityLaunchResolutionError::Storage(other.to_string()),
        })?;

    // 4. 在真实包上再次交叉核对 ProcessId（拦截 `(process_id, valid_installation_id)`）。
    check_process_bindings_for_spoof(
        authority,
        receipt.package_id,
        app_bytes,
        inst_bytes,
        &mut known_process_ids,
    )?;

    if receipt.application_id != application_id {
        return Err(
            AuthorityLaunchResolutionError::ApplicationInstallationMismatch {
                requested_application_id: app_bytes,
                receipt_application_id: *receipt.application_id.as_bytes(),
                installation_id: inst_bytes,
            },
        );
    }

    // 5. 核验应用当前状态与当前安装代际。
    let app_view = authority
        .inspect_application(receipt.package_id)
        .map_err(|err| AuthorityLaunchResolutionError::Storage(err.to_string()))?
        .ok_or(AuthorityLaunchResolutionError::InstallationNotFound {
            installation_id: inst_bytes,
        })?;

    if app_view.status != ApplicationStatus::Installed {
        return Err(AuthorityLaunchResolutionError::ApplicationNotInstalled {
            application_id: app_bytes,
            status: status_name(app_view.status).to_owned(),
        });
    }

    if receipt.installation_generation != app_view.current_installation_generation {
        return Err(
            AuthorityLaunchResolutionError::StaleInstallationGeneration {
                application_id: app_bytes,
                installation_id: inst_bytes,
                package_id: *receipt.package_id.as_bytes(),
                requested_generation: receipt.installation_generation.get(),
                current_generation: app_view.current_installation_generation.get(),
            },
        );
    }

    // 6. 读取当前安装代际的可呈现表面。
    let presentation = present_surfaces_core(authority, receipt.package_id)
        .map_err(|err| AuthorityLaunchResolutionError::Storage(err.message))?;

    if presentation.presentable_surfaces.is_empty() {
        return Err(AuthorityLaunchResolutionError::NoPresentableSurfaces {
            application_id: app_bytes,
            generation: app_view.current_installation_generation.get(),
        });
    }

    Ok(ResolvedAuthorityLaunch {
        application_id,
        installation_id,
        package_id: receipt.package_id,
        current_installation_generation: app_view.current_installation_generation.get(),
        known_process_ids,
        presentation,
    })
}

/// 按 `PackageId` 探查权威最新代际与状态（用于已打开窗口的升级代际同步围栏）。
///
/// # Errors
///
/// 存储读取失败返回 [`DesktopError`]。
pub fn inspect_current_generation_by_package(
    authority: &ApplicationAuthority,
    package_id: PackageId,
) -> Result<Option<(ApplicationId, u64, ApplicationStatus)>, DesktopError> {
    authority
        .inspect_application(package_id)
        .map(|opt| {
            opt.map(|view| {
                (
                    view.application_id,
                    view.current_installation_generation.get(),
                    view.status,
                )
            })
        })
        .map_err(authority_failure)
}

#[cfg(test)]
mod tests {
    use super::present_surfaces_core;
    use crate::error::ErrorCode;

    #[test]
    fn status_and_kind_names_cover_the_declared_sets() {
        assert_eq!(
            super::status_name(nlos_application::ApplicationStatus::Installed),
            "installed"
        );
        assert_eq!(
            super::kind_name(nlos_application::PackageSurfaceKind::Window),
            "window"
        );
        assert_eq!(
            super::kind_name(nlos_application::PackageSurfaceKind::Panel),
            "panel"
        );
    }

    #[test]
    fn unknown_package_is_typed_not_found() {
        let dir = std::env::temp_dir().join(format!("llmosdt-surface-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let authority = nlos_application::ApplicationAuthority::open(&dir).expect("open");
        let error =
            present_surfaces_core(&authority, nlos_types::PackageId::from_bytes([0xEE; 16]))
                .expect_err("unknown package must refuse");
        assert_eq!(error.code, ErrorCode::NotFound);
        std::fs::remove_dir_all(&dir).ok();
    }
}
