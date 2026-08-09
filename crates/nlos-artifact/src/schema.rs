//! Schema DDL. Kept separate so the durable format is auditable in one
//! place; any future migration gets its own function and `user_version`.

use nlos_types::{ArtifactId, Generation, ReceiptId, TaskParticipantId};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::{ArtifactError, ArtifactHeadEndpointProof};

/// Creates the v1 schema in one transaction: artifact metadata, immutable
/// revisions (enforced by triggers), and the best-effort cache table.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), ArtifactError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE artifacts (
            artifact_id BLOB PRIMARY KEY NOT NULL CHECK(length(artifact_id) = 16),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
            content_type TEXT NOT NULL
                CHECK(length(content_type) BETWEEN 1 AND 255
                      AND instr(content_type, char(0)) = 0),
            application_id BLOB CHECK(application_id IS NULL OR length(application_id) = 16),
            owner TEXT CHECK(owner IS NULL OR (length(owner) BETWEEN 1 AND 255
                      AND instr(owner, char(0)) = 0)),
            head_revision INTEGER NOT NULL DEFAULT 0 CHECK(head_revision >= 0),
            head_digest BLOB CHECK(head_digest IS NULL OR length(head_digest) = 32),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            CHECK((head_revision = 0) = (head_digest IS NULL))
        ) STRICT;

        CREATE TABLE artifact_revisions (
            artifact_id BLOB NOT NULL CHECK(length(artifact_id) = 16),
            revision INTEGER NOT NULL CHECK(revision >= 1),
            digest BLOB NOT NULL CHECK(length(digest) = 32),
            size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            PRIMARY KEY(artifact_id, revision),
            FOREIGN KEY(artifact_id) REFERENCES artifacts(artifact_id)
        ) STRICT;

        CREATE TRIGGER artifact_revisions_immutable_update
        BEFORE UPDATE ON artifact_revisions
        BEGIN
            SELECT RAISE(ABORT, 'artifact revision is immutable');
        END;

        CREATE TRIGGER artifact_revisions_immutable_delete
        BEFORE DELETE ON artifact_revisions
        BEGIN
            SELECT RAISE(ABORT, 'artifact revision is immutable');
        END;

        CREATE TABLE cache_entries (
            cache_key TEXT PRIMARY KEY NOT NULL
                CHECK(length(cache_key) BETWEEN 1 AND 255
                      AND instr(cache_key, char(0)) = 0),
            digest BLOB NOT NULL CHECK(length(digest) = 32),
            size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
        ) STRICT;

        CREATE INDEX cache_entries_by_digest ON cache_entries(digest);

        PRAGMA user_version = 1;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds durable staged revisions and immutable publication receipts.
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), ArtifactError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE artifact_staged_revisions (
            staging_id BLOB PRIMARY KEY NOT NULL CHECK(length(staging_id) = 16),
            idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
            artifact_id BLOB NOT NULL CHECK(length(artifact_id) = 16),
            expected_head_revision INTEGER NOT NULL CHECK(expected_head_revision >= 0),
            target_revision INTEGER NOT NULL CHECK(target_revision >= 1),
            digest BLOB NOT NULL CHECK(length(digest) = 32),
            size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
            stage_state INTEGER NOT NULL DEFAULT 0 CHECK(stage_state IN (0, 1)),
            publication_receipt_id BLOB CHECK(publication_receipt_id IS NULL OR length(publication_receipt_id) = 16),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
            FOREIGN KEY(artifact_id) REFERENCES artifacts(artifact_id),
            CHECK(target_revision = expected_head_revision + 1),
            CHECK((stage_state = 0) = (publication_receipt_id IS NULL))
        ) STRICT;

        CREATE INDEX artifact_staged_by_artifact
            ON artifact_staged_revisions(artifact_id, target_revision, stage_state);

        CREATE TABLE artifact_publication_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            staging_id BLOB NOT NULL UNIQUE CHECK(length(staging_id) = 16),
            artifact_id BLOB NOT NULL CHECK(length(artifact_id) = 16),
            revision INTEGER NOT NULL CHECK(revision >= 1),
            digest BLOB NOT NULL CHECK(length(digest) = 32),
            size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
            prior_head_revision INTEGER NOT NULL CHECK(prior_head_revision >= 0),
            prior_head_digest BLOB CHECK(prior_head_digest IS NULL OR length(prior_head_digest) = 32),
            new_head_revision INTEGER NOT NULL CHECK(new_head_revision >= 1),
            new_head_digest BLOB NOT NULL CHECK(length(new_head_digest) = 32),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            FOREIGN KEY(staging_id) REFERENCES artifact_staged_revisions(staging_id),
            FOREIGN KEY(artifact_id, revision) REFERENCES artifact_revisions(artifact_id, revision),
            CHECK((prior_head_revision = 0) = (prior_head_digest IS NULL)),
            CHECK(new_head_revision = prior_head_revision + 1),
            CHECK(new_head_revision = revision),
            CHECK(new_head_digest = digest)
        ) STRICT;

        CREATE TRIGGER artifact_publication_receipts_immutable_update
        BEFORE UPDATE ON artifact_publication_receipts
        BEGIN
            SELECT RAISE(ABORT, 'artifact publication receipt is immutable');
        END;

        CREATE TRIGGER artifact_publication_receipts_immutable_delete
        BEFORE DELETE ON artifact_publication_receipts
        BEGIN
            SELECT RAISE(ABORT, 'artifact publication receipt is immutable');
        END;

        PRAGMA user_version = 2;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds immutable authority-assigned endpoint identity/proof for every
/// Artifact head. Existing heads receive identities during the migration;
/// future heads receive them in their creation transaction.
pub(crate) fn migrate_v3(connection: &mut Connection) -> Result<(), ArtifactError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name='artifact_head_endpoint_proofs'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name LIKE 'artifact_head_endpoint_proofs_%'",
        [],
        |row| row.get(0),
    )?;
    let missing_proof_count = if table_count == 1 {
        connection.query_row(
            "SELECT COUNT(*) FROM artifacts AS a
             LEFT JOIN artifact_head_endpoint_proofs AS p ON p.artifact_id=a.artifact_id
             WHERE p.artifact_id IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )?
    } else {
        0
    };
    if table_count == 1 && trigger_count == 2 && missing_proof_count == 0 {
        connection.pragma_update(None, "user_version", 3)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 || missing_proof_count != 0 {
        return Err(ArtifactError::CorruptRecord(
            "partial artifact endpoint proof schema",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE artifact_head_endpoint_proofs (
            artifact_id BLOB PRIMARY KEY NOT NULL CHECK(length(artifact_id) = 16),
            participant_id BLOB NOT NULL UNIQUE CHECK(length(participant_id) = 16),
            participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
            admission_receipt_id BLOB NOT NULL UNIQUE CHECK(length(admission_receipt_id) = 16),
            FOREIGN KEY(artifact_id) REFERENCES artifacts(artifact_id)
        ) STRICT;

        INSERT INTO artifact_head_endpoint_proofs
            (artifact_id, participant_id, participant_generation, admission_receipt_id)
        SELECT artifact_id, randomblob(16), X'0000000000000001', randomblob(16)
        FROM artifacts;

        CREATE TRIGGER artifact_head_endpoint_proofs_immutable_update
        BEFORE UPDATE ON artifact_head_endpoint_proofs
        BEGIN SELECT RAISE(ABORT, 'artifact head endpoint proof is immutable'); END;
        CREATE TRIGGER artifact_head_endpoint_proofs_immutable_delete
        BEFORE DELETE ON artifact_head_endpoint_proofs
        BEGIN SELECT RAISE(ABORT, 'artifact head endpoint proof is immutable'); END;

        PRAGMA user_version = 3;",
    )?;
    transaction.commit()?;
    Ok(())
}

pub(crate) fn insert_artifact_head_endpoint_proof(
    transaction: &Transaction<'_>,
    artifact_id: ArtifactId,
) -> Result<(), ArtifactError> {
    transaction.execute(
        "INSERT INTO artifact_head_endpoint_proofs (
            artifact_id, participant_id, participant_generation, admission_receipt_id
         ) VALUES (?1, randomblob(16), ?2, randomblob(16))",
        params![
            artifact_id.as_bytes().as_slice(),
            1_u64.to_be_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

pub(crate) fn load_artifact_head_endpoint_proof(
    connection: &Connection,
    artifact_id: ArtifactId,
) -> Result<ArtifactHeadEndpointProof, ArtifactError> {
    let result = connection.query_row(
        "SELECT participant_id, participant_generation, admission_receipt_id
         FROM artifact_head_endpoint_proofs WHERE artifact_id=?1",
        [artifact_id.as_bytes().as_slice()],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        },
    );
    let (participant_id, generation, receipt_id) = match result {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(ArtifactError::ArtifactNotFound(artifact_id));
        }
        Err(error) => return Err(error.into()),
    };
    let generation = u64::from_be_bytes(
        generation
            .try_into()
            .map_err(|_| ArtifactError::CorruptRecord("artifact endpoint generation"))?,
    );
    let generation = std::num::NonZeroU64::new(generation)
        .map(Generation::new)
        .ok_or(ArtifactError::CorruptRecord(
            "zero artifact endpoint generation",
        ))?;
    Ok(ArtifactHeadEndpointProof {
        artifact_id,
        participant_id: TaskParticipantId::from_bytes(
            participant_id
                .try_into()
                .map_err(|_| ArtifactError::CorruptRecord("artifact endpoint id"))?,
        ),
        participant_generation: generation,
        admission_receipt_id: ReceiptId::from_bytes(
            receipt_id
                .try_into()
                .map_err(|_| ArtifactError::CorruptRecord("artifact endpoint receipt"))?,
        ),
    })
}
