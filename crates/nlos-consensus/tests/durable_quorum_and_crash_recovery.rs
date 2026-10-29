//! Real TLS 1.3 mTLS quorum replication, OS `Child::kill()` hard-crash failover,
//! `[DUR-SNAP-001]` snapshot compaction catch-up, `[DUR-FAILCLOSED-001]`
//! corruption refusal, and `N = 1` `SingleHostDesktop` zero-overhead passthrough
//! integration tests (`ADR-0026` `C-CONSENSUS-CP`).

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nlos_capability::custody::{
    BootstrapCustodyPrincipalRequest, CustodyTier, KeyPurpose, SigningKeyCustodyVault,
};
use nlos_cell::{CellEpoch, CellFence, CellFencingToken, CellIdentity};
use nlos_consensus::{
    CellDirectoryCommand, ConsensusError, ConsensusNode, ConsensusRole, ConsensusStoreError,
    ConsensusTimingConfig, ControlPlaneAuditCommand, ControlPlaneCommand, ControlPlaneProposal,
    ForbiddenDataPlaneKind, MultiHostQuorumConfig, MultiHostQuorumNode, ProposalRejectReason,
    RootLeasePoolCommand, ScopeViolationError, SingleHostPassthroughNode, SqliteConsensusStore,
    TaskAuthorityShardCommand, ValidatedVoterRoster, VoterReplica,
    encode_forbidden_data_plane_wire_probe, staggered_timing_for_roster,
};
use nlos_identity::IdentityAuthority;
use nlos_net::{
    AuthorityCustodyPeerVerifier, ClusterProfile, CustodyPeerSigner, EndpointHintRecord,
    EndpointResolver, EndpointRoutingVersion, FenceNonceRegistry, MonotonicEndpointResolver,
    MonotonicPeerFenceGate, NodeSecurityDeps, NodeTlsCredentials,
};
use nlos_types::{
    ControlDomainId, Generation, IdempotencyKey, KeyId, PrincipalId, QuotaLeaseId, ReceiptId,
    SchedulerDomainId, TaskAuthorityAssignmentId, TaskId,
};

static DUR_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

const TEST_NOW_MS: u64 = 1_780_000_000_000;
const CHILD_ENV_KEY: &str = "NLOS_CONSENSUS_CHILD_WORKER";

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = DUR_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "nlos-consensus-dur-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create durable test dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn cell_id(tag: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([tag; 16]))
}

fn wrapping_kek(node_idx: u8) -> [u8; 32] {
    [node_idx.wrapping_add(0xA0); 32]
}

#[derive(Clone, Copy)]
struct ProvisionedHostMeta {
    node_idx: u8,
    cell: CellIdentity,
    principal_id: PrincipalId,
    control_domain_id: ControlDomainId,
    key_id: KeyId,
    key_generation: Generation,
}

struct ProvisionedCluster {
    root: PathBuf,
    hosts: [ProvisionedHostMeta; 3],
    roster: ValidatedVoterRoster,
}

fn provision_three_host_cluster(root: &Path) -> ProvisionedCluster {
    let master_id_dir = root.join("master-identity");
    let mut metas = Vec::with_capacity(3);
    {
        let master_identity =
            IdentityAuthority::open(&master_id_dir).expect("open master IdentityAuthority");
        for idx in 1..=3_u8 {
            let host_dir = root.join(format!("host-{idx}"));
            let custody_dir = host_dir.join("custody");
            fs::create_dir_all(&custody_dir).expect("create custody dir");
            let vault = SigningKeyCustodyVault::open_durable(&custody_dir, wrapping_kek(idx))
                .expect("open durable custody vault");
            let (_, binding) = vault
                .bootstrap_principal_under_custody(
                    &master_identity,
                    BootstrapCustodyPrincipalRequest {
                        authority_id: [idx; 16],
                        principal_profile_digest: [idx.wrapping_add(0x10); 32],
                        control_domain_policy_digest: [idx.wrapping_add(0x20); 32],
                        key_purpose: KeyPurpose::SemanticSigning,
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
                .expect("bootstrap host principal under custody");
            metas.push(ProvisionedHostMeta {
                node_idx: idx,
                cell: cell_id(idx),
                principal_id: binding.principal_id,
                control_domain_id: binding.control_domain_id,
                key_id: binding.key_id,
                key_generation: binding.key_generation,
            });
        }
    }

    let master_db_file = master_id_dir.join("identity-authority.db");
    for idx in 1..=3_u8 {
        let host_id_dir = root.join(format!("host-{idx}")).join("identity");
        fs::create_dir_all(&host_id_dir).expect("create host identity dir");
        fs::copy(&master_db_file, host_id_dir.join("identity-authority.db"))
            .expect("copy host IdentityAuthority mirror");
    }

    let roster = ValidatedVoterRoster::new(
        metas
            .iter()
            .map(|m| VoterReplica {
                cell_identity: m.cell,
                control_domain_id: m.control_domain_id,
            })
            .collect(),
    )
    .expect("valid 3-host independent-domain roster");

    ProvisionedCluster {
        root: root.to_path_buf(),
        hosts: [metas[0], metas[1], metas[2]],
        roster,
    }
}

fn load_host_security_deps(
    root: &Path,
    node_idx: u8,
    boot_gen: u64,
    all_hosts: &[ProvisionedHostMeta; 3],
    resolver: &Arc<MonotonicEndpointResolver>,
) -> NodeSecurityDeps {
    let host_dir = root.join(format!("host-{node_idx}"));
    let identity =
        Arc::new(IdentityAuthority::open(host_dir.join("identity")).expect("open host identity"));
    let vault = Arc::new(
        SigningKeyCustodyVault::open_durable(host_dir.join("custody"), wrapping_kek(node_idx))
            .expect("open host custody vault"),
    );
    let me = all_hosts[usize::from(node_idx - 1)];
    let signer = Arc::new(CustodyPeerSigner::new(
        Arc::clone(&vault),
        me.principal_id,
        me.key_id,
        me.key_generation,
    ));
    let tls_seed = [node_idx.wrapping_add(u8::try_from(boot_gen).unwrap_or(1) * 17); 32];
    let tls_credentials = NodeTlsCredentials::issue(
        signer.as_ref(),
        tls_seed,
        TEST_NOW_MS - 1_000,
        TEST_NOW_MS + 3_600_000,
    )
    .expect("issue per-boot mTLS X.509v3 credentials");

    let dyn_resolver: Arc<dyn EndpointResolver> = Arc::clone(resolver) as Arc<dyn EndpointResolver>;
    let fence_gate = Arc::new(MonotonicPeerFenceGate::new(Some(dyn_resolver)));
    for peer in all_hosts {
        fence_gate
            .pin_cell_principal(peer.cell, peer.principal_id)
            .expect("pin peer CellIdentity <-> PrincipalId");
    }

    let generation = Generation::new(NonZeroU64::new(boot_gen).expect("non-zero boot generation"));
    let cell = me.cell;
    NodeSecurityDeps {
        local_fence: Arc::new(move || {
            CellFence::present(
                cell,
                generation,
                CellEpoch::INITIAL,
                CellFencingToken::INITIAL,
            )
        }),
        local_signer: signer,
        peer_verifier: Arc::new(AuthorityCustodyPeerVerifier::new(identity, vec![vault])),
        fence_policy: fence_gate,
        nonce_registry: Arc::new(FenceNonceRegistry::new(256)),
        clock_ms: Arc::new(|| TEST_NOW_MS),
        tls_credentials,
    }
}

fn register_endpoint_hint(
    resolver: &MonotonicEndpointResolver,
    meta: ProvisionedHostMeta,
    boot_gen: u64,
    heartbeat_seq: u64,
    addr: SocketAddr,
) {
    let version = EndpointRoutingVersion::new(
        Generation::new(NonZeroU64::new(boot_gen).expect("boot_gen >= 1")),
        CellEpoch::INITIAL,
        heartbeat_seq,
    );
    let hint = EndpointHintRecord::new(meta.cell, addr, version, heartbeat_seq * 1_000)
        .with_expected_principal(meta.principal_id);
    resolver.upsert_hint(hint).expect("upsert endpoint hint");
}

fn sample_four_family_proposals(seed: u8) -> Vec<ControlPlaneProposal> {
    let c1 = cell_id(1);
    let c2 = cell_id(2);
    let task_id = TaskId::from_bytes([seed.wrapping_add(0x11); 16]);
    let a1 = TaskAuthorityAssignmentId::from_bytes([seed.wrapping_add(0x21); 16]);
    let a2 = TaskAuthorityAssignmentId::from_bytes([seed.wrapping_add(0x22); 16]);

    vec![
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([seed, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            ControlPlaneCommand::CellDirectoryMutation(CellDirectoryCommand::UpsertCellEndpoint {
                cell: c1,
                control_domain_id: ControlDomainId::from_bytes([seed.wrapping_add(1); 16]),
                principal_id: PrincipalId::from_bytes([seed.wrapping_add(2); 16]),
                node_boot_generation: Generation::INITIAL,
                epoch: CellEpoch::INITIAL,
                heartbeat_seq: u64::from(seed) + 1,
                endpoint_hint: Some(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    20_000 + u16::from(seed),
                )),
            }),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([seed, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]),
            ControlPlaneCommand::RootLeasePoolMutation(RootLeasePoolCommand::RegisterQuotaPool {
                budget_root: [seed.wrapping_add(0x30); 16],
                initial_available: 50_000,
            }),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([seed, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]),
            ControlPlaneCommand::RootLeasePoolMutation(RootLeasePoolCommand::AllocateQuotaLease {
                lease_id: QuotaLeaseId::from_bytes([seed.wrapping_add(0x31); 16]),
                budget_root: [seed.wrapping_add(0x30); 16],
                holder_cell: c1,
                node_boot_generation: Generation::INITIAL,
                face_value: 5_000,
                epoch: CellEpoch::INITIAL,
                fencing_token: CellFencingToken::INITIAL,
                fence_scope: [seed.wrapping_add(0x32); 16],
            }),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([seed, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4]),
            ControlPlaneCommand::TaskAuthorityShardMutation(
                TaskAuthorityShardCommand::AssignInitialAuthority {
                    assignment_id: a1,
                    task_id,
                    task_generation: Generation::INITIAL,
                    authority_shard: [seed; 16],
                    home_cell: c1,
                    authority_term: NonZeroU64::new(1).unwrap(),
                    control_epoch: 1,
                    participant_registry_generation: 1,
                    participant_registry_root: [0xB1; 32],
                },
            ),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([seed, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5]),
            ControlPlaneCommand::TaskAuthorityShardMutation(
                TaskAuthorityShardCommand::RecordCompletedTakeover {
                    takeover_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x70); 16]),
                    task_id,
                    task_generation: Generation::INITIAL,
                    old_assignment_id: a1,
                    new_assignment_id: a2,
                    frozen_old_authority_term: NonZeroU64::new(1).unwrap(),
                    new_authority_term: NonZeroU64::new(2).unwrap(),
                    new_home_cell: c2,
                    new_control_epoch: 2,
                    new_participant_registry_generation: 2,
                    new_participant_registry_root: [0xB2; 32],
                    exact_fence_set_root: [0xDD; 32],
                    outstanding_effect_quarantine_root: None,
                },
            ),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([seed, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6]),
            ControlPlaneCommand::ControlPlaneAuditAnchor(
                ControlPlaneAuditCommand::AnchorReconciliationCheckpoint {
                    cell: c2,
                    node_boot_generation: Generation::INITIAL,
                    epoch: CellEpoch::INITIAL,
                    checkpoint_seq: NonZeroU64::new(u64::from(seed) + 1).unwrap(),
                    checkpoint_digest: [seed.wrapping_add(0x80); 32],
                },
            ),
        ),
    ]
}

fn hex_encode_32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ============================================================================
// Child-Process Replica Worker (`Child::kill()` Crash Target)
// ============================================================================

fn load_provisioned_cluster_from_disk(root: &Path) -> ProvisionedCluster {
    let mut metas = Vec::with_capacity(3);
    for idx in 1..=3_u8 {
        let vault = SigningKeyCustodyVault::open_durable(
            root.join(format!("host-{idx}")).join("custody"),
            wrapping_kek(idx),
        )
        .expect("open custody");
        let summary_path = root.join(format!("host-{idx}-binding.txt"));
        let raw = fs::read_to_string(&summary_path).expect("read binding summary");
        let parts: Vec<&str> = raw.trim().split(',').collect();
        let principal_id = PrincipalId::from_bytes(decode_hex_16(parts[0]));
        let control_domain_id = ControlDomainId::from_bytes(decode_hex_16(parts[1]));
        let key_id = KeyId::from_bytes(decode_hex_16(parts[2]));
        let slot = vault
            .inspect_latest_key_slot(key_id)
            .expect("inspect custody slot");
        metas.push(ProvisionedHostMeta {
            node_idx: idx,
            cell: cell_id(idx),
            principal_id,
            control_domain_id,
            key_id,
            key_generation: Generation::new(NonZeroU64::new(slot.rotation_epoch).unwrap()),
        });
    }
    let roster = ValidatedVoterRoster::new(
        metas
            .iter()
            .map(|m| VoterReplica {
                cell_identity: m.cell,
                control_domain_id: m.control_domain_id,
            })
            .collect(),
    )
    .expect("child roster");
    ProvisionedCluster {
        root: root.to_path_buf(),
        hosts: [metas[0], metas[1], metas[2]],
        roster,
    }
}

fn write_host_binding_files(cluster: &ProvisionedCluster) {
    for host in &cluster.hosts {
        let mut line = String::new();
        for arr in [
            host.principal_id.as_bytes(),
            host.control_domain_id.as_bytes(),
            host.key_id.as_bytes(),
        ] {
            if !line.is_empty() {
                line.push(',');
            }
            for b in arr {
                use std::fmt::Write as _;
                let _ = write!(line, "{b:02x}");
            }
        }
        fs::write(
            cluster
                .root
                .join(format!("host-{}-binding.txt", host.node_idx)),
            line,
        )
        .expect("write host binding file");
    }
}

fn decode_hex_16(s: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (idx, pair) in s.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let text = std::str::from_utf8(pair).expect("hex utf8");
        out[idx] = u8::from_str_radix(text, 16).expect("hex byte");
    }
    out
}

fn handle_child_control_command(
    rt: &tokio::runtime::Runtime,
    node: &MultiHostQuorumNode,
    resolver: &MonotonicEndpointResolver,
    cluster: &ProvisionedCluster,
    cmd_line: &str,
) -> (String, bool) {
    let parts: Vec<&str> = cmd_line.split_whitespace().collect();
    match parts.as_slice() {
        ["STATUS"] => {
            let role = match node.role() {
                ConsensusRole::Leader => "Leader",
                ConsensusRole::Follower => "Follower",
                ConsensusRole::PreCandidate => "PreCandidate",
                ConsensusRole::Candidate => "Candidate",
            };
            let msg = format!(
                "OK {role} {} {} {}",
                node.current_term(),
                node.consensus_commit_index(),
                hex_encode_32(&node.state_root_digest())
            );
            (msg, false)
        }
        ["PEER_HINT", idx_s, gen_s, seq_s, addr_s] => {
            let idx: u8 = idx_s.parse().expect("peer idx");
            let bgen: u64 = gen_s.parse().expect("boot gen");
            let seq: u64 = seq_s.parse().expect("seq");
            let addr: SocketAddr = addr_s.parse().expect("addr");
            register_endpoint_hint(
                resolver,
                cluster.hosts[usize::from(idx - 1)],
                bgen,
                seq,
                addr,
            );
            ("OK HINT".to_owned(), false)
        }
        ["PROPOSE_BATCH", seed_s] => {
            let seed: u8 = seed_s.parse().expect("seed");
            let props = sample_four_family_proposals(seed);
            let res = rt.block_on(async {
                let mut last_idx = 0;
                let mut last_term = 0;
                for prop in props {
                    let receipt = node.propose(prop, Duration::from_secs(10)).await?;
                    last_idx = receipt.consensus_commit_index.get();
                    last_term = receipt.term.get();
                }
                let (read_idx, read_term) = node
                    .linearizable_read(None, Duration::from_secs(10), |_sm, cidx, cterm| {
                        (cidx, cterm)
                    })
                    .await?;
                assert!(read_idx >= last_idx);
                Ok::<_, ConsensusError>((
                    last_idx,
                    last_term.max(read_term),
                    node.state_root_digest(),
                ))
            });
            match res {
                Ok((idx, term, digest)) => (
                    format!("OK COMMITTED {idx} {term} {}", hex_encode_32(&digest)),
                    false,
                ),
                Err(err) => (format!("ERR {err}"), false),
            }
        }
        ["TRY_STALE_PROPOSE", stale_term_s] => {
            let stale_term: u64 = stale_term_s.parse().expect("stale term");
            let prop = sample_four_family_proposals(99)[0];
            let res = rt.block_on(node.propose_in_term(prop, stale_term, Duration::from_secs(2)));
            match res {
                Err(ConsensusError::ProposalRejected(ProposalRejectReason::StaleLeaderTerm {
                    attempted_term,
                    current_term,
                    ..
                })) => (
                    format!("REJECTED_STALE {attempted_term} {current_term}"),
                    false,
                ),
                other => (format!("UNEXPECTED {other:?}"), false),
            }
        }
        ["SHUTDOWN"] => {
            rt.block_on(node.shutdown());
            ("OK SHUTDOWN".to_owned(), true)
        }
        _ => ("ERR UNKNOWN_CMD".to_owned(), false),
    }
}

#[test]
#[ignore = "child-only replica worker spawned by parent crash-recovery tests"]
fn consensus_replica_child_worker() {
    let Ok(spec) = env::var(CHILD_ENV_KEY) else {
        eprintln!("skipped: child-only entrypoint ({CHILD_ENV_KEY} not set)");
        return;
    };
    let fields: Vec<&str> = spec.split('|').collect();
    let root = PathBuf::from(fields[0]);
    let node_idx: u8 = fields[1].parse().expect("node_idx");
    let boot_gen: u64 = fields[2].parse().expect("boot_gen");
    let election_ticks: u32 = fields[3].parse().expect("election_ticks");
    let ready_file = PathBuf::from(fields[4]);

    let cluster = load_provisioned_cluster_from_disk(&root);
    let resolver = Arc::new(MonotonicEndpointResolver::new());
    let security = load_host_security_deps(&root, node_idx, boot_gen, &cluster.hosts, &resolver);
    let me = cluster.hosts[usize::from(node_idx - 1)];
    let store_dir = root
        .join(format!("host-{node_idx}"))
        .join("consensus-store");

    let mut cfg = MultiHostQuorumConfig::new(
        me.cell,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        cluster.roster.clone(),
    );
    cfg.timing = Some(ConsensusTimingConfig {
        heartbeat_interval_ticks: 2,
        leader_lease_ticks: 5,
        election_timeout_ticks: election_ticks,
        max_snapshot_chunk_bytes: 16 * 1024,
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build child tokio runtime");

    let dyn_resolver: Arc<dyn EndpointResolver> =
        Arc::clone(&resolver) as Arc<dyn EndpointResolver>;
    let node = rt
        .block_on(MultiHostQuorumNode::bind_and_start(
            &store_dir,
            cfg,
            dyn_resolver,
            security,
        ))
        .expect("bind child MultiHostQuorumNode");

    let ctrl_listener = TcpListener::bind("127.0.0.1:0").expect("bind child control socket");
    let ctrl_addr = ctrl_listener.local_addr().expect("ctrl local_addr");
    fs::write(&ready_file, format!("{}|{ctrl_addr}", node.local_addr())).expect("write ready");

    for incoming in ctrl_listener.incoming() {
        let mut stream = incoming.expect("accept ctrl");
        let mut reader = BufReader::new(stream.try_clone().expect("clone ctrl stream"));
        let mut line = String::new();
        if reader.read_line(&mut line).expect("read ctrl line") == 0 {
            continue;
        }
        let (reply, should_exit) =
            handle_child_control_command(&rt, &node, &resolver, &cluster, &line);
        writeln!(stream, "{reply}").expect("write ctrl reply");
        stream.flush().expect("flush ctrl reply");
        if should_exit {
            break;
        }
    }
}

struct ChildReplicaHandle {
    child: Child,
    mtls_addr: SocketAddr,
    ctrl_addr: SocketAddr,
    reaped: bool,
}

impl Drop for ChildReplicaHandle {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

impl ChildReplicaHandle {
    fn spawn(root: &Path, node_idx: u8, boot_gen: u64, election_ticks: u32) -> Self {
        let ready_file = root.join(format!("ready-node{node_idx}-gen{boot_gen}.txt"));
        let _ = fs::remove_file(&ready_file);
        let spec = format!(
            "{}|{node_idx}|{boot_gen}|{election_ticks}|{}",
            root.display(),
            ready_file.display()
        );
        let exe = env::current_exe().expect("current test binary path");
        let mut child = Command::new(exe)
            .args([
                "--ignored",
                "--exact",
                "consensus_replica_child_worker",
                "--nocapture",
            ])
            .env(CHILD_ENV_KEY, spec)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn child replica worker");

        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready_file.exists() {
            if let Some(status) = child.try_wait().expect("poll child replica worker") {
                let mut stderr_out = String::new();
                if let Some(mut err_pipe) = child.stderr.take() {
                    let _ = err_pipe.read_to_string(&mut stderr_out);
                }
                panic!(
                    "child replica worker (node {node_idx}, gen {boot_gen}) exited prematurely with {status}: {stderr_out}"
                );
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let mut stderr_out = String::new();
                if let Some(mut err_pipe) = child.stderr.take() {
                    let _ = err_pipe.read_to_string(&mut stderr_out);
                }
                panic!(
                    "timed out waiting for child replica ready file (node {node_idx}, gen {boot_gen}); stderr: {stderr_out}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let content = fs::read_to_string(&ready_file).expect("read ready file");
        let parts: Vec<&str> = content.trim().split('|').collect();
        Self {
            child,
            mtls_addr: parts[0].parse().expect("parse child mtls_addr"),
            ctrl_addr: parts[1].parse().expect("parse child ctrl_addr"),
            reaped: false,
        }
    }

    fn send_cmd(&self, cmd: &str) -> String {
        let mut stream = TcpStream::connect(self.ctrl_addr).expect("connect child ctrl");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("set_read_timeout");
        writeln!(stream, "{cmd}").expect("send ctrl cmd");
        stream.flush().expect("flush ctrl cmd");
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        reader.read_line(&mut reply).expect("read ctrl reply");
        reply.trim().to_owned()
    }

    fn hard_kill(&mut self) {
        self.child.kill().expect("OS hard kill child replica");
        let _ = self.child.wait().expect("reap hard-killed child replica");
        self.reaped = true;
    }

    fn graceful_shutdown(mut self) {
        let reply = self.send_cmd("SHUTDOWN");
        assert_eq!(reply, "OK SHUTDOWN");
        let status = self.child.wait().expect("wait graceful child exit");
        self.reaped = true;
        assert!(status.success());
    }
}

// ============================================================================
// Test 1: N = 1 `SingleHostDesktop` Zero-Overhead Passthrough (`ADR-0026` §6.3)
// ============================================================================

#[test]
fn single_host_passthrough_zero_overhead_and_restart_durability() {
    let tmp = TempDir::new("single-host");
    let cell = cell_id(1);
    let domain = ControlDomainId::from_bytes([0x41; 16]);

    let digest_before_close = {
        let unified = ConsensusNode::open_single_host_passthrough(&tmp.path, cell, domain)
            .expect("open unified single-host node");
        assert_eq!(unified.cluster_profile(), ClusterProfile::SingleHostDesktop);
        assert_eq!(unified.active_socket_count(), 0);
        drop(unified);

        let node = SingleHostPassthroughNode::open(&tmp.path, cell, domain)
            .expect("reopen SingleHostPassthroughNode");
        assert_eq!(node.active_socket_count(), 0);
        assert_eq!(node.active_background_timer_count(), 0);
        assert!(!node.requires_tls_certificates());

        let bad_frame = encode_forbidden_data_plane_wire_probe(
            ForbiddenDataPlaneKind::TopicMessagePublish,
            IdempotencyKey::from_bytes([0xEE; 16]),
            b"topic-payload",
        );
        assert!(matches!(
            node.propose_wire(&bad_frame),
            Err(ConsensusError::ScopeViolation(
                ScopeViolationError::RejectDataPlanePayload(_)
            ))
        ));
        assert_eq!(node.consensus_commit_index(), 0);

        for prop in sample_four_family_proposals(1) {
            let receipt = node.propose(prop).expect("local WAL+FULL commit");
            assert_eq!(receipt.term.get(), 1);
        }
        assert_eq!(node.consensus_commit_index(), 6);
        let snap_meta = node.create_snapshot_only().expect("create snapshot");
        assert_eq!(snap_meta.included_index, 6);
        assert_eq!(node.compact_log_up_to(6).expect("compact log"), 6);
        assert_eq!(node.uncompacted_log_len(), 0);
        node.state_root_digest()
    };

    let reopened = SingleHostPassthroughNode::open(&tmp.path, cell, domain)
        .expect("recover SingleHostPassthroughNode from compacted snapshot");
    assert_eq!(reopened.consensus_commit_index(), 6);
    assert_eq!(reopened.state_root_digest(), digest_before_close);
    let next_receipt = reopened
        .propose(sample_four_family_proposals(2)[0])
        .expect("commit post-restart entry");
    assert_eq!(next_receipt.consensus_commit_index.get(), 7);
}

// ============================================================================
// Test 2: `[DUR-FAILCLOSED-001]` Corrupted Log / WAL / Snapshot Refusal
// ============================================================================

#[test]
fn dur_failclosed_001_corrupted_log_wal_and_snapshot_refuse_startup_without_silent_truncation() {
    let cell = cell_id(1);
    let domain = ControlDomainId::from_bytes([0x41; 16]);

    // Case A: Tampered committed `consensus_log` row payload on disk.
    let tmp_log = TempDir::new("corrupt-log");
    {
        let node = SingleHostPassthroughNode::open(&tmp_log.path, cell, domain).unwrap();
        for prop in sample_four_family_proposals(5).into_iter().take(3) {
            node.propose(prop).unwrap();
        }
    }
    {
        let (store, _) = SqliteConsensusStore::open_or_recover(&tmp_log.path).unwrap();
        store.checkpoint_wal_truncate().unwrap();
    }
    {
        let raw_conn = rusqlite::Connection::open(tmp_log.path.join("consensus.sqlite3")).unwrap();
        raw_conn
            .execute_batch(
                "DROP TRIGGER trg_consensus_log_no_update;
                 UPDATE consensus_log SET payload_bytes = X'DEADBEEF' WHERE log_index = 2;",
            )
            .unwrap();
    }
    assert!(matches!(
        SqliteConsensusStore::open_or_recover(&tmp_log.path),
        Err(ConsensusStoreError::IntegrityCorrupted(_))
    ));

    // Case B: Corrupted `consensus.sqlite3-wal` header magic.
    let tmp_wal = TempDir::new("corrupt-wal");
    {
        let node = SingleHostPassthroughNode::open(&tmp_wal.path, cell, domain).unwrap();
        node.propose(sample_four_family_proposals(6)[0]).unwrap();
        let wal_path = tmp_wal.path.join("consensus.sqlite3-wal");
        // Write a malformed 64-byte WAL header while simulating crash residue after close.
        drop(node);
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&wal_path)
            .unwrap();
        f.write_all(&[0xFF; 64]).unwrap();
        f.sync_all().unwrap();
    }
    assert!(matches!(
        SqliteConsensusStore::open_or_recover(&tmp_wal.path),
        Err(ConsensusStoreError::IntegrityCorrupted(_))
    ));

    // Case C: Corrupted snapshot `.bin` payload bytes.
    let tmp_snap = TempDir::new("corrupt-snap");
    {
        let node = SingleHostPassthroughNode::open(&tmp_snap.path, cell, domain).unwrap();
        for prop in sample_four_family_proposals(7).into_iter().take(4) {
            node.propose(prop).unwrap();
        }
        node.create_snapshot_only().unwrap();
        node.compact_log_up_to(4).unwrap();
    }
    let snap_file = tmp_snap
        .path
        .join("snapshot-00000000000000000004-00000000000000000001.bin");
    let mut snap_bytes = fs::read(&snap_file).unwrap();
    snap_bytes[12] ^= 0x5A;
    fs::write(&snap_file, &snap_bytes).unwrap();
    assert!(matches!(
        SingleHostPassthroughNode::open(&tmp_snap.path, cell, domain),
        Err(ConsensusError::Store(
            ConsensusStoreError::IntegrityCorrupted(_)
        ))
    ));
}

// ============================================================================
// Test 3: `[DUR-SNAP-001]` mTLS Snapshot + Compaction Catch-Up of Offline Node
// ============================================================================

async fn start_inproc_node(
    cluster: &ProvisionedCluster,
    node_idx: u8,
    boot_gen: u64,
    timing: ConsensusTimingConfig,
    resolver: &Arc<MonotonicEndpointResolver>,
) -> MultiHostQuorumNode {
    let security =
        load_host_security_deps(&cluster.root, node_idx, boot_gen, &cluster.hosts, resolver);
    let me = cluster.hosts[usize::from(node_idx - 1)];
    let store_dir = cluster
        .root
        .join(format!("host-{node_idx}"))
        .join("consensus-store");
    let mut cfg = MultiHostQuorumConfig::new(
        me.cell,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        cluster.roster.clone(),
    );
    cfg.timing = Some(timing);
    let dyn_res: Arc<dyn EndpointResolver> = Arc::clone(resolver) as Arc<dyn EndpointResolver>;
    MultiHostQuorumNode::bind_and_start(&store_dir, cfg, dyn_res, security)
        .await
        .expect("start inproc MultiHostQuorumNode")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dur_snap_001_mtls_snapshot_compaction_catches_up_offline_follower() {
    let tmp = TempDir::new("mtls-snap-catchup");
    let cluster = provision_three_host_cluster(&tmp.path);

    // Leave dangling `.tmp` file in host-1 store dir to verify `[DUR-SNAP-001]` startup cleanup.
    let h1_store = tmp.path.join("host-1").join("consensus-store");
    fs::create_dir_all(&h1_store).unwrap();
    let dangling_tmp = h1_store.join("snapshot-99-1.tmp");
    fs::write(&dangling_tmp, b"partial-crash-staging").unwrap();

    let r1 = Arc::new(MonotonicEndpointResolver::new());
    let r2 = Arc::new(MonotonicEndpointResolver::new());

    let mut t1 = staggered_timing_for_roster(cluster.hosts[0].cell, &cluster.roster);
    t1.max_snapshot_chunk_bytes = 192;
    let mut t2 = staggered_timing_for_roster(cluster.hosts[1].cell, &cluster.roster);
    t2.max_snapshot_chunk_bytes = 192;

    // Boot ONLY Node 1 and Node 2 (2-of-3 majority) while Node 3 is offline!
    let n1 = start_inproc_node(&cluster, 1, 1, t1, &r1).await;
    let n2 = start_inproc_node(&cluster, 2, 1, t2, &r2).await;
    assert!(!dangling_tmp.exists(), "dangling .tmp must be cleaned up");

    for r in [&r1, &r2] {
        register_endpoint_hint(r, cluster.hosts[0], 1, 1, n1.local_addr());
        register_endpoint_hint(r, cluster.hosts[1], 1, 1, n2.local_addr());
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    while n1.role() != ConsensusRole::Leader && n2.role() != ConsensusRole::Leader {
        assert!(
            Instant::now() < deadline,
            "2-of-3 mTLS quorum must elect leader"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let leader = if n1.role() == ConsensusRole::Leader {
        &n1
    } else {
        &n2
    };

    for prop in sample_four_family_proposals(15) {
        leader
            .propose(prop, Duration::from_secs(10))
            .await
            .expect("commit proposal over mTLS");
    }
    let snap_meta = leader
        .create_snapshot_and_compact()
        .expect("create snapshot and compact log");
    assert_eq!(snap_meta.included_index, 6);
    assert_eq!(leader.uncompacted_log_len(), 0);

    // Commit 2 more post-snapshot entries (`7` and `8`).
    for prop in sample_four_family_proposals(16).into_iter().take(2) {
        leader
            .propose(prop, Duration::from_secs(10))
            .await
            .expect("commit post-snapshot proposal");
    }
    assert_eq!(leader.consensus_commit_index(), 8);

    // Bring Node 3 online for the first time: must catch up via chunked InstallSnapshot over mTLS!
    let r3 = Arc::new(MonotonicEndpointResolver::new());
    let mut t3 = staggered_timing_for_roster(cluster.hosts[2].cell, &cluster.roster);
    t3.max_snapshot_chunk_bytes = 192;
    let n3 = start_inproc_node(&cluster, 3, 1, t3, &r3).await;
    for r in [&r1, &r2, &r3] {
        register_endpoint_hint(r, cluster.hosts[0], 1, 2, n1.local_addr());
        register_endpoint_hint(r, cluster.hosts[1], 1, 2, n2.local_addr());
        register_endpoint_hint(r, cluster.hosts[2], 1, 1, n3.local_addr());
    }

    let sync_deadline = Instant::now() + Duration::from_secs(15);
    while n3.consensus_commit_index() < 8 {
        assert!(
            Instant::now() < sync_deadline,
            "Node 3 failed to catch up via InstallSnapshot over mTLS"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(n3.snapshot_metadata().map(|m| m.included_index), Some(6));
    assert_eq!(n3.state_root_digest(), leader.state_root_digest());

    n3.shutdown().await;
    n2.shutdown().await;
    n1.shutdown().await;
}

// ============================================================================
// Test 4: Real OS `Child::kill()` Leader Hard Crash + 2-of-3 Failover + Rejoin
// ============================================================================

async fn boot_and_kill_initial_child_leader(
    cluster: &ProvisionedCluster,
    mut child1: ChildReplicaHandle,
    n2: &MultiHostQuorumNode,
    n3: &MultiHostQuorumNode,
    r2: &MonotonicEndpointResolver,
    r3: &MonotonicEndpointResolver,
) -> (u64, String) {
    // Wire outbound `EndpointHint`s on `child1` FIRST while withholding outbound
    // hints on `r2`/`r3`: `n2` and `n3` answer `PreVote` / `RequestVote` /
    // `AppendEntries` inline over `child1`'s inbound mTLS streams without needing
    // outbound dial capability, preventing `n2`/`n3` from preempting Term 1 Leader
    // if concurrent `WAL+FULL` `fsync` stalls node startup.
    assert_eq!(
        child1.send_cmd(&format!("PEER_HINT 2 1 1 {}", n2.local_addr())),
        "OK HINT"
    );
    assert_eq!(
        child1.send_cmd(&format!("PEER_HINT 3 1 1 {}", n3.local_addr())),
        "OK HINT"
    );

    let elect_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = child1.send_cmd("STATUS");
        if status.starts_with("OK Leader ") {
            break;
        }
        assert!(
            Instant::now() < elect_deadline,
            "Child 1 failed to elect as initial Leader: {status}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // Populate full `EndpointHint` routing on `r2` and `r3` once `child1` is
    // established as initial Leader so the surviving 2-of-3 quorum can fail over
    // immediately after `child1.hard_kill()`.
    for r in [r2, r3] {
        register_endpoint_hint(r, cluster.hosts[0], 1, 1, child1.mtls_addr);
        register_endpoint_hint(r, cluster.hosts[1], 1, 1, n2.local_addr());
        register_endpoint_hint(r, cluster.hosts[2], 1, 1, n3.local_addr());
    }

    let commit_reply = child1.send_cmd("PROPOSE_BATCH 31");
    assert!(
        commit_reply.starts_with("OK COMMITTED 6 "),
        "unexpected batch reply: {commit_reply}"
    );
    let parts: Vec<&str> = commit_reply.split_whitespace().collect();
    let old_leader_term: u64 = parts[3].parse().expect("old_leader_term");
    let pre_crash_digest = parts[4].to_owned();

    // REAL HARD CRASH: kill the Leader OS process abruptly via `Child::kill()`!
    child1.hard_kill();
    (old_leader_term, pre_crash_digest)
}

async fn run_leader_hard_kill_cycle(compact_before_rejoin: bool) {
    let tmp = TempDir::new("leader-hard-kill");
    let cluster = provision_three_host_cluster(&tmp.path);
    write_host_binding_files(&cluster);

    let r2 = Arc::new(MonotonicEndpointResolver::new());
    let r3 = Arc::new(MonotonicEndpointResolver::new());

    let chunk_bytes = if compact_before_rejoin {
        192
    } else {
        16 * 1024
    };
    // Spawn `child1` FIRST (`election_ticks = 16` -> `320ms`) before starting
    // `n2` (`36` -> `720ms`) and `n3` (`58` -> `1160ms`) for rapid initial
    // election and fast 2-of-3 failover after `Child::kill()`.
    let child1 = ChildReplicaHandle::spawn(&cluster.root, 1, 1, 16);
    let n2 = start_inproc_node(
        &cluster,
        2,
        1,
        ConsensusTimingConfig {
            heartbeat_interval_ticks: 2,
            leader_lease_ticks: 6,
            election_timeout_ticks: 36,
            max_snapshot_chunk_bytes: chunk_bytes,
        },
        &r2,
    )
    .await;
    let n3 = start_inproc_node(
        &cluster,
        3,
        1,
        ConsensusTimingConfig {
            heartbeat_interval_ticks: 2,
            leader_lease_ticks: 6,
            election_timeout_ticks: 58,
            max_snapshot_chunk_bytes: chunk_bytes,
        },
        &r3,
    )
    .await;

    let (old_leader_term, pre_crash_digest) =
        boot_and_kill_initial_child_leader(&cluster, child1, &n2, &n3, &r2, &r3).await;

    // Surviving 2-of-3 quorum (`n2` + `n3`) MUST elect a new Leader in `new_term > old_leader_term`!
    let failover_deadline = Instant::now() + Duration::from_secs(15);
    while n2.role() != ConsensusRole::Leader && n3.role() != ConsensusRole::Leader {
        assert!(
            Instant::now() < failover_deadline,
            "surviving 2-of-3 quorum failed to elect successor Leader after Child::kill()"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let new_leader = if n2.role() == ConsensusRole::Leader {
        &n2
    } else {
        &n3
    };
    assert!(new_leader.current_term() > old_leader_term);
    assert_eq!(new_leader.consensus_commit_index(), 6);
    assert_eq!(
        hex_encode_32(&new_leader.state_root_digest()),
        pre_crash_digest
    );

    // Commit 3 additional proposals on the surviving 2-of-3 quorum while Node 1 is dead!
    for prop in sample_four_family_proposals(42).into_iter().take(3) {
        new_leader
            .propose(prop, Duration::from_secs(10))
            .await
            .expect("commit on 2-of-3 surviving quorum");
    }
    assert_eq!(new_leader.consensus_commit_index(), 9);
    if compact_before_rejoin {
        new_leader
            .create_snapshot_and_compact()
            .expect("compact survivor leader before rejoin");
    }
    let canonical_digest = hex_encode_32(&new_leader.state_root_digest());

    // Restart killed Node 1 on `node_boot_generation = 2` over its crash-recovered WAL+FULL store!
    rejoin_killed_leader_and_verify_fence(
        &tmp.path,
        &cluster,
        &n2,
        &n3,
        &r2,
        &r3,
        old_leader_term,
        &canonical_digest,
    )
    .await;

    n3.shutdown().await;
    n2.shutdown().await;
}

#[allow(clippy::too_many_arguments)]
async fn rejoin_killed_leader_and_verify_fence(
    root: &Path,
    cluster: &ProvisionedCluster,
    n2: &MultiHostQuorumNode,
    n3: &MultiHostQuorumNode,
    r2: &Arc<MonotonicEndpointResolver>,
    r3: &Arc<MonotonicEndpointResolver>,
    old_leader_term: u64,
    canonical_digest: &str,
) {
    let child1_rejoined = ChildReplicaHandle::spawn(root, 1, 2, 100);
    for r in [r2, r3] {
        register_endpoint_hint(r, cluster.hosts[0], 2, 1, child1_rejoined.mtls_addr);
    }
    assert_eq!(
        child1_rejoined.send_cmd(&format!("PEER_HINT 2 1 2 {}", n2.local_addr())),
        "OK HINT"
    );
    assert_eq!(
        child1_rejoined.send_cmd(&format!("PEER_HINT 3 1 2 {}", n3.local_addr())),
        "OK HINT"
    );

    let rejoin_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let st = child1_rejoined.send_cmd("STATUS");
        let f: Vec<&str> = st.split_whitespace().collect();
        if f.len() == 5 && f[1] == "Follower" && f[3] == "9" && f[4] == canonical_digest {
            break;
        }
        assert!(
            Instant::now() < rejoin_deadline,
            "restarted old Leader failed to catch up: {st}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    let fence_reply = child1_rejoined.send_cmd(&format!("TRY_STALE_PROPOSE {old_leader_term}"));
    assert!(
        fence_reply.starts_with(&format!("REJECTED_STALE {old_leader_term} ")),
        "expected StaleLeaderTerm fencing on rejoined node, got: {fence_reply}"
    );
    child1_rejoined.graceful_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_mtls_quorum_survives_leader_hard_kill_and_fences_rejoined_old_leader() {
    run_leader_hard_kill_cycle(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "extended hard-kill + compacted snapshot InstallSnapshot rejoin soak"]
async fn ignored_leader_hard_kill_with_compacted_snapshot_rejoin_soak() {
    run_leader_hard_kill_cycle(true).await;
}
