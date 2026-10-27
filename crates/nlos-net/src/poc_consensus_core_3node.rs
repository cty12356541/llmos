//! Pure I/O-free deterministic `ConsensusCore` state machine `PoC` seed
//! (`ADR-0026` §2.1, §3.2, §6.3, §7.1 promotion gate).
//!
//! Strictly separates pure consensus safety transitions from sockets, timers,
//! and disk I/O so both `N = 1` degenerate single-host direct commit and `N = 3`
//! majority quorum election, `consensus_commit_index` advancement, stale-term
//! leader fencing, and partition-heal log convergence can be verified under
//! deterministic simulation before `ROAD-C-001-XHOST` extracts `nlos-consensus`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;

use nlos_cell::{CellEpoch, CellFencingToken, CellIdentity};
use nlos_types::{ControlDomainId, Generation};

/// Monotonic consensus election term (`>= 1` once any election begins; `0` at
/// cold uninitialized boot).
pub type ConsensusTerm = u64;

/// 1-based monotonic consensus log index (`0` denotes the empty log sentinel).
pub type LogIndex = u64;

/// Voter descriptor binding a nominal [`CellIdentity`] to its failure-domain
/// [`ControlDomainId`] (`[TASK-SCALE-002]`, `ADR-0026` §2.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoterMember {
    /// Nominal 16-byte voter Cell identity.
    pub cell_identity: CellIdentity,
    /// Disjoint physical/administrative failure domain of this voter.
    pub control_domain_id: ControlDomainId,
}

/// Fail-closed roster validation error (`[TASK-SCALE-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoterRosterError {
    /// Voter roster cannot be empty.
    EmptyRoster,
    /// Local node identity is not a member of the voter roster.
    LocalNodeNotInRoster(CellIdentity),
    /// Duplicate `CellIdentity` in voter list.
    DuplicateCellIdentity(CellIdentity),
    /// Two voters in an `N > 1` cluster share the same `ControlDomainId`,
    /// violating failure-domain disjointness (`[TASK-SCALE-002]`).
    DuplicateControlDomain {
        /// Shared `ControlDomainId`.
        control_domain_id: ControlDomainId,
        /// First voter claiming this failure domain.
        first_cell: CellIdentity,
        /// Second voter attempting to claim the same failure domain.
        second_cell: CellIdentity,
    },
}

impl fmt::Display for VoterRosterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRoster => formatter.write_str("consensus voter roster cannot be empty"),
            Self::LocalNodeNotInRoster(cell) => {
                write!(formatter, "local Cell {cell:?} is absent from voter roster")
            }
            Self::DuplicateCellIdentity(cell) => {
                write!(formatter, "duplicate voter CellIdentity {cell:?}")
            }
            Self::DuplicateControlDomain {
                control_domain_id,
                first_cell,
                second_cell,
            } => write!(
                formatter,
                "[TASK-SCALE-002] voters {first_cell:?} and {second_cell:?} share ControlDomainId {control_domain_id:?}"
            ),
        }
    }
}

impl Error for VoterRosterError {}

/// Validated voter roster enforcing `[TASK-SCALE-002]` failure-domain
/// disjointness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoterRoster {
    members: Vec<VoterMember>,
}

impl VoterRoster {
    /// Validates a voter list under `[TASK-SCALE-002]`.
    ///
    /// # Errors
    ///
    /// Rejects empty rosters, duplicate `CellIdentity`s, or duplicate
    /// `ControlDomainId`s when `voters.len() > 1`.
    pub fn new(voters: Vec<VoterMember>) -> Result<Self, VoterRosterError> {
        if voters.is_empty() {
            return Err(VoterRosterError::EmptyRoster);
        }
        let mut seen_cells = BTreeSet::new();
        let mut seen_domains: BTreeMap<ControlDomainId, CellIdentity> = BTreeMap::new();
        let enforce_disjoint_domains = voters.len() > 1;

        for member in &voters {
            if !seen_cells.insert(member.cell_identity) {
                return Err(VoterRosterError::DuplicateCellIdentity(
                    member.cell_identity,
                ));
            }
            if enforce_disjoint_domains
                && let Some(&existing) = seen_domains.get(&member.control_domain_id)
            {
                return Err(VoterRosterError::DuplicateControlDomain {
                    control_domain_id: member.control_domain_id,
                    first_cell: existing,
                    second_cell: member.cell_identity,
                });
            }
            seen_domains.insert(member.control_domain_id, member.cell_identity);
        }

        let mut sorted = voters;
        sorted.sort_by_key(|m| m.cell_identity);
        Ok(Self { members: sorted })
    }

    /// Number of voting members (`N`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Always `false` for a validated [`VoterRoster`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Majority quorum threshold `(N / 2) + 1` (`1` for `N = 1`, `2` for `N = 3`).
    #[must_use]
    pub fn quorum_size(&self) -> usize {
        (self.members.len() / 2) + 1
    }

    /// Returns `true` if `cell` is a member of this voter roster.
    #[must_use]
    pub fn contains(&self, cell: CellIdentity) -> bool {
        self.members.iter().any(|m| m.cell_identity == cell)
    }

    /// Canonical slice of validated voter members.
    #[must_use]
    pub fn members(&self) -> &[VoterMember] {
        &self.members
    }
}

/// Strictly scoped control-plane metadata payload permitted inside the
/// consensus log (`[CONS-SCOPE-001]`, `ADR-0026` §2.1 & §3.2).
///
/// High-frequency data-plane execution events, audit streams, and bulk artifact
/// bytes are structurally unrepresentable in this enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneLogPayload {
    /// Authoritative Cell membership / incarnation announcement.
    CellDirectoryUpdate {
        /// Nominal Cell identity.
        cell: CellIdentity,
        /// Monotonic boot generation.
        node_boot_generation: Generation,
        /// Observed Cell epoch.
        epoch: CellEpoch,
    },
    /// Monotonic advancement of the cluster root authority epoch and fencing
    /// token.
    RootLeaseEpochAdvance {
        /// Newly committed cluster epoch.
        new_epoch: CellEpoch,
        /// Monotonic fencing token paired with `new_epoch`.
        fencing_token: CellFencingToken,
        /// Authoritative holder Cell.
        holder: CellIdentity,
    },
    /// Authoritative task ownership / lease assignment across failure domains.
    TaskAuthorityAssignment {
        /// 16-byte canonical task identifier.
        task_key: [u8; 16],
        /// Assigned authoritative Cell.
        authoritative_cell: CellIdentity,
        /// Lease epoch gating mutations.
        lease_epoch: CellEpoch,
    },
    /// Post-partition reconciliation barrier anchor digest.
    ReconciliationAnchor {
        /// Reconciled epoch.
        epoch: CellEpoch,
        /// Domain-separated uncertainty digest being resolved.
        uncertainty_digest: [u8; 32],
    },
}

/// Replicated consensus log entry (`(index, term, payload)`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsensusLogEntry {
    /// 1-based monotonic log index.
    pub index: LogIndex,
    /// Election term of the leader that created this entry.
    pub term: ConsensusTerm,
    /// Scoped control-plane metadata payload (`[CONS-SCOPE-001]`).
    pub payload: ControlPlaneLogPayload,
}

/// Current Raft role of a [`ConsensusCore`] instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsensusRole {
    /// Passive follower tracking leader heartbeats / election timeout.
    Follower,
    /// Candidate soliciting majority votes for `current_term`.
    Candidate,
    /// Elected majority leader for `current_term`.
    Leader,
}

/// Wire-serializable Raft consensus protocol message exchanged between
/// [`ConsensusCore`] peers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsensusMessage {
    /// Candidate vote solicitation (`RequestVote`).
    RequestVote {
        /// Candidate's new election term.
        term: ConsensusTerm,
        /// Candidate's nominal identity.
        candidate_id: CellIdentity,
        /// Index of candidate's last log entry.
        last_log_index: LogIndex,
        /// Term of candidate's last log entry.
        last_log_term: ConsensusTerm,
    },
    /// Voter response to [`Self::RequestVote`].
    RequestVoteReply {
        /// Responder's authoritative `current_term`.
        term: ConsensusTerm,
        /// Responder's nominal identity.
        voter_id: CellIdentity,
        /// `true` iff `voter_id` granted its single vote in `term` to the
        /// candidate.
        vote_granted: bool,
    },
    /// Leader log replication, heartbeat, or `ReadIndex` quorum round
    /// (`AppendEntries`).
    AppendEntries {
        /// Leader's `current_term`.
        term: ConsensusTerm,
        /// Leader's nominal identity.
        leader_id: CellIdentity,
        /// Index of log entry immediately preceding `entries`.
        prev_log_index: LogIndex,
        /// Term of `prev_log_index` (`0` when `prev_log_index == 0`).
        prev_log_term: ConsensusTerm,
        /// Log entries to replicate (empty for heartbeats / `ReadIndex`).
        entries: Vec<ConsensusLogEntry>,
        /// Leader's `consensus_commit_index`.
        leader_commit: LogIndex,
        /// Optional `ReadIndex` quorum correlation round id.
        read_round_id: Option<u64>,
    },
    /// Follower response to [`Self::AppendEntries`].
    AppendEntriesReply {
        /// Follower's authoritative `current_term`.
        term: ConsensusTerm,
        /// Follower's nominal identity.
        follower_id: CellIdentity,
        /// `true` iff `(prev_log_index, prev_log_term)` matched on the follower.
        success: bool,
        /// Highest log index known to be replicated on `follower_id` when
        /// `success == true`, or conflict hint index when `success == false`.
        match_or_hint_index: LogIndex,
        /// Echoed `ReadIndex` round correlation id, if any.
        read_round_id: Option<u64>,
    },
}

/// Deterministic input events driving [`ConsensusCore::step`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsensusInput {
    /// Logical tick advancing election or heartbeat clocks (`1` tick).
    LogicalTick,
    /// Client/controller proposal of a scoped control-plane metadata mutation.
    Propose(ControlPlaneLogPayload),
    /// Request a linearizable `ReadIndex` confirmation fenced by a live
    /// current-term majority heartbeat round (`ADR-0026` §2.3).
    RequestLinearizableRead {
        /// Caller-supplied correlation identifier.
        read_id: u64,
    },
    /// Inbound protocol message from `from`.
    ReceiveMessage {
        /// Authenticated sender `CellIdentity`.
        from: CellIdentity,
        /// Consensus protocol payload.
        message: ConsensusMessage,
    },
}

/// Refusal reason when a non-leader or fenced stale leader rejects a proposal
/// or linearizable read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProposalRejectReason {
    /// Node is not the active leader; `known_leader` provides a routing hint if
    /// available.
    NotLeader {
        /// Current known leader in `current_term`, if any.
        known_leader: Option<CellIdentity>,
        /// Node's `current_term`.
        current_term: ConsensusTerm,
    },
}

/// Pure deterministic output effects emitted by [`ConsensusCore::step`].
///
/// In production `nlos-consensus`, the runtime shell executes `PersistHardState`
/// and `AppendLogEntries` (`WAL+FULL` `fsync`) BEFORE transmitting `SendMessage`
/// or exposing `CommittedEntries` (`ADR-0026` §3.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsensusEffect {
    /// Persist `(current_term, voted_for)` durably before sending any messages.
    PersistHardState {
        /// Authoritative election term.
        current_term: ConsensusTerm,
        /// Durable vote record in `current_term`.
        voted_for: Option<CellIdentity>,
    },
    /// Truncate uncommitted conflicting log suffix starting at `from_index`
    /// (inclusive).
    TruncateLogSuffixFrom {
        /// First 1-based index to remove.
        from_index: LogIndex,
    },
    /// Append new replicated entries to the durable WAL.
    AppendLogEntries(Vec<ConsensusLogEntry>),
    /// Send a consensus RPC frame to `target` (resolved via `EndpointResolver`).
    SendMessage {
        /// Target voter `CellIdentity`.
        target: CellIdentity,
        /// Outbound consensus message.
        message: ConsensusMessage,
    },
    /// Newly committed control-plane entries (`old_commit + 1 ..= new_commit`)
    /// ready for state-machine application.
    AdvanceCommitIndex {
        /// New `consensus_commit_index`.
        commit_index: LogIndex,
        /// Term in which commitment occurred.
        term: ConsensusTerm,
        /// Ordered newly committed entries.
        newly_committed: Vec<ConsensusLogEntry>,
    },
    /// Confirms that a linearizable read at `commit_index` was fenced by a live
    /// majority quorum in `confirmed_term`.
    LinearizableReadConfirmed {
        /// Caller-supplied read correlation id.
        read_id: u64,
        /// Confirmed leader term.
        confirmed_term: ConsensusTerm,
        /// Authoritative `consensus_commit_index` at read confirmation.
        commit_index: LogIndex,
    },
    /// Proposal or linearizable read was rejected fail-closed (`NotLeader` or
    /// fenced stale term).
    ProposalRejected(ProposalRejectReason),
}

#[derive(Clone, Debug)]
struct PendingReadRound {
    read_id: u64,
    term: ConsensusTerm,
    commit_index_at_issue: LogIndex,
    acks: BTreeSet<CellIdentity>,
}

/// Pure, I/O-free deterministic Raft consensus state machine (`ADR-0026` §3.2,
/// §6.3, §7.1).
#[derive(Clone, Debug)]
pub struct ConsensusCore {
    local_id: CellIdentity,
    roster: VoterRoster,
    role: ConsensusRole,
    current_term: ConsensusTerm,
    voted_for: Option<CellIdentity>,
    known_leader: Option<CellIdentity>,
    log: Vec<ConsensusLogEntry>,
    commit_index: LogIndex,
    election_timeout_ticks: u32,
    heartbeat_interval_ticks: u32,
    ticks_since_event: u32,
    votes_granted_in_term: BTreeSet<CellIdentity>,
    next_index: BTreeMap<CellIdentity, LogIndex>,
    match_index: BTreeMap<CellIdentity, LogIndex>,
    next_read_round_id: u64,
    pending_reads: BTreeMap<u64, PendingReadRound>,
}

impl ConsensusCore {
    /// Constructs a validated `ConsensusCore` instance.
    ///
    /// - For `N = 1` (`roster.len() == 1`), initializes immediately in
    ///   degenerate direct-commit `Leader` mode (`term = 1`, zero election
    ///   traffic; `ADR-0026` §6.3).
    /// - For `N >= 2`, initializes as `Follower` in `term = 0` with
    ///   deterministic staggered `election_timeout_ticks` supplied by caller.
    ///
    /// # Errors
    ///
    /// Fails closed if `local_id` is not a member of `roster`.
    pub fn new(
        local_id: CellIdentity,
        roster: VoterRoster,
        election_timeout_ticks: u32,
        heartbeat_interval_ticks: u32,
    ) -> Result<Self, VoterRosterError> {
        if !roster.contains(local_id) {
            return Err(VoterRosterError::LocalNodeNotInRoster(local_id));
        }

        if roster.len() == 1 {
            let mut next_index = BTreeMap::new();
            let mut match_index = BTreeMap::new();
            next_index.insert(local_id, 1);
            match_index.insert(local_id, 0);
            let mut votes = BTreeSet::new();
            votes.insert(local_id);
            return Ok(Self {
                local_id,
                roster,
                role: ConsensusRole::Leader,
                current_term: 1,
                voted_for: Some(local_id),
                known_leader: Some(local_id),
                log: Vec::new(),
                commit_index: 0,
                election_timeout_ticks: election_timeout_ticks.max(2),
                heartbeat_interval_ticks: heartbeat_interval_ticks.max(1),
                ticks_since_event: 0,
                votes_granted_in_term: votes,
                next_index,
                match_index,
                next_read_round_id: 1,
                pending_reads: BTreeMap::new(),
            });
        }

        Ok(Self {
            local_id,
            roster,
            role: ConsensusRole::Follower,
            current_term: 0,
            voted_for: None,
            known_leader: None,
            log: Vec::new(),
            commit_index: 0,
            election_timeout_ticks: election_timeout_ticks.max(2),
            heartbeat_interval_ticks: heartbeat_interval_ticks.max(1),
            ticks_since_event: 0,
            votes_granted_in_term: BTreeSet::new(),
            next_index: BTreeMap::new(),
            match_index: BTreeMap::new(),
            next_read_round_id: 1,
            pending_reads: BTreeMap::new(),
        })
    }

    /// Nominal `CellIdentity` of this consensus node.
    #[must_use]
    pub const fn local_id(&self) -> CellIdentity {
        self.local_id
    }

    /// Current consensus role (`Follower`, `Candidate`, or `Leader`).
    #[must_use]
    pub const fn role(&self) -> ConsensusRole {
        self.role
    }

    /// Current monotonic election term.
    #[must_use]
    pub const fn current_term(&self) -> ConsensusTerm {
        self.current_term
    }

    /// Candidate voted for in `current_term`, if any.
    #[must_use]
    pub const fn voted_for(&self) -> Option<CellIdentity> {
        self.voted_for
    }

    /// Currently recognized leader in `current_term`, if any.
    #[must_use]
    pub const fn known_leader(&self) -> Option<CellIdentity> {
        self.known_leader
    }

    /// Authoritative `consensus_commit_index` (`ADR-0026` §3.2).
    #[must_use]
    pub const fn consensus_commit_index(&self) -> LogIndex {
        self.commit_index
    }

    /// Highest log index currently appended locally (`>= consensus_commit_index`).
    #[must_use]
    pub fn last_log_index(&self) -> LogIndex {
        self.log.len() as LogIndex
    }

    /// Term of [`Self::last_log_index`] (`0` if log is empty).
    #[must_use]
    pub fn last_log_term(&self) -> ConsensusTerm {
        self.log.last().map_or(0, |e| e.term)
    }

    /// Full slice of locally stored [`ConsensusLogEntry`] items.
    #[must_use]
    pub fn log_entries(&self) -> &[ConsensusLogEntry] {
        &self.log
    }

    /// Committed prefix of the consensus log (`1 ..= consensus_commit_index`).
    #[must_use]
    pub fn committed_entries(&self) -> &[ConsensusLogEntry] {
        let committed_len = usize::try_from(self.commit_index).unwrap_or(0);
        &self.log[..committed_len.min(self.log.len())]
    }

    /// Pure deterministic state transition (`(self, input) -> Vec<ConsensusEffect>`).
    pub fn step(&mut self, input: ConsensusInput) -> Vec<ConsensusEffect> {
        match input {
            ConsensusInput::LogicalTick => self.handle_tick(),
            ConsensusInput::Propose(payload) => self.handle_propose(payload),
            ConsensusInput::RequestLinearizableRead { read_id } => {
                self.handle_linearizable_read(read_id)
            }
            ConsensusInput::ReceiveMessage { from, message } => self.handle_message(from, message),
        }
    }

    fn handle_tick(&mut self) -> Vec<ConsensusEffect> {
        // Degenerate `N = 1` desktop profile: zero election timers, zero
        // heartbeat frames (`ADR-0026` §6.3).
        if self.roster.len() == 1 {
            return Vec::new();
        }

        self.ticks_since_event = self.ticks_since_event.saturating_add(1);
        match self.role {
            ConsensusRole::Leader => {
                if self.ticks_since_event >= self.heartbeat_interval_ticks {
                    self.ticks_since_event = 0;
                    self.broadcast_append_entries(None)
                } else {
                    Vec::new()
                }
            }
            ConsensusRole::Follower | ConsensusRole::Candidate => {
                if self.ticks_since_event >= self.election_timeout_ticks {
                    self.start_election()
                } else {
                    Vec::new()
                }
            }
        }
    }

    fn start_election(&mut self) -> Vec<ConsensusEffect> {
        self.current_term = self.current_term.saturating_add(1);
        self.role = ConsensusRole::Candidate;
        self.voted_for = Some(self.local_id);
        self.known_leader = None;
        self.ticks_since_event = 0;
        self.pending_reads.clear();
        self.votes_granted_in_term.clear();
        self.votes_granted_in_term.insert(self.local_id);

        let mut effects = vec![ConsensusEffect::PersistHardState {
            current_term: self.current_term,
            voted_for: self.voted_for,
        }];

        let last_log_index = self.last_log_index();
        let last_log_term = self.last_log_term();
        for member in self.roster.members() {
            if member.cell_identity == self.local_id {
                continue;
            }
            effects.push(ConsensusEffect::SendMessage {
                target: member.cell_identity,
                message: ConsensusMessage::RequestVote {
                    term: self.current_term,
                    candidate_id: self.local_id,
                    last_log_index,
                    last_log_term,
                },
            });
        }
        effects
    }

    fn handle_propose(&mut self, payload: ControlPlaneLogPayload) -> Vec<ConsensusEffect> {
        if self.role != ConsensusRole::Leader {
            return vec![ConsensusEffect::ProposalRejected(
                ProposalRejectReason::NotLeader {
                    known_leader: self.known_leader,
                    current_term: self.current_term,
                },
            )];
        }

        let new_index = self.last_log_index().saturating_add(1);
        let entry = ConsensusLogEntry {
            index: new_index,
            term: self.current_term,
            payload,
        };
        self.log.push(entry);
        self.match_index.insert(self.local_id, new_index);
        self.next_index
            .insert(self.local_id, new_index.saturating_add(1));

        let mut effects = vec![ConsensusEffect::AppendLogEntries(vec![entry])];

        if self.roster.len() == 1 {
            // `N = 1` single-host direct commit (`quorum_size == 1`).
            self.commit_index = new_index;
            effects.push(ConsensusEffect::AdvanceCommitIndex {
                commit_index: new_index,
                term: self.current_term,
                newly_committed: vec![entry],
            });
            return effects;
        }

        effects.extend(self.broadcast_append_entries(None));
        effects
    }

    fn handle_linearizable_read(&mut self, read_id: u64) -> Vec<ConsensusEffect> {
        if self.role != ConsensusRole::Leader {
            return vec![ConsensusEffect::ProposalRejected(
                ProposalRejectReason::NotLeader {
                    known_leader: self.known_leader,
                    current_term: self.current_term,
                },
            )];
        }

        if self.roster.len() == 1 {
            return vec![ConsensusEffect::LinearizableReadConfirmed {
                read_id,
                confirmed_term: self.current_term,
                commit_index: self.commit_index,
            }];
        }

        let round_id = self.next_read_round_id;
        self.next_read_round_id = self.next_read_round_id.saturating_add(1);
        let mut acks = BTreeSet::new();
        acks.insert(self.local_id);
        self.pending_reads.insert(
            round_id,
            PendingReadRound {
                read_id,
                term: self.current_term,
                commit_index_at_issue: self.commit_index,
                acks,
            },
        );
        self.broadcast_append_entries(Some(round_id))
    }

    fn handle_message(
        &mut self,
        from: CellIdentity,
        message: ConsensusMessage,
    ) -> Vec<ConsensusEffect> {
        if !self.roster.contains(from) {
            return Vec::new();
        }
        match message {
            ConsensusMessage::RequestVote {
                term,
                candidate_id,
                last_log_index,
                last_log_term,
            } => {
                if candidate_id != from {
                    return Vec::new();
                }
                self.on_request_vote(term, candidate_id, last_log_index, last_log_term)
            }
            ConsensusMessage::RequestVoteReply {
                term,
                voter_id,
                vote_granted,
            } => {
                if voter_id != from {
                    return Vec::new();
                }
                self.on_request_vote_reply(term, voter_id, vote_granted)
            }
            ConsensusMessage::AppendEntries {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
                read_round_id,
            } => {
                if leader_id != from {
                    return Vec::new();
                }
                self.on_append_entries(
                    term,
                    leader_id,
                    prev_log_index,
                    prev_log_term,
                    entries,
                    leader_commit,
                    read_round_id,
                )
            }
            ConsensusMessage::AppendEntriesReply {
                term,
                follower_id,
                success,
                match_or_hint_index,
                read_round_id,
            } => {
                if follower_id != from {
                    return Vec::new();
                }
                self.on_append_entries_reply(
                    term,
                    follower_id,
                    success,
                    match_or_hint_index,
                    read_round_id,
                )
            }
        }
    }

    fn step_down_to_higher_term(&mut self, higher_term: ConsensusTerm) -> ConsensusEffect {
        self.current_term = higher_term;
        self.role = ConsensusRole::Follower;
        self.voted_for = None;
        self.known_leader = None;
        self.votes_granted_in_term.clear();
        self.pending_reads.clear();
        ConsensusEffect::PersistHardState {
            current_term: self.current_term,
            voted_for: None,
        }
    }

    fn on_request_vote(
        &mut self,
        term: ConsensusTerm,
        candidate_id: CellIdentity,
        cand_last_index: LogIndex,
        cand_last_term: ConsensusTerm,
    ) -> Vec<ConsensusEffect> {
        let mut effects = Vec::new();
        if term > self.current_term {
            effects.push(self.step_down_to_higher_term(term));
        }

        if term < self.current_term {
            effects.push(ConsensusEffect::SendMessage {
                target: candidate_id,
                message: ConsensusMessage::RequestVoteReply {
                    term: self.current_term,
                    voter_id: self.local_id,
                    vote_granted: false,
                },
            });
            return effects;
        }

        let vote_available = self.voted_for.is_none() || self.voted_for == Some(candidate_id);
        let log_up_to_date =
            (cand_last_term, cand_last_index) >= (self.last_log_term(), self.last_log_index());

        let granted = vote_available && log_up_to_date;
        if granted {
            self.voted_for = Some(candidate_id);
            self.ticks_since_event = 0;
            effects.push(ConsensusEffect::PersistHardState {
                current_term: self.current_term,
                voted_for: self.voted_for,
            });
        }

        effects.push(ConsensusEffect::SendMessage {
            target: candidate_id,
            message: ConsensusMessage::RequestVoteReply {
                term: self.current_term,
                voter_id: self.local_id,
                vote_granted: granted,
            },
        });
        effects
    }

    fn on_request_vote_reply(
        &mut self,
        term: ConsensusTerm,
        voter_id: CellIdentity,
        vote_granted: bool,
    ) -> Vec<ConsensusEffect> {
        if term > self.current_term {
            return vec![self.step_down_to_higher_term(term)];
        }
        if self.role != ConsensusRole::Candidate || term != self.current_term {
            return Vec::new();
        }
        if vote_granted {
            self.votes_granted_in_term.insert(voter_id);
        }

        if self.votes_granted_in_term.len() >= self.roster.quorum_size() {
            self.become_leader()
        } else {
            Vec::new()
        }
    }

    fn become_leader(&mut self) -> Vec<ConsensusEffect> {
        self.role = ConsensusRole::Leader;
        self.known_leader = Some(self.local_id);
        self.ticks_since_event = 0;
        self.next_index.clear();
        self.match_index.clear();
        let next_idx = self.last_log_index().saturating_add(1);
        for member in self.roster.members() {
            if member.cell_identity == self.local_id {
                self.next_index.insert(member.cell_identity, next_idx);
                self.match_index
                    .insert(member.cell_identity, self.last_log_index());
            } else {
                self.next_index.insert(member.cell_identity, next_idx);
                self.match_index.insert(member.cell_identity, 0);
            }
        }
        self.broadcast_append_entries(None)
    }

    #[allow(clippy::too_many_arguments)]
    fn on_append_entries(
        &mut self,
        term: ConsensusTerm,
        leader_id: CellIdentity,
        prev_log_index: LogIndex,
        prev_log_term: ConsensusTerm,
        entries: Vec<ConsensusLogEntry>,
        leader_commit: LogIndex,
        read_round_id: Option<u64>,
    ) -> Vec<ConsensusEffect> {
        let mut effects = Vec::new();
        if term > self.current_term {
            effects.push(self.step_down_to_higher_term(term));
        }
        if term < self.current_term {
            effects.push(ConsensusEffect::SendMessage {
                target: leader_id,
                message: ConsensusMessage::AppendEntriesReply {
                    term: self.current_term,
                    follower_id: self.local_id,
                    success: false,
                    match_or_hint_index: self.last_log_index(),
                    read_round_id,
                },
            });
            return effects;
        }

        // Valid current-term leader heartbeat/replication resets election clock
        // and converts any Candidate in the same term back to Follower.
        self.role = ConsensusRole::Follower;
        self.known_leader = Some(leader_id);
        self.ticks_since_event = 0;
        self.pending_reads.clear();

        if !self.matches_prev_log(prev_log_index, prev_log_term) {
            let hint = self.last_log_index().min(prev_log_index.saturating_sub(1));
            effects.push(ConsensusEffect::SendMessage {
                target: leader_id,
                message: ConsensusMessage::AppendEntriesReply {
                    term: self.current_term,
                    follower_id: self.local_id,
                    success: false,
                    match_or_hint_index: hint,
                    read_round_id,
                },
            });
            return effects;
        }

        // Reconcile incoming entries with local log (Raft §5.3): truncate ONLY
        // when an existing uncommitted entry conflicts in term at the same index.
        let mut newly_appended = Vec::new();
        for entry in entries {
            let Some(pos) = usize::try_from(entry.index.saturating_sub(1)).ok() else {
                continue;
            };
            if let Some(existing) = self.log.get(pos) {
                if existing.term != entry.term {
                    // Never truncate committed entries (`pos + 1 > commit_index`).
                    if entry.index > self.commit_index {
                        self.log.truncate(pos);
                        effects.push(ConsensusEffect::TruncateLogSuffixFrom {
                            from_index: entry.index,
                        });
                        self.log.push(entry);
                        newly_appended.push(entry);
                    }
                }
            } else if pos == self.log.len() {
                self.log.push(entry);
                newly_appended.push(entry);
            }
        }
        if !newly_appended.is_empty() {
            effects.push(ConsensusEffect::AppendLogEntries(newly_appended));
        }

        if leader_commit > self.commit_index {
            let old_commit = self.commit_index;
            let new_commit = leader_commit.min(self.last_log_index());
            if new_commit > old_commit {
                self.commit_index = new_commit;
                let start = usize::try_from(old_commit).unwrap_or(0);
                let end = usize::try_from(new_commit).unwrap_or(0);
                effects.push(ConsensusEffect::AdvanceCommitIndex {
                    commit_index: new_commit,
                    term: self.current_term,
                    newly_committed: self.log[start..end].to_vec(),
                });
            }
        }

        effects.push(ConsensusEffect::SendMessage {
            target: leader_id,
            message: ConsensusMessage::AppendEntriesReply {
                term: self.current_term,
                follower_id: self.local_id,
                success: true,
                match_or_hint_index: self.last_log_index(),
                read_round_id,
            },
        });
        effects
    }

    fn on_append_entries_reply(
        &mut self,
        term: ConsensusTerm,
        follower_id: CellIdentity,
        success: bool,
        match_or_hint_index: LogIndex,
        read_round_id: Option<u64>,
    ) -> Vec<ConsensusEffect> {
        if term > self.current_term {
            // Higher-term reply immediately fences this stale leader (`ADR-0026` §2.3)!
            return vec![self.step_down_to_higher_term(term)];
        }
        if self.role != ConsensusRole::Leader || term != self.current_term {
            return Vec::new();
        }

        let mut effects = Vec::new();
        if success {
            let prev_match = self.match_index.get(&follower_id).copied().unwrap_or(0);
            let updated_match = prev_match
                .max(match_or_hint_index)
                .min(self.last_log_index());
            self.match_index.insert(follower_id, updated_match);
            self.next_index
                .insert(follower_id, updated_match.saturating_add(1));

            if let Some(advance) = self.try_advance_leader_commit() {
                effects.push(advance);
            }

            if let Some(round_id) = read_round_id
                && let Some(round) = self.pending_reads.get_mut(&round_id)
                && round.term == self.current_term
            {
                round.acks.insert(follower_id);
                if round.acks.len() >= self.roster.quorum_size() {
                    let completed = self.pending_reads.remove(&round_id).expect("present");
                    effects.push(ConsensusEffect::LinearizableReadConfirmed {
                        read_id: completed.read_id,
                        confirmed_term: self.current_term,
                        commit_index: self.commit_index.max(completed.commit_index_at_issue),
                    });
                }
            }
        } else {
            let next_try = match_or_hint_index.saturating_add(1).max(1);
            self.next_index.insert(follower_id, next_try);
            effects.push(self.build_append_entries_for_peer(follower_id, None));
        }

        effects
    }

    fn try_advance_leader_commit(&mut self) -> Option<ConsensusEffect> {
        let quorum = self.roster.quorum_size();
        let mut best_commit = self.commit_index;

        for candidate_idx in (self.commit_index + 1)..=self.last_log_index() {
            let Some(entry) = self
                .log
                .get(usize::try_from(candidate_idx - 1).unwrap_or(usize::MAX))
            else {
                continue;
            };
            // Raft §5.4.2 safety rule: only commit entries from `current_term` by
            // counting replicas (earlier-term entries commit transitively).
            if entry.term != self.current_term {
                continue;
            }
            let replicated = self
                .roster
                .members()
                .iter()
                .filter(|m| {
                    self.match_index.get(&m.cell_identity).copied().unwrap_or(0) >= candidate_idx
                })
                .count();
            if replicated >= quorum {
                best_commit = candidate_idx;
            }
        }

        if best_commit > self.commit_index {
            let old = self.commit_index;
            self.commit_index = best_commit;
            let start = usize::try_from(old).unwrap_or(0);
            let end = usize::try_from(best_commit).unwrap_or(0);
            Some(ConsensusEffect::AdvanceCommitIndex {
                commit_index: best_commit,
                term: self.current_term,
                newly_committed: self.log[start..end].to_vec(),
            })
        } else {
            None
        }
    }

    fn matches_prev_log(&self, prev_log_index: LogIndex, prev_log_term: ConsensusTerm) -> bool {
        if prev_log_index == 0 {
            return prev_log_term == 0;
        }
        let Some(pos) = usize::try_from(prev_log_index - 1).ok() else {
            return false;
        };
        self.log
            .get(pos)
            .is_some_and(|entry| entry.term == prev_log_term)
    }

    fn broadcast_append_entries(&self, read_round_id: Option<u64>) -> Vec<ConsensusEffect> {
        let mut effects = Vec::new();
        for member in self.roster.members() {
            if member.cell_identity == self.local_id {
                continue;
            }
            effects.push(self.build_append_entries_for_peer(member.cell_identity, read_round_id));
        }
        effects
    }

    fn build_append_entries_for_peer(
        &self,
        peer: CellIdentity,
        read_round_id: Option<u64>,
    ) -> ConsensusEffect {
        let next_idx = self
            .next_index
            .get(&peer)
            .copied()
            .unwrap_or_else(|| self.last_log_index().saturating_add(1))
            .max(1);
        let prev_log_index = next_idx - 1;
        let prev_log_term = if prev_log_index == 0 {
            0
        } else {
            self.log
                .get(usize::try_from(prev_log_index - 1).unwrap_or(usize::MAX))
                .map_or(0, |e| e.term)
        };
        let entries = if next_idx <= self.last_log_index() {
            let start = usize::try_from(next_idx - 1).unwrap_or(0);
            self.log[start..].to_vec()
        } else {
            Vec::new()
        };

        ConsensusEffect::SendMessage {
            target: peer,
            message: ConsensusMessage::AppendEntries {
                term: self.current_term,
                leader_id: self.local_id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit: self.commit_index,
                read_round_id,
            },
        }
    }
}

/// Deterministic in-memory multi-node consensus simulator with link-level
/// partition and heal controls (`ADR-0026` §7.1 `poc_consensus_core_3node`).
#[derive(Debug)]
pub struct DeterministicConsensusHarness {
    nodes: BTreeMap<CellIdentity, ConsensusCore>,
    in_flight: VecDeque<(CellIdentity, CellIdentity, ConsensusMessage)>,
    blocked_links: BTreeSet<(CellIdentity, CellIdentity)>,
    dropped_message_count: u64,
}

impl DeterministicConsensusHarness {
    /// Builds a deterministic 3-node (or `N`-node) cluster harness with
    /// staggered election timeouts (`base_election_ticks + 2 * i`).
    ///
    /// # Errors
    ///
    /// Fails closed if `voters` violates `[TASK-SCALE-002]` (`VoterRoster::new`).
    pub fn new_cluster(
        voters: Vec<VoterMember>,
        base_election_ticks: u32,
        heartbeat_ticks: u32,
    ) -> Result<Self, VoterRosterError> {
        let roster = VoterRoster::new(voters)?;
        let mut nodes = BTreeMap::new();
        for (idx, member) in roster.members().iter().enumerate() {
            let offset = u32::try_from(idx).unwrap_or(0).saturating_mul(3);
            let core = ConsensusCore::new(
                member.cell_identity,
                roster.clone(),
                base_election_ticks.saturating_add(offset),
                heartbeat_ticks,
            )?;
            nodes.insert(member.cell_identity, core);
        }
        Ok(Self {
            nodes,
            in_flight: VecDeque::new(),
            blocked_links: BTreeSet::new(),
            dropped_message_count: 0,
        })
    }

    /// Immutable reference to a node's [`ConsensusCore`].
    #[must_use]
    pub fn node(&self, cell: CellIdentity) -> Option<&ConsensusCore> {
        self.nodes.get(&cell)
    }

    /// Returns all nodes currently in `ConsensusRole::Leader` paired with their
    /// `current_term`.
    #[must_use]
    pub fn current_leaders(&self) -> Vec<(CellIdentity, ConsensusTerm)> {
        self.nodes
            .iter()
            .filter(|(_, core)| core.role() == ConsensusRole::Leader)
            .map(|(&cell, core)| (cell, core.current_term()))
            .collect()
    }

    /// Returns the single leader in the highest election term, if any.
    #[must_use]
    pub fn highest_term_leader(&self) -> Option<(CellIdentity, ConsensusTerm)> {
        self.current_leaders()
            .into_iter()
            .max_by_key(|&(_, term)| term)
    }

    /// Symmetrically isolates `isolated` from all other voters in the cluster.
    pub fn isolate_node(&mut self, isolated: CellIdentity) {
        let all_ids: Vec<CellIdentity> = self.nodes.keys().copied().collect();
        for peer in all_ids {
            if peer != isolated {
                self.blocked_links.insert((isolated, peer));
                self.blocked_links.insert((peer, isolated));
            }
        }
    }

    /// Blocks directed message delivery from `from -> to`.
    pub fn block_directed_link(&mut self, from: CellIdentity, to: CellIdentity) {
        self.blocked_links.insert((from, to));
    }

    /// Heals all network partitions across the cluster.
    pub fn heal_all_links(&mut self) {
        self.blocked_links.clear();
    }

    /// Total messages dropped by simulated partitions.
    #[must_use]
    pub const fn dropped_message_count(&self) -> u64 {
        self.dropped_message_count
    }

    /// Steps `cell` with `input`, queueing any outbound `SendMessage` effects
    /// (or dropping them if `(cell, target)` is currently partitioned).
    pub fn step_node(&mut self, cell: CellIdentity, input: ConsensusInput) -> Vec<ConsensusEffect> {
        let Some(core) = self.nodes.get_mut(&cell) else {
            return Vec::new();
        };
        let effects = core.step(input);
        self.enqueue_outbound(cell, &effects);
        effects
    }

    /// Advances every node by 1 logical tick and drains all resulting message
    /// exchanges until quiescence (bounded by `max_deliveries`).
    pub fn tick_all_and_drain(
        &mut self,
        max_deliveries: usize,
    ) -> Vec<(CellIdentity, ConsensusEffect)> {
        let ids: Vec<CellIdentity> = self.nodes.keys().copied().collect();
        let mut collected = Vec::new();
        for id in ids {
            let effs = self.step_node(id, ConsensusInput::LogicalTick);
            for eff in effs {
                collected.push((id, eff));
            }
        }
        collected.extend(self.drain_messages(max_deliveries));
        collected
    }

    /// Delivers queued in-flight messages FIFO up to `max_deliveries` steps.
    pub fn drain_messages(
        &mut self,
        max_deliveries: usize,
    ) -> Vec<(CellIdentity, ConsensusEffect)> {
        let mut collected = Vec::new();
        let mut delivered = 0usize;
        while delivered < max_deliveries {
            let Some((from, to, message)) = self.in_flight.pop_front() else {
                break;
            };
            delivered += 1;
            if self.blocked_links.contains(&(from, to)) {
                self.dropped_message_count = self.dropped_message_count.saturating_add(1);
                continue;
            }
            let effs = self.step_node(to, ConsensusInput::ReceiveMessage { from, message });
            for eff in effs {
                collected.push((to, eff));
            }
        }
        collected
    }

    fn enqueue_outbound(&mut self, from: CellIdentity, effects: &[ConsensusEffect]) {
        for eff in effects {
            if let ConsensusEffect::SendMessage { target, message } = eff {
                if self.blocked_links.contains(&(from, *target)) {
                    self.dropped_message_count = self.dropped_message_count.saturating_add(1);
                } else {
                    self.in_flight.push_back((from, *target, message.clone()));
                }
            }
        }
    }
}
