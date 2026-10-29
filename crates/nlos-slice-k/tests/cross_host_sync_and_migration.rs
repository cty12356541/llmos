//! Cross-physical-host streaming sync (`[SEM-CHECKPOINT-001]`, `[DIST-ART-001]`),
//! 8-step process migration (`[DIST-MIGRATE-001/002]`, `[LEASE-MIGRATE-001]`),
//! post-`SourceFenced` network-cut reconciliation (`[DIST-FAIL-001]`), and
//! `N = 3` Raft quorum-backed 6-endpoint takeover (`[DIST-TASK-002/004]`,
//! `[TASK-SCALE-002]`) integration suite (`W71-C-SYNC-MIGRATE`).
//!
//! Every [`NetCellHost`] runs inside its own isolated child OS process
//! (`ADR-0018`: 1 OS process = 1 `CellAuthority` claim) rooted in its own
//! dedicated `TempDir` with **zero** shared filesystem paths across hosts.
//! Parent-to-child test harness orchestration uses anonymous OS stdio pipes,
//! while all inter-host data/control traffic traverses real loopback TCP +
//! TLS 1.3 mTLS + 5-axis Exporter `CellFence` sockets.

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use nlos_artifact::{
    ArtifactConflictStrategy, ArtifactDeletionSemantics, ArtifactMergeStrategy,
    ArtifactReplicaAuthority, ArtifactSyncPolicy,
};
use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodyTier, KeyPurpose as CustodyKeyPurpose,
    SigningKeyCustodyVault,
};
use nlos_capability::{
    CapabilityAuthorityError, CapabilityHandle, CapabilityRecord, CapabilityRights,
    CapabilityTarget, DelegateCapabilityRequest, IssueRootCapabilityRequest,
    RevokeCapabilityRequest, SignedDelegateCapabilityRequest, SignedIssueRootCapabilityRequest,
    SignedRevokeCapabilityRequest, delegate_command_message, issue_root_command_message,
    revoke_command_message,
};
use nlos_cell::{
    CellEpoch, CellFence, CellFencingToken, CellIdentity, FailureDetectorConfig, InsertOutcome,
    NamePath,
};
use nlos_consensus::{
    ControlPlaneAuditCommand, ControlPlaneCommand, ControlPlaneProposal, MultiHostQuorumConfig,
    MultiHostQuorumNode, ValidatedVoterRoster, VoterReplica,
};
use nlos_identity::{
    BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose, semantic_signature_message,
};
use nlos_lease::clock_horizon::ClockSkewPolicy;
use nlos_lease::{CapacityLeaseState, ExclusiveDeviceLeaseState, QuotaLeaseState};
use nlos_net::{
    AuthorityCustodyPeerVerifier, ClusterProfile, CustodyPeerSigner, EndpointHintRecord,
    EndpointResolver, EndpointRoutingVersion, FenceNonceRegistry, MonotonicEndpointResolver,
    MonotonicPeerFenceGate, NetTransportConfig, NodeSecurityDeps, NodeTlsCredentials,
};
use nlos_operation::{OperationHandle, OperationSpec, OperationState};
use nlos_process::{
    CreateIsolationDomainRequest, DriverSessionAttachment, PinnedSessionPolicy,
    ProcessAuthorityError, ProcessBindingRecord, ProcessLifecycleState, ProcessMigrationStep,
    RegisterDelegatedProcessRequest, RegisterFiberIncarnationRequest,
};
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, RuntimeAdapter};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_schema::sabi::v1::{
    CellLeaseKind, CellLeaseLockState, CellReachabilityState, PartitionAutonomyGateState,
    SabiErrorCode,
};
use nlos_semantic::{
    AppendAssertionRequest, AssertionMode, LocalProcessRef, TaintFlags, UnsignedAssertionEvent,
    content_digest, encode_unsigned_assertion_event, semantic_event_id,
};
use nlos_slice_k::{
    CapabilityRebindItemSpec, CapacityLeaseMigrationSpec, CellHost, CellHostConfig,
    CellHostProcessMigrationRequest, CheckpointSourceSpec, CrossSocketMigrationError,
    CrossSocketProcessMigrationOptions, DEFAULT_MIGRATION_ARTIFACT_CHUNK_BYTES,
    DEFAULT_NET_CELL_TRUST_SNAPSHOT_DIGEST, DeviceLeaseMigrationSpec, LeaseMigrationBundleSpec,
    NetCellHost, NetCellHostNetworkConfig, NetConsensusBackend, ProcessLineageGenesis,
    ProcessLineageSpec, ProcessMigrationDriveError, QuorumSixEndpointTakeoverRequest,
    QuotaLeaseMigrationSpec, SourceFenceSpec, SourceQuiesceSpec, TargetRestoreSpec,
    VoterAttestationSigner, drive_cross_socket_process_migration,
    drive_quorum_backed_six_endpoint_takeover, exchange_peer_bootstrap_over_mtls,
    inspect_host_active_capability, inspect_remote_target_migration_state,
    publish_restart_image_artifact, reconcile_post_fence_interrupted_migration, seeded_key,
    setup_six_endpoint_incumbent_task_on_host,
};
use nlos_system_control::cell_inspector::{
    CellLeaseInspector, CellTopologyInspector, ProcessMigrationControlRequest,
};
use nlos_takeover_control::AdmittedCommitDisposition;
use nlos_task::{
    AuthorityAssignmentRecord, AuthorityAssignmentState, AuthorityLeaseBinding,
    AuthorityLeaseRecord, ParticipantRegistryBinding, ParticipantRegistryState,
};
use nlos_types::{
    ArtifactId, CallbackId, CancellationScopeId, CapacityLeaseId, ControlDomainId, DeviceId,
    ExclusiveDeviceLeaseId, ExecutionFiberId, Generation, IdempotencyKey, KeyId, NamespaceId,
    OperationId, PrincipalId, ProcessId, QuotaLeaseId, ReceiptId, ResourceGroupId,
    SchedulerDomainId, SemanticEventId, TaskAttemptId, TaskAuthorityAssignmentId, TaskId,
    TaskParticipantId,
};

const ENV_WORKER_ROLE: &str = "LLMOS_W71_WORKER_ROLE";
const ENV_HOST_ROOT: &str = "LLMOS_W71_HOST_ROOT";
const ENV_NODE_IDX: &str = "LLMOS_W71_NODE_IDX";

const PKI_VALID_FROM_MS: u64 = 1_700_000_000_000;
const PKI_CLOCK_MS: u64 = 1_800_000_000_000;
const PKI_VALID_UNTIL_MS: u64 = 4_102_444_800_000;

const SHARED_DEVICE_BYTES: [u8; 16] = [0xD4; 16];
const ARTIFACT_CONTENT_TYPE: &str = "application/vnd.llmos.process-restart-image+v1";
const CAP_NAME_PATH: &str = "/w71/migrated/capability";
const SEMANTIC_ISSUER_SEED: u8 = 0x51;

static TEMP_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-w71-xhost-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create isolated host temp dir");
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

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn hex_decode(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "hex string must have even length");
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().as_chunks::<2>().0 {
        let s = std::str::from_utf8(pair).expect("utf8 hex");
        out.push(u8::from_str_radix(s, 16).expect("valid hex byte"));
    }
    out
}

// ============================================================================
// Zero-Shared-Filesystem Per-Host mTLS Security Provisioner
// ============================================================================

/// Provisions this host's own private `SigningKeyCustodyVault` + `IdentityAuthority`
/// inside `<host_root>/<subdir>` and registers ONLY public `BootstrapPrincipalRequest`s
/// for remote peers (`peer_idx != node_idx`). Zero files or directories are
/// shared or copied across hosts.
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
    std::fs::create_dir_all(&id_dir).expect("create transport identity dir");
    std::fs::create_dir_all(&custody_dir).expect("create transport custody dir");

    let identity = Arc::new(IdentityAuthority::open(&id_dir).expect("open transport identity"));
    let vault = Arc::new(
        SigningKeyCustodyVault::open_durable(&custody_dir, [node_idx.wrapping_add(0x90); 32])
            .expect("open transport custody vault"),
    );

    let fence_gate = Arc::new(MonotonicPeerFenceGate::new(None));
    let mut my_binding = None;

    for peer_idx in 1..=cluster_size {
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
                .expect("bootstrap local custody principal");
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
                .expect("register remote peer public principal")
                .binding()
        };

        fence_gate
            .pin_cell_principal(cell_for_node(peer_idx), binding.principal_id)
            .expect("pin peer CellIdentity <-> PrincipalId");
    }

    let me = my_binding.expect("local node_idx must be in 1..=cluster_size");
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

fn peer_principal_id_for_node(peer_idx: u8) -> PrincipalId {
    let scratch = TempDir::new("principal-derive");
    let id = IdentityAuthority::open(scratch.path()).expect("open scratch identity");
    let seed = [peer_idx.wrapping_add(0x30); 32];
    let public_key = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    id.bootstrap_principal(BootstrapPrincipalRequest {
        principal_profile_digest: [peer_idx.wrapping_add(0x10); 32],
        control_domain_policy_digest: [peer_idx.wrapping_add(0x20); 32],
        public_key,
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: PKI_VALID_FROM_MS,
        key_valid_until_ms: PKI_VALID_UNTIL_MS,
        idempotency_key: IdempotencyKey::from_bytes([peer_idx.wrapping_add(0x40); 16]),
        created_at_ms: PKI_VALID_FROM_MS,
    })
    .expect("derive peer principal")
    .binding()
    .principal_id
}

// ============================================================================
// Parent <-> Child OS Process Anonymous Pipe Controller
// ============================================================================

struct SpawnedHostWorker {
    role: String,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    exited: bool,
}

impl SpawnedHostWorker {
    fn spawn(role: &str, host_root: &Path, node_idx: u8) -> Self {
        let exe = std::env::current_exe().expect("resolve current test executable");
        let mut child = Command::new(exe)
            .arg("child_net_cell_host_worker")
            .arg("--exact")
            .arg("--ignored")
            .arg("--nocapture")
            .env(ENV_WORKER_ROLE, role)
            .env(ENV_HOST_ROOT, host_root)
            .env(ENV_NODE_IDX, node_idx.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn child NetCellHost OS process");

        let stdin = child.stdin.take().expect("child stdin pipe");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout pipe"));
        Self {
            role: role.to_owned(),
            child,
            stdin: Some(stdin),
            stdout,
            exited: false,
        }
    }

    fn send_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("child stdin still open");
        writeln!(stdin, "{line}").expect("write control line to child stdin");
        stdin.flush().expect("flush child stdin");
    }

    fn recv_ctrl(&mut self) -> String {
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = self
                .stdout
                .read_line(&mut buf)
                .expect("read line from child stdout");
            assert!(
                n > 0,
                "child worker `{}` closed stdout unexpectedly before emitting CTRL message",
                self.role
            );
            let trimmed = buf.trim_end_matches(&['\r', '\n'][..]);
            if let Some(rest) = trimmed.strip_prefix("CTRL:") {
                return rest.to_owned();
            }
        }
    }

    fn wait_clean_exit(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait for child OS process");
        self.exited = true;
        assert!(
            status.success(),
            "child worker `{}` exited with non-zero status: {status}",
            self.role
        );
    }
}

impl Drop for SpawnedHostWorker {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if !self.exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read_parent_line(stdin: &mut BufReader<std::io::Stdin>) -> String {
    let mut line = String::new();
    let n = stdin.read_line(&mut line).expect("read parent stdin line");
    assert!(n > 0, "parent closed stdin pipe unexpectedly");
    line.trim_end_matches(&['\r', '\n'][..]).to_owned()
}

fn emit_ctrl(msg: &str) {
    println!("CTRL:{msg}");
    std::io::stdout().flush().expect("flush child stdout");
}

// ============================================================================
// Migration Fixture Helpers (Capabilities, Semantic Provenance, Process Setup)
// ============================================================================

struct SeededSourceMigrationEnv {
    proc_seed: u8,
    binding: ProcessBindingRecord,
    root_cap: CapabilityRecord,
    issuer_sk: SigningKey,
    holder_sk: SigningKey,
    issuer_bootstrap: BootstrapPrincipalRequest,
    holder_bootstrap: BootstrapPrincipalRequest,
    issuer_principal: PrincipalId,
    issuer_key_id: KeyId,
    holder_principal: PrincipalId,
    holder_key_id: KeyId,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn seed_migrating_process_on_source(
    source_host: &NetCellHost,
    adapter: &TokioRuntimeAdapter,
    proc_seed: u8,
    amplify_rights_on_delegate: bool,
    with_three_family_leases: bool,
    quota_face: u64,
    quota_spent: u64,
    cap_amount: u64,
    quota_id: QuotaLeaseId,
    cap_lease_id: CapacityLeaseId,
    dev_lease_id: ExclusiveDeviceLeaseId,
    fiber_id: ExecutionFiberId,
    op_id: OperationId,
    scope_id: CancellationScopeId,
) -> SeededSourceMigrationEnv {
    let binding = source_host.with_cell_host_mut(|ch| {
        ch.spawn_monitored_process(
            proc_seed,
            TaskId::from_bytes([proc_seed.wrapping_add(1); 16]),
            TaskAttemptId::from_bytes([proc_seed.wrapping_add(2); 16]),
            Generation::INITIAL,
        )
        .expect("spawn monitored process on source")
    });
    let base_ms = binding.created_at_ms + 1_000;

    source_host.with_cell_host(|ch| {
        ch.runtime()
            .process
            .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                process_id: binding.process_id,
                expected_process_generation: Generation::INITIAL,
                expected_process_fencing_token: binding.process_fencing_token,
                binding: fiber_id,
                idempotency_key: seeded_key(proc_seed, 200),
                registered_at_ms: binding.created_at_ms + 10,
            })
            .expect("register durable fiber incarnation");
    });

    let _fiber_handle = RuntimeAdapter::spawn_fiber(
        adapter,
        FiberSpec {
            fiber_id,
            fiber_generation: Generation::INITIAL,
            agent_instance_id: binding.agent_instance_id,
            agent_generation: binding.agent_instance_generation,
            process_id: binding.process_id,
            process_generation: binding.process_generation,
            task_attempt_id: Some(binding.task_attempt_id),
            cancellation_scope_id: scope_id,
            cancellation_generation: Generation::INITIAL,
            resource_group_id: ResourceGroupId::from_bytes([proc_seed.wrapping_add(3); 16]),
            scheduler_domain_id: domain_for_node(1),
            deadline: None,
        },
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            FiberExit::Cancelled
        }),
    )
    .expect("spawn live Tokio fiber on source");

    source_host.with_cell_host(|ch| {
        let op_handle = ch
            .runtime()
            .operations
            .register(OperationSpec {
                operation_id: op_id,
                generation: Generation::INITIAL,
                owner_fiber: FiberHandle {
                    fiber_id,
                    generation: Generation::INITIAL,
                },
                cancellation_scope_id: scope_id,
                cancellation_generation: Generation::INITIAL,
            })
            .expect("register inflight operation")
            .handle();
        let _ticket = ch
            .runtime()
            .operations
            .dispatch(
                op_handle,
                CallbackId::from_bytes([proc_seed.wrapping_add(4); 16]),
            )
            .expect("dispatch inflight operation");
    });

    let issuer_seed = proc_seed.wrapping_add(0x11);
    let holder_seed = proc_seed.wrapping_add(0x22);
    let issuer_sk = SigningKey::from_bytes(&[issuer_seed; 32]);
    let holder_sk = SigningKey::from_bytes(&[holder_seed; 32]);
    let issuer_bootstrap = BootstrapPrincipalRequest {
        principal_profile_digest: [issuer_seed.wrapping_add(1); 32],
        control_domain_policy_digest: [issuer_seed.wrapping_add(2); 32],
        public_key: issuer_sk.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 0,
        key_valid_until_ms: base_ms + 86_400_000,
        idempotency_key: seeded_key(proc_seed, 11),
        created_at_ms: base_ms + 100,
    };
    let holder_bootstrap = BootstrapPrincipalRequest {
        principal_profile_digest: [holder_seed.wrapping_add(1); 32],
        control_domain_policy_digest: [holder_seed.wrapping_add(2); 32],
        public_key: holder_sk.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 0,
        key_valid_until_ms: base_ms + 86_400_000,
        idempotency_key: seeded_key(proc_seed, 12),
        created_at_ms: base_ms + 100,
    };

    let (root_cap, issuer_principal, issuer_key_id, holder_principal, holder_key_id) = source_host
        .with_cell_host_mut(|ch| {
            let issuer_b = ch
                .runtime()
                .identity
                .bootstrap_principal(issuer_bootstrap)
                .expect("bootstrap capability issuer")
                .binding();
            let holder_b = ch
                .runtime()
                .identity
                .bootstrap_principal(holder_bootstrap)
                .expect("bootstrap capability holder")
                .binding();

            let parent_rights = if amplify_rights_on_delegate {
                CapabilityRights::DELEGATE
            } else {
                CapabilityRights::DELEGATE.union(CapabilityRights::SEMANTIC_APPEND)
            };
            let root_cmd = IssueRootCapabilityRequest {
                issuer_key_id: issuer_b.key_id,
                holder_key_id: holder_b.key_id,
                target: CapabilityTarget::Namespace(NamespaceId::from_bytes(
                    [proc_seed.wrapping_add(9); 16],
                )),
                rights: parent_rights,
                purpose_digest: Some([0xAA; 32]),
                valid_from_ms: base_ms + 100,
                valid_until_ms: base_ms + 500_000,
                delegation_depth_remaining: 2,
                call_limit: Some(10),
                idempotency_key: seeded_key(proc_seed, 13),
                issued_at_ms: base_ms + 150,
            };
            let signed_root = SignedIssueRootCapabilityRequest {
                command: root_cmd,
                signer: issuer_b.principal_id,
                signature: issuer_sk
                    .sign(&issue_root_command_message(root_cmd))
                    .to_bytes(),
            };
            let rec = ch
                .runtime()
                .capability()
                .issue_root_signed(&ch.runtime().identity, signed_root)
                .expect("issue root capability on source")
                .record();

            if !amplify_rights_on_delegate {
                let cap_path = NamePath::new(CAP_NAME_PATH).expect("cap name path");
                assert_eq!(
                    ch.insert_cached_name(cap_path, rec.handle.capability_id, Generation::INITIAL)
                        .expect("cache cap path on source"),
                    InsertOutcome::Inserted
                );
            }
            (
                rec,
                issuer_b.principal_id,
                issuer_b.key_id,
                holder_b.principal_id,
                holder_b.key_id,
            )
        });

    source_host
        .grant_and_track_quota_lease(quota_id, quota_face, None, binding.process_id, 60_000, true)
        .expect("grant & track source quota lease");
    if quota_spent > 0 {
        source_host.with_cell_host_mut(|ch| {
            ch.report_quota_usage(quota_id, quota_spent)
                .expect("report source quota spend");
        });
    }
    if with_three_family_leases {
        source_host
            .grant_and_track_capacity_lease(cap_lease_id, cap_amount, binding.process_id, 60_000)
            .expect("grant & track source capacity lease");
        source_host
            .grant_and_track_device_lease(dev_lease_id, binding.process_id, 60_000)
            .expect("grant & track source device lease");
    }

    SeededSourceMigrationEnv {
        proc_seed,
        binding,
        root_cap,
        issuer_sk,
        holder_sk,
        issuer_bootstrap,
        holder_bootstrap,
        issuer_principal,
        issuer_key_id,
        holder_principal,
        holder_key_id,
    }
}

#[allow(clippy::too_many_lines)]
fn append_two_event_causal_provenance_chain_on_source(
    source_host: &NetCellHost,
    admitted_base_ms: u64,
) -> (SemanticEventId, SemanticEventId) {
    let (issuer_sk, issuer_boot) = semantic_issuer_bootstrap();
    source_host.with_cell_host(|ch| {
        let rt = ch.runtime();
        let issuer_b = rt
            .identity
            .bootstrap_principal(issuer_boot)
            .expect("bootstrap semantic provenance issuer")
            .binding();
        let iso = rt
            .process
            .create_isolation_domain(CreateIsolationDomainRequest {
                policy_digest: [0x5B; 32],
                idempotency_key: IdempotencyKey::from_bytes([0x5C; 16]),
                created_at_ms: admitted_base_ms - 500,
            })
            .expect("create semantic issuer isolation domain")
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
                created_at_ms: admitted_base_ms - 400,
            })
            .expect("register semantic issuer process")
            .record()
            .clone();

        let cap_cmd = IssueRootCapabilityRequest {
            issuer_key_id: issuer_b.key_id,
            holder_key_id: issuer_b.key_id,
            target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x59; 16])),
            rights: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: Some([0x5A; 32]),
            valid_from_ms: PKI_VALID_FROM_MS,
            valid_until_ms: PKI_VALID_UNTIL_MS,
            delegation_depth_remaining: 0,
            call_limit: None,
            idempotency_key: IdempotencyKey::from_bytes([0x60; 16]),
            issued_at_ms: admitted_base_ms - 300,
        };
        let cap_rec = rt
            .capability()
            .issue_root_signed(
                &rt.identity,
                SignedIssueRootCapabilityRequest {
                    command: cap_cmd,
                    signer: issuer_b.principal_id,
                    signature: issuer_sk
                        .sign(&issue_root_command_message(cap_cmd))
                        .to_bytes(),
                },
            )
            .expect("issue semantic append root capability")
            .record();

        let media_type = "text/plain".to_owned();
        let body_1 = b"w71-causal-parent-assertion-v1";
        let ev1 = UnsignedAssertionEvent {
            scope: cap_rec.target,
            issuer: issuer_b.principal_id,
            issuer_execution: LocalProcessRef {
                process_id: proc_rec.process_id,
                generation: proc_rec.process_generation,
            },
            control_domain: issuer_b.control_domain_id,
            issued_at_unix_ns: admitted_base_ms * 1_000_000,
            nonce: vec![0xE1; 16],
            declared_parents: Vec::new(),
            declassification_receipt_id: None,
            valid_until_ms: Some(PKI_VALID_UNTIL_MS),
            purpose_digest: cap_rec.purpose_digest,
            content_digest: content_digest(&media_type, body_1).expect("digest 1"),
            assertion_mode: AssertionMode::Inference,
            execution_evidence_receipt_id: None,
            confidence_bp: Some(9_200),
            key_id: issuer_b.key_id,
        };
        let cbor_1 = encode_unsigned_assertion_event(&ev1).expect("encode ev1");
        let id_1 = semantic_event_id(&cbor_1);
        let sig_1 = issuer_sk.sign(&semantic_signature_message(id_1)).to_bytes();
        rt.semantic()
            .append_assertion(
                &rt.identity,
                rt.capability(),
                &rt.process,
                rt.semantic_writer(),
                &AppendAssertionRequest {
                    canonical_unsigned_event: cbor_1,
                    claimed_event_id: id_1,
                    signature: sig_1,
                    capability: cap_rec.handle,
                    content_media_type: media_type.clone(),
                    content_bytes: body_1.to_vec(),
                    captured_inputs: Vec::new(),
                    ingress_taint: TaintFlags::default(),
                    authz_policy_digest: [0x99; 32],
                    admission_limit_ms: Some(PKI_VALID_UNTIL_MS - 1_000),
                    admitted_at_ms: admitted_base_ms,
                },
            )
            .expect("append causal root semantic assertion e1");

        let body_2 = b"w71-causal-child-assertion-v2";
        let ev2 = UnsignedAssertionEvent {
            scope: cap_rec.target,
            issuer: issuer_b.principal_id,
            issuer_execution: LocalProcessRef {
                process_id: proc_rec.process_id,
                generation: proc_rec.process_generation,
            },
            control_domain: issuer_b.control_domain_id,
            issued_at_unix_ns: (admitted_base_ms + 5) * 1_000_000,
            nonce: vec![0xE2; 16],
            declared_parents: vec![id_1],
            declassification_receipt_id: None,
            valid_until_ms: Some(PKI_VALID_UNTIL_MS),
            purpose_digest: cap_rec.purpose_digest,
            content_digest: content_digest(&media_type, body_2).expect("digest 2"),
            assertion_mode: AssertionMode::Inference,
            execution_evidence_receipt_id: None,
            confidence_bp: Some(9_600),
            key_id: issuer_b.key_id,
        };
        let cbor_2 = encode_unsigned_assertion_event(&ev2).expect("encode ev2");
        let id_2 = semantic_event_id(&cbor_2);
        let sig_2 = issuer_sk.sign(&semantic_signature_message(id_2)).to_bytes();
        rt.semantic()
            .append_assertion(
                &rt.identity,
                rt.capability(),
                &rt.process,
                rt.semantic_writer(),
                &AppendAssertionRequest {
                    canonical_unsigned_event: cbor_2,
                    claimed_event_id: id_2,
                    signature: sig_2,
                    capability: cap_rec.handle,
                    content_media_type: media_type,
                    content_bytes: body_2.to_vec(),
                    captured_inputs: vec![id_1],
                    ingress_taint: TaintFlags::default(),
                    authz_policy_digest: [0x99; 32],
                    admission_limit_ms: Some(PKI_VALID_UNTIL_MS - 1_000),
                    admitted_at_ms: admitted_base_ms + 5,
                },
            )
            .expect("append causal child semantic assertion e2");

        (id_1, id_2)
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn build_migration_request_from_env(
    env: &SeededSourceMigrationEnv,
    migration_id: [u8; 16],
    target_cell_id: CellIdentity,
    attached_sessions: Vec<DriverSessionAttachment>,
    pinned_session_policy: PinnedSessionPolicy,
    checkpoint: CheckpointSourceSpec,
    amplify_rights_on_delegate: bool,
    leases: &LeaseMigrationBundleSpec,
    op_id: OperationId,
    scope_id: CancellationScopeId,
) -> CellHostProcessMigrationRequest {
    let base_ms = env.binding.created_at_ms + 1_000;
    let parent_rights = if amplify_rights_on_delegate {
        CapabilityRights::DELEGATE
    } else {
        CapabilityRights::DELEGATE.union(CapabilityRights::SEMANTIC_APPEND)
    };
    let root_cmd = IssueRootCapabilityRequest {
        issuer_key_id: env.issuer_key_id,
        holder_key_id: env.holder_key_id,
        target: env.root_cap.target,
        rights: parent_rights,
        purpose_digest: env.root_cap.purpose_digest,
        valid_from_ms: env.root_cap.valid_from_ms,
        valid_until_ms: env.root_cap.valid_until_ms,
        delegation_depth_remaining: env.root_cap.delegation_depth_remaining,
        call_limit: env.root_cap.call_limit,
        idempotency_key: seeded_key(env.proc_seed, 13),
        issued_at_ms: base_ms + 150,
    };
    let signed_root = SignedIssueRootCapabilityRequest {
        command: root_cmd,
        signer: env.issuer_principal,
        signature: env
            .issuer_sk
            .sign(&issue_root_command_message(root_cmd))
            .to_bytes(),
    };

    let child_rights = if amplify_rights_on_delegate {
        CapabilityRights::DELEGATE.union(CapabilityRights::SEMANTIC_APPEND)
    } else {
        CapabilityRights::SEMANTIC_APPEND
    };
    let delegate_cmd = DelegateCapabilityRequest {
        parent: env.root_cap.handle,
        delegator_key_id: env.holder_key_id,
        recipient_key_id: env.holder_key_id,
        target: env.root_cap.target,
        rights: child_rights,
        purpose_digest: env.root_cap.purpose_digest,
        valid_from_ms: base_ms + 200,
        valid_until_ms: base_ms + 400_000,
        delegation_depth_remaining: 1,
        call_limit: Some(5),
        idempotency_key: seeded_key(migration_id[0], 32),
        delegated_at_ms: base_ms + 210,
    };
    let signed_delegation = SignedDelegateCapabilityRequest {
        command: delegate_cmd,
        signer: env.holder_principal,
        signature: env
            .holder_sk
            .sign(&delegate_command_message(delegate_cmd))
            .to_bytes(),
    };

    let revoke_cmd = RevokeCapabilityRequest {
        handle: env.root_cap.handle,
        revoker_key_id: env.issuer_key_id,
        idempotency_key: seeded_key(migration_id[0], 33),
        revoked_at_ms: base_ms + 300,
    };
    let signed_revoke = SignedRevokeCapabilityRequest {
        command: revoke_cmd,
        signer: env.issuer_principal,
        signature: env
            .issuer_sk
            .sign(&revoke_command_message(revoke_cmd))
            .to_bytes(),
    };

    let cap_name_path = if amplify_rights_on_delegate {
        None
    } else {
        Some(NamePath::new(CAP_NAME_PATH).expect("cap name path"))
    };

    CellHostProcessMigrationRequest {
        migration_id,
        process_id: env.binding.process_id,
        source_cell_id: cell_for_node(1),
        target_cell_id,
        source_generation: Generation::INITIAL,
        lineage: ProcessLineageSpec {
            genesis: ProcessLineageGenesis::from_spawned_binding(env.proc_seed, &env.binding),
            prior_restores: Vec::new(),
        },
        attached_sessions,
        pinned_session_policy,
        custom_intent_reason: None,
        initiated_at_ms: base_ms + 180,
        quiesce: SourceQuiesceSpec {
            cancel_scope: Some((scope_id, Generation::INITIAL)),
            active_fiber_count: 1,
            inflight_operations: vec![(
                OperationHandle {
                    operation_id: op_id,
                    generation: Generation::INITIAL,
                },
                ReceiptId::from_bytes([migration_id[0].wrapping_add(7); 16]),
            )],
            quiesced_at_ms: base_ms + 190,
        },
        checkpoint,
        artifact_sync_seed: migration_id[0],
        source_fence: SourceFenceSpec {
            invalidate_name_paths: cap_name_path.iter().cloned().collect(),
            terminate_idempotency_key: seeded_key(migration_id[0], 34),
            stale_dispatch_probe_fiber_id: ExecutionFiberId::from_bytes([0x77; 16]),
            stale_dispatch_probe_key: seeded_key(migration_id[0], 35),
            fenced_at_ms: base_ms + 205,
        },
        capabilities: vec![CapabilityRebindItemSpec {
            bootstrap_principals: vec![env.issuer_bootstrap, env.holder_bootstrap],
            seed_parent_root: Some(signed_root),
            signed_delegation,
            target_name_path: cap_name_path,
            revoke_on_source: Some(signed_revoke),
        }],
        capabilities_rebound_at_ms: base_ms + 220,
        leases: *leases,
        target_restore: TargetRestoreSpec {
            existing_target_domain_id: None,
            domain_policy_digest: [migration_id[0].wrapping_add(0x41); 32],
            domain_idempotency_key: seeded_key(migration_id[0], 36),
            restore_idempotency_key: seeded_key(migration_id[0], 38),
            restored_at_ms: base_ms + 260,
        },
        completed_at_ms: base_ms + 280,
    }
}

async fn bind_isolated_networked_host(
    host_root: &Path,
    node_idx: u8,
    cluster_size: u8,
    consensus: NetConsensusBackend,
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

    let host = NetCellHost::bind_networked(
        host_root,
        NetCellHostNetworkConfig {
            cell_config: cell_host_config(node_idx),
            cell_label: format!("w71-host-{node_idx}"),
            service_bind_addr: loopback_ephemeral(),
            listener_security,
            client_security,
            transport_config: NetTransportConfig::default(),
            skew_policy: skew_policy(),
            trust_snapshot_digest: DEFAULT_NET_CELL_TRUST_SNAPSHOT_DIGEST,
            artifact_policies: vec![restart_artifact_policy()],
            remote_lease_server: None,
            consensus,
        },
    )
    .await
    .expect("bind isolated NetCellHost");

    let _ = live_slot.set(Arc::downgrade(host.cell_host_arc()));
    host
}

fn open_single_host_consensus(host_root: &Path, node_idx: u8) -> NetConsensusBackend {
    let node = nlos_consensus::SingleHostPassthroughNode::open(
        host_root.join("local-consensus"),
        cell_for_node(node_idx),
        control_domain_for_node(node_idx),
    )
    .expect("open SingleHostPassthroughNode");
    NetConsensusBackend::SingleHostPassthrough(Box::new(node))
}

// ============================================================================
// Wire Helpers for Passing Initial Takeover Metadata Over Pipe (`Host 1 -> Host 2`)
// ============================================================================

fn encode_incumbent_takeover_meta(
    lease: &AuthorityLeaseRecord,
    assignment: &AuthorityAssignmentRecord,
) -> String {
    let mut buf = Vec::with_capacity(224);
    buf.extend_from_slice(lease.authority_id.as_bytes());
    buf.extend_from_slice(lease.holder_id.as_bytes());
    buf.extend_from_slice(&lease.term.to_be_bytes());
    buf.extend_from_slice(&lease.lease_epoch.to_be_bytes());
    buf.extend_from_slice(&lease.fencing_token);
    buf.extend_from_slice(&lease.requested_at_ms.to_be_bytes());
    buf.extend_from_slice(&lease.expires_at_ms.to_be_bytes());
    buf.extend_from_slice(&lease.ttl_ms.to_be_bytes());
    buf.extend_from_slice(lease.idempotency_key.as_bytes());

    buf.extend_from_slice(assignment.assignment_id.as_bytes());
    buf.extend_from_slice(assignment.task_id.as_bytes());
    buf.extend_from_slice(&assignment.task_generation.get().to_be_bytes());
    buf.extend_from_slice(&assignment.control_epoch.to_be_bytes());
    buf.extend_from_slice(
        &assignment
            .participant_registry_binding
            .generation
            .to_be_bytes(),
    );
    buf.extend_from_slice(&assignment.participant_registry_binding.root);
    buf.extend_from_slice(&assignment.created_at_ms.to_be_bytes());
    buf.extend_from_slice(&assignment.updated_at_ms.to_be_bytes());
    hex_encode(&buf)
}

fn decode_incumbent_takeover_meta(hex: &str) -> (AuthorityLeaseRecord, AuthorityAssignmentRecord) {
    let raw = hex_decode(hex);
    assert_eq!(
        raw.len(),
        224,
        "encoded takeover metadata must be 224 bytes"
    );
    let arr16 = |off: usize| -> [u8; 16] { raw[off..off + 16].try_into().expect("16 bytes") };
    let arr32 = |off: usize| -> [u8; 32] { raw[off..off + 32].try_into().expect("32 bytes") };
    let u64_at =
        |off: usize| -> u64 { u64::from_be_bytes(raw[off..off + 8].try_into().expect("8 bytes")) };
    let i64_at =
        |off: usize| -> i64 { i64::from_be_bytes(raw[off..off + 8].try_into().expect("8 bytes")) };

    let lease = AuthorityLeaseRecord {
        authority_id: TaskParticipantId::from_bytes(arr16(0)),
        holder_id: ProcessId::from_bytes(arr16(16)),
        term: u64_at(32),
        lease_epoch: u64_at(40),
        fencing_token: arr32(48),
        requested_at_ms: i64_at(80),
        expires_at_ms: i64_at(88),
        ttl_ms: i64_at(96),
        idempotency_key: IdempotencyKey::from_bytes(arr16(104)),
    };
    let binding = AuthorityLeaseBinding {
        authority_id: lease.authority_id,
        holder_id: lease.holder_id,
        term: lease.term,
        lease_epoch: lease.lease_epoch,
        fencing_token: lease.fencing_token,
        expires_at_ms: lease.expires_at_ms,
    };
    let assignment = AuthorityAssignmentRecord {
        assignment_id: TaskAuthorityAssignmentId::from_bytes(arr16(120)),
        task_id: TaskId::from_bytes(arr16(136)),
        task_generation: Generation::new(NonZeroU64::new(u64_at(152)).expect("non-zero gen")),
        authority_lease_binding: binding,
        control_epoch: u64_at(160),
        participant_registry_binding: ParticipantRegistryBinding {
            generation: u64_at(168),
            root: arr32(176),
        },
        state: AuthorityAssignmentState::Active,
        created_at_ms: i64_at(208),
        updated_at_ms: i64_at(216),
    };
    (lease, assignment)
}

// ============================================================================
// Child Worker Implementations (Invoked Inside Isolated OS Processes)
// ============================================================================

#[allow(clippy::too_many_lines)]
async fn run_worker_n1_desktop(host_root: &Path, node_idx: u8) {
    let host = NetCellHost::open_single_host_desktop(
        host_root,
        cell_host_config(node_idx),
        control_domain_for_node(node_idx),
        skew_policy(),
    )
    .expect("open N=1 SingleHostDesktop NetCellHost");

    // 1. `[NLOS-GOAL-001]` / `ADR-0026` §6.3: zero sockets, zero timers, zero TLS!
    assert_eq!(host.cluster_profile(), ClusterProfile::SingleHostDesktop);
    assert!(host.service_addr().is_none());
    assert_eq!(host.active_socket_count(), 0);
    assert_eq!(host.active_background_timer_count(), 0);
    assert!(!host.requires_tls_certificates());

    // 2. Local SQLite WAL+FULL passthrough commit with 0 network overhead.
    let proposal = ControlPlaneProposal::new(
        IdempotencyKey::from_bytes([0x11; 16]),
        ControlPlaneCommand::ControlPlaneAuditAnchor(
            ControlPlaneAuditCommand::AnchorReconciliationCheckpoint {
                cell: host.local_cell(),
                node_boot_generation: Generation::INITIAL,
                epoch: CellEpoch::INITIAL,
                checkpoint_seq: NonZeroU64::MIN,
                checkpoint_digest: [0xAB; 32],
            },
        ),
    );
    let commit = host
        .consensus()
        .propose(proposal, Duration::from_secs(2))
        .await
        .expect("N=1 passthrough commit");
    assert_eq!(commit.consensus_commit_index.get(), 1);
    assert_eq!(host.active_socket_count(), 0);
    assert_eq!(host.active_background_timer_count(), 0);

    // 3. Stage D `CellTopologyInspector` & `CellLeaseInspector` read-only projections.
    let binding = host.with_cell_host_mut(|ch| {
        ch.spawn_monitored_process(
            0x31,
            TaskId::from_bytes([0x32; 16]),
            TaskAttemptId::from_bytes([0x33; 16]),
            Generation::INITIAL,
        )
        .expect("spawn local process")
    });

    let q_id = QuotaLeaseId::from_bytes([0x41; 16]);
    let c_id = CapacityLeaseId::from_bytes([0x42; 16]);
    let d_id = ExclusiveDeviceLeaseId::from_bytes([0x43; 16]);

    // Quota: Active (TTL well above skew horizon)
    host.grant_and_track_quota_lease(q_id, 200, None, binding.process_id, 60_000, true)
        .expect("grant active quota lease");
    // Capacity: Expiring (`nominal_remaining_ttl_ms = 1 <= clock_skew_horizon_ms`)
    host.grant_and_track_capacity_lease(c_id, 100, binding.process_id, 1)
        .expect("grant expiring capacity lease");
    // ExclusiveDevice: first verify FullAutonomy before quarantine, then Quarantined after.
    host.grant_and_track_device_lease(d_id, binding.process_id, 60_000)
        .expect("grant device lease");
    let pre_quarantine_cells = host
        .list_cells(None, 16)
        .expect("list_cells pre-quarantine");
    assert_eq!(pre_quarantine_cells.cells.len(), 1);
    assert_eq!(
        pre_quarantine_cells.cells[0].partition_autonomy_gate,
        PartitionAutonomyGateState::FullAutonomy
    );
    assert_eq!(pre_quarantine_cells.cells[0].active_lease_count, 3);

    host.with_cell_host_mut(|ch| {
        ch.quarantine_device_lease(d_id)
            .expect("quarantine device lease");
    });

    let cells_page = host.list_cells(None, 16).expect("list_cells");
    assert_eq!(cells_page.cells.len(), 1);
    let local_summary = &cells_page.cells[0];
    assert_eq!(local_summary.cell_id, *host.local_cell().as_bytes());
    assert_eq!(
        local_summary.reachability,
        CellReachabilityState::OnlineLocal
    );
    assert_eq!(
        local_summary.partition_autonomy_gate,
        PartitionAutonomyGateState::Quarantined
    );
    assert_eq!(local_summary.active_lease_count, 2);

    let leases_page = host
        .inspect_cell_leases(None, 16)
        .expect("inspect_cell_leases");
    assert_eq!(leases_page.leases.len(), 3);
    let quota_row = leases_page
        .leases
        .iter()
        .find(|r| r.lease_id == *q_id.as_bytes())
        .expect("quota row");
    assert_eq!(quota_row.kind, CellLeaseKind::Quota);
    assert_eq!(quota_row.lock_state, CellLeaseLockState::Active);
    assert!(quota_row.clock_skew_horizon_ms > 0);

    let cap_row = leases_page
        .leases
        .iter()
        .find(|r| r.lease_id == *c_id.as_bytes())
        .expect("capacity row");
    assert_eq!(cap_row.kind, CellLeaseKind::Capacity);
    assert_eq!(cap_row.lock_state, CellLeaseLockState::Expiring);

    let dev_row = leases_page
        .leases
        .iter()
        .find(|r| r.lease_id == *d_id.as_bytes())
        .expect("device row");
    assert_eq!(dev_row.kind, CellLeaseKind::ExclusiveDevice);
    assert_eq!(dev_row.lock_state, CellLeaseLockState::Quarantined);

    // `request_process_migration` MUST fail closed in `N = 1` SingleHostDesktop mode.
    let mig_err = host
        .request_process_migration(ProcessMigrationControlRequest {
            process_id: *binding.process_id.as_bytes(),
            destination_cell_id: [0xFE; 16],
            expected_generation_or_revision: 1,
            issuer_principal_id: [0x11; 16],
            idempotency_key: [0x22; 16],
            requested_at_ms: 1_000,
        })
        .expect_err("N=1 desktop migration request must fail closed");
    assert_eq!(mig_err.code, i32::from(SabiErrorCode::NotSupported));

    host.shutdown().await;
    emit_ctrl("PASS");
}

#[allow(clippy::too_many_lines)]
async fn run_worker_mig_happy_source(host_root: &Path, node_idx: u8) {
    let mut stdin = BufReader::new(std::io::stdin());
    let adapter = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("TokioRuntimeAdapter");
    let consensus = open_single_host_consensus(host_root, node_idx);
    let source_host = bind_isolated_networked_host(host_root, node_idx, 2, consensus).await;
    let svc_addr = source_host.service_addr().expect("bound service addr");
    emit_ctrl(&format!("READY:{svc_addr}"));

    let peer_line = read_parent_line(&mut stdin);
    let target_addr: SocketAddr = peer_line
        .strip_prefix("PEER:")
        .expect("PEER:<addr>")
        .parse()
        .expect("parse target SocketAddr");
    let target_cell = cell_for_node(2);

    exchange_peer_bootstrap_over_mtls(
        &source_host,
        target_cell,
        target_addr,
        EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 1),
        IdempotencyKey::from_bytes([0x01; 16]),
    )
    .await
    .expect("exchange peer bootstrap over mTLS");

    // Seed primary migrating process with 3-family leases (`Quota` 200 w/ 50 spent -> 150
    // returned, `Capacity` 120, `ExclusiveDevice`), live fiber + inflight op, and capability.
    let q_src = QuotaLeaseId::from_bytes([0x71; 16]);
    let q_dst = QuotaLeaseId::from_bytes([0x72; 16]);
    let c_src = CapacityLeaseId::from_bytes([0x73; 16]);
    let c_dst = CapacityLeaseId::from_bytes([0x74; 16]);
    let d_src = ExclusiveDeviceLeaseId::from_bytes([0x75; 16]);
    let d_dst = ExclusiveDeviceLeaseId::from_bytes([0x76; 16]);
    let fiber_1 = ExecutionFiberId::from_bytes([0x11; 16]);
    let op_1 = OperationId::from_bytes([0x61; 16]);
    let scope_1 = CancellationScopeId::from_bytes([0x51; 16]);

    let env = seed_migrating_process_on_source(
        &source_host,
        &adapter,
        0xA5,
        false,
        true,
        200,
        50,
        120,
        q_src,
        c_src,
        d_src,
        fiber_1,
        op_1,
        scope_1,
    );
    let base_ms = env.binding.created_at_ms + 1_000;

    // Publish a multi-chunk (`70 KiB` -> `5` chunks at `16 KiB` chunk size) restart
    // artifact on Source AND append a 2-event causal semantic provenance chain (`e1 -> e2`).
    let restart_artifact_id = ArtifactId::from_bytes([0x81; 16]);
    let mut multi_chunk_image = vec![0u8; 70 * 1024];
    for (i, byte) in multi_chunk_image.iter_mut().enumerate() {
        *byte = u8::try_from((i * 31 + 7) % 251).expect("byte < 251");
    }
    let (artifact_rev, artifact_digest) = source_host.with_cell_host(|ch| {
        publish_restart_image_artifact(
            ch,
            restart_artifact_id,
            0x88,
            ARTIFACT_CONTENT_TYPE,
            &multi_chunk_image,
            base_ms + 25,
        )
        .expect("publish 70 KiB restart image artifact on source")
    });
    let (_e1, _e2) = append_two_event_causal_provenance_chain_on_source(&source_host, base_ms);

    let attached_pinned = vec![
        DriverSessionAttachment::Migratable {
            session_id: [0x11; 16],
            state_digest: [0x12; 32],
        },
        DriverSessionAttachment::PinnedNonMigratable {
            session_id: [0x21; 16],
            pin_reason: "gpu-vram-pinned-handle".to_owned(),
            artifact_restart_image_digest: Some(artifact_digest.into_bytes()),
        },
    ];
    let leases_bundle = LeaseMigrationBundleSpec {
        quota: QuotaLeaseMigrationSpec {
            source_lease_id: q_src,
            gateway_id: [0x81; 16],
            override_fence_ack: None,
            target_lease_id: q_dst,
            target_face_value: None,
            target_expires_at: None,
            activate_on_target: true,
        },
        capacity: Some(CapacityLeaseMigrationSpec {
            source_lease_id: c_src,
            reconcile_receipt_id: ReceiptId::from_bytes([0x82; 16]),
            gateway_id: [0x83; 16],
            target_lease_id: c_dst,
            target_amount: None,
        }),
        device: Some(DeviceLeaseMigrationSpec {
            source_lease_id: d_src,
            reset_receipt_id: ReceiptId::from_bytes([0x84; 16]),
            reconcile_receipt_id: ReceiptId::from_bytes([0x85; 16]),
            gateway_id: [0x86; 16],
            target_lease_id: d_dst,
            advance_target_epoch_if_needed: true,
        }),
        regranted_at_ms: base_ms + 240,
    };

    // ------------------------------------------------------------------------
    // Sub-case A (`[DIST-MIGRATE-002]`): `PinnedSessionPolicy::RefuseMigration`
    // MUST abort pre-fence at `Initiated`; Source stays Active & untouched!
    // ------------------------------------------------------------------------
    let refuse_req = build_migration_request_from_env(
        &env,
        [0x41; 16],
        target_cell,
        attached_pinned.clone(),
        PinnedSessionPolicy::RefuseMigration,
        CheckpointSourceSpec::ArtifactRestartImage {
            artifact_id: restart_artifact_id,
            revision: artifact_rev,
            sync_from_source_if_missing: true,
            checkpointed_at_ms: base_ms + 200,
        },
        false,
        &leases_bundle,
        op_1,
        scope_1,
    );
    let refuse_err = drive_cross_socket_process_migration(
        &source_host,
        Some(&adapter),
        refuse_req,
        CrossSocketProcessMigrationOptions::default(),
    )
    .await
    .expect_err("PinnedNonMigratable + RefuseMigration must abort pre-fence");

    match refuse_err {
        CrossSocketMigrationError::Drive(inner) => match *inner {
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::Initiated,
                source: nlos_process::ProcessMigrationError::PinnedSessionNonMigratable { .. },
            } => {}
            other => panic!(
                "expected pre-fence Protocol(Initiated, PinnedSessionNonMigratable), got {other:?}"
            ),
        },
        other => panic!("expected Drive(Protocol), got {other:?}"),
    }

    source_host.with_cell_host(|ch| {
        let still_active = ch
            .runtime()
            .process
            .inspect_active_process_binding(env.binding.process_id)
            .expect("source process must remain Active after RefuseMigration");
        assert_eq!(still_active.process_generation, Generation::INITIAL);
        assert!(ch.supervisor_pids().lookup(env.binding.process_id).is_ok());
        assert_eq!(
            ch.query_quota_lease(q_src).expect("quota").state(),
            QuotaLeaseState::Active
        );
    });
    let target_after_refuse = inspect_remote_target_migration_state(
        &source_host,
        target_cell,
        env.binding.process_id,
        IdempotencyKey::from_bytes([0x42; 16]),
        5_000,
    )
    .await
    .expect("inspect target after RefuseMigration");
    assert_eq!(target_after_refuse.active_generation, None);
    assert!(!target_after_refuse.supervisor_registered);

    // ------------------------------------------------------------------------
    // Sub-case B (`[DIST-MIGRATE-001/002]`, `[LEASE-MIGRATE-001]`, `[DIST-ART-001]`,
    // `[SEM-CHECKPOINT-001]`): `DowngradeToRestartFromArtifact` streams 5 chunks
    // (`70 KiB` at `16 KiB` chunks), pulls & verifies the 2-event causal semantic
    // provenance chain, closes 3-family leases on Source before Target regrant,
    // rebinds attenuated capabilities, and restores `gen = 2` on Target!
    // ------------------------------------------------------------------------
    let downgrade_req = build_migration_request_from_env(
        &env,
        [0x44; 16],
        target_cell,
        attached_pinned,
        PinnedSessionPolicy::DowngradeToRestartFromArtifact,
        CheckpointSourceSpec::ArtifactRestartImage {
            artifact_id: restart_artifact_id,
            revision: artifact_rev,
            sync_from_source_if_missing: true,
            checkpointed_at_ms: base_ms + 200,
        },
        false,
        &leases_bundle,
        op_1,
        scope_1,
    );
    let outcome = drive_cross_socket_process_migration(
        &source_host,
        Some(&adapter),
        downgrade_req,
        CrossSocketProcessMigrationOptions {
            artifact_chunk_payload_bytes: DEFAULT_MIGRATION_ARTIFACT_CHUNK_BYTES,
            sync_semantic_provenance: true,
            semantic_max_batch_events: 1,
            rpc_deadline_ms: 5_000,
            after_source_fenced_hook: None,
        },
    )
    .await
    .expect("8-step cross-socket process migration over mTLS must succeed");

    assert_eq!(
        outcome.coordinator.current_step(),
        ProcessMigrationStep::Completed
    );
    assert!(outcome.receipt.verify_digest());
    assert!(outcome.target_checkpoint_summary.verify_digest());
    assert!(outcome.consensus_audit_commit.consensus_commit_index.get() >= 1);

    let art_summary = outcome
        .artifact_transfer_summary
        .expect("restart artifact must stream over chunked mTLS bulk transfer");
    assert_eq!(art_summary.chunks_transmitted, 5);
    assert_eq!(art_summary.payload_bytes_transmitted, 70 * 1024);

    let sem_summary = outcome
        .semantic_sync_summary
        .expect("semantic provenance delta sync must run");
    assert_eq!(sem_summary.batches_pulled, 2);
    assert_eq!(sem_summary.pulled_event_count, 2);
    assert_eq!(sem_summary.imported_events, 2);
    assert_eq!(
        sem_summary.target_checkpoint_digest,
        sem_summary.source_checkpoint_digest
    );

    assert!(outcome.source_fence_report.stale_heartbeat_refused);
    assert!(outcome.source_fence_report.stale_dispatch_refused);
    assert_eq!(outcome.source_revocation_receipts.len(), 1);
    assert_eq!(outcome.source_lease_close_report.closed_quota.returned, 150);
    assert_eq!(
        outcome.target_lease_regrant_summary.target_quota_face_value,
        150
    );
    assert!(outcome.target_lease_regrant_summary.target_quota_active);
    assert_eq!(
        outcome
            .target_lease_regrant_summary
            .target_capacity
            .expect("target capacity regranted")
            .1,
        120
    );
    assert!(outcome.target_lease_regrant_summary.target_device.is_some());
    assert_eq!(
        outcome
            .target_restore_report
            .restored_binding
            .process_generation
            .get(),
        2
    );

    // Verify post-migration Source state locally (fenced, leases Closed/Returned, cap revoked).
    source_host.with_cell_host(|ch| {
        assert!(matches!(
            ch.runtime()
                .process
                .inspect_active_process_binding(env.binding.process_id),
            Err(ProcessAuthorityError::ProcessBindingTerminal(
                ProcessLifecycleState::Terminated
            ))
        ));
        assert!(ch.supervisor_pids().lookup(env.binding.process_id).is_err());
        assert_eq!(
            ch.query_quota_lease(q_src).expect("q").state(),
            QuotaLeaseState::Closed
        );
        assert_eq!(
            ch.query_capacity_lease(c_src).expect("c").state(),
            CapacityLeaseState::Returned
        );
        assert_eq!(
            ch.query_device_lease(d_src).expect("d").state(),
            ExclusiveDeviceLeaseState::Returned
        );
        let op_snap = ch
            .runtime()
            .operations
            .inspect(OperationHandle {
                operation_id: op_1,
                generation: Generation::INITIAL,
            })
            .expect("inflight op");
        assert_eq!(op_snap.state, OperationState::CancelRequested);
        let gen_two_cap = CapabilityHandle {
            capability_id: env.root_cap.handle.capability_id,
            generation: Generation::new(NonZeroU64::new(2).expect("gen 2")),
        };
        assert!(matches!(
            inspect_host_active_capability(ch, gen_two_cap, base_ms + 310),
            Err(CapabilityAuthorityError::CapabilityRevoked)
        ));
    });

    // ------------------------------------------------------------------------
    // Sub-case C: Capability privilege amplification (`DELEGATE` -> `DELEGATE |
    // SEMANTIC_APPEND`) MUST fail closed at `CapabilitiesRebound`!
    // ------------------------------------------------------------------------
    let q_amp_src = QuotaLeaseId::from_bytes([0x91; 16]);
    let q_amp_dst = QuotaLeaseId::from_bytes([0x92; 16]);
    let env_amp = seed_migrating_process_on_source(
        &source_host,
        &adapter,
        0xB6,
        true,
        false,
        80,
        20,
        0,
        q_amp_src,
        CapacityLeaseId::from_bytes([0x93; 16]),
        ExclusiveDeviceLeaseId::from_bytes([0x94; 16]),
        ExecutionFiberId::from_bytes([0x22; 16]),
        OperationId::from_bytes([0x62; 16]),
        CancellationScopeId::from_bytes([0x52; 16]),
    );
    let amp_base_ms = env_amp.binding.created_at_ms + 1_000;
    let amp_req = build_migration_request_from_env(
        &env_amp,
        [0x55; 16],
        target_cell,
        Vec::new(),
        PinnedSessionPolicy::RefuseMigration,
        CheckpointSourceSpec::TransparentState {
            state_payload: b"amp-probe-state".to_vec(),
            cache_key: None,
            checkpointed_at_ms: amp_base_ms + 200,
        },
        true,
        &LeaseMigrationBundleSpec {
            quota: QuotaLeaseMigrationSpec {
                source_lease_id: q_amp_src,
                gateway_id: [0x95; 16],
                override_fence_ack: None,
                target_lease_id: q_amp_dst,
                target_face_value: None,
                target_expires_at: None,
                activate_on_target: true,
            },
            capacity: None,
            device: None,
            regranted_at_ms: amp_base_ms + 240,
        },
        OperationId::from_bytes([0x62; 16]),
        CancellationScopeId::from_bytes([0x52; 16]),
    );
    let amp_err = drive_cross_socket_process_migration(
        &source_host,
        Some(&adapter),
        amp_req,
        CrossSocketProcessMigrationOptions {
            sync_semantic_provenance: false,
            ..CrossSocketProcessMigrationOptions::default()
        },
    )
    .await
    .expect_err("capability privilege amplification must fail closed");

    match amp_err {
        CrossSocketMigrationError::Drive(inner) => match *inner {
            ProcessMigrationDriveError::QuarantinedPostFence { failed_step, .. } => {
                assert_eq!(failed_step, ProcessMigrationStep::CapabilitiesRebound);
            }
            other => panic!("expected QuarantinedPostFence at CapabilitiesRebound, got {other:?}"),
        },
        other => panic!("expected Drive(QuarantinedPostFence), got {other:?}"),
    }

    let amp_target_snap = inspect_remote_target_migration_state(
        &source_host,
        target_cell,
        env_amp.binding.process_id,
        IdempotencyKey::from_bytes([0x56; 16]),
        5_000,
    )
    .await
    .expect("inspect target after amplification refusal");
    assert_eq!(amp_target_snap.active_generation, None);
    assert!(!amp_target_snap.supervisor_registered);

    emit_ctrl(&format!(
        "MIGRATED_OK:{}",
        hex_encode(env.binding.process_id.as_bytes())
    ));
    let finish = read_parent_line(&mut stdin);
    assert_eq!(finish, "SHUTDOWN");
    source_host.shutdown().await;
    emit_ctrl("PASS");
}

async fn run_worker_mig_happy_target(host_root: &Path, node_idx: u8) {
    let mut stdin = BufReader::new(std::io::stdin());
    let consensus = open_single_host_consensus(host_root, node_idx);
    let target_host = bind_isolated_networked_host(host_root, node_idx, 2, consensus).await;

    // Register public-key-only principal for Source's semantic assertion issuer
    // so Target's `IdentityAuthority` verifies the 2-event provenance chain.
    let (_sem_sk, sem_boot) = semantic_issuer_bootstrap();
    target_host
        .register_peer_principal(sem_boot)
        .expect("register semantic issuer public principal on target");

    let svc_addr = target_host
        .service_addr()
        .expect("bound target service addr");
    emit_ctrl(&format!("READY:{svc_addr}"));

    let peer_line = read_parent_line(&mut stdin);
    let source_addr: SocketAddr = peer_line
        .strip_prefix("PEER:")
        .expect("PEER:<addr>")
        .parse()
        .expect("parse source SocketAddr");
    target_host
        .upsert_peer_endpoint_hint(
            cell_for_node(1),
            source_addr,
            EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 1),
        )
        .expect("upsert source endpoint hint on target");

    let verify_cmd = read_parent_line(&mut stdin);
    let proc_hex = verify_cmd
        .strip_prefix("VERIFY_TARGET:")
        .expect("VERIFY_TARGET:<proc_hex>");
    let proc_bytes: [u8; 16] = hex_decode(proc_hex)
        .try_into()
        .expect("16-byte migrated ProcessId");
    let migrated_pid = ProcessId::from_bytes(proc_bytes);

    target_host.with_cell_host(|ch| {
        let active = ch
            .runtime()
            .process
            .inspect_active_process_binding(migrated_pid)
            .expect("migrated process must be Active on Target");
        assert_eq!(active.process_generation.get(), 2);
        let sup = ch
            .supervisor_pids()
            .lookup(migrated_pid)
            .expect("registered in Target SupervisorPidRegistry");
        assert_eq!(sup.process_generation.get(), 2);
        assert_eq!(sup.os_pid, ch.os_process_id());

        // Verify streamed 5-chunk 70 KiB artifact exists in Target's isolated `ArtifactStore`!
        let art_bytes = ch
            .runtime()
            .artifacts
            .get_revision(ArtifactId::from_bytes([0x81; 16]), 1, PKI_CLOCK_MS)
            .expect("read streamed multi-chunk restart artifact on Target");
        assert_eq!(art_bytes.len(), 70 * 1024);

        // Verify 2-event causal semantic provenance chain imported into Target's `SemanticAuthority`!
        let imported_events = ch
            .runtime()
            .semantic()
            .export_signed_event_set()
            .expect("export Target semantic event set");
        assert_eq!(imported_events.len(), 2);

        // Verify attenuated capability & 3-family leases active on Target!
        let cap_path = NamePath::new(CAP_NAME_PATH).expect("cap name path");
        assert!(ch.cached_name(&cap_path).is_some());
        let q = ch
            .query_quota_lease(QuotaLeaseId::from_bytes([0x72; 16]))
            .expect("target quota");
        assert_eq!(q.state(), QuotaLeaseState::Active);
        assert_eq!(q.face_value(), 150);
        let c = ch
            .query_capacity_lease(CapacityLeaseId::from_bytes([0x74; 16]))
            .expect("target capacity");
        assert_eq!(c.state(), CapacityLeaseState::GlobalReserved);
        assert_eq!(c.amount(), 120);
        assert!(!ch.device_head_free());
    });

    emit_ctrl("TARGET_VERIFIED");
    let finish = read_parent_line(&mut stdin);
    assert_eq!(finish, "SHUTDOWN");
    target_host.shutdown().await;
    emit_ctrl("PASS");
}

#[allow(clippy::too_many_lines)]
async fn run_worker_mig_cut_source(host_root: &Path, node_idx: u8) {
    let stdin = Arc::new(Mutex::new(BufReader::new(std::io::stdin())));
    let adapter = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("TokioRuntimeAdapter");
    let consensus = open_single_host_consensus(host_root, node_idx);
    let source_host = bind_isolated_networked_host(host_root, node_idx, 2, consensus).await;
    let svc_addr = source_host.service_addr().expect("bound service addr");
    emit_ctrl(&format!("READY:{svc_addr}"));

    let peer_line = {
        let mut guard = stdin.lock().unwrap_or_else(PoisonError::into_inner);
        read_parent_line(&mut guard)
    };
    let target_addr: SocketAddr = peer_line
        .strip_prefix("PEER:")
        .expect("PEER:<addr>")
        .parse()
        .expect("parse target SocketAddr");
    let target_cell = cell_for_node(2);

    exchange_peer_bootstrap_over_mtls(
        &source_host,
        target_cell,
        target_addr,
        EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 1),
        IdempotencyKey::from_bytes([0x11; 16]),
    )
    .await
    .expect("exchange peer bootstrap over mTLS");

    let q_src = QuotaLeaseId::from_bytes([0xA1; 16]);
    let q_dst = QuotaLeaseId::from_bytes([0xA2; 16]);
    let c_src = CapacityLeaseId::from_bytes([0xA3; 16]);
    let c_dst = CapacityLeaseId::from_bytes([0xA4; 16]);
    let d_src = ExclusiveDeviceLeaseId::from_bytes([0xA5; 16]);
    let d_dst = ExclusiveDeviceLeaseId::from_bytes([0xA6; 16]);
    let fiber_id = ExecutionFiberId::from_bytes([0x31; 16]);
    let op_id = OperationId::from_bytes([0x32; 16]);
    let scope_id = CancellationScopeId::from_bytes([0x33; 16]);

    let env = seed_migrating_process_on_source(
        &source_host,
        &adapter,
        0xC7,
        false,
        true,
        200,
        40,
        100,
        q_src,
        c_src,
        d_src,
        fiber_id,
        op_id,
        scope_id,
    );
    let base_ms = env.binding.created_at_ms + 1_000;
    let (_e1, _e2) = append_two_event_causal_provenance_chain_on_source(&source_host, base_ms);

    let cut_req = build_migration_request_from_env(
        &env,
        [0x66; 16],
        target_cell,
        vec![DriverSessionAttachment::Migratable {
            session_id: [0x34; 16],
            state_digest: [0x35; 32],
        }],
        PinnedSessionPolicy::RefuseMigration,
        CheckpointSourceSpec::TransparentState {
            state_payload: b"w71-transparent-checkpoint-cut-reconcile".to_vec(),
            cache_key: Some("w71-cut-ckpt".to_owned()),
            checkpointed_at_ms: base_ms + 200,
        },
        false,
        &LeaseMigrationBundleSpec {
            quota: QuotaLeaseMigrationSpec {
                source_lease_id: q_src,
                gateway_id: [0xA7; 16],
                override_fence_ack: None,
                target_lease_id: q_dst,
                target_face_value: None,
                target_expires_at: None,
                activate_on_target: true,
            },
            capacity: Some(CapacityLeaseMigrationSpec {
                source_lease_id: c_src,
                reconcile_receipt_id: ReceiptId::from_bytes([0xA8; 16]),
                gateway_id: [0xA9; 16],
                target_lease_id: c_dst,
                target_amount: None,
            }),
            device: Some(DeviceLeaseMigrationSpec {
                source_lease_id: d_src,
                reset_receipt_id: ReceiptId::from_bytes([0xAA; 16]),
                reconcile_receipt_id: ReceiptId::from_bytes([0xAB; 16]),
                gateway_id: [0xAC; 16],
                target_lease_id: d_dst,
                advance_target_epoch_if_needed: true,
            }),
            regranted_at_ms: base_ms + 240,
        },
        op_id,
        scope_id,
    );

    let proc_hex = hex_encode(env.binding.process_id.as_bytes());
    let stdin_for_hook = Arc::clone(&stdin);
    let hook = Box::pin(async move {
        emit_ctrl(&format!("TRIGGER_CUT:{proc_hex}"));
        let ack = tokio::task::spawn_blocking(move || {
            let mut guard = stdin_for_hook
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            read_parent_line(&mut guard)
        })
        .await
        .expect("await CUT_ACTIVE ack");
        assert_eq!(ack, "CUT_ACTIVE");
    });

    let cut_err = drive_cross_socket_process_migration(
        &source_host,
        Some(&adapter),
        cut_req,
        CrossSocketProcessMigrationOptions {
            sync_semantic_provenance: true,
            semantic_max_batch_events: 8,
            rpc_deadline_ms: 5_000,
            after_source_fenced_hook: Some(hook),
            ..CrossSocketProcessMigrationOptions::default()
        },
    )
    .await
    .expect_err("network cut immediately after SourceFenced must return PostFenceNetworkCut");

    let CrossSocketMigrationError::PostFenceNetworkCut(mut interrupted) = cut_err else {
        panic!("expected CrossSocketMigrationError::PostFenceNetworkCut, got {cut_err:?}");
    };

    // 1. Verify post-cut invariants: ZERO dual-live execution, Source process is
    //    `Terminated` + unregistered, unsettled 3-family leases are `Quarantined`,
    //    and `[DIST-FAIL-001]` `PendingReconciliationTracker` holds the uncertain key!
    assert_eq!(
        interrupted.interrupted_step,
        ProcessMigrationStep::CapabilitiesRebound
    );
    assert!(
        interrupted
            .pending_tracker
            .requires_reconciliation(interrupted.interrupted_idempotency_key)
    );
    assert_eq!(
        interrupted.source_quarantine.quota_state,
        QuotaLeaseState::Quarantined
    );
    assert_eq!(
        interrupted.source_quarantine.capacity_state,
        Some(CapacityLeaseState::Quarantined)
    );
    assert_eq!(
        interrupted.source_quarantine.device_state,
        Some(ExclusiveDeviceLeaseState::Quarantined)
    );

    source_host.with_cell_host(|ch| {
        assert!(matches!(
            ch.runtime()
                .process
                .inspect_active_process_binding(env.binding.process_id),
            Err(ProcessAuthorityError::ProcessBindingTerminal(
                ProcessLifecycleState::Terminated
            ))
        ));
        assert!(ch.supervisor_pids().lookup(env.binding.process_id).is_err());
    });
    let lease_page_during_cut = source_host
        .inspect_cell_leases(None, 16)
        .expect("inspect quarantined source leases");
    assert_eq!(lease_page_during_cut.leases.len(), 3);
    assert!(
        lease_page_during_cut
            .leases
            .iter()
            .all(|r| r.lock_state == CellLeaseLockState::Quarantined)
    );

    // Coordinate with parent + Target to verify Target ALSO has zero active execution
    // during the cut, then heal the mTLS link!
    emit_ctrl("CHECK_TARGET_DURING_CUT");
    let healed = {
        let mut guard = stdin.lock().unwrap_or_else(PoisonError::into_inner);
        read_parent_line(&mut guard)
    };
    assert_eq!(healed, "LINK_HEALED");

    // 2. Reconcile idempotently (`[DIST-FAIL-001]`): inspect Target first, clear
    //    `PendingReconciliationTracker`, settle Source's Quarantined leases, and
    //    replay Steps 5..=8 to `ProcessMigrationStep::Completed`!
    //    Deterministically record the uncertain side-effecting key in the underlying
    //    `MtlsCellTransport` tracker so Linux/macOS (`BeforeFirstPayloadByte`) and
    //    Windows (`AfterFirstPayloadByte` `WSAECONNRESET`) exercise the exact same
    //    transport-level fence clearing path.
    source_host
        .transport()
        .reconciliation_tracker()
        .record_uncertain_side_effect(
            interrupted.interrupted_idempotency_key,
            nlos_net::SocketFailureKind::ConnectionReset,
        );
    assert!(
        source_host
            .transport()
            .reconciliation_tracker()
            .requires_reconciliation(interrupted.interrupted_idempotency_key)
    );

    let (pre_reconcile_snap, outcome) =
        reconcile_post_fence_interrupted_migration(&source_host, &mut interrupted)
            .await
            .expect("reconcile_post_fence_interrupted_migration must converge cleanly");

    assert_eq!(pre_reconcile_snap.active_generation, None);
    assert!(!pre_reconcile_snap.supervisor_registered);
    assert!(
        !interrupted
            .pending_tracker
            .requires_reconciliation(interrupted.interrupted_idempotency_key)
    );
    assert!(
        !source_host
            .transport()
            .reconciliation_tracker()
            .requires_reconciliation(interrupted.interrupted_idempotency_key)
    );
    assert_eq!(
        outcome.coordinator.current_step(),
        ProcessMigrationStep::Completed
    );
    assert!(outcome.receipt.verify_digest());
    assert!(outcome.target_checkpoint_summary.verify_digest());
    assert_eq!(outcome.source_lease_close_report.closed_quota.returned, 160);
    assert_eq!(
        outcome.target_lease_regrant_summary.target_quota_face_value,
        160
    );
    assert_eq!(
        outcome
            .target_restore_report
            .restored_binding
            .process_generation
            .get(),
        2
    );

    source_host.with_cell_host(|ch| {
        assert_eq!(
            ch.query_quota_lease(q_src).expect("q").state(),
            QuotaLeaseState::Closed
        );
        assert_eq!(
            ch.query_capacity_lease(c_src).expect("c").state(),
            CapacityLeaseState::Returned
        );
        assert_eq!(
            ch.query_device_lease(d_src).expect("d").state(),
            ExclusiveDeviceLeaseState::Returned
        );
    });

    emit_ctrl("RECONCILED_OK");
    let finish = {
        let mut guard = stdin.lock().unwrap_or_else(PoisonError::into_inner);
        read_parent_line(&mut guard)
    };
    assert_eq!(finish, "SHUTDOWN");
    source_host.shutdown().await;
    emit_ctrl("PASS");
}

async fn run_worker_mig_cut_target(host_root: &Path, node_idx: u8) {
    let mut stdin = BufReader::new(std::io::stdin());
    let consensus = open_single_host_consensus(host_root, node_idx);
    let target_host = bind_isolated_networked_host(host_root, node_idx, 2, consensus).await;

    let (_sem_sk, sem_boot) = semantic_issuer_bootstrap();
    target_host
        .register_peer_principal(sem_boot)
        .expect("register semantic issuer public principal on target");

    let svc_addr = target_host
        .service_addr()
        .expect("bound target service addr");
    emit_ctrl(&format!("READY:{svc_addr}"));

    let peer_line = read_parent_line(&mut stdin);
    let source_addr: SocketAddr = peer_line
        .strip_prefix("PEER:")
        .expect("PEER:<addr>")
        .parse()
        .expect("parse source SocketAddr");
    target_host
        .upsert_peer_endpoint_hint(
            cell_for_node(1),
            source_addr,
            EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 1),
        )
        .expect("upsert source endpoint hint on target");

    // 1. Cut network link immediately after SourceFenced!
    let cut_cmd = read_parent_line(&mut stdin);
    let proc_hex = cut_cmd.strip_prefix("CUT:").expect("CUT:<proc_hex>");
    let proc_bytes: [u8; 16] = hex_decode(proc_hex).try_into().expect("16-byte ProcessId");
    let migrating_pid = ProcessId::from_bytes(proc_bytes);

    target_host.set_network_cut(true).await;
    emit_ctrl("CUT_APPLIED");

    // 2. While cut is active and Source is fenced, verify zero dual-live execution on Target!
    let check_cmd = read_parent_line(&mut stdin);
    assert_eq!(check_cmd, "ASSERT_NO_TARGET_EXEC_AND_HEAL");
    target_host.with_cell_host(|ch| {
        assert!(
            ch.runtime()
                .process
                .inspect_active_process_binding(migrating_pid)
                .is_err(),
            "Target MUST NOT have restored process before reconciliation"
        );
        assert!(ch.supervisor_pids().lookup(migrating_pid).is_err());
    });
    target_host.set_network_cut(false).await;
    emit_ctrl("HEALED");

    // 3. After reconciliation completes, verify `gen = 2` is active on Target!
    let final_cmd = read_parent_line(&mut stdin);
    assert_eq!(final_cmd, "VERIFY_FINAL_TARGET");
    target_host.with_cell_host(|ch| {
        let active = ch
            .runtime()
            .process
            .inspect_active_process_binding(migrating_pid)
            .expect("reconciled process must be Active on Target");
        assert_eq!(active.process_generation.get(), 2);
        assert!(ch.supervisor_pids().lookup(migrating_pid).is_ok());
        let q = ch
            .query_quota_lease(QuotaLeaseId::from_bytes([0xA2; 16]))
            .expect("regranted quota");
        assert_eq!(q.state(), QuotaLeaseState::Active);
        assert_eq!(q.face_value(), 160);
    });
    emit_ctrl("FINAL_TARGET_OK");

    let finish = read_parent_line(&mut stdin);
    assert_eq!(finish, "SHUTDOWN");
    target_host.shutdown().await;
    emit_ctrl("PASS");
}

#[allow(clippy::too_many_lines)]
async fn run_worker_quorum_host(host_root: &Path, node_idx: u8) {
    let mut stdin = BufReader::new(std::io::stdin());
    let voter_cells = vec![cell_for_node(1), cell_for_node(2), cell_for_node(3)];
    let roster = ValidatedVoterRoster::new(vec![
        VoterReplica {
            cell_identity: cell_for_node(1),
            control_domain_id: control_domain_for_node(1),
        },
        VoterReplica {
            cell_identity: cell_for_node(2),
            control_domain_id: control_domain_for_node(2),
        },
        VoterReplica {
            cell_identity: cell_for_node(3),
            control_domain_id: control_domain_for_node(3),
        },
    ])
    .expect("valid N=3 independent failure-domain voter roster");

    let live_raft_slot = Arc::new(OnceLock::new());
    let (raft_security, _) = provision_isolated_security(
        host_root,
        "raft-transport",
        node_idx,
        3,
        Arc::clone(&live_raft_slot),
    );
    let raft_resolver = Arc::new(MonotonicEndpointResolver::new());
    let quorum_node = Arc::new(
        MultiHostQuorumNode::bind_and_start(
            host_root.join("raft-consensus"),
            MultiHostQuorumConfig::new(cell_for_node(node_idx), loopback_ephemeral(), roster),
            Arc::clone(&raft_resolver) as Arc<dyn EndpointResolver>,
            raft_security,
        )
        .await
        .expect("bind & start MultiHostQuorumNode"),
    );
    let raft_addr = quorum_node.local_addr();

    let host = bind_isolated_networked_host(
        host_root,
        node_idx,
        3,
        NetConsensusBackend::MultiHostQuorum(Arc::clone(&quorum_node)),
    )
    .await;
    let _ = live_raft_slot.set(Arc::downgrade(host.cell_host_arc()));

    // Register this voter's `KeyPurpose::SemanticSigning` attestation signer.
    let _voter_signer = VoterAttestationSigner::bootstrap_on_host(&host, 0x20 + node_idx)
        .expect("bootstrap voter attestation signer on host");

    let svc_addr = host.service_addr().expect("bound service addr");
    emit_ctrl(&format!("READY:{raft_addr}|{svc_addr}"));

    // Receive `PEERS:<r1>,<s1>;<r2>,<s2>;<r3>,<s3>` and populate both resolvers!
    let peers_line = read_parent_line(&mut stdin);
    let peers_spec = peers_line.strip_prefix("PEERS:").expect("PEERS:...");
    let ver = EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 1);
    for (i, entry) in peers_spec.split(';').enumerate() {
        let peer_idx = u8::try_from(i + 1).expect("peer idx");
        let (r_str, s_str) = entry.split_once(',').expect("raft,svc pair");
        let r_addr: SocketAddr = r_str.parse().expect("parse peer raft SocketAddr");
        let s_addr: SocketAddr = s_str.parse().expect("parse peer service SocketAddr");
        let peer_cell = cell_for_node(peer_idx);
        let peer_principal = peer_principal_id_for_node(peer_idx);

        raft_resolver
            .upsert_hint(
                EndpointHintRecord::new(peer_cell, r_addr, ver, 1)
                    .with_expected_principal(peer_principal),
            )
            .expect("upsert raft peer hint");
        if peer_idx != node_idx {
            host.upsert_peer_endpoint_hint(peer_cell, s_addr, ver)
                .expect("upsert service peer hint");
        }
    }

    let task_id = TaskId::from_bytes([0xD7; 16]);
    if node_idx == 1 {
        // Host 1 (`Incumbent`): initialize 6-endpoint §26.1 task shard in private dir!
        let setup = setup_six_endpoint_incumbent_task_on_host(
            &host,
            &host_root.join("private-incumbent-task-shard"),
            task_id,
            ProcessId::from_bytes([0xE1; 16]),
            0x41,
            1_000,
            500,
        )
        .expect("setup 6-endpoint incumbent task on Host 1");
        assert_eq!(setup.canonical_participants.len(), 6);
        assert_eq!(setup.incumbent_lease.term, 1);

        let meta_hex =
            encode_incumbent_takeover_meta(&setup.incumbent_lease, &setup.initial_assignment);
        emit_ctrl(&format!("INCUMBENT_READY:{meta_hex}"));

        let cmd = read_parent_line(&mut stdin);
        assert_eq!(cmd, "SHUTDOWN");
    } else if node_idx == 2 {
        // Host 2 (`Successor`): receive incumbent metadata, exchange mTLS bootstrap,
        // and drive full `N = 3` Raft quorum-backed 6-endpoint takeover!
        let run_line = read_parent_line(&mut stdin);
        let meta_hex = run_line
            .strip_prefix("RUN_TAKEOVER:")
            .expect("RUN_TAKEOVER:<hex>");
        let (incumbent_stale_lease, initial_assignment) = decode_incumbent_takeover_meta(meta_hex);

        for peer_idx in [1_u8, 3_u8] {
            let peer_cell = cell_for_node(peer_idx);
            // Peer hint was already inserted at heartbeat_seq = 1; bump to 2 for exchange.
            let peer_s_addr = peers_spec
                .split(';')
                .nth(usize::from(peer_idx - 1))
                .and_then(|s| s.split_once(','))
                .expect("peer addr")
                .1
                .parse()
                .expect("SocketAddr");
            exchange_peer_bootstrap_over_mtls(
                &host,
                peer_cell,
                peer_s_addr,
                EndpointRoutingVersion::new(Generation::INITIAL, CellEpoch::INITIAL, 2),
                IdempotencyKey::from_bytes([0x70 + peer_idx; 16]),
            )
            .await
            .expect("exchange peer bootstrap with voter");
        }

        let outcome = drive_quorum_backed_six_endpoint_takeover(
            &host,
            QuorumSixEndpointTakeoverRequest {
                task_id,
                incumbent_cell: cell_for_node(1),
                incumbent_stale_lease,
                initial_assignment,
                voter_cells,
                successor_private_shard_dir: host_root.join("private-successor-task-shard"),
                successor_holder_id: ProcessId::from_bytes([0xE2; 16]),
                successor_lease_idempotency_key: IdempotencyKey::from_bytes([0xE3; 16]),
                takeover_start_ms: 2_000,
                successor_lease_ttl_ms: 5_000,
                stale_probe_at_ms: 2_200,
            },
        )
        .await
        .expect("N=3 quorum-backed 6-endpoint takeover");

        // Verify Raft majority commits + `CommittedControlIndexGate` quorum proofs!
        assert!(
            outcome
                .initial_consensus_receipt
                .consensus_commit_index
                .get()
                >= 1
        );
        assert!(outcome.initial_commit_proof.verified_voter_domains.len() >= 2);
        assert_eq!(
            outcome.initial_commit_proof.disposition,
            AdmittedCommitDisposition::Advanced
        );
        assert!(
            outcome.takeover_consensus_receipt.consensus_commit_index
                > outcome.initial_consensus_receipt.consensus_commit_index
        );
        assert!(outcome.takeover_commit_proof.verified_voter_domains.len() >= 2);
        assert_eq!(
            outcome.takeover_commit_proof.disposition,
            AdmittedCommitDisposition::Advanced
        );
        assert_eq!(outcome.takeover_commit_proof.authority_term.get(), 2);

        // Verify all 6 canonical §26.1 endpoint barriers signed & observed over mTLS!
        assert!(outcome.six_endpoint_audit.all_six_canonical_types_present);
        assert!(
            outcome
                .six_endpoint_audit
                .all_union_members_signed_and_observed
        );
        assert_eq!(outcome.takeover_outcome.fence_members.len(), 6);
        assert_eq!(outcome.takeover_outcome.observations.len(), 6);
        assert_eq!(outcome.takeover_outcome.successor_lease.term, 2);
        assert_eq!(
            outcome.takeover_outcome.active_assignment.state,
            AuthorityAssignmentState::Active
        );
        assert_eq!(
            outcome.takeover_outcome.reopened_registry.state,
            ParticipantRegistryState::Open
        );

        // Verify federation binding + Raft checkpoint anchor + old-term fencing on Host 1!
        assert!(
            outcome.checkpoint_anchor_receipt.consensus_commit_index
                > outcome.takeover_consensus_receipt.consensus_commit_index
        );
        assert!(outcome.stale_incumbent_probe.stale_prepare_fenced);
        assert!(outcome.stale_incumbent_probe.stale_permit_fenced);
        assert_eq!(outcome.stale_incumbent_probe.observed_durable_term, 2);

        emit_ctrl("TAKEOVER_PASS");
        let cmd = read_parent_line(&mut stdin);
        assert_eq!(cmd, "SHUTDOWN");
    } else {
        // Host 3 (`Third Voter` in independent `ControlDomainId`): participates in
        // Raft leader election, WAL+FULL log replication, and Ed25519 commit attestations.
        let cmd = read_parent_line(&mut stdin);
        assert_eq!(cmd, "SHUTDOWN");
    }

    host.shutdown().await;
    emit_ctrl("PASS");
}

#[test]
#[ignore = "subprocess worker invoked by cross_host_sync_and_migration parent tests"]
fn child_net_cell_host_worker() {
    let (Ok(role), Ok(raw_root), Ok(raw_idx)) = (
        std::env::var(ENV_WORKER_ROLE),
        std::env::var(ENV_HOST_ROOT),
        std::env::var(ENV_NODE_IDX),
    ) else {
        eprintln!(
            "skipped: {ENV_WORKER_ROLE}/{ENV_HOST_ROOT}/{ENV_NODE_IDX} unset (parent spawn only)"
        );
        return;
    };
    let host_root = PathBuf::from(raw_root);
    let node_idx: u8 = raw_idx.parse().expect("valid u8 node_idx");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build child tokio runtime");

    rt.block_on(async move {
        match role.as_str() {
            "n1_desktop" => Box::pin(run_worker_n1_desktop(&host_root, node_idx)).await,
            "mig_happy_source" => Box::pin(run_worker_mig_happy_source(&host_root, node_idx)).await,
            "mig_happy_target" => Box::pin(run_worker_mig_happy_target(&host_root, node_idx)).await,
            "mig_cut_source" => Box::pin(run_worker_mig_cut_source(&host_root, node_idx)).await,
            "mig_cut_target" => Box::pin(run_worker_mig_cut_target(&host_root, node_idx)).await,
            "quorum_host" => Box::pin(run_worker_quorum_host(&host_root, node_idx)).await,
            other => panic!("unknown child worker role: {other}"),
        }
    });
}

// ============================================================================
// Top-Level Integration Tests (Spawn Isolated Child OS Processes)
// ============================================================================

#[test]
fn single_host_n1_net_cell_host_zero_sockets_and_cell_inspector_projection() {
    let host_dir = TempDir::new("n1-desktop");
    let mut worker = SpawnedHostWorker::spawn("n1_desktop", host_dir.path(), 1);
    assert_eq!(worker.recv_ctrl(), "PASS");
    worker.wait_clean_exit();
}

#[test]
fn cross_socket_eight_step_migration_and_pinned_session_policies_over_mtls() {
    // Strictly separate TempDirs — zero shared filesystem paths between Source & Target!
    let source_dir = TempDir::new("mig-happy-src");
    let target_dir = TempDir::new("mig-happy-dst");
    assert_ne!(source_dir.path(), target_dir.path());

    let mut source = SpawnedHostWorker::spawn("mig_happy_source", source_dir.path(), 1);
    let mut target = SpawnedHostWorker::spawn("mig_happy_target", target_dir.path(), 2);

    let src_ready = source.recv_ctrl();
    let dst_ready = target.recv_ctrl();
    let src_addr = src_ready.strip_prefix("READY:").expect("src READY");
    let dst_addr = dst_ready.strip_prefix("READY:").expect("dst READY");

    source.send_line(&format!("PEER:{dst_addr}"));
    target.send_line(&format!("PEER:{src_addr}"));

    let migrated_msg = source.recv_ctrl();
    let proc_hex = migrated_msg
        .strip_prefix("MIGRATED_OK:")
        .expect("MIGRATED_OK:<proc_hex>");

    target.send_line(&format!("VERIFY_TARGET:{proc_hex}"));
    assert_eq!(target.recv_ctrl(), "TARGET_VERIFIED");

    source.send_line("SHUTDOWN");
    target.send_line("SHUTDOWN");
    assert_eq!(source.recv_ctrl(), "PASS");
    assert_eq!(target.recv_ctrl(), "PASS");
    source.wait_clean_exit();
    target.wait_clean_exit();
}

#[test]
fn post_source_fenced_network_cut_prevents_dual_live_and_reconciles_idempotently() {
    let source_dir = TempDir::new("mig-cut-src");
    let target_dir = TempDir::new("mig-cut-dst");
    assert_ne!(source_dir.path(), target_dir.path());

    let mut source = SpawnedHostWorker::spawn("mig_cut_source", source_dir.path(), 1);
    let mut target = SpawnedHostWorker::spawn("mig_cut_target", target_dir.path(), 2);

    let src_ready = source.recv_ctrl();
    let dst_ready = target.recv_ctrl();
    let src_addr = src_ready.strip_prefix("READY:").expect("src READY");
    let dst_addr = dst_ready.strip_prefix("READY:").expect("dst READY");

    source.send_line(&format!("PEER:{dst_addr}"));
    target.send_line(&format!("PEER:{src_addr}"));

    // 1. Source reaches Step 4 (`SourceFenced`) and pauses at `after_source_fenced_hook`;
    //    sever Target's mTLS service link before Step 5 (`CapabilitiesRebound`) dials!
    let trigger = source.recv_ctrl();
    let proc_hex = trigger
        .strip_prefix("TRIGGER_CUT:")
        .expect("TRIGGER_CUT:<proc_hex>");
    target.send_line(&format!("CUT:{proc_hex}"));
    assert_eq!(target.recv_ctrl(), "CUT_APPLIED");
    source.send_line("CUT_ACTIVE");

    // 2. Source observes `PostFenceNetworkCut`, verifies Source process is fenced +
    //    3-family leases are `Quarantined`, and asks Target to verify zero dual-live
    //    execution before healing the mTLS link!
    assert_eq!(source.recv_ctrl(), "CHECK_TARGET_DURING_CUT");
    target.send_line("ASSERT_NO_TARGET_EXEC_AND_HEAL");
    assert_eq!(target.recv_ctrl(), "HEALED");
    source.send_line("LINK_HEALED");

    // 3. Source reconciles idempotently to `ProcessMigrationStep::Completed`!
    assert_eq!(source.recv_ctrl(), "RECONCILED_OK");
    target.send_line("VERIFY_FINAL_TARGET");
    assert_eq!(target.recv_ctrl(), "FINAL_TARGET_OK");

    source.send_line("SHUTDOWN");
    target.send_line("SHUTDOWN");
    assert_eq!(source.recv_ctrl(), "PASS");
    assert_eq!(target.recv_ctrl(), "PASS");
    source.wait_clean_exit();
    target.wait_clean_exit();
}

#[test]
fn three_host_quorum_consensus_backed_six_endpoint_takeover_fences_old_term_host() {
    // 3 completely separate TempDirs — zero shared filesystem paths across any pair of hosts!
    let host_1_dir = TempDir::new("quorum-host-1");
    let host_2_dir = TempDir::new("quorum-host-2");
    let host_3_dir = TempDir::new("quorum-host-3");

    let mut host_1 = SpawnedHostWorker::spawn("quorum_host", host_1_dir.path(), 1);
    let mut host_2 = SpawnedHostWorker::spawn("quorum_host", host_2_dir.path(), 2);
    let mut host_3 = SpawnedHostWorker::spawn("quorum_host", host_3_dir.path(), 3);

    let parse_ready = |msg: String| -> (String, String) {
        let rest = msg.strip_prefix("READY:").expect("READY:<raft>|<svc>");
        let (r, s) = rest.split_once('|').expect("<raft>|<svc>");
        (r.to_owned(), s.to_owned())
    };
    let (r1, s1) = parse_ready(host_1.recv_ctrl());
    let (r2, s2) = parse_ready(host_2.recv_ctrl());
    let (r3, s3) = parse_ready(host_3.recv_ctrl());

    let peers_line = format!("PEERS:{r1},{s1};{r2},{s2};{r3},{s3}");
    host_1.send_line(&peers_line);
    host_2.send_line(&peers_line);
    host_3.send_line(&peers_line);

    let inc_ready = host_1.recv_ctrl();
    let meta_hex = inc_ready
        .strip_prefix("INCUMBENT_READY:")
        .expect("INCUMBENT_READY:<hex>");
    host_2.send_line(&format!("RUN_TAKEOVER:{meta_hex}"));

    assert_eq!(host_2.recv_ctrl(), "TAKEOVER_PASS");

    host_1.send_line("SHUTDOWN");
    host_2.send_line("SHUTDOWN");
    host_3.send_line("SHUTDOWN");
    assert_eq!(host_1.recv_ctrl(), "PASS");
    assert_eq!(host_2.recv_ctrl(), "PASS");
    assert_eq!(host_3.recv_ctrl(), "PASS");
    host_1.wait_clean_exit();
    host_2.wait_clean_exit();
    host_3.wait_clean_exit();
}
