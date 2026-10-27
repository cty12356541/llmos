//! Embedded deterministic Raft control-plane quorum log, `SQLite WAL+FULL`
//! durability engine, strict `[CONS-SCOPE-001]` 4-family scope guard, and
//! zero-config `N = 1` single-host passthrough mode (`ADR-0026` `C-CONSENSUS-CP`).
//!
//! # Architectural Invariants Enforced
//!
//! 1. **`[CONS-SCOPE-001]` Low-Frequency Control-Plane Scope Fence**:
//!    Only [`ControlPlaneCommand::CellDirectoryMutation`],
//!    [`ControlPlaneCommand::RootLeasePoolMutation`],
//!    [`ControlPlaneCommand::TaskAuthorityShardMutation`], and
//!    [`ControlPlaneCommand::ControlPlaneAuditAnchor`] may enter
//!    `nlos-consensus`. High-frequency data-plane `Task` / `Fiber` / `Topic` /
//!    `SemanticEvent` / `Artifact` payloads are rejected at both compile time
//!    ([`ControlPlaneScopePermitted`]) and wire decoding
//!    ([`RejectDataPlanePayload`]).
//! 2. **`[TASK-SCALE-002]` Independent Failure-Domain Quorum**:
//!    Every voter in an `N >= 3` [`ValidatedVoterRoster`] MUST bind a distinct
//!    `ControlDomainId` and `CellIdentity`; shared failure domains fail closed
//!    with [`ConsensusConfigError::NonIndependentControlDomain`].
//! 3. **`[DUR-ACK-002]` Majority Durable Commitment**:
//!    [`ConsensusCommitReceipt`] is emitted ONLY after `>= floor(N / 2) + 1`
//!    independent voter replicas have committed the entry to
//!    `SQLite WAL + synchronous=FULL`.
//! 4. **`[DUR-SNAP-001]` & `[DUR-FAILCLOSED-001]`**:
//!    Crash-safe atomic snapshot creation, `InstallSnapshot` catch-up, and
//!    bounded log compaction paired with `SHA-256` + `PRAGMA quick_check`
//!    verification that fails closed on any silent disk/WAL corruption.
//! 5. **`[NLOS-GOAL-001]` / `ADR-0026` §6.3 (`N = 1` Zero-Overhead Default)**:
//!    [`SingleHostPassthroughNode`] (`ClusterProfile::SingleHostDesktop`) binds
//!    **0** sockets, spawns **0** background timers, requires **0** TLS
//!    certificates, and commits in 1 local `SQLite WAL + FULL` transaction.

pub mod core;
pub mod net_driver;
pub mod single_node;
pub mod state_machine;
pub mod store;

pub use core::{
    ConsensusConfigError, ConsensusCore, ConsensusEffect, ConsensusInput, ConsensusRole,
    ConsensusTerm, ConsensusTimingConfig, DEFAULT_SNAPSHOT_CHUNK_BYTES, LogIndex,
    ProposalRejectReason, RaftLogEntry, RaftMessage, RehydratedCoreState, SnapshotMetadata,
    TermBoundLeaderLease, ValidatedVoterRoster, VoterReplica, compute_log_entry_sha256,
    compute_snapshot_transfer_sha256,
};
pub use net_driver::{
    ConsensusError, ConsensusNode, MultiHostQuorumConfig, MultiHostQuorumNode, RaftWireCodecError,
    decode_raft_message, encode_raft_message, staggered_timing_for_roster,
};
pub use single_node::SingleHostPassthroughNode;
pub use state_machine::{
    AppliedProposalOutcome, CellDirectoryCommand, CommandFamilyCode,
    CommittedTaskAuthorityAssignment, CommittedTaskAuthorityTakeover, ControlPlaneAuditCommand,
    ControlPlaneCommand, ControlPlaneDomainRejection, ControlPlaneProposal,
    ControlPlaneScopePermitted, ControlPlaneStateMachine, ForbiddenDataPlaneKind,
    RejectDataPlanePayload, ReplicatedCapacityLeaseEntry, ReplicatedCellDirectoryEntry,
    ReplicatedCheckpointAnchor, ReplicatedDeviceHeadEntry, ReplicatedDeviceHeadState,
    ReplicatedMigrationAnchor, ReplicatedQuotaLeaseDisposition, ReplicatedQuotaLeaseEntry,
    RootLeasePoolCommand, ScopeViolationError, SnapshotCodecError, TaskAuthorityShardCommand,
    encode_forbidden_data_plane_wire_probe,
};
pub use store::{
    ConsensusCommitReceipt, ConsensusStoreError, RecoveredConsensusState, SqliteConsensusStore,
};
