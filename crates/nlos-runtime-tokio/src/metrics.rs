//! Minimal `OpenMetrics` text exposition for the lifecycle meter aggregate.
//!
//! Pure in-memory rendering ([`LifecycleMeterAggregate`] → `String`) in the
//! Prometheus 0.0.4 / `OpenMetrics` text exposition format. Deliberately a
//! prefix: no scrape endpoint, auth, retention, labels, timestamps,
//! exemplars, or the `OpenMetrics`-only `# EOF` terminator (omitting it keeps
//! the body valid for Prometheus 0.0.4 parsers; `OpenMetrics` parsers accept
//! the same text body).

use crate::LifecycleMeterAggregate;

impl LifecycleMeterAggregate {
    /// Renders this aggregate as `OpenMetrics` / Prometheus text exposition.
    ///
    /// Pure function: no I/O, no global state; deterministic output for a
    /// given input. Durations convert to floating-point seconds via
    /// [`std::time::Duration::as_secs_f64`] and render with Rust's shortest
    /// round-trip float formatting (never trailing-zero padded, never
    /// scientific notation; e.g. `1_500_000_000 ns` → `1.5`, `42 ns` →
    /// `0.000000042`).
    ///
    /// | Aggregate field           | Metric name                                  | Type    |
    /// |---------------------------|----------------------------------------------|---------|
    /// | `total_backpressure_wait` | `nlos_fiber_backpressure_wait_seconds_total` | counter |
    /// | `total_suspended`         | `nlos_fiber_suspended_seconds_total`         | counter |
    /// | `sampled_fibers`          | `nlos_fiber_sampled`                         | gauge   |
    ///
    /// Each metric emits `# HELP` then `# TYPE` then one unlabeled sample
    /// line; the output ends with a trailing newline.
    #[must_use]
    pub fn to_open_metrics_text(&self) -> String {
        let backpressure_seconds = self.total_backpressure_wait.as_secs_f64();
        let suspended_seconds = self.total_suspended.as_secs_f64();
        let sampled_fibers = self.sampled_fibers;
        format!(
            "# HELP nlos_fiber_backpressure_wait_seconds_total Cumulative time live fibers spent in scheduler/admission backpressure wait.\n\
             # TYPE nlos_fiber_backpressure_wait_seconds_total counter\n\
             nlos_fiber_backpressure_wait_seconds_total {backpressure_seconds}\n\
             # HELP nlos_fiber_suspended_seconds_total Cumulative time live fibers spent cooperatively suspended.\n\
             # TYPE nlos_fiber_suspended_seconds_total counter\n\
             nlos_fiber_suspended_seconds_total {suspended_seconds}\n\
             # HELP nlos_fiber_sampled Number of live fibers sampled by the latest lifecycle meter aggregate inspect.\n\
             # TYPE nlos_fiber_sampled gauge\n\
             nlos_fiber_sampled {sampled_fibers}\n"
        )
    }
}
