//! `C-UNFREEZE-001` (`W66`) production wiring integration tests for
//! [`nlos_search::SearchService`] and [`nlos_notify::NotificationService`] inside
//! `nlos-system-control`.
//!
//! Covers four production pillars without SABI `.proto` changes:
//! 1. `SystemControlDaemon` end-to-end `InspectSearch` over real admitted and
//!    retracted `SemanticAuthority` assertions (`[CONS-DECL-001]`,
//!    `[CONS-CTRL-001]`, `[PERF-CONTROL-001]`);
//! 2. `SystemControlDaemon` end-to-end `InspectNotification` over real
//!    `NotificationService` subscriptions, `[TASK-NOTIFY-001]` background task
//!    transition publications, `WaitAuthority` delivery-wait wakeups, and
//!    `[DUI-OFFLINE-001]` bounded offline history + retention pruning;
//! 3. `[CTRL-PARITY-001]` byte-level receipt parity across English/Chinese
//!    `parse_nl_command`, structured `ControlCommand`, in-process daemon
//!    dispatch, and ADR-0011 authenticated IPC socket/named-pipe round-trips;
//! 4. Fail-closed `NOT_FOUND` behavior when `SearchInspector` or
//!    `NotificationInspector` is unwired (`None`), preserving exact legacy
//!    signatures of `ControlReceipt::compose`, `dispatch_in_process`, and
//!    `dispatch_over_authenticated_socket`.

#![cfg(feature = "daemon")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::Signer as _;
use nlos_capability::{
    CapabilityAuthority, CapabilityRights, CapabilityTarget, IssueRootCapabilityRequest,
    SignedIssueRootCapabilityRequest, issue_root_command_message,
};
use nlos_channel::{ChannelDecision, CreateChannelRequest};
use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_notify::{
    AckNotificationRequest, NotificationId, NotificationPublishDecision,
    NotificationSubscribeDecision, RegisterDeliveryWaitRequest, SubscribeNotificationRequest,
    TaskNotificationEnvelope, TaskTransitionKind,
};
use nlos_process::{CreateIsolationDomainRequest, RegisterDelegatedProcessRequest};
use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode};
use nlos_semantic::{
    AppendAssertionRequest, AppendDecision, AppendTypedEventRequest, AssertionMode,
    LocalProcessRef, RetractionMode, StoreSigner, StoreSignerError, TaintFlags,
    UnsignedAssertionEvent, UnsignedRetractionEvent, content_digest,
    encode_unsigned_assertion_event, encode_unsigned_retraction_event, semantic_event_id,
};
use nlos_system_control::RecoveryHealthSource;
use nlos_system_control::RecoverySystemControl;
use nlos_system_control::auth::{
    dispatch_over_authenticated_socket, dispatch_over_authenticated_socket_with_services,
};
use nlos_system_control::control::{
    ControlCommand, ControlOutcome, ControlReceipt, NotificationInspection, SearchInspection,
    build_request_envelope, dispatch_in_process, dispatch_in_process_with_services, receipt_to_hex,
};
use nlos_system_control::daemon::{
    ControlCapabilityPolicy, DaemonOptions, SystemControlDaemon, assemble,
    serve_authenticated_endpoint,
};
use nlos_system_control::nl::parse_nl_command;
use nlos_system_control::notify_inspector::NotificationServiceInspector;
use nlos_system_control::search_inspector::{
    SearchServiceInspector, UNSCOPED_SEARCH_SCOPE_ID, encode_namespace_scope_id,
    encode_task_scope_id,
};
use nlos_topic::{CreateTopicRequest, SubscriberKey, TopicDecision, TopicId, TopicPolicy};
use nlos_types::{
    ApplicationId, ControlDomainId, Generation, IdempotencyKey, KeyId, NamespaceId, PrincipalId,
    ReceiptId, ResourceAccountId, SemanticEventId, TaskAttemptId, TaskId,
};
use nlos_wait::{BindingId, RegisterDecision, WaitState};

static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-sc-unfreeze-{label}-{}-{nonce}-{seq}",
            std::process::id(),
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn temp_endpoint(label: &str, suffix: &str) -> PathBuf {
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nlos-sc-uf-{label}-{suffix}-{}-{seq}.sock",
        std::process::id(),
    ))
}

#[cfg(windows)]
fn temp_endpoint(label: &str, suffix: &str) -> PathBuf {
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        r"\\.\pipe\nlos-sc-uf-{label}-{suffix}-{}-{seq}",
        std::process::id(),
    ))
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[derive(Clone)]
struct TestStoreSigner {
    key: ed25519_dalek::SigningKey,
    binding: IdentityBinding,
}

impl StoreSigner for TestStoreSigner {
    fn principal_id(&self) -> PrincipalId {
        self.binding.principal_id
    }

    fn control_domain_id(&self) -> ControlDomainId {
        self.binding.control_domain_id
    }

    fn key_id(&self) -> KeyId {
        self.binding.key_id
    }

    fn sign(&self, message_digest: &[u8; 32]) -> Result<[u8; 64], StoreSignerError> {
        Ok(self.key.sign(message_digest).to_bytes())
    }
}

struct SemanticSeedHarness {
    capability: CapabilityAuthority,
    issuer_key: ed25519_dalek::SigningKey,
    issuer: IdentityBinding,
    process_binding: nlos_process::ProcessBindingRecord,
    store_signer: TestStoreSigner,
}

fn bootstrap_signing_principal(
    identity: &IdentityAuthority,
    seed: u8,
) -> (ed25519_dalek::SigningKey, IdentityBinding) {
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 4_102_444_800_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap signing principal")
        .binding();
    (key, binding)
}

fn setup_semantic_harness(daemon: &SystemControlDaemon, seed: u8) -> SemanticSeedHarness {
    let (issuer_key, issuer) = bootstrap_signing_principal(&daemon.identity, seed);
    let (store_key, store_binding) =
        bootstrap_signing_principal(&daemon.identity, seed.wrapping_add(20));
    let domain = daemon
        .process
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [seed.wrapping_add(4); 32],
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
            created_at_ms: 0,
        })
        .expect("create isolation domain")
        .record()
        .clone();
    let process_binding = daemon
        .process
        .register_delegated_process(RegisterDelegatedProcessRequest {
            task_id: TaskId::from_bytes([seed.wrapping_add(6); 16]),
            task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(7); 16]),
            attempt_generation: Generation::INITIAL,
            isolation_domain_id: domain.isolation_domain_id,
            isolation_domain_generation: domain.generation,
            isolation_domain_fencing_token: domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(8); 16]),
            created_at_ms: 0,
        })
        .expect("register delegated process")
        .record()
        .clone();
    let capability = CapabilityAuthority::open(daemon.root.join("capability"))
        .expect("open capability authority");
    SemanticSeedHarness {
        capability,
        issuer_key,
        issuer,
        process_binding,
        store_signer: TestStoreSigner {
            key: store_key,
            binding: store_binding,
        },
    }
}

fn issue_semantic_capability(
    daemon: &SystemControlDaemon,
    harness: &SemanticSeedHarness,
    scope: CapabilityTarget,
    rights: CapabilityRights,
    seed: u8,
) -> nlos_capability::CapabilityRecord {
    let command = IssueRootCapabilityRequest {
        issuer_key_id: harness.issuer.key_id,
        holder_key_id: harness.issuer.key_id,
        target: scope,
        rights,
        purpose_digest: Some([0x77; 32]),
        valid_from_ms: 0,
        valid_until_ms: 90_000,
        delegation_depth_remaining: 0,
        call_limit: None,
        idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
        issued_at_ms: 0,
    };
    harness
        .capability
        .issue_root_signed(
            &daemon.identity,
            SignedIssueRootCapabilityRequest {
                command,
                signer: harness.issuer.principal_id,
                signature: harness
                    .issuer_key
                    .sign(&issue_root_command_message(command))
                    .to_bytes(),
            },
        )
        .expect("issue semantic capability")
        .record()
}

fn admit_assertion(
    daemon: &SystemControlDaemon,
    harness: &SemanticSeedHarness,
    scope: CapabilityTarget,
    append_cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    content: &str,
) -> SemanticEventId {
    let media_type = "text/plain";
    let event = UnsignedAssertionEvent {
        scope,
        issuer: harness.issuer.principal_id,
        issuer_execution: LocalProcessRef {
            process_id: harness.process_binding.process_id,
            generation: harness.process_binding.process_generation,
        },
        control_domain: harness.issuer.control_domain_id,
        issued_at_unix_ns: 1_000_000_000 + u64::from(seed),
        nonce: vec![seed; 16],
        declared_parents: Vec::new(),
        declassification_receipt_id: None,
        valid_until_ms: Some(80_000),
        purpose_digest: Some([0x77; 32]),
        content_digest: content_digest(media_type, content.as_bytes()).expect("content digest"),
        assertion_mode: AssertionMode::FactFromTool,
        execution_evidence_receipt_id: Some(ReceiptId::from_bytes([seed.wrapping_add(0x30); 16])),
        confidence_bp: Some(9_000),
        key_id: harness.issuer.key_id,
    };
    let canonical = encode_unsigned_assertion_event(&event).expect("encode assertion");
    let event_id = semantic_event_id(&canonical);
    let request = AppendAssertionRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: harness
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: append_cap.handle,
        content_media_type: media_type.to_owned(),
        content_bytes: content.as_bytes().to_vec(),
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(85_000),
        admitted_at_ms: 2_000 + u64::from(seed),
    };
    match daemon.semantic.append_assertion(
        &daemon.identity,
        &harness.capability,
        &daemon.process,
        &harness.store_signer,
        &request,
    ) {
        Ok(AppendDecision::Admitted(_) | AppendDecision::Replayed(_)) => event_id,
        Err(error) => panic!("admit assertion failed: {error}"),
    }
}

fn admit_retraction(
    daemon: &SystemControlDaemon,
    harness: &SemanticSeedHarness,
    scope: CapabilityTarget,
    retract_cap: &nlos_capability::CapabilityRecord,
    seed: u8,
    target: SemanticEventId,
) {
    let event = UnsignedRetractionEvent {
        scope,
        issuer: harness.issuer.principal_id,
        issuer_execution: LocalProcessRef {
            process_id: harness.process_binding.process_id,
            generation: harness.process_binding.process_generation,
        },
        control_domain: harness.issuer.control_domain_id,
        issued_at_unix_ns: 6_000_000_000 + u64::from(seed),
        nonce: vec![seed.wrapping_add(100); 16],
        declared_parents: Vec::new(),
        valid_until_ms: None,
        purpose_digest: Some([0x77; 32]),
        key_id: harness.issuer.key_id,
        target_event_id: target,
        mode: RetractionMode::Withdraw,
        reason_digest: Some([0xdb; 32]),
        authority_evidence_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x70); 16]),
    };
    let canonical = encode_unsigned_retraction_event(&event).expect("encode retraction");
    let event_id = semantic_event_id(&canonical);
    let request = AppendTypedEventRequest {
        canonical_unsigned_event: canonical,
        claimed_event_id: event_id,
        signature: harness
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(event_id))
            .to_bytes(),
        capability: retract_cap.handle,
        captured_inputs: Vec::new(),
        ingress_taint: TaintFlags::default(),
        authz_policy_digest: [0x99; 32],
        admission_limit_ms: Some(85_000),
        admitted_at_ms: 3_000 + u64::from(seed),
    };
    match daemon.semantic.append_retraction(
        &daemon.identity,
        &harness.capability,
        &daemon.process,
        &harness.store_signer,
        &request,
    ) {
        Ok(AppendDecision::Admitted(_) | AppendDecision::Replayed(_)) => {}
        Err(error) => panic!("admit retraction failed: {error}"),
    }
}

fn create_daemon_topic(daemon: &SystemControlDaemon, chan_seed: u8, topic_seed: u8) -> TopicId {
    let channel_record = match daemon
        .channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 32_768,
            policy_digest: [0x55; 32],
            idempotency_key: IdempotencyKey::from_bytes([chan_seed; 16]),
            created_at_ms: 1_000,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh channel cannot replay"),
    };
    let topic_record = match daemon
        .topics
        .create_topic(CreateTopicRequest {
            channel_id: channel_record.channel_id,
            name: b"system/task-transitions".to_vec(),
            policy: TopicPolicy {
                max_recipients: 8,
                delivery_attempts: 4,
                cascade_depth: 2,
                retained_bytes: 32_768,
                retention_ms: 86_400_000,
                payer: ResourceAccountId::from_bytes([0x09; 16]),
            },
            idempotency_key: IdempotencyKey::from_bytes([topic_seed; 16]),
            created_at_ms: 1_500,
        })
        .expect("create topic")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh topic cannot replay"),
    };
    topic_record.topic_id
}

fn subscribe_daemon_notification(
    daemon: &SystemControlDaemon,
    topic_id: TopicId,
    subscriber_seed: u8,
) -> NotificationId {
    match daemon
        .notification_service()
        .subscribe(SubscribeNotificationRequest {
            topic_id,
            subscriber_key: SubscriberKey::from_bytes([subscriber_seed; 16]),
            subscribed_at_ms: 2_000,
        })
        .expect("subscribe notification")
    {
        NotificationSubscribeDecision::Subscribed(sub) => sub.notification_id,
        NotificationSubscribeDecision::Replayed(_) => panic!("fresh subscription cannot replay"),
    }
}

fn shutdown_daemon(daemon: Arc<SystemControlDaemon>) {
    daemon.stop_worker();
    daemon.stop_materialization();
    drop(daemon);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // End-to-end daemon scenario across namespace, task, unscoped, and malformed scopes.
async fn daemon_inspect_search_end_to_end_over_admitted_and_retracted_assertions() {
    let root = TempRoot::new("search-e2e");
    let auth_ep = temp_endpoint("search-e2e", "auth");
    let plain_ep = temp_endpoint("search-e2e", "plain");
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(root.path())
            .with_auth_socket(&auth_ep)
            .with_plain_socket(&plain_ep),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    assert!(root.path().join("waits").exists());
    assert!(
        root.path()
            .join("notify")
            .join("notify-service.db")
            .exists()
    );
    assert!(
        root.path()
            .join("semantic")
            .join("semantic-authority.db")
            .exists()
    );
    assert_eq!(
        daemon
            .search_service()
            .build_index()
            .expect("empty index")
            .assertion_count(),
        0
    );

    let ns_alpha = [0x44; 16];
    let ns_beta = [0x55; 16];
    let task_gamma = [0x66; 16];
    let scope_alpha = CapabilityTarget::Namespace(NamespaceId::from_bytes(ns_alpha));
    let scope_beta = CapabilityTarget::Namespace(NamespaceId::from_bytes(ns_beta));
    let scope_task = CapabilityTarget::Task(TaskId::from_bytes(task_gamma));

    {
        let harness = setup_semantic_harness(&daemon, 0x10);
        let rights = CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::SEMANTIC_RETRACT);
        let cap_alpha = issue_semantic_capability(&daemon, &harness, scope_alpha, rights, 0x71);
        let cap_beta = issue_semantic_capability(&daemon, &harness, scope_beta, rights, 0x72);
        let cap_task = issue_semantic_capability(&daemon, &harness, scope_task, rights, 0x73);

        let first = admit_assertion(
            &daemon,
            &harness,
            scope_alpha,
            &cap_alpha,
            1,
            "alpha-active-assertion",
        );
        let second = admit_assertion(
            &daemon,
            &harness,
            scope_alpha,
            &cap_alpha,
            2,
            "alpha-retracted-assertion",
        );
        let _third = admit_assertion(
            &daemon,
            &harness,
            scope_beta,
            &cap_beta,
            3,
            "beta-active-assertion",
        );
        let _fourth = admit_assertion(
            &daemon,
            &harness,
            scope_task,
            &cap_task,
            4,
            "task-scoped-assertion",
        );
        admit_retraction(&daemon, &harness, scope_alpha, &cap_alpha, 5, second);
        assert_ne!(first, second);
    }

    // Namespace-scoped inspection (`[0x01, 0..0, ns_alpha]`).
    let alpha_scope_id = encode_namespace_scope_id(NamespaceId::from_bytes(ns_alpha));
    let alpha_receipt = daemon
        .dispatch_command(
            &ControlCommand::InspectSearch {
                scope_id: alpha_scope_id,
            },
            100,
            5_000,
        )
        .expect("dispatch alpha InspectSearch");
    assert_eq!(
        alpha_receipt.outcome,
        Ok(ControlOutcome::SearchInspected(SearchInspection {
            scope_id: alpha_scope_id,
            consistency_class: 1,
            degradation_policy: 0,
            non_authoritative_restrictions_bits: 0x1F,
            admitted_event_count: 2,
            active_assertion_count: 1,
            retracted_event_count: 1,
            truncated: false,
        }))
    );

    // Task-scoped inspection (`[0x02, 0..0, task_gamma]`).
    let task_scope_id = encode_task_scope_id(TaskId::from_bytes(task_gamma));
    let task_receipt = daemon
        .dispatch_command(
            &ControlCommand::InspectSearch {
                scope_id: task_scope_id,
            },
            110,
            5_010,
        )
        .expect("dispatch task InspectSearch");
    assert_eq!(
        task_receipt.outcome,
        Ok(ControlOutcome::SearchInspected(SearchInspection {
            scope_id: task_scope_id,
            consistency_class: 1,
            degradation_policy: 0,
            non_authoritative_restrictions_bits: 0x1F,
            admitted_event_count: 1,
            active_assertion_count: 1,
            retracted_event_count: 0,
            truncated: false,
        }))
    );

    // Unscoped inspection (`[0; 32]`) observes all 4 admitted assertions (3 active, 1 retracted).
    let unscoped_receipt = daemon
        .dispatch_command(
            &ControlCommand::InspectSearch {
                scope_id: UNSCOPED_SEARCH_SCOPE_ID,
            },
            120,
            5_020,
        )
        .expect("dispatch unscoped InspectSearch");
    assert_ne!(unscoped_receipt.control_command_id, [0; 16]);
    assert_ne!(unscoped_receipt.correlation_id, vec![0; 16]);
    assert_eq!(
        unscoped_receipt.outcome,
        Ok(ControlOutcome::SearchInspected(SearchInspection {
            scope_id: UNSCOPED_SEARCH_SCOPE_ID,
            consistency_class: 1,
            degradation_policy: 0,
            non_authoritative_restrictions_bits: 0x1F,
            admitted_event_count: 4,
            active_assertion_count: 3,
            retracted_event_count: 1,
            truncated: false,
        }))
    );

    // Malformed `scope_id` tag (`0xFF`) fails closed with `INVALID_ARGUMENT`.
    let invalid_scope = daemon
        .dispatch_command(
            &ControlCommand::InspectSearch {
                scope_id: [0xFF; 32],
            },
            130,
            5_030,
        )
        .expect("dispatch malformed scope_id");
    let failure = invalid_scope
        .outcome
        .expect_err("malformed scope_id must fail closed");
    assert_eq!(failure.code, i32::from(SabiErrorCode::InvalidArgument));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));

    drop(endpoints);
    shutdown_daemon(daemon);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // End-to-end daemon scenario across subscribe, wait wake, publish, ack, and prune.
async fn daemon_inspect_notification_end_to_end_over_task_transitions_and_offline_history() {
    let root = TempRoot::new("notify-e2e");
    let auth_ep = temp_endpoint("notify-e2e", "auth");
    let plain_ep = temp_endpoint("notify-e2e", "plain");
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(root.path())
            .with_auth_socket(&auth_ep)
            .with_plain_socket(&plain_ep),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    let topic_id = create_daemon_topic(&daemon, 0x21, 0x22);
    let subscriber_bytes = [0x33; 16];
    let notification_id = subscribe_daemon_notification(&daemon, topic_id, 0x33);

    let wait_row = match daemon
        .notification_service()
        .register_delivery_wait(RegisterDeliveryWaitRequest {
            notification_id,
            target_sequence: 3,
            binding: BindingId::from_bytes([0x41; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
            registered_at_ms: 2_100,
        })
        .expect("register delivery wait")
    {
        RegisterDecision::Registered(row) => row,
        RegisterDecision::Replayed(_) => panic!("fresh wait registration cannot replay"),
    };
    assert_eq!(wait_row.state, WaitState::Pending);

    for (transition, at_ms, requires_action, idem_seed) in [
        (TaskTransitionKind::Proposed, 3_000_u64, false, 0x51_u8),
        (TaskTransitionKind::WaitingConsent, 3_100, true, 0x52),
        (TaskTransitionKind::Completed, 3_200, false, 0x53),
    ] {
        let envelope = TaskNotificationEnvelope {
            task_id: TaskId::from_bytes([0xA1; 16]),
            application_id: ApplicationId::from_bytes([0xB1; 16]),
            task_generation: Generation::INITIAL,
            transition,
            occurred_at_ms: at_ms,
            summary_digest: [0xCC; 32],
            requires_user_action: requires_action,
        };
        let decision = daemon
            .notification_service()
            .publish_task_transition(
                topic_id,
                &envelope,
                IdempotencyKey::from_bytes([idem_seed; 16]),
            )
            .expect("publish task transition");
        assert!(matches!(
            decision,
            NotificationPublishDecision::Published(_)
        ));
    }

    assert_eq!(
        daemon
            .wait_authority()
            .inspect_wait(wait_row.wait_id)
            .expect("inspect wait")
            .state,
        WaitState::Woken,
    );

    let before_ack = daemon
        .dispatch_command(
            &ControlCommand::InspectNotification {
                notification_id: *notification_id.as_bytes(),
            },
            200,
            6_000,
        )
        .expect("dispatch InspectNotification before ack");
    assert_eq!(
        before_ack.outcome,
        Ok(ControlOutcome::NotificationInspected(
            NotificationInspection {
                notification_id: *notification_id.as_bytes(),
                topic_id: *topic_id.as_bytes(),
                subscriber_key: subscriber_bytes,
                next_sequence: 1,
                history_entry_count: 3,
                history_truncated: false,
                prune_watermark_sequence: 0,
            }
        ))
    );

    daemon
        .notification_service()
        .ack(AckNotificationRequest {
            notification_id,
            up_to_sequence: 2,
            acked_at_ms: 4_000,
        })
        .expect("ack notification up to sequence 2");
    let prune = daemon
        .notification_service()
        .prune_history_before(topic_id, 2)
        .expect("prune history before sequence 2");
    assert_eq!(prune.pruned_entries, 1);
    assert_eq!(prune.retained_from_sequence, 2);

    let after_ack = daemon
        .dispatch_command(
            &ControlCommand::InspectNotification {
                notification_id: *notification_id.as_bytes(),
            },
            210,
            6_100,
        )
        .expect("dispatch InspectNotification after ack and prune");
    assert_eq!(
        after_ack.outcome,
        Ok(ControlOutcome::NotificationInspected(
            NotificationInspection {
                notification_id: *notification_id.as_bytes(),
                topic_id: *topic_id.as_bytes(),
                subscriber_key: subscriber_bytes,
                next_sequence: 3,
                history_entry_count: 2,
                history_truncated: false,
                prune_watermark_sequence: 2,
            }
        ))
    );

    // Unknown `notification_id` fails closed with `NOT_FOUND`.
    let unknown = daemon
        .dispatch_command(
            &ControlCommand::InspectNotification {
                notification_id: [0xEE; 16],
            },
            220,
            6_200,
        )
        .expect("dispatch unknown notification_id");
    let failure = unknown
        .outcome
        .expect_err("unknown notification_id must fail closed");
    assert_eq!(failure.code, i32::from(SabiErrorCode::NotFound));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));

    drop(endpoints);
    shutdown_daemon(daemon);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // End-to-end CTRL-PARITY-001 scenario across EN/ZH NL, in-process, and authenticated IPC.
async fn nl_command_structured_and_authenticated_ipc_receipt_parity() {
    let root = TempRoot::new("ctrl-parity");
    let auth_ep = temp_endpoint("ctrl-parity", "auth");
    let plain_ep = temp_endpoint("ctrl-parity", "plain");
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(root.path())
            .with_auth_socket(&auth_ep)
            .with_plain_socket(&plain_ep),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    let ns_bytes = [0x7B; 16];
    let scope_id = encode_namespace_scope_id(NamespaceId::from_bytes(ns_bytes));
    {
        let harness = setup_semantic_harness(&daemon, 0x30);
        let cap = issue_semantic_capability(
            &daemon,
            &harness,
            CapabilityTarget::Namespace(NamespaceId::from_bytes(ns_bytes)),
            CapabilityRights::SEMANTIC_APPEND,
            0x81,
        );
        let _ = admit_assertion(
            &daemon,
            &harness,
            CapabilityTarget::Namespace(NamespaceId::from_bytes(ns_bytes)),
            &cap,
            1,
            "parity-assertion",
        );
    }

    let topic_id = create_daemon_topic(&daemon, 0x61, 0x62);
    let notification_id = subscribe_daemon_notification(&daemon, topic_id, 0x63);
    let notif_bytes = *notification_id.as_bytes();
    daemon
        .notification_service()
        .publish_task_transition(
            topic_id,
            &TaskNotificationEnvelope {
                task_id: TaskId::from_bytes([0xD1; 16]),
                application_id: ApplicationId::from_bytes([0xE1; 16]),
                task_generation: Generation::INITIAL,
                transition: TaskTransitionKind::Running,
                occurred_at_ms: 4_500,
                summary_digest: [0xAB; 32],
                requires_user_action: false,
            },
            IdempotencyKey::from_bytes([0x64; 16]),
        )
        .expect("publish parity task transition");

    let scope_hex = hex_encode(&scope_id);
    let notif_hex = hex_encode(&notif_bytes);

    let structured_search = ControlCommand::InspectSearch { scope_id };
    let nl_search_en =
        parse_nl_command(&format!("inspect search {scope_hex}")).expect("parse EN inspect search");
    let nl_search_zh =
        parse_nl_command(&format!("查看搜索 {scope_hex}")).expect("parse ZH inspect search");
    assert_eq!(nl_search_en, structured_search);
    assert_eq!(nl_search_zh, structured_search);

    let structured_notify = ControlCommand::InspectNotification {
        notification_id: notif_bytes,
    };
    let nl_notify_en = parse_nl_command(&format!("inspect notification {notif_hex}"))
        .expect("parse EN inspect notification");
    let nl_notify_zh =
        parse_nl_command(&format!("查看通知 {notif_hex}")).expect("parse ZH inspect notification");
    assert_eq!(nl_notify_en, structured_notify);
    assert_eq!(nl_notify_zh, structured_notify);

    let search_in_proc = daemon
        .dispatch_command(&structured_search, 300, 7_000)
        .expect("in-process search receipt");
    let search_nl_in_proc = daemon
        .dispatch_command(&nl_search_en, 300, 7_000)
        .expect("in-process NL search receipt");
    assert_eq!(search_in_proc.to_bytes(), search_nl_in_proc.to_bytes());
    assert_eq!(
        receipt_to_hex(&search_in_proc),
        receipt_to_hex(&search_nl_in_proc)
    );
    // Tag `23` at byte offset `16 (command_id) + 4 (len) + 16 (correlation_id) = 36`.
    assert_eq!(search_in_proc.to_bytes()[36], 23);

    let notify_in_proc = daemon
        .dispatch_command(&structured_notify, 310, 7_100)
        .expect("in-process notify receipt");
    let notify_nl_in_proc = daemon
        .dispatch_command(&nl_notify_zh, 310, 7_100)
        .expect("in-process NL notify receipt");
    assert_eq!(notify_in_proc.to_bytes(), notify_nl_in_proc.to_bytes());
    assert_eq!(
        receipt_to_hex(&notify_in_proc),
        receipt_to_hex(&notify_nl_in_proc)
    );
    // Tag `24` at byte offset `36`.
    assert_eq!(notify_in_proc.to_bytes()[36], 24);

    // ADR-0011 authenticated endpoint parity over real host IPC (Unix domain socket / Windows named pipe).
    let (client_key, client_binding) = bootstrap_signing_principal(&daemon.identity, 0x95);
    let stop = Arc::new(AtomicBool::new(false));
    let server_daemon = Arc::clone(&daemon);
    let server_stop = Arc::clone(&stop);
    let server = tokio::spawn(serve_authenticated_endpoint(
        server_daemon,
        endpoints.listener_authenticated,
        server_stop,
    ));
    drop(endpoints.listener_plain);

    let search_inspector =
        SearchServiceInspector::new(daemon.search_service(), daemon.semantic.as_ref());
    let notify_inspector =
        NotificationServiceInspector::new(daemon.notification_service(), daemon.topics.as_ref());

    let search_ipc = dispatch_over_authenticated_socket_with_services(
        &auth_ep,
        client_binding.principal_id,
        |digest| Ok(client_key.sign(digest).to_bytes()),
        &nl_search_en,
        None,
        None,
        None,
        Some(&search_inspector),
        Some(&notify_inspector),
    )
    .await
    .expect("authenticated IPC InspectSearch");
    assert_eq!(search_ipc.to_bytes(), search_in_proc.to_bytes());
    assert_eq!(receipt_to_hex(&search_ipc), receipt_to_hex(&search_in_proc));

    let notify_ipc = dispatch_over_authenticated_socket_with_services(
        &auth_ep,
        client_binding.principal_id,
        |digest| Ok(client_key.sign(digest).to_bytes()),
        &nl_notify_en,
        None,
        None,
        None,
        Some(&search_inspector),
        Some(&notify_inspector),
    )
    .await
    .expect("authenticated IPC InspectNotification");
    assert_eq!(notify_ipc.to_bytes(), notify_in_proc.to_bytes());
    assert_eq!(receipt_to_hex(&notify_ipc), receipt_to_hex(&notify_in_proc));

    // Legacy `dispatch_over_authenticated_socket` leaves search/notify unwired (`NOT_FOUND`).
    let unwired_ipc = dispatch_over_authenticated_socket(
        &auth_ep,
        client_binding.principal_id,
        |digest| Ok(client_key.sign(digest).to_bytes()),
        &structured_search,
        None,
        None,
        None,
    )
    .await
    .expect("authenticated IPC unwired InspectSearch");
    let unwired_err = unwired_ipc
        .outcome
        .expect_err("legacy socket helper must fail closed when unwired");
    assert_eq!(unwired_err.code, i32::from(SabiErrorCode::NotFound));

    stop.store(true, Ordering::Relaxed);
    server.await.expect("authenticated server retires cleanly");
    shutdown_daemon(daemon);
}

struct HealthyFixtureSource;

impl RecoveryHealthSource for HealthyFixtureSource {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        RecoveryWorkerHealth {
            state: RecoveryWorkerState::Running,
            completed_cycles: 1,
            ..RecoveryWorkerHealth::default()
        }
    }
}

#[test]
fn unwired_inspectors_fail_closed_with_not_found_and_preserve_legacy_signatures() {
    let root = TempRoot::new("unwired-fail-closed");
    fs::create_dir_all(root.path()).expect("create root");
    let tasks = nlos_task::SqliteTaskAuthority::open(root.path().join("tasks.sqlite3"))
        .expect("open task authority");
    let health = HealthyFixtureSource;
    let control = RecoverySystemControl::new(&tasks, &health, &ControlCapabilityPolicy);

    let search_cmd = ControlCommand::InspectSearch {
        scope_id: [0xAB; 32],
    };
    let legacy_search = dispatch_in_process(&control, &search_cmd, 50, 1_000, None, None, None)
        .expect("legacy dispatch_in_process search");
    let services_search = dispatch_in_process_with_services(
        &control,
        &search_cmd,
        50,
        1_000,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("dispatch_in_process_with_services unwired search");
    assert_eq!(legacy_search.to_bytes(), services_search.to_bytes());
    let search_failure = legacy_search
        .outcome
        .expect_err("unwired search inspector must fail closed");
    assert_eq!(search_failure.code, i32::from(SabiErrorCode::NotFound));
    assert_eq!(search_failure.retry, i32::from(RetryDirective::DoNotRetry));

    let request = build_request_envelope(&search_cmd).expect("build search envelope");
    let response = control.handle_for_ipc(&request, 50, 1_000);
    let composed_legacy = ControlReceipt::compose(&search_cmd, &response, None, None, None)
        .expect("legacy ControlReceipt::compose");
    assert_eq!(composed_legacy.to_bytes(), services_search.to_bytes());

    let notify_cmd = ControlCommand::InspectNotification {
        notification_id: [0xCD; 16],
    };
    let legacy_notify = dispatch_in_process(&control, &notify_cmd, 60, 1_100, None, None, None)
        .expect("legacy dispatch_in_process notify");
    let services_notify = dispatch_in_process_with_services(
        &control,
        &notify_cmd,
        60,
        1_100,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("dispatch_in_process_with_services unwired notify");
    assert_eq!(legacy_notify.to_bytes(), services_notify.to_bytes());
    let notify_failure = legacy_notify
        .outcome
        .expect_err("unwired notify inspector must fail closed");
    assert_eq!(notify_failure.code, i32::from(SabiErrorCode::NotFound));
    assert_eq!(notify_failure.retry, i32::from(RetryDirective::DoNotRetry));
}
