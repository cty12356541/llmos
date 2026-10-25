//! Hierarchical subgroup fanout, deterministic local reduction with Merkle
//! result roots, multi-axis spawn/fanout admission backpressure, and
//! multi-scope bulkhead isolation (`[DIST-TASK-003]`, `[DIST-FANOUT-001]`,
//! `[DIST-BULKHEAD-001]`).
//!
//! This module implements the Stage C (`C-FANOUT`) algorithmic and control
//! plane primitives mandated by `06-架构设计总纲-v0.5.md` §26.1 and §25.1:
//!
//! 1. **`[DIST-TASK-003]` Hierarchical Subgroups, Local Reducer, Merkle Result
//!    Root & Incremental Checkpoint**:
//!    - [`HierarchicalFanoutTree`] & [`SubgroupNode`] partition large
//!      `TaskGroup`s into bounded-branching-factor (`max_branching_factor`),
//!      bounded-depth (`max_depth`), cycle-free subgroup hierarchies
//!      (`root -> intermediate subgroups -> leaf attempts/agents`) so no
//!      coordinator requires `O(N^2)` all-to-all links or full-state broadcast.
//!    - [`DeterministicLocalReducer`] binds an immutable non-zero
//!      `reducer_digest`, sorts members canonically independent of arrival
//!      order (`[TASK-GROUP-003]`), preserves conflicting/divergent candidate
//!      receipts explicitly rather than silent last-writer-wins, and computes
//!      a domain-separated binary SHA-256 [`MerkleResultRoot`] plus verifiable
//!      [`MerkleInclusionProof`]s ([`verify_merkle_proof`]).
//!    - [`IncrementalSubgroupCheckpoint`] tracks dirty vs cached subgroup paths
//!      and recomputes only ancestor subgroups from an updated leaf to the root
//!      (`O(B * depth)` hashed nodes rather than `O(N)` full-tree rescans).
//! 2. **`[DIST-FANOUT-001]` Multi-Axis Spawn/Fanout Admission & Backpressure**:
//!    - [`FanoutQuotaContract`] & [`FanoutUsageLedger`] pre-reserve and cap all
//!      six normative dimensions (`max_group_depth`,
//!      `max_total_children` / `max_active_agents`,
//!      `max_message_count` / `max_message_bytes`, `max_context_read_bytes`,
//!      `max_write_bytes`, `max_external_effects`).
//!    - Exceeding resource capacity transitions requests cleanly to
//!      [`FanoutAdmissionDecision::WaitingResourceBackpressure`] instead of
//!      runaway recursion; settling or cancelling reservations refunds unused
//!      headroom and deterministically unblocks waiting requests in FIFO order.
//! 3. **`[DIST-BULKHEAD-001]` Multi-Scope Bulkhead Isolation & `SafetyTcb`
//!    System Reserve**:
//!    - [`BulkheadRegistry`] & [`BulkheadController`] enforce unique
//!      [`ResourceGroupId`] bindings, independent concurrency pools, bounded
//!      wait queues, and [`FailureBudget`] circuit breakers across `Tenant`,
//!      `Workspace`, `TaskGroup`, and `Provider` scopes, alongside an
//!      untouchable [`SafetyTcbSystemReserve`] whose slots non-TCB traffic can
//!      never borrow or exhaust.
//!
//! Honest scope boundaries:
//! - Pure deterministic in-memory state machines and cryptographic proofs;
//!   `SqliteTaskAuthority` durable schema remains at `SCHEMA_VERSION = 47`.
//! - Cross-Cell network transport and async RPC timers stay in the Cell /
//!   runtime layers.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;

use nlos_types::{
    AgentInstanceId, ControlDomainId, DriverId, IdempotencyKey, PrincipalId, ReceiptId,
    ReservationId, ResourceGroupId, TaskAttemptId, TaskGroupId, WorkspaceId,
};
use sha2::{Digest, Sha256};

/// Domain separator for leaf hashes in a fanout binary Merkle tree.
pub const FANOUT_MERKLE_LEAF_DOMAIN: &[u8] = b"llmos/fanout-merkle-leaf/v1";

/// Domain separator for internal binary node hashes in a fanout Merkle tree.
pub const FANOUT_MERKLE_NODE_DOMAIN: &[u8] = b"llmos/fanout-merkle-node/v1";

/// Domain separator for unreported leaf slots during incremental checkpoints.
pub const FANOUT_MERKLE_PENDING_LEAF_DOMAIN: &[u8] = b"llmos/fanout-merkle-pending-leaf/v1";

/// Domain separator for an empty Merkle result tree (`0` leaves).
pub const FANOUT_MERKLE_EMPTY_DOMAIN: &[u8] = b"llmos/fanout-merkle-empty/v1";

/// Domain separator for [`DeterministicLocalReducer`] summary digests.
pub const FANOUT_LOCAL_REDUCE_DOMAIN: &[u8] = b"llmos/fanout-local-reduce/v1";

/// Domain separator for [`IncrementalSubgroupCheckpoint`] subgroup digests.
pub const FANOUT_SUBGROUP_CHECKPOINT_DOMAIN: &[u8] = b"llmos/fanout-subgroup-checkpoint/v1";

/// Domain separator for deterministic intermediate `TaskGroupId` derivation.
pub const FANOUT_DERIVED_SUBGROUP_DOMAIN: &[u8] = b"llmos/fanout-derived-subgroup-id/v1";

/// Domain errors produced by the `C-FANOUT` subgroup, reducer, quota, and
/// bulkhead controllers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FanoutError {
    /// A configuration parameter (`max_branching_factor`, concurrency cap, etc.)
    /// violated basic structural bounds.
    InvalidConfiguration {
        /// Human-readable static explanation of the violated invariant.
        reason: &'static str,
    },
    /// A zero `[0u8; 32]` reducer digest was supplied where an immutable bound
    /// digest is mandatory (`[TASK-GROUP-003]`).
    ZeroReducerDigest,
    /// Caller attempted to evaluate or merge against a mismatched reducer
    /// digest.
    ReducerDigestMismatch {
        /// Expected immutable reducer digest.
        expected: [u8; 32],
        /// Caller-supplied reducer digest.
        supplied: [u8; 32],
    },
    /// Duplicate subgroup registration in a [`HierarchicalFanoutTree`].
    DuplicateSubgroup {
        /// Colliding subgroup identifier.
        subgroup_id: TaskGroupId,
    },
    /// Referenced parent or target subgroup does not exist in the tree.
    UnknownSubgroup {
        /// Missing subgroup identifier.
        subgroup_id: TaskGroupId,
    },
    /// Subgroup graph contains a parent/child cycle (`[TASK-GROUP-001]`).
    CycleDetected {
        /// Subgroup participating in the cycle.
        subgroup_id: TaskGroupId,
    },
    /// A subgroup was given more than one parent (`[TASK-GROUP-001]` requires a
    /// strict tree).
    MultipleParents {
        /// Subgroup that declared conflicting parents.
        subgroup_id: TaskGroupId,
    },
    /// A subgroup is unreachable from `root_group_id`.
    DisconnectedSubgroup {
        /// Unreachable subgroup identifier.
        subgroup_id: TaskGroupId,
    },
    /// Adding a child subgroup or leaf would exceed `max_branching_factor`.
    BranchingFactorExceeded {
        /// Parent subgroup at capacity.
        subgroup_id: TaskGroupId,
        /// Degree that was attempted.
        attempted_degree: usize,
        /// Configured upper bound `B`.
        max_branching_factor: usize,
    },
    /// Adding a subgroup would exceed `max_depth`.
    MaxDepthExceeded {
        /// Requested subgroup depth from root (`root = 0`).
        requested_depth: u32,
        /// Configured maximum subgroup depth.
        max_depth: u32,
    },
    /// The same leaf attempt or agent was registered more than once.
    DuplicateLeafMember {
        /// Colliding leaf member identity.
        leaf_id: FanoutLeafId,
    },
    /// Referenced leaf is not registered in the tree or reducer.
    UnknownLeafMember {
        /// Missing leaf member identity.
        leaf_id: FanoutLeafId,
    },
    /// Requested Merkle inclusion proof for a leaf that has not yet reported a
    /// candidate receipt.
    LeafNotYetReported {
        /// Leaf member with no reported candidate receipt.
        leaf_id: FanoutLeafId,
    },
    /// Reservation ID already exists with different parameters.
    ConflictingReservationRequest {
        /// Colliding reservation identity.
        reservation_id: ReservationId,
    },
    /// Attempted to settle or cancel an unknown or already-closed reservation.
    UnknownActiveReservation {
        /// Missing reservation identity.
        reservation_id: ReservationId,
    },
    /// Actual usage reported at settlement exceeded the pre-reserved amount on
    /// at least one axis.
    ActualUsageExceedsReservation {
        /// Reservation whose settlement violated pre-reserved bounds.
        reservation_id: ReservationId,
        /// First axis that exceeded its reservation.
        axis: FanoutAxis,
    },
    /// Duplicate bulkhead registration for the same scope identity.
    DuplicateBulkheadScope {
        /// Already-registered bulkhead scope.
        scope_id: BulkheadScopeId,
    },
    /// Two bulkheads (or a bulkhead and `SafetyTcbSystemReserve`) attempted to
    /// share the same [`ResourceGroupId`] (`[DIST-BULKHEAD-001]`).
    DuplicateBulkheadResourceGroup {
        /// Colliding resource group identity.
        resource_group_id: ResourceGroupId,
    },
    /// Referenced bulkhead scope has not been registered.
    UnknownBulkheadScope {
        /// Unregistered bulkhead scope.
        scope_id: BulkheadScopeId,
    },
    /// Referenced workload bulkhead lease is unknown or already completed.
    UnknownBulkheadLease {
        /// Missing lease identifier.
        lease_id: BulkheadLeaseId,
    },
    /// Referenced `SafetyTcb` reserve lease is unknown or already released.
    UnknownTcbLease {
        /// Missing TCB lease identifier.
        lease_id: TcbSlotLeaseId,
    },
    /// Failure attribution referenced a scope not held by the completed lease,
    /// or provided an empty attribution set.
    InvalidFailureAttribution {
        /// Completed lease identifier.
        lease_id: BulkheadLeaseId,
    },
    /// Administrative bulkhead reset supplied a stale or non-monotonic epoch.
    StaleBulkheadResetEpoch {
        /// Target bulkhead scope.
        scope_id: BulkheadScopeId,
        /// Current reset epoch on record.
        current_epoch: u64,
        /// Caller-supplied reset epoch.
        supplied_epoch: u64,
    },
}

impl fmt::Display for FanoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration { reason } => {
                write!(f, "invalid fanout configuration: {reason}")
            }
            Self::ZeroReducerDigest => f.write_str("reducer_digest must be non-zero"),
            Self::ReducerDigestMismatch { .. } => {
                f.write_str("supplied reducer_digest does not match bound reducer")
            }
            Self::DuplicateSubgroup { subgroup_id } => {
                write!(f, "duplicate subgroup {subgroup_id:?}")
            }
            Self::UnknownSubgroup { subgroup_id } => {
                write!(f, "unknown subgroup {subgroup_id:?}")
            }
            Self::CycleDetected { subgroup_id } => {
                write!(f, "cycle detected at subgroup {subgroup_id:?}")
            }
            Self::MultipleParents { subgroup_id } => {
                write!(f, "subgroup {subgroup_id:?} declared multiple parents")
            }
            Self::DisconnectedSubgroup { subgroup_id } => {
                write!(f, "subgroup {subgroup_id:?} is disconnected from root")
            }
            Self::BranchingFactorExceeded {
                subgroup_id,
                attempted_degree,
                max_branching_factor,
            } => write!(
                f,
                "subgroup {subgroup_id:?} degree {attempted_degree} exceeds max_branching_factor {max_branching_factor}"
            ),
            Self::MaxDepthExceeded {
                requested_depth,
                max_depth,
            } => write!(
                f,
                "subgroup depth {requested_depth} exceeds max_depth {max_depth}"
            ),
            Self::DuplicateLeafMember { leaf_id } => {
                write!(f, "duplicate leaf member {leaf_id:?}")
            }
            Self::UnknownLeafMember { leaf_id } => {
                write!(f, "unknown leaf member {leaf_id:?}")
            }
            Self::LeafNotYetReported { leaf_id } => {
                write!(f, "leaf member {leaf_id:?} has no candidate receipt yet")
            }
            Self::ConflictingReservationRequest { reservation_id } => {
                write!(
                    f,
                    "reservation {reservation_id:?} replayed with conflicting request"
                )
            }
            Self::UnknownActiveReservation { reservation_id } => {
                write!(f, "unknown active reservation {reservation_id:?}")
            }
            Self::ActualUsageExceedsReservation {
                reservation_id,
                axis,
            } => write!(
                f,
                "actual usage for reservation {reservation_id:?} exceeded reserved budget on {axis:?}"
            ),
            Self::DuplicateBulkheadScope { scope_id } => {
                write!(f, "duplicate bulkhead scope {scope_id:?}")
            }
            Self::DuplicateBulkheadResourceGroup { resource_group_id } => {
                write!(
                    f,
                    "duplicate bulkhead ResourceGroupId {resource_group_id:?}"
                )
            }
            Self::UnknownBulkheadScope { scope_id } => {
                write!(f, "unknown bulkhead scope {scope_id:?}")
            }
            Self::UnknownBulkheadLease { lease_id } => {
                write!(f, "unknown bulkhead lease {lease_id:?}")
            }
            Self::UnknownTcbLease { lease_id } => {
                write!(f, "unknown SafetyTcb lease {lease_id:?}")
            }
            Self::InvalidFailureAttribution { lease_id } => {
                write!(
                    f,
                    "invalid failure attribution for bulkhead lease {lease_id:?}"
                )
            }
            Self::StaleBulkheadResetEpoch {
                scope_id,
                current_epoch,
                supplied_epoch,
            } => write!(
                f,
                "stale reset epoch {supplied_epoch} for bulkhead {scope_id:?} (current {current_epoch})"
            ),
        }
    }
}

impl Error for FanoutError {}

// ============================================================================
// 1. [DIST-TASK-003] Hierarchical Subgroups, Local Reducer & Merkle Checkpoint
// ============================================================================

/// Stable leaf member identity inside a hierarchical fanout `TaskGroup`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FanoutLeafId {
    /// Leaf backed by a [`TaskAttemptId`].
    TaskAttempt(TaskAttemptId),
    /// Leaf backed by a logical [`AgentInstanceId`].
    AgentInstance(AgentInstanceId),
}

impl FanoutLeafId {
    /// Stable 1-byte wire tag used in canonical Merkle leaf hashing.
    #[must_use]
    pub const fn kind_tag(self) -> u8 {
        match self {
            Self::TaskAttempt(_) => 0,
            Self::AgentInstance(_) => 1,
        }
    }

    /// Underlying 16-byte nominal identifier.
    #[must_use]
    pub const fn id_bytes(&self) -> &[u8; 16] {
        match self {
            Self::TaskAttempt(id) => id.as_bytes(),
            Self::AgentInstance(id) => id.as_bytes(),
        }
    }
}

/// Leaf registration record binding a [`FanoutLeafId`] to its failure/trust
/// [`ControlDomainId`] (`[TASK-GROUP-003]`, `[TASK-SCALE-002]`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FanoutLeafMember {
    /// Stable leaf identity.
    pub leaf_id: FanoutLeafId,
    /// Failure/trust independence domain of this leaf.
    pub control_domain_id: ControlDomainId,
}

/// Candidate result receipt reported by a leaf attempt or agent.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LeafCandidateReceipt {
    /// Reporting leaf member identity.
    pub leaf_id: FanoutLeafId,
    /// Receipt identity emitted by the leaf execution.
    pub receipt_id: ReceiptId,
    /// Trust / control domain of the reporting leaf.
    pub control_domain_id: ControlDomainId,
    /// Digest of the candidate output payload.
    pub result_digest: [u8; 32],
    /// Digest of the input snapshot / model / judgment provenance.
    pub provenance_digest: [u8; 32],
}

impl LeafCandidateReceipt {
    fn canonical_sort_key(&self) -> (FanoutLeafId, [u8; 32], [u8; 16], [u8; 16], [u8; 32]) {
        (
            self.leaf_id,
            self.result_digest,
            *self.receipt_id.as_bytes(),
            *self.control_domain_id.as_bytes(),
            self.provenance_digest,
        )
    }
}

impl Ord for LeafCandidateReceipt {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.canonical_sort_key().cmp(&other.canonical_sort_key())
    }
}

impl PartialOrd for LeafCandidateReceipt {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Canonical per-leaf slot committed into a [`MerkleResultRoot`].
///
/// When multiple conflicting/divergent candidate receipts are observed for the
/// same `leaf_id`, all distinct candidates are retained in canonical sorted
/// order (`[TASK-GROUP-003]`), never overwritten by arrival order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerkleLeafEntry {
    /// Leaf member identity.
    pub leaf_id: FanoutLeafId,
    /// Canonically sorted non-empty candidate receipts for `leaf_id`.
    pub candidates: Vec<LeafCandidateReceipt>,
}

impl MerkleLeafEntry {
    /// Returns `true` when more than one distinct candidate receipt was
    /// observed for this leaf.
    #[must_use]
    pub fn is_divergent(&self) -> bool {
        self.candidates.len() > 1
    }

    /// Validates canonical sorting, non-emptiness, and `leaf_id` consistency.
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        if self.candidates.is_empty() {
            return false;
        }
        for (idx, candidate) in self.candidates.iter().enumerate() {
            if candidate.leaf_id != self.leaf_id {
                return false;
            }
            if idx > 0 && self.candidates[idx - 1] >= *candidate {
                return false;
            }
        }
        true
    }
}

/// Explicit record of a leaf that reported divergent candidate receipts
/// (`[TASK-GROUP-003]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DivergentLeafRecord {
    /// Leaf with conflicting candidates.
    pub leaf_id: FanoutLeafId,
    /// Canonically ordered conflicting candidates preserved without
    /// last-writer-wins truncation.
    pub candidates: Vec<LeafCandidateReceipt>,
}

/// Binary SHA-256 Merkle root over canonically ordered fanout results.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MerkleResultRoot([u8; 32]);

impl MerkleResultRoot {
    /// Wraps a raw 32-byte SHA-256 Merkle root digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the 32-byte digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Consumes the wrapper and returns the 32-byte digest.
    #[must_use]
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Position of a sibling node along a binary [`MerkleInclusionProof`] path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MerkleSiblingPosition {
    /// Sibling sits on the left (`H(sibling, current)`).
    Left,
    /// Sibling sits on the right (`H(current, sibling)`).
    Right,
}

/// One binary hash step in a [`MerkleInclusionProof`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MerkleProofStep {
    /// 32-byte hash of the sibling branch.
    pub sibling_hash: [u8; 32],
    /// Whether `sibling_hash` is the left or right input to
    /// [`compute_merkle_node_hash`].
    pub position: MerkleSiblingPosition,
}

/// Self-contained inclusion proof from a [`MerkleLeafEntry`] to a
/// [`MerkleResultRoot`] (either subgroup-local or full hierarchical root).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerkleInclusionProof {
    /// Canonical leaf entry being proven.
    pub leaf_entry: MerkleLeafEntry,
    /// Bottom-up sibling hashes from the leaf to the target Merkle root.
    pub steps: Vec<MerkleProofStep>,
}

/// Computes the domain-separated SHA-256 hash of a [`MerkleLeafEntry`]
/// (`"llmos/fanout-merkle-leaf/v1"`).
#[must_use]
pub fn compute_merkle_leaf_hash(reducer_digest: &[u8; 32], entry: &MerkleLeafEntry) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(FANOUT_MERKLE_LEAF_DOMAIN);
    hasher.update([0u8]);
    hasher.update(reducer_digest);
    hasher.update([entry.leaf_id.kind_tag()]);
    hasher.update(entry.leaf_id.id_bytes());
    let count = u64::try_from(entry.candidates.len()).unwrap_or(u64::MAX);
    hasher.update(count.to_be_bytes());
    for candidate in &entry.candidates {
        hasher.update(candidate.receipt_id.as_bytes());
        hasher.update(candidate.control_domain_id.as_bytes());
        hasher.update(candidate.result_digest);
        hasher.update(candidate.provenance_digest);
    }
    hasher.finalize().into()
}

/// Computes the domain-separated SHA-256 hash of a binary internal Merkle node
/// (`"llmos/fanout-merkle-node/v1"`).
#[must_use]
pub fn compute_merkle_node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(FANOUT_MERKLE_NODE_DOMAIN);
    hasher.update([0u8]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

fn compute_pending_leaf_hash(reducer_digest: &[u8; 32], leaf_id: FanoutLeafId) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(FANOUT_MERKLE_PENDING_LEAF_DOMAIN);
    hasher.update([0u8]);
    hasher.update(reducer_digest);
    hasher.update([leaf_id.kind_tag()]);
    hasher.update(leaf_id.id_bytes());
    hasher.finalize().into()
}

fn compute_empty_merkle_root(reducer_digest: &[u8; 32]) -> MerkleResultRoot {
    let mut hasher = Sha256::new();
    hasher.update(FANOUT_MERKLE_EMPTY_DOMAIN);
    hasher.update([0u8]);
    hasher.update(reducer_digest);
    MerkleResultRoot::from_bytes(hasher.finalize().into())
}

/// Folds a slice of child hashes into a binary Merkle root, returning
/// `(root, internal_nodes_hashed)`.
///
/// Odd trailing elements at any level are promoted unchanged (RFC 6962 style)
/// so internal node counts equal `hashes.len() - 1` for non-empty inputs and
/// second-preimage ambiguity from leaf duplication cannot arise.
fn fold_binary_merkle_hashes(
    reducer_digest: &[u8; 32],
    hashes: &[[u8; 32]],
) -> (MerkleResultRoot, usize) {
    if hashes.is_empty() {
        return (compute_empty_merkle_root(reducer_digest), 0);
    }
    let mut current = hashes.to_vec();
    let mut internal_nodes_hashed = 0usize;
    while current.len() > 1 {
        let mut next = Vec::with_capacity(current.len().div_ceil(2));
        let mut idx = 0usize;
        while idx < current.len() {
            if idx + 1 < current.len() {
                next.push(compute_merkle_node_hash(&current[idx], &current[idx + 1]));
                internal_nodes_hashed += 1;
                idx += 2;
            } else {
                next.push(current[idx]);
                idx += 1;
            }
        }
        current = next;
    }
    (
        MerkleResultRoot::from_bytes(current[0]),
        internal_nodes_hashed,
    )
}

/// Builds the local binary Merkle sibling steps for `target_index` within
/// `hashes`.
fn build_binary_merkle_steps(hashes: &[[u8; 32]], mut target_index: usize) -> Vec<MerkleProofStep> {
    let mut steps = Vec::new();
    let mut current = hashes.to_vec();
    while current.len() > 1 {
        if target_index.is_multiple_of(2) {
            if target_index + 1 < current.len() {
                steps.push(MerkleProofStep {
                    sibling_hash: current[target_index + 1],
                    position: MerkleSiblingPosition::Right,
                });
            }
        } else {
            steps.push(MerkleProofStep {
                sibling_hash: current[target_index - 1],
                position: MerkleSiblingPosition::Left,
            });
        }

        let mut next = Vec::with_capacity(current.len().div_ceil(2));
        let mut idx = 0usize;
        while idx < current.len() {
            if idx + 1 < current.len() {
                next.push(compute_merkle_node_hash(&current[idx], &current[idx + 1]));
                idx += 2;
            } else {
                next.push(current[idx]);
                idx += 1;
            }
        }
        target_index /= 2;
        current = next;
    }
    steps
}

/// Verifies a [`MerkleInclusionProof`] against `expected_root` and the bound
/// `reducer_digest` (`[DIST-TASK-003]`, `[TASK-GROUP-003]`).
#[must_use]
pub fn verify_merkle_proof(
    expected_root: &MerkleResultRoot,
    reducer_digest: &[u8; 32],
    proof: &MerkleInclusionProof,
) -> bool {
    if *reducer_digest == [0u8; 32] || !proof.leaf_entry.is_canonical() {
        return false;
    }
    let mut acc = compute_merkle_leaf_hash(reducer_digest, &proof.leaf_entry);
    for step in &proof.steps {
        acc = match step.position {
            MerkleSiblingPosition::Left => compute_merkle_node_hash(&step.sibling_hash, &acc),
            MerkleSiblingPosition::Right => compute_merkle_node_hash(&acc, &step.sibling_hash),
        };
    }
    acc == *expected_root.as_bytes()
}

/// Semantics bound to a [`DeterministicLocalReducer`] (`[TASK-GROUP-003]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalReducerKind {
    /// Deterministic structural fold over canonical ID-sorted members.
    CanonicalSortedFold,
    /// Expects unanimous `result_digest` across members while preserving all
    /// distinct candidate digests when members disagree.
    UnanimousConsensusCheck,
    /// Provenance-bound judgment synthesis (`[TASK-GROUP-003]`).
    ProvenanceBoundJudgment {
        /// Authority/policy digest governing the judgment receipt.
        judgment_policy_digest: [u8; 32],
    },
}

impl LocalReducerKind {
    const fn tag(self) -> u8 {
        match self {
            Self::CanonicalSortedFold => 0,
            Self::UnanimousConsensusCheck => 1,
            Self::ProvenanceBoundJudgment { .. } => 2,
        }
    }
}

/// Output produced by [`DeterministicLocalReducer::reduce`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalReductionSummary {
    /// Immutable reducer digest bound to this reduction.
    pub reducer_digest: [u8; 32],
    /// Reducer mode tag.
    pub reducer_kind: LocalReducerKind,
    /// Binary SHA-256 Merkle root over canonically sorted member entries.
    pub merkle_result_root: MerkleResultRoot,
    /// Domain-separated aggregate digest committing to `reducer_digest`,
    /// `reducer_kind`, `merkle_result_root`, distinct `ControlDomainId` count,
    /// and explicit divergence summary.
    pub reduced_digest: [u8; 32],
    /// Number of distinct reported leaves included in the reduction.
    pub reported_leaf_count: usize,
    /// Number of distinct [`ControlDomainId`]s represented among single-value
    /// non-divergent leaves (`[TASK-GROUP-003]`, `[TASK-SCALE-002]`).
    pub independent_control_domains: usize,
    /// Distinct `result_digest` values observed across all candidates in
    /// canonical sorted order.
    pub distinct_result_digests: Vec<[u8; 32]>,
    /// Leaves that reported multiple conflicting candidate receipts.
    pub divergent_leaves: Vec<DivergentLeafRecord>,
    /// Internal binary Merkle nodes hashed during this reduction.
    pub internal_nodes_hashed: usize,
}

impl LocalReductionSummary {
    /// Returns `true` when neither per-leaf conflicts nor cross-leaf digest
    /// disagreements occurred under [`LocalReducerKind::UnanimousConsensusCheck`].
    #[must_use]
    pub fn has_conflict(&self) -> bool {
        if !self.divergent_leaves.is_empty() {
            return true;
        }
        matches!(self.reducer_kind, LocalReducerKind::UnanimousConsensusCheck)
            && self.distinct_result_digests.len() > 1
    }
}

/// Arrival-order-independent local subgroup reducer (`[DIST-TASK-003]`,
/// `[TASK-GROUP-003]`).
#[derive(Clone, Debug)]
pub struct DeterministicLocalReducer {
    reducer_digest: [u8; 32],
    reducer_kind: LocalReducerKind,
    entries: BTreeMap<FanoutLeafId, BTreeSet<LeafCandidateReceipt>>,
}

impl DeterministicLocalReducer {
    /// Creates a new local reducer bound to an immutable non-zero
    /// `reducer_digest`.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::ZeroReducerDigest`] if `reducer_digest` is all
    /// zeros.
    pub fn new(
        reducer_digest: [u8; 32],
        reducer_kind: LocalReducerKind,
    ) -> Result<Self, FanoutError> {
        if reducer_digest == [0u8; 32] {
            return Err(FanoutError::ZeroReducerDigest);
        }
        Ok(Self {
            reducer_digest,
            reducer_kind,
            entries: BTreeMap::new(),
        })
    }

    /// Immutable reducer digest bound at construction.
    #[must_use]
    pub const fn reducer_digest(&self) -> &[u8; 32] {
        &self.reducer_digest
    }

    /// Reducer mode bound at construction.
    #[must_use]
    pub const fn reducer_kind(&self) -> LocalReducerKind {
        self.reducer_kind
    }

    /// Verifies that `expected_digest` matches this reducer's immutable digest.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::ReducerDigestMismatch`] when the digests differ.
    pub fn verify_reducer_binding(&self, expected_digest: &[u8; 32]) -> Result<(), FanoutError> {
        if *expected_digest != self.reducer_digest {
            return Err(FanoutError::ReducerDigestMismatch {
                expected: self.reducer_digest,
                supplied: *expected_digest,
            });
        }
        Ok(())
    }

    /// Records a candidate receipt for `candidate.leaf_id`.
    ///
    /// Returns `true` if this candidate introduced new state, or `false` if the
    /// exact candidate receipt was already recorded (idempotent replay).
    /// Conflicting candidates for the same `leaf_id` are preserved in a
    /// canonical `BTreeSet` rather than overwriting previous candidates.
    pub fn record_candidate(&mut self, candidate: LeafCandidateReceipt) -> bool {
        self.entries
            .entry(candidate.leaf_id)
            .or_default()
            .insert(candidate)
    }

    /// Number of distinct leaves that have reported at least one candidate.
    #[must_use]
    pub fn reported_leaf_count(&self) -> usize {
        self.entries.len()
    }

    /// Produces the canonical [`MerkleLeafEntry`] for `leaf_id`, if reported.
    #[must_use]
    pub fn leaf_entry(&self, leaf_id: FanoutLeafId) -> Option<MerkleLeafEntry> {
        self.entries.get(&leaf_id).map(|set| MerkleLeafEntry {
            leaf_id,
            candidates: set.iter().copied().collect(),
        })
    }

    /// Computes the order-independent [`LocalReductionSummary`] and binary
    /// [`MerkleResultRoot`] over all recorded leaves.
    #[must_use]
    pub fn reduce(&self) -> LocalReductionSummary {
        let mut leaf_hashes = Vec::with_capacity(self.entries.len());
        let mut divergent_leaves = Vec::new();
        let mut distinct_results = BTreeSet::new();
        let mut non_divergent_domains = BTreeSet::new();

        for (&leaf_id, candidate_set) in &self.entries {
            let candidates: Vec<LeafCandidateReceipt> = candidate_set.iter().copied().collect();
            for candidate in &candidates {
                distinct_results.insert(candidate.result_digest);
            }
            if candidates.len() == 1 {
                non_divergent_domains.insert(candidates[0].control_domain_id);
            } else {
                divergent_leaves.push(DivergentLeafRecord {
                    leaf_id,
                    candidates: candidates.clone(),
                });
            }
            let entry = MerkleLeafEntry {
                leaf_id,
                candidates,
            };
            leaf_hashes.push(compute_merkle_leaf_hash(&self.reducer_digest, &entry));
        }

        let (merkle_result_root, internal_nodes_hashed) =
            fold_binary_merkle_hashes(&self.reducer_digest, &leaf_hashes);
        let distinct_result_digests: Vec<[u8; 32]> = distinct_results.into_iter().collect();

        let mut hasher = Sha256::new();
        hasher.update(FANOUT_LOCAL_REDUCE_DOMAIN);
        hasher.update([0u8]);
        hasher.update(self.reducer_digest);
        hasher.update([self.reducer_kind.tag()]);
        if let LocalReducerKind::ProvenanceBoundJudgment {
            judgment_policy_digest,
        } = self.reducer_kind
        {
            hasher.update(judgment_policy_digest);
        }
        hasher.update(merkle_result_root.as_bytes());
        hasher.update(
            u64::try_from(self.entries.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        hasher.update(
            u64::try_from(non_divergent_domains.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        hasher.update(
            u64::try_from(divergent_leaves.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for digest in &distinct_result_digests {
            hasher.update(digest);
        }
        let reduced_digest = hasher.finalize().into();

        LocalReductionSummary {
            reducer_digest: self.reducer_digest,
            reducer_kind: self.reducer_kind,
            merkle_result_root,
            reduced_digest,
            reported_leaf_count: self.entries.len(),
            independent_control_domains: non_divergent_domains.len(),
            distinct_result_digests,
            divergent_leaves,
            internal_nodes_hashed,
        }
    }

    /// Generates a binary [`MerkleInclusionProof`] for `leaf_id` against the
    /// reducer's current [`MerkleResultRoot`].
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::LeafNotYetReported`] when `leaf_id` has not
    /// submitted any candidate receipt.
    pub fn prove_leaf(&self, leaf_id: FanoutLeafId) -> Result<MerkleInclusionProof, FanoutError> {
        let mut target_index = None;
        let mut target_entry = None;
        let mut leaf_hashes = Vec::with_capacity(self.entries.len());

        for (idx, (&id, set)) in self.entries.iter().enumerate() {
            let entry = MerkleLeafEntry {
                leaf_id: id,
                candidates: set.iter().copied().collect(),
            };
            leaf_hashes.push(compute_merkle_leaf_hash(&self.reducer_digest, &entry));
            if id == leaf_id {
                target_index = Some(idx);
                target_entry = Some(entry);
            }
        }

        let (Some(idx), Some(leaf_entry)) = (target_index, target_entry) else {
            return Err(FanoutError::LeafNotYetReported { leaf_id });
        };
        let steps = build_binary_merkle_steps(&leaf_hashes, idx);
        Ok(MerkleInclusionProof { leaf_entry, steps })
    }
}

/// Directed parent-to-child subgroup edge used when validating arbitrary
/// topology inputs via [`HierarchicalFanoutTree::from_topology`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubgroupEdge {
    /// Parent subgroup identifier.
    pub parent_group_id: TaskGroupId,
    /// Child subgroup identifier.
    pub child_group_id: TaskGroupId,
}

/// One node in a [`HierarchicalFanoutTree`] (`root` or intermediate subgroup).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubgroupNode {
    /// Identity of this subgroup.
    pub group_id: TaskGroupId,
    /// Parent subgroup (`None` only for the root subgroup).
    pub parent_group_id: Option<TaskGroupId>,
    /// Depth relative to `root_group_id` (`root = 0`).
    pub depth: u32,
    /// Canonically sorted direct child subgroups.
    pub child_subgroups: Vec<TaskGroupId>,
    /// Canonically sorted direct leaf attempts/agents.
    pub leaf_members: Vec<FanoutLeafMember>,
}

impl SubgroupNode {
    /// Total direct coordinator fanout (`child_subgroups + leaf_members`),
    /// bounded by `max_branching_factor`.
    #[must_use]
    pub fn local_fanout(&self) -> usize {
        self.child_subgroups.len() + self.leaf_members.len()
    }
}

/// Quantitative proof that a [`HierarchicalFanoutTree`] avoids `O(N^2)`
/// all-to-all coordination (`[DIST-TASK-003]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopologyComplexityMetrics {
    /// Total subgroups (`root` + intermediates).
    pub subgroup_count: usize,
    /// Total registered leaf attempts/agents `N`.
    pub total_leaves: usize,
    /// Maximum observed subgroup depth from root.
    pub observed_max_depth: u32,
    /// Maximum local degree across all subgroups (`<= max_branching_factor`).
    pub max_local_subgroup_degree: usize,
    /// Direct degree of the root coordinator (`<= max_branching_factor`).
    pub root_coordinator_degree: usize,
    /// Total tree edges (`(subgroup_count - 1) + total_leaves`, i.e., `O(N)`).
    pub hierarchical_edge_count: u64,
    /// Hypothetical `N * (N - 1) / 2` all-to-all peer mesh edges avoided.
    pub avoided_all_to_all_edges: u64,
}

/// Bounded-branching-factor, bounded-depth, cycle-free subgroup hierarchy
/// (`[DIST-TASK-003]`, `[TASK-GROUP-001]`).
#[derive(Clone, Debug)]
pub struct HierarchicalFanoutTree {
    root_group_id: TaskGroupId,
    max_branching_factor: usize,
    max_depth: u32,
    subgroups: BTreeMap<TaskGroupId, SubgroupNode>,
    leaf_to_subgroup: BTreeMap<FanoutLeafId, TaskGroupId>,
}

impl HierarchicalFanoutTree {
    /// Initializes an empty hierarchy rooted at `root_group_id` (`depth = 0`).
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::InvalidConfiguration`] if
    /// `max_branching_factor == 0`.
    pub fn new(
        root_group_id: TaskGroupId,
        max_branching_factor: usize,
        max_depth: u32,
    ) -> Result<Self, FanoutError> {
        if max_branching_factor == 0 {
            return Err(FanoutError::InvalidConfiguration {
                reason: "max_branching_factor must be >= 1",
            });
        }
        let mut subgroups = BTreeMap::new();
        subgroups.insert(
            root_group_id,
            SubgroupNode {
                group_id: root_group_id,
                parent_group_id: None,
                depth: 0,
                child_subgroups: Vec::new(),
                leaf_members: Vec::new(),
            },
        );
        Ok(Self {
            root_group_id,
            max_branching_factor,
            max_depth,
            subgroups,
            leaf_to_subgroup: BTreeMap::new(),
        })
    }

    /// Deterministically organizes `leaves` into a balanced multi-level
    /// hierarchy whose every subgroup satisfies
    /// `local_fanout <= max_branching_factor` and `depth <= max_depth`.
    ///
    /// Input order of `leaves` does not affect the resulting tree structure.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError`] if `max_branching_factor < 2` while
    /// `leaves.len() > max_branching_factor`, if duplicate `leaf_id`s exist, or
    /// if the required balanced tree height exceeds `max_depth`.
    pub fn build_balanced(
        root_group_id: TaskGroupId,
        max_branching_factor: usize,
        max_depth: u32,
        leaves: &[FanoutLeafMember],
    ) -> Result<Self, FanoutError> {
        if max_branching_factor == 0 {
            return Err(FanoutError::InvalidConfiguration {
                reason: "max_branching_factor must be >= 1",
            });
        }
        if leaves.len() > max_branching_factor && max_branching_factor < 2 {
            return Err(FanoutError::InvalidConfiguration {
                reason: "max_branching_factor must be >= 2 when partitioning > 1 leaves",
            });
        }

        let mut sorted_leaves = leaves.to_vec();
        sorted_leaves.sort_by_key(|m| m.leaf_id);
        for idx in 1..sorted_leaves.len() {
            if sorted_leaves[idx - 1].leaf_id == sorted_leaves[idx].leaf_id {
                return Err(FanoutError::DuplicateLeafMember {
                    leaf_id: sorted_leaves[idx].leaf_id,
                });
            }
        }

        if sorted_leaves.len() <= max_branching_factor {
            let mut tree = Self::new(root_group_id, max_branching_factor, max_depth)?;
            for member in sorted_leaves {
                tree.register_leaf(root_group_id, member)?;
            }
            return Ok(tree);
        }

        // Bottom-up chunking guarantees every subgroup has `<= B` children.
        let mut leaf_parent_groups: Vec<(TaskGroupId, Vec<FanoutLeafMember>)> = sorted_leaves
            .chunks(max_branching_factor)
            .enumerate()
            .map(|(chunk_idx, chunk)| {
                let gid = derive_intermediate_subgroup_id(root_group_id, 0, chunk_idx);
                (gid, chunk.to_vec())
            })
            .collect();

        let mut intermediate_levels: Vec<Vec<(TaskGroupId, Vec<TaskGroupId>)>> = Vec::new();
        let mut current_ids: Vec<TaskGroupId> =
            leaf_parent_groups.iter().map(|(gid, _)| *gid).collect();
        let mut level_from_bottom = 1u32;

        while current_ids.len() > max_branching_factor {
            let next_level: Vec<(TaskGroupId, Vec<TaskGroupId>)> = current_ids
                .chunks(max_branching_factor)
                .enumerate()
                .map(|(chunk_idx, chunk)| {
                    let gid = derive_intermediate_subgroup_id(
                        root_group_id,
                        level_from_bottom,
                        chunk_idx,
                    );
                    (gid, chunk.to_vec())
                })
                .collect();
            current_ids = next_level.iter().map(|(gid, _)| *gid).collect();
            intermediate_levels.push(next_level);
            level_from_bottom = level_from_bottom.saturating_add(1);
        }

        let mut tree = Self::new(root_group_id, max_branching_factor, max_depth)?;
        // Attach top-most chunks directly under `root_group_id`, then walk down.
        for &top_child_id in &current_ids {
            tree.register_subgroup(root_group_id, top_child_id)?;
        }
        for level in intermediate_levels.iter().rev() {
            for (parent_id, children) in level {
                for &child_id in children {
                    tree.register_subgroup(*parent_id, child_id)?;
                }
            }
        }
        for (subgroup_id, members) in leaf_parent_groups.drain(..) {
            for member in members {
                tree.register_leaf(subgroup_id, member)?;
            }
        }
        Ok(tree)
    }

    /// Validates and constructs a [`HierarchicalFanoutTree`] from raw directed
    /// subgroup edges and leaf placements, rejecting cycles, multiple parents,
    /// disconnected subgroups, depth overflow, and branching-factor overflow.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError`] on any structural violation.
    pub fn from_topology(
        root_group_id: TaskGroupId,
        max_branching_factor: usize,
        max_depth: u32,
        edges: &[SubgroupEdge],
        leaf_assignments: &[(TaskGroupId, FanoutLeafMember)],
    ) -> Result<Self, FanoutError> {
        if max_branching_factor == 0 {
            return Err(FanoutError::InvalidConfiguration {
                reason: "max_branching_factor must be >= 1",
            });
        }

        let mut parents_map: BTreeMap<TaskGroupId, BTreeSet<TaskGroupId>> = BTreeMap::new();
        let mut children_map: BTreeMap<TaskGroupId, BTreeSet<TaskGroupId>> = BTreeMap::new();
        let mut all_nodes: BTreeSet<TaskGroupId> = BTreeSet::new();
        all_nodes.insert(root_group_id);

        for edge in edges {
            if edge.parent_group_id == edge.child_group_id {
                return Err(FanoutError::CycleDetected {
                    subgroup_id: edge.child_group_id,
                });
            }
            if edge.child_group_id == root_group_id {
                return Err(FanoutError::CycleDetected {
                    subgroup_id: root_group_id,
                });
            }
            parents_map
                .entry(edge.child_group_id)
                .or_default()
                .insert(edge.parent_group_id);
            children_map
                .entry(edge.parent_group_id)
                .or_default()
                .insert(edge.child_group_id);
            all_nodes.insert(edge.parent_group_id);
            all_nodes.insert(edge.child_group_id);
        }

        // 1. Detect directed cycles before checking strict single-parent tree degree.
        let mut visit_state: BTreeMap<TaskGroupId, u8> = BTreeMap::new();
        for &start_node in &all_nodes {
            if visit_state.get(&start_node).copied().unwrap_or(0) == 0
                && let Some(cycle_node) =
                    detect_directed_cycle(start_node, &children_map, &mut visit_state)
            {
                return Err(FanoutError::CycleDetected {
                    subgroup_id: cycle_node,
                });
            }
        }

        // 2. Enforce strict single-parent tree invariant (`[TASK-GROUP-001]`).
        for (&child_id, parents) in &parents_map {
            if parents.len() > 1 {
                return Err(FanoutError::MultipleParents {
                    subgroup_id: child_id,
                });
            }
        }

        let mut tree = Self::new(root_group_id, max_branching_factor, max_depth)?;
        let mut queue = VecDeque::new();
        queue.push_back(root_group_id);
        let mut visited_from_root = BTreeSet::new();
        visited_from_root.insert(root_group_id);

        while let Some(parent_id) = queue.pop_front() {
            if let Some(children) = children_map.get(&parent_id) {
                for &child_id in children {
                    tree.register_subgroup(parent_id, child_id)?;
                    visited_from_root.insert(child_id);
                    queue.push_back(child_id);
                }
            }
        }

        for &node in &all_nodes {
            if !visited_from_root.contains(&node) {
                return Err(FanoutError::DisconnectedSubgroup { subgroup_id: node });
            }
        }

        for &(subgroup_id, member) in leaf_assignments {
            tree.register_leaf(subgroup_id, member)?;
        }

        Ok(tree)
    }

    /// Registers a direct child subgroup under `parent_group_id`.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError`] if `subgroup_id` already exists, `parent_group_id`
    /// is unknown, `max_depth` would be exceeded, or `parent_group_id` is
    /// already at `max_branching_factor`.
    pub fn register_subgroup(
        &mut self,
        parent_group_id: TaskGroupId,
        subgroup_id: TaskGroupId,
    ) -> Result<(), FanoutError> {
        if parent_group_id == subgroup_id {
            return Err(FanoutError::CycleDetected { subgroup_id });
        }
        if self.subgroups.contains_key(&subgroup_id) {
            return Err(FanoutError::DuplicateSubgroup { subgroup_id });
        }
        let Some(parent) = self.subgroups.get(&parent_group_id) else {
            return Err(FanoutError::UnknownSubgroup {
                subgroup_id: parent_group_id,
            });
        };
        let child_depth = parent.depth.saturating_add(1);
        if child_depth > self.max_depth {
            return Err(FanoutError::MaxDepthExceeded {
                requested_depth: child_depth,
                max_depth: self.max_depth,
            });
        }
        let attempted_degree = parent.local_fanout() + 1;
        if attempted_degree > self.max_branching_factor {
            return Err(FanoutError::BranchingFactorExceeded {
                subgroup_id: parent_group_id,
                attempted_degree,
                max_branching_factor: self.max_branching_factor,
            });
        }

        if let Some(parent_mut) = self.subgroups.get_mut(&parent_group_id)
            && let Err(pos) = parent_mut.child_subgroups.binary_search(&subgroup_id)
        {
            parent_mut.child_subgroups.insert(pos, subgroup_id);
        }
        self.subgroups.insert(
            subgroup_id,
            SubgroupNode {
                group_id: subgroup_id,
                parent_group_id: Some(parent_group_id),
                depth: child_depth,
                child_subgroups: Vec::new(),
                leaf_members: Vec::new(),
            },
        );
        Ok(())
    }

    /// Registers a leaf attempt or agent inside `subgroup_id`.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError`] if `subgroup_id` is unknown, `member.leaf_id` is
    /// already registered anywhere in the tree, or `subgroup_id` would exceed
    /// `max_branching_factor`.
    pub fn register_leaf(
        &mut self,
        subgroup_id: TaskGroupId,
        member: FanoutLeafMember,
    ) -> Result<(), FanoutError> {
        if self.leaf_to_subgroup.contains_key(&member.leaf_id) {
            return Err(FanoutError::DuplicateLeafMember {
                leaf_id: member.leaf_id,
            });
        }
        let Some(subgroup) = self.subgroups.get(&subgroup_id) else {
            return Err(FanoutError::UnknownSubgroup { subgroup_id });
        };
        let attempted_degree = subgroup.local_fanout() + 1;
        if attempted_degree > self.max_branching_factor {
            return Err(FanoutError::BranchingFactorExceeded {
                subgroup_id,
                attempted_degree,
                max_branching_factor: self.max_branching_factor,
            });
        }

        if let Some(subgroup_mut) = self.subgroups.get_mut(&subgroup_id)
            && let Err(pos) = subgroup_mut
                .leaf_members
                .binary_search_by_key(&member.leaf_id, |m| m.leaf_id)
        {
            subgroup_mut.leaf_members.insert(pos, member);
        }
        self.leaf_to_subgroup.insert(member.leaf_id, subgroup_id);
        Ok(())
    }

    /// Root `TaskGroupId` of this hierarchy.
    #[must_use]
    pub const fn root_group_id(&self) -> TaskGroupId {
        self.root_group_id
    }

    /// Configured maximum branching factor `B`.
    #[must_use]
    pub const fn max_branching_factor(&self) -> usize {
        self.max_branching_factor
    }

    /// Configured maximum subgroup depth.
    #[must_use]
    pub const fn max_depth(&self) -> u32 {
        self.max_depth
    }

    /// Total number of subgroups (`root` + intermediates).
    #[must_use]
    pub fn subgroup_count(&self) -> usize {
        self.subgroups.len()
    }

    /// Total number of registered leaf members across all subgroups.
    #[must_use]
    pub fn leaf_count(&self) -> usize {
        self.leaf_to_subgroup.len()
    }

    /// Looks up a [`SubgroupNode`] by its `TaskGroupId`.
    #[must_use]
    pub fn subgroup(&self, group_id: TaskGroupId) -> Option<&SubgroupNode> {
        self.subgroups.get(&group_id)
    }

    /// Returns the owning subgroup `TaskGroupId` for `leaf_id`.
    #[must_use]
    pub fn owning_subgroup(&self, leaf_id: FanoutLeafId) -> Option<TaskGroupId> {
        self.leaf_to_subgroup.get(&leaf_id).copied()
    }

    /// Computes topology degree and edge metrics proving `O(B)` local degree
    /// and `O(N)` total tree edges (`[DIST-TASK-003]`).
    #[must_use]
    pub fn complexity_metrics(&self) -> TopologyComplexityMetrics {
        let mut observed_max_depth = 0u32;
        let mut max_local_subgroup_degree = 0usize;
        for node in self.subgroups.values() {
            observed_max_depth = observed_max_depth.max(node.depth);
            max_local_subgroup_degree = max_local_subgroup_degree.max(node.local_fanout());
        }
        let root_coordinator_degree = self
            .subgroups
            .get(&self.root_group_id)
            .map_or(0, SubgroupNode::local_fanout);
        let subgroup_edges = u64::try_from(self.subgroups.len().saturating_sub(1)).unwrap_or(0);
        let leaf_edges = u64::try_from(self.leaf_to_subgroup.len()).unwrap_or(0);
        let n = leaf_edges;
        let all_to_all = n.saturating_mul(n.saturating_sub(1)) / 2;

        TopologyComplexityMetrics {
            subgroup_count: self.subgroups.len(),
            total_leaves: self.leaf_to_subgroup.len(),
            observed_max_depth,
            max_local_subgroup_degree,
            root_coordinator_degree,
            hierarchical_edge_count: subgroup_edges.saturating_add(leaf_edges),
            avoided_all_to_all_edges: all_to_all,
        }
    }
}

fn detect_directed_cycle(
    node: TaskGroupId,
    children_map: &BTreeMap<TaskGroupId, BTreeSet<TaskGroupId>>,
    visit_state: &mut BTreeMap<TaskGroupId, u8>,
) -> Option<TaskGroupId> {
    visit_state.insert(node, 1);
    if let Some(children) = children_map.get(&node) {
        for &child in children {
            match visit_state.get(&child).copied().unwrap_or(0) {
                1 => return Some(child),
                0 => {
                    if let Some(found) = detect_directed_cycle(child, children_map, visit_state) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
    }
    visit_state.insert(node, 2);
    None
}

fn derive_intermediate_subgroup_id(
    root_group_id: TaskGroupId,
    level_from_bottom: u32,
    chunk_index: usize,
) -> TaskGroupId {
    let mut nonce = 0u32;
    loop {
        let mut hasher = Sha256::new();
        hasher.update(FANOUT_DERIVED_SUBGROUP_DOMAIN);
        hasher.update([0u8]);
        hasher.update(root_group_id.as_bytes());
        hasher.update(level_from_bottom.to_be_bytes());
        hasher.update(u64::try_from(chunk_index).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(nonce.to_be_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        let mut id_bytes = [0u8; 16];
        id_bytes.copy_from_slice(&digest[..16]);
        let candidate = TaskGroupId::from_bytes(id_bytes);
        if candidate != root_group_id {
            return candidate;
        }
        nonce = nonce.wrapping_add(1);
    }
}

/// Cached per-subgroup state maintained by [`IncrementalSubgroupCheckpoint`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubgroupCheckpointCache {
    /// Subgroup identity.
    pub subgroup_id: TaskGroupId,
    /// Whether this subgroup has pending leaf/child updates not yet folded.
    pub dirty: bool,
    /// Checkpoint sequence at which this subgroup digest was last computed.
    pub last_computed_seq: u64,
    /// Subgroup-level binary Merkle root over its direct leaves and child
    /// subgroups.
    pub merkle_root: MerkleResultRoot,
    /// Domain-separated subgroup checkpoint digest committing to subgroup
    /// metadata, `reducer_digest`, and `merkle_root`.
    pub checkpoint_digest: [u8; 32],
    /// Completed leaves in this subgroup's entire subtree.
    pub subtree_completed_leaves: usize,
    /// Total leaves in this subgroup's entire subtree.
    pub subtree_total_leaves: usize,
    /// Divergent leaves in this subgroup's entire subtree.
    pub subtree_divergent_leaves: usize,
}

/// Metrics and root digests returned by [`IncrementalSubgroupCheckpoint::checkpoint`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointUpdateReport {
    /// Monotonic checkpoint sequence number.
    pub checkpoint_seq: u64,
    /// Updated hierarchical [`MerkleResultRoot`] at `root_group_id`.
    pub root_merkle_result: MerkleResultRoot,
    /// Updated root subgroup checkpoint digest.
    pub root_checkpoint_digest: [u8; 32],
    /// Number of dirty subgroups recomputed during this checkpoint pass.
    pub dirty_subgroups_recomputed: usize,
    /// Number of clean subgroups whose cached digests were reused without
    /// hashing.
    pub cached_subgroups_reused: usize,
    /// Number of dirty leaf hashes recomputed during this pass.
    pub leaf_hashes_computed: usize,
    /// Number of binary internal Merkle nodes hashed across dirty subgroups.
    pub internal_merkle_nodes_hashed: usize,
    /// Sum of `leaf_hashes_computed + internal_merkle_nodes_hashed`
    /// (`O(B * depth)` for single-leaf updates).
    pub total_hashed_nodes: usize,
    /// Total leaves registered in the tree.
    pub total_leaves: usize,
    /// Total leaves that have reported at least one candidate receipt.
    pub completed_leaves: usize,
    /// Total leaves currently holding divergent candidate receipts.
    pub divergent_leaves: usize,
}

/// Incremental `O(B * depth)` subgroup checkpoint engine over a
/// [`HierarchicalFanoutTree`] (`[DIST-TASK-003]`).
#[derive(Clone, Debug)]
pub struct IncrementalSubgroupCheckpoint {
    tree: HierarchicalFanoutTree,
    reducer_digest: [u8; 32],
    reducer_kind: LocalReducerKind,
    checkpoint_seq: u64,
    leaf_candidates: BTreeMap<FanoutLeafId, BTreeSet<LeafCandidateReceipt>>,
    cached_leaf_hashes: BTreeMap<FanoutLeafId, [u8; 32]>,
    dirty_leaves: BTreeSet<FanoutLeafId>,
    subgroup_caches: BTreeMap<TaskGroupId, SubgroupCheckpointCache>,
    subgroups_by_depth_desc: Vec<TaskGroupId>,
}

impl IncrementalSubgroupCheckpoint {
    /// Initializes an incremental checkpoint tracker over `tree` and performs
    /// the baseline (`checkpoint_seq = 0`) initialization.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::ZeroReducerDigest`] if `reducer_digest` is all
    /// zeros.
    pub fn new(
        tree: HierarchicalFanoutTree,
        reducer_digest: [u8; 32],
        reducer_kind: LocalReducerKind,
    ) -> Result<Self, FanoutError> {
        if reducer_digest == [0u8; 32] {
            return Err(FanoutError::ZeroReducerDigest);
        }

        let mut cached_leaf_hashes = BTreeMap::new();
        let mut dirty_leaves = BTreeSet::new();
        for &leaf_id in tree.leaf_to_subgroup.keys() {
            cached_leaf_hashes.insert(leaf_id, compute_pending_leaf_hash(&reducer_digest, leaf_id));
            dirty_leaves.insert(leaf_id);
        }

        let mut depth_pairs: Vec<(u32, TaskGroupId)> = tree
            .subgroups
            .values()
            .map(|node| (node.depth, node.group_id))
            .collect();
        depth_pairs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let subgroups_by_depth_desc: Vec<TaskGroupId> =
            depth_pairs.into_iter().map(|(_, gid)| gid).collect();

        let empty_root = compute_empty_merkle_root(&reducer_digest);
        let mut subgroup_caches = BTreeMap::new();
        for &gid in &subgroups_by_depth_desc {
            subgroup_caches.insert(
                gid,
                SubgroupCheckpointCache {
                    subgroup_id: gid,
                    dirty: true,
                    last_computed_seq: 0,
                    merkle_root: empty_root,
                    checkpoint_digest: [0u8; 32],
                    subtree_completed_leaves: 0,
                    subtree_total_leaves: 0,
                    subtree_divergent_leaves: 0,
                },
            );
        }

        let mut engine = Self {
            tree,
            reducer_digest,
            reducer_kind,
            checkpoint_seq: 0,
            leaf_candidates: BTreeMap::new(),
            cached_leaf_hashes,
            dirty_leaves,
            subgroup_caches,
            subgroups_by_depth_desc,
        };
        let _ = engine.checkpoint();
        Ok(engine)
    }

    /// Borrows the underlying [`HierarchicalFanoutTree`].
    #[must_use]
    pub const fn tree(&self) -> &HierarchicalFanoutTree {
        &self.tree
    }

    /// Bound immutable reducer digest.
    #[must_use]
    pub const fn reducer_digest(&self) -> &[u8; 32] {
        &self.reducer_digest
    }

    /// Inspects the cached checkpoint record for `subgroup_id`.
    #[must_use]
    pub fn subgroup_cache(&self, subgroup_id: TaskGroupId) -> Option<&SubgroupCheckpointCache> {
        self.subgroup_caches.get(&subgroup_id)
    }

    /// Records a candidate receipt for `candidate.leaf_id` and marks ONLY its
    /// ancestor subgroup chain (`leaf -> ... -> root`) dirty.
    ///
    /// Returns `Ok(true)` if new state was added and marked dirty, or
    /// `Ok(false)` on an idempotent duplicate candidate replay.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::UnknownLeafMember`] if `candidate.leaf_id` is not
    /// registered in the tree.
    pub fn record_leaf_candidate(
        &mut self,
        candidate: LeafCandidateReceipt,
    ) -> Result<bool, FanoutError> {
        let Some(mut cursor_group) = self.tree.owning_subgroup(candidate.leaf_id) else {
            return Err(FanoutError::UnknownLeafMember {
                leaf_id: candidate.leaf_id,
            });
        };

        let inserted = self
            .leaf_candidates
            .entry(candidate.leaf_id)
            .or_default()
            .insert(candidate);
        if !inserted {
            return Ok(false);
        }

        self.dirty_leaves.insert(candidate.leaf_id);
        loop {
            if let Some(cache) = self.subgroup_caches.get_mut(&cursor_group) {
                cache.dirty = true;
            }
            let parent = self
                .tree
                .subgroup(cursor_group)
                .and_then(|node| node.parent_group_id);
            match parent {
                Some(parent_id) => cursor_group = parent_id,
                None => break,
            }
        }
        Ok(true)
    }

    /// Recomputes only dirty leaf hashes and dirty ancestor subgroups from
    /// bottom to top (`O(B * depth)`), reusing cached digests for all untouched
    /// sibling subtrees.
    pub fn checkpoint(&mut self) -> CheckpointUpdateReport {
        self.checkpoint_seq = self.checkpoint_seq.saturating_add(1);
        let seq = self.checkpoint_seq;
        let leaf_hashes_computed = self.refresh_dirty_leaf_hashes();

        let mut dirty_subgroups_recomputed = 0usize;
        let mut cached_subgroups_reused = 0usize;
        let mut internal_merkle_nodes_hashed = 0usize;

        for idx in 0..self.subgroups_by_depth_desc.len() {
            let gid = self.subgroups_by_depth_desc[idx];
            if !self.subgroup_caches.get(&gid).is_some_and(|c| c.dirty) {
                cached_subgroups_reused += 1;
                continue;
            }
            internal_merkle_nodes_hashed += self.recompute_dirty_subgroup(gid, seq);
            dirty_subgroups_recomputed += 1;
        }

        let root_cache = self
            .subgroup_caches
            .get(&self.tree.root_group_id())
            .cloned()
            .unwrap_or(SubgroupCheckpointCache {
                subgroup_id: self.tree.root_group_id(),
                dirty: false,
                last_computed_seq: seq,
                merkle_root: compute_empty_merkle_root(&self.reducer_digest),
                checkpoint_digest: [0u8; 32],
                subtree_completed_leaves: 0,
                subtree_total_leaves: 0,
                subtree_divergent_leaves: 0,
            });

        CheckpointUpdateReport {
            checkpoint_seq: seq,
            root_merkle_result: root_cache.merkle_root,
            root_checkpoint_digest: root_cache.checkpoint_digest,
            dirty_subgroups_recomputed,
            cached_subgroups_reused,
            leaf_hashes_computed,
            internal_merkle_nodes_hashed,
            total_hashed_nodes: leaf_hashes_computed + internal_merkle_nodes_hashed,
            total_leaves: root_cache.subtree_total_leaves,
            completed_leaves: root_cache.subtree_completed_leaves,
            divergent_leaves: root_cache.subtree_divergent_leaves,
        }
    }

    fn refresh_dirty_leaf_hashes(&mut self) -> usize {
        let dirty_leaf_list: Vec<FanoutLeafId> = self.dirty_leaves.iter().copied().collect();
        self.dirty_leaves.clear();
        let count = dirty_leaf_list.len();
        for leaf_id in dirty_leaf_list {
            let hash = if let Some(candidates) = self.leaf_candidates.get(&leaf_id) {
                let entry = MerkleLeafEntry {
                    leaf_id,
                    candidates: candidates.iter().copied().collect(),
                };
                compute_merkle_leaf_hash(&self.reducer_digest, &entry)
            } else {
                compute_pending_leaf_hash(&self.reducer_digest, leaf_id)
            };
            self.cached_leaf_hashes.insert(leaf_id, hash);
        }
        count
    }

    fn recompute_dirty_subgroup(&mut self, gid: TaskGroupId, seq: u64) -> usize {
        let Some(node) = self.tree.subgroup(gid) else {
            return 0;
        };
        let mut item_hashes = Vec::with_capacity(node.local_fanout());
        let mut subtree_completed = 0usize;
        let mut subtree_total = node.leaf_members.len();
        let mut subtree_divergent = 0usize;

        for member in &node.leaf_members {
            if let Some(&h) = self.cached_leaf_hashes.get(&member.leaf_id) {
                item_hashes.push(h);
            }
            if let Some(set) = self.leaf_candidates.get(&member.leaf_id) {
                subtree_completed += 1;
                if set.len() > 1 {
                    subtree_divergent += 1;
                }
            }
        }
        for &child_gid in &node.child_subgroups {
            if let Some(child_cache) = self.subgroup_caches.get(&child_gid) {
                item_hashes.push(*child_cache.merkle_root.as_bytes());
                subtree_completed += child_cache.subtree_completed_leaves;
                subtree_total += child_cache.subtree_total_leaves;
                subtree_divergent += child_cache.subtree_divergent_leaves;
            }
        }

        let (merkle_root, hashed_internal) =
            fold_binary_merkle_hashes(&self.reducer_digest, &item_hashes);
        let checkpoint_digest = compute_subgroup_checkpoint_digest(
            gid,
            node.depth,
            &self.reducer_digest,
            self.reducer_kind,
            &merkle_root,
            subtree_completed,
            subtree_total,
            subtree_divergent,
        );

        if let Some(cache) = self.subgroup_caches.get_mut(&gid) {
            cache.dirty = false;
            cache.last_computed_seq = seq;
            cache.merkle_root = merkle_root;
            cache.checkpoint_digest = checkpoint_digest;
            cache.subtree_completed_leaves = subtree_completed;
            cache.subtree_total_leaves = subtree_total;
            cache.subtree_divergent_leaves = subtree_divergent;
        }
        hashed_internal
    }

    /// Performs a clean `O(N)` full-tree recomputation from scratch without
    /// mutating cached state, returning `(root_merkle, root_checkpoint_digest,
    /// full_hashed_nodes)` so tests can mechanically verify bit-for-bit
    /// equivalence against incremental checkpoints.
    #[must_use]
    pub fn recompute_full_reference(&self) -> (MerkleResultRoot, [u8; 32], usize) {
        let mut full_hashed_nodes = 0usize;
        let mut temp_roots: BTreeMap<
            TaskGroupId,
            (MerkleResultRoot, [u8; 32], usize, usize, usize),
        > = BTreeMap::new();

        for &gid in &self.subgroups_by_depth_desc {
            let Some(node) = self.tree.subgroup(gid) else {
                continue;
            };
            let mut item_hashes = Vec::with_capacity(node.local_fanout());
            let mut subtree_completed = 0usize;
            let mut subtree_total = node.leaf_members.len();
            let mut subtree_divergent = 0usize;

            for member in &node.leaf_members {
                let leaf_hash = if let Some(set) = self.leaf_candidates.get(&member.leaf_id) {
                    subtree_completed += 1;
                    if set.len() > 1 {
                        subtree_divergent += 1;
                    }
                    let entry = MerkleLeafEntry {
                        leaf_id: member.leaf_id,
                        candidates: set.iter().copied().collect(),
                    };
                    compute_merkle_leaf_hash(&self.reducer_digest, &entry)
                } else {
                    compute_pending_leaf_hash(&self.reducer_digest, member.leaf_id)
                };
                full_hashed_nodes += 1;
                item_hashes.push(leaf_hash);
            }

            for &child_gid in &node.child_subgroups {
                if let Some((c_root, _, c_comp, c_tot, c_div)) = temp_roots.get(&child_gid) {
                    item_hashes.push(*c_root.as_bytes());
                    subtree_completed += *c_comp;
                    subtree_total += *c_tot;
                    subtree_divergent += *c_div;
                }
            }

            let (merkle_root, hashed_internal) =
                fold_binary_merkle_hashes(&self.reducer_digest, &item_hashes);
            full_hashed_nodes += hashed_internal;
            let digest = compute_subgroup_checkpoint_digest(
                gid,
                node.depth,
                &self.reducer_digest,
                self.reducer_kind,
                &merkle_root,
                subtree_completed,
                subtree_total,
                subtree_divergent,
            );
            temp_roots.insert(
                gid,
                (
                    merkle_root,
                    digest,
                    subtree_completed,
                    subtree_total,
                    subtree_divergent,
                ),
            );
        }

        let (root_merkle, root_digest, _, _, _) = temp_roots
            .get(&self.tree.root_group_id())
            .copied()
            .unwrap_or((
                compute_empty_merkle_root(&self.reducer_digest),
                [0u8; 32],
                0,
                0,
                0,
            ));
        (root_merkle, root_digest, full_hashed_nodes)
    }

    /// Generates a binary [`MerkleInclusionProof`] from a reported leaf all the
    /// way up the subgroup hierarchy to the root [`MerkleResultRoot`],
    /// verifiable with [`verify_merkle_proof`].
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::UnknownLeafMember`] if `leaf_id` is not in the
    /// tree, or [`FanoutError::LeafNotYetReported`] if `leaf_id` has not yet
    /// reported a candidate receipt.
    pub fn prove_leaf_to_root(
        &mut self,
        leaf_id: FanoutLeafId,
    ) -> Result<MerkleInclusionProof, FanoutError> {
        let Some(owning_group) = self.tree.owning_subgroup(leaf_id) else {
            return Err(FanoutError::UnknownLeafMember { leaf_id });
        };
        let Some(candidate_set) = self.leaf_candidates.get(&leaf_id) else {
            return Err(FanoutError::LeafNotYetReported { leaf_id });
        };
        let leaf_entry = MerkleLeafEntry {
            leaf_id,
            candidates: candidate_set.iter().copied().collect(),
        };

        // Ensure cached subgroup Merkle roots are up-to-date before extracting
        // sibling hashes.
        if self
            .subgroup_caches
            .get(&self.tree.root_group_id())
            .is_some_and(|c| c.dirty)
            || !self.dirty_leaves.is_empty()
        {
            let _ = self.checkpoint();
        }

        let mut all_steps = Vec::new();
        let mut cursor_group = owning_group;
        let mut child_group_for_parent: Option<TaskGroupId> = None;

        while let Some(node) = self.tree.subgroup(cursor_group) {
            let mut item_hashes = Vec::with_capacity(node.local_fanout());
            let mut target_idx = 0usize;

            for (idx, member) in node.leaf_members.iter().enumerate() {
                if let Some(&h) = self.cached_leaf_hashes.get(&member.leaf_id) {
                    item_hashes.push(h);
                }
                if child_group_for_parent.is_none() && member.leaf_id == leaf_id {
                    target_idx = idx;
                }
            }
            for (offset, &child_gid) in node.child_subgroups.iter().enumerate() {
                if let Some(child_cache) = self.subgroup_caches.get(&child_gid) {
                    item_hashes.push(*child_cache.merkle_root.as_bytes());
                }
                if child_group_for_parent == Some(child_gid) {
                    target_idx = node.leaf_members.len() + offset;
                }
            }

            let mut local_steps = build_binary_merkle_steps(&item_hashes, target_idx);
            all_steps.append(&mut local_steps);

            match node.parent_group_id {
                Some(parent_id) => {
                    child_group_for_parent = Some(cursor_group);
                    cursor_group = parent_id;
                }
                None => break,
            }
        }

        Ok(MerkleInclusionProof {
            leaf_entry,
            steps: all_steps,
        })
    }

    /// Returns all leaves that currently hold conflicting candidate receipts in
    /// canonical sorted order.
    #[must_use]
    pub fn divergent_leaves(&self) -> Vec<DivergentLeafRecord> {
        self.leaf_candidates
            .iter()
            .filter(|(_, set)| set.len() > 1)
            .map(|(&leaf_id, set)| DivergentLeafRecord {
                leaf_id,
                candidates: set.iter().copied().collect(),
            })
            .collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_subgroup_checkpoint_digest(
    subgroup_id: TaskGroupId,
    depth: u32,
    reducer_digest: &[u8; 32],
    reducer_kind: LocalReducerKind,
    merkle_root: &MerkleResultRoot,
    subtree_completed: usize,
    subtree_total: usize,
    subtree_divergent: usize,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(FANOUT_SUBGROUP_CHECKPOINT_DOMAIN);
    hasher.update([0u8]);
    hasher.update(subgroup_id.as_bytes());
    hasher.update(depth.to_be_bytes());
    hasher.update(reducer_digest);
    hasher.update([reducer_kind.tag()]);
    if let LocalReducerKind::ProvenanceBoundJudgment {
        judgment_policy_digest,
    } = reducer_kind
    {
        hasher.update(judgment_policy_digest);
    }
    hasher.update(merkle_root.as_bytes());
    hasher.update(
        u64::try_from(subtree_completed)
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hasher.update(
        u64::try_from(subtree_total)
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hasher.update(
        u64::try_from(subtree_divergent)
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hasher.finalize().into()
}

// ============================================================================
// 2. [DIST-FANOUT-001] Multi-Axis Spawn/Fanout Admission Gate & Backpressure
// ============================================================================

/// Normative quota axes enforced by [`FanoutUsageLedger`] (`[DIST-FANOUT-001]`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FanoutAxis {
    /// Subgroup hierarchy depth (`max_group_depth`).
    GroupDepth,
    /// Cumulative spawned children (`max_total_children`).
    TotalChildren,
    /// Concurrently active agents/attempts (`max_active_agents`).
    ActiveAgents,
    /// Inter-agent / channel message count (`max_message_count`).
    MessageCount,
    /// Inter-agent / channel message payload bytes (`max_message_bytes`).
    MessageBytes,
    /// Context and Artifact read bytes (`max_context_read_bytes`).
    ContextReadBytes,
    /// Proposed / committed write bytes (`max_write_bytes`).
    WriteBytes,
    /// Planned external effect slots (`max_external_effects`).
    ExternalEffects,
}

/// Administrative state of a fanout `TaskGroup`'s quota gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanoutGroupLifecycle {
    /// Group admits new spawn/fanout reservations.
    Open,
    /// Group membership is sealed (`[TASK-GROUP-002]`); new fanout is refused.
    Sealed,
    /// Group was cancelled (`[TASK-CANCEL-001]`); new fanout is refused.
    Cancelled,
    /// Group exceeded its failure budget or integrity fence; new fanout is
    /// refused.
    Poisoned,
}

/// Six-family multi-axis quota contract for a `TaskGroup` (`[DIST-FANOUT-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FanoutQuotaContract {
    /// 1. Maximum allowed subgroup / spawn depth (`root = 0`).
    pub max_group_depth: u32,
    /// 2a. Maximum cumulative child spawns permitted in this group.
    pub max_total_children: u32,
    /// 2b. Maximum concurrently active agents/attempts permitted.
    pub max_active_agents: u32,
    /// 3a. Maximum cumulative message count.
    pub max_message_count: u64,
    /// 3b. Maximum cumulative message bytes.
    pub max_message_bytes: u64,
    /// 4. Maximum cumulative Context / Artifact read bytes.
    pub max_context_read_bytes: u64,
    /// 5. Maximum cumulative write bytes.
    pub max_write_bytes: u64,
    /// 6. Maximum cumulative external effects.
    pub max_external_effects: u32,
    /// Maximum waiting backpressure queue entries before shedding new waiters.
    pub max_backpressure_queue_len: usize,
    /// Base retry-after hint (in milliseconds) surfaced on backpressure.
    pub base_retry_after_ms: u64,
}

/// Remaining reservable capacity across all quota axes after accounting for
/// committed usage and active reservations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FanoutHeadroom {
    /// Remaining cumulative child spawns (`max_total_children - used`).
    pub remaining_total_children: u32,
    /// Remaining concurrent active agent slots (`max_active_agents - active`).
    pub remaining_active_agents: u32,
    /// Remaining message count budget.
    pub remaining_message_count: u64,
    /// Remaining message byte budget.
    pub remaining_message_bytes: u64,
    /// Remaining Context / read byte budget.
    pub remaining_context_read_bytes: u64,
    /// Remaining write byte budget.
    pub remaining_write_bytes: u64,
    /// Remaining external effects budget.
    pub remaining_external_effects: u32,
}

/// Pre-reservation request submitted before spawning child work or dispatching
/// fanout communication/effects (`[DIST-FANOUT-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FanoutReservationRequest {
    /// Deterministic reservation identity.
    pub reservation_id: ReservationId,
    /// Idempotency key of the admission request.
    pub idempotency_key: IdempotencyKey,
    /// Target subgroup / spawn depth (`root = 0`).
    pub requested_depth: u32,
    /// Number of child entities to spawn under this reservation.
    pub child_spawns: u32,
    /// Concurrent active agent slots held while this reservation is active.
    pub active_agents: u32,
    /// Pre-reserved message count.
    pub reserved_messages: u64,
    /// Pre-reserved message payload bytes.
    pub reserved_message_bytes: u64,
    /// Pre-reserved Context / read bytes.
    pub reserved_context_read_bytes: u64,
    /// Pre-reserved write bytes.
    pub reserved_write_bytes: u64,
    /// Pre-reserved external effect slots.
    pub reserved_external_effects: u32,
}

/// Actual resource consumption reported when closing an active reservation.
///
/// Unused reserved budget (`reserved - actual`) is deterministically refunded
/// to the ledger's headroom, and held `active_agents` slots are released.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FanoutActualUsage {
    /// Actual children materialized (`<= reserved.child_spawns`).
    pub actual_child_spawns: u32,
    /// Actual messages emitted (`<= reserved.reserved_messages`).
    pub actual_messages: u64,
    /// Actual message bytes emitted (`<= reserved.reserved_message_bytes`).
    pub actual_message_bytes: u64,
    /// Actual Context / read bytes consumed
    /// (`<= reserved.reserved_context_read_bytes`).
    pub actual_context_read_bytes: u64,
    /// Actual write bytes consumed (`<= reserved.reserved_write_bytes`).
    pub actual_write_bytes: u64,
    /// Actual external effects dispatched
    /// (`<= reserved.reserved_external_effects`).
    pub actual_external_effects: u32,
}

/// Non-retryable refusal reasons returned by
/// [`FanoutAdmissionDecision::HardRefused`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanoutHardRefusalReason {
    /// Requested spawn depth exceeds `contract.max_group_depth`.
    MaxGroupDepthExceeded {
        /// Requested depth.
        requested_depth: u32,
        /// Contract maximum depth.
        max_group_depth: u32,
    },
    /// Group membership is sealed (`[TASK-GROUP-002]`).
    GroupSealed,
    /// Group has been cancelled (`[TASK-CANCEL-001]`).
    GroupCancelled,
    /// Group has been poisoned / fenced due to failure budget exhaustion.
    GroupPoisoned,
    /// Waiting backpressure queue is itself at `max_backpressure_queue_len`.
    BackpressureQueueFull {
        /// Configured backpressure queue bound.
        max_backpressure_queue_len: usize,
    },
}

/// Monotonic ticket assigned to a backpressured admission request waiting in
/// [`FanoutUsageLedger`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FanoutWaitTicketId(pub u64);

/// Admission outcome produced by [`FanoutUsageLedger::request_admission`]
/// (`[DIST-FANOUT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FanoutAdmissionDecision {
    /// Quota pre-reserved; caller may proceed within its reservation.
    Admitted {
        /// Active reservation identity.
        reservation_id: ReservationId,
        /// Remaining group headroom immediately after this reservation.
        remaining_headroom: FanoutHeadroom,
    },
    /// One or more resource axes are saturated; request entered structured
    /// `WAITING_RESOURCE` backpressure instead of runaway recursion.
    WaitingResourceBackpressure {
        /// Deterministic FIFO wait ticket in the ledger's backpressure queue.
        wait_ticket_id: FanoutWaitTicketId,
        /// All axes that lacked sufficient headroom in canonical order.
        saturated_axes: Vec<FanoutAxis>,
        /// Optional retry-after hint in milliseconds.
        retry_after_hint_ms: Option<u64>,
    },
    /// Request cannot succeed even after resource release (e.g. structural
    /// depth exceeded or cancelled/poisoned group).
    HardRefused {
        /// Typed refusal cause.
        reason: FanoutHardRefusalReason,
    },
}

/// Outcome of settling or cancelling an active reservation, including any
/// waiting requests deterministically promoted out of backpressure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanoutSettlementReport {
    /// Updated headroom after settlement/refund and any FIFO promotions.
    pub updated_headroom: FanoutHeadroom,
    /// Waiting reservations promoted to `Admitted` in deterministic FIFO order.
    pub unblocked_reservations: Vec<(FanoutWaitTicketId, ReservationId)>,
}

/// Multi-axis spawn/fanout quota reservation ledger and backpressure gate
/// (`[DIST-FANOUT-001]`).
#[derive(Clone, Debug)]
pub struct FanoutUsageLedger {
    group_id: TaskGroupId,
    contract: FanoutQuotaContract,
    lifecycle: FanoutGroupLifecycle,
    committed_children: u32,
    committed_messages: u64,
    committed_message_bytes: u64,
    committed_context_read_bytes: u64,
    committed_write_bytes: u64,
    committed_external_effects: u32,
    reserved_children: u32,
    active_agents: u32,
    reserved_messages: u64,
    reserved_message_bytes: u64,
    reserved_context_read_bytes: u64,
    reserved_write_bytes: u64,
    reserved_external_effects: u32,
    active_reservations: BTreeMap<ReservationId, FanoutReservationRequest>,
    waiting_queue: VecDeque<(FanoutWaitTicketId, FanoutReservationRequest)>,
    next_wait_ticket_seq: u64,
}

impl FanoutUsageLedger {
    /// Creates a new open [`FanoutUsageLedger`] governed by `contract`.
    #[must_use]
    pub fn new(group_id: TaskGroupId, contract: FanoutQuotaContract) -> Self {
        Self {
            group_id,
            contract,
            lifecycle: FanoutGroupLifecycle::Open,
            committed_children: 0,
            committed_messages: 0,
            committed_message_bytes: 0,
            committed_context_read_bytes: 0,
            committed_write_bytes: 0,
            committed_external_effects: 0,
            reserved_children: 0,
            active_agents: 0,
            reserved_messages: 0,
            reserved_message_bytes: 0,
            reserved_context_read_bytes: 0,
            reserved_write_bytes: 0,
            reserved_external_effects: 0,
            active_reservations: BTreeMap::new(),
            waiting_queue: VecDeque::new(),
            next_wait_ticket_seq: 0,
        }
    }

    /// Bound `TaskGroupId`.
    #[must_use]
    pub const fn group_id(&self) -> TaskGroupId {
        self.group_id
    }

    /// Governing [`FanoutQuotaContract`].
    #[must_use]
    pub const fn contract(&self) -> &FanoutQuotaContract {
        &self.contract
    }

    /// Current [`FanoutGroupLifecycle`] state.
    #[must_use]
    pub const fn lifecycle(&self) -> FanoutGroupLifecycle {
        self.lifecycle
    }

    /// Transitions the group's administrative lifecycle. Cancelling or
    /// poisoning clears queued backpressure waiters so blocked callers fail
    /// closed rather than hanging indefinitely.
    pub fn set_lifecycle(&mut self, lifecycle: FanoutGroupLifecycle) -> usize {
        self.lifecycle = lifecycle;
        if matches!(
            lifecycle,
            FanoutGroupLifecycle::Cancelled | FanoutGroupLifecycle::Poisoned
        ) {
            let dropped = self.waiting_queue.len();
            self.waiting_queue.clear();
            dropped
        } else {
            0
        }
    }

    /// Number of currently active reservations.
    #[must_use]
    pub fn active_reservation_count(&self) -> usize {
        self.active_reservations.len()
    }

    /// Number of currently held concurrent active agent slots.
    #[must_use]
    pub const fn active_agents(&self) -> u32 {
        self.active_agents
    }

    /// Number of requests currently waiting in `WAITING_RESOURCE` backpressure.
    #[must_use]
    pub fn waiting_backpressure_count(&self) -> usize {
        self.waiting_queue.len()
    }

    /// Computes current remaining [`FanoutHeadroom`].
    #[must_use]
    pub fn headroom(&self) -> FanoutHeadroom {
        let used_children = self
            .committed_children
            .saturating_add(self.reserved_children);
        let used_messages = self
            .committed_messages
            .saturating_add(self.reserved_messages);
        let used_message_bytes = self
            .committed_message_bytes
            .saturating_add(self.reserved_message_bytes);
        let used_context = self
            .committed_context_read_bytes
            .saturating_add(self.reserved_context_read_bytes);
        let used_write = self
            .committed_write_bytes
            .saturating_add(self.reserved_write_bytes);
        let used_effects = self
            .committed_external_effects
            .saturating_add(self.reserved_external_effects);

        FanoutHeadroom {
            remaining_total_children: self
                .contract
                .max_total_children
                .saturating_sub(used_children),
            remaining_active_agents: self
                .contract
                .max_active_agents
                .saturating_sub(self.active_agents),
            remaining_message_count: self
                .contract
                .max_message_count
                .saturating_sub(used_messages),
            remaining_message_bytes: self
                .contract
                .max_message_bytes
                .saturating_sub(used_message_bytes),
            remaining_context_read_bytes: self
                .contract
                .max_context_read_bytes
                .saturating_sub(used_context),
            remaining_write_bytes: self.contract.max_write_bytes.saturating_sub(used_write),
            remaining_external_effects: self
                .contract
                .max_external_effects
                .saturating_sub(used_effects),
        }
    }

    /// Evaluates and pre-reserves quota across all six normative dimensions
    /// (`[DIST-FANOUT-001]`).
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::ConflictingReservationRequest`] if
    /// `request.reservation_id` is already active or queued with different
    /// parameters.
    pub fn request_admission(
        &mut self,
        request: FanoutReservationRequest,
    ) -> Result<FanoutAdmissionDecision, FanoutError> {
        if let Some(existing) = self.active_reservations.get(&request.reservation_id) {
            if *existing == request {
                return Ok(FanoutAdmissionDecision::Admitted {
                    reservation_id: request.reservation_id,
                    remaining_headroom: self.headroom(),
                });
            }
            return Err(FanoutError::ConflictingReservationRequest {
                reservation_id: request.reservation_id,
            });
        }
        for (ticket, queued) in &self.waiting_queue {
            if queued.reservation_id == request.reservation_id {
                if *queued == request {
                    let saturated_axes = self.saturated_axes_for(&request);
                    return Ok(FanoutAdmissionDecision::WaitingResourceBackpressure {
                        wait_ticket_id: *ticket,
                        saturated_axes,
                        retry_after_hint_ms: self.compute_retry_hint_ms(),
                    });
                }
                return Err(FanoutError::ConflictingReservationRequest {
                    reservation_id: request.reservation_id,
                });
            }
        }

        match self.lifecycle {
            FanoutGroupLifecycle::Open => {}
            FanoutGroupLifecycle::Sealed => {
                return Ok(FanoutAdmissionDecision::HardRefused {
                    reason: FanoutHardRefusalReason::GroupSealed,
                });
            }
            FanoutGroupLifecycle::Cancelled => {
                return Ok(FanoutAdmissionDecision::HardRefused {
                    reason: FanoutHardRefusalReason::GroupCancelled,
                });
            }
            FanoutGroupLifecycle::Poisoned => {
                return Ok(FanoutAdmissionDecision::HardRefused {
                    reason: FanoutHardRefusalReason::GroupPoisoned,
                });
            }
        }

        if request.requested_depth > self.contract.max_group_depth {
            return Ok(FanoutAdmissionDecision::HardRefused {
                reason: FanoutHardRefusalReason::MaxGroupDepthExceeded {
                    requested_depth: request.requested_depth,
                    max_group_depth: self.contract.max_group_depth,
                },
            });
        }

        let saturated_axes = self.saturated_axes_for(&request);
        if saturated_axes.is_empty() && self.waiting_queue.is_empty() {
            self.apply_reservation(request);
            return Ok(FanoutAdmissionDecision::Admitted {
                reservation_id: request.reservation_id,
                remaining_headroom: self.headroom(),
            });
        }

        if self.waiting_queue.len() >= self.contract.max_backpressure_queue_len {
            return Ok(FanoutAdmissionDecision::HardRefused {
                reason: FanoutHardRefusalReason::BackpressureQueueFull {
                    max_backpressure_queue_len: self.contract.max_backpressure_queue_len,
                },
            });
        }

        let reported_axes = if saturated_axes.is_empty() {
            // Preserve strict FIFO fairness behind earlier backpressured requests.
            vec![FanoutAxis::ActiveAgents]
        } else {
            saturated_axes
        };

        self.next_wait_ticket_seq = self.next_wait_ticket_seq.saturating_add(1);
        let wait_ticket_id = FanoutWaitTicketId(self.next_wait_ticket_seq);
        self.waiting_queue.push_back((wait_ticket_id, request));
        let retry_after_hint_ms = self.compute_retry_hint_ms();

        Ok(FanoutAdmissionDecision::WaitingResourceBackpressure {
            wait_ticket_id,
            saturated_axes: reported_axes,
            retry_after_hint_ms,
        })
    }

    /// Settles an active reservation with `actual` usage, releasing active
    /// agent slots, refunding unused pre-reserved quota, and promoting any
    /// newly admissible backpressured requests in deterministic FIFO order.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::UnknownActiveReservation`] if `reservation_id` is
    /// not active, or [`FanoutError::ActualUsageExceedsReservation`] if any
    /// `actual` field exceeds what was pre-reserved.
    pub fn settle_reservation(
        &mut self,
        reservation_id: ReservationId,
        actual: FanoutActualUsage,
    ) -> Result<FanoutSettlementReport, FanoutError> {
        let Some(reserved) = self.active_reservations.get(&reservation_id).copied() else {
            return Err(FanoutError::UnknownActiveReservation { reservation_id });
        };

        if actual.actual_child_spawns > reserved.child_spawns {
            return Err(FanoutError::ActualUsageExceedsReservation {
                reservation_id,
                axis: FanoutAxis::TotalChildren,
            });
        }
        if actual.actual_messages > reserved.reserved_messages {
            return Err(FanoutError::ActualUsageExceedsReservation {
                reservation_id,
                axis: FanoutAxis::MessageCount,
            });
        }
        if actual.actual_message_bytes > reserved.reserved_message_bytes {
            return Err(FanoutError::ActualUsageExceedsReservation {
                reservation_id,
                axis: FanoutAxis::MessageBytes,
            });
        }
        if actual.actual_context_read_bytes > reserved.reserved_context_read_bytes {
            return Err(FanoutError::ActualUsageExceedsReservation {
                reservation_id,
                axis: FanoutAxis::ContextReadBytes,
            });
        }
        if actual.actual_write_bytes > reserved.reserved_write_bytes {
            return Err(FanoutError::ActualUsageExceedsReservation {
                reservation_id,
                axis: FanoutAxis::WriteBytes,
            });
        }
        if actual.actual_external_effects > reserved.reserved_external_effects {
            return Err(FanoutError::ActualUsageExceedsReservation {
                reservation_id,
                axis: FanoutAxis::ExternalEffects,
            });
        }

        self.active_reservations.remove(&reservation_id);
        self.reserved_children = self.reserved_children.saturating_sub(reserved.child_spawns);
        self.active_agents = self.active_agents.saturating_sub(reserved.active_agents);
        self.reserved_messages = self
            .reserved_messages
            .saturating_sub(reserved.reserved_messages);
        self.reserved_message_bytes = self
            .reserved_message_bytes
            .saturating_sub(reserved.reserved_message_bytes);
        self.reserved_context_read_bytes = self
            .reserved_context_read_bytes
            .saturating_sub(reserved.reserved_context_read_bytes);
        self.reserved_write_bytes = self
            .reserved_write_bytes
            .saturating_sub(reserved.reserved_write_bytes);
        self.reserved_external_effects = self
            .reserved_external_effects
            .saturating_sub(reserved.reserved_external_effects);

        self.committed_children = self
            .committed_children
            .saturating_add(actual.actual_child_spawns);
        self.committed_messages = self
            .committed_messages
            .saturating_add(actual.actual_messages);
        self.committed_message_bytes = self
            .committed_message_bytes
            .saturating_add(actual.actual_message_bytes);
        self.committed_context_read_bytes = self
            .committed_context_read_bytes
            .saturating_add(actual.actual_context_read_bytes);
        self.committed_write_bytes = self
            .committed_write_bytes
            .saturating_add(actual.actual_write_bytes);
        self.committed_external_effects = self
            .committed_external_effects
            .saturating_add(actual.actual_external_effects);

        let unblocked_reservations = self.drain_admissible_waiters();
        Ok(FanoutSettlementReport {
            updated_headroom: self.headroom(),
            unblocked_reservations,
        })
    }

    /// Cancels an unconsumed active reservation, refunding 100% of its
    /// reserved budget and draining any newly admissible waiting requests.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::UnknownActiveReservation`] if `reservation_id` is
    /// not active.
    pub fn cancel_reservation(
        &mut self,
        reservation_id: ReservationId,
    ) -> Result<FanoutSettlementReport, FanoutError> {
        self.settle_reservation(reservation_id, FanoutActualUsage::default())
    }

    /// Replenishes cumulative contract ceilings (e.g., when a parent authority
    /// grants an extended `QuotaLease`) and drains newly unblocked waiters.
    pub fn extend_quota_contract(
        &mut self,
        updated_contract: FanoutQuotaContract,
    ) -> FanoutSettlementReport {
        self.contract = updated_contract;
        let unblocked_reservations = self.drain_admissible_waiters();
        FanoutSettlementReport {
            updated_headroom: self.headroom(),
            unblocked_reservations,
        }
    }

    fn saturated_axes_for(&self, request: &FanoutReservationRequest) -> Vec<FanoutAxis> {
        let headroom = self.headroom();
        let mut axes = Vec::new();
        if request.child_spawns > headroom.remaining_total_children {
            axes.push(FanoutAxis::TotalChildren);
        }
        if request.active_agents > headroom.remaining_active_agents {
            axes.push(FanoutAxis::ActiveAgents);
        }
        if request.reserved_messages > headroom.remaining_message_count {
            axes.push(FanoutAxis::MessageCount);
        }
        if request.reserved_message_bytes > headroom.remaining_message_bytes {
            axes.push(FanoutAxis::MessageBytes);
        }
        if request.reserved_context_read_bytes > headroom.remaining_context_read_bytes {
            axes.push(FanoutAxis::ContextReadBytes);
        }
        if request.reserved_write_bytes > headroom.remaining_write_bytes {
            axes.push(FanoutAxis::WriteBytes);
        }
        if request.reserved_external_effects > headroom.remaining_external_effects {
            axes.push(FanoutAxis::ExternalEffects);
        }
        axes
    }

    fn apply_reservation(&mut self, request: FanoutReservationRequest) {
        self.reserved_children = self.reserved_children.saturating_add(request.child_spawns);
        self.active_agents = self.active_agents.saturating_add(request.active_agents);
        self.reserved_messages = self
            .reserved_messages
            .saturating_add(request.reserved_messages);
        self.reserved_message_bytes = self
            .reserved_message_bytes
            .saturating_add(request.reserved_message_bytes);
        self.reserved_context_read_bytes = self
            .reserved_context_read_bytes
            .saturating_add(request.reserved_context_read_bytes);
        self.reserved_write_bytes = self
            .reserved_write_bytes
            .saturating_add(request.reserved_write_bytes);
        self.reserved_external_effects = self
            .reserved_external_effects
            .saturating_add(request.reserved_external_effects);
        self.active_reservations
            .insert(request.reservation_id, request);
    }

    fn drain_admissible_waiters(&mut self) -> Vec<(FanoutWaitTicketId, ReservationId)> {
        if self.lifecycle != FanoutGroupLifecycle::Open {
            return Vec::new();
        }
        let mut promoted = Vec::new();
        let mut remaining_waiters = VecDeque::with_capacity(self.waiting_queue.len());
        while let Some((ticket, req)) = self.waiting_queue.pop_front() {
            if self.saturated_axes_for(&req).is_empty() {
                let res_id = req.reservation_id;
                self.apply_reservation(req);
                promoted.push((ticket, res_id));
            } else {
                remaining_waiters.push_back((ticket, req));
            }
        }
        self.waiting_queue = remaining_waiters;
        promoted
    }

    fn compute_retry_hint_ms(&self) -> Option<u64> {
        if self.contract.base_retry_after_ms == 0 {
            return None;
        }
        let queue_factor = u64::try_from(self.waiting_queue.len().max(1)).unwrap_or(1);
        Some(
            self.contract
                .base_retry_after_ms
                .saturating_mul(queue_factor),
        )
    }
}

// ============================================================================
// 3. [DIST-BULKHEAD-001] Multi-Scope Bulkhead Isolation & Safety TCB Reserve
// ============================================================================

/// Nominal 16-byte Tenant identifier for bulkhead isolation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FanoutTenantId([u8; 16]);

impl FanoutTenantId {
    /// Constructs a `FanoutTenantId` from raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Constructs a `FanoutTenantId` from an `nlos_types::PrincipalId`.
    #[must_use]
    pub const fn from_principal(principal_id: PrincipalId) -> Self {
        Self(principal_id.into_bytes())
    }

    /// Borrows the 16-byte identity.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Nominal 16-byte remote model/service Provider identifier for bulkhead
/// isolation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FanoutProviderId([u8; 16]);

impl FanoutProviderId {
    /// Constructs a `FanoutProviderId` from raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Constructs a `FanoutProviderId` from an `nlos_types::DriverId`.
    #[must_use]
    pub const fn from_driver(driver_id: DriverId) -> Self {
        Self(driver_id.into_bytes())
    }

    /// Borrows the 16-byte identity.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Normative bulkhead isolation scope categories (`[DIST-BULKHEAD-001]`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BulkheadScopeKind {
    /// Tenant-level isolation pool.
    Tenant,
    /// Workspace-level isolation pool.
    Workspace,
    /// `TaskGroup`-level isolation pool.
    TaskGroup,
    /// Model / external Provider-level isolation pool.
    Provider,
}

/// Concrete bulkhead identity across the four normative workload scopes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BulkheadScopeId {
    /// Tenant bulkhead.
    Tenant(FanoutTenantId),
    /// Workspace bulkhead.
    Workspace(WorkspaceId),
    /// `TaskGroup` bulkhead.
    TaskGroup(TaskGroupId),
    /// Provider bulkhead.
    Provider(FanoutProviderId),
}

impl BulkheadScopeId {
    /// Returns the [`BulkheadScopeKind`] of this scope identity.
    #[must_use]
    pub const fn kind(self) -> BulkheadScopeKind {
        match self {
            Self::Tenant(_) => BulkheadScopeKind::Tenant,
            Self::Workspace(_) => BulkheadScopeKind::Workspace,
            Self::TaskGroup(_) => BulkheadScopeKind::TaskGroup,
            Self::Provider(_) => BulkheadScopeKind::Provider,
        }
    }
}

/// Per-bulkhead failure budget before tripping into
/// [`BulkheadHealthState::PoisonedCircuitOpen`] (`[DIST-BULKHEAD-001]`,
/// `[TASK-SCALE-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FailureBudget {
    /// Consecutive failures tolerated before opening the bulkhead circuit.
    pub max_consecutive_failures: u32,
    /// Cumulative failures tolerated before opening the bulkhead circuit.
    pub max_total_failures: u32,
}

/// Cause that tripped a bulkhead into [`BulkheadHealthState::PoisonedCircuitOpen`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkheadTripReason {
    /// `consecutive_failures` reached `max_consecutive_failures`.
    ConsecutiveFailuresExceeded {
        /// Observed consecutive failure count.
        observed: u32,
        /// Configured threshold.
        limit: u32,
    },
    /// `total_failures` reached `max_total_failures`.
    TotalFailuresExceeded {
        /// Observed cumulative failure count.
        observed: u32,
        /// Configured threshold.
        limit: u32,
    },
}

/// Circuit/health state of a single bulkhead (`[DIST-BULKHEAD-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkheadHealthState {
    /// Bulkhead admits traffic within concurrency and queue limits.
    Healthy,
    /// Bulkhead exceeded its [`FailureBudget`] and fails closed until an
    /// explicit monotonic administrative reset.
    PoisonedCircuitOpen {
        /// Threshold that tripped the circuit.
        reason: BulkheadTripReason,
        /// Timestamp (ms) when the bulkhead tripped.
        tripped_at_ms: i64,
    },
}

/// Registration contract for one isolated bulkhead (`[DIST-BULKHEAD-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkheadSpec {
    /// Scope identity (`Tenant`, `Workspace`, `TaskGroup`, or `Provider`).
    pub scope_id: BulkheadScopeId,
    /// Dedicated [`ResourceGroupId`] owned exclusively by this bulkhead.
    pub resource_group_id: ResourceGroupId,
    /// Maximum concurrent active executions inside this bulkhead (`>= 1`).
    pub max_concurrent: u32,
    /// Maximum queued requests waiting specifically on this bulkhead.
    pub max_queue_depth: u32,
    /// Failure budget governing circuit-open poisoning.
    pub failure_budget: FailureBudget,
}

/// Read-only inspection snapshot of a single bulkhead's live counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkheadStateSnapshot {
    /// Scope identity.
    pub scope_id: BulkheadScopeId,
    /// Bound exclusive [`ResourceGroupId`].
    pub resource_group_id: ResourceGroupId,
    /// Current health / circuit state.
    pub health: BulkheadHealthState,
    /// Currently active concurrent leases holding this bulkhead.
    pub active_concurrent: u32,
    /// Configured concurrency limit.
    pub max_concurrent: u32,
    /// Requests currently queued on this bulkhead's saturation queue.
    pub queued_count: u32,
    /// Configured queue limit.
    pub max_queue_depth: u32,
    /// Current consecutive failure counter.
    pub consecutive_failures: u32,
    /// Current cumulative failure counter.
    pub total_failures: u32,
    /// Monotonic administrative reset epoch.
    pub reset_epoch: u64,
}

/// Dedicated untouchable system reserve for `SafetyTcb` critical operations
/// (cancellation propagation, lease fencing, watchdog, emergency revoke)
/// (`[DIST-BULKHEAD-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SafetyTcbSystemReserve {
    /// Dedicated [`ResourceGroupId`] for `SafetyTcb` execution.
    pub resource_group_id: ResourceGroupId,
    /// Slots strictly reserved for `SafetyTcb`; non-TCB traffic can never
    /// borrow or exhaust these slots.
    pub reserved_tcb_slots: u32,
}

/// Active lease handle for a `SafetyTcb` reserved execution slot.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TcbSlotLeaseId(pub u64);

/// Active lease handle for a composite workload bulkhead admission.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BulkheadLeaseId(pub u64);

/// Monotonic queue ticket for a workload request queued on a saturated
/// bulkhead.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BulkheadQueueTicketId(pub u64);

/// Composite scope route traversed by a non-TCB workload execution
/// (`Tenant + Workspace + TaskGroup + optional Provider`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkloadBulkheadRoute {
    /// Tenant scope.
    pub tenant_id: FanoutTenantId,
    /// Workspace scope.
    pub workspace_id: WorkspaceId,
    /// `TaskGroup` scope.
    pub task_group_id: TaskGroupId,
    /// Optional external model/tool Provider scope.
    pub provider_id: Option<FanoutProviderId>,
}

impl WorkloadBulkheadRoute {
    fn scopes(&self) -> Vec<BulkheadScopeId> {
        let mut scopes = vec![
            BulkheadScopeId::Tenant(self.tenant_id),
            BulkheadScopeId::Workspace(self.workspace_id),
            BulkheadScopeId::TaskGroup(self.task_group_id),
        ];
        if let Some(provider_id) = self.provider_id {
            scopes.push(BulkheadScopeId::Provider(provider_id));
        }
        scopes
    }
}

/// Admission outcome for [`BulkheadController::acquire_workload`]
/// (`[DIST-BULKHEAD-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BulkheadAdmissionDecision {
    /// Concurrency slot acquired across every scope in the route (and within
    /// the non-TCB system ceiling).
    Acquired {
        /// Issued lease handle.
        lease_id: BulkheadLeaseId,
        /// Exclusive [`ResourceGroupId`]s bound along the route.
        bound_resource_groups: Vec<ResourceGroupId>,
    },
    /// At least one bulkhead in the route reached `max_concurrent`; the request
    /// was placed into that saturated bulkhead's bounded queue without pinning
    /// concurrency slots on healthy scopes.
    Queued {
        /// Assigned FIFO queue ticket.
        queue_ticket_id: BulkheadQueueTicketId,
        /// Primary saturated bulkhead scope gating execution.
        bottleneck_scope: BulkheadScopeId,
    },
    /// Rejected immediately because a scope in the route is
    /// [`BulkheadHealthState::PoisonedCircuitOpen`].
    RejectedPoisoned {
        /// Poisoned bulkhead scope.
        scope_id: BulkheadScopeId,
        /// Active trip cause.
        reason: BulkheadTripReason,
    },
    /// Rejected because the saturated bulkhead's bounded queue is full.
    RejectedQueueFull {
        /// Saturated bulkhead scope whose queue reached `max_queue_depth`.
        saturated_scope: BulkheadScopeId,
    },
    /// Rejected because the shared non-TCB system slot ceiling is exhausted;
    /// [`SafetyTcbSystemReserve`] remains untouched.
    RejectedNonTcbSystemCeilingExhausted {
        /// Non-TCB workload ceiling (`total_system_slots - reserved_tcb_slots`).
        non_tcb_ceiling: u32,
    },
}

/// Outcome of a completed workload bulkhead lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BulkheadExecutionOutcome {
    /// Execution succeeded; resets `consecutive_failures` to `0` on route
    /// scopes.
    Success,
    /// Execution failed; increments failure counters ONLY on the explicitly
    /// attributed fault domain scope(s) (e.g. a buggy `TaskGroup` or stalled
    /// `Provider`) so healthy parent `Tenant`/`Workspace` bulkheads are not
    /// collaterally poisoned.
    Failure {
        /// Non-empty subset of the lease's route scopes responsible for the
        /// failure.
        attributed_scopes: Vec<BulkheadScopeId>,
    },
}

/// Record of a queued workload request promoted to an active lease when a slot
/// freed up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkheadPromotedTicket {
    /// Promoted queue ticket.
    pub queue_ticket_id: BulkheadQueueTicketId,
    /// Newly issued active bulkhead lease.
    pub lease_id: BulkheadLeaseId,
    /// Route of the promoted workload.
    pub route: WorkloadBulkheadRoute,
}

/// Report returned by [`BulkheadController::complete_workload_lease`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkheadCompletionReport {
    /// Any scopes that newly tripped into
    /// [`BulkheadHealthState::PoisonedCircuitOpen`] from this failure.
    pub newly_poisoned_scopes: Vec<(BulkheadScopeId, BulkheadTripReason)>,
    /// Number of queued tickets evicted because their bottleneck scope tripped
    /// into `PoisonedCircuitOpen`.
    pub dropped_poisoned_waiters: usize,
    /// Waiting tickets deterministically promoted into active leases.
    pub promoted_tickets: Vec<BulkheadPromotedTicket>,
}

#[derive(Clone, Debug)]
struct BulkheadEntry {
    spec: BulkheadSpec,
    health: BulkheadHealthState,
    active_concurrent: u32,
    queued_tickets: u32,
    consecutive_failures: u32,
    total_failures: u32,
    reset_epoch: u64,
}

/// Registry enforcing unique [`BulkheadScopeId`] and [`ResourceGroupId`]
/// bindings across `Tenant`, `Workspace`, `TaskGroup`, and `Provider` scopes.
#[derive(Clone, Debug)]
pub struct BulkheadRegistry {
    entries: BTreeMap<BulkheadScopeId, BulkheadEntry>,
    resource_group_owners: BTreeMap<ResourceGroupId, Option<BulkheadScopeId>>,
}

impl BulkheadRegistry {
    /// Creates an empty registry pre-reserving `tcb_resource_group_id` so no
    /// workload bulkhead can ever alias the `SafetyTcb` resource group.
    #[must_use]
    pub fn new(tcb_resource_group_id: ResourceGroupId) -> Self {
        let mut resource_group_owners = BTreeMap::new();
        resource_group_owners.insert(tcb_resource_group_id, None);
        Self {
            entries: BTreeMap::new(),
            resource_group_owners,
        }
    }

    /// Registers a new isolated bulkhead.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError`] if `max_concurrent == 0`, failure thresholds are
    /// zero, `scope_id` is already registered, or `resource_group_id` collides
    /// with any existing bulkhead or `SafetyTcbSystemReserve`.
    pub fn register(&mut self, spec: BulkheadSpec) -> Result<(), FanoutError> {
        if spec.max_concurrent == 0 {
            return Err(FanoutError::InvalidConfiguration {
                reason: "bulkhead max_concurrent must be >= 1",
            });
        }
        if spec.failure_budget.max_consecutive_failures == 0
            || spec.failure_budget.max_total_failures == 0
        {
            return Err(FanoutError::InvalidConfiguration {
                reason: "bulkhead failure_budget thresholds must be >= 1",
            });
        }
        if self.entries.contains_key(&spec.scope_id) {
            return Err(FanoutError::DuplicateBulkheadScope {
                scope_id: spec.scope_id,
            });
        }
        if self
            .resource_group_owners
            .contains_key(&spec.resource_group_id)
        {
            return Err(FanoutError::DuplicateBulkheadResourceGroup {
                resource_group_id: spec.resource_group_id,
            });
        }

        self.resource_group_owners
            .insert(spec.resource_group_id, Some(spec.scope_id));
        self.entries.insert(
            spec.scope_id,
            BulkheadEntry {
                spec,
                health: BulkheadHealthState::Healthy,
                active_concurrent: 0,
                queued_tickets: 0,
                consecutive_failures: 0,
                total_failures: 0,
                reset_epoch: 0,
            },
        );
        Ok(())
    }

    /// Returns a live [`BulkheadStateSnapshot`] for `scope_id`.
    #[must_use]
    pub fn snapshot(&self, scope_id: BulkheadScopeId) -> Option<BulkheadStateSnapshot> {
        self.entries
            .get(&scope_id)
            .map(|entry| BulkheadStateSnapshot {
                scope_id,
                resource_group_id: entry.spec.resource_group_id,
                health: entry.health,
                active_concurrent: entry.active_concurrent,
                max_concurrent: entry.spec.max_concurrent,
                queued_count: entry.queued_tickets,
                max_queue_depth: entry.spec.max_queue_depth,
                consecutive_failures: entry.consecutive_failures,
                total_failures: entry.total_failures,
                reset_epoch: entry.reset_epoch,
            })
    }

    /// Total registered workload bulkheads.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no workload bulkheads are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
struct QueuedWorkloadEntry {
    ticket_id: BulkheadQueueTicketId,
    bottleneck_scope: BulkheadScopeId,
    route: WorkloadBulkheadRoute,
}

/// Multi-scope bulkhead isolation controller with an untouchable
/// [`SafetyTcbSystemReserve`] (`[DIST-BULKHEAD-001]`).
#[derive(Clone, Debug)]
pub struct BulkheadController {
    total_system_slots: u32,
    tcb_reserve: SafetyTcbSystemReserve,
    active_tcb_leases: BTreeSet<TcbSlotLeaseId>,
    active_non_tcb_slots: u32,
    registry: BulkheadRegistry,
    active_workload_leases: BTreeMap<BulkheadLeaseId, WorkloadBulkheadRoute>,
    wait_queue: VecDeque<QueuedWorkloadEntry>,
    next_lease_seq: u64,
    next_tcb_lease_seq: u64,
    next_queue_ticket_seq: u64,
}

impl BulkheadController {
    /// Creates a new [`BulkheadController`] partitioning `total_system_slots`
    /// into an untouchable `tcb_reserve` (`reserved_tcb_slots`) and a bounded
    /// non-TCB workload ceiling (`total_system_slots - reserved_tcb_slots`).
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::InvalidConfiguration`] if `reserved_tcb_slots == 0`
    /// or `reserved_tcb_slots >= total_system_slots`.
    pub fn new(
        total_system_slots: u32,
        tcb_reserve: SafetyTcbSystemReserve,
    ) -> Result<Self, FanoutError> {
        if tcb_reserve.reserved_tcb_slots == 0 {
            return Err(FanoutError::InvalidConfiguration {
                reason: "SafetyTcbSystemReserve.reserved_tcb_slots must be >= 1",
            });
        }
        if tcb_reserve.reserved_tcb_slots >= total_system_slots {
            return Err(FanoutError::InvalidConfiguration {
                reason: "total_system_slots must exceed reserved_tcb_slots",
            });
        }
        let registry = BulkheadRegistry::new(tcb_reserve.resource_group_id);
        Ok(Self {
            total_system_slots,
            tcb_reserve,
            active_tcb_leases: BTreeSet::new(),
            active_non_tcb_slots: 0,
            registry,
            active_workload_leases: BTreeMap::new(),
            wait_queue: VecDeque::new(),
            next_lease_seq: 0,
            next_tcb_lease_seq: 0,
            next_queue_ticket_seq: 0,
        })
    }

    /// Registers a `Tenant`, `Workspace`, `TaskGroup`, or `Provider` bulkhead.
    ///
    /// # Errors
    ///
    /// Propagates [`BulkheadRegistry::register`] validation errors.
    pub fn register_bulkhead(&mut self, spec: BulkheadSpec) -> Result<(), FanoutError> {
        self.registry.register(spec)
    }

    /// Borrows the underlying [`BulkheadRegistry`].
    #[must_use]
    pub const fn registry(&self) -> &BulkheadRegistry {
        &self.registry
    }

    /// Inspects a single bulkhead's state snapshot.
    #[must_use]
    pub fn bulkhead_snapshot(&self, scope_id: BulkheadScopeId) -> Option<BulkheadStateSnapshot> {
        self.registry.snapshot(scope_id)
    }

    /// Maximum slots non-TCB workloads can ever occupy simultaneously
    /// (`total_system_slots - reserved_tcb_slots`).
    #[must_use]
    pub const fn non_tcb_system_ceiling(&self) -> u32 {
        self.total_system_slots - self.tcb_reserve.reserved_tcb_slots
    }

    /// Currently active non-TCB workload slots.
    #[must_use]
    pub const fn active_non_tcb_slots(&self) -> u32 {
        self.active_non_tcb_slots
    }

    /// Remaining untouchable `SafetyTcb` slots currently available.
    #[must_use]
    pub fn available_tcb_slots(&self) -> u32 {
        let active = u32::try_from(self.active_tcb_leases.len()).unwrap_or(u32::MAX);
        self.tcb_reserve.reserved_tcb_slots.saturating_sub(active)
    }

    /// Acquires a dedicated `SafetyTcb` slot from [`SafetyTcbSystemReserve`].
    ///
    /// Completely unaffected by workload fanout storms, provider stalls,
    /// poisoned `TaskGroup`s, or 100% saturation of `non_tcb_system_ceiling`.
    #[must_use]
    pub fn acquire_safety_tcb_slot(&mut self) -> Option<(TcbSlotLeaseId, ResourceGroupId)> {
        if self.available_tcb_slots() == 0 {
            return None;
        }
        self.next_tcb_lease_seq = self.next_tcb_lease_seq.saturating_add(1);
        let lease_id = TcbSlotLeaseId(self.next_tcb_lease_seq);
        self.active_tcb_leases.insert(lease_id);
        Some((lease_id, self.tcb_reserve.resource_group_id))
    }

    /// Releases a `SafetyTcb` slot back to [`SafetyTcbSystemReserve`].
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::UnknownTcbLease`] if `lease_id` is not active.
    pub fn release_safety_tcb_slot(&mut self, lease_id: TcbSlotLeaseId) -> Result<(), FanoutError> {
        if !self.active_tcb_leases.remove(&lease_id) {
            return Err(FanoutError::UnknownTcbLease { lease_id });
        }
        Ok(())
    }

    /// Attempts atomic all-or-nothing bulkhead admission across every scope in
    /// `route` (`Tenant`, `Workspace`, `TaskGroup`, and optional `Provider`),
    /// strictly capped by `non_tcb_system_ceiling`.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::UnknownBulkheadScope`] if any scope in `route`
    /// has not been registered.
    pub fn acquire_workload(
        &mut self,
        route: WorkloadBulkheadRoute,
    ) -> Result<BulkheadAdmissionDecision, FanoutError> {
        let scopes = route.scopes();
        for &scope_id in &scopes {
            if !self.registry.entries.contains_key(&scope_id) {
                return Err(FanoutError::UnknownBulkheadScope { scope_id });
            }
        }

        // 1. Fail-closed check for poisoned bulkheads along the route.
        for &scope_id in &scopes {
            if let Some(entry) = self.registry.entries.get(&scope_id)
                && let BulkheadHealthState::PoisonedCircuitOpen { reason, .. } = entry.health
            {
                return Ok(BulkheadAdmissionDecision::RejectedPoisoned { scope_id, reason });
            }
        }

        // 2. Check per-scope concurrency saturation (most specific scope first:
        // Provider -> TaskGroup -> Workspace -> Tenant so localized stalls queue
        // on the localized bulkhead rather than the shared Tenant queue).
        let mut saturated_scope = None;
        for &scope_id in scopes.iter().rev() {
            if let Some(entry) = self.registry.entries.get(&scope_id)
                && (entry.active_concurrent >= entry.spec.max_concurrent
                    || entry.queued_tickets > 0)
            {
                saturated_scope = Some(scope_id);
                break;
            }
        }

        if let Some(bottleneck_scope) = saturated_scope {
            let Some(entry) = self.registry.entries.get_mut(&bottleneck_scope) else {
                return Err(FanoutError::UnknownBulkheadScope {
                    scope_id: bottleneck_scope,
                });
            };
            if entry.queued_tickets >= entry.spec.max_queue_depth {
                return Ok(BulkheadAdmissionDecision::RejectedQueueFull {
                    saturated_scope: bottleneck_scope,
                });
            }
            entry.queued_tickets = entry.queued_tickets.saturating_add(1);
            self.next_queue_ticket_seq = self.next_queue_ticket_seq.saturating_add(1);
            let queue_ticket_id = BulkheadQueueTicketId(self.next_queue_ticket_seq);
            self.wait_queue.push_back(QueuedWorkloadEntry {
                ticket_id: queue_ticket_id,
                bottleneck_scope,
                route,
            });
            return Ok(BulkheadAdmissionDecision::Queued {
                queue_ticket_id,
                bottleneck_scope,
            });
        }

        // 3. Enforce untouchable SafetyTcbSystemReserve boundary.
        let ceiling = self.non_tcb_system_ceiling();
        if self.active_non_tcb_slots >= ceiling {
            return Ok(
                BulkheadAdmissionDecision::RejectedNonTcbSystemCeilingExhausted {
                    non_tcb_ceiling: ceiling,
                },
            );
        }

        // 4. All scopes healthy & available: atomically acquire across route.
        let (lease_id, bound_resource_groups) = self.commit_route_acquisition(route, &scopes);
        Ok(BulkheadAdmissionDecision::Acquired {
            lease_id,
            bound_resource_groups,
        })
    }

    /// Completes an active workload lease, updates [`FailureBudget`] state,
    /// trips poisoned circuit breakers if thresholds are crossed, and promotes
    /// newly unblocked queued requests on healthy bulkheads.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::UnknownBulkheadLease`] if `lease_id` is unknown,
    /// or [`FanoutError::InvalidFailureAttribution`] if `outcome` attributes
    /// failure to an empty set or to a scope outside `lease_id`'s route.
    pub fn complete_workload_lease(
        &mut self,
        lease_id: BulkheadLeaseId,
        outcome: &BulkheadExecutionOutcome,
        completed_at_ms: i64,
    ) -> Result<BulkheadCompletionReport, FanoutError> {
        let Some(route) = self.active_workload_leases.get(&lease_id).copied() else {
            return Err(FanoutError::UnknownBulkheadLease { lease_id });
        };
        let route_scopes = route.scopes();

        if let BulkheadExecutionOutcome::Failure { attributed_scopes } = outcome
            && (attributed_scopes.is_empty()
                || attributed_scopes.iter().any(|s| !route_scopes.contains(s)))
        {
            return Err(FanoutError::InvalidFailureAttribution { lease_id });
        }

        self.active_workload_leases.remove(&lease_id);
        self.active_non_tcb_slots = self.active_non_tcb_slots.saturating_sub(1);
        for &scope_id in &route_scopes {
            if let Some(entry) = self.registry.entries.get_mut(&scope_id) {
                entry.active_concurrent = entry.active_concurrent.saturating_sub(1);
            }
        }

        let mut newly_poisoned_scopes = Vec::new();
        match outcome {
            BulkheadExecutionOutcome::Success => {
                for &scope_id in &route_scopes {
                    if let Some(entry) = self.registry.entries.get_mut(&scope_id)
                        && matches!(entry.health, BulkheadHealthState::Healthy)
                    {
                        entry.consecutive_failures = 0;
                    }
                }
            }
            BulkheadExecutionOutcome::Failure { attributed_scopes } => {
                let unique_attributed: BTreeSet<BulkheadScopeId> =
                    attributed_scopes.iter().copied().collect();
                for scope_id in unique_attributed {
                    if let Some(entry) = self.registry.entries.get_mut(&scope_id) {
                        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                        entry.total_failures = entry.total_failures.saturating_add(1);
                        if matches!(entry.health, BulkheadHealthState::Healthy) {
                            let trip_reason = if entry.consecutive_failures
                                >= entry.spec.failure_budget.max_consecutive_failures
                            {
                                Some(BulkheadTripReason::ConsecutiveFailuresExceeded {
                                    observed: entry.consecutive_failures,
                                    limit: entry.spec.failure_budget.max_consecutive_failures,
                                })
                            } else if entry.total_failures
                                >= entry.spec.failure_budget.max_total_failures
                            {
                                Some(BulkheadTripReason::TotalFailuresExceeded {
                                    observed: entry.total_failures,
                                    limit: entry.spec.failure_budget.max_total_failures,
                                })
                            } else {
                                None
                            };
                            if let Some(reason) = trip_reason {
                                entry.health = BulkheadHealthState::PoisonedCircuitOpen {
                                    reason,
                                    tripped_at_ms: completed_at_ms,
                                };
                                newly_poisoned_scopes.push((scope_id, reason));
                            }
                        }
                    }
                }
            }
        }

        // Evict queued waiters whose route touches any poisoned scope.
        let mut dropped_poisoned_waiters = 0usize;
        let mut surviving_queue = VecDeque::with_capacity(self.wait_queue.len());
        while let Some(waiter) = self.wait_queue.pop_front() {
            let touches_poisoned = waiter.route.scopes().iter().any(|s| {
                self.registry
                    .entries
                    .get(s)
                    .is_some_and(|e| !matches!(e.health, BulkheadHealthState::Healthy))
            });
            if touches_poisoned {
                if let Some(bottleneck) = self.registry.entries.get_mut(&waiter.bottleneck_scope) {
                    bottleneck.queued_tickets = bottleneck.queued_tickets.saturating_sub(1);
                }
                dropped_poisoned_waiters += 1;
            } else {
                surviving_queue.push_back(waiter);
            }
        }
        self.wait_queue = surviving_queue;

        let promoted_tickets = self.promote_ready_waiters();
        Ok(BulkheadCompletionReport {
            newly_poisoned_scopes,
            dropped_poisoned_waiters,
            promoted_tickets,
        })
    }

    /// Administratively resets a poisoned bulkhead back to `Healthy` under a
    /// strictly increasing `reset_epoch`.
    ///
    /// # Errors
    ///
    /// Returns [`FanoutError::UnknownBulkheadScope`] if `scope_id` is unknown,
    /// or [`FanoutError::StaleBulkheadResetEpoch`] if
    /// `new_reset_epoch <= current_epoch`.
    pub fn reset_poisoned_bulkhead(
        &mut self,
        scope_id: BulkheadScopeId,
        new_reset_epoch: u64,
    ) -> Result<(), FanoutError> {
        let Some(entry) = self.registry.entries.get_mut(&scope_id) else {
            return Err(FanoutError::UnknownBulkheadScope { scope_id });
        };
        if new_reset_epoch <= entry.reset_epoch {
            return Err(FanoutError::StaleBulkheadResetEpoch {
                scope_id,
                current_epoch: entry.reset_epoch,
                supplied_epoch: new_reset_epoch,
            });
        }
        entry.reset_epoch = new_reset_epoch;
        entry.health = BulkheadHealthState::Healthy;
        entry.consecutive_failures = 0;
        entry.total_failures = 0;
        Ok(())
    }

    fn commit_route_acquisition(
        &mut self,
        route: WorkloadBulkheadRoute,
        scopes: &[BulkheadScopeId],
    ) -> (BulkheadLeaseId, Vec<ResourceGroupId>) {
        let mut bound_resource_groups = Vec::with_capacity(scopes.len());
        for &scope_id in scopes {
            if let Some(entry) = self.registry.entries.get_mut(&scope_id) {
                entry.active_concurrent = entry.active_concurrent.saturating_add(1);
                bound_resource_groups.push(entry.spec.resource_group_id);
            }
        }
        self.active_non_tcb_slots = self.active_non_tcb_slots.saturating_add(1);
        self.next_lease_seq = self.next_lease_seq.saturating_add(1);
        let lease_id = BulkheadLeaseId(self.next_lease_seq);
        self.active_workload_leases.insert(lease_id, route);
        (lease_id, bound_resource_groups)
    }

    fn promote_ready_waiters(&mut self) -> Vec<BulkheadPromotedTicket> {
        let ceiling = self.non_tcb_system_ceiling();
        let mut promoted = Vec::new();
        let mut remaining = VecDeque::with_capacity(self.wait_queue.len());

        while let Some(waiter) = self.wait_queue.pop_front() {
            let scopes = waiter.route.scopes();
            let can_acquire = self.active_non_tcb_slots < ceiling
                && scopes.iter().all(|s| {
                    self.registry.entries.get(s).is_some_and(|e| {
                        matches!(e.health, BulkheadHealthState::Healthy)
                            && e.active_concurrent < e.spec.max_concurrent
                    })
                });
            if can_acquire {
                if let Some(bottleneck) = self.registry.entries.get_mut(&waiter.bottleneck_scope) {
                    bottleneck.queued_tickets = bottleneck.queued_tickets.saturating_sub(1);
                }
                let (lease_id, _) = self.commit_route_acquisition(waiter.route, &scopes);
                promoted.push(BulkheadPromotedTicket {
                    queue_ticket_id: waiter.ticket_id,
                    lease_id,
                    route: waiter.route,
                });
            } else {
                remaining.push_back(waiter);
            }
        }

        self.wait_queue = remaining;
        promoted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_reducer_digest_is_rejected_fail_closed() {
        let err = DeterministicLocalReducer::new([0u8; 32], LocalReducerKind::CanonicalSortedFold)
            .expect_err("zero reducer digest must fail closed");
        assert_eq!(err, FanoutError::ZeroReducerDigest);
    }

    #[test]
    fn duplicate_bulkhead_resource_group_is_rejected() {
        let tcb_rg = ResourceGroupId::from_bytes([0xFE; 16]);
        let mut controller = BulkheadController::new(
            16,
            SafetyTcbSystemReserve {
                resource_group_id: tcb_rg,
                reserved_tcb_slots: 4,
            },
        )
        .expect("valid controller");

        let shared_rg = ResourceGroupId::from_bytes([0x01; 16]);
        let spec_a = BulkheadSpec {
            scope_id: BulkheadScopeId::Tenant(FanoutTenantId::from_bytes([0xAA; 16])),
            resource_group_id: shared_rg,
            max_concurrent: 4,
            max_queue_depth: 4,
            failure_budget: FailureBudget {
                max_consecutive_failures: 3,
                max_total_failures: 5,
            },
        };
        controller.register_bulkhead(spec_a).expect("first ok");

        let spec_b = BulkheadSpec {
            scope_id: BulkheadScopeId::Tenant(FanoutTenantId::from_bytes([0xBB; 16])),
            resource_group_id: shared_rg,
            max_concurrent: 4,
            max_queue_depth: 4,
            failure_budget: FailureBudget {
                max_consecutive_failures: 3,
                max_total_failures: 5,
            },
        };
        assert_eq!(
            controller.register_bulkhead(spec_b),
            Err(FanoutError::DuplicateBulkheadResourceGroup {
                resource_group_id: shared_rg,
            })
        );

        let collide_tcb = BulkheadSpec {
            scope_id: BulkheadScopeId::Provider(FanoutProviderId::from_bytes([0xCC; 16])),
            resource_group_id: tcb_rg,
            max_concurrent: 2,
            max_queue_depth: 2,
            failure_budget: FailureBudget {
                max_consecutive_failures: 2,
                max_total_failures: 4,
            },
        };
        assert_eq!(
            controller.register_bulkhead(collide_tcb),
            Err(FanoutError::DuplicateBulkheadResourceGroup {
                resource_group_id: tcb_rg,
            })
        );
    }
}
