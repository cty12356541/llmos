//! Durable staging and ArtifactAuthority-local atomic publication.

use nlos_types::{ArtifactId, CommitPermitId, IdempotencyKey, ReceiptId, TaskId};
use rusqlite::{Row, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::ArtifactError;
use crate::blob;
use crate::model::{
    ArtifactPublicationReceipt, ContentDigest, PublishStagedRevisionDecision,
    PublishStagedRevisionRequest, RevisionRecord, StageRevisionDecision, StageRevisionRequest,
    StagedRevisionRecord, StagedRevisionState, StagingId,
};
use crate::query::{SqlRead, load_artifact_optional, load_revision_optional};
use crate::store::{ArtifactStore, encode_u64};

impl ArtifactStore {
    /// Durably stages bytes without creating a revision or advancing head.
    ///
    /// # Errors
    ///
    /// Returns a typed validation, idempotency, head, blob, or storage error.
    pub fn stage_revision(
        &self,
        request: StageRevisionRequest<'_>,
    ) -> Result<StageRevisionDecision, ArtifactError> {
        let digest = ContentDigest::of_bytes(request.bytes);
        blob::commit_blob(&self.paths().artifacts, digest, request.bytes)?;

        let staging_id = staging_id_for(request.artifact_id, request.idempotency_key);
        let size_bytes = u64::try_from(request.bytes.len())
            .map_err(|_| ArtifactError::InvalidSpec("blob length exceeds u64"))?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = load_staged_by_key(&transaction, request.idempotency_key)? {
            let same = existing.staging_id == staging_id
                && existing.artifact_id == request.artifact_id
                && existing.expected_head_revision == request.expected_head_revision
                && existing.digest == digest
                && existing.size_bytes == size_bytes
                && existing.task_id == request.task_id
                && existing.permit_id == request.permit_id
                && existing.write_set_root == request.write_set_root;
            if !same {
                return Err(ArtifactError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(StageRevisionDecision::Replayed(existing));
        }
        if load_staged_optional(&transaction, staging_id)?.is_some() {
            return Err(ArtifactError::IdempotencyConflict);
        }

        let artifact = load_artifact_optional(&transaction, request.artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(request.artifact_id))?;
        // Admission gate after the replay branches: fresh staging into an
        // artifact past its retention bound is refused; replays of durable
        // staged state above stay un-gated.
        crate::retention::ensure_readable(&artifact, request.created_at_ms)?;
        if artifact.head_revision != request.expected_head_revision {
            return Err(ArtifactError::HeadConflict {
                expected: request.expected_head_revision,
                current: artifact.head_revision,
            });
        }
        let target_revision =
            request
                .expected_head_revision
                .checked_add(1)
                .ok_or(ArtifactError::HeadConflict {
                    expected: request.expected_head_revision,
                    current: artifact.head_revision,
                })?;
        let record = StagedRevisionRecord {
            staging_id,
            artifact_id: request.artifact_id,
            expected_head_revision: request.expected_head_revision,
            target_revision,
            digest,
            size_bytes,
            task_id: request.task_id,
            permit_id: request.permit_id,
            write_set_root: request.write_set_root,
            state: StagedRevisionState::Staged,
            publication_receipt_id: None,
            created_at_ms: request.created_at_ms,
            updated_at_ms: request.created_at_ms,
        };
        transaction.execute(
            "INSERT INTO artifact_staged_revisions (
                staging_id, idempotency_key, artifact_id, expected_head_revision,
                target_revision, digest, size_bytes, task_id, permit_id,
                write_set_root, stage_state, publication_receipt_id,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, NULL, ?11, ?11)",
            params![
                record.staging_id.as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice(),
                record.artifact_id.as_bytes().as_slice(),
                encode_u64(record.expected_head_revision)?,
                encode_u64(record.target_revision)?,
                record.digest.as_bytes().as_slice(),
                encode_u64(record.size_bytes)?,
                record.task_id.as_bytes().as_slice(),
                record.permit_id.as_bytes().as_slice(),
                record.write_set_root.as_bytes().as_slice(),
                encode_u64(record.created_at_ms)?,
            ],
        )?;
        transaction.commit()?;
        Ok(StageRevisionDecision::Staged(record))
    }

    /// Publishes a staged revision with `ArtifactAuthority`-local atomicity.
    ///
    /// # Errors
    ///
    /// Returns a typed not-found, binding, blob, head, revision, or storage
    /// error. No publication metadata commits on failure.
    pub fn publish_staged_revision(
        &self,
        request: PublishStagedRevisionRequest,
    ) -> Result<PublishStagedRevisionDecision, ArtifactError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let staged = load_staged_optional(&transaction, request.staging_id)?
            .ok_or(ArtifactError::StagedRevisionNotFound(request.staging_id))?;
        if staged.task_id != request.task_id
            || staged.permit_id != request.permit_id
            || staged.write_set_root != request.write_set_root
        {
            return Err(ArtifactError::PublicationBindingMismatch);
        }
        if staged.state == StagedRevisionState::Published {
            let receipt_id = staged
                .publication_receipt_id
                .ok_or(ArtifactError::CorruptRecord(
                    "published stage lacks publication receipt id",
                ))?;
            let receipt = load_receipt_optional(&transaction, receipt_id)?.ok_or(
                ArtifactError::CorruptRecord("published stage lacks publication receipt"),
            )?;
            transaction.commit()?;
            return Ok(PublishStagedRevisionDecision::Replayed(receipt));
        }

        let path = self.paths().artifacts.blob_path(staged.digest);
        let bytes = blob::read_blob_verified(&self.paths().artifacts, staged.digest)?.ok_or(
            ArtifactError::StagedBlobMissing {
                staging_id: staged.staging_id,
                digest: staged.digest,
                path,
            },
        )?;
        if u64::try_from(bytes.len()).ok() != Some(staged.size_bytes) {
            return Err(ArtifactError::CorruptRecord(
                "staged blob size disagrees with metadata",
            ));
        }

        let artifact = load_artifact_optional(&transaction, staged.artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(staged.artifact_id))?;
        // Admission gate on the fresh-publication path only (the
        // already-published replay above returned earlier): publishing
        // staged bytes into an artifact past its retention bound would
        // create a revision no one can read.
        crate::retention::ensure_readable(&artifact, request.published_at_ms)?;
        if artifact.head_revision != staged.expected_head_revision {
            return Err(ArtifactError::HeadConflict {
                expected: staged.expected_head_revision,
                current: artifact.head_revision,
            });
        }
        if load_revision_optional(&transaction, staged.artifact_id, staged.target_revision)?
            .is_some()
        {
            return Err(ArtifactError::RevisionConflict {
                artifact_id: staged.artifact_id,
                revision: staged.target_revision,
            });
        }

        let receipt = commit_publication(
            &transaction,
            &staged,
            artifact.head_digest,
            request.published_at_ms,
        )?;
        transaction.commit()?;
        Ok(PublishStagedRevisionDecision::Published(receipt))
    }

    /// Reads one staged revision record.
    ///
    /// # Errors
    ///
    /// Returns a typed not-found, corrupt-record, or storage error.
    pub fn inspect_staged_revision(
        &self,
        staging_id: StagingId,
    ) -> Result<StagedRevisionRecord, ArtifactError> {
        let connection = self.lock_connection()?;
        load_staged_optional(&*connection, staging_id)?
            .ok_or(ArtifactError::StagedRevisionNotFound(staging_id))
    }

    /// Reads one immutable publication receipt.
    ///
    /// # Errors
    ///
    /// Returns a typed not-found, corrupt-record, or storage error.
    pub fn inspect_publication_receipt(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<ArtifactPublicationReceipt, ArtifactError> {
        let connection = self.lock_connection()?;
        load_receipt_optional(&*connection, receipt_id)?
            .ok_or(ArtifactError::PublicationReceiptNotFound(receipt_id))
    }
}

fn commit_publication(
    transaction: &Transaction<'_>,
    staged: &StagedRevisionRecord,
    prior_head_digest: Option<ContentDigest>,
    published_at_ms: u64,
) -> Result<ArtifactPublicationReceipt, ArtifactError> {
    let revision = RevisionRecord {
        artifact_id: staged.artifact_id,
        revision: staged.target_revision,
        digest: staged.digest,
        size_bytes: staged.size_bytes,
        created_at_ms: published_at_ms,
    };
    transaction.execute(
        "INSERT INTO artifact_revisions (
            artifact_id, revision, digest, size_bytes, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            revision.artifact_id.as_bytes().as_slice(),
            encode_u64(revision.revision)?,
            revision.digest.as_bytes().as_slice(),
            encode_u64(revision.size_bytes)?,
            encode_u64(revision.created_at_ms)?,
        ],
    )?;
    let changed = transaction.execute(
        "UPDATE artifacts SET head_revision = ?1, head_digest = ?2
         WHERE artifact_id = ?3 AND head_revision = ?4",
        params![
            encode_u64(revision.revision)?,
            revision.digest.as_bytes().as_slice(),
            revision.artifact_id.as_bytes().as_slice(),
            encode_u64(staged.expected_head_revision)?,
        ],
    )?;
    if changed != 1 {
        return Err(ArtifactError::CorruptRecord(
            "head compare-and-swap failed under BEGIN IMMEDIATE",
        ));
    }

    let receipt = ArtifactPublicationReceipt {
        receipt_id: derive_receipt_id(staged.staging_id),
        staging_id: staged.staging_id,
        artifact_id: staged.artifact_id,
        revision: staged.target_revision,
        digest: staged.digest,
        size_bytes: staged.size_bytes,
        task_id: staged.task_id,
        permit_id: staged.permit_id,
        write_set_root: staged.write_set_root,
        prior_head_revision: staged.expected_head_revision,
        prior_head_digest,
        new_head_revision: staged.target_revision,
        new_head_digest: staged.digest,
        created_at_ms: published_at_ms,
    };
    insert_receipt(transaction, &receipt)?;
    crate::provenance::insert_owner_derived_provenance(transaction, &receipt)?;
    let changed = transaction.execute(
        "UPDATE artifact_staged_revisions
         SET stage_state = 1, publication_receipt_id = ?1, updated_at_ms = ?2
         WHERE staging_id = ?3 AND stage_state = 0",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            encode_u64(published_at_ms)?,
            staged.staging_id.as_bytes().as_slice(),
        ],
    )?;
    if changed != 1 {
        return Err(ArtifactError::CorruptRecord(
            "staged revision state transition lost under BEGIN IMMEDIATE",
        ));
    }
    Ok(receipt)
}

/// Deterministically derives the authority staging identity before bytes
/// are staged, allowing a Task write-set plan to bind the future stage.
#[must_use]
pub fn staging_id_for(artifact_id: ArtifactId, key: IdempotencyKey) -> StagingId {
    derive_id(
        b"llmos/artifact-staging/v1",
        artifact_id.as_bytes(),
        key.as_bytes(),
    )
    .into()
}

fn derive_receipt_id(staging_id: StagingId) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/artifact-publication-receipt/v1");
    hasher.update(staging_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(bytes)
}

fn derive_id(domain: &[u8], left: &[u8], right: &[u8]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(left);
    hasher.update(right);
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes
}

impl From<[u8; 16]> for StagingId {
    fn from(bytes: [u8; 16]) -> Self {
        Self::from_bytes(bytes)
    }
}

pub(crate) fn load_staged_optional(
    source: &impl SqlRead,
    staging_id: StagingId,
) -> Result<Option<StagedRevisionRecord>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT staging_id, artifact_id, expected_head_revision, target_revision,
                digest, size_bytes, task_id, permit_id, write_set_root,
                stage_state, publication_receipt_id, created_at_ms, updated_at_ms
         FROM artifact_staged_revisions WHERE staging_id = ?1",
    )?;
    let mut rows = statement.query([staging_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_staged_row).transpose()
}

fn load_staged_by_key(
    source: &impl SqlRead,
    key: IdempotencyKey,
) -> Result<Option<StagedRevisionRecord>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT staging_id, artifact_id, expected_head_revision, target_revision,
                digest, size_bytes, task_id, permit_id, write_set_root,
                stage_state, publication_receipt_id, created_at_ms, updated_at_ms
         FROM artifact_staged_revisions WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([key.as_bytes().as_slice()])?;
    rows.next()?.map(decode_staged_row).transpose()
}

pub(crate) fn load_all_staged_digests(
    source: &impl SqlRead,
) -> Result<Vec<(StagingId, ArtifactId, u64, ContentDigest)>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT staging_id, artifact_id, target_revision, digest
         FROM artifact_staged_revisions WHERE stage_state = 0 ORDER BY staging_id",
    )?;
    let mut rows = statement.query([])?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        records.push((
            StagingId::from_bytes(blob16(row, 0)?),
            ArtifactId::from_bytes(blob16(row, 1)?),
            decode_u64(row, 2)?,
            ContentDigest::from_bytes(blob32(row, 3)?),
        ));
    }
    Ok(records)
}

pub(crate) fn load_receipt_optional(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<Option<ArtifactPublicationReceipt>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, staging_id, artifact_id, revision, digest, size_bytes,
                task_id, permit_id, write_set_root, prior_head_revision,
                prior_head_digest, new_head_revision, new_head_digest, created_at_ms
         FROM artifact_publication_receipts WHERE receipt_id = ?1",
    )?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn insert_receipt(
    transaction: &rusqlite::Transaction<'_>,
    receipt: &ArtifactPublicationReceipt,
) -> Result<(), ArtifactError> {
    transaction.execute(
        "INSERT INTO artifact_publication_receipts (
            receipt_id, staging_id, artifact_id, revision, digest, size_bytes,
            task_id, permit_id, write_set_root, prior_head_revision,
            prior_head_digest, new_head_revision, new_head_digest, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.staging_id.as_bytes().as_slice(),
            receipt.artifact_id.as_bytes().as_slice(),
            encode_u64(receipt.revision)?,
            receipt.digest.as_bytes().as_slice(),
            encode_u64(receipt.size_bytes)?,
            receipt.task_id.as_bytes().as_slice(),
            receipt.permit_id.as_bytes().as_slice(),
            receipt.write_set_root.as_bytes().as_slice(),
            encode_u64(receipt.prior_head_revision)?,
            receipt.prior_head_digest.map(ContentDigest::into_bytes),
            encode_u64(receipt.new_head_revision)?,
            receipt.new_head_digest.as_bytes().as_slice(),
            encode_u64(receipt.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn decode_staged_row(row: &Row<'_>) -> Result<StagedRevisionRecord, ArtifactError> {
    let state_value: i64 = row.get(9)?;
    let state = match state_value {
        0 => StagedRevisionState::Staged,
        1 => StagedRevisionState::Published,
        _ => {
            return Err(ArtifactError::CorruptRecord(
                "unknown staged revision state",
            ));
        }
    };
    let publication_receipt_id = optional_blob16(row, 10)?.map(ReceiptId::from_bytes);
    if (state == StagedRevisionState::Staged) != publication_receipt_id.is_none() {
        return Err(ArtifactError::CorruptRecord(
            "staged revision state and receipt id disagree",
        ));
    }
    Ok(StagedRevisionRecord {
        staging_id: StagingId::from_bytes(blob16(row, 0)?),
        artifact_id: ArtifactId::from_bytes(blob16(row, 1)?),
        expected_head_revision: decode_u64(row, 2)?,
        target_revision: decode_u64(row, 3)?,
        digest: ContentDigest::from_bytes(blob32(row, 4)?),
        size_bytes: decode_u64(row, 5)?,
        task_id: TaskId::from_bytes(blob16(row, 6)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 7)?),
        write_set_root: ContentDigest::from_bytes(blob32(row, 8)?),
        state,
        publication_receipt_id,
        created_at_ms: decode_u64(row, 11)?,
        updated_at_ms: decode_u64(row, 12)?,
    })
}

fn decode_receipt_row(row: &Row<'_>) -> Result<ArtifactPublicationReceipt, ArtifactError> {
    Ok(ArtifactPublicationReceipt {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        staging_id: StagingId::from_bytes(blob16(row, 1)?),
        artifact_id: ArtifactId::from_bytes(blob16(row, 2)?),
        revision: decode_u64(row, 3)?,
        digest: ContentDigest::from_bytes(blob32(row, 4)?),
        size_bytes: decode_u64(row, 5)?,
        task_id: TaskId::from_bytes(blob16(row, 6)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 7)?),
        write_set_root: ContentDigest::from_bytes(blob32(row, 8)?),
        prior_head_revision: decode_u64(row, 9)?,
        prior_head_digest: optional_blob32(row, 10)?.map(ContentDigest::from_bytes),
        new_head_revision: decode_u64(row, 11)?,
        new_head_digest: ContentDigest::from_bytes(blob32(row, 12)?),
        created_at_ms: decode_u64(row, 13)?,
    })
}

fn decode_u64(row: &Row<'_>, index: usize) -> Result<u64, ArtifactError> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| ArtifactError::CorruptRecord("negative u64 column"))
}

fn blob16(row: &Row<'_>, index: usize) -> Result<[u8; 16], ArtifactError> {
    blob_n(row, index)
}

fn blob32(row: &Row<'_>, index: usize) -> Result<[u8; 32], ArtifactError> {
    blob_n(row, index)
}

fn optional_blob16(row: &Row<'_>, index: usize) -> Result<Option<[u8; 16]>, ArtifactError> {
    optional_blob_n(row, index)
}

fn optional_blob32(row: &Row<'_>, index: usize) -> Result<Option<[u8; 32]>, ArtifactError> {
    optional_blob_n(row, index)
}

fn blob_n<const N: usize>(row: &Row<'_>, index: usize) -> Result<[u8; N], ArtifactError> {
    let bytes: Vec<u8> = row.get(index)?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| ArtifactError::CorruptRecord("blob column length mismatch"))
}

fn optional_blob_n<const N: usize>(
    row: &Row<'_>,
    index: usize,
) -> Result<Option<[u8; N]>, ArtifactError> {
    let bytes: Option<Vec<u8>> = row.get(index)?;
    bytes
        .map(|value| {
            <[u8; N]>::try_from(value.as_slice())
                .map_err(|_| ArtifactError::CorruptRecord("blob column length mismatch"))
        })
        .transpose()
}
