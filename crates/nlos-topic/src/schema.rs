use rusqlite::{Connection, TransactionBehavior, params};

use crate::TopicAuthorityError;

pub(crate) const SCHEMA_VERSION: i64 = 3;

/// Creates the Topic service-layer authority schema v1: the immutable-topic
/// head (policy, payer binding, channel fence snapshot, active-subscriber
/// counter), the per-subscriber state rows and the immutable publication
/// journal whose only legal mutation is the `PENDING_ENQUEUE -> ENQUEUED`
/// commit transition.
///
/// The Topic authority never stores message bodies: the Channel authority
/// remains the single message-log source of truth.  Durable rows carry only
/// bindings, digests, cursors and the channel sequence association.
#[allow(clippy::too_many_lines)] // One atomic STRICT schema batch with its immutability triggers.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), TopicAuthorityError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE topics (
            topic_id BLOB PRIMARY KEY NOT NULL CHECK(length(topic_id)=16),
            channel_id BLOB NOT NULL CHECK(length(channel_id)=16),
            topic_name BLOB NOT NULL CHECK(length(topic_name) > 0),
            create_idempotency_key BLOB NOT NULL UNIQUE
                CHECK(length(create_idempotency_key)=16),
            channel_generation INTEGER NOT NULL CHECK(channel_generation >= 1),
            channel_fencing_token BLOB NOT NULL CHECK(length(channel_fencing_token)=32),
            max_recipients INTEGER NOT NULL CHECK(max_recipients >= 1),
            delivery_attempts INTEGER NOT NULL CHECK(delivery_attempts >= 1),
            cascade_depth INTEGER NOT NULL CHECK(cascade_depth >= 1),
            retained_bytes INTEGER NOT NULL CHECK(retained_bytes >= 1),
            retention_ms INTEGER NOT NULL CHECK(retention_ms >= 1),
            payer_account_id BLOB NOT NULL
                CHECK(length(payer_account_id)=16 AND payer_account_id != x'00000000000000000000000000000000'),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest)=32),
            active_subscriptions INTEGER NOT NULL DEFAULT 0
                CHECK(active_subscriptions >= 0 AND active_subscriptions <= max_recipients),
            created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
        ) STRICT;

        CREATE TABLE topic_subscriptions (
            subscription_id BLOB PRIMARY KEY NOT NULL CHECK(length(subscription_id)=16),
            topic_id BLOB NOT NULL CHECK(length(topic_id)=16),
            subscriber_key BLOB NOT NULL CHECK(length(subscriber_key)=16),
            active INTEGER NOT NULL CHECK(active IN (0,1)),
            cursor INTEGER NOT NULL CHECK(cursor >= 0),
            subscribed_at_ms INTEGER NOT NULL CHECK(subscribed_at_ms >= 0),
            unsubscribed_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(unsubscribed_at_ms >= 0),
            last_advanced_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(last_advanced_at_ms >= 0),
            UNIQUE(topic_id, subscriber_key),
            FOREIGN KEY(topic_id) REFERENCES topics(topic_id)
        ) STRICT;

        CREATE TABLE topic_publications (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            topic_id BLOB NOT NULL CHECK(length(topic_id)=16),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest)=32),
            payer_account_id BLOB NOT NULL CHECK(length(payer_account_id)=16),
            payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
            status INTEGER NOT NULL CHECK(status IN (0,1)),
            channel_sequence INTEGER NOT NULL DEFAULT 0 CHECK(channel_sequence >= 0),
            channel_generation INTEGER NOT NULL DEFAULT 0 CHECK(channel_generation >= 0),
            cascade_budget_remaining INTEGER NOT NULL CHECK(cascade_budget_remaining >= 1),
            published_at_ms INTEGER NOT NULL CHECK(published_at_ms >= 0),
            enqueued_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(enqueued_at_ms >= 0),
            CHECK((status = 1) = (channel_sequence >= 1)),
            CHECK((status = 1) = (channel_generation >= 1)),
            CHECK((status = 0) = (enqueued_at_ms = 0)),
            FOREIGN KEY(topic_id) REFERENCES topics(topic_id)
        ) STRICT;

        CREATE TRIGGER topics_identity_frozen
        BEFORE UPDATE ON topics
        WHEN NEW.topic_id != OLD.topic_id
            OR NEW.channel_id != OLD.channel_id
            OR NEW.topic_name != OLD.topic_name
            OR NEW.create_idempotency_key != OLD.create_idempotency_key
            OR NEW.max_recipients != OLD.max_recipients
            OR NEW.delivery_attempts != OLD.delivery_attempts
            OR NEW.cascade_depth != OLD.cascade_depth
            OR NEW.retained_bytes != OLD.retained_bytes
            OR NEW.retention_ms != OLD.retention_ms
            OR NEW.payer_account_id != OLD.payer_account_id
            OR NEW.policy_digest != OLD.policy_digest
            OR NEW.created_at_ms != OLD.created_at_ms
        BEGIN
            SELECT RAISE(ABORT, 'topic identity and policy are immutable');
        END;
        CREATE TRIGGER topics_no_delete
        BEFORE DELETE ON topics BEGIN
            SELECT RAISE(ABORT, 'topic head is durable');
        END;
        CREATE TRIGGER topic_subscriptions_identity_frozen
        BEFORE UPDATE ON topic_subscriptions
        WHEN NEW.subscription_id != OLD.subscription_id
            OR NEW.topic_id != OLD.topic_id
            OR NEW.subscriber_key != OLD.subscriber_key
        BEGIN
            SELECT RAISE(ABORT, 'subscription identity is immutable');
        END;
        CREATE TRIGGER topic_subscriptions_no_delete
        BEFORE DELETE ON topic_subscriptions BEGIN
            SELECT RAISE(ABORT, 'subscription rows are durable');
        END;
        CREATE TRIGGER topic_publications_commit_transition
        BEFORE UPDATE ON topic_publications
        WHEN NEW.idempotency_key != OLD.idempotency_key
            OR NEW.topic_id != OLD.topic_id
            OR NEW.policy_digest != OLD.policy_digest
            OR NEW.payer_account_id != OLD.payer_account_id
            OR NEW.payload_digest != OLD.payload_digest
            OR NEW.cascade_budget_remaining != OLD.cascade_budget_remaining
            OR NEW.published_at_ms != OLD.published_at_ms
            OR OLD.status != 0 OR NEW.status != 1
        BEGIN
            SELECT RAISE(ABORT, 'topic publication is immutable beyond enqueue commit');
        END;
        CREATE TRIGGER topic_publications_no_delete
        BEFORE DELETE ON topic_publications BEGIN
            SELECT RAISE(ABORT, 'topic publication is durable');
        END;

        PRAGMA user_version=1;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds the cascade provenance and budget-spend prefix to the publication
/// journal (schema v2, `RSM-FANOUT-001`): every publication row records its
/// cascade level and its parent publication, the cascade budget may reach 0
/// (fully spent publications stay durable) and may only decrease, and only on
/// an already-enqueued row.
///
/// `SQLite` cannot alter `CHECK` constraints or triggers in place, so
/// `topic_publications` is rebuilt through the documented table-rebuild
/// procedure: the v1 rows are carried over verbatim as level-0 root
/// publications with no parent, and the immutability trigger is replaced by
/// one admitting exactly two legal mutations — the
/// `PENDING_ENQUEUE -> ENQUEUED` commit transition and a monotone
/// cascade-budget decrement on an enqueued row.
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), TopicAuthorityError> {
    let cascade_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('topic_publications')
         WHERE name='parent_idempotency_key'",
        [],
        |row| row.get(0),
    )?;
    if cascade_column_count == 1 {
        connection.pragma_update(None, "user_version", 2)?;
        return Ok(());
    }
    if cascade_column_count != 0 {
        return Err(TopicAuthorityError::CorruptRecord(
            "partial topic cascade schema",
        ));
    }

    // The documented SQLite table-rebuild procedure requires foreign-key
    // enforcement off around (not inside) the rebuild transaction.
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "DROP TRIGGER topic_publications_commit_transition;

        CREATE TABLE topic_publications_v2 (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            topic_id BLOB NOT NULL CHECK(length(topic_id)=16),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest)=32),
            payer_account_id BLOB NOT NULL CHECK(length(payer_account_id)=16),
            payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
            status INTEGER NOT NULL CHECK(status IN (0,1)),
            channel_sequence INTEGER NOT NULL DEFAULT 0 CHECK(channel_sequence >= 0),
            channel_generation INTEGER NOT NULL DEFAULT 0 CHECK(channel_generation >= 0),
            cascade_budget_remaining INTEGER NOT NULL CHECK(cascade_budget_remaining >= 0),
            cascade_level INTEGER NOT NULL DEFAULT 0 CHECK(cascade_level >= 0),
            parent_idempotency_key BLOB
                CHECK(parent_idempotency_key IS NULL
                       OR length(parent_idempotency_key)=16),
            published_at_ms INTEGER NOT NULL CHECK(published_at_ms >= 0),
            enqueued_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(enqueued_at_ms >= 0),
            CHECK((status = 1) = (channel_sequence >= 1)),
            CHECK((status = 1) = (channel_generation >= 1)),
            CHECK((status = 0) = (enqueued_at_ms = 0)),
            CHECK((parent_idempotency_key IS NULL) = (cascade_level = 0)),
            FOREIGN KEY(topic_id) REFERENCES topics(topic_id),
            FOREIGN KEY(parent_idempotency_key)
                REFERENCES topic_publications(idempotency_key)
        ) STRICT;

        INSERT INTO topic_publications_v2 (
            idempotency_key, topic_id, policy_digest, payer_account_id,
            payload_digest, status, channel_sequence, channel_generation,
            cascade_budget_remaining, cascade_level, parent_idempotency_key,
            published_at_ms, enqueued_at_ms
         )
         SELECT idempotency_key, topic_id, policy_digest, payer_account_id,
                payload_digest, status, channel_sequence, channel_generation,
                cascade_budget_remaining, 0, NULL, published_at_ms, enqueued_at_ms
           FROM topic_publications;

        DROP TABLE topic_publications;
        ALTER TABLE topic_publications_v2 RENAME TO topic_publications;

        CREATE TRIGGER topic_publications_commit_transition
        BEFORE UPDATE ON topic_publications
        WHEN NEW.idempotency_key != OLD.idempotency_key
            OR NEW.topic_id != OLD.topic_id
            OR NEW.policy_digest != OLD.policy_digest
            OR NEW.payer_account_id != OLD.payer_account_id
            OR NEW.payload_digest != OLD.payload_digest
            OR NEW.parent_idempotency_key IS NOT OLD.parent_idempotency_key
            OR NEW.cascade_level != OLD.cascade_level
            OR NEW.published_at_ms != OLD.published_at_ms
            OR NEW.cascade_budget_remaining > OLD.cascade_budget_remaining
            OR (NEW.cascade_budget_remaining != OLD.cascade_budget_remaining
                AND OLD.status != 1)
            OR (OLD.status = 1
                AND (NEW.channel_sequence != OLD.channel_sequence
                     OR NEW.channel_generation != OLD.channel_generation
                     OR NEW.enqueued_at_ms != OLD.enqueued_at_ms))
            OR (OLD.status != NEW.status
                AND NOT (OLD.status = 0 AND NEW.status = 1))
        BEGIN
            SELECT RAISE(
                ABORT,
                'topic publication is immutable beyond enqueue commit and cascade budget spend'
            );
        END;
        CREATE TRIGGER topic_publications_no_delete
        BEFORE DELETE ON topic_publications BEGIN
            SELECT RAISE(ABORT, 'topic publication is durable');
        END;

        PRAGMA user_version=2;",
    )?;
    transaction.commit()?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Adds the consumption-token identity binding to the per-subscriber state
/// rows (schema v3): every subscription row durably carries the
/// authority-derived [`ConsumeToken`] and its subscription generation, so
/// `advance`/`unsubscribe` can require the caller to present the token the
/// authority issued at subscribe time instead of trusting the public
/// `subscriber_key` alone.
///
/// The token is deterministic over the stored [`crate::SubscriptionId`] and
/// the subscription generation, so the migration re-derives it for every
/// existing row (generation 1, the historical single-subscription lifetime)
/// rather than leaving it NULL: upgraded databases immediately enforce the
/// same fail-closed binding as fresh ones.  Idempotent on reopen via the
/// column pre-check; an unexpected partial column state fails closed as
/// [`TopicAuthorityError::CorruptRecord`].
///
/// `SQLite` cannot alter `CHECK` constraints in place, so
/// `topic_subscriptions` is rebuilt through the documented table-rebuild
/// procedure; the immutability and no-delete triggers are recreated.
pub(crate) fn migrate_v3(connection: &mut Connection) -> Result<(), TopicAuthorityError> {
    let token_column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('topic_subscriptions')
         WHERE name='consume_token'",
        [],
        |row| row.get(0),
    )?;
    if token_column_count == 1 {
        connection.pragma_update(None, "user_version", 3)?;
        return Ok(());
    }
    if token_column_count != 0 {
        return Err(TopicAuthorityError::CorruptRecord(
            "partial topic subscription token schema",
        ));
    }

    // The documented SQLite table-rebuild procedure requires foreign-key
    // enforcement off around (not inside) the rebuild transaction.
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS topic_subscriptions_identity_frozen;
        DROP TRIGGER IF EXISTS topic_subscriptions_no_delete;

        CREATE TABLE topic_subscriptions_v3 (
            subscription_id BLOB PRIMARY KEY NOT NULL CHECK(length(subscription_id)=16),
            topic_id BLOB NOT NULL CHECK(length(topic_id)=16),
            subscriber_key BLOB NOT NULL CHECK(length(subscriber_key)=16),
            active INTEGER NOT NULL CHECK(active IN (0,1)),
            cursor INTEGER NOT NULL CHECK(cursor >= 0),
            subscribed_at_ms INTEGER NOT NULL CHECK(subscribed_at_ms >= 0),
            unsubscribed_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(unsubscribed_at_ms >= 0),
            last_advanced_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(last_advanced_at_ms >= 0),
            consume_token BLOB NOT NULL CHECK(length(consume_token)=32),
            subscription_generation INTEGER NOT NULL CHECK(subscription_generation >= 1),
            UNIQUE(topic_id, subscriber_key),
            FOREIGN KEY(topic_id) REFERENCES topics(topic_id)
        ) STRICT;

        CREATE TRIGGER topic_subscriptions_identity_frozen
        BEFORE UPDATE ON topic_subscriptions_v3
        WHEN NEW.subscription_id != OLD.subscription_id
            OR NEW.topic_id != OLD.topic_id
            OR NEW.subscriber_key != OLD.subscriber_key
        BEGIN
            SELECT RAISE(ABORT, 'subscription identity is immutable');
        END;
        CREATE TRIGGER topic_subscriptions_no_delete
        BEFORE DELETE ON topic_subscriptions_v3 BEGIN
            SELECT RAISE(ABORT, 'subscription rows are durable');
        END;

        PRAGMA user_version=3;",
    )?;
    // Re-derive the deterministic token for every existing row (generation
    // 1: pre-v3 databases have a single subscription lifetime per key).
    {
        let mut read = transaction.prepare("SELECT subscription_id FROM topic_subscriptions")?;
        let ids = read
            .query_map([], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<Result<Vec<Vec<u8>>, _>>()?;
        drop(read);
        let mut write = transaction.prepare(
            "INSERT INTO topic_subscriptions_v3 (
                subscription_id, topic_id, subscriber_key, active, cursor,
                subscribed_at_ms, unsubscribed_at_ms, last_advanced_at_ms,
                consume_token, subscription_generation
             )
             SELECT subscription_id, topic_id, subscriber_key, active, cursor,
                    subscribed_at_ms, unsubscribed_at_ms, last_advanced_at_ms,
                    ?1, 1
               FROM topic_subscriptions
              WHERE subscription_id=?2",
        )?;
        for id in ids {
            let subscription_id =
                crate::SubscriptionId::from_bytes(id.try_into().map_err(|_| {
                    TopicAuthorityError::CorruptRecord("identity length is not 16")
                })?);
            let token = crate::derive_consume_token(subscription_id, 1);
            write.execute(params![
                token.as_slice(),
                subscription_id.as_bytes().as_slice()
            ])?;
        }
    }
    transaction.execute_batch(
        "DROP TABLE topic_subscriptions;
        ALTER TABLE topic_subscriptions_v3 RENAME TO topic_subscriptions;",
    )?;
    transaction.commit()?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}
