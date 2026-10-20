use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::time::{sleep, timeout};
use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

use crate::{IoOperation, IpcError, PeerIdentity, TransportConfig, map_io, timeout_io};

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
        timeout_io(
            IoOperation::Accept,
            config.connect_timeout(),
            server.connect(),
        )
        .await?;
        // Fail closed: the peer identity carries the kernel-observed client
        // pid or the accept fails with a typed error — never an unknown
        // credential that would make the pre-gate vacuously match.
        let process_id = client_process_id(&server)?;
        self.next = Some(create_server(
            &self.name,
            self.maximum_instances,
            config,
            false,
        )?);
        Ok((
            server,
            PeerIdentity::WindowsNamedPipe {
                process_id: Some(process_id),
            },
        ))
    }
}

/// Reads the kernel-observed client pid of one connected pipe instance.
///
/// This is the authoritative OS credential for the authorization pre-gate
/// ([`ExactPeerAuthorizer`](crate::ExactPeerAuthorizer)); a client's
/// self-reported identity is never consulted for authorization.
///
/// # Errors
///
/// Fails closed with a typed accept error when the OS refuses to disclose
/// the client pid. The connection is then dropped by the caller instead of
/// continuing with an unknown credential.
fn client_process_id(server: &NamedPipeServer) -> Result<u32, IpcError> {
    let mut process_id = 0_u32;
    // SAFETY: `server` owns its named-pipe handle for the duration of this
    // shared borrow, so the HANDLE lent by `as_raw_handle` is valid and open
    // for the whole call, and `GetNamedPipeClientProcessId` neither closes
    // it nor retains it. The call runs strictly after `connect()` completed,
    // so the instance is bound to exactly one client and the kernel can
    // attribute a pid. `addr_of_mut!` hands the API a raw pointer to the
    // valid, aligned, fully initialized `u32` out-parameter without ever
    // creating an intermediate reference. The returned BOOL is checked
    // against zero, so `process_id` is only read after the kernel reported
    // success.
    let result = unsafe {
        GetNamedPipeClientProcessId(server.as_raw_handle(), std::ptr::addr_of_mut!(process_id))
    };
    if result == 0 {
        return Err(IpcError::Io {
            operation: IoOperation::Accept,
            source: io::Error::last_os_error(),
        });
    }
    Ok(process_id)
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
    // Self-reported identity: the client end of a named pipe exposes no
    // in-kernel peer credential to read, so this process fills in its own
    // pid for informational symmetry with the Unix adapter's shape. The
    // asymmetry with the accept path is intentional: only the server-side
    // `GetNamedPipeClientProcessId` credential is authoritative for
    // authorization; nothing ever authorizes against this self-report.
    Ok((
        client,
        PeerIdentity::WindowsNamedPipe {
            process_id: Some(std::process::id()),
        },
    ))
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
