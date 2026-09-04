//! Acceptance tests for B-PROCESS-003 process crash propagation / terminal
//! lifecycle: durable terminal markers, fiber registration and resume
//! fail-closed, and exact replay across authority reopen.

use nlos_process::{
    CreateIsolationDomainRequest, FiberIncarnationDecision, IsolationDomainDecision,
    MarkProcessTerminatedRequest, ProcessAuthority, ProcessAuthorityError, ProcessBindingDecision,
    ProcessLifecycleState, ProcessTerminalDecision, PropagateCrashRequest,
    RegisterDelegatedProcessRequest, RegisterFiberIncarnationRequest,
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
            "nlos-process-crash-{label}-{}-{nonce}-{sequence}",
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

#[test]
fn propagate_crash_marks_terminal_and_replays_across_restart() {
    let root = TestRoot::new("crash-replay");
    let crash_key = IdempotencyKey::from_bytes([0xC1; 16]);
    let (process_id, generation, token, marked) = {
        let fixture = open_fixture(&root, 10);
        let request = PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: crash_key,
            marked_at_ms: 5_000,
        };
        let first = fixture
            .authority
            .propagate_crash(request)
            .expect("propagate crash");
        assert!(matches!(first, ProcessTerminalDecision::Marked(_)));
        assert_eq!(
            first.record().lifecycle_state,
            ProcessLifecycleState::Crashed
        );
        let replay = fixture
            .authority
            .propagate_crash(request)
            .expect("replay crash");
        assert!(matches!(replay, ProcessTerminalDecision::Replayed(_)));
        assert_eq!(replay.record(), first.record());
        (
            fixture.process_id,
            fixture.process_generation,
            fixture.process_fencing_token,
            first.record().clone(),
        )
    };

    let reopened = ProcessAuthority::open(root.path()).expect("reopen");
    let replay = reopened
        .propagate_crash(PropagateCrashRequest {
            process_id,
            expected_process_generation: generation,
            expected_process_fencing_token: token,
            idempotency_key: crash_key,
            marked_at_ms: 5_000,
        })
        .expect("reopen replay");
    assert!(matches!(replay, ProcessTerminalDecision::Replayed(_)));
    assert_eq!(replay.record(), &marked);
    assert_eq!(
        reopened
            .inspect_process_terminal(process_id)
            .expect("inspect terminal")
            .as_ref(),
        Some(&marked)
    );
}

#[test]
fn mark_process_terminated_is_distinct_from_crash() {
    let root = TestRoot::new("terminated");
    let fixture = open_fixture(&root, 20);
    let marked = fixture
        .authority
        .mark_process_terminated(MarkProcessTerminatedRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xA0; 16]),
            marked_at_ms: 6_000,
        })
        .expect("mark terminated")
        .record()
        .clone();
    assert_eq!(marked.lifecycle_state, ProcessLifecycleState::Terminated);
}

#[test]
fn fiber_registration_and_snapshot_fail_closed_after_terminal() {
    let root = TestRoot::new("fail-closed");
    let fixture = open_fixture(&root, 30);
    let binding = ExecutionFiberId::from_bytes([0xB1; 16]);
    fixture
        .authority
        .register_fiber_incarnation(RegisterFiberIncarnationRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            binding,
            idempotency_key: IdempotencyKey::from_bytes([0xF1; 16]),
            registered_at_ms: 3_000,
        })
        .expect("pre-terminal incarnation");

    fixture
        .authority
        .propagate_crash(PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xC2; 16]),
            marked_at_ms: 7_000,
        })
        .expect("crash");

    assert!(matches!(
        fixture
            .authority
            .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: fixture.process_fencing_token,
                binding: ExecutionFiberId::from_bytes([0xB2; 16]),
                idempotency_key: IdempotencyKey::from_bytes([0xF2; 16]),
                registered_at_ms: 8_000,
            }),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed
        ))
    ));

    assert!(matches!(
        fixture
            .authority
            .write_fiber_entry_snapshot(WriteFiberEntrySnapshotRequest {
                process_id: fixture.process_id,
                binding,
                expected_incarnation_generation: Generation::INITIAL,
                handler_input: b"snapshot-after-crash".to_vec(),
                written_at_ms: 9_000,
            }),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed
        ))
    ));

    assert!(matches!(
        fixture
            .authority
            .inspect_active_process_binding(fixture.process_id),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed
        ))
    ));
}

#[test]
fn stale_fence_and_idempotency_rebinding_fail_closed() {
    let root = TestRoot::new("conflicts");
    let fixture = open_fixture(&root, 40);
    fixture
        .authority
        .propagate_crash(PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xD0; 16]),
            marked_at_ms: 10_000,
        })
        .expect("first crash");

    assert!(matches!(
        fixture.authority.propagate_crash(PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: [0xFF; 32],
            idempotency_key: IdempotencyKey::from_bytes([0xD1; 16]),
            marked_at_ms: 10_001,
        }),
        Err(ProcessAuthorityError::StaleProcessBinding)
    ));

    assert!(matches!(
        fixture.authority.propagate_crash(PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xD2; 16]),
            marked_at_ms: 10_002,
        }),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed
        ))
    ));

    assert!(matches!(
        fixture.authority.propagate_crash(PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xD0; 16]),
            marked_at_ms: 10_003,
        }),
        Ok(ProcessTerminalDecision::Replayed(_))
    ));
}

#[test]
fn pre_terminal_incarnation_replay_still_works() {
    let root = TestRoot::new("pre-terminal-replay");
    let fixture = open_fixture(&root, 50);
    let binding = ExecutionFiberId::from_bytes([0xE1; 16]);
    let request = RegisterFiberIncarnationRequest {
        process_id: fixture.process_id,
        expected_process_generation: fixture.process_generation,
        expected_process_fencing_token: fixture.process_fencing_token,
        binding,
        idempotency_key: IdempotencyKey::from_bytes([0xE2; 16]),
        registered_at_ms: 4_000,
    };
    let first = fixture
        .authority
        .register_fiber_incarnation(request)
        .expect("register");
    assert!(matches!(first, FiberIncarnationDecision::Registered(_)));

    fixture
        .authority
        .mark_process_terminated(MarkProcessTerminatedRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xE3; 16]),
            marked_at_ms: 11_000,
        })
        .expect("terminate");

    let replay = fixture
        .authority
        .register_fiber_incarnation(request)
        .expect("idempotent replay of pre-terminal registration");
    assert!(matches!(replay, FiberIncarnationDecision::Replayed(_)));
    assert_eq!(replay.record(), first.record());
}
