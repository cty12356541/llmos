//! End-to-end integration tests for the durable Outbox closed loop:
//! `SQLite` authority → bounded pump → `TokioWakeSink`/reconcile sink.
//!
//! Every test uses a temporary `SQLite` database, the real store, the real
//! pump thread, and the real Tokio wake sink. Timing assertions use Tokio
//! time with bounded probes, never `std::thread::sleep` on the test side
//! (the slow sink inside the pump thread is the one allowed exception).

use std::collections::HashSet;
use std::fs;
use std::future::pending;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nlos_operation::{CallbackTicket, CompletionOutcome, OperationSpec};
use nlos_outbox::{
    ConsumerConfig, OutboxConsumer, OutboxError, OutboxItem, OutboxSource, ReconcileSink,
};
use nlos_runtime::{FiberHandle, FiberSpec, RuntimeAdapter, RuntimeError, WakeOutcome, WakeSink};
use nlos_runtime_tokio::{
    OperationWait, OutboxPump, PumpConfig, PumpState, RecordingReconcileSink, StoreOutboxSource,
    TokioRuntimeAdapter, TokioRuntimeConfig, TokioWakeSink, WaitOutcome,
};
use nlos_store::SqliteOperationStore;
use nlos_types::{
    AgentInstanceId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId,
    ProcessId, ReceiptId, ResourceGroupId, SchedulerDomainId,
};
use tokio::runtime::Handle;

/// Bounded observation window for "still pending" assertions.
const PENDING_PROBE: Duration = Duration::from_millis(150);
/// Generous bound for events that must happen.
const RESOLVE: Duration = Duration::from_secs(10);
/// Polling step inside `wait_until`.
const POLL_STEP: Duration = Duration::from_millis(10);

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-outbox-it-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn id_bytes(value: u64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&value.to_be_bytes());
    bytes
}

fn scope_id(value: u64) -> CancellationScopeId {
    CancellationScopeId::from_bytes(id_bytes(50_000 + value))
}

fn callback(value: u64) -> CallbackId {
    CallbackId::from_bytes(id_bytes(60_000 + value))
}

fn receipt(value: u64) -> ReceiptId {
    ReceiptId::from_bytes(id_bytes(70_000 + value))
}

fn fiber_spec(index: u64, generation: Generation, scope: CancellationScopeId) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        fiber_generation: generation,
        agent_instance_id: AgentInstanceId::from_bytes(id_bytes(10_000 + index)),
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

fn op_spec(index: u64, owner: FiberHandle) -> OperationSpec {
    OperationSpec {
        operation_id: OperationId::from_bytes(id_bytes(index)),
        generation: Generation::INITIAL,
        owner_fiber: owner,
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(80_000 + index)),
        cancellation_generation: Generation::INITIAL,
    }
}

fn runtime(max_live_fibers: usize) -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(Handle::current(), TokioRuntimeConfig { max_live_fibers })
        .expect("runtime")
}

fn start_pump<S, W, R>(source: S, wake_sink: W, reconcile_sink: R, batch_limit: usize) -> OutboxPump
where
    S: OutboxSource + 'static,
    W: WakeSink + 'static,
    R: ReconcileSink + 'static,
{
    OutboxPump::start(
        OutboxConsumer {
            source,
            wake_sink,
            reconcile_sink,
            config: ConsumerConfig { batch_limit },
        },
        PumpConfig::default(),
    )
}

/// Polls `condition` with Tokio time until it holds or the bound expires.
async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(RESOLVE, async move {
        while !condition() {
            tokio::time::sleep(POLL_STEP).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn pending_count(store: &SqliteOperationStore) -> usize {
    store.pending_outbox(64).expect("pending outbox").len()
}

/// Wake sink decorator recording every delivered outcome.
#[derive(Clone)]
struct CountingWakeSink {
    inner: TokioWakeSink,
    outcomes: Arc<Mutex<Vec<WakeOutcome>>>,
}

impl CountingWakeSink {
    fn new(inner: TokioWakeSink) -> Self {
        Self {
            inner,
            outcomes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn outcomes(&self) -> Vec<WakeOutcome> {
        lock(&self.outcomes).clone()
    }
}

impl WakeSink for CountingWakeSink {
    fn wake(
        &self,
        fiber: &FiberHandle,
        operation_id: OperationId,
        operation_generation: Generation,
    ) -> Result<WakeOutcome, RuntimeError> {
        let outcome = self.inner.wake(fiber, operation_id, operation_generation)?;
        lock(&self.outcomes).push(outcome);
        Ok(outcome)
    }
}

/// Reconcile sink decorator counting raw (pre-idempotency) applies.
#[derive(Clone)]
struct CountingReconcileSink {
    inner: RecordingReconcileSink,
    raw_calls: Arc<AtomicU64>,
}

impl CountingReconcileSink {
    fn new(inner: RecordingReconcileSink) -> Self {
        Self {
            inner,
            raw_calls: Arc::new(AtomicU64::new(0)),
        }
    }

    fn raw_calls(&self) -> u64 {
        self.raw_calls.load(Ordering::Acquire)
    }
}

impl ReconcileSink for CountingReconcileSink {
    fn reconcile(&self, item: &OutboxItem) -> Result<(), OutboxError> {
        self.raw_calls.fetch_add(1, Ordering::AcqRel);
        self.inner.reconcile(item)
    }
}

/// Source decorator failing the first ACK of every sequence, which forces
/// at-least-once redelivery of every entry.
struct FirstAckFailsSource {
    inner: StoreOutboxSource,
    failed: Mutex<HashSet<u64>>,
}

impl FirstAckFailsSource {
    fn new(inner: StoreOutboxSource) -> Self {
        Self {
            inner,
            failed: Mutex::new(HashSet::new()),
        }
    }
}

impl OutboxSource for FirstAckFailsSource {
    fn pending(&self, limit: usize) -> Result<Vec<OutboxItem>, OutboxError> {
        self.inner.pending(limit)
    }

    fn ack(&self, sequence: u64) -> Result<(), OutboxError> {
        if lock(&self.failed).insert(sequence) {
            return Err(OutboxError::Source {
                detail: "scripted first ack failure".to_owned(),
            });
        }
        self.inner.ack(sequence)
    }
}

/// Source decorator whose ACK boundary is permanently down; used by the
/// crash-consumer child process to apply an entry without acknowledging it.
struct NeverAckSource {
    inner: StoreOutboxSource,
}

impl OutboxSource for NeverAckSource {
    fn pending(&self, limit: usize) -> Result<Vec<OutboxItem>, OutboxError> {
        self.inner.pending(limit)
    }

    fn ack(&self, _sequence: u64) -> Result<(), OutboxError> {
        Err(OutboxError::Source {
            detail: "ack boundary down".to_owned(),
        })
    }
}

/// Source decorator recording the size of every polled batch.
struct BatchRecordingSource {
    inner: StoreOutboxSource,
    returned: Arc<Mutex<Vec<usize>>>,
}

impl BatchRecordingSource {
    fn new(inner: StoreOutboxSource, returned: Arc<Mutex<Vec<usize>>>) -> Self {
        Self { inner, returned }
    }
}

impl OutboxSource for BatchRecordingSource {
    fn pending(&self, limit: usize) -> Result<Vec<OutboxItem>, OutboxError> {
        let items = self.inner.pending(limit)?;
        lock(&self.returned).push(items.len());
        Ok(items)
    }

    fn ack(&self, sequence: u64) -> Result<(), OutboxError> {
        self.inner.ack(sequence)
    }
}

/// Wake sink decorator adding artificial apply latency on the pump thread.
struct SlowWakeSink {
    inner: TokioWakeSink,
    delay: Duration,
}

impl WakeSink for SlowWakeSink {
    fn wake(
        &self,
        fiber: &FiberHandle,
        operation_id: OperationId,
        operation_generation: Generation,
    ) -> Result<WakeOutcome, RuntimeError> {
        // Runs on the dedicated pump thread, simulating a slow consumer; the
        // writer path must not observe this latency.
        std::thread::sleep(self.delay);
        self.inner.wake(fiber, operation_id, operation_generation)
    }
}

/// Wake sink accepting everything; used by the crash-consumer child, which
/// has no runtime of its own.
struct NullWakeSink;

impl WakeSink for NullWakeSink {
    fn wake(
        &self,
        _fiber: &FiberHandle,
        _operation_id: OperationId,
        _operation_generation: Generation,
    ) -> Result<WakeOutcome, RuntimeError> {
        Ok(WakeOutcome::Delivered)
    }
}

/// 1. A current callback committed to the authority wakes the waiting fiber
/// only through the Outbox pump.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_callback_commit_then_pump_wakes_waiting_fiber() {
    let database = TestDatabase::new("current-wake");
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("open"));
    let runtime = runtime(2);
    let scope = scope_id(1);
    let fiber = runtime
        .spawn_fiber(
            fiber_spec(1, Generation::INITIAL, scope),
            Box::pin(pending()),
        )
        .expect("spawn");

    let handle = store
        .register(op_spec(1, fiber))
        .expect("register")
        .handle();
    let ticket = store.dispatch(handle, callback(1)).expect("dispatch");
    let wait = runtime
        .wait_for_operation(fiber, handle.operation_id, handle.generation)
        .expect("wait");

    let reconcile = RecordingReconcileSink::default();
    let pump = start_pump(
        StoreOutboxSource::new(Arc::clone(&store)),
        runtime.wake_sink(),
        reconcile.clone(),
        8,
    );

    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(1),
            },
        )
        .expect("complete");
    let _ = pump.hint();

    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("wake resolves"),
        WaitOutcome::Woken
    );
    wait_until("outbox fully acked", || pending_count(&store) == 0).await;
    pump.stop();
    assert!(reconcile.is_empty());
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// 2. A late callback after cancel is reconciled and never wakes the fiber.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_callback_is_reconciled_without_waking_the_fiber() {
    let database = TestDatabase::new("late-callback");
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("open"));
    let runtime = runtime(2);
    let scope = scope_id(2);
    let fiber = runtime
        .spawn_fiber(
            fiber_spec(2, Generation::INITIAL, scope),
            Box::pin(pending()),
        )
        .expect("spawn");

    let handle = store
        .register(op_spec(2, fiber))
        .expect("register")
        .handle();
    let ticket = store.dispatch(handle, callback(2)).expect("dispatch");
    let mut wait = runtime
        .wait_for_operation(fiber, handle.operation_id, handle.generation)
        .expect("wait");
    store
        .request_cancel(handle, receipt(2))
        .expect("cancel after dispatch");

    let reconcile = RecordingReconcileSink::default();
    let pump = start_pump(
        StoreOutboxSource::new(Arc::clone(&store)),
        runtime.wake_sink(),
        reconcile.clone(),
        8,
    );

    let decision = store
        .complete(
            ticket,
            CompletionOutcome::PartialEffect {
                receipt_id: receipt(3),
            },
        )
        .expect("late complete");
    assert!(matches!(
        decision,
        nlos_operation::CompletionDecision::CanonicalizedForReconciliation { .. }
    ));
    let _ = pump.hint();

    wait_until("reconcile effect recorded", || reconcile.len() == 1).await;
    let recorded = reconcile.records();
    assert_eq!(recorded[0].kind, nlos_outbox::OutboxKind::ReconcileEffect);
    assert_eq!(recorded[0].callback_id, Some(ticket.callback_id));
    wait_until("outbox fully acked", || pending_count(&store) == 0).await;

    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut wait)
            .await
            .is_err(),
        "the reconciled entry must not wake the waiting fiber"
    );
    pump.stop();
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("cancelled wait resolves"),
        WaitOutcome::Cancelled
    );
}

/// 3. Cancel-before-dispatch commits the terminal state and the
/// `callback_id = None` wake in one transaction; the fiber is woken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_before_dispatch_wakes_fiber_without_callback_identity() {
    let database = TestDatabase::new("cancel-before-dispatch");
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("open"));
    let runtime = runtime(2);
    let scope = scope_id(3);
    let fiber = runtime
        .spawn_fiber(
            fiber_spec(3, Generation::INITIAL, scope),
            Box::pin(pending()),
        )
        .expect("spawn");

    let handle = store
        .register(op_spec(3, fiber))
        .expect("register")
        .handle();
    let wait = runtime
        .wait_for_operation(fiber, handle.operation_id, handle.generation)
        .expect("wait");

    let reconcile = RecordingReconcileSink::default();
    let pump = start_pump(
        StoreOutboxSource::new(Arc::clone(&store)),
        runtime.wake_sink(),
        reconcile.clone(),
        8,
    );

    let snapshot = store
        .request_cancel(handle, receipt(4))
        .expect("cancel before dispatch");
    assert!(matches!(
        snapshot.state,
        nlos_operation::OperationState::CancelledBeforeEffect { .. }
    ));
    let _ = pump.hint();

    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("wake resolves"),
        WaitOutcome::Woken
    );
    wait_until("outbox fully acked", || pending_count(&store) == 0).await;
    pump.stop();
    assert!(reconcile.is_empty());
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// 4. Redelivery after a lost ACK is absorbed: exactly one logical wake and
/// exactly one recorded reconciliation despite duplicate applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redelivery_after_lost_ack_causes_no_second_logical_effect() {
    let database = TestDatabase::new("redelivery");
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("open"));
    let runtime = runtime(2);
    let scope = scope_id(4);
    let fiber = runtime
        .spawn_fiber(
            fiber_spec(4, Generation::INITIAL, scope),
            Box::pin(pending()),
        )
        .expect("spawn");

    // Operation 4 takes the wake path; operation 5 the reconciliation path.
    let wake_handle = store
        .register(op_spec(4, fiber))
        .expect("register")
        .handle();
    let wake_ticket = store.dispatch(wake_handle, callback(4)).expect("dispatch");
    let wait = runtime
        .wait_for_operation(fiber, wake_handle.operation_id, wake_handle.generation)
        .expect("wait");
    store
        .complete(
            wake_ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(5),
            },
        )
        .expect("complete wake path");

    let reconcile_handle = store
        .register(op_spec(5, fiber))
        .expect("register")
        .handle();
    let reconcile_ticket = store
        .dispatch(reconcile_handle, callback(5))
        .expect("dispatch");
    store
        .request_cancel(reconcile_handle, receipt(6))
        .expect("cancel after dispatch");
    store
        .complete(
            reconcile_ticket,
            CompletionOutcome::PartialEffect {
                receipt_id: receipt(7),
            },
        )
        .expect("complete reconcile path");

    let source = FirstAckFailsSource::new(StoreOutboxSource::new(Arc::clone(&store)));
    let wake = CountingWakeSink::new(runtime.wake_sink());
    let recording = RecordingReconcileSink::default();
    let reconcile = CountingReconcileSink::new(recording.clone());
    let pump = start_pump(source, wake.clone(), reconcile.clone(), 8);
    let _ = pump.hint();

    assert_eq!(
        tokio::time::timeout(RESOLVE, wait)
            .await
            .expect("wake resolves"),
        WaitOutcome::Woken
    );
    wait_until("outbox fully acked", || pending_count(&store) == 0).await;
    wait_until("reconcile effect recorded", || recording.len() == 1).await;
    pump.stop();

    // Both entries were applied twice (first ACK lost), but the sinks absorb
    // the duplicate: one logical wake, one recorded reconciliation.
    assert_eq!(wake.outcomes().len(), 2);
    assert_eq!(reconcile.raw_calls(), 2);
    assert_eq!(recording.len(), 1);
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// 5. A wake fenced to a fiber generation that is not live classifies as
/// `FiberGone`, is acknowledged, and does not touch the live generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_generation_wake_is_fiber_gone_and_acked() {
    let database = TestDatabase::new("stale-generation");
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("open"));
    let runtime = runtime(2);
    let scope = scope_id(5);
    let live_generation = Generation::INITIAL;
    let fiber = runtime
        .spawn_fiber(fiber_spec(5, live_generation, scope), Box::pin(pending()))
        .expect("spawn");

    // The committed entry targets the same fiber id at a newer generation
    // that this runtime never admitted.
    let stale_owner = FiberHandle {
        fiber_id: fiber.fiber_id,
        generation: live_generation.checked_next().expect("next generation"),
    };
    let handle = store
        .register(op_spec(6, stale_owner))
        .expect("register")
        .handle();
    let ticket = store.dispatch(handle, callback(6)).expect("dispatch");
    let mut wait = runtime
        .wait_for_operation(fiber, handle.operation_id, handle.generation)
        .expect("wait");

    let wake = CountingWakeSink::new(runtime.wake_sink());
    let reconcile = RecordingReconcileSink::default();
    let pump = start_pump(
        StoreOutboxSource::new(Arc::clone(&store)),
        wake.clone(),
        reconcile.clone(),
        8,
    );
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(8),
            },
        )
        .expect("complete");
    let _ = pump.hint();

    wait_until("outbox fully acked", || pending_count(&store) == 0).await;
    pump.stop();
    assert_eq!(wake.outcomes(), vec![WakeOutcome::FiberGone]);
    assert!(reconcile.is_empty());
    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut wait)
            .await
            .is_err(),
        "the live generation must not be woken by the stale entry"
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// 6. A consumer that crashes after apply but before ACK causes redelivery
/// on restart: nothing is lost, and the replayed wake classifies as
/// `FiberGone` on the fresh runtime (no fiber rehydration yet).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consumer_crash_before_ack_redelivers_without_loss_or_stale_wake() {
    let database = TestDatabase::new("consumer-crash");
    let status = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_consumer_helper", "--nocapture"])
        .env("NLOS_OUTBOX_CRASH_DATABASE", &database.path)
        .status()
        .expect("run crash consumer");
    assert_eq!(status.code(), Some(97));

    // Runtime "restart": the fresh adapter holds no fiber records.
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("reopen"));
    assert_eq!(pending_count(&store), 1, "unacked entry survives the crash");

    let runtime = runtime(2);
    let scope = scope_id(6);
    // A live fiber reusing the fiber id at a new generation must not observe
    // the replayed wake.
    let fiber = runtime
        .spawn_fiber(
            fiber_spec(
                900,
                Generation::INITIAL.checked_next().expect("next generation"),
                scope,
            ),
            Box::pin(pending()),
        )
        .expect("spawn");
    let mut wait = runtime
        .wait_for_operation(
            fiber,
            OperationId::from_bytes(id_bytes(900)),
            Generation::INITIAL,
        )
        .expect("wait");

    let wake = CountingWakeSink::new(runtime.wake_sink());
    let reconcile = RecordingReconcileSink::default();
    let pump = start_pump(
        StoreOutboxSource::new(Arc::clone(&store)),
        wake.clone(),
        reconcile.clone(),
        8,
    );
    let _ = pump.hint();

    wait_until("outbox fully acked", || pending_count(&store) == 0).await;
    pump.stop();
    assert_eq!(wake.outcomes(), vec![WakeOutcome::FiberGone]);
    assert!(reconcile.is_empty());
    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut wait)
            .await
            .is_err(),
        "the replayed wake must not touch the new generation fiber"
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// Child process for the crash test: commits the terminal state, applies the
/// Outbox entry, and exits with code 97 before the ACK can commit.
#[test]
fn crash_consumer_helper() {
    let Ok(path) = std::env::var("NLOS_OUTBOX_CRASH_DATABASE") else {
        return;
    };
    let store = SqliteOperationStore::open(path).expect("open crash consumer store");
    let owner = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(900)),
        generation: Generation::INITIAL,
    };
    let handle = store
        .register(op_spec(900, owner))
        .expect("register")
        .handle();
    let ticket = store.dispatch(handle, callback(900)).expect("dispatch");
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(900),
            },
        )
        .expect("durable completion");

    let consumer = OutboxConsumer {
        source: NeverAckSource {
            inner: StoreOutboxSource::new(Arc::new(store)),
        },
        wake_sink: NullWakeSink,
        reconcile_sink: RecordingReconcileSink::default(),
        config: ConsumerConfig { batch_limit: 8 },
    };
    let report = consumer.drain_once().expect("apply succeeds");
    assert_eq!(report.applied, 1);
    assert_eq!(report.acked, 0);
    std::process::exit(97);
}

/// 7. Bounded batches and a slow consumer never block the authority writer:
/// all commits return promptly, every batch respects the limit, and every
/// entry is eventually delivered and acknowledged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_batches_and_slow_sink_do_not_block_the_writer() {
    const OPERATIONS: u64 = 10;
    const FIRST_HALF: usize = 5;

    let database = TestDatabase::new("backpressure");
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("open"));
    let runtime = runtime(16);
    let scope = scope_id(7);

    let mut dispatches: Vec<(CallbackTicket, OperationWait)> = Vec::new();
    for index in 0..OPERATIONS {
        let fiber = runtime
            .spawn_fiber(
                fiber_spec(1_000 + index, Generation::INITIAL, scope),
                Box::pin(pending()),
            )
            .expect("spawn");
        let handle = store
            .register(op_spec(1_000 + index, fiber))
            .expect("register")
            .handle();
        let ticket = store
            .dispatch(handle, callback(1_000 + index))
            .expect("dispatch");
        let wait = runtime
            .wait_for_operation(fiber, handle.operation_id, handle.generation)
            .expect("wait");
        dispatches.push((ticket, wait));
    }
    // Commit half the entries up front so the consumer starts with a backlog.
    for (index, (ticket, _)) in dispatches[..FIRST_HALF].iter().enumerate() {
        store
            .complete(
                *ticket,
                CompletionOutcome::Completed {
                    receipt_id: receipt(2_000 + index as u64),
                },
            )
            .expect("backlog complete");
    }

    let batch_sizes = Arc::new(Mutex::new(Vec::new()));
    let source = BatchRecordingSource::new(
        StoreOutboxSource::new(Arc::clone(&store)),
        Arc::clone(&batch_sizes),
    );
    let slow_sink = SlowWakeSink {
        inner: runtime.wake_sink(),
        delay: Duration::from_millis(50),
    };
    let pump = start_pump(source, slow_sink, RecordingReconcileSink::default(), 4);
    let _ = pump.hint();

    // The writer commits the second half from its own thread while the slow
    // consumer is still working through the backlog.
    let remaining: Vec<CallbackTicket> = dispatches[FIRST_HALF..]
        .iter()
        .map(|(ticket, _)| *ticket)
        .collect();
    let writer_store = Arc::clone(&store);
    let writer = std::thread::spawn(move || {
        let started = Instant::now();
        for (offset, ticket) in remaining.into_iter().enumerate() {
            writer_store
                .complete(
                    ticket,
                    CompletionOutcome::Completed {
                        receipt_id: receipt(3_000 + offset as u64),
                    },
                )
                .expect("writer complete");
        }
        started.elapsed()
    });
    let writer_elapsed = writer.join().expect("writer thread");
    // Coarse upper bound only: the writer must not be dragged by the slow
    // sink. 1s is deliberately generous so slow-disk CI cannot flake; the
    // real signal is the backlog assertion below, not the wall clock.
    assert!(
        writer_elapsed < Duration::from_secs(1),
        "writer must not be dragged by the slow consumer: {writer_elapsed:?}"
    );
    assert!(
        pending_count(&store) > 0,
        "the writer outran the slow consumer, proving the writer was not blocked"
    );
    let _ = pump.hint();

    for (_, wait) in dispatches {
        assert_eq!(
            tokio::time::timeout(RESOLVE, wait)
                .await
                .expect("wake resolves"),
            WaitOutcome::Woken
        );
    }
    wait_until("outbox fully acked", || pending_count(&store) == 0).await;
    pump.stop();

    let batches = lock(&batch_sizes).clone();
    let non_empty: Vec<usize> = batches.into_iter().filter(|&size| size > 0).collect();
    assert!(
        non_empty.len() >= 2,
        "ten entries at limit four require multiple batches: {non_empty:?}"
    );
    assert!(
        non_empty.iter().all(|&size| size <= 4),
        "every batch respects the limit: {non_empty:?}"
    );
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// 8. Ordering: the fiber never observes a wake before `complete()` returns;
/// a shared atomic sequence proves commit precedes observation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_is_observed_only_after_commit_returns() {
    let database = TestDatabase::new("ordering");
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("open"));
    let runtime = runtime(2);
    let scope = scope_id(8);
    let fiber = runtime
        .spawn_fiber(
            fiber_spec(8, Generation::INITIAL, scope),
            Box::pin(pending()),
        )
        .expect("spawn");

    let handle = store
        .register(op_spec(8, fiber))
        .expect("register")
        .handle();
    let ticket = store.dispatch(handle, callback(8)).expect("dispatch");
    let mut wait = runtime
        .wait_for_operation(fiber, handle.operation_id, handle.generation)
        .expect("wait");

    let pump = start_pump(
        StoreOutboxSource::new(Arc::clone(&store)),
        runtime.wake_sink(),
        RecordingReconcileSink::default(),
        8,
    );

    // Before the commit returns there is no committed entry, hence no wake.
    assert!(
        tokio::time::timeout(PENDING_PROBE, &mut wait)
            .await
            .is_err(),
        "no wake may be observed before the commit returns"
    );

    let order = AtomicU64::new(0);
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(9),
            },
        )
        .expect("complete");
    let commit_order = order.fetch_add(1, Ordering::SeqCst);
    let _ = pump.hint();

    let outcome = tokio::time::timeout(RESOLVE, wait)
        .await
        .expect("wake resolves");
    let wake_order = order.fetch_add(1, Ordering::SeqCst);
    assert_eq!(outcome, WaitOutcome::Woken);
    assert!(
        commit_order < wake_order,
        "the wake observation must be ordered after the commit return"
    );
    wait_until("outbox fully acked", || pending_count(&store) == 0).await;
    pump.stop();
    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
}

/// 9. Runtime shutdown is terminal, not transient: with an unacknowledged
/// entry pending, the pump observes `DrainReport::shutdown`, transitions to
/// `Stopped`, ends its thread (so `stop()` joins promptly), and leaves the
/// entry durable instead of retrying every poll interval forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_shutdown_stops_the_pump_without_draining_the_outbox() {
    let database = TestDatabase::new("shutdown-terminal");
    let store = Arc::new(SqliteOperationStore::open(&database.path).expect("open"));
    let runtime = runtime(2);
    let scope = scope_id(9);
    let fiber = runtime
        .spawn_fiber(
            fiber_spec(9, Generation::INITIAL, scope),
            Box::pin(pending()),
        )
        .expect("spawn");

    let handle = store
        .register(op_spec(9, fiber))
        .expect("register")
        .handle();
    let ticket = store.dispatch(handle, callback(9)).expect("dispatch");
    store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: receipt(10),
            },
        )
        .expect("complete");
    assert_eq!(pending_count(&store), 1);

    // Shut down before the pump starts so every wake hits the terminal
    // `ShuttingDown` path deterministically.
    runtime.shutdown();

    let pump = start_pump(
        StoreOutboxSource::new(Arc::clone(&store)),
        runtime.wake_sink(),
        RecordingReconcileSink::default(),
        8,
    );
    let _ = pump.hint();

    wait_until("pump reaches terminal Stopped", || {
        pump.health().state == PumpState::Stopped
    })
    .await;
    let health = pump.health();
    assert_eq!(health.consecutive_failures, 0);
    assert_eq!(health.last_error, None);

    // The unacknowledged entry stays durable and is not redelivered in a
    // retry loop: the pump thread has exited, so nothing polls anymore.
    assert_eq!(
        pending_count(&store),
        1,
        "shutdown must not drain or ack the durable entry"
    );
    pump.stop();
}
