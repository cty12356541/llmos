use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nlos_commit_coordinator::{
    RecoveryFailureAuthority as WorkerFailureAuthority, RecoveryWorkerFailure,
    RecoveryWorkerHealth, RecoveryWorkerState,
};
use nlos_ipc::{
    LocalRpcClient, OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::sabi::v1::{
    AcknowledgeArtifactRecoveryAlertCommand, CallerIdentity, CapabilityHandle, ControlCommand,
    ControlCommandSource, ControlScope, Envelope, ExchangeRequest, ExchangeResponse,
    GetSystemControlRequest, LocalEndpoint, LocalTransportKind, NegotiateServiceRequest,
    ReceiptReference, RetryDirective, SabiErrorCode, SabiRequestContext, ServiceCandidate,
    ServiceVersion, SubmitControlCommandRequest, SystemControlView, control_command, envelope,
    negotiate_service_response,
};
use nlos_schema::{
    MethodSemantics, SABI_ENVELOPE_SCHEMA, SABI_SYSTEM_CONTROL_SCHEMA,
    decode_artifact_recovery_operations_snapshot, decode_control_command_result,
    encode_get_system_control_request, encode_submit_control_command_request,
    system_control_schema_identity, validate_sabi_response_context,
};
use nlos_service_directory::{ServiceRegistration, SnapshotDirectory};
use nlos_system_control::{
    GET_METHOD, RecoveryCounter, RecoveryGauge, RecoveryHealthSource, RecoveryMetricsSink,
    RecoverySystemControl, SUBMIT_METHOD, SYSTEM_CONTROL_SERVICE, SystemControlAuthorizer,
};
use nlos_task::{
    ArtifactPublicationExpectation, ArtifactRecoveryFailureRequest, ArtifactRecoveryFailureSource,
    AttemptSpec, PermitDecision, PermitRequest, PlanArtifactCommitRequest, SnapshotBundle,
    SqliteTaskAuthority, TaskSpec, artifact_publication_plan_root, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use tokio::io::duplex;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-system-control-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).unwrap()
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

struct CapabilityPolicy;

impl SystemControlAuthorizer for CapabilityPolicy {
    fn authorize_get(
        &self,
        context: &SabiRequestContext,
        _: &GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        authorize(context)
    }

    fn authorize_submit(
        &self,
        context: &SabiRequestContext,
        _: &ControlCommand,
    ) -> Result<(), &'static str> {
        authorize(context)
    }
}

fn authorize(context: &SabiRequestContext) -> Result<(), &'static str> {
    if context.capability_handles
        == [CapabilityHandle {
            slot: 9,
            generation: 1,
        }]
    {
        Ok(())
    } else {
        Err("missing recovery operations capability")
    }
}

#[derive(Clone)]
struct StubHealth(RecoveryWorkerHealth);

impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.0.clone()
    }
}

#[derive(Default)]
struct RecordingMetrics {
    state: Option<RecoveryWorkerState>,
    counters: Vec<(RecoveryCounter, u64)>,
    gauges: Vec<(RecoveryGauge, u64)>,
}

impl RecoveryMetricsSink for RecordingMetrics {
    type Error = std::convert::Infallible;

    fn record_worker_state(&mut self, state: RecoveryWorkerState) -> Result<(), Self::Error> {
        self.state = Some(state);
        Ok(())
    }

    fn set_counter_total(
        &mut self,
        counter: RecoveryCounter,
        value: u64,
    ) -> Result<(), Self::Error> {
        self.counters.push((counter, value));
        Ok(())
    }

    fn set_gauge(&mut self, gauge: RecoveryGauge, value: u64) -> Result<(), Self::Error> {
        self.gauges.push((gauge, value));
        Ok(())
    }
}

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

fn create_escalated_plan(authority: &SqliteTaskAuthority) -> nlos_task::ArtifactCommitPlanId {
    let task_id = TaskId::from_bytes([0x11; 16]);
    authority
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([0x12; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x13; 16]),
            snapshot_digest: [0x14; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x15; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x16; 16]),
        registered_at_ms: 2_000,
    };
    authority.register_attempt(attempt).unwrap();
    let expectation = ArtifactPublicationExpectation {
        staging_id: [0x21; 16],
        artifact_id: ArtifactId::from_bytes([0x22; 16]),
        target_revision: 1,
        digest: [0x23; 32],
        size_bytes: 10,
    };
    let PermitDecision::Issued(permit) = authority
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            write_set_root: artifact_publication_plan_root(std::slice::from_ref(&expectation))
                .unwrap(),
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x17; 16]),
            valid_until_ms: 20_000,
            requested_at_ms: 3_000,
        })
        .unwrap()
    else {
        panic!("expected permit");
    };
    let plan = authority
        .plan_artifact_commit(PlanArtifactCommitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x18; 16]),
            expectations: vec![expectation],
            planned_at_ms: 4_000,
        })
        .unwrap()
        .record()
        .clone();
    authority
        .record_artifact_recovery_failure(ArtifactRecoveryFailureRequest {
            plan_id: plan.plan_id,
            expected_total_failures: 0,
            source: ArtifactRecoveryFailureSource::ArtifactAuthority,
            observed_at_ms: 5_000,
            base_delay_ms: 100,
            max_delay_ms: 1_000,
            escalation_threshold: 1,
        })
        .unwrap();
    plan.plan_id
}

fn request_context(idempotency_key: Vec<u8>) -> SabiRequestContext {
    SabiRequestContext {
        caller: Some(CallerIdentity {
            principal_id: vec![0x31; 16],
            application_id: vec![0x32; 16],
            process_id: vec![0x33; 16],
            process_generation: 1,
        }),
        activity_context: Vec::new(),
        task_execution_binding: None,
        correlation_id: vec![0x34; 16],
        idempotency_key,
        deadline_monotonic_ns: 0,
        capability_handles: vec![CapabilityHandle {
            slot: 9,
            generation: 1,
        }],
        reservation_handle: None,
        proposal_or_input_digest_sha256: Vec::new(),
    }
}

fn envelope(method: &str, context: SabiRequestContext, payload: Vec<u8>) -> Envelope {
    Envelope {
        schema: Some(nlos_schema::sabi::v1::SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x35; 16],
        service: SYSTEM_CONTROL_SERVICE.to_owned(),
        method: method.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(context)),
        payload,
    }
}

#[cfg(unix)]
fn get_exchange_request() -> ExchangeRequest {
    ExchangeRequest {
        envelope: Some(envelope(
            GET_METHOD,
            request_context(Vec::new()),
            encode_get_system_control_request(&GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::ArtifactCommitRecovery.into(),
                alert_limit: 8,
            })
            .unwrap(),
        )),
    }
}

fn transport_config() -> TransportConfig {
    TransportConfig::new(
        64 * 1024,
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .unwrap()
}

fn health(plan_id: nlos_task::ArtifactCommitPlanId) -> StubHealth {
    StubHealth(RecoveryWorkerHealth {
        state: RecoveryWorkerState::BackingOff,
        completed_cycles: 4,
        total_inspected: 3,
        total_finalized: 2,
        consecutive_failed_cycles: 0,
        retry_delay: Some(Duration::from_millis(250)),
        last_failures: vec![RecoveryWorkerFailure {
            plan_id: Some(plan_id),
            authority: WorkerFailureAuthority::Artifact,
            message: "secret local database path must not cross IPC".to_owned(),
        }],
        durable_retrying: 0,
        durable_escalated: 1,
        durable_unacknowledged_escalated: 1,
        durable_resolved: 0,
    })
}

fn submit_envelope(
    plan_id: nlos_task::ArtifactCommitPlanId,
    issuer_principal_id: Vec<u8>,
    command_id: Vec<u8>,
    idempotency_key: Vec<u8>,
) -> Envelope {
    let submit = SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(ControlCommand {
            control_command_id: command_id,
            issuer_principal_id,
            source: ControlCommandSource::Cli.into(),
            scope: ControlScope::Operation.into(),
            target_id: plan_id.as_bytes().to_vec(),
            expected_generation_or_revision: 1,
            command: Some(control_command::Command::AcknowledgeArtifactRecoveryAlert(
                AcknowledgeArtifactRecoveryAlertCommand {},
            )),
            reason: "inspected recovery evidence".to_owned(),
        }),
    };
    envelope(
        SUBMIT_METHOD,
        request_context(idempotency_key),
        encode_submit_control_command_request(&submit).unwrap(),
    )
}

#[test]
fn get_returns_bounded_typed_health_without_local_diagnostics() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    let health = health(plan_id);
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy);
    let payload = encode_get_system_control_request(&GetSystemControlRequest {
        schema: Some(system_control_schema_identity()),
        view: SystemControlView::ArtifactCommitRecovery.into(),
        alert_limit: 8,
    })
    .unwrap();
    let response = control
        .handle(
            &envelope(GET_METHOD, request_context(Vec::new()), payload),
            10,
            6_000,
        )
        .unwrap();
    let snapshot = decode_artifact_recovery_operations_snapshot(&response.payload).unwrap();
    assert_eq!(snapshot.alerts.len(), 1);
    assert_eq!(snapshot.alerts[0].plan_id, plan_id.as_bytes());
    assert_eq!(snapshot.metrics.as_ref().unwrap().last_failures.len(), 1);
    assert!(
        !response
            .payload
            .windows(6)
            .any(|window| window == b"secret")
    );
    validate_sabi_response_context(&response, MethodSemantics::QUERY).unwrap();

    let mut denied = request_context(Vec::new());
    denied.capability_handles.clear();
    assert!(
        control
            .handle(
                &envelope(
                    GET_METHOD,
                    denied,
                    encode_get_system_control_request(&GetSystemControlRequest {
                        schema: Some(system_control_schema_identity()),
                        view: SystemControlView::ArtifactCommitRecovery.into(),
                        alert_limit: 8,
                    },)
                    .unwrap()
                ),
                10,
                6_000,
            )
            .is_err()
    );
}

#[test]
fn metrics_export_uses_stable_catalog_and_live_task_authority_gauges() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    let mut stale_health = health(plan_id);
    stale_health.0.durable_escalated = 99;
    stale_health.0.durable_unacknowledged_escalated = 99;
    let control = RecoverySystemControl::new(&authority, &stale_health, &CapabilityPolicy);
    let mut metrics = RecordingMetrics::default();
    control.export_metrics(&mut metrics).unwrap();

    assert_eq!(metrics.state, Some(RecoveryWorkerState::BackingOff));
    assert_eq!(metrics.counters.len(), 3);
    assert!(
        metrics
            .counters
            .contains(&(RecoveryCounter::CompletedCycles, 4))
    );
    assert!(
        metrics
            .gauges
            .contains(&(RecoveryGauge::DurableEscalated, 1))
    );
    assert!(
        metrics
            .gauges
            .contains(&(RecoveryGauge::DurableUnacknowledgedEscalated, 1))
    );
    assert_eq!(
        RecoveryGauge::DurableUnacknowledgedEscalated.name(),
        "nlos_artifact_recovery_durable_unacknowledged_escalated"
    );
}

#[tokio::test]
async fn submit_crosses_real_ipc_and_replays_the_task_authority_receipt() {
    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(&authority);
    let command_id = vec![0x41; 16];
    let request = ExchangeRequest {
        envelope: Some(submit_envelope(
            plan_id,
            vec![0x31; 16],
            command_id.clone(),
            command_id,
        )),
    };
    let config = transport_config();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let server_authority = Arc::clone(&authority);
    let server_health = health(plan_id);
    let server = tokio::spawn(async move {
        serve_one(
            server_stream,
            config,
            PeerIdentity::InMemory,
            &AllowPeer,
            move |validated| {
                let response = RecoverySystemControl::new(
                    server_authority.as_ref(),
                    &server_health,
                    &CapabilityPolicy,
                )
                .handle_for_ipc(validated.envelope(), 10, 6_000);
                async move {
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await
    });
    let response = LocalRpcClient::new(client_stream, config)
        .exchange_validated(request.clone())
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    let response_envelope = response.envelope();
    validate_sabi_response_context(response_envelope, MethodSemantics::MUTATION).unwrap();
    let result = decode_control_command_result(&response_envelope.payload).unwrap();
    let receipt = result.receipt.unwrap();
    assert_eq!(
        response_envelope
            .common_context
            .as_ref()
            .and_then(|context| match context {
                envelope::CommonContext::ResponseContext(context) => context.receipts.first(),
                envelope::CommonContext::RequestContext(_) => None,
            }),
        Some(&ReceiptReference {
            receipt_id: receipt.receipt_id.clone(),
        })
    );
    assert_eq!(
        authority
            .inspect_artifact_recovery(plan_id)
            .unwrap()
            .unwrap()
            .state,
        nlos_task::ArtifactRecoveryState::Escalated
    );

    let replay_health = health(plan_id);
    let replay = RecoverySystemControl::new(authority.as_ref(), &replay_health, &CapabilityPolicy)
        .handle(request.envelope.as_ref().unwrap(), 10, 7_000)
        .unwrap();
    assert_eq!(
        decode_control_command_result(&replay.payload)
            .unwrap()
            .receipt,
        Some(receipt)
    );
}

#[tokio::test]
async fn denied_submit_crosses_real_ipc_as_bounded_failure() {
    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(&authority);
    let mut denied_envelope =
        submit_envelope(plan_id, vec![0x31; 16], vec![0x41; 16], vec![0x41; 16]);
    let envelope::CommonContext::RequestContext(context) = denied_envelope
        .common_context
        .as_mut()
        .expect("request context")
    else {
        panic!("expected request context");
    };
    context.capability_handles.clear();
    let request = ExchangeRequest {
        envelope: Some(denied_envelope),
    };
    let config = transport_config();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let server_authority = Arc::clone(&authority);
    let server_health = health(plan_id);
    let server = tokio::spawn(async move {
        serve_one(
            server_stream,
            config,
            PeerIdentity::InMemory,
            &AllowPeer,
            move |validated| {
                let response = RecoverySystemControl::new(
                    server_authority.as_ref(),
                    &server_health,
                    &CapabilityPolicy,
                )
                .handle_for_ipc(validated.envelope(), 10, 6_000);
                async move {
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await
    });
    let response = LocalRpcClient::new(client_stream, config)
        .exchange_validated(request)
        .await
        .unwrap();
    server.await.unwrap().unwrap();

    let response_envelope = response.envelope();
    validate_sabi_response_context(response_envelope, MethodSemantics::MUTATION).unwrap();
    assert!(response_envelope.payload.is_empty());
    let envelope::CommonContext::ResponseContext(context) = response_envelope
        .common_context
        .as_ref()
        .expect("response context")
    else {
        panic!("expected response context");
    };
    let failure = context.failure.as_ref().expect("typed rejection");
    assert_eq!(failure.code, i32::from(SabiErrorCode::Rights));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
    assert!(context.operation.is_none());
    assert!(context.receipts.is_empty());
    assert_eq!(
        authority
            .list_artifact_recovery_alerts(8)
            .unwrap()
            .first()
            .unwrap()
            .acknowledgement,
        None
    );
}

#[cfg(unix)]
#[tokio::test]
async fn get_uses_a_service_directory_resolved_unix_endpoint() {
    use nlos_ipc::unix::{UnixListenerAdapter, connect};

    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(&authority);
    let socket_path = database.path.with_extension("sock");
    let listener = UnixListenerAdapter::bind(&socket_path).unwrap();
    let directory = SnapshotDirectory::new([ServiceRegistration {
        candidate: ServiceCandidate {
            binding_id: vec![0x61; 16],
            generation: 1,
            service: SYSTEM_CONTROL_SERVICE.to_owned(),
            version: Some(ServiceVersion {
                schema_name: SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
                major: 1,
                minor: 0,
            }),
            feature_ids: Vec::new(),
            transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
        },
        endpoint: LocalEndpoint {
            kind: LocalTransportKind::UnixSocket.into(),
            address: socket_path.to_string_lossy().into_owned(),
        },
    }])
    .unwrap();
    let negotiation = directory.negotiate(&NegotiateServiceRequest {
        schema: Some(nlos_schema::service_directory_schema_identity()),
        service: SYSTEM_CONTROL_SERVICE.to_owned(),
        schema_name: SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minimum_minor: 0,
        required_feature_ids: Vec::new(),
        supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
    });
    let negotiate_service_response::Result::Binding(binding) = negotiation.result.unwrap() else {
        panic!("expected binding")
    };
    let endpoint = binding.endpoint.unwrap().address;
    let server_authority = Arc::clone(&authority);
    let server_health = health(plan_id);
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept(transport_config()).await?;
        serve_one(
            stream,
            transport_config(),
            peer,
            &AllowPeer,
            move |validated| {
                let response = RecoverySystemControl::new(
                    server_authority.as_ref(),
                    &server_health,
                    &CapabilityPolicy,
                )
                .handle_for_ipc(validated.envelope(), 10, 6_000);
                async move {
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await
    });
    let (stream, peer) = connect(endpoint, transport_config()).await.unwrap();
    assert!(matches!(peer, PeerIdentity::Unix { .. }));
    let response = LocalRpcClient::new(stream, transport_config())
        .exchange_validated(get_exchange_request())
        .await
        .unwrap();
    assert_eq!(
        decode_artifact_recovery_operations_snapshot(&response.envelope().payload)
            .unwrap()
            .alerts
            .len(),
        1
    );
    server.await.unwrap().unwrap();
    fs::remove_file(socket_path).unwrap();
}

#[test]
fn submit_rejects_forged_issuer_and_mismatched_command_key_without_receipt() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    let health = health(plan_id);
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy);
    assert!(
        control
            .handle(
                &submit_envelope(plan_id, vec![0x99; 16], vec![0x41; 16], vec![0x41; 16],),
                10,
                6_000,
            )
            .is_err()
    );
    assert!(
        control
            .handle(
                &submit_envelope(plan_id, vec![0x31; 16], vec![0x41; 16], vec![0x42; 16],),
                10,
                6_000,
            )
            .is_err()
    );
    assert_eq!(
        authority
            .list_artifact_recovery_alerts(8)
            .unwrap()
            .first()
            .unwrap()
            .acknowledgement,
        None
    );
}

#[test]
fn service_directory_negotiates_the_system_control_contract() {
    let directory = SnapshotDirectory::new([ServiceRegistration {
        candidate: ServiceCandidate {
            binding_id: vec![0x51; 16],
            generation: 1,
            service: SYSTEM_CONTROL_SERVICE.to_owned(),
            version: Some(ServiceVersion {
                schema_name: SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
                major: 1,
                minor: 0,
            }),
            feature_ids: Vec::new(),
            transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
        },
        endpoint: LocalEndpoint {
            kind: LocalTransportKind::UnixSocket.into(),
            address: "/tmp/nlos-system-control.sock".to_owned(),
        },
    }])
    .unwrap();
    let response = directory.negotiate(&NegotiateServiceRequest {
        schema: Some(nlos_schema::service_directory_schema_identity()),
        service: SYSTEM_CONTROL_SERVICE.to_owned(),
        schema_name: SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minimum_minor: 0,
        required_feature_ids: Vec::new(),
        supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
    });
    let negotiate_service_response::Result::Binding(binding) = response.result.unwrap() else {
        panic!("expected SystemControl binding")
    };
    assert_eq!(binding.candidate.unwrap().service, SYSTEM_CONTROL_SERVICE);
}
