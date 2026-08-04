//! Domain model for the durable task authority.
//!
//! This module contains the caller-supplied specifications, the durable
//! record types returned by inspections, the lifecycle enums, and the
//! deterministic digest/identity formulas used by this slice. All digest
//! formulas are domain-separated SHA-256 placeholders: they fix the
//! deterministic shape required by `[TASK-EFFECT-ID-001]` but do not yet
//! implement the canonical deterministic-CBOR encoding or signatures
//! mandated by the full §25.1 contract.

use nlos_types::{
    CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId,
    TaskId, TaskSnapshotId,
};
use sha2::{Digest, Sha256};

use crate::TaskStoreError;

/// Computes the domain-separated placeholder digest of the empty task effect
/// history.
///
/// `[TASK-EFFECT-ID-001]` requires the initial `TaskHead` history to be the
/// fixed formula over empty entries. This slice uses
/// `SHA-256("llmos/task-effect-history/v1" || 0x80)` where `0x80` is the CBOR
/// empty array standing in for the deterministic-CBOR encoding of an empty
/// entry list. The constant is stable across restarts and platforms.
#[must_use]
pub fn empty_effect_history_root() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-effect-history/v1");
    hasher.update([0x80u8]);
    hasher.finalize().into()
}

/// Durable specification of a registered Task.
///
/// Registration is idempotent on `task_id`: repeating the exact same
/// specification returns the existing record, while reusing the ID with a
/// different generation is rejected fail-closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskSpec {
    pub task_id: TaskId,
    pub task_generation: Generation,
    /// Caller-supplied registration time in milliseconds (wall-clock is not
    /// part of the authority's causality; it is stored for observability).
    pub registered_at_ms: i64,
}

/// Frozen-input digest bundle standing in for a full `TaskSnapshot`.
///
/// `[TASK-SNAPSHOT-001/002]` requires a causal-closed cut over the current
/// `TaskHead`, the durable effect-history root, and the retry-fence epoch,
/// frozen before the attempt starts. In this slice the snapshot is
/// represented by this caller-supplied bundle; the per-authority checkpoint
/// collection and the signed `TaskSnapshotReceipt` are out of scope. The
/// bundle is immutable once inserted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotBundle {
    pub snapshot_id: TaskSnapshotId,
    pub snapshot_digest: [u8; 32],
    pub expected_head_commit_seq: u64,
    pub effect_history_root: [u8; 32],
    pub retry_fence_epoch: u64,
}

/// Durable specification of one `TaskAttempt` registration.
///
/// Every retry or parallel candidate MUST use an independent
/// `attempt_id`/`attempt_generation` pair with its own cancellation scope
/// (`[TASK-ATTEMPT-001]`); the authority never rewrites an old attempt to
/// fake "the same success".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptSpec {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub snapshot: SnapshotBundle,
    pub cancellation_scope_id: CancellationScopeId,
    pub cancellation_generation: Generation,
    pub idempotency_key: IdempotencyKey,
    pub registered_at_ms: i64,
}

/// A `CommitPermit` issuance request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermitRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    /// Caller-supplied digest placeholder for the staged `TaskWriteSet`.
    pub write_set_root: [u8; 32],
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied expiry. Expiry never clears an issued permit
    /// (`[TASK-COMMIT-003]`); it is stored for the later effect slice.
    pub valid_until_ms: i64,
    pub requested_at_ms: i64,
}

/// A Task cancellation request (`[TASK-CANCEL-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelRequest {
    pub task_id: TaskId,
    pub idempotency_key: IdempotencyKey,
    pub requested_at_ms: i64,
}

/// A permit-holder finalize request.
///
/// The caller supplies the post-commit roots. Because the effect-history
/// slice is out of scope, a no-effect commit keeps
/// `new_effect_history_root` equal to the prior root and
/// `new_retry_fence_epoch` equal to the prior fence; the authority enforces
/// only that the fence never regresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalizeRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub permit_id: CommitPermitId,
    pub new_effect_history_root: [u8; 32],
    pub new_retry_fence_epoch: u64,
    pub finalized_at_ms: i64,
}

/// Lifecycle of a registered Task in this slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskState {
    Active,
    Cancelled,
}

/// Pre-permit subset of the §25.1 `TaskAttempt` state machine.
///
/// Reachable in this slice: `Created` → `ReadyToCommit` →
/// `CommitPermitted` (CAS win) | `Superseded` (CAS loss) | `Conflicted`
/// (validation failure), `CommitPermitted` → `Committed` (finalize), and any
/// open pre-permit state → `Cancelled`. The remaining variants are reserved
/// for the scheduling and effect slices and cannot be produced here;
/// post-permit `EFFECTING`/`FINALIZING`/`UNCERTAIN`/`RECONCILING` are
/// represented as permit states rather than attempt states in this slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptState {
    Created,
    /// Reserved for the scheduling slice; not producible here.
    Admitted,
    /// Reserved for the scheduling slice; not producible here.
    Running,
    /// Reserved for the scheduling slice; not producible here.
    Waiting,
    /// Reserved for the scheduling slice; not producible here.
    Sealing,
    /// Reserved for the scheduling slice; not producible here.
    Sealed,
    /// Reserved for the scheduling slice; not producible here.
    Validating,
    /// Permit CAS validation failed (stale snapshot/head binding).
    Conflicted,
    /// Candidate sealed and competing for the `CommitPermit`.
    ReadyToCommit,
    /// Lost the permit CAS to another attempt; terminally fenced.
    Superseded,
    /// Holds the issued `CommitPermit`.
    CommitPermitted,
    /// Reserved for the cancellation-drain slice; not producible here.
    Cancelling,
    /// Closed before any effect with a closure receipt; `TaskHead` unchanged.
    Cancelled,
    /// Reserved for the failure-reporting slice; not producible here.
    Failed,
    /// Permit holder finalized; `TaskHead` advanced with a commit receipt.
    Committed,
}

impl AttemptState {
    /// Whether the attempt is still an open pre-permit candidate that may
    /// request a `CommitPermit`.
    #[must_use]
    pub const fn is_open_candidate(self) -> bool {
        matches!(self, Self::Created | Self::ReadyToCommit)
    }

    /// Whether the attempt has reached a state this slice never leaves.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Conflicted | Self::Superseded | Self::Cancelled | Self::Committed
        )
    }

    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Created => 0,
            Self::Admitted => 1,
            Self::Running => 2,
            Self::Waiting => 3,
            Self::Sealing => 4,
            Self::Sealed => 5,
            Self::Validating => 6,
            Self::Conflicted => 7,
            Self::ReadyToCommit => 8,
            Self::Superseded => 9,
            Self::CommitPermitted => 10,
            Self::Cancelling => 11,
            Self::Cancelled => 12,
            Self::Failed => 13,
            Self::Committed => 14,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Created),
            1 => Ok(Self::Admitted),
            2 => Ok(Self::Running),
            3 => Ok(Self::Waiting),
            4 => Ok(Self::Sealing),
            5 => Ok(Self::Sealed),
            6 => Ok(Self::Validating),
            7 => Ok(Self::Conflicted),
            8 => Ok(Self::ReadyToCommit),
            9 => Ok(Self::Superseded),
            10 => Ok(Self::CommitPermitted),
            11 => Ok(Self::Cancelling),
            12 => Ok(Self::Cancelled),
            13 => Ok(Self::Failed),
            14 => Ok(Self::Committed),
            _ => Err(TaskStoreError::CorruptRecord("unknown attempt state")),
        }
    }
}

/// Lifecycle of a `CommitPermit`.
///
/// Only `Issued` and `Closed` are producible in this slice. `Superseded`
/// and `Quarantined` are reserved: CAS losers never receive a permit row
/// (the losing *attempt* enters `AttemptState::Superseded`), and the
/// quarantine tombstone belongs to the effect-reconciliation slice
/// (`[TASK-COMMIT-003]` / `[TASK-EFFECT-003]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermitState {
    Issued,
    Closed,
    /// Reserved tombstone; not producible in this slice.
    Superseded,
    /// Reserved tombstone; not producible in this slice.
    Quarantined,
}

impl PermitState {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Issued => 0,
            Self::Closed => 1,
            Self::Superseded => 2,
            Self::Quarantined => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Issued),
            1 => Ok(Self::Closed),
            2 => Ok(Self::Superseded),
            3 => Ok(Self::Quarantined),
            _ => Err(TaskStoreError::CorruptRecord("unknown permit state")),
        }
    }
}

/// Outcome recorded on a durable task receipt.
///
/// Only `Committed`, `FailedBeforeEffect`, and `CancelledBeforeEffect` are
/// producible in this slice (the latter two carry a `TaskPermitClosureReceipt`
/// shape with an unchanged `TaskHead`). `Partial`, `PartialEffect`, and
/// `FailedAfterEffect` are reserved for the effect slice
/// (`[TASK-RETRY-EFFECT-001]`) and are rejected fail-closed if ever
/// presented for insertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptOutcome {
    Committed,
    FailedBeforeEffect,
    CancelledBeforeEffect,
    /// Reserved for the effect slice; not producible here.
    Partial,
    /// Reserved for the effect slice; not producible here.
    PartialEffect,
    /// Reserved for the effect slice; not producible here.
    FailedAfterEffect,
}

impl ReceiptOutcome {
    pub(crate) const fn is_producible(self) -> bool {
        matches!(
            self,
            Self::Committed | Self::FailedBeforeEffect | Self::CancelledBeforeEffect
        )
    }

    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Committed => 0,
            Self::FailedBeforeEffect => 1,
            Self::CancelledBeforeEffect => 2,
            Self::Partial => 3,
            Self::PartialEffect => 4,
            Self::FailedAfterEffect => 5,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Committed),
            1 => Ok(Self::FailedBeforeEffect),
            2 => Ok(Self::CancelledBeforeEffect),
            3 => Ok(Self::Partial),
            4 => Ok(Self::PartialEffect),
            5 => Ok(Self::FailedAfterEffect),
            _ => Err(TaskStoreError::CorruptRecord("unknown receipt outcome")),
        }
    }
}

impl TaskState {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Active => 0,
            Self::Cancelled => 1,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Active),
            1 => Ok(Self::Cancelled),
            _ => Err(TaskStoreError::CorruptRecord("unknown task state")),
        }
    }
}

/// Identity of a registered attempt returned by registration decisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptHandle {
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub snapshot_id: TaskSnapshotId,
}

/// Durable head/control view of a Task (`TaskHead` + `TaskControlRecord`
/// subset for a single authority).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRecord {
    pub task_id: TaskId,
    pub task_generation: Generation,
    pub head_commit_seq: u64,
    pub head_effect_history_root: [u8; 32],
    pub retry_fence_epoch: u64,
    pub control_epoch: u64,
    pub cancel_epoch: u64,
    pub permit_epoch: u64,
    pub state: TaskState,
    /// The currently outstanding permit, if any. A `Closed` permit is not
    /// reported here; the CAS gate recomputes eligibility from permit rows.
    pub active_permit: Option<CommitPermitId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable view of one `TaskAttempt`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptRecord {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub snapshot: SnapshotBundle,
    pub cancellation_scope_id: CancellationScopeId,
    pub cancellation_generation: Generation,
    pub state: AttemptState,
    pub receipt_id: Option<ReceiptId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable view of one `CommitPermit`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermitRecord {
    pub permit_id: CommitPermitId,
    pub task_id: TaskId,
    pub idempotency_key: IdempotencyKey,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub expected_head_commit_seq: u64,
    pub expected_effect_history_root: [u8; 32],
    pub expected_retry_fence_epoch: u64,
    pub write_set_root: [u8; 32],
    pub permit_epoch: u64,
    pub control_epoch: u64,
    pub cancel_epoch: u64,
    pub valid_until_ms: i64,
    pub state: PermitState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable view of one task receipt (commit or closure).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskReceiptRecord {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    /// `None` for pre-permit closure receipts, which have no permit binding.
    pub permit_id: Option<CommitPermitId>,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub outcome: ReceiptOutcome,
    pub prior_head_commit_seq: u64,
    pub prior_effect_history_root: [u8; 32],
    pub prior_retry_fence_epoch: u64,
    pub new_head_commit_seq: u64,
    pub new_effect_history_root: [u8; 32],
    pub new_retry_fence_epoch: u64,
    pub created_at_ms: i64,
}

/// Decision of an idempotent Task registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskRegistrationDecision {
    Created(TaskId),
    Existing(TaskId),
}

impl TaskRegistrationDecision {
    #[must_use]
    pub const fn task_id(self) -> TaskId {
        match self {
            Self::Created(task_id) | Self::Existing(task_id) => task_id,
        }
    }
}

/// Decision of an idempotent `TaskAttempt` registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptRegistrationDecision {
    Created(AttemptHandle),
    Existing(AttemptHandle),
}

impl AttemptRegistrationDecision {
    #[must_use]
    pub const fn handle(self) -> AttemptHandle {
        match self {
            Self::Created(handle) | Self::Existing(handle) => handle,
        }
    }
}

/// Machine-readable reason a permit request was rejected as `Conflicted`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermitConflict {
    /// The attempt snapshot's expected head commit sequence no longer
    /// matches the current `TaskHead`.
    StaleTaskHead { expected: u64, current: u64 },
    /// The attempt snapshot's effect-history root no longer matches the
    /// current `TaskHead`.
    StaleEffectHistoryRoot,
    /// The attempt snapshot's retry-fence epoch no longer matches the
    /// current `TaskHead`.
    StaleRetryFenceEpoch,
    /// The same attempt already holds an issued permit under a different
    /// idempotency key.
    AttemptAlreadyHoldsPermit { permit_id: CommitPermitId },
}

/// Linearized decision of a `CommitPermit` request (`[TASK-COMMIT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermitDecision {
    /// This attempt won the CAS and holds the new permit.
    Issued(Box<PermitRecord>),
    /// Same idempotency key and same request bytes: the original permit.
    Replayed(Box<PermitRecord>),
    /// Another attempt already holds the outstanding permit; the requesting
    /// attempt is durably fenced as `Superseded`.
    Superseded { winner: Box<PermitRecord> },
    /// Validation failed; the requesting attempt is durably `Conflicted`.
    Conflicted { reason: PermitConflict },
    /// Cancellation committed first (`[TASK-CANCEL-003]`): no permit was
    /// issued, the attempt closed pre-permit with a closure receipt, and the
    /// `TaskHead` is unchanged.
    CancelledBeforeEffect { receipt_id: ReceiptId },
}

/// One attempt closed by a committed cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClosedAttempt {
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub receipt_id: ReceiptId,
}

/// Linearized decision of a Task cancellation (`[TASK-CANCEL-002]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancelDecision {
    /// The first committed cancellation: `cancel_epoch` advanced exactly
    /// once and every open pre-permit attempt closed with a closure receipt.
    Applied {
        cancel_epoch: u64,
        closed_attempts: Vec<ClosedAttempt>,
    },
    /// Exact replay of the original cancellation key; nothing re-applied.
    Replayed { cancel_epoch: u64 },
    /// A different key arrived after cancellation was already committed;
    /// the epoch is not incremented again.
    AlreadyCancelled { cancel_epoch: u64 },
}

/// Decision of a permit-holder finalize (`[TASK-COMMIT-001]` CAS commit).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinalizeDecision {
    /// `TaskHead` advanced and the permit closed in one transaction.
    Committed(Box<TaskReceiptRecord>),
    /// Exact replay of an already-finalized permit: the original receipt.
    Replayed(Box<TaskReceiptRecord>),
}

fn sha256_prefix16(domain: &str, parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update(part);
    }
    let full: [u8; 32] = hasher.finalize().into();
    let mut prefix = [0u8; 16];
    prefix.copy_from_slice(&full[..16]);
    prefix
}

/// Deterministic authority-issued permit identity. Deriving the ID from the
/// task and idempotency key makes replay after a restart return the same
/// permit and makes a never-issued "ghost" permit unrepresentable.
pub(crate) fn derive_permit_id(task_id: TaskId, idempotency_key: IdempotencyKey) -> CommitPermitId {
    CommitPermitId::from_bytes(sha256_prefix16(
        "llmos/task-commit-permit/v1",
        &[task_id.as_bytes(), idempotency_key.as_bytes()],
    ))
}

/// Deterministic commit receipt identity bound to its permit.
pub(crate) fn derive_commit_receipt_id(permit_id: CommitPermitId) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-commit-receipt/v1",
        &[permit_id.as_bytes()],
    ))
}

/// Deterministic pre-permit closure receipt identity bound to the attempt
/// and the cancel epoch that closed it (`[TASK-CANCEL-003]`).
pub(crate) fn derive_closure_receipt_id(
    task_id: TaskId,
    attempt_id: TaskAttemptId,
    cancel_epoch: u64,
) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-closure-receipt/v1",
        &[
            task_id.as_bytes(),
            attempt_id.as_bytes(),
            &cancel_epoch.to_be_bytes(),
        ],
    ))
}
