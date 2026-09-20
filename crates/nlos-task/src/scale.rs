//! `ScaleProfile` declaration surface for the Task domain
//! (`[ROAD-B-004]` front slice).
//!
//! `[ROAD-B-004]` (`06-架构设计总纲-v0.5.md` §28.2) requires the single-node
//! implementation to publish at least one `ScaleProfile` and to benchmark
//! 10K/100K logical `TaskNodes` across active working-set ratios before any
//! PID-scale Agent capacity is claimed. This module publishes the first
//! named tier as **constants only**.
//!
//! Honest scope of this skeleton:
//!
//! - **Prefix enforcement only.** [`crate::SqliteTaskAuthority::request_commit_permit`]
//!   consults [`crate::WorkingSetPressure::admits`] via the authority's
//!   configured [`ScaleProfile`] before issuing a new outstanding permit,
//!   and [`crate::SqliteTaskAuthority::register_task`] consults
//!   [`Self::admits_task_registrations`] before a net-new durable Task
//!   registration; idempotent permit and registration replays bypass the
//!   gates. Soft reclaim is not enforced yet; gaps remain in
//!   `docs/evidence/stage-b/b-task-scale-001.md`.
//! - **Normalized dimension mapping (ADR-0016 决定 4, W29-A).**
//!   `max_task_nodes` counts declared `TaskNode`s (the persisted
//!   `plan_nodes` count of the `nlos-plan` declaration authority). 口径
//!   switch note: before W29-A (W22-004) the register gate provisionally
//!   consulted `max_task_nodes` over durable `Task` registrations; since
//!   W29-A the registration bound is the independent second explicit
//!   dimension `max_task_registrations`, which is what `register_task`
//!   enforces. The declared-`TaskNode` dimension is consulted through
//!   [`enforce_task_node_admission`] at the plan-association/
//!   materialization boundary (W30-D, registered gap); the published
//!   `TASK_PROFILE_10K` probe numbers measure the registration dimension.
//!   `max_active_working_set` remains carried by outstanding issued
//!   `CommitPermit`s.
//! - The lazy-materialization semantics this tier is measured against are
//!   the **key-scoped query pattern** of the landed store (primary-key /
//!   unique-index lookups only, including the
//!   `commit_permits_single_active` partial unique index); see the probe
//!   `tests/scale_profile_probe.rs` for measured numbers.
//! - **Pressure/reclaim declaration only.** Soft reclaim thresholds and
//!   default degrade ordering live in [`crate::pressure`]; no controller
//!   enforces them yet and rehydrate remains out of scope.

/// Default soft reclaim threshold as a percent of
/// [`ScaleProfile::max_active_working_set`] when
/// [`ScaleProfile::reclaim_threshold_ratio`] is unset.
pub const DEFAULT_RECLAIM_THRESHOLD_RATIO: u64 = 90;

/// One named capacity tier of the single-node Task domain.
///
/// Counts are logical units, not resident resources: per §25.2 of the
/// architecture master plan, registered logical nodes must not be conflated
/// with materialized instances, running fibers, or OS PIDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScaleProfile {
    /// Stable tier identifier (KABI: treat as opaque text).
    pub profile_id: &'static str,
    /// Upper bound for declared logical `TaskNode`s — the persisted
    /// `plan_nodes` count of the plan declaration authority (ADR-0016
    /// 决定 4 normalized 口径; consulted at the plan-association/
    /// materialization boundary, W30-D gap).
    pub max_task_nodes: u64,
    /// Upper bound for durable `Task` registrations — the independent
    /// second explicit dimension kept by 决定 4; this is the dimension
    /// `register_task` admission enforces.
    pub max_task_registrations: u64,
    /// Upper bound for the active working set: concurrently outstanding
    /// (issued) `CommitPermit`s in this tier's probe.
    pub max_active_working_set: u64,
    /// Optional soft reclaim threshold as a percent of
    /// `max_active_working_set` (`1..=100`). When `None`, callers use
    /// [`DEFAULT_RECLAIM_THRESHOLD_RATIO`].
    pub reclaim_threshold_ratio: Option<u64>,
}

/// First published single-node tier: 10K logical task units with a bounded
/// active working set of 512 (~5% ratio), mirroring the measured probe
/// sample in `tests/scale_profile_probe.rs`. 口径 note (ADR-0016 决定 4):
/// the published 10K probe numbers were measured over durable `Task`
/// registrations, which since W29-A are bounded by
/// `max_task_registrations`; the declared-`TaskNode` bound
/// (`max_task_nodes`) keeps the same 10K magnitude and is re-measured by
/// the G5 gate once plan-side counting lands (W30-D/W31).
pub const TASK_PROFILE_10K: ScaleProfile = ScaleProfile {
    profile_id: "task-10k",
    max_task_nodes: 10_000,
    max_task_registrations: 10_000,
    max_active_working_set: 512,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

/// Second published single-node tier: 100K logical task units with a bounded
/// active working set of `5_120` (same ~5% ratio as [`TASK_PROFILE_10K`]).
/// Measured numbers are recorded by the `#[ignore]` probe in
/// `tests/scale_profile_probe.rs` (registration 口径, see
/// [`TASK_PROFILE_10K`]).
pub const TASK_PROFILE_100K: ScaleProfile = ScaleProfile {
    profile_id: "task-100k",
    max_task_nodes: 100_000,
    max_task_registrations: 100_000,
    max_active_working_set: 5_120,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

impl ScaleProfile {
    /// Reports whether `count` declared logical `TaskNode`s fit inside
    /// this tier (决定 4 normalized 口径; see [`Self::max_task_nodes`]).
    #[must_use]
    pub const fn admits_task_nodes(&self, count: u64) -> bool {
        count <= self.max_task_nodes
    }

    /// Reports whether `count` durable `Task` registrations fit inside
    /// this tier (决定 4 second explicit dimension; enforced by
    /// `register_task` admission).
    #[must_use]
    pub const fn admits_task_registrations(&self, count: u64) -> bool {
        count <= self.max_task_registrations
    }

    /// Reports whether `count` active working-set units fit inside this
    /// tier.
    #[must_use]
    pub const fn admits_active_working_set(&self, count: u64) -> bool {
        count <= self.max_active_working_set
    }

    /// Effective reclaim threshold ratio percent (`1..=100`).
    #[must_use]
    pub const fn effective_reclaim_threshold_ratio(&self) -> u64 {
        match self.reclaim_threshold_ratio {
            Some(ratio) => ratio,
            None => DEFAULT_RECLAIM_THRESHOLD_RATIO,
        }
    }

    /// Soft reclaim count derived from `max_active_working_set` and the
    /// effective reclaim ratio.
    #[must_use]
    pub const fn reclaim_threshold_count(&self) -> u64 {
        self.max_active_working_set
            .saturating_mul(self.effective_reclaim_threshold_ratio())
            / 100
    }

    /// Reports whether `active_count` crossed the soft reclaim threshold.
    ///
    /// Hard admission still uses [`Self::admits_active_working_set`].
    #[must_use]
    pub const fn needs_reclaim(&self, active_count: u64) -> bool {
        active_count > self.reclaim_threshold_count()
    }
}

#[cfg(test)]
mod tests {
    use super::{ScaleProfile, TASK_PROFILE_10K, TASK_PROFILE_100K};

    #[test]
    fn task_10k_tier_publishes_declared_constants() {
        assert_eq!(TASK_PROFILE_10K.profile_id, "task-10k");
        assert_eq!(TASK_PROFILE_10K.max_task_nodes, 10_000);
        assert_eq!(TASK_PROFILE_10K.max_task_registrations, 10_000);
        assert_eq!(TASK_PROFILE_10K.max_active_working_set, 512);
    }

    #[test]
    fn task_100k_tier_publishes_declared_constants() {
        assert_eq!(TASK_PROFILE_100K.profile_id, "task-100k");
        assert_eq!(TASK_PROFILE_100K.max_task_nodes, 100_000);
        assert_eq!(TASK_PROFILE_100K.max_task_registrations, 100_000);
        assert_eq!(TASK_PROFILE_100K.max_active_working_set, 5_120);
    }

    #[test]
    fn tier_predicates_are_inclusive_upper_bounds() {
        assert!(TASK_PROFILE_10K.admits_task_nodes(0));
        assert!(TASK_PROFILE_10K.admits_task_nodes(10_000));
        assert!(!TASK_PROFILE_10K.admits_task_nodes(10_001));
        assert!(TASK_PROFILE_10K.admits_task_registrations(10_000));
        assert!(!TASK_PROFILE_10K.admits_task_registrations(10_001));
        assert!(TASK_PROFILE_10K.admits_active_working_set(512));
        assert!(!TASK_PROFILE_10K.admits_active_working_set(513));
        assert!(TASK_PROFILE_100K.admits_task_nodes(100_000));
        assert!(!TASK_PROFILE_100K.admits_task_nodes(100_001));
        assert!(TASK_PROFILE_100K.admits_task_registrations(100_000));
        assert!(!TASK_PROFILE_100K.admits_task_registrations(100_001));
        assert!(TASK_PROFILE_100K.admits_active_working_set(5_120));
        assert!(!TASK_PROFILE_100K.admits_active_working_set(5_121));
    }

    #[test]
    fn task_node_and_registration_dimensions_are_independent() {
        let split = ScaleProfile {
            profile_id: "task-split-dimensions",
            max_task_nodes: 1,
            max_task_registrations: 1_000,
            max_active_working_set: 8,
            reclaim_threshold_ratio: None,
        };
        assert!(!split.admits_task_nodes(2));
        assert!(split.admits_task_registrations(2));
        let mirrored = ScaleProfile {
            profile_id: "task-split-dimensions",
            max_task_nodes: 1_000,
            max_task_registrations: 1,
            max_active_working_set: 8,
            reclaim_threshold_ratio: None,
        };
        assert!(mirrored.admits_task_nodes(2));
        assert!(!mirrored.admits_task_registrations(2));
    }

    #[test]
    fn hundred_k_tier_scales_working_set_from_10k_ratio() {
        assert_eq!(
            TASK_PROFILE_100K.max_active_working_set,
            TASK_PROFILE_10K.max_active_working_set
                * (TASK_PROFILE_100K.max_task_nodes / TASK_PROFILE_10K.max_task_nodes)
        );
    }

    #[test]
    fn custom_profiles_are_representable_without_new_types() {
        let profile = ScaleProfile {
            profile_id: "task-custom",
            max_task_nodes: 50_000,
            max_task_registrations: 50_000,
            max_active_working_set: 2_048,
            reclaim_threshold_ratio: None,
        };
        assert!(profile.admits_task_nodes(50_000));
        assert!(!profile.admits_active_working_set(2_049));
        assert_eq!(profile.reclaim_threshold_count(), 2_048 * 90 / 100);
        assert!(!profile.needs_reclaim(1_843));
        assert!(profile.needs_reclaim(1_844));
    }

    #[test]
    fn reclaim_threshold_predicates_are_strictly_above_soft_cap() {
        let threshold = TASK_PROFILE_10K.reclaim_threshold_count();
        assert!(!TASK_PROFILE_10K.needs_reclaim(threshold));
        assert!(TASK_PROFILE_10K.needs_reclaim(threshold + 1));
        assert!(TASK_PROFILE_10K.admits_active_working_set(threshold + 1));
    }
}
