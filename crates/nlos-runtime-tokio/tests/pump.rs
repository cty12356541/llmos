//! Pump-thread failure semantics: health observability, bounded exponential
//! backoff, faulting after too many consecutive failures, panic containment,
//! and poison apply stops (a durable entry whose apply keeps failing) being
//! backed off and observable rather than retried at the raw poll interval.
//! These tests use scripted in-memory fakes only — no `SQLite` store and no
//! Tokio runtime — so timing assertions stay deterministic.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nlos_operation::OperationState;
use nlos_outbox::{
    ConsumerConfig, OutboxConsumer, OutboxError, OutboxItem, OutboxKind, OutboxSource,
    ReconcileSink,
};
use nlos_runtime::{FiberHandle, RuntimeError, WakeOutcome, WakeSink};
use nlos_runtime_tokio::{
    OutboxPump, OutboxPumpStartError, PumpConfig, PumpState, RecordingReconcileSink,
};
use nlos_types::{CallbackId, ExecutionFiberId, Generation, OperationId, ReceiptId};

/// Generous bound for events that must happen.
const RESOLVE: Duration = Duration::from_secs(10);
/// Polling step inside `wait_until`.
const POLL_STEP: Duration = Duration::from_millis(5);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Polls `condition` until it holds or the bound expires.
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + RESOLVE;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(POLL_STEP);
    }
}

fn item(sequence: u64) -> OutboxItem {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&sequence.to_be_bytes());
    OutboxItem {
        sequence,
        kind: OutboxKind::WakeFiber,
        operation_id: OperationId::from_bytes(bytes),
        operation_generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([0x11; 16]),
            generation: Generation::INITIAL,
        },
        callback_id: Some(CallbackId::from_bytes([0x22; 16])),
        state: OperationState::Completed {
            receipt_id: ReceiptId::from_bytes([0x33; 16]),
        },
    }
}

/// Shared observation handle for [`FlakySource`].
struct FlakyProbe {
    attempts: Arc<AtomicUsize>,
    timestamps: Arc<Mutex<Vec<Instant>>>,
}

impl FlakyProbe {
    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Acquire)
    }

    /// Durations between consecutive `pending` attempts, in attempt order.
    fn gaps(&self) -> Vec<Duration> {
        let timestamps = lock(&self.timestamps);
        timestamps
            .windows(2)
            .map(|pair| pair[1].saturating_duration_since(pair[0]))
            .collect()
    }
}

/// Source that fails `pending` until `failures_remaining` hits zero, then
/// serves empty batches. Every attempt timestamp is recorded so the test can
/// observe the backoff directly.
struct FlakySource {
    failures_remaining: AtomicUsize,
    attempts: Arc<AtomicUsize>,
    timestamps: Arc<Mutex<Vec<Instant>>>,
}

impl FlakySource {
    fn new(failures: usize) -> (Self, FlakyProbe) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let timestamps = Arc::new(Mutex::new(Vec::new()));
        let probe = FlakyProbe {
            attempts: Arc::clone(&attempts),
            timestamps: Arc::clone(&timestamps),
        };
        (
            Self {
                failures_remaining: AtomicUsize::new(failures),
                attempts,
                timestamps,
            },
            probe,
        )
    }
}

impl OutboxSource for FlakySource {
    fn pending(&self, _limit: usize) -> Result<Vec<OutboxItem>, OutboxError> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        lock(&self.timestamps).push(Instant::now());
        if self.failures_remaining.load(Ordering::Acquire) > 0 {
            self.failures_remaining.fetch_sub(1, Ordering::AcqRel);
            return Err(OutboxError::Source {
                detail: "scripted persistent read failure".to_owned(),
            });
        }
        Ok(Vec::new())
    }

    fn ack(&self, _sequence: u64) -> Result<(), OutboxError> {
        Ok(())
    }
}

/// Source that always serves the same unacknowledged entry, so every drain
/// reaches the wake sink.
struct OneEntrySource;

impl OutboxSource for OneEntrySource {
    fn pending(&self, _limit: usize) -> Result<Vec<OutboxItem>, OutboxError> {
        Ok(vec![item(1)])
    }

    fn ack(&self, _sequence: u64) -> Result<(), OutboxError> {
        Ok(())
    }
}

/// Wake sink that panics on every delivery.
struct PanickingWakeSink;

impl WakeSink for PanickingWakeSink {
    fn wake(
        &self,
        _fiber: &FiberHandle,
        _operation_id: OperationId,
        _operation_generation: Generation,
    ) -> Result<WakeOutcome, RuntimeError> {
        panic!("scripted sink panic");
    }
}

fn start<S, W, R>(source: S, wake_sink: W, reconcile_sink: R, config: PumpConfig) -> OutboxPump
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
            config: ConsumerConfig { batch_limit: 8 },
        },
        config,
    )
    .expect("spawn outbox pump thread")
}

fn config(poll_interval: Duration, failure_threshold: usize) -> PumpConfig {
    PumpConfig {
        poll_interval,
        failure_threshold,
    }
}

/// A zero `poll_interval` is rejected at `start` instead of degenerating
/// into an unbacked busy-poll loop; no pump thread is spawned.
#[test]
fn zero_poll_interval_is_rejected_at_start() {
    let (source, _probe) = FlakySource::new(1);
    let rejection = OutboxPump::start(
        OutboxConsumer {
            source,
            wake_sink: PanickingWakeSink, // never reached: start fails first
            reconcile_sink: RecordingReconcileSink::default(),
            config: ConsumerConfig { batch_limit: 8 },
        },
        config(Duration::ZERO, 16),
    )
    .err()
    .expect("zero poll interval must be rejected");
    assert!(matches!(rejection, OutboxPumpStartError::InvalidConfig(_)));
}

/// A sub-millisecond `poll_interval` is rejected at `start` exactly like a
/// zero one: `recv_timeout(999µs)` still returns between source polls fast
/// enough to behave as an unbacked busy-poll, and the failure backoff —
/// derived from the same base — would inherit the degenerate floor.
#[test]
fn sub_millisecond_poll_interval_is_rejected_at_start() {
    let (source, _probe) = FlakySource::new(1);
    let rejection = OutboxPump::start(
        OutboxConsumer {
            source,
            wake_sink: PanickingWakeSink, // never reached: start fails first
            reconcile_sink: RecordingReconcileSink::default(),
            config: ConsumerConfig { batch_limit: 8 },
        },
        config(Duration::from_micros(999), 16),
    )
    .err()
    .expect("sub-millisecond poll interval must be rejected");
    assert!(matches!(rejection, OutboxPumpStartError::InvalidConfig(_)));
}

/// A consumer `batch_limit` of zero is rejected at `start`: the consumer
/// only debug-asserts it, so a release build would otherwise spawn a pump
/// that polls empty batches forever while reporting itself healthy.
#[test]
fn zero_batch_limit_is_rejected_at_start() {
    let (source, _probe) = FlakySource::new(1);
    let rejection = OutboxPump::start(
        OutboxConsumer {
            source,
            wake_sink: PanickingWakeSink, // never reached: start fails first
            reconcile_sink: RecordingReconcileSink::default(),
            config: ConsumerConfig { batch_limit: 0 },
        },
        config(Duration::from_millis(25), 16),
    )
    .err()
    .expect("zero batch limit must be rejected");
    match rejection {
        OutboxPumpStartError::InvalidConfig(reason) => {
            assert!(
                reason.contains("batch_limit"),
                "reason names the field: {reason}"
            );
        }
        error @ OutboxPumpStartError::Spawn(_) => {
            panic!("expected InvalidConfig, got {error:?}")
        }
    }
}

/// Observability: a persistently failing source shows up in `health()` with
/// the failure count and root-cause text, drain attempts are spaced by a
/// growing bounded backoff, and a later recovery resets the counter to zero.
#[test]
fn failing_source_is_observed_through_health_and_backoff() {
    const FAILURES: usize = 5;
    let (source, probe) = FlakySource::new(FAILURES);
    let pump = start(
        source,
        PanickingWakeSink, // never reached: `pending` is what fails
        RecordingReconcileSink::default(),
        config(Duration::from_millis(5), 16),
    );

    wait_until("all scripted failures to happen", || {
        pump.health().consecutive_failures >= FAILURES
    });
    let health = pump.health();
    assert_eq!(health.state, PumpState::Running);
    assert!(
        health
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("scripted persistent read failure")),
        "last_error must carry the root cause: {health:?}"
    );

    // Backoff: with a 5ms poll interval the minimum waits after failures
    // 1..=5 are 5/10/20/40/80ms. Runner scheduling may delay any attempt by
    // an arbitrary amount, so assert each configured lower bound rather than
    // comparing ratios between two scheduler-inflated observations.
    wait_until("the source to recover and serve again", || {
        probe.attempts() > FAILURES
    });
    let gaps = probe.gaps();
    assert!(
        gaps.len() >= FAILURES,
        "enough attempts recorded: {} gaps {gaps:?}",
        gaps.len()
    );
    for (failure, gap) in gaps.iter().take(FAILURES).enumerate() {
        let expected = Duration::from_millis(5 * (1_u64 << failure));
        assert!(
            *gap >= expected,
            "retry after failure {} was early: expected at least {expected:?}, got {gap:?}; all={gaps:?}",
            failure + 1
        );
    }

    // Recovery: once the source serves again, a successful drain resets the
    // failure counter while attempts keep increasing.
    wait_until("recovery resets the failure counter", || {
        pump.health().consecutive_failures == 0 && probe.attempts() > FAILURES
    });
    let health = pump.health();
    assert_eq!(health.state, PumpState::Running);
    assert_eq!(health.last_error, None);

    pump.stop();
}

/// Panic containment: a panicking sink cannot kill the pump thread silently.
/// Each panic is caught, counted, and retried with backoff until the failure
/// threshold faults the pump. `stop()` still joins promptly and health
/// records the panic as the last error.
#[test]
fn panicking_sink_faults_the_pump_without_killing_it() {
    let pump = start(
        OneEntrySource,
        PanickingWakeSink,
        RecordingReconcileSink::default(),
        config(Duration::from_millis(5), 3),
    );

    wait_until("pump to fault after the panic threshold", || {
        pump.health().state == PumpState::Faulted
    });
    let health = pump.health();
    assert_eq!(health.consecutive_failures, 3);
    assert_eq!(health.last_error.as_deref(), Some("consumer panicked"));

    // The pump thread has already exited, so join returns immediately.
    let started = Instant::now();
    pump.stop();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "stop() must join a faulted pump promptly"
    );
}

// ---------------------------------------------------------------------------
// Poison apply stops: a durable entry whose apply keeps failing.
// ---------------------------------------------------------------------------

fn reconcile_item(sequence: u64) -> OutboxItem {
    let mut entry = item(sequence);
    entry.kind = OutboxKind::ReconcileEffect;
    entry
}

/// Source that always serves the same unacknowledged `ReconcileEffect`
/// entry, so every drain reaches the reconcile sink and stops there.
struct PoisonSource;

impl OutboxSource for PoisonSource {
    fn pending(&self, _limit: usize) -> Result<Vec<OutboxItem>, OutboxError> {
        Ok(vec![reconcile_item(1)])
    }

    fn ack(&self, _sequence: u64) -> Result<(), OutboxError> {
        Ok(())
    }
}

/// Reconcile sink that always fails, recording every attempt timestamp so
/// the test can observe the backoff directly.
struct PoisonReconcileSink {
    timestamps: Arc<Mutex<Vec<Instant>>>,
}

impl PoisonReconcileSink {
    fn new() -> (Self, Arc<Mutex<Vec<Instant>>>) {
        let timestamps = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                timestamps: Arc::clone(&timestamps),
            },
            timestamps,
        )
    }
}

impl ReconcileSink for PoisonReconcileSink {
    fn reconcile(&self, _item: &OutboxItem) -> Result<(), OutboxError> {
        lock(&self.timestamps).push(Instant::now());
        Err(OutboxError::Reconcile {
            detail: "poison reconcile entry".to_owned(),
        })
    }
}

/// Source that redelivers one `ReconcileEffect` entry until it is
/// acknowledged, then serves empty batches.
struct UntilAckedSource {
    entry: OutboxItem,
    acked: Arc<std::sync::atomic::AtomicBool>,
}

impl OutboxSource for UntilAckedSource {
    fn pending(&self, _limit: usize) -> Result<Vec<OutboxItem>, OutboxError> {
        if self.acked.load(Ordering::Acquire) {
            Ok(Vec::new())
        } else {
            Ok(vec![self.entry])
        }
    }

    fn ack(&self, _sequence: u64) -> Result<(), OutboxError> {
        self.acked.store(true, Ordering::Release);
        Ok(())
    }
}

/// Reconcile sink that fails its first `failures` attempts and then
/// succeeds, recording every attempt.
struct FlakyReconcileSink {
    failures_remaining: AtomicUsize,
    attempts: Arc<AtomicUsize>,
}

impl FlakyReconcileSink {
    fn new(failures: usize) -> Self {
        Self {
            failures_remaining: AtomicUsize::new(failures),
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl ReconcileSink for FlakyReconcileSink {
    fn reconcile(&self, _item: &OutboxItem) -> Result<(), OutboxError> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        if self.failures_remaining.load(Ordering::Acquire) > 0 {
            self.failures_remaining.fetch_sub(1, Ordering::AcqRel);
            return Err(OutboxError::Reconcile {
                detail: "transient reconcile failure".to_owned(),
            });
        }
        Ok(())
    }
}

/// Poison observability: a reconcile entry that fails on every attempt keeps
/// the pump `Running` (apply stops never fault and never dead-letter), is
/// retried through the same bounded exponential backoff as source failures,
/// and shows up in `health()` as the stuck sequence, the consecutive
/// same-sequence stop count and the last apply error.
#[test]
fn poison_reconcile_entry_backs_off_and_is_visible_in_health() {
    const STOPS: usize = 5;
    let (sink, timestamps) = PoisonReconcileSink::new();
    let pump = start(
        PoisonSource,
        PanickingWakeSink, // never reached: the entry is a reconcile entry
        sink,
        config(Duration::from_millis(5), 16),
    );

    wait_until("consecutive same-sequence stops to accumulate", || {
        pump.health().consecutive_apply_failures >= STOPS
    });
    let health = pump.health();
    assert_eq!(
        health.state,
        PumpState::Running,
        "apply stops must not fault the pump"
    );
    assert_eq!(
        health.consecutive_failures, 0,
        "apply stops are not source failures"
    );
    assert_eq!(health.stuck_sequence, Some(1));
    assert!(
        health
            .last_apply_error
            .as_deref()
            .is_some_and(|error| error.contains("poison reconcile entry")),
        "last_apply_error must carry the root cause: {health:?}"
    );
    assert!(
        health.last_error.is_none(),
        "the source-level error surface stays clean: {health:?}"
    );

    // Backoff: with a 5ms poll interval the minimum wait after the n-th
    // consecutive stop at the same sequence is `5ms * 2^(n-1)` (the shared
    // failure backoff keyed on the consecutive same-sequence count), so the
    // first observed gaps are bounded below by 5/10/20/40ms. Runner
    // scheduling may delay any attempt, so assert each configured lower
    // bound.
    let attempts = lock(&timestamps).clone();
    assert!(attempts.len() >= STOPS, "enough attempts: {attempts:?}");
    let gaps: Vec<Duration> = attempts
        .windows(2)
        .map(|pair| pair[1].saturating_duration_since(pair[0]))
        .collect();
    for (stop, gap) in gaps.iter().take(STOPS.saturating_sub(1)).enumerate() {
        let expected = Duration::from_millis(5 * (1_u64 << stop));
        assert!(
            *gap >= expected,
            "retry after same-sequence stop {} was early: expected at least \
             {expected:?}, got {gap:?}; all={gaps:?}",
            stop + 1,
        );
    }

    pump.stop();
}

/// Poison recovery: once the stuck entry applies, the next completed pass
/// clears the apply-failure health surface while the entry stays durable
/// and is acknowledged exactly once.
#[test]
fn apply_stop_recovery_clears_health_and_acknowledges_the_entry() {
    const TRANSIENT_FAILURES: usize = 3;
    let sink = FlakyReconcileSink::new(TRANSIENT_FAILURES);
    let attempts = Arc::clone(&sink.attempts);
    let acked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pump = start(
        UntilAckedSource {
            entry: reconcile_item(7),
            acked: Arc::clone(&acked),
        },
        PanickingWakeSink, // never reached: the entry is a reconcile entry
        sink,
        config(Duration::from_millis(5), 16),
    );

    wait_until("the same-sequence stops to accumulate", || {
        pump.health().consecutive_apply_failures >= TRANSIENT_FAILURES
    });
    let during = pump.health();
    assert_eq!(during.stuck_sequence, Some(7));
    assert_eq!(during.consecutive_apply_failures, TRANSIENT_FAILURES);
    assert!(
        during
            .last_apply_error
            .as_deref()
            .is_some_and(|error| error.contains("transient reconcile failure")),
        "last_apply_error carries the stop cause: {during:?}"
    );

    wait_until("recovery clears the apply-failure surface", || {
        acked.load(Ordering::Acquire)
            && pump.health().stuck_sequence.is_none()
            && pump.health().consecutive_apply_failures == 0
            && pump.health().last_apply_error.is_none()
    });
    assert!(
        attempts.load(Ordering::Acquire) > TRANSIENT_FAILURES,
        "the entry must have applied after its transient failures"
    );
    let health = pump.health();
    assert_eq!(health.state, PumpState::Running);
    assert_eq!(health.consecutive_failures, 0);

    pump.stop();
}
