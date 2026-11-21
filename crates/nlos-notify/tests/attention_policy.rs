//! Integration and adversarial tests for [`AttentionPolicyController`]
//! (`[RSM-HUMAN-001]`, `[RSM-ATTN-002]`, `[TUI-CONSENT-001]`, `[DUI-NOTIFY-001]`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest};
use nlos_notify::{
    ATTENTION_POLICY_DB_FILENAME, ATTENTION_POLICY_SCHEMA_VERSION, AttentionDecision,
    AttentionDeliveryRequest, AttentionLaneClass, AttentionOrigin, AttentionOutcomeKind,
    AttentionPolicyController, AttentionPolicyError, AttentionQueueTarget, AttentionRatePolicy,
    IssueNotificationActionTokenRequest, MAX_COALESCE_KEY_LEN, MAX_RECEIPT_LIST_LIMIT,
    NotificationService, NotificationSubscribeDecision, QuietHoursPolicy,
    RedeemNotificationActionRequest, SubscribeNotificationRequest, TaskNotificationEnvelope,
    TaskTransitionKind,
};
use nlos_topic::{CreateTopicRequest, SubscriberKey, TopicAuthority, TopicDecision, TopicPolicy};
use nlos_types::{ApplicationId, Generation, IdempotencyKey, ResourceAccountId, TaskId};
use nlos_wait::WaitAuthority;
use rusqlite::{Connection, params};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let seq = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-attn-policy-{label}-{}-{nonce}-{seq}",
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

fn key(seed: u128) -> IdempotencyKey {
    IdempotencyKey::from_bytes(seed.to_be_bytes())
}

fn app(seed: u8) -> ApplicationId {
    ApplicationId::from_bytes([seed; 16])
}

fn default_rate_policy() -> AttentionRatePolicy {
    AttentionRatePolicy {
        max_per_app_per_window: 3,
        max_global_ordinary_per_window: 5,
        window_ms: 60_000,
        allow_coalescing: true,
    }
}

fn disabled_quiet_hours() -> QuietHoursPolicy {
    QuietHoursPolicy {
        enabled: false,
        start_minute_of_day: 22 * 60,
        end_minute_of_day: 7 * 60,
        utc_offset_minutes: 0,
    }
}

fn req(
    idempotency_seed: u128,
    origin: AttentionOrigin,
    lane: AttentionLaneClass,
    coalesce_key: Option<&str>,
    requested_at_ms: u64,
) -> AttentionDeliveryRequest {
    AttentionDeliveryRequest {
        idempotency_key: key(idempotency_seed),
        origin,
        lane,
        coalesce_key: coalesce_key.map(str::to_owned),
        requested_at_ms,
        payment_priority_override_attempted: false,
    }
}

fn exercise_notify_service_v2_stack(root: &Path) {
    let channel = Arc::new(ChannelAuthority::open(root).expect("open channel"));
    let topics = Arc::new(TopicAuthority::open(root, Arc::clone(&channel)).expect("open topics"));
    let waits = Arc::new(WaitAuthority::open(root, Arc::clone(&channel)).expect("open waits"));
    let notify =
        NotificationService::open(root, Arc::clone(&topics), Arc::clone(&waits)).expect("notify");

    let ch = match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 16_384,
            policy_digest: [0x11; 32],
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
            name: b"attn/coexist".to_vec(),
            policy: TopicPolicy {
                max_recipients: 4,
                delivery_attempts: 3,
                cascade_depth: 1,
                retained_bytes: 16_384,
                retention_ms: 60_000,
                payer: ResourceAccountId::from_bytes([0x05; 16]),
            },
            idempotency_key: key(2),
            created_at_ms: 1_100,
        })
        .expect("topic")
    {
        TopicDecision::Created(r) => r,
        TopicDecision::Replayed(_) => panic!("fresh"),
    };
    let sub_id = match notify
        .subscribe(SubscribeNotificationRequest {
            topic_id: tp.topic_id,
            subscriber_key: SubscriberKey::from_bytes([0x07; 16]),
            subscribed_at_ms: 1_200,
        })
        .expect("subscribe")
    {
        NotificationSubscribeDecision::Subscribed(s) => s.notification_id,
        NotificationSubscribeDecision::Replayed(_) => panic!("fresh"),
    };
    let env = TaskNotificationEnvelope {
        task_id: TaskId::from_bytes([0x21; 16]),
        application_id: app(0x31),
        task_generation: Generation::INITIAL,
        transition: TaskTransitionKind::WaitingConsent,
        occurred_at_ms: 1_300,
        summary_digest: [0xAA; 32],
        requires_user_action: true,
    };
    let _ = notify
        .publish_task_transition(tp.topic_id, &env, key(3))
        .expect("publish task transition");
    let token = notify
        .issue_action_token(IssueNotificationActionTokenRequest {
            notification_id: sub_id,
            application_id: app(0x31),
            task_id: TaskId::from_bytes([0x21; 16]),
            delivery_sequence: 1,
            action_code: 1,
            secret_nonce: [0x44; 32],
            issued_at_ms: 1_400,
            expires_at_ms: 9_000,
            idempotency_key: key(4),
        })
        .expect("issue token")
        .token();
    let _ = notify
        .redeem_action_token(RedeemNotificationActionRequest {
            token_id: token.token_id,
            presented_proof: token.secret_proof,
            caller_notification_id: sub_id,
            caller_application_id: app(0x31),
            caller_task_id: TaskId::from_bytes([0x21; 16]),
            action_code: 1,
            redeemed_at_ms: 1_600,
        })
        .expect("redeem token");
    let page = notify
        .query_notification_history_page(tp.topic_id, 0, 10)
        .expect("history page");
    assert_eq!(page.entries.len(), 1);
}

#[test]
fn schema_durability_and_coexistence_with_notify_service_v2() {
    let root = TempRoot::new("coexist-v2");
    exercise_notify_service_v2_stack(root.path());

    let controller =
        AttentionPolicyController::open(root.path(), default_rate_policy(), disabled_quiet_hours())
            .expect("open attention policy controller");
    assert_eq!(controller.policy_generation().expect("gen"), 1);

    let updated_rate = AttentionRatePolicy {
        max_per_app_per_window: 4,
        ..default_rate_policy()
    };
    let updated_quiet = QuietHoursPolicy {
        enabled: true,
        start_minute_of_day: 23 * 60,
        end_minute_of_day: 6 * 60,
        utc_offset_minutes: 480,
    };
    assert_eq!(
        controller
            .update_policy(updated_rate, updated_quiet)
            .expect("update policy"),
        2
    );
    assert_eq!(
        controller
            .update_policy(updated_rate, updated_quiet)
            .expect("noop update"),
        2
    );
    assert!(matches!(
        controller
            .evaluate(&req(
                10,
                AttentionOrigin::TrustedSystemUi,
                AttentionLaneClass::ConsentPrompt,
                None,
                1_500,
            ))
            .expect("priority consent prompt"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::PriorityConsentLane,
            ..
        }
    ));
    drop(controller);

    let notify_db =
        Connection::open(root.path().join("notify-service.db")).expect("open notify db");
    let notify_ver: i64 = notify_db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("notify user_version");
    assert_eq!(notify_ver, 2);

    let attn_db =
        Connection::open(root.path().join(ATTENTION_POLICY_DB_FILENAME)).expect("open attn db");
    let attn_ver: i64 = attn_db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .expect("attn user_version");
    assert_eq!(attn_ver, ATTENTION_POLICY_SCHEMA_VERSION);
    let journal: String = attn_db
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .expect("journal");
    assert!(journal.eq_ignore_ascii_case("wal"));

    let reopened =
        AttentionPolicyController::open_existing(root.path()).expect("reopen existing controller");
    assert_eq!(reopened.policy_generation().expect("gen"), 2);
    assert_eq!(
        reopened.current_policy().expect("policy"),
        (updated_rate, updated_quiet)
    );
}

#[test]
fn invalid_policies_fail_closed() {
    let root = TempRoot::new("invalid-policies");
    let valid_rate = default_rate_policy();
    let valid_quiet = disabled_quiet_hours();

    for bad_rate in [
        AttentionRatePolicy {
            max_per_app_per_window: 0,
            ..valid_rate
        },
        AttentionRatePolicy {
            max_global_ordinary_per_window: 0,
            ..valid_rate
        },
        AttentionRatePolicy {
            window_ms: 0,
            ..valid_rate
        },
        AttentionRatePolicy {
            window_ms: (i64::MAX as u64) + 1,
            ..valid_rate
        },
    ] {
        assert!(matches!(
            AttentionPolicyController::open(root.path(), bad_rate, valid_quiet),
            Err(AttentionPolicyError::InvalidPolicy(_))
        ));
    }

    for bad_quiet in [
        QuietHoursPolicy {
            enabled: true,
            start_minute_of_day: 1_440,
            end_minute_of_day: 420,
            utc_offset_minutes: 0,
        },
        QuietHoursPolicy {
            enabled: true,
            start_minute_of_day: 1_320,
            end_minute_of_day: 1_440,
            utc_offset_minutes: 0,
        },
        QuietHoursPolicy {
            enabled: true,
            start_minute_of_day: 1_320,
            end_minute_of_day: 420,
            utc_offset_minutes: -721,
        },
        QuietHoursPolicy {
            enabled: true,
            start_minute_of_day: 1_320,
            end_minute_of_day: 420,
            utc_offset_minutes: 841,
        },
        QuietHoursPolicy {
            enabled: true,
            start_minute_of_day: 600,
            end_minute_of_day: 600,
            utc_offset_minutes: 0,
        },
    ] {
        assert!(matches!(
            AttentionPolicyController::open(root.path(), valid_rate, bad_quiet),
            Err(AttentionPolicyError::InvalidPolicy(_))
        ));
    }
}

#[test]
fn malformed_requests_and_queries_fail_closed() {
    let root = TempRoot::new("malformed-requests");
    let controller =
        AttentionPolicyController::open(root.path(), default_rate_policy(), disabled_quiet_hours())
            .expect("open");

    let oversized_key = "x".repeat(MAX_COALESCE_KEY_LEN + 1);
    for bad_req in [
        req(
            1,
            AttentionOrigin::Application(app(1)),
            AttentionLaneClass::Ordinary,
            None,
            (i64::MAX as u64) + 1,
        ),
        req(
            2,
            AttentionOrigin::Application(app(1)),
            AttentionLaneClass::Ordinary,
            Some(""),
            1_000,
        ),
        req(
            3,
            AttentionOrigin::Application(app(1)),
            AttentionLaneClass::Ordinary,
            Some(&oversized_key),
            1_000,
        ),
        req(
            4,
            AttentionOrigin::Application(app(1)),
            AttentionLaneClass::Ordinary,
            Some("bad\0key"),
            1_000,
        ),
    ] {
        assert!(matches!(
            controller.evaluate(&bad_req),
            Err(AttentionPolicyError::InvalidRequest(_))
        ));
    }

    assert!(matches!(
        controller.list_receipts(0, 0),
        Err(AttentionPolicyError::InvalidRequest(_))
    ));
    assert!(matches!(
        controller.list_receipts(0, MAX_RECEIPT_LIST_LIMIT + 1),
        Err(AttentionPolicyError::InvalidRequest(_))
    ));
}

#[test]
fn per_app_rate_limit_global_cap_and_window_rollover_recovery() {
    let root = TempRoot::new("rate-and-rollover");
    let rate = AttentionRatePolicy {
        max_per_app_per_window: 2,
        max_global_ordinary_per_window: 3,
        window_ms: 10_000,
        allow_coalescing: false,
    };
    let controller =
        AttentionPolicyController::open(root.path(), rate, disabled_quiet_hours()).expect("open");

    let app_a = AttentionOrigin::Application(app(0xA1));
    let app_b = AttentionOrigin::Application(app(0xB1));
    let app_c = AttentionOrigin::Application(app(0xC1));

    // Window [10_000, 20_000): App A consumes its 2-per-app quota.
    for (seed, at) in [(1u128, 10_000u64), (2, 12_000)] {
        assert!(matches!(
            controller
                .evaluate(&req(seed, app_a, AttentionLaneClass::Ordinary, None, at))
                .expect("app A deliver"),
            AttentionDecision::Delivered {
                queue: AttentionQueueTarget::StandardNotificationLane,
                ..
            }
        ));
    }

    // 3rd from App A at t=19_999 fails per-app cap with exact retry_after_ms == 1.
    assert!(matches!(
        controller
            .evaluate(&req(3, app_a, AttentionLaneClass::Ordinary, None, 19_999))
            .expect("app A limited"),
        AttentionDecision::RateLimited {
            retry_after_ms: 1,
            ..
        }
    ));

    // App B still has quota (global is 2/3), so 1st succeeds.
    assert!(matches!(
        controller
            .evaluate(&req(
                4,
                app_b,
                AttentionLaneClass::BackgroundDigest,
                None,
                15_000
            ))
            .expect("app B deliver"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::StandardNotificationLane,
            ..
        }
    ));

    // Now global ordinary is 3/3: both App B (1 < 2 per app) and App C (0 < 2 per app) are global-limited.
    assert!(matches!(
        controller
            .evaluate(&req(5, app_b, AttentionLaneClass::Ordinary, None, 16_000))
            .expect("app B global limited"),
        AttentionDecision::RateLimited {
            retry_after_ms: 4_000,
            ..
        }
    ));
    assert!(matches!(
        controller
            .evaluate(&req(6, app_c, AttentionLaneClass::Ordinary, None, 17_000))
            .expect("app C global limited"),
        AttentionDecision::RateLimited {
            retry_after_ms: 3_000,
            ..
        }
    ));

    let usage_a = controller.window_usage(app_a, 17_000).expect("usage a");
    assert_eq!(usage_a.window_start_ms, 10_000);
    assert_eq!(usage_a.window_end_ms, 20_000);
    assert_eq!(usage_a.per_origin_delivered, 2);
    assert_eq!(usage_a.global_standard_delivered, 3);

    // At t = 20_000 (window rolls over), counts reset automatically and deliveries succeed.
    for (seed, origin, at) in [
        (7u128, app_a, 20_000u64),
        (8, app_b, 20_100),
        (9, app_c, 20_200),
    ] {
        assert!(matches!(
            controller
                .evaluate(&req(seed, origin, AttentionLaneClass::Ordinary, None, at))
                .expect("rollover deliver"),
            AttentionDecision::Delivered {
                queue: AttentionQueueTarget::StandardNotificationLane,
                ..
            }
        ));
    }
    let usage_a_next = controller
        .window_usage(app_a, 20_500)
        .expect("usage a next");
    assert_eq!(usage_a_next.window_start_ms, 20_000);
    assert_eq!(usage_a_next.per_origin_delivered, 1);
    assert_eq!(usage_a_next.global_standard_delivered, 3);
}

#[test]
fn same_app_coalescing_increments_and_isolates_across_apps() {
    let root = TempRoot::new("coalesce-apps");
    let rate = AttentionRatePolicy {
        max_per_app_per_window: 2,
        max_global_ordinary_per_window: 5,
        window_ms: 60_000,
        allow_coalescing: true,
    };
    let controller =
        AttentionPolicyController::open(root.path(), rate, disabled_quiet_hours()).expect("open");

    let app_a = AttentionOrigin::Application(app(0xA2));
    let app_b = AttentionOrigin::Application(app(0xB2));

    let first = controller
        .evaluate(&req(
            1,
            app_a,
            AttentionLaneClass::Ordinary,
            Some("build-progress"),
            60_000,
        ))
        .expect("first deliver");
    assert!(matches!(
        first,
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::StandardNotificationLane,
            ..
        }
    ));

    for (idx, (lane, at)) in [
        (AttentionLaneClass::Ordinary, 61_000u64),
        (AttentionLaneClass::BackgroundDigest, 62_000),
        (AttentionLaneClass::Ordinary, 63_000),
    ]
    .into_iter()
    .enumerate()
    {
        let expected_count = u64::try_from(idx + 1).expect("u64");
        let decision = controller
            .evaluate(&req(
                u128::from(idx as u64 + 2),
                app_a,
                lane,
                Some("build-progress"),
                at,
            ))
            .expect("coalesce");
        assert!(matches!(
            decision,
            AttentionDecision::Coalesced {
                ref coalesce_key,
                coalesced_count,
                window_start_ms: 60_000,
                ..
            } if coalesce_key == "build-progress" && coalesced_count == expected_count
        ));
    }

    let counter = controller
        .inspect_coalesce_counter(app_a, "build-progress", 60_000)
        .expect("inspect counter")
        .expect("counter exists");
    assert_eq!(counter.delivered_count, 1);
    assert_eq!(counter.coalesced_count, 3);
    assert_eq!(counter.total_events(), 4);
    assert_eq!(counter.initial_receipt_id, first.receipt_id());

    // Different application using the SAME coalesce_key gets its own delivery.
    assert!(matches!(
        controller
            .evaluate(&req(
                10,
                app_b,
                AttentionLaneClass::Ordinary,
                Some("build-progress"),
                64_000,
            ))
            .expect("app B independent"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::StandardNotificationLane,
            ..
        }
    ));
}

#[test]
fn coalescing_coexists_with_quota_and_resets_across_windows() {
    let root = TempRoot::new("coalesce-windows");
    let rate = AttentionRatePolicy {
        max_per_app_per_window: 2,
        max_global_ordinary_per_window: 5,
        window_ms: 60_000,
        allow_coalescing: true,
    };
    let controller =
        AttentionPolicyController::open(root.path(), rate, disabled_quiet_hours()).expect("open");
    let app_a = AttentionOrigin::Application(app(0xA3));

    for (seed, key_name, at) in [(1u128, "build", 60_000u64), (2, "lint", 61_000)] {
        assert!(matches!(
            controller
                .evaluate(&req(
                    seed,
                    app_a,
                    AttentionLaneClass::Ordinary,
                    Some(key_name),
                    at
                ))
                .expect("deliver"),
            AttentionDecision::Delivered {
                queue: AttentionQueueTarget::StandardNotificationLane,
                ..
            }
        ));
    }
    assert!(matches!(
        controller
            .evaluate(&req(
                3,
                app_a,
                AttentionLaneClass::Ordinary,
                Some("third"),
                62_000
            ))
            .expect("rate limited"),
        AttentionDecision::RateLimited { .. }
    ));
    assert!(matches!(
        controller
            .evaluate(&req(
                4,
                app_a,
                AttentionLaneClass::Ordinary,
                Some("build"),
                63_000
            ))
            .expect("coalesce when full"),
        AttentionDecision::Coalesced {
            coalesced_count: 1,
            window_start_ms: 60_000,
            ..
        }
    ));

    assert!(matches!(
        controller
            .evaluate(&req(
                5,
                app_a,
                AttentionLaneClass::Ordinary,
                Some("build"),
                120_000
            ))
            .expect("new window deliver"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::StandardNotificationLane,
            ..
        }
    ));
    let next_counter = controller
        .inspect_coalesce_counter(app_a, "build", 120_000)
        .expect("next counter")
        .expect("exists");
    assert_eq!(next_counter.coalesced_count, 0);
}

#[test]
fn disabling_allow_coalescing_enforces_quota_without_collapsing() {
    let root = TempRoot::new("coalesce-disabled");
    let rate = AttentionRatePolicy {
        max_per_app_per_window: 2,
        max_global_ordinary_per_window: 5,
        window_ms: 60_000,
        allow_coalescing: true,
    };
    let controller =
        AttentionPolicyController::open(root.path(), rate, disabled_quiet_hours()).expect("open");
    let app_a = AttentionOrigin::Application(app(0xA4));

    assert!(matches!(
        controller
            .evaluate(&req(
                1,
                app_a,
                AttentionLaneClass::Ordinary,
                Some("build"),
                120_000
            ))
            .expect("first deliver"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::StandardNotificationLane,
            ..
        }
    ));

    controller
        .update_policy(
            AttentionRatePolicy {
                allow_coalescing: false,
                ..rate
            },
            disabled_quiet_hours(),
        )
        .expect("disable coalescing");

    assert!(matches!(
        controller
            .evaluate(&req(
                2,
                app_a,
                AttentionLaneClass::Ordinary,
                Some("build"),
                121_000
            ))
            .expect("second deliver without coalescing"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::StandardNotificationLane,
            ..
        }
    ));
    assert!(matches!(
        controller
            .evaluate(&req(
                3,
                app_a,
                AttentionLaneClass::Ordinary,
                Some("build"),
                122_000
            ))
            .expect("rate limited without coalescing"),
        AttentionDecision::RateLimited { .. }
    ));
}

#[test]
fn quiet_hours_cross_midnight_defers_ordinary_while_priority_bypasses() {
    let root = TempRoot::new("quiet-cross-midnight");
    let rate = AttentionRatePolicy {
        max_per_app_per_window: 2,
        max_global_ordinary_per_window: 4,
        window_ms: 60_000,
        allow_coalescing: false,
    };
    let night_quiet = QuietHoursPolicy {
        enabled: true,
        start_minute_of_day: 22 * 60,
        end_minute_of_day: 7 * 60,
        utc_offset_minutes: 480,
    };
    let controller = AttentionPolicyController::open(root.path(), rate, night_quiet).expect("open");
    let app_a = AttentionOrigin::Application(app(0xD1));

    let day0_utc_midnight: u64 = 20_779 * 86_400_000;
    let offset_ms: u64 = 480 * 60_000;
    let local_time_to_utc = |day_offset: u64, hour: u64, min: u64, sec: u64, ms: u64| -> u64 {
        day0_utc_midnight
            + day_offset * 86_400_000
            + hour * 3_600_000
            + min * 60_000
            + sec * 1_000
            + ms
            - offset_ms
    };

    let before_quiet = local_time_to_utc(1, 21, 59, 59, 999);
    let at_start = local_time_to_utc(1, 22, 0, 0, 0);
    let mid_night = local_time_to_utc(2, 3, 15, 0, 0);
    let just_before_end = local_time_to_utc(2, 6, 59, 59, 999);
    let exact_resume = local_time_to_utc(2, 7, 0, 0, 0);

    assert!(matches!(
        controller
            .evaluate(&req(
                1,
                app_a,
                AttentionLaneClass::Ordinary,
                None,
                before_quiet
            ))
            .expect("before quiet"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::StandardNotificationLane,
            ..
        }
    ));

    for (seed, lane, at) in [
        (2u128, AttentionLaneClass::Ordinary, at_start),
        (3, AttentionLaneClass::BackgroundDigest, mid_night),
        (4, AttentionLaneClass::Ordinary, just_before_end),
    ] {
        assert_eq!(
            match controller
                .evaluate(&req(seed, app_a, lane, None, at))
                .expect("deferred")
            {
                AttentionDecision::DeferredQuietHours { resume_at_ms, .. } => resume_at_ms,
                other => panic!("expected DeferredQuietHours, got {other:?}"),
            },
            exact_resume
        );
    }

    for (seed, lane, at) in [
        (5u128, AttentionLaneClass::ConsentPrompt, at_start),
        (6, AttentionLaneClass::SystemCritical, mid_night),
    ] {
        assert!(matches!(
            controller
                .evaluate(&req(seed, AttentionOrigin::TrustedSystemUi, lane, None, at))
                .expect("trusted bypasses quiet"),
            AttentionDecision::Delivered {
                queue: AttentionQueueTarget::PriorityConsentLane,
                ..
            }
        ));
    }

    for (seed, lane, at) in [
        (7u128, AttentionLaneClass::Ordinary, exact_resume),
        (
            8,
            AttentionLaneClass::BackgroundDigest,
            exact_resume + 1_000,
        ),
    ] {
        assert!(matches!(
            controller
                .evaluate(&req(seed, app_a, lane, None, at))
                .expect("deliver at resume"),
            AttentionDecision::Delivered {
                queue: AttentionQueueTarget::StandardNotificationLane,
                ..
            }
        ));
    }
}

#[test]
fn quiet_hours_daytime_negative_offset_defers_and_resumes_cleanly() {
    let root = TempRoot::new("quiet-daytime");
    let rate = AttentionRatePolicy {
        max_per_app_per_window: 2,
        max_global_ordinary_per_window: 4,
        window_ms: 60_000,
        allow_coalescing: false,
    };
    let day_quiet = QuietHoursPolicy {
        enabled: true,
        start_minute_of_day: 13 * 60,
        end_minute_of_day: 15 * 60,
        utc_offset_minutes: -300,
    };
    let controller = AttentionPolicyController::open(root.path(), rate, day_quiet).expect("open");
    let app_a = AttentionOrigin::Application(app(0xD2));

    let day0_utc_midnight: u64 = 20_779 * 86_400_000;
    let utc_minus5_to_utc = |day_offset: u64, hour: u64, min: u64, sec: u64, ms: u64| -> u64 {
        day0_utc_midnight
            + day_offset * 86_400_000
            + hour * 3_600_000
            + min * 60_000
            + sec * 1_000
            + ms
            + 300 * 60_000
    };
    let before_day_quiet = utc_minus5_to_utc(3, 12, 59, 59, 999);
    let inside_day_quiet = utc_minus5_to_utc(3, 13, 45, 0, 0);
    let day_resume = utc_minus5_to_utc(3, 15, 0, 0, 0);

    assert!(matches!(
        controller
            .evaluate(&req(
                20,
                app_a,
                AttentionLaneClass::Ordinary,
                None,
                before_day_quiet
            ))
            .expect("before daytime quiet"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::StandardNotificationLane,
            ..
        }
    ));
    assert_eq!(
        match controller
            .evaluate(&req(
                21,
                app_a,
                AttentionLaneClass::Ordinary,
                None,
                inside_day_quiet
            ))
            .expect("inside daytime quiet")
        {
            AttentionDecision::DeferredQuietHours { resume_at_ms, .. } => resume_at_ms,
            other => panic!("expected DeferredQuietHours, got {other:?}"),
        },
        day_resume
    );
    assert!(matches!(
        controller
            .evaluate(&req(
                22,
                AttentionOrigin::TrustedSystemUi,
                AttentionLaneClass::ConsentPrompt,
                None,
                inside_day_quiet
            ))
            .expect("consent prompt inside daytime quiet"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::PriorityConsentLane,
            ..
        }
    ));
    assert!(matches!(
        controller
            .evaluate(&req(
                23,
                app_a,
                AttentionLaneClass::BackgroundDigest,
                None,
                day_resume
            ))
            .expect("deliver at daytime resume"),
        AttentionDecision::Delivered {
            queue: AttentionQueueTarget::StandardNotificationLane,
            ..
        }
    ));
}

#[test]
fn flood_500_ordinary_notifications_never_starves_priority_consent_lane() {
    let root = TempRoot::new("flood-500");
    let rate = AttentionRatePolicy {
        max_per_app_per_window: 3,
        max_global_ordinary_per_window: 5,
        window_ms: 60_000,
        allow_coalescing: true,
    };
    let controller =
        AttentionPolicyController::open(root.path(), rate, disabled_quiet_hours()).expect("open");

    let mut standard_delivered = 0usize;
    let mut coalesced = 0usize;
    let mut rate_limited = 0usize;
    let mut priority_delivered = 0usize;

    for i in 0u64..500u64 {
        let app_seed = u8::try_from((i % 5) + 1).expect("app seed");
        let coalesce_tag = if i % 10 == 0 {
            Some("repeating-job".to_owned())
        } else {
            Some(format!("unique-alert-{i}"))
        };
        let decision = controller
            .evaluate(&AttentionDeliveryRequest {
                idempotency_key: key(u128::from(i) + 1_000),
                origin: AttentionOrigin::Application(app(app_seed)),
                lane: AttentionLaneClass::Ordinary,
                coalesce_key: coalesce_tag,
                requested_at_ms: 60_000 + i,
                payment_priority_override_attempted: false,
            })
            .expect("evaluate ordinary flood");

        match decision {
            AttentionDecision::Delivered {
                queue: AttentionQueueTarget::StandardNotificationLane,
                ..
            } => standard_delivered += 1,
            AttentionDecision::Coalesced { .. } => coalesced += 1,
            AttentionDecision::RateLimited { .. } => rate_limited += 1,
            other => panic!("unexpected flood outcome: {other:?}"),
        }

        // Interleave 50 TrustedSystemUi ConsentPrompt / SystemCritical calls throughout the flood.
        if i % 10 == 5 {
            let lane = if (i / 10).is_multiple_of(2) {
                AttentionLaneClass::ConsentPrompt
            } else {
                AttentionLaneClass::SystemCritical
            };
            let priority_decision = controller
                .evaluate(&AttentionDeliveryRequest {
                    idempotency_key: key(u128::from(i) + 10_000),
                    origin: AttentionOrigin::TrustedSystemUi,
                    lane,
                    coalesce_key: Some("repeating-job".to_owned()),
                    requested_at_ms: 60_000 + i,
                    payment_priority_override_attempted: false,
                })
                .expect("priority must never be blocked");
            assert!(matches!(
                priority_decision,
                AttentionDecision::Delivered {
                    queue: AttentionQueueTarget::PriorityConsentLane,
                    ..
                }
            ));
            priority_delivered += 1;
        }
    }

    assert_eq!(standard_delivered, 5);
    assert!(coalesced > 0);
    assert!(rate_limited > 400);
    assert_eq!(standard_delivered + coalesced + rate_limited, 500);
    assert_eq!(priority_delivered, 50);

    // Priority deliveries must not consume global ordinary quota.
    let usage = controller
        .window_usage(AttentionOrigin::Application(app(1)), 60_100)
        .expect("usage");
    assert_eq!(usage.global_standard_delivered, 5);
}

#[test]
fn negative_security_payment_bypass_and_untrusted_origin_audits() {
    let root = TempRoot::new("negative-security");
    let controller =
        AttentionPolicyController::open(root.path(), default_rate_policy(), disabled_quiet_hours())
            .expect("open");

    let bypass_cases = [
        (
            1u128,
            AttentionOrigin::Application(app(0x11)),
            AttentionLaneClass::Ordinary,
        ),
        (
            2,
            AttentionOrigin::Application(app(0x11)),
            AttentionLaneClass::BackgroundDigest,
        ),
        (
            3,
            AttentionOrigin::Application(app(0x11)),
            AttentionLaneClass::ConsentPrompt,
        ),
        (
            4,
            AttentionOrigin::TrustedSystemUi,
            AttentionLaneClass::Ordinary,
        ),
        (
            5,
            AttentionOrigin::TrustedSystemUi,
            AttentionLaneClass::ConsentPrompt,
        ),
    ];

    for (seed, origin, lane) in bypass_cases {
        let mut r = req(seed, origin, lane, Some("vip"), 5_000);
        r.payment_priority_override_attempted = true;
        assert!(matches!(
            controller.evaluate(&r),
            Err(AttentionPolicyError::PaymentBypassForbidden)
        ));
    }

    // Untrusted applications cannot forge ConsentPrompt or SystemCritical.
    for (seed, lane) in [
        (10u128, AttentionLaneClass::ConsentPrompt),
        (11, AttentionLaneClass::SystemCritical),
    ] {
        let r = req(
            seed,
            AttentionOrigin::Application(app(0x22)),
            lane,
            None,
            5_100,
        );
        assert!(matches!(
            controller.evaluate(&r),
            Err(AttentionPolicyError::UntrustedConsentOrigin)
        ));
    }

    drop(controller);

    // Reopen and verify all 7 security rejection receipts were durably committed to the audit log.
    let reopened = AttentionPolicyController::open_existing(root.path()).expect("reopen");
    let receipts = reopened.list_receipts(0, 50).expect("list receipts");
    assert_eq!(receipts.len(), 7);

    for seed in 1u128..=5 {
        let rec = reopened
            .inspect_receipt_by_idempotency_key(key(seed))
            .expect("inspect")
            .expect("payment bypass receipt persisted");
        assert_eq!(rec.outcome, AttentionOutcomeKind::RejectedPaymentBypass);
        assert!(rec.payment_priority_override_attempted);
        assert!(rec.queue_target.is_none());
    }

    for seed in [10u128, 11] {
        let rec = reopened
            .inspect_receipt_by_idempotency_key(key(seed))
            .expect("inspect")
            .expect("untrusted origin receipt persisted");
        assert_eq!(rec.outcome, AttentionOutcomeKind::RejectedUntrustedOrigin);
        assert!(!rec.payment_priority_override_attempted);
        assert!(rec.queue_target.is_none());
    }
}

fn assert_idempotency_conflicts(
    reopened: &AttentionPolicyController,
    r_delivered: &AttentionDeliveryRequest,
    r_coalesced: &AttentionDeliveryRequest,
    r_payment: &AttentionDeliveryRequest,
) {
    let mut conflict_time = r_delivered.clone();
    conflict_time.requested_at_ms += 1;
    assert!(matches!(
        reopened.evaluate(&conflict_time),
        Err(AttentionPolicyError::IdempotencyConflict(k)) if k == key(2)
    ));

    let mut conflict_key = r_coalesced.clone();
    conflict_key.coalesce_key = Some("job-mutated".to_owned());
    assert!(matches!(
        reopened.evaluate(&conflict_key),
        Err(AttentionPolicyError::IdempotencyConflict(k)) if k == key(3)
    ));

    let mut conflict_payment = r_payment.clone();
    conflict_payment.payment_priority_override_attempted = false;
    assert!(matches!(
        reopened.evaluate(&conflict_payment),
        Err(AttentionPolicyError::IdempotencyConflict(k)) if k == key(6)
    ));
}

fn build_replay_sample_requests(app_a: AttentionOrigin) -> Vec<AttentionDeliveryRequest> {
    let mut r_payment = req(6, app_a, AttentionLaneClass::Ordinary, None, 12 * 3_600_000);
    r_payment.payment_priority_override_attempted = true;
    vec![
        req(
            1,
            AttentionOrigin::TrustedSystemUi,
            AttentionLaneClass::ConsentPrompt,
            None,
            12 * 3_600_000,
        ),
        req(
            2,
            app_a,
            AttentionLaneClass::Ordinary,
            Some("job"),
            12 * 3_600_000,
        ),
        req(
            3,
            app_a,
            AttentionLaneClass::Ordinary,
            Some("job"),
            12 * 3_600_000 + 100,
        ),
        req(
            4,
            app_a,
            AttentionLaneClass::Ordinary,
            Some("other"),
            12 * 3_600_000 + 200,
        ),
        req(
            5,
            app_a,
            AttentionLaneClass::BackgroundDigest,
            None,
            23 * 3_600_000,
        ),
        r_payment,
        req(
            7,
            app_a,
            AttentionLaneClass::SystemCritical,
            None,
            12 * 3_600_000,
        ),
    ]
}

#[test]
fn deterministic_idempotent_replay_across_restart_and_conflict_rejection() {
    let root = TempRoot::new("idempotency-replay");
    let rate = AttentionRatePolicy {
        max_per_app_per_window: 1,
        max_global_ordinary_per_window: 5,
        window_ms: 10_000,
        allow_coalescing: true,
    };
    let quiet = QuietHoursPolicy {
        enabled: true,
        start_minute_of_day: 22 * 60,
        end_minute_of_day: 7 * 60,
        utc_offset_minutes: 0,
    };
    let controller = AttentionPolicyController::open(root.path(), rate, quiet).expect("open");
    let app_a = AttentionOrigin::Application(app(0x55));
    let reqs = build_replay_sample_requests(app_a);

    let d_ok: Vec<AttentionDecision> = reqs[..5]
        .iter()
        .map(|r| controller.evaluate(r).expect("ok outcome"))
        .collect();
    assert!(matches!(
        controller.evaluate(&reqs[5]),
        Err(AttentionPolicyError::PaymentBypassForbidden)
    ));
    assert!(matches!(
        controller.evaluate(&reqs[6]),
        Err(AttentionPolicyError::UntrustedConsentOrigin)
    ));

    controller
        .update_policy(
            AttentionRatePolicy {
                max_per_app_per_window: 99,
                ..rate
            },
            disabled_quiet_hours(),
        )
        .expect("update policy");
    drop(controller);

    let reopened = AttentionPolicyController::open_existing(root.path()).expect("reopen");
    for (r, expected) in reqs[..5].iter().zip(d_ok.iter()) {
        assert_eq!(&reopened.evaluate(r).expect("replay ok"), expected);
    }
    assert!(matches!(
        reopened.evaluate(&reqs[5]),
        Err(AttentionPolicyError::PaymentBypassForbidden)
    ));
    assert!(matches!(
        reopened.evaluate(&reqs[6]),
        Err(AttentionPolicyError::UntrustedConsentOrigin)
    ));

    let counter = reopened
        .inspect_coalesce_counter(app_a, "job", 12 * 3_600_000)
        .expect("counter")
        .expect("exists");
    assert_eq!(counter.coalesced_count, 1);
    let receipt = reopened
        .inspect_receipt(d_ok[2].receipt_id())
        .expect("inspect by receipt_id")
        .expect("found");
    assert_eq!(receipt.idempotency_key, key(3));

    assert_idempotency_conflicts(&reopened, &reqs[1], &reqs[2], &reqs[5]);
}

#[test]
fn sqlite_immutability_triggers_and_tamper_detection_fail_closed() {
    let root = TempRoot::new("triggers-tamper");
    let controller =
        AttentionPolicyController::open(root.path(), default_rate_policy(), disabled_quiet_hours())
            .expect("open");

    let app_a = AttentionOrigin::Application(app(0x77));
    let d1 = controller
        .evaluate(&req(
            1,
            app_a,
            AttentionLaneClass::Ordinary,
            Some("sync"),
            60_000,
        ))
        .expect("deliver");
    let _d2 = controller
        .evaluate(&req(
            2,
            app_a,
            AttentionLaneClass::Ordinary,
            Some("sync"),
            60_500,
        ))
        .expect("coalesce");
    drop(controller);

    let db_path = root.path().join(ATTENTION_POLICY_DB_FILENAME);
    let raw_db = Connection::open(&db_path).expect("raw open");

    // 1. Receipt table UPDATE and DELETE must be aborted by triggers.
    assert!(
        raw_db
            .execute(
                "UPDATE attention_decision_receipts SET outcome = 'RATE_LIMITED' WHERE receipt_id = ?1",
                params![d1.receipt_id().as_bytes().as_slice()],
            )
            .is_err()
    );
    assert!(
        raw_db
            .execute(
                "DELETE FROM attention_decision_receipts WHERE receipt_id = ?1",
                params![d1.receipt_id().as_bytes().as_slice()],
            )
            .is_err()
    );

    // 2. Policy config UPDATE and DELETE must be aborted by triggers.
    assert!(
        raw_db
            .execute(
                "UPDATE attention_policy_config SET max_per_app_per_window = 999 WHERE policy_generation = 1",
                [],
            )
            .is_err()
    );
    assert!(
        raw_db
            .execute(
                "DELETE FROM attention_policy_config WHERE policy_generation = 1",
                [],
            )
            .is_err()
    );

    // 3. Coalesce counter non-monotonic UPDATE (e.g., resetting count to 0) and DELETE must abort.
    assert!(
        raw_db
            .execute(
                "UPDATE attention_coalesce_counters SET coalesced_count = 0 WHERE coalesce_key = 'sync'",
                [],
            )
            .is_err()
    );
    assert!(
        raw_db
            .execute(
                "DELETE FROM attention_coalesce_counters WHERE coalesce_key = 'sync'",
                [],
            )
            .is_err()
    );

    // 4. Dropping a trigger causes open_existing/open to fail closed with CorruptRecord.
    raw_db
        .execute("DROP TRIGGER trg_attention_decision_receipts_no_delete", [])
        .expect("drop trigger");
    drop(raw_db);

    assert!(matches!(
        AttentionPolicyController::open_existing(root.path()),
        Err(AttentionPolicyError::CorruptRecord(_))
    ));
    assert!(matches!(
        AttentionPolicyController::open(root.path(), default_rate_policy(), disabled_quiet_hours()),
        Err(AttentionPolicyError::CorruptRecord(_))
    ));

    // 5. Unsupported user_version fails closed with SchemaVersionUnsupported.
    let raw_db = Connection::open(&db_path).expect("raw open again");
    raw_db
        .pragma_update(None, "user_version", 99)
        .expect("bump version");
    drop(raw_db);
    assert!(matches!(
        AttentionPolicyController::open_existing(root.path()),
        Err(AttentionPolicyError::SchemaVersionUnsupported(99))
    ));
}
