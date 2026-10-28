//! Cross-physical-host TLS 1.3 mTLS transport, 5-axis TLS Exporter `CellFence`
//! peer authentication, dual-class stream multiplexing, deterministic socket
//! failure classification, and pure 3-node `ConsensusCore` `PoC` seed
//! (`ADR-0026`, `C-NET-TRANSPORT`, `PoC-XHOST-001`).
//!
//! # Architectural Invariants Enforced
//!
//! - **`[NLOS-GOAL-001]` / `ADR-0026` §6.3 (`N = 1` Zero-Overhead Default)**:
//!   [`ClusterProfile::SingleHostDesktop`] is the default profile, binding **0**
//!   TCP/UDP sockets, initializing **0** TLS listeners/connectors, and failing
//!   closed on any accidental cross-host socket operation.
//! - **`[DIST-NAME-001]` (`CellIdentity` vs Ephemeral `SocketAddr`)**:
//!   [`CellRpcTransport`] and [`BulkStreamTransport`] address nominal 16-byte
//!   `CellIdentity` values exclusively; `SocketAddr` hints live solely in
//!   [`EndpointResolver`] guarded by monotonic
//!   `(node_boot_generation, epoch, heartbeat_seq)` watermarks.
//! - **`[TCB-ENDPOINT-001]` / `ADR-0026` §2.2 (5-Axis TLS 1.3 Exporter Fence)**:
//!   Strict TLS 1.3-only (`0-RTT` disabled) mutual X.509 authentication backed
//!   by `IdentityAuthority` and `SigningKeyCustodyVault` tombstones, followed
//!   immediately by bidirectional Ed25519 challenge-response over RFC 8446 /
//!   RFC 9266 `export_keying_material("EXPORTER-nlos-cell-fence-v1")` binding
//!   `(CellIdentity, node_boot_generation, PrincipalId, CellEpoch, CellFencingToken)`.
//! - **`[DIST-FAIL-001]` / `ADR-0026` §2.4 (Deterministic Failure Taxonomy)**:
//!   Exact [`WriteProgressPhase`] (`BeforeFirstPayloadByte` vs
//!   `AfterFirstPayloadByte`) classification into
//!   `nlos_cell::partition::RemoteCallOutcome` plus client-side
//!   [`PendingReconciliationTracker`] refusing blind retries on uncertain
//!   side-effecting calls until reconciled.
//! - **`ADR-0026` §3.1 (Dual-Class Stream Isolation)**:
//!   Strict priority writer scheduling `ControlRpc` + window-credit frames ahead
//!   of `<= 64 KiB` `BulkChunk` frames with pull-driven per-stream credit-window
//!   backpressure.

pub mod cert;
pub mod chaos_proxy;
pub mod failure;
pub mod frame;
pub mod handshake;
pub mod identity;
pub mod mux;
pub mod poc_consensus_core_3node;
pub mod profile;
pub mod resolver;
pub mod transport;

pub use cert::{
    CertVerifyError, MtlsNodeCertVerifier, NLOS_CELL_TLS_SNI, NodeIdentityCertClaims,
    NodeTlsCredentials, TLS_CERT_PRINCIPAL_BINDING_DOMAIN, VerificationTimeSource,
    VerifiedNodeCert, build_tls13_client_config, build_tls13_server_config,
    verify_node_x509_cert_der,
};
pub use chaos_proxy::{
    ChaosDirectedMesh, ChaosDropBehavior, ChaosLinkController, ChaosLinkMetricsSnapshot,
    ChaosLinkPolicy, ChaosNodeDescriptor, ChaosPrng, ChaosResetMode, ChaosTcpProxy,
    CiphertextCorruptionRule, HANDSHAKE_S2C_FLIGHT_TURNS,
};
pub use failure::{
    PendingReconciliationTracker, SocketFailureKind, WriteProgressPhase, aggregate_fanout_outcomes,
    classify_transport_failure,
};
pub use frame::{
    DEFAULT_BULK_WINDOW_BYTES, DEFAULT_MAX_CONTROL_PAYLOAD_BYTES, FrameCodecError,
    FramePriorityClass, MAX_BULK_CHUNK_BYTES, MuxFrame, RpcWireStatus, read_mux_frame,
    write_mux_frame,
};
pub use handshake::{
    AuthenticatedPeerSession, CELL_FENCE_HANDSHAKE_DOMAIN, ExtractedTls13Material,
    FenceHandshakeError, FenceNonceRegistry, HandshakeContext, HandshakeRejectCode, HandshakeRole,
    MonotonicPeerFenceGate, PeerFenceClaims, PeerFencePolicy, TLS_EXPORTER_CONTEXT,
    TLS_EXPORTER_LABEL, TLS_EXPORTER_LEN, compute_fence_exporter_digest,
    extract_tls13_fence_exporter, extract_tls13_material, run_client_fence_handshake,
    run_server_fence_handshake,
};
pub use identity::{
    AuthorityCustodyPeerVerifier, CustodyPeerSigner, IdentityPeerVerifier, PeerDigestSigner,
    PeerIdentityError, VerifiedPeerKey,
};
pub use mux::{
    BulkHandlerCompletion, BulkStreamHandler, BulkStreamReader, BulkTransferReceipt,
    DEFAULT_MAX_CONCURRENT_BULK_STREAMS, IncomingBulkMeta, IncomingRpcRequest, MultiplexedSession,
    MuxConfig, RpcHandlerReply, RpcRequestHandler, RpcResponsePayload,
};
pub use profile::{ClusterProfile, SingleHostProfileViolation, SocketActivityAccounting};
pub use resolver::{
    EndpointHintRecord, EndpointResolver, EndpointRoutingVersion, EndpointStaleError,
    EndpointUpdateDecision, MonotonicEndpointResolver,
};
pub use transport::{
    BulkStreamTransport, CellRpcTransport, DialSessionError, MAX_REJECTED_HANDSHAKE_AUDIT_ENTRIES,
    MAX_RETAINED_ACCEPTED_SESSIONS, MtlsCellTransport, MtlsEndpointListener, NetInitError,
    NetTransportConfig, NodeSecurityDeps, RunningMtlsListener, TransportDispatchRefusal,
};
