//! Platform kill adapter contract (contract-layer minimum prefix).
//!
//! Contract tests use [`StubPlatformKillAdapter`] or [`NoopPlatformKillAdapter`].
//! Unix hosts may inject [`PosixPlatformKillAdapter`] with an explicit
//! `ProcessId` → OS pid map when signaling real child processes. Windows
//! hosts may inject [`WindowsPlatformKillAdapter`] with the same map shape.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use nlos_types::{Generation, ProcessId};

/// Outcome of one platform kill adapter invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformKillAdapterOutcome {
    /// The adapter accepted the kill signal (stub records it; noop discards).
    Signaled,
    /// The host OS process was already terminated before the signal arrived.
    AlreadyTerminated,
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
        self.lock_signals().push((process_id, process_generation));
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

/// Unix adapter that signals real OS processes via `kill(2)` and SIGTERM.
///
/// NLOS [`ProcessId`] values are authority-assigned identifiers; callers must
/// inject the host pid mapping explicitly (typically from a process
/// supervisor). The mapping is held and consulted on Unix hosts only; on
/// other hosts the constructor discards it and signaling fails closed, so
/// the type stays nameable cross-platform.
#[derive(Debug)]
pub struct PosixPlatformKillAdapter {
    #[cfg(unix)]
    pid_map: HashMap<ProcessId, u32>,
}

#[cfg(unix)]
impl PosixPlatformKillAdapter {
    /// Creates an adapter backed by the supplied `ProcessId` → OS pid map.
    #[must_use]
    pub fn new(pid_map: HashMap<ProcessId, u32>) -> Self {
        Self { pid_map }
    }
}

#[cfg(not(unix))]
impl PosixPlatformKillAdapter {
    /// Creates an adapter that discards the supplied map: signaling always
    /// fails closed on non-Unix hosts, so no mapping is ever consulted.
    #[must_use]
    pub fn new(_pid_map: HashMap<ProcessId, u32>) -> Self {
        Self {}
    }
}

#[cfg(unix)]
impl PlatformKillAdapter for PosixPlatformKillAdapter {
    fn signal_platform_kill(
        &self,
        process_id: ProcessId,
        _process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let os_pid = self
            .pid_map
            .get(&process_id)
            .ok_or(PlatformKillAdapterError::Platform(
                "os pid mapping not found for process id",
            ))?;
        match kill(Pid::from_raw((*os_pid).cast_signed()), Signal::SIGTERM) {
            Ok(()) => Ok(PlatformKillAdapterOutcome::Signaled),
            Err(Errno::ESRCH) => Ok(PlatformKillAdapterOutcome::AlreadyTerminated),
            Err(_) => Err(PlatformKillAdapterError::Platform(
                "kill(SIGTERM) failed for mapped os pid",
            )),
        }
    }
}

#[cfg(windows)]
impl PlatformKillAdapter for PosixPlatformKillAdapter {
    fn signal_platform_kill(
        &self,
        _process_id: ProcessId,
        _process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        Err(PlatformKillAdapterError::Platform(
            "posix platform kill adapter unavailable on windows",
        ))
    }
}

/// Windows adapter that signals real OS processes via `TerminateProcess`.
///
/// NLOS [`ProcessId`] values are authority-assigned identifiers; callers must
/// inject the host pid mapping explicitly (typically from a process
/// supervisor). The workspace forbids `unsafe`, so this adapter uses
/// `taskkill /F` (which invokes `TerminateProcess` under the hood) rather
/// than binding Win32 directly. The mapping is held and consulted on Windows
/// hosts only; on other hosts the constructor discards it and signaling
/// fails closed, so the type stays nameable cross-platform.
#[derive(Debug)]
pub struct WindowsPlatformKillAdapter {
    #[cfg(windows)]
    pid_map: HashMap<ProcessId, u32>,
}

#[cfg(windows)]
impl WindowsPlatformKillAdapter {
    /// Creates an adapter backed by the supplied `ProcessId` → OS pid map.
    #[must_use]
    pub fn new(pid_map: HashMap<ProcessId, u32>) -> Self {
        Self { pid_map }
    }
}

#[cfg(not(windows))]
impl WindowsPlatformKillAdapter {
    /// Creates an adapter that discards the supplied map: signaling always
    /// fails closed on non-Windows hosts, so no mapping is ever consulted.
    #[must_use]
    pub fn new(_pid_map: HashMap<ProcessId, u32>) -> Self {
        Self {}
    }
}

#[cfg(windows)]
impl PlatformKillAdapter for WindowsPlatformKillAdapter {
    fn signal_platform_kill(
        &self,
        process_id: ProcessId,
        _process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        let os_pid = self
            .pid_map
            .get(&process_id)
            .ok_or(PlatformKillAdapterError::Platform(
                "os pid mapping not found for process id",
            ))?;

        let output = std::process::Command::new("taskkill")
            .args(["/PID", &os_pid.to_string(), "/F", "/T"])
            .output()
            .map_err(|_| {
                PlatformKillAdapterError::Platform("taskkill spawn failed for mapped os pid")
            })?;

        if output.status.success() {
            return Ok(PlatformKillAdapterOutcome::Signaled);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(128)
            || stderr.contains("not found")
            || stderr.contains("ERROR: The process") && stderr.contains("not found")
        {
            return Ok(PlatformKillAdapterOutcome::AlreadyTerminated);
        }

        Err(PlatformKillAdapterError::Platform(
            "TerminateProcess equivalent failed for mapped os pid",
        ))
    }
}

#[cfg(not(windows))]
impl PlatformKillAdapter for WindowsPlatformKillAdapter {
    fn signal_platform_kill(
        &self,
        _process_id: ProcessId,
        _process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        Err(PlatformKillAdapterError::Platform(
            "windows platform kill adapter unavailable on non-windows",
        ))
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
