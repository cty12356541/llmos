//! Acceptance tests for B-PROCESS-003 platform kill contract-layer minimum
//! prefix: durable kill receipt, stub adapter invocation, terminal fail-closed,
//! and idempotent replay. Does **not** exercise real macOS/Windows OS kill.

use nlos_process::{
    CreateIsolationDomainRequest, IsolationDomainDecision, MarkProcessTerminatedRequest,
    PlatformKillDecision, ProcessAuthority, ProcessAuthorityError, ProcessBindingDecision,
    ProcessLifecycleState, PropagateCrashRequest, RegisterDelegatedProcessRequest,
    RequestPlatformKillRequest, StubPlatformKillAdapter,
};
use nlos_types::{Generation, IdempotencyKey, ProcessId, TaskAttemptId, TaskId};
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
            "nlos-process-platform-kill-{label}-{}-{nonce}-{sequence}",
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

fn kill_request(fixture: &Fixture, key: IdempotencyKey) -> RequestPlatformKillRequest {
    RequestPlatformKillRequest {
        process_id: fixture.process_id,
        expected_process_generation: fixture.process_generation,
        expected_process_fencing_token: fixture.process_fencing_token,
        idempotency_key: key,
        killed_at_ms: 12_000,
    }
}

#[test]
fn request_platform_kill_records_receipt_and_invokes_stub_adapter() {
    let root = TestRoot::new("stub-success");
    let fixture = open_fixture(&root, 60);
    let adapter = StubPlatformKillAdapter::new();
    let key = IdempotencyKey::from_bytes([0xA1; 16]);
    let request = kill_request(&fixture, key);

    let decision = fixture
        .authority
        .request_platform_kill(request, &adapter)
        .expect("platform kill");
    assert!(matches!(decision, PlatformKillDecision::Signaled(_)));
    assert_eq!(decision.receipt().idempotency_key, key);

    assert_eq!(
        adapter.recorded_signals(),
        vec![(fixture.process_id, fixture.process_generation)]
    );
    assert_eq!(
        fixture
            .authority
            .inspect_platform_kill_receipt(fixture.process_id, fixture.process_generation)
            .expect("inspect kill receipt")
            .as_ref(),
        Some(decision.receipt())
    );
    fixture
        .authority
        .inspect_active_process_binding(fixture.process_id)
        .expect("binding stays active until crash propagation");
}

#[test]
fn request_platform_kill_fail_closed_on_terminal_binding() {
    let root = TestRoot::new("terminal-reject");
    let terminated = open_fixture(&root, 61);
    terminated
        .authority
        .mark_process_terminated(MarkProcessTerminatedRequest {
            process_id: terminated.process_id,
            expected_process_generation: terminated.process_generation,
            expected_process_fencing_token: terminated.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xB1; 16]),
            marked_at_ms: 13_000,
        })
        .expect("terminate");

    let adapter = StubPlatformKillAdapter::new();
    assert!(matches!(
        terminated.authority.request_platform_kill(
            kill_request(&terminated, IdempotencyKey::from_bytes([0xA2; 16])),
            &adapter
        ),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Terminated
        ))
    ));
    assert!(adapter.recorded_signals().is_empty());

    let crashed = open_fixture(&root, 63);
    crashed
        .authority
        .propagate_crash(PropagateCrashRequest {
            process_id: crashed.process_id,
            expected_process_generation: crashed.process_generation,
            expected_process_fencing_token: crashed.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xC3; 16]),
            marked_at_ms: 13_500,
        })
        .expect("crash");
    assert!(matches!(
        crashed.authority.request_platform_kill(
            kill_request(&crashed, IdempotencyKey::from_bytes([0xA3; 16])),
            &StubPlatformKillAdapter::new()
        ),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed
        ))
    ));
}

#[test]
fn request_platform_kill_replays_without_reinvoking_adapter() {
    let root = TestRoot::new("replay");
    let kill_key = IdempotencyKey::from_bytes([0xD1; 16]);
    let (process_id, generation, receipt) = {
        let fixture = open_fixture(&root, 62);
        let adapter = StubPlatformKillAdapter::new();
        let request = kill_request(&fixture, kill_key);
        let first = fixture
            .authority
            .request_platform_kill(request, &adapter)
            .expect("first kill");
        assert!(matches!(first, PlatformKillDecision::Signaled(_)));
        assert_eq!(adapter.recorded_signals().len(), 1);

        let replay = fixture
            .authority
            .request_platform_kill(request, &adapter)
            .expect("in-memory replay");
        assert!(matches!(replay, PlatformKillDecision::Replayed(_)));
        assert_eq!(replay.receipt(), first.receipt());
        assert_eq!(adapter.recorded_signals().len(), 1);

        (
            fixture.process_id,
            fixture.process_generation,
            first.receipt().clone(),
        )
    };

    let reopened = ProcessAuthority::open(root.path()).expect("reopen");
    let adapter = StubPlatformKillAdapter::new();
    let replay = reopened
        .request_platform_kill(
            RequestPlatformKillRequest {
                process_id,
                expected_process_generation: generation,
                expected_process_fencing_token: receipt.process_fencing_token,
                idempotency_key: kill_key,
                killed_at_ms: receipt.killed_at_ms,
            },
            &adapter,
        )
        .expect("reopen replay");
    assert!(matches!(replay, PlatformKillDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &receipt);
    assert!(adapter.recorded_signals().is_empty());
}
