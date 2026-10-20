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
use nlos_capability::CapabilityTarget;
use nlos_semantic::{
    PublishSemanticPublicationRequest, SemanticAuthority, SemanticAuthorityError,
    SemanticPublicationReceipt,
};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactCommitPlanState, ArtifactFinalizeDecision,
    ArtifactTaskCommitReceipt, FinalizeArtifactCommitRequest, NestedArtifactPublicationReceipt,
    RecordArtifactPublicationsRequest, RecordSemanticPublicationsRequest, SemanticCommitPlanId,
    SemanticCommitPlanState, SemanticFinalizeDecision, SemanticTaskCommitReceipt,
    SqliteTaskAuthority, TaskStoreError, TaskWriteSetSemanticTarget,
};

mod worker;

pub use worker::{
    RecoveryFailureAuthority, RecoveryWorkerConfig, RecoveryWorkerFailure, RecoveryWorkerHealth,
    RecoveryWorkerStartError, RecoveryWorkerState, TaskAuthorityCommitRecoveryWorker,
};

/// Maximum durable steps one `converge` invocation may execute before it
/// returns [`CoordinatorError::IterationLimitExceeded`]. Every step is one
/// durable cross-authority boundary, so a healthy plan needs
/// `expectations + 2` steps; a state-machine regression that stops
/// progressing surfaces as the typed error instead of an unbounded spin
/// (audit 41 D1 / registry 48 #10). A plan that legitimately needs more
/// steps converges across repeated calls: each call resumes from the
/// durable prefix its predecessor committed.
pub const CONVERGE_MAX_STEPS: usize = 256;

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
    /// The convergence loop executed [`CONVERGE_MAX_STEPS`] durable steps
    /// without reaching a terminal plan state. The durable prefix reached
    /// so far stays committed; a later call resumes from it. A recurring
    /// exhaustion means the authority state machine stopped advancing
    /// (audit 41 D1 / registry 48 #10).
    IterationLimitExceeded {
        steps: usize,
    },
    Task(TaskStoreError),
    Artifact(ArtifactError),
    Semantic(SemanticAuthorityError),
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
            Self::IterationLimitExceeded { steps } => write!(
                formatter,
                "coordinator exceeded {steps} convergence steps without reaching a terminal plan state"
            ),
            Self::Task(error) => write!(formatter, "TaskAuthority coordination failure: {error}"),
            Self::Artifact(error) => {
                write!(formatter, "ArtifactAuthority coordination failure: {error}")
            }
            Self::Semantic(error) => {
                write!(formatter, "SemanticAuthority coordination failure: {error}")
            }
        }
    }
}

impl Error for CoordinatorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidTimestamp | Self::IterationLimitExceeded { .. } => None,
            Self::Task(error) => Some(error),
            Self::Artifact(error) => Some(error),
            Self::Semantic(error) => Some(error),
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

impl From<SemanticAuthorityError> for CoordinatorError {
    fn from(error: SemanticAuthorityError) -> Self {
        Self::Semantic(error)
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
                // Terminal state answered idempotently from the durable
                // facts: no second finalize write intent is issued, so the
                // reported step cannot misreport a fresh `Committed`
                // decision as `AlreadyFinalized` (audit 41 D2 / registry
                // 48 coordinator-domain finding).
                let receipt_id =
                    progress
                        .plan
                        .task_receipt_id
                        .ok_or(TaskStoreError::CorruptRecord(
                            "finalized Artifact plan lacks Task receipt",
                        ))?;
                let task_receipt = self
                    .tasks
                    .inspect_receipt(progress.plan.task_id, receipt_id)?;
                Ok(ConvergeStep::AlreadyFinalized(Box::new(
                    ArtifactTaskCommitReceipt {
                        task_receipt,
                        artifact_publications: progress.publications,
                    },
                )))
            }
        }
    }

    /// Repeats bounded steps until the plan is durably finalized, with a
    /// hard per-invocation step budget of [`CONVERGE_MAX_STEPS`].
    ///
    /// # Errors
    ///
    /// Returns the first typed authority failure, or
    /// [`CoordinatorError::IterationLimitExceeded`] when the budget is
    /// exhausted without a terminal state; retrying resumes from the
    /// durable prefix already reached.
    pub fn converge(
        &self,
        request: ConvergeArtifactCommitRequest,
    ) -> Result<ArtifactTaskCommitReceipt, CoordinatorError> {
        for _ in 0..CONVERGE_MAX_STEPS {
            match self.converge_one_step(request)? {
                ConvergeStep::Authorized | ConvergeStep::PublishedOne { .. } => {}
                ConvergeStep::Finalized(receipt) | ConvergeStep::AlreadyFinalized(receipt) => {
                    return Ok(*receipt);
                }
            }
        }
        Err(CoordinatorError::IterationLimitExceeded {
            steps: CONVERGE_MAX_STEPS,
        })
    }

    /// Scans a bounded set of durable non-finalized plans and converges
    /// each one. This is the restart entry point; finalized plans disappear
    /// from subsequent scans.
    ///
    /// Every plan in the snapshot is attempted: a failing plan no longer
    /// starves the plans behind it (registry 48 #11). The successes stay
    /// durably finalized even when a later plan fails.
    ///
    /// # Errors
    ///
    /// Returns the first typed per-plan failure, but only after the whole
    /// snapshot has been attempted. Scan-level failures abort immediately.
    /// Callers that need per-plan failure detail use
    /// [`Self::converge_pending_best_effort`].
    pub fn converge_pending(
        &self,
        limit: usize,
        now_ms: i64,
    ) -> Result<Vec<ArtifactTaskCommitReceipt>, CoordinatorError> {
        let plans = self.tasks.list_incomplete_artifact_commit_plans(limit)?;
        let mut finalized = Vec::with_capacity(plans.len());
        let mut first_failure = None;
        for plan in plans {
            let outcome = self.converge(ConvergeArtifactCommitRequest {
                plan_id: plan.plan_id,
                now_ms,
            });
            match outcome {
                Ok(receipt) => finalized.push(receipt),
                Err(error) => {
                    if first_failure.is_none() {
                        first_failure = Some(error);
                    }
                }
            }
        }
        first_failure.map_or(Ok(finalized), Err)
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

/// One bounded Semantic coordinator invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConvergeSemanticCommitRequest {
    pub plan_id: SemanticCommitPlanId,
    pub now_ms: i64,
}

/// Durable boundary reached by one Semantic coordinator step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConvergeSemanticStep {
    Authorized,
    PublishedOne {
        event_id: nlos_types::SemanticEventId,
        state_after: SemanticCommitPlanState,
    },
    Finalized(Box<SemanticTaskCommitReceipt>),
    AlreadyFinalized(Box<SemanticTaskCommitReceipt>),
}

/// Stateless driver over the Task and Semantic authorities. This first
/// coordinator slice converges Semantic-only plans; the mixed Effect +
/// Semantic finalize request still needs a durable persisted effect-proof
/// envelope before it can be recovered without caller input.
pub struct SemanticCommitCoordinator<'a> {
    tasks: &'a SqliteTaskAuthority,
    semantic: &'a SemanticAuthority,
}

impl<'a> SemanticCommitCoordinator<'a> {
    #[must_use]
    pub const fn new(tasks: &'a SqliteTaskAuthority, semantic: &'a SemanticAuthority) -> Self {
        Self { tasks, semantic }
    }

    /// Advances at most one durable cross-authority Semantic boundary.
    ///
    /// # Errors
    ///
    /// Returns the original typed authority failure. A later call resumes
    /// from the last committed plan prefix.
    pub fn converge_one_step(
        &self,
        request: ConvergeSemanticCommitRequest,
    ) -> Result<ConvergeSemanticStep, CoordinatorError> {
        let published_at_ms =
            u64::try_from(request.now_ms).map_err(|_| CoordinatorError::InvalidTimestamp)?;
        let progress = self
            .tasks
            .inspect_semantic_commit_progress(request.plan_id)?;
        match progress.plan.state {
            SemanticCommitPlanState::Planned => {
                self.tasks
                    .authorize_semantic_publication(request.plan_id, request.now_ms)?;
                Ok(ConvergeSemanticStep::Authorized)
            }
            SemanticCommitPlanState::Publishing => {
                let expectations = self
                    .tasks
                    .inspect_semantic_commit_expectations(request.plan_id)?;
                let expectation = expectations
                    .iter()
                    .find(|expected| {
                        !progress
                            .publications
                            .iter()
                            .any(|receipt| receipt.event_id == expected.event_id)
                    })
                    .ok_or(TaskStoreError::CorruptRecord(
                        "Publishing Semantic plan has no missing expectation",
                    ))?;
                let owner_receipt = self.semantic.publish_semantic_publication(
                    PublishSemanticPublicationRequest {
                        task_id: progress.plan.task_id,
                        permit_id: progress.plan.permit_id,
                        write_set_root: progress.plan.write_set_root,
                        event_id: expectation.event_id,
                        target: semantic_target(expectation.target),
                        admission_receipt_id: expectation.admission_receipt_id,
                        durability_receipt_id: expectation.durability_receipt_id,
                        published_at_ms,
                    },
                )?;
                let nested = nested_semantic_receipt(&owner_receipt.receipt());
                let updated = self.tasks.record_semantic_publications(
                    self.semantic,
                    RecordSemanticPublicationsRequest {
                        plan_id: request.plan_id,
                        receipts: vec![nested],
                        observed_at_ms: request.now_ms,
                    },
                )?;
                Ok(ConvergeSemanticStep::PublishedOne {
                    event_id: expectation.event_id,
                    state_after: updated.plan.state,
                })
            }
            SemanticCommitPlanState::Ready => {
                let decision = self.finalize_ready(request.plan_id, request.now_ms)?;
                Ok(match decision {
                    SemanticFinalizeDecision::Committed(receipt) => {
                        ConvergeSemanticStep::Finalized(receipt)
                    }
                    SemanticFinalizeDecision::Replayed(receipt) => {
                        ConvergeSemanticStep::AlreadyFinalized(receipt)
                    }
                })
            }
            SemanticCommitPlanState::Finalized => {
                // Terminal state answered idempotently from the durable
                // facts: no second finalize write intent is issued, so the
                // reported step cannot misreport a fresh `Committed`
                // decision as `AlreadyFinalized` (audit 41 D2 / registry
                // 48 coordinator-domain finding).
                let receipt_id =
                    progress
                        .plan
                        .task_receipt_id
                        .ok_or(TaskStoreError::CorruptRecord(
                            "finalized Semantic plan lacks Task receipt",
                        ))?;
                let task_receipt = self
                    .tasks
                    .inspect_receipt(progress.plan.task_id, receipt_id)?;
                Ok(ConvergeSemanticStep::AlreadyFinalized(Box::new(
                    SemanticTaskCommitReceipt {
                        task_receipt,
                        semantic_publications: progress.publications,
                    },
                )))
            }
        }
    }

    /// Repeats bounded steps until the Semantic plan is durably finalized,
    /// with a hard per-invocation step budget of [`CONVERGE_MAX_STEPS`].
    ///
    /// # Errors
    ///
    /// Returns the first typed authority failure, or
    /// [`CoordinatorError::IterationLimitExceeded`] when the budget is
    /// exhausted without a terminal state; retrying resumes from the
    /// durable prefix already reached.
    pub fn converge(
        &self,
        request: ConvergeSemanticCommitRequest,
    ) -> Result<SemanticTaskCommitReceipt, CoordinatorError> {
        for _ in 0..CONVERGE_MAX_STEPS {
            match self.converge_one_step(request)? {
                ConvergeSemanticStep::Authorized | ConvergeSemanticStep::PublishedOne { .. } => {}
                ConvergeSemanticStep::Finalized(receipt)
                | ConvergeSemanticStep::AlreadyFinalized(receipt) => return Ok(*receipt),
            }
        }
        Err(CoordinatorError::IterationLimitExceeded {
            steps: CONVERGE_MAX_STEPS,
        })
    }

    /// Scans a bounded set of durable non-finalized Semantic plans and
    /// converges each one. This is the restart entry point for this slice.
    ///
    /// Every plan in the snapshot is attempted: a failing plan no longer
    /// starves the plans behind it (registry 48 #11). The successes stay
    /// durably finalized even when a later plan fails.
    ///
    /// # Errors
    ///
    /// Returns the first typed per-plan failure, but only after the whole
    /// snapshot has been attempted. Scan-level failures abort immediately.
    pub fn converge_pending(
        &self,
        limit: usize,
        now_ms: i64,
    ) -> Result<Vec<SemanticTaskCommitReceipt>, CoordinatorError> {
        let plans = self.tasks.list_incomplete_semantic_commit_plans(limit)?;
        let mut finalized = Vec::with_capacity(plans.len());
        let mut first_failure = None;
        for plan in plans {
            let outcome = self.converge(ConvergeSemanticCommitRequest {
                plan_id: plan.plan_id,
                now_ms,
            });
            match outcome {
                Ok(receipt) => finalized.push(receipt),
                Err(error) => {
                    if first_failure.is_none() {
                        first_failure = Some(error);
                    }
                }
            }
        }
        first_failure.map_or(Ok(finalized), Err)
    }

    fn finalize_ready(
        &self,
        plan_id: SemanticCommitPlanId,
        finalized_at_ms: i64,
    ) -> Result<SemanticFinalizeDecision, CoordinatorError> {
        if self
            .tasks
            .inspect_semantic_finalize_envelope(plan_id)?
            .is_some()
        {
            return Ok(self
                .tasks
                .finalize_commit_v3_with_persisted_semantic_envelope(
                    self.semantic,
                    plan_id,
                    finalized_at_ms,
                )?);
        }
        Ok(self
            .tasks
            .finalize_semantic_commit(nlos_task::FinalizeSemanticCommitRequest {
                plan_id,
                finalized_at_ms,
            })?)
    }
}

fn semantic_target(target: TaskWriteSetSemanticTarget) -> CapabilityTarget {
    match target {
        TaskWriteSetSemanticTarget::Namespace(namespace) => CapabilityTarget::Namespace(namespace),
        TaskWriteSetSemanticTarget::Task(task) => CapabilityTarget::Task(task),
    }
}

fn nested_semantic_receipt(
    receipt: &SemanticPublicationReceipt,
) -> nlos_task::NestedSemanticPublicationReceipt {
    nlos_task::NestedSemanticPublicationReceipt {
        receipt_id: receipt.receipt_id,
        task_id: receipt.task_id,
        permit_id: receipt.permit_id,
        write_set_root: receipt.write_set_root,
        event_id: receipt.event_id,
        target: match receipt.target {
            CapabilityTarget::Namespace(namespace) => {
                TaskWriteSetSemanticTarget::Namespace(namespace)
            }
            CapabilityTarget::Task(task) => TaskWriteSetSemanticTarget::Task(task),
        },
        log_seq: receipt.log_seq,
        admission_receipt_id: receipt.admission_receipt_id,
        durability_receipt_id: receipt.durability_receipt_id,
        semantic_checkpoint_after: receipt.semantic_checkpoint_after,
        created_at_ms: receipt.created_at_ms,
    }
}
