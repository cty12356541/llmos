//! End-to-end tests for the ADR-0012 B-PROCESS-002 remainder: the
//! registration-based projection across the wait/effect/queue authorities,
//! the ADR-0012 fiber-incarnation generation gate on the re-drive, and the
//! promoted `SnapshotResumable` B path (entry snapshot, crash-window
//! restore, terminal GC). Harness follows `tests/fiber_replay.rs`.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_channel::{
    ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest, EnqueueDecision,
    EnqueueRequest, ProducerRegistration, RegisterQueueConsumptionRequest,
};
use nlos_process::{
    CreateIsolationDomainRequest, FiberIncarnationDecision, IsolationDomainDecision,
    ProcessAuthority, ProcessBindingDecision, RegisterDelegatedProcessRequest,
    RegisterFiberIncarnationRequest,
};
use nlos_runtime::{FiberHandle, FiberSpec, FiberState, RuntimeAdapter};
use nlos_runtime_tokio::{
    BindingEventProjection, BindingReplay, BindingReplayEvent, ChannelSequenceWait,
    ChannelWaitError, DeliveryReport, ReplayAuthorities, ResumableBinding, ResumePlan,
    ResumeRejection, SnapshotResumable, SnapshotResumeReport, TokioRuntimeAdapter,
    TokioRuntimeConfig, WaitOutcome,
};
use nlos_task::{
    AttemptSpec, EffectBindingDecision, LogicalEffectDescriptor, PermitDecision, PermitRequest,
    PlannedEffect, RegisterEffectBindingRequest, SlotState, SnapshotBundle, SqliteTaskAuthority,
    TaskSpec, empty_effect_history_root,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ChannelId, CommitPermitId, ExecutionFiberId, Generation,
    IdempotencyKey, IsolationDomainId, ProcessId, ResourceGroupId, SchedulerDomainId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use nlos_wait::{BindingId, NotifyCommitsRequest, RegisterWaitRequest, WaitAuthority, WakeReport};
use tokio::runtime::Handle;

const PENDING_PROBE: Duration = Duration::from_millis(100);
const RESOLVE: Duration = Duration::from_secs(5);

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

fn fiber_binding(seed: u8) -> ExecutionFiberId {
    ExecutionFiberId::from_bytes([seed; 16])
}

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
            "nlos-runtime-tokio-fiber-reg-{label}-{}-{nonce}-{sequence}",
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

struct Authorities {
    channel: Arc<ChannelAuthority>,
    wait: Arc<WaitAuthority>,
    process: Arc<ProcessAuthority>,
    task: Arc<SqliteTaskAuthority>,
}

fn open_authorities(root: &Root) -> Authorities {
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait = Arc::new(
        WaitAuthority::open(root.path(), Arc::clone(&channel)).expect("open wait authority"),
    );
    let process = Arc::new(ProcessAuthority::open(root.path()).expect("open process authority"));
    let task = Arc::new(
        SqliteTaskAuthority::open(root.path().join("task.sqlite3")).expect("open task authority"),
    );
    Authorities {
        channel,
        wait,
        process,
        task,
    }
}

fn create_channel(authority: &ChannelAuthority, seed: u8) -> ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: key(200 + seed),
            created_at_ms: 900,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) | ChannelDecision::Replayed(record) => record,
    }
}

fn enqueue_registered(
    authorities: &Authorities,
    channel_id: ChannelId,
    payload_seed: u8,
    key_seed: u8,
) -> u64 {
    let head = authorities
        .channel
        .inspect_channel(channel_id)
        .expect("head");
    match authorities.channel.enqueue_registered(
        EnqueueRequest {
            channel_id,
            expected_generation: head.generation,
            expected_fencing_token: head.fencing_token,
            payload: vec![payload_seed; 8],
            idempotency_key: key(key_seed),
            enqueued_at_ms: 1_300,
        },
        ProducerRegistration {
            binding: fiber_binding(1),
            fiber_generation: Generation::INITIAL,
        },
    ) {
        Ok(EnqueueDecision::Enqueued(entry)) => entry.sequence,
        other => panic!("expected Enqueued, got {other:?}"),
    }
}

fn register_consumption(
    authorities: &Authorities,
    channel_id: ChannelId,
    sequence: u64,
    fiber: ExecutionFiberId,
    key_seed: u8,
    at: u64,
) {
    authorities
        .channel
        .register_queue_consumption(RegisterQueueConsumptionRequest {
            channel_id,
            sequence,
            binding: fiber,
            fiber_generation: Generation::INITIAL,
            idempotency_key: key(key_seed),
            registered_at_ms: at,
        })
        .expect("register consumption");
}

/// Task-plane fixture: task + attempt + `CommitPermit` carrying one planned
/// effect slot (the permit request persists the `Planned` slot set).
fn setup_effect_slot(
    task: &SqliteTaskAuthority,
    seed: u8,
) -> (TaskId, TaskAttemptId, CommitPermitId, u64) {
    let task_id = TaskId::from_bytes(id_bytes(60 + usize::from(seed)));
    task.register_task(TaskSpec {
        task_id,
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
    })
    .expect("register task");
    let attempt_id = TaskAttemptId::from_bytes(id_bytes(70 + usize::from(seed)));
    let spec = AttemptSpec {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(id_bytes(50 + usize::from(seed))),
            snapshot_digest: [0x20; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(40 + usize::from(seed))),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: key(30 + seed),
        registered_at_ms: 1_050,
    };
    task.register_attempt(spec).expect("register attempt");
    let permit = match task
        .request_commit_permit_with_authorities_struct(
            nlos_task::Authorities::default(),
            PermitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: [seed; 32],
                planned_effects: vec![PlannedEffect {
                    descriptor: LogicalEffectDescriptor {
                        task_id,
                        task_generation: Generation::INITIAL,
                        intent_spec_id: [0x44; 32],
                        stable_action_slot: 1,
                        target_authority_object_id: [0x55; 32],
                        effect_class: 7,
                        idempotency_scope: 3,
                    },
                    required: false,
                    required_condition_digest: None,
                    success_criteria_digest: [0x66; 32],
                    action_proposal_digest: [0x77; 32],
                }],
                idempotency_key: key(20 + seed),
                valid_until_ms: 99_999,
                requested_at_ms: 1_100,
            },
        )
        .expect("request commit permit")
    {
        PermitDecision::Issued(record) => record,
        other => panic!("expected Issued permit, got {other:?}"),
    };
    let permit_epoch = permit.permit_epoch;
    let permit_id = permit.permit_id;
    (task_id, attempt_id, permit_id, permit_epoch)
}

#[allow(clippy::too_many_arguments)]
fn register_effect(
    task: &SqliteTaskAuthority,
    task_id: TaskId,
    attempt_id: TaskAttemptId,
    permit_id: CommitPermitId,
    permit_epoch: u64,
    fiber: ExecutionFiberId,
    key_seed: u8,
    at: i64,
) -> EffectBindingDecision {
    match task.register_effect_binding(RegisterEffectBindingRequest {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        permit_id,
        permit_epoch,
        effect_seq: 0,
        binding: fiber,
        fiber_generation: Generation::INITIAL,
        idempotency_key: key(key_seed),
        registered_at_ms: at,
    }) {
        Ok(decision) => decision,
        Err(error) => panic!("register effect binding: {error}"),
    }
}

fn register_process_and_incarnation(
    process: &ProcessAuthority,
    seed: u8,
    fiber: ExecutionFiberId,
) -> (ProcessId, Generation) {
    let domain = match process.create_isolation_domain(CreateIsolationDomainRequest {
        policy_digest: [0x11; 32],
        idempotency_key: key(220 + seed),
        created_at_ms: 900,
    }) {
        Ok(IsolationDomainDecision::Created(record)) => record,
        other => panic!("expected Created domain, got {other:?}"),
    };
    let binding = match process.register_delegated_process(RegisterDelegatedProcessRequest {
        task_id: TaskId::from_bytes(id_bytes(80 + usize::from(seed))),
        task_attempt_id: TaskAttemptId::from_bytes(id_bytes(90 + usize::from(seed))),
        attempt_generation: Generation::INITIAL,
        isolation_domain_id: IsolationDomainId::from_bytes(domain.isolation_domain_id.into_bytes()),
        isolation_domain_generation: domain.generation,
        isolation_domain_fencing_token: domain.fencing_token,
        idempotency_key: key(230 + seed),
        created_at_ms: 950,
    }) {
        Ok(ProcessBindingDecision::Registered(record)) => record,
        other => panic!("expected Registered binding, got {other:?}"),
    };
    let incarnation = match process.register_fiber_incarnation(RegisterFiberIncarnationRequest {
        process_id: binding.process_id,
        expected_process_generation: binding.process_generation,
        expected_process_fencing_token: binding.process_fencing_token,
        binding: fiber,
        idempotency_key: key(240 + seed),
        registered_at_ms: 990,
    }) {
        Ok(FiberIncarnationDecision::Registered(record)) => record,
        other => panic!("expected Registered incarnation, got {other:?}"),
    };
    (binding.process_id, incarnation.incarnation_generation)
}

fn next_incarnation(
    process: &ProcessAuthority,
    process_id: ProcessId,
    fiber: ExecutionFiberId,
    key_seed: u8,
) -> Generation {
    let head = process
        .inspect_active_process_binding(process_id)
        .expect("process head");
    match process.register_fiber_incarnation(RegisterFiberIncarnationRequest {
        process_id,
        expected_process_generation: head.process_generation,
        expected_process_fencing_token: head.process_fencing_token,
        binding: fiber,
        idempotency_key: key(key_seed),
        registered_at_ms: 1_900,
    }) {
        Ok(FiberIncarnationDecision::Registered(record)) => record.incarnation_generation,
        other => panic!("expected Registered incarnation, got {other:?}"),
    }
}

fn fiber_spec(index: usize, scope: CancellationScopeId, generation: Generation) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        fiber_generation: generation,
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
    }
}

fn runtime() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(Handle::current(), TokioRuntimeConfig { max_live_fibers: 8 })
        .expect("runtime")
}

fn spawn_waiter(
    adapter: &TokioRuntimeAdapter,
    index: usize,
    generation: Generation,
) -> (FiberHandle, CancellationScopeId) {
    let scope = CancellationScopeId::from_bytes(id_bytes(300 + index));
    let handle = adapter
        .spawn_fiber(fiber_spec(index, scope, generation), Box::pin(pending()))
        .expect("spawn");
    (handle, scope)
}

fn wait_request(
    channel: &ChannelRecord,
    binding_id: BindingId,
    target: u64,
    key_seed: u8,
    at: u64,
) -> RegisterWaitRequest {
    RegisterWaitRequest {
        binding: binding_id,
        channel_id: channel.channel_id,
        target_sequence: target,
        idempotency_key: key(key_seed),
        registered_at_ms: at,
    }
}

fn notify(
    authority: &WaitAuthority,
    channel_id: ChannelId,
    up_to_sequence: u64,
    key_seed: u8,
) -> WakeReport {
    authority
        .notify_commits(NotifyCommitsRequest {
            channel_id,
            up_to_sequence,
            notified_at_ms: 4_000,
            idempotency_key: key(key_seed),
        })
        .expect("notify commits")
}

struct Redrive {
    binding_id: BindingId,
    process: Option<ProcessId>,
    incarnation: Option<Generation>,
}

impl ResumableBinding for Redrive {
    fn binding(&self) -> BindingId {
        self.binding_id
    }

    fn resume(&self, replay: &BindingReplay) -> Result<ResumePlan, ResumeRejection> {
        Ok(ResumePlan::all_pending(replay))
    }

    fn process_id(&self) -> Option<ProcessId> {
        self.process
    }

    fn expected_incarnation(&self) -> Option<Generation> {
        self.incarnation
    }
}

/// Given/When/Then: given one binding with a registered wait, effect and
/// queue consumption across the three authorities, and a second binding
/// registered on the wait and queue authorities too; when the stream is
/// projected, then it is registration-ordered across authorities, excludes
/// the foreign binding, equals the direct authority queries, and the
/// default (wait-only) sources project exactly the wait event. The stream
/// shape replays identically after a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // One test covers the full three-authority merge.
async fn projection_merges_three_authorities_in_order() {
    let root = Root::new("projection-merge");
    let authorities = open_authorities(&root);
    let channel = create_channel(&authorities.channel, 1);
    let entry_sequence = enqueue_registered(&authorities, channel.channel_id, 1, 2);

    // Out of time order on purpose: consumption (1500), wait (1100), effect
    // (1400). The stream must re-order to registration time.
    register_consumption(
        &authorities,
        channel.channel_id,
        entry_sequence,
        fiber_binding(1),
        3,
        1_500,
    );
    authorities
        .wait
        .register_wait(wait_request(&channel, binding(1), 5, 1, 1_100))
        .expect("register wait");
    let (task_id, attempt_id, permit_id, permit_epoch) = setup_effect_slot(&authorities.task, 1);
    register_effect(
        &authorities.task,
        task_id,
        attempt_id,
        permit_id,
        permit_epoch,
        fiber_binding(1),
        4,
        1_400,
    );

    // A foreign binding registers on the wait and queue authorities: none of
    // it may leak into binding one's stream.
    authorities
        .wait
        .register_wait(wait_request(&channel, binding(2), 5, 5, 1_050))
        .expect("foreign wait");
    register_consumption(
        &authorities,
        channel.channel_id,
        entry_sequence,
        fiber_binding(2),
        6,
        1_010,
    );

    let sources = ReplayAuthorities {
        channel: Some(&authorities.channel),
        task: Some(&authorities.task),
        process: None,
    };
    let replay =
        BindingEventProjection::project(&authorities.wait, sources, binding(1)).expect("project");
    assert_eq!(
        replay.events.len(),
        3,
        "one event per authority, foreign excluded"
    );
    assert!(matches!(&replay.events[0], BindingReplayEvent::Wait(_)));
    assert!(matches!(&replay.events[1], BindingReplayEvent::Effect(_)));
    assert!(matches!(
        &replay.events[2],
        BindingReplayEvent::QueueConsumed(_)
    ));
    assert_eq!(replay.events[0].recorded_at_ms(), 1_100);
    assert_eq!(replay.events[1].recorded_at_ms(), 1_400);
    assert_eq!(replay.events[2].recorded_at_ms(), 1_500);

    // Direct-query equivalence: the projection is exactly the join of the
    // three authority reads.
    let waits = authorities
        .wait
        .list_waits_for_binding(binding(1))
        .expect("list waits");
    assert_eq!(waits.len(), 1);
    let effects = authorities
        .task
        .list_effect_registrations_for_binding(fiber_binding(1))
        .expect("list effect registrations");
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].slot_state, SlotState::Planned);
    let consumptions = authorities
        .channel
        .list_consumptions_for_binding(fiber_binding(1))
        .expect("list consumptions");
    assert_eq!(consumptions.len(), 1);
    assert_eq!(
        replay
            .events
            .iter()
            .filter(|event| matches!(event, BindingReplayEvent::Wait(_)))
            .count(),
        waits.len()
    );
    assert_eq!(
        replay
            .events
            .iter()
            .filter(|event| matches!(event, BindingReplayEvent::Effect(_)))
            .count(),
        effects.len()
    );
    assert_eq!(
        replay
            .events
            .iter()
            .filter(|event| matches!(event, BindingReplayEvent::QueueConsumed(_)))
            .count(),
        consumptions.len()
    );

    // Default sources project the wait registry alone.
    let wait_only = BindingEventProjection::project(
        &authorities.wait,
        ReplayAuthorities::default(),
        binding(1),
    )
    .expect("project wait-only");
    assert_eq!(wait_only.events.len(), 1);

    // The all-zero value is not a binding: fail closed.
    assert!(matches!(
        BindingEventProjection::project(&authorities.wait, sources, BindingId::from_bytes([0; 16])),
        Err(ChannelWaitError::WaitAuthority(
            nlos_wait::WaitAuthorityError::InvalidBinding
        ))
    ));

    // Restart equivalence: the projection over the reopened authorities is
    // the same stream shape.
    drop(authorities);
    let authorities = open_authorities(&root);
    let sources = ReplayAuthorities {
        channel: Some(&authorities.channel),
        task: Some(&authorities.task),
        process: None,
    };
    let replay = BindingEventProjection::project(&authorities.wait, sources, binding(1))
        .expect("project after restart");
    assert_eq!(replay.events.len(), 3);
    assert!(matches!(&replay.events[0], BindingReplayEvent::Wait(_)));
    assert!(matches!(&replay.events[1], BindingReplayEvent::Effect(_)));
    assert!(matches!(
        &replay.events[2],
        BindingReplayEvent::QueueConsumed(_)
    ));
}

/// Given/When/Then: given a previous incarnation that registered a
/// `PENDING` wait, an effect and a queue consumption under fiber incarnation
/// generation 1; when the supervisor restarts, registers incarnation
/// generation 2 and resumes with the ADR-0012 generation gate; then the
/// wait is re-armed live, the effect and consumption events are report-only
/// facts (the effect registration window converged: the slot is still
/// `Planned` and the same-key registration replays), and a stale-incarnation
/// resume fails closed with zero side effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // One test covers the gated resume plus both fault windows.
async fn resume_binding_gates_incarnation_and_reports_new_events() {
    let root = Root::new("resume-gate");
    #[allow(unused_assignments)] // The inner scope always assigns before the outer scope reads.
    let mut pre_restart: Option<(
        ProcessId,
        Generation,
        ChannelId,
        TaskId,
        TaskAttemptId,
        CommitPermitId,
        u64,
    )> = None;
    {
        let authorities = open_authorities(&root);
        let channel = create_channel(&authorities.channel, 2);
        let entry_sequence = enqueue_registered(&authorities, channel.channel_id, 2, 12);
        register_consumption(
            &authorities,
            channel.channel_id,
            entry_sequence,
            fiber_binding(1),
            13,
            1_500,
        );
        authorities
            .wait
            .register_wait(wait_request(&channel, binding(1), 5, 11, 1_100))
            .expect("register wait");
        let (task_id, attempt_id, permit_id, permit_epoch) =
            setup_effect_slot(&authorities.task, 2);
        register_effect(
            &authorities.task,
            task_id,
            attempt_id,
            permit_id,
            permit_epoch,
            fiber_binding(1),
            14,
            1_400,
        );
        let (process_id, incarnation) =
            register_process_and_incarnation(&authorities.process, 2, fiber_binding(1));
        pre_restart = Some((
            process_id,
            incarnation,
            channel.channel_id,
            task_id,
            attempt_id,
            permit_id,
            permit_epoch,
        ));
    }
    let (process_id, first_incarnation, channel_id, task_id, attempt_id, permit_id, permit_epoch) =
        pre_restart.expect("pre-restart facts");

    // The "restart": fresh authorities and a new durable incarnation
    // generation for the binding.
    let authorities = open_authorities(&root);
    let second_incarnation =
        next_incarnation(&authorities.process, process_id, fiber_binding(1), 41);
    assert_eq!(
        second_incarnation,
        first_incarnation.checked_next().expect("next")
    );

    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);
    let sources = ReplayAuthorities {
        channel: Some(&authorities.channel),
        task: Some(&authorities.task),
        process: Some(&authorities.process),
    };
    let mut report = adapter
        .resume_binding(
            handle,
            &authorities.wait,
            sources,
            &Redrive {
                binding_id: binding(1),
                process: Some(process_id),
                incarnation: Some(second_incarnation),
            },
        )
        .expect("resume binding");
    assert_eq!(report.replay.events.len(), 3);
    assert!(report.rearmed_satisfied.is_empty());
    assert_eq!(report.rearmed_pending.len(), 1);
    assert!(report.already_woken.is_empty());
    assert!(report.cancelled.is_empty());
    assert_eq!(report.effect_events.len(), 1);
    assert_eq!(report.queue_events.len(), 1);
    // Report-only effect fact: the effect initiation window converged — the
    // effect was registered but never dispatched, so the slot is still
    // `Planned` (registration-time state survives the restart), and the
    // same-key registration replays idempotently: the write window between
    // "registered" and "dispatched" costs nothing on replay.
    assert_eq!(
        report.effect_events[0].registration.slot_state,
        SlotState::Planned
    );
    match register_effect(
        &authorities.task,
        task_id,
        attempt_id,
        permit_id,
        permit_epoch,
        fiber_binding(1),
        14,
        1_400,
    ) {
        EffectBindingDecision::Replayed(_) => {}
        other @ EffectBindingDecision::Registered(_) => {
            panic!("expected Replayed effect registration, got {other:?}")
        }
    }

    let mut armed = report.rearmed_pending.remove(0).wait;
    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut armed)
            .await
            .is_err()
    );
    assert_eq!(
        adapter.inspect(handle).expect("inspect"),
        FiberState::WaitingIo
    );

    // Stale-incarnation resume: fail closed, zero durable side effect — the
    // gate fires before the projection, so nothing is armed and the live
    // wait keeps waiting.
    let stale = adapter
        .resume_binding(
            handle,
            &authorities.wait,
            sources,
            &Redrive {
                binding_id: binding(1),
                process: Some(process_id),
                incarnation: Some(first_incarnation),
            },
        )
        .expect_err("stale incarnation must fail closed");
    assert!(matches!(stale, ChannelWaitError::StaleFiberIncarnation));
    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut armed)
            .await
            .is_err(),
        "the failed resume must not disturb the live armed wait"
    );

    // A later notification wakes the re-armed wait of the new incarnation.
    let wake = notify(&authorities.wait, channel_id, 5, 15);
    assert_eq!(wake.woken.len(), 1);
    assert_eq!(
        adapter.channel_wait_sink().deliver(&wake).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, armed)
            .await
            .expect("resolves"),
        WaitOutcome::Woken
    );
    assert_eq!(
        adapter.inspect(handle).expect("inspect"),
        FiberState::Running
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// The B-path handler of the snapshot test: re-executes the handler from its
/// entry by re-registering the fiber's wait through the normal runtime
/// entry, so the resumed fiber drives itself back to its wait point with its
/// own code.
struct TestHandler {
    binding_id: BindingId,
    process: ProcessId,
    incarnation: Generation,
    handle: FiberHandle,
    adapter: TokioRuntimeAdapter,
    waits: Arc<WaitAuthority>,
    channel_id: ChannelId,
    registered: Mutex<Option<ChannelSequenceWait>>,
}

impl TestHandler {
    fn new(
        base: &TestHandler,
        incarnation: Generation,
        handle: FiberHandle,
        adapter: TokioRuntimeAdapter,
    ) -> Self {
        Self {
            binding_id: base.binding_id,
            process: base.process,
            incarnation,
            handle,
            adapter,
            waits: Arc::clone(&base.waits),
            channel_id: base.channel_id,
            registered: Mutex::new(None),
        }
    }

    fn wait_registration(&self) -> RegisterWaitRequest {
        RegisterWaitRequest {
            binding: self.binding_id,
            channel_id: self.channel_id,
            target_sequence: 5,
            idempotency_key: key(51),
            registered_at_ms: 2_500,
        }
    }
}

impl SnapshotResumable for TestHandler {
    fn binding(&self) -> BindingId {
        self.binding_id
    }

    fn process_id(&self) -> ProcessId {
        self.process
    }

    fn expected_incarnation(&self) -> Generation {
        self.incarnation
    }

    fn handler_input(&self) -> Vec<u8> {
        b"handler-entry-input".to_vec()
    }

    fn resume_from_entry(&self, input: &[u8]) -> Result<(), ResumeRejection> {
        assert_eq!(input, b"handler-entry-input");
        let wait = self
            .adapter
            .wait_for_channel(self.handle, &self.waits, self.wait_registration())
            .map_err(|error| ResumeRejection {
                reason: format!("re-registration failed: {error}"),
            })?;
        *self.registered.lock().expect("registered cell") = Some(wait);
        Ok(())
    }
}

/// Given/When/Then: given a fiber that recorded its handler-entry snapshot
/// and then crashed mid-handler (snapshot committed, wait point never
/// reached); when the supervisor restarts, registers the next incarnation
/// and resumes from the snapshot; then the handler re-executes from its
/// entry, re-registers the same wait key (idempotent re-execution: exactly
/// one durable wait row), and the fiber is driven back to its wait point; a
/// stale-incarnation restore fails closed; and once the fiber reaches
/// terminal state, the GC removes the slot so a later restore reports
/// unavailable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::similar_names)] // handler1/2/3 are the three incarnations under test
#[allow(clippy::too_many_lines)] // One test covers the whole B-path crash window, restore and GC.
async fn snapshot_path_restores_crash_window_and_gcs_on_terminal() {
    let root = Root::new("snapshot-b-path");
    let (process_id, first_incarnation, channel_id) = {
        let authorities = open_authorities(&root);
        let channel = create_channel(&authorities.channel, 3);
        let (pid, incarnation) =
            register_process_and_incarnation(&authorities.process, 3, fiber_binding(1));
        (pid, incarnation, channel.channel_id)
    };

    // Incarnation 1: live fiber, entry snapshot recorded, then the crash
    // window — the snapshot committed but the handler never reached its
    // wait point.
    let authorities = open_authorities(&root);
    let adapter1 = runtime();
    let (handle1, scope1) = spawn_waiter(&adapter1, 1, Generation::INITIAL);
    let first_handler = TestHandler {
        binding_id: binding(1),
        process: process_id,
        incarnation: first_incarnation,
        handle: handle1,
        adapter: adapter1.clone(),
        waits: Arc::clone(&authorities.wait),
        channel_id,
        registered: Mutex::new(None),
    };
    let record = adapter1
        .snapshot_handler_entry(handle1, &authorities.process, &first_handler)
        .expect("snapshot write")
        .expect("live fiber records its entry");
    assert_eq!(record.handler_input, b"handler-entry-input".to_vec());
    assert_eq!(record.written_by_incarnation, first_incarnation);

    // The crash: incarnation 1's world goes away.
    adapter1.cancel_scope(scope1, Generation::INITIAL).ok();
    drop(first_handler);
    drop(adapter1);

    // The supervisor restarts the binding: incarnation generation 2.
    let second_incarnation =
        next_incarnation(&authorities.process, process_id, fiber_binding(1), 42);
    assert_eq!(
        second_incarnation,
        first_incarnation.checked_next().expect("next")
    );

    let adapter2 = runtime();
    let (handle2, scope2) = spawn_waiter(
        &adapter2,
        2,
        Generation::INITIAL.checked_next().expect("next"),
    );
    let second_handler = TestHandler::new(
        &TestHandler {
            binding_id: binding(1),
            process: process_id,
            incarnation: second_incarnation,
            handle: handle2,
            adapter: adapter2.clone(),
            waits: Arc::clone(&authorities.wait),
            channel_id,
            registered: Mutex::new(None),
        },
        second_incarnation,
        handle2,
        adapter2.clone(),
    );

    // Stale gate first: a restorer claiming the dead incarnation fails
    // closed with zero side effect (no wait row exists yet).
    let stale_handler = TestHandler::new(
        &second_handler,
        first_incarnation,
        handle2,
        adapter2.clone(),
    );
    assert!(matches!(
        adapter2.resume_from_snapshot(handle2, &authorities.process, &stale_handler),
        Err(ChannelWaitError::StaleFiberIncarnation)
    ));
    assert!(
        authorities
            .wait
            .list_waits_for_binding(binding(1))
            .expect("no wait row after stale restore")
            .is_empty()
    );

    // Restore: the handler re-executes from its entry input and registers
    // its wait through the normal runtime entry — driven back to the wait
    // point.
    let report = adapter2
        .resume_from_snapshot(handle2, &authorities.process, &second_handler)
        .expect("restore");
    let SnapshotResumeReport {
        binding: restored_binding,
        restored,
    } = report;
    assert_eq!(restored_binding, binding(1));
    assert!(restored.is_some());
    assert_eq!(
        adapter2.inspect(handle2).expect("inspect"),
        FiberState::WaitingIo
    );

    // Idempotent re-execution: a second restore re-registers the same wait
    // key — exactly one durable wait row, field-for-field the original, and
    // the first armed wait is superseded (the surviving wait sits in the
    // handler cell).
    let before = authorities
        .wait
        .list_waits_for_binding(binding(1))
        .expect("wait rows before second restore");
    assert_eq!(before.len(), 1);
    let report2 = adapter2
        .resume_from_snapshot(handle2, &authorities.process, &second_handler)
        .expect("second restore");
    assert!(report2.restored.is_some());
    let after = authorities
        .wait
        .list_waits_for_binding(binding(1))
        .expect("wait rows after second restore");
    assert_eq!(
        after, before,
        "re-execution is idempotent on the durable face"
    );

    // Driven to and through the wait point: enqueue, notify, deliver.
    let head = authorities
        .channel
        .inspect_channel(channel_id)
        .expect("head");
    authorities
        .channel
        .enqueue(EnqueueRequest {
            channel_id,
            expected_generation: head.generation,
            expected_fencing_token: head.fencing_token,
            payload: vec![9; 8],
            idempotency_key: key(16),
            enqueued_at_ms: 3_000,
        })
        .expect("enqueue");
    let wake = notify(&authorities.wait, channel_id, 5, 17);
    assert_eq!(wake.woken.len(), 1);
    assert_eq!(
        adapter2
            .channel_wait_sink()
            .deliver(&wake)
            .expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    let survived = second_handler
        .registered
        .lock()
        .expect("registered cell")
        .take()
        .expect("the surviving re-registered wait");
    let mut survived = survived;
    assert_eq!(
        tokio::time::timeout(RESOLVE, &mut survived)
            .await
            .expect("resolves"),
        WaitOutcome::Woken
    );

    // Terminal state closes the B path: once incarnation 2 went terminal
    // and generation 3 exists, its snapshot slot is collected and restore
    // reports unavailable.
    adapter2
        .cancel_scope(scope2, Generation::INITIAL)
        .expect("cancel incarnation 2");
    let third_incarnation =
        next_incarnation(&authorities.process, process_id, fiber_binding(1), 43);
    assert_eq!(
        third_incarnation,
        second_incarnation.checked_next().expect("next")
    );
    let (handle3, scope3) = spawn_waiter(
        &adapter2,
        3,
        Generation::INITIAL.checked_next().expect("next"),
    );
    let third_handler = TestHandler::new(
        &second_handler,
        third_incarnation,
        handle3,
        adapter2.clone(),
    );
    assert!(
        adapter2
            .gc_handler_entry_snapshot(handle3, &authorities.process, &third_handler)
            .expect("gc")
    );
    assert!(matches!(
        adapter2.resume_from_snapshot(handle3, &authorities.process, &third_handler),
        Err(ChannelWaitError::SnapshotUnavailable)
    ));
    adapter2
        .cancel_scope(scope3, Generation::INITIAL)
        .expect("cancel incarnation 3");
}
