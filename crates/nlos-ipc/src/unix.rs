use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use crate::{IoOperation, IpcError, PeerIdentity, TransportConfig, map_io, timeout_io};

pub struct UnixListenerAdapter {
    listener: UnixListener,
}

impl UnixListenerAdapter {
    /// Binds a caller-owned endpoint and restricts its filesystem mode to
    /// owner read/write. Existing paths are never removed automatically.
    ///
    /// # Errors
    ///
    /// Returns an OS error if binding or permission hardening fails.
    pub fn bind(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        let path = path.as_ref();
        let listener =
            UnixListener::bind(path).map_err(|source| map_io(IoOperation::Accept, source))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|source| map_io(IoOperation::Accept, source))?;
        Ok(Self { listener })
    }

    /// Accepts one connection within the configured connect/accept timeout.
    ///
    /// # Errors
    ///
    /// Returns a timeout, accept, or peer-credential error.
    pub async fn accept(
        &self,
        config: TransportConfig,
    ) -> Result<(UnixStream, PeerIdentity), IpcError> {
        let (stream, _) = timeout_io(
            IoOperation::Accept,
            config.connect_timeout(),
            self.listener.accept(),
        )
        .await?;
        let peer = peer_identity(&stream)?;
        Ok((stream, peer))
    }
}

/// Connects to a ServiceDirectory-resolved Unix socket endpoint.
///
/// # Errors
///
/// Returns a timeout, connect, or peer-credential error.
pub async fn connect(
    path: impl AsRef<Path>,
    config: TransportConfig,
) -> Result<(UnixStream, PeerIdentity), IpcError> {
    let stream = timeout_io(
        IoOperation::Connect,
        config.connect_timeout(),
        UnixStream::connect(path),
    )
    .await?;
    let peer = peer_identity(&stream)?;
    Ok((stream, peer))
}

fn peer_identity(stream: &UnixStream) -> Result<PeerIdentity, IpcError> {
    let credential = stream
        .peer_cred()
        .map_err(|source| map_io(IoOperation::Connect, source))?;
    Ok(PeerIdentity::Unix {
        process_id: credential.pid().and_then(|pid| u32::try_from(pid).ok()),
        user_id: credential.uid(),
        group_id: credential.gid(),
    })
}
