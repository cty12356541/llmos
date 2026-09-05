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
//! - **Declaration, not enforcement.** No `TaskAuthority` path consults a
//!   `ScaleProfile` yet; wiring tier limits into registration/admission is
//!   future work and is registered as a gap in
//!   `docs/evidence/stage-b/b-task-scale-001.md`.
//! - **Provisional dimension mapping.** The durable `TaskPlan`/`TaskNode`
//!   declaration surface is not landed in this crate (`TaskSpec` carries no
//!   plan field; the crate docs list TaskPlan/TaskNode materialization as
//!   out of scope). Until that surface exists, `max_task_nodes` is
//!   provisionally carried by durable [`crate::TaskSpec`] registrations (the
//!   only declared-unit surface in this crate) and `max_active_working_set`
//!   by outstanding issued `CommitPermit`s (the only bounded
//!   active-concurrency surface). The nominal [`nlos_types::TaskPlanId`] /
//!   [`nlos_types::TaskNodeId`] identities already exist but bind nothing
//!   durable yet.
//! - The lazy-materialization semantics this tier is measured against are
//!   the **key-scoped query pattern** of the landed store (primary-key /
//!   unique-index lookups only, including the
//!   `commit_permits_single_active` partial unique index); see the probe
//!   `tests/scale_profile_probe.rs` for measured numbers.

/// One named capacity tier of the single-node Task domain.
///
/// Counts are logical units, not resident resources: per §25.2 of the
/// architecture master plan, registered logical nodes must not be conflated
/// with materialized instances, running fibers, or OS PIDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScaleProfile {
    /// Stable tier identifier (KABI: treat as opaque text).
    pub profile_id: &'static str,
    /// Upper bound for logical task units. Provisionally measured over
    /// durable `Task` registrations until the `TaskPlan`/`TaskNode`
    /// declaration surface lands.
    pub max_task_nodes: u64,
    /// Upper bound for the active working set: concurrently outstanding
    /// (issued) `CommitPermit`s in this tier's probe.
    pub max_active_working_set: u64,
}

/// First published single-node tier: 10K logical task units with a bounded
/// active working set of 512 (~5% ratio), mirroring the measured probe
/// sample in `tests/scale_profile_probe.rs`.
pub const TASK_PROFILE_10K: ScaleProfile = ScaleProfile {
    profile_id: "task-10k",
    max_task_nodes: 10_000,
    max_active_working_set: 512,
};

/// Second published single-node tier: 100K logical task units with a bounded
/// active working set of `5_120` (same ~5% ratio as [`TASK_PROFILE_10K`]).
/// Measured numbers are recorded by the `#[ignore]` probe in
/// `tests/scale_profile_probe.rs`.
pub const TASK_PROFILE_100K: ScaleProfile = ScaleProfile {
    profile_id: "task-100k",
    max_task_nodes: 100_000,
    max_active_working_set: 5_120,
};

impl ScaleProfile {
    /// Reports whether `count` logical task units fit inside this tier.
    #[must_use]
    pub const fn admits_task_nodes(&self, count: u64) -> bool {
        count <= self.max_task_nodes
    }

    /// Reports whether `count` active working-set units fit inside this
    /// tier.
    #[must_use]
    pub const fn admits_active_working_set(&self, count: u64) -> bool {
        count <= self.max_active_working_set
    }
}

#[cfg(test)]
mod tests {
    use super::{ScaleProfile, TASK_PROFILE_10K, TASK_PROFILE_100K};

    #[test]
    fn task_10k_tier_publishes_declared_constants() {
        assert_eq!(TASK_PROFILE_10K.profile_id, "task-10k");
        assert_eq!(TASK_PROFILE_10K.max_task_nodes, 10_000);
        assert_eq!(TASK_PROFILE_10K.max_active_working_set, 512);
    }

    #[test]
    fn task_100k_tier_publishes_declared_constants() {
        assert_eq!(TASK_PROFILE_100K.profile_id, "task-100k");
        assert_eq!(TASK_PROFILE_100K.max_task_nodes, 100_000);
        assert_eq!(TASK_PROFILE_100K.max_active_working_set, 5_120);
    }

    #[test]
    fn tier_predicates_are_inclusive_upper_bounds() {
        assert!(TASK_PROFILE_10K.admits_task_nodes(0));
        assert!(TASK_PROFILE_10K.admits_task_nodes(10_000));
        assert!(!TASK_PROFILE_10K.admits_task_nodes(10_001));
        assert!(TASK_PROFILE_10K.admits_active_working_set(512));
        assert!(!TASK_PROFILE_10K.admits_active_working_set(513));
        assert!(TASK_PROFILE_100K.admits_task_nodes(100_000));
        assert!(!TASK_PROFILE_100K.admits_task_nodes(100_001));
        assert!(TASK_PROFILE_100K.admits_active_working_set(5_120));
        assert!(!TASK_PROFILE_100K.admits_active_working_set(5_121));
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
            max_active_working_set: 2_048,
        };
        assert!(profile.admits_task_nodes(50_000));
        assert!(!profile.admits_active_working_set(2_049));
    }
}
