//! Artifact publication planning for the recoverable Task commit protocol.
//!
//! Schema v6 deliberately lands the durable, immutable plan before any
//! publication authorization. A plan binds the current `CommitPermit` and
//! its artifact-only `write_set_root` to a canonical set of staged Artifact
//! expectations. Later slices advance the state only after full finalize
//! readiness validation and consume Artifact publication receipts.

use std::fmt;

use nlos_types::{ArtifactId, CommitPermitId, IdempotencyKey, TaskAttemptId, TaskId};
use rusqlite::{Row, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::store::{
    SqlRead, SqliteTaskAuthority, encode_u64, load_attempt, load_permit_by_id, load_task,
};
use crate::{PermitState, TaskStoreError};

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

/// Lifecycle reserved by schema v6. Only `Planned` is currently
/// producible; later transitions cannot reinterpret the immutable plan.
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
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Idempotent plan decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactCommitPlanDecision {
    Planned(Box<ArtifactCommitPlanRecord>),
    Replayed(Box<ArtifactCommitPlanRecord>),
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
     expected_artifact_count, plan_state, created_at_ms, updated_at_ms";

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
        created_at_ms: row.get(9)?,
        updated_at_ms: row.get(10)?,
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
