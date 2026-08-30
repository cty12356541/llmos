//! Deterministic OpenMetrics text renderer for the recovery metrics catalog.
//!
//! This is the minimal concrete prefix of the B-TASK-006M open item
//! "concrete OpenMetrics/ETW adapter": an [`OpenMetricsRenderer`] that
//! implements the backend-neutral [`RecoveryMetricsSink`] and renders the
//! recorded catalog into Prometheus/OpenMetrics text exposition
//! (`text/plain; version=0.0.4`, see [`CONTENT_TYPE`]).
//!
//! Contract:
//!
//! - **Deterministic.** For one recorded snapshot, [`OpenMetricsRenderer::render`]
//!   is byte-for-byte stable across calls, processes, and platforms. Rendered
//!   family order follows the catalog canonical order (lifecycle, counters in
//!   declaration order, gauges in declaration order) regardless of the order
//!   in which the sink methods were called; re-recording a metric overwrites
//!   its value (last write wins).
//! - **Whitelist names.** Counter and gauge metric names come exclusively from
//!   [`RecoveryCounter::name`] and [`RecoveryGauge::name`]; the lifecycle
//!   family is the single fixed [`WORKER_STATE_METRIC`] registration of this
//!   module. No free string is ever concatenated into a name field.
//! - **Fail-closed label admission.** The only label this renderer can emit is
//!   the lifecycle [`WORKER_STATE_LABEL`], whose values are derived from the
//!   closed [`RecoveryWorkerState`] enum. Every candidate label value passes
//!   [`validate_label_value`] at record time; a value containing a quote,
//!   backslash, or control character (including newline) is rejected and the
//!   snapshot is not recorded. Sample values are `u64` integers rendered as
//!   decimal text, so no value escaping exists or is needed.
//! - **Sink-agnostic.** The renderer changes nothing about the sink interface,
//!   catalog semantics, or snapshot semantics; it is one pure consumer:
//!
//!   ```
//!   use nlos_system_control::openmetrics::OpenMetricsRenderer;
//!   # use nlos_commit_coordinator::RecoveryWorkerState;
//!   # use nlos_system_control::{RecoveryCounter, RecoveryMetricsSink, RecoveryGauge};
//!   let mut renderer = OpenMetricsRenderer::new();
//!   renderer.record_worker_state(RecoveryWorkerState::Running).unwrap();
//!   renderer.set_counter_total(RecoveryCounter::CompletedCycles, 17).unwrap();
//!   assert!(renderer.render().contains("nlos_artifact_recovery_cycles_total 17"));
//!   ```
//!
//! Not included (remaining B-TASK-006M scope): an HTTP/scrape endpoint, ETW
//! or signpost adapters, scrape authentication, and retention/alert rules.

use std::error::Error;
use std::fmt;
use std::fmt::Write as _;

use nlos_commit_coordinator::RecoveryWorkerState;

use crate::{RecoveryCounter, RecoveryGauge, RecoveryMetricsSink};

/// Media type of the text produced by [`OpenMetricsRenderer::render`].
///
/// This is the Prometheus/OpenMetrics text exposition format historically
/// served as `text/plain; version=0.0.4`; it carries no scraping transport,
/// which remains future scope.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// Metric family that renders the worker lifecycle with the state-machine
/// pattern: one gauge sample per lifecycle state, `1` for the recorded state
/// and `0` for every other state.
pub const WORKER_STATE_METRIC: &str = "nlos_artifact_recovery_worker_state";

/// Label carrying the lifecycle state on [`WORKER_STATE_METRIC`] samples.
pub const WORKER_STATE_LABEL: &str = "state";

/// Every lifecycle state with its label value, in catalog declaration order.
/// Label values are closed-enum constants, never caller-supplied strings.
const WORKER_STATE_LABELS: [(RecoveryWorkerState, &str); 5] = [
    (RecoveryWorkerState::Starting, "starting"),
    (RecoveryWorkerState::Running, "running"),
    (RecoveryWorkerState::BackingOff, "backing_off"),
    (RecoveryWorkerState::Faulted, "faulted"),
    (RecoveryWorkerState::Stopped, "stopped"),
];

/// Canonical render order for counters, mirroring `export_metrics`.
const COUNTER_ORDER: [RecoveryCounter; 3] = [
    RecoveryCounter::CompletedCycles,
    RecoveryCounter::InspectedPlans,
    RecoveryCounter::FinalizedPlans,
];

/// Canonical render order for gauges, mirroring `export_metrics`.
const GAUGE_ORDER: [RecoveryGauge; 6] = [
    RecoveryGauge::ConsecutiveFailedCycles,
    RecoveryGauge::RetryDelayMilliseconds,
    RecoveryGauge::DurableRetrying,
    RecoveryGauge::DurableEscalated,
    RecoveryGauge::DurableUnacknowledgedEscalated,
    RecoveryGauge::DurableResolved,
];

const fn counter_slot(counter: RecoveryCounter) -> usize {
    match counter {
        RecoveryCounter::CompletedCycles => 0,
        RecoveryCounter::InspectedPlans => 1,
        RecoveryCounter::FinalizedPlans => 2,
    }
}

const fn gauge_slot(gauge: RecoveryGauge) -> usize {
    match gauge {
        RecoveryGauge::ConsecutiveFailedCycles => 0,
        RecoveryGauge::RetryDelayMilliseconds => 1,
        RecoveryGauge::DurableRetrying => 2,
        RecoveryGauge::DurableEscalated => 3,
        RecoveryGauge::DurableUnacknowledgedEscalated => 4,
        RecoveryGauge::DurableResolved => 5,
    }
}

/// Fail-closed rejection of a label value that `OpenMetrics` text exposition
/// cannot carry without escaping. This renderer never escapes: a value
/// containing a double quote, a backslash, or any control character
/// (including `\n` and `\r`) is refused with the offending character.
/// Non-control Unicode and the empty value are legal and accepted.
///
/// # Errors
///
/// Returns the first forbidden character in `value`.
pub fn validate_label_value(value: &str) -> Result<(), char> {
    value
        .chars()
        .find(|candidate| *candidate == '"' || *candidate == '\\' || candidate.is_control())
        .map_or(Ok(()), Err)
}

/// Failure to admit one recorded metric into the renderer. The renderer never
/// renders partially admitted state, so this can only surface through the
/// [`RecoveryMetricsSink`] error channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenMetricsRenderError {
    /// A label value was refused by [`validate_label_value`]. The renderer
    /// state is unchanged (fail-closed: nothing is recorded).
    InvalidLabelValue {
        /// Metric family whose label admission failed.
        metric: &'static str,
        /// Label name whose value was refused.
        label: &'static str,
        /// First forbidden character found in the label value.
        character: char,
    },
}

impl fmt::Display for OpenMetricsRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self::InvalidLabelValue {
            metric,
            label,
            character,
        } = *self;
        write!(
            formatter,
            "OpenMetrics export refused {metric} label {label}: \
             value contains forbidden character {character:?} (fail-closed)"
        )
    }
}

impl Error for OpenMetricsRenderError {}

/// A [`RecoveryMetricsSink`] that accumulates one metrics snapshot and
/// renders it as deterministic `OpenMetrics` text exposition. See the module
/// documentation for the full contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenMetricsRenderer {
    worker_state: Option<RecoveryWorkerState>,
    counters: [Option<u64>; 3],
    gauges: [Option<u64>; 6],
}

impl OpenMetricsRenderer {
    /// Creates an empty renderer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            worker_state: None,
            counters: [None; 3],
            gauges: [None; 6],
        }
    }

    /// Returns whether nothing has been recorded yet. An empty renderer
    /// renders the empty document (zero bytes), which is a valid text
    /// exposition of an empty snapshot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.worker_state.is_none()
            && self.counters.iter().all(Option::is_none)
            && self.gauges.iter().all(Option::is_none)
    }

    /// Renders the recorded snapshot deterministically: canonical catalog
    /// family order (lifecycle, then counters, then gauges), each family
    /// introduced by one `# TYPE` line, `u64` values as decimal integers,
    /// LF line endings, and one trailing newline per line. Calling this
    /// twice on an unchanged renderer yields byte-identical output.
    ///
    /// The lifecycle family renders every state with `0`/`1` so exactly one
    /// sample carries the recorded state. Families never recorded are
    /// omitted entirely. `String` writes are infallible, so a failed write
    /// panics instead of silently producing partial exposition.
    #[must_use]
    pub fn render(&self) -> String {
        // Every name and label emitted here was admitted through the
        // fail-closed gate before recording.
        let mut out = String::new();
        if let Some(active) = self.worker_state {
            writeln!(out, "# TYPE {WORKER_STATE_METRIC} gauge").expect("infallible String write");
            for (candidate, label) in WORKER_STATE_LABELS {
                let value = u64::from(candidate == active);
                writeln!(
                    out,
                    "{WORKER_STATE_METRIC}{{{WORKER_STATE_LABEL}=\"{label}\"}} {value}"
                )
                .expect("infallible String write");
            }
        }
        for counter in COUNTER_ORDER {
            if let Some(value) = self.counters[counter_slot(counter)] {
                let name = counter.name();
                writeln!(out, "# TYPE {name} counter").expect("infallible String write");
                writeln!(out, "{name} {value}").expect("infallible String write");
            }
        }
        for gauge in GAUGE_ORDER {
            if let Some(value) = self.gauges[gauge_slot(gauge)] {
                let name = gauge.name();
                writeln!(out, "# TYPE {name} gauge").expect("infallible String write");
                writeln!(out, "{name} {value}").expect("infallible String write");
            }
        }
        out
    }
}

impl RecoveryMetricsSink for OpenMetricsRenderer {
    type Error = OpenMetricsRenderError;

    /// Records the worker lifecycle. Before recording, every label value this
    /// renderer can ever emit is re-validated fail-closed, so `render` only
    /// ever sees admitted strings.
    ///
    /// # Errors
    ///
    /// Returns [`OpenMetricsRenderError::InvalidLabelValue`] if any lifecycle
    /// label value contains a forbidden character; the renderer state is then
    /// unchanged.
    fn record_worker_state(&mut self, state: RecoveryWorkerState) -> Result<(), Self::Error> {
        for (_, label) in WORKER_STATE_LABELS {
            if let Err(character) = validate_label_value(label) {
                return Err(OpenMetricsRenderError::InvalidLabelValue {
                    metric: WORKER_STATE_METRIC,
                    label,
                    character,
                });
            }
        }
        self.worker_state = Some(state);
        Ok(())
    }

    /// Sets one monotonic counter to its authoritative total. Re-recording a
    /// counter overwrites the previous value.
    ///
    /// # Errors
    ///
    /// Never fails: `u64` values render as decimal text without escaping.
    fn set_counter_total(
        &mut self,
        counter: RecoveryCounter,
        value: u64,
    ) -> Result<(), Self::Error> {
        self.counters[counter_slot(counter)] = Some(value);
        Ok(())
    }

    /// Sets one point-in-time gauge. Re-recording a gauge overwrites the
    /// previous value.
    ///
    /// # Errors
    ///
    /// Never fails: `u64` values render as decimal text without escaping.
    fn set_gauge(&mut self, gauge: RecoveryGauge, value: u64) -> Result<(), Self::Error> {
        self.gauges[gauge_slot(gauge)] = Some(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_admission_is_fail_closed() {
        assert_eq!(validate_label_value("running"), Ok(()));
        assert_eq!(validate_label_value(""), Ok(()));
        assert_eq!(validate_label_value("a b / état-실행"), Ok(()));
        assert_eq!(validate_label_value("quo\"te"), Err('"'));
        assert_eq!(validate_label_value("back\\slash"), Err('\\'));
        assert_eq!(validate_label_value("new\nline"), Err('\n'));
        assert_eq!(validate_label_value("carriage\rreturn"), Err('\r'));
        assert_eq!(validate_label_value("tab\tstop"), Err('\t'));
        assert_eq!(validate_label_value("null\0byte"), Err('\0'));
    }

    #[test]
    fn lifecycle_label_table_covers_every_variant_in_declaration_order() {
        let table = [
            (RecoveryWorkerState::Starting, "starting"),
            (RecoveryWorkerState::Running, "running"),
            (RecoveryWorkerState::BackingOff, "backing_off"),
            (RecoveryWorkerState::Faulted, "faulted"),
            (RecoveryWorkerState::Stopped, "stopped"),
        ];
        assert_eq!(table, WORKER_STATE_LABELS);
        for (_, label) in WORKER_STATE_LABELS {
            assert_eq!(validate_label_value(label), Ok(()));
        }
    }

    #[test]
    fn catalog_slots_are_bijections_in_canonical_order() {
        for (slot, counter) in COUNTER_ORDER.iter().enumerate() {
            assert_eq!(counter_slot(*counter), slot);
        }
        for (slot, gauge) in GAUGE_ORDER.iter().enumerate() {
            assert_eq!(gauge_slot(*gauge), slot);
        }
    }
}
