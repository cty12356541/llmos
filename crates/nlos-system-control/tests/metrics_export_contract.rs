use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_schema::sabi::v1::{ControlCommand, GetSystemControlRequest, SabiRequestContext};
use nlos_system_control::{
    RecoveryCounter, RecoveryGauge, RecoveryHealthSource, RecoveryMetricsExportError,
    RecoveryMetricsSink, RecoverySystemControl, SystemControlAuthorizer,
};
use nlos_task::SqliteTaskAuthority;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-system-control-metrics-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open metrics test database")
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
                Err(error) => panic!("remove metrics test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

struct NoopAuthorizer;

impl SystemControlAuthorizer for NoopAuthorizer {
    fn authorize_get(
        &self,
        _context: &SabiRequestContext,
        _request: &GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_submit(
        &self,
        _context: &SabiRequestContext,
        _command: &ControlCommand,
    ) -> Result<(), &'static str> {
        Ok(())
    }
}

#[derive(Clone)]
struct FixedHealth(RecoveryWorkerHealth);

impl RecoveryHealthSource for FixedHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.0.clone()
    }
}

struct FlappingHealth {
    calls: AtomicU64,
}

impl FlappingHealth {
    fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
        }
    }
}

impl RecoveryHealthSource for FlappingHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        let generation = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        RecoveryWorkerHealth {
            state: RecoveryWorkerState::Running,
            completed_cycles: generation,
            total_inspected: 100 + generation,
            total_finalized: 200 + generation,
            consecutive_failed_cycles: usize::try_from(300 + generation)
                .expect("test generation fits usize"),
            retry_delay: Some(Duration::from_millis(400 + generation)),
            last_failures: Vec::new(),
            // The exporter must replace these cache values with the live
            // TaskAuthority summary before handing the snapshot to the sink.
            durable_retrying: 900 + generation,
            durable_escalated: 1_000 + generation,
            durable_unacknowledged_escalated: 1_100 + generation,
            durable_resolved: 1_200 + generation,
        }
    }
}

fn health() -> FixedHealth {
    FixedHealth(RecoveryWorkerHealth {
        state: RecoveryWorkerState::Faulted,
        completed_cycles: 17,
        total_inspected: 29,
        total_finalized: 31,
        consecutive_failed_cycles: 3,
        retry_delay: Some(Duration::from_millis(1_234)),
        last_failures: Vec::new(),
        durable_retrying: 5,
        durable_escalated: 7,
        durable_unacknowledged_escalated: 2,
        durable_resolved: 11,
    })
}

#[derive(Debug, Eq, PartialEq)]
enum Event {
    State(RecoveryWorkerState),
    Counter(RecoveryCounter, u64),
    Gauge(RecoveryGauge, u64),
}

#[derive(Default)]
struct OrderedSink {
    events: Vec<Event>,
}

impl RecoveryMetricsSink for OrderedSink {
    type Error = std::convert::Infallible;

    fn record_worker_state(&mut self, state: RecoveryWorkerState) -> Result<(), Self::Error> {
        self.events.push(Event::State(state));
        Ok(())
    }

    fn set_counter_total(
        &mut self,
        counter: RecoveryCounter,
        value: u64,
    ) -> Result<(), Self::Error> {
        self.events.push(Event::Counter(counter, value));
        Ok(())
    }

    fn set_gauge(&mut self, gauge: RecoveryGauge, value: u64) -> Result<(), Self::Error> {
        self.events.push(Event::Gauge(gauge, value));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureStage {
    Lifecycle,
    Counter,
    Gauge,
}

#[derive(Debug, Eq, PartialEq)]
enum SinkFailure {
    Lifecycle,
    Counter,
    Gauge,
}

struct FailingSink {
    stage: FailureStage,
    events: Vec<Event>,
    lifecycle_attempts: usize,
    counter_attempts: usize,
    gauge_attempts: usize,
}

impl FailingSink {
    fn new(stage: FailureStage) -> Self {
        Self {
            stage,
            events: Vec::new(),
            lifecycle_attempts: 0,
            counter_attempts: 0,
            gauge_attempts: 0,
        }
    }
}

impl RecoveryMetricsSink for FailingSink {
    type Error = SinkFailure;

    fn record_worker_state(&mut self, state: RecoveryWorkerState) -> Result<(), Self::Error> {
        self.lifecycle_attempts += 1;
        if self.stage == FailureStage::Lifecycle {
            return Err(SinkFailure::Lifecycle);
        }
        self.events.push(Event::State(state));
        Ok(())
    }

    fn set_counter_total(
        &mut self,
        counter: RecoveryCounter,
        value: u64,
    ) -> Result<(), Self::Error> {
        self.counter_attempts += 1;
        if self.stage == FailureStage::Counter {
            return Err(SinkFailure::Counter);
        }
        self.events.push(Event::Counter(counter, value));
        Ok(())
    }

    fn set_gauge(&mut self, gauge: RecoveryGauge, value: u64) -> Result<(), Self::Error> {
        self.gauge_attempts += 1;
        if self.stage == FailureStage::Gauge {
            return Err(SinkFailure::Gauge);
        }
        self.events.push(Event::Gauge(gauge, value));
        Ok(())
    }
}

#[test]
fn export_emits_complete_typed_catalog_in_stable_order() {
    let database = TestDatabase::new();
    let authority = database.open();
    let health = health();
    let control = RecoverySystemControl::new(&authority, &health, &NoopAuthorizer);
    let mut sink = OrderedSink::default();

    control
        .export_metrics(&mut sink)
        .expect("metrics export should succeed");

    assert_eq!(
        sink.events,
        vec![
            Event::State(RecoveryWorkerState::Faulted),
            Event::Counter(RecoveryCounter::CompletedCycles, 17),
            Event::Counter(RecoveryCounter::InspectedPlans, 29),
            Event::Counter(RecoveryCounter::FinalizedPlans, 31),
            Event::Gauge(RecoveryGauge::ConsecutiveFailedCycles, 3),
            Event::Gauge(RecoveryGauge::RetryDelayMilliseconds, 1_234),
            Event::Gauge(RecoveryGauge::DurableRetrying, 0),
            Event::Gauge(RecoveryGauge::DurableEscalated, 0),
            Event::Gauge(RecoveryGauge::DurableUnacknowledgedEscalated, 0),
            Event::Gauge(RecoveryGauge::DurableResolved, 0),
        ]
    );
    assert_eq!(
        [
            RecoveryCounter::CompletedCycles.name(),
            RecoveryCounter::InspectedPlans.name(),
            RecoveryCounter::FinalizedPlans.name(),
        ],
        [
            "nlos_artifact_recovery_cycles_total",
            "nlos_artifact_recovery_plans_inspected_total",
            "nlos_artifact_recovery_plans_finalized_total",
        ]
    );
    assert_eq!(
        [
            RecoveryGauge::ConsecutiveFailedCycles.name(),
            RecoveryGauge::RetryDelayMilliseconds.name(),
            RecoveryGauge::DurableRetrying.name(),
            RecoveryGauge::DurableEscalated.name(),
            RecoveryGauge::DurableUnacknowledgedEscalated.name(),
            RecoveryGauge::DurableResolved.name(),
        ],
        [
            "nlos_artifact_recovery_consecutive_failed_cycles",
            "nlos_artifact_recovery_retry_delay_milliseconds",
            "nlos_artifact_recovery_durable_retrying",
            "nlos_artifact_recovery_durable_escalated",
            "nlos_artifact_recovery_durable_unacknowledged_escalated",
            "nlos_artifact_recovery_durable_resolved",
        ]
    );
}

#[test]
fn export_uses_one_health_generation_for_the_complete_catalog() {
    let database = TestDatabase::new();
    let authority = database.open();
    let health = FlappingHealth::new();
    let control = RecoverySystemControl::new(&authority, &health, &NoopAuthorizer);
    let mut sink = OrderedSink::default();

    control
        .export_metrics(&mut sink)
        .expect("metrics export should succeed");

    assert_eq!(health.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        sink.events,
        vec![
            Event::State(RecoveryWorkerState::Running),
            Event::Counter(RecoveryCounter::CompletedCycles, 1),
            Event::Counter(RecoveryCounter::InspectedPlans, 101),
            Event::Counter(RecoveryCounter::FinalizedPlans, 201),
            Event::Gauge(RecoveryGauge::ConsecutiveFailedCycles, 301),
            Event::Gauge(RecoveryGauge::RetryDelayMilliseconds, 401),
            // These four values come from the live empty TaskAuthority, not
            // from the deliberately different worker cache generation.
            Event::Gauge(RecoveryGauge::DurableRetrying, 0),
            Event::Gauge(RecoveryGauge::DurableEscalated, 0),
            Event::Gauge(RecoveryGauge::DurableUnacknowledgedEscalated, 0),
            Event::Gauge(RecoveryGauge::DurableResolved, 0),
        ]
    );
}

#[test]
fn export_stops_at_first_sink_failure_for_every_sink_stage() {
    let database = TestDatabase::new();
    let authority = database.open();
    let health = health();
    let control = RecoverySystemControl::new(&authority, &health, &NoopAuthorizer);

    for (stage, expected_error, expected_events, expected_attempts) in [
        (
            FailureStage::Lifecycle,
            SinkFailure::Lifecycle,
            Vec::new(),
            (1, 0, 0),
        ),
        (
            FailureStage::Counter,
            SinkFailure::Counter,
            vec![Event::State(RecoveryWorkerState::Faulted)],
            (1, 1, 0),
        ),
        (
            FailureStage::Gauge,
            SinkFailure::Gauge,
            vec![
                Event::State(RecoveryWorkerState::Faulted),
                Event::Counter(RecoveryCounter::CompletedCycles, 17),
                Event::Counter(RecoveryCounter::InspectedPlans, 29),
                Event::Counter(RecoveryCounter::FinalizedPlans, 31),
            ],
            (1, 3, 1),
        ),
    ] {
        let mut sink = FailingSink::new(stage);
        let error = control
            .export_metrics(&mut sink)
            .expect_err("first sink failure must be surfaced");

        match error {
            RecoveryMetricsExportError::Sink(actual) => assert_eq!(actual, expected_error),
            RecoveryMetricsExportError::Task(_) => {
                panic!("sink failure must not be reported as a TaskAuthority failure")
            }
        }
        assert_eq!(sink.events, expected_events);
        assert_eq!(
            (
                sink.lifecycle_attempts,
                sink.counter_attempts,
                sink.gauge_attempts,
            ),
            expected_attempts
        );
    }
}
