use rusqlite::{Connection, TransactionBehavior, params};

use crate::TopicAuthorityError;

/// The `user_version` watermark of the durable schema: the version through
/// which the v1-v5 table-rebuild chain has run.  The v6 matching-predicate
/// step and the v7 payer-attribution-ledger step (the ADR-0007 addenda) are
/// additive and are tracked by the durable presence of their objects
/// (`topic_patterns` plus the `topic_subscriptions.attached_by` column, and
/// `topic_attribution_ledger` respectively) instead of a watermark bump — a
/// database carrying those objects therefore still reads `5`, every open
/// re-runs the idempotent additive pre-checks, and any higher watermark (a
/// schema this build does not know) fails closed.
pub(crate) const SCHEMA_VERSION: i64 = 5;

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

/// Adds the delivery-attempt execution state to the per-subscriber rows
/// (schema v4, `RSM-FANOUT-001`): every subscription durably carries its
/// delivery state (`0` = ACTIVE, `1` = QUARANTINED), the durable
/// `redelivery_used` counter billed at the enqueue-commit point, the
/// timestamp of the quarantine flip and the timestamp of the most recent
/// explicit reinstate (the reinstate replay marker).
///
/// The billing point — one counter unit per publication that enqueued while
/// the subscriber was genuinely lagging, with the quarantine flip at the
/// declared `delivery_attempts` — lives in the enqueue-commit transaction of
/// [`crate::TopicAuthority::publish`] and
/// [`crate::TopicAuthority::republish`]; this migration only backfills the
/// new columns for existing rows (`0` counter, ACTIVE, no quarantine or
/// reinstate timestamps), so upgraded databases start from the same state as
/// fresh ones.  Idempotent on reopen via the column pre-check; an unexpected
/// partial column state fails closed as
/// [`TopicAuthorityError::CorruptRecord`].
///
/// `SQLite` cannot alter `CHECK` constraints in place, so
/// `topic_subscriptions` is rebuilt through the documented table-rebuild
/// procedure; the immutability and no-delete triggers are recreated.
pub(crate) fn migrate_v4(connection: &mut Connection) -> Result<(), TopicAuthorityError> {
    let column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('topic_subscriptions')
         WHERE name IN ('state', 'redelivery_used', 'quarantined_at_ms', 'reinstated_at_ms')",
        [],
        |row| row.get(0),
    )?;
    if column_count == 4 {
        connection.pragma_update(None, "user_version", 4)?;
        return Ok(());
    }
    if column_count != 0 {
        return Err(TopicAuthorityError::CorruptRecord(
            "partial topic subscription delivery-attempt schema",
        ));
    }

    // The documented SQLite table-rebuild procedure requires foreign-key
    // enforcement off around (not inside) the rebuild transaction.
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS topic_subscriptions_identity_frozen;
        DROP TRIGGER IF EXISTS topic_subscriptions_no_delete;

        CREATE TABLE topic_subscriptions_v4 (
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
            state INTEGER NOT NULL DEFAULT 0 CHECK(state IN (0,1)),
            redelivery_used INTEGER NOT NULL DEFAULT 0 CHECK(redelivery_used >= 0),
            quarantined_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(quarantined_at_ms >= 0),
            reinstated_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(reinstated_at_ms >= 0),
            UNIQUE(topic_id, subscriber_key),
            FOREIGN KEY(topic_id) REFERENCES topics(topic_id)
        ) STRICT;

        INSERT INTO topic_subscriptions_v4 (
            subscription_id, topic_id, subscriber_key, active, cursor,
            subscribed_at_ms, unsubscribed_at_ms, last_advanced_at_ms,
            consume_token, subscription_generation,
            state, redelivery_used, quarantined_at_ms, reinstated_at_ms
         )
         SELECT subscription_id, topic_id, subscriber_key, active, cursor,
                subscribed_at_ms, unsubscribed_at_ms, last_advanced_at_ms,
                consume_token, subscription_generation,
                0, 0, 0, 0
           FROM topic_subscriptions;

        CREATE TRIGGER topic_subscriptions_identity_frozen
        BEFORE UPDATE ON topic_subscriptions_v4
        WHEN NEW.subscription_id != OLD.subscription_id
            OR NEW.topic_id != OLD.topic_id
            OR NEW.subscriber_key != OLD.subscriber_key
        BEGIN
            SELECT RAISE(ABORT, 'subscription identity is immutable');
        END;
        CREATE TRIGGER topic_subscriptions_no_delete
        BEFORE DELETE ON topic_subscriptions_v4 BEGIN
            SELECT RAISE(ABORT, 'subscription rows are durable');
        END;

        DROP TABLE topic_subscriptions;
        ALTER TABLE topic_subscriptions_v4 RENAME TO topic_subscriptions;

        PRAGMA user_version=4;",
    )?;
    transaction.commit()?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Adds the cached payload-length metadata to the publication journal
/// (schema v5, increment 52): every publication row durably records the
/// byte length of its payload alongside its digest, so the retention byte
/// bound can be evaluated as the exact `Σ payload_bytes(sequence >
/// min(active cursors, channel consume high-water))` of the ADR-0007
/// retention addendum for any sequence window — including the window a
/// live subscriber shadows below the channel consume point, which the
/// channel-side `inspect_queue` byte counters cannot expose.
///
/// This is length *metadata*, not a message-body copy: the payload itself
/// stays in the Channel log (the Topic authority continues to store no
/// message bodies, per ADR-0007).  Enqueue rejects empty payloads, so a
/// recorded length is always `>= 1`; `0` is therefore an unambiguous
/// sentinel for "recorded before schema v5" and rows migrated from earlier
/// schemas are backfilled with it.  The retention admission treats any
/// sentinel row inside the summation window by merging the exact known-row
/// sum with the channel-side retained upper bound in the never-understating
/// direction; sentinel rows leave the window as catch-up and compaction
/// advance the bound past them.  Rows inserted from schema v5 on always
/// carry the true length.
///
/// `SQLite` cannot alter `CHECK` constraints or triggers in place, so
/// `topic_publications` is rebuilt through the documented table-rebuild
/// procedure; the immutability trigger additionally freezes the recorded
/// length on an enqueued row except for a regression to the `0` sentinel
/// (the conservative direction — an unknown length can only widen, never
/// narrow, the admission's byte estimate).  Idempotent on reopen via the
/// column pre-check; an unexpected partial column state fails closed as
/// [`TopicAuthorityError::CorruptRecord`].
pub(crate) fn migrate_v5(connection: &mut Connection) -> Result<(), TopicAuthorityError> {
    let column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('topic_publications')
         WHERE name='payload_bytes'",
        [],
        |row| row.get(0),
    )?;
    if column_count == 1 {
        connection.pragma_update(None, "user_version", 5)?;
        return Ok(());
    }
    if column_count != 0 {
        return Err(TopicAuthorityError::CorruptRecord(
            "partial topic publication payload-length schema",
        ));
    }

    // The documented SQLite table-rebuild procedure requires foreign-key
    // enforcement off around (not inside) the rebuild transaction.
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS topic_publications_commit_transition;
        DROP TRIGGER IF EXISTS topic_publications_no_delete;

        CREATE TABLE topic_publications_v5 (
            idempotency_key BLOB PRIMARY KEY NOT NULL CHECK(length(idempotency_key)=16),
            topic_id BLOB NOT NULL CHECK(length(topic_id)=16),
            policy_digest BLOB NOT NULL CHECK(length(policy_digest)=32),
            payer_account_id BLOB NOT NULL CHECK(length(payer_account_id)=16),
            payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
            payload_bytes INTEGER NOT NULL CHECK(payload_bytes >= 0),
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

        INSERT INTO topic_publications_v5 (
            idempotency_key, topic_id, policy_digest, payer_account_id,
            payload_digest, payload_bytes, status, channel_sequence,
            channel_generation, cascade_budget_remaining, cascade_level,
            parent_idempotency_key, published_at_ms, enqueued_at_ms
         )
         SELECT idempotency_key, topic_id, policy_digest, payer_account_id,
                payload_digest, 0, status, channel_sequence,
                channel_generation, cascade_budget_remaining, cascade_level,
                parent_idempotency_key, published_at_ms, enqueued_at_ms
           FROM topic_publications;

        DROP TABLE topic_publications;
        ALTER TABLE topic_publications_v5 RENAME TO topic_publications;

        CREATE TRIGGER topic_publications_commit_transition
        BEFORE UPDATE ON topic_publications
        WHEN NEW.idempotency_key != OLD.idempotency_key
            OR NEW.topic_id != OLD.topic_id
            OR NEW.policy_digest != OLD.policy_digest
            OR NEW.payer_account_id != OLD.payer_account_id
            OR NEW.payload_digest != OLD.payload_digest
            OR (OLD.status = 1
                AND NEW.payload_bytes != OLD.payload_bytes
                AND NEW.payload_bytes != 0)
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

        PRAGMA user_version=5;",
    )?;
    transaction.commit()?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Adds the matching-predicate prefix (schema v6, the ADR-0007
/// matching-predicate addendum): a durable `topic_patterns` table holds the
/// pattern subscription rows — the pattern text, the opaque 16-byte
/// subscriber binding (never the all-zero unbound value), the subscriber
/// key, the authority-derived consumption token over the pattern id and its
/// generation (mirroring the concrete subscription token), the UNIQUE
/// idempotency key, the subscribe/cancel timestamps and the generation —
/// with the crate's durable-row conventions: the identity triple
/// (pattern id, pattern text, subscriber key) is frozen by trigger, rows
/// are never deleted, and the only legal state transition is the guarded
/// `ACTIVE -> CANCELLED` flip (the re-activation path re-derives the token
/// and generation through the same Rust-side CAS that guards the concrete
/// re-subscribe).
///
/// `topic_subscriptions` gains the `attached_by` provenance column: `NULL`
/// for a direct subscription, the originating pattern row id for a
/// subscription expanded by a pattern attach.  `SQLite` cannot add a column
/// through the `CHECK`-carrying table in place, so the table is rebuilt
/// through the documented table-rebuild procedure; existing rows are
/// carried over verbatim as direct subscriptions (`attached_by = NULL`).
///
/// The joint column-and-table pre-check keeps reopen idempotent and orders
/// the failure modes by recoverability: both objects present is the
/// completed migration (the `user_version` watermark is left untouched — it
/// belongs to the v1-v5 rebuild chain and already reads 5); the provenance
/// column present but the pattern table missing is unrecoverable (durable
/// rows would reference a dropped table) and fails closed as
/// [`TopicAuthorityError::CorruptRecord`]; the pattern table present on a
/// subscriptions table that predates v6 is simply an older subscriptions
/// shape under an already-created pattern table (the whole migration is one
/// atomic transaction, so this cannot be a torn v6) and takes the
/// subscriptions-rebuild-only path.  A truly fresh database runs both.
#[allow(clippy::too_many_lines)] // One atomic STRICT schema batch, mirroring every other migration step.
pub(crate) fn migrate_v6(connection: &mut Connection) -> Result<(), TopicAuthorityError> {
    let attached_by_columns: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('topic_subscriptions')
         WHERE name='attached_by'",
        [],
        |row| row.get(0),
    )?;
    let pattern_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='topic_patterns'",
        [],
        |row| row.get(0),
    )?;
    if attached_by_columns == 1 {
        if pattern_tables == 1 {
            // The v6 step is complete: the watermark belongs to the v1-v5
            // rebuild chain and is left untouched (it already reads 5 after
            // v5, or is restored by the v5 pre-check in the open chain).
            return Ok(());
        }
        return Err(TopicAuthorityError::CorruptRecord(
            "topic pattern schema lost its pattern table",
        ));
    }

    // The documented SQLite table-rebuild procedure requires foreign-key
    // enforcement off around (not inside) the rebuild transaction.
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if pattern_tables == 0 {
        transaction.execute_batch(
            "CREATE TABLE topic_patterns (
                pattern_id BLOB PRIMARY KEY NOT NULL CHECK(length(pattern_id)=16),
                pattern_text BLOB NOT NULL CHECK(length(pattern_text) > 0),
                binding BLOB NOT NULL
                    CHECK(length(binding)=16 AND binding != x'00000000000000000000000000000000'),
                subscriber_key BLOB NOT NULL CHECK(length(subscriber_key)=16),
                active INTEGER NOT NULL CHECK(active IN (0,1)),
                consume_token BLOB NOT NULL CHECK(length(consume_token)=32),
                pattern_generation INTEGER NOT NULL CHECK(pattern_generation >= 1),
                subscribed_at_ms INTEGER NOT NULL CHECK(subscribed_at_ms >= 0),
                cancelled_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(cancelled_at_ms >= 0),
                create_idempotency_key BLOB NOT NULL UNIQUE
                    CHECK(length(create_idempotency_key)=16),
                CHECK((active = 1) = (cancelled_at_ms = 0)),
                UNIQUE(pattern_text, subscriber_key)
            ) STRICT;

            CREATE TRIGGER topic_patterns_identity_frozen
            BEFORE UPDATE ON topic_patterns
            WHEN NEW.pattern_id != OLD.pattern_id
                OR NEW.pattern_text != OLD.pattern_text
                OR NEW.subscriber_key != OLD.subscriber_key
            BEGIN
                SELECT RAISE(ABORT, 'pattern identity is immutable');
            END;
            CREATE TRIGGER topic_patterns_no_delete
            BEFORE DELETE ON topic_patterns BEGIN
                SELECT RAISE(ABORT, 'pattern rows are durable');
            END;",
        )?;
    }
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS topic_subscriptions_identity_frozen;
        DROP TRIGGER IF EXISTS topic_subscriptions_no_delete;

        CREATE TABLE topic_subscriptions_v6 (
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
            state INTEGER NOT NULL DEFAULT 0 CHECK(state IN (0,1)),
            redelivery_used INTEGER NOT NULL DEFAULT 0 CHECK(redelivery_used >= 0),
            quarantined_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(quarantined_at_ms >= 0),
            reinstated_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(reinstated_at_ms >= 0),
            attached_by BLOB CHECK(attached_by IS NULL OR length(attached_by)=16),
            UNIQUE(topic_id, subscriber_key),
            FOREIGN KEY(topic_id) REFERENCES topics(topic_id),
            FOREIGN KEY(attached_by) REFERENCES topic_patterns(pattern_id)
        ) STRICT;

        INSERT INTO topic_subscriptions_v6 (
            subscription_id, topic_id, subscriber_key, active, cursor,
            subscribed_at_ms, unsubscribed_at_ms, last_advanced_at_ms,
            consume_token, subscription_generation, state, redelivery_used,
            quarantined_at_ms, reinstated_at_ms, attached_by
         )
         SELECT subscription_id, topic_id, subscriber_key, active, cursor,
                subscribed_at_ms, unsubscribed_at_ms, last_advanced_at_ms,
                consume_token, subscription_generation, state, redelivery_used,
                quarantined_at_ms, reinstated_at_ms, NULL
           FROM topic_subscriptions;

        CREATE TRIGGER topic_subscriptions_identity_frozen
        BEFORE UPDATE ON topic_subscriptions_v6
        WHEN NEW.subscription_id != OLD.subscription_id
            OR NEW.topic_id != OLD.topic_id
            OR NEW.subscriber_key != OLD.subscriber_key
        BEGIN
            SELECT RAISE(ABORT, 'subscription identity is immutable');
        END;
        CREATE TRIGGER topic_subscriptions_no_delete
        BEFORE DELETE ON topic_subscriptions_v6 BEGIN
            SELECT RAISE(ABORT, 'subscription rows are durable');
        END;

        DROP TABLE topic_subscriptions;
        ALTER TABLE topic_subscriptions_v6 RENAME TO topic_subscriptions;",
    )?;
    transaction.commit()?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Adds the payer metering ledger (schema v7, the ADR-0007 payer-metering
/// addendum): a durable `topic_attribution_ledger` table holds one immutable
/// row per accounted publication — the authority-derived ledger identity, the
/// topic, the publication row's payer (metering follows the publication, not
/// the subscriber), the kind (`1` = `Attributed`: an advance crossed the
/// sequence and the payload was delivered; `2` = `Unallocated`: the
/// publication left the log without ever being delivered), the accounted
/// payload byte length, the [`crate::ATTRIBUTION_POLICY_VERSION`] frozen into
/// the row, the single evidence sequence, and the recording timestamp.
///
/// One row per `(topic_id, evidence_sequence)` is a table-level constraint
/// (the no-double-count backstop behind the first-accounting-event-wins
/// semantics), and both triggers ban `UPDATE` and `DELETE` outright: ledger
/// rows are write-once audit facts, corrected only by new facts.
///
/// Like v6, the step is additive and is tracked by the durable presence of
/// its object instead of a watermark bump (the `user_version` watermark
/// belongs to the v1-v5 rebuild chain and already reads 5): the pre-check
/// keeps reopen idempotent, and the table plus its triggers are created in
/// one atomic transaction, so no partial state is representable.
pub(crate) fn migrate_v7(connection: &mut Connection) -> Result<(), TopicAuthorityError> {
    let ledger_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name='topic_attribution_ledger'",
        [],
        |row| row.get(0),
    )?;
    if ledger_tables == 1 {
        // The v7 step is complete: the watermark belongs to the v1-v5
        // rebuild chain and is left untouched (it already reads 5 after v5,
        // or is restored by the v5 pre-check in the open chain).
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE topic_attribution_ledger (
            ledger_id BLOB PRIMARY KEY NOT NULL CHECK(length(ledger_id)=16),
            topic_id BLOB NOT NULL CHECK(length(topic_id)=16),
            payer_account_id BLOB NOT NULL
                CHECK(length(payer_account_id)=16
                      AND payer_account_id != x'00000000000000000000000000000000'),
            kind INTEGER NOT NULL CHECK(kind IN (1,2)),
            payload_bytes INTEGER NOT NULL CHECK(payload_bytes >= 0),
            policy_version INTEGER NOT NULL CHECK(policy_version >= 1),
            evidence_sequence INTEGER NOT NULL CHECK(evidence_sequence >= 1),
            recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms >= 0),
            UNIQUE(topic_id, evidence_sequence),
            FOREIGN KEY(topic_id) REFERENCES topics(topic_id)
        ) STRICT;

        CREATE TRIGGER topic_attribution_ledger_no_update
        BEFORE UPDATE ON topic_attribution_ledger BEGIN
            SELECT RAISE(ABORT, 'attribution ledger rows are immutable');
        END;
        CREATE TRIGGER topic_attribution_ledger_no_delete
        BEFORE DELETE ON topic_attribution_ledger BEGIN
            SELECT RAISE(ABORT, 'attribution ledger rows are durable');
        END;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds the publisher credit-admission prefix (schema v8, the B-TOPIC-001
/// credit increment): a durable `topic_credit_accounts` table holds one
/// prepaid account row per payer — the payer [`crate::ResourceAccountId`]
/// (never the all-zero unbound value) as the primary key, the cached
/// `balance_units`, the cumulative `total_granted_units` (initial grant plus
/// every accepted recharge) and `total_spent_units`, the frozen opening
/// idempotency key and the two timestamps — with the row-level CHECK
/// `balance_units + total_spent_units = total_granted_units` making an
/// internally inconsistent account row structurally impossible, and the
/// crate's durable-row conventions: the payer, the opening key and the
/// open timestamp are frozen by trigger, and the row is never deleted.
///
/// The companion `topic_credit_entries` table is the account's append-only
/// movement journal: one immutable row per movement — `1` = `Open` (the
/// initial grant, carrying the opening idempotency key), `2` = `Recharge`
/// (one accepted recharge, carrying its own idempotency key — the replay
/// receipt that makes recharges idempotent), `3` = `Spend` (one admitted
/// publication's byte charge, carrying the publication's idempotency key as
/// the UNIQUE evidence — a second charge for one publication is
/// structurally impossible, mirroring the attribution ledger's
/// one-row-per-evidence discipline).  UPDATE and DELETE are banned outright
/// by trigger: entries are write-once audit facts.
///
/// The credit face is deliberately separate from the payer attribution
/// ledger (`topic_attribution_ledger`): attribution records what *was
/// delivered* after the fact and never rejects; credit pre-charges what is
/// *admitted* and gates.  Neither reads nor writes the other's rows and the
/// attribution reconciliation identity is untouched.
///
/// Like v6 and v7, the step is additive and is tracked by the durable
/// presence of its objects instead of a watermark bump (the `user_version`
/// watermark belongs to the v1-v5 rebuild chain and already reads 5): the
/// joint two-table pre-check keeps reopen idempotent, one table present
/// without the other is unrecoverable and fails closed as
/// [`TopicAuthorityError::CorruptRecord`], and both tables plus their
/// triggers are created in one atomic transaction, so no partial state is
/// representable.
pub(crate) fn migrate_v8(connection: &mut Connection) -> Result<(), TopicAuthorityError> {
    let account_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name='topic_credit_accounts'",
        [],
        |row| row.get(0),
    )?;
    let entry_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name='topic_credit_entries'",
        [],
        |row| row.get(0),
    )?;
    if account_tables == 1 {
        if entry_tables == 1 {
            // The v8 step is complete: the watermark belongs to the v1-v5
            // rebuild chain and is left untouched (it already reads 5 after
            // v5, or is restored by the v5 pre-check in the open chain).
            return Ok(());
        }
        return Err(TopicAuthorityError::CorruptRecord(
            "topic credit schema lost its entry journal",
        ));
    }
    if entry_tables == 1 {
        return Err(TopicAuthorityError::CorruptRecord(
            "topic credit schema lost its account table",
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE topic_credit_accounts (
            payer_account_id BLOB PRIMARY KEY NOT NULL
                CHECK(length(payer_account_id)=16
                      AND payer_account_id != x'00000000000000000000000000000000'),
            balance_units INTEGER NOT NULL CHECK(balance_units >= 0),
            total_granted_units INTEGER NOT NULL CHECK(total_granted_units >= 0),
            total_spent_units INTEGER NOT NULL CHECK(total_spent_units >= 0),
            open_idempotency_key BLOB NOT NULL UNIQUE
                CHECK(length(open_idempotency_key)=16),
            opened_at_ms INTEGER NOT NULL CHECK(opened_at_ms >= 0),
            last_mutated_at_ms INTEGER NOT NULL CHECK(last_mutated_at_ms >= 0),
            CHECK(balance_units + total_spent_units = total_granted_units)
        ) STRICT;

        CREATE TRIGGER topic_credit_accounts_identity_frozen
        BEFORE UPDATE ON topic_credit_accounts
        WHEN NEW.payer_account_id != OLD.payer_account_id
            OR NEW.open_idempotency_key != OLD.open_idempotency_key
            OR NEW.opened_at_ms != OLD.opened_at_ms
        BEGIN
            SELECT RAISE(ABORT, 'credit account identity is immutable');
        END;
        CREATE TRIGGER topic_credit_accounts_no_delete
        BEFORE DELETE ON topic_credit_accounts BEGIN
            SELECT RAISE(ABORT, 'credit account rows are durable');
        END;

        CREATE TABLE topic_credit_entries (
            entry_id BLOB PRIMARY KEY NOT NULL CHECK(length(entry_id)=16),
            payer_account_id BLOB NOT NULL CHECK(length(payer_account_id)=16),
            kind INTEGER NOT NULL CHECK(kind IN (1,2,3)),
            units INTEGER NOT NULL
                CHECK(units >= CASE kind WHEN 1 THEN 0 ELSE 1 END),
            idempotency_key BLOB UNIQUE CHECK(idempotency_key IS NULL
                                               OR length(idempotency_key)=16),
            evidence_key BLOB UNIQUE CHECK(evidence_key IS NULL
                                            OR length(evidence_key)=16),
            recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms >= 0),
            CHECK((kind = 3) = (evidence_key IS NOT NULL)),
            CHECK((kind = 3) = (idempotency_key IS NULL)),
            FOREIGN KEY(payer_account_id)
                REFERENCES topic_credit_accounts(payer_account_id)
        ) STRICT;

        CREATE TRIGGER topic_credit_entries_no_update
        BEFORE UPDATE ON topic_credit_entries BEGIN
            SELECT RAISE(ABORT, 'credit entries are immutable');
        END;
        CREATE TRIGGER topic_credit_entries_no_delete
        BEFORE DELETE ON topic_credit_entries BEGIN
            SELECT RAISE(ABORT, 'credit entries are durable');
        END;",
    )?;
    transaction.commit()?;
    Ok(())
}
