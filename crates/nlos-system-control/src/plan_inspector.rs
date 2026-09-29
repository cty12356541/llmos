//! Optional [`TaskNodeInspectSource`] adapter backed by the durable
//! [`nlos_plan::SqlitePlanAuthority`] (W32-G, B5-3).
//!
//! Enabled with the crate's `plan` feature; the default control prefix uses
//! [`crate::UnwiredTaskNodeInspectSource`] until a host wires this adapter.

use nlos_plan::{
    NodeResidencyTier, PlanNodeKind, PlanNodeState, PlanStoreError, SqlitePlanAuthority,
};
use nlos_schema::sabi::v1::{
    ContextResidencyTier, PlanNodeKind as WireKind, PlanNodeLifecycleState as WireState,
    RetryDirective, SabiErrorCode, SabiFailure,
};
use nlos_types::{TaskNodeId, TaskPlanId};

use crate::TaskNodeInspectSource;
use crate::control::TaskNodeInspection;

/// Reads bounded plan-node facts through the durable plan authority.
pub struct PlanAuthorityTaskNodeSource<'a> {
    authority: &'a SqlitePlanAuthority,
}

impl<'a> PlanAuthorityTaskNodeSource<'a> {
    #[must_use]
    pub const fn new(authority: &'a SqlitePlanAuthority) -> Self {
        Self { authority }
    }
}

impl TaskNodeInspectSource for PlanAuthorityTaskNodeSource<'_> {
    fn inspect_task_node(
        &self,
        plan_id: [u8; 16],
        node_id: [u8; 16],
    ) -> Result<TaskNodeInspection, SabiFailure> {
        let record = self
            .authority
            .inspect_node(
                TaskPlanId::from_bytes(plan_id),
                TaskNodeId::from_bytes(node_id),
            )
            .map_err(|error| map_plan_error(&error))?
            .ok_or_else(|| not_found("requested task node was not found"))?;
        Ok(TaskNodeInspection {
            plan_id: *record.plan_id.as_bytes(),
            node_id: *record.node_id.as_bytes(),
            kind: plan_node_kind(record.kind),
            state: plan_node_state(record.state),
            declared_revision: record.declared_revision,
            node_digest: record.node_digest.to_vec(),
            transition_count: record.transition_count,
            residency_tier: residency_tier(record.residency_tier),
            residency_transition_count: record.residency_transition_count,
            first_declared_at_ms: record.first_declared_at_ms,
            updated_at_ms: record.updated_at_ms,
        })
    }
}

const fn plan_node_kind(kind: PlanNodeKind) -> WireKind {
    match kind {
        PlanNodeKind::AgentRole => WireKind::AgentRole,
        PlanNodeKind::Executable => WireKind::Executable,
    }
}

const fn plan_node_state(state: PlanNodeState) -> WireState {
    use PlanNodeState as Source;
    match state {
        Source::Declared => WireState::Declared,
        Source::BlockedDependency => WireState::BlockedDependency,
        Source::Eligible => WireState::Eligible,
        Source::WaitingAuthorization => WireState::WaitingAuthorization,
        Source::WaitingResource => WireState::WaitingResource,
        Source::Materializing => WireState::Materializing,
        Source::Active => WireState::Active,
        Source::Checkpointed => WireState::Checkpointed,
        Source::Evicted => WireState::Evicted,
        Source::Rehydrating => WireState::Rehydrating,
        Source::Completed => WireState::Completed,
        Source::Failed => WireState::Failed,
        Source::Cancelled => WireState::Cancelled,
    }
}

const fn residency_tier(tier: NodeResidencyTier) -> ContextResidencyTier {
    use NodeResidencyTier as Source;
    match tier {
        Source::MetadataOnly => ContextResidencyTier::MetadataOnly,
        Source::Cold => ContextResidencyTier::Cold,
        Source::Warm => ContextResidencyTier::Warm,
        Source::Hot => ContextResidencyTier::Hot,
        Source::Running => ContextResidencyTier::Running,
    }
}

fn not_found(message: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

fn map_plan_error(error: &PlanStoreError) -> SabiFailure {
    let (code, retry, safe_message) = match &error {
        PlanStoreError::PlanNotFound(_)
        | PlanStoreError::RevisionNotFound { .. }
        | PlanStoreError::NodeNotFound { .. } => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested plan authority object was not found",
        ),
        PlanStoreError::Sqlite(_) => (
            SabiErrorCode::Durability,
            RetryDirective::RetrySameIdempotencyKey,
            "plan authority storage failure; retry with the same idempotency key",
        ),
        PlanStoreError::DurabilityUnavailable { .. }
        | PlanStoreError::SchemaVersionUnsupported(_)
        | PlanStoreError::CorruptRecord(_)
        | PlanStoreError::LockPoisoned => (
            SabiErrorCode::Durability,
            RetryDirective::DoNotRetry,
            "plan authority durability configuration is unavailable",
        ),
        _ => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "plan authority rejected the inspection request",
        ),
    };
    SabiFailure {
        code: code.into(),
        retry: retry.into(),
        safe_message: safe_message.to_owned(),
    }
}
