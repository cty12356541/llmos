use rusqlite::{Connection, TransactionBehavior};

use crate::TopicAuthorityError;

pub(crate) const SCHEMA_VERSION: i64 = 1;

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
