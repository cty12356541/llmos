//! The three longitudinal scenarios of the slice, composed once and shared
//! by the integration tests and the demo bin: the happy chain
//! (sign → verify → install → task → permit → fiber → converge), the
//! cancel path, and the crash-recovery split (durable prefix, then drop +
//! reopen + converge).

use std::sync::Arc;

use nlos_application::ProcessBindingReceipt;
use nlos_artifact::{ContentDigest, staging_id_for};
use nlos_operation::OperationHandle;
#[cfg(not(unix))]
use nlos_process::NoopPlatformKillAdapter;
use nlos_process::{
    PlatformKillDecision, PosixPlatformKillAdapter, ProcessBindingRecord, ProcessLifecycleState,
    ProcessTerminalRecord, PropagateCancelToFibersRequest, PropagateCrashRequest,
    RegisterFiberIncarnationRequest, RegisterSupervisorPidRequest, RequestPlatformKillRequest,
    SupervisorPidRegistry,
};
use nlos_runtime::{FiberHandle, RuntimeAdapter};
use nlos_runtime_tokio::{ProcessFiberCancelReport, TokioRuntimeAdapter};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactPublicationExpectation, ArtifactTaskCommitReceipt, Authorities,
    CancelDecision, CancelRequest, PermitDecision, PermitRequest, artifact_publication_plan_root,
    empty_effect_history_root,
};
use nlos_types::{
    ApplicationId, CancellationScopeId, CommitPermitId, ExecutionFiberId, Generation,
    InstallationId, PackageId, PrincipalId, TaskAttemptId, TaskId,
};

use crate::error::SliceKResult;
use crate::fiber::{FiberOutcome, WriteFiberJob, spawn_write_fiber};
use crate::package::{PublishedPackage, Publisher, fixture_bytes};
use crate::runtime::{SliceKRuntime, seeded_key};

/// One fixture id from its `from_bytes` constructor: the convention
/// `[seed + offset; 16]`.
macro_rules! seeded {
    ($from:expr, $seed:expr, $offset:expr) => {
        $from([$seed.wrapping_add($offset); 16])
    };
}

/// Everything the happy chain durably produced, one field per slice step.
pub struct HappyChain {
    pub publisher: Publisher,
    pub package: PublishedPackage,
    pub verification_receipt_id: nlos_types::ReceiptId,
    pub installation_id: nlos_types::InstallationId,
    pub application_id: nlos_types::ApplicationId,
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub scope_id: CancellationScopeId,
    /// The durable process binding the fiber was spawned under.
    pub process: ProcessBindingRecord,
    pub permit_id: CommitPermitId,
    pub plan_id: ArtifactCommitPlanId,
    pub fiber: FiberHandle,
    pub outcome: FiberOutcome,
    pub receipt: ArtifactTaskCommitReceipt,
}

/// Runs the full happy chain to its terminal `TaskCommitReceipt`.
///
/// # Errors
///
/// Propagates every authority error; any refusal aborts the chain
/// fail-closed (the authorities guarantee zero durable state on refusal).
///
/// # Panics
///
/// Panics if the permit CAS or the commit plan on a fresh seeded chain does
/// not produce its unique outcome — unreachable by construction (fresh ids,
/// single attempt, no competing writer), and a hard stop if the landed
/// authorities ever change that contract.
pub async fn run_happy_chain(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    seed: u8,
) -> SliceKResult<HappyChain> {
    let publisher = runtime.bootstrap_publisher(seed)?;
    let payload = fixture_bytes(seed, 256);
    let package = runtime.publish_signed_package(&publisher, seed, &payload)?;
    let verification = runtime.verify_signed_package(&package, seed)?;
    let verification_receipt_id = verification.receipt_id;
    let installation = runtime.install_verified_package(&verification, seed)?;
    let (task_id, attempt_id, scope_id) = runtime.register_task_and_attempt(seed)?;
    let process = runtime.materialize_process(seed, task_id, attempt_id, Generation::INITIAL)?;

    let write_bytes = fixture_bytes(seed.wrapping_add(200), 128);
    let stage_key = seeded_key(seed, 40);
    let artifact_id = package.payload_artifact;
    let stage_created_at_ms = runtime.wall_now_ms(seeded_key(seed, 41))?;
    let expectation = ArtifactPublicationExpectation {
        staging_id: staging_id_for(artifact_id, stage_key).into_bytes(),
        artifact_id,
        target_revision: 2,
        digest: ContentDigest::of_bytes(&write_bytes).into_bytes(),
        size_bytes: u64::try_from(write_bytes.len()).unwrap_or(u64::MAX),
    };
    let write_set_root = artifact_publication_plan_root(&[expectation])?;
    let requested_at_ms = runtime.wall_now_i64(seeded_key(seed, 42))?;
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
                idempotency_key: seeded_key(seed, 43),
                valid_until_ms: i64::MAX,
                requested_at_ms,
            },
        )?
    else {
        panic!("happy chain: permit must be issued on a fresh task");
    };
    let permit_id = permit.permit_id;

    let job = WriteFiberJob {
        operation_id: seeded!(nlos_types::OperationId::from_bytes, seed, 44),
        callback_id: seeded!(nlos_types::CallbackId::from_bytes, seed, 45),
        completion_receipt_id: seeded!(nlos_types::ReceiptId::from_bytes, seed, 46),
        expected_head_revision: 1,
        artifact_id,
        stage_key,
        stage_bytes: write_bytes.into(),
        stage_created_at_ms,
        permit: Some(permit_id),
        write_set_root,
        plan_key: seeded_key(seed, 47),
        planned_at_ms: runtime.wall_now_i64(seeded_key(seed, 48))?,
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
    };
    let spec = nlos_runtime::FiberSpec {
        fiber_id: seeded!(ExecutionFiberId::from_bytes, seed, 50),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: process.agent_instance_id,
        agent_generation: process.agent_instance_generation,
        process_id: process.process_id,
        process_generation: process.process_generation,
        task_attempt_id: Some(attempt_id),
        cancellation_scope_id: scope_id,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: seeded!(nlos_types::ResourceGroupId::from_bytes, seed, 53),
        scheduler_domain_id: seeded!(nlos_types::SchedulerDomainId::from_bytes, seed, 54),
        deadline: None,
    };
    let (fiber, receiver) = spawn_write_fiber(Arc::clone(runtime), adapter, spec, job)?;
    let outcome = receiver.await.expect("fiber outcome channel")?;
    let Some(plan_id) = outcome.plan_id else {
        panic!("happy chain: the permit-bound fiber must plan the commit");
    };

    let now_ms = runtime.wall_now_i64(seeded_key(seed, 55))?;
    let receipt = runtime
        .converge_pending(16, now_ms)?
        .into_iter()
        .find(|receipt| receipt.task_receipt.task_id == task_id)
        .expect("happy chain: converged receipt for the chain task");

    Ok(HappyChain {
        publisher,
        package,
        verification_receipt_id,
        installation_id: installation.installation_id,
        application_id: installation.application_id,
        task_id,
        attempt_id,
        scope_id,
        process,
        permit_id,
        plan_id,
        fiber,
        outcome,
        receipt,
    })
}

/// Everything the cancel path durably produced.
pub struct CancelFacts {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub scope_id: CancellationScopeId,
    /// The durable process binding the fiber was spawned under.
    pub process: ProcessBindingRecord,
    pub fiber: FiberHandle,
    pub cancel: CancelDecision,
    pub fenced_permit: PermitDecision,
    pub converged_plans: usize,
}

/// Runs the cancel path: a fiber executes its durable operation-only
/// prefix, the task is cancelled, the outstanding permit request is fenced
/// (`CancelledBeforeEffect`), the runtime scope refuses new fibers, and no
/// commit ever appears.
///
/// # Errors
///
/// Propagates authority and runtime errors.
///
/// # Panics
///
/// Panics if the operation-only fiber reports a commit plan — unreachable
/// by construction (`permit: None`), and a hard stop if that contract ever
/// changes.
pub async fn run_cancel_path(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    seed: u8,
) -> SliceKResult<CancelFacts> {
    let (task_id, attempt_id, scope_id) = runtime.register_task_and_attempt(seed)?;
    let process = runtime.materialize_process(seed, task_id, attempt_id, Generation::INITIAL)?;
    let now_ms = runtime.wall_now_i64(seeded_key(seed, 61))?;

    let job = WriteFiberJob {
        operation_id: seeded!(nlos_types::OperationId::from_bytes, seed, 62),
        callback_id: seeded!(nlos_types::CallbackId::from_bytes, seed, 63),
        completion_receipt_id: seeded!(nlos_types::ReceiptId::from_bytes, seed, 64),
        expected_head_revision: 0,
        artifact_id: seeded!(nlos_types::ArtifactId::from_bytes, seed, 65),
        stage_key: seeded_key(seed, 66),
        stage_bytes: Vec::new().into(),
        stage_created_at_ms: 0,
        permit: None,
        write_set_root: empty_effect_history_root(),
        plan_key: seeded_key(seed, 67),
        planned_at_ms: now_ms,
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
    };
    let spec = nlos_runtime::FiberSpec {
        fiber_id: seeded!(ExecutionFiberId::from_bytes, seed, 70),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: process.agent_instance_id,
        agent_generation: process.agent_instance_generation,
        process_id: process.process_id,
        process_generation: process.process_generation,
        task_attempt_id: Some(attempt_id),
        cancellation_scope_id: scope_id,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: seeded!(nlos_types::ResourceGroupId::from_bytes, seed, 73),
        scheduler_domain_id: seeded!(nlos_types::SchedulerDomainId::from_bytes, seed, 74),
        deadline: None,
    };
    let (fiber, receiver) = spawn_write_fiber(Arc::clone(runtime), adapter, spec, job)?;
    let outcome = receiver.await.expect("fiber outcome channel")?;
    assert!(
        outcome.plan_id.is_none(),
        "operation-only fiber plans nothing"
    );

    let cancel = runtime.tasks.cancel_task(CancelRequest {
        task_id,
        idempotency_key: seeded_key(seed, 75),
        requested_at_ms: runtime.wall_now_i64(seeded_key(seed, 76))?,
    })?;
    // The same cancellation also closes the runtime side: the attempt's
    // structured scope refuses every future fiber admission.
    nlos_runtime::RuntimeAdapter::cancel_scope(adapter, scope_id, Generation::INITIAL)?;
    let fenced_permit = runtime
        .tasks
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: empty_effect_history_root(),
                planned_effects: Vec::new(),
                idempotency_key: seeded_key(seed, 77),
                valid_until_ms: i64::MAX,
                requested_at_ms: runtime.wall_now_i64(seeded_key(seed, 78))?,
            },
        )?;
    let converged_plans = runtime.converge_pending(16, now_ms)?.len();
    Ok(CancelFacts {
        task_id,
        attempt_id,
        scope_id,
        process,
        fiber,
        cancel,
        fenced_permit,
        converged_plans,
    })
}

/// The durable prefix of the crash-recovery scenario: everything through
/// the fiber's stage+plan, deliberately **without** converging — the caller
/// then drops the runtime (the kill -9 analogue) and reopens.
pub struct RecoveryPrefix {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    /// The durable process binding the fiber was spawned under.
    pub process: ProcessBindingRecord,
    pub permit_id: CommitPermitId,
    pub plan_id: ArtifactCommitPlanId,
    pub artifact_id: nlos_types::ArtifactId,
    pub operation: OperationHandle,
    pub verification_receipt_id: nlos_types::ReceiptId,
    pub installation_id: nlos_types::InstallationId,
    /// The exact signed envelope of the chain, carried out so the reopened
    /// runtime can replay the same verify request byte-identically.
    pub signed: nlos_artifact::SignedPackage,
}

/// Runs the pre-crash half of the recovery scenario.
///
/// # Errors
///
/// Propagates authority errors.
///
/// # Panics
///
/// Panics if the permit-bound fiber does not produce its commit plan —
/// unreachable by construction (fresh seeded ids), and a hard stop if the
/// landed authorities ever change that contract.
pub async fn run_recovery_prefix(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    seed: u8,
) -> SliceKResult<RecoveryPrefix> {
    let publisher = runtime.bootstrap_publisher(seed)?;
    let package = runtime.publish_signed_package(&publisher, seed, &fixture_bytes(seed, 64))?;
    let verification = runtime.verify_signed_package(&package, seed)?;
    let installation = runtime.install_verified_package(&verification, seed)?;
    let (task_id, attempt_id, scope_id) = runtime.register_task_and_attempt(seed)?;
    let process = runtime.materialize_process(seed, task_id, attempt_id, Generation::INITIAL)?;

    let write_bytes = fixture_bytes(seed.wrapping_add(210), 96);
    let stage_key = seeded_key(seed, 80);
    let artifact_id = package.payload_artifact;
    let expectation = ArtifactPublicationExpectation {
        staging_id: staging_id_for(artifact_id, stage_key).into_bytes(),
        artifact_id,
        target_revision: 2,
        digest: ContentDigest::of_bytes(&write_bytes).into_bytes(),
        size_bytes: u64::try_from(write_bytes.len()).unwrap_or(u64::MAX),
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
                idempotency_key: seeded_key(seed, 81),
                valid_until_ms: i64::MAX,
                requested_at_ms: runtime.wall_now_i64(seeded_key(seed, 82))?,
            },
        )?
    else {
        panic!("recovery prefix: permit must be issued on a fresh task");
    };

    let job = WriteFiberJob {
        operation_id: seeded!(nlos_types::OperationId::from_bytes, seed, 83),
        callback_id: seeded!(nlos_types::CallbackId::from_bytes, seed, 84),
        completion_receipt_id: seeded!(nlos_types::ReceiptId::from_bytes, seed, 85),
        expected_head_revision: 1,
        artifact_id,
        stage_key,
        stage_bytes: write_bytes.into(),
        stage_created_at_ms: runtime.wall_now_ms(seeded_key(seed, 86))?,
        permit: Some(permit.permit_id),
        write_set_root,
        plan_key: seeded_key(seed, 87),
        planned_at_ms: runtime.wall_now_i64(seeded_key(seed, 88))?,
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
    };
    let spec = nlos_runtime::FiberSpec {
        fiber_id: seeded!(ExecutionFiberId::from_bytes, seed, 90),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: process.agent_instance_id,
        agent_generation: process.agent_instance_generation,
        process_id: process.process_id,
        process_generation: process.process_generation,
        task_attempt_id: Some(attempt_id),
        cancellation_scope_id: scope_id,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: seeded!(nlos_types::ResourceGroupId::from_bytes, seed, 93),
        scheduler_domain_id: seeded!(nlos_types::SchedulerDomainId::from_bytes, seed, 94),
        deadline: None,
    };
    let (_fiber, receiver) = spawn_write_fiber(Arc::clone(runtime), adapter, spec, job)?;
    let outcome = receiver.await.expect("fiber outcome channel")?;
    let Some(plan_id) = outcome.plan_id else {
        panic!("recovery prefix: the permit-bound fiber must plan the commit");
    };
    Ok(RecoveryPrefix {
        task_id,
        attempt_id,
        process,
        permit_id: permit.permit_id,
        plan_id,
        artifact_id,
        operation: outcome.operation,
        verification_receipt_id: verification.receipt_id,
        installation_id: installation.installation_id,
        signed: package.signed,
    })
}

/// Seed offset of the second process's `Task`/`Attempt` pair relative to
/// the scenario seed (the key-band contract of the second-process lane:
/// `register_task_and_attempt` consumes offsets 20–26 of its seed).
pub const SECOND_TASK_SEED_OFFSET: u8 = 0x20;

/// Seed offset of the second process's materialization relative to the
/// scenario seed (`materialize_process` consumes offsets 110–114 of its
/// seed).
pub const SECOND_MATERIALIZE_SEED_OFFSET: u8 = 0x40;

/// Seed offset of the second process's application binding registration
/// relative to the scenario seed (`register_process_binding` consumes
/// offsets 32–33 of its seed; the first binding uses the scenario seed).
pub const SECOND_BINDING_SEED_OFFSET: u8 = 0x02;

/// Everything the spawn phase of the second-process lane (ROAD-B-002
/// B2-1) durably produced: one installed application owning TWO process
/// bindings, each process materialized, supervised (os pid entries) and
/// driving runtime fibers with durable incarnations.
pub struct SecondProcessPair {
    /// The scenario seed every lane key derives from.
    pub seed: u8,
    pub package_id: PackageId,
    pub application_id: ApplicationId,
    pub installation_id: InstallationId,
    /// The principal that registered both process bindings (the replay
    /// input for `register_process_binding`).
    pub registrant_principal: PrincipalId,
    pub task_id_first: TaskId,
    pub attempt_id_first: TaskAttemptId,
    pub scope_first: CancellationScopeId,
    pub task_id_second: TaskId,
    pub attempt_id_second: TaskAttemptId,
    pub scope_second: CancellationScopeId,
    pub process_first: ProcessBindingRecord,
    pub process_second: ProcessBindingRecord,
    pub binding_receipt_first: ProcessBindingReceipt,
    pub binding_receipt_second: ProcessBindingReceipt,
    /// The in-memory supervisor registry holding both os pid entries
    /// (W22-P); `pid_map()` is the exact kill-adapter feed shape.
    pub registry: SupervisorPidRegistry,
    pub supervisor_registered_at_ms: u64,
    pub os_pid_first: u32,
    pub os_pid_second: u32,
    /// The first process's live fiber (never completes on its own).
    pub fiber_first: FiberHandle,
    /// The second process's live fiber (the batch-cancel surface).
    pub fiber_second: FiberHandle,
    /// The second process's completed operation-only write fiber (the
    /// process "ran" durable work before the kill).
    pub fiber_second_write: FiberHandle,
    pub write_outcome_second: FiberOutcome,
}

/// Everything the kill phase of the second-process lane durably produced:
/// the platform-kill receipt (adapter signaled), the crash terminal marker
/// (which also wrote the fiber batch-cancel receipts), and the W27-C
/// runtime linkage report that drove the scope cancel. The spawn-phase
/// facts live in the caller's [`SecondProcessPair`].
pub struct SecondProcessKill {
    /// The full kill decision (`Signaled` on both the real POSIX and the
    /// noop contract adapter).
    pub kill: PlatformKillDecision,
    /// The crash terminal marker of the second process.
    pub crash: ProcessTerminalRecord,
    /// The W27-C linkage report (`TokioRuntimeAdapter::cancel_process_fibers`).
    pub linkage: ProcessFiberCancelReport,
}

/// Runs the spawn phase of the second-process lane: one application
/// installed once, then TWO process bindings materialized concurrently —
/// each with its own `Task`/`Attempt` and delegated process binding, both
/// registered against the application, both supervised with an os pid
/// entry, and both driving runtime fibers (the second additionally runs
/// one durable operation-only write job to completion).
///
/// # Errors
///
/// Propagates every authority/runtime error; any refusal aborts the lane
/// fail-closed.
///
/// # Panics
///
/// Panics if the operation-only write fiber reports a commit plan —
/// unreachable by construction (`permit: None`).
pub async fn run_second_process_pair(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    seed: u8,
    os_pid_first: u32,
    os_pid_second: u32,
) -> SliceKResult<SecondProcessPair> {
    let publisher = runtime.bootstrap_publisher(seed)?;
    let package = runtime.publish_signed_package(&publisher, seed, &fixture_bytes(seed, 64))?;
    let verification = runtime.verify_signed_package(&package, seed)?;
    let installation = runtime.install_verified_package(&verification, seed)?;

    let (task_id_first, attempt_id_first, scope_first) = runtime.register_task_and_attempt(seed)?;
    let process_first =
        runtime.materialize_process(seed, task_id_first, attempt_id_first, Generation::INITIAL)?;
    let (task_id_second, attempt_id_second, scope_second) =
        runtime.register_task_and_attempt(seed.wrapping_add(SECOND_TASK_SEED_OFFSET))?;
    let process_second = runtime.materialize_process(
        seed.wrapping_add(SECOND_MATERIALIZE_SEED_OFFSET),
        task_id_second,
        attempt_id_second,
        Generation::INITIAL,
    )?;

    let binding_receipt_first = runtime.register_process_binding(
        package.package_id,
        process_first.process_id,
        publisher.principal_id,
        seed,
    )?;
    let binding_receipt_second = runtime.register_process_binding(
        package.package_id,
        process_second.process_id,
        publisher.principal_id,
        seed.wrapping_add(SECOND_BINDING_SEED_OFFSET),
    )?;

    let registry = SupervisorPidRegistry::new();
    let supervisor_registered_at_ms = runtime.wall_now_ms(seeded_key(seed, 139))?;
    registry.register(RegisterSupervisorPidRequest {
        process_id: process_first.process_id,
        process_generation: process_first.process_generation,
        os_pid: os_pid_first,
        registered_at_ms: supervisor_registered_at_ms,
    })?;
    registry.register(RegisterSupervisorPidRequest {
        process_id: process_second.process_id,
        process_generation: process_second.process_generation,
        os_pid: os_pid_second,
        registered_at_ms: supervisor_registered_at_ms,
    })?;

    let FiberPair {
        fiber_first,
        fiber_second,
        fiber_second_write,
        write_outcome_second,
    } = spawn_pair_fibers(
        runtime,
        adapter,
        seed,
        &process_first,
        &process_second,
        (attempt_id_first, scope_first),
        (attempt_id_second, scope_second),
    )
    .await?;

    Ok(SecondProcessPair {
        seed,
        package_id: package.package_id,
        application_id: installation.application_id,
        installation_id: installation.installation_id,
        registrant_principal: publisher.principal_id,
        task_id_first,
        attempt_id_first,
        scope_first,
        task_id_second,
        attempt_id_second,
        scope_second,
        process_first,
        process_second,
        binding_receipt_first,
        binding_receipt_second,
        registry,
        supervisor_registered_at_ms,
        os_pid_first,
        os_pid_second,
        fiber_first,
        fiber_second,
        fiber_second_write,
        write_outcome_second,
    })
}

/// The runtime-side fibers of the second-process lane.
struct FiberPair {
    fiber_first: FiberHandle,
    fiber_second: FiberHandle,
    fiber_second_write: FiberHandle,
    write_outcome_second: FiberOutcome,
}

/// Spawns the lane's runtime fibers and registers their durable
/// incarnations: the second process "ran" (one operation-only write job to
/// completion), both processes are "alive" (one never-completing live
/// fiber each), and both live fibers own a durable incarnation — the
/// batch-cancel receipt surface — registered while both bindings are
/// still Active.
///
/// # Errors
///
/// Propagates authority and runtime errors.
///
/// # Panics
///
/// Panics if the operation-only write fiber reports a commit plan —
/// unreachable by construction (`permit: None`).
async fn spawn_pair_fibers(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    seed: u8,
    process_first: &ProcessBindingRecord,
    process_second: &ProcessBindingRecord,
    (attempt_id_first, scope_first): (TaskAttemptId, CancellationScopeId),
    (attempt_id_second, scope_second): (TaskAttemptId, CancellationScopeId),
) -> SliceKResult<FiberPair> {
    let write_planned_at_ms = runtime.wall_now_i64(seeded_key(seed, 124))?;
    let write_job = WriteFiberJob {
        operation_id: seeded!(nlos_types::OperationId::from_bytes, seed, 125),
        callback_id: seeded!(nlos_types::CallbackId::from_bytes, seed, 126),
        completion_receipt_id: seeded!(nlos_types::ReceiptId::from_bytes, seed, 127),
        expected_head_revision: 0,
        artifact_id: seeded!(nlos_types::ArtifactId::from_bytes, seed, 143),
        stage_key: seeded_key(seed, 129),
        stage_bytes: Vec::new().into(),
        stage_created_at_ms: 0,
        permit: None,
        write_set_root: empty_effect_history_root(),
        plan_key: seeded_key(seed, 130),
        planned_at_ms: write_planned_at_ms,
        task_id: process_second.task_id,
        attempt_id: attempt_id_second,
        attempt_generation: Generation::INITIAL,
    };
    let (fiber_second_write, receiver) = spawn_write_fiber(
        Arc::clone(runtime),
        adapter,
        linked_live_spec(seed, 128, scope_second, attempt_id_second, process_second),
        write_job,
    )?;
    let write_outcome_second = receiver.await.expect("fiber outcome channel")?;
    assert!(
        write_outcome_second.plan_id.is_none(),
        "operation-only fiber plans nothing"
    );

    let fiber_first = RuntimeAdapter::spawn_fiber(
        adapter,
        linked_live_spec(seed, 136, scope_first, attempt_id_first, process_first),
        Box::pin(std::future::pending()),
    )?;
    let fiber_second = RuntimeAdapter::spawn_fiber(
        adapter,
        linked_live_spec(seed, 131, scope_second, attempt_id_second, process_second),
        Box::pin(std::future::pending()),
    )?;

    runtime
        .process
        .register_fiber_incarnation(RegisterFiberIncarnationRequest {
            process_id: process_second.process_id,
            expected_process_generation: process_second.process_generation,
            expected_process_fencing_token: process_second.process_fencing_token,
            binding: fiber_second.fiber_id,
            idempotency_key: seeded_key(seed, 133),
            registered_at_ms: runtime.wall_now_ms(seeded_key(seed, 132))?,
        })?;
    runtime
        .process
        .register_fiber_incarnation(RegisterFiberIncarnationRequest {
            process_id: process_first.process_id,
            expected_process_generation: process_first.process_generation,
            expected_process_fencing_token: process_first.process_fencing_token,
            binding: fiber_first.fiber_id,
            idempotency_key: seeded_key(seed, 138),
            registered_at_ms: runtime.wall_now_ms(seeded_key(seed, 137))?,
        })?;

    Ok(FiberPair {
        fiber_first,
        fiber_second,
        fiber_second_write,
        write_outcome_second,
    })
}

/// Runs the kill phase of the second-process lane on the spawned pair's
/// SECOND process: durable platform-kill receipt → adapter signal (the
/// real POSIX adapter fed by the supervisor registry's `pid_map()` on
/// Unix; the noop contract adapter elsewhere) → crash terminal marker
/// (which also writes the fiber batch-cancel receipts) → the W27-C
/// runtime linkage driving the scope cancel. The FIRST process is never
/// touched — the caller asserts the isolation.
///
/// # Errors
///
/// Propagates authority errors and the linkage's typed refusal; the
/// durable kill receipt commits before the adapter invocation
/// (at-least-once), so an adapter failure still leaves the receipt
/// durable.
pub async fn run_second_process_platform_kill(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    pair: &SecondProcessPair,
) -> SliceKResult<SecondProcessKill> {
    let seed = pair.seed;
    let second = &pair.process_second;

    let killed_at_ms = runtime.wall_now_ms(seeded_key(seed, 120))?;
    #[cfg(unix)]
    let kill_adapter = PosixPlatformKillAdapter::new(pair.registry.pid_map());
    #[cfg(not(unix))]
    let kill_adapter = NoopPlatformKillAdapter;
    let kill = runtime.process.request_platform_kill(
        RequestPlatformKillRequest {
            process_id: second.process_id,
            expected_process_generation: second.process_generation,
            expected_process_fencing_token: second.process_fencing_token,
            idempotency_key: seeded_key(seed, 121),
            killed_at_ms,
        },
        &kill_adapter,
    )?;

    let marked_at_ms = runtime.wall_now_ms(seeded_key(seed, 122))?;
    let crash = runtime
        .process
        .propagate_crash(PropagateCrashRequest {
            process_id: second.process_id,
            expected_process_generation: second.process_generation,
            expected_process_fencing_token: second.process_fencing_token,
            idempotency_key: seeded_key(seed, 123),
            marked_at_ms,
        })?
        .record()
        .clone();

    let linkage = adapter.cancel_process_fibers(
        &runtime.process,
        PropagateCancelToFibersRequest {
            process_id: second.process_id,
            expected_process_generation: second.process_generation,
            expected_process_fencing_token: second.process_fencing_token,
            lifecycle_state: ProcessLifecycleState::Crashed,
            idempotency_key: seeded_key(seed, 123),
            cancelled_at_ms: marked_at_ms,
        },
    )?;

    Ok(SecondProcessKill {
        kill,
        crash,
        linkage,
    })
}

/// One fiber spec linked to `process` under its attempt's scope: the
/// live-execution shape shared by the lane's write and live fibers.
fn linked_live_spec(
    seed: u8,
    fiber_offset: u8,
    scope: CancellationScopeId,
    attempt: TaskAttemptId,
    process: &ProcessBindingRecord,
) -> nlos_runtime::FiberSpec {
    nlos_runtime::FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes([seed.wrapping_add(fiber_offset); 16]),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: process.agent_instance_id,
        agent_generation: process.agent_instance_generation,
        process_id: process.process_id,
        process_generation: process.process_generation,
        task_attempt_id: Some(attempt),
        cancellation_scope_id: scope,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: nlos_types::ResourceGroupId::from_bytes([seed.wrapping_add(141); 16]),
        scheduler_domain_id: nlos_types::SchedulerDomainId::from_bytes(
            [seed.wrapping_add(142); 16],
        ),
        deadline: None,
    }
}
