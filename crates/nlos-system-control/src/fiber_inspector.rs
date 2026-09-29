//! Optional [`ExecutionFiberInspectSource`] adapter backed by the
//! [`nlos_runtime_tokio::TokioRuntimeAdapter`] snapshot surface (W32-G,
//! B5-3).
//!
//! Enabled with the crate's `runtime` feature; the default control prefix
//! uses [`crate::UnwiredExecutionFiberInspectSource`] until a host wires
//! this adapter. Only the read-only inspect surface is touched — state,
//! lifecycle phase, and bounded usage meters.

use std::num::NonZeroU64;
use std::time::Duration;

use nlos_runtime::{FiberHandle, RuntimeAdapter as _, RuntimeError};
use nlos_runtime_tokio::{FiberLifecyclePhase, TokioRuntimeAdapter};
use nlos_schema::sabi::v1::{
    ExecutionFiberLifecycleState as WireState, ExecutionFiberPhase as WirePhase, RetryDirective,
    SabiErrorCode, SabiFailure,
};
use nlos_types::{ExecutionFiberId, Generation};

use crate::ExecutionFiberInspectSource;
use crate::control::ExecutionFiberInspection;

/// Reads bounded fiber snapshot facts through the live tokio runtime.
pub struct TokioExecutionFiberSource<'a> {
    runtime: &'a TokioRuntimeAdapter,
}

impl<'a> TokioExecutionFiberSource<'a> {
    #[must_use]
    pub const fn new(runtime: &'a TokioRuntimeAdapter) -> Self {
        Self { runtime }
    }
}

impl ExecutionFiberInspectSource for TokioExecutionFiberSource<'_> {
    fn inspect_execution_fiber(
        &self,
        fiber_id: [u8; 16],
        generation: u64,
    ) -> Result<ExecutionFiberInspection, SabiFailure> {
        let Some(generation) = NonZeroU64::new(generation) else {
            return Err(invalid_argument(
                "fiber generation must be a non-zero generation",
            ));
        };
        let handle = FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes(fiber_id),
            generation: Generation::new(generation),
        };
        let state = self.runtime.inspect(handle).map_err(map_runtime_error)?;
        let phase = self
            .runtime
            .inspect_lifecycle_phase(handle)
            .map_err(map_runtime_error)?;
        let usage = self
            .runtime
            .activation_usage(handle)
            .map_err(map_runtime_error)?;
        Ok(ExecutionFiberInspection {
            fiber_id: *handle.fiber_id.as_bytes(),
            generation: handle.generation.get(),
            state: fiber_state(state),
            lifecycle_phase: fiber_phase(phase),
            active_cpu_ms: duration_ms(usage.active_cpu),
            elapsed_wall_ms: duration_ms(usage.elapsed_wall),
            scheduler_wait_ms: duration_ms(usage.scheduler_wait),
            external_wait_ms: duration_ms(usage.external_wait),
            backpressure_wait_ms: duration_ms(usage.backpressure_wait),
            suspended_ms: duration_ms(usage.suspended),
        })
    }
}

const fn fiber_state(state: nlos_runtime::FiberState) -> WireState {
    use nlos_runtime::FiberState as Source;
    match state {
        Source::Created => WireState::Created,
        Source::Ready => WireState::Ready,
        Source::Running => WireState::Running,
        Source::WaitingIo => WireState::WaitingIo,
        Source::WaitingModel => WireState::WaitingModel,
        Source::WaitingTool => WireState::WaitingTool,
        Source::Suspended => WireState::Suspended,
        Source::Completed => WireState::Completed,
        Source::Failed => WireState::Failed,
        Source::Cancelled => WireState::Cancelled,
    }
}

const fn fiber_phase(phase: FiberLifecyclePhase) -> WirePhase {
    match phase {
        FiberLifecyclePhase::Running => WirePhase::Running,
        FiberLifecyclePhase::WaitingExternal => WirePhase::WaitingExternal,
        FiberLifecyclePhase::BackpressureWait => WirePhase::BackpressureWait,
        FiberLifecyclePhase::Suspended => WirePhase::Suspended,
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn invalid_argument(message: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::InvalidArgument.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

/// The runtime exposes no dedicated not-found variant: an unknown handle
/// resolves as `InvalidGeneration` and a reaped record as `FiberReaped`, so
/// both map to bounded `NOT_FOUND` readings with distinct messages.
fn map_runtime_error(error: RuntimeError) -> SabiFailure {
    let (code, retry, safe_message) = match &error {
        RuntimeError::InvalidGeneration => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested execution fiber handle was not found",
        ),
        RuntimeError::FiberReaped { .. } => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested execution fiber record was already reaped",
        ),
        RuntimeError::DuplicateFiber
        | RuntimeError::Cancelled
        | RuntimeError::DeadlineExceeded
        | RuntimeError::QueueFull
        | RuntimeError::ShuttingDown => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "runtime rejected the fiber inspection request",
        ),
    };
    SabiFailure {
        code: code.into(),
        retry: retry.into(),
        safe_message: safe_message.to_owned(),
    }
}
