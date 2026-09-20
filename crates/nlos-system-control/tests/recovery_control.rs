#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
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
    AcknowledgeArtifactRecoveryAlertCommand, AcknowledgeResourceRecoveryAlertCommand,
    AcknowledgeSemanticRecoveryAlertCommand, CallerIdentity, CancelCommand, CapabilityHandle,
    ControlCommand, ControlCommandSource, ControlScope, Envelope, ExchangeRequest,
    ExchangeResponse, GetSystemControlRequest, KillCommand, LocalEndpoint, LocalTransportKind,
    NegotiateServiceRequest, PauseCommand, ReceiptReference, ReclaimCommand, ResumeCommand,
    ResumeResourceRecoveryCommand, ResumeSemanticRecoveryCommand, RetryDirective, SabiErrorCode,
    SabiFailure, SabiRequestContext, ServiceCandidate, ServiceVersion, SubmitControlCommandRequest,
    SystemControlView, ThrottleCommand, control_command, envelope, negotiate_service_response,
};
use nlos_schema::{
    MethodSemantics, SABI_ENVELOPE_SCHEMA, SABI_SYSTEM_CONTROL_SCHEMA,
    decode_artifact_recovery_operations_snapshot, decode_control_command_result,
    decode_resource_recovery_operations_snapshot, decode_semantic_recovery_operations_snapshot,
    encode_get_system_control_request, encode_submit_control_command_request,
    system_control_schema_identity, validate_sabi_response_context,
};
use nlos_service_directory::{ServiceRegistration, SnapshotDirectory};
use nlos_system_control::{
    GET_METHOD, OperationCommandExecutor, OperationControlRequest, RecoveryCounter, RecoveryGauge,
    RecoveryHealthSource, RecoveryMetricsSink, RecoverySystemControl, SUBMIT_METHOD,
    SYSTEM_CONTROL_SERVICE, SystemControlAuthorizer, resource_recovery_resume_reference,
};
use nlos_task::{
    ArtifactPublicationExpectation, ArtifactRecoveryFailureRequest, ArtifactRecoveryFailureSource,
    AttemptSpec, PermitDecision, PermitRequest, PlanArtifactCommitRequest, ResourceCommitPlanId,
    ResourceRecoveryState, SemanticCommitPlanId, SemanticRecoveryState, SnapshotBundle,
    SqliteTaskAuthority, TaskSpec, artifact_publication_plan_root, empty_effect_history_root,
    semantic_recovery_resume_reference,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId, TaskId,
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
            application_id: None,
            plan_revision: None,
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
                target_id: Vec::new(),
                plan_id: Vec::new(),
                target_generation: 0,
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
        semantic_durable_retrying: 0,
        semantic_durable_escalated: 0,
        semantic_durable_unacknowledged_escalated: 0,
        semantic_durable_resolved: 0,
        semantic_consecutive_failed_cycles: 0,
        semantic_total_inspected: 0,
        semantic_total_finalized: 0,
        semantic_domain_faulted: false,
        artifact_domain_faulted: false,
        resource_durable_retrying: 0,
        resource_durable_escalated: 0,
        resource_durable_unacknowledged_escalated: 0,
        resource_durable_resolved: 0,
        resource_consecutive_failed_cycles: 0,
        resource_total_inspected: 0,
        resource_total_finalized: 0,
        resource_domain_faulted: false,
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
        target_id: Vec::new(),
        plan_id: Vec::new(),
        target_generation: 0,
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
                        target_id: Vec::new(),
                        plan_id: Vec::new(),
                        target_generation: 0,
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
    assert_eq!(metrics.counters.len(), 7);
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
    assert!(
        metrics
            .counters
            .contains(&(RecoveryCounter::ResourcePlansInspected, 0))
    );
    assert!(
        metrics
            .gauges
            .contains(&(RecoveryGauge::ResourceDurableUnacknowledgedEscalated, 0))
    );
    assert_eq!(
        RecoveryGauge::ResourceDurableUnacknowledgedEscalated.name(),
        "nlos_resource_recovery_durable_unacknowledged_escalated"
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

const OPERATION_TARGET_ID: [u8; 16] = [0x81; 16];
const OPERATION_COMMAND_ID: [u8; 16] = [0x61; 16];
const OPERATION_CAS: u64 = 5;

fn operation_submit_envelope(arm: control_command::Command) -> Envelope {
    let submit = SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(ControlCommand {
            control_command_id: OPERATION_COMMAND_ID.to_vec(),
            issuer_principal_id: vec![0x31; 16],
            source: ControlCommandSource::Cli.into(),
            scope: ControlScope::Operation.into(),
            target_id: OPERATION_TARGET_ID.to_vec(),
            expected_generation_or_revision: OPERATION_CAS,
            command: Some(arm),
            reason: "operator pauses the escalated operation".to_owned(),
        }),
    };
    envelope(
        SUBMIT_METHOD,
        request_context(OPERATION_COMMAND_ID.to_vec()),
        encode_submit_control_command_request(&submit).unwrap(),
    )
}

/// One request served by [`RecordingOperationExecutor`].
struct ExecutedOperation {
    arm: &'static str,
    target_id: [u8; 16],
    expected_generation_or_revision: u64,
    issuer_principal_id: [u8; 16],
    idempotency_key: [u8; 16],
    requested_at_ms: i64,
    throttle_percent: Option<u64>,
}

/// Deterministic stub executor: records every request it serves and answers
/// with a receipt id whose first byte names the executed arm.
struct RecordingOperationExecutor {
    requests: std::sync::Mutex<Vec<ExecutedOperation>>,
}

impl RecordingOperationExecutor {
    fn receipt(arm_tag: u8) -> ReceiptId {
        let mut id = OPERATION_TARGET_ID;
        id[0] = arm_tag;
        ReceiptId::from_bytes(id)
    }

    fn record(&self, arm: &'static str, request: &OperationControlRequest) {
        self.record_with_percent(arm, request, None);
    }

    fn record_with_percent(
        &self,
        arm: &'static str,
        request: &OperationControlRequest,
        throttle_percent: Option<u64>,
    ) {
        self.requests.lock().unwrap().push(ExecutedOperation {
            arm,
            target_id: request.target_id,
            expected_generation_or_revision: request.expected_generation_or_revision,
            issuer_principal_id: request.issuer_principal_id,
            idempotency_key: request.idempotency_key,
            requested_at_ms: request.requested_at_ms,
            throttle_percent,
        });
    }
}

impl OperationCommandExecutor for RecordingOperationExecutor {
    fn pause_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        self.record("pause", &request);
        Ok(Self::receipt(1))
    }

    fn resume_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        self.record("resume", &request);
        Ok(Self::receipt(2))
    }

    fn cancel_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        self.record("cancel", &request);
        Err(SabiFailure {
            code: SabiErrorCode::Conflict.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: "stub executor rejects cancels".to_owned(),
        })
    }

    fn kill_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        self.record("kill", &request);
        Ok(Self::receipt(4))
    }

    fn throttle_operation(
        &self,
        request: OperationControlRequest,
        throttle_percent: u64,
    ) -> Result<ReceiptId, SabiFailure> {
        self.record_with_percent("throttle", &request, Some(throttle_percent));
        Ok(Self::receipt(5))
    }

    fn reclaim_operation(
        &self,
        request: OperationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        self.record("reclaim", &request);
        Ok(Self::receipt(6))
    }
}

#[test]
fn operation_commands_refuse_fail_closed_without_an_executor() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    let health = health(plan_id);
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy);
    for arm in [
        control_command::Command::PauseOperation(PauseCommand {}),
        control_command::Command::ResumeOperation(ResumeCommand {}),
        control_command::Command::CancelOperation(CancelCommand {}),
        control_command::Command::KillOperation(KillCommand {}),
        control_command::Command::ThrottleOperation(ThrottleCommand {
            throttle_percent: 50,
        }),
        control_command::Command::ReclaimOperation(ReclaimCommand {}),
    ] {
        let response = control.handle_for_ipc(&operation_submit_envelope(arm), 10, 6_000);
        let Some(envelope::CommonContext::ResponseContext(context)) =
            response.common_context.as_ref()
        else {
            panic!("expected response context");
        };
        let failure = context.failure.as_ref().unwrap();
        assert_eq!(failure.code, i32::from(SabiErrorCode::NotFound));
        assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
        assert_eq!(
            failure.safe_message,
            "operation control execution backend is not wired"
        );
        assert!(context.receipts.is_empty());
    }
}

#[test]
fn operation_commands_route_to_the_wired_executor_with_typed_receipts() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    let health = health(plan_id);
    let executor = RecordingOperationExecutor {
        requests: std::sync::Mutex::new(Vec::new()),
    };
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy)
        .with_operation_executor(&executor);

    let pause = control.handle_for_ipc(
        &operation_submit_envelope(control_command::Command::PauseOperation(PauseCommand {})),
        10,
        6_000,
    );
    let result = decode_control_command_result(&pause.payload).unwrap();
    assert_eq!(result.control_command_id, OPERATION_COMMAND_ID.to_vec());
    assert_eq!(
        result.receipt.unwrap().receipt_id,
        RecordingOperationExecutor::receipt(1).into_bytes().to_vec()
    );
    validate_sabi_response_context(&pause, MethodSemantics::MUTATION).unwrap();

    let resume = control.handle_for_ipc(
        &operation_submit_envelope(control_command::Command::ResumeOperation(ResumeCommand {})),
        10,
        6_000,
    );
    let result = decode_control_command_result(&resume.payload).unwrap();
    assert_eq!(
        result.receipt.unwrap().receipt_id,
        RecordingOperationExecutor::receipt(2).into_bytes().to_vec()
    );

    // An executor rejection crosses as the bounded failure it produced —
    // class, retry directive, and safe message forwarded verbatim, with no
    // receipt evidence.
    let cancel = control.handle_for_ipc(
        &operation_submit_envelope(control_command::Command::CancelOperation(CancelCommand {})),
        10,
        6_000,
    );
    let Some(envelope::CommonContext::ResponseContext(context)) = cancel.common_context.as_ref()
    else {
        panic!("expected response context");
    };
    let failure = context.failure.as_ref().unwrap();
    assert_eq!(failure.code, i32::from(SabiErrorCode::Conflict));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
    assert_eq!(failure.safe_message, "stub executor rejects cancels");
    assert!(cancel.payload.is_empty());
    assert!(context.receipts.is_empty());

    let requests = executor.requests.lock().unwrap();
    assert_eq!(
        requests.iter().map(|entry| entry.arm).collect::<Vec<_>>(),
        vec!["pause", "resume", "cancel"]
    );
    for entry in requests.iter() {
        assert_eq!(entry.target_id, OPERATION_TARGET_ID);
        assert_eq!(entry.expected_generation_or_revision, OPERATION_CAS);
        assert_eq!(entry.issuer_principal_id, [0x31; 16]);
        assert_eq!(entry.idempotency_key, OPERATION_COMMAND_ID);
        assert_eq!(entry.requested_at_ms, 6_000);
    }
}

/// W29-D arms through the same seam: the kill/throttle/reclaim wire
/// commands carry their typed receipt ids back, and throttle additionally
/// forwards the whole-percent level to the executor.
#[test]
fn w29d_operation_arms_route_to_the_wired_executor_with_typed_receipts() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    let health = health(plan_id);
    let executor = RecordingOperationExecutor {
        requests: std::sync::Mutex::new(Vec::new()),
    };
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy)
        .with_operation_executor(&executor);

    let kill = control.handle_for_ipc(
        &operation_submit_envelope(control_command::Command::KillOperation(KillCommand {})),
        10,
        6_000,
    );
    let result = decode_control_command_result(&kill.payload).unwrap();
    assert_eq!(
        result.receipt.unwrap().receipt_id,
        RecordingOperationExecutor::receipt(4).into_bytes().to_vec()
    );

    let throttle = control.handle_for_ipc(
        &operation_submit_envelope(control_command::Command::ThrottleOperation(
            ThrottleCommand {
                throttle_percent: 50,
            },
        )),
        10,
        6_000,
    );
    let result = decode_control_command_result(&throttle.payload).unwrap();
    assert_eq!(
        result.receipt.unwrap().receipt_id,
        RecordingOperationExecutor::receipt(5).into_bytes().to_vec()
    );

    let reclaim = control.handle_for_ipc(
        &operation_submit_envelope(control_command::Command::ReclaimOperation(
            ReclaimCommand {},
        )),
        10,
        6_000,
    );
    let result = decode_control_command_result(&reclaim.payload).unwrap();
    assert_eq!(
        result.receipt.unwrap().receipt_id,
        RecordingOperationExecutor::receipt(6).into_bytes().to_vec()
    );
    validate_sabi_response_context(&reclaim, MethodSemantics::MUTATION).unwrap();

    let requests = executor.requests.lock().unwrap();
    assert_eq!(
        requests.iter().map(|entry| entry.arm).collect::<Vec<_>>(),
        vec!["kill", "throttle", "reclaim"]
    );
    for entry in requests.iter() {
        assert_eq!(entry.target_id, OPERATION_TARGET_ID);
        assert_eq!(entry.expected_generation_or_revision, OPERATION_CAS);
        assert_eq!(entry.issuer_principal_id, [0x31; 16]);
        assert_eq!(entry.idempotency_key, OPERATION_COMMAND_ID);
        assert_eq!(entry.requested_at_ms, 6_000);
    }
    let throttle_entry = requests
        .iter()
        .find(|entry| entry.arm == "throttle")
        .unwrap();
    assert_eq!(throttle_entry.throttle_percent, Some(50));
    assert!(
        requests
            .iter()
            .filter(|entry| entry.arm != "throttle")
            .all(|entry| entry.throttle_percent.is_none())
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
        panic!("expected SystemControl binding");
    };
    assert_eq!(binding.candidate.unwrap().service, SYSTEM_CONTROL_SERVICE);
}

const SEMANTIC_PLAN_ID: [u8; 16] = [0x71; 16];
const SEMANTIC_ACK_COMMAND_ID: [u8; 16] = [0x53; 16];
const SEMANTIC_RESUME_COMMAND_ID: [u8; 16] = [0x54; 16];
const SEMANTIC_TOTAL_FAILURES: u64 = 8;

/// Seeds one escalated `task_semantic_recovery` ledger row straight into the
/// task database. The `Escalated` transition itself is W26-tested inside
/// `nlos-task`; this fixture only manufactures the durable operations-face
/// input the `SystemControl` handler reads. The recovery table's foreign key
/// to `task_semantic_commit_plans` is enforced per-connection, so a raw
/// seeding connection leaves it unchecked.
fn seed_escalated_semantic_recovery(database: &TestDatabase) -> SemanticCommitPlanId {
    let authority = database.open();
    let summary = authority.summarize_semantic_recovery().unwrap();
    assert_eq!(summary.escalated, 0, "fixture expects an empty ledger");
    drop(authority);

    let raw = rusqlite::Connection::open(&database.path).unwrap();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    raw.execute(
        "INSERT INTO task_semantic_recovery (
            plan_id, recovery_state, consecutive_failures, total_failures,
            last_failure_source, first_failed_at_ms, last_failed_at_ms,
            next_retry_at_ms, escalated_at_ms, resolved_at_ms, updated_at_ms
        ) VALUES (?1, 1, ?2, ?3, 1, 1000, 1400, NULL, 1500, NULL, 1500)",
        rusqlite::params![
            SEMANTIC_PLAN_ID.as_slice(),
            SEMANTIC_TOTAL_FAILURES.to_be_bytes().as_slice(),
            SEMANTIC_TOTAL_FAILURES.to_be_bytes().as_slice(),
        ],
    )
    .unwrap();
    SemanticCommitPlanId::from_bytes(SEMANTIC_PLAN_ID)
}

fn semantic_health(plan_id: nlos_task::ArtifactCommitPlanId) -> StubHealth {
    StubHealth(RecoveryWorkerHealth {
        state: RecoveryWorkerState::Running,
        completed_cycles: 21,
        total_inspected: 5,
        total_finalized: 4,
        consecutive_failed_cycles: 0,
        retry_delay: None,
        last_failures: vec![RecoveryWorkerFailure {
            plan_id: Some(plan_id),
            authority: WorkerFailureAuthority::Coordinator,
            message: "secret local database path must not cross IPC".to_owned(),
        }],
        durable_retrying: 0,
        durable_escalated: 0,
        durable_unacknowledged_escalated: 0,
        durable_resolved: 0,
        semantic_durable_retrying: 2,
        semantic_durable_escalated: 3,
        semantic_durable_unacknowledged_escalated: 3,
        semantic_durable_resolved: 7,
        semantic_consecutive_failed_cycles: 4,
        semantic_total_inspected: 13,
        semantic_total_finalized: 6,
        semantic_domain_faulted: false,
        artifact_domain_faulted: false,
        resource_durable_retrying: 0,
        resource_durable_escalated: 0,
        resource_durable_unacknowledged_escalated: 0,
        resource_durable_resolved: 0,
        resource_consecutive_failed_cycles: 0,
        resource_total_inspected: 0,
        resource_total_finalized: 0,
        resource_domain_faulted: false,
    })
}

fn semantic_get_envelope(alert_limit: u32) -> Envelope {
    envelope(
        GET_METHOD,
        request_context(Vec::new()),
        encode_get_system_control_request(&GetSystemControlRequest {
            schema: Some(system_control_schema_identity()),
            view: SystemControlView::SemanticCommitRecovery.into(),
            alert_limit,
            target_id: Vec::new(),
            plan_id: Vec::new(),
            target_generation: 0,
        })
        .unwrap(),
    )
}

fn semantic_submit_envelope(
    command_id: [u8; 16],
    command: control_command::Command,
    expected_total_failures: u64,
) -> Envelope {
    let submit = SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(ControlCommand {
            control_command_id: command_id.to_vec(),
            issuer_principal_id: vec![0x31; 16],
            source: ControlCommandSource::Cli.into(),
            scope: ControlScope::Operation.into(),
            target_id: SEMANTIC_PLAN_ID.to_vec(),
            expected_generation_or_revision: expected_total_failures,
            command: Some(command),
            reason: "operator inspected durable semantic recovery state".to_owned(),
        }),
    };
    envelope(
        SUBMIT_METHOD,
        request_context(command_id.to_vec()),
        encode_submit_control_command_request(&submit).unwrap(),
    )
}

#[test]
fn semantic_get_routes_by_view_and_reports_authoritative_ledger_facts() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    seed_escalated_semantic_recovery(&database);
    let health = semantic_health(plan_id);
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy);

    let response = control
        .handle(&semantic_get_envelope(8), 10, 6_000)
        .unwrap();
    validate_sabi_response_context(&response, MethodSemantics::QUERY).unwrap();
    let snapshot = decode_semantic_recovery_operations_snapshot(&response.payload).unwrap();
    let metrics = snapshot.metrics.as_ref().unwrap();
    // The durable gauges must come from the live semantic ledger, not the
    // deliberately different worker cache in `semantic_health`.
    assert_eq!(metrics.durable_retrying, 0);
    assert_eq!(metrics.durable_escalated, 1);
    assert_eq!(metrics.durable_unacknowledged_escalated, 1);
    assert_eq!(metrics.durable_resolved, 0);
    assert_eq!(metrics.total_inspected, 13);
    assert_eq!(metrics.total_finalized, 6);
    assert_eq!(metrics.consecutive_failed_cycles, 4);
    assert!(!metrics.domain_faulted);
    assert_eq!(snapshot.alerts.len(), 1);
    assert_eq!(snapshot.alerts[0].plan_id, SEMANTIC_PLAN_ID);
    assert_eq!(snapshot.alerts[0].total_failures, SEMANTIC_TOTAL_FAILURES);
    assert_eq!(
        snapshot.alerts[0].last_failure_authority,
        i32::from(nlos_schema::sabi::v1::RecoveryFailureAuthority::Semantic)
    );
    assert_eq!(snapshot.alerts[0].escalated_at_ms, 1_500);
    assert_eq!(snapshot.alerts[0].acknowledgement_receipt, None);
    assert!(!snapshot.alerts_truncated);
    assert!(
        !response
            .payload
            .windows(6)
            .any(|window| window == b"secret")
    );

    let artifact = control
        .handle(
            &envelope(
                GET_METHOD,
                request_context(Vec::new()),
                encode_get_system_control_request(&GetSystemControlRequest {
                    schema: Some(system_control_schema_identity()),
                    view: SystemControlView::ArtifactCommitRecovery.into(),
                    alert_limit: 8,
                    target_id: Vec::new(),
                    plan_id: Vec::new(),
                    target_generation: 0,
                })
                .unwrap(),
            ),
            10,
            6_000,
        )
        .unwrap();
    let artifact_snapshot =
        decode_artifact_recovery_operations_snapshot(&artifact.payload).unwrap();
    assert_eq!(artifact_snapshot.alerts.len(), 1);
    assert_eq!(artifact_snapshot.alerts[0].plan_id, plan_id.as_bytes());
}

#[test]
fn semantic_acknowledge_replays_idempotently_with_typed_cas_failures() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    seed_escalated_semantic_recovery(&database);
    let health = semantic_health(plan_id);
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy);

    let stale_cas = control
        .handle(
            &semantic_submit_envelope(
                SEMANTIC_ACK_COMMAND_ID,
                control_command::Command::AcknowledgeSemanticRecoveryAlert(
                    AcknowledgeSemanticRecoveryAlertCommand {},
                ),
                SEMANTIC_TOTAL_FAILURES + 1,
            ),
            10,
            6_000,
        )
        .unwrap_err();
    let failure = stale_cas.to_sabi_failure();
    assert_eq!(failure.code, i32::from(SabiErrorCode::Conflict));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));

    let acknowledged = control
        .handle(
            &semantic_submit_envelope(
                SEMANTIC_ACK_COMMAND_ID,
                control_command::Command::AcknowledgeSemanticRecoveryAlert(
                    AcknowledgeSemanticRecoveryAlertCommand {},
                ),
                SEMANTIC_TOTAL_FAILURES,
            ),
            10,
            6_000,
        )
        .unwrap();
    validate_sabi_response_context(&acknowledged, MethodSemantics::MUTATION).unwrap();
    let ack_result = decode_control_command_result(&acknowledged.payload).unwrap();
    let ack_receipt = ack_result.receipt.unwrap();
    assert_eq!(ack_receipt.receipt_id.len(), 16);

    let replay = control
        .handle(
            &semantic_submit_envelope(
                SEMANTIC_ACK_COMMAND_ID,
                control_command::Command::AcknowledgeSemanticRecoveryAlert(
                    AcknowledgeSemanticRecoveryAlertCommand {},
                ),
                SEMANTIC_TOTAL_FAILURES,
            ),
            10,
            7_000,
        )
        .unwrap();
    assert_eq!(
        decode_control_command_result(&replay.payload)
            .unwrap()
            .receipt,
        Some(ack_receipt.clone())
    );
    let alerts = authority.list_semantic_recovery_alerts().unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(
        alerts[0].acknowledgement.map(|receipt| receipt.receipt_id),
        Some(ReceiptId::from_bytes(
            ack_receipt.receipt_id.clone().try_into().unwrap()
        ))
    );
}

#[test]
fn semantic_resume_requeues_the_escalated_ledger_with_typed_replay_failure() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    seed_escalated_semantic_recovery(&database);
    let semantic_plan = SemanticCommitPlanId::from_bytes(SEMANTIC_PLAN_ID);
    let health = semantic_health(plan_id);
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy);

    let resumed = control
        .handle(
            &semantic_submit_envelope(
                SEMANTIC_RESUME_COMMAND_ID,
                control_command::Command::ResumeSemanticRecovery(ResumeSemanticRecoveryCommand {}),
                SEMANTIC_TOTAL_FAILURES,
            ),
            10,
            6_000,
        )
        .unwrap();
    validate_sabi_response_context(&resumed, MethodSemantics::MUTATION).unwrap();
    let resume_result = decode_control_command_result(&resumed.payload).unwrap();
    let resume_reference = resume_result.receipt.unwrap();
    assert_eq!(
        resume_reference.receipt_id,
        semantic_recovery_resume_reference(semantic_plan, SEMANTIC_TOTAL_FAILURES)
            .as_bytes()
            .to_vec()
    );
    let record = authority
        .inspect_semantic_recovery(semantic_plan)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, SemanticRecoveryState::Retrying);
    assert_eq!(record.total_failures, SEMANTIC_TOTAL_FAILURES);
    assert_eq!(record.consecutive_failures, 0);
    assert_eq!(record.next_retry_at_ms, Some(6_000));
    assert_eq!(record.escalated_at_ms, None);

    let replayed_resume = control
        .handle(
            &semantic_submit_envelope(
                SEMANTIC_RESUME_COMMAND_ID,
                control_command::Command::ResumeSemanticRecovery(ResumeSemanticRecoveryCommand {}),
                SEMANTIC_TOTAL_FAILURES,
            ),
            10,
            7_000,
        )
        .unwrap_err();
    let replay_failure = replayed_resume.to_sabi_failure();
    assert_eq!(replay_failure.code, i32::from(SabiErrorCode::State));
    assert_eq!(replay_failure.retry, i32::from(RetryDirective::DoNotRetry));
}

#[tokio::test]
async fn semantic_escalated_plan_is_acknowledged_and_resumed_over_real_ipc() {
    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(&authority);
    seed_escalated_semantic_recovery(&database);
    let semantic_plan = SemanticCommitPlanId::from_bytes(SEMANTIC_PLAN_ID);

    let acknowledge_request = ExchangeRequest {
        envelope: Some(semantic_submit_envelope(
            SEMANTIC_ACK_COMMAND_ID,
            control_command::Command::AcknowledgeSemanticRecoveryAlert(
                AcknowledgeSemanticRecoveryAlertCommand {},
            ),
            SEMANTIC_TOTAL_FAILURES,
        )),
    };
    let config = transport_config();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let server_authority = Arc::clone(&authority);
    let server_health = semantic_health(plan_id);
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
        .exchange_validated(acknowledge_request)
        .await
        .unwrap();
    server.await.unwrap().unwrap();

    let response_envelope = response.envelope();
    validate_sabi_response_context(response_envelope, MethodSemantics::MUTATION).unwrap();
    let result = decode_control_command_result(&response_envelope.payload).unwrap();
    let ack_receipt = result.receipt.unwrap();
    assert_eq!(ack_receipt.receipt_id.len(), 16);
    assert_eq!(
        response_envelope
            .common_context
            .as_ref()
            .and_then(|context| match context {
                envelope::CommonContext::ResponseContext(context) => context.receipts.first(),
                envelope::CommonContext::RequestContext(_) => None,
            }),
        Some(&ack_receipt)
    );

    let resume_request = ExchangeRequest {
        envelope: Some(semantic_submit_envelope(
            SEMANTIC_RESUME_COMMAND_ID,
            control_command::Command::ResumeSemanticRecovery(ResumeSemanticRecoveryCommand {}),
            SEMANTIC_TOTAL_FAILURES,
        )),
    };
    let resume_health = semantic_health(plan_id);
    let resume = RecoverySystemControl::new(authority.as_ref(), &resume_health, &CapabilityPolicy)
        .handle(resume_request.envelope.as_ref().unwrap(), 10, 6_000)
        .unwrap();
    validate_sabi_response_context(&resume, MethodSemantics::MUTATION).unwrap();
    let resume_result = decode_control_command_result(&resume.payload).unwrap();
    assert_eq!(
        resume_result.receipt.unwrap().receipt_id,
        semantic_recovery_resume_reference(semantic_plan, SEMANTIC_TOTAL_FAILURES)
            .as_bytes()
            .to_vec()
    );
    assert_eq!(
        authority
            .inspect_semantic_recovery(semantic_plan)
            .unwrap()
            .unwrap()
            .state,
        SemanticRecoveryState::Retrying
    );
}

const RESOURCE_PLAN_ID: [u8; 16] = [0x81; 16];
const RESOURCE_ACK_COMMAND_ID: [u8; 16] = [0x57; 16];
const RESOURCE_RESUME_COMMAND_ID: [u8; 16] = [0x58; 16];
const RESOURCE_TOTAL_FAILURES: u64 = 8;

/// Seeds one escalated `task_resource_recovery` ledger row straight into the
/// task database, mirroring the semantic fixture: the `Escalated`
/// transition itself is W28-C-tested inside `nlos-task`; this fixture only
/// manufactures the durable operations-face input the `SystemControl`
/// handler reads. The recovery table's foreign key to
/// `task_resource_commit_plans` is enforced per-connection, so a raw
/// seeding connection leaves it unchecked.
fn seed_escalated_resource_recovery(database: &TestDatabase) -> ResourceCommitPlanId {
    let authority = database.open();
    let summary = authority.summarize_resource_recovery().unwrap();
    assert_eq!(summary.escalated, 0, "fixture expects an empty ledger");
    drop(authority);

    let raw = rusqlite::Connection::open(&database.path).unwrap();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    raw.execute(
        "INSERT INTO task_resource_recovery (
            plan_id, recovery_state, consecutive_failures, total_failures,
            last_failure_source, first_failed_at_ms, last_failed_at_ms,
            next_retry_at_ms, escalated_at_ms, resolved_at_ms, updated_at_ms
        ) VALUES (?1, 1, ?2, ?3, 1, 1000, 1400, NULL, 1500, NULL, 1500)",
        rusqlite::params![
            RESOURCE_PLAN_ID.as_slice(),
            RESOURCE_TOTAL_FAILURES.to_be_bytes().as_slice(),
            RESOURCE_TOTAL_FAILURES.to_be_bytes().as_slice(),
        ],
    )
    .unwrap();
    ResourceCommitPlanId::from_bytes(RESOURCE_PLAN_ID)
}

fn resource_health(plan_id: nlos_task::ArtifactCommitPlanId) -> StubHealth {
    StubHealth(RecoveryWorkerHealth {
        state: RecoveryWorkerState::Running,
        completed_cycles: 21,
        total_inspected: 5,
        total_finalized: 4,
        consecutive_failed_cycles: 0,
        retry_delay: None,
        last_failures: vec![RecoveryWorkerFailure {
            plan_id: Some(plan_id),
            authority: WorkerFailureAuthority::Coordinator,
            message: "secret local database path must not cross IPC".to_owned(),
        }],
        durable_retrying: 0,
        durable_escalated: 0,
        durable_unacknowledged_escalated: 0,
        durable_resolved: 0,
        semantic_durable_retrying: 0,
        semantic_durable_escalated: 0,
        semantic_durable_unacknowledged_escalated: 0,
        semantic_durable_resolved: 0,
        semantic_consecutive_failed_cycles: 0,
        semantic_total_inspected: 0,
        semantic_total_finalized: 0,
        semantic_domain_faulted: false,
        artifact_domain_faulted: false,
        resource_durable_retrying: 2,
        resource_durable_escalated: 3,
        resource_durable_unacknowledged_escalated: 3,
        resource_durable_resolved: 7,
        resource_consecutive_failed_cycles: 4,
        resource_total_inspected: 15,
        resource_total_finalized: 7,
        resource_domain_faulted: false,
    })
}

fn resource_get_envelope(alert_limit: u32) -> Envelope {
    envelope(
        GET_METHOD,
        request_context(Vec::new()),
        encode_get_system_control_request(&GetSystemControlRequest {
            schema: Some(system_control_schema_identity()),
            view: SystemControlView::ResourceCommitRecovery.into(),
            alert_limit,
            target_id: Vec::new(),
            plan_id: Vec::new(),
            target_generation: 0,
        })
        .unwrap(),
    )
}

fn resource_submit_envelope(
    command_id: [u8; 16],
    command: control_command::Command,
    expected_total_failures: u64,
) -> Envelope {
    let submit = SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(ControlCommand {
            control_command_id: command_id.to_vec(),
            issuer_principal_id: vec![0x31; 16],
            source: ControlCommandSource::Cli.into(),
            scope: ControlScope::Operation.into(),
            target_id: RESOURCE_PLAN_ID.to_vec(),
            expected_generation_or_revision: expected_total_failures,
            command: Some(command),
            reason: "operator inspected durable resource recovery state".to_owned(),
        }),
    };
    envelope(
        SUBMIT_METHOD,
        request_context(command_id.to_vec()),
        encode_submit_control_command_request(&submit).unwrap(),
    )
}

#[test]
fn resource_get_routes_by_view_and_reports_authoritative_ledger_facts() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    seed_escalated_resource_recovery(&database);
    let health = resource_health(plan_id);
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy);

    let response = control
        .handle(&resource_get_envelope(8), 10, 6_000)
        .unwrap();
    validate_sabi_response_context(&response, MethodSemantics::QUERY).unwrap();
    let snapshot = decode_resource_recovery_operations_snapshot(&response.payload).unwrap();
    let metrics = snapshot.metrics.as_ref().unwrap();
    // The durable gauges must come from the live resource ledger, not the
    // deliberately different worker cache in `resource_health`.
    assert_eq!(metrics.durable_retrying, 0);
    assert_eq!(metrics.durable_escalated, 1);
    assert_eq!(metrics.durable_unacknowledged_escalated, 1);
    assert_eq!(metrics.durable_resolved, 0);
    assert_eq!(metrics.total_inspected, 15);
    assert_eq!(metrics.total_finalized, 7);
    assert_eq!(metrics.consecutive_failed_cycles, 4);
    assert!(!metrics.domain_faulted);
    assert_eq!(snapshot.alerts.len(), 1);
    assert_eq!(snapshot.alerts[0].plan_id, RESOURCE_PLAN_ID);
    assert_eq!(snapshot.alerts[0].total_failures, RESOURCE_TOTAL_FAILURES);
    assert_eq!(
        snapshot.alerts[0].last_failure_authority,
        i32::from(nlos_schema::sabi::v1::RecoveryFailureAuthority::Resource)
    );
    assert_eq!(snapshot.alerts[0].escalated_at_ms, 1_500);
    assert_eq!(snapshot.alerts[0].acknowledgement_receipt, None);
    assert!(!snapshot.alerts_truncated);
    assert!(
        !response
            .payload
            .windows(6)
            .any(|window| window == b"secret")
    );

    let semantic = control
        .handle(&semantic_get_envelope(8), 10, 6_000)
        .unwrap();
    let semantic_snapshot = decode_semantic_recovery_operations_snapshot(&semantic.payload)
        .expect("resource view must not leak into the semantic view");
    assert!(semantic_snapshot.alerts.is_empty());
}

#[test]
fn resource_acknowledge_replays_idempotently_with_typed_cas_failures() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    seed_escalated_resource_recovery(&database);
    let health = resource_health(plan_id);
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy);

    let stale_cas = control
        .handle(
            &resource_submit_envelope(
                RESOURCE_ACK_COMMAND_ID,
                control_command::Command::AcknowledgeResourceRecoveryAlert(
                    AcknowledgeResourceRecoveryAlertCommand {},
                ),
                RESOURCE_TOTAL_FAILURES + 1,
            ),
            10,
            6_000,
        )
        .unwrap_err();
    let failure = stale_cas.to_sabi_failure();
    assert_eq!(failure.code, i32::from(SabiErrorCode::Conflict));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));

    let acknowledged = control
        .handle(
            &resource_submit_envelope(
                RESOURCE_ACK_COMMAND_ID,
                control_command::Command::AcknowledgeResourceRecoveryAlert(
                    AcknowledgeResourceRecoveryAlertCommand {},
                ),
                RESOURCE_TOTAL_FAILURES,
            ),
            10,
            6_000,
        )
        .unwrap();
    validate_sabi_response_context(&acknowledged, MethodSemantics::MUTATION).unwrap();
    let ack_result = decode_control_command_result(&acknowledged.payload).unwrap();
    let ack_receipt = ack_result.receipt.unwrap();
    assert_eq!(ack_receipt.receipt_id.len(), 16);

    let replay = control
        .handle(
            &resource_submit_envelope(
                RESOURCE_ACK_COMMAND_ID,
                control_command::Command::AcknowledgeResourceRecoveryAlert(
                    AcknowledgeResourceRecoveryAlertCommand {},
                ),
                RESOURCE_TOTAL_FAILURES,
            ),
            10,
            7_000,
        )
        .unwrap();
    assert_eq!(
        decode_control_command_result(&replay.payload)
            .unwrap()
            .receipt,
        Some(ack_receipt.clone())
    );
    let alerts = authority.list_resource_recovery_alerts().unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(
        alerts[0].acknowledgement.map(|receipt| receipt.receipt_id),
        Some(ReceiptId::from_bytes(
            ack_receipt.receipt_id.clone().try_into().unwrap()
        ))
    );
}

#[test]
fn resource_resume_requeues_the_escalated_ledger_with_typed_replay_failure() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    seed_escalated_resource_recovery(&database);
    let resource_plan = ResourceCommitPlanId::from_bytes(RESOURCE_PLAN_ID);
    let health = resource_health(plan_id);
    let control = RecoverySystemControl::new(&authority, &health, &CapabilityPolicy);

    let resumed = control
        .handle(
            &resource_submit_envelope(
                RESOURCE_RESUME_COMMAND_ID,
                control_command::Command::ResumeResourceRecovery(ResumeResourceRecoveryCommand {}),
                RESOURCE_TOTAL_FAILURES,
            ),
            10,
            6_000,
        )
        .unwrap();
    validate_sabi_response_context(&resumed, MethodSemantics::MUTATION).unwrap();
    let resume_result = decode_control_command_result(&resumed.payload).unwrap();
    let resume_reference = resume_result.receipt.unwrap();
    assert_eq!(
        resume_reference.receipt_id,
        resource_recovery_resume_reference(resource_plan, RESOURCE_TOTAL_FAILURES)
            .as_bytes()
            .to_vec()
    );
    let record = authority
        .inspect_resource_recovery(resource_plan)
        .unwrap()
        .unwrap();
    assert_eq!(record.state, ResourceRecoveryState::Retrying);
    assert_eq!(record.total_failures, RESOURCE_TOTAL_FAILURES);
    assert_eq!(record.consecutive_failures, 0);
    assert_eq!(record.next_retry_at_ms, Some(6_000));
    assert_eq!(record.escalated_at_ms, None);

    let replayed_resume = control
        .handle(
            &resource_submit_envelope(
                RESOURCE_RESUME_COMMAND_ID,
                control_command::Command::ResumeResourceRecovery(ResumeResourceRecoveryCommand {}),
                RESOURCE_TOTAL_FAILURES,
            ),
            10,
            7_000,
        )
        .unwrap_err();
    let replay_failure = replayed_resume.to_sabi_failure();
    assert_eq!(replay_failure.code, i32::from(SabiErrorCode::State));
    assert_eq!(replay_failure.retry, i32::from(RetryDirective::DoNotRetry));
}

#[tokio::test]
async fn resource_escalated_plan_is_acknowledged_and_resumed_over_real_ipc() {
    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(&authority);
    seed_escalated_resource_recovery(&database);
    let resource_plan = ResourceCommitPlanId::from_bytes(RESOURCE_PLAN_ID);

    let acknowledge_request = ExchangeRequest {
        envelope: Some(resource_submit_envelope(
            RESOURCE_ACK_COMMAND_ID,
            control_command::Command::AcknowledgeResourceRecoveryAlert(
                AcknowledgeResourceRecoveryAlertCommand {},
            ),
            RESOURCE_TOTAL_FAILURES,
        )),
    };
    let config = transport_config();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let server_authority = Arc::clone(&authority);
    let server_health = resource_health(plan_id);
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
        .exchange_validated(acknowledge_request)
        .await
        .unwrap();
    server.await.unwrap().unwrap();

    let response_envelope = response.envelope();
    validate_sabi_response_context(response_envelope, MethodSemantics::MUTATION).unwrap();
    let result = decode_control_command_result(&response_envelope.payload).unwrap();
    let ack_receipt = result.receipt.unwrap();
    assert_eq!(ack_receipt.receipt_id.len(), 16);
    assert_eq!(
        response_envelope
            .common_context
            .as_ref()
            .and_then(|context| match context {
                envelope::CommonContext::ResponseContext(context) => context.receipts.first(),
                envelope::CommonContext::RequestContext(_) => None,
            }),
        Some(&ack_receipt)
    );

    let resume_request = ExchangeRequest {
        envelope: Some(resource_submit_envelope(
            RESOURCE_RESUME_COMMAND_ID,
            control_command::Command::ResumeResourceRecovery(ResumeResourceRecoveryCommand {}),
            RESOURCE_TOTAL_FAILURES,
        )),
    };
    let resume_health = resource_health(plan_id);
    let resume = RecoverySystemControl::new(authority.as_ref(), &resume_health, &CapabilityPolicy)
        .handle(resume_request.envelope.as_ref().unwrap(), 10, 6_000)
        .unwrap();
    validate_sabi_response_context(&resume, MethodSemantics::MUTATION).unwrap();
    let resume_result = decode_control_command_result(&resume.payload).unwrap();
    assert_eq!(
        resume_result.receipt.unwrap().receipt_id,
        resource_recovery_resume_reference(resource_plan, RESOURCE_TOTAL_FAILURES)
            .as_bytes()
            .to_vec()
    );
    assert_eq!(
        authority
            .inspect_resource_recovery(resource_plan)
            .unwrap()
            .unwrap()
            .state,
        ResourceRecoveryState::Retrying
    );
}

// W32-G (B5-3): per-layer inspect views — unwired fail-closed defaults,
// wired stub receipts, and the real TaskAuthority-backed TaskGroup view.

struct StubLayerSources {
    node: Option<nlos_system_control::control::TaskNodeInspection>,
    fiber: Option<nlos_system_control::control::ExecutionFiberInspection>,
    topic: Option<nlos_system_control::control::TopicInspection>,
    operation: Option<nlos_system_control::control::DurableOperationInspection>,
}

impl nlos_system_control::TaskNodeInspectSource for StubLayerSources {
    fn inspect_task_node(
        &self,
        plan_id: [u8; 16],
        node_id: [u8; 16],
    ) -> Result<nlos_system_control::control::TaskNodeInspection, SabiFailure> {
        match &self.node {
            Some(inspection) if inspection.plan_id == plan_id && inspection.node_id == node_id => {
                Ok(inspection.clone())
            }
            _ => Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "requested task node was not found".to_owned(),
            }),
        }
    }
}

impl nlos_system_control::ExecutionFiberInspectSource for StubLayerSources {
    fn inspect_execution_fiber(
        &self,
        fiber_id: [u8; 16],
        generation: u64,
    ) -> Result<nlos_system_control::control::ExecutionFiberInspection, SabiFailure> {
        match &self.fiber {
            Some(inspection)
                if inspection.fiber_id == fiber_id && inspection.generation == generation =>
            {
                Ok(inspection.clone())
            }
            _ => Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "requested execution fiber handle was not found".to_owned(),
            }),
        }
    }
}

impl nlos_system_control::TopicInspectSource for StubLayerSources {
    fn inspect_topic(
        &self,
        topic_id: [u8; 16],
    ) -> Result<nlos_system_control::control::TopicInspection, SabiFailure> {
        match &self.topic {
            Some(inspection) if inspection.topic_id == topic_id => Ok(inspection.clone()),
            _ => Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "requested topic was not found".to_owned(),
            }),
        }
    }
}

impl nlos_system_control::OperationInspectSource for StubLayerSources {
    fn inspect_operation(
        &self,
        operation_id: [u8; 16],
        generation: u64,
    ) -> Result<nlos_system_control::control::DurableOperationInspection, SabiFailure> {
        match &self.operation {
            Some(inspection)
                if inspection.operation_id == operation_id
                    && inspection.generation == generation =>
            {
                Ok(inspection.clone())
            }
            _ => Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "requested operation row was not found".to_owned(),
            }),
        }
    }
}

fn stub_layer_sources() -> StubLayerSources {
    use nlos_schema::sabi::v1::{
        ContextResidencyTier, DurableOperationState, ExecutionFiberLifecycleState,
        ExecutionFiberPhase, PlanNodeKind, PlanNodeLifecycleState,
    };
    StubLayerSources {
        node: Some(nlos_system_control::control::TaskNodeInspection {
            plan_id: [0xA1; 16],
            node_id: [0xA2; 16],
            kind: PlanNodeKind::Executable,
            state: PlanNodeLifecycleState::Eligible,
            declared_revision: 4,
            node_digest: vec![0xA3; 32],
            transition_count: 2,
            residency_tier: ContextResidencyTier::MetadataOnly,
            residency_transition_count: 0,
            first_declared_at_ms: 2_000,
            updated_at_ms: 2_400,
        }),
        fiber: Some(nlos_system_control::control::ExecutionFiberInspection {
            fiber_id: [0xB1; 16],
            generation: 2,
            state: ExecutionFiberLifecycleState::Running,
            lifecycle_phase: ExecutionFiberPhase::WaitingExternal,
            active_cpu_ms: 11,
            elapsed_wall_ms: 40,
            scheduler_wait_ms: 3,
            external_wait_ms: 20,
            backpressure_wait_ms: 1,
            suspended_ms: 0,
        }),
        topic: Some(nlos_system_control::control::TopicInspection {
            topic_id: [0xC1; 16],
            channel_id: [0xC2; 16],
            channel_generation: 5,
            name: b"stage-b/inspect".to_vec(),
            active_subscriptions: 2,
            policy_digest: vec![0xC3; 32],
            created_at_ms: 3_000,
        }),
        operation: Some(nlos_system_control::control::DurableOperationInspection {
            operation_id: [0xD1; 16],
            generation: 1,
            state: DurableOperationState::Dispatched,
            cancel_epoch: 0,
            owner_fiber_id: [0xB1; 16],
            owner_fiber_generation: 2,
            outcome_receipt_id: None,
        }),
    }
}

fn layer_get_envelope(
    view: SystemControlView,
    target_id: Vec<u8>,
    plan_id: Vec<u8>,
    target_generation: u64,
) -> Envelope {
    let mut request = request_context(Vec::new());
    request.correlation_id = vec![0x44; 16];
    let mut envelope = envelope(GET_METHOD, request, Vec::new());
    envelope.payload = encode_get_system_control_request(&GetSystemControlRequest {
        schema: Some(system_control_schema_identity()),
        view: view.into(),
        alert_limit: 8,
        target_id,
        plan_id,
        target_generation,
    })
    .unwrap();
    envelope
}

fn w32g_group_fixture(authority: &SqliteTaskAuthority) -> nlos_types::TaskGroupId {
    use nlos_task::{
        AttemptSpec, CompletionMode, FailureMode, GroupBinding, GroupSpec, SnapshotBundle, TaskSpec,
    };
    let task_id = TaskId::from_bytes([0x11; 16]);
    authority
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
            application_id: None,
            plan_revision: None,
        })
        .unwrap();
    let group_id = nlos_types::TaskGroupId::from_bytes([0x91; 16]);
    authority
        .register_group(GroupSpec {
            group_id,
            task_id,
            task_generation: Generation::INITIAL,
            parent_group_id: None,
            group_policy_digest: [0x91; 32],
            completion_mode: CompletionMode::All,
            failure_mode: FailureMode::CollectAll,
            max_children: 4,
            max_depth: 1,
            resource_group_id: None,
            resource_account_digest: None,
            cancellation_scope_id: CancellationScopeId::from_bytes([0x92; 16]),
            registered_at_ms: 1_500,
        })
        .unwrap();
    let record = authority.inspect_group(group_id).unwrap();
    let binding = GroupBinding {
        group_id,
        expected_membership_generation: record.membership_generation,
        expected_membership_root: record.membership_root,
        expected_group_policy_digest: record.group_policy_digest,
    };
    authority
        .register_attempt_in_group(
            AttemptSpec {
                task_id,
                attempt_id: TaskAttemptId::from_bytes([0x93; 16]),
                attempt_generation: Generation::INITIAL,
                snapshot: SnapshotBundle {
                    snapshot_id: TaskSnapshotId::from_bytes([0x94; 16]),
                    snapshot_digest: [0x95; 32],
                    expected_head_commit_seq: 0,
                    effect_history_root: empty_effect_history_root(),
                    retry_fence_epoch: 0,
                },
                cancellation_scope_id: CancellationScopeId::from_bytes([0x96; 16]),
                cancellation_generation: Generation::INITIAL,
                idempotency_key: IdempotencyKey::from_bytes([0x97; 16]),
                registered_at_ms: 2_000,
            },
            binding,
        )
        .unwrap();
    group_id
}

#[test]
fn w32g_layer_views_refuse_fail_closed_without_sources() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    let stub_health = health(plan_id);
    let control = RecoverySystemControl::new(&authority, &stub_health, &CapabilityPolicy);
    for (view, target_id, plan, generation) in [
        (
            SystemControlView::TaskNode,
            vec![0xA2; 16],
            vec![0xA1; 16],
            0_u64,
        ),
        (
            SystemControlView::ExecutionFiber,
            vec![0xB1; 16],
            Vec::new(),
            2_u64,
        ),
        (SystemControlView::Topic, vec![0xC1; 16], Vec::new(), 0_u64),
        (
            SystemControlView::Operation,
            vec![0xD1; 16],
            Vec::new(),
            1_u64,
        ),
    ] {
        let response = control.handle_for_ipc(
            &layer_get_envelope(view, target_id, plan, generation),
            10,
            6_000,
        );
        let Some(envelope::CommonContext::ResponseContext(context)) =
            response.common_context.as_ref()
        else {
            panic!("expected response context");
        };
        let failure = context.failure.as_ref().unwrap();
        assert_eq!(failure.code, i32::from(SabiErrorCode::NotFound));
        assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
        assert_eq!(
            failure.safe_message,
            "layer inspection backend is not wired"
        );
        assert!(context.receipts.is_empty());
    }
}

#[test]
fn w32g_layer_views_route_to_wired_sources_with_typed_snapshots() {
    use nlos_system_control::control::{ControlCommand, ControlOutcome, dispatch_in_process};
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    let sources = stub_layer_sources();
    let stub_health = health(plan_id);
    let control = RecoverySystemControl::new(&authority, &stub_health, &CapabilityPolicy)
        .with_task_node_source(&sources)
        .with_execution_fiber_source(&sources)
        .with_topic_source(&sources)
        .with_operation_source(&sources);

    let node_receipt = dispatch_in_process(
        &control,
        &ControlCommand::InspectTaskNode {
            plan_id: [0xA1; 16],
            node_id: [0xA2; 16],
        },
        10,
        6_000,
        None,
        None,
    )
    .unwrap();
    let ControlOutcome::TaskNodeInspected(node) = node_receipt.outcome.as_ref().unwrap() else {
        panic!("expected task node inspection receipt");
    };
    assert_eq!(node.plan_id, [0xA1; 16]);
    assert_eq!(node.declared_revision, 4);
    assert_eq!(node.node_digest, vec![0xA3; 32]);

    let fiber_receipt = dispatch_in_process(
        &control,
        &ControlCommand::InspectExecutionFiber {
            fiber_id: [0xB1; 16],
            generation: 2,
        },
        10,
        6_000,
        None,
        None,
    )
    .unwrap();
    let ControlOutcome::ExecutionFiberInspected(fiber) = fiber_receipt.outcome.as_ref().unwrap()
    else {
        panic!("expected execution fiber inspection receipt");
    };
    assert_eq!(fiber.fiber_id, [0xB1; 16]);
    assert_eq!(fiber.elapsed_wall_ms, 40);

    let topic_receipt = dispatch_in_process(
        &control,
        &ControlCommand::InspectTopic {
            topic_id: [0xC1; 16],
        },
        10,
        6_000,
        None,
        None,
    )
    .unwrap();
    let ControlOutcome::TopicInspected(topic) = topic_receipt.outcome.as_ref().unwrap() else {
        panic!("expected topic inspection receipt");
    };
    assert_eq!(topic.name, b"stage-b/inspect".to_vec());
    assert_eq!(topic.active_subscriptions, 2);

    let operation_receipt = dispatch_in_process(
        &control,
        &ControlCommand::InspectOperation {
            operation_id: [0xD1; 16],
            generation: 1,
        },
        10,
        6_000,
        None,
        None,
    )
    .unwrap();
    let ControlOutcome::DurableOperationInspected(operation) =
        operation_receipt.outcome.as_ref().unwrap()
    else {
        panic!("expected durable operation inspection receipt");
    };
    assert_eq!(operation.owner_fiber_id, [0xB1; 16]);
    assert!(operation.outcome_receipt_id.is_none());

    let missing = dispatch_in_process(
        &control,
        &ControlCommand::InspectTopic {
            topic_id: [0xEE; 16],
        },
        10,
        6_000,
        None,
        None,
    )
    .unwrap();
    let Err(failure) = missing.outcome.as_ref() else {
        panic!("expected typed failure for a missing topic");
    };
    assert_eq!(failure.code, i32::from(SabiErrorCode::NotFound));
    assert_eq!(failure.safe_message, "requested topic was not found");
}

#[test]
fn w32g_task_group_view_reads_the_real_task_authority() {
    use nlos_schema::sabi::v1::TaskGroupLifecycleState;
    use nlos_system_control::control::{ControlCommand, ControlOutcome, dispatch_in_process};
    let database = TestDatabase::new();
    let authority = database.open();
    let group_id = w32g_group_fixture(&authority);
    let plan_id = create_escalated_plan(&authority);
    let stub_health = health(plan_id);
    let control = RecoverySystemControl::new(&authority, &stub_health, &CapabilityPolicy);

    let receipt = dispatch_in_process(
        &control,
        &ControlCommand::InspectTaskGroup {
            group_id: *group_id.as_bytes(),
        },
        10,
        6_000,
        None,
        None,
    )
    .unwrap();
    let ControlOutcome::TaskGroupInspected(group) = receipt.outcome.as_ref().unwrap() else {
        panic!("expected task group inspection receipt");
    };
    assert_eq!(group.group_id, *group_id.as_bytes());
    assert_eq!(group.task_id, [0x11; 16]);
    assert_eq!(group.parent_group_id, None);
    assert_eq!(group.state, TaskGroupLifecycleState::Open);
    assert_eq!(group.members.len(), 1);
    assert_eq!(group.members[0].member_id, [0x93; 16]);
    assert_eq!(group.members[0].admission_receipt_id.len(), 16);
    assert!(!group.members_truncated);

    let missing = dispatch_in_process(
        &control,
        &ControlCommand::InspectTaskGroup {
            group_id: [0xEE; 16],
        },
        10,
        6_000,
        None,
        None,
    )
    .unwrap();
    let Err(failure) = missing.outcome.as_ref() else {
        panic!("expected typed failure for a missing group");
    };
    assert_eq!(failure.code, i32::from(SabiErrorCode::NotFound));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
}
