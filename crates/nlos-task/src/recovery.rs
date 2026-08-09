//! Durable retry and escalation ledger for Artifact commit recovery.

use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::commit::{ArtifactCommitPlanId, ArtifactCommitPlanRecord, ArtifactCommitPlanState};
use crate::store::{SqlRead, SqliteTaskAuthority, encode_u64, u64_from_blob};
use crate::{TaskStoreError, commit};

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
