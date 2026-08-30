//! The three longitudinal scenarios of the slice, composed once and shared
//! by the integration tests and the demo bin: the happy chain
//! (sign → verify → install → task → permit → fiber → converge), the
//! cancel path, and the crash-recovery split (durable prefix, then drop +
//! reopen + converge).

use std::sync::Arc;

use nlos_artifact::{ContentDigest, staging_id_for};
use nlos_operation::OperationHandle;
use nlos_process::ProcessBindingRecord;
use nlos_runtime::FiberHandle;
use nlos_runtime_tokio::TokioRuntimeAdapter;
use nlos_task::{
    ArtifactCommitPlanId, ArtifactPublicationExpectation, ArtifactTaskCommitReceipt, Authorities,
    CancelDecision, CancelRequest, PermitDecision, PermitRequest, artifact_publication_plan_root,
    empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, CommitPermitId, ExecutionFiberId, Generation, TaskAttemptId, TaskId,
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
