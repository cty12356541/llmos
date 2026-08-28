//! Lane T2: subscription consumption-token identity binding.
//!
//! Pins the authority-issued [`ConsumeToken`] semantics: issuance at
//! subscribe time, same-key replay stability, fail-closed enforcement on
//! `advance_with_token` / `unsubscribe_with_token`, generation bump on
//! re-subscribe (old token invalidated), restart replay, and the schema v3
//! migration re-deriving tokens for pre-existing subscription rows.

#![allow(deprecated)] // Token-free advance/unsubscribe deprecated in favor of the *_with_token entries.

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest};
use nlos_topic::{
    AdvanceDecision, AdvanceRequest, ConsumeToken, CreateTopicRequest, PublishRequest,
    SubscribeDecision, SubscribeRequest, SubscriberKey, TopicAuthority, TopicAuthorityError,
    TopicDecision, TopicPolicy, TopicRecord, UnsubscribeRequest,
};
use nlos_types::{IdempotencyKey, ResourceAccountId};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn payer(seed: u8) -> ResourceAccountId {
    ResourceAccountId::from_bytes([seed; 16])
}

fn subscriber(seed: u8) -> SubscriberKey {
    SubscriberKey::from_bytes([seed; 16])
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-topic-binding-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn topic_db(&self) -> PathBuf {
        self.0.join("topic-authority.db")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    root: Root,
    channel: Arc<ChannelAuthority>,
    topics: TopicAuthority,
}

impl Harness {
    fn new(label: &str) -> Self {
        let root = Root::new(label);
        let channel =
            Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
        let topics =
            TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("open topic authority");
        Self {
            root,
            channel,
            topics,
        }
    }
}

fn policy() -> TopicPolicy {
    TopicPolicy {
        max_recipients: 4,
        delivery_attempts: 3,
        cascade_depth: 2,
        retained_bytes: 4_096,
        retention_ms: 86_400_000,
        payer: payer(7),
    }
}

fn bootstrap(label: &str) -> (Harness, TopicRecord) {
    let harness = Harness::new(label);
    let channel = match harness
        .channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 65_536,
            policy_digest: [0x44; 32],
            idempotency_key: key(1),
            created_at_ms: 1_000,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    };
    let topic = match harness
        .topics
        .create_topic(CreateTopicRequest {
            channel_id: channel.channel_id,
            name: b"binding".to_vec(),
            policy: policy(),
            idempotency_key: key(2),
            created_at_ms: 2_000,
        })
        .expect("create topic")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh topic cannot replay"),
    };
    (harness, topic)
}

fn subscribe(harness: &Harness, topic_id: nlos_topic::TopicId, seed: u8) -> SubscribeDecision {
    harness
        .topics
        .subscribe(SubscribeRequest {
            topic_id,
            subscriber_key: subscriber(seed),
            subscribed_at_ms: 3_000,
        })
        .expect("subscribe")
}

fn publish(harness: &Harness, topic: &TopicRecord, payload_byte: u8) -> u64 {
    let decision = harness
        .topics
        .publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: vec![payload_byte; 8],
            idempotency_key: key(payload_byte),
            published_at_ms: 4_000,
        })
        .expect("publish");
    assert!(matches!(
        decision,
        nlos_topic::PublishDecision::Published(_)
    ));
    harness
        .channel
        .inspect_queue(topic.channel_id)
        .expect("queue state")
        .max_sequence
}

fn advance_request(topic: &TopicRecord, seed: u8, up_to: u64) -> AdvanceRequest {
    AdvanceRequest {
        topic_id: topic.topic_id,
        subscriber_key: subscriber(seed),
        up_to_sequence: up_to,
        advanced_at_ms: 5_000,
    }
}

fn unsubscribe_request(topic: &TopicRecord, seed: u8) -> UnsubscribeRequest {
    UnsubscribeRequest {
        topic_id: topic.topic_id,
        subscriber_key: subscriber(seed),
        unsubscribed_at_ms: 6_000,
    }
}

/// The reference token derivation, mirrored from the authority.
fn expected_token(subscription_id: [u8; 16], generation: u64) -> ConsumeToken {
    let tag = b"nlos/topic/consume-token/v1";
    let mut hasher = Sha256::new();
    hasher.update((tag.len() as u64).to_be_bytes());
    hasher.update(tag);
    hasher.update(16u64.to_be_bytes());
    hasher.update(subscription_id);
    hasher.update(8u64.to_be_bytes());
    hasher.update(generation.to_be_bytes());
    hasher.finalize().into()
}

fn raw_cursor(path: &Path, subscription_id: [u8; 16]) -> u64 {
    let connection = Connection::open(path).expect("open raw db");
    connection
        .query_row(
            "SELECT cursor FROM topic_subscriptions WHERE subscription_id=?1",
            [&subscription_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| u64::try_from(value).expect("cursor"))
        .expect("subscription row")
}

/// Downgrades an existing v3 `topic_subscriptions` table back to its v2
/// shape (dropping the token columns) and rewinds `user_version` to 2, so
/// the next [`TopicAuthority::open`] exercises the v2 -> v3 migration on a
/// database carrying a pre-token subscription row.
fn downgrade_to_v2(path: &Path) {
    let connection = Connection::open(path).expect("open raw db");
    connection
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
            DROP TRIGGER topic_subscriptions_identity_frozen;
            DROP TRIGGER topic_subscriptions_no_delete;
            CREATE TABLE topic_subscriptions_v2 (
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
            INSERT INTO topic_subscriptions_v2
                SELECT subscription_id, topic_id, subscriber_key, active, cursor,
                       subscribed_at_ms, unsubscribed_at_ms, last_advanced_at_ms
                  FROM topic_subscriptions;
            DROP TABLE topic_subscriptions;
            ALTER TABLE topic_subscriptions_v2 RENAME TO topic_subscriptions;
            PRAGMA user_version=2;",
        )
        .expect("downgrade to v2 shape");
}

#[test]
fn subscribe_issues_token_and_replay_returns_the_original() {
    let (harness, topic) = bootstrap("issue-replay");
    let decision = subscribe(&harness, topic.topic_id, 1);
    let issued = match decision {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    assert_ne!(issued.consume_token, [0u8; 32]);
    assert_eq!(issued.subscription_generation, 1);
    // Deterministic authority derivation over id + generation.
    assert_eq!(
        issued.consume_token,
        expected_token(issued.subscription_id.into_bytes(), 1)
    );
    // Same-key replay returns the originally issued token, idempotently.
    let replay = subscribe(&harness, topic.topic_id, 1);
    match replay {
        SubscribeDecision::Replayed(record) => {
            assert_eq!(record.consume_token, issued.consume_token);
            assert_eq!(record.subscription_generation, 1);
        }
        SubscribeDecision::Subscribed(_) => panic!("active key must replay"),
    }
    // A different key gets a different token.
    let other = subscribe(&harness, topic.topic_id, 2);
    let other = match other {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    assert_ne!(other.consume_token, issued.consume_token);
}

#[test]
fn advance_and_unsubscribe_succeed_with_the_correct_token() {
    let (harness, topic) = bootstrap("token-ok");
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    let high = publish(&harness, &topic, 1);
    let decision = harness
        .topics
        .advance_with_token(advance_request(&topic, 1, high), &record.consume_token)
        .expect("advance with the issued token");
    assert!(matches!(decision, AdvanceDecision::Advanced(_)));
    let decision = harness
        .topics
        .unsubscribe_with_token(unsubscribe_request(&topic, 1), &record.consume_token)
        .expect("unsubscribe with the issued token");
    assert!(matches!(
        decision,
        nlos_topic::UnsubscribeDecision::Unsubscribed(_)
    ));
}

#[test]
fn wrong_token_fails_closed_with_zero_writes() {
    let (harness, topic) = bootstrap("token-bad");
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    let high = publish(&harness, &topic, 1);
    let subscription_id: [u8; 16] = record.subscription_id.into_bytes();

    let advance = harness
        .topics
        .advance_with_token(advance_request(&topic, 1, high), &[0xAA; 32]);
    match advance {
        Err(TopicAuthorityError::ConsumptionTokenMismatch(id)) => {
            assert_eq!(id.into_bytes(), subscription_id);
        }
        other => panic!("expected ConsumptionTokenMismatch, got {other:?}"),
    }
    let unsubscribe = harness
        .topics
        .unsubscribe_with_token(unsubscribe_request(&topic, 1), &[0xBB; 32]);
    assert!(matches!(
        unsubscribe,
        Err(TopicAuthorityError::ConsumptionTokenMismatch(_))
    ));

    // Fail-closed, zero durable writes: the cursor and active bit are raw-asserted unchanged.
    assert_eq!(raw_cursor(&harness.root.topic_db(), subscription_id), 0);
    let subscription = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("subscription readback");
    assert!(subscription.active);
    // The correct token still works after the rejected attempts.
    harness
        .topics
        .advance_with_token(advance_request(&topic, 1, high), &record.consume_token)
        .expect("correct token after rejections");
}

#[test]
fn token_free_entries_keep_the_previous_behavior() {
    let (harness, topic) = bootstrap("token-free");
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    let high = publish(&harness, &topic, 1);
    // Legacy entries still work with only the subscriber key.
    let decision = harness
        .topics
        .advance(advance_request(&topic, 1, high))
        .expect("legacy advance");
    assert!(matches!(decision, AdvanceDecision::Advanced(_)));
    let decision = harness
        .topics
        .unsubscribe(unsubscribe_request(&topic, 1))
        .expect("legacy unsubscribe");
    assert!(matches!(
        decision,
        nlos_topic::UnsubscribeDecision::Unsubscribed(_)
    ));
    // poll stays token-free as documented.
    harness
        .topics
        .poll(topic.topic_id, subscriber(1), 8)
        .expect_err("inactive subscription polls rejected");
    assert_eq!(
        record.consume_token,
        expected_token(record.subscription_id.into_bytes(), 1)
    );
}

#[test]
fn resubscribe_bumps_generation_and_invalidates_the_old_token() {
    let (harness, topic) = bootstrap("generation");
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    harness
        .topics
        .unsubscribe_with_token(unsubscribe_request(&topic, 1), &record.consume_token)
        .expect("unsubscribe");
    let high = publish(&harness, &topic, 1);
    let resubscribed = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("resubscribe after unsubscribe is fresh"),
    };
    assert_eq!(resubscribed.subscription_id, record.subscription_id);
    assert_eq!(resubscribed.subscription_generation, 2);
    assert_ne!(resubscribed.consume_token, record.consume_token);
    assert_eq!(
        resubscribed.consume_token,
        expected_token(resubscribed.subscription_id.into_bytes(), 2)
    );

    // The old generation's token fails closed; the new one is admitted.
    let advance = harness
        .topics
        .advance_with_token(advance_request(&topic, 1, high), &record.consume_token);
    assert!(matches!(
        advance,
        Err(TopicAuthorityError::ConsumptionTokenMismatch(_))
    ));
    harness
        .topics
        .advance_with_token(
            advance_request(&topic, 1, high),
            &resubscribed.consume_token,
        )
        .expect("new generation token");
    let unsubscribe = harness
        .topics
        .unsubscribe_with_token(unsubscribe_request(&topic, 1), &record.consume_token);
    assert!(matches!(
        unsubscribe,
        Err(TopicAuthorityError::ConsumptionTokenMismatch(_))
    ));
}

#[test]
fn restart_replays_the_same_token() {
    let (mut harness, topic) = bootstrap("restart");
    let high = publish(&harness, &topic, 1);
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    let token = record.consume_token;
    drop(harness.topics);
    harness.topics = TopicAuthority::open(harness.root.path(), Arc::clone(&harness.channel))
        .expect("reopen topic authority");

    // Active key replay after restart returns the durable token.
    let replay = subscribe(&harness, topic.topic_id, 1);
    match replay {
        SubscribeDecision::Replayed(replayed) => {
            assert_eq!(replayed.consume_token, token);
            assert_eq!(replayed.subscription_generation, 1);
        }
        SubscribeDecision::Subscribed(_) => panic!("active key must replay"),
    }
    harness
        .topics
        .advance_with_token(advance_request(&topic, 1, high), &token)
        .expect("token survives restart");
}

#[test]
fn schema_v3_migration_rederives_tokens_for_existing_rows() {
    let (harness, topic) = bootstrap("migration");
    let high = publish(&harness, &topic, 1);
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    let subscription_id: [u8; 16] = record.subscription_id.into_bytes();
    let db = harness.root.topic_db();
    drop(harness.topics);

    // Rewind the database to the pre-token v2 shape.
    downgrade_to_v2(&db);

    // Reopen: the v2 -> v3 migration runs and re-derives the token for the
    // existing row (generation 1) deterministically.
    let topics = TopicAuthority::open(harness.root.path(), Arc::clone(&harness.channel))
        .expect("migrating reopen");

    let migrated = topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("migrated subscription readback");
    assert_eq!(migrated.subscription_generation, 1);
    assert_eq!(migrated.consume_token, expected_token(subscription_id, 1));
    // And the re-derived token is admitted on the authenticated boundary.
    topics
        .advance_with_token(advance_request(&topic, 1, high), &migrated.consume_token)
        .expect("migrated token advances");
}

/// The deprecated token-free advance entry stays callable and, for the same
/// request, the two entries converge onto one cursor: whichever entry
/// issues, the other replays the byte-identical receipt (mirrors the task
/// ladder-deprecation equivalence convention).
#[test]
#[allow(deprecated)]
fn deprecated_token_free_advance_matches_token_entry() {
    // Direction 1: the deprecated entry issues; the token entry replays
    // the exact same receipt.
    let (harness, topic) = bootstrap("advance-equiv-legacy");
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    let high = publish(&harness, &topic, 1);
    let legacy = harness
        .topics
        .advance(advance_request(&topic, 1, high))
        .expect("legacy advance");
    assert!(matches!(legacy, AdvanceDecision::Advanced(_)));
    let legacy_receipt = legacy.receipt();
    assert_eq!(legacy_receipt.cursor, high);
    assert!(matches!(
        harness
            .topics
            .advance_with_token(advance_request(&topic, 1, high), &record.consume_token),
        Ok(AdvanceDecision::Replayed(receipt)) if receipt == legacy_receipt
    ));

    // Direction 2: the token entry issues; the deprecated entry replays
    // the exact same receipt.
    let (harness, topic) = bootstrap("advance-equiv-token");
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    let high = publish(&harness, &topic, 1);
    let authenticated = harness
        .topics
        .advance_with_token(advance_request(&topic, 1, high), &record.consume_token)
        .expect("token advance");
    assert!(matches!(authenticated, AdvanceDecision::Advanced(_)));
    let authenticated_receipt = authenticated.receipt();
    assert_eq!(authenticated_receipt.cursor, high);
    assert!(matches!(
        harness.topics.advance(advance_request(&topic, 1, high)),
        Ok(AdvanceDecision::Replayed(receipt)) if receipt == authenticated_receipt
    ));
}

/// The deprecated token-free unsubscribe entry stays callable and, for the
/// same request, the two entries converge onto one flip: whichever entry
/// issues, the other replays the byte-identical receipt.
#[test]
#[allow(deprecated)]
fn deprecated_token_free_unsubscribe_matches_token_entry() {
    // Direction 1: the deprecated entry flips; the token entry replays
    // the exact same receipt.
    let (harness, topic) = bootstrap("unsubscribe-equiv-legacy");
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    let legacy = harness
        .topics
        .unsubscribe(unsubscribe_request(&topic, 1))
        .expect("legacy unsubscribe");
    assert!(matches!(
        legacy,
        nlos_topic::UnsubscribeDecision::Unsubscribed(_)
    ));
    let legacy_receipt = legacy.receipt();
    assert!(matches!(
        harness.topics.unsubscribe_with_token(
            unsubscribe_request(&topic, 1),
            &record.consume_token
        ),
        Ok(nlos_topic::UnsubscribeDecision::Replayed(receipt)) if receipt == legacy_receipt
    ));

    // Direction 2: the token entry flips; the deprecated entry replays
    // the exact same receipt.
    let (harness, topic) = bootstrap("unsubscribe-equiv-token");
    let record = match subscribe(&harness, topic.topic_id, 1) {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    let authenticated = harness
        .topics
        .unsubscribe_with_token(unsubscribe_request(&topic, 1), &record.consume_token)
        .expect("token unsubscribe");
    assert!(matches!(
        authenticated,
        nlos_topic::UnsubscribeDecision::Unsubscribed(_)
    ));
    let authenticated_receipt = authenticated.receipt();
    assert!(matches!(
        harness.topics.unsubscribe(unsubscribe_request(&topic, 1)),
        Ok(nlos_topic::UnsubscribeDecision::Replayed(receipt)) if receipt == authenticated_receipt
    ));
}
