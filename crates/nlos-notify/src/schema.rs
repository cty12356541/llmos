use rusqlite::{Connection, TransactionBehavior};

use crate::NotifyError;

pub(crate) const SCHEMA_VERSION_V1: i64 = 1;
pub(crate) const SCHEMA_VERSION: i64 = 2;

/// Creates the notification face schema v1: the thin-layer subscription
/// references table (`notify_subscriptions`).
///
/// Each row references `TopicAuthority` entities (the topic id, the
/// subscriber key and the authority-issued consumption token) plus the face
/// registration time — nothing else.  There is deliberately no subscription
/// state, cursor, queue, payload or delivery column: every canonical fanout
/// fact lives in the `Topic`/`Channel`/`Wait` authorities and is read back
/// live through them.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), NotifyError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name='notify_subscriptions'",
        [],
        |row| row.get(0),
    )?;
    if table_count == 1 {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION_V1)?;
        return Ok(());
    }
    if table_count != 0 {
        return Err(NotifyError::CorruptRecord(
            "partial notification face schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE notify_subscriptions (
            notification_id BLOB PRIMARY KEY NOT NULL
                CHECK(length(notification_id)=16),
            topic_id BLOB NOT NULL CHECK(length(topic_id)=16),
            subscriber_key BLOB NOT NULL CHECK(length(subscriber_key)=16),
            consume_token BLOB NOT NULL CHECK(length(consume_token)=32),
            registered_at_ms INTEGER NOT NULL CHECK(registered_at_ms >= 0)
        ) STRICT;
        PRAGMA user_version = 1;",
    )?;
    transaction.commit()?;
    Ok(())
}

const MIGRATE_V2_SQL: &str = "
CREATE TABLE notify_action_tokens (
    token_id BLOB PRIMARY KEY NOT NULL CHECK(length(token_id)=16),
    idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key)=16),
    notification_id BLOB NOT NULL CHECK(length(notification_id)=16),
    application_id BLOB NOT NULL CHECK(length(application_id)=16),
    task_id BLOB NOT NULL CHECK(length(task_id)=16),
    delivery_sequence INTEGER NOT NULL CHECK(delivery_sequence >= 1),
    action_code INTEGER NOT NULL CHECK(action_code >= 0 AND action_code <= 65535),
    secret_commitment BLOB NOT NULL CHECK(length(secret_commitment)=32),
    issued_at_ms INTEGER NOT NULL CHECK(issued_at_ms >= 0),
    expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms >= issued_at_ms),
    status TEXT NOT NULL CHECK(status IN ('ISSUED', 'REDEEMED', 'REVOKED')),
    redeemed_at_ms INTEGER CHECK(redeemed_at_ms IS NULL OR redeemed_at_ms >= 0),
    redeem_receipt_id BLOB CHECK(redeem_receipt_id IS NULL OR length(redeem_receipt_id)=16),
    revoked_at_ms INTEGER CHECK(revoked_at_ms IS NULL OR revoked_at_ms >= 0),
    CHECK(
        (status = 'ISSUED'
            AND redeemed_at_ms IS NULL
            AND redeem_receipt_id IS NULL
            AND revoked_at_ms IS NULL)
        OR (status = 'REDEEMED'
            AND redeemed_at_ms IS NOT NULL
            AND redeem_receipt_id IS NOT NULL
            AND revoked_at_ms IS NULL)
        OR (status = 'REVOKED'
            AND redeemed_at_ms IS NULL
            AND redeem_receipt_id IS NULL
            AND revoked_at_ms IS NOT NULL)
    )
) STRICT;

CREATE INDEX idx_notify_action_tokens_notification
    ON notify_action_tokens (notification_id);

CREATE TRIGGER trg_notify_action_tokens_immutable_bindings
BEFORE UPDATE ON notify_action_tokens
WHEN OLD.token_id IS NOT NEW.token_id
  OR OLD.idempotency_key IS NOT NEW.idempotency_key
  OR OLD.notification_id IS NOT NEW.notification_id
  OR OLD.application_id IS NOT NEW.application_id
  OR OLD.task_id IS NOT NEW.task_id
  OR OLD.delivery_sequence IS NOT NEW.delivery_sequence
  OR OLD.action_code IS NOT NEW.action_code
  OR OLD.secret_commitment IS NOT NEW.secret_commitment
  OR OLD.issued_at_ms IS NOT NEW.issued_at_ms
  OR OLD.expires_at_ms IS NOT NEW.expires_at_ms
BEGIN
    SELECT RAISE(ABORT, 'notify_action_tokens bound columns are immutable');
END;

CREATE TRIGGER trg_notify_action_tokens_terminal_once
BEFORE UPDATE ON notify_action_tokens
WHEN OLD.status != 'ISSUED' OR NEW.status NOT IN ('REDEEMED', 'REVOKED')
BEGIN
    SELECT RAISE(ABORT, 'notify_action_tokens terminal token cannot be resurrected');
END;

CREATE TRIGGER trg_notify_action_tokens_no_delete
BEFORE DELETE ON notify_action_tokens
BEGIN
    SELECT RAISE(ABORT, 'notify_action_tokens one-shot tombstones cannot be deleted');
END;

CREATE TABLE notify_history_refs (
    topic_id BLOB NOT NULL CHECK(length(topic_id)=16),
    channel_sequence INTEGER NOT NULL CHECK(channel_sequence >= 1),
    channel_id BLOB NOT NULL CHECK(length(channel_id)=16),
    payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
    recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms >= 0),
    entry_kind TEXT NOT NULL CHECK(entry_kind IN ('GENERIC', 'TASK_TRANSITION')),
    task_id BLOB CHECK(task_id IS NULL OR length(task_id)=16),
    application_id BLOB CHECK(application_id IS NULL OR length(application_id)=16),
    task_generation INTEGER CHECK(task_generation IS NULL OR task_generation >= 1),
    transition_tag INTEGER
        CHECK(transition_tag IS NULL OR (transition_tag >= 1 AND transition_tag <= 16)),
    requires_user_action INTEGER
        CHECK(requires_user_action IS NULL OR requires_user_action IN (0, 1)),
    summary_digest BLOB CHECK(summary_digest IS NULL OR length(summary_digest)=32),
    PRIMARY KEY (topic_id, channel_sequence),
    CHECK(
        (entry_kind = 'GENERIC'
            AND task_id IS NULL
            AND application_id IS NULL
            AND task_generation IS NULL
            AND transition_tag IS NULL
            AND requires_user_action IS NULL
            AND summary_digest IS NULL)
        OR (entry_kind = 'TASK_TRANSITION'
            AND task_id IS NOT NULL
            AND application_id IS NOT NULL
            AND task_generation IS NOT NULL
            AND transition_tag IS NOT NULL
            AND requires_user_action IS NOT NULL
            AND summary_digest IS NOT NULL)
    )
) STRICT;

CREATE TRIGGER trg_notify_history_refs_immutable
BEFORE UPDATE ON notify_history_refs
BEGIN
    SELECT RAISE(ABORT, 'notify_history_refs rows are immutable');
END;

CREATE TABLE notify_history_prune_watermarks (
    topic_id BLOB PRIMARY KEY NOT NULL CHECK(length(topic_id)=16),
    retained_from_sequence INTEGER NOT NULL CHECK(retained_from_sequence >= 0)
) STRICT;

CREATE TRIGGER trg_notify_history_prune_monotonic
BEFORE UPDATE ON notify_history_prune_watermarks
WHEN OLD.topic_id IS NOT NEW.topic_id
  OR NEW.retained_from_sequence < OLD.retained_from_sequence
BEGIN
    SELECT RAISE(ABORT, 'notify_history_prune_watermarks retained_from_sequence must not regress');
END;

CREATE TRIGGER trg_notify_history_prune_no_delete
BEFORE DELETE ON notify_history_prune_watermarks
BEGIN
    SELECT RAISE(ABORT, 'notify_history_prune_watermarks cannot be deleted');
END;

PRAGMA user_version = 2;";

/// Upgrades an existing v1 notification face database in-place to schema v2
/// (`SCHEMA_VERSION = 2`), preserving every `notify_subscriptions` row while
/// adding durable `STRICT` tables and immutability/one-shot triggers for:
///
/// - `[DUI-NOTIFY-001]` cryptographically bound one-shot action tokens
///   (`notify_action_tokens`, storing only the `SHA-256` verifier commitment
///   of `secret_proof`, never raw secrets, with `SQLite` triggers blocking
///   mutation of bound columns, resurrection of redeemed/revoked tokens, and
///   tombstone deletion);
/// - `[DUI-OFFLINE-001]` local offline notification history references and
///   monotonic retention watermarks (`notify_history_refs`,
///   `notify_history_prune_watermarks`).
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), NotifyError> {
    let v1_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name='notify_subscriptions'",
        [],
        |row| row.get(0),
    )?;
    if v1_count != 1 {
        return Err(NotifyError::CorruptRecord(
            "missing v1 notify_subscriptions table during v2 migration",
        ));
    }

    let (v2_tables, v2_triggers) = count_v2_objects(connection)?;
    if v2_tables == 3 && v2_triggers == 6 {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }
    if v2_tables != 0 || v2_triggers != 0 {
        return Err(NotifyError::CorruptRecord(
            "partial notification face v2 schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(MIGRATE_V2_SQL)?;
    transaction.commit()?;
    Ok(())
}

fn count_v2_objects(connection: &Connection) -> Result<(i64, i64), NotifyError> {
    let v2_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
             'notify_action_tokens',
             'notify_history_refs',
             'notify_history_prune_watermarks'
         )",
        [],
        |row| row.get(0),
    )?;
    let v2_triggers: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'trg_notify_action_tokens_immutable_bindings',
             'trg_notify_action_tokens_terminal_once',
             'trg_notify_action_tokens_no_delete',
             'trg_notify_history_refs_immutable',
             'trg_notify_history_prune_monotonic',
             'trg_notify_history_prune_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    Ok((v2_tables, v2_triggers))
}

/// Verifies that a database claiming `SCHEMA_VERSION` (`2`) carries all
/// expected `STRICT` tables and immutability triggers.
pub(crate) fn verify_v2(connection: &Connection) -> Result<(), NotifyError> {
    let tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
             'notify_subscriptions',
             'notify_action_tokens',
             'notify_history_refs',
             'notify_history_prune_watermarks'
         )",
        [],
        |row| row.get(0),
    )?;
    let triggers: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'trg_notify_action_tokens_immutable_bindings',
             'trg_notify_action_tokens_terminal_once',
             'trg_notify_action_tokens_no_delete',
             'trg_notify_history_refs_immutable',
             'trg_notify_history_prune_monotonic',
             'trg_notify_history_prune_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if tables != 4 || triggers != 6 {
        return Err(NotifyError::CorruptRecord(
            "corrupt or incomplete notification face v2 schema",
        ));
    }
    Ok(())
}

/// Ensures the connection is upgraded to `SCHEMA_VERSION` (`2`) before any
/// v2 extension table is accessed.
pub(crate) fn ensure_v2(connection: &mut Connection) -> Result<(), NotifyError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        SCHEMA_VERSION => Ok(()),
        SCHEMA_VERSION_V1 => migrate_v2(connection),
        0 => {
            migrate_v1(connection)?;
            migrate_v2(connection)
        }
        other => Err(NotifyError::SchemaVersionUnsupported(other)),
    }
}
