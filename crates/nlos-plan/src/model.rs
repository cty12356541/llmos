//! Domain model of the durable `TaskPlan`/`TaskNode` declaration authority
//! (ADR-0016 决定 2, v0.5 §24.1.1/§25.2.1 skeleton subset).

use nlos_types::{
    ArtifactId, IdempotencyKey, NamespaceId, PackageId, ReceiptId, TaskNodeId, TaskPlanId,
};

/// Domain separator for the authority-derived [`TaskPlanId`].
pub const PLAN_ID_DOMAIN: &[u8] = b"llmos/plan/plan-id/v1";
/// Domain separator for the authority-derived [`TaskNodeId`].
pub const TASK_NODE_ID_DOMAIN: &[u8] = b"llmos/plan/task-node-id/v1";
/// Domain separator for the per-node durable-metadata digest.
pub const NODE_DIGEST_DOMAIN: &[u8] = b"llmos/plan/node-digest/v1";
/// Domain separator for the per-revision node-set root.
pub const NODES_ROOT_DOMAIN: &[u8] = b"llmos/plan/nodes-root/v1";
/// Domain separator for the per-revision dependency-edge root.
pub const DEPENDENCIES_ROOT_DOMAIN: &[u8] = b"llmos/plan/dependencies-root/v1";
/// Domain separator for the revision digest (the immutable chain link).
pub const REVISION_DIGEST_DOMAIN: &[u8] = b"llmos/plan/revision-digest/v1";
/// Domain separator for the state-transition voucher id.
pub const VOUCHER_ID_DOMAIN: &[u8] = b"llmos/plan/transition-voucher-id/v1";
/// Domain separator for the residency-transition voucher id.
pub const RESIDENCY_VOUCHER_ID_DOMAIN: &[u8] = b"llmos/plan/residency-voucher-id/v1";
/// Domain separator for the PINNED-overlay voucher id (W36-P8).
pub const PIN_VOUCHER_ID_DOMAIN: &[u8] = b"llmos/plan/pin-voucher-id/v1";
/// Domain separator for the resolution receipt id.
pub const RESOLUTION_ID_DOMAIN: &[u8] = b"llmos/plan/resolution-id/v1";
/// Domain separator for the resolution receipt content digest.
pub const RESOLUTION_DIGEST_DOMAIN: &[u8] = b"llmos/plan/resolution-digest/v1";
/// Domain separator for the materialization request id (W31-A gate).
pub const MATERIALIZATION_REQUEST_ID_DOMAIN: &[u8] = b"llmos/plan/materialization-request-id/v1";
/// Domain separator for the derived idempotency key of the
/// state-driving vouchers a gate request records (W31-A gate).
pub const MATERIALIZATION_DRIVE_KEY_DOMAIN: &[u8] = b"llmos/plan/materialization-drive-key/v1";
/// Domain separator for the derived idempotency key of the
/// `WAITING_* → MATERIALIZING` approval voucher (W31-A gate).
pub const MATERIALIZATION_APPROVAL_KEY_DOMAIN: &[u8] =
    b"llmos/plan/materialization-approval-key/v1";
/// Domain separator for the scheduler request keys (W31-F): digest over
/// (node id, per-node retry round), so a restarted scheduler derives the
/// same key for the same round and a fresh key after each durable
/// resolution.
pub const SCHEDULER_REQUEST_KEY_DOMAIN: &[u8] = b"llmos/plan/scheduler-request-key/v1";
/// Domain separator for the ecosystem resolution receipt id (W36-P7,
/// ADR-0016 决定 5 ecosystem half; `[PLAN-DEPENDENCY-001]`).
pub const ECOSYSTEM_RESOLUTION_ID_DOMAIN: &[u8] = b"llmos/plan/ecosystem-resolution-id/v1";

/// Structural admission bound for one plan revision's declared node set.
/// The 100K logical-node tier is the G2 benchmark target (W31); this bound
/// only refuses unbounded declarations, it does not claim the tier.
pub const MAX_DECLARED_NODES_PER_REVISION: usize = 100_000;
/// Structural admission bound for one node's dependency edges.
pub const MAX_DEPENDENCIES_PER_NODE: usize = 256;
/// Structural admission bound for one node's declared namespace set
/// (the [`NamespaceCondition`] posture; same bound as dependencies).
pub const MAX_CONDITION_NAMESPACES: usize = 256;

/// What a declared node executes as (v0.5 `agent_role_or_executable`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PlanNodeKind {
    /// An `AgentRole` binding.
    AgentRole,
    /// An executable binding.
    Executable,
}

impl PlanNodeKind {
    const fn encode(self) -> i64 {
        match self {
            Self::AgentRole => 1,
            Self::Executable => 2,
        }
    }

    fn decode(value: i64) -> Result<Self, crate::PlanStoreError> {
        match value {
            1 => Ok(Self::AgentRole),
            2 => Ok(Self::Executable),
            _ => Err(crate::PlanStoreError::CorruptRecord("unknown node kind")),
        }
    }
}

/// §25.2.1 `TaskNode` execution state machine, full state set. The legal
/// edge subset producible in this skeleton is
/// [`PlanNodeState::transition_is_legal`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PlanNodeState {
    Declared,
    BlockedDependency,
    Eligible,
    WaitingAuthorization,
    WaitingResource,
    Materializing,
    Active,
    Checkpointed,
    Evicted,
    Rehydrating,
    Completed,
    Failed,
    Cancelled,
}

impl PlanNodeState {
    /// Stable one-byte wire discriminant (also the hash-input encoding).
    pub(crate) const fn discriminant(self) -> u8 {
        match self {
            Self::Declared => 1,
            Self::BlockedDependency => 2,
            Self::Eligible => 3,
            Self::WaitingAuthorization => 4,
            Self::WaitingResource => 5,
            Self::Materializing => 6,
            Self::Active => 7,
            Self::Checkpointed => 8,
            Self::Evicted => 9,
            Self::Rehydrating => 10,
            Self::Completed => 11,
            Self::Failed => 12,
            Self::Cancelled => 13,
        }
    }

    const fn encode(self) -> i64 {
        self.discriminant() as i64
    }

    fn decode(value: i64) -> Result<Self, crate::PlanStoreError> {
        match value {
            1 => Ok(Self::Declared),
            2 => Ok(Self::BlockedDependency),
            3 => Ok(Self::Eligible),
            4 => Ok(Self::WaitingAuthorization),
            5 => Ok(Self::WaitingResource),
            6 => Ok(Self::Materializing),
            7 => Ok(Self::Active),
            8 => Ok(Self::Checkpointed),
            9 => Ok(Self::Evicted),
            10 => Ok(Self::Rehydrating),
            11 => Ok(Self::Completed),
            12 => Ok(Self::Failed),
            13 => Ok(Self::Cancelled),
            _ => Err(crate::PlanStoreError::CorruptRecord("unknown node state")),
        }
    }

    /// Whether the state is terminal for the §25.2.1 machine.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Whether the node has crossed the execution boundary: from
    /// `MATERIALIZING` on, the node owns (or owned) execution footprint, so
    /// its declared shape revision is frozen (`[PLAN-DAG-001]` final clause:
    /// executed nodes keep their original revision/digest). Authorization
    /// facts themselves live in other authorities and reach this one through
    /// the ADR-0013 verify-then-commit materialization gate; the durable
    /// fence here is the materialization boundary.
    #[must_use]
    pub const fn is_execution_frozen(self) -> bool {
        self.discriminant() >= Self::Materializing.discriminant()
    }

    /// The §25.2.1 edge subset producible in this skeleton:
    /// `DECLARED → BLOCKED_DEPENDENCY | ELIGIBLE`,
    /// `BLOCKED_DEPENDENCY → ELIGIBLE`,
    /// `ELIGIBLE → WAITING_AUTHORIZATION | WAITING_RESOURCE`,
    /// `WAITING_AUTHORIZATION|WAITING_RESOURCE → MATERIALIZING`,
    /// `REHYDRATING → MATERIALIZING`, `MATERIALIZING → ACTIVE`,
    /// `ACTIVE → CHECKPOINTED | COMPLETED | FAILED`,
    /// `CHECKPOINTED → EVICTED`, `EVICTED → REHYDRATING`, and
    /// `→ CANCELLED` from any non-terminal state.
    #[must_use]
    pub fn transition_is_legal(from: Self, to: Self) -> bool {
        use PlanNodeState as S;
        if to == S::Cancelled {
            return !from.is_terminal() && from != S::Cancelled;
        }
        matches!(
            (from, to),
            (S::Declared, S::BlockedDependency | S::Eligible)
                | (S::BlockedDependency, S::Eligible)
                | (S::Eligible, S::WaitingAuthorization | S::WaitingResource)
                | (
                    S::WaitingAuthorization | S::WaitingResource | S::Rehydrating,
                    S::Materializing
                )
                | (S::Materializing, S::Active)
                | (S::Active, S::Checkpointed | S::Completed | S::Failed)
                | (S::Checkpointed, S::Evicted)
                | (S::Evicted, S::Rehydrating)
        )
    }
}

/// v0.5 §25.2.1 `ResidencyClass` subset for the W31-E minimal lane
/// (议题 28 定案 2): the Context residency tier of one plan node —
/// where the node's working set lives. This is a **separate axis** from
/// the §25.2.1 execution state machine ([`PlanNodeState`]): the two
/// advance independently and neither observes the other's vouchers.
///
/// The spec's sixth class `PINNED` is deliberately absent:
/// `[SCALE-PIN-001]` binds every pin to a `ResourceAllocation`, owner,
/// reason, upper bound, and expiry/renewal — a Resource-authority face
/// outside this lane (deferred, see evidence §8).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NodeResidencyTier {
    /// Only `TaskNode`/`AgentRole` dependency and resource declarations
    /// (the `[SCALE-LOGICAL-001]` bounded durable metadata row). The
    /// default tier of every declared node.
    MetadataOnly,
    /// Checkpoint/Artifact resides in persistent storage.
    Cold,
    /// Code, index, or partial Context quickly recoverable.
    Warm,
    /// `Process`/`AgentInstance` materialized, waiting to run.
    Hot,
    /// Currently holding a CPU/GPU/model/Driver execution slot.
    Running,
}

impl NodeResidencyTier {
    /// Stable one-byte wire discriminant (also the hash-input encoding);
    /// the ordering is the 议题 28 linear chain
    /// `METADATA_ONLY → COLD → WARM → HOT → RUNNING`.
    pub(crate) const fn discriminant(self) -> u8 {
        match self {
            Self::MetadataOnly => 1,
            Self::Cold => 2,
            Self::Warm => 3,
            Self::Hot => 4,
            Self::Running => 5,
        }
    }

    const fn encode(self) -> i64 {
        self.discriminant() as i64
    }

    fn decode(value: i64) -> Result<Self, crate::PlanStoreError> {
        match value {
            1 => Ok(Self::MetadataOnly),
            2 => Ok(Self::Cold),
            3 => Ok(Self::Warm),
            4 => Ok(Self::Hot),
            5 => Ok(Self::Running),
            _ => Err(crate::PlanStoreError::CorruptRecord(
                "unknown residency tier",
            )),
        }
    }

    /// The conservative legal residency edge set: a single adjacent step
    /// along the 议题 28 linear chain, in either direction (eviction walks
    /// down, rehydration walks up). Self-loops and multi-tier skips are
    /// illegal — every step is one typed voucher, so a HOT→WARM→COLD
    /// eviction and its rehydration are always fully auditable. Residency
    /// legality never consults the lifecycle state machine.
    #[must_use]
    pub const fn tier_transition_is_legal(from: Self, to: Self) -> bool {
        let delta = to.discriminant() as i16 - from.discriminant() as i16;
        delta == 1 || delta == -1
    }
}

/// One node's declaration inside a plan revision. Dependency edges are
/// structured (declaration-local `node_key` references, `[PLAN-DAG-001]`);
/// the remaining metadata bodies are digest-bound (their parsed models are
/// W28-B/W29 territory).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanNodeDeclaration {
    /// Declaration-local stable name; the authority derives the durable
    /// [`TaskNodeId`] from `(plan_id, node_key)`, so the same key across
    /// revisions addresses the same logical node.
    pub node_key: [u8; 16],
    pub kind: PlanNodeKind,
    /// Digest of the bound `AgentRole`/executable identity.
    pub binding_digest: [u8; 32],
    /// Dependencies by declaration-local `node_key`.
    pub dependency_keys: Vec<[u8; 16]>,
    /// Digest of the input-selector list.
    pub input_selectors_digest: [u8; 32],
    /// Digest of the output contract.
    pub output_contract_digest: [u8; 32],
    /// Digest of the failure/retry/reducer policy.
    pub policy_digest: [u8; 32],
    /// Digest of the requested resource upper bound.
    pub resource_ceiling_digest: [u8; 32],
    /// Structured form of the three G3 gate conditions (W36-P7): the
    /// typed [`NodeConditions`] upgrade of what previously rode the
    /// digest slots by convention only. `None` (the digest-only form)
    /// remains fully accepted and hashes bit-identically to the
    /// pre-v6 formula.
    pub conditions: Option<NodeConditions>,
}

/// Structured form of the three G3 gate conditions W28-A carried as
/// opaque digests (W36-P7; W31-G §8.2.2): v0.5 行 3650
/// `[PLAN-LAZY-001]`'s Namespace / `ResourceContract` / fanout gate
/// declarations. Validation is structural (canonical, bounded, honest
/// to declare); *enforcement* stays with the future authorities — the
/// materialization gate consumes this typed form when they land.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeConditions {
    pub namespace: NamespaceCondition,
    pub resource_contract: ResourceContractCondition,
    pub fanout: FanoutCondition,
}

/// The namespaces the node declares it will act within; the gate
/// consults the Namespace/Capability authority against this set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespaceCondition {
    /// Declaration order is not identity: the canonical encoding sorts,
    /// so re-declaring the same set in another order keeps the node
    /// digest bitwise (duplicates are refused typed, not deduplicated).
    pub namespaces: Vec<NamespaceId>,
}

/// Requested per-dimension resource ceilings, mirroring the fixed
/// three-dimension `ResourceDemand` vocabulary of the resource
/// authority (`cpu_shares` / `memory_mib` / `io_weight`, W22-R). At least one
/// dimension must be non-zero — a declared contract of nothing is
/// refused typed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceContractCondition {
    pub cpu_shares: u64,
    pub memory_mib: u64,
    pub io_weight: u64,
}

/// The node's declared upper bound on live downstream fanout
/// (`[PLAN-TOPIC-001]`/`[MSG-PAY-001]` posture: a declared fanout bound
/// must carry a bounded payer/grant before `MATERIALIZING`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FanoutCondition {
    pub max_downstream_fanout: u64,
}

impl NodeConditions {
    /// Structural validation of one typed condition set. Called on the
    /// apply path before any digest is computed, and re-run by decode
    /// (a stored body that fails it is corrupt, never reinterpreted).
    pub(crate) fn validate(&self) -> Result<(), crate::PlanStoreError> {
        if self.namespace.namespaces.is_empty() {
            return Err(crate::PlanStoreError::InvalidNodeConditions {
                reason: "namespace condition must declare at least one namespace",
            });
        }
        if self.namespace.namespaces.len() > MAX_CONDITION_NAMESPACES {
            return Err(crate::PlanStoreError::InvalidNodeConditions {
                reason: "namespace condition exceeds the admission bound",
            });
        }
        let mut seen = std::collections::HashSet::with_capacity(self.namespace.namespaces.len());
        for namespace in &self.namespace.namespaces {
            if !seen.insert(*namespace) {
                return Err(crate::PlanStoreError::InvalidNodeConditions {
                    reason: "duplicate namespace in one condition set",
                });
            }
        }
        if self.resource_contract.cpu_shares == 0
            && self.resource_contract.memory_mib == 0
            && self.resource_contract.io_weight == 0
        {
            return Err(crate::PlanStoreError::InvalidNodeConditions {
                reason: "resource contract must request at least one non-zero dimension",
            });
        }
        if self.fanout.max_downstream_fanout == 0 {
            return Err(crate::PlanStoreError::InvalidNodeConditions {
                reason: "fanout condition must bound at least one downstream",
            });
        }
        Ok(())
    }

    /// Canonical byte encoding (schema tag 1): the sorted namespace set
    /// (order-independent identity), then the three resource dimensions
    /// and the fanout bound as big-endian u64. Assumes [`Self::validate`]
    /// passed; the encoding is the node-digest fold input.
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut namespaces = self.namespace.namespaces.clone();
        namespaces.sort_unstable();
        let mut bytes = Vec::with_capacity(41 + namespaces.len() * 16);
        bytes.push(1_u8);
        bytes.extend_from_slice(&(namespaces.len() as u64).to_be_bytes());
        for namespace in namespaces {
            bytes.extend_from_slice(namespace.as_bytes());
        }
        bytes.extend_from_slice(&self.resource_contract.cpu_shares.to_be_bytes());
        bytes.extend_from_slice(&self.resource_contract.memory_mib.to_be_bytes());
        bytes.extend_from_slice(&self.resource_contract.io_weight.to_be_bytes());
        bytes.extend_from_slice(&self.fanout.max_downstream_fanout.to_be_bytes());
        bytes
    }

    /// Decodes one canonical body and re-validates it; anything malformed
    /// or non-canonical (unsorted namespaces, trailing bytes) fails
    /// closed as a corrupt record.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, crate::PlanStoreError> {
        let corrupt = |reason: &'static str| crate::PlanStoreError::CorruptRecord(reason);
        if bytes.first() != Some(&1_u8) {
            return Err(corrupt("node conditions schema tag"));
        }
        let mut offset = 1_usize;
        let take = |offset: &mut usize, width: usize| -> Option<&[u8]> {
            let slice = bytes.get(*offset..(*offset + width))?;
            *offset += width;
            Some(slice)
        };
        let count_bytes = take(&mut offset, 8)
            .and_then(|slice| <[u8; 8]>::try_from(slice).ok())
            .ok_or_else(|| corrupt("node conditions namespace count"))?;
        let count = usize::try_from(u64::from_be_bytes(count_bytes))
            .map_err(|_| corrupt("node conditions namespace count"))?;
        if count == 0 || count > MAX_CONDITION_NAMESPACES {
            return Err(corrupt("node conditions namespace count"));
        }
        let mut namespaces = Vec::with_capacity(count);
        for _ in 0..count {
            let id_bytes = take(&mut offset, 16)
                .and_then(|slice| <[u8; 16]>::try_from(slice).ok())
                .ok_or_else(|| corrupt("node conditions namespace id"))?;
            namespaces.push(NamespaceId::from_bytes(id_bytes));
        }
        let mut dimension = || -> Result<u64, crate::PlanStoreError> {
            take(&mut offset, 8)
                .and_then(|slice| <[u8; 8]>::try_from(slice).ok())
                .map(u64::from_be_bytes)
                .ok_or_else(|| corrupt("node conditions resource dimension"))
        };
        let cpu_shares = dimension()?;
        let memory_mib = dimension()?;
        let io_weight = dimension()?;
        let max_downstream_fanout = dimension()?;
        if offset != bytes.len() {
            return Err(corrupt("node conditions trailing bytes"));
        }
        let is_sorted = namespaces.is_sorted();
        let conditions = Self {
            namespace: NamespaceCondition { namespaces },
            resource_contract: ResourceContractCondition {
                cpu_shares,
                memory_mib,
                io_weight,
            },
            fanout: FanoutCondition {
                max_downstream_fanout,
            },
        };
        conditions.validate()?;
        if !is_sorted {
            return Err(corrupt("node conditions are not canonical"));
        }
        Ok(conditions)
    }
}

/// Request to apply one plan revision (revision 1 creates the plan).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyPlanRevisionRequest {
    /// `None` for the initial revision (the authority derives the
    /// [`TaskPlanId`]); `Some` for every later revision of that plan.
    pub plan_id: Option<TaskPlanId>,
    /// The complete node set of this revision (a revision is total: nodes
    /// not re-declared are dropped from the plan's current shape).
    pub nodes: Vec<PlanNodeDeclaration>,
    /// Exactly-once key for the revision receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied observation time (ms); the store holds no clock.
    pub applied_at_ms: u64,
}

/// Immutable receipt of one applied plan revision (the chain link).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanRevisionReceipt {
    pub plan_id: TaskPlanId,
    pub revision: u64,
    pub idempotency_key: IdempotencyKey,
    /// Digest of revision `revision - 1`, `None` for revision 1: the
    /// immutable chain head link.
    pub parent_revision_digest: Option<[u8; 32]>,
    pub nodes_root: [u8; 32],
    pub dependencies_root: [u8; 32],
    pub plan_digest: [u8; 32],
    pub declared_node_count: u64,
    pub applied_at_ms: u64,
}

/// Outcome of one `apply_plan_revision` call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanRevisionDecision {
    /// First execution of this key: the revision (and for revision 1, the
    /// plan) committed.
    Applied(PlanRevisionReceipt),
    /// Durable replay: the original receipt is returned unchanged.
    Replayed(PlanRevisionReceipt),
}

impl PlanRevisionDecision {
    /// The receipt this call denotes, whichever branch.
    #[must_use]
    pub const fn receipt(self) -> PlanRevisionReceipt {
        match self {
            Self::Applied(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Read-only current-state view of one plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanView {
    pub plan_id: TaskPlanId,
    pub current_revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Durable metadata of one `TaskNode` (the `[SCALE-LOGICAL-001]` bounded
/// per-node row).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanNodeRecord {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub node_key: [u8; 16],
    pub kind: PlanNodeKind,
    /// The revision whose shape this node carries. Frozen at its original
    /// value once the node crosses the execution boundary (`[PLAN-DAG-001]`).
    pub declared_revision: u64,
    /// Digest of the declared shape (kind, binding, dependencies, selector/
    /// contract/policy/ceiling digests).
    pub node_digest: [u8; 32],
    pub state: PlanNodeState,
    /// Number of state-transition vouchers recorded for this node.
    pub transition_count: u64,
    /// The node's Context residency tier (§25.2.1 residency axis,
    /// separate from `state`).
    pub residency_tier: NodeResidencyTier,
    /// Number of residency-transition vouchers recorded for this node.
    pub residency_transition_count: u64,
    pub first_declared_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Request to record one node state transition (the 状态机迁移凭证).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeTransitionRequest {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    /// CAS on the node's current state.
    pub from_state: PlanNodeState,
    pub to_state: PlanNodeState,
    /// CAS on the node's declared-shape revision (`[PLAN-DAG-001]` fence:
    /// callers must observe the revision the node is actually bound to).
    pub expected_declared_revision: u64,
    /// Exactly-once key for the voucher.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied observation time (ms).
    pub transitioned_at_ms: u64,
}

/// Immutable voucher of one recorded node state transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeTransitionVoucher {
    pub voucher_id: ReceiptId,
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    /// Dense per-node sequence, starting at 1.
    pub transition_seq: u64,
    pub from_state: PlanNodeState,
    pub to_state: PlanNodeState,
    /// The node's declared-shape revision observed by the transition.
    pub observed_revision: u64,
    pub idempotency_key: IdempotencyKey,
    pub transitioned_at_ms: u64,
}

/// Outcome of one `record_node_transition` call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodeTransitionDecision {
    /// First execution of this key: the voucher committed with the state
    /// CAS.
    Recorded(NodeTransitionVoucher),
    /// Durable replay: the original voucher is returned unchanged.
    Replayed(NodeTransitionVoucher),
}

impl NodeTransitionDecision {
    /// The voucher this call denotes, whichever branch.
    #[must_use]
    pub const fn voucher(self) -> NodeTransitionVoucher {
        match self {
            Self::Recorded(voucher) | Self::Replayed(voucher) => voucher,
        }
    }
}

/// Request to record one residency tier transition (W31-E, v0.5 §25.2.1
/// residency axis). The tier CAS and the declared-revision CAS mirror
/// the lifecycle face's fences; the two voucher faces are otherwise
/// fully independent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResidencyTransitionRequest {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    /// CAS on the node's current residency tier.
    pub from_tier: NodeResidencyTier,
    pub to_tier: NodeResidencyTier,
    /// CAS on the node's declared-shape revision (the same
    /// `[PLAN-DAG-001]` fence the lifecycle face applies: a reshaping
    /// revision fences in-flight residency moves that observed the
    /// pre-reshape revision).
    pub expected_declared_revision: u64,
    /// Exactly-once key for the voucher.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied observation time (ms).
    pub transitioned_at_ms: u64,
}

/// Immutable voucher of one recorded residency tier transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResidencyTransitionVoucher {
    pub voucher_id: ReceiptId,
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    /// Dense per-node residency sequence, starting at 1 (independent of
    /// the lifecycle voucher sequence).
    pub transition_seq: u64,
    pub from_tier: NodeResidencyTier,
    pub to_tier: NodeResidencyTier,
    /// The node's declared-shape revision observed by the transition.
    pub observed_revision: u64,
    pub idempotency_key: IdempotencyKey,
    pub transitioned_at_ms: u64,
}

/// Outcome of one `record_residency_transition` call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResidencyTransitionDecision {
    /// First execution of this key: the voucher committed with the tier
    /// CAS.
    Recorded(ResidencyTransitionVoucher),
    /// Durable replay: the original voucher is returned unchanged.
    Replayed(ResidencyTransitionVoucher),
}

impl ResidencyTransitionDecision {
    /// The voucher this call denotes, whichever branch.
    #[must_use]
    pub const fn voucher(self) -> ResidencyTransitionVoucher {
        match self {
            Self::Recorded(voucher) | Self::Replayed(voucher) => voucher,
        }
    }
}

/// Typed graded readback of one node's residency axis (W31-E lane gate
/// 分级读回): the current tier and the last transition voucher in one
/// read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeResidencyView {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub tier: NodeResidencyTier,
    /// Number of residency-transition vouchers recorded for the node.
    pub transition_count: u64,
    /// The node's newest residency voucher, `None` before the first
    /// transition.
    pub last_voucher: Option<ResidencyTransitionVoucher>,
}

/// Request to pin or unpin one plan node (W36-P8 PINNED overlay).
///
/// Pin and unpin share the request shape; the write face (`record_node_pin`
/// vs `record_node_unpin`) is the intent. The declared-revision CAS is
/// the same `[PLAN-DAG-001]` fence the residency axis uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodePinRequest {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub expected_declared_revision: u64,
    pub idempotency_key: IdempotencyKey,
    pub transitioned_at_ms: u64,
}

/// Immutable voucher of one pin/unpin overlay transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodePinVoucher {
    pub voucher_id: ReceiptId,
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub transition_seq: u64,
    pub from_pinned: bool,
    pub to_pinned: bool,
    pub observed_revision: u64,
    pub idempotency_key: IdempotencyKey,
    pub transitioned_at_ms: u64,
}

/// Outcome of one pin/unpin call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NodePinDecision {
    Recorded(NodePinVoucher),
    Replayed(NodePinVoucher),
}

impl NodePinDecision {
    /// The voucher this call denotes, whichever branch.
    #[must_use]
    pub fn voucher(self) -> NodePinVoucher {
        match self {
            Self::Recorded(voucher) | Self::Replayed(voucher) => voucher,
        }
    }
}

/// Typed readback of one node's PINNED overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodePinView {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub pinned: bool,
    pub transition_count: u64,
    pub last_voucher: Option<NodePinVoucher>,
}

/// Result of walking one plan's immutable revision chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainVerification {
    pub plan_id: TaskPlanId,
    /// Number of revision links verified (0 for an unknown plan).
    pub revision_count: u64,
    /// The newest revision, `None` for an unknown plan.
    pub head_revision: Option<u64>,
    /// The newest revision's digest, `None` for an unknown plan.
    pub head_digest: Option<[u8; 32]>,
}

/// Typed selector naming the plan revision a resolution is computed
/// against (ADR-0016 决定 5: typed selector → version/generation-carrying
/// handle; `[PLAN-DEPENDENCY-001]`). `Current` pins exactly once, at the
/// head observed when the resolution commits — it never floats to later
/// revisions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PlanRevisionSelector {
    /// Resolve whatever revision is the plan's current head.
    Current(TaskPlanId),
    /// Resolve exactly the named revision.
    At { plan_id: TaskPlanId, revision: u64 },
}

impl PlanRevisionSelector {
    /// The plan this selector addresses.
    #[must_use]
    pub const fn plan_id(&self) -> TaskPlanId {
        match self {
            Self::Current(plan_id) | Self::At { plan_id, .. } => *plan_id,
        }
    }
}

/// Request to resolve one plan revision's declared dependency DAG
/// (topological order + edge set) into a durable resolution receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvePlanRequest {
    pub selector: PlanRevisionSelector,
    /// Exactly-once key for the resolution receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied observation time (ms); the store holds no clock.
    pub resolved_at_ms: u64,
}

/// The durable resolution receipt — simultaneously the
/// version/generation-carrying handle (ADR-0016 决定 5). Every field is
/// pinned at resolution time; later plan revisions never change it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanResolutionHandle {
    pub resolution_id: ReceiptId,
    pub plan_id: TaskPlanId,
    /// The generation this resolution was computed against.
    pub revision: u64,
    /// The revision digest this resolution was computed against.
    pub plan_digest: [u8; 32],
    /// Digest over (plan, revision, plan digest, order, edges); the
    /// receipt's own content root.
    pub resolution_digest: [u8; 32],
    /// Topological order (dependencies first), deterministic under the
    /// minimum-`TaskNodeId` tiebreak.
    pub resolved_order: Vec<TaskNodeId>,
    /// Canonical edge set, `(dependent, dependency)` pairs, sorted.
    pub resolved_edges: Vec<(TaskNodeId, TaskNodeId)>,
    pub idempotency_key: IdempotencyKey,
    pub resolved_at_ms: u64,
}

/// Outcome of one `resolve_plan` call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanResolutionDecision {
    /// First execution of this key: the resolution receipt committed.
    Resolved(PlanResolutionHandle),
    /// Durable replay: the original receipt is returned byte-equal.
    Replayed(PlanResolutionHandle),
}

impl PlanResolutionDecision {
    /// The handle this call denotes, whichever branch.
    #[must_use]
    pub fn handle(self) -> PlanResolutionHandle {
        match self {
            Self::Resolved(handle) | Self::Replayed(handle) => handle,
        }
    }
}

/// One node's declared shape **as pinned by a resolution**: read from the
/// revision's immutable declared-shape rows, never from the mutable
/// current node rows (G4: a later revision must not silently change an
/// in-flight resolution's view).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPlanNode {
    pub node_id: TaskNodeId,
    pub node_key: [u8; 16],
    pub kind: PlanNodeKind,
    /// The node's shape digest as declared in the resolution's revision.
    pub node_digest: [u8; 32],
    /// 1-based position in the resolved topological order.
    pub position: u64,
}

/// The closed kind set of ecosystem entities the resolver's selector
/// half addresses (W36-P7; `[PLAN-DEPENDENCY-001]`: "Package、Skill、
/// Tool、Model、Artifact、Topic 和外部服务依赖 MUST 在计划中以 typed
/// selector 声明，并在执行前解析为带版本/generation 的 handle"). A kind
/// enters this enum only once its authority exposes a generation-carrying
/// readback; kinds without one stay in the lane evidence's deferred
/// register (Topic has no advancing topic-level generation; Skill/Tool/
/// Model/外部服务 have no authority at all yet).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EcosystemEntityKind {
    /// An application installed from one package (`nlos-application`:
    /// keyed by `PackageId`; generation = installation generation).
    Application,
    /// One artifact's head (`nlos-artifact`: generation = head revision).
    Artifact,
}

impl EcosystemEntityKind {
    /// Stable one-byte wire discriminant (also the hash-input encoding).
    pub(crate) const fn discriminant(self) -> u8 {
        match self {
            Self::Application => 1,
            Self::Artifact => 2,
        }
    }

    pub(crate) const fn encode(self) -> i64 {
        self.discriminant() as i64
    }

    pub(crate) fn decode(value: i64) -> Result<Self, crate::PlanStoreError> {
        match value {
            1 => Ok(Self::Application),
            2 => Ok(Self::Artifact),
            _ => Err(crate::PlanStoreError::EcosystemKindUnknown(value)),
        }
    }
}

/// The generation side of an ecosystem selector, mirroring
/// [`PlanRevisionSelector`]: `Current` pins whatever generation the
/// source observes when the resolution commits (exactly once, never
/// floating); `At` demands one exact generation and fails the typed
/// stale fence on any mismatch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GenerationExpectation {
    Current,
    At(u64),
}

/// Typed ecosystem selector (W36-P7): entity kind + nominal id +
/// generation expectation — the `[PLAN-DEPENDENCY-001]` declaration form
/// for ecosystem dependencies, resolved by
/// [`crate::SqlitePlanAuthority::resolve_ecosystem_selector`] into a
/// durable generation-carrying handle.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EcosystemSelector {
    Application {
        package_id: PackageId,
        expectation: GenerationExpectation,
    },
    Artifact {
        artifact_id: ArtifactId,
        expectation: GenerationExpectation,
    },
}

impl EcosystemSelector {
    #[must_use]
    pub const fn kind(&self) -> EcosystemEntityKind {
        match self {
            Self::Application { .. } => EcosystemEntityKind::Application,
            Self::Artifact { .. } => EcosystemEntityKind::Artifact,
        }
    }

    #[must_use]
    pub const fn entity_id(&self) -> [u8; 16] {
        match self {
            Self::Application { package_id, .. } => *package_id.as_bytes(),
            Self::Artifact { artifact_id, .. } => *artifact_id.as_bytes(),
        }
    }

    #[must_use]
    pub const fn expectation(&self) -> GenerationExpectation {
        match self {
            Self::Application { expectation, .. } | Self::Artifact { expectation, .. } => {
                *expectation
            }
        }
    }
}

/// Generation-carrying readback of one ecosystem entity, as plain data
/// (the boundary crossing between a source authority and this resolver).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EcosystemEntityState {
    /// Dense, monotonic, never zero.
    pub generation: u64,
    /// The generation's content anchor (e.g. package manifest digest,
    /// head content digest).
    pub content_digest: [u8; 32],
}

/// One source lookup's answer: `NotFound` is a typed miss (the entity id
/// is unknown to the source), not an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EcosystemSourceLookup {
    Found(EcosystemEntityState),
    NotFound,
}

/// Request to resolve one ecosystem selector into a durable resolution
/// receipt (the ecosystem half of ADR-0016 决定 5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolveEcosystemRequest {
    pub selector: EcosystemSelector,
    /// Exactly-once key for the resolution receipt.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied observation time (ms); the store holds no clock.
    pub resolved_at_ms: u64,
}

/// The durable ecosystem resolution receipt — simultaneously the
/// generation-carrying handle. Every field is pinned at resolution time;
/// later generations in the source never change it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EcosystemResolutionHandle {
    pub resolution_id: ReceiptId,
    pub kind: EcosystemEntityKind,
    pub entity_id: [u8; 16],
    /// The generation this resolution was computed against.
    pub generation: u64,
    /// The entity's content digest as observed at that generation.
    pub content_digest: [u8; 32],
    pub idempotency_key: IdempotencyKey,
    pub resolved_at_ms: u64,
}

/// Outcome of one `resolve_ecosystem_selector` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EcosystemResolutionDecision {
    /// First execution of this key: the resolution receipt committed.
    Resolved(EcosystemResolutionHandle),
    /// Durable replay: the original receipt is returned byte-equal.
    Replayed(EcosystemResolutionHandle),
}

impl EcosystemResolutionDecision {
    /// The handle this call denotes, whichever branch.
    #[must_use]
    pub const fn handle(self) -> EcosystemResolutionHandle {
        match self {
            Self::Resolved(handle) | Self::Replayed(handle) => handle,
        }
    }
}

/// Request to open one materialization gate round for a node (W31-A,
/// ADR-0016 G3; the readiness half of the ADR-0013 verify-then-commit
/// protocol). The authority drives the node forward through the legal
/// §25.2.1 edges (`DECLARED/BLOCKED_DEPENDENCY → ELIGIBLE →
/// WAITING_RESOURCE`, as dense vouchers), verifies dependency readiness
/// (every declared dependency must be `COMPLETED`), and records one
/// durable pending request the Task side consumes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializationRequest {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    /// Exactly-once key for the request.
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied observation time (ms).
    pub requested_at_ms: u64,
}

/// The Task-side admission verdict one resolution commits (the commit
/// half of the ADR-0013 protocol). `Approved` carries the admission facts
/// the Task authority's consumption path consulted; `Rejected` carries
/// the typed window-shrink reason — the node stays `WAITING_*`, the plan
/// does not fail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializationAdmissionVerdict {
    Approved(MaterializationAdmission),
    Rejected(MaterializationRejection),
}

/// Admission facts recorded on an approved materialization request: the
/// tier that answered and the projected counts the consult admitted
/// (ADR-0016 决定 4 dimensions).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationAdmission {
    pub profile_id: String,
    pub projected_task_nodes: u64,
    pub projected_active_working_set: u64,
}

/// Typed reason a materialization request was rejected (the observable
/// window-shrink fact, `[SCALE-MATERIALIZE-001]` /
/// `[SCHED-BACKPRESSURE-001]` posture).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializationRejection {
    /// The tier's active working-set dimension denied the projection
    /// (`WorkingSetAdmissionDenied` on the Task side).
    WorkingSetFull {
        profile_id: String,
        active_count: u64,
        max_active_working_set: u64,
    },
    /// The tier's declared-TaskNode dimension denied the projection
    /// (`TaskNodeAdmissionDenied` on the Task side, ADR-0016 决定 4).
    TaskNodeCapExceeded {
        profile_id: String,
        task_count: u64,
        max_task_nodes: u64,
    },
}

/// Request to resolve one pending materialization request with the
/// Task-side admission verdict. The approval path re-verifies dependency
/// readiness and the declared-revision fence, then commits the request
/// row and the `WAITING_* → MATERIALIZING` voucher in one transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationResolution {
    /// The pending request's exactly-once key.
    pub request_key: IdempotencyKey,
    pub verdict: MaterializationAdmissionVerdict,
    pub resolved_at_ms: u64,
}

/// Lifecycle status of one durable materialization request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MaterializationRequestStatus {
    /// Recorded, awaiting the Task-side admission verdict.
    Pending,
    /// Admission approved; the node crossed into `MATERIALIZING` in the
    /// same transaction (`approved_voucher_id` names the voucher).
    Approved,
    /// Admission rejected (typed reason recorded); the node stayed
    /// `WAITING_*` — the window shrank, the plan did not fail.
    Rejected,
}

impl MaterializationRequestStatus {
    pub(crate) fn decode(value: i64) -> Result<Self, crate::PlanStoreError> {
        match value {
            1 => Ok(Self::Pending),
            2 => Ok(Self::Approved),
            3 => Ok(Self::Rejected),
            _ => Err(crate::PlanStoreError::CorruptRecord(
                "unknown materialization status",
            )),
        }
    }
}

/// Durable readback of one materialization request (either lifecycle
/// half included).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationRequestRecord {
    pub request_id: ReceiptId,
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub idempotency_key: IdempotencyKey,
    /// The node's declared-shape revision observed when the request was
    /// recorded; a reshaping revision fences the resolution
    /// (`[PLAN-DAG-001]`, G4-consistent CAS).
    pub observed_declared_revision: u64,
    pub status: MaterializationRequestStatus,
    /// The admission facts, `Approved` rows only.
    pub admission: Option<MaterializationAdmission>,
    /// The typed rejection, `Rejected` rows only.
    pub rejection: Option<MaterializationRejection>,
    /// The `WAITING_* → MATERIALIZING` voucher an approval committed.
    pub approved_voucher_id: Option<ReceiptId>,
    pub requested_at_ms: u64,
    pub resolved_at_ms: Option<u64>,
}

/// Outcome of one `request_materialization` call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializationRequestDecision {
    /// First execution of this key: the pending request (and any driving
    /// vouchers) committed.
    Requested(MaterializationRequestRecord),
    /// Durable replay: the original record is returned unchanged.
    Replayed(MaterializationRequestRecord),
}

impl MaterializationRequestDecision {
    /// The record this call denotes, whichever branch.
    #[must_use]
    pub fn record(self) -> MaterializationRequestRecord {
        match self {
            Self::Requested(record) | Self::Replayed(record) => record,
        }
    }
}

/// One approved materialization: the resolved request plus the
/// `WAITING_* → MATERIALIZING` voucher committed with it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationApproval {
    pub request: MaterializationRequestRecord,
    pub voucher: NodeTransitionVoucher,
}

/// Outcome of one `resolve_materialization` call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializationResolutionDecision {
    /// First execution: admission approved, the node is `MATERIALIZING`.
    Approved(MaterializationApproval),
    /// First execution: admission rejected (typed reason on the record);
    /// the node stays `WAITING_*`.
    Rejected(MaterializationRequestRecord),
    /// Durable replay of a prior approval.
    ReplayedApproved(MaterializationApproval),
    /// Durable replay of a prior rejection.
    ReplayedRejected(MaterializationRequestRecord),
}

impl MaterializationResolutionDecision {
    /// The request record this call denotes, whichever branch.
    #[must_use]
    pub const fn record(&self) -> &MaterializationRequestRecord {
        match self {
            Self::Approved(approval) | Self::ReplayedApproved(approval) => &approval.request,
            Self::Rejected(record) | Self::ReplayedRejected(record) => record,
        }
    }
}

pub(crate) fn encode_kind(kind: PlanNodeKind) -> i64 {
    kind.encode()
}

pub(crate) fn decode_kind(value: i64) -> Result<PlanNodeKind, crate::PlanStoreError> {
    PlanNodeKind::decode(value)
}

pub(crate) fn encode_state(state: PlanNodeState) -> i64 {
    state.encode()
}

pub(crate) fn decode_state(value: i64) -> Result<PlanNodeState, crate::PlanStoreError> {
    PlanNodeState::decode(value)
}

pub(crate) fn encode_tier(tier: NodeResidencyTier) -> i64 {
    tier.encode()
}

pub(crate) fn decode_tier(value: i64) -> Result<NodeResidencyTier, crate::PlanStoreError> {
    NodeResidencyTier::decode(value)
}
