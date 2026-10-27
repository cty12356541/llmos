//! `PoC-XHOST-001` integration & promotion-gate suite (`ADR-0026` §7.1,
//! `C-NET-TRANSPORT`).
//!
//! Exercises real loopback multi-socket TLS 1.3 mTLS (`tokio-rustls` + `ring`),
//! durable `IdentityAuthority` (`WAL+FULL`) + `SigningKeyCustodyVault`
//! (`WAL+FULL`) key custody and revocation tombstones, 5-axis TLS Exporter
//! (`"EXPORTER-nlos-cell-fence-v1"`) mutual authentication, `EndpointResolver`
//! monotonic routing hints (`[DIST-NAME-001]`), deterministic pre/post-first-byte
//! socket failure classification (`[DIST-FAIL-001]`), `64 MiB` (`1024 * 64 KiB`)
//! bounded-window bulk backpressure + concurrent high-priority `ControlRpc`
//! isolation, `ClusterProfile::SingleHostDesktop` (`N = 1`) zero-socket
//! guarantee, and the pure 3-node `ConsensusCore` `PoC` state machine.

use std::env;
use std::fs;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodyTier, RevokeCompromisedKeyRequest,
    SigningKeyCustodyVault,
};
use nlos_cell::{
    CellEpoch, CellFence, CellFencingToken, CellIdentity, RemoteCallContract, RemoteCallOutcome,
    RemoteRetryDirective, RemoteRetryRefusal,
};
use nlos_identity::{IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_net::frame::{MuxFrame, read_mux_frame};
use nlos_net::handshake::{
    CELL_FENCE_HANDSHAKE_DOMAIN, TAG_CLIENT_ATTEST, TAG_CLIENT_HELLO, TAG_HANDSHAKE_REJECT,
    TAG_SERVER_CHALLENGE_ATTEST, TAG_SERVER_READY_ACK, read_handshake_frame, write_handshake_frame,
};
use nlos_net::poc_consensus_core_3node::{
    ConsensusCore, ConsensusEffect, ConsensusInput, ConsensusRole, ControlPlaneLogPayload,
    DeterministicConsensusHarness, ProposalRejectReason, VoterMember, VoterRoster,
    VoterRosterError,
};
use nlos_net::{
    AuthorityCustodyPeerVerifier, BulkHandlerCompletion, BulkStreamHandler, BulkStreamReader,
    BulkStreamTransport, CellRpcTransport, ClusterProfile, CustodyPeerSigner, DialSessionError,
    EndpointHintRecord, EndpointResolver, EndpointRoutingVersion, EndpointUpdateDecision,
    FenceHandshakeError, FenceNonceRegistry, HandshakeRejectCode, HandshakeRole,
    IdentityPeerVerifier, IncomingBulkMeta, IncomingRpcRequest, MAX_BULK_CHUNK_BYTES,
    MonotonicEndpointResolver, MonotonicPeerFenceGate, MtlsCellTransport, MtlsEndpointListener,
    MtlsNodeCertVerifier, MuxConfig, NLOS_CELL_TLS_SNI, NetInitError, NetTransportConfig,
    NodeSecurityDeps, NodeTlsCredentials, PeerDigestSigner, PeerFenceClaims, RpcHandlerReply,
    RpcRequestHandler, RpcWireStatus, SocketActivityAccounting, SocketFailureKind,
    TransportDispatchRefusal, VerificationTimeSource, aggregate_fanout_outcomes,
    build_tls13_client_config, compute_fence_exporter_digest, extract_tls13_material,
};
use nlos_types::{ControlDomainId, Generation, IdempotencyKey, SchedulerDomainId};
use rustls::pki_types::ServerName;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_rustls::TlsConnector;

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

const NOW_MS: u64 = 1_780_000_000_000;
const VALID_FROM_MS: u64 = NOW_MS - 60_000;
const VALID_UNTIL_MS: u64 = NOW_MS + 86_400_000;

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn new(label: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = env::temp_dir().join(format!("nlos-net-poc-{label}-{pid}-{seq}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn sub(&self, child: &str) -> PathBuf {
        let p = self.path.join(child);
        fs::create_dir_all(&p).expect("create sub dir");
        p
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

const fn cell_id(tag: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([tag; 16]))
}

const fn gen_u64(v: u64) -> Generation {
    Generation::new(NonZeroU64::new(v).expect("non-zero generation"))
}

const fn epoch_u64(v: u64) -> CellEpoch {
    CellEpoch::from_u64(v).expect("non-zero epoch")
}

const fn token_u64(v: u64) -> CellFencingToken {
    CellFencingToken::from_u64(v).expect("non-zero token")
}

struct ProvisionedNode {
    cell: CellIdentity,
    binding: IdentityBinding,
    vault: Arc<SigningKeyCustodyVault>,
    signer: Arc<CustodyPeerSigner>,
    tls_creds: NodeTlsCredentials,
    fence_state: Arc<Mutex<CellFence>>,
}

impl ProvisionedNode {
    fn bootstrap(
        identity: &Arc<IdentityAuthority>,
        vault_dir: &Path,
        node_tag: u8,
        boot_gen: u64,
        epoch: u64,
        token: u64,
    ) -> Self {
        let kek = [node_tag.wrapping_add(0x40); 32];
        let vault = Arc::new(
            SigningKeyCustodyVault::open_durable(vault_dir, kek).expect("open durable custody"),
        );
        let (_, binding) = vault
            .bootstrap_principal_under_custody(
                identity,
                BootstrapCustodyPrincipalRequest {
                    authority_id: [node_tag; 16],
                    principal_profile_digest: [node_tag.wrapping_add(1); 32],
                    control_domain_policy_digest: [node_tag.wrapping_add(2); 32],
                    key_purpose: KeyPurpose::SemanticSigning,
                    custody_tier: CustodyTier::OperatorFileSealed,
                    valid_from_ms: VALID_FROM_MS,
                    valid_until_ms: VALID_UNTIL_MS,
                    seed: [node_tag.wrapping_add(0x10); 32],
                    bootstrap_idempotency_key: IdempotencyKey::from_bytes(
                        [node_tag.wrapping_add(0x20); 16],
                    ),
                    custody_binding_idempotency_key: IdempotencyKey::from_bytes(
                        [node_tag.wrapping_add(0x30); 16],
                    ),
                    provisioned_at_ms: VALID_FROM_MS,
                },
            )
            .expect("bootstrap principal under custody");

        let signer = Arc::new(CustodyPeerSigner::new(
            Arc::clone(&vault),
            binding.principal_id,
            binding.key_id,
            binding.key_generation,
        ));
        let tls_creds = NodeTlsCredentials::issue(
            signer.as_ref(),
            [node_tag.wrapping_add(0x70); 32],
            VALID_FROM_MS,
            VALID_UNTIL_MS,
        )
        .expect("issue X.509v3 TLS credentials");

        let cell = cell_id(node_tag);
        let initial_fence =
            CellFence::present(cell, gen_u64(boot_gen), epoch_u64(epoch), token_u64(token));
        Self {
            cell,
            binding,
            vault,
            signer,
            tls_creds,
            fence_state: Arc::new(Mutex::new(initial_fence)),
        }
    }

    fn set_fence(&self, boot_gen: u64, epoch: u64, token: u64) {
        *self
            .fence_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = CellFence::present(
            self.cell,
            gen_u64(boot_gen),
            epoch_u64(epoch),
            token_u64(token),
        );
    }

    fn security_deps(
        &self,
        peer_verifier: Arc<AuthorityCustodyPeerVerifier>,
        fence_gate: Arc<MonotonicPeerFenceGate>,
    ) -> NodeSecurityDeps {
        let fence_slot = Arc::clone(&self.fence_state);
        NodeSecurityDeps {
            local_fence: Arc::new(move || {
                *fence_slot.lock().unwrap_or_else(PoisonError::into_inner)
            }),
            local_signer: self.signer.clone(),
            peer_verifier,
            fence_policy: fence_gate,
            nonce_registry: Arc::new(FenceNonceRegistry::default()),
            clock_ms: Arc::new(|| NOW_MS),
            tls_credentials: self.tls_creds.clone(),
        }
    }
}

/// Echo + idempotency-aware RPC handler for loopback integration scenarios.
struct EchoRpcHandler {
    epoch_source: Arc<Mutex<CellEpoch>>,
    call_count: AtomicUsize,
    seen_keys: Mutex<Vec<IdempotencyKey>>,
    artificial_delay: Duration,
}

impl EchoRpcHandler {
    fn new(initial_epoch: CellEpoch, artificial_delay: Duration) -> Self {
        Self {
            epoch_source: Arc::new(Mutex::new(initial_epoch)),
            call_count: AtomicUsize::new(0),
            seen_keys: Mutex::new(Vec::new()),
            artificial_delay,
        }
    }

    fn invocations(&self) -> usize {
        self.call_count.load(Ordering::Acquire)
    }
}

impl RpcRequestHandler for EchoRpcHandler {
    fn handle_rpc(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        self.call_count.fetch_add(1, Ordering::AcqRel);
        let delay = self.artificial_delay;
        let epoch = *self
            .epoch_source
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let replayed = {
            let mut keys = self
                .seen_keys
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if keys.contains(&request.idempotency_key) {
                true
            } else {
                keys.push(request.idempotency_key);
                false
            }
        };
        let mut out = b"ack:".to_vec();
        out.extend_from_slice(&request.payload);
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            RpcHandlerReply {
                observed_epoch: epoch,
                idempotency_replayed: replayed,
                status: RpcWireStatus::Ok,
                payload: out,
            }
        })
    }
}

// ============================================================================
// Scenario 1: Valid 5-Axis mTLS + TLS 1.3 Exporter Handshake & Custody Audit
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn poc_01_valid_5axis_mtls_and_tls13_exporter_handshake_pins_authenticated_peer_session() {
    let dir = TempTestDir::new("s1-valid-5axis");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let node_a = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-a"), 0x11, 2, 3, 30);
    let node_b = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-b"), 0x22, 4, 7, 70);
    assert_ne!(
        node_a.binding.control_domain_id, node_b.binding.control_domain_id,
        "distinct bootstrapped principals must occupy distinct failure domains"
    );

    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![Arc::clone(&node_a.vault), Arc::clone(&node_b.vault)],
    ));
    let accounting_a = SocketActivityAccounting::new();
    let accounting_b = SocketActivityAccounting::new();

    let rpc_handler_b = Arc::new(EchoRpcHandler::new(epoch_u64(7), Duration::ZERO));
    let bind_b: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
    let listener_b = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: node_b.cell,
            bind_hint: bind_b,
        },
        node_b.security_deps(
            Arc::clone(&verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        NetTransportConfig::default(),
        accounting_b.clone(),
        Some(rpc_handler_b.clone()),
        None,
    )
    .await
    .expect("bind listener B");
    let addr_b = listener_b.local_addr();
    assert_eq!(accounting_b.bound_listener_count(), 1);
    let running_b = listener_b.spawn_accept_loop();

    let resolver_a = Arc::new(MonotonicEndpointResolver::new());
    resolver_a
        .upsert_hint(
            EndpointHintRecord::new(
                node_b.cell,
                addr_b,
                EndpointRoutingVersion::new(gen_u64(4), epoch_u64(7), 1),
                1_000,
            )
            .with_expected_principal(node_b.binding.principal_id),
        )
        .expect("register cell_b hint");

    let transport_a = MtlsCellTransport::new_multi_host(
        ClusterProfile::MultiHostCluster {
            local_cell: node_a.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        resolver_a,
        node_a.security_deps(
            Arc::clone(&verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        NetTransportConfig::default(),
        accounting_a.clone(),
        None,
        None,
    )
    .expect("init client transport A");

    let client_session = transport_a
        .get_or_dial_session(node_b.cell)
        .await
        .expect("5-axis mTLS handshake must succeed");
    let pinned_client = *client_session.peer_session();

    // Verify all 5 axes + ControlDomainId + non-zero 32B TLS 1.3 Exporter secret!
    assert_eq!(pinned_client.peer_claims.cell_identity, node_b.cell);
    assert_eq!(pinned_client.peer_claims.node_boot_generation, gen_u64(4));
    assert_eq!(
        pinned_client.peer_claims.principal_id,
        node_b.binding.principal_id
    );
    assert_eq!(pinned_client.peer_claims.cell_epoch, epoch_u64(7));
    assert_eq!(pinned_client.peer_claims.cell_fencing_token, token_u64(70));
    assert_eq!(
        pinned_client.peer_control_domain,
        node_b.binding.control_domain_id
    );
    assert_ne!(pinned_client.tls_exporter_binding, [0u8; 32]);

    // Execute high-priority ControlRpc over the established session.
    let contract = RemoteCallContract::new(
        [0x01; 16],
        IdempotencyKey::from_bytes([0xa1; 16]),
        100,
        2,
        0,
        true,
    );
    let outcome = transport_a
        .call_cell_rpc(node_b.cell, &contract, b"hello-xhost".to_vec())
        .await
        .expect("dispatch rpc");
    match outcome {
        RemoteCallOutcome::Succeeded {
            value,
            observed_epoch,
            idempotency_replayed,
        } => {
            assert_eq!(value.payload, b"ack:hello-xhost");
            assert_eq!(observed_epoch, epoch_u64(7));
            assert!(!idempotency_replayed);
        }
        other => panic!("expected Succeeded, got {other:?}"),
    }

    // Confirm server pinned exact matching transcript & TLS Exporter secret.
    let server_sessions = running_b.accepted_sessions();
    assert_eq!(server_sessions.len(), 1);
    let pinned_server = *server_sessions[0].peer_session();
    assert_eq!(
        pinned_client.tls_exporter_binding,
        pinned_server.tls_exporter_binding
    );
    assert_eq!(
        pinned_client.session_transcript_digest,
        pinned_server.session_transcript_digest
    );
    assert_eq!(pinned_server.peer_claims.cell_identity, node_a.cell);
    assert_eq!(
        pinned_server.peer_claims.principal_id,
        node_a.binding.principal_id
    );

    // Durable custody audit receipts: 1 for X.509 cert issuance + 1 for 5-axis
    // Exporter handshake signature on each vault (raw seeds never exported).
    let receipts_a = node_a.vault.list_audit_receipts().expect("audit A");
    let receipts_b = node_b.vault.list_audit_receipts().expect("audit B");
    assert_eq!(receipts_a.len(), 2);
    assert_eq!(receipts_b.len(), 2);

    running_b.shutdown().await;
}

// ============================================================================
// Scenario 2: Negative Security Matrix (Revoked Key, Unknown Principal,
//             Exporter Replay on 2nd TLS Session, Stale Boot/Epoch, TLS 1.2)
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn poc_02_negative_security_matrix_fails_closed_before_any_rpc_payload() {
    let dir = TempTestDir::new("s2-negative-matrix");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let node_good = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-good"), 0x10, 3, 5, 50);
    let node_revoked =
        ProvisionedNode::bootstrap(&identity, &dir.sub("vault-revoked"), 0x20, 1, 1, 10);
    let node_server =
        ProvisionedNode::bootstrap(&identity, &dir.sub("vault-server"), 0x30, 1, 5, 50);
    let node_imposter =
        ProvisionedNode::bootstrap(&identity, &dir.sub("vault-imposter"), 0x35, 9, 9, 90);

    // Separate unshared IdentityAuthority for unknown Principal test (2b).
    let foreign_identity = Arc::new(
        IdentityAuthority::open(dir.sub("foreign-identity")).expect("open foreign identity"),
    );
    let node_unknown =
        ProvisionedNode::bootstrap(&foreign_identity, &dir.sub("vault-unknown"), 0x40, 1, 1, 10);

    // Revoke `node_revoked`'s key across BOTH CustodyVault (CompromisedRevoked
    // tombstone) and IdentityAuthority AFTER its X.509 cert was issued!
    node_revoked
        .vault
        .revoke_compromised_key_with_identity(
            &identity,
            &node_revoked.binding,
            RevokeCompromisedKeyRequest {
                key_id: node_revoked.binding.key_id,
                rotation_epoch: node_revoked.binding.key_generation.get(),
                revoked_from_ms: VALID_FROM_MS + 1_000,
                reason_digest: [0xee; 32],
                recorded_at_ms: NOW_MS - 1_000,
            },
            IdempotencyKey::from_bytes([0xef; 16]),
        )
        .expect("revoke compromised key");

    let server_verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![
            Arc::clone(&node_good.vault),
            Arc::clone(&node_revoked.vault),
            Arc::clone(&node_server.vault),
            Arc::clone(&node_imposter.vault),
        ],
    ));
    let server_fence_gate = Arc::new(MonotonicPeerFenceGate::default());
    server_fence_gate
        .pin_cell_principal(node_good.cell, node_good.binding.principal_id)
        .expect("pin node_good principal");
    let server_rpc = Arc::new(EchoRpcHandler::new(epoch_u64(5), Duration::ZERO));

    let listener = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: node_server.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        node_server.security_deps(Arc::clone(&server_verifier), Arc::clone(&server_fence_gate)),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        Some(server_rpc.clone()),
        None,
    )
    .await
    .expect("bind server");
    let server_addr = listener.local_addr();
    let running_server = listener.spawn_accept_loop();

    let resolver = Arc::new(MonotonicEndpointResolver::new());
    resolver
        .upsert_hint(
            EndpointHintRecord::new(
                node_server.cell,
                server_addr,
                EndpointRoutingVersion::new(gen_u64(1), epoch_u64(5), 1),
                100,
            )
            .with_expected_principal(node_server.binding.principal_id),
        )
        .expect("upsert server hint");

    // --- (2a) Revoked / Compromised Key Fails Closed at TLS 1.3 mTLS ---
    let transport_revoked = MtlsCellTransport::new_multi_host(
        ClusterProfile::MultiHostCluster {
            local_cell: node_revoked.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        resolver.clone(),
        node_revoked.security_deps(
            Arc::clone(&server_verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        None,
        None,
    )
    .expect("init revoked client");

    let contract_2a = RemoteCallContract::new(
        [0x2a; 16],
        IdempotencyKey::from_bytes([0x2a; 16]),
        100,
        2,
        0,
        true,
    );
    let (out_2a, kind_2a) = transport_revoked
        .call_cell_rpc_detailed(node_server.cell, &contract_2a, b"forbidden".to_vec())
        .await
        .expect("dispatch 2a");
    assert!(
        matches!(
            out_2a,
            RemoteCallOutcome::Unavailable {
                retry_directive: RemoteRetryDirective::SafeToRetryWithSameKey,
                ..
            }
        ),
        "revoked peer must fail BeforeFirstPayloadByte as Unavailable, got {out_2a:?}"
    );
    assert!(matches!(
        kind_2a,
        Some(
            SocketFailureKind::TlsHandshakeRejected
                | SocketFailureKind::PeerFenceHandshakeRejected
                | SocketFailureKind::ConnectionReset
        )
    ));
    assert_eq!(
        server_rpc.invocations(),
        0,
        "zero RPC frames may reach handler from revoked key"
    );

    // --- (2b) Unknown PrincipalId Fails Closed ---
    let unknown_client_verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![Arc::clone(&node_server.vault)],
    ));
    let transport_unknown = MtlsCellTransport::new_multi_host(
        ClusterProfile::MultiHostCluster {
            local_cell: node_unknown.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        resolver.clone(),
        node_unknown.security_deps(
            unknown_client_verifier,
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        None,
        None,
    )
    .expect("init unknown client");
    let err_2b = transport_unknown
        .get_or_dial_session(node_server.cell)
        .await
        .expect_err("unknown PrincipalId must be rejected");
    assert!(
        matches!(
            err_2b,
            DialSessionError::TlsHandshake(_) | DialSessionError::FenceHandshake(_)
        ),
        "expected TLS/Fence rejection for unknown principal, got {err_2b:?}"
    );
    assert_eq!(server_rpc.invocations(), 0);

    // --- (2c.0 / F1a) Unverified Step 1 `ClientHelloFence(u64::MAX)` MUST NOT Poison Watermarks ---
    let tls_verifier = Arc::new(MtlsNodeCertVerifier::new(
        server_verifier.clone() as Arc<dyn IdentityPeerVerifier>,
        VerificationTimeSource::CustomMs(Arc::new(|| NOW_MS)),
    ));
    let client_tls_cfg = build_tls13_client_config(&node_good.tls_creds, tls_verifier.clone())
        .expect("build client tls config");
    let connector = TlsConnector::from(client_tls_cfg);

    {
        let poisoned_claims = PeerFenceClaims::from_fence_and_signer(
            CellFence::present(
                node_good.cell,
                gen_u64(u64::MAX),
                epoch_u64(u64::MAX),
                token_u64(u64::MAX),
            ),
            node_good.signer.as_ref(),
        );
        let tcp_poison = TcpStream::connect(server_addr)
            .await
            .expect("connect poison");
        let sni = ServerName::try_from(NLOS_CELL_TLS_SNI).expect("valid sni");
        let mut tls_poison = connector
            .connect(sni, tcp_poison)
            .await
            .expect("tls poison");
        let mut poison_hello = Vec::with_capacity(1 + 32 + 80);
        poison_hello.push(TAG_CLIENT_HELLO);
        poison_hello.extend_from_slice(&[0x77u8; 32]);
        poisoned_claims.encode_into(&mut poison_hello);
        write_handshake_frame(&mut tls_poison, &poison_hello)
            .await
            .expect("send unverified u64::MAX hello");

        let srv_challenge = read_handshake_frame(&mut tls_poison)
            .await
            .expect("read server challenge");
        assert_eq!(srv_challenge[0], TAG_SERVER_CHALLENGE_ATTEST);

        // Send forged/invalid Step 3 signature: server MUST reject with ExporterSignatureInvalid
        // AND MUST NOT have committed `u64::MAX` into `server_fence_gate`!
        let mut bad_attest = Vec::with_capacity(1 + 64);
        bad_attest.push(TAG_CLIENT_ATTEST);
        bad_attest.extend_from_slice(&[0xde; 64]);
        write_handshake_frame(&mut tls_poison, &bad_attest)
            .await
            .expect("send forged attest");
        let poison_reply = read_handshake_frame(&mut tls_poison)
            .await
            .expect("read poison reject");
        assert_eq!(
            poison_reply,
            vec![
                TAG_HANDSHAKE_REJECT,
                HandshakeRejectCode::ExporterSignatureInvalid as u8
            ]
        );
    }

    // --- (2c.1 / F1b) Authenticated Sibling Principal Masquerading `CellIdentity` Fails Closed ---
    {
        // Inbound masquerade: `node_imposter` has a valid Authority-anchored X.509 cert & custody
        // key for `Principal_Imposter`, but claims `cell_identity = node_good.cell`!
        let imposter_tls_cfg =
            build_tls13_client_config(&node_imposter.tls_creds, tls_verifier.clone())
                .expect("build imposter tls config");
        let imposter_connector = TlsConnector::from(imposter_tls_cfg);
        let tcp_imp = TcpStream::connect(server_addr)
            .await
            .expect("connect imposter");
        let sni = ServerName::try_from(NLOS_CELL_TLS_SNI).expect("valid sni");
        let mut tls_imp = imposter_connector
            .connect(sni, tcp_imp)
            .await
            .expect("tls imposter");
        let imposter_claims = PeerFenceClaims::from_fence_and_signer(
            CellFence::present(node_good.cell, gen_u64(9), epoch_u64(9), token_u64(90)),
            node_imposter.signer.as_ref(),
        );
        let mut imp_hello = Vec::with_capacity(1 + 32 + 80);
        imp_hello.push(TAG_CLIENT_HELLO);
        imp_hello.extend_from_slice(&[0x88u8; 32]);
        imposter_claims.encode_into(&mut imp_hello);
        write_handshake_frame(&mut tls_imp, &imp_hello)
            .await
            .expect("send imposter hello");
        let imp_reply = read_handshake_frame(&mut tls_imp)
            .await
            .expect("read imposter reject");
        assert_eq!(
            imp_reply,
            vec![
                TAG_HANDSHAKE_REJECT,
                HandshakeRejectCode::CellPrincipalBindingMismatch as u8
            ],
            "sibling Principal masquerading pinned CellIdentity must fail closed with CellPrincipalBindingMismatch"
        );
    }

    // --- (2c.2) Replay of Signed Exporter Transcript Onto a 2nd TLS 1.3 Session ---
    // Session #1: Legitimate handshake on `node_good` (`boot_gen=3, epoch=5, token=50`),
    // proving `server_fence_gate` was NOT poisoned by `u64::MAX` in (2c.0) above, and
    // capturing the exact signed wire frames (`hello_1` and `attest_1`).
    let claims_good = PeerFenceClaims::from_fence_and_signer(
        CellFence::present(node_good.cell, gen_u64(3), epoch_u64(5), token_u64(50)),
        node_good.signer.as_ref(),
    );
    let client_nonce_1 = [0x99u8; 32];
    let (captured_hello_1, captured_attest_1, exporter_1) = {
        let tcp1 = TcpStream::connect(server_addr).await.expect("connect #1");
        let sni = ServerName::try_from(NLOS_CELL_TLS_SNI).expect("valid sni");
        let mut tls1 = connector.connect(sni, tcp1).await.expect("tls #1");
        let mat1 = extract_tls13_material(tls1.get_ref().1).expect("exporter #1");

        let mut hello = Vec::with_capacity(1 + 32 + 80);
        hello.push(TAG_CLIENT_HELLO);
        hello.extend_from_slice(&client_nonce_1);
        claims_good.encode_into(&mut hello);
        write_handshake_frame(&mut tls1, &hello)
            .await
            .expect("send hello #1");

        let srv_msg = read_handshake_frame(&mut tls1)
            .await
            .expect("read server challenge #1");
        assert_eq!(srv_msg[0], TAG_SERVER_CHALLENGE_ATTEST);
        let srv_nonce_1: [u8; 32] = srv_msg[1..33].try_into().expect("32B");

        let digest_1 = compute_fence_exporter_digest(
            HandshakeRole::Client,
            &mat1.exporter,
            &client_nonce_1,
            &srv_nonce_1,
            &claims_good,
            node_server.cell,
        );
        let sig_1 = node_good
            .signer
            .sign_domain_digest(CELL_FENCE_HANDSHAKE_DOMAIN, digest_1, NOW_MS)
            .expect("sign exporter #1");
        let mut attest = Vec::with_capacity(1 + 64);
        attest.push(TAG_CLIENT_ATTEST);
        attest.extend_from_slice(&sig_1);
        write_handshake_frame(&mut tls1, &attest)
            .await
            .expect("send attest #1");

        let ack = read_handshake_frame(&mut tls1).await.expect("read ack #1");
        assert_eq!(ack[0], TAG_SERVER_READY_ACK);
        (hello, attest, mat1.exporter)
    };

    // Session #2: Open a brand-new TLS 1.3 connection (`exporter_2 != exporter_1`)
    // and replay `captured_hello_1` + `captured_attest_1` verbatim!
    {
        let tcp2 = TcpStream::connect(server_addr).await.expect("connect #2");
        let sni = ServerName::try_from(NLOS_CELL_TLS_SNI).expect("valid sni");
        let mut tls2 = connector.connect(sni, tcp2).await.expect("tls #2");
        let mat2 = extract_tls13_material(tls2.get_ref().1).expect("exporter #2");
        assert_ne!(
            exporter_1, mat2.exporter,
            "RFC 8446 TLS 1.3 Exporter secret must differ per ephemeral ECDHE session"
        );

        write_handshake_frame(&mut tls2, &captured_hello_1)
            .await
            .expect("send replayed hello");
        let srv_msg2 = read_handshake_frame(&mut tls2)
            .await
            .expect("read server challenge #2");
        assert_eq!(srv_msg2[0], TAG_SERVER_CHALLENGE_ATTEST);

        write_handshake_frame(&mut tls2, &captured_attest_1)
            .await
            .expect("send replayed attest from session #1");
        let reply2 = read_handshake_frame(&mut tls2)
            .await
            .expect("read reject frame #2");
        assert_eq!(
            reply2,
            vec![
                TAG_HANDSHAKE_REJECT,
                HandshakeRejectCode::ExporterSignatureInvalid as u8
            ],
            "replayed Exporter attestation on 2nd TLS session must fail closed with ExporterSignatureInvalid"
        );
    }

    // --- (2d) Stale `node_boot_generation` & Stale `CellEpoch` Fail Closed ---
    // `server_fence_gate` already recorded `node_good` at `(boot_gen=3, epoch=5, token=50)`
    // during Session #1 above!
    node_good.set_fence(2, 5, 50); // Regressed boot_gen: 2 < 3!
    let transport_stale_boot = MtlsCellTransport::new_multi_host(
        ClusterProfile::MultiHostCluster {
            local_cell: node_good.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        resolver.clone(),
        node_good.security_deps(
            Arc::clone(&server_verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        None,
        None,
    )
    .expect("init stale boot client");

    let err_boot = transport_stale_boot
        .get_or_dial_session(node_server.cell)
        .await
        .expect_err("stale node_boot_generation must fail closed");
    assert!(
        matches!(
            err_boot,
            DialSessionError::FenceHandshake(FenceHandshakeError::RejectedByPeer(
                HandshakeRejectCode::StaleBootGeneration
            ))
        ),
        "expected StaleBootGeneration, got {err_boot:?}"
    );

    node_good.set_fence(3, 4, 40); // Regressed CellEpoch: 4 < 5!
    let err_epoch = transport_stale_boot
        .get_or_dial_session(node_server.cell)
        .await
        .expect_err("stale CellEpoch must fail closed");
    assert!(
        matches!(
            err_epoch,
            DialSessionError::FenceHandshake(FenceHandshakeError::RejectedByPeer(
                HandshakeRejectCode::StaleCellEpoch
            ))
        ),
        "expected StaleCellEpoch, got {err_epoch:?}"
    );

    // --- (2e) Non-TLS / TLS 1.2 Downgrade Bytes Rejected ---
    {
        let mut raw_tcp = TcpStream::connect(server_addr)
            .await
            .expect("raw tcp connect");
        // Minimal legacy plaintext / TLS 1.0-1.2 record header triggering rustls alert.
        raw_tcp
            .write_all(b"GET /plaintext HTTP/1.1\r\n\r\n")
            .await
            .expect("write non-tls bytes");
        let mut buf = [0u8; 64];
        let _ = raw_tcp.read(&mut buf).await;
    }

    // --- (2f / F1b Outbound) DHCP/IP Reassignment to Authenticated Sibling Node Fails Closed ---
    {
        // `node_imposter` binds a listener claiming `node_server.cell` on a reassigned port,
        // while client's `EndpointResolver` pins `expected_principal = node_server.binding.principal_id`!
        let mut spoofed_fence = *node_imposter
            .fence_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        spoofed_fence = CellFence::present(
            node_server.cell,
            spoofed_fence.node_boot_generation(),
            spoofed_fence.epoch(),
            spoofed_fence.fencing_token(),
        );
        let imposter_deps = NodeSecurityDeps {
            local_fence: Arc::new(move || spoofed_fence),
            local_signer: node_imposter.signer.clone(),
            peer_verifier: Arc::clone(&server_verifier) as Arc<dyn IdentityPeerVerifier>,
            fence_policy: Arc::new(MonotonicPeerFenceGate::default()),
            nonce_registry: Arc::new(FenceNonceRegistry::default()),
            clock_ms: Arc::new(|| NOW_MS),
            tls_credentials: node_imposter.tls_creds.clone(),
        };
        let imposter_listener = MtlsEndpointListener::bind(
            ClusterProfile::MultiHostCluster {
                local_cell: node_server.cell,
                bind_hint: "127.0.0.1:0".parse().expect("loopback"),
            },
            imposter_deps,
            NetTransportConfig::default(),
            SocketActivityAccounting::new(),
            Some(server_rpc.clone()),
            None,
        )
        .await
        .expect("bind imposter listener");
        let imposter_addr = imposter_listener.local_addr();
        let running_imposter = imposter_listener.spawn_accept_loop();

        let pinned_resolver = Arc::new(MonotonicEndpointResolver::new());
        pinned_resolver
            .upsert_hint(
                EndpointHintRecord::new(
                    node_server.cell,
                    imposter_addr,
                    EndpointRoutingVersion::new(gen_u64(9), epoch_u64(9), 1),
                    500,
                )
                .with_expected_principal(node_server.binding.principal_id),
            )
            .expect("pin expected server principal on resolver");
        node_good.set_fence(4, 6, 60);
        let client_vs_imposter = MtlsCellTransport::new_multi_host(
            ClusterProfile::MultiHostCluster {
                local_cell: node_good.cell,
                bind_hint: "127.0.0.1:0".parse().expect("loopback"),
            },
            pinned_resolver,
            node_good.security_deps(
                Arc::clone(&server_verifier),
                Arc::new(MonotonicPeerFenceGate::default()),
            ),
            NetTransportConfig::default(),
            SocketActivityAccounting::new(),
            None,
            None,
        )
        .expect("init client vs imposter");
        let err_outbound_masquerade = client_vs_imposter
            .get_or_dial_session(node_server.cell)
            .await
            .expect_err("outbound sibling principal masquerade must fail closed");
        assert!(
            matches!(
                err_outbound_masquerade,
                DialSessionError::FenceHandshake(FenceHandshakeError::RejectedByLocal(
                    HandshakeRejectCode::CellPrincipalBindingMismatch
                ))
            ),
            "expected CellPrincipalBindingMismatch, got {err_outbound_masquerade:?}"
        );
        running_imposter.shutdown().await;
    }

    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        running_server.total_rejected_handshakes() >= 6,
        "listener must audit all rejected TLS/fence handshakes"
    );
    assert_eq!(server_rpc.invocations(), 0);
    running_server.shutdown().await;
}

// ============================================================================
// Scenario 3: `EndpointResolver` Monotonic CAS & Live Socket Rotation
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn poc_03_endpoint_resolver_monotonic_cas_and_live_socket_rotation_by_cell_identity() {
    let dir = TempTestDir::new("s3-resolver-rotation");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let node_a = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-a"), 0x51, 1, 1, 10);
    let node_b = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-b"), 0x52, 1, 1, 10);
    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![Arc::clone(&node_a.vault), Arc::clone(&node_b.vault)],
    ));

    let rpc_b1 = Arc::new(EchoRpcHandler::new(epoch_u64(1), Duration::ZERO));
    let listener_b1 = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: node_b.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        node_b.security_deps(
            Arc::clone(&verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        Some(rpc_b1.clone()),
        None,
    )
    .await
    .expect("bind listener b1");
    let addr_1 = listener_b1.local_addr();
    let running_b1 = listener_b1.spawn_accept_loop();

    let resolver = Arc::new(MonotonicEndpointResolver::new());
    let v1 = EndpointRoutingVersion::new(gen_u64(1), epoch_u64(1), 5);
    assert_eq!(
        resolver
            .upsert_hint(
                EndpointHintRecord::new(node_b.cell, addr_1, v1, 1_000)
                    .with_expected_principal(node_b.binding.principal_id),
            )
            .expect("insert initial hint"),
        EndpointUpdateDecision::Inserted
    );

    // Exact idempotent replay at equal `(boot_gen, epoch, heartbeat_seq)` + same addr succeeds.
    assert_eq!(
        resolver
            .upsert_hint(EndpointHintRecord::new(node_b.cell, addr_1, v1, 1_001))
            .expect("idempotent replay"),
        EndpointUpdateDecision::IdempotentReplay
    );

    // Out-of-order stale heartbeat_seq (4 < 5), stale boot_gen, and conflicting addr at equal version all fail closed!
    let bogus_addr: SocketAddr = "127.0.0.1:65001".parse().expect("addr");
    assert!(
        resolver
            .upsert_hint(EndpointHintRecord::new(
                node_b.cell,
                bogus_addr,
                EndpointRoutingVersion::new(gen_u64(1), epoch_u64(1), 4),
                2_000,
            ))
            .is_err(),
        "stale heartbeat_seq must fail closed"
    );
    assert!(
        resolver
            .upsert_hint(EndpointHintRecord::new(node_b.cell, bogus_addr, v1, 2_000))
            .is_err(),
        "conflicting addr_hint at identical routing version must fail closed"
    );
    assert_eq!(
        resolver.resolve(node_b.cell).expect("present").addr_hint,
        addr_1
    );

    let client_fence_gate = Arc::new(MonotonicPeerFenceGate::default());
    let transport_a = MtlsCellTransport::new_multi_host(
        ClusterProfile::MultiHostCluster {
            local_cell: node_a.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        resolver.clone(),
        node_a.security_deps(Arc::clone(&verifier), client_fence_gate),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        None,
        None,
    )
    .expect("init transport A");

    // 1. Live RPC routes to `addr_1` using ONLY nominal `node_b.cell`.
    let c1 = RemoteCallContract::new(
        [0x31; 16],
        IdempotencyKey::from_bytes([0x31; 16]),
        50,
        1,
        0,
        false,
    );
    let out1 = transport_a
        .call_cell_rpc(node_b.cell, &c1, b"route-1".to_vec())
        .await
        .expect("rpc 1");
    assert!(matches!(
        out1,
        RemoteCallOutcome::Succeeded {
            observed_epoch,
            ..
        } if observed_epoch == epoch_u64(1)
    ));
    assert_eq!(rpc_b1.invocations(), 1);

    // 2. Simulate DHCP / port rebind + node reboot (`boot_gen: 1 -> 2`, `epoch: 1 -> 2`):
    //    `CellIdentity` (`node_b.cell`) stays 100% invariant (`[DIST-NAME-001]`)!
    running_b1.shutdown().await;
    node_b.set_fence(2, 2, 20);
    let rpc_b2 = Arc::new(EchoRpcHandler::new(epoch_u64(2), Duration::ZERO));
    let listener_b2 = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: node_b.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        node_b.security_deps(
            Arc::clone(&verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        Some(rpc_b2.clone()),
        None,
    )
    .await
    .expect("bind listener b2");
    let addr_2 = listener_b2.local_addr();
    assert_ne!(addr_1, addr_2);
    let running_b2 = listener_b2.spawn_accept_loop();

    let v2 = EndpointRoutingVersion::new(gen_u64(2), epoch_u64(2), 1);
    assert_eq!(
        resolver
            .upsert_hint(EndpointHintRecord::new(node_b.cell, addr_2, v2, 3_000))
            .expect("advance hint to addr_2"),
        EndpointUpdateDecision::Advanced {
            previous_addr: addr_1,
            previous_version: v1,
        }
    );

    let c2 = RemoteCallContract::new(
        [0x32; 16],
        IdempotencyKey::from_bytes([0x32; 16]),
        60,
        1,
        0,
        false,
    );
    let out2 = transport_a
        .call_cell_rpc(node_b.cell, &c2, b"route-2".to_vec())
        .await
        .expect("rpc 2 over rotated SocketAddr");
    match out2 {
        RemoteCallOutcome::Succeeded {
            value,
            observed_epoch,
            ..
        } => {
            assert_eq!(value.payload, b"ack:route-2");
            assert_eq!(observed_epoch, epoch_u64(2));
        }
        other => panic!("expected Succeeded on rotated addr_2, got {other:?}"),
    }
    assert_eq!(rpc_b2.invocations(), 1);
    running_b2.shutdown().await;
}

// ============================================================================
// Scenario 4: Deterministic `[DIST-FAIL-001]` Socket Failure Taxonomy &
//             Mandatory Reconciliation Fence
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::similar_names, clippy::too_many_lines)]
async fn poc_04_deterministic_socket_failure_taxonomy_and_reconciliation_fence() {
    let dir = TempTestDir::new("s4-failure-taxonomy");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let node_client = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-c"), 0x61, 1, 1, 10);
    let node_srv1 = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-s1"), 0x62, 1, 2, 20);
    let node_srv2 = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-s2"), 0x63, 1, 2, 20);
    let node_srv3 = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-s3"), 0x64, 1, 2, 20);

    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![
            Arc::clone(&node_client.vault),
            Arc::clone(&node_srv1.vault),
            Arc::clone(&node_srv2.vault),
            Arc::clone(&node_srv3.vault),
        ],
    ));

    let resolver = Arc::new(MonotonicEndpointResolver::new());
    let fast_cfg = NetTransportConfig {
        connect_and_tls_timeout: Duration::from_secs(2),
        fence_handshake_timeout: Duration::from_secs(2),
        rpc_call_timeout: Duration::from_millis(120),
        bulk_transfer_timeout: Duration::from_secs(5),
        timeout_elapsed_ticks: 15,
        mux: MuxConfig::default(),
    };
    let transport = MtlsCellTransport::new_multi_host(
        ClusterProfile::MultiHostCluster {
            local_cell: node_client.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        resolver.clone(),
        node_client.security_deps(
            Arc::clone(&verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        fast_cfg,
        SocketActivityAccounting::new(),
        None,
        None,
    )
    .expect("init client transport");

    // --- (4a) Pre-first-byte `ECONNREFUSED` -> `Unavailable` (`SafeToRetryWithSameKey`) ---
    let ephemeral_dead = TcpListener::bind("127.0.0.1:0").await.expect("bind temp");
    let closed_addr = ephemeral_dead.local_addr().expect("addr");
    drop(ephemeral_dead); // Guaranteed closed local port -> ECONNREFUSED!

    resolver
        .upsert_hint(EndpointHintRecord::new(
            node_srv2.cell,
            closed_addr,
            EndpointRoutingVersion::new(gen_u64(1), epoch_u64(2), 1),
            100,
        ))
        .expect("hint dead port");

    let key_4a = IdempotencyKey::from_bytes([0x4a; 16]);
    let contract_4a = RemoteCallContract::new([0x4a; 16], key_4a, 100, 2, 0, true);
    let (out_4a, kind_4a) = transport
        .call_cell_rpc_detailed(node_srv2.cell, &contract_4a, b"pre-byte-cmd".to_vec())
        .await
        .expect("dispatch 4a");
    assert_eq!(
        out_4a,
        RemoteCallOutcome::Unavailable {
            deadline_tick: 100,
            retry_directive: RemoteRetryDirective::SafeToRetryWithSameKey,
            idempotency_key: key_4a,
        }
    );
    assert!(matches!(
        kind_4a,
        Some(SocketFailureKind::ConnectRefusedOrUnreachable | SocketFailureKind::ConnectTimeout)
    ));
    assert!(
        !transport
            .reconciliation_tracker()
            .requires_reconciliation(key_4a),
        "BeforeFirstPayloadByte failure must NEVER block retry with reconciliation"
    );
    assert!(
        contract_4a.next_retry_for_outcome(&out_4a, 120).is_ok(),
        "Unavailable pre-dispatch call is safe to retry with same IdempotencyKey"
    );

    // --- (4b) Post-first-byte TCP `RST` / Abrupt TLS Truncation on `side_effecting = true` ---
    // Build a custom TLS 1.3 peer listener for `node_srv3` that completes mTLS +
    // 5-axis Exporter handshake, reads the inbound `ControlRpcRequest` frame
    // (confirming `AfterFirstPayloadByte`!), and immediately sends TCP `RST`
    // (`set_linger(Some(Duration::ZERO))`) without `close_notify` or response!
    let rst_tcp_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind rst listener");
    let rst_addr = rst_tcp_listener.local_addr().expect("rst addr");
    let srv3_verifier = Arc::new(MtlsNodeCertVerifier::new(
        verifier.clone() as Arc<dyn IdentityPeerVerifier>,
        VerificationTimeSource::CustomMs(Arc::new(|| NOW_MS)),
    ));
    let srv3_tls_cfg =
        nlos_net::build_tls13_server_config(&node_srv3.tls_creds, srv3_verifier).expect("tls cfg");
    let srv3_acceptor = tokio_rustls::TlsAcceptor::from(srv3_tls_cfg);
    let srv3_deps = node_srv3.security_deps(
        Arc::clone(&verifier),
        Arc::new(MonotonicPeerFenceGate::default()),
    );

    let rst_task = tokio::spawn(async move {
        let (tcp, _) = rst_tcp_listener.accept().await.expect("accept rst");
        #[allow(deprecated)]
        let _ = tcp.set_linger(Some(Duration::ZERO));
        let mut tls = srv3_acceptor.accept(tcp).await.expect("tls accept");
        let mat = extract_tls13_material(tls.get_ref().1).expect("mat");
        let claims = PeerFenceClaims::from_fence_and_signer(
            (srv3_deps.local_fence)(),
            srv3_deps.local_signer.as_ref(),
        );
        let ctx = nlos_net::HandshakeContext {
            local_claims: claims,
            local_signer: srv3_deps.local_signer.as_ref(),
            peer_verifier: srv3_deps.peer_verifier.as_ref(),
            fence_policy: srv3_deps.fence_policy.as_ref(),
            nonce_registry: srv3_deps.nonce_registry.as_ref(),
            now_ms: NOW_MS,
            handshake_timeout: Duration::from_secs(2),
        };
        nlos_net::run_server_fence_handshake(&mut tls, &mat, &ctx)
            .await
            .expect("server fence");
        // Read the full business `ControlRpcRequest` frame, then drop with SO_LINGER=0 -> TCP RST!
        let frame = read_mux_frame(&mut tls, 256 * 1024, Duration::from_secs(2))
            .await
            .expect("read business frame");
        assert!(matches!(frame, MuxFrame::ControlRpcRequest { .. }));
        drop(tls);
    });

    resolver
        .upsert_hint(EndpointHintRecord::new(
            node_srv3.cell,
            rst_addr,
            EndpointRoutingVersion::new(gen_u64(1), epoch_u64(2), 1),
            200,
        ))
        .expect("hint rst peer");

    let key_4b = IdempotencyKey::from_bytes([0x4b; 16]);
    let contract_4b = RemoteCallContract::new([0x4b; 16], key_4b, 100, 2, 0, true);
    let (out_4b, kind_4b) = transport
        .call_cell_rpc_detailed(node_srv3.cell, &contract_4b, b"mutating-transfer".to_vec())
        .await
        .expect("dispatch 4b");
    rst_task.await.expect("rst server task");

    assert_eq!(
        out_4b,
        RemoteCallOutcome::DeadlineExceededUncertain {
            deadline_tick: 100,
            elapsed_ticks: 15,
            side_effecting: true,
            retry_directive: RemoteRetryDirective::ReconcileBeforeRetry,
            idempotency_key: key_4b,
        },
        "post-write TCP RST on side-effecting call MUST map to DeadlineExceededUncertain + ReconcileBeforeRetry"
    );
    assert!(matches!(
        kind_4b,
        Some(
            SocketFailureKind::ConnectionReset
                | SocketFailureKind::UnexpectedEofOrTlsTruncation
                | SocketFailureKind::ReadTimeout
        )
    ));

    // Both `RemoteCallContract::next_retry_for_outcome` AND `MtlsCellTransport`
    // MUST refuse blind retry until `mark_reconciled`!
    assert_eq!(
        contract_4b.next_retry_for_outcome(&out_4b, 130),
        Err(
            RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation {
                idempotency_key: key_4b,
            }
        )
    );
    assert_eq!(
        transport
            .call_cell_rpc(node_srv3.cell, &contract_4b, b"mutating-transfer".to_vec())
            .await,
        Err(TransportDispatchRefusal::BlindRetryBlocked(
            RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation {
                idempotency_key: key_4b,
            }
        ))
    );

    // --- (4c) Half-Open Silent Read Timeout: `side_effecting = false` vs `true` ---
    let stalled_rpc = Arc::new(EchoRpcHandler::new(
        epoch_u64(2),
        Duration::from_millis(350), // > 120ms `rpc_call_timeout`
    ));
    let listener_s1 = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: node_srv1.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        node_srv1.security_deps(
            Arc::clone(&verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        fast_cfg,
        SocketActivityAccounting::new(),
        Some(stalled_rpc),
        None,
    )
    .await
    .expect("bind stalled s1");
    let addr_s1_stalled = listener_s1.local_addr();
    let running_s1_stalled = listener_s1.spawn_accept_loop();

    resolver
        .upsert_hint(EndpointHintRecord::new(
            node_srv1.cell,
            addr_s1_stalled,
            EndpointRoutingVersion::new(gen_u64(1), epoch_u64(2), 1),
            300,
        ))
        .expect("hint s1 stalled");

    let key_readonly = IdempotencyKey::from_bytes([0x4c; 16]);
    let contract_readonly = RemoteCallContract::new([0x4c; 16], key_readonly, 100, 2, 0, false);
    let (out_ro, kind_ro) = transport
        .call_cell_rpc_detailed(node_srv1.cell, &contract_readonly, b"query".to_vec())
        .await
        .expect("dispatch readonly timeout");
    assert_eq!(kind_ro, Some(SocketFailureKind::ReadTimeout));
    assert_eq!(
        out_ro,
        RemoteCallOutcome::DeadlineExceededUncertain {
            deadline_tick: 100,
            elapsed_ticks: 15,
            side_effecting: false,
            retry_directive: RemoteRetryDirective::SafeToRetryWithSameKey,
            idempotency_key: key_readonly,
        }
    );
    assert!(
        !transport
            .reconciliation_tracker()
            .requires_reconciliation(key_readonly)
    );
    running_s1_stalled.shutdown().await;

    // Now spin up healthy `node_srv3` at version 2, mark `key_4b` reconciled,
    // and verify that reconciled retry succeeds!
    let healthy_rpc3 = Arc::new(EchoRpcHandler::new(epoch_u64(2), Duration::ZERO));
    let listener_s3_healthy = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: node_srv3.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        node_srv3.security_deps(
            Arc::clone(&verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        fast_cfg,
        SocketActivityAccounting::new(),
        Some(healthy_rpc3),
        None,
    )
    .await
    .expect("bind healthy s3");
    let addr_s3_healthy = listener_s3_healthy.local_addr();
    let running_s3_healthy = listener_s3_healthy.spawn_accept_loop();
    resolver
        .upsert_hint(EndpointHintRecord::new(
            node_srv3.cell,
            addr_s3_healthy,
            EndpointRoutingVersion::new(gen_u64(1), epoch_u64(2), 2),
            400,
        ))
        .expect("advance s3 hint");

    assert!(transport.reconciliation_tracker().mark_reconciled(key_4b));
    let out_4b_reconciled = transport
        .call_cell_rpc(node_srv3.cell, &contract_4b, b"mutating-transfer".to_vec())
        .await
        .expect("reconciled retry");
    assert!(matches!(
        out_4b_reconciled,
        RemoteCallOutcome::Succeeded { .. }
    ));

    // --- (4d) 3-Shard Multi-Cell Fanout Partial Failure & Canonical `SHA-256` Digest ---
    let fanout_contract = RemoteCallContract::new(
        [0x4d; 16],
        IdempotencyKey::from_bytes([0x4d; 16]),
        200,
        2,
        0,
        true,
    );
    let partial_a = aggregate_fanout_outcomes(
        &fanout_contract,
        vec![
            (node_srv3.cell, out_4b_reconciled.clone()),
            (node_srv2.cell, out_4a.clone()),
            (node_srv1.cell, out_4b.clone()),
        ],
        true,
    );
    let partial_permuted = aggregate_fanout_outcomes(
        &fanout_contract,
        vec![
            (node_srv1.cell, out_4b),
            (node_srv3.cell, out_4b_reconciled),
            (node_srv2.cell, out_4a),
        ],
        true,
    );
    assert_eq!(
        partial_a, partial_permuted,
        "Partial uncertainty_digest must be canonically order-independent across shard arrival order"
    );
    match partial_a {
        RemoteCallOutcome::Partial {
            completed_shards,
            failed_shards,
            partial_value,
            retry_directive,
            uncertainty_digest,
            ..
        } => {
            assert_eq!(completed_shards, 1);
            assert_eq!(failed_shards, 2);
            assert_eq!(partial_value.as_ref().map(Vec::len), Some(1));
            assert_eq!(retry_directive, RemoteRetryDirective::ReconcileBeforeRetry);
            assert_ne!(uncertainty_digest, [0u8; 32]);
        }
        other => panic!("expected Partial outcome, got {other:?}"),
    }

    running_s3_healthy.shutdown().await;
}

// ============================================================================
// Scenario 5: `64 MiB` (`1024 * 64 KiB`) Bulk Stream With Slow-Consumer Credit
//             Window Backpressure + Concurrent High-Priority `ControlRpc`
// ============================================================================

struct SlowVerifyingBulkHandler {
    window_paused_once: Arc<Notify>,
    resume_drain: Arc<Notify>,
    received_total: Arc<AtomicU64>,
}

impl BulkStreamHandler for SlowVerifyingBulkHandler {
    fn handle_bulk_stream(
        &self,
        _meta: IncomingBulkMeta,
        mut reader: BulkStreamReader,
    ) -> Pin<Box<dyn Future<Output = Result<BulkHandlerCompletion, &'static str>> + Send + 'static>>
    {
        let paused = Arc::clone(&self.window_paused_once);
        let resume = Arc::clone(&self.resume_drain);
        let counter = Arc::clone(&self.received_total);

        Box::pin(async move {
            // Hold draining initially so the sender fills the `256 KiB` credit
            // window (`4 * 64 KiB` chunks) and parks under backpressure while
            // concurrent `ControlRpc` pings run on the SAME TLS 1.3 connection!
            paused.notify_waiters();
            resume.notified().await;

            let mut chunk_idx = 0usize;
            while let Some(chunk) = reader.next_chunk().await? {
                assert!(
                    chunk.len() <= MAX_BULK_CHUNK_BYTES,
                    "received chunk {} exceeds 64 KiB",
                    chunk.len()
                );
                counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                chunk_idx += 1;
                // Periodically yield briefly on early chunks so concurrent pings
                // interleave directly with active bulk chunk transmission.
                if chunk_idx <= 32 && chunk_idx.is_multiple_of(8) {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
            Ok(BulkHandlerCompletion {
                observed_epoch: epoch_u64(4),
                idempotency_replayed: false,
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn poc_05_64mib_chunked_bulk_stream_backpressure_and_priority_rpc_isolation() {
    const TOTAL_CHUNKS: u32 = 1024;
    const CHUNK_BYTES: usize = MAX_BULK_CHUNK_BYTES; // 64 KiB
    const TOTAL_BULK_BYTES: u64 = (TOTAL_CHUNKS as u64) * (CHUNK_BYTES as u64); // 67_108_864 B (64 MiB)
    const CREDIT_WINDOW_BYTES: usize = 4 * MAX_BULK_CHUNK_BYTES; // 256 KiB

    let dir = TempTestDir::new("s5-64mib-bulk");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let node_a = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-a"), 0x71, 1, 4, 40);
    let node_b = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-b"), 0x72, 1, 4, 40);
    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![Arc::clone(&node_a.vault), Arc::clone(&node_b.vault)],
    ));

    let window_paused = Arc::new(Notify::new());
    let resume_drain = Arc::new(Notify::new());
    let server_received_bytes = Arc::new(AtomicU64::new(0));
    let bulk_handler = Arc::new(SlowVerifyingBulkHandler {
        window_paused_once: Arc::clone(&window_paused),
        resume_drain: Arc::clone(&resume_drain),
        received_total: Arc::clone(&server_received_bytes),
    });
    let rpc_handler = Arc::new(EchoRpcHandler::new(epoch_u64(4), Duration::ZERO));

    let net_cfg = NetTransportConfig {
        connect_and_tls_timeout: Duration::from_secs(5),
        fence_handshake_timeout: Duration::from_secs(5),
        rpc_call_timeout: Duration::from_secs(3),
        bulk_transfer_timeout: Duration::from_secs(60),
        timeout_elapsed_ticks: 20,
        mux: MuxConfig {
            max_control_payload_bytes: 256 * 1024,
            bulk_window_bytes: CREDIT_WINDOW_BYTES,
            max_concurrent_bulk_streams: nlos_net::DEFAULT_MAX_CONCURRENT_BULK_STREAMS,
            frame_write_timeout: Duration::from_secs(10),
            frame_read_timeout: Duration::from_secs(30),
            default_rpc_timeout: Duration::from_secs(3),
        },
    };

    let listener_b = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: node_b.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        node_b.security_deps(
            Arc::clone(&verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        net_cfg,
        SocketActivityAccounting::new(),
        Some(rpc_handler.clone()),
        Some(bulk_handler),
    )
    .await
    .expect("bind listener B");
    let addr_b = listener_b.local_addr();
    let running_b = listener_b.spawn_accept_loop();

    let resolver = Arc::new(MonotonicEndpointResolver::new());
    resolver
        .upsert_hint(
            EndpointHintRecord::new(
                node_b.cell,
                addr_b,
                EndpointRoutingVersion::new(gen_u64(1), epoch_u64(4), 1),
                100,
            )
            .with_expected_principal(node_b.binding.principal_id),
        )
        .expect("hint B");

    let transport_a = Arc::new(
        MtlsCellTransport::new_multi_host(
            ClusterProfile::MultiHostCluster {
                local_cell: node_a.cell,
                bind_hint: "127.0.0.1:0".parse().expect("loopback"),
            },
            resolver,
            node_a.security_deps(
                Arc::clone(&verifier),
                Arc::new(MonotonicPeerFenceGate::default()),
            ),
            net_cfg,
            SocketActivityAccounting::new(),
            None,
            None,
        )
        .expect("init transport A"),
    );

    // 1. Fail-closed guard: chunk > `64 KiB` (`65_537 B`) is immediately rejected!
    let oversize_contract = RemoteCallContract::new(
        [0x70; 16],
        IdempotencyKey::from_bytes([0x70; 16]),
        100,
        1,
        0,
        false,
    );
    let oversize_err = transport_a
        .stream_bulk_chunks(
            node_b.cell,
            &oversize_contract,
            vec![vec![0xabu8; MAX_BULK_CHUNK_BYTES + 1]],
        )
        .await
        .expect_err("chunk > 64 KiB must fail closed");
    assert_eq!(
        oversize_err,
        TransportDispatchRefusal::BulkChunkExceedsMax {
            chunk_len: MAX_BULK_CHUNK_BYTES + 1,
            max_len: MAX_BULK_CHUNK_BYTES,
        }
    );

    // 2. Pre-compute deterministic expected SHA-256 of the `64 MiB` stream
    //    without holding `64 MiB` in RAM (`chunk[seq]` = deterministic pattern).
    let make_chunk = |seq: u32| -> Vec<u8> {
        let mut buf = vec![(seq & 0xff) as u8; CHUNK_BYTES];
        buf[0..4].copy_from_slice(&seq.to_be_bytes());
        buf[CHUNK_BYTES - 4..].copy_from_slice(&(!seq).to_be_bytes());
        buf
    };
    let expected_sha256: [u8; 32] = {
        let mut h = Sha256::new();
        for seq in 0..TOTAL_CHUNKS {
            h.update(make_chunk(seq));
        }
        h.finalize().into()
    };

    // Warm the single pooled TLS 1.3 connection so BOTH the `64 MiB` bulk stream
    // AND all 20 `ControlRpc` pings share the exact same TCP/TLS connection!
    let _ = transport_a
        .get_or_dial_session(node_b.cell)
        .await
        .expect("warm pooled mTLS session");
    assert_eq!(transport_a.socket_accounting().active_connection_count(), 1);

    let bulk_opened_wait = window_paused.notified();
    let transport_for_bulk = Arc::clone(&transport_a);
    let target_cell = node_b.cell;
    let bulk_task = tokio::spawn(async move {
        let bulk_contract = RemoteCallContract::new(
            [0x75; 16],
            IdempotencyKey::from_bytes([0x75; 16]),
            500,
            1,
            0,
            true,
        );
        transport_for_bulk
            .stream_bulk_from_producer(target_cell, &bulk_contract, TOTAL_BULK_BYTES, move |seq| {
                if seq < TOTAL_CHUNKS {
                    Some(make_chunk(seq))
                } else {
                    None
                }
            })
            .await
    });

    // Wait for `BulkStreamOpen` to arrive at server and allow sender to saturate
    // the `256 KiB` (`4 * 64 KiB`) credit window while consumer is paused!
    bulk_opened_wait.await;
    tokio::time::sleep(Duration::from_millis(35)).await;

    // Fire 10 high-priority `ControlRpc` pings WHILE the `64 MiB` bulk consumer
    // is completely paused at a full `256 KiB` window (proving zero head-of-line
    // blocking even when bulk flow control is 100% stalled!).
    for idx in 0u8..10 {
        let ping_contract = RemoteCallContract::new(
            [0x80 + idx; 16],
            IdempotencyKey::from_bytes([0x80 + idx; 16]),
            200,
            1,
            0,
            false,
        );
        let payload = format!("ping-stalled-{idx}").into_bytes();
        let ping_out = transport_a
            .call_cell_rpc(node_b.cell, &ping_contract, payload.clone())
            .await
            .expect("priority ping while bulk window full");
        match ping_out {
            RemoteCallOutcome::Succeeded { value, .. } => {
                let mut expected = b"ack:".to_vec();
                expected.extend_from_slice(&payload);
                assert_eq!(value.payload, expected);
            }
            other => panic!("ping {idx} failed during stalled bulk window: {other:?}"),
        }
    }

    // Confirm receiver buffer never exceeded `CREDIT_WINDOW_BYTES` (`256 KiB`) while stalled!
    let buffered_while_stalled = running_b.max_inbound_bulk_buffered_bytes();
    assert!(
        buffered_while_stalled > 0 && buffered_while_stalled <= CREDIT_WINDOW_BYTES,
        "stalled consumer buffered {buffered_while_stalled} B (expected 1..={CREDIT_WINDOW_BYTES})"
    );

    // Unpause slow consumer and fire 10 MORE concurrent `ControlRpc` pings while
    // all `1024` chunks (`64 MiB`) actively stream across the TLS 1.3 socket!
    resume_drain.notify_waiters();
    for idx in 10u8..20 {
        let ping_contract = RemoteCallContract::new(
            [0x80 + idx; 16],
            IdempotencyKey::from_bytes([0x80 + idx; 16]),
            200,
            1,
            0,
            false,
        );
        let payload = format!("ping-streaming-{idx}").into_bytes();
        let ping_out = transport_a
            .call_cell_rpc(node_b.cell, &ping_contract, payload.clone())
            .await
            .expect("priority ping during active 64 MiB stream");
        assert!(matches!(ping_out, RemoteCallOutcome::Succeeded { .. }));
    }

    let (bulk_outcome, bulk_fail_kind) = bulk_task
        .await
        .expect("join bulk task")
        .expect("bulk stream result");
    assert_eq!(bulk_fail_kind, None);
    match bulk_outcome {
        RemoteCallOutcome::Succeeded {
            value,
            observed_epoch,
            idempotency_replayed,
        } => {
            assert_eq!(value.verified_bytes, TOTAL_BULK_BYTES);
            assert_eq!(value.total_chunks, TOTAL_CHUNKS);
            assert_eq!(value.verified_sha256, expected_sha256);
            assert_eq!(observed_epoch, epoch_u64(4));
            assert!(!idempotency_replayed);
        }
        other => panic!("expected 64 MiB bulk Succeeded, got {other:?}"),
    }

    assert_eq!(
        server_received_bytes.load(Ordering::Acquire),
        TOTAL_BULK_BYTES
    );
    assert_eq!(rpc_handler.invocations(), 20);
    assert_eq!(
        transport_a
            .socket_accounting()
            .peak_active_connection_count(),
        1,
        "all 20 ControlRpc pings and 64 MiB bulk transfer multiplexed onto a single TLS 1.3 connection"
    );

    let peak_buffered = running_b.max_inbound_bulk_buffered_bytes();
    assert!(
        peak_buffered <= CREDIT_WINDOW_BYTES,
        "peak receiver bulk buffer ({peak_buffered} B) exceeded 256 KiB credit window ({CREDIT_WINDOW_BYTES} B)"
    );

    running_b.shutdown().await;
}

// ============================================================================
// Scenario 6: `ClusterProfile::SingleHostDesktop` (`N = 1`) Zero-Socket Default
// ============================================================================

#[tokio::test]
async fn poc_06_single_host_desktop_n1_default_binds_zero_sockets_and_fails_closed() {
    assert_eq!(ClusterProfile::default(), ClusterProfile::SingleHostDesktop);
    assert!(ClusterProfile::default().is_single_host_desktop());
    assert!(!ClusterProfile::default().allows_network_sockets());

    let accounting = SocketActivityAccounting::new();
    let desktop_transport = MtlsCellTransport::new_single_host_desktop(accounting.clone());
    assert_eq!(
        desktop_transport.profile(),
        ClusterProfile::SingleHostDesktop
    );
    assert_eq!(accounting.bound_listener_count(), 0);
    assert_eq!(accounting.peak_bound_listener_count(), 0);
    assert_eq!(accounting.active_connection_count(), 0);
    assert_eq!(accounting.peak_active_connection_count(), 0);

    let target = cell_id(0x99);
    let contract = RemoteCallContract::new(
        [0x66; 16],
        IdempotencyKey::from_bytes([0x66; 16]),
        50,
        1,
        0,
        false,
    );

    // Outbound RPC and bulk streaming fail closed with zero sockets touched.
    assert!(matches!(
        desktop_transport
            .call_cell_rpc(target, &contract, b"noop".to_vec())
            .await,
        Err(TransportDispatchRefusal::SingleHostProfile(_))
    ));
    assert!(matches!(
        desktop_transport
            .stream_bulk_chunks(target, &contract, vec![vec![1u8; 16]])
            .await,
        Err(TransportDispatchRefusal::SingleHostProfile(_))
    ));

    // Attempting to bind a listener under `SingleHostDesktop` also fails closed
    // before touching `TcpListener::bind`.
    let dir = TempTestDir::new("s6-single-host");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));
    let node = ProvisionedNode::bootstrap(&identity, &dir.sub("vault"), 0x91, 1, 1, 10);
    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![Arc::clone(&node.vault)],
    ));
    let bind_res = MtlsEndpointListener::bind(
        ClusterProfile::SingleHostDesktop,
        node.security_deps(verifier, Arc::new(MonotonicPeerFenceGate::default())),
        NetTransportConfig::default(),
        accounting.clone(),
        None,
        None,
    )
    .await;
    assert!(matches!(bind_res, Err(NetInitError::SingleHostProfile(_))));
    assert_eq!(accounting.peak_bound_listener_count(), 0);
    assert_eq!(accounting.peak_active_connection_count(), 0);
}

// ============================================================================
// Scenario 7: Pure 3-Node `ConsensusCore` Seed (`ADR-0026` §7.1 Promotion Gate)
// ============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn poc_07_consensus_core_3node_election_quorum_commit_fencing_and_heal_convergence() {
    let cell_1 = cell_id(0x01);
    let cell_2 = cell_id(0x02);
    let cell_3 = cell_id(0x03);
    let dom_1 = ControlDomainId::from_bytes([0xd1; 16]);
    let dom_2 = ControlDomainId::from_bytes([0xd2; 16]);
    let dom_3 = ControlDomainId::from_bytes([0xd3; 16]);

    // --- (7a) `[TASK-SCALE-002]` Rejects Voters Sharing a ControlDomainId ---
    let dup_err = VoterRoster::new(vec![
        VoterMember {
            cell_identity: cell_1,
            control_domain_id: dom_1,
        },
        VoterMember {
            cell_identity: cell_2,
            control_domain_id: dom_1, // Duplicate failure domain!
        },
        VoterMember {
            cell_identity: cell_3,
            control_domain_id: dom_3,
        },
    ])
    .expect_err("duplicate ControlDomainId across N>1 voters must fail closed");
    assert_eq!(
        dup_err,
        VoterRosterError::DuplicateControlDomain {
            control_domain_id: dom_1,
            first_cell: cell_1,
            second_cell: cell_2,
        }
    );

    // --- (7b) `N = 1` Degenerate Single-Host Direct-Commit Mode ---
    let single_roster = VoterRoster::new(vec![VoterMember {
        cell_identity: cell_1,
        control_domain_id: dom_1,
    }])
    .expect("single voter roster");
    let mut single_core =
        ConsensusCore::new(cell_1, single_roster, 10, 3).expect("init N=1 ConsensusCore");
    assert_eq!(single_core.role(), ConsensusRole::Leader);
    assert_eq!(single_core.current_term(), 1);
    assert!(
        single_core.step(ConsensusInput::LogicalTick).is_empty(),
        "N=1 degenerate mode must emit zero timer/heartbeat traffic"
    );
    let n1_effects = single_core.step(ConsensusInput::Propose(
        ControlPlaneLogPayload::RootLeaseEpochAdvance {
            new_epoch: epoch_u64(2),
            fencing_token: token_u64(20),
            holder: cell_1,
        },
    ));
    assert_eq!(single_core.consensus_commit_index(), 1);
    assert!(
        !n1_effects
            .iter()
            .any(|e| matches!(e, ConsensusEffect::SendMessage { .. })),
        "N=1 direct commit must emit zero network messages"
    );

    // --- (7c) `N = 3` Election, `2/3` Quorum Commit, Old-Leader Fencing & Heal ---
    let mut cluster = DeterministicConsensusHarness::new_cluster(
        vec![
            VoterMember {
                cell_identity: cell_1,
                control_domain_id: dom_1,
            },
            VoterMember {
                cell_identity: cell_2,
                control_domain_id: dom_2,
            },
            VoterMember {
                cell_identity: cell_3,
                control_domain_id: dom_3,
            },
        ],
        6, // staggered timeouts: cell_1=6, cell_2=9, cell_3=12
        2,
    )
    .expect("init 3-node cluster");

    // 1. Tick deterministic election: `cell_1` times out first at tick 6 and wins `3/3` votes in term 1!
    for _ in 0..8 {
        cluster.tick_all_and_drain(128);
    }
    assert_eq!(cluster.current_leaders(), vec![(cell_1, 1)]);

    // Non-leader (`cell_2`) rejects client proposals with `NotLeader { known_leader: Some(cell_1) }`.
    let reject_eff = cluster.step_node(
        cell_2,
        ConsensusInput::Propose(ControlPlaneLogPayload::CellDirectoryUpdate {
            cell: cell_2,
            node_boot_generation: gen_u64(1),
            epoch: epoch_u64(1),
        }),
    );
    assert_eq!(
        reject_eff,
        vec![ConsensusEffect::ProposalRejected(
            ProposalRejectReason::NotLeader {
                known_leader: Some(cell_1),
                current_term: 1,
            }
        )]
    );

    // 2. Isolate follower `cell_3` (`1` node down); `cell_1 + cell_2` (`2/3` majority)
    //    MUST still advance `consensus_commit_index`!
    cluster.isolate_node(cell_3);
    cluster.step_node(
        cell_1,
        ConsensusInput::Propose(ControlPlaneLogPayload::CellDirectoryUpdate {
            cell: cell_1,
            node_boot_generation: gen_u64(1),
            epoch: epoch_u64(2),
        }),
    );
    cluster.drain_messages(128);
    cluster.step_node(
        cell_1,
        ConsensusInput::Propose(ControlPlaneLogPayload::RootLeaseEpochAdvance {
            new_epoch: epoch_u64(2),
            fencing_token: token_u64(20),
            holder: cell_1,
        }),
    );
    cluster.drain_messages(128);
    // Heartbeat round propagates `leader_commit = 2` to `cell_2`.
    cluster.tick_all_and_drain(128);
    cluster.tick_all_and_drain(128);

    assert_eq!(
        cluster.node(cell_1).expect("c1").consensus_commit_index(),
        2
    );
    assert_eq!(
        cluster.node(cell_2).expect("c2").consensus_commit_index(),
        2
    );
    assert_eq!(
        cluster.node(cell_3).expect("c3").consensus_commit_index(),
        0,
        "isolated follower cell_3 has not received entries yet"
    );

    // 3. Asymmetric Split-Brain Partition Flip:
    //    Heal `{cell_2, cell_3}` and isolate old leader `cell_1` into a `1/3` minority!
    cluster.heal_all_links();
    cluster.isolate_node(cell_1);

    // Old leader `cell_1` (`term = 1`, minority `1/3`) receives a speculative
    // proposal AND a linearizable read request: NEITHER may commit or confirm!
    cluster.step_node(
        cell_1,
        ConsensusInput::Propose(ControlPlaneLogPayload::TaskAuthorityAssignment {
            task_key: [0xba; 16],
            authoritative_cell: cell_1,
            lease_epoch: epoch_u64(2),
        }),
    );
    let minority_read_eff = cluster.step_node(
        cell_1,
        ConsensusInput::RequestLinearizableRead { read_id: 404 },
    );
    let drained_minority = cluster.drain_messages(128);
    assert_eq!(
        cluster.node(cell_1).expect("c1").last_log_index(),
        3,
        "minority leader appended uncommitted entry 3 locally"
    );
    assert_eq!(
        cluster.node(cell_1).expect("c1").consensus_commit_index(),
        2,
        "minority old-term leader MUST NOT advance consensus_commit_index"
    );
    assert!(
        !minority_read_eff
            .iter()
            .chain(drained_minority.iter().map(|(_, e)| e))
            .any(|e| matches!(e, ConsensusEffect::LinearizableReadConfirmed { .. })),
        "isolated old-term leader MUST NEVER confirm a stale linearizable read"
    );

    // Advance `{cell_2, cell_3}` majority partition: `cell_2` (whose log has
    // `(term=1, idx=2)`, more up-to-date than `cell_3`'s empty log!) wins
    // election in higher term (`>= 2`), replicates entries `1..=2` to `cell_3`,
    // and commits two new majority entries (`3..=4`)!
    for _ in 0..15 {
        cluster.tick_all_and_drain(128);
    }
    let (new_leader, new_term) = cluster
        .highest_term_leader()
        .expect("majority partition elected leader");
    assert_eq!(
        new_leader, cell_2,
        "only cell_2 has up-to-date committed prefix (idx=2) to win majority vote"
    );
    assert!(new_term >= 2);

    cluster.step_node(
        cell_2,
        ConsensusInput::Propose(ControlPlaneLogPayload::TaskAuthorityAssignment {
            task_key: [0xca; 16],
            authoritative_cell: cell_2,
            lease_epoch: epoch_u64(3),
        }),
    );
    cluster.drain_messages(128);
    cluster.step_node(
        cell_2,
        ConsensusInput::Propose(ControlPlaneLogPayload::ReconciliationAnchor {
            epoch: epoch_u64(3),
            uncertainty_digest: [0xdd; 32],
        }),
    );
    cluster.drain_messages(128);

    // Confirm linearizable read on majority leader `cell_2` succeeds!
    let mut read_confirmed = false;
    let init_effs = cluster.step_node(
        cell_2,
        ConsensusInput::RequestLinearizableRead { read_id: 777 },
    );
    for eff in init_effs {
        if matches!(
            eff,
            ConsensusEffect::LinearizableReadConfirmed { read_id: 777, .. }
        ) {
            read_confirmed = true;
        }
    }
    for (_, eff) in cluster.drain_messages(128) {
        if matches!(
            eff,
            ConsensusEffect::LinearizableReadConfirmed {
                read_id: 777,
                commit_index: 4,
                ..
            }
        ) {
            read_confirmed = true;
        }
    }
    assert!(
        read_confirmed,
        "majority leader must confirm linearizable read at commit_index=4"
    );

    // Propagate `leader_commit = 4` to `cell_3`.
    cluster.tick_all_and_drain(128);
    cluster.tick_all_and_drain(128);
    assert_eq!(
        cluster.node(cell_2).expect("c2").consensus_commit_index(),
        4
    );
    assert_eq!(
        cluster.node(cell_3).expect("c3").consensus_commit_index(),
        4
    );

    // 4. Heal Partition: stale leader `cell_1` steps down to `Follower`,
    //    truncates its uncommitted minority entry at index 3, and converges
    //    identically with `cell_2` and `cell_3` at `commit_index = 4`!
    cluster.heal_all_links();
    let mut saw_truncation_on_c1 = false;
    for _ in 0..8 {
        for (node_id, eff) in cluster.tick_all_and_drain(256) {
            if node_id == cell_1
                && matches!(
                    eff,
                    ConsensusEffect::TruncateLogSuffixFrom { from_index: 3 }
                )
            {
                saw_truncation_on_c1 = true;
            }
        }
    }
    assert!(
        saw_truncation_on_c1,
        "healed minority ex-leader cell_1 must truncate conflicting uncommitted entry at index 3"
    );

    let c1 = cluster.node(cell_1).expect("c1");
    let c2 = cluster.node(cell_2).expect("c2");
    let c3 = cluster.node(cell_3).expect("c3");
    assert_eq!(c1.role(), ConsensusRole::Follower);
    assert_eq!(c2.role(), ConsensusRole::Leader);
    assert_eq!(c3.role(), ConsensusRole::Follower);
    assert_eq!(c1.current_term(), c2.current_term());
    assert_eq!(c3.current_term(), c2.current_term());
    assert_eq!(c1.consensus_commit_index(), 4);
    assert_eq!(c2.consensus_commit_index(), 4);
    assert_eq!(c3.consensus_commit_index(), 4);
    assert_eq!(c1.log_entries(), c2.log_entries());
    assert_eq!(c3.log_entries(), c2.log_entries());
}

// ============================================================================
// Scenario 8: Post-First-Byte Bulk Producer Fault Reconciliation (`[F2]`) &
//             Bounded Connection / Stream Resource Accounting (`[F3]`)
// ============================================================================

struct HoldableBulkHandler {
    hold_drain: Arc<AtomicUsize>,
    unblock: Arc<Notify>,
    completed_streams: Arc<AtomicUsize>,
}

impl BulkStreamHandler for HoldableBulkHandler {
    fn handle_bulk_stream(
        &self,
        _meta: IncomingBulkMeta,
        mut reader: BulkStreamReader,
    ) -> Pin<Box<dyn Future<Output = Result<BulkHandlerCompletion, &'static str>> + Send + 'static>>
    {
        let hold_mode = Arc::clone(&self.hold_drain);
        let unblock = Arc::clone(&self.unblock);
        let completed = Arc::clone(&self.completed_streams);
        Box::pin(async move {
            if hold_mode.load(Ordering::Acquire) != 0 {
                unblock.notified().await;
            }
            while reader.next_chunk().await?.is_some() {}
            completed.fetch_add(1, Ordering::AcqRel);
            Ok(BulkHandlerCompletion {
                observed_epoch: epoch_u64(3),
                idempotency_replayed: false,
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn poc_08_post_first_byte_bulk_producer_reconciliation_and_bounded_resource_accounting() {
    let dir = TempTestDir::new("s8-f2-f3-hardening");
    let identity =
        Arc::new(IdentityAuthority::open(dir.sub("identity")).expect("open IdentityAuthority"));

    let node_a = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-a"), 0xa1, 1, 3, 30);
    let node_b = ProvisionedNode::bootstrap(&identity, &dir.sub("vault-b"), 0xb2, 1, 3, 30);
    let verifier = Arc::new(AuthorityCustodyPeerVerifier::new(
        Arc::clone(&identity),
        vec![Arc::clone(&node_a.vault), Arc::clone(&node_b.vault)],
    ));

    let hold_drain = Arc::new(AtomicUsize::new(0));
    let unblock_drain = Arc::new(Notify::new());
    let completed_streams = Arc::new(AtomicUsize::new(0));
    let bulk_handler = Arc::new(HoldableBulkHandler {
        hold_drain: Arc::clone(&hold_drain),
        unblock: Arc::clone(&unblock_drain),
        completed_streams: Arc::clone(&completed_streams),
    });

    let cfg = NetTransportConfig {
        connect_and_tls_timeout: Duration::from_secs(3),
        fence_handshake_timeout: Duration::from_secs(3),
        rpc_call_timeout: Duration::from_secs(2),
        bulk_transfer_timeout: Duration::from_secs(3),
        timeout_elapsed_ticks: 12,
        mux: MuxConfig {
            max_control_payload_bytes: 256 * 1024,
            bulk_window_bytes: 4 * MAX_BULK_CHUNK_BYTES,
            max_concurrent_bulk_streams: 2,
            frame_write_timeout: Duration::from_secs(3),
            frame_read_timeout: Duration::from_secs(10),
            default_rpc_timeout: Duration::from_secs(2),
        },
    };

    let accounting_a = SocketActivityAccounting::new();
    let accounting_b = SocketActivityAccounting::new();
    let listener_b = MtlsEndpointListener::bind(
        ClusterProfile::MultiHostCluster {
            local_cell: node_b.cell,
            bind_hint: "127.0.0.1:0".parse().expect("loopback"),
        },
        node_b.security_deps(
            Arc::clone(&verifier),
            Arc::new(MonotonicPeerFenceGate::default()),
        ),
        cfg,
        accounting_b.clone(),
        Some(Arc::new(EchoRpcHandler::new(epoch_u64(3), Duration::ZERO))),
        Some(bulk_handler),
    )
    .await
    .expect("bind listener B");
    let addr_b = listener_b.local_addr();
    let running_b = listener_b.spawn_accept_loop();

    let resolver = Arc::new(MonotonicEndpointResolver::new());
    resolver
        .upsert_hint(
            EndpointHintRecord::new(
                node_b.cell,
                addr_b,
                EndpointRoutingVersion::new(gen_u64(1), epoch_u64(3), 1),
                100,
            )
            .with_expected_principal(node_b.binding.principal_id),
        )
        .expect("upsert B hint");

    let transport_a = Arc::new(
        MtlsCellTransport::new_multi_host(
            ClusterProfile::MultiHostCluster {
                local_cell: node_a.cell,
                bind_hint: "127.0.0.1:0".parse().expect("loopback"),
            },
            resolver,
            node_a.security_deps(
                Arc::clone(&verifier),
                Arc::new(MonotonicPeerFenceGate::default()),
            ),
            cfg,
            accounting_a.clone(),
            None,
            None,
        )
        .expect("init transport A"),
    );

    // --- (8a / F2 Pre-Dispatch) Empty chunk 0 & `total_bytes > 0` with `None` fail BeforeFirstPayloadByte ---
    let pre_key = IdempotencyKey::from_bytes([0x80; 16]);
    let pre_contract = RemoteCallContract::new([0x80; 16], pre_key, 100, 2, 0, true);
    assert_eq!(
        transport_a
            .stream_bulk_from_producer(node_b.cell, &pre_contract, 64, |_| Some(Vec::new()))
            .await,
        Err(TransportDispatchRefusal::BulkChunkEmpty)
    );
    assert_eq!(
        transport_a
            .stream_bulk_from_producer(node_b.cell, &pre_contract, 64, |_| None)
            .await,
        Err(TransportDispatchRefusal::BulkTotalBytesMismatch {
            declared: 64,
            actual: 0,
        })
    );
    assert!(
        !transport_a
            .reconciliation_tracker()
            .requires_reconciliation(pre_key),
        "pre-dispatch validation failures must not record uncertain side-effect state"
    );
    assert_eq!(accounting_a.peak_active_connection_count(), 0);

    // --- (8b / F2 Post-First-Byte) Mid-stream oversize chunk `k = 1` on `side_effecting = true` ---
    // Chunk 0 (`64 KiB`) crosses the wire after `BulkStreamOpen` (`AfterFirstPayloadByte`),
    // then chunk 1 faults with `MAX_BULK_CHUNK_BYTES + 1`:
    // MUST return `DeadlineExceededUncertain { side_effecting: true, ReconcileBeforeRetry }`,
    // record reconciliation lock, and close the half-written session so server leaks 0 streams!
    let fault_key = IdempotencyKey::from_bytes([0x81; 16]);
    let fault_contract = RemoteCallContract::new([0x81; 16], fault_key, 100, 2, 0, true);
    let declared_two_chunks = (2 * MAX_BULK_CHUNK_BYTES) as u64;
    let (fault_outcome, fault_kind) = transport_a
        .stream_bulk_from_producer(node_b.cell, &fault_contract, declared_two_chunks, |seq| {
            match seq {
                0 => Some(vec![0x5au8; MAX_BULK_CHUNK_BYTES]),
                1 => Some(vec![0xa5u8; MAX_BULK_CHUNK_BYTES + 1]),
                _ => None,
            }
        })
        .await
        .expect("post-first-byte producer fault must classify as RemoteCallOutcome");
    assert!(fault_kind.is_some());
    assert_eq!(
        fault_outcome,
        RemoteCallOutcome::DeadlineExceededUncertain {
            deadline_tick: 100,
            elapsed_ticks: 12,
            side_effecting: true,
            retry_directive: RemoteRetryDirective::ReconcileBeforeRetry,
            idempotency_key: fault_key,
        }
    );
    assert!(
        transport_a
            .reconciliation_tracker()
            .requires_reconciliation(fault_key),
        "post-first-byte bulk producer fault MUST record uncertain side-effect reconciliation lock"
    );
    assert_eq!(
        transport_a
            .stream_bulk_chunks(node_b.cell, &fault_contract, vec![vec![0x11u8; 32]])
            .await,
        Err(TransportDispatchRefusal::BlindRetryBlocked(
            RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation {
                idempotency_key: fault_key,
            }
        ))
    );

    // Wait briefly for TCP/TLS close to propagate to server reader task:
    // BOTH client & server active connection counts and server's inbound stream map MUST drop to 0!
    for _ in 0..25 {
        if accounting_a.active_connection_count() == 0
            && accounting_b.active_connection_count() == 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(accounting_a.active_connection_count(), 0);
    assert_eq!(accounting_b.active_connection_count(), 0);
    for s in running_b.accepted_sessions() {
        assert_eq!(
            s.active_inbound_bulk_stream_count(),
            0,
            "aborted bulk session must leave zero leaked inbound streams on server"
        );
    }

    // Reconcile `fault_key` and verify a clean retry succeeds over a fresh session!
    assert!(
        transport_a
            .reconciliation_tracker()
            .mark_reconciled(fault_key)
    );
    let retry_out = transport_a
        .stream_bulk_chunks(node_b.cell, &fault_contract, vec![vec![0x33u8; 1024]])
        .await
        .expect("reconciled bulk retry");
    assert!(matches!(retry_out, RemoteCallOutcome::Succeeded { .. }));
    assert_eq!(completed_streams.load(Ordering::Acquire), 1);

    // --- (8c / F3) Active Connection Accounting Decrements Immediately on Session Close ---
    assert_eq!(accounting_a.active_connection_count(), 1);
    assert_eq!(accounting_b.active_connection_count(), 1);
    transport_a.close_all_sessions();
    for _ in 0..25 {
        if accounting_a.active_connection_count() == 0
            && accounting_b.active_connection_count() == 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        accounting_a.active_connection_count(),
        0,
        "client active_connection_count must return to 0 on session close"
    );
    assert_eq!(
        accounting_b.active_connection_count(),
        0,
        "server active_connection_count must return to 0 on peer disconnect even while listener is alive"
    );
    assert!(accounting_a.peak_active_connection_count() >= 1);
    assert!(accounting_b.peak_active_connection_count() >= 1);

    // --- (8d / F3) `max_concurrent_bulk_streams = 2` Rejects 3rd Concurrent Stream Fail-Closed ---
    hold_drain.store(1, Ordering::Release);
    let mut stream_tasks = Vec::new();
    for idx in 0u8..3 {
        let t = Arc::clone(&transport_a);
        let target = node_b.cell;
        stream_tasks.push(tokio::spawn(async move {
            let c = RemoteCallContract::new(
                [0x90 + idx; 16],
                IdempotencyKey::from_bytes([0x90 + idx; 16]),
                200,
                1,
                0,
                false,
            );
            t.stream_bulk_chunks(target, &c, vec![vec![0x77u8; 4096]])
                .await
        }));
    }
    let mut saw_stream_cap_abort = false;
    for h in stream_tasks {
        let res = h.await.expect("join concurrent bulk").expect("dispatch");
        if matches!(res, RemoteCallOutcome::DeadlineExceededUncertain { .. }) {
            saw_stream_cap_abort = true;
        }
    }
    assert!(
        saw_stream_cap_abort,
        "exceeding max_concurrent_bulk_streams=2 on one connection must fail closed"
    );
    unblock_drain.notify_waiters();

    running_b.shutdown().await;
    assert_eq!(accounting_b.bound_listener_count(), 0);
}
