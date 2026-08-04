//! Schema v1 DDL. Kept separate so the durable format is auditable in one
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
