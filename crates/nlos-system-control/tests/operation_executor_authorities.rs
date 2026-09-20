//! W29-D authority-backed executor tests: each operation executor drives
//! its real authority and produces a deterministic receipt id derived from
//! the authority's own facts, refusing fail-closed on CAS mismatches,
//! absent authority objects, and out-of-domain input.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_schema::sabi::v1::{CallerIdentity, CapabilityHandle, SabiRequestContext, envelope};
use nlos_schema::{
    SABI_ENVELOPE_SCHEMA, encode_submit_control_command_request, system_control_schema_identity,
};
use nlos_system_control::control::{ControlCommand, ControlOutcome, dispatch_in_process};
use nlos_system_control::{
    RecoveryHealthSource, RecoverySystemControl, SUBMIT_METHOD, SYSTEM_CONTROL_SERVICE,
    SystemControlAuthorizer,
};
use nlos_task::SqliteTaskAuthority;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);
impl TempDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nlos-sc-w29d-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct TestDatabase {
    path: PathBuf,
    _root: TempDir,
}
impl TestDatabase {
    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).unwrap()
    }

    fn new() -> Self {
        let root = TempDir::new("taskdb");
        Self {
            path: root.0.join("task-authority.sqlite3"),
            _root: root,
        }
    }
}
impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
    }
}

struct StubHealth(RecoveryWorkerHealth);
impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.0.clone()
    }
}

struct CapabilityPolicy;
impl SystemControlAuthorizer for CapabilityPolicy {
    fn authorize_get(
        &self,
        _: &SabiRequestContext,
        _: &nlos_schema::sabi::v1::GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }
    fn authorize_submit(
        &self,
        _: &SabiRequestContext,
        _: &nlos_schema::sabi::v1::ControlCommand,
    ) -> Result<(), &'static str> {
        Ok(())
    }
}

fn stub_health() -> StubHealth {
    StubHealth(RecoveryWorkerHealth {
        state: RecoveryWorkerState::Running,
        ..RecoveryWorkerHealth::default()
    })
}

const MONOTONIC_NOW_NS: u64 = 10;
const WALL_NOW_MS: i64 = 6_000;

/// Dispatches one `ControlCommand` end-to-end through the shared handler
/// with the given executor wired, mirroring the production submit path.
fn dispatch<E: nlos_system_control::OperationCommandExecutor>(
    tasks: &SqliteTaskAuthority,
    executor: &E,
    command: &ControlCommand,
) -> nlos_system_control::control::ControlReceipt {
    let health = stub_health();
    let control = RecoverySystemControl::new(tasks, &health, &CapabilityPolicy)
        .with_operation_executor(executor);
    dispatch_in_process(&control, command, MONOTONIC_NOW_NS, WALL_NOW_MS, None, None).unwrap()
}

fn operation_request(
    target_id: [u8; 16],
    expected_generation_or_revision: u64,
) -> nlos_system_control::OperationControlRequest {
    nlos_system_control::OperationControlRequest {
        target_id,
        expected_generation_or_revision,
        issuer_principal_id: [0x31; 16],
        idempotency_key: target_id,
        requested_at_ms: WALL_NOW_MS,
    }
}

#[allow(dead_code)]
fn submit_envelope(
    arm: nlos_schema::sabi::v1::control_command::Command,
    target_id: [u8; 16],
    cas: u64,
) -> nlos_schema::sabi::v1::Envelope {
    let submit = nlos_schema::sabi::v1::SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(nlos_schema::sabi::v1::ControlCommand {
            control_command_id: target_id.to_vec(),
            issuer_principal_id: vec![0x31; 16],
            source: nlos_schema::sabi::v1::ControlCommandSource::Cli.into(),
            scope: nlos_schema::sabi::v1::ControlScope::Operation.into(),
            target_id: target_id.to_vec(),
            expected_generation_or_revision: cas,
            command: Some(arm),
            reason: "operator drives the authority".to_owned(),
        }),
    };
    let context = SabiRequestContext {
        caller: Some(CallerIdentity {
            principal_id: vec![0x31; 16],
            application_id: vec![0x32; 16],
            process_id: vec![0x33; 16],
            process_generation: 1,
        }),
        activity_context: Vec::new(),
        task_execution_binding: None,
        correlation_id: target_id.to_vec(),
        idempotency_key: target_id.to_vec(),
        deadline_monotonic_ns: 0,
        capability_handles: vec![CapabilityHandle {
            slot: 9,
            generation: 1,
        }],
        reservation_handle: None,
        proposal_or_input_digest_sha256: Vec::new(),
    };
    nlos_schema::sabi::v1::Envelope {
        schema: Some(nlos_schema::sabi::v1::SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x35; 16],
        service: SYSTEM_CONTROL_SERVICE.to_owned(),
        method: SUBMIT_METHOD.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(context)),
        payload: encode_submit_control_command_request(&submit).unwrap(),
    }
}

#[cfg(feature = "process")]
mod kill {
    use super::*;
    use nlos_process::{
        CreateIsolationDomainRequest, ProcessAuthority, RegisterDelegatedProcessRequest,
        RegisterSupervisorPidRequest, StubPlatformKillAdapter, SupervisorPidRegistry,
    };
    use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode};
    use nlos_system_control::OperationCommandExecutor;
    use nlos_system_control::process_kill_executor::ProcessAuthorityKillExecutor;
    use nlos_types::{Generation, IdempotencyKey};
    use nlos_types::{TaskAttemptId, TaskId};

    fn process_fixture(
        authority: &ProcessAuthority,
        seed: u8,
    ) -> nlos_process::ProcessBindingRecord {
        let domain = authority
            .create_isolation_domain(CreateIsolationDomainRequest {
                policy_digest: [seed; 32],
                idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
                created_at_ms: 1_000,
            })
            .unwrap()
            .record()
            .clone();
        authority
            .register_delegated_process(RegisterDelegatedProcessRequest {
                task_id: TaskId::from_bytes([seed; 16]),
                task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(1); 16]),
                attempt_generation: Generation::INITIAL,
                isolation_domain_id: domain.isolation_domain_id,
                isolation_domain_generation: domain.generation,
                isolation_domain_fencing_token: domain.fencing_token,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
                created_at_ms: 1_100,
            })
            .unwrap()
            .record()
            .clone()
    }

    fn wired(
        root: &TempDir,
    ) -> (
        ProcessAuthority,
        SupervisorPidRegistry,
        StubPlatformKillAdapter,
    ) {
        let authority = ProcessAuthority::open(root.0.join("process")).unwrap();
        (
            authority,
            SupervisorPidRegistry::new(),
            StubPlatformKillAdapter::new(),
        )
    }

    #[test]
    fn kill_drives_the_process_authority_and_supervisor_registry() {
        let root = TempDir::new("kill");
        let (authority, registry, adapter) = wired(&root);
        let binding = process_fixture(&authority, 0x41);
        registry
            .register(RegisterSupervisorPidRequest {
                process_id: binding.process_id,
                process_generation: binding.process_generation,
                os_pid: 4242,
                registered_at_ms: 1_200,
            })
            .unwrap();
        let executor = ProcessAuthorityKillExecutor::new(&authority, &registry, &adapter);
        let make_request = || {
            operation_request(
                binding.process_id.into_bytes(),
                binding.process_generation.get(),
            )
        };
        let request = make_request();
        let idempotency_key = request.idempotency_key;

        let receipt_id = executor.kill_operation(request).unwrap();
        assert_eq!(
            adapter.recorded_signals(),
            vec![(binding.process_id, binding.process_generation)],
            "the platform adapter was signaled through the authority path"
        );
        let durable = authority
            .inspect_platform_kill_receipt(binding.process_id, binding.process_generation)
            .unwrap()
            .expect("durable platform kill receipt");
        assert_eq!(durable.idempotency_key.as_bytes(), &idempotency_key);

        // Replay re-derives the identical receipt id from the same durable
        // receipt facts.
        assert_eq!(executor.kill_operation(make_request()).unwrap(), receipt_id);
    }

    #[test]
    fn kill_refuses_cas_mismatch_absent_supervisor_and_negative_wall() {
        let root = TempDir::new("kill-refuse");
        let (authority, registry, adapter) = wired(&root);
        let binding = process_fixture(&authority, 0x51);
        registry
            .register(RegisterSupervisorPidRequest {
                process_id: binding.process_id,
                process_generation: binding.process_generation,
                os_pid: 4243,
                registered_at_ms: 1_200,
            })
            .unwrap();
        let executor = ProcessAuthorityKillExecutor::new(&authority, &registry, &adapter);

        let stale = executor.kill_operation(operation_request(
            binding.process_id.into_bytes(),
            binding.process_generation.get() + 7,
        ));
        let failure = stale.unwrap_err();
        assert_eq!(failure.code, i32::from(SabiErrorCode::Conflict));
        assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
        assert_eq!(
            failure.safe_message,
            "kill CAS mismatch: expected revision is not the process head generation"
        );

        let mut negative = operation_request(
            binding.process_id.into_bytes(),
            binding.process_generation.get(),
        );
        negative.requested_at_ms = -1;
        let failure = executor.kill_operation(negative).unwrap_err();
        assert_eq!(failure.code, i32::from(SabiErrorCode::InvalidArgument));
        assert!(
            authority
                .inspect_platform_kill_receipt(binding.process_id, binding.process_generation)
                .unwrap()
                .is_none(),
            "a rejected kill takes no durable side effect"
        );

        // No supervisor pid mapping registered: fail-closed before the
        // authority is driven.
        let unsupervised = process_fixture(&authority, 0x52);
        let failure = executor
            .kill_operation(operation_request(
                unsupervised.process_id.into_bytes(),
                unsupervised.process_generation.get(),
            ))
            .unwrap_err();
        assert_eq!(failure.code, i32::from(SabiErrorCode::NotFound));
        assert!(
            authority
                .inspect_platform_kill_receipt(
                    unsupervised.process_id,
                    unsupervised.process_generation
                )
                .unwrap()
                .is_none(),
            "a rejected kill takes no durable side effect"
        );
    }

    #[test]
    fn kill_end_to_end_through_the_shared_handler() {
        let root = TempDir::new("kill-e2e");
        let (authority, registry, adapter) = wired(&root);
        let binding = process_fixture(&authority, 0x61);
        registry
            .register(RegisterSupervisorPidRequest {
                process_id: binding.process_id,
                process_generation: binding.process_generation,
                os_pid: 4242,
                registered_at_ms: 1_200,
            })
            .unwrap();
        let executor = ProcessAuthorityKillExecutor::new(&authority, &registry, &adapter);
        let direct = executor
            .kill_operation(operation_request(
                binding.process_id.into_bytes(),
                binding.process_generation.get(),
            ))
            .unwrap();

        let database = TestDatabase::new();
        let tasks = database.open();
        let command = ControlCommand::KillOperation {
            control_command_id: binding.process_id.into_bytes(),
            target_id: binding.process_id.into_bytes(),
            expected_generation_or_revision: binding.process_generation.get(),
            reason: "operator kills the delegated process".to_owned(),
        };
        let receipt = dispatch(&tasks, &executor, &command);
        let ControlOutcome::OperationKilled { receipt_id } = receipt.outcome.unwrap() else {
            panic!("expected an operation-killed receipt");
        };
        assert_eq!(receipt_id, direct.into_bytes().to_vec());
    }
}

#[cfg(feature = "resource")]
mod throttle {
    use super::*;
    use nlos_resource::{
        ActivateReservationRequest, ConsumeReservationRequest, CreateAccountRequest,
        CreateQuoteRequest, RegisterDriverRequest, ReserveRequest, ResourceAuthority,
        ResourceDemand,
    };
    use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode};
    use nlos_system_control::OperationCommandExecutor;
    use nlos_system_control::resource_throttle_executor::ResourceDemandThrottleExecutor;
    use nlos_types::{CallId, IdempotencyKey, OperationId};

    fn demand_fixture(root: &TempDir, seed: u8) -> nlos_resource::ReservationRecord {
        let authority = ResourceAuthority::open(root.0.join("resource")).unwrap();
        let driver = authority
            .register_driver(RegisterDriverRequest {
                profile_digest: [seed; 32],
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
                created_at_ms: 1_000,
            })
            .unwrap()
            .record();
        let account = authority
            .create_account(CreateAccountRequest {
                initial_credit: 1_000,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
                created_at_ms: 1_000,
            })
            .unwrap();
        let capacity = ResourceDemand {
            cpu_shares: 100,
            memory_mib: 1_024,
            io_weight: 10,
        };
        let quote = authority
            .create_quote(CreateQuoteRequest {
                driver_id: driver.driver_id,
                driver_generation: driver.generation,
                driver_fencing_token: driver.fencing_token,
                operation_proposal_digest: [seed.wrapping_add(3); 32],
                pricing_version: [seed.wrapping_add(4); 32],
                upper_bound: 100,
                demand_capacity: capacity,
                valid_until_ms: 10_000,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
                created_at_ms: 1_000,
            })
            .unwrap()
            .record();
        let reservation = authority
            .reserve(ReserveRequest {
                account_id: account.account_id,
                quote_id: quote.quote_id,
                call_id: CallId::from_bytes([seed.wrapping_add(6); 16]),
                operation_id: OperationId::from_bytes([seed.wrapping_add(7); 16]),
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(8); 16]),
                demand: ResourceDemand {
                    cpu_shares: 64,
                    memory_mib: 512,
                    io_weight: 5,
                },
                reserved_at_ms: 2_000,
            })
            .unwrap()
            .record();
        let activation = authority
            .activate(ActivateReservationRequest {
                reservation_id: reservation.reservation_id,
                call_id: reservation.call_id,
                operation_id: reservation.operation_id,
                driver_id: driver.driver_id,
                driver_generation: driver.generation,
                driver_fencing_token: driver.fencing_token,
                activation_token: reservation.activation_token,
                activated_at_ms: 2_500,
            })
            .unwrap()
            .receipt();
        // One consumption bumps `usage_high_water_seq` to 1 so the wire CAS
        // (which rejects zero) can address the reservation revision.
        authority
            .consume(ConsumeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: activation.receipt_id,
                sequence: 1,
                cumulative_usage: 10,
                consumed_at_ms: 3_000,
            })
            .unwrap();
        authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
    }

    #[test]
    fn throttle_drives_the_resource_authority_demand_adjustment() {
        let root = TempDir::new("throttle");
        let authority = ResourceAuthority::open(root.0.join("resource")).unwrap();
        let reservation = demand_fixture(&root, 0x42);
        assert_eq!(reservation.usage_high_water_seq, 1);
        let executor = ResourceDemandThrottleExecutor::new(&authority);
        let make_request = || {
            operation_request(
                reservation.reservation_id.into_bytes(),
                reservation.usage_high_water_seq,
            )
        };

        let receipt_id = executor.throttle_operation(make_request(), 50).unwrap();
        assert_eq!(
            executor.throttle_operation(make_request(), 50).unwrap(),
            receipt_id
        );
        // A different level is a different authority-driven adjustment.
        assert_ne!(
            executor.throttle_operation(make_request(), 25).unwrap(),
            receipt_id
        );
    }

    #[test]
    fn throttle_refuses_cas_mismatch_absent_reservation_and_percent_bound() {
        let root = TempDir::new("throttle-refuse");
        let authority = ResourceAuthority::open(root.0.join("resource")).unwrap();
        let reservation = demand_fixture(&root, 0x52);
        let executor = ResourceDemandThrottleExecutor::new(&authority);

        let stale = executor.throttle_operation(
            operation_request(
                reservation.reservation_id.into_bytes(),
                reservation.usage_high_water_seq + 3,
            ),
            50,
        );
        let failure = stale.unwrap_err();
        assert_eq!(failure.code, i32::from(SabiErrorCode::Conflict));
        assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
        assert_eq!(
            failure.safe_message,
            "throttle CAS mismatch: expected revision is not the reservation usage sequence"
        );

        let absent = executor.throttle_operation(operation_request([0xFF; 16], 1), 50);
        assert_eq!(absent.unwrap_err().code, i32::from(SabiErrorCode::NotFound));

        for percent in [0, 101] {
            let failure = executor
                .throttle_operation(
                    operation_request(
                        reservation.reservation_id.into_bytes(),
                        reservation.usage_high_water_seq,
                    ),
                    percent,
                )
                .unwrap_err();
            assert_eq!(failure.code, i32::from(SabiErrorCode::InvalidArgument));
            assert_eq!(
                failure.safe_message,
                "throttle percent must be a whole percent from 1 to 100"
            );
        }
    }

    #[test]
    fn throttle_end_to_end_through_the_shared_handler() {
        let root = TempDir::new("throttle-e2e");
        let authority = ResourceAuthority::open(root.0.join("resource")).unwrap();
        let reservation = demand_fixture(&root, 0x62);
        let executor = ResourceDemandThrottleExecutor::new(&authority);
        let direct = executor
            .throttle_operation(
                operation_request(
                    reservation.reservation_id.into_bytes(),
                    reservation.usage_high_water_seq,
                ),
                50,
            )
            .unwrap();

        let database = TestDatabase::new();
        let tasks = database.open();
        let command = ControlCommand::ThrottleOperation {
            control_command_id: reservation.reservation_id.into_bytes(),
            target_id: reservation.reservation_id.into_bytes(),
            expected_generation_or_revision: reservation.usage_high_water_seq,
            throttle_percent: 50,
            reason: "operator throttles the reservation demand".to_owned(),
        };
        let receipt = dispatch(&tasks, &executor, &command);
        let ControlOutcome::OperationThrottled { receipt_id } = receipt.outcome.unwrap() else {
            panic!("expected an operation-throttled receipt");
        };
        assert_eq!(receipt_id, direct.into_bytes().to_vec());
    }
}

mod reclaim {
    use super::*;
    use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode};
    use nlos_system_control::OperationCommandExecutor;
    use nlos_system_control::working_set_reclaim_executor::{
        FixedWorkingSetOccupancy, WorkingSetReclaimExecutor,
    };
    use nlos_task::{ScaleProfile, TASK_PROFILE_10K};
    #[test]
    fn reclaim_drives_the_nlos_task_reclaim_entry() {
        let profile: ScaleProfile = TASK_PROFILE_10K;
        let occupancy = FixedWorkingSetOccupancy(500);
        let executor = WorkingSetReclaimExecutor::new(profile, &occupancy);
        let receipt_id = executor
            .reclaim_operation(operation_request([0x71; 16], 500))
            .unwrap();
        assert_eq!(
            executor
                .reclaim_operation(operation_request([0x71; 16], 500))
                .unwrap(),
            receipt_id
        );

        // A different observed occupancy is a different authority-driven
        // execution (different advisory and evicted-unit overshoot).
        let shifted = FixedWorkingSetOccupancy(501);
        let shifted_executor = WorkingSetReclaimExecutor::new(profile, &shifted);
        assert_ne!(
            shifted_executor
                .reclaim_operation(operation_request([0x71; 16], 501))
                .unwrap(),
            receipt_id
        );
    }

    #[test]
    fn reclaim_refuses_below_the_soft_threshold_and_on_a_moved_occupancy() {
        let profile: ScaleProfile = TASK_PROFILE_10K;
        let below = FixedWorkingSetOccupancy(100);
        let executor = WorkingSetReclaimExecutor::new(profile, &below);
        let failure = executor
            .reclaim_operation(operation_request([0x71; 16], 100))
            .unwrap_err();
        assert_eq!(failure.code, i32::from(SabiErrorCode::State));
        assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
        assert_eq!(
            failure.safe_message,
            "working set is below the soft reclaim threshold; nothing to reclaim"
        );

        let moved = FixedWorkingSetOccupancy(500);
        let executor = WorkingSetReclaimExecutor::new(profile, &moved);
        let failure = executor
            .reclaim_operation(operation_request([0x71; 16], 499))
            .unwrap_err();
        assert_eq!(failure.code, i32::from(SabiErrorCode::Conflict));
        assert_eq!(
            failure.safe_message,
            "reclaim CAS mismatch: the observed working-set count moved"
        );
    }

    #[test]
    fn reclaim_end_to_end_through_the_shared_handler() {
        let profile: ScaleProfile = TASK_PROFILE_10K;
        let occupancy = FixedWorkingSetOccupancy(500);
        let executor = WorkingSetReclaimExecutor::new(profile, &occupancy);
        let direct = executor
            .reclaim_operation(operation_request([0x71; 16], 500))
            .unwrap();

        let database = TestDatabase::new();
        let tasks = database.open();
        let command = ControlCommand::ReclaimOperation {
            control_command_id: [0x71; 16],
            target_id: [0x71; 16],
            expected_generation_or_revision: 500,
            reason: "operator reclaims the working set".to_owned(),
        };
        let receipt = dispatch(&tasks, &executor, &command);
        let ControlOutcome::OperationReclaimed { receipt_id } = receipt.outcome.unwrap() else {
            panic!("expected an operation-reclaimed receipt");
        };
        assert_eq!(receipt_id, direct.into_bytes().to_vec());
    }
}
