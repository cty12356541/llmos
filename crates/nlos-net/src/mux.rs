//! Dual-class multiplexed session engine with high-priority `ControlRpc`
//! scheduling and per-stream credit-window `BulkChunk` backpressure
//! (`ADR-0026` §3.1, `[DIST-FAIL-001]`).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nlos_cell::{CellEpoch, RemoteCallContract, RemoteCallOutcome};
use nlos_types::IdempotencyKey;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::timeout;

use crate::failure::{SocketFailureKind, WriteProgressPhase, classify_transport_failure};
use crate::frame::{
    DEFAULT_BULK_WINDOW_BYTES, DEFAULT_MAX_CONTROL_PAYLOAD_BYTES, FrameCodecError,
    FramePriorityClass, MAX_BULK_CHUNK_BYTES, MuxFrame, RpcWireStatus, read_mux_frame,
    write_mux_frame,
};
use crate::handshake::AuthenticatedPeerSession;
use crate::profile::ConnectionSocketGuard;

/// Default upper bound on simultaneously open inbound bulk streams on a single
/// [`MultiplexedSession`], preventing stream-table memory exhaustion.
pub const DEFAULT_MAX_CONCURRENT_BULK_STREAMS: usize = 16;

/// Configuration governing frame limits, timeouts, and per-stream bulk flow
/// control credits on a [`MultiplexedSession`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MuxConfig {
    /// Maximum payload bytes on a single `ControlRpcRequest` / `ControlRpcResponse`.
    pub max_control_payload_bytes: usize,
    /// Strict per-stream credit window (`bytes`) for `BulkChunk` transfers.
    /// Must be `>= MAX_BULK_CHUNK_BYTES` (`64 KiB`).
    pub bulk_window_bytes: usize,
    /// Maximum number of concurrent inbound `BulkStreamOpen` streams permitted
    /// on a single connection before failing closed (`>= 1`).
    pub max_concurrent_bulk_streams: usize,
    /// Per-frame socket write timeout.
    pub frame_write_timeout: Duration,
    /// Idle read timeout on the multiplexed reader loop.
    pub frame_read_timeout: Duration,
    /// Per-RPC response deadline duration.
    pub default_rpc_timeout: Duration,
}

impl Default for MuxConfig {
    fn default() -> Self {
        Self {
            max_control_payload_bytes: DEFAULT_MAX_CONTROL_PAYLOAD_BYTES,
            bulk_window_bytes: DEFAULT_BULK_WINDOW_BYTES,
            max_concurrent_bulk_streams: DEFAULT_MAX_CONCURRENT_BULK_STREAMS,
            frame_write_timeout: Duration::from_secs(5),
            frame_read_timeout: Duration::from_secs(30),
            default_rpc_timeout: Duration::from_secs(5),
        }
    }
}

/// Verified RPC response returned inside `RemoteCallOutcome::Succeeded`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcResponsePayload {
    /// Opaque response bytes from peer handler.
    pub payload: Vec<u8>,
}

/// Server-side response returned by an [`RpcRequestHandler`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RpcHandlerReply {
    /// Responder's current `CellEpoch`.
    pub observed_epoch: CellEpoch,
    /// Whether this reply was served from an idempotency replay cache.
    pub idempotency_replayed: bool,
    /// Execution status (`Ok` vs `PreExecutionUnavailable`).
    pub status: RpcWireStatus,
    /// Opaque response payload bytes.
    pub payload: Vec<u8>,
}

/// Incoming RPC request metadata passed to an [`RpcRequestHandler`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncomingRpcRequest {
    /// Authenticated peer session metadata pinned at handshake.
    pub peer_session: AuthenticatedPeerSession,
    /// Logical call id.
    pub call_id: [u8; 16],
    /// Mandatory idempotency key.
    pub idempotency_key: IdempotencyKey,
    /// Caller logical deadline tick.
    pub deadline_tick: u64,
    /// Whether the caller marked this request side-effecting.
    pub side_effecting: bool,
    /// Request payload bytes.
    pub payload: Vec<u8>,
}

/// Server handler invoked asynchronously for each high-priority `ControlRpcRequest`.
pub trait RpcRequestHandler: Send + Sync + 'static {
    /// Handles one incoming RPC request without blocking the multiplexer reader.
    fn handle_rpc(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>>;
}

/// Verified completion receipt returned when a `BulkStream` transfer finishes
/// end-to-end `SHA-256` verification at the destination Cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkTransferReceipt {
    /// Multiplexed stream id used for this transfer.
    pub stream_id: u64,
    /// Total bytes verified by the receiver.
    pub verified_bytes: u64,
    /// Total `<= 64 KiB` chunks transmitted.
    pub total_chunks: u32,
    /// End-to-end `SHA-256` digest verified by both sender and receiver.
    pub verified_sha256: [u8; 32],
}

/// Metadata accompanying an incoming bulk stream opened by a remote peer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncomingBulkMeta {
    /// Pinned authenticated session context.
    pub peer_session: AuthenticatedPeerSession,
    /// Stream identifier.
    pub stream_id: u64,
    /// Logical call id.
    pub call_id: [u8; 16],
    /// Idempotency key for transfer deduplication.
    pub idempotency_key: IdempotencyKey,
    /// Declared total byte length.
    pub total_bytes: u64,
}

/// Server handler outcomes upon consuming a [`BulkStreamReader`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkHandlerCompletion {
    /// Authoritative `CellEpoch` of the receiving Cell.
    pub observed_epoch: CellEpoch,
    /// Whether this transfer matched an already-materialized idempotent artifact.
    pub idempotency_replayed: bool,
}

/// Server handler invoked when a peer opens a bounded-window bulk stream.
pub trait BulkStreamHandler: Send + Sync + 'static {
    /// Consumes chunks from `reader` at the application's own pace; window
    /// credits are replenished ONLY as chunks are drained from `reader`.
    fn handle_bulk_stream(
        &self,
        meta: IncomingBulkMeta,
        reader: BulkStreamReader,
    ) -> Pin<Box<dyn Future<Output = Result<BulkHandlerCompletion, &'static str>> + Send + 'static>>;
}

/// Pull-based bounded-window reader handed to [`BulkStreamHandler`].
///
/// Replenishes `BulkWindowCredit` to the remote sender ONLY when
/// [`Self::next_chunk`] drains buffered bytes, guaranteeing that slow consumers
/// apply end-to-end backpressure without stalling concurrent `ControlRpc`
/// frames on the same TLS 1.3 connection.
pub struct BulkStreamReader {
    stream_id: u64,
    inbound: Arc<InboundBulkStreamState>,
    high_prio_tx: mpsc::UnboundedSender<OutboundWorkItem>,
    session_buffered_bytes: Arc<AtomicUsize>,
}

impl fmt::Debug for BulkStreamReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BulkStreamReader")
            .field("stream_id", &self.stream_id)
            .finish_non_exhaustive()
    }
}

impl BulkStreamReader {
    /// Awaits and drains the next `<= 64 KiB` chunk, or returns `Ok(None)` once
    /// all chunks have been drained AND end-to-end `SHA-256` verification on
    /// `BulkStreamFinish` succeeded.
    ///
    /// # Errors
    ///
    /// Fails closed if the connection dropped mid-stream, credit window was
    /// violated, or final `SHA-256` / byte count verification failed.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, &'static str> {
        loop {
            let notified = self.inbound.notify.notified();
            {
                let mut guard = self
                    .inbound
                    .queue
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(err) = guard.aborted_reason {
                    return Err(err);
                }
                if let Some(chunk) = guard.chunks.pop_front() {
                    let len = chunk.len();
                    guard.buffered_bytes = guard.buffered_bytes.saturating_sub(len);
                    self.session_buffered_bytes.fetch_sub(len, Ordering::AcqRel);
                    drop(guard);

                    if let Ok(granted_bytes) = u32::try_from(len) {
                        let _ = self.high_prio_tx.send(OutboundWorkItem {
                            frame: MuxFrame::BulkWindowCredit {
                                stream_id: self.stream_id,
                                granted_bytes,
                            },
                            write_started: None,
                            write_done: None,
                        });
                    }
                    return Ok(Some(chunk));
                }
                if guard.finished_verified.is_some() {
                    return Ok(None);
                }
            }
            notified.await;
        }
    }

    /// Returns `(verified_bytes, verified_sha256)` after [`Self::next_chunk`]
    /// has yielded `Ok(None)`.
    #[must_use]
    pub fn verified_digest(&self) -> Option<(u64, [u8; 32])> {
        let guard = self
            .inbound
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.finished_verified
    }
}

impl Drop for BulkStreamReader {
    fn drop(&mut self) {
        let mut guard = self
            .inbound
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if guard.buffered_bytes > 0 {
            let rem = guard.buffered_bytes;
            guard.buffered_bytes = 0;
            guard.chunks.clear();
            self.session_buffered_bytes.fetch_sub(rem, Ordering::AcqRel);
        }
    }
}

struct InboundBulkStreamState {
    expected_total_bytes: u64,
    window_limit: usize,
    hasher: Mutex<Sha256>,
    queue: Mutex<InboundBulkQueue>,
    notify: Notify,
}

struct InboundBulkQueue {
    expected_seq: u32,
    received_bytes: u64,
    buffered_bytes: usize,
    chunks: VecDeque<Vec<u8>>,
    finished_verified: Option<(u64, [u8; 32])>,
    aborted_reason: Option<&'static str>,
}

struct SenderBulkFlowState {
    credit_and_status: Mutex<(usize, Option<SocketFailureKind>)>,
    credit_notify: Notify,
}

struct OutboundWorkItem {
    frame: MuxFrame,
    write_started: Option<Arc<AtomicBool>>,
    write_done: Option<oneshot::Sender<Result<(), (WriteProgressPhase, SocketFailureKind)>>>,
}

type PendingRpcSender = oneshot::Sender<Result<MuxFrame, SocketFailureKind>>;
type PendingBulkAckSender =
    oneshot::Sender<Result<(CellEpoch, u64, [u8; 32], bool), SocketFailureKind>>;

struct SharedMuxState {
    pending_rpcs: Mutex<BTreeMap<u64, PendingRpcSender>>,
    pending_bulk_senders: Mutex<BTreeMap<u64, (Arc<SenderBulkFlowState>, PendingBulkAckSender)>>,
    inbound_bulk_streams: Mutex<BTreeMap<u64, Arc<InboundBulkStreamState>>>,
    terminal_failure: Mutex<Option<SocketFailureKind>>,
    shutdown_tx: watch::Sender<bool>,
    current_bulk_buffered_bytes: Arc<AtomicUsize>,
    peak_bulk_buffered_bytes: AtomicUsize,
    listener_peak_bulk_bytes: Option<Arc<AtomicUsize>>,
}

impl SharedMuxState {
    fn record_failure_and_abort_all(&self, kind: SocketFailureKind) {
        {
            let mut term = self
                .terminal_failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if term.is_none() {
                *term = Some(kind);
            }
        }
        let _ = self.shutdown_tx.send(true);
        {
            let mut rpcs = self
                .pending_rpcs
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (_, tx) in std::mem::take(&mut *rpcs) {
                let _ = tx.send(Err(kind));
            }
        }
        {
            let mut bulks = self
                .pending_bulk_senders
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (_, (flow, ack_tx)) in std::mem::take(&mut *bulks) {
                {
                    let mut g = flow
                        .credit_and_status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    g.1 = Some(kind);
                }
                flow.credit_notify.notify_waiters();
                let _ = ack_tx.send(Err(kind));
            }
        }
        {
            let mut inbound = self
                .inbound_bulk_streams
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (_, st) in std::mem::take(&mut *inbound) {
                {
                    let mut q = st.queue.lock().unwrap_or_else(PoisonError::into_inner);
                    q.aborted_reason = Some("multiplexed connection closed during bulk stream");
                }
                st.notify.notify_waiters();
            }
        }
    }
}

/// Active multiplexed session over an authenticated TLS 1.3 connection.
///
/// Supports concurrent high-priority `ControlRpc` calls and credit-bounded
/// `<= 64 KiB` `BulkChunk` transfers in both directions.
#[derive(Clone)]
pub struct MultiplexedSession {
    peer_session: AuthenticatedPeerSession,
    config: MuxConfig,
    next_stream_id: Arc<AtomicU64>,
    high_prio_tx: mpsc::UnboundedSender<OutboundWorkItem>,
    bulk_prio_tx: mpsc::Sender<OutboundWorkItem>,
    shared: Arc<SharedMuxState>,
}

impl fmt::Debug for MultiplexedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MultiplexedSession")
            .field("peer_session", &self.peer_session)
            .field("config", &self.config)
            .field("peak_bulk_buffered_bytes", &self.peak_bulk_buffered_bytes())
            .finish_non_exhaustive()
    }
}

impl MultiplexedSession {
    /// Spawns the reader and strict-priority writer loops over an authenticated
    /// split stream `(reader, writer)`.
    #[must_use]
    pub fn spawn<R, W>(
        reader: R,
        writer: W,
        peer_session: AuthenticatedPeerSession,
        config: MuxConfig,
        rpc_handler: Option<Arc<dyn RpcRequestHandler>>,
        bulk_handler: Option<Arc<dyn BulkStreamHandler>>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::spawn_with_guard(
            reader,
            writer,
            peer_session,
            config,
            rpc_handler,
            bulk_handler,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(crate) fn spawn_with_guard<R, W>(
        mut reader: R,
        mut writer: W,
        peer_session: AuthenticatedPeerSession,
        mut config: MuxConfig,
        rpc_handler: Option<Arc<dyn RpcRequestHandler>>,
        bulk_handler: Option<Arc<dyn BulkStreamHandler>>,
        conn_guard: Option<ConnectionSocketGuard>,
        listener_peak_bulk_bytes: Option<Arc<AtomicUsize>>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        config.bulk_window_bytes = config.bulk_window_bytes.max(MAX_BULK_CHUNK_BYTES);
        config.max_concurrent_bulk_streams = config.max_concurrent_bulk_streams.max(1);

        let (high_prio_tx, mut high_prio_rx) = mpsc::unbounded_channel::<OutboundWorkItem>();
        // Bounded low-priority queue (capacity 2) so bulk senders cannot pile
        // up un-credited chunks inside writer channel memory.
        let (bulk_prio_tx, mut bulk_prio_rx) = mpsc::channel::<OutboundWorkItem>(2);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let current_bulk_buffered = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(SharedMuxState {
            pending_rpcs: Mutex::new(BTreeMap::new()),
            pending_bulk_senders: Mutex::new(BTreeMap::new()),
            inbound_bulk_streams: Mutex::new(BTreeMap::new()),
            terminal_failure: Mutex::new(None),
            shutdown_tx,
            current_bulk_buffered_bytes: Arc::clone(&current_bulk_buffered),
            peak_bulk_buffered_bytes: AtomicUsize::new(0),
            listener_peak_bulk_bytes,
        });

        // Hold `ConnectionSocketGuard` ONLY inside the active reader/writer I/O
        // tasks (never on `MultiplexedSession` clones) so socket accounting
        // decrements immediately when the underlying connection closes!
        let guard_arc = Arc::new(conn_guard);

        // Spawn strict-priority writer loop: ALWAYS drains `high_prio_rx`
        // (`ControlRpc` + `BulkWindowCredit` + `BulkStreamCompleteAck`) ahead of
        // `bulk_prio_rx` (`BulkChunk`) via `biased;` select!
        let writer_shared = Arc::clone(&shared);
        let writer_guard = Arc::clone(&guard_arc);
        let mut writer_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let _keep_guard = writer_guard;
            loop {
                if *writer_shutdown_rx.borrow() {
                    break;
                }
                let next_item = tokio::select! {
                    biased;
                    _ = writer_shutdown_rx.changed() => break,
                    high = high_prio_rx.recv() => match high {
                        Some(item) => item,
                        None => break,
                    },
                    low = bulk_prio_rx.recv() => match low {
                        Some(item) => item,
                        None => {
                            tokio::select! {
                                biased;
                                _ = writer_shutdown_rx.changed() => break,
                                maybe_high = high_prio_rx.recv() => match maybe_high {
                                    Some(item) => item,
                                    None => break,
                                },
                            }
                        }
                    },
                };

                if let Some(started) = &next_item.write_started {
                    started.store(true, Ordering::Release);
                }

                match write_mux_frame(
                    &mut writer,
                    &next_item.frame,
                    config.max_control_payload_bytes,
                    config.frame_write_timeout,
                )
                .await
                {
                    Ok(()) => {
                        if let Some(done) = next_item.write_done {
                            let _ = done.send(Ok(()));
                        }
                    }
                    Err(err) => {
                        let (phase, kind) = extract_codec_failure(&err);
                        if let Some(done) = next_item.write_done {
                            let _ = done.send(Err((phase, kind)));
                        }
                        writer_shared.record_failure_and_abort_all(kind);
                        break;
                    }
                }
            }
        });

        // Spawn non-blocking reader loop.
        let reader_shared = Arc::clone(&shared);
        let reader_high_tx = high_prio_tx.clone();
        let reader_guard = guard_arc;
        let mut reader_shutdown_rx = shutdown_rx;
        tokio::spawn(async move {
            let _keep_guard = reader_guard;
            loop {
                if *reader_shutdown_rx.borrow() {
                    break;
                }
                let frame_res = tokio::select! {
                    biased;
                    _ = reader_shutdown_rx.changed() => break,
                    res = read_mux_frame(
                        &mut reader,
                        config.max_control_payload_bytes,
                        config.frame_read_timeout,
                    ) => res,
                };
                let frame = match frame_res {
                    Ok(f) => f,
                    Err(err) => {
                        let (_, kind) = extract_codec_failure(&err);
                        reader_shared.record_failure_and_abort_all(kind);
                        break;
                    }
                };

                if !dispatch_inbound_frame(
                    frame,
                    peer_session,
                    config,
                    &reader_shared,
                    &reader_high_tx,
                    rpc_handler.as_ref(),
                    bulk_handler.as_ref(),
                ) {
                    reader_shared.record_failure_and_abort_all(
                        SocketFailureKind::UnexpectedEofOrTlsTruncation,
                    );
                    break;
                }
            }
        });

        Self {
            peer_session,
            config,
            next_stream_id: Arc::new(AtomicU64::new(1)),
            high_prio_tx,
            bulk_prio_tx,
            shared,
        }
    }

    /// Explicitly closes this multiplexed session, waking both reader and writer
    /// loops so the underlying TLS/TCP socket and [`ConnectionSocketGuard`]
    /// drop immediately.
    pub fn close(&self) {
        self.shared
            .record_failure_and_abort_all(SocketFailureKind::UnexpectedEofOrTlsTruncation);
    }

    /// Number of currently open inbound bulk streams on this session.
    #[must_use]
    pub fn active_inbound_bulk_stream_count(&self) -> usize {
        self.shared
            .inbound_bulk_streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Returns the authenticated 5-axis peer session context pinned at handshake.
    #[must_use]
    pub const fn peer_session(&self) -> &AuthenticatedPeerSession {
        &self.peer_session
    }

    /// Returns `true` if the underlying TLS/TCP stream is still healthy.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.shared
            .terminal_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
    }

    /// Current unconsumed bytes buffered across all inbound bulk streams on
    /// this session.
    #[must_use]
    pub fn current_bulk_buffered_bytes(&self) -> usize {
        self.shared
            .current_bulk_buffered_bytes
            .load(Ordering::Acquire)
    }

    /// Peak unconsumed bytes ever simultaneously buffered across inbound bulk
    /// streams on this session (`<= bulk_window_bytes` for single-stream flow).
    #[must_use]
    pub fn peak_bulk_buffered_bytes(&self) -> usize {
        self.shared.peak_bulk_buffered_bytes.load(Ordering::Acquire)
    }

    /// Executes a high-priority `ControlRpc` call with deterministic
    /// `(WriteProgressPhase, SocketFailureKind) -> RemoteCallOutcome` mapping
    /// (`[DIST-FAIL-001]`).
    #[allow(clippy::too_many_lines)]
    pub async fn call_rpc(
        &self,
        contract: &RemoteCallContract,
        payload: Vec<u8>,
        call_timeout: Duration,
        elapsed_ticks_on_timeout: u64,
    ) -> (
        RemoteCallOutcome<RpcResponsePayload>,
        Option<SocketFailureKind>,
    ) {
        if payload.len() > self.config.max_control_payload_bytes {
            return (
                classify_transport_failure(
                    contract,
                    WriteProgressPhase::BeforeFirstPayloadByte,
                    SocketFailureKind::EndpointUnresolved,
                    0,
                ),
                None,
            );
        }

        if let Some(prior_fail) = *self
            .shared
            .terminal_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return (
                classify_transport_failure(
                    contract,
                    WriteProgressPhase::BeforeFirstPayloadByte,
                    prior_fail,
                    0,
                ),
                Some(prior_fail),
            );
        }

        let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let (resp_tx, resp_rx) = oneshot::channel();
        {
            self.shared
                .pending_rpcs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(stream_id, resp_tx);
        }

        let write_started = Arc::new(AtomicBool::new(false));
        let (write_done_tx, write_done_rx) = oneshot::channel();
        let frame = MuxFrame::ControlRpcRequest {
            stream_id,
            call_id: contract.call_id,
            idempotency_key: contract.idempotency_key,
            deadline_tick: contract.deadline_tick,
            side_effecting: contract.side_effecting,
            payload,
        };
        debug_assert_eq!(frame.priority_class(), FramePriorityClass::HighControl);

        if self
            .high_prio_tx
            .send(OutboundWorkItem {
                frame,
                write_started: Some(Arc::clone(&write_started)),
                write_done: Some(write_done_tx),
            })
            .is_err()
        {
            self.shared
                .pending_rpcs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&stream_id);
            let kind = SocketFailureKind::ConnectionReset;
            return (
                classify_transport_failure(
                    contract,
                    WriteProgressPhase::BeforeFirstPayloadByte,
                    kind,
                    0,
                ),
                Some(kind),
            );
        }

        // Wait for frame write completion within `call_timeout` first so any
        // pre-first-byte failure vs mid-write failure is exact.
        let write_res = timeout(call_timeout, write_done_rx).await;
        match write_res {
            Err(_) => {
                self.shared
                    .pending_rpcs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&stream_id);
                let phase = if write_started.load(Ordering::Acquire) {
                    WriteProgressPhase::AfterFirstPayloadByte
                } else {
                    WriteProgressPhase::BeforeFirstPayloadByte
                };
                let kind = SocketFailureKind::WriteTimeout;
                return (
                    classify_transport_failure(contract, phase, kind, elapsed_ticks_on_timeout),
                    Some(kind),
                );
            }
            Ok(Err(_)) => {
                self.shared
                    .pending_rpcs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&stream_id);
                let phase = if write_started.load(Ordering::Acquire) {
                    WriteProgressPhase::AfterFirstPayloadByte
                } else {
                    WriteProgressPhase::BeforeFirstPayloadByte
                };
                let kind = SocketFailureKind::ConnectionReset;
                return (
                    classify_transport_failure(contract, phase, kind, elapsed_ticks_on_timeout),
                    Some(kind),
                );
            }
            Ok(Ok(Err((phase, kind)))) => {
                self.shared
                    .pending_rpcs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&stream_id);
                return (
                    classify_transport_failure(contract, phase, kind, elapsed_ticks_on_timeout),
                    Some(kind),
                );
            }
            Ok(Ok(Ok(()))) => {}
        }

        // Request bytes are now on the wire (`AfterFirstPayloadByte`).
        match timeout(call_timeout, resp_rx).await {
            Ok(Ok(Ok(MuxFrame::ControlRpcResponse {
                observed_epoch,
                idempotency_replayed,
                status,
                payload,
                ..
            }))) => match status {
                RpcWireStatus::Ok => (
                    contract.classify_succeeded(
                        RpcResponsePayload { payload },
                        observed_epoch,
                        idempotency_replayed,
                    ),
                    None,
                ),
                RpcWireStatus::PreExecutionUnavailable => {
                    (contract.classify_pre_dispatch_unavailable(), None)
                }
            },
            Ok(Ok(Ok(_))) => {
                let kind = SocketFailureKind::UnexpectedEofOrTlsTruncation;
                (
                    classify_transport_failure(
                        contract,
                        WriteProgressPhase::AfterFirstPayloadByte,
                        kind,
                        elapsed_ticks_on_timeout,
                    ),
                    Some(kind),
                )
            }
            Ok(Ok(Err(kind))) => (
                classify_transport_failure(
                    contract,
                    WriteProgressPhase::AfterFirstPayloadByte,
                    kind,
                    elapsed_ticks_on_timeout,
                ),
                Some(kind),
            ),
            Ok(Err(_)) => {
                let kind = self
                    .shared
                    .terminal_failure
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .unwrap_or(SocketFailureKind::UnexpectedEofOrTlsTruncation);
                (
                    classify_transport_failure(
                        contract,
                        WriteProgressPhase::AfterFirstPayloadByte,
                        kind,
                        elapsed_ticks_on_timeout,
                    ),
                    Some(kind),
                )
            }
            Err(_) => {
                self.shared
                    .pending_rpcs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&stream_id);
                let kind = SocketFailureKind::ReadTimeout;
                (
                    classify_transport_failure(
                        contract,
                        WriteProgressPhase::AfterFirstPayloadByte,
                        kind,
                        elapsed_ticks_on_timeout,
                    ),
                    Some(kind),
                )
            }
        }
    }

    /// Streams an iterator/generator of `<= 64 KiB` chunks over the bulk
    /// channel, parking whenever `bulk_window_bytes` credits are exhausted until
    /// the receiver's consumer drains buffered chunks and sends
    /// `BulkWindowCredit`.
    #[allow(clippy::too_many_lines)]
    pub async fn send_bulk_chunks<I>(
        &self,
        contract: &RemoteCallContract,
        total_bytes: u64,
        chunks: I,
        transfer_timeout: Duration,
        elapsed_ticks_on_timeout: u64,
    ) -> (
        RemoteCallOutcome<BulkTransferReceipt>,
        Option<SocketFailureKind>,
    )
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let flow = Arc::new(SenderBulkFlowState {
            credit_and_status: Mutex::new((self.config.bulk_window_bytes, None)),
            credit_notify: Notify::new(),
        });
        let (ack_tx, ack_rx) = oneshot::channel();
        {
            self.shared
                .pending_bulk_senders
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(stream_id, (Arc::clone(&flow), ack_tx));
        }

        let cleanup = || {
            self.shared
                .pending_bulk_senders
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&stream_id);
        };

        let open_started = Arc::new(AtomicBool::new(false));
        let post_first_byte = AtomicBool::new(false);
        let run = async {
            // 1. Send BulkStreamOpen
            let (open_done_tx, open_done_rx) = oneshot::channel();
            self.bulk_prio_tx
                .send(OutboundWorkItem {
                    frame: MuxFrame::BulkStreamOpen {
                        stream_id,
                        call_id: contract.call_id,
                        idempotency_key: contract.idempotency_key,
                        total_bytes,
                    },
                    write_started: Some(Arc::clone(&open_started)),
                    write_done: Some(open_done_tx),
                })
                .await
                .map_err(|_| {
                    (
                        WriteProgressPhase::BeforeFirstPayloadByte,
                        SocketFailureKind::ConnectionReset,
                    )
                })?;

            match open_done_rx.await {
                Ok(Ok(())) => {
                    post_first_byte.store(true, Ordering::Release);
                }
                Ok(Err((p, k))) => return Err((p, k)),
                Err(_) => {
                    let p = if open_started.load(Ordering::Acquire) {
                        WriteProgressPhase::AfterFirstPayloadByte
                    } else {
                        WriteProgressPhase::BeforeFirstPayloadByte
                    };
                    return Err((p, SocketFailureKind::ConnectionReset));
                }
            }

            let phase = WriteProgressPhase::AfterFirstPayloadByte;

            // 2. Stream chunks under strict credit window
            let mut hasher = Sha256::new();
            let mut sent_bytes = 0u64;
            let mut chunk_seq = 0u32;

            for chunk in chunks {
                if chunk.is_empty() || chunk.len() > MAX_BULK_CHUNK_BYTES {
                    return Err((
                        phase,
                        SocketFailureKind::TransportIoOther(std::io::ErrorKind::InvalidInput),
                    ));
                }
                if sent_bytes.saturating_add(chunk.len() as u64) > total_bytes {
                    return Err((
                        phase,
                        SocketFailureKind::TransportIoOther(std::io::ErrorKind::InvalidData),
                    ));
                }
                // Wait for credit >= chunk.len()
                loop {
                    let notified = flow.credit_notify.notified();
                    {
                        let mut guard = flow
                            .credit_and_status
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        if let Some(fail_kind) = guard.1 {
                            return Err((phase, fail_kind));
                        }
                        if guard.0 >= chunk.len() {
                            guard.0 -= chunk.len();
                            break;
                        }
                    }
                    notified.await;
                }

                hasher.update(&chunk);
                sent_bytes = sent_bytes.saturating_add(chunk.len() as u64);

                let (chunk_done_tx, chunk_done_rx) = oneshot::channel();
                self.bulk_prio_tx
                    .send(OutboundWorkItem {
                        frame: MuxFrame::BulkChunk {
                            stream_id,
                            chunk_seq,
                            data: chunk,
                        },
                        write_started: None,
                        write_done: Some(chunk_done_tx),
                    })
                    .await
                    .map_err(|_| (phase, SocketFailureKind::ConnectionReset))?;

                match chunk_done_rx.await {
                    Ok(Ok(())) => {}
                    Ok(Err((_, k))) => return Err((phase, k)),
                    Err(_) => return Err((phase, SocketFailureKind::ConnectionReset)),
                }
                chunk_seq = chunk_seq.saturating_add(1);
            }

            if sent_bytes != total_bytes {
                return Err((
                    phase,
                    SocketFailureKind::TransportIoOther(std::io::ErrorKind::InvalidData),
                ));
            }

            let sha256_digest: [u8; 32] = hasher.finalize().into();
            let (fin_done_tx, fin_done_rx) = oneshot::channel();
            self.bulk_prio_tx
                .send(OutboundWorkItem {
                    frame: MuxFrame::BulkStreamFinish {
                        stream_id,
                        total_chunks: chunk_seq,
                        total_bytes: sent_bytes,
                        sha256_digest,
                    },
                    write_started: None,
                    write_done: Some(fin_done_tx),
                })
                .await
                .map_err(|_| (phase, SocketFailureKind::ConnectionReset))?;

            match fin_done_rx.await {
                Ok(Ok(())) => {}
                Ok(Err((_, k))) => return Err((phase, k)),
                Err(_) => return Err((phase, SocketFailureKind::ConnectionReset)),
            }

            // 3. Await receiver end-to-end SHA-256 verification ACK
            match ack_rx.await {
                Ok(Ok((observed_epoch, verified_bytes, verified_sha256, replayed))) => {
                    if verified_bytes != sent_bytes || verified_sha256 != sha256_digest {
                        return Err((phase, SocketFailureKind::UnexpectedEofOrTlsTruncation));
                    }
                    Ok((
                        observed_epoch,
                        replayed,
                        BulkTransferReceipt {
                            stream_id,
                            verified_bytes,
                            total_chunks: chunk_seq,
                            verified_sha256,
                        },
                    ))
                }
                Ok(Err(k)) => Err((phase, k)),
                Err(_) => Err((phase, SocketFailureKind::UnexpectedEofOrTlsTruncation)),
            }
        };

        match timeout(transfer_timeout, run).await {
            Ok(Ok((observed_epoch, replayed, receipt))) => {
                cleanup();
                (
                    contract.classify_succeeded(receipt, observed_epoch, replayed),
                    None,
                )
            }
            Ok(Err((fail_phase, kind))) => {
                cleanup();
                if fail_phase == WriteProgressPhase::AfterFirstPayloadByte {
                    self.shared.record_failure_and_abort_all(kind);
                }
                (
                    classify_transport_failure(
                        contract,
                        fail_phase,
                        kind,
                        elapsed_ticks_on_timeout,
                    ),
                    Some(kind),
                )
            }
            Err(_) => {
                cleanup();
                let fail_phase = if post_first_byte.load(Ordering::Acquire)
                    || open_started.load(Ordering::Acquire)
                {
                    WriteProgressPhase::AfterFirstPayloadByte
                } else {
                    WriteProgressPhase::BeforeFirstPayloadByte
                };
                let kind = SocketFailureKind::ReadTimeout;
                self.shared.record_failure_and_abort_all(kind);
                (
                    classify_transport_failure(
                        contract,
                        fail_phase,
                        kind,
                        elapsed_ticks_on_timeout,
                    ),
                    Some(kind),
                )
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch_inbound_frame(
    frame: MuxFrame,
    peer_session: AuthenticatedPeerSession,
    config: MuxConfig,
    shared: &Arc<SharedMuxState>,
    high_prio_tx: &mpsc::UnboundedSender<OutboundWorkItem>,
    rpc_handler: Option<&Arc<dyn RpcRequestHandler>>,
    bulk_handler: Option<&Arc<dyn BulkStreamHandler>>,
) -> bool {
    match frame {
        MuxFrame::ControlRpcRequest {
            stream_id,
            call_id,
            idempotency_key,
            deadline_tick,
            side_effecting,
            payload,
        } => {
            let tx = high_prio_tx.clone();
            if let Some(handler) = rpc_handler.cloned() {
                let req = IncomingRpcRequest {
                    peer_session,
                    call_id,
                    idempotency_key,
                    deadline_tick,
                    side_effecting,
                    payload,
                };
                tokio::spawn(async move {
                    let reply = handler.handle_rpc(req).await;
                    let _ = tx.send(OutboundWorkItem {
                        frame: MuxFrame::ControlRpcResponse {
                            stream_id,
                            observed_epoch: reply.observed_epoch,
                            idempotency_replayed: reply.idempotency_replayed,
                            status: reply.status,
                            payload: reply.payload,
                        },
                        write_started: None,
                        write_done: None,
                    });
                });
            } else {
                let _ = tx.send(OutboundWorkItem {
                    frame: MuxFrame::ControlRpcResponse {
                        stream_id,
                        observed_epoch: peer_session.local_claims.cell_epoch,
                        idempotency_replayed: false,
                        status: RpcWireStatus::PreExecutionUnavailable,
                        payload: Vec::new(),
                    },
                    write_started: None,
                    write_done: None,
                });
            }
            true
        }
        MuxFrame::ControlRpcResponse { stream_id, .. } => {
            if let Some(resp_tx) = shared
                .pending_rpcs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&stream_id)
            {
                let _ = resp_tx.send(Ok(frame));
            }
            true
        }
        MuxFrame::BulkWindowCredit {
            stream_id,
            granted_bytes,
        } => {
            if let Some(flow) = shared
                .pending_bulk_senders
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&stream_id)
                .map(|(flow, _)| Arc::clone(flow))
            {
                {
                    let mut guard = flow
                        .credit_and_status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    guard.0 = guard.0.saturating_add(granted_bytes as usize);
                }
                flow.credit_notify.notify_waiters();
            }
            true
        }
        MuxFrame::BulkStreamCompleteAck {
            stream_id,
            observed_epoch,
            verified_bytes,
            verified_sha256,
            idempotency_replayed,
        } => {
            if let Some((_, ack_tx)) = shared
                .pending_bulk_senders
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&stream_id)
            {
                let _ = ack_tx.send(Ok((
                    observed_epoch,
                    verified_bytes,
                    verified_sha256,
                    idempotency_replayed,
                )));
            }
            true
        }
        MuxFrame::BulkStreamOpen {
            stream_id,
            call_id,
            idempotency_key,
            total_bytes,
        } => {
            let Some(handler) = bulk_handler.cloned() else {
                return false;
            };
            let inbound = {
                let mut streams = shared
                    .inbound_bulk_streams
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if streams.len() >= config.max_concurrent_bulk_streams.max(1)
                    || streams.contains_key(&stream_id)
                {
                    return false;
                }
                let state = Arc::new(InboundBulkStreamState {
                    expected_total_bytes: total_bytes,
                    window_limit: config.bulk_window_bytes,
                    hasher: Mutex::new(Sha256::new()),
                    queue: Mutex::new(InboundBulkQueue {
                        expected_seq: 0,
                        received_bytes: 0,
                        buffered_bytes: 0,
                        chunks: VecDeque::new(),
                        finished_verified: None,
                        aborted_reason: None,
                    }),
                    notify: Notify::new(),
                });
                streams.insert(stream_id, Arc::clone(&state));
                state
            };

            let reader_handle = BulkStreamReader {
                stream_id,
                inbound: Arc::clone(&inbound),
                high_prio_tx: high_prio_tx.clone(),
                session_buffered_bytes: Arc::clone(&shared.current_bulk_buffered_bytes),
            };
            let meta = IncomingBulkMeta {
                peer_session,
                stream_id,
                call_id,
                idempotency_key,
                total_bytes,
            };
            let tx_ack = high_prio_tx.clone();
            let shared_cleanup = Arc::clone(shared);
            tokio::spawn(async move {
                let res = handler.handle_bulk_stream(meta, reader_handle).await;
                shared_cleanup
                    .inbound_bulk_streams
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&stream_id);
                if let Ok(completion) = res {
                    let verified = inbound
                        .queue
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .finished_verified;
                    if let Some((verified_bytes, verified_sha256)) = verified {
                        let _ = tx_ack.send(OutboundWorkItem {
                            frame: MuxFrame::BulkStreamCompleteAck {
                                stream_id,
                                observed_epoch: completion.observed_epoch,
                                verified_bytes,
                                verified_sha256,
                                idempotency_replayed: completion.idempotency_replayed,
                            },
                            write_started: None,
                            write_done: None,
                        });
                    }
                }
            });
            true
        }
        MuxFrame::BulkChunk {
            stream_id,
            chunk_seq,
            data,
        } => {
            let Some(inbound) = shared
                .inbound_bulk_streams
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&stream_id)
                .cloned()
            else {
                return false;
            };

            let chunk_len = data.len();
            {
                let mut q = inbound.queue.lock().unwrap_or_else(PoisonError::into_inner);
                if q.expected_seq != chunk_seq {
                    q.aborted_reason = Some("out-of-sequence BulkChunk");
                    inbound.notify.notify_waiters();
                    return false;
                }
                // Enforce hard credit-window memory bound!
                if q.buffered_bytes.saturating_add(chunk_len) > inbound.window_limit {
                    q.aborted_reason = Some("peer exceeded BulkWindowCredit limit");
                    inbound.notify.notify_waiters();
                    return false;
                }
                if q.received_bytes.saturating_add(chunk_len as u64) > inbound.expected_total_bytes
                {
                    q.aborted_reason = Some("BulkChunk exceeded declared total_bytes");
                    inbound.notify.notify_waiters();
                    return false;
                }
                inbound
                    .hasher
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .update(&data);
                q.expected_seq = q.expected_seq.saturating_add(1);
                q.received_bytes = q.received_bytes.saturating_add(chunk_len as u64);
                q.buffered_bytes += chunk_len;
                q.chunks.push_back(data);

                let total_buf = shared
                    .current_bulk_buffered_bytes
                    .fetch_add(chunk_len, Ordering::AcqRel)
                    + chunk_len;
                shared
                    .peak_bulk_buffered_bytes
                    .fetch_max(total_buf, Ordering::AcqRel);
                if let Some(listener_peak) = &shared.listener_peak_bulk_bytes {
                    listener_peak.fetch_max(total_buf, Ordering::AcqRel);
                }
            }
            inbound.notify.notify_one();
            true
        }
        MuxFrame::BulkStreamFinish {
            stream_id,
            total_chunks,
            total_bytes,
            sha256_digest,
        } => {
            let Some(inbound) = shared
                .inbound_bulk_streams
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&stream_id)
                .cloned()
            else {
                return false;
            };

            {
                let mut q = inbound.queue.lock().unwrap_or_else(PoisonError::into_inner);
                if q.expected_seq != total_chunks
                    || q.received_bytes != total_bytes
                    || total_bytes != inbound.expected_total_bytes
                {
                    q.aborted_reason = Some("BulkStreamFinish count mismatch");
                    inbound.notify.notify_waiters();
                    return false;
                }
                let computed: [u8; 32] = inbound
                    .hasher
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                    .finalize()
                    .into();
                if computed != sha256_digest {
                    q.aborted_reason = Some("BulkStreamFinish SHA-256 digest mismatch");
                    inbound.notify.notify_waiters();
                    return false;
                }
                q.finished_verified = Some((total_bytes, sha256_digest));
            }
            inbound.notify.notify_waiters();
            true
        }
    }
}

const fn extract_codec_failure(err: &FrameCodecError) -> (WriteProgressPhase, SocketFailureKind) {
    match *err {
        FrameCodecError::Timeout { phase, kind } | FrameCodecError::Io { phase, kind, .. } => {
            (phase, kind)
        }
        FrameCodecError::UnsupportedVersion(_)
        | FrameCodecError::ControlPayloadTooLarge { .. }
        | FrameCodecError::BulkChunkOutOfBounds { .. }
        | FrameCodecError::OuterFrameTooLarge { .. }
        | FrameCodecError::MalformedFrame(_) => (
            WriteProgressPhase::AfterFirstPayloadByte,
            SocketFailureKind::UnexpectedEofOrTlsTruncation,
        ),
    }
}
