use std::ffi::{OsStr, OsString};
use std::io;
use std::time::Duration;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::time::{sleep, timeout};
use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

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
        self.next = Some(create_server(
            &self.name,
            self.maximum_instances,
            config,
            false,
        )?);
        Ok((server, PeerIdentity::WindowsNamedPipe { process_id: None }))
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
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
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
    Ok((client, PeerIdentity::WindowsNamedPipe { process_id: None }))
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
