//! Durable retry and escalation ledger for Artifact and Semantic commit
//! recovery.

use nlos_types::{IdempotencyKey, PrincipalId, ReceiptId};
use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::commit::{ArtifactCommitPlanId, ArtifactCommitPlanRecord, ArtifactCommitPlanState};
use crate::semantic_commit::{
    SemanticCommitPlanId, SemanticCommitPlanRecord, SemanticCommitPlanState,
};
use crate::store::{SqlRead, SqliteTaskAuthority, encode_u64, u64_from_blob};
use crate::{TaskStoreError, commit, semantic_commit};

const JITTER_MIN_BPS: u64 = 8_000;
const JITTER_SPAN_BPS: u64 = 4_001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactRecoveryState {
    Retrying,
    Escalated,
    Resolved,
}

impl ArtifactRecoveryState {
    const fn code(self) -> i64 {
        match self {
            Self::Retrying => 0,
            Self::Escalated => 1,
            Self::Resolved => 2,
        }
    }

    fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Retrying),
            1 => Ok(Self::Escalated),
            2 => Ok(Self::Resolved),
            _ => Err(TaskStoreError::CorruptRecord(
                "unknown Artifact recovery state",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactRecoveryFailureSource {
    TaskAuthority,
    ArtifactAuthority,
    Coordinator,
}

impl ArtifactRecoveryFailureSource {
    const fn code(self) -> i64 {
        match self {
            Self::TaskAuthority => 0,
            Self::ArtifactAuthority => 1,
            Self::Coordinator => 2,
        }
    }

    fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::TaskAuthority),
            1 => Ok(Self::ArtifactAuthority),
            2 => Ok(Self::Coordinator),
            _ => Err(TaskStoreError::CorruptRecord(
                "unknown Artifact recovery failure source",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactRecoveryFailureRequest {
    pub plan_id: ArtifactCommitPlanId,
    pub expected_total_failures: u64,
    pub source: ArtifactRecoveryFailureSource,
    pub observed_at_ms: i64,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    pub escalation_threshold: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactRecoveryResumeRequest {
    pub plan_id: ArtifactCommitPlanId,
    pub expected_total_failures: u64,
    pub resumed_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactRecoveryRecord {
    pub plan_id: ArtifactCommitPlanId,
    pub state: ArtifactRecoveryState,
    pub consecutive_failures: u64,
    pub total_failures: u64,
    pub last_source: ArtifactRecoveryFailureSource,
    pub first_failed_at_ms: i64,
    pub last_failed_at_ms: i64,
    pub next_retry_at_ms: Option<i64>,
    pub escalated_at_ms: Option<i64>,
    pub resolved_at_ms: Option<i64>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArtifactRecoverySummary {
    pub retrying: u64,
    pub escalated: u64,
    pub unacknowledged_escalated: u64,
    pub resolved: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactRecoveryAlertAcknowledgeRequest {
    pub plan_id: ArtifactCommitPlanId,
    pub expected_total_failures: u64,
    pub principal_id: PrincipalId,
    pub idempotency_key: IdempotencyKey,
    pub acknowledged_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactRecoveryAlertReceipt {
    pub receipt_id: ReceiptId,
    pub plan_id: ArtifactCommitPlanId,
    pub total_failures: u64,
    pub principal_id: PrincipalId,
    pub idempotency_key: IdempotencyKey,
    pub acknowledged_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactRecoveryAlertAcknowledgeDecision {
    Created(ArtifactRecoveryAlertReceipt),
    Existing(ArtifactRecoveryAlertReceipt),
}

impl ArtifactRecoveryAlertAcknowledgeDecision {
    #[must_use]
    pub const fn receipt(self) -> ArtifactRecoveryAlertReceipt {
        match self {
            Self::Created(receipt) | Self::Existing(receipt) => receipt,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactRecoveryAlert {
    pub recovery: ArtifactRecoveryRecord,
    pub acknowledgement: Option<ArtifactRecoveryAlertReceipt>,
}

/// Escalation threshold for the Semantic recovery ledger. The Artifact
/// ledger receives its threshold per request from the recovery worker
/// (`failure_threshold`, default 8); the Semantic ledger fixes the same
/// default because its request carries no threshold field.
const SEMANTIC_ESCALATION_THRESHOLD: u64 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticRecoveryState {
    Retrying,
    Escalated,
    Resolved,
}

impl SemanticRecoveryState {
    const fn code(self) -> i64 {
        match self {
            Self::Retrying => 0,
            Self::Escalated => 1,
            Self::Resolved => 2,
        }
    }

    fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Retrying),
            1 => Ok(Self::Escalated),
            2 => Ok(Self::Resolved),
            _ => Err(TaskStoreError::CorruptRecord(
                "unknown Semantic recovery state",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticRecoveryFailureSource {
    TaskAuthority,
    SemanticAuthority,
    Coordinator,
}

impl SemanticRecoveryFailureSource {
    const fn code(self) -> i64 {
        match self {
            Self::TaskAuthority => 0,
            Self::SemanticAuthority => 1,
            Self::Coordinator => 2,
        }
    }

    fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::TaskAuthority),
            1 => Ok(Self::SemanticAuthority),
            2 => Ok(Self::Coordinator),
            _ => Err(TaskStoreError::CorruptRecord(
                "unknown Semantic recovery failure source",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticRecoveryFailureRequest {
    pub plan_id: SemanticCommitPlanId,
    pub expected_total_failures: u64,
    pub source: SemanticRecoveryFailureSource,
    pub observed_at_ms: i64,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticRecoveryResumeRequest {
    pub plan_id: SemanticCommitPlanId,
    pub expected_total_failures: u64,
    pub resumed_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticRecoveryRecord {
    pub plan_id: SemanticCommitPlanId,
    pub state: SemanticRecoveryState,
    pub consecutive_failures: u64,
    pub total_failures: u64,
    pub last_source: SemanticRecoveryFailureSource,
    pub first_failed_at_ms: i64,
    pub last_failed_at_ms: i64,
    pub next_retry_at_ms: Option<i64>,
    pub escalated_at_ms: Option<i64>,
    pub resolved_at_ms: Option<i64>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SemanticRecoverySummary {
    pub retrying: u64,
    pub escalated: u64,
    pub unacknowledged_escalated: u64,
    pub resolved: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticRecoveryAlertAcknowledgeRequest {
    pub plan_id: SemanticCommitPlanId,
    pub expected_total_failures: u64,
    pub principal_id: PrincipalId,
    pub idempotency_key: IdempotencyKey,
    pub acknowledged_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticRecoveryAlertReceipt {
    pub receipt_id: ReceiptId,
    pub plan_id: SemanticCommitPlanId,
    pub total_failures: u64,
    pub principal_id: PrincipalId,
    pub idempotency_key: IdempotencyKey,
    pub acknowledged_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticRecoveryAlertAcknowledgeDecision {
    Acknowledged(SemanticRecoveryAlertReceipt),
    Replayed(SemanticRecoveryAlertReceipt),
}

impl SemanticRecoveryAlertAcknowledgeDecision {
    #[must_use]
    pub const fn receipt(self) -> SemanticRecoveryAlertReceipt {
        match self {
            Self::Acknowledged(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticRecoveryAlert {
    pub recovery: SemanticRecoveryRecord,
    pub acknowledgement: Option<SemanticRecoveryAlertReceipt>,
}

impl SqliteTaskAuthority {
    /// Appends one failed recovery cycle and computes its durable next due
    /// time or escalation state.
    ///
    /// # Errors
    ///
    /// Returns a typed policy/state/not-found error, epoch exhaustion, or a
    /// storage failure. No partial ledger update is committed on error.
    pub fn record_artifact_recovery_failure(
        &self,
        request: ArtifactRecoveryFailureRequest,
    ) -> Result<ArtifactRecoveryRecord, TaskStoreError> {
        validate_request(request)?;
        let mut connection = self.lock_connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let plan = commit::load_plan_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::ArtifactCommitPlanNotFound)?;
        if plan.state == ArtifactCommitPlanState::Finalized {
            return Err(TaskStoreError::InvalidArtifactRecoveryState {
                state: ArtifactRecoveryState::Resolved,
            });
        }
        let prior = load_optional(&transaction, request.plan_id)?;
        let current_total = prior.map_or(0, |record| record.total_failures);
        if current_total != request.expected_total_failures {
            return Err(TaskStoreError::ArtifactRecoveryCasMismatch {
                expected: request.expected_total_failures,
                current: current_total,
            });
        }
        if let Some(record) = prior
            && record.state != ArtifactRecoveryState::Retrying
        {
            return Err(TaskStoreError::InvalidArtifactRecoveryState {
                state: record.state,
            });
        }
        if prior.is_some_and(|record| request.observed_at_ms < record.last_failed_at_ms) {
            return Err(TaskStoreError::InvalidArtifactRecoveryPolicy {
                reason: "failure timestamp regresses durable history",
            });
        }
        let consecutive = prior
            .map_or(0, |record| record.consecutive_failures)
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let total = current_total
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let escalated = consecutive >= request.escalation_threshold;
        let next_retry_at_ms = if escalated {
            None
        } else {
            Some(
                request
                    .observed_at_ms
                    .checked_add(
                        i64::try_from(jittered_delay(&request, consecutive)?).map_err(|_| {
                            TaskStoreError::InvalidArtifactRecoveryPolicy {
                                reason: "retry delay exceeds i64 milliseconds",
                            }
                        })?,
                    )
                    .ok_or(TaskStoreError::EpochExhausted)?,
            )
        };
        let record = ArtifactRecoveryRecord {
            plan_id: request.plan_id,
            state: if escalated {
                ArtifactRecoveryState::Escalated
            } else {
                ArtifactRecoveryState::Retrying
            },
            consecutive_failures: consecutive,
            total_failures: total,
            last_source: request.source,
            first_failed_at_ms: prior
                .map_or(request.observed_at_ms, |record| record.first_failed_at_ms),
            last_failed_at_ms: request.observed_at_ms,
            next_retry_at_ms,
            escalated_at_ms: escalated.then_some(request.observed_at_ms),
            resolved_at_ms: None,
            updated_at_ms: request.observed_at_ms,
        };
        upsert(&transaction, &record)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Reads the optional durable recovery ledger for one plan.
    ///
    /// # Errors
    ///
    /// Returns corrupt-record or storage failures.
    pub fn inspect_artifact_recovery(
        &self,
        plan_id: ArtifactCommitPlanId,
    ) -> Result<Option<ArtifactRecoveryRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_optional(&*connection, plan_id)
    }

    /// Returns bounded aggregate counts for the local operations health
    /// surface without exposing diagnostic strings.
    ///
    /// # Errors
    ///
    /// Returns a storage failure or corrupt negative count.
    pub fn summarize_artifact_recovery(&self) -> Result<ArtifactRecoverySummary, TaskStoreError> {
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT
                COALESCE(SUM(CASE WHEN recovery_state = 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN recovery_state = 1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN recovery_state = 1 AND NOT EXISTS (
                    SELECT 1 FROM task_artifact_recovery_alert_receipts AS receipts
                    WHERE receipts.plan_id = task_artifact_recovery.plan_id
                      AND receipts.total_failures = task_artifact_recovery.total_failures
                ) THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN recovery_state = 2 THEN 1 ELSE 0 END), 0)
             FROM task_artifact_recovery",
        )?;
        statement
            .query_row([], |row| {
                Ok(ArtifactRecoverySummary {
                    retrying: count_from_i64(row.get(0)?)?,
                    escalated: count_from_i64(row.get(1)?)?,
                    unacknowledged_escalated: count_from_i64(row.get(2)?)?,
                    resolved: count_from_i64(row.get(3)?)?,
                })
            })
            .map_err(TaskStoreError::from)
    }

    /// Returns a bounded, stable list of escalated recovery alerts and their
    /// optional immutable acknowledgement receipt.
    ///
    /// # Errors
    ///
    /// Returns corrupt-record or storage failures.
    pub fn list_artifact_recovery_alerts(
        &self,
        limit: usize,
    ) -> Result<Vec<ArtifactRecoveryAlert>, TaskStoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT plan_id FROM task_artifact_recovery
             WHERE recovery_state = ?1
             ORDER BY escalated_at_ms, plan_id LIMIT ?2",
        )?;
        let mut rows = statement.query(params![
            ArtifactRecoveryState::Escalated.code(),
            i64::try_from(limit).unwrap_or(i64::MAX),
        ])?;
        let mut plan_ids = Vec::new();
        while let Some(row) = rows.next()? {
            plan_ids.push(ArtifactCommitPlanId::from_bytes(crate::store::blob16(
                row, 0,
            )?));
        }
        drop(rows);
        drop(statement);
        plan_ids
            .into_iter()
            .map(|plan_id| {
                let recovery = load_optional(&*connection, plan_id)?
                    .ok_or(TaskStoreError::ArtifactRecoveryNotFound)?;
                let acknowledgement =
                    load_alert_receipt_optional(&*connection, plan_id, recovery.total_failures)?;
                Ok(ArtifactRecoveryAlert {
                    recovery,
                    acknowledgement,
                })
            })
            .collect()
    }

    /// Acknowledges one exact escalation instance without resuming it.
    /// The failure-count CAS prevents a stale UI from acknowledging a later
    /// escalation, and the immutable receipt makes exact retries restart-safe.
    ///
    /// # Errors
    ///
    /// Returns not-found, stale-CAS, invalid-state/timestamp, idempotency, or
    /// storage failures. No partial acknowledgement is committed on error.
    pub fn acknowledge_artifact_recovery_alert(
        &self,
        request: ArtifactRecoveryAlertAcknowledgeRequest,
    ) -> Result<ArtifactRecoveryAlertAcknowledgeDecision, TaskStoreError> {
        if request.acknowledged_at_ms < 0 {
            return Err(TaskStoreError::InvalidArtifactRecoveryPolicy {
                reason: "acknowledgement timestamp must be non-negative",
            });
        }
        let mut connection = self.lock_connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(receipt) =
            load_alert_receipt_by_idempotency_key(&transaction, request.idempotency_key)?
        {
            if receipt.plan_id != request.plan_id
                || receipt.total_failures != request.expected_total_failures
                || receipt.principal_id != request.principal_id
            {
                return Err(TaskStoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(ArtifactRecoveryAlertAcknowledgeDecision::Existing(receipt));
        }
        let recovery = load_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::ArtifactRecoveryNotFound)?;
        if recovery.total_failures != request.expected_total_failures {
            return Err(TaskStoreError::ArtifactRecoveryCasMismatch {
                expected: request.expected_total_failures,
                current: recovery.total_failures,
            });
        }
        if recovery.state != ArtifactRecoveryState::Escalated {
            return Err(TaskStoreError::InvalidArtifactRecoveryState {
                state: recovery.state,
            });
        }
        if request.acknowledged_at_ms < recovery.last_failed_at_ms {
            return Err(TaskStoreError::InvalidArtifactRecoveryPolicy {
                reason: "acknowledgement timestamp regresses durable history",
            });
        }
        if let Some(receipt) = load_alert_receipt_optional(
            &transaction,
            request.plan_id,
            request.expected_total_failures,
        )? {
            transaction.commit()?;
            return Ok(ArtifactRecoveryAlertAcknowledgeDecision::Existing(receipt));
        }
        let receipt = ArtifactRecoveryAlertReceipt {
            receipt_id: derive_alert_receipt_id(request.plan_id, request.expected_total_failures),
            plan_id: request.plan_id,
            total_failures: request.expected_total_failures,
            principal_id: request.principal_id,
            idempotency_key: request.idempotency_key,
            acknowledged_at_ms: request.acknowledged_at_ms,
        };
        insert_alert_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(ArtifactRecoveryAlertAcknowledgeDecision::Created(receipt))
    }

    /// Lists non-finalized plans whose durable retry time is due. Escalated
    /// plans are excluded until an explicit CAS resume.
    ///
    /// # Errors
    ///
    /// Returns an invalid timestamp, corrupt-record, or storage failure.
    pub fn list_due_artifact_commit_plans(
        &self,
        limit: usize,
        now_ms: i64,
    ) -> Result<Vec<ArtifactCommitPlanRecord>, TaskStoreError> {
        if now_ms < 0 {
            return Err(TaskStoreError::InvalidArtifactRecoveryPolicy {
                reason: "scan timestamp must be non-negative",
            });
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT plans.plan_id FROM task_artifact_commit_plans AS plans
             LEFT JOIN task_artifact_recovery AS recovery ON recovery.plan_id = plans.plan_id
             WHERE plans.plan_state != ?1 AND (
                recovery.plan_id IS NULL OR
                (recovery.recovery_state = ?2 AND recovery.next_retry_at_ms <= ?3)
             )
             ORDER BY plans.created_at_ms, plans.plan_id LIMIT ?4",
        )?;
        let mut rows = statement.query(params![
            ArtifactCommitPlanState::Finalized.code(),
            ArtifactRecoveryState::Retrying.code(),
            now_ms,
            i64::try_from(limit).unwrap_or(i64::MAX),
        ])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(ArtifactCommitPlanId::from_bytes(crate::store::blob16(
                row, 0,
            )?));
        }
        drop(rows);
        drop(statement);
        ids.into_iter()
            .map(|plan_id| {
                commit::load_plan_optional(&*connection, plan_id)?
                    .ok_or(TaskStoreError::ArtifactCommitPlanNotFound)
            })
            .collect()
    }

    /// Requeues one escalated plan using its total-failure count as a CAS.
    ///
    /// # Errors
    ///
    /// Returns not-found, stale-CAS, invalid-state/timestamp, or storage
    /// failures. Total failure history is preserved.
    pub fn resume_artifact_recovery(
        &self,
        request: ArtifactRecoveryResumeRequest,
    ) -> Result<ArtifactRecoveryRecord, TaskStoreError> {
        if request.resumed_at_ms < 0 {
            return Err(TaskStoreError::InvalidArtifactRecoveryPolicy {
                reason: "resume timestamp must be non-negative",
            });
        }
        let mut connection = self.lock_connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut record = load_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::ArtifactRecoveryNotFound)?;
        if record.total_failures != request.expected_total_failures {
            return Err(TaskStoreError::ArtifactRecoveryCasMismatch {
                expected: request.expected_total_failures,
                current: record.total_failures,
            });
        }
        if record.state != ArtifactRecoveryState::Escalated {
            return Err(TaskStoreError::InvalidArtifactRecoveryState {
                state: record.state,
            });
        }
        if request.resumed_at_ms < record.last_failed_at_ms {
            return Err(TaskStoreError::InvalidArtifactRecoveryPolicy {
                reason: "resume timestamp regresses durable history",
            });
        }
        record.state = ArtifactRecoveryState::Retrying;
        record.consecutive_failures = 0;
        record.next_retry_at_ms = Some(request.resumed_at_ms);
        record.escalated_at_ms = None;
        record.updated_at_ms = request.resumed_at_ms;
        upsert(&transaction, &record)?;
        transaction.commit()?;
        Ok(record)
    }
}

impl SqliteTaskAuthority {
    /// Appends one failed Semantic recovery cycle and computes its durable
    /// next due time or escalation state.
    ///
    /// # Errors
    ///
    /// Returns a typed policy/state/not-found error, epoch exhaustion, or a
    /// storage failure. No partial ledger update is committed on error.
    pub fn record_semantic_recovery_failure(
        &self,
        request: SemanticRecoveryFailureRequest,
    ) -> Result<SemanticRecoveryRecord, TaskStoreError> {
        semantic_validate_request(request)?;
        let mut connection = self.lock_connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let plan = semantic_commit::load_plan_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
        if plan.state == SemanticCommitPlanState::Finalized {
            return Err(TaskStoreError::InvalidSemanticRecoveryState {
                state: SemanticRecoveryState::Resolved,
            });
        }
        let prior = semantic_load_optional(&transaction, request.plan_id)?;
        let current_total = prior.map_or(0, |record| record.total_failures);
        if current_total != request.expected_total_failures {
            return Err(TaskStoreError::SemanticRecoveryCasMismatch {
                expected: request.expected_total_failures,
                current: current_total,
            });
        }
        if let Some(record) = prior
            && record.state != SemanticRecoveryState::Retrying
        {
            return Err(TaskStoreError::InvalidSemanticRecoveryState {
                state: record.state,
            });
        }
        if prior.is_some_and(|record| request.observed_at_ms < record.last_failed_at_ms) {
            return Err(TaskStoreError::InvalidSemanticRecoveryPolicy {
                reason: "failure timestamp regresses durable history",
            });
        }
        let consecutive = prior
            .map_or(0, |record| record.consecutive_failures)
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let total = current_total
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let escalated = consecutive >= SEMANTIC_ESCALATION_THRESHOLD;
        let next_retry_at_ms = if escalated {
            None
        } else {
            Some(
                request
                    .observed_at_ms
                    .checked_add(
                        i64::try_from(capped_exponential_delay(&request, consecutive)).map_err(
                            |_| TaskStoreError::InvalidSemanticRecoveryPolicy {
                                reason: "retry delay exceeds i64 milliseconds",
                            },
                        )?,
                    )
                    .ok_or(TaskStoreError::EpochExhausted)?,
            )
        };
        let record = SemanticRecoveryRecord {
            plan_id: request.plan_id,
            state: if escalated {
                SemanticRecoveryState::Escalated
            } else {
                SemanticRecoveryState::Retrying
            },
            consecutive_failures: consecutive,
            total_failures: total,
            last_source: request.source,
            first_failed_at_ms: prior
                .map_or(request.observed_at_ms, |record| record.first_failed_at_ms),
            last_failed_at_ms: request.observed_at_ms,
            next_retry_at_ms,
            escalated_at_ms: escalated.then_some(request.observed_at_ms),
            resolved_at_ms: None,
            updated_at_ms: request.observed_at_ms,
        };
        semantic_upsert(&transaction, &record)?;
        transaction.commit()?;
        Ok(record)
    }

    /// Reads the optional durable Semantic recovery ledger for one plan.
    ///
    /// # Errors
    ///
    /// Returns corrupt-record or storage failures.
    pub fn inspect_semantic_recovery(
        &self,
        plan_id: SemanticCommitPlanId,
    ) -> Result<Option<SemanticRecoveryRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        semantic_load_optional(&*connection, plan_id)
    }

    /// Returns bounded aggregate counts for the local operations health
    /// surface without exposing diagnostic strings.
    ///
    /// # Errors
    ///
    /// Returns a storage failure or corrupt negative count.
    pub fn summarize_semantic_recovery(&self) -> Result<SemanticRecoverySummary, TaskStoreError> {
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT
                COALESCE(SUM(CASE WHEN recovery_state = 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN recovery_state = 1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN recovery_state = 1 AND NOT EXISTS (
                    SELECT 1 FROM task_semantic_recovery_alert_receipts AS receipts
                    WHERE receipts.plan_id = task_semantic_recovery.plan_id
                      AND receipts.total_failures = task_semantic_recovery.total_failures
                ) THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN recovery_state = 2 THEN 1 ELSE 0 END), 0)
             FROM task_semantic_recovery",
        )?;
        statement
            .query_row([], |row| {
                Ok(SemanticRecoverySummary {
                    retrying: count_from_i64(row.get(0)?)?,
                    escalated: count_from_i64(row.get(1)?)?,
                    unacknowledged_escalated: count_from_i64(row.get(2)?)?,
                    resolved: count_from_i64(row.get(3)?)?,
                })
            })
            .map_err(TaskStoreError::from)
    }

    /// Returns a bounded, stable list of escalated Semantic recovery alerts
    /// and their optional immutable acknowledgement receipt.
    ///
    /// Deviation from the Artifact mirror (`list_artifact_recovery_alerts`):
    /// the W26-001 interface pins a zero-argument shape, so no `limit` is
    /// taken; the result stays bounded by the escalated-state filter and is
    /// ordered by escalation time then plan id. A ledger row that vanishes
    /// between the two read passes reports
    /// [`TaskStoreError::SemanticCommitPlanNotFound`] — the dedicated
    /// ledger-not-found variant of the artifact family
    /// (`ArtifactRecoveryNotFound`) is deliberately not added, so the
    /// plan-not-found variant is reused (see `resume_semantic_recovery`).
    ///
    /// # Errors
    ///
    /// Returns corrupt-record or storage failures.
    pub fn list_semantic_recovery_alerts(
        &self,
    ) -> Result<Vec<SemanticRecoveryAlert>, TaskStoreError> {
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT plan_id FROM task_semantic_recovery
             WHERE recovery_state = ?1
             ORDER BY escalated_at_ms, plan_id",
        )?;
        let mut rows = statement.query(params![SemanticRecoveryState::Escalated.code()])?;
        let mut plan_ids = Vec::new();
        while let Some(row) = rows.next()? {
            plan_ids.push(SemanticCommitPlanId::from_bytes(crate::store::blob16(
                row, 0,
            )?));
        }
        drop(rows);
        drop(statement);
        plan_ids
            .into_iter()
            .map(|plan_id| {
                let recovery = semantic_load_optional(&*connection, plan_id)?
                    .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
                let acknowledgement = semantic_load_alert_receipt_optional(
                    &*connection,
                    plan_id,
                    recovery.total_failures,
                )?;
                Ok(SemanticRecoveryAlert {
                    recovery,
                    acknowledgement,
                })
            })
            .collect()
    }

    /// Acknowledges one exact Semantic escalation instance without resuming
    /// it. The failure-count CAS prevents a stale UI from acknowledging a
    /// later escalation, and the immutable receipt (enforced by the v42
    /// triggers on `task_semantic_recovery_alert_receipts`) makes exact
    /// retries restart-safe: a replay of the same idempotency key returns
    /// [`SemanticRecoveryAlertAcknowledgeDecision::Replayed`] with the
    /// original bytes and never double-records
    /// (`UNIQUE(plan_id, total_failures)`).
    ///
    /// A missing ledger row reports
    /// [`TaskStoreError::SemanticCommitPlanNotFound`]: the artifact family's
    /// dedicated `ArtifactRecoveryNotFound` variant has no Semantic
    /// counterpart because this lane adds no `TaskStoreError` variants.
    ///
    /// # Errors
    ///
    /// Returns not-found, stale-CAS, invalid-state/timestamp, idempotency,
    /// or storage failures. No partial acknowledgement is committed on
    /// error.
    pub fn acknowledge_semantic_recovery_alert(
        &self,
        request: SemanticRecoveryAlertAcknowledgeRequest,
    ) -> Result<SemanticRecoveryAlertAcknowledgeDecision, TaskStoreError> {
        if request.acknowledged_at_ms < 0 {
            return Err(TaskStoreError::InvalidSemanticRecoveryPolicy {
                reason: "acknowledgement timestamp must be non-negative",
            });
        }
        let mut connection = self.lock_connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(receipt) =
            semantic_load_alert_receipt_by_idempotency_key(&transaction, request.idempotency_key)?
        {
            if receipt.plan_id != request.plan_id
                || receipt.total_failures != request.expected_total_failures
                || receipt.principal_id != request.principal_id
            {
                return Err(TaskStoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(SemanticRecoveryAlertAcknowledgeDecision::Replayed(receipt));
        }
        let recovery = semantic_load_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
        if recovery.total_failures != request.expected_total_failures {
            return Err(TaskStoreError::SemanticRecoveryCasMismatch {
                expected: request.expected_total_failures,
                current: recovery.total_failures,
            });
        }
        if recovery.state != SemanticRecoveryState::Escalated {
            return Err(TaskStoreError::InvalidSemanticRecoveryState {
                state: recovery.state,
            });
        }
        if request.acknowledged_at_ms < recovery.last_failed_at_ms {
            return Err(TaskStoreError::InvalidSemanticRecoveryPolicy {
                reason: "acknowledgement timestamp regresses durable history",
            });
        }
        if let Some(receipt) = semantic_load_alert_receipt_optional(
            &transaction,
            request.plan_id,
            request.expected_total_failures,
        )? {
            transaction.commit()?;
            return Ok(SemanticRecoveryAlertAcknowledgeDecision::Replayed(receipt));
        }
        let receipt = SemanticRecoveryAlertReceipt {
            receipt_id: derive_semantic_alert_receipt_id(
                request.plan_id,
                request.expected_total_failures,
            ),
            plan_id: request.plan_id,
            total_failures: request.expected_total_failures,
            principal_id: request.principal_id,
            idempotency_key: request.idempotency_key,
            acknowledged_at_ms: request.acknowledged_at_ms,
        };
        semantic_insert_alert_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(SemanticRecoveryAlertAcknowledgeDecision::Acknowledged(
            receipt,
        ))
    }

    /// Lists non-finalized Semantic plans whose durable retry time is due.
    /// Escalated plans are excluded until an explicit CAS resume. A plan
    /// with no ledger row is returned immediately: the plan itself is the
    /// durable fact and a lost ledger row is rebuilt by rescanning it
    /// (spec SEM-RECOV-005).
    ///
    /// # Errors
    ///
    /// Returns an invalid timestamp, corrupt-record, or storage failure.
    pub fn list_due_semantic_commit_plans(
        &self,
        limit: usize,
        now_ms: i64,
    ) -> Result<Vec<SemanticCommitPlanRecord>, TaskStoreError> {
        if now_ms < 0 {
            return Err(TaskStoreError::InvalidSemanticRecoveryPolicy {
                reason: "scan timestamp must be non-negative",
            });
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT plans.plan_id FROM task_semantic_commit_plans AS plans
             LEFT JOIN task_semantic_recovery AS recovery ON recovery.plan_id = plans.plan_id
             WHERE plans.plan_state != ?1 AND (
                recovery.plan_id IS NULL OR
                (recovery.recovery_state = ?2 AND recovery.next_retry_at_ms <= ?3)
             )
             ORDER BY plans.created_at_ms, plans.plan_id LIMIT ?4",
        )?;
        let mut rows = statement.query(params![
            SemanticCommitPlanState::Finalized.code(),
            SemanticRecoveryState::Retrying.code(),
            now_ms,
            i64::try_from(limit).unwrap_or(i64::MAX),
        ])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(SemanticCommitPlanId::from_bytes(crate::store::blob16(
                row, 0,
            )?));
        }
        drop(rows);
        drop(statement);
        ids.into_iter()
            .map(|plan_id| {
                semantic_commit::load_plan_optional(&*connection, plan_id)?
                    .ok_or(TaskStoreError::SemanticCommitPlanNotFound)
            })
            .collect()
    }

    /// Requeues one escalated Semantic plan using its total-failure count as
    /// a CAS. A plan with no ledger row reports
    /// [`TaskStoreError::SemanticCommitPlanNotFound`]; a dedicated Semantic
    /// ledger-not-found variant mirroring the artifact family's
    /// `ArtifactRecoveryNotFound` is deliberately not added (this lane adds
    /// no `TaskStoreError` variants), so the plan-not-found variant is
    /// reused. Known mirrored wart (tracked, unchanged):
    /// `resume_artifact_recovery` likewise does not re-read the plan, so an
    /// `Escalated` row for an already-`Finalized` plan can be resumed into
    /// an orphan `Retrying` row — the due scan excludes `Finalized` plans,
    /// so the orphan never re-enters scheduling and stays inert.
    ///
    /// # Errors
    ///
    /// Returns not-found, stale-CAS, invalid-state/timestamp, or storage
    /// failures. Total failure history is preserved.
    pub fn resume_semantic_recovery(
        &self,
        request: SemanticRecoveryResumeRequest,
    ) -> Result<SemanticRecoveryRecord, TaskStoreError> {
        if request.resumed_at_ms < 0 {
            return Err(TaskStoreError::InvalidSemanticRecoveryPolicy {
                reason: "resume timestamp must be non-negative",
            });
        }
        let mut connection = self.lock_connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut record = semantic_load_optional(&transaction, request.plan_id)?
            .ok_or(TaskStoreError::SemanticCommitPlanNotFound)?;
        if record.total_failures != request.expected_total_failures {
            return Err(TaskStoreError::SemanticRecoveryCasMismatch {
                expected: request.expected_total_failures,
                current: record.total_failures,
            });
        }
        if record.state != SemanticRecoveryState::Escalated {
            return Err(TaskStoreError::InvalidSemanticRecoveryState {
                state: record.state,
            });
        }
        if request.resumed_at_ms < record.last_failed_at_ms {
            return Err(TaskStoreError::InvalidSemanticRecoveryPolicy {
                reason: "resume timestamp regresses durable history",
            });
        }
        record.state = SemanticRecoveryState::Retrying;
        record.consecutive_failures = 0;
        record.next_retry_at_ms = Some(request.resumed_at_ms);
        record.escalated_at_ms = None;
        record.updated_at_ms = request.resumed_at_ms;
        semantic_upsert(&transaction, &record)?;
        transaction.commit()?;
        Ok(record)
    }
}

fn count_from_i64(value: i64) -> Result<u64, rusqlite::Error> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

pub(crate) fn resolve_recovery(
    transaction: &rusqlite::Transaction<'_>,
    plan_id: ArtifactCommitPlanId,
    resolved_at_ms: i64,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "UPDATE task_artifact_recovery SET recovery_state = ?1,
         consecutive_failures = ?2, next_retry_at_ms = NULL,
         escalated_at_ms = NULL, resolved_at_ms = ?3, updated_at_ms = ?3
         WHERE plan_id = ?4 AND recovery_state != ?1",
        params![
            ArtifactRecoveryState::Resolved.code(),
            encode_u64(0).as_slice(),
            resolved_at_ms,
            plan_id.as_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

/// Semantic mirror of [`resolve_recovery`]: flips any non-`Resolved` ledger
/// row for one plan to `Resolved` inside the caller's finalize transaction
/// (`SEM-RECOV-004`). Total failure history is preserved; a missing row is
/// a no-op so a lost ledger row never blocks or re-derives from a
/// successful finalize. The `WHERE recovery_state != ?1` guard makes
/// repeated calls idempotent.
pub(crate) fn resolve_semantic_recovery(
    transaction: &rusqlite::Transaction<'_>,
    plan_id: SemanticCommitPlanId,
    resolved_at_ms: i64,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "UPDATE task_semantic_recovery SET recovery_state = ?1,
         consecutive_failures = ?2, next_retry_at_ms = NULL,
         escalated_at_ms = NULL, resolved_at_ms = ?3, updated_at_ms = ?3
         WHERE plan_id = ?4 AND recovery_state != ?1",
        params![
            SemanticRecoveryState::Resolved.code(),
            encode_u64(0).as_slice(),
            resolved_at_ms,
            plan_id.as_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

fn validate_request(request: ArtifactRecoveryFailureRequest) -> Result<(), TaskStoreError> {
    let reason = if request.observed_at_ms < 0 {
        Some("failure timestamp must be non-negative")
    } else if request.base_delay_ms == 0 {
        Some("base delay must be non-zero")
    } else if request.max_delay_ms < request.base_delay_ms {
        Some("maximum delay must be at least base delay")
    } else if request.escalation_threshold == 0 {
        Some("escalation threshold must be non-zero")
    } else {
        None
    };
    reason.map_or(Ok(()), |reason| {
        Err(TaskStoreError::InvalidArtifactRecoveryPolicy { reason })
    })
}

fn jittered_delay(
    request: &ArtifactRecoveryFailureRequest,
    consecutive: u64,
) -> Result<u64, TaskStoreError> {
    let exponent = u32::try_from(consecutive.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(63);
    let exponential = request
        .base_delay_ms
        .checked_mul(1_u64 << exponent)
        .unwrap_or(request.max_delay_ms)
        .min(request.max_delay_ms);
    let mut hasher = Sha256::new();
    hasher.update(b"nlos.task.artifact-recovery-jitter.v1\0");
    hasher.update(request.plan_id.as_bytes());
    hasher.update(consecutive.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let sample = u64::from_be_bytes(
        digest[..8]
            .try_into()
            .map_err(|_| TaskStoreError::CorruptRecord("recovery jitter digest width mismatch"))?,
    );
    let basis_points = JITTER_MIN_BPS + sample % JITTER_SPAN_BPS;
    let jittered = u128::from(exponential) * u128::from(basis_points) / 10_000;
    Ok(u64::try_from(jittered)
        .unwrap_or(request.max_delay_ms)
        .clamp(1, request.max_delay_ms))
}

fn derive_alert_receipt_id(plan_id: ArtifactCommitPlanId, total_failures: u64) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-artifact-recovery-alert-ack/v1\0");
    hasher.update(plan_id.as_bytes());
    hasher.update(total_failures.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(id)
}

fn insert_alert_receipt(
    transaction: &rusqlite::Transaction<'_>,
    receipt: &ArtifactRecoveryAlertReceipt,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_artifact_recovery_alert_receipts (
            receipt_id, plan_id, total_failures, principal_id,
            idempotency_key, acknowledged_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.plan_id.as_bytes().as_slice(),
            encode_u64(receipt.total_failures).as_slice(),
            receipt.principal_id.as_bytes().as_slice(),
            receipt.idempotency_key.as_bytes().as_slice(),
            receipt.acknowledged_at_ms,
        ],
    )?;
    Ok(())
}

fn load_alert_receipt_optional(
    reader: &impl SqlRead,
    plan_id: ArtifactCommitPlanId,
    total_failures: u64,
) -> Result<Option<ArtifactRecoveryAlertReceipt>, TaskStoreError> {
    load_alert_receipt(
        reader,
        "SELECT receipt_id, plan_id, total_failures, principal_id,
                idempotency_key, acknowledged_at_ms
         FROM task_artifact_recovery_alert_receipts
         WHERE plan_id = ?1 AND total_failures = ?2",
        params![
            plan_id.as_bytes().as_slice(),
            encode_u64(total_failures).as_slice()
        ],
    )
}

fn load_alert_receipt_by_idempotency_key(
    reader: &impl SqlRead,
    idempotency_key: IdempotencyKey,
) -> Result<Option<ArtifactRecoveryAlertReceipt>, TaskStoreError> {
    load_alert_receipt(
        reader,
        "SELECT receipt_id, plan_id, total_failures, principal_id,
                idempotency_key, acknowledged_at_ms
         FROM task_artifact_recovery_alert_receipts
         WHERE idempotency_key = ?1",
        [idempotency_key.as_bytes().as_slice()],
    )
}

fn load_alert_receipt<P: rusqlite::Params>(
    reader: &impl SqlRead,
    sql: &str,
    params: P,
) -> Result<Option<ArtifactRecoveryAlertReceipt>, TaskStoreError> {
    let mut statement = reader.prepare_statement(sql)?;
    let mut rows = statement.query(params)?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(ArtifactRecoveryAlertReceipt {
        receipt_id: ReceiptId::from_bytes(crate::store::blob16(row, 0)?),
        plan_id: ArtifactCommitPlanId::from_bytes(crate::store::blob16(row, 1)?),
        total_failures: u64_from_blob(row, 2)?,
        principal_id: PrincipalId::from_bytes(crate::store::blob16(row, 3)?),
        idempotency_key: IdempotencyKey::from_bytes(crate::store::blob16(row, 4)?),
        acknowledged_at_ms: row.get(5)?,
    }))
}

fn derive_semantic_alert_receipt_id(
    plan_id: SemanticCommitPlanId,
    total_failures: u64,
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-semantic-recovery-alert-ack/v1\0");
    hasher.update(plan_id.as_bytes());
    hasher.update(total_failures.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(id)
}

/// Deterministic 16-byte reference naming one semantic recovery resume
/// outcome (`Escalated`→`Retrying` at one `total_failures` revision).
///
/// The resume transition itself is the durable evidence — the ledger row
/// moves under the same CAS the command carried — but unlike an
/// acknowledgement no receipt row is stored, so this reference only names
/// the outcome: it is stable across idempotent replays of the same resume
/// command and domain-separated from the alert acknowledgement derivation.
#[must_use]
pub fn semantic_recovery_resume_reference(
    plan_id: SemanticCommitPlanId,
    total_failures: u64,
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-semantic-recovery-resume/v1\0");
    hasher.update(plan_id.as_bytes());
    hasher.update(total_failures.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(id)
}

fn semantic_insert_alert_receipt(
    transaction: &rusqlite::Transaction<'_>,
    receipt: &SemanticRecoveryAlertReceipt,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_semantic_recovery_alert_receipts (
            receipt_id, plan_id, total_failures, principal_id,
            idempotency_key, acknowledged_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.plan_id.as_bytes().as_slice(),
            encode_u64(receipt.total_failures).as_slice(),
            receipt.principal_id.as_bytes().as_slice(),
            receipt.idempotency_key.as_bytes().as_slice(),
            receipt.acknowledged_at_ms,
        ],
    )?;
    Ok(())
}

fn semantic_load_alert_receipt_optional(
    reader: &impl SqlRead,
    plan_id: SemanticCommitPlanId,
    total_failures: u64,
) -> Result<Option<SemanticRecoveryAlertReceipt>, TaskStoreError> {
    semantic_load_alert_receipt(
        reader,
        "SELECT receipt_id, plan_id, total_failures, principal_id,
                idempotency_key, acknowledged_at_ms
         FROM task_semantic_recovery_alert_receipts
         WHERE plan_id = ?1 AND total_failures = ?2",
        params![
            plan_id.as_bytes().as_slice(),
            encode_u64(total_failures).as_slice()
        ],
    )
}

fn semantic_load_alert_receipt_by_idempotency_key(
    reader: &impl SqlRead,
    idempotency_key: IdempotencyKey,
) -> Result<Option<SemanticRecoveryAlertReceipt>, TaskStoreError> {
    semantic_load_alert_receipt(
        reader,
        "SELECT receipt_id, plan_id, total_failures, principal_id,
                idempotency_key, acknowledged_at_ms
         FROM task_semantic_recovery_alert_receipts
         WHERE idempotency_key = ?1",
        [idempotency_key.as_bytes().as_slice()],
    )
}

fn semantic_load_alert_receipt<P: rusqlite::Params>(
    reader: &impl SqlRead,
    sql: &str,
    params: P,
) -> Result<Option<SemanticRecoveryAlertReceipt>, TaskStoreError> {
    let mut statement = reader.prepare_statement(sql)?;
    let mut rows = statement.query(params)?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(SemanticRecoveryAlertReceipt {
        receipt_id: ReceiptId::from_bytes(crate::store::blob16(row, 0)?),
        plan_id: SemanticCommitPlanId::from_bytes(crate::store::blob16(row, 1)?),
        total_failures: u64_from_blob(row, 2)?,
        principal_id: PrincipalId::from_bytes(crate::store::blob16(row, 3)?),
        idempotency_key: IdempotencyKey::from_bytes(crate::store::blob16(row, 4)?),
        acknowledged_at_ms: row.get(5)?,
    }))
}

fn upsert(
    transaction: &rusqlite::Transaction<'_>,
    record: &ArtifactRecoveryRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_artifact_recovery (
            plan_id, recovery_state, consecutive_failures, total_failures,
            last_failure_source, first_failed_at_ms, last_failed_at_ms,
            next_retry_at_ms, escalated_at_ms, resolved_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(plan_id) DO UPDATE SET
            recovery_state = excluded.recovery_state,
            consecutive_failures = excluded.consecutive_failures,
            total_failures = excluded.total_failures,
            last_failure_source = excluded.last_failure_source,
            first_failed_at_ms = excluded.first_failed_at_ms,
            last_failed_at_ms = excluded.last_failed_at_ms,
            next_retry_at_ms = excluded.next_retry_at_ms,
            escalated_at_ms = excluded.escalated_at_ms,
            resolved_at_ms = excluded.resolved_at_ms,
            updated_at_ms = excluded.updated_at_ms",
        params![
            record.plan_id.as_bytes().as_slice(),
            record.state.code(),
            encode_u64(record.consecutive_failures).as_slice(),
            encode_u64(record.total_failures).as_slice(),
            record.last_source.code(),
            record.first_failed_at_ms,
            record.last_failed_at_ms,
            record.next_retry_at_ms,
            record.escalated_at_ms,
            record.resolved_at_ms,
            record.updated_at_ms,
        ],
    )?;
    Ok(())
}

fn load_optional(
    reader: &impl SqlRead,
    plan_id: ArtifactCommitPlanId,
) -> Result<Option<ArtifactRecoveryRecord>, TaskStoreError> {
    let mut statement = reader.prepare_statement(
        "SELECT recovery_state, consecutive_failures, total_failures,
         last_failure_source, first_failed_at_ms, last_failed_at_ms,
         next_retry_at_ms, escalated_at_ms, resolved_at_ms, updated_at_ms
         FROM task_artifact_recovery WHERE plan_id = ?1",
    )?;
    let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(ArtifactRecoveryRecord {
        plan_id,
        state: ArtifactRecoveryState::from_code(row.get(0)?)?,
        consecutive_failures: u64_from_blob(row, 1)?,
        total_failures: u64_from_blob(row, 2)?,
        last_source: ArtifactRecoveryFailureSource::from_code(row.get(3)?)?,
        first_failed_at_ms: row.get(4)?,
        last_failed_at_ms: row.get(5)?,
        next_retry_at_ms: row.get(6)?,
        escalated_at_ms: row.get(7)?,
        resolved_at_ms: row.get(8)?,
        updated_at_ms: row.get(9)?,
    }))
}

fn semantic_validate_request(
    request: SemanticRecoveryFailureRequest,
) -> Result<(), TaskStoreError> {
    let reason = if request.observed_at_ms < 0 {
        Some("failure timestamp must be non-negative")
    } else if request.base_delay_ms == 0 {
        Some("base delay must be non-zero")
    } else if request.max_delay_ms < request.base_delay_ms {
        Some("maximum delay must be at least base delay")
    } else {
        None
    };
    reason.map_or(Ok(()), |reason| {
        Err(TaskStoreError::InvalidSemanticRecoveryPolicy { reason })
    })
}

/// Exponential capped backoff: the Artifact ledger's capped exponential
/// term (`base * 2^(consecutive - 1)`, saturating at `max_delay_ms`) with
/// the per-plan jitter factor omitted, so the Semantic retry due time is a
/// pure function of the request and durable count.
fn capped_exponential_delay(request: &SemanticRecoveryFailureRequest, consecutive: u64) -> u64 {
    let exponent = u32::try_from(consecutive.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(63);
    request
        .base_delay_ms
        .checked_mul(1_u64 << exponent)
        .unwrap_or(request.max_delay_ms)
        .min(request.max_delay_ms)
}

fn semantic_upsert(
    transaction: &rusqlite::Transaction<'_>,
    record: &SemanticRecoveryRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_semantic_recovery (
            plan_id, recovery_state, consecutive_failures, total_failures,
            last_failure_source, first_failed_at_ms, last_failed_at_ms,
            next_retry_at_ms, escalated_at_ms, resolved_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(plan_id) DO UPDATE SET
            recovery_state = excluded.recovery_state,
            consecutive_failures = excluded.consecutive_failures,
            total_failures = excluded.total_failures,
            last_failure_source = excluded.last_failure_source,
            first_failed_at_ms = excluded.first_failed_at_ms,
            last_failed_at_ms = excluded.last_failed_at_ms,
            next_retry_at_ms = excluded.next_retry_at_ms,
            escalated_at_ms = excluded.escalated_at_ms,
            resolved_at_ms = excluded.resolved_at_ms,
            updated_at_ms = excluded.updated_at_ms",
        params![
            record.plan_id.as_bytes().as_slice(),
            record.state.code(),
            encode_u64(record.consecutive_failures).as_slice(),
            encode_u64(record.total_failures).as_slice(),
            record.last_source.code(),
            record.first_failed_at_ms,
            record.last_failed_at_ms,
            record.next_retry_at_ms,
            record.escalated_at_ms,
            record.resolved_at_ms,
            record.updated_at_ms,
        ],
    )?;
    Ok(())
}

fn semantic_load_optional(
    reader: &impl SqlRead,
    plan_id: SemanticCommitPlanId,
) -> Result<Option<SemanticRecoveryRecord>, TaskStoreError> {
    let mut statement = reader.prepare_statement(
        "SELECT recovery_state, consecutive_failures, total_failures,
         last_failure_source, first_failed_at_ms, last_failed_at_ms,
         next_retry_at_ms, escalated_at_ms, resolved_at_ms, updated_at_ms
         FROM task_semantic_recovery WHERE plan_id = ?1",
    )?;
    let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(SemanticRecoveryRecord {
        plan_id,
        state: SemanticRecoveryState::from_code(row.get(0)?)?,
        consecutive_failures: u64_from_blob(row, 1)?,
        total_failures: u64_from_blob(row, 2)?,
        last_source: SemanticRecoveryFailureSource::from_code(row.get(3)?)?,
        first_failed_at_ms: row.get(4)?,
        last_failed_at_ms: row.get(5)?,
        next_retry_at_ms: row.get(6)?,
        escalated_at_ms: row.get(7)?,
        resolved_at_ms: row.get(8)?,
        updated_at_ms: row.get(9)?,
    }))
}

pub(crate) const SCHEMA_V8_SQL: &str = "CREATE TABLE task_artifact_recovery (
        plan_id BLOB PRIMARY KEY NOT NULL CHECK(length(plan_id) = 16),
        recovery_state INTEGER NOT NULL CHECK(recovery_state IN (0, 1, 2)),
        consecutive_failures BLOB NOT NULL CHECK(length(consecutive_failures) = 8),
        total_failures BLOB NOT NULL CHECK(length(total_failures) = 8),
        last_failure_source INTEGER NOT NULL CHECK(last_failure_source IN (0, 1, 2)),
        first_failed_at_ms INTEGER NOT NULL CHECK(first_failed_at_ms >= 0),
        last_failed_at_ms INTEGER NOT NULL CHECK(last_failed_at_ms >= first_failed_at_ms),
        next_retry_at_ms INTEGER,
        escalated_at_ms INTEGER,
        resolved_at_ms INTEGER,
        updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
        FOREIGN KEY(plan_id) REFERENCES task_artifact_commit_plans(plan_id),
        CHECK(total_failures >= consecutive_failures),
        CHECK((recovery_state = 0) = (next_retry_at_ms IS NOT NULL)),
        CHECK((recovery_state = 1) = (escalated_at_ms IS NOT NULL)),
        CHECK((recovery_state = 2) = (resolved_at_ms IS NOT NULL))
     ) STRICT;

     CREATE INDEX task_artifact_recovery_due
        ON task_artifact_recovery(recovery_state, next_retry_at_ms, plan_id);

     PRAGMA user_version = 8;";

pub(crate) const SCHEMA_V9_SQL: &str = "CREATE TABLE task_artifact_recovery_alert_receipts (
        receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
        plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
        total_failures BLOB NOT NULL CHECK(length(total_failures) = 8),
        principal_id BLOB NOT NULL CHECK(length(principal_id) = 16),
        idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
        acknowledged_at_ms INTEGER NOT NULL CHECK(acknowledged_at_ms >= 0),
        FOREIGN KEY(plan_id) REFERENCES task_artifact_recovery(plan_id),
        UNIQUE(plan_id, total_failures)
     ) STRICT;

     CREATE TRIGGER task_artifact_recovery_alert_receipts_immutable_update
     BEFORE UPDATE ON task_artifact_recovery_alert_receipts
     BEGIN
        SELECT RAISE(ABORT, 'Artifact recovery alert receipts are immutable');
     END;

     CREATE TRIGGER task_artifact_recovery_alert_receipts_immutable_delete
     BEFORE DELETE ON task_artifact_recovery_alert_receipts
     BEGIN
        SELECT RAISE(ABORT, 'Artifact recovery alert receipts are immutable');
     END;

     PRAGMA user_version = 9;";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_recovery_resume_reference_pins_the_domain_separated_formula() {
        let plan_id = SemanticCommitPlanId::from_bytes([0x71; 16]);

        let mut hasher = Sha256::new();
        hasher.update(b"llmos/task-semantic-recovery-resume/v1\0");
        hasher.update(plan_id.as_bytes());
        hasher.update(8_u64.to_be_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        let mut expected = [0_u8; 16];
        expected.copy_from_slice(&digest[..16]);

        let reference = semantic_recovery_resume_reference(plan_id, 8);
        assert_eq!(reference.as_bytes(), &expected);
        assert_eq!(
            semantic_recovery_resume_reference(plan_id, 8),
            reference,
            "idempotent replays of one resume command name the same reference"
        );
        assert_ne!(
            semantic_recovery_resume_reference(plan_id, 9),
            reference,
            "each CAS revision names a distinct reference"
        );
        assert_ne!(
            semantic_recovery_resume_reference(SemanticCommitPlanId::from_bytes([0x72; 16]), 8),
            reference,
            "each plan names a distinct reference"
        );
        assert_ne!(
            semantic_recovery_resume_reference(plan_id, 8),
            derive_semantic_alert_receipt_id(plan_id, 8),
            "resume references never collide with acknowledgement receipts"
        );
    }
}
