use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_artifact::ArtifactStore;
use nlos_resource::ResourceAuthority;
use nlos_semantic::SemanticAuthority;
use nlos_task::{
    ArtifactCommitPlanId, ArtifactRecoveryFailureRequest, ArtifactRecoveryFailureSource,
    ArtifactRecoveryState, ResourceConvergeDecision, ResourceRecoveryFailureRequest,
    ResourceRecoveryFailureSource, ResourceRecoveryState, SemanticRecoveryFailureRequest,
    SemanticRecoveryFailureSource, SemanticRecoveryState, SqliteTaskAuthority, TaskStoreError,
};

use crate::{
    ArtifactCommitCoordinator, ConvergeSemanticCommitRequest, CoordinatorError,
    SemanticCommitCoordinator,
};

/// One domain half's result for a single worker cycle. The artifact half
/// fills the durable gauges from the artifact recovery summary; the semantic
/// half fills its own from the semantic recovery summary when the worker was
/// started with a semantic authority.
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
    /// gauges. Represents the semantic half of a worker started without a
    /// semantic authority and any domain half skipped after its per-domain
    /// fault bit was set.
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
/// `semantic_*` fields mirror them for the semantic domain and stay zero for
/// a worker started without a semantic authority.
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
    /// state. A faulted semantic or resource domain leaves the other
    /// halves running; the artifact bit is set on the same transition that
    /// faults the thread, because an exhausted artifact-domain failure
    /// budget has always terminated the worker.
    pub semantic_domain_faulted: bool,
    pub artifact_domain_faulted: bool,
    /// Resource-domain durable recovery gauges, mirroring the semantic
    /// `semantic_durable_*` fields (schema v43 third-domain ledger).
    pub resource_durable_retrying: u64,
    pub resource_durable_escalated: u64,
    pub resource_durable_unacknowledged_escalated: u64,
    pub resource_durable_resolved: u64,
    /// Resource-domain consecutive infrastructure-failed cycles, mirroring
    /// `semantic_consecutive_failed_cycles` for the resource half.
    pub resource_consecutive_failed_cycles: usize,
    pub resource_total_inspected: u64,
    pub resource_total_finalized: u64,
    /// Resource-domain fault bit, sticky and isolated exactly like
    /// `semantic_domain_faulted`.
    pub resource_domain_faulted: bool,
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
            resource_durable_retrying: 0,
            resource_durable_escalated: 0,
            resource_durable_unacknowledged_escalated: 0,
            resource_durable_resolved: 0,
            resource_consecutive_failed_cycles: 0,
            resource_total_inspected: 0,
            resource_total_finalized: 0,
            resource_domain_faulted: false,
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

/// TaskAuthority-owned lifecycle handle for artifact and Semantic commit
/// recovery.
///
/// The worker owns no canonical state. Its dedicated thread opens no third
/// store: it drives the supplied `TaskAuthority` and `ArtifactAuthority`
/// (plus the optional `SemanticAuthority`) and can always be replaced after
/// a crash from their durable prefix.
pub struct TaskAuthorityCommitRecoveryWorker {
    stop_tx: SyncSender<()>,
    join: Option<JoinHandle<()>>,
    health: Arc<Mutex<RecoveryWorkerHealth>>,
}

impl TaskAuthorityCommitRecoveryWorker {
    /// Starts a dedicated worker whose semantic half stays quiescent: no
    /// semantic authority is supplied, so only artifact plans are scanned and
    /// every `semantic_*` health field remains at its zero default. Use
    /// [`Self::start_with_semantic_authority`] to drive Semantic commit
    /// convergence on the same thread.
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
        Self::start_with_semantic_authority(tasks, artifacts, None, config)
    }

    /// Starts a dedicated worker driving both recovery domains: the artifact
    /// half scans `ArtifactCommitPlan`s and the semantic half (when
    /// `semantic` is `Some`) scans due `SemanticCommitPlan`s through the same
    /// `TaskAuthority`. The first bounded pending scan runs immediately;
    /// `poll_interval` applies only after that scan.
    ///
    /// # Errors
    ///
    /// Returns before spawning for an invalid config, or when the OS cannot
    /// create the worker thread.
    pub fn start_with_semantic_authority(
        tasks: Arc<SqliteTaskAuthority>,
        artifacts: Arc<ArtifactStore>,
        semantic: Option<Arc<SemanticAuthority>>,
        config: RecoveryWorkerConfig,
    ) -> Result<Self, RecoveryWorkerStartError> {
        Self::start_with_semantic_and_resource_authorities(tasks, artifacts, semantic, None, config)
    }

    /// Starts a dedicated worker driving all three recovery domains: the
    /// artifact half scans `ArtifactCommitPlan`s, the semantic half (when
    /// `semantic` is `Some`) scans due `SemanticCommitPlan`s, and the
    /// resource half (when `resource` is `Some`) scans due Resource
    /// finalize plans through the same `TaskAuthority`, in that order. The
    /// first bounded pending scan runs immediately; `poll_interval` applies
    /// only after that scan.
    ///
    /// # Errors
    ///
    /// Returns before spawning for an invalid config, or when the OS cannot
    /// create the worker thread.
    pub fn start_with_semantic_and_resource_authorities(
        tasks: Arc<SqliteTaskAuthority>,
        artifacts: Arc<ArtifactStore>,
        semantic: Option<Arc<SemanticAuthority>>,
        resource: Option<Arc<ResourceAuthority>>,
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
                    run_worker(
                        &tasks,
                        &artifacts,
                        semantic.as_deref(),
                        resource.as_deref(),
                        config,
                        &stop_rx,
                        &thread_health,
                    );
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
    semantic: Option<&SemanticAuthority>,
    resource: Option<&ResourceAuthority>,
    config: RecoveryWorkerConfig,
    stop_rx: &Receiver<()>,
    health: &Mutex<RecoveryWorkerHealth>,
) {
    lock(health).state = RecoveryWorkerState::Running;
    loop {
        let (artifact, semantic, resource_outcome) = match now_ms() {
            Ok(timestamp) => {
                // One cycle drives the artifact half, then the semantic
                // half, then the resource half (W28-C-3 pinned order). A
                // faulted semantic or resource domain is skipped while the
                // other halves keep scanning; an artifact-domain fault is
                // terminal for the whole thread (the pre-existing Faulted
                // transition), so the artifact half needs no skip check. A
                // worker started without an authority for a domain runs no
                // half for it at all.
                let artifact = artifact_cycle(tasks, artifacts, config, timestamp);
                let semantic = match semantic {
                    Some(authority) => {
                        if lock(health).semantic_domain_faulted {
                            DomainCycleOutcome::empty()
                        } else {
                            semantic_cycle(tasks, authority, config, timestamp)
                        }
                    }
                    None => DomainCycleOutcome::empty(),
                };
                let resource_outcome = match resource {
                    Some(authority) => {
                        if lock(health).resource_domain_faulted {
                            DomainCycleOutcome::empty()
                        } else {
                            resource_cycle(tasks, authority, config, timestamp)
                        }
                    }
                    None => DomainCycleOutcome::empty(),
                };
                (artifact, semantic, resource_outcome)
            }
            // An unusable clock blocks all halves before any can run. It
            // is accounted against the artifact-domain failure budget —
            // the budget that has always terminated the worker thread —
            // and surfaces as exactly one Worker-authority failure.
            Err(message) => (
                DomainCycleOutcome::worker_infrastructure_failure(message),
                DomainCycleOutcome::empty(),
                DomainCycleOutcome::empty(),
            ),
        };

        let Some(delay) = account_cycle(config, health, artifact, semantic, resource_outcome)
        else {
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

/// Aggregates the three domain halves into one health update and returns
/// the delay before the next cycle, or `None` when the worker thread
/// faulted terminally.
// One aggregation per cycle stays linear so each domain's counters, fault
// bit, and backoff contribution read adjacently (same convention as the
// domain cycle functions below).
#[allow(clippy::too_many_lines)]
fn account_cycle(
    config: RecoveryWorkerConfig,
    health: &Mutex<RecoveryWorkerHealth>,
    artifact: DomainCycleOutcome,
    semantic: DomainCycleOutcome,
    resource: DomainCycleOutcome,
) -> Option<Duration> {
    let artifact_infra = artifact.infrastructure_failure;
    let semantic_infra = semantic.infrastructure_failure;
    let resource_infra = resource.infrastructure_failure;
    let mut failures = artifact.failures;
    failures.extend(semantic.failures);
    failures.extend(resource.failures);
    let plan_retry_delay = min_option_duration(
        min_option_duration(artifact.retry_delay, semantic.retry_delay),
        resource.retry_delay,
    );

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
    current.resource_total_inspected = current
        .resource_total_inspected
        .saturating_add(u64::try_from(resource.inspected).unwrap_or(u64::MAX));
    current.resource_total_finalized = current
        .resource_total_finalized
        .saturating_add(u64::try_from(resource.finalized).unwrap_or(u64::MAX));
    current.resource_durable_retrying = resource.durable_retrying;
    current.resource_durable_escalated = resource.durable_escalated;
    current.resource_durable_unacknowledged_escalated = resource.durable_unacknowledged_escalated;
    current.resource_durable_resolved = resource.durable_resolved;

    // Per-domain consecutive infrastructure-failure accounting. The
    // existing `consecutive_failed_cycles` field is the artifact-domain
    // counter; the semantic and resource domains count independently
    // against the same threshold. A skipped (faulted) domain neither counts
    // nor resets.
    //
    // Pinned decision (W26-002, mirrored for the resource domain): setting
    // a domain's fault bit does NOT clear its consecutive counter. The bit
    // is sticky for the life of the worker instance, so clearing the
    // counter on that transition would leave a permanently faulted domain
    // with a zeroed counter — an unexplainable health surface. The counter
    // stays at the threshold as the evidence of why the bit was set; a
    // fresh worker instance starts both at zero.
    if artifact_infra {
        current.consecutive_failed_cycles = current.consecutive_failed_cycles.saturating_add(1);
    } else {
        current.consecutive_failed_cycles = 0;
    }
    let (semantic_consecutive, semantic_faulted) = account_isolated_domain_failures(
        semantic_infra,
        current.semantic_consecutive_failed_cycles,
        current.semantic_domain_faulted,
        config.failure_threshold,
    );
    current.semantic_consecutive_failed_cycles = semantic_consecutive;
    current.semantic_domain_faulted = semantic_faulted;
    let (resource_consecutive, resource_faulted) = account_isolated_domain_failures(
        resource_infra,
        current.resource_consecutive_failed_cycles,
        current.resource_domain_faulted,
        config.failure_threshold,
    );
    current.resource_consecutive_failed_cycles = resource_consecutive;
    current.resource_domain_faulted = resource_faulted;

    if failures.is_empty() {
        current.retry_delay = None;
        current.last_failures.clear();
        current.state = RecoveryWorkerState::Running;
        return Some(config.poll_interval);
    }
    current.last_failures = failures;
    if !artifact_infra && !semantic_infra && !resource_infra {
        // Plan-level failures only: the affected plans retry on their own
        // durable schedule; no domain's failure budget is consumed.
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
    if resource_infra && !current.resource_domain_faulted {
        let resource_backoff = retry_delay(config, current.resource_consecutive_failed_cycles);
        backoff = Some(backoff.map_or(resource_backoff, |current| current.max(resource_backoff)));
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

/// Per-domain consecutive infrastructure-failure accounting for a domain
/// whose fault bit is isolated from the thread lifecycle (semantic and
/// resource). An exhausted budget only sets the domain's sticky fault bit;
/// the caller holds the pinned W26-002 observability decision that the
/// counter is not cleared on that transition.
fn account_isolated_domain_failures(
    infrastructure_failure: bool,
    consecutive: usize,
    domain_faulted: bool,
    threshold: usize,
) -> (usize, bool) {
    if infrastructure_failure {
        let consecutive = consecutive.saturating_add(1);
        if consecutive >= threshold {
            (consecutive, true)
        } else {
            (consecutive, domain_faulted)
        }
    } else if domain_faulted {
        (consecutive, domain_faulted)
    } else {
        (0, domain_faulted)
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

/// Semantic-domain half of one cycle: scan the due Semantic plans, converge
/// each through the coordinator, and record plan-level failures in the
/// durable semantic recovery ledger with a compare-and-swap on the plan's
/// total-failure count. Storage failures on this path (scan, ledger read,
/// ledger append, summary) are infrastructure failures that consume the
/// semantic domain's failure budget; owner/coordinator rejections are
/// plan-level and only advance the ledger.
// Keeping one cycle linear makes the external converge result and its
// TaskAuthority ledger CAS visibly adjacent for crash-window review.
#[allow(clippy::too_many_lines)]
fn semantic_cycle(
    tasks: &SqliteTaskAuthority,
    semantic: &SemanticAuthority,
    config: RecoveryWorkerConfig,
    now_ms: i64,
) -> DomainCycleOutcome {
    let plans = match tasks.list_due_semantic_commit_plans(config.scan_limit, now_ms) {
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
    let coordinator = SemanticCommitCoordinator::new(tasks, semantic);
    for plan in plans {
        match coordinator.converge(ConvergeSemanticCommitRequest {
            plan_id: plan.plan_id,
            now_ms,
        }) {
            Ok(_) => outcome.finalized += 1,
            Err(error) => {
                // The health failure surface types plan identity as
                // `ArtifactCommitPlanId` (pre-dual shape); a Semantic plan id
                // cannot be laundered into it, so semantic entries carry no
                // plan id. Durable per-plan identity lives in the semantic
                // recovery ledger (`inspect_semantic_recovery`); widening the
                // health failure struct belongs to the semantic alert
                // surface, which owns its SABI consumers.
                outcome.failures.push(failure_of(None, &error));
                let current = match tasks.inspect_semantic_recovery(plan.plan_id) {
                    Ok(record) => record.map_or(0, |record| record.total_failures),
                    Err(ledger_error) => {
                        outcome.infrastructure_failure = true;
                        outcome
                            .failures
                            .push(failure_of(None, &CoordinatorError::Task(ledger_error)));
                        continue;
                    }
                };
                match tasks.record_semantic_recovery_failure(SemanticRecoveryFailureRequest {
                    plan_id: plan.plan_id,
                    expected_total_failures: current,
                    source: semantic_recovery_source(&error),
                    observed_at_ms: now_ms,
                    // Both ledgers receive the same config-derived bounds
                    // (`poll_interval`/`max_backoff`), but they do not share
                    // a delay distribution: the artifact ledger jitters per
                    // plan while the semantic ledger is a pure capped
                    // exponential. Equal retry times across the two domains
                    // must not be assumed by callers or tests.
                    base_delay_ms: duration_ms(config.poll_interval),
                    max_delay_ms: duration_ms(config.max_backoff),
                }) {
                    Ok(record) if record.state == SemanticRecoveryState::Retrying => {
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
                        outcome
                            .failures
                            .push(failure_of(None, &CoordinatorError::Task(ledger_error)));
                    }
                }
            }
        }
    }
    match tasks.summarize_semantic_recovery() {
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

/// Maps one semantic-domain coordinator failure to the durable ledger's
/// failure source. Unlike the artifact mapping, a `CoordinatorError::Semantic`
/// is the semantic domain's own owner authority; `CoordinatorError::Artifact`
/// cannot occur on the semantic path and falls back to `Coordinator` so the
/// mapping stays total.
const fn semantic_recovery_source(error: &CoordinatorError) -> SemanticRecoveryFailureSource {
    match error {
        CoordinatorError::Task(_) => SemanticRecoveryFailureSource::TaskAuthority,
        CoordinatorError::Semantic(_) => SemanticRecoveryFailureSource::SemanticAuthority,
        CoordinatorError::InvalidTimestamp | CoordinatorError::Artifact(_) => {
            SemanticRecoveryFailureSource::Coordinator
        }
    }
}

/// Resource-domain half of one cycle: scan the due Resource finalize plans
/// and converge each through the nlos-task plan API. A `NotDue` decision is
/// the honest boundary of ADR-0017 decision R-C, not a failure: nothing is
/// recorded and the plan remains an inspectable durable fact (G4). Every
/// converge error is recorded in the durable resource recovery ledger with
/// a compare-and-swap on the plan's total-failure count; storage failures
/// on this path (scan, ledger read, ledger append, summary) are
/// infrastructure failures that consume the resource domain's own failure
/// budget.
// Keeping one cycle linear makes the external converge result and its
// TaskAuthority ledger CAS visibly adjacent for crash-window review.
#[allow(clippy::too_many_lines)]
fn resource_cycle(
    tasks: &SqliteTaskAuthority,
    resource: &ResourceAuthority,
    config: RecoveryWorkerConfig,
    now_ms: i64,
) -> DomainCycleOutcome {
    let plans = match tasks.list_due_resource_commit_plans(config.scan_limit, now_ms) {
        Ok(plans) => plans,
        Err(error) => {
            return DomainCycleOutcome {
                inspected: 0,
                finalized: 0,
                failures: vec![resource_failure_of(&error)],
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
    for plan in plans {
        match tasks.converge_resource_commit_plan(resource, plan.plan_id, now_ms) {
            Ok(ResourceConvergeDecision::Finalized(_) | ResourceConvergeDecision::Replayed(_)) => {
                outcome.finalized += 1;
            }
            // Not-due is a decision, not a failure: zero ledger writes, so
            // the plan stays due on every later scan until the owner
            // settles (ADR-0017 G4).
            Ok(ResourceConvergeDecision::NotDue(_)) => {}
            Err(error) => {
                // Same typed-id wart as the semantic half: the health
                // failure surface types plan identity as
                // `ArtifactCommitPlanId`, so resource entries carry no plan
                // id. Durable per-plan identity lives in the resource
                // recovery ledger (`inspect_resource_recovery`).
                outcome.failures.push(resource_failure_of(&error));
                let current = match tasks.inspect_resource_recovery(plan.plan_id) {
                    Ok(record) => record.map_or(0, |record| record.total_failures),
                    Err(ledger_error) => {
                        outcome.infrastructure_failure = true;
                        outcome.failures.push(resource_failure_of(&ledger_error));
                        continue;
                    }
                };
                match tasks.record_resource_recovery_failure(ResourceRecoveryFailureRequest {
                    plan_id: plan.plan_id,
                    expected_total_failures: current,
                    source: resource_recovery_source(&error),
                    observed_at_ms: now_ms,
                    // Same config-derived bounds as the semantic ledger:
                    // the resource ledger's escalation threshold is pinned
                    // inside nlos-task (schema v43 mirror of v42), so no
                    // threshold travels with the request.
                    base_delay_ms: duration_ms(config.poll_interval),
                    max_delay_ms: duration_ms(config.max_backoff),
                }) {
                    Ok(record) if record.state == ResourceRecoveryState::Retrying => {
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
                        outcome.failures.push(resource_failure_of(&ledger_error));
                    }
                }
            }
        }
    }
    match tasks.summarize_resource_recovery() {
        Ok(summary) => {
            outcome.durable_retrying = summary.retrying;
            outcome.durable_escalated = summary.escalated;
            outcome.durable_unacknowledged_escalated = summary.unacknowledged_escalated;
            outcome.durable_resolved = summary.resolved;
        }
        Err(error) => {
            outcome.infrastructure_failure = true;
            outcome.failures.push(resource_failure_of(&error));
        }
    }
    outcome
}

/// Maps one resource-domain converge failure to the durable ledger's
/// failure source: the owner read surfaces as
/// [`TaskStoreError::ResourceParticipantAuthority`] and everything else is
/// a Task-side failure.
fn resource_recovery_source(error: &TaskStoreError) -> ResourceRecoveryFailureSource {
    match error {
        TaskStoreError::ResourceParticipantAuthority(_) => {
            ResourceRecoveryFailureSource::ResourceAuthority
        }
        _ => ResourceRecoveryFailureSource::TaskAuthority,
    }
}

/// Health-safe failure for the resource half. The coarse health authority
/// enum has no resource-owner variant (its SABI consumers in
/// nlos-system-control match it exhaustively); mirroring the semantic
/// precedent, an owner-read failure is reported as `Coordinator` and the
/// precise Task-vs-owner source lives in the durable resource ledger.
fn resource_failure_of(error: &TaskStoreError) -> RecoveryWorkerFailure {
    let authority = match error {
        TaskStoreError::ResourceParticipantAuthority(_) => RecoveryFailureAuthority::Coordinator,
        _ => RecoveryFailureAuthority::Task,
    };
    RecoveryWorkerFailure {
        plan_id: None,
        authority,
        message: error.to_string(),
    }
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
