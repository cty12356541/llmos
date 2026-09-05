//! Platform kill adapter contract (contract-layer minimum prefix).
//!
//! This module deliberately does **not** spawn or signal real macOS/Windows
//! OS processes; consumers inject [`StubPlatformKillAdapter`] or
//! [`NoopPlatformKillAdapter`] until native supervisors land.

use std::error::Error;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use nlos_types::{Generation, ProcessId};

/// Outcome of one platform kill adapter invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformKillAdapterOutcome {
    /// The adapter accepted the kill signal (stub records it; noop discards).
    Signaled,
}

/// Platform-specific OS process kill signaling.
///
/// Implementations must be side-effect bounded to the presented Process
/// identity; this crate's authority path durably records the receipt before
/// invoking the adapter.
pub trait PlatformKillAdapter {
    /// Signals the host platform to kill the OS process backing `process_id`
    /// at `process_generation`. Failures propagate to the caller; the durable
    /// kill receipt remains committed (at-least-once semantics).
    ///
    /// # Errors
    ///
    /// Returns an adapter-specific error when the platform signal fails.
    fn signal_platform_kill(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError>;
}

/// Test-oriented adapter that records every signaled kill without touching the OS.
#[derive(Debug, Default)]
pub struct StubPlatformKillAdapter {
    signals: Mutex<Vec<(ProcessId, Generation)>>,
}

impl StubPlatformKillAdapter {
    /// Creates an empty stub adapter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a snapshot of every kill the adapter accepted.
    #[must_use]
    pub fn recorded_signals(&self) -> Vec<(ProcessId, Generation)> {
        self.lock_signals().clone()
    }

    fn lock_signals(&self) -> MutexGuard<'_, Vec<(ProcessId, Generation)>> {
        self.signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PlatformKillAdapter for StubPlatformKillAdapter {
    fn signal_platform_kill(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        self.lock_signals()
            .push((process_id, process_generation));
        Ok(PlatformKillAdapterOutcome::Signaled)
    }
}

/// Adapter that accepts kill requests and performs no platform action.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NoopPlatformKillAdapter;

impl PlatformKillAdapter for NoopPlatformKillAdapter {
    fn signal_platform_kill(
        &self,
        _process_id: ProcessId,
        _process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        Ok(PlatformKillAdapterOutcome::Signaled)
    }
}

#[derive(Debug)]
pub enum PlatformKillAdapterError {
    Platform(&'static str),
}

impl fmt::Display for PlatformKillAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(reason) => write!(formatter, "platform kill adapter failure: {reason}"),
        }
    }
}

impl Error for PlatformKillAdapterError {}
