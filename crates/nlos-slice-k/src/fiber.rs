//! The fiber lane of the slice: one tokio fiber that materializes the
//! attempt's write as a durable driver Operation and, when a permit is
//! bound, stages the authorized Artifact revision and plans its commit.
//! The future itself adds no authority semantics — it composes the landed
//! `SqliteOperationStore`, `ArtifactStore`, and `SqliteTaskAuthority` APIs
//! exactly as their own docs prescribe.

use std::sync::Arc;

use nlos_artifact::{ContentDigest, StageRevisionRequest, staging_id_for};
use nlos_operation::{CompletionOutcome, OperationHandle, OperationSpec};
use nlos_runtime::{FiberExit, FiberFuture, FiberHandle, FiberSpec};
use nlos_runtime_tokio::TokioRuntimeAdapter;
use nlos_task::{ArtifactCommitPlanId, ArtifactPublicationExpectation, PlanArtifactCommitRequest};
use nlos_types::{
    CallbackId, CommitPermitId, Generation, IdempotencyKey, OperationId, ReceiptId, TaskAttemptId,
    TaskId,
};

use crate::error::{SliceKError, SliceKResult};
use crate::runtime::SliceKRuntime;

/// One fiber's write job, fully owned (the future is `'static`).
///
/// `permit = None` runs the operation-only prefix (the cancel-scenario
/// shape); `permit = Some` also stages the authorized revision and plans
/// the cross-authority commit (the happy-chain shape).
#[derive(Clone)]
pub struct WriteFiberJob {
    pub operation_id: OperationId,
    pub callback_id: CallbackId,
    pub completion_receipt_id: ReceiptId,
    /// The revision the fiber observed before writing (package payload =
    /// revision 1 in the happy chain).
    pub expected_head_revision: u64,
    pub artifact_id: nlos_types::ArtifactId,
    pub stage_key: IdempotencyKey,
    pub stage_bytes: Arc<[u8]>,
    pub stage_created_at_ms: u64,
    /// `None` = operation-only fiber; `Some` = permit-bound write.
    pub permit: Option<CommitPermitId>,
    pub write_set_root: [u8; 32],
    pub plan_key: IdempotencyKey,
    pub planned_at_ms: i64,
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
}

/// What the fiber durably accomplished, handed back over a oneshot (the
/// `FiberFuture` output itself is only `FiberExit`).
#[derive(Clone, Copy, Debug)]
pub struct FiberOutcome {
    pub operation: OperationHandle,
    pub plan_id: Option<ArtifactCommitPlanId>,
}

/// Spawns the write fiber on the adapter and returns its handle plus the
/// receiver of its durable outcome.
///
/// # Errors
///
/// Propagates [`nlos_runtime::RuntimeError`] from fiber admission
/// (duplicate id, cancelled scope, queue full, stale generation).
pub fn spawn_write_fiber(
    runtime: Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    spec: FiberSpec,
    job: WriteFiberJob,
) -> Result<
    (
        FiberHandle,
        tokio::sync::oneshot::Receiver<SliceKResult<FiberOutcome>>,
    ),
    nlos_runtime::RuntimeError,
> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let future: FiberFuture = Box::pin(async move {
        let outcome = run_write_job(&runtime, &spec, &job);
        let succeeded = outcome.is_ok();
        let _ = sender.send(outcome);
        if succeeded {
            FiberExit::Completed
        } else {
            FiberExit::Failed
        }
    });
    let handle = nlos_runtime::RuntimeAdapter::spawn_fiber(adapter, spec, future)?;
    Ok((handle, receiver))
}

/// Runs the job to its durable effect: driver Operation
/// `register → dispatch → complete`, then — under a permit — stage + plan.
fn run_write_job(
    runtime: &SliceKRuntime,
    spec: &FiberSpec,
    job: &WriteFiberJob,
) -> SliceKResult<FiberOutcome> {
    let owner_fiber = FiberHandle {
        fiber_id: spec.fiber_id,
        generation: spec.fiber_generation,
    };
    let operation = runtime
        .operations
        .register(OperationSpec {
            operation_id: job.operation_id,
            generation: Generation::INITIAL,
            owner_fiber,
            cancellation_scope_id: spec.cancellation_scope_id,
            cancellation_generation: spec.cancellation_generation,
        })?
        .handle();
    let ticket = runtime.operations.dispatch(operation, job.callback_id)?;
    runtime.operations.complete(
        ticket,
        CompletionOutcome::Completed {
            receipt_id: job.completion_receipt_id,
        },
    )?;

    let mut plan_id = None;
    if let Some(permit_id) = job.permit {
        runtime.artifacts.stage_revision(StageRevisionRequest {
            artifact_id: job.artifact_id,
            expected_head_revision: job.expected_head_revision,
            bytes: &job.stage_bytes,
            task_id: job.task_id,
            permit_id,
            write_set_root: ContentDigest::from_bytes(job.write_set_root),
            idempotency_key: job.stage_key,
            created_at_ms: job.stage_created_at_ms,
        })?;
        let size_bytes = u64::try_from(job.stage_bytes.len())
            .map_err(|_| SliceKError::SizeOverflow(job.stage_bytes.len()))?;
        let expectation = ArtifactPublicationExpectation {
            staging_id: staging_id_for(job.artifact_id, job.stage_key).into_bytes(),
            artifact_id: job.artifact_id,
            target_revision: job.expected_head_revision + 1,
            digest: ContentDigest::of_bytes(&job.stage_bytes).into_bytes(),
            size_bytes,
        };
        let decision = runtime
            .tasks
            .plan_artifact_commit(PlanArtifactCommitRequest {
                task_id: job.task_id,
                attempt_id: job.attempt_id,
                attempt_generation: job.attempt_generation,
                permit_id,
                expectations: vec![expectation],
                idempotency_key: job.plan_key,
                planned_at_ms: job.planned_at_ms,
            })?;
        plan_id = Some(decision.record().plan_id);
    }
    Ok(FiberOutcome { operation, plan_id })
}
