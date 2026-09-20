//! The uninstall-driven teardown lane (W30-D, ROAD-B-002 B2-3): given one
//! installed application, drive every durably registered background Task
//! and Process binding to its terminal state through the landed machinery —
//! per binding the W29-F kill chain (durable platform-kill receipt →
//! adapter signal → crash terminal marker → W27-C batch-cancel linkage),
//! per registered background Task the `cancel_task` fence — and only then
//! cross the W27-D production uninstall activity gate. Re-running the
//! teardown replays the identical durable receipts (the kill replay works
//! even against an empty supervisor registry: the durable receipt
//! short-circuits before any adapter invocation).
//!
//! The module invents no authority semantics: every transition is the
//! landed public API of `nlos-process`, `nlos-task`, and
//! `nlos-application`; this assembly only fixes the order and the
//! idempotency-key derivation. Per-entity keys are domain-separated
//! SHA-256 derivations of the entity id (the same derivation discipline
//! the process authority itself uses for its derived ids), because the
//! entity set is discovered at runtime — a seeded band key could not be
//! reconstructed per entity on replay.

use std::sync::Arc;

use nlos_process::{
    PlatformKillDecision, ProcessLifecycleState, ProcessTerminalRecord,
    PropagateCancelToFibersRequest, PropagateCrashRequest, RequestPlatformKillRequest,
};
use nlos_runtime_tokio::{ProcessFiberCancelReport, TokioRuntimeAdapter};
use nlos_task::{CancelDecision, CancelRequest};
use nlos_types::{ApplicationId, IdempotencyKey, PackageId, ProcessId};
use sha2::{Digest, Sha256};

use crate::error::SliceKResult;
use crate::runtime::SliceKRuntime;
use nlos_application::UninstallReceipt;

/// Domain separator of every teardown-derived idempotency/clock key.
const TEARDOWN_KEY_DOMAIN: &[u8] = b"nlos/slice-k/teardown-key/v1";

/// Everything one teardown run durably produced, in the registrations'
/// stable order: per Process binding the kill/crash/linkage chain, per
/// registered background Task the cancel decision, and the gated
/// uninstall receipt that closes the chain.
pub struct ApplicationTeardown {
    pub application_id: ApplicationId,
    pub package_id: PackageId,
    /// Per binding: the durable platform-kill decision (`Signaled` on a
    /// fresh run, `Replayed` on a re-run).
    pub kills: Vec<PlatformKillDecision>,
    /// Per binding: the crash terminal marker (`Crashed`).
    pub crashes: Vec<ProcessTerminalRecord>,
    /// Per binding: the W27-C runtime linkage report.
    pub linkages: Vec<ProcessFiberCancelReport>,
    /// Per registered background Task: `Applied` on a fresh run,
    /// `Replayed` on a re-run.
    pub task_cancels: Vec<CancelDecision>,
    pub uninstall: UninstallReceipt,
}

/// Deterministic teardown key of one entity: SHA-256 over the teardown
/// domain, the per-operation tag, and the entity id. Reconstructible from
/// the durable registration rows alone, so a re-run rebuilds byte-identical
/// requests (the clock replays the same reading under the same key).
fn teardown_key(tag: &[u8], entity: &[u8]) -> IdempotencyKey {
    let digest = Sha256::new()
        .chain_update(TEARDOWN_KEY_DOMAIN)
        .chain_update(tag)
        .chain_update(entity)
        .finalize();
    let mut key = [0_u8; 16];
    key.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(key)
}

/// One binding's teardown request shape: the authoritative
/// generation/fencing token (from the active binding on a fresh run, from
/// the durable terminal marker on a replay run) plus the byte-identical
/// kill/crash keys and timestamps the replay must reconstruct.
struct BindingTeardownStep {
    process_id: ProcessId,
    process_generation: nlos_types::Generation,
    process_fencing_token: nlos_process::FencingToken,
    kill_idempotency_key: IdempotencyKey,
    killed_at_ms: u64,
    crash_idempotency_key: IdempotencyKey,
    marked_at_ms: u64,
}

/// Runs one application's full teardown: every registered Process binding
/// to its crash terminal (kill → terminal → linkage), every registered
/// background Task to `Cancelled`, then the gated uninstall. A re-run over
/// an already-torn-down application replays byte-identical durable
/// receipts: the kill replay short-circuits on the durable receipt before
/// any adapter invocation (the supervisor registry may even be empty on a
/// re-run — the W29-F replay proof), the crash/linkage/cancel replays are
/// exact, and the gated uninstall replays without consulting the activity
/// gate.
///
/// # Errors
///
/// Propagates authority, runtime, and clock errors fail-closed. A binding
/// that is terminal without a durable kill receipt (an out-of-band crash
/// this chain never produces) is refused as
/// [`crate::SliceKError::TeardownState`] rather than guessed around.
///
/// # Panics
///
/// Never panics by construction; every authority refusal is a typed error.
#[cfg_attr(not(unix), allow(unused_variables))]
pub fn run_application_teardown(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    package_id: PackageId,
    seed: u8,
    supervisor: &nlos_process::SupervisorPidRegistry,
) -> SliceKResult<ApplicationTeardown> {
    let registrations = runtime.inspect_application_registrations(package_id)?;
    let application_id = resolve_application_id(runtime, package_id)?;

    #[cfg(unix)]
    let kill_adapter = nlos_process::PosixPlatformKillAdapter::new(supervisor.pid_map());
    #[cfg(not(unix))]
    let kill_adapter = nlos_process::NoopPlatformKillAdapter;

    let mut kills = Vec::new();
    let mut crashes = Vec::new();
    let mut linkages = Vec::new();
    for binding in &registrations.process_bindings {
        let step = teardown_step_for(runtime, binding.process_id)?;
        let kill = runtime
            .process
            .request_platform_kill(
                RequestPlatformKillRequest {
                    process_id: step.process_id,
                    expected_process_generation: step.process_generation,
                    expected_process_fencing_token: step.process_fencing_token,
                    idempotency_key: step.kill_idempotency_key,
                    killed_at_ms: step.killed_at_ms,
                },
                &kill_adapter,
            )?
            .clone();
        kills.push(kill);
        let crash = runtime
            .process
            .propagate_crash(PropagateCrashRequest {
                process_id: step.process_id,
                expected_process_generation: step.process_generation,
                expected_process_fencing_token: step.process_fencing_token,
                idempotency_key: step.crash_idempotency_key,
                marked_at_ms: step.marked_at_ms,
            })?
            .record()
            .clone();
        let linkage = adapter.cancel_process_fibers(
            &runtime.process,
            PropagateCancelToFibersRequest {
                process_id: step.process_id,
                expected_process_generation: step.process_generation,
                expected_process_fencing_token: step.process_fencing_token,
                lifecycle_state: ProcessLifecycleState::Crashed,
                idempotency_key: step.crash_idempotency_key,
                cancelled_at_ms: step.marked_at_ms,
            },
        )?;
        crashes.push(crash);
        linkages.push(linkage);
    }

    let mut task_cancels = Vec::new();
    for task in &registrations.background_tasks {
        let cancel = runtime.tasks.cancel_task(CancelRequest {
            task_id: task.task_id,
            idempotency_key: teardown_key(b"cancel", task.task_id.as_bytes()),
            requested_at_ms: runtime
                .wall_now_i64(teardown_key(b"cancel-clock", task.task_id.as_bytes()))?,
        })?;
        task_cancels.push(cancel);
    }

    let uninstall = runtime.uninstall_application_gated_by_task_activity(package_id, seed)?;

    Ok(ApplicationTeardown {
        application_id,
        package_id,
        kills,
        crashes,
        linkages,
        task_cancels,
        uninstall,
    })
}

/// Resolves the request shape of one binding's teardown chain. A fresh run
/// reads the active binding (generation + fencing token) and takes fresh
/// clock readings under the deterministic keys; a replay run (terminal
/// marker present) rebuilds the byte-identical requests from the durable
/// kill receipt and terminal marker instead.
fn teardown_step_for(
    runtime: &SliceKRuntime,
    process_id: ProcessId,
) -> SliceKResult<BindingTeardownStep> {
    let kill_key = teardown_key(b"kill", process_id.as_bytes());
    let crash_key = teardown_key(b"crash", process_id.as_bytes());
    if let Some(terminal) = runtime.process.inspect_process_terminal(process_id)? {
        let kill = runtime
            .process
            .inspect_platform_kill_receipt(process_id, terminal.process_generation)?
            .ok_or(crate::SliceKError::TeardownState(
                "binding is terminal without a durable kill receipt; out-of-band crash is not \
                 this teardown chain's state",
            ))?;
        return Ok(BindingTeardownStep {
            process_id,
            process_generation: terminal.process_generation,
            process_fencing_token: terminal.process_fencing_token,
            kill_idempotency_key: kill.idempotency_key,
            killed_at_ms: kill.killed_at_ms,
            crash_idempotency_key: terminal.idempotency_key,
            marked_at_ms: terminal.marked_at_ms,
        });
    }
    let binding = runtime.process.inspect_active_process_binding(process_id)?;
    Ok(BindingTeardownStep {
        process_id,
        process_generation: binding.process_generation,
        process_fencing_token: binding.process_fencing_token,
        kill_idempotency_key: kill_key,
        killed_at_ms: runtime.wall_now_ms(teardown_key(b"kill-clock", process_id.as_bytes()))?,
        crash_idempotency_key: crash_key,
        marked_at_ms: runtime.wall_now_ms(teardown_key(b"crash-clock", process_id.as_bytes()))?,
    })
}

/// The application id behind one package — read straight from the
/// application authority (`ApplicationNotFound` propagates fail-closed for
/// an unknown package, a missing row is a typed teardown refusal).
fn resolve_application_id(
    runtime: &SliceKRuntime,
    package_id: PackageId,
) -> SliceKResult<ApplicationId> {
    Ok(runtime
        .applications
        .inspect_application(package_id)?
        .ok_or(crate::SliceKError::TeardownState(
            "no application row names this package",
        ))?
        .application_id)
}
