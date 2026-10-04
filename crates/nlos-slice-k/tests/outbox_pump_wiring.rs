//! Wave D-b → W48-2: the production Outbox pump wiring of
//! [`SliceKRuntime`].
//!
//! The durable closed loop under test: terminal Operation commits write
//! `WakeFiber`/`ReconcileEffect` rows into `operation_outbox` in the same
//! transaction, and the runtime-owned pump (started against a caller's
//! runtime adapter) drains, applies, and acknowledges them — the reconcile
//! lane routes bound late outcomes into the task authority (the bound
//! effect slot converges and the entry is acknowledged), while unbindable
//! entries stay durable, visible, and retried instead of silently dropped.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nlos_operation::{CompletionOutcome, OperationSpec};
use nlos_runtime::{FiberExit, FiberFuture, FiberHandle, FiberSpec, RuntimeAdapter as _};
use nlos_runtime_tokio::{PumpState, TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome};
use nlos_slice_k::{SliceKError, SliceKRuntime};
use nlos_store::OutboxKind;
use nlos_task::{
    AttemptSpec, Authorities, EffectPermitRequest, LogicalEffectDescriptor, PermitDecision,
    PermitRequest, PlannedEffect, SlotState, SnapshotBundle, SnapshotConsistency,
    TaskSnapshotReceiptSpec, TaskSpec, TaskWriteSetArtifactRead, TaskWriteSetEffectEndpointRequest,
    TaskWriteSetRequest, empty_effect_history_root,
};
use nlos_types::{
    AgentInstanceId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, ProcessId, ReceiptId, ResourceGroupId, SchedulerDomainId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

/// Generous bound for "the 25ms fallback poll must have delivered by now".
const DRAIN_BOUND: Duration = Duration::from_secs(5);
/// Poll step for bounded waits.
const POLL_STEP: Duration = Duration::from_millis(10);
/// Several pump poll intervals — enough for a retried refusal to be
/// re-offered (and re-counted) at least twice.
const RETRY_WINDOW: Duration = Duration::from_millis(300);
/// Bounded wait for pump backoff re-offers on slow CI runners.
const RETRY_BOUND: Duration = Duration::from_secs(8);

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-pump-{name}-{}-{sequence}",
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

fn seeded(bytes: u8) -> [u8; 16] {
    [bytes; 16]
}

fn adapter() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("tokio adapter")
}

/// A raw fiber spec with seeded identities (the runtime-only lane of the
/// `outbox_wake_latency` fixture discipline: no process-authority binding).
fn raw_fiber_spec(seed: u8) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(seeded(seed)),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: AgentInstanceId::from_bytes(seeded(seed.wrapping_add(1))),
        agent_generation: Generation::INITIAL,
        process_id: ProcessId::from_bytes(seeded(seed.wrapping_add(2))),
        process_generation: Generation::INITIAL,
        task_attempt_id: None,
        cancellation_scope_id: CancellationScopeId::from_bytes(seeded(seed.wrapping_add(3))),
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(seeded(seed.wrapping_add(4))),
        scheduler_domain_id: SchedulerDomainId::from_bytes(seeded(seed.wrapping_add(5))),
        deadline: None,
    }
}

/// Registers, dispatches, and completes one operation owned by a fiber no
/// adapter knows: the wake lane must treat it as the permanent terminal
/// `FiberGone` condition and still acknowledge the entry.
fn complete_orphaned_operation(runtime: &SliceKRuntime, seed: u8) {
    let spec = raw_fiber_spec(seed);
    let owner = FiberHandle {
        fiber_id: spec.fiber_id,
        generation: spec.fiber_generation,
    };
    let handle = runtime
        .operations
        .register(OperationSpec {
            operation_id: OperationId::from_bytes(seeded(seed.wrapping_add(6))),
            generation: Generation::INITIAL,
            owner_fiber: owner,
            cancellation_scope_id: spec.cancellation_scope_id,
            cancellation_generation: spec.cancellation_generation,
        })
        .expect("register operation")
        .handle();
    let ticket = runtime
        .operations
        .dispatch(handle, CallbackId::from_bytes(seeded(seed.wrapping_add(7))))
        .expect("dispatch operation");
    runtime
        .operations
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(seeded(seed.wrapping_add(8))),
            },
        )
        .expect("complete operation");
}

async fn wait_until(description: &str, probe: impl Fn() -> bool) {
    let mut remaining = DRAIN_BOUND;
    loop {
        if probe() {
            return;
        }
        assert!(!remaining.is_zero(), "timed out waiting for {description}");
        tokio::time::sleep(POLL_STEP).await;
        remaining = remaining.saturating_sub(POLL_STEP);
    }
}

/// Given/When/Then: given a runtime whose operations committed terminal
/// states with no consumer running; when the pump starts against an
/// adapter that never saw the owner fibers; then the fallback poll drains
/// the backlog — `FiberGone` is a permanent terminal wake condition, so the
/// entries are applied and acknowledged, `pending_outbox` empties, the pump
/// stays healthy, stopping twice is idempotent, and health reads `None`
/// after the stop.
#[tokio::test]
async fn pump_consumes_and_acks_backlog_and_stop_is_idempotent() {
    let dir = TempDir::new("backlog");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    complete_orphaned_operation(&runtime, 0x10);
    complete_orphaned_operation(&runtime, 0x20);
    assert_eq!(
        runtime
            .operations
            .pending_outbox(16)
            .expect("pending")
            .len(),
        2,
        "both terminal commits wrote outbox rows"
    );

    let adapter = adapter();
    runtime.start_pump(&adapter).expect("start pump");
    wait_until("backlog drained", || {
        runtime
            .operations
            .pending_outbox(16)
            .expect("pending")
            .is_empty()
    })
    .await;
    assert_eq!(
        runtime.pump_health().expect("pump health").state,
        PumpState::Running
    );

    runtime.stop_pump();
    runtime.stop_pump();
    assert!(runtime.pump_health().is_none());
}

/// Given/When/Then: given a pump bound to the adapter and a fiber that
/// owns, dispatches, and then waits for its own operation's terminal wake;
/// when the test completes the operation through the durable authority;
/// then the pump delivers the wake through the store-adapter lane, the
/// fiber's wait resolves `Woken` (the durable closed loop, not the
/// process-local oneshot), and the entry is acknowledged away.
#[tokio::test]
async fn pump_delivers_durable_wake_to_waiting_fiber() {
    let dir = TempDir::new("closed-loop");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let adapter = adapter();
    runtime.start_pump(&adapter).expect("start pump");

    let spec = raw_fiber_spec(0x30);
    let owner = FiberHandle {
        fiber_id: spec.fiber_id,
        generation: spec.fiber_generation,
    };
    let operation_id = OperationId::from_bytes(seeded(0x36));
    let (ticket_tx, mut ticket_rx) = tokio::sync::mpsc::unbounded_channel();
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
    let operations = std::sync::Arc::clone(&runtime.operations);
    let waiting_lane = adapter.clone();
    let future: FiberFuture = Box::pin(async move {
        let handle = operations
            .register(OperationSpec {
                operation_id,
                generation: Generation::INITIAL,
                owner_fiber: owner,
                cancellation_scope_id: spec.cancellation_scope_id,
                cancellation_generation: spec.cancellation_generation,
            })
            .expect("register")
            .handle();
        let ticket = operations
            .dispatch(handle, CallbackId::from_bytes(seeded(0x37)))
            .expect("dispatch");
        ticket_tx.send(ticket).expect("hand ticket to the test");
        let wait = waiting_lane
            .wait_for_operation(owner, operation_id, Generation::INITIAL)
            .expect("register wait");
        let outcome = wait.await;
        let _ = outcome_tx.send(outcome);
        FiberExit::Completed
    });
    let fiber = adapter
        .spawn_fiber(spec, future)
        .expect("spawn waiting fiber");

    let ticket = ticket_rx.recv().await.expect("ticket handoff");
    runtime
        .operations
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(seeded(0x38)),
            },
        )
        .expect("complete");

    assert_eq!(outcome_rx.await.expect("outcome"), WaitOutcome::Woken);
    assert_eq!(
        adapter.join_fiber(fiber).expect("join"),
        FiberExit::Completed
    );
    wait_until("wake entry acknowledged", || {
        runtime
            .operations
            .pending_outbox(16)
            .expect("pending")
            .is_empty()
    })
    .await;
}

/// Given/When/Then: given a stopped pump; when new terminal commits land;
/// then the entries stay durable (nothing consumes them) until a fresh
/// pump starts and drains them — stop/restart never loses a committed
/// outbox row.
#[tokio::test]
async fn stopped_pump_keeps_entries_durable_until_restart() {
    let dir = TempDir::new("restart");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let adapter = adapter();
    runtime.start_pump(&adapter).expect("start pump");
    runtime.stop_pump();
    runtime.stop_pump();

    complete_orphaned_operation(&runtime, 0x40);
    tokio::time::sleep(RETRY_WINDOW).await;
    assert_eq!(
        runtime
            .operations
            .pending_outbox(16)
            .expect("pending")
            .len(),
        1,
        "a stopped pump must not consume committed entries"
    );

    runtime.start_pump(&adapter).expect("restart pump");
    wait_until("entry drained after restart", || {
        runtime
            .operations
            .pending_outbox(16)
            .expect("pending")
            .is_empty()
    })
    .await;
}

/// Given/When/Then: given a completion that lands after a cancel request
/// (the ticket's cancel epoch is stale, so the store canonicalizes it
/// `CanonicalizedForReconciliation` and commits a `ReconcileEffect` row)
/// for an operation no task write set ever bound; when the pump offers it
/// to the slice's reconcile lane; then the sink keeps the fail-closed
/// refusal semantics for the typed no-route answer — the entry is retried
/// (the no-route counter grows) but never acknowledged, a later
/// `WakeFiber` entry queued behind it stays durable too (in-order
/// batches), and the pump reports this as backpressure, staying `Running`
/// with zero drain failures.
#[tokio::test]
async fn unbound_reconcile_effect_refuses_as_no_route_and_stays_durable() {
    let dir = TempDir::new("reconcile-no-route");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let adapter = adapter();
    runtime.start_pump(&adapter).expect("start pump");

    let spec = raw_fiber_spec(0x50);
    let owner = FiberHandle {
        fiber_id: spec.fiber_id,
        generation: spec.fiber_generation,
    };
    let handle = runtime
        .operations
        .register(OperationSpec {
            operation_id: OperationId::from_bytes(seeded(0x56)),
            generation: Generation::INITIAL,
            owner_fiber: owner,
            cancellation_scope_id: spec.cancellation_scope_id,
            cancellation_generation: spec.cancellation_generation,
        })
        .expect("register operation")
        .handle();
    let ticket = runtime
        .operations
        .dispatch(handle, CallbackId::from_bytes(seeded(0x57)))
        .expect("dispatch");
    runtime
        .operations
        .request_cancel(handle, ReceiptId::from_bytes(seeded(0x58)))
        .expect("cancel request advances to CancelRequested");
    runtime
        .operations
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(seeded(0x59)),
            },
        )
        .expect("late completion canonicalizes for reconciliation");

    let pending = runtime.operations.pending_outbox(16).expect("pending");
    assert_eq!(pending.len(), 1, "exactly the reconcile entry so far");
    assert_eq!(pending[0].kind, OutboxKind::ReconcileEffect);

    // Head-of-line: a wake entry queued behind the refused reconcile entry
    // must stay durable too — the consumer drains in sequence order.
    complete_orphaned_operation(&runtime, 0x60);

    // The pump re-offers the refused entry on its failure backoff schedule;
    // poll for the second refusal instead of racing a fixed window, because
    // runner speed and synchronous=FULL fsync latency vary widely in CI.
    let deadline = tokio::time::Instant::now() + RETRY_BOUND;
    let mut lane = runtime.reconcile_lane();
    while lane.no_route < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(POLL_STEP).await;
        lane = runtime.reconcile_lane();
    }
    let pending = runtime.operations.pending_outbox(16).expect("pending");
    assert_eq!(pending.len(), 2, "neither entry may be acknowledged away");
    assert_eq!(pending[0].kind, OutboxKind::ReconcileEffect);

    assert!(
        lane.no_route >= 2,
        "the pump re-offered the unbindable entry ({} no-route refusals recorded)",
        lane.no_route
    );
    assert_eq!(lane.routed, 0, "nothing may route for an unbound operation");
    assert_eq!(
        lane.failed, 0,
        "an unbound entry is a no-route, not a failure"
    );
    assert!(
        lane.last_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("no effect slot binds")),
        "the refusal reason must name the typed no-route decision"
    );

    let health = runtime.pump_health().expect("pump health");
    assert_eq!(health.state, PumpState::Running);
    assert_eq!(health.consecutive_failures, 0);
    runtime.stop_pump();
}

/// Task-plane Given-fixture inside one runtime: task + attempt + snapshot
/// receipt + registered Operation participant + sealed write set whose
/// single (required) effect slot 0 carries an `OperationBinding` endpoint +
/// commit permit + owner-activated dispatch + consumed one-shot token, so
/// the effect slot is `Dispatched` exactly as a fenced fiber would have
/// left it. Returns the identities the routing assertions need.
struct BoundEffectSlot {
    operation: nlos_operation::OperationHandle,
    ticket: nlos_operation::CallbackTicket,
    task_id: TaskId,
    permit: nlos_task::PermitRecord,
}

/// The whole task-plane Given-fixture assembles one durable chain, so the
/// constructor stays contiguous for audit at the cost of the line budget.
#[allow(clippy::too_many_lines)]
fn bound_effect_slot(runtime: &SliceKRuntime) -> BoundEffectSlot {
    let fiber = raw_fiber_spec(0x80);
    let owner = FiberHandle {
        fiber_id: fiber.fiber_id,
        generation: fiber.fiber_generation,
    };
    let operation = nlos_operation::OperationSpec {
        operation_id: OperationId::from_bytes(seeded(0x86)),
        generation: Generation::INITIAL,
        owner_fiber: owner,
        cancellation_scope_id: fiber.cancellation_scope_id,
        cancellation_generation: fiber.cancellation_generation,
    };
    let handle = runtime
        .operations
        .register(operation)
        .expect("register bound operation")
        .handle();

    let task_id = TaskId::from_bytes(seeded(0x87));
    let spec = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes(seeded(0x88)),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(seeded(0x89)),
            snapshot_digest: [0x8a; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: fiber.cancellation_scope_id,
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(seeded(0x8b)),
        registered_at_ms: 2_000,
    };
    let tasks = &*runtime.tasks;
    tasks
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    let snapshot_receipt = ReceiptId::from_bytes(seeded(0x8c));
    tasks
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id,
            snapshot: spec.snapshot,
            receipt_id: snapshot_receipt,
            builder_id: seeded(0x8d),
            builder_version_digest: [0x8e; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes(seeded(0x8f))],
            dependency_closure_root: [0x90; 32],
            semantic_resolver_digest: [0x91; 32],
            canonical_iteration_digest: [0x92; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_100,
            authority_id: seeded(0x93),
            key_id: seeded(0x94),
            signature: [0x95; 64],
        })
        .expect("snapshot receipt");
    tasks
        .register_attempt_with_snapshot_receipt(spec, snapshot_receipt)
        .expect("attempt");
    let registry = tasks
        .inspect_participant_registry(task_id)
        .expect("registry");
    tasks
        .register_operation_binding_participant(
            &runtime.operations,
            task_id,
            nlos_task::ParticipantRegistryBinding {
                generation: registry.generation,
                root: registry.root,
            },
            handle.operation_id,
            handle.generation,
            1_150,
        )
        .expect("operation participant");

    let artifact_id = nlos_types::ArtifactId::from_bytes(seeded(0x96));
    runtime
        .artifacts
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes(seeded(0x97)),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 1_500,
        })
        .expect("artifact");
    let authorities = Authorities {
        artifact: Some(&runtime.artifacts),
        process: None,
        semantic: None,
        resource: None,
        operation: Some(&runtime.operations),
        channel: None,
    };
    let sealed = tasks
        .seal_task_write_set_with_authorities_struct(
            authorities,
            TaskWriteSetRequest {
                task_id,
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                artifact_reads: vec![TaskWriteSetArtifactRead {
                    artifact_id,
                    expected_head_revision: 0,
                    expected_head_digest: None,
                }],
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: Vec::new(),
                resource_reservations: Vec::new(),
                planned_effects: vec![PlannedEffect {
                    descriptor: LogicalEffectDescriptor {
                        task_id,
                        task_generation: Generation::INITIAL,
                        intent_spec_id: [0x98; 32],
                        stable_action_slot: 0,
                        target_authority_object_id: [0x99; 32],
                        effect_class: 1,
                        idempotency_scope: 1,
                    },
                    required: true,
                    required_condition_digest: None,
                    success_criteria_digest: [0x9a; 32],
                    action_proposal_digest: [0x9b; 32],
                }],
                effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::OperationBinding {
                    effect_seq: 0,
                    operation_id: handle.operation_id,
                    expected_operation_generation: handle.generation,
                }],
                idempotency_key: IdempotencyKey::from_bytes(seeded(0x9c)),
                sealed_at_ms: 1_200,
            },
        )
        .expect("seal write set")
        .record()
        .clone();
    let permit = match tasks.request_commit_permit_with_authorities_struct(
        authorities,
        PermitRequest {
            task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            write_set_root: sealed.write_set_root,
            planned_effects: sealed.planned_effects.clone(),
            idempotency_key: IdempotencyKey::from_bytes(seeded(0x9d)),
            valid_until_ms: 9_000,
            requested_at_ms: 3_000,
        },
    ) {
        Ok(PermitDecision::Issued(record)) => *record,
        other => panic!("expected issued commit permit, got {other:?}"),
    };

    // Owner-side prepare+activate (ADR-0005 authority-first order), then the
    // effect-permit mint and the one-shot token consumption: the slot is
    // Dispatched and the operation store holds the live callback.
    let preparation = match runtime
        .operations
        .prepare_dispatch(handle, CallbackId::from_bytes(seeded(0x9e)))
        .expect("prepare dispatch")
    {
        nlos_store::OperationPrepareDecision::Prepared(preparation)
        | nlos_store::OperationPrepareDecision::Replayed(preparation) => preparation,
    };
    let ticket = match runtime
        .operations
        .activate_dispatch(preparation)
        .expect("activate dispatch")
    {
        nlos_store::OperationActivationDecision::Activated(activation)
        | nlos_store::OperationActivationDecision::Replayed(activation) => activation.ticket,
    };
    let issued = match tasks.request_effect_permit_with_operation_authority(
        &runtime.operations,
        EffectPermitRequest {
            task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            idempotency_key: IdempotencyKey::from_bytes(seeded(0x9f)),
            valid_until_ms: 9_000,
            requested_at_ms: 4_000,
        },
    ) {
        Ok(nlos_task::EffectPermitDecision::Issued(record)) => *record,
        other => panic!("expected issued effect permit, got {other:?}"),
    };
    tasks
        .consume_dispatch_token(nlos_task::DispatchRequest {
            task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_permit_id: issued.effect_permit_id,
            dispatch_token: issued.one_shot_dispatch_token,
            dispatched_at_ms: 5_000,
        })
        .expect("consume dispatch token");
    BoundEffectSlot {
        operation: handle,
        ticket,
        task_id,
        permit,
    }
}

/// Given/When/Then: given a `Dispatched` effect slot bound to an operation
/// that is cancel-requested and then completes late (the store
/// canonicalizes the completion for reconciliation and commits a
/// `ReconcileEffect` row); when the pump offers the entry to the slice's
/// reconcile lane; then the sink performs real routing — the task
/// authority's late-outcome lane converges the slot to `EffectClosed` with
/// a cross-attempt history entry citing the operation, and the entry is
/// acknowledged away (routed counter, zero refusals).
#[tokio::test]
async fn bound_reconcile_effect_routes_to_task_authority_and_acks() {
    let dir = TempDir::new("reconcile-routed");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let bound = bound_effect_slot(&runtime);

    // Cancel wins the wake fence, then the authoritative completion lands:
    // one `ReconcileEffect` row commits in the same transaction.
    runtime
        .operations
        .request_cancel(bound.operation, ReceiptId::from_bytes(seeded(0xa1)))
        .expect("cancel request");
    runtime
        .operations
        .complete(
            bound.ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(seeded(0xa2)),
            },
        )
        .expect("late completion canonicalizes for reconciliation");
    let pending = runtime.operations.pending_outbox(16).expect("pending");
    assert_eq!(pending.len(), 1, "exactly the reconcile entry");
    assert_eq!(pending[0].kind, OutboxKind::ReconcileEffect);

    let slot_before = runtime
        .tasks
        .inspect_effect_slot(bound.permit.permit_id, 0)
        .expect("slot before routing");
    assert_eq!(slot_before.state, SlotState::Dispatched);

    let adapter = adapter();
    runtime.start_pump(&adapter).expect("start pump");
    wait_until("reconcile entry routed and acknowledged", || {
        runtime
            .operations
            .pending_outbox(16)
            .expect("pending")
            .is_empty()
    })
    .await;

    let slot_after = runtime
        .tasks
        .inspect_effect_slot(bound.permit.permit_id, 0)
        .expect("slot after routing");
    assert_eq!(slot_after.state, SlotState::EffectClosed);
    let history = runtime
        .tasks
        .list_effect_history(bound.task_id)
        .expect("effect history");
    let entry = history
        .iter()
        .find(|entry| entry.logical_effect_id == slot_after.logical_effect_id)
        .expect("late closure appended an effect history entry");
    assert_eq!(
        entry.operation_id,
        Some(bound.operation.operation_id.into_bytes()),
        "the history entry must cite the routed operation"
    );
    assert_eq!(
        entry.authoritative_effect_receipt_id,
        slot_after.effect_receipt_id.expect("closure receipt")
    );

    let lane = runtime.reconcile_lane();
    assert!(lane.routed >= 1, "the routing must be counted");
    assert_eq!(lane.no_route, 0);
    assert_eq!(lane.failed, 0);
    assert_eq!(lane.last_detail, None);

    let health = runtime.pump_health().expect("pump health");
    assert_eq!(health.state, PumpState::Running);
    assert_eq!(health.consecutive_failures, 0);
    runtime.stop_pump();
}

/// Given/When/Then: given a bound (routable) `ReconcileEffect` entry that a
/// manual adjudication parked, and a running pump; when the runtime's W58-1
/// unpark entry point recovers the entry; then the store reverse commits and
/// the pump redelivers it — the entry routes into the task authority and is
/// acknowledged away, the lane health shows the adjudication (unparked count
/// and reason) and the recovered delivery, and the park history reads the
/// entry as recovered with one unpark. The unpark replay through the runtime
/// stays idempotent, and the typed refusals (unknown sequence, never parked)
/// surface as the store's operation error.
#[tokio::test]
async fn parked_entry_unparks_through_runtime_and_redelivers() {
    let dir = TempDir::new("unpark-recovery");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let bound = bound_effect_slot(&runtime);

    // Cancel wins the wake fence, then the authoritative completion lands:
    // one routable `ReconcileEffect` row commits.
    runtime
        .operations
        .request_cancel(bound.operation, ReceiptId::from_bytes(seeded(0xb1)))
        .expect("cancel request");
    runtime
        .operations
        .complete(
            bound.ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes(seeded(0xb2)),
            },
        )
        .expect("late completion canonicalizes for reconciliation");
    let pending = runtime.operations.pending_outbox(16).expect("pending");
    assert_eq!(pending.len(), 1);
    let sequence = pending[0].sequence;

    // The adjudication parks the routable entry (for example a suspected
    // consumer outage), so the pending lane no longer serves it.
    runtime
        .operations
        .park_outbox_entry(sequence, "manual: suspected consumer outage", 6_000)
        .expect("park");
    assert!(
        runtime
            .operations
            .pending_outbox(16)
            .expect("pending")
            .is_empty(),
        "the parked row leaves the pending lane"
    );

    // The runtime's unpark entry point fails closed on the typed lanes.
    match runtime.unpark_outbox_entry(4_242, "unknown entry") {
        Err(SliceKError::Operation(error)) => {
            assert!(matches!(error, nlos_store::StoreError::OutboxEntryNotFound));
        }
        other => panic!("expected the typed not-found refusal, got {other:?}"),
    }

    let adapter = adapter();
    runtime.start_pump(&adapter).expect("start pump");

    let decision = runtime
        .unpark_outbox_entry(sequence, "adjudicated: outage resolved, retry")
        .expect("unpark through the runtime entry point");
    assert!(matches!(
        decision,
        nlos_store::OutboxUnparkDecision::Unparked { .. }
    ));
    // The replay is idempotent through the same entry point (the durable
    // clock key replays the reading, the store replays the decision).
    let replay = runtime
        .unpark_outbox_entry(sequence, "adjudicated: outage resolved, retry")
        .expect("unpark replay");
    assert!(matches!(
        replay,
        nlos_store::OutboxUnparkDecision::Replayed { .. }
    ));

    // The recovered entry redelivers, routes, and is acknowledged.
    wait_until("recovered entry routed and acknowledged", || {
        runtime
            .operations
            .pending_outbox(16)
            .expect("pending")
            .is_empty()
    })
    .await;
    let slot = runtime
        .tasks
        .inspect_effect_slot(bound.permit.permit_id, 0)
        .expect("slot after recovery routing");
    assert_eq!(slot.state, SlotState::EffectClosed);

    let lane = runtime.reconcile_lane();
    assert!(lane.routed >= 1, "the recovered entry routed");
    assert_eq!(lane.no_route, 0);
    assert_eq!(lane.failed, 0);
    assert_eq!(lane.parked, 0, "nothing was parked by the lane itself");
    assert_eq!(lane.unparked, 2, "the unpark and its replay are counted");
    assert_eq!(
        lane.last_unpark_reason.as_deref(),
        Some("adjudicated: outage resolved, retry")
    );
    assert!(
        lane.recovered >= 1,
        "the post-unpark redelivery that applied is visible"
    );

    let history = runtime
        .operations
        .inspect_outbox_park_history_entry(sequence)
        .expect("park history")
        .expect("the recovered row keeps its history");
    assert!(!history.parked);
    assert!(history.acknowledged, "the redelivery was acknowledged");
    assert_eq!(history.park_count, 1);
    assert_eq!(history.unpark_count, 1);
    assert_eq!(
        history.unpark_reason.as_deref(),
        Some("adjudicated: outage resolved, retry")
    );

    runtime.stop_pump();
}

/// Given/When/Then: given a running pump; when a second start is requested;
/// then the runtime fails closed instead of letting a second pump
/// generation ack the first lane's wakes as `FiberGone`.
#[tokio::test]
async fn second_start_while_running_fails_closed() {
    let dir = TempDir::new("double-start");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let adapter = adapter();
    runtime.start_pump(&adapter).expect("start pump");
    match runtime.start_pump(&adapter) {
        Err(SliceKError::Pump(reason)) => {
            assert!(reason.contains("already running"));
        }
        other => panic!("expected a pump lifecycle refusal, got {other:?}"),
    }
    runtime.stop_pump();
}

/// Given/When/Then: given a runtime with a running pump; when it is dropped
/// without an explicit stop; then Drop joins the pump thread within a
/// bounded deadline (no hang, no orphaned consumer against the database)
/// and the same root reopens cleanly.
#[tokio::test]
async fn drop_joins_pump_bounded_and_root_reopens() {
    let dir = TempDir::new("drop");
    let root = dir.root().to_path_buf();
    {
        let runtime = SliceKRuntime::open(&root).expect("open runtime");
        let adapter = adapter();
        runtime.start_pump(&adapter).expect("start pump");
        complete_orphaned_operation(&runtime, 0x70);
        let started = std::time::Instant::now();
        drop(runtime);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "drop must join the pump promptly, took {elapsed:?}"
        );
    }
    let runtime = SliceKRuntime::open(&root).expect("reopen after drop");
    let adapter = adapter();
    runtime.start_pump(&adapter).expect("pump after reopen");
    wait_until("backlog drained by the reopened runtime's pump", || {
        runtime
            .operations
            .pending_outbox(16)
            .expect("pending")
            .is_empty()
    })
    .await;
}
