//! Deterministic, seedable TCP chaos proxy and directed multi-node fault-injection
//! mesh for `ROAD-C-001-XHOST` H6 multi-machine chaos verification
//! (`ADR-0026` §2.3, §2.4, §5.3; `C-XHOST-GATE`).
//!
//! # Capabilities
//!
//! - **Deterministic Seeded PRNG ([`ChaosPrng`])**: `SplitMix64` generator keyed
//!   by a `u64` seed so packet-drop and RTT-jitter schedules are 100%
//!   reproducible across runs.
//! - **Programmable Link Fault Policy ([`ChaosLinkPolicy`])**:
//!   - Basis-point drop probability (`drop_probability_bp: 0..=10_000`) and
//!     deterministic `N`-th chunk drop (`drop_every_nth_chunk`).
//!   - Base latency (`base_delay_ms`) + deterministic jitter (`0..=jitter_max_ms`).
//!   - Directional one-way blackhole (`c2s_blackhole` vs `s2c_blackhole`).
//!   - Silent TCP half-open blackhole (`half_open_blackhole`) that accepts
//!     connections and parks sockets indefinitely without sending `FIN` or `RST`.
//!   - Phase-exact connection reset ([`ChaosResetMode`]):
//!     `BeforeFirstPayloadByte` (maps to `RemoteCallOutcome::Unavailable`) vs
//!     `AfterFirstPayloadByte` (tracks the 3-turn TLS 1.3 + 5-axis Exporter
//!     handshake and aborts after forwarding `K` post-handshake application
//!     bytes to the server, mapping to
//!     `RemoteCallOutcome::DeadlineExceededUncertain` +
//!     [`crate::failure::PendingReconciliationTracker`]).
//!   - In-transit TLS 1.3 ciphertext bit-flip ([`CiphertextCorruptionRule`])
//!     proving `rustls` AEAD fail-closed rejection with zero corrupted bytes
//!     delivered to application handlers.
//! - **Directed Multi-Node Mesh ([`ChaosDirectedMesh`])**:
//!   Builds an `N x (N - 1)` directed proxy mesh with per-node
//!   [`MonotonicEndpointResolver`] instances supporting node isolation,
//!   directional one-way cuts (`A -> B` dropped while `B -> A` flows), and
//!   triangle asymmetric partitions (`A <-> B` up, `B <-> C` up, `A <-> C`
//!   blackholed).

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nlos_cell::CellIdentity;
use nlos_types::PrincipalId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::resolver::{
    EndpointHintRecord, EndpointResolver, EndpointRoutingVersion, MonotonicEndpointResolver,
};

/// Number of server-to-client response flights in a standard `nlos-net`
/// TLS 1.3 mTLS + 4-step 5-axis TLS Exporter handshake:
///
/// 1. `s2c` flight 1: TLS 1.3 `ServerHello .. Finished`
/// 2. `s2c` flight 2: 5-axis `ServerChallengeAttestation`
/// 3. `s2c` flight 3: 5-axis `ServerReadyAck`
///
/// Any client-to-server bytes written after `s2c` flight 3 has been forwarded
/// to the client are guaranteed to be post-handshake application `MuxFrame`
/// payload bytes (`WriteProgressPhase::AfterFirstPayloadByte`).
pub const HANDSHAKE_S2C_FLIGHT_TURNS: u32 = 3;

/// Deterministic `SplitMix64` pseudo-random number generator for reproducible
/// chaos schedules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChaosPrng {
    state: u64,
}

impl ChaosPrng {
    /// Creates a new deterministic PRNG seeded with `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Generates the next deterministic `u64`.
    pub const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Samples a deterministic integer in `0..=max_inclusive`.
    pub const fn next_range_inclusive(&mut self, max_inclusive: u64) -> u64 {
        if max_inclusive == 0 {
            return 0;
        }
        let span = max_inclusive.saturating_add(1);
        if span == 0 {
            self.next_u64()
        } else {
            self.next_u64() % span
        }
    }

    /// Evaluates a drop decision for `probability_bp` in basis points
    /// (`0..=10_000`, where `10_000 == 100%`).
    pub const fn should_drop_bp(&mut self, probability_bp: u16) -> bool {
        if probability_bp == 0 {
            return false;
        }
        if probability_bp >= 10_000 {
            return true;
        }
        (self.next_u64() % 10_000) < (probability_bp as u64)
    }
}

/// Action taken when a chunk is selected for drop by `drop_probability_bp` or
/// `drop_every_nth_chunk`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChaosDropBehavior {
    /// Drops the chunk and closes the underlying TCP stream so TLS 1.3 callers
    /// observe a clean connection reset/EOF and reconnect on retry.
    #[default]
    SeverConnectionOnDrop,
    /// Silently discards the chunk bytes while keeping the TCP stream open.
    SilentDiscardChunk,
}

/// Phase-exact connection termination (`RST`/EOF) timing (`[DIST-FAIL-001]`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChaosResetMode {
    /// Do not inject automatic connection resets.
    #[default]
    None,
    /// Abort the connection immediately upon accept before any payload byte is
    /// forwarded (`WriteProgressPhase::BeforeFirstPayloadByte` ->
    /// `RemoteCallOutcome::Unavailable`).
    BeforeFirstPayloadByte,
    /// Allow the 3-turn TLS 1.3 + 5-axis Exporter handshake to complete, forward
    /// `after_app_c2s_bytes` (`>= 1`) of post-handshake application payload to
    /// the upstream server, and then immediately abort the connection before
    /// forwarding the server's reply (`WriteProgressPhase::AfterFirstPayloadByte`
    /// -> `RemoteCallOutcome::DeadlineExceededUncertain`).
    AfterFirstPayloadByte {
        /// Post-handshake client-to-server byte threshold before severing.
        after_app_c2s_bytes: usize,
    },
    /// Abort the connection after forwarding `after_raw_c2s_bytes` total raw
    /// client-to-server TCP bytes (regardless of handshake phase).
    AfterRawClientBytes {
        /// Raw client-to-server TCP byte threshold before severing.
        after_raw_c2s_bytes: usize,
    },
}

/// In-transit ciphertext bit-flip rule proving TLS 1.3 AEAD fail-closed
/// rejection (`[TCB-ENDPOINT-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CiphertextCorruptionRule {
    /// If `true`, wait until the 3-turn TLS 1.3 + 5-axis Exporter handshake has
    /// completed before flipping bits so corruption targets an application
    /// `MuxFrame` TLS record.
    pub post_handshake_only: bool,
    /// Stream byte offset at which to apply `xor_mask`.
    pub byte_offset: usize,
    /// Non-zero bitmask `XORed` into the target byte in transit.
    pub xor_mask: u8,
}

/// Programmable fault-injection policy for a [`ChaosTcpProxy`] link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChaosLinkPolicy {
    /// Deterministic PRNG seed.
    pub seed: u64,
    /// Random chunk drop rate in basis points (`0..=10_000`).
    pub drop_probability_bp: u16,
    /// Deterministic every-`N`-th chunk drop schedule.
    pub drop_every_nth_chunk: Option<NonZeroU32>,
    /// Behavior when a chunk is dropped.
    pub drop_behavior: ChaosDropBehavior,
    /// Fixed one-way delay (`ms`) added to each forwarded chunk.
    pub base_delay_ms: u64,
    /// Maximum additional deterministic jitter (`0..=jitter_max_ms`) added to
    /// each forwarded chunk.
    pub jitter_max_ms: u64,
    /// Directional blackhole dropping all client-to-server (`c2s`) bytes.
    pub c2s_blackhole: bool,
    /// Directional blackhole dropping all server-to-client (`s2c`) bytes.
    pub s2c_blackhole: bool,
    /// Silent TCP half-open blackhole: accepts TCP connections and parks them
    /// without reading/forwarding bytes or sending `FIN`/`RST`.
    pub half_open_blackhole: bool,
    /// Phase-exact connection reset mode.
    pub reset_mode: ChaosResetMode,
    /// Optional in-transit ciphertext bit-flip rule.
    pub ciphertext_corruption: Option<CiphertextCorruptionRule>,
}

impl Default for ChaosLinkPolicy {
    fn default() -> Self {
        Self::passthrough(0xC0DE_C0DE_7200_0001)
    }
}

impl ChaosLinkPolicy {
    /// Healthy passthrough policy with deterministic `seed`.
    #[must_use]
    pub const fn passthrough(seed: u64) -> Self {
        Self {
            seed,
            drop_probability_bp: 0,
            drop_every_nth_chunk: None,
            drop_behavior: ChaosDropBehavior::SeverConnectionOnDrop,
            base_delay_ms: 0,
            jitter_max_ms: 0,
            c2s_blackhole: false,
            s2c_blackhole: false,
            half_open_blackhole: false,
            reset_mode: ChaosResetMode::None,
            ciphertext_corruption: None,
        }
    }
}

/// Observable telemetry counters recorded by a [`ChaosTcpProxy`] link.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChaosLinkMetricsSnapshot {
    /// Total TCP connections accepted by the proxy listener.
    pub connections_accepted: u64,
    /// Total client-to-server chunks forwarded.
    pub c2s_chunks_forwarded: u64,
    /// Total server-to-client chunks forwarded.
    pub s2c_chunks_forwarded: u64,
    /// Total raw client-to-server bytes forwarded.
    pub c2s_bytes_forwarded: u64,
    /// Total raw server-to-client bytes forwarded.
    pub s2c_bytes_forwarded: u64,
    /// Total post-handshake application client-to-server bytes forwarded.
    pub app_c2s_bytes_forwarded: u64,
    /// Total chunks dropped by `drop_probability_bp` or `drop_every_nth_chunk`.
    pub chunks_dropped: u64,
    /// Total chunks absorbed by `c2s_blackhole`.
    pub c2s_blackholed_chunks: u64,
    /// Total chunks absorbed by `s2c_blackhole`.
    pub s2c_blackholed_chunks: u64,
    /// Connections terminated in `BeforeFirstPayloadByte` phase.
    pub connections_reset_before_payload: u64,
    /// Connections terminated in `AfterFirstPayloadByte` (or raw threshold) phase.
    pub connections_reset_after_payload: u64,
    /// Connections stalled in `half_open_blackhole` mode.
    pub half_open_stalls_triggered: u64,
    /// In-transit ciphertext bit-flips injected.
    pub ciphertext_corruptions_injected: u64,
    /// Cumulative artificial delay (`ms`) injected across forwarded chunks.
    pub injected_delay_total_ms: u64,
}

impl ChaosLinkMetricsSnapshot {
    /// Accumulates `other` into `self`.
    pub fn merge(&mut self, other: &Self) {
        self.connections_accepted = self
            .connections_accepted
            .saturating_add(other.connections_accepted);
        self.c2s_chunks_forwarded = self
            .c2s_chunks_forwarded
            .saturating_add(other.c2s_chunks_forwarded);
        self.s2c_chunks_forwarded = self
            .s2c_chunks_forwarded
            .saturating_add(other.s2c_chunks_forwarded);
        self.c2s_bytes_forwarded = self
            .c2s_bytes_forwarded
            .saturating_add(other.c2s_bytes_forwarded);
        self.s2c_bytes_forwarded = self
            .s2c_bytes_forwarded
            .saturating_add(other.s2c_bytes_forwarded);
        self.app_c2s_bytes_forwarded = self
            .app_c2s_bytes_forwarded
            .saturating_add(other.app_c2s_bytes_forwarded);
        self.chunks_dropped = self.chunks_dropped.saturating_add(other.chunks_dropped);
        self.c2s_blackholed_chunks = self
            .c2s_blackholed_chunks
            .saturating_add(other.c2s_blackholed_chunks);
        self.s2c_blackholed_chunks = self
            .s2c_blackholed_chunks
            .saturating_add(other.s2c_blackholed_chunks);
        self.connections_reset_before_payload = self
            .connections_reset_before_payload
            .saturating_add(other.connections_reset_before_payload);
        self.connections_reset_after_payload = self
            .connections_reset_after_payload
            .saturating_add(other.connections_reset_after_payload);
        self.half_open_stalls_triggered = self
            .half_open_stalls_triggered
            .saturating_add(other.half_open_stalls_triggered);
        self.ciphertext_corruptions_injected = self
            .ciphertext_corruptions_injected
            .saturating_add(other.ciphertext_corruptions_injected);
        self.injected_delay_total_ms = self
            .injected_delay_total_ms
            .saturating_add(other.injected_delay_total_ms);
    }
}

#[derive(Debug, Default)]
struct AtomicChaosMetrics {
    connections_accepted: AtomicU64,
    c2s_chunks_forwarded: AtomicU64,
    s2c_chunks_forwarded: AtomicU64,
    c2s_bytes_forwarded: AtomicU64,
    s2c_bytes_forwarded: AtomicU64,
    app_c2s_bytes_forwarded: AtomicU64,
    chunks_dropped: AtomicU64,
    c2s_blackholed_chunks: AtomicU64,
    s2c_blackholed_chunks: AtomicU64,
    connections_reset_before_payload: AtomicU64,
    connections_reset_after_payload: AtomicU64,
    half_open_stalls_triggered: AtomicU64,
    ciphertext_corruptions_injected: AtomicU64,
    injected_delay_total_ms: AtomicU64,
}

impl AtomicChaosMetrics {
    fn snapshot(&self) -> ChaosLinkMetricsSnapshot {
        ChaosLinkMetricsSnapshot {
            connections_accepted: self.connections_accepted.load(Ordering::Acquire),
            c2s_chunks_forwarded: self.c2s_chunks_forwarded.load(Ordering::Acquire),
            s2c_chunks_forwarded: self.s2c_chunks_forwarded.load(Ordering::Acquire),
            c2s_bytes_forwarded: self.c2s_bytes_forwarded.load(Ordering::Acquire),
            s2c_bytes_forwarded: self.s2c_bytes_forwarded.load(Ordering::Acquire),
            app_c2s_bytes_forwarded: self.app_c2s_bytes_forwarded.load(Ordering::Acquire),
            chunks_dropped: self.chunks_dropped.load(Ordering::Acquire),
            c2s_blackholed_chunks: self.c2s_blackholed_chunks.load(Ordering::Acquire),
            s2c_blackholed_chunks: self.s2c_blackholed_chunks.load(Ordering::Acquire),
            connections_reset_before_payload: self
                .connections_reset_before_payload
                .load(Ordering::Acquire),
            connections_reset_after_payload: self
                .connections_reset_after_payload
                .load(Ordering::Acquire),
            half_open_stalls_triggered: self.half_open_stalls_triggered.load(Ordering::Acquire),
            ciphertext_corruptions_injected: self
                .ciphertext_corruptions_injected
                .load(Ordering::Acquire),
            injected_delay_total_ms: self.injected_delay_total_ms.load(Ordering::Acquire),
        }
    }
}

#[derive(Debug)]
struct MutableChaosState {
    policy: ChaosLinkPolicy,
    prng: ChaosPrng,
    c2s_chunk_seq: u32,
}

#[derive(Debug)]
struct SharedChaosLink {
    state: Mutex<MutableChaosState>,
    metrics: AtomicChaosMetrics,
    abort_epoch_tx: watch::Sender<u64>,
    abort_epoch_rx: watch::Receiver<u64>,
}

/// Live controller handle for inspecting metrics and reconfiguring faults on a
/// running [`ChaosTcpProxy`].
#[derive(Clone, Debug)]
pub struct ChaosLinkController {
    shared: Arc<SharedChaosLink>,
}

impl ChaosLinkController {
    fn new(initial_policy: ChaosLinkPolicy) -> Self {
        let (abort_epoch_tx, abort_epoch_rx) = watch::channel(0u64);
        Self {
            shared: Arc::new(SharedChaosLink {
                state: Mutex::new(MutableChaosState {
                    policy: initial_policy,
                    prng: ChaosPrng::new(initial_policy.seed),
                    c2s_chunk_seq: 0,
                }),
                metrics: AtomicChaosMetrics::default(),
                abort_epoch_tx,
                abort_epoch_rx,
            }),
        }
    }

    /// Returns the currently active [`ChaosLinkPolicy`].
    #[must_use]
    pub fn policy(&self) -> ChaosLinkPolicy {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .policy
    }

    /// Replaces the active [`ChaosLinkPolicy`] and reseeds the deterministic
    /// PRNG if `new_policy.seed` changed.
    pub fn set_policy(&self, new_policy: ChaosLinkPolicy) {
        let mut guard = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if guard.policy.seed != new_policy.seed {
            guard.prng = ChaosPrng::new(new_policy.seed);
        }
        guard.policy = new_policy;
        guard.c2s_chunk_seq = 0;
    }

    /// Mutates the active [`ChaosLinkPolicy`] in place without resetting the
    /// PRNG state.
    pub fn update_policy<F>(&self, mutator: F)
    where
        F: FnOnce(&mut ChaosLinkPolicy),
    {
        let mut guard = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        mutator(&mut guard.policy);
    }

    /// Returns a point-in-time [`ChaosLinkMetricsSnapshot`].
    #[must_use]
    pub fn metrics(&self) -> ChaosLinkMetricsSnapshot {
        self.shared.metrics.snapshot()
    }

    /// Aborts all currently open relayed TCP connections on this proxy link
    /// immediately.
    pub fn abort_active_connections(&self) {
        let next = self.shared.abort_epoch_rx.borrow().wrapping_add(1);
        let _ = self.shared.abort_epoch_tx.send(next);
    }

    /// Restores the link to a healthy passthrough policy and aborts any stalled
    /// half-open or partitioned connections so subsequent dials establish clean
    /// sessions immediately.
    pub fn heal(&self) {
        {
            let mut guard = self
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let seed = guard.policy.seed;
            guard.policy = ChaosLinkPolicy::passthrough(seed);
            guard.c2s_chunk_seq = 0;
        }
        self.abort_active_connections();
    }

    /// Enables or disables client-to-server (`c2s`) directional blackhole.
    pub fn cut_c2s(&self, blackhole: bool) {
        self.update_policy(|p| p.c2s_blackhole = blackhole);
    }

    /// Enables or disables server-to-client (`s2c`) directional blackhole.
    pub fn cut_s2c(&self, blackhole: bool) {
        self.update_policy(|p| p.s2c_blackhole = blackhole);
    }

    /// Places the link into silent TCP half-open blackhole mode and aborts
    /// previously healthy pooled connections so new operations hit the stall.
    pub fn enter_half_open_blackhole(&self) {
        self.update_policy(|p| p.half_open_blackhole = true);
        self.abort_active_connections();
    }

    /// Arms `ChaosResetMode::BeforeFirstPayloadByte` and closes any pooled
    /// connections so the next operation fails before its first payload byte.
    pub fn arm_reset_before_first_payload_byte(&self) {
        self.update_policy(|p| {
            p.reset_mode = ChaosResetMode::BeforeFirstPayloadByte;
            p.half_open_blackhole = false;
        });
        self.abort_active_connections();
    }

    /// Arms `ChaosResetMode::AfterFirstPayloadByte { after_app_c2s_bytes }` and
    /// closes any pooled connections so the next operation completes the 3-turn
    /// TLS 1.3 + 5-axis Exporter handshake, forwards `after_app_c2s_bytes` of
    /// application payload to the server, and then severs the connection before
    /// the server response returns.
    pub fn arm_reset_after_first_payload_byte(&self, after_app_c2s_bytes: usize) {
        self.update_policy(|p| {
            p.reset_mode = ChaosResetMode::AfterFirstPayloadByte {
                after_app_c2s_bytes: after_app_c2s_bytes.max(1),
            };
            p.half_open_blackhole = false;
        });
        self.abort_active_connections();
    }

    /// Arms an in-transit TLS 1.3 ciphertext bit-flip rule and closes pooled
    /// connections so the next operation exercises the rule deterministically.
    pub fn arm_ciphertext_corruption(&self, rule: CiphertextCorruptionRule) {
        self.update_policy(|p| {
            p.ciphertext_corruption = Some(rule);
        });
        self.abort_active_connections();
    }

    /// Configures artificial base latency and deterministic jitter (`ms`).
    pub fn set_latency_and_jitter(&self, base_delay_ms: u64, jitter_max_ms: u64) {
        self.update_policy(|p| {
            p.base_delay_ms = base_delay_ms;
            p.jitter_max_ms = jitter_max_ms;
        });
    }
}

#[derive(Debug, Default)]
struct PerConnectionTurnTracker {
    c2s_had_bytes_since_last_s2c: bool,
    s2c_turns_completed: u32,
    raw_c2s_bytes: usize,
    app_c2s_bytes: usize,
    corruption_applied: bool,
}

/// Deterministic TCP fault-injection proxy relaying between a local ephemeral
/// listener and an upstream `SocketAddr`.
pub struct ChaosTcpProxy {
    listen_addr: SocketAddr,
    upstream_target: SocketAddr,
    controller: ChaosLinkController,
    shutdown_tx: watch::Sender<bool>,
    accept_handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ChaosTcpProxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChaosTcpProxy")
            .field("listen_addr", &self.listen_addr)
            .field("upstream_target", &self.upstream_target)
            .field("metrics", &self.controller.metrics())
            .finish_non_exhaustive()
    }
}

impl Drop for ChaosTcpProxy {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.controller.abort_active_connections();
    }
}

impl ChaosTcpProxy {
    /// Binds a new [`ChaosTcpProxy`] on `127.0.0.1:0` forwarding to
    /// `upstream_target`.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if binding the local TCP listener fails.
    pub async fn bind(
        upstream_target: SocketAddr,
        initial_policy: ChaosLinkPolicy,
    ) -> io::Result<Self> {
        let bind_addr: SocketAddr = ([127, 0, 0, 1], 0).into();
        Self::bind_on(bind_addr, upstream_target, initial_policy).await
    }

    /// Binds a new [`ChaosTcpProxy`] on `listen_addr` forwarding to
    /// `upstream_target`.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if binding `listen_addr` fails.
    pub async fn bind_on(
        listen_addr: SocketAddr,
        upstream_target: SocketAddr,
        initial_policy: ChaosLinkPolicy,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(listen_addr).await?;
        let bound_addr = listener.local_addr()?;
        let controller = ChaosLinkController::new(initial_policy);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        let shared = Arc::clone(&controller.shared);
        let accept_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        let Ok((client_stream, _peer_addr)) = accepted else {
                            continue;
                        };
                        let _ = client_stream.set_nodelay(true);
                        shared
                            .metrics
                            .connections_accepted
                            .fetch_add(1, Ordering::Release);

                        let conn_shared = Arc::clone(&shared);
                        let conn_shutdown_rx = shutdown_rx.clone();
                        tokio::spawn(async move {
                            run_proxied_connection(
                                client_stream,
                                upstream_target,
                                conn_shared,
                                conn_shutdown_rx,
                            )
                            .await;
                        });
                    }
                }
            }
        });

        Ok(Self {
            listen_addr: bound_addr,
            upstream_target,
            controller,
            shutdown_tx,
            accept_handle: Some(accept_handle),
        })
    }

    /// Local `SocketAddr` where clients connect to reach `upstream_target`
    /// through this chaos proxy.
    #[must_use]
    pub const fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Upstream target `SocketAddr` to which this proxy forwards traffic.
    #[must_use]
    pub const fn upstream_target(&self) -> SocketAddr {
        self.upstream_target
    }

    /// Cloneable [`ChaosLinkController`] for dynamic fault injection and
    /// telemetry inspection.
    #[must_use]
    pub fn controller(&self) -> ChaosLinkController {
        self.controller.clone()
    }

    /// Snapshot of current link telemetry counters.
    #[must_use]
    pub fn metrics(&self) -> ChaosLinkMetricsSnapshot {
        self.controller.metrics()
    }

    /// Gracefully stops the proxy listener and aborts all active relayed
    /// connections.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        self.controller.abort_active_connections();
        if let Some(handle) = self.accept_handle.take() {
            let _ = handle.await;
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_proxied_connection(
    client_stream: TcpStream,
    upstream_target: SocketAddr,
    shared: Arc<SharedChaosLink>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut abort_rx = shared.abort_epoch_rx.clone();
    let initial_abort_epoch = *abort_rx.borrow_and_update();

    let initial_policy = shared
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .policy;

    // 1. Check `BeforeFirstPayloadByte` immediate reset before connecting upstream.
    if initial_policy.reset_mode == ChaosResetMode::BeforeFirstPayloadByte {
        shared
            .metrics
            .connections_reset_before_payload
            .fetch_add(1, Ordering::Release);
        drop(client_stream);
        return;
    }

    // 2. Check silent `half_open_blackhole` on accept: hold `client_stream` open
    // without forwarding or sending FIN/RST until aborted or shut down.
    if initial_policy.half_open_blackhole {
        shared
            .metrics
            .half_open_stalls_triggered
            .fetch_add(1, Ordering::Release);
        let _keep_client_open = client_stream;
        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => break,
                changed = abort_rx.changed() => {
                    if changed.is_err() || *abort_rx.borrow() != initial_abort_epoch {
                        break;
                    }
                }
            }
        }
        return;
    }

    let Ok(upstream_stream) = TcpStream::connect(upstream_target).await else {
        drop(client_stream);
        return;
    };
    let _ = upstream_stream.set_nodelay(true);

    let (mut client_read, mut client_write) = client_stream.into_split();
    let (mut server_read, mut server_write) = upstream_stream.into_split();

    let tracker = Arc::new(Mutex::new(PerConnectionTurnTracker::default()));
    let conn_kill = Arc::new(AtomicBool::new(false));
    let (conn_kill_tx, mut conn_kill_rx_s2c) = watch::channel(false);
    let mut conn_kill_rx_c2s = conn_kill_tx.subscribe();

    let shared_c2s = Arc::clone(&shared);
    let tracker_c2s = Arc::clone(&tracker);
    let kill_c2s = Arc::clone(&conn_kill);
    let kill_tx_c2s = conn_kill_tx.clone();

    let c2s_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            if kill_c2s.load(Ordering::Acquire) {
                break;
            }
            let read_res = tokio::select! {
                biased;
                _ = conn_kill_rx_c2s.changed() => break,
                res = client_read.read(&mut buf) => res,
            };
            let Ok(n) = read_res else {
                break;
            };
            if n == 0 {
                let _ = server_write.shutdown().await;
                break;
            }

            let (
                policy,
                delay_ms,
                should_drop,
                reset_before,
                reset_after_bytes,
                corrupt_idx_and_mask,
                is_post_handshake,
            ) = {
                let mut state_guard = shared_c2s
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let mut turn_guard = tracker_c2s.lock().unwrap_or_else(PoisonError::into_inner);

                let policy = state_guard.policy;
                let is_post_hs = turn_guard.s2c_turns_completed >= HANDSHAKE_S2C_FLIGHT_TURNS;
                turn_guard.c2s_had_bytes_since_last_s2c = true;

                // Evaluate delay + deterministic jitter
                let jitter = if policy.jitter_max_ms > 0 {
                    state_guard.prng.next_range_inclusive(policy.jitter_max_ms)
                } else {
                    0
                };
                let delay_ms = policy.base_delay_ms.saturating_add(jitter);

                // Evaluate chunk drop only when not in handshake (or if raw drop configured)
                state_guard.c2s_chunk_seq = state_guard.c2s_chunk_seq.saturating_add(1);
                let seq = state_guard.c2s_chunk_seq;
                let nth_drop = policy
                    .drop_every_nth_chunk
                    .is_some_and(|nth| seq.is_multiple_of(nth.get()));
                let prob_drop = state_guard.prng.should_drop_bp(policy.drop_probability_bp);
                let should_drop = nth_drop || prob_drop;

                let reset_before = policy.reset_mode == ChaosResetMode::BeforeFirstPayloadByte;

                let prev_raw = turn_guard.raw_c2s_bytes;
                let prev_app = turn_guard.app_c2s_bytes;
                let next_raw = prev_raw.saturating_add(n);
                let next_app = if is_post_hs {
                    prev_app.saturating_add(n)
                } else {
                    prev_app
                };

                let reset_after_bytes = match policy.reset_mode {
                    ChaosResetMode::AfterFirstPayloadByte {
                        after_app_c2s_bytes,
                    } if is_post_hs && next_app >= after_app_c2s_bytes.max(1) => {
                        let needed = after_app_c2s_bytes.max(1).saturating_sub(prev_app);
                        Some(needed.clamp(1, n))
                    }
                    ChaosResetMode::AfterRawClientBytes {
                        after_raw_c2s_bytes,
                    } if next_raw >= after_raw_c2s_bytes.max(1) => {
                        let needed = after_raw_c2s_bytes.max(1).saturating_sub(prev_raw);
                        Some(needed.clamp(1, n))
                    }
                    _ => None,
                };

                let corrupt_idx_and_mask = if !turn_guard.corruption_applied
                    && let Some(rule) = policy.ciphertext_corruption
                    && (!rule.post_handshake_only || is_post_hs)
                {
                    let stream_prev = if rule.post_handshake_only {
                        prev_app
                    } else {
                        prev_raw
                    };
                    let stream_next = stream_prev.saturating_add(n);
                    if rule.byte_offset >= stream_prev && rule.byte_offset < stream_next {
                        turn_guard.corruption_applied = true;
                        Some((rule.byte_offset - stream_prev, rule.xor_mask.max(1)))
                    } else {
                        None
                    }
                } else {
                    None
                };

                turn_guard.raw_c2s_bytes = next_raw;
                turn_guard.app_c2s_bytes = next_app;

                (
                    policy,
                    delay_ms,
                    should_drop,
                    reset_before,
                    reset_after_bytes,
                    corrupt_idx_and_mask,
                    is_post_hs,
                )
            };

            if reset_before {
                shared_c2s
                    .metrics
                    .connections_reset_before_payload
                    .fetch_add(1, Ordering::Release);
                kill_c2s.store(true, Ordering::Release);
                let _ = kill_tx_c2s.send(true);
                break;
            }

            if policy.half_open_blackhole {
                shared_c2s
                    .metrics
                    .half_open_stalls_triggered
                    .fetch_add(1, Ordering::Release);
                let _ = conn_kill_rx_c2s.changed().await;
                break;
            }

            if policy.c2s_blackhole {
                shared_c2s
                    .metrics
                    .c2s_blackholed_chunks
                    .fetch_add(1, Ordering::Release);
                continue;
            }

            if should_drop {
                shared_c2s
                    .metrics
                    .chunks_dropped
                    .fetch_add(1, Ordering::Release);
                match policy.drop_behavior {
                    ChaosDropBehavior::SeverConnectionOnDrop => {
                        kill_c2s.store(true, Ordering::Release);
                        let _ = kill_tx_c2s.send(true);
                        break;
                    }
                    ChaosDropBehavior::SilentDiscardChunk => continue,
                }
            }

            if delay_ms > 0 {
                shared_c2s
                    .metrics
                    .injected_delay_total_ms
                    .fetch_add(delay_ms, Ordering::Release);
                sleep(Duration::from_millis(delay_ms)).await;
            }

            if let Some((idx, mask)) = corrupt_idx_and_mask {
                buf[idx] ^= mask;
                shared_c2s
                    .metrics
                    .ciphertext_corruptions_injected
                    .fetch_add(1, Ordering::Release);
            }

            if let Some(forward_prefix_len) = reset_after_bytes {
                if server_write
                    .write_all(&buf[..forward_prefix_len])
                    .await
                    .is_ok()
                {
                    let _ = server_write.flush().await;
                    shared_c2s
                        .metrics
                        .c2s_chunks_forwarded
                        .fetch_add(1, Ordering::Release);
                    shared_c2s
                        .metrics
                        .c2s_bytes_forwarded
                        .fetch_add(forward_prefix_len as u64, Ordering::Release);
                    if is_post_handshake {
                        shared_c2s
                            .metrics
                            .app_c2s_bytes_forwarded
                            .fetch_add(forward_prefix_len as u64, Ordering::Release);
                    }
                }
                shared_c2s
                    .metrics
                    .connections_reset_after_payload
                    .fetch_add(1, Ordering::Release);
                kill_c2s.store(true, Ordering::Release);
                let _ = kill_tx_c2s.send(true);
                break;
            }

            if server_write.write_all(&buf[..n]).await.is_err() {
                break;
            }
            let _ = server_write.flush().await;
            shared_c2s
                .metrics
                .c2s_chunks_forwarded
                .fetch_add(1, Ordering::Release);
            shared_c2s
                .metrics
                .c2s_bytes_forwarded
                .fetch_add(n as u64, Ordering::Release);
            if is_post_handshake {
                shared_c2s
                    .metrics
                    .app_c2s_bytes_forwarded
                    .fetch_add(n as u64, Ordering::Release);
            }
        }
    });

    let shared_s2c = Arc::clone(&shared);
    let tracker_s2c = Arc::clone(&tracker);
    let kill_s2c = Arc::clone(&conn_kill);
    let kill_tx_s2c = conn_kill_tx.clone();

    let s2c_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            if kill_s2c.load(Ordering::Acquire) {
                break;
            }
            let read_res = tokio::select! {
                biased;
                _ = conn_kill_rx_s2c.changed() => break,
                res = server_read.read(&mut buf) => res,
            };
            let Ok(n) = read_res else {
                break;
            };
            if n == 0 {
                let _ = client_write.shutdown().await;
                break;
            }

            let (policy, delay_ms) = {
                let mut state_guard = shared_s2c
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let mut turn_guard = tracker_s2c.lock().unwrap_or_else(PoisonError::into_inner);
                if turn_guard.c2s_had_bytes_since_last_s2c {
                    turn_guard.s2c_turns_completed =
                        turn_guard.s2c_turns_completed.saturating_add(1);
                    turn_guard.c2s_had_bytes_since_last_s2c = false;
                }
                let policy = state_guard.policy;
                let jitter = if policy.jitter_max_ms > 0 {
                    state_guard.prng.next_range_inclusive(policy.jitter_max_ms)
                } else {
                    0
                };
                (policy, policy.base_delay_ms.saturating_add(jitter))
            };

            if policy.half_open_blackhole {
                shared_s2c
                    .metrics
                    .half_open_stalls_triggered
                    .fetch_add(1, Ordering::Release);
                let _ = conn_kill_rx_s2c.changed().await;
                break;
            }

            if policy.s2c_blackhole {
                shared_s2c
                    .metrics
                    .s2c_blackholed_chunks
                    .fetch_add(1, Ordering::Release);
                continue;
            }

            if delay_ms > 0 {
                shared_s2c
                    .metrics
                    .injected_delay_total_ms
                    .fetch_add(delay_ms, Ordering::Release);
                sleep(Duration::from_millis(delay_ms)).await;
            }

            if kill_s2c.load(Ordering::Acquire) {
                break;
            }

            if client_write.write_all(&buf[..n]).await.is_err() {
                break;
            }
            let _ = client_write.flush().await;
            shared_s2c
                .metrics
                .s2c_chunks_forwarded
                .fetch_add(1, Ordering::Release);
            shared_s2c
                .metrics
                .s2c_bytes_forwarded
                .fetch_add(n as u64, Ordering::Release);
        }
        let _ = kill_tx_s2c.send(true);
    });

    let mut c2s_handle = c2s_task;
    let mut s2c_handle = s2c_task;

    tokio::select! {
        biased;
        _ = shutdown_rx.changed() => {
            conn_kill.store(true, Ordering::Release);
            let _ = conn_kill_tx.send(true);
            c2s_handle.abort();
            s2c_handle.abort();
        }
        changed = abort_rx.changed() => {
            if changed.is_err() || *abort_rx.borrow() != initial_abort_epoch {
                conn_kill.store(true, Ordering::Release);
                let _ = conn_kill_tx.send(true);
                c2s_handle.abort();
                s2c_handle.abort();
            }
        }
        _ = &mut c2s_handle => {
            if conn_kill.load(Ordering::Acquire) {
                let _ = conn_kill_tx.send(true);
                s2c_handle.abort();
            } else {
                let _ = s2c_handle.await;
            }
        }
        _ = &mut s2c_handle => {
            if conn_kill.load(Ordering::Acquire) {
                let _ = conn_kill_tx.send(true);
                c2s_handle.abort();
            } else {
                let _ = c2s_handle.await;
            }
        }
    }
}

/// Node endpoint descriptor used to build a [`ChaosDirectedMesh`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChaosNodeDescriptor {
    /// Nominal 16-byte `CellIdentity`.
    pub cell_identity: CellIdentity,
    /// Real bound `MtlsEndpointListener` address of this node.
    pub real_listen_addr: SocketAddr,
    /// Expected `PrincipalId` for `EndpointHintRecord`.
    pub expected_principal: Option<PrincipalId>,
    /// Monotonic routing version watermark for `EndpointResolver`.
    pub routing_version: EndpointRoutingVersion,
}

/// Directed `N x (N - 1)` mesh of [`ChaosTcpProxy`] instances for multi-node
/// chaos testing (`ROAD-C-001-XHOST`).
///
/// Each node `from_cell` gets its own [`MonotonicEndpointResolver`] where every
/// peer `to_cell != from_cell` resolves to the dedicated proxy link
/// `(from_cell -> to_cell)`.
pub struct ChaosDirectedMesh {
    nodes: Vec<ChaosNodeDescriptor>,
    resolvers: BTreeMap<CellIdentity, Arc<MonotonicEndpointResolver>>,
    links: BTreeMap<(CellIdentity, CellIdentity), ChaosTcpProxy>,
}

impl std::fmt::Debug for ChaosDirectedMesh {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChaosDirectedMesh")
            .field("node_count", &self.nodes.len())
            .field("link_count", &self.links.len())
            .finish_non_exhaustive()
    }
}

impl ChaosDirectedMesh {
    /// Binds a full directed mesh of [`ChaosTcpProxy`] links across `nodes`.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if any proxy listener fails to bind.
    pub async fn bind(nodes: &[ChaosNodeDescriptor], base_seed: u64) -> io::Result<Self> {
        let mut resolvers = BTreeMap::new();
        let mut links = BTreeMap::new();
        let mut link_idx = 0u64;

        for from in nodes {
            let resolver = Arc::new(MonotonicEndpointResolver::new());
            // Register self real address
            let _ = resolver.upsert_hint(EndpointHintRecord {
                cell: from.cell_identity,
                addr_hint: from.real_listen_addr,
                version: from.routing_version,
                observed_at_mono_ns: 1,
                expected_principal: from.expected_principal,
            });

            for to in nodes {
                if from.cell_identity == to.cell_identity {
                    continue;
                }
                link_idx = link_idx.saturating_add(1);
                let link_seed =
                    base_seed.wrapping_add(link_idx.wrapping_mul(0x517C_C1B7_2722_0A95));
                let proxy = ChaosTcpProxy::bind(
                    to.real_listen_addr,
                    ChaosLinkPolicy::passthrough(link_seed),
                )
                .await?;
                let proxy_addr = proxy.listen_addr();
                let _ = resolver.upsert_hint(EndpointHintRecord {
                    cell: to.cell_identity,
                    addr_hint: proxy_addr,
                    version: to.routing_version,
                    observed_at_mono_ns: 1,
                    expected_principal: to.expected_principal,
                });
                links.insert((from.cell_identity, to.cell_identity), proxy);
            }

            resolvers.insert(from.cell_identity, resolver);
        }

        Ok(Self {
            nodes: nodes.to_vec(),
            resolvers,
            links,
        })
    }

    /// Returns the dedicated [`MonotonicEndpointResolver`] for `cell` whose
    /// peer hints route through this chaos mesh.
    #[must_use]
    pub fn resolver_for(&self, cell: CellIdentity) -> Option<Arc<MonotonicEndpointResolver>> {
        self.resolvers.get(&cell).cloned()
    }

    /// Returns the [`ChaosLinkController`] for the directed link `(from, to)`.
    #[must_use]
    pub fn link_controller(
        &self,
        from: CellIdentity,
        to: CellIdentity,
    ) -> Option<ChaosLinkController> {
        self.links.get(&(from, to)).map(ChaosTcpProxy::controller)
    }

    /// Returns the local proxy `SocketAddr` for the directed link `(from, to)`.
    #[must_use]
    pub fn link_proxy_addr(&self, from: CellIdentity, to: CellIdentity) -> Option<SocketAddr> {
        self.links.get(&(from, to)).map(ChaosTcpProxy::listen_addr)
    }

    /// Updates a node's real upstream listener address (e.g., after a node
    /// restarts on a new ephemeral port) by rebinding its inbound proxy links
    /// and advancing the `EndpointRoutingVersion` in all peer resolvers.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if rebinding any inbound proxy link fails.
    pub async fn rebind_node_upstream(
        &mut self,
        node_cell: CellIdentity,
        new_real_addr: SocketAddr,
        expected_principal: Option<PrincipalId>,
        new_version: EndpointRoutingVersion,
    ) -> io::Result<()> {
        for desc in &mut self.nodes {
            if desc.cell_identity == node_cell {
                desc.real_listen_addr = new_real_addr;
                desc.expected_principal = expected_principal;
                desc.routing_version = new_version;
            }
        }
        if let Some(self_res) = self.resolvers.get(&node_cell) {
            let _ = self_res.upsert_hint(EndpointHintRecord {
                cell: node_cell,
                addr_hint: new_real_addr,
                version: new_version,
                observed_at_mono_ns: 2,
                expected_principal,
            });
        }
        let peer_cells: Vec<CellIdentity> = self
            .nodes
            .iter()
            .map(|n| n.cell_identity)
            .filter(|c| *c != node_cell)
            .collect();

        for from_cell in peer_cells {
            let old_policy = self
                .links
                .get(&(from_cell, node_cell))
                .map_or_else(ChaosLinkPolicy::default, |p| p.controller().policy());
            if let Some(old_proxy) = self.links.remove(&(from_cell, node_cell)) {
                old_proxy.shutdown().await;
            }
            let new_proxy = ChaosTcpProxy::bind(new_real_addr, old_policy).await?;
            let new_proxy_addr = new_proxy.listen_addr();
            self.links.insert((from_cell, node_cell), new_proxy);
            if let Some(peer_res) = self.resolvers.get(&from_cell) {
                let _ = peer_res.upsert_hint(EndpointHintRecord {
                    cell: node_cell,
                    addr_hint: new_proxy_addr,
                    version: new_version,
                    observed_at_mono_ns: 2,
                    expected_principal,
                });
            }
        }
        Ok(())
    }

    /// Completely isolates `isolated` from all other nodes in the mesh (cutting
    /// all inbound and outbound links and aborting active connections).
    pub fn isolate_node(&self, isolated: CellIdentity) {
        for (&(from, to), proxy) in &self.links {
            if from == isolated || to == isolated {
                let ctrl = proxy.controller();
                ctrl.update_policy(|p| {
                    p.c2s_blackhole = true;
                    p.s2c_blackhole = true;
                    p.reset_mode = ChaosResetMode::BeforeFirstPayloadByte;
                });
                ctrl.abort_active_connections();
            }
        }
    }

    /// Restores all inbound and outbound links for `cell` to healthy passthrough.
    pub fn heal_node(&self, cell: CellIdentity) {
        for (&(from, to), proxy) in &self.links {
            if from == cell || to == cell {
                proxy.controller().heal();
            }
        }
    }

    /// Restores every directed link in the mesh to healthy passthrough.
    pub fn heal_all(&self) {
        for proxy in self.links.values() {
            proxy.controller().heal();
        }
    }

    /// Injects a directional one-way cut where traffic from `from -> to` is
    /// blackholed while traffic from `to -> from` continues to flow.
    pub fn inject_one_way_cut(&self, from: CellIdentity, to: CellIdentity) {
        if let Some(forward_proxy) = self.links.get(&(from, to)) {
            let ctrl = forward_proxy.controller();
            ctrl.update_policy(|p| {
                p.c2s_blackhole = true;
            });
        }
        if let Some(reverse_proxy) = self.links.get(&(to, from)) {
            let ctrl = reverse_proxy.controller();
            ctrl.update_policy(|p| {
                p.c2s_blackhole = false;
                p.s2c_blackhole = false;
                p.reset_mode = ChaosResetMode::None;
                p.half_open_blackhole = false;
            });
        }
    }

    /// Injects a 3-node triangle asymmetric partition where `node_a <-> node_b`
    /// and `node_b <-> node_c` are healthy while `node_a <-> node_c` is
    /// completely blackholed in both directions (`ADR-0026` §5.3).
    pub fn inject_triangle_asymmetric_partition(
        &self,
        node_a: CellIdentity,
        node_b: CellIdentity,
        node_c: CellIdentity,
    ) {
        for &(x, y) in &[
            (node_a, node_b),
            (node_b, node_a),
            (node_b, node_c),
            (node_c, node_b),
        ] {
            if let Some(proxy) = self.links.get(&(x, y)) {
                proxy.controller().heal();
            }
        }
        for &(x, y) in &[(node_a, node_c), (node_c, node_a)] {
            if let Some(proxy) = self.links.get(&(x, y)) {
                let ctrl = proxy.controller();
                ctrl.update_policy(|p| {
                    p.c2s_blackhole = true;
                    p.s2c_blackhole = true;
                    p.reset_mode = ChaosResetMode::BeforeFirstPayloadByte;
                });
                ctrl.abort_active_connections();
            }
        }
    }

    /// Configures artificial base latency and deterministic jitter (`ms`) across
    /// all links in the mesh.
    pub fn set_all_links_latency(&self, base_delay_ms: u64, jitter_max_ms: u64) {
        for proxy in self.links.values() {
            proxy
                .controller()
                .set_latency_and_jitter(base_delay_ms, jitter_max_ms);
        }
    }

    /// Aggregates [`ChaosLinkMetricsSnapshot`] across all directed links in the
    /// mesh.
    #[must_use]
    pub fn aggregate_metrics(&self) -> ChaosLinkMetricsSnapshot {
        let mut total = ChaosLinkMetricsSnapshot::default();
        for proxy in self.links.values() {
            total.merge(&proxy.metrics());
        }
        total
    }

    /// Shuts down all [`ChaosTcpProxy`] listeners in the mesh.
    pub async fn shutdown(self) {
        for (_, proxy) in self.links {
            proxy.shutdown().await;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::type_complexity,
    clippy::field_reassign_with_default
)]
mod tests {
    use std::future::Future;
    use std::num::NonZeroU64;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;

    use ed25519_dalek::{Signer, SigningKey};
    use nlos_cell::{
        CellEpoch, CellFence, CellFencingToken, RemoteCallContract, RemoteCallOutcome,
        RemoteRetryDirective, RemoteRetryRefusal,
    };
    use nlos_types::{
        ControlDomainId, Generation, IdempotencyKey, KeyId, PrincipalId, SchedulerDomainId,
    };

    use super::*;
    use crate::cert::NodeTlsCredentials;
    use crate::failure::SocketFailureKind;
    use crate::frame::RpcWireStatus;
    use crate::handshake::{FenceNonceRegistry, MonotonicPeerFenceGate};
    use crate::identity::{
        IdentityPeerVerifier, PeerDigestSigner, PeerIdentityError, VerifiedPeerKey,
    };
    use crate::mux::{IncomingRpcRequest, RpcHandlerReply, RpcRequestHandler};
    use crate::profile::{ClusterProfile, SocketActivityAccounting};
    use crate::transport::{
        CellRpcTransport, MtlsCellTransport, MtlsEndpointListener, NetTransportConfig,
        NodeSecurityDeps, TransportDispatchRefusal,
    };

    #[derive(Clone)]
    struct TestSigner {
        principal_id: PrincipalId,
        key_id: KeyId,
        key_generation: Generation,
        signing_key: SigningKey,
    }

    impl PeerDigestSigner for TestSigner {
        fn principal_id(&self) -> PrincipalId {
            self.principal_id
        }

        fn key_id(&self) -> KeyId {
            self.key_id
        }

        fn key_generation(&self) -> Generation {
            self.key_generation
        }

        fn sign_domain_digest(
            &self,
            _domain_tag: &'static [u8],
            message_digest: [u8; 32],
            _signed_at_ms: u64,
        ) -> Result<[u8; 64], PeerIdentityError> {
            Ok(self.signing_key.sign(&message_digest).to_bytes())
        }
    }

    #[derive(Default)]
    struct TestVerifier {
        peers: Mutex<BTreeMap<PrincipalId, (KeyId, Generation, ControlDomainId, [u8; 32])>>,
    }

    impl TestVerifier {
        fn register(&self, signer: &TestSigner, domain: ControlDomainId) {
            self.peers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(
                    signer.principal_id,
                    (
                        signer.key_id,
                        signer.key_generation,
                        domain,
                        signer.signing_key.verifying_key().to_bytes(),
                    ),
                );
        }
    }

    impl IdentityPeerVerifier for TestVerifier {
        fn verify_peer_digest(
            &self,
            principal: PrincipalId,
            key_id: KeyId,
            key_generation: Generation,
            message_digest: [u8; 32],
            signature: [u8; 64],
            _verified_at_ms: u64,
        ) -> Result<VerifiedPeerKey, PeerIdentityError> {
            use ed25519_dalek::{Signature, Verifier, VerifyingKey};

            let guard = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(&(exp_kid, exp_gen, domain, pub_bytes)) = guard.get(&principal) else {
                return Err(PeerIdentityError::PrincipalUnknown(principal));
            };
            if exp_kid != key_id || exp_gen != key_generation {
                return Err(PeerIdentityError::KeyRotatedOrStale {
                    presented_key_id: key_id,
                    presented_generation: key_generation,
                    active_generation: Some(exp_gen),
                });
            }
            let vk = VerifyingKey::from_bytes(&pub_bytes)
                .map_err(|_| PeerIdentityError::SignatureInvalid)?;
            let sig = Signature::from_bytes(&signature);

            if vk.verify(&message_digest, &sig).is_ok() {
                Ok(VerifiedPeerKey {
                    principal_id: principal,
                    control_domain_id: domain,
                    key_id,
                    key_generation,
                })
            } else {
                Err(PeerIdentityError::SignatureInvalid)
            }
        }
    }

    struct CountingEchoRpcHandler {
        invocations: AtomicUsize,
    }

    impl RpcRequestHandler for CountingEchoRpcHandler {
        fn handle_rpc(
            &self,
            request: IncomingRpcRequest,
        ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            let mut out = b"ACK:".to_vec();
            out.extend_from_slice(&request.payload);
            let epoch = request.peer_session.local_claims.cell_epoch;
            Box::pin(async move {
                RpcHandlerReply {
                    observed_epoch: epoch,
                    idempotency_replayed: false,
                    status: RpcWireStatus::Ok,
                    payload: out,
                }
            })
        }
    }

    fn make_cell(seed_byte: u8) -> CellIdentity {
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([seed_byte; 16]))
    }

    fn make_node_security(
        seed_byte: u8,
        cell: CellIdentity,
        principal: PrincipalId,
        domain: ControlDomainId,
        verifier: &Arc<TestVerifier>,
    ) -> NodeSecurityDeps {
        let key_gen = Generation::new(NonZeroU64::new(1).expect("1"));
        let signer = TestSigner {
            principal_id: principal,
            key_id: KeyId::from_bytes([seed_byte; 16]),
            key_generation: key_gen,
            signing_key: SigningKey::from_bytes(&[seed_byte; 32]),
        };
        verifier.register(&signer, domain);
        let tls_creds =
            NodeTlsCredentials::issue(&signer, [seed_byte.wrapping_add(100); 32], 1_000, 9_000_000)
                .expect("issue cert");
        let fence = CellFence::present(
            cell,
            key_gen,
            CellEpoch::from_u64(1).expect("1"),
            CellFencingToken::from_u64(1).expect("1"),
        );
        NodeSecurityDeps {
            local_fence: Arc::new(move || fence),
            local_signer: Arc::new(signer),
            peer_verifier: Arc::clone(verifier) as Arc<dyn IdentityPeerVerifier>,
            fence_policy: Arc::new(MonotonicPeerFenceGate::new(None)),
            nonce_registry: Arc::new(FenceNonceRegistry::new(64)),
            clock_ms: Arc::new(|| 50_000),
            tls_credentials: tls_creds,
        }
    }

    fn sample_side_effecting_contract(seed: u8) -> RemoteCallContract {
        RemoteCallContract {
            call_id: [seed; 16],
            idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
            deadline_tick: 100,
            max_retries: 2,
            attempt_number: 0,
            side_effecting: true,
        }
    }

    #[test]
    fn seeded_prng_is_deterministic_across_runs() {
        let mut p1 = ChaosPrng::new(42);
        let mut p2 = ChaosPrng::new(42);
        let mut p3 = ChaosPrng::new(99);

        let seq1: Vec<u64> = (0..16).map(|_| p1.next_u64()).collect();
        let seq2: Vec<u64> = (0..16).map(|_| p2.next_u64()).collect();
        let seq3: Vec<u64> = (0..16).map(|_| p3.next_u64()).collect();

        assert_eq!(seq1, seq2);
        assert_ne!(seq1, seq3);
    }

    #[tokio::test]
    async fn chaos_proxy_nth_chunk_drop_and_latency_injection_are_deterministic() {
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let upstream_addr = upstream_listener.local_addr().expect("addr");

        let recv_task = tokio::spawn(async move {
            let (mut sock, _) = upstream_listener.accept().await.expect("accept");
            let mut received = Vec::new();
            let mut buf = [0u8; 4];
            while sock.read_exact(&mut buf).await.is_ok() {
                received.push(u32::from_be_bytes(buf));
            }
            received
        });

        let mut policy = ChaosLinkPolicy::passthrough(777);
        policy.drop_every_nth_chunk = NonZeroU32::new(3);
        policy.drop_behavior = ChaosDropBehavior::SilentDiscardChunk;
        policy.base_delay_ms = 2;
        policy.jitter_max_ms = 2;

        let proxy = ChaosTcpProxy::bind(upstream_addr, policy)
            .await
            .expect("bind proxy");

        let mut client = TcpStream::connect(proxy.listen_addr())
            .await
            .expect("connect");
        let _ = client.set_nodelay(true);

        for i in 1u32..=6u32 {
            client.write_all(&i.to_be_bytes()).await.expect("write");
            client.flush().await.expect("flush");
            sleep(Duration::from_millis(8)).await;
        }
        drop(client);

        let received = recv_task.await.expect("join");
        // Chunks 3 and 6 were dropped deterministically!
        assert_eq!(received, vec![1, 2, 4, 5]);

        let metrics = proxy.metrics();
        assert_eq!(metrics.chunks_dropped, 2);
        assert_eq!(metrics.c2s_chunks_forwarded, 4);
        assert!(metrics.injected_delay_total_ms >= 8);
        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn chaos_proxy_mtls_before_vs_after_payload_rst_and_ciphertext_corruption() {
        let verifier = Arc::new(TestVerifier::default());
        let cell_a = make_cell(0xA1);
        let cell_b = make_cell(0xB2);
        let princ_a = PrincipalId::from_bytes([0x11; 16]);
        let princ_b = PrincipalId::from_bytes([0x22; 16]);
        let dom_a = ControlDomainId::from_bytes([1u8; 16]);
        let dom_b = ControlDomainId::from_bytes([2u8; 16]);

        let sec_a = make_node_security(1, cell_a, princ_a, dom_a, &verifier);
        let sec_b = make_node_security(2, cell_b, princ_b, dom_b, &verifier);

        let handler_b = Arc::new(CountingEchoRpcHandler {
            invocations: AtomicUsize::new(0),
        });

        let mut net_cfg = NetTransportConfig::default();
        net_cfg.connect_and_tls_timeout = Duration::from_millis(300);
        net_cfg.fence_handshake_timeout = Duration::from_millis(300);
        net_cfg.rpc_call_timeout = Duration::from_millis(300);

        let listener_b = MtlsEndpointListener::bind(
            ClusterProfile::MultiHostCluster {
                local_cell: cell_b,
                bind_hint: ([127, 0, 0, 1], 0).into(),
            },
            sec_b,
            net_cfg,
            SocketActivityAccounting::new(),
            Some(Arc::clone(&handler_b) as Arc<dyn RpcRequestHandler>),
            None,
        )
        .await
        .expect("bind B");
        let real_addr_b = listener_b.local_addr();
        let running_b = listener_b.spawn_accept_loop();

        let proxy = ChaosTcpProxy::bind(real_addr_b, ChaosLinkPolicy::passthrough(2026))
            .await
            .expect("bind proxy");
        let ctrl = proxy.controller();

        let resolver_a = Arc::new(MonotonicEndpointResolver::new());
        let _ = resolver_a.upsert_hint(EndpointHintRecord {
            cell: cell_b,
            addr_hint: proxy.listen_addr(),
            version: EndpointRoutingVersion {
                node_boot_generation: Generation::new(NonZeroU64::new(1).expect("1")),
                epoch: CellEpoch::from_u64(1).expect("1"),
                heartbeat_seq: 1,
            },
            observed_at_mono_ns: 1,
            expected_principal: Some(princ_b),
        });

        let transport_a = MtlsCellTransport::new_multi_host(
            ClusterProfile::MultiHostCluster {
                local_cell: cell_a,
                bind_hint: ([127, 0, 0, 1], 0).into(),
            },
            resolver_a,
            sec_a,
            net_cfg,
            SocketActivityAccounting::new(),
            None,
            None,
        )
        .expect("transport A");

        // 1. Healthy passthrough call succeeds.
        let c1 = sample_side_effecting_contract(1);
        let res1 = transport_a
            .call_cell_rpc(cell_b, &c1, b"ping-1".to_vec())
            .await
            .expect("dispatch 1");
        assert!(matches!(res1, RemoteCallOutcome::Succeeded { .. }));
        assert_eq!(handler_b.invocations.load(Ordering::SeqCst), 1);

        // 2. Arm `BeforeFirstPayloadByte` reset -> maps to `Unavailable` (`SafeToRetryWithSameKey`).
        ctrl.arm_reset_before_first_payload_byte();
        transport_a.invalidate_cached_session(cell_b);
        let c2 = sample_side_effecting_contract(2);
        let res2 = transport_a
            .call_cell_rpc(cell_b, &c2, b"ping-2".to_vec())
            .await
            .expect("dispatch 2");
        assert_eq!(res2, c2.classify_pre_dispatch_unavailable());
        assert!(
            !transport_a
                .reconciliation_tracker()
                .requires_reconciliation(c2.idempotency_key),
            "pre-payload reset must not taint PendingReconciliationTracker"
        );

        // 3. Arm `AfterFirstPayloadByte` reset -> completes 3-turn handshake, forwards
        // application bytes, then severs before response -> `DeadlineExceededUncertain`
        // + blocks blind retry!
        ctrl.heal();
        ctrl.arm_reset_after_first_payload_byte(16);
        transport_a.invalidate_cached_session(cell_b);

        let c3 = sample_side_effecting_contract(3);
        let res3 = transport_a
            .call_cell_rpc(cell_b, &c3, b"side-effect-payload".to_vec())
            .await
            .expect("dispatch 3");
        assert!(
            matches!(
                res3,
                RemoteCallOutcome::DeadlineExceededUncertain {
                    side_effecting: true,
                    retry_directive: RemoteRetryDirective::ReconcileBeforeRetry,
                    ..
                }
            ),
            "expected DeadlineExceededUncertain, got {res3:?}"
        );
        assert!(proxy.metrics().connections_reset_after_payload >= 1);

        // Blind retry with same `IdempotencyKey` MUST be refused fail-closed!
        let blind_retry = transport_a
            .call_cell_rpc(cell_b, &c3, b"side-effect-payload".to_vec())
            .await;
        assert_eq!(
            blind_retry,
            Err(TransportDispatchRefusal::BlindRetryBlocked(
                RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation {
                    idempotency_key: c3.idempotency_key,
                }
            ))
        );
        transport_a
            .reconciliation_tracker()
            .mark_reconciled(c3.idempotency_key);

        // 4. Arm post-handshake ciphertext bit-flip -> TLS 1.3 AEAD rejects tampered
        // record fail-closed and handler_b is NOT invoked with corrupted data!
        ctrl.heal();
        let count_before_corrupt = handler_b.invocations.load(Ordering::SeqCst);
        ctrl.arm_ciphertext_corruption(CiphertextCorruptionRule {
            post_handshake_only: true,
            byte_offset: 6,
            xor_mask: 0xAA,
        });
        transport_a.invalidate_cached_session(cell_b);
        let c4 = sample_side_effecting_contract(4);
        let (res4, fail_kind4) = transport_a
            .call_cell_rpc_detailed(cell_b, &c4, b"tamper-probe".to_vec())
            .await
            .expect("dispatch 4");
        assert!(fail_kind4.is_some());
        assert!(matches!(
            res4,
            RemoteCallOutcome::DeadlineExceededUncertain { .. }
        ));
        assert_eq!(
            handler_b.invocations.load(Ordering::SeqCst),
            count_before_corrupt,
            "corrupted TLS 1.3 ciphertext record must never reach application RPC handler"
        );
        assert_eq!(proxy.metrics().ciphertext_corruptions_injected, 1);

        // 5. Silent TCP half-open blackhole -> triggers timeout classification.
        ctrl.heal();
        ctrl.enter_half_open_blackhole();
        transport_a.invalidate_cached_session(cell_b);
        let c5 = sample_side_effecting_contract(5);
        let (res5, fail_kind5) = transport_a
            .call_cell_rpc_detailed(cell_b, &c5, b"stall-probe".to_vec())
            .await
            .expect("dispatch 5");
        assert_eq!(fail_kind5, Some(SocketFailureKind::TlsHandshakeRejected));
        assert_eq!(res5, c5.classify_pre_dispatch_unavailable());
        assert!(proxy.metrics().half_open_stalls_triggered >= 1);

        // 6. Heal proxy -> subsequent retry with `c5` succeeds cleanly!
        ctrl.heal();
        transport_a.invalidate_cached_session(cell_b);
        let res5_retry = transport_a
            .call_cell_rpc(cell_b, &c5, b"stall-probe".to_vec())
            .await
            .expect("dispatch 5 retry");
        assert!(matches!(res5_retry, RemoteCallOutcome::Succeeded { .. }));

        proxy.shutdown().await;
        running_b.shutdown().await;
    }

    #[tokio::test]
    async fn chaos_directed_mesh_enforces_triangle_asymmetric_and_one_way_partitions() {
        let verifier = Arc::new(TestVerifier::default());
        let cells = [make_cell(1), make_cell(2), make_cell(3)];
        let principals = [
            PrincipalId::from_bytes([0x11; 16]),
            PrincipalId::from_bytes([0x22; 16]),
            PrincipalId::from_bytes([0x33; 16]),
        ];
        let domains = [
            ControlDomainId::from_bytes([1u8; 16]),
            ControlDomainId::from_bytes([2u8; 16]),
            ControlDomainId::from_bytes([3u8; 16]),
        ];

        let mut net_cfg = NetTransportConfig::default();
        net_cfg.connect_and_tls_timeout = Duration::from_millis(250);
        net_cfg.fence_handshake_timeout = Duration::from_millis(250);
        net_cfg.rpc_call_timeout = Duration::from_millis(250);

        let mut secs = Vec::new();
        let mut listeners = Vec::new();
        let mut descriptors = Vec::new();

        for i in 0..3 {
            let sec = make_node_security(
                u8::try_from(i + 1).expect("u8"),
                cells[i],
                principals[i],
                domains[i],
                &verifier,
            );
            let handler = Arc::new(CountingEchoRpcHandler {
                invocations: AtomicUsize::new(0),
            });
            let listener = MtlsEndpointListener::bind(
                ClusterProfile::MultiHostCluster {
                    local_cell: cells[i],
                    bind_hint: ([127, 0, 0, 1], 0).into(),
                },
                sec.clone(),
                net_cfg,
                SocketActivityAccounting::new(),
                Some(handler),
                None,
            )
            .await
            .expect("bind");
            let addr = listener.local_addr();
            listeners.push(listener.spawn_accept_loop());
            secs.push(sec);
            descriptors.push(ChaosNodeDescriptor {
                cell_identity: cells[i],
                real_listen_addr: addr,
                expected_principal: Some(principals[i]),
                routing_version: EndpointRoutingVersion {
                    node_boot_generation: Generation::new(NonZeroU64::new(1).expect("1")),
                    epoch: CellEpoch::from_u64(1).expect("1"),
                    heartbeat_seq: 1,
                },
            });
        }

        let mesh = ChaosDirectedMesh::bind(&descriptors, 9001)
            .await
            .expect("bind mesh");

        let mut transports = Vec::new();
        for i in 0..3 {
            let t = MtlsCellTransport::new_multi_host(
                ClusterProfile::MultiHostCluster {
                    local_cell: cells[i],
                    bind_hint: descriptors[i].real_listen_addr,
                },
                mesh.resolver_for(cells[i]).expect("resolver"),
                secs[i].clone(),
                net_cfg,
                SocketActivityAccounting::new(),
                None,
                None,
            )
            .expect("transport");
            transports.push(t);
        }

        // Inject triangle asymmetric partition: A <-> B up, B <-> C up, A <-> C blackholed.
        mesh.inject_triangle_asymmetric_partition(cells[0], cells[1], cells[2]);

        let c_ab = sample_side_effecting_contract(10);
        let c_bc = sample_side_effecting_contract(11);
        let c_ac = sample_side_effecting_contract(12);
        let c_ca = sample_side_effecting_contract(13);

        assert!(matches!(
            transports[0]
                .call_cell_rpc(cells[1], &c_ab, b"ab".to_vec())
                .await
                .expect("ab"),
            RemoteCallOutcome::Succeeded { .. }
        ));
        assert!(matches!(
            transports[1]
                .call_cell_rpc(cells[2], &c_bc, b"bc".to_vec())
                .await
                .expect("bc"),
            RemoteCallOutcome::Succeeded { .. }
        ));
        assert!(matches!(
            transports[0]
                .call_cell_rpc(cells[2], &c_ac, b"ac".to_vec())
                .await
                .expect("ac"),
            RemoteCallOutcome::Unavailable { .. }
        ));
        assert!(matches!(
            transports[2]
                .call_cell_rpc(cells[0], &c_ca, b"ca".to_vec())
                .await
                .expect("ca"),
            RemoteCallOutcome::Unavailable { .. }
        ));

        // Inject one-way cut: A -> B cut while B -> A remains healthy!
        mesh.heal_all();
        mesh.inject_one_way_cut(cells[0], cells[1]);
        transports[0].invalidate_cached_session(cells[1]);
        transports[1].invalidate_cached_session(cells[0]);

        let c_ba = sample_side_effecting_contract(14);
        assert!(matches!(
            transports[1]
                .call_cell_rpc(cells[0], &c_ba, b"ba-flows".to_vec())
                .await
                .expect("ba"),
            RemoteCallOutcome::Succeeded { .. }
        ));
        let c_ab_cut = sample_side_effecting_contract(15);
        assert!(matches!(
            transports[0]
                .call_cell_rpc(cells[1], &c_ab_cut, b"ab-blocked".to_vec())
                .await
                .expect("ab blocked"),
            RemoteCallOutcome::Unavailable { .. }
        ));

        mesh.shutdown().await;
        for l in listeners {
            l.shutdown().await;
        }
    }
}
