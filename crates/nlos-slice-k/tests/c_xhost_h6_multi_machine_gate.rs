//! `ROAD-C-001-XHOST` H6 Multi-Machine Chaos Verification Gate (`C-XHOST-GATE`,
//! `ADR-0026` §2.3, §2.4, §5.3, §6.3).
//!
//! Exercises the 5 mandatory cross-host chaos scenarios over `nlos-net`
//! [`ChaosTcpProxy`] / [`ChaosDirectedMesh`], `nlos-consensus` `N = 3` Raft
//! quorum, `nlos-takeover-control` [`CommittedControlIndexGate`], `nlos-lease`
//! [`clock_horizon`], `nlos-artifact` resumable chunked streaming, and
//! `nlos-slice-k` 8-step process migration + bounded-loss partition recovery.

#![allow(clippy::too_many_lines, clippy::similar_names)]

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use nlos_artifact::net_transfer::ArtifactNetTransferClient;
use nlos_artifact::{
    ArtifactConflictStrategy, ArtifactDeletionSemantics, ArtifactMergeStrategy,
    ArtifactReplicaAuthority, ArtifactSyncPolicy, ContentDigest, ExportArtifactTransferRequest,
    export_artifact_transfer_package,
};
use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodyTier, KeyPurpose as CustodyKeyPurpose,
    SigningKeyCustodyVault,
};
use nlos_capability::{
    CapabilityRights, CapabilityTarget, DelegateCapabilityRequest, IssueRootCapabilityRequest,
    RevokeCapabilityRequest, SignedDelegateCapabilityRequest, SignedIssueRootCapabilityRequest,
    SignedRevokeCapabilityRequest, delegate_command_message, issue_root_command_message,
    revoke_command_message,
};
use nlos_cell::{
    CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity, FailureDetectorConfig,
    NamePath, RemoteCallContract, RemoteCallOutcome, RemoteRetryRefusal,
};
use nlos_clock::AuthorityClock;
use nlos_consensus::{
    ConsensusRole, ControlPlaneCommand, ControlPlaneProposal, MultiHostQuorumConfig,
    MultiHostQuorumNode, TaskAuthorityShardCommand, ValidatedVoterRoster, VoterReplica,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose, semantic_signature_message,
};
use nlos_lease::clock_horizon::{
    AuthorityClockHorizonGuard, ClockSkewPolicy, CrossHostGrantParams, CrossHostLeaseFamily,
    GrantorClockBoundedCoordinator, GrantorHorizonStatus, HolderClockBoundedLease,
    HolderHorizonState, MonotonicInstantMs, MultiEndpointFenceScope, PhysicalCleanupProofKind,
    QuarantineDisposition, QuarantinedClockViolation,
};
use nlos_lease::{CapacityLeaseState, CleanupAttestation, FenceBarrierAck, QuotaLeaseState};
use nlos_net::{
    AuthorityCustodyPeerVerifier, CellRpcTransport, ChaosDirectedMesh, ChaosLinkPolicy,
    ChaosNodeDescriptor, ChaosTcpProxy, CiphertextCorruptionRule, CustodyPeerSigner,
    EndpointHintRecord, EndpointResolver, EndpointRoutingVersion, FenceNonceRegistry,
    MonotonicEndpointResolver, MonotonicPeerFenceGate, NetTransportConfig, NodeSecurityDeps,
    NodeTlsCredentials, PendingReconciliationTracker, SocketFailureKind,
};
use nlos_process::{
    CreateIsolationDomainRequest, DriverSessionAttachment, PinnedSessionPolicy,
    ProcessMigrationStep, RegisterDelegatedProcessRequest,
};
use nlos_semantic::net_sync::{SemanticNetSyncClient, SemanticNetSyncConfig};
use nlos_semantic::{
    AppendAssertionRequest, AssertionMode, LocalProcessRef, TaintFlags, UnsignedAssertionEvent,
    content_digest, encode_unsigned_assertion_event, semantic_event_id,
};
use nlos_slice_k::{
    CapabilityRebindItemSpec, CellHost, CellHostConfig, CellHostProcessMigrationRequest,
    CellPartitionEndpoint, CheckpointSourceSpec, CrossSocketMigrationError,
    CrossSocketProcessMigrationOptions, DEFAULT_NET_CELL_TRUST_SNAPSHOT_DIGEST,
    LeaseMigrationBundleSpec, LocalCellHostPartitionEndpoint, NetCellHost,
    NetCellHostNetworkConfig, NetConsensusBackend, PartitionBoundedLossProof,
    ProcessLineageGenesis, ProcessLineageSpec, ProcessMigrationDriveError, QuotaLeaseMigrationSpec,
    QuotaPartitionSettlementSpec, SourceFenceSpec, SourceQuiesceSpec, TargetRestoreSpec,
    drive_cross_socket_process_migration, exchange_peer_bootstrap_over_mtls,
    publish_restart_image_artifact,
};
use nlos_takeover_control::{
    AdmittedCommitDisposition, CommittedControlIndexGate, ConsensusBackingMode,
    ControlCommitSubject, QuorumCommitAttestation, VoterCommitAttestation,
    quorum_commit_attestation_digest,
};
use nlos_types::{
    ArtifactId, CapacityLeaseId, CommitPermitId, ControlDomainId, DeviceId, ExecutionFiberId,
    Generation, IdempotencyKey, NamespaceId, PrincipalId, ProcessId, QuotaLeaseId, ReceiptId,
    SchedulerDomainId, TaskAttemptId, TaskAuthorityAssignmentId, TaskId,
};

const PKI_VALID_FROM_MS: u64 = 1_700_000_000_000;
const PKI_CLOCK_MS: u64 = 1_800_000_000_000;
const PKI_VALID_UNTIL_MS: u64 = 4_102_444_800_000;
const SHARED_DEVICE_BYTES: [u8; 16] = [0xD4; 16];
const ARTIFACT_CONTENT_TYPE: &str = "application/vnd.llmos.process-restart-image+v1";
const SEMANTIC_ISSUER_SEED: u8 = 0x57;
const ENV_XHOST_CHILD_ROOT: &str = "LLMOS_W72_XHOST_CHILD_ROOT";
const ENV_XHOST_CHILD_NODE: &str = "LLMOS_W72_XHOST_CHILD_NODE";
const ENV_XHOST_CHILD_CLUSTER: &str = "LLMOS_W72_XHOST_CHILD_CLUSTER";
const ENV_XHOST_ISOLATED_TEST: &str = "LLMOS_W72_XHOST_ISOLATED_TEST";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("nlos-w72-h6-{label}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create isolated temp dir");
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

fn domain_for_node(node_idx: u8) -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([node_idx.wrapping_add(0xA0); 16])
}

fn cell_for_node(node_idx: u8) -> CellIdentity {
    CellIdentity::from_domain(domain_for_node(node_idx))
}

fn control_domain_for_node(node_idx: u8) -> ControlDomainId {
    ControlDomainId::from_bytes([node_idx.wrapping_add(0xC0); 16])
}

fn loopback_ephemeral() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

fn detector_config() -> FailureDetectorConfig {
    FailureDetectorConfig::new(
        NonZeroU64::new(3).expect("suspect"),
        NonZeroU64::new(5).expect("dead"),
    )
    .expect("detector thresholds")
}

fn cell_host_config(node_idx: u8) -> CellHostConfig {
    CellHostConfig::new(
        domain_for_node(node_idx),
        1_000,
        500,
        DeviceId::from_bytes(SHARED_DEVICE_BYTES),
        detector_config(),
    )
}

fn skew_policy() -> ClockSkewPolicy {
    ClockSkewPolicy::new(
        NonZeroU64::new(500).expect("max_clock_skew_ms"),
        NonZeroU64::new(250).expect("safety_margin_ms"),
    )
}

fn restart_artifact_policy() -> ArtifactSyncPolicy {
    ArtifactSyncPolicy {
        content_type: ARTIFACT_CONTENT_TYPE.to_owned(),
        merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
        conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
        replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
        deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
    }
}
fn semantic_issuer_bootstrap() -> (SigningKey, BootstrapPrincipalRequest) {
    let sk = SigningKey::from_bytes(&[SEMANTIC_ISSUER_SEED; 32]);
    let req = BootstrapPrincipalRequest {
        principal_profile_digest: [SEMANTIC_ISSUER_SEED.wrapping_add(1); 32],
        control_domain_policy_digest: [SEMANTIC_ISSUER_SEED.wrapping_add(2); 32],
        public_key: sk.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: PKI_VALID_FROM_MS,
        key_valid_until_ms: PKI_VALID_UNTIL_MS,
        idempotency_key: IdempotencyKey::from_bytes([SEMANTIC_ISSUER_SEED.wrapping_add(3); 16]),
        created_at_ms: PKI_VALID_FROM_MS,
    };
    (sk, req)
}

fn provision_isolated_security(
    host_root: &Path,
    subdir: &str,
    node_idx: u8,
    cluster_size: u8,
    live_host_slot: Arc<OnceLock<Weak<Mutex<CellHost>>>>,
) -> (NodeSecurityDeps, PrincipalId) {
    let sec_dir = host_root.join(subdir);
    let id_dir = sec_dir.join("identity");
    let custody_dir = sec_dir.join("custody");
    std::fs::create_dir_all(&id_dir).expect("create identity dir");
    std::fs::create_dir_all(&custody_dir).expect("create custody dir");

    let identity = Arc::new(IdentityAuthority::open(&id_dir).expect("open identity"));
    let vault = Arc::new(
        SigningKeyCustodyVault::open_durable(&custody_dir, [node_idx.wrapping_add(0x90); 32])
            .expect("open custody vault"),
    );
    let fence_gate = Arc::new(MonotonicPeerFenceGate::new(None));
    let mut my_binding = None;

    let start_idx = if cluster_size <= 3 {
        1
    } else {
        cluster_size.saturating_sub(1)
    };
    for peer_idx in start_idx..=cluster_size {
        let seed = [peer_idx.wrapping_add(0x30); 32];
        let profile_digest = [peer_idx.wrapping_add(0x10); 32];
        let policy_digest = [peer_idx.wrapping_add(0x20); 32];
        let boot_idem = IdempotencyKey::from_bytes([peer_idx.wrapping_add(0x40); 16]);

        let binding = if peer_idx == node_idx {
            let (_, b) = vault
                .bootstrap_principal_under_custody(
                    &identity,
                    BootstrapCustodyPrincipalRequest {
                        authority_id: [node_idx; 16],
                        principal_profile_digest: profile_digest,
                        control_domain_policy_digest: policy_digest,
                        key_purpose: CustodyKeyPurpose::SemanticSigning,
                        custody_tier: CustodyTier::OperatorFileSealed,
                        valid_from_ms: PKI_VALID_FROM_MS,
                        valid_until_ms: PKI_VALID_UNTIL_MS,
                        seed,
                        bootstrap_idempotency_key: boot_idem,
                        custody_binding_idempotency_key: IdempotencyKey::from_bytes(
                            [node_idx.wrapping_add(0x50); 16],
                        ),
                        provisioned_at_ms: PKI_VALID_FROM_MS,
                    },
                )
                .expect("bootstrap custody principal");
            my_binding = Some(b);
            b
        } else {
            let public_key = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
            identity
                .bootstrap_principal(BootstrapPrincipalRequest {
                    principal_profile_digest: profile_digest,
                    control_domain_policy_digest: policy_digest,
                    public_key,
                    key_purpose: KeyPurpose::SemanticSigning,
                    key_valid_from_ms: PKI_VALID_FROM_MS,
                    key_valid_until_ms: PKI_VALID_UNTIL_MS,
                    idempotency_key: boot_idem,
                    created_at_ms: PKI_VALID_FROM_MS,
                })
                .expect("bootstrap remote peer")
                .binding()
        };

        fence_gate
            .pin_cell_principal(cell_for_node(peer_idx), binding.principal_id)
            .expect("pin peer CellIdentity <-> PrincipalId");
    }

    let me = my_binding.expect("local binding");
    let signer = Arc::new(CustodyPeerSigner::new(
        Arc::clone(&vault),
        me.principal_id,
        me.key_id,
        me.key_generation,
    ));
    let tls_credentials = NodeTlsCredentials::issue(
        signer.as_ref(),
        [node_idx.wrapping_add(0x61); 32],
        PKI_VALID_FROM_MS,
        PKI_VALID_UNTIL_MS,
    )
    .expect("issue node X.509v3 mTLS credentials");

    let fallback_cell = cell_for_node(node_idx);
    let local_fence: Arc<dyn Fn() -> CellFence + Send + Sync> = Arc::new(move || {
        if let Some(weak) = live_host_slot.get()
            && let Some(arc) = weak.upgrade()
        {
            return arc.lock().unwrap_or_else(PoisonError::into_inner).fence();
        }
        CellFence::present(
            fallback_cell,
            Generation::INITIAL,
            CellEpoch::INITIAL,
            CellFencingToken::INITIAL,
        )
    });

    let deps = NodeSecurityDeps {
        local_fence,
        local_signer: signer,
        peer_verifier: Arc::new(AuthorityCustodyPeerVerifier::new(identity, vec![vault])),
        fence_policy: fence_gate,
        nonce_registry: Arc::new(FenceNonceRegistry::new(512)),
        clock_ms: Arc::new(|| PKI_CLOCK_MS),
        tls_credentials,
    };
    (deps, me.principal_id)
}

async fn bind_isolated_networked_host(
    host_root: &Path,
    node_idx: u8,
    cluster_size: u8,
) -> NetCellHost {
    let live_slot = Arc::new(OnceLock::new());
    let (listener_security, _) = provision_isolated_security(
        host_root,
        "transport-listener",
        node_idx,
        cluster_size,
        Arc::clone(&live_slot),
    );
    let (client_security, _) = provision_isolated_security(
        host_root,
        "transport-client",
        node_idx,
        cluster_size,
        Arc::clone(&live_slot),
    );
    let single_node = nlos_consensus::SingleHostPassthroughNode::open(
        host_root.join("single-consensus"),
        cell_for_node(node_idx),
        control_domain_for_node(node_idx),
    )
    .expect("open single-node consensus");

    let host = NetCellHost::bind_networked(
        host_root,
        NetCellHostNetworkConfig {
            cell_config: cell_host_config(node_idx),
            cell_label: format!("w72-host-{node_idx}"),
            service_bind_addr: loopback_ephemeral(),
            listener_security,
            client_security,
            transport_config: NetTransportConfig::default(),
            skew_policy: skew_policy(),
            trust_snapshot_digest: DEFAULT_NET_CELL_TRUST_SNAPSHOT_DIGEST,
            artifact_policies: vec![restart_artifact_policy()],
            remote_lease_server: None,
            consensus: NetConsensusBackend::SingleHostPassthrough(Box::new(single_node)),
        },
    )
    .await
    .expect("bind isolated NetCellHost");

    let _ = live_slot.set(Arc::downgrade(host.cell_host_arc()));
    host
}
fn append_semantic_event_on_host(host: &NetCellHost, admitted_at_ms: u64) {
    let (issuer_sk, issuer_boot) = semantic_issuer_bootstrap();
    host.with_cell_host(|ch| {
        let rt = ch.runtime();
        let issuer_b = rt
            .identity
            .bootstrap_principal(issuer_boot)
            .expect("bootstrap semantic issuer")
            .binding();
        let iso = rt
            .process
            .create_isolation_domain(CreateIsolationDomainRequest {
                policy_digest: [0x5B; 32],
                idempotency_key: IdempotencyKey::from_bytes([0x5C; 16]),
                created_at_ms: admitted_at_ms - 500,
            })
            .expect("create iso domain")
            .record()
            .clone();
        let proc_rec = rt
            .process
            .register_delegated_process(RegisterDelegatedProcessRequest {
                task_id: TaskId::from_bytes([0x5D; 16]),
                task_attempt_id: TaskAttemptId::from_bytes([0x5E; 16]),
                attempt_generation: Generation::INITIAL,
                isolation_domain_id: iso.isolation_domain_id,
                isolation_domain_generation: iso.generation,
                isolation_domain_fencing_token: iso.fencing_token,
                idempotency_key: IdempotencyKey::from_bytes([0x5F; 16]),
                created_at_ms: admitted_at_ms - 400,
            })
            .expect("register delegated process")
            .record()
            .clone();

        let cap_cmd = IssueRootCapabilityRequest {
            issuer_key_id: issuer_b.key_id,
            holder_key_id: issuer_b.key_id,
            target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x59; 16])),
            rights: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some([0x5A; 32]),
            valid_from_ms: 0,
            valid_until_ms: PKI_VALID_UNTIL_MS,
            delegation_depth_remaining: 0,
            call_limit: None,
            idempotency_key: IdempotencyKey::from_bytes([0x60; 16]),
            issued_at_ms: admitted_at_ms - 300,
        };
        let signed_cap = SignedIssueRootCapabilityRequest {
            command: cap_cmd,
            signer: issuer_b.principal_id,
            signature: issuer_sk
                .sign(&issue_root_command_message(cap_cmd))
                .to_bytes(),
        };
        let cap_rec = rt
            .capability()
            .issue_root_signed(&rt.identity, signed_cap)
            .expect("issue semantic cap")
            .record();

        let media_type = "application/vnd.llmos.xhost-h6-fact+v1".to_owned();
        let body = b"w72-xhost-h6-semantic-payload-v1";
        let ev = UnsignedAssertionEvent {
            scope: cap_rec.target,
            issuer: issuer_b.principal_id,
            issuer_execution: LocalProcessRef {
                process_id: proc_rec.process_id,
                generation: proc_rec.process_generation,
            },
            control_domain: issuer_b.control_domain_id,
            issued_at_unix_ns: admitted_at_ms * 1_000_000,
            nonce: vec![0xE1; 16],
            declared_parents: Vec::new(),
            declassification_receipt_id: None,
            valid_until_ms: Some(PKI_VALID_UNTIL_MS),
            purpose_digest: cap_rec.purpose_digest,
            content_digest: content_digest(&media_type, body).expect("digest"),
            assertion_mode: AssertionMode::Inference,
            execution_evidence_receipt_id: None,
            confidence_bp: Some(9_000),
            key_id: issuer_b.key_id,
        };
        let cbor = encode_unsigned_assertion_event(&ev).expect("encode ev");
        let id = semantic_event_id(&cbor);
        let sig = issuer_sk.sign(&semantic_signature_message(id)).to_bytes();
        rt.semantic()
            .append_assertion(
                &rt.identity,
                rt.capability(),
                &rt.process,
                rt.semantic_writer(),
                &AppendAssertionRequest {
                    canonical_unsigned_event: cbor,
                    claimed_event_id: id,
                    signature: sig,
                    capability: cap_rec.handle,
                    content_media_type: media_type,
                    content_bytes: body.to_vec(),
                    captured_inputs: Vec::new(),
                    ingress_taint: TaintFlags::default(),
                    authz_policy_digest: [0x98; 32],
                    admission_limit_ms: Some(PKI_VALID_UNTIL_MS - 1_000),
                    admitted_at_ms,
                },
            )
            .expect("append semantic assertion");
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn road_c001_xhost_asymmetric_triangle_partition_and_raft_quorum_fencing() {
    if !is_isolated_test_subprocess() {
        run_test_in_isolated_subprocess(
            "road_c001_xhost_asymmetric_triangle_partition_and_raft_quorum_fencing",
            false,
        );
        return;
    }

    let dir1 = TempDir::new("raft-n1");
    let dir2 = TempDir::new("raft-n2");
    let dir3 = TempDir::new("raft-n3");
    let gate_dir = TempDir::new("raft-gate-id");
    let dirs = [&dir1, &dir2, &dir3];

    let c1 = cell_for_node(1);
    let c2 = cell_for_node(2);
    let c3 = cell_for_node(3);
    let cells = [c1, c2, c3];
    let roster = ValidatedVoterRoster::new(vec![
        VoterReplica {
            cell_identity: c1,
            control_domain_id: control_domain_for_node(1),
        },
        VoterReplica {
            cell_identity: c2,
            control_domain_id: control_domain_for_node(2),
        },
        VoterReplica {
            cell_identity: c3,
            control_domain_id: control_domain_for_node(3),
        },
    ])
    .expect("valid N=3 roster");

    let mut nodes = Vec::with_capacity(3);
    let mut resolvers = Vec::with_capacity(3);
    let mut principals = Vec::with_capacity(3);

    for (idx, root_dir) in dirs.iter().enumerate() {
        let node_idx = u8::try_from(idx + 1).expect("node_idx");
        let slot = Arc::new(OnceLock::new());
        let (sec, principal_id) =
            provision_isolated_security(root_dir.path(), "raft-sec", node_idx, 3, slot);
        let res = Arc::new(MonotonicEndpointResolver::new());
        let mut cfg = MultiHostQuorumConfig::new(
            cell_for_node(node_idx),
            loopback_ephemeral(),
            roster.clone(),
        );
        cfg.tick_interval = Duration::from_millis(25);
        let node = MultiHostQuorumNode::bind_and_start(
            root_dir.path().join("raft-store"),
            cfg,
            Arc::clone(&res) as Arc<dyn EndpointResolver>,
            sec,
        )
        .await
        .expect("start MultiHostQuorumNode");
        principals.push(principal_id);
        resolvers.push(res);
        nodes.push(node);
    }

    let ver1 = EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 1);
    let descriptors = vec![
        ChaosNodeDescriptor {
            cell_identity: c1,
            real_listen_addr: nodes[0].local_addr(),
            expected_principal: Some(principals[0]),
            routing_version: ver1,
        },
        ChaosNodeDescriptor {
            cell_identity: c2,
            real_listen_addr: nodes[1].local_addr(),
            expected_principal: Some(principals[1]),
            routing_version: ver1,
        },
        ChaosNodeDescriptor {
            cell_identity: c3,
            real_listen_addr: nodes[2].local_addr(),
            expected_principal: Some(principals[2]),
            routing_version: ver1,
        },
    ];
    let mesh = ChaosDirectedMesh::bind(&descriptors, 0x7201_A5A5_0000_0001)
        .await
        .expect("bind ChaosDirectedMesh");

    for from_idx in 0..3 {
        for to_idx in 0..3 {
            if from_idx == to_idx {
                continue;
            }
            let proxy_addr = mesh
                .link_proxy_addr(cells[from_idx], cells[to_idx])
                .expect("proxy link addr");
            resolvers[from_idx]
                .upsert_hint(
                    EndpointHintRecord::new(cells[to_idx], proxy_addr, ver1, 1)
                        .with_expected_principal(principals[to_idx]),
                )
                .expect("route Raft peer through ChaosDirectedMesh");
        }
    }
    // Wait for initial N=3 leader election across ChaosDirectedMesh.
    let mut initial_leader_idx = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Some(i) = nodes.iter().position(|n| n.role() == ConsensusRole::Leader) {
            initial_leader_idx = Some(i);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let leader_1_idx = initial_leader_idx.expect("N=3 cluster must elect initial leader");
    let term_1 = nodes[leader_1_idx].current_term();
    assert!(term_1 >= 1);

    let task_id = TaskId::from_bytes([0xD1; 16]);
    let assign_1_id = TaskAuthorityAssignmentId::from_bytes([0xA1; 16]);
    let prop_1 = ControlPlaneProposal::new(
        IdempotencyKey::from_bytes([0x11; 16]),
        ControlPlaneCommand::TaskAuthorityShardMutation(
            TaskAuthorityShardCommand::AssignInitialAuthority {
                assignment_id: assign_1_id,
                task_id,
                task_generation: Generation::INITIAL,
                authority_shard: *task_id.as_bytes(),
                home_cell: cells[leader_1_idx],
                authority_term: NonZeroU64::MIN,
                control_epoch: 1,
                participant_registry_generation: 1,
                participant_registry_root: [0x31; 32],
            },
        ),
    );
    let receipt_1 = nodes[leader_1_idx]
        .propose(prop_1, Duration::from_secs(3))
        .await
        .expect("initial quorum commit");
    assert!(receipt_1.consensus_commit_index.get() >= 1);

    // Bootstrap 3 independent voter principals in `CommittedControlIndexGate` identity store.
    let gate_id = IdentityAuthority::open(gate_dir.path()).expect("open gate identity");
    let mut voter_keys = Vec::with_capacity(3);
    let mut voter_roster_tuples = Vec::with_capacity(3);
    for v_idx in 1..=3_u8 {
        let sk = SigningKey::from_bytes(&[v_idx.wrapping_add(0x70); 32]);
        let b = gate_id
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [v_idx.wrapping_add(0x71); 32],
                control_domain_policy_digest: [v_idx.wrapping_add(0x72); 32],
                public_key: sk.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: PKI_VALID_FROM_MS,
                key_valid_until_ms: PKI_VALID_UNTIL_MS,
                idempotency_key: IdempotencyKey::from_bytes([v_idx.wrapping_add(0x73); 16]),
                created_at_ms: PKI_VALID_FROM_MS,
            })
            .expect("bootstrap voter")
            .binding();
        voter_roster_tuples.push((b.principal_id, b.control_domain_id));
        voter_keys.push((sk, b));
    }
    let mut index_gate = CommittedControlIndexGate::new(
        ConsensusBackingMode::multi_host_quorum(&voter_roster_tuples).expect("quorum mode"),
    );

    let assign_1_rec = nlos_task::AuthorityAssignmentRecord {
        assignment_id: assign_1_id,
        task_id,
        task_generation: Generation::INITIAL,
        authority_lease_binding: nlos_task::AuthorityLeaseBinding {
            authority_id: nlos_types::TaskParticipantId::from_bytes([0x01; 16]),
            holder_id: ProcessId::from_bytes([0x02; 16]),
            term: 1,
            lease_epoch: 1,
            fencing_token: [0x03; 32],
            expires_at_ms: 2_000,
        },
        control_epoch: 1,
        participant_registry_binding: nlos_task::ParticipantRegistryBinding {
            generation: 1,
            root: [0x31; 32],
        },
        state: nlos_task::AuthorityAssignmentState::Active,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    };
    let subj_1 = ControlCommitSubject::from_assignment(&assign_1_rec).expect("subj_1");
    let dig_1 = quorum_commit_attestation_digest(
        receipt_1.consensus_commit_index,
        receipt_1.term,
        &receipt_1.state_root_digest,
        &subj_1,
    );
    let att_1 = QuorumCommitAttestation {
        consensus_commit_index: receipt_1.consensus_commit_index,
        consensus_term: receipt_1.term,
        state_root_digest: receipt_1.state_root_digest,
        verified_at_ms: PKI_CLOCK_MS,
        voter_attestations: voter_keys[0..2]
            .iter()
            .map(|(sk, b)| VoterCommitAttestation {
                voter_principal: b.principal_id,
                voter_control_domain: b.control_domain_id,
                voter_key_id: b.key_id,
                signature: sk.sign(&dig_1).to_bytes(),
            })
            .collect(),
    };
    let proof_1 = index_gate
        .admit_assignment(
            &gate_id,
            &assign_1_rec,
            receipt_1.consensus_commit_index,
            &att_1,
        )
        .expect("admit initial assignment");
    assert_eq!(proof_1.disposition, AdmittedCommitDisposition::Advanced);

    // 1. Inject triangle asymmetric partition (blackhole between the two followers while
    //    bridge node remains connected to both), then isolate the old leader completely!
    let survivor_a = (leader_1_idx + 1) % 3;
    let survivor_b = (leader_1_idx + 2) % 3;
    mesh.inject_triangle_asymmetric_partition(
        cells[leader_1_idx],
        cells[survivor_a],
        cells[survivor_b],
    );
    mesh.isolate_node(cells[leader_1_idx]);

    // Proposals on the isolated old leader MUST fail closed (`split_brain_commits == 0`).
    let stale_prop = ControlPlaneProposal::new(
        IdempotencyKey::from_bytes([0x22; 16]),
        ControlPlaneCommand::TaskAuthorityShardMutation(
            TaskAuthorityShardCommand::RecordCompletedTakeover {
                takeover_receipt_id: ReceiptId::from_bytes([0x99; 16]),
                task_id,
                task_generation: Generation::INITIAL,
                old_assignment_id: assign_1_id,
                new_assignment_id: TaskAuthorityAssignmentId::from_bytes([0xA9; 16]),
                frozen_old_authority_term: NonZeroU64::MIN,
                new_authority_term: NonZeroU64::new(2).expect("2"),
                new_home_cell: cells[leader_1_idx],
                new_control_epoch: 2,
                new_participant_registry_generation: 2,
                new_participant_registry_root: [0x9A; 32],
                exact_fence_set_root: [0x9A; 32],
                outstanding_effect_quarantine_root: None,
            },
        ),
    );
    let split_brain_res = nodes[leader_1_idx]
        .propose(stale_prop, Duration::from_millis(350))
        .await;
    assert!(
        split_brain_res.is_err(),
        "isolated old leader MUST NOT commit without majority quorum"
    );

    // 2. Wait for the 2-node majority (`survivor_a`, `survivor_b`) to elect a new leader in term > term_1.
    let mut new_leader_idx = None;
    let el_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < el_deadline {
        for &idx in &[survivor_a, survivor_b] {
            if nodes[idx].role() == ConsensusRole::Leader && nodes[idx].current_term() > term_1 {
                new_leader_idx = Some(idx);
                break;
            }
        }
        if new_leader_idx.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let leader_2_idx = new_leader_idx.expect("connected 2-node majority must elect new leader");
    assert!(nodes[leader_2_idx].current_term() > term_1);

    let takeover_rcpt_id = ReceiptId::from_bytes([0x33; 16]);
    let assign_2_id = TaskAuthorityAssignmentId::from_bytes([0xA2; 16]);
    let prop_2 = ControlPlaneProposal::new(
        IdempotencyKey::from_bytes([0x34; 16]),
        ControlPlaneCommand::TaskAuthorityShardMutation(
            TaskAuthorityShardCommand::RecordCompletedTakeover {
                takeover_receipt_id: takeover_rcpt_id,
                task_id,
                task_generation: Generation::INITIAL,
                old_assignment_id: assign_1_id,
                new_assignment_id: assign_2_id,
                frozen_old_authority_term: NonZeroU64::MIN,
                new_authority_term: NonZeroU64::new(2).expect("2"),
                new_home_cell: cells[leader_2_idx],
                new_control_epoch: 2,
                new_participant_registry_generation: 2,
                new_participant_registry_root: [0x44; 32],
                exact_fence_set_root: [0x44; 32],
                outstanding_effect_quarantine_root: None,
            },
        ),
    );
    let receipt_2 = nodes[leader_2_idx]
        .propose(prop_2, Duration::from_secs(3))
        .await
        .expect("new majority leader commit");
    assert!(receipt_2.consensus_commit_index > receipt_1.consensus_commit_index);
    assert!(receipt_2.term > receipt_1.term);
    let assign_2_rec = nlos_task::AuthorityAssignmentRecord {
        assignment_id: assign_2_id,
        task_id,
        task_generation: Generation::INITIAL,
        authority_lease_binding: nlos_task::AuthorityLeaseBinding {
            authority_id: nlos_types::TaskParticipantId::from_bytes([0x01; 16]),
            holder_id: ProcessId::from_bytes([0x04; 16]),
            term: 2,
            lease_epoch: 2,
            fencing_token: [0x05; 32],
            expires_at_ms: 5_000,
        },
        control_epoch: 2,
        participant_registry_binding: nlos_task::ParticipantRegistryBinding {
            generation: 2,
            root: [0x44; 32],
        },
        state: nlos_task::AuthorityAssignmentState::Active,
        created_at_ms: 2_000,
        updated_at_ms: 2_000,
    };
    let subj_2 = ControlCommitSubject::from_assignment(&assign_2_rec).expect("subj_2");
    let dig_2 = quorum_commit_attestation_digest(
        receipt_2.consensus_commit_index,
        receipt_2.term,
        &receipt_2.state_root_digest,
        &subj_2,
    );
    let att_2 = QuorumCommitAttestation {
        consensus_commit_index: receipt_2.consensus_commit_index,
        consensus_term: receipt_2.term,
        state_root_digest: receipt_2.state_root_digest,
        verified_at_ms: PKI_CLOCK_MS,
        voter_attestations: vec![survivor_a, survivor_b]
            .into_iter()
            .map(|idx| {
                let (sk, b) = &voter_keys[idx];
                VoterCommitAttestation {
                    voter_principal: b.principal_id,
                    voter_control_domain: b.control_domain_id,
                    voter_key_id: b.key_id,
                    signature: sk.sign(&dig_2).to_bytes(),
                }
            })
            .collect(),
    };
    let proof_2 = index_gate
        .admit_assignment(
            &gate_id,
            &assign_2_rec,
            receipt_2.consensus_commit_index,
            &att_2,
        )
        .expect("admit term>=2 quorum assignment");
    assert_eq!(proof_2.disposition, AdmittedCommitDisposition::Advanced);
    assert_eq!(proof_2.authority_term.get(), 2);

    // Stale commit index / stale term must be rejected fail-closed by CommittedControlIndexGate.
    assert!(
        index_gate
            .admit_assignment(
                &gate_id,
                &assign_1_rec,
                receipt_1.consensus_commit_index,
                &att_1
            )
            .is_err()
    );

    // 3. Heal mesh and verify isolated old leader steps down and converges to `receipt_2`.
    mesh.heal_all();
    let heal_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < heal_deadline {
        if nodes[leader_1_idx].consensus_commit_index() >= receipt_2.consensus_commit_index.get()
            && nodes[leader_1_idx].state_root_digest() == receipt_2.state_root_digest
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        nodes[leader_1_idx].consensus_commit_index(),
        receipt_2.consensus_commit_index.get()
    );
    assert_eq!(
        nodes[leader_1_idx].state_root_digest(),
        receipt_2.state_root_digest
    );

    for node in &nodes {
        node.shutdown().await;
    }
    mesh.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn road_c001_xhost_mid_stream_rst_ciphertext_corrupt_and_bounded_loss_reconciliation() {
    if !is_isolated_test_subprocess() {
        run_test_in_isolated_subprocess(
            "road_c001_xhost_mid_stream_rst_ciphertext_corrupt_and_bounded_loss_reconciliation",
            false,
        );
        return;
    }

    let dir_src = TempDir::new("rst-src");
    let dir_dst = TempDir::new("rst-dst");
    let source_host = bind_isolated_networked_host(dir_src.path(), 11, 12).await;
    let (_sem_sk, sem_boot) = semantic_issuer_bootstrap();
    source_host
        .register_peer_principal(sem_boot)
        .expect("register semantic issuer on source");

    let (target_real_addr, mut target_guard) = spawn_child_target_host(dir_dst.path(), 12, 12);
    let proxy = ChaosTcpProxy::bind(
        target_real_addr,
        ChaosLinkPolicy::passthrough(0x7202_B6B6_0000_0002),
    )
    .await
    .expect("bind ChaosTcpProxy");
    let ctrl = proxy.controller();

    let source_cell = cell_for_node(11);
    let target_cell = cell_for_node(12);
    exchange_peer_bootstrap_over_mtls(
        &source_host,
        target_cell,
        proxy.listen_addr(),
        EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 1),
        IdempotencyKey::from_bytes([0x10; 16]),
    )
    .await
    .expect("exchange peer bootstrap through ChaosTcpProxy");

    let artifact_id = ArtifactId::from_bytes([0x82; 16]);
    let blob_256k: Vec<u8> = (0..(256 * 1024))
        .map(|i| u8::try_from((i * 31 + 7) % 251).expect("byte"))
        .collect();
    let (rev, _) = source_host.with_cell_host(|ch| {
        publish_restart_image_artifact(
            ch,
            artifact_id,
            0x12,
            ARTIFACT_CONTENT_TYPE,
            &blob_256k,
            PKI_CLOCK_MS - 50,
        )
        .expect("publish 256 KiB artifact on source")
    });

    // Phase A: In-transit TLS 1.3 ciphertext bit-flip (`[TCB-ENDPOINT-001]`).
    ctrl.arm_ciphertext_corruption(CiphertextCorruptionRule {
        post_handshake_only: true,
        byte_offset: 12,
        xor_mask: 0x5A,
    });
    source_host
        .transport()
        .invalidate_cached_session(target_cell);
    let sem_bridge = source_host
        .semantic_sync_bridge()
        .expect("source semantic bridge");
    let sem_client =
        SemanticNetSyncClient::new(source_host.transport().as_ref(), sem_bridge.as_ref());
    let corrupt_sync = sem_client
        .pull_and_merge_from_peer(
            target_cell,
            SemanticNetSyncConfig {
                max_batch_events: 16,
                rpc_deadline_ms: 2_000,
                signed_at_ms: PKI_CLOCK_MS,
                base_idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
            },
        )
        .await;
    assert!(
        corrupt_sync.is_err(),
        "rustls AEAD must reject corrupted in-transit ciphertext"
    );
    assert!(
        proxy.metrics().ciphertext_corruptions_injected >= 1,
        "proxy must record injected ciphertext corruption"
    );

    // Phase B: Mid-stream TCP RST after 128 client->server application bytes (`[REMOTE-FAIL-001]`).
    ctrl.heal();
    ctrl.arm_reset_after_first_payload_byte(128);
    source_host
        .transport()
        .invalidate_cached_session(target_cell);

    let contract = RemoteCallContract {
        call_id: [0x23; 16],
        idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
        deadline_tick: 1_500,
        max_retries: 0,
        attempt_number: 0,
        side_effecting: true,
    };
    let tracker = PendingReconciliationTracker::new();
    let rpc_res = source_host
        .transport()
        .call_cell_rpc(target_cell, &contract, vec![0xAB; 256])
        .await;
    let uncertain_outcome = match rpc_res {
        Ok(outcome) => outcome,
        Err(_net_err) => contract.classify_inflight_timeout(50),
    };
    assert!(
        matches!(
            uncertain_outcome,
            RemoteCallOutcome::DeadlineExceededUncertain {
                side_effecting: true,
                ..
            }
        ),
        "expected DeadlineExceededUncertain, got {uncertain_outcome:?}"
    );
    assert!(
        proxy.metrics().connections_reset_after_payload >= 1,
        "ChaosTcpProxy must record mid-stream TCP reset injection"
    );
    tracker
        .record_uncertain_side_effect(contract.idempotency_key, SocketFailureKind::ConnectionReset);
    assert!(matches!(
        tracker.check_before_dispatch(contract.idempotency_key),
        Err(RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation { .. })
    ));
    tracker.mark_reconciled(contract.idempotency_key);
    assert!(
        tracker
            .check_before_dispatch(contract.idempotency_key)
            .is_ok()
    );

    // Phase C: Heal link and verify resumable 256 KiB chunked artifact transfer + semantic sync.
    ctrl.heal();
    source_host
        .transport()
        .invalidate_cached_session(target_cell);
    let pkg = source_host.with_cell_host(|ch| {
        export_artifact_transfer_package(
            &ch.runtime().artifacts,
            ExportArtifactTransferRequest {
                artifact_id,
                target_revision: Some(rev),
                origin_cell_id: *source_cell.as_bytes(),
                producer_epoch: ch.fence().epoch().get(),
                tombstone: None,
                now_ms: PKI_CLOCK_MS - 50,
            },
        )
        .expect("export 256 KiB artifact package")
    });
    assert_eq!(pkg.revisions[0].revision, rev);
    let art_client = ArtifactNetTransferClient::new(source_cell, source_host.transport().as_ref())
        .with_chunk_payload_bytes(60 * 1024);
    let art_summary = art_client
        .transfer_package_resumable(
            target_cell,
            &pkg,
            None,
            TaskId::from_bytes([0x31; 16]),
            CommitPermitId::from_bytes([0x32; 16]),
            ContentDigest::from_bytes([0x33; 32]),
            PKI_CLOCK_MS,
            0x44,
        )
        .await
        .expect("resumable chunked 256 KiB artifact transfer after heal");
    assert_eq!(art_summary.0.chunks_transmitted, 5);
    assert_eq!(art_summary.0.payload_bytes_transmitted, 256 * 1024);

    let healed_sync = sem_client
        .pull_and_merge_from_peer(
            target_cell,
            SemanticNetSyncConfig {
                max_batch_events: 16,
                rpc_deadline_ms: 2_000,
                signed_at_ms: PKI_CLOCK_MS,
                base_idempotency_key: IdempotencyKey::from_bytes([0x25; 16]),
            },
        )
        .await
        .expect("semantic pull_and_merge after link heal");
    assert_eq!(healed_sync.pulled_event_count, 1);

    // Phase D: Bounded-loss partition reconciliation (`[LEASE-LOSS-001]`, `[ROAD-C-001]`).
    let lease_id = QuotaLeaseId::from_bytes([0x61; 16]);
    let proof = source_host.with_cell_host_mut(|ch| {
        let _ = ch
            .grant_quota_lease(lease_id, 300, None)
            .expect("grant 300");
        let _ = ch.activate_quota_lease(lease_id).expect("activate 300");
        let _ = ch.report_quota_usage(lease_id, 90).expect("report 90");
        let _ = ch
            .enter_control_plane_partition(10, 50)
            .expect("src partition");
        let mut ep = LocalCellHostPartitionEndpoint::new(ch);
        let prior_frozen_fence = ep.current_fence().expect("prior fence");
        let adv = ep.advance_cell_epoch().expect("advance cell epoch");
        let _ = ep.begin_reconnection(20).expect("begin reconnection");
        let pool_before = ep.quota_available().expect("pool before");
        let (summary, _delta) = ep
            .settle_quarantined_quota_lease(QuotaPartitionSettlementSpec {
                lease_id,
                receipt_id: ReceiptId::from_bytes([0x78; 16]),
                reconcile_seq: 1,
                gateway_id: [0x79; 16],
                attested_spent: 120,
                unattested_inflight_loss: 30,
                max_unreported_inflight_window: 50,
            })
            .expect("settle quarantined quota lease");
        let pool_after = ep.quota_available().expect("pool after");
        let _ = ep.complete_reconnection(30).expect("complete reconnection");
        let mut p = PartitionBoundedLossProof {
            cell_id: source_cell,
            prior_frozen_fence,
            reconnected_fence: adv.fence,
            pool_available_before_reconcile: pool_before,
            pool_available_after_reconcile: pool_after,
            total_granted_face_value: summary.face_value,
            reconciled_spent: summary.attested_spent,
            refunded_available: summary.refunded_to_available,
            permanently_quarantined_loss: summary.permanently_quarantined_loss,
            max_unreported_inflight_window: summary.max_unreported_inflight_window,
            quota_summaries: vec![summary],
            reconciled_capacity_leases: Vec::new(),
            reconciled_device_leases: Vec::new(),
            proof_digest: [0; 32],
        };
        p.proof_digest = p.compute_digest();
        p
    });
    assert!(
        proof.verify_invariants(),
        "PartitionBoundedLossProof must hold"
    );
    assert_eq!(
        proof.reconciled_spent + proof.refunded_available + proof.permanently_quarantined_loss,
        proof.total_granted_face_value
    );
    assert_eq!(proof.permanently_quarantined_loss, 30);

    target_guard.shutdown_clean();
    source_host.shutdown().await;
    proxy.shutdown().await;
}

#[test]
fn road_c001_xhost_clock_skew_bound_breach_and_remote_lease_quarantine() {
    if !is_isolated_test_subprocess() {
        run_test_in_isolated_subprocess(
            "road_c001_xhost_clock_skew_bound_breach_and_remote_lease_quarantine",
            false,
        );
        return;
    }
    let dir = TempDir::new("clock-skew-gate");
    let policy = skew_policy(); // max_clock_skew_ms = 500, safety_margin_ms = 250
    let ttl_grant_ms = 3_000_u64;
    let ttl_holder = policy
        .validate_and_compute_holder_budget(ttl_grant_ms)
        .expect("ttl_holder = 3000 - 2*500 - 250 = 1750 ms");
    assert_eq!(ttl_holder.get(), 1_750);
    let wait_floor = policy
        .grantor_expiry_wait_floor(ttl_grant_ms)
        .expect("grantor wait floor = 3000 + 500 = 3500 ms");
    assert_eq!(wait_floor.get(), 3_500);

    // 1. AuthorityClockHorizonGuard latches sticky QuarantinedClockViolation on monotonic
    //    rollback or boot-generation regression (`[CLOCK-BOUND-001]`).
    let mut guard = AuthorityClockHorizonGuard::open(
        Arc::new(AuthorityClock::open(dir.path().join("auth-clock")).expect("open AuthorityClock")),
        Generation::INITIAL,
    )
    .expect("open AuthorityClockHorizonGuard");
    let _ = guard
        .sample_verified_instant(
            IdempotencyKey::from_bytes([0x01; 16]),
            MonotonicInstantMs::new(1_000),
            Generation::INITIAL,
        )
        .expect("monotonic advance 1000");
    let rollback_err = guard
        .sample_verified_instant(
            IdempotencyKey::from_bytes([0x02; 16]),
            MonotonicInstantMs::new(999),
            Generation::INITIAL,
        )
        .expect_err("monotonic rollback must fail closed");
    assert!(matches!(
        rollback_err,
        QuarantinedClockViolation::LocalMonotonicRollback { .. }
    ));
    assert!(
        guard.latched_violation().is_some(),
        "violation must be sticky"
    );

    // 2. GrantorClockBoundedCoordinator + HolderClockBoundedLease horizon & barrier gate.
    let holder_cell = cell_for_node(22);
    let gw_a = [0xA1; 16];
    let gw_b = [0xB2; 16];
    let scope =
        MultiEndpointFenceScope::with_gateways(holder_cell, [gw_a, gw_b]).expect("2-gateway scope");
    let dev_id = DeviceId::from_bytes(SHARED_DEVICE_BYTES);
    let cell_auth =
        CellAuthority::claim_with_data_dir(domain_for_node(21), dir.path().join("cell-auth"))
            .expect("claim CellAuthority");
    let mut coord = GrantorClockBoundedCoordinator::open_durable(
        dir.path().join("lease-horizon.sqlite"),
        Arc::new(cell_auth),
        1_000,
        500,
        &[dev_id],
    )
    .expect("open durable GrantorClockBoundedCoordinator");
    let fence_1 = coord.authority().fence();

    let quota_id = QuotaLeaseId::from_bytes([0x11; 16]);
    let cap_id = CapacityLeaseId::from_bytes([0x22; 16]);
    let grant_params = CrossHostGrantParams {
        holder_cell,
        holder_boot_generation: Generation::INITIAL,
        grantor_commit_mono: MonotonicInstantMs::new(10_000),
        ttl_grant_ms,
        policy,
        authority_expiry_tick: 500,
        multi_endpoint_scope: scope,
    };
    let (quota_grant, _) = coord
        .grant_quota_cross_host(&fence_1, quota_id, 400, grant_params.clone())
        .expect("grant cross-host quota lease");
    let (cap_grant, _) = coord
        .grant_capacity_cross_host(&fence_1, cap_id, 100, grant_params)
        .expect("grant cross-host capacity lease");

    let mut holder_lease = HolderClockBoundedLease::new_quota(
        quota_grant,
        policy,
        MonotonicInstantMs::new(10_000),
        MonotonicInstantMs::new(10_040),
        ttl_grant_ms,
    )
    .expect("construct HolderClockBoundedLease");
    holder_lease
        .admit_local_quota_reserve(MonotonicInstantMs::new(11_000), 150)
        .expect("admit reserve within shrunk ttl_holder (elapsed 1000 < 1750)");
    let _ = holder_lease
        .admit_commit_permit(MonotonicInstantMs::new(11_500), 1)
        .expect("admit CommitPermit before shrunk horizon");
    assert_eq!(
        holder_lease.refresh_state(MonotonicInstantMs::new(11_750)),
        HolderHorizonState::FencedByShrunkHorizon
    );
    assert!(
        holder_lease
            .admit_commit_permit(MonotonicInstantMs::new(11_750), 2)
            .is_err(),
        "holder must self-fence at t_req + ttl_holder = 11_750 ms"
    );

    let mut holder_rtt_breach = HolderClockBoundedLease::new_quota(
        quota_grant,
        policy,
        MonotonicInstantMs::new(20_000),
        MonotonicInstantMs::new(20_020),
        ttl_grant_ms,
    )
    .expect("second holder handle");
    let rtt_err = holder_rtt_breach
        .observe_rtt_sample(1_200)
        .expect_err("RTT/2 = 600ms > max_clock_skew_ms = 500ms must quarantine");
    assert!(matches!(
        rtt_err,
        QuarantinedClockViolation::RttHalfExceedsMaxClockSkew { .. }
    ));

    // Grantor cannot sweep before ttl_grant + max_clock_skew = 10_000 + 3_500 = 13_500 ms.
    let within = coord
        .evaluate_unreachable_holder_horizon(
            &fence_1,
            CrossHostLeaseFamily::Quota,
            *quota_id.as_bytes(),
            MonotonicInstantMs::new(13_500),
        )
        .expect("evaluate at 13_500");
    assert!(matches!(
        within,
        GrantorHorizonStatus::StillWithinSkewEnvelope { .. }
    ));

    // At 13_501 ms (> wait_floor), grantor moves leases strictly to QUARANTINED (never AVAILABLE).
    let swept_q = coord
        .evaluate_unreachable_holder_horizon(
            &fence_1,
            CrossHostLeaseFamily::Quota,
            *quota_id.as_bytes(),
            MonotonicInstantMs::new(13_501),
        )
        .expect("sweep quota at 13_501");
    assert_eq!(
        swept_q,
        GrantorHorizonStatus::QuarantinedAfterSkewHorizon {
            disposition: QuarantineDisposition::LedgerQuarantinedFenceOnly,
        }
    );
    let _ = coord
        .evaluate_unreachable_holder_horizon(
            &fence_1,
            CrossHostLeaseFamily::Capacity,
            *cap_id.as_bytes(),
            MonotonicInstantMs::new(13_501),
        )
        .expect("sweep capacity at 13_501");
    assert_eq!(
        coord.quota().available(),
        600,
        "no blind refund to AVAILABLE"
    );

    // Advance epoch and require 100% of MultiEndpointFenceScope gateways before barrier reconcile.
    let fence_2 = coord
        .advance_epoch_and_quarantine_all()
        .expect("advance grantor epoch");
    let q_quarantined = coord.quota().query(quota_id).expect("query quota");
    let ack_q_a = FenceBarrierAck::for_quota(&q_quarantined, gw_a, 150);
    let ack_q_b = FenceBarrierAck::for_quota(&q_quarantined, gw_b, 150);
    let partial_cov = coord
        .record_fence_barrier_ack(CrossHostLeaseFamily::Quota, ack_q_a)
        .expect("record gw_a ack");
    assert!(!partial_cov.is_complete());
    let q_recon = nlos_lease::QuotaReconciliationReceipt::for_lease(
        ReceiptId::from_bytes([0x51; 16]),
        1,
        &q_quarantined,
        150,
        250,
        0,
        ack_q_a,
    );
    assert!(
        coord
            .reconcile_quarantined_quota_via_barrier(&fence_2, q_recon)
            .is_err(),
        "partial gateway barrier coverage (1/2) must fail closed"
    );
    let full_cov = coord
        .record_fence_barrier_ack(CrossHostLeaseFamily::Quota, ack_q_b)
        .expect("record gw_b ack");
    assert!(full_cov.is_complete());
    let settled_q = coord
        .reconcile_quarantined_quota_via_barrier(&fence_2, q_recon)
        .expect("reconcile quota after 100% barrier coverage");
    assert_eq!(settled_q.state(), QuotaLeaseState::Closed);
    assert_eq!(coord.quota().available(), 850);
    assert_eq!(
        coord
            .quarantine_disposition(CrossHostLeaseFamily::Quota, *quota_id.as_bytes())
            .expect("quota disposition"),
        QuarantineDisposition::LedgerQuarantinedFenceOnly
    );

    // Capacity reconciliation with verified CleanupAttestation upgrades disposition to PhysicallyCleanedVerified.
    let c_quarantined = coord.capacity().query(cap_id).expect("query cap");
    let ack_c_a = FenceBarrierAck::for_capacity(&c_quarantined, gw_a);
    let ack_c_b = FenceBarrierAck::for_capacity(&c_quarantined, gw_b);
    let _ = coord
        .record_fence_barrier_ack(CrossHostLeaseFamily::Capacity, ack_c_a)
        .expect("cap gw_a");
    let _ = coord
        .record_fence_barrier_ack(CrossHostLeaseFamily::Capacity, ack_c_b)
        .expect("cap gw_b");
    let cleanup = CleanupAttestation::for_lease(&cap_grant, 100, true);
    let c_recon = nlos_lease::CapacityReconciliationReceipt::for_lease(
        ReceiptId::from_bytes([0x52; 16]),
        &c_quarantined,
        cleanup,
        ack_c_a,
    );
    let settled_c = coord
        .reconcile_quarantined_capacity_via_barrier(&fence_2, c_recon)
        .expect("reconcile capacity with CleanupAttestation");
    assert_eq!(settled_c.state(), CapacityLeaseState::Returned);
    assert!(matches!(
        coord
            .quarantine_disposition(CrossHostLeaseFamily::Capacity, *cap_id.as_bytes())
            .expect("cap disposition"),
        QuarantineDisposition::PhysicallyCleanedVerified {
            proof_kind: PhysicalCleanupProofKind::CapacityCleanupAttestation,
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn road_c001_xhost_eight_step_migration_under_rtt_jitter_and_pinned_rejection() {
    if !is_isolated_test_subprocess() {
        run_test_in_isolated_subprocess(
            "road_c001_xhost_eight_step_migration_under_rtt_jitter_and_pinned_rejection",
            false,
        );
        return;
    }

    let dir_src = TempDir::new("mig-src");
    let dir_dst = TempDir::new("mig-dst");
    let source_host = bind_isolated_networked_host(dir_src.path(), 31, 32).await;
    let (target_addr, mut target_guard) = spawn_child_target_host(dir_dst.path(), 32, 32);

    let proxy = ChaosTcpProxy::bind(
        target_addr,
        ChaosLinkPolicy::passthrough(0x7202_D4D4_0000_0004),
    )
    .await
    .expect("bind ChaosTcpProxy");
    let ctrl = proxy.controller();
    ctrl.set_latency_and_jitter(3, 4);

    let source_cell = cell_for_node(31);
    let target_cell = cell_for_node(32);
    exchange_peer_bootstrap_over_mtls(
        &source_host,
        target_cell,
        proxy.listen_addr(),
        EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 1),
        IdempotencyKey::from_bytes([0x41; 16]),
    )
    .await
    .expect("exchange peer bootstrap under RTT jitter");

    let seed = 0x51_u8;
    let task_id = TaskId::from_bytes([0x52; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x53; 16]);
    let src_quota_id = QuotaLeaseId::from_bytes([0x54; 16]);
    let dst_quota_id = QuotaLeaseId::from_bytes([0x55; 16]);
    let restart_art_id = ArtifactId::from_bytes([0x56; 16]);
    let cap_path = NamePath::new("/w72/xhost/mig").expect("valid NamePath");

    let (binding, cap_rebind_item, restart_rev, restart_dig) =
        source_host.with_cell_host_mut(|ch| {
            let b = ch
                .spawn_monitored_process(seed, task_id, attempt_id, Generation::INITIAL)
                .expect("spawn monitored process on source");
            let _ = ch
                .grant_quota_lease(src_quota_id, 200, None)
                .expect("grant 200 on source");
            let _ = ch
                .activate_quota_lease(src_quota_id)
                .expect("activate 200 on source");
            let _ = ch
                .report_quota_usage(src_quota_id, 50)
                .expect("spend 50 on source");
            let (rev, dig) = publish_restart_image_artifact(
                ch,
                restart_art_id,
                seed,
                ARTIFACT_CONTENT_TYPE,
                b"xhost-restart-image-v1",
                PKI_CLOCK_MS - 200,
            )
            .expect("publish restart image");

            let root_sk = SigningKey::from_bytes(&[0x71; 32]);
            let del_sk = SigningKey::from_bytes(&[0x72; 32]);
            let root_boot = BootstrapPrincipalRequest {
                principal_profile_digest: [0x71; 32],
                control_domain_policy_digest: [0x72; 32],
                public_key: root_sk.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: PKI_VALID_FROM_MS,
                key_valid_until_ms: PKI_VALID_UNTIL_MS,
                idempotency_key: IdempotencyKey::from_bytes([0x71; 16]),
                created_at_ms: PKI_VALID_FROM_MS,
            };
            let del_boot = BootstrapPrincipalRequest {
                principal_profile_digest: [0x73; 32],
                control_domain_policy_digest: [0x74; 32],
                public_key: del_sk.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: PKI_VALID_FROM_MS,
                key_valid_until_ms: PKI_VALID_UNTIL_MS,
                idempotency_key: IdempotencyKey::from_bytes([0x72; 16]),
                created_at_ms: PKI_VALID_FROM_MS,
            };
            let root_b = ch
                .runtime()
                .identity
                .bootstrap_principal(root_boot)
                .expect("root_b")
                .binding();
            let del_b = ch
                .runtime()
                .identity
                .bootstrap_principal(del_boot)
                .expect("del_b")
                .binding();
            let root_cmd = IssueRootCapabilityRequest {
                issuer_key_id: root_b.key_id,
                holder_key_id: root_b.key_id,
                target: CapabilityTarget::Task(task_id),
                rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
                purpose_digest: Some([0x75; 32]),
                valid_from_ms: PKI_VALID_FROM_MS,
                valid_until_ms: PKI_VALID_UNTIL_MS,
                delegation_depth_remaining: 2,
                call_limit: None,
                idempotency_key: IdempotencyKey::from_bytes([0x73; 16]),
                issued_at_ms: PKI_VALID_FROM_MS + 10,
            };
            let root_sig = root_sk
                .sign(&issue_root_command_message(root_cmd))
                .to_bytes();
            let signed_root = SignedIssueRootCapabilityRequest {
                command: root_cmd,
                signer: root_b.principal_id,
                signature: root_sig,
            };
            let root_rec = ch
                .runtime()
                .capability()
                .issue_root_signed(&ch.runtime().identity, signed_root)
                .expect("issue root on source")
                .record();
            let _ = ch
                .insert_cached_name(
                    cap_path.clone(),
                    root_rec.handle.capability_id,
                    Generation::INITIAL,
                )
                .expect("cache cap on source");

            let del_cmd = DelegateCapabilityRequest {
                parent: root_rec.handle,
                delegator_key_id: root_b.key_id,
                recipient_key_id: del_b.key_id,
                target: CapabilityTarget::Task(task_id),
                rights: CapabilityRights::SEMANTIC_APPEND,
                purpose_digest: Some([0x75; 32]),
                valid_from_ms: PKI_VALID_FROM_MS + 20,
                valid_until_ms: PKI_VALID_UNTIL_MS - 1_000,
                delegation_depth_remaining: 1,
                call_limit: Some(16),
                idempotency_key: IdempotencyKey::from_bytes([0x74; 16]),
                delegated_at_ms: PKI_VALID_FROM_MS + 20,
            };
            let del_sig = root_sk.sign(&delegate_command_message(del_cmd)).to_bytes();
            let signed_del = SignedDelegateCapabilityRequest {
                command: del_cmd,
                signer: root_b.principal_id,
                signature: del_sig,
            };
            let rev_cmd = RevokeCapabilityRequest {
                handle: root_rec.handle,
                revoker_key_id: root_b.key_id,
                idempotency_key: IdempotencyKey::from_bytes([0x75; 16]),
                revoked_at_ms: PKI_CLOCK_MS,
            };
            let rev_sig = root_sk.sign(&revoke_command_message(rev_cmd)).to_bytes();
            let item = CapabilityRebindItemSpec {
                bootstrap_principals: vec![root_boot, del_boot],
                seed_parent_root: Some(signed_root),
                signed_delegation: signed_del,
                target_name_path: Some(cap_path.clone()),
                revoke_on_source: Some(SignedRevokeCapabilityRequest {
                    command: rev_cmd,
                    signer: root_b.principal_id,
                    signature: rev_sig,
                }),
            };
            (b, item, rev, dig)
        });

    let base_req = CellHostProcessMigrationRequest {
        migration_id: [0x58; 16],
        process_id: binding.process_id,
        source_cell_id: source_cell,
        target_cell_id: target_cell,
        source_generation: Generation::INITIAL,
        lineage: ProcessLineageSpec {
            genesis: ProcessLineageGenesis::from_spawned_binding(seed, &binding),
            prior_restores: Vec::new(),
        },
        attached_sessions: vec![DriverSessionAttachment::PinnedNonMigratable {
            session_id: [0x59; 16],
            pin_reason: "pci-passthrough-sriov".to_owned(),
            artifact_restart_image_digest: Some(restart_dig.into_bytes()),
        }],
        pinned_session_policy: PinnedSessionPolicy::RefuseMigration,
        custom_intent_reason: None,
        initiated_at_ms: PKI_CLOCK_MS,
        quiesce: SourceQuiesceSpec {
            cancel_scope: None,
            active_fiber_count: 1,
            inflight_operations: Vec::new(),
            quiesced_at_ms: PKI_CLOCK_MS + 10,
        },
        checkpoint: CheckpointSourceSpec::ArtifactRestartImage {
            artifact_id: restart_art_id,
            revision: restart_rev,
            sync_from_source_if_missing: true,
            checkpointed_at_ms: PKI_CLOCK_MS + 20,
        },
        artifact_sync_seed: seed,
        source_fence: SourceFenceSpec {
            invalidate_name_paths: vec![cap_path],
            terminate_idempotency_key: IdempotencyKey::from_bytes([0x5A; 16]),
            stale_dispatch_probe_fiber_id: ExecutionFiberId::from_bytes([0x5B; 16]),
            stale_dispatch_probe_key: IdempotencyKey::from_bytes([0x5C; 16]),
            fenced_at_ms: PKI_CLOCK_MS + 30,
        },
        capabilities: vec![cap_rebind_item],
        capabilities_rebound_at_ms: PKI_CLOCK_MS + 40,
        leases: LeaseMigrationBundleSpec {
            quota: QuotaLeaseMigrationSpec {
                source_lease_id: src_quota_id,
                gateway_id: [0x5D; 16],
                override_fence_ack: None,
                target_lease_id: dst_quota_id,
                target_face_value: None,
                target_expires_at: None,
                activate_on_target: true,
            },
            capacity: None,
            device: None,
            regranted_at_ms: PKI_CLOCK_MS + 50,
        },
        target_restore: TargetRestoreSpec {
            existing_target_domain_id: None,
            domain_policy_digest: [0x5E; 32],
            domain_idempotency_key: IdempotencyKey::from_bytes([0x5F; 16]),
            restore_idempotency_key: IdempotencyKey::from_bytes([0x60; 16]),
            restored_at_ms: PKI_CLOCK_MS + 60,
        },
        completed_at_ms: PKI_CLOCK_MS + 70,
    };

    // 1. PinnedNonMigratable under RefuseMigration fails closed pre-fence at Step 1 (`Initiated`).
    let refused = drive_cross_socket_process_migration(
        &source_host,
        None,
        base_req.clone(),
        CrossSocketProcessMigrationOptions::default(),
    )
    .await
    .expect_err("PinnedNonMigratable + RefuseMigration must fail closed pre-fence");
    assert!(matches!(
        refused,
        CrossSocketMigrationError::Drive(ref err)
            if matches!(
                **err,
                ProcessMigrationDriveError::Protocol {
                    step: ProcessMigrationStep::Initiated,
                    ..
                }
            )
    ));

    // 2. DowngradeToRestartFromArtifact succeeds across all 8 steps under RTT jitter.
    let mut downgrade_req = base_req;
    downgrade_req.pinned_session_policy = PinnedSessionPolicy::DowngradeToRestartFromArtifact;
    let outcome = drive_cross_socket_process_migration(
        &source_host,
        None,
        downgrade_req,
        CrossSocketProcessMigrationOptions {
            sync_semantic_provenance: false,
            ..CrossSocketProcessMigrationOptions::default()
        },
    )
    .await
    .expect("8-step cross-socket migration under RTT jitter");
    assert_eq!(
        outcome.coordinator.current_step(),
        ProcessMigrationStep::Completed
    );
    assert!(outcome.receipt.verify_digest());
    assert!(outcome.target_checkpoint_summary.verify_digest());
    assert_eq!(outcome.source_lease_close_report.closed_quota.returned, 150);
    assert_eq!(
        outcome.target_lease_regrant_summary.target_quota_face_value,
        150
    );
    assert_eq!(
        outcome
            .target_restore_report
            .restored_binding
            .process_generation
            .get(),
        2
    );
    assert!(
        proxy.metrics().injected_delay_total_ms > 0,
        "ChaosTcpProxy must inject non-zero RTT latency/jitter during 8-step migration"
    );

    target_guard.shutdown_clean();
    source_host.shutdown().await;
    proxy.shutdown().await;
}

fn is_isolated_test_subprocess() -> bool {
    std::env::var(ENV_XHOST_ISOLATED_TEST).is_ok()
}

fn run_test_in_isolated_subprocess(test_name: &str, ignored: bool) {
    let exe = std::env::current_exe().expect("current test binary");
    let mut cmd = Command::new(exe);
    cmd.arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(ENV_XHOST_ISOLATED_TEST, "1");
    if ignored {
        cmd.arg("--ignored");
    }
    let status = cmd.status().expect("run isolated test subprocess");
    assert!(
        status.success(),
        "isolated test subprocess `{test_name}` failed: {status}"
    );
}

struct ChildWorkerProcessGuard {
    child: Child,
    stdin: Option<ChildStdin>,
    _stdout: Option<BufReader<ChildStdout>>,
    exited: bool,
}

impl ChildWorkerProcessGuard {
    fn shutdown_clean(&mut self) {
        if let Some(mut child_in) = self.stdin.take() {
            let _ = writeln!(child_in, "SHUTDOWN");
            let _ = child_in.flush();
        }
        let status = self.child.wait().expect("wait for child OS process");
        self.exited = true;
        assert!(status.success(), "child OS process must exit 0");
    }
}

impl Drop for ChildWorkerProcessGuard {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if !self.exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn spawn_child_target_host(
    child_root: &Path,
    node_idx: u8,
    cluster_size: u8,
) -> (SocketAddr, ChildWorkerProcessGuard) {
    let exe = std::env::current_exe().expect("current test binary");
    let mut child = Command::new(exe)
        .arg("--exact")
        .arg("road_c001_xhost_multi_process_child_worker_chaos_gate")
        .arg("--ignored")
        .arg("--nocapture")
        .env(ENV_XHOST_CHILD_ROOT, child_root)
        .env(ENV_XHOST_CHILD_NODE, node_idx.to_string())
        .env(ENV_XHOST_CHILD_CLUSTER, cluster_size.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn child OS process");

    let stdout = child.stdout.take().expect("child stdout");
    let stdin = child.stdin.take();
    let mut reader = BufReader::new(stdout);
    let mut child_addr: Option<SocketAddr> = None;
    let mut line = String::new();
    while reader.read_line(&mut line).expect("read child stdout") > 0 {
        if let Some(rest) = line.trim().strip_prefix("XHOST_CHILD_READY ") {
            child_addr = Some(rest.parse().expect("parse child SocketAddr"));
            break;
        }
        line.clear();
    }
    let guard = ChildWorkerProcessGuard {
        child,
        stdin,
        _stdout: Some(reader),
        exited: false,
    };
    (
        child_addr.expect("child worker reported XHOST_CHILD_READY"),
        guard,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawns OS child process worker over ChaosTcpProxy; run with --ignored"]
async fn road_c001_xhost_multi_process_child_worker_chaos_gate() {
    if let Ok(child_root) = std::env::var(ENV_XHOST_CHILD_ROOT) {
        let node_idx: u8 = std::env::var(ENV_XHOST_CHILD_NODE)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(42);
        let cluster_size: u8 = std::env::var(ENV_XHOST_CHILD_CLUSTER)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(42);
        let host =
            bind_isolated_networked_host(Path::new(&child_root), node_idx, cluster_size).await;
        let (_sem_sk, sem_boot) = semantic_issuer_bootstrap();
        host.register_peer_principal(sem_boot)
            .expect("register semantic issuer on child worker");
        append_semantic_event_on_host(&host, PKI_CLOCK_MS - 100);
        let addr = host.service_addr().expect("child service addr");
        println!("XHOST_CHILD_READY {addr}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        host.shutdown().await;
        return;
    }
    if !is_isolated_test_subprocess() {
        run_test_in_isolated_subprocess(
            "road_c001_xhost_multi_process_child_worker_chaos_gate",
            true,
        );
        return;
    }

    let dir_parent = TempDir::new("mp-parent");
    let dir_child = TempDir::new("mp-child");
    let parent_host = bind_isolated_networked_host(dir_parent.path(), 41, 42).await;
    append_semantic_event_on_host(&parent_host, PKI_CLOCK_MS - 100);

    let (real_child_addr, mut guard) = spawn_child_target_host(dir_child.path(), 42, 42);

    let proxy = ChaosTcpProxy::bind(
        real_child_addr,
        ChaosLinkPolicy::passthrough(0x7202_E5E5_0000_0005),
    )
    .await
    .expect("bind ChaosTcpProxy to child OS process");
    proxy.controller().set_latency_and_jitter(2, 3);

    exchange_peer_bootstrap_over_mtls(
        &parent_host,
        cell_for_node(42),
        proxy.listen_addr(),
        EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 1),
        IdempotencyKey::from_bytes([0x88; 16]),
    )
    .await
    .expect("exchange peer bootstrap with separate OS child process over ChaosTcpProxy");
    assert!(proxy.metrics().connections_accepted >= 1);

    guard.shutdown_clean();
    parent_host.shutdown().await;
    proxy.shutdown().await;
}
