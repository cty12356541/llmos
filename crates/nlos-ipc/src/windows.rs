use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::ptr::null_mut;
use std::time::Duration;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::time::{sleep, timeout};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, HANDLE};
use windows_sys::Win32::Security::{
    EqualSid, GetLengthSid, GetTokenInformation, IsValidSid, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::{
    IoOperation, IpcError, PeerAuthorizer, PeerIdentity, TransportConfig, map_io, timeout_io,
};

pub struct NamedPipeListenerAdapter {
    name: OsString,
    maximum_instances: usize,
    next: Option<NamedPipeServer>,
}

impl NamedPipeListenerAdapter {
    /// Creates the first local-only named-pipe instance. The first-instance
    /// flag prevents silently attaching to an existing pipe namespace.
    ///
    /// # Errors
    ///
    /// Returns a config or OS error.
    pub fn bind(
        name: impl AsRef<OsStr>,
        maximum_instances: usize,
        config: TransportConfig,
    ) -> Result<Self, IpcError> {
        if !(2..=254).contains(&maximum_instances) {
            return Err(IpcError::InvalidConfig(
                "named-pipe maximum_instances must be within 2..=254",
            ));
        }
        let name = name.as_ref().to_owned();
        let next = create_server(&name, maximum_instances, config, true)?;
        Ok(Self {
            name,
            maximum_instances,
            next: Some(next),
        })
    }

    /// Accepts one client and creates the next listening instance before
    /// handing the connected stream to the caller.
    ///
    /// An accept timeout is cancel-safe: the timed-out instance keeps its
    /// pending `ConnectNamedPipe` and is parked back into the listener, so
    /// the next `accept` call reuses it and no client connection is lost.
    /// Destroying the instance instead would be wrong twice over: it bricks
    /// the listener (no next instance), and the asynchronous teardown races
    /// a client `CreateFile` that still sees the instance listening. Only a
    /// non-timeout OS error consumes the instance; the listener immediately
    /// rebuilds a fresh one unless that rebuild itself fails (then the next
    /// call surfaces `InvalidConfig`).
    ///
    /// # Errors
    ///
    /// Returns a timeout or named-pipe OS error.
    pub async fn accept(
        &mut self,
        config: TransportConfig,
    ) -> Result<(NamedPipeServer, PeerIdentity), IpcError> {
        let server = self.next.take().ok_or(IpcError::InvalidConfig(
            "named-pipe listener lost its next instance",
        ))?;
        if let Err(error) = timeout_io(
            IoOperation::Accept,
            config.connect_timeout(),
            server.connect(),
        )
        .await
        {
            if matches!(error, IpcError::Timeout(IoOperation::Accept)) {
                self.next = Some(server);
            } else {
                drop(server);
                self.replace_next_instance(config)?;
            }
            return Err(error);
        }
        self.next = Some(create_server(
            &self.name,
            self.maximum_instances,
            config,
            false,
        )?);
        let process_id = client_process_id(server.as_raw_handle());
        Ok((server, PeerIdentity::WindowsNamedPipe { process_id }))
    }

    fn replace_next_instance(&mut self, config: TransportConfig) -> Result<(), IpcError> {
        if self.next.is_none() {
            self.next = Some(create_server(
                &self.name,
                self.maximum_instances,
                config,
                false,
            )?);
        }
        Ok(())
    }
}

/// Connects to a local named pipe with a bounded busy/not-found retry loop.
/// `SECURITY_IDENTIFICATION` prevents the server from impersonating this
/// client through an untrusted endpoint name.
///
/// # Errors
///
/// Returns a timeout or named-pipe OS error.
pub async fn connect(
    name: impl AsRef<OsStr>,
    config: TransportConfig,
) -> Result<(NamedPipeClient, PeerIdentity), IpcError> {
    let name = name.as_ref().to_owned();
    let connect = async {
        loop {
            let mut options = ClientOptions::new();
            options.security_qos_flags(SECURITY_IDENTIFICATION);
            match options.open(&name) {
                Ok(client) => return Ok(client),
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY.cast_signed())
                        || error.kind() == io::ErrorKind::NotFound =>
                {
                    sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error),
            }
        }
    };
    let client = timeout(config.connect_timeout(), connect)
        .await
        .map_err(|_| IpcError::Timeout(IoOperation::Connect))?
        .map_err(|source| map_io(IoOperation::Connect, source))?;
    let process_id = server_process_id(client.as_raw_handle());
    Ok((client, PeerIdentity::WindowsNamedPipe { process_id }))
}

// ---------------------------------------------------------------------------
// Peer credentials (F3-c3)
// ---------------------------------------------------------------------------

/// Fail-closed peer gate for Windows named-pipe servers: admits only peers
/// whose process-token user SID equals the gate owner's own process-token
/// user SID. This is the Windows counterpart of admitting only the creating
/// uid through a 0600 Unix domain socket endpoint.
///
/// The peer pid is taken from [`PeerIdentity::WindowsNamedPipe`]. An
/// unresolved pid, an unopenable process, an unreadable token, or a SID
/// mismatch is a denial. Between credential capture and `OpenProcess` the pid
/// could in principle be reused by another process; every doubt resolves to
/// denial rather than admission.
#[derive(Clone, Debug)]
pub struct SameUserAuthorizer {
    owner_user_sid: Vec<u8>,
}

impl SameUserAuthorizer {
    /// Captures the current process token's user SID as the admission
    /// reference.
    ///
    /// # Errors
    ///
    /// Returns an OS error when the current process token user cannot be
    /// queried.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            owner_user_sid: current_process_user_sid()?,
        })
    }

    /// Test seam binding the admission reference to explicit SID bytes; the
    /// bytes must be exactly one well-formed SID.
    #[cfg(test)]
    fn with_owner_sid(owner_user_sid: Vec<u8>) -> Self {
        Self { owner_user_sid }
    }

    fn admits(&self, peer_user_sid: &[u8]) -> bool {
        // The shortest well-formed SID is 8 bytes (revision, sub-authority
        // count, 6-byte authority); shorter inputs are malformed and denied
        // before any pointer arithmetic.
        if self.owner_user_sid.len() < 8 || peer_user_sid.len() < 8 {
            return false;
        }
        // SAFETY: both slices hold exactly the bytes of one well-formed SID
        // (captured via GetLengthSid over a validated SID, or allocated by
        // AllocateAndInitializeSid in tests); EqualSid only reads them.
        unsafe {
            EqualSid(
                self.owner_user_sid
                    .as_ptr()
                    .cast::<core::ffi::c_void>()
                    .cast_mut(),
                peer_user_sid
                    .as_ptr()
                    .cast::<core::ffi::c_void>()
                    .cast_mut(),
            ) != 0
        }
    }
}

impl PeerAuthorizer for SameUserAuthorizer {
    fn authorize(&self, peer: &PeerIdentity) -> Result<(), String> {
        let PeerIdentity::WindowsNamedPipe {
            process_id: Some(process_id),
        } = peer
        else {
            return Err("named-pipe peer has no resolved client process id".to_owned());
        };
        let peer_user_sid = process_user_sid(*process_id)
            .ok_or_else(|| "peer process token is not queryable".to_owned())?;
        if self.admits(&peer_user_sid) {
            Ok(())
        } else {
            Err("peer process runs under a different user".to_owned())
        }
    }
}

/// Returns the current process token's user SID bytes.
///
/// # Errors
///
/// Returns an OS error when the current process token user cannot be queried.
pub fn current_process_user_sid() -> io::Result<Vec<u8>> {
    // SAFETY: GetCurrentProcess returns a pseudo handle that must not be
    // closed and needs no further validity assumptions.
    let user_sid = unsafe { process_token_user_sid(GetCurrentProcess()) };
    user_sid.ok_or_else(|| io::Error::other("current process token user is not queryable"))
}

/// Resolves the client process id behind a connected server-side pipe
/// handle. `None` keeps the identity honest when the OS cannot resolve the
/// peer; peer gates are expected to fail closed on `None`.
fn client_process_id(pipe: RawHandle) -> Option<u32> {
    let mut process_id = 0_u32;
    // SAFETY: `pipe` is a live named-pipe handle owned by the caller and
    // `process_id` is a valid out-pointer; the call writes at most 4 bytes.
    let resolved = unsafe { GetNamedPipeClientProcessId(pipe, &raw mut process_id) };
    (resolved != 0).then_some(process_id)
}

/// Resolves the server process id behind a connected client-side pipe
/// handle; see [`client_process_id`] for the `None` semantics.
fn server_process_id(pipe: RawHandle) -> Option<u32> {
    let mut process_id = 0_u32;
    // SAFETY: `pipe` is a live named-pipe handle owned by the caller and
    // `process_id` is a valid out-pointer; the call writes at most 4 bytes.
    let resolved = unsafe { GetNamedPipeServerProcessId(pipe, &raw mut process_id) };
    (resolved != 0).then_some(process_id)
}

/// Resolves the token user SID of an arbitrary process id, or `None` when the
/// process cannot be opened or its token queried.
fn process_user_sid(process_id: u32) -> Option<Vec<u8>> {
    // SAFETY: a plain pid with the least-privileged query right; the returned
    // handle is closed on every path below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return None;
    }
    let user_sid = unsafe { process_token_user_sid(process) };
    // SAFETY: non-null owning handle from OpenProcess, closed exactly once.
    unsafe { CloseHandle(process) };
    user_sid
}

/// # Safety
///
/// `process` must be a live process handle whose token the caller may query.
/// The handle remains owned by the caller.
unsafe fn process_token_user_sid(process: HANDLE) -> Option<Vec<u8>> {
    let mut token = null_mut();
    // SAFETY: caller-guaranteed live process handle; `token` is a valid
    // out-pointer for the new token handle.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
        return None;
    }
    let user_sid = unsafe { token_user_sid(token) };
    // SAFETY: non-null owning handle from OpenProcessToken, closed exactly once.
    unsafe { CloseHandle(token) };
    user_sid
}

/// # Safety
///
/// `token` must be a live token handle opened with `TOKEN_QUERY`. The handle
/// remains owned by the caller.
unsafe fn token_user_sid(token: HANDLE) -> Option<Vec<u8>> {
    let mut length = 0_u32;
    // SAFETY: sizing call with a null buffer; `length` is a valid out-pointer.
    // A FALSE return with ERROR_INSUFFICIENT_BUFFER is the expected path.
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &raw mut length) };
    if length == 0 {
        return None;
    }
    // u64-aligned backing store: the TOKEN_USER header needs pointer
    // alignment, and the referenced SID is laid out inside the same buffer.
    let words = usize::try_from(length).unwrap_or(usize::MAX / 8) / 8 + 1;
    let mut buffer = vec![0_u64; words];
    let byte_capacity = u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX);
    // SAFETY: buffer covers at least `length` bytes and outlives the call.
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            byte_capacity,
            &raw mut length,
        )
    } == 0
    {
        return None;
    }
    // SAFETY: a successful TokenUser query leaves a TOKEN_USER at the buffer
    // start by contract; the buffer is u64-aligned for the struct.
    let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    // SAFETY: the SID pointer comes from a successful TOKEN_USER query.
    if unsafe { IsValidSid(user.User.Sid) } == 0 {
        return None;
    }
    // SAFETY: validated SID; GetLengthSid only reads its declared size.
    let sid_length = unsafe { GetLengthSid(user.User.Sid) };
    if sid_length == 0 {
        return None;
    }
    // SAFETY: `sid_length` bytes lie inside the token buffer per the SID
    // layout; the copy severs the buffer's lifetime.
    Some(
        unsafe { std::slice::from_raw_parts(user.User.Sid.cast::<u8>(), sid_length as usize) }
            .to_vec(),
    )
}

fn create_server(
    name: &OsStr,
    maximum_instances: usize,
    config: TransportConfig,
    first: bool,
) -> Result<NamedPipeServer, IpcError> {
    let buffer_size = u32::try_from(config.maximum_frame_bytes())
        .map_err(|_| IpcError::InvalidConfig("named-pipe buffer size exceeds u32"))?;
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .max_instances(maximum_instances)
        .in_buffer_size(buffer_size)
        .out_buffer_size(buffer_size);
    options
        .create(name)
        .map_err(|source| map_io(IoOperation::Accept, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{
        AllocateAndInitializeSid, FreeSid, PSID, SID_IDENTIFIER_AUTHORITY,
    };

    /// Allocates one real well-formed SID (`S-1-5-x`) via
    /// `AllocateAndInitializeSid` and returns its exact bytes.
    fn allocated_sid(sub_authority: u32) -> Vec<u8> {
        let authority = SID_IDENTIFIER_AUTHORITY {
            Value: [0, 0, 0, 0, 0, 5],
        };
        let mut sid: PSID = null_mut();
        // SAFETY: valid authority pointer and out-pointer; the SID is freed
        // by FreeSid below after the byte copy.
        assert_ne!(
            unsafe {
                AllocateAndInitializeSid(
                    &raw const authority,
                    1,
                    sub_authority,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    &raw mut sid,
                )
            },
            0
        );
        // SAFETY: the SID from a successful allocation is well-formed.
        let length = unsafe { GetLengthSid(sid) } as usize;
        // SAFETY: `length` bytes per the SID layout of a validated SID.
        let bytes = unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), length) }.to_vec();
        // SAFETY: SID from AllocateAndInitializeSid is released with FreeSid.
        unsafe { FreeSid(sid) };
        bytes
    }

    #[test]
    fn same_user_authorizer_admits_only_matching_wellformed_sids() {
        let owner = allocated_sid(11);
        let gate = SameUserAuthorizer::with_owner_sid(owner.clone());
        assert!(gate.admits(&owner));
        assert!(!gate.admits(&allocated_sid(12)));
        // Malformed inputs are denied before any pointer use.
        assert!(!gate.admits(&[]));
        assert!(!gate.admits(&[1, 1, 0, 0, 0, 0, 0, 5]));
    }

    #[test]
    fn current_process_user_sid_is_a_wellformed_user_sid() {
        let sid = current_process_user_sid().unwrap();
        // Revision byte 1 (SID_REVISION); the declared sub-authority count
        // must match the byte length (8 header bytes + 4 per sub-authority).
        assert!(sid.len() >= 12);
        assert_eq!(sid[0], 1);
        assert_eq!(usize::from(sid[1]), (sid.len() - 8) / 4);
    }
}
