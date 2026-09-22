//! Supervisor-driven spawn / suspend / resume / kill chain (B6-4 minimum
//! prefix).
//!
//! [`ProcessSupervisor`] closes the supervisor half behind the
//! caller-injected pid maps of the platform kill adapters and the
//! [`SupervisorPidRegistry`]: it spawns a real host child, registers the
//! observed OS pid under the authority-assigned
//! `(ProcessId, Generation)` fence, and drives suspend / resume / kill over
//! the registered mapping with the registry's monotonic generation fence
//! (a stale presented generation never signals the current mapping's pid).
//!
//! Platform matrix (fail-closed, typed):
//!
//! - spawn / kill are real on Unix and Windows — kill reuses
//!   [`PosixPlatformKillAdapter`] / [`WindowsPlatformKillAdapter`] over a
//!   one-entry map taken from the generation-fenced registry entry (not a
//!   later `pid_map()` snapshot, whose `ProcessId → os_pid` shape drops the
//!   generation), so the supervisor signal path and the
//!   [`crate::ProcessAuthority::request_platform_kill`] durable path share
//!   one adapter implementation per host;
//! - suspend / resume are real on Unix (`SIGSTOP` / `SIGCONT`); Windows has
//!   no safe process-suspend API under the workspace `unsafe_code =
//! "forbid"` ban, so both fail closed with
//!   [`SupervisorError::UnsupportedOnPlatform`] (typed, registered — not an
//!   invented capability);
//! - hosts that are neither Unix nor Windows fail closed on kill the same
//!   way.
//!
//! This host-side chain does not itself write durable state: the authority
//! integration stays with
//! [`crate::ProcessAuthority::request_platform_kill`], whose adapter can be
//! fed by [`SupervisorPidRegistry::pid_map`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::process::{Child, Command};

use nlos_types::{Generation, ProcessId};

#[cfg(any(unix, windows))]
use crate::platform_kill::PlatformKillAdapter;
use crate::supervisor_pid::{
    RegisterSupervisorPidRequest, SupervisorPidDecision, SupervisorPidRegistry,
    SupervisorPidRegistryError,
};

#[cfg(unix)]
use crate::platform_kill::PosixPlatformKillAdapter;
#[cfg(windows)]
use crate::platform_kill::WindowsPlatformKillAdapter;

/// One-shot kill-path interleave hook (integration tests only).
type KillAfterResolveHook = Box<dyn FnOnce(&SupervisorPidRegistry)>;

thread_local! {
    /// One-shot interleaving point for the G1-resolve / G2-supersede TOCTOU
    /// (integration tests only). `kill` takes the hook after the generation
    /// fence resolves and before the adapter is built.
    static KILL_AFTER_RESOLVE: RefCell<Option<KillAfterResolveHook>> =
        const { RefCell::new(None) };
}

/// One supervisor spawn request: the authority-assigned Process identity
/// the spawned child backs, plus the registry bookkeeping timestamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpawnSupervisedRequest {
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub registered_at_ms: u64,
}

/// A successfully spawned and registered host child. The supervisor hands
/// ownership of the child to the caller — it never keeps a nursery, so
/// reaping (and teardown on test failure) stays with the caller.
#[derive(Debug)]
pub struct SupervisedSpawn {
    child: Child,
    decision: SupervisorPidDecision,
}

impl SupervisedSpawn {
    /// The host OS pid of the spawned child (identical to the registered
    /// mapping's pid).
    #[must_use]
    pub fn os_pid(&self) -> u32 {
        self.child.id()
    }

    /// Mutable access for waiting / polling / teardown of the child.
    pub fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    /// The registry decision for the registration this spawn performed.
    #[must_use]
    pub const fn decision(&self) -> &SupervisorPidDecision {
        &self.decision
    }
}

/// Outcome of one supervisor signal (suspend / resume / kill).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorSignalOutcome {
    /// The signal was delivered to the registered OS pid.
    Applied,
    /// The registered OS pid was already terminated (ESRCH-family).
    AlreadyTerminated,
}

#[derive(Debug)]
pub enum SupervisorError {
    /// The pid registry rejected the operation (unknown mapping, stale
    /// generation, or a same-generation rebind during spawn); zero side
    /// effect.
    Registry(SupervisorPidRegistryError),
    /// The host child could not be spawned.
    Spawn(std::io::Error),
    /// The platform kill adapter rejected the supervisor kill (missing
    /// mapping, host signal failure, or wrong-host adapter).
    PlatformKill(crate::platform_kill::PlatformKillAdapterError),
    /// The operation has no real implementation on this host platform;
    /// fail-closed, zero side effect (B6-4: Windows suspend / resume).
    UnsupportedOnPlatform { operation: &'static str },
    /// A Unix signal syscall failed for the registered OS pid.
    Signal(&'static str),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => write!(formatter, "supervisor registry failure: {error}"),
            Self::Spawn(error) => write!(formatter, "supervisor spawn failure: {error}"),
            Self::PlatformKill(error) => write!(formatter, "supervisor kill failure: {error}"),
            Self::UnsupportedOnPlatform { operation } => write!(
                formatter,
                "supervisor {operation} is unsupported on this host platform"
            ),
            Self::Signal(reason) => write!(formatter, "supervisor signal failure: {reason}"),
        }
    }
}

impl Error for SupervisorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Registry(error) => Some(error),
            Self::Spawn(error) => Some(error),
            Self::PlatformKill(error) => Some(error),
            Self::UnsupportedOnPlatform { .. } | Self::Signal(_) => None,
        }
    }
}

/// The host kill adapter selection for [`ProcessSupervisor::kill`]: exactly
/// the platform with a real adapter, per the platform matrix in the module
/// doc.
#[cfg(any(unix, windows))]
#[derive(Debug)]
enum HostKillAdapter {
    #[cfg(unix)]
    Posix(PosixPlatformKillAdapter),
    #[cfg(windows)]
    Windows(WindowsPlatformKillAdapter),
}

#[cfg(any(unix, windows))]
impl PlatformKillAdapter for HostKillAdapter {
    fn signal_platform_kill(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
    ) -> Result<
        crate::platform_kill::PlatformKillAdapterOutcome,
        crate::platform_kill::PlatformKillAdapterError,
    > {
        match self {
            #[cfg(unix)]
            Self::Posix(adapter) => adapter.signal_platform_kill(process_id, process_generation),
            #[cfg(windows)]
            Self::Windows(adapter) => adapter.signal_platform_kill(process_id, process_generation),
        }
    }
}

/// A supervisor over real host children, backed by one
/// [`SupervisorPidRegistry`].
#[derive(Debug, Default)]
pub struct ProcessSupervisor {
    registry: SupervisorPidRegistry,
}

impl ProcessSupervisor {
    /// Creates a supervisor with an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The pid registry backing this supervisor; its `pid_map()` snapshot
    /// feeds the platform kill adapters directly.
    #[must_use]
    pub fn registry(&self) -> &SupervisorPidRegistry {
        &self.registry
    }

    /// Installs a one-shot callback that [`Self::kill`] runs after the
    /// generation fence resolves and before the platform adapter is built.
    ///
    /// Integration tests use this to interleave a `Supersede` between those
    /// two steps (the G1/G2 TOCTOU sequential stale-generation coverage
    /// cannot reach). Production callers must not install a hook.
    #[doc(hidden)]
    pub fn install_kill_after_resolve_hook(
        &self,
        hook: impl FnOnce(&SupervisorPidRegistry) + 'static,
    ) {
        let _ = self;
        KILL_AFTER_RESOLVE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    fn run_kill_after_resolve_hook(&self) {
        if let Some(hook) = KILL_AFTER_RESOLVE.with(|slot| slot.borrow_mut().take()) {
            hook(&self.registry);
        }
    }

    /// Spawns `command` as the host child backing
    /// `(process_id, process_generation)` and registers the observed OS pid
    /// in the supervisor registry.
    ///
    /// Fail-closed on a refused registration (same-generation OS pid rebind
    /// or a stale presented generation): the just-spawned child is killed
    /// and reaped before the rejection surfaces, so a refused spawn never
    /// leaves a half-owned child (the `[PROC-SPAWN-003]` supervisor-side
    /// analogue).
    ///
    /// # Errors
    ///
    /// Fails when the host spawn fails, or when the registry rejects the
    /// registration (the child is then torn down).
    pub fn spawn_supervised(
        &self,
        request: SpawnSupervisedRequest,
        command: &mut Command,
    ) -> Result<SupervisedSpawn, SupervisorError> {
        let mut child = command.spawn().map_err(SupervisorError::Spawn)?;
        let os_pid = child.id();
        match self.registry.register(RegisterSupervisorPidRequest {
            process_id: request.process_id,
            process_generation: request.process_generation,
            os_pid,
            registered_at_ms: request.registered_at_ms,
        }) {
            Ok(decision) => Ok(SupervisedSpawn { child, decision }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(SupervisorError::Registry(error))
            }
        }
    }

    /// Resolves the current registry entry for `process_id` and fails
    /// closed unless the mapping is still at `expected_process_generation` —
    /// a stale presentation must never signal the current mapping's pid.
    #[cfg(any(unix, windows))]
    fn resolve_current_entry(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<crate::supervisor_pid::SupervisorPidEntry, SupervisorError> {
        let entry = self
            .registry
            .lookup(process_id)
            .map_err(SupervisorError::Registry)?;
        if entry.process_generation != expected_process_generation {
            return Err(SupervisorError::Registry(
                SupervisorPidRegistryError::StaleProcessGeneration {
                    process_id,
                    current: entry.process_generation,
                    presented: expected_process_generation,
                },
            ));
        }
        Ok(entry)
    }

    /// Suspends the host child registered for
    /// `(process_id, expected_process_generation)`.
    ///
    /// Real on Unix (`SIGSTOP`); typed fail-closed
    /// ([`SupervisorError::UnsupportedOnPlatform`]) elsewhere.
    ///
    /// # Errors
    ///
    /// Fails closed on an unknown mapping, a stale generation, a host
    /// signal failure, or an unsupported platform.
    #[cfg(unix)]
    pub fn suspend(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<SupervisorSignalOutcome, SupervisorError> {
        let os_pid = self
            .resolve_current_entry(process_id, expected_process_generation)?
            .os_pid;
        signal_unix(os_pid, UnixSignal::Stop)
    }

    /// Suspends the host child registered for
    /// `(process_id, expected_process_generation)` — unsupported off Unix
    /// (see the module's platform matrix).
    ///
    /// # Errors
    ///
    /// Always fails closed with
    /// [`SupervisorError::UnsupportedOnPlatform`] on a non-Unix host.
    #[cfg(not(unix))]
    pub fn suspend(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<SupervisorSignalOutcome, SupervisorError> {
        let _ = (process_id, expected_process_generation);
        Err(SupervisorError::UnsupportedOnPlatform {
            operation: "suspend",
        })
    }

    /// Resumes a suspended host child (`SIGCONT` on Unix; typed
    /// fail-closed elsewhere).
    ///
    /// # Errors
    ///
    /// Same fail-closed family as [`Self::suspend`].
    #[cfg(unix)]
    pub fn resume(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<SupervisorSignalOutcome, SupervisorError> {
        let os_pid = self
            .resolve_current_entry(process_id, expected_process_generation)?
            .os_pid;
        signal_unix(os_pid, UnixSignal::Continue)
    }

    /// Resumes a suspended host child — unsupported off Unix (see the
    /// module's platform matrix).
    ///
    /// # Errors
    ///
    /// Always fails closed with
    /// [`SupervisorError::UnsupportedOnPlatform`] on a non-Unix host.
    #[cfg(not(unix))]
    pub fn resume(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<SupervisorSignalOutcome, SupervisorError> {
        let _ = (process_id, expected_process_generation);
        Err(SupervisorError::UnsupportedOnPlatform {
            operation: "resume",
        })
    }

    /// Kills the host child registered for
    /// `(process_id, expected_process_generation)` through the host's real
    /// platform kill adapter (SIGTERM on Unix, `taskkill /F /T` on Windows),
    /// built from a one-entry map of the fenced entry's `os_pid` — the same
    /// adapter family the durable
    /// [`crate::ProcessAuthority::request_platform_kill`] path accepts. A
    /// later `pid_map()` snapshot is not consulted: that map is
    /// `ProcessId → os_pid` only, so a `Supersede` between resolve and
    /// signal would otherwise deliver the G2 child's pid to a G1 kill.
    ///
    /// # Errors
    ///
    /// Fails closed on an unknown mapping, a stale generation, an adapter
    /// rejection, or a host with no real kill adapter.
    #[cfg(any(unix, windows))]
    pub fn kill(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<SupervisorSignalOutcome, SupervisorError> {
        let entry = self.resolve_current_entry(process_id, expected_process_generation)?;
        self.run_kill_after_resolve_hook();
        let adapter = HostKillAdapter::from_fenced_entry(&entry);
        match adapter.signal_platform_kill(process_id, entry.process_generation) {
            Ok(crate::platform_kill::PlatformKillAdapterOutcome::Signaled) => {
                Ok(SupervisorSignalOutcome::Applied)
            }
            Ok(crate::platform_kill::PlatformKillAdapterOutcome::AlreadyTerminated) => {
                Ok(SupervisorSignalOutcome::AlreadyTerminated)
            }
            Err(error) => Err(SupervisorError::PlatformKill(error)),
        }
    }

    /// Kills the host child registered for
    /// `(process_id, expected_process_generation)` — unsupported on hosts
    /// with no real platform kill adapter (see the module's platform
    /// matrix).
    ///
    /// # Errors
    ///
    /// Always fails closed with
    /// [`SupervisorError::UnsupportedOnPlatform`].
    #[cfg(not(any(unix, windows)))]
    pub fn kill(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<SupervisorSignalOutcome, SupervisorError> {
        let _ = (process_id, expected_process_generation);
        Err(SupervisorError::UnsupportedOnPlatform { operation: "kill" })
    }
}

#[cfg(any(unix, windows))]
impl HostKillAdapter {
    /// Builds the host adapter from the already-fenced registry entry, not
    /// from a later `pid_map()` snapshot (generation-blind `ProcessId →
    /// os_pid`). Same adapter types; no second pid ledger.
    fn from_fenced_entry(entry: &crate::supervisor_pid::SupervisorPidEntry) -> Self {
        let map = HashMap::from([(entry.process_id, entry.os_pid)]);
        #[cfg(unix)]
        {
            Self::Posix(PosixPlatformKillAdapter::new(map))
        }
        #[cfg(windows)]
        {
            Self::Windows(WindowsPlatformKillAdapter::new(map))
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum UnixSignal {
    Stop,
    Continue,
}

#[cfg(unix)]
fn signal_unix(
    os_pid: u32,
    signal: UnixSignal,
) -> Result<SupervisorSignalOutcome, SupervisorError> {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let signal = match signal {
        UnixSignal::Stop => Signal::SIGSTOP,
        UnixSignal::Continue => Signal::SIGCONT,
    };
    match kill(Pid::from_raw(os_pid.cast_signed()), signal) {
        Ok(()) => Ok(SupervisorSignalOutcome::Applied),
        Err(Errno::ESRCH) => Ok(SupervisorSignalOutcome::AlreadyTerminated),
        Err(_) => Err(SupervisorError::Signal(
            "unix signal failed for registered os pid",
        )),
    }
}
