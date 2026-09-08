//! Working-set pressure and reclaim policy declaration surface for
//! `[ROAD-B-004]` (`06-架构设计总纲-v0.5.md` §25.2, §28.2).
//!
//! Honest scope of this skeleton:
//!
//! - **Prefix enforcement only.** [`crate::SqliteTaskAuthority::request_commit_permit`]
//!   consults [`WorkingSetPressure::admits`] before issuing a new outstanding
//!   `CommitPermit`; idempotent permit replays bypass the gate. When admission
//!   still passes but the projected active count crosses the soft threshold,
//!   [`working_set_reclaim_advisory`] surfaces a typed
//!   [`WorkingSetReclaimAdvisory`] on [`CommitPermitDecision`] (advisory only;
//!   no reclaim execution). No Materialization Controller, Context Residency
//!   Controller, or soft reclaim path enforces execution yet.
//! - **No rehydrate.** Checkpoint/evict/rehydrate benchmarks and recovery
//!   wiring are registered gaps in `docs/evidence/stage-b/b-task-scale-001.md`.
//! - **Predicate surface.** [`WorkingSetPressure::needs_reclaim`] reports when
//!   the observed active working set crosses the tier's soft threshold;
//!   [`crate::ScaleProfile::admits_active_working_set`] remains the hard
//!   inclusive upper bound checked by [`enforce_working_set_admission`].

use crate::TaskStoreError;
use crate::model::PermitDecision;
use crate::scale::ScaleProfile;

/// One reclaim phase in priority order (`[RSM-RECLAIM-001]` subset).
///
/// Phases are ordered from least to most disruptive; controllers that land
/// later MUST walk the policy in this order rather than jumping to kill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReclaimPhase {
    /// Rebuildable cache, embedding, or derived Context
    /// (`[SCALE-CONTEXT-001]`).
    RebuildableCache,
    /// Degrade background `QoS` before touching foreground work.
    DegradeBackgroundQos,
    /// Checkpoint or evict resident instances (`[SCALE-MATERIALIZE-001]`).
    CheckpointEvict,
    /// Kill or fence as the last resort.
    Kill,
}

/// Versioned reclaim ordering for a Task scale tier.
///
/// This is a **semantic placeholder**: it documents the default degrade path
/// for `[ROAD-B-004]` without binding any runtime controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReclaimPolicy {
    /// Ordered phases; index `0` is reclaimed first.
    pub phases: &'static [ReclaimPhase],
}

/// Default single-node reclaim ordering mirroring `[RSM-RECLAIM-001]`.
pub const TASK_DEFAULT_RECLAIM_POLICY: ReclaimPolicy = ReclaimPolicy {
    phases: &[
        ReclaimPhase::RebuildableCache,
        ReclaimPhase::DegradeBackgroundQos,
        ReclaimPhase::CheckpointEvict,
        ReclaimPhase::Kill,
    ],
};

/// Observed active working-set pressure against one [`ScaleProfile`] tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkingSetPressure<'profile> {
    pub profile: &'profile ScaleProfile,
    pub active_count: u64,
}

impl<'profile> WorkingSetPressure<'profile> {
    #[must_use]
    pub const fn new(profile: &'profile ScaleProfile, active_count: u64) -> Self {
        Self {
            profile,
            active_count,
        }
    }

    /// Soft threshold count derived from the tier's reclaim ratio.
    #[must_use]
    pub const fn threshold(&self) -> u64 {
        self.profile.reclaim_threshold_count()
    }

    /// Effective reclaim ratio percent for this observation (`1..=100`).
    #[must_use]
    pub const fn threshold_ratio(&self) -> u64 {
        self.profile.effective_reclaim_threshold_ratio()
    }

    /// Reports whether `active_count` crossed the soft reclaim threshold.
    ///
    /// Hard admission still uses [`ScaleProfile::admits_active_working_set`];
    /// pressure may become true while admission remains true.
    #[must_use]
    pub const fn needs_reclaim(&self) -> bool {
        self.profile.needs_reclaim(self.active_count)
    }

    /// Hard tier predicate: whether the observed count still fits the tier.
    #[must_use]
    pub const fn admits(&self) -> bool {
        self.profile.admits_active_working_set(self.active_count)
    }
}

/// Typed advisory when a net-new permit issuance crosses the soft reclaim
/// threshold but still fits the tier hard cap.
///
/// Controllers that land later may consult this prefix signal; this slice
/// does not execute any [`ReclaimPolicy`] phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkingSetReclaimAdvisory {
    pub profile_id: &'static str,
    /// Active working-set count after the issued permit (`current + 1`).
    pub projected_active_count: u64,
    pub reclaim_threshold_count: u64,
    pub reclaim_threshold_ratio: u64,
    pub max_active_working_set: u64,
}

/// Observed store-wide working-set pressure against the authority tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkingSetPressureSnapshot {
    pub profile_id: &'static str,
    pub active_count: u64,
    pub reclaim_threshold_count: u64,
    pub needs_reclaim: bool,
    pub admits: bool,
    pub reclaim_advisory: Option<WorkingSetReclaimAdvisory>,
}

/// Linearized `CommitPermit` request outcome plus optional reclaim advisory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitPermitDecision {
    pub permit: PermitDecision,
    pub reclaim_advisory: Option<WorkingSetReclaimAdvisory>,
}

/// Read-side snapshot of the authority's configured tier and issued permit
/// count.
pub fn inspect_working_set_pressure(
    profile: &ScaleProfile,
    active_count: u64,
) -> WorkingSetPressureSnapshot {
    let pressure = WorkingSetPressure::new(profile, active_count);
    let reclaim_advisory = current_working_set_reclaim_advisory(profile, active_count);
    WorkingSetPressureSnapshot {
        profile_id: profile.profile_id,
        active_count,
        reclaim_threshold_count: pressure.threshold(),
        needs_reclaim: pressure.needs_reclaim(),
        admits: pressure.admits(),
        reclaim_advisory,
    }
}

/// Reports whether a projected net-new issuance should surface reclaim
/// advisory after hard admission passes.
///
/// `current_active` is the store-wide issued permit count before the
/// candidate issuance; the advisory consults `current_active + 1`.
pub fn working_set_reclaim_advisory(
    profile: &ScaleProfile,
    current_active: u64,
) -> Result<Option<WorkingSetReclaimAdvisory>, TaskStoreError> {
    let projected = current_active
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    Ok(projected_working_set_reclaim_advisory(profile, projected))
}

fn current_working_set_reclaim_advisory(
    profile: &ScaleProfile,
    active_count: u64,
) -> Option<WorkingSetReclaimAdvisory> {
    let pressure = WorkingSetPressure::new(profile, active_count);
    if pressure.needs_reclaim() && pressure.admits() {
        Some(WorkingSetReclaimAdvisory {
            profile_id: profile.profile_id,
            projected_active_count: active_count,
            reclaim_threshold_count: pressure.threshold(),
            reclaim_threshold_ratio: pressure.threshold_ratio(),
            max_active_working_set: profile.max_active_working_set,
        })
    } else {
        None
    }
}

fn projected_working_set_reclaim_advisory(
    profile: &ScaleProfile,
    projected_active_count: u64,
) -> Option<WorkingSetReclaimAdvisory> {
    let pressure = WorkingSetPressure::new(profile, projected_active_count);
    if pressure.needs_reclaim() && pressure.admits() {
        Some(WorkingSetReclaimAdvisory {
            profile_id: profile.profile_id,
            projected_active_count,
            reclaim_threshold_count: pressure.threshold(),
            reclaim_threshold_ratio: pressure.threshold_ratio(),
            max_active_working_set: profile.max_active_working_set,
        })
    } else {
        None
    }
}

/// Fail-closed when a new outstanding `CommitPermit` would exceed the tier
/// hard active working-set cap.
///
/// `current_active` is the store-wide count of issued permits before the
/// candidate issuance; the gate consults `current_active + 1`.
///
/// # Errors
///
/// Returns [`TaskStoreError::WorkingSetAdmissionDenied`] when the projected
/// count exceeds [`ScaleProfile::max_active_working_set`].
pub fn enforce_working_set_admission(
    profile: &ScaleProfile,
    current_active: u64,
) -> Result<(), TaskStoreError> {
    let projected = current_active
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let pressure = WorkingSetPressure::new(profile, projected);
    if pressure.admits() {
        Ok(())
    } else {
        Err(TaskStoreError::WorkingSetAdmissionDenied {
            profile_id: profile.profile_id,
            active_count: projected,
            max_active_working_set: profile.max_active_working_set,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommitPermitDecision, ReclaimPhase, ReclaimPolicy, WorkingSetPressure,
        WorkingSetReclaimAdvisory, enforce_working_set_admission, inspect_working_set_pressure,
        working_set_reclaim_advisory, TASK_DEFAULT_RECLAIM_POLICY,
    };
    use crate::TaskStoreError;
    use crate::model::PermitDecision;
    use crate::scale::{
        ScaleProfile, DEFAULT_RECLAIM_THRESHOLD_RATIO, TASK_PROFILE_10K, TASK_PROFILE_100K,
    };

    #[test]
    fn default_reclaim_policy_publishes_rsm_ordering() {
        assert_eq!(
            TASK_DEFAULT_RECLAIM_POLICY.phases,
            &[
                ReclaimPhase::RebuildableCache,
                ReclaimPhase::DegradeBackgroundQos,
                ReclaimPhase::CheckpointEvict,
                ReclaimPhase::Kill,
            ]
        );
    }

    #[test]
    fn ten_k_pressure_needs_reclaim_above_threshold_not_at_cap() {
        let threshold = TASK_PROFILE_10K.reclaim_threshold_count();
        assert_eq!(threshold, 512 * 90 / 100);

        let below = WorkingSetPressure::new(&TASK_PROFILE_10K, threshold);
        assert!(!below.needs_reclaim());
        assert!(below.admits());

        let above = WorkingSetPressure::new(&TASK_PROFILE_10K, threshold + 1);
        assert!(above.needs_reclaim());
        assert!(above.admits());

        let at_cap = WorkingSetPressure::new(&TASK_PROFILE_10K, 512);
        assert!(at_cap.needs_reclaim());
        assert!(at_cap.admits());

        let over = WorkingSetPressure::new(&TASK_PROFILE_10K, 513);
        assert!(over.needs_reclaim());
        assert!(!over.admits());
    }

    #[test]
    fn hundred_k_pressure_scales_threshold_with_tier() {
        let pressure = WorkingSetPressure::new(&TASK_PROFILE_100K, 5_120);
        assert_eq!(
            pressure.threshold(),
            TASK_PROFILE_100K.max_active_working_set * 90 / 100
        );
        assert!(pressure.needs_reclaim());
        assert!(pressure.admits());
    }

    #[test]
    fn enforce_admits_at_cap_rejects_one_over() {
        let profile = ScaleProfile {
            profile_id: "task-admission-unit",
            max_task_nodes: 64,
            max_active_working_set: 2,
            reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
        };
        enforce_working_set_admission(&profile, 0).expect("first slot");
        enforce_working_set_admission(&profile, 1).expect("at cap");
        let denied = enforce_working_set_admission(&profile, 2).unwrap_err();
        assert!(matches!(
            denied,
            TaskStoreError::WorkingSetAdmissionDenied {
                profile_id: "task-admission-unit",
                active_count: 3,
                max_active_working_set: 2,
            }
        ));
    }

    #[test]
    fn reclaim_advisory_fires_between_soft_threshold_and_hard_cap() {
        let profile = ScaleProfile {
            profile_id: "task-advisory-unit",
            max_task_nodes: 64,
            max_active_working_set: 2,
            reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
        };
        assert_eq!(profile.reclaim_threshold_count(), 1);

        assert_eq!(
            working_set_reclaim_advisory(&profile, 0).expect("below soft"),
            None
        );
        assert_eq!(
            working_set_reclaim_advisory(&profile, 1).expect("above soft below cap"),
            Some(WorkingSetReclaimAdvisory {
                profile_id: "task-advisory-unit",
                projected_active_count: 2,
                reclaim_threshold_count: 1,
                reclaim_threshold_ratio: DEFAULT_RECLAIM_THRESHOLD_RATIO,
                max_active_working_set: 2,
            })
        );
        assert_eq!(
            working_set_reclaim_advisory(&profile, 2).expect("over cap"),
            None
        );
    }

    #[test]
    fn inspect_pressure_snapshot_matches_current_active_count() {
        let profile = ScaleProfile {
            profile_id: "task-inspect-unit",
            max_task_nodes: 64,
            max_active_working_set: 2,
            reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
        };
        let below = inspect_working_set_pressure(&profile, 1);
        assert!(!below.needs_reclaim);
        assert!(below.admits);
        assert!(below.reclaim_advisory.is_none());

        let advisory_zone = inspect_working_set_pressure(&profile, 2);
        assert!(advisory_zone.needs_reclaim);
        assert!(advisory_zone.admits);
        assert!(advisory_zone.reclaim_advisory.is_some());

        let over = inspect_working_set_pressure(&profile, 3);
        assert!(over.needs_reclaim);
        assert!(!over.admits);
        assert!(over.reclaim_advisory.is_none());
    }

    #[test]
    fn commit_permit_decision_carries_optional_advisory() {
        let advisory = WorkingSetReclaimAdvisory {
            profile_id: "task-advisory-unit",
            projected_active_count: 2,
            reclaim_threshold_count: 1,
            reclaim_threshold_ratio: DEFAULT_RECLAIM_THRESHOLD_RATIO,
            max_active_working_set: 2,
        };
        let decision = CommitPermitDecision {
            permit: PermitDecision::Conflicted {
                reason: crate::PermitConflict::AttemptAlreadyHoldsPermit {
                    permit_id: nlos_types::CommitPermitId::from_bytes([0x11; 16]),
                },
            },
            reclaim_advisory: Some(advisory),
        };
        assert_eq!(decision.reclaim_advisory, Some(advisory));
    }

    #[test]
    fn custom_profile_uses_default_ratio_when_unset() {
        let profile = ScaleProfile {
            profile_id: "task-custom",
            max_task_nodes: 1_000,
            max_active_working_set: 100,
            reclaim_threshold_ratio: None,
        };
        assert_eq!(
            profile.effective_reclaim_threshold_ratio(),
            DEFAULT_RECLAIM_THRESHOLD_RATIO
        );
        let threshold = profile.reclaim_threshold_count();
        assert_eq!(threshold, 90);
        assert!(!profile.needs_reclaim(90));
        assert!(profile.needs_reclaim(91));

        let policy = ReclaimPolicy {
            phases: TASK_DEFAULT_RECLAIM_POLICY.phases,
        };
        assert_eq!(policy.phases.len(), 4);
    }
}
