//! B-SLICE-K-001: the first longitudinal slice end to end.
//!
//! Three tests, one per scenario lane:
//!
//! 1. `full_vertical_slice_produces_every_receipt_and_is_inspectable` —
//!    signed Package → verify → install Application → Task/Attempt →
//!    `CommitPermit` → tokio fiber (durable driver Operation + staged
//!    revision + commit plan) → converge → `TaskCommitReceipt` → inspect.
//! 2. `cancel_closes_attempt_fences_permit_and_runtime_scope` — the cancel
//!    path with no commit appearing.
//! 3. `drop_reopen_replays_durable_prefix_to_consistent_terminal_state` —
//!    the crash-recovery analogue: every authority dropped mid-chain,
//!    reopened over the same root, converged to the identical durable
//!    terminal state, idempotent under a second drain.
//! 4. `second_process_platform_kill_isolates_first_and_replays_after_reopen`
//!    (Unix) — ROAD-B-002 B2-1: one application driving TWO process
//!    bindings concurrently; the second process spawns (real OS child +
//!    runtime fiber + durable incarnation), is platform-killed through the
//!    real POSIX adapter fed by the supervisor pid registry, its binding
//!    goes terminal (crash propagation + W27-C batch-cancel linkage), the
//!    first process/fiber/OS child is untouched (isolation), and everything
//!    replays idempotently after a crash-drop + reopen. Non-Unix hosts run
//!    the same chain against the noop contract adapter.

use std::future::pending;
use std::sync::Arc;

use nlos_application::ApplicationStatus;
use nlos_artifact::{
    ContentDigest, PackageVerificationDecision, VerifyPackageRequest, package_manifest_message,
};
use nlos_process::{
    FiberCancelPropagationDecision, PlatformKillDecision, ProcessAuthorityError,
    ProcessLifecycleState, ProcessTerminalDecision, PropagateCancelToFibersRequest,
    PropagateCrashRequest, RegisterSupervisorPidRequest, RequestPlatformKillRequest,
    StubPlatformKillAdapter, SupervisorPidDecision, SupervisorPidRegistry,
};
use nlos_runtime::{FiberExit, FiberSpec, FiberState, RuntimeAdapter as _, RuntimeError};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{
    ChainQuery, SECOND_BINDING_SEED_OFFSET, SECOND_MATERIALIZE_SEED_OFFSET, SliceKRuntime,
    run_cancel_path, run_happy_chain, run_recovery_prefix, run_second_process_pair,
    run_second_process_platform_kill, seeded_key,
};
use nlos_task::{AttemptState, CancelDecision, PermitDecision, PermitState, TaskState};
use nlos_types::{ExecutionFiberId, Generation, ResourceGroupId, SchedulerDomainId};

fn slice_runtime(name: &str) -> (TempDir, Arc<SliceKRuntime>) {
    let dir = TempDir::new(name);
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    (dir, runtime)
}

fn slice_adapter() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("tokio adapter")
}

struct TempDir {
    root: std::path::PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove slice-k temp root: {error}"),
        }
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn full_vertical_slice_produces_every_receipt_and_is_inspectable() {
    let (_dir, runtime) = slice_runtime("happy");
    let adapter = slice_adapter();

    let chain = run_happy_chain(&runtime, &adapter, 0xA0)
        .await
        .expect("happy chain");

    // The digest-binding ladder, one authority per step.
    let verification = runtime
        .artifacts
        .inspect_package_verification_receipt(chain.verification_receipt_id)
        .expect("verification receipt readback");
    assert_eq!(verification.signer, chain.publisher.principal_id);
    assert_eq!(
        verification.manifest_digest,
        ContentDigest::from_bytes(package_manifest_message(&chain.package.manifest))
    );

    let installation = runtime
        .applications
        .inspect_installation(chain.installation_id)
        .expect("installation readback");
    assert_eq!(
        installation.package_verification_receipt_id,
        chain.verification_receipt_id
    );
    assert_eq!(
        installation.package_manifest_digest,
        verification.manifest_digest
    );

    // The fiber's write landed as revision 2 (the package payload is 1).
    let head = runtime
        .artifacts
        .resolve_head(chain.package.payload_artifact, u64::MAX)
        .expect("head readback")
        .expect("head after commit");
    assert_eq!(head.revision, 2);

    // Terminal task facts: permit consumed, head advanced, attempt committed.
    let task = runtime.tasks.inspect_task(chain.task_id).expect("task");
    assert_eq!(task.head_commit_seq, 1);
    assert_eq!(task.state, TaskState::Active);
    let attempt = runtime
        .tasks
        .inspect_attempt(chain.task_id, chain.attempt_id)
        .expect("attempt");
    assert_eq!(attempt.state, AttemptState::Committed);
    let permit = runtime
        .tasks
        .inspect_permit(chain.task_id, chain.permit_id)
        .expect("permit");
    assert_eq!(permit.state, PermitState::Closed);

    // The receipt binds the whole ladder.
    assert_eq!(chain.receipt.task_receipt.task_id, chain.task_id);
    assert_eq!(chain.receipt.task_receipt.permit_id, Some(chain.permit_id));
    assert_eq!(chain.receipt.artifact_publications.len(), 1);

    // The fiber ran under an authority-registered process binding, not a
    // fabricated id: the durable record readback agrees with the spec.
    assert_eq!(chain.process.process_generation, Generation::INITIAL);
    let binding = runtime
        .process
        .inspect_active_process_binding(chain.process.process_id)
        .expect("process binding readback");
    assert_eq!(binding, chain.process);
    assert_eq!(binding.task_id, chain.task_id);
    assert_eq!(binding.task_attempt_id, chain.attempt_id);

    // The application is durably installed.
    let application = runtime
        .applications
        .inspect_application(chain.package.package_id)
        .expect("application readback")
        .expect("installed application");
    assert_eq!(application.status, ApplicationStatus::Installed);
    assert_eq!(application.application_id, chain.application_id);

    // In-process inspect sees the same facts from the authorities alone.
    let inspect = runtime
        .inspect_chain(ChainQuery {
            package_id: chain.package.package_id,
            installation_id: Some(chain.installation_id),
            process_id: Some(chain.process.process_id),
            task_id: chain.task_id,
            attempt_id: chain.attempt_id,
            permit_id: Some(chain.permit_id),
            artifact_id: chain.package.payload_artifact,
            operation: Some(chain.outcome.operation),
        })
        .expect("inspect");
    assert_eq!(inspect.task.head_commit_seq, 1);
    assert_eq!(inspect.artifact_head.as_ref().expect("head").revision, 2);
    assert_eq!(
        inspect.application.as_ref().expect("app").status,
        ApplicationStatus::Installed
    );
    assert_eq!(
        inspect.process.as_ref().expect("process binding"),
        &chain.process
    );
    let lines = inspect.report_lines();
    assert!(lines.iter().any(|line| line.contains("head_commit_seq=1")));
    assert!(lines.iter().any(|line| line.contains("revision=2")));
    assert!(lines.iter().any(|line| line.contains(&format!(
        "process={} generation={}",
        nlos_slice_k::short_hex(chain.process.process_id.as_bytes()),
        chain.process.process_generation.get()
    ))));

    // ROAD-B-002 (W20-002): registration inspect wired into the full chain —
    // the happy-chain Task and Process binding are registered against the
    // installed application, then read back via the aggregated inspect path.
    let background = runtime
        .register_background_task(
            chain.package.package_id,
            chain.task_id,
            chain.publisher.principal_id,
            0xA0,
        )
        .expect("register background task");
    let binding = runtime
        .register_process_binding(
            chain.package.package_id,
            chain.process.process_id,
            chain.publisher.principal_id,
            0xA0,
        )
        .expect("register process binding");
    let registrations = runtime
        .inspect_application_registrations(chain.package.package_id)
        .expect("inspect application registrations");
    assert_eq!(registrations.background_tasks.len(), 1);
    assert_eq!(registrations.process_bindings.len(), 1);
    assert_eq!(registrations.background_tasks[0], background);
    assert_eq!(registrations.process_bindings[0], binding);
    assert_eq!(registrations.background_tasks[0].task_id, chain.task_id);
    assert_eq!(
        registrations.process_bindings[0].process_id,
        chain.process.process_id
    );
    let reg_lines = registrations.report_lines();
    assert!(reg_lines.iter().any(|line| line == "background_tasks=1"));
    assert!(reg_lines.iter().any(|line| line == "process_bindings=1"));
}

#[tokio::test]
async fn cancel_closes_attempt_fences_permit_and_runtime_scope() {
    let (_dir, runtime) = slice_runtime("cancel");
    let adapter = slice_adapter();

    let facts = run_cancel_path(&runtime, &adapter, 0xB0)
        .await
        .expect("cancel path");

    let CancelDecision::Applied {
        cancel_epoch,
        closed_attempts,
    } = facts.cancel
    else {
        panic!("fresh task must cancel as Applied");
    };
    assert_eq!(cancel_epoch, 1);
    assert_eq!(closed_attempts.len(), 1);
    assert_eq!(closed_attempts[0].attempt_id, facts.attempt_id);

    // The task authority durably fences any later permit request.
    assert!(matches!(
        facts.fenced_permit,
        PermitDecision::CancelledBeforeEffect { .. }
    ));

    // The runtime scope of the cancelled attempt refuses new fibers.
    let fiber_spec = FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes([0xB0u8.wrapping_add(200); 16]),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: facts.process.agent_instance_id,
        agent_generation: facts.process.agent_instance_generation,
        process_id: facts.process.process_id,
        process_generation: facts.process.process_generation,
        task_attempt_id: Some(facts.attempt_id),
        cancellation_scope_id: facts.scope_id,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes([0xB3; 16]),
        scheduler_domain_id: SchedulerDomainId::from_bytes([0xB4; 16]),
        deadline: None,
    };
    assert!(matches!(
        adapter.spawn_fiber(fiber_spec, Box::pin(async { FiberExit::Completed })),
        Err(RuntimeError::Cancelled)
    ));

    // Durable terminal state: cancelled task, no commit anywhere.
    let task = runtime.tasks.inspect_task(facts.task_id).expect("task");
    assert_eq!(task.state, TaskState::Cancelled);
    assert_eq!(task.cancel_epoch, 1);
    assert_eq!(task.head_commit_seq, 0);
    let attempt = runtime
        .tasks
        .inspect_attempt(facts.task_id, facts.attempt_id)
        .expect("attempt");
    assert_eq!(attempt.state, AttemptState::Cancelled);
    assert_eq!(facts.converged_plans, 0, "no plan ever existed");
    assert_eq!(
        adapter.inspect(facts.fiber).expect("fiber state"),
        FiberState::Completed,
        "the operation-only prefix completed before the cancel"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn drop_reopen_replays_durable_prefix_to_consistent_terminal_state() {
    let (dir, runtime) = slice_runtime("recovery");
    let adapter = slice_adapter();

    // Pre-crash: everything through the fiber's durable prefix, no converge.
    let prefix = run_recovery_prefix(&runtime, &adapter, 0xC0)
        .await
        .expect("recovery prefix");
    let pre_verification = runtime
        .artifacts
        .inspect_package_verification_receipt(prefix.verification_receipt_id)
        .expect("pre-crash verification receipt");
    let pre_installation = runtime
        .applications
        .inspect_installation(prefix.installation_id)
        .expect("pre-crash installation");
    let pre_task = runtime
        .tasks
        .inspect_task(prefix.task_id)
        .expect("pre-crash task");
    assert_eq!(pre_task.head_commit_seq, 0, "not converged yet");
    let pre_head = runtime
        .artifacts
        .resolve_head(prefix.artifact_id, u64::MAX)
        .expect("pre-crash head")
        .expect("package head");
    assert_eq!(pre_head.revision, 1, "only the package payload head");
    let pre_operation = runtime
        .operations
        .inspect(prefix.operation)
        .expect("pre-crash operation");
    let pre_operation_state = pre_operation.state;
    let pre_binding = runtime
        .process
        .inspect_active_process_binding(prefix.process.process_id)
        .expect("pre-crash process binding");
    assert_eq!(pre_binding, prefix.process);

    // kill -9 analogue: the runtime adapter and every authority handle are
    // dropped without any close; the durable bytes under the root survive.
    drop(adapter);
    drop(runtime);
    let root = dir.root().to_path_buf();

    // Reopen every authority over the same root.
    let reopened = Arc::new(SliceKRuntime::open(&root).expect("reopen slice-k runtime"));

    // Nothing advanced, nothing vanished: the durable prefix is whole.
    let reopened_task = reopened.tasks.inspect_task(prefix.task_id).expect("task");
    assert_eq!(reopened_task.head_commit_seq, 0);
    assert_eq!(
        reopened
            .artifacts
            .resolve_head(prefix.artifact_id, u64::MAX)
            .expect("head")
            .expect("head")
            .revision,
        1
    );
    assert_eq!(
        reopened
            .applications
            .inspect_installation(prefix.installation_id)
            .expect("installation"),
        pre_installation
    );
    let reopened_operation = reopened
        .operations
        .inspect(prefix.operation)
        .expect("reopened operation");
    assert_eq!(reopened_operation.state, pre_operation_state);

    // The process binding is a durable fact of the crash too: the reopened
    // authority readback returns the identical binding with an unchanged
    // generation, and the same registration replays idempotently.
    let reopened_binding = reopened
        .process
        .inspect_active_process_binding(prefix.process.process_id)
        .expect("reopened process binding");
    assert_eq!(reopened_binding, pre_binding);
    assert_eq!(
        reopened_binding.process_generation,
        prefix.process.process_generation
    );
    let replay = reopened
        .materialize_process(0xC0, prefix.task_id, prefix.attempt_id, Generation::INITIAL)
        .expect("replay process materialization after reopen");
    assert_eq!(replay, prefix.process);

    // Fiber replay/收敛: the coordinator replays the durable prefix
    // (staged revision + commit plan) to the terminal state.
    let now_ms = reopened
        .wall_now_i64(seeded_key(0xC0, 95))
        .expect("post-reopen wall reading");
    let receipts = reopened
        .converge_pending(16, now_ms)
        .expect("converge after reopen");
    assert_eq!(receipts.len(), 1, "exactly the prefix plan finalizes");
    let receipt = &receipts[0];
    assert_eq!(receipt.task_receipt.task_id, prefix.task_id);
    assert_eq!(receipt.task_receipt.permit_id, Some(prefix.permit_id));
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_eq!(receipt.artifact_publications.len(), 1);

    // Durable terminal state agrees with the receipt everywhere.
    let post_task = reopened.tasks.inspect_task(prefix.task_id).expect("task");
    assert_eq!(post_task.head_commit_seq, 1);
    let post_attempt = reopened
        .tasks
        .inspect_attempt(prefix.task_id, prefix.attempt_id)
        .expect("attempt");
    assert_eq!(post_attempt.state, AttemptState::Committed);
    let post_head = reopened
        .artifacts
        .resolve_head(prefix.artifact_id, u64::MAX)
        .expect("head")
        .expect("head");
    assert_eq!(post_head.revision, 2);
    assert_eq!(
        post_head.digest.as_bytes(),
        &receipt.artifact_publications[0].digest
    );

    // A second drain is a no-op: no double commit.
    let later = reopened
        .wall_now_i64(seeded_key(0xC0, 96))
        .expect("wall reading");
    assert!(
        reopened
            .converge_pending(16, later)
            .expect("second drain")
            .is_empty()
    );

    // The package verification receipt is the durable authority after the
    // crash too: the same verify request replays byte-identically.
    let reopened_publisher = reopened.bootstrap_publisher(0xC0).expect("publisher");
    let decision = reopened
        .artifacts
        .verify_package(
            &reopened.identity,
            VerifyPackageRequest {
                signed: &prefix.signed,
                idempotency_key: seeded_key(0xC0, 14),
                verified_at_ms: pre_verification.verified_at_ms,
            },
        )
        .expect("verify replay after reopen");
    assert!(matches!(decision, PackageVerificationDecision::Replayed(_)));
    assert_eq!(decision.receipt(), &pre_verification);
    assert_eq!(
        reopened_publisher.principal_id, pre_verification.signer,
        "identity authority reopened with the same principal"
    );
}

/// Polls the adapter until the fiber reaches `expected` (the
/// `batch_cancel.rs` wait pattern; bounded by a generous timeout).
async fn wait_for_fiber_state(
    adapter: &TokioRuntimeAdapter,
    handle: nlos_runtime::FiberHandle,
    expected: FiberState,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if adapter.inspect(handle) == Ok(expected) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fiber did not reach expected state");
}

/// One live-fiber spec linked to `process` under its attempt's scope (the
/// chain's live-execution shape; distinct id offsets from the chain band).
fn live_probe_spec(
    seed: u8,
    fiber_offset: u8,
    scope: nlos_types::CancellationScopeId,
    attempt: nlos_types::TaskAttemptId,
    process: &nlos_process::ProcessBindingRecord,
) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes([seed.wrapping_add(fiber_offset); 16]),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: process.agent_instance_id,
        agent_generation: process.agent_instance_generation,
        process_id: process.process_id,
        process_generation: process.process_generation,
        task_attempt_id: Some(attempt),
        cancellation_scope_id: scope,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes([seed.wrapping_add(160); 16]),
        scheduler_domain_id: SchedulerDomainId::from_bytes([seed.wrapping_add(161); 16]),
        deadline: None,
    }
}

/// Shared body of the second-process kill chain (B2-1): runs the pair
/// spawn phase, asserts both processes alive/inspectable, runs the
/// platform-kill chain, asserts the kill terminal facts and the first
/// process's isolation, then crash-drops, reopens, and asserts the
/// idempotent replay of bindings / pids / kills. `real_children` carries
/// the OS children on Unix (`None` on the non-Unix contract lane).
#[allow(clippy::too_many_lines)]
async fn second_process_kill_chain_body(
    dir: &TempDir,
    seed: u8,
    mut real_children: Option<(std::process::Child, std::process::Child)>,
) {
    let (runtime, adapter) = {
        let slice = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
        let adapter = slice_adapter();
        (slice, adapter)
    };
    let (os_pid_first, os_pid_second) = match &mut real_children {
        Some((first, second)) => (first.id(), second.id()),
        None => (std::process::id(), std::process::id()),
    };

    let pair = run_second_process_pair(&runtime, &adapter, seed, os_pid_first, os_pid_second)
        .await
        .expect("second process pair");
    let second = &pair.process_second;
    let first = &pair.process_first;

    // Both processes of the one application are alive and inspectable:
    // active durable bindings, two registered application bindings, two
    // supervisor pid entries, live runtime fibers, a completed durable
    // write under the second process, and (on Unix) two live OS children.
    assert_eq!(
        runtime
            .process
            .inspect_active_process_binding(first.process_id)
            .expect("first binding active"),
        *first
    );
    assert_eq!(
        runtime
            .process
            .inspect_active_process_binding(second.process_id)
            .expect("second binding active before the kill"),
        *second
    );
    let registrations = runtime
        .inspect_application_registrations(pair.package_id)
        .expect("registration inspect before the kill");
    assert_eq!(registrations.process_bindings.len(), 2);
    assert!(
        registrations
            .process_bindings
            .contains(&pair.binding_receipt_first)
    );
    assert!(
        registrations
            .process_bindings
            .contains(&pair.binding_receipt_second)
    );
    assert_eq!(pair.registry.pid_map().len(), 2);
    wait_for_fiber_state(&adapter, pair.fiber_first, FiberState::Running).await;
    wait_for_fiber_state(&adapter, pair.fiber_second, FiberState::Running).await;
    wait_for_fiber_state(&adapter, pair.fiber_second_write, FiberState::Completed).await;
    assert!(pair.write_outcome_second.plan_id.is_none());
    runtime
        .process
        .inspect_fiber_incarnation(second.process_id, pair.fiber_second.fiber_id)
        .expect("second incarnation inspectable while active");
    if let Some((first_child, second_child)) = &mut real_children {
        assert!(first_child.try_wait().expect("first child alive").is_none());
        assert!(
            second_child
                .try_wait()
                .expect("second child alive")
                .is_none()
        );
    }

    // The kill chain: durable receipt → real adapter signal → binding
    // terminal (crash propagation, auto batch-cancel receipts) → W27-C
    // runtime linkage.
    let kill = run_second_process_platform_kill(&runtime, &adapter, &pair)
        .await
        .expect("second process platform kill chain");
    assert!(
        matches!(kill.kill, PlatformKillDecision::Signaled(_)),
        "the kill chain must report Signaled, got {:?}",
        kill.kill
    );
    if let Some((_, second_child)) = &mut real_children {
        let status = second_child.wait().expect("wait for killed child");
        assert!(!status.success(), "the real OS child died by signal");
    }

    assert_eq!(kill.linkage.matched_fibers, 2);
    assert_eq!(kill.linkage.already_terminal, 1);
    assert_eq!(kill.linkage.canceled_scopes, 1);
    assert_eq!(kill.linkage.vanished_scopes, 0);
    assert_eq!(kill.linkage.decision.receipts().len(), 1);
    assert_eq!(
        kill.linkage.decision.receipts()[0].binding,
        pair.fiber_second.fiber_id
    );

    wait_for_fiber_state(&adapter, pair.fiber_second, FiberState::Cancelled).await;
    assert_eq!(
        adapter.inspect(pair.fiber_second_write),
        Ok(FiberState::Completed)
    );

    // Isolation: the first process is untouched at every level — OS child,
    // runtime fiber, cancellation scope, durable binding and incarnation.
    if let Some((first_child, _)) = &mut real_children {
        assert!(
            first_child
                .try_wait()
                .expect("first child still alive")
                .is_none(),
            "the SIGTERM must reach only the second process's os pid"
        );
    }
    assert_eq!(
        adapter.inspect(pair.fiber_first),
        Ok(FiberState::Running),
        "the first process's live fiber keeps running"
    );
    let isolation_probe = adapter
        .spawn_fiber(
            live_probe_spec(seed, 151, pair.scope_first, pair.attempt_id_first, first),
            Box::pin(pending()),
        )
        .expect("the first attempt's scope still admits fibers");
    wait_for_fiber_state(&adapter, isolation_probe, FiberState::Running).await;
    assert!(matches!(
        adapter.spawn_fiber(
            live_probe_spec(seed, 152, pair.scope_second, pair.attempt_id_second, second,),
            Box::pin(pending()),
        ),
        Err(RuntimeError::Cancelled)
    ));
    assert_eq!(
        runtime
            .process
            .inspect_active_process_binding(first.process_id)
            .expect("first binding stays active"),
        *first
    );
    runtime
        .process
        .inspect_fiber_incarnation(first.process_id, pair.fiber_first.fiber_id)
        .expect("first incarnation is not cancelled");

    // Terminal facts of the second process: binding fail-closed, durable
    // kill receipt and terminal marker readback.
    assert!(matches!(
        runtime
            .process
            .inspect_active_process_binding(second.process_id),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed
        ))
    ));
    assert!(matches!(
        runtime
            .process
            .inspect_fiber_incarnation(second.process_id, pair.fiber_second.fiber_id),
        Err(ProcessAuthorityError::FiberIncarnationCancelled(
            ProcessLifecycleState::Crashed
        ))
    ));
    assert_eq!(
        runtime
            .process
            .inspect_platform_kill_receipt(second.process_id, second.process_generation)
            .expect("kill receipt inspect"),
        Some(kill.kill.receipt().clone())
    );
    assert_eq!(
        runtime
            .process
            .inspect_process_terminal(second.process_id)
            .expect("terminal marker inspect"),
        Some(kill.crash.clone())
    );

    if let Some((first_child, _)) = &mut real_children {
        first_child.kill().expect("cleanup first child");
        first_child.wait().expect("reap first child");
    }

    // Crash-drop analogue: every authority and the runtime dropped without
    // any close; the durable bytes under the root survive.
    drop(adapter);
    drop(runtime);
    let reopened = Arc::new(SliceKRuntime::open(dir.root()).expect("reopen slice-k runtime"));
    let fresh_adapter = slice_adapter();

    // The terminal state of the second process and the aliveness of the
    // first survive the reopen verbatim.
    assert!(matches!(
        reopened
            .process
            .inspect_active_process_binding(second.process_id),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed
        ))
    ));
    assert_eq!(
        reopened
            .process
            .inspect_active_process_binding(first.process_id)
            .expect("first binding active after reopen"),
        *first
    );
    assert_eq!(
        reopened
            .process
            .inspect_platform_kill_receipt(second.process_id, second.process_generation)
            .expect("kill receipt after reopen"),
        Some(kill.kill.receipt().clone())
    );
    assert_eq!(
        reopened
            .process
            .inspect_process_terminal(second.process_id)
            .expect("terminal marker after reopen"),
        Some(kill.crash.clone())
    );
    assert!(matches!(
        reopened
            .process
            .inspect_fiber_incarnation(second.process_id, pair.fiber_second.fiber_id),
        Err(ProcessAuthorityError::FiberIncarnationCancelled(
            ProcessLifecycleState::Crashed
        ))
    ));
    reopened
        .process
        .inspect_fiber_incarnation(first.process_id, pair.fiber_first.fiber_id)
        .expect("first incarnation survives the reopen");

    // Application process bindings replay idempotently: both receipts are
    // still registered and the same registrations replay byte-identically.
    let replay_first = reopened
        .register_process_binding(
            pair.package_id,
            first.process_id,
            pair.registrant_principal,
            seed,
        )
        .expect("first binding registration replay");
    assert_eq!(replay_first, pair.binding_receipt_first);
    let replay_second = reopened
        .register_process_binding(
            pair.package_id,
            second.process_id,
            pair.registrant_principal,
            seed.wrapping_add(SECOND_BINDING_SEED_OFFSET),
        )
        .expect("second binding registration replay");
    assert_eq!(replay_second, pair.binding_receipt_second);
    let reopened_registrations = reopened
        .inspect_application_registrations(pair.package_id)
        .expect("registration inspect after reopen");
    assert_eq!(reopened_registrations.process_bindings.len(), 2);

    // Process materialization replays idempotently: both delegated
    // bindings return byte-identically under their original seeds.
    assert_eq!(
        reopened
            .materialize_process(
                seed,
                pair.task_id_first,
                pair.attempt_id_first,
                Generation::INITIAL
            )
            .expect("first materialization replay"),
        *first
    );
    assert_eq!(
        reopened
            .materialize_process(
                seed.wrapping_add(SECOND_MATERIALIZE_SEED_OFFSET),
                pair.task_id_second,
                pair.attempt_id_second,
                Generation::INITIAL,
            )
            .expect("second materialization replay"),
        *second
    );

    // The kill replays while STILL driving the adapter (at-least-once): a
    // fresh recording stub accepts exactly one supplementary signal for
    // the killed second process — a short-circuiting replay would record
    // zero — and the decision replays the byte-identical receipt (a broken
    // replay would fail closed or re-derive a distinct receipt).
    let replay_adapter = StubPlatformKillAdapter::new();
    let kill_replay = reopened
        .process
        .request_platform_kill(
            RequestPlatformKillRequest {
                process_id: kill.kill.receipt().process_id,
                expected_process_generation: kill.kill.receipt().process_generation,
                expected_process_fencing_token: kill.kill.receipt().process_fencing_token,
                idempotency_key: kill.kill.receipt().idempotency_key,
                killed_at_ms: kill.kill.receipt().killed_at_ms,
            },
            &replay_adapter,
        )
        .expect("kill replay after reopen");
    assert!(matches!(kill_replay, PlatformKillDecision::Replayed(_)));
    assert_eq!(kill_replay.receipt(), kill.kill.receipt());
    assert_eq!(
        replay_adapter.recorded_signals(),
        vec![(
            kill.kill.receipt().process_id,
            kill.kill.receipt().process_generation
        )],
        "the replayed kill re-signals the dead process through the adapter"
    );

    let crash_replay = reopened
        .process
        .propagate_crash(PropagateCrashRequest {
            process_id: kill.crash.process_id,
            expected_process_generation: kill.crash.process_generation,
            expected_process_fencing_token: kill.crash.process_fencing_token,
            idempotency_key: kill.crash.idempotency_key,
            marked_at_ms: kill.crash.marked_at_ms,
        })
        .expect("crash marker replay after reopen");
    assert!(matches!(crash_replay, ProcessTerminalDecision::Replayed(_)));
    assert_eq!(crash_replay.record(), &kill.crash);

    // The W27-C linkage replays on the fresh runtime: the durable batch is
    // Replayed with identical receipts and the empty runtime registry
    // matches zero live fibers.
    let linkage_replay = fresh_adapter
        .cancel_process_fibers(
            &reopened.process,
            PropagateCancelToFibersRequest {
                process_id: second.process_id,
                expected_process_generation: second.process_generation,
                expected_process_fencing_token: second.process_fencing_token,
                lifecycle_state: ProcessLifecycleState::Crashed,
                idempotency_key: kill.crash.idempotency_key,
                cancelled_at_ms: kill.crash.marked_at_ms,
            },
        )
        .expect("linkage replay after reopen");
    assert!(matches!(
        linkage_replay.decision,
        FiberCancelPropagationDecision::Replayed(_)
    ));
    assert_eq!(linkage_replay.matched_fibers, 0);
    assert_eq!(linkage_replay.canceled_scopes, 0);
    assert_eq!(
        linkage_replay.decision.receipts(),
        kill.linkage.decision.receipts()
    );

    // The supervisor registry is in-memory: after the restart a supervisor
    // re-registers the surviving first process idempotently.
    let restarted = SupervisorPidRegistry::new();
    restarted
        .register(RegisterSupervisorPidRequest {
            process_id: first.process_id,
            process_generation: first.process_generation,
            os_pid: os_pid_first,
            registered_at_ms: pair.supervisor_registered_at_ms,
        })
        .expect("restart registers the surviving process");
    assert!(matches!(
        restarted.register(RegisterSupervisorPidRequest {
            process_id: first.process_id,
            process_generation: first.process_generation,
            os_pid: os_pid_first,
            registered_at_ms: pair.supervisor_registered_at_ms,
        }),
        Ok(SupervisorPidDecision::Replayed(_))
    ));

    // No commit plan ever existed on this chain; the converge drain is the
    // idempotent no-op.
    let now_ms = reopened
        .wall_now_i64(seeded_key(seed, 150))
        .expect("post-reopen wall reading");
    assert!(
        reopened
            .converge_pending(16, now_ms)
            .expect("drain after reopen")
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn second_process_platform_kill_isolates_first_and_replays_after_reopen() {
    let dir = TempDir::new("second-kill");
    let child_first = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn first real child");
    let child_second = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn second real child");

    second_process_kill_chain_body(&dir, 0xD0, Some((child_first, child_second))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(not(unix))]
async fn second_process_kill_chain_contract_via_noop_adapter_on_non_unix() {
    let dir = TempDir::new("second-kill-contract");
    second_process_kill_chain_body(&dir, 0xD0, None).await;
}
