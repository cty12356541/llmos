//! Compilation of the package manifest `tasks` template segment into a
//! `TaskPlan` proposal (ADR-0016 决定 1, W28-B template face).
//!
//! `[PLAN-OVERRIDE-001]`: the manifest template segment must not become a
//! second declaration dialect. This module holds no schema of its own — it
//! maps [`PackageTaskTemplate`]s field-for-field into `nlos-plan`'s
//! [`ApplyPlanRevisionRequest`], the exact request a caller would declare
//! directly against the plan authority, and nothing else. The manifest
//! answers *where a declaration comes from* (signed, immutable, packaged);
//! the plan authority remains the single owner of *what a declaration is*
//! and whether a declared graph is admissible (cycles, admission bounds).
//!
//! The output is proposal data only: this module opens no plan store and
//! writes nothing durable. Wiring the compiled proposal into an actual
//! `apply_plan_revision` at install/launch time is a later Slice K lane,
//! behind the ADR-0013 verify-then-commit gate.

use std::error::Error;
use std::fmt;

use nlos_artifact::{
    ArtifactError, PackageTaskKind, SignedPackageWithTasks, validate_task_templates,
};
use nlos_plan::{ApplyPlanRevisionRequest, PlanNodeDeclaration, PlanNodeKind};
use nlos_types::IdempotencyKey;

/// Compiles one signed package's `tasks` template segment into the plan
/// proposal a caller would declare directly: `plan_id: None` (the initial
/// revision; the plan authority derives the `TaskPlanId` from the
/// idempotency key), one [`PlanNodeDeclaration`] per template in declared
/// order, dependency order preserved bitwise, and the caller's exactly-once
/// key and observation time.
///
/// The segment shape is validated by the same shared `nlos-artifact`
/// validator the verification path uses, so a segment that already
/// produced a verification receipt can never fail here; the check exists
/// to fail closed on unverified inputs. Package *verification* itself is
/// not re-done: compilation is a pure function over declared data, and in
/// the real wiring its input is a package whose verification receipt
/// exists.
///
/// # Errors
///
/// Returns [`TaskTemplateError::InvalidSegment`] when the segment violates
/// the manifest template shape contract.
pub fn compile_task_templates(
    signed: &SignedPackageWithTasks,
    idempotency_key: IdempotencyKey,
    applied_at_ms: u64,
) -> Result<ApplyPlanRevisionRequest, TaskTemplateError> {
    validate_task_templates(&signed.tasks).map_err(TaskTemplateError::InvalidSegment)?;
    let nodes = signed
        .tasks
        .iter()
        .map(|template| PlanNodeDeclaration {
            node_key: template.node_key,
            kind: compile_kind(template.kind),
            binding_digest: template.binding_digest,
            dependency_keys: template.dependency_keys.clone(),
            input_selectors_digest: template.input_selectors_digest,
            output_contract_digest: template.output_contract_digest,
            policy_digest: template.policy_digest,
            resource_ceiling_digest: template.resource_ceiling_digest,
            conditions: None,
        })
        .collect();
    Ok(ApplyPlanRevisionRequest {
        plan_id: None,
        nodes,
        idempotency_key,
        applied_at_ms,
    })
}

/// Total kind mapping from the manifest face to the plan face. If
/// `nlos-plan` grows a kind variant, this match stops compiling until the
/// manifest face is reconciled — dialect drift is a build failure, never
/// a silent remap.
const fn compile_kind(kind: PackageTaskKind) -> PlanNodeKind {
    match kind {
        PackageTaskKind::AgentRole => PlanNodeKind::AgentRole,
        PackageTaskKind::Executable => PlanNodeKind::Executable,
    }
}

/// Fail-closed typed errors of manifest task-template compilation.
#[derive(Debug)]
pub enum TaskTemplateError {
    /// The declared segment violates the manifest template shape contract
    /// (the shared `nlos-artifact` validator refused it). A segment that
    /// passed `verify_package_with_tasks` can never produce this; the
    /// variant guards compilation of unverified inputs.
    InvalidSegment(ArtifactError),
}

impl fmt::Display for TaskTemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSegment(error) => write!(
                formatter,
                "task template segment violates the manifest shape contract: {error}"
            ),
        }
    }
}

impl Error for TaskTemplateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSegment(error) => Some(error),
        }
    }
}
