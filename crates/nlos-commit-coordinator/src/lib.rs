//! Recoverable single-node coordinator for artifact-only Task commits.
//!
//! The coordinator owns no independent truth. It advances the durable
//! state machine already recorded by `TaskAuthority` and
//! `ArtifactAuthority`, one idempotent step at a time, so process restart
//! can resume from any committed prefix.
//! [`TaskAuthorityCommitRecoveryWorker`] gives the owning `TaskAuthority`
//! service a startup scan, periodic retry, bounded backoff, lifecycle health,
//! and prompt joined shutdown without introducing a third authority.

use std::error::Error;
use std::fmt;

use nlos_artifact::{
    ArtifactError, ArtifactPublicationReceipt, ArtifactStore, ContentDigest,
    PublishStagedRevisionRequest, StagingId,
};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactCommitPlanState, ArtifactFinalizeDecision,
    ArtifactTaskCommitReceipt, FinalizeArtifactCommitRequest, NestedArtifactPublicationReceipt,
    RecordArtifactPublicationsRequest, SqliteTaskAuthority, TaskStoreError,
};

mod worker;

pub use worker::{
    RecoveryFailureAuthority, RecoveryWorkerConfig, RecoveryWorkerFailure, RecoveryWorkerHealth,
    RecoveryWorkerStartError, RecoveryWorkerState, TaskAuthorityCommitRecoveryWorker,
};

/// One bounded coordinator invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConvergeArtifactCommitRequest {
    pub plan_id: ArtifactCommitPlanId,
    pub now_ms: i64,
}

/// Durable boundary reached by one coordinator step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConvergeStep {
    Authorized,
    PublishedOne {
        staging_id: [u8; 16],
        state_after: ArtifactCommitPlanState,
    },
    Finalized(Box<ArtifactTaskCommitReceipt>),
    AlreadyFinalized(Box<ArtifactTaskCommitReceipt>),
}

/// Cross-authority coordination error; both authority errors stay typed.
#[derive(Debug)]
pub enum CoordinatorError {
    InvalidTimestamp,
    Task(TaskStoreError),
    Artifact(ArtifactError),
}

/// One plan that could not converge during a best-effort pending scan.
#[derive(Debug)]
pub struct PendingConvergenceFailure {
    pub plan_id: ArtifactCommitPlanId,
    pub error: CoordinatorError,
}

/// Bounded pending-scan result. A failed plan does not prevent later plans
/// in the same snapshot from converging.
#[derive(Debug)]
pub struct PendingConvergenceReport {
    pub inspected: usize,
    pub finalized: Vec<ArtifactTaskCommitReceipt>,
    pub failures: Vec<PendingConvergenceFailure>,
}

impl fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimestamp => {
                formatter.write_str("coordinator timestamp must be non-negative")
            }
            Self::Task(error) => write!(formatter, "TaskAuthority coordination failure: {error}"),
            Self::Artifact(error) => {
                write!(formatter, "ArtifactAuthority coordination failure: {error}")
            }
        }
    }
}

impl Error for CoordinatorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidTimestamp => None,
            Self::Task(error) => Some(error),
            Self::Artifact(error) => Some(error),
        }
    }
}

impl From<TaskStoreError> for CoordinatorError {
    fn from(error: TaskStoreError) -> Self {
        Self::Task(error)
    }
}

impl From<ArtifactError> for CoordinatorError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

/// Stateless driver over the two durable authorities.
pub struct ArtifactCommitCoordinator<'a> {
    tasks: &'a SqliteTaskAuthority,
    artifacts: &'a ArtifactStore,
}

impl<'a> ArtifactCommitCoordinator<'a> {
    #[must_use]
    pub const fn new(tasks: &'a SqliteTaskAuthority, artifacts: &'a ArtifactStore) -> Self {
        Self { tasks, artifacts }
    }

    /// Advances at most one durable cross-authority boundary.
    ///
    /// # Errors
    ///
    /// Returns the original typed authority failure. A later call can retry
    /// from the last committed prefix.
    pub fn converge_one_step(
        &self,
        request: ConvergeArtifactCommitRequest,
    ) -> Result<ConvergeStep, CoordinatorError> {
        let published_at_ms =
            u64::try_from(request.now_ms).map_err(|_| CoordinatorError::InvalidTimestamp)?;
        let progress = self
            .tasks
            .inspect_artifact_commit_progress(request.plan_id)?;
        match progress.plan.state {
            ArtifactCommitPlanState::Planned => {
                self.tasks
                    .authorize_artifact_publication(request.plan_id, request.now_ms)?;
                Ok(ConvergeStep::Authorized)
            }
            ArtifactCommitPlanState::Publishing => {
                let expectation = progress
                    .plan
                    .expectations
                    .iter()
                    .find(|expected| {
                        !progress
                            .publications
                            .iter()
                            .any(|receipt| receipt.staging_id == expected.staging_id)
                    })
                    .ok_or(TaskStoreError::CorruptRecord(
                        "Publishing plan has no missing Artifact expectation",
                    ))?;
                let decision =
                    self.artifacts
                        .publish_staged_revision(PublishStagedRevisionRequest {
                            staging_id: StagingId::from_bytes(expectation.staging_id),
                            task_id: progress.plan.task_id,
                            permit_id: progress.plan.permit_id,
                            write_set_root: ContentDigest::from_bytes(progress.plan.write_set_root),
                            published_at_ms,
                        })?;
                let nested = nested_receipt(decision.receipt())?;
                let updated =
                    self.tasks
                        .record_artifact_publications(RecordArtifactPublicationsRequest {
                            plan_id: request.plan_id,
                            receipts: vec![nested],
                            observed_at_ms: request.now_ms,
                        })?;
                Ok(ConvergeStep::PublishedOne {
                    staging_id: expectation.staging_id,
                    state_after: updated.plan.state,
                })
            }
            ArtifactCommitPlanState::Ready => {
                let decision =
                    self.tasks
                        .finalize_artifact_commit(FinalizeArtifactCommitRequest {
                            plan_id: request.plan_id,
                            finalized_at_ms: request.now_ms,
                        })?;
                Ok(match decision {
                    ArtifactFinalizeDecision::Committed(receipt) => {
                        ConvergeStep::Finalized(receipt)
                    }
                    ArtifactFinalizeDecision::Replayed(receipt) => {
                        ConvergeStep::AlreadyFinalized(receipt)
                    }
                })
            }
            ArtifactCommitPlanState::Finalized => {
                let decision =
                    self.tasks
                        .finalize_artifact_commit(FinalizeArtifactCommitRequest {
                            plan_id: request.plan_id,
                            finalized_at_ms: request.now_ms,
                        })?;
                Ok(ConvergeStep::AlreadyFinalized(Box::new(
                    decision.receipt().clone(),
                )))
            }
        }
    }

    /// Repeats bounded steps until the plan is durably finalized.
    ///
    /// # Errors
    ///
    /// Returns the first typed authority failure; retrying resumes from the
    /// durable prefix already reached.
    pub fn converge(
        &self,
        request: ConvergeArtifactCommitRequest,
    ) -> Result<ArtifactTaskCommitReceipt, CoordinatorError> {
        loop {
            match self.converge_one_step(request)? {
                ConvergeStep::Authorized | ConvergeStep::PublishedOne { .. } => {}
                ConvergeStep::Finalized(receipt) | ConvergeStep::AlreadyFinalized(receipt) => {
                    return Ok(*receipt);
                }
            }
        }
    }

    /// Scans a bounded set of durable non-finalized plans and converges
    /// each one. This is the restart entry point; finalized plans disappear
    /// from subsequent scans.
    ///
    /// # Errors
    ///
    /// Returns the first typed authority failure. Earlier plans remain
    /// durably finalized and later scans continue from that prefix.
    pub fn converge_pending(
        &self,
        limit: usize,
        now_ms: i64,
    ) -> Result<Vec<ArtifactTaskCommitReceipt>, CoordinatorError> {
        self.tasks
            .list_incomplete_artifact_commit_plans(limit)?
            .into_iter()
            .map(|plan| {
                self.converge(ConvergeArtifactCommitRequest {
                    plan_id: plan.plan_id,
                    now_ms,
                })
            })
            .collect()
    }

    /// Scans a bounded snapshot and attempts every plan independently.
    /// Per-plan authority failures are returned in the report so one bad
    /// plan cannot starve unrelated commits.
    ///
    /// # Errors
    ///
    /// Returns only when the `TaskAuthority` scan itself fails. Individual
    /// convergence failures remain typed in `report.failures`.
    pub fn converge_pending_best_effort(
        &self,
        limit: usize,
        now_ms: i64,
    ) -> Result<PendingConvergenceReport, CoordinatorError> {
        let plans = self.tasks.list_incomplete_artifact_commit_plans(limit)?;
        let mut report = PendingConvergenceReport {
            inspected: plans.len(),
            finalized: Vec::new(),
            failures: Vec::new(),
        };
        for plan in plans {
            match self.converge(ConvergeArtifactCommitRequest {
                plan_id: plan.plan_id,
                now_ms,
            }) {
                Ok(receipt) => report.finalized.push(receipt),
                Err(error) => report.failures.push(PendingConvergenceFailure {
                    plan_id: plan.plan_id,
                    error,
                }),
            }
        }
        Ok(report)
    }
}

fn nested_receipt(
    receipt: &ArtifactPublicationReceipt,
) -> Result<NestedArtifactPublicationReceipt, CoordinatorError> {
    Ok(NestedArtifactPublicationReceipt {
        receipt_id: receipt.receipt_id,
        staging_id: receipt.staging_id.into_bytes(),
        artifact_id: receipt.artifact_id,
        revision: receipt.revision,
        digest: receipt.digest.into_bytes(),
        size_bytes: receipt.size_bytes,
        task_id: receipt.task_id,
        permit_id: receipt.permit_id,
        write_set_root: receipt.write_set_root.into_bytes(),
        prior_head_revision: receipt.prior_head_revision,
        prior_head_digest: receipt.prior_head_digest.map(ContentDigest::into_bytes),
        new_head_revision: receipt.new_head_revision,
        new_head_digest: receipt.new_head_digest.into_bytes(),
        created_at_ms: i64::try_from(receipt.created_at_ms)
            .map_err(|_| CoordinatorError::InvalidTimestamp)?,
    })
}
