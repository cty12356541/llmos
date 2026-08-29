#![allow(deprecated)] // Token-free advance/unsubscribe deprecated in favor of the *_with_token entries.
use nlos_channel::{
    AckRequest, ChannelAuthority, ChannelAuthorityError, ChannelDecision, ChannelRecord,
    CreateChannelRequest, EnqueueDecision, EnqueueRequest, QueueState, RotateChannelRequest,
};
use nlos_topic::{
    AdvanceDecision, AdvanceReceipt, AdvanceRequest, CreateTopicRequest, PublicationRecord,
    PublicationStatus, PublishDecision, PublishRequest, SubscribeDecision, SubscribeRequest,
    SubscriberKey, SubscriptionRecord, TopicAuthority, TopicAuthorityError, TopicCompactDecision,
    TopicDecision, TopicPolicy, TopicRecord, UnsubscribeDecision, UnsubscribeRequest,
};
use nlos_types::{ChannelId, IdempotencyKey, ResourceAccountId};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
            "nlos-topic-svc-{label}-{}-{nonce}-{sequence}",
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

fn policy_for(max_recipients: u64) -> TopicPolicy {
    TopicPolicy {
        max_recipients,
        delivery_attempts: 3,
        cascade_depth: 2,
        retained_bytes: 4_096,
        retention_ms: 86_400_000,
        payer: payer(7),
    }
}

fn create_channel(channel: &ChannelAuthority, capacity_bytes: u64, seed: u8) -> ChannelRecord {
    match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes,
            policy_digest: [0x44; 32],
            idempotency_key: key(seed),
            created_at_ms: 1_000,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

fn create_topic(
    topics: &TopicAuthority,
    channel_id: ChannelId,
    name: &[u8],
    policy: TopicPolicy,
    seed: u8,
) -> TopicRecord {
    match topics
        .create_topic(CreateTopicRequest {
            channel_id,
            name: name.to_vec(),
            policy,
            idempotency_key: key(seed),
            created_at_ms: 2_000,
        })
        .expect("create topic")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh topic cannot replay"),
    }
}

fn subscribe_at(
    topics: &TopicAuthority,
    topic_id: nlos_topic::TopicId,
    seed: u8,
    at: u64,
) -> SubscriptionRecord {
    match topics
        .subscribe(SubscribeRequest {
            topic_id,
            subscriber_key: subscriber(seed),
            subscribed_at_ms: at,
        })
        .expect("subscribe")
    {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    }
}

fn publish_at(
    topics: &TopicAuthority,
    topic_id: nlos_topic::TopicId,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> PublicationRecord {
    match topics
        .publish(PublishRequest {
            topic_id,
            payload: payload.to_vec(),
            idempotency_key: key(seed),
            published_at_ms: at,
        })
        .expect("publish")
    {
        PublishDecision::Published(record) => record,
        PublishDecision::Replayed(_) => panic!("fresh publish cannot replay"),
    }
}

fn direct_enqueue(
    channel: &ChannelAuthority,
    head: &ChannelRecord,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> nlos_channel::QueueEntryRecord {
    match channel
        .enqueue(EnqueueRequest {
            channel_id: head.channel_id,
            expected_generation: head.generation,
            expected_fencing_token: head.fencing_token,
            payload: payload.to_vec(),
            idempotency_key: key(seed),
            enqueued_at_ms: at,
        })
        .expect("direct enqueue")
    {
        EnqueueDecision::Enqueued(entry) => entry,
        EnqueueDecision::Replayed(_) => panic!("fresh direct enqueue cannot replay"),
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full create validation and replay matrix.
fn create_topic_validates_policy_and_replays_idempotently() {
    let harness = Harness::new("create");
    let head = create_channel(&harness.channel, 1_024, 200);

    // The channel must exist and be readable through the owner readback
    // before any durable topic write.
    let unknown = ChannelId::from_bytes([0x00; 16]);
    assert!(matches!(
        harness.topics.create_topic(CreateTopicRequest {
            channel_id: unknown,
            name: b"ghost".to_vec(),
            policy: policy_for(4),
            idempotency_key: key(10),
            created_at_ms: 2_000,
        }),
        Err(TopicAuthorityError::Channel(
            ChannelAuthorityError::ChannelNotFound(_)
        ))
    ));

    // Missing payer and every out-of-range declaration fail closed pre-write.
    let base = policy_for(4);
    let cases: [(&'static str, TopicPolicy); 5] = [
        (
            "max_recipients",
            TopicPolicy {
                max_recipients: 0,
                ..base
            },
        ),
        (
            "delivery_attempts",
            TopicPolicy {
                delivery_attempts: 0,
                ..base
            },
        ),
        (
            "cascade_depth",
            TopicPolicy {
                cascade_depth: 0,
                ..base
            },
        ),
        (
            "retained_bytes",
            TopicPolicy {
                retained_bytes: 0,
                ..base
            },
        ),
        (
            "retention_ms",
            TopicPolicy {
                retention_ms: 0,
                ..base
            },
        ),
    ];
    for (label, policy) in cases {
        let error = harness
            .topics
            .create_topic(CreateTopicRequest {
                channel_id: head.channel_id,
                name: b"range".to_vec(),
                policy,
                idempotency_key: key(11),
                created_at_ms: 2_000,
            })
            .expect_err(label);
        assert!(
            matches!(error, TopicAuthorityError::InvalidPolicy(_)),
            "{label}: {error:?}"
        );
    }
    assert!(matches!(
        harness.topics.create_topic(CreateTopicRequest {
            channel_id: head.channel_id,
            name: Vec::new(),
            policy: base,
            idempotency_key: key(11),
            created_at_ms: 2_000,
        }),
        Err(TopicAuthorityError::InvalidPolicy(_))
    ));
    assert!(matches!(
        harness.topics.create_topic(CreateTopicRequest {
            channel_id: head.channel_id,
            name: b"unbound".to_vec(),
            policy: TopicPolicy {
                payer: payer(0),
                ..base
            },
            idempotency_key: key(11),
            created_at_ms: 2_000,
        }),
        Err(TopicAuthorityError::InvalidPolicy(_))
    ));

    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"telemetry.events",
        base,
        21,
    );
    assert_eq!(topic.channel_generation, head.generation);
    assert_eq!(topic.channel_fencing_token, head.fencing_token);
    assert_eq!(topic.active_subscriptions, 0);

    // Exact replay returns the original record (the fence snapshot is
    // authority state and is not a replay-compare field).
    assert_eq!(
        harness
            .topics
            .create_topic(CreateTopicRequest {
                channel_id: head.channel_id,
                name: b"telemetry.events".to_vec(),
                policy: base,
                idempotency_key: key(21),
                created_at_ms: 9_999,
            })
            .expect("replay create"),
        TopicDecision::Replayed(topic.clone())
    );

    // The same key rebound to drifted policy, name or channel conflicts.
    assert!(matches!(
        harness.topics.create_topic(CreateTopicRequest {
            channel_id: head.channel_id,
            name: b"telemetry.events".to_vec(),
            policy: TopicPolicy {
                retained_bytes: 8_192,
                ..base
            },
            idempotency_key: key(21),
            created_at_ms: 2_000,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    assert!(matches!(
        harness.topics.create_topic(CreateTopicRequest {
            channel_id: head.channel_id,
            name: b"telemetry.other".to_vec(),
            policy: base,
            idempotency_key: key(21),
            created_at_ms: 2_000,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));

    // The TopicId is authority-derived from (channel, name): re-deriving the
    // same pair under a different key collides and fails closed.
    assert!(matches!(
        harness.topics.create_topic(CreateTopicRequest {
            channel_id: head.channel_id,
            name: b"telemetry.events".to_vec(),
            policy: base,
            idempotency_key: key(22),
            created_at_ms: 2_000,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));

    // A different pair under the same key is a drift conflict, and a
    // different name under a new key creates a distinct topic.
    assert!(matches!(
        harness.topics.create_topic(CreateTopicRequest {
            channel_id: head.channel_id,
            name: b"telemetry.metrics".to_vec(),
            policy: base,
            idempotency_key: key(21),
            created_at_ms: 2_000,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    let sibling = create_topic(
        &harness.topics,
        head.channel_id,
        b"telemetry.metrics",
        base,
        23,
    );
    assert_ne!(sibling.topic_id, topic.topic_id);
    assert_eq!(
        harness
            .topics
            .inspect_topic(topic.topic_id)
            .expect("inspect topic"),
        topic
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers admission, idempotency and min-cursor moves.
fn subscribe_enforces_limit_and_unsubscribe_changes_min_cursor() {
    let harness = Harness::new("subscribe");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"fanout",
        policy_for(2),
        31,
    );

    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 3_000);
    assert_eq!(a.cursor, 0);
    assert_eq!(a.subscription_id, {
        let bytes = a.subscription_id.into_bytes();
        nlos_topic::SubscriptionId::from_bytes(bytes)
    });
    publish_at(&harness.topics, topic.topic_id, 41, b"one", 3_100);
    publish_at(&harness.topics, topic.topic_id, 42, b"two", 3_101);
    publish_at(&harness.topics, topic.topic_id, 43, b"three", 3_102);

    // B joins after three publications: its cursor starts at the subscribe
    // point and history is never replayed to it.
    let b = subscribe_at(&harness.topics, topic.topic_id, 2, 3_200);
    assert_eq!(b.cursor, 3);
    assert_eq!(
        harness
            .topics
            .inspect_topic(topic.topic_id)
            .expect("count")
            .active_subscriptions,
        2
    );

    // Admission is bounded by the declared max_recipients.
    assert!(matches!(
        harness.topics.subscribe(SubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(3),
            subscribed_at_ms: 3_300,
        }),
        Err(TopicAuthorityError::SubscriberLimitReached)
    ));

    // Subscribing an active key is idempotent, including the stored time.
    // The stored record has since been billed twice by the three
    // publications that arrived while A lagged (the first publication finds
    // no backlog and is free); replay must return exactly that record.
    let a_stored = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("inspect lagging subscriber");
    assert_eq!(a_stored.redelivery_used, 2);
    assert_eq!(
        harness
            .topics
            .subscribe(SubscribeRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(1),
                subscribed_at_ms: 9_999,
            })
            .expect("replay subscribe"),
        SubscribeDecision::Replayed(a_stored)
    );

    harness
        .topics
        .advance(AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            up_to_sequence: 1,
            advanced_at_ms: 3_400,
        })
        .expect("advance slow subscriber");
    harness
        .channel
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 3,
            acked_at_ms: 3_500,
        })
        .expect("external channel ack");

    // The min-live-cursor bound covers the slow subscriber's cursor.
    assert_eq!(
        harness
            .topics
            .compact_bound(topic.topic_id)
            .expect("bound with slow subscriber"),
        1
    );

    // Unsubscribe flips the state row and lifts the bound to B's cursor.
    assert_eq!(
        harness
            .topics
            .unsubscribe(UnsubscribeRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(1),
                unsubscribed_at_ms: 3_600,
            })
            .expect("unsubscribe"),
        UnsubscribeDecision::Unsubscribed(nlos_topic::UnsubscribeReceipt {
            subscription_id: a.subscription_id,
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            unsubscribed_at_ms: 3_600,
        })
    );
    assert!(
        !harness
            .topics
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("inspect inactive")
            .active
    );
    assert_eq!(
        harness
            .topics
            .compact_bound(topic.topic_id)
            .expect("bound without unsubscribed subscriber"),
        3
    );

    // Re-subscribing re-activates the row at a fresh subscribe point and
    // re-occupies the admission slot.
    let resumed = subscribe_at(&harness.topics, topic.topic_id, 1, 3_700);
    assert_eq!(resumed.cursor, 3);
    assert!(resumed.active);
    assert!(matches!(
        harness.topics.subscribe(SubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(3),
            subscribed_at_ms: 3_800,
        }),
        Err(TopicAuthorityError::SubscriberLimitReached)
    ));

    // Unsubscribe replays the original receipt.
    assert!(matches!(
        harness.topics.unsubscribe(UnsubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(2),
            unsubscribed_at_ms: 3_900,
        }),
        Ok(UnsubscribeDecision::Unsubscribed(_))
    ));
    assert!(matches!(
        harness.topics.unsubscribe(UnsubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(2),
            unsubscribed_at_ms: 9_999,
        }),
        Ok(UnsubscribeDecision::Replayed(_))
    ));
    assert!(matches!(
        harness.topics.unsubscribe(UnsubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(9),
            unsubscribed_at_ms: 3_901,
        }),
        Err(TopicAuthorityError::SubscriptionNotFound(_))
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers exactly-once enqueue plus replay and drift.
fn publish_enqueues_exactly_once_and_replays_without_duplicates() {
    let harness = Harness::new("publish");
    let head = create_channel(&harness.channel, 1_024, 200);
    let policy = policy_for(8);
    let topic = create_topic(&harness.topics, head.channel_id, b"publish", policy, 51);
    let sibling = create_topic(
        &harness.topics,
        head.channel_id,
        b"publish.other",
        policy,
        52,
    );

    let first = publish_at(&harness.topics, topic.topic_id, 61, b"alpha", 5_000);
    assert_eq!(first.status, PublicationStatus::Enqueued);
    assert_eq!(first.channel_sequence, 1);
    assert_eq!(first.channel_generation, head.generation.get());
    assert_eq!(first.cascade_budget_remaining, policy.cascade_depth);
    assert_eq!(first.payer, policy.payer);
    assert_eq!(first.policy_digest, topic.policy_digest);
    publish_at(&harness.topics, topic.topic_id, 62, b"beta", 5_001);

    // Exactly two durable queue entries exist on the channel.
    assert_eq!(
        harness
            .channel
            .inspect_queue(head.channel_id)
            .expect("queue"),
        QueueState {
            channel_id: head.channel_id,
            capacity_bytes: 1_024,
            consume_high_water: 0,
            trim_high_water: 0,
            backlog_bytes: 9,
            retained_bytes: 9,
            max_sequence: 2,
        }
    );

    // Replaying the exact request returns the original record and does not
    // enqueue again.
    assert_eq!(
        harness
            .topics
            .publish(PublishRequest {
                topic_id: topic.topic_id,
                payload: b"alpha".to_vec(),
                idempotency_key: key(61),
                published_at_ms: 9_999,
            })
            .expect("publish replay"),
        PublishDecision::Replayed(first.clone())
    );
    assert_eq!(
        harness
            .channel
            .inspect_queue(head.channel_id)
            .expect("queue after replay")
            .max_sequence,
        2
    );

    // A key rebound to a different payload or topic is a drift conflict.
    assert!(matches!(
        harness.topics.publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: b"gamma".to_vec(),
            idempotency_key: key(61),
            published_at_ms: 5_002,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    assert!(matches!(
        harness.topics.publish(PublishRequest {
            topic_id: sibling.topic_id,
            payload: b"alpha".to_vec(),
            idempotency_key: key(61),
            published_at_ms: 5_002,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    assert!(matches!(
        harness.topics.publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: Vec::new(),
            idempotency_key: key(63),
            published_at_ms: 5_002,
        }),
        Err(TopicAuthorityError::InvalidPayload)
    ));

    let publications = harness
        .topics
        .inspect_publications(topic.topic_id)
        .expect("publications");
    assert_eq!(publications.len(), 2);
    assert!(publications.contains(&first));
    assert!(
        harness
            .topics
            .inspect_publications(sibling.topic_id)
            .expect("sibling publications")
            .is_empty()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers both PENDING_ENQUEUE crash-window sub-cases.
fn pending_enqueue_crash_window_converges_without_duplicates() {
    let harness = Harness::new("crash-window");
    let head = create_channel(&harness.channel, 4, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"crash",
        policy_for(4),
        71,
    );

    direct_enqueue(&harness.channel, &head, 81, b"aa", 6_000);
    direct_enqueue(&harness.channel, &head, 82, b"cc", 6_001);

    // The channel is at capacity: the publish's enqueue is rejected, the
    // typed error propagates and the publication row stays PENDING_ENQUEUE.
    assert!(matches!(
        harness.topics.publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: b"bb".to_vec(),
            idempotency_key: key(83),
            published_at_ms: 6_002,
        }),
        Err(TopicAuthorityError::Channel(
            ChannelAuthorityError::QueueFull
        ))
    ));
    let pending = harness
        .topics
        .inspect_publications(topic.topic_id)
        .expect("pending row");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, PublicationStatus::PendingEnqueue);
    assert_eq!(pending[0].channel_sequence, 0);

    // Crash window: the enqueue actually completed (simulated by the direct
    // owner enqueue under the same idempotency key after capacity drained)
    // but the ENQUEUED commit was lost.  The replay converges onto the
    // existing entry instead of duplicating it.
    harness
        .channel
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 2,
            acked_at_ms: 6_100,
        })
        .expect("drain capacity");
    direct_enqueue(&harness.channel, &head, 83, b"bb", 6_002);
    let converged = publish_at(&harness.topics, topic.topic_id, 83, b"bb", 6_003);
    assert_eq!(converged.status, PublicationStatus::Enqueued);
    assert_eq!(converged.channel_sequence, 3);
    assert_eq!(
        harness
            .channel
            .inspect_queue(head.channel_id)
            .expect("no duplicate enqueue")
            .max_sequence,
        3
    );
    assert_eq!(
        harness
            .topics
            .publish(PublishRequest {
                topic_id: topic.topic_id,
                payload: b"bb".to_vec(),
                idempotency_key: key(83),
                published_at_ms: 6_004,
            })
            .expect("replay converged publication"),
        PublishDecision::Replayed(converged)
    );

    // The other crash sub-case: the enqueue never happened.  The replay
    // supplements it (补投) as a fresh entry.
    assert!(matches!(
        harness.topics.publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: b"ddd".to_vec(),
            idempotency_key: key(84),
            published_at_ms: 6_005,
        }),
        Err(TopicAuthorityError::Channel(
            ChannelAuthorityError::QueueFull
        ))
    ));
    harness
        .channel
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 3,
            acked_at_ms: 6_200,
        })
        .expect("drain again");
    let supplemented = publish_at(&harness.topics, topic.topic_id, 84, b"ddd", 6_006);
    assert_eq!(supplemented.channel_sequence, 4);
    let state = harness
        .channel
        .inspect_queue(head.channel_id)
        .expect("final queue");
    assert_eq!(state.max_sequence, 4);
    // Only the supplemented "ddd" (3 bytes) sits beyond the consume
    // high-water; the consumed prefix stays retained until compaction.
    assert_eq!(state.backlog_bytes, 3);

    let publications = harness
        .topics
        .inspect_publications(topic.topic_id)
        .expect("publications");
    assert_eq!(publications.len(), 2);
    assert!(publications.iter().all(|record| {
        record.status == PublicationStatus::Enqueued && record.channel_sequence >= 3
    }));
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers per-subscriber isolation and cursor CAS bounds.
fn poll_and_advance_isolate_slow_subscribers() {
    let harness = Harness::new("isolation");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"isolation",
        policy_for(8),
        91,
    );

    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 7_000);
    publish_at(&harness.topics, topic.topic_id, 92, b"one", 7_001);
    publish_at(&harness.topics, topic.topic_id, 93, b"two", 7_002);
    let b = subscribe_at(&harness.topics, topic.topic_id, 2, 7_003);
    assert_eq!(b.cursor, 2);
    publish_at(&harness.topics, topic.topic_id, 94, b"three", 7_004);

    // A's lag does not hide anything from B, and vice versa: both filter the
    // shared receive window with their own cursors only.
    let window_a = harness
        .topics
        .poll(topic.topic_id, subscriber(1), 10)
        .expect("poll slow subscriber");
    assert_eq!(
        window_a
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let window_b = harness
        .topics
        .poll(topic.topic_id, subscriber(2), 10)
        .expect("poll fast subscriber");
    assert_eq!(
        window_b
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        [3]
    );

    // Poll is zero-write: repeating it returns the same window.
    assert_eq!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(1), 10)
            .expect("poll again"),
        window_a
    );

    // B advances to the head; A's window is unaffected by B's progress.
    assert_eq!(
        harness
            .topics
            .advance(AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(2),
                up_to_sequence: 3,
                advanced_at_ms: 7_100,
            })
            .expect("advance fast subscriber"),
        AdvanceDecision::Advanced(AdvanceReceipt {
            subscription_id: b.subscription_id,
            topic_id: topic.topic_id,
            subscriber_key: subscriber(2),
            cursor: 3,
            advanced_at_ms: 7_100,
        })
    );
    assert!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(2), 10)
            .expect("fast subscriber drained")
            .is_empty()
    );
    assert_eq!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(1), 10)
            .expect("slow subscriber unaffected")
            .len(),
        3
    );

    // Same-value advance replays the original receipt (stored timestamp);
    // regression and over-range advances fail closed.
    assert_eq!(
        harness
            .topics
            .advance(AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(2),
                up_to_sequence: 3,
                advanced_at_ms: 9_999,
            })
            .expect("advance replay"),
        AdvanceDecision::Replayed(AdvanceReceipt {
            subscription_id: b.subscription_id,
            topic_id: topic.topic_id,
            subscriber_key: subscriber(2),
            cursor: 3,
            advanced_at_ms: 7_100,
        })
    );
    assert!(matches!(
        harness.topics.advance(AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(2),
            up_to_sequence: 2,
            advanced_at_ms: 7_101,
        }),
        Err(TopicAuthorityError::InvalidSequence(_))
    ));
    assert!(matches!(
        harness.topics.advance(AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            up_to_sequence: 4,
            advanced_at_ms: 7_102,
        }),
        Err(TopicAuthorityError::InvalidSequence(_))
    ));

    // A advances within range; its own window shrinks accordingly.
    harness
        .topics
        .advance(AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            up_to_sequence: 2,
            advanced_at_ms: 7_200,
        })
        .expect("advance slow subscriber");
    assert_eq!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(1), 10)
            .expect("slow subscriber tail")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        [3]
    );

    // Polling an inactive or unknown subscription fails closed.
    harness
        .topics
        .unsubscribe(UnsubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            unsubscribed_at_ms: 7_300,
        })
        .expect("unsubscribe slow subscriber");
    assert!(matches!(
        harness.topics.poll(topic.topic_id, subscriber(1), 10),
        Err(TopicAuthorityError::SubscriptionInactive(_))
    ));
    assert!(matches!(
        harness.topics.poll(topic.topic_id, subscriber(9), 10),
        Err(TopicAuthorityError::SubscriptionNotFound(_))
    ));
    let _ = a;
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers clamping, replay and the no-subscriber bound.
fn compact_bound_clamps_to_min_live_subscriber_cursor() {
    let harness = Harness::new("compact");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"compact",
        policy_for(8),
        101,
    );

    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"one", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"two", 8_002);
    publish_at(&harness.topics, topic.topic_id, 104, b"three", 8_003);
    let b = subscribe_at(&harness.topics, topic.topic_id, 2, 8_004);
    assert_eq!((a.cursor, b.cursor), (0, 3));

    harness
        .topics
        .advance(AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            up_to_sequence: 2,
            advanced_at_ms: 8_100,
        })
        .expect("advance A");

    // The channel owner consumes everything, but the service-layer bound
    // still refuses to trim past the min live subscriber cursor.
    harness
        .channel
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 3,
            acked_at_ms: 8_200,
        })
        .expect("external channel ack");
    assert_eq!(
        harness
            .topics
            .compact_bound(topic.topic_id)
            .expect("bound clamped to live cursor"),
        2
    );

    let trimmed = harness
        .topics
        .compact(topic.topic_id, 3)
        .expect("compact clamped");
    assert!(matches!(trimmed, TopicCompactDecision::Trimmed(_)));
    assert_eq!(trimmed.receipt().effective_trim_high_water, 2);
    assert_eq!(trimmed.receipt().channel.trim_high_water, 2);

    // Entry 3 is hidden by the shared channel consume high-water (the
    // documented single-high-water limitation of this slice), while the live
    // tail beyond it stays pollable for the slow subscriber.
    assert!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(1), 10)
            .expect("poll consumed tail")
            .is_empty()
    );
    // A catches up past the hidden entry before the next publication: lag
    // billing (delivery attempts) charges subscribers that are behind when a
    // publication lands, and an unconsumed seq 3 would bill A a third time
    // and quarantine it at the declared budget of three.
    harness
        .topics
        .advance(AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            up_to_sequence: 3,
            advanced_at_ms: 8_240,
        })
        .expect("A catches up before the next publication");
    publish_at(&harness.topics, topic.topic_id, 105, b"four", 8_250);
    assert_eq!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(1), 10)
            .expect("poll live tail after compact")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        [4]
    );

    // Repeating the same effective watermark replays; the channel compact
    // semantics (clamped, idempotent) are preserved through the service.
    // (A has since caught up to 3, so requesting 3 again would be a fresh
    // trim; the already-applied watermark 2 still replays.)
    assert_eq!(
        harness
            .topics
            .compact(topic.topic_id, 2)
            .expect("compact replay"),
        TopicCompactDecision::Replayed(nlos_topic::TopicCompactReceipt {
            topic_id: topic.topic_id,
            channel_id: head.channel_id,
            effective_trim_high_water: 2,
            channel: nlos_channel::CompactReceipt {
                channel_id: head.channel_id,
                trim_high_water: 2,
            },
        })
    );

    // With no active subscribers the bound falls back to the channel consume
    // high-water (the documented trade-off of this slice).
    harness
        .topics
        .unsubscribe(UnsubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            unsubscribed_at_ms: 8_300,
        })
        .expect("unsubscribe A");
    harness
        .topics
        .unsubscribe(UnsubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(2),
            unsubscribed_at_ms: 8_301,
        })
        .expect("unsubscribe B");
    assert_eq!(
        harness
            .topics
            .compact_bound(topic.topic_id)
            .expect("no-subscriber bound"),
        3
    );
    assert_eq!(
        harness
            .topics
            .compact(topic.topic_id, 9)
            .expect("trim rest"),
        TopicCompactDecision::Trimmed(nlos_topic::TopicCompactReceipt {
            topic_id: topic.topic_id,
            channel_id: head.channel_id,
            effective_trim_high_water: 3,
            channel: nlos_channel::CompactReceipt {
                channel_id: head.channel_id,
                trim_high_water: 3,
            },
        })
    );
    // The unconsumed entry 4 survives the trim and stays receivable by the
    // channel owner; draining it consumes the log.
    assert_eq!(
        harness
            .channel
            .receive(head.channel_id, 10)
            .expect("live tail receivable")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        [4]
    );
    harness
        .channel
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 4,
            acked_at_ms: 8_400,
        })
        .expect("drain tail");
    assert!(
        harness
            .channel
            .receive(head.channel_id, 10)
            .expect("channel drained")
            .is_empty()
    );

    // An unknown topic fails closed.
    assert!(matches!(
        harness
            .topics
            .compact_bound(nlos_topic::TopicId::from_bytes([0x00; 16])),
        Err(TopicAuthorityError::TopicNotFound(_))
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers stale-fence propagation and re-read convergence.
fn stale_channel_after_rotation_propagates_then_rebound_retry_succeeds() {
    let harness = Harness::new("stale");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"stale",
        policy_for(4),
        111,
    );

    let rotated = match harness
        .channel
        .rotate_channel(RotateChannelRequest {
            channel_id: head.channel_id,
            expected_generation: head.generation,
            expected_fencing_token: head.fencing_token,
            idempotency_key: key(150),
            rotated_at_ms: 9_000,
        })
        .expect("rotate")
    {
        nlos_channel::ChannelRotationDecision::Rotated(record) => record,
        nlos_channel::ChannelRotationDecision::Replayed(_) => panic!("fresh rotate cannot replay"),
    };

    // The publish enqueues against the fence bound on the topic head, so the
    // rotation surfaces as a propagated StaleChannel with no silent retry.
    assert!(matches!(
        harness.topics.publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: b"stale".to_vec(),
            idempotency_key: key(112),
            published_at_ms: 9_100,
        }),
        Err(TopicAuthorityError::Channel(
            ChannelAuthorityError::StaleChannel
        ))
    ));
    let pending = harness
        .topics
        .inspect_publications(topic.topic_id)
        .expect("pending after stale");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, PublicationStatus::PendingEnqueue);
    assert_eq!(
        harness
            .channel
            .inspect_queue(head.channel_id)
            .expect("no silent enqueue")
            .max_sequence,
        0
    );

    // The retry re-reads the channel head live, re-binds the topic head
    // fence and converges the pending publication under the new generation.
    let resumed = publish_at(&harness.topics, topic.topic_id, 112, b"stale", 9_101);
    assert_eq!(resumed.status, PublicationStatus::Enqueued);
    assert_eq!(resumed.channel_sequence, 1);
    assert_eq!(resumed.channel_generation, rotated.generation.get());
    let head_after = harness
        .topics
        .inspect_topic(topic.topic_id)
        .expect("rebound topic head");
    assert_eq!(head_after.channel_generation, rotated.generation);
    assert_eq!(head_after.channel_fencing_token, rotated.fencing_token);

    // A fresh publish on the rebound head no longer hits the stale fence.
    let fresh = publish_at(&harness.topics, topic.topic_id, 113, b"fresh", 9_102);
    assert_eq!(fresh.channel_sequence, 2);
    let window = harness
        .channel
        .receive(head.channel_id, 10)
        .expect("receive both");
    assert_eq!(
        window
            .iter()
            .map(|entry| entry.generation)
            .collect::<Vec<_>>(),
        [rotated.generation, rotated.generation]
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full restart replay of the service lifecycle.
fn restart_replays_topics_subscriptions_cursors_and_publications() {
    let root = Root::new("restart");
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let topics =
        TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("open topic authority");
    let head = create_channel(&channel, 256, 200);
    let topic = create_topic(&topics, head.channel_id, b"restart", policy_for(4), 121);

    let (topic_before, subs_before, publications_before, poll_first_before, poll_second_before) = {
        subscribe_at(&topics, topic.topic_id, 1, 10_000);
        publish_at(&topics, topic.topic_id, 122, b"one", 10_001);
        publish_at(&topics, topic.topic_id, 123, b"two", 10_002);
        topics
            .advance(AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(1),
                up_to_sequence: 1,
                advanced_at_ms: 10_100,
            })
            .expect("advance A");
        let b = subscribe_at(&topics, topic.topic_id, 2, 10_003);
        publish_at(&topics, topic.topic_id, 124, b"three", 10_004);
        // Snapshot A only after the third publication: A was still lagging
        // when it landed, so the delivery-attempt billing charged A once more
        // (seq 2 and seq 3; seq 1 found no backlog and was free).
        let a = topics
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("inspect A after its advance");
        assert_eq!(a.redelivery_used, 2);
        let topic_record = topics.inspect_topic(topic.topic_id).expect("inspect topic");
        let subs = vec![
            topics
                .inspect_subscription(topic.topic_id, subscriber(1))
                .expect("inspect A"),
            topics
                .inspect_subscription(topic.topic_id, subscriber(2))
                .expect("inspect B"),
        ];
        let publications = topics
            .inspect_publications(topic.topic_id)
            .expect("publications");
        assert_eq!(publications.len(), 3);
        assert_eq!(subs[0], a);
        assert_eq!(subs[1], b);
        let poll_first = topics
            .poll(topic.topic_id, subscriber(1), 10)
            .expect("poll A");
        let poll_second = topics
            .poll(topic.topic_id, subscriber(2), 10)
            .expect("poll B");
        (topic_record, subs, publications, poll_first, poll_second)
    };

    // Simulate a restart: both authorities are dropped and reopened on the
    // same roots.  Every record replays field-for-field.
    drop(topics);
    drop(channel);
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("reopen channel authority"));
    let topics =
        TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("reopen topic authority");

    assert_eq!(
        topics
            .inspect_topic(topic.topic_id)
            .expect("topic after restart"),
        topic_before
    );
    assert_eq!(
        topics
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("A after restart"),
        subs_before[0]
    );
    assert_eq!(
        topics
            .inspect_subscription(topic.topic_id, subscriber(2))
            .expect("B after restart"),
        subs_before[1]
    );
    assert_eq!(
        topics
            .inspect_publications(topic.topic_id)
            .expect("publications after restart"),
        publications_before
    );
    assert_eq!(
        topics
            .poll(topic.topic_id, subscriber(1), 10)
            .expect("poll A after restart"),
        poll_first_before
    );
    assert_eq!(
        topics
            .poll(topic.topic_id, subscriber(2), 10)
            .expect("poll B after restart"),
        poll_second_before
    );

    // Decision replays stay consistent after the restart.
    assert_eq!(
        topics
            .advance(AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(1),
                up_to_sequence: 1,
                advanced_at_ms: 9_999,
            })
            .expect("advance replay"),
        AdvanceDecision::Replayed(AdvanceReceipt {
            subscription_id: subs_before[0].subscription_id,
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            cursor: 1,
            advanced_at_ms: 10_100,
        })
    );
    assert_eq!(
        topics
            .publish(PublishRequest {
                topic_id: topic.topic_id,
                payload: b"three".to_vec(),
                idempotency_key: key(124),
                published_at_ms: 9_999,
            })
            .expect("publish replay"),
        PublishDecision::Replayed(publications_before[2].clone())
    );
    // A fresh publish continues the channel's monotonic sequence.
    let continued = match topics
        .publish(PublishRequest {
            topic_id: topic.topic_id,
            payload: b"four".to_vec(),
            idempotency_key: key(125),
            published_at_ms: 10_005,
        })
        .expect("publish after restart")
    {
        PublishDecision::Published(record) => record,
        PublishDecision::Replayed(_) => panic!("fresh publish cannot replay"),
    };
    assert_eq!(continued.channel_sequence, 4);
    assert_eq!(
        channel
            .inspect_queue(head.channel_id)
            .expect("queue after restart")
            .max_sequence,
        4
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the tamper matrix and the DDL immutability guards.
fn tampered_state_fails_closed_and_ddl_guards_hold() {
    let harness = Harness::new("tamper");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"tamper",
        policy_for(4),
        131,
    );
    subscribe_at(&harness.topics, topic.topic_id, 1, 11_000);
    subscribe_at(&harness.topics, topic.topic_id, 2, 11_001);
    publish_at(&harness.topics, topic.topic_id, 132, b"one", 11_002);
    harness
        .topics
        .advance(AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(1),
            up_to_sequence: 1,
            advanced_at_ms: 11_100,
        })
        .expect("advance A");

    let raw = Connection::open(harness.root.topic_db()).expect("open raw topic db");
    let topic_id = topic.topic_id.as_bytes().as_slice();

    // (a) The stored active-subscription counter drifts from the rows.
    raw.execute(
        "UPDATE topics SET active_subscriptions=active_subscriptions+1 WHERE topic_id=?1",
        [topic_id],
    )
    .expect("tamper active count");
    assert!(matches!(
        harness.topics.inspect_topic(topic.topic_id),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));
    assert!(matches!(
        harness.topics.subscribe(SubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(3),
            subscribed_at_ms: 11_200,
        }),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE topics SET active_subscriptions=active_subscriptions-1 WHERE topic_id=?1",
        [topic_id],
    )
    .expect("restore active count");

    // (b) A subscriber cursor drifts beyond the channel high-water.
    raw.execute(
        "UPDATE topic_subscriptions SET cursor=cursor+100
         WHERE topic_id=?1 AND subscriber_key=x'02020202020202020202020202020202'",
        [topic_id],
    )
    .expect("tamper cursor");
    assert!(matches!(
        harness.topics.poll(topic.topic_id, subscriber(2), 10),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));
    assert!(matches!(
        harness.topics.advance(AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(2),
            up_to_sequence: 2,
            advanced_at_ms: 11_201,
        }),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE topic_subscriptions SET cursor=cursor-100
         WHERE topic_id=?1 AND subscriber_key=x'02020202020202020202020202020202'",
        [topic_id],
    )
    .expect("restore cursor");

    // (c) The immutability triggers reject direct mutation of identity,
    // policy, publication bindings and row deletion.
    assert!(
        raw.execute(
            "UPDATE topics SET topic_name=x'6e' WHERE topic_id=?1",
            [topic_id],
        )
        .is_err(),
        "topic identity is frozen"
    );
    assert!(
        raw.execute(
            "UPDATE topics SET policy_digest=x'0000000000000000000000000000000000000000000000000000000000000000' WHERE topic_id=?1",
            [topic_id],
        )
        .is_err(),
        "topic policy is frozen"
    );
    let rejection = raw
        .execute(
            "UPDATE topic_publications SET payload_digest=randomblob(32)",
            [],
        )
        .expect_err("publication is immutable");
    assert!(
        rejection.to_string().contains("immutable"),
        "unexpected rejection message: {rejection}"
    );
    assert!(raw.execute("DELETE FROM topics", []).is_err());
    assert!(raw.execute("DELETE FROM topic_subscriptions", []).is_err());
    assert!(raw.execute("DELETE FROM topic_publications", []).is_err());
    drop(raw);

    // The restored state serves reads again.
    assert_eq!(
        harness
            .topics
            .inspect_topic(topic.topic_id)
            .expect("topic after restore")
            .active_subscriptions,
        2
    );
    assert_eq!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(1), 10)
            .expect("poll after restore")
            .len(),
        0
    );
}

#[test]
fn policy_digest_binds_declarations_deterministically() {
    let root = Root::new("digest");
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let topics =
        TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("open topic authority");
    let head = create_channel(&channel, 1_024, 200);
    let base = policy_for(8);

    let first = create_topic(&topics, head.channel_id, b"digest.one", base, 141);
    let second = create_topic(&topics, head.channel_id, b"digest.two", base, 142);
    // The digest covers the declarations and the payer, not the name: two
    // topics with the same policy share it.
    assert_eq!(first.policy_digest, second.policy_digest);

    let drifted: [(&'static str, u8, TopicPolicy); 4] = [
        (
            "max_recipients",
            143,
            TopicPolicy {
                max_recipients: 9,
                ..base
            },
        ),
        (
            "delivery_attempts",
            144,
            TopicPolicy {
                delivery_attempts: 4,
                ..base
            },
        ),
        (
            "retained_bytes",
            145,
            TopicPolicy {
                retained_bytes: 8_192,
                ..base
            },
        ),
        (
            "payer",
            146,
            TopicPolicy {
                payer: payer(8),
                ..base
            },
        ),
    ];
    for (label, seed, policy) in drifted {
        let name = format!("digest.drift.{label}");
        let topic = create_topic(&topics, head.channel_id, name.as_bytes(), policy, seed);
        assert_ne!(
            topic.policy_digest, first.policy_digest,
            "{label} must be bound by the digest"
        );
    }

    // Publications carry the topic head's digest and payer verbatim.
    let publication = publish_at(&topics, first.topic_id, 151, b"payload", 12_000);
    assert_eq!(publication.policy_digest, first.policy_digest);
    assert_eq!(publication.payer, base.payer);
    assert_eq!(publication.cascade_budget_remaining, base.cascade_depth);

    // The digest survives the restart re-derivation (inspect cross-checks
    // the stored digest against the stored declarations) on the same root.
    drop(topics);
    drop(channel);
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("reopen channel"));
    let topics = TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("reopen topics");
    let reopened = topics
        .inspect_topic(first.topic_id)
        .expect("inspect reopened");
    assert_eq!(reopened.policy, first.policy);
    assert_eq!(reopened.policy_digest, first.policy_digest);
    let publications = topics
        .inspect_publications(first.topic_id)
        .expect("reopened publications");
    assert_eq!(publications.len(), 1);
    assert_eq!(publications[0], publication);
}
