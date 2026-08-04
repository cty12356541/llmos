//! Durable single-authority Task store for NLOS (B-TASK first slice).
//!
//! This crate implements the durable `TaskAuthority` subset required by the
//! B-TASK first-slice acceptance gate: Task registration, frozen-input
//! snapshot digest binding, `TaskHead` revision CAS, dual `TaskAttempt`
//! registration with independent generations and cancellation scopes,
//! unique `CommitPermit` issuance (`[TASK-COMMIT-001]`), cancel/permit
//! linearization on one control/cancel/permit epoch (`[TASK-CANCEL-002]` /
//! `[TASK-CANCEL-003]`), and crash/restart recovery with no ghost permits.
//!
//! Explicitly out of scope for this slice: `EffectPermit` / `EffectSlot` /
//! effect history, TaskPlan/TaskNode materialization, Process/AgentInstance
//! binding, IsolationDomain/ResourceGroup, signatures, and any IPC surface.
//! Post-permit `EFFECTING`/`FINALIZING`/`UNCERTAIN`/`RECONCILING` from the
//! §25.1 attempt state machine are represented as permit states rather than
//! attempt states here.

mod model;
mod store;

pub use model::{
    AttemptHandle, AttemptRecord, AttemptRegistrationDecision, AttemptSpec, AttemptState,
    CancelDecision, CancelRequest, ClosedAttempt, FinalizeDecision, FinalizeRequest,
    PermitConflict, PermitDecision, PermitRecord, PermitRequest, PermitState, ReceiptOutcome,
    SnapshotBundle, TaskReceiptRecord, TaskRecord, TaskRegistrationDecision, TaskSpec, TaskState,
    empty_effect_history_root,
};
pub use store::SqliteTaskAuthority;

use std::error::Error;
use std::fmt;

/// Errors produced by the durable task authority.
///
/// Storage-level failures, durability negotiation, and schema validation
/// mirror `nlos-store`; domain violations (stale handles, holder mismatch,
/// invalid transitions) are typed so callers can distinguish safely
/// retryable conditions (`[TASK-CONFLICT-001]`).
#[derive(Debug)]
pub enum TaskStoreError {
    Sqlite(rusqlite::Error),
    CorruptRecord(&'static str),
    UnsupportedSchema(i64),
    LockPoisoned,
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    /// No task with the given ID is registered.
    TaskNotFound,
    /// No attempt with the given ID exists under the given task.
    AttemptNotFound,
    /// No permit with the given ID exists under the given task.
    PermitNotFound,
    /// No receipt with the given ID exists under the given task.
    ReceiptNotFound,
    /// The task ID is already registered with a different specification.
    DuplicateTask,
    /// The attempt ID is already registered with a different specification.
    DuplicateAttempt,
    /// A snapshot ID was rebound to different digest bytes; snapshots are
    /// immutable once inserted (`[TASK-SNAPSHOT-001]`).
    SnapshotConflict,
    /// An idempotency key was replayed with different request bytes.
    IdempotencyConflict,
    /// The caller presented a stale generation for an existing object.
    InvalidGeneration,
    /// The attempt is not in a state that allows the requested transition.
    InvalidAttemptState {
        state: AttemptState,
    },
    /// Attempts cannot be registered on a cancelled task.
    TaskCancelled,
    /// The finalize caller does not match the permit's attempt binding.
    NotPermitHolder,
    /// The permit is not in `Issued` state and the request is not an exact
    /// replay of the original finalize.
    PermitNotIssued,
    /// The current `TaskHead` no longer matches the permit's expected head.
    StaleTaskHead,
    /// A finalize tried to move the retry-fence epoch backwards.
    FenceRegression,
    /// A monotonic epoch/sequence space is exhausted; fail closed instead
    /// of wrapping.
    EpochExhausted,
}

impl fmt::Display for TaskStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite authority failure: {error}"),
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "unsupported task authority schema version {version}"
                )
            }
            Self::LockPoisoned => formatter.write_str("authority writer lock is poisoned"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::TaskNotFound => formatter.write_str("task is not registered"),
            Self::AttemptNotFound => formatter.write_str("attempt does not exist under the task"),
            Self::PermitNotFound => formatter.write_str("permit does not exist under the task"),
            Self::ReceiptNotFound => formatter.write_str("receipt does not exist under the task"),
            Self::DuplicateTask => {
                formatter.write_str("task ID re-registered with different specification")
            }
            Self::DuplicateAttempt => {
                formatter.write_str("attempt ID re-registered with different specification")
            }
            Self::SnapshotConflict => {
                formatter.write_str("snapshot ID rebound to different digest bytes")
            }
            Self::IdempotencyConflict => {
                formatter.write_str("idempotency key was reused for different request bytes")
            }
            Self::InvalidGeneration => formatter.write_str("stale generation for durable object"),
            Self::InvalidAttemptState { state } => {
                write!(formatter, "attempt state {state:?} rejects the transition")
            }
            Self::TaskCancelled => {
                formatter.write_str("cancelled task does not admit new attempts")
            }
            Self::NotPermitHolder => {
                formatter.write_str("caller does not match the permit attempt binding")
            }
            Self::PermitNotIssued => formatter.write_str("permit is not in issued state"),
            Self::StaleTaskHead => {
                formatter.write_str("current TaskHead does not match the permit expected head")
            }
            Self::FenceRegression => formatter.write_str("retry-fence epoch must never regress"),
            Self::EpochExhausted => formatter.write_str("monotonic epoch space exhausted"),
        }
    }
}

impl Error for TaskStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for TaskStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
