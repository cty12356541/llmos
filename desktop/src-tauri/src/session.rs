//! Stage D (`[DUI-SESSION-001]`) 多用户工作站会话、独立数据/密钥命名空间与工作区容器状态机。
//!
//! 权威契约对齐 `06-架构设计总纲-v0.5.md` §25.4 与 `ADR-0025` 决定 1 / 决定 3：
//! - `DesktopSession`: `LOCKED → ACTIVE ↔ SUSPENDED → CLOSED`；
//! - `Workspace`: `CREATED → ACTIVE ↔ ARCHIVED → CLOSED`（`ARCHIVED`/`CLOSED` 拒绝新工作准入）；
//! - 每次 `lock()`、`suspend()`、`switch_user()` 或 `close()` 严格单调递增 `session_generation`，
//!   并立即撤销全部旧会话签发的瞬态 `SessionScopedCapabilityGrant`；
//! - `UserProfile.security_seal_hint` 私密防钓鱼短语严格隔离在宿主可信态内，绝不进入 `Z1..Z3`/`Z2`
//!   公共投影，仅由 `Z4` 可信安全确认屏投影读取；
//! - 跨用户访问或激活他人 `Workspace` 律以类型化 `SessionError::CrossUserWorkspaceAccessDenied` 拒绝。

use std::collections::BTreeMap;
use std::fmt;

use crate::dto::hex;

/// 桌面工作站语言设置（对齐双语字典 `zh-CN` / `en-US`）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum WorkstationLocale {
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en-US")]
    EnUs,
}

impl WorkstationLocale {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::EnUs => "en-US",
        }
    }

    /// 解析语言标识字符串。
    ///
    /// # Errors
    ///
    /// 非受支持语言返回 [`SessionError::InvalidLocale`]。
    pub fn parse(raw: &str) -> Result<Self, SessionError> {
        match raw.trim() {
            "zh-CN" | "zh_CN" | "zh-cn" => Ok(Self::ZhCn),
            "en-US" | "en_US" | "en-us" => Ok(Self::EnUs),
            other => Err(SessionError::InvalidLocale(other.to_owned())),
        }
    }
}

/// 工业工作站双主题设置（对齐 `ADR-0025` 决定 3 `Graphite / Paper`）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum WorkstationTheme {
    #[default]
    #[serde(rename = "graphite-workstation")]
    GraphiteWorkstation,
    #[serde(rename = "paper-workstation")]
    PaperWorkstation,
    #[serde(rename = "system")]
    System,
}

impl WorkstationTheme {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GraphiteWorkstation => "graphite-workstation",
            Self::PaperWorkstation => "paper-workstation",
            Self::System => "system",
        }
    }

    /// 解析主题标识字符串。
    ///
    /// # Errors
    ///
    /// 非受支持主题返回 [`SessionError::InvalidTheme`]。
    pub fn parse(raw: &str) -> Result<Self, SessionError> {
        match raw.trim() {
            "graphite-workstation" | "graphite_workstation" | "graphite" => {
                Ok(Self::GraphiteWorkstation)
            }
            "paper-workstation" | "paper_workstation" | "paper" => Ok(Self::PaperWorkstation),
            "system" => Ok(Self::System),
            other => Err(SessionError::InvalidTheme(other.to_owned())),
        }
    }
}

/// 确定性域分离 128-bit FNV-1a 派生（仅用于本地会话级命名空间默认派生，不冒充密码学摘要）。
#[must_use]
pub(crate) fn derive_scoped_id_16(domain: &[u8], first: &[u8], second: &[u8]) -> [u8; 16] {
    fn fnv64(seed: u64, chunks: &[&[u8]]) -> u64 {
        let mut hash = seed;
        for chunk in chunks {
            for &byte in *chunk {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
        hash
    }
    let hi = fnv64(0xcbf2_9ce4_8422_2325, &[domain, &[0x01], first, second]);
    let lo = fnv64(0x8422_2325_cbf2_9ce4, &[domain, &[0x02], second, first]);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&hi.to_be_bytes());
    out[8..].copy_from_slice(&lo.to_be_bytes());
    out
}

/// 桌面多用户档案（`[DUI-SESSION-001]`）。
///
/// `security_seal_hint` 字段保持模块私有，从类型层面防止任何 `Z1..Z3`/`Z2` 公共投影意外读取或序列化；
/// 仅能通过显式 [`UserProfile::z4_trusted_seal_hint`] 由宿主 `Z4` 安全确认屏投影读取。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserProfile {
    pub principal_id: [u8; 16],
    pub display_name: String,
    security_seal_hint: String,
    pub locale: WorkstationLocale,
    pub theme: WorkstationTheme,
    pub data_container_scope_id: [u8; 16],
    pub keychain_namespace_id: [u8; 16],
}

impl UserProfile {
    /// 构造显式隔离作用域的用户档案。
    ///
    /// # Errors
    ///
    /// 空白显示名、空白防伪短语或数据容器与密钥命名空间冲突时返回 [`SessionError`]。
    pub fn new(
        principal_id: [u8; 16],
        display_name: impl Into<String>,
        security_seal_hint: impl Into<String>,
        locale: WorkstationLocale,
        theme: WorkstationTheme,
        data_container_scope_id: [u8; 16],
        keychain_namespace_id: [u8; 16],
    ) -> Result<Self, SessionError> {
        let display_name = display_name.into().trim().to_owned();
        if display_name.is_empty() {
            return Err(SessionError::EmptyDisplayName);
        }
        let security_seal_hint = security_seal_hint.into().trim().to_owned();
        if security_seal_hint.is_empty() {
            return Err(SessionError::EmptySecuritySealHint);
        }
        if data_container_scope_id == keychain_namespace_id {
            return Err(SessionError::ContainerScopeCollision { principal_id });
        }
        Ok(Self {
            principal_id,
            display_name,
            security_seal_hint,
            locale,
            theme,
            data_container_scope_id,
            keychain_namespace_id,
        })
    }

    /// 为指定 `principal_id` 确定性派生独立 `data_container_scope_id` 与 `keychain_namespace_id`。
    ///
    /// # Errors
    ///
    /// 空白显示名或空白防伪短语时返回 [`SessionError`]。
    pub fn with_isolated_scopes(
        principal_id: [u8; 16],
        display_name: impl Into<String>,
        security_seal_hint: impl Into<String>,
        locale: WorkstationLocale,
        theme: WorkstationTheme,
    ) -> Result<Self, SessionError> {
        let data_scope = derive_scoped_id_16(
            b"llmos/desktop/session/data-container/v1",
            &principal_id,
            &[],
        );
        let keychain_ns = derive_scoped_id_16(
            b"llmos/desktop/session/keychain-namespace/v1",
            &principal_id,
            &[],
        );
        Self::new(
            principal_id,
            display_name,
            security_seal_hint,
            locale,
            theme,
            data_scope,
            keychain_ns,
        )
    }

    #[must_use]
    pub fn principal_id_hex(&self) -> String {
        hex(&self.principal_id)
    }

    #[must_use]
    pub fn data_container_scope_id_hex(&self) -> String {
        hex(&self.data_container_scope_id)
    }

    #[must_use]
    pub fn keychain_namespace_id_hex(&self) -> String {
        hex(&self.keychain_namespace_id)
    }

    /// 仅供宿主 `Z4` 系统安全确认屏读取私密防钓鱼标识。
    #[must_use]
    pub fn z4_trusted_seal_hint(&self) -> &str {
        &self.security_seal_hint
    }
}

/// 桌面会话生命周期：`LOCKED → ACTIVE ↔ SUSPENDED → CLOSED`。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DesktopSessionState {
    Locked,
    Active,
    Suspended,
    Closed,
}

impl DesktopSessionState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Locked => "LOCKED",
            Self::Active => "ACTIVE",
            Self::Suspended => "SUSPENDED",
            Self::Closed => "CLOSED",
        }
    }

    /// `LOCKED → ACTIVE`。
    ///
    /// # Errors
    ///
    /// `CLOSED` 返回 [`SessionError::AlreadyClosed`]；其余非 `LOCKED` 状态返回非法跃迁错误。
    pub fn unlock(self) -> Result<Self, SessionError> {
        match self {
            Self::Locked => Ok(Self::Active),
            Self::Closed => Err(SessionError::AlreadyClosed),
            Self::Active | Self::Suspended => Err(SessionError::InvalidSessionTransition {
                from: self,
                attempted: "unlock",
            }),
        }
    }

    /// `ACTIVE | SUSPENDED → LOCKED`。
    ///
    /// # Errors
    ///
    /// `CLOSED` 返回 [`SessionError::AlreadyClosed`]；已在 `LOCKED` 返回非法跃迁错误。
    pub fn lock(self) -> Result<Self, SessionError> {
        match self {
            Self::Active | Self::Suspended => Ok(Self::Locked),
            Self::Closed => Err(SessionError::AlreadyClosed),
            Self::Locked => Err(SessionError::InvalidSessionTransition {
                from: self,
                attempted: "lock",
            }),
        }
    }

    /// `ACTIVE → SUSPENDED`。
    ///
    /// # Errors
    ///
    /// `CLOSED` 返回 [`SessionError::AlreadyClosed`]；非 `ACTIVE` 返回非法跃迁错误。
    pub fn suspend(self) -> Result<Self, SessionError> {
        match self {
            Self::Active => Ok(Self::Suspended),
            Self::Closed => Err(SessionError::AlreadyClosed),
            Self::Locked | Self::Suspended => Err(SessionError::InvalidSessionTransition {
                from: self,
                attempted: "suspend",
            }),
        }
    }

    /// `SUSPENDED → ACTIVE`。
    ///
    /// # Errors
    ///
    /// `CLOSED` 返回 [`SessionError::AlreadyClosed`]；非 `SUSPENDED` 返回非法跃迁错误。
    pub fn resume(self) -> Result<Self, SessionError> {
        match self {
            Self::Suspended => Ok(Self::Active),
            Self::Closed => Err(SessionError::AlreadyClosed),
            Self::Locked | Self::Active => Err(SessionError::InvalidSessionTransition {
                from: self,
                attempted: "resume",
            }),
        }
    }

    /// `LOCKED | ACTIVE | SUSPENDED → CLOSED`（终态）。
    ///
    /// # Errors
    ///
    /// 已在 `CLOSED` 时返回 [`SessionError::AlreadyClosed`]。
    pub fn close(self) -> Result<Self, SessionError> {
        match self {
            Self::Closed => Err(SessionError::AlreadyClosed),
            Self::Locked | Self::Active | Self::Suspended => Ok(Self::Closed),
        }
    }
}

/// 工作区容器生命周期：`CREATED → ACTIVE ↔ ARCHIVED → CLOSED`。
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkspaceState {
    Created,
    Active,
    Archived,
    Closed,
}

impl WorkspaceState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Active => "ACTIVE",
            Self::Archived => "ARCHIVED",
            Self::Closed => "CLOSED",
        }
    }

    /// 仅 `ACTIVE` 工作区允许接纳新窗口或后台任务；`ARCHIVED` 与 `CLOSED` 严格锁定准入。
    #[must_use]
    pub const fn can_admit_work(self) -> bool {
        matches!(self, Self::Active)
    }

    /// `CREATED | ARCHIVED | ACTIVE → ACTIVE`。
    ///
    /// # Errors
    ///
    /// `CLOSED` 返回 [`SessionError::WorkspaceClosed`]。
    pub fn activate(self) -> Result<Self, SessionError> {
        match self {
            Self::Created | Self::Active | Self::Archived => Ok(Self::Active),
            Self::Closed => Err(SessionError::WorkspaceClosed),
        }
    }

    /// `ACTIVE | ARCHIVED → ARCHIVED`。
    ///
    /// # Errors
    ///
    /// `CLOSED` 返回 [`SessionError::WorkspaceClosed`]；`CREATED` 尚未激活不可直接归档。
    pub fn archive(self) -> Result<Self, SessionError> {
        match self {
            Self::Active | Self::Archived => Ok(Self::Archived),
            Self::Created => Err(SessionError::InvalidWorkspaceTransition {
                from: self,
                attempted: "archive",
            }),
            Self::Closed => Err(SessionError::WorkspaceClosed),
        }
    }

    /// `CREATED | ACTIVE | ARCHIVED → CLOSED`。
    ///
    /// # Errors
    ///
    /// 已在 `CLOSED` 时返回 [`SessionError::WorkspaceClosed`]。
    pub fn close(self) -> Result<Self, SessionError> {
        match self {
            Self::Closed => Err(SessionError::WorkspaceClosed),
            Self::Created | Self::Active | Self::Archived => Ok(Self::Closed),
        }
    }
}

/// 单用户隔离工作区实例。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionWorkspace {
    pub workspace_id: [u8; 16],
    pub owner_principal_id: [u8; 16],
    pub name: String,
    pub state: WorkspaceState,
    pub revision: u64,
    pub layout_mode: String,
}

impl SessionWorkspace {
    #[must_use]
    pub fn workspace_id_hex(&self) -> String {
        hex(&self.workspace_id)
    }

    #[must_use]
    pub fn owner_principal_id_hex(&self) -> String {
        hex(&self.owner_principal_id)
    }
}

/// 绑定特定会话代际、Principal 与 Workspace 的瞬态桌面能力凭证（`[DUI-SESSION-001]`）。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionScopedCapabilityGrant {
    pub grant_id: [u8; 16],
    pub owner_principal_id: [u8; 16],
    pub workspace_id: [u8; 16],
    pub issued_session_generation: u64,
    pub revoked: bool,
}

impl SessionScopedCapabilityGrant {
    #[must_use]
    pub const fn is_active_for(
        &self,
        principal_id: [u8; 16],
        workspace_id: [u8; 16],
        current_session_generation: u64,
        session_state: DesktopSessionState,
    ) -> bool {
        !self.revoked
            && matches!(session_state, DesktopSessionState::Active)
            && self.issued_session_generation == current_session_generation
            && const_eq_16(self.owner_principal_id, principal_id)
            && const_eq_16(self.workspace_id, workspace_id)
    }
}

const fn const_eq_16(lhs: [u8; 16], rhs: [u8; 16]) -> bool {
    let mut idx = 0;
    while idx < 16 {
        if lhs[idx] != rhs[idx] {
            return false;
        }
        idx += 1;
    }
    true
}

/// 会话与多用户工作区类型化错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    AlreadyClosed,
    InvalidSessionTransition {
        from: DesktopSessionState,
        attempted: &'static str,
    },
    SessionNotActive {
        state: DesktopSessionState,
    },
    EmptyDisplayName,
    EmptySecuritySealHint,
    InvalidLocale(String),
    InvalidTheme(String),
    ContainerScopeCollision {
        principal_id: [u8; 16],
    },
    UserProfileNotFound {
        principal_id: [u8; 16],
    },
    WorkspaceNotFound {
        workspace_id: [u8; 16],
    },
    WorkspaceAlreadyExists {
        workspace_id: [u8; 16],
    },
    WorkspaceClosed,
    InvalidWorkspaceTransition {
        from: WorkspaceState,
        attempted: &'static str,
    },
    WorkspaceAdmissionLocked {
        workspace_id: [u8; 16],
        state: WorkspaceState,
    },
    CrossUserWorkspaceAccessDenied {
        workspace_id: [u8; 16],
        owner_principal_id: [u8; 16],
        caller_principal_id: [u8; 16],
    },
    GrantNotFound {
        grant_id: [u8; 16],
    },
    GrantRevokedOrStale {
        grant_id: [u8; 16],
        issued_session_generation: u64,
        current_session_generation: u64,
    },
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyClosed => formatter.write_str("desktop session is already CLOSED"),
            Self::InvalidSessionTransition { from, attempted } => write!(
                formatter,
                "illegal desktop session transition '{attempted}' from {}",
                from.as_str()
            ),
            Self::SessionNotActive { state } => write!(
                formatter,
                "desktop session must be ACTIVE, currently {}",
                state.as_str()
            ),
            Self::EmptyDisplayName => {
                formatter.write_str("user profile display_name cannot be empty")
            }
            Self::EmptySecuritySealHint => {
                formatter.write_str("user profile security_seal_hint cannot be empty")
            }
            Self::InvalidLocale(raw) => write!(formatter, "unsupported workstation locale: {raw}"),
            Self::InvalidTheme(raw) => write!(formatter, "unsupported workstation theme: {raw}"),
            Self::ContainerScopeCollision { principal_id } => write!(
                formatter,
                "user container scope or keychain namespace collision for principal {}",
                hex(principal_id)
            ),
            Self::UserProfileNotFound { principal_id } => write!(
                formatter,
                "user profile not registered for principal {}",
                hex(principal_id)
            ),
            Self::WorkspaceNotFound { workspace_id } => {
                write!(formatter, "workspace {} not found", hex(workspace_id))
            }
            Self::WorkspaceAlreadyExists { workspace_id } => {
                write!(formatter, "workspace {} already exists", hex(workspace_id))
            }
            Self::WorkspaceClosed => formatter.write_str("workspace is already CLOSED"),
            Self::InvalidWorkspaceTransition { from, attempted } => write!(
                formatter,
                "illegal workspace transition '{attempted}' from {}",
                from.as_str()
            ),
            Self::WorkspaceAdmissionLocked {
                workspace_id,
                state,
            } => write!(
                formatter,
                "workspace {} is {} and refuses new window/task admission",
                hex(workspace_id),
                state.as_str()
            ),
            Self::CrossUserWorkspaceAccessDenied {
                workspace_id,
                owner_principal_id,
                caller_principal_id,
            } => write!(
                formatter,
                "cross-user workspace access denied: workspace {} belongs to {}, caller is {}",
                hex(workspace_id),
                hex(owner_principal_id),
                hex(caller_principal_id)
            ),
            Self::GrantNotFound { grant_id } => {
                write!(
                    formatter,
                    "transient session grant {} not found",
                    hex(grant_id)
                )
            }
            Self::GrantRevokedOrStale {
                grant_id,
                issued_session_generation,
                current_session_generation,
            } => write!(
                formatter,
                "transient session grant {} is revoked or stale (issued gen {issued_session_generation}, current gen {current_session_generation})",
                hex(grant_id)
            ),
        }
    }
}

impl std::error::Error for SessionError {}

/// 多用户 `UserProfile`、隔离 `Workspace` 与单调代际 `DesktopSession` 管理器。
#[derive(Clone, Debug)]
pub struct DesktopSessionManager {
    state: DesktopSessionState,
    session_generation: u64,
    active_principal_id: [u8; 16],
    active_workspace_by_user: BTreeMap<[u8; 16], [u8; 16]>,
    users: BTreeMap<[u8; 16], UserProfile>,
    workspaces: BTreeMap<[u8; 16], SessionWorkspace>,
    grants: BTreeMap<[u8; 16], SessionScopedCapabilityGrant>,
}

impl DesktopSessionManager {
    /// 以初始 `LOCKED` 态（`session_generation = 1`）创建会话管理器。
    ///
    /// # Errors
    ///
    /// 若初始工作区名称为空或初始状态不合法则返回 [`SessionError`]。
    pub fn new_locked(
        initial_user: UserProfile,
        initial_workspace_id: [u8; 16],
        initial_workspace_name: impl Into<String>,
    ) -> Result<Self, SessionError> {
        Self::new_with_state(
            initial_user,
            initial_workspace_id,
            initial_workspace_name,
            DesktopSessionState::Locked,
        )
    }

    /// 以初始 `ACTIVE` 态（`session_generation = 1`）创建工作站会话管理器。
    ///
    /// # Errors
    ///
    /// 若初始工作区配置非法则返回 [`SessionError`]。
    pub fn new_active(
        initial_user: UserProfile,
        initial_workspace_id: [u8; 16],
        initial_workspace_name: impl Into<String>,
    ) -> Result<Self, SessionError> {
        Self::new_with_state(
            initial_user,
            initial_workspace_id,
            initial_workspace_name,
            DesktopSessionState::Active,
        )
    }

    fn new_with_state(
        initial_user: UserProfile,
        initial_workspace_id: [u8; 16],
        initial_workspace_name: impl Into<String>,
        initial_state: DesktopSessionState,
    ) -> Result<Self, SessionError> {
        let name = initial_workspace_name.into().trim().to_owned();
        let workspace_name = if name.is_empty() {
            "默认工作区".to_owned()
        } else {
            name
        };
        let principal_id = initial_user.principal_id;
        let activated_ws_state = WorkspaceState::Created.activate()?;
        let workspace = SessionWorkspace {
            workspace_id: initial_workspace_id,
            owner_principal_id: principal_id,
            name: workspace_name,
            state: activated_ws_state,
            revision: 1,
            layout_mode: "three-pane-workstation".to_owned(),
        };
        let mut users = BTreeMap::new();
        users.insert(principal_id, initial_user);
        let mut workspaces = BTreeMap::new();
        workspaces.insert(initial_workspace_id, workspace);
        let mut active_workspace_by_user = BTreeMap::new();
        active_workspace_by_user.insert(principal_id, initial_workspace_id);
        Ok(Self {
            state: initial_state,
            session_generation: 1,
            active_principal_id: principal_id,
            active_workspace_by_user,
            users,
            workspaces,
            grants: BTreeMap::new(),
        })
    }

    /// 默认本地单机工作站会话（用于 `AppState` 默认启动，无 `unwrap`）。
    #[must_use]
    pub fn default_workstation() -> Self {
        let principal_id = [0xA1; 16];
        let workspace_id = [0xB1; 16];
        let data_scope = derive_scoped_id_16(
            b"llmos/desktop/session/data-container/v1",
            &principal_id,
            &[],
        );
        let keychain_ns = derive_scoped_id_16(
            b"llmos/desktop/session/keychain-namespace/v1",
            &principal_id,
            &[],
        );
        let profile = UserProfile {
            principal_id,
            display_name: "本地操作员".to_owned(),
            security_seal_hint: "青松-07".to_owned(),
            locale: WorkstationLocale::ZhCn,
            theme: WorkstationTheme::GraphiteWorkstation,
            data_container_scope_id: data_scope,
            keychain_namespace_id: keychain_ns,
        };
        let workspace = SessionWorkspace {
            workspace_id,
            owner_principal_id: principal_id,
            name: "主工作区".to_owned(),
            state: WorkspaceState::Active,
            revision: 1,
            layout_mode: "three-pane-workstation".to_owned(),
        };
        let mut users = BTreeMap::new();
        users.insert(principal_id, profile);
        let mut workspaces = BTreeMap::new();
        workspaces.insert(workspace_id, workspace);
        let mut active_workspace_by_user = BTreeMap::new();
        active_workspace_by_user.insert(principal_id, workspace_id);
        Self {
            state: DesktopSessionState::Active,
            session_generation: 1,
            active_principal_id: principal_id,
            active_workspace_by_user,
            users,
            workspaces,
            grants: BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn state(&self) -> DesktopSessionState {
        self.state
    }

    #[must_use]
    pub const fn session_generation(&self) -> u64 {
        self.session_generation
    }

    #[must_use]
    pub const fn active_principal_id(&self) -> [u8; 16] {
        self.active_principal_id
    }

    /// 读取当前活跃用户档案。
    ///
    /// # Errors
    ///
    /// 档案不存在时返回 [`SessionError::UserProfileNotFound`]。
    pub fn active_user(&self) -> Result<&UserProfile, SessionError> {
        self.users
            .get(&self.active_principal_id)
            .ok_or(SessionError::UserProfileNotFound {
                principal_id: self.active_principal_id,
            })
    }

    /// 读取当前活跃用户的活动工作区。
    ///
    /// # Errors
    ///
    /// 工作区不存在或跨用户越界时返回 [`SessionError`]。
    pub fn active_workspace(&self) -> Result<&SessionWorkspace, SessionError> {
        let workspace_id = self
            .active_workspace_by_user
            .get(&self.active_principal_id)
            .copied()
            .ok_or(SessionError::WorkspaceNotFound {
                workspace_id: [0u8; 16],
            })?;
        self.inspect_workspace_for_active_user(workspace_id)
    }

    /// 枚举当前活跃用户拥有的全部工作区（严禁泄漏其他用户的私有工作区列表）。
    #[must_use]
    pub fn workspaces_for_active_user(&self) -> Vec<&SessionWorkspace> {
        self.workspaces
            .values()
            .filter(|ws| ws.owner_principal_id == self.active_principal_id)
            .collect()
    }

    /// 注册额外用户档案及其独立默认工作区。
    ///
    /// 强制校验证书/数据容器/密钥命名空间与其他用户零碰撞。
    ///
    /// # Errors
    ///
    /// 若数据容器命名空间碰撞或默认工作区 ID 冲突则返回 [`SessionError`]。
    pub fn register_user_with_default_workspace(
        &mut self,
        profile: UserProfile,
        default_workspace_id: [u8; 16],
        default_workspace_name: impl Into<String>,
    ) -> Result<(), SessionError> {
        for (existing_principal, existing_user) in &self.users {
            if *existing_principal != profile.principal_id
                && (existing_user.data_container_scope_id == profile.data_container_scope_id
                    || existing_user.keychain_namespace_id == profile.keychain_namespace_id
                    || existing_user.data_container_scope_id == profile.keychain_namespace_id
                    || existing_user.keychain_namespace_id == profile.data_container_scope_id)
            {
                return Err(SessionError::ContainerScopeCollision {
                    principal_id: profile.principal_id,
                });
            }
        }
        if let Some(existing_ws) = self.workspaces.get(&default_workspace_id)
            && existing_ws.owner_principal_id != profile.principal_id
        {
            return Err(SessionError::WorkspaceAlreadyExists {
                workspace_id: default_workspace_id,
            });
        }
        let principal_id = profile.principal_id;
        let raw_name = default_workspace_name.into().trim().to_owned();
        let name = if raw_name.is_empty() {
            "默认工作区".to_owned()
        } else {
            raw_name
        };
        self.users.insert(principal_id, profile);
        self.workspaces
            .entry(default_workspace_id)
            .or_insert(SessionWorkspace {
                workspace_id: default_workspace_id,
                owner_principal_id: principal_id,
                name,
                state: WorkspaceState::Active,
                revision: 1,
                layout_mode: "three-pane-workstation".to_owned(),
            });
        self.active_workspace_by_user
            .entry(principal_id)
            .or_insert(default_workspace_id);
        Ok(())
    }

    fn bump_generation_and_revoke_transient_grants(&mut self) {
        self.session_generation = self.session_generation.saturating_add(1);
        for grant in self.grants.values_mut() {
            grant.revoked = true;
        }
    }

    /// 解锁会话：`LOCKED → ACTIVE`。
    ///
    /// # Errors
    ///
    /// 非法源状态或已关闭会话返回 [`SessionError`]。
    pub fn unlock(&mut self) -> Result<u64, SessionError> {
        self.state = self.state.unlock()?;
        Ok(self.session_generation)
    }

    /// 锁定会话：`ACTIVE | SUSPENDED → LOCKED`，严格递增 `session_generation` 并撤销全部瞬态授权。
    ///
    /// # Errors
    ///
    /// 非法源状态或已关闭会话返回 [`SessionError`]。
    pub fn lock(&mut self) -> Result<u64, SessionError> {
        self.state = self.state.lock()?;
        self.bump_generation_and_revoke_transient_grants();
        Ok(self.session_generation)
    }

    /// 挂起会话：`ACTIVE → SUSPENDED`，严格递增 `session_generation` 并撤销全部瞬态授权。
    ///
    /// # Errors
    ///
    /// 非法源状态或已关闭会话返回 [`SessionError`]。
    pub fn suspend(&mut self) -> Result<u64, SessionError> {
        self.state = self.state.suspend()?;
        self.bump_generation_and_revoke_transient_grants();
        Ok(self.session_generation)
    }

    /// 恢复会话：`SUSPENDED → ACTIVE`。
    ///
    /// # Errors
    ///
    /// 非法源状态或已关闭会话返回 [`SessionError`]。
    pub fn resume(&mut self) -> Result<u64, SessionError> {
        self.state = self.state.resume()?;
        Ok(self.session_generation)
    }

    /// 切换活跃用户档案：严格递增 `session_generation` 并使全部旧会话瞬态能力立即失效。
    ///
    /// 若切换前处于 `SUSPENDED`，切换后进入 `LOCKED` 以防止未解锁接管；`ACTIVE` 与 `LOCKED` 保持对应状态。
    ///
    /// # Errors
    ///
    /// 会话已 `CLOSED` 或目标用户未注册时返回 [`SessionError`]。
    pub fn switch_user(&mut self, target_principal_id: [u8; 16]) -> Result<u64, SessionError> {
        if self.state == DesktopSessionState::Closed {
            return Err(SessionError::AlreadyClosed);
        }
        if !self.users.contains_key(&target_principal_id) {
            return Err(SessionError::UserProfileNotFound {
                principal_id: target_principal_id,
            });
        }
        if self.state == DesktopSessionState::Suspended {
            self.state = DesktopSessionState::Locked;
        }
        self.active_principal_id = target_principal_id;
        self.bump_generation_and_revoke_transient_grants();
        Ok(self.session_generation)
    }

    /// 关闭会话：`→ CLOSED`（终态），严格递增 `session_generation` 并撤销全部瞬态授权。
    ///
    /// # Errors
    ///
    /// 已在 `CLOSED` 时返回 [`SessionError::AlreadyClosed`]。
    pub fn close(&mut self) -> Result<u64, SessionError> {
        self.state = self.state.close()?;
        self.bump_generation_and_revoke_transient_grants();
        Ok(self.session_generation)
    }

    /// 为当前活跃用户创建新工作区（初始 `CREATED`，可选立即激活为 `ACTIVE`）。
    ///
    /// # Errors
    ///
    /// 会话已关闭或 `workspace_id` 已存在时返回 [`SessionError`]。
    pub fn create_workspace(
        &mut self,
        workspace_id: [u8; 16],
        name: impl Into<String>,
        activate_immediately: bool,
    ) -> Result<&SessionWorkspace, SessionError> {
        if self.state == DesktopSessionState::Closed {
            return Err(SessionError::AlreadyClosed);
        }
        if self.workspaces.contains_key(&workspace_id) {
            return Err(SessionError::WorkspaceAlreadyExists { workspace_id });
        }
        let raw_name = name.into().trim().to_owned();
        let clean_name = if raw_name.is_empty() {
            "新建工作区".to_owned()
        } else {
            raw_name
        };
        let mut ws_state = WorkspaceState::Created;
        if activate_immediately {
            ws_state = ws_state.activate()?;
            self.active_workspace_by_user
                .insert(self.active_principal_id, workspace_id);
        }
        let ws = SessionWorkspace {
            workspace_id,
            owner_principal_id: self.active_principal_id,
            name: clean_name,
            state: ws_state,
            revision: 1,
            layout_mode: "three-pane-workstation".to_owned(),
        };
        self.workspaces.insert(workspace_id, ws);
        self.workspaces
            .get(&workspace_id)
            .ok_or(SessionError::WorkspaceNotFound { workspace_id })
    }

    /// 按当前活跃用户权限检视指定工作区（跨用户访问返回 `CrossUserWorkspaceAccessDenied`）。
    ///
    /// # Errors
    ///
    /// 工作区不存在或不属于当前用户时返回 [`SessionError`]。
    pub fn inspect_workspace_for_active_user(
        &self,
        workspace_id: [u8; 16],
    ) -> Result<&SessionWorkspace, SessionError> {
        let ws = self
            .workspaces
            .get(&workspace_id)
            .ok_or(SessionError::WorkspaceNotFound { workspace_id })?;
        if ws.owner_principal_id != self.active_principal_id {
            return Err(SessionError::CrossUserWorkspaceAccessDenied {
                workspace_id,
                owner_principal_id: ws.owner_principal_id,
                caller_principal_id: self.active_principal_id,
            });
        }
        Ok(ws)
    }

    /// 激活当前用户所属工作区并将其设为当前活动工作区。
    ///
    /// # Errors
    ///
    /// 跨用户访问、会话已关闭或工作区已 `CLOSED` 时返回 [`SessionError`]。
    pub fn activate_workspace(
        &mut self,
        workspace_id: [u8; 16],
    ) -> Result<&SessionWorkspace, SessionError> {
        if self.state == DesktopSessionState::Closed {
            return Err(SessionError::AlreadyClosed);
        }
        let caller = self.active_principal_id;
        let ws = self
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(SessionError::WorkspaceNotFound { workspace_id })?;
        if ws.owner_principal_id != caller {
            return Err(SessionError::CrossUserWorkspaceAccessDenied {
                workspace_id,
                owner_principal_id: ws.owner_principal_id,
                caller_principal_id: caller,
            });
        }
        let next = ws.state.activate()?;
        if ws.state != next {
            ws.state = next;
            ws.revision = ws.revision.saturating_add(1);
        }
        self.active_workspace_by_user.insert(caller, workspace_id);
        self.workspaces
            .get(&workspace_id)
            .ok_or(SessionError::WorkspaceNotFound { workspace_id })
    }

    /// 归档当前用户所属工作区（`ACTIVE → ARCHIVED`，归档后禁止新窗口/任务准入）。
    ///
    /// # Errors
    ///
    /// 跨用户访问或状态非法时返回 [`SessionError`]。
    pub fn archive_workspace(
        &mut self,
        workspace_id: [u8; 16],
    ) -> Result<&SessionWorkspace, SessionError> {
        if self.state == DesktopSessionState::Closed {
            return Err(SessionError::AlreadyClosed);
        }
        let caller = self.active_principal_id;
        let ws = self
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(SessionError::WorkspaceNotFound { workspace_id })?;
        if ws.owner_principal_id != caller {
            return Err(SessionError::CrossUserWorkspaceAccessDenied {
                workspace_id,
                owner_principal_id: ws.owner_principal_id,
                caller_principal_id: caller,
            });
        }
        let next = ws.state.archive()?;
        if ws.state != next {
            ws.state = next;
            ws.revision = ws.revision.saturating_add(1);
        }
        self.workspaces
            .get(&workspace_id)
            .ok_or(SessionError::WorkspaceNotFound { workspace_id })
    }

    /// 关闭当前用户所属工作区（`→ CLOSED` 终态）。
    ///
    /// # Errors
    ///
    /// 跨用户访问或已在 `CLOSED` 时返回 [`SessionError`]。
    pub fn close_workspace(
        &mut self,
        workspace_id: [u8; 16],
    ) -> Result<&SessionWorkspace, SessionError> {
        if self.state == DesktopSessionState::Closed {
            return Err(SessionError::AlreadyClosed);
        }
        let caller = self.active_principal_id;
        let ws = self
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(SessionError::WorkspaceNotFound { workspace_id })?;
        if ws.owner_principal_id != caller {
            return Err(SessionError::CrossUserWorkspaceAccessDenied {
                workspace_id,
                owner_principal_id: ws.owner_principal_id,
                caller_principal_id: caller,
            });
        }
        ws.state = ws.state.close()?;
        ws.revision = ws.revision.saturating_add(1);
        self.workspaces
            .get(&workspace_id)
            .ok_or(SessionError::WorkspaceNotFound { workspace_id })
    }

    /// 同步更新当前活动工作区的布局模式记录。
    pub fn sync_active_workspace_layout_mode(&mut self, layout_mode: &str) {
        if let Some(ws_id) = self
            .active_workspace_by_user
            .get(&self.active_principal_id)
            .copied()
            && let Some(ws) = self.workspaces.get_mut(&ws_id)
            && ws.owner_principal_id == self.active_principal_id
            && ws.layout_mode != layout_mode
        {
            ws.layout_mode = layout_mode.to_owned();
            ws.revision = ws.revision.saturating_add(1);
        }
    }

    /// 在当前 `ACTIVE` 会话与可准入工作区内签发瞬态能力凭证。
    ///
    /// # Errors
    ///
    /// 会话非 `ACTIVE`、跨用户访问工作区或工作区已归档/关闭时返回 [`SessionError`]。
    pub fn issue_transient_grant(
        &mut self,
        grant_id: [u8; 16],
        workspace_id: [u8; 16],
    ) -> Result<SessionScopedCapabilityGrant, SessionError> {
        if self.state != DesktopSessionState::Active {
            return Err(SessionError::SessionNotActive { state: self.state });
        }
        let ws = self.inspect_workspace_for_active_user(workspace_id)?;
        if !ws.state.can_admit_work() {
            return Err(SessionError::WorkspaceAdmissionLocked {
                workspace_id,
                state: ws.state,
            });
        }
        let grant = SessionScopedCapabilityGrant {
            grant_id,
            owner_principal_id: self.active_principal_id,
            workspace_id,
            issued_session_generation: self.session_generation,
            revoked: false,
        };
        self.grants.insert(grant_id, grant.clone());
        Ok(grant)
    }

    /// 校验会话瞬态能力凭证在当前会话代际与当前用户/工作区下是否仍有效。
    ///
    /// # Errors
    ///
    /// 会话非 `ACTIVE`、跨用户越界、凭证不存在或因锁屏/挂起/切用户已失效时返回 [`SessionError`]。
    pub fn validate_transient_grant(
        &self,
        grant_id: [u8; 16],
        workspace_id: [u8; 16],
    ) -> Result<&SessionScopedCapabilityGrant, SessionError> {
        if self.state != DesktopSessionState::Active {
            return Err(SessionError::SessionNotActive { state: self.state });
        }
        self.inspect_workspace_for_active_user(workspace_id)?;
        let grant = self
            .grants
            .get(&grant_id)
            .ok_or(SessionError::GrantNotFound { grant_id })?;
        if grant.owner_principal_id != self.active_principal_id
            || grant.workspace_id != workspace_id
        {
            return Err(SessionError::CrossUserWorkspaceAccessDenied {
                workspace_id,
                owner_principal_id: grant.owner_principal_id,
                caller_principal_id: self.active_principal_id,
            });
        }
        if !grant.is_active_for(
            self.active_principal_id,
            workspace_id,
            self.session_generation,
            self.state,
        ) {
            return Err(SessionError::GrantRevokedOrStale {
                grant_id,
                issued_session_generation: grant.issued_session_generation,
                current_session_generation: self.session_generation,
            });
        }
        Ok(grant)
    }
}
