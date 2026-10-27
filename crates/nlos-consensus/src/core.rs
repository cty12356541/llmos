//! Pure I/O-free deterministic Raft consensus state machine (`ADR-0026` §2.1,
//! §3.2, §3.3 `C-CONSENSUS-CP`, §6.3).
//!
//! # Safety Invariants Enforced
//!
//! 1. **Pre-Vote (`RequestPreVote` / `RequestPreVoteResponse`, Ongaro §9.6)**:
//!    Partitioned minority nodes solicit pre-votes at `current_term + 1`
//!    without incrementing `current_term` or mutating `voted_for`, preventing
//!    term inflation and leader disruption upon reconnection; healthy followers
//!    additionally enforce monotonic leader-lease stickiness.
//! 2. **Single-Vote-Per-Term & Election Restriction (§5.2, §5.4.1)**:
//!    Each replica grants at most one vote per term and only to candidates
//!    whose `(last_log_term, last_log_index)` is at least as up-to-date.
//! 3. **Log Matching & Fast Conflict-Term / Conflict-Index Truncation (§5.3)**:
//!    Committed prefixes are immutable; conflicting uncommitted suffixes back
//!    up by whole terms in a single RTT.
//! 4. **Current-Term Majority Commit Rule (§5.4.2)**:
//!    `commit_index` advances ONLY when an entry created in `current_term` is
//!    replicated across a majority of distinct `ControlDomainId` voters; prior
//!    term entries are never committed by counting replicas alone.
//! 5. **Term-Bound Monotonic Leader Lease & `ReadIndex` Quorum Fence**:
//!    Zero wall-clock comparisons; leadership read fences rely strictly on
//!    current-term majority heartbeat round acknowledgements and local
//!    monotonic tick bounds.
//! 6. **`[TASK-SCALE-002]` Independent Failure-Domain Quorum Guard**:
//!    Every voter in an `N >= 3` configuration MUST bind a distinct
//!    [`ControlDomainId`] and [`CellIdentity`]; duplicate domains fail closed
//!    with [`ConsensusConfigError::NonIndependentControlDomain`].

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use nlos_cell::CellIdentity;
use nlos_types::ControlDomainId;
use sha2::{Digest, Sha256};

use crate::state_machine::ControlPlaneProposal;

/// Monotonic consensus election term (`0` at uninitialized multi-node boot;
/// `>= 1` once any election begins).
pub type ConsensusTerm = u64;

/// 1-based monotonic Raft log index (`0` denotes empty log).
pub type LogIndex = u64;

/// Default maximum payload chunk size (`16 KiB`) for `InstallSnapshotChunk`
/// frames, fitting comfortably within `nlos-net` `ControlRpc` limits.
pub const DEFAULT_SNAPSHOT_CHUNK_BYTES: usize = 16 * 1024;

const LOG_ENTRY_HASH_DOMAIN: &[u8] = b"llmos/consensus/log-entry/v1";
const SNAPSHOT_WIRE_HASH_DOMAIN: &[u8] = b"llmos/consensus/install-snapshot/v1";

/// Voting replica identity paired with its independent failure/control domain
/// (`[TASK-SCALE-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoterReplica {
    /// Nominal 16-byte `CellIdentity` (`[DIST-NAME-001]`).
    pub cell_identity: CellIdentity,
    /// Independent physical/administrative failure domain (`[TASK-SCALE-002]`).
    pub control_domain_id: ControlDomainId,
}

/// Fail-closed configuration validation errors (`[TASK-SCALE-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsensusConfigError {
    /// Voter roster cannot be empty.
    EmptyVoterSet,
    /// Duplicate `CellIdentity` in voter configuration.
    DuplicateCellIdentity(CellIdentity),
    /// Two or more voters share the same `ControlDomainId` (`[TASK-SCALE-002]`).
    NonIndependentControlDomain {
        /// Shared `ControlDomainId` violating failure-domain independence.
        control_domain_id: ControlDomainId,
        /// First voter bound to `control_domain_id`.
        first_cell: CellIdentity,
        /// Second voter attempting to reuse `control_domain_id`.
        second_cell: CellIdentity,
    },
    /// `N = 2` quorum offers zero crash fault tolerance (`quorum = 2`); multi-host
    /// Raft requires `N = 1` (single-host passthrough) or `N >= 3`.
    UnsupportedTwoNodeQuorum,
    /// Local node `CellIdentity` is absent from the validated voter roster.
    LocalCellNotInRoster(CellIdentity),
    /// Monotonic tick parameters violate `1 <= heartbeat < leader_lease < election_timeout`.
    InvalidTimingBounds {
        /// Configured heartbeat tick interval.
        heartbeat_ticks: u32,
        /// Configured monotonic leader-lease tick bound.
        leader_lease_ticks: u32,
        /// Configured election-timeout tick bound.
        election_timeout_ticks: u32,
    },
}

impl fmt::Display for ConsensusConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyVoterSet => formatter.write_str("consensus voter set cannot be empty"),
            Self::DuplicateCellIdentity(cell) => {
                write!(formatter, "duplicate voter CellIdentity {cell:?}")
            }
            Self::NonIndependentControlDomain {
                control_domain_id,
                first_cell,
                second_cell,
            } => write!(
                formatter,
                "[TASK-SCALE-002] non-independent quorum: {first_cell:?} and {second_cell:?} share ControlDomainId {control_domain_id:?}"
            ),
            Self::UnsupportedTwoNodeQuorum => {
                formatter.write_str("N=2 quorum has zero fault tolerance; configure N=1 or N>=3")
            }
            Self::LocalCellNotInRoster(cell) => {
                write!(
                    formatter,
                    "local CellIdentity {cell:?} is not in voter roster"
                )
            }
            Self::InvalidTimingBounds {
                heartbeat_ticks,
                leader_lease_ticks,
                election_timeout_ticks,
            } => write!(
                formatter,
                "invalid monotonic tick bounds: require 1 <= heartbeat({heartbeat_ticks}) < leader_lease({leader_lease_ticks}) < election_timeout({election_timeout_ticks})"
            ),
        }
    }
}

impl Error for ConsensusConfigError {}

/// Validated voter roster enforcing `[TASK-SCALE-002]` failure-domain
/// independence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedVoterRoster {
    members: Vec<VoterReplica>,
}

impl ValidatedVoterRoster {
    /// Validates a voter list under `[TASK-SCALE-002]`.
    ///
    /// # Errors
    ///
    /// Fails closed on empty rosters, duplicate `CellIdentity`s, duplicate
    /// `ControlDomainId`s (`NonIndependentControlDomain`), or `N = 2`.
    pub fn new(voters: Vec<VoterReplica>) -> Result<Self, ConsensusConfigError> {
        if voters.is_empty() {
            return Err(ConsensusConfigError::EmptyVoterSet);
        }
        let mut seen_cells = BTreeSet::new();
        let mut seen_domains: BTreeMap<ControlDomainId, CellIdentity> = BTreeMap::new();

        for voter in &voters {
            if !seen_cells.insert(voter.cell_identity) {
                return Err(ConsensusConfigError::DuplicateCellIdentity(
                    voter.cell_identity,
                ));
            }
            if voters.len() > 1
                && let Some(&first_cell) = seen_domains.get(&voter.control_domain_id)
            {
                return Err(ConsensusConfigError::NonIndependentControlDomain {
                    control_domain_id: voter.control_domain_id,
                    first_cell,
                    second_cell: voter.cell_identity,
                });
            }
            seen_domains.insert(voter.control_domain_id, voter.cell_identity);
        }

        if voters.len() == 2 {
            return Err(ConsensusConfigError::UnsupportedTwoNodeQuorum);
        }

        let mut sorted = voters;
        sorted.sort_by_key(|v| v.cell_identity);
        Ok(Self { members: sorted })
    }

    /// Number of voting replicas (`N`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Always `false` for a [`ValidatedVoterRoster`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Strict majority quorum size `floor(N / 2) + 1`.
    #[must_use]
    pub fn quorum_size(&self) -> usize {
        (self.members.len() / 2) + 1
    }

    /// Returns `true` if `cell` is a member of this voter roster.
    #[must_use]
    pub fn contains(&self, cell: CellIdentity) -> bool {
        self.members.iter().any(|m| m.cell_identity == cell)
    }

    /// Looks up the [`ControlDomainId`] bound to `cell`.
    #[must_use]
    pub fn domain_of(&self, cell: CellIdentity) -> Option<ControlDomainId> {
        self.members
            .iter()
            .find(|m| m.cell_identity == cell)
            .map(|m| m.control_domain_id)
    }

    /// Canonical sorted slice of [`VoterReplica`]s.
    #[must_use]
    pub fn members(&self) -> &[VoterReplica] {
        &self.members
    }
}

/// Monotonic tick timing parameters for [`ConsensusCore`] (zero wall-clock).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsensusTimingConfig {
    /// Leader heartbeat broadcast cadence (`>= 1` tick).
    pub heartbeat_interval_ticks: u32,
    /// Term-bound monotonic leader-lease & follower Pre-Vote stickiness horizon
    /// (`heartbeat_interval_ticks < leader_lease_ticks < election_timeout_ticks`).
    pub leader_lease_ticks: u32,
    /// Follower/PreCandidate/Candidate election timeout (`> leader_lease_ticks`).
    pub election_timeout_ticks: u32,
    /// Maximum byte size per `InstallSnapshotChunk` RPC (`>= 64`).
    pub max_snapshot_chunk_bytes: usize,
}

impl ConsensusTimingConfig {
    /// Validates monotonic tick ordering invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ConsensusConfigError::InvalidTimingBounds`] if
    /// `1 <= heartbeat < leader_lease < election_timeout` is violated.
    pub const fn validate(self) -> Result<Self, ConsensusConfigError> {
        if self.heartbeat_interval_ticks == 0
            || self.leader_lease_ticks <= self.heartbeat_interval_ticks
            || self.election_timeout_ticks <= self.leader_lease_ticks
        {
            return Err(ConsensusConfigError::InvalidTimingBounds {
                heartbeat_ticks: self.heartbeat_interval_ticks,
                leader_lease_ticks: self.leader_lease_ticks,
                election_timeout_ticks: self.election_timeout_ticks,
            });
        }
        Ok(self)
    }
}

impl Default for ConsensusTimingConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_ticks: 2,
            leader_lease_ticks: 5,
            election_timeout_ticks: 10,
            max_snapshot_chunk_bytes: DEFAULT_SNAPSHOT_CHUNK_BYTES,
        }
    }
}

/// Immutable Raft log entry with domain-separated `SHA-256` integrity digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RaftLogEntry {
    /// 1-based monotonic Raft log index.
    pub index: LogIndex,
    /// Leader election term in which this entry was proposed (`>= 1`).
    pub term: ConsensusTerm,
    /// Strictly scoped `[CONS-SCOPE-001]` proposal.
    pub proposal: ControlPlaneProposal,
    /// Canonical `SHA-256` integrity hash of `(index, term, proposal)`.
    pub entry_sha256: [u8; 32],
}

impl RaftLogEntry {
    /// Constructs a [`RaftLogEntry`] and computes its canonical `entry_sha256`.
    #[must_use]
    pub fn new(index: LogIndex, term: ConsensusTerm, proposal: ControlPlaneProposal) -> Self {
        let body = proposal.encode_command_body();
        let entry_sha256 = compute_log_entry_sha256(
            index,
            term,
            proposal.command.family_code().as_u8(),
            proposal.idempotency_key.as_bytes(),
            &body,
        );
        Self {
            index,
            term,
            proposal,
            entry_sha256,
        }
    }

    /// Recomputes and verifies `self.entry_sha256`.
    #[must_use]
    pub fn verify_integrity(&self) -> bool {
        let body = self.proposal.encode_command_body();
        let expected = compute_log_entry_sha256(
            self.index,
            self.term,
            self.proposal.command.family_code().as_u8(),
            self.proposal.idempotency_key.as_bytes(),
            &body,
        );
        self.entry_sha256 == expected
    }
}

/// Computes the canonical `SHA-256` digest stored in `SQLite` `consensus_log`.
#[must_use]
pub fn compute_log_entry_sha256(
    log_index: LogIndex,
    term: ConsensusTerm,
    command_kind: u8,
    idempotency_key: &[u8; 16],
    payload_bytes: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(LOG_ENTRY_HASH_DOMAIN);
    hasher.update(log_index.to_be_bytes());
    hasher.update(term.to_be_bytes());
    hasher.update([command_kind]);
    hasher.update(idempotency_key);
    hasher.update((payload_bytes.len() as u64).to_be_bytes());
    hasher.update(payload_bytes);
    hasher.finalize().into()
}

/// Computes the canonical `SHA-256` digest of a snapshot blob for
/// `InstallSnapshot` transfer verification.
#[must_use]
pub fn compute_snapshot_transfer_sha256(
    included_index: LogIndex,
    included_term: ConsensusTerm,
    state_root_digest: &[u8; 32],
    snapshot_bytes: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SNAPSHOT_WIRE_HASH_DOMAIN);
    hasher.update(included_index.to_be_bytes());
    hasher.update(included_term.to_be_bytes());
    hasher.update(state_root_digest);
    hasher.update((snapshot_bytes.len() as u64).to_be_bytes());
    hasher.update(snapshot_bytes);
    hasher.finalize().into()
}

/// Compacted snapshot boundary metadata (`[DUR-SNAP-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotMetadata {
    /// Highest log index included in the snapshot (`>= 1`).
    pub included_index: LogIndex,
    /// Term of `included_index` (`>= 1`).
    pub included_term: ConsensusTerm,
    /// State-machine root digest at `included_index`.
    pub state_root_digest: [u8; 32],
}

/// Raft replica role (`Follower`, `PreCandidate`, `Candidate`, `Leader`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsensusRole {
    /// Passive voter replicating entries from the current-term Leader.
    Follower,
    /// Pre-Vote solicitor probing quorum reachability & log freshness without
    /// incrementing `current_term`.
    PreCandidate,
    /// Active election candidate in `current_term`.
    Candidate,
    /// Elected majority Leader for `current_term`.
    Leader,
}

/// Monotonic, wall-clock-free Leader lease snapshot (`ADR-0026` §2.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TermBoundLeaderLease {
    /// Term in which this lease is bound.
    pub term: ConsensusTerm,
    /// Remaining monotonic ticks before lease expiry (`0` if unconfirmed or expired).
    pub remaining_ticks: u32,
    /// Last current-term heartbeat round acknowledged by a distinct-domain quorum.
    pub last_confirmed_round: u64,
}

impl TermBoundLeaderLease {
    /// Returns `true` when this lease has `remaining_ticks > 0` in `expected_term`.
    #[must_use]
    pub const fn is_active_in_term(&self, expected_term: ConsensusTerm) -> bool {
        self.term == expected_term && self.remaining_ticks > 0
    }
}

/// Wire-serializable Raft protocol messages exchanged over `nlos-net`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RaftMessage {
    /// Ongaro §9.6 Pre-Vote request (never increments `current_term`).
    RequestPreVote {
        /// Prospective next term (`sender.current_term + 1`).
        prospective_term: ConsensusTerm,
        /// Pre-candidate nominal identity.
        candidate_id: CellIdentity,
        /// Index of candidate's last log entry.
        last_log_index: LogIndex,
        /// Term of candidate's last log entry.
        last_log_term: ConsensusTerm,
    },
    /// Voter response to [`Self::RequestPreVote`].
    RequestPreVoteResponse {
        /// Responder's authoritative `current_term`.
        term: ConsensusTerm,
        /// Echoed `prospective_term` from the request.
        prospective_term: ConsensusTerm,
        /// Responder's nominal identity.
        voter_id: CellIdentity,
        /// `true` iff pre-vote was granted.
        vote_granted: bool,
    },
    /// Standard Raft §5.2 `RequestVote` RPC.
    RequestVote {
        /// Candidate's incremented `current_term`.
        term: ConsensusTerm,
        /// Candidate's nominal identity.
        candidate_id: CellIdentity,
        /// Index of candidate's last log entry.
        last_log_index: LogIndex,
        /// Term of candidate's last log entry.
        last_log_term: ConsensusTerm,
    },
    /// Voter response to [`Self::RequestVote`].
    RequestVoteResponse {
        /// Responder's authoritative `current_term`.
        term: ConsensusTerm,
        /// Responder's nominal identity.
        voter_id: CellIdentity,
        /// `true` iff single vote in `term` was durably granted.
        vote_granted: bool,
    },
    /// Leader log replication, heartbeat, and `ReadIndex` fence RPC.
    AppendEntries {
        /// Leader's `current_term`.
        term: ConsensusTerm,
        /// Leader's nominal identity.
        leader_id: CellIdentity,
        /// Index immediately preceding `entries`.
        prev_log_index: LogIndex,
        /// Term of `prev_log_index` (`0` when `prev_log_index == 0`).
        prev_log_term: ConsensusTerm,
        /// Entries to append (empty on heartbeats).
        entries: Vec<RaftLogEntry>,
        /// Leader's `consensus_commit_index`.
        leader_commit: LogIndex,
        /// Monotonic heartbeat / `ReadIndex` quorum round id.
        quorum_round_id: u64,
    },
    /// Follower response to [`Self::AppendEntries`] with fast conflict hints.
    AppendEntriesResponse {
        /// Follower's authoritative `current_term`.
        term: ConsensusTerm,
        /// Follower's nominal identity.
        follower_id: CellIdentity,
        /// `true` iff `(prev_log_index, prev_log_term)` matched.
        success: bool,
        /// Highest replicated index on follower when `success == true`.
        match_index: LogIndex,
        /// Conflicting entry term at `prev_log_index` when `success == false`, if present.
        conflict_term: Option<ConsensusTerm>,
        /// First index of `conflict_term` (or `last_log_index + 1`) for 1-RTT backup.
        conflict_index: LogIndex,
        /// Echoed `quorum_round_id`.
        quorum_round_id: u64,
    },
    /// Chunked `[DUR-SNAP-001]` snapshot transfer for lagging followers whose
    /// `next_index <= leader.snapshot.included_index`.
    InstallSnapshotChunk {
        /// Leader's `current_term`.
        term: ConsensusTerm,
        /// Leader's nominal identity.
        leader_id: CellIdentity,
        /// Highest log index covered by snapshot.
        last_included_index: LogIndex,
        /// Term of `last_included_index`.
        last_included_term: ConsensusTerm,
        /// State-machine root digest at `last_included_index`.
        state_root_digest: [u8; 32],
        /// Full-snapshot transfer `SHA-256`.
        snapshot_sha256: [u8; 32],
        /// Total byte length of the serialized snapshot.
        total_bytes: u64,
        /// Byte offset of `chunk_data` within the snapshot.
        offset: u64,
        /// Chunk payload (`<= max_snapshot_chunk_bytes`).
        chunk_data: Vec<u8>,
        /// `true` iff this chunk completes the snapshot.
        done: bool,
    },
    /// Follower acknowledgement for [`Self::InstallSnapshotChunk`].
    InstallSnapshotResponse {
        /// Follower's authoritative `current_term`.
        term: ConsensusTerm,
        /// Follower's nominal identity.
        follower_id: CellIdentity,
        /// Snapshot `last_included_index` being acknowledged.
        last_included_index: LogIndex,
        /// Next expected byte offset on the follower.
        next_offset: u64,
        /// `true` iff the complete snapshot was verified and installed.
        installed: bool,
    },
}

/// Fail-closed proposal / linearizable-read rejection reasons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProposalRejectReason {
    /// Caller attempted to write or read under a stale leader term after a
    /// higher term fenced this node (`ADR-0026` §3.3 WP3).
    StaleLeaderTerm {
        /// Stale leader term presented or previously held before step-down.
        attempted_term: ConsensusTerm,
        /// Current authoritative term on this replica.
        current_term: ConsensusTerm,
        /// Known leader in `current_term`, if any.
        known_leader: Option<CellIdentity>,
    },
    /// Node is not currently in `ConsensusRole::Leader`.
    NotLeader {
        /// Known leader in `current_term`, if any.
        known_leader: Option<CellIdentity>,
        /// Current authoritative term on this replica.
        current_term: ConsensusTerm,
    },
}

/// Deterministic input events driving [`ConsensusCore::step`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsensusInput {
    /// Advances local monotonic clock by 1 tick.
    Tick,
    /// Submits a scoped control-plane proposal, optionally fenced to
    /// `expected_leader_term`.
    Propose {
        /// Scoped `[CONS-SCOPE-001]` proposal.
        proposal: ControlPlaneProposal,
        /// Optional expected leader term (`Some` enforces `StaleLeaderTerm`).
        expected_leader_term: Option<ConsensusTerm>,
    },
    /// Requests a linearizable `ReadIndex` confirmation fenced by a live
    /// current-term majority heartbeat round.
    RequestLinearizableRead {
        /// Caller correlation identifier.
        read_id: u64,
        /// Optional expected leader term (`Some` enforces `StaleLeaderTerm`).
        expected_leader_term: Option<ConsensusTerm>,
    },
    /// Delivers an authenticated peer [`RaftMessage`].
    ReceiveMessage {
        /// Authenticated peer `CellIdentity` from `nlos-net` 5-axis session.
        from: CellIdentity,
        /// Inbound Raft RPC message.
        message: RaftMessage,
    },
}

/// Deterministic output effects emitted by [`ConsensusCore::step`].
///
/// Durable storage effects (`PersistHardState`, `InstallSnapshot`,
/// `TruncateLogSuffixFrom`, `AppendLogEntries`) MUST be committed to
/// `SQLite WAL + synchronous=FULL` BEFORE sending any `SendMessage` or
/// acknowledging `AdvanceCommitIndex` (`[DUR-ACK-002]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsensusEffect {
    /// Atomically persist `(current_term, voted_for)` before emitting messages.
    PersistHardState {
        /// Authoritative election term.
        current_term: ConsensusTerm,
        /// Durable single vote in `current_term`.
        voted_for: Option<CellIdentity>,
    },
    /// Atomically install a verified `[DUR-SNAP-001]` snapshot on a lagging
    /// follower and advance `commit_index` to at least `meta.included_index`.
    InstallSnapshot {
        /// Snapshot boundary metadata.
        meta: SnapshotMetadata,
        /// Verified serialized `ControlPlaneStateMachine` snapshot bytes.
        snapshot_bytes: Vec<u8>,
    },
    /// Delete conflicting uncommitted log entries at `>= from_index`.
    TruncateLogSuffixFrom {
        /// First 1-based log index to truncate (strictly `> commit_index`).
        from_index: LogIndex,
    },
    /// Persist newly appended [`RaftLogEntry`] records to `SQLite WAL + FULL`.
    AppendLogEntries(Vec<RaftLogEntry>),
    /// Advance durable `commit_index` and apply newly committed entries in order.
    AdvanceCommitIndex {
        /// New majority-committed `consensus_commit_index`.
        commit_index: LogIndex,
        /// Current term at commitment.
        term: ConsensusTerm,
        /// Ordered newly committed entries (`old_commit + 1 ..= commit_index`).
        newly_committed: Vec<RaftLogEntry>,
    },
    /// Abort all uncommitted proposals / pending reads from `aborted_term`
    /// because this Leader stepped down to `new_term` (`StaleLeaderTerm`).
    LeaderSteppedDown {
        /// Former leader term that was fenced.
        aborted_term: ConsensusTerm,
        /// Higher term that caused step-down.
        new_term: ConsensusTerm,
    },
    /// Confirms linearizable read safety at `commit_index` in `confirmed_term`.
    LinearizableReadConfirmed {
        /// Caller correlation id.
        read_id: u64,
        /// Confirmed active leader term.
        confirmed_term: ConsensusTerm,
        /// Linearizable `consensus_commit_index`.
        commit_index: LogIndex,
    },
    /// Rejects a proposal or read fail-closed (`StaleLeaderTerm` or `NotLeader`).
    ProposalRejected {
        /// Optional rejected read id (`None` for write proposals).
        read_id: Option<u64>,
        /// Typed rejection reason.
        reason: ProposalRejectReason,
    },
    /// Transmit a [`RaftMessage`] to nominal `target` over `nlos-net`.
    SendMessage {
        /// Destination voter `CellIdentity`.
        target: CellIdentity,
        /// Outbound Raft protocol message.
        message: RaftMessage,
    },
}

#[derive(Clone, Debug)]
struct PendingQuorumRound {
    term: ConsensusTerm,
    commit_index_at_issue: LogIndex,
    acked_domains: BTreeSet<ControlDomainId>,
    waiting_read_ids: Vec<u64>,
}

#[derive(Clone, Debug)]
struct InboundSnapshotStaging {
    term: ConsensusTerm,
    leader_id: CellIdentity,
    last_included_index: LogIndex,
    last_included_term: ConsensusTerm,
    state_root_digest: [u8; 32],
    snapshot_sha256: [u8; 32],
    total_bytes: u64,
    buffer: Vec<u8>,
}

/// Recovery state loaded from `SqliteConsensusStore` to rehydrate
/// [`ConsensusCore`] across crashes and restarts.
#[derive(Clone, Debug, Default)]
pub struct RehydratedCoreState {
    /// Persisted `current_term`.
    pub current_term: ConsensusTerm,
    /// Persisted `voted_for` in `current_term`.
    pub voted_for: Option<CellIdentity>,
    /// Persisted `commit_index`.
    pub commit_index: LogIndex,
    /// Latest persisted `[DUR-SNAP-001]` snapshot metadata and bytes, if any.
    pub snapshot: Option<(SnapshotMetadata, Vec<u8>)>,
    /// Uncompacted contiguous log suffix (`snapshot_index + 1 ..= last_log_index`).
    pub log_tail: Vec<RaftLogEntry>,
}

/// Pure, I/O-free deterministic Raft state machine (`unsafe_code = "forbid"`).
#[derive(Clone, Debug)]
pub struct ConsensusCore {
    local_id: CellIdentity,
    local_domain: ControlDomainId,
    roster: ValidatedVoterRoster,
    timing: ConsensusTimingConfig,
    role: ConsensusRole,
    current_term: ConsensusTerm,
    voted_for: Option<CellIdentity>,
    known_leader: Option<CellIdentity>,
    former_leader_term: Option<ConsensusTerm>,
    snapshot_meta: Option<SnapshotMetadata>,
    snapshot_bytes: Option<Vec<u8>>,
    log_tail: Vec<RaftLogEntry>,
    commit_index: LogIndex,
    ticks_since_leader_contact: u32,
    ticks_since_heartbeat_sent: u32,
    pre_vote_prospective_term: ConsensusTerm,
    pre_votes_granted_domains: BTreeSet<ControlDomainId>,
    votes_granted_domains: BTreeSet<ControlDomainId>,
    next_index: BTreeMap<CellIdentity, LogIndex>,
    match_index: BTreeMap<CellIdentity, LogIndex>,
    snapshot_send_offset: BTreeMap<CellIdentity, u64>,
    inbound_snapshot: Option<InboundSnapshotStaging>,
    next_quorum_round_id: u64,
    quorum_rounds: BTreeMap<u64, PendingQuorumRound>,
    leader_lease: TermBoundLeaderLease,
}

impl ConsensusCore {
    /// Constructs a fresh [`ConsensusCore`] at genesis.
    ///
    /// - For `N = 1`, initializes directly as `Leader` in `term = 1` (`ADR-0026` §6.3).
    /// - For `N >= 3`, initializes as `Follower` in `term = 0`.
    ///
    /// # Errors
    ///
    /// Fails closed if `local_id` is absent from `roster` or `timing` violates
    /// monotonic tick ordering.
    pub fn new(
        local_id: CellIdentity,
        roster: ValidatedVoterRoster,
        timing: ConsensusTimingConfig,
    ) -> Result<Self, ConsensusConfigError> {
        let timing = timing.validate()?;
        let Some(local_domain) = roster.domain_of(local_id) else {
            return Err(ConsensusConfigError::LocalCellNotInRoster(local_id));
        };

        if roster.len() == 1 {
            let mut next_index = BTreeMap::new();
            let mut match_index = BTreeMap::new();
            next_index.insert(local_id, 1);
            match_index.insert(local_id, 0);
            let mut votes = BTreeSet::new();
            votes.insert(local_domain);
            return Ok(Self {
                local_id,
                local_domain,
                roster,
                timing,
                role: ConsensusRole::Leader,
                current_term: 1,
                voted_for: Some(local_id),
                known_leader: Some(local_id),
                former_leader_term: None,
                snapshot_meta: None,
                snapshot_bytes: None,
                log_tail: Vec::new(),
                commit_index: 0,
                ticks_since_leader_contact: 0,
                ticks_since_heartbeat_sent: 0,
                pre_vote_prospective_term: 0,
                pre_votes_granted_domains: BTreeSet::new(),
                votes_granted_domains: votes,
                next_index,
                match_index,
                snapshot_send_offset: BTreeMap::new(),
                inbound_snapshot: None,
                next_quorum_round_id: 1,
                quorum_rounds: BTreeMap::new(),
                leader_lease: TermBoundLeaderLease {
                    term: 1,
                    remaining_ticks: timing.leader_lease_ticks,
                    last_confirmed_round: 0,
                },
            });
        }

        Ok(Self {
            local_id,
            local_domain,
            roster,
            timing,
            role: ConsensusRole::Follower,
            current_term: 0,
            voted_for: None,
            known_leader: None,
            former_leader_term: None,
            snapshot_meta: None,
            snapshot_bytes: None,
            log_tail: Vec::new(),
            commit_index: 0,
            // Initialize >= leader_lease_ticks so cold-boot initial election is
            // not artificially delayed by leader-lease stickiness when
            // `known_leader` is still `None`.
            ticks_since_leader_contact: 0,
            ticks_since_heartbeat_sent: 0,
            pre_vote_prospective_term: 0,
            pre_votes_granted_domains: BTreeSet::new(),
            votes_granted_domains: BTreeSet::new(),
            next_index: BTreeMap::new(),
            match_index: BTreeMap::new(),
            snapshot_send_offset: BTreeMap::new(),
            inbound_snapshot: None,
            next_quorum_round_id: 1,
            quorum_rounds: BTreeMap::new(),
            leader_lease: TermBoundLeaderLease {
                term: 0,
                remaining_ticks: 0,
                last_confirmed_round: 0,
            },
        })
    }

    /// Rehydrates [`ConsensusCore`] from verified `SqliteConsensusStore` state.
    ///
    /// # Errors
    ///
    /// Fails closed if `roster` or `timing` is invalid.
    pub fn rehydrate(
        local_id: CellIdentity,
        roster: ValidatedVoterRoster,
        timing: ConsensusTimingConfig,
        state: RehydratedCoreState,
    ) -> Result<Self, ConsensusConfigError> {
        let mut core = Self::new(local_id, roster, timing)?;
        let (snap_meta, snap_bytes) = match state.snapshot {
            Some((meta, bytes)) => (Some(meta), Some(bytes)),
            None => (None, None),
        };
        let min_commit = snap_meta.map_or(0, |m| m.included_index);
        core.snapshot_meta = snap_meta;
        core.snapshot_bytes = snap_bytes;
        core.log_tail = state.log_tail;
        core.commit_index = state.commit_index.max(min_commit);

        if core.roster.len() == 1 {
            core.current_term = state.current_term.max(1);
            core.voted_for = Some(local_id);
            core.role = ConsensusRole::Leader;
            core.known_leader = Some(local_id);
            let last = core.last_log_index();
            core.commit_index = last;
            core.match_index.insert(local_id, last);
            core.next_index.insert(local_id, last.saturating_add(1));
        } else {
            core.current_term = state.current_term;
            core.voted_for = state.voted_for;
            core.role = ConsensusRole::Follower;
            core.known_leader = None;
        }
        Ok(core)
    }

    /// Local nominal `CellIdentity`.
    #[must_use]
    pub const fn local_id(&self) -> CellIdentity {
        self.local_id
    }

    /// Validated voter roster.
    #[must_use]
    pub const fn roster(&self) -> &ValidatedVoterRoster {
        &self.roster
    }

    /// Current replica role (`Follower`, `PreCandidate`, `Candidate`, `Leader`).
    #[must_use]
    pub const fn role(&self) -> ConsensusRole {
        self.role
    }

    /// Authoritative `current_term`.
    #[must_use]
    pub const fn current_term(&self) -> ConsensusTerm {
        self.current_term
    }

    /// Durable single vote granted in `current_term`, if any.
    #[must_use]
    pub const fn voted_for(&self) -> Option<CellIdentity> {
        self.voted_for
    }

    /// Known leader in `current_term`, if any.
    #[must_use]
    pub const fn known_leader(&self) -> Option<CellIdentity> {
        self.known_leader
    }

    /// Current majority-committed `consensus_commit_index`.
    #[must_use]
    pub const fn consensus_commit_index(&self) -> LogIndex {
        self.commit_index
    }

    /// Snapshot boundary metadata (`None` if no snapshot installed/created yet).
    #[must_use]
    pub const fn snapshot_metadata(&self) -> Option<SnapshotMetadata> {
        self.snapshot_meta
    }

    /// Monotonic term-bound leader lease state.
    #[must_use]
    pub const fn leader_lease(&self) -> TermBoundLeaderLease {
        self.leader_lease
    }

    /// First uncompacted 1-based log index stored in `log_tail`
    /// (`snapshot_included_index + 1`, or `1` before any snapshot).
    #[must_use]
    pub fn first_uncompacted_index(&self) -> LogIndex {
        self.snapshot_meta
            .map_or(1, |m| m.included_index.saturating_add(1))
    }

    /// Highest log index known on this replica (`snapshot` + `log_tail`).
    #[must_use]
    pub fn last_log_index(&self) -> LogIndex {
        self.log_tail.last().map_or_else(
            || self.snapshot_meta.map_or(0, |m| m.included_index),
            |e| e.index,
        )
    }

    /// Term of [`Self::last_log_index`] (`0` when log is empty).
    #[must_use]
    pub fn last_log_term(&self) -> ConsensusTerm {
        self.log_tail.last().map_or_else(
            || self.snapshot_meta.map_or(0, |m| m.included_term),
            |e| e.term,
        )
    }

    /// Uncompacted log tail slice (`first_uncompacted_index ..= last_log_index`).
    #[must_use]
    pub fn uncompacted_log(&self) -> &[RaftLogEntry] {
        &self.log_tail
    }

    /// Looks up the term at `index` if `index == 0`, `index == snapshot.included_index`,
    /// or `index` resides in `log_tail`.
    #[must_use]
    pub fn term_at(&self, index: LogIndex) -> Option<ConsensusTerm> {
        if index == 0 {
            return Some(0);
        }
        if let Some(meta) = self.snapshot_meta {
            if index == meta.included_index {
                return Some(meta.included_term);
            }
            if index < meta.included_index {
                return None;
            }
        }
        let base = self.first_uncompacted_index();
        let offset = usize::try_from(index.checked_sub(base)?).ok()?;
        self.log_tail.get(offset).map(|e| e.term)
    }

    /// Records a locally created `[DUR-SNAP-001]` snapshot and compacts the
    /// in-memory log prefix up to `compact_up_to` (`<= meta.included_index <= commit_index`).
    ///
    /// Returns `true` if installed, or `false` if `meta.included_index` was
    /// stale or exceeded `commit_index`.
    pub fn record_local_snapshot(
        &mut self,
        meta: SnapshotMetadata,
        snapshot_bytes: Vec<u8>,
        compact_up_to: LogIndex,
    ) -> bool {
        if meta.included_index == 0 || meta.included_index > self.commit_index {
            return false;
        }
        if let Some(existing) = self.snapshot_meta
            && meta.included_index < existing.included_index
        {
            return false;
        }
        let target_compact = compact_up_to.min(meta.included_index);
        self.log_tail.retain(|e| e.index > target_compact);
        if target_compact == meta.included_index {
            self.snapshot_meta = Some(meta);
            self.snapshot_bytes = Some(snapshot_bytes);
            self.snapshot_send_offset.clear();
        }
        true
    }

    /// Pure deterministic transition (`(self, input) -> Vec<ConsensusEffect>`).
    pub fn step(&mut self, input: ConsensusInput) -> Vec<ConsensusEffect> {
        match input {
            ConsensusInput::Tick => self.on_tick(),
            ConsensusInput::Propose {
                proposal,
                expected_leader_term,
            } => self.on_propose(proposal, expected_leader_term),
            ConsensusInput::RequestLinearizableRead {
                read_id,
                expected_leader_term,
            } => self.on_request_linearizable_read(read_id, expected_leader_term),
            ConsensusInput::ReceiveMessage { from, message } => self.on_message(from, message),
        }
    }

    fn on_tick(&mut self) -> Vec<ConsensusEffect> {
        if self.roster.len() == 1 {
            return Vec::new();
        }

        if self.leader_lease.remaining_ticks > 0 {
            self.leader_lease.remaining_ticks -= 1;
        }

        match self.role {
            ConsensusRole::Leader => {
                self.ticks_since_heartbeat_sent = self.ticks_since_heartbeat_sent.saturating_add(1);
                if self.ticks_since_heartbeat_sent >= self.timing.heartbeat_interval_ticks {
                    self.ticks_since_heartbeat_sent = 0;
                    let round_id = self.allocate_quorum_round(None);
                    self.broadcast_replication_or_heartbeat(round_id)
                } else {
                    Vec::new()
                }
            }
            ConsensusRole::Follower | ConsensusRole::PreCandidate | ConsensusRole::Candidate => {
                self.ticks_since_leader_contact = self.ticks_since_leader_contact.saturating_add(1);
                if self.ticks_since_leader_contact >= self.timing.election_timeout_ticks {
                    self.start_pre_vote()
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Ongaro §9.6 Pre-Vote phase: probes quorum without advancing `current_term`!
    fn start_pre_vote(&mut self) -> Vec<ConsensusEffect> {
        let prospective_term = self.current_term.saturating_add(1);
        self.role = ConsensusRole::PreCandidate;
        self.known_leader = None;
        self.ticks_since_leader_contact = 0;
        self.pre_vote_prospective_term = prospective_term;
        self.pre_votes_granted_domains.clear();
        self.pre_votes_granted_domains.insert(self.local_domain);

        let last_log_index = self.last_log_index();
        let last_log_term = self.last_log_term();
        let mut effects = Vec::new();
        for voter in self.roster.members() {
            if voter.cell_identity == self.local_id {
                continue;
            }
            effects.push(ConsensusEffect::SendMessage {
                target: voter.cell_identity,
                message: RaftMessage::RequestPreVote {
                    prospective_term,
                    candidate_id: self.local_id,
                    last_log_index,
                    last_log_term,
                },
            });
        }
        effects
    }

    /// Promotes from `PreCandidate` to `Candidate` ONLY after a distinct-domain
    /// quorum granted Pre-Vote.
    fn start_real_election(&mut self) -> Vec<ConsensusEffect> {
        self.current_term = self.current_term.saturating_add(1);
        self.role = ConsensusRole::Candidate;
        self.voted_for = Some(self.local_id);
        self.known_leader = None;
        self.ticks_since_leader_contact = 0;
        self.pre_votes_granted_domains.clear();
        self.votes_granted_domains.clear();
        self.votes_granted_domains.insert(self.local_domain);
        self.quorum_rounds.clear();
        self.leader_lease = TermBoundLeaderLease {
            term: self.current_term,
            remaining_ticks: 0,
            last_confirmed_round: 0,
        };

        let mut effects = vec![ConsensusEffect::PersistHardState {
            current_term: self.current_term,
            voted_for: self.voted_for,
        }];

        let last_log_index = self.last_log_index();
        let last_log_term = self.last_log_term();
        for voter in self.roster.members() {
            if voter.cell_identity == self.local_id {
                continue;
            }
            effects.push(ConsensusEffect::SendMessage {
                target: voter.cell_identity,
                message: RaftMessage::RequestVote {
                    term: self.current_term,
                    candidate_id: self.local_id,
                    last_log_index,
                    last_log_term,
                },
            });
        }
        effects
    }

    fn check_leader_gate(
        &self,
        expected_leader_term: Option<ConsensusTerm>,
    ) -> Result<(), ProposalRejectReason> {
        if let Some(expected_term) = expected_leader_term {
            if expected_term < self.current_term {
                return Err(ProposalRejectReason::StaleLeaderTerm {
                    attempted_term: expected_term,
                    current_term: self.current_term,
                    known_leader: self.known_leader,
                });
            }
            if self.role != ConsensusRole::Leader || expected_term != self.current_term {
                return Err(ProposalRejectReason::NotLeader {
                    known_leader: self.known_leader,
                    current_term: self.current_term,
                });
            }
            return Ok(());
        }

        if self.role != ConsensusRole::Leader {
            if let Some(old_term) = self.former_leader_term
                && old_term < self.current_term
                && self.known_leader != Some(self.local_id)
            {
                return Err(ProposalRejectReason::StaleLeaderTerm {
                    attempted_term: old_term,
                    current_term: self.current_term,
                    known_leader: self.known_leader,
                });
            }
            return Err(ProposalRejectReason::NotLeader {
                known_leader: self.known_leader,
                current_term: self.current_term,
            });
        }
        Ok(())
    }

    fn on_propose(
        &mut self,
        proposal: ControlPlaneProposal,
        expected_leader_term: Option<ConsensusTerm>,
    ) -> Vec<ConsensusEffect> {
        if let Err(reason) = self.check_leader_gate(expected_leader_term) {
            return vec![ConsensusEffect::ProposalRejected {
                read_id: None,
                reason,
            }];
        }

        let new_index = self.last_log_index().saturating_add(1);
        let entry = RaftLogEntry::new(new_index, self.current_term, proposal);
        self.log_tail.push(entry);
        self.match_index.insert(self.local_id, new_index);
        self.next_index
            .insert(self.local_id, new_index.saturating_add(1));

        let mut effects = vec![ConsensusEffect::AppendLogEntries(vec![entry])];

        if self.roster.len() == 1 {
            self.commit_index = new_index;
            effects.push(ConsensusEffect::AdvanceCommitIndex {
                commit_index: new_index,
                term: self.current_term,
                newly_committed: vec![entry],
            });
            return effects;
        }

        self.ticks_since_heartbeat_sent = 0;
        let round_id = self.allocate_quorum_round(None);
        effects.extend(self.broadcast_replication_or_heartbeat(round_id));
        effects
    }

    fn on_request_linearizable_read(
        &mut self,
        read_id: u64,
        expected_leader_term: Option<ConsensusTerm>,
    ) -> Vec<ConsensusEffect> {
        if let Err(reason) = self.check_leader_gate(expected_leader_term) {
            return vec![ConsensusEffect::ProposalRejected {
                read_id: Some(read_id),
                reason,
            }];
        }

        if self.roster.len() == 1 {
            return vec![ConsensusEffect::LinearizableReadConfirmed {
                read_id,
                confirmed_term: self.current_term,
                commit_index: self.commit_index,
            }];
        }

        let round_id = self.allocate_quorum_round(Some(read_id));
        self.ticks_since_heartbeat_sent = 0;
        self.broadcast_replication_or_heartbeat(round_id)
    }

    fn allocate_quorum_round(&mut self, read_id: Option<u64>) -> u64 {
        let round_id = self.next_quorum_round_id;
        self.next_quorum_round_id = self.next_quorum_round_id.saturating_add(1);
        let mut acked_domains = BTreeSet::new();
        acked_domains.insert(self.local_domain);
        let mut waiting_read_ids = Vec::new();
        if let Some(rid) = read_id {
            waiting_read_ids.push(rid);
        }
        // Keep `quorum_rounds` bounded (`<= 256`) by pruning oldest read-free rounds.
        if self.quorum_rounds.len() >= 256
            && let Some((&oldest_key, _)) = self
                .quorum_rounds
                .iter()
                .find(|(_, r)| r.waiting_read_ids.is_empty())
        {
            self.quorum_rounds.remove(&oldest_key);
        }
        self.quorum_rounds.insert(
            round_id,
            PendingQuorumRound {
                term: self.current_term,
                commit_index_at_issue: self.commit_index,
                acked_domains,
                waiting_read_ids,
            },
        );
        round_id
    }

    #[allow(clippy::too_many_lines)]
    fn on_message(&mut self, from: CellIdentity, message: RaftMessage) -> Vec<ConsensusEffect> {
        if !self.roster.contains(from) || from == self.local_id {
            return Vec::new();
        }
        match message {
            RaftMessage::RequestPreVote {
                prospective_term,
                candidate_id,
                last_log_index,
                last_log_term,
            } => {
                if candidate_id != from {
                    return Vec::new();
                }
                self.on_request_pre_vote(
                    prospective_term,
                    candidate_id,
                    last_log_index,
                    last_log_term,
                )
            }
            RaftMessage::RequestPreVoteResponse {
                term,
                prospective_term,
                voter_id,
                vote_granted,
            } => {
                if voter_id != from {
                    return Vec::new();
                }
                self.on_request_pre_vote_response(term, prospective_term, voter_id, vote_granted)
            }
            RaftMessage::RequestVote {
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
            RaftMessage::RequestVoteResponse {
                term,
                voter_id,
                vote_granted,
            } => {
                if voter_id != from {
                    return Vec::new();
                }
                self.on_request_vote_response(term, voter_id, vote_granted)
            }
            RaftMessage::AppendEntries {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
                quorum_round_id,
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
                    quorum_round_id,
                )
            }
            RaftMessage::AppendEntriesResponse {
                term,
                follower_id,
                success,
                match_index,
                conflict_term,
                conflict_index,
                quorum_round_id,
            } => {
                if follower_id != from {
                    return Vec::new();
                }
                self.on_append_entries_response(
                    term,
                    follower_id,
                    success,
                    match_index,
                    conflict_term,
                    conflict_index,
                    quorum_round_id,
                )
            }
            RaftMessage::InstallSnapshotChunk {
                term,
                leader_id,
                last_included_index,
                last_included_term,
                state_root_digest,
                snapshot_sha256,
                total_bytes,
                offset,
                chunk_data,
                done,
            } => {
                if leader_id != from {
                    return Vec::new();
                }
                self.on_install_snapshot_chunk(
                    term,
                    leader_id,
                    last_included_index,
                    last_included_term,
                    state_root_digest,
                    snapshot_sha256,
                    total_bytes,
                    offset,
                    &chunk_data,
                    done,
                )
            }
            RaftMessage::InstallSnapshotResponse {
                term,
                follower_id,
                last_included_index,
                next_offset,
                installed,
            } => {
                if follower_id != from {
                    return Vec::new();
                }
                self.on_install_snapshot_response(
                    term,
                    follower_id,
                    last_included_index,
                    next_offset,
                    installed,
                )
            }
        }
    }

    fn step_down_to_higher_term(&mut self, higher_term: ConsensusTerm) -> Vec<ConsensusEffect> {
        let was_leader = self.role == ConsensusRole::Leader;
        let old_term = self.current_term;
        if was_leader {
            self.former_leader_term = Some(old_term);
        }

        // Collect pending read IDs from aborted rounds so waiting callers fail
        // closed immediately with `StaleLeaderTerm` instead of hanging!
        let mut aborted_reads = Vec::new();
        for round in std::mem::take(&mut self.quorum_rounds).into_values() {
            aborted_reads.extend(round.waiting_read_ids);
        }

        self.current_term = higher_term;
        self.role = ConsensusRole::Follower;
        self.voted_for = None;
        self.known_leader = None;
        self.pre_votes_granted_domains.clear();
        self.votes_granted_domains.clear();
        self.snapshot_send_offset.clear();
        self.leader_lease = TermBoundLeaderLease {
            term: higher_term,
            remaining_ticks: 0,
            last_confirmed_round: 0,
        };

        let mut effects = vec![ConsensusEffect::PersistHardState {
            current_term: higher_term,
            voted_for: None,
        }];
        if was_leader {
            effects.push(ConsensusEffect::LeaderSteppedDown {
                aborted_term: old_term,
                new_term: higher_term,
            });
        }
        for rid in aborted_reads {
            effects.push(ConsensusEffect::ProposalRejected {
                read_id: Some(rid),
                reason: ProposalRejectReason::StaleLeaderTerm {
                    attempted_term: old_term,
                    current_term: higher_term,
                    known_leader: None,
                },
            });
        }
        effects
    }

    fn on_request_pre_vote(
        &self,
        prospective_term: ConsensusTerm,
        candidate_id: CellIdentity,
        cand_last_index: LogIndex,
        cand_last_term: ConsensusTerm,
    ) -> Vec<ConsensusEffect> {
        let heard_live_leader_recently = self.role == ConsensusRole::Leader
            || (self.known_leader.is_some()
                && self.ticks_since_leader_contact < self.timing.leader_lease_ticks);

        let log_up_to_date =
            (cand_last_term, cand_last_index) >= (self.last_log_term(), self.last_log_index());

        let granted =
            prospective_term > self.current_term && !heard_live_leader_recently && log_up_to_date;

        vec![ConsensusEffect::SendMessage {
            target: candidate_id,
            message: RaftMessage::RequestPreVoteResponse {
                term: self.current_term,
                prospective_term,
                voter_id: self.local_id,
                vote_granted: granted,
            },
        }]
    }

    fn on_request_pre_vote_response(
        &mut self,
        responder_term: ConsensusTerm,
        prospective_term: ConsensusTerm,
        voter_id: CellIdentity,
        vote_granted: bool,
    ) -> Vec<ConsensusEffect> {
        if responder_term > self.current_term {
            return self.step_down_to_higher_term(responder_term);
        }
        if self.role != ConsensusRole::PreCandidate
            || prospective_term != self.pre_vote_prospective_term
            || prospective_term != self.current_term.saturating_add(1)
        {
            return Vec::new();
        }
        if vote_granted && let Some(domain) = self.roster.domain_of(voter_id) {
            self.pre_votes_granted_domains.insert(domain);
        }
        if self.pre_votes_granted_domains.len() >= self.roster.quorum_size() {
            self.start_real_election()
        } else {
            Vec::new()
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
            effects.extend(self.step_down_to_higher_term(term));
        }

        if term < self.current_term {
            effects.push(ConsensusEffect::SendMessage {
                target: candidate_id,
                message: RaftMessage::RequestVoteResponse {
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
            self.ticks_since_leader_contact = 0;
            effects.push(ConsensusEffect::PersistHardState {
                current_term: self.current_term,
                voted_for: self.voted_for,
            });
        }

        effects.push(ConsensusEffect::SendMessage {
            target: candidate_id,
            message: RaftMessage::RequestVoteResponse {
                term: self.current_term,
                voter_id: self.local_id,
                vote_granted: granted,
            },
        });
        effects
    }

    fn on_request_vote_response(
        &mut self,
        term: ConsensusTerm,
        voter_id: CellIdentity,
        vote_granted: bool,
    ) -> Vec<ConsensusEffect> {
        if term > self.current_term {
            return self.step_down_to_higher_term(term);
        }
        if self.role != ConsensusRole::Candidate || term != self.current_term {
            return Vec::new();
        }
        if vote_granted && let Some(domain) = self.roster.domain_of(voter_id) {
            self.votes_granted_domains.insert(domain);
        }
        if self.votes_granted_domains.len() >= self.roster.quorum_size() {
            self.become_leader()
        } else {
            Vec::new()
        }
    }

    fn become_leader(&mut self) -> Vec<ConsensusEffect> {
        self.role = ConsensusRole::Leader;
        self.known_leader = Some(self.local_id);
        self.former_leader_term = None;
        self.ticks_since_heartbeat_sent = 0;
        self.next_index.clear();
        self.match_index.clear();
        self.snapshot_send_offset.clear();
        let last_idx = self.last_log_index();
        let next_idx = last_idx.saturating_add(1);
        for voter in self.roster.members() {
            self.next_index.insert(voter.cell_identity, next_idx);
            if voter.cell_identity == self.local_id {
                self.match_index.insert(voter.cell_identity, last_idx);
            } else {
                self.match_index.insert(voter.cell_identity, 0);
            }
        }
        let round_id = self.allocate_quorum_round(None);
        self.broadcast_replication_or_heartbeat(round_id)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn on_append_entries(
        &mut self,
        term: ConsensusTerm,
        leader_id: CellIdentity,
        prev_log_index: LogIndex,
        prev_log_term: ConsensusTerm,
        entries: Vec<RaftLogEntry>,
        leader_commit: LogIndex,
        quorum_round_id: u64,
    ) -> Vec<ConsensusEffect> {
        let mut effects = Vec::new();
        if term > self.current_term {
            effects.extend(self.step_down_to_higher_term(term));
        }
        if term < self.current_term {
            effects.push(ConsensusEffect::SendMessage {
                target: leader_id,
                message: RaftMessage::AppendEntriesResponse {
                    term: self.current_term,
                    follower_id: self.local_id,
                    success: false,
                    match_index: 0,
                    conflict_term: None,
                    conflict_index: self.last_log_index().saturating_add(1),
                    quorum_round_id,
                },
            });
            return effects;
        }

        // Authoritative current-term Leader message resets election timer and
        // converts PreCandidate/Candidate in `term` to Follower.
        self.role = ConsensusRole::Follower;
        self.known_leader = Some(leader_id);
        self.ticks_since_leader_contact = 0;

        let snapshot_idx = self.snapshot_meta.map_or(0, |m| m.included_index);
        if prev_log_index < snapshot_idx {
            // Follower's compacted snapshot is already ahead of `prev_log_index`.
            // Ask Leader to advance `next_index` to `snapshot_idx + 1` (unless
            // `entries` extends beyond `snapshot_idx`).
            let usable: Vec<RaftLogEntry> = entries
                .into_iter()
                .filter(|e| e.index > snapshot_idx)
                .collect();
            if usable.is_empty() {
                effects.push(ConsensusEffect::SendMessage {
                    target: leader_id,
                    message: RaftMessage::AppendEntriesResponse {
                        term: self.current_term,
                        follower_id: self.local_id,
                        success: true,
                        match_index: self.last_log_index(),
                        conflict_term: None,
                        conflict_index: snapshot_idx.saturating_add(1),
                        quorum_round_id,
                    },
                });
                return effects;
            }
            if usable[0].index != snapshot_idx.saturating_add(1) {
                effects.push(ConsensusEffect::SendMessage {
                    target: leader_id,
                    message: RaftMessage::AppendEntriesResponse {
                        term: self.current_term,
                        follower_id: self.local_id,
                        success: false,
                        match_index: 0,
                        conflict_term: None,
                        conflict_index: snapshot_idx.saturating_add(1),
                        quorum_round_id,
                    },
                });
                return effects;
            }
            return self.reconcile_and_commit_entries(
                leader_id,
                usable,
                leader_commit,
                quorum_round_id,
                effects,
            );
        }

        match self.term_at(prev_log_index) {
            None => {
                effects.push(ConsensusEffect::SendMessage {
                    target: leader_id,
                    message: RaftMessage::AppendEntriesResponse {
                        term: self.current_term,
                        follower_id: self.local_id,
                        success: false,
                        match_index: 0,
                        conflict_term: None,
                        conflict_index: self.last_log_index().saturating_add(1),
                        quorum_round_id,
                    },
                });
                return effects;
            }
            Some(local_prev_term) if local_prev_term != prev_log_term => {
                // Fast conflict-term / conflict-index backup (never backing up
                // below `commit_index + 1` or `first_uncompacted_index`).
                let floor_idx = self
                    .commit_index
                    .saturating_add(1)
                    .max(self.first_uncompacted_index());
                let first_of_conflict_term = self
                    .log_tail
                    .iter()
                    .find(|e| e.index >= floor_idx && e.term == local_prev_term)
                    .map_or(prev_log_index.max(floor_idx), |e| e.index);

                effects.push(ConsensusEffect::SendMessage {
                    target: leader_id,
                    message: RaftMessage::AppendEntriesResponse {
                        term: self.current_term,
                        follower_id: self.local_id,
                        success: false,
                        match_index: 0,
                        conflict_term: Some(local_prev_term),
                        conflict_index: first_of_conflict_term,
                        quorum_round_id,
                    },
                });
                return effects;
            }
            Some(_) => {}
        }

        self.reconcile_and_commit_entries(
            leader_id,
            entries,
            leader_commit,
            quorum_round_id,
            effects,
        )
    }

    fn reconcile_and_commit_entries(
        &mut self,
        leader_id: CellIdentity,
        entries: Vec<RaftLogEntry>,
        leader_commit: LogIndex,
        quorum_round_id: u64,
        mut effects: Vec<ConsensusEffect>,
    ) -> Vec<ConsensusEffect> {
        let mut newly_appended = Vec::new();
        for entry in entries {
            if !entry.verify_integrity() {
                // Reject corrupted wire entry fail-closed without mutating log.
                effects.push(ConsensusEffect::SendMessage {
                    target: leader_id,
                    message: RaftMessage::AppendEntriesResponse {
                        term: self.current_term,
                        follower_id: self.local_id,
                        success: false,
                        match_index: 0,
                        conflict_term: None,
                        conflict_index: self.commit_index.saturating_add(1),
                        quorum_round_id,
                    },
                });
                return effects;
            }

            let base = self.first_uncompacted_index();
            if entry.index < base {
                continue;
            }
            let Ok(pos) = usize::try_from(entry.index - base) else {
                continue;
            };
            if let Some(existing) = self.log_tail.get(pos) {
                if existing.term != entry.term && entry.index > self.commit_index {
                    self.log_tail.truncate(pos);
                    effects.push(ConsensusEffect::TruncateLogSuffixFrom {
                        from_index: entry.index,
                    });
                    self.log_tail.push(entry);
                    newly_appended.push(entry);
                }
            } else if pos == self.log_tail.len() {
                self.log_tail.push(entry);
                newly_appended.push(entry);
            }
        }

        if !newly_appended.is_empty() {
            effects.push(ConsensusEffect::AppendLogEntries(newly_appended));
        }

        if leader_commit > self.commit_index {
            let new_commit = leader_commit.min(self.last_log_index());
            if new_commit > self.commit_index {
                let old_commit = self.commit_index;
                self.commit_index = new_commit;
                let newly_committed = self.collect_range(old_commit + 1, new_commit);
                effects.push(ConsensusEffect::AdvanceCommitIndex {
                    commit_index: new_commit,
                    term: self.current_term,
                    newly_committed,
                });
            }
        }

        effects.push(ConsensusEffect::SendMessage {
            target: leader_id,
            message: RaftMessage::AppendEntriesResponse {
                term: self.current_term,
                follower_id: self.local_id,
                success: true,
                match_index: self.last_log_index(),
                conflict_term: None,
                conflict_index: 0,
                quorum_round_id,
            },
        });
        effects
    }

    #[allow(clippy::too_many_arguments)]
    fn on_append_entries_response(
        &mut self,
        term: ConsensusTerm,
        follower_id: CellIdentity,
        success: bool,
        match_index: LogIndex,
        conflict_term: Option<ConsensusTerm>,
        conflict_index: LogIndex,
        quorum_round_id: u64,
    ) -> Vec<ConsensusEffect> {
        if term > self.current_term {
            return self.step_down_to_higher_term(term);
        }
        if self.role != ConsensusRole::Leader || term != self.current_term {
            return Vec::new();
        }

        let mut effects = Vec::new();
        if success {
            let prev_match = self.match_index.get(&follower_id).copied().unwrap_or(0);
            let updated_match = prev_match.max(match_index).min(self.last_log_index());
            self.match_index.insert(follower_id, updated_match);
            self.next_index
                .insert(follower_id, updated_match.saturating_add(1));
            self.snapshot_send_offset.remove(&follower_id);

            if let Some(advance) = self.try_advance_leader_commit() {
                effects.push(advance);
            }

            if let Some(domain) = self.roster.domain_of(follower_id)
                && let Some(round) = self.quorum_rounds.get_mut(&quorum_round_id)
                && round.term == self.current_term
            {
                round.acked_domains.insert(domain);
            }

            effects.extend(self.sweep_confirmed_quorum_rounds());

            // If follower still lags `last_log_index()`, pipeline the remaining tail.
            if updated_match < self.last_log_index() {
                let next_round = self.allocate_quorum_round(None);
                effects.push(self.build_replication_for_peer(follower_id, next_round));
            }
        } else {
            // Fast conflict backup (§5.3 extended):
            let backed_up_next = if let Some(ct) = conflict_term {
                if let Some(last_with_ct) = self
                    .log_tail
                    .iter()
                    .rev()
                    .find(|e| e.term == ct)
                    .map(|e| e.index)
                {
                    last_with_ct.saturating_add(1)
                } else {
                    conflict_index.max(1)
                }
            } else {
                conflict_index.max(1)
            };
            let current_next = self.next_index.get(&follower_id).copied().unwrap_or(1);
            let next_try = backed_up_next
                .min(current_next.saturating_sub(1).max(1))
                .max(1);
            self.next_index.insert(follower_id, next_try);
            self.snapshot_send_offset.remove(&follower_id);
            let retry_round = self.allocate_quorum_round(None);
            effects.push(self.build_replication_for_peer(follower_id, retry_round));
        }

        effects
    }

    /// Raft §5.4.2 Current-Term Majority Commit Rule:
    /// ONLY entries with `entry.term == self.current_term` may advance
    /// `commit_index` by counting quorum replicas!
    fn try_advance_leader_commit(&mut self) -> Option<ConsensusEffect> {
        let quorum = self.roster.quorum_size();
        let mut best_commit = self.commit_index;

        for candidate_idx in (self.commit_index + 1)..=self.last_log_index() {
            if self.term_at(candidate_idx) != Some(self.current_term) {
                continue;
            }
            let mut distinct_domains = BTreeSet::new();
            for voter in self.roster.members() {
                let m = self
                    .match_index
                    .get(&voter.cell_identity)
                    .copied()
                    .unwrap_or(0);
                if m >= candidate_idx {
                    distinct_domains.insert(voter.control_domain_id);
                }
            }
            if distinct_domains.len() >= quorum {
                best_commit = candidate_idx;
            }
        }

        if best_commit > self.commit_index {
            let old_commit = self.commit_index;
            self.commit_index = best_commit;
            let newly_committed = self.collect_range(old_commit + 1, best_commit);
            Some(ConsensusEffect::AdvanceCommitIndex {
                commit_index: best_commit,
                term: self.current_term,
                newly_committed,
            })
        } else {
            None
        }
    }

    fn has_authoritative_commit_view_in_current_term(&self) -> bool {
        (self.commit_index == 0 && self.last_log_index() == 0)
            || self.term_at(self.commit_index) == Some(self.current_term)
    }

    fn sweep_confirmed_quorum_rounds(&mut self) -> Vec<ConsensusEffect> {
        let quorum = self.roster.quorum_size();
        let authoritative_view = self.has_authoritative_commit_view_in_current_term();
        let mut confirmed_keys = Vec::new();
        let mut effects = Vec::new();

        for (&round_id, round) in &self.quorum_rounds {
            if round.term == self.current_term && round.acked_domains.len() >= quorum {
                // Renew term-bound monotonic leader lease on every current-term
                // quorum acknowledgement round!
                if round_id >= self.leader_lease.last_confirmed_round {
                    self.leader_lease = TermBoundLeaderLease {
                        term: self.current_term,
                        remaining_ticks: self.timing.leader_lease_ticks,
                        last_confirmed_round: round_id,
                    };
                }
                if authoritative_view || round.waiting_read_ids.is_empty() {
                    confirmed_keys.push(round_id);
                }
            }
        }

        for key in confirmed_keys {
            if let Some(done_round) = self.quorum_rounds.remove(&key) {
                let confirmed_idx = self.commit_index.max(done_round.commit_index_at_issue);
                for read_id in done_round.waiting_read_ids {
                    effects.push(ConsensusEffect::LinearizableReadConfirmed {
                        read_id,
                        confirmed_term: self.current_term,
                        commit_index: confirmed_idx,
                    });
                }
            }
        }
        effects
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn on_install_snapshot_chunk(
        &mut self,
        term: ConsensusTerm,
        leader_id: CellIdentity,
        last_included_index: LogIndex,
        last_included_term: ConsensusTerm,
        state_root_digest: [u8; 32],
        snapshot_sha256: [u8; 32],
        total_bytes: u64,
        offset: u64,
        chunk_data: &[u8],
        done: bool,
    ) -> Vec<ConsensusEffect> {
        let mut effects = Vec::new();
        if term > self.current_term {
            effects.extend(self.step_down_to_higher_term(term));
        }
        if term < self.current_term {
            effects.push(ConsensusEffect::SendMessage {
                target: leader_id,
                message: RaftMessage::InstallSnapshotResponse {
                    term: self.current_term,
                    follower_id: self.local_id,
                    last_included_index,
                    next_offset: 0,
                    installed: false,
                },
            });
            return effects;
        }

        self.role = ConsensusRole::Follower;
        self.known_leader = Some(leader_id);
        self.ticks_since_leader_contact = 0;

        // If follower already committed >= `last_included_index`, ack as installed.
        if last_included_index <= self.commit_index {
            effects.push(ConsensusEffect::SendMessage {
                target: leader_id,
                message: RaftMessage::InstallSnapshotResponse {
                    term: self.current_term,
                    follower_id: self.local_id,
                    last_included_index,
                    next_offset: total_bytes,
                    installed: true,
                },
            });
            return effects;
        }

        let reset_and_nack = |this: &mut Self, mut effs: Vec<ConsensusEffect>| {
            this.inbound_snapshot = None;
            effs.push(ConsensusEffect::SendMessage {
                target: leader_id,
                message: RaftMessage::InstallSnapshotResponse {
                    term: this.current_term,
                    follower_id: this.local_id,
                    last_included_index,
                    next_offset: 0,
                    installed: false,
                },
            });
            effs
        };

        let matches_existing = self.inbound_snapshot.as_ref().is_some_and(|s| {
            s.term == term
                && s.leader_id == leader_id
                && s.last_included_index == last_included_index
                && s.last_included_term == last_included_term
                && s.state_root_digest == state_root_digest
                && s.snapshot_sha256 == snapshot_sha256
                && s.total_bytes == total_bytes
        });

        if !matches_existing {
            if offset == 0 {
                let cap = usize::try_from(total_bytes).unwrap_or(0);
                self.inbound_snapshot = Some(InboundSnapshotStaging {
                    term,
                    leader_id,
                    last_included_index,
                    last_included_term,
                    state_root_digest,
                    snapshot_sha256,
                    total_bytes,
                    buffer: Vec::with_capacity(cap.min(4 * 1024 * 1024)),
                });
            } else {
                return reset_and_nack(self, effects);
            }
        }

        let Some(staging) = self.inbound_snapshot.as_mut() else {
            return reset_and_nack(self, effects);
        };
        let current_offset = staging.buffer.len() as u64;
        if offset != current_offset {
            // Preserve buffered prefix on duplicate or out-of-order chunk delivery
            // and report the exact contiguous offset already staged.
            effects.push(ConsensusEffect::SendMessage {
                target: leader_id,
                message: RaftMessage::InstallSnapshotResponse {
                    term: self.current_term,
                    follower_id: self.local_id,
                    last_included_index,
                    next_offset: current_offset,
                    installed: false,
                },
            });
            return effects;
        }

        staging.buffer.extend_from_slice(chunk_data);
        let new_offset = staging.buffer.len() as u64;
        if new_offset > total_bytes {
            return reset_and_nack(self, effects);
        }

        if !done {
            effects.push(ConsensusEffect::SendMessage {
                target: leader_id,
                message: RaftMessage::InstallSnapshotResponse {
                    term: self.current_term,
                    follower_id: self.local_id,
                    last_included_index,
                    next_offset: new_offset,
                    installed: false,
                },
            });
            return effects;
        }

        if new_offset != total_bytes {
            return reset_and_nack(self, effects);
        }

        let completed = self.inbound_snapshot.take().expect("staging present");
        let computed_sha = compute_snapshot_transfer_sha256(
            completed.last_included_index,
            completed.last_included_term,
            &completed.state_root_digest,
            &completed.buffer,
        );
        if computed_sha != completed.snapshot_sha256 {
            return reset_and_nack(self, effects);
        }

        // Preserve any local log suffix strictly after `last_included_index` ONLY
        // if `(last_included_index, last_included_term)` matched locally (§7).
        let matches_at_boundary = self.term_at(last_included_index) == Some(last_included_term);
        if matches_at_boundary {
            self.log_tail.retain(|e| e.index > last_included_index);
        } else {
            self.log_tail.clear();
        }

        let meta = SnapshotMetadata {
            included_index: last_included_index,
            included_term: last_included_term,
            state_root_digest: completed.state_root_digest,
        };
        self.snapshot_meta = Some(meta);
        self.snapshot_bytes = Some(completed.buffer.clone());
        self.commit_index = self.commit_index.max(last_included_index);

        effects.push(ConsensusEffect::InstallSnapshot {
            meta,
            snapshot_bytes: completed.buffer,
        });
        effects.push(ConsensusEffect::SendMessage {
            target: leader_id,
            message: RaftMessage::InstallSnapshotResponse {
                term: self.current_term,
                follower_id: self.local_id,
                last_included_index,
                next_offset: total_bytes,
                installed: true,
            },
        });
        effects
    }

    fn on_install_snapshot_response(
        &mut self,
        term: ConsensusTerm,
        follower_id: CellIdentity,
        last_included_index: LogIndex,
        next_offset: u64,
        installed: bool,
    ) -> Vec<ConsensusEffect> {
        if term > self.current_term {
            return self.step_down_to_higher_term(term);
        }
        if self.role != ConsensusRole::Leader || term != self.current_term {
            return Vec::new();
        }
        let Some(meta) = self.snapshot_meta else {
            return Vec::new();
        };
        if last_included_index != meta.included_index {
            return Vec::new();
        }

        if installed {
            self.snapshot_send_offset.remove(&follower_id);
            let prev = self.match_index.get(&follower_id).copied().unwrap_or(0);
            let updated = prev.max(last_included_index);
            self.match_index.insert(follower_id, updated);
            self.next_index
                .insert(follower_id, updated.saturating_add(1));

            let mut effects = Vec::new();
            if let Some(advance) = self.try_advance_leader_commit() {
                effects.push(advance);
            }
            if updated < self.last_log_index() {
                let round_id = self.allocate_quorum_round(None);
                effects.push(self.build_replication_for_peer(follower_id, round_id));
            }
            effects
        } else if next_offset == 0 {
            self.snapshot_send_offset.insert(follower_id, 0);
            let round_id = self.allocate_quorum_round(None);
            vec![self.build_replication_for_peer(follower_id, round_id)]
        } else {
            let prev_offset = self
                .snapshot_send_offset
                .get(&follower_id)
                .copied()
                .unwrap_or(0);
            if next_offset <= prev_offset {
                return Vec::new();
            }
            self.snapshot_send_offset.insert(follower_id, next_offset);
            let round_id = self.allocate_quorum_round(None);
            vec![self.build_replication_for_peer(follower_id, round_id)]
        }
    }

    fn collect_range(
        &self,
        start_inclusive: LogIndex,
        end_inclusive: LogIndex,
    ) -> Vec<RaftLogEntry> {
        if start_inclusive > end_inclusive {
            return Vec::new();
        }
        self.log_tail
            .iter()
            .filter(|e| e.index >= start_inclusive && e.index <= end_inclusive)
            .copied()
            .collect()
    }

    fn broadcast_replication_or_heartbeat(&mut self, quorum_round_id: u64) -> Vec<ConsensusEffect> {
        let peers: Vec<CellIdentity> = self
            .roster
            .members()
            .iter()
            .map(|m| m.cell_identity)
            .filter(|&id| id != self.local_id)
            .collect();
        let mut effects = Vec::with_capacity(peers.len());
        for peer in peers {
            effects.push(self.build_replication_for_peer(peer, quorum_round_id));
        }
        effects
    }

    fn build_replication_for_peer(
        &mut self,
        peer: CellIdentity,
        quorum_round_id: u64,
    ) -> ConsensusEffect {
        let next_idx = self
            .next_index
            .get(&peer)
            .copied()
            .unwrap_or_else(|| self.last_log_index().saturating_add(1))
            .max(1);

        if let (Some(meta), Some(snap_bytes)) = (self.snapshot_meta, self.snapshot_bytes.as_ref())
            && next_idx <= meta.included_index
        {
            let total_bytes = snap_bytes.len() as u64;
            let raw_offset = self.snapshot_send_offset.get(&peer).copied().unwrap_or(0);
            let offset_usize = usize::try_from(raw_offset)
                .unwrap_or(0)
                .min(snap_bytes.len());
            let chunk_cap = self.timing.max_snapshot_chunk_bytes.max(64);
            let end_usize = (offset_usize + chunk_cap).min(snap_bytes.len());
            let chunk_data = snap_bytes[offset_usize..end_usize].to_vec();
            let done = end_usize >= snap_bytes.len();
            let snapshot_sha256 = compute_snapshot_transfer_sha256(
                meta.included_index,
                meta.included_term,
                &meta.state_root_digest,
                snap_bytes,
            );

            return ConsensusEffect::SendMessage {
                target: peer,
                message: RaftMessage::InstallSnapshotChunk {
                    term: self.current_term,
                    leader_id: self.local_id,
                    last_included_index: meta.included_index,
                    last_included_term: meta.included_term,
                    state_root_digest: meta.state_root_digest,
                    snapshot_sha256,
                    total_bytes,
                    offset: offset_usize as u64,
                    chunk_data,
                    done,
                },
            };
        }

        let prev_log_index = next_idx - 1;
        let prev_log_term = self.term_at(prev_log_index).unwrap_or(0);
        let entries = self.collect_range(next_idx, self.last_log_index());

        ConsensusEffect::SendMessage {
            target: peer,
            message: RaftMessage::AppendEntries {
                term: self.current_term,
                leader_id: self.local_id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit: self.commit_index,
                quorum_round_id,
            },
        }
    }
}
