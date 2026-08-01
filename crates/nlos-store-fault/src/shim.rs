//! Shim VFS implementation — the single `unsafe` audit unit of the workspace.
//!
//! Audit boundary (screenpipe `failpoint_vfs.rs` pattern):
//! - The shim `sqlite3_vfs` is a byte copy of the process-default VFS with a
//!   different name and an overridden `xOpen`; every other method is
//!   inherited untouched.
//! - `xOpen` delegates to the default VFS's real `xOpen`, then replaces the
//!   returned file's `pMethods` with a per-file [`ShimFile`] copy of that
//!   file's own I/O-methods table, overriding only
//!   `xClose`/`xWrite`/`xSync`/`xTruncate`. A single process-wide copy is
//!   NOT sufficient: the default VFS picks different methods tables per file
//!   kind (e.g. `nolock` variants for temporary files), and only per-file
//!   copying preserves each file's locking/close semantics.
//! - The overridden methods consult the atomic state machine in `state.rs`
//!   and otherwise forward to the original per-file function pointers kept
//!   inside [`ShimFile`]. `xClose` additionally frees the [`ShimFile`] box,
//!   so nothing leaks once a file is closed.
//! - [`ShimFile`] is `repr(C)` with `methods` as its first field, so the
//!   `pMethods` pointer is also the `ShimFile` pointer; that is the only
//!   layout assumption made. No other lifetimes are invented.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::OnceLock;

use libsqlite3_sys::{
    SQLITE_CANTOPEN, SQLITE_FULL, SQLITE_IOERR, SQLITE_OK, sqlite3_file, sqlite3_int64,
    sqlite3_io_methods, sqlite3_vfs, sqlite3_vfs_find, sqlite3_vfs_register,
};

use crate::FaultVfsError;
use crate::state::{self, WriteDecision};

type CloseFn = unsafe extern "C" fn(*mut sqlite3_file) -> c_int;
type WriteFn =
    unsafe extern "C" fn(*mut sqlite3_file, *const c_void, c_int, sqlite3_int64) -> c_int;
type SyncFn = unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int;
type TruncateFn = unsafe extern "C" fn(*mut sqlite3_file, sqlite3_int64) -> c_int;

/// Per-file I/O-methods override plus the original pointers to forward to.
///
/// `methods` must stay the first field: `sqlite3_file.pMethods` points at it
/// and the shim callbacks recover the whole `ShimFile` from that pointer.
#[repr(C)]
struct ShimFile {
    methods: sqlite3_io_methods,
    real_close: Option<CloseFn>,
    real_write: Option<WriteFn>,
    real_sync: Option<SyncFn>,
    real_truncate: Option<TruncateFn>,
}

struct RegisteredVfs {
    name: &'static str,
    parent: *mut sqlite3_vfs,
}

// The raw pointer identifies the SQLite-owned default VFS, which is valid
// for the process lifetime and only ever read by the callbacks.
unsafe impl Send for RegisteredVfs {}
unsafe impl Sync for RegisteredVfs {}

static REGISTRATION: OnceLock<Result<RegisteredVfs, FaultVfsError>> = OnceLock::new();

pub(crate) fn register(vfs_name: &'static str) -> Result<(), FaultVfsError> {
    match REGISTRATION.get_or_init(|| install(vfs_name)) {
        Ok(registered) if registered.name == vfs_name => Ok(()),
        Ok(_) => Err(FaultVfsError::NameMismatch),
        Err(error) => Err(*error),
    }
}

fn install(vfs_name: &'static str) -> Result<RegisteredVfs, FaultVfsError> {
    let z_name = CString::new(vfs_name).map_err(|_| FaultVfsError::InvalidName)?;

    // SAFETY: `sqlite3_vfs_find(NULL)` returns the SQLite-owned default VFS;
    // the pointer is valid for the process lifetime.
    let parent = unsafe { sqlite3_vfs_find(ptr::null()) };
    if parent.is_null() {
        return Err(FaultVfsError::NoDefaultVfs);
    }
    // SAFETY: non-null pointer returned by SQLite above.
    let parent_ref = unsafe { &*parent };

    let shim = Box::leak(Box::new(sqlite3_vfs {
        zName: z_name.into_raw(),
        xOpen: Some(shim_x_open),
        ..*parent_ref
    }));
    // SAFETY: `shim` points to a valid, leaked, fully initialized
    // `sqlite3_vfs` that outlives the registration.
    let registered = unsafe { sqlite3_vfs_register(shim, 0) };
    if registered != SQLITE_OK {
        return Err(FaultVfsError::RegisterFailed(registered));
    }

    Ok(RegisteredVfs {
        name: vfs_name,
        parent,
    })
}

/// Recovers the per-file override record from a file whose `pMethods` was
/// swapped by `shim_x_open`.
fn shim_of(file: *mut sqlite3_file) -> *mut ShimFile {
    // SAFETY: only files opened through `shim_x_open` carry our methods
    // table, and for those `pMethods` points at `ShimFile.methods`, the
    // first field of a `repr(C)` struct — hence also at the `ShimFile`.
    unsafe { (*file).pMethods.cast_mut().cast::<ShimFile>() }
}

fn fault_code(code: u8) -> c_int {
    match code {
        state::CODE_FULL => SQLITE_FULL,
        _ => SQLITE_IOERR,
    }
}

unsafe extern "C" fn shim_x_open(
    _vfs: *mut sqlite3_vfs,
    z_name: *const c_char,
    file: *mut sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    let Some(Ok(registered)) = REGISTRATION.get() else {
        return SQLITE_CANTOPEN;
    };
    let parent = registered.parent;
    // SAFETY: `parent` is the SQLite-owned default VFS, valid for the
    // process lifetime.
    let Some(parent_open) = (unsafe { &*parent }).xOpen else {
        return SQLITE_CANTOPEN;
    };
    // SAFETY: all pointers originate from SQLite's VFS dispatch and satisfy
    // the VFS contract; `parent` is the VFS the methods belong to.
    let opened = unsafe { parent_open(parent, z_name, file, flags, out_flags) };
    if opened != SQLITE_OK {
        return opened;
    }

    // SAFETY: on success SQLite requires the callee to have initialized the
    // file; we only read its methods table and swap in our own copy.
    let file_ref = unsafe { &mut *file };
    let real = file_ref.pMethods;
    if real.is_null() {
        return opened;
    }
    // SAFETY: non-null `pMethods` of a successfully opened file.
    let real_ref = unsafe { &*real };
    let shim = Box::leak(Box::new(ShimFile {
        methods: sqlite3_io_methods {
            xClose: Some(shim_x_close),
            xWrite: Some(shim_x_write),
            xSync: Some(shim_x_sync),
            xTruncate: Some(shim_x_truncate),
            ..*real_ref
        },
        real_close: real_ref.xClose,
        real_write: real_ref.xWrite,
        real_sync: real_ref.xSync,
        real_truncate: real_ref.xTruncate,
    }));
    file_ref.pMethods = &raw const shim.methods;
    opened
}

unsafe extern "C" fn shim_x_close(file: *mut sqlite3_file) -> c_int {
    let shim = shim_of(file);
    // SAFETY: `shim` points to a live boxed `ShimFile` installed by
    // `shim_x_open`; this callback runs at most once per file.
    let real_close = unsafe { &*shim }.real_close;
    let closed = match real_close {
        // SAFETY: `file` satisfies the io-methods contract; `real_close` is
        // this file's own original `xClose`.
        Some(close) => unsafe { close(file) },
        None => SQLITE_OK,
    };
    // SAFETY: `shim` came from `Box::leak` in `shim_x_open` and this is the
    // file's only `xClose`, so the box is reclaimed exactly once.
    drop(unsafe { Box::from_raw(shim) });
    closed
}

unsafe extern "C" fn shim_x_write(
    file: *mut sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: sqlite3_int64,
) -> c_int {
    match state::decide_write() {
        WriteDecision::Passthrough => {
            // SAFETY: `shim_of` yields a live `ShimFile` for files opened
            // through `shim_x_open`.
            let real_write = unsafe { &*shim_of(file) }.real_write;
            match real_write {
                // SAFETY: `file`/`buffer` come from SQLite and satisfy the
                // io-methods contract; `real_write` is this file's own
                // original `xWrite`.
                Some(write) => unsafe { write(file, buffer, amount, offset) },
                None => SQLITE_IOERR,
            }
        }
        WriteDecision::Fail(code) => fault_code(code),
        WriteDecision::Drop => SQLITE_OK,
    }
}

unsafe extern "C" fn shim_x_sync(file: *mut sqlite3_file, flags: c_int) -> c_int {
    if state::sync_or_truncate_dropped() {
        return SQLITE_OK;
    }
    // SAFETY: see `shim_x_write`.
    let real_sync = unsafe { &*shim_of(file) }.real_sync;
    match real_sync {
        // SAFETY: `file` comes from SQLite; `real_sync` is this file's own
        // original `xSync`.
        Some(sync) => unsafe { sync(file, flags) },
        None => SQLITE_IOERR,
    }
}

unsafe extern "C" fn shim_x_truncate(file: *mut sqlite3_file, size: sqlite3_int64) -> c_int {
    if state::sync_or_truncate_dropped() {
        return SQLITE_OK;
    }
    // SAFETY: see `shim_x_write`.
    let real_truncate = unsafe { &*shim_of(file) }.real_truncate;
    match real_truncate {
        // SAFETY: `file` comes from SQLite; `real_truncate` is this file's
        // own original `xTruncate`.
        Some(truncate) => unsafe { truncate(file, size) },
        None => SQLITE_IOERR,
    }
}
