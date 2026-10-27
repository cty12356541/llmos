//! Deterministic, wall-clock-free Raft simulation suite (`ADR-0026`
//! `C-CONSENSUS-CP`, `[CONS-SCOPE-001]`, `[TASK-SCALE-002]`, `[DUR-ACK-002]`,
//! `[DUR-SNAP-001]`).

use std::collections::{BTreeSet, VecDeque};
use std::env;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_cell::{CellEpoch, CellFencingToken, CellIdentity};
use nlos_consensus::{
    AppliedProposalOutcome, CellDirectoryCommand, ConsensusConfigError, ConsensusCore,
    ConsensusEffect, ConsensusError, ConsensusInput, ConsensusRole, ConsensusTerm,
    ConsensusTimingConfig, ControlPlaneAuditCommand, ControlPlaneCommand, ControlPlaneProposal,
    ControlPlaneStateMachine, ForbiddenDataPlaneKind, LogIndex, ProposalRejectReason,
    RejectDataPlanePayload, ReplicatedQuotaLeaseDisposition, RootLeasePoolCommand,
    ScopeViolationError, SingleHostPassthroughNode, SqliteConsensusStore,
    TaskAuthorityShardCommand, ValidatedVoterRoster, VoterReplica, decode_raft_message,
    encode_forbidden_data_plane_wire_probe, encode_raft_message, staggered_timing_for_roster,
};
use nlos_types::{
    ControlDomainId, Generation, IdempotencyKey, PrincipalId, QuotaLeaseId, ReceiptId,
    SchedulerDomainId, TaskAuthorityAssignmentId, TaskId,
};

static SIM_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SIM_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "nlos-consensus-sim-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create sim temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    const fn chance_permille(&mut self, permille: u64) -> bool {
        (self.next_u64() % 1_000) < permille
    }
}

fn cell_id(tag: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([tag; 16]))
}

fn domain_id(tag: u8) -> ControlDomainId {
    ControlDomainId::from_bytes([tag.wrapping_add(0x40); 16])
}

struct InFlightPacket {
    from: CellIdentity,
    to: CellIdentity,
    wire_bytes: Vec<u8>,
    deliver_at_step: u64,
}

struct SimNode {
    id: CellIdentity,
    core: ConsensusCore,
    store: SqliteConsensusStore,
    sm: ControlPlaneStateMachine,
    stepped_down_events: Vec<(ConsensusTerm, ConsensusTerm)>,
    rejected_reasons: Vec<ProposalRejectReason>,
    confirmed_reads: Vec<(u64, ConsensusTerm, LogIndex)>,
}

struct SimCluster {
    _tmp: TempDir,
    nodes: Vec<SimNode>,
    blocked_links: BTreeSet<(CellIdentity, CellIdentity)>,
    queue: VecDeque<InFlightPacket>,
    rng: SplitMix64,
    step_clock: u64,
    drop_permille: u64,
    max_reorder_delay: u64,
}

impl SimCluster {
    fn new(
        label: &str,
        node_count: u8,
        seed: u64,
        max_snapshot_chunk_bytes: Option<usize>,
    ) -> Self {
        let tmp = TempDir::new(label);
        let voters: Vec<VoterReplica> = (1..=node_count)
            .map(|i| VoterReplica {
                cell_identity: cell_id(i),
                control_domain_id: domain_id(i),
            })
            .collect();
        let roster = ValidatedVoterRoster::new(voters).expect("valid independent-domain roster");

        let mut nodes = Vec::with_capacity(usize::from(node_count));
        for (rank, member) in roster.members().iter().enumerate() {
            let mut timing = staggered_timing_for_roster(member.cell_identity, &roster);
            if let Some(chunk_sz) = max_snapshot_chunk_bytes {
                timing.max_snapshot_chunk_bytes = chunk_sz;
            }
            let node_dir = tmp.path.join(format!("node-{rank}"));
            let (store, recovered) =
                SqliteConsensusStore::open_or_recover(&node_dir).expect("open sim store");
            let core = ConsensusCore::rehydrate(
                member.cell_identity,
                roster.clone(),
                timing,
                recovered.core_state,
            )
            .expect("init sim core");
            nodes.push(SimNode {
                id: member.cell_identity,
                core,
                store,
                sm: recovered.state_machine,
                stepped_down_events: Vec::new(),
                rejected_reasons: Vec::new(),
                confirmed_reads: Vec::new(),
            });
        }

        Self {
            _tmp: tmp,
            nodes,
            blocked_links: BTreeSet::new(),
            queue: VecDeque::new(),
            rng: SplitMix64::new(seed),
            step_clock: 0,
            drop_permille: 0,
            max_reorder_delay: 0,
        }
    }

    fn node_idx(&self, id: CellIdentity) -> usize {
        self.nodes
            .iter()
            .position(|n| n.id == id)
            .expect("known sim node")
    }

    fn isolate_group(&mut self, group_a: &[CellIdentity], group_b: &[CellIdentity]) {
        for &a in group_a {
            for &b in group_b {
                self.blocked_links.insert((a, b));
                self.blocked_links.insert((b, a));
            }
        }
    }

    fn heal_all(&mut self) {
        self.blocked_links.clear();
    }

    fn apply_input(&mut self, idx: usize, input: ConsensusInput) {
        let from_id = self.nodes[idx].id;
        let effects = self.nodes[idx].core.step(input);
        {
            let node = &mut self.nodes[idx];
            node.store
                .apply_durable_effects(&effects, &mut node.sm)
                .expect("persist durable effects in WAL+FULL");
            for eff in &effects {
                match eff {
                    ConsensusEffect::LeaderSteppedDown {
                        aborted_term,
                        new_term,
                    } => node.stepped_down_events.push((*aborted_term, *new_term)),
                    ConsensusEffect::ProposalRejected { reason, .. } => {
                        node.rejected_reasons.push(*reason);
                    }
                    ConsensusEffect::LinearizableReadConfirmed {
                        read_id,
                        confirmed_term,
                        commit_index,
                    } => {
                        node.confirmed_reads
                            .push((*read_id, *confirmed_term, *commit_index));
                    }
                    _ => {}
                }
            }
        }

        for eff in effects {
            if let ConsensusEffect::SendMessage { target, message } = eff {
                if self.blocked_links.contains(&(from_id, target)) {
                    continue;
                }
                if self.drop_permille > 0 && self.rng.chance_permille(self.drop_permille) {
                    continue;
                }
                let delay = if self.max_reorder_delay == 0 {
                    0
                } else {
                    self.rng.next_u64() % (self.max_reorder_delay + 1)
                };
                self.queue.push_back(InFlightPacket {
                    from: from_id,
                    to: target,
                    wire_bytes: encode_raft_message(&message),
                    deliver_at_step: self.step_clock.saturating_add(delay),
                });
            }
        }
    }

    fn drain_ready_packets(&mut self) {
        while let Some(pos) = self
            .queue
            .iter()
            .position(|pkt| pkt.deliver_at_step <= self.step_clock)
        {
            let pkt = self.queue.remove(pos).expect("ready packet");
            if self.blocked_links.contains(&(pkt.from, pkt.to)) {
                continue;
            }
            let decoded = decode_raft_message(&pkt.wire_bytes).expect("valid NLOSRF01 wire frame");
            let dst_idx = self.node_idx(pkt.to);
            self.apply_input(
                dst_idx,
                ConsensusInput::ReceiveMessage {
                    from: pkt.from,
                    message: decoded,
                },
            );
        }
    }

    fn tick_all(&mut self, ticks: u32) {
        for _ in 0..ticks {
            self.step_clock = self.step_clock.saturating_add(1);
            for idx in 0..self.nodes.len() {
                self.apply_input(idx, ConsensusInput::Tick);
            }
            self.drain_ready_packets();
        }
    }

    fn active_leader_idx(&self) -> Option<usize> {
        let max_term = self
            .nodes
            .iter()
            .map(|n| n.core.current_term())
            .max()
            .unwrap_or(0);
        self.nodes.iter().position(|n| {
            n.core.role() == ConsensusRole::Leader && n.core.current_term() == max_term
        })
    }

    fn wait_for_leader(&mut self, max_ticks: u32) -> usize {
        for _ in 0..max_ticks {
            if let Some(idx) = self.active_leader_idx() {
                return idx;
            }
            self.tick_all(1);
        }
        panic!("cluster failed to elect a leader within {max_ticks} ticks");
    }
}

fn build_lease_and_directory_proposals(batch_seed: u8) -> Vec<ControlPlaneProposal> {
    let c1 = cell_id(1);
    let lease_id = QuotaLeaseId::from_bytes([batch_seed.wrapping_add(0x30); 16]);
    let budget_root = [batch_seed.wrapping_add(0x40); 16];
    let fence_scope = [batch_seed.wrapping_add(0x50); 16];

    vec![
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([batch_seed, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            ControlPlaneCommand::CellDirectoryMutation(CellDirectoryCommand::UpsertCellEndpoint {
                cell: c1,
                control_domain_id: domain_id(1),
                principal_id: PrincipalId::from_bytes([batch_seed.wrapping_add(1); 16]),
                node_boot_generation: Generation::INITIAL,
                epoch: CellEpoch::INITIAL,
                heartbeat_seq: u64::from(batch_seed) + 1,
                endpoint_hint: Some(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    19_000 + u16::from(batch_seed),
                )),
            }),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([batch_seed, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]),
            ControlPlaneCommand::RootLeasePoolMutation(RootLeasePoolCommand::RegisterQuotaPool {
                budget_root,
                initial_available: 10_000,
            }),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([batch_seed, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]),
            ControlPlaneCommand::RootLeasePoolMutation(RootLeasePoolCommand::AllocateQuotaLease {
                lease_id,
                budget_root,
                holder_cell: c1,
                node_boot_generation: Generation::INITIAL,
                face_value: 2_500,
                epoch: CellEpoch::INITIAL,
                fencing_token: CellFencingToken::INITIAL,
                fence_scope,
            }),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([batch_seed, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4]),
            ControlPlaneCommand::RootLeasePoolMutation(
                RootLeasePoolCommand::SettleOrQuarantineQuotaLease {
                    lease_id,
                    disposition: ReplicatedQuotaLeaseDisposition::Quarantined,
                    returned_amount: 0,
                    quarantined_amount: 2_500,
                    new_epoch: CellEpoch::INITIAL.checked_next().unwrap(),
                    new_fencing_token: CellFencingToken::INITIAL.checked_next().unwrap(),
                },
            ),
        ),
    ]
}

fn build_four_family_proposals(batch_seed: u8) -> Vec<ControlPlaneProposal> {
    let mut out = build_lease_and_directory_proposals(batch_seed);
    let c1 = cell_id(1);
    let c2 = cell_id(2);
    let task_id = TaskId::from_bytes([batch_seed.wrapping_add(0x10); 16]);
    let a1 = TaskAuthorityAssignmentId::from_bytes([batch_seed.wrapping_add(0x20); 16]);
    let a2 = TaskAuthorityAssignmentId::from_bytes([batch_seed.wrapping_add(0x21); 16]);

    out.extend([
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([batch_seed, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5]),
            ControlPlaneCommand::TaskAuthorityShardMutation(
                TaskAuthorityShardCommand::AssignInitialAuthority {
                    assignment_id: a1,
                    task_id,
                    task_generation: Generation::INITIAL,
                    authority_shard: [batch_seed; 16],
                    home_cell: c1,
                    authority_term: NonZeroU64::new(1).unwrap(),
                    control_epoch: 1,
                    participant_registry_generation: 1,
                    participant_registry_root: [0xA1; 32],
                },
            ),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([batch_seed, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6]),
            ControlPlaneCommand::TaskAuthorityShardMutation(
                TaskAuthorityShardCommand::RecordCompletedTakeover {
                    takeover_receipt_id: ReceiptId::from_bytes([batch_seed.wrapping_add(0x60); 16]),
                    task_id,
                    task_generation: Generation::INITIAL,
                    old_assignment_id: a1,
                    new_assignment_id: a2,
                    frozen_old_authority_term: NonZeroU64::new(1).unwrap(),
                    new_authority_term: NonZeroU64::new(2).unwrap(),
                    new_home_cell: c2,
                    new_control_epoch: 2,
                    new_participant_registry_generation: 2,
                    new_participant_registry_root: [0xA2; 32],
                    exact_fence_set_root: [0xEE; 32],
                    outstanding_effect_quarantine_root: Some([0xCC; 32]),
                },
            ),
        ),
        ControlPlaneProposal::new(
            IdempotencyKey::from_bytes([batch_seed, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7]),
            ControlPlaneCommand::ControlPlaneAuditAnchor(
                ControlPlaneAuditCommand::AnchorReconciliationCheckpoint {
                    cell: c2,
                    node_boot_generation: Generation::INITIAL,
                    epoch: CellEpoch::INITIAL.checked_next().unwrap(),
                    checkpoint_seq: NonZeroU64::new(1).unwrap(),
                    checkpoint_digest: [batch_seed.wrapping_add(0x90); 32],
                },
            ),
        ),
    ]);
    out
}

fn verify_seeded_convergence(node_count: u8, seed: u64) {
    let mut sim = SimCluster::new(&format!("converge-n{node_count}"), node_count, seed, None);
    let leader_idx = sim.wait_for_leader(60);
    sim.drop_permille = 120;
    sim.max_reorder_delay = 3;

    let proposals = build_four_family_proposals(node_count);
    for prop in &proposals {
        let l_idx = sim.wait_for_leader(80);
        sim.apply_input(
            l_idx,
            ConsensusInput::Propose {
                proposal: *prop,
                expected_leader_term: None,
            },
        );
        sim.drain_ready_packets();
        sim.tick_all(6);
    }

    sim.drop_permille = 0;
    sim.max_reorder_delay = 0;
    sim.tick_all(25);

    let expected_len = proposals.len() as u64;
    let leader_digest = sim.nodes[leader_idx].sm.state_root_digest();
    for node in &sim.nodes {
        assert_eq!(node.core.consensus_commit_index(), expected_len);
        assert_eq!(node.sm.state_root_digest(), leader_digest);
        let takeover_id = ReceiptId::from_bytes([node_count.wrapping_add(0x60); 16]);
        let takeover = node
            .sm
            .takeover_receipt(takeover_id)
            .expect("committed takeover receipt must exist");
        assert_eq!(takeover.consensus_commit_index.get(), 6);
    }
}

#[test]
fn three_and_five_node_quorums_converge_across_all_four_families_under_reorder_and_drop() {
    verify_seeded_convergence(3, 0xC09C_54B3_0003);
    verify_seeded_convergence(5, 0xC09C_54B3_0005);
}

#[test]
fn isolated_minority_uses_pre_vote_without_inflating_term_or_disrupting_active_leader() {
    let mut sim = SimCluster::new("prevote-minority", 3, 0x1111_2222, None);
    let leader_idx = sim.wait_for_leader(40);
    sim.tick_all(6);

    let active_leader = sim.nodes[leader_idx].id;
    let initial_term = sim.nodes[leader_idx].core.current_term();
    let minority_id = sim
        .nodes
        .iter()
        .find(|n| n.id != active_leader)
        .expect("follower")
        .id;
    let majority_ids: Vec<CellIdentity> = sim
        .nodes
        .iter()
        .map(|n| n.id)
        .filter(|&id| id != minority_id)
        .collect();

    sim.isolate_group(&[minority_id], &majority_ids);
    // Advance many election timeouts while minority node is completely isolated.
    sim.tick_all(80);

    let minority_idx = sim.node_idx(minority_id);
    assert_eq!(
        sim.nodes[minority_idx].core.current_term(),
        initial_term,
        "Pre-Vote must prevent isolated minority from inflating current_term"
    );
    assert_eq!(sim.nodes[leader_idx].core.role(), ConsensusRole::Leader);
    assert_eq!(sim.nodes[leader_idx].core.current_term(), initial_term);

    // Reconnect minority: active Leader stays undisturbed in `initial_term`!
    sim.heal_all();
    sim.tick_all(12);
    assert_eq!(sim.nodes[leader_idx].core.role(), ConsensusRole::Leader);
    assert_eq!(sim.nodes[leader_idx].core.current_term(), initial_term);
    assert_eq!(sim.nodes[minority_idx].core.role(), ConsensusRole::Follower);
    assert_eq!(
        sim.nodes[minority_idx].core.known_leader(),
        Some(active_leader)
    );
}

#[test]
fn deposed_leader_steps_down_on_heal_and_rejects_stale_proposals_and_reads() {
    let mut sim = SimCluster::new("stale-leader-fence", 3, 0x3333_4444, None);
    let old_leader_idx = sim.wait_for_leader(40);
    let old_leader_id = sim.nodes[old_leader_idx].id;
    let old_term = sim.nodes[old_leader_idx].core.current_term();

    let props = build_four_family_proposals(10);
    sim.apply_input(
        old_leader_idx,
        ConsensusInput::Propose {
            proposal: props[0],
            expected_leader_term: Some(old_term),
        },
    );
    sim.drain_ready_packets();
    sim.tick_all(6);
    assert_eq!(sim.nodes[old_leader_idx].core.consensus_commit_index(), 1);

    // Partition old Leader from the 2-of-3 majority.
    let majority_peers: Vec<CellIdentity> = sim
        .nodes
        .iter()
        .map(|n| n.id)
        .filter(|&id| id != old_leader_id)
        .collect();
    sim.isolate_group(&[old_leader_id], &majority_peers);

    // Request a linearizable read on isolated old Leader: cannot confirm without quorum!
    sim.apply_input(
        old_leader_idx,
        ConsensusInput::RequestLinearizableRead {
            read_id: 99,
            expected_leader_term: Some(old_term),
        },
    );
    sim.drain_ready_packets();
    sim.tick_all(35);
    assert_eq!(sim.nodes[old_leader_idx].confirmed_reads.len(), 0);
    assert_eq!(
        sim.nodes[old_leader_idx]
            .core
            .leader_lease()
            .remaining_ticks,
        0,
        "isolated leader monotonic lease must expire without quorum heartbeats"
    );

    // Majority elects a new Leader in `new_term > old_term` and commits `props[1]`.
    let new_leader_idx = sim
        .nodes
        .iter()
        .position(|n| n.id != old_leader_id && n.core.role() == ConsensusRole::Leader)
        .expect("majority partition must elect a new leader");
    let new_term = sim.nodes[new_leader_idx].core.current_term();
    assert!(new_term > old_term);

    sim.apply_input(
        new_leader_idx,
        ConsensusInput::Propose {
            proposal: props[1],
            expected_leader_term: Some(new_term),
        },
    );
    sim.drain_ready_packets();
    sim.tick_all(6);
    assert_eq!(sim.nodes[new_leader_idx].core.consensus_commit_index(), 2);

    // Heal partition: old Leader immediately steps down and rejects stale term!
    sim.heal_all();
    sim.tick_all(8);
    assert_eq!(
        sim.nodes[old_leader_idx].core.role(),
        ConsensusRole::Follower
    );
    assert!(
        sim.nodes[old_leader_idx]
            .stepped_down_events
            .iter()
            .any(|&(aborted, higher)| aborted == old_term && higher >= new_term)
    );

    sim.nodes[old_leader_idx].rejected_reasons.clear();
    sim.apply_input(
        old_leader_idx,
        ConsensusInput::Propose {
            proposal: props[2],
            expected_leader_term: Some(old_term),
        },
    );
    sim.apply_input(
        old_leader_idx,
        ConsensusInput::RequestLinearizableRead {
            read_id: 100,
            expected_leader_term: Some(old_term),
        },
    );
    assert_eq!(sim.nodes[old_leader_idx].rejected_reasons.len(), 2);
    for reason in &sim.nodes[old_leader_idx].rejected_reasons {
        assert!(matches!(
            reason,
            ProposalRejectReason::StaleLeaderTerm { attempted_term, current_term, .. }
                if *attempted_term == old_term && *current_term >= new_term
        ));
    }
}

#[test]
fn conflicting_uncommitted_tail_truncates_and_overwrites_without_touching_committed_prefix() {
    let mut sim = SimCluster::new("tail-truncate", 3, 0x5555_6666, None);
    let old_leader_idx = sim.wait_for_leader(40);
    let old_leader_id = sim.nodes[old_leader_idx].id;

    // Commit prefix 1..=3 across the cluster.
    let prefix = build_four_family_proposals(20);
    for prop in prefix.iter().take(3) {
        sim.apply_input(
            old_leader_idx,
            ConsensusInput::Propose {
                proposal: *prop,
                expected_leader_term: None,
            },
        );
        sim.drain_ready_packets();
        sim.tick_all(4);
    }
    for node in &sim.nodes {
        assert_eq!(node.core.consensus_commit_index(), 3);
    }

    // Isolate old Leader and append uncommitted conflicting entries 4..=6 on it.
    let majority_peers: Vec<CellIdentity> = sim
        .nodes
        .iter()
        .map(|n| n.id)
        .filter(|&id| id != old_leader_id)
        .collect();
    sim.isolate_group(&[old_leader_id], &majority_peers);

    let stale_branch = build_four_family_proposals(30);
    for prop in stale_branch.iter().take(3) {
        sim.apply_input(
            old_leader_idx,
            ConsensusInput::Propose {
                proposal: *prop,
                expected_leader_term: None,
            },
        );
    }
    assert_eq!(sim.nodes[old_leader_idx].core.last_log_index(), 6);
    assert_eq!(sim.nodes[old_leader_idx].core.consensus_commit_index(), 3);

    // Majority elects new Leader and commits canonical entries 4..=7.
    sim.tick_all(35);
    let new_leader_idx = sim
        .nodes
        .iter()
        .position(|n| n.id != old_leader_id && n.core.role() == ConsensusRole::Leader)
        .expect("new majority leader");
    let canonical_branch = build_four_family_proposals(40);
    for prop in canonical_branch.iter().take(4) {
        sim.apply_input(
            new_leader_idx,
            ConsensusInput::Propose {
                proposal: *prop,
                expected_leader_term: None,
            },
        );
        sim.drain_ready_packets();
        sim.tick_all(4);
    }
    assert_eq!(sim.nodes[new_leader_idx].core.consensus_commit_index(), 7);

    // Heal partition: old Leader truncates 4..=6 and converges to canonical 1..=7!
    sim.heal_all();
    sim.tick_all(15);
    let expected_digest = sim.nodes[new_leader_idx].sm.state_root_digest();
    for node in &sim.nodes {
        assert_eq!(node.core.consensus_commit_index(), 7);
        assert_eq!(node.core.last_log_index(), 7);
        assert_eq!(node.sm.state_root_digest(), expected_digest);
        for (i, expected_prop) in prefix.iter().take(3).enumerate() {
            let stored = node
                .store
                .read_log_entry((i as u64) + 1)
                .expect("read log")
                .expect("committed prefix entry");
            assert_eq!(&stored.proposal, expected_prop);
        }
    }
}

#[test]
fn figure_8_prior_term_entry_is_not_committed_until_current_term_entry_reaches_quorum() {
    let mut sim = SimCluster::new("figure-8", 5, 0x7777_8888, None);
    let n1 = cell_id(1);
    let n2 = cell_id(2);
    let n3 = cell_id(3);
    let n4 = cell_id(4);
    let n5 = cell_id(5);

    // Step 1: Elect N1 in term 1; isolate N3, N4, N5 before proposing index 1 so
    // index 1 (term 1) replicates ONLY to {N1, N2} (2 of 5 < quorum 3).
    let l1_idx = sim.wait_for_leader(40);
    assert_eq!(sim.nodes[l1_idx].id, n1);
    assert_eq!(sim.nodes[l1_idx].core.current_term(), 1);
    sim.isolate_group(&[n1, n2], &[n3, n4, n5]);

    let props = build_four_family_proposals(50);
    sim.apply_input(
        l1_idx,
        ConsensusInput::Propose {
            proposal: props[0],
            expected_leader_term: Some(1),
        },
    );
    sim.drain_ready_packets();
    sim.tick_all(4);
    assert_eq!(sim.nodes[sim.node_idx(n1)].core.last_log_index(), 1);
    assert_eq!(sim.nodes[sim.node_idx(n2)].core.last_log_index(), 1);
    assert_eq!(sim.nodes[l1_idx].core.consensus_commit_index(), 0);

    // Step 2: {N3, N4, N5} elect N3 in term >= 2 (so N4 has current_term >= 2 and empty log).
    sim.tick_all(35);
    let higher_term = sim.nodes[sim.node_idx(n4)].core.current_term();
    assert!(higher_term >= 2);

    // Step 3: N3 (the term-2 leader) is isolated; {N1, N2, N4} form a 3-of-5 majority.
    // N1 has the freshest log (term=1, idx=1) and lowest staggered timeout, so N1
    // learns `higher_term` from N4 and wins election in `term_3 > higher_term`!
    sim.heal_all();
    sim.isolate_group(&[n1, n2, n4], &[n3, n5]);
    sim.tick_all(40);
    let n1_idx = sim.node_idx(n1);
    assert_eq!(
        sim.nodes[n1_idx].core.role(),
        ConsensusRole::Leader,
        "N1 has the most up-to-date log (term=1, idx=1) so it must win election on {{N1,N2,N4}}"
    );
    let term_3 = sim.nodes[n1_idx].core.current_term();
    assert!(term_3 > higher_term);

    // Now N1, N2, AND N4 (3 of 5 = majority!) hold entry 1 from term 1, yet NO entry
    // from `term_3` exists yet -> Raft §5.4.2 forbids committing entry 1!
    sim.tick_all(6);
    assert_eq!(sim.nodes[sim.node_idx(n4)].core.last_log_index(), 1);
    assert_eq!(
        sim.nodes[n1_idx].core.consensus_commit_index(),
        0,
        "Raft §5.4.2: prior-term entry MUST NOT commit by counting replicas alone"
    );

    // Step 4: Propose entry 2 in `term_3`: once replicated to {N1, N2, N4}, BOTH
    // entry 1 and entry 2 commit atomically!
    sim.apply_input(
        n1_idx,
        ConsensusInput::Propose {
            proposal: props[1],
            expected_leader_term: Some(term_3),
        },
    );
    sim.drain_ready_packets();
    sim.tick_all(6);
    assert_eq!(sim.nodes[n1_idx].core.consensus_commit_index(), 2);
    assert_eq!(sim.nodes[sim.node_idx(n4)].core.consensus_commit_index(), 2);
}

#[test]
fn chunked_install_snapshot_catches_up_lagging_follower_after_leader_compaction() {
    // Force 128-byte chunks so the snapshot transfers across multiple RPC chunks!
    let mut sim = SimCluster::new("install-snapshot", 3, 0x9999_AAAA, Some(128));
    let leader_idx = sim.wait_for_leader(40);
    let leader_id = sim.nodes[leader_idx].id;
    let lagging_id = sim
        .nodes
        .iter()
        .rev()
        .find(|n| n.id != leader_id)
        .expect("follower")
        .id;
    let active_pair: Vec<CellIdentity> = sim
        .nodes
        .iter()
        .map(|n| n.id)
        .filter(|&id| id != lagging_id)
        .collect();

    // Isolate lagging follower while committing 7 proposals on the 2-of-3 quorum.
    sim.isolate_group(&[lagging_id], &active_pair);
    let batch_a = build_four_family_proposals(60);
    for prop in &batch_a {
        sim.apply_input(
            leader_idx,
            ConsensusInput::Propose {
                proposal: *prop,
                expected_leader_term: None,
            },
        );
        sim.drain_ready_packets();
        sim.tick_all(4);
    }
    assert_eq!(sim.nodes[leader_idx].core.consensus_commit_index(), 7);

    // Compact Leader's log completely (`uncompacted_log().is_empty()`).
    {
        let leader = &mut sim.nodes[leader_idx];
        let snap_bytes = leader.sm.encode_snapshot();
        assert!(
            snap_bytes.len() > 256,
            "snapshot must span >= 3 chunks at 128B"
        );
        let inc_idx = leader.sm.last_applied_index();
        let inc_term = leader.sm.last_applied_term();
        let meta = leader
            .store
            .create_snapshot(inc_idx, inc_term, &snap_bytes)
            .expect("create leader snapshot");
        leader
            .store
            .compact_log_up_to(inc_idx)
            .expect("compact leader log");
        assert!(leader.core.record_local_snapshot(meta, snap_bytes, inc_idx));
        assert_eq!(leader.core.uncompacted_log().len(), 0);
    }

    // Propose 2 post-snapshot entries (indices 8 and 9) before healing.
    let batch_b = build_four_family_proposals(70);
    for prop in batch_b.iter().take(2) {
        sim.apply_input(
            leader_idx,
            ConsensusInput::Propose {
                proposal: *prop,
                expected_leader_term: None,
            },
        );
        sim.drain_ready_packets();
        sim.tick_all(4);
    }
    assert_eq!(sim.nodes[leader_idx].core.consensus_commit_index(), 9);

    // Reconnect lagging follower: must catch up via multi-chunk InstallSnapshot + AppendEntries!
    sim.heal_all();
    sim.tick_all(25);

    let lag_idx = sim.node_idx(lagging_id);
    assert_eq!(sim.nodes[lag_idx].core.consensus_commit_index(), 9);
    assert_eq!(
        sim.nodes[lag_idx]
            .core
            .snapshot_metadata()
            .map(|m| m.included_index),
        Some(7)
    );
    assert_eq!(
        sim.nodes[lag_idx].sm.state_root_digest(),
        sim.nodes[leader_idx].sm.state_root_digest()
    );
}

#[test]
fn cons_scope_001_wire_guard_rejects_all_forbidden_data_plane_payloads() {
    let tmp = TempDir::new("scope-guard");
    let single = SingleHostPassthroughNode::open(&tmp.path, cell_id(1), domain_id(1))
        .expect("open single-host node");

    let forbidden_kinds = [
        ForbiddenDataPlaneKind::TaskExecutionMutation,
        ForbiddenDataPlaneKind::FiberScheduleEvent,
        ForbiddenDataPlaneKind::TopicMessagePublish,
        ForbiddenDataPlaneKind::SemanticEventAppend,
        ForbiddenDataPlaneKind::ArtifactBulkPayload,
        ForbiddenDataPlaneKind::UnknownNonControlFamily(0xFE),
    ];

    for kind in forbidden_kinds {
        let probe = encode_forbidden_data_plane_wire_probe(
            kind,
            IdempotencyKey::from_bytes([kind.wire_tag(); 16]),
            b"high-frequency-data-plane-payload",
        );
        let decode_err =
            ControlPlaneProposal::decode_wire(&probe).expect_err("data-plane wire tag must fail");
        assert!(matches!(
            decode_err,
            ScopeViolationError::RejectDataPlanePayload(RejectDataPlanePayload {
                kind: k,
                wire_tag,
                ..
            }) if k == kind && wire_tag == kind.wire_tag()
        ));

        let node_err = single
            .propose_wire(&probe)
            .expect_err("SingleHostPassthroughNode must reject data-plane wire frame");
        assert!(matches!(
            node_err,
            ConsensusError::ScopeViolation(ScopeViolationError::RejectDataPlanePayload(_))
        ));
    }
    assert_eq!(single.consensus_commit_index(), 0);
    assert!(matches!(
        single
            .propose(build_four_family_proposals(1)[0])
            .expect("permitted control-plane command commits")
            .outcome,
        AppliedProposalOutcome::Applied { .. }
    ));
}

#[test]
fn task_scale_002_voter_roster_rejects_shared_control_domains_and_invalid_sizes() {
    assert_eq!(
        ValidatedVoterRoster::new(Vec::new()),
        Err(ConsensusConfigError::EmptyVoterSet)
    );
    assert_eq!(
        ValidatedVoterRoster::new(vec![
            VoterReplica {
                cell_identity: cell_id(1),
                control_domain_id: domain_id(1),
            },
            VoterReplica {
                cell_identity: cell_id(2),
                control_domain_id: domain_id(2),
            },
        ]),
        Err(ConsensusConfigError::UnsupportedTwoNodeQuorum)
    );
    assert_eq!(
        ValidatedVoterRoster::new(vec![
            VoterReplica {
                cell_identity: cell_id(1),
                control_domain_id: domain_id(1),
            },
            VoterReplica {
                cell_identity: cell_id(1),
                control_domain_id: domain_id(2),
            },
            VoterReplica {
                cell_identity: cell_id(3),
                control_domain_id: domain_id(3),
            },
        ]),
        Err(ConsensusConfigError::DuplicateCellIdentity(cell_id(1)))
    );

    let shared_domain = domain_id(9);
    let shared_err = ValidatedVoterRoster::new(vec![
        VoterReplica {
            cell_identity: cell_id(1),
            control_domain_id: shared_domain,
        },
        VoterReplica {
            cell_identity: cell_id(2),
            control_domain_id: domain_id(2),
        },
        VoterReplica {
            cell_identity: cell_id(3),
            control_domain_id: shared_domain,
        },
    ])
    .expect_err("shared ControlDomainId must fail closed");
    assert_eq!(
        shared_err,
        ConsensusConfigError::NonIndependentControlDomain {
            control_domain_id: shared_domain,
            first_cell: cell_id(1),
            second_cell: cell_id(3),
        }
    );

    assert!(matches!(
        ConsensusTimingConfig {
            heartbeat_interval_ticks: 5,
            leader_lease_ticks: 5,
            election_timeout_ticks: 10,
            max_snapshot_chunk_bytes: 1024,
        }
        .validate(),
        Err(ConsensusConfigError::InvalidTimingBounds { .. })
    ));
}
