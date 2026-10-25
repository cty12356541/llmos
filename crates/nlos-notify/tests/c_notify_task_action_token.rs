//! C-NOTIFY-001 (`W65-NOTIFY-EXT`) integration tests:
//! - `[TASK-NOTIFY-001]` / `[TASK-STATE-001]` background task transition
//!   notifications over the single `TopicAuthority` + `WaitAuthority` commit
//!   path with zero shadow fanout;
//! - `[DUI-NOTIFY-001]` cryptographically bound one-shot notification action
//!   tokens (`notify-service.db` schema v2 `STRICT` tables + immutability /
//!   anti-resurrection `SQLite` triggers);
//! - `[DUI-OFFLINE-001]` deterministic bounded cursor-paginated local offline
//!   notification history and monotonic retention pruning;
//! - Schema `v1 -> v2` seamless in-place migration.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest};
use nlos_notify::{
    AckNotificationRequest, ActionTokenId, ActionTokenStatus, DeliveredTaskNotification,
    IssueActionTokenDecision, IssueNotificationActionTokenRequest, MAX_HISTORY_PAGE_LIMIT,
    NotificationId, NotificationPublishDecision, NotificationService,
    NotificationSubscribeDecision, NotifyError, PublishNotificationRequest,
    RecordDeliveryHistoryRequest, RecordHistoryDecision, RedeemNotificationActionRequest,
    RegisterDeliveryWaitRequest, SubscribeNotificationRequest, TASK_NOTIFICATION_WIRE_LEN,
    TASK_NOTIFICATION_WIRE_MAGIC, TaskHistoryMetadata, TaskNotificationEnvelope,
    TaskNotificationWireError, TaskTransitionKind, action_secret_commitment,
    decode_task_notification_envelope, derive_action_secret_proof,
    encode_task_notification_envelope,
};
use nlos_topic::{CreateTopicRequest, SubscriberKey, TopicAuthority, TopicDecision, TopicPolicy};
use nlos_types::{ApplicationId, ChannelId, Generation, IdempotencyKey, ResourceAccountId, TaskId};
use nlos_wait::{BindingId, RegisterDecision, WaitAuthority, WaitState};
use rusqlite::{Connection, params};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-c-notify-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Drops `notify` and all `SQLite`-backed authorities BEFORE `dir` (the final
/// struct field) so Windows releases WAL/SHM handles cleanly prior to removal.
struct Harness {
    notify: NotificationService,
    waits: Arc<WaitAuthority>,
    topics: Arc<TopicAuthority>,
    channel: Arc<ChannelAuthority>,
    dir: TempRoot,
}

impl Harness {
    fn new(label: &str) -> Self {
        let dir = TempRoot::new(label);
        let (channel, topics, waits, notify) = open_stack(dir.path());
        Self {
            notify,
            waits,
            topics,
            channel,
            dir,
        }
    }

    fn root_path(&self) -> &Path {
        self.dir.path()
    }
}

fn open_stack(
    root: &Path,
) -> (
    Arc<ChannelAuthority>,
    Arc<TopicAuthority>,
    Arc<WaitAuthority>,
    NotificationService,
) {
    let channel = Arc::new(ChannelAuthority::open(root).expect("open channel authority"));
    let topics =
        Arc::new(TopicAuthority::open(root, Arc::clone(&channel)).expect("open topic authority"));
    let waits =
        Arc::new(WaitAuthority::open(root, Arc::clone(&channel)).expect("open wait authority"));
    let notify = NotificationService::open(root, Arc::clone(&topics), Arc::clone(&waits))
        .expect("open notification service");
    (channel, topics, waits, notify)
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn app(seed: u8) -> ApplicationId {
    ApplicationId::from_bytes([seed; 16])
}

fn task(seed: u8) -> TaskId {
    TaskId::from_bytes([seed; 16])
}

fn subscriber(seed: u8) -> SubscriberKey {
    SubscriberKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
}

fn setup_topic(
    harness: &Harness,
    name: &[u8],
    chan_seed: u8,
    topic_seed: u8,
) -> (ChannelId, nlos_topic::TopicId) {
    setup_topic_on(
        &harness.channel,
        &harness.topics,
        name,
        chan_seed,
        topic_seed,
    )
}

fn setup_topic_on(
    channel: &ChannelAuthority,
    topics: &TopicAuthority,
    name: &[u8],
    chan_seed: u8,
    topic_seed: u8,
) -> (ChannelId, nlos_topic::TopicId) {
    let channel_record = match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 32_768,
            policy_digest: [0x55; 32],
            idempotency_key: key(chan_seed),
            created_at_ms: 1_000,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh channel cannot replay"),
    };
    let topic_record = match topics
        .create_topic(CreateTopicRequest {
            channel_id: channel_record.channel_id,
            name: name.to_vec(),
            policy: TopicPolicy {
                max_recipients: 8,
                delivery_attempts: 4,
                cascade_depth: 2,
                retained_bytes: 32_768,
                retention_ms: 86_400_000,
                payer: ResourceAccountId::from_bytes([0x09; 16]),
            },
            idempotency_key: key(topic_seed),
            created_at_ms: 1_500,
        })
        .expect("create topic")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh topic cannot replay"),
    };
    (channel_record.channel_id, topic_record.topic_id)
}

fn page_sequences(page: &nlos_notify::NotificationHistoryPage) -> Vec<u64> {
    page.entries
        .iter()
        .map(|entry| entry.channel_sequence)
        .collect()
}

fn subscribe_face(
    notify: &NotificationService,
    topic_id: nlos_topic::TopicId,
    sub_seed: u8,
) -> NotificationId {
    match notify
        .subscribe(SubscribeNotificationRequest {
            topic_id,
            subscriber_key: subscriber(sub_seed),
            subscribed_at_ms: 2_000,
        })
        .expect("subscribe")
    {
        NotificationSubscribeDecision::Subscribed(sub) => sub.notification_id,
        NotificationSubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    }
}

fn sample_envelope(
    transition: TaskTransitionKind,
    at_ms: u64,
    requires_action: bool,
) -> TaskNotificationEnvelope {
    TaskNotificationEnvelope {
        task_id: task(0xA1),
        application_id: app(0xB1),
        task_generation: Generation::INITIAL,
        transition,
        occurred_at_ms: at_ms,
        summary_digest: [0xCC; 32],
        requires_user_action: requires_action,
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Scenario 1 (`[TASK-NOTIFY-001]`, `[TASK-STATE-001]`): background task
/// transition publish -> wake -> poll -> ack roundtrip across multiple
/// transition states with two independent subscribers and zero shadow fanout.
#[test]
fn background_task_transitions_publish_wake_and_poll_without_shadow_fanout() {
    let harness = Harness::new("task-roundtrip");
    let (channel_id, topic_id) = setup_topic(&harness, b"system/task-transitions", 1, 2);
    let desktop_sub = subscribe_face(&harness.notify, topic_id, 10);
    let tray_sub = subscribe_face(&harness.notify, topic_id, 11);

    let wait_row = match harness
        .notify
        .register_delivery_wait(RegisterDeliveryWaitRequest {
            notification_id: desktop_sub,
            target_sequence: 4,
            binding: binding(21),
            idempotency_key: key(30),
            registered_at_ms: 2_100,
        })
        .expect("register wait")
    {
        RegisterDecision::Registered(row) => row,
        RegisterDecision::Replayed(_) => panic!("fresh wait"),
    };
    assert_eq!(wait_row.state, WaitState::Pending);

    let lifecycle = [
        (TaskTransitionKind::Proposed, 3_000u64, false, 40u8),
        (TaskTransitionKind::WaitingConsent, 3_100, true, 41),
        (TaskTransitionKind::Running, 3_200, false, 42),
        (TaskTransitionKind::Completed, 3_300, false, 43),
    ];

    for (kind, at_ms, needs_action, idem) in lifecycle {
        let env = sample_envelope(kind, at_ms, needs_action);
        let decision = harness
            .notify
            .publish_task_transition(topic_id, &env, key(idem))
            .expect("publish task transition");
        assert!(matches!(
            decision,
            NotificationPublishDecision::Published(_)
        ));
    }

    let replay = harness
        .notify
        .publish_task_transition(
            topic_id,
            &sample_envelope(TaskTransitionKind::Completed, 3_300, false),
            key(43),
        )
        .expect("replay publish_task_transition");
    assert!(matches!(replay, NotificationPublishDecision::Replayed(_)));

    assert_eq!(
        harness
            .waits
            .inspect_wait(wait_row.wait_id)
            .expect("wait")
            .state,
        WaitState::Woken
    );

    let desktop_items = harness
        .notify
        .poll_task_notifications(desktop_sub, 8)
        .expect("poll desktop");
    let tray_items = harness
        .notify
        .poll_task_notifications(tray_sub, 8)
        .expect("poll tray");
    assert_eq!(desktop_items.len(), 4);
    assert_eq!(tray_items.len(), 4);
    assert_eq!(
        desktop_items[1].envelope.transition,
        TaskTransitionKind::WaitingConsent
    );
    assert!(desktop_items[1].envelope.requires_user_action);
    assert_eq!(
        desktop_items[3].envelope.transition,
        TaskTransitionKind::Completed
    );

    let queue = harness.channel.inspect_queue(channel_id).expect("queue");
    assert_eq!(queue.max_sequence, 4);
    assert_eq!(queue.backlog_bytes, 4 * (TASK_NOTIFICATION_WIRE_LEN as u64));

    harness
        .notify
        .ack(AckNotificationRequest {
            notification_id: desktop_sub,
            up_to_sequence: 4,
            acked_at_ms: 3_500,
        })
        .expect("ack desktop");
    assert_eq!(
        harness
            .notify
            .poll_task_notifications(desktop_sub, 8)
            .expect("repoll desktop"),
        [] as [DeliveredTaskNotification; 0]
    );
    assert_eq!(
        harness
            .notify
            .poll_task_notifications(tray_sub, 8)
            .expect("tray independent cursor")
            .len(),
        4
    );
}

/// Scenario 2: malformed/truncated/tampered `TaskNotificationEnvelope` wire
/// payloads fail closed both in `decode_task_notification_envelope` and in
/// `poll_task_notifications` without advancing the subscriber cursor.
#[test]
fn malformed_and_truncated_task_notification_payloads_fail_closed() {
    let valid = encode_task_notification_envelope(&sample_envelope(
        TaskTransitionKind::WaitingInput,
        5_000,
        true,
    ));
    assert_eq!(valid.len(), TASK_NOTIFICATION_WIRE_LEN);
    assert_eq!(&valid[..9], TASK_NOTIFICATION_WIRE_MAGIC);

    assert_eq!(
        decode_task_notification_envelope(&valid[..40]),
        Err(TaskNotificationWireError::InvalidLength {
            expected: TASK_NOTIFICATION_WIRE_LEN,
            actual: 40,
        })
    );
    let mut oversized = valid.clone();
    oversized.push(0);
    assert_eq!(
        decode_task_notification_envelope(&oversized),
        Err(TaskNotificationWireError::InvalidLength {
            expected: TASK_NOTIFICATION_WIRE_LEN,
            actual: TASK_NOTIFICATION_WIRE_LEN + 1,
        })
    );

    let mut bad_magic = valid.clone();
    bad_magic[8] = 0x02;
    assert_eq!(
        decode_task_notification_envelope(&bad_magic),
        Err(TaskNotificationWireError::InvalidMagic)
    );

    let mut zero_gen = valid.clone();
    zero_gen[41..49].copy_from_slice(&0u64.to_be_bytes());
    assert_eq!(
        decode_task_notification_envelope(&zero_gen),
        Err(TaskNotificationWireError::ZeroTaskGeneration)
    );

    let mut bad_tag = valid.clone();
    bad_tag[49] = 0xFE;
    assert_eq!(
        decode_task_notification_envelope(&bad_tag),
        Err(TaskNotificationWireError::UnknownTransitionTag(0xFE))
    );

    let mut bad_flag = valid;
    bad_flag[90] = 2;
    assert_eq!(
        decode_task_notification_envelope(&bad_flag),
        Err(TaskNotificationWireError::InvalidUserActionFlag(2))
    );

    let harness = Harness::new("task-malformed-poll");
    let (_channel_id, topic_id) = setup_topic(&harness, b"system/mixed", 3, 4);
    let sub_id = subscribe_face(&harness.notify, topic_id, 12);
    harness
        .notify
        .publish(PublishNotificationRequest {
            topic_id,
            payload: b"not-a-task-envelope".to_vec(),
            idempotency_key: key(50),
            published_at_ms: 5_100,
        })
        .expect("publish raw");
    assert!(matches!(
        harness.notify.poll_task_notifications(sub_id, 4),
        Err(NotifyError::MalformedTaskNotification(
            TaskNotificationWireError::InvalidLength { .. }
        ))
    ));
}

/// Scenario 3 (`[DUI-NOTIFY-001]`): happy-path `issue_action_token` -> durable
/// `SQLite` persistence across drop & reopen -> `redeem_action_token`,
/// verifying raw secrets never hit disk.
#[test]
fn action_token_happy_path_persists_across_reopen_without_storing_raw_secret() {
    let root = TempRoot::new("action-happy-persist");
    let secret_nonce = [0x7A; 32];
    let (notification_id, issued_token) = {
        let (channel, topics, _waits, notify) = open_stack(root.path());
        let (_channel_id, topic_id) = setup_topic_on(&channel, &topics, b"desktop/actions", 1, 2);
        let sub_id = subscribe_face(&notify, topic_id, 20);
        let token = match notify
            .issue_action_token(IssueNotificationActionTokenRequest {
                notification_id: sub_id,
                application_id: app(0x31),
                task_id: task(0x41),
                delivery_sequence: 1,
                action_code: 7,
                secret_nonce,
                issued_at_ms: 2_000,
                expires_at_ms: 10_000,
                idempotency_key: key(60),
            })
            .expect("issue token")
        {
            IssueActionTokenDecision::Issued(t) => t,
            IssueActionTokenDecision::Replayed(_) => panic!("fresh issue"),
        };
        assert_eq!(
            token.secret_proof,
            derive_action_secret_proof(
                token.token_id,
                sub_id,
                app(0x31),
                task(0x41),
                1,
                7,
                &secret_nonce,
            )
        );
        let stored = notify
            .inspect_action_token(token.token_id)
            .expect("inspect");
        assert_eq!(
            stored.secret_commitment,
            action_secret_commitment(&token.secret_proof)
        );
        assert_eq!(stored.status, ActionTokenStatus::Issued);
        (sub_id, token)
    };

    let db_bytes = std::fs::read(root.path().join("notify-service.db")).expect("read db");
    assert!(!contains_subslice(&db_bytes, &secret_nonce));
    assert!(!contains_subslice(&db_bytes, &issued_token.secret_proof));

    let receipt = {
        let (_channel, _topics, _waits, notify) = open_stack(root.path());
        notify
            .redeem_action_token(RedeemNotificationActionRequest {
                token_id: issued_token.token_id,
                presented_proof: issued_token.secret_proof,
                caller_notification_id: notification_id,
                caller_application_id: app(0x31),
                caller_task_id: task(0x41),
                action_code: 7,
                redeemed_at_ms: 4_500,
            })
            .expect("redeem after reopen")
    };
    assert_eq!(receipt.token_id, issued_token.token_id);
    assert_eq!(receipt.redeemed_at_ms, 4_500);

    let (_channel, _topics, _waits, notify) = open_stack(root.path());
    let after = notify
        .inspect_action_token(issued_token.token_id)
        .expect("inspect after second reopen");
    assert_eq!(
        after.status,
        ActionTokenStatus::Redeemed {
            redeemed_at_ms: 4_500,
            receipt_id: receipt.receipt_id,
        }
    );
}

/// Scenario 4 (`[DUI-NOTIFY-001]`): `issue_action_token` verifies
/// `notification_id` exists first, rejects malformed bounds, replays
/// idempotently on identical `idempotency_key`, and rejects conflicting
/// rebinding.
#[test]
fn issue_action_token_verifies_subscription_bounds_and_idempotency() {
    let harness = Harness::new("issue-validation");
    let (_channel_id, topic_id) = setup_topic(&harness, b"desktop/issue", 5, 6);
    let sub_id = subscribe_face(&harness.notify, topic_id, 21);

    let unknown_notif = NotificationId::from_bytes([0xEE; 16]);
    assert!(matches!(
        harness
            .notify
            .issue_action_token(IssueNotificationActionTokenRequest {
                notification_id: unknown_notif,
                application_id: app(1),
                task_id: task(1),
                delivery_sequence: 1,
                action_code: 1,
                secret_nonce: [0x11; 32],
                issued_at_ms: 1_000,
                expires_at_ms: 2_000,
                idempotency_key: key(70),
            }),
        Err(NotifyError::NotificationNotFound(id)) if id == unknown_notif
    ));

    assert!(matches!(
        harness
            .notify
            .issue_action_token(IssueNotificationActionTokenRequest {
                notification_id: sub_id,
                application_id: app(1),
                task_id: task(1),
                delivery_sequence: 0,
                action_code: 1,
                secret_nonce: [0x11; 32],
                issued_at_ms: 1_000,
                expires_at_ms: 2_000,
                idempotency_key: key(71),
            }),
        Err(NotifyError::InvalidActionTokenRequest(_))
    ));
    assert!(matches!(
        harness
            .notify
            .issue_action_token(IssueNotificationActionTokenRequest {
                notification_id: sub_id,
                application_id: app(1),
                task_id: task(1),
                delivery_sequence: 1,
                action_code: 1,
                secret_nonce: [0u8; 32],
                issued_at_ms: 1_000,
                expires_at_ms: 2_000,
                idempotency_key: key(72),
            }),
        Err(NotifyError::InvalidActionTokenRequest(_))
    ));

    let req = IssueNotificationActionTokenRequest {
        notification_id: sub_id,
        application_id: app(0x32),
        task_id: task(0x42),
        delivery_sequence: 2,
        action_code: 3,
        secret_nonce: [0x99; 32],
        issued_at_ms: 2_000,
        expires_at_ms: 8_000,
        idempotency_key: key(73),
    };
    let first = match harness.notify.issue_action_token(req).expect("first issue") {
        IssueActionTokenDecision::Issued(t) => t,
        IssueActionTokenDecision::Replayed(_) => panic!("fresh"),
    };
    let replay = match harness
        .notify
        .issue_action_token(req)
        .expect("replay issue")
    {
        IssueActionTokenDecision::Replayed(t) => t,
        IssueActionTokenDecision::Issued(_) => panic!("must replay"),
    };
    assert_eq!(first, replay);

    let mut conflicting = req;
    conflicting.action_code = 99;
    assert!(matches!(
        harness.notify.issue_action_token(conflicting),
        Err(NotifyError::ActionIdempotencyConflict(k)) if k == key(73)
    ));
}

/// Scenario 5 (`[DUI-NOTIFY-001]`): cross-`Application` spoofing (`App B`
/// attempting to redeem `App A`'s action token, even with valid `token_id`
/// and valid `presented_proof`) fails closed with `ActionCrossApplicationSpoof`
/// and leaves `App A`'s token unconsumed so `App A` can still redeem it once.
#[test]
fn cross_application_spoof_is_rejected_and_leaves_token_unconsumed_for_owner() {
    let harness = Harness::new("cross-app-spoof");
    let (_channel_id, topic_id) = setup_topic(&harness, b"desktop/spoof", 7, 8);
    let sub_id = subscribe_face(&harness.notify, topic_id, 22);

    let app_a = app(0xAA);
    let app_b = app(0xBB);
    let token = harness
        .notify
        .issue_action_token(IssueNotificationActionTokenRequest {
            notification_id: sub_id,
            application_id: app_a,
            task_id: task(0x55),
            delivery_sequence: 1,
            action_code: 10,
            secret_nonce: [0x42; 32],
            issued_at_ms: 3_000,
            expires_at_ms: 9_000,
            idempotency_key: key(80),
        })
        .expect("issue")
        .token();

    let spoof_attempt = harness
        .notify
        .redeem_action_token(RedeemNotificationActionRequest {
            token_id: token.token_id,
            presented_proof: token.secret_proof,
            caller_notification_id: sub_id,
            caller_application_id: app_b,
            caller_task_id: task(0x55),
            action_code: 10,
            redeemed_at_ms: 3_500,
        });
    assert!(matches!(
        spoof_attempt,
        Err(NotifyError::ActionCrossApplicationSpoof { expected, actual })
            if expected == app_a && actual == app_b
    ));

    assert_eq!(
        harness
            .notify
            .inspect_action_token(token.token_id)
            .expect("inspect")
            .status,
        ActionTokenStatus::Issued
    );
    let receipt = harness
        .notify
        .redeem_action_token(RedeemNotificationActionRequest {
            token_id: token.token_id,
            presented_proof: token.secret_proof,
            caller_notification_id: sub_id,
            caller_application_id: app_a,
            caller_task_id: task(0x55),
            action_code: 10,
            redeemed_at_ms: 3_600,
        })
        .expect("legitimate owner redeem");
    assert_eq!(receipt.application_id, app_a);
}

/// Scenario 6 (`[DUI-NOTIFY-001]`): wrong `NotificationId`, wrong `TaskId`,
/// wrong `action_code`, invalid `presented_proof`, and unknown `token_id` all
/// fail closed with distinct typed `NotifyError` variants.
#[test]
fn wrong_bindings_invalid_proof_and_unknown_token_fail_closed() {
    let harness = Harness::new("binding-mismatch");
    let (_channel_id, topic_id) = setup_topic(&harness, b"desktop/bindings", 9, 10);
    let sub_id = subscribe_face(&harness.notify, topic_id, 23);
    let other_sub_id = subscribe_face(&harness.notify, topic_id, 24);

    let token = harness
        .notify
        .issue_action_token(IssueNotificationActionTokenRequest {
            notification_id: sub_id,
            application_id: app(0x10),
            task_id: task(0x20),
            delivery_sequence: 1,
            action_code: 5,
            secret_nonce: [0x66; 32],
            issued_at_ms: 3_000,
            expires_at_ms: 9_000,
            idempotency_key: key(81),
        })
        .expect("issue")
        .token();

    let unknown_id = ActionTokenId::from_bytes([0xFF; 16]);
    assert!(matches!(
        harness
            .notify
            .redeem_action_token(RedeemNotificationActionRequest {
                token_id: unknown_id,
                presented_proof: token.secret_proof,
                caller_notification_id: sub_id,
                caller_application_id: app(0x10),
                caller_task_id: task(0x20),
                action_code: 5,
                redeemed_at_ms: 4_000,
            }),
        Err(NotifyError::ActionTokenNotFound(id)) if id == unknown_id
    ));

    assert!(matches!(
        harness
            .notify
            .redeem_action_token(RedeemNotificationActionRequest {
                token_id: token.token_id,
                presented_proof: token.secret_proof,
                caller_notification_id: other_sub_id,
                caller_application_id: app(0x10),
                caller_task_id: task(0x20),
                action_code: 5,
                redeemed_at_ms: 4_000,
            }),
        Err(NotifyError::ActionBindingMismatch(_))
    ));

    assert!(matches!(
        harness
            .notify
            .redeem_action_token(RedeemNotificationActionRequest {
                token_id: token.token_id,
                presented_proof: token.secret_proof,
                caller_notification_id: sub_id,
                caller_application_id: app(0x10),
                caller_task_id: task(0x99),
                action_code: 5,
                redeemed_at_ms: 4_000,
            }),
        Err(NotifyError::ActionBindingMismatch(_))
    ));

    assert!(matches!(
        harness
            .notify
            .redeem_action_token(RedeemNotificationActionRequest {
                token_id: token.token_id,
                presented_proof: token.secret_proof,
                caller_notification_id: sub_id,
                caller_application_id: app(0x10),
                caller_task_id: task(0x20),
                action_code: 6,
                redeemed_at_ms: 4_000,
            }),
        Err(NotifyError::ActionBindingMismatch(_))
    ));

    let mut bad_proof = token.secret_proof;
    bad_proof[0] ^= 0x01;
    assert!(matches!(
        harness
            .notify
            .redeem_action_token(RedeemNotificationActionRequest {
                token_id: token.token_id,
                presented_proof: bad_proof,
                caller_notification_id: sub_id,
                caller_application_id: app(0x10),
                caller_task_id: task(0x20),
                action_code: 5,
                redeemed_at_ms: 4_000,
            }),
        Err(NotifyError::ActionTokenProofInvalid(id)) if id == token.token_id
    ));
}

/// Scenario 7 (`[DUI-NOTIFY-001]`): strict one-shot enforcement — second
/// redemption fails closed with `ActionTokenAlreadyRedeemed`, and `SQLite`
/// triggers block raw SQL status rollback, bound-column mutation, and delete.
#[test]
fn one_shot_token_blocks_second_redeem_and_sqlite_triggers_block_rollback() {
    let harness = Harness::new("one-shot-triggers");
    let (_channel_id, topic_id) = setup_topic(&harness, b"desktop/oneshot", 11, 12);
    let sub_id = subscribe_face(&harness.notify, topic_id, 25);

    let token = harness
        .notify
        .issue_action_token(IssueNotificationActionTokenRequest {
            notification_id: sub_id,
            application_id: app(0x12),
            task_id: task(0x22),
            delivery_sequence: 1,
            action_code: 1,
            secret_nonce: [0x51; 32],
            issued_at_ms: 2_000,
            expires_at_ms: 5_000,
            idempotency_key: key(85),
        })
        .expect("issue")
        .token();

    let first_receipt = harness
        .notify
        .redeem_action_token(RedeemNotificationActionRequest {
            token_id: token.token_id,
            presented_proof: token.secret_proof,
            caller_notification_id: sub_id,
            caller_application_id: app(0x12),
            caller_task_id: task(0x22),
            action_code: 1,
            redeemed_at_ms: 3_000,
        })
        .expect("first redeem");

    for retry_at in [3_000u64, 3_500, 9_999] {
        let second = harness
            .notify
            .redeem_action_token(RedeemNotificationActionRequest {
                token_id: token.token_id,
                presented_proof: token.secret_proof,
                caller_notification_id: sub_id,
                caller_application_id: app(0x12),
                caller_task_id: task(0x22),
                action_code: 1,
                redeemed_at_ms: retry_at,
            });
        assert!(matches!(
            second,
            Err(NotifyError::ActionTokenAlreadyRedeemed {
                first_redeemed_at_ms: 3_000,
                receipt_id,
            }) if receipt_id == first_receipt.receipt_id
        ));
    }

    let raw_db = Connection::open(harness.root_path().join("notify-service.db")).expect("raw db");
    assert!(
        raw_db
            .execute(
                "UPDATE notify_action_tokens
                 SET status='ISSUED', redeemed_at_ms=NULL, redeem_receipt_id=NULL
                 WHERE token_id=?1",
                params![token.token_id.as_bytes().as_slice()],
            )
            .is_err()
    );
    assert!(
        raw_db
            .execute(
                "UPDATE notify_action_tokens SET application_id=?1 WHERE token_id=?2",
                params![
                    app(0x99).as_bytes().as_slice(),
                    token.token_id.as_bytes().as_slice()
                ],
            )
            .is_err()
    );
    assert!(
        raw_db
            .execute(
                "DELETE FROM notify_action_tokens WHERE token_id=?1",
                params![token.token_id.as_bytes().as_slice()],
            )
            .is_err()
    );
}

/// Scenario 8 (`[DUI-NOTIFY-001]`): administrative revocation transitions
/// `ISSUED -> REVOKED`, blocks redemption with `ActionTokenRevoked`, and
/// `SQLite` triggers forbid resurrecting a revoked token.
#[test]
fn revoked_action_token_cannot_be_redeemed_or_resurrected() {
    let harness = Harness::new("revoke-token");
    let (_channel_id, topic_id) = setup_topic(&harness, b"desktop/revoke", 13, 14);
    let sub_id = subscribe_face(&harness.notify, topic_id, 26);

    let token = harness
        .notify
        .issue_action_token(IssueNotificationActionTokenRequest {
            notification_id: sub_id,
            application_id: app(0x14),
            task_id: task(0x24),
            delivery_sequence: 1,
            action_code: 2,
            secret_nonce: [0x77; 32],
            issued_at_ms: 2_000,
            expires_at_ms: 8_000,
            idempotency_key: key(88),
        })
        .expect("issue")
        .token();

    harness
        .notify
        .revoke_action_token(token.token_id, 2_500)
        .expect("revoke");
    harness
        .notify
        .revoke_action_token(token.token_id, 2_600)
        .expect("revoke replay");

    assert!(matches!(
        harness
            .notify
            .redeem_action_token(RedeemNotificationActionRequest {
                token_id: token.token_id,
                presented_proof: token.secret_proof,
                caller_notification_id: sub_id,
                caller_application_id: app(0x14),
                caller_task_id: task(0x24),
                action_code: 2,
                redeemed_at_ms: 3_000,
            }),
        Err(NotifyError::ActionTokenRevoked {
            revoked_at_ms: 2_500
        })
    ));

    let raw_db = Connection::open(harness.root_path().join("notify-service.db")).expect("raw db");
    assert!(
        raw_db
            .execute(
                "UPDATE notify_action_tokens
                 SET status='ISSUED', revoked_at_ms=NULL
                 WHERE token_id=?1",
                params![token.token_id.as_bytes().as_slice()],
            )
            .is_err()
    );
}

/// Scenario 9: expired action tokens (`redeemed_at_ms > expires_at_ms`) fail
/// closed with `ActionTokenExpired`, while exact-deadline redemption (`==`)
/// succeeds.
#[test]
fn expired_action_token_is_rejected_and_exact_deadline_succeeds() {
    let harness = Harness::new("expiry-token");
    let (_channel_id, topic_id) = setup_topic(&harness, b"desktop/expiry", 15, 16);
    let sub_id = subscribe_face(&harness.notify, topic_id, 27);

    let expired_token = harness
        .notify
        .issue_action_token(IssueNotificationActionTokenRequest {
            notification_id: sub_id,
            application_id: app(0x15),
            task_id: task(0x25),
            delivery_sequence: 1,
            action_code: 1,
            secret_nonce: [0x33; 32],
            issued_at_ms: 2_000,
            expires_at_ms: 5_000,
            idempotency_key: key(90),
        })
        .expect("issue")
        .token();

    assert!(matches!(
        harness
            .notify
            .redeem_action_token(RedeemNotificationActionRequest {
                token_id: expired_token.token_id,
                presented_proof: expired_token.secret_proof,
                caller_notification_id: sub_id,
                caller_application_id: app(0x15),
                caller_task_id: task(0x25),
                action_code: 1,
                redeemed_at_ms: 5_001,
            }),
        Err(NotifyError::ActionTokenExpired {
            expires_at_ms: 5_000,
            redeemed_at_ms: 5_001,
        })
    ));

    harness
        .notify
        .redeem_action_token(RedeemNotificationActionRequest {
            token_id: expired_token.token_id,
            presented_proof: expired_token.secret_proof,
            caller_notification_id: sub_id,
            caller_application_id: app(0x15),
            caller_task_id: task(0x25),
            action_code: 1,
            redeemed_at_ms: 5_000,
        })
        .expect("exact deadline redeem");
}

/// Scenario 10: schema `v1 -> v2` in-place migration preserves existing `v1`
/// subscriptions seamlessly and upgrades `user_version` to `2`, while
/// unsupported future versions fail closed.
#[test]
fn schema_v1_migrates_in_place_to_v2_preserving_existing_subscriptions() {
    let root = TempRoot::new("v1-to-v2-migration");
    let sub_id = {
        let (channel, topics, _waits, notify) = open_stack(root.path());
        let ch = match channel
            .create_channel(CreateChannelRequest {
                capacity_bytes: 8_192,
                policy_digest: [0x22; 32],
                idempotency_key: key(1),
                created_at_ms: 1_000,
            })
            .expect("channel")
        {
            ChannelDecision::Created(r) => r,
            ChannelDecision::Replayed(_) => panic!("fresh"),
        };
        let tp = match topics
            .create_topic(CreateTopicRequest {
                channel_id: ch.channel_id,
                name: b"legacy/v1".to_vec(),
                policy: TopicPolicy {
                    max_recipients: 4,
                    delivery_attempts: 3,
                    cascade_depth: 1,
                    retained_bytes: 8_192,
                    retention_ms: 60_000,
                    payer: ResourceAccountId::from_bytes([2; 16]),
                },
                idempotency_key: key(2),
                created_at_ms: 1_100,
            })
            .expect("topic")
        {
            TopicDecision::Created(r) => r,
            TopicDecision::Replayed(_) => panic!("fresh"),
        };
        let sub = subscribe_face(&notify, tp.topic_id, 30);
        notify
            .publish(PublishNotificationRequest {
                topic_id: tp.topic_id,
                payload: b"v1-entry".to_vec(),
                idempotency_key: key(3),
                published_at_ms: 1_200,
            })
            .expect("v1 publish");
        sub
    };

    {
        let db = Connection::open(root.path().join("notify-service.db")).expect("open v1 db");
        let version: i64 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version");
        assert_eq!(version, 1);
    }

    {
        let (_channel, _topics, _waits, notify) = open_stack(root.path());
        let subs = notify.list_subscriptions().expect("list after v2 upgrade");
        assert_eq!(subs.len(), 1);
        let polled = notify
            .poll(sub_id, 4)
            .expect("poll v1 entry after v2 upgrade");
        assert_eq!(polled.len(), 1);
        assert_eq!(polled[0].payload, b"v1-entry");
        let _ = notify
            .issue_action_token(IssueNotificationActionTokenRequest {
                notification_id: sub_id,
                application_id: app(0x50),
                task_id: task(0x60),
                delivery_sequence: 1,
                action_code: 1,
                secret_nonce: [0xAB; 32],
                issued_at_ms: 2_000,
                expires_at_ms: 9_000,
                idempotency_key: key(95),
            })
            .expect("v2 token issuance on migrated db");
    }

    let db = Connection::open(root.path().join("notify-service.db")).expect("open v2 db");
    let version: i64 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("user_version");
    assert_eq!(version, 2);

    db.pragma_update(None, "user_version", 99)
        .expect("set unsupported version");
    drop(db);
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("channel"));
    let topics = Arc::new(TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("topics"));
    let waits = Arc::new(WaitAuthority::open(root.path(), Arc::clone(&channel)).expect("waits"));
    assert!(matches!(
        NotificationService::open(root.path(), topics, waits),
        Err(NotifyError::SchemaVersionUnsupported(99))
    ));
}

/// Scenario 11 (`[DUI-OFFLINE-001]`): cursor-paginated offline notification
/// history, bounds validation (`limit == 0` and `> MAX_HISTORY_PAGE_LIMIT`
/// fail closed), idempotent replay, and duplicate-sequence conflict rejection.
#[test]
fn offline_history_cursor_pagination_and_conflict_bounds() {
    let harness = Harness::new("offline-history-pages");
    let (channel_id, topic_id) = setup_topic(&harness, b"desktop/history", 17, 18);

    assert!(matches!(
        harness
            .notify
            .query_notification_history_page(topic_id, 0, 0),
        Err(NotifyError::InvalidHistoryPagination(_))
    ));
    assert!(matches!(
        harness
            .notify
            .query_notification_history_page(topic_id, 0, MAX_HISTORY_PAGE_LIMIT + 1),
        Err(NotifyError::InvalidHistoryPagination(_))
    ));

    let kinds = [
        TaskTransitionKind::Proposed,
        TaskTransitionKind::Authorized,
        TaskTransitionKind::Running,
        TaskTransitionKind::Completing,
        TaskTransitionKind::Completed,
    ];
    for (idx, kind) in kinds.into_iter().enumerate() {
        let seed = u8::try_from(100 + idx).expect("seed");
        harness
            .notify
            .publish_task_transition(
                topic_id,
                &sample_envelope(kind, 4_000 + (idx as u64) * 100, false),
                key(seed),
            )
            .expect("publish task transition");
    }
    for seq in [6u64, 7u64] {
        let req = RecordDeliveryHistoryRequest {
            topic_id,
            channel_id,
            channel_sequence: seq,
            payload_digest: [u8::try_from(seq).expect("u8"); 32],
            recorded_at_ms: 5_000 + seq,
            task_metadata: None,
        };
        assert!(matches!(
            harness.notify.record_delivery_history(req).expect("record"),
            RecordHistoryDecision::Recorded(_)
        ));
        assert!(matches!(
            harness.notify.record_delivery_history(req).expect("replay"),
            RecordHistoryDecision::Replayed(_)
        ));
    }

    assert!(matches!(
        harness
            .notify
            .record_delivery_history(RecordDeliveryHistoryRequest {
                topic_id,
                channel_id,
                channel_sequence: 6,
                payload_digest: [0xEE; 32],
                recorded_at_ms: 5_006,
                task_metadata: None,
            }),
        Err(NotifyError::HistoryConflict {
            channel_sequence: 6,
            ..
        })
    ));

    let p1 = harness
        .notify
        .query_notification_history_page(topic_id, 0, 3)
        .expect("page 1");
    assert_eq!(page_sequences(&p1), vec![1, 2, 3]);
    assert!(p1.has_more);
    assert_eq!(p1.next_after_sequence, Some(3));

    let p2 = harness
        .notify
        .query_notification_history_page(topic_id, p1.next_after_sequence.expect("cursor"), 3)
        .expect("page 2");
    assert_eq!(page_sequences(&p2), vec![4, 5, 6]);
    assert!(p2.has_more);

    let p3 = harness
        .notify
        .query_notification_history_page(topic_id, p2.next_after_sequence.expect("cursor"), 3)
        .expect("page 3");
    assert_eq!(page_sequences(&p3), vec![7]);
    assert!(!p3.has_more);
}

/// Scenario 12 (`[DUI-OFFLINE-001]`): monotonic `prune_history_before`
/// advances the topic retention watermark, rejects regressing watermarks and
/// below-watermark inserts, and enforces `SQLite` immutability triggers.
#[test]
fn offline_history_monotonic_retention_pruning_and_sqlite_triggers() {
    let harness = Harness::new("offline-history-prune");
    let (channel_id, topic_id) = setup_topic(&harness, b"desktop/history-prune", 19, 20);

    for seq in 1u64..=5u64 {
        harness
            .notify
            .record_delivery_history(RecordDeliveryHistoryRequest {
                topic_id,
                channel_id,
                channel_sequence: seq,
                payload_digest: [u8::try_from(seq).expect("u8"); 32],
                recorded_at_ms: 4_000 + seq,
                task_metadata: Some(TaskHistoryMetadata::from(&sample_envelope(
                    TaskTransitionKind::Running,
                    4_000 + seq,
                    false,
                ))),
            })
            .expect("record history");
    }

    let prune = harness
        .notify
        .prune_history_before(topic_id, 4)
        .expect("prune < 4");
    assert_eq!(prune.pruned_entries, 3);
    assert_eq!(prune.retained_from_sequence, 4);

    let remaining = harness
        .notify
        .query_notification_history_page(topic_id, 0, 10)
        .expect("remaining");
    assert_eq!(
        remaining
            .entries
            .iter()
            .map(|e| e.channel_sequence)
            .collect::<Vec<_>>(),
        vec![4, 5]
    );

    assert!(matches!(
        harness.notify.prune_history_before(topic_id, 2),
        Err(NotifyError::InvalidHistoryPagination(_))
    ));
    assert!(matches!(
        harness
            .notify
            .record_delivery_history(RecordDeliveryHistoryRequest {
                topic_id,
                channel_id,
                channel_sequence: 2,
                payload_digest: [0x22; 32],
                recorded_at_ms: 6_000,
                task_metadata: None,
            }),
        Err(NotifyError::HistorySequencePruned {
            channel_sequence: 2,
            retained_from_sequence: 4,
            ..
        })
    ));

    let raw_db = Connection::open(harness.root_path().join("notify-service.db")).expect("raw db");
    assert!(
        raw_db
            .execute(
                "UPDATE notify_history_refs SET recorded_at_ms = 99999 WHERE channel_sequence = 4",
                [],
            )
            .is_err()
    );
    assert!(
        raw_db
            .execute(
                "UPDATE notify_history_prune_watermarks SET retained_from_sequence = 1",
                [],
            )
            .is_err()
    );
    assert!(
        raw_db
            .execute("DELETE FROM notify_history_prune_watermarks", [])
            .is_err()
    );
}

/// Scenario 13 (`[DUI-OFFLINE-001]`): pruned notification history watermarks
/// survive restart and replaying a pruned `publish_task_transition` converges
/// cleanly.
#[test]
fn pruned_offline_history_survives_restart_and_replayed_publish_converges() {
    let root = TempRoot::new("offline-history-restart");
    let topic_id = {
        let (channel, topics, _waits, notify) = open_stack(root.path());
        let ch = match channel
            .create_channel(CreateChannelRequest {
                capacity_bytes: 8_192,
                policy_digest: [0x33; 32],
                idempotency_key: key(1),
                created_at_ms: 1_000,
            })
            .expect("channel")
        {
            ChannelDecision::Created(r) => r,
            ChannelDecision::Replayed(_) => panic!("fresh"),
        };
        let tp = match topics
            .create_topic(CreateTopicRequest {
                channel_id: ch.channel_id,
                name: b"desktop/history-restart".to_vec(),
                policy: TopicPolicy {
                    max_recipients: 4,
                    delivery_attempts: 3,
                    cascade_depth: 1,
                    retained_bytes: 8_192,
                    retention_ms: 60_000,
                    payer: ResourceAccountId::from_bytes([3; 16]),
                },
                idempotency_key: key(2),
                created_at_ms: 1_100,
            })
            .expect("topic")
        {
            TopicDecision::Created(r) => r,
            TopicDecision::Replayed(_) => panic!("fresh"),
        };
        notify
            .publish_task_transition(
                tp.topic_id,
                &sample_envelope(TaskTransitionKind::Running, 2_000, false),
                key(110),
            )
            .expect("seq 1");
        notify
            .publish_task_transition(
                tp.topic_id,
                &sample_envelope(TaskTransitionKind::Completed, 2_100, false),
                key(111),
            )
            .expect("seq 2");
        notify
            .prune_history_before(tp.topic_id, 2)
            .expect("prune seq 1");
        tp.topic_id
    };

    let (_channel, _topics, _waits, notify) = open_stack(root.path());
    let page = notify
        .query_notification_history_page(topic_id, 0, 10)
        .expect("query after restart");
    assert_eq!(page.retained_from_sequence, 2);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].channel_sequence, 2);

    let replayed = notify
        .publish_task_transition(
            topic_id,
            &sample_envelope(TaskTransitionKind::Running, 2_000, false),
            key(110),
        )
        .expect("replay pruned sequence 1");
    assert!(matches!(replayed, NotificationPublishDecision::Replayed(_)));
}
