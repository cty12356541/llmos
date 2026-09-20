#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
#![allow(clippy::too_many_lines)]
//! B-CONTROL-001 integration evidence: the in-process dispatcher and the
//! `system-control-cli` binary both cross the same real `SystemControl`
//! handler path — the CLI and the library client over a real Unix socket —
//! and produce byte-identical typed [`ControlReceipt`]s.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_schema::sabi::v1::SabiFailure;
use nlos_schema::sabi::v1::{
    ControlCommand as WireControlCommand, GetSystemControlRequest, SabiErrorCode,
    SabiRequestContext,
};
use nlos_system_control::control::{
    ControlCommand, ControlOutcome, ProcessInspection, ProcessInspector, RecoveryWorkerLifecycle,
    ResourceInspection, ResourceInspector, dispatch_in_process, parse_hex_id,
};
use nlos_system_control::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};
#[cfg(unix)]
use nlos_system_control::{OperationCommandExecutor, OperationControlRequest};
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
#[cfg(unix)]
use nlos_types::ReceiptId;

const MONOTONIC_NOW_NS: u64 = 10;
const WALL_NOW_MS: i64 = 6_000;
const ACK_COMMAND_ID: [u8; 16] = [0x41; 16];
const DENIED_COMMAND_ID: [u8; 16] = [0x42; 16];
const ACK_REASON: &str = "inspected recovery evidence";
const DENIED_REASON: &str = "denied: exercising the policy denial path";

const PROCESS_ID: [u8; 16] = [0x77; 16];
const RESERVATION_ID: [u8; 16] = [0x88; 16];

// 仅被 #[cfg(unix)] 的 socket/CLI parity 测试使用；Windows 腿编译时置空避免 dead_code
#[cfg(unix)]
const SEMANTIC_PLAN_ID: [u8; 16] = [0x71; 16];
#[cfg(unix)]
const SEMANTIC_ACK_COMMAND_ID: [u8; 16] = [0x53; 16];
#[cfg(unix)]
const SEMANTIC_RESUME_COMMAND_ID: [u8; 16] = [0x54; 16];
#[cfg(unix)]
const SEMANTIC_TOTAL_FAILURES: u64 = 8;
#[cfg(unix)]
const SEMANTIC_REASON: &str = "inspected semantic recovery evidence";

#[cfg(unix)]
const RESOURCE_PLAN_ID: [u8; 16] = [0x81; 16];
#[cfg(unix)]
const RESOURCE_ACK_COMMAND_ID: [u8; 16] = [0x57; 16];
#[cfg(unix)]
const RESOURCE_RESUME_COMMAND_ID: [u8; 16] = [0x58; 16];
#[cfg(unix)]
const RESOURCE_TOTAL_FAILURES: u64 = 8;
#[cfg(unix)]
const RESOURCE_REASON: &str = "inspected resource recovery evidence";

/// Seeds one escalated semantic recovery ledger row directly (see the
/// `recovery_control` fixture note: the `Escalated` transition is W26-tested
/// inside `nlos-task`; the per-connection foreign key is left unchecked by
/// the raw seeding connection).
#[cfg(unix)]
fn seed_escalated_semantic_recovery(database: &TestDatabase) {
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
}

/// Returns the semantic ledger row to its seeded `Escalated` shape. Unlike an
/// acknowledgement, one resume consumes the `Escalated` state, so the
/// byte-parity harness re-arms the durable input between the reference
/// dispatch and the CLI dispatch of the same resume command.
#[cfg(unix)]
fn reset_escalated_semantic_recovery(database: &TestDatabase) {
    let raw = rusqlite::Connection::open(&database.path).unwrap();
    raw.execute(
        "UPDATE task_semantic_recovery
         SET recovery_state = 1, consecutive_failures = ?2, next_retry_at_ms = NULL,
             escalated_at_ms = 1500, resolved_at_ms = NULL, updated_at_ms = 1500
         WHERE plan_id = ?1",
        rusqlite::params![
            SEMANTIC_PLAN_ID.as_slice(),
            SEMANTIC_TOTAL_FAILURES.to_be_bytes().as_slice(),
        ],
    )
    .unwrap();
}

/// Seeds one escalated `task_resource_recovery` ledger row directly (the
/// W28-C `Escalated` transition is tested inside `nlos-task`; the
/// per-connection foreign key to `task_resource_commit_plans` is left
/// unchecked by the raw seeding connection).
#[cfg(unix)]
fn seed_escalated_resource_recovery(database: &TestDatabase) {
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
}

/// Resource mirror of [`reset_escalated_semantic_recovery`]: re-arms the
/// `Escalated` row between the reference dispatch and the CLI dispatch of
/// the same resume command.
#[cfg(unix)]
fn reset_escalated_resource_recovery(database: &TestDatabase) {
    let raw = rusqlite::Connection::open(&database.path).unwrap();
    raw.execute(
        "UPDATE task_resource_recovery
         SET recovery_state = 1, consecutive_failures = ?2, next_retry_at_ms = NULL,
             escalated_at_ms = 1500, resolved_at_ms = NULL, updated_at_ms = 1500
         WHERE plan_id = ?1",
        rusqlite::params![
            RESOURCE_PLAN_ID.as_slice(),
            RESOURCE_TOTAL_FAILURES.to_be_bytes().as_slice(),
        ],
    )
    .unwrap();
}

struct StubProcessInspector {
    snapshot: ProcessInspection,
}

impl ProcessInspector for StubProcessInspector {
    fn inspect_process(&self, process_id: [u8; 16]) -> Result<ProcessInspection, SabiFailure> {
        if process_id == self.snapshot.process_id {
            Ok(self.snapshot.clone())
        } else {
            Err(nlos_schema::sabi::v1::SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: nlos_schema::sabi::v1::RetryDirective::DoNotRetry.into(),
                safe_message: "requested process binding was not found".to_owned(),
            })
        }
    }
}

fn stub_process_inspection() -> StubProcessInspector {
    StubProcessInspector {
        snapshot: ProcessInspection {
            process_id: PROCESS_ID,
            process_generation: 1,
            agent_instance_id: [0x78; 16],
            task_id: [0x79; 16],
            task_attempt_id: [0x7A; 16],
            isolation_domain_id: [0x7B; 16],
        },
    }
}

struct StubResourceInspector {
    snapshot: ResourceInspection,
}

impl ResourceInspector for StubResourceInspector {
    fn inspect_resource(
        &self,
        reservation_id: [u8; 16],
    ) -> Result<ResourceInspection, SabiFailure> {
        if reservation_id == self.snapshot.reservation_id {
            Ok(self.snapshot.clone())
        } else {
            Err(nlos_schema::sabi::v1::SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: nlos_schema::sabi::v1::RetryDirective::DoNotRetry.into(),
                safe_message: "requested resource reservation was not found".to_owned(),
            })
        }
    }
}

fn stub_resource_inspection() -> StubResourceInspector {
    StubResourceInspector {
        snapshot: ResourceInspection {
            reservation_id: RESERVATION_ID,
            account_id: [0x89; 16],
            upper_bound: 100,
            usage_high_water: 37,
            consumption_count: 2,
        },
    }
}

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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
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
        None,
        None,
    )
    .unwrap();
    let Err(denial) = denied.outcome.as_ref() else {
        panic!("expected typed policy failure");
    };
    assert_eq!(denial.code, i32::from(SabiErrorCode::Rights));
    assert_eq!(denial.safe_message, "SystemControl authorization denied");

    let unwired_process = dispatch_in_process(
        &control,
        &ControlCommand::InspectProcess {
            process_id: PROCESS_ID,
        },
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
    let Err(unwired_failure) = unwired_process.outcome.as_ref() else {
        panic!("expected typed failure when the process backend is unwired");
    };
    assert_eq!(unwired_failure.code, i32::from(SabiErrorCode::NotFound));

    let stub = stub_process_inspection();
    let process_receipt = dispatch_in_process(
        &control,
        &ControlCommand::InspectProcess {
            process_id: PROCESS_ID,
        },
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        Some(&stub),
        None,
    )
    .unwrap();
    let ControlOutcome::ProcessInspected(snapshot) = process_receipt.outcome.as_ref().unwrap()
    else {
        panic!("expected process inspection receipt");
    };
    assert_eq!(snapshot.process_id, PROCESS_ID);
    assert_eq!(snapshot.process_generation, 1);

    let unwired_resource = dispatch_in_process(
        &control,
        &ControlCommand::InspectResource {
            reservation_id: RESERVATION_ID,
        },
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
    let Err(unwired_resource_failure) = unwired_resource.outcome.as_ref() else {
        panic!("expected typed failure when the resource backend is unwired");
    };
    assert_eq!(
        unwired_resource_failure.code,
        i32::from(SabiErrorCode::NotFound)
    );

    let resource_stub = stub_resource_inspection();
    let resource_receipt = dispatch_in_process(
        &control,
        &ControlCommand::InspectResource {
            reservation_id: RESERVATION_ID,
        },
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        Some(&resource_stub),
    )
    .unwrap();
    let ControlOutcome::ResourceInspected(resource_snapshot) =
        resource_receipt.outcome.as_ref().unwrap()
    else {
        panic!("expected resource inspection receipt");
    };
    assert_eq!(resource_snapshot.reservation_id, RESERVATION_ID);
    assert_eq!(resource_snapshot.usage_high_water, 37);
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
        serve_forever_with_executor(listener, authority, health, None)
    }

    pub fn serve_forever_with_executor(
        listener: UnixListenerAdapter,
        authority: Arc<SqliteTaskAuthority>,
        health: StubHealth,
        executor: Option<Arc<dyn OperationCommandExecutor + Send + Sync>>,
    ) -> tokio::task::JoinHandle<()> {
        serve_forever_with_backends(listener, authority, health, executor, None)
    }

    pub fn serve_forever_with_backends(
        listener: UnixListenerAdapter,
        authority: Arc<SqliteTaskAuthority>,
        health: StubHealth,
        executor: Option<Arc<dyn OperationCommandExecutor + Send + Sync>>,
        layer_sources: Option<Arc<StubLayerSources>>,
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
                    let executor = executor.clone();
                    let layer_sources = layer_sources.clone();
                    move |validated| {
                        let control = RecoverySystemControl::new(
                            authority.as_ref(),
                            &health,
                            &CapabilityPolicy,
                        );
                        let control = match executor.as_ref() {
                            Some(executor) => control.with_operation_executor(executor.as_ref()),
                            None => control,
                        };
                        let control = match layer_sources.as_ref() {
                            Some(sources) => control
                                .with_task_node_source(sources.as_ref())
                                .with_execution_fiber_source(sources.as_ref())
                                .with_topic_source(sources.as_ref())
                                .with_operation_source(sources.as_ref()),
                            None => control,
                        };
                        let response = control.handle_for_ipc(
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
        process: Option<&dyn ProcessInspector>,
        resource: Option<&dyn ResourceInspector>,
    ) {
        use nlos_system_control::control::dispatch_over_socket;

        let direct = dispatch_in_process(
            control,
            command,
            MONOTONIC_NOW_NS,
            WALL_NOW_MS,
            process,
            resource,
        )
        .unwrap();
        let library = dispatch_over_socket(socket_path, command, process, resource)
            .await
            .unwrap();
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

    pub async fn assert_nl_socket_and_in_process_parity(
        socket_path: &Path,
        control: &RecoverySystemControl<'_, StubHealth, CapabilityPolicy>,
        direct: &ControlCommand,
        nl_commands: &[ControlCommand],
        process: Option<&dyn ProcessInspector>,
        resource: Option<&dyn ResourceInspector>,
    ) {
        use nlos_system_control::control::dispatch_over_socket;

        let direct_receipt = dispatch_over_socket(socket_path, direct, process, resource)
            .await
            .unwrap();
        for nl_command in nl_commands {
            let nl_receipt = dispatch_over_socket(socket_path, nl_command, process, resource)
                .await
                .unwrap();
            assert_eq!(direct_receipt.to_bytes(), nl_receipt.to_bytes());
            let in_process = dispatch_in_process(
                control,
                nl_command,
                MONOTONIC_NOW_NS,
                WALL_NOW_MS,
                process,
                resource,
            )
            .unwrap();
            assert_eq!(in_process.to_bytes(), direct_receipt.to_bytes());
        }
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
        None,
        None,
    )
    .await;
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::ExportMetrics,
        &["export-metrics"],
        None,
        None,
    )
    .await;

    let unwired_process = ControlCommand::InspectProcess {
        process_id: PROCESS_ID,
    };
    let unwired_reference = dispatch_in_process(
        &control,
        &unwired_process,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
    let cli_unwired = run_cli(&socket_path, &["inspect-process", &hex(&PROCESS_ID)]);
    assert_eq!(cli_unwired.status.code(), Some(1));
    assert_eq!(
        cli_receipt_bytes(&cli_unwired),
        unwired_reference.to_bytes()
    );

    let unwired_resource = ControlCommand::InspectResource {
        reservation_id: RESERVATION_ID,
    };
    let unwired_resource_reference = dispatch_in_process(
        &control,
        &unwired_resource,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
    let cli_unwired_resource = run_cli(&socket_path, &["inspect-resource", &hex(&RESERVATION_ID)]);
    assert_eq!(cli_unwired_resource.status.code(), Some(1));
    assert_eq!(
        cli_receipt_bytes(&cli_unwired_resource),
        unwired_resource_reference.to_bytes()
    );

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
        None,
        None,
    )
    .await;
    let ack_direct = dispatch_in_process(
        &control,
        &acknowledge,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
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
        None,
        None,
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

    // Semantic channel: the escalated semantic plan is visible through the
    // semantic inspection, acknowledged, and resumed through the same
    // in-process / socket / CLI paths, byte-identically.
    seed_escalated_semantic_recovery(&database);
    let semantic_plan = nlos_task::SemanticCommitPlanId::from_bytes(SEMANTIC_PLAN_ID);

    let semantic_inspection = dispatch_in_process(
        &control,
        &ControlCommand::InspectSemanticHealth,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
    let ControlOutcome::SemanticInspected(inspection) =
        semantic_inspection.outcome.as_ref().unwrap()
    else {
        panic!("expected semantic inspection receipt");
    };
    assert_eq!(inspection.alerts.len(), 1);
    assert_eq!(inspection.alerts[0].plan_id, SEMANTIC_PLAN_ID);

    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectSemanticHealth,
        &["inspect-semantic-health"],
        None,
        None,
    )
    .await;
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::ExportSemanticMetrics,
        &["export-semantic-metrics"],
        None,
        None,
    )
    .await;

    let semantic_acknowledge = ControlCommand::AcknowledgeSemanticRecoveryAlert {
        control_command_id: SEMANTIC_ACK_COMMAND_ID,
        plan_id: SEMANTIC_PLAN_ID,
        expected_total_failures: SEMANTIC_TOTAL_FAILURES,
        reason: SEMANTIC_REASON.to_owned(),
    };
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &semantic_acknowledge,
        &[
            "ack-semantic-recovery-alert",
            &hex(&SEMANTIC_ACK_COMMAND_ID),
            &hex(&SEMANTIC_PLAN_ID),
            &SEMANTIC_TOTAL_FAILURES.to_string(),
            SEMANTIC_REASON,
        ],
        None,
        None,
    )
    .await;

    let semantic_resume = ControlCommand::ResumeSemanticRecovery {
        control_command_id: SEMANTIC_RESUME_COMMAND_ID,
        plan_id: SEMANTIC_PLAN_ID,
        expected_total_failures: SEMANTIC_TOTAL_FAILURES,
        reason: SEMANTIC_REASON.to_owned(),
    };
    let resume_reference = dispatch_in_process(
        &control,
        &semantic_resume,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
    let ControlOutcome::Resumed { receipt_id } = resume_reference.outcome.as_ref().unwrap() else {
        panic!("expected resumed receipt");
    };
    assert_eq!(
        receipt_id,
        &nlos_task::semantic_recovery_resume_reference(semantic_plan, SEMANTIC_TOTAL_FAILURES)
            .as_bytes()
            .to_vec()
    );
    assert_eq!(
        authority
            .inspect_semantic_recovery(semantic_plan)
            .unwrap()
            .unwrap()
            .state,
        nlos_task::SemanticRecoveryState::Retrying
    );

    reset_escalated_semantic_recovery(&database);
    let cli_resume = run_cli(
        &socket_path,
        &[
            "resume-semantic-recovery",
            &hex(&SEMANTIC_RESUME_COMMAND_ID),
            &hex(&SEMANTIC_PLAN_ID),
            &SEMANTIC_TOTAL_FAILURES.to_string(),
            SEMANTIC_REASON,
        ],
    );
    assert!(
        cli_resume.status.success(),
        "cli resume failed: code={:?} stdout={} stderr={}",
        cli_resume.status.code(),
        String::from_utf8_lossy(&cli_resume.stdout),
        String::from_utf8_lossy(&cli_resume.stderr),
    );
    assert_eq!(cli_receipt_bytes(&cli_resume), resume_reference.to_bytes());
    assert_eq!(
        authority
            .inspect_semantic_recovery(semantic_plan)
            .unwrap()
            .unwrap()
            .state,
        nlos_task::SemanticRecoveryState::Retrying
    );

    server.abort();
    fs::remove_file(&socket_path).unwrap();
}

#[cfg(unix)]
fn assert_nl_sentences_reject(sentences: &[&str]) {
    use nlos_system_control::control::ControlError;
    use nlos_system_control::nl::parse_nl_command;

    for sentence in sentences {
        assert!(
            matches!(
                parse_nl_command(sentence),
                Err(ControlError::InvalidCommand(_))
            ),
            "expected typed rejection for {sentence:?}"
        );
    }
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

    use socket_harness::{assert_nl_socket_and_in_process_parity, bind_socket, serve_forever};

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

    // Inspect: canonical and synonym NL sentences compile to the same command,
    // so both socket dispatches answer with byte-identical receipts.
    let nl_inspect = parse_nl_command("  Inspect   Health ").unwrap();
    assert_eq!(nl_inspect, ControlCommand::InspectHealth);
    assert_eq!(
        parse_nl_command("查看 系统 健康").unwrap(),
        ControlCommand::InspectHealth
    );
    assert_eq!(
        parse_nl_command("查看 健康").unwrap(),
        ControlCommand::InspectHealth
    );
    assert_eq!(
        parse_nl_command("health check").unwrap(),
        ControlCommand::InspectHealth
    );
    assert_eq!(
        parse_nl_command("系统状态").unwrap(),
        ControlCommand::InspectHealth
    );
    let synonym_inspect = parse_nl_command("check health").unwrap();
    assert_eq!(synonym_inspect, ControlCommand::InspectHealth);
    let status_health = parse_nl_command("status health").unwrap();
    assert_eq!(status_health, ControlCommand::InspectHealth);
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectHealth,
        &[nl_inspect, synonym_inspect, status_health],
        None,
        None,
    )
    .await;
    let health_check = parse_nl_command("health check").unwrap();
    let system_status = parse_nl_command("系统状态").unwrap();
    let spaced_health = parse_nl_command("查看 健康").unwrap();
    let check_health = parse_nl_command("检查 健康").unwrap();
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectHealth,
        &[health_check, system_status, spaced_health, check_health],
        None,
        None,
    )
    .await;
    let health_status = parse_nl_command("health status").unwrap();
    assert_eq!(health_status, ControlCommand::InspectHealth);
    let zh_health_status = parse_nl_command("健康 状态").unwrap();
    assert_eq!(zh_health_status, ControlCommand::InspectHealth);
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectHealth,
        &[health_status, zh_health_status],
        None,
        None,
    )
    .await;

    // Inspect task: NL sentences compile to the same command and produce
    // byte-identical receipts over socket and in-process dispatch.
    let direct_task = ControlCommand::InspectTask {
        plan_id: plan_bytes,
    };
    let nl_task = parse_nl_command(&format!("inspect task {plan_hex}")).unwrap();
    assert_eq!(nl_task, direct_task);
    assert_eq!(
        parse_nl_command(&format!("查看任务 {plan_hex}")).unwrap(),
        direct_task
    );
    let synonym_task = parse_nl_command(&format!("check task {plan_hex}")).unwrap();
    assert_eq!(synonym_task, direct_task);
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &direct_task,
        &[nl_task, synonym_task],
        None,
        None,
    )
    .await;
    let task_status = parse_nl_command(&format!("task status {plan_hex}")).unwrap();
    assert_eq!(task_status, direct_task);
    let zh_task_status = parse_nl_command(&format!("任务 状态 {plan_hex}")).unwrap();
    assert_eq!(zh_task_status, direct_task);
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &direct_task,
        &[task_status, zh_task_status],
        None,
        None,
    )
    .await;
    let task_receipt = dispatch_in_process(
        &control,
        &direct_task,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
    let ControlOutcome::Inspected(task_inspection) = task_receipt.outcome.as_ref().unwrap() else {
        panic!("expected scoped inspection receipt");
    };
    assert_eq!(task_inspection.alerts.len(), 1);
    assert_eq!(task_receipt.control_command_id, plan_bytes);

    // Export metrics: read-only `get` with zero alerts, OpenMetrics projection.
    let nl_export = parse_nl_command("export metrics").unwrap();
    assert_eq!(nl_export, ControlCommand::ExportMetrics);
    assert_eq!(
        parse_nl_command("show metrics").unwrap(),
        ControlCommand::ExportMetrics
    );
    assert_eq!(
        parse_nl_command("metrics").unwrap(),
        ControlCommand::ExportMetrics
    );
    assert_eq!(
        parse_nl_command("指标").unwrap(),
        ControlCommand::ExportMetrics
    );
    let direct_export =
        dispatch_over_socket(&socket_path, &ControlCommand::ExportMetrics, None, None)
            .await
            .unwrap();
    let nl_export_receipt = dispatch_over_socket(&socket_path, &nl_export, None, None)
        .await
        .unwrap();
    assert_eq!(direct_export.to_bytes(), nl_export_receipt.to_bytes());
    let metrics_synonym = parse_nl_command("metrics").unwrap();
    let metrics_zh = parse_nl_command("指标").unwrap();
    let metrics_synonym_receipt = dispatch_over_socket(&socket_path, &metrics_synonym, None, None)
        .await
        .unwrap();
    assert_eq!(direct_export.to_bytes(), metrics_synonym_receipt.to_bytes());
    let metrics_zh_receipt = dispatch_over_socket(&socket_path, &metrics_zh, None, None)
        .await
        .unwrap();
    assert_eq!(direct_export.to_bytes(), metrics_zh_receipt.to_bytes());
    let export_in_process = dispatch_in_process(
        &control,
        &nl_export,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
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

    // Inspect process: NL sentences compile to the same command and, with a
    // wired stub inspector, produce byte-identical receipts over socket and
    // in-process dispatch.
    let process_hex = hex(&PROCESS_ID);
    let direct_process = ControlCommand::InspectProcess {
        process_id: PROCESS_ID,
    };
    let nl_process = parse_nl_command(&format!("inspect process {process_hex}")).unwrap();
    assert_eq!(nl_process, direct_process);
    assert_eq!(
        parse_nl_command(&format!("检查进程 {process_hex}")).unwrap(),
        direct_process
    );
    let synonym_process = parse_nl_command(&format!("check process {process_hex}")).unwrap();
    assert_eq!(synonym_process, direct_process);
    let stub = stub_process_inspection();
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &direct_process,
        &[nl_process, synonym_process],
        Some(&stub),
        None,
    )
    .await;
    let process_receipt = dispatch_in_process(
        &control,
        &direct_process,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        Some(&stub),
        None,
    )
    .unwrap();
    let ControlOutcome::ProcessInspected(snapshot) = process_receipt.outcome.as_ref().unwrap()
    else {
        panic!("expected process inspection receipt");
    };
    assert_eq!(snapshot.process_id, PROCESS_ID);

    // Inspect resource: NL sentences compile to the same command and, with a
    // wired stub inspector, produce byte-identical receipts over socket and
    // in-process dispatch.
    let reservation_hex = hex(&RESERVATION_ID);
    let direct_resource = ControlCommand::InspectResource {
        reservation_id: RESERVATION_ID,
    };
    let nl_resource = parse_nl_command(&format!("inspect resource {reservation_hex}")).unwrap();
    assert_eq!(nl_resource, direct_resource);
    assert_eq!(
        parse_nl_command(&format!("查看资源 {reservation_hex}")).unwrap(),
        direct_resource
    );
    let synonym_resource = parse_nl_command(&format!("check resource {reservation_hex}")).unwrap();
    assert_eq!(synonym_resource, direct_resource);
    let resource_stub = stub_resource_inspection();
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &direct_resource,
        &[nl_resource, synonym_resource],
        None,
        Some(&resource_stub),
    )
    .await;
    let resource_status = parse_nl_command(&format!("resource status {reservation_hex}")).unwrap();
    assert_eq!(resource_status, direct_resource);
    let zh_resource_status = parse_nl_command(&format!("资源 状态 {reservation_hex}")).unwrap();
    assert_eq!(zh_resource_status, direct_resource);
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &direct_resource,
        &[resource_status, zh_resource_status],
        None,
        Some(&resource_stub),
    )
    .await;
    let resource_receipt = dispatch_in_process(
        &control,
        &direct_resource,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        Some(&resource_stub),
    )
    .unwrap();
    let ControlOutcome::ResourceInspected(resource_snapshot) =
        resource_receipt.outcome.as_ref().unwrap()
    else {
        panic!("expected resource inspection receipt");
    };
    assert_eq!(resource_snapshot.reservation_id, RESERVATION_ID);

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
    let nl_ack = parse_nl_command(&format!("确认 告警 {plan_hex} 期望 1")).unwrap();
    assert_eq!(nl_ack, direct_ack);
    let ack_synonym = parse_nl_command(&format!("ack alert {plan_hex} expecting 1")).unwrap();
    assert_eq!(ack_synonym, direct_ack);
    let direct_ack_receipt = dispatch_over_socket(&socket_path, &direct_ack, None, None)
        .await
        .unwrap();
    let ControlOutcome::Acknowledged { receipt_id } = direct_ack_receipt.outcome.as_ref().unwrap()
    else {
        panic!("expected acknowledgement receipt");
    };
    assert_eq!(receipt_id.len(), 16);
    for nl_command in [nl_ack, ack_synonym] {
        let nl_ack_receipt = dispatch_over_socket(&socket_path, &nl_command, None, None)
            .await
            .unwrap();
        assert_eq!(direct_ack_receipt.to_bytes(), nl_ack_receipt.to_bytes());
    }
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
    assert_nl_sentences_reject(&[
        "pause everything",
        "show health now",
        "cancel alert a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 1",
        "task status now",
        "任务状态了",
        "resource status now",
        "资源状态了",
        "pause operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting",
        "resume task a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 1",
        "暂停操作 a1b2c3d4e5f60718293a4b5c6d7e8f90",
    ]);

    server.abort();
    fs::remove_file(&socket_path).unwrap();
}

#[cfg(unix)]
const OPERATION_TARGET_ID: [u8; 16] = [0x81; 16];
#[cfg(unix)]
const OPERATION_CAS: u64 = 4;

/// Deterministic stub executor whose receipt id names the executed arm in
/// its first byte (mirrors `recovery_control`'s recording stub).
#[cfg(unix)]
struct DeterministicOperationExecutor;

#[cfg(unix)]
fn operation_receipt(arm_tag: u8) -> [u8; 16] {
    let mut id = OPERATION_TARGET_ID;
    id[0] = arm_tag;
    id
}

#[cfg(unix)]
impl OperationCommandExecutor for DeterministicOperationExecutor {
    fn pause_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(1)))
    }

    fn resume_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(2)))
    }

    fn cancel_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(3)))
    }

    fn kill_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(4)))
    }

    fn throttle_operation(
        &self,
        _: OperationControlRequest,
        _: u64,
    ) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(5)))
    }

    fn reclaim_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(6)))
    }
}

/// W28-D parity gate: the pause/resume/cancel command surface compiles from
/// direct construction, NL sentences (EN/ZH/synonyms), and the CLI into the
/// same wire command, and all three dispatch paths answer with
/// byte-identical typed receipts through the wired executor seam. This is
/// the constructive first half of ROAD-B-005's operation-level control
/// requirement (B5-1); the real executors land in W29-D.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn operation_control_commands_are_byte_identical_across_nl_cli_and_direct_paths() {
    use nlos_system_control::control::dispatch_over_socket;
    use nlos_system_control::nl::{
        NL_CANCEL_REASON, NL_PAUSE_REASON, NL_RESUME_REASON, parse_nl_command,
    };

    use socket_harness::{
        assert_in_process_socket_and_cli_parity, assert_nl_socket_and_in_process_parity,
        bind_socket, run_cli, serve_forever_with_executor,
    };

    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(authority.as_ref());
    let socket_path = database.path.with_extension("sock");
    let listener = bind_socket(&socket_path);
    let server = serve_forever_with_executor(
        listener,
        Arc::clone(&authority),
        health(&plan_id),
        Some(Arc::new(DeterministicOperationExecutor)),
    );
    let stub_health = health(&plan_id);
    let control = RecoverySystemControl::new(authority.as_ref(), &stub_health, &CapabilityPolicy)
        .with_operation_executor(&DeterministicOperationExecutor);

    let target_hex = hex(&OPERATION_TARGET_ID);
    for (label, reason, nl_sentences, cli_operation, expected_receipt, expected_outcome) in [
        (
            "pause",
            NL_PAUSE_REASON,
            vec![
                format!("pause operation {target_hex} expecting {OPERATION_CAS}"),
                format!("halt operation {target_hex} expecting {OPERATION_CAS}"),
                format!("暂停操作 {target_hex} 期望 {OPERATION_CAS}"),
                format!("暂停 操作 {target_hex} 期望 {OPERATION_CAS}"),
            ],
            "pause-operation",
            operation_receipt(1),
            8_u8,
        ),
        (
            "resume",
            NL_RESUME_REASON,
            vec![
                format!("resume operation {target_hex} expecting {OPERATION_CAS}"),
                format!("恢复操作 {target_hex} 期望 {OPERATION_CAS}"),
                format!("恢复 操作 {target_hex} 期望 {OPERATION_CAS}"),
            ],
            "resume-operation",
            operation_receipt(2),
            9_u8,
        ),
        (
            "cancel",
            NL_CANCEL_REASON,
            vec![
                format!("cancel operation {target_hex} expecting {OPERATION_CAS}"),
                format!("abort operation {target_hex} expecting {OPERATION_CAS}"),
                format!("取消操作 {target_hex} 期望 {OPERATION_CAS}"),
                format!("取消 操作 {target_hex} 期望 {OPERATION_CAS}"),
            ],
            "cancel-operation",
            operation_receipt(3),
            10_u8,
        ),
    ] {
        let operation = match label {
            "pause" => ControlCommand::PauseOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: reason.to_owned(),
            },
            "resume" => ControlCommand::ResumeOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: reason.to_owned(),
            },
            _ => ControlCommand::CancelOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: reason.to_owned(),
            },
        };
        for sentence in &nl_sentences {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                operation,
                "{label} sentence {sentence:?}"
            );
        }
        let nl_commands: Vec<ControlCommand> = nl_sentences
            .iter()
            .map(|sentence| parse_nl_command(sentence).unwrap())
            .collect();
        assert_nl_socket_and_in_process_parity(
            &socket_path,
            &control,
            &operation,
            &nl_commands,
            None,
            None,
        )
        .await;
        assert_in_process_socket_and_cli_parity(
            &socket_path,
            &control,
            &operation,
            &[
                cli_operation,
                &hex(&OPERATION_TARGET_ID),
                &hex(&OPERATION_TARGET_ID),
                &OPERATION_CAS.to_string(),
                reason,
            ],
            None,
            None,
        )
        .await;
        let receipt = dispatch_over_socket(&socket_path, &operation, None, None)
            .await
            .unwrap();
        match receipt.outcome.as_ref().unwrap() {
            ControlOutcome::OperationPaused { receipt_id }
            | ControlOutcome::OperationResumed { receipt_id }
            | ControlOutcome::OperationCancelled { receipt_id } => {
                assert_eq!(receipt_id, &expected_receipt.to_vec());
            }
            other => panic!("expected an operation-level outcome, got {other:?}"),
        }
        // Deterministic encoding: 16 command id bytes, u32 length prefix,
        // 16 correlation bytes, then the outcome tag byte.
        assert_eq!(receipt.to_bytes()[16 + 4 + 16], expected_outcome);
    }

    // The unwired default is exercised against the same socket contract in
    // `recovery_control`; here the denial path proves the authorizer still
    // fronts the new arms on the shared submit route.
    let denied = ControlCommand::PauseOperation {
        control_command_id: DENIED_COMMAND_ID,
        target_id: OPERATION_TARGET_ID,
        expected_generation_or_revision: OPERATION_CAS,
        reason: DENIED_REASON.to_owned(),
    };
    let denied_reference =
        dispatch_in_process(&control, &denied, MONOTONIC_NOW_NS, WALL_NOW_MS, None, None).unwrap();
    let Err(failure) = denied_reference.outcome.as_ref() else {
        panic!("expected typed policy failure");
    };
    assert_eq!(failure.code, i32::from(SabiErrorCode::Rights));
    let cli_denied = run_cli(
        &socket_path,
        &[
            "pause-operation",
            &hex(&DENIED_COMMAND_ID),
            &target_hex,
            &OPERATION_CAS.to_string(),
            DENIED_REASON,
        ],
    );
    assert_eq!(cli_denied.status.code(), Some(1));
    let stdout = String::from_utf8(cli_denied.stdout.clone()).unwrap();
    assert!(stdout.contains("outcome=failure"));
    assert_eq!(
        socket_harness::cli_receipt_bytes(&cli_denied),
        denied_reference.to_bytes()
    );

    server.abort();
    fs::remove_file(&socket_path).unwrap();
}

/// W29-D parity gate: the kill/throttle/reclaim command surface compiles
/// from direct construction, NL sentences (EN/ZH/synonyms), and the CLI
/// into the same wire command, and all three dispatch paths answer with
/// byte-identical typed receipts through the wired executor seam (B5-1
/// second half + B5-2; real authority executors are covered feature-gated
/// in `operation_executor_authorities`).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn kill_throttle_reclaim_commands_are_byte_identical_across_nl_cli_and_direct_paths() {
    use nlos_system_control::control::dispatch_over_socket;
    use nlos_system_control::nl::{
        NL_KILL_REASON, NL_RECLAIM_REASON, NL_THROTTLE_REASON, parse_nl_command,
    };

    use socket_harness::{
        assert_in_process_socket_and_cli_parity, assert_nl_socket_and_in_process_parity,
        bind_socket, run_cli, serve_forever_with_executor,
    };

    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(authority.as_ref());
    let socket_path = database.path.with_extension("sock");
    let listener = bind_socket(&socket_path);
    let server = serve_forever_with_executor(
        listener,
        Arc::clone(&authority),
        health(&plan_id),
        Some(Arc::new(DeterministicOperationExecutor)),
    );
    let stub_health = health(&plan_id);
    let control = RecoverySystemControl::new(authority.as_ref(), &stub_health, &CapabilityPolicy)
        .with_operation_executor(&DeterministicOperationExecutor);

    let target_hex = hex(&OPERATION_TARGET_ID);
    let throttle_percent = 50_u64;
    for (label, reason, nl_sentences, cli_args, expected_receipt, expected_outcome) in [
        (
            "kill",
            NL_KILL_REASON,
            vec![
                format!("kill operation {target_hex} expecting {OPERATION_CAS}"),
                format!("terminate operation {target_hex} expecting {OPERATION_CAS}"),
                format!("终止操作 {target_hex} 期望 {OPERATION_CAS}"),
                format!("终止 操作 {target_hex} 期望 {OPERATION_CAS}"),
            ],
            vec![
                "kill-operation".to_owned(),
                target_hex.clone(),
                target_hex.clone(),
                OPERATION_CAS.to_string(),
                NL_KILL_REASON.to_owned(),
            ],
            operation_receipt(4),
            11_u8,
        ),
        (
            "throttle",
            NL_THROTTLE_REASON,
            vec![
                format!(
                    "throttle operation {target_hex} to {throttle_percent} percent expecting {OPERATION_CAS}"
                ),
                format!("限流操作 {target_hex} 到 {throttle_percent} 百分比 期望 {OPERATION_CAS}"),
                format!("限流 操作 {target_hex} 到 {throttle_percent} 百分比 期望 {OPERATION_CAS}"),
            ],
            vec![
                "throttle-operation".to_owned(),
                target_hex.clone(),
                target_hex.clone(),
                throttle_percent.to_string(),
                OPERATION_CAS.to_string(),
                NL_THROTTLE_REASON.to_owned(),
            ],
            operation_receipt(5),
            12_u8,
        ),
        (
            "reclaim",
            NL_RECLAIM_REASON,
            vec![
                format!("reclaim operation {target_hex} expecting {OPERATION_CAS}"),
                format!("回收操作 {target_hex} 期望 {OPERATION_CAS}"),
                format!("回收 操作 {target_hex} 期望 {OPERATION_CAS}"),
            ],
            vec![
                "reclaim-operation".to_owned(),
                target_hex.clone(),
                target_hex.clone(),
                OPERATION_CAS.to_string(),
                NL_RECLAIM_REASON.to_owned(),
            ],
            operation_receipt(6),
            13_u8,
        ),
    ] {
        let operation = match label {
            "kill" => ControlCommand::KillOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: reason.to_owned(),
            },
            "throttle" => ControlCommand::ThrottleOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                throttle_percent,
                reason: reason.to_owned(),
            },
            _ => ControlCommand::ReclaimOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: reason.to_owned(),
            },
        };
        for sentence in &nl_sentences {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                operation,
                "{label} sentence {sentence:?}"
            );
        }
        let nl_commands: Vec<ControlCommand> = nl_sentences
            .iter()
            .map(|sentence| parse_nl_command(sentence).unwrap())
            .collect();
        assert_nl_socket_and_in_process_parity(
            &socket_path,
            &control,
            &operation,
            &nl_commands,
            None,
            None,
        )
        .await;
        let cli_reference: Vec<&str> = cli_args.iter().map(String::as_str).collect();
        assert_in_process_socket_and_cli_parity(
            &socket_path,
            &control,
            &operation,
            &cli_reference,
            None,
            None,
        )
        .await;
        let receipt = dispatch_over_socket(&socket_path, &operation, None, None)
            .await
            .unwrap();
        match receipt.outcome.as_ref().unwrap() {
            ControlOutcome::OperationKilled { receipt_id }
            | ControlOutcome::OperationThrottled { receipt_id }
            | ControlOutcome::OperationReclaimed { receipt_id } => {
                assert_eq!(receipt_id, &expected_receipt.to_vec());
            }
            other => panic!("expected a W29-D operation-level outcome, got {other:?}"),
        }
        // Deterministic encoding: 16 command id bytes, u32 length prefix,
        // 16 correlation bytes, then the outcome tag byte.
        assert_eq!(receipt.to_bytes()[16 + 4 + 16], expected_outcome);
    }

    // The out-of-domain throttle percent refuses before the wire on the CLI
    // surface too.
    let refused = run_cli(
        &socket_path,
        &[
            "throttle-operation",
            &target_hex,
            &target_hex,
            "101",
            &OPERATION_CAS.to_string(),
            "operator throttles the reservation demand",
        ],
    );
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("usage:"));

    server.abort();
    fs::remove_file(&socket_path).unwrap();
}

/// W28-C-3b (ADR-0017 G8) parity gate: the resource recovery command
/// surface — domain health/metrics reads and the escalated-plan
/// acknowledge/resume mutations — compiles from direct construction, NL
/// sentences (EN/ZH/synonyms), and the CLI into the same wire command, and
/// all three dispatch paths answer with byte-identical typed receipts over
/// the real `task_resource_recovery` ledger.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn resource_recovery_commands_are_byte_identical_across_nl_cli_and_direct_paths() {
    use nlos_system_control::control::dispatch_over_socket;
    use nlos_system_control::nl::{
        NL_RESOURCE_ACK_REASON, NL_RESOURCE_RESUME_REASON, parse_nl_command,
    };
    use nlos_system_control::resource_recovery_resume_reference;

    use socket_harness::{
        assert_in_process_socket_and_cli_parity, assert_nl_socket_and_in_process_parity,
        bind_socket, cli_receipt_bytes, run_cli, serve_forever,
    };

    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let plan_id = create_escalated_plan(authority.as_ref());
    seed_escalated_resource_recovery(&database);
    let resource_plan = nlos_task::ResourceCommitPlanId::from_bytes(RESOURCE_PLAN_ID);
    let socket_path = database.path.with_extension("sock");
    let listener = bind_socket(&socket_path);
    let server = serve_forever(listener, Arc::clone(&authority), health(&plan_id));
    let stub_health = health(&plan_id);
    let control = RecoverySystemControl::new(authority.as_ref(), &stub_health, &CapabilityPolicy);

    // Inspect: NL sentences (EN canonical/synonyms, ZH) compile to the same
    // domain read, so every dispatch answers with byte-identical receipts.
    let nl_inspect = parse_nl_command("inspect resource recovery").unwrap();
    assert_eq!(nl_inspect, ControlCommand::InspectResourceHealth);
    let nl_inspect_zh = parse_nl_command("查看资源恢复").unwrap();
    assert_eq!(nl_inspect_zh, ControlCommand::InspectResourceHealth);
    let nl_inspect_status = parse_nl_command("resource recovery status").unwrap();
    assert_eq!(nl_inspect_status, ControlCommand::InspectResourceHealth);
    let inspection = dispatch_in_process(
        &control,
        &ControlCommand::InspectResourceHealth,
        MONOTONIC_NOW_NS,
        WALL_NOW_MS,
        None,
        None,
    )
    .unwrap();
    let ControlOutcome::ResourceRecoveryInspected(resource_inspection) =
        inspection.outcome.as_ref().unwrap()
    else {
        panic!("expected resource recovery inspection receipt");
    };
    assert_eq!(resource_inspection.alerts.len(), 1);
    assert_eq!(resource_inspection.alerts[0].plan_id, RESOURCE_PLAN_ID);
    assert_eq!(resource_inspection.durable_escalated, 1);
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectResourceHealth,
        &[nl_inspect, nl_inspect_zh, nl_inspect_status],
        None,
        None,
    )
    .await;
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectResourceHealth,
        &["inspect-resource-health"],
        None,
        None,
    )
    .await;

    // Export: the resource-domain metrics projection renders the
    // `nlos_resource_recovery_*` catalog through the same paths.
    let nl_export = parse_nl_command("export resource metrics").unwrap();
    assert_eq!(nl_export, ControlCommand::ExportResourceMetrics);
    assert_eq!(
        parse_nl_command("导出资源指标").unwrap(),
        ControlCommand::ExportResourceMetrics
    );
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::ExportResourceMetrics,
        &[nl_export],
        None,
        None,
    )
    .await;
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::ExportResourceMetrics,
        &["export-resource-metrics"],
        None,
        None,
    )
    .await;
    let export_receipt = dispatch_over_socket(
        &socket_path,
        &ControlCommand::ExportResourceMetrics,
        None,
        None,
    )
    .await
    .unwrap();
    let ControlOutcome::MetricsExported(export) = export_receipt.outcome.as_ref().unwrap() else {
        panic!("expected metrics export receipt");
    };
    assert!(
        export
            .openmetrics_text
            .contains("nlos_resource_recovery_durable_escalated")
    );

    // Acknowledge: EN/ZH sentences and the CLI compile to the same
    // fully-determined mutation the direct construction spells out, and the
    // idempotent replay answers with the same receipt bytes.
    let direct_ack = ControlCommand::AcknowledgeResourceRecoveryAlert {
        control_command_id: RESOURCE_ACK_COMMAND_ID,
        plan_id: RESOURCE_PLAN_ID,
        expected_total_failures: RESOURCE_TOTAL_FAILURES,
        reason: RESOURCE_REASON.to_owned(),
    };
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &direct_ack,
        &[
            "ack-resource-recovery-alert",
            &hex(&RESOURCE_ACK_COMMAND_ID),
            &hex(&RESOURCE_PLAN_ID),
            &RESOURCE_TOTAL_FAILURES.to_string(),
            RESOURCE_REASON,
        ],
        None,
        None,
    )
    .await;
    let nl_ack = parse_nl_command(&format!(
        "acknowledge resource alert {} expecting {RESOURCE_TOTAL_FAILURES}",
        hex(&RESOURCE_PLAN_ID)
    ))
    .unwrap();
    assert_eq!(
        nl_ack,
        ControlCommand::AcknowledgeResourceRecoveryAlert {
            control_command_id: RESOURCE_PLAN_ID,
            plan_id: RESOURCE_PLAN_ID,
            expected_total_failures: RESOURCE_TOTAL_FAILURES,
            reason: NL_RESOURCE_ACK_REASON.to_owned(),
        }
    );
    let nl_ack_zh = parse_nl_command(&format!(
        "确认资源告警 {} 期望 {RESOURCE_TOTAL_FAILURES}",
        hex(&RESOURCE_PLAN_ID)
    ))
    .unwrap();
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &nl_ack,
        &[nl_ack.clone(), nl_ack_zh],
        None,
        None,
    )
    .await;
    assert!(
        authority
            .list_resource_recovery_alerts()
            .unwrap()
            .first()
            .unwrap()
            .acknowledgement
            .is_some()
    );

    // Resume: the ledger row returns to `Retrying`; the receipt names the
    // deterministic domain-separated resume outcome, and the re-armed CLI
    // dispatch of the same command answers byte-identically.
    let resume = ControlCommand::ResumeResourceRecovery {
        control_command_id: RESOURCE_RESUME_COMMAND_ID,
        plan_id: RESOURCE_PLAN_ID,
        expected_total_failures: RESOURCE_TOTAL_FAILURES,
        reason: RESOURCE_REASON.to_owned(),
    };
    let resume_reference =
        dispatch_in_process(&control, &resume, MONOTONIC_NOW_NS, WALL_NOW_MS, None, None).unwrap();
    let ControlOutcome::Resumed { receipt_id } = resume_reference.outcome.as_ref().unwrap() else {
        panic!("expected resumed receipt");
    };
    assert_eq!(
        receipt_id,
        &resource_recovery_resume_reference(resource_plan, RESOURCE_TOTAL_FAILURES)
            .as_bytes()
            .to_vec()
    );
    assert_eq!(
        authority
            .inspect_resource_recovery(resource_plan)
            .unwrap()
            .unwrap()
            .state,
        nlos_task::ResourceRecoveryState::Retrying
    );

    let nl_resume = parse_nl_command(&format!(
        "resume resource recovery {} expecting {RESOURCE_TOTAL_FAILURES}",
        hex(&RESOURCE_PLAN_ID)
    ))
    .unwrap();
    assert_eq!(
        nl_resume,
        ControlCommand::ResumeResourceRecovery {
            control_command_id: RESOURCE_PLAN_ID,
            plan_id: RESOURCE_PLAN_ID,
            expected_total_failures: RESOURCE_TOTAL_FAILURES,
            reason: NL_RESOURCE_RESUME_REASON.to_owned(),
        }
    );

    reset_escalated_resource_recovery(&database);
    let nl_resume_receipt = dispatch_over_socket(&socket_path, &nl_resume, None, None)
        .await
        .unwrap();
    let ControlOutcome::Resumed { receipt_id } = nl_resume_receipt.outcome.as_ref().unwrap() else {
        panic!("expected NL resumed receipt");
    };
    assert_eq!(
        receipt_id,
        &resource_recovery_resume_reference(resource_plan, RESOURCE_TOTAL_FAILURES)
            .as_bytes()
            .to_vec()
    );

    reset_escalated_resource_recovery(&database);
    let cli_resume = run_cli(
        &socket_path,
        &[
            "resume-resource-recovery",
            &hex(&RESOURCE_RESUME_COMMAND_ID),
            &hex(&RESOURCE_PLAN_ID),
            &RESOURCE_TOTAL_FAILURES.to_string(),
            RESOURCE_REASON,
        ],
    );
    assert!(
        cli_resume.status.success(),
        "cli resource resume failed: code={:?} stdout={} stderr={}",
        cli_resume.status.code(),
        String::from_utf8_lossy(&cli_resume.stdout),
        String::from_utf8_lossy(&cli_resume.stderr),
    );
    assert_eq!(cli_receipt_bytes(&cli_resume), resume_reference.to_bytes());
    assert_eq!(
        authority
            .inspect_resource_recovery(resource_plan)
            .unwrap()
            .unwrap()
            .state,
        nlos_task::ResourceRecoveryState::Retrying
    );

    server.abort();
    fs::remove_file(&socket_path).unwrap();
}

// W32-G (B5-3): per-layer inspect parity — the four source-backed views and
// the TaskAuthority-owned TaskGroup view produce byte-identical receipts
// across direct construction, the restricted NL surface, and the CLI.

#[cfg(unix)]
struct StubLayerSources {
    node: nlos_system_control::control::TaskNodeInspection,
    fiber: nlos_system_control::control::ExecutionFiberInspection,
    topic: nlos_system_control::control::TopicInspection,
    operation: nlos_system_control::control::DurableOperationInspection,
}

#[cfg(unix)]
impl nlos_system_control::TaskNodeInspectSource for StubLayerSources {
    fn inspect_task_node(
        &self,
        plan_id: [u8; 16],
        node_id: [u8; 16],
    ) -> Result<nlos_system_control::control::TaskNodeInspection, SabiFailure> {
        if self.node.plan_id == plan_id && self.node.node_id == node_id {
            Ok(self.node.clone())
        } else {
            Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: nlos_schema::sabi::v1::RetryDirective::DoNotRetry.into(),
                safe_message: "requested task node was not found".to_owned(),
            })
        }
    }
}

#[cfg(unix)]
impl nlos_system_control::ExecutionFiberInspectSource for StubLayerSources {
    fn inspect_execution_fiber(
        &self,
        fiber_id: [u8; 16],
        generation: u64,
    ) -> Result<nlos_system_control::control::ExecutionFiberInspection, SabiFailure> {
        if self.fiber.fiber_id == fiber_id && self.fiber.generation == generation {
            Ok(self.fiber.clone())
        } else {
            Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: nlos_schema::sabi::v1::RetryDirective::DoNotRetry.into(),
                safe_message: "requested execution fiber handle was not found".to_owned(),
            })
        }
    }
}

#[cfg(unix)]
impl nlos_system_control::TopicInspectSource for StubLayerSources {
    fn inspect_topic(
        &self,
        topic_id: [u8; 16],
    ) -> Result<nlos_system_control::control::TopicInspection, SabiFailure> {
        if self.topic.topic_id == topic_id {
            Ok(self.topic.clone())
        } else {
            Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: nlos_schema::sabi::v1::RetryDirective::DoNotRetry.into(),
                safe_message: "requested topic was not found".to_owned(),
            })
        }
    }
}

#[cfg(unix)]
impl nlos_system_control::OperationInspectSource for StubLayerSources {
    fn inspect_operation(
        &self,
        operation_id: [u8; 16],
        generation: u64,
    ) -> Result<nlos_system_control::control::DurableOperationInspection, SabiFailure> {
        if self.operation.operation_id == operation_id && self.operation.generation == generation {
            Ok(self.operation.clone())
        } else {
            Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: nlos_schema::sabi::v1::RetryDirective::DoNotRetry.into(),
                safe_message: "requested operation row was not found".to_owned(),
            })
        }
    }
}

#[cfg(unix)]
fn stub_layer_sources() -> StubLayerSources {
    use nlos_schema::sabi::v1::{
        ContextResidencyTier, DurableOperationState, ExecutionFiberLifecycleState,
        ExecutionFiberPhase, PlanNodeKind, PlanNodeLifecycleState,
    };
    StubLayerSources {
        node: nlos_system_control::control::TaskNodeInspection {
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
        },
        fiber: nlos_system_control::control::ExecutionFiberInspection {
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
        },
        topic: nlos_system_control::control::TopicInspection {
            topic_id: [0xC1; 16],
            channel_id: [0xC2; 16],
            channel_generation: 5,
            name: b"stage-b/inspect".to_vec(),
            active_subscriptions: 2,
            policy_digest: vec![0xC3; 32],
            created_at_ms: 3_000,
        },
        operation: nlos_system_control::control::DurableOperationInspection {
            operation_id: [0xD1; 16],
            generation: 1,
            state: DurableOperationState::Dispatched,
            cancel_epoch: 0,
            owner_fiber_id: [0xB1; 16],
            owner_fiber_generation: 2,
            outcome_receipt_id: None,
        },
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn w32g_layer_reads_are_byte_identical_across_nl_cli_and_direct_paths() {
    use socket_harness::{
        assert_in_process_socket_and_cli_parity, assert_nl_socket_and_in_process_parity,
        bind_socket, serve_forever_with_backends,
    };

    let database = Arc::new(TestDatabase::new());
    let authority = Arc::new(database.open());
    let group_id = w32g_group_fixture(authority.as_ref());
    let plan_id = create_escalated_plan(authority.as_ref());
    let stub_health = health(&plan_id);
    let sources = Arc::new(stub_layer_sources());
    let socket_path = database.path.with_extension("sock");
    let listener = bind_socket(&socket_path);
    let server = serve_forever_with_backends(
        listener,
        Arc::clone(&authority),
        health(&plan_id),
        None,
        Some(Arc::clone(&sources)),
    );
    let control = RecoverySystemControl::new(authority.as_ref(), &stub_health, &CapabilityPolicy)
        .with_task_node_source(sources.as_ref())
        .with_execution_fiber_source(sources.as_ref())
        .with_topic_source(sources.as_ref())
        .with_operation_source(sources.as_ref());

    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectTaskGroup {
            group_id: *group_id.as_bytes(),
        },
        &["inspect-task-group", &hex(group_id.as_bytes())],
        None,
        None,
    )
    .await;
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectTaskGroup {
            group_id: *group_id.as_bytes(),
        },
        &[
            nlos_system_control::nl::parse_nl_command(&format!(
                "查看任务组 {}",
                hex(group_id.as_bytes())
            ))
            .unwrap(),
            nlos_system_control::nl::parse_nl_command(&format!(
                "task group status {}",
                hex(group_id.as_bytes())
            ))
            .unwrap(),
        ],
        None,
        None,
    )
    .await;

    let node_hex_a1 = hex(&[0xA1; 16]);
    let node_hex_a2 = hex(&[0xA2; 16]);
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectTaskNode {
            plan_id: [0xA1; 16],
            node_id: [0xA2; 16],
        },
        &["inspect-task-node", &node_hex_a1, &node_hex_a2],
        None,
        None,
    )
    .await;
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectTaskNode {
            plan_id: [0xA1; 16],
            node_id: [0xA2; 16],
        },
        &[
            nlos_system_control::nl::parse_nl_command(&format!(
                "查看任务节点 {node_hex_a1} {node_hex_a2}"
            ))
            .unwrap(),
            nlos_system_control::nl::parse_nl_command(&format!(
                "inspect task node {node_hex_a1} {node_hex_a2}"
            ))
            .unwrap(),
        ],
        None,
        None,
    )
    .await;

    let fiber_hex = hex(&[0xB1; 16]);
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectExecutionFiber {
            fiber_id: [0xB1; 16],
            generation: 2,
        },
        &["inspect-fiber", &fiber_hex, "2"],
        None,
        None,
    )
    .await;
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectExecutionFiber {
            fiber_id: [0xB1; 16],
            generation: 2,
        },
        &[
            nlos_system_control::nl::parse_nl_command(&format!("查看纤程 {fiber_hex} 世代 2"))
                .unwrap(),
            nlos_system_control::nl::parse_nl_command(&format!(
                "fiber status {fiber_hex} generation 2"
            ))
            .unwrap(),
        ],
        None,
        None,
    )
    .await;

    let topic_hex = hex(&[0xC1; 16]);
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectTopic {
            topic_id: [0xC1; 16],
        },
        &["inspect-topic", &topic_hex],
        None,
        None,
    )
    .await;
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectTopic {
            topic_id: [0xC1; 16],
        },
        &[
            nlos_system_control::nl::parse_nl_command(&format!("查看主题 {topic_hex}")).unwrap(),
            nlos_system_control::nl::parse_nl_command(&format!("topic status {topic_hex}"))
                .unwrap(),
        ],
        None,
        None,
    )
    .await;

    let operation_hex = hex(&[0xD1; 16]);
    assert_in_process_socket_and_cli_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectOperation {
            operation_id: [0xD1; 16],
            generation: 1,
        },
        &["inspect-operation", &operation_hex, "1"],
        None,
        None,
    )
    .await;
    assert_nl_socket_and_in_process_parity(
        &socket_path,
        &control,
        &ControlCommand::InspectOperation {
            operation_id: [0xD1; 16],
            generation: 1,
        },
        &[
            nlos_system_control::nl::parse_nl_command(&format!("查看操作 {operation_hex} 世代 1"))
                .unwrap(),
            nlos_system_control::nl::parse_nl_command(&format!(
                "operation status {operation_hex} generation 1"
            ))
            .unwrap(),
        ],
        None,
        None,
    )
    .await;

    server.abort();
}
