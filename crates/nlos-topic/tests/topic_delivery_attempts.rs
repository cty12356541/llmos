//! Lane A (increment 47): delivery-attempts enforcement per the ADR-0007
//! addendum.
//!
//! Pins the billing point (one durable unit per publication that lands while
//! a subscriber is lagging, the first publication finding no backlog is
//! free), exhaustion (the declared budget flips the subscription
//! `QUARANTINED`), the quarantine semantics (poll fails closed for the
//! quarantined subscriber only, advance/unsubscribe stay available, nothing
//! is deleted, publishers are never blocked, quarantined rows stop billing),
//! and the explicit token-authenticated recovery (`reinstate_with_token`).

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest};
use nlos_topic::{
    AdvanceDecision, AdvanceRequest, ConsumeToken, CreateTopicRequest, PublishRequest,
    ReinstateDecision, SubscribeDecision, SubscribeRequest, SubscriberKey, SubscriptionState,
    TopicAuthority, TopicAuthorityError, TopicDecision, TopicPolicy, TopicRecord,
    UnsubscribeRequest,
};
use nlos_types::{IdempotencyKey, ResourceAccountId};
use rusqlite::Connection;
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
            "nlos-topic-attempts-{label}-{}-{nonce}-{sequence}",
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

fn policy_with_attempts(delivery_attempts: u64) -> TopicPolicy {
    TopicPolicy {
        max_recipients: 4,
        delivery_attempts,
        cascade_depth: 2,
        retained_bytes: 4_096,
        retention_ms: 86_400_000,
        payer: payer(7),
    }
}

fn bootstrap(label: &str, delivery_attempts: u64) -> (Harness, TopicRecord) {
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
            name: b"attempts".to_vec(),
            policy: policy_with_attempts(delivery_attempts),
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

fn subscribe(harness: &Harness, topic_id: nlos_topic::TopicId, seed: u8, at: u64) -> Subscription {
    match harness
        .topics
        .subscribe(SubscribeRequest {
            topic_id,
            subscriber_key: subscriber(seed),
            subscribed_at_ms: at,
        })
        .expect("subscribe")
    {
        SubscribeDecision::Subscribed(record) => Subscription {
            record,
            token: token_of(&harness.topics, topic_id, seed),
        },
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    }
}

struct Subscription {
    record: nlos_topic::SubscriptionRecord,
    token: ConsumeToken,
}

fn token_of(topics: &TopicAuthority, topic_id: nlos_topic::TopicId, seed: u8) -> ConsumeToken {
    topics
        .inspect_subscription(topic_id, subscriber(seed))
        .expect("inspect subscription")
        .consume_token
}

fn publish(harness: &Harness, topic: &TopicRecord, seed: u8, at: u64) -> u64 {
    match harness
        .topics
        .publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: vec![seed; 8],
            idempotency_key: key(seed),
            published_at_ms: at,
        })
        .expect("publish")
    {
        nlos_topic::PublishDecision::Published(record) => record.channel_sequence,
        nlos_topic::PublishDecision::Replayed(_) => panic!("fresh publish cannot replay"),
    }
}

fn advance_with_token(harness: &Harness, topic: &TopicRecord, sub: &Subscription, up_to: u64) {
    match harness
        .topics
        .advance_with_token(
            AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: sub.record.subscriber_key,
                up_to_sequence: up_to,
                advanced_at_ms: 9_000,
            },
            &sub.token,
        )
        .expect("advance with token")
    {
        AdvanceDecision::Advanced(_) => {}
        AdvanceDecision::Replayed(_) => panic!("fresh advance cannot replay"),
    }
}

fn raw_row(path: &Path, subscription_id: [u8; 16]) -> (u64, i64, i64) {
    let connection = Connection::open(path).expect("open raw db");
    connection
        .query_row(
            "SELECT cursor, state, redelivery_used FROM topic_subscriptions
             WHERE subscription_id=?1",
            [&subscription_id],
            |row| {
                Ok((
                    u64::try_from(row.get::<_, i64>(0)?).expect("cursor"),
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .expect("subscription row")
}

#[test]
fn caught_up_subscriber_is_never_billed() {
    let (harness, topic) = bootstrap("caught-up", 2);
    let sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    publish(&harness, &topic, 11, 4_000);
    advance_with_token(&harness, &topic, &sub, 1);
    publish(&harness, &topic, 12, 4_100);
    advance_with_token(&harness, &topic, &sub, 2);
    publish(&harness, &topic, 13, 4_200);
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect");
    assert_eq!(record.redelivery_used, 0);
    assert_eq!(record.state, SubscriptionState::Active);
}

#[test]
fn lagging_subscriber_is_billed_per_backlog_publication() {
    let (harness, topic) = bootstrap("billed", 8);
    let sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    // The first publication finds no backlog: a fully caught-up subscriber
    // receives its first delivery free.
    publish(&harness, &topic, 21, 4_000);
    assert_eq!(
        harness
            .topics
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("inspect")
            .redelivery_used,
        0
    );
    publish(&harness, &topic, 22, 4_100);
    publish(&harness, &topic, 23, 4_200);
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect");
    assert_eq!(record.redelivery_used, 2);
    assert_eq!(record.state, SubscriptionState::Active);
    // Catching up does not reset the budget: it is granted once, per ADR.
    let _ = sub;
    advance_with_token(&harness, &topic, &sub, 3);
    publish(&harness, &topic, 24, 4_300);
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect");
    assert_eq!(record.redelivery_used, 2);
    assert_eq!(record.state, SubscriptionState::Active);
}

#[test]
fn budget_exhaustion_flips_quarantined_at_declared_attempts() {
    let (harness, topic) = bootstrap("exhausted", 2);
    let _sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    publish(&harness, &topic, 31, 4_000); // free (no backlog yet)
    publish(&harness, &topic, 32, 4_100); // used 1 < 2: still active
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect");
    assert_eq!(record.redelivery_used, 1);
    assert_eq!(record.state, SubscriptionState::Active);
    publish(&harness, &topic, 33, 4_200); // used 2 >= 2: quarantined
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect");
    assert_eq!(record.redelivery_used, 2);
    assert_eq!(record.state, SubscriptionState::Quarantined);
    assert_eq!(record.quarantined_at_ms, 4_200);
}

#[test]
fn quarantined_poll_fails_closed_while_peers_unaffected() {
    let (harness, topic) = bootstrap("peer", 2);
    let _slow = subscribe(&harness, topic.topic_id, 1, 3_000);
    publish(&harness, &topic, 41, 4_000); // free for both (no backlog yet)
    // Quick joins after seq 1, so its cursor starts at 1: only the slow
    // subscriber is lagging when seqs 2-3 land.
    let quick = subscribe(&harness, topic.topic_id, 2, 3_100);
    publish(&harness, &topic, 42, 4_100); // slow billed 1 (cursor 0 < 1); quick cursor 1: free
    publish(&harness, &topic, 43, 4_200); // slow billed 2 -> quarantined; quick billed 1 < 2
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect slow");
    assert_eq!(record.state, SubscriptionState::Quarantined);
    assert!(matches!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(1), 10)
            .expect_err("quarantined poll"),
        TopicAuthorityError::DeliveryQuarantined(_)
    ));
    let entries = harness
        .topics
        .poll(topic.topic_id, subscriber(2), 10)
        .expect("peer poll unaffected");
    assert_eq!(entries.len(), 2);
    let _ = quick;
}

#[test]
fn quarantined_can_still_advance_and_unsubscribe_with_token() {
    let (harness, topic) = bootstrap("retain-rights", 2);
    let sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    publish(&harness, &topic, 51, 4_000);
    publish(&harness, &topic, 52, 4_100);
    publish(&harness, &topic, 53, 4_200);
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect");
    assert_eq!(record.state, SubscriptionState::Quarantined);
    // Advance and unsubscribe stay available: quarantine only stops delivery.
    advance_with_token(&harness, &topic, &sub, 3);
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect after advance");
    assert_eq!(record.cursor, 3);
    assert_eq!(record.state, SubscriptionState::Quarantined);
    harness
        .topics
        .unsubscribe_with_token(
            UnsubscribeRequest {
                topic_id: topic.topic_id,
                subscriber_key: sub.record.subscriber_key,
                unsubscribed_at_ms: 9_500,
            },
            &sub.token,
        )
        .expect("unsubscribe while quarantined");
}

#[test]
fn reinstate_with_token_restores_active_and_replays() {
    let (harness, topic) = bootstrap("reinstate", 2);
    let sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    publish(&harness, &topic, 61, 4_000);
    publish(&harness, &topic, 62, 4_100);
    publish(&harness, &topic, 63, 4_200);
    assert_eq!(
        harness
            .topics
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("inspect")
            .state,
        SubscriptionState::Quarantined
    );
    // Wrong token: fail closed, zero durable writes.
    let forged: ConsumeToken = [0xEE; 32];
    assert!(matches!(
        harness
            .topics
            .reinstate_with_token(topic.topic_id, subscriber(1), &forged, 9_000,),
        Err(TopicAuthorityError::ConsumptionTokenMismatch(_))
    ));
    let (cursor, state, used) = raw_row(
        &harness.root.topic_db(),
        sub.record.subscription_id.into_bytes(),
    );
    assert_eq!((cursor, state, used), (0, 1, 2));
    // Correct token: counter zeroed, cursor exactly where it was.
    let reinstated = match harness
        .topics
        .reinstate_with_token(topic.topic_id, subscriber(1), &sub.token, 9_100)
        .expect("reinstate")
    {
        ReinstateDecision::Reinstated(record) => record,
        ReinstateDecision::Replayed(_) => panic!("fresh reinstate cannot replay"),
    };
    assert_eq!(reinstated.redelivery_used, 0);
    assert_eq!(reinstated.state, SubscriptionState::Active);
    assert_eq!(reinstated.cursor, 0);
    assert_eq!(reinstated.reinstated_at_ms, 9_100);
    // Repeating an already-completed reinstate replays the durable record.
    assert_eq!(
        harness
            .topics
            .reinstate_with_token(topic.topic_id, subscriber(1), &sub.token, 9_999)
            .expect("reinstate replay"),
        ReinstateDecision::Replayed(reinstated)
    );
}

#[test]
fn reinstate_on_active_subscription_fails_closed() {
    let (harness, topic) = bootstrap("not-quarantined", 4);
    let sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    publish(&harness, &topic, 71, 4_000);
    assert!(matches!(
        harness
            .topics
            .reinstate_with_token(topic.topic_id, subscriber(1), &sub.token, 9_000,),
        Err(TopicAuthorityError::NotQuarantined(_))
    ));
}

#[test]
fn quarantine_does_not_block_publishers_and_stops_billing() {
    let (harness, topic) = bootstrap("publisher", 2);
    let slow = subscribe(&harness, topic.topic_id, 1, 3_000);
    publish(&harness, &topic, 81, 4_000);
    publish(&harness, &topic, 82, 4_100);
    publish(&harness, &topic, 83, 4_200);
    assert_eq!(
        harness
            .topics
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("inspect")
            .state,
        SubscriptionState::Quarantined
    );
    let used_before = raw_row(
        &harness.root.topic_db(),
        slow.record.subscription_id.into_bytes(),
    )
    .2;
    // Publishers are never blocked by a quarantined subscriber, and the
    // quarantined row stops billing (state filter in the billing UPDATE).
    publish(&harness, &topic, 84, 4_300);
    publish(&harness, &topic, 85, 4_400);
    let used_after = raw_row(
        &harness.root.topic_db(),
        slow.record.subscription_id.into_bytes(),
    )
    .2;
    assert_eq!(used_after, used_before);
}

#[test]
fn quarantine_state_replays_after_restart() {
    let (harness, topic) = bootstrap("restart", 2);
    let _ = subscribe(&harness, topic.topic_id, 1, 3_000);
    publish(&harness, &topic, 91, 4_000);
    publish(&harness, &topic, 92, 4_100);
    publish(&harness, &topic, 93, 4_200);
    let before = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect before restart");
    assert_eq!(before.state, SubscriptionState::Quarantined);
    drop(harness.topics);
    drop(harness.channel);
    let channel = Arc::new(ChannelAuthority::open(harness.root.path()).expect("reopen channel"));
    let topics =
        TopicAuthority::open(harness.root.path(), Arc::clone(&channel)).expect("reopen topics");
    assert_eq!(
        topics
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("inspect after restart"),
        before
    );
}
