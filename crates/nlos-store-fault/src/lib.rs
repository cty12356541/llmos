//! Test-only fault-injection `SQLite` VFS shim for `nlos-store`.
//!
//! The shim wraps the process-default VFS: every method is delegated
//! unchanged, except that opened files get a swapped `sqlite3_io_methods`
//! table whose `xWrite`/`xSync`/`xTruncate` first consult a process-global
//! fault state machine (screenpipe `failpoint_vfs.rs` pattern). This crate
//! must never appear on a production path; it is only a dev-dependency of
//! `nlos-store`.
//!
//! Fault state is **process-global** (VFS callbacks are plain C function
//! pointers and cannot carry a Rust handle). Tests using this crate must
//! therefore serialize against each other, e.g. with a shared `Mutex`.
//!
//! `unsafe` is confined to the `shim` module; the entire public API is safe.

mod shim;
mod state;

use std::sync::atomic::Ordering;

/// Hard failure codes injected by [`FaultMode::FailWritesAfter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultCode {
    /// `SQLITE_IOERR` (fail-closed; SQLite cannot ignore it, unlike
    /// `SQLITE_IOERR_SHORT_READ`).
    IoErr,
    /// `SQLITE_FULL`.
    Full,
}

/// Process-global fault injection mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultMode {
    /// All I/O passes through unchanged.
    Disabled,
    /// The first `remaining` `xWrite` calls succeed; every `xWrite` after
    /// that fails with `code` until [`disarm`].
    FailWritesAfter { remaining: u64, code: FaultCode },
    /// The first `remaining` `xWrite` calls succeed; after that the machine
    /// is "powered off": `xWrite`/`xSync`/`xTruncate` are silently dropped
    /// (reported successful but never reach disk) until [`disarm`].
    PowerLossAfter { remaining: u64 },
}

/// Registers the shim VFS under `vfs_name` (process-global, once).
///
/// The first successful call wins: later calls with the same name are
/// no-ops, later calls with a different name return
/// [`FaultVfsError::NameMismatch`].
///
/// # Errors
///
/// Returns an error when the name is invalid or already taken by a previous
/// registration, when no default VFS exists, when the default VFS cannot be
/// probed for its I/O methods, or when `SQLite` rejects the registration.
pub fn register(vfs_name: &'static str) -> Result<(), FaultVfsError> {
    shim::register(vfs_name)
}

/// Arms the fault state machine. Resets [`writes_observed`] to zero.
pub fn arm(mode: FaultMode) {
    state::arm(mode);
}

/// Disarms the fault state machine; all I/O passes through again.
pub fn disarm() {
    state::arm(FaultMode::Disabled);
}

/// Number of `xWrite` calls observed while armed since the last [`arm`].
#[must_use]
pub fn writes_observed() -> u64 {
    state::WRITES_OBSERVED.load(Ordering::Acquire)
}

/// Errors returned by [`register`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultVfsError {
    /// The VFS name contains an interior NUL byte.
    InvalidName,
    /// A different name was already registered in this process.
    NameMismatch,
    /// `sqlite3_vfs_find(NULL)` returned no default VFS.
    NoDefaultVfs,
    /// `sqlite3_vfs_register` rejected the shim.
    RegisterFailed(i32),
}

impl std::fmt::Display for FaultVfsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName => formatter.write_str("VFS name contains an interior NUL byte"),
            Self::NameMismatch => {
                formatter.write_str("a different fault VFS name is already registered")
            }
            Self::NoDefaultVfs => formatter.write_str("no default SQLite VFS found"),
            Self::RegisterFailed(code) => {
                write!(
                    formatter,
                    "sqlite3_vfs_register failed with SQLite code {code}"
                )
            }
        }
    }
}

impl std::error::Error for FaultVfsError {}
