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
        })
    }
}

impl Error for RuntimeError {}

/// The executor boundary used by NLOS services.
///
/// Implementations must preserve NLOS identity, cancellation, admission, and
/// metering semantics. An implementation-local task handle is not authority.
pub trait RuntimeAdapter: Send + Sync {
    /// Admits and schedules a new execution fiber.
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
