//! Schema DDL. Kept separate so the durable format is auditable in one
//! place; any future migration gets its own function and `user_version`.

use rusqlite::{Connection, TransactionBehavior};

use crate::ArtifactError;

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
