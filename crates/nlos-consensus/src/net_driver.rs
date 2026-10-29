//! Thin async Raft RPC & `InstallSnapshot` network driver over
//! `nlos_net::CellRpcTransport` (`MtlsCellTransport` + `MtlsEndpointListener`
//! TLS 1.3 mTLS + 5-axis Exporter `CellFence` sessions, `ADR-0026` §2.2, §3.3).
//!
//! # Strict Ordering & Safety Guarantees
//!
//! 1. **Persist-Before-Send / Persist-Before-Ack (`[DUR-ACK-002]`)**:
//!    Every `PersistHardState`, `InstallSnapshot`, `TruncateLogSuffixFrom`,
//!    `AppendLogEntries`, and `AdvanceCommitIndex` effect emitted by
//!    [`ConsensusCore::step`] is committed to `SQLite WAL + synchronous=FULL`
//!    BEFORE any outbound RPC response/request leaves the node or any client
//!    [`ConsensusCommitReceipt`] future resolves.
//! 2. **Authenticated Nominal Peer Binding (`[TCB-ENDPOINT-001]`)**:
//!    Inbound `ControlRpcRequest` handlers extract `from` exclusively from the
//!    authenticated 5-axis `request.peer_session.peer_claims.cell_identity`
//!    and drop/refuse any mismatched payload identity.
//! 3. **Zero Lock-Across-Await**:
//!    `ConsensusCore` + `SqliteConsensusStore` + `ControlPlaneStateMachine`
//!    live behind a synchronous `std::sync::Mutex` that is never held across
//!    `.await` points.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nlos_cell::{CellEpoch, CellIdentity, RemoteCallContract, RemoteCallOutcome};
use nlos_net::{
    CellRpcTransport, ClusterProfile, EndpointResolver, IncomingRpcRequest, MtlsCellTransport,
    MtlsEndpointListener, NetInitError, NetTransportConfig, NodeSecurityDeps, RpcHandlerReply,
    RpcRequestHandler, RpcWireStatus, RunningMtlsListener, SocketActivityAccounting,
};
use nlos_types::{IdempotencyKey, SchedulerDomainId};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::core::{
    ConsensusCore, ConsensusEffect, ConsensusInput, ConsensusRole, ConsensusTerm,
    ConsensusTimingConfig, LogIndex, ProposalRejectReason, RaftLogEntry, RaftMessage,
    SnapshotMetadata, TermBoundLeaderLease, ValidatedVoterRoster, compute_log_entry_sha256,
};
use crate::state_machine::{
    CommandFamilyCode, ControlPlaneProposal, ControlPlaneStateMachine, ScopeViolationError,
};
use crate::store::{ConsensusCommitReceipt, SqliteConsensusStore};

const RAFT_WIRE_MAGIC: &[u8; 8] = b"NLOSRF01";

/// Wire serialization / framing errors for [`RaftMessage`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RaftWireCodecError {
    /// Missing `NLOSRF01` header.
    InvalidMagic,
    /// Frame truncated before expected field boundary.
    Truncated,
    /// Unexpected trailing bytes after message body.
    TrailingBytes,
    /// Unknown `RaftMessage` tag or invalid nominal value (`CellIdentity` / `NonZeroU64`).
    InvalidField(&'static str),
    /// Trailing `SHA-256` frame checksum or embedded `entry_sha256` mismatched.
    ChecksumMismatch,
    /// Embedded log entry violated `[CONS-SCOPE-001]`.
    ScopeViolation(ScopeViolationError),
}

impl fmt::Display for RaftWireCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic => formatter.write_str("invalid NLOSRF01 Raft wire magic"),
            Self::Truncated => formatter.write_str("truncated Raft wire message"),
            Self::TrailingBytes => formatter.write_str("trailing bytes in Raft wire message"),
            Self::InvalidField(f) => write!(formatter, "invalid Raft wire field: {f}"),
            Self::ChecksumMismatch => formatter.write_str("Raft wire SHA-256 checksum mismatch"),
            Self::ScopeViolation(err) => write!(formatter, "Raft wire scope violation: {err}"),
        }
    }
}

impl Error for RaftWireCodecError {}

/// Encodes a [`RaftMessage`] into a checksummed `NLOSRF01` binary frame.
///
/// # Panics
///
/// Panics only if an entry vector or snapshot chunk exceeds `u32::MAX` bytes.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn encode_raft_message(message: &RaftMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    out.extend_from_slice(RAFT_WIRE_MAGIC);
    match message {
        RaftMessage::RequestPreVote {
            prospective_term,
            candidate_id,
            last_log_index,
            last_log_term,
        } => {
            out.push(1);
            out.extend_from_slice(&prospective_term.to_be_bytes());
            out.extend_from_slice(candidate_id.as_bytes());
            out.extend_from_slice(&last_log_index.to_be_bytes());
            out.extend_from_slice(&last_log_term.to_be_bytes());
        }
        RaftMessage::RequestPreVoteResponse {
            term,
            prospective_term,
            voter_id,
            vote_granted,
        } => {
            out.push(2);
            out.extend_from_slice(&term.to_be_bytes());
            out.extend_from_slice(&prospective_term.to_be_bytes());
            out.extend_from_slice(voter_id.as_bytes());
            out.push(u8::from(*vote_granted));
        }
        RaftMessage::RequestVote {
            term,
            candidate_id,
            last_log_index,
            last_log_term,
        } => {
            out.push(3);
            out.extend_from_slice(&term.to_be_bytes());
            out.extend_from_slice(candidate_id.as_bytes());
            out.extend_from_slice(&last_log_index.to_be_bytes());
            out.extend_from_slice(&last_log_term.to_be_bytes());
        }
        RaftMessage::RequestVoteResponse {
            term,
            voter_id,
            vote_granted,
        } => {
            out.push(4);
            out.extend_from_slice(&term.to_be_bytes());
            out.extend_from_slice(voter_id.as_bytes());
            out.push(u8::from(*vote_granted));
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
            out.push(5);
            out.extend_from_slice(&term.to_be_bytes());
            out.extend_from_slice(leader_id.as_bytes());
            out.extend_from_slice(&prev_log_index.to_be_bytes());
            out.extend_from_slice(&prev_log_term.to_be_bytes());
            out.extend_from_slice(&leader_commit.to_be_bytes());
            out.extend_from_slice(&quorum_round_id.to_be_bytes());
            let count = u32::try_from(entries.len()).expect("entries len <= u32::MAX");
            out.extend_from_slice(&count.to_be_bytes());
            for entry in entries {
                out.extend_from_slice(&entry.index.to_be_bytes());
                out.extend_from_slice(&entry.term.to_be_bytes());
                out.push(entry.proposal.command.family_code().as_u8());
                out.extend_from_slice(entry.proposal.idempotency_key.as_bytes());
                let body = entry.proposal.encode_command_body();
                let body_len = u32::try_from(body.len()).expect("body len <= u32::MAX");
                out.extend_from_slice(&body_len.to_be_bytes());
                out.extend_from_slice(&body);
                out.extend_from_slice(&entry.entry_sha256);
            }
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
            out.push(6);
            out.extend_from_slice(&term.to_be_bytes());
            out.extend_from_slice(follower_id.as_bytes());
            out.push(u8::from(*success));
            out.extend_from_slice(&match_index.to_be_bytes());
            match conflict_term {
                Some(ct) => {
                    out.push(1);
                    out.extend_from_slice(&ct.to_be_bytes());
                }
                None => out.push(0),
            }
            out.extend_from_slice(&conflict_index.to_be_bytes());
            out.extend_from_slice(&quorum_round_id.to_be_bytes());
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
            out.push(7);
            out.extend_from_slice(&term.to_be_bytes());
            out.extend_from_slice(leader_id.as_bytes());
            out.extend_from_slice(&last_included_index.to_be_bytes());
            out.extend_from_slice(&last_included_term.to_be_bytes());
            out.extend_from_slice(state_root_digest);
            out.extend_from_slice(snapshot_sha256);
            out.extend_from_slice(&total_bytes.to_be_bytes());
            out.extend_from_slice(&offset.to_be_bytes());
            out.push(u8::from(*done));
            let chunk_len = u32::try_from(chunk_data.len()).expect("chunk <= u32::MAX");
            out.extend_from_slice(&chunk_len.to_be_bytes());
            out.extend_from_slice(chunk_data);
        }
        RaftMessage::InstallSnapshotResponse {
            term,
            follower_id,
            last_included_index,
            next_offset,
            installed,
        } => {
            out.push(8);
            out.extend_from_slice(&term.to_be_bytes());
            out.extend_from_slice(follower_id.as_bytes());
            out.extend_from_slice(&last_included_index.to_be_bytes());
            out.extend_from_slice(&next_offset.to_be_bytes());
            out.push(u8::from(*installed));
        }
    }
    let checksum: [u8; 32] = Sha256::digest(&out).into();
    out.extend_from_slice(&checksum);
    out
}

/// Decodes and integrity-verifies a [`RaftMessage`] wire frame.
///
/// # Errors
///
/// Fails closed on magic, framing, checksum, or `[CONS-SCOPE-001]` violations.
#[allow(clippy::too_many_lines)]
pub fn decode_raft_message(bytes: &[u8]) -> Result<RaftMessage, RaftWireCodecError> {
    if bytes.len() < 8 + 1 + 32 {
        return Err(RaftWireCodecError::Truncated);
    }
    if &bytes[0..8] != RAFT_WIRE_MAGIC {
        return Err(RaftWireCodecError::InvalidMagic);
    }
    let payload_end = bytes.len() - 32;
    let expected_checksum: [u8; 32] = Sha256::digest(&bytes[..payload_end]).into();
    if bytes[payload_end..] != expected_checksum {
        return Err(RaftWireCodecError::ChecksumMismatch);
    }

    let mut cur = WireReader::new(&bytes[8..payload_end]);
    let tag = cur.read_u8()?;
    let msg = match tag {
        1 => RaftMessage::RequestPreVote {
            prospective_term: cur.read_u64()?,
            candidate_id: cur.read_cell()?,
            last_log_index: cur.read_u64()?,
            last_log_term: cur.read_u64()?,
        },
        2 => RaftMessage::RequestPreVoteResponse {
            term: cur.read_u64()?,
            prospective_term: cur.read_u64()?,
            voter_id: cur.read_cell()?,
            vote_granted: cur.read_bool()?,
        },
        3 => RaftMessage::RequestVote {
            term: cur.read_u64()?,
            candidate_id: cur.read_cell()?,
            last_log_index: cur.read_u64()?,
            last_log_term: cur.read_u64()?,
        },
        4 => RaftMessage::RequestVoteResponse {
            term: cur.read_u64()?,
            voter_id: cur.read_cell()?,
            vote_granted: cur.read_bool()?,
        },
        5 => {
            let term = cur.read_u64()?;
            let leader_id = cur.read_cell()?;
            let prev_log_index = cur.read_u64()?;
            let prev_log_term = cur.read_u64()?;
            let leader_commit = cur.read_u64()?;
            let quorum_round_id = cur.read_u64()?;
            let count = usize::try_from(cur.read_u32()?).unwrap_or(0);
            if count > 4096 {
                return Err(RaftWireCodecError::InvalidField(
                    "AppendEntries count > 4096",
                ));
            }
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let index = cur.read_u64()?;
                let entry_term = cur.read_u64()?;
                let raw_family = cur.read_u8()?;
                let family = CommandFamilyCode::from_wire_tag(raw_family)
                    .map_err(|r| RaftWireCodecError::ScopeViolation(r.into()))?;
                let idem_bytes = cur.read_16()?;
                let body_len = usize::try_from(cur.read_u32()?).unwrap_or(0);
                let body = cur.read_slice(body_len)?;
                let entry_sha256 = cur.read_32()?;
                let expected_entry_sha =
                    compute_log_entry_sha256(index, entry_term, family.as_u8(), &idem_bytes, body);
                if entry_sha256 != expected_entry_sha {
                    return Err(RaftWireCodecError::ChecksumMismatch);
                }
                let proposal = ControlPlaneProposal::decode_command_body(
                    family,
                    IdempotencyKey::from_bytes(idem_bytes),
                    body,
                )
                .map_err(RaftWireCodecError::ScopeViolation)?;
                entries.push(RaftLogEntry {
                    index,
                    term: entry_term,
                    proposal,
                    entry_sha256,
                });
            }
            RaftMessage::AppendEntries {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
                quorum_round_id,
            }
        }
        6 => {
            let term = cur.read_u64()?;
            let follower_id = cur.read_cell()?;
            let success = cur.read_bool()?;
            let match_index = cur.read_u64()?;
            let conflict_term = match cur.read_u8()? {
                0 => None,
                1 => Some(cur.read_u64()?),
                _ => return Err(RaftWireCodecError::InvalidField("conflict_term flag")),
            };
            let conflict_index = cur.read_u64()?;
            let quorum_round_id = cur.read_u64()?;
            RaftMessage::AppendEntriesResponse {
                term,
                follower_id,
                success,
                match_index,
                conflict_term,
                conflict_index,
                quorum_round_id,
            }
        }
        7 => {
            let term = cur.read_u64()?;
            let leader_id = cur.read_cell()?;
            let last_included_index = cur.read_u64()?;
            let last_included_term = cur.read_u64()?;
            let state_root_digest = cur.read_32()?;
            let snapshot_sha256 = cur.read_32()?;
            let total_bytes = cur.read_u64()?;
            let offset = cur.read_u64()?;
            let done = cur.read_bool()?;
            let chunk_len = usize::try_from(cur.read_u32()?).unwrap_or(0);
            let chunk_data = cur.read_slice(chunk_len)?.to_vec();
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
            }
        }
        8 => RaftMessage::InstallSnapshotResponse {
            term: cur.read_u64()?,
            follower_id: cur.read_cell()?,
            last_included_index: cur.read_u64()?,
            next_offset: cur.read_u64()?,
            installed: cur.read_bool()?,
        },
        _ => return Err(RaftWireCodecError::InvalidField("unknown RaftMessage tag")),
    };

    if !cur.is_empty() {
        return Err(RaftWireCodecError::TrailingBytes);
    }
    Ok(msg)
}

struct WireReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    const fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }

    fn read_slice(&mut self, len: usize) -> Result<&'a [u8], RaftWireCodecError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(RaftWireCodecError::Truncated)?;
        if end > self.buf.len() {
            return Err(RaftWireCodecError::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8, RaftWireCodecError> {
        Ok(self.read_slice(1)?[0])
    }

    fn read_bool(&mut self) -> Result<bool, RaftWireCodecError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(RaftWireCodecError::InvalidField("bool")),
        }
    }

    fn read_u32(&mut self) -> Result<u32, RaftWireCodecError> {
        Ok(u32::from_be_bytes(
            self.read_slice(4)?.try_into().expect("4B"),
        ))
    }

    fn read_u64(&mut self) -> Result<u64, RaftWireCodecError> {
        Ok(u64::from_be_bytes(
            self.read_slice(8)?.try_into().expect("8B"),
        ))
    }

    fn read_16(&mut self) -> Result<[u8; 16], RaftWireCodecError> {
        Ok(self.read_slice(16)?.try_into().expect("16B"))
    }

    fn read_32(&mut self) -> Result<[u8; 32], RaftWireCodecError> {
        Ok(self.read_slice(32)?.try_into().expect("32B"))
    }

    fn read_cell(&mut self) -> Result<CellIdentity, RaftWireCodecError> {
        let bytes = self.read_16()?;
        if bytes == [0u8; 16] {
            return Err(RaftWireCodecError::InvalidField("zero CellIdentity"));
        }
        Ok(CellIdentity::from_domain(SchedulerDomainId::from_bytes(
            bytes,
        )))
    }
}

/// Computes deterministic rank-staggered [`ConsensusTimingConfig`] for
/// `local_id` within `roster` so simultaneous multi-node boots elect a leader
/// cleanly without random number generators.
#[must_use]
pub fn staggered_timing_for_roster(
    local_id: CellIdentity,
    roster: &ValidatedVoterRoster,
) -> ConsensusTimingConfig {
    let rank = roster
        .members()
        .iter()
        .position(|m| m.cell_identity == local_id)
        .unwrap_or(0);
    let rank_u32 = u32::try_from(rank).unwrap_or(0);
    ConsensusTimingConfig {
        heartbeat_interval_ticks: 2,
        leader_lease_ticks: 6,
        election_timeout_ticks: 12_u32.saturating_add(rank_u32.saturating_mul(6)),
        max_snapshot_chunk_bytes: crate::core::DEFAULT_SNAPSHOT_CHUNK_BYTES,
    }
}

/// Configuration for [`MultiHostQuorumNode`].
#[derive(Clone, Debug)]
pub struct MultiHostQuorumConfig {
    /// Local nominal `CellIdentity` (`[DIST-NAME-001]`).
    pub local_cell: CellIdentity,
    /// Ephemeral local listener bind hint (`127.0.0.1:0` or explicit `IP:port`).
    pub bind_hint: SocketAddr,
    /// Validated `N >= 3` independent-domain voter roster (`[TASK-SCALE-002]`).
    pub roster: ValidatedVoterRoster,
    /// Monotonic tick timing bounds (`None` uses [`staggered_timing_for_roster`]).
    pub timing: Option<ConsensusTimingConfig>,
    /// Physical monotonic tick cadence driving [`ConsensusInput::Tick`].
    pub tick_interval: Duration,
    /// `nlos-net` TLS 1.3 + multiplexed stream configuration.
    pub net_config: NetTransportConfig,
}

impl MultiHostQuorumConfig {
    /// Creates a standard multi-host quorum configuration with 20ms tick cadence
    /// and deterministic rank-staggered election timeouts.
    #[must_use]
    pub fn new(
        local_cell: CellIdentity,
        bind_hint: SocketAddr,
        roster: ValidatedVoterRoster,
    ) -> Self {
        Self {
            local_cell,
            bind_hint,
            roster,
            timing: None,
            tick_interval: Duration::from_millis(20),
            net_config: NetTransportConfig {
                connect_and_tls_timeout: Duration::from_millis(800),
                fence_handshake_timeout: Duration::from_millis(800),
                rpc_call_timeout: Duration::from_millis(800),
                bulk_transfer_timeout: Duration::from_secs(10),
                timeout_elapsed_ticks: 4,
                mux: nlos_net::MuxConfig::default(),
            },
        }
    }
}

struct PendingProposalWaiter {
    term: ConsensusTerm,
    tx: oneshot::Sender<Result<ConsensusCommitReceipt, ConsensusError>>,
}

struct PendingReadWaiter {
    round_id: Option<u64>,
    tx: oneshot::Sender<Result<(LogIndex, ConsensusTerm), ConsensusError>>,
}

struct DriverSyncState {
    store: SqliteConsensusStore,
    core: ConsensusCore,
    state_machine: ControlPlaneStateMachine,
    pending_proposals: BTreeMap<LogIndex, PendingProposalWaiter>,
    pending_reads: BTreeMap<u64, PendingReadWaiter>,
    sticky_fatal_error: Option<String>,
}

struct SharedEngine {
    local_cell: CellIdentity,
    observed_epoch_fn: Arc<dyn Fn() -> CellEpoch + Send + Sync>,
    sync_state: Mutex<DriverSyncState>,
    peer_outbound_txs: BTreeMap<CellIdentity, mpsc::UnboundedSender<RaftMessage>>,
    next_read_id: AtomicU64,
    next_rpc_seq: AtomicU64,
    shutting_down: AtomicBool,
}

type ReadWaiterRegistration = (
    u64,
    oneshot::Sender<Result<(LogIndex, ConsensusTerm), ConsensusError>>,
);

impl SharedEngine {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, DriverSyncState> {
        self.sync_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Processes a [`ConsensusInput`], durably commits all storage effects to
    /// `SQLite WAL + synchronous=FULL` BEFORE releasing any outbound messages or
    /// waking any client receipt waiters (`[DUR-ACK-002]`), and optionally
    /// extracts one direct response message for `direct_reply_peer`.
    #[allow(clippy::too_many_lines)]
    fn step_and_persist(
        &self,
        input: ConsensusInput,
        direct_reply_peer: Option<CellIdentity>,
        proposal_waiter: Option<oneshot::Sender<Result<ConsensusCommitReceipt, ConsensusError>>>,
        read_waiter: Option<ReadWaiterRegistration>,
    ) -> Result<Option<RaftMessage>, ConsensusError> {
        let mut outbound = Vec::new();
        let mut direct_reply = None;

        {
            let mut guard = self.lock_state();
            if let Some(ref fatal) = guard.sticky_fatal_error {
                return Err(ConsensusError::DriverFatal(fatal.clone()));
            }

            let effects = guard.core.step(input);

            // Check immediate proposal rejection BEFORE touching SQLite.
            for eff in &effects {
                if let ConsensusEffect::ProposalRejected {
                    read_id: None,
                    reason,
                } = eff
                {
                    if let Some(tx) = proposal_waiter {
                        let _ = tx.send(Err(ConsensusError::ProposalRejected(*reason)));
                    }
                    return Ok(None);
                }
            }

            // If a proposal succeeded locally (`AppendLogEntries`), register its
            // `[DUR-ACK-002]` waiter BEFORE `apply_durable_effects` (in case a
            // single-node or immediate commit resolves in the same step).
            if let Some(tx) = proposal_waiter {
                for eff in &effects {
                    if let ConsensusEffect::AppendLogEntries(entries) = eff
                        && let Some(first) = entries.first()
                    {
                        if let Some(displaced) = guard.pending_proposals.insert(
                            first.index,
                            PendingProposalWaiter {
                                term: first.term,
                                tx,
                            },
                        ) {
                            let current_term = guard.core.current_term();
                            let known_leader = guard.core.known_leader();
                            let _ = displaced.tx.send(Err(ConsensusError::ProposalRejected(
                                ProposalRejectReason::StaleLeaderTerm {
                                    attempted_term: displaced.term,
                                    current_term,
                                    known_leader,
                                },
                            )));
                        }
                        break;
                    }
                }
            }

            if let Some((rid, rtx)) = read_waiter {
                let round_id = effects.iter().find_map(|eff| match eff {
                    ConsensusEffect::SendMessage {
                        message:
                            RaftMessage::AppendEntries {
                                quorum_round_id, ..
                            },
                        ..
                    } => Some(*quorum_round_id),
                    _ => None,
                });
                guard
                    .pending_reads
                    .insert(rid, PendingReadWaiter { round_id, tx: rtx });
            }

            // Persist all hard-state, snapshot, truncation, append, and commit
            // effects in ONE atomic `SQLite WAL + synchronous=FULL` transaction!
            let DriverSyncState {
                store,
                state_machine,
                ..
            } = &mut *guard;
            let committed_receipts = match store.apply_durable_effects(&effects, state_machine) {
                Ok(receipts) => receipts,
                Err(err) => {
                    let msg = err.to_string();
                    guard.sticky_fatal_error = Some(msg);
                    return Err(ConsensusError::Store(err));
                }
            };

            // Handle truncation/step-down fencing of uncommitted waiters.
            let current_term_now = guard.core.current_term();
            let known_leader_now = guard.core.known_leader();

            for eff in &effects {
                match eff {
                    ConsensusEffect::TruncateLogSuffixFrom { from_index } => {
                        let split = guard.pending_proposals.split_off(from_index);
                        for (_, w) in split {
                            let _ = w.tx.send(Err(ConsensusError::ProposalRejected(
                                ProposalRejectReason::StaleLeaderTerm {
                                    attempted_term: w.term,
                                    current_term: current_term_now,
                                    known_leader: known_leader_now,
                                },
                            )));
                        }
                    }
                    ConsensusEffect::LeaderSteppedDown {
                        aborted_term,
                        new_term,
                    } => {
                        let all_props = std::mem::take(&mut guard.pending_proposals);
                        for (_, w) in all_props {
                            let _ = w.tx.send(Err(ConsensusError::ProposalRejected(
                                ProposalRejectReason::StaleLeaderTerm {
                                    attempted_term: w.term.min(*aborted_term),
                                    current_term: *new_term,
                                    known_leader: known_leader_now,
                                },
                            )));
                        }
                    }
                    ConsensusEffect::ProposalRejected {
                        read_id: Some(rid),
                        reason,
                    } => {
                        if let Some(w) = guard.pending_reads.remove(rid) {
                            let _ = w.tx.send(Err(ConsensusError::ProposalRejected(*reason)));
                        }
                    }
                    ConsensusEffect::LinearizableReadConfirmed {
                        read_id,
                        confirmed_term,
                        commit_index,
                    } => {
                        if let Some(w) = guard.pending_reads.remove(read_id) {
                            let _ = w.tx.send(Ok((*commit_index, *confirmed_term)));
                        }
                    }
                    _ => {}
                }
            }

            // Deliver `[DUR-ACK-002]` receipts ONLY after `WAL+FULL` commit!
            for (entry, receipt) in committed_receipts {
                if let Some(waiter) = guard.pending_proposals.remove(&entry.index) {
                    if waiter.term == entry.term {
                        let _ = waiter.tx.send(Ok(receipt));
                    } else {
                        let _ = waiter.tx.send(Err(ConsensusError::ProposalRejected(
                            ProposalRejectReason::StaleLeaderTerm {
                                attempted_term: waiter.term,
                                current_term: entry.term,
                                known_leader: known_leader_now,
                            },
                        )));
                    }
                }
            }

            for eff in effects {
                if let ConsensusEffect::SendMessage { target, message } = eff {
                    if direct_reply.is_none()
                        && direct_reply_peer == Some(target)
                        && is_response_message(&message)
                    {
                        direct_reply = Some(message);
                    } else {
                        outbound.push((target, message));
                    }
                }
            }
        }

        if !self.shutting_down.load(Ordering::Acquire) {
            for (target, message) in outbound {
                if let Some(peer_tx) = self.peer_outbound_txs.get(&target) {
                    let _ = peer_tx.send(message);
                }
            }
        }

        Ok(direct_reply)
    }

    fn snapshot_coalesce_state(&self) -> (ConsensusTerm, Vec<u64>) {
        let guard = self.lock_state();
        let local_term = guard.core.current_term();
        let active_read_rounds = guard
            .pending_reads
            .values()
            .filter_map(|w| w.round_id)
            .collect();
        (local_term, active_read_rounds)
    }

    fn next_call_contract(&self) -> RemoteCallContract {
        let seq = self.next_rpc_seq.fetch_add(1, Ordering::Relaxed);
        let mut key_bytes = [0u8; 16];
        key_bytes[0..8].copy_from_slice(&self.local_cell.as_bytes()[0..8]);
        key_bytes[8..16].copy_from_slice(&seq.to_be_bytes());
        // Raft RPC messages carry explicit `(term, index / round_id)` idempotency
        // at the consensus layer; marking transport frame `side_effecting = false`
        // prevents transient peer crash sockets from tripping the client's
        // `PendingReconciliationTracker` when Raft heartbeats retry.
        RemoteCallContract::new(
            key_bytes,
            IdempotencyKey::from_bytes(key_bytes),
            seq.saturating_add(100),
            0,
            0,
            false,
        )
    }
}

const fn is_response_message(msg: &RaftMessage) -> bool {
    matches!(
        msg,
        RaftMessage::RequestPreVoteResponse { .. }
            | RaftMessage::RequestVoteResponse { .. }
            | RaftMessage::AppendEntriesResponse { .. }
            | RaftMessage::InstallSnapshotResponse { .. }
    )
}

const fn outbound_message_term(msg: &RaftMessage) -> ConsensusTerm {
    match msg {
        RaftMessage::RequestPreVote {
            prospective_term, ..
        } => *prospective_term,
        RaftMessage::RequestPreVoteResponse { term, .. }
        | RaftMessage::RequestVote { term, .. }
        | RaftMessage::RequestVoteResponse { term, .. }
        | RaftMessage::AppendEntries { term, .. }
        | RaftMessage::AppendEntriesResponse { term, .. }
        | RaftMessage::InstallSnapshotChunk { term, .. }
        | RaftMessage::InstallSnapshotResponse { term, .. } => *term,
    }
}

fn coalesce_outbound_peer_batch(
    raw_batch: Vec<RaftMessage>,
    local_current_term: ConsensusTerm,
    active_read_rounds: &[u64],
) -> Vec<RaftMessage> {
    // 1) Drop stale outbound messages whose carried term (or `prospective_term`)
    //    is strictly less than local `current_term`.
    let term_filtered: Vec<RaftMessage> = raw_batch
        .into_iter()
        .filter(|msg| outbound_message_term(msg) >= local_current_term)
        .collect();

    // 2) Drop pure empty `AppendEntries` heartbeats superseded by a later
    //    same-term or higher-term `AppendEntries` or `InstallSnapshotChunk`
    //    (preserving any round actively backing a pending `ReadIndex` fence).
    let mut kept_rev = Vec::with_capacity(term_filtered.len());
    let mut max_later_repl_term: Option<ConsensusTerm> = None;
    for msg in term_filtered.into_iter().rev() {
        match &msg {
            RaftMessage::AppendEntries {
                term,
                entries,
                quorum_round_id,
                ..
            } => {
                let is_droppable_heartbeat =
                    entries.is_empty() && !active_read_rounds.contains(quorum_round_id);
                if is_droppable_heartbeat
                    && max_later_repl_term.is_some_and(|later_term| later_term >= *term)
                {
                    continue;
                }
                max_later_repl_term =
                    Some(max_later_repl_term.map_or(*term, |prev| prev.max(*term)));
                kept_rev.push(msg);
            }
            RaftMessage::InstallSnapshotChunk { term, .. } => {
                max_later_repl_term =
                    Some(max_later_repl_term.map_or(*term, |prev| prev.max(*term)));
                kept_rev.push(msg);
            }
            _ => kept_rev.push(msg),
        }
    }
    kept_rev.reverse();

    // 3) Deduplicate adjacent identical messages (`msg_a == msg_b`, e.g.,
    //    duplicate `InstallSnapshotChunk` at the same offset or `RequestPreVote`).
    kept_rev.dedup();
    kept_rev
}

fn spawn_peer_outbound_worker(
    target: CellIdentity,
    mut peer_rx: mpsc::UnboundedReceiver<RaftMessage>,
    eng: Arc<SharedEngine>,
    tr: Arc<dyn CellRpcTransport>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        'worker: loop {
            let first_msg = tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break 'worker;
                    }
                    continue 'worker;
                }
                maybe_msg = peer_rx.recv() => {
                    let Some(msg) = maybe_msg else {
                        break 'worker;
                    };
                    msg
                }
            };

            let mut raw_batch = vec![first_msg];
            while let Ok(next_msg) = peer_rx.try_recv() {
                raw_batch.push(next_msg);
            }

            let (local_term, active_read_rounds) = eng.snapshot_coalesce_state();
            let batch = coalesce_outbound_peer_batch(raw_batch, local_term, &active_read_rounds);

            for raft_msg in batch {
                if eng.shutting_down.load(Ordering::Acquire) || *shutdown_rx.borrow() {
                    break 'worker;
                }
                if outbound_message_term(&raft_msg) < eng.lock_state().core.current_term() {
                    continue;
                }
                let contract = eng.next_call_contract();
                let payload = encode_raft_message(&raft_msg);
                let rpc_res = tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break 'worker;
                        }
                        continue 'worker;
                    }
                    res = tr.call_cell_rpc(target, &contract, payload) => res,
                };

                if let Ok(RemoteCallOutcome::Succeeded { value, .. }) = rpc_res
                    && !value.payload.is_empty()
                    && let Ok(reply_msg) = decode_raft_message(&value.payload)
                {
                    let _ = eng.step_and_persist(
                        ConsensusInput::ReceiveMessage {
                            from: target,
                            message: reply_msg,
                        },
                        None,
                        None,
                        None,
                    );
                }
            }
        }
    })
}

struct ConsensusRpcAdapter {
    engine: Arc<SharedEngine>,
}

impl RpcRequestHandler for ConsensusRpcAdapter {
    fn handle_rpc(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        let engine = Arc::clone(&self.engine);
        Box::pin(async move {
            let observed_epoch = (engine.observed_epoch_fn)();
            if engine.shutting_down.load(Ordering::Acquire) {
                return RpcHandlerReply {
                    observed_epoch,
                    idempotency_replayed: false,
                    status: RpcWireStatus::PreExecutionUnavailable,
                    payload: Vec::new(),
                };
            }

            // `[TCB-ENDPOINT-001]`: peer identity is cryptographically pinned by
            // TLS 1.3 mTLS + 5-axis Exporter challenge-response!
            let authenticated_from = request.peer_session.peer_claims.cell_identity;
            let Ok(message) = decode_raft_message(&request.payload) else {
                return RpcHandlerReply {
                    observed_epoch,
                    idempotency_replayed: false,
                    status: RpcWireStatus::PreExecutionUnavailable,
                    payload: Vec::new(),
                };
            };

            match engine.step_and_persist(
                ConsensusInput::ReceiveMessage {
                    from: authenticated_from,
                    message,
                },
                Some(authenticated_from),
                None,
                None,
            ) {
                Ok(Some(reply_msg)) => RpcHandlerReply {
                    observed_epoch,
                    idempotency_replayed: false,
                    status: RpcWireStatus::Ok,
                    payload: encode_raft_message(&reply_msg),
                },
                Ok(None) => RpcHandlerReply {
                    observed_epoch,
                    idempotency_replayed: false,
                    status: RpcWireStatus::Ok,
                    payload: Vec::new(),
                },
                Err(_) => RpcHandlerReply {
                    observed_epoch,
                    idempotency_replayed: false,
                    status: RpcWireStatus::PreExecutionUnavailable,
                    payload: Vec::new(),
                },
            }
        })
    }
}

/// Multi-host (`N >= 3`) embedded Raft control-plane node backed by
/// `SqliteConsensusStore` (`WAL + FULL`) and `nlos-net` TLS 1.3 mTLS + 5-axis
/// Exporter transport (`ADR-0026` `C-CONSENSUS-CP`).
pub struct MultiHostQuorumNode {
    local_cell: CellIdentity,
    local_addr: SocketAddr,
    roster: ValidatedVoterRoster,
    accounting: SocketActivityAccounting,
    engine: Arc<SharedEngine>,
    shutdown_tx: watch::Sender<bool>,
    listener_slot: tokio::sync::Mutex<Option<RunningMtlsListener>>,
    worker_handles: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl fmt::Debug for MultiHostQuorumNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MultiHostQuorumNode")
            .field("local_cell", &self.local_cell)
            .field("local_addr", &self.local_addr)
            .field("quorum_size", &self.roster.quorum_size())
            .finish_non_exhaustive()
    }
}

impl MultiHostQuorumNode {
    /// Opens or recovers `store_dir`, binds an `MtlsEndpointListener`, creates
    /// an `MtlsCellTransport`, and starts the background Raft RPC & monotonic
    /// tick drivers.
    ///
    /// # Errors
    ///
    /// Fails closed on `[TASK-SCALE-002]` roster violations, `SQLite` durability
    /// or `[DUR-FAILCLOSED-001]` corruption errors, or TLS 1.3 / socket bind
    /// failures.
    #[allow(clippy::too_many_lines)]
    pub async fn bind_and_start(
        store_dir: impl AsRef<Path>,
        config: MultiHostQuorumConfig,
        resolver: Arc<dyn EndpointResolver>,
        security: NodeSecurityDeps,
    ) -> Result<Self, ConsensusError> {
        if config.roster.len() < 3 {
            return Err(ConsensusError::Config(
                crate::core::ConsensusConfigError::UnsupportedTwoNodeQuorum,
            ));
        }
        let timing = match config.timing {
            Some(t) => t.validate()?,
            None => staggered_timing_for_roster(config.local_cell, &config.roster).validate()?,
        };

        let (store, recovered) = SqliteConsensusStore::open_or_recover(store_dir)?;
        let core = ConsensusCore::rehydrate(
            config.local_cell,
            config.roster.clone(),
            timing,
            recovered.core_state,
        )?;

        let mut peer_outbound_txs = BTreeMap::new();
        let mut peer_receivers = Vec::with_capacity(config.roster.len().saturating_sub(1));
        for member in config.roster.members() {
            if member.cell_identity == config.local_cell {
                continue;
            }
            let (peer_tx, peer_rx) = mpsc::unbounded_channel::<RaftMessage>();
            peer_outbound_txs.insert(member.cell_identity, peer_tx);
            peer_receivers.push((member.cell_identity, peer_rx));
        }

        let local_fence_fn = Arc::clone(&security.local_fence);
        let observed_epoch_fn: Arc<dyn Fn() -> CellEpoch + Send + Sync> =
            Arc::new(move || (local_fence_fn)().epoch());

        let engine = Arc::new(SharedEngine {
            local_cell: config.local_cell,
            observed_epoch_fn,
            sync_state: Mutex::new(DriverSyncState {
                store,
                core,
                state_machine: recovered.state_machine,
                pending_proposals: BTreeMap::new(),
                pending_reads: BTreeMap::new(),
                sticky_fatal_error: None,
            }),
            peer_outbound_txs,
            next_read_id: AtomicU64::new(1),
            next_rpc_seq: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
        });

        let rpc_handler: Arc<dyn RpcRequestHandler> = Arc::new(ConsensusRpcAdapter {
            engine: Arc::clone(&engine),
        });
        let accounting = SocketActivityAccounting::new();
        let requested_profile = ClusterProfile::MultiHostCluster {
            local_cell: config.local_cell,
            bind_hint: config.bind_hint,
        };

        let listener = MtlsEndpointListener::bind(
            requested_profile,
            security.clone(),
            config.net_config,
            accounting.clone(),
            Some(Arc::clone(&rpc_handler)),
            None,
        )
        .await
        .map_err(ConsensusError::TransportInit)?;
        let local_addr = listener.local_addr();
        let active_profile = ClusterProfile::MultiHostCluster {
            local_cell: config.local_cell,
            bind_hint: local_addr,
        };

        let transport: Arc<dyn CellRpcTransport> = Arc::new(
            MtlsCellTransport::new_multi_host(
                active_profile,
                resolver,
                security,
                config.net_config,
                accounting.clone(),
                Some(rpc_handler),
                None,
            )
            .map_err(ConsensusError::TransportInit)?,
        );

        let running_listener = listener.spawn_accept_loop();
        let (shutdown_tx, _) = watch::channel(false);
        let mut shutdown_rx_tick = shutdown_tx.subscribe();

        // Dedicated per-remote-peer serial outbound dispatch coroutines with
        // backlog coalescing: peers execute independently in parallel while each
        // individual peer holds at most 1 in-flight RPC at any instant.
        let mut worker_handles_vec = Vec::with_capacity(peer_receivers.len().saturating_add(1));
        for (peer_cell, peer_rx) in peer_receivers {
            worker_handles_vec.push(spawn_peer_outbound_worker(
                peer_cell,
                peer_rx,
                Arc::clone(&engine),
                Arc::clone(&transport),
                shutdown_tx.subscribe(),
            ));
        }

        // Monotonic tick driver (`ConsensusInput::Tick`).
        let tick_engine = Arc::clone(&engine);
        let tick_interval = config.tick_interval;
        let tick_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Consume initial immediate tick.
            interval.tick().await;
            loop {
                tokio::select! {
                    biased;
                    changed = shutdown_rx_tick.changed() => {
                        if changed.is_err() || *shutdown_rx_tick.borrow() {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        let _ = tick_engine.step_and_persist(
                            ConsensusInput::Tick,
                            None,
                            None,
                            None,
                        );
                    }
                }
            }
        });
        worker_handles_vec.push(tick_handle);

        Ok(Self {
            local_cell: config.local_cell,
            local_addr,
            roster: config.roster,
            accounting,
            engine,
            shutdown_tx,
            listener_slot: tokio::sync::Mutex::new(Some(running_listener)),
            worker_handles: tokio::sync::Mutex::new(worker_handles_vec),
        })
    }

    /// Local nominal `CellIdentity`.
    #[must_use]
    pub const fn local_cell(&self) -> CellIdentity {
        self.local_cell
    }

    /// Ephemeral bound listener `SocketAddr`.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Cluster profile (`MultiHostCluster`).
    #[must_use]
    pub const fn cluster_profile(&self) -> ClusterProfile {
        ClusterProfile::MultiHostCluster {
            local_cell: self.local_cell,
            bind_hint: self.local_addr,
        }
    }

    /// Active listener + peer connection socket count tracked by `nlos-net`.
    #[must_use]
    pub fn active_socket_count(&self) -> usize {
        self.accounting
            .bound_listener_count()
            .saturating_add(self.accounting.active_connection_count())
    }

    /// Current Raft role (`Follower`, `PreCandidate`, `Candidate`, `Leader`).
    #[must_use]
    pub fn role(&self) -> ConsensusRole {
        self.engine.lock_state().core.role()
    }

    /// Current durable `current_term`.
    #[must_use]
    pub fn current_term(&self) -> ConsensusTerm {
        self.engine.lock_state().core.current_term()
    }

    /// Known leader in `current_term`, if any.
    #[must_use]
    pub fn known_leader(&self) -> Option<CellIdentity> {
        self.engine.lock_state().core.known_leader()
    }

    /// Current majority-committed `consensus_commit_index`.
    #[must_use]
    pub fn consensus_commit_index(&self) -> LogIndex {
        self.engine.lock_state().core.consensus_commit_index()
    }

    /// Current monotonic `TermBoundLeaderLease`.
    #[must_use]
    pub fn leader_lease(&self) -> TermBoundLeaderLease {
        self.engine.lock_state().core.leader_lease()
    }

    /// Current deterministic `ControlPlaneStateMachine` root digest.
    #[must_use]
    pub fn state_root_digest(&self) -> [u8; 32] {
        self.engine.lock_state().state_machine.state_root_digest()
    }

    /// Number of uncompacted entries in `consensus_log`.
    #[must_use]
    pub fn uncompacted_log_len(&self) -> usize {
        self.engine.lock_state().core.uncompacted_log().len()
    }

    /// Latest installed/created snapshot metadata, if any.
    #[must_use]
    pub fn snapshot_metadata(&self) -> Option<SnapshotMetadata> {
        self.engine.lock_state().core.snapshot_metadata()
    }

    /// Proposes a `[CONS-SCOPE-001]` command on this Leader, resolving ONLY
    /// after `>= floor(N/2) + 1` distinct-domain replicas durably persist the
    /// entry to `SQLite WAL + synchronous=FULL` (`[DUR-ACK-002]`).
    ///
    /// # Errors
    ///
    /// Fails closed with `ProposalRejected(NotLeader | StaleLeaderTerm)` or
    /// `ProposalTimeout`.
    pub async fn propose(
        &self,
        proposal: ControlPlaneProposal,
        timeout_budget: Duration,
    ) -> Result<ConsensusCommitReceipt, ConsensusError> {
        self.propose_fenced(proposal, None, timeout_budget).await
    }

    /// Proposes a `[CONS-SCOPE-001]` command explicitly fenced to
    /// `expected_leader_term` (immediately returning `StaleLeaderTerm` if a
    /// higher term has already superseded `expected_leader_term`).
    ///
    /// # Errors
    ///
    /// Fails closed on term mismatch, step-down, store error, or timeout.
    pub async fn propose_in_term(
        &self,
        proposal: ControlPlaneProposal,
        expected_leader_term: ConsensusTerm,
        timeout_budget: Duration,
    ) -> Result<ConsensusCommitReceipt, ConsensusError> {
        self.propose_fenced(proposal, Some(expected_leader_term), timeout_budget)
            .await
    }

    async fn propose_fenced(
        &self,
        proposal: ControlPlaneProposal,
        expected_leader_term: Option<ConsensusTerm>,
        timeout_budget: Duration,
    ) -> Result<ConsensusCommitReceipt, ConsensusError> {
        let (tx, rx) = oneshot::channel();
        self.engine.step_and_persist(
            ConsensusInput::Propose {
                proposal,
                expected_leader_term,
            },
            None,
            Some(tx),
            None,
        )?;

        match tokio::time::timeout(timeout_budget, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ConsensusError::NodeShutDown),
            Err(_) => Err(ConsensusError::ProposalTimeout),
        }
    }

    /// Decodes a wire proposal frame under `[CONS-SCOPE-001]` (rejecting
    /// `Task` / `Fiber` / `Topic` / `SemanticEvent` / `Artifact` payloads with
    /// `RejectDataPlanePayload` before touching `SQLite` or the network) and
    /// replicates it across the quorum.
    ///
    /// # Errors
    ///
    /// Returns [`ConsensusError::ScopeViolation`] on forbidden data-plane or
    /// malformed payloads.
    pub async fn propose_wire(
        &self,
        wire_frame: &[u8],
        timeout_budget: Duration,
    ) -> Result<ConsensusCommitReceipt, ConsensusError> {
        let proposal = ControlPlaneProposal::decode_wire(wire_frame)?;
        self.propose(proposal, timeout_budget).await
    }

    /// Executes a `ReadIndex` quorum-heartbeat-fenced linearizable read on this
    /// Leader (optionally pinned to `expected_leader_term`), invoking `reader`
    /// only after a live majority of distinct `ControlDomainId` voters confirm
    /// leadership in `current_term`.
    ///
    /// # Errors
    ///
    /// Fails closed with `StaleLeaderTerm`, `NotLeader`, or `ProposalTimeout`.
    pub async fn linearizable_read<R>(
        &self,
        expected_leader_term: Option<ConsensusTerm>,
        timeout_budget: Duration,
        reader: impl FnOnce(&ControlPlaneStateMachine, LogIndex, ConsensusTerm) -> R,
    ) -> Result<R, ConsensusError> {
        let read_id = self.engine.next_read_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.engine.step_and_persist(
            ConsensusInput::RequestLinearizableRead {
                read_id,
                expected_leader_term,
            },
            None,
            None,
            Some((read_id, tx)),
        )?;

        let (confirmed_commit, confirmed_term) =
            match tokio::time::timeout(timeout_budget, rx).await {
                Ok(Ok(res)) => res?,
                Ok(Err(_)) => {
                    self.engine.lock_state().pending_reads.remove(&read_id);
                    return Err(ConsensusError::NodeShutDown);
                }
                Err(_) => {
                    self.engine.lock_state().pending_reads.remove(&read_id);
                    return Err(ConsensusError::ProposalTimeout);
                }
            };

        let guard = self.engine.lock_state();
        Ok(reader(
            &guard.state_machine,
            confirmed_commit,
            confirmed_term,
        ))
    }

    /// Inspects the locally applied [`ControlPlaneStateMachine`] on this
    /// replica (useful for verifying follower convergence).
    pub fn inspect_local_state<R>(
        &self,
        reader: impl FnOnce(&ControlPlaneStateMachine, LogIndex, ConsensusTerm) -> R,
    ) -> R {
        let guard = self.engine.lock_state();
        reader(
            &guard.state_machine,
            guard.core.consensus_commit_index(),
            guard.core.current_term(),
        )
    }

    /// Creates a durable `[DUR-SNAP-001]` snapshot at `last_applied_index` and
    /// compacts covered `consensus_log` entries so lagging followers catch up
    /// via `InstallSnapshotChunk`.
    ///
    /// # Errors
    ///
    /// Fails closed if `last_applied_index == 0` or snapshot persistence fails.
    pub fn create_snapshot_and_compact(&self) -> Result<SnapshotMetadata, ConsensusError> {
        let mut guard = self.engine.lock_state();
        let included_index = guard.state_machine.last_applied_index();
        let included_term = guard.state_machine.last_applied_term();
        let bytes = guard.state_machine.encode_snapshot();
        let meta = guard
            .store
            .create_snapshot(included_index, included_term, &bytes)?;
        guard.store.compact_log_up_to(included_index)?;
        guard
            .core
            .record_local_snapshot(meta, bytes, included_index);
        Ok(meta)
    }

    /// Gracefully stops background tick/dispatch tasks and closes the mTLS
    /// listener + accepted sessions.
    pub async fn shutdown(&self) {
        self.engine.shutting_down.store(true, Ordering::Release);
        let _ = self.shutdown_tx.send(true);
        if let Some(listener) = self.listener_slot.lock().await.take() {
            listener.shutdown().await;
        }
        let handles = std::mem::take(&mut *self.worker_handles.lock().await);
        for h in handles {
            let _ = h.await;
        }
        let mut guard = self.engine.lock_state();
        for (_, w) in std::mem::take(&mut guard.pending_proposals) {
            let _ = w.tx.send(Err(ConsensusError::NodeShutDown));
        }
        for (_, r) in std::mem::take(&mut guard.pending_reads) {
            let _ = r.tx.send(Err(ConsensusError::NodeShutDown));
        }
    }
}

impl Drop for MultiHostQuorumNode {
    fn drop(&mut self) {
        self.engine.shutting_down.store(true, Ordering::Release);
        let _ = self.shutdown_tx.send(true);
    }
}

/// Top-level unified control-plane consensus handle supporting both zero-config
/// `N = 1` `SingleHostDesktop` passthrough and `N >= 3` `MultiHostCluster`
/// mTLS quorum (`ADR-0026` §6.3).
#[derive(Debug)]
pub enum ConsensusNode {
    /// `N = 1` single-host desktop passthrough (`0` sockets, `0` timers, `0` TLS).
    SingleHost(Box<crate::single_node::SingleHostPassthroughNode>),
    /// `N >= 3` multi-host deterministic Raft quorum over `nlos-net` mTLS.
    MultiHost(Box<MultiHostQuorumNode>),
}

impl ConsensusNode {
    /// Opens an `N = 1` zero-socket `SingleHostDesktop` passthrough node.
    ///
    /// # Errors
    ///
    /// Fails closed on `SQLite` durability or `[DUR-FAILCLOSED-001]` errors.
    pub fn open_single_host_passthrough(
        store_dir: impl AsRef<Path>,
        local_cell: CellIdentity,
        local_domain: nlos_types::ControlDomainId,
    ) -> Result<Self, ConsensusError> {
        Ok(Self::SingleHost(Box::new(
            crate::single_node::SingleHostPassthroughNode::open(
                store_dir,
                local_cell,
                local_domain,
            )?,
        )))
    }

    /// Binds and starts an `N >= 3` `MultiHostCluster` mTLS Raft quorum node.
    ///
    /// # Errors
    ///
    /// Fails closed on configuration, durability, integrity, or TLS/bind errors.
    pub async fn open_multi_host_quorum(
        store_dir: impl AsRef<Path>,
        config: MultiHostQuorumConfig,
        resolver: Arc<dyn EndpointResolver>,
        security: NodeSecurityDeps,
    ) -> Result<Self, ConsensusError> {
        Ok(Self::MultiHost(Box::new(
            MultiHostQuorumNode::bind_and_start(store_dir, config, resolver, security).await?,
        )))
    }

    /// Deployment [`ClusterProfile`].
    #[must_use]
    pub const fn cluster_profile(&self) -> ClusterProfile {
        match self {
            Self::SingleHost(n) => n.cluster_profile(),
            Self::MultiHost(n) => n.cluster_profile(),
        }
    }

    /// Active open listener + peer socket count (`0` in `SingleHost`).
    #[must_use]
    pub fn active_socket_count(&self) -> usize {
        match self {
            Self::SingleHost(n) => n.active_socket_count(),
            Self::MultiHost(n) => n.active_socket_count(),
        }
    }

    /// Proposes a `[CONS-SCOPE-001]` command across either `N = 1` passthrough
    /// or `N >= 3` quorum.
    ///
    /// # Errors
    ///
    /// Fails closed on leadership rejection, durability failure, or timeout.
    pub async fn propose(
        &self,
        proposal: ControlPlaneProposal,
        timeout_budget: Duration,
    ) -> Result<ConsensusCommitReceipt, ConsensusError> {
        match self {
            Self::SingleHost(n) => n.propose(proposal),
            Self::MultiHost(n) => n.propose(proposal, timeout_budget).await,
        }
    }

    /// Decodes and proposes a wire frame under `[CONS-SCOPE-001]`.
    ///
    /// # Errors
    ///
    /// Fails closed on `RejectDataPlanePayload` or proposal failure.
    pub async fn propose_wire(
        &self,
        wire_frame: &[u8],
        timeout_budget: Duration,
    ) -> Result<ConsensusCommitReceipt, ConsensusError> {
        match self {
            Self::SingleHost(n) => n.propose_wire(wire_frame),
            Self::MultiHost(n) => n.propose_wire(wire_frame, timeout_budget).await,
        }
    }

    /// Gracefully shuts down background network/timer resources (`no-op` in `N = 1`).
    pub async fn shutdown(&self) {
        if let Self::MultiHost(n) = self {
            n.shutdown().await;
        }
    }
}

/// Top-level error type for [`SingleHostPassthroughNode`](crate::single_node::SingleHostPassthroughNode),
/// [`MultiHostQuorumNode`], and [`ConsensusNode`].
#[derive(Debug)]
pub enum ConsensusError {
    /// Invalid voter roster or timing bounds (`[TASK-SCALE-002]`).
    Config(crate::core::ConsensusConfigError),
    /// Forbidden data-plane or malformed wire proposal (`[CONS-SCOPE-001]`).
    ScopeViolation(ScopeViolationError),
    /// Leadership or stale-term rejection (`StaleLeaderTerm` / `NotLeader`).
    ProposalRejected(ProposalRejectReason),
    /// `SQLite WAL + FULL` durability or `[DUR-FAILCLOSED-001]` corruption error.
    Store(crate::store::ConsensusStoreError),
    /// `nlos-net` TLS 1.3 / TCP listener initialization failure.
    TransportInit(NetInitError),
    /// Proposal or `ReadIndex` timed out before achieving a durable majority.
    ProposalTimeout,
    /// Node encountered a sticky store/integrity failure and halted fail-closed.
    DriverFatal(String),
    /// Node was shut down while proposal/read was in flight.
    NodeShutDown,
}

impl fmt::Display for ConsensusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(e) => write!(formatter, "{e}"),
            Self::ScopeViolation(e) => write!(formatter, "{e}"),
            Self::ProposalRejected(r) => write!(formatter, "consensus proposal rejected: {r:?}"),
            Self::Store(e) => write!(formatter, "{e}"),
            Self::TransportInit(e) => write!(formatter, "consensus transport init failed: {e}"),
            Self::ProposalTimeout => {
                formatter.write_str("consensus proposal timed out awaiting durable majority")
            }
            Self::DriverFatal(msg) => {
                write!(formatter, "consensus driver halted fail-closed: {msg}")
            }
            Self::NodeShutDown => formatter.write_str("consensus node shut down"),
        }
    }
}

impl Error for ConsensusError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(e) => Some(e),
            Self::ScopeViolation(e) => Some(e),
            Self::Store(e) => Some(e),
            Self::TransportInit(e) => Some(e),
            Self::ProposalRejected(_)
            | Self::ProposalTimeout
            | Self::DriverFatal(_)
            | Self::NodeShutDown => None,
        }
    }
}

impl From<crate::core::ConsensusConfigError> for ConsensusError {
    fn from(value: crate::core::ConsensusConfigError) -> Self {
        Self::Config(value)
    }
}

impl From<ScopeViolationError> for ConsensusError {
    fn from(value: ScopeViolationError) -> Self {
        Self::ScopeViolation(value)
    }
}

impl From<crate::store::ConsensusStoreError> for ConsensusError {
    fn from(value: crate::store::ConsensusStoreError) -> Self {
        Self::Store(value)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cell(tag: u8) -> CellIdentity {
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([tag; 16]))
    }

    #[test]
    fn coalesce_outbound_peer_batch_folds_stale_terms_heartbeats_and_duplicates() {
        let leader = sample_cell(1);
        let stale_hb = RaftMessage::AppendEntries {
            term: 1,
            leader_id: leader,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: Vec::new(),
            leader_commit: 0,
            quorum_round_id: 1,
        };
        let stale_prevote = RaftMessage::RequestPreVote {
            prospective_term: 1,
            candidate_id: leader,
            last_log_index: 0,
            last_log_term: 0,
        };
        let read_fence_hb = RaftMessage::AppendEntries {
            term: 2,
            leader_id: leader,
            prev_log_index: 3,
            prev_log_term: 2,
            entries: Vec::new(),
            leader_commit: 3,
            quorum_round_id: 10,
        };
        let superseded_hb_a = RaftMessage::AppendEntries {
            term: 2,
            leader_id: leader,
            prev_log_index: 3,
            prev_log_term: 2,
            entries: Vec::new(),
            leader_commit: 3,
            quorum_round_id: 11,
        };
        let superseded_hb_b = RaftMessage::AppendEntries {
            term: 2,
            leader_id: leader,
            prev_log_index: 3,
            prev_log_term: 2,
            entries: Vec::new(),
            leader_commit: 3,
            quorum_round_id: 12,
        };
        let snap_chunk_0 = RaftMessage::InstallSnapshotChunk {
            term: 2,
            leader_id: leader,
            last_included_index: 6,
            last_included_term: 2,
            state_root_digest: [0xAA; 32],
            snapshot_sha256: [0xBB; 32],
            total_bytes: 512,
            offset: 0,
            chunk_data: vec![0x11; 192],
            done: false,
        };

        let raw = vec![
            stale_hb,
            stale_prevote,
            read_fence_hb.clone(),
            superseded_hb_a,
            snap_chunk_0.clone(),
            superseded_hb_b,
            snap_chunk_0.clone(),
        ];

        let folded = coalesce_outbound_peer_batch(raw, 2, &[10]);
        assert_eq!(folded, vec![read_fence_hb, snap_chunk_0]);
    }
}
