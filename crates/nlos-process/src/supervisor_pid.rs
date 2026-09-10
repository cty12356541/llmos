//! Supervisor pid registry minimum prefix (W22-P).
//!
//! A managed in-memory `ProcessId` → OS pid registry that closes the first
//! gap behind the caller-injected `HashMap<ProcessId, u32>` of
//! [`PosixPlatformKillAdapter`](crate::PosixPlatformKillAdapter) and
//! [`WindowsPlatformKillAdapter`](crate::WindowsPlatformKillAdapter): the
//! supervisor registers the host pid it observed for an authority-assigned
//! `ProcessId` at a concrete [`Generation`], and the registry's
//! [`SupervisorPidRegistry::pid_map`] snapshot feeds both adapters directly.
//!
//! Generation semantics mirror the `ProcessAuthority` fence family: the
//! current mapping advances monotonically (`SupervisorPidDecision::Superseded`
//! for a strictly newer generation, the `restore_process` analogue), an exact
//! same-generation replay returns the original entry
//! (`SupervisorPidDecision::Replayed`), and every other presentation fails
//! closed with zero side effect (stale generation, or a same-generation OS
//! pid rebind). The registry mints no generations and touches no OS state.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use nlos_types::{Generation, ProcessId};

/// One request to bind an OS pid to an authority-assigned Process identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterSupervisorPidRequest {
    pub process_id: ProcessId,
    /// The process binding generation the supervisor observed as current.
    pub process_generation: Generation,
    pub os_pid: u32,
    pub registered_at_ms: u64,
}

/// One immutable registry row: the Process identity, the generation it was
/// registered under, and the host OS pid the supervisor observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorPidEntry {
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub os_pid: u32,
    pub registered_at_ms: u64,
}

/// Outcome of one supervised pid registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorPidDecision {
    /// The mapping is new.
    Registered(SupervisorPidEntry),
    /// The exact same (process, generation, os pid) replayed; the original
    /// entry (including its original `registered_at_ms`) is returned.
    Replayed(SupervisorPidEntry),
    /// A strictly newer generation took over the mapping; the superseded
    /// entry is returned alongside the current one.
    Superseded {
        previous: SupervisorPidEntry,
        current: SupervisorPidEntry,
    },
}

impl SupervisorPidDecision {
    /// The mapping in effect after the registration.
    #[must_use]
    pub const fn current(&self) -> &SupervisorPidEntry {
        match self {
            Self::Registered(entry)
            | Self::Replayed(entry)
            | Self::Superseded { current: entry, .. } => entry,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum SupervisorPidRegistryError {
    /// No mapping is registered for the presented Process identity.
    ProcessNotRegistered(ProcessId),
    /// The registration presented a strictly older generation than the
    /// current mapping (stale replay; fail-closed, zero side effect).
    StaleProcessGeneration {
        process_id: ProcessId,
        current: Generation,
        presented: Generation,
    },
    /// The same generation presented a different OS pid; one process
    /// generation owns exactly one OS pid (fail-closed, zero side effect).
    OsPidRebind {
        process_id: ProcessId,
        generation: Generation,
        registered: u32,
        presented: u32,
    },
    LockPoisoned,
}

impl fmt::Display for SupervisorPidRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProcessNotRegistered(id) => {
                write!(
                    formatter,
                    "no os pid mapping is registered for process {id:?}"
                )
            }
            Self::StaleProcessGeneration {
                current, presented, ..
            } => write!(
                formatter,
                "stale os pid registration: presented generation {} \
                 is older than the current mapping generation {}",
                presented.get(),
                current.get()
            ),
            Self::OsPidRebind {
                registered,
                presented,
                ..
            } => write!(
                formatter,
                "os pid rebind rejected for the same process generation: \
                 registered {registered}, presented {presented}"
            ),
            Self::LockPoisoned => formatter.write_str("supervisor pid registry lock is poisoned"),
        }
    }
}

impl Error for SupervisorPidRegistryError {}

/// Managed `ProcessId` → OS pid registry with `ProcessAuthority`-style
/// generation fencing (module doc for the full contract).
#[derive(Debug, Default)]
pub struct SupervisorPidRegistry {
    entries: Mutex<HashMap<ProcessId, SupervisorPidEntry>>,
}

impl SupervisorPidRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the OS pid the supervisor observed for `process_id` at
    /// `process_generation`. Fail-closed gate order, all before any write:
    ///
    /// 1. no current mapping → [`SupervisorPidDecision::Registered`];
    /// 2. same generation and same OS pid →
    ///    [`SupervisorPidDecision::Replayed`] with the original entry;
    /// 3. same generation and a different OS pid →
    ///    [`SupervisorPidRegistryError::OsPidRebind`];
    /// 4. strictly newer generation →
    ///    [`SupervisorPidDecision::Superseded`] (the `restore_process`
    ///    analogue: the mapping advances with the authority head);
    /// 5. strictly older generation →
    ///    [`SupervisorPidRegistryError::StaleProcessGeneration`].
    ///
    /// # Errors
    ///
    /// Fails closed on a stale generation, a same-generation OS pid rebind,
    /// or a poisoned registry lock; rejections have zero side effect.
    pub fn register(
        &self,
        request: RegisterSupervisorPidRequest,
    ) -> Result<SupervisorPidDecision, SupervisorPidRegistryError> {
        let mut entries = self.lock()?;
        let entry = SupervisorPidEntry {
            process_id: request.process_id,
            process_generation: request.process_generation,
            os_pid: request.os_pid,
            registered_at_ms: request.registered_at_ms,
        };
        let Some(&current) = entries.get(&request.process_id) else {
            entries.insert(request.process_id, entry);
            return Ok(SupervisorPidDecision::Registered(entry));
        };
        if current.process_generation == request.process_generation {
            if current.os_pid == request.os_pid {
                // Exact replay: the original entry (with its original
                // `registered_at_ms`) stays in place untouched.
                return Ok(SupervisorPidDecision::Replayed(current));
            }
            return Err(SupervisorPidRegistryError::OsPidRebind {
                process_id: request.process_id,
                generation: request.process_generation,
                registered: current.os_pid,
                presented: request.os_pid,
            });
        }
        if request.process_generation < current.process_generation {
            return Err(SupervisorPidRegistryError::StaleProcessGeneration {
                process_id: request.process_id,
                current: current.process_generation,
                presented: request.process_generation,
            });
        }
        entries.insert(request.process_id, entry);
        Ok(SupervisorPidDecision::Superseded {
            previous: current,
            current: entry,
        })
    }

    /// Removes the mapping for `process_id` when it is still registered at
    /// `expected_process_generation`. Removing an already-absent mapping is
    /// the idempotent `false`; a stale generation fails closed.
    ///
    /// # Errors
    ///
    /// Fails closed on a stale generation or a poisoned registry lock.
    pub fn unregister(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<bool, SupervisorPidRegistryError> {
        let mut entries = self.lock()?;
        let current = entries
            .get(&process_id)
            .map(|entry: &SupervisorPidEntry| entry.process_generation);
        match current {
            None => Ok(false),
            Some(generation) if generation == expected_process_generation => {
                // The guard above proved the row exists at this generation.
                entries.remove(&process_id);
                Ok(true)
            }
            Some(current) => Err(SupervisorPidRegistryError::StaleProcessGeneration {
                process_id,
                current,
                presented: expected_process_generation,
            }),
        }
    }

    /// Reads the current mapping for `process_id` (read-only snapshot; the
    /// entry is `Copy`).
    ///
    /// # Errors
    ///
    /// Fails with [`SupervisorPidRegistryError::ProcessNotRegistered`] on a
    /// miss, or on a poisoned registry lock.
    pub fn lookup(
        &self,
        process_id: ProcessId,
    ) -> Result<SupervisorPidEntry, SupervisorPidRegistryError> {
        let entries = self.lock()?;
        entries
            .get(&process_id)
            .copied()
            .ok_or(SupervisorPidRegistryError::ProcessNotRegistered(process_id))
    }

    /// Returns the `ProcessId` → OS pid snapshot in the exact
    /// `HashMap<ProcessId, u32>` shape
    /// [`PosixPlatformKillAdapter::new`](crate::PosixPlatformKillAdapter::new)
    /// and
    /// [`WindowsPlatformKillAdapter::new`](crate::WindowsPlatformKillAdapter::new)
    /// accept.
    #[must_use]
    pub fn pid_map(&self) -> HashMap<ProcessId, u32> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(process_id, entry)| (*process_id, entry.os_pid))
            .collect()
    }

    fn lock(
        &self,
    ) -> Result<MutexGuard<'_, HashMap<ProcessId, SupervisorPidEntry>>, SupervisorPidRegistryError>
    {
        self.entries
            .lock()
            .map_err(|_| SupervisorPidRegistryError::LockPoisoned)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Generation, ProcessId, RegisterSupervisorPidRequest, SupervisorPidDecision,
        SupervisorPidRegistry, SupervisorPidRegistryError,
    };

    fn register_request(
        process_id: ProcessId,
        process_generation: Generation,
        os_pid: u32,
    ) -> RegisterSupervisorPidRequest {
        RegisterSupervisorPidRequest {
            process_id,
            process_generation,
            os_pid,
            registered_at_ms: 7,
        }
    }

    #[test]
    fn display_names_every_failure_class() {
        let process_id = ProcessId::from_bytes([0x11; 16]);
        let next = Generation::INITIAL.checked_next().expect("next");
        let cases = [
            SupervisorPidRegistryError::ProcessNotRegistered(process_id),
            SupervisorPidRegistryError::StaleProcessGeneration {
                process_id,
                current: next,
                presented: Generation::INITIAL,
            },
            SupervisorPidRegistryError::OsPidRebind {
                process_id,
                generation: Generation::INITIAL,
                registered: 1,
                presented: 2,
            },
            SupervisorPidRegistryError::LockPoisoned,
        ];
        for case in cases {
            assert!(!case.to_string().is_empty());
        }
    }

    #[test]
    fn replay_preserves_the_original_registered_at_ms() {
        let registry = SupervisorPidRegistry::new();
        let process_id = ProcessId::from_bytes([0x12; 16]);
        let mut first = register_request(process_id, Generation::INITIAL, 4242);
        first.registered_at_ms = 100;
        let mut replay = register_request(process_id, Generation::INITIAL, 4242);
        replay.registered_at_ms = 200;

        let registered = registry.register(first).expect("first");
        let replayed = registry.register(replay).expect("replay");

        assert!(matches!(registered, SupervisorPidDecision::Registered(_)));
        assert_eq!(replayed.current().registered_at_ms, 100);
    }
}
