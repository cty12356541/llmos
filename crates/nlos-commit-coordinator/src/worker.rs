use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_artifact::ArtifactStore;
use nlos_task::{
    ArtifactCommitPlanId, ArtifactRecoveryFailureRequest, ArtifactRecoveryFailureSource,
    ArtifactRecoveryState, SqliteTaskAuthority,
};

use crate::{ArtifactCommitCoordinator, CoordinatorError};

/// One domain half's result for a single worker cycle. The artifact half
/// fills the durable gauges from the artifact recovery summary; the semantic
/// half fills its own once the semantic scan is wired in a follow-up lane.
struct DomainCycleOutcome {
    inspected: usize,
    finalized: usize,
    failures: Vec<RecoveryWorkerFailure>,
    retry_delay: Option<Duration>,
    infrastructure_failure: bool,
    durable_retrying: u64,
    durable_escalated: u64,
    durable_unacknowledged_escalated: u64,
    durable_resolved: u64,
}

impl DomainCycleOutcome {
    /// Quiescent outcome: nothing inspected, nothing failed, no durable
    /// gauges. Represents the unwired semantic stub and any domain half
    /// skipped after its per-domain fault bit was set.
    fn empty() -> Self {
        Self {
            inspected: 0,
            finalized: 0,
            failures: Vec::new(),
            retry_delay: None,
            infrastructure_failure: false,
            durable_retrying: 0,
            durable_escalated: 0,
            durable_unacknowledged_escalated: 0,
            durable_resolved: 0,
        }
    }

    /// Worker-level infrastructure failure that prevented the half from
    /// running at all (for example an unreadable system clock).
    fn worker_infrastructure_failure(message: String) -> Self {
        Self {
            infrastructure_failure: true,
            failures: vec![RecoveryWorkerFailure {
                plan_id: None,
                authority: RecoveryFailureAuthority::Worker,
                message,
            }],
            ..Self::empty()
        }
    }
}

/// Lifecycle tuning for the TaskAuthority-owned commit recovery worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryWorkerConfig {
    /// Maximum pending plans captured in one stable scan snapshot.
    pub scan_limit: usize,
    /// Delay after a completely successful scan, including an empty scan.
    pub poll_interval: Duration,
    /// Maximum delay after consecutive failed scans.
    pub max_backoff: Duration,
    /// Consecutive cycles containing a scan or plan failure before the worker
    /// faults and requires its `TaskAuthority` service owner to restart it.
    pub failure_threshold: usize,
}

impl Default for RecoveryWorkerConfig {
    fn default() -> Self {
        Self {
            scan_limit: 64,
            poll_interval: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
            failure_threshold: 8,
        }
    }
}

/// Observable worker lifecycle. `Faulted` and `Stopped` are terminal for one
/// worker instance; durable plans remain available to a newly started worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryWorkerState {
    Starting,
    Running,
    BackingOff,
    Faulted,
    Stopped,
}

/// Authority that produced a recovery failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryFailureAuthority {
    Task,
    Artifact,
    Coordinator,
    Worker,
}

/// Health-safe failure summary. Plan identity and authority source remain
/// typed; the local diagnostic text is not an external SABI contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryWorkerFailure {
    pub plan_id: Option<ArtifactCommitPlanId>,
    pub authority: RecoveryFailureAuthority,
    pub message: String,
}

/// Read-only snapshot for `TaskAuthority` service health and supervision.
///
/// The pre-existing counter/gauge fields describe the artifact domain; the
/// `semantic_*` fields mirror them for the semantic domain and stay zero
/// until the semantic half of the cycle is wired in a follow-up lane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryWorkerHealth {
    pub state: RecoveryWorkerState,
    pub completed_cycles: u64,
    pub total_inspected: u64,
    pub total_finalized: u64,
    pub consecutive_failed_cycles: usize,
    pub retry_delay: Option<Duration>,
    pub last_failures: Vec<RecoveryWorkerFailure>,
    pub durable_retrying: u64,
    pub durable_escalated: u64,
    pub durable_unacknowledged_escalated: u64,
    pub durable_resolved: u64,
    /// Semantic-domain durable recovery gauges, mirroring the artifact
    /// `durable_*` fields above.
    pub semantic_durable_retrying: u64,
    pub semantic_durable_escalated: u64,
    pub semantic_durable_unacknowledged_escalated: u64,
    pub semantic_durable_resolved: u64,
    /// Semantic-domain consecutive infrastructure-failed cycles, mirroring
    /// `consecutive_failed_cycles` for the semantic half of the cycle.
    pub semantic_consecutive_failed_cycles: usize,
    pub semantic_total_inspected: u64,
    pub semantic_total_finalized: u64,
    /// Per-domain fault bits. `true` stops scanning that domain only;
    /// `RecoveryWorkerState::Faulted` remains the thread-level terminal
    /// state. A faulted semantic domain leaves the artifact half running;
    /// the artifact bit is set on the same transition that faults the
    /// thread, because an exhausted artifact-domain failure budget has
    /// always terminated the worker.
    pub semantic_domain_faulted: bool,
    pub artifact_domain_faulted: bool,
}

impl Default for RecoveryWorkerHealth {
    fn default() -> Self {
        Self {
            state: RecoveryWorkerState::Starting,
            completed_cycles: 0,
            total_inspected: 0,
            total_finalized: 0,
            consecutive_failed_cycles: 0,
            retry_delay: None,
            last_failures: Vec::new(),
            durable_retrying: 0,
            durable_escalated: 0,
            durable_unacknowledged_escalated: 0,
            durable_resolved: 0,
            semantic_durable_retrying: 0,
            semantic_durable_escalated: 0,
            semantic_durable_unacknowledged_escalated: 0,
            semantic_durable_resolved: 0,
            semantic_consecutive_failed_cycles: 0,
            semantic_total_inspected: 0,
            semantic_total_finalized: 0,
            semantic_domain_faulted: false,
            artifact_domain_faulted: false,
        }
    }
}

/// Failure to create the worker. No durable state has been changed solely by
/// constructing the handle.
#[derive(Debug)]
pub enum RecoveryWorkerStartError {
    InvalidConfig(&'static str),
    Spawn(std::io::Error),
}

impl fmt::Display for RecoveryWorkerStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(reason) => write!(formatter, "invalid recovery config: {reason}"),
            Self::Spawn(error) => write!(formatter, "could not spawn recovery worker: {error}"),
        }
    }
}

impl Error for RecoveryWorkerStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(_) => None,
            Self::Spawn(error) => Some(error),
        }
    }
}

/// TaskAuthority-owned lifecycle handle for Artifact commit recovery.
///
/// The worker owns no canonical state. Its dedicated thread opens no third
/// store: it drives the supplied `TaskAuthority` and `ArtifactAuthority` and can
/// always be replaced after a crash from their durable prefix.
pub struct TaskAuthorityCommitRecoveryWorker {
    stop_tx: SyncSender<()>,
    join: Option<JoinHandle<()>>,
    health: Arc<Mutex<RecoveryWorkerHealth>>,
}

impl TaskAuthorityCommitRecoveryWorker {
    /// Starts a dedicated worker. The first bounded pending scan runs
    /// immediately; `poll_interval` applies only after that scan.
    ///
    /// # Errors
    ///
    /// Returns before spawning for an invalid config, or when the OS cannot
    /// create the worker thread.
    pub fn start(
        tasks: Arc<SqliteTaskAuthority>,
        artifacts: Arc<ArtifactStore>,
        config: RecoveryWorkerConfig,
    ) -> Result<Self, RecoveryWorkerStartError> {
        validate_config(config)?;
        let (stop_tx, stop_rx) = sync_channel(1);
        let health = Arc::new(Mutex::new(RecoveryWorkerHealth::default()));
        let thread_health = Arc::clone(&health);
        let join = thread::Builder::new()
            .name("task-authority-commit-recovery".to_string())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run_worker(&tasks, &artifacts, config, &stop_rx, &thread_health);
                }));
                if outcome.is_err() {
                    let mut current = lock(&thread_health);
                    current.state = RecoveryWorkerState::Faulted;
                    current.retry_delay = None;
                    current.last_failures = vec![RecoveryWorkerFailure {
                        plan_id: None,
                        authority: RecoveryFailureAuthority::Worker,
                        message: "recovery worker panicked".to_string(),
                    }];
                }
            })
            .map_err(RecoveryWorkerStartError::Spawn)?;
        Ok(Self {
            stop_tx,
            join: Some(join),
            health,
        })
    }

    #[must_use]
    pub fn health(&self) -> RecoveryWorkerHealth {
        lock(&self.health).clone()
    }

    /// Requests shutdown and joins the dedicated thread. Repeated calls are
    /// harmless.
    pub fn stop(&mut self) {
        let _ = self.stop_tx.try_send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for TaskAuthorityCommitRecoveryWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate_config(config: RecoveryWorkerConfig) -> Result<(), RecoveryWorkerStartError> {
    if config.scan_limit == 0 {
        return Err(RecoveryWorkerStartError::InvalidConfig(
            "scan_limit must be non-zero",
        ));
    }
    if config.poll_interval.is_zero() {
        return Err(RecoveryWorkerStartError::InvalidConfig(
            "poll_interval must be non-zero",
        ));
    }
    if config.max_backoff < config.poll_interval {
        return Err(RecoveryWorkerStartError::InvalidConfig(
            "max_backoff must be at least poll_interval",
        ));
    }
    if config.failure_threshold == 0 {
        return Err(RecoveryWorkerStartError::InvalidConfig(
            "failure_threshold must be non-zero",
        ));
    }
    Ok(())
}

fn run_worker(
    tasks: &SqliteTaskAuthority,
    artifacts: &ArtifactStore,
    config: RecoveryWorkerConfig,
    stop_rx: &Receiver<()>,
    health: &Mutex<RecoveryWorkerHealth>,
) {
    lock(health).state = RecoveryWorkerState::Running;
    loop {
        let (artifact, semantic) = match now_ms() {
            Ok(timestamp) => {
                // One cycle drives the artifact half and then the semantic
                // half. A faulted semantic domain is skipped while the
                // artifact half keeps scanning; an artifact-domain fault is
                // terminal for the whole thread (the pre-existing Faulted
                // transition), so the artifact half needs no skip check.
                let artifact = artifact_cycle(tasks, artifacts, config, timestamp);
                let semantic = if lock(health).semantic_domain_faulted {
                    DomainCycleOutcome::empty()
                } else {
                    semantic_cycle_stub(tasks, config, timestamp)
                };
                (artifact, semantic)
            }
            // An unusable clock blocks both halves before either can run.
            // It is accounted against the artifact-domain failure budget —
            // the budget that has always terminated the worker thread — and
            // surfaces as exactly one Worker-authority failure.
            Err(message) => (
                DomainCycleOutcome::worker_infrastructure_failure(message),
                DomainCycleOutcome::empty(),
            ),
        };

        let Some(delay) = account_cycle(config, health, artifact, semantic) else {
            return;
        };

        match stop_rx.recv_timeout(delay) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let mut current = lock(health);
                current.state = RecoveryWorkerState::Stopped;
                current.retry_delay = None;
                return;
            }
        }
    }
}

/// Aggregates both domain halves into one health update and returns the
/// delay before the next cycle, or `None` when the worker thread faulted
/// terminally.
fn account_cycle(
    config: RecoveryWorkerConfig,
    health: &Mutex<RecoveryWorkerHealth>,
    artifact: DomainCycleOutcome,
    semantic: DomainCycleOutcome,
) -> Option<Duration> {
    let artifact_infra = artifact.infrastructure_failure;
    let semantic_infra = semantic.infrastructure_failure;
    let mut failures = artifact.failures;
    failures.extend(semantic.failures);
    let plan_retry_delay = min_option_duration(artifact.retry_delay, semantic.retry_delay);

    let mut current = lock(health);
    current.completed_cycles = current.completed_cycles.saturating_add(1);
    current.total_inspected = current
        .total_inspected
        .saturating_add(u64::try_from(artifact.inspected).unwrap_or(u64::MAX));
    current.total_finalized = current
        .total_finalized
        .saturating_add(u64::try_from(artifact.finalized).unwrap_or(u64::MAX));
    current.durable_retrying = artifact.durable_retrying;
    current.durable_escalated = artifact.durable_escalated;
    current.durable_unacknowledged_escalated = artifact.durable_unacknowledged_escalated;
    current.durable_resolved = artifact.durable_resolved;
    current.semantic_total_inspected = current
        .semantic_total_inspected
        .saturating_add(u64::try_from(semantic.inspected).unwrap_or(u64::MAX));
    current.semantic_total_finalized = current
        .semantic_total_finalized
        .saturating_add(u64::try_from(semantic.finalized).unwrap_or(u64::MAX));
    current.semantic_durable_retrying = semantic.durable_retrying;
    current.semantic_durable_escalated = semantic.durable_escalated;
    current.semantic_durable_unacknowledged_escalated = semantic.durable_unacknowledged_escalated;
    current.semantic_durable_resolved = semantic.durable_resolved;

    // Per-domain consecutive infrastructure-failure accounting. The existing
    // `consecutive_failed_cycles` field is the artifact-domain counter; the
    // semantic domain counts independently against the same threshold. A
    // skipped (faulted) semantic domain neither counts nor resets.
    if artifact_infra {
        current.consecutive_failed_cycles = current.consecutive_failed_cycles.saturating_add(1);
    } else {
        current.consecutive_failed_cycles = 0;
    }
    if semantic_infra {
        current.semantic_consecutive_failed_cycles =
            current.semantic_consecutive_failed_cycles.saturating_add(1);
        if current.semantic_consecutive_failed_cycles >= config.failure_threshold {
            current.semantic_domain_faulted = true;
        }
    } else if !current.semantic_domain_faulted {
        current.semantic_consecutive_failed_cycles = 0;
    }

    if failures.is_empty() {
        current.retry_delay = None;
        current.last_failures.clear();
        current.state = RecoveryWorkerState::Running;
        return Some(config.poll_interval);
    }
    current.last_failures = failures;
    if !artifact_infra && !semantic_infra {
        // Plan-level failures only: the affected plans retry on their own
        // durable schedule; neither domain's failure budget is consumed.
        current.retry_delay = plan_retry_delay;
        current.state = if plan_retry_delay.is_some() {
            RecoveryWorkerState::BackingOff
        } else {
            RecoveryWorkerState::Running
        };
        return Some(plan_retry_delay.unwrap_or(config.poll_interval));
    }
    // Infrastructure failure. An exhausted artifact-domain budget is the
    // pre-existing thread-level Faulted transition.
    if artifact_infra && current.consecutive_failed_cycles >= config.failure_threshold {
        current.artifact_domain_faulted = true;
        current.state = RecoveryWorkerState::Faulted;
        current.retry_delay = None;
        return None;
    }
    let mut backoff = None;
    if artifact_infra {
        backoff = Some(retry_delay(config, current.consecutive_failed_cycles));
    }
    if semantic_infra && !current.semantic_domain_faulted {
        let semantic_backoff = retry_delay(config, current.semantic_consecutive_failed_cycles);
        backoff = Some(backoff.map_or(semantic_backoff, |current| current.max(semantic_backoff)));
    }
    if let Some(delay) = backoff {
        current.retry_delay = Some(delay);
        current.state = RecoveryWorkerState::BackingOff;
        Some(delay)
    } else {
        // The only infrastructure failures came from a domain that just
        // faulted; it will not be scanned again, so the thread resumes
        // its normal poll cadence.
        current.retry_delay = None;
        current.state = RecoveryWorkerState::Running;
        Some(config.poll_interval)
    }
}

fn min_option_duration(first: Option<Duration>, second: Option<Duration>) -> Option<Duration> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (first, None) => first,
        (None, second) => second,
    }
}

// Keeping one cycle linear makes the external converge result and its
// TaskAuthority ledger CAS visibly adjacent for crash-window review.
#[allow(clippy::too_many_lines)]
fn artifact_cycle(
    tasks: &SqliteTaskAuthority,
    artifacts: &ArtifactStore,
    config: RecoveryWorkerConfig,
    now_ms: i64,
) -> DomainCycleOutcome {
    let plans = match tasks.list_due_artifact_commit_plans(config.scan_limit, now_ms) {
        Ok(plans) => plans,
        Err(error) => {
            return DomainCycleOutcome {
                inspected: 0,
                finalized: 0,
                failures: vec![failure_of(None, &CoordinatorError::Task(error))],
                retry_delay: None,
                infrastructure_failure: true,
                durable_retrying: 0,
                durable_escalated: 0,
                durable_unacknowledged_escalated: 0,
                durable_resolved: 0,
            };
        }
    };
    let mut outcome = DomainCycleOutcome {
        inspected: plans.len(),
        finalized: 0,
        failures: Vec::new(),
        retry_delay: None,
        infrastructure_failure: false,
        durable_retrying: 0,
        durable_escalated: 0,
        durable_unacknowledged_escalated: 0,
        durable_resolved: 0,
    };
    let coordinator = ArtifactCommitCoordinator::new(tasks, artifacts);
    for plan in plans {
        match coordinator.converge(crate::ConvergeArtifactCommitRequest {
            plan_id: plan.plan_id,
            now_ms,
        }) {
            Ok(_) => outcome.finalized += 1,
            Err(error) => {
                outcome
                    .failures
                    .push(failure_of(Some(plan.plan_id), &error));
                let current = match tasks.inspect_artifact_recovery(plan.plan_id) {
                    Ok(record) => record.map_or(0, |record| record.total_failures),
                    Err(ledger_error) => {
                        outcome.infrastructure_failure = true;
                        outcome.failures.push(failure_of(
                            Some(plan.plan_id),
                            &CoordinatorError::Task(ledger_error),
                        ));
                        continue;
                    }
                };
                let source = match error {
                    CoordinatorError::Task(_) => ArtifactRecoveryFailureSource::TaskAuthority,
                    CoordinatorError::Artifact(_) => {
                        ArtifactRecoveryFailureSource::ArtifactAuthority
                    }
                    CoordinatorError::InvalidTimestamp => {
                        ArtifactRecoveryFailureSource::Coordinator
                    }
                    CoordinatorError::Semantic(_) => ArtifactRecoveryFailureSource::Coordinator,
                };
                match tasks.record_artifact_recovery_failure(ArtifactRecoveryFailureRequest {
                    plan_id: plan.plan_id,
                    expected_total_failures: current,
                    source,
                    observed_at_ms: now_ms,
                    base_delay_ms: duration_ms(config.poll_interval),
                    max_delay_ms: duration_ms(config.max_backoff),
                    escalation_threshold: u64::try_from(config.failure_threshold)
                        .unwrap_or(u64::MAX),
                }) {
                    Ok(record) if record.state == ArtifactRecoveryState::Retrying => {
                        let delay_ms = record.next_retry_at_ms.unwrap_or(now_ms) - now_ms;
                        let delay = Duration::from_millis(u64::try_from(delay_ms).unwrap_or(0));
                        outcome.retry_delay = Some(
                            outcome
                                .retry_delay
                                .map_or(delay, |current| current.min(delay)),
                        );
                    }
                    Ok(_) => {}
                    Err(ledger_error) => {
                        outcome.infrastructure_failure = true;
                        outcome.failures.push(failure_of(
                            Some(plan.plan_id),
                            &CoordinatorError::Task(ledger_error),
                        ));
                    }
                }
            }
        }
    }
    match tasks.summarize_artifact_recovery() {
        Ok(summary) => {
            outcome.durable_retrying = summary.retrying;
            outcome.durable_escalated = summary.escalated;
            outcome.durable_unacknowledged_escalated = summary.unacknowledged_escalated;
            outcome.durable_resolved = summary.resolved;
        }
        Err(error) => {
            outcome.infrastructure_failure = true;
            outcome
                .failures
                .push(failure_of(None, &CoordinatorError::Task(error)));
        }
    }
    outcome
}

/// Semantic-domain half of one cycle. The real semantic scan is wired in a
/// follow-up lane; until then this stub returns the quiescent outcome so
/// the semantic health projection stays zeroed and the artifact half's
/// behavior is unchanged.
fn semantic_cycle_stub(
    _tasks: &SqliteTaskAuthority,
    _config: RecoveryWorkerConfig,
    _now_ms: i64,
) -> DomainCycleOutcome {
    DomainCycleOutcome::empty()
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn failure_of(
    plan_id: Option<ArtifactCommitPlanId>,
    error: &CoordinatorError,
) -> RecoveryWorkerFailure {
    let authority = match error {
        CoordinatorError::Task(_) => RecoveryFailureAuthority::Task,
        CoordinatorError::Artifact(_) => RecoveryFailureAuthority::Artifact,
        CoordinatorError::InvalidTimestamp | CoordinatorError::Semantic(_) => {
            RecoveryFailureAuthority::Coordinator
        }
    };
    RecoveryWorkerFailure {
        plan_id,
        authority,
        message: error.to_string(),
    }
}

fn retry_delay(config: RecoveryWorkerConfig, consecutive_failures: usize) -> Duration {
    let exponent = u32::try_from(consecutive_failures.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(31);
    config
        .poll_interval
        .checked_mul(1_u32 << exponent)
        .unwrap_or(config.max_backoff)
        .min(config.max_backoff)
}

fn now_ms() -> Result<i64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock precedes Unix epoch: {error}"))?;
    i64::try_from(elapsed.as_millis()).map_err(|_| "system clock exceeds i64 milliseconds".into())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
