//! W15-P / B-PROCESS-003 runtime linkage: after `propagate_crash` or
//! `mark_process_terminated`, runtime resume and snapshot paths fail closed
//! through `inspect_active_process_binding` before any durable side effect.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_channel::ChannelAuthority;
use nlos_process::{
    CreateIsolationDomainRequest, FiberIncarnationDecision, IsolationDomainDecision,
    MarkProcessTerminatedRequest, ProcessAuthority, ProcessAuthorityError, ProcessBindingDecision,
    ProcessLifecycleState, PropagateCrashRequest, RegisterDelegatedProcessRequest,
    RegisterFiberIncarnationRequest,
};
use nlos_runtime::{FiberHandle, FiberSpec, RuntimeAdapter};
use nlos_runtime_tokio::{
    BindingReplay, ChannelWaitError, ReplayAuthorities, ResumableBinding, ResumePlan,
    ResumeRejection, SnapshotResumable, TokioRuntimeAdapter, TokioRuntimeConfig,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey, ProcessId,
    ResourceGroupId, SchedulerDomainId, TaskAttemptId, TaskId,
};
use nlos_wait::{BindingId, WaitAuthority};
use tokio::runtime::Handle;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-runtime-tokio-process-crash-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn id_bytes(value: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(value as u64).to_be_bytes());
    bytes
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
}

struct ProcessFixture {
    process: ProcessAuthority,
    process_id: ProcessId,
    process_generation: Generation,
    process_fencing_token: nlos_process::FencingToken,
    fiber_binding: ExecutionFiberId,
    incarnation: Generation,
}

fn open_process_fixture(root: &Root, seed: u8) -> ProcessFixture {
    let process = ProcessAuthority::open(root.path()).expect("open process");
    let domain = match process.create_isolation_domain(CreateIsolationDomainRequest {
        policy_digest: [seed; 32],
        idempotency_key: key(seed.wrapping_add(1)),
        created_at_ms: 1_000,
    }) {
        Ok(
            IsolationDomainDecision::Created(record) | IsolationDomainDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("domain: {error}"),
    };
    let binding_record = match process.register_delegated_process(RegisterDelegatedProcessRequest {
        task_id: TaskId::from_bytes([seed.wrapping_add(2); 16]),
        task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(3); 16]),
        attempt_generation: Generation::INITIAL,
        isolation_domain_id: domain.isolation_domain_id,
        isolation_domain_generation: domain.generation,
        isolation_domain_fencing_token: domain.fencing_token,
        idempotency_key: key(seed.wrapping_add(4)),
        created_at_ms: 2_000,
    }) {
        Ok(
            ProcessBindingDecision::Registered(record) | ProcessBindingDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("register process: {error}"),
    };
    let fiber_binding = ExecutionFiberId::from_bytes(id_bytes(usize::from(seed) + 100));
    let incarnation = match process.register_fiber_incarnation(RegisterFiberIncarnationRequest {
        process_id: binding_record.process_id,
        expected_process_generation: binding_record.process_generation,
        expected_process_fencing_token: binding_record.process_fencing_token,
        binding: fiber_binding,
        idempotency_key: key(seed.wrapping_add(5)),
        registered_at_ms: 3_000,
    }) {
        Ok(FiberIncarnationDecision::Registered(record)) => record.incarnation_generation,
        other => panic!("incarnation: {other:?}"),
    };
    ProcessFixture {
        process_id: binding_record.process_id,
        process_generation: binding_record.process_generation,
        process_fencing_token: binding_record.process_fencing_token,
        process,
        fiber_binding,
        incarnation,
    }
}

fn runtime() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(Handle::current(), TokioRuntimeConfig { max_live_fibers: 4 })
        .expect("runtime")
}

fn spawn_fiber(adapter: &TokioRuntimeAdapter, index: usize) -> FiberHandle {
    let scope = CancellationScopeId::from_bytes(id_bytes(400 + index));
    adapter
        .spawn_fiber(
            FiberSpec {
                fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
                fiber_generation: Generation::INITIAL,
                agent_instance_id: AgentInstanceId::from_bytes(id_bytes(index)),
                agent_generation: Generation::INITIAL,
                process_id: ProcessId::from_bytes(id_bytes(1)),
                process_generation: Generation::INITIAL,
                task_attempt_id: None,
                cancellation_scope_id: scope,
                cancellation_generation: Generation::INITIAL,
                resource_group_id: ResourceGroupId::from_bytes(id_bytes(1)),
                scheduler_domain_id: SchedulerDomainId::from_bytes(id_bytes(1)),
                deadline: None,
            },
            Box::pin(pending()),
        )
        .expect("spawn")
}

struct StubResumable {
    binding_id: BindingId,
    process_id: ProcessId,
    incarnation: Generation,
}

impl ResumableBinding for StubResumable {
    fn binding(&self) -> BindingId {
        self.binding_id
    }

    fn process_id(&self) -> Option<ProcessId> {
        Some(self.process_id)
    }

    fn expected_incarnation(&self) -> Option<Generation> {
        Some(self.incarnation)
    }

    fn resume(&self, _replay: &BindingReplay) -> Result<ResumePlan, ResumeRejection> {
        Ok(ResumePlan {
            rearm_wait_ids: Vec::new(),
        })
    }
}

struct StubSnapshot {
    binding_id: BindingId,
    process_id: ProcessId,
    incarnation: Generation,
}

impl SnapshotResumable for StubSnapshot {
    fn binding(&self) -> BindingId {
        self.binding_id
    }

    fn process_id(&self) -> ProcessId {
        self.process_id
    }

    fn expected_incarnation(&self) -> Generation {
        self.incarnation
    }

    fn handler_input(&self) -> Vec<u8> {
        b"entry".to_vec()
    }

    fn resume_from_entry(&self, _input: &[u8]) -> Result<(), ResumeRejection> {
        Ok(())
    }
}

fn assert_process_terminal(error: ChannelWaitError) {
    match error {
        ChannelWaitError::ProcessAuthority(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed,
        )) => {}
        other => panic!("expected ProcessBindingTerminal(Crashed), got {other}"),
    }
}

fn propagate_crash(fixture: &ProcessFixture) {
    fixture
        .process
        .propagate_crash(PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: key(0x70),
            marked_at_ms: 9_000,
        })
        .expect("propagate crash");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_process_rejects_resume_binding() {
    let root = Root::new("resume-binding");
    let channel = std::sync::Arc::new(ChannelAuthority::open(root.path()).expect("channel"));
    let wait = WaitAuthority::open(root.path(), channel).expect("wait");
    let fixture = open_process_fixture(&root, 0x11);
    let binding_id = binding(0x22);
    let adapter = runtime();
    let handle = spawn_fiber(&adapter, 1);
    propagate_crash(&fixture);
    let resumable = StubResumable {
        binding_id,
        process_id: fixture.process_id,
        incarnation: fixture.incarnation,
    };
    let error = adapter
        .resume_binding(
            handle,
            &wait,
            ReplayAuthorities {
                channel: None,
                task: None,
                process: Some(&fixture.process),
            },
            &resumable,
        )
        .expect_err("terminal process must fail resume_binding");
    assert_process_terminal(error);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_process_rejects_resume_from_snapshot() {
    let root = Root::new("resume-snapshot");
    let fixture = open_process_fixture(&root, 0x21);
    let adapter = runtime();
    let handle = spawn_fiber(&adapter, 2);
    let binding_id = BindingId::from_bytes(*fixture.fiber_binding.as_bytes());
    let snapshot = StubSnapshot {
        binding_id,
        process_id: fixture.process_id,
        incarnation: fixture.incarnation,
    };
    adapter
        .snapshot_handler_entry(handle, &fixture.process, &snapshot)
        .expect("record snapshot before terminal");
    propagate_crash(&fixture);
    let error = adapter
        .resume_from_snapshot(handle, &fixture.process, &snapshot)
        .expect_err("terminal process must fail resume_from_snapshot");
    assert_process_terminal(error);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_process_rejects_snapshot_handler_entry() {
    let root = Root::new("snapshot-write");
    let fixture = open_process_fixture(&root, 0x31);
    fixture
        .process
        .mark_process_terminated(MarkProcessTerminatedRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: key(0x80),
            marked_at_ms: 8_000,
        })
        .expect("terminate");
    let adapter = runtime();
    let handle = spawn_fiber(&adapter, 3);
    let binding_id = BindingId::from_bytes(*fixture.fiber_binding.as_bytes());
    let snapshot = StubSnapshot {
        binding_id,
        process_id: fixture.process_id,
        incarnation: fixture.incarnation,
    };
    let error = adapter
        .snapshot_handler_entry(handle, &fixture.process, &snapshot)
        .expect_err("terminal process must fail snapshot write");
    match error {
        ChannelWaitError::ProcessAuthority(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Terminated,
        )) => {}
        other => panic!("expected ProcessBindingTerminal(Terminated), got {other}"),
    }
}
