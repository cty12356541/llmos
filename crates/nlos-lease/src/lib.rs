//! Single-Cell `QuotaLease` / `CapacityLease` grant (C-LEASE slices).
//!
//! Control-plane prepaid transfer for one Cell:
//! - `QuotaLease` (`LEASE-GRANT-001`): `AVAILABLE` → node `LEASE`; face value
//!   deducted from the in-memory available pool before issue.
//! - `CapacityLease` (`LEASE-CAPACITY-001` prefix): source pool →
//!   `GLOBAL_RESERVED`; `amount` deducted before issue.
//!
//! Stale epoch is a typed fail-closed reject for both families.
//!
//! Fence admit is delegated to [`CellAuthority::admit`]; this crate does not
//! re-implement the fail-closed fence checks.
//!
//! Out of scope for this slice: `ExclusiveDevice`, reconciliation, custody
//! (#17), cross-cell grant, transport, Raft, durable second ledger, and the
//! `CapacityLease` full state machine beyond `GLOBAL_RESERVED`.

use std::error::Error;
use std::fmt;

use nlos_cell::{
    CellAdmitError, CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity,
};
use nlos_types::{CapacityLeaseId, Generation, QuotaLeaseId};

/// Fence scope for a single-Cell grant: the Cell identity itself.
///
/// Cross-cell / multi-gateway scopes are out of scope for this slice.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FenceScope {
    cell: CellIdentity,
}

impl FenceScope {
    /// Single-Cell fence scope keyed by Cell identity.
    #[must_use]
    pub const fn cell(cell: CellIdentity) -> Self {
        Self { cell }
    }

    /// Cell identity that defines this scope.
    #[must_use]
    pub const fn as_cell(self) -> CellIdentity {
        self.cell
    }
}

/// `QuotaLease` state after a successful grant. Full state machine (ACTIVE /
/// CLOSING / FENCED / …) is deferred with Capacity/Device and reconciliation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum QuotaLeaseState {
    /// Issued after durable (here: in-memory) `face_value` deduct.
    Issued,
}

/// A single-Cell `QuotaLease` grant snapshot (v0.5 §12 `QuotaLease` fields used
/// by this slice).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaLeaseGrant {
    lease_id: QuotaLeaseId,
    cell: CellIdentity,
    node_boot_generation: Generation,
    face_value: u64,
    remaining: u64,
    epoch: CellEpoch,
    fencing_token: CellFencingToken,
    fence_scope: FenceScope,
    state: QuotaLeaseState,
}

impl QuotaLeaseGrant {
    /// Stable lease identity.
    #[must_use]
    pub const fn lease_id(self) -> QuotaLeaseId {
        self.lease_id
    }

    /// Target Cell (`node_id` in §12 `QuotaLease`).
    #[must_use]
    pub const fn cell(self) -> CellIdentity {
        self.cell
    }

    /// Node boot generation bound into the grant.
    #[must_use]
    pub const fn node_boot_generation(self) -> Generation {
        self.node_boot_generation
    }

    /// Prepaid face value deducted at grant time.
    #[must_use]
    pub const fn face_value(self) -> u64 {
        self.face_value
    }

    /// Remaining face value immediately after issue (= `face_value`).
    #[must_use]
    pub const fn remaining(self) -> u64 {
        self.remaining
    }

    /// Epoch bound into the grant.
    #[must_use]
    pub const fn epoch(self) -> CellEpoch {
        self.epoch
    }

    /// Fencing token bound into the grant.
    #[must_use]
    pub const fn fencing_token(self) -> CellFencingToken {
        self.fencing_token
    }

    /// Fence scope for this grant.
    #[must_use]
    pub const fn fence_scope(self) -> FenceScope {
        self.fence_scope
    }

    /// Lease state after grant.
    #[must_use]
    pub const fn state(self) -> QuotaLeaseState {
        self.state
    }
}

/// In-memory single-Cell `QuotaLease` grantor.
///
/// Holds the control-plane AVAILABLE pool and the Cell authority used for
/// fence admit. Not a durable ledger and not a second authority store.
#[derive(Debug)]
pub struct QuotaLeaseGrantor {
    authority: CellAuthority,
    available: u64,
}

impl QuotaLeaseGrantor {
    /// Opens a grantor bound to an existing Cell authority with an initial
    /// AVAILABLE pool.
    #[must_use]
    pub const fn open(authority: CellAuthority, available: u64) -> Self {
        Self {
            authority,
            available,
        }
    }

    /// Remaining control-plane AVAILABLE for this Cell.
    #[must_use]
    pub const fn available(&self) -> u64 {
        self.available
    }

    /// Grants a `QuotaLease` against a presented Cell fence.
    ///
    /// `LEASE-GRANT-001`: `face_value` is deducted from AVAILABLE before the
    /// lease is issued. Fail-closed fence checks run via
    /// [`CellAuthority::admit`] before any deduct.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseGrantError`].
    pub fn grant(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        face_value: u64,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseGrantError> {
        self.authority.admit(presented)?;
        if face_value > self.available {
            return Err(QuotaLeaseGrantError::InsufficientAvailable {
                requested: face_value,
                available: self.available,
            });
        }
        self.available -= face_value;
        let cell = self.authority.identity();
        Ok(QuotaLeaseGrant {
            lease_id,
            cell,
            node_boot_generation: self.authority.node_boot_generation(),
            face_value,
            remaining: face_value,
            epoch: self.authority.epoch(),
            fencing_token: self.authority.fencing_token(),
            fence_scope: FenceScope::cell(cell),
            state: QuotaLeaseState::Issued,
        })
    }
}

/// Typed fail-closed rejects for a `QuotaLease` grant attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaLeaseGrantError {
    /// Presented epoch is older than the grantor's current epoch.
    StaleEpoch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented epoch is not current and is not older (fail closed).
    EpochMismatch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented boot generation does not match the grantor.
    BootGenerationMismatch {
        /// Boot generation on the presentation.
        presented: Generation,
        /// Boot generation currently held by the grantor.
        current: Generation,
    },
    /// Presented identity is not this Cell.
    IdentityMismatch {
        /// Identity on the presentation.
        presented: CellIdentity,
        /// Identity of this grantor.
        current: CellIdentity,
    },
    /// Epoch matches but the fencing token does not.
    FencingTokenMismatch {
        /// Token on the presentation.
        presented: CellFencingToken,
        /// Token currently held by the grantor.
        current: CellFencingToken,
    },
    /// AVAILABLE is strictly less than the requested `face_value`.
    InsufficientAvailable {
        /// Requested `face_value`.
        requested: u64,
        /// Remaining AVAILABLE before the attempt.
        available: u64,
    },
}

impl From<CellAdmitError> for QuotaLeaseGrantError {
    fn from(error: CellAdmitError) -> Self {
        match error {
            CellAdmitError::StaleEpoch { presented, current } => {
                Self::StaleEpoch { presented, current }
            }
            CellAdmitError::EpochMismatch { presented, current } => {
                Self::EpochMismatch { presented, current }
            }
            CellAdmitError::BootGenerationMismatch { presented, current } => {
                Self::BootGenerationMismatch { presented, current }
            }
            CellAdmitError::IdentityMismatch { presented, current } => {
                Self::IdentityMismatch { presented, current }
            }
            CellAdmitError::FencingTokenMismatch { presented, current } => {
                Self::FencingTokenMismatch { presented, current }
            }
        }
    }
}

impl fmt::Display for QuotaLeaseGrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleEpoch { presented, current } => write!(
                formatter,
                "stale Cell epoch for QuotaLease grant: presented {} < current {}",
                presented.get(),
                current.get()
            ),
            Self::EpochMismatch { presented, current } => write!(
                formatter,
                "Cell epoch mismatch for QuotaLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::BootGenerationMismatch { presented, current } => write!(
                formatter,
                "Cell boot generation mismatch for QuotaLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::IdentityMismatch { presented, current } => write!(
                formatter,
                "Cell identity mismatch for QuotaLease grant: presented {presented:?} != current {current:?}"
            ),
            Self::FencingTokenMismatch { presented, current } => write!(
                formatter,
                "Cell fencing token mismatch for QuotaLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::InsufficientAvailable {
                requested,
                available,
            } => write!(
                formatter,
                "insufficient AVAILABLE for QuotaLease grant: requested {requested} > available {available}"
            ),
        }
    }
}

impl Error for QuotaLeaseGrantError {}

/// `CapacityLease` state after a successful grant. Full state machine
/// (`TARGET_PREPARED` / `ACTIVE` / reclaim / …) is deferred.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CapacityLeaseState {
    /// Issued after durable (here: in-memory) `amount` deduct from the source
    /// pool (`LEASE-CAPACITY-001` `GLOBAL_RESERVED`).
    GlobalReserved,
}

/// A single-Cell `CapacityLease` grant snapshot (v0.5 §12 fields used by this
/// slice: id, amount, target node/boot, capacity epoch, fencing token, state).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityLeaseGrant {
    capacity_lease_id: CapacityLeaseId,
    target_node: CellIdentity,
    target_node_boot_generation: Generation,
    amount: u64,
    capacity_epoch: CellEpoch,
    fencing_token: CellFencingToken,
    fence_scope: FenceScope,
    state: CapacityLeaseState,
}

impl CapacityLeaseGrant {
    /// Stable capacity lease identity.
    #[must_use]
    pub const fn capacity_lease_id(self) -> CapacityLeaseId {
        self.capacity_lease_id
    }

    /// Target Cell (`target_node` in §12 `CapacityLease`).
    #[must_use]
    pub const fn target_node(self) -> CellIdentity {
        self.target_node
    }

    /// Target node boot generation bound into the grant.
    #[must_use]
    pub const fn target_node_boot_generation(self) -> Generation {
        self.target_node_boot_generation
    }

    /// Capacity amount deducted at grant time.
    #[must_use]
    pub const fn amount(self) -> u64 {
        self.amount
    }

    /// Capacity epoch bound into the grant (Cell epoch for this single-Cell
    /// fence slice).
    #[must_use]
    pub const fn capacity_epoch(self) -> CellEpoch {
        self.capacity_epoch
    }

    /// Fencing token bound into the grant.
    #[must_use]
    pub const fn fencing_token(self) -> CellFencingToken {
        self.fencing_token
    }

    /// Fence scope for this grant.
    #[must_use]
    pub const fn fence_scope(self) -> FenceScope {
        self.fence_scope
    }

    /// Lease state after grant.
    #[must_use]
    pub const fn state(self) -> CapacityLeaseState {
        self.state
    }
}

/// In-memory single-Cell `CapacityLease` grantor.
///
/// Holds the source capacity pool remaining and the Cell authority used for
/// fence admit. Not a durable ledger and not a second authority store.
#[derive(Debug)]
pub struct CapacityLeaseGrantor {
    authority: CellAuthority,
    pool_remaining: u64,
}

impl CapacityLeaseGrantor {
    /// Opens a grantor bound to an existing Cell authority with an initial
    /// source capacity pool.
    #[must_use]
    pub const fn open(authority: CellAuthority, pool_remaining: u64) -> Self {
        Self {
            authority,
            pool_remaining,
        }
    }

    /// Remaining source capacity pool for this Cell.
    #[must_use]
    pub const fn pool_remaining(&self) -> u64 {
        self.pool_remaining
    }

    /// Grants a `CapacityLease` against a presented Cell fence.
    ///
    /// `LEASE-CAPACITY-001` prefix: `amount` is deducted from the source pool
    /// before the lease enters [`CapacityLeaseState::GlobalReserved`].
    /// Fail-closed fence checks run via [`CellAuthority::admit`] before any
    /// deduct.
    ///
    /// # Errors
    ///
    /// Typed reject: [`CapacityLeaseGrantError`].
    pub fn grant(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
        amount: u64,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseGrantError> {
        self.authority.admit(presented)?;
        if amount > self.pool_remaining {
            return Err(CapacityLeaseGrantError::InsufficientCapacity {
                requested: amount,
                available: self.pool_remaining,
            });
        }
        self.pool_remaining -= amount;
        let target_node = self.authority.identity();
        Ok(CapacityLeaseGrant {
            capacity_lease_id,
            target_node,
            target_node_boot_generation: self.authority.node_boot_generation(),
            amount,
            capacity_epoch: self.authority.epoch(),
            fencing_token: self.authority.fencing_token(),
            fence_scope: FenceScope::cell(target_node),
            state: CapacityLeaseState::GlobalReserved,
        })
    }
}

/// Typed fail-closed rejects for a `CapacityLease` grant attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityLeaseGrantError {
    /// Presented epoch is older than the grantor's current epoch.
    StaleEpoch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented epoch is not current and is not older (fail closed).
    EpochMismatch {
        /// Epoch on the presentation.
        presented: CellEpoch,
        /// Epoch currently held by the grantor.
        current: CellEpoch,
    },
    /// Presented boot generation does not match the grantor.
    BootGenerationMismatch {
        /// Boot generation on the presentation.
        presented: Generation,
        /// Boot generation currently held by the grantor.
        current: Generation,
    },
    /// Presented identity is not this Cell.
    IdentityMismatch {
        /// Identity on the presentation.
        presented: CellIdentity,
        /// Identity of this grantor.
        current: CellIdentity,
    },
    /// Epoch matches but the fencing token does not.
    FencingTokenMismatch {
        /// Token on the presentation.
        presented: CellFencingToken,
        /// Token currently held by the grantor.
        current: CellFencingToken,
    },
    /// Source pool remaining is strictly less than the requested `amount`.
    InsufficientCapacity {
        /// Requested `amount`.
        requested: u64,
        /// Remaining source pool before the attempt.
        available: u64,
    },
}

impl From<CellAdmitError> for CapacityLeaseGrantError {
    fn from(error: CellAdmitError) -> Self {
        match error {
            CellAdmitError::StaleEpoch { presented, current } => {
                Self::StaleEpoch { presented, current }
            }
            CellAdmitError::EpochMismatch { presented, current } => {
                Self::EpochMismatch { presented, current }
            }
            CellAdmitError::BootGenerationMismatch { presented, current } => {
                Self::BootGenerationMismatch { presented, current }
            }
            CellAdmitError::IdentityMismatch { presented, current } => {
                Self::IdentityMismatch { presented, current }
            }
            CellAdmitError::FencingTokenMismatch { presented, current } => {
                Self::FencingTokenMismatch { presented, current }
            }
        }
    }
}

impl fmt::Display for CapacityLeaseGrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleEpoch { presented, current } => write!(
                formatter,
                "stale Cell epoch for CapacityLease grant: presented {} < current {}",
                presented.get(),
                current.get()
            ),
            Self::EpochMismatch { presented, current } => write!(
                formatter,
                "Cell epoch mismatch for CapacityLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::BootGenerationMismatch { presented, current } => write!(
                formatter,
                "Cell boot generation mismatch for CapacityLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::IdentityMismatch { presented, current } => write!(
                formatter,
                "Cell identity mismatch for CapacityLease grant: presented {presented:?} != current {current:?}"
            ),
            Self::FencingTokenMismatch { presented, current } => write!(
                formatter,
                "Cell fencing token mismatch for CapacityLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::InsufficientCapacity {
                requested,
                available,
            } => write!(
                formatter,
                "insufficient CapacityLease source pool: requested {requested} > available {available}"
            ),
        }
    }
}

impl Error for CapacityLeaseGrantError {}
