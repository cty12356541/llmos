use rusqlite::{Connection, TransactionBehavior};

use crate::ChannelAuthorityError;

pub(crate) const SCHEMA_VERSION: i64 = 1;

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
