//! W36 kill-receipt consumption × Activation-meter linkage tests
//! (B-PROCESS-003 §6-§8 residual): when
//! [`ProcessAuthority::request_platform_kill`] has committed a durable
//! receipt and the binding went terminal,
//! [`TokioRuntimeAdapter::consume_platform_kill`] consumes that receipt and
//! drives the W27-C batch-cancel linkage for the killed process's fibers,
//! feeding the lock-free health meters; a missing receipt or a non-terminal
//! binding fails closed with zero runtime side effect.
//!
//! Coverage map (anti-duplication survey): single-scope cancel semantics,
//! terminal uniqueness, late callbacks, respawn fences and authority
//! fail-closed gates are covered by `batch_cancel.rs` rows 1-6 and
//! `cancel_late_callback_matrix.rs`; this file covers the kill-receipt gate
//! (evidence, ordering, metering) on top of that linkage — including the
//! end-to-end POSIX chain (real supervisor child → real SIGTERM receipt →
//! runtime consumption → fiber cancellation → meter deltas).

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_process::{
    CreateIsolationDomainRequest, IsolationDomainDecision, PlatformKillDecision,
    PosixPlatformKillAdapter, ProcessAuthority, ProcessBindingDecision, ProcessLifecycleState,
    PropagateCancelToFibersRequest, PropagateCrashRequest, RegisterDelegatedProcessRequest,
    RegisterFiberIncarnationRequest, RequestPlatformKillRequest, StubPlatformKillAdapter,
};
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter};
use nlos_runtime_tokio::{
    ChannelWaitError, PlatformKillConsumptionReport, RuntimeHealth, TokioRuntimeAdapter,
    TokioRuntimeConfig,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, ProcessId, ResourceGroupId, SchedulerDomainId, TaskAttemptId, TaskId,
};
use tokio::runtime::Handle;

fn id_bytes(value: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(value as u64).to_be_bytes());
    bytes
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
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
            "nlos-runtime-tokio-kill-receipt-{label}-{}-{nonce}-{sequence}",
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

struct ProcessFixture {
    process: ProcessAuthority,
    process_id: ProcessId,
    process_generation: Generation,
    process_fencing_token: nlos_process::FencingToken,
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
    let binding = match process.register_delegated_process(RegisterDelegatedProcessRequest {
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
    ProcessFixture {
        process,
        process_id: binding.process_id,
        process_generation: binding.process_generation,
        process_fencing_token: binding.process_fencing_token,
    }
}

fn kill_request(fixture: &ProcessFixture, key_seed: u8) -> RequestPlatformKillRequest {
    RequestPlatformKillRequest {
        process_id: fixture.process_id,
        expected_process_generation: fixture.process_generation,
        expected_process_fencing_token: fixture.process_fencing_token,
        idempotency_key: key(key_seed),
        killed_at_ms: 8_000,
    }
}

fn propagate_crash(fixture: &ProcessFixture, key_seed: u8) {
    fixture
        .process
        .propagate_crash(PropagateCrashRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            idempotency_key: key(key_seed),
            marked_at_ms: 9_000,
        })
        .expect("propagate crash");
}

fn propagation_request(fixture: &ProcessFixture, key_seed: u8) -> PropagateCancelToFibersRequest {
    PropagateCancelToFibersRequest {
        process_id: fixture.process_id,
        expected_process_generation: fixture.process_generation,
        expected_process_fencing_token: fixture.process_fencing_token,
        lifecycle_state: ProcessLifecycleState::Crashed,
        idempotency_key: key(key_seed),
        cancelled_at_ms: 9_500,
    }
}

/// Registers one durable fiber incarnation under the fixture's process so
/// the crash-time batch cancel propagation (and its replay) has receipt
/// rows to be observable through.
fn register_incarnation(fixture: &ProcessFixture, fiber: ExecutionFiberId, seed: u8) {
    match fixture
        .process
        .register_fiber_incarnation(RegisterFiberIncarnationRequest {
            process_id: fixture.process_id,
            expected_process_generation: fixture.process_generation,
            expected_process_fencing_token: fixture.process_fencing_token,
            binding: fiber,
            idempotency_key: key(seed),
            registered_at_ms: 3_000,
        }) {
        Ok(nlos_process::FiberIncarnationDecision::Registered(_)) => {}
        other => panic!("incarnation: {other:?}"),
    }
}

fn runtime(max_live_fibers: usize) -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime")
}

fn linked_spec(
    index: usize,
    scope: CancellationScopeId,
    process_id: ProcessId,
    process_generation: Generation,
) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: AgentInstanceId::from_bytes(id_bytes(index)),
        agent_generation: Generation::INITIAL,
        process_id,
        process_generation,
        task_attempt_id: None,
        cancellation_scope_id: scope,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(id_bytes(1)),
        scheduler_domain_id: SchedulerDomainId::from_bytes(id_bytes(1)),
        deadline: None,
    }
}

fn spawn(
    runtime: &TokioRuntimeAdapter,
    spec: FiberSpec,
    future: nlos_runtime::FiberFuture,
) -> FiberHandle {
    runtime.spawn_fiber(spec, future).expect("spawn")
}

async fn wait_for_state(runtime: &TokioRuntimeAdapter, handle: FiberHandle, expected: FiberState) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if runtime.inspect(handle) == Ok(expected) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fiber did not reach expected state");
}

async fn settle_and_assert_state(
    runtime: &TokioRuntimeAdapter,
    handle: FiberHandle,
    expected: FiberState,
) {
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        runtime.inspect(handle),
        Ok(expected),
        "state must be stable across the kill-receipt linkage"
    );
}

fn assert_health_delta(
    later: RuntimeHealth,
    earlier: RuntimeHealth,
    sweeps: u64,
    matched: u64,
    kills: u64,
) {
    assert_eq!(
        later.process_cancel_sweeps_total - earlier.process_cancel_sweeps_total,
        sweeps,
        "sweep meter delta"
    );
    assert_eq!(
        later.process_fiber_cancel_matched_total - earlier.process_fiber_cancel_matched_total,
        matched,
        "matched-fiber meter delta"
    );
    assert_eq!(
        later.platform_kills_consumed_total - earlier.platform_kills_consumed_total,
        kills,
        "kill-consumption meter delta"
    );
    assert_eq!(
        later.orphan_buffer_dropped_total, earlier.orphan_buffer_dropped_total,
        "the kill linkage never touches the orphan buffer"
    );
}

/// Happy path: a durable kill receipt plus a terminal binding is consumed —
/// the W27-C sweep drives the killed process's fibers to unique terminals,
/// the receipt rides in the report, and the lock-free meters observe the
/// linkage (an idempotent re-consumption meters again).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_receipt_consumption_drives_w27c_cancel_and_meters() {
    let root = Root::new("happy");
    let fixture = open_process_fixture(&root, 0x11);
    let runtime = runtime(8);
    let running_scope = CancellationScopeId::from_bytes(id_bytes(700));
    let done_scope = CancellationScopeId::from_bytes(id_bytes(701));

    let running = spawn(
        &runtime,
        linked_spec(
            1,
            running_scope,
            fixture.process_id,
            fixture.process_generation,
        ),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, running, FiberState::Running).await;
    let done = spawn(
        &runtime,
        linked_spec(
            2,
            done_scope,
            fixture.process_id,
            fixture.process_generation,
        ),
        Box::pin(async { FiberExit::Completed }),
    );
    wait_for_state(&runtime, done, FiberState::Completed).await;
    register_incarnation(&fixture, running.fiber_id, 0x50);

    let decision = fixture
        .process
        .request_platform_kill(
            kill_request(&fixture, 0x80),
            &StubPlatformKillAdapter::new(),
        )
        .expect("platform kill");
    assert!(matches!(decision, PlatformKillDecision::Signaled(_)));
    propagate_crash(&fixture, 0x81);

    let health_before = runtime.health();
    let report = runtime
        .consume_platform_kill(&fixture.process, propagation_request(&fixture, 0x81))
        .expect("consume platform kill");
    assert_eq!(report.receipt, *decision.receipt());
    assert_eq!(report.cancel.matched_fibers, 2);
    assert_eq!(report.cancel.already_terminal, 1);
    assert_eq!(report.cancel.canceled_scopes, 2);
    assert_eq!(report.cancel.vanished_scopes, 0);
    assert_health_delta(runtime.health(), health_before, 1, 2, 1);

    wait_for_state(&runtime, running, FiberState::Cancelled).await;
    settle_and_assert_state(&runtime, done, FiberState::Completed).await;

    let replay = runtime
        .consume_platform_kill(&fixture.process, propagation_request(&fixture, 0x81))
        .expect("idempotent re-consumption");
    assert!(
        matches!(
            replay.cancel.decision,
            nlos_process::FiberCancelPropagationDecision::Replayed(_)
        ),
        "the durable propagation replays"
    );
    assert_eq!(replay.receipt, report.receipt);
    assert_health_delta(runtime.health(), health_before, 2, 4, 2);
}

/// Fail-closed gate: without a durable kill receipt there is nothing to
/// consume — neither a plain crash nor an untouched active binding feeds
/// the runtime linkage, and the meters stay flat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_receipt_consumption_fails_closed_without_receipt() {
    let root = Root::new("absent");
    let fixture = open_process_fixture(&root, 0x21);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(710));

    let handle = spawn(
        &runtime,
        linked_spec(1, scope, fixture.process_id, fixture.process_generation),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, handle, FiberState::Running).await;
    let health_before = runtime.health();

    // (a) active binding, no kill at all.
    let active = runtime
        .consume_platform_kill(&fixture.process, propagation_request(&fixture, 0x82))
        .expect_err("no receipt on an active binding");
    assert!(matches!(
        active,
        ChannelWaitError::PlatformKillReceiptAbsent { process_id }
            if process_id == fixture.process_id
    ));
    settle_and_assert_state(&runtime, handle, FiberState::Running).await;
    assert_health_delta(runtime.health(), health_before, 0, 0, 0);

    // (b) a crash alone is still not kill evidence.
    propagate_crash(&fixture, 0x83);
    let crashed = runtime
        .consume_platform_kill(&fixture.process, propagation_request(&fixture, 0x83))
        .expect_err("crash without platform kill must not be consumable");
    assert!(matches!(
        crashed,
        ChannelWaitError::PlatformKillReceiptAbsent { .. }
    ));
    settle_and_assert_state(&runtime, handle, FiberState::Running).await;
    assert_health_delta(runtime.health(), health_before, 0, 0, 0);

    // The gates do not poison the runtime: the scope still cancels through
    // the plain W27-C path (kill evidence only gates the kill entry).
    runtime
        .cancel_process_fibers(&fixture.process, propagation_request(&fixture, 0x83))
        .expect("plain batch cancel still works");
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;
}

/// Fail-closed ordering: a receipt on a still-Active binding is rejected by
/// the W27-C durable gate (the binding must be terminal) with zero runtime
/// side effect; marking terminal afterwards unblocks the same consumption.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_receipt_consumption_requires_terminal_binding() {
    let root = Root::new("not-terminal");
    let fixture = open_process_fixture(&root, 0x31);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(720));

    let handle = spawn(
        &runtime,
        linked_spec(1, scope, fixture.process_id, fixture.process_generation),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, handle, FiberState::Running).await;

    let decision = fixture
        .process
        .request_platform_kill(
            kill_request(&fixture, 0x85),
            &StubPlatformKillAdapter::new(),
        )
        .expect("platform kill on the active binding");
    assert!(matches!(decision, PlatformKillDecision::Signaled(_)));

    let health_before = runtime.health();
    let not_terminal = runtime
        .consume_platform_kill(&fixture.process, propagation_request(&fixture, 0x86))
        .expect_err("an active binding must fail the W27-C gate");
    assert!(
        matches!(
            not_terminal,
            ChannelWaitError::ProcessAuthority(nlos_process::ProcessAuthorityError::CorruptRecord(
                _
            ))
        ),
        "expected the W27-C non-terminal rejection, got {not_terminal:?}"
    );
    settle_and_assert_state(&runtime, handle, FiberState::Running).await;
    assert_health_delta(runtime.health(), health_before, 0, 0, 0);

    propagate_crash(&fixture, 0x87);
    let report = runtime
        .consume_platform_kill(&fixture.process, propagation_request(&fixture, 0x87))
        .expect("consumption after the binding went terminal");
    assert_eq!(report.receipt, *decision.receipt());
    wait_for_state(&runtime, handle, FiberState::Cancelled).await;
    assert_health_delta(runtime.health(), health_before, 1, 1, 1);
}

/// Activation-meter linkage at the fiber level: a fiber parked in an
/// Operation wait keeps its accumulated `external_wait` (the kill-driven
/// terminal closes the open phase through the existing finalize seam), and
/// the kill meters observe the consumption.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_receipt_consumption_feeds_activation_meter_dimensions() {
    let root = Root::new("meter");
    let fixture = open_process_fixture(&root, 0x41);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(730));

    let handle = spawn(
        &runtime,
        linked_spec(1, scope, fixture.process_id, fixture.process_generation),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, handle, FiberState::Running).await;
    let operation = OperationId::from_bytes(id_bytes(41));
    // The registration (not the receiver future) drives the record's
    // WaitingIo phase; dropping the receiver keeps the durable entry armed.
    let _wait = runtime
        .wait_for_operation(handle, operation, Generation::INITIAL)
        .expect("operation wait");
    wait_for_state(&runtime, handle, FiberState::WaitingIo).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    fixture
        .process
        .request_platform_kill(
            kill_request(&fixture, 0x88),
            &StubPlatformKillAdapter::new(),
        )
        .expect("platform kill");
    propagate_crash(&fixture, 0x89);
    let health_before = runtime.health();
    runtime
        .consume_platform_kill(&fixture.process, propagation_request(&fixture, 0x89))
        .expect("consume platform kill");

    wait_for_state(&runtime, handle, FiberState::Cancelled).await;
    let usage = runtime.activation_usage(handle).expect("usage readback");
    assert!(
        usage.external_wait >= Duration::from_millis(40),
        "the kill-driven terminal must keep the accumulated external_wait, got {:?}",
        usage.external_wait
    );
    assert!(
        usage.elapsed_wall > Duration::ZERO,
        "elapsed_wall closes at the kill-driven terminal"
    );
    assert!(
        usage.active_cpu < usage.external_wait,
        "active_cpu={:?} must stay small vs external_wait={:?}",
        usage.active_cpu,
        usage.external_wait
    );
    assert_health_delta(runtime.health(), health_before, 1, 1, 1);
}

/// End-to-end POSIX chain: a supervisor-spawned real child is killed through
/// the authority's durable path (real SIGTERM via the POSIX adapter fed by
/// the supervisor pid registry), and the runtime consumes the kill receipt
/// to cancel the linked fibers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn kill_receipt_consumption_end_to_end_posix_real_child() {
    use nlos_process::{ProcessSupervisor, SpawnSupervisedRequest};

    let root = Root::new("posix-e2e");
    let fixture = open_process_fixture(&root, 0x51);
    let runtime = runtime(4);
    let scope = CancellationScopeId::from_bytes(id_bytes(740));

    let supervisor = ProcessSupervisor::new();
    let mut command = std::process::Command::new("sleep");
    command.arg("600");
    let mut child = supervisor
        .spawn_supervised(
            SpawnSupervisedRequest {
                process_id: fixture.process_id,
                process_generation: fixture.process_generation,
                registered_at_ms: 7_000,
            },
            &mut command,
        )
        .expect("supervisor spawns the real child");

    let handle = spawn(
        &runtime,
        linked_spec(1, scope, fixture.process_id, fixture.process_generation),
        Box::pin(pending()),
    );
    wait_for_state(&runtime, handle, FiberState::Running).await;

    let adapter = PosixPlatformKillAdapter::new(supervisor.registry().pid_map());
    let decision = fixture
        .process
        .request_platform_kill(kill_request(&fixture, 0x89), &adapter)
        .expect("real platform kill");
    assert!(matches!(decision, PlatformKillDecision::Signaled(_)));
    let status = tokio::task::spawn_blocking(move || child.child().wait())
        .await
        .expect("join waiter")
        .expect("child wait");
    assert!(!status.success(), "the real child must die to the SIGTERM");

    propagate_crash(&fixture, 0x8A);
    let health_before = runtime.health();
    let report: PlatformKillConsumptionReport = runtime
        .consume_platform_kill(&fixture.process, propagation_request(&fixture, 0x8A))
        .expect("consume the real kill receipt");
    assert_eq!(report.receipt, *decision.receipt());
    assert_eq!(report.cancel.matched_fibers, 1);

    wait_for_state(&runtime, handle, FiberState::Cancelled).await;
    assert_health_delta(runtime.health(), health_before, 1, 1, 1);
}
