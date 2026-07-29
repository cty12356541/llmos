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
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, ProcessId, ResourceGroupId,
    SchedulerDomainId, TaskAttemptId,
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
