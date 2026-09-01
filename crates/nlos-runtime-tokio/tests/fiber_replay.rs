//! End-to-end tests for the ADR-0009 fiber replay minimal prefix:
//! `BindingEventProjection` projects one binding's durable wait rows into a
//! registration-ordered `BindingReplay`; `TokioRuntimeAdapter::resume_binding`
//! re-drives a new fiber incarnation through `ResumableBinding` and re-arms
//! the planned still-`PENDING` waits through the same single-row logic as
//! `rearm_channel_waits` (satisfied self-flip, live re-arm, supersede),
//! while `WOKEN`/`CANCELLED` events are report-only facts. Harness follows
//! `tests/channel_rehydration.rs` (temp `Root`, channel/wait authority pair,
//! multi-thread runtime, bounded pending probe, generous resolve window).

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_channel::{
    ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest, EnqueueDecision,
    EnqueueRequest,
};
use nlos_runtime::{FiberHandle, FiberSpec, FiberState, RuntimeAdapter, RuntimeError};
use nlos_runtime_tokio::{
    BindingEventProjection, BindingReplay, ChannelSequenceWait, ChannelWaitError, DeliveryReport,
    RearmedChannelWait, ReplayAuthorities, ResumableBinding, ResumePlan, ResumeRejection,
    TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ChannelId, ExecutionFiberId, Generation, IdempotencyKey,
    ProcessId, ResourceGroupId, SchedulerDomainId,
};
use nlos_wait::{
    BindingId, CancelWaitRequest, NotifyCommitsRequest, RegisterDecision, RegisterWaitRequest,
    WaitAuthority, WaitId, WaitRecord, WaitState, WakeReport,
};
use tokio::runtime::Handle;

/// Bounded observation window for "still pending" assertions.
const PENDING_PROBE: Duration = Duration::from_millis(100);
/// Generous bound for waits that must resolve.
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
            "nlos-runtime-tokio-fiber-replay-{label}-{}-{nonce}-{sequence}",
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

struct Pair {
    channel: Arc<ChannelAuthority>,
    wait: WaitAuthority,
}

fn open_pair(root: &Root) -> Pair {
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait = WaitAuthority::open(root.path(), Arc::clone(&channel)).expect("open wait authority");
    Pair { channel, wait }
}

fn create_channel(authority: &ChannelAuthority, seed: u8) -> ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: key(seed),
            created_at_ms: 900,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

/// Appends one entry and returns its durable sequence.
fn enqueue(authority: &ChannelAuthority, channel_id: ChannelId, seed: u8) -> u64 {
    let head = authority.inspect_channel(channel_id).expect("channel head");
    match authority
        .enqueue(EnqueueRequest {
            channel_id,
            expected_generation: head.generation,
            expected_fencing_token: head.fencing_token,
            payload: vec![seed; 8],
            idempotency_key: key(seed),
            enqueued_at_ms: 1_500,
        })
        .expect("enqueue")
    {
        EnqueueDecision::Enqueued(record) | EnqueueDecision::Replayed(record) => record.sequence,
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
            notified_at_ms: 2_000,
            idempotency_key: key(key_seed),
        })
        .expect("notify commits")
}

fn register_request(
    channel: &ChannelRecord,
    binding_id: BindingId,
    target: u64,
    key_seed: u8,
) -> RegisterWaitRequest {
    RegisterWaitRequest {
        binding: binding_id,
        channel_id: channel.channel_id,
        target_sequence: target,
        idempotency_key: key(key_seed),
        registered_at_ms: 1_000,
    }
}

/// Registers the durable wait directly (bypassing the runtime adapter).
fn register(authority: &WaitAuthority, request: RegisterWaitRequest) -> WaitRecord {
    match authority.register_wait(request).expect("register wait") {
        RegisterDecision::Registered(record) => record,
        RegisterDecision::Replayed(_) => panic!("fresh register cannot replay"),
    }
}

fn runtime() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(Handle::current(), TokioRuntimeConfig { max_live_fibers: 4 })
        .expect("runtime")
}

/// Spawns a fiber on `index`'s identity that pends forever; returns its
/// handle and scope id.
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

/// Asserts the armed wait stays pending inside the observation window.
async fn assert_stays_pending(wait: &mut ChannelSequenceWait) {
    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut *wait)
            .await
            .is_err(),
        "the armed wait must stay pending before its wake arrives"
    );
}

/// The canonical new-incarnation re-drive: re-arms every still-`PENDING`
/// wait event of the replay.
struct Redrive {
    binding_id: BindingId,
}

impl ResumableBinding for Redrive {
    fn binding(&self) -> BindingId {
        self.binding_id
    }

    fn resume(&self, replay: &BindingReplay) -> Result<ResumePlan, ResumeRejection> {
        Ok(ResumePlan::all_pending(replay))
    }
}

/// An incarnation whose re-drive always fails with its own reason.
struct Refusing {
    binding_id: BindingId,
}

impl ResumableBinding for Refusing {
    fn binding(&self) -> BindingId {
        self.binding_id
    }

    fn resume(&self, _replay: &BindingReplay) -> Result<ResumePlan, ResumeRejection> {
        Err(ResumeRejection {
            reason: "re-drive precondition failed".to_owned(),
        })
    }
}

/// An incarnation whose plan names a wait outside the replay's
/// still-`PENDING` events.
struct ForeignPlan {
    binding_id: BindingId,
    wait_id: WaitId,
}

impl ResumableBinding for ForeignPlan {
    fn binding(&self) -> BindingId {
        self.binding_id
    }

    fn resume(&self, _replay: &BindingReplay) -> Result<ResumePlan, ResumeRejection> {
        Ok(ResumePlan {
            rearm_wait_ids: vec![self.wait_id],
        })
    }
}

/// Given/When/Then: given one binding with waits on two channels registered
/// at distinct times and a second binding on the first channel; when the
/// binding's stream is projected; then every wait event of the binding is
/// present in registration-time order (across channels), the other
/// binding's wait never mixes in, and the all-zero binding fails closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // One test covers multi-channel order, isolation and the zero gate.
async fn projection_covers_one_binding_in_registration_order_and_excludes_others() {
    let root = Root::new("projection");
    let pair = open_pair(&root);
    let channel_a = create_channel(&pair.channel, 200);
    let channel_b = create_channel(&pair.channel, 201);

    // Binding 1 registers across channels out of channel order, with rising
    // registration timestamps; binding 2 registers on channel A.
    let first = register(
        &pair.wait,
        RegisterWaitRequest {
            binding: binding(1),
            channel_id: channel_b.channel_id,
            target_sequence: 1,
            idempotency_key: key(1),
            registered_at_ms: 1_000,
        },
    );
    let second = register(
        &pair.wait,
        RegisterWaitRequest {
            binding: binding(1),
            channel_id: channel_a.channel_id,
            target_sequence: 2,
            idempotency_key: key(2),
            registered_at_ms: 1_100,
        },
    );
    let foreign = register(&pair.wait, register_request(&channel_a, binding(2), 1, 3));

    let replay =
        BindingEventProjection::project(&pair.wait, ReplayAuthorities::default(), binding(1))
            .expect("project");
    assert_eq!(replay.binding, binding(1));
    assert_eq!(replay.events.len(), 2, "both wait events, others excluded");
    assert_eq!(
        replay.events[0]
            .as_wait()
            .expect("wait event")
            .record
            .wait_id,
        first.wait_id
    );
    assert_eq!(
        replay.events[0]
            .as_wait()
            .expect("wait event")
            .record
            .channel_id,
        channel_b.channel_id
    );
    assert_eq!(
        replay.events[0]
            .as_wait()
            .expect("wait event")
            .record
            .registered_at_ms,
        1_000
    );
    assert_eq!(
        replay.events[0].as_wait().expect("wait event").record.state,
        WaitState::Pending
    );
    assert_eq!(
        replay.events[1]
            .as_wait()
            .expect("wait event")
            .record
            .wait_id,
        second.wait_id
    );
    assert_eq!(
        replay.events[1]
            .as_wait()
            .expect("wait event")
            .record
            .channel_id,
        channel_a.channel_id
    );
    assert_eq!(
        replay.events[1]
            .as_wait()
            .expect("wait event")
            .record
            .registered_at_ms,
        1_100
    );

    let other =
        BindingEventProjection::project(&pair.wait, ReplayAuthorities::default(), binding(2))
            .expect("project other");
    assert_eq!(other.events.len(), 1);
    assert_eq!(
        other.events[0]
            .as_wait()
            .expect("wait event")
            .record
            .wait_id,
        foreign.wait_id
    );

    // The authority read behind the projection agrees on the order.
    let listed = pair
        .wait
        .list_waits_for_binding(binding(1))
        .expect("list binding waits");
    assert_eq!(
        listed.iter().map(|row| row.wait_id).collect::<Vec<_>>(),
        vec![first.wait_id, second.wait_id]
    );

    // The all-zero value is not a binding: fail closed, no fabrication.
    assert!(matches!(
        BindingEventProjection::project(
            &pair.wait,
            ReplayAuthorities::default(),
            BindingId::from_bytes([0; 16]),
        ),
        Err(ChannelWaitError::WaitAuthority(
            nlos_wait::WaitAuthorityError::InvalidBinding
        ))
    ));
}

/// Given/When/Then: given a `PENDING` durable wait registered by a previous
/// fiber incarnation; when the process "restarts" and the supervisor
/// resumes the binding on a new fiber incarnation; then the wait is
/// re-armed as a live in-memory wait of the new incarnation, a later commit
/// notification delivered through the new runtime's sink wakes it, the
/// fiber transitions `WaitingIo -> Running`, and the durable row ends
/// `WOKEN`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_redrives_pending_wait_to_live_wait_and_deliver_wakes_it() {
    let root = Root::new("resume-pending");
    let (durable, channel_id) = {
        let pair = open_pair(&root);
        let channel = create_channel(&pair.channel, 200);
        let adapter = runtime();
        let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);
        let request = register_request(&channel, binding(1), 1, 1);
        let durable = register(&pair.wait, request);
        let mut live = adapter
            .wait_for_channel(handle, &pair.wait, request)
            .expect("pre-restart wait");
        assert_stays_pending(&mut live).await;
        adapter
            .cancel_scope(scope, Generation::INITIAL)
            .expect("cancel");
        (durable, channel.channel_id)
    };

    // The "restart": fresh authorities, fresh adapter, new incarnation.
    let pair = open_pair(&root);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(
        &adapter,
        1,
        Generation::INITIAL.checked_next().expect("next"),
    );
    let mut report = adapter
        .resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1),
            },
        )
        .expect("resume binding");
    assert!(report.rearmed_satisfied.is_empty());
    assert_eq!(report.rearmed_pending.len(), 1);
    assert!(report.already_woken.is_empty());
    assert!(report.cancelled.is_empty());
    assert_eq!(report.replay.binding, binding(1));
    assert_eq!(report.replay.events.len(), 1);
    let RearmedChannelWait {
        record: armed_record,
        wait: mut armed_wait,
    } = report.rearmed_pending.remove(0);
    assert_eq!(armed_record.wait_id, durable.wait_id);
    assert_eq!(armed_record.state, WaitState::Pending);
    assert_stays_pending(&mut armed_wait).await;
    // The re-armed incarnation sets its wait state only once the runtime
    // polls it; CI runners schedule slower than the resume call returns, so
    // yield until the state settles instead of asserting immediately.
    // Pure `yield_now` polling: blocking sleeps would starve the very task
    // we are waiting for under a current-thread runtime.
    let mut state = adapter.inspect(handle).expect("inspect");
    for round in 0..200_000u32 {
        if state == FiberState::WaitingIo {
            break;
        }
        tokio::task::yield_now().await;
        // Slow CI runners (observed on windows-latest) can leave the resumed
        // incarnation unscheduled past any pure-yield budget; async sleep
        // yields the worker without starving it (blocking sleep deadlocks a
        // current-thread runtime — do not use it here).
        if round % 1_000 == 999 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        state = adapter.inspect(handle).expect("inspect");
    }
    assert_eq!(state, FiberState::WaitingIo);

    assert_eq!(enqueue(&pair.channel, channel_id, 50), 1);
    let wake = notify(&pair.wait, channel_id, 1, 30);
    assert_eq!(wake.woken.len(), 1);
    assert_eq!(
        adapter.channel_wait_sink().deliver(&wake).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, armed_wait)
            .await
            .expect("resolves"),
        WaitOutcome::Woken
    );
    // The wake transitioned the new incarnation back out of `WaitingIo`.
    assert_eq!(
        adapter.inspect(handle).expect("inspect"),
        FiberState::Running
    );
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect durable row")
            .state,
        WaitState::Woken
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a `PENDING` durable wait whose channel high-water
/// already covers its target; when the binding is resumed on a new
/// incarnation; then the wait is re-armed satisfied through the
/// domain-reserved self-flip (the one durable write resume performs), the
/// future resolves immediately `Woken`, and a later independent notify
/// flips nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_self_flips_high_water_covered_pending_wait_satisfied() {
    let root = Root::new("resume-satisfied");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let durable = register(&pair.wait, register_request(&channel, binding(1), 1, 1));
    assert_eq!(enqueue(&pair.channel, channel.channel_id, 52), 1);

    let mut report = adapter
        .resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1),
            },
        )
        .expect("resume binding");
    assert!(report.rearmed_pending.is_empty());
    assert_eq!(report.rearmed_satisfied.len(), 1);
    let RearmedChannelWait { record, wait } = report.rearmed_satisfied.remove(0);
    // The report carries the authoritative post-flip row.
    assert_eq!(record.wait_id, durable.wait_id);
    assert_eq!(record.state, WaitState::Woken);
    assert!(record.woken_at_ms > 0);
    assert_eq!(record.woken_up_to_sequence, 1);
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait).await.expect("resolves"),
        WaitOutcome::Woken
    );
    let flipped = pair
        .wait
        .inspect_wait(durable.wait_id)
        .expect("inspect flipped row");
    assert_eq!(flipped.state, WaitState::Woken);
    // A distinct notify key over the same range flips nothing: the row is
    // terminal, so the self-flip notification stays idempotent.
    assert!(
        notify(&pair.wait, channel.channel_id, 1, 31)
            .woken
            .is_empty()
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a `WOKEN` durable wait event (its wake even
/// buffered by an early delivery); when the binding is resumed; then the
/// event lands in `already_woken`, nothing is re-armed and no buffer is
/// consumed (the incarnation decides to skip or re-execute), and a later
/// rearm still resolves the row satisfied — at-least-once preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_reports_woken_events_as_facts_without_rearming() {
    let root = Root::new("resume-woken");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let sink = adapter.channel_wait_sink();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let durable = register(&pair.wait, register_request(&channel, binding(1), 1, 1));
    assert_eq!(enqueue(&pair.channel, channel.channel_id, 51), 1);
    let wake = notify(&pair.wait, channel.channel_id, 1, 30);
    assert_eq!(wake.woken.len(), 1);
    // The wake arrives with no registration anywhere: buffered placeholder.
    assert_eq!(
        sink.deliver(&wake).expect("early deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 1
        }
    );

    let report = adapter
        .resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1),
            },
        )
        .expect("resume binding");
    assert!(report.rearmed_satisfied.is_empty());
    assert!(report.rearmed_pending.is_empty());
    assert!(report.cancelled.is_empty());
    assert_eq!(report.already_woken.len(), 1);
    assert_eq!(report.already_woken[0].wait_id, durable.wait_id);
    assert_eq!(report.already_woken[0].state, WaitState::Woken);
    assert!(report.already_woken[0].woken_at_ms > 0);

    // Zero durable action: the row is exactly as the projection found it.
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect durable row")
            .state,
        WaitState::Woken
    );
    // At-least-once: the wake fact is still fully consumable afterwards —
    // a rearm of the same binding resolves the row satisfied.
    let mut follow_up = adapter
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("follow-up rearm");
    assert!(follow_up.pending.is_empty());
    assert_eq!(follow_up.satisfied.len(), 1);
    let RearmedChannelWait { record, wait } = follow_up.satisfied.remove(0);
    assert_eq!(record.wait_id, durable.wait_id);
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait).await.expect("resolves"),
        WaitOutcome::Woken
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a `CANCELLED` durable wait event; when the
/// binding is resumed; then the event lands in `cancelled`, nothing is
/// armed, and the durable row is untouched — a cancelled wait is never
/// resurrected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_reports_cancelled_events_as_facts_with_zero_action() {
    let root = Root::new("resume-cancelled");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let durable = register(&pair.wait, register_request(&channel, binding(1), 3, 1));
    pair.wait
        .cancel_wait(CancelWaitRequest {
            wait_id: durable.wait_id,
            cancelled_at_ms: 2_500,
            idempotency_key: key(90),
        })
        .expect("cancel the wait");

    let report = adapter
        .resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1),
            },
        )
        .expect("resume binding");
    assert!(report.rearmed_satisfied.is_empty());
    assert!(report.rearmed_pending.is_empty());
    assert!(report.already_woken.is_empty());
    assert_eq!(report.cancelled.len(), 1);
    assert_eq!(report.cancelled[0].wait_id, durable.wait_id);
    assert_eq!(report.cancelled[0].state, WaitState::Cancelled);
    assert_eq!(report.cancelled[0].cancelled_at_ms, 2_500);
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect durable row")
            .state,
        WaitState::Cancelled
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given the same binding resumed twice on the same new
/// incarnation; when the second resume re-arms the same still-`PENDING`
/// wait; then the first armed wait is superseded (resolves `Cancelled`),
/// the second stays live and wakes through the deliver path, and the raw
/// durable row is field-for-field unchanged across both resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_resume_supersedes_armed_wait_and_keeps_raw_row_unchanged() {
    let root = Root::new("resume-supersede");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let durable = register(&pair.wait, register_request(&channel, binding(1), 5, 1));
    let raw_before = pair
        .wait
        .inspect_wait(durable.wait_id)
        .expect("raw row before resumes");

    let mut first = adapter
        .resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1),
            },
        )
        .expect("first resume");
    assert_eq!(first.rearmed_pending.len(), 1);
    let mut armed_first = first.rearmed_pending.remove(0).wait;
    assert_stays_pending(&mut armed_first).await;

    // Idempotent replay: the second resume re-arms the same row and
    // supersedes the first arming.
    let mut second = adapter
        .resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1),
            },
        )
        .expect("second resume");
    assert!(second.rearmed_satisfied.is_empty());
    assert_eq!(second.rearmed_pending.len(), 1);
    let RearmedChannelWait {
        record: armed_record,
        wait: mut armed_second,
    } = second.rearmed_pending.remove(0);
    assert_eq!(armed_record.wait_id, durable.wait_id);
    assert_eq!(
        tokio::time::timeout(RESOLVE, armed_first)
            .await
            .expect("resolves"),
        WaitOutcome::Cancelled,
        "the superseded first arming resolves Cancelled"
    );
    assert_stays_pending(&mut armed_second).await;

    // Durable zero-side-effect: the raw row never changed across resumes.
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("raw row after resumes"),
        raw_before
    );

    // The surviving armed wait still wakes through the deliver path.
    for seed in 60..65 {
        enqueue(&pair.channel, channel.channel_id, seed);
    }
    let wake = notify(&pair.wait, channel.channel_id, 5, 33);
    assert_eq!(wake.woken.len(), 1);
    assert_eq!(
        adapter.channel_wait_sink().deliver(&wake).expect("deliver"),
        DeliveryReport {
            delivered: 1,
            buffered: 0
        }
    );
    assert_eq!(
        tokio::time::timeout(RESOLVE, armed_second)
            .await
            .expect("resolves"),
        WaitOutcome::Woken
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given stale and unknown fiber handles and a shut-down
/// runtime; when `resume_binding` is called; then each fails with the
/// mirrored gate error and the durable rows stay untouched (fail-closed
/// error paths perform zero durable side effects).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_unknown_handle_and_shutdown_fail_closed_without_side_effects() {
    let root = Root::new("resume-errors");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);
    let durable = register(&pair.wait, register_request(&channel, binding(1), 1, 1));

    let stale = FiberHandle {
        fiber_id: handle.fiber_id,
        generation: handle.generation.checked_next().expect("next generation"),
    };
    assert!(matches!(
        adapter.resume_binding(
            stale,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1)
            }
        ),
        Err(ChannelWaitError::Runtime(RuntimeError::InvalidGeneration))
    ));
    let unknown = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(999)),
        generation: Generation::INITIAL,
    };
    assert!(matches!(
        adapter.resume_binding(
            unknown,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1)
            }
        ),
        Err(ChannelWaitError::Runtime(RuntimeError::InvalidGeneration))
    ));

    // The rejected resumes left the durable enumeration unchanged.
    let listed = pair
        .wait
        .list_waits_for_binding(binding(1))
        .expect("list binding waits");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].wait_id, durable.wait_id);
    assert_eq!(listed[0].state, WaitState::Pending);

    adapter.shutdown();
    assert!(matches!(
        adapter.resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1)
            }
        ),
        Err(ChannelWaitError::Runtime(RuntimeError::ShuttingDown))
    ));
    // Shutdown does not touch the durable rows either.
    let listed = pair
        .wait
        .list_waits_for_binding(binding(1))
        .expect("list binding waits after shutdown");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, WaitState::Pending);
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given the same still-`PENDING` durable wait first
/// re-armed through `rearm_channel_waits` and then resumed through
/// `resume_binding` on the same new incarnation; then the rearm-armed wait
/// is superseded (resolves `Cancelled`), the resume-armed wait stays live
/// and wakes through the deliver path — both entry points share one wait
/// key space and one supersede semantics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rearm_then_resume_supersedes_across_entry_points() {
    let root = Root::new("rearm-then-resume");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let durable = register(&pair.wait, register_request(&channel, binding(1), 5, 1));
    let mut first = adapter
        .rearm_channel_waits(handle, &pair.wait, None)
        .expect("rearm")
        .pending
        .remove(0)
        .wait;
    assert_stays_pending(&mut first).await;

    let mut second = adapter
        .resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1),
            },
        )
        .expect("resume binding");
    assert_eq!(second.rearmed_pending.len(), 1);
    let mut armed = second.rearmed_pending.remove(0).wait;
    assert_eq!(
        tokio::time::timeout(RESOLVE, first)
            .await
            .expect("resolves"),
        WaitOutcome::Cancelled,
        "the rearm-armed wait is superseded by the resume"
    );
    assert_stays_pending(&mut armed).await;
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect durable row")
            .state,
        WaitState::Pending
    );

    for seed in 65..70 {
        enqueue(&pair.channel, channel.channel_id, seed);
    }
    let wake = notify(&pair.wait, channel.channel_id, 5, 34);
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
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given a binding with no durable wait rows; when the
/// stream is projected and the binding resumed on a new incarnation; then
/// the projection is the legal empty stream and the resume is the legal
/// empty report — zero events, zero arming, not an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_binding_projects_empty_replay_and_empty_report() {
    let root = Root::new("resume-empty");
    let pair = open_pair(&root);
    let _channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 2, Generation::INITIAL);

    let replay =
        BindingEventProjection::project(&pair.wait, ReplayAuthorities::default(), binding(7))
            .expect("project empty");
    assert_eq!(replay.binding, binding(7));
    assert!(replay.events.is_empty());

    let report = adapter
        .resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(7),
            },
        )
        .expect("resume empty binding");
    assert_eq!(report.replay.binding, binding(7));
    assert!(report.replay.events.is_empty());
    assert!(report.rearmed_satisfied.is_empty());
    assert!(report.rearmed_pending.is_empty());
    assert!(report.already_woken.is_empty());
    assert!(report.cancelled.is_empty());
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Given/When/Then: given an incarnation whose re-drive refuses, and an
/// incarnation whose plan names a wait outside the replay's still-`PENDING`
/// events; when either resumes; then the framework fails closed with the
/// matching typed error before any arming and the durable row stays raw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_re_drive_and_foreign_plan_fail_closed() {
    let root = Root::new("resume-contract");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let adapter = runtime();
    let (handle, scope) = spawn_waiter(&adapter, 1, Generation::INITIAL);

    let durable = register(&pair.wait, register_request(&channel, binding(1), 1, 1));

    // A refused re-drive propagates the incarnation's own reason verbatim.
    assert!(matches!(
        adapter.resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Refusing { binding_id: binding(1) }
        ),
        Err(ChannelWaitError::ResumeRejected(rejection))
            if rejection.reason == "re-drive precondition failed"
    ));

    // A plan naming a foreign (other binding's) wait fails closed.
    let other = register(&pair.wait, register_request(&channel, binding(2), 1, 2));
    assert!(matches!(
        adapter.resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &ForeignPlan {
                binding_id: binding(1),
                wait_id: other.wait_id,
            }
        ),
        Err(ChannelWaitError::ResumePlanMismatch)
    ));

    // Neither attempt armed anything: a clean resume still arms the row as
    // a live pending wait, proving no state leaked from the failures.
    let mut report = adapter
        .resume_binding(
            handle,
            &pair.wait,
            ReplayAuthorities::default(),
            &Redrive {
                binding_id: binding(1),
            },
        )
        .expect("clean resume after failures");
    assert_eq!(report.rearmed_pending.len(), 1);
    let mut armed = report.rearmed_pending.remove(0).wait;
    assert_stays_pending(&mut armed).await;
    assert_eq!(
        pair.wait
            .inspect_wait(durable.wait_id)
            .expect("inspect durable row")
            .state,
        WaitState::Pending
    );
    adapter
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}
