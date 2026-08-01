//! Deterministic, single-threaded fake-based tests for `OutboxConsumer`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use nlos_operation::OperationState;
use nlos_outbox::{
    ConsumerConfig, DrainReport, OutboxConsumer, OutboxError, OutboxItem, OutboxKind, OutboxSource,
    ReconcileSink,
};
use nlos_runtime::{FiberHandle, RuntimeError, WakeOutcome, WakeSink};
use nlos_types::{CallbackId, ExecutionFiberId, Generation, OperationId, ReceiptId};

/// Interleaved consumer-boundary events, in call order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    Wake(u64),
    Reconcile(u64),
    Ack(u64),
}

type Shared<T> = Arc<Mutex<T>>;

fn shared<T>(value: T) -> Shared<T> {
    Arc::new(Mutex::new(value))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn operation_id_for(sequence: u64) -> OperationId {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&sequence.to_be_bytes());
    OperationId::from_bytes(bytes)
}

fn sequence_of(operation_id: OperationId) -> u64 {
    u64::from_be_bytes(operation_id.as_bytes()[..8].try_into().expect("16-byte id"))
}

fn item(sequence: u64, kind: OutboxKind) -> OutboxItem {
    OutboxItem {
        sequence,
        kind,
        operation_id: operation_id_for(sequence),
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

struct FakeSource {
    batches: Mutex<VecDeque<Result<Vec<OutboxItem>, OutboxError>>>,
    limits: Mutex<Vec<usize>>,
    failed_acks: Mutex<Vec<u64>>,
    log: Shared<Vec<Event>>,
}

impl OutboxSource for &FakeSource {
    fn pending(&self, limit: usize) -> Result<Vec<OutboxItem>, OutboxError> {
        lock(&self.limits).push(limit);
        lock(&self.batches)
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    fn ack(&self, sequence: u64) -> Result<(), OutboxError> {
        lock(&self.log).push(Event::Ack(sequence));
        if lock(&self.failed_acks).contains(&sequence) {
            return Err(OutboxError::Source {
                detail: "scripted ack failure",
            });
        }
        Ok(())
    }
}

struct FakeWakeSink {
    scripted: Mutex<VecDeque<Result<WakeOutcome, RuntimeError>>>,
    log: Shared<Vec<Event>>,
}

impl WakeSink for &FakeWakeSink {
    fn wake(
        &self,
        _fiber: &FiberHandle,
        operation_id: OperationId,
        _operation_generation: Generation,
    ) -> Result<WakeOutcome, RuntimeError> {
        lock(&self.log).push(Event::Wake(sequence_of(operation_id)));
        lock(&self.scripted)
            .pop_front()
            .unwrap_or(Ok(WakeOutcome::Delivered))
    }
}

struct FakeReconcileSink {
    scripted: Mutex<VecDeque<Result<(), OutboxError>>>,
    log: Shared<Vec<Event>>,
}

impl ReconcileSink for &FakeReconcileSink {
    fn reconcile(&self, item: &OutboxItem) -> Result<(), OutboxError> {
        lock(&self.log).push(Event::Reconcile(item.sequence));
        lock(&self.scripted).pop_front().unwrap_or(Ok(()))
    }
}

struct Harness {
    source: FakeSource,
    wake_sink: FakeWakeSink,
    reconcile_sink: FakeReconcileSink,
    log: Shared<Vec<Event>>,
}

impl Harness {
    fn new() -> Self {
        let log = shared(Vec::new());
        Self {
            source: FakeSource {
                batches: Mutex::new(VecDeque::new()),
                limits: Mutex::new(Vec::new()),
                failed_acks: Mutex::new(Vec::new()),
                log: Arc::clone(&log),
            },
            wake_sink: FakeWakeSink {
                scripted: Mutex::new(VecDeque::new()),
                log: Arc::clone(&log),
            },
            reconcile_sink: FakeReconcileSink {
                scripted: Mutex::new(VecDeque::new()),
                log: Arc::clone(&log),
            },
            log,
        }
    }

    fn script_batch(&self, batch: Result<Vec<OutboxItem>, OutboxError>) {
        lock(&self.source.batches).push_back(batch);
    }

    fn script_wake(&self, outcome: Result<WakeOutcome, RuntimeError>) {
        lock(&self.wake_sink.scripted).push_back(outcome);
    }

    fn fail_ack(&self, sequence: u64) {
        lock(&self.source.failed_acks).push(sequence);
    }

    fn forgive_acks(&self) {
        lock(&self.source.failed_acks).clear();
    }

    fn events(&self) -> Vec<Event> {
        lock(&self.log).clone()
    }

    fn consumer(
        &self,
        batch_limit: usize,
    ) -> OutboxConsumer<&FakeSource, &FakeWakeSink, &FakeReconcileSink> {
        OutboxConsumer {
            source: &self.source,
            wake_sink: &self.wake_sink,
            reconcile_sink: &self.reconcile_sink,
            config: ConsumerConfig { batch_limit },
        }
    }
}

fn wake_batch(sequences: &[u64]) -> Vec<OutboxItem> {
    sequences
        .iter()
        .map(|&sequence| item(sequence, OutboxKind::WakeFiber))
        .collect()
}

#[test]
fn empty_pending_returns_all_zero_report() {
    let harness = Harness::new();
    let report = harness.consumer(8).drain_once().expect("drain succeeds");
    assert_eq!(report, DrainReport::default());
    assert_eq!(harness.events(), Vec::new());
}

#[test]
fn batch_limit_is_forwarded_to_the_source() {
    let harness = Harness::new();
    harness.script_batch(Ok(wake_batch(&[1, 2])));
    let report = harness.consumer(2).drain_once().expect("drain succeeds");
    assert_eq!(lock(&harness.source.limits).as_slice(), &[2]);
    assert_eq!(report.polled, 2);
    assert_eq!(report.acked, 2);
}

#[test]
fn entries_are_applied_then_acked_in_ascending_sequence_order() {
    let harness = Harness::new();
    harness.script_batch(Ok(vec![
        item(1, OutboxKind::WakeFiber),
        item(2, OutboxKind::ReconcileEffect),
        item(3, OutboxKind::WakeFiber),
    ]));
    let report = harness.consumer(8).drain_once().expect("drain succeeds");
    assert_eq!(
        harness.events(),
        vec![
            Event::Wake(1),
            Event::Ack(1),
            Event::Reconcile(2),
            Event::Ack(2),
            Event::Wake(3),
            Event::Ack(3),
        ]
    );
    assert_eq!(
        report,
        DrainReport {
            polled: 3,
            applied: 3,
            acked: 3,
            stopped_at: None,
        }
    );
}

#[test]
fn out_of_order_batch_is_rejected_as_source_contract_violation() {
    let harness = Harness::new();
    harness.script_batch(Ok(wake_batch(&[2, 1])));
    let error = harness.consumer(8).drain_once().expect_err("must reject");
    assert_eq!(
        error,
        OutboxError::Source {
            detail: "pending entries are not in strictly ascending sequence order",
        }
    );
    assert_eq!(harness.events(), Vec::new());
}

#[test]
fn pending_failure_propagates_as_source_error() {
    let harness = Harness::new();
    harness.script_batch(Err(OutboxError::Source {
        detail: "scripted read failure",
    }));
    let error = harness
        .consumer(8)
        .drain_once()
        .expect_err("must propagate");
    assert_eq!(
        error,
        OutboxError::Source {
            detail: "scripted read failure",
        }
    );
}

#[test]
fn transient_wake_error_stops_batch_without_applying_or_acknowledging_rest() {
    let harness = Harness::new();
    harness.script_batch(Ok(wake_batch(&[1, 2, 3])));
    harness.script_wake(Ok(WakeOutcome::Delivered));
    harness.script_wake(Err(RuntimeError::ShuttingDown));
    let report = harness.consumer(8).drain_once().expect("stop is Ok");
    assert_eq!(
        report,
        DrainReport {
            polled: 3,
            applied: 1,
            acked: 1,
            stopped_at: Some(2),
        }
    );
    assert_eq!(
        harness.events(),
        vec![Event::Wake(1), Event::Ack(1), Event::Wake(2)]
    );
}

#[test]
fn permanent_wake_outcomes_are_applied_and_acknowledged() {
    let harness = Harness::new();
    harness.script_batch(Ok(wake_batch(&[1, 2, 3])));
    harness.script_wake(Ok(WakeOutcome::FiberGone));
    harness.script_wake(Ok(WakeOutcome::NotWaiting));
    harness.script_wake(Ok(WakeOutcome::Delivered));
    let report = harness.consumer(8).drain_once().expect("drain succeeds");
    assert_eq!(
        report,
        DrainReport {
            polled: 3,
            applied: 3,
            acked: 3,
            stopped_at: None,
        }
    );
}

#[test]
fn reconcile_effect_goes_only_to_the_reconcile_sink() {
    let harness = Harness::new();
    harness.script_batch(Ok(vec![
        item(1, OutboxKind::ReconcileEffect),
        item(2, OutboxKind::ReconcileEffect),
    ]));
    let report = harness.consumer(8).drain_once().expect("drain succeeds");
    assert_eq!(report.acked, 2);
    assert_eq!(
        harness.events(),
        vec![
            Event::Reconcile(1),
            Event::Ack(1),
            Event::Reconcile(2),
            Event::Ack(2),
        ]
    );
}

#[test]
fn transient_reconcile_error_stops_batch() {
    let harness = Harness::new();
    harness.script_batch(Ok(vec![
        item(1, OutboxKind::ReconcileEffect),
        item(2, OutboxKind::ReconcileEffect),
    ]));
    lock(&harness.reconcile_sink.scripted).push_back(Err(OutboxError::Reconcile {
        detail: "scripted reconcile failure",
    }));
    let report = harness.consumer(8).drain_once().expect("stop is Ok");
    assert_eq!(
        report,
        DrainReport {
            polled: 2,
            applied: 0,
            acked: 0,
            stopped_at: Some(1),
        }
    );
    assert_eq!(harness.events(), vec![Event::Reconcile(1)]);
}

#[test]
fn ack_failure_stops_batch_and_later_entries_are_not_applied() {
    let harness = Harness::new();
    harness.script_batch(Ok(wake_batch(&[1, 2])));
    harness.fail_ack(1);
    let report = harness.consumer(8).drain_once().expect("stop is Ok");
    assert_eq!(
        report,
        DrainReport {
            polled: 2,
            applied: 1,
            acked: 0,
            stopped_at: Some(1),
        }
    );
    // Entry 2 was never applied and never acknowledged.
    assert_eq!(harness.events(), vec![Event::Wake(1), Event::Ack(1)]);

    // Next drain redelivers both entries; the applied-but-unacked entry is
    // applied again (sink idempotency absorbs the duplicate) and then acked.
    harness.forgive_acks();
    harness.script_batch(Ok(wake_batch(&[1, 2])));
    let report = harness.consumer(8).drain_once().expect("drain succeeds");
    assert_eq!(
        report,
        DrainReport {
            polled: 2,
            applied: 2,
            acked: 2,
            stopped_at: None,
        }
    );
    assert_eq!(
        harness.events(),
        vec![
            Event::Wake(1),
            Event::Ack(1),
            Event::Wake(1),
            Event::Ack(1),
            Event::Wake(2),
            Event::Ack(2),
        ]
    );
}

#[test]
fn redelivered_entry_is_applied_again_and_eventually_acknowledged() {
    let harness = Harness::new();
    // Crash window: applied but ACK lost; the source redelivers the entry.
    harness.script_batch(Ok(wake_batch(&[7])));
    harness.fail_ack(7);
    let report = harness.consumer(8).drain_once().expect("stop is Ok");
    assert_eq!(report.applied, 1);
    assert_eq!(report.acked, 0);
    assert_eq!(report.stopped_at, Some(7));

    harness.forgive_acks();
    harness.script_batch(Ok(wake_batch(&[7])));
    let report = harness.consumer(8).drain_once().expect("drain succeeds");
    assert_eq!(report.acked, 1);
    // The consumer did not deduplicate: the sink saw the entry twice.
    assert_eq!(
        harness.events(),
        vec![Event::Wake(7), Event::Ack(7), Event::Wake(7), Event::Ack(7),]
    );
}
