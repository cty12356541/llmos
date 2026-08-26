#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_ipc::windows::{NamedPipeListenerAdapter, connect};
use nlos_ipc::{
    ExactPeerAuthorizer, IpcError, LocalRpcClient, OutboundResponse, PeerCredentialBinding,
    PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::sabi::v1::{
    AcknowledgeArtifactRecoveryAlertCommand, CallerIdentity, CapabilityHandle, ControlCommand,
    ControlCommandSource, ControlScope, Envelope, ExchangeRequest, ExchangeResponse,
    SabiRequestContext, SubmitControlCommandRequest, control_command, envelope,
};
use nlos_schema::{
    MethodSemantics, SABI_ENVELOPE_SCHEMA, decode_control_command_result,
    encode_submit_control_command_request, system_control_schema_identity,
    validate_sabi_response_context,
};
use nlos_system_control::{
    RecoveryHealthSource, RecoverySystemControl, SUBMIT_METHOD, SYSTEM_CONTROL_SERVICE,
    SystemControlAuthorizer,
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

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);
static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-system-control-windows-{}-{sequence}.sqlite3",
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
        _: &nlos_schema::sabi::v1::GetSystemControlRequest,
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

fn health() -> StubHealth {
    StubHealth(RecoveryWorkerHealth {
        state: RecoveryWorkerState::BackingOff,
        ..RecoveryWorkerHealth::default()
    })
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

fn transport_config() -> TransportConfig {
    TransportConfig::new(
        64 * 1024,
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap()
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

fn submit_request(
    plan_id: nlos_task::ArtifactCommitPlanId,
    command_id: Vec<u8>,
) -> ExchangeRequest {
    let submit = SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(ControlCommand {
            control_command_id: command_id.clone(),
            issuer_principal_id: vec![0x31; 16],
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
    ExchangeRequest {
        envelope: Some(envelope(
            SUBMIT_METHOD,
            request_context(command_id),
            encode_submit_control_command_request(&submit).unwrap(),
        )),
    }
}

#[tokio::test]
async fn submit_crosses_real_windows_named_pipe_and_replays_receipt() {
    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(&authority);
    let command_id = vec![0x41; 16];
    let request = submit_request(plan_id, command_id);
    let config = transport_config();
    let pipe_sequence = NEXT_PIPE.fetch_add(1, Ordering::Relaxed);
    let pipe_name = format!(
        r"\\.\pipe\nlos-system-control-recovery-{}-{pipe_sequence}",
        std::process::id()
    );
    let mut listener = NamedPipeListenerAdapter::bind(&pipe_name, 2, config).unwrap();
    let server_authority = Arc::clone(&authority);
    let server = tokio::spawn(async move {
        let peer_binding =
            PeerCredentialBinding::from_peer(PeerIdentity::WindowsNamedPipe { process_id: None });
        let peer_authorizer = ExactPeerAuthorizer::new(peer_binding);
        let server_health = health();
        let policy = CapabilityPolicy;
        for _ in 0..2 {
            let (stream, peer) = listener.accept(config).await?;
            assert_eq!(peer, peer_binding.identity());
            serve_one(stream, config, peer, &peer_authorizer, |validated| {
                let response =
                    RecoverySystemControl::new(server_authority.as_ref(), &server_health, &policy)
                        .handle_for_ipc(validated.envelope(), 10, 6_000);
                async move {
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            })
            .await?;
        }
        Ok::<(), IpcError>(())
    });

    let first = {
        let (stream, peer) = connect(&pipe_name, config).await.unwrap();
        assert_eq!(peer, PeerIdentity::WindowsNamedPipe { process_id: None });
        LocalRpcClient::new(stream, config)
            .exchange_validated(request.clone())
            .await
            .unwrap()
    };
    validate_sabi_response_context(first.envelope(), MethodSemantics::MUTATION).unwrap();
    let first_result = decode_control_command_result(&first.envelope().payload).unwrap();
    let first_receipt = first_result
        .receipt
        .clone()
        .expect("receipt over named pipe");
    assert_eq!(
        first_result.control_command_id,
        request
            .envelope
            .as_ref()
            .unwrap()
            .common_context
            .as_ref()
            .and_then(|context| match context {
                envelope::CommonContext::RequestContext(context) =>
                    Some(context.idempotency_key.clone()),
                envelope::CommonContext::ResponseContext(_) => None,
            })
            .unwrap()
    );
    assert_eq!(
        authority
            .list_artifact_recovery_alerts(8)
            .unwrap()
            .first()
            .and_then(|alert| alert.acknowledgement)
            .map(|receipt| receipt.receipt_id.into_bytes().to_vec()),
        Some(first_receipt.receipt_id.clone()),
    );

    let replay = {
        let (stream, peer) = connect(&pipe_name, config).await.unwrap();
        assert_eq!(peer, PeerIdentity::WindowsNamedPipe { process_id: None });
        LocalRpcClient::new(stream, config)
            .exchange_validated(request)
            .await
            .unwrap()
    };
    validate_sabi_response_context(replay.envelope(), MethodSemantics::MUTATION).unwrap();
    let replay_result = decode_control_command_result(&replay.envelope().payload).unwrap();
    assert_eq!(replay_result.receipt, Some(first_receipt));
    assert_eq!(replay.envelope().payload, first.envelope().payload);

    server.await.unwrap().unwrap();
}
