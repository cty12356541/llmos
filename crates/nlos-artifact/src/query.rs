//! Read paths and durable-row decoders: `get_revision`, `resolve_head`,
//! and the `inspect_*` APIs.

use rusqlite::{Connection, Row, Transaction, params};

use nlos_types::{ApplicationId, ArtifactId};

use crate::ArtifactError;
use crate::blob;
use crate::model::{ArtifactRecord, ContentDigest, HeadState, RevisionRecord};
use crate::store::{ArtifactStore, encode_u64};

/// Minimal read abstraction shared by plain connections and transactions
/// (same pattern as `nlos-store`).
pub(crate) trait SqlRead {
    fn prepare_statement(&self, sql: &str) -> Result<rusqlite::Statement<'_>, rusqlite::Error>;
}

impl SqlRead for Connection {
    fn prepare_statement(&self, sql: &str) -> Result<rusqlite::Statement<'_>, rusqlite::Error> {
        self.prepare(sql)
    }
}

impl SqlRead for Transaction<'_> {
    fn prepare_statement(&self, sql: &str) -> Result<rusqlite::Statement<'_>, rusqlite::Error> {
        self.prepare(sql)
    }
}

impl ArtifactStore {
    /// Reads one revision's bytes, re-verifying the digest against the
    /// content address. A torn or corrupted blob surfaces as
    /// [`ArtifactError::DigestMismatch`]; a missing blob as
    /// [`ArtifactError::BlobMissing`] (run
    /// [`ArtifactStore::recover`](crate::ArtifactStore::recover) to
    /// reconcile). Wrong bytes are never returned silently.
    ///
    /// `now_ms` is the caller-supplied observation time (crate-wide
    /// time-source discipline): an artifact past its retention time upper
    /// bound at `now_ms` fails closed with
    /// [`ArtifactError::RetentionExpired`] before any bytes are read —
    /// expired and not-found stay distinct typed states, and no data is
    /// deleted by expiry.
    ///
    /// # Errors
    ///
    /// Returns a not-found, retention-expired, missing-blob,
    /// digest-mismatch, or storage error.
    pub fn get_revision(
        &self,
        artifact_id: ArtifactId,
        revision: u64,
        now_ms: u64,
    ) -> Result<Vec<u8>, ArtifactError> {
        let connection = self.lock_connection()?;
        let artifact = load_artifact_optional(&*connection, artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(artifact_id))?;
        // Fail-closed retention gate: past the artifact's time upper bound
        // the bytes are refused before any revision state is touched.
        crate::retention::ensure_readable(&artifact, now_ms)?;
        let record = load_revision_optional(&*connection, artifact_id, revision)?.ok_or(
            ArtifactError::RevisionNotFound {
                artifact_id,
                revision,
            },
        )?;
        crate::provenance::ensure_provenance_recorded(&*connection, artifact_id, revision)?;
        let path = self.paths().artifacts.blob_path(record.digest);
        blob::read_blob_verified(&self.paths().artifacts, record.digest)?.ok_or(
            ArtifactError::BlobMissing {
                artifact_id,
                revision,
                digest: record.digest,
                path,
            },
        )
    }

    /// Resolves the mutable head pointer. Returns `Ok(None)` when the
    /// artifact exists but has no revisions yet.
    ///
    /// This is the poll surface: an artifact past its retention time upper
    /// bound at `now_ms` fails closed with
    /// [`ArtifactError::RetentionExpired`] instead of reporting head state
    /// (even an empty head), so polling can never observe a live pointer
    /// into expired data.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::ArtifactNotFound`],
    /// [`ArtifactError::RetentionExpired`], or a storage error.
    pub fn resolve_head(
        &self,
        artifact_id: ArtifactId,
        now_ms: u64,
    ) -> Result<Option<HeadState>, ArtifactError> {
        let connection = self.lock_connection()?;
        let record = load_artifact_optional(&*connection, artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(artifact_id))?;
        crate::retention::ensure_readable(&record, now_ms)?;
        match (record.head_revision, record.head_digest) {
            (0, None) => Ok(None),
            (revision, Some(digest)) => Ok(Some(HeadState { revision, digest })),
            _ => Err(ArtifactError::CorruptRecord(
                "head revision and head digest disagree",
            )),
        }
    }

    /// Reads the durable metadata row of one artifact.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::ArtifactNotFound`] or a storage error.
    pub fn inspect_artifact(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<ArtifactRecord, ArtifactError> {
        let connection = self.lock_connection()?;
        load_artifact_optional(&*connection, artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(artifact_id))
    }

    /// Reads the durable metadata row of one revision.
    ///
    /// # Errors
    ///
    /// Returns a not-found or storage error.
    pub fn inspect_revision(
        &self,
        artifact_id: ArtifactId,
        revision: u64,
    ) -> Result<RevisionRecord, ArtifactError> {
        let connection = self.lock_connection()?;
        load_artifact_optional(&*connection, artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(artifact_id))?;
        load_revision_optional(&*connection, artifact_id, revision)?.ok_or(
            ArtifactError::RevisionNotFound {
                artifact_id,
                revision,
            },
        )
    }

    /// Lists all immutable revision rows of one artifact, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::ArtifactNotFound`] or a storage error.
    pub fn list_revisions(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<Vec<RevisionRecord>, ArtifactError> {
        let connection = self.lock_connection()?;
        load_artifact_optional(&*connection, artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(artifact_id))?;
        let mut statement = connection.prepare(
            "SELECT artifact_id, revision, digest, size_bytes, created_at_ms
             FROM artifact_revisions WHERE artifact_id = ?1 ORDER BY revision",
        )?;
        let mut rows = statement.query([artifact_id.as_bytes().as_slice()])?;
        let mut revisions = Vec::new();
        while let Some(row) = rows.next()? {
            revisions.push(decode_revision_row(row)?);
        }
        Ok(revisions)
    }
}

pub(crate) fn load_artifact_optional(
    source: &impl SqlRead,
    artifact_id: ArtifactId,
) -> Result<Option<ArtifactRecord>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT artifact_id, content_type, application_id, owner,
                head_revision, head_digest, created_at_ms, retention_ms
         FROM artifacts WHERE artifact_id = ?1",
    )?;
    let mut rows = statement.query([artifact_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_artifact_row).transpose()
}

pub(crate) fn load_artifact_by_key(
    source: &impl SqlRead,
    idempotency_key: nlos_types::IdempotencyKey,
) -> Result<Option<ArtifactRecord>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT artifact_id, content_type, application_id, owner,
                head_revision, head_digest, created_at_ms, retention_ms
         FROM artifacts WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([idempotency_key.as_bytes().as_slice()])?;
    rows.next()?.map(decode_artifact_row).transpose()
}

pub(crate) fn insert_artifact(
    transaction: &Transaction<'_>,
    record: &ArtifactRecord,
    idempotency_key: nlos_types::IdempotencyKey,
) -> Result<(), ArtifactError> {
    transaction.execute(
        "INSERT INTO artifacts (
            artifact_id, idempotency_key, content_type, application_id,
            owner, head_revision, head_digest, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6)",
        params![
            record.artifact_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            record.content_type,
            record
                .application_id
                .map(ApplicationId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            record.owner,
            encode_u64(record.created_at_ms)?,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_revision_optional(
    source: &impl SqlRead,
    artifact_id: ArtifactId,
    revision: u64,
) -> Result<Option<RevisionRecord>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT artifact_id, revision, digest, size_bytes, created_at_ms
         FROM artifact_revisions WHERE artifact_id = ?1 AND revision = ?2",
    )?;
    let mut rows = statement.query(params![
        artifact_id.as_bytes().as_slice(),
        encode_u64(revision)?,
    ])?;
    rows.next()?.map(decode_revision_row).transpose()
}

/// Every committed revision's `(artifact, revision, digest)`, for recovery.
pub(crate) fn load_all_revision_digests(
    source: &impl SqlRead,
) -> Result<Vec<(ArtifactId, u64, ContentDigest)>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT artifact_id, revision, digest FROM artifact_revisions ORDER BY artifact_id, revision",
    )?;
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((
            ArtifactId::from_bytes(blob16(row, 0)?),
            decode_u64(row, 1)?,
            ContentDigest::from_bytes(blob32(row, 2)?),
        ));
    }
    Ok(out)
}

pub(crate) fn decode_artifact_row(row: &Row<'_>) -> Result<ArtifactRecord, ArtifactError> {
    let artifact_id = ArtifactId::from_bytes(blob16(row, 0)?);
    let content_type: String = row.get(1)?;
    let application_id = optional_blob16(row, 2)?.map(ApplicationId::from_bytes);
    let owner: Option<String> = row.get(3)?;
    let head_revision = decode_u64(row, 4)?;
    let head_digest = optional_blob32(row, 5)?.map(ContentDigest::from_bytes);
    let created_at_ms = decode_u64(row, 6)?;
    let retention_ms = optional_u64(row, 7)?;
    if (head_revision == 0) != head_digest.is_none() {
        return Err(ArtifactError::CorruptRecord(
            "head revision and head digest disagree",
        ));
    }
    Ok(ArtifactRecord {
        artifact_id,
        content_type,
        application_id,
        owner,
        head_revision,
        head_digest,
        created_at_ms,
        retention_ms,
    })
}

pub(crate) fn decode_revision_row(row: &Row<'_>) -> Result<RevisionRecord, ArtifactError> {
    Ok(RevisionRecord {
        artifact_id: ArtifactId::from_bytes(blob16(row, 0)?),
        revision: decode_u64(row, 1)?,
        digest: ContentDigest::from_bytes(blob32(row, 2)?),
        size_bytes: decode_u64(row, 3)?,
        created_at_ms: decode_u64(row, 4)?,
    })
}

fn decode_u64(row: &Row<'_>, index: usize) -> Result<u64, ArtifactError> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| ArtifactError::CorruptRecord("negative u64 column"))
}

fn optional_u64(row: &Row<'_>, index: usize) -> Result<Option<u64>, ArtifactError> {
    let value: Option<i64> = row.get(index)?;
    value
        .map(|value| {
            u64::try_from(value).map_err(|_| ArtifactError::CorruptRecord("negative u64 column"))
        })
        .transpose()
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
