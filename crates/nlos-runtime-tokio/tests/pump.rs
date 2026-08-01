//! Pump-thread failure semantics: health observability, bounded exponential
//! backoff, faulting after too many consecutive failures, and panic
//! containment. These tests use scripted in-memory fakes only — no `SQLite`
//! store and no Tokio runtime — so timing assertions stay deterministic.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nlos_operation::OperationState;
use nlos_outbox::{
    ConsumerConfig, OutboxConsumer, OutboxError, OutboxItem, OutboxKind, OutboxSource,
    ReconcileSink,
};
use nlos_runtime::{FiberHandle, RuntimeError, WakeOutcome, WakeSink};
use nlos_runtime_tokio::{OutboxPump, PumpConfig, PumpState, RecordingReconcileSink};
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
}

fn config(poll_interval: Duration, failure_threshold: usize) -> PumpConfig {
    PumpConfig {
        poll_interval,
        failure_threshold,
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
