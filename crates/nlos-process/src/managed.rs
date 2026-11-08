//! W82-C 托管约定 C 的受管 worker 注入与回收面。
//!
//! 三变量白名单:受管 worker 子进程环境 = `env_clear()` 后恰好三个
//! `LLMOS_*` 变量(spec §4.2)——子进程无 PATH(裸解释器名由编排器在
//! 父侧解析为绝对路径),父环境零继承。升级式回收:组 SIGTERM → 宽限
//! 轮询直子 → 组 SIGKILL → 有界组排空核验,at-least-once(重放安全)。
//! Unix 依赖 Task 2 的 `process_group(0)` + `killpg`;Windows 沿用
//! `taskkill /F /T`(单级强杀,无宽限二级,平台差异如实登记)。

use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::time::Duration;

use nlos_types::{Generation, ProcessId};

use crate::supervisor::{
    ProcessSupervisor, SpawnSupervisedRequest, SupervisedSpawn, SupervisorError,
    SupervisorSignalOutcome,
};
use crate::supervisor_pid::KillScope;

/// 认证 IPC 端点(Unix socket / Windows 命名管道路径字符串)。
pub const MANAGED_ENV_AUTH_ENDPOINT: &str = "LLMOS_AUTH_ENDPOINT";
/// 进程身份种子(当前约定为 operator 种子文件路径;见 spec §8)。
pub const MANAGED_ENV_PROCESS_IDENTITY_SEED: &str = "LLMOS_PROCESS_IDENTITY_SEED";
/// 根能力槽位(字节透传;格式契约 TODO 对表 W82-B 设计)。
pub const MANAGED_ENV_ROOT_CAPABILITY_SLOT: &str = "LLMOS_ROOT_CAPABILITY_SLOT";

const DRAIN_PROBE_ATTEMPTS: u32 = 200;
const DRAIN_PROBE_STEP: Duration = Duration::from_millis(10);
const GRACE_POLL_STEP: Duration = Duration::from_millis(20);

/// 一次受管拉起注入的三变量值(顺序即注入顺序)。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorkerEnv {
    pub auth_endpoint: String,
    pub process_identity_seed: String,
    pub root_capability_slot: String,
}

impl ManagedWorkerEnv {
    /// Ordered `(NAME, VALUE)` pairs — exactly three, no more, no less.
    #[must_use]
    pub fn entries(&self) -> [(String, String); 3] {
        [
            (
                MANAGED_ENV_AUTH_ENDPOINT.to_owned(),
                self.auth_endpoint.clone(),
            ),
            (
                MANAGED_ENV_PROCESS_IDENTITY_SEED.to_owned(),
                self.process_identity_seed.clone(),
            ),
            (
                MANAGED_ENV_ROOT_CAPABILITY_SLOT.to_owned(),
                self.root_capability_slot.clone(),
            ),
        ]
    }
}

/// 构造最小权限命令:`env_clear` + 恰三变量 + cwd(unix 分组由
/// [`spawn_managed_worker`] 追加)。纯构造,可跨平台单测(`get_envs`)。
#[must_use]
pub fn prepare_managed_command(
    program: &str,
    argv: &[String],
    cwd: &Path,
    env: &ManagedWorkerEnv,
) -> Command {
    let mut command = Command::new(program);
    command.args(argv);
    command.current_dir(cwd);
    command.env_clear();
    for (name, value) in env.entries() {
        command.env(name, value);
    }
    command
}

/// 分组拉起一个受管 worker:三变量白名单 + 进程组组长(KillScope 记录)。
///
/// # Errors
///
/// 透传 [`ProcessSupervisor::spawn_supervised_grouped`] 的失败
/// (宿主 spawn 失败,或注册拒绝——此时子进程已被拆干净)。
pub fn spawn_managed_worker(
    supervisor: &ProcessSupervisor,
    request: SpawnSupervisedRequest,
    program: &str,
    argv: &[String],
    cwd: &Path,
    env: &ManagedWorkerEnv,
) -> Result<SupervisedSpawn, SupervisorError> {
    let mut command = prepare_managed_command(program, argv, cwd, env);
    supervisor.spawn_supervised_grouped(request, &mut command)
}

/// 升级式组回收的终端结果分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedKillOutcome {
    /// 首发组信号时目标仍存活(后续按回执 escalated 区分是否强杀)。
    Terminated,
    /// 首发组信号时目标组已整体不存在(ESRCH / taskkill 128)。
    AlreadyTerminated,
}

/// 一次受管 worker 组回收的 at-least-once 回执。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedKillReceipt {
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub os_pid: u32,
    pub outcome: ManagedKillOutcome,
    /// 宽限超时后是否升级到组 SIGKILL(Windows 单级强杀恒 false)。
    pub escalated: bool,
    /// 组排空核验:反复组信号至 ESRCH(有界次数内)。
    pub group_drained: bool,
    pub exit_status: Option<ExitStatus>,
}

/// 升级式组回收:组 SIGTERM → 宽限轮询直子 → (未退)组 SIGKILL →
/// 有界组排空核验。at-least-once:任一步重放安全;首发 `AlreadyTerminated`
/// 仍会执行排空核验(直子先亡、孙进程残留的场景由此覆盖)。
///
/// 注意:直子在宽限内自行退出即进入排空核验——孙进程在直子退出瞬间
/// 被强杀收尾(整组已收过 SIGTERM;回收优先于孙进程的额外宽限,
/// spec §4.4 契约)。
///
/// # Errors
///
/// 代际栅栏拒绝(未知映射/陈旧代际/Process scope 的 `kill_force` 门槛)
/// 或宿主信号失败——均为 fail-closed,调用方回执如实记录。
pub fn terminate_group_with_escalation(
    supervisor: &ProcessSupervisor,
    process_id: ProcessId,
    expected_generation: Generation,
    child: &mut Child,
    grace: Duration,
) -> Result<ManagedKillReceipt, SupervisorError> {
    let os_pid = child.id();
    let first = supervisor.kill(process_id, expected_generation)?;
    let outcome = match first {
        SupervisorSignalOutcome::Applied => ManagedKillOutcome::Terminated,
        SupervisorSignalOutcome::AlreadyTerminated => ManagedKillOutcome::AlreadyTerminated,
    };

    let mut exit_status = None;
    let mut waited = false;
    let mut elapsed = Duration::ZERO;
    while elapsed < grace {
        if let Some(status) = child.try_wait().map_err(SupervisorError::Spawn)? {
            exit_status = Some(status);
            waited = true;
            break;
        }
        std::thread::sleep(GRACE_POLL_STEP);
        elapsed += GRACE_POLL_STEP;
    }
    let mut escalated = false;
    if !waited {
        supervisor.kill_force(process_id, expected_generation)?;
        escalated = true;
        exit_status = Some(child.wait().map_err(SupervisorError::Spawn)?);
    }

    let mut group_drained = false;
    for _ in 0..DRAIN_PROBE_ATTEMPTS {
        match supervisor.kill_force(process_id, expected_generation) {
            Ok(SupervisorSignalOutcome::AlreadyTerminated) => {
                group_drained = true;
                break;
            }
            Ok(SupervisorSignalOutcome::Applied) => {
                std::thread::sleep(DRAIN_PROBE_STEP);
            }
            Err(error) => return Err(error),
        }
    }

    Ok(ManagedKillReceipt {
        process_id,
        process_generation: expected_generation,
        os_pid,
        outcome,
        escalated,
        group_drained,
        exit_status,
    })
}

// KillScope 仅出现在 doc 链接中;保持导入用于文档不变量锚点。
const _: fn(KillScope) = |scope| {
    let _ = scope;
};
