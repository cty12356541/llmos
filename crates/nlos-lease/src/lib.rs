//! Single-Cell `QuotaLease` / `CapacityLease` / `ExclusiveDeviceLease` grant
//! (C-LEASE slices).
//!
//! Control-plane prepaid transfer for one Cell:
//! - `QuotaLease` (`LEASE-GRANT-001`): `AVAILABLE` → node `LEASE`; face value
//!   deducted from the in-memory available pool before issue.
//! - `CapacityLease` (`LEASE-CAPACITY-001` prefix): source pool →
//!   `GLOBAL_RESERVED`; `amount` deducted before issue.
//! - `ExclusiveDeviceLease` (`LEASE-DEVICE-001` prefix): FREE `DeviceLeaseHead` →
//!   `DEVICE_RESERVED`; device claimed before issue.
//!
//! Stale epoch is a typed fail-closed reject for all three families.
//!
//! `QuotaLease` also tracks a node-local monotonic usage high-water
//! (`LEASE-SPEND-001`, `LEASE-REPORT-001`), `ACTIVE → CLOSING → CLOSED`
//! (`LEASE-CLOSE-001`), and idempotent `ISSUED` cancel (`LEASE-PREACTIVE-001`).
//! The high-water moves face value straight from local remaining to spent;
//! this slice has no held-reservation bucket.
//!
//! Fence admit is delegated to [`CellAuthority::admit`]; this crate does not
//! re-implement the fail-closed fence checks.
//!
//! `CapacityLease` additionally has idempotent
//! `GLOBAL_RESERVED → TARGET_PREPARED` and
//! `GLOBAL_RESERVED`/`TARGET_PREPARED → RETURNING → RETURNED`
//! (`LEASE-PREACTIVE-001`). `RETURNING` does not refund; `RETURNED` refunds
//! `amount` once. `ACTIVE` stays out: there is no host attach receipt.
//!
//! `QuotaLease` loss (`LEASE-LOSS-001`): advancing the Cell epoch moves
//! `ISSUED` / `ACTIVE` / `CLOSING` leases into `QUARANTINED` without returning
//! their face value to `AVAILABLE`. `CLOSED` and `CANCELLED` stay put.
//!
//! Out of scope: reconciliation receipts, custody (#17), cross-cell grant,
//! transport, Raft, gateway fence-barrier, TTL-as-reuse, durable second
//! ledger, Capacity `ACTIVE` and later reclaim states, and Device
//! reset/zeroization.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use nlos_cell::{
    CellAdmitError, CellAuthority, CellEpoch, CellError, CellFence, CellFencingToken, CellIdentity,
};
use nlos_types::{CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, QuotaLeaseId};

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

/// `QuotaLease` state for the single-Cell prefix.
///
/// Spec chain used here: `ISSUED → ACTIVE → CLOSING → CLOSED`,
/// `ISSUED → CANCELLED`, and non-terminal → `QUARANTINED` on epoch advance.
/// `FENCED` stays out of this slice.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum QuotaLeaseState {
    /// Prepaid and not yet admitted on the node (`ISSUED`).
    Issued,
    /// Node-local sub-ledger is open (`ACTIVE`).
    Active,
    /// New usage is frozen; remainder is not back in `AVAILABLE` (`CLOSING`).
    Closing,
    /// Close ACK applied; unspent face value returned (`CLOSED`).
    Closed,
    /// Pre-active cancel; full face value returned (`CANCELLED`).
    Cancelled,
    /// Unreconciled face value frozen after epoch advance (`QUARANTINED`).
    Quarantined,
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
    spent: u64,
    returned: u64,
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

    /// Local remainder not yet spent or returned.
    #[must_use]
    pub const fn remaining(self) -> u64 {
        self.remaining
    }

    /// Monotonic usage high-water already consumed from the face value.
    #[must_use]
    pub const fn spent(self) -> u64 {
        self.spent
    }

    /// Face value returned to the control-plane pool.
    #[must_use]
    pub const fn returned(self) -> u64 {
        self.returned
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
    leases: HashMap<QuotaLeaseId, QuotaLeaseGrant>,
}

impl QuotaLeaseGrantor {
    /// Opens a grantor bound to an existing Cell authority with an initial
    /// AVAILABLE pool.
    #[must_use]
    pub fn open(authority: CellAuthority, available: u64) -> Self {
        Self {
            authority,
            available,
            leases: HashMap::new(),
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
    /// [`CellAuthority::admit`] before any deduct. An identical
    /// `(lease_id, face_value)` returns the committed lease and does not
    /// deduct again. A different face value for the same id is
    /// [`QuotaLeaseGrantError::ConflictingFace`].
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
        if let Some(existing) = self.leases.get(&lease_id).copied() {
            if existing.face_value == face_value {
                return Ok(existing);
            }
            self.authority.admit(presented)?;
            return Err(QuotaLeaseGrantError::ConflictingFace {
                existing: existing.face_value,
                requested: face_value,
            });
        }
        self.authority.admit(presented)?;
        if face_value > self.available {
            return Err(QuotaLeaseGrantError::InsufficientAvailable {
                requested: face_value,
                available: self.available,
            });
        }
        self.available -= face_value;
        let cell = self.authority.identity();
        let grant = QuotaLeaseGrant {
            lease_id,
            cell,
            node_boot_generation: self.authority.node_boot_generation(),
            face_value,
            remaining: face_value,
            spent: 0,
            returned: 0,
            epoch: self.authority.epoch(),
            fencing_token: self.authority.fencing_token(),
            fence_scope: FenceScope::cell(cell),
            state: QuotaLeaseState::Issued,
        };
        self.leases.insert(lease_id, grant);
        Ok(grant)
    }

    /// Reads the committed lease snapshot.
    ///
    /// # Errors
    ///
    /// [`QuotaLeaseLedgerError::UnknownLease`] when `lease_id` was never issued.
    pub fn query(&self, lease_id: QuotaLeaseId) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.leases
            .get(&lease_id)
            .copied()
            .ok_or(QuotaLeaseLedgerError::UnknownLease)
    }

    /// Moves `ISSUED → ACTIVE`. A second call returns the committed lease.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn activate(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let lease = self.lease_mut(lease_id)?;
        match lease.state {
            QuotaLeaseState::Issued => {
                lease.state = QuotaLeaseState::Active;
                Ok(*lease)
            }
            QuotaLeaseState::Active => Ok(*lease),
            state => Err(QuotaLeaseLedgerError::ActivateRequiresIssued { state }),
        }
    }

    /// Advances the monotonic usage high-water inside the issued face value.
    ///
    /// `LEASE-SPEND-001` / `LEASE-REPORT-001`: the same high-water is
    /// idempotent. A lower mark is a typed regression. A mark above
    /// `face_value` is refused, so the lease cannot expand. `CLOSING` accepts
    /// only the already-committed mark.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn report_usage(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        high_water: u64,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let lease = self.lease_mut(lease_id)?;
        match lease.state {
            QuotaLeaseState::Active => apply_quota_high_water(lease, high_water),
            QuotaLeaseState::Closing => closing_high_water(lease, high_water),
            state => Err(QuotaLeaseLedgerError::NotActive { state }),
        }
    }

    /// `ACTIVE → CLOSING`. Does not return remainder to `AVAILABLE`.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn begin_close(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let lease = self.lease_mut(lease_id)?;
        match lease.state {
            QuotaLeaseState::Active => {
                lease.state = QuotaLeaseState::Closing;
                Ok(*lease)
            }
            QuotaLeaseState::Closing => Ok(*lease),
            state => Err(QuotaLeaseLedgerError::NotActive { state }),
        }
    }

    /// `CLOSING → CLOSED`. Returns only the unspent remainder to `AVAILABLE`.
    ///
    /// A second ACK returns the committed lease and does not refund again.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn ack_close(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let (snapshot, refund) = {
            let lease = self.lease_mut(lease_id)?;
            match lease.state {
                QuotaLeaseState::Closing => {
                    let refund = lease.remaining;
                    lease.returned += refund;
                    lease.remaining = 0;
                    lease.state = QuotaLeaseState::Closed;
                    (*lease, refund)
                }
                QuotaLeaseState::Closed => (*lease, 0),
                state => return Err(QuotaLeaseLedgerError::NotClosing { state }),
            }
        };
        self.available += refund;
        Ok(snapshot)
    }

    /// Idempotent `ISSUED → CANCELLED`. Refunds the full face value once.
    ///
    /// A lease that has left `ISSUED` is refused: this slice has no fence
    /// proof that would let an active lease cancel.
    ///
    /// # Errors
    ///
    /// Typed reject: [`QuotaLeaseLedgerError`].
    pub fn cancel(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
    ) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.authority.admit(presented)?;
        let (snapshot, refund) = {
            let lease = self.lease_mut(lease_id)?;
            match lease.state {
                QuotaLeaseState::Issued => {
                    let refund = lease.face_value;
                    lease.remaining = 0;
                    lease.returned = refund;
                    lease.spent = 0;
                    lease.state = QuotaLeaseState::Cancelled;
                    (*lease, refund)
                }
                QuotaLeaseState::Cancelled => (*lease, 0),
                state => {
                    return Err(QuotaLeaseLedgerError::CancelRequiresNeverActive { state });
                }
            }
        };
        self.available += refund;
        Ok(snapshot)
    }

    /// Advances the Cell epoch and quarantines unreconciled leases.
    ///
    /// `LEASE-LOSS-001`: `ISSUED`, `ACTIVE`, and `CLOSING` leases bound to the
    /// previous epoch become `QUARANTINED`. Their face value is not returned
    /// to `AVAILABLE`. `CLOSED` and `CANCELLED` are already reconciled.
    ///
    /// # Errors
    ///
    /// [`CellError::GenerationExhausted`] when the epoch cannot advance.
    pub fn advance_epoch_and_quarantine(&mut self) -> Result<CellFence, CellError> {
        let fence = self.authority.advance_epoch()?;
        for lease in self.leases.values_mut() {
            if lease.epoch >= fence.epoch() {
                continue;
            }
            match lease.state {
                QuotaLeaseState::Issued | QuotaLeaseState::Active | QuotaLeaseState::Closing => {
                    lease.state = QuotaLeaseState::Quarantined;
                }
                QuotaLeaseState::Closed
                | QuotaLeaseState::Cancelled
                | QuotaLeaseState::Quarantined => {}
            }
        }
        Ok(fence)
    }

    fn lease_mut(
        &mut self,
        lease_id: QuotaLeaseId,
    ) -> Result<&mut QuotaLeaseGrant, QuotaLeaseLedgerError> {
        self.leases
            .get_mut(&lease_id)
            .ok_or(QuotaLeaseLedgerError::UnknownLease)
    }
}

fn apply_quota_high_water(
    lease: &mut QuotaLeaseGrant,
    high_water: u64,
) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
    if high_water < lease.spent {
        return Err(QuotaLeaseLedgerError::UsageRegression {
            reported: high_water,
            current: lease.spent,
        });
    }
    if high_water > lease.face_value {
        return Err(QuotaLeaseLedgerError::UsageExceedsFace {
            reported: high_water,
            face_value: lease.face_value,
        });
    }
    lease.spent = high_water;
    lease.remaining = lease.face_value - lease.spent;
    Ok(*lease)
}

fn closing_high_water(
    lease: &QuotaLeaseGrant,
    high_water: u64,
) -> Result<QuotaLeaseGrant, QuotaLeaseLedgerError> {
    if high_water < lease.spent {
        return Err(QuotaLeaseLedgerError::UsageRegression {
            reported: high_water,
            current: lease.spent,
        });
    }
    if high_water == lease.spent {
        return Ok(*lease);
    }
    Err(QuotaLeaseLedgerError::NewReserveForbidden)
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
    /// The same `lease_id` was already issued with a different face value.
    ConflictingFace {
        /// Face value on the committed lease.
        existing: u64,
        /// Face value on this grant attempt.
        requested: u64,
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
            Self::ConflictingFace {
                existing,
                requested,
            } => write!(
                formatter,
                "conflicting QuotaLease face value: existing {existing} != requested {requested}"
            ),
        }
    }
}

impl Error for QuotaLeaseGrantError {}

/// Typed fail-closed rejects for `QuotaLease` activate, usage, close, and cancel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaLeaseLedgerError {
    /// Presented fence was rejected by [`CellAuthority::admit`].
    Fence(QuotaLeaseGrantError),
    /// No lease with this id has been issued.
    UnknownLease,
    /// Activate was asked of a lease that is neither `ISSUED` nor `ACTIVE`.
    ActivateRequiresIssued {
        /// State of the committed lease.
        state: QuotaLeaseState,
    },
    /// Usage was asked of a lease that is not `ACTIVE` (and not an idempotent
    /// `CLOSING` replay).
    NotActive {
        /// State of the committed lease.
        state: QuotaLeaseState,
    },
    /// `CLOSING` refused a high-water above the committed mark.
    NewReserveForbidden,
    /// Reported usage is below the committed high-water.
    UsageRegression {
        /// Mark on this report.
        reported: u64,
        /// Committed high-water.
        current: u64,
    },
    /// Reported usage is above the issued face value.
    UsageExceedsFace {
        /// Mark on this report.
        reported: u64,
        /// Issued face value.
        face_value: u64,
    },
    /// Close ACK was asked of a lease that is neither `CLOSING` nor `CLOSED`.
    NotClosing {
        /// State of the committed lease.
        state: QuotaLeaseState,
    },
    /// Cancel was asked of a lease that has left `ISSUED`.
    CancelRequiresNeverActive {
        /// State of the committed lease.
        state: QuotaLeaseState,
    },
}

impl From<CellAdmitError> for QuotaLeaseLedgerError {
    fn from(error: CellAdmitError) -> Self {
        Self::Fence(QuotaLeaseGrantError::from(error))
    }
}

impl fmt::Display for QuotaLeaseLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fence(error) => write!(formatter, "QuotaLease fence rejected: {error}"),
            Self::UnknownLease => write!(formatter, "unknown QuotaLease"),
            Self::ActivateRequiresIssued { state } => {
                write!(
                    formatter,
                    "QuotaLease activate requires ISSUED, found {state:?}"
                )
            }
            Self::NotActive { state } => {
                write!(
                    formatter,
                    "QuotaLease usage requires ACTIVE, found {state:?}"
                )
            }
            Self::NewReserveForbidden => {
                write!(
                    formatter,
                    "QuotaLease CLOSING forbids a higher usage high-water"
                )
            }
            Self::UsageRegression { reported, current } => write!(
                formatter,
                "QuotaLease usage high-water regressed: reported {reported} < current {current}"
            ),
            Self::UsageExceedsFace {
                reported,
                face_value,
            } => write!(
                formatter,
                "QuotaLease usage exceeds face value: reported {reported} > face {face_value}"
            ),
            Self::NotClosing { state } => {
                write!(
                    formatter,
                    "QuotaLease close ACK requires CLOSING, found {state:?}"
                )
            }
            Self::CancelRequiresNeverActive { state } => write!(
                formatter,
                "QuotaLease cancel requires a lease that was never ACTIVE, found {state:?}"
            ),
        }
    }
}

impl Error for QuotaLeaseLedgerError {}

/// `CapacityLease` state for the single-Cell prefix.
///
/// Spec chain used here: `GLOBAL_RESERVED → TARGET_PREPARED` and
/// `GLOBAL_RESERVED`/`TARGET_PREPARED → RETURNING → RETURNED`.
/// `ACTIVE` / reclaim stay out of this slice.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CapacityLeaseState {
    /// Issued after durable (here: in-memory) `amount` deduct from the source
    /// pool (`LEASE-CAPACITY-001` `GLOBAL_RESERVED`).
    GlobalReserved,
    /// Pre-active prepare. Source pool is unchanged (`TARGET_PREPARED`).
    TargetPrepared,
    /// Pre-active return started. Source pool is unchanged (`RETURNING`).
    Returning,
    /// Source authority accepted the return. `amount` is back in the pool
    /// (`RETURNED`).
    Returned,
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
    leases: HashMap<CapacityLeaseId, CapacityLeaseGrant>,
}

impl CapacityLeaseGrantor {
    /// Opens a grantor bound to an existing Cell authority with an initial
    /// source capacity pool.
    #[must_use]
    pub fn open(authority: CellAuthority, pool_remaining: u64) -> Self {
        Self {
            authority,
            pool_remaining,
            leases: HashMap::new(),
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
        if let Some(existing) = self.leases.get(&capacity_lease_id).copied() {
            if existing.amount == amount {
                return Ok(existing);
            }
            self.authority.admit(presented)?;
            return Err(CapacityLeaseGrantError::ConflictingAmount {
                existing: existing.amount,
                requested: amount,
            });
        }
        self.authority.admit(presented)?;
        if amount > self.pool_remaining {
            return Err(CapacityLeaseGrantError::InsufficientCapacity {
                requested: amount,
                available: self.pool_remaining,
            });
        }
        self.pool_remaining -= amount;
        let target_node = self.authority.identity();
        let grant = CapacityLeaseGrant {
            capacity_lease_id,
            target_node,
            target_node_boot_generation: self.authority.node_boot_generation(),
            amount,
            capacity_epoch: self.authority.epoch(),
            fencing_token: self.authority.fencing_token(),
            fence_scope: FenceScope::cell(target_node),
            state: CapacityLeaseState::GlobalReserved,
        };
        self.leases.insert(capacity_lease_id, grant);
        Ok(grant)
    }

    /// Reads the committed capacity lease snapshot.
    ///
    /// # Errors
    ///
    /// [`CapacityLeaseReturnError::UnknownLease`] when the id was never issued.
    pub fn query(
        &self,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.leases
            .get(&capacity_lease_id)
            .copied()
            .ok_or(CapacityLeaseReturnError::UnknownLease)
    }

    /// `GLOBAL_RESERVED → TARGET_PREPARED`. Does not refund the source pool.
    ///
    /// A second call on `TARGET_PREPARED` returns the committed lease.
    ///
    /// # Errors
    ///
    /// Typed reject: [`CapacityLeaseReturnError`].
    pub fn prepare(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.authority.admit(presented)?;
        let lease = self.lease_mut(capacity_lease_id)?;
        match lease.state {
            CapacityLeaseState::GlobalReserved => {
                lease.state = CapacityLeaseState::TargetPrepared;
                Ok(*lease)
            }
            CapacityLeaseState::TargetPrepared => Ok(*lease),
            state => Err(CapacityLeaseReturnError::NotGlobalReserved { state }),
        }
    }

    /// `GLOBAL_RESERVED` or `TARGET_PREPARED` → `RETURNING`. Does not refund
    /// the source pool.
    ///
    /// # Errors
    ///
    /// Typed reject: [`CapacityLeaseReturnError`].
    pub fn begin_return(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.authority.admit(presented)?;
        let lease = self.lease_mut(capacity_lease_id)?;
        match lease.state {
            CapacityLeaseState::GlobalReserved | CapacityLeaseState::TargetPrepared => {
                lease.state = CapacityLeaseState::Returning;
                Ok(*lease)
            }
            CapacityLeaseState::Returning => Ok(*lease),
            state @ CapacityLeaseState::Returned => {
                Err(CapacityLeaseReturnError::NotReserved { state })
            }
        }
    }

    /// `RETURNING → RETURNED`. Refunds `amount` to the source pool once.
    ///
    /// # Errors
    ///
    /// Typed reject: [`CapacityLeaseReturnError`].
    pub fn ack_return(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.authority.admit(presented)?;
        let (snapshot, refund) = {
            let lease = self.lease_mut(capacity_lease_id)?;
            match lease.state {
                CapacityLeaseState::Returning => {
                    lease.state = CapacityLeaseState::Returned;
                    (*lease, lease.amount)
                }
                CapacityLeaseState::Returned => (*lease, 0),
                state @ (CapacityLeaseState::GlobalReserved
                | CapacityLeaseState::TargetPrepared) => {
                    return Err(CapacityLeaseReturnError::NotReturning { state });
                }
            }
        };
        self.pool_remaining += refund;
        Ok(snapshot)
    }

    fn lease_mut(
        &mut self,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<&mut CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.leases
            .get_mut(&capacity_lease_id)
            .ok_or(CapacityLeaseReturnError::UnknownLease)
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
    /// The same capacity lease id was already issued with a different amount.
    ConflictingAmount {
        /// Amount on the committed lease.
        existing: u64,
        /// Amount on this grant attempt.
        requested: u64,
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
            Self::ConflictingAmount {
                existing,
                requested,
            } => write!(
                formatter,
                "conflicting CapacityLease amount: existing {existing} != requested {requested}"
            ),
        }
    }
}

impl Error for CapacityLeaseGrantError {}

/// Typed fail-closed rejects for `CapacityLease` pre-active return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityLeaseReturnError {
    /// Presented fence was rejected by [`CellAuthority::admit`].
    Fence(CapacityLeaseGrantError),
    /// No capacity lease with this id has been issued.
    UnknownLease,
    /// Return was asked of a lease that is neither `GLOBAL_RESERVED`,
    /// `TARGET_PREPARED`, nor `RETURNING`.
    NotReserved {
        /// State of the committed lease.
        state: CapacityLeaseState,
    },
    /// Prepare was asked of a lease that is neither `GLOBAL_RESERVED` nor
    /// `TARGET_PREPARED`.
    NotGlobalReserved {
        /// State of the committed lease.
        state: CapacityLeaseState,
    },
    /// Return ACK was asked of a lease that is neither `RETURNING` nor
    /// `RETURNED`.
    NotReturning {
        /// State of the committed lease.
        state: CapacityLeaseState,
    },
}

impl From<CellAdmitError> for CapacityLeaseReturnError {
    fn from(error: CellAdmitError) -> Self {
        Self::Fence(CapacityLeaseGrantError::from(error))
    }
}

impl fmt::Display for CapacityLeaseReturnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fence(error) => write!(formatter, "CapacityLease fence rejected: {error}"),
            Self::UnknownLease => write!(formatter, "unknown CapacityLease"),
            Self::NotReserved { state } => write!(
                formatter,
                "CapacityLease return requires GLOBAL_RESERVED or TARGET_PREPARED, found {state:?}"
            ),
            Self::NotGlobalReserved { state } => write!(
                formatter,
                "CapacityLease prepare requires GLOBAL_RESERVED, found {state:?}"
            ),
            Self::NotReturning { state } => write!(
                formatter,
                "CapacityLease return ACK requires RETURNING, found {state:?}"
            ),
        }
    }
}

impl Error for CapacityLeaseReturnError {}

/// `ExclusiveDeviceLease` state after a successful grant. Full state machine
/// (`HOLDER_PREPARED` / `ACTIVE` / reset / …) is deferred.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ExclusiveDeviceLeaseState {
    /// Issued after durable (here: in-memory) FREE→reserved claim on the
    /// `DeviceLeaseHead` (`LEASE-DEVICE-001` `DEVICE_RESERVED`).
    DeviceReserved,
}

/// A single-Cell `ExclusiveDeviceLease` grant snapshot (v0.5 §12 fields used by
/// this slice: id, device, exclusivity epoch, holder node/boot, fencing token,
/// state).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExclusiveDeviceLeaseGrant {
    device_lease_id: ExclusiveDeviceLeaseId,
    device_id: DeviceId,
    holder_node: CellIdentity,
    holder_node_boot_generation: Generation,
    exclusivity_epoch: CellEpoch,
    fencing_token: CellFencingToken,
    fence_scope: FenceScope,
    state: ExclusiveDeviceLeaseState,
}

impl ExclusiveDeviceLeaseGrant {
    /// Stable exclusive device lease identity.
    #[must_use]
    pub const fn device_lease_id(self) -> ExclusiveDeviceLeaseId {
        self.device_lease_id
    }

    /// Device identity bound into the grant.
    #[must_use]
    pub const fn device_id(self) -> DeviceId {
        self.device_id
    }

    /// Holder Cell (`holder_node` in §12 `ExclusiveDeviceLease`).
    #[must_use]
    pub const fn holder_node(self) -> CellIdentity {
        self.holder_node
    }

    /// Holder node boot generation bound into the grant.
    #[must_use]
    pub const fn holder_node_boot_generation(self) -> Generation {
        self.holder_node_boot_generation
    }

    /// Exclusivity epoch bound into the grant (Cell epoch for this single-Cell
    /// fence slice).
    #[must_use]
    pub const fn exclusivity_epoch(self) -> CellEpoch {
        self.exclusivity_epoch
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
    pub const fn state(self) -> ExclusiveDeviceLeaseState {
        self.state
    }
}

/// In-memory single-Cell `ExclusiveDeviceLease` grantor.
///
/// Holds one `DeviceLeaseHead` (FREE or reserved) and the Cell authority used
/// for fence admit. Not a durable ledger and not a second authority store.
#[derive(Debug)]
pub struct ExclusiveDeviceLeaseGrantor {
    authority: CellAuthority,
    device_id: DeviceId,
    free: bool,
}

impl ExclusiveDeviceLeaseGrantor {
    /// Opens a grantor bound to an existing Cell authority with a FREE device
    /// head for `device_id`.
    #[must_use]
    pub const fn open(authority: CellAuthority, device_id: DeviceId) -> Self {
        Self {
            authority,
            device_id,
            free: true,
        }
    }

    /// Device identity this grantor manages.
    #[must_use]
    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// Whether the in-memory `DeviceLeaseHead` is still FREE.
    #[must_use]
    pub const fn is_free(&self) -> bool {
        self.free
    }

    /// Grants an `ExclusiveDeviceLease` against a presented Cell fence.
    ///
    /// `LEASE-DEVICE-001` prefix: the FREE head is claimed before the lease
    /// enters [`ExclusiveDeviceLeaseState::DeviceReserved`]. Fail-closed fence
    /// checks run via [`CellAuthority::admit`] before any claim.
    ///
    /// # Errors
    ///
    /// Typed reject: [`ExclusiveDeviceLeaseGrantError`].
    pub fn grant(
        &mut self,
        presented: &CellFence,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseGrantError> {
        self.authority.admit(presented)?;
        if !self.free {
            return Err(ExclusiveDeviceLeaseGrantError::DeviceNotFree {
                device_id: self.device_id,
            });
        }
        self.free = false;
        let holder_node = self.authority.identity();
        Ok(ExclusiveDeviceLeaseGrant {
            device_lease_id,
            device_id: self.device_id,
            holder_node,
            holder_node_boot_generation: self.authority.node_boot_generation(),
            exclusivity_epoch: self.authority.epoch(),
            fencing_token: self.authority.fencing_token(),
            fence_scope: FenceScope::cell(holder_node),
            state: ExclusiveDeviceLeaseState::DeviceReserved,
        })
    }
}

/// Typed fail-closed rejects for an `ExclusiveDeviceLease` grant attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExclusiveDeviceLeaseGrantError {
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
    /// `DeviceLeaseHead` is not FREE; cannot install a second exclusive holder.
    DeviceNotFree {
        /// Device whose head is already reserved.
        device_id: DeviceId,
    },
}

impl From<CellAdmitError> for ExclusiveDeviceLeaseGrantError {
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

impl fmt::Display for ExclusiveDeviceLeaseGrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleEpoch { presented, current } => write!(
                formatter,
                "stale Cell epoch for ExclusiveDeviceLease grant: presented {} < current {}",
                presented.get(),
                current.get()
            ),
            Self::EpochMismatch { presented, current } => write!(
                formatter,
                "Cell epoch mismatch for ExclusiveDeviceLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::BootGenerationMismatch { presented, current } => write!(
                formatter,
                "Cell boot generation mismatch for ExclusiveDeviceLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::IdentityMismatch { presented, current } => write!(
                formatter,
                "Cell identity mismatch for ExclusiveDeviceLease grant: presented {presented:?} != current {current:?}"
            ),
            Self::FencingTokenMismatch { presented, current } => write!(
                formatter,
                "Cell fencing token mismatch for ExclusiveDeviceLease grant: presented {} != current {}",
                presented.get(),
                current.get()
            ),
            Self::DeviceNotFree { device_id } => write!(
                formatter,
                "ExclusiveDeviceLease DeviceLeaseHead not FREE for device {device_id:?}"
            ),
        }
    }
}

impl Error for ExclusiveDeviceLeaseGrantError {}
