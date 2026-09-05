//! Acceptance tests for B-PROCESS-003 batch cancel propagation: immutable
//! fiber incarnation cancel receipts, fail-closed inspect/resume gates, and
//! idempotent replay (process-domain cancel, not platform kill).

use nlos_process::{
    CreateIsolationDomainRequest, FiberCancelPropagationDecision, IsolationDomainDecision,
    MarkProcessTerminatedRequest, ProcessAuthority, ProcessAuthorityError, ProcessBindingDecision,
    ProcessLifecycleState, ProcessTerminalDecision, PropagateCancelToFibersRequest,
    PropagateCrashRequest, RegisterDelegatedProcessRequest, RegisterFiberIncarnationRequest,
    WriteFiberEntrySnapshotRequest,
};
use nlos_types::{ExecutionFiberId, Generation, IdempotencyKey, ProcessId, TaskAttemptId, TaskId};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-process-cancel-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    authority: ProcessAuthority,
    process_id: ProcessId,
    process_generation: Generation,
    process_fencing_token: nlos_process::FencingToken,
}

fn open_fixture(root: &TestRoot, seed: u8) -> Fixture {
    let authority = ProcessAuthority::open(root.path()).expect("open authority");
    let domain = match authority.create_isolation_domain(CreateIsolationDomainRequest {
        policy_digest: [seed; 32],
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
        created_at_ms: 1_000,
    }) {
        Ok(
            IsolationDomainDecision::Created(record) | IsolationDomainDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("domain: {error}"),
    };
    let binding = match authority.register_delegated_process(RegisterDelegatedProcessRequest {
        task_id: TaskId::from_bytes([seed.wrapping_add(2); 16]),
        task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(3); 16]),
        attempt_generation: Generation::INITIAL,
        isolation_domain_id: domain.isolation_domain_id,
        isolation_domain_generation: domain.generation,
        isolation_domain_fencing_token: domain.fencing_token,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(4); 16]),
        created_at_ms: 2_000,
    }) {
        Ok(
            ProcessBindingDecision::Registered(record) | ProcessBindingDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("register: {error}"),
    };
    Fixture {
        authority,
        process_id: binding.process_id,
        process_generation: binding.process_generation,
        process_fencing_token: binding.process_fencing_token,
    }
}

fn register_binding(
    fixture: &Fixture,
    binding_byte: u8,
    idempotency_byte: u8,
    at_ms: u64,
) -> ExecutionFiberId {
    let binding = ExecutionFiberId::from_bytes([binding_byte; 16]);
    fixture
        .authority
        .register_fiber_incarnation(RegisterFiberIncarnationRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            binding,
            idempotency_key: IdempotencyKey::from_bytes([idempotency_byte; 16]),
            registered_at_ms: at_ms,
        })
        .expect("register incarnation");
    binding
}

#[test]
fn terminal_propagation_batch_cancels_incarnations_and_inspect_fail_closed() {
    let root = TestRoot::new("batch-cancel");
    let fixture = open_fixture(&root, 60);
    let binding_a = register_binding(&fixture, 0xA1, 0x01, 3_000);
    let binding_b = register_binding(&fixture, 0xB2, 0x02, 3_100);
    let cancel_key = IdempotencyKey::from_bytes([0xCC; 16]);

    let terminal = fixture
        .authority
        .mark_process_terminated(MarkProcessTerminatedRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: cancel_key,
            marked_at_ms: 8_000,
        })
        .expect("terminate");
    assert!(matches!(terminal, ProcessTerminalDecision::Marked(_)));

    let receipts = fixture
        .authority
        .propagate_cancel_to_fibers(PropagateCancelToFibersRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            lifecycle_state: ProcessLifecycleState::Terminated,
            idempotency_key: cancel_key,
            cancelled_at_ms: 8_000,
        })
        .expect("replay cancel batch");
    assert!(matches!(
        receipts,
        FiberCancelPropagationDecision::Replayed(_)
    ));
    assert_eq!(receipts.receipts().len(), 2);

    for binding in [binding_a, binding_b] {
        assert!(matches!(
            fixture
                .authority
                .inspect_fiber_incarnation(fixture.process_id, binding),
            Err(ProcessAuthorityError::FiberIncarnationCancelled(
                ProcessLifecycleState::Terminated
            ))
        ));
        assert!(
            fixture
                .authority
                .inspect_fiber_incarnation_cancel_receipt(
                    fixture.process_id,
                    fixture.process_generation,
                    binding,
                )
                .expect("inspect receipt")
                .is_some()
        );
    }
}

#[test]
fn crash_path_links_cancel_receipts_and_replays_across_restart() {
    let root = TestRoot::new("crash-cancel-replay");
    let crash_key = IdempotencyKey::from_bytes([0xD0; 16]);
    let binding = {
        let fixture = open_fixture(&root, 70);
        register_binding(&fixture, 0xC3, 0x03, 4_000);
        fixture
            .authority
            .propagate_crash(PropagateCrashRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: fixture.process_fencing_token,
                idempotency_key: crash_key,
                marked_at_ms: 9_000,
            })
            .expect("crash");
        assert!(matches!(
            fixture.authority.inspect_fiber_incarnation(
                fixture.process_id,
                ExecutionFiberId::from_bytes([0xC3; 16])
            ),
            Err(ProcessAuthorityError::FiberIncarnationCancelled(
                ProcessLifecycleState::Crashed
            ))
        ));
        (
            fixture.process_id,
            fixture.process_generation,
            fixture.process_fencing_token,
            ExecutionFiberId::from_bytes([0xC3; 16]),
        )
    };

    let reopened = ProcessAuthority::open(root.path()).expect("reopen");
    let replay = reopened
        .propagate_cancel_to_fibers(PropagateCancelToFibersRequest {
            process_id: binding.0,
            expected_process_generation: binding.1,
            expected_process_fencing_token: binding.2,
            lifecycle_state: ProcessLifecycleState::Crashed,
            idempotency_key: crash_key,
            cancelled_at_ms: 9_000,
        })
        .expect("reopen replay");
    assert!(matches!(
        replay,
        FiberCancelPropagationDecision::Replayed(_)
    ));
    assert_eq!(replay.receipts().len(), 1);
    assert_eq!(replay.receipts()[0].binding, binding.3);
}

#[test]
fn stale_fence_and_idempotency_rebinding_fail_closed_on_cancel_propagation() {
    let root = TestRoot::new("cancel-conflicts");
    let fixture = open_fixture(&root, 80);
    register_binding(&fixture, 0xE4, 0x04, 5_000);
    let batch_key = IdempotencyKey::from_bytes([0xE0; 16]);

    fixture
        .authority
        .propagate_crash(PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: batch_key,
            marked_at_ms: 10_000,
        })
        .expect("crash");

    assert!(matches!(
        fixture
            .authority
            .propagate_cancel_to_fibers(PropagateCancelToFibersRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: [0xAB; 32],
                lifecycle_state: ProcessLifecycleState::Crashed,
                idempotency_key: IdempotencyKey::from_bytes([0xE1; 16]),
                cancelled_at_ms: 10_001,
            }),
        Err(ProcessAuthorityError::StaleProcessBinding)
    ));

    assert!(matches!(
        fixture
            .authority
            .propagate_cancel_to_fibers(PropagateCancelToFibersRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: fixture.process_fencing_token,
                lifecycle_state: ProcessLifecycleState::Terminated,
                idempotency_key: IdempotencyKey::from_bytes([0xE2; 16]),
                cancelled_at_ms: 10_002,
            }),
        Err(ProcessAuthorityError::IdempotencyConflict)
    ));

    assert!(matches!(
        fixture
            .authority
            .propagate_cancel_to_fibers(PropagateCancelToFibersRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: fixture.process_fencing_token,
                lifecycle_state: ProcessLifecycleState::Crashed,
                idempotency_key: batch_key,
                cancelled_at_ms: 10_003,
            }),
        Ok(FiberCancelPropagationDecision::Replayed(_))
    ));

    assert!(matches!(
        fixture
            .authority
            .write_fiber_entry_snapshot(WriteFiberEntrySnapshotRequest {
                process_id: fixture.process_id,
                binding: ExecutionFiberId::from_bytes([0xE4; 16]),
                expected_incarnation_generation: Generation::INITIAL,
                handler_input: b"after-cancel".to_vec(),
                written_at_ms: 10_004,
            }),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed
        ))
    ));
}
