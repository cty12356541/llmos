//! Lane R (increment 48): retention backpressure execution per the ADR-0007
//! "retention 执行语义" addendum.
//!
//! Pins: the byte bound (an exact fill is admitted, one payload over is
//! rejected, measured as channel-side payload bytes beyond
//! `min(active subscriber cursors, channel consume high-water)` — the same
//! release point as `compact_bound`, released only when consumption
//! progresses on both axes), the no-subscriber bound (the channel consume
//! high-water, released by ack), the time bound (the oldest live entry still
//! held by an active subscriber, measured against the caller-supplied
//! request time, released when the subscriber advances past it), republish
//! child-enqueue admission (rejected before the parent cascade budget is
//! spent), zero partial state on rejection coexisting with same-key
//! idempotent retry, the conservative stand-in when a subscriber lags behind
//! the channel consume point, and the orthogonality of quarantine (a
//! `QUARANTINED` subscriber's lag holds no retention budget).

use nlos_channel::{
    AckRequest, ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest,
};
use nlos_topic::{
    AdvanceDecision, AdvanceRequest, ConsumeToken, CreateTopicRequest, PublishDecision,
    PublishRequest, RepublishDecision, RepublishRequest, SubscribeDecision, SubscribeRequest,
    SubscriberKey, SubscriptionState, TopicAuthority, TopicAuthorityError, TopicDecision, TopicId,
    TopicPolicy, TopicRecord,
};
use nlos_types::{IdempotencyKey, ResourceAccountId};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

const PAYLOAD: usize = 10;
const DAY_MS: u64 = 86_400_000;

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
            "nlos-topic-retention-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    /// Held (never read) for its [`Drop`]: removing the temporary directory
    /// when the harness goes away.
    #[allow(dead_code)]
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

fn policy(retained_bytes: u64, retention_ms: u64, delivery_attempts: u64) -> TopicPolicy {
    TopicPolicy {
        max_recipients: 4,
        delivery_attempts,
        cascade_depth: 2,
        retained_bytes,
        retention_ms,
        payer: payer(7),
    }
}

fn create_channel(harness: &Harness, key_seed: u8) -> ChannelRecord {
    match harness
        .channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 65_536,
            policy_digest: [0x44; 32],
            idempotency_key: key(key_seed),
            created_at_ms: 1_000,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

fn create_topic(
    harness: &Harness,
    channel_id: nlos_types::ChannelId,
    name: &[u8],
    policy: TopicPolicy,
    key_seed: u8,
) -> TopicRecord {
    match harness
        .topics
        .create_topic(CreateTopicRequest {
            channel_id,
            name: name.to_vec(),
            policy,
            idempotency_key: key(key_seed),
            created_at_ms: 2_000,
        })
        .expect("create topic")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh topic cannot replay"),
    }
}

fn bootstrap(
    label: &str,
    retained_bytes: u64,
    retention_ms: u64,
    delivery_attempts: u64,
) -> (Harness, TopicRecord) {
    let harness = Harness::new(label);
    let channel = create_channel(&harness, 1);
    let topic = create_topic(
        &harness,
        channel.channel_id,
        b"retention",
        policy(retained_bytes, retention_ms, delivery_attempts),
        2,
    );
    (harness, topic)
}

struct Subscription {
    record: nlos_topic::SubscriptionRecord,
    token: ConsumeToken,
}

fn subscribe(harness: &Harness, topic_id: TopicId, seed: u8, at: u64) -> Subscription {
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
            token: harness
                .topics
                .inspect_subscription(topic_id, subscriber(seed))
                .expect("inspect subscription")
                .consume_token,
            record,
        },
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    }
}

fn publish_ok(harness: &Harness, topic: &TopicRecord, seed: u8, at: u64) -> u64 {
    match harness
        .topics
        .publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: vec![seed; PAYLOAD],
            idempotency_key: key(seed),
            published_at_ms: at,
        })
        .expect("publish admitted by retention")
    {
        PublishDecision::Published(record) => record.channel_sequence,
        PublishDecision::Replayed(_) => panic!("fresh publish cannot replay"),
    }
}

fn publish_err(harness: &Harness, topic: &TopicRecord, seed: u8, at: u64) -> TopicAuthorityError {
    harness
        .topics
        .publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: vec![seed; PAYLOAD],
            idempotency_key: key(seed),
            published_at_ms: at,
        })
        .expect_err("publish must be rejected by retention")
}

fn republish_ok(
    harness: &Harness,
    child: &TopicRecord,
    parent_key: IdempotencyKey,
    seed: u8,
    at: u64,
) -> nlos_topic::PublicationRecord {
    match harness
        .topics
        .republish(RepublishRequest {
            child_topic_id: child.topic_id,
            parent_publication_key: parent_key,
            payload: vec![seed; PAYLOAD],
            idempotency_key: key(seed),
            republished_at_ms: at,
        })
        .expect("republish admitted by retention")
    {
        RepublishDecision::Republished(record) => record,
        RepublishDecision::Replayed(_) => panic!("fresh republish cannot replay"),
    }
}

fn republish_err(
    harness: &Harness,
    child: &TopicRecord,
    parent_key: IdempotencyKey,
    seed: u8,
    at: u64,
) -> TopicAuthorityError {
    harness
        .topics
        .republish(RepublishRequest {
            child_topic_id: child.topic_id,
            parent_publication_key: parent_key,
            payload: vec![seed; PAYLOAD],
            idempotency_key: key(seed),
            republished_at_ms: at,
        })
        .expect_err("republish must be rejected by retention")
}

/// Asserts the error is the typed retention rejection and returns
/// `(retained_bytes_declared, backlog_bytes, payload_bytes,
/// retention_ms_declared, oldest_unconsumed_age_ms)` for exact pinning.
fn expect_retention(error: TopicAuthorityError) -> (u64, u64, u64, u64, u64) {
    match error {
        TopicAuthorityError::TopicRetentionExhausted {
            retained_bytes_declared,
            backlog_bytes,
            payload_bytes,
            retention_ms_declared,
            oldest_unconsumed_age_ms,
            ..
        } => (
            retained_bytes_declared,
            backlog_bytes,
            payload_bytes,
            retention_ms_declared,
            oldest_unconsumed_age_ms,
        ),
        other => panic!("expected TopicRetentionExhausted, got {other:?}"),
    }
}

fn advance(harness: &Harness, topic: &TopicRecord, sub: &Subscription, up_to: u64) {
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

fn ack(harness: &Harness, topic: &TopicRecord, up_to: u64) {
    harness
        .channel
        .ack(AckRequest {
            channel_id: topic.channel_id,
            up_to_sequence: up_to,
            acked_at_ms: 9_100,
        })
        .expect("channel ack");
}

fn backlog_of(harness: &Harness, topic: &TopicRecord) -> (u64, u64) {
    let queue = harness
        .channel
        .inspect_queue(topic.channel_id)
        .expect("queue");
    (queue.backlog_bytes, queue.max_sequence)
}

#[test]
fn byte_bound_admits_exact_fill_and_rejects_one_payload_over() {
    // 40 retained bytes = exactly four 10-byte payloads.
    let (harness, topic) = bootstrap("byte-exact", 40, DAY_MS, 64);
    let _sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    // The subscriber never advances, so every publication is unconsumed
    // backlog: 10 + 10 + 10 + 10 == 40 is the exact fill.
    assert_eq!(publish_ok(&harness, &topic, 11, 4_000), 1);
    assert_eq!(publish_ok(&harness, &topic, 12, 4_100), 2);
    assert_eq!(publish_ok(&harness, &topic, 13, 4_200), 3);
    assert_eq!(publish_ok(&harness, &topic, 14, 4_300), 4);
    let (backlog, max_sequence) = backlog_of(&harness, &topic);
    assert_eq!((backlog, max_sequence), (40, 4));
    // One more payload projects to 50 > 40: rejected with the measured
    // values pinned against the declared bounds.
    let error = publish_err(&harness, &topic, 15, 4_400);
    let (retained_declared, backlog, payload, _retention_ms, _age) = expect_retention(error);
    assert_eq!(
        (retained_declared, backlog, payload),
        (40, 40, PAYLOAD as u64)
    );
    // Zero partial state: the journal still holds exactly the four admitted
    // publications.
    assert_eq!(
        harness
            .topics
            .inspect_publications(topic.topic_id)
            .expect("publications")
            .len(),
        4
    );
}

#[test]
fn byte_bound_releases_at_the_compact_bound_release_point() {
    let (harness, topic) = bootstrap("byte-release", 40, DAY_MS, 64);
    let sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    for (seed, at) in [(21_u8, 4_000_u64), (22, 4_100), (23, 4_200), (24, 4_300)] {
        publish_ok(&harness, &topic, seed, at);
    }
    let _ = publish_err(&harness, &topic, 25, 4_400);
    // The subscriber catches up completely: the topic cursors are past every
    // entry, but the channel consume high-water has not moved, so the
    // release point `min(cursors, consume high-water)` is still 0 and the
    // backlog is unchanged.
    advance(&harness, &topic, &sub, 4);
    let error = publish_err(&harness, &topic, 26, 4_500);
    let (_, backlog, _, _, _) = expect_retention(error);
    assert_eq!(backlog, 40);
    // The channel-level ack moves the consume high-water past the same
    // entries: the release point becomes 4 and the backlog drops to zero.
    ack(&harness, &topic, 4);
    assert_eq!(backlog_of(&harness, &topic).0, 0);
    assert_eq!(publish_ok(&harness, &topic, 26, 4_600), 5);
}

#[test]
fn byte_bound_without_subscribers_uses_channel_consume_high_water() {
    // No subscriber exists: the consume high-water alone is the backlog
    // bound (the same trade-off as compact_bound).
    let (harness, topic) = bootstrap("byte-no-sub", 20, DAY_MS, 64);
    assert_eq!(publish_ok(&harness, &topic, 31, 4_000), 1);
    assert_eq!(publish_ok(&harness, &topic, 32, 4_100), 2);
    let error = publish_err(&harness, &topic, 33, 4_200);
    let (_, backlog, _, _, _) = expect_retention(error);
    assert_eq!(backlog, 20);
    // Acking the whole log releases the backlog and publishing resumes.
    ack(&harness, &topic, 2);
    assert_eq!(backlog_of(&harness, &topic).0, 0);
    assert_eq!(publish_ok(&harness, &topic, 34, 4_300), 3);
}

#[test]
fn time_bound_admits_within_retention_and_rejects_expired_held_entry() {
    let (harness, topic) = bootstrap("time-bound", 4_096, 1_000, 64);
    let _sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    assert_eq!(publish_ok(&harness, &topic, 41, 5_000), 1);
    // Oldest held entry is 500ms old: inside the declared budget.
    assert_eq!(publish_ok(&harness, &topic, 42, 5_500), 2);
    // Exactly at the bound: 1000ms is not older than retention_ms.
    assert_eq!(publish_ok(&harness, &topic, 44, 6_000), 3);
    // 1200ms past the oldest held entry (enqueued at 5000): rejected.  The
    // byte numbers are far inside the bound, so the time bound is the
    // trigger.
    let error = publish_err(&harness, &topic, 43, 6_200);
    let (retained_declared, backlog, payload, retention_declared, age) = expect_retention(error);
    assert_eq!(
        (retained_declared, backlog, payload, retention_declared, age),
        (4_096, 30, PAYLOAD as u64, 1_000, 1_200)
    );
    // Zero partial state: the rejected key is not in the journal.
    assert_eq!(
        harness
            .topics
            .inspect_publications(topic.topic_id)
            .expect("publications")
            .len(),
        3
    );
}

#[test]
fn time_bound_releases_when_the_subscriber_advances_past_the_oldest() {
    let (harness, topic) = bootstrap("time-release", 4_096, 1_000, 64);
    let sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    assert_eq!(publish_ok(&harness, &topic, 51, 5_000), 1);
    assert_eq!(publish_ok(&harness, &topic, 52, 5_500), 2);
    // Oldest held entry (seq 1, enqueued 5000) is 1600ms old at now=6600.
    let _ = publish_err(&harness, &topic, 53, 6_600);
    // Advancing past the oldest entry only promotes the next one: it is
    // 1100ms old at the same now, still beyond the budget.
    advance(&harness, &topic, &sub, 1);
    let error = publish_err(&harness, &topic, 54, 6_600);
    let (_, _, _, _, age) = expect_retention(error);
    assert_eq!(age, 1_100);
    // Catching up fully releases the time bound: the same now admits.
    advance(&harness, &topic, &sub, 2);
    assert_eq!(publish_ok(&harness, &topic, 55, 6_600), 3);
}

#[test]
fn republish_child_enqueue_enforces_retention_without_spending_parent_budget() {
    let harness = Harness::new("republish-retention");
    let parent_channel = create_channel(&harness, 1);
    let child_channel = create_channel(&harness, 2);
    let parent = create_topic(
        &harness,
        parent_channel.channel_id,
        b"parent",
        policy(4_096, DAY_MS, 64),
        3,
    );
    // The child declares room for exactly two payloads.
    let child = create_topic(
        &harness,
        child_channel.channel_id,
        b"child",
        policy(20, DAY_MS, 64),
        4,
    );
    assert_eq!(publish_ok(&harness, &parent, 61, 4_000), 1);
    let child_sub = subscribe(&harness, child.topic_id, 2, 3_000);
    assert_eq!(publish_ok(&harness, &child, 62, 4_100), 1);
    assert_eq!(publish_ok(&harness, &child, 63, 4_200), 2);
    // The child backlog is exactly at its declared bound: the forwarded
    // payload does not fit.
    let error = republish_err(&harness, &child, key(61), 64, 5_000);
    let (retained_declared, backlog, payload, _, _) = expect_retention(error);
    assert_eq!(
        (retained_declared, backlog, payload),
        (20, 20, PAYLOAD as u64)
    );
    // The rejection happened before the budget CAS: the parent budget is
    // untouched and the parent row unchanged.
    let parent_publication = harness
        .topics
        .inspect_publication(key(61))
        .expect("parent publication");
    assert_eq!(parent_publication.cascade_budget_remaining, 2);
    assert_eq!(
        harness
            .topics
            .inspect_publications(child.topic_id)
            .expect("child publications")
            .len(),
        2
    );
    // Free the child (subscriber catch-up plus the channel consume point
    // move the release point past the held entries) and retry the same key:
    // it now completes and spends exactly one unit.
    advance(&harness, &child, &child_sub, 2);
    ack(&harness, &child, 2);
    let child_publication = republish_ok(&harness, &child, key(61), 64, 5_100);
    assert_eq!(child_publication.channel_sequence, 3);
    assert_eq!(child_publication.cascade_level, 1);
    assert_eq!(child_publication.parent_publication_key, Some(key(61)));
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(61))
            .expect("parent publication after spend")
            .cascade_budget_remaining,
        1
    );
    assert_eq!(
        harness
            .topics
            .inspect_publications(child.topic_id)
            .expect("child publications after retry")
            .len(),
        3
    );
}

#[test]
fn rejected_publish_leaves_zero_state_and_same_key_retry_is_idempotent() {
    let (harness, topic) = bootstrap("zero-partial", 20, DAY_MS, 64);
    let sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    assert_eq!(publish_ok(&harness, &topic, 71, 4_000), 1);
    assert_eq!(publish_ok(&harness, &topic, 72, 4_100), 2);
    let _ = publish_err(&harness, &topic, 73, 4_200);
    // The rejected call wrote nothing: no pending row, no publication record.
    assert!(matches!(
        harness.topics.inspect_publication(key(73)),
        Err(TopicAuthorityError::PublicationNotFound(_))
    ));
    assert_eq!(
        harness
            .topics
            .inspect_publications(topic.topic_id)
            .expect("publications")
            .len(),
        2
    );
    // Once the backlog is released — the subscriber catches up and the
    // channel consume point moves past the same entries, so the release
    // point is sequence 2 — the same key retries successfully and lands
    // exactly one entry.
    advance(&harness, &topic, &sub, 2);
    ack(&harness, &topic, 2);
    assert_eq!(publish_ok(&harness, &topic, 73, 4_300), 3);
    let (_, max_sequence) = backlog_of(&harness, &topic);
    assert_eq!(max_sequence, 3);
    // Replaying the same key returns the original record without a second
    // enqueue: zero-partial-state rejection and idempotency coexist.
    let replayed = match harness
        .topics
        .publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: vec![73; PAYLOAD],
            idempotency_key: key(73),
            published_at_ms: 4_400,
        })
        .expect("replay publish")
    {
        PublishDecision::Replayed(record) => record,
        PublishDecision::Published(_) => panic!("replayed key cannot publish again"),
    };
    assert_eq!(replayed.channel_sequence, 3);
    assert_eq!(backlog_of(&harness, &topic).1, 3);
    assert_eq!(
        harness
            .topics
            .inspect_publications(topic.topic_id)
            .expect("publications after replay")
            .len(),
        3
    );
}

#[test]
fn quarantined_subscriber_lag_holds_no_retention_budget() {
    // One delivery attempt: the first lagging publication flips the
    // subscriber QUARANTINED.
    let (harness, topic) = bootstrap("quarantine-orthogonal", 20, 1_000, 1);
    let _slow = subscribe(&harness, topic.topic_id, 1, 3_000);
    assert_eq!(publish_ok(&harness, &topic, 81, 5_000), 1);
    assert_eq!(publish_ok(&harness, &topic, 82, 5_100), 2);
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect slow");
    assert_eq!(record.state, SubscriptionState::Quarantined);
    assert_eq!(record.cursor, 0);
    assert!(matches!(
        harness.topics.poll(topic.topic_id, subscriber(1), 10),
        Err(TopicAuthorityError::DeliveryQuarantined(_))
    ));
    // The channel consume point moves past the entries the quarantined
    // subscriber still holds.  Its lag is excluded from the backlog (it no
    // longer receives deliveries, so it holds no retention budget), so the
    // publish is admitted even though the held entries are far older than
    // retention_ms — with the quarantine counted, both bounds would reject.
    ack(&harness, &topic, 2);
    assert_eq!(publish_ok(&harness, &topic, 83, 8_000), 3);
    // Orthogonality in the other direction: exclusion is not reinstatement.
    // The quarantined row keeps its state, cursor and counter, and nothing
    // was deleted.
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect slow after publish");
    assert_eq!(record.state, SubscriptionState::Quarantined);
    assert_eq!(record.cursor, 0);
    assert_eq!(record.redelivery_used, 1);
    assert_eq!(backlog_of(&harness, &topic).1, 3);
}

#[test]
fn subscriber_behind_the_channel_consume_point_uses_the_conservative_bound() {
    let (harness, topic) = bootstrap("conservative", 10, DAY_MS, 64);
    let sub = subscribe(&harness, topic.topic_id, 1, 3_000);
    assert_eq!(publish_ok(&harness, &topic, 91, 4_000), 1);
    // The channel consume point advances past the entry while the live
    // subscriber still sits behind it: the bytes of that window are not
    // exposed below the consume point, so the total live retained bytes
    // stand in as the (fail-closed) backlog and the publish is rejected.
    ack(&harness, &topic, 1);
    let error = publish_err(&harness, &topic, 92, 4_100);
    let (retained_declared, backlog, payload, _, _) = expect_retention(error);
    assert_eq!(
        (retained_declared, backlog, payload),
        (10, 10, PAYLOAD as u64)
    );
    // Once the subscriber catches up past the consume point, the release
    // point is the consume high-water again and the true (zero) backlog
    // applies.
    advance(&harness, &topic, &sub, 1);
    assert_eq!(publish_ok(&harness, &topic, 92, 4_200), 2);
}
