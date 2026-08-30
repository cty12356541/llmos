//! `OpenMetrics` text rendering contract for the recovery metrics catalog
//! (B-TASK-006M renderer prefix): byte determinism, catalog name/order
//! whitelist, fail-closed snapshot admission, and empty-snapshot behavior.
//! The existing catalog/snapshot semantics are consumed, never changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_schema::sabi::v1::{ControlCommand, GetSystemControlRequest, SabiRequestContext};
use nlos_system_control::openmetrics::{
    CONTENT_TYPE, OpenMetricsRenderer, WORKER_STATE_LABEL, WORKER_STATE_METRIC,
};
use nlos_system_control::{
    RecoveryCounter, RecoveryGauge, RecoveryHealthSource, RecoveryMetricsSink,
    RecoverySystemControl, SystemControlAuthorizer,
};
use nlos_task::SqliteTaskAuthority;

/// The fixture mirrors the authoritative catalog contract test: the same
/// Faulted snapshot that `export_metrics` produces against a fresh
/// `TaskAuthority` (durable gauges overridden to zero by the live summary).
const FULL_CATALOG_TEXT: &str = r#"# TYPE nlos_artifact_recovery_worker_state gauge
nlos_artifact_recovery_worker_state{state="starting"} 0
nlos_artifact_recovery_worker_state{state="running"} 0
nlos_artifact_recovery_worker_state{state="backing_off"} 0
nlos_artifact_recovery_worker_state{state="faulted"} 1
nlos_artifact_recovery_worker_state{state="stopped"} 0
# TYPE nlos_artifact_recovery_cycles_total counter
nlos_artifact_recovery_cycles_total 17
# TYPE nlos_artifact_recovery_plans_inspected_total counter
nlos_artifact_recovery_plans_inspected_total 29
# TYPE nlos_artifact_recovery_plans_finalized_total counter
nlos_artifact_recovery_plans_finalized_total 31
# TYPE nlos_artifact_recovery_consecutive_failed_cycles gauge
nlos_artifact_recovery_consecutive_failed_cycles 3
# TYPE nlos_artifact_recovery_retry_delay_milliseconds gauge
nlos_artifact_recovery_retry_delay_milliseconds 1234
# TYPE nlos_artifact_recovery_durable_retrying gauge
nlos_artifact_recovery_durable_retrying 0
# TYPE nlos_artifact_recovery_durable_escalated gauge
nlos_artifact_recovery_durable_escalated 0
# TYPE nlos_artifact_recovery_durable_unacknowledged_escalated gauge
nlos_artifact_recovery_durable_unacknowledged_escalated 0
# TYPE nlos_artifact_recovery_durable_resolved gauge
nlos_artifact_recovery_durable_resolved 0
"#;

fn record_full_catalog(renderer: &mut OpenMetricsRenderer) {
    renderer
        .record_worker_state(RecoveryWorkerState::Faulted)
        .expect("lifecycle label values are closed-enum constants");
    renderer
        .set_counter_total(RecoveryCounter::CompletedCycles, 17)
        .expect("u64 values are always admissible");
    renderer
        .set_counter_total(RecoveryCounter::InspectedPlans, 29)
        .expect("u64 values are always admissible");
    renderer
        .set_counter_total(RecoveryCounter::FinalizedPlans, 31)
        .expect("u64 values are always admissible");
    renderer
        .set_gauge(RecoveryGauge::ConsecutiveFailedCycles, 3)
        .expect("u64 values are always admissible");
    renderer
        .set_gauge(RecoveryGauge::RetryDelayMilliseconds, 1_234)
        .expect("u64 values are always admissible");
    renderer
        .set_gauge(RecoveryGauge::DurableRetrying, 0)
        .expect("u64 values are always admissible");
    renderer
        .set_gauge(RecoveryGauge::DurableEscalated, 0)
        .expect("u64 values are always admissible");
    renderer
        .set_gauge(RecoveryGauge::DurableUnacknowledgedEscalated, 0)
        .expect("u64 values are always admissible");
    renderer
        .set_gauge(RecoveryGauge::DurableResolved, 0)
        .expect("u64 values are always admissible");
}

fn full_catalog_text() -> String {
    let mut renderer = OpenMetricsRenderer::new();
    record_full_catalog(&mut renderer);
    renderer.render()
}

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-system-control-openmetrics-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open OpenMetrics test database")
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
                Err(error) => panic!("remove OpenMetrics test database: {error}"),
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

struct CountingHealth {
    fixture: RecoveryWorkerHealth,
    reads: AtomicU64,
}

impl CountingHealth {
    fn new() -> Self {
        Self {
            fixture: RecoveryWorkerHealth {
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
            },
            reads: AtomicU64::new(0),
        }
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::Acquire)
    }
}

impl RecoveryHealthSource for CountingHealth {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.reads.fetch_add(1, Ordering::AcqRel);
        self.fixture.clone()
    }
}

#[test]
fn full_catalog_renders_byte_deterministic_openmetrics() {
    assert_eq!(CONTENT_TYPE, "text/plain; version=0.0.4");

    let mut renderer = OpenMetricsRenderer::new();
    record_full_catalog(&mut renderer);
    assert!(!renderer.is_empty());

    let first = renderer.render();
    let second = renderer.render();
    assert_eq!(first.as_bytes(), second.as_bytes());
    assert_eq!(first, FULL_CATALOG_TEXT);
    assert_eq!(first, full_catalog_text());
}

#[test]
fn families_use_catalog_names_in_canonical_order() {
    let families = [
        (WORKER_STATE_METRIC, "gauge"),
        (RecoveryCounter::CompletedCycles.name(), "counter"),
        (RecoveryCounter::InspectedPlans.name(), "counter"),
        (RecoveryCounter::FinalizedPlans.name(), "counter"),
        (RecoveryGauge::ConsecutiveFailedCycles.name(), "gauge"),
        (RecoveryGauge::RetryDelayMilliseconds.name(), "gauge"),
        (RecoveryGauge::DurableRetrying.name(), "gauge"),
        (RecoveryGauge::DurableEscalated.name(), "gauge"),
        (
            RecoveryGauge::DurableUnacknowledgedEscalated.name(),
            "gauge",
        ),
        (RecoveryGauge::DurableResolved.name(), "gauge"),
    ];
    let text = full_catalog_text();

    let mut search_from = 0;
    for (name, kind) in families {
        let header = format!("# TYPE {name} {kind}\n");
        let found = text[search_from..]
            .find(&header)
            .unwrap_or_else(|| panic!("missing family header {header:?}"))
            + search_from;
        assert!(
            found >= search_from,
            "family {name} rendered out of canonical order"
        );
        search_from = found + header.len();
    }
    assert_eq!(
        text.matches("# TYPE ").count(),
        families.len(),
        "every recorded family is introduced exactly once"
    );
}

#[test]
fn state_machine_renders_exactly_one_active_lifecycle() {
    for (state, label) in [
        (RecoveryWorkerState::Starting, "starting"),
        (RecoveryWorkerState::Running, "running"),
        (RecoveryWorkerState::BackingOff, "backing_off"),
        (RecoveryWorkerState::Faulted, "faulted"),
        (RecoveryWorkerState::Stopped, "stopped"),
    ] {
        let mut renderer = OpenMetricsRenderer::new();
        renderer
            .record_worker_state(state)
            .expect("lifecycle label values are closed-enum constants");
        let text = renderer.render();

        assert!(text.contains(&format!("# TYPE {WORKER_STATE_METRIC} gauge\n")));
        let active: Vec<&str> = text.lines().filter(|line| line.ends_with(" 1")).collect();
        assert_eq!(
            active,
            vec![&*format!(
                "{WORKER_STATE_METRIC}{{{WORKER_STATE_LABEL}=\"{label}\"}} 1"
            )]
        );
        assert_eq!(text.lines().filter(|line| line.ends_with(" 0")).count(), 4);
    }
}

#[test]
fn record_order_does_not_change_rendered_family_order() {
    let mut reversed = OpenMetricsRenderer::new();
    reversed
        .set_gauge(RecoveryGauge::DurableResolved, 6)
        .expect("u64 values are always admissible");
    reversed
        .set_gauge(RecoveryGauge::DurableUnacknowledgedEscalated, 5)
        .expect("u64 values are always admissible");
    reversed
        .set_gauge(RecoveryGauge::DurableEscalated, 4)
        .expect("u64 values are always admissible");
    reversed
        .set_gauge(RecoveryGauge::DurableRetrying, 3)
        .expect("u64 values are always admissible");
    reversed
        .set_gauge(RecoveryGauge::RetryDelayMilliseconds, 2)
        .expect("u64 values are always admissible");
    reversed
        .set_gauge(RecoveryGauge::ConsecutiveFailedCycles, 1)
        .expect("u64 values are always admissible");

    let text = reversed.render();
    let mut search_from = 0;
    for gauge in [
        RecoveryGauge::ConsecutiveFailedCycles,
        RecoveryGauge::RetryDelayMilliseconds,
        RecoveryGauge::DurableRetrying,
        RecoveryGauge::DurableEscalated,
        RecoveryGauge::DurableUnacknowledgedEscalated,
        RecoveryGauge::DurableResolved,
    ] {
        let header = format!("# TYPE {} gauge\n", gauge.name());
        let found = text[search_from..]
            .find(&header)
            .unwrap_or_else(|| panic!("missing gauge header {header:?}"))
            + search_from;
        search_from = found + header.len();
    }

    let mut overwrite = OpenMetricsRenderer::new();
    overwrite
        .set_counter_total(RecoveryCounter::CompletedCycles, 1)
        .expect("u64 values are always admissible");
    overwrite
        .set_counter_total(RecoveryCounter::CompletedCycles, 9)
        .expect("u64 values are always admissible");
    assert!(
        overwrite
            .render()
            .contains("nlos_artifact_recovery_cycles_total 9")
    );
    assert!(
        !overwrite
            .render()
            .contains("nlos_artifact_recovery_cycles_total 1\n")
    );
}

#[test]
fn empty_renderer_renders_the_empty_document() {
    let renderer = OpenMetricsRenderer::new();
    assert!(renderer.is_empty());
    assert_eq!(renderer.render(), "");
    assert_eq!(renderer.render().as_bytes(), renderer.render().as_bytes());
}

#[test]
fn partial_recording_omits_unrecorded_families() {
    let mut renderer = OpenMetricsRenderer::new();
    renderer
        .set_gauge(RecoveryGauge::DurableResolved, 4)
        .expect("u64 values are always admissible");
    assert_eq!(
        renderer.render(),
        "# TYPE nlos_artifact_recovery_durable_resolved gauge\n\
         nlos_artifact_recovery_durable_resolved 4\n"
    );
}

#[test]
fn export_metrics_feeds_the_renderer_exactly_one_snapshot() {
    let database = TestDatabase::new();
    let authority = database.open();
    let health = CountingHealth::new();
    let control = RecoverySystemControl::new(&authority, &health, &NoopAuthorizer);

    let mut renderer = OpenMetricsRenderer::new();
    control
        .export_metrics(&mut renderer)
        .expect("export_metrics accepts the OpenMetrics renderer sink");

    let text = renderer.render();
    assert_eq!(text, FULL_CATALOG_TEXT);
    assert_eq!(text.as_bytes(), renderer.render().as_bytes());
    assert!(!renderer.is_empty());
    // One render consumed exactly one health generation, preserving the
    // single-snapshot semantics of the neutral exporter.
    assert_eq!(health.reads(), 1);
}
