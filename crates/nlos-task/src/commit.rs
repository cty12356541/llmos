//! Artifact publication planning for the recoverable Task commit protocol.
//!
//! Schema v6 lands the durable immutable plan before publication. Schema v7
//! consumes immutable Artifact publication receipts and exposes partial
//! `Publishing` versus complete `Ready` state without closing the permit or
//! advancing `TaskHead`.

use std::fmt;

use nlos_types::{ArtifactId, CommitPermitId, IdempotencyKey, ReceiptId, TaskAttemptId, TaskId};
use rusqlite::{Row, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::store::{
    SqlRead, SqliteTaskAuthority, close_permit, encode_u64, insert_receipt, load_attempt,
    load_permit_by_id, load_receipt, load_task, optional_blob16, set_attempt_state, update_task,
};
use crate::{AttemptState, PermitState, ReceiptOutcome, TaskReceiptRecord, TaskStoreError};

/// Authority-derived identity of one Artifact commit plan.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtifactCommitPlanId([u8; 16]);

impl ArtifactCommitPlanId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for ArtifactCommitPlanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ArtifactCommitPlanId(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// One staged Artifact revision expected by an artifact-only write set.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ArtifactPublicationExpectation {
    /// Authority-issued staging identity from `ArtifactAuthority`.
    pub staging_id: [u8; 16],
    pub artifact_id: ArtifactId,
    pub target_revision: u64,
    pub digest: [u8; 32],
    pub size_bytes: u64,
}

/// Durable lifecycle of an Artifact commit plan. Schema v7 produces
/// `Planned`, `Publishing`, and `Ready`; `Finalized` remains reserved for
/// the Task finalize slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactCommitPlanState {
    Planned,
    Publishing,
    Ready,
    Finalized,
}

impl ArtifactCommitPlanState {
    const fn code(self) -> i64 {
        match self {
            Self::Planned => 0,
            Self::Publishing => 1,
            Self::Ready => 2,
            Self::Finalized => 3,
        }
    }

    fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Planned),
            1 => Ok(Self::Publishing),
            2 => Ok(Self::Ready),
            3 => Ok(Self::Finalized),
            _ => Err(TaskStoreError::CorruptRecord(
                "unknown artifact commit plan state",
            )),
        }
    }
}

/// Request to durably bind an Artifact publication plan to an issued
/// `CommitPermit`. This does not authorize publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanArtifactCommitRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: nlos_types::Generation,
    pub permit_id: CommitPermitId,
    pub expectations: Vec<ArtifactPublicationExpectation>,
    pub idempotency_key: IdempotencyKey,
    pub planned_at_ms: i64,
}

/// Durable Artifact commit plan and its canonical expectation order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactCommitPlanRecord {
    pub plan_id: ArtifactCommitPlanId,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: nlos_types::Generation,
    pub write_set_root: [u8; 32],
    pub expectations: Vec<ArtifactPublicationExpectation>,
    pub state: ArtifactCommitPlanState,
    pub task_receipt_id: Option<ReceiptId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Artifact publication receipt consumed as nested Task commit evidence.
/// The shape mirrors the authority output without depending on its
/// concrete implementation crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NestedArtifactPublicationReceipt {
    pub receipt_id: ReceiptId,
    pub staging_id: [u8; 16],
    pub artifact_id: ArtifactId,
    pub revision: u64,
    pub digest: [u8; 32],
    pub size_bytes: u64,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: [u8; 32],
    pub prior_head_revision: u64,
    pub prior_head_digest: Option<[u8; 32]>,
    pub new_head_revision: u64,
    pub new_head_digest: [u8; 32],
    pub created_at_ms: i64,
}

/// Request to consume one or more authoritative Artifact publication
/// receipts against an immutable plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordArtifactPublicationsRequest {
    pub plan_id: ArtifactCommitPlanId,
    pub receipts: Vec<NestedArtifactPublicationReceipt>,
    pub observed_at_ms: i64,
}

/// Queryable partial/ready state of a planned Artifact commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactCommitProgress {
    pub plan: ArtifactCommitPlanRecord,
    pub publications: Vec<NestedArtifactPublicationReceipt>,
}

/// Final Task receipt together with the immutable Artifact receipts it
/// nests as commit evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactTaskCommitReceipt {
    pub task_receipt: TaskReceiptRecord,
    pub artifact_publications: Vec<NestedArtifactPublicationReceipt>,
}

/// Request to atomically finalize one complete artifact-only plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalizeArtifactCommitRequest {
    pub plan_id: ArtifactCommitPlanId,
    pub finalized_at_ms: i64,
}

/// Idempotent Artifact-aware Task finalize decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactFinalizeDecision {
    Committed(Box<ArtifactTaskCommitReceipt>),
    Replayed(Box<ArtifactTaskCommitReceipt>),
}

impl ArtifactFinalizeDecision {
    #[must_use]
    pub fn receipt(&self) -> &ArtifactTaskCommitReceipt {
        match self {
            Self::Committed(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Idempotent plan decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactCommitPlanDecision {
    Planned(Box<ArtifactCommitPlanRecord>),
    Replayed(Box<ArtifactCommitPlanRecord>),
}

/// Idempotent decision that fences an immutable plan immediately before
/// the first canonical Artifact publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactPublicationAuthorizationDecision {
    Authorized(Box<ArtifactCommitPlanRecord>),
    Replayed(Box<ArtifactCommitPlanRecord>),
}

impl ArtifactPublicationAuthorizationDecision {
    #[must_use]
    pub fn record(&self) -> &ArtifactCommitPlanRecord {
        match self {
            Self::Authorized(record) | Self::Replayed(record) => record,
        }
    }
}

impl ArtifactCommitPlanDecision {
    #[must_use]
    pub fn record(&self) -> &ArtifactCommitPlanRecord {
        match self {
            Self::Planned(record) | Self::Replayed(record) => record,
        }
    }
}

/// Canonical domain-separated root of an Artifact publication expectation
/// set. Caller order is ignored; duplicate staging identities or duplicate
/// `(ArtifactId, target_revision)` slots fail closed.
///
/// # Errors
///
/// Returns [`TaskStoreError::InvalidArtifactPublicationPlan`] for an empty
/// or ambiguous set.
pub fn artifact_publication_plan_root(
    expectations: &[ArtifactPublicationExpectation],
) -> Result<[u8; 32], TaskStoreError> {
    let canonical = canonical_expectations(expectations)?;
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-artifact-publication-plan/v1");
    hasher.update(
        u64::try_from(canonical.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for (ordinal, expectation) in canonical.iter().enumerate() {
        hasher.update(u64::try_from(ordinal).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(expectation.staging_id);
        hasher.update(expectation.artifact_id.as_bytes());
        hasher.update(expectation.target_revision.to_be_bytes());
        hasher.update(expectation.digest);
        hasher.update(expectation.size_bytes.to_be_bytes());
    }
    Ok(hasher.finalize().into())
}

fn canonical_expectations(
    expectations: &[ArtifactPublicationExpectation],
) -> Result<Vec<ArtifactPublicationExpectation>, TaskStoreError> {
    if expectations.is_empty() {
        return Err(TaskStoreError::InvalidArtifactPublicationPlan {
            reason: "at least one Artifact publication is required",
        });
    }
    if expectations.iter().any(|item| item.target_revision == 0) {
        return Err(TaskStoreError::InvalidArtifactPublicationPlan {
            reason: "target revision must be non-zero",
        });
    }
    let mut canonical = expectations.to_vec();
    canonical.sort_unstable_by_key(|item| {
        (
            item.artifact_id.into_bytes(),
            item.target_revision,
            item.staging_id,
        )
    });
    let mut staging_ids = std::collections::BTreeSet::new();
    for expectation in &canonical {
        if !staging_ids.insert(expectation.staging_id) {
            return Err(TaskStoreError::InvalidArtifactPublicationPlan {
                reason: "duplicate staging identity",
            });
        }
    }
    for pair in canonical.windows(2) {
        if pair[0].artifact_id == pair[1].artifact_id
            && pair[0].target_revision == pair[1].target_revision
        {
            return Err(TaskStoreError::InvalidArtifactPublicationPlan {
                reason: "duplicate Artifact revision slot",
            });
        }
    }
    Ok(canonical)
}

impl SqliteTaskAuthority {
    /// Durably records an immutable Artifact publication plan for one
    /// issued permit. The plan root must exactly equal the permit's
    /// artifact-only `write_set_root`; this call does not authorize any
    /// canonical Artifact publication.
    ///
    /// # Errors
    ///
    /// Returns a typed plan, holder, head, membership, idempotency, or
    /// storage error.
    // By-value request mirrors the other mutating authority APIs; canonical
    // ordering intentionally preserves the caller's request for replay
    // comparison instead of consuming individual fields.
    #[allow(clippy::needless_pass_by_value)]
    pub fn plan_artifact_commit(
        &self,
        request: PlanArtifactCommitRequest,
    ) -> Result<ArtifactCommitPlanDecision, TaskStoreError> {
        let canonical = canonical_expectations(&request.expectations)?;
        let root = artifact_publication_plan_root(&canonical)?;
        let plan_id = derive_plan_id(request.permit_id);
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) =
            load_plan_by_key(&transaction, request.task_id, request.idempotency_key)?
        {
            if same_plan_request(&existing, &request, &canonical) {
                transaction.commit()?;
                return Ok(ArtifactCommitPlanDecision::Replayed(Box::new(existing)));
            }
            return Err(TaskStoreError::IdempotencyConflict);
        }
        if load_plan_optional(&transaction, plan_id)?.is_some() {
            return Err(TaskStoreError::IdempotencyConflict);
        }

        let task = load_task(&transaction, request.task_id)?;
        let permit = load_permit_by_id(&transaction, request.task_id, request.permit_id)?;
        let attempt = load_attempt(&transaction, request.task_id, request.attempt_id)?;
        if attempt.attempt_generation != request.attempt_generation {
            return Err(TaskStoreError::InvalidGeneration);
        }
        if permit.attempt_id != request.attempt_id
            || permit.attempt_generation != request.attempt_generation
        {
            return Err(TaskStoreError::NotPermitHolder);
        }
        if permit.state != PermitState::Issued {
            return Err(TaskStoreError::PermitNotIssued);
        }
        if task.record.head_commit_seq != permit.expected_head_commit_seq
            || task.record.head_effect_history_root != permit.expected_effect_history_root
            || task.record.retry_fence_epoch != permit.expected_retry_fence_epoch
        {
            return Err(TaskStoreError::StaleTaskHead);
        }
        crate::group::validate_commit_binding(
            &transaction,
            attempt.attempt_id,
            permit.group_binding,
        )?;
        if root != permit.write_set_root {
            return Err(TaskStoreError::InvalidArtifactPublicationPlan {
                reason: "canonical plan root differs from permit write_set_root",
            });
        }

        let record = ArtifactCommitPlanRecord {
            plan_id,
            task_id: request.task_id,
            permit_id: request.permit_id,
            attempt_id: request.attempt_id,
            attempt_generation: request.attempt_generation,
            write_set_root: root,
            expectations: canonical,
            state: ArtifactCommitPlanState::Planned,
            task_receipt_id: None,
            created_at_ms: request.planned_at_ms,
            updated_at_ms: request.planned_at_ms,
        };
        insert_plan(&transaction, &record, request.idempotency_key)?;
        insert_expectations(&transaction, &record)?;
        transaction.commit()?;
        Ok(ArtifactCommitPlanDecision::Planned(Box::new(record)))
    }

    /// Reads one durable Artifact commit plan and its canonical expectation
    /// list.
    ///
    /// # Errors
    ///
    /// Returns `ArtifactCommitPlanNotFound`, corrupt-record, or storage
    /// errors.
    pub fn inspect_artifact_commit_plan(
        &self,
        plan_id: ArtifactCommitPlanId,
    ) -> Result<ArtifactCommitPlanRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_plan_optional(&*connection, plan_id)?.ok_or(TaskStoreError::ArtifactCommitPlanNotFound)
    }

    /// Revalidates every live Task-side fence and authorizes canonical
    /// Artifact publication by durably moving `Planned` to `Publishing`.
    /// Exact retries after that transition replay the durable decision.
    ///
    /// The current cross-authority slice is deliberately artifact-only:
    /// any declared effect slot rejects authorization fail-closed.
    ///
    /// # Errors
    ///
    /// Returns a typed plan/permit/head/membership validation error or a
    /// storage error without authorizing publication.
    pub fn authorize_artifact_publication(
        &self,
        plan_id: ArtifactCommitPlanId,
        authorized_at_ms: i64,
    ) -> Result<ArtifactPublicationAuthorizationDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut plan = load_plan_optional(&transaction, plan_id)?
            .ok_or(TaskStoreError::ArtifactCommitPlanNotFound)?;
        if plan.state != ArtifactCommitPlanState::Planned {
            transaction.commit()?;
            return Ok(ArtifactPublicationAuthorizationDecision::Replayed(
                Box::new(plan),
            ));
        }

        let task = load_task(&transaction, plan.task_id)?;
        let permit = load_permit_by_id(&transaction, plan.task_id, plan.permit_id)?;
        let attempt = load_attempt(&transaction, plan.task_id, plan.attempt_id)?;
        if attempt.attempt_generation != plan.attempt_generation {
            return Err(TaskStoreError::InvalidGeneration);
        }
        if permit.attempt_id != plan.attempt_id
            || permit.attempt_generation != plan.attempt_generation
        {
            return Err(TaskStoreError::NotPermitHolder);
        }
        if permit.state != PermitState::Issued {
            return Err(TaskStoreError::PermitNotIssued);
        }
        if permit.write_set_root != plan.write_set_root {
            return Err(TaskStoreError::InvalidArtifactPublicationPlan {
                reason: "durable plan root differs from permit write_set_root",
            });
        }
        if task.record.head_commit_seq != permit.expected_head_commit_seq
            || task.record.head_effect_history_root != permit.expected_effect_history_root
            || task.record.retry_fence_epoch != permit.expected_retry_fence_epoch
        {
            return Err(TaskStoreError::StaleTaskHead);
        }
        crate::group::validate_commit_binding(
            &transaction,
            attempt.attempt_id,
            permit.group_binding,
        )?;
        let effect_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM effect_slots WHERE permit_id = ?1",
            [plan.permit_id.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        if effect_count != 0 {
            return Err(TaskStoreError::InvalidArtifactPublicationPlan {
                reason: "Artifact publication authorization requires an artifact-only permit",
            });
        }

        update_plan_state(
            &transaction,
            plan.plan_id,
            ArtifactCommitPlanState::Planned,
            ArtifactCommitPlanState::Publishing,
            authorized_at_ms,
        )?;
        plan.state = ArtifactCommitPlanState::Publishing;
        plan.updated_at_ms = authorized_at_ms;
        transaction.commit()?;
        Ok(ArtifactPublicationAuthorizationDecision::Authorized(
            Box::new(plan),
        ))
    }

    /// Consumes Artifact publication receipts idempotently. A partial set
    /// advances the plan to `Publishing`; a complete exact set advances it
    /// to `Ready`. Neither state closes the permit or advances `TaskHead`.
    ///
    /// # Errors
    ///
    /// Returns a typed plan-not-found, publication-conflict, corrupt-record,
    /// or storage error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn record_artifact_publications(
        &self,
        request: RecordArtifactPublicationsRequest,
    ) -> Result<ArtifactCommitProgress, TaskStoreError> {
        if request.receipts.is_empty() {
            return Err(TaskStoreError::ArtifactPublicationConflict {
                reason: "receipt batch must not be empty",
            });
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut plan = load_plan_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::ArtifactCommitPlanNotFound)?;
        match plan.state {
            ArtifactCommitPlanState::Planned => {
                return Err(TaskStoreError::ArtifactPublicationConflict {
                    reason: "Artifact publication was not authorized by TaskAuthority",
                });
            }
            ArtifactCommitPlanState::Publishing | ArtifactCommitPlanState::Ready => {}
            ArtifactCommitPlanState::Finalized => {
                return Err(TaskStoreError::ArtifactPublicationConflict {
                    reason: "finalized plan cannot consume publication receipts",
                });
            }
        }

        let mut inserted_any = false;
        for receipt in &request.receipts {
            validate_publication_receipt(&plan, receipt)?;
            if let Some(existing) =
                load_publication_by_staging(&transaction, plan.plan_id, &receipt.staging_id)?
            {
                if existing != *receipt {
                    return Err(TaskStoreError::ArtifactPublicationConflict {
                        reason: "staging expectation already consumed by different receipt",
                    });
                }
                continue;
            }
            if load_publication_by_receipt_id(&transaction, receipt.receipt_id)?.is_some() {
                return Err(TaskStoreError::ArtifactPublicationConflict {
                    reason: "receipt identity already belongs to another staging slot",
                });
            }
            insert_publication(&transaction, plan.plan_id, receipt)?;
            inserted_any = true;
        }

        let publications = load_publications(&transaction, plan.plan_id)?;
        let state = if publications.len() == plan.expectations.len() {
            ArtifactCommitPlanState::Ready
        } else {
            ArtifactCommitPlanState::Publishing
        };
        if inserted_any || plan.state != state {
            update_plan_state(
                &transaction,
                plan.plan_id,
                plan.state,
                state,
                request.observed_at_ms,
            )?;
            plan.state = state;
            plan.updated_at_ms = request.observed_at_ms;
        }
        transaction.commit()?;
        Ok(ArtifactCommitProgress { plan, publications })
    }

    /// Reads a plan together with all nested Artifact publication receipts
    /// consumed so far.
    ///
    /// # Errors
    ///
    /// Returns `ArtifactCommitPlanNotFound`, corrupt-record, or storage
    /// errors.
    pub fn inspect_artifact_commit_progress(
        &self,
        plan_id: ArtifactCommitPlanId,
    ) -> Result<ArtifactCommitProgress, TaskStoreError> {
        let connection = self.lock_connection()?;
        let plan = load_plan_optional(&*connection, plan_id)?
            .ok_or(TaskStoreError::ArtifactCommitPlanNotFound)?;
        let publications = load_publications(&*connection, plan_id)?;
        validate_progress(&plan, &publications)?;
        Ok(ArtifactCommitProgress { plan, publications })
    }

    /// Lists non-finalized Artifact plans in stable creation/identity order
    /// for a restart coordinator scan.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-record or storage error.
    pub fn list_incomplete_artifact_commit_plans(
        &self,
        limit: usize,
    ) -> Result<Vec<ArtifactCommitPlanRecord>, TaskStoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT plan_id FROM task_artifact_commit_plans
             WHERE plan_state != ?1 ORDER BY created_at_ms, plan_id LIMIT ?2",
        )?;
        let mut rows = statement.query(params![
            ArtifactCommitPlanState::Finalized.code(),
            i64::try_from(limit).unwrap_or(i64::MAX),
        ])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(ArtifactCommitPlanId::from_bytes(blob16(row, 0)?));
        }
        drop(rows);
        drop(statement);
        ids.into_iter()
            .map(|plan_id| {
                load_plan_optional(&*connection, plan_id)?
                    .ok_or(TaskStoreError::ArtifactCommitPlanNotFound)
            })
            .collect()
    }

    /// Atomically closes an artifact-only permit after every planned
    /// publication receipt is durable, advances `TaskHead`, links the Task
    /// receipt, and marks the plan `Finalized`.
    ///
    /// # Errors
    ///
    /// Returns a typed readiness/holder/head/membership error or storage
    /// error; no subset of the terminal facts is committed on failure.
    pub fn finalize_artifact_commit(
        &self,
        request: FinalizeArtifactCommitRequest,
    ) -> Result<ArtifactFinalizeDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut plan = load_plan_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::ArtifactCommitPlanNotFound)?;
        let publications = load_publications(&transaction, plan.plan_id)?;
        validate_progress(&plan, &publications)?;
        if plan.state == ArtifactCommitPlanState::Finalized {
            let receipt_id = plan.task_receipt_id.ok_or(TaskStoreError::CorruptRecord(
                "finalized Artifact plan lacks Task receipt",
            ))?;
            let task_receipt = load_receipt(&transaction, plan.task_id, receipt_id)?;
            transaction.commit()?;
            return Ok(ArtifactFinalizeDecision::Replayed(Box::new(
                ArtifactTaskCommitReceipt {
                    task_receipt,
                    artifact_publications: publications,
                },
            )));
        }
        if plan.state != ArtifactCommitPlanState::Ready {
            return Err(TaskStoreError::ArtifactCommitPlanNotReady { state: plan.state });
        }

        let task_receipt = finalize_ready_plan(&transaction, &plan, request.finalized_at_ms)?;
        let receipt_id = task_receipt.receipt_id;
        plan.state = ArtifactCommitPlanState::Finalized;
        plan.task_receipt_id = Some(receipt_id);
        plan.updated_at_ms = request.finalized_at_ms;
        transaction.commit()?;
        Ok(ArtifactFinalizeDecision::Committed(Box::new(
            ArtifactTaskCommitReceipt {
                task_receipt,
                artifact_publications: publications,
            },
        )))
    }
}

fn finalize_ready_plan(
    transaction: &rusqlite::Transaction<'_>,
    plan: &ArtifactCommitPlanRecord,
    finalized_at_ms: i64,
) -> Result<TaskReceiptRecord, TaskStoreError> {
    let task = load_task(transaction, plan.task_id)?;
    let permit = load_permit_by_id(transaction, plan.task_id, plan.permit_id)?;
    let attempt = load_attempt(transaction, plan.task_id, plan.attempt_id)?;
    if attempt.attempt_generation != plan.attempt_generation {
        return Err(TaskStoreError::InvalidGeneration);
    }
    if permit.attempt_id != plan.attempt_id || permit.attempt_generation != plan.attempt_generation
    {
        return Err(TaskStoreError::NotPermitHolder);
    }
    if permit.state != PermitState::Issued {
        return Err(TaskStoreError::PermitNotIssued);
    }
    if permit.write_set_root != plan.write_set_root {
        return Err(TaskStoreError::InvalidArtifactPublicationPlan {
            reason: "durable plan root differs from permit write_set_root",
        });
    }
    if task.record.head_commit_seq != permit.expected_head_commit_seq
        || task.record.head_effect_history_root != permit.expected_effect_history_root
        || task.record.retry_fence_epoch != permit.expected_retry_fence_epoch
    {
        return Err(TaskStoreError::StaleTaskHead);
    }
    crate::group::validate_commit_binding(transaction, attempt.attempt_id, permit.group_binding)?;
    let effect_count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM effect_slots WHERE permit_id = ?1",
        [plan.permit_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if effect_count != 0 {
        return Err(TaskStoreError::InvalidArtifactPublicationPlan {
            reason: "Artifact finalize requires an artifact-only permit",
        });
    }
    let new_seq = task
        .record
        .head_commit_seq
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let control_epoch = task
        .record
        .control_epoch
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let receipt_id = crate::model::derive_commit_receipt_id(permit.permit_id);
    let receipt = TaskReceiptRecord {
        receipt_id,
        task_id: plan.task_id,
        permit_id: Some(permit.permit_id),
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        group_binding: permit.group_binding,
        outcome: ReceiptOutcome::Committed,
        prior_head_commit_seq: task.record.head_commit_seq,
        prior_effect_history_root: task.record.head_effect_history_root,
        prior_retry_fence_epoch: task.record.retry_fence_epoch,
        new_head_commit_seq: new_seq,
        new_effect_history_root: task.record.head_effect_history_root,
        new_retry_fence_epoch: task.record.retry_fence_epoch,
        created_at_ms: finalized_at_ms,
    };
    insert_receipt(transaction, &receipt)?;
    close_permit(transaction, &permit, finalized_at_ms)?;
    set_attempt_state(
        transaction,
        &attempt,
        AttemptState::Committed,
        Some(receipt_id),
        finalized_at_ms,
    )?;
    update_task(transaction, &task, finalized_at_ms, |record| {
        record.head_commit_seq = new_seq;
        record.control_epoch = control_epoch;
    })?;
    finalize_plan(transaction, plan.plan_id, receipt_id, finalized_at_ms)?;
    Ok(receipt)
}

fn validate_publication_receipt(
    plan: &ArtifactCommitPlanRecord,
    receipt: &NestedArtifactPublicationReceipt,
) -> Result<(), TaskStoreError> {
    if receipt.task_id != plan.task_id
        || receipt.permit_id != plan.permit_id
        || receipt.write_set_root != plan.write_set_root
    {
        return Err(TaskStoreError::ArtifactPublicationConflict {
            reason: "Task/Permit/write-set binding differs from plan",
        });
    }
    let expectation = plan
        .expectations
        .iter()
        .find(|item| item.staging_id == receipt.staging_id)
        .ok_or(TaskStoreError::ArtifactPublicationConflict {
            reason: "receipt staging identity is absent from plan",
        })?;
    if expectation.artifact_id != receipt.artifact_id
        || expectation.target_revision != receipt.revision
        || expectation.digest != receipt.digest
        || expectation.size_bytes != receipt.size_bytes
    {
        return Err(TaskStoreError::ArtifactPublicationConflict {
            reason: "receipt content differs from planned Artifact revision",
        });
    }
    if receipt.new_head_revision != receipt.revision
        || receipt.new_head_digest != receipt.digest
        || receipt.prior_head_revision.checked_add(1) != Some(receipt.new_head_revision)
        || (receipt.prior_head_revision == 0) != receipt.prior_head_digest.is_none()
    {
        return Err(TaskStoreError::ArtifactPublicationConflict {
            reason: "receipt head transition is internally inconsistent",
        });
    }
    Ok(())
}

fn validate_progress(
    plan: &ArtifactCommitPlanRecord,
    publications: &[NestedArtifactPublicationReceipt],
) -> Result<(), TaskStoreError> {
    for receipt in publications {
        validate_publication_receipt(plan, receipt).map_err(|_| {
            TaskStoreError::CorruptRecord("stored publication receipt disagrees with plan")
        })?;
    }
    let expected_state = if publications.is_empty() {
        if plan.state == ArtifactCommitPlanState::Publishing {
            ArtifactCommitPlanState::Publishing
        } else {
            ArtifactCommitPlanState::Planned
        }
    } else if publications.len() == plan.expectations.len() {
        ArtifactCommitPlanState::Ready
    } else {
        ArtifactCommitPlanState::Publishing
    };
    let state_matches = match plan.state {
        ArtifactCommitPlanState::Finalized => expected_state == ArtifactCommitPlanState::Ready,
        state => state == expected_state,
    };
    if !state_matches {
        return Err(TaskStoreError::CorruptRecord(
            "artifact commit plan state disagrees with publication count",
        ));
    }
    Ok(())
}

pub(crate) fn group_has_publication_in_flight(
    source: &impl SqlRead,
    group_id: crate::TaskGroupId,
) -> Result<bool, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT COUNT(*)
         FROM task_artifact_commit_plans AS plans
         JOIN commit_permits AS permits ON permits.permit_id = plans.permit_id
         WHERE permits.group_id = ?1 AND plans.plan_state IN (1, 2)",
    )?;
    let count: i64 = statement.query_row([group_id.as_bytes().as_slice()], |row| row.get(0))?;
    Ok(count != 0)
}

fn update_plan_state(
    transaction: &rusqlite::Transaction<'_>,
    plan_id: ArtifactCommitPlanId,
    old: ArtifactCommitPlanState,
    new: ArtifactCommitPlanState,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE task_artifact_commit_plans
         SET plan_state = ?1, updated_at_ms = ?2
         WHERE plan_id = ?3 AND plan_state = ?4",
        params![
            new.code(),
            now_ms,
            plan_id.as_bytes().as_slice(),
            old.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "artifact commit plan state compare-and-swap failed",
        ));
    }
    Ok(())
}

fn finalize_plan(
    transaction: &rusqlite::Transaction<'_>,
    plan_id: ArtifactCommitPlanId,
    receipt_id: ReceiptId,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE task_artifact_commit_plans
         SET plan_state = ?1, task_receipt_id = ?2, updated_at_ms = ?3
         WHERE plan_id = ?4 AND plan_state = ?5 AND task_receipt_id IS NULL",
        params![
            ArtifactCommitPlanState::Finalized.code(),
            receipt_id.as_bytes().as_slice(),
            now_ms,
            plan_id.as_bytes().as_slice(),
            ArtifactCommitPlanState::Ready.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "Artifact plan finalize compare-and-swap failed",
        ));
    }
    Ok(())
}

fn same_plan_request(
    existing: &ArtifactCommitPlanRecord,
    request: &PlanArtifactCommitRequest,
    canonical: &[ArtifactPublicationExpectation],
) -> bool {
    existing.plan_id == derive_plan_id(request.permit_id)
        && existing.task_id == request.task_id
        && existing.permit_id == request.permit_id
        && existing.attempt_id == request.attempt_id
        && existing.attempt_generation == request.attempt_generation
        && existing.expectations == canonical
}

fn derive_plan_id(permit_id: CommitPermitId) -> ArtifactCommitPlanId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-artifact-commit-plan/v1");
    hasher.update(permit_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ArtifactCommitPlanId::from_bytes(bytes)
}

fn insert_plan(
    transaction: &rusqlite::Transaction<'_>,
    record: &ArtifactCommitPlanRecord,
    idempotency_key: IdempotencyKey,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_artifact_commit_plans (
            plan_id, task_id, permit_id, idempotency_key, attempt_id,
            attempt_generation, write_set_root, artifact_plan_root,
            expected_artifact_count, plan_state, task_receipt_id,
            created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, NULL, ?10, ?10)",
        params![
            record.plan_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record.permit_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            record.attempt_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            record.write_set_root.as_slice(),
            encode_u64(u64::try_from(record.expectations.len()).map_err(|_| {
                TaskStoreError::InvalidArtifactPublicationPlan {
                    reason: "expectation count exceeds u64",
                }
            })?)
            .as_slice(),
            record.state.code(),
            record.created_at_ms,
        ],
    )?;
    Ok(())
}

fn insert_expectations(
    transaction: &rusqlite::Transaction<'_>,
    record: &ArtifactCommitPlanRecord,
) -> Result<(), TaskStoreError> {
    for (ordinal, expectation) in record.expectations.iter().enumerate() {
        transaction.execute(
            "INSERT INTO task_artifact_publication_expectations (
                plan_id, ordinal, staging_id, artifact_id, target_revision,
                digest, size_bytes
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.plan_id.as_bytes().as_slice(),
                encode_u64(u64::try_from(ordinal).map_err(|_| {
                    TaskStoreError::InvalidArtifactPublicationPlan {
                        reason: "expectation ordinal exceeds u64",
                    }
                })?)
                .as_slice(),
                expectation.staging_id.as_slice(),
                expectation.artifact_id.as_bytes().as_slice(),
                encode_u64(expectation.target_revision).as_slice(),
                expectation.digest.as_slice(),
                encode_u64(expectation.size_bytes).as_slice(),
            ],
        )?;
    }
    Ok(())
}

const PLAN_COLUMNS: &str = "plan_id, task_id, permit_id, attempt_id,
     attempt_generation, write_set_root, artifact_plan_root,
     expected_artifact_count, plan_state, task_receipt_id, created_at_ms, updated_at_ms";

fn load_plan_optional(
    source: &impl SqlRead,
    plan_id: ArtifactCommitPlanId,
) -> Result<Option<ArtifactCommitPlanRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PLAN_COLUMNS} FROM task_artifact_commit_plans WHERE plan_id = ?1"
    ))?;
    let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| decode_plan_row(source, row))
        .transpose()
}

fn load_plan_by_key(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<ArtifactCommitPlanRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PLAN_COLUMNS} FROM task_artifact_commit_plans
         WHERE task_id = ?1 AND idempotency_key = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?
        .map(|row| decode_plan_row(source, row))
        .transpose()
}

fn decode_plan_row(
    source: &impl SqlRead,
    row: &Row<'_>,
) -> Result<ArtifactCommitPlanRecord, TaskStoreError> {
    let plan_id = ArtifactCommitPlanId::from_bytes(blob16(row, 0)?);
    let write_set_root = blob32(row, 5)?;
    let artifact_plan_root = blob32(row, 6)?;
    let expected_count = u64_from_blob(row, 7)?;
    let expectations = load_expectations(source, plan_id)?;
    let actual_count = u64::try_from(expectations.len())
        .map_err(|_| TaskStoreError::CorruptRecord("artifact expectation count overflows"))?;
    let recomputed_root = artifact_publication_plan_root(&expectations).map_err(|_| {
        TaskStoreError::CorruptRecord("durable artifact expectations are ambiguous")
    })?;
    if artifact_plan_root != write_set_root
        || expected_count != actual_count
        || recomputed_root != write_set_root
    {
        return Err(TaskStoreError::CorruptRecord(
            "artifact commit plan root/count disagrees with expectations",
        ));
    }
    Ok(ArtifactCommitPlanRecord {
        plan_id,
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 2)?),
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 3)?),
        attempt_generation: generation_from_blob(row, 4)?,
        write_set_root,
        state: ArtifactCommitPlanState::from_code(row.get(8)?)?,
        expectations,
        task_receipt_id: optional_blob16(row, 9)?.map(ReceiptId::from_bytes),
        created_at_ms: row.get(10)?,
        updated_at_ms: row.get(11)?,
    })
}

fn load_expectations(
    source: &impl SqlRead,
    plan_id: ArtifactCommitPlanId,
) -> Result<Vec<ArtifactPublicationExpectation>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT staging_id, artifact_id, target_revision, digest, size_bytes
         FROM task_artifact_publication_expectations
         WHERE plan_id = ?1 ORDER BY ordinal",
    )?;
    let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(ArtifactPublicationExpectation {
            staging_id: blob16(row, 0)?,
            artifact_id: ArtifactId::from_bytes(blob16(row, 1)?),
            target_revision: u64_from_blob(row, 2)?,
            digest: blob32(row, 3)?,
            size_bytes: u64_from_blob(row, 4)?,
        });
    }
    Ok(out)
}

fn insert_publication(
    transaction: &rusqlite::Transaction<'_>,
    plan_id: ArtifactCommitPlanId,
    receipt: &NestedArtifactPublicationReceipt,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_artifact_publication_receipts (
            plan_id, receipt_id, staging_id, artifact_id, revision, digest,
            size_bytes, task_id, permit_id, write_set_root,
            prior_head_revision, prior_head_digest, new_head_revision,
            new_head_digest, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            plan_id.as_bytes().as_slice(),
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.staging_id.as_slice(),
            receipt.artifact_id.as_bytes().as_slice(),
            encode_u64(receipt.revision).as_slice(),
            receipt.digest.as_slice(),
            encode_u64(receipt.size_bytes).as_slice(),
            receipt.task_id.as_bytes().as_slice(),
            receipt.permit_id.as_bytes().as_slice(),
            receipt.write_set_root.as_slice(),
            encode_u64(receipt.prior_head_revision).as_slice(),
            receipt.prior_head_digest.as_ref().map(<[u8; 32]>::as_slice),
            encode_u64(receipt.new_head_revision).as_slice(),
            receipt.new_head_digest.as_slice(),
            receipt.created_at_ms,
        ],
    )?;
    Ok(())
}

const PUBLICATION_COLUMNS: &str = "receipt_id, staging_id, artifact_id, revision,
     digest, size_bytes, task_id, permit_id, write_set_root,
     prior_head_revision, prior_head_digest, new_head_revision,
     new_head_digest, created_at_ms";

fn load_publication_by_staging(
    source: &impl SqlRead,
    plan_id: ArtifactCommitPlanId,
    staging_id: &[u8; 16],
) -> Result<Option<NestedArtifactPublicationReceipt>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PUBLICATION_COLUMNS} FROM task_artifact_publication_receipts
         WHERE plan_id = ?1 AND staging_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        plan_id.as_bytes().as_slice(),
        staging_id.as_slice(),
    ])?;
    rows.next()?.map(decode_publication_row).transpose()
}

fn load_publication_by_receipt_id(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<Option<NestedArtifactPublicationReceipt>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PUBLICATION_COLUMNS} FROM task_artifact_publication_receipts
         WHERE receipt_id = ?1"
    ))?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_publication_row).transpose()
}

fn load_publications(
    source: &impl SqlRead,
    plan_id: ArtifactCommitPlanId,
) -> Result<Vec<NestedArtifactPublicationReceipt>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PUBLICATION_COLUMNS} FROM task_artifact_publication_receipts
         WHERE plan_id = ?1 ORDER BY artifact_id, revision, staging_id"
    ))?;
    let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(decode_publication_row(row)?);
    }
    Ok(out)
}

fn decode_publication_row(
    row: &Row<'_>,
) -> Result<NestedArtifactPublicationReceipt, TaskStoreError> {
    Ok(NestedArtifactPublicationReceipt {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        staging_id: blob16(row, 1)?,
        artifact_id: ArtifactId::from_bytes(blob16(row, 2)?),
        revision: u64_from_blob(row, 3)?,
        digest: blob32(row, 4)?,
        size_bytes: u64_from_blob(row, 5)?,
        task_id: TaskId::from_bytes(blob16(row, 6)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 7)?),
        write_set_root: blob32(row, 8)?,
        prior_head_revision: u64_from_blob(row, 9)?,
        prior_head_digest: optional_blob32(row, 10)?,
        new_head_revision: u64_from_blob(row, 11)?,
        new_head_digest: blob32(row, 12)?,
        created_at_ms: row.get(13)?,
    })
}

fn generation_from_blob(
    row: &Row<'_>,
    index: usize,
) -> Result<nlos_types::Generation, TaskStoreError> {
    let value = u64_from_blob(row, index)?;
    std::num::NonZeroU64::new(value)
        .map(nlos_types::Generation::new)
        .ok_or(TaskStoreError::CorruptRecord("zero generation"))
}

fn u64_from_blob(row: &Row<'_>, index: usize) -> Result<u64, TaskStoreError> {
    Ok(u64::from_be_bytes(blob8(row, index)?))
}

fn blob8(row: &Row<'_>, index: usize) -> Result<[u8; 8], TaskStoreError> {
    blob_n(row, index)
}

fn blob16(row: &Row<'_>, index: usize) -> Result<[u8; 16], TaskStoreError> {
    blob_n(row, index)
}

fn blob32(row: &Row<'_>, index: usize) -> Result<[u8; 32], TaskStoreError> {
    blob_n(row, index)
}

fn optional_blob32(row: &Row<'_>, index: usize) -> Result<Option<[u8; 32]>, TaskStoreError> {
    let bytes: Option<Vec<u8>> = row.get(index)?;
    bytes
        .map(|value| {
            value
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("blob column length mismatch"))
        })
        .transpose()
}

fn blob_n<const N: usize>(row: &Row<'_>, index: usize) -> Result<[u8; N], TaskStoreError> {
    let bytes: Vec<u8> = row.get(index)?;
    bytes
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("blob column length mismatch"))
}

pub(crate) const SCHEMA_V6_SQL: &str = "CREATE TABLE task_artifact_commit_plans (
        plan_id BLOB PRIMARY KEY NOT NULL CHECK(length(plan_id) = 16),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        permit_id BLOB NOT NULL UNIQUE CHECK(length(permit_id) = 16),
        idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
        attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
        attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
        write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
        artifact_plan_root BLOB NOT NULL CHECK(length(artifact_plan_root) = 32),
        expected_artifact_count BLOB NOT NULL CHECK(length(expected_artifact_count) = 8),
        plan_state INTEGER NOT NULL CHECK(plan_state IN (0, 1, 2, 3)),
        task_receipt_id BLOB CHECK(task_receipt_id IS NULL OR length(task_receipt_id) = 16),
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        UNIQUE(task_id, idempotency_key),
        FOREIGN KEY(task_id) REFERENCES tasks(task_id),
        FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id),
        CHECK(artifact_plan_root = write_set_root),
        CHECK((plan_state = 3) = (task_receipt_id IS NOT NULL))
     ) STRICT;

     CREATE TABLE task_artifact_publication_expectations (
        plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
        ordinal BLOB NOT NULL CHECK(length(ordinal) = 8),
        staging_id BLOB NOT NULL CHECK(length(staging_id) = 16),
        artifact_id BLOB NOT NULL CHECK(length(artifact_id) = 16),
        target_revision BLOB NOT NULL CHECK(length(target_revision) = 8),
        digest BLOB NOT NULL CHECK(length(digest) = 32),
        size_bytes BLOB NOT NULL CHECK(length(size_bytes) = 8),
        PRIMARY KEY(plan_id, ordinal),
        UNIQUE(plan_id, staging_id),
        UNIQUE(plan_id, artifact_id, target_revision),
        FOREIGN KEY(plan_id) REFERENCES task_artifact_commit_plans(plan_id)
     ) STRICT;

     CREATE TRIGGER task_artifact_commit_plan_identity_immutable
     BEFORE UPDATE ON task_artifact_commit_plans
     WHEN OLD.plan_id IS NOT NEW.plan_id
       OR OLD.task_id IS NOT NEW.task_id
       OR OLD.permit_id IS NOT NEW.permit_id
       OR OLD.idempotency_key IS NOT NEW.idempotency_key
       OR OLD.attempt_id IS NOT NEW.attempt_id
       OR OLD.attempt_generation IS NOT NEW.attempt_generation
       OR OLD.write_set_root IS NOT NEW.write_set_root
       OR OLD.artifact_plan_root IS NOT NEW.artifact_plan_root
       OR OLD.expected_artifact_count IS NOT NEW.expected_artifact_count
       OR OLD.created_at_ms IS NOT NEW.created_at_ms
     BEGIN
        SELECT RAISE(ABORT, 'artifact commit plan identity is immutable');
     END;

     CREATE TRIGGER task_artifact_commit_plan_no_delete
     BEFORE DELETE ON task_artifact_commit_plans
     BEGIN
        SELECT RAISE(ABORT, 'artifact commit plan is durable evidence');
     END;

     CREATE TRIGGER task_artifact_expectation_immutable_update
     BEFORE UPDATE ON task_artifact_publication_expectations
     BEGIN
        SELECT RAISE(ABORT, 'artifact publication expectation is immutable');
     END;

     CREATE TRIGGER task_artifact_expectation_immutable_delete
     BEFORE DELETE ON task_artifact_publication_expectations
     BEGIN
        SELECT RAISE(ABORT, 'artifact publication expectation is immutable');
     END;

     PRAGMA user_version = 6;";

pub(crate) const SCHEMA_V7_SQL: &str = "CREATE TABLE task_artifact_publication_receipts (
        plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        staging_id BLOB NOT NULL CHECK(length(staging_id) = 16),
        artifact_id BLOB NOT NULL CHECK(length(artifact_id) = 16),
        revision BLOB NOT NULL CHECK(length(revision) = 8),
        digest BLOB NOT NULL CHECK(length(digest) = 32),
        size_bytes BLOB NOT NULL CHECK(length(size_bytes) = 8),
        task_id BLOB NOT NULL CHECK(length(task_id) = 16),
        permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
        write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
        prior_head_revision BLOB NOT NULL CHECK(length(prior_head_revision) = 8),
        prior_head_digest BLOB CHECK(prior_head_digest IS NULL OR length(prior_head_digest) = 32),
        new_head_revision BLOB NOT NULL CHECK(length(new_head_revision) = 8),
        new_head_digest BLOB NOT NULL CHECK(length(new_head_digest) = 32),
        created_at_ms INTEGER NOT NULL,
        UNIQUE(plan_id, staging_id),
        UNIQUE(plan_id, artifact_id, revision),
        FOREIGN KEY(plan_id) REFERENCES task_artifact_commit_plans(plan_id)
     ) STRICT;

     CREATE TRIGGER task_artifact_publication_receipt_immutable_update
     BEFORE UPDATE ON task_artifact_publication_receipts
     BEGIN
        SELECT RAISE(ABORT, 'nested artifact publication receipt is immutable');
     END;

     CREATE TRIGGER task_artifact_publication_receipt_immutable_delete
     BEFORE DELETE ON task_artifact_publication_receipts
     BEGIN
        SELECT RAISE(ABORT, 'nested artifact publication receipt is durable evidence');
     END;

     PRAGMA user_version = 7;";
