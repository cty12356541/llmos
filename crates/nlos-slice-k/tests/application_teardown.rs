//! W30-D lane: uninstall-driven Task/Process teardown over the
//! second-process spawn chain. One application owns two associated Tasks
//! (durable `TaskSpec.application_id`, §4 gap 1 closure) and two Process
//! bindings; the W27-D gated uninstall is refused while both Tasks are
//! outstanding, the teardown chain drives every binding through the W29-F
//! kill path (platform kill → crash terminal → W27-C linkage) and every
//! Task through the `cancel_task` fence, the gate then opens and the
//! uninstall commits — and re-running the whole teardown replays the
//! identical durable receipts while STILL driving the kill adapter
//! (at-least-once signal delivery: a replay through an EMPTY pid map now
//! fails closed on the missing mapping, and a replay through the real
//! registry re-signals the children — the already-dead ones map ESRCH to
//! `AlreadyTerminated` success). Everything survives a crash-drop + reopen,
//! including the task-row association.

use std::future::pending;
use std::sync::Arc;

use nlos_application::{ApplicationAuthorityError, ApplicationStatus};
use nlos_process::{
    FiberCancelPropagationDecision, PlatformKillDecision, ProcessAuthorityError,
    ProcessLifecycleState, ProcessTerminalDecision, SupervisorPidRegistry,
};
use nlos_runtime::{FiberSpec, FiberState, RuntimeAdapter as _, RuntimeError};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{SliceKError, SliceKRuntime, run_application_teardown, run_second_process_pair};
use nlos_task::{AttemptState, CancelDecision, TaskState};
use nlos_types::{ApplicationId, ExecutionFiberId, Generation, ResourceGroupId, SchedulerDomainId};

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

fn refusal_probe_spec(
    scope: nlos_types::CancellationScopeId,
    attempt: nlos_types::TaskAttemptId,
    process: &nlos_process::ProcessBindingRecord,
) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes([0x9A; 16]),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: process.agent_instance_id,
        agent_generation: process.agent_instance_generation,
        process_id: process.process_id,
        process_generation: process.process_generation,
        task_attempt_id: Some(attempt),
        cancellation_scope_id: scope,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes([0x9B; 16]),
        scheduler_domain_id: SchedulerDomainId::from_bytes([0x9C; 16]),
        deadline: None,
    }
}

/// Unix-only fail-closed probe registry: empty, so every platform kill
/// replay that consults the adapter fails on the missing mapping.
#[cfg(unix)]
fn empty_supervisor() -> SupervisorPidRegistry {
    SupervisorPidRegistry::new()
}

/// Shared body: see the module doc. `real_children` carries the two OS
/// children on Unix (`None` on the non-Unix contract lane).
#[allow(clippy::too_many_lines)]
async fn uninstall_teardown_body(
    dir: &TempDir,
    mut real_children: Option<(std::process::Child, std::process::Child)>,
) {
    let seed = 0xE0_u8;
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    let adapter = slice_adapter();
    let (os_pid_first, os_pid_second) = match &mut real_children {
        Some((first, second)) => (first.id(), second.id()),
        None => (std::process::id(), std::process::id()),
    };

    let pair = run_second_process_pair(&runtime, &adapter, seed, os_pid_first, os_pid_second)
        .await
        .expect("second process pair");

    // 关联下沉 (§4 gap 1): both Tasks carry the application association in
    // their DURABLE task rows — the association the slice previously could
    // only hold in its own orchestration struct.
    let task_first_row = runtime.tasks.inspect_task(pair.task_id_first).unwrap();
    assert_eq!(task_first_row.application_id, Some(pair.application_id));
    assert_eq!(task_first_row.plan_revision, None);
    let task_second_row = runtime.tasks.inspect_task(pair.task_id_second).unwrap();
    assert_eq!(task_second_row.application_id, Some(pair.application_id));

    // The W27-D gate is closed while the registered Tasks are outstanding.
    assert_eq!(
        runtime
            .tasks
            .inspect_outstanding_task_count(&[pair.task_id_first, pair.task_id_second])
            .unwrap(),
        2
    );
    let refused = runtime
        .uninstall_application_gated_by_task_activity(pair.package_id, seed)
        .expect_err("gate must refuse while tasks are outstanding");
    assert!(
        matches!(
            &refused,
            SliceKError::Application(ApplicationAuthorityError::ApplicationActiveTasksRunning {
                active_task_count: 2,
                ..
            })
        ),
        "unexpected refusal: {refused:?}"
    );
    assert_eq!(
        runtime
            .applications
            .inspect_application(pair.package_id)
            .unwrap()
            .expect("application")
            .status,
        ApplicationStatus::Installed,
        "a refused uninstall takes no durable state"
    );

    // Teardown, first run: every binding killed → terminal → linked, every
    // Task cancelled, then the gated uninstall commits.
    let teardown =
        run_application_teardown(&runtime, &adapter, pair.package_id, seed, &pair.registry)
            .expect("application teardown");
    assert_eq!(teardown.application_id, pair.application_id);
    assert_eq!(teardown.kills.len(), 2);
    assert_eq!(teardown.crashes.len(), 2);
    assert_eq!(teardown.linkages.len(), 2);
    assert_eq!(teardown.task_cancels.len(), 2);
    for kill in &teardown.kills {
        assert!(
            matches!(kill, PlatformKillDecision::Signaled(_)),
            "fresh teardown kills must signal, got {kill:?}"
        );
    }
    for crash in &teardown.crashes {
        assert_eq!(crash.lifecycle_state, ProcessLifecycleState::Crashed);
    }
    for linkage in &teardown.linkages {
        assert_eq!(linkage.canceled_scopes, 1);
        assert_eq!(linkage.vanished_scopes, 0);
        assert_eq!(linkage.decision.receipts().len(), 1);
    }
    for (index, cancel) in teardown.task_cancels.iter().enumerate() {
        let CancelDecision::Applied {
            cancel_epoch,
            closed_attempts,
        } = cancel
        else {
            panic!("fresh teardown cancel #{index} must apply: {cancel:?}");
        };
        assert_eq!(*cancel_epoch, 1);
        assert_eq!(closed_attempts.len(), 1);
    }
    if let Some((_, second_child)) = &mut real_children {
        let status = second_child.wait().expect("wait for killed child");
        assert!(!status.success(), "the real OS child died by signal");
    }

    // Runtime side: both live fibers cancelled, the durable write fiber
    // untouched, both scopes fenced.
    wait_for_fiber_state(&adapter, pair.fiber_first, FiberState::Cancelled).await;
    wait_for_fiber_state(&adapter, pair.fiber_second, FiberState::Cancelled).await;
    assert_eq!(
        adapter.inspect(pair.fiber_second_write),
        Ok(FiberState::Completed)
    );
    assert!(matches!(
        adapter.spawn_fiber(
            refusal_probe_spec(pair.scope_first, pair.attempt_id_first, &pair.process_first),
            Box::pin(pending()),
        ),
        Err(RuntimeError::Cancelled)
    ));
    assert!(matches!(
        adapter.spawn_fiber(
            refusal_probe_spec(
                pair.scope_second,
                pair.attempt_id_second,
                &pair.process_second
            ),
            Box::pin(pending()),
        ),
        Err(RuntimeError::Cancelled)
    ));

    // Durable side: bindings and incarnations terminal, Tasks and Attempts
    // cancelled, the gate now counts zero outstanding.
    for process in [&pair.process_first, &pair.process_second] {
        assert!(matches!(
            runtime
                .process
                .inspect_active_process_binding(process.process_id),
            Err(ProcessAuthorityError::ProcessBindingTerminal(
                ProcessLifecycleState::Crashed
            ))
        ));
    }
    assert!(matches!(
        runtime
            .process
            .inspect_fiber_incarnation(pair.process_first.process_id, pair.fiber_first.fiber_id),
        Err(ProcessAuthorityError::FiberIncarnationCancelled(
            ProcessLifecycleState::Crashed
        ))
    ));
    for task_id in [pair.task_id_first, pair.task_id_second] {
        let row = runtime.tasks.inspect_task(task_id).unwrap();
        assert_eq!(row.state, TaskState::Cancelled);
        assert_eq!(row.cancel_epoch, 1);
    }
    let attempt_first = runtime
        .tasks
        .inspect_attempt(pair.task_id_first, pair.attempt_id_first)
        .unwrap();
    assert_eq!(attempt_first.state, AttemptState::Cancelled);
    assert_eq!(
        runtime
            .tasks
            .inspect_outstanding_task_count(&[pair.task_id_first, pair.task_id_second])
            .unwrap(),
        0
    );
    let application = runtime
        .applications
        .inspect_application(pair.package_id)
        .unwrap()
        .expect("application");
    assert_eq!(application.status, ApplicationStatus::Uninstalled);
    assert_eq!(application.application_id, pair.application_id);

    // Idempotent replay, at-least-once edition: the re-run re-drives the
    // kill adapter — through an EMPTY pid map the replay now fails closed
    // on the missing mapping (the proof the durable receipt no longer
    // short-circuits before the adapter), while through the REAL registry
    // it re-signals both children (the second is dead and reaped: ESRCH
    // maps to AlreadyTerminated success; the first is an unreaped zombie:
    // the supplementary signal is a no-op), every receipt replays
    // byte-identically, and the gated uninstall replays without
    // consulting the activity gate.
    #[cfg(unix)]
    assert!(
        matches!(
            run_application_teardown(
                &runtime,
                &adapter,
                pair.package_id,
                seed,
                &empty_supervisor(),
            ),
            Err(SliceKError::Process(
                ProcessAuthorityError::PlatformKillAdapter(_)
            ))
        ),
        "the replayed kill must still consult the platform adapter"
    );
    let replay =
        run_application_teardown(&runtime, &adapter, pair.package_id, seed, &pair.registry)
            .expect("teardown replay");
    for (index, kill) in replay.kills.iter().enumerate() {
        assert!(
            matches!(kill, PlatformKillDecision::Replayed(_)),
            "replay kill #{index} must be Replayed, got {kill:?}"
        );
        assert_eq!(kill.receipt(), teardown.kills[index].receipt());
    }
    assert_eq!(replay.crashes, teardown.crashes);
    for (index, linkage) in replay.linkages.iter().enumerate() {
        assert!(matches!(
            linkage.decision,
            FiberCancelPropagationDecision::Replayed(_)
        ));
        assert_eq!(
            linkage.decision.receipts(),
            teardown.linkages[index].decision.receipts()
        );
        assert_eq!(
            linkage.already_terminal, linkage.matched_fibers,
            "the replay drove nothing new: every matched fiber was already terminal"
        );
        assert_eq!(linkage.canceled_scopes, 1);
        assert_eq!(linkage.vanished_scopes, 0);
    }
    for (index, cancel) in replay.task_cancels.iter().enumerate() {
        assert_eq!(
            cancel,
            &CancelDecision::Replayed { cancel_epoch: 1 },
            "replay cancel #{index}"
        );
    }
    assert_eq!(replay.uninstall, teardown.uninstall);

    // A distinct-key fresh uninstall against the terminal application is
    // the typed refusal, while the original key keeps replaying.
    let distinct = runtime
        .uninstall_application_gated_by_task_activity(pair.package_id, seed.wrapping_add(0x10))
        .expect_err("distinct-key uninstall must refuse");
    assert!(
        matches!(
            &distinct,
            SliceKError::Application(
                ApplicationAuthorityError::ApplicationAlreadyUninstalled { .. }
            )
        ),
        "unexpected refusal: {distinct:?}"
    );
    assert_eq!(
        runtime
            .uninstall_application_gated_by_task_activity(pair.package_id, seed)
            .unwrap(),
        teardown.uninstall
    );

    if let Some((first_child, _)) = &mut real_children {
        first_child.kill().expect("cleanup first child");
        first_child.wait().expect("reap first child");
    }

    // Crash-drop + reopen: the terminal facts, the task-row association,
    // and the replayability of the whole teardown all survive.
    drop(adapter);
    drop(runtime);
    let reopened = Arc::new(SliceKRuntime::open(dir.root()).expect("reopen slice-k runtime"));
    let fresh_adapter = slice_adapter();
    for process in [&pair.process_first, &pair.process_second] {
        assert!(matches!(
            reopened
                .process
                .inspect_active_process_binding(process.process_id),
            Err(ProcessAuthorityError::ProcessBindingTerminal(
                ProcessLifecycleState::Crashed
            ))
        ));
        let terminal = reopened
            .process
            .inspect_process_terminal(process.process_id)
            .unwrap()
            .expect("terminal marker survives the reopen");
        assert_eq!(terminal.lifecycle_state, ProcessLifecycleState::Crashed);
    }
    for (task_id, application_id) in [
        (pair.task_id_first, pair.application_id),
        (pair.task_id_second, pair.application_id),
    ] {
        let row = reopened.tasks.inspect_task(task_id).unwrap();
        assert_eq!(row.state, TaskState::Cancelled);
        assert_eq!(
            row.application_id,
            Some(application_id),
            "the association is a durable fact of the task row"
        );
    }
    assert_eq!(
        reopened
            .applications
            .inspect_application(pair.package_id)
            .unwrap()
            .expect("application")
            .status,
        ApplicationStatus::Uninstalled
    );
    let registrations = reopened
        .inspect_application_registrations(pair.package_id)
        .unwrap();
    assert_eq!(registrations.background_tasks.len(), 2);
    assert_eq!(registrations.process_bindings.len(), 2);

    // Reopen replay through the original registry mappings: both children
    // are dead and reaped by now, so every supplementary signal reports
    // AlreadyTerminated — success — and every receipt still replays.
    let reopened_teardown = run_application_teardown(
        &reopened,
        &fresh_adapter,
        pair.package_id,
        seed,
        &pair.registry,
    )
    .expect("teardown replay after reopen");
    for kill in &reopened_teardown.kills {
        assert!(matches!(kill, PlatformKillDecision::Replayed(_)));
    }
    for crash in &reopened_teardown.crashes {
        let replay_marker = reopened
            .process
            .propagate_crash(nlos_process::PropagateCrashRequest {
                process_id: crash.process_id,
                expected_process_generation: crash.process_generation,
                expected_process_fencing_token: crash.process_fencing_token,
                idempotency_key: crash.idempotency_key,
                marked_at_ms: crash.marked_at_ms,
            })
            .unwrap();
        assert!(matches!(
            replay_marker,
            ProcessTerminalDecision::Replayed(_)
        ));
    }
    assert_eq!(reopened_teardown.uninstall, teardown.uninstall);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn uninstall_teardown_drives_tasks_and_bindings_terminal_then_replays() {
    let dir = TempDir::new("teardown");
    let child_first = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn first real child");
    let child_second = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn second real child");
    uninstall_teardown_body(&dir, Some((child_first, child_second))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(not(unix))]
async fn uninstall_teardown_contract_via_noop_adapter_on_non_unix() {
    let dir = TempDir::new("teardown-contract");
    uninstall_teardown_body(&dir, None).await;
}

/// Kill receipt committed, crash marker not yet written: teardown must adopt
/// that receipt (NL kill's command id is the process id) instead of minting
/// a second key and failing `PlatformKillAlreadySignaled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_adopts_committed_platform_kill_before_terminal_marker() {
    let dir = TempDir::new("teardown-kill-race");
    let seed = 0xE1_u8;
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    let adapter = slice_adapter();
    let mut children = Vec::new();
    let (pid_first, pid_second) = if cfg!(unix) {
        let first = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn first sleeper");
        let second = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn second sleeper");
        let pids = (first.id(), second.id());
        children.push(first);
        children.push(second);
        pids
    } else {
        (std::process::id(), std::process::id())
    };
    let pair = run_second_process_pair(&runtime, &adapter, seed, pid_first, pid_second)
        .await
        .expect("second process pair");
    let process = pair.process_second;
    let nl_key = nlos_types::IdempotencyKey::from_bytes(*process.process_id.as_bytes());
    let killed_at_ms = 9_001_u64;
    let prior_adapter = nlos_process::StubPlatformKillAdapter::new();
    let prior = runtime
        .process
        .request_platform_kill(
            nlos_process::RequestPlatformKillRequest {
                process_id: process.process_id,
                expected_process_generation: process.process_generation,
                expected_process_fencing_token: process.process_fencing_token,
                idempotency_key: nl_key,
                killed_at_ms,
            },
            &prior_adapter,
        )
        .expect("NL-shaped kill commits before the terminal marker");
    assert!(matches!(prior, PlatformKillDecision::Signaled(_)));
    assert!(
        runtime
            .process
            .inspect_process_terminal(process.process_id)
            .expect("terminal inspect")
            .is_none(),
        "the race window is a kill receipt with no crash marker"
    );

    let teardown =
        run_application_teardown(&runtime, &adapter, pair.package_id, seed, &pair.registry)
            .expect("teardown adopts the committed kill");
    let adopted = teardown
        .kills
        .iter()
        .find(|kill| kill.receipt().process_id == process.process_id)
        .expect("second process kill");
    assert!(
        matches!(adopted, PlatformKillDecision::Replayed(_)),
        "committed kill must replay, got {adopted:?}"
    );
    assert_eq!(adopted.receipt().idempotency_key, nl_key);
    assert_eq!(adopted.receipt().killed_at_ms, killed_at_ms);

    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 关联下沉 (W30-D deliverable 3): the schema-v44 declaration association
/// — `application_id` from a real installation AND a declared plan
/// revision — lands in the durable task rows, survives reopen, and the
/// legacy association-free form keeps registering `None` (augmenting, not
/// breaking).
#[test]
fn task_spec_association_lands_in_durable_task_rows_and_survives_reopen() {
    let dir = TempDir::new("association");
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let publisher = runtime.bootstrap_publisher(0xF0).expect("publisher");
    let package = runtime
        .publish_signed_package(&publisher, 0xF0, &nlos_slice_k::fixture_bytes(0xF0, 32))
        .expect("package");
    let verification = runtime
        .verify_signed_package(&package, 0xF0)
        .expect("verify");
    let installation = runtime
        .install_verified_package(&verification, 0xF0)
        .expect("install");
    let application_id: ApplicationId = installation.application_id;
    let plan_revision = nlos_task::TaskPlanRevisionRef {
        plan_id: nlos_types::TaskPlanId::from_bytes([0xF5; 16]),
        revision: 3,
    };

    let (associated_task, _, _) = runtime
        .register_task_and_attempt_for(0xF0, Some(application_id), Some(plan_revision))
        .expect("register associated task");
    let (legacy_task, _, _) = runtime
        .register_task_and_attempt(0xF1)
        .expect("register legacy task");

    let associated = runtime.tasks.inspect_task(associated_task).unwrap();
    assert_eq!(associated.application_id, Some(application_id));
    assert_eq!(associated.plan_revision, Some(plan_revision));
    let legacy = runtime.tasks.inspect_task(legacy_task).unwrap();
    assert_eq!(legacy.application_id, None);
    assert_eq!(legacy.plan_revision, None);
    assert_ne!(legacy_task, associated_task);

    drop(runtime);
    let reopened = SliceKRuntime::open(dir.root()).expect("reopen");
    let durable = reopened.tasks.inspect_task(associated_task).unwrap();
    assert_eq!(durable.application_id, Some(application_id));
    assert_eq!(durable.plan_revision, Some(plan_revision));
}
