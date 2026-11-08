//! Bounded local IPC framing for the transport-neutral NLOS SABI surface.
//!
//! The wire protocol is a four-byte big-endian length followed by one
//! Protobuf `ExchangeRequest` or `ExchangeResponse`. Platform endpoint names
//! are supplied by a service resolver and never become schema identity.

// unsafe 审计说明：本 crate 刻意不继承 workspace 的 `unsafe_code = "forbid"`
// （见 Cargo.toml [lints] 注释）：唯一的 unsafe 面是 `windows` 模块对
// `GetNamedPipeClientProcessId` 的单点 FFI 桥——服务端 OS 级客户端 pid 凭据
// 只能经该系统调用获得（与 `nlos-store-fault` 的 VFS 垫片同族的指定审计
// 单元）。unsafe 全部收敛于 `src/windows.rs`，逐块附 SAFETY 论证；其余模块
// 不含任何 unsafe 代码。

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::time::{Duration, Instant};

use nlos_schema::sabi::v1::{self, ExchangeRequest, ExchangeResponse};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, MAX_ENVELOPE_BYTES, REQUEST_ID_BYTES,
    SABI_ENVELOPE_SCHEMA, ValidatedExchangeRequest, ValidatedExchangeResponse,
    decode_exchange_request, decode_exchange_response, encode_exchange_request,
    encode_exchange_response,
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

    /// Reads one bounded frame under a two-phase budget for long-lived
    /// sessions: the FIRST byte of the length prefix may take up to
    /// `idle_timeout` (the inter-frame idle window [`serve_connection`]
    /// enforces), while everything after it — the remaining prefix bytes and
    /// the body — stays under the config's per-frame `read_timeout` exactly
    /// like [`Self::receive`]. A clean close at a frame boundary, an idle
    /// expiry, and an over-limit DECLARED length are reported as typed
    /// outcomes, not errors; a partial frame (EOF after the first byte) and
    /// I/O failures stay errors. The over-limit body is deliberately NOT
    /// consumed here — [`serve_connection`] decides the bounded-discard
    /// per-call rejection posture (W82-B D3).
    ///
    /// # Errors
    ///
    /// Returns a bound, timeout (per-frame), or I/O error.
    pub(crate) async fn receive_with_idle(
        &mut self,
        idle_timeout: Duration,
    ) -> Result<IdleReceive, IpcError> {
        let maximum = self.config.maximum_frame_bytes;
        let mut first = [0_u8; 1];
        match timeout(idle_timeout, self.stream.read(&mut first)).await {
            Err(_elapsed) => return Ok(IdleReceive::IdleExpired),
            Ok(Err(source)) => {
                return Err(IpcError::Io {
                    operation: IoOperation::Read,
                    source,
                });
            }
            Ok(Ok(0)) => return Ok(IdleReceive::PeerClosed),
            Ok(Ok(_)) => {}
        }
        let read = async {
            let mut rest = [0_u8; LENGTH_PREFIX_BYTES - 1];
            self.stream
                .read_exact(&mut rest)
                .await
                .map_err(|source| IpcError::Io {
                    operation: IoOperation::Read,
                    source,
                })?;
            let mut prefix = [0_u8; LENGTH_PREFIX_BYTES];
            prefix[0] = first[0];
            prefix[1..].copy_from_slice(&rest);
            let declared = u32::from_be_bytes(prefix) as usize;
            if declared > maximum {
                // Typed outcome, body unconsumed: the caller owns the
                // bounded-discard decision (W82-B D3 per-call rejection).
                return Ok(IdleReceive::OverLimit { declared, maximum });
            }
            let mut wire = vec![0_u8; declared];
            self.stream
                .read_exact(&mut wire)
                .await
                .map_err(|source| IpcError::Io {
                    operation: IoOperation::Read,
                    source,
                })?;
            Ok(IdleReceive::Frame(wire))
        };
        timeout(self.config.read_timeout, read)
            .await
            .map_err(|_| IpcError::Timeout(IoOperation::Read))?
    }

    /// Discards exactly `declared` bytes of an already-rejected frame body
    /// under the config's per-frame `read_timeout` budget, restoring frame
    /// synchronization so the connection can survive a per-call rejection
    /// (W82-B D3). An absurd attacker-controlled declaration cannot sustain
    /// this read within the budget and surfaces the typed timeout error —
    /// the caller then drops the connection fail-closed.
    ///
    /// # Errors
    ///
    /// Returns a timeout or I/O error.
    pub(crate) async fn discard_declared(&mut self, declared: usize) -> Result<(), IpcError> {
        const CHUNK: usize = 8_192;
        let mut chunk = vec![0_u8; CHUNK.min(declared.max(1))];
        let mut remaining = declared;
        let discard = async {
            while remaining > 0 {
                let take = remaining.min(chunk.len());
                self.stream
                    .read_exact(&mut chunk[..take])
                    .await
                    .map_err(|source| IpcError::Io {
                        operation: IoOperation::Read,
                        source,
                    })?;
                remaining -= take;
            }
            Ok(())
        };
        timeout(self.config.read_timeout, discard)
            .await
            .map_err(|_| IpcError::Timeout(IoOperation::Read))?
    }
}

/// Outcome of [`FramedIo::receive_with_idle`]: one decoded frame, a clean
/// peer close at a frame boundary, an idle-window expiry, or an over-limit
/// declared length (body unconsumed — see [`FramedIo::discard_declared`]).
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum IdleReceive {
    Frame(Vec<u8>),
    PeerClosed,
    IdleExpired,
    OverLimit { declared: usize, maximum: usize },
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
        // Pre-poison `usable` before entering any `.await` suspension point so
        // dropping/cancelling this future mid-send or mid-receive prevents
        // subsequent calls from reusing a half-written or stale-buffered stream.
        connection.usable = false;
        if let Err(error) = connection.framed.send(&wire).await {
            // 写前界检失败(FrameTooLarge)零字节上网,连接仍然完好,恢复可用
            // ——与编码失败路径对称(深审计 36 D2);其余 send 失败(写超时/
            // 写中 I/O 错误)可能已留下部分写,保持 usable = false 要求上层重连。
            if matches!(error, IpcError::FrameTooLarge { .. }) {
                connection.usable = true;
            }
            return Err(error);
        }
        let response_wire = connection.framed.receive().await?;
        let response = decode_exchange_response(&response_wire)?;
        if response.envelope().request_id != request_id {
            return Err(IpcError::RequestIdMismatch);
        }
        connection.usable = true;
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
    // 库层强制回写 request_id(深审计 36 D3):Typed 响应由 serve_one 盖章
    // echo 请求关联,handler 遗漏回写不再使客户端收到 RequestIdMismatch
    // 并毒化连接;Forwarded 响应保持上游字节不变(避免重编码漂移),仅
    // 校验关联,失配以类型化错误拒绝发送。客户端侧的关联校验保留为
    // 纵深防御。
    let request_id = request.envelope().request_id.clone();
    let response = stamp_outbound_response(request_id, handler(request).await?)?;
    let response_wire = response.into_wire()?;
    connection.send(&response_wire).await
}

/// Stamps one outbound response with the request's correlation id (深审计 36
/// D3): Typed responses get the echoed `request_id` stamped by the library so
/// a handler that forgets the echo cannot poison the client with a
/// `RequestIdMismatch`; Forwarded responses keep the upstream bytes verbatim
/// (avoiding re-encoding drift) and are only correlation-checked, with a
/// mismatch rejected as a typed error before anything is sent.
fn stamp_outbound_response(
    request_id: Vec<u8>,
    response: OutboundResponse,
) -> Result<OutboundResponse, IpcError> {
    match response {
        OutboundResponse::Typed(mut response) => {
            if let Some(envelope) = response.envelope.as_mut() {
                envelope.request_id = request_id;
            }
            Ok(OutboundResponse::Typed(response))
        }
        OutboundResponse::Forwarded(validated) => {
            if validated.envelope().request_id != request_id {
                return Err(IpcError::RequestIdMismatch);
            }
            Ok(OutboundResponse::Forwarded(validated))
        }
    }
}

/// How one [`serve_connection`] cycle ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionTermination {
    /// The peer closed the connection cleanly at a frame boundary.
    ClientClosed,
    /// No request frame arrived within the connection idle timeout — the
    /// fail-closed posture: an idle session is retired, never held open.
    IdleExpired,
    /// Rejected/malformed frames exceeded the per-session sliding-window
    /// budget (W82-B D3): the connection is disconnected without answering
    /// the budget-exceeding frame, bounding the per-call rejection response
    /// amplification vector.
    RejectionBudgetExhausted,
}

/// Outcome of one bounded multi-round [`serve_connection`] cycle: how many
/// complete request→response exchanges were served, how many inbound frames
/// were rejected per-call (over-limit or undecodable), and how the
/// connection ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServedConnection {
    /// Completed exchanges before termination (saturating).
    pub rounds: u32,
    /// Frames rejected per-call under the W82-B D3 posture (each answered
    /// with a typed failure while the connection stayed alive, until the
    /// budget exhausted).
    pub rejected_frames: u32,
    /// Why the connection ended.
    pub termination: ConnectionTermination,
}

/// Per-session frame-rejection budget window (W82-B D3 frozen semantics,
/// maintainer ruling 2026-11-08): a rejected frame is answered with one
/// bounded typed failure while the connection stays alive; more than
/// [`FRAME_REJECTION_BUDGET`] rejections inside this sliding window
/// disconnects the session (the budget-exceeding frame is NOT answered —
/// the rejection privilege is revoked), bounding the 4-byte-prefix → full
/// typed response amplification vector.
pub const FRAME_REJECTION_WINDOW: Duration = Duration::from_secs(60);
/// Rejection threshold inside [`FRAME_REJECTION_WINDOW`]: the (budget + 1)-th
/// strike in the window disconnects.
pub const FRAME_REJECTION_BUDGET: u32 = 8;

/// The bounded typed failure a rejected frame is answered with. The frame
/// itself carries no decodable correlation, so the envelope reports the
/// transport-level neutral identity: zero correlation ids, the envelope
/// schema, and a `nlos.ipc`/`reject` service/method pair that never
/// dispatches anywhere — a well-behaved client never receives one (its
/// `LocalRpcClient` would fail request-id correlation), the response exists
/// for the D3 per-call rejection contract and observability.
fn frame_rejection_envelope(reason: &str) -> v1::Envelope {
    v1::Envelope {
        schema: Some(v1::SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0; REQUEST_ID_BYTES],
        service: "nlos.ipc".to_owned(),
        method: "reject".to_owned(),
        common_context: Some(v1::envelope::CommonContext::ResponseContext(
            v1::SabiResponseContext {
                correlation_id: vec![0; REQUEST_ID_BYTES],
                operation: None,
                receipts: Vec::new(),
                failure: Some(v1::SabiFailure {
                    code: v1::SabiErrorCode::InvalidArgument.into(),
                    retry: v1::RetryDirective::DoNotRetry.into(),
                    safe_message: reason.to_owned(),
                }),
            },
        )),
        payload: Vec::new(),
    }
}

/// Records one rejection strike against the per-session sliding window and
/// answers the frame unless the budget just exhausted.
///
/// Returns `Some(ServedConnection)` when the D3 budget disconnected the
/// session (caller returns it), `None` to keep serving.
async fn reject_frame<S>(
    connection: &mut FramedIo<S>,
    strikes: &mut VecDeque<Instant>,
    rounds: u32,
    rejected_frames: &mut u32,
    reason: &str,
) -> Result<Option<ServedConnection>, IpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let now = Instant::now();
    while strikes
        .front()
        .is_some_and(|struck_at| now.duration_since(*struck_at) > FRAME_REJECTION_WINDOW)
    {
        strikes.pop_front();
    }
    strikes.push_back(now);
    *rejected_frames += 1;
    if u32::try_from(strikes.len()).unwrap_or(u32::MAX) > FRAME_REJECTION_BUDGET {
        // Disconnect without answering: responses are capped at the budget.
        return Ok(Some(ServedConnection {
            rounds,
            rejected_frames: *rejected_frames,
            termination: ConnectionTermination::RejectionBudgetExhausted,
        }));
    }
    let response = encode_exchange_response(&ExchangeResponse {
        envelope: Some(frame_rejection_envelope(reason)),
    })?;
    connection.send(&response).await?;
    Ok(None)
}

/// Authorizes a peer, then serves BOUNDED multi-round exchanges on one
/// connection: receive → decode → `handler` → send, repeated until the client
/// closes the connection or the connection-level `idle_timeout` expires
/// between frames (fail-closed). Unlike the per-frame `read_timeout` baked
/// into [`TransportConfig`], the idle budget covers only the wait for the
/// FIRST byte of the next frame; once a frame starts, it completes (or
/// fails) under the per-frame budget like [`serve_one`].
///
/// Rejected frames (an over-limit declared length, or an undecodable body)
/// follow the W82-B D3 frozen posture: each is answered with one bounded
/// typed failure while the connection STAYS ALIVE — the over-limit body is
/// discarded under the per-frame budget to keep the stream synchronized —
/// until more than [`FRAME_REJECTION_BUDGET`] rejections land inside
/// [`FRAME_REJECTION_WINDOW`], which disconnects the session without
/// answering (see [`ConnectionTermination::RejectionBudgetExhausted`]).
///
/// The handler is `FnMut` and is invoked once per round, so a caller that
/// rebuilds per-exchange state inside the handler observes the same
/// per-round freshness semantics as one [`serve_one`] call per connection.
/// Every completed round keeps the exact [`serve_one`] wire contract: Typed
/// responses are stamped with the round's `request_id`, Forwarded responses
/// are correlation-checked verbatim, and any error (authorization, framing
/// mid-frame, handler, I/O) drops the connection fail-closed.
///
/// # Errors
///
/// Returns authorization, framing, compatibility, handler, or I/O errors; a
/// clean client close, idle expiry, or rejection-budget exhaustion is `Ok`
/// with the termination recorded.
pub async fn serve_connection<S, A, H, F>(
    stream: S,
    config: TransportConfig,
    peer: PeerIdentity,
    authorizer: &A,
    idle_timeout: Duration,
    mut handler: H,
) -> Result<ServedConnection, IpcError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    A: PeerAuthorizer,
    H: FnMut(ValidatedExchangeRequest) -> F,
    F: Future<Output = Result<OutboundResponse, IpcError>>,
{
    if idle_timeout.is_zero() {
        return Err(IpcError::InvalidConfig(
            "connection idle timeout must be non-zero",
        ));
    }
    authorizer
        .authorize(&peer)
        .map_err(IpcError::AuthorizationDenied)?;
    let mut connection = FramedIo::new(stream, config);
    let mut rounds = 0_u32;
    let mut rejected_frames = 0_u32;
    let mut strikes: VecDeque<Instant> = VecDeque::new();
    loop {
        let request = match connection.receive_with_idle(idle_timeout).await? {
            IdleReceive::Frame(wire) => {
                let Ok(request) = decode_exchange_request(&wire) else {
                    // W82-B D3: undecodable body; the frame was fully
                    // consumed, so the stream stays synchronized — reject
                    // per-call and keep serving.
                    if let Some(finished) = reject_frame(
                        &mut connection,
                        &mut strikes,
                        rounds,
                        &mut rejected_frames,
                        "frame body is not a decodable exchange request",
                    )
                    .await?
                    {
                        return Ok(finished);
                    }
                    continue;
                };
                request
            }
            IdleReceive::OverLimit { declared, .. } => {
                // W82-B D3: bounded discard restores synchronization so the
                // per-call rejection leaves the connection usable; an
                // absurd attacker-controlled declaration cannot sustain the
                // discard inside the per-frame budget and drops the
                // connection here (fail-closed).
                connection.discard_declared(declared).await?;
                if let Some(finished) = reject_frame(
                    &mut connection,
                    &mut strikes,
                    rounds,
                    &mut rejected_frames,
                    "frame declares more bytes than the transport maximum",
                )
                .await?
                {
                    return Ok(finished);
                }
                continue;
            }
            IdleReceive::PeerClosed => {
                return Ok(ServedConnection {
                    rounds,
                    rejected_frames,
                    termination: ConnectionTermination::ClientClosed,
                });
            }
            IdleReceive::IdleExpired => {
                return Ok(ServedConnection {
                    rounds,
                    rejected_frames,
                    termination: ConnectionTermination::IdleExpired,
                });
            }
        };
        let request_id = request.envelope().request_id.clone();
        let response = stamp_outbound_response(request_id, handler(request).await?)?;
        let response_wire = response.into_wire()?;
        connection.send(&response_wire).await?;
        rounds = rounds.saturating_add(1);
    }
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
