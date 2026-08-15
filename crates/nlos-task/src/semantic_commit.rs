//! Task-side consumption of `SemanticAuthority` publication receipts.
//!
//! The Semantic authority owns the canonical publication fact. This module
//! only verifies that immutable owner receipt against a sealed `TaskWriteSet`,
//! records a durable nested copy, and closes a Semantic-only permit once the
//! complete receipt set is present. It intentionally does not acknowledge the
//! Semantic outbox or derive a checkpoint locally.

use std::fmt;

use nlos_types::{
    CommitPermitId, Generation, IdempotencyKey, ReceiptId, SemanticEventId, TaskAttemptId, TaskId,
};
use rusqlite::{Row, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::effect::list_slots;
use crate::lease::{AuthorityLeaseRecord, validate_authority_lease_binding_in_transaction};
use crate::store::{
    SqlRead, SqliteTaskAuthority, blob16, blob32, close_permit, encode_u64, generation_from_blob,
    load_attempt, load_permit_by_id, load_receipt, load_task, load_write_set_by_root,
    optional_blob16, set_attempt_state, u64_from_blob, update_task,
};
use crate::{
    AttemptState, PermitState, ReceiptOutcome, RequiredSatisfaction, RequiredSatisfactionProof,
    TaskReceiptRecord, TaskStoreError, TaskWriteSetRecord, TaskWriteSetSemanticAppend,
    TaskWriteSetSemanticTarget,
};

/// Durable state of a Task-side Semantic publication plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticCommitPlanState {
    Planned,
    Publishing,
    Ready,
    Finalized,
}

impl SemanticCommitPlanState {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Planned => 0,
            Self::Publishing => 1,
            Self::Ready => 2,
            Self::Finalized => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Planned),
            1 => Ok(Self::Publishing),
            2 => Ok(Self::Ready),
            3 => Ok(Self::Finalized),
            _ => Err(TaskStoreError::CorruptRecord(
                "unknown semantic commit plan state",
            )),
        }
    }
}

/// Authority-derived identity of one Semantic commit plan.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SemanticCommitPlanId([u8; 16]);

impl SemanticCommitPlanId {
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

impl fmt::Debug for SemanticCommitPlanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SemanticCommitPlanId(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// Request to durably bind a Semantic publication plan to an issued permit.
/// The expected event set is read from the immutable owner-verified
/// `TaskWriteSet`; caller-supplied publication lists are not accepted here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanSemanticCommitRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub permit_id: CommitPermitId,
    pub idempotency_key: IdempotencyKey,
    pub planned_at_ms: i64,
}

/// Durable Semantic publication plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticCommitPlanRecord {
    pub plan_id: SemanticCommitPlanId,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub write_set_root: [u8; 32],
    pub semantic_append_set_root: [u8; 32],
    pub expected_semantic_count: u64,
    pub state: SemanticCommitPlanState,
    pub task_receipt_id: Option<ReceiptId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Task-side immutable copy of one `SemanticAuthority` publication receipt.
/// `target` is retained in the typed `TaskWriteSet` form; the owner receipt's
/// target kind/id is compared bit-for-bit before this row is inserted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NestedSemanticPublicationReceipt {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: [u8; 32],
    pub event_id: SemanticEventId,
    pub target: TaskWriteSetSemanticTarget,
    pub log_seq: u64,
    pub admission_receipt_id: ReceiptId,
    pub durability_receipt_id: Option<ReceiptId>,
    pub semantic_checkpoint_after: [u8; 32],
    pub created_at_ms: u64,
}

/// Request to consume one or more owner-issued Semantic publication receipts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordSemanticPublicationsRequest {
    pub plan_id: SemanticCommitPlanId,
    pub receipts: Vec<NestedSemanticPublicationReceipt>,
    pub observed_at_ms: i64,
}

/// Durable plan progress and all nested Semantic receipts consumed so far.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticCommitProgress {
    pub plan: SemanticCommitPlanRecord,
    pub publications: Vec<NestedSemanticPublicationReceipt>,
}

/// Final Task receipt together with the canonical Semantic publication facts
/// embedded as nested commit evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticTaskCommitReceipt {
    pub task_receipt: TaskReceiptRecord,
    pub semantic_publications: Vec<NestedSemanticPublicationReceipt>,
}

/// Request to atomically finalize one complete Semantic-only plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalizeSemanticCommitRequest {
    pub plan_id: SemanticCommitPlanId,
    pub finalized_at_ms: i64,
}

/// Typed effect-proof envelope persisted before a mixed Effect + Semantic
/// coordinator starts publication. Persisting the proofs makes a later
/// restart able to reconstruct the v3 finalize request without caller memory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareSemanticFinalizeRequest {
    pub plan_id: SemanticCommitPlanId,
    pub required_satisfaction: Vec<RequiredSatisfaction>,
    pub fenced_participant_digest: [u8; 32],
    pub prepared_at_ms: i64,
}

/// Immutable durable mixed-finalize envelope bound to one Semantic plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticFinalizeEnvelopeRecord {
    pub plan_id: SemanticCommitPlanId,
    pub required_satisfaction: Vec<RequiredSatisfaction>,
    pub fenced_participant_digest: [u8; 32],
    pub prepared_at_ms: i64,
}

/// Idempotent result of preparing the mixed-finalize envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticFinalizeEnvelopeDecision {
    Prepared(Box<SemanticFinalizeEnvelopeRecord>),
    Replayed(Box<SemanticFinalizeEnvelopeRecord>),
}

impl SemanticFinalizeEnvelopeDecision {
    #[must_use]
    pub fn record(&self) -> &SemanticFinalizeEnvelopeRecord {
        match self {
            Self::Prepared(record) | Self::Replayed(record) => record,
        }
    }
}

/// Idempotent Semantic-aware Task finalize decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticFinalizeDecision {
    Committed(Box<SemanticTaskCommitReceipt>),
    Replayed(Box<SemanticTaskCommitReceipt>),
}

impl SemanticFinalizeDecision {
    #[must_use]
    pub fn receipt(&self) -> &SemanticTaskCommitReceipt {
        match self {
            Self::Committed(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Idempotent Semantic plan decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticCommitPlanDecision {
    Planned(Box<SemanticCommitPlanRecord>),
    Replayed(Box<SemanticCommitPlanRecord>),
}

impl SemanticCommitPlanDecision {
    #[must_use]
    pub fn record(&self) -> &SemanticCommitPlanRecord {
        match self {
            Self::Planned(record) | Self::Replayed(record) => record,
        }
    }
}

/// Idempotent decision that authorizes the first Semantic publication
/// consumption attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticPublicationAuthorizationDecision {
    Authorized(Box<SemanticCommitPlanRecord>),
    Replayed(Box<SemanticCommitPlanRecord>),
}

impl SemanticPublicationAuthorizationDecision {
    #[must_use]
    pub fn record(&self) -> &SemanticCommitPlanRecord {
        match self {
            Self::Authorized(record) | Self::Replayed(record) => record,
        }
    }
}

impl SqliteTaskAuthority {
    /// Creates an immutable Semantic publication plan from the sealed
    /// `TaskWriteSet` bound to an issued permit. Mixed Effect + Semantic
    /// plans are consumed by the unified v3 finalize hook; the dedicated
    /// Semantic-only finalize entry point remains intentionally narrower.
    ///
    /// # Errors
    ///
    /// Returns a typed plan/permit/head/write-set/membership error or a
    /// storage error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn plan_semantic_commit(
        &self,
        request: PlanSemanticCommitRequest,
    ) -> Result<SemanticCommitPlanDecision, TaskStoreError> {
        if request.planned_at_ms < 0 {
            return Err(TaskStoreError::InvalidSemanticPublicationPlan {
                reason: "planned_at_ms must be non-negative",
            });
        }
        let plan_id = derive_plan_id(request.permit_id);
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) =
            load_plan_by_key(&transaction, request.task_id, request.idempotency_key)?
        {
            if same_plan_request(&existing, &request) {
                transaction.commit()?;
                return Ok(SemanticCommitPlanDecision::Replayed(Box::new(existing)));
            }
            return Err(TaskStoreError::IdempotencyConflict);
        }
        if load_plan_optional(&transaction, plan_id)?.is_some() {
            return Err(TaskStoreError::IdempotencyConflict);
        }
        let (task, permit, _attempt, write_set) = load_plan_context(
            &transaction,
            request.task_id,
            request.attempt_id,
            request.attempt_generation,
            request.permit_id,
        )?;
        validate_semantic_only_context(&transaction, &task, &permit, &write_set)?;
        let expected_semantic_count =
            u64::try_from(write_set.semantic_appends.len()).map_err(|_| {
                TaskStoreError::InvalidSemanticPublicationPlan {
                    reason: "Semantic append count exceeds u64",
                }
            })?;
        let record = SemanticCommitPlanRecord {
            plan_id,
            task_id: request.task_id,
            permit_id: request.permit_id,
            attempt_id: request.attempt_id,
            attempt_generation: request.attempt_generation,
            write_set_root: permit.write_set_root,
            semantic_append_set_root: write_set.semantic_append_set_root,
            expected_semantic_count,
            state: SemanticCommitPlanState::Planned,
            task_receipt_id: None,
            created_at_ms: request.planned_at_ms,
            updated_at_ms: request.planned_at_ms,
        };
        insert_plan(&transaction, &record, request.idempotency_key)?;
        transaction.commit()?;
        Ok(SemanticCommitPlanDecision::Planned(Box::new(record)))
    }

    /// Reads one durable Semantic publication plan.
    ///
    /// # Errors
    ///
    /// Returns `SemanticCommitPlanNotFound`, a corrupt-record error, or a
    /// storage error.
    pub fn inspect_semantic_commit_plan(
        &self,
        plan_id: SemanticCommitPlanId,
    ) -> Result<SemanticCommitPlanRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_plan_optional(&*connection, plan_id)?.ok_or(TaskStoreError::SemanticCommitPlanNotFound)
    }

    /// Revalidates the live Task fences and authorizes Semantic receipt
    /// consumption by moving `Planned → Publishing`.
    ///
    /// # Errors
    ///
    /// Returns a typed plan/permit/head/write-set/membership error or a
    /// storage error.
    pub fn authorize_semantic_publication(
        &self,
        plan_id: SemanticCommitPlanId,
        authorized_at_ms: i64,
    ) -> Result<SemanticPublicationAuthorizationDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut plan = load_plan_optional(&transaction, plan_id)?
            .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
        if plan.state != SemanticCommitPlanState::Planned {
            transaction.commit()?;
            return Ok(SemanticPublicationAuthorizationDecision::Replayed(
                Box::new(plan),
            ));
        }
        let (task, permit, _attempt, write_set) = load_plan_context(
            &transaction,
            plan.task_id,
            plan.attempt_id,
            plan.attempt_generation,
            plan.permit_id,
        )?;
        validate_semantic_only_context(&transaction, &task, &permit, &write_set)?;
        validate_plan_against_write_set(&plan, &write_set)?;
        update_plan_state(
            &transaction,
            plan.plan_id,
            SemanticCommitPlanState::Planned,
            SemanticCommitPlanState::Publishing,
            authorized_at_ms,
        )?;
        plan.state = SemanticCommitPlanState::Publishing;
        plan.updated_at_ms = authorized_at_ms;
        transaction.commit()?;
        Ok(SemanticPublicationAuthorizationDecision::Authorized(
            Box::new(plan),
        ))
    }

    /// Consumes owner-issued Semantic publication receipts idempotently. A
    /// partial set remains `Publishing`; the complete exact set becomes
    /// `Ready`. This method never acknowledges the Semantic outbox and never
    /// closes the Task permit.
    ///
    /// # Errors
    ///
    /// Returns a typed owner, plan, receipt-binding, or storage error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn record_semantic_publications(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        request: RecordSemanticPublicationsRequest,
    ) -> Result<SemanticCommitProgress, TaskStoreError> {
        if request.receipts.is_empty() {
            return Err(TaskStoreError::SemanticPublicationConflict {
                reason: "receipt batch must not be empty",
            });
        }
        let mut owner_receipts = Vec::with_capacity(request.receipts.len());
        for receipt in &request.receipts {
            let owner = semantic_authority
                .inspect_publication_receipt(receipt.receipt_id)
                .map_err(TaskStoreError::SemanticParticipantAuthority)?;
            validate_owner_copy(receipt, &owner)?;
            owner_receipts.push(owner);
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut plan = load_plan_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
        match plan.state {
            SemanticCommitPlanState::Planned => {
                return Err(TaskStoreError::SemanticPublicationConflict {
                    reason: "Semantic publication was not authorized by TaskAuthority",
                });
            }
            SemanticCommitPlanState::Publishing | SemanticCommitPlanState::Ready => {}
            SemanticCommitPlanState::Finalized => {
                return Err(TaskStoreError::SemanticPublicationConflict {
                    reason: "finalized plan cannot consume publication receipts",
                });
            }
        }
        let (_task, _permit, _attempt, write_set) = load_plan_context(
            &transaction,
            plan.task_id,
            plan.attempt_id,
            plan.attempt_generation,
            plan.permit_id,
        )?;
        validate_plan_against_write_set(&plan, &write_set)?;
        let mut inserted_any = false;
        for (receipt, owner) in request.receipts.iter().zip(owner_receipts.iter()) {
            validate_publication_receipt(&plan, &write_set, receipt, owner)?;
            if let Some(existing) =
                load_publication_by_event(&transaction, plan.plan_id, receipt.event_id)?
            {
                if existing != *receipt {
                    return Err(TaskStoreError::SemanticPublicationConflict {
                        reason: "event expectation already consumed by a different receipt",
                    });
                }
                continue;
            }
            if load_publication_by_receipt_id(&transaction, receipt.receipt_id)?.is_some() {
                return Err(TaskStoreError::SemanticPublicationConflict {
                    reason: "receipt identity already belongs to another event",
                });
            }
            insert_publication(&transaction, plan.plan_id, receipt)?;
            inserted_any = true;
        }
        let publications = load_publications(&transaction, plan.plan_id)?;
        let expected_count = usize::try_from(plan.expected_semantic_count).map_err(|_| {
            TaskStoreError::CorruptRecord("Semantic publication count exceeds usize")
        })?;
        let state = if publications.len() == expected_count {
            SemanticCommitPlanState::Ready
        } else {
            SemanticCommitPlanState::Publishing
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
        Ok(SemanticCommitProgress { plan, publications })
    }

    /// Reads a Semantic plan together with all nested owner receipts.
    ///
    /// # Errors
    ///
    /// Returns `SemanticCommitPlanNotFound`, a corrupt-record error, or a
    /// storage error.
    pub fn inspect_semantic_commit_progress(
        &self,
        plan_id: SemanticCommitPlanId,
    ) -> Result<SemanticCommitProgress, TaskStoreError> {
        let connection = self.lock_connection()?;
        let plan = load_plan_optional(&*connection, plan_id)?
            .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
        let publications = load_publications(&*connection, plan_id)?;
        validate_progress(&plan, &publications)?;
        Ok(SemanticCommitProgress { plan, publications })
    }

    /// Reads the sealed Semantic publication declarations for a coordinator
    /// step. The declarations come from the `TaskWriteSet`; callers cannot
    /// inject a new event/target/receipt binding while recovering a plan.
    ///
    /// # Errors
    ///
    /// Returns a typed plan, write-set, binding, or storage error.
    pub fn inspect_semantic_commit_expectations(
        &self,
        plan_id: SemanticCommitPlanId,
    ) -> Result<Vec<TaskWriteSetSemanticAppend>, TaskStoreError> {
        let connection = self.lock_connection()?;
        let plan = load_plan_optional(&*connection, plan_id)?
            .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
        let write_set = load_write_set_by_root(&*connection, plan.task_id, plan.write_set_root)?
            .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
        validate_plan_against_write_set(&plan, &write_set)?;
        Ok(write_set.semantic_appends)
    }

    /// Lists non-finalized Semantic publication plans in stable creation/
    /// identity order for a restart coordinator scan.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-record or storage error.
    pub fn list_incomplete_semantic_commit_plans(
        &self,
        limit: usize,
    ) -> Result<Vec<SemanticCommitPlanRecord>, TaskStoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT plan_id FROM task_semantic_commit_plans
             WHERE plan_state != ?1 ORDER BY created_at_ms, plan_id LIMIT ?2",
        )?;
        let mut rows = statement.query(params![
            SemanticCommitPlanState::Finalized.code(),
            i64::try_from(limit).unwrap_or(i64::MAX),
        ])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(SemanticCommitPlanId::from_bytes(blob16(row, 0)?));
        }
        drop(rows);
        drop(statement);
        ids.into_iter()
            .map(|plan_id| {
                load_plan_optional(&*connection, plan_id)?
                    .ok_or(TaskStoreError::SemanticCommitPlanNotFound)
            })
            .collect()
    }

    /// Persists the typed required-effect proofs needed by the mixed v3
    /// finalize path. The envelope is immutable and may be prepared before
    /// Semantic publication authorization; exact retries replay its bytes.
    ///
    /// # Errors
    ///
    /// Returns a typed binding, slot, idempotency, or storage error.
    pub fn prepare_semantic_finalize(
        &self,
        request: PrepareSemanticFinalizeRequest,
    ) -> Result<SemanticFinalizeEnvelopeDecision, TaskStoreError> {
        if request.prepared_at_ms < 0 {
            return Err(TaskStoreError::InvalidSemanticPublicationPlan {
                reason: "mixed finalize envelope timestamp must be non-negative",
            });
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let plan = load_plan_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
        if let Some(existing) = load_finalize_envelope_optional(&transaction, request.plan_id)? {
            if existing.required_satisfaction == request.required_satisfaction
                && existing.fenced_participant_digest == request.fenced_participant_digest
                && existing.prepared_at_ms == request.prepared_at_ms
            {
                transaction.commit()?;
                return Ok(SemanticFinalizeEnvelopeDecision::Replayed(Box::new(
                    existing,
                )));
            }
            return Err(TaskStoreError::InvalidSemanticPublicationPlan {
                reason: "mixed finalize envelope request conflicts with durable bytes",
            });
        }
        if plan.state == SemanticCommitPlanState::Finalized {
            return Err(TaskStoreError::InvalidSemanticPublicationPlan {
                reason: "finalized Semantic plan lacks its mixed finalize envelope",
            });
        }
        let (task, permit, _attempt, write_set) = load_plan_context(
            &transaction,
            plan.task_id,
            plan.attempt_id,
            plan.attempt_generation,
            plan.permit_id,
        )?;
        validate_semantic_only_context(&transaction, &task, &permit, &write_set)?;
        validate_plan_against_write_set(&plan, &write_set)?;
        let slots = list_slots(&transaction, permit.permit_id)?;
        if slots.is_empty() {
            return Err(TaskStoreError::InvalidSemanticPublicationPlan {
                reason: "mixed finalize envelope requires declared Effect slots",
            });
        }
        validate_finalize_satisfaction_shape(&slots, &request.required_satisfaction)?;
        let envelope = SemanticFinalizeEnvelopeRecord {
            plan_id: request.plan_id,
            required_satisfaction: request.required_satisfaction,
            fenced_participant_digest: request.fenced_participant_digest,
            prepared_at_ms: request.prepared_at_ms,
        };
        insert_finalize_envelope(&transaction, &envelope)?;
        transaction.commit()?;
        Ok(SemanticFinalizeEnvelopeDecision::Prepared(Box::new(
            envelope,
        )))
    }

    /// Reads the immutable mixed-finalize envelope, if one was prepared for
    /// the plan. The absence of an envelope is the Semantic-only path.
    ///
    /// # Errors
    ///
    /// Returns a storage or corrupt-record error.
    pub fn inspect_semantic_finalize_envelope(
        &self,
        plan_id: SemanticCommitPlanId,
    ) -> Result<Option<SemanticFinalizeEnvelopeRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_finalize_envelope_optional(&*connection, plan_id)
    }

    /// Finalizes a complete Semantic-only plan in one `TaskAuthority`
    /// transaction. The returned wrapper nests the exact owner receipts;
    /// the base `TaskReceiptRecord` remains the existing immutable receipt
    /// shape for compatibility with legacy inspection APIs.
    ///
    /// # Errors
    ///
    /// Returns a typed readiness/holder/head/membership/write-set error or a
    /// storage error. No subset of terminal Task facts is committed on
    /// failure.
    pub fn finalize_semantic_commit(
        &self,
        request: FinalizeSemanticCommitRequest,
    ) -> Result<SemanticFinalizeDecision, TaskStoreError> {
        self.finalize_semantic_commit_inner(request, None)
    }

    /// Finalizes a complete Semantic-only plan while proving the current
    /// durable authority lease bound into its `CommitPermit`.
    ///
    /// Finalized plans remain idempotently readable without presenting the
    /// lease again; a fresh terminal mutation requires the exact holder,
    /// term, epoch, fencing token, and expiry binding that was issued with
    /// the permit.
    ///
    /// # Errors
    ///
    /// Returns a typed lease, readiness/holder/head/membership/write-set
    /// error or a storage error. No subset of terminal Task facts is
    /// committed on failure.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_semantic_commit_with_authority_lease(
        &self,
        request: FinalizeSemanticCommitRequest,
        authority_lease: AuthorityLeaseRecord,
    ) -> Result<SemanticFinalizeDecision, TaskStoreError> {
        self.finalize_semantic_commit_inner(request, Some(authority_lease))
    }

    fn finalize_semantic_commit_inner(
        &self,
        request: FinalizeSemanticCommitRequest,
        authority_lease: Option<AuthorityLeaseRecord>,
    ) -> Result<SemanticFinalizeDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let plan = load_plan_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
        let publications = load_publications(&transaction, plan.plan_id)?;
        validate_progress(&plan, &publications)?;
        if plan.state == SemanticCommitPlanState::Finalized {
            let receipt_id = plan.task_receipt_id.ok_or(TaskStoreError::CorruptRecord(
                "finalized Semantic plan lacks Task receipt",
            ))?;
            let task_receipt = load_receipt(&transaction, plan.task_id, receipt_id)?;
            transaction.commit()?;
            return Ok(SemanticFinalizeDecision::Replayed(Box::new(
                SemanticTaskCommitReceipt {
                    task_receipt,
                    semantic_publications: publications,
                },
            )));
        }
        if plan.state != SemanticCommitPlanState::Ready {
            return Err(TaskStoreError::SemanticCommitPlanNotReady { state: plan.state });
        }
        let (task, permit, attempt, write_set) = load_plan_context(
            &transaction,
            plan.task_id,
            plan.attempt_id,
            plan.attempt_generation,
            plan.permit_id,
        )?;
        validate_semantic_only_lease(
            &transaction,
            &permit,
            request.finalized_at_ms,
            authority_lease,
        )?;
        validate_semantic_only_context(&transaction, &task, &permit, &write_set)?;
        validate_plan_against_write_set(&plan, &write_set)?;
        ensure_no_effect_slots(&transaction, permit.permit_id)?;
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
            participant_registry_binding: permit.participant_registry_binding,
            outcome: ReceiptOutcome::Committed,
            prior_head_commit_seq: task.record.head_commit_seq,
            prior_effect_history_root: task.record.head_effect_history_root,
            prior_retry_fence_epoch: task.record.retry_fence_epoch,
            new_head_commit_seq: new_seq,
            new_effect_history_root: task.record.head_effect_history_root,
            new_retry_fence_epoch: task.record.retry_fence_epoch,
            created_at_ms: request.finalized_at_ms,
        };
        crate::store::insert_receipt(&transaction, &receipt)?;
        close_permit(&transaction, &permit, request.finalized_at_ms)?;
        set_attempt_state(
            &transaction,
            &attempt,
            AttemptState::Committed,
            Some(receipt_id),
            request.finalized_at_ms,
        )?;
        update_task(&transaction, &task, request.finalized_at_ms, |record| {
            record.head_commit_seq = new_seq;
            record.control_epoch = control_epoch;
        })?;
        finalize_plan(
            &transaction,
            plan.plan_id,
            receipt_id,
            request.finalized_at_ms,
        )?;
        transaction.commit()?;
        Ok(SemanticFinalizeDecision::Committed(Box::new(
            SemanticTaskCommitReceipt {
                task_receipt: receipt,
                semantic_publications: publications,
            },
        )))
    }
}

fn load_plan_context(
    source: &impl SqlRead,
    task_id: TaskId,
    attempt_id: TaskAttemptId,
    attempt_generation: Generation,
    permit_id: CommitPermitId,
) -> Result<
    (
        crate::store::StoredTask,
        crate::PermitRecord,
        crate::AttemptRecord,
        TaskWriteSetRecord,
    ),
    TaskStoreError,
> {
    let task = load_task(source, task_id)?;
    let permit = load_permit_by_id(source, task_id, permit_id)?;
    let attempt = load_attempt(source, task_id, attempt_id)?;
    if attempt.attempt_generation != attempt_generation {
        return Err(TaskStoreError::InvalidGeneration);
    }
    if permit.attempt_id != attempt_id || permit.attempt_generation != attempt_generation {
        return Err(TaskStoreError::NotPermitHolder);
    }
    let write_set = load_write_set_by_root(source, task_id, permit.write_set_root)?
        .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
    Ok((task, permit, attempt, write_set))
}

fn validate_semantic_only_lease(
    transaction: &Transaction<'_>,
    permit: &crate::PermitRecord,
    now_ms: i64,
    authority_lease: Option<AuthorityLeaseRecord>,
) -> Result<(), TaskStoreError> {
    match (permit.authority_lease_binding, authority_lease) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(TaskStoreError::AuthorityLeaseBindingMismatch),
        (Some(_), None) => Err(TaskStoreError::AuthorityLeaseRequired),
        (Some(binding), Some(lease)) => {
            if binding != lease.binding() {
                return Err(TaskStoreError::AuthorityLeaseBindingMismatch);
            }
            validate_authority_lease_binding_in_transaction(transaction, binding, now_ms)
        }
    }
}

fn validate_semantic_only_context(
    transaction: &Transaction<'_>,
    task: &crate::store::StoredTask,
    permit: &crate::PermitRecord,
    write_set: &TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    if permit.state != PermitState::Issued {
        return Err(TaskStoreError::PermitNotIssued);
    }
    if permit.write_set_root != write_set.write_set_root
        || write_set.write_set_root != crate::model::task_write_set_root(write_set)
    {
        return Err(TaskStoreError::InvalidSemanticPublicationPlan {
            reason: "permit and sealed TaskWriteSet roots disagree",
        });
    }
    if task.record.head_commit_seq != permit.expected_head_commit_seq
        || task.record.head_effect_history_root != permit.expected_effect_history_root
        || task.record.retry_fence_epoch != permit.expected_retry_fence_epoch
    {
        return Err(TaskStoreError::StaleTaskHead);
    }
    if write_set.semantic_appends.is_empty() || write_set.semantic_append_set_root == [0; 32] {
        return Err(TaskStoreError::InvalidSemanticPublicationPlan {
            reason: "sealed TaskWriteSet has no Semantic append declarations",
        });
    }
    crate::group::validate_commit_binding(transaction, permit.attempt_id, permit.group_binding)?;
    crate::participant::validate_frozen_binding(
        transaction,
        &task.record,
        permit.participant_registry_binding,
    )?;
    Ok(())
}

fn ensure_no_effect_slots(
    transaction: &Transaction<'_>,
    permit_id: CommitPermitId,
) -> Result<(), TaskStoreError> {
    let effect_count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM effect_slots WHERE permit_id = ?1",
        [permit_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    if effect_count != 0 {
        return Err(TaskStoreError::InvalidSemanticPublicationPlan {
            reason: "Semantic-only finalize cannot carry Effect slots",
        });
    }
    Ok(())
}

fn validate_finalize_satisfaction_shape(
    slots: &[crate::SlotRecord],
    satisfactions: &[RequiredSatisfaction],
) -> Result<(), TaskStoreError> {
    let mut seen = std::collections::BTreeSet::new();
    for satisfaction in satisfactions {
        if !seen.insert(satisfaction.effect_seq) {
            return Err(TaskStoreError::InvalidSemanticPublicationPlan {
                reason: "mixed finalize envelope repeats an effect satisfaction",
            });
        }
        let slot = slots
            .iter()
            .find(|slot| slot.effect_seq == satisfaction.effect_seq)
            .ok_or(TaskStoreError::EffectSlotNotFound)?;
        if !slot.required {
            return Err(TaskStoreError::InvalidSemanticPublicationPlan {
                reason: "mixed finalize envelope covers a non-required Effect slot",
            });
        }
        if matches!(
            satisfaction.proof,
            RequiredSatisfactionProof::ConditionNotApplicable { .. }
        ) && slot.required_condition_digest.is_none()
        {
            return Err(TaskStoreError::ConditionNotBound);
        }
    }
    Ok(())
}

pub(crate) fn validate_plan_against_write_set(
    plan: &SemanticCommitPlanRecord,
    write_set: &TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    let expected_count = u64::try_from(write_set.semantic_appends.len())
        .map_err(|_| TaskStoreError::CorruptRecord("Semantic append count exceeds u64"))?;
    if plan.write_set_root != write_set.write_set_root
        || plan.semantic_append_set_root != write_set.semantic_append_set_root
        || plan.expected_semantic_count != expected_count
    {
        return Err(TaskStoreError::CorruptRecord(
            "Semantic commit plan disagrees with sealed TaskWriteSet",
        ));
    }
    Ok(())
}

fn validate_owner_copy(
    nested: &NestedSemanticPublicationReceipt,
    owner: &nlos_semantic::SemanticPublicationReceipt,
) -> Result<(), TaskStoreError> {
    if nested.task_id != owner.task_id
        || nested.permit_id != owner.permit_id
        || nested.write_set_root != owner.write_set_root
        || nested.event_id != owner.event_id
        || nested.log_seq != owner.log_seq
        || nested.admission_receipt_id != owner.admission_receipt_id
        || nested.durability_receipt_id != owner.durability_receipt_id
        || nested.semantic_checkpoint_after != owner.semantic_checkpoint_after
        || nested.created_at_ms != owner.created_at_ms
        || !target_matches(nested.target, owner)
    {
        return Err(TaskStoreError::SemanticPublicationConflict {
            reason: "nested receipt differs from SemanticAuthority owner readback",
        });
    }
    Ok(())
}

fn target_matches(
    target: TaskWriteSetSemanticTarget,
    owner: &nlos_semantic::SemanticPublicationReceipt,
) -> bool {
    match (target, owner.target) {
        (
            TaskWriteSetSemanticTarget::Namespace(expected),
            nlos_capability::CapabilityTarget::Namespace(actual),
        ) => expected == actual,
        (
            TaskWriteSetSemanticTarget::Task(expected),
            nlos_capability::CapabilityTarget::Task(actual),
        ) => expected == actual,
        _ => false,
    }
}

fn validate_publication_receipt(
    plan: &SemanticCommitPlanRecord,
    write_set: &TaskWriteSetRecord,
    nested: &NestedSemanticPublicationReceipt,
    owner: &nlos_semantic::SemanticPublicationReceipt,
) -> Result<(), TaskStoreError> {
    validate_owner_copy(nested, owner)?;
    if nested.task_id != plan.task_id
        || nested.permit_id != plan.permit_id
        || nested.write_set_root != plan.write_set_root
    {
        return Err(TaskStoreError::SemanticPublicationConflict {
            reason: "Task/Permit/write-set binding differs from Semantic plan",
        });
    }
    let append = write_set
        .semantic_appends
        .iter()
        .find(|append| append.event_id == nested.event_id)
        .ok_or(TaskStoreError::SemanticPublicationConflict {
            reason: "publication event is absent from sealed Semantic append set",
        })?;
    if append.target != nested.target
        || append.admission_receipt_id != nested.admission_receipt_id
        || append.durability_receipt_id != nested.durability_receipt_id
    {
        return Err(TaskStoreError::SemanticPublicationConflict {
            reason: "publication receipt differs from sealed Semantic append",
        });
    }
    if nested.log_seq == 0 {
        return Err(TaskStoreError::SemanticPublicationConflict {
            reason: "publication log sequence must be non-zero",
        });
    }
    Ok(())
}

pub(crate) fn validate_progress(
    plan: &SemanticCommitPlanRecord,
    publications: &[NestedSemanticPublicationReceipt],
) -> Result<(), TaskStoreError> {
    let expected_count = usize::try_from(plan.expected_semantic_count)
        .map_err(|_| TaskStoreError::CorruptRecord("Semantic publication count exceeds usize"))?;
    if publications.len() > expected_count {
        return Err(TaskStoreError::CorruptRecord(
            "Semantic publication count exceeds plan",
        ));
    }
    let mut events = std::collections::BTreeSet::new();
    for publication in publications {
        if !events.insert(publication.event_id) {
            return Err(TaskStoreError::CorruptRecord(
                "duplicate stored Semantic publication event",
            ));
        }
    }
    let expected_state = if publications.len() == expected_count {
        SemanticCommitPlanState::Ready
    } else if publications.is_empty() && plan.state == SemanticCommitPlanState::Planned {
        SemanticCommitPlanState::Planned
    } else {
        SemanticCommitPlanState::Publishing
    };
    let state_matches = match plan.state {
        SemanticCommitPlanState::Finalized => expected_state == SemanticCommitPlanState::Ready,
        state => state == expected_state,
    };
    if !state_matches {
        return Err(TaskStoreError::CorruptRecord(
            "semantic commit plan state disagrees with publication count",
        ));
    }
    Ok(())
}

/// Loads a READY Semantic plan for the unified Effect + Semantic finalize
/// hook. The caller owns the surrounding Task transaction and supplies the
/// sealed write set already bound to the permit.
pub(crate) fn load_ready_semantic_plan(
    source: &impl SqlRead,
    plan_id: SemanticCommitPlanId,
    task_id: TaskId,
    permit_id: CommitPermitId,
    write_set: &TaskWriteSetRecord,
) -> Result<
    (
        SemanticCommitPlanRecord,
        Vec<NestedSemanticPublicationReceipt>,
    ),
    TaskStoreError,
> {
    let plan =
        load_plan_optional(source, plan_id)?.ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
    if plan.state != SemanticCommitPlanState::Ready {
        return Err(TaskStoreError::SemanticCommitPlanNotReady { state: plan.state });
    }
    if plan.task_id != task_id || plan.permit_id != permit_id {
        return Err(TaskStoreError::InvalidSemanticPublicationPlan {
            reason: "Semantic plan belongs to a different Task/permit",
        });
    }
    validate_plan_against_write_set(&plan, write_set)?;
    let publications = load_publications(source, plan.plan_id)?;
    validate_progress(&plan, &publications)?;
    Ok((plan, publications))
}

/// Loads a FINALIZED Semantic plan during unified terminal replay and returns
/// its immutable nested publication set.
pub(crate) fn load_finalized_semantic_publications(
    source: &impl SqlRead,
    plan_id: SemanticCommitPlanId,
    task_id: TaskId,
    receipt_id: ReceiptId,
) -> Result<Vec<NestedSemanticPublicationReceipt>, TaskStoreError> {
    let plan =
        load_plan_optional(source, plan_id)?.ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
    if plan.state != SemanticCommitPlanState::Finalized || plan.task_id != task_id {
        return Err(TaskStoreError::InvalidSemanticPublicationPlan {
            reason: "Semantic plan is not finalized for this Task",
        });
    }
    if plan.task_receipt_id != Some(receipt_id) {
        return Err(TaskStoreError::HistoryConflict);
    }
    let publications = load_publications(source, plan.plan_id)?;
    validate_progress(&plan, &publications)?;
    Ok(publications)
}

fn same_plan_request(
    existing: &SemanticCommitPlanRecord,
    request: &PlanSemanticCommitRequest,
) -> bool {
    existing.plan_id == derive_plan_id(request.permit_id)
        && existing.task_id == request.task_id
        && existing.permit_id == request.permit_id
        && existing.attempt_id == request.attempt_id
        && existing.attempt_generation == request.attempt_generation
}

fn derive_plan_id(permit_id: CommitPermitId) -> SemanticCommitPlanId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-semantic-commit-plan/v1");
    hasher.update(permit_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    SemanticCommitPlanId::from_bytes(bytes)
}

fn insert_plan(
    transaction: &Transaction<'_>,
    record: &SemanticCommitPlanRecord,
    idempotency_key: IdempotencyKey,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_semantic_commit_plans (
            plan_id, task_id, permit_id, idempotency_key, attempt_id,
            attempt_generation, write_set_root, semantic_append_set_root,
            expected_semantic_count, plan_state, task_receipt_id,
            created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?11)",
        params![
            record.plan_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record.permit_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            record.attempt_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            record.write_set_root.as_slice(),
            record.semantic_append_set_root.as_slice(),
            encode_u64(record.expected_semantic_count).as_slice(),
            record.state.code(),
            record.created_at_ms,
        ],
    )?;
    Ok(())
}

fn update_plan_state(
    transaction: &Transaction<'_>,
    plan_id: SemanticCommitPlanId,
    old: SemanticCommitPlanState,
    new: SemanticCommitPlanState,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE task_semantic_commit_plans
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
            "semantic commit plan state compare-and-swap failed",
        ));
    }
    Ok(())
}

pub(crate) fn finalize_plan(
    transaction: &Transaction<'_>,
    plan_id: SemanticCommitPlanId,
    receipt_id: ReceiptId,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE task_semantic_commit_plans
         SET plan_state = ?1, task_receipt_id = ?2, updated_at_ms = ?3
         WHERE plan_id = ?4 AND plan_state = ?5 AND task_receipt_id IS NULL",
        params![
            SemanticCommitPlanState::Finalized.code(),
            receipt_id.as_bytes().as_slice(),
            now_ms,
            plan_id.as_bytes().as_slice(),
            SemanticCommitPlanState::Ready.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "Semantic commit plan finalize compare-and-swap failed",
        ));
    }
    Ok(())
}

fn insert_finalize_envelope(
    transaction: &Transaction<'_>,
    envelope: &SemanticFinalizeEnvelopeRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_semantic_finalize_envelopes (
            plan_id, fenced_participant_digest, prepared_at_ms
         ) VALUES (?1, ?2, ?3)",
        params![
            envelope.plan_id.as_bytes().as_slice(),
            envelope.fenced_participant_digest.as_slice(),
            envelope.prepared_at_ms,
        ],
    )?;
    for satisfaction in &envelope.required_satisfaction {
        let (proof_kind, proof_digest) = match satisfaction.proof {
            RequiredSatisfactionProof::EffectClosedSuccess {
                success_assertion_digest,
            } => (0_i64, success_assertion_digest),
            RequiredSatisfactionProof::ConditionNotApplicable {
                condition_false_proof_digest,
            } => (1_i64, condition_false_proof_digest),
        };
        transaction.execute(
            "INSERT INTO task_semantic_finalize_satisfactions (
                plan_id, effect_seq, proof_kind, proof_digest
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                envelope.plan_id.as_bytes().as_slice(),
                encode_u64(satisfaction.effect_seq).as_slice(),
                proof_kind,
                proof_digest.as_slice(),
            ],
        )?;
    }
    Ok(())
}

fn load_finalize_envelope_optional(
    source: &impl SqlRead,
    plan_id: SemanticCommitPlanId,
) -> Result<Option<SemanticFinalizeEnvelopeRecord>, TaskStoreError> {
    let (fenced_participant_digest, prepared_at_ms) = {
        let mut statement = source.prepare_statement(
            "SELECT fenced_participant_digest, prepared_at_ms
             FROM task_semantic_finalize_envelopes WHERE plan_id = ?1",
        )?;
        let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        (blob32(row, 0)?, row.get::<_, i64>(1)?)
    };
    let mut statement = source.prepare_statement(
        "SELECT effect_seq, proof_kind, proof_digest
         FROM task_semantic_finalize_satisfactions
         WHERE plan_id = ?1 ORDER BY effect_seq",
    )?;
    let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
    let mut required_satisfaction = Vec::new();
    while let Some(row) = rows.next()? {
        let effect_seq = u64_from_blob(row, 0)?;
        let proof_digest = blob32(row, 2)?;
        let proof = match row.get::<_, i64>(1)? {
            0 => RequiredSatisfactionProof::EffectClosedSuccess {
                success_assertion_digest: proof_digest,
            },
            1 => RequiredSatisfactionProof::ConditionNotApplicable {
                condition_false_proof_digest: proof_digest,
            },
            _ => {
                return Err(TaskStoreError::CorruptRecord(
                    "unknown mixed finalize proof kind",
                ));
            }
        };
        required_satisfaction.push(RequiredSatisfaction { effect_seq, proof });
    }
    Ok(Some(SemanticFinalizeEnvelopeRecord {
        plan_id,
        required_satisfaction,
        fenced_participant_digest,
        prepared_at_ms,
    }))
}

const PLAN_COLUMNS: &str = "plan_id, task_id, permit_id, attempt_id,
     attempt_generation, write_set_root, semantic_append_set_root,
     expected_semantic_count, plan_state, task_receipt_id, created_at_ms, updated_at_ms";

fn load_plan_optional(
    source: &impl SqlRead,
    plan_id: SemanticCommitPlanId,
) -> Result<Option<SemanticCommitPlanRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PLAN_COLUMNS} FROM task_semantic_commit_plans WHERE plan_id = ?1"
    ))?;
    let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_plan_row).transpose()
}

fn load_plan_by_key(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<SemanticCommitPlanRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PLAN_COLUMNS} FROM task_semantic_commit_plans
         WHERE task_id = ?1 AND idempotency_key = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_plan_row).transpose()
}

fn decode_plan_row(row: &Row<'_>) -> Result<SemanticCommitPlanRecord, TaskStoreError> {
    Ok(SemanticCommitPlanRecord {
        plan_id: SemanticCommitPlanId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 2)?),
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 3)?),
        attempt_generation: generation_from_blob(row, 4)?,
        write_set_root: blob32(row, 5)?,
        semantic_append_set_root: blob32(row, 6)?,
        expected_semantic_count: u64_from_blob(row, 7)?,
        state: SemanticCommitPlanState::from_code(row.get(8)?)?,
        task_receipt_id: optional_blob16(row, 9)?.map(ReceiptId::from_bytes),
        created_at_ms: row.get(10)?,
        updated_at_ms: row.get(11)?,
    })
}

fn insert_publication(
    transaction: &Transaction<'_>,
    plan_id: SemanticCommitPlanId,
    receipt: &NestedSemanticPublicationReceipt,
) -> Result<(), TaskStoreError> {
    let created_at_ms = i64::try_from(receipt.created_at_ms).map_err(|_| {
        TaskStoreError::SemanticPublicationConflict {
            reason: "publication timestamp exceeds Task integer range",
        }
    })?;
    let (target_scope_kind, target_scope_id) = target_parts(receipt.target);
    transaction.execute(
        "INSERT INTO task_semantic_publication_receipts (
            plan_id, receipt_id, task_id, permit_id, write_set_root, event_id,
            target_scope_kind, target_scope_id, log_seq, admission_receipt_id,
            durability_receipt_id, semantic_checkpoint_after, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            plan_id.as_bytes().as_slice(),
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.task_id.as_bytes().as_slice(),
            receipt.permit_id.as_bytes().as_slice(),
            receipt.write_set_root.as_slice(),
            receipt.event_id.as_bytes().as_slice(),
            target_scope_kind,
            target_scope_id.as_slice(),
            encode_u64(receipt.log_seq).as_slice(),
            receipt.admission_receipt_id.as_bytes().as_slice(),
            receipt
                .durability_receipt_id
                .map(ReceiptId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            receipt.semantic_checkpoint_after.as_slice(),
            created_at_ms,
        ],
    )?;
    Ok(())
}

const PUBLICATION_COLUMNS: &str = "receipt_id, task_id, permit_id, write_set_root,
     event_id, target_scope_kind, target_scope_id, log_seq, admission_receipt_id,
     durability_receipt_id, semantic_checkpoint_after, created_at_ms";

fn load_publication_by_event(
    source: &impl SqlRead,
    plan_id: SemanticCommitPlanId,
    event_id: SemanticEventId,
) -> Result<Option<NestedSemanticPublicationReceipt>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PUBLICATION_COLUMNS} FROM task_semantic_publication_receipts
         WHERE plan_id = ?1 AND event_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        plan_id.as_bytes().as_slice(),
        event_id.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_publication_row).transpose()
}

fn load_publication_by_receipt_id(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<Option<NestedSemanticPublicationReceipt>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PUBLICATION_COLUMNS} FROM task_semantic_publication_receipts
         WHERE receipt_id = ?1"
    ))?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_publication_row).transpose()
}

fn load_publications(
    source: &impl SqlRead,
    plan_id: SemanticCommitPlanId,
) -> Result<Vec<NestedSemanticPublicationReceipt>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PUBLICATION_COLUMNS} FROM task_semantic_publication_receipts
         WHERE plan_id = ?1 ORDER BY event_id"
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
) -> Result<NestedSemanticPublicationReceipt, TaskStoreError> {
    let (target_kind, target_id) = (row.get::<_, i64>(5)?, blob16(row, 6)?);
    Ok(NestedSemanticPublicationReceipt {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 2)?),
        write_set_root: blob32(row, 3)?,
        event_id: SemanticEventId::from_bytes(blob32(row, 4)?),
        target: target_from_parts(target_kind, target_id)?,
        log_seq: u64_from_blob(row, 7)?,
        admission_receipt_id: ReceiptId::from_bytes(blob16(row, 8)?),
        durability_receipt_id: optional_blob16(row, 9)?.map(ReceiptId::from_bytes),
        semantic_checkpoint_after: blob32(row, 10)?,
        created_at_ms: u64::try_from(row.get::<_, i64>(11)?).map_err(|_| {
            TaskStoreError::CorruptRecord("negative Semantic publication timestamp")
        })?,
    })
}

fn target_parts(target: TaskWriteSetSemanticTarget) -> (i64, [u8; 16]) {
    match target {
        TaskWriteSetSemanticTarget::Namespace(id) => (1, id.into_bytes()),
        TaskWriteSetSemanticTarget::Task(id) => (2, id.into_bytes()),
    }
}

fn target_from_parts(
    kind: i64,
    id: [u8; 16],
) -> Result<TaskWriteSetSemanticTarget, TaskStoreError> {
    match kind {
        1 => Ok(TaskWriteSetSemanticTarget::Namespace(
            nlos_types::NamespaceId::from_bytes(id),
        )),
        2 => Ok(TaskWriteSetSemanticTarget::Task(TaskId::from_bytes(id))),
        _ => Err(TaskStoreError::CorruptRecord(
            "unknown Semantic publication target kind",
        )),
    }
}
