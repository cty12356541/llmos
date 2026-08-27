use rusqlite::{Connection, TransactionBehavior};

use crate::ChannelAuthorityError;

pub(crate) const SCHEMA_VERSION: i64 = 2;

pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), ChannelAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE channels (
            channel_id BLOB PRIMARY KEY NOT NULL CHECK(length(channel_id)=16),
            create_idempotency_key BLOB NOT NULL UNIQUE CHECK(length(create_idempotency_key)=16),
            current_generation INTEGER NOT NULL CHECK(current_generation >= 1),
            current_fencing_token BLOB NOT NULL CHECK(length(current_fencing_token)=32),
            capacity_bytes INTEGER NOT NULL CHECK(capacity_bytes > 0),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest)=32),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
        ) STRICT;

        CREATE TABLE channel_topic_identities (
            channel_id BLOB PRIMARY KEY NOT NULL CHECK(length(channel_id)=16),
            participant_id BLOB NOT NULL UNIQUE CHECK(length(participant_id)=16),
            FOREIGN KEY(channel_id) REFERENCES channels(channel_id)
        ) STRICT;

        CREATE TABLE channel_generations (
            channel_id BLOB NOT NULL CHECK(length(channel_id)=16),
            channel_generation INTEGER NOT NULL CHECK(channel_generation >= 1),
            fencing_token BLOB NOT NULL CHECK(length(fencing_token)=32),
            capacity_bytes INTEGER NOT NULL CHECK(capacity_bytes > 0),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest)=32),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key)=16),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
            PRIMARY KEY(channel_id, channel_generation),
            FOREIGN KEY(channel_id) REFERENCES channels(channel_id)
        ) STRICT;

        CREATE TABLE channel_rotations (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            channel_id BLOB NOT NULL CHECK(length(channel_id)=16),
            expected_generation INTEGER NOT NULL CHECK(expected_generation >= 1),
            expected_fencing_token BLOB NOT NULL CHECK(length(expected_fencing_token)=32),
            resulting_generation INTEGER NOT NULL CHECK(resulting_generation = expected_generation + 1),
            resulting_fencing_token BLOB NOT NULL CHECK(length(resulting_fencing_token)=32),
            rotated_at_ms INTEGER NOT NULL CHECK(rotated_at_ms >= 0),
            FOREIGN KEY(channel_id, resulting_generation)
                REFERENCES channel_generations(channel_id, channel_generation)
        ) STRICT;

        CREATE TABLE channel_endpoint_proofs (
            channel_id BLOB NOT NULL CHECK(length(channel_id)=16),
            channel_generation INTEGER NOT NULL CHECK(channel_generation >= 1),
            participant_id BLOB NOT NULL CHECK(length(participant_id)=16),
            admission_receipt_id BLOB NOT NULL UNIQUE CHECK(length(admission_receipt_id)=16),
            PRIMARY KEY(channel_id, channel_generation),
            FOREIGN KEY(channel_id) REFERENCES channel_topic_identities(channel_id),
            FOREIGN KEY(channel_id, channel_generation)
                REFERENCES channel_generations(channel_id, channel_generation)
        ) STRICT;

        CREATE TRIGGER channel_generations_immutable_update
        BEFORE UPDATE ON channel_generations BEGIN
            SELECT RAISE(ABORT, 'channel generation is immutable');
        END;
        CREATE TRIGGER channel_generations_immutable_delete
        BEFORE DELETE ON channel_generations BEGIN
            SELECT RAISE(ABORT, 'channel generation is immutable');
        END;
        CREATE TRIGGER channel_topic_identities_immutable_update
        BEFORE UPDATE ON channel_topic_identities BEGIN
            SELECT RAISE(ABORT, 'channel topic identity is immutable');
        END;
        CREATE TRIGGER channel_topic_identities_immutable_delete
        BEFORE DELETE ON channel_topic_identities BEGIN
            SELECT RAISE(ABORT, 'channel topic identity is immutable');
        END;
        CREATE TRIGGER channel_endpoint_proofs_immutable_update
        BEFORE UPDATE ON channel_endpoint_proofs BEGIN
            SELECT RAISE(ABORT, 'channel endpoint proof is immutable');
        END;
        CREATE TRIGGER channel_endpoint_proofs_immutable_delete
        BEFORE DELETE ON channel_endpoint_proofs BEGIN
            SELECT RAISE(ABORT, 'channel endpoint proof is immutable');
        END;

        PRAGMA user_version=1;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds the durable queue delivery prefix: immutable queue entries, the
/// non-destructive consume/trim high-water cursors and the byte bookkeeping
/// row.  Existing v1 tables and rows are left untouched; every current
/// channel is backfilled with a zeroed cursor and bookkeeping row.
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), ChannelAuthorityError> {
    let queue_table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'channel_queue_entries', 'channel_queue_cursors', 'channel_queue_bytes'
         )",
        [],
        |row| row.get(0),
    )?;
    let queue_trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'channel_queue_entries_immutable_update',
            'channel_queue_entries_compaction_delete',
            'channel_queue_cursors_no_delete',
            'channel_queue_bytes_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if queue_table_count == 3 && queue_trigger_count == 4 {
        connection.pragma_update(None, "user_version", 2)?;
        return Ok(());
    }
    if queue_table_count != 0 || queue_trigger_count != 0 {
        return Err(ChannelAuthorityError::CorruptRecord(
            "partial channel queue schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE channel_queue_cursors (
            channel_id BLOB PRIMARY KEY NOT NULL CHECK(length(channel_id)=16),
            consume_high_water INTEGER NOT NULL DEFAULT 0 CHECK(consume_high_water >= 0),
            trim_high_water INTEGER NOT NULL DEFAULT 0
                CHECK(trim_high_water >= 0 AND trim_high_water <= consume_high_water),
            last_ack_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(last_ack_at_ms >= 0),
            FOREIGN KEY(channel_id) REFERENCES channels(channel_id)
        ) STRICT;

        CREATE TABLE channel_queue_entries (
            channel_id BLOB NOT NULL CHECK(length(channel_id)=16),
            channel_generation INTEGER NOT NULL CHECK(channel_generation >= 1),
            fencing_token BLOB NOT NULL CHECK(length(fencing_token)=32),
            sequence INTEGER NOT NULL CHECK(sequence >= 1),
            payload BLOB NOT NULL CHECK(length(payload) > 0),
            payload_bytes INTEGER NOT NULL
                CHECK(payload_bytes > 0 AND payload_bytes = length(payload)),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key)=16),
            enqueued_at_ms INTEGER NOT NULL CHECK(enqueued_at_ms >= 0),
            PRIMARY KEY(channel_id, sequence),
            UNIQUE(idempotency_key),
            FOREIGN KEY(channel_id) REFERENCES channels(channel_id),
            FOREIGN KEY(channel_id, channel_generation)
                REFERENCES channel_generations(channel_id, channel_generation)
        ) STRICT;

        CREATE TABLE channel_queue_bytes (
            channel_id BLOB PRIMARY KEY NOT NULL CHECK(length(channel_id)=16),
            backlog_bytes INTEGER NOT NULL DEFAULT 0 CHECK(backlog_bytes >= 0),
            retained_bytes INTEGER NOT NULL DEFAULT 0
                CHECK(retained_bytes >= 0 AND backlog_bytes <= retained_bytes),
            FOREIGN KEY(channel_id) REFERENCES channels(channel_id)
        ) STRICT;

        CREATE TRIGGER channel_queue_entries_immutable_update
        BEFORE UPDATE ON channel_queue_entries BEGIN
            SELECT RAISE(ABORT, 'channel queue entry is immutable');
        END;
        CREATE TRIGGER channel_queue_entries_compaction_delete
        BEFORE DELETE ON channel_queue_entries
        WHEN OLD.sequence > COALESCE(
            (SELECT trim_high_water FROM channel_queue_cursors WHERE channel_id = OLD.channel_id),
            -1
        )
        BEGIN
            SELECT RAISE(
                ABORT,
                'channel queue entry deletion requires the compaction trim high-water'
            );
        END;
        CREATE TRIGGER channel_queue_cursors_no_delete
        BEFORE DELETE ON channel_queue_cursors BEGIN
            SELECT RAISE(ABORT, 'channel queue cursor is non-destructive');
        END;
        CREATE TRIGGER channel_queue_bytes_no_delete
        BEFORE DELETE ON channel_queue_bytes BEGIN
            SELECT RAISE(ABORT, 'channel queue byte bookkeeping is non-destructive');
        END;

        INSERT INTO channel_queue_cursors (channel_id) SELECT channel_id FROM channels;
        INSERT INTO channel_queue_bytes (channel_id) SELECT channel_id FROM channels;

        PRAGMA user_version=2;",
    )?;
    transaction.commit()?;
    Ok(())
}
