//! Bounded local IPC framing for the transport-neutral NLOS SABI surface.
//!
//! The wire protocol is a four-byte big-endian length followed by one
//! Protobuf `ExchangeRequest` or `ExchangeResponse`. Platform endpoint names
//! are supplied by a service resolver and never become schema identity.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::time::Duration;

use nlos_schema::sabi::v1::{self, ExchangeRequest, ExchangeResponse};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, MAX_ENVELOPE_BYTES, ValidatedExchangeRequest,
    ValidatedExchangeResponse, decode_exchange_request, decode_exchange_response,
    encode_exchange_request, encode_exchange_response,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::time::timeout;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

pub mod handshake;

const LENGTH_PREFIX_BYTES: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoOperation {
    Accept,
    Connect,
    Read,
    Write,
}

impl fmt::Display for IoOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accept => formatter.write_str("accept"),
            Self::Connect => formatter.write_str("connect"),
            Self::Read => formatter.write_str("read"),
            Self::Write => formatter.write_str("write"),
        }
    }
}

#[derive(Debug)]
pub enum IpcError {
    InvalidConfig(&'static str),
    FrameTooLarge {
        actual: usize,
        maximum: usize,
    },
    Timeout(IoOperation),
    Io {
        operation: IoOperation,
        source: io::Error,
    },
    Compatibility(CompatibilityError),
    CommonSemantics(CommonSemanticsError),
    Backpressure,
    ConnectionUnusable,
    RequestIdMismatch,
    AuthorizationDenied(String),
    ServiceFailure(&'static str),
}

impl fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(formatter, "invalid IPC config: {message}"),
            Self::FrameTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "IPC frame has {actual} bytes; maximum is {maximum}"
                )
            }
            Self::Timeout(operation) => write!(formatter, "IPC {operation} timed out"),
            Self::Io { operation, source } => write!(formatter, "IPC {operation} failed: {source}"),
            Self::Compatibility(error) => write!(formatter, "incompatible IPC frame: {error}"),
            Self::CommonSemantics(error) => {
                write!(formatter, "invalid common SABI context: {error}")
            }
            Self::Backpressure => formatter.write_str("IPC client already has an in-flight call"),
            Self::ConnectionUnusable => formatter.write_str(
                "IPC connection is unusable after an uncertain or invalid exchange; reconnect",
            ),
            Self::RequestIdMismatch => {
                formatter.write_str("IPC response request_id does not match the request")
            }
            Self::AuthorizationDenied(reason) => {
                write!(formatter, "IPC peer authorization denied: {reason}")
            }
            Self::ServiceFailure(reason) => write!(formatter, "IPC service failed: {reason}"),
        }
    }
}

impl Error for IpcError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Compatibility(error) => Some(error),
            Self::CommonSemantics(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CompatibilityError> for IpcError {
    fn from(error: CompatibilityError) -> Self {
        Self::Compatibility(error)
    }
}

impl From<CommonSemanticsError> for IpcError {
    fn from(error: CommonSemanticsError) -> Self {
        Self::CommonSemantics(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportConfig {
    maximum_frame_bytes: usize,
    connect_timeout: Duration,
    read_timeout: Duration,
    write_timeout: Duration,
}

impl TransportConfig {
    /// Creates a bounded transport policy.
    ///
    /// # Errors
    ///
    /// Rejects zero/oversized frame bounds and zero timeouts.
    pub fn new(
        maximum_frame_bytes: usize,
        connect_timeout: Duration,
        read_timeout: Duration,
        write_timeout: Duration,
    ) -> Result<Self, IpcError> {
        if maximum_frame_bytes == 0 || maximum_frame_bytes > MAX_ENVELOPE_BYTES {
            return Err(IpcError::InvalidConfig(
                "maximum_frame_bytes must be within 1..=MAX_ENVELOPE_BYTES",
            ));
        }
        if connect_timeout.is_zero() || read_timeout.is_zero() || write_timeout.is_zero() {
            return Err(IpcError::InvalidConfig("timeouts must be non-zero"));
        }
        Ok(Self {
            maximum_frame_bytes,
            connect_timeout,
            read_timeout,
            write_timeout,
        })
    }

    #[must_use]
    pub const fn maximum_frame_bytes(self) -> usize {
        self.maximum_frame_bytes
    }

    #[must_use]
    pub const fn connect_timeout(self) -> Duration {
        self.connect_timeout
    }

    #[must_use]
    pub const fn read_timeout(self) -> Duration {
        self.read_timeout
    }

    #[must_use]
    pub const fn write_timeout(self) -> Duration {
        self.write_timeout
    }
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            maximum_frame_bytes: MAX_ENVELOPE_BYTES,
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerIdentity {
    InMemory,
    Unix {
        process_id: Option<u32>,
        user_id: u32,
        group_id: u32,
    },
    WindowsNamedPipe {
        process_id: Option<u32>,
    },
}

/// Exact operating-system credential tuple a local service may bind before
/// reading any request bytes. This is an authentication pre-gate, not a
/// signed NLOS principal or authority-lease proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentialBinding {
    identity: PeerIdentity,
}

impl PeerCredentialBinding {
    /// Captures the exact peer identity observed by the platform adapter.
    #[must_use]
    pub const fn from_peer(identity: PeerIdentity) -> Self {
        Self { identity }
    }

    /// Returns the identity captured by this binding.
    #[must_use]
    pub const fn identity(self) -> PeerIdentity {
        self.identity
    }

    fn matches(self, peer: &PeerIdentity) -> bool {
        self.identity == *peer
    }
}

/// Fail-closed authorizer for one exact local OS peer credential tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExactPeerAuthorizer {
    binding: PeerCredentialBinding,
}

impl ExactPeerAuthorizer {
    /// Creates an authorizer bound to one previously observed peer tuple.
    #[must_use]
    pub const fn new(binding: PeerCredentialBinding) -> Self {
        Self { binding }
    }

    /// Returns the credential tuple this authorizer accepts.
    #[must_use]
    pub const fn binding(self) -> PeerCredentialBinding {
        self.binding
    }
}

impl PeerAuthorizer for ExactPeerAuthorizer {
    fn authorize(&self, peer: &PeerIdentity) -> Result<(), String> {
        if self.binding.matches(peer) {
            Ok(())
        } else {
            Err("peer credentials do not match the exact binding".to_owned())
        }
    }
}

pub trait PeerAuthorizer: Send + Sync {
    /// Decides whether the resolved operating-system peer may use the service.
    ///
    /// # Errors
    ///
    /// Returns a policy-safe denial reason. Authorization runs before a frame
    /// is read or dispatched.
    fn authorize(&self, peer: &PeerIdentity) -> Result<(), String>;
}

impl<F> PeerAuthorizer for F
where
    F: Fn(&PeerIdentity) -> Result<(), String> + Send + Sync,
{
    fn authorize(&self, peer: &PeerIdentity) -> Result<(), String> {
        self(peer)
    }
}

pub struct FramedIo<S> {
    stream: S,
    config: TransportConfig,
}

impl<S> FramedIo<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    #[must_use]
    pub const fn new(stream: S, config: TransportConfig) -> Self {
        Self { stream, config }
    }

    /// Returns the wrapped stream, consuming the framer. This lets a caller
    /// run the connection-level handshake with [`Self`] and then hand the
    /// same stream to `serve_one` or [`LocalRpcClient`].
    #[must_use]
    pub fn into_inner(self) -> S {
        self.stream
    }

    /// Writes one bounded length-prefixed frame.
    ///
    /// # Errors
    ///
    /// Returns a bound, timeout, or I/O error.
    pub async fn send(&mut self, wire: &[u8]) -> Result<(), IpcError> {
        if wire.len() > self.config.maximum_frame_bytes {
            return Err(IpcError::FrameTooLarge {
                actual: wire.len(),
                maximum: self.config.maximum_frame_bytes,
            });
        }
        let length = u32::try_from(wire.len()).map_err(|_| IpcError::FrameTooLarge {
            actual: wire.len(),
            maximum: self.config.maximum_frame_bytes,
        })?;
        let prefix = length.to_be_bytes();
        let write = async {
            self.stream.write_all(&prefix).await?;
            self.stream.write_all(wire).await?;
            self.stream.flush().await
        };
        timeout(self.config.write_timeout, write)
            .await
            .map_err(|_| IpcError::Timeout(IoOperation::Write))?
            .map_err(|source| IpcError::Io {
                operation: IoOperation::Write,
                source,
            })
    }

    /// Reads one bounded length-prefixed frame without allocating an
    /// attacker-declared oversized body.
    ///
    /// # Errors
    ///
    /// Returns a bound, timeout, EOF/half-frame, or other I/O error.
    pub async fn receive(&mut self) -> Result<Vec<u8>, IpcError> {
        let maximum = self.config.maximum_frame_bytes;
        let read = async {
            let mut prefix = [0_u8; LENGTH_PREFIX_BYTES];
            self.stream
                .read_exact(&mut prefix)
                .await
                .map_err(|source| IpcError::Io {
                    operation: IoOperation::Read,
                    source,
                })?;
            let declared = u32::from_be_bytes(prefix) as usize;
            if declared > maximum {
                return Err(IpcError::FrameTooLarge {
                    actual: declared,
                    maximum,
                });
            }
            let mut wire = vec![0_u8; declared];
            self.stream
                .read_exact(&mut wire)
                .await
                .map_err(|source| IpcError::Io {
                    operation: IoOperation::Read,
                    source,
                })?;
            Ok(wire)
        };
        timeout(self.config.read_timeout, read)
            .await
            .map_err(|_| IpcError::Timeout(IoOperation::Read))?
    }
}

pub enum OutboundResponse {
    Typed(ExchangeResponse),
    Forwarded(ValidatedExchangeResponse),
}

impl OutboundResponse {
    /// Encodes a response or returns exact bytes from a previously validated
    /// upstream response.
    fn into_wire(self) -> Result<Vec<u8>, IpcError> {
        match self {
            Self::Typed(response) => Ok(encode_exchange_response(&response)?),
            Self::Forwarded(response) => Ok(response.into_wire_bytes()),
        }
    }
}

pub struct LocalRpcClient<S> {
    connection: Mutex<ClientConnection<S>>,
}

struct ClientConnection<S> {
    framed: FramedIo<S>,
    usable: bool,
}

impl<S> LocalRpcClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    #[must_use]
    pub const fn new(stream: S, config: TransportConfig) -> Self {
        Self {
            connection: Mutex::const_new(ClientConnection {
                framed: FramedIo::new(stream, config),
                usable: true,
            }),
        }
    }

    /// Executes one unary call. A second concurrent call fails immediately
    /// with backpressure instead of entering an unbounded queue.
    ///
    /// # Errors
    ///
    /// Returns framing, compatibility, backpressure, or correlation errors.
    pub async fn exchange_validated(
        &self,
        request: ExchangeRequest,
    ) -> Result<ValidatedExchangeResponse, IpcError> {
        let request_id = request
            .envelope
            .as_ref()
            .ok_or(CompatibilityError::MissingExchangeEnvelope)?
            .request_id
            .clone();
        let wire = encode_exchange_request(&request)?;
        let mut connection = self
            .connection
            .try_lock()
            .map_err(|_| IpcError::Backpressure)?;
        if !connection.usable {
            return Err(IpcError::ConnectionUnusable);
        }
        if let Err(error) = connection.framed.send(&wire).await {
            connection.usable = false;
            return Err(error);
        }
        let response_wire = match connection.framed.receive().await {
            Ok(wire) => wire,
            Err(error) => {
                connection.usable = false;
                return Err(error);
            }
        };
        let response = match decode_exchange_response(&response_wire) {
            Ok(response) => response,
            Err(error) => {
                connection.usable = false;
                return Err(error.into());
            }
        };
        if response.envelope().request_id != request_id {
            connection.usable = false;
            return Err(IpcError::RequestIdMismatch);
        }
        Ok(response)
    }
}

impl<S> v1::local_rpc::Client for LocalRpcClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    type Error = IpcError;

    async fn exchange(&self, request: ExchangeRequest) -> Result<ExchangeResponse, Self::Error> {
        Ok(self.exchange_validated(request).await?.response().clone())
    }
}

/// Authorizes a peer, receives and validates one request, invokes the handler,
/// and sends one response.
///
/// # Errors
///
/// Returns authorization, framing, compatibility, handler, or I/O errors.
pub async fn serve_one<S, A, H, F>(
    stream: S,
    config: TransportConfig,
    peer: PeerIdentity,
    authorizer: &A,
    handler: H,
) -> Result<(), IpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    A: PeerAuthorizer,
    H: FnOnce(ValidatedExchangeRequest) -> F,
    F: Future<Output = Result<OutboundResponse, IpcError>>,
{
    authorizer
        .authorize(&peer)
        .map_err(IpcError::AuthorizationDenied)?;
    let mut connection = FramedIo::new(stream, config);
    let request_wire = connection.receive().await?;
    let request = decode_exchange_request(&request_wire)?;
    let response_wire = handler(request).await?.into_wire()?;
    connection.send(&response_wire).await
}

pub(crate) fn map_io(operation: IoOperation, source: io::Error) -> IpcError {
    IpcError::Io { operation, source }
}

pub(crate) async fn timeout_io<T>(
    operation: IoOperation,
    duration: Duration,
    future: impl Future<Output = io::Result<T>>,
) -> Result<T, IpcError> {
    timeout(duration, future)
        .await
        .map_err(|_| IpcError::Timeout(operation))?
        .map_err(|source| map_io(operation, source))
}
