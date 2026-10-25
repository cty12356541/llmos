//! Cell-local partition autonomy gate (`[DIST-LOCAL-001]`) and explicit remote
//! call failure / retry / uncertainty semantics (`[DIST-FAIL-001]`).
//!
//! When a Cell loses connectivity to control-plane peers (`Partitioned`) or is
//! mid-convergence (`Reconnecting`), it MUST NOT assume remote success or grant
//! itself fresh authority:
//! - Only pure local compute or spend strictly inside an already-granted,
//!   unexpired, epoch-matching `QuotaLease` and a locally provable, unexpired,
//!   non-revoked `Capability` may proceed (`[DIST-LOCAL-001]`).
//! - Quota expansion, lease renewal, uncached remote authority mutation, and
//!   cross-Cell canonical publication without local authority fail closed with
//!   [`PartitionViolation::ControlPlaneRequiredDuringPartition`].
//! - Remote calls across Cell boundaries carry explicit [`IdempotencyKey`],
//!   deadline, and retry-budget contracts ([`RemoteCallContract`]), surfacing
//!   distinct [`RemoteCallOutcome`] states (`Unavailable`,
//!   `DeadlineExceededUncertain`, `Partial`) rather than masking in-flight
//!   side-effecting timeouts as clean local errors (`[DIST-FAIL-001]`).

use std::error::Error;
use std::fmt;

use nlos_types::IdempotencyKey;
use sha2::{Digest, Sha256};

use crate::{CellEpoch, CellFence};

/// Domain separator for [`PartitionAutonomyReceipt`] digests.
pub const PARTITION_AUTONOMY_RECEIPT_DOMAIN: &[u8] = b"llmos/cell-partition-autonomy-receipt/v1";

/// Domain separator for [`RemoteCallOutcome::Partial`] uncertainty digests.
pub const REMOTE_CALL_PARTIAL_UNCERTAINTY_DOMAIN: &[u8] =
    b"llmos/remote-call-partial-uncertainty/v1";

/// Coarse phase of [`ControlPlaneLinkState`] used in compact fail-closed
/// diagnostics (`[DIST-LOCAL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneLinkPhase {
    /// Control-plane connectivity is healthy.
    Connected,
    /// Control-plane connectivity is severed and local authority is frozen.
    Partitioned,
    /// Reconnection convergence is in progress under an advanced epoch.
    Reconnecting,
}

/// Connectivity state of a Cell's control-plane link (`[DIST-LOCAL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneLinkState {
    /// Control-plane connectivity is healthy at `verified_fence`.
    Connected {
        /// Current verified local Cell fence.
        verified_fence: CellFence,
        /// Monotonic logical tick of the latest verified control-plane contact.
        last_contact_tick: u64,
    },
    /// Control-plane connectivity is severed; local authority is frozen at
    /// `frozen_fence` (`[DIST-LOCAL-001]`).
    Partitioned {
        /// Cell fence frozen when the partition was detected.
        frozen_fence: CellFence,
        /// Monotonic logical tick at which the partition began.
        partitioned_since_tick: u64,
        /// Caller-supplied diagnostic reason code for auditability.
        reason_code: u16,
    },
    /// Control-plane link has returned and reconnection convergence is in
    /// progress from `prior_frozen_fence` to `candidate_fence`
    /// (`[LEASE-LOSS-001]`, `[LEASE-RECON-001]`).
    Reconnecting {
        /// Fence that was frozen during the partition window.
        prior_frozen_fence: CellFence,
        /// Advanced fence with bumped `CellEpoch` and `CellFencingToken`.
        candidate_fence: CellFence,
        /// Monotonic logical tick at which reconnection convergence began.
        started_tick: u64,
    },
}

impl ControlPlaneLinkState {
    /// Coarse phase discriminant of this link state.
    #[must_use]
    pub const fn phase(&self) -> ControlPlaneLinkPhase {
        match self {
            Self::Connected { .. } => ControlPlaneLinkPhase::Connected,
            Self::Partitioned { .. } => ControlPlaneLinkPhase::Partitioned,
            Self::Reconnecting { .. } => ControlPlaneLinkPhase::Reconnecting,
        }
    }

    /// Returns `true` when in [`Self::Connected`].
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// Returns `true` when in [`Self::Partitioned`].
    #[must_use]
    pub const fn is_partitioned(&self) -> bool {
        matches!(self, Self::Partitioned { .. })
    }

    /// Returns `true` when in [`Self::Reconnecting`].
    #[must_use]
    pub const fn is_reconnecting(&self) -> bool {
        matches!(self, Self::Reconnecting { .. })
    }

    /// Returns `true` when control-plane operations are gated by partition
    /// autonomy rules (`Partitioned` or `Reconnecting`).
    #[must_use]
    pub const fn requires_local_autonomy_gate(&self) -> bool {
        matches!(self, Self::Partitioned { .. } | Self::Reconnecting { .. })
    }

    /// Returns the authoritative [`CellFence`] for this state (`verified_fence`
    /// when connected, `frozen_fence` when partitioned, `candidate_fence` when
    /// reconnecting).
    #[must_use]
    pub const fn authoritative_fence(&self) -> CellFence {
        match *self {
            Self::Connected { verified_fence, .. } => verified_fence,
            Self::Partitioned { frozen_fence, .. } => frozen_fence,
            Self::Reconnecting {
                candidate_fence, ..
            } => candidate_fence,
        }
    }

    const fn wire_tag(&self) -> u8 {
        match self {
            Self::Connected { .. } => 1,
            Self::Partitioned { .. } => 2,
            Self::Reconnecting { .. } => 3,
        }
    }
}

/// Locally verifiable proof bundle required to admit pure compute or local
/// prepaid quota spend during a control-plane partition (`[DIST-LOCAL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalAutonomyProof {
    /// Inclusive logical tick until which the cached/local `Capability` is valid.
    pub capability_valid_until_tick: u64,
    /// Whether local revocation state marks the `Capability` as revoked.
    pub capability_revoked: bool,
    /// Whether the `Capability` signature and lineage were verified locally
    /// without requiring an uncached remote authority round-trip.
    pub capability_locally_verified: bool,
    /// `CellEpoch` bound on the local `QuotaLease`.
    pub lease_epoch: CellEpoch,
    /// Optional exclusive logical tick at which the `QuotaLease` TTL expires
    /// (`now_tick < expires_at` required when `Some`, matching `nlos-lease`
    /// `LeaseInstant` expiry semantics).
    pub lease_expires_at_tick: Option<u64>,
    /// Remaining locally prepaid balance on the `QuotaLease`.
    pub lease_remaining: u64,
    /// Spend requested by this local operation (`0` for pure local compute).
    pub requested_spend: u64,
}

impl LocalAutonomyProof {
    /// Constructs a [`LocalAutonomyProof`] with explicit capability and lease
    /// bounds.
    #[must_use]
    pub const fn new(
        capability_valid_until_tick: u64,
        capability_revoked: bool,
        capability_locally_verified: bool,
        lease_epoch: CellEpoch,
        lease_expires_at_tick: Option<u64>,
        lease_remaining: u64,
        requested_spend: u64,
    ) -> Self {
        Self {
            capability_valid_until_tick,
            capability_revoked,
            capability_locally_verified,
            lease_epoch,
            lease_expires_at_tick,
            lease_remaining,
            requested_spend,
        }
    }
}

/// Action classes evaluated by [`PartitionAutonomyGate`] (`[DIST-LOCAL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PartitionedActionKind {
    /// Pure local compute (`requested_spend == 0`) or local spend strictly
    /// within an already-granted, unexpired `QuotaLease` and locally verified
    /// `Capability`.
    LocalPureComputeOrAuthorizedSpend(LocalAutonomyProof),
    /// Requesting fresh quota expansion or lease TTL renewal from a remote or
    /// partitioned control plane.
    RequestQuotaExpansionOrRenewal,
    /// Publishing canonical state across Cells without local authority.
    CrossCellCanonicalPublishWithoutLocalAuthority,
    /// Mutating uncached remote authority state across a severed control plane.
    UncachedRemoteAuthorityMutation,
}

impl PartitionedActionKind {
    /// Human-readable static label for diagnostics and audit logs.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::LocalPureComputeOrAuthorizedSpend(_) => "local_pure_compute_or_authorized_spend",
            Self::RequestQuotaExpansionOrRenewal => "request_quota_expansion_or_renewal",
            Self::CrossCellCanonicalPublishWithoutLocalAuthority => {
                "cross_cell_canonical_publish_without_local_authority"
            }
            Self::UncachedRemoteAuthorityMutation => "uncached_remote_authority_mutation",
        }
    }
}

/// Machine-verifiable receipt proving that [`PartitionAutonomyGate`] admitted a
/// local action at a specific tick and [`CellFence`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionAutonomyReceipt {
    admitted_at_tick: u64,
    link_state: ControlPlaneLinkState,
    verified_fence: CellFence,
    authorized_spend: u64,
    remaining_after_spend: u64,
    receipt_digest: [u8; 32],
}

impl PartitionAutonomyReceipt {
    #[must_use]
    fn new(
        admitted_at_tick: u64,
        link_state: ControlPlaneLinkState,
        verified_fence: CellFence,
        authorized_spend: u64,
        remaining_after_spend: u64,
    ) -> Self {
        let receipt_digest = Self::compute_digest(
            admitted_at_tick,
            link_state,
            verified_fence,
            authorized_spend,
            remaining_after_spend,
        );
        Self {
            admitted_at_tick,
            link_state,
            verified_fence,
            authorized_spend,
            remaining_after_spend,
            receipt_digest,
        }
    }

    fn compute_digest(
        admitted_at_tick: u64,
        link_state: ControlPlaneLinkState,
        verified_fence: CellFence,
        authorized_spend: u64,
        remaining_after_spend: u64,
    ) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(PARTITION_AUTONOMY_RECEIPT_DOMAIN);
        hasher.update([link_state.wire_tag()]);
        hasher.update(admitted_at_tick.to_be_bytes());
        hasher.update(verified_fence.identity().as_bytes());
        hasher.update(verified_fence.node_boot_generation().get().to_be_bytes());
        hasher.update(verified_fence.epoch().get().to_be_bytes());
        hasher.update(verified_fence.fencing_token().get().to_be_bytes());
        hasher.update(authorized_spend.to_be_bytes());
        hasher.update(remaining_after_spend.to_be_bytes());
        hasher.finalize().into()
    }

    /// Logical tick at which the action was admitted.
    #[must_use]
    pub const fn admitted_at_tick(&self) -> u64 {
        self.admitted_at_tick
    }

    /// Control-plane link state observed at admission time.
    #[must_use]
    pub const fn link_state(&self) -> ControlPlaneLinkState {
        self.link_state
    }

    /// Authoritative [`CellFence`] verified at admission time.
    #[must_use]
    pub const fn verified_fence(&self) -> CellFence {
        self.verified_fence
    }

    /// Spend authorized by this admission (`0` for pure compute or control-plane
    /// actions while connected).
    #[must_use]
    pub const fn authorized_spend(&self) -> u64 {
        self.authorized_spend
    }

    /// Remaining local lease balance after subtracting `authorized_spend`.
    #[must_use]
    pub const fn remaining_after_spend(&self) -> u64 {
        self.remaining_after_spend
    }

    /// Domain-separated `SHA-256` digest binding the receipt fields.
    #[must_use]
    pub const fn receipt_digest(&self) -> [u8; 32] {
        self.receipt_digest
    }

    /// Recomputes and verifies [`Self::receipt_digest`] fail-closed.
    #[must_use]
    pub fn verify_digest(&self) -> bool {
        Self::compute_digest(
            self.admitted_at_tick,
            self.link_state,
            self.verified_fence,
            self.authorized_spend,
            self.remaining_after_spend,
        ) == self.receipt_digest
    }
}

/// Fail-closed refusals emitted by [`PartitionAutonomyGate`] (`[DIST-LOCAL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PartitionViolation {
    /// Cached/local `Capability` cannot be proven valid locally or has been
    /// revoked.
    CapabilityNotLocallyProvable {
        /// Whether local signature/lineage verification succeeded.
        capability_locally_verified: bool,
        /// Whether the capability is marked revoked.
        capability_revoked: bool,
    },
    /// Cached/local `Capability` validity window has expired (`now_tick > valid_until_tick`).
    CapabilityExpiredDuringPartition {
        /// Supplied current logical tick.
        now_tick: u64,
        /// Inclusive expiration tick of the capability.
        valid_until_tick: u64,
    },
    /// `QuotaLease` TTL has expired (`now_tick >= expires_at_tick`).
    QuotaLeaseExpiredDuringPartition {
        /// Supplied current logical tick.
        now_tick: u64,
        /// Exclusive expiration tick of the quota lease.
        expires_at_tick: u64,
    },
    /// Requested spend exceeds the frozen local `QuotaLease` remaining balance.
    QuotaFrozenInsufficientRemaining {
        /// Spend requested by the caller.
        requested_spend: u64,
        /// Locally available lease remainder.
        lease_remaining: u64,
    },
    /// Action requires live control-plane coordination while in `Partitioned`
    /// or `Reconnecting`.
    ControlPlaneRequiredDuringPartition {
        /// Static label of the rejected action kind.
        action: &'static str,
        /// Coarse link phase that barred the action.
        link_phase: ControlPlaneLinkPhase,
    },
    /// `QuotaLease` epoch does not match the authoritative `CellFence` epoch.
    LeaseEpochMismatchDuringPartition {
        /// Epoch carried on the `QuotaLease`.
        lease_epoch: CellEpoch,
        /// Expected authoritative `CellEpoch`.
        expected_epoch: CellEpoch,
    },
    /// Presented [`CellFence`] does not match the gate's authoritative fence.
    FenceMismatchDuringPartition {
        /// Caller-presented fence.
        presented: CellFence,
        /// Gate's authoritative fence.
        expected: CellFence,
    },
    /// Invalid [`ControlPlaneLinkState`] state-machine transition.
    InvalidStateTransition {
        /// Current gate phase.
        current_phase: ControlPlaneLinkPhase,
        /// Attempted transition name.
        attempted: &'static str,
    },
    /// Supplied logical tick regressed below the gate's watermark.
    NonMonotonicTick {
        /// Previously observed logical tick.
        previous_tick: u64,
        /// Regressing logical tick supplied by the caller.
        supplied_tick: u64,
    },
    /// Reconnection candidate fence did not advance `CellEpoch` and
    /// `CellFencingToken` past the frozen partition fence (`[LEASE-LOSS-001]`).
    ReconnectingEpochNotAdvanced {
        /// Frozen partition fence.
        prior_fence: CellFence,
        /// Candidate reconnection fence.
        candidate_fence: CellFence,
    },
}

impl fmt::Display for PartitionViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapabilityNotLocallyProvable {
                capability_locally_verified,
                capability_revoked,
            } => write!(
                formatter,
                "capability not locally provable during partition (verified={capability_locally_verified}, revoked={capability_revoked})"
            ),
            Self::CapabilityExpiredDuringPartition {
                now_tick,
                valid_until_tick,
            } => write!(
                formatter,
                "capability expired during partition: now_tick {now_tick} > valid_until_tick {valid_until_tick}"
            ),
            Self::QuotaLeaseExpiredDuringPartition {
                now_tick,
                expires_at_tick,
            } => write!(
                formatter,
                "QuotaLease expired during partition: now_tick {now_tick} >= expires_at_tick {expires_at_tick}"
            ),
            Self::QuotaFrozenInsufficientRemaining {
                requested_spend,
                lease_remaining,
            } => write!(
                formatter,
                "QuotaLease frozen balance insufficient: requested {requested_spend} > remaining {lease_remaining}"
            ),
            Self::ControlPlaneRequiredDuringPartition { action, link_phase } => write!(
                formatter,
                "action '{action}' requires live control plane, forbidden in {link_phase:?}"
            ),
            Self::LeaseEpochMismatchDuringPartition {
                lease_epoch,
                expected_epoch,
            } => write!(
                formatter,
                "QuotaLease epoch {} does not match authoritative Cell epoch {}",
                lease_epoch.get(),
                expected_epoch.get()
            ),
            Self::FenceMismatchDuringPartition {
                presented,
                expected,
            } => write!(
                formatter,
                "presented fence {presented:?} does not match gate authoritative fence {expected:?}"
            ),
            Self::InvalidStateTransition {
                current_phase,
                attempted,
            } => write!(
                formatter,
                "invalid ControlPlaneLinkState transition '{attempted}' from {current_phase:?}"
            ),
            Self::NonMonotonicTick {
                previous_tick,
                supplied_tick,
            } => write!(
                formatter,
                "non-monotonic partition gate tick: supplied {supplied_tick} < previous {previous_tick}"
            ),
            Self::ReconnectingEpochNotAdvanced {
                prior_fence,
                candidate_fence,
            } => write!(
                formatter,
                "reconnecting fence {candidate_fence:?} must advance epoch and fencing_token past frozen fence {prior_fence:?}"
            ),
        }
    }
}

impl Error for PartitionViolation {}

/// Cell-local partition autonomy state machine and fail-closed admission gate
/// (`[DIST-LOCAL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionAutonomyGate {
    state: ControlPlaneLinkState,
    last_observed_tick: u64,
}

impl PartitionAutonomyGate {
    /// Initializes a gate in [`ControlPlaneLinkState::Connected`] at
    /// `verified_fence` and `now_tick`.
    #[must_use]
    pub const fn new_connected(verified_fence: CellFence, now_tick: u64) -> Self {
        Self {
            state: ControlPlaneLinkState::Connected {
                verified_fence,
                last_contact_tick: now_tick,
            },
            last_observed_tick: now_tick,
        }
    }

    /// Initializes a gate directly in [`ControlPlaneLinkState::Partitioned`].
    #[must_use]
    pub const fn new_partitioned(
        frozen_fence: CellFence,
        partitioned_since_tick: u64,
        reason_code: u16,
    ) -> Self {
        Self {
            state: ControlPlaneLinkState::Partitioned {
                frozen_fence,
                partitioned_since_tick,
                reason_code,
            },
            last_observed_tick: partitioned_since_tick,
        }
    }

    /// Current [`ControlPlaneLinkState`].
    #[must_use]
    pub const fn state(&self) -> ControlPlaneLinkState {
        self.state
    }

    /// Highest logical tick observed across state transitions.
    #[must_use]
    pub const fn last_observed_tick(&self) -> u64 {
        self.last_observed_tick
    }

    fn verify_monotonic_tick(&self, now_tick: u64) -> Result<(), PartitionViolation> {
        if now_tick < self.last_observed_tick {
            return Err(PartitionViolation::NonMonotonicTick {
                previous_tick: self.last_observed_tick,
                supplied_tick: now_tick,
            });
        }
        Ok(())
    }

    fn fence_is_monotonic_successor(prior: CellFence, next: CellFence) -> bool {
        prior.identity() == next.identity()
            && next.node_boot_generation() >= prior.node_boot_generation()
            && next.epoch() >= prior.epoch()
            && next.fencing_token() >= prior.fencing_token()
    }

    /// Refreshes a healthy [`ControlPlaneLinkState::Connected`] fence and
    /// contact tick.
    ///
    /// # Errors
    ///
    /// Fails closed if not currently `Connected`, if `now_tick` regresses, or
    /// if `verified_fence` regresses or names a different Cell.
    pub fn record_control_plane_contact(
        &mut self,
        verified_fence: CellFence,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionViolation> {
        self.verify_monotonic_tick(now_tick)?;
        match self.state {
            ControlPlaneLinkState::Connected {
                verified_fence: prior,
                ..
            } => {
                if !Self::fence_is_monotonic_successor(prior, verified_fence) {
                    return Err(PartitionViolation::FenceMismatchDuringPartition {
                        presented: verified_fence,
                        expected: prior,
                    });
                }
                let next_state = ControlPlaneLinkState::Connected {
                    verified_fence,
                    last_contact_tick: now_tick,
                };
                self.state = next_state;
                self.last_observed_tick = now_tick;
                Ok(next_state)
            }
            current => Err(PartitionViolation::InvalidStateTransition {
                current_phase: current.phase(),
                attempted: "record_control_plane_contact",
            }),
        }
    }

    /// Cuts the control-plane link into [`ControlPlaneLinkState::Partitioned`],
    /// freezing local authority at `frozen_fence` (`[DIST-LOCAL-001]`).
    ///
    /// Idempotent when already `Partitioned` at the same `(frozen_fence,
    /// reason_code)`. Also permits re-entering `Partitioned` from an
    /// interrupted `Reconnecting` attempt at its `candidate_fence`.
    ///
    /// # Errors
    ///
    /// Fails closed on tick regression or fence mismatch/regression.
    pub fn enter_partition(
        &mut self,
        frozen_fence: CellFence,
        now_tick: u64,
        reason_code: u16,
    ) -> Result<ControlPlaneLinkState, PartitionViolation> {
        self.verify_monotonic_tick(now_tick)?;
        match self.state {
            ControlPlaneLinkState::Connected { verified_fence, .. } => {
                if !Self::fence_is_monotonic_successor(verified_fence, frozen_fence) {
                    return Err(PartitionViolation::FenceMismatchDuringPartition {
                        presented: frozen_fence,
                        expected: verified_fence,
                    });
                }
                let next = ControlPlaneLinkState::Partitioned {
                    frozen_fence,
                    partitioned_since_tick: now_tick,
                    reason_code,
                };
                self.state = next;
                self.last_observed_tick = now_tick;
                Ok(next)
            }
            ControlPlaneLinkState::Partitioned {
                frozen_fence: existing_fence,
                partitioned_since_tick,
                reason_code: existing_reason,
            } => {
                if existing_fence == frozen_fence && existing_reason == reason_code {
                    self.last_observed_tick = now_tick;
                    return Ok(ControlPlaneLinkState::Partitioned {
                        frozen_fence: existing_fence,
                        partitioned_since_tick,
                        reason_code: existing_reason,
                    });
                }
                Err(PartitionViolation::InvalidStateTransition {
                    current_phase: self.state.phase(),
                    attempted: "enter_partition",
                })
            }
            ControlPlaneLinkState::Reconnecting {
                candidate_fence, ..
            } => {
                if !Self::fence_is_monotonic_successor(candidate_fence, frozen_fence) {
                    return Err(PartitionViolation::FenceMismatchDuringPartition {
                        presented: frozen_fence,
                        expected: candidate_fence,
                    });
                }
                let next = ControlPlaneLinkState::Partitioned {
                    frozen_fence,
                    partitioned_since_tick: now_tick,
                    reason_code,
                };
                self.state = next;
                self.last_observed_tick = now_tick;
                Ok(next)
            }
        }
    }

    /// Transitions from [`ControlPlaneLinkState::Partitioned`] into
    /// [`ControlPlaneLinkState::Reconnecting`] once the local `CellEpoch` and
    /// `CellFencingToken` have strictly advanced (`[LEASE-LOSS-001]`,
    /// `[LEASE-FENCE-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if not `Partitioned` (unless idempotently replaying the
    /// exact `candidate_fence`), if `now_tick` regresses, or if
    /// `candidate_fence` did not strictly advance `epoch` and `fencing_token`.
    pub fn begin_reconnection(
        &mut self,
        candidate_fence: CellFence,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionViolation> {
        self.verify_monotonic_tick(now_tick)?;
        match self.state {
            ControlPlaneLinkState::Partitioned { frozen_fence, .. } => {
                if candidate_fence.identity() != frozen_fence.identity()
                    || candidate_fence.node_boot_generation() < frozen_fence.node_boot_generation()
                {
                    return Err(PartitionViolation::FenceMismatchDuringPartition {
                        presented: candidate_fence,
                        expected: frozen_fence,
                    });
                }
                if candidate_fence.epoch() <= frozen_fence.epoch()
                    || candidate_fence.fencing_token() <= frozen_fence.fencing_token()
                {
                    return Err(PartitionViolation::ReconnectingEpochNotAdvanced {
                        prior_fence: frozen_fence,
                        candidate_fence,
                    });
                }
                let next = ControlPlaneLinkState::Reconnecting {
                    prior_frozen_fence: frozen_fence,
                    candidate_fence,
                    started_tick: now_tick,
                };
                self.state = next;
                self.last_observed_tick = now_tick;
                Ok(next)
            }
            ControlPlaneLinkState::Reconnecting {
                prior_frozen_fence,
                candidate_fence: existing_candidate,
                started_tick,
            } if existing_candidate == candidate_fence => {
                self.last_observed_tick = now_tick;
                Ok(ControlPlaneLinkState::Reconnecting {
                    prior_frozen_fence,
                    candidate_fence: existing_candidate,
                    started_tick,
                })
            }
            current => Err(PartitionViolation::InvalidStateTransition {
                current_phase: current.phase(),
                attempted: "begin_reconnection",
            }),
        }
    }

    /// Completes reconnection convergence from
    /// [`ControlPlaneLinkState::Reconnecting`] back to
    /// [`ControlPlaneLinkState::Connected`] at `verified_fence`.
    ///
    /// # Errors
    ///
    /// Fails closed if not `Reconnecting` (unless idempotently replaying the
    /// exact `verified_fence` while already `Connected`), if `now_tick`
    /// regresses, or if `verified_fence` does not match or succeed
    /// `candidate_fence`.
    pub fn complete_reconnection(
        &mut self,
        verified_fence: CellFence,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionViolation> {
        self.verify_monotonic_tick(now_tick)?;
        match self.state {
            ControlPlaneLinkState::Reconnecting {
                candidate_fence, ..
            } => {
                if !Self::fence_is_monotonic_successor(candidate_fence, verified_fence) {
                    return Err(PartitionViolation::FenceMismatchDuringPartition {
                        presented: verified_fence,
                        expected: candidate_fence,
                    });
                }
                let next = ControlPlaneLinkState::Connected {
                    verified_fence,
                    last_contact_tick: now_tick,
                };
                self.state = next;
                self.last_observed_tick = now_tick;
                Ok(next)
            }
            ControlPlaneLinkState::Connected {
                verified_fence: existing,
                ..
            } if existing == verified_fence => {
                let next = ControlPlaneLinkState::Connected {
                    verified_fence,
                    last_contact_tick: now_tick,
                };
                self.state = next;
                self.last_observed_tick = now_tick;
                Ok(next)
            }
            current => Err(PartitionViolation::InvalidStateTransition {
                current_phase: current.phase(),
                attempted: "complete_reconnection",
            }),
        }
    }

    /// Evaluates whether `action` may execute at `now_tick` under
    /// `current_fence` (`[DIST-LOCAL-001]`).
    ///
    /// During `Partitioned` or `Reconnecting`, ONLY
    /// [`PartitionedActionKind::LocalPureComputeOrAuthorizedSpend`] is eligible
    /// for admission, and only when:
    /// 1. `current_fence == self.state.authoritative_fence()`,
    /// 2. `capability_locally_verified && !capability_revoked`,
    /// 3. `now_tick <= capability_valid_until_tick`,
    /// 4. `lease_epoch == current_fence.epoch()`,
    /// 5. `lease_expires_at_tick.is_none_or(|exp| now_tick < exp)`, and
    /// 6. `requested_spend <= lease_remaining`.
    ///
    /// # Errors
    ///
    /// Returns a specific [`PartitionViolation`] variant for every fail-closed
    /// refusal condition.
    pub fn admit_action(
        &self,
        now_tick: u64,
        current_fence: &CellFence,
        action: &PartitionedActionKind,
    ) -> Result<PartitionAutonomyReceipt, PartitionViolation> {
        let expected_fence = self.state.authoritative_fence();
        if *current_fence != expected_fence {
            return Err(PartitionViolation::FenceMismatchDuringPartition {
                presented: *current_fence,
                expected: expected_fence,
            });
        }

        match action {
            PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(proof) => {
                if !proof.capability_locally_verified || proof.capability_revoked {
                    return Err(PartitionViolation::CapabilityNotLocallyProvable {
                        capability_locally_verified: proof.capability_locally_verified,
                        capability_revoked: proof.capability_revoked,
                    });
                }
                if now_tick > proof.capability_valid_until_tick {
                    return Err(PartitionViolation::CapabilityExpiredDuringPartition {
                        now_tick,
                        valid_until_tick: proof.capability_valid_until_tick,
                    });
                }
                if proof.lease_epoch != current_fence.epoch() {
                    return Err(PartitionViolation::LeaseEpochMismatchDuringPartition {
                        lease_epoch: proof.lease_epoch,
                        expected_epoch: current_fence.epoch(),
                    });
                }
                if let Some(expires_at_tick) = proof.lease_expires_at_tick
                    && now_tick >= expires_at_tick
                {
                    return Err(PartitionViolation::QuotaLeaseExpiredDuringPartition {
                        now_tick,
                        expires_at_tick,
                    });
                }
                if proof.requested_spend > proof.lease_remaining {
                    return Err(PartitionViolation::QuotaFrozenInsufficientRemaining {
                        requested_spend: proof.requested_spend,
                        lease_remaining: proof.lease_remaining,
                    });
                }
                Ok(PartitionAutonomyReceipt::new(
                    now_tick,
                    self.state,
                    *current_fence,
                    proof.requested_spend,
                    proof.lease_remaining - proof.requested_spend,
                ))
            }
            PartitionedActionKind::RequestQuotaExpansionOrRenewal
            | PartitionedActionKind::CrossCellCanonicalPublishWithoutLocalAuthority
            | PartitionedActionKind::UncachedRemoteAuthorityMutation => {
                if self.state.requires_local_autonomy_gate() {
                    return Err(PartitionViolation::ControlPlaneRequiredDuringPartition {
                        action: action.label(),
                        link_phase: self.state.phase(),
                    });
                }
                Ok(PartitionAutonomyReceipt::new(
                    now_tick,
                    self.state,
                    *current_fence,
                    0,
                    0,
                ))
            }
        }
    }
}

/// Explicit retry / recovery directive attached to non-success
/// [`RemoteCallOutcome`] variants (`[DIST-FAIL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteRetryDirective {
    /// Pre-dispatch `Unavailable` (zero remote side effects executed) or pure
    /// read-only timeout with remaining retry budget: caller may retry ONLY by
    /// reusing the exact same [`IdempotencyKey`].
    SafeToRetryWithSameKey,
    /// Side-effecting call timed out in flight or partially mutated non-atomic
    /// remote state: remote effect is indeterminate, so blind retry or masking
    /// as a clean local error is FORBIDDEN until reconciliation / status query.
    ReconcileBeforeRetry,
    /// Multi-shard call achieved partial completion with shard-idempotent
    /// semantics and remaining retry budget: retry only the failed shards.
    RetryFailedShardsOnly,
    /// Retry budget is exhausted (`attempt_number >= max_retries`) or failure
    /// is terminal; surface failure without further automatic retries.
    DoNotRetryBudgetExhausted,
}

/// Fail-closed refusal when attempting to construct a retry attempt that
/// violates `[DIST-FAIL-001]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteRetryRefusal {
    /// Remote call already succeeded; retrying is unnecessary.
    AlreadySucceeded,
    /// In-flight side-effecting call is `Uncertain`; caller MUST reconcile
    /// remote state before retrying (`[DIST-FAIL-001]`).
    UncertainSideEffectRequiresReconciliation {
        /// Original idempotency key bound to the uncertain call.
        idempotency_key: IdempotencyKey,
    },
    /// Retry budget `max_retries` has been exhausted.
    RetryBudgetExhausted {
        /// Current attempt index.
        attempt_number: u32,
        /// Configured maximum retry count.
        max_retries: u32,
    },
}

impl fmt::Display for RemoteRetryRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadySucceeded => {
                formatter.write_str("remote call already succeeded; retry refused")
            }
            Self::UncertainSideEffectRequiresReconciliation { idempotency_key } => write!(
                formatter,
                "uncertain side-effecting remote call {idempotency_key:?} requires reconciliation before retry"
            ),
            Self::RetryBudgetExhausted {
                attempt_number,
                max_retries,
            } => write!(
                formatter,
                "remote call retry budget exhausted: attempt {attempt_number} >= max_retries {max_retries}"
            ),
        }
    }
}

impl Error for RemoteRetryRefusal {}

/// Explicit cross-Cell remote call contract carrying idempotency, deadline,
/// retry budget, and side-effect classification (`[DIST-FAIL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteCallContract {
    /// Stable 16-byte logical call identifier across retry attempts.
    pub call_id: [u8; 16],
    /// Mandatory idempotency key preserved across all retries of this call.
    pub idempotency_key: IdempotencyKey,
    /// Logical tick deadline by which a response must arrive.
    pub deadline_tick: u64,
    /// Maximum allowed retry attempts after initial attempt `0`.
    pub max_retries: u32,
    /// Zero-based attempt number (`0` for initial attempt).
    pub attempt_number: u32,
    /// Whether the remote operation mutates remote state or consumes authority.
    pub side_effecting: bool,
}

impl RemoteCallContract {
    /// Constructs a [`RemoteCallContract`] for initial or retried execution.
    #[must_use]
    pub const fn new(
        call_id: [u8; 16],
        idempotency_key: IdempotencyKey,
        deadline_tick: u64,
        max_retries: u32,
        attempt_number: u32,
        side_effecting: bool,
    ) -> Self {
        Self {
            call_id,
            idempotency_key,
            deadline_tick,
            max_retries,
            attempt_number,
            side_effecting,
        }
    }

    /// Returns `true` if `attempt_number < max_retries`.
    #[must_use]
    pub const fn has_retry_budget_remaining(&self) -> bool {
        self.attempt_number < self.max_retries
    }

    /// Classifies a verified remote response into [`RemoteCallOutcome::Succeeded`].
    #[must_use]
    pub const fn classify_succeeded<T>(
        &self,
        value: T,
        observed_epoch: CellEpoch,
        idempotency_replayed: bool,
    ) -> RemoteCallOutcome<T> {
        RemoteCallOutcome::Succeeded {
            value,
            observed_epoch,
            idempotency_replayed,
        }
    }

    /// Classifies a pre-dispatch link/peer unreachability failure (where the
    /// request is known NOT to have entered remote execution) into
    /// [`RemoteCallOutcome::Unavailable`].
    #[must_use]
    pub const fn classify_pre_dispatch_unavailable<T>(&self) -> RemoteCallOutcome<T> {
        let retry_directive = if self.has_retry_budget_remaining() {
            RemoteRetryDirective::SafeToRetryWithSameKey
        } else {
            RemoteRetryDirective::DoNotRetryBudgetExhausted
        };
        RemoteCallOutcome::Unavailable {
            deadline_tick: self.deadline_tick,
            retry_directive,
            idempotency_key: self.idempotency_key,
        }
    }

    /// Classifies an in-flight deadline expiration (where request dispatch
    /// occurred or could not be ruled out before partition/timeout) into
    /// [`RemoteCallOutcome::DeadlineExceededUncertain`] (`[DIST-FAIL-001]`).
    ///
    /// When `side_effecting == true`, `retry_directive` is ALWAYS
    /// [`RemoteRetryDirective::ReconcileBeforeRetry`] — never masked as a clean
    /// local failure or blind retry.
    #[must_use]
    pub const fn classify_inflight_timeout<T>(&self, elapsed_ticks: u64) -> RemoteCallOutcome<T> {
        let retry_directive = if self.side_effecting {
            RemoteRetryDirective::ReconcileBeforeRetry
        } else if self.has_retry_budget_remaining() {
            RemoteRetryDirective::SafeToRetryWithSameKey
        } else {
            RemoteRetryDirective::DoNotRetryBudgetExhausted
        };
        RemoteCallOutcome::DeadlineExceededUncertain {
            deadline_tick: self.deadline_tick,
            elapsed_ticks,
            side_effecting: self.side_effecting,
            retry_directive,
            idempotency_key: self.idempotency_key,
        }
    }

    /// Classifies a multi-shard partial completion into
    /// [`RemoteCallOutcome::Partial`] with a domain-separated `SHA-256`
    /// `uncertainty_digest`.
    #[must_use]
    pub fn classify_partial<T>(
        &self,
        completed_shards: u32,
        failed_shards: u32,
        partial_value: Option<T>,
        shards_idempotent: bool,
        shard_evidence: &[u8],
    ) -> RemoteCallOutcome<T> {
        let mut hasher = Sha256::new();
        hasher.update(REMOTE_CALL_PARTIAL_UNCERTAINTY_DOMAIN);
        hasher.update(self.call_id);
        hasher.update(self.idempotency_key.as_bytes());
        hasher.update(completed_shards.to_be_bytes());
        hasher.update(failed_shards.to_be_bytes());
        hasher.update(shard_evidence);
        let uncertainty_digest: [u8; 32] = hasher.finalize().into();

        let retry_directive = if self.side_effecting && !shards_idempotent {
            RemoteRetryDirective::ReconcileBeforeRetry
        } else if self.has_retry_budget_remaining() {
            RemoteRetryDirective::RetryFailedShardsOnly
        } else {
            RemoteRetryDirective::DoNotRetryBudgetExhausted
        };

        RemoteCallOutcome::Partial {
            completed_shards,
            failed_shards,
            partial_value,
            uncertainty_digest,
            retry_directive,
        }
    }

    /// Advances to the next retry attempt (`attempt_number + 1`) with a new
    /// `next_deadline_tick` while preserving `call_id` and `idempotency_key`,
    /// enforcing `[DIST-FAIL-001]` retry rules against `prior_outcome`.
    ///
    /// # Errors
    ///
    /// Refuses retry with [`RemoteRetryRefusal`] when `prior_outcome` already
    /// succeeded, requires reconciliation first (`ReconcileBeforeRetry`), or
    /// has exhausted `max_retries`.
    pub const fn next_retry_for_outcome<T>(
        &self,
        prior_outcome: &RemoteCallOutcome<T>,
        next_deadline_tick: u64,
    ) -> Result<Self, RemoteRetryRefusal> {
        match prior_outcome {
            RemoteCallOutcome::Succeeded { .. } => Err(RemoteRetryRefusal::AlreadySucceeded),
            RemoteCallOutcome::Unavailable {
                retry_directive, ..
            }
            | RemoteCallOutcome::DeadlineExceededUncertain {
                retry_directive, ..
            }
            | RemoteCallOutcome::Partial {
                retry_directive, ..
            } => match retry_directive {
                RemoteRetryDirective::ReconcileBeforeRetry => Err(
                    RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation {
                        idempotency_key: self.idempotency_key,
                    },
                ),
                RemoteRetryDirective::DoNotRetryBudgetExhausted => {
                    Err(RemoteRetryRefusal::RetryBudgetExhausted {
                        attempt_number: self.attempt_number,
                        max_retries: self.max_retries,
                    })
                }
                RemoteRetryDirective::SafeToRetryWithSameKey
                | RemoteRetryDirective::RetryFailedShardsOnly => {
                    if !self.has_retry_budget_remaining() {
                        return Err(RemoteRetryRefusal::RetryBudgetExhausted {
                            attempt_number: self.attempt_number,
                            max_retries: self.max_retries,
                        });
                    }
                    Ok(Self {
                        call_id: self.call_id,
                        idempotency_key: self.idempotency_key,
                        deadline_tick: next_deadline_tick,
                        max_retries: self.max_retries,
                        attempt_number: self.attempt_number + 1,
                        side_effecting: self.side_effecting,
                    })
                }
            },
        }
    }
}

/// Explicit outcome of a cross-Cell remote call (`[DIST-FAIL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteCallOutcome<T> {
    /// Call completed and returned a verified response at `observed_epoch`.
    Succeeded {
        /// Verified payload returned by the peer Cell.
        value: T,
        /// Peer `CellEpoch` observed on the response.
        observed_epoch: CellEpoch,
        /// `true` when the peer served an idempotent replay for
        /// `idempotency_key`.
        idempotency_replayed: bool,
    },
    /// Peer Cell was unreachable prior to dispatch; zero remote side effects
    /// occurred on this attempt.
    Unavailable {
        /// Deadline tick configured on the call contract.
        deadline_tick: u64,
        /// Retry directive (`SafeToRetryWithSameKey` within budget, else
        /// `DoNotRetryBudgetExhausted`).
        retry_directive: RemoteRetryDirective,
        /// Idempotency key that any retry MUST reuse.
        idempotency_key: IdempotencyKey,
    },
    /// Call exceeded its deadline after entering flight (or across a partition
    /// cut where remote execution cannot be ruled out).
    DeadlineExceededUncertain {
        /// Deadline tick configured on the call contract.
        deadline_tick: u64,
        /// Elapsed ticks observed before timeout.
        elapsed_ticks: u64,
        /// Whether the call was marked side-effecting.
        side_effecting: bool,
        /// Mandatory retry directive (`ReconcileBeforeRetry` when
        /// `side_effecting == true`).
        retry_directive: RemoteRetryDirective,
        /// Idempotency key bound to the uncertain call for reconciliation.
        idempotency_key: IdempotencyKey,
    },
    /// Multi-shard remote call completed only a subset of shards before failure
    /// or partition.
    Partial {
        /// Count of shards confirmed completed.
        completed_shards: u32,
        /// Count of shards failed or uncertain.
        failed_shards: u32,
        /// Optional aggregated partial value from completed shards.
        partial_value: Option<T>,
        /// Domain-separated `SHA-256` digest of shard completion/uncertainty
        /// evidence.
        uncertainty_digest: [u8; 32],
        /// Retry directive for remaining/uncertain shards.
        retry_directive: RemoteRetryDirective,
    },
}

impl<T> RemoteCallOutcome<T> {
    /// Returns `true` when the outcome is [`Self::Succeeded`].
    #[must_use]
    pub const fn is_succeeded(&self) -> bool {
        matches!(self, Self::Succeeded { .. })
    }

    /// Returns `true` when remote side effects may have partially or
    /// indeterminately executed (`DeadlineExceededUncertain` or `Partial`).
    #[must_use]
    pub const fn is_uncertain(&self) -> bool {
        matches!(
            self,
            Self::DeadlineExceededUncertain { .. } | Self::Partial { .. }
        )
    }

    /// Returns the attached [`RemoteRetryDirective`] for non-success outcomes.
    #[must_use]
    pub const fn retry_directive(&self) -> Option<RemoteRetryDirective> {
        match *self {
            Self::Succeeded { .. } => None,
            Self::Unavailable {
                retry_directive, ..
            }
            | Self::DeadlineExceededUncertain {
                retry_directive, ..
            }
            | Self::Partial {
                retry_directive, ..
            } => Some(retry_directive),
        }
    }

    /// Returns `true` when the caller MUST reconcile remote state before
    /// issuing any retry (`[DIST-FAIL-001]`).
    #[must_use]
    pub const fn requires_reconciliation(&self) -> bool {
        matches!(
            self.retry_directive(),
            Some(RemoteRetryDirective::ReconcileBeforeRetry)
        )
    }
}
