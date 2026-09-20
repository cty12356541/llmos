//! `sample-app-driver` — W33-B 第三方样板应用的生命周期驱动（B1-5）。
//!
//! 消费端演示：一个不触任何内核内部 API 的第三方程序，如何只骑公共面
//! 完成 install → run → update → uninstall——
//!
//! - **install**：`nlos-package verify`（W33-A CLI）把包物化进真实
//!   `ArtifactStore` 并产出 verified receipt；本驱动只拿 receipt id 走
//!   `nlos-slice-k` 装配的 `install_verified_package_by_id`（内里是
//!   `ApplicationAuthority::install_application` 的 verify-then-commit 门）。
//! - **run**：`Task`/`Attempt`（durable 行携带 application 关联）→ delegated
//!   `Process` → `CommitPermit` → tokio fiber 的 durable driver Operation
//!   （register→dispatch→complete）+ 应用自有 artifact 的
//!   stage → plan → converge（TaskCommitReceipt）→ background task /
//!   process binding 注册（W27-D 门与 W30-D 链的消费面）。
//! - **update**：同 major 新 revision 的 verified receipt 经 W29-E 迁移
//!   runner：begin → 步骤记录 → 健康检查（probe 恰一次）→ 单事务原子切换。
//! - **uninstall**：先见证 W27-D 真实活动门的 typed 拒绝，再走 W30-D
//!   teardown 链（platform kill → crash terminal → W27-C linkage →
//!   `cancel_task`）过门卸载。
//!
//! 用法：
//!
//! ```text
//! sample-app-driver install   <root> <receipt-id-HEX32>
//! sample-app-driver run       <root> <package-id-HEX32>
//! sample-app-driver update    <root> <package-id-HEX32> <receipt-id-HEX32>
//! sample-app-driver uninstall <root> <package-id-HEX32> [os-pid]
//! ```
//!
//! 退出码：`0` 成功 · `1` 用法 · `2` 权威拒绝或执行失败。
//! 密钥纪律：slice-k 复用助手内部用 seed 带（见各 SEED 常量注释）；本
//! 驱动自有的幂等/时钟键走 `llmos/sample-app/driver-key/v1` 域分隔派生，
//! 与任何 seed 带无碰撞面。

use std::fmt;
use std::process::ExitCode;
use std::sync::Arc;

use nlos_application::{
    ActivateMigrationDecision, ActivatePackageMigrationRequest, ApplicationStatus,
    CompatibilityWindow, MigrateApplicationRequest, MigrateDecision, MigrationHealthContext,
    MigrationHealthProbe, RecordMigrationStepRequest,
};
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, ProvenanceSourceTriple, PutRevisionRequest,
};
use nlos_process::{PlatformKillDecision, RegisterSupervisorPidRequest, SupervisorPidRegistry};
use nlos_runtime::{FiberSpec, RuntimeAdapter as _};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{
    SliceKError, SliceKRuntime, WriteFiberJob, run_application_teardown, short_hex,
    spawn_write_fiber,
};
use nlos_task::{
    ArtifactPublicationExpectation, Authorities, PermitDecision, PermitRequest,
    artifact_publication_plan_root,
};
use nlos_types::{
    ApplicationId, ArtifactId, CallbackId, Generation, IdempotencyKey, PackageId, ReceiptId, TaskId,
};
use sha2::{Digest, Sha256};

const USAGE: &str = "usage: sample-app-driver install <root> <receipt-id-HEX32> \
 | run <root> <package-id-HEX32> \
 | update <root> <package-id-HEX32> <receipt-id-HEX32> \
 | uninstall <root> <package-id-HEX32> [os-pid]";

/// install 阶段 seed 带（slice-k 助手内部键 15/16/21/22 → 0x6A/0x6B/0x70/0x71）。
const SEED_INSTALL: u8 = 0x5B;
/// run 阶段 seed 带（slice-k 助手内部键 20..26/30..33/40..48/50..54/110..114
/// → 0x80..0x86/0x8A..0x8D/0x94..0x9C/0x9E..0xA2/0xDA..0xDE）。
const SEED_RUN: u8 = 0x6C;
/// uninstall 阶段 seed 带（gated 卸载键 17/18 → 0x5F/0x60，与上述各带不相交）。
const SEED_UNINSTALL: u8 = 0x4E;

/// run 阶段的初始台账（输出 artifact 的 revision 1）。
const RUN_LEDGER_V1: &[u8] = b"sample-app run ledger v1\ninitial state after install\n";
/// run 阶段 fiber 发布的输出（输出 artifact 的 revision 2）。
const RUN_OUTPUT_V2: &[u8] =
    b"sample-app run output v1\none driver operation, one staged revision\n";

/// 本驱动自有幂等/时钟键的域分隔符（`teardown_key` 先例：SHA-256 域‖相‖索引）。
const DRIVER_KEY_DOMAIN: &[u8] = b"llmos/sample-app/driver-key/v1";
/// 应用自有输出 artifact id 的域分隔派生。
const OUTPUT_ARTIFACT_DOMAIN: &[u8] = b"llmos/sample-app/output-artifact/v1";

/// Typed 驱动失败：`1` 用法，`2` 权威拒绝或执行失败（main 按此映射）。
enum DriverError {
    Usage,
    Failed(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => write!(formatter, "bad invocation"),
            Self::Failed(reason) => write!(formatter, "{reason}"),
        }
    }
}

macro_rules! from_display {
    ($source:ty) => {
        impl From<$source> for DriverError {
            fn from(source: $source) -> Self {
                Self::Failed(source.to_string())
            }
        }
    };
}

from_display!(SliceKError);
from_display!(nlos_application::ApplicationAuthorityError);
from_display!(nlos_artifact::ArtifactError);
from_display!(nlos_task::TaskStoreError);
from_display!(nlos_process::ProcessAuthorityError);
from_display!(nlos_process::SupervisorPidRegistryError);
from_display!(nlos_runtime::RuntimeError);
from_display!(std::io::Error);

type DriverResult<T> = Result<T, DriverError>;

/// 域分隔驱动键的原始 16 字节（同时喂各类 `from_bytes` 身份构造器）。
fn driver_key_bytes(phase: &[u8], index: u8) -> [u8; 16] {
    let digest = Sha256::new()
        .chain_update(DRIVER_KEY_DOMAIN)
        .chain_update(phase)
        .chain_update([index])
        .finalize();
    let mut key = [0_u8; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

fn driver_key(phase: &[u8], index: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes(driver_key_bytes(phase, index))
}

/// 应用自有输出 artifact 的确定性身份（由 application/task 身份派生，
/// 与任何权威派生 id 域分隔）。
fn output_artifact_id(application_id: ApplicationId, task_id: TaskId) -> ArtifactId {
    let digest = Sha256::new()
        .chain_update(OUTPUT_ARTIFACT_DOMAIN)
        .chain_update(application_id.as_bytes())
        .chain_update(task_id.as_bytes())
        .finalize();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    ArtifactId::from_bytes(id)
}

/// 32 个十六进制字符 → 16 字节（receipt id / package id 通用）。
fn parse_hex16(text: &str, what: &str) -> DriverResult<[u8; 16]> {
    let mut out = [0_u8; 16];
    if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DriverError::Failed(format!(
            "{what} must be exactly 32 hex chars, got {text:?}"
        )));
    }
    for index in 0..16 {
        out[index] = u8::from_str_radix(&text[2 * index..2 * index + 2], 16)
            .map_err(|error| DriverError::Failed(format!("{what}: {error}")))?;
    }
    Ok(out)
}

fn receipt_line(kind: &str, id: &[u8], detail: &str) {
    println!(
        "[sample-app] RECEIPT kind={kind} id={} {detail}",
        short_hex(id)
    );
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let rest: &[String] = arguments.get(1..).unwrap_or(&[]);
    let result = match (arguments.first().map(String::as_str), rest) {
        (Some("install"), [root, receipt]) => cmd_install(root, receipt),
        (Some("run"), [root, package]) => cmd_run(root, package).await,
        (Some("update"), [root, package, receipt]) => cmd_update(root, package, receipt),
        (Some("uninstall"), [root, package]) => cmd_uninstall(root, package, None),
        (Some("uninstall"), [root, package, pid]) => match pid.parse::<u32>() {
            Ok(os_pid) => cmd_uninstall(root, package, Some(os_pid)),
            Err(error) => Err(DriverError::Failed(format!("os-pid: {error}"))),
        },
        _ => Err(DriverError::Usage),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(DriverError::Usage) => {
            eprintln!("{USAGE}");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("sample-app-driver: {error}");
            ExitCode::from(2)
        }
    }
}

/// install：receipt id → `ApplicationAuthority::install_application`
/// （verify-then-commit、digest 七项绑定、单事务 CAS 推进代际）。
fn cmd_install(root: &str, receipt_hex: &str) -> DriverResult<()> {
    let receipt_id = ReceiptId::from_bytes(parse_hex16(receipt_hex, "receipt id")?);
    let runtime = SliceKRuntime::open(root)?;
    let installation = runtime.install_verified_package_by_id(receipt_id, SEED_INSTALL)?;
    println!(
        "[sample-app] INSTALL installation={} application={} package={} generation={} version={} \
             entries={} installer={}",
        short_hex(installation.installation_id.as_bytes()),
        short_hex(installation.application_id.as_bytes()),
        short_hex(installation.package_id.as_bytes()),
        installation.installation_generation.get(),
        installation.package_version,
        installation.entry_count,
        short_hex(installation.installer_principal.as_bytes()),
    );
    receipt_line(
        "installation",
        installation.installation_id.as_bytes(),
        &format!(
            "application={} generation={}",
            short_hex(installation.application_id.as_bytes()),
            installation.installation_generation.get()
        ),
    );
    Ok(())
}

/// run：Task/Attempt/Process → permit → fiber driver operation → 应用自有
/// artifact 的 stage/plan/converge → background task + process binding 注册。
/// Unix 下同时以真实 OS 子进程充当 manifest 声明的 background-service
/// 载荷的 Os 侧替身（teardown 演练会经真实 POSIX kill 链杀死它）。
#[allow(clippy::too_many_lines)]
async fn cmd_run(root: &str, package_hex: &str) -> DriverResult<()> {
    let package_id = PackageId::from_bytes(parse_hex16(package_hex, "package id")?);
    let runtime = Arc::new(SliceKRuntime::open(root)?);
    let adapter = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )?;

    let application = runtime
        .applications
        .inspect_application(package_id)?
        .ok_or_else(|| {
            DriverError::Failed(format!(
                "no application installed for package {package_hex}"
            ))
        })?;
    if application.status != ApplicationStatus::Installed {
        return Err(DriverError::Failed(format!(
            "application {} is not installed (status={})",
            short_hex(application.application_id.as_bytes()),
            status_name(application.status)
        )));
    }
    let application_id = application.application_id;
    let installer_principal = runtime
        .applications
        .list_installations(application_id)?
        .into_iter()
        .max_by_key(|receipt| receipt.installation_generation.get())
        .map(|receipt| receipt.installer_principal)
        .ok_or_else(|| DriverError::Failed("no installation receipt".to_string()))?;
    println!(
        "[sample-app] RUN begin application={} generation={} status={}",
        short_hex(application_id.as_bytes()),
        application.current_installation_generation.get(),
        status_name(application.status)
    );

    // 1) Task/Attempt：durable 任务行携带 application 关联（W30-A 面）。
    let (task_id, attempt_id, scope_id) =
        runtime.register_task_and_attempt_for(SEED_RUN, Some(application_id), None)?;
    let task_row = runtime.tasks.inspect_task(task_id)?;
    println!(
        "[sample-app] RUN task={} attempt={} scope={} associated_application={}",
        short_hex(task_id.as_bytes()),
        short_hex(attempt_id.as_bytes()),
        short_hex(scope_id.as_bytes()),
        task_row
            .application_id
            .is_some_and(|associated| associated == application_id)
    );

    // 2) delegated Process（fiber 只能活在这样的绑定之下）。
    let process =
        runtime.materialize_process(SEED_RUN, task_id, attempt_id, Generation::INITIAL)?;
    println!(
        "[sample-app] RUN process={} agent={}",
        short_hex(process.process_id.as_bytes()),
        short_hex(process.agent_instance_id.as_bytes())
    );

    // 3) 应用自有输出 artifact：revision 1 = 初始台账。
    let artifact_id = output_artifact_id(application_id, task_id);
    let ledger_at_ms = runtime.wall_now_ms(driver_key(b"run", 0))?;
    runtime.artifacts.create_artifact(CreateArtifactSpec {
        artifact_id,
        idempotency_key: driver_key(b"run", 1),
        content_type: "text/plain".to_string(),
        application_id: Some(application_id),
        owner: None,
        created_at_ms: ledger_at_ms,
    })?;
    runtime.artifacts.put_revision(PutRevisionRequest {
        artifact_id,
        expected_head_revision: 0,
        bytes: RUN_LEDGER_V1,
        created_at_ms: ledger_at_ms,
        provenance: ProvenanceSourceTriple {
            source_a: *application_id.as_bytes(),
            source_b: *package_id.as_bytes(),
            source_digest: ContentDigest::of_bytes(RUN_LEDGER_V1),
        },
    })?;

    // 4) CommitPermit：写集 = 一次 artifact 发布期望（target revision 2）。
    let stage_key = driver_key(b"run", 2);
    let expectation = ArtifactPublicationExpectation {
        staging_id: nlos_artifact::staging_id_for(artifact_id, stage_key).into_bytes(),
        artifact_id,
        target_revision: 2,
        digest: ContentDigest::of_bytes(RUN_OUTPUT_V2).into_bytes(),
        size_bytes: u64::try_from(RUN_OUTPUT_V2.len()).unwrap_or(u64::MAX),
    };
    let write_set_root = artifact_publication_plan_root(&[expectation])?;
    let PermitDecision::Issued(permit) = runtime
        .tasks
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: driver_key(b"run", 3),
                valid_until_ms: i64::MAX,
                requested_at_ms: runtime.wall_now_i64(driver_key(b"run", 4))?,
            },
        )?
    else {
        return Err(DriverError::Failed(
            "permit must be issued on a fresh task".to_string(),
        ));
    };
    receipt_line(
        "commit-permit",
        permit.permit_id.as_bytes(),
        &format!(
            "task={} attempt={}",
            short_hex(task_id.as_bytes()),
            short_hex(attempt_id.as_bytes())
        ),
    );

    // 5) fiber：durable driver Operation（register→dispatch→complete）+
    //    permit 下的 stage 与 commit plan。
    let job = WriteFiberJob {
        operation_id: nlos_types::OperationId::from_bytes(driver_key_bytes(b"run", 5)),
        callback_id: CallbackId::from_bytes(driver_key_bytes(b"run", 6)),
        completion_receipt_id: ReceiptId::from_bytes(driver_key_bytes(b"run", 7)),
        expected_head_revision: 1,
        artifact_id,
        stage_key,
        stage_bytes: RUN_OUTPUT_V2.to_vec().into(),
        stage_created_at_ms: runtime.wall_now_ms(driver_key(b"run", 8))?,
        permit: Some(permit.permit_id),
        write_set_root,
        plan_key: driver_key(b"run", 9),
        planned_at_ms: runtime.wall_now_i64(driver_key(b"run", 10))?,
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
    };
    let spec = FiberSpec {
        fiber_id: nlos_types::ExecutionFiberId::from_bytes(driver_key_bytes(b"run", 11)),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: process.agent_instance_id,
        agent_generation: process.agent_instance_generation,
        process_id: process.process_id,
        process_generation: process.process_generation,
        task_attempt_id: Some(attempt_id),
        cancellation_scope_id: scope_id,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: nlos_types::ResourceGroupId::from_bytes(driver_key_bytes(b"run", 12)),
        scheduler_domain_id: nlos_types::SchedulerDomainId::from_bytes(driver_key_bytes(
            b"run", 13,
        )),
        deadline: None,
    };
    let (fiber, receiver) = spawn_write_fiber(Arc::clone(&runtime), &adapter, spec, job)?;
    let outcome = receiver.await.expect("fiber outcome channel")?;
    let Some(plan_id) = outcome.plan_id else {
        return Err(DriverError::Failed(
            "the permit-bound fiber must plan the commit".to_string(),
        ));
    };
    println!(
        "[sample-app] RUN operation={} fiber_state={:?} plan={}",
        short_hex(outcome.operation.operation_id.as_bytes()),
        adapter.inspect(fiber),
        short_hex(plan_id.as_bytes())
    );

    // 6) converge → TaskCommitReceipt（head revision 2 发布）。
    let receipts = runtime.converge_pending(16, runtime.wall_now_i64(driver_key(b"run", 14))?)?;
    let Some(commit) = receipts
        .iter()
        .find(|receipt| receipt.task_receipt.task_id == task_id)
    else {
        return Err(DriverError::Failed(
            "converge produced no commit receipt for the run task".to_string(),
        ));
    };
    let head = runtime
        .artifacts
        .resolve_head(artifact_id, u64::MAX)?
        .ok_or_else(|| DriverError::Failed("output artifact head missing".to_string()))?;
    receipt_line(
        "task-commit",
        commit.task_receipt.receipt_id.as_bytes(),
        &format!(
            "task={} head_commit_seq={} publications={} output_head_revision={}",
            short_hex(task_id.as_bytes()),
            commit.task_receipt.new_head_commit_seq,
            commit.artifact_publications.len(),
            head.revision
        ),
    );

    // 7) 应用注册：background task + process binding（卸载门与 teardown 的消费面）。
    runtime.register_background_task(package_id, task_id, installer_principal, SEED_RUN)?;
    runtime.register_process_binding(
        package_id,
        process.process_id,
        installer_principal,
        SEED_RUN,
    )?;
    let outstanding = runtime.tasks.inspect_outstanding_task_count(&[task_id])?;
    println!(
        "[sample-app] RUN registered background_task={} process_binding={} outstanding_tasks={}",
        short_hex(task_id.as_bytes()),
        short_hex(process.process_id.as_bytes()),
        outstanding
    );

    // 8) Unix：background-service 载荷的 Os 侧替身（真实子进程，存活到
    //    uninstall 的平台 kill 链）。非 Unix 契约道打印自身 pid（noop 适配器
    //    从不真的发信号）。
    #[cfg(unix)]
    let service_os_pid = {
        // stdio 置 null：替身进程不得继承本驱动的输出管道，否则
        // `driver | tee` 在驱动退出后等不到管道 EOF（孤儿持有写端）。
        let child = std::process::Command::new("sleep")
            .arg("600")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| DriverError::Failed(format!("spawn service stand-in: {error}")))?;
        let pid = child.id();
        // 有意不回收：子进程是服务的 Os 替身，生命周期归 uninstall 阶段。
        std::mem::forget(child);
        pid
    };
    #[cfg(not(unix))]
    let service_os_pid = std::process::id();
    println!("[sample-app] RUN service_os_pid={service_os_pid}");
    println!(
        "[sample-app] RUN done application={}",
        short_hex(application_id.as_bytes())
    );
    Ok(())
}

/// 健康检查 probe：真实部署里这里做 config-shape 校验/烟雾启动；样板
/// 声明「目标 revision 可用」并让权威侧的绑定复验（内置）先行。
struct AlwaysHealthy;

impl MigrationHealthProbe for AlwaysHealthy {
    fn target_revision_healthy(&self, context: &MigrationHealthContext<'_>) -> bool {
        println!(
            "[sample-app] UPDATE probe package={} from_generation={} steps={}/{}",
            short_hex(context.package_id.as_bytes()),
            context.from_generation.get(),
            context.completed_step_count,
            context.declared_step_count
        );
        true
    }
}

/// update：W29-E 迁移 runner 全链——begin（冻结基线/目标 + `SameMajor` 窗）→
/// 步骤记录 ×2 → 健康检查（probe 恰一次）→ 单事务原子切换（代际 +1）。
fn cmd_update(root: &str, package_hex: &str, receipt_hex: &str) -> DriverResult<()> {
    let package_id = PackageId::from_bytes(parse_hex16(package_hex, "package id")?);
    let target_receipt = ReceiptId::from_bytes(parse_hex16(receipt_hex, "receipt id")?);
    let runtime = SliceKRuntime::open(root)?;
    let drill = driver_key(b"update", 0);

    let view = match runtime.applications.migrate_application(
        &runtime.artifacts,
        MigrateApplicationRequest {
            package_id,
            package_verification_receipt_id: target_receipt,
            idempotency_key: drill,
            compatibility_window: CompatibilityWindow::SameMajor,
            declared_step_count: 2,
            requested_at_ms: runtime.wall_now_ms(driver_key(b"update", 1))?,
        },
    )? {
        MigrateDecision::Started(view) => {
            println!(
                "[sample-app] UPDATE drill=started from_generation={} from_version={} \
                     target_version={}",
                view.from_generation.get(),
                view.from_package_version,
                view.target_package_version
            );
            view
        }
        MigrateDecision::Replayed(view) => {
            println!("[sample-app] UPDATE drill=replayed state={:?}", view.state);
            view
        }
    };

    for (step, key_index) in [(1_u64, 2_u8), (2, 3)] {
        runtime
            .applications
            .record_migration_step(RecordMigrationStepRequest {
                idempotency_key: drill,
                step_index: step,
                completed_at_ms: runtime.wall_now_ms(driver_key(b"update", key_index))?,
            })?;
        println!("[sample-app] UPDATE step={step} recorded");
    }

    let report = match runtime.applications.run_migration_health_check(
        &runtime.artifacts,
        drill,
        &AlwaysHealthy,
        runtime.wall_now_ms(driver_key(b"update", 4))?,
    )? {
        nlos_application::MigrationHealthDecision::Recorded(report)
        | nlos_application::MigrationHealthDecision::Replayed(report) => report,
    };
    println!(
        "[sample-app] UPDATE health passed={} checked_at_ms={}",
        report.passed, report.checked_at_ms
    );

    let installation = match runtime.applications.activate_package_migration(
        &runtime.artifacts,
        ActivatePackageMigrationRequest {
            idempotency_key: drill,
            activated_at_ms: runtime.wall_now_ms(driver_key(b"update", 5))?,
        },
    )? {
        ActivateMigrationDecision::Activated(receipt)
        | ActivateMigrationDecision::Replayed(receipt) => receipt,
    };
    let application = runtime
        .applications
        .inspect_application(package_id)?
        .ok_or_else(|| DriverError::Failed("application vanished mid-update".to_string()))?;
    println!(
        "[sample-app] UPDATE installation={} generation={} manifest={} application_status={}",
        short_hex(installation.installation_id.as_bytes()),
        installation.installation_generation.get(),
        short_hex(installation.package_manifest_digest.as_bytes()),
        status_name(application.status)
    );
    receipt_line(
        "installation",
        installation.installation_id.as_bytes(),
        &format!(
            "generation={} via=migration-activate",
            installation.installation_generation.get()
        ),
    );
    if application.status != ApplicationStatus::Installed {
        return Err(DriverError::Failed(
            "application must remain installed after the atomic switch".to_string(),
        ));
    }
    if installation.installation_generation.get() != view.from_generation.get() + 1 {
        return Err(DriverError::Failed(
            "the atomic switch must advance exactly one generation".to_string(),
        ));
    }
    Ok(())
}

/// uninstall：先见证 W27-D 真实活动门的 typed 拒绝，再走 W30-D teardown 链
/// （platform kill → crash terminal → W27-C linkage → `cancel_task`）过门卸载。
fn cmd_uninstall(root: &str, package_hex: &str, os_pid: Option<u32>) -> DriverResult<()> {
    let package_id = PackageId::from_bytes(parse_hex16(package_hex, "package id")?);
    let runtime = Arc::new(SliceKRuntime::open(root)?);
    let adapter = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )?;

    // supervisor 内存 registry 的既定重启模式：卸载时对存活 binding 重注册
    // pid（run 阶段输出、脚本携带；未给则用自身 pid——非 Unix noop 契约道）。
    let stand_in_pid = os_pid.unwrap_or_else(std::process::id);
    let registry = SupervisorPidRegistry::new();
    let registrations = runtime.inspect_application_registrations(package_id)?;
    for binding in &registrations.process_bindings {
        let active = runtime
            .process
            .inspect_active_process_binding(binding.process_id)?;
        registry.register(RegisterSupervisorPidRequest {
            process_id: binding.process_id,
            process_generation: active.process_generation,
            os_pid: stand_in_pid,
            registered_at_ms: runtime.wall_now_ms(driver_key(b"uninstall", 0))?,
        })?;
    }
    println!(
        "[sample-app] UNINSTALL registered process_bindings={} background_tasks={} os_pid={}",
        registrations.process_bindings.len(),
        registrations.background_tasks.len(),
        stand_in_pid
    );

    // W27-D 真实活动门：注册的后台 Task 未收敛，卸载必须 typed 拒绝。
    match runtime.uninstall_application_gated_by_task_activity(package_id, SEED_UNINSTALL) {
        Err(SliceKError::Application(
            nlos_application::ApplicationAuthorityError::ApplicationActiveTasksRunning {
                active_task_count,
                ..
            },
        )) => {
            println!("[sample-app] GATE_REFUSED active_task_count={active_task_count}");
        }
        Err(other) => return Err(other.into()),
        Ok(receipt) => {
            return Err(DriverError::Failed(format!(
                "the activity gate unexpectedly opened before teardown (application_generation={})",
                receipt.application_generation.get()
            )));
        }
    }

    // W30-D teardown 链 + 过门卸载（replay 时 kill 凭证在适配器调用前短路）。
    let teardown =
        run_application_teardown(&runtime, &adapter, package_id, SEED_UNINSTALL, &registry)?;
    println!(
        "[sample-app] UNINSTALL kills={} crashes={} linkages={} task_cancels={}",
        teardown.kills.len(),
        teardown.crashes.len(),
        teardown.linkages.len(),
        teardown.task_cancels.len()
    );
    for (index, kill) in teardown.kills.iter().enumerate() {
        let decision = if matches!(kill, PlatformKillDecision::Signaled(_)) {
            "signaled"
        } else {
            "replayed"
        };
        println!("[sample-app] UNINSTALL kill#{index} decision={decision}");
    }
    for (index, cancel) in teardown.task_cancels.iter().enumerate() {
        println!("[sample-app] UNINSTALL cancel#{index} decision={cancel:?}");
    }
    receipt_line(
        "uninstall",
        teardown.uninstall.idempotency_key.as_bytes(),
        &format!(
            "application={} application_generation={} uninstalled_at_ms={}",
            short_hex(teardown.application_id.as_bytes()),
            teardown.uninstall.application_generation.get(),
            teardown.uninstall.uninstalled_at_ms
        ),
    );

    let application = runtime
        .applications
        .inspect_application(package_id)?
        .ok_or_else(|| DriverError::Failed("application row vanished".to_string()))?;
    println!(
        "[sample-app] UNINSTALL done application={} status={}",
        short_hex(application.application_id.as_bytes()),
        status_name(application.status)
    );
    if application.status != ApplicationStatus::Uninstalled {
        return Err(DriverError::Failed(
            "application must be durably uninstalled after the teardown chain".to_string(),
        ));
    }
    Ok(())
}

fn status_name(status: ApplicationStatus) -> &'static str {
    match status {
        ApplicationStatus::Installed => "installed",
        ApplicationStatus::Disabled => "disabled",
        ApplicationStatus::Uninstalled => "uninstalled",
    }
}
