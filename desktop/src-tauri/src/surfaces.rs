//! W32-F / B2-2 表面呈现核心:Application 声明的 UI Surface 经本地
//! 应用权威(`nlos-application` 的 `ApplicationAuthority`)读回并投影为
//! 桌面可呈现的数据(声明 → 呈现最小链,非窗口管理系统)。
//!
//! 机制镜像 W32-D 的 resource_root 接线:会话配置 `application_root`
//! 指向本地应用权威根目录(`application-authority.db` 所在目录),每次
//! 呈现即时打开(WAL 多进程读安全),不缓存句柄。投影纪律:
//!
//! - **只呈现声明过的事实**:`inspect_surfaces` 的 durable 行逐位携带
//!   应用声明的 surface(id/kind/title/entry),DTO 不发明任何字段;
//! - **stale 代际不呈现**:只呈现登记在应用**当前**安装代际的表面
//!   (`[DUI-WINDOW-001]` 「stale Surface 不得接收新输入」的最小版),
//!   非当前代际与已卸载/停用状态如实投影为空集 + 状态行,不报错;
//! - **未知应用是类型化 NOT_FOUND**:从未安装的包是事实,不是错误。
//!
//! 渲染边界(诚实登记,视图内静态缺口卡同步):载荷内容渲染(entry
//! 引用的 artifact 字节)、焦点/输入路由、多窗口几何——均不在本最小链内。
//! 表面开合终态见 `window_lifecycle`(`REGISTERED`…`CLOSED` 的 open/close)。

use nlos_application::{
    ApplicationAuthority, ApplicationAuthorityError, ApplicationStatus, PackageSurfaceKind,
};
use nlos_types::PackageId;

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
    let presentable = if application.status == ApplicationStatus::Installed {
        registrations
            .iter()
            .filter(|row| row.application_generation == application.current_installation_generation)
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
    Ok(SurfacesPresentationDto {
        application_id_hex: hex(application.application_id.as_bytes()),
        package_id_hex: hex(package_id.as_bytes()),
        application_generation: application.current_installation_generation.get(),
        status: status_name(application.status).to_owned(),
        package_manifest_digest_hex: hex(application.package_manifest_digest.as_bytes()),
        presentable_surfaces: presentable,
    })
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
