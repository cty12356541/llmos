//! B-NOTIFY-001 integration tests: the notification service is a thin
//! subscription face over the existing Topic/Channel/Wait authorities
//! (W33-D gate: subscribe/deliver/ack all route through the Topic
//! authority; the face holds references only, never a second source of
//! truth).

use nlos_channel::{
    ChannelAuthority, ChannelDecision, CreateChannelRequest, QueueEntryRecord, QueueState,
};
use nlos_notify::{
    AckNotificationRequest, CancelNotificationRequest, NotificationId, NotificationService,
    NotificationSubscriptionView, NotifyError, PublishNotificationRequest,
    RegisterDeliveryWaitRequest, SubscribeNotificationRequest,
};
use nlos_topic::{
    CreateTopicRequest, SubscribeDecision, SubscribeRequest, TopicAuthority, TopicAuthorityError,
    TopicDecision, TopicPolicy, TopicRecord, UnsubscribeRequest,
};
use nlos_types::{ChannelId, IdempotencyKey, ResourceAccountId};
use nlos_wait::{BindingId, WaitAuthority, WaitState};
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

fn subscriber(seed: u8) -> nlos_topic::SubscriberKey {
    nlos_topic::SubscriberKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
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
            "nlos-notify-{label}-{}-{nonce}-{sequence}",
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
    root: Root,
    channel: Arc<ChannelAuthority>,
    topics: Arc<TopicAuthority>,
    waits: Arc<WaitAuthority>,
    notify: NotificationService,
}

impl Harness {
    fn new(label: &str) -> Self {
        let root = Root::new(label);
        let authorities = Authorities::open(root.path());
        let notify = NotificationService::open(
            root.path(),
            Arc::clone(&authorities.topics),
            Arc::clone(&authorities.waits),
        )
        .expect("open notification service");
        Self {
            root,
            channel: authorities.channel,
            topics: authorities.topics,
            waits: authorities.waits,
            notify,
        }
    }
}

struct Authorities {
    channel: Arc<ChannelAuthority>,
    topics: Arc<TopicAuthority>,
    waits: Arc<WaitAuthority>,
}

impl Authorities {
    fn open(root: &Path) -> Self {
        let channel = Arc::new(ChannelAuthority::open(root).expect("open channel authority"));
        let topics = Arc::new(
            TopicAuthority::open(root, Arc::clone(&channel)).expect("open topic authority"),
        );
        let waits =
            Arc::new(WaitAuthority::open(root, Arc::clone(&channel)).expect("open wait authority"));
        Self {
            channel,
            topics,
            waits,
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

fn create_channel(channel: &ChannelAuthority, seed: u8) -> nlos_channel::ChannelRecord {
    match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 8_192,
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
    max_recipients: u64,
    seed: u8,
) -> TopicRecord {
    match topics
        .create_topic(CreateTopicRequest {
            channel_id,
            name: name.to_vec(),
            policy: policy_for(max_recipients),
            idempotency_key: key(seed),
            created_at_ms: 2_000,
        })
        .expect("create topic")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh topic cannot replay"),
    }
}

fn face_subscribe(
    notify: &NotificationService,
    topic_id: nlos_topic::TopicId,
    seed: u8,
    at: u64,
) -> nlos_notify::NotificationSubscription {
    match notify
        .subscribe(SubscribeNotificationRequest {
            topic_id,
            subscriber_key: subscriber(seed),
            subscribed_at_ms: at,
        })
        .expect("face subscribe")
    {
        nlos_notify::NotificationSubscribeDecision::Subscribed(record) => record,
        nlos_notify::NotificationSubscribeDecision::Replayed(_) => {
            panic!("fresh face subscribe cannot replay")
        }
    }
}

fn live_views(notify: &NotificationService) -> Vec<nlos_notify::NotificationSubscription> {
    notify
        .list_subscriptions()
        .expect("list")
        .into_iter()
        .map(|view| match view {
            NotificationSubscriptionView::Live { subscription, .. } => subscription,
            NotificationSubscriptionView::Dangling { subscription } => {
                panic!("expected live view, got dangling for {subscription:?}")
            }
        })
        .collect()
}

/// W33-D gate chain: subscribe (Topic authority) → register a durable
/// delivery wait (Wait authority) → publish through the face (Topic
/// authority enqueue + the ADR-0008 commit-side `notify_commits` wiring)
/// → deliver by reading what the Topic authority fanned out → ack by the
/// token-authenticated cursor advance.  Every canonical fact is observable
/// in an authority database, none in the face.
#[test]
fn subscribe_wait_publish_deliver_ack_full_chain() {
    let harness = Harness::new("chain");
    let channel_head = create_channel(&harness.channel, 200);
    let topic = create_topic(&harness.topics, channel_head.channel_id, b"alerts", 8, 201);

    let subscription = face_subscribe(&harness.notify, topic.topic_id, 3, 3_000);
    assert_eq!(subscription.topic_id, topic.topic_id);
    assert_eq!(subscription.subscriber_key, subscriber(3));
    // The face stores the authority-issued credential reference verbatim.
    let authority_record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(3))
        .expect("authority subscription");
    assert_eq!(subscription.consume_token, authority_record.consume_token);

    // Durable wait on the subscription's channel: target = next sequence.
    let queue: QueueState = harness
        .channel
        .inspect_queue(channel_head.channel_id)
        .expect("queue");
    let target = queue.max_sequence + 1;
    let wait = match harness
        .notify
        .register_delivery_wait(RegisterDeliveryWaitRequest {
            notification_id: subscription.notification_id,
            target_sequence: target,
            binding: binding(11),
            idempotency_key: key(210),
            registered_at_ms: 3_100,
        })
        .expect("register delivery wait")
    {
        nlos_wait::RegisterDecision::Registered(record) => record,
        nlos_wait::RegisterDecision::Replayed(_) => panic!("fresh wait cannot replay"),
    };
    assert_eq!(wait.state, WaitState::Pending);

    // Publish through the face: the Topic authority owns the single
    // enqueue; the face adds only the explicit wait notify.
    let publish = match harness
        .notify
        .publish(PublishNotificationRequest {
            topic_id: topic.topic_id,
            payload: b"disk-full".to_vec(),
            idempotency_key: key(220),
            published_at_ms: 3_200,
        })
        .expect("face publish")
    {
        nlos_notify::NotificationPublishDecision::Published(receipt) => receipt,
        nlos_notify::NotificationPublishDecision::Replayed(_) => {
            panic!("fresh publish cannot replay")
        }
    };
    assert_eq!(publish.publication.channel_sequence, target);
    assert_eq!(
        publish
            .wakes
            .woken
            .iter()
            .map(|w| w.wait_id)
            .collect::<Vec<_>>(),
        vec![wait.wait_id],
        "the registered wait must be flipped WOKEN by the publish wiring"
    );
    assert_eq!(
        harness
            .waits
            .inspect_wait(wait.wait_id)
            .expect("wait row")
            .state,
        WaitState::Woken
    );

    // Deliver: the face only reads what the Topic authority fanned out.
    let delivered: Vec<QueueEntryRecord> = harness
        .notify
        .poll(subscription.notification_id, 8)
        .expect("face poll");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].payload, b"disk-full");
    assert_eq!(delivered[0].sequence, target);
    assert_eq!(delivered[0].idempotency_key, key(220));

    // Ack: the token-authenticated authority cursor advance.
    let ack = match harness
        .notify
        .ack(AckNotificationRequest {
            notification_id: subscription.notification_id,
            up_to_sequence: target,
            acked_at_ms: 3_300,
        })
        .expect("face ack")
    {
        nlos_notify::NotificationAckDecision::Acknowledged(receipt) => receipt,
        nlos_notify::NotificationAckDecision::Replayed(_) => panic!("fresh ack cannot replay"),
    };
    assert_eq!(ack.authority.cursor, target);
    let after = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(3))
        .expect("authority subscription after ack");
    assert_eq!(after.cursor, target);
    assert!(after.active);

    // A re-poll after the cursor advance observes nothing new: the face
    // has no buffer of its own to replay.
    assert!(
        harness
            .notify
            .poll(subscription.notification_id, 8)
            .expect("re-poll")
            .is_empty()
    );
}

/// The publish face replays without a second enqueue and without a second
/// wake flip: the channel sequence high-water and the wake report are
/// byte-identical durable facts of the authorities.
#[test]
fn publish_replay_reenqueues_nothing_and_rereports_the_original_wake() {
    let harness = Harness::new("publish-replay");
    let channel_head = create_channel(&harness.channel, 200);
    let topic = create_topic(&harness.topics, channel_head.channel_id, b"jobs", 8, 201);
    let subscription = face_subscribe(&harness.notify, topic.topic_id, 4, 3_000);
    harness
        .notify
        .register_delivery_wait(RegisterDeliveryWaitRequest {
            notification_id: subscription.notification_id,
            target_sequence: 1,
            binding: binding(12),
            idempotency_key: key(230),
            registered_at_ms: 3_100,
        })
        .expect("register wait");

    let first = match harness
        .notify
        .publish(PublishNotificationRequest {
            topic_id: topic.topic_id,
            payload: b"once".to_vec(),
            idempotency_key: key(231),
            published_at_ms: 3_200,
        })
        .expect("first publish")
    {
        nlos_notify::NotificationPublishDecision::Published(receipt) => receipt,
        nlos_notify::NotificationPublishDecision::Replayed(_) => panic!("first publish is fresh"),
    };

    let replay = match harness
        .notify
        .publish(PublishNotificationRequest {
            topic_id: topic.topic_id,
            payload: b"once".to_vec(),
            idempotency_key: key(231),
            published_at_ms: 9_999,
        })
        .expect("replayed publish")
    {
        nlos_notify::NotificationPublishDecision::Replayed(receipt) => receipt,
        nlos_notify::NotificationPublishDecision::Published(_) => {
            panic!("same-key publish must replay")
        }
    };

    assert_eq!(first.publication, replay.publication);
    assert_eq!(first.wakes.woken.len(), replay.wakes.woken.len());
    let queue = harness
        .channel
        .inspect_queue(channel_head.channel_id)
        .expect("queue");
    assert_eq!(queue.max_sequence, 1, "no second enqueue may happen");
    assert_eq!(queue.backlog_bytes, b"once".len() as u64);
}

/// Restart replay: the durable reference records resolve field-for-field
/// after every authority and the face reopen, and keep routing poll/ack.
#[test]
fn subscription_reference_records_replay_across_restart() {
    let root = Root::new("restart");
    let authorities = Authorities::open(root.path());
    let notify = NotificationService::open(
        root.path(),
        Arc::clone(&authorities.topics),
        Arc::clone(&authorities.waits),
    )
    .expect("open notification service");
    let head = create_channel(&authorities.channel, 200);
    let topic = create_topic(&authorities.topics, head.channel_id, b"events", 8, 201);

    let first = face_subscribe(&notify, topic.topic_id, 5, 3_000);
    let second = face_subscribe(&notify, topic.topic_id, 6, 3_050);
    notify
        .publish(PublishNotificationRequest {
            topic_id: topic.topic_id,
            payload: b"before-restart".to_vec(),
            idempotency_key: key(240),
            published_at_ms: 3_100,
        })
        .expect("publish before restart");
    notify
        .ack(AckNotificationRequest {
            notification_id: second.notification_id,
            up_to_sequence: 1,
            acked_at_ms: 3_150,
        })
        .expect("ack before restart");
    let views_before = notify.list_subscriptions().expect("list before restart");

    // Simulate a restart: the face and every authority are dropped and
    // reopened on the same root.
    drop(notify);
    drop(authorities);
    let authorities = Authorities::open(root.path());
    let notify = NotificationService::open(
        root.path(),
        Arc::clone(&authorities.topics),
        Arc::clone(&authorities.waits),
    )
    .expect("reopen notification service");

    let views_after = notify.list_subscriptions().expect("list after restart");
    assert_eq!(views_after, views_before);
    let views = live_views(&notify);
    assert_eq!(views.len(), 2);
    assert!(views.iter().any(|record| {
        record.notification_id == first.notification_id
            && record.consume_token == first.consume_token
    }));
    assert!(views.iter().any(|record| {
        record.notification_id == second.notification_id
            && record.consume_token == second.consume_token
    }));

    // The replayed references keep routing: the un-acked subscription
    // still delivers, and a subscribe replay stays idempotent.
    let delivered = notify
        .poll(first.notification_id, 4)
        .expect("poll after restart");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].payload, b"before-restart");
    assert!(
        notify
            .poll(second.notification_id, 4)
            .expect("acked subscription observes nothing")
            .is_empty()
    );
    match notify
        .subscribe(SubscribeNotificationRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(5),
            subscribed_at_ms: 9_999,
        })
        .expect("subscribe replay after restart")
    {
        nlos_notify::NotificationSubscribeDecision::Replayed(record) => {
            assert_eq!(record.notification_id, first.notification_id);
        }
        nlos_notify::NotificationSubscribeDecision::Subscribed(_) => {
            panic!("the authority row is active; replay is mandatory")
        }
    }
    notify
        .ack(AckNotificationRequest {
            notification_id: first.notification_id,
            up_to_sequence: 1,
            acked_at_ms: 3_900,
        })
        .expect("ack after restart");
}

/// The negative gate: with two subscriptions on one topic and one publish,
/// delivery is the single Topic-authority log entry — no per-recipient
/// copy, and the face database physically has no queue to shadow from.
#[test]
fn no_shadow_fanout_delivery_reads_only_authority_state() {
    let harness = Harness::new("no-shadow");
    let channel_head = create_channel(&harness.channel, 200);
    let topic = create_topic(
        &harness.topics,
        channel_head.channel_id,
        b"broadcast",
        8,
        201,
    );
    let first = face_subscribe(&harness.notify, topic.topic_id, 5, 3_000);
    let second = face_subscribe(&harness.notify, topic.topic_id, 6, 3_001);

    // Publish through the Topic authority directly — not through the face.
    harness
        .topics
        .publish(nlos_topic::PublishRequest {
            topic_id: topic.topic_id,
            payload: b"single-log".to_vec(),
            idempotency_key: key(250),
            published_at_ms: 3_100,
        })
        .expect("authority publish");

    let first_delivery = harness
        .notify
        .poll(first.notification_id, 4)
        .expect("poll 1");
    let second_delivery = harness
        .notify
        .poll(second.notification_id, 4)
        .expect("poll 2");
    assert_eq!(first_delivery.len(), 1);
    assert_eq!(second_delivery.len(), 1);
    assert_eq!(
        first_delivery[0], second_delivery[0],
        "both subscribers read the one shared log entry"
    );

    // The channel owns exactly one enqueue: sequence high-water advanced
    // by one and the queue holds exactly one live entry.
    let queue = harness
        .channel
        .inspect_queue(channel_head.channel_id)
        .expect("queue");
    assert_eq!(queue.max_sequence, 1);
    assert_eq!(queue.backlog_bytes, b"single-log".len() as u64);
    let live: Vec<QueueEntryRecord> = harness
        .channel
        .receive(channel_head.channel_id, 16)
        .expect("receive");
    assert_eq!(live.len(), 1, "no per-recipient copies may exist");

    // Structural negative: the face database has no queue/message table
    // at all — only the thin reference table and its unique index.
    let face_db =
        Connection::open(harness.root.path().join("notify-service.db")).expect("open face db");
    let mut statement = face_db
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','trigger')")
        .expect("list schema");
    let objects: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");
    assert_eq!(objects, vec!["notify_subscriptions".to_string()]);
}

/// Cancel routes the unsubscribe through the Topic authority (with the
/// stored credential) and removes only the thin reference; the authority
/// keeps the canonical audit row.
#[test]
fn cancel_routes_through_topic_authority_and_removes_the_reference() {
    let harness = Harness::new("cancel");
    let channel_head = create_channel(&harness.channel, 200);
    let topic = create_topic(&harness.topics, channel_head.channel_id, b"feed", 8, 201);
    let subscription = face_subscribe(&harness.notify, topic.topic_id, 7, 3_000);

    let cancel = match harness
        .notify
        .cancel(CancelNotificationRequest {
            notification_id: subscription.notification_id,
            cancelled_at_ms: 3_500,
        })
        .expect("face cancel")
    {
        nlos_notify::NotificationCancelDecision::Cancelled(receipt) => receipt,
        nlos_notify::NotificationCancelDecision::Replayed(_) => {
            panic!("fresh cancel cannot replay")
        }
    };
    assert_eq!(cancel.authority.unsubscribed_at_ms, 3_500);

    // The authority row is the canonical fact: inactive, audit intact.
    let authority_record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(7))
        .expect("authority subscription after cancel");
    assert!(!authority_record.active);

    // The face reference is gone; routing through it fails typed.
    assert!(live_views(&harness.notify).is_empty());
    assert!(matches!(
        harness.notify.poll(subscription.notification_id, 4),
        Err(NotifyError::NotificationNotFound(id)) if id == subscription.notification_id
    ));
    assert!(matches!(
        harness.notify.cancel(CancelNotificationRequest {
            notification_id: subscription.notification_id,
            cancelled_at_ms: 3_600,
        }),
        Err(NotifyError::NotificationNotFound(_))
    ));

    // A re-subscribe through the face mints a fresh generation/token and
    // the same derived face identity.
    let resubscribed = face_subscribe(&harness.notify, topic.topic_id, 7, 4_000);
    assert_eq!(resubscribed.notification_id, subscription.notification_id);
    let refreshed = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(7))
        .expect("re-subscribed");
    assert!(refreshed.active);
    assert_eq!(
        refreshed.subscription_generation,
        authority_record.subscription_generation + 1
    );
    assert_eq!(resubscribed.consume_token, refreshed.consume_token);
}

/// List reports the live authority state per reference — never a copy of
/// it: an out-of-band authority unsubscribe shows through the same thin
/// record with `active == false`.
#[test]
fn list_reports_live_authority_state_through_the_thin_reference() {
    let harness = Harness::new("list-live");
    let channel_head = create_channel(&harness.channel, 200);
    let topic_a = create_topic(&harness.topics, channel_head.channel_id, b"a", 8, 201);
    let topic_b = create_topic(&harness.topics, channel_head.channel_id, b"b", 8, 202);
    let kept = face_subscribe(&harness.notify, topic_a.topic_id, 8, 3_000);
    let dropped = face_subscribe(&harness.notify, topic_b.topic_id, 9, 3_001);

    // Unsubscribe out-of-band at the authority, presenting the very token
    // the face record references.
    harness
        .topics
        .unsubscribe_with_token(
            UnsubscribeRequest {
                topic_id: topic_b.topic_id,
                subscriber_key: subscriber(9),
                unsubscribed_at_ms: 3_900,
            },
            &dropped.consume_token,
        )
        .expect("authority unsubscribe");

    let views = harness.notify.list_subscriptions().expect("list");
    assert_eq!(views.len(), 2);
    let mut by_key: Vec<(nlos_topic::SubscriberKey, bool, NotificationId)> = views
        .into_iter()
        .map(|view| match view {
            NotificationSubscriptionView::Live {
                subscription,
                authority,
            } => (
                subscription.subscriber_key,
                authority.active,
                subscription.notification_id,
            ),
            NotificationSubscriptionView::Dangling { .. } => {
                panic!("authority rows exist; no dangling view expected")
            }
        })
        .collect();
    by_key.sort_by_key(|(key, _, _)| key.into_bytes());
    assert_eq!(by_key[0].0, subscriber(8));
    assert!(by_key[0].1);
    assert_eq!(by_key[0].2, kept.notification_id);
    assert_eq!(by_key[1].0, subscriber(9));
    assert!(!by_key[1].1, "the authority state must show through");
    assert_eq!(by_key[1].2, dropped.notification_id);
}

/// A reference whose authority rows have diverged away (the topic
/// authority was rebuilt elsewhere) surfaces as `Dangling`, never as a
/// silently copied state and never dropped from the list.
#[test]
fn list_surfaces_dangling_references_when_the_authority_diverged() {
    let harness = Harness::new("dangling");
    let channel_head = create_channel(&harness.channel, 200);
    let topic = create_topic(&harness.topics, channel_head.channel_id, b"gone", 8, 201);
    let subscription = face_subscribe(&harness.notify, topic.topic_id, 10, 3_000);
    drop(harness.notify);

    // A fresh topic/wait authority pair on a different root, bound to the
    // face's database: the reference now resolves to nothing.
    let other_root = Root::new("dangling-other");
    let other = Authorities::open(other_root.path());
    let face_over_diverged =
        NotificationService::open(harness.root.path(), other.topics, other.waits)
            .expect("reopen face over diverged authority");

    let views = face_over_diverged.list_subscriptions().expect("list");
    assert_eq!(views.len(), 1);
    match &views[0] {
        NotificationSubscriptionView::Dangling { subscription: thin } => {
            assert_eq!(thin.notification_id, subscription.notification_id);
            assert_eq!(thin.topic_id, topic.topic_id);
        }
        NotificationSubscriptionView::Live { .. } => {
            panic!("the diverged authority holds no such subscription")
        }
    }

    // Typed failures on the dangling routing paths.
    assert!(matches!(
        face_over_diverged.poll(subscription.notification_id, 4),
        Err(NotifyError::Topic(TopicAuthorityError::TopicNotFound(_)))
    ));
}

#[test]
fn typed_failures_for_unknown_topic_and_unknown_notification() {
    let harness = Harness::new("typed-unknown");
    let missing_topic = nlos_topic::TopicId::from_bytes([0x99; 16]);
    assert!(matches!(
        harness.notify.subscribe(SubscribeNotificationRequest {
            topic_id: missing_topic,
            subscriber_key: subscriber(21),
            subscribed_at_ms: 3_000,
        }),
        Err(NotifyError::Topic(TopicAuthorityError::TopicNotFound(id))) if id == missing_topic
    ));

    let unknown = NotificationId::from_bytes([0x77; 16]);
    assert!(matches!(
        harness.notify.poll(unknown, 4),
        Err(NotifyError::NotificationNotFound(id)) if id == unknown
    ));
    assert!(matches!(
        harness.notify.ack(AckNotificationRequest {
            notification_id: unknown,
            up_to_sequence: 1,
            acked_at_ms: 3_001,
        }),
        Err(NotifyError::NotificationNotFound(_))
    ));
    assert!(matches!(
        harness.notify.cancel(CancelNotificationRequest {
            notification_id: unknown,
            cancelled_at_ms: 3_002,
        }),
        Err(NotifyError::NotificationNotFound(_))
    ));
    assert!(matches!(
        harness
            .notify
            .register_delivery_wait(RegisterDeliveryWaitRequest {
                notification_id: unknown,
                target_sequence: 1,
                binding: binding(13),
                idempotency_key: key(60),
                registered_at_ms: 3_003,
            }),
        Err(NotifyError::NotificationNotFound(_))
    ));

    // The rejected subscribe left zero face state.
    assert!(
        harness
            .notify
            .list_subscriptions()
            .expect("list")
            .is_empty()
    );
}

#[test]
fn typed_failures_for_ack_sequence_bounds() {
    let harness = Harness::new("typed-ack");
    let channel_head = create_channel(&harness.channel, 200);
    let topic = create_topic(&harness.topics, channel_head.channel_id, b"acks", 8, 201);
    let subscription = face_subscribe(&harness.notify, topic.topic_id, 22, 3_000);
    harness
        .topics
        .publish(nlos_topic::PublishRequest {
            topic_id: topic.topic_id,
            payload: b"one".to_vec(),
            idempotency_key: key(61),
            published_at_ms: 3_100,
        })
        .expect("publish");

    // The subscribe point is the channel high-water at subscribe time
    // (sequence 1 landed afterwards): advancing beyond the high-water
    // fails typed inside the authority with zero durable movement.
    assert!(matches!(
        harness.notify.ack(AckNotificationRequest {
            notification_id: subscription.notification_id,
            up_to_sequence: 99,
            acked_at_ms: 3_200,
        }),
        Err(NotifyError::Topic(TopicAuthorityError::InvalidSequence(_)))
    ));
    let record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(22))
        .expect("subscription");
    assert_eq!(record.cursor, 0);

    harness
        .notify
        .ack(AckNotificationRequest {
            notification_id: subscription.notification_id,
            up_to_sequence: 1,
            acked_at_ms: 3_201,
        })
        .expect("ack to the high-water");
    // A regression below the consume point fails typed; the cursor holds.
    assert!(matches!(
        harness.notify.ack(AckNotificationRequest {
            notification_id: subscription.notification_id,
            up_to_sequence: 0,
            acked_at_ms: 3_202,
        }),
        Err(NotifyError::Topic(TopicAuthorityError::InvalidSequence(_)))
    ));
    let advanced = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(22))
        .expect("subscription after regression attempt");
    assert_eq!(advanced.cursor, 1);
}

/// A stale stored credential (the authority re-subscribed the key
/// out-of-band, bumping the generation) fails closed on ack; a face
/// re-subscribe self-heals the reference to the current token.
#[test]
fn stale_stored_token_fails_closed_then_self_heals_on_resubscribe() {
    let harness = Harness::new("stale-token");
    let channel_head = create_channel(&harness.channel, 200);
    let topic = create_topic(&harness.topics, channel_head.channel_id, b"tokens", 8, 201);
    let subscription = face_subscribe(&harness.notify, topic.topic_id, 23, 3_000);
    harness
        .topics
        .publish(nlos_topic::PublishRequest {
            topic_id: topic.topic_id,
            payload: b"payload".to_vec(),
            idempotency_key: key(62),
            published_at_ms: 3_100,
        })
        .expect("publish");

    // Out-of-band unsubscribe + re-subscribe at the authority: the key's
    // generation bumps and a fresh token is issued.
    harness
        .topics
        .unsubscribe_with_token(
            UnsubscribeRequest {
                topic_id: topic.topic_id,
                subscriber_key: subscriber(23),
                unsubscribed_at_ms: 3_200,
            },
            &subscription.consume_token,
        )
        .expect("authority unsubscribe");
    match harness
        .topics
        .subscribe(SubscribeRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(23),
            subscribed_at_ms: 3_300,
        })
        .expect("authority re-subscribe")
    {
        SubscribeDecision::Subscribed(record) => {
            assert_ne!(record.consume_token, subscription.consume_token);
        }
        SubscribeDecision::Replayed(_) => panic!("re-subscribe after unsubscribe is fresh"),
    }

    // The face still holds the previous generation's token: ack fails
    // closed at the authority boundary.
    assert!(matches!(
        harness.notify.ack(AckNotificationRequest {
            notification_id: subscription.notification_id,
            up_to_sequence: 1,
            acked_at_ms: 3_400,
        }),
        Err(NotifyError::Topic(
            TopicAuthorityError::ConsumptionTokenMismatch(_)
        ))
    ));

    // A face re-subscribe replays against the active authority row and
    // refreshes the stored credential; ack then succeeds.
    let healed = match harness
        .notify
        .subscribe(SubscribeNotificationRequest {
            topic_id: topic.topic_id,
            subscriber_key: subscriber(23),
            subscribed_at_ms: 3_500,
        })
        .expect("face re-subscribe")
    {
        nlos_notify::NotificationSubscribeDecision::Replayed(record) => record,
        nlos_notify::NotificationSubscribeDecision::Subscribed(_) => {
            panic!("the authority row is active; the face subscribe must replay")
        }
    };
    assert_eq!(healed.notification_id, subscription.notification_id);
    let authority_record = harness
        .topics
        .inspect_subscription(topic.topic_id, subscriber(23))
        .expect("subscription");
    assert_eq!(healed.consume_token, authority_record.consume_token);
    harness
        .notify
        .ack(AckNotificationRequest {
            notification_id: healed.notification_id,
            up_to_sequence: 1,
            acked_at_ms: 3_600,
        })
        .expect("ack after heal");
}

#[test]
fn typed_failures_for_delivery_wait_registration() {
    let harness = Harness::new("typed-wait");
    let channel_head = create_channel(&harness.channel, 200);
    let topic = create_topic(&harness.topics, channel_head.channel_id, b"waits", 8, 201);
    let subscription = face_subscribe(&harness.notify, topic.topic_id, 24, 3_000);

    assert!(matches!(
        harness
            .notify
            .register_delivery_wait(RegisterDeliveryWaitRequest {
                notification_id: subscription.notification_id,
                target_sequence: 0,
                binding: binding(14),
                idempotency_key: key(70),
                registered_at_ms: 3_100,
            }),
        Err(NotifyError::Wait(
            nlos_wait::WaitAuthorityError::InvalidSequence(_)
        ))
    ));
    assert!(matches!(
        harness
            .notify
            .register_delivery_wait(RegisterDeliveryWaitRequest {
                notification_id: subscription.notification_id,
                target_sequence: 1,
                binding: BindingId::from_bytes([0; 16]),
                idempotency_key: key(71),
                registered_at_ms: 3_101,
            }),
        Err(NotifyError::Wait(
            nlos_wait::WaitAuthorityError::InvalidBinding
        ))
    ));
}

/// The wake wiring is per-target, exactly as the Wait authority defines
/// it: a publish flips only the waits whose target it covered.
#[test]
fn publish_notify_flips_only_covered_waits() {
    let harness = Harness::new("wait-coverage");
    let channel_head = create_channel(&harness.channel, 200);
    let topic = create_topic(
        &harness.topics,
        channel_head.channel_id,
        b"coverage",
        8,
        201,
    );
    let subscription = face_subscribe(&harness.notify, topic.topic_id, 25, 3_000);

    let early = match harness
        .notify
        .register_delivery_wait(RegisterDeliveryWaitRequest {
            notification_id: subscription.notification_id,
            target_sequence: 1,
            binding: binding(15),
            idempotency_key: key(80),
            registered_at_ms: 3_100,
        })
        .expect("register early wait")
    {
        nlos_wait::RegisterDecision::Registered(record) => record,
        nlos_wait::RegisterDecision::Replayed(_) => panic!("fresh wait cannot replay"),
    };
    let late = match harness
        .notify
        .register_delivery_wait(RegisterDeliveryWaitRequest {
            notification_id: subscription.notification_id,
            target_sequence: 3,
            binding: binding(16),
            idempotency_key: key(81),
            registered_at_ms: 3_101,
        })
        .expect("register late wait")
    {
        nlos_wait::RegisterDecision::Registered(record) => record,
        nlos_wait::RegisterDecision::Replayed(_) => panic!("fresh wait cannot replay"),
    };

    for seed in [82u8, 83u8] {
        harness
            .notify
            .publish(PublishNotificationRequest {
                topic_id: topic.topic_id,
                payload: vec![seed],
                idempotency_key: key(seed),
                published_at_ms: 3_200,
            })
            .expect("publish");
    }
    assert_eq!(
        harness
            .waits
            .inspect_wait(early.wait_id)
            .expect("early")
            .state,
        WaitState::Woken
    );
    assert_eq!(
        harness
            .waits
            .inspect_wait(late.wait_id)
            .expect("late")
            .state,
        WaitState::Pending,
        "sequence 2 of 3 must not wake a target-3 wait"
    );

    harness
        .notify
        .publish(PublishNotificationRequest {
            topic_id: topic.topic_id,
            payload: b"third".to_vec(),
            idempotency_key: key(84),
            published_at_ms: 3_300,
        })
        .expect("third publish");
    assert_eq!(
        harness
            .waits
            .inspect_wait(late.wait_id)
            .expect("late")
            .state,
        WaitState::Woken
    );
}
