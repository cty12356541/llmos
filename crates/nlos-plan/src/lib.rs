//! Durable single-authority TaskPlan/TaskNode declaration store for NLOS
//! (ADR-0016 决定 2, ROAD-B-004 state face, W28-A skeleton).
//!
//! This crate owns the *state face* of the `TaskPlan`/`TaskNode`
//! declaration surface decided by
//! [ADR-0016](../../docs/management/adrs/0016-task-plan-declaration-surface.md):
//!
//! - its own `SQLite` database (mirroring the `nlos-task` authority
//!   structure: WAL/FULL durability verified on open, one linear migration
//!   chain, `STRICT` tables guarded by DDL triggers, a process-local
//!   writer mutex ahead of `BEGIN IMMEDIATE`);
//! - domain-separated [`TaskPlanId`]/[`TaskNodeId`] derivation (the
//!   `nlos-types` nominal-ID pattern): the authority assigns every identity,
//!   callers only supply declaration-local `node_key` names;
//! - `plan_revisions`, an immutable digest chain: every revision produces a
//!   new digest chained to its parent (`[PLAN-DAG-001]`), and already
//!   executed nodes keep their original revision/digest — the G1 gate;
//! - `plan_nodes`, the bounded durable metadata per logical node
//!   (`[SCALE-LOGICAL-001]` posture), advanced only through
//!   `plan_node_transitions` vouchers (§25.2.1 state machine, one immutable
//!   voucher per legal edge, CAS on state and declared revision);
//! - idempotency keys on both write faces: replays return the durably
//!   recorded original receipt/voucher, never a second effect.
//!
//! Explicitly remaining outside this crate (per ADR-0016 lane splits):
//! the production consult wiring that maps the Task authority's
//! admission answer onto the scheduler's consult boundary (assembler
//! territory, slice-k), dispatch decisions (派发决策留控制器 — the W31-F
//! scheduler only selects nodes for materialization and drives the
//! W31-A gate), and any IPC/CLI surface.

mod materialization;
mod model;
mod residency;
mod resolver;
mod scheduler;
mod schema;
mod selector;
mod store;

use std::error::Error;
use std::fmt;

pub use model::{
    ApplyPlanRevisionRequest, ChainVerification, EcosystemEntityKind, EcosystemEntityState,
    EcosystemResolutionDecision, EcosystemResolutionHandle, EcosystemSelector,
    EcosystemSourceLookup, GenerationExpectation, MAX_DECLARED_NODES_PER_REVISION,
    MAX_DEPENDENCIES_PER_NODE, NodeResidencyTier, NodeResidencyView, NodeTransitionDecision,
    NodeTransitionRequest, NodeTransitionVoucher, PlanNodeDeclaration, PlanNodeKind,
    PlanNodeRecord, PlanNodeState, PlanResolutionDecision, PlanResolutionHandle,
    PlanRevisionDecision, PlanRevisionReceipt, PlanRevisionSelector, PlanView,
    ResidencyTransitionDecision, ResidencyTransitionRequest, ResidencyTransitionVoucher,
    ResolveEcosystemRequest, ResolvePlanRequest, ResolvedPlanNode,
};
pub use model::{
    MaterializationAdmission, MaterializationAdmissionVerdict, MaterializationApproval,
    MaterializationRejection, MaterializationRequest, MaterializationRequestDecision,
    MaterializationRequestRecord, MaterializationRequestStatus, MaterializationResolution,
    MaterializationResolutionDecision,
};
use nlos_types::{IdempotencyKey, ReceiptId, TaskNodeId, TaskPlanId};
pub use scheduler::{
    AdmissionConsult, AdmissionConsultOutcome, DEFAULT_DECISION_LOG_CAPACITY,
    MaterializationScheduler, SchedulerDecision, SchedulerDecisionRecord, SchedulerPassSummary,
    SelectionEntry, SelectionKind, SelectionReport, SelectionSkipReason, SkipEntry,
};
pub use selector::{EcosystemResolutionError, EcosystemSelectorSource};
pub use store::SqlitePlanAuthority;

/// Errors produced by the durable plan authority.
///
/// Storage-level failures mirror the other NLOS authorities; domain
/// violations (stale fences, illegal transitions, frozen shapes) are typed
/// so callers can distinguish them from retryable conditions.
#[derive(Debug)]
pub enum PlanStoreError {
    Sqlite(rusqlite::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    CorruptRecord(&'static str),
    LockPoisoned,
    /// No plan with the given ID exists.
    PlanNotFound(TaskPlanId),
    /// No revision with the given number exists under the plan.
    RevisionNotFound {
        plan_id: TaskPlanId,
        revision: u64,
    },
    /// No node with the given ID exists under the plan.
    NodeNotFound {
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
    },
    /// An idempotency key was replayed with different request content.
    IdempotencyConflict,
    /// A revision re-declared an execution-frozen node with a different
    /// shape (`[PLAN-DAG-001]` final clause; G1). The durable node keeps
    /// its original revision and digest.
    FrozenNodeShapeRewrite {
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        /// The frozen node's durable declared revision.
        declared_revision: u64,
    },
    /// The requested node state transition is not a legal §25.2.1 edge.
    IllegalNodeTransition {
        node_id: TaskNodeId,
        from: PlanNodeState,
        to: PlanNodeState,
    },
    /// The requested residency tier transition is not a legal edge of
    /// the conservative W31-E set (a single adjacent step along the
    /// 议题 28 chain; no self-loops, no skips, no `PINNED`).
    IllegalResidencyTransition {
        node_id: TaskNodeId,
        from: NodeResidencyTier,
        to: NodeResidencyTier,
    },
    /// The requested residency tier transition's `from_tier` does not
    /// match the node's durable tier (tier CAS failure).
    ResidencyTierCasMismatch {
        node_id: TaskNodeId,
        expected_from: NodeResidencyTier,
        current: NodeResidencyTier,
    },
    /// The transition's expected declared revision does not match the
    /// node's durable declared revision (`[PLAN-DAG-001]` fence).
    StaleNodeRevision {
        node_id: TaskNodeId,
        expected: u64,
        current: u64,
    },
    /// The dependency graph of a declared revision contains a cycle
    /// (`[PLAN-DAG-001]`).
    PlanCycle,
    /// The dependency graph a resolution was computed over contains a
    /// cycle; the members name the cycle participants (`[PLAN-DAG-001]`
    /// requires a DAG, `[PLAN-DEPENDENCY-001]` fails closed). Reachable
    /// only from inconsistent durable shape rows — the apply path already
    /// refuses cyclic declarations with [`PlanStoreError::PlanCycle`].
    PlanCycleMembers {
        plan_id: TaskPlanId,
        revision: u64,
        /// `TaskNodeId`s on the cycle, deterministically sorted.
        members: Vec<TaskNodeId>,
    },
    /// The revision carries no durable declared-shape rows (applied before
    /// schema v2 persisted per-revision shape, or the rows were lost).
    /// Resolving it fails closed; the shape is never silently re-derived
    /// from mutable current rows.
    RevisionShapeUnavailable {
        plan_id: TaskPlanId,
        revision: u64,
    },
    /// No resolution receipt with the given id exists.
    ResolutionNotFound(ReceiptId),
    /// The requested node state transition's `from_state` does not match
    /// the node's durable state (state CAS failure).
    NodeStateCasMismatch {
        node_id: TaskNodeId,
        expected_from: PlanNodeState,
        current: PlanNodeState,
    },
    /// A dependency edge references a `node_key` not declared in the same
    /// revision.
    UnknownDependency {
        node_key: [u8; 16],
    },
    /// The declaration or transition request violates a structural rule
    /// (empty node set, duplicate `node_key`, self-dependency, admission
    /// bounds, or a timestamp that precedes durable history).
    InvalidRequest {
        reason: &'static str,
    },
    /// The materialization gate refused to open a request: at least one
    /// declared dependency of the node's pinned revision shape is not
    /// `COMPLETED` (W31-A, G3 falsification #1;
    /// `[PLAN-LAZY-001]`). `unresolved` names the dependency nodes,
    /// deterministically sorted.
    DependenciesNotReady {
        node_id: TaskNodeId,
        unresolved: Vec<TaskNodeId>,
    },
    /// No materialization request with the given exactly-once key exists.
    MaterializationRequestNotFound(IdempotencyKey),
    /// The node already carries an in-flight pending materialization
    /// request under a different key; one gate round per node.
    MaterializationRequestAlreadyPending {
        node_id: TaskNodeId,
        /// The pending round's exactly-once key (diagnostic only).
        pending_key: IdempotencyKey,
    },
    /// The node's durable state cannot await materialization (past the
    /// execution boundary, evicted, or terminal).
    NodeNotAwaitingMaterialization {
        node_id: TaskNodeId,
        current: PlanNodeState,
    },
    /// No ecosystem source is registered for the selector's entity kind
    /// (W36-P7: the source's `kinds()` declaration does not cover it);
    /// the resolution fails closed instead of panicking or guessing.
    EcosystemSourceUnavailable {
        kind: crate::model::EcosystemEntityKind,
    },
    /// The source answered that the selector's entity id is unknown — a
    /// typed miss, never silently treated as "resolved against nothing".
    EcosystemEntityNotFound {
        kind: crate::model::EcosystemEntityKind,
        entity_id: [u8; 16],
    },
    /// The selector's expected generation does not match the entity's
    /// current generation (G4 fence, ecosystem half: stale generation ⇒
    /// typed fail-closed in either direction — behind or ahead).
    StaleEcosystemGeneration {
        kind: crate::model::EcosystemEntityKind,
        entity_id: [u8; 16],
        expected: u64,
        current: u64,
    },
    /// No ecosystem resolution receipt with the given id exists.
    EcosystemResolutionNotFound(ReceiptId),
    /// A stored ecosystem receipt row carries an entity kind outside the
    /// closed enum (fail-closed readback; the kind domain grows only via
    /// schema migrations).
    EcosystemKindUnknown(i64),
}

impl fmt::Display for PlanStoreError {
    #[allow(clippy::too_many_lines)] // One match arm per typed error keeps diagnostics exhaustive.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite plan authority failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported plan authority schema version {version}"
                )
            }
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::LockPoisoned => formatter.write_str("plan authority writer lock is poisoned"),
            Self::PlanNotFound(id) => write!(formatter, "plan {id:?} does not exist"),
            Self::RevisionNotFound { plan_id, revision } => {
                write!(
                    formatter,
                    "revision {revision} does not exist under plan {plan_id:?}"
                )
            }
            Self::NodeNotFound { plan_id, node_id } => {
                write!(
                    formatter,
                    "node {node_id:?} does not exist under plan {plan_id:?}"
                )
            }
            Self::IdempotencyConflict => {
                formatter.write_str("idempotency key was rebound to different request content")
            }
            Self::FrozenNodeShapeRewrite {
                plan_id,
                node_id,
                declared_revision,
            } => write!(
                formatter,
                "node {node_id:?} of plan {plan_id:?} is execution-frozen at revision {declared_revision} and cannot be reshaped (PLAN-DAG-001)"
            ),
            Self::IllegalNodeTransition { node_id, from, to } => write!(
                formatter,
                "node {node_id:?} transition {from:?} -> {to:?} is not a legal edge"
            ),
            Self::IllegalResidencyTransition { node_id, from, to } => write!(
                formatter,
                "node {node_id:?} residency transition {from:?} -> {to:?} is not a legal tier edge"
            ),
            Self::ResidencyTierCasMismatch {
                node_id,
                expected_from,
                current,
            } => write!(
                formatter,
                "node {node_id:?} residency tier CAS expected {expected_from:?} but found {current:?}"
            ),
            Self::StaleNodeRevision {
                node_id,
                expected,
                current,
            } => write!(
                formatter,
                "node {node_id:?} declared revision CAS expected {expected} but found {current}"
            ),
            Self::PlanCycle => formatter.write_str("declared dependency graph contains a cycle"),
            Self::PlanCycleMembers {
                plan_id,
                revision,
                members,
            } => write!(
                formatter,
                "revision {revision} of plan {plan_id:?} contains a dependency cycle among {} node(s):",
                members.len()
            ),
            Self::RevisionShapeUnavailable { plan_id, revision } => write!(
                formatter,
                "revision {revision} of plan {plan_id:?} has no durable declared shape to resolve"
            ),
            Self::ResolutionNotFound(resolution_id) => {
                write!(
                    formatter,
                    "plan resolution receipt {resolution_id:?} does not exist"
                )
            }
            Self::NodeStateCasMismatch {
                node_id,
                expected_from,
                current,
            } => write!(
                formatter,
                "node {node_id:?} state CAS expected {expected_from:?} but found {current:?}"
            ),
            Self::UnknownDependency { node_key } => write!(
                formatter,
                "dependency references node key {node_key:02x?} not declared in this revision"
            ),
            Self::InvalidRequest { reason } => {
                write!(formatter, "invalid plan request: {reason}")
            }
            Self::DependenciesNotReady {
                node_id,
                unresolved,
            } => write!(
                formatter,
                "node {node_id:?} has {} unresolved dependency/dependencies; materialization gate refuses (PLAN-LAZY-001)",
                unresolved.len()
            ),
            Self::MaterializationRequestNotFound(key) => {
                write!(formatter, "materialization request {key:?} does not exist")
            }
            Self::MaterializationRequestAlreadyPending {
                node_id,
                pending_key,
            } => write!(
                formatter,
                "node {node_id:?} already has a pending materialization request ({pending_key:?})"
            ),
            Self::NodeNotAwaitingMaterialization { node_id, current } => write!(
                formatter,
                "node {node_id:?} in state {current:?} cannot await materialization"
            ),
            Self::EcosystemSourceUnavailable { kind } => write!(
                formatter,
                "no ecosystem selector source is registered for kind {kind:?} (fail closed)"
            ),
            Self::EcosystemEntityNotFound { kind, entity_id } => write!(
                formatter,
                "ecosystem entity {entity_id:02x?} of kind {kind:?} does not exist"
            ),
            Self::StaleEcosystemGeneration {
                kind,
                entity_id,
                expected,
                current,
            } => write!(
                formatter,
                "ecosystem entity {entity_id:02x?} of kind {kind:?} generation CAS expected {expected} but found {current}"
            ),
            Self::EcosystemResolutionNotFound(resolution_id) => {
                write!(
                    formatter,
                    "ecosystem resolution receipt {resolution_id:?} does not exist"
                )
            }
            Self::EcosystemKindUnknown(value) => write!(
                formatter,
                "stored ecosystem receipt carries unknown entity kind {value}"
            ),
        }
    }
}

impl Error for PlanStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for PlanStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
