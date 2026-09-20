use rusqlite::{Connection, TransactionBehavior};

use crate::NotifyError;

pub(crate) const SCHEMA_VERSION: i64 = 1;

/// Creates the notification face schema v1: exactly one table of thin-layer
/// subscription references.  Each row references Topic-authority entities
/// (the topic id, the subscriber key and the authority-issued consumption
/// token) plus the face registration time — nothing else.  There is
/// deliberately no subscription state, cursor, queue, payload or delivery
/// column: every canonical fact lives in the Topic/Channel/Wait
/// authorities and is read back live through them.
///
/// The only legal `UPDATE` is the subscribe-time upsert refreshing the
/// stored credential to the subscription's current generation; `DELETE` is
/// the cancel path (the authority keeps the canonical unsubscribe audit
/// row, so the face deletion loses no durable fact).
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), NotifyError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name='notify_subscriptions'",
        [],
        |row| row.get(0),
    )?;
    if table_count == 1 {
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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
        ) STRICT;",
    )?;
    transaction.commit()?;
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}
