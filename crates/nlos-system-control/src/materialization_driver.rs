//! Production materialization drive for the resident daemon (W54-2:
//! the "built but not powered" registry item — `MaterializationScheduler`
//! had zero production references, so the plan→materialization→task
//! admission chain never turned under a real assembly).
//!
//! [`MaterializationDriver`] is the daemon's materialization worker
//! thread, lifecycle-shaped exactly like
//! [`nlos_commit_coordinator::TaskAuthorityCommitRecoveryWorker`]: a
//! named dedicated thread, idempotent start/stop with join, a read-only
//! health face, per-failure exponential backoff, and a terminal `Faulted`
//! state after a configurable run of consecutive failed cycles — never a
//! panic.
//!
//! Each cycle drives one scheduler pass over **every** durable plan
//! (enumerated through [`nlos_plan::SqlitePlanAuthority::list_plan_ids`]):
//! the Global tier selects ready nodes within the window
//! ([`MaterializationScheduler::select`]), the Worker tier drives each
//! selection through the W31-A gate — request → admission consult →
//! resolve ([`MaterializationScheduler::drive`]). One scheduler instance
//! owns the whole store, so the materialization window is one global
//! bound across plans, the designed two-tier posture.
//!
//! **Consult boundary**: the Task-side consumption path
//! (`SqliteTaskAuthority::answer_plan_materialization`) is mapped 1:1
//! onto [`nlos_plan::AdmissionConsult`] by the private adapter in this
//! module — the assembler wiring the scheduler's contract assigns to
//! slice-k; denials map to typed rejections (the window-shrink facts),
//! other Task errors stay `Err` (a failed consult leaves the round
//! `PENDING` for the next pass to adopt).
//!
//! **Error policy**: typed gate refusals and admission denials are
//! per-node scheduler decisions, not failures — the driver keeps
//! running. Only storage failures (plan enumeration or a pass aborting
//! with [`nlos_plan::PlanStoreError`]) fail a cycle: each failure
//! consumes one unit of the consecutive-failure budget with exponential
//! backoff, and an exhausted budget faults the thread terminally
//! (the daemon's service owner restarts it by reassembling). Nothing
//! here panics; a panicking pass is caught and surfaced as `Faulted`.
//!
//! **Ecosystem selector: deliberately not wired this lane.** The gate
//! the driver turns enforces dependency readiness and Task admission;
//! nodes whose structured conditions need ecosystem resolution are left
//! to the later lane that owns selector wiring — the scheduler battery's
//! selection semantics are unchanged by this module.

use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_plan::{
    AdmissionConsult, AdmissionConsultOutcome, MaterializationAdmission, MaterializationRejection,
    MaterializationScheduler, PlanStoreError, SchedulerPassSummary, SelectionReport,
    SqlitePlanAuthority,
};
use nlos_task::{MaterializationAdmissionFacts, SqliteTaskAuthority, TaskStoreError};

/// Lifecycle tuning for the daemon's materialization driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializationDriverConfig {
    /// The scheduler's materialization window: the global bound on
    /// concurrent gate rounds plus window-band nodes. The controller
    /// growth lever (`set_window`) is not exercised by the driver.
    pub window: u64,
    /// Delay after a completely successful cycle, including an empty one.
    pub poll_interval: Duration,
    /// Maximum delay after consecutive failed cycles.
    pub max_backoff: Duration,
    /// Consecutive failed cycles before the driver faults terminally.
    pub failure_threshold: usize,
}

impl Default for MaterializationDriverConfig {
    fn default() -> Self {
        Self {
            window: 8,
            poll_interval: Duration::from_millis(200),
            max_backoff: Duration::from_secs(5),
            failure_threshold: 8,
        }
    }
}

/// Observable driver lifecycle. `Faulted` and `Stopped` are terminal for
/// one driver instance; durable plans remain available to a newly started
/// driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaterializationDriverState {
    Starting,
    Running,
    BackingOff,
    Faulted,
    Stopped,
}

/// Read-only snapshot for daemon health and supervision. The counters
/// aggregate every plan the driver has driven; `window` and
/// `seats_in_use` mirror the scheduler's in-memory policy state (copied
/// out per cycle, so a restart honestly resets them).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationDriverHealth {
    pub state: MaterializationDriverState,
    /// Driver cycles completed (pass attempts over the whole plan set).
    pub completed_cycles: u64,
    /// Plans enumerated by the last cycle.
    pub plans_driven: u64,
    /// Cumulative scheduler passes (one per plan per cycle).
    pub passes: u64,
    pub total_selected: u64,
    pub total_approved: u64,
    pub total_rejected: u64,
    /// The scheduler's window after the last completed pass.
    pub window: u64,
    /// Window seats held after the last cycle (pending gate rounds plus
    /// window-band nodes, summed over the plan set).
    pub seats_in_use: u64,
    pub consecutive_failed_cycles: usize,
    pub retry_delay: Option<Duration>,
    /// Health-safe diagnostic for the last failed cycle (typed plan
    /// failures live in the scheduler decision trail and the plan
    /// authority's durable rows).
    pub last_failure: Option<String>,
}

impl Default for MaterializationDriverHealth {
    fn default() -> Self {
        Self {
            state: MaterializationDriverState::Starting,
            completed_cycles: 0,
            plans_driven: 0,
            passes: 0,
            total_selected: 0,
            total_approved: 0,
            total_rejected: 0,
            window: 0,
            seats_in_use: 0,
            consecutive_failed_cycles: 0,
            retry_delay: None,
            last_failure: None,
        }
    }
}

/// Failure to create the driver. No durable state has been changed solely
/// by constructing the handle.
#[derive(Debug)]
pub enum MaterializationDriverStartError {
    InvalidConfig(&'static str),
    Spawn(std::io::Error),
}

impl fmt::Display for MaterializationDriverStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(reason) => {
                write!(formatter, "invalid materialization driver config: {reason}")
            }
            Self::Spawn(error) => {
                write!(formatter, "could not spawn materialization driver: {error}")
            }
        }
    }
}

impl Error for MaterializationDriverStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(_) => None,
            Self::Spawn(error) => Some(error),
        }
    }
}

/// Daemon-owned lifecycle handle for the materialization drive.
///
/// The driver owns no canonical state: its thread drives the shared plan
/// authority through the W31-A gate faces and the shared task authority
/// through the read-only consult, so it can always be replaced after a
/// crash or fault from their durable prefix (pending gate rounds are
/// adopted by the next pass of a fresh driver).
pub struct MaterializationDriver {
    stop_tx: SyncSender<()>,
    join: Option<JoinHandle<()>>,
    health: Arc<Mutex<MaterializationDriverHealth>>,
}

impl MaterializationDriver {
    /// Starts the dedicated driver thread: one scheduler instance over
    /// the whole plan set, the Task authority's consumption path as the
    /// admission consult, and the config's window and cadence. The first
    /// cycle runs immediately; `poll_interval` applies only after it.
    ///
    /// # Errors
    ///
    /// Returns before spawning for an invalid config, or when the OS
    /// cannot create the driver thread.
    pub fn start(
        plans: Arc<SqlitePlanAuthority>,
        tasks: Arc<SqliteTaskAuthority>,
        config: MaterializationDriverConfig,
    ) -> Result<Self, MaterializationDriverStartError> {
        validate_config(config)?;
        let (stop_tx, stop_rx) = sync_channel(1);
        let health = Arc::new(Mutex::new(MaterializationDriverHealth::default()));
        let thread_health = Arc::clone(&health);
        let join = thread::Builder::new()
            .name("plan-materialization-driver".to_string())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run_driver(&plans, &tasks, config, &stop_rx, &thread_health);
                }));
                if outcome.is_err() {
                    let mut current = lock(&thread_health);
                    current.state = MaterializationDriverState::Faulted;
                    current.retry_delay = None;
                    current.last_failure = Some("materialization driver panicked".to_string());
                }
            })
            .map_err(MaterializationDriverStartError::Spawn)?;
        Ok(Self {
            stop_tx,
            join: Some(join),
            health,
        })
    }

    #[must_use]
    pub fn health(&self) -> MaterializationDriverHealth {
        lock(&self.health).clone()
    }

    /// Requests shutdown and joins the dedicated thread. Repeated calls
    /// are harmless.
    pub fn stop(&mut self) {
        let _ = self.stop_tx.try_send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for MaterializationDriver {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate_config(
    config: MaterializationDriverConfig,
) -> Result<(), MaterializationDriverStartError> {
    if config.poll_interval.is_zero() {
        return Err(MaterializationDriverStartError::InvalidConfig(
            "poll_interval must be non-zero",
        ));
    }
    if config.max_backoff < config.poll_interval {
        return Err(MaterializationDriverStartError::InvalidConfig(
            "max_backoff must be at least poll_interval",
        ));
    }
    if config.failure_threshold == 0 {
        return Err(MaterializationDriverStartError::InvalidConfig(
            "failure_threshold must be non-zero",
        ));
    }
    Ok(())
}

/// One cycle's aggregate, folded into the health snapshot.
struct CycleOutcome {
    plans: u64,
    passes: u64,
    selected: u64,
    approved: u64,
    rejected: u64,
    window: u64,
    seats_in_use: u64,
}

fn run_driver(
    plans: &SqlitePlanAuthority,
    tasks: &SqliteTaskAuthority,
    config: MaterializationDriverConfig,
    stop_rx: &Receiver<()>,
    health: &Mutex<MaterializationDriverHealth>,
) {
    lock(health).state = MaterializationDriverState::Running;
    let mut scheduler = MaterializationScheduler::new(config.window);
    loop {
        let at_ms = now_ms();
        let next_delay = match run_cycle(plans, tasks, &mut scheduler, at_ms) {
            Ok(outcome) => {
                let mut current = lock(health);
                current.state = MaterializationDriverState::Running;
                current.completed_cycles = current.completed_cycles.saturating_add(1);
                current.plans_driven = outcome.plans;
                current.passes = current.passes.saturating_add(outcome.passes);
                current.total_selected = current.total_selected.saturating_add(outcome.selected);
                current.total_approved = current.total_approved.saturating_add(outcome.approved);
                current.total_rejected = current.total_rejected.saturating_add(outcome.rejected);
                current.window = outcome.window;
                current.seats_in_use = outcome.seats_in_use;
                current.consecutive_failed_cycles = 0;
                current.retry_delay = None;
                current.last_failure = None;
                config.poll_interval
            }
            Err(error) => {
                let consecutive = {
                    let mut current = lock(health);
                    current.consecutive_failed_cycles =
                        current.consecutive_failed_cycles.saturating_add(1);
                    current.last_failure = Some(error.to_string());
                    current.consecutive_failed_cycles
                };
                if consecutive >= config.failure_threshold {
                    let mut current = lock(health);
                    current.state = MaterializationDriverState::Faulted;
                    current.retry_delay = None;
                    return;
                }
                let delay = retry_delay(config, consecutive);
                let mut current = lock(health);
                current.state = MaterializationDriverState::BackingOff;
                current.retry_delay = Some(delay);
                delay
            }
        };

        match stop_rx.recv_timeout(next_delay) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let mut current = lock(health);
                current.state = MaterializationDriverState::Stopped;
                current.retry_delay = None;
                return;
            }
        }
    }
}

/// One cycle: enumerate the plan set and drive one scheduler pass per
/// plan. The scheduler (and therefore the window) is shared across the
/// whole store — the designed global tier.
///
/// # Errors
///
/// Fails with the first storage failure (enumeration or a pass abort);
/// per-node gate refusals and admission denials are scheduler decisions,
/// not errors.
fn run_cycle(
    plans: &SqlitePlanAuthority,
    tasks: &SqliteTaskAuthority,
    scheduler: &mut MaterializationScheduler,
    at_ms: u64,
) -> Result<CycleOutcome, PlanStoreError> {
    let plan_ids = plans.list_plan_ids()?;
    let consult = TaskAuthorityMaterializationConsult { tasks };
    let mut outcome = CycleOutcome {
        plans: u64::try_from(plan_ids.len()).unwrap_or(u64::MAX),
        passes: 0,
        selected: 0,
        approved: 0,
        rejected: 0,
        window: scheduler.window(),
        seats_in_use: 0,
    };
    for plan_id in plan_ids {
        let report = scheduler.select(plans, plan_id)?;
        let summary = scheduler.drive(plans, &report, &consult, at_ms)?;
        fold_pass(&mut outcome, &report, &summary);
    }
    Ok(outcome)
}

fn fold_pass(outcome: &mut CycleOutcome, report: &SelectionReport, summary: &SchedulerPassSummary) {
    outcome.passes = outcome.passes.saturating_add(1);
    outcome.selected = outcome.selected.saturating_add(summary.selected);
    outcome.approved = outcome.approved.saturating_add(summary.approved);
    outcome.rejected = outcome.rejected.saturating_add(summary.rejected);
    outcome.window = summary.window_after;
    outcome.seats_in_use = outcome.seats_in_use.saturating_add(report.seats_in_use);
}

/// The production-shaped Worker-tier consult boundary: the Task
/// authority's consumption path (`answer_plan_materialization`, W31-A)
/// mapped onto the scheduler's typed consult outcome — the same wiring
/// the scheduler battery and the G3 battery drive inline. A typed
/// admission denial is a `Denied` answer (the window-shrink fact);
/// every other Task error stays `Err` (a failed consult leaves the
/// round `PENDING` for the next pass to adopt).
struct TaskAuthorityMaterializationConsult<'a> {
    tasks: &'a SqliteTaskAuthority,
}

impl AdmissionConsult for TaskAuthorityMaterializationConsult<'_> {
    type Error = TaskStoreError;

    fn consult_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, TaskStoreError> {
        match self
            .tasks
            .answer_plan_materialization(other_declared_task_nodes)
        {
            Ok(MaterializationAdmissionFacts {
                profile_id,
                projected_task_nodes,
                projected_active_working_set,
            }) => Ok(AdmissionConsultOutcome::Admitted(
                MaterializationAdmission {
                    profile_id: profile_id.to_string(),
                    projected_task_nodes,
                    projected_active_working_set,
                },
            )),
            Err(TaskStoreError::WorkingSetAdmissionDenied {
                profile_id,
                active_count,
                max_active_working_set,
            }) => Ok(AdmissionConsultOutcome::Denied(
                MaterializationRejection::WorkingSetFull {
                    profile_id: profile_id.to_string(),
                    active_count,
                    max_active_working_set,
                },
            )),
            Err(TaskStoreError::TaskNodeAdmissionDenied {
                profile_id,
                task_count,
                max_task_nodes,
            }) => Ok(AdmissionConsultOutcome::Denied(
                MaterializationRejection::TaskNodeCapExceeded {
                    profile_id: profile_id.to_string(),
                    task_count,
                    max_task_nodes,
                },
            )),
            Err(other) => Err(other),
        }
    }
}

fn retry_delay(config: MaterializationDriverConfig, consecutive_failures: usize) -> Duration {
    let exponent = u32::try_from(consecutive_failures.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(31);
    config
        .poll_interval
        .checked_mul(1_u32 << exponent)
        .unwrap_or(config.max_backoff)
        .min(config.max_backoff)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
