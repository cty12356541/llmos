use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_artifact::ArtifactStore;
use nlos_task::{ArtifactCommitPlanId, SqliteTaskAuthority};

use crate::{ArtifactCommitCoordinator, CoordinatorError, PendingConvergenceReport};

struct CycleOutcome {
    inspected: usize,
    finalized: usize,
    failures: Vec<RecoveryWorkerFailure>,
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryWorkerHealth {
    pub state: RecoveryWorkerState,
    pub completed_cycles: u64,
    pub total_inspected: u64,
    pub total_finalized: u64,
    pub consecutive_failed_cycles: usize,
    pub retry_delay: Option<Duration>,
    pub last_failures: Vec<RecoveryWorkerFailure>,
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
        let outcome = now_ms().map_or_else(
            |message| CycleOutcome {
                inspected: 0,
                finalized: 0,
                failures: vec![RecoveryWorkerFailure {
                    plan_id: None,
                    authority: RecoveryFailureAuthority::Worker,
                    message,
                }],
            },
            |timestamp| match ArtifactCommitCoordinator::new(tasks, artifacts)
                .converge_pending_best_effort(config.scan_limit, timestamp)
            {
                Ok(report) => cycle_outcome(&report),
                Err(error) => CycleOutcome {
                    inspected: 0,
                    finalized: 0,
                    failures: vec![failure_of(None, &error)],
                },
            },
        );

        let delay = {
            let mut current = lock(health);
            current.completed_cycles = current.completed_cycles.saturating_add(1);
            current.total_inspected = current
                .total_inspected
                .saturating_add(u64::try_from(outcome.inspected).unwrap_or(u64::MAX));
            current.total_finalized = current
                .total_finalized
                .saturating_add(u64::try_from(outcome.finalized).unwrap_or(u64::MAX));
            if outcome.failures.is_empty() {
                current.consecutive_failed_cycles = 0;
                current.retry_delay = None;
                current.last_failures.clear();
                current.state = RecoveryWorkerState::Running;
                config.poll_interval
            } else {
                current.consecutive_failed_cycles =
                    current.consecutive_failed_cycles.saturating_add(1);
                current.last_failures = outcome.failures;
                if current.consecutive_failed_cycles >= config.failure_threshold {
                    current.state = RecoveryWorkerState::Faulted;
                    current.retry_delay = None;
                    return;
                }
                let delay = retry_delay(config, current.consecutive_failed_cycles);
                current.state = RecoveryWorkerState::BackingOff;
                current.retry_delay = Some(delay);
                delay
            }
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

fn cycle_outcome(report: &PendingConvergenceReport) -> CycleOutcome {
    CycleOutcome {
        inspected: report.inspected,
        finalized: report.finalized.len(),
        failures: report
            .failures
            .iter()
            .map(|failure| failure_of(Some(failure.plan_id), &failure.error))
            .collect(),
    }
}

fn failure_of(
    plan_id: Option<ArtifactCommitPlanId>,
    error: &CoordinatorError,
) -> RecoveryWorkerFailure {
    let authority = match error {
        CoordinatorError::Task(_) => RecoveryFailureAuthority::Task,
        CoordinatorError::Artifact(_) => RecoveryFailureAuthority::Artifact,
        CoordinatorError::InvalidTimestamp => RecoveryFailureAuthority::Coordinator,
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
