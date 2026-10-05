//! W46-B lane: the complete crash-revival chain of #12 (`C-LIFECYCLE`) —
//! fault → recovery → re-dispatch over the second-process spawn chain. The
//! kill lane drives the second process to its crashed terminal (`LOST`),
//! the revival lane restores a fresh Process/`AgentInstance` generation
//! under the same identity (`RECOVERING`), re-registers the supervisor os
//! pid mapping at the new generation, and re-dispatches a write fiber that
//! runs one durable operation job and registers its incarnation under the
//! restored fence (`RUNNABLE`). The dead incarnation's fence stays refused
//! before AND after the revival, the durable prefix replays
//! byte-identically on a re-run (no second fiber), and the revived head
//! survives a drop + reopen.

use std::sync::Arc;

use nlos_process::{
    FiberIncarnationDecision, PlatformKillDecision, ProcessAuthorityError, ProcessBindingRecord,
    ProcessLifecycleState, ProcessTerminalRecord, RegisterFiberIncarnationRequest,
    RequestPlatformKillRequest, StubPlatformKillAdapter, SupervisorPidDecision,
};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{
    ProcessRevival, SliceKRuntime, run_process_revival, run_second_process_pair,
    run_second_process_platform_kill,
};
use nlos_types::{ExecutionFiberId, IdempotencyKey};

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

/// Spawns one `sleep` child per requested pid on Unix; non-Unix hosts
/// reuse the test process's own pid (the registry mapping is in-memory and
/// the revival lane never signals it).
fn sleeper_pids(count: usize, children: &mut Vec<std::process::Child>) -> Vec<u32> {
    if cfg!(unix) {
        (0..count)
            .map(|_| {
                let child = std::process::Command::new("sleep")
                    .arg("600")
                    .spawn()
                    .expect("spawn sleeper");
                let pid = child.id();
                children.push(child);
                pid
            })
            .collect()
    } else {
        vec![std::process::id(); count]
    }
}

/// `LOST` assertions: the binding is crashed-terminal at its head
/// generation and refuses both the kill and the incarnation write
/// fail-closed.
fn assert_lost_refuses_every_write(
    runtime: &SliceKRuntime,
    crashed: &ProcessBindingRecord,
    crash: &ProcessTerminalRecord,
) {
    assert_eq!(crash.process_generation, crashed.process_generation);
    assert_eq!(
        crash.lifecycle_state,
        ProcessLifecycleState::Crashed,
        "the kill lane must leave the binding crashed-terminal"
    );
    let stub = StubPlatformKillAdapter::new();
    assert!(matches!(
        runtime.process.request_platform_kill(
            RequestPlatformKillRequest {
                process_id: crashed.process_id,
                expected_process_generation: crashed.process_generation,
                expected_process_fencing_token: crashed.process_fencing_token,
                idempotency_key: IdempotencyKey::from_bytes([0xa1; 16]),
                killed_at_ms: 1_000,
            },
            &stub,
        ),
        Err(ProcessAuthorityError::ProcessBindingTerminal(_))
    ));
    assert!(matches!(
        runtime
            .process
            .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                process_id: crashed.process_id,
                expected_process_generation: crashed.process_generation,
                expected_process_fencing_token: crashed.process_fencing_token,
                binding: ExecutionFiberId::from_bytes([0xa2; 16]),
                idempotency_key: IdempotencyKey::from_bytes([0xa3; 16]),
                registered_at_ms: 1_100,
            }),
        Err(ProcessAuthorityError::ProcessBindingTerminal(_))
    ));
}

/// `RECOVERING`-outcome assertions: the restored binding is the same
/// identity one Process/`AgentInstance` generation further, fencing the
/// crashed reference, and the head is `Active` with no terminal marker.
fn assert_restored_identity(
    runtime: &SliceKRuntime,
    crashed: &ProcessBindingRecord,
    restored: &ProcessBindingRecord,
) {
    assert_eq!(restored.process_id, crashed.process_id);
    assert_eq!(
        restored.process_generation.get(),
        crashed.process_generation.get() + 1
    );
    assert_eq!(
        restored.agent_instance_generation.get(),
        crashed.agent_instance_generation.get() + 1
    );
    assert_eq!(restored.agent_instance_id, crashed.agent_instance_id);
    assert_eq!(restored.task_id, crashed.task_id);
    assert_eq!(restored.task_attempt_id, crashed.task_attempt_id);
    assert_eq!(
        restored.prior_process_generation,
        Some(crashed.process_generation)
    );
    assert_ne!(
        restored.process_fencing_token,
        crashed.process_fencing_token
    );
    assert_eq!(
        runtime
            .process
            .inspect_active_process_binding(restored.process_id)
            .expect("restored head must be the active binding"),
        *restored
    );
    assert!(
        runtime
            .process
            .inspect_process_terminal(restored.process_id)
            .expect("terminal inspect")
            .is_none(),
        "the restored generation has no terminal marker"
    );
}

/// Generation alignment: the dead incarnation's fence stays refused after
/// the revival — the head moved, so the crashed reference is stale.
fn assert_crashed_fence_stays_stale(runtime: &SliceKRuntime, crashed: &ProcessBindingRecord) {
    let stub = StubPlatformKillAdapter::new();
    assert!(matches!(
        runtime.process.request_platform_kill(
            RequestPlatformKillRequest {
                process_id: crashed.process_id,
                expected_process_generation: crashed.process_generation,
                expected_process_fencing_token: crashed.process_fencing_token,
                idempotency_key: IdempotencyKey::from_bytes([0xb1; 16]),
                killed_at_ms: 1_200,
            },
            &stub,
        ),
        Err(ProcessAuthorityError::StaleProcessBinding)
    ));
    assert!(matches!(
        runtime
            .process
            .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                process_id: crashed.process_id,
                expected_process_generation: crashed.process_generation,
                expected_process_fencing_token: crashed.process_fencing_token,
                binding: ExecutionFiberId::from_bytes([0xb2; 16]),
                idempotency_key: IdempotencyKey::from_bytes([0xb3; 16]),
                registered_at_ms: 1_300,
            }),
        Err(ProcessAuthorityError::StaleProcessBinding)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashed_process_revives_restores_and_redispatches() {
    let dir = TempDir::new("process-revival");
    // The pair seed's key bands (seed+{0x14..0x1A, 0x1E..0x26, 0x34..0x3A,
    // 0x6E..0x72, 0x78..0x8F, 0xAE..0xB2}) and the revival lane's band
    // (revival_seed+{0x96..0xA2}) are disjoint: the 0x71 bands live in
    // {0x85..0x97, 0xA5..0xAB, 0xDF..0x00, 0x1F..0x23}, the 0x2B band in
    // {0xC1..0xCD}.
    let seed = 0x71_u8;
    let revival_seed = 0x2B_u8;
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    let adapter = slice_adapter();
    let mut children = Vec::new();
    let pids = sleeper_pids(3, &mut children);
    let [pid_first, pid_second, pid_revived]: [u32; 3] = [pids[0], pids[1], pids[2]];

    // Fault setup: one application, two supervised live processes.
    let pair = run_second_process_pair(&runtime, &adapter, seed, pid_first, pid_second)
        .await
        .expect("second process pair");

    // FAULT: kill → crash terminal → fiber linkage (`LOST`).
    let kill = run_second_process_platform_kill(&runtime, &adapter, &pair)
        .await
        .expect("kill lane");
    assert!(matches!(
        kill.kill,
        PlatformKillDecision::Signaled(_) | PlatformKillDecision::Replayed(_)
    ));
    let crashed = pair.process_second.clone();
    assert_lost_refuses_every_write(&runtime, &crashed, &kill.crash);

    // RECOVERY: restore under the same identity, supervisor mapping to the
    // restored generation's fresh pid.
    let revival = run_process_revival(
        &runtime,
        &adapter,
        &crashed,
        &pair.registry,
        pid_revived,
        revival_seed,
    )
    .await
    .expect("revival");
    assert!(!revival.replayed, "the fresh run must dispatch");
    assert_restored_identity(&runtime, &crashed, &revival.restored);
    let SupervisorPidDecision::Superseded { previous, current } = &revival.supervisor else {
        panic!("a fresh revival must supersede the crashed os pid mapping");
    };
    assert_eq!(previous.process_generation, crashed.process_generation);
    assert_eq!(
        current.process_generation,
        revival.restored.process_generation
    );
    assert_eq!(current.os_pid, pid_revived);

    // RE-DISPATCH: the revived incarnation ran its durable operation job
    // and holds a durable incarnation row against the restored fence.
    let dispatch = revival.dispatch.as_ref().expect("fresh dispatch");
    assert!(
        dispatch.outcome.plan_id.is_none(),
        "the operation-only revival job plans nothing"
    );
    assert_eq!(dispatch.incarnation.process_id, revival.restored.process_id);
    assert_eq!(
        dispatch.incarnation.process_generation,
        revival.restored.process_generation
    );
    assert_eq!(
        dispatch.incarnation.process_fencing_token,
        revival.restored.process_fencing_token
    );
    assert_crashed_fence_stays_stale(&runtime, &crashed);

    // REPLAY: the durable prefix replays byte-identically and the runtime
    // re-dispatch is skipped (no duplicate fiber).
    let rerun = run_process_revival(
        &runtime,
        &adapter,
        &crashed,
        &pair.registry,
        pid_revived,
        revival_seed,
    )
    .await
    .expect("revival replay");
    assert!(rerun.replayed, "the re-run must recognize the revival");
    assert!(rerun.dispatch.is_none(), "the re-run re-dispatches nothing");
    assert_eq!(rerun.restored, revival.restored);
    assert!(matches!(
        rerun.supervisor,
        SupervisorPidDecision::Replayed(_)
    ));
    assert_incarnation_replays(&runtime, &revival);

    // The revived head is durable: drop + reopen keeps the restored
    // binding as the active generation.
    drop(runtime);
    let reopened = SliceKRuntime::open(dir.root()).expect("reopen slice-k runtime");
    assert_eq!(
        reopened
            .process
            .inspect_active_process_binding(revival.restored.process_id)
            .expect("active binding after reopen"),
        revival.restored
    );

    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// The dispatch's durable incarnation row replays exactly under the
/// restored fence with its original key and timestamp.
fn assert_incarnation_replays(runtime: &SliceKRuntime, revival: &ProcessRevival) {
    let dispatch = revival.dispatch.as_ref().expect("fresh dispatch");
    assert!(matches!(
        runtime
            .process
            .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                process_id: revival.restored.process_id,
                expected_process_generation: revival.restored.process_generation,
                expected_process_fencing_token: revival.restored.process_fencing_token,
                binding: dispatch.incarnation.binding,
                idempotency_key: IdempotencyKey::from_bytes(
                    *dispatch.incarnation.idempotency_key.as_bytes(),
                ),
                registered_at_ms: dispatch.incarnation.created_at_ms,
            }),
        Ok(FiberIncarnationDecision::Replayed(_))
    ));
}
