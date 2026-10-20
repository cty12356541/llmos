//! The crash-revival lane (#12 C-LIFECYCLE, the completion half the
//! `restore_process` prefix tests left open): given one Process binding
//! whose head generation is crashed-terminal — the `LOST` state of the
//! v0.5 process state machine (`任一 live state ─host loss→ LOST →
//! RECOVERING → RUNNABLE | EXITED`) — drive the full revival chain through
//! the landed public authorities:
//!
//! 1. `ProcessAuthority::restore_process` — the durable `RECOVERING` step:
//!    a fresh `Process`/`AgentInstance` generation pair under the same
//!    identity, the head reset to `Active`;
//! 2. `SupervisorPidRegistry::register` — the new generation's os pid
//!    mapping supersedes the crashed one (the registry's documented
//!    `restore_process` analogue: strictly newer generation takes over);
//! 3. re-dispatch — the `RUNNABLE` proof: a write fiber spawned under the
//!    restored fence runs one durable `register → dispatch → complete`
//!    operation job and registers its durable fiber incarnation, so the
//!    revived incarnation holds both a live runtime fiber and a durable
//!    binding again.
//!
//! The module invents no authority semantics (every transition is the
//! landed public API of `nlos-process` and `nlos-runtime`); this assembly
//! only fixes the order and the idempotency-key derivation. Per-revival
//! keys are domain-separated SHA-256 derivations of the process id AND the
//! crashed generation being revived from — a Process can crash and revive
//! repeatedly, so unlike the teardown lane's per-entity keys the revival
//! key must name the generation it revives; a re-run rebuilds
//! byte-identical requests from those durable inputs alone.
//!
//! Re-run contract: the durable prefix (`restore_process` and the
//! supervisor registration) replays byte-identically, while the runtime
//! re-dispatch is skipped — a re-run observes the head already past the
//! crashed generation (`replayed == true`), and re-spawning the identical
//! fiber identity would trip the adapter's duplicate fence without
//! producing anything new. Re-dispatching after a real host restart of
//! this assembly itself (fresh adapter, durable state intact) needs a
//! fiber-generation bump and stays outside this lane's contract.

use std::sync::Arc;

use nlos_process::{
    FiberIncarnationRecord, ProcessBindingRecord, RegisterFiberIncarnationRequest,
    RegisterSupervisorPidRequest, RestoreProcessDecision, RestoreProcessRequest,
    SupervisorPidDecision, SupervisorPidRegistry,
};
use nlos_runtime::{FiberHandle, FiberSpec};
use nlos_runtime_tokio::TokioRuntimeAdapter;
use nlos_task::empty_effect_history_root;
use nlos_types::{
    ArtifactId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, ProcessId, ReceiptId, ResourceGroupId, SchedulerDomainId,
};
use sha2::{Digest, Sha256};

use crate::SliceKResult;
use crate::error::SliceKError;
use crate::fiber::{FiberOutcome, WriteFiberJob, spawn_write_fiber};
use crate::runtime::{SliceKRuntime, seeded_key};

/// Domain separator of every revival-derived idempotency/clock key.
const REVIVAL_KEY_DOMAIN: &[u8] = b"nlos/slice-k/revival-key/v1";

/// Deterministic revival key of one (process, crashed generation) pair:
/// SHA-256 over the revival domain, the per-operation tag, the process id,
/// and the crashed generation. Reconstructible from the durable binding
/// rows alone, so a re-run rebuilds byte-identical requests.
fn revival_key(tag: &[u8], process_id: ProcessId, generation: Generation) -> IdempotencyKey {
    let digest = Sha256::new()
        .chain_update(REVIVAL_KEY_DOMAIN)
        .chain_update(tag)
        .chain_update(process_id.as_bytes())
        .chain_update(generation.get().to_be_bytes())
        .finalize();
    let mut key = [0_u8; 16];
    key.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(key)
}

/// Everything one revival run durably produced for the revived incarnation.
pub struct ProcessRevival {
    /// The restored binding: fresh `Process`/`AgentInstance` generations
    /// under the same identity, head `Active` again.
    pub restored: ProcessBindingRecord,
    /// `true` when this run found the head already past the crashed
    /// generation (an earlier run's durable revival): the durable prefix
    /// replayed and the runtime re-dispatch was skipped.
    pub replayed: bool,
    /// The supervisor registration decision of the restored generation's
    /// os pid mapping (`Superseded` on a fresh run over the crashed
    /// mapping, `Replayed` on a re-run with the same os pid).
    pub supervisor: SupervisorPidDecision,
    /// The `RUNNABLE` proof — `None` exactly when `replayed` is `true`.
    pub dispatch: Option<RevivalDispatch>,
}

/// The re-dispatch half of one revival: the revived incarnation's live
/// runtime fiber, its completed durable operation job, and the durable
/// incarnation row binding the fiber to the restored generation.
pub struct RevivalDispatch {
    pub fiber: FiberHandle,
    pub outcome: FiberOutcome,
    pub incarnation: FiberIncarnationRecord,
}

/// Runs one crashed binding's full revival: restore → supervisor
/// re-registration → re-dispatch under the restored fence.
///
/// # Errors
///
/// Propagates authority, runtime, and clock errors fail-closed, and
/// refuses as [`SliceKError::RevivalState`] the states this chain never
/// produces: a terminal marker at a generation other than the crashed
/// binding's, or a head that is neither the crashed generation nor its
/// direct revival (a never-crashed binding, or two generations past).
///
/// # Panics
///
/// Never panics by construction; every authority refusal is a typed error.
pub async fn run_process_revival(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    crashed: &ProcessBindingRecord,
    registry: &SupervisorPidRegistry,
    os_pid: u32,
    seed: u8,
) -> SliceKResult<ProcessRevival> {
    let (restored, fresh) = revival_target_for(runtime, crashed)?;
    let supervisor = registry.register(RegisterSupervisorPidRequest {
        process_id: restored.process_id,
        process_generation: restored.process_generation,
        os_pid,
        registered_at_ms: runtime.wall_now_ms(revival_key(
            b"supervisor-clock",
            restored.process_id,
            restored.process_generation,
        ))?,
    })?;
    let dispatch = if fresh {
        Some(revival_dispatch(runtime, adapter, &restored, seed).await?)
    } else {
        None
    };
    Ok(ProcessRevival {
        restored,
        replayed: !fresh,
        supervisor,
        dispatch,
    })
}

/// Resolves the revival target of one crashed binding: a fresh entry (the
/// head crashed-terminal at exactly the crashed generation) restores a new
/// generation; an already-revived head (terminal marker gone, head exactly
/// one generation past the crashed one) replays the durable revival; every
/// other state is a typed refusal.
fn revival_target_for(
    runtime: &SliceKRuntime,
    crashed: &ProcessBindingRecord,
) -> SliceKResult<(ProcessBindingRecord, bool)> {
    match runtime
        .process
        .inspect_process_terminal(crashed.process_id)?
    {
        Some(marker) if marker.process_generation == crashed.process_generation => {
            let decision = runtime.process.restore_process(RestoreProcessRequest {
                process_id: crashed.process_id,
                expected_process_generation: crashed.process_generation,
                expected_process_fencing_token: crashed.process_fencing_token,
                isolation_domain_id: crashed.isolation_domain_id,
                isolation_domain_generation: crashed.isolation_domain_generation,
                isolation_domain_fencing_token: crashed.isolation_domain_fencing_token,
                idempotency_key: revival_key(
                    b"restore",
                    crashed.process_id,
                    crashed.process_generation,
                ),
                restored_at_ms: runtime.wall_now_ms(revival_key(
                    b"restore-clock",
                    crashed.process_id,
                    crashed.process_generation,
                ))?,
            })?;
            match decision {
                RestoreProcessDecision::Restored(record) => Ok((record, true)),
                // Unreachable against this runtime's own single-writer
                // lane (a committed restore moves the head, so the entry
                // inspect above would not have seen the marker); kept as
                // the honest answer to a concurrent restorer.
                RestoreProcessDecision::Replayed(record) => Ok((record, false)),
            }
        }
        Some(_) => Err(SliceKError::RevivalState(
            "terminal marker names a generation other than the crashed binding; not this \
             revival chain's state",
        )),
        None => {
            let current = runtime
                .process
                .inspect_active_process_binding(crashed.process_id)?;
            if current.prior_process_generation == Some(crashed.process_generation) {
                Ok((current, false))
            } else {
                Err(SliceKError::RevivalState(
                    "process head is neither the crashed generation nor its direct revival",
                ))
            }
        }
    }
}

/// Re-dispatches the revived incarnation: the crash linkage cancelled the
/// prior incarnation's runtime scope, so the fiber runs under a fresh
/// `CancellationScopeId` (the runtime-side fresh-binding analogue of
/// `[PROC-RESTORE-002]`), executes one durable operation-only write job,
/// and registers its durable incarnation against the restored fence.
///
/// Key band of the lane seed (`seed.wrapping_add(offset)`): 150 scope,
/// 151 fiber id, 152 job wall clock, 153 operation id, 154 callback id,
/// 155 completion receipt id, 156 artifact id, 157 stage key, 158 plan
/// key, 159 incarnation key, 160 incarnation clock, 161 resource group,
/// 162 scheduler domain.
///
/// # Errors
///
/// Propagates authority, runtime, and clock errors fail-closed.
async fn revival_dispatch(
    runtime: &Arc<SliceKRuntime>,
    adapter: &TokioRuntimeAdapter,
    restored: &ProcessBindingRecord,
    seed: u8,
) -> SliceKResult<RevivalDispatch> {
    let job = WriteFiberJob {
        operation_id: OperationId::from_bytes([seed.wrapping_add(153); 16]),
        callback_id: CallbackId::from_bytes([seed.wrapping_add(154); 16]),
        completion_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(155); 16]),
        expected_head_revision: 0,
        artifact_id: ArtifactId::from_bytes([seed.wrapping_add(156); 16]),
        stage_key: seeded_key(seed, 157),
        stage_bytes: Vec::new().into(),
        stage_created_at_ms: 0,
        permit: None,
        write_set_root: empty_effect_history_root(),
        plan_key: seeded_key(seed, 158),
        planned_at_ms: runtime.wall_now_i64(seeded_key(seed, 152))?,
        task_id: restored.task_id,
        attempt_id: restored.task_attempt_id,
        attempt_generation: restored.attempt_generation,
    };
    let spec = FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes([seed.wrapping_add(151); 16]),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: restored.agent_instance_id,
        agent_generation: restored.agent_instance_generation,
        process_id: restored.process_id,
        process_generation: restored.process_generation,
        task_attempt_id: Some(restored.task_attempt_id),
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(150); 16]),
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes([seed.wrapping_add(161); 16]),
        scheduler_domain_id: SchedulerDomainId::from_bytes([seed.wrapping_add(162); 16]),
        deadline: None,
    };
    let (fiber, receiver) = spawn_write_fiber(Arc::clone(runtime), adapter, spec, job)?;
    let outcome = receiver.await.expect("fiber outcome channel")?;
    let incarnation = runtime
        .process
        .register_fiber_incarnation(RegisterFiberIncarnationRequest {
            process_id: restored.process_id,
            expected_process_generation: restored.process_generation,
            expected_process_fencing_token: restored.process_fencing_token,
            binding: fiber.fiber_id,
            idempotency_key: seeded_key(seed, 159),
            registered_at_ms: runtime.wall_now_ms(seeded_key(seed, 160))?,
        })?
        .record()
        .clone();
    Ok(RevivalDispatch {
        fiber,
        outcome,
        incarnation,
    })
}
