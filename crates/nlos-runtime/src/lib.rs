//! Runtime-independent contracts for NLOS execution fibers.
//!
//! Tokio or another executor may implement [`RuntimeAdapter`], but its local
//! task identity must never replace an NLOS `ExecutionFiberId`.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ProcessId,
    ResourceGroupId, SchedulerDomainId, TaskAttemptId,
};

pub type FiberFuture = Pin<Box<dyn Future<Output = FiberExit> + Send + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FiberState {
    Created,
    Ready,
    Running,
    WaitingIo,
    WaitingModel,
    WaitingTool,
    Suspended,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FiberExit {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FiberSpec {
    pub fiber_id: ExecutionFiberId,
    pub fiber_generation: Generation,
    pub agent_instance_id: AgentInstanceId,
    pub agent_generation: Generation,
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub task_attempt_id: Option<TaskAttemptId>,
    pub cancellation_scope_id: CancellationScopeId,
    pub cancellation_generation: Generation,
    pub resource_group_id: ResourceGroupId,
    pub scheduler_domain_id: SchedulerDomainId,
    pub deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ActivationUsage {
    pub active_cpu: Duration,
    pub elapsed_wall: Duration,
    pub scheduler_wait: Duration,
    pub external_wait: Duration,
    pub backpressure_wait: Duration,
    pub suspended: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FiberHandle {
    pub fiber_id: ExecutionFiberId,
    pub generation: Generation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    DuplicateFiber,
    InvalidGeneration,
    Cancelled,
    DeadlineExceeded,
    QueueFull,
    ShuttingDown,
    /// The fiber generation's record was already reaped: a join consumed its
    /// terminal exit, or a detach reclaimed it. The `(fiber_id, generation)`
    /// pair is carried so callers can attribute the rejection without a
    /// registry lookup.
    FiberReaped {
        fiber_id: ExecutionFiberId,
        generation: Generation,
    },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DuplicateFiber => "execution fiber already exists",
            Self::InvalidGeneration => "execution fiber generation is stale",
            Self::Cancelled => "cancellation scope is cancelled",
            Self::DeadlineExceeded => "execution fiber deadline was exceeded",
            Self::QueueFull => "runtime admission queue is full",
            Self::ShuttingDown => "runtime is shutting down",
            Self::FiberReaped { .. } => "execution fiber record was already reaped",
        })
    }
}

impl Error for RuntimeError {}

/// The typed birth (admission) decision for one
/// [`RuntimeAdapter::spawn_fiber`] attempt: the fiber generation was either
/// admitted and scheduled, or rejected before any side effect with the
/// contract's own admission reason.
///
/// This is the runtime-contract slice of the process-domain `BirthDecision`
/// family (design §8.2 `disposition: COMMIT | ABORT`): it types the
/// runtime's admission dimensions only. The cross-authority birth object
/// (launch grant digest, resource admission, capability prepares, process
/// identity receipts) is a different, durable artifact and stays out of
/// this contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BirthDecision {
    /// The fiber generation was admitted (disposition COMMIT): the handle
    /// is live and the future is scheduled without requiring a join.
    Admitted(FiberHandle),
    /// The birth was rejected before any side effect (disposition ABORT)
    /// with the typed admission reason.
    Rejected(BirthRejection),
}

impl BirthDecision {
    /// The admitted handle, or `None` for a rejected birth.
    #[must_use]
    pub const fn handle(&self) -> Option<&FiberHandle> {
        match self {
            Self::Admitted(handle) => Some(handle),
            Self::Rejected(_) => None,
        }
    }
}

/// Typed spawn-admission rejection reasons — exactly the failure dimensions
/// the runtime contract defines for [`RuntimeAdapter::spawn_fiber`]; no
/// dimension beyond that family is invented. Every variant carries the
/// underlying [`RuntimeError`] verbatim, so a decision consumer never loses
/// the wire error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BirthRejection {
    /// Runtime admission capacity is exhausted
    /// ([`RuntimeError::QueueFull`]).
    Capacity(RuntimeError),
    /// The cancellation scope is closed, so the birth is fenced
    /// ([`RuntimeError::Cancelled`]).
    Scope(RuntimeError),
    /// The fiber's deadline budget was already exceeded at birth
    /// ([`RuntimeError::DeadlineExceeded`]).
    Budget(RuntimeError),
    /// The fiber identity fence rejected the birth: a duplicate live
    /// identity, or a reaped identity still inside its tombstone window
    /// ([`RuntimeError::DuplicateFiber`] /
    /// [`RuntimeError::FiberReaped`]).
    Identity(RuntimeError),
    /// A generation fence rejected the birth: the fiber id is live under a
    /// different fiber generation, or the scope id is registered or fenced
    /// under a different cancellation generation
    /// ([`RuntimeError::InvalidGeneration`]).
    GenerationFence(RuntimeError),
    /// The runtime is unavailable for new births
    /// ([`RuntimeError::ShuttingDown`]).
    Unavailable(RuntimeError),
}

impl BirthRejection {
    /// The underlying contract error, verbatim.
    #[must_use]
    pub const fn error(&self) -> &RuntimeError {
        match self {
            Self::Capacity(error)
            | Self::Scope(error)
            | Self::Budget(error)
            | Self::Identity(error)
            | Self::GenerationFence(error)
            | Self::Unavailable(error) => error,
        }
    }
}

impl From<RuntimeError> for BirthRejection {
    fn from(error: RuntimeError) -> Self {
        match error {
            RuntimeError::QueueFull => Self::Capacity(error),
            RuntimeError::Cancelled => Self::Scope(error),
            RuntimeError::DeadlineExceeded => Self::Budget(error),
            RuntimeError::DuplicateFiber | RuntimeError::FiberReaped { .. } => {
                Self::Identity(error)
            }
            RuntimeError::InvalidGeneration => Self::GenerationFence(error),
            RuntimeError::ShuttingDown => Self::Unavailable(error),
        }
    }
}

/// The executor boundary used by NLOS services.
///
/// Implementations must preserve NLOS identity, cancellation, admission, and
/// metering semantics. An implementation-local task handle is not authority.
pub trait RuntimeAdapter: Send + Sync {
    /// Admits and schedules a new execution fiber.
    ///
    /// Success returns a [`FiberHandle`] immediately. The fiber runs
    /// concurrently without requiring a join (implicit detach). Callers that
    /// need to observe completion use [`Self::join_fiber`]; callers that
    /// explicitly relinquish join obligation may call [`Self::detach_fiber`]
    /// to reclaim the record (it does not change scheduling). A fiber that is
    /// neither joined nor detached keeps its terminal record registered —
    /// within a bounded tombstone window a re-spawn of the same
    /// `(fiber_id, generation)` stays [`RuntimeError::DuplicateFiber`] even
    /// after the record itself was reclaimed.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`] when identity/generation validation fails, the
    /// cancellation scope is closed, or runtime admission is unavailable.
    fn spawn_fiber(
        &self,
        spec: FiberSpec,
        future: FiberFuture,
    ) -> Result<FiberHandle, RuntimeError>;

    /// Admits a fiber and returns the complete typed birth decision.
    ///
    /// The default implementation drives [`Self::spawn_fiber`] and
    /// classifies its error with [`BirthRejection::from`], so every
    /// [`RuntimeAdapter`] exposes the same admission-decision surface.
    /// Implementations with richer admission internals MAY override it, but
    /// the classification MUST stay total over the contract's error family
    /// and MUST NOT invent dimensions beyond it. A
    /// [`BirthDecision::Rejected`] birth has zero runtime side effect.
    fn birth_fiber(&self, spec: FiberSpec, future: FiberFuture) -> BirthDecision {
        match self.spawn_fiber(spec, future) {
            Ok(handle) => BirthDecision::Admitted(handle),
            Err(error) => BirthDecision::Rejected(BirthRejection::from(error)),
        }
    }

    /// Waits until the fiber generation reaches a terminal state and returns
    /// its [`FiberExit`].
    ///
    /// A successful join is one-shot consumption: the terminal exit is
    /// handed to exactly one joiner and the fiber's runtime record is
    /// reclaimed. Joining an already-terminal fiber that has not been
    /// consumed returns the stored exit without blocking. A re-join of a
    /// reaped generation MUST be rejected with [`RuntimeError::FiberReaped`];
    /// a stale handle generation MUST be rejected with
    /// [`RuntimeError::InvalidGeneration`]. This contract MUST NOT expose an
    /// executor-local task handle.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::FiberReaped`] when the generation's record was
    /// already consumed by a join or reclaimed by a detach,
    /// [`RuntimeError::InvalidGeneration`] when the handle is stale, or an
    /// availability error when the runtime cannot accept the join.
    fn join_fiber(&self, handle: FiberHandle) -> Result<FiberExit, RuntimeError>;

    /// Explicitly relinquishes join obligation for a live or terminal fiber
    /// generation and reclaims its runtime record.
    ///
    /// [`Self::spawn_fiber`] is already implicitly detached (the fiber runs
    /// without requiring a join); this method validates the handle and
    /// documents the relinquishment for structured-concurrency audits. An
    /// already-terminal record is reclaimed immediately; a live one is
    /// reclaimed when it reaches its terminal state. Scheduling and
    /// admission are unchanged. After the record is reclaimed, a join of the
    /// same handle reports [`RuntimeError::FiberReaped`].
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::FiberReaped`] when the generation's record was
    /// already consumed, [`RuntimeError::InvalidGeneration`] when the handle
    /// is stale, or an availability error when the runtime cannot answer.
    fn detach_fiber(&self, handle: FiberHandle) -> Result<(), RuntimeError>;

    /// Cancels a structured cancellation scope.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] for a stale scope, or a
    /// runtime availability error when cancellation cannot be accepted.
    fn cancel_scope(
        &self,
        scope_id: CancellationScopeId,
        generation: Generation,
    ) -> Result<(), RuntimeError>;

    /// Reads the current runtime-local state of a fiber generation.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale, or
    /// an availability error when the runtime cannot answer.
    fn inspect(&self, handle: FiberHandle) -> Result<FiberState, RuntimeError>;

    /// Reads the current best-effort usage dimensions for a fiber generation.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidGeneration`] when the handle is stale, or
    /// an availability error when the runtime cannot answer.
    fn activation_usage(&self, handle: FiberHandle) -> Result<ActivationUsage, RuntimeError>;
}

/// The outcome of attempting to deliver an Operation wake to a fiber.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeOutcome {
    /// The fiber was waiting on this Operation and has been woken, or was
    /// already logically woken for the same Operation. Redelivery of the same
    /// wake MUST produce this outcome again without a second logical wake.
    Delivered,
    /// The fiber generation no longer exists. The wake is permanently
    /// undeliverable and MUST NOT be retried.
    FiberGone,
    /// The fiber exists but is not waiting on this Operation (already woken,
    /// cancelled, or completed). The wake is obsolete and MUST NOT be retried.
    NotWaiting,
}

/// Runtime-independent sink for durable Operation wakes.
///
/// This is the runtime-facing half of the Outbox closed loop: the persistent
/// authority commits `WakeFiber` entries in the same transaction as the
/// Operation terminal state, and a bounded consumer delivers them here only
/// after that transaction has committed.
///
/// Implementations:
///
/// - MUST fence on both the fiber handle generation and the Operation
///   identity + generation; a stale generation wake MUST NOT resume newer
///   fiber state;
/// - MUST be idempotent per `(fiber, operation)` pair, so that at-least-once
///   Outbox redelivery never causes a second logical wake;
/// - MUST NOT block the caller on fiber execution; delivery is a handoff, not
///   a join;
/// - MUST NOT expose executor-local task identity through this contract.
pub trait WakeSink: Send + Sync {
    /// Idempotently delivers the terminal wake for `operation_id` +
    /// `operation_generation` to `fiber`.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`] only for delivery failures where redelivery
    /// is meaningful. [`RuntimeError::ShuttingDown`] is terminal, not
    /// transient: the runtime is going away, so an Outbox consumer MUST stop
    /// draining (leaving entries durable for a future runtime) instead of
    /// retrying it like ordinary backpressure. Permanent per-entry conditions
    /// MUST be reported as [`WakeOutcome`] instead, so the Outbox consumer
    /// can acknowledge the entry instead of retrying forever.
    fn wake(
        &self,
        fiber: &FiberHandle,
        operation_id: OperationId,
        operation_generation: Generation,
    ) -> Result<WakeOutcome, RuntimeError>;
}

#[cfg(test)]
mod tests {
    use super::{
        BirthDecision, BirthRejection, ExecutionFiberId, FiberHandle, Generation, RuntimeError,
    };

    const HANDLE: FiberHandle = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes([0x42; 16]),
        generation: Generation::INITIAL,
    };

    #[test]
    fn birth_rejection_classifies_every_runtime_error_without_loss() {
        let errors = [
            RuntimeError::QueueFull,
            RuntimeError::Cancelled,
            RuntimeError::DeadlineExceeded,
            RuntimeError::DuplicateFiber,
            RuntimeError::InvalidGeneration,
            RuntimeError::ShuttingDown,
            RuntimeError::FiberReaped {
                fiber_id: HANDLE.fiber_id,
                generation: HANDLE.generation,
            },
        ];
        let expected = [
            BirthRejection::Capacity(RuntimeError::QueueFull),
            BirthRejection::Scope(RuntimeError::Cancelled),
            BirthRejection::Budget(RuntimeError::DeadlineExceeded),
            BirthRejection::Identity(RuntimeError::DuplicateFiber),
            BirthRejection::GenerationFence(RuntimeError::InvalidGeneration),
            BirthRejection::Unavailable(RuntimeError::ShuttingDown),
            BirthRejection::Identity(RuntimeError::FiberReaped {
                fiber_id: HANDLE.fiber_id,
                generation: HANDLE.generation,
            }),
        ];
        for (error, expected) in errors.into_iter().zip(expected) {
            let rejection = BirthRejection::from(error);
            assert_eq!(rejection, expected);
            assert_eq!(rejection.error(), expected.error());
        }
    }

    #[test]
    fn birth_decision_handle_is_present_only_for_admission() {
        assert_eq!(BirthDecision::Admitted(HANDLE).handle(), Some(&HANDLE));
        assert_eq!(
            BirthDecision::Rejected(BirthRejection::Capacity(RuntimeError::QueueFull)).handle(),
            None
        );
    }
}
