//! Platform kill adapter contract (contract-layer minimum prefix).
//!
//! Contract tests use [`StubPlatformKillAdapter`] or [`NoopPlatformKillAdapter`].
//! Unix hosts may inject [`PosixPlatformKillAdapter`] with an explicit
//! `ProcessId` → OS pid map when signaling real child processes. Windows
//! hosts may inject [`WindowsPlatformKillAdapter`] with the same map shape.
//!
//! Every host adapter accepts two mapping shapes (W59-2 / evaluation F7):
//! the legacy generation-blind `ProcessId` → OS pid map ([`new`]), and the
//! generation-fenced `(ProcessId, Generation)` → OS pid map
//! ([`PosixPlatformKillAdapter::with_generation_pid_map`] /
//! [`WindowsPlatformKillAdapter::with_generation_pid_map`], fed by
//! [`crate::SupervisorPidRegistry::generation_pid_map`]). Only the fenced
//! shape consults the kill's `process_generation`: a blind map resolves
//! the pid by identity alone and cannot defend against OS pid reuse, so
//! callers that must not signal a reused pid use the fenced shape or the
//! authority-side pre-signal fence
//! ([`crate::ProcessAuthority::request_platform_kill_with_registry`]).

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
/// identity. This crate's authority path durably records the receipt before
/// invoking the adapter, and re-invokes the adapter on every idempotent
/// replay of that receipt (at-least-once signal delivery), so
/// implementations must tolerate being called more than once per kill:
/// re-signaling an already-dead process maps to
/// [`PlatformKillAdapterOutcome::AlreadyTerminated`], never an error.
/// Separately, the fenced authority entry point
/// [`crate::ProcessAuthority::request_platform_kill_with_registry`] may
/// decline to invoke the adapter at all when the supervisor pid registry
/// fence rejects the target generation (W59-2 / evaluation F7); that
/// pre-signal gate lives above this trait and does not relax the
/// at-least-once contract for invocations that do happen.
pub trait PlatformKillAdapter {
    /// Signals the host platform to kill the OS process backing `process_id`
    /// at `process_generation`. Every call — fresh or replay — issues the
    /// signal; [`PlatformKillAdapterOutcome::AlreadyTerminated`] reports
    /// success when the target already died. Failures propagate to the
    /// caller; the durable kill receipt remains committed, so the caller's
    /// retry replays and signals again (at-least-once semantics).
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

/// The two caller-injectable pid mapping shapes every host adapter accepts
/// (W59-2 / evaluation F7). The blind shape is the legacy
/// `HashMap<ProcessId, u32>`; the fenced shape keys the map on the full
/// `(ProcessId, Generation)` fence so a kill presented at the wrong
/// generation resolves no pid at all.
#[cfg(any(unix, windows))]
#[derive(Debug)]
enum SupervisedPidMap {
    /// Legacy `new` shape: resolves by `ProcessId` alone (generation-blind;
    /// cannot defend against OS pid reuse).
    GenerationBlind(HashMap<ProcessId, u32>),
    /// Fenced shape: resolves by `(ProcessId, Generation)`; a generation
    /// miss is a typed fail-closed mapping error.
    GenerationFenced(HashMap<(ProcessId, Generation), u32>),
}

#[cfg(any(unix, windows))]
impl SupervisedPidMap {
    /// Resolves the OS pid for `(process_id, process_generation)`, with a
    /// shape-specific reason when no mapping answers.
    fn resolve(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
    ) -> Result<u32, PlatformKillAdapterError> {
        match self {
            Self::GenerationBlind(map) => {
                map.get(&process_id)
                    .copied()
                    .ok_or(PlatformKillAdapterError::Platform(
                        "os pid mapping not found for process id",
                    ))
            }
            Self::GenerationFenced(map) => map
                .get(&(process_id, process_generation))
                .copied()
                .ok_or(PlatformKillAdapterError::Platform(
                    "os pid mapping not found for process id at process generation",
                )),
        }
    }
}

/// Unix adapter that signals real OS processes via `kill(2)` and SIGTERM.
///
/// NLOS [`ProcessId`] values are authority-assigned identifiers; callers must
/// inject the host pid mapping explicitly (typically from a process
/// supervisor). Two mapping shapes are accepted: the legacy generation-blind
/// `ProcessId` → OS pid map ([`Self::new`]) and the generation-fenced
/// `(ProcessId, Generation)` → OS pid map
/// ([`Self::with_generation_pid_map`], fed by
/// [`crate::SupervisorPidRegistry::generation_pid_map`]) which consults the
/// kill's `process_generation` and fails closed on a generation miss — the
/// OS pid reuse fence evaluation F7 asked for. The mapping is held and
/// consulted on Unix hosts only; on other hosts the constructors discard it
/// and signaling fails closed, so the type stays nameable cross-platform.
#[derive(Debug)]
pub struct PosixPlatformKillAdapter {
    #[cfg(unix)]
    pid_map: SupervisedPidMap,
}

#[cfg(unix)]
impl PosixPlatformKillAdapter {
    /// Creates an adapter backed by the supplied generation-blind
    /// `ProcessId` → OS pid map. The map resolves by identity alone; OS pid
    /// reuse between snapshot and signal is the caller's fence to hold (see
    /// [`Self::with_generation_pid_map`] for the fenced shape).
    #[must_use]
    pub fn new(pid_map: HashMap<ProcessId, u32>) -> Self {
        Self {
            pid_map: SupervisedPidMap::GenerationBlind(pid_map),
        }
    }

    /// Creates an adapter backed by the supplied
    /// `(ProcessId, Generation)` → OS pid map (the
    /// [`crate::SupervisorPidRegistry::generation_pid_map`] shape). Every
    /// signal resolves the pid through the full generation fence: a kill
    /// presented at any other generation finds no mapping and fails closed,
    /// so a reused OS pid cannot be signaled on behalf of a stale
    /// generation.
    #[must_use]
    pub fn with_generation_pid_map(pid_map: HashMap<(ProcessId, Generation), u32>) -> Self {
        Self {
            pid_map: SupervisedPidMap::GenerationFenced(pid_map),
        }
    }
}

#[cfg(not(unix))]
impl PosixPlatformKillAdapter {
    /// Creates an adapter that discards the supplied generation-blind map:
    /// signaling always fails closed on non-Unix hosts, so no mapping is
    /// ever consulted.
    #[must_use]
    pub fn new(_pid_map: HashMap<ProcessId, u32>) -> Self {
        Self {}
    }

    /// Creates an adapter that discards the supplied generation-fenced map:
    /// signaling always fails closed on non-Unix hosts, so no mapping is
    /// ever consulted.
    #[must_use]
    pub fn with_generation_pid_map(_pid_map: HashMap<(ProcessId, Generation), u32>) -> Self {
        Self {}
    }
}

#[cfg(unix)]
impl PlatformKillAdapter for PosixPlatformKillAdapter {
    fn signal_platform_kill(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let os_pid = self.pid_map.resolve(process_id, process_generation)?;
        match kill(Pid::from_raw(os_pid.cast_signed()), Signal::SIGTERM) {
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
/// supervisor). Two mapping shapes are accepted: the legacy generation-blind
/// `ProcessId` → OS pid map ([`Self::new`]) and the generation-fenced
/// `(ProcessId, Generation)` → OS pid map
/// ([`Self::with_generation_pid_map`], fed by
/// [`crate::SupervisorPidRegistry::generation_pid_map`]) which consults the
/// kill's `process_generation` and fails closed on a generation miss — the
/// OS pid reuse fence evaluation F7 asked for. The workspace forbids
/// `unsafe`, so this adapter uses `taskkill /F` (which invokes
/// `TerminateProcess` under the hood) rather than binding Win32 directly.
/// The mapping is held and consulted on Windows hosts only; on other hosts
/// the constructors discard it and signaling fails closed, so the type
/// stays nameable cross-platform.
#[derive(Debug)]
pub struct WindowsPlatformKillAdapter {
    #[cfg(windows)]
    pid_map: SupervisedPidMap,
}

#[cfg(windows)]
impl WindowsPlatformKillAdapter {
    /// Creates an adapter backed by the supplied generation-blind
    /// `ProcessId` → OS pid map. The map resolves by identity alone; OS pid
    /// reuse between snapshot and signal is the caller's fence to hold (see
    /// [`Self::with_generation_pid_map`] for the fenced shape).
    #[must_use]
    pub fn new(pid_map: HashMap<ProcessId, u32>) -> Self {
        Self {
            pid_map: SupervisedPidMap::GenerationBlind(pid_map),
        }
    }

    /// Creates an adapter backed by the supplied
    /// `(ProcessId, Generation)` → OS pid map (the
    /// [`crate::SupervisorPidRegistry::generation_pid_map`] shape). Every
    /// signal resolves the pid through the full generation fence: a kill
    /// presented at any other generation finds no mapping and fails closed,
    /// so a reused OS pid cannot be signaled on behalf of a stale
    /// generation.
    #[must_use]
    pub fn with_generation_pid_map(pid_map: HashMap<(ProcessId, Generation), u32>) -> Self {
        Self {
            pid_map: SupervisedPidMap::GenerationFenced(pid_map),
        }
    }
}

#[cfg(not(windows))]
impl WindowsPlatformKillAdapter {
    /// Creates an adapter that discards the supplied generation-blind map:
    /// signaling always fails closed on non-Windows hosts, so no mapping is
    /// ever consulted.
    #[must_use]
    pub fn new(_pid_map: HashMap<ProcessId, u32>) -> Self {
        Self {}
    }

    /// Creates an adapter that discards the supplied generation-fenced map:
    /// signaling always fails closed on non-Windows hosts, so no mapping is
    /// ever consulted.
    #[must_use]
    pub fn with_generation_pid_map(_pid_map: HashMap<(ProcessId, Generation), u32>) -> Self {
        Self {}
    }
}

#[cfg(windows)]
impl PlatformKillAdapter for WindowsPlatformKillAdapter {
    fn signal_platform_kill(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        let os_pid = self.pid_map.resolve(process_id, process_generation)?;

        let output = std::process::Command::new("taskkill")
            .args(["/PID", &os_pid.to_string(), "/F", "/T"])
            .output()
            .map_err(|_| {
                PlatformKillAdapterError::Platform("taskkill spawn failed for mapped os pid")
            })?;

        if output.status.success() {
            return Ok(PlatformKillAdapterOutcome::Signaled);
        }

        // `taskkill` exits 128 when the pid is already gone (locale
        // independent); the stderr conjunct is the English-locale fallback,
        // matched case-insensitively and parenthesized so the `&&` pair
        // stays one `||` operand instead of being swallowed by precedence.
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if output.status.code() == Some(128)
            || (stderr.contains("error:") && stderr.contains("not found"))
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
