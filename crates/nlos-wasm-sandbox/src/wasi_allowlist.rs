//! The hand-written `wasi_snapshot_preview1` allowlist surface
//! (`ADR-0015` capability convergence; `[TCB-BYPASS-001]`,
//! `[TCB-EGRESS-001]`).
//!
//! The sandbox deliberately does **not** link a general-purpose WASI
//! implementation. Only the imports registered by [`add_to_linker`] exist;
//! every capability decision inside them is deny-by-default:
//!
//! * **Environment**: `environ_sizes_get`/`environ_get` always report an
//!   empty environment — zero host environment variables are ever exposed
//!   at the WASI layer, and the dedicated host process is additionally
//!   spawned with `.env_clear()` (layer 1, see [`crate::process`]).
//! * **Arguments**: only the explicit argv declared in
//!   [`crate::SandboxProfile`] is visible via `args_sizes_get`/`args_get`.
//! * **Filesystem**: the preopen set defaults to empty. Granted preopens are
//!   enforced by a double fence: a lexical fence
//!   ([`normalize_guest_path`] rejects absolute paths, backslashes, and any
//!   `..` escaping the mount) plus a canonicalization fence (`realpath`
//!   containment under the canonicalized host root rejects symlink
//!   escapes). Read-only grants deny every write-intent open with
//!   `ENOTCAPABLE`.
//! * **Network**: no socket imports are registered at all — a guest that
//!   imports `sock_open` (or any other absent import) fails instantiation
//!   with an unknown-import error. There is no code path that could reach
//!   a socket.
//! * **Entropy**: `random_get` is not granted; modules importing it fail
//!   instantiation. (Explicit E6 boundary: cryptographic guest entropy
//!   requires a sanctioned entropy source in a later lane.)
//!
//! `errno` values follow the WASI preview 1 numbering; capability
//! violations are reported as `ENOTCAPABLE` (76).

use std::any::Any;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use wasmtime::{Caller, Extern, Linker, Memory};

use crate::isolate::PageLimiter;
use crate::profile::{PreopenGrant, SandboxProfile};

/// WASI preview 1 errno values used by this surface.
pub(crate) mod errno {
    pub(crate) const SUCCESS: i32 = 0;
    pub(crate) const ACCES: i32 = 2;
    pub(crate) const BADF: i32 = 8;
    pub(crate) const EXIST: i32 = 20;
    pub(crate) const FAULT: i32 = 21;
    pub(crate) const INVAL: i32 = 28;
    pub(crate) const IO: i32 = 29;
    pub(crate) const ISDIR: i32 = 31;
    pub(crate) const MFILE: i32 = 33;
    pub(crate) const NAMETOOLONG: i32 = 37;
    pub(crate) const NOENT: i32 = 44;
    pub(crate) const NOTDIR: i32 = 54;
    pub(crate) const NOTEMPTY: i32 = 55;
    pub(crate) const SPIPE: i32 = 69;
    pub(crate) const NOTCAPABLE: i32 = 76;
}

/// `oflags` bits of `path_open` (preview 1).
mod oflags {
    pub(crate) const CREAT: i32 = 0x1;
    pub(crate) const DIRECTORY: i32 = 0x2;
    pub(crate) const TRUNC: i32 = 0x8;
}

/// `fd_flags` bits of `path_open` (preview 1).
mod fd_flags {
    pub(crate) const APPEND: i32 = 0x1;
}

/// Rights bits whose request implies write intent (preview 1 bit order).
const RIGHTS_WRITE_MASK: i64 = (1 << 6) // fd_write
    | (1 << 8) // fd_allocate
    | (1 << 9) // path_create_directory
    | (1 << 10) // path_create_file
    | (1 << 11) // path_link_source
    | (1 << 12) // path_link_target
    | (1 << 16) // path_rename_source
    | (1 << 17) // path_rename_target
    | (1 << 24) // path_set_times
    | (1 << 25) // fd_filestat_set_size
    | (1 << 26) // fd_filestat_set_times
    | (1 << 27) // path_symlink
    | (1 << 28) // path_remove_directory
    | (1 << 29); // path_unlink_file

/// Hard cap on simultaneously open guest file descriptors (`DoS` hardening).
const MAX_OPEN_FDS: usize = 256;

/// Hard cap on the total bytes of one `iovec` gather/scatter operation.
const MAX_IOV_TOTAL: u64 = 4 << 20;

/// Cooperative guest termination marker raised by `proc_exit`
/// (`[DRV-LIFE-001]` lifecycle terminal state).
#[derive(Debug)]
pub(crate) struct GuestExited {
    /// The exit code the guest requested.
    pub code: i32,
}

impl fmt::Display for GuestExited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "guest requested exit with code {}", self.code)
    }
}

impl Error for GuestExited {}

/// Destination for guest-originated byte I/O. The engine layer never touches
/// process stdio directly; the dedicated host binary wires a [`StdioSink`]
/// and tests wire a [`VecIoSink`].
pub(crate) trait GuestIoSink: Send {
    /// Writes guest bytes emitted via `fd_write` to descriptor `fd` (1 or 2).
    fn write_guest(&mut self, fd: u32, bytes: &[u8]) -> std::io::Result<()>;
    /// Returns this sink for downcasting.
    fn as_any(&mut self) -> &mut dyn Any;
}

/// In-memory sink used by tests; captures guest stdout/stderr.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct VecIoSink {
    /// Captured guest stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured guest stderr bytes.
    pub stderr: Vec<u8>,
}

#[cfg(test)]
impl GuestIoSink for VecIoSink {
    fn write_guest(&mut self, fd: u32, bytes: &[u8]) -> std::io::Result<()> {
        if fd == 2 {
            self.stderr.extend_from_slice(bytes);
        } else {
            self.stdout.extend_from_slice(bytes);
        }
        Ok(())
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Real-stdio sink used by the dedicated host binary.
pub(crate) struct StdioSink {
    /// Total guest stdout bytes emitted (report metric).
    pub guest_stdout_len: u64,
}

impl Default for StdioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl StdioSink {
    /// Creates a sink writing guest stdout to the process stdout and guest
    /// stderr to the process stderr.
    pub(crate) fn new() -> Self {
        Self {
            guest_stdout_len: 0,
        }
    }
}

impl GuestIoSink for StdioSink {
    fn write_guest(&mut self, fd: u32, bytes: &[u8]) -> std::io::Result<()> {
        if fd == 2 {
            return std::io::stderr().write_all(bytes);
        }
        self.guest_stdout_len = self
            .guest_stdout_len
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(0));
        std::io::stdout().write_all(bytes)
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// One entry of the guest-visible file descriptor table.
#[derive(Debug)]
enum FdEntry {
    /// A preopen grant occupying its fixed descriptor.
    Preopen { grant_index: usize },
    /// A regular file opened through `path_open`.
    File { file: std::fs::File, writable: bool },
    /// A directory opened through `path_open` (read-only view, no handle).
    Directory,
}

/// Store state for one guest run: the fd table, explicit argv, granted
/// preopens, the memory-page limiter, and the I/O sink.
pub(crate) struct GuestState {
    /// Linear-memory page limiter (layer-2 hard cap).
    pub limiter: PageLimiter,
    args: Vec<String>,
    preopens: Vec<PreopenGrant>,
    fds: BTreeMap<u32, FdEntry>,
    next_fd: u32,
    sink: Box<dyn GuestIoSink>,
    monotonic_origin: Instant,
}

impl GuestState {
    /// Builds the deny-by-default state from a profile.
    pub(crate) fn new(profile: &SandboxProfile, sink: Box<dyn GuestIoSink>) -> Self {
        let mut fds = BTreeMap::new();
        let mut next_fd = 3;
        for grant_index in 0..profile.preopens().len() {
            fds.insert(next_fd, FdEntry::Preopen { grant_index });
            next_fd += 1;
        }
        Self {
            limiter: PageLimiter::new(profile.max_memory_pages()),
            args: profile.guest_args().to_vec(),
            preopens: profile.preopens().to_vec(),
            fds,
            next_fd,
            sink,
            monotonic_origin: Instant::now(),
        }
    }

    /// Consumes and returns the boxed I/O sink (for result inspection).
    pub(crate) fn into_sink(self) -> Box<dyn GuestIoSink> {
        self.sink
    }

    fn preopen_grant_for_fd(&self, fd: u32) -> Option<&PreopenGrant> {
        match self.fds.get(&fd) {
            Some(FdEntry::Preopen { grant_index }) => self.preopens.get(*grant_index),
            _ => None,
        }
    }

    fn close_fd(&mut self, fd: u32) -> bool {
        match fd {
            0..=2 => true, // reserved stdio: closing is a no-op SUCCESS
            _ => self.fds.remove(&fd).is_some(),
        }
    }

    fn alloc_fd(&mut self) -> Result<u32, i32> {
        if self.fds.len() >= MAX_OPEN_FDS {
            return Err(errno::MFILE);
        }
        loop {
            let candidate = self.next_fd;
            self.next_fd = self.next_fd.saturating_add(1);
            if !self.fds.contains_key(&candidate) {
                return Ok(candidate);
            }
        }
    }
}

/// Lexically normalizes a guest-relative path against the `/` separator.
///
/// Returns the remaining components (empty = the mount root itself), or the
/// WASI errno for the violation: `ENOTCAPABLE` for absolute paths and for
/// `..` escapes past the mount root, `EINVAL` for empty paths, empty
/// components, backslashes (which would be re-interpreted as separators on
/// Windows), and NUL bytes.
pub(crate) fn normalize_guest_path(guest_path: &str) -> Result<Vec<String>, i32> {
    if guest_path.is_empty() || guest_path.contains('\0') || guest_path.contains('\\') {
        return Err(errno::INVAL);
    }
    if guest_path.starts_with('/') {
        return Err(errno::NOTCAPABLE);
    }
    let bytes = guest_path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        // Windows drive-letter pattern such as `C:/Windows/win.ini`.
        return Err(errno::NOTCAPABLE);
    }
    let mut stack: Vec<String> = Vec::new();
    for component in guest_path.split('/') {
        match component {
            "" => return Err(errno::INVAL),
            "." => {}
            ".." => {
                if stack.pop().is_none() {
                    return Err(errno::NOTCAPABLE);
                }
            }
            other => stack.push(other.to_string()),
        }
    }
    Ok(stack)
}

fn io_errno(error: &std::io::Error) -> i32 {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => errno::NOENT,
        ErrorKind::PermissionDenied => errno::ACCES,
        ErrorKind::AlreadyExists => errno::EXIST,
        ErrorKind::InvalidInput => errno::INVAL,
        ErrorKind::IsADirectory => errno::ISDIR,
        ErrorKind::NotADirectory => errno::NOTDIR,
        ErrorKind::DirectoryNotEmpty => errno::NOTEMPTY,
        _ => errno::IO,
    }
}

fn memory_of(caller: &mut Caller<'_, GuestState>) -> Option<Memory> {
    caller.get_export("memory").and_then(Extern::into_memory)
}

/// Clamps a guest-provided i32 offset into a memory-safe value: wild
/// pointers become `i32::MAX`, which the memory access then reports as
/// `EFAULT` instead of panicking on arithmetic overflow.
fn clamped_i32_add(base: i32, delta: i64) -> i32 {
    i32::try_from(i64::from(base) + delta).unwrap_or(i32::MAX)
}

fn write_u32(caller: &mut Caller<'_, GuestState>, memory: &Memory, offset: i32, value: u32) -> i32 {
    let target = usize::try_from(u32::try_from(offset).unwrap_or(u32::MAX)).unwrap_or(usize::MAX);
    match memory.write(&mut *caller, target, &value.to_le_bytes()) {
        Ok(()) => errno::SUCCESS,
        Err(_) => errno::FAULT,
    }
}

fn write_u64(caller: &mut Caller<'_, GuestState>, memory: &Memory, offset: i32, value: u64) -> i32 {
    let target = usize::try_from(u32::try_from(offset).unwrap_or(u32::MAX)).unwrap_or(usize::MAX);
    match memory.write(&mut *caller, target, &value.to_le_bytes()) {
        Ok(()) => errno::SUCCESS,
        Err(_) => errno::FAULT,
    }
}

fn write_bytes(
    caller: &mut Caller<'_, GuestState>,
    memory: &Memory,
    offset: i32,
    bytes: &[u8],
) -> i32 {
    let target = usize::try_from(u32::try_from(offset).unwrap_or(u32::MAX)).unwrap_or(usize::MAX);
    match memory.write(&mut *caller, target, bytes) {
        Ok(()) => errno::SUCCESS,
        Err(_) => errno::FAULT,
    }
}

fn read_bytes(
    caller: &mut Caller<'_, GuestState>,
    memory: &Memory,
    offset: i32,
    len: i32,
) -> Result<Vec<u8>, i32> {
    let start = u32::try_from(offset).map_err(|_| errno::FAULT)?;
    let length = u32::try_from(len).map_err(|_| errno::FAULT)?;
    let target = usize::try_from(start).map_err(|_| errno::FAULT)?;
    let mut buffer = vec![0u8; usize::try_from(length).map_err(|_| errno::INVAL)?];
    memory
        .read(&mut *caller, target, &mut buffer)
        .map_err(|_| errno::FAULT)?;
    Ok(buffer)
}

/// Collects an iovec array (`(ptr: u32, len: u32)` pairs) from guest memory.
fn read_iovs(
    caller: &mut Caller<'_, GuestState>,
    memory: &Memory,
    iovs_ptr: i32,
    iovs_len: i32,
) -> Result<Vec<(u32, u32)>, i32> {
    let count = u64::try_from(iovs_len).map_err(|_| errno::INVAL)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    if count > 64 {
        return Err(errno::INVAL);
    }
    let mut iovs = Vec::new();
    let mut total: u64 = 0;
    for index in 0..count {
        let entry_ptr = clamped_i32_add(iovs_ptr, 8 * i64::try_from(index).unwrap_or(i64::MAX));
        let raw = read_bytes(caller, memory, entry_ptr, 8)?;
        let ptr = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        total += u64::from(len);
        if total > MAX_IOV_TOTAL {
            return Err(errno::INVAL);
        }
        iovs.push((ptr, len));
    }
    Ok(iovs)
}

fn iov_ptr_as_i32(ptr: u32) -> Result<i32, i32> {
    i32::try_from(ptr).map_err(|_| errno::INVAL)
}

/// Registers the whitelisted `wasi_snapshot_preview1` imports.
///
/// This is deliberately the single registration site: a capability exists
/// for guests if and only if it is registered in this one function and
/// nowhere else in the crate.
#[allow(clippy::too_many_lines)] // single auditable allowlist registration site
pub(crate) fn add_to_linker(linker: &mut Linker<GuestState>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "args_sizes_get",
        |mut caller: Caller<'_, GuestState>, argc_ptr: i32, argv_buf_size_ptr: i32| -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            let (count, total) = {
                let state = caller.data();
                let count = u32::try_from(state.args.len()).unwrap_or(u32::MAX);
                let total: usize = state.args.iter().map(|a| a.len() + 1).sum();
                (count, total)
            };
            let total = u32::try_from(total).unwrap_or(u32::MAX);
            let e1 = write_u32(&mut caller, &memory, argc_ptr, count);
            if e1 != errno::SUCCESS {
                return e1;
            }
            write_u32(&mut caller, &memory, argv_buf_size_ptr, total)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "args_get",
        |mut caller: Caller<'_, GuestState>, argv_ptr: i32, argv_buf_ptr: i32| -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            let args = caller.data().args.clone();
            let mut offset: u32 = 0;
            for (index, arg) in args.iter().enumerate() {
                let entry_ptr =
                    clamped_i32_add(argv_ptr, 4 * i64::try_from(index).unwrap_or(i64::MAX));
                let e = write_u32(&mut caller, &memory, entry_ptr, offset);
                if e != errno::SUCCESS {
                    return e;
                }
                let byte_ptr = clamped_i32_add(argv_buf_ptr, i64::from(offset));
                let e = write_bytes(&mut caller, &memory, byte_ptr, arg.as_bytes());
                if e != errno::SUCCESS {
                    return e;
                }
                // NUL terminator follows each argument.
                let nul_ptr = clamped_i32_add(
                    argv_buf_ptr,
                    i64::from(offset) + i64::try_from(arg.len()).unwrap_or(i64::MAX),
                );
                let e = write_bytes(&mut caller, &memory, nul_ptr, &[0]);
                if e != errno::SUCCESS {
                    return e;
                }
                offset = offset
                    .saturating_add(u32::try_from(arg.len()).unwrap_or(u32::MAX))
                    .saturating_add(1);
            }
            errno::SUCCESS
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "environ_sizes_get",
        |mut caller: Caller<'_, GuestState>, environc_ptr: i32, environ_buf_size_ptr: i32| -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            // Zero host environment inheritance ([TCB-BYPASS-001]): the
            // guest environment is empty by construction.
            let e1 = write_u32(&mut caller, &memory, environc_ptr, 0);
            if e1 != errno::SUCCESS {
                return e1;
            }
            write_u32(&mut caller, &memory, environ_buf_size_ptr, 0)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "environ_get",
        |_caller: Caller<'_, GuestState>, _environ_ptr: i32, _environ_buf_ptr: i32| -> i32 {
            // Zero entries: no guest memory is touched, so even wild
            // pointers are observably safe here.
            errno::SUCCESS
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_time_get",
        |mut caller: Caller<'_, GuestState>,
         clock_id: i32,
         _precision: i64,
         time_ptr: i32|
         -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            let nanos = match clock_id {
                0 => match SystemTime::now().duration_since(UNIX_EPOCH) {
                    Ok(d) => u64::try_from(d.as_nanos()).unwrap_or(u64::MAX),
                    Err(_) => return errno::IO,
                },
                1 => {
                    let state = caller.data();
                    u64::try_from(state.monotonic_origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
                }
                _ => return errno::INVAL,
            };
            write_u64(&mut caller, &memory, time_ptr, nanos)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_res_get",
        |mut caller: Caller<'_, GuestState>, clock_id: i32, resolution_ptr: i32| -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            match clock_id {
                0 | 1 => write_u64(&mut caller, &memory, resolution_ptr, 1),
                _ => errno::INVAL,
            }
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "proc_exit",
        |_caller: Caller<'_, GuestState>, code: i32| -> Result<(), wasmtime::Error> {
            Err(wasmtime::Error::new(GuestExited { code }))
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_write",
        |mut caller: Caller<'_, GuestState>,
         fd: i32,
         iovs_ptr: i32,
         iovs_len: i32,
         nwritten_ptr: i32|
         -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            let iovs = match read_iovs(&mut caller, &memory, iovs_ptr, iovs_len) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let Ok(fd) = u32::try_from(fd) else {
                return errno::BADF;
            };
            let mut written: u32 = 0;
            let file_writable = match fd {
                1 | 2 => true,
                _ => match caller.data().fds.get(&fd) {
                    Some(FdEntry::File { writable, .. }) => *writable,
                    _ => return errno::BADF,
                },
            };
            if !file_writable {
                return errno::BADF;
            }
            for (ptr, len) in iovs {
                let ptr_i32 = match iov_ptr_as_i32(ptr) {
                    Ok(p) => p,
                    Err(e) => return e,
                };
                let Ok(len_i32) = i32::try_from(len) else {
                    return errno::INVAL;
                };
                let bytes = match read_bytes(&mut caller, &memory, ptr_i32, len_i32) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                let outcome = match fd {
                    1 | 2 => caller.data_mut().sink.write_guest(fd, &bytes),
                    _ => match caller.data_mut().fds.get_mut(&fd) {
                        Some(FdEntry::File { file, .. }) => file.write_all(&bytes),
                        _ => return errno::BADF,
                    },
                };
                if outcome.is_err() {
                    return errno::IO;
                }
                written = written.saturating_add(len);
            }
            write_u32(&mut caller, &memory, nwritten_ptr, written)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_read",
        |mut caller: Caller<'_, GuestState>,
         fd: i32,
         iovs_ptr: i32,
         iovs_len: i32,
         nread_ptr: i32|
         -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            let iovs = match read_iovs(&mut caller, &memory, iovs_ptr, iovs_len) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let Ok(fd) = u32::try_from(fd) else {
                return errno::BADF;
            };
            if fd == 0 {
                // stdin is never wired into guests: immediate EOF.
                return write_u32(&mut caller, &memory, nread_ptr, 0);
            }
            if matches!(fd, 1 | 2) {
                return errno::BADF;
            }
            let is_file = match caller.data().fds.get(&fd) {
                Some(FdEntry::File { .. }) => true,
                Some(_) => return errno::ISDIR,
                None => return errno::BADF,
            };
            if !is_file {
                return errno::ISDIR;
            }
            let mut read_total: u32 = 0;
            for (ptr, len) in iovs {
                if len == 0 {
                    continue;
                }
                let mut buffer = vec![0u8; usize::try_from(len).unwrap_or(0)];
                let n = {
                    let entry = caller.data_mut().fds.get_mut(&fd);
                    match entry {
                        Some(FdEntry::File { file, .. }) => match file.read(&mut buffer) {
                            Ok(n) => n,
                            Err(_) => return errno::IO,
                        },
                        _ => return errno::BADF,
                    }
                };
                if n == 0 {
                    break;
                }
                let ptr_i32 = match iov_ptr_as_i32(ptr) {
                    Ok(p) => p,
                    Err(e) => return e,
                };
                let e = write_bytes(&mut caller, &memory, ptr_i32, &buffer[..n]);
                if e != errno::SUCCESS {
                    return e;
                }
                read_total = read_total.saturating_add(u32::try_from(n).unwrap_or(0));
            }
            write_u32(&mut caller, &memory, nread_ptr, read_total)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_close",
        |mut caller: Caller<'_, GuestState>, fd: i32| -> i32 {
            let Ok(fd) = u32::try_from(fd) else {
                return errno::BADF;
            };
            if caller.data_mut().close_fd(fd) {
                errno::SUCCESS
            } else {
                errno::BADF
            }
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_seek",
        |mut caller: Caller<'_, GuestState>,
         fd: i32,
         offset: i64,
         whence: i32,
         newoffset_ptr: i32|
         -> i32 {
            use std::io::SeekFrom;
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            let Ok(fd) = u32::try_from(fd) else {
                return errno::BADF;
            };
            if matches!(fd, 0..=2) {
                return errno::SPIPE;
            }
            let seek = match whence {
                0 => match u64::try_from(offset) {
                    Ok(v) => SeekFrom::Start(v),
                    Err(_) => return errno::INVAL,
                },
                1 => SeekFrom::Current(offset),
                2 => SeekFrom::End(offset),
                _ => return errno::INVAL,
            };
            let seek_result = {
                let entry = caller.data_mut().fds.get_mut(&fd);
                match entry {
                    Some(FdEntry::File { file, .. }) => file.seek(seek).map_err(|e| io_errno(&e)),
                    Some(_) => Err(errno::ISDIR),
                    None => Err(errno::BADF),
                }
            };
            match seek_result {
                Ok(pos) => write_u64(&mut caller, &memory, newoffset_ptr, pos),
                Err(e) => e,
            }
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_prestat_get",
        |mut caller: Caller<'_, GuestState>, fd: i32, prestat_ptr: i32| -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            let Ok(fd) = u32::try_from(fd) else {
                return errno::BADF;
            };
            let Some(grant) = caller.data().preopen_grant_for_fd(fd) else {
                return errno::BADF;
            };
            let name_len = u32::try_from(grant.guest_dir().len()).unwrap_or(u32::MAX);
            // prestat layout: u32 tag (0 = directory preopen), u32 name_len.
            let e1 = write_u32(&mut caller, &memory, prestat_ptr, 0);
            if e1 != errno::SUCCESS {
                return e1;
            }
            write_u32(
                &mut caller,
                &memory,
                clamped_i32_add(prestat_ptr, 4),
                name_len,
            )
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_prestat_dir_name",
        |mut caller: Caller<'_, GuestState>, fd: i32, path_ptr: i32, path_len: i32| -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            let Ok(fd) = u32::try_from(fd) else {
                return errno::BADF;
            };
            let Some(grant) = caller.data().preopen_grant_for_fd(fd).cloned() else {
                return errno::BADF;
            };
            let name = grant.guest_dir().as_bytes();
            let Ok(buf_len) = u64::try_from(path_len) else {
                return errno::INVAL;
            };
            if buf_len < u64::try_from(name.len()).unwrap_or(u64::MAX) {
                return errno::NAMETOOLONG;
            }
            write_bytes(&mut caller, &memory, path_ptr, name)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "path_open",
        |mut caller: Caller<'_, GuestState>,
         dirfd: i32,
         _dirflags: i32,
         path_ptr: i32,
         path_len: i32,
         oflags: i32,
         rights_base: i64,
         _rights_inheriting: i64,
         fd_flags: i32,
         opened_fd_ptr: i32|
         -> i32 {
            let Some(memory) = memory_of(&mut caller) else {
                return errno::FAULT;
            };
            let Ok(dirfd) = u32::try_from(dirfd) else {
                return errno::BADF;
            };
            let Some(grant) = caller.data().preopen_grant_for_fd(dirfd).cloned() else {
                return errno::BADF;
            };
            let raw_path = match read_bytes(&mut caller, &memory, path_ptr, path_len) {
                Ok(p) => p,
                Err(e) => return e,
            };
            let Ok(guest_path) = String::from_utf8(raw_path) else {
                return errno::INVAL;
            };
            let components = match normalize_guest_path(&guest_path) {
                Ok(c) => c,
                Err(e) => return e,
            };
            let write_intent = (rights_base & RIGHTS_WRITE_MASK) != 0
                || (oflags & (oflags::CREAT | oflags::TRUNC)) != 0
                || (fd_flags & fd_flags::APPEND) != 0;
            if write_intent && !grant.allow_write() {
                return errno::NOTCAPABLE;
            }
            let root = grant.host_root().to_path_buf();
            let target: PathBuf = if components.is_empty() {
                root.clone()
            } else {
                root.join(components.join("/"))
            };
            let opened_file = if write_intent {
                if components.is_empty() {
                    return errno::ISDIR;
                }
                let parent = target.parent().unwrap_or(&root).to_path_buf();
                let canonical_parent = match std::fs::canonicalize(&parent) {
                    Ok(p) => p,
                    Err(e) => return io_errno(&e),
                };
                if !canonical_parent.starts_with(&root) {
                    return errno::NOTCAPABLE;
                }
                let Some(file_name) = target.file_name() else {
                    return errno::INVAL;
                };
                let final_path = canonical_parent.join(file_name);
                let mut options = OpenOptions::new();
                options.write(true);
                if (oflags & oflags::CREAT) != 0 {
                    options.create(true);
                }
                if (oflags & oflags::TRUNC) != 0 {
                    options.truncate(true);
                }
                if (fd_flags & fd_flags::APPEND) != 0 {
                    options.append(true);
                }
                match options.open(&final_path) {
                    Ok(file) => file,
                    Err(e) => return io_errno(&e),
                }
            } else {
                let canonical = match std::fs::canonicalize(&target) {
                    Ok(p) => p,
                    Err(e) => return io_errno(&e),
                };
                if !canonical.starts_with(&root) {
                    return errno::NOTCAPABLE;
                }
                if canonical.is_dir() {
                    let fd = match caller.data_mut().alloc_fd() {
                        Ok(fd) => fd,
                        Err(e) => return e,
                    };
                    caller.data_mut().fds.insert(fd, FdEntry::Directory);
                    return write_u32(&mut caller, &memory, opened_fd_ptr, fd);
                }
                if (oflags & oflags::DIRECTORY) != 0 {
                    return errno::NOTDIR;
                }
                match std::fs::File::open(&canonical) {
                    Ok(file) => file,
                    Err(e) => return io_errno(&e),
                }
            };
            let fd = match caller.data_mut().alloc_fd() {
                Ok(fd) => fd,
                Err(e) => return e,
            };
            caller.data_mut().fds.insert(
                fd,
                FdEntry::File {
                    file: opened_file,
                    writable: write_intent,
                },
            );
            write_u32(&mut caller, &memory, opened_fd_ptr, fd)
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_fence_rejects_absolute_and_escape_paths() {
        // Absolute guest paths are capability violations.
        assert_eq!(normalize_guest_path("/etc/passwd"), Err(errno::NOTCAPABLE));
        assert_eq!(
            normalize_guest_path("C:/Windows/win.ini"),
            Err(errno::NOTCAPABLE)
        );
        assert_eq!(
            normalize_guest_path("C:\\Windows\\win.ini"),
            Err(errno::INVAL)
        );
        // `..` escaping the mount root is a capability violation.
        assert_eq!(
            normalize_guest_path("../../outside.txt"),
            Err(errno::NOTCAPABLE)
        );
        assert_eq!(
            normalize_guest_path("a/../../outside.txt"),
            Err(errno::NOTCAPABLE)
        );
        // Structural violations are EINVAL.
        assert_eq!(normalize_guest_path(""), Err(errno::INVAL));
        assert_eq!(normalize_guest_path("a//b"), Err(errno::INVAL));
        assert_eq!(normalize_guest_path("a\\b"), Err(errno::INVAL));
    }

    #[test]
    fn lexical_fence_permits_in_mount_relative_paths() {
        assert_eq!(
            normalize_guest_path("allowed.txt"),
            Ok(vec!["allowed.txt".to_string()])
        );
        assert_eq!(
            normalize_guest_path("sub/./inner.txt"),
            Ok(vec!["sub".to_string(), "inner.txt".to_string()])
        );
        // "." alone resolves to the mount root itself.
        assert_eq!(normalize_guest_path("."), Ok(Vec::new()));
    }

    #[test]
    fn errno_constants_match_wasi_preview1_numbering() {
        assert_eq!(errno::SUCCESS, 0);
        assert_eq!(errno::BADF, 8);
        assert_eq!(errno::NOENT, 44);
        assert_eq!(errno::NOTDIR, 54);
        assert_eq!(errno::NOTCAPABLE, 76);
    }

    #[test]
    fn guest_exited_display_mentions_code() {
        let e = GuestExited { code: 7 };
        assert!(e.to_string().contains('7'));
    }
}
