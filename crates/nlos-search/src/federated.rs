//! Multi-Cell federated non-authoritative candidate aggregation
//! (`[SEM-CHECKPOINT-001]`, `[DIST-FAIL-001]`, `[CONS-DECL-001]`,
//! `[CONS-CTRL-001]`, `[SEM-RETRACT-004]`).
//!
//! Each Cell owns an independent `SemanticAuthority` event log whose scalar
//! `log_seq` is purely store-local (`[SEM-CHECKPOINT-001]`). Federated search
//! therefore:
//!
//! - aggregates shard-local [`AssertionHit`] slices keyed by content-addressed
//!   [`SemanticEventId`] without exposing or comparing any cross-Cell scalar
//!   `log_seq`;
//! - preserves the sorted set of observing `cell_domain_id`s (and any reported
//!   per-shard checkpoint digests) for provenance;
//! - propagates append-only retractions monotonically (`[SEM-RETRACT-004]`): if
//!   *any* shard observed `retracted == true` for an `EventId`, the merged
//!   candidate is marked `retracted = true` regardless of shard arrival order;
//! - enforces `[DIST-FAIL-001]` failure transparency: remote shard failures
//!   (`DeadlineExceeded`, `Unavailable`, `Uncertain`) fail closed with
//!   [`SearchError::RemoteShardFailed`] unless the caller explicitly sets
//!   `allow_partial_on_remote_failure = true`, in which case the report
//!   downgrades `achieved_consistency` to
//!   [`SearchAchievedConsistency::FederatedPartialEventual`] and surfaces every
//!   failed shard in [`FederatedSearchReport::failed_shards`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nlos_capability::CapabilityTarget;
use nlos_semantic::AssertionMode;
use nlos_types::{PrincipalId, SemanticEventId};

use crate::{
    AssertionHit, ControlConsumerKind, SearchAchievedConsistency, SearchError,
    SearchUseRestrictions,
};

/// Explicit remote shard failure taxonomy (`[DIST-FAIL-001]`, v0.5 §26.1).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RemoteSearchFailureReason {
    /// The shard query exceeded its caller-assigned deadline budget.
    DeadlineExceeded { budget_ms: u64 },
    /// The remote Cell or transport endpoint was unreachable or unavailable.
    Unavailable,
    /// Transport or shard execution terminated in an uncertain state.
    Uncertain,
}

impl fmt::Display for RemoteSearchFailureReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeadlineExceeded { budget_ms } => {
                write!(formatter, "deadline exceeded ({budget_ms} ms)")
            }
            Self::Unavailable => formatter.write_str("unavailable"),
            Self::Uncertain => formatter.write_str("uncertain"),
        }
    }
}

/// Outcome reported by one Cell search shard (`[DIST-FAIL-001]`,
/// `[SEM-CHECKPOINT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CellSearchShardOutcome {
    /// The Cell shard responded with zero or more local [`AssertionHit`]s and
    /// an optional verified `checkpoint_digest` pinning its snapshot cut.
    Available {
        cell_domain_id: [u8; 16],
        hits: Vec<AssertionHit>,
        checkpoint_digest: Option<[u8; 32]>,
    },
    /// The Cell shard failed with an explicit `[DIST-FAIL-001]` failure reason
    /// and retryability classification.
    Failed {
        cell_domain_id: [u8; 16],
        reason: RemoteSearchFailureReason,
        retryable: bool,
    },
}

/// Structured summary of one failed Cell shard preserved on partial federated
/// responses and fail-closed errors (`[DIST-FAIL-001]`).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FailedShardSummary {
    pub cell_domain_id: [u8; 16],
    pub reason: RemoteSearchFailureReason,
    pub retryable: bool,
}

/// One deduplicated federated candidate hit (`[SEM-CHECKPOINT-001]`,
/// `[CONS-CTRL-001]`).
///
/// Store-local scalar `log_seq` values are intentionally omitted because
/// scalar sequence numbers are incomparable across Cells
/// (`[SEM-CHECKPOINT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FederatedCandidateHit {
    pub event_id: SemanticEventId,
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub assertion_mode: AssertionMode,
    pub content_digest: [u8; 32],
    pub content_media_type: String,
    /// Earliest `admitted_at_ms` observed across all reporting shards
    /// (commutative `min`, ensuring arrival-order independence).
    pub admitted_at_ms: u64,
    /// Monotonic OR across all reporting shards (`[SEM-RETRACT-004]`): `true`
    /// whenever at least one shard observed a durable retraction for `event_id`.
    pub retracted: bool,
    /// Canonical byte-sorted list of `cell_domain_id`s that observed this
    /// `event_id`.
    pub observed_by_cells: Vec<[u8; 16]>,
    /// Canonical sorted `(cell_domain_id, checkpoint_digest)` pairs from shards
    /// that observed this `event_id` under a pinned checkpoint digest.
    pub observed_checkpoint_digests: Vec<([u8; 16], [u8; 32])>,
    pub achieved_consistency: SearchAchievedConsistency,
    pub use_restrictions: SearchUseRestrictions,
}

impl FederatedCandidateHit {
    /// Enforces `[CONS-CTRL-001]` non-authoritative usage rules on this
    /// federated candidate.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::ControlAuthoritativeUseForbidden`] for any
    /// control-plane gate consumer.
    pub const fn enforce_not_control_authoritative(
        &self,
        purpose: ControlConsumerKind,
    ) -> Result<(), SearchError> {
        self.use_restrictions
            .enforce_not_control_authoritative(purpose)
    }
}

/// Deterministic multi-Cell federated candidate report (`[SEM-CHECKPOINT-001]`,
/// `[DIST-FAIL-001]`, `[CONS-DECL-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FederatedSearchReport {
    pub candidates: Vec<FederatedCandidateHit>,
    pub failed_shards: Vec<FailedShardSummary>,
    pub available_shard_count: usize,
    pub total_unique_candidates: usize,
    pub truncated: bool,
    /// Search candidate aggregation is a pure, side-effect-free read
    /// (`[DIST-FAIL-001]`).
    pub idempotent_read: bool,
    pub achieved_consistency: SearchAchievedConsistency,
    pub use_restrictions: SearchUseRestrictions,
}

impl FederatedSearchReport {
    /// Returns `true` when one or more remote shards failed and the report
    /// represents a degraded partial view (`[DIST-FAIL-001]`).
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.failed_shards.is_empty()
    }

    /// Enforces `[CONS-CTRL-001]` non-authoritative usage rules on this report.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::ControlAuthoritativeUseForbidden`] for any
    /// control-plane gate consumer.
    pub const fn enforce_not_control_authoritative(
        &self,
        purpose: ControlConsumerKind,
    ) -> Result<(), SearchError> {
        self.use_restrictions
            .enforce_not_control_authoritative(purpose)
    }
}

struct CandidateAccumulator {
    event_id: SemanticEventId,
    scope: CapabilityTarget,
    issuer: PrincipalId,
    assertion_mode: AssertionMode,
    content_digest: [u8; 32],
    content_media_type: String,
    admitted_at_ms: u64,
    retracted: bool,
    observed_by_cells: BTreeSet<[u8; 16]>,
    observed_checkpoint_digests: BTreeSet<([u8; 16], [u8; 32])>,
}

fn merge_hit_into_accumulator(
    merged_by_id: &mut BTreeMap<SemanticEventId, CandidateAccumulator>,
    cell_domain_id: [u8; 16],
    checkpoint_digest: Option<[u8; 32]>,
    hit: &AssertionHit,
) -> Result<(), SearchError> {
    if let Some(existing) = merged_by_id.get_mut(&hit.event_id) {
        if existing.scope != hit.scope
            || existing.issuer != hit.issuer
            || existing.assertion_mode != hit.assertion_mode
            || existing.content_digest != hit.content_digest
            || existing.content_media_type != hit.content_media_type
        {
            return Err(SearchError::CorruptRecord(
                "federated shards disagree on canonical assertion fields for same event_id",
            ));
        }
        existing.admitted_at_ms = existing.admitted_at_ms.min(hit.admitted_at_ms);
        existing.retracted |= hit.retracted;
        existing.observed_by_cells.insert(cell_domain_id);
        if let Some(digest) = checkpoint_digest {
            existing
                .observed_checkpoint_digests
                .insert((cell_domain_id, digest));
        }
    } else {
        let mut observed_by_cells = BTreeSet::new();
        observed_by_cells.insert(cell_domain_id);
        let mut observed_checkpoint_digests = BTreeSet::new();
        if let Some(digest) = checkpoint_digest {
            observed_checkpoint_digests.insert((cell_domain_id, digest));
        }
        merged_by_id.insert(
            hit.event_id,
            CandidateAccumulator {
                event_id: hit.event_id,
                scope: hit.scope,
                issuer: hit.issuer,
                assertion_mode: hit.assertion_mode,
                content_digest: hit.content_digest,
                content_media_type: hit.content_media_type.clone(),
                admitted_at_ms: hit.admitted_at_ms,
                retracted: hit.retracted,
                observed_by_cells,
                observed_checkpoint_digests,
            },
        );
    }
    Ok(())
}

/// Aggregates shard-level [`AssertionHit`] outcomes across multiple Cells
/// without assuming any cross-Cell scalar `log_seq` (`[SEM-CHECKPOINT-001]`,
/// `[DIST-FAIL-001]`, `[SEM-RETRACT-004]`).
///
/// - Identical [`SemanticEventId`]s across Cells deduplicate into one
///   [`FederatedCandidateHit`] carrying the sorted union of observing
///   `cell_domain_id`s, the minimum observed `admitted_at_ms`, and monotonic
///   `retracted` propagation (`true` if *any* shard observed retraction).
/// - Surviving candidates are ordered deterministically by
///   `(admitted_at_ms, event_id.into_bytes())` and truncated to `limit`, making
///   the output 100% independent of shard arrival order or local `log_seq`.
/// - When any shard is [`CellSearchShardOutcome::Failed`] and
///   `allow_partial_on_remote_failure == false`, aggregation fails closed with
///   [`SearchError::RemoteShardFailed`]. When `allow_partial_on_remote_failure
///   == true`, partial results return with
///   `achieved_consistency = SearchAchievedConsistency::FederatedPartialEventual`
///   and explicit `failed_shards`.
///
/// # Errors
///
/// Fails closed with:
/// - [`SearchError::InvalidLimit`] when `limit == 0`;
/// - [`SearchError::RemoteShardFailed`] when at least one shard failed and
///   `allow_partial_on_remote_failure == false`;
/// - [`SearchError::CorruptRecord`] if two shards report conflicting immutable
///   canonical fields (`scope`, `issuer`, `assertion_mode`, `content_digest`,
///   or `content_media_type`) for the same [`SemanticEventId`].
pub fn aggregate_federated_candidates(
    shards: &[CellSearchShardOutcome],
    limit: usize,
    allow_partial_on_remote_failure: bool,
) -> Result<FederatedSearchReport, SearchError> {
    if limit == 0 {
        return Err(SearchError::InvalidLimit);
    }

    let mut failed_set: BTreeSet<FailedShardSummary> = BTreeSet::new();
    let mut available_shard_count: usize = 0;
    let mut all_available_have_checkpoint = true;

    for shard in shards {
        match shard {
            CellSearchShardOutcome::Available {
                checkpoint_digest, ..
            } => {
                available_shard_count += 1;
                if checkpoint_digest.is_none() {
                    all_available_have_checkpoint = false;
                }
            }
            CellSearchShardOutcome::Failed {
                cell_domain_id,
                reason,
                retryable,
            } => {
                failed_set.insert(FailedShardSummary {
                    cell_domain_id: *cell_domain_id,
                    reason: *reason,
                    retryable: *retryable,
                });
            }
        }
    }

    let failed_shards: Vec<FailedShardSummary> = failed_set.into_iter().collect();
    if !failed_shards.is_empty() && !allow_partial_on_remote_failure {
        return Err(SearchError::RemoteShardFailed(failed_shards));
    }

    let mut merged_by_id: BTreeMap<SemanticEventId, CandidateAccumulator> = BTreeMap::new();
    for shard in shards {
        if let CellSearchShardOutcome::Available {
            cell_domain_id,
            hits,
            checkpoint_digest,
        } = shard
        {
            for hit in hits {
                merge_hit_into_accumulator(
                    &mut merged_by_id,
                    *cell_domain_id,
                    *checkpoint_digest,
                    hit,
                )?;
            }
        }
    }

    let achieved_consistency = if !failed_shards.is_empty() {
        SearchAchievedConsistency::FederatedPartialEventual
    } else if available_shard_count > 0 && all_available_have_checkpoint {
        SearchAchievedConsistency::CheckpointBoundedSnapshot
    } else {
        SearchAchievedConsistency::EventualLiveScan
    };

    let total_unique_candidates = merged_by_id.len();
    let mut candidates: Vec<FederatedCandidateHit> = merged_by_id
        .into_values()
        .map(|acc| FederatedCandidateHit {
            event_id: acc.event_id,
            scope: acc.scope,
            issuer: acc.issuer,
            assertion_mode: acc.assertion_mode,
            content_digest: acc.content_digest,
            content_media_type: acc.content_media_type,
            admitted_at_ms: acc.admitted_at_ms,
            retracted: acc.retracted,
            observed_by_cells: acc.observed_by_cells.into_iter().collect(),
            observed_checkpoint_digests: acc.observed_checkpoint_digests.into_iter().collect(),
            achieved_consistency,
            use_restrictions: SearchUseRestrictions::NON_AUTHORITATIVE,
        })
        .collect();

    // Deterministic cross-Cell presentation ordering (`[SEM-CHECKPOINT-001]`):
    // never relies on any store-local scalar `log_seq`.
    candidates.sort_by_key(|candidate| (candidate.admitted_at_ms, candidate.event_id.into_bytes()));
    let truncated = candidates.len() > limit;
    candidates.truncate(limit);

    Ok(FederatedSearchReport {
        candidates,
        failed_shards,
        available_shard_count,
        total_unique_candidates,
        truncated,
        idempotent_read: true,
        achieved_consistency,
        use_restrictions: SearchUseRestrictions::NON_AUTHORITATIVE,
    })
}
