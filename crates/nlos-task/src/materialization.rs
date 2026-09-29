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

use nlos_types::{CommitPermitId, TaskId};

use crate::pressure::{WorkingSetReclaimEviction, WorkingSetReclaimExecutionReport};
use crate::scale::ScaleProfile;
use crate::store::SqliteTaskAuthority;
use crate::{TaskStoreError, enforce_task_node_admission, enforce_working_set_admission};

/// Production reclaim×residency drive (W36-P8; W31-G §8.2.5): the Task
/// authority owns permit closure; this boundary is invoked **per
/// victim, before** [`SqliteTaskAuthority::drive_working_set_reclaim`]
/// calls `close_permit`, so a PINNED refusal leaves Task occupancy
/// unchanged. `Err` is fail-closed (PINNED / CAS / consult failure) —
/// never a silent skip after a Task write.
pub trait ReclaimResidencyDrive {
    /// The drive's own failure type (plan-side refusals stay with the
    /// implementation; Task errors convert via `From`).
    type Error;

    /// Records one adjacent residency evict step for the candidate
    /// victims. Invoked with a single pending member (closure receipt
    /// not yet minted) **before** Task permit close.
    ///
    /// # Errors
    ///
    /// Implementation-defined typed refusal (for example a PINNED node).
    fn drive_reclaim_residency(
        &self,
        evictions: &[WorkingSetReclaimEviction],
        executed_at_ms: i64,
    ) -> Result<(), Self::Error>;
}

/// Drive for Tasks that carry no plan binding: residency is not a
/// second ledger for these members. Plan-bound reclaim must pass a
/// drive that records `record_residency_transition`.
pub struct UnlinkedReclaimResidency;

impl ReclaimResidencyDrive for UnlinkedReclaimResidency {
    type Error = TaskStoreError;

    fn drive_reclaim_residency(
        &self,
        evictions: &[WorkingSetReclaimEviction],
        executed_at_ms: i64,
    ) -> Result<(), TaskStoreError> {
        let _ = (evictions, executed_at_ms);
        Ok(())
    }
}

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

    /// Answers the plan-declaration (apply-time) consult over the
    /// declared-TaskNode dimension only (W36-P8; W31-G §8.2.4 — the
    /// declaration half the materialization consult left open): whether
    /// a projected store-wide declared-`TaskNode` population still fits
    /// the configured [`ScaleProfile`]'s `max_task_nodes`. Read-only
    /// cross-authority consult — no Task-side durable write, the same
    /// ADR-0013 posture as [`Self::answer_plan_materialization`].
    ///
    /// # Errors
    ///
    /// Returns [`TaskStoreError::TaskNodeAdmissionDenied`] as the typed
    /// denial; no other failure surface (the projection is caller-owned
    /// plan-side data).
    pub fn answer_plan_declaration(&self, projected_task_nodes: u64) -> Result<(), TaskStoreError> {
        let profile = self.scale_profile();
        if profile.admits_task_nodes(projected_task_nodes) {
            Ok(())
        } else {
            Err(TaskStoreError::TaskNodeAdmissionDenied {
                profile_id: profile.profile_id,
                task_count: projected_task_nodes,
                max_task_nodes: profile.max_task_nodes,
            })
        }
    }

    /// The Task half of the reclaim×residency seam (W36-P8; W31-G §8.2.5):
    /// identities of durably evicted working-set members, in eviction
    /// order. The assembler binds each `task_id` to a plan node; the
    /// plan authority records the matching residency step.
    #[must_use]
    pub fn reclaim_residency_victims(
        report: &WorkingSetReclaimExecutionReport,
    ) -> Vec<(TaskId, CommitPermitId)> {
        report
            .evictions
            .iter()
            .map(|eviction| (eviction.task_id, eviction.permit_id))
            .collect()
    }
}
