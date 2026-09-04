#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! B-CONTROL-001 integration evidence: the in-process dispatcher and the
//! `system-control-cli` binary both cross the same real `SystemControl`
//! handler path — the CLI and the library client over a real Unix socket —
//! and produce byte-identical typed [`ControlReceipt`]s.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_schema::sabi::v1::{
    ControlCommand as WireControlCommand, GetSystemControlRequest, SabiErrorCode,
    SabiRequestContext,
};
use nlos_system_control::control::{
    ControlCommand, ControlOutcome, RecoveryWorkerLifecycle, dispatch_in_process, parse_hex_id,
};
use nlos_system_control::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactPublicationExpectation, ArtifactRecoveryFailureRequest,
    ArtifactRecoveryFailureSource, AttemptSpec, PermitDecision, PermitRequest,
    PlanArtifactCommitRequest, SnapshotBundle, SqliteTaskAuthority, artifact_publication_plan_root,
    empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

const MONOTONIC_NOW_NS: u64 = 10;
const WALL_NOW_MS: i64 = 6_000;
const ACK_COMMAND_ID: [u8; 16] = [0x41; 16];
const DENIED_COMMAND_ID: [u8; 16] = [0x42; 16];
const ACK_REASON: &str = "inspected recovery evidence";
const DENIED_REASON: &str = "denied: exercising the policy denial path";

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-system-control-cli-{}-{sequence}.sqlite3",
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

/// Test policy: capability handle `{slot: 9, generation: 1}` authorizes;
/// acknowledgements whose reason is prefixed `denied` exercise the typed
/// policy rejection. The denial reason itself never crosses the boundary —
/// the response carries only the bounded `RIGHTS` class.
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
        command: &WireControlCommand,
    ) -> Result<(), &'static str> {
        authorize(context)?;
        if command.reason.starts_with("denied") {
            Err("policy denied this acknowledgement")
        } else {
            Ok(())
        }
    }
}

fn authorize(context: &SabiRequestContext) -> Result<(), &'static str> {
    let expected = nlos_schema::sabi::v1::CapabilityHandle {
        slot: nlos_system_control::control::CONTROL_CAPABILITY_SLOT,
        generation: nlos_system_control::control::CONTROL_CAPABILITY_GENERATION,
    };
    if context.capability_handles.as_slice() == [expected] {
        Ok(())
    } else {
        Err("missing recovery operations capability")
    }
}

#[derive(Clone)]
struct StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth);

impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> nlos_commit_coordinator::RecoveryWorkerHealth {
        self.0.clone()
    }
}

fn health(plan_id: &ArtifactCommitPlanId) -> StubHealth {
    StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth {
        state: nlos_commit_coordinator::RecoveryWorkerState::BackingOff,
        completed_cycles: 4,
        total_inspected: 3,
        total_finalized: 2,
        consecutive_failed_cycles: 0,
        retry_delay: Some(std::time::Duration::from_millis(250)),
        last_failures: vec![nlos_commit_coordinator::RecoveryWorkerFailure {
            plan_id: Some(*plan_id),
            authority: nlos_commit_coordinator::RecoveryFailureAuthority::Artifact,
            message: "secret local database path must not cross IPC".to_owned(),
        }],
        durable_retrying: 0,
        durable_escalated: 1,
        durable_unacknowledged_escalated: 1,
        durable_resolved: 0,
    })
}

// Platform-neutral stub authorizer; only the Unix socket harness constructs
// it, so silence the dead-code warning on Windows instead of gating the type.
#[cfg_attr(not(unix), allow(dead_code))]
struct AllowPeer;

#[cfg_attr(not(unix), allow(dead_code))]
impl nlos_ipc::PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &nlos_ipc::PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

fn create_escalated_plan(authority: &SqliteTaskAuthority) -> ArtifactCommitPlanId {
    let task_id = TaskId::from_bytes([0x11; 16]);
    authority
        .register_task(nlos_task::TaskSpec {
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

fn acknowledge_command(plan_id: &ArtifactCommitPlanId) -> ControlCommand {
    ControlCommand::AcknowledgeRecoveryAlert {
        control_command_id: ACK_COMMAND_ID,
        plan_id: *plan_id.as_bytes(),
        expected_total_failures: 1,
        reason: ACK_REASON.to_owned(),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[test]
fn hex_ids_roundtrip_through_the_cli_parser() {
    assert_eq!(parse_hex_id(&hex(&ACK_COMMAND_ID)).unwrap(), ACK_COMMAND_ID);
}

#[test]
fn in_process_dispatch_produces_typed_receipts() {
    let database = TestDatabase::new();
    let authority = database.open();
    let plan_id = create_escalated_plan(&authority);
    let stub_health = health(&plan_id);
    let control = RecoverySystemControl::new(&authority, &stub_health, &CapabilityPolicy);

    let health_receipt = dispatch_in_process(
        &control,
        &ControlCommand::InspectHealth,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
    )
    .unwrap();
    let ControlOutcome::Inspected(inspection) = health_receipt.outcome.as_ref().unwrap() else {
        panic!("expected inspection receipt");
    };
    assert_eq!(inspection.worker_state, RecoveryWorkerLifecycle::BackingOff);
    assert_eq!(inspection.completed_cycles, 4);
    assert_eq!(inspection.durable_escalated, 1);
    assert_eq!(inspection.alerts.len(), 1);
    assert_eq!(inspection.alerts[0].plan_id, plan_id.as_bytes());
    assert_eq!(health_receipt.control_command_id, [0xC0; 16]);

    let task_receipt = dispatch_in_process(
        &control,
        &ControlCommand::InspectTask {
            plan_id: *plan_id.as_bytes(),
        },
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
    )
    .unwrap();
    let ControlOutcome::Inspected(task_inspection) = task_receipt.outcome.as_ref().unwrap() else {
        panic!("expected scoped inspection receipt");
    };
    assert_eq!(task_inspection.alerts.len(), 1);
    assert_eq!(task_receipt.control_command_id, *plan_id.as_bytes());

    let missing = dispatch_in_process(
        &control,
        &ControlCommand::InspectTask {
            plan_id: [0xEE; 16],
        },
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
    )
    .unwrap();
    let Err(failure) = missing.outcome.as_ref() else {
        panic!("expected typed failure for a missing target");
    };
    assert_eq!(failure.code, i32::from(SabiErrorCode::NotFound));

    let acknowledgement = dispatch_in_process(
        &control,
        &acknowledge_command(&plan_id),
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
    )
    .unwrap();
    let ControlOutcome::Acknowledged { receipt_id } = acknowledgement.outcome.as_ref().unwrap()
    else {
        panic!("expected acknowledgement receipt");
    };
    assert_eq!(receipt_id.len(), 16);
    let replay = dispatch_in_process(
        &control,
        &acknowledge_command(&plan_id),
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
    )
    .unwrap();
    assert_eq!(acknowledgement.to_bytes(), replay.to_bytes());
    assert!(
        authority
            .list_artifact_recovery_alerts(8)
            .unwrap()
            .first()
            .unwrap()
            .acknowledgement
            .is_some()
    );

    let denied = dispatch_in_process(
        &control,
        &ControlCommand::AcknowledgeRecoveryAlert {
            control_command_id: DENIED_COMMAND_ID,
            plan_id: *plan_id.as_bytes(),
            expected_total_failures: 1,
            reason: DENIED_REASON.to_owned(),
        },
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
    )
    .unwrap();
    let Err(denial) = denied.outcome.as_ref() else {
        panic!("expected typed policy failure");
    };
    assert_eq!(denial.code, i32::from(SabiErrorCode::Rights));
    assert_eq!(denial.safe_message, "SystemControl authorization denied");
}

#[cfg(unix)]
mod socket_harness {
    use std::path::Path;
    use std::sync::Arc;

    use nlos_ipc::unix::UnixListenerAdapter;
    use nlos_ipc::{OutboundResponse, TransportConfig, serve_one};
    use nlos_schema::sabi::v1::ExchangeResponse;

    use super::*;

    pub fn bind_socket(socket_path: &Path) -> UnixListenerAdapter {
        UnixListenerAdapter::bind(socket_path).unwrap()
    }

    pub fn serve_forever(
        listener: UnixListenerAdapter,
        authority: Arc<SqliteTaskAuthority>,
        health: StubHealth,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // An idle accept window is normal: the transport bounds
                // each accept by the 5s connect timeout, and a fresh CLI
                // subprocess (macOS first-exec scan) can exceed it. Only a
                // hard listener failure may retire the endpoint.
                let (stream, peer) = match listener.accept(TransportConfig::default()).await {
                    Ok(connection) => connection,
                    Err(nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept)) => continue,
                    Err(_) => break,
                };
                // One misbehaving exchange never takes the endpoint down.
                let _ = serve_one(stream, TransportConfig::default(), peer, &AllowPeer, {
                    let health = health.clone();
                    let authority = Arc::clone(&authority);
                    move |validated| {
                        let response = RecoverySystemControl::new(
                            authority.as_ref(),
                            &health,
                            &CapabilityPolicy,
                        )
                        .handle_for_ipc(
                            validated.envelope(),
                            MONOTONIC_NOW_NS,
                            WALL_NOW_MS,
                        );
                        async move {
                            Ok(OutboundResponse::Typed(ExchangeResponse {
                                envelope: Some(response),
                            }))
                        }
                    }
                })
                .await;
            }
        })
    }

    pub fn run_cli(socket: &Path, arguments: &[&str]) -> std::process::Output {
        // Cargo 1.97 names this env var after the exact bin name (dashes kept).
        let binary = std::env::var("CARGO_BIN_EXE_system-control-cli")
            .expect("system-control-cli binary missing; run tests with default features");
        std::process::Command::new(binary)
            .arg(socket.to_str().unwrap())
            .args(arguments)
            .output()
            .unwrap()
    }

    pub fn cli_receipt_bytes(output: &std::process::Output) -> Vec<u8> {
        let stdout = String::from_utf8(output.stdout.clone()).unwrap();
        let line = stdout.lines().next().unwrap();
        let hex = line.strip_prefix("RECEIPT ").unwrap();
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect()
    }

    pub async fn assert_in_process_socket_and_cli_parity(
        socket_path: &Path,
        control: &RecoverySystemControl<'_, StubHealth, CapabilityPolicy>,
        command: &ControlCommand,
        cli_args: &[&str],
    ) {
        use nlos_system_control::control::dispatch_over_socket;

        let direct = dispatch_in_process(control, command, MONOTONIC_NOW_NS, WALL_NOW_MS).unwrap();
        let library = dispatch_over_socket(socket_path, command).await.unwrap();
        assert_eq!(direct.to_bytes(), library.to_bytes());
        let cli = run_cli(socket_path, cli_args);
        assert!(
            cli.status.success(),
            "cli {:?} failed: code={:?} stdout={} stderr={}",
            cli_args,
            cli.status.code(),
            String::from_utf8_lossy(&cli.stdout),
            String::from_utf8_lossy(&cli.stderr),
        );
        assert_eq!(cli_receipt_bytes(&cli), direct.to_bytes());
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn cli_and_in_process_paths_produce_byte_identical_receipts() {
    use socket_harness::{
        assert_in_process_socket_and_cli_parity, bind_socket, cli_receipt_bytes, run_cli,
        serve_forever,
    };

    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(authority.as_ref());
    let plan_bytes: [u8; 16] = *plan_id.as_bytes();
    let socket_path = database.path.with_extension("sock");
    let listener = bind_socket(&socket_path);
    let server = serve_forever(listener, Arc::clone(&authority), health(&plan_id));
    let stub_health = health(&plan_id);
    let control = RecoverySystemControl::new(authority.as_ref(), &stub_health, &CapabilityPolicy);

    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectHealth,
        &["inspect-health"],
    )
    .await;
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::ExportMetrics,
        &["export-metrics"],
    )
    .await;

    let acknowledge = acknowledge_command(&plan_id);
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &acknowledge,
        &[
            "ack-recovery-alert",
            &hex(&ACK_COMMAND_ID),
            &hex(&plan_bytes),
            "1",
            ACK_REASON,
        ],
    )
    .await;
    let ack_direct =
        dispatch_in_process(&control, &acknowledge, MONOTONIC_NOW_NS, WALL_NOW_MS).unwrap();
    let ControlOutcome::Acknowledged { receipt_id } = ack_direct.outcome.as_ref().unwrap() else {
        panic!("expected acknowledgement receipt");
    };
    assert_eq!(receipt_id.len(), 16);

    let denied_reference = dispatch_in_process(
        &control,
        &ControlCommand::AcknowledgeRecoveryAlert {
            control_command_id: DENIED_COMMAND_ID,
            plan_id: plan_bytes,
            expected_total_failures: 1,
            reason: DENIED_REASON.to_owned(),
        },
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
    )
    .unwrap();
    let cli_denied = run_cli(
        &socket_path,
        &[
            "ack-recovery-alert",
            &hex(&DENIED_COMMAND_ID),
            &hex(&plan_bytes),
            "1",
            DENIED_REASON,
        ],
    );
    assert_eq!(cli_denied.status.code(), Some(1));
    let stdout = String::from_utf8(cli_denied.stdout.clone()).unwrap();
    assert!(stdout.contains("outcome=failure"));
    assert!(stdout.contains(&format!("code={}", i32::from(SabiErrorCode::Rights))));
    assert_eq!(cli_receipt_bytes(&cli_denied), denied_reference.to_bytes());

    assert_eq!(
        authority
            .inspect_artifact_recovery(plan_id)
            .unwrap()
            .unwrap()
            .state,
        nlos_task::ArtifactRecoveryState::Escalated
    );
    assert!(
        authority
            .list_artifact_recovery_alerts(8)
            .unwrap()
            .first()
            .unwrap()
            .acknowledgement
            .is_some()
    );

    server.abort();
    fs::remove_file(&socket_path).unwrap();
}

/// B-CONTROL-003 evidence: natural-language sentences compile to the exact
/// [`ControlCommand`]s a caller would construct directly, and dispatching
/// each over the same real Unix socket yields byte-identical receipts —
/// the first constructive slice of ROAD-B-005's "NL and CLI walk the same
/// ControlCommand/Receipt path" requirement.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn nl_sentences_compile_to_the_same_socket_receipts_as_direct_commands() {
    use nlos_system_control::control::dispatch_over_socket;
    use nlos_system_control::nl::{NL_ACK_REASON, parse_nl_command};

    use socket_harness::{bind_socket, serve_forever};

    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(authority.as_ref());
    let plan_bytes: [u8; 16] = *plan_id.as_bytes();
    let plan_hex = hex(&plan_bytes);
    let socket_path = database.path.with_extension("sock");
    let listener = bind_socket(&socket_path);
    let server = serve_forever(listener, Arc::clone(&authority), health(&plan_id));
    let stub_health = health(&plan_id);
    let control = RecoverySystemControl::new(authority.as_ref(), &stub_health, &CapabilityPolicy);

    // Inspect: the NL sentence and the direct construction are the same
    // command, so both socket dispatches answer with byte-identical receipts.
    let nl_inspect = parse_nl_command("  Inspect   Health ").unwrap();
    assert_eq!(nl_inspect, ControlCommand::InspectHealth);
    assert_eq!(
        parse_nl_command("查看健康").unwrap(),
        ControlCommand::InspectHealth
    );
    let direct_inspect = dispatch_over_socket(&socket_path, &ControlCommand::InspectHealth)
        .await
        .unwrap();
    let nl_inspect_receipt = dispatch_over_socket(&socket_path, &nl_inspect)
        .await
        .unwrap();
    assert_eq!(direct_inspect.to_bytes(), nl_inspect_receipt.to_bytes());
    let inspect_in_process =
        dispatch_in_process(&control, &nl_inspect, MONOTONIC_NOW_NS, WALL_NOW_MS).unwrap();
    assert_eq!(inspect_in_process.to_bytes(), direct_inspect.to_bytes());

    // Export metrics: read-only `get` with zero alerts, OpenMetrics projection.
    let nl_export = parse_nl_command("export metrics").unwrap();
    assert_eq!(nl_export, ControlCommand::ExportMetrics);
    assert_eq!(
        parse_nl_command("导出指标").unwrap(),
        ControlCommand::ExportMetrics
    );
    let direct_export = dispatch_over_socket(&socket_path, &ControlCommand::ExportMetrics)
        .await
        .unwrap();
    let nl_export_receipt = dispatch_over_socket(&socket_path, &nl_export)
        .await
        .unwrap();
    assert_eq!(direct_export.to_bytes(), nl_export_receipt.to_bytes());
    let export_in_process =
        dispatch_in_process(&control, &nl_export, MONOTONIC_NOW_NS, WALL_NOW_MS).unwrap();
    assert_eq!(export_in_process.to_bytes(), direct_export.to_bytes());
    let ControlOutcome::MetricsExported(export_payload) = direct_export.outcome.as_ref().unwrap()
    else {
        panic!("expected metrics export receipt");
    };
    assert!(
        export_payload
            .openmetrics_text
            .contains("nlos_artifact_recovery_cycles_total")
    );

    // Acknowledge: the English and Chinese sentences both compile to the
    // same fully-determined mutation the direct construction spells out.
    let direct_ack = ControlCommand::AcknowledgeRecoveryAlert {
        control_command_id: plan_bytes,
        plan_id: plan_bytes,
        expected_total_failures: 1,
        reason: NL_ACK_REASON.to_owned(),
    };
    let english_ack =
        parse_nl_command(&format!("acknowledge alert {plan_hex} expecting 1")).unwrap();
    assert_eq!(english_ack, direct_ack);
    let nl_ack = parse_nl_command(&format!("确认告警 {plan_hex} 期望 1")).unwrap();
    assert_eq!(nl_ack, direct_ack);
    let direct_ack_receipt = dispatch_over_socket(&socket_path, &direct_ack)
        .await
        .unwrap();
    let nl_ack_receipt = dispatch_over_socket(&socket_path, &nl_ack).await.unwrap();
    let ControlOutcome::Acknowledged { receipt_id } = direct_ack_receipt.outcome.as_ref().unwrap()
    else {
        panic!("expected acknowledgement receipt");
    };
    assert_eq!(receipt_id.len(), 16);
    assert_eq!(direct_ack_receipt.to_bytes(), nl_ack_receipt.to_bytes());
    let ack_in_process =
        dispatch_in_process(&control, &nl_ack, MONOTONIC_NOW_NS, WALL_NOW_MS).unwrap();
    assert_eq!(ack_in_process.to_bytes(), direct_ack_receipt.to_bytes());
    assert!(
        authority
            .list_artifact_recovery_alerts(8)
            .unwrap()
            .first()
            .unwrap()
            .acknowledgement
            .is_some()
    );

    // Out-of-grammar natural language is a typed rejection before any
    // dispatch; it never reaches the socket.
    assert!(matches!(
        parse_nl_command("pause everything"),
        Err(nlos_system_control::control::ControlError::InvalidCommand(
            _
        ))
    ));

    server.abort();
    fs::remove_file(&socket_path).unwrap();
}
