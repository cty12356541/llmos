//! Wave D-b: the production Outbox pump wiring of [`SliceKRuntime`].
//!
//! The durable closed loop under test: terminal Operation commits write
//! `WakeFiber`/`ReconcileEffect` rows into `operation_outbox` in the same
//! transaction, and the runtime-owned pump (started against a caller's
//! runtime adapter) drains, applies, and acknowledges them — with the
//! fail-closed reconcile lane keeping unconsumable entries durable,
//! visible, and retried instead of silently dropped.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nlos_operation::{CompletionOutcome, OperationSpec};
use nlos_runtime::{FiberExit, FiberFuture, FiberHandle, FiberSpec, RuntimeAdapter as _};
use nlos_runtime_tokio::{PumpState, TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome};
use nlos_slice_k::{SliceKError, SliceKRuntime};
use nlos_store::OutboxKind;
use nlos_types::{
    AgentInstanceId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId,
    ProcessId, ReceiptId, ResourceGroupId, SchedulerDomainId,
};

/// Generous bound for "the 25ms fallback poll must have delivered by now".
const DRAIN_BOUND: Duration = Duration::from_secs(5);
/// Poll step for bounded waits.
const POLL_STEP: Duration = Duration::from_millis(10);
/// Several pump poll intervals — enough for a retried refusal to be
/// re-offered (and re-counted) at least twice.
const RETRY_WINDOW: Duration = Duration::from_millis(300);

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
/// `CanonicalizedForReconciliation` and commits a `ReconcileEffect` row);
/// when the pump offers it to the slice's reconcile lane; then the sink
/// refuses with the typed error — the entry is retried (refusal counter
/// grows) but never acknowledged, a later `WakeFiber` entry queued behind
/// it stays durable too (in-order batches), and the pump reports this as
/// backpressure, staying `Running` with zero drain failures.
#[tokio::test]
async fn reconcile_effect_backs_off_visibly_without_being_acked() {
    let dir = TempDir::new("reconcile-refusal");
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
        .expect("register")
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

    tokio::time::sleep(RETRY_WINDOW).await;
    let pending = runtime.operations.pending_outbox(16).expect("pending");
    assert_eq!(pending.len(), 2, "neither entry may be acknowledged away");
    assert_eq!(pending[0].kind, OutboxKind::ReconcileEffect);

    let refusals = runtime.reconcile_refusals();
    assert!(
        refusals.total >= 2,
        "the pump re-offered the refused entry ({} refusals recorded)",
        refusals.total
    );
    assert!(
        refusals
            .last_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("slice-k has no reconcile consumer")),
        "the refusal reason must name the fail-closed routing decision"
    );

    let health = runtime.pump_health().expect("pump health");
    assert_eq!(health.state, PumpState::Running);
    assert_eq!(health.consecutive_failures, 0);
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
