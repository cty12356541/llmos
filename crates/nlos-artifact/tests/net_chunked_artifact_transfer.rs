//! Cross-socket chunked resumable content-addressed [`ArtifactTransferPackage`]
//! streaming integration tests over real TLS 1.3 mTLS + 5-axis `CellFence`
//! sessions (`[DIST-ART-001]`, `[DIST-NAME-001]`, `[TCB-ENDPOINT-001]`,
//! `[DIST-FAIL-001]`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::{
    ArtifactBulkChunkFrame, ArtifactConflictStrategy, ArtifactDeletionSemantics, ArtifactError,
    ArtifactMergeStrategy, ArtifactNetTransferClient, ArtifactNetTransferError,
    ArtifactNetTransferRejection, ArtifactNetTransferServer, ArtifactReplicaAuthority,
    ArtifactStore, ArtifactSyncPolicy, ArtifactSyncPolicyRegistry, ArtifactTransferCursorState,
    ArtifactTransferManifest, CollectOrphanBlobsRequest, ContentDigest, CreateArtifactSpec,
    CrossStoreArtifactSyncReport, ExportArtifactTransferRequest,
    FinalizeNetArtifactTransferRequest, ProvenanceSourceTriple, PutRevisionRequest,
    collect_orphan_blobs_with_sync_roots, export_artifact_transfer_package,
    read_retained_sync_blob, slice_remaining_blob_into_framed_chunks,
    staged_transfer_dir_for_package,
};
use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodyTier, KeyPurpose as CustodyKeyPurpose,
    SigningKeyCustodyVault,
};
use nlos_cell::{
    CellEpoch, CellFence, CellFencingToken, CellIdentity, RemoteCallContract, RemoteCallOutcome,
    RemoteRetryDirective, RemoteRetryRefusal,
};
use nlos_identity::IdentityAuthority;
use nlos_net::{
    AuthorityCustodyPeerVerifier, BulkStreamHandler, BulkStreamTransport, ClusterProfile,
    CustodyPeerSigner, EndpointHintRecord, EndpointResolver, EndpointRoutingVersion,
    FenceNonceRegistry, MonotonicEndpointResolver, MonotonicPeerFenceGate, MtlsCellTransport,
    MtlsEndpointListener, MuxConfig, NetTransportConfig, NodeSecurityDeps, NodeTlsCredentials,
    RpcRequestHandler, RunningMtlsListener, SocketActivityAccounting, TransportDispatchRefusal,
};
use nlos_types::{
    ArtifactId, CommitPermitId, Generation, IdempotencyKey, KeyId, PrincipalId, SchedulerDomainId,
    TaskId,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

const TEST_NOW_MS: u64 = 1_785_000_000_000;
const CONTENT_TYPE_FF: &str = "application/vnd.llmos.checkpoint-v1";
const CONTENT_TYPE_QUARANTINE: &str = "application/vnd.llmos.collab-doc-v1";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-art-netxfer-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create test temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn cell_id(tag: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([tag; 16]))
}

fn wrapping_kek(node_idx: u8) -> [u8; 32] {
    [node_idx.wrapping_add(0xA0); 32]
}

fn provenance(tag: u8) -> ProvenanceSourceTriple {
    ProvenanceSourceTriple {
        source_a: [tag; 16],
        source_b: [tag.wrapping_add(1); 16],
        source_digest: ContentDigest::from_bytes([tag.wrapping_add(2); 32]),
    }
}

fn standard_policy_registry() -> ArtifactSyncPolicyRegistry {
    let mut reg = ArtifactSyncPolicyRegistry::new();
    reg.register(ArtifactSyncPolicy {
        content_type: CONTENT_TYPE_FF.to_string(),
        merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
        conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
        replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
        deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
    })
    .expect("register fast-forward policy");
    reg.register(ArtifactSyncPolicy {
        content_type: CONTENT_TYPE_QUARANTINE.to_string(),
        merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
        conflict_strategy: ArtifactConflictStrategy::QuarantineDivergentBranch,
        replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
        deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
    })
    .expect("register quarantine-divergent policy");
    reg
}

#[derive(Clone, Copy)]
struct PeerTransportMeta {
    cell: CellIdentity,
    principal_id: PrincipalId,
    key_id: KeyId,
    key_generation: Generation,
}

struct IsolatedArtifactHost {
    meta: PeerTransportMeta,
    store: Arc<ArtifactStore>,
    server: Arc<ArtifactNetTransferServer>,
    security: NodeSecurityDeps,
}

struct TwoHostArtifactHarness {
    resolver: Arc<MonotonicEndpointResolver>,
    host_a: Option<IsolatedArtifactHost>,
    host_b: Option<IsolatedArtifactHost>,
    listener_a: Option<RunningMtlsListener>,
    listener_b: Option<RunningMtlsListener>,
    transport_a: Option<MtlsCellTransport>,
    transport_b: Option<MtlsCellTransport>,
    // Always last field for Windows file-lock safe TempDir teardown.
    _temp_dir: TempDir,
}

impl TwoHostArtifactHarness {
    #[allow(clippy::too_many_lines)]
    async fn setup(label: &str, net_cfg: NetTransportConfig) -> Self {
        let temp_dir = TempDir::new(label);
        let root = temp_dir.path();

        let bootstrap_dir = root.join("bootstrap-pki");
        let mut metas = Vec::with_capacity(2);
        {
            let pki_identity =
                IdentityAuthority::open(&bootstrap_dir).expect("open bootstrap PKI identity");
            for idx in 1..=2_u8 {
                let host_dir = root.join(format!("host-{idx}"));
                let custody_dir = host_dir.join("custody");
                std::fs::create_dir_all(&custody_dir).expect("create custody dir");
                let vault = SigningKeyCustodyVault::open_durable(&custody_dir, wrapping_kek(idx))
                    .expect("open custody vault");
                let (_, binding) = vault
                    .bootstrap_principal_under_custody(
                        &pki_identity,
                        BootstrapCustodyPrincipalRequest {
                            authority_id: [idx; 16],
                            principal_profile_digest: [idx.wrapping_add(0x11); 32],
                            control_domain_policy_digest: [idx.wrapping_add(0x22); 32],
                            key_purpose: CustodyKeyPurpose::SemanticSigning,
                            custody_tier: CustodyTier::OperatorFileSealed,
                            valid_from_ms: TEST_NOW_MS - 60_000,
                            valid_until_ms: TEST_NOW_MS + 86_400_000,
                            seed: [idx.wrapping_add(0x33); 32],
                            bootstrap_idempotency_key: IdempotencyKey::from_bytes(
                                [idx.wrapping_add(0x44); 16],
                            ),
                            custody_binding_idempotency_key: IdempotencyKey::from_bytes(
                                [idx.wrapping_add(0x55); 16],
                            ),
                            provisioned_at_ms: TEST_NOW_MS - 60_000,
                        },
                    )
                    .expect("bootstrap mTLS node principal");
                metas.push(PeerTransportMeta {
                    cell: cell_id(idx),
                    principal_id: binding.principal_id,
                    key_id: binding.key_id,
                    key_generation: binding.key_generation,
                });
            }
        }

        let pki_db = bootstrap_dir.join("identity-authority.db");
        for idx in 1..=2_u8 {
            let host_dir = root.join(format!("host-{idx}"));
            std::fs::copy(&pki_db, host_dir.join("identity-authority.db"))
                .expect("seed host public-key DB");
        }
        let _ = std::fs::remove_dir_all(&bootstrap_dir);

        let all_peers = [metas[0], metas[1]];
        let resolver = Arc::new(MonotonicEndpointResolver::new());
        let host_a = open_isolated_artifact_host(&root.join("host-1"), 1, &all_peers, &resolver);
        let host_b = open_isolated_artifact_host(&root.join("host-2"), 2, &all_peers, &resolver);

        let bind_loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let profile_a = ClusterProfile::MultiHostCluster {
            local_cell: host_a.meta.cell,
            bind_hint: bind_loopback,
        };
        let profile_b = ClusterProfile::MultiHostCluster {
            local_cell: host_b.meta.cell,
            bind_hint: bind_loopback,
        };

        let rpc_a: Arc<dyn RpcRequestHandler> = Arc::clone(&host_a.server) as _;
        let bulk_a: Arc<dyn BulkStreamHandler> = Arc::clone(&host_a.server) as _;
        let rpc_b: Arc<dyn RpcRequestHandler> = Arc::clone(&host_b.server) as _;
        let bulk_b: Arc<dyn BulkStreamHandler> = Arc::clone(&host_b.server) as _;

        let listener_a = MtlsEndpointListener::bind(
            profile_a,
            host_a.security.clone(),
            net_cfg,
            SocketActivityAccounting::new(),
            Some(Arc::clone(&rpc_a)),
            Some(Arc::clone(&bulk_a)),
        )
        .await
        .expect("bind mTLS listener A")
        .spawn_accept_loop();

        let listener_b = MtlsEndpointListener::bind(
            profile_b,
            host_b.security.clone(),
            net_cfg,
            SocketActivityAccounting::new(),
            Some(Arc::clone(&rpc_b)),
            Some(Arc::clone(&bulk_b)),
        )
        .await
        .expect("bind mTLS listener B")
        .spawn_accept_loop();

        register_hint(&resolver, all_peers[0], listener_a.local_addr(), 1);
        register_hint(&resolver, all_peers[1], listener_b.local_addr(), 1);

        let dyn_resolver: Arc<dyn EndpointResolver> =
            Arc::clone(&resolver) as Arc<dyn EndpointResolver>;

        let transport_a = MtlsCellTransport::new_multi_host(
            profile_a,
            Arc::clone(&dyn_resolver),
            host_a.security.clone(),
            net_cfg,
            SocketActivityAccounting::new(),
            Some(rpc_a),
            Some(bulk_a),
        )
        .expect("build mTLS transport A");

        let transport_b = MtlsCellTransport::new_multi_host(
            profile_b,
            dyn_resolver,
            host_b.security.clone(),
            net_cfg,
            SocketActivityAccounting::new(),
            Some(rpc_b),
            Some(bulk_b),
        )
        .expect("build mTLS transport B");

        Self {
            resolver,
            host_a: Some(host_a),
            host_b: Some(host_b),
            listener_a: Some(listener_a),
            listener_b: Some(listener_b),
            transport_a: Some(transport_a),
            transport_b: Some(transport_b),
            _temp_dir: temp_dir,
        }
    }

    async fn shutdown(mut self) {
        if let Some(ta) = self.transport_a.take() {
            ta.close_all_sessions();
        }
        if let Some(tb) = self.transport_b.take() {
            tb.close_all_sessions();
        }
        if let Some(la) = self.listener_a.take() {
            la.shutdown().await;
        }
        if let Some(lb) = self.listener_b.take() {
            lb.shutdown().await;
        }
        self.host_a.take();
        self.host_b.take();
        let _ = &self.resolver;
    }
}

fn open_isolated_artifact_host(
    host_dir: &Path,
    node_idx: u8,
    all_peers: &[PeerTransportMeta; 2],
    resolver: &Arc<MonotonicEndpointResolver>,
) -> IsolatedArtifactHost {
    let identity = Arc::new(IdentityAuthority::open(host_dir).expect("open host identity"));
    let vault = Arc::new(
        SigningKeyCustodyVault::open_durable(host_dir.join("custody"), wrapping_kek(node_idx))
            .expect("open host custody vault"),
    );
    let me = all_peers[usize::from(node_idx - 1)];
    let signer = Arc::new(CustodyPeerSigner::new(
        Arc::clone(&vault),
        me.principal_id,
        me.key_id,
        me.key_generation,
    ));
    let tls_credentials = NodeTlsCredentials::issue(
        signer.as_ref(),
        [node_idx.wrapping_add(0x71); 32],
        TEST_NOW_MS - 1_000,
        TEST_NOW_MS + 3_600_000,
    )
    .expect("issue node mTLS credentials");

    let dyn_resolver: Arc<dyn EndpointResolver> = Arc::clone(resolver) as Arc<dyn EndpointResolver>;
    let fence_gate = Arc::new(MonotonicPeerFenceGate::new(Some(dyn_resolver)));
    for peer in all_peers {
        fence_gate
            .pin_cell_principal(peer.cell, peer.principal_id)
            .expect("pin peer CellIdentity <-> PrincipalId");
    }

    let boot_gen = Generation::new(NonZeroU64::new(1).expect("non-zero"));
    let cell = me.cell;
    let security = NodeSecurityDeps {
        local_fence: Arc::new(move || {
            CellFence::present(
                cell,
                boot_gen,
                CellEpoch::INITIAL,
                CellFencingToken::INITIAL,
            )
        }),
        local_signer: signer,
        peer_verifier: Arc::new(AuthorityCustodyPeerVerifier::new(
            Arc::clone(&identity),
            vec![vault],
        )),
        fence_policy: fence_gate,
        nonce_registry: Arc::new(FenceNonceRegistry::new(256)),
        clock_ms: Arc::new(|| TEST_NOW_MS),
        tls_credentials,
    };

    let store_dir = host_dir.join("artifact-store");
    let store = Arc::new(ArtifactStore::open(&store_dir).expect("open ArtifactStore"));
    let server = Arc::new(ArtifactNetTransferServer::new(
        cell,
        CellEpoch::INITIAL,
        Arc::clone(&store),
        standard_policy_registry(),
    ));
    IsolatedArtifactHost {
        meta: me,
        store,
        server,
        security,
    }
}

fn register_hint(
    resolver: &MonotonicEndpointResolver,
    meta: PeerTransportMeta,
    addr: SocketAddr,
    seq: u64,
) {
    let version = EndpointRoutingVersion::new(
        Generation::new(NonZeroU64::new(1).expect("non-zero")),
        CellEpoch::INITIAL,
        seq,
    );
    let hint = EndpointHintRecord::new(meta.cell, addr, version, TEST_NOW_MS)
        .with_expected_principal(meta.principal_id);
    resolver.upsert_hint(hint).expect("upsert endpoint hint");
}

fn deterministic_blob(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            let mix = u8::try_from(i % 251).unwrap_or(0);
            seed.wrapping_mul(31).wrapping_add(mix)
        })
        .collect()
}

/// Spawns a one-shot TCP proxy that relays client->server bytes up to `cut_after_c2s_bytes`
/// and server->client bytes unimpeded until the cut triggers, then abruptly drops both
/// TCP halves to simulate a mid-blob physical network cut (`AfterFirstPayloadByte`).
async fn spawn_one_shot_cut_proxy(
    target_addr: SocketAddr,
    cut_after_c2s_bytes: usize,
) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind one-shot TCP cut proxy");
    let proxy_addr = listener.local_addr().expect("proxy local addr");
    tokio::spawn(async move {
        let Ok((mut inbound, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut outbound) = TcpStream::connect(target_addr).await else {
            return;
        };
        let (mut in_r, mut in_w) = inbound.split();
        let (mut out_r, mut out_w) = outbound.split();

        let mut c2s_forwarded = 0_usize;
        let mut c2s_buf = [0_u8; 8_192];
        let mut s2c_buf = [0_u8; 8_192];
        loop {
            tokio::select! {
                res = in_r.read(&mut c2s_buf) => {
                    let Ok(n) = res else { break };
                    if n == 0 { break; }
                    let remaining = cut_after_c2s_bytes.saturating_sub(c2s_forwarded);
                    let to_send = n.min(remaining);
                    if to_send > 0 && out_w.write_all(&c2s_buf[..to_send]).await.is_err() {
                        break;
                    }
                    let _ = out_w.flush().await;
                    c2s_forwarded += to_send;
                    if c2s_forwarded >= cut_after_c2s_bytes {
                        break;
                    }
                }
                res = out_r.read(&mut s2c_buf) => {
                    let Ok(n) = res else { break };
                    if n == 0 { break; }
                    if in_w.write_all(&s2c_buf[..n]).await.is_err() {
                        break;
                    }
                    let _ = in_w.flush().await;
                }
            }
        }
    });
    proxy_addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn large_multi_chunk_artifact_transfer_with_concurrent_control_rpc_non_starvation() {
    let net_cfg = NetTransportConfig {
        mux: MuxConfig {
            bulk_window_bytes: 65_536,
            ..MuxConfig::default()
        },
        ..NetTransportConfig::default()
    };
    let harness = TwoHostArtifactHarness::setup("multi-chunk-window", net_cfg).await;
    let host_a = harness.host_a.as_ref().expect("host A");
    let host_b = harness.host_b.as_ref().expect("host B");
    let transport_a = harness.transport_a.as_ref().expect("transport A");

    let artifact_id = ArtifactId::from_bytes([0x11; 16]);
    host_a
        .store
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
            content_type: CONTENT_TYPE_FF.to_string(),
            application_id: None,
            owner: Some("stage-c-owner".to_string()),
            created_at_ms: TEST_NOW_MS,
        })
        .expect("create artifact on A");

    let rev1_bytes = deterministic_blob(0x31, 220 * 1024);
    let rev2_bytes = deterministic_blob(0x42, 380 * 1024);
    host_a
        .store
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &rev1_bytes,
            created_at_ms: TEST_NOW_MS + 10,
            provenance: provenance(1),
        })
        .expect("put rev1");
    host_a
        .store
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 1,
            bytes: &rev2_bytes,
            created_at_ms: TEST_NOW_MS + 20,
            provenance: provenance(2),
        })
        .expect("put rev2");

    let package = export_artifact_transfer_package(
        &host_a.store,
        ExportArtifactTransferRequest::head(
            artifact_id,
            *host_a.meta.cell.as_bytes(),
            1,
            TEST_NOW_MS + 30,
        ),
    )
    .expect("export multi-revision package");
    let pkg_digest = package.package_digest;

    let target_cell = host_b.meta.cell;
    let caller_cell = host_a.meta.cell;
    let (xfer_res, ping_res) = tokio::join!(
        async {
            let client = ArtifactNetTransferClient::new(caller_cell, transport_a)
                .with_chunk_payload_bytes(32 * 1024);
            client
                .transfer_package_resumable(
                    target_cell,
                    &package,
                    None,
                    TaskId::from_bytes([0x51; 16]),
                    CommitPermitId::from_bytes([0x52; 16]),
                    ContentDigest::from_bytes([0x53; 32]),
                    TEST_NOW_MS + 100,
                    1,
                )
                .await
        },
        async {
            let client = ArtifactNetTransferClient::new(caller_cell, transport_a);
            for i in 0..8_u8 {
                let contract = RemoteCallContract::new(
                    [0x80 + i; 16],
                    IdempotencyKey::from_bytes([0x90 + i; 16]),
                    50,
                    1,
                    0,
                    false,
                );
                let out = client
                    .query_transfer_cursor(target_cell, &contract, pkg_digest)
                    .await
                    .expect("concurrent ControlRpc cursor query must not starve");
                assert!(matches!(
                    out,
                    RemoteCallOutcome::Succeeded { value: Ok(_), .. }
                ));
            }
        }
    );
    let () = ping_res;
    let (summary, report) = xfer_res.expect("chunked mTLS transfer must succeed");
    assert_eq!(summary.resumed_from_verified_chunks, 0);
    assert_eq!(summary.resumed_from_offset_bytes, 0);
    assert_eq!(summary.payload_bytes_transmitted, (220 + 380) * 1024);
    assert!(summary.chunks_transmitted >= 19);
    assert!(matches!(
        report,
        CrossStoreArtifactSyncReport::ImportedFastForward { .. }
    ));

    assert!(
        !staged_transfer_dir_for_package(&host_b.store, pkg_digest).exists(),
        "staging directory must be cleaned up upon finalization"
    );
    assert_eq!(
        host_b
            .store
            .get_revision(artifact_id, 1, TEST_NOW_MS + 200)
            .expect("read rev1 on B"),
        rev1_bytes
    );
    assert_eq!(
        host_b
            .store
            .get_revision(artifact_id, 2, TEST_NOW_MS + 200)
            .expect("read rev2 on B"),
        rev2_bytes
    );

    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn mid_blob_tcp_disconnect_preserves_staged_cursor_blocks_blind_retry_and_resumes_exact_offset()
 {
    let net_cfg = NetTransportConfig {
        mux: MuxConfig {
            bulk_window_bytes: 65_536,
            ..MuxConfig::default()
        },
        ..NetTransportConfig::default()
    };
    let harness = TwoHostArtifactHarness::setup("mid-blob-resume", net_cfg).await;
    let host_a = harness.host_a.as_ref().expect("host A");
    let host_b = harness.host_b.as_ref().expect("host B");
    let listener_b = harness.listener_b.as_ref().expect("listener B");
    let transport_a = harness.transport_a.as_ref().expect("transport A");

    let artifact_id = ArtifactId::from_bytes([0x61; 16]);
    host_a
        .store
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0x62; 16]),
            content_type: CONTENT_TYPE_FF.to_string(),
            application_id: None,
            owner: None,
            created_at_ms: TEST_NOW_MS,
        })
        .expect("create artifact on A");

    // 12 chunks of exactly 32 KiB = 384 KiB.
    let chunk_payload_bytes: u32 = 32 * 1024;
    let blob_bytes = deterministic_blob(0x77, 12 * 32 * 1024);
    host_a
        .store
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &blob_bytes,
            created_at_ms: TEST_NOW_MS + 10,
            provenance: provenance(3),
        })
        .expect("put 384 KiB rev1");

    let package = export_artifact_transfer_package(
        &host_a.store,
        ExportArtifactTransferRequest::head(
            artifact_id,
            *host_a.meta.cell.as_bytes(),
            1,
            TEST_NOW_MS + 20,
        ),
    )
    .expect("export 384 KiB package");
    let manifest = ArtifactTransferManifest::from_package(&package);
    let client = ArtifactNetTransferClient::new(host_a.meta.cell, transport_a)
        .with_chunk_payload_bytes(chunk_payload_bytes);

    // Route Host B through a one-shot TCP cut proxy that drops after ~190 KiB
    // (after mTLS handshake + BeginTransferSession + ~4-5 window-paced 32 KiB chunks).
    let cut_proxy_addr = spawn_one_shot_cut_proxy(listener_b.local_addr(), 190_000).await;
    register_hint(&harness.resolver, host_b.meta, cut_proxy_addr, 2);

    let begin_contract = RemoteCallContract::new(
        [0x70; 16],
        IdempotencyKey::from_bytes([0x71; 16]),
        50,
        1,
        0,
        false,
    );
    let initial_cursor = match client
        .begin_transfer_session(host_b.meta.cell, &begin_contract, &manifest)
        .await
        .expect("begin session over proxy")
    {
        RemoteCallOutcome::Succeeded { value: Ok(c), .. } => c,
        other => panic!("expected session cursor, got {other:?}"),
    };
    assert_eq!(initial_cursor.total_verified_chunks(), 0);

    let interrupted_key = IdempotencyKey::from_bytes([0x72; 16]);
    let interrupted_contract = RemoteCallContract::new([0x73; 16], interrupted_key, 50, 2, 0, true);
    let cut_outcome = client
        .stream_remaining_blob_chunks(
            host_b.meta.cell,
            &interrupted_contract,
            package.package_digest,
            &package.blobs[0],
            &initial_cursor.blob_cursors[0],
            chunk_payload_bytes,
        )
        .await
        .expect("transport call returns classified outcome, not pre-dispatch refusal");

    assert!(
        matches!(
            cut_outcome,
            RemoteCallOutcome::DeadlineExceededUncertain {
                side_effecting: true,
                retry_directive: RemoteRetryDirective::ReconcileBeforeRetry,
                ..
            }
        ),
        "mid-blob TCP cut must surface DeadlineExceededUncertain(ReconcileBeforeRetry), got {cut_outcome:?}"
    );

    // 1. Canonical ArtifactStore on Host B MUST remain completely untouched!
    assert!(matches!(
        host_b.store.inspect_artifact(artifact_id),
        Err(ArtifactError::ArtifactNotFound(_))
    ));
    let stage_dir = staged_transfer_dir_for_package(&host_b.store, package.package_digest);
    assert!(
        stage_dir.is_dir(),
        "verified partial chunks must remain staged in artifacts/tmp/net-stage/"
    );

    // 2. Blind retry on `interrupted_key` BEFORE reconciliation MUST be refused!
    let blind_retry = client
        .stream_remaining_blob_chunks(
            host_b.meta.cell,
            &interrupted_contract,
            package.package_digest,
            &package.blobs[0],
            &initial_cursor.blob_cursors[0],
            chunk_payload_bytes,
        )
        .await;
    assert!(matches!(
        blind_retry,
        Err(ArtifactNetTransferError::DispatchRefusal(
            TransportDispatchRefusal::BlindRetryBlocked(
                RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation { idempotency_key }
            )
        )) if idempotency_key == interrupted_key
    ));

    // Restore direct routing hint to Host B's live mTLS listener.
    register_hint(&harness.resolver, host_b.meta, listener_b.local_addr(), 3);
    transport_a.invalidate_cached_session(host_b.meta.cell);

    // 3. Premature FinalizeStagedTransfer while chunks are missing MUST fail closed!
    let premature_finalize_contract = RemoteCallContract::new(
        [0x74; 16],
        IdempotencyKey::from_bytes([0x75; 16]),
        50,
        1,
        0,
        true,
    );
    let premature_res = client
        .finalize_staged_transfer(
            host_b.meta.cell,
            &premature_finalize_contract,
            &FinalizeNetArtifactTransferRequest {
                package_manifest_digest: package.package_digest,
                expected_local_state: None,
                task_id: TaskId::from_bytes([0x76; 16]),
                permit_id: CommitPermitId::from_bytes([0x77; 16]),
                write_set_root: ContentDigest::from_bytes([0x78; 32]),
                synced_at_ms: TEST_NOW_MS + 50,
            },
        )
        .await
        .expect("premature finalize rpc");
    assert!(matches!(
        premature_res,
        RemoteCallOutcome::Succeeded {
            value: Err(ArtifactNetTransferRejection::IncompleteStagedBlobs {
                incomplete_blobs: 1,
                ..
            }),
            ..
        }
    ));
    assert!(matches!(
        host_b.store.inspect_artifact(artifact_id),
        Err(ArtifactError::ArtifactNotFound(_))
    ));

    // 4. Reconcile interrupted stream via QueryTransferCursor and inspect exact cursor!
    let query_contract = RemoteCallContract::new(
        [0x79; 16],
        IdempotencyKey::from_bytes([0x7A; 16]),
        50,
        1,
        0,
        false,
    );
    let cursor_state = client
        .reconcile_interrupted_stream(
            host_b.meta.cell,
            transport_a.reconciliation_tracker(),
            interrupted_key,
            &query_contract,
            package.package_digest,
        )
        .await
        .expect("reconcile interrupted stream");

    let staged_cursor = match cursor_state {
        ArtifactTransferCursorState::Staged(c) => c,
        other => panic!("expected Staged cursor after mid-blob cut, got {other:?}"),
    };
    let verified_chunks = staged_cursor.total_verified_chunks();
    assert!(
        (2..12).contains(&verified_chunks),
        "expected partial verified chunks in 2..12, got {verified_chunks}"
    );
    assert_eq!(
        staged_cursor.total_verified_bytes(),
        u64::from(verified_chunks) * u64::from(chunk_payload_bytes)
    );
    assert!(
        !transport_a
            .reconciliation_tracker()
            .requires_reconciliation(interrupted_key)
    );

    // 5. Resume transfer: ONLY remaining (12 - verified_chunks) chunks are transmitted!
    let (resume_summary, report) = client
        .transfer_package_resumable(
            host_b.meta.cell,
            &package,
            None,
            TaskId::from_bytes([0x7B; 16]),
            CommitPermitId::from_bytes([0x7C; 16]),
            ContentDigest::from_bytes([0x7D; 32]),
            TEST_NOW_MS + 100,
            2,
        )
        .await
        .expect("resume remaining chunks and finalize");

    assert_eq!(resume_summary.resumed_from_verified_chunks, verified_chunks);
    assert_eq!(resume_summary.chunks_transmitted, 12 - verified_chunks);
    assert_eq!(
        resume_summary.payload_bytes_transmitted,
        u64::from(12 - verified_chunks) * u64::from(chunk_payload_bytes)
    );
    assert!(matches!(
        report,
        CrossStoreArtifactSyncReport::ImportedFastForward { .. }
    ));
    assert!(!stage_dir.exists());
    assert_eq!(
        host_b
            .store
            .get_revision(artifact_id, 1, TEST_NOW_MS + 200)
            .expect("read finalized rev1 on B"),
        blob_bytes
    );

    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn tampered_chunk_sha256_tampered_manifest_unregistered_policy_and_forged_cell_fail_closed() {
    let harness =
        TwoHostArtifactHarness::setup("tamper-fail-closed", NetTransportConfig::default()).await;
    let host_a = harness.host_a.as_ref().expect("host A");
    let host_b = harness.host_b.as_ref().expect("host B");
    let transport_a = harness.transport_a.as_ref().expect("transport A");

    let artifact_id = ArtifactId::from_bytes([0x81; 16]);
    host_a
        .store
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
            content_type: CONTENT_TYPE_FF.to_string(),
            application_id: None,
            owner: None,
            created_at_ms: TEST_NOW_MS,
        })
        .expect("create artifact on A");
    host_a
        .store
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &deterministic_blob(0x55, 40_000),
            created_at_ms: TEST_NOW_MS + 10,
            provenance: provenance(4),
        })
        .expect("put rev1");
    let package = export_artifact_transfer_package(
        &host_a.store,
        ExportArtifactTransferRequest::head(
            artifact_id,
            *host_a.meta.cell.as_bytes(),
            1,
            TEST_NOW_MS + 20,
        ),
    )
    .expect("export package");
    let valid_manifest = ArtifactTransferManifest::from_package(&package);

    // 1. Forged caller_cell != authenticated mTLS peer CellIdentity -> rejected!
    let forged_client = ArtifactNetTransferClient::new(cell_id(0xEE), transport_a);
    let c1 = RemoteCallContract::new(
        [0x83; 16],
        IdempotencyKey::from_bytes([0x84; 16]),
        50,
        1,
        0,
        false,
    );
    let forged_res = forged_client
        .begin_transfer_session(host_b.meta.cell, &c1, &valid_manifest)
        .await
        .expect("forged cell rpc");
    assert!(matches!(
        forged_res,
        RemoteCallOutcome::Succeeded {
            value: Err(ArtifactNetTransferRejection::PeerCellIdentityMismatch { .. }),
            ..
        }
    ));

    let client = ArtifactNetTransferClient::new(host_a.meta.cell, transport_a);

    // 2. Unregistered content_type -> rejected before staging!
    let mut unregistered_manifest = valid_manifest.clone();
    unregistered_manifest.content_type = "application/x-unregistered-v0".to_string();
    unregistered_manifest.package_manifest_digest = unregistered_manifest.compute_manifest_digest();
    let c2 = RemoteCallContract::new(
        [0x85; 16],
        IdempotencyKey::from_bytes([0x86; 16]),
        50,
        1,
        0,
        false,
    );
    let unreg_res = client
        .begin_transfer_session(host_b.meta.cell, &c2, &unregistered_manifest)
        .await
        .expect("unregistered policy rpc");
    assert!(matches!(
        unreg_res,
        RemoteCallOutcome::Succeeded {
            value: Err(ArtifactNetTransferRejection::PolicyOrSyncRejected(_)),
            ..
        }
    ));
    assert!(
        !staged_transfer_dir_for_package(
            &host_b.store,
            unregistered_manifest.package_manifest_digest
        )
        .exists()
    );

    // 3. Tampered package_manifest_digest -> rejected before staging!
    let mut bad_digest_manifest = valid_manifest.clone();
    bad_digest_manifest.package_manifest_digest[0] ^= 0xFF;
    let c3 = RemoteCallContract::new(
        [0x87; 16],
        IdempotencyKey::from_bytes([0x88; 16]),
        50,
        1,
        0,
        false,
    );
    let bad_pkg_res = client
        .begin_transfer_session(host_b.meta.cell, &c3, &bad_digest_manifest)
        .await
        .expect("tampered manifest digest rpc");
    assert!(matches!(
        bad_pkg_res,
        RemoteCallOutcome::Succeeded {
            value: Err(ArtifactNetTransferRejection::PackageDigestMismatch { .. }),
            ..
        }
    ));

    // 4. Tampered chunk_sha256 / corrupted payload byte inside BulkStream -> rejected!
    let c4 = RemoteCallContract::new(
        [0x89; 16],
        IdempotencyKey::from_bytes([0x8A; 16]),
        50,
        1,
        0,
        false,
    );
    let cursor = match client
        .begin_transfer_session(host_b.meta.cell, &c4, &valid_manifest)
        .await
        .expect("begin valid session")
    {
        RemoteCallOutcome::Succeeded { value: Ok(c), .. } => c,
        other => panic!("expected cursor, got {other:?}"),
    };
    let mut raw_frames = slice_remaining_blob_into_framed_chunks(
        host_a.meta.cell,
        package.package_digest,
        &package.blobs[0],
        &cursor.blob_cursors[0],
        20_000,
    )
    .expect("slice valid frames");
    // Corrupt one payload byte in chunk 0 so NLOSAB01 chunk_sha256 fails on the server,
    // even though transport-level BulkStreamFinish sha256 is computed over the mutated frame!
    let last_idx = raw_frames[0].len() - 1;
    raw_frames[0][last_idx] ^= 0xAA;
    assert!(ArtifactBulkChunkFrame::decode_and_verify(&raw_frames[0]).is_err());

    let c5 = RemoteCallContract::new(
        [0x8B; 16],
        IdempotencyKey::from_bytes([0x8C; 16]),
        50,
        1,
        0,
        true,
    );
    let corrupt_bulk_out = transport_a
        .stream_bulk_chunks(host_b.meta.cell, &c5, raw_frames)
        .await
        .expect("corrupt bulk stream dispatch");
    assert!(
        !matches!(corrupt_bulk_out, RemoteCallOutcome::Succeeded { .. }),
        "corrupted chunk_sha256 must fail closed, got {corrupt_bulk_out:?}"
    );
    assert!(matches!(
        host_b.store.inspect_artifact(artifact_id),
        Err(ArtifactError::ArtifactNotFound(_))
    ));

    // Confirm chunk_sha256 mismatch also verified locally by ArtifactBulkChunkFrame::encode.
    let bad_local_frame = ArtifactBulkChunkFrame {
        caller_cell: host_a.meta.cell,
        package_manifest_digest: package.package_digest,
        total_blob_sha256: package.blobs[0].blob_digest,
        total_blob_bytes: 40_000,
        chunk_index: 0,
        offset: 0,
        chunk_sha256: Sha256::digest(b"wrong").into(),
        payload: vec![1, 2, 3, 4],
    };
    assert!(matches!(
        bad_local_frame.encode(),
        Err(ArtifactNetTransferError::LocalChunkDigestMismatch)
    ));

    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::similar_names, clippy::too_many_lines)]
async fn divergent_replica_heads_over_mtls_quarantine_both_heads_and_survive_orphan_gc() {
    let harness =
        TwoHostArtifactHarness::setup("divergent-quarantine", NetTransportConfig::default()).await;
    let host_a = harness.host_a.as_ref().expect("host A");
    let host_b = harness.host_b.as_ref().expect("host B");
    let transport_a = harness.transport_a.as_ref().expect("transport A");
    let transport_b = harness.transport_b.as_ref().expect("transport B");

    let artifact_id = ArtifactId::from_bytes([0x91; 16]);
    host_a
        .store
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
            content_type: CONTENT_TYPE_QUARANTINE.to_string(),
            application_id: None,
            owner: None,
            created_at_ms: TEST_NOW_MS,
        })
        .expect("create collaborative artifact on A");

    let base_bytes = deterministic_blob(0x10, 70 * 1024);
    host_a
        .store
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &base_bytes,
            created_at_ms: TEST_NOW_MS + 10,
            provenance: provenance(5),
        })
        .expect("put common base rev1 on A");

    let base_pkg = export_artifact_transfer_package(
        &host_a.store,
        ExportArtifactTransferRequest::head(
            artifact_id,
            *host_a.meta.cell.as_bytes(),
            1,
            TEST_NOW_MS + 15,
        ),
    )
    .expect("export base rev1");
    let client_a = ArtifactNetTransferClient::new(host_a.meta.cell, transport_a);
    let client_b = ArtifactNetTransferClient::new(host_b.meta.cell, transport_b);

    let (_, base_report) = client_a
        .transfer_package_resumable(
            host_b.meta.cell,
            &base_pkg,
            None,
            TaskId::from_bytes([0x93; 16]),
            CommitPermitId::from_bytes([0x94; 16]),
            ContentDigest::from_bytes([0x95; 32]),
            TEST_NOW_MS + 20,
            10,
        )
        .await
        .expect("sync common base rev1 A -> B");
    assert!(matches!(
        base_report,
        CrossStoreArtifactSyncReport::ImportedFastForward { .. }
    ));

    // Concurrent divergent rev2 edits on Host A and Host B!
    let branch_a_bytes = deterministic_blob(0x2A, 95 * 1024);
    let branch_b_bytes = deterministic_blob(0x2B, 95 * 1024);
    let digest_a = ContentDigest::of_bytes(&branch_a_bytes);
    let digest_b = ContentDigest::of_bytes(&branch_b_bytes);

    host_a
        .store
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 1,
            bytes: &branch_a_bytes,
            created_at_ms: TEST_NOW_MS + 30,
            provenance: provenance(6),
        })
        .expect("put divergent rev2 on A");
    host_b
        .store
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 1,
            bytes: &branch_b_bytes,
            created_at_ms: TEST_NOW_MS + 31,
            provenance: provenance(7),
        })
        .expect("put divergent rev2 on B");

    let pkg_a = export_artifact_transfer_package(
        &host_a.store,
        ExportArtifactTransferRequest::head(
            artifact_id,
            *host_a.meta.cell.as_bytes(),
            1,
            TEST_NOW_MS + 40,
        ),
    )
    .expect("export divergent pkg A");
    let pkg_b = export_artifact_transfer_package(
        &host_b.store,
        ExportArtifactTransferRequest::head(
            artifact_id,
            *host_b.meta.cell.as_bytes(),
            1,
            TEST_NOW_MS + 41,
        ),
    )
    .expect("export divergent pkg B");

    let (_, report_on_b) = client_a
        .transfer_package_resumable(
            host_b.meta.cell,
            &pkg_a,
            None,
            TaskId::from_bytes([0x96; 16]),
            CommitPermitId::from_bytes([0x97; 16]),
            ContentDigest::from_bytes([0x98; 32]),
            TEST_NOW_MS + 50,
            11,
        )
        .await
        .expect("transfer divergent A -> B");
    let (_, report_on_a) = client_b
        .transfer_package_resumable(
            host_a.meta.cell,
            &pkg_b,
            None,
            TaskId::from_bytes([0x99; 16]),
            CommitPermitId::from_bytes([0x9A; 16]),
            ContentDigest::from_bytes([0x9B; 32]),
            TEST_NOW_MS + 51,
            12,
        )
        .await
        .expect("transfer divergent B -> A");

    let receipt_b = match report_on_b {
        CrossStoreArtifactSyncReport::ConflictIsolated {
            conflict_receipt,
            preserved_both_blobs: true,
            ..
        } => conflict_receipt,
        other => panic!("expected ConflictIsolated on B, got {other:?}"),
    };
    let receipt_a = match report_on_a {
        CrossStoreArtifactSyncReport::ConflictIsolated {
            conflict_receipt,
            preserved_both_blobs: true,
            ..
        } => conflict_receipt,
        other => panic!("expected ConflictIsolated on A, got {other:?}"),
    };

    // Order-independent symmetric conflict_digest across both hosts!
    assert_eq!(receipt_a.conflict_digest, receipt_b.conflict_digest);
    assert_eq!(
        receipt_b.strategy,
        ArtifactConflictStrategy::QuarantineDivergentBranch
    );

    // Canonical heads stay unchanged on each host, while both competing blobs
    // are retained across orphan GC via ReplicaSyncRetentionSet!
    assert_eq!(
        host_b
            .store
            .resolve_head(artifact_id, TEST_NOW_MS + 60)
            .expect("resolve B head")
            .expect("B head")
            .digest,
        digest_b
    );
    let retention_b = host_b.server.snapshot_retention_set();
    let gc_decision = collect_orphan_blobs_with_sync_roots(
        &host_b.store,
        CollectOrphanBlobsRequest {
            idempotency_key: IdempotencyKey::from_bytes([0x9C; 16]),
            collected_at_ms: TEST_NOW_MS + 70,
        },
        &retention_b,
    )
    .expect("run sync-root-aware orphan GC on B");
    assert!(
        gc_decision.receipt().collected_digests.is_empty(),
        "quarantined competing head blob must never be GC-collected while pinned"
    );
    assert_eq!(
        read_retained_sync_blob(&host_b.store, &retention_b, digest_a, TEST_NOW_MS + 75)
            .expect("read quarantined remote blob on B"),
        branch_a_bytes
    );
    assert_eq!(
        read_retained_sync_blob(&host_b.store, &retention_b, digest_b, TEST_NOW_MS + 75)
            .expect("read local head blob on B"),
        branch_b_bytes
    );

    harness.shutdown().await;
}
