//! Transport-neutral cross-Cell RPC / bulk traits, `N = 1` zero-socket desktop
//! profile enforcement, and `tokio-rustls` TLS 1.3 mTLS listener + connector
//! (`ADR-0026` §2.2, §2.4, §3.1, §6.3).

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nlos_cell::{
    CellFence, CellIdentity, RemoteCallContract, RemoteCallOutcome, RemoteRetryDirective,
    RemoteRetryRefusal,
};
use nlos_types::PrincipalId;
use rustls::pki_types::ServerName;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::cert::{
    MtlsNodeCertVerifier, NLOS_CELL_TLS_SNI, NodeTlsCredentials, VerificationTimeSource,
    build_tls13_client_config, build_tls13_server_config,
};
use crate::failure::{
    PendingReconciliationTracker, SocketFailureKind, WriteProgressPhase, classify_transport_failure,
};
use crate::frame::MAX_BULK_CHUNK_BYTES;
use crate::handshake::{
    AuthenticatedPeerSession, FenceHandshakeError, FenceNonceRegistry, HandshakeContext,
    PeerFenceClaims, PeerFencePolicy, extract_tls13_material, run_client_fence_handshake,
    run_server_fence_handshake,
};
use crate::identity::{IdentityPeerVerifier, PeerDigestSigner};
use crate::mux::{
    BulkStreamHandler, BulkTransferReceipt, MultiplexedSession, MuxConfig, RpcRequestHandler,
    RpcResponsePayload,
};
use crate::profile::{
    ClusterProfile, ListenerSocketGuard, SingleHostProfileViolation, SocketActivityAccounting,
};
use crate::resolver::EndpointResolver;

/// Configuration parameters for [`MtlsEndpointListener`] and
/// [`MtlsCellTransport`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetTransportConfig {
    /// Timeout for TCP `connect()` + TLS 1.3 mTLS handshake.
    pub connect_and_tls_timeout: Duration,
    /// Timeout for the post-TLS 5-axis Exporter challenge-response handshake.
    pub fence_handshake_timeout: Duration,
    /// Timeout for a single high-priority `ControlRpc` request/response.
    pub rpc_call_timeout: Duration,
    /// Timeout for a bounded-window `BulkStream` transfer.
    pub bulk_transfer_timeout: Duration,
    /// Logical tick increment reported to `RemoteCallContract::classify_inflight_timeout`
    /// when an in-flight call hits a socket timeout or post-write `RST`/EOF.
    pub timeout_elapsed_ticks: u64,
    /// Multiplexed frame & bounded-window parameters.
    pub mux: MuxConfig,
}

impl Default for NetTransportConfig {
    fn default() -> Self {
        Self {
            connect_and_tls_timeout: Duration::from_secs(5),
            fence_handshake_timeout: Duration::from_secs(5),
            rpc_call_timeout: Duration::from_secs(5),
            bulk_transfer_timeout: Duration::from_secs(30),
            timeout_elapsed_ticks: 10,
            mux: MuxConfig::default(),
        }
    }
}

/// Fail-closed pre-dispatch contract refusals surfaced BEFORE touching the
/// network (`ADR-0026` §2.4, §3.1, §6.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportDispatchRefusal {
    /// `ClusterProfile::SingleHostDesktop` (`N = 1`) forbids cross-host socket
    /// operations (`[NLOS-GOAL-001]`).
    SingleHostProfile(SingleHostProfileViolation),
    /// Blind retry of an uncertain side-effecting `IdempotencyKey` was blocked
    /// until authoritative reconciliation (`[DIST-FAIL-001]`).
    BlindRetryBlocked(RemoteRetryRefusal),
    /// Caller supplied an empty (`0`-byte) bulk chunk.
    BulkChunkEmpty,
    /// Caller supplied a bulk chunk exceeding `MAX_BULK_CHUNK_BYTES` (`64 KiB`).
    BulkChunkExceedsMax {
        /// Offending chunk length.
        chunk_len: usize,
        /// Normative ceiling (`65_536`).
        max_len: usize,
    },
    /// Declared `total_bytes` did not match the sum of supplied chunk lengths.
    BulkTotalBytesMismatch {
        /// Declared byte count.
        declared: u64,
        /// Actual byte count.
        actual: u64,
    },
}

impl fmt::Display for TransportDispatchRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SingleHostProfile(v) => write!(formatter, "{v}"),
            Self::BlindRetryBlocked(r) => write!(formatter, "blind retry refused: {r:?}"),
            Self::BulkChunkEmpty => {
                formatter.write_str("bulk chunk must be non-empty (1..=65536 bytes)")
            }
            Self::BulkChunkExceedsMax { chunk_len, max_len } => write!(
                formatter,
                "bulk chunk ({chunk_len} bytes) exceeds MAX_BULK_CHUNK_BYTES ({max_len} bytes)"
            ),
            Self::BulkTotalBytesMismatch { declared, actual } => write!(
                formatter,
                "bulk total_bytes mismatch: declared {declared}, actual {actual}"
            ),
        }
    }
}

impl Error for TransportDispatchRefusal {}

/// Transport-neutral cross-Cell control RPC interface (`ADR-0026` §3.1, §6.3).
///
/// Targets nominal [`CellIdentity`] ONLY (`[DIST-NAME-001]`); ephemeral
/// `SocketAddr` routing hints are resolved internally via [`EndpointResolver`].
pub trait CellRpcTransport: Send + Sync {
    /// Dispatches a high-priority control-plane RPC to `target` under
    /// `contract`, mapping socket/TLS failures deterministically to
    /// [`RemoteCallOutcome`].
    fn call_cell_rpc<'a>(
        &'a self,
        target: CellIdentity,
        contract: &'a RemoteCallContract,
        payload: Vec<u8>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        RemoteCallOutcome<RpcResponsePayload>,
                        TransportDispatchRefusal,
                    >,
                > + Send
                + 'a,
        >,
    >;
}

/// Transport-neutral bounded-window cross-Cell bulk streaming interface
/// (`ADR-0026` §3.1, §6.3).
pub trait BulkStreamTransport: Send + Sync {
    /// Streams `<= 64 KiB` chunks to `target` under per-stream credit-window
    /// backpressure without starving concurrent [`CellRpcTransport`] calls.
    fn stream_bulk_chunks<'a>(
        &'a self,
        target: CellIdentity,
        contract: &'a RemoteCallContract,
        chunks: Vec<Vec<u8>>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        RemoteCallOutcome<BulkTransferReceipt>,
                        TransportDispatchRefusal,
                    >,
                > + Send
                + 'a,
        >,
    >;
}

/// Detailed failure surfaced when dialing or authenticating a peer session
/// before any business payload byte is written.
#[derive(Debug)]
pub enum DialSessionError {
    /// `ClusterProfile::SingleHostDesktop` (`N = 1`) forbids network dial.
    SingleHostProfile(SingleHostProfileViolation),
    /// `EndpointResolver` returned `None` for `target`.
    EndpointUnresolved(CellIdentity),
    /// Dialing local Cell's own nominal identity is forbidden.
    SelfDialForbidden(CellIdentity),
    /// TCP `connect()` timed out.
    ConnectTimeout(SocketAddr),
    /// TCP `connect()` failed (`ECONNREFUSED`, `ENETUNREACH`, etc.).
    TcpConnect(io::Error),
    /// TLS 1.3 handshake timed out or failed (`rustls` / X.509 verifier rejection).
    TlsHandshake(io::Error),
    /// Post-TLS 5-axis Exporter challenge-response failed closed.
    FenceHandshake(FenceHandshakeError),
}

impl DialSessionError {
    /// Maps this pre-payload dial error to its canonical [`SocketFailureKind`].
    #[must_use]
    pub fn socket_failure_kind(&self) -> SocketFailureKind {
        match self {
            Self::SingleHostProfile(_) => SocketFailureKind::SingleHostProfileDisabled,
            Self::EndpointUnresolved(_) | Self::SelfDialForbidden(_) => {
                SocketFailureKind::EndpointUnresolved
            }
            Self::ConnectTimeout(_) => SocketFailureKind::ConnectTimeout,
            Self::TcpConnect(err) => SocketFailureKind::from_stream_io_error(err),
            Self::TlsHandshake(_) => SocketFailureKind::TlsHandshakeRejected,
            Self::FenceHandshake(_) => SocketFailureKind::PeerFenceHandshakeRejected,
        }
    }
}

impl fmt::Display for DialSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SingleHostProfile(v) => write!(formatter, "{v}"),
            Self::EndpointUnresolved(cell) => {
                write!(formatter, "no EndpointResolver routing hint for {cell:?}")
            }
            Self::SelfDialForbidden(cell) => {
                write!(formatter, "refusing self-dial to local {cell:?}")
            }
            Self::ConnectTimeout(addr) => write!(formatter, "TCP connect timed out to {addr}"),
            Self::TcpConnect(err) => write!(formatter, "TCP connect failed: {err}"),
            Self::TlsHandshake(err) => write!(formatter, "TLS 1.3 mTLS handshake rejected: {err}"),
            Self::FenceHandshake(err) => {
                write!(formatter, "5-axis fence handshake rejected: {err}")
            }
        }
    }
}

impl Error for DialSessionError {}

/// Initialization errors when constructing [`MtlsEndpointListener`] or
/// [`MtlsCellTransport`].
#[derive(Debug)]
pub enum NetInitError {
    /// `ClusterProfile::SingleHostDesktop` (`N = 1`) refused socket bind/init.
    SingleHostProfile(SingleHostProfileViolation),
    /// `rustls` TLS 1.3 configuration error.
    TlsConfig(rustls::Error),
    /// OS TCP listener bind failure.
    BindIo(io::Error),
}

impl fmt::Display for NetInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SingleHostProfile(v) => write!(formatter, "{v}"),
            Self::TlsConfig(err) => write!(formatter, "TLS 1.3 config error: {err}"),
            Self::BindIo(err) => write!(formatter, "TCP listener bind failed: {err}"),
        }
    }
}

impl Error for NetInitError {}

/// Shared node identity, custody signer/verifier, fence state, and clock source
/// used by a Cell's TLS 1.3 listener and connector.
#[derive(Clone)]
pub struct NodeSecurityDeps {
    /// Current 4-axis local [`CellFence`] provider (allowing dynamic epoch
    /// advancement without rebuilding transport handles).
    pub local_fence: Arc<dyn Fn() -> CellFence + Send + Sync>,
    /// Custody-backed digest signer for local `PrincipalId`.
    pub local_signer: Arc<dyn PeerDigestSigner>,
    /// Authority + custody tombstone verifier for peer `PrincipalId`s.
    pub peer_verifier: Arc<dyn IdentityPeerVerifier>,
    /// Monotonic peer `(node_boot_generation, epoch, token)` admission gate.
    pub fence_policy: Arc<dyn PeerFencePolicy>,
    /// Replay-defense nonce registry.
    pub nonce_registry: Arc<FenceNonceRegistry>,
    /// Logical clock source (`ms`) for certificate & key validity checks.
    pub clock_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Issued per-boot Ed25519 TLS 1.3 X.509v3 credentials.
    pub tls_credentials: NodeTlsCredentials,
}

impl fmt::Debug for NodeSecurityDeps {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fence = (self.local_fence)();
        formatter
            .debug_struct("NodeSecurityDeps")
            .field("local_cell", &fence.identity())
            .field("local_principal", &self.local_signer.principal_id())
            .field("tls_credentials", &self.tls_credentials)
            .finish_non_exhaustive()
    }
}

impl NodeSecurityDeps {
    fn current_local_claims(&self) -> PeerFenceClaims {
        let fence = (self.local_fence)();
        PeerFenceClaims::from_fence_and_signer(fence, self.local_signer.as_ref())
    }

    fn verification_time_source(&self) -> VerificationTimeSource {
        let clk = Arc::clone(&self.clock_ms);
        VerificationTimeSource::CustomMs(clk)
    }
}

/// Bounded ring capacity for rejected TLS / 5-axis fence handshake audit reasons.
pub const MAX_REJECTED_HANDSHAKE_AUDIT_ENTRIES: usize = 256;

/// Bounded retention capacity for accepted [`MultiplexedSession`] handles on a
/// listener, preventing unbounded session-table growth across churn.
pub const MAX_RETAINED_ACCEPTED_SESSIONS: usize = 256;

#[derive(Debug, Default)]
struct RejectedHandshakeAudit {
    reasons: VecDeque<String>,
    total_rejected_count: u64,
}

impl RejectedHandshakeAudit {
    fn record(&mut self, reason: String) {
        if self.reasons.len() >= MAX_REJECTED_HANDSHAKE_AUDIT_ENTRIES {
            let _ = self.reasons.pop_front();
        }
        self.reasons.push_back(reason);
        self.total_rejected_count = self.total_rejected_count.saturating_add(1);
    }
}

#[derive(Default)]
struct BoundedAcceptedSessions {
    sessions: VecDeque<MultiplexedSession>,
}

impl BoundedAcceptedSessions {
    fn record(&mut self, session: MultiplexedSession) {
        self.sessions.retain(MultiplexedSession::is_healthy);
        if self.sessions.len() >= MAX_RETAINED_ACCEPTED_SESSIONS {
            let _ = self.sessions.pop_front();
        }
        self.sessions.push_back(session);
    }

    fn snapshot(&self) -> Vec<MultiplexedSession> {
        self.sessions.iter().cloned().collect()
    }

    fn close_all(&mut self) {
        for session in &self.sessions {
            session.close();
        }
    }
}

/// TLS 1.3 mTLS + 5-axis Exporter authenticating endpoint listener
/// (`ADR-0026` §2.2, §3.1).
pub struct MtlsEndpointListener {
    local_cell: CellIdentity,
    local_addr: SocketAddr,
    tcp_listener: TcpListener,
    tls_acceptor: TlsAcceptor,
    security: NodeSecurityDeps,
    config: NetTransportConfig,
    accounting: SocketActivityAccounting,
    rpc_handler: Option<Arc<dyn RpcRequestHandler>>,
    bulk_handler: Option<Arc<dyn BulkStreamHandler>>,
    accepted_sessions: Arc<Mutex<BoundedAcceptedSessions>>,
    peak_inbound_bulk_bytes: Arc<AtomicUsize>,
    rejected_handshakes: Arc<Mutex<RejectedHandshakeAudit>>,
    _listener_guard: ListenerSocketGuard,
}

impl fmt::Debug for MtlsEndpointListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MtlsEndpointListener")
            .field("local_cell", &self.local_cell)
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl MtlsEndpointListener {
    /// Binds a TLS 1.3 mTLS listener when `profile` is
    /// [`ClusterProfile::MultiHostCluster`], or fails closed with
    /// [`NetInitError::SingleHostProfile`] (**0 OS sockets bound**) when
    /// `profile` is [`ClusterProfile::SingleHostDesktop`].
    ///
    /// # Errors
    ///
    /// Returns [`NetInitError`] on `SingleHostDesktop` profile refusal, TLS 1.3
    /// configuration error, or TCP socket bind failure.
    pub async fn bind(
        profile: ClusterProfile,
        security: NodeSecurityDeps,
        config: NetTransportConfig,
        accounting: SocketActivityAccounting,
        rpc_handler: Option<Arc<dyn RpcRequestHandler>>,
        bulk_handler: Option<Arc<dyn BulkStreamHandler>>,
    ) -> Result<Self, NetInitError> {
        let (local_cell, bind_hint) = profile
            .require_multi_host_socket_permission("MtlsEndpointListener::bind")
            .map_err(NetInitError::SingleHostProfile)?;

        let verifier = Arc::new(MtlsNodeCertVerifier::new(
            Arc::clone(&security.peer_verifier),
            security.verification_time_source(),
        ));
        let server_cfg = build_tls13_server_config(&security.tls_credentials, verifier)
            .map_err(NetInitError::TlsConfig)?;
        let tcp_listener = TcpListener::bind(bind_hint)
            .await
            .map_err(NetInitError::BindIo)?;
        let local_addr = tcp_listener.local_addr().map_err(NetInitError::BindIo)?;
        let listener_guard = accounting.acquire_listener_guard();

        Ok(Self {
            local_cell,
            local_addr,
            tcp_listener,
            tls_acceptor: TlsAcceptor::from(server_cfg),
            security,
            config,
            accounting,
            rpc_handler,
            bulk_handler,
            accepted_sessions: Arc::new(Mutex::new(BoundedAcceptedSessions::default())),
            peak_inbound_bulk_bytes: Arc::new(AtomicUsize::new(0)),
            rejected_handshakes: Arc::new(Mutex::new(RejectedHandshakeAudit::default())),
            _listener_guard: listener_guard,
        })
    }

    /// Ephemeral bound `SocketAddr` (`127.0.0.1:<port>` when bound with port 0).
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Nominal `CellIdentity` served by this listener.
    #[must_use]
    pub const fn local_cell(&self) -> CellIdentity {
        self.local_cell
    }

    /// Accepts and authenticates a single inbound connection (TLS 1.3 mTLS +
    /// 5-axis TLS Exporter handshake) and spawns its [`MultiplexedSession`].
    ///
    /// # Errors
    ///
    /// Fails closed before spawning any `MultiplexedSession` if TCP accept,
    /// TLS 1.3 mTLS, or 5-axis Exporter authentication fails.
    pub async fn accept_one(&self) -> Result<MultiplexedSession, DialSessionError> {
        let (tcp_stream, _peer_addr) = self
            .tcp_listener
            .accept()
            .await
            .map_err(DialSessionError::TcpConnect)?;
        let _ = tcp_stream.set_nodelay(true);

        authenticate_inbound_stream(
            tcp_stream,
            &self.tls_acceptor,
            &self.security,
            &self.config,
            &self.accounting,
            self.rpc_handler.clone(),
            self.bulk_handler.clone(),
            &self.accepted_sessions,
            &self.peak_inbound_bulk_bytes,
            &self.rejected_handshakes,
        )
        .await
    }

    /// Consumes this listener into a background accept loop with graceful
    /// shutdown control and telemetry inspection handles.
    #[must_use]
    pub fn spawn_accept_loop(self) -> RunningMtlsListener {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let local_addr = self.local_addr;
        let local_cell = self.local_cell;
        let accepted_sessions = Arc::clone(&self.accepted_sessions);
        let peak_inbound_bulk_bytes = Arc::clone(&self.peak_inbound_bulk_bytes);
        let rejected_handshakes = Arc::clone(&self.rejected_handshakes);

        let join_handle = tokio::spawn(async move {
            let listener = self;
            loop {
                let accepted = tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                        continue;
                    }
                    res = listener.tcp_listener.accept() => res,
                };

                let Ok((tcp_stream, _)) = accepted else {
                    break;
                };
                let _ = tcp_stream.set_nodelay(true);

                let acceptor = listener.tls_acceptor.clone();
                let security = listener.security.clone();
                let config = listener.config;
                let accounting = listener.accounting.clone();
                let rpc_handler = listener.rpc_handler.clone();
                let bulk_handler = listener.bulk_handler.clone();
                let sessions_slot = Arc::clone(&listener.accepted_sessions);
                let peak_bulk_slot = Arc::clone(&listener.peak_inbound_bulk_bytes);
                let rejected_slot = Arc::clone(&listener.rejected_handshakes);

                tokio::spawn(async move {
                    let _ = authenticate_inbound_stream(
                        tcp_stream,
                        &acceptor,
                        &security,
                        &config,
                        &accounting,
                        rpc_handler,
                        bulk_handler,
                        &sessions_slot,
                        &peak_bulk_slot,
                        &rejected_slot,
                    )
                    .await;
                });
            }
        });

        RunningMtlsListener {
            local_cell,
            local_addr,
            shutdown_tx,
            join_handle,
            accepted_sessions,
            peak_inbound_bulk_bytes,
            rejected_handshakes,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn authenticate_inbound_stream(
    tcp_stream: TcpStream,
    tls_acceptor: &TlsAcceptor,
    security: &NodeSecurityDeps,
    config: &NetTransportConfig,
    accounting: &SocketActivityAccounting,
    rpc_handler: Option<Arc<dyn RpcRequestHandler>>,
    bulk_handler: Option<Arc<dyn BulkStreamHandler>>,
    accepted_sessions: &Arc<Mutex<BoundedAcceptedSessions>>,
    peak_inbound_bulk_bytes: &Arc<AtomicUsize>,
    rejected_handshakes: &Arc<Mutex<RejectedHandshakeAudit>>,
) -> Result<MultiplexedSession, DialSessionError> {
    let mut tls_stream = match timeout(
        config.connect_and_tls_timeout,
        tls_acceptor.accept(tcp_stream),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            rejected_handshakes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record(format!("tls_reject: {err}"));
            return Err(DialSessionError::TlsHandshake(err));
        }
        Err(_) => {
            let err = io::Error::new(io::ErrorKind::TimedOut, "server TLS 1.3 handshake timeout");
            rejected_handshakes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record(format!("tls_timeout: {err}"));
            return Err(DialSessionError::TlsHandshake(err));
        }
    };

    let tls_material = match extract_tls13_material(tls_stream.get_ref().1) {
        Ok(m) => m,
        Err(err) => {
            rejected_handshakes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record(format!("exporter_extract_reject: {err}"));
            return Err(DialSessionError::FenceHandshake(err));
        }
    };

    let local_claims = security.current_local_claims();
    let now_ms = (security.clock_ms)();
    let ctx = HandshakeContext {
        local_claims,
        local_signer: security.local_signer.as_ref(),
        peer_verifier: security.peer_verifier.as_ref(),
        fence_policy: security.fence_policy.as_ref(),
        nonce_registry: security.nonce_registry.as_ref(),
        now_ms,
        handshake_timeout: config.fence_handshake_timeout,
    };

    let peer_session = match run_server_fence_handshake(&mut tls_stream, &tls_material, &ctx).await
    {
        Ok(s) => s,
        Err(err) => {
            rejected_handshakes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record(format!("fence_reject: {err}"));
            return Err(DialSessionError::FenceHandshake(err));
        }
    };

    let conn_guard = accounting.acquire_connection_guard();
    let (reader, writer) = tokio::io::split(tls_stream);
    let mux = MultiplexedSession::spawn_with_guard(
        reader,
        writer,
        peer_session,
        config.mux,
        rpc_handler,
        bulk_handler,
        Some(conn_guard),
        Some(Arc::clone(peak_inbound_bulk_bytes)),
    );
    accepted_sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(mux.clone());
    Ok(mux)
}

/// Handle to a background [`MtlsEndpointListener`] accept task.
pub struct RunningMtlsListener {
    local_cell: CellIdentity,
    local_addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    join_handle: JoinHandle<()>,
    accepted_sessions: Arc<Mutex<BoundedAcceptedSessions>>,
    peak_inbound_bulk_bytes: Arc<AtomicUsize>,
    rejected_handshakes: Arc<Mutex<RejectedHandshakeAudit>>,
}

impl fmt::Debug for RunningMtlsListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunningMtlsListener")
            .field("local_cell", &self.local_cell)
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl RunningMtlsListener {
    /// Nominal `CellIdentity` served by this listener.
    #[must_use]
    pub const fn local_cell(&self) -> CellIdentity {
        self.local_cell
    }

    /// Bound `SocketAddr` hint of this listener.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Bounded snapshot (`<= 256`) of authenticated [`MultiplexedSession`]s
    /// retained by this listener.
    #[must_use]
    pub fn accepted_sessions(&self) -> Vec<MultiplexedSession> {
        self.accepted_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot()
    }

    /// Peak inbound bulk buffered bytes observed across all accepted sessions
    /// on this listener (preserved across session closure and retention pruning).
    #[must_use]
    pub fn max_inbound_bulk_buffered_bytes(&self) -> usize {
        let live_max = self
            .accepted_sessions()
            .into_iter()
            .map(|s| s.peak_bulk_buffered_bytes())
            .max()
            .unwrap_or(0);
        live_max.max(self.peak_inbound_bulk_bytes.load(Ordering::Acquire))
    }

    /// Bounded audit log (`<= 256` most recent entries) of rejected TLS / 5-axis
    /// fence handshakes observed by this listener.
    #[must_use]
    pub fn rejected_handshake_reasons(&self) -> Vec<String> {
        self.rejected_handshakes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reasons
            .iter()
            .cloned()
            .collect()
    }

    /// Monotonic total count of rejected TLS / 5-axis fence handshakes observed
    /// by this listener (never wraps or drops under bounded log eviction).
    #[must_use]
    pub fn total_rejected_handshakes(&self) -> u64 {
        self.rejected_handshakes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .total_rejected_count
    }

    /// Signals the accept loop to stop accepting new connections, closes active
    /// accepted sessions, and awaits listener socket teardown.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.join_handle.await;
        self.accepted_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .close_all();
    }
}

/// Cross-Cell TLS 1.3 mTLS connector + session multiplexer implementing
/// [`CellRpcTransport`] and [`BulkStreamTransport`] (`ADR-0026` §3.1, §6.3).
///
/// In [`ClusterProfile::SingleHostDesktop`] (`N = 1`, the default), this
/// transport holds **zero** TLS state, binds/dials **zero** OS sockets, and
/// rejects cross-host calls fail-closed.
pub struct MtlsCellTransport {
    profile: ClusterProfile,
    config: NetTransportConfig,
    accounting: SocketActivityAccounting,
    reconciliation: PendingReconciliationTracker,
    active_mode: Option<MultiHostClientState>,
}

struct MultiHostClientState {
    local_cell: CellIdentity,
    resolver: Arc<dyn EndpointResolver>,
    tls_connector: TlsConnector,
    security: NodeSecurityDeps,
    inbound_rpc_handler: Option<Arc<dyn RpcRequestHandler>>,
    inbound_bulk_handler: Option<Arc<dyn BulkStreamHandler>>,
    pooled_sessions: Mutex<BTreeMap<CellIdentity, (SocketAddr, MultiplexedSession)>>,
}

impl Drop for MultiHostClientState {
    fn drop(&mut self) {
        let mut pool = self
            .pooled_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (_, (_, session)) in std::mem::take(&mut *pool) {
            session.close();
        }
    }
}

impl fmt::Debug for MtlsCellTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MtlsCellTransport")
            .field("profile", &self.profile)
            .field("config", &self.config)
            .field("accounting", &self.accounting)
            .finish_non_exhaustive()
    }
}

impl Default for MtlsCellTransport {
    /// Default `N = 1` [`ClusterProfile::SingleHostDesktop`] instance: binds
    /// **0** sockets and creates **0** TLS configurations.
    fn default() -> Self {
        Self::new_single_host_desktop(SocketActivityAccounting::new())
    }
}

impl MtlsCellTransport {
    /// Constructs the default `N = 1` single-host desktop transport (`0`
    /// sockets, `0` TLS state).
    #[must_use]
    pub fn new_single_host_desktop(accounting: SocketActivityAccounting) -> Self {
        Self {
            profile: ClusterProfile::SingleHostDesktop,
            config: NetTransportConfig::default(),
            accounting,
            reconciliation: PendingReconciliationTracker::new(),
            active_mode: None,
        }
    }

    /// Constructs a [`ClusterProfile::MultiHostCluster`] mTLS client transport.
    ///
    /// # Errors
    ///
    /// Fails closed with [`NetInitError::SingleHostProfile`] if `profile` is
    /// `SingleHostDesktop`, or [`NetInitError::TlsConfig`] if TLS 1.3 setup
    /// fails.
    pub fn new_multi_host(
        profile: ClusterProfile,
        resolver: Arc<dyn EndpointResolver>,
        security: NodeSecurityDeps,
        config: NetTransportConfig,
        accounting: SocketActivityAccounting,
        inbound_rpc_handler: Option<Arc<dyn RpcRequestHandler>>,
        inbound_bulk_handler: Option<Arc<dyn BulkStreamHandler>>,
    ) -> Result<Self, NetInitError> {
        let (local_cell, _bind_hint) = profile
            .require_multi_host_socket_permission("MtlsCellTransport::new_multi_host")
            .map_err(NetInitError::SingleHostProfile)?;

        let verifier = Arc::new(MtlsNodeCertVerifier::new(
            Arc::clone(&security.peer_verifier),
            security.verification_time_source(),
        ));
        let client_cfg = build_tls13_client_config(&security.tls_credentials, verifier)
            .map_err(NetInitError::TlsConfig)?;

        Ok(Self {
            profile,
            config,
            accounting,
            reconciliation: PendingReconciliationTracker::new(),
            active_mode: Some(MultiHostClientState {
                local_cell,
                resolver,
                tls_connector: TlsConnector::from(client_cfg),
                security,
                inbound_rpc_handler,
                inbound_bulk_handler,
                pooled_sessions: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    /// Active [`ClusterProfile`].
    #[must_use]
    pub const fn profile(&self) -> ClusterProfile {
        self.profile
    }

    /// Observable socket accounting ledger.
    #[must_use]
    pub const fn socket_accounting(&self) -> &SocketActivityAccounting {
        &self.accounting
    }

    /// Client-side reconciliation fence tracker enforcing `[DIST-FAIL-001]`
    /// against blind retries of uncertain side-effecting calls.
    #[must_use]
    pub const fn reconciliation_tracker(&self) -> &PendingReconciliationTracker {
        &self.reconciliation
    }

    /// Evicts and closes any cached [`MultiplexedSession`] for `target`.
    pub fn invalidate_cached_session(&self, target: CellIdentity) {
        if let Some(state) = &self.active_mode
            && let Some((_, old_session)) = state
                .pooled_sessions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&target)
        {
            old_session.close();
        }
    }

    /// Explicitly closes and evicts all pooled outbound [`MultiplexedSession`]s
    /// so their underlying TLS/TCP sockets and `ConnectionSocketGuard`s drop
    /// immediately.
    pub fn close_all_sessions(&self) {
        if let Some(state) = &self.active_mode {
            let mut pool = state
                .pooled_sessions
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (_, (_, session)) in std::mem::take(&mut *pool) {
                session.close();
            }
        }
    }

    /// Dials a fresh TLS 1.3 mTLS + 5-axis Exporter authenticated session to
    /// `target` via [`EndpointResolver`] (or reuses a healthy session bound to
    /// the current resolved `SocketAddr` hint and `expected_principal`).
    ///
    /// # Errors
    ///
    /// Fails closed before any business frame can be written if `SingleHostDesktop`
    /// is active, `EndpointResolver` has no hint, TCP/TLS 1.3 fails, or the
    /// 5-axis Exporter `CellFence` handshake fails.
    pub async fn get_or_dial_session(
        &self,
        target: CellIdentity,
    ) -> Result<MultiplexedSession, DialSessionError> {
        let Some(state) = &self.active_mode else {
            return Err(DialSessionError::SingleHostProfile(
                SingleHostProfileViolation {
                    attempted_op: "MtlsCellTransport::get_or_dial_session",
                },
            ));
        };

        if target == state.local_cell {
            return Err(DialSessionError::SelfDialForbidden(target));
        }

        let hint = state
            .resolver
            .resolve(target)
            .ok_or(DialSessionError::EndpointUnresolved(target))?;

        // Reuse pooled session ONLY if it is still healthy, its routing hint
        // matches the authoritative EndpointResolver SocketAddr (`[DIST-NAME-001]`),
        // and any expected PrincipalId matches the pinned peer claims.
        {
            let mut pool = state
                .pooled_sessions
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some((cached_addr, session)) = pool.get(&target) {
                let principal_ok = hint
                    .expected_principal
                    .is_none_or(|exp_p| session.peer_session().peer_claims.principal_id == exp_p);
                if *cached_addr == hint.addr_hint && principal_ok && session.is_healthy() {
                    return Ok(session.clone());
                }
                if let Some((_, stale_session)) = pool.remove(&target) {
                    stale_session.close();
                }
            }
        }

        let fresh = self
            .dial_fresh_session_to_addr(state, target, hint.addr_hint, hint.expected_principal)
            .await?;
        {
            state
                .pooled_sessions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(target, (hint.addr_hint, fresh.clone()));
        }
        Ok(fresh)
    }

    async fn dial_fresh_session_to_addr(
        &self,
        state: &MultiHostClientState,
        target: CellIdentity,
        addr_hint: SocketAddr,
        expected_principal: Option<PrincipalId>,
    ) -> Result<MultiplexedSession, DialSessionError> {
        let tcp_stream = match timeout(
            self.config.connect_and_tls_timeout,
            TcpStream::connect(addr_hint),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(err)) => return Err(DialSessionError::TcpConnect(err)),
            Err(_) => return Err(DialSessionError::ConnectTimeout(addr_hint)),
        };
        let _ = tcp_stream.set_nodelay(true);

        let server_name = ServerName::try_from(NLOS_CELL_TLS_SNI)
            .expect("static NLOS_CELL_TLS_SNI is a valid syntactic DNS name");
        let mut tls_stream = match timeout(
            self.config.connect_and_tls_timeout,
            state.tls_connector.connect(server_name, tcp_stream),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(err)) => return Err(DialSessionError::TlsHandshake(err)),
            Err(_) => {
                return Err(DialSessionError::TlsHandshake(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "client TLS 1.3 handshake timeout",
                )));
            }
        };

        let tls_material = extract_tls13_material(tls_stream.get_ref().1)
            .map_err(DialSessionError::FenceHandshake)?;

        let local_claims = state.security.current_local_claims();
        let now_ms = (state.security.clock_ms)();
        let ctx = HandshakeContext {
            local_claims,
            local_signer: state.security.local_signer.as_ref(),
            peer_verifier: state.security.peer_verifier.as_ref(),
            fence_policy: state.security.fence_policy.as_ref(),
            nonce_registry: state.security.nonce_registry.as_ref(),
            now_ms,
            handshake_timeout: self.config.fence_handshake_timeout,
        };

        let peer_session: AuthenticatedPeerSession = run_client_fence_handshake(
            &mut tls_stream,
            &tls_material,
            &ctx,
            Some(target),
            expected_principal,
        )
        .await
        .map_err(DialSessionError::FenceHandshake)?;

        let conn_guard = self.accounting.acquire_connection_guard();
        let (reader, writer) = tokio::io::split(tls_stream);
        Ok(MultiplexedSession::spawn_with_guard(
            reader,
            writer,
            peer_session,
            self.config.mux,
            state.inbound_rpc_handler.clone(),
            state.inbound_bulk_handler.clone(),
            Some(conn_guard),
            None,
        ))
    }

    /// Executes a high-priority `ControlRpc` call and returns both the typed
    /// [`RemoteCallOutcome`] and any underlying [`SocketFailureKind`] observed.
    ///
    /// # Errors
    ///
    /// Fails closed with [`TransportDispatchRefusal`] if `SingleHostDesktop` is
    /// active or if `contract.idempotency_key` is blocked awaiting
    /// reconciliation (`[DIST-FAIL-001]`).
    pub async fn call_cell_rpc_detailed(
        &self,
        target: CellIdentity,
        contract: &RemoteCallContract,
        payload: Vec<u8>,
    ) -> Result<
        (
            RemoteCallOutcome<RpcResponsePayload>,
            Option<SocketFailureKind>,
        ),
        TransportDispatchRefusal,
    > {
        self.profile
            .require_multi_host_socket_permission("MtlsCellTransport::call_cell_rpc")
            .map_err(TransportDispatchRefusal::SingleHostProfile)?;

        self.reconciliation
            .check_before_dispatch(contract.idempotency_key)
            .map_err(TransportDispatchRefusal::BlindRetryBlocked)?;

        let session = match self.get_or_dial_session(target).await {
            Ok(s) => s,
            Err(dial_err) => {
                let kind = dial_err.socket_failure_kind();
                return Ok((
                    classify_transport_failure(
                        contract,
                        WriteProgressPhase::BeforeFirstPayloadByte,
                        kind,
                        0,
                    ),
                    Some(kind),
                ));
            }
        };

        let (outcome, fail_kind) = session
            .call_rpc(
                contract,
                payload,
                self.config.rpc_call_timeout,
                self.config.timeout_elapsed_ticks,
            )
            .await;

        if fail_kind.is_some() {
            self.invalidate_cached_session(target);
        }
        if matches!(
            outcome,
            RemoteCallOutcome::DeadlineExceededUncertain {
                side_effecting: true,
                retry_directive: RemoteRetryDirective::ReconcileBeforeRetry,
                ..
            }
        ) {
            self.reconciliation.record_uncertain_side_effect(
                contract.idempotency_key,
                fail_kind.unwrap_or(SocketFailureKind::ReadTimeout),
            );
        }

        Ok((outcome, fail_kind))
    }

    /// Streams `total_bytes` generated lazily chunk-by-chunk (`<= 64 KiB` per
    /// chunk) under per-stream credit-window backpressure (`ADR-0026` §3.1).
    ///
    /// Calling `next_chunk(seq)` lazily only as window credit permits ensures a
    /// `64 MiB` transfer never materializes `64 MiB` simultaneously in either
    /// sender or receiver RAM.
    ///
    /// # Errors
    ///
    /// Fails closed with [`TransportDispatchRefusal`] if `SingleHostDesktop` is
    /// active, if `contract.idempotency_key` is blocked awaiting reconciliation,
    /// or if any chunk exceeds `MAX_BULK_CHUNK_BYTES`.
    pub async fn stream_bulk_from_producer<F>(
        &self,
        target: CellIdentity,
        contract: &RemoteCallContract,
        total_bytes: u64,
        mut next_chunk: F,
    ) -> Result<
        (
            RemoteCallOutcome<BulkTransferReceipt>,
            Option<SocketFailureKind>,
        ),
        TransportDispatchRefusal,
    >
    where
        F: FnMut(u32) -> Option<Vec<u8>> + Send,
    {
        self.profile
            .require_multi_host_socket_permission("MtlsCellTransport::stream_bulk_from_producer")
            .map_err(TransportDispatchRefusal::SingleHostProfile)?;

        self.reconciliation
            .check_before_dispatch(contract.idempotency_key)
            .map_err(TransportDispatchRefusal::BlindRetryBlocked)?;

        // Pre-pull chunk 0 to validate `BeforeFirstPayloadByte` bounds BEFORE
        // dialing or writing `BulkStreamOpen` onto the wire (`[DIST-FAIL-001]`).
        let first_chunk = next_chunk(0);
        if let Some(ref c0) = first_chunk {
            if c0.is_empty() {
                return Err(TransportDispatchRefusal::BulkChunkEmpty);
            }
            if c0.len() > MAX_BULK_CHUNK_BYTES {
                return Err(TransportDispatchRefusal::BulkChunkExceedsMax {
                    chunk_len: c0.len(),
                    max_len: MAX_BULK_CHUNK_BYTES,
                });
            }
            if (c0.len() as u64) > total_bytes {
                return Err(TransportDispatchRefusal::BulkTotalBytesMismatch {
                    declared: total_bytes,
                    actual: c0.len() as u64,
                });
            }
        } else if total_bytes > 0 {
            return Err(TransportDispatchRefusal::BulkTotalBytesMismatch {
                declared: total_bytes,
                actual: 0,
            });
        }

        let session = match self.get_or_dial_session(target).await {
            Ok(s) => s,
            Err(dial_err) => {
                let kind = dial_err.socket_failure_kind();
                return Ok((
                    classify_transport_failure(
                        contract,
                        WriteProgressPhase::BeforeFirstPayloadByte,
                        kind,
                        0,
                    ),
                    Some(kind),
                ));
            }
        };

        let mut cached_first = first_chunk;
        let mut seq_index = 0u32;
        let producer = || {
            let item = if seq_index == 0 {
                cached_first.take()
            } else {
                next_chunk(seq_index)
            };
            if let Some(chunk) = item {
                seq_index = seq_index.saturating_add(1);
                Some(chunk)
            } else {
                None
            }
        };

        // Once `send_bulk_chunks` writes `BulkStreamOpen` (`AfterFirstPayloadByte`),
        // any mid-stream producer fault (oversize chunk `k >= 1`, empty chunk, or
        // `sent_bytes != total_bytes`) aborts the session inside `send_bulk_chunks`
        // and MUST flow through `invalidate_cached_session` + `reconciliation` below!
        let (outcome, fail_kind) = session
            .send_bulk_chunks(
                contract,
                total_bytes,
                std::iter::from_fn(producer),
                self.config.bulk_transfer_timeout,
                self.config.timeout_elapsed_ticks,
            )
            .await;

        if fail_kind.is_some() {
            self.invalidate_cached_session(target);
        }
        if matches!(
            outcome,
            RemoteCallOutcome::DeadlineExceededUncertain {
                side_effecting: true,
                retry_directive: RemoteRetryDirective::ReconcileBeforeRetry,
                ..
            }
        ) {
            self.reconciliation.record_uncertain_side_effect(
                contract.idempotency_key,
                fail_kind.unwrap_or(SocketFailureKind::ReadTimeout),
            );
        }

        Ok((outcome, fail_kind))
    }
}

impl CellRpcTransport for MtlsCellTransport {
    fn call_cell_rpc<'a>(
        &'a self,
        target: CellIdentity,
        contract: &'a RemoteCallContract,
        payload: Vec<u8>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        RemoteCallOutcome<RpcResponsePayload>,
                        TransportDispatchRefusal,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let (outcome, _) = self
                .call_cell_rpc_detailed(target, contract, payload)
                .await?;
            Ok(outcome)
        })
    }
}

impl BulkStreamTransport for MtlsCellTransport {
    fn stream_bulk_chunks<'a>(
        &'a self,
        target: CellIdentity,
        contract: &'a RemoteCallContract,
        chunks: Vec<Vec<u8>>,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        RemoteCallOutcome<BulkTransferReceipt>,
                        TransportDispatchRefusal,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut total_bytes = 0u64;
            for chunk in &chunks {
                if chunk.is_empty() {
                    return Err(TransportDispatchRefusal::BulkChunkEmpty);
                }
                if chunk.len() > MAX_BULK_CHUNK_BYTES {
                    return Err(TransportDispatchRefusal::BulkChunkExceedsMax {
                        chunk_len: chunk.len(),
                        max_len: MAX_BULK_CHUNK_BYTES,
                    });
                }
                total_bytes = total_bytes.saturating_add(chunk.len() as u64);
            }
            let mut iter = chunks.into_iter();
            let (outcome, _) = self
                .stream_bulk_from_producer(target, contract, total_bytes, move |_| iter.next())
                .await?;
            Ok(outcome)
        })
    }
}
