//! Cross-socket signed semantic vector checkpoint delta pull and 3-part
//! provenance merge integration tests over real TLS 1.3 mTLS + 5-axis Exporter
//! `CellFence` sessions (`[SEM-CHECKPOINT-001]`, `[DIST-EVENT-001]`,
//! `[DIST-AUDIT-001]`, `[DIST-NAME-001]`, `[TCB-ENDPOINT-001]`).

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer, SigningKey};
use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodyTier, KeyPurpose as CustodyKeyPurpose,
    SigningKeyCustodyVault,
};
use nlos_capability::{
    CapabilityAuthority, CapabilityRights, CapabilityTarget, IssueRootCapabilityRequest,
    SignedIssueRootCapabilityRequest, issue_root_command_message,
};
use nlos_cell::{CellEpoch, CellFence, CellFencingToken, CellIdentity, RemoteCallContract};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose, RevokeKeyRequest};
use nlos_net::{
    AuthorityCustodyPeerVerifier, ClusterProfile, CustodyPeerSigner, EndpointHintRecord,
    EndpointResolver, EndpointRoutingVersion, FenceNonceRegistry, IncomingRpcRequest,
    MonotonicEndpointResolver, MonotonicPeerFenceGate, MtlsCellTransport, MtlsEndpointListener,
    NetTransportConfig, NodeSecurityDeps, NodeTlsCredentials, RpcHandlerReply, RpcRequestHandler,
    RunningMtlsListener, SocketActivityAccounting,
};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessAuthority, RegisterDelegatedProcessRequest,
};
use nlos_semantic::{
    AppendAssertionRequest, AssertionMode, AuthoritySemanticSyncProvider, CrossDomainEventItem,
    SemanticAuthority, SemanticAuthorityError, SemanticNetRejectionCode, SemanticNetSyncClient,
    SemanticNetSyncConfig, SemanticNetSyncError, SemanticNetSyncRequest, SemanticNetSyncResponse,
    SemanticNetSyncServer, SemanticSyncProvider, SemanticVectorCheckpointError, StoreSigner,
    StoreSignerError, TaintFlags, UnsignedAssertionEvent, VectorCheckpointRelation, content_digest,
    decode_sync_request, encode_sync_response, encode_unsigned_assertion_event, semantic_event_id,
    verify_signed_vector_checkpoint,
};
use nlos_types::{
    Generation, IdempotencyKey, KeyId, NamespaceId, PrincipalId, SchedulerDomainId, TaskAttemptId,
    TaskId,
};

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

const TEST_NOW_MS: u64 = 1_780_000_000_000;
const TRUST_SNAPSHOT_A: [u8; 32] = [0xAA; 32];
const TRUST_SNAPSHOT_B: [u8; 32] = [0xBB; 32];

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-sem-netsync-{label}-{}-{seq}",
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
    [node_idx.wrapping_add(0x90); 32]
}

#[derive(Clone)]
struct TestStoreSigner {
    key: SigningKey,
    binding: nlos_identity::IdentityBinding,
}

impl StoreSigner for TestStoreSigner {
    fn principal_id(&self) -> PrincipalId {
        self.binding.principal_id
    }

    fn control_domain_id(&self) -> nlos_types::ControlDomainId {
        self.binding.control_domain_id
    }

    fn key_id(&self) -> KeyId {
        self.binding.key_id
    }

    fn sign(&self, message_digest: &[u8; 32]) -> Result<[u8; 64], StoreSignerError> {
        Ok(self.key.sign(message_digest).to_bytes())
    }
}

#[derive(Clone, Copy)]
struct PeerTransportMeta {
    cell: CellIdentity,
    principal_id: PrincipalId,
    key_id: KeyId,
    key_generation: Generation,
}

struct IsolatedSemanticHost {
    cell: CellIdentity,
    identity: Arc<IdentityAuthority>,
    capability: CapabilityAuthority,
    process: ProcessAuthority,
    semantic: Arc<SemanticAuthority>,
    issuer_key: SigningKey,
    issuer_binding: nlos_identity::IdentityBinding,
    process_binding: nlos_process::ProcessBindingRecord,
    capability_record: nlos_capability::CapabilityRecord,
    store_signer: TestStoreSigner,
    provider: Arc<AuthoritySemanticSyncProvider<TestStoreSigner>>,
    security: NodeSecurityDeps,
}

impl IsolatedSemanticHost {
    fn append_assertion(&self, seed: u8, content: &[u8]) -> nlos_types::SemanticEventId {
        let media_type = "text/plain".to_owned();
        let event = UnsignedAssertionEvent {
            scope: self.capability_record.target,
            issuer: self.issuer_binding.principal_id,
            issuer_execution: nlos_semantic::LocalProcessRef {
                process_id: self.process_binding.process_id,
                generation: self.process_binding.process_generation,
            },
            control_domain: self.issuer_binding.control_domain_id,
            issued_at_unix_ns: (TEST_NOW_MS + u64::from(seed)) * 1_000_000,
            nonce: vec![seed; 16],
            declared_parents: Vec::new(),
            declassification_receipt_id: None,
            valid_until_ms: Some(TEST_NOW_MS + 3_600_000),
            purpose_digest: self.capability_record.purpose_digest,
            content_digest: content_digest(&media_type, content).expect("content digest"),
            assertion_mode: AssertionMode::Inference,
            execution_evidence_receipt_id: None,
            confidence_bp: Some(9_500),
            key_id: self.issuer_binding.key_id,
        };
        let canonical_unsigned_event =
            encode_unsigned_assertion_event(&event).expect("encode assertion");
        let claimed_event_id = semantic_event_id(&canonical_unsigned_event);
        let signature = self
            .issuer_key
            .sign(&nlos_identity::semantic_signature_message(claimed_event_id))
            .to_bytes();

        self.semantic
            .append_assertion(
                &self.identity,
                &self.capability,
                &self.process,
                &self.store_signer,
                &AppendAssertionRequest {
                    canonical_unsigned_event,
                    claimed_event_id,
                    signature,
                    capability: self.capability_record.handle,
                    content_media_type: media_type,
                    content_bytes: content.to_vec(),
                    captured_inputs: Vec::new(),
                    ingress_taint: TaintFlags::default(),
                    authz_policy_digest: [0x99; 32],
                    admission_limit_ms: Some(TEST_NOW_MS + 3_500_000),
                    admitted_at_ms: TEST_NOW_MS + u64::from(seed),
                },
            )
            .expect("append assertion locally");
        claimed_event_id
    }

    fn event_count(&self) -> usize {
        self.semantic
            .export_signed_event_set()
            .expect("export local signed event set")
            .len()
    }
}

struct TwoHostSemanticHarness {
    resolver: Arc<MonotonicEndpointResolver>,
    host_a: Option<IsolatedSemanticHost>,
    host_b: Option<IsolatedSemanticHost>,
    listener_a: Option<RunningMtlsListener>,
    listener_b: Option<RunningMtlsListener>,
    transport_a: Option<MtlsCellTransport>,
    transport_b: Option<MtlsCellTransport>,
    // Always last field for Windows file-lock safe TempDir teardown.
    _temp_dir: TempDir,
}

impl TwoHostSemanticHarness {
    #[allow(clippy::too_many_lines)]
    async fn setup(label: &str, custom_b_handler: Option<Arc<dyn RpcRequestHandler>>) -> Self {
        let temp_dir = TempDir::new(label);
        let root = temp_dir.path();

        // Provision transport custody + semantic writer/store public-key bindings
        // into per-host completely separate directories (`host-1/` and `host-2/`).
        let bootstrap_dir = root.join("bootstrap-pki");
        let mut transport_metas = Vec::with_capacity(2);
        let mut semantic_keys = Vec::with_capacity(2);
        {
            let pki_identity =
                IdentityAuthority::open(&bootstrap_dir).expect("open bootstrap PKI identity");
            for idx in 1..=2_u8 {
                let host_dir = root.join(format!("host-{idx}"));
                let custody_dir = host_dir.join("custody");
                std::fs::create_dir_all(&custody_dir).expect("create custody dir");
                let vault = SigningKeyCustodyVault::open_durable(&custody_dir, wrapping_kek(idx))
                    .expect("open custody vault");
                let (_, transport_binding) = vault
                    .bootstrap_principal_under_custody(
                        &pki_identity,
                        BootstrapCustodyPrincipalRequest {
                            authority_id: [idx; 16],
                            principal_profile_digest: [idx.wrapping_add(0x10); 32],
                            control_domain_policy_digest: [idx.wrapping_add(0x20); 32],
                            key_purpose: CustodyKeyPurpose::SemanticSigning,
                            custody_tier: CustodyTier::OperatorFileSealed,
                            valid_from_ms: TEST_NOW_MS - 60_000,
                            valid_until_ms: TEST_NOW_MS + 86_400_000,
                            seed: [idx.wrapping_add(0x30); 32],
                            bootstrap_idempotency_key: IdempotencyKey::from_bytes(
                                [idx.wrapping_add(0x40); 16],
                            ),
                            custody_binding_idempotency_key: IdempotencyKey::from_bytes(
                                [idx.wrapping_add(0x50); 16],
                            ),
                            provisioned_at_ms: TEST_NOW_MS - 60_000,
                        },
                    )
                    .expect("bootstrap mTLS node principal");
                transport_metas.push(PeerTransportMeta {
                    cell: cell_id(idx),
                    principal_id: transport_binding.principal_id,
                    key_id: transport_binding.key_id,
                    key_generation: transport_binding.key_generation,
                });

                let issuer_seed = idx.wrapping_mul(11);
                let store_seed = idx.wrapping_mul(23);
                let issuer_key = SigningKey::from_bytes(&[issuer_seed; 32]);
                let issuer_binding = pki_identity
                    .bootstrap_principal(BootstrapPrincipalRequest {
                        principal_profile_digest: [issuer_seed.wrapping_add(1); 32],
                        control_domain_policy_digest: [issuer_seed.wrapping_add(2); 32],
                        public_key: issuer_key.verifying_key().to_bytes(),
                        key_purpose: KeyPurpose::SemanticSigning,
                        key_valid_from_ms: TEST_NOW_MS - 60_000,
                        key_valid_until_ms: TEST_NOW_MS + 86_400_000,
                        idempotency_key: IdempotencyKey::from_bytes(
                            [issuer_seed.wrapping_add(3); 16],
                        ),
                        created_at_ms: TEST_NOW_MS - 60_000,
                    })
                    .expect("bootstrap semantic issuer")
                    .binding();

                let store_key = SigningKey::from_bytes(&[store_seed; 32]);
                let store_binding = pki_identity
                    .bootstrap_principal(BootstrapPrincipalRequest {
                        principal_profile_digest: [store_seed.wrapping_add(1); 32],
                        control_domain_policy_digest: [store_seed.wrapping_add(2); 32],
                        public_key: store_key.verifying_key().to_bytes(),
                        key_purpose: KeyPurpose::SemanticSigning,
                        key_valid_from_ms: TEST_NOW_MS - 60_000,
                        key_valid_until_ms: TEST_NOW_MS + 86_400_000,
                        idempotency_key: IdempotencyKey::from_bytes(
                            [store_seed.wrapping_add(3); 16],
                        ),
                        created_at_ms: TEST_NOW_MS - 60_000,
                    })
                    .expect("bootstrap semantic store signer")
                    .binding();

                semantic_keys.push((issuer_key, issuer_binding, store_key, store_binding));
            }
        }

        // Copy only the public-key identity DB into each isolated host root, then delete
        // the bootstrap staging directory so zero shared filesystem path exists.
        let pki_db = bootstrap_dir.join("identity-authority.db");
        for idx in 1..=2_u8 {
            let host_dir = root.join(format!("host-{idx}"));
            std::fs::copy(&pki_db, host_dir.join("identity-authority.db"))
                .expect("seed host public-key directory");
        }
        let _ = std::fs::remove_dir_all(&bootstrap_dir);

        let all_peers: [PeerTransportMeta; 2] = [transport_metas[0], transport_metas[1]];
        let resolver = Arc::new(MonotonicEndpointResolver::new());

        let mut sem_iter = semantic_keys.into_iter();
        let (iss_a, iss_bind_a, st_a, st_bind_a) = sem_iter.next().expect("host A keys");
        let (iss_b, iss_bind_b, st_b, st_bind_b) = sem_iter.next().expect("host B keys");

        let host_a = open_isolated_host(
            &root.join("host-1"),
            1,
            &all_peers,
            &resolver,
            iss_a,
            iss_bind_a,
            st_a,
            st_bind_a,
            TRUST_SNAPSHOT_A,
        );
        let host_b = open_isolated_host(
            &root.join("host-2"),
            2,
            &all_peers,
            &resolver,
            iss_b,
            iss_bind_b,
            st_b,
            st_bind_b,
            TRUST_SNAPSHOT_B,
        );

        let handler_a: Arc<dyn RpcRequestHandler> = Arc::new(SemanticNetSyncServer::new(
            Arc::clone(&host_a.provider),
            TEST_NOW_MS + 500,
        ));
        let handler_b: Arc<dyn RpcRequestHandler> = custom_b_handler.unwrap_or_else(|| {
            Arc::new(SemanticNetSyncServer::new(
                Arc::clone(&host_b.provider),
                TEST_NOW_MS + 500,
            ))
        });

        let bind_loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let profile_a = ClusterProfile::MultiHostCluster {
            local_cell: host_a.cell,
            bind_hint: bind_loopback,
        };
        let profile_b = ClusterProfile::MultiHostCluster {
            local_cell: host_b.cell,
            bind_hint: bind_loopback,
        };

        let listener_a = MtlsEndpointListener::bind(
            profile_a,
            host_a.security.clone(),
            NetTransportConfig::default(),
            SocketActivityAccounting::new(),
            Some(Arc::clone(&handler_a)),
            None,
        )
        .await
        .expect("bind mTLS listener A")
        .spawn_accept_loop();

        let listener_b = MtlsEndpointListener::bind(
            profile_b,
            host_b.security.clone(),
            NetTransportConfig::default(),
            SocketActivityAccounting::new(),
            Some(Arc::clone(&handler_b)),
            None,
        )
        .await
        .expect("bind mTLS listener B")
        .spawn_accept_loop();

        register_hint(&resolver, all_peers[0], listener_a.local_addr());
        register_hint(&resolver, all_peers[1], listener_b.local_addr());

        let dyn_resolver: Arc<dyn EndpointResolver> =
            Arc::clone(&resolver) as Arc<dyn EndpointResolver>;

        let transport_a = MtlsCellTransport::new_multi_host(
            profile_a,
            Arc::clone(&dyn_resolver),
            host_a.security.clone(),
            NetTransportConfig::default(),
            SocketActivityAccounting::new(),
            Some(handler_a),
            None,
        )
        .expect("build mTLS client transport A");

        let transport_b = MtlsCellTransport::new_multi_host(
            profile_b,
            dyn_resolver,
            host_b.security.clone(),
            NetTransportConfig::default(),
            SocketActivityAccounting::new(),
            Some(handler_b),
            None,
        )
        .expect("build mTLS client transport B");

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
        self.transport_a.take();
        self.transport_b.take();
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn open_isolated_host(
    host_dir: &Path,
    node_idx: u8,
    all_peers: &[PeerTransportMeta; 2],
    resolver: &Arc<MonotonicEndpointResolver>,
    issuer_key: SigningKey,
    issuer_binding: nlos_identity::IdentityBinding,
    store_key: SigningKey,
    store_binding: nlos_identity::IdentityBinding,
    trust_snapshot_digest: [u8; 32],
) -> IsolatedSemanticHost {
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
        [node_idx.wrapping_add(0x61); 32],
        TEST_NOW_MS - 1_000,
        TEST_NOW_MS + 3_600_000,
    )
    .expect("issue node mTLS X.509v3 credentials");

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

    let process = ProcessAuthority::open(host_dir).expect("open ProcessAuthority");
    let iso_domain = process
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [node_idx.wrapping_add(4); 32],
            idempotency_key: IdempotencyKey::from_bytes([node_idx.wrapping_add(5); 16]),
            created_at_ms: TEST_NOW_MS - 10_000,
        })
        .expect("create isolation domain")
        .record()
        .clone();
    let process_binding = process
        .register_delegated_process(RegisterDelegatedProcessRequest {
            task_id: TaskId::from_bytes([node_idx.wrapping_add(6); 16]),
            task_attempt_id: TaskAttemptId::from_bytes([node_idx.wrapping_add(7); 16]),
            attempt_generation: Generation::INITIAL,
            isolation_domain_id: iso_domain.isolation_domain_id,
            isolation_domain_generation: iso_domain.generation,
            isolation_domain_fencing_token: iso_domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([node_idx.wrapping_add(8); 16]),
            created_at_ms: TEST_NOW_MS - 10_000,
        })
        .expect("register delegated process")
        .record()
        .clone();

    let capability = CapabilityAuthority::open(host_dir).expect("open CapabilityAuthority");
    let cap_cmd = IssueRootCapabilityRequest {
        issuer_key_id: issuer_binding.key_id,
        holder_key_id: issuer_binding.key_id,
        target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x44; 16])),
        rights: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: Some([0x77; 32]),
        valid_from_ms: TEST_NOW_MS - 50_000,
        valid_until_ms: TEST_NOW_MS + 3_600_000,
        delegation_depth_remaining: 0,
        call_limit: None,
        idempotency_key: IdempotencyKey::from_bytes([node_idx.wrapping_add(9); 16]),
        issued_at_ms: TEST_NOW_MS - 50_000,
    };
    let capability_record = capability
        .issue_root_signed(
            &identity,
            SignedIssueRootCapabilityRequest {
                command: cap_cmd,
                signer: issuer_binding.principal_id,
                signature: issuer_key
                    .sign(&issue_root_command_message(cap_cmd))
                    .to_bytes(),
            },
        )
        .expect("issue semantic append capability")
        .record();

    let semantic = Arc::new(SemanticAuthority::open(host_dir).expect("open SemanticAuthority"));
    let store_signer = TestStoreSigner {
        key: store_key,
        binding: store_binding,
    };
    let provider = Arc::new(
        AuthoritySemanticSyncProvider::new(
            cell,
            Arc::clone(&semantic),
            Arc::clone(&identity),
            store_signer.clone(),
            trust_snapshot_digest,
        )
        .expect("build AuthoritySemanticSyncProvider"),
    );

    IsolatedSemanticHost {
        cell,
        identity,
        capability,
        process,
        semantic,
        issuer_key,
        issuer_binding,
        process_binding,
        capability_record,
        store_signer,
        provider,
        security,
    }
}

fn register_hint(resolver: &MonotonicEndpointResolver, meta: PeerTransportMeta, addr: SocketAddr) {
    let version = EndpointRoutingVersion::new(
        Generation::new(NonZeroU64::new(1).expect("non-zero")),
        CellEpoch::INITIAL,
        1,
    );
    let hint = EndpointHintRecord::new(meta.cell, addr, version, TEST_NOW_MS)
        .with_expected_principal(meta.principal_id);
    resolver.upsert_hint(hint).expect("upsert endpoint hint");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_batch_remote_ahead_and_bidirectional_concurrent_convergence_over_mtls() {
    let harness = TwoHostSemanticHarness::setup("converge", None).await;
    let host_a = harness.host_a.as_ref().expect("host A");
    let host_b = harness.host_b.as_ref().expect("host B");
    let transport_a = harness.transport_a.as_ref().expect("transport A");
    let transport_b = harness.transport_b.as_ref().expect("transport B");

    // Host A appends 5 assertions (`log_seq = 1..=5` in Domain A).
    // Host B independently appends 4 assertions (`log_seq = 1..=4` in Domain B).
    // Overlapping local `log_seq` values across Cells prove zero cross-Cell scalar
    // `log_seq` assumption (`[SEM-CHECKPOINT-001]`).
    for idx in 1..=5_u8 {
        host_a.append_assertion(idx, format!("host-a-event-{idx}").as_bytes());
    }
    for idx in 1..=4_u8 {
        host_b.append_assertion(idx + 20, format!("host-b-event-{idx}").as_bytes());
    }
    assert_eq!(host_a.event_count(), 5);
    assert_eq!(host_b.event_count(), 4);

    let cfg = SemanticNetSyncConfig {
        max_batch_events: 2, // Force 2-item pagination across multiple RPC batches!
        rpc_deadline_ms: 5_000,
        signed_at_ms: TEST_NOW_MS + 1_000,
        base_idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
    };

    // 1. Host A pulls missing Domain B events from Host B (Concurrent pre-sync diff,
    //    4 missing events across 2 bounded batches of size <= 2).
    let client_a = SemanticNetSyncClient::new(transport_a, host_a.provider.as_ref());
    let report_a = client_a
        .pull_and_merge_from_peer(host_b.cell, cfg)
        .await
        .expect("Host A pull and merge from Host B");
    assert_eq!(
        report_a.pre_sync_diff.relation,
        VectorCheckpointRelation::Concurrent
    );
    assert_eq!(report_a.batches_pulled, 2);
    assert_eq!(report_a.pulled_event_count, 4);
    let conv_a = report_a.convergence.as_ref().expect("convergence report A");
    assert_eq!(conv_a.imported_event_ids.len(), 4);
    assert_eq!(conv_a.audit_records.len(), 9);
    assert_eq!(
        report_a.post_sync_diff.relation,
        VectorCheckpointRelation::LocalAhead
    );
    assert_eq!(host_a.event_count(), 9);

    // 2. Host B pulls missing Domain A events from Host A (`RemoteAhead`, 5 missing
    //    events across 3 bounded batches of size 2, 2, 1).
    let client_b = SemanticNetSyncClient::new(transport_b, host_b.provider.as_ref());
    let report_b = client_b
        .pull_and_merge_from_peer(
            host_a.cell,
            SemanticNetSyncConfig {
                base_idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
                ..cfg
            },
        )
        .await
        .expect("Host B pull and merge from Host A");
    assert_eq!(
        report_b.pre_sync_diff.relation,
        VectorCheckpointRelation::RemoteAhead
    );
    assert_eq!(report_b.batches_pulled, 3);
    assert_eq!(report_b.pulled_event_count, 5);
    let conv_b = report_b.convergence.as_ref().expect("convergence report B");
    assert_eq!(conv_b.imported_event_ids.len(), 5);
    assert_eq!(
        report_b.post_sync_diff.relation,
        VectorCheckpointRelation::Equal
    );
    assert_eq!(host_b.event_count(), 9);

    // Verify covered_event_ids_root matches identically across both isolated stores!
    assert_eq!(
        report_a.post_sync_local_checkpoint.covered_event_ids_root,
        report_b.post_sync_local_checkpoint.covered_event_ids_root
    );
    verify_signed_vector_checkpoint(&host_a.identity, &report_b.post_sync_local_checkpoint)
        .expect("Host A verifies Host B post-sync checkpoint");
    verify_signed_vector_checkpoint(&host_b.identity, &report_a.post_sync_local_checkpoint)
        .expect("Host B verifies Host A post-sync checkpoint");

    // 3. Idempotent replay sees Equal, pulls 0 batches, and mutates 0 SQLite rows.
    let replay_a = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                base_idempotency_key: IdempotencyKey::from_bytes([0x33; 16]),
                ..cfg
            },
        )
        .await
        .expect("idempotent replay on Host A");
    assert_eq!(
        replay_a.pre_sync_diff.relation,
        VectorCheckpointRelation::Equal
    );
    assert_eq!(replay_a.batches_pulled, 0);
    assert_eq!(replay_a.pulled_event_count, 0);
    assert!(replay_a.convergence.is_none());
    assert_eq!(host_a.event_count(), 9);

    harness.shutdown().await;
}

fn readonly_contract(tag: u8) -> RemoteCallContract {
    let key = IdempotencyKey::from_bytes([tag; 16]);
    RemoteCallContract::new(key.into_bytes(), key, 5_000, 1, 0, false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_caller_cell_identity_is_rejected_by_mtls_five_axis_pinning() {
    let harness = TwoHostSemanticHarness::setup("forged-caller", None).await;
    let host_a = harness.host_a.as_ref().expect("host A");
    let host_b = harness.host_b.as_ref().expect("host B");
    let transport_a = harness.transport_a.as_ref().expect("transport A");

    host_b.append_assertion(1, b"secret-domain-b-event");
    let ckpt_a = host_a
        .provider
        .export_local_checkpoint(TEST_NOW_MS + 100)
        .expect("export ckpt A");

    let client_a = SemanticNetSyncClient::new(transport_a, host_a.provider.as_ref());

    // Claim `caller_cell = cell_id(99)` over an mTLS session authenticated as `cell_id(1)`.
    let forged_exchange = SemanticNetSyncRequest::ExchangeVectorCheckpoint {
        caller_cell: cell_id(99),
        caller_checkpoint: ckpt_a,
    };
    let err = client_a
        .send_raw_request(host_b.cell, readonly_contract(0x41), &forged_exchange)
        .await
        .expect_err("forged caller_cell on ExchangeVectorCheckpoint must fail closed");
    assert!(matches!(
        err,
        SemanticNetSyncError::RemoteRejected {
            code: SemanticNetRejectionCode::CallerCellIdentityMismatch,
            ..
        }
    ));

    let forged_pull = SemanticNetSyncRequest::PullDomainEventsBatch {
        caller_cell: cell_id(99),
        authority_ids: [host_b.issuer_binding.control_domain_id.into_bytes()]
            .into_iter()
            .collect(),
        known_event_ids: std::collections::BTreeSet::new(),
        cursor: None,
        max_batch_events: 8,
    };
    let pull_err = client_a
        .send_raw_request(host_b.cell, readonly_contract(0x42), &forged_pull)
        .await
        .expect_err("forged caller_cell on PullDomainEventsBatch must fail closed");
    assert!(matches!(
        pull_err,
        SemanticNetSyncError::RemoteRejected {
            code: SemanticNetRejectionCode::CallerCellIdentityMismatch,
            ..
        }
    ));
    assert_eq!(host_a.event_count(), 0);

    harness.shutdown().await;
}

/// Fault-injecting responder wrapper over a real [`SemanticNetSyncServer`] used
/// to test tampered remote checkpoint signatures, missing provenance elements,
/// and conflicting payloads mid-pagination over live TLS 1.3 mTLS sockets.
#[derive(Clone, Copy)]
enum ResponderFaultMode {
    TamperCheckpointSignature,
    ZeroTrustSnapshotInSecondBatch,
    TamperAdmissionSignatureInSecondBatch,
    TamperProducerSignatureInSecondBatch,
    ConflictingPayloadSameEventIdInSecondBatch,
}

#[derive(Clone)]
struct FaultInjectingSyncResponder {
    inner:
        Arc<Mutex<Option<SemanticNetSyncServer<AuthoritySemanticSyncProvider<TestStoreSigner>>>>>,
    mode: Arc<Mutex<ResponderFaultMode>>,
}

impl FaultInjectingSyncResponder {
    fn set_inner(
        &self,
        server: SemanticNetSyncServer<AuthoritySemanticSyncProvider<TestStoreSigner>>,
    ) {
        *self.inner.lock().expect("lock inner") = Some(server);
    }

    fn set_mode(&self, mode: ResponderFaultMode) {
        *self.mode.lock().expect("lock mode") = mode;
    }
}

impl RpcRequestHandler for FaultInjectingSyncResponder {
    fn handle_rpc(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        let this = self.clone();
        Box::pin(async move {
            let req_decoded = decode_sync_request(&request.payload);
            let mut reply = {
                let guard = this.inner.lock().expect("lock inner");
                let server = guard.as_ref().expect("inner server initialized");
                server.handle_incoming_rpc(&request)
            };

            let mode = *this.mode.lock().expect("lock mode");
            if let Some(decoded_resp) = nlos_semantic::decode_sync_response(&reply.payload) {
                match (mode, req_decoded, decoded_resp) {
                    (
                        ResponderFaultMode::TamperCheckpointSignature,
                        _,
                        SemanticNetSyncResponse::ExchangeVectorCheckpoint {
                            responder_cell,
                            mut responder_checkpoint,
                        },
                    ) => {
                        responder_checkpoint.signature[0] ^= 0xFF;
                        reply.payload = encode_sync_response(
                            &SemanticNetSyncResponse::ExchangeVectorCheckpoint {
                                responder_cell,
                                responder_checkpoint,
                            },
                        );
                    }
                    (
                        fault,
                        Some(SemanticNetSyncRequest::PullDomainEventsBatch {
                            cursor: Some(_), ..
                        }),
                        SemanticNetSyncResponse::PullDomainEventsBatch {
                            responder_cell,
                            mut items,
                            next_cursor,
                            has_more,
                        },
                    ) => {
                        if let Some(first) = items.first_mut() {
                            match fault {
                                ResponderFaultMode::ZeroTrustSnapshotInSecondBatch => {
                                    first.trust_snapshot_digest = [0_u8; 32];
                                }
                                ResponderFaultMode::TamperAdmissionSignatureInSecondBatch => {
                                    first.admission.store_signature[0] ^= 0xAA;
                                }
                                ResponderFaultMode::TamperProducerSignatureInSecondBatch => {
                                    first.producer_signature[0] ^= 0x55;
                                }
                                ResponderFaultMode::ConflictingPayloadSameEventIdInSecondBatch => {
                                    let mut conflicting: CrossDomainEventItem = first.clone();
                                    conflicting.producer_signature[7] ^= 0xFF;
                                    items.push(conflicting);
                                }
                                ResponderFaultMode::TamperCheckpointSignature => {}
                            }
                        }
                        reply.payload =
                            encode_sync_response(&SemanticNetSyncResponse::PullDomainEventsBatch {
                                responder_cell,
                                items,
                                next_cursor,
                                has_more,
                            });
                    }
                    _ => {}
                }
            }
            reply
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn tampered_checkpoint_missing_provenance_and_conflicting_payload_fail_closed_with_zero_mutation()
 {
    let fault_responder = Arc::new(FaultInjectingSyncResponder {
        inner: Arc::new(Mutex::new(None)),
        mode: Arc::new(Mutex::new(ResponderFaultMode::TamperCheckpointSignature)),
    });
    let harness = TwoHostSemanticHarness::setup(
        "negative-gates",
        Some(Arc::clone(&fault_responder) as Arc<dyn RpcRequestHandler>),
    )
    .await;
    let host_a = harness.host_a.as_ref().expect("host A");
    let host_b = harness.host_b.as_ref().expect("host B");
    let transport_a = harness.transport_a.as_ref().expect("transport A");

    fault_responder.set_inner(SemanticNetSyncServer::new(
        Arc::clone(&host_b.provider),
        TEST_NOW_MS + 500,
    ));

    // Seed 1 baseline event on Host A and 3 events on Host B so multi-batch pull
    // (`max_batch_events = 2`) has 2 valid events in batch 1 and 1 event in batch 2!
    host_a.append_assertion(1, b"host-a-baseline");
    for idx in 1..=3_u8 {
        host_b.append_assertion(idx + 30, format!("host-b-item-{idx}").as_bytes());
    }
    assert_eq!(host_a.event_count(), 1);

    let client_a = SemanticNetSyncClient::new(transport_a, host_a.provider.as_ref());

    // (1a) Caller sends tampered checkpoint signature to Host B -> rejected by responder.
    let mut tampered_caller_ckpt = host_a
        .provider
        .export_local_checkpoint(TEST_NOW_MS + 100)
        .expect("export caller ckpt");
    tampered_caller_ckpt.signature[3] ^= 0xFF;
    let caller_tamper_err = client_a
        .send_raw_request(
            host_b.cell,
            readonly_contract(0x61),
            &SemanticNetSyncRequest::ExchangeVectorCheckpoint {
                caller_cell: host_a.cell,
                caller_checkpoint: tampered_caller_ckpt,
            },
        )
        .await
        .expect_err("tampered caller checkpoint must be rejected by server");
    assert!(matches!(
        caller_tamper_err,
        SemanticNetSyncError::RemoteRejected {
            code: SemanticNetRejectionCode::InvalidCallerCheckpoint,
            ..
        }
    ));

    // (1b) Responder returns tampered checkpoint signature -> rejected by client before pull.
    fault_responder.set_mode(ResponderFaultMode::TamperCheckpointSignature);
    let remote_tamper_err = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 2,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 200,
                base_idempotency_key: IdempotencyKey::from_bytes([0x62; 16]),
            },
        )
        .await
        .expect_err("tampered remote checkpoint signature must fail closed");
    assert!(matches!(
        remote_tamper_err,
        SemanticNetSyncError::InvalidRemoteCheckpoint(
            SemanticVectorCheckpointError::InvalidSignature
        )
    ));
    assert_eq!(host_a.event_count(), 1);

    // (2a) Zero trust_snapshot_digest in batch 2 after valid batch 1 -> zero partial SQLite writes!
    fault_responder.set_mode(ResponderFaultMode::ZeroTrustSnapshotInSecondBatch);
    let zero_trust_err = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 2,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 300,
                base_idempotency_key: IdempotencyKey::from_bytes([0x63; 16]),
            },
        )
        .await
        .expect_err("zero trust_snapshot_digest must fail closed");
    assert!(matches!(
        zero_trust_err,
        SemanticNetSyncError::Authority(SemanticAuthorityError::VectorCheckpoint(
            SemanticVectorCheckpointError::MissingTrustSnapshot
        ))
    ));
    assert_eq!(
        host_a.event_count(),
        1,
        "batch 1 valid items must NOT partially mutate SQLite when batch 2 fails"
    );

    // (2b) Tampered source AdmissionReceipt signature in batch 2 -> zero partial SQLite writes!
    fault_responder.set_mode(ResponderFaultMode::TamperAdmissionSignatureInSecondBatch);
    let bad_adm_err = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 2,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 400,
                base_idempotency_key: IdempotencyKey::from_bytes([0x64; 16]),
            },
        )
        .await
        .expect_err("tampered AdmissionReceipt signature must fail closed");
    assert!(matches!(
        bad_adm_err,
        SemanticNetSyncError::InvalidProvenanceReceipt { .. }
    ));
    assert_eq!(host_a.event_count(), 1);

    // (2c) Tampered original producer signature in batch 2 -> zero partial SQLite writes!
    fault_responder.set_mode(ResponderFaultMode::TamperProducerSignatureInSecondBatch);
    let bad_prod_err = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 2,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 500,
                base_idempotency_key: IdempotencyKey::from_bytes([0x65; 16]),
            },
        )
        .await
        .expect_err("tampered producer signature must fail closed");
    assert!(matches!(
        bad_prod_err,
        SemanticNetSyncError::InvalidProvenanceReceipt { .. }
    ));
    assert_eq!(host_a.event_count(), 1);

    // (3) Conflicting event under same SemanticEventId in batch 2 -> zero partial SQLite writes!
    // Note: increase max_batch_events allowance on client to 3 while responder returns the extra
    // conflicting duplicate in batch 2, or test conflicting local vs remote payload directly!
    fault_responder.set_mode(ResponderFaultMode::ConflictingPayloadSameEventIdInSecondBatch);
    let conflict_err = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 2,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 600,
                base_idempotency_key: IdempotencyKey::from_bytes([0x66; 16]),
            },
        )
        .await
        .expect_err("conflicting event under same SemanticEventId must fail closed");
    // Notice in batch 2 (which originally had 1 item, +1 injected duplicate with same event_id!),
    // either monotonic `(authority_id, event_id)` check or `CrossDomainMergeRejected` catches it!
    // Let's also verify `import_remote_cross_domain_events` directly with a conflicting payload
    // under the same `SemanticEventId` so `CrossDomainMergeRejected { conflicts }` is explicitly proven!
    assert!(matches!(
        conflict_err,
        SemanticNetSyncError::PaginationProtocolViolation(_)
            | SemanticNetSyncError::Authority(
                SemanticAuthorityError::CrossDomainMergeRejected { .. }
            )
    ));
    assert_eq!(host_a.event_count(), 1);

    // (3b) Mutated canonical_unsigned_event under same SemanticEventId -> InvalidProvenanceReceipt!
    let mut mutated_items = host_b
        .provider
        .export_local_cross_domain_events()
        .expect("export Host B items");
    mutated_items[0].event.canonical_unsigned_event.push(0x00);
    let mutated_err = host_a
        .provider
        .import_remote_cross_domain_events(&mutated_items)
        .expect_err("mutated canonical event under same SemanticEventId must fail closed");
    assert!(matches!(
        mutated_err,
        SemanticNetSyncError::InvalidProvenanceReceipt { .. }
    ));
    assert_eq!(host_a.event_count(), 1);

    // (3c) Two individually provenance-valid observations for the same SemanticEventId / receipt_id
    // with conflicting trust_snapshot_digest -> CrossDomainMergeRejected { conflicts } with 0 writes!
    let mut conflicting_valid_items = host_b
        .provider
        .export_local_cross_domain_events()
        .expect("export Host B items");
    let mut divergent_obs = conflicting_valid_items[0].clone();
    divergent_obs.trust_snapshot_digest = [0xEE; 32];
    conflicting_valid_items.push(divergent_obs);
    let merge_conflict = host_a
        .provider
        .import_remote_cross_domain_events(&conflicting_valid_items)
        .expect_err("conflicting valid observations must fail closed with zero SQLite writes");
    assert!(matches!(
        merge_conflict,
        SemanticNetSyncError::Authority(SemanticAuthorityError::CrossDomainMergeRejected {
            ref conflicts,
            ..
        }) if !conflicts.is_empty()
    ));
    assert_eq!(host_a.event_count(), 1);

    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn revoked_signer_key_causal_local_ahead_and_epoch_regression_fail_closed_over_mtls() {
    let harness = TwoHostSemanticHarness::setup("revoked-and-epoch-regression", None).await;
    let host_a = harness.host_a.as_ref().expect("host A");
    let host_b = harness.host_b.as_ref().expect("host B");
    let transport_a = harness.transport_a.as_ref().expect("transport A");
    let listener_b_addr = harness
        .listener_b
        .as_ref()
        .expect("listener B")
        .local_addr();

    // 1. Seed 2 events on Host B, sync them into Host A (`Equal`), then append 2 new
    //    local events on Host A so Host A is causally `LocalAhead` of Host B.
    host_b.append_assertion(1, b"host-b-seed-1");
    host_b.append_assertion(2, b"host-b-seed-2");

    let client_a = SemanticNetSyncClient::new(transport_a, host_a.provider.as_ref());
    let initial_sync = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 4,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 100,
                base_idempotency_key: IdempotencyKey::from_bytes([0x71; 16]),
            },
        )
        .await
        .expect("initial sync from Host B");
    assert_eq!(initial_sync.pulled_event_count, 2);
    assert_eq!(host_a.event_count(), 2);

    host_a.append_assertion(10, b"host-a-ahead-1");
    host_a.append_assertion(11, b"host-a-ahead-2");
    assert_eq!(host_a.event_count(), 4);

    // Causal non-regression: Host B's checkpoint is behind Host A (`LocalAhead`);
    // Host A pulls 0 batches and never rolls back or loses its local events.
    let local_ahead_report = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 4,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 200,
                base_idempotency_key: IdempotencyKey::from_bytes([0x72; 16]),
            },
        )
        .await
        .expect("LocalAhead sync check must succeed without regression");
    assert_eq!(
        local_ahead_report.pre_sync_diff.relation,
        VectorCheckpointRelation::LocalAhead
    );
    assert_eq!(local_ahead_report.batches_pulled, 0);
    assert_eq!(local_ahead_report.pulled_event_count, 0);
    assert!(local_ahead_report.convergence.is_none());
    assert_eq!(host_a.event_count(), 4);

    // 2. Revoked producer signing key fail-closed: Host B appends a new event, while
    //    Host A revokes Host B's producer signing key in Host A's `IdentityAuthority`.
    host_b.append_assertion(50, b"host-b-post-revocation-event");
    host_a
        .identity
        .revoke_key(RevokeKeyRequest {
            key_id: host_b.issuer_binding.key_id,
            expected_key_generation: host_b.issuer_binding.key_generation,
            expected_identity_snapshot_id: host_b.issuer_binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0x73; 16]),
            revoked_at_ms: TEST_NOW_MS,
        })
        .expect("revoke Host B producer key on Host A");

    let revoked_producer_err = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 4,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 300,
                base_idempotency_key: IdempotencyKey::from_bytes([0x74; 16]),
            },
        )
        .await
        .expect_err("revoked producer key must fail closed during 3-part provenance check");
    assert!(matches!(
        revoked_producer_err,
        SemanticNetSyncError::InvalidProvenanceReceipt { .. }
    ));
    assert_eq!(
        host_a.event_count(),
        4,
        "revoked producer key must leave zero partial writes on Host A"
    );

    // 3. Revoked remote checkpoint store key fail-closed: Host A revokes Host B's
    //    store checkpoint signing key; checkpoint exchange immediately fails closed!
    host_a
        .identity
        .revoke_key(RevokeKeyRequest {
            key_id: host_b.store_signer.binding.key_id,
            expected_key_generation: host_b.store_signer.binding.key_generation,
            expected_identity_snapshot_id: host_b.store_signer.binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0x75; 16]),
            revoked_at_ms: TEST_NOW_MS,
        })
        .expect("revoke Host B store checkpoint key on Host A");

    let revoked_store_err = client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 4,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 400,
                base_idempotency_key: IdempotencyKey::from_bytes([0x76; 16]),
            },
        )
        .await
        .expect_err("revoked remote store checkpoint key must fail closed");
    assert!(matches!(
        revoked_store_err,
        SemanticNetSyncError::InvalidRemoteCheckpoint(
            SemanticVectorCheckpointError::InvalidSignature
                | SemanticVectorCheckpointError::SignerBindingMismatch
        )
    ));
    assert_eq!(host_a.event_count(), 4);

    // 4. Peer `CellEpoch` regression fail-closed over mTLS (`[TCB-ENDPOINT-001]`):
    //    Advance Host A's expected `EndpointRoutingVersion` for Host B to `CellEpoch = 2`
    //    while Host B's TLS 1.3 Exporter `CellFence` still presents `CellEpoch::INITIAL` (`1`).
    let advanced_epoch = CellEpoch::from_u64(2).expect("epoch 2");
    let expected_b_principal = harness
        .resolver
        .resolve(host_b.cell)
        .and_then(|h| h.expected_principal)
        .expect("pinned Host B principal");
    harness
        .resolver
        .upsert_hint(
            EndpointHintRecord::new(
                host_b.cell,
                listener_b_addr,
                EndpointRoutingVersion::new(
                    Generation::new(NonZeroU64::new(1).expect("boot gen 1")),
                    advanced_epoch,
                    2,
                ),
                TEST_NOW_MS + 500,
            )
            .with_expected_principal(expected_b_principal),
        )
        .expect("advance expected CellEpoch for Host B");

    let dyn_resolver: Arc<dyn EndpointResolver> =
        Arc::clone(&harness.resolver) as Arc<dyn EndpointResolver>;
    let fresh_transport_a = MtlsCellTransport::new_multi_host(
        ClusterProfile::MultiHostCluster {
            local_cell: host_a.cell,
            bind_hint: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        },
        dyn_resolver,
        host_a.security.clone(),
        NetTransportConfig::default(),
        SocketActivityAccounting::new(),
        None,
        None,
    )
    .expect("build fresh mTLS client transport A for epoch regression check");
    let fresh_client_a = SemanticNetSyncClient::new(&fresh_transport_a, host_a.provider.as_ref());

    let epoch_regressed_err = fresh_client_a
        .pull_and_merge_from_peer(
            host_b.cell,
            SemanticNetSyncConfig {
                max_batch_events: 4,
                rpc_deadline_ms: 5_000,
                signed_at_ms: TEST_NOW_MS + 600,
                base_idempotency_key: IdempotencyKey::from_bytes([0x77; 16]),
            },
        )
        .await
        .expect_err("peer presenting regressed CellEpoch must be rejected by mTLS fence gate");
    assert!(matches!(
        epoch_regressed_err,
        SemanticNetSyncError::TransportFailure(_)
    ));
    assert_eq!(host_a.event_count(), 4);

    drop(fresh_transport_a);
    harness.shutdown().await;
}
