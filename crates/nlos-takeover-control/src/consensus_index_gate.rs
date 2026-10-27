//! Transport-neutral majority-commit `consensus_commit_index` verification gate
//! binding `TaskAuthorityAssignment` and `TaskAuthorityTakeoverReceipt` to
//! `nlos-consensus` quorum proofs (`S-A` `N >= 3`) or single-host passthrough
//! proofs (`S-B` `N = 1`) (`ADR-0026` §2.1 #3, §3.3 `C-CONSENSUS-CP`, §6.3,
//! `[DUR-ACK-002]`, `[TASK-SCALE-002]`).
//!
//! # Security & Monotonicity Invariants
//!
//! 1. **Cryptographic Quorum Attestation (`S-A` vs `S-B`)**:
//!    Every voter attestation is verified through
//!    [`IdentityAuthority::verify_semantic_authority_signature`] against the
//!    domain-separated `SHA-256` digest of
//!    `(consensus_commit_index, consensus_term, state_root_digest, subject)`.
//!    In `MultiHostQuorum` (`N >= 3`), at least `floor(N / 2) + 1` distinct
//!    roster voters in **distinct** [`ControlDomainId`]s (`[TASK-SCALE-002]`)
//!    must sign the exact commit tuple. In `SingleHostPassthrough` (`N = 1`),
//!    the single local authority principal signs (`quorum = 1`).
//! 2. **Authentic `consensus_commit_index` Binding**:
//!    `TaskAuthorityTakeoverReceipt.consensus_commit_index` MUST be `Some(idx)`
//!    with `idx >= 1` and byte-equal to the quorum-attested
//!    `consensus_commit_index`. `None`, `Some(0)`, or mismatched indices fail
//!    closed before any authority activation.
//! 3. **Monotonic Progression & Exact Idempotent Replay**:
//!    Per `(TaskId, Generation)`, admitted commits strictly advance
//!    `consensus_commit_index` and `authority_term` while `consensus_term` is
//!    monotonically non-decreasing. Byte-identical replays at the latest
//!    admitted `consensus_commit_index` return
//!    [`AdmittedCommitDisposition::IdempotentReplay`]; divergent payloads at an
//!    existing index fail closed.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;

use nlos_identity::{
    Ed25519Signature, IdentityAuthority, IdentityAuthorityError,
    VerifySemanticAuthoritySignatureRequest,
};
use nlos_task::{
    AuthorityAssignmentState, AuthorityTakeoverReceiptState, TaskAuthorityAssignment,
    TaskAuthorityTakeoverReceipt,
};
use nlos_types::{
    ControlDomainId, Generation, KeyId, PrincipalId, ReceiptId, TaskAuthorityAssignmentId, TaskId,
};

/// Domain separator for [`quorum_commit_attestation_digest`].
pub const QUORUM_COMMIT_ATTESTATION_DOMAIN: &[u8] = b"llmos/consensus-quorum-commit-attestation/v1";

/// Control-plane consensus topology backing [`CommittedControlIndexGate`]
/// (`ADR-0026` §6.3 `S-A` vs `S-B`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsensusBackingMode {
    /// `S-B` (`N = 1` `SingleHostDesktop` passthrough): local authority commits
    /// in a single `SQLite WAL + FULL` transaction (`quorum_size == 1`).
    SingleHostPassthrough {
        /// Local control-plane authority `PrincipalId`.
        local_principal: PrincipalId,
        /// Local failure/control domain.
        local_domain: ControlDomainId,
    },
    /// `S-A` (`N >= 3` `MultiHostCluster` Raft quorum): requires
    /// `>= floor(N / 2) + 1` distinct roster voters across independent
    /// `ControlDomainId`s (`[TASK-SCALE-002]`).
    MultiHostQuorum {
        /// Validated map from voter `PrincipalId` to its unique `ControlDomainId`.
        voter_roster: BTreeMap<PrincipalId, ControlDomainId>,
    },
}

impl ConsensusBackingMode {
    /// Constructs a `SingleHostPassthrough` (`N = 1`) backing mode.
    #[must_use]
    pub const fn single_host_passthrough(
        local_principal: PrincipalId,
        local_domain: ControlDomainId,
    ) -> Self {
        Self::SingleHostPassthrough {
            local_principal,
            local_domain,
        }
    }

    /// Validates and constructs an `N >= 3` `MultiHostQuorum` backing mode
    /// enforcing `[TASK-SCALE-002]` failure-domain independence.
    ///
    /// # Errors
    ///
    /// Fails closed if `voters.len() < 3`, if a `PrincipalId` is duplicated,
    /// or if two voters share the same `ControlDomainId`.
    pub fn multi_host_quorum(
        voters: &[(PrincipalId, ControlDomainId)],
    ) -> Result<Self, ConsensusIndexGateError> {
        if voters.len() < 3 {
            return Err(ConsensusIndexGateError::InvalidVoterRosterSize(
                voters.len(),
            ));
        }
        let mut roster = BTreeMap::new();
        let mut seen_domains = BTreeSet::new();
        for &(principal, domain) in voters {
            if roster.insert(principal, domain).is_some() {
                return Err(ConsensusIndexGateError::DuplicateVoterPrincipal(principal));
            }
            if !seen_domains.insert(domain) {
                return Err(ConsensusIndexGateError::NonIndependentControlDomain(domain));
            }
        }
        Ok(Self::MultiHostQuorum {
            voter_roster: roster,
        })
    }

    /// Total configured voter count (`N`).
    #[must_use]
    pub fn cluster_size(&self) -> usize {
        match self {
            Self::SingleHostPassthrough { .. } => 1,
            Self::MultiHostQuorum { voter_roster } => voter_roster.len(),
        }
    }

    /// Strict majority quorum size `floor(N / 2) + 1`.
    #[must_use]
    pub fn required_quorum(&self) -> usize {
        (self.cluster_size() / 2) + 1
    }
}

/// Canonical control-plane authority record covered by a
/// [`QuorumCommitAttestation`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlCommitSubject {
    /// Initial or active `TaskAuthorityAssignment` (§26.1).
    TaskAuthorityAssignment {
        /// Assignment identifier.
        assignment_id: TaskAuthorityAssignmentId,
        /// Subject `TaskId`.
        task_id: TaskId,
        /// Subject `Task` generation.
        task_generation: Generation,
        /// Non-zero `authority_term`.
        authority_term: NonZeroU64,
        /// Paired control epoch.
        control_epoch: u64,
        /// Participant-registry generation.
        participant_registry_generation: u64,
        /// Participant-registry digest root.
        participant_registry_root: [u8; 32],
    },
    /// Completed `TaskAuthorityTakeoverReceipt` (§26.1, `[DIST-TASK-002]`).
    TaskAuthorityTakeoverReceipt {
        /// Takeover receipt identifier.
        takeover_receipt_id: ReceiptId,
        /// Subject `TaskId`.
        task_id: TaskId,
        /// Subject `Task` generation.
        task_generation: Generation,
        /// Fenced predecessor assignment id.
        old_assignment_id: TaskAuthorityAssignmentId,
        /// Activated successor assignment id.
        new_assignment_id: TaskAuthorityAssignmentId,
        /// Frozen predecessor `authority_term`.
        frozen_old_authority_term: NonZeroU64,
        /// Strictly greater successor `authority_term`.
        new_authority_term: NonZeroU64,
        /// Successor control epoch.
        new_control_epoch: u64,
        /// Exact fenced participant set root.
        exact_fence_set_root: [u8; 32],
        /// Optional quarantined effect root.
        outstanding_effect_quarantine_root: Option<[u8; 32]>,
    },
}

impl ControlCommitSubject {
    /// Extracts a [`ControlCommitSubject`] from an active
    /// [`TaskAuthorityAssignment`].
    ///
    /// # Errors
    ///
    /// Fails closed if `assignment.state != Active` or `term == 0`.
    pub fn from_assignment(
        assignment: &TaskAuthorityAssignment,
    ) -> Result<Self, ConsensusIndexGateError> {
        if assignment.state != AuthorityAssignmentState::Active {
            return Err(ConsensusIndexGateError::AssignmentNotActive(
                assignment.state,
            ));
        }
        let authority_term = NonZeroU64::new(assignment.authority_lease_binding.term)
            .ok_or(ConsensusIndexGateError::ZeroAuthorityTerm)?;
        Ok(Self::TaskAuthorityAssignment {
            assignment_id: assignment.assignment_id,
            task_id: assignment.task_id,
            task_generation: assignment.task_generation,
            authority_term,
            control_epoch: assignment.control_epoch,
            participant_registry_generation: assignment.participant_registry_binding.generation,
            participant_registry_root: assignment.participant_registry_binding.root,
        })
    }

    /// Extracts a [`ControlCommitSubject`] from a completed
    /// [`TaskAuthorityTakeoverReceipt`].
    ///
    /// # Errors
    ///
    /// Fails closed if `barrier_state != Complete`, if `new_assignment_id` or
    /// `exact_fence_set_root` is missing, or if `new_authority_term <= frozen_old_authority_term`.
    pub fn from_takeover_receipt(
        receipt: &TaskAuthorityTakeoverReceipt,
    ) -> Result<Self, ConsensusIndexGateError> {
        if receipt.barrier_state != AuthorityTakeoverReceiptState::Complete {
            return Err(ConsensusIndexGateError::TakeoverReceiptNotComplete);
        }
        let new_assignment_id = receipt
            .new_assignment_id
            .ok_or(ConsensusIndexGateError::MissingSuccessorAssignmentId)?;
        let exact_fence_set_root = receipt
            .exact_fence_set_root
            .ok_or(ConsensusIndexGateError::MissingExactFenceSetRoot)?;
        let frozen_old_authority_term = NonZeroU64::new(receipt.frozen_old_authority_term)
            .ok_or(ConsensusIndexGateError::ZeroAuthorityTerm)?;
        let new_authority_term = NonZeroU64::new(receipt.new_authority_lease_binding.term)
            .ok_or(ConsensusIndexGateError::ZeroAuthorityTerm)?;
        if new_authority_term <= frozen_old_authority_term {
            return Err(ConsensusIndexGateError::NonMonotonicAuthorityTerm {
                previous_term: frozen_old_authority_term,
                attempted_term: new_authority_term,
            });
        }
        Ok(Self::TaskAuthorityTakeoverReceipt {
            takeover_receipt_id: receipt.receipt_id,
            task_id: receipt.task_id,
            task_generation: receipt.task_generation,
            old_assignment_id: receipt.old_assignment_id,
            new_assignment_id,
            frozen_old_authority_term,
            new_authority_term,
            new_control_epoch: receipt.new_control_epoch,
            exact_fence_set_root,
            outstanding_effect_quarantine_root: receipt.outstanding_effect_quarantine_root,
        })
    }

    const fn task_scope(&self) -> (TaskId, Generation) {
        match *self {
            Self::TaskAuthorityAssignment {
                task_id,
                task_generation,
                ..
            }
            | Self::TaskAuthorityTakeoverReceipt {
                task_id,
                task_generation,
                ..
            } => (task_id, task_generation),
        }
    }

    const fn effective_authority_term(&self) -> NonZeroU64 {
        match *self {
            Self::TaskAuthorityAssignment { authority_term, .. } => authority_term,
            Self::TaskAuthorityTakeoverReceipt {
                new_authority_term, ..
            } => new_authority_term,
        }
    }

    fn append_canonical_bytes(&self, out: &mut Vec<u8>) {
        match *self {
            Self::TaskAuthorityAssignment {
                assignment_id,
                task_id,
                task_generation,
                authority_term,
                control_epoch,
                participant_registry_generation,
                participant_registry_root,
            } => {
                out.push(1);
                out.extend_from_slice(assignment_id.as_bytes());
                out.extend_from_slice(task_id.as_bytes());
                out.extend_from_slice(&task_generation.get().to_be_bytes());
                out.extend_from_slice(&authority_term.get().to_be_bytes());
                out.extend_from_slice(&control_epoch.to_be_bytes());
                out.extend_from_slice(&participant_registry_generation.to_be_bytes());
                out.extend_from_slice(&participant_registry_root);
            }
            Self::TaskAuthorityTakeoverReceipt {
                takeover_receipt_id,
                task_id,
                task_generation,
                old_assignment_id,
                new_assignment_id,
                frozen_old_authority_term,
                new_authority_term,
                new_control_epoch,
                exact_fence_set_root,
                outstanding_effect_quarantine_root,
            } => {
                out.push(2);
                out.extend_from_slice(takeover_receipt_id.as_bytes());
                out.extend_from_slice(task_id.as_bytes());
                out.extend_from_slice(&task_generation.get().to_be_bytes());
                out.extend_from_slice(old_assignment_id.as_bytes());
                out.extend_from_slice(new_assignment_id.as_bytes());
                out.extend_from_slice(&frozen_old_authority_term.get().to_be_bytes());
                out.extend_from_slice(&new_authority_term.get().to_be_bytes());
                out.extend_from_slice(&new_control_epoch.to_be_bytes());
                out.extend_from_slice(&exact_fence_set_root);
                match outstanding_effect_quarantine_root {
                    Some(root) => {
                        out.push(1);
                        out.extend_from_slice(&root);
                    }
                    None => out.push(0),
                }
            }
        }
    }
}

/// Computes the canonical 32-byte `SHA-256` message digest signed by each
/// consensus voter (`[DUR-ACK-002]`).
#[must_use]
pub fn quorum_commit_attestation_digest(
    consensus_commit_index: NonZeroU64,
    consensus_term: NonZeroU64,
    state_root_digest: &[u8; 32],
    subject: &ControlCommitSubject,
) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(224);
    preimage.extend_from_slice(QUORUM_COMMIT_ATTESTATION_DOMAIN);
    preimage.extend_from_slice(&consensus_commit_index.get().to_be_bytes());
    preimage.extend_from_slice(&consensus_term.get().to_be_bytes());
    preimage.extend_from_slice(state_root_digest);
    subject.append_canonical_bytes(&mut preimage);
    compute_sha256(&preimage)
}

/// Single voter's Ed25519 signature over [`quorum_commit_attestation_digest`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoterCommitAttestation {
    /// Voter's `PrincipalId`.
    pub voter_principal: PrincipalId,
    /// Voter's independent failure/control domain (`[TASK-SCALE-002]`).
    pub voter_control_domain: ControlDomainId,
    /// Active `KeyPurpose::SemanticSigning` key in `IdentityAuthority`.
    pub voter_key_id: KeyId,
    /// Ed25519 signature over [`quorum_commit_attestation_digest`].
    pub signature: Ed25519Signature,
}

/// Quorum commit bundle proving durable commitment of a control-plane authority
/// record at `(consensus_commit_index, consensus_term, state_root_digest)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuorumCommitAttestation {
    /// Majority-committed 1-based Raft log index (`[DUR-ACK-002]`).
    pub consensus_commit_index: NonZeroU64,
    /// Election term at commitment (`>= 1`).
    pub consensus_term: NonZeroU64,
    /// Deterministic `ControlPlaneStateMachine` root digest at commitment.
    pub state_root_digest: [u8; 32],
    /// Monotonic verification timestamp (`ms`) checked against key validity.
    pub verified_at_ms: u64,
    /// Voter Ed25519 attestations (`1` in `S-B`, `>= floor(N/2)+1` in `S-A`).
    pub voter_attestations: Vec<VoterCommitAttestation>,
}

/// Disposition of an admitted control-plane commit proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmittedCommitDisposition {
    /// Fresh commit advancing `(consensus_commit_index, authority_term)`.
    Advanced,
    /// Byte-identical replay of the latest admitted commit for `(TaskId, Generation)`.
    IdempotentReplay,
}

/// Machine-verifiable proof emitted when [`CommittedControlIndexGate`] admits
/// an assignment or takeover receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedControlCommitProof {
    /// Verified majority `consensus_commit_index`.
    pub consensus_commit_index: NonZeroU64,
    /// Verified `consensus_term`.
    pub consensus_term: NonZeroU64,
    /// Verified `state_root_digest`.
    pub state_root_digest: [u8; 32],
    /// Verified `authority_term`.
    pub authority_term: NonZeroU64,
    /// Distinct `IdentityAuthority`-verified `(PrincipalId, ControlDomainId)` signers.
    pub verified_voter_domains: Vec<(PrincipalId, ControlDomainId)>,
    /// Whether this admission advanced high-water or replayed identically.
    pub disposition: AdmittedCommitDisposition,
}

/// Fail-closed rejection reasons returned by [`CommittedControlIndexGate`].
#[derive(Debug)]
pub enum ConsensusIndexGateError {
    /// `MultiHostQuorum` requires `N >= 3` voters.
    InvalidVoterRosterSize(usize),
    /// Duplicate `PrincipalId` in voter roster or attestation bundle.
    DuplicateVoterPrincipal(PrincipalId),
    /// Two voters share the same `ControlDomainId` (`[TASK-SCALE-002]`).
    NonIndependentControlDomain(ControlDomainId),
    /// Voter principal is not in the configured backing roster.
    UnauthorizedVoter {
        /// Presented `PrincipalId`.
        principal: PrincipalId,
        /// Presented `ControlDomainId`.
        domain: ControlDomainId,
    },
    /// `IdentityAuthority` rejected a voter's Ed25519 signature or key state.
    VoterIdentityRejected {
        /// Voter whose signature failed verification.
        voter: PrincipalId,
        /// Underlying `IdentityAuthority` error.
        error: IdentityAuthorityError,
    },
    /// Fewer than `floor(N / 2) + 1` distinct-domain voters attested commitment.
    InsufficientQuorum {
        /// Verified independent-domain voter count.
        verified_count: usize,
        /// Required majority quorum (`floor(N / 2) + 1`).
        required_quorum: usize,
    },
    /// `TaskAuthorityAssignment` was not in `Active` state.
    AssignmentNotActive(AuthorityAssignmentState),
    /// `TaskAuthorityTakeoverReceipt` was still `Pending`.
    TakeoverReceiptNotComplete,
    /// Completed takeover receipt lacked `new_assignment_id`.
    MissingSuccessorAssignmentId,
    /// Completed takeover receipt lacked `exact_fence_set_root`.
    MissingExactFenceSetRoot,
    /// `authority_term` was zero.
    ZeroAuthorityTerm,
    /// `TaskAuthorityTakeoverReceipt.consensus_commit_index` was `None` or `Some(0)`.
    MissingReceiptConsensusCommitIndex,
    /// Record's `consensus_commit_index` did not match the quorum attestation.
    ConsensusCommitIndexMismatch {
        /// Index carried on the assignment/receipt expectation.
        record_index: u64,
        /// Index attested by the consensus quorum.
        attested_index: NonZeroU64,
    },
    /// Supplied `consensus_commit_index` regressed below the admitted high-water mark.
    StaleConsensusCommitIndex {
        /// Highest admitted `consensus_commit_index` for `(TaskId, Generation)`.
        high_water_index: NonZeroU64,
        /// Regressing `consensus_commit_index`.
        attempted_index: NonZeroU64,
    },
    /// Divergent commit payload presented at an already-admitted `consensus_commit_index`.
    ConflictingCommitAtSameIndex {
        /// Conflicting `consensus_commit_index`.
        commit_index: NonZeroU64,
    },
    /// Supplied `consensus_term` regressed below the admitted high-water term.
    StaleConsensusTerm {
        /// Highest admitted `consensus_term`.
        high_water_term: NonZeroU64,
        /// Regressing `consensus_term`.
        attempted_term: NonZeroU64,
    },
    /// Successor `authority_term` did not strictly advance beyond predecessor.
    NonMonotonicAuthorityTerm {
        /// Previously admitted or frozen `authority_term`.
        previous_term: NonZeroU64,
        /// Non-advancing `authority_term`.
        attempted_term: NonZeroU64,
    },
}

impl fmt::Display for ConsensusIndexGateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVoterRosterSize(n) => {
                write!(
                    formatter,
                    "multi-host voter roster requires N >= 3, got {n}"
                )
            }
            Self::DuplicateVoterPrincipal(p) => {
                write!(formatter, "duplicate voter principal {p:?}")
            }
            Self::NonIndependentControlDomain(d) => write!(
                formatter,
                "[TASK-SCALE-002] duplicate voter ControlDomainId {d:?}"
            ),
            Self::UnauthorizedVoter { principal, domain } => write!(
                formatter,
                "voter ({principal:?}, {domain:?}) is not in configured consensus roster"
            ),
            Self::VoterIdentityRejected { voter, error } => {
                write!(formatter, "voter {voter:?} signature rejected: {error}")
            }
            Self::InsufficientQuorum {
                verified_count,
                required_quorum,
            } => write!(
                formatter,
                "[DUR-ACK-002] insufficient quorum: verified {verified_count} < required {required_quorum}"
            ),
            Self::AssignmentNotActive(state) => {
                write!(
                    formatter,
                    "TaskAuthorityAssignment is {state:?}, expected Active"
                )
            }
            Self::TakeoverReceiptNotComplete => {
                formatter.write_str("TaskAuthorityTakeoverReceipt is not Complete")
            }
            Self::MissingSuccessorAssignmentId => {
                formatter.write_str("completed takeover receipt missing new_assignment_id")
            }
            Self::MissingExactFenceSetRoot => {
                formatter.write_str("completed takeover receipt missing exact_fence_set_root")
            }
            Self::ZeroAuthorityTerm => formatter.write_str("authority_term must be non-zero"),
            Self::MissingReceiptConsensusCommitIndex => formatter.write_str(
                "TaskAuthorityTakeoverReceipt.consensus_commit_index must be Some(>= 1)",
            ),
            Self::ConsensusCommitIndexMismatch {
                record_index,
                attested_index,
            } => write!(
                formatter,
                "consensus_commit_index mismatch: record={record_index} != attested={attested_index}"
            ),
            Self::StaleConsensusCommitIndex {
                high_water_index,
                attempted_index,
            } => write!(
                formatter,
                "stale consensus_commit_index {attempted_index} < high-water {high_water_index}"
            ),
            Self::ConflictingCommitAtSameIndex { commit_index } => write!(
                formatter,
                "conflicting payload at already-admitted consensus_commit_index {commit_index}"
            ),
            Self::StaleConsensusTerm {
                high_water_term,
                attempted_term,
            } => write!(
                formatter,
                "stale consensus_term {attempted_term} < high-water {high_water_term}"
            ),
            Self::NonMonotonicAuthorityTerm {
                previous_term,
                attempted_term,
            } => write!(
                formatter,
                "non-monotonic authority_term {attempted_term} <= {previous_term}"
            ),
        }
    }
}

impl Error for ConsensusIndexGateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::VoterIdentityRejected { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TaskCommitWatermark {
    consensus_commit_index: NonZeroU64,
    consensus_term: NonZeroU64,
    authority_term: NonZeroU64,
    attestation_digest: [u8; 32],
}

/// Gate verifying majority-committed `consensus_commit_index` proofs before
/// admitting cross-Cell `TaskAuthorityAssignment` or
/// `TaskAuthorityTakeoverReceipt` transitions (`ADR-0026` `C-CONSENSUS-CP`).
#[derive(Clone, Debug)]
pub struct CommittedControlIndexGate {
    backing_mode: ConsensusBackingMode,
    watermarks: BTreeMap<(TaskId, Generation), TaskCommitWatermark>,
}

impl CommittedControlIndexGate {
    /// Initializes a [`CommittedControlIndexGate`] under `backing_mode`
    /// (`SingleHostPassthrough` vs `MultiHostQuorum`).
    #[must_use]
    pub const fn new(backing_mode: ConsensusBackingMode) -> Self {
        Self {
            backing_mode,
            watermarks: BTreeMap::new(),
        }
    }

    /// Configured [`ConsensusBackingMode`].
    #[must_use]
    pub const fn backing_mode(&self) -> &ConsensusBackingMode {
        &self.backing_mode
    }

    /// Highest admitted `(consensus_commit_index, authority_term)` for
    /// `(task_id, task_generation)`, if any.
    #[must_use]
    pub fn high_water_for(
        &self,
        task_id: TaskId,
        task_generation: Generation,
    ) -> Option<(NonZeroU64, NonZeroU64)> {
        self.watermarks
            .get(&(task_id, task_generation))
            .map(|w| (w.consensus_commit_index, w.authority_term))
    }

    /// Verifies and admits an active [`TaskAuthorityAssignment`] bound to
    /// `expected_commit_index` and `attestation`.
    ///
    /// # Errors
    ///
    /// Fails closed on index mismatch, insufficient/invalid quorum signatures,
    /// duplicate failure domains, or non-monotonic commit/term progression.
    pub fn admit_assignment(
        &mut self,
        identity: &IdentityAuthority,
        assignment: &TaskAuthorityAssignment,
        expected_commit_index: NonZeroU64,
        attestation: &QuorumCommitAttestation,
    ) -> Result<VerifiedControlCommitProof, ConsensusIndexGateError> {
        if expected_commit_index != attestation.consensus_commit_index {
            return Err(ConsensusIndexGateError::ConsensusCommitIndexMismatch {
                record_index: expected_commit_index.get(),
                attested_index: attestation.consensus_commit_index,
            });
        }
        let subject = ControlCommitSubject::from_assignment(assignment)?;
        self.verify_and_record(identity, &subject, attestation)
    }

    /// Verifies and admits a completed [`TaskAuthorityTakeoverReceipt`],
    /// requiring `receipt.consensus_commit_index` to be `Some(>= 1)` and
    /// cryptographically bound to `attestation`.
    ///
    /// # Errors
    ///
    /// Fails closed if `receipt.consensus_commit_index` is `None` / `0` /
    /// mismatched, if `receipt` is not `Complete`, if quorum signatures fail,
    /// or if `(consensus_commit_index, consensus_term, authority_term)` regresses.
    pub fn admit_takeover_receipt(
        &mut self,
        identity: &IdentityAuthority,
        receipt: &TaskAuthorityTakeoverReceipt,
        attestation: &QuorumCommitAttestation,
    ) -> Result<VerifiedControlCommitProof, ConsensusIndexGateError> {
        let raw_idx = receipt
            .consensus_commit_index
            .filter(|&v| v > 0)
            .ok_or(ConsensusIndexGateError::MissingReceiptConsensusCommitIndex)?;
        if raw_idx != attestation.consensus_commit_index.get() {
            return Err(ConsensusIndexGateError::ConsensusCommitIndexMismatch {
                record_index: raw_idx,
                attested_index: attestation.consensus_commit_index,
            });
        }
        let subject = ControlCommitSubject::from_takeover_receipt(receipt)?;
        self.verify_and_record(identity, &subject, attestation)
    }

    fn verify_and_record(
        &mut self,
        identity: &IdentityAuthority,
        subject: &ControlCommitSubject,
        attestation: &QuorumCommitAttestation,
    ) -> Result<VerifiedControlCommitProof, ConsensusIndexGateError> {
        let message_digest = quorum_commit_attestation_digest(
            attestation.consensus_commit_index,
            attestation.consensus_term,
            &attestation.state_root_digest,
            subject,
        );

        let verified_voter_domains =
            self.verify_quorum_signatures(identity, message_digest, attestation)?;

        let task_key = subject.task_scope();
        let authority_term = subject.effective_authority_term();

        if let Some(&prev) = self.watermarks.get(&task_key) {
            if attestation.consensus_commit_index < prev.consensus_commit_index {
                return Err(ConsensusIndexGateError::StaleConsensusCommitIndex {
                    high_water_index: prev.consensus_commit_index,
                    attempted_index: attestation.consensus_commit_index,
                });
            }
            if attestation.consensus_commit_index == prev.consensus_commit_index {
                if message_digest == prev.attestation_digest {
                    return Ok(VerifiedControlCommitProof {
                        consensus_commit_index: attestation.consensus_commit_index,
                        consensus_term: attestation.consensus_term,
                        state_root_digest: attestation.state_root_digest,
                        authority_term,
                        verified_voter_domains,
                        disposition: AdmittedCommitDisposition::IdempotentReplay,
                    });
                }
                return Err(ConsensusIndexGateError::ConflictingCommitAtSameIndex {
                    commit_index: attestation.consensus_commit_index,
                });
            }
            if attestation.consensus_term < prev.consensus_term {
                return Err(ConsensusIndexGateError::StaleConsensusTerm {
                    high_water_term: prev.consensus_term,
                    attempted_term: attestation.consensus_term,
                });
            }
            if authority_term <= prev.authority_term {
                return Err(ConsensusIndexGateError::NonMonotonicAuthorityTerm {
                    previous_term: prev.authority_term,
                    attempted_term: authority_term,
                });
            }
        }

        self.watermarks.insert(
            task_key,
            TaskCommitWatermark {
                consensus_commit_index: attestation.consensus_commit_index,
                consensus_term: attestation.consensus_term,
                authority_term,
                attestation_digest: message_digest,
            },
        );

        Ok(VerifiedControlCommitProof {
            consensus_commit_index: attestation.consensus_commit_index,
            consensus_term: attestation.consensus_term,
            state_root_digest: attestation.state_root_digest,
            authority_term,
            verified_voter_domains,
            disposition: AdmittedCommitDisposition::Advanced,
        })
    }

    fn verify_quorum_signatures(
        &self,
        identity: &IdentityAuthority,
        message_digest: [u8; 32],
        attestation: &QuorumCommitAttestation,
    ) -> Result<Vec<(PrincipalId, ControlDomainId)>, ConsensusIndexGateError> {
        let mut seen_principals = BTreeSet::new();
        let mut seen_domains = BTreeSet::new();
        let mut verified = Vec::with_capacity(attestation.voter_attestations.len());

        for vote in &attestation.voter_attestations {
            match &self.backing_mode {
                ConsensusBackingMode::SingleHostPassthrough {
                    local_principal,
                    local_domain,
                } => {
                    if vote.voter_principal != *local_principal
                        || vote.voter_control_domain != *local_domain
                    {
                        return Err(ConsensusIndexGateError::UnauthorizedVoter {
                            principal: vote.voter_principal,
                            domain: vote.voter_control_domain,
                        });
                    }
                }
                ConsensusBackingMode::MultiHostQuorum { voter_roster } => {
                    let Some(&expected_domain) = voter_roster.get(&vote.voter_principal) else {
                        return Err(ConsensusIndexGateError::UnauthorizedVoter {
                            principal: vote.voter_principal,
                            domain: vote.voter_control_domain,
                        });
                    };
                    if expected_domain != vote.voter_control_domain {
                        return Err(ConsensusIndexGateError::UnauthorizedVoter {
                            principal: vote.voter_principal,
                            domain: vote.voter_control_domain,
                        });
                    }
                }
            }

            if !seen_principals.insert(vote.voter_principal) {
                return Err(ConsensusIndexGateError::DuplicateVoterPrincipal(
                    vote.voter_principal,
                ));
            }
            if !seen_domains.insert(vote.voter_control_domain) {
                return Err(ConsensusIndexGateError::NonIndependentControlDomain(
                    vote.voter_control_domain,
                ));
            }

            let signer = identity
                .verify_semantic_authority_signature(VerifySemanticAuthoritySignatureRequest {
                    message_digest,
                    issuer: vote.voter_principal,
                    control_domain_id: vote.voter_control_domain,
                    key_id: vote.voter_key_id,
                    signature: vote.signature,
                    verified_at_ms: attestation.verified_at_ms,
                })
                .map_err(|error| ConsensusIndexGateError::VoterIdentityRejected {
                    voter: vote.voter_principal,
                    error,
                })?;

            verified.push((signer.principal_id(), signer.control_domain_id()));
        }

        let required_quorum = self.backing_mode.required_quorum();
        if verified.len() < required_quorum {
            return Err(ConsensusIndexGateError::InsufficientQuorum {
                verified_count: verified.len(),
                required_quorum,
            });
        }

        verified.sort_by_key(|&(p, _)| p);
        Ok(verified)
    }
}

// ============================================================================
// Pure-Rust `unsafe_code = "forbid"` RFC 6234 SHA-256 (zero external crate dep)
// ============================================================================

const SHA256_IV: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

fn sha256_compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut schedule = [0u32; 64];
    for (idx, word_bytes) in block.as_chunks::<4>().0.iter().enumerate() {
        schedule[idx] = u32::from_be_bytes(*word_bytes);
    }
    for idx in 16..64 {
        let s0 = schedule[idx - 15].rotate_right(7)
            ^ schedule[idx - 15].rotate_right(18)
            ^ (schedule[idx - 15] >> 3);
        let s1 = schedule[idx - 2].rotate_right(17)
            ^ schedule[idx - 2].rotate_right(19)
            ^ (schedule[idx - 2] >> 10);
        schedule[idx] = schedule[idx - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[idx - 7])
            .wrapping_add(s1);
    }

    let [
        mut v0,
        mut v1,
        mut v2,
        mut v3,
        mut v4,
        mut v5,
        mut v6,
        mut v7,
    ] = *state;
    for idx in 0..64 {
        let s1 = v4.rotate_right(6) ^ v4.rotate_right(11) ^ v4.rotate_right(25);
        let ch = (v4 & v5) ^ ((!v4) & v6);
        let temp1 = v7
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[idx])
            .wrapping_add(schedule[idx]);
        let s0 = v0.rotate_right(2) ^ v0.rotate_right(13) ^ v0.rotate_right(22);
        let maj = (v0 & v1) ^ (v0 & v2) ^ (v1 & v2);
        let temp2 = s0.wrapping_add(maj);

        v7 = v6;
        v6 = v5;
        v5 = v4;
        v4 = v3.wrapping_add(temp1);
        v3 = v2;
        v2 = v1;
        v1 = v0;
        v0 = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(v0);
    state[1] = state[1].wrapping_add(v1);
    state[2] = state[2].wrapping_add(v2);
    state[3] = state[3].wrapping_add(v3);
    state[4] = state[4].wrapping_add(v4);
    state[5] = state[5].wrapping_add(v5);
    state[6] = state[6].wrapping_add(v6);
    state[7] = state[7].wrapping_add(v7);
}

#[must_use]
fn compute_sha256(data: &[u8]) -> [u8; 32] {
    let mut state = SHA256_IV;
    let (blocks, remainder) = data.as_chunks::<64>();
    for block in blocks {
        sha256_compress(&mut state, block);
    }

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut tail = [0u8; 128];
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    if remainder.len() < 56 {
        tail[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let first_block: &[u8; 64] = tail[0..64].try_into().expect("64B");
        sha256_compress(&mut state, first_block);
    } else {
        tail[120..128].copy_from_slice(&bit_len.to_be_bytes());
        let first_block: &[u8; 64] = tail[0..64].try_into().expect("64B");
        let second_block: &[u8; 64] = tail[64..128].try_into().expect("64B");
        sha256_compress(&mut state, first_block);
        sha256_compress(&mut state, second_block);
    }

    let mut out = [0u8; 32];
    for (idx, word) in state.iter().enumerate() {
        out[idx * 4..(idx + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::compute_sha256;

    #[test]
    fn rfc6234_sha256_vectors_match() {
        let empty = compute_sha256(b"");
        assert_eq!(
            empty,
            [
                0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
                0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
                0x78, 0x52, 0xb8, 0x55
            ]
        );
        let abc = compute_sha256(b"abc");
        assert_eq!(
            abc,
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad
            ]
        );
        let two_block = compute_sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            two_block,
            [
                0x24, 0x8d, 0x6a, 0x61, 0xd2, 0x06, 0x38, 0xb8, 0xe5, 0xc0, 0x26, 0x93, 0x0c, 0x3e,
                0x60, 0x39, 0xa3, 0x3c, 0xe4, 0x59, 0x64, 0xff, 0x21, 0x67, 0xf6, 0xec, 0xed, 0xd4,
                0x19, 0xdb, 0x06, 0xc1
            ]
        );
    }
}
