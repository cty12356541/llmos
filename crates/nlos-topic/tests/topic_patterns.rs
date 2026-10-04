//! Matching-predicate subscription tests (schema v6, the ADR-0007
//! matching-predicate addendum): pattern language validation, the two attach
//! time points, verbatim skip reporting, delivery parity of attached
//! subscriptions, cancel semantics and the v6 migration paths.
use nlos_channel::{
    AckRequest, ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest,
};
use nlos_topic::{
    AttachReport, AttachSkipReason, CancelPatternDecision, CancelPatternRequest, ConsumeToken,
    CreateTopicRequest, PatternId, PatternRecord, PatternSubscribeDecision, PublishDecision,
    PublishRequest, SubscribeDecision, SubscribePatternRequest, SubscribeRequest, SubscriberKey,
    SubscriptionRecord, TopicAuthority, TopicAuthorityError, TopicDecision, TopicId, TopicPolicy,
    TopicRecord,
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

fn binding(seed: u8) -> ResourceAccountId {
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
            "nlos-topic-patterns-{label}-{}-{nonce}-{sequence}",
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
        payer: binding(7),
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
    topic_id: TopicId,
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
    topic_id: TopicId,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> nlos_topic::PublicationRecord {
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

fn subscribe_pattern_at(
    topics: &TopicAuthority,
    pattern: &[u8],
    binding_seed: u8,
    subscriber_seed: u8,
    seed: u8,
    at: u64,
) -> (PatternRecord, AttachReport) {
    match topics
        .subscribe_pattern(SubscribePatternRequest {
            pattern: pattern.to_vec(),
            binding: binding(binding_seed),
            subscriber_key: subscriber(subscriber_seed),
            idempotency_key: key(seed),
            subscribed_at_ms: at,
        })
        .expect("subscribe pattern")
    {
        PatternSubscribeDecision::Subscribed(outcome) => (outcome.pattern, outcome.report),
        PatternSubscribeDecision::Replayed(_) => panic!("fresh pattern subscribe cannot replay"),
    }
}

fn cancel_pattern_at(
    topics: &TopicAuthority,
    pattern_id: PatternId,
    consume_token: &ConsumeToken,
    at: u64,
) -> nlos_topic::CancelPatternReceipt {
    match topics
        .cancel_pattern(
            CancelPatternRequest {
                pattern_id,
                cancelled_at_ms: at,
            },
            consume_token,
        )
        .expect("cancel pattern")
    {
        CancelPatternDecision::Cancelled(receipt) => receipt,
        CancelPatternDecision::Replayed(_) => panic!("fresh cancel cannot replay"),
    }
}

#[test]
fn invalid_pattern_and_zero_binding_fail_closed_pre_write() {
    let harness = Harness::new("invalid");
    let head = create_channel(&harness.channel, 1_024, 200);
    let _topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"feed.items",
        policy_for(4),
        31,
    );

    // The empty string, a `*` in the middle or at the start of a longer
    // pattern, and a second `*` are all rejections; only a final `*` is a
    // wildcard.
    let invalid: [&[u8]; 5] = [b"", b"a*b", b"*a", b"a**", b"telemetry.*x"];
    for pattern in invalid {
        let error = harness
            .topics
            .subscribe_pattern(SubscribePatternRequest {
                pattern: pattern.to_vec(),
                binding: binding(9),
                subscriber_key: subscriber(1),
                idempotency_key: key(201),
                subscribed_at_ms: 3_000,
            })
            .expect_err("invalid pattern must be rejected");
        assert!(
            matches!(error, TopicAuthorityError::InvalidPattern(_)),
            "{pattern:?}: {error:?}"
        );
    }

    // A zero-valued binding is rejected even for a legal pattern.
    assert!(matches!(
        harness.topics.subscribe_pattern(SubscribePatternRequest {
            pattern: b"feed*".to_vec(),
            binding: binding(0),
            subscriber_key: subscriber(1),
            idempotency_key: key(201),
            subscribed_at_ms: 3_000,
        }),
        Err(TopicAuthorityError::InvalidPolicy(_))
    ));

    // Fail-closed pre-write: no pattern row was created by any rejection,
    // and the idempotency keys were not burned.
    let raw = Connection::open(harness.root.topic_db()).expect("open raw topic db");
    let rows: i64 = raw
        .query_row("SELECT COUNT(*) FROM topic_patterns", [], |row| row.get(0))
        .expect("count pattern rows");
    assert_eq!(rows, 0);
    let (pattern, report) = subscribe_pattern_at(&harness.topics, b"feed*", 9, 1, 201, 3_100);
    assert_eq!(report.attached.len(), 1);
    assert_eq!(report.skipped, Vec::new());
    assert_eq!(pattern.pattern_generation, 1);
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers exact, empty-suffix and dotted-boundary matching.
fn exact_and_prefix_patterns_enumerate_and_attach() {
    let harness = Harness::new("enumerate");
    let head = create_channel(&harness.channel, 1_024, 200);
    let bare = create_topic(
        &harness.topics,
        head.channel_id,
        b"telemetry",
        policy_for(8),
        31,
    );
    let cpu = create_topic(
        &harness.topics,
        head.channel_id,
        b"telemetry.cpu",
        policy_for(8),
        32,
    );
    let gpu = create_topic(
        &harness.topics,
        head.channel_id,
        b"telemetry.gpu.temp",
        policy_for(8),
        33,
    );
    let metrics = create_topic(
        &harness.topics,
        head.channel_id,
        b"metrics.cpu",
        policy_for(8),
        34,
    );

    // The tail wildcard matches any suffix including the empty one, so the
    // bare `telemetry` topic is attached alongside the nested ones.
    let (pattern, report) = subscribe_pattern_at(&harness.topics, b"telemetry*", 9, 1, 201, 3_000);
    let attached_ids: Vec<TopicId> = report.attached.iter().map(|a| a.topic_id).collect();
    assert_eq!(attached_ids.len(), 3);
    for expected in [bare.topic_id, cpu.topic_id, gpu.topic_id] {
        assert!(attached_ids.contains(&expected));
    }
    assert_eq!(report.skipped, Vec::new());
    for attached in &report.attached {
        assert_eq!(attached.subscription.attached_by, Some(pattern.pattern_id));
        assert_eq!(attached.subscription.cursor, 0);
        assert!(attached.subscription.consume_token != [0; 32]);
    }
    // The observation entry lists the same rows, ordered by topic id.
    let observed = harness
        .topics
        .inspect_pattern_attachments(pattern.pattern_id)
        .expect("inspect attachments");
    assert_eq!(observed.len(), 3);
    let observed_ids: Vec<TopicId> = observed.iter().map(|row| row.topic_id).collect();
    let mut sorted_ids = observed_ids.clone();
    sorted_ids.sort();
    assert_eq!(observed_ids, sorted_ids);
    for row in &observed {
        assert!(row.attached_by == Some(pattern.pattern_id) && row.active);
    }

    // An exact pattern matches exactly one topic.
    let (exact, exact_report) =
        subscribe_pattern_at(&harness.topics, b"metrics.cpu", 9, 2, 202, 3_100);
    assert_eq!(exact_report.attached.len(), 1);
    assert_eq!(exact_report.attached[0].topic_id, metrics.topic_id);
    assert_eq!(exact_report.skipped, Vec::new());
    assert_eq!(
        harness
            .topics
            .inspect_pattern(exact.pattern_id)
            .expect("inspect pattern"),
        exact
    );

    // The dotted prefix does not match the bare name: the prefix bytes must
    // match exactly.
    let (_dotted, dotted_report) =
        subscribe_pattern_at(&harness.topics, b"telemetry.*", 9, 3, 203, 3_200);
    let dotted_ids: Vec<TopicId> = dotted_report.attached.iter().map(|a| a.topic_id).collect();
    assert_eq!(dotted_ids.len(), 2);
    assert!(dotted_ids.contains(&cpu.topic_id));
    assert!(dotted_ids.contains(&gpu.topic_id));
    assert!(!dotted_ids.contains(&bare.topic_id));

    // Direct subscriber counts absorbed the attached rows.
    assert_eq!(
        harness
            .topics
            .inspect_topic(bare.topic_id)
            .expect("bare")
            .active_subscriptions,
        1
    );
    assert_eq!(
        harness
            .topics
            .inspect_topic(metrics.topic_id)
            .expect("metrics")
            .active_subscriptions,
        1
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers active-skip and dead-row re-activation ownership.
fn pattern_attach_skips_active_and_reattaches_cancelled_rows() {
    let harness = Harness::new("skip");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"feed.items",
        policy_for(8),
        31,
    );

    // An existing active direct subscription wins: the matching topic is
    // skipped with the typed reason and the direct row is untouched.
    let direct = subscribe_at(&harness.topics, topic.topic_id, 1, 3_000);
    let (_pattern_a, report_a) = subscribe_pattern_at(&harness.topics, b"feed*", 9, 1, 201, 3_100);
    assert_eq!(report_a.attached, Vec::new());
    assert_eq!(
        report_a.skipped,
        vec![nlos_topic::AttachSkipped {
            topic_id: topic.topic_id,
            reason: AttachSkipReason::AlreadySubscribed,
        }]
    );
    assert_eq!(
        harness
            .topics
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("direct row"),
        direct
    );

    // A previously unsubscribed direct row is re-activated by the attach as
    // an attached subscription: bumped generation, fresh token, and the
    // provenance flips from NULL to the pattern id.
    let second = subscribe_at(&harness.topics, topic.topic_id, 2, 3_200);
    harness
        .topics
        .unsubscribe_with_token(
            nlos_topic::UnsubscribeRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(2),
                unsubscribed_at_ms: 3_300,
            },
            &second.consume_token,
        )
        .expect("unsubscribe second");
    let (pattern_b, report_b) = subscribe_pattern_at(&harness.topics, b"feed*", 9, 2, 202, 3_400);
    assert_eq!(report_b.skipped, Vec::new());
    assert_eq!(report_b.attached.len(), 1);
    let attached = &report_b.attached[0].subscription;
    assert_eq!(attached.subscription_generation, 2);
    assert!(attached.active);
    assert_eq!(attached.attached_by, Some(pattern_b.pattern_id));
    assert_ne!(attached.consume_token, second.consume_token);
    assert_eq!(
        harness
            .topics
            .inspect_topic(topic.topic_id)
            .expect("count")
            .active_subscriptions,
        2
    );
}

#[test]
fn pattern_attach_reports_recipient_limit_verbatim() {
    let harness = Harness::new("limit");
    let head = create_channel(&harness.channel, 1_024, 200);
    let full = create_topic(
        &harness.topics,
        head.channel_id,
        b"cap.one",
        policy_for(1),
        31,
    );
    let free = create_topic(
        &harness.topics,
        head.channel_id,
        b"cap.two",
        policy_for(4),
        32,
    );

    // Fill cap.one with a direct subscriber.
    let _occupant = subscribe_at(&harness.topics, full.topic_id, 1, 3_000);

    let (_pattern, report) = subscribe_pattern_at(&harness.topics, b"cap*", 9, 9, 201, 3_100);
    // The filled topic is skipped and reported, not silently queued; the
    // free one is attached.
    assert_eq!(report.attached.len(), 1);
    assert_eq!(report.attached[0].topic_id, free.topic_id);
    assert_eq!(
        report.skipped,
        vec![nlos_topic::AttachSkipped {
            topic_id: full.topic_id,
            reason: AttachSkipReason::RecipientLimitReached,
        }]
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the create-time attach point, replay and cancelled patterns.
fn create_topic_attaches_active_patterns_and_replay_does_not() {
    let harness = Harness::new("create-attach");
    let head = create_channel(&harness.channel, 1_024, 200);

    // Subscribe the patterns before any matching topic exists.
    let (pattern, report) = subscribe_pattern_at(&harness.topics, b"jobs.*", 9, 1, 201, 3_000);
    assert_eq!(report.attached, Vec::new());
    let (gone, _gone_report) = subscribe_pattern_at(&harness.topics, b"gone*", 8, 2, 202, 3_100);
    cancel_pattern_at(&harness.topics, gone.pattern_id, &gone.consume_token, 3_200);

    // Creating a matching topic attaches every active matching pattern.
    let jobs_a = create_topic(
        &harness.topics,
        head.channel_id,
        b"jobs.a",
        policy_for(8),
        31,
    );
    let attached = harness
        .topics
        .inspect_pattern_attachments(pattern.pattern_id)
        .expect("attachments after create");
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].topic_id, jobs_a.topic_id);
    assert_eq!(attached[0].attached_by, Some(pattern.pattern_id));
    assert_eq!(attached[0].subscribed_at_ms, jobs_a.created_at_ms);
    assert_eq!(jobs_a.active_subscriptions, 1);
    // The cancelled pattern does not attach.
    assert_eq!(
        harness
            .topics
            .inspect_pattern_attachments(gone.pattern_id)
            .expect("cancelled attachments"),
        Vec::<SubscriptionRecord>::new()
    );

    // A non-matching topic changes nothing.
    let tasks = create_topic(
        &harness.topics,
        head.channel_id,
        b"tasks.b",
        policy_for(8),
        32,
    );
    assert_eq!(tasks.active_subscriptions, 0);
    assert_eq!(
        harness
            .topics
            .inspect_pattern_attachments(pattern.pattern_id)
            .expect("attachments unchanged")
            .len(),
        1
    );

    // The create replay is zero-write: no duplicate attach.
    assert!(matches!(
        harness.topics.create_topic(CreateTopicRequest {
            channel_id: head.channel_id,
            name: b"jobs.a".to_vec(),
            policy: policy_for(8),
            idempotency_key: key(31),
            created_at_ms: 9_999,
        }),
        Ok(TopicDecision::Replayed(_))
    ));
    assert_eq!(
        harness
            .topics
            .inspect_topic(jobs_a.topic_id)
            .expect("jobs.a after replay")
            .active_subscriptions,
        1
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full delivery/advance/billing/compact parity of an attach.
fn attached_subscription_delivers_like_a_direct_one() {
    let harness = Harness::new("delivery");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"bus.events",
        policy_for(8),
        31,
    );

    let (pattern, report) = subscribe_pattern_at(&harness.topics, b"bus*", 9, 5, 201, 3_000);
    assert_eq!(report.attached.len(), 1);
    let attached = report.attached[0].subscription;
    // A direct subscriber starts from the identical state.
    let direct = subscribe_at(&harness.topics, topic.topic_id, 6, 3_100);
    assert_eq!(attached.cursor, direct.cursor);

    publish_at(&harness.topics, topic.topic_id, 251, b"one", 3_200);
    publish_at(&harness.topics, topic.topic_id, 252, b"two", 3_201);
    publish_at(&harness.topics, topic.topic_id, 253, b"three", 3_202);

    // The attached subscriber receives everything, filtered by its own
    // cursor exactly like the direct one.
    for seed in [5u8, 6] {
        let window = harness
            .topics
            .poll(topic.topic_id, subscriber(seed), 10)
            .expect("poll");
        assert_eq!(
            window
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }
    // Billing parity: both were lagging for seq 2 and seq 3 (the first
    // publication found no backlog and was free).
    assert_eq!(
        harness
            .topics
            .inspect_subscription(topic.topic_id, subscriber(5))
            .expect("attached billing")
            .redelivery_used,
        2
    );
    assert_eq!(
        harness
            .topics
            .inspect_subscription(topic.topic_id, subscriber(6))
            .expect("direct billing")
            .redelivery_used,
        2
    );

    // The attached row's own consume token drives its cursor.
    harness
        .topics
        .advance_with_token(
            nlos_topic::AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(5),
                up_to_sequence: 2,
                advanced_at_ms: 3_300,
            },
            &attached.consume_token,
        )
        .expect("advance attached");
    assert_eq!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(5), 10)
            .expect("attached tail")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        [3]
    );
    // The direct subscriber's window is unaffected.
    assert_eq!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(6), 10)
            .expect("direct window")
            .len(),
        3
    );

    // The attached cursor participates in the service-layer compaction
    // bound: the direct subscriber catches up first, then the channel owner
    // consumes everything, and the attached subscriber's cursor (2) is the
    // one clamping the trim below the consume high-water (3).
    harness
        .topics
        .advance_with_token(
            nlos_topic::AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(6),
                up_to_sequence: 3,
                advanced_at_ms: 3_350,
            },
            &direct.consume_token,
        )
        .expect("advance direct");
    harness
        .channel
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 3,
            acked_at_ms: 3_400,
        })
        .expect("channel ack");
    assert_eq!(
        harness.topics.compact_bound(topic.topic_id).expect("bound"),
        2
    );

    // A catch-up publication is delivered to both identically.
    harness
        .topics
        .advance_with_token(
            nlos_topic::AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(5),
                up_to_sequence: 3,
                advanced_at_ms: 3_500,
            },
            &attached.consume_token,
        )
        .expect("attached catches up");
    publish_at(&harness.topics, topic.topic_id, 254, b"four", 3_600);
    for seed in [5u8, 6] {
        assert_eq!(
            harness
                .topics
                .poll(topic.topic_id, subscriber(seed), 10)
                .expect("catch-up delivery")
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            [4]
        );
    }
    assert!(pattern.active);
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers detach, direct-subscription safety and cancel replay.
fn cancel_pattern_detaches_attached_and_spares_direct() {
    let harness = Harness::new("cancel");
    let head = create_channel(&harness.channel, 1_024, 200);
    let x = create_topic(
        &harness.topics,
        head.channel_id,
        b"multi.x",
        policy_for(8),
        31,
    );
    let y = create_topic(
        &harness.topics,
        head.channel_id,
        b"multi.y",
        policy_for(8),
        32,
    );

    // Direct subscription on x wins over the pattern attach; y is attached.
    let direct = subscribe_at(&harness.topics, x.topic_id, 1, 3_000);
    publish_at(&harness.topics, x.topic_id, 251, b"early", 3_100);
    let (pattern, report) = subscribe_pattern_at(&harness.topics, b"multi*", 9, 1, 201, 3_200);
    assert_eq!(report.attached.len(), 1);
    assert_eq!(report.attached[0].topic_id, y.topic_id);

    let receipt = cancel_pattern_at(
        &harness.topics,
        pattern.pattern_id,
        &pattern.consume_token,
        3_300,
    );
    assert!(!receipt.pattern.active);
    assert_eq!(receipt.pattern.cancelled_at_ms, 3_300);
    assert_eq!(receipt.detached.len(), 1);
    assert_eq!(receipt.detached[0].topic_id, y.topic_id);
    assert_eq!(receipt.detached[0].receipt.subscriber_key, subscriber(1));
    assert_eq!(
        receipt.detached[0].receipt.subscription_id,
        report.attached[0].subscription.subscription_id
    );

    // The detached subscription is inactive; the direct one is untouched.
    assert!(matches!(
        harness.topics.poll(y.topic_id, subscriber(1), 10),
        Err(TopicAuthorityError::SubscriptionInactive(_))
    ));
    assert_eq!(
        harness
            .topics
            .inspect_subscription(x.topic_id, subscriber(1))
            .expect("direct row after cancel"),
        direct
    );
    assert_eq!(
        harness
            .topics
            .poll(x.topic_id, subscriber(1), 10)
            .expect("direct still delivers")
            .len(),
        1
    );
    assert_eq!(
        harness
            .topics
            .inspect_topic(y.topic_id)
            .expect("y count")
            .active_subscriptions,
        0
    );
    assert_eq!(
        harness
            .topics
            .inspect_pattern_attachments(pattern.pattern_id)
            .expect("attachments after cancel"),
        Vec::<SubscriptionRecord>::new()
    );

    // The cancel replay returns the current cancelled row with no detach.
    assert!(matches!(
        harness.topics.cancel_pattern(
            CancelPatternRequest {
                pattern_id: pattern.pattern_id,
                cancelled_at_ms: 9_999,
            },
            &pattern.consume_token,
        ),
        Ok(CancelPatternDecision::Replayed(replay))
            if replay.pattern.cancelled_at_ms == 3_300 && replay.detached.is_empty()
    ));
}

#[test]
fn cancel_pattern_with_wrong_token_writes_nothing() {
    let harness = Harness::new("cancel-token");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"secure.events",
        policy_for(8),
        31,
    );
    let (pattern, report) = subscribe_pattern_at(&harness.topics, b"secure*", 9, 5, 201, 3_000);
    assert_eq!(report.attached.len(), 1);

    // A wrong token fails closed before any write.
    let wrong: [u8; 32] = [0xAA; 32];
    assert_ne!(wrong, pattern.consume_token);
    assert!(matches!(
        harness.topics.cancel_pattern(
            CancelPatternRequest {
                pattern_id: pattern.pattern_id,
                cancelled_at_ms: 3_100,
            },
            &wrong,
        ),
        Err(TopicAuthorityError::PatternConsumptionTokenMismatch(id))
            if id == pattern.pattern_id
    ));
    let unchanged = harness
        .topics
        .inspect_pattern(pattern.pattern_id)
        .expect("pattern readback");
    assert_eq!(unchanged, pattern);
    assert_eq!(
        harness
            .topics
            .inspect_pattern_attachments(pattern.pattern_id)
            .expect("attachments unchanged")
            .len(),
        1
    );
    assert!(
        harness
            .topics
            .poll(topic.topic_id, subscriber(5), 10)
            .is_ok()
    );

    // An unknown pattern id fails closed as well.
    assert!(matches!(
        harness.topics.cancel_pattern(
            CancelPatternRequest {
                pattern_id: PatternId::from_bytes([0; 16]),
                cancelled_at_ms: 3_200,
            },
            &pattern.consume_token,
        ),
        Err(TopicAuthorityError::TopicPatternNotFound(_))
    ));

    // The correct token still cancels after the rejected attempts.
    let receipt = cancel_pattern_at(
        &harness.topics,
        pattern.pattern_id,
        &pattern.consume_token,
        3_300,
    );
    assert!(!receipt.pattern.active);
    assert_eq!(receipt.detached.len(), 1);
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the replay matrix and the frozen create-key binding.
fn pattern_replays_return_current_row_and_foreign_keys_conflict() {
    let harness = Harness::new("replay");
    let head = create_channel(&harness.channel, 1_024, 200);
    let _topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"gen.a",
        policy_for(8),
        31,
    );

    let request = SubscribePatternRequest {
        pattern: b"gen*".to_vec(),
        binding: binding(9),
        subscriber_key: subscriber(7),
        idempotency_key: key(61),
        subscribed_at_ms: 3_000,
    };
    let (first, first_report) = match harness
        .topics
        .subscribe_pattern(request.clone())
        .expect("first subscribe")
    {
        PatternSubscribeDecision::Subscribed(outcome) => (outcome.pattern, outcome.report),
        PatternSubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    assert_eq!(first.pattern_generation, 1);
    assert_eq!(first_report.attached.len(), 1);

    // Same-key replay returns the active row.
    assert!(matches!(
        harness.topics.subscribe_pattern(request.clone()),
        Ok(PatternSubscribeDecision::Replayed(outcome))
            if outcome.pattern == first && outcome.report.attached.is_empty()
    ));
    // A foreign key naming the already-active (pattern, subscriber key)
    // identity is a typed rebinding conflict (the W55 reconciliation of
    // deep-audit finding #15: the create key is frozen identity, and the
    // pre-W55 silent replay under any new key destroyed the original
    // key's rebinding evidence).
    assert!(matches!(
        harness.topics.subscribe_pattern(SubscribePatternRequest {
            idempotency_key: key(62),
            subscribed_at_ms: 9_999,
            ..request.clone()
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));

    // Cancel, then the original key replays the *current* (cancelled) row.
    harness
        .topics
        .cancel_pattern(
            CancelPatternRequest {
                pattern_id: first.pattern_id,
                cancelled_at_ms: 3_500,
            },
            &first.consume_token,
        )
        .expect("cancel");
    let cancelled_row = harness
        .topics
        .inspect_pattern(first.pattern_id)
        .expect("cancelled row");
    assert!(!cancelled_row.active);
    assert!(matches!(
        harness.topics.subscribe_pattern(request),
        Ok(PatternSubscribeDecision::Replayed(outcome))
            if outcome.pattern == cancelled_row && outcome.report.attached.is_empty()
    ));
    // A foreign key naming the cancelled identity conflicts as well: the
    // (pattern, subscriber key) lifecycle stays bound to its first create
    // key, so a cancelled pair stays cancelled for every later attempt.
    assert!(matches!(
        harness.topics.subscribe_pattern(SubscribePatternRequest {
            pattern: b"gen*".to_vec(),
            binding: binding(9),
            subscriber_key: subscriber(7),
            idempotency_key: key(63),
            subscribed_at_ms: 3_600,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    // Nothing was written by the rejected attempts: the durable row still
    // carries the original generation, token and create-key binding.
    assert_eq!(
        harness
            .topics
            .inspect_pattern(first.pattern_id)
            .expect("row unchanged"),
        cancelled_row
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full restart replay of pattern rows and attachments.
fn restart_replays_pattern_rows_attachments_and_tokens() {
    let root = Root::new("restart");
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let topics =
        TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("open topic authority");
    let head = create_channel(&channel, 256, 200);

    // Pattern first, topic second: the attach happens at the create time
    // point and must replay across the restart.
    let (pattern, report) = subscribe_pattern_at(&topics, b"life*", 9, 5, 201, 3_000);
    assert_eq!(report.attached, Vec::new());
    let topic = create_topic(&topics, head.channel_id, b"life.cycle", policy_for(8), 31);
    publish_at(&topics, topic.topic_id, 251, b"one", 3_100);
    let attached = topics
        .inspect_subscription(topic.topic_id, subscriber(5))
        .expect("attached row");
    assert_eq!(attached.attached_by, Some(pattern.pattern_id));
    topics
        .advance_with_token(
            nlos_topic::AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(5),
                up_to_sequence: 1,
                advanced_at_ms: 3_200,
            },
            &attached.consume_token,
        )
        .expect("advance attached");
    let pattern_before = topics.inspect_pattern(pattern.pattern_id).expect("pattern");
    let attached_before = topics
        .inspect_pattern_attachments(pattern.pattern_id)
        .expect("attachments");

    drop(topics);
    drop(channel);
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("reopen channel authority"));
    let topics =
        TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("reopen topic authority");

    // Every durable row replays field-for-field.
    assert_eq!(
        topics
            .inspect_pattern(pattern.pattern_id)
            .expect("pattern after restart"),
        pattern_before
    );
    assert_eq!(
        topics
            .inspect_pattern_attachments(pattern.pattern_id)
            .expect("attachments after restart"),
        attached_before
    );
    // The deterministic token still authenticates the attached subscriber.
    publish_at(&topics, topic.topic_id, 252, b"two", 3_300);
    assert_eq!(
        topics
            .poll(topic.topic_id, subscriber(5), 10)
            .expect("poll after restart")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        [2]
    );
    topics
        .advance_with_token(
            nlos_topic::AdvanceRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(5),
                up_to_sequence: 2,
                advanced_at_ms: 3_400,
            },
            &attached.consume_token,
        )
        .expect("advance after restart");
    // And the pattern token still cancels after the restart.
    let receipt = cancel_pattern_at(&topics, pattern.pattern_id, &pattern.consume_token, 3_500);
    assert!(!receipt.pattern.active);
    assert_eq!(receipt.detached.len(), 1);
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the idempotent downgrade, unknown-version and partial-state paths.
fn schema_v6_migration_downgrade_and_fail_closed_paths() {
    let root = Root::new("schema");
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let topics =
        TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("open topic authority");
    let head = create_channel(&channel, 256, 200);
    let (pattern, _report) = subscribe_pattern_at(&topics, b"six*", 9, 5, 201, 3_000);
    let _topic = create_topic(&topics, head.channel_id, b"six.a", policy_for(8), 31);
    let attached_before = topics
        .inspect_pattern_attachments(pattern.pattern_id)
        .expect("attachments");
    assert_eq!(attached_before.len(), 1);

    // The v6 step is additive and object-tracked: a database carrying the
    // pattern table and the attached_by column still carries the v5
    // user_version watermark (the v1-v5 rebuild chain marker).
    {
        let raw = Connection::open(root.topic_db()).expect("open raw topic db");
        let version: i64 = raw
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(version, 5);
    }

    // A database stamped back to v4 (but carrying the v5/v6 objects)
    // reopens idempotently: the rebuild-chain pre-checks restore the
    // watermark and the v6 pre-check is a no-op; every row survives,
    // including the attached_by provenance.
    drop(topics);
    drop(channel);
    {
        let raw = Connection::open(root.topic_db()).expect("open raw for downgrade");
        raw.pragma_update(None, "user_version", 4)
            .expect("downgrade");
    }
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("reopen channel"));
    let topics = TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("reopen after v4");
    assert_eq!(
        topics
            .inspect_pattern(pattern.pattern_id)
            .expect("pattern after downgrade reopen"),
        pattern
    );
    assert_eq!(
        topics
            .inspect_pattern_attachments(pattern.pattern_id)
            .expect("attachments after downgrade reopen"),
        attached_before
    );
    {
        let raw = Connection::open(root.topic_db()).expect("open raw version check");
        let version: i64 = raw
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version after reopen");
        assert_eq!(version, 5);
    }

    // An unknown future watermark fails closed.
    drop(topics);
    drop(channel);
    {
        let raw = Connection::open(root.topic_db()).expect("open raw for unknown");
        raw.pragma_update(None, "user_version", 99).expect("bump");
    }
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("reopen channel again"));
    assert!(matches!(
        TopicAuthority::open(root.path(), Arc::clone(&channel)),
        Err(TopicAuthorityError::SchemaVersionUnsupported(99))
    ));
    {
        let raw = Connection::open(root.topic_db()).expect("open raw to restore");
        raw.pragma_update(None, "user_version", 5).expect("restore");
    }

    // A half-applied v6 (the provenance column exists, the pattern table
    // does not) fails closed as corrupt instead of guessing: durable
    // attached rows would reference a dropped table.
    {
        let raw = Connection::open(root.topic_db()).expect("open raw for partial");
        raw.pragma_update(None, "foreign_keys", "OFF")
            .expect("fk off");
        raw.pragma_update(None, "user_version", 4)
            .expect("downgrade again");
        raw.execute("DROP TABLE topic_patterns", [])
            .expect("drop table");
    }
    assert!(matches!(
        TopicAuthority::open(root.path(), channel),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));
}

/// The pattern create key is frozen replay evidence (the W55
/// reconciliation of deep-audit finding #15): a foreign-key re-subscribe
/// attempt can never overwrite it — active or cancelled — the original
/// key's rebinding detection survives every lifecycle step, the v9
/// identity trigger freezes the column against raw rewrites, and the v9
/// publication indexes are present (deep-audit finding #22) with an
/// idempotent reopen.
#[test]
fn pattern_create_key_is_frozen_replay_evidence() {
    let root = Root::new("create-key");
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let topics =
        TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("open topic authority");
    let head = create_channel(&channel, 256, 200);
    let _topic = create_topic(&topics, head.channel_id, b"key.a", policy_for(8), 41);

    let request = SubscribePatternRequest {
        pattern: b"key*".to_vec(),
        binding: binding(9),
        subscriber_key: subscriber(6),
        idempotency_key: key(71),
        subscribed_at_ms: 4_000,
    };
    let (first, report) = match topics
        .subscribe_pattern(request.clone())
        .expect("subscribe pattern")
    {
        PatternSubscribeDecision::Subscribed(outcome) => (outcome.pattern, outcome.report),
        PatternSubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    };
    assert_eq!(report.attached.len(), 1);

    // A foreign key naming the active identity conflicts before any write.
    assert!(matches!(
        topics.subscribe_pattern(SubscribePatternRequest {
            idempotency_key: key(72),
            ..request.clone()
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    // ...and after a cancel too, while the original key keeps replaying
    // the *current* (cancelled) row: the evidence was never rewritten.
    cancel_pattern_at(&topics, first.pattern_id, &first.consume_token, 4_100);
    assert!(matches!(
        topics.subscribe_pattern(SubscribePatternRequest {
            idempotency_key: key(73),
            ..request.clone()
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    assert!(matches!(
        topics.subscribe_pattern(request.clone()),
        Ok(PatternSubscribeDecision::Replayed(outcome)) if !outcome.pattern.active
    ));
    // The original key rebound to a different binding is the typed
    // conflict — the pre-W55 key overwrite silently replayed this case.
    assert!(matches!(
        topics.subscribe_pattern(SubscribePatternRequest {
            binding: binding(8),
            ..request.clone()
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));

    // Storage layer: the stored create key is still the original one, the
    // v9 trigger freezes it against raw rewrites, and both v9 publication
    // indexes exist.
    {
        let raw = Connection::open(root.topic_db()).expect("open raw topic db");
        let stored: Vec<u8> = raw
            .query_row(
                "SELECT create_idempotency_key FROM topic_patterns WHERE pattern_id=?1",
                [first.pattern_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .expect("stored create key");
        assert_eq!(stored, key(71).into_bytes().to_vec());
        assert!(
            raw.execute(
                "UPDATE topic_patterns SET create_idempotency_key=?1 WHERE pattern_id=?2",
                [
                    key(72).as_bytes().as_slice(),
                    first.pattern_id.as_bytes().as_slice()
                ],
            )
            .is_err(),
            "the create key is frozen by the v9 identity trigger"
        );
        let indexes: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name IN (
                    'topic_publications_topic_sequence',
                    'topic_publications_topic_parent')",
                [],
                |row| row.get(0),
            )
            .expect("count v9 indexes");
        assert_eq!(indexes, 2, "both v9 publication indexes are present");
    }

    // The reopen is idempotent: the v9 completed-state probe finds its
    // objects and writes nothing, and the (cancelled) row replays
    // byte-for-byte.
    let cancelled_row = topics
        .inspect_pattern(first.pattern_id)
        .expect("cancelled row before reopen");
    drop(topics);
    drop(channel);
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("reopen channel"));
    let topics = TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("reopen topics");
    assert_eq!(
        topics
            .inspect_pattern(first.pattern_id)
            .expect("pattern after reopen"),
        cancelled_row
    );
}

/// The v10 step (W58-2, the deep-audit #24 residual W57-C registered): the
/// subscribe-time attach enumeration narrows its `topics` name scan through
/// the stored-name index instead of enumerating every row.  The narrowing's
/// memcmp edges are exercised end to end (exact equality, a plain prefix, a
/// `0xFF`-heavy prefix whose successor bound carries, an all-`0xFF` prefix
/// with no finite upper bound, the bare `*`), the storage layer carries the
/// index with the watermark untouched (object-tracked like v6-v9), the
/// query plans really go through the index, and the reopen is an
/// idempotent no-op.
#[test]
#[allow(clippy::too_many_lines)] // One test covers the semantics, the storage layer and idempotence.
fn schema_v10_name_index_narrows_the_attach_enumeration() {
    let harness = Harness::new("v10-name-index");
    let head = create_channel(&harness.channel, 4_096, 90);
    // Names chosen to exercise the narrowing edges: two plain names under
    // one prefix, a `0xFE 0xFF`-rooted name (the successor bound carries
    // past the trailing `0xFF`), an all-`0xFF`-rooted name (inside the
    // unbounded class) and a non-matching neighbour.
    let plain_a = create_topic(
        &harness.topics,
        head.channel_id,
        b"idx.a",
        policy_for(8),
        91,
    );
    let _plain_b = create_topic(
        &harness.topics,
        head.channel_id,
        b"idx.b",
        policy_for(8),
        92,
    );
    let edge_a = create_topic(
        &harness.topics,
        head.channel_id,
        &[0xFE, 0xFF, 0x01],
        policy_for(8),
        93,
    );
    let edge_b = create_topic(
        &harness.topics,
        head.channel_id,
        &[0xFF, 0x00],
        policy_for(8),
        94,
    );
    let _other = create_topic(
        &harness.topics,
        head.channel_id,
        b"other",
        policy_for(8),
        95,
    );

    // Exact pattern: an equality seek attaching only the byte-equal topic.
    let (_exact, exact_report) = subscribe_pattern_at(&harness.topics, b"idx.a", 9, 10, 96, 4_000);
    assert_eq!(exact_report.attached.len(), 1);
    assert_eq!(
        exact_report.attached[0].subscription.topic_id,
        plain_a.topic_id
    );

    // Plain prefix: both plain topics, none of the edges.
    let (_plain, plain_report) = subscribe_pattern_at(&harness.topics, b"idx.*", 9, 11, 97, 4_100);
    assert_eq!(plain_report.attached.len(), 2);

    // `0xFF`-heavy prefix `[0xFE, 0xFF]*`: the successor bound `[0xFF]`
    // carries past the trailing `0xFF`, so exactly the one rooted name
    // attaches (the all-`0xFF`-rooted name sorts at or after the bound).
    let (_carry, carry_report) =
        subscribe_pattern_at(&harness.topics, &[0xFE, 0xFF, b'*'], 9, 12, 98, 4_200);
    assert_eq!(carry_report.attached.len(), 1);
    assert_eq!(
        carry_report.attached[0].subscription.topic_id,
        edge_a.topic_id
    );

    // All-`0xFF` prefix `[0xFF]*`: no finite upper bound — the range runs
    // to the end of the index and attaches the rooted name alone.
    let (_unbounded, unbounded_report) =
        subscribe_pattern_at(&harness.topics, &[0xFF, b'*'], 9, 13, 99, 4_300);
    assert_eq!(unbounded_report.attached.len(), 1);
    assert_eq!(
        unbounded_report.attached[0].subscription.topic_id,
        edge_b.topic_id
    );

    // The bare `*` matches every topic by definition.
    let (_star, star_report) = subscribe_pattern_at(&harness.topics, b"*", 9, 14, 100, 4_400);
    assert_eq!(star_report.attached.len(), 5);

    // Storage layer: the index is present, the watermark stays 5 (v10 is
    // object-tracked, not a watermark bump) and the narrowed scans really
    // go through the index.
    {
        let raw = Connection::open(harness.root.topic_db()).expect("open raw topic db");
        let version: i64 = raw
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("version");
        assert_eq!(version, 5, "v10 is object-tracked, the watermark stays 5");
        let indexes: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type='index' AND name='topics_topic_name'",
                [],
                |row| row.get(0),
            )
            .expect("count v10 index");
        assert_eq!(indexes, 1, "the v10 name index is present");
        let seek: String = raw
            .query_row(
                "EXPLAIN QUERY PLAN SELECT topic_id, topic_name FROM topics
                  WHERE topic_name = ?1 ORDER BY topic_id",
                rusqlite::params![b"idx.a".to_vec()],
                |row| row.get(3),
            )
            .expect("equality plan");
        assert!(
            seek.contains("topics_topic_name"),
            "the equality seek goes through the index, plan: {seek}"
        );
        let range: String = raw
            .query_row(
                "EXPLAIN QUERY PLAN SELECT topic_id, topic_name FROM topics
                  WHERE topic_name >= ?1 AND topic_name < ?2 ORDER BY topic_id",
                rusqlite::params![b"idx".to_vec(), b"idy".to_vec()],
                |row| row.get(3),
            )
            .expect("range plan");
        assert!(
            range.contains("topics_topic_name"),
            "the prefix range goes through the index, plan: {range}"
        );
    }

    // The reopen is idempotent: the v10 completed-state probe finds its
    // object and writes nothing, and a later pattern subscribe still
    // attaches through the narrowed scan.
    let Harness {
        root,
        channel,
        topics,
    } = harness;
    drop(topics);
    drop(channel);
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("reopen channel"));
    let topics = TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("reopen topics");
    let publications = topics
        .inspect_publications(plain_a.topic_id)
        .expect("journal after reopen");
    assert_eq!(publications.len(), 0);
    let (_late, late_report) = subscribe_pattern_at(&topics, b"idx.a", 9, 15, 101, 4_500);
    assert_eq!(late_report.attached.len(), 1);
    assert_eq!(
        late_report.attached[0].subscription.topic_id,
        plain_a.topic_id
    );
}
