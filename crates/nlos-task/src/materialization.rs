//! Task-side consumption path for plan materialization requests (W31-A,
//! B4-4; ADR-0016 G3 consult surface).
//!
//! Per ADR-0013 the materialization gate splits across two authorities:
//! the plan authority records the readiness fact (the pending
//! `plan_materialization_requests` row) and commits the verdict; this
//! crate answers the consult. [`SqliteTaskAuthority::answer_plan_materialization`]
//! consumes the request's question — "does one more materialization
//! fit the tier?" — through the existing `ScaleProfile` admission APIs
//! ([`crate::enforce_task_node_admission`] over the declared-TaskNode
//! dimension, ADR-0016 决定 4, and [`crate::enforce_working_set_admission`]
//! over this authority's outstanding `CommitPermit`s) and returns the
//! admission facts an approval records, or the typed admission-denial
//! error a rejection records (the observable window-shrink reason).
//!
//! The consult is read-only: no Task-side durable row is written; the
//! composite materialization facts live plan-side on the resolved
//! request (the nested-receipt posture of the ADR-0013 contract). The
//! mapping from the typed denial to the plan gate's rejection shape is
//! deliberate wiring (caller-owned), keeping the two authority
//! vocabularies decoupled.

use crate::scale::ScaleProfile;
use crate::store::SqliteTaskAuthority;
use crate::{TaskStoreError, enforce_task_node_admission, enforce_working_set_admission};

/// The admission facts an approved materialization carries back to the
/// plan gate: the tier that answered and the projected counts the
/// consult admitted (ADR-0016 决定 4 dimensions).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializationAdmissionFacts {
    /// Tier identifier of the answering profile.
    pub profile_id: &'static str,
    /// Projected declared-TaskNode count including the candidate.
    pub projected_task_nodes: u64,
    /// Projected active working-set count including the candidate.
    pub projected_active_working_set: u64,
}

/// Pure consult over the two `ScaleProfile` dimensions (the free-function
/// twin of [`SqliteTaskAuthority::answer_plan_materialization`]).
///
/// `other_declared_task_nodes` is the plan authority's persisted
/// declared-TaskNode count **excluding** the candidate — the consult
/// projects `+1` for the candidate itself, mirroring the registration
/// gate's projection. `active_working_set` is the current outstanding
/// working-set occupancy.
///
/// # Errors
///
/// Returns [`TaskStoreError::TaskNodeAdmissionDenied`] or
/// [`TaskStoreError::WorkingSetAdmissionDenied`] (in that order — the
/// declared-node dimension is consulted first) as the typed
/// window-shrink reasons, or [`TaskStoreError::EpochExhausted`] on
/// count overflow.
pub fn admit_plan_materialization(
    profile: &ScaleProfile,
    other_declared_task_nodes: u64,
    active_working_set: u64,
) -> Result<MaterializationAdmissionFacts, TaskStoreError> {
    enforce_task_node_admission(profile, other_declared_task_nodes)?;
    enforce_working_set_admission(profile, active_working_set)?;
    Ok(MaterializationAdmissionFacts {
        profile_id: profile.profile_id,
        projected_task_nodes: other_declared_task_nodes
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?,
        projected_active_working_set: active_working_set
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?,
    })
}

impl SqliteTaskAuthority {
    /// Answers one pending plan-side materialization request (the
    /// consumption path the W31-A gate wiring calls): consults the
    /// configured [`ScaleProfile`]'s declared-TaskNode dimension over the
    /// caller-supplied plan-side count and the working-set dimension over
    /// this authority's outstanding `CommitPermit`s.
    ///
    /// # Errors
    ///
    /// Same typed denials as [`admit_plan_materialization`], or a
    /// storage error when the issued-permit count cannot be read.
    pub fn answer_plan_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<MaterializationAdmissionFacts, TaskStoreError> {
        let snapshot = self.inspect_working_set_pressure()?;
        admit_plan_materialization(
            self.scale_profile(),
            other_declared_task_nodes,
            snapshot.active_count,
        )
    }
}
