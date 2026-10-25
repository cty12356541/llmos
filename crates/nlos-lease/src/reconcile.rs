//! Cross-epoch reconciliation receipts (`[LEASE-RECON-001]`), fence-barrier ACKs
//! (`[LEASE-FENCE-001]`), cross-node migration helpers (`[LEASE-MIGRATE-001]`,
//! `[LEASE-CAPACITY-002]`, `[LEASE-DEVICE-001]`), and local lease WAL-loss
//! quarantine guards (`[LEASE-WAL-001]`).

use std::error::Error;
use std::fmt;

use nlos_cell::{CellAdmitError, CellEpoch, CellError, CellFence, CellFencingToken, CellIdentity};
use nlos_types::{
    CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, QuotaLeaseId, ReceiptId,
};

use crate::{
    CapacityLeaseGrant, CapacityLeaseGrantError, CapacityLeaseGrantor, CapacityLeaseReturnError,
    CapacityLeaseState, DeviceResetReceipt, ExclusiveDeviceLeaseGrant,
    ExclusiveDeviceLeaseGrantError, ExclusiveDeviceLeaseGrantor, ExclusiveDeviceLeaseReturnError,
    ExclusiveDeviceLeaseState, FenceScope, LeaseInstant, LeaseStoreError, QuotaLeaseGrant,
    QuotaLeaseGrantError, QuotaLeaseGrantor, QuotaLeaseLedgerError, QuotaLeaseState,
    receipt_binds_lease,
};

const FENCE_BARRIER_DOMAIN_TAG: &[u8; 16] = b"NLOS-FENCE-ACK01";
const CLEANUP_ATTEST_DOMAIN_TAG: &[u8; 16] = b"NLOS-CLEAN-ATT01";

/// Durable fence-barrier acknowledgment from the enforcement gateway /
/// authority covering a lease's `fence_scope` and `fencing_token`
/// (`[LEASE-FENCE-001]`).
///
/// Without a valid `FenceBarrierAck`, unsettled or quarantined quota, capacity,
/// or device leases MUST remain `QUARANTINED` and cannot be returned to the
/// allocatable pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FenceBarrierAck {
    pub(crate) lease_id: [u8; 16],
    pub(crate) fence_scope: FenceScope,
    pub(crate) epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) gateway_id: [u8; 16],
    pub(crate) observed_high_water: u64,
    pub(crate) ack_digest: [u8; 32],
}

impl FenceBarrierAck {
    /// Builds a `FenceBarrierAck` with an explicit `ack_digest`.
    #[must_use]
    pub const fn new(
        lease_id: [u8; 16],
        fence_scope: FenceScope,
        epoch: CellEpoch,
        fencing_token: CellFencingToken,
        gateway_id: [u8; 16],
        observed_high_water: u64,
        ack_digest: [u8; 32],
    ) -> Self {
        Self {
            lease_id,
            fence_scope,
            epoch,
            fencing_token,
            gateway_id,
            observed_high_water,
            ack_digest,
        }
    }

    /// Computes the deterministic domain-separated 32-byte digest binding all
    /// fence-barrier axes.
    #[allow(clippy::cast_possible_truncation)]
    #[must_use]
    pub const fn compute_digest(
        lease_id: [u8; 16],
        fence_scope: FenceScope,
        epoch: CellEpoch,
        fencing_token: CellFencingToken,
        gateway_id: [u8; 16],
        observed_high_water: u64,
    ) -> [u8; 32] {
        let scope_bytes = *fence_scope.as_cell().as_bytes();
        let epoch_bytes = epoch.get().to_le_bytes();
        let token_bytes = fencing_token.get().to_le_bytes();
        let hw_bytes = observed_high_water.to_le_bytes();

        let mut out = [0u8; 32];
        let mut idx = 0usize;
        while idx < 16 {
            out[idx] = FENCE_BARRIER_DOMAIN_TAG[idx]
                ^ lease_id[idx]
                ^ scope_bytes[idx].rotate_left(3)
                ^ gateway_id[idx].rotate_right(2);
            out[idx + 16] = FENCE_BARRIER_DOMAIN_TAG[15 - idx]
                ^ lease_id[idx].rotate_left(5)
                ^ scope_bytes[15 - idx]
                ^ gateway_id[idx]
                ^ epoch_bytes[idx % 8]
                ^ token_bytes[(idx + 3) % 8]
                ^ hw_bytes[(idx + 5) % 8];
            idx += 1;
        }
        let mut round = 0usize;
        while round < 32 {
            let prev = out[(round + 31) % 32];
            out[round] = out[round]
                .wrapping_add(prev.rotate_left(1))
                .wrapping_add(epoch_bytes[round % 8])
                .wrapping_add(token_bytes[(round + 2) % 8])
                .wrapping_add(hw_bytes[(round + 4) % 8])
                ^ (round as u8).wrapping_mul(0x9e);
            round += 1;
        }
        out
    }

    /// Builds a verified `FenceBarrierAck` for a `QuotaLeaseGrant`.
    #[must_use]
    pub const fn for_quota(
        lease: &QuotaLeaseGrant,
        gateway_id: [u8; 16],
        observed_high_water: u64,
    ) -> Self {
        let lease_id = *lease.lease_id().as_bytes();
        let fence_scope = lease.fence_scope();
        let epoch = lease.epoch();
        let fencing_token = lease.fencing_token();
        let ack_digest = Self::compute_digest(
            lease_id,
            fence_scope,
            epoch,
            fencing_token,
            gateway_id,
            observed_high_water,
        );
        Self::new(
            lease_id,
            fence_scope,
            epoch,
            fencing_token,
            gateway_id,
            observed_high_water,
            ack_digest,
        )
    }

    /// Builds a verified `FenceBarrierAck` for a `CapacityLeaseGrant`.
    #[must_use]
    pub const fn for_capacity(lease: &CapacityLeaseGrant, gateway_id: [u8; 16]) -> Self {
        let lease_id = *lease.capacity_lease_id().as_bytes();
        let fence_scope = lease.fence_scope();
        let epoch = lease.capacity_epoch();
        let fencing_token = lease.fencing_token();
        let observed_high_water = lease.amount();
        let ack_digest = Self::compute_digest(
            lease_id,
            fence_scope,
            epoch,
            fencing_token,
            gateway_id,
            observed_high_water,
        );
        Self::new(
            lease_id,
            fence_scope,
            epoch,
            fencing_token,
            gateway_id,
            observed_high_water,
            ack_digest,
        )
    }

    /// Builds a verified `FenceBarrierAck` for an `ExclusiveDeviceLeaseGrant`.
    #[must_use]
    pub const fn for_device(lease: &ExclusiveDeviceLeaseGrant, gateway_id: [u8; 16]) -> Self {
        let lease_id = *lease.device_lease_id().as_bytes();
        let fence_scope = lease.fence_scope();
        let epoch = lease.exclusivity_epoch();
        let fencing_token = lease.fencing_token();
        let observed_high_water = lease.exclusivity_epoch().get();
        let ack_digest = Self::compute_digest(
            lease_id,
            fence_scope,
            epoch,
            fencing_token,
            gateway_id,
            observed_high_water,
        );
        Self::new(
            lease_id,
            fence_scope,
            epoch,
            fencing_token,
            gateway_id,
            observed_high_water,
            ack_digest,
        )
    }

    /// Returns a copy with an overridden `ack_digest` (useful for testing
    /// fail-closed digest rejection).
    #[must_use]
    pub const fn with_digest(mut self, ack_digest: [u8; 32]) -> Self {
        self.ack_digest = ack_digest;
        self
    }

    /// Verifies that `gateway_id` is non-zero and `ack_digest` matches the
    /// canonical digest of all bound fields.
    #[must_use]
    pub fn verify_digest(&self) -> bool {
        if self.gateway_id == [0u8; 16] {
            return false;
        }
        let expected = Self::compute_digest(
            self.lease_id,
            self.fence_scope,
            self.epoch,
            self.fencing_token,
            self.gateway_id,
            self.observed_high_water,
        );
        self.ack_digest == expected
    }

    /// Bound lease identity bytes.
    #[must_use]
    pub const fn lease_id(&self) -> [u8; 16] {
        self.lease_id
    }

    /// Bound fence scope.
    #[must_use]
    pub const fn fence_scope(&self) -> FenceScope {
        self.fence_scope
    }

    /// Bound lease epoch.
    #[must_use]
    pub const fn epoch(&self) -> CellEpoch {
        self.epoch
    }

    /// Bound fencing token.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// Enforcement gateway identity that issued the barrier ACK.
    #[must_use]
    pub const fn gateway_id(&self) -> [u8; 16] {
        self.gateway_id
    }

    /// Gateway-observed usage / capacity / epoch high-water mark.
    #[must_use]
    pub const fn observed_high_water(&self) -> u64 {
        self.observed_high_water
    }

    /// Digest committing all barrier axes.
    #[must_use]
    pub const fn ack_digest(&self) -> [u8; 32] {
        self.ack_digest
    }
}

/// Typed reconciliation receipt for `QuotaLease` (`[LEASE-RECON-001]`,
/// `[LEASE-FENCE-001]`).
///
/// Proves `face_value == settled_spent + returned_to_available + remaining_quarantined`,
/// monotonic `high_water_spent`, and idempotent replay by `(lease_id, reconcile_seq)`.
/// Not interchangeable with [`CapacityReconciliationReceipt`] or
/// [`DeviceReconciliationReceipt`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaReconciliationReceipt {
    pub(crate) receipt_id: ReceiptId,
    pub(crate) lease_id: QuotaLeaseId,
    pub(crate) reconcile_seq: u64,
    pub(crate) cell: CellIdentity,
    pub(crate) node_boot_generation: Generation,
    pub(crate) epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) face_value: u64,
    pub(crate) high_water_spent: u64,
    pub(crate) settled_spent: u64,
    pub(crate) returned_to_available: u64,
    pub(crate) remaining_quarantined: u64,
    pub(crate) fence_barrier_ack: FenceBarrierAck,
}

impl QuotaReconciliationReceipt {
    /// Builds a `QuotaReconciliationReceipt` with explicit accounting fields.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub const fn new(
        receipt_id: ReceiptId,
        lease_id: QuotaLeaseId,
        reconcile_seq: u64,
        cell: CellIdentity,
        node_boot_generation: Generation,
        epoch: CellEpoch,
        fencing_token: CellFencingToken,
        face_value: u64,
        high_water_spent: u64,
        settled_spent: u64,
        returned_to_available: u64,
        remaining_quarantined: u64,
        fence_barrier_ack: FenceBarrierAck,
    ) -> Self {
        Self {
            receipt_id,
            lease_id,
            reconcile_seq,
            cell,
            node_boot_generation,
            epoch,
            fencing_token,
            face_value,
            high_water_spent,
            settled_spent,
            returned_to_available,
            remaining_quarantined,
            fence_barrier_ack,
        }
    }

    /// Builds a `QuotaReconciliationReceipt` directly from a committed
    /// `QuotaLeaseGrant` snapshot.
    #[must_use]
    pub const fn for_lease(
        receipt_id: ReceiptId,
        reconcile_seq: u64,
        lease: &QuotaLeaseGrant,
        high_water_spent: u64,
        returned_to_available: u64,
        remaining_quarantined: u64,
        fence_barrier_ack: FenceBarrierAck,
    ) -> Self {
        Self::new(
            receipt_id,
            lease.lease_id(),
            reconcile_seq,
            lease.cell(),
            lease.node_boot_generation(),
            lease.epoch(),
            lease.fencing_token(),
            lease.face_value(),
            high_water_spent,
            high_water_spent,
            returned_to_available,
            remaining_quarantined,
            fence_barrier_ack,
        )
    }

    /// Verifies the conservation equation:
    /// `settled_spent == high_water_spent` and
    /// `face_value == settled_spent + returned_to_available + remaining_quarantined`.
    #[must_use]
    pub fn verify_conservation(&self) -> bool {
        if self.settled_spent != self.high_water_spent {
            return false;
        }
        self.settled_spent
            .checked_add(self.returned_to_available)
            .and_then(|sum| sum.checked_add(self.remaining_quarantined))
            == Some(self.face_value)
    }

    /// Receipt idempotency key.
    #[must_use]
    pub const fn receipt_id(&self) -> ReceiptId {
        self.receipt_id
    }

    /// Reconciled `QuotaLeaseId`.
    #[must_use]
    pub const fn lease_id(&self) -> QuotaLeaseId {
        self.lease_id
    }

    /// Monotonic reconciliation sequence number (`>= 1`) for `lease_id`.
    #[must_use]
    pub const fn reconcile_seq(&self) -> u64 {
        self.reconcile_seq
    }

    /// Cell identity of the lease.
    #[must_use]
    pub const fn cell(&self) -> CellIdentity {
        self.cell
    }

    /// Node boot generation of the lease.
    #[must_use]
    pub const fn node_boot_generation(&self) -> Generation {
        self.node_boot_generation
    }

    /// Epoch of the lease.
    #[must_use]
    pub const fn epoch(&self) -> CellEpoch {
        self.epoch
    }

    /// Fencing token of the lease.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// Issued face value of the lease.
    #[must_use]
    pub const fn face_value(&self) -> u64 {
        self.face_value
    }

    /// Monotonic high-water spent mark verified by the barrier.
    #[must_use]
    pub const fn high_water_spent(&self) -> u64 {
        self.high_water_spent
    }

    /// Settled spent amount (`== high_water_spent`).
    #[must_use]
    pub const fn settled_spent(&self) -> u64 {
        self.settled_spent
    }

    /// Cumulative unspent face value returned to `AVAILABLE`.
    #[must_use]
    pub const fn returned_to_available(&self) -> u64 {
        self.returned_to_available
    }

    /// Unverified remainder still kept in `QUARANTINED` (`0` when fully closed).
    #[must_use]
    pub const fn remaining_quarantined(&self) -> u64 {
        self.remaining_quarantined
    }

    /// Bound `FenceBarrierAck`.
    #[must_use]
    pub const fn fence_barrier_ack(&self) -> FenceBarrierAck {
        self.fence_barrier_ack
    }
}

/// Fail-closed errors for `QuotaLease` reconciliation (`[LEASE-RECON-001]`,
/// `[LEASE-FENCE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaReconcileError {
    /// Presented authority fence was rejected.
    Fence(QuotaLeaseGrantError),
    /// Unknown `QuotaLeaseId`.
    UnknownLease,
    /// Lease is neither `QUARANTINED` nor `FENCED`.
    NotQuarantinedOrFenced {
        /// Current state of the lease.
        state: QuotaLeaseState,
    },
    /// `reconcile_seq` is zero or not strictly advancing.
    InvalidReconcileSeq {
        /// Minimum expected next sequence number.
        expected_min: u64,
        /// Presented sequence number.
        requested: u64,
    },
    /// `FenceBarrierAck` is missing, tampered, or mismatched on lease/epoch/token/high-water.
    InvalidFenceBarrierAck {
        /// Static diagnostic reason.
        reason: &'static str,
    },
    /// Receipt `identity/epoch/token/face_value` does not match the committed lease.
    ReceiptBindMismatch,
    /// Reported `high_water_spent` is below the lease's committed `spent`.
    HighWaterRegression {
        /// Reported high-water mark.
        reported: u64,
        /// Committed spent mark.
        committed: u64,
    },
    /// Reported `high_water_spent` exceeds `face_value`.
    HighWaterExceedsFace {
        /// Reported high-water mark.
        reported: u64,
        /// Lease face value.
        face_value: u64,
    },
    /// Reported `returned_to_available` regressed below already-returned amount.
    ReturnedRegression {
        /// Reported cumulative returned amount.
        reported: u64,
        /// Already returned amount.
        committed: u64,
    },
    /// `face_value != settled_spent + returned_to_available + remaining_quarantined`.
    ConservationViolated {
        /// Lease face value.
        face_value: u64,
        /// Settled spent on receipt.
        settled_spent: u64,
        /// Returned to available on receipt.
        returned_to_available: u64,
        /// Remaining quarantined on receipt.
        remaining_quarantined: u64,
    },
    /// Conflicting receipt replayed for the same `(lease_id, reconcile_seq)` or `receipt_id`.
    ConflictingReconciliationReceipt {
        /// Committed receipt id.
        existing: ReceiptId,
        /// Requested receipt id.
        requested: ReceiptId,
    },
    /// Durable store failure.
    Storage(&'static str),
}

impl From<CellAdmitError> for QuotaReconcileError {
    fn from(error: CellAdmitError) -> Self {
        Self::Fence(QuotaLeaseGrantError::from(error))
    }
}

impl From<LeaseStoreError> for QuotaReconcileError {
    fn from(error: LeaseStoreError) -> Self {
        Self::Storage(error.reason())
    }
}

impl fmt::Display for QuotaReconcileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fence(err) => write!(formatter, "QuotaLease reconcile fence rejected: {err}"),
            Self::UnknownLease => formatter.write_str("unknown QuotaLease for reconciliation"),
            Self::NotQuarantinedOrFenced { state } => write!(
                formatter,
                "QuotaLease reconciliation requires QUARANTINED or FENCED, found {state:?}"
            ),
            Self::InvalidReconcileSeq {
                expected_min,
                requested,
            } => write!(
                formatter,
                "invalid QuotaLease reconcile_seq: requested {requested} < expected_min {expected_min}"
            ),
            Self::InvalidFenceBarrierAck { reason } => {
                write!(formatter, "invalid QuotaLease FenceBarrierAck: {reason}")
            }
            Self::ReceiptBindMismatch => formatter
                .write_str("QuotaReconciliationReceipt does not bind committed QuotaLease axes"),
            Self::HighWaterRegression {
                reported,
                committed,
            } => write!(
                formatter,
                "QuotaLease reconcile high-water regressed: reported {reported} < committed {committed}"
            ),
            Self::HighWaterExceedsFace {
                reported,
                face_value,
            } => write!(
                formatter,
                "QuotaLease reconcile high-water {reported} exceeds face_value {face_value}"
            ),
            Self::ReturnedRegression {
                reported,
                committed,
            } => write!(
                formatter,
                "QuotaLease reconcile returned_to_available regressed: {reported} < {committed}"
            ),
            Self::ConservationViolated {
                face_value,
                settled_spent,
                returned_to_available,
                remaining_quarantined,
            } => write!(
                formatter,
                "QuotaLease conservation violated: face_value {face_value} != {settled_spent} + {returned_to_available} + {remaining_quarantined}"
            ),
            Self::ConflictingReconciliationReceipt {
                existing,
                requested,
            } => write!(
                formatter,
                "conflicting QuotaReconciliationReceipt: existing {existing:?}, requested {requested:?}"
            ),
            Self::Storage(reason) => {
                write!(formatter, "QuotaLease reconcile storage error: {reason}")
            }
        }
    }
}

impl Error for QuotaReconcileError {}

/// Holder cleanup attestation required before returning a fenced or quarantined
/// `CapacityLease` back to the allocatable pool (`[LEASE-CAPACITY-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CleanupAttestation {
    pub(crate) capacity_lease_id: CapacityLeaseId,
    pub(crate) target_node: CellIdentity,
    pub(crate) target_node_boot_generation: Generation,
    pub(crate) capacity_epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) cleaned_amount: u64,
    pub(crate) holder_processes_terminated: bool,
    pub(crate) attestation_digest: [u8; 32],
}

impl CleanupAttestation {
    /// Builds a `CleanupAttestation` with an explicit `attestation_digest`.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub const fn new(
        capacity_lease_id: CapacityLeaseId,
        target_node: CellIdentity,
        target_node_boot_generation: Generation,
        capacity_epoch: CellEpoch,
        fencing_token: CellFencingToken,
        cleaned_amount: u64,
        holder_processes_terminated: bool,
        attestation_digest: [u8; 32],
    ) -> Self {
        Self {
            capacity_lease_id,
            target_node,
            target_node_boot_generation,
            capacity_epoch,
            fencing_token,
            cleaned_amount,
            holder_processes_terminated,
            attestation_digest,
        }
    }

    /// Computes the canonical 32-byte digest for a `CleanupAttestation`.
    #[must_use]
    pub const fn compute_digest(
        capacity_lease_id: CapacityLeaseId,
        target_node: CellIdentity,
        target_node_boot_generation: Generation,
        capacity_epoch: CellEpoch,
        fencing_token: CellFencingToken,
        cleaned_amount: u64,
        holder_processes_terminated: bool,
    ) -> [u8; 32] {
        let id_bytes = *capacity_lease_id.as_bytes();
        let node_bytes = *target_node.as_bytes();
        let boot_bytes = target_node_boot_generation.get().to_le_bytes();
        let epoch_bytes = capacity_epoch.get().to_le_bytes();
        let token_bytes = fencing_token.get().to_le_bytes();
        let amount_bytes = cleaned_amount.to_le_bytes();
        let term_byte = if holder_processes_terminated {
            0xa5u8
        } else {
            0x5au8
        };

        let mut out = [0u8; 32];
        let mut idx = 0usize;
        while idx < 16 {
            out[idx] = CLEANUP_ATTEST_DOMAIN_TAG[idx]
                ^ id_bytes[idx]
                ^ node_bytes[idx].rotate_left(2)
                ^ boot_bytes[idx % 8]
                ^ term_byte;
            out[idx + 16] = CLEANUP_ATTEST_DOMAIN_TAG[15 - idx]
                ^ id_bytes[15 - idx]
                ^ node_bytes[idx].rotate_right(3)
                ^ epoch_bytes[idx % 8]
                ^ token_bytes[(idx + 2) % 8]
                ^ amount_bytes[(idx + 4) % 8]
                ^ term_byte.rotate_left(1);
            idx += 1;
        }
        out
    }

    /// Builds a verified `CleanupAttestation` for a committed `CapacityLeaseGrant`.
    #[must_use]
    pub const fn for_lease(
        lease: &CapacityLeaseGrant,
        cleaned_amount: u64,
        holder_processes_terminated: bool,
    ) -> Self {
        let digest = Self::compute_digest(
            lease.capacity_lease_id(),
            lease.target_node(),
            lease.target_node_boot_generation(),
            lease.capacity_epoch(),
            lease.fencing_token(),
            cleaned_amount,
            holder_processes_terminated,
        );
        Self::new(
            lease.capacity_lease_id(),
            lease.target_node(),
            lease.target_node_boot_generation(),
            lease.capacity_epoch(),
            lease.fencing_token(),
            cleaned_amount,
            holder_processes_terminated,
            digest,
        )
    }

    /// Overrides `attestation_digest` (useful for testing tamper rejection).
    #[must_use]
    pub const fn with_digest(mut self, attestation_digest: [u8; 32]) -> Self {
        self.attestation_digest = attestation_digest;
        self
    }

    /// Verifies that `attestation_digest` matches all bound fields.
    #[must_use]
    pub fn verify_digest(&self) -> bool {
        let expected = Self::compute_digest(
            self.capacity_lease_id,
            self.target_node,
            self.target_node_boot_generation,
            self.capacity_epoch,
            self.fencing_token,
            self.cleaned_amount,
            self.holder_processes_terminated,
        );
        self.attestation_digest == expected
    }

    /// Capacity lease id.
    #[must_use]
    pub const fn capacity_lease_id(&self) -> CapacityLeaseId {
        self.capacity_lease_id
    }

    /// Target node identity.
    #[must_use]
    pub const fn target_node(&self) -> CellIdentity {
        self.target_node
    }

    /// Target node boot generation.
    #[must_use]
    pub const fn target_node_boot_generation(&self) -> Generation {
        self.target_node_boot_generation
    }

    /// Capacity epoch.
    #[must_use]
    pub const fn capacity_epoch(&self) -> CellEpoch {
        self.capacity_epoch
    }

    /// Fencing token.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// Cleaned capacity amount.
    #[must_use]
    pub const fn cleaned_amount(&self) -> u64 {
        self.cleaned_amount
    }

    /// Whether all holder processes/allocations under the lease were terminated.
    #[must_use]
    pub const fn holder_processes_terminated(&self) -> bool {
        self.holder_processes_terminated
    }

    /// Canonical digest committing the attestation fields.
    #[must_use]
    pub const fn attestation_digest(&self) -> [u8; 32] {
        self.attestation_digest
    }
}

/// Typed reconciliation receipt for `CapacityLease` (`[LEASE-RECON-001]`,
/// `[LEASE-CAPACITY-002]`, `[LEASE-FENCE-001]`).
///
/// Proves `amount`, holder node/boot generation fence, `FenceBarrierAck`, and
/// `CleanupAttestation` before transitioning a `FENCED` / `QUARANTINED` /
/// `RETURNING` capacity lease to `RETURNED` and restoring `allocatable` pool
/// capacity. Not interchangeable with [`QuotaReconciliationReceipt`] or
/// [`DeviceReconciliationReceipt`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityReconciliationReceipt {
    pub(crate) receipt_id: ReceiptId,
    pub(crate) capacity_lease_id: CapacityLeaseId,
    pub(crate) target_node: CellIdentity,
    pub(crate) target_node_boot_generation: Generation,
    pub(crate) capacity_epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) amount: u64,
    pub(crate) cleanup_attestation: CleanupAttestation,
    pub(crate) fence_barrier_ack: FenceBarrierAck,
}

impl CapacityReconciliationReceipt {
    /// Builds a `CapacityReconciliationReceipt` with explicit axes.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub const fn new(
        receipt_id: ReceiptId,
        capacity_lease_id: CapacityLeaseId,
        target_node: CellIdentity,
        target_node_boot_generation: Generation,
        capacity_epoch: CellEpoch,
        fencing_token: CellFencingToken,
        amount: u64,
        cleanup_attestation: CleanupAttestation,
        fence_barrier_ack: FenceBarrierAck,
    ) -> Self {
        Self {
            receipt_id,
            capacity_lease_id,
            target_node,
            target_node_boot_generation,
            capacity_epoch,
            fencing_token,
            amount,
            cleanup_attestation,
            fence_barrier_ack,
        }
    }

    /// Builds a `CapacityReconciliationReceipt` for a committed `CapacityLeaseGrant`.
    #[must_use]
    pub const fn for_lease(
        receipt_id: ReceiptId,
        lease: &CapacityLeaseGrant,
        cleanup_attestation: CleanupAttestation,
        fence_barrier_ack: FenceBarrierAck,
    ) -> Self {
        Self::new(
            receipt_id,
            lease.capacity_lease_id(),
            lease.target_node(),
            lease.target_node_boot_generation(),
            lease.capacity_epoch(),
            lease.fencing_token(),
            lease.amount(),
            cleanup_attestation,
            fence_barrier_ack,
        )
    }

    /// Idempotency key of this reconciliation receipt.
    #[must_use]
    pub const fn receipt_id(&self) -> ReceiptId {
        self.receipt_id
    }

    /// Reconciled `CapacityLeaseId`.
    #[must_use]
    pub const fn capacity_lease_id(&self) -> CapacityLeaseId {
        self.capacity_lease_id
    }

    /// Target node identity.
    #[must_use]
    pub const fn target_node(&self) -> CellIdentity {
        self.target_node
    }

    /// Target node boot generation.
    #[must_use]
    pub const fn target_node_boot_generation(&self) -> Generation {
        self.target_node_boot_generation
    }

    /// Capacity epoch.
    #[must_use]
    pub const fn capacity_epoch(&self) -> CellEpoch {
        self.capacity_epoch
    }

    /// Fencing token.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// Reconciled capacity amount.
    #[must_use]
    pub const fn amount(&self) -> u64 {
        self.amount
    }

    /// Holder cleanup attestation.
    #[must_use]
    pub const fn cleanup_attestation(&self) -> CleanupAttestation {
        self.cleanup_attestation
    }

    /// Enforcement gateway fence-barrier ACK.
    #[must_use]
    pub const fn fence_barrier_ack(&self) -> FenceBarrierAck {
        self.fence_barrier_ack
    }
}

/// Fail-closed errors for `CapacityLease` reconciliation (`[LEASE-RECON-001]`,
/// `[LEASE-CAPACITY-002]`, `[LEASE-FENCE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityReconcileError {
    /// Presented authority fence was rejected.
    Fence(CapacityLeaseGrantError),
    /// Unknown `CapacityLeaseId`.
    UnknownLease,
    /// Lease is not in `FENCED`, `QUARANTINED`, or `RETURNING`.
    NotReconcilableState {
        /// Current state of the lease.
        state: CapacityLeaseState,
    },
    /// `FenceBarrierAck` is invalid or does not bind the lease.
    InvalidFenceBarrierAck {
        /// Static diagnostic reason.
        reason: &'static str,
    },
    /// `CleanupAttestation` is invalid, unverified, or holder cleanup is incomplete.
    InvalidCleanupAttestation {
        /// Static diagnostic reason.
        reason: &'static str,
    },
    /// Receipt axes do not bind the committed `CapacityLeaseGrant`.
    ReceiptBindMismatch,
    /// Conflicting reconciliation receipt for the same lease or receipt id.
    ConflictingReconciliationReceipt {
        /// Committed receipt id.
        existing: ReceiptId,
        /// Requested receipt id.
        requested: ReceiptId,
    },
    /// Rebooted boot generation did not advance past the WAL-lost generation.
    BootGenerationNotAdvanced {
        /// Lost boot generation.
        lost: Generation,
        /// Reconnected boot generation.
        reconnected: Generation,
    },
    /// Durable store failure.
    Storage(&'static str),
}

impl From<CellAdmitError> for CapacityReconcileError {
    fn from(error: CellAdmitError) -> Self {
        Self::Fence(CapacityLeaseGrantError::from(error))
    }
}

impl From<LeaseStoreError> for CapacityReconcileError {
    fn from(error: LeaseStoreError) -> Self {
        Self::Storage(error.reason())
    }
}

impl fmt::Display for CapacityReconcileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fence(err) => write!(formatter, "CapacityLease reconcile fence rejected: {err}"),
            Self::UnknownLease => formatter.write_str("unknown CapacityLease for reconciliation"),
            Self::NotReconcilableState { state } => write!(
                formatter,
                "CapacityLease reconciliation requires FENCED, QUARANTINED, or RETURNING, found {state:?}"
            ),
            Self::InvalidFenceBarrierAck { reason } => {
                write!(formatter, "invalid CapacityLease FenceBarrierAck: {reason}")
            }
            Self::InvalidCleanupAttestation { reason } => write!(
                formatter,
                "invalid CapacityLease CleanupAttestation: {reason}"
            ),
            Self::ReceiptBindMismatch => formatter
                .write_str("CapacityReconciliationReceipt does not bind committed CapacityLease"),
            Self::ConflictingReconciliationReceipt {
                existing,
                requested,
            } => write!(
                formatter,
                "conflicting CapacityReconciliationReceipt: existing {existing:?}, requested {requested:?}"
            ),
            Self::BootGenerationNotAdvanced { lost, reconnected } => write!(
                formatter,
                "CapacityLease WAL-loss boot generation not advanced: reconnected {} <= lost {}",
                reconnected.get(),
                lost.get()
            ),
            Self::Storage(reason) => {
                write!(formatter, "CapacityLease reconcile storage error: {reason}")
            }
        }
    }
}

impl Error for CapacityReconcileError {}

/// Typed reconciliation receipt for `ExclusiveDeviceLease` (`[LEASE-RECON-001]`,
/// `[LEASE-DEVICE-001]`, `[LEASE-FENCE-001]`).
///
/// Proves `exclusivity_epoch`, old fencing token rejection, queue drain,
/// `FenceBarrierAck`, and `DeviceResetReceipt` zeroization attestation before
/// clearing a `QUARANTINED` / `FENCING` device head back to `FREE` with bumped
/// reset generation. Not interchangeable with [`QuotaReconciliationReceipt`] or
/// [`CapacityReconciliationReceipt`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceReconciliationReceipt {
    pub(crate) receipt_id: ReceiptId,
    pub(crate) device_lease_id: ExclusiveDeviceLeaseId,
    pub(crate) device_id: DeviceId,
    pub(crate) holder_node: CellIdentity,
    pub(crate) holder_node_boot_generation: Generation,
    pub(crate) exclusivity_epoch: CellEpoch,
    pub(crate) fencing_token: CellFencingToken,
    pub(crate) reset_generation: Generation,
    pub(crate) endpoint_fenced_old_token: bool,
    pub(crate) queue_drained: bool,
    pub(crate) vram_and_secrets_zeroized: bool,
    pub(crate) reset_receipt: DeviceResetReceipt,
    pub(crate) fence_barrier_ack: FenceBarrierAck,
}

impl DeviceReconciliationReceipt {
    /// Builds a `DeviceReconciliationReceipt` with explicit axes.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub const fn new(
        receipt_id: ReceiptId,
        device_lease_id: ExclusiveDeviceLeaseId,
        device_id: DeviceId,
        holder_node: CellIdentity,
        holder_node_boot_generation: Generation,
        exclusivity_epoch: CellEpoch,
        fencing_token: CellFencingToken,
        reset_generation: Generation,
        endpoint_fenced_old_token: bool,
        queue_drained: bool,
        vram_and_secrets_zeroized: bool,
        reset_receipt: DeviceResetReceipt,
        fence_barrier_ack: FenceBarrierAck,
    ) -> Self {
        Self {
            receipt_id,
            device_lease_id,
            device_id,
            holder_node,
            holder_node_boot_generation,
            exclusivity_epoch,
            fencing_token,
            reset_generation,
            endpoint_fenced_old_token,
            queue_drained,
            vram_and_secrets_zeroized,
            reset_receipt,
            fence_barrier_ack,
        }
    }

    /// Builds a `DeviceReconciliationReceipt` for a committed
    /// `ExclusiveDeviceLeaseGrant`.
    #[must_use]
    pub const fn for_lease(
        receipt_id: ReceiptId,
        lease: &ExclusiveDeviceLeaseGrant,
        endpoint_fenced_old_token: bool,
        queue_drained: bool,
        vram_and_secrets_zeroized: bool,
        reset_receipt: DeviceResetReceipt,
        fence_barrier_ack: FenceBarrierAck,
    ) -> Self {
        Self::new(
            receipt_id,
            lease.device_lease_id(),
            lease.device_id(),
            lease.holder_node(),
            lease.holder_node_boot_generation(),
            lease.exclusivity_epoch(),
            lease.fencing_token(),
            lease.reset_generation(),
            endpoint_fenced_old_token,
            queue_drained,
            vram_and_secrets_zeroized,
            reset_receipt,
            fence_barrier_ack,
        )
    }

    /// Idempotency key of this reconciliation receipt.
    #[must_use]
    pub const fn receipt_id(&self) -> ReceiptId {
        self.receipt_id
    }

    /// Reconciled `ExclusiveDeviceLeaseId`.
    #[must_use]
    pub const fn device_lease_id(&self) -> ExclusiveDeviceLeaseId {
        self.device_lease_id
    }

    /// Device identity.
    #[must_use]
    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// Holder node identity.
    #[must_use]
    pub const fn holder_node(&self) -> CellIdentity {
        self.holder_node
    }

    /// Holder node boot generation.
    #[must_use]
    pub const fn holder_node_boot_generation(&self) -> Generation {
        self.holder_node_boot_generation
    }

    /// Exclusivity epoch.
    #[must_use]
    pub const fn exclusivity_epoch(&self) -> CellEpoch {
        self.exclusivity_epoch
    }

    /// Fencing token.
    #[must_use]
    pub const fn fencing_token(&self) -> CellFencingToken {
        self.fencing_token
    }

    /// Head reset generation fenced by this receipt.
    #[must_use]
    pub const fn reset_generation(&self) -> Generation {
        self.reset_generation
    }

    /// Whether all device endpoints rejected the old fencing token.
    #[must_use]
    pub const fn endpoint_fenced_old_token(&self) -> bool {
        self.endpoint_fenced_old_token
    }

    /// Whether device hardware queues were drained.
    #[must_use]
    pub const fn queue_drained(&self) -> bool {
        self.queue_drained
    }

    /// Whether VRAM and device secrets were zeroized.
    #[must_use]
    pub const fn vram_and_secrets_zeroized(&self) -> bool {
        self.vram_and_secrets_zeroized
    }

    /// Underlying `DeviceResetReceipt` proving reset + zeroization.
    #[must_use]
    pub const fn reset_receipt(&self) -> DeviceResetReceipt {
        self.reset_receipt
    }

    /// Enforcement gateway fence-barrier ACK.
    #[must_use]
    pub const fn fence_barrier_ack(&self) -> FenceBarrierAck {
        self.fence_barrier_ack
    }
}

/// Fail-closed errors for `ExclusiveDeviceLease` reconciliation
/// (`[LEASE-RECON-001]`, `[LEASE-DEVICE-001]`, `[LEASE-FENCE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceReconcileError {
    /// Presented authority fence was rejected.
    Fence(ExclusiveDeviceLeaseGrantError),
    /// Unknown `ExclusiveDeviceLeaseId`.
    UnknownLease,
    /// Lease is not in `QUARANTINED`, `FENCING`, or `RESETTING`.
    NotReconcilableState {
        /// Current state of the lease.
        state: ExclusiveDeviceLeaseState,
    },
    /// `FenceBarrierAck` is invalid or does not bind the device lease.
    InvalidFenceBarrierAck {
        /// Static diagnostic reason.
        reason: &'static str,
    },
    /// Device endpoints have not confirmed rejection of the old fencing token.
    OldFencingTokenNotRejected,
    /// Device queues have not been drained.
    QueueNotDrained,
    /// Device VRAM / secret zeroization was not attested.
    ZeroizationNotAttested,
    /// `reset_generation` does not match the `DeviceLeaseHead` generation fence.
    StaleResetGeneration {
        /// Generation on the receipt.
        receipt: Generation,
        /// Generation on the device head.
        head: Generation,
    },
    /// Embedded `DeviceResetReceipt` does not bind the committed lease.
    ResetReceiptMismatch,
    /// Receipt axes do not bind the committed `ExclusiveDeviceLeaseGrant`.
    ReceiptBindMismatch,
    /// Conflicting reconciliation receipt or reset receipt id.
    ConflictingReconciliationReceipt {
        /// Existing receipt id.
        existing: ReceiptId,
        /// Requested receipt id.
        requested: ReceiptId,
    },
    /// Head reset generation space exhausted.
    ResetGenerationExhausted,
    /// Rebooted boot generation did not advance past the WAL-lost generation.
    BootGenerationNotAdvanced {
        /// Lost boot generation.
        lost: Generation,
        /// Reconnected boot generation.
        reconnected: Generation,
    },
    /// Durable store failure.
    Storage(&'static str),
}

impl From<CellAdmitError> for DeviceReconcileError {
    fn from(error: CellAdmitError) -> Self {
        Self::Fence(ExclusiveDeviceLeaseGrantError::from(error))
    }
}

impl From<LeaseStoreError> for DeviceReconcileError {
    fn from(error: LeaseStoreError) -> Self {
        Self::Storage(error.reason())
    }
}

impl fmt::Display for DeviceReconcileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fence(err) => {
                write!(
                    formatter,
                    "ExclusiveDeviceLease reconcile fence rejected: {err}"
                )
            }
            Self::UnknownLease => {
                formatter.write_str("unknown ExclusiveDeviceLease for reconciliation")
            }
            Self::NotReconcilableState { state } => write!(
                formatter,
                "ExclusiveDeviceLease reconciliation requires QUARANTINED, FENCING, or RESETTING, found {state:?}"
            ),
            Self::InvalidFenceBarrierAck { reason } => write!(
                formatter,
                "invalid ExclusiveDeviceLease FenceBarrierAck: {reason}"
            ),
            Self::OldFencingTokenNotRejected => formatter
                .write_str("ExclusiveDeviceLease endpoints have not rejected old fencing token"),
            Self::QueueNotDrained => {
                formatter.write_str("ExclusiveDeviceLease queues are not drained")
            }
            Self::ZeroizationNotAttested => {
                formatter.write_str("ExclusiveDeviceLease VRAM/secret zeroization is not attested")
            }
            Self::StaleResetGeneration { receipt, head } => write!(
                formatter,
                "ExclusiveDeviceLease reconcile stale reset_generation: receipt {} != head {}",
                receipt.get(),
                head.get()
            ),
            Self::ResetReceiptMismatch => formatter
                .write_str("ExclusiveDeviceLease embedded DeviceResetReceipt does not bind lease"),
            Self::ReceiptBindMismatch => formatter.write_str(
                "DeviceReconciliationReceipt does not bind committed ExclusiveDeviceLease",
            ),
            Self::ConflictingReconciliationReceipt {
                existing,
                requested,
            } => write!(
                formatter,
                "conflicting DeviceReconciliationReceipt: existing {existing:?}, requested {requested:?}"
            ),
            Self::ResetGenerationExhausted => {
                formatter.write_str("ExclusiveDeviceLease reset_generation exhausted")
            }
            Self::BootGenerationNotAdvanced { lost, reconnected } => write!(
                formatter,
                "ExclusiveDeviceLease WAL-loss boot generation not advanced: reconnected {} <= lost {}",
                reconnected.get(),
                lost.get()
            ),
            Self::Storage(reason) => write!(
                formatter,
                "ExclusiveDeviceLease reconcile storage error: {reason}"
            ),
        }
    }
}

impl Error for DeviceReconcileError {}

/// Outcome of a `QuotaLease` migration (`[LEASE-MIGRATE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaMigrationOutcome {
    /// Source lease after drain + close + fence verification (`CLOSED`).
    pub closed_source: QuotaLeaseGrant,
    /// Newly issued target lease (`ISSUED`).
    pub target_grant: QuotaLeaseGrant,
}

/// Fail-closed errors for `QuotaLeaseGrantor::migrate_lease` (`[LEASE-MIGRATE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaMigrationError {
    /// Target grant failed fence or pool check.
    Grant(QuotaLeaseGrantError),
    /// Source lease ledger lookup/close failed.
    Ledger(QuotaLeaseLedgerError),
    /// Source lease is `QUARANTINED` (partitioned / unreconciled) and cannot migrate directly.
    SourceQuarantinedInPartition {
        /// Quarantined source lease id.
        lease_id: QuotaLeaseId,
    },
    /// Source lease has not been drained into `CLOSING` / `CLOSED`.
    SourceNotDrainedOrClosed {
        /// Source lease id.
        lease_id: QuotaLeaseId,
        /// Current state of the source lease.
        state: QuotaLeaseState,
    },
    /// Source `FenceBarrierAck` is missing or invalid.
    InvalidSourceFenceBarrierAck {
        /// Static diagnostic reason.
        reason: &'static str,
    },
    /// Requested target face value exceeds the unspent remainder returned from the source lease.
    TargetExceedsUnspentRemainder {
        /// Requested target face value.
        requested: u64,
        /// Unspent remainder returned from the source lease.
        unspent: u64,
    },
}

impl fmt::Display for QuotaMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grant(err) => write!(formatter, "QuotaLease migration grant failed: {err}"),
            Self::Ledger(err) => write!(formatter, "QuotaLease migration ledger failed: {err}"),
            Self::SourceQuarantinedInPartition { lease_id } => write!(
                formatter,
                "QuotaLease {lease_id:?} is QUARANTINED in partition and cannot migrate without reconciliation"
            ),
            Self::SourceNotDrainedOrClosed { lease_id, state } => write!(
                formatter,
                "QuotaLease {lease_id:?} must be CLOSING or CLOSED before migration, found {state:?}"
            ),
            Self::InvalidSourceFenceBarrierAck { reason } => write!(
                formatter,
                "QuotaLease migration source FenceBarrierAck invalid: {reason}"
            ),
            Self::TargetExceedsUnspentRemainder { requested, unspent } => write!(
                formatter,
                "QuotaLease migration target face_value {requested} exceeds source unspent {unspent}"
            ),
        }
    }
}

impl Error for QuotaMigrationError {}

/// Outcome of a `CapacityLease` migration (`[LEASE-CAPACITY-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityMigrationOutcome {
    /// Reconciled and returned source capacity lease (`RETURNED`).
    pub returned_source: CapacityLeaseGrant,
    /// Newly granted target capacity lease (`GLOBAL_RESERVED`).
    pub target_grant: CapacityLeaseGrant,
}

/// Fail-closed errors for `CapacityLeaseGrantor::migrate_lease` (`[LEASE-CAPACITY-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityMigrationError {
    /// Target grant failed fence or pool check.
    Grant(CapacityLeaseGrantError),
    /// Source lease return/fence transition failed.
    Return(CapacityLeaseReturnError),
    /// Source lease reconciliation failed.
    Reconcile(CapacityReconcileError),
    /// Target amount exceeds the reconciled source lease amount.
    TargetExceedsSourceAmount {
        /// Requested target amount.
        requested: u64,
        /// Source lease amount.
        source_amount: u64,
    },
}

impl fmt::Display for CapacityMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grant(err) => write!(formatter, "CapacityLease migration grant failed: {err}"),
            Self::Return(err) => write!(formatter, "CapacityLease migration fence failed: {err}"),
            Self::Reconcile(err) => {
                write!(formatter, "CapacityLease migration reconcile failed: {err}")
            }
            Self::TargetExceedsSourceAmount {
                requested,
                source_amount,
            } => write!(
                formatter,
                "CapacityLease migration target amount {requested} exceeds source amount {source_amount}"
            ),
        }
    }
}

impl Error for CapacityMigrationError {}

/// Outcome of an `ExclusiveDeviceLease` migration (`[LEASE-DEVICE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceMigrationOutcome {
    /// Reconciled and returned source device lease (`RETURNED`).
    pub returned_source: ExclusiveDeviceLeaseGrant,
    /// Newly granted target device lease (`DEVICE_RESERVED`) with bumped
    /// exclusivity epoch and bumped head `reset_generation`.
    pub target_grant: ExclusiveDeviceLeaseGrant,
}

/// Fail-closed errors for `ExclusiveDeviceLeaseGrantor::migrate_lease`
/// (`[LEASE-DEVICE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceMigrationError {
    /// Target grant failed fence or head check.
    Grant(ExclusiveDeviceLeaseGrantError),
    /// Source lease fence transition failed.
    Return(ExclusiveDeviceLeaseReturnError),
    /// Source lease reconciliation failed.
    Reconcile(DeviceReconcileError),
    /// Target exclusivity epoch did not advance past the source lease's exclusivity epoch.
    ExclusivityEpochNotBumped {
        /// Source lease exclusivity epoch.
        source_epoch: CellEpoch,
        /// Target fence epoch.
        target_epoch: CellEpoch,
    },
}

impl fmt::Display for DeviceMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grant(err) => {
                write!(
                    formatter,
                    "ExclusiveDeviceLease migration grant failed: {err}"
                )
            }
            Self::Return(err) => {
                write!(
                    formatter,
                    "ExclusiveDeviceLease migration fence failed: {err}"
                )
            }
            Self::Reconcile(err) => write!(
                formatter,
                "ExclusiveDeviceLease migration reconcile failed: {err}"
            ),
            Self::ExclusivityEpochNotBumped {
                source_epoch,
                target_epoch,
            } => write!(
                formatter,
                "ExclusiveDeviceLease migration requires bumped exclusivity_epoch: target {} <= source {}",
                target_epoch.get(),
                source_epoch.get()
            ),
        }
    }
}

impl Error for DeviceMigrationError {}

/// Fail-closed errors for `QuotaLease` local WAL-loss handling (`[LEASE-WAL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaWalLossError {
    /// Cell epoch advance failed.
    Cell(CellError),
    /// Unknown `QuotaLeaseId`.
    UnknownLease,
    /// Rebooted boot generation did not advance past the WAL-lost generation.
    BootGenerationNotAdvanced {
        /// Lost boot generation.
        lost: Generation,
        /// Reconnected boot generation.
        reconnected: Generation,
    },
    /// Guessing `remaining` from `face_value` after local WAL loss is forbidden (`[LEASE-WAL-001]`).
    GuessedRemainingForbidden {
        /// Quarantined lease id.
        lease_id: QuotaLeaseId,
        /// Issued face value.
        face_value: u64,
        /// Caller's guessed remaining balance.
        guessed_remaining: u64,
    },
}

impl From<CellError> for QuotaWalLossError {
    fn from(error: CellError) -> Self {
        Self::Cell(error)
    }
}

impl fmt::Display for QuotaWalLossError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cell(err) => write!(formatter, "QuotaLease WAL-loss epoch advance failed: {err}"),
            Self::UnknownLease => formatter.write_str("unknown QuotaLease on WAL loss"),
            Self::BootGenerationNotAdvanced { lost, reconnected } => write!(
                formatter,
                "QuotaLease WAL-loss boot generation not advanced: reconnected {} <= lost {}",
                reconnected.get(),
                lost.get()
            ),
            Self::GuessedRemainingForbidden {
                lease_id,
                face_value,
                guessed_remaining,
            } => write!(
                formatter,
                "LEASE-WAL-001 violation: guessing remaining {guessed_remaining} from face_value {face_value} for {lease_id:?} is forbidden"
            ),
        }
    }
}

impl Error for QuotaWalLossError {}

pub(crate) fn verify_quota_fence_ack(
    ack: &FenceBarrierAck,
    lease: &QuotaLeaseGrant,
    expected_high_water: u64,
) -> Result<(), &'static str> {
    if ack.lease_id != *lease.lease_id.as_bytes() {
        return Err("FenceBarrierAck lease_id mismatch");
    }
    if ack.fence_scope != lease.fence_scope {
        return Err("FenceBarrierAck fence_scope mismatch");
    }
    if ack.epoch != lease.epoch {
        return Err("FenceBarrierAck epoch mismatch");
    }
    if ack.fencing_token != lease.fencing_token {
        return Err("FenceBarrierAck fencing_token mismatch");
    }
    if ack.observed_high_water != expected_high_water {
        return Err("FenceBarrierAck observed_high_water mismatch");
    }
    if !ack.verify_digest() {
        return Err("FenceBarrierAck digest verification failed");
    }
    Ok(())
}

pub(crate) fn verify_capacity_fence_ack(
    ack: &FenceBarrierAck,
    lease: &CapacityLeaseGrant,
) -> Result<(), &'static str> {
    if ack.lease_id != *lease.capacity_lease_id.as_bytes() {
        return Err("FenceBarrierAck capacity_lease_id mismatch");
    }
    if ack.fence_scope != lease.fence_scope {
        return Err("FenceBarrierAck fence_scope mismatch");
    }
    if ack.epoch != lease.capacity_epoch {
        return Err("FenceBarrierAck capacity_epoch mismatch");
    }
    if ack.fencing_token != lease.fencing_token {
        return Err("FenceBarrierAck fencing_token mismatch");
    }
    if ack.observed_high_water != lease.amount {
        return Err("FenceBarrierAck observed_high_water != amount");
    }
    if !ack.verify_digest() {
        return Err("FenceBarrierAck digest verification failed");
    }
    Ok(())
}

pub(crate) fn verify_cleanup_attestation(
    attestation: &CleanupAttestation,
    lease: &CapacityLeaseGrant,
) -> Result<(), &'static str> {
    if attestation.capacity_lease_id != lease.capacity_lease_id
        || attestation.target_node != lease.target_node
        || attestation.target_node_boot_generation != lease.target_node_boot_generation
        || attestation.capacity_epoch != lease.capacity_epoch
        || attestation.fencing_token != lease.fencing_token
    {
        return Err("CleanupAttestation axes do not bind CapacityLease");
    }
    if attestation.cleaned_amount != lease.amount {
        return Err("CleanupAttestation cleaned_amount != lease amount");
    }
    if !attestation.holder_processes_terminated {
        return Err("CleanupAttestation holder_processes_terminated is false");
    }
    if !attestation.verify_digest() {
        return Err("CleanupAttestation digest verification failed");
    }
    Ok(())
}

pub(crate) fn verify_device_fence_ack(
    ack: &FenceBarrierAck,
    lease: &ExclusiveDeviceLeaseGrant,
) -> Result<(), &'static str> {
    if ack.lease_id != *lease.device_lease_id.as_bytes() {
        return Err("FenceBarrierAck device_lease_id mismatch");
    }
    if ack.fence_scope != lease.fence_scope {
        return Err("FenceBarrierAck fence_scope mismatch");
    }
    if ack.epoch != lease.exclusivity_epoch {
        return Err("FenceBarrierAck exclusivity_epoch mismatch");
    }
    if ack.fencing_token != lease.fencing_token {
        return Err("FenceBarrierAck fencing_token mismatch");
    }
    if ack.observed_high_water != lease.exclusivity_epoch.get() {
        return Err("FenceBarrierAck observed_high_water != exclusivity_epoch");
    }
    if !ack.verify_digest() {
        return Err("FenceBarrierAck digest verification failed");
    }
    Ok(())
}

impl QuotaLeaseGrantor {
    /// Returns all committed [`QuotaReconciliationReceipt`] records for
    /// `lease_id` in `reconcile_seq` order.
    #[must_use]
    pub fn reconciliation_receipts(&self, lease_id: QuotaLeaseId) -> &[QuotaReconciliationReceipt] {
        self.reconciliation_receipts
            .get(&lease_id)
            .map_or(&[], Vec::as_slice)
    }

    /// Reconciles a `QUARANTINED` or `FENCED` `QuotaLease` upon a valid
    /// [`QuotaReconciliationReceipt`] and [`FenceBarrierAck`]
    /// (`[LEASE-RECON-001]`, `[LEASE-FENCE-001]`).
    ///
    /// Returns the proven unspent delta (`returned_to_available - lease.returned`)
    /// to `AVAILABLE`, updates `spent` to `settled_spent`, and transitions the
    /// lease to `CLOSED` when `remaining_quarantined == 0` (or keeps it in
    /// `QUARANTINED` with `remaining = remaining_quarantined` when partially
    /// reconciled). Replaying an identical `(lease_id, reconcile_seq)` receipt
    /// is idempotent.
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`QuotaReconcileError`].
    #[allow(clippy::too_many_lines)]
    pub fn reconcile_quarantined(
        &mut self,
        presented: &CellFence,
        receipt: QuotaReconciliationReceipt,
    ) -> Result<QuotaLeaseGrant, QuotaReconcileError> {
        self.authority.admit(presented)?;
        if receipt.reconcile_seq == 0 {
            return Err(QuotaReconcileError::InvalidReconcileSeq {
                expected_min: 1,
                requested: 0,
            });
        }

        // Check idempotent replay or receipt_id conflict across all leases.
        for history in self.reconciliation_receipts.values() {
            for existing in history {
                if existing.receipt_id == receipt.receipt_id {
                    if *existing == receipt {
                        return self
                            .leases
                            .get(&receipt.lease_id)
                            .copied()
                            .ok_or(QuotaReconcileError::UnknownLease);
                    }
                    return Err(QuotaReconcileError::ConflictingReconciliationReceipt {
                        existing: existing.receipt_id,
                        requested: receipt.receipt_id,
                    });
                }
            }
        }

        if let Some(history) = self.reconciliation_receipts.get(&receipt.lease_id) {
            if let Some(same_seq) = history
                .iter()
                .find(|entry| entry.reconcile_seq == receipt.reconcile_seq)
            {
                if *same_seq == receipt {
                    return self
                        .leases
                        .get(&receipt.lease_id)
                        .copied()
                        .ok_or(QuotaReconcileError::UnknownLease);
                }
                return Err(QuotaReconcileError::ConflictingReconciliationReceipt {
                    existing: same_seq.receipt_id,
                    requested: receipt.receipt_id,
                });
            }
            if let Some(last) = history.last()
                && receipt.reconcile_seq <= last.reconcile_seq
            {
                return Err(QuotaReconcileError::InvalidReconcileSeq {
                    expected_min: last.reconcile_seq + 1,
                    requested: receipt.reconcile_seq,
                });
            }
        }

        let existing = self
            .leases
            .get(&receipt.lease_id)
            .copied()
            .ok_or(QuotaReconcileError::UnknownLease)?;

        if !matches!(
            existing.state,
            QuotaLeaseState::Quarantined | QuotaLeaseState::Fenced
        ) {
            return Err(QuotaReconcileError::NotQuarantinedOrFenced {
                state: existing.state,
            });
        }

        if receipt.cell != existing.cell
            || receipt.node_boot_generation != existing.node_boot_generation
            || receipt.epoch != existing.epoch
            || receipt.fencing_token != existing.fencing_token
            || receipt.face_value != existing.face_value
        {
            return Err(QuotaReconcileError::ReceiptBindMismatch);
        }

        if let Err(reason) = verify_quota_fence_ack(
            &receipt.fence_barrier_ack,
            &existing,
            receipt.high_water_spent,
        ) {
            return Err(QuotaReconcileError::InvalidFenceBarrierAck { reason });
        }

        if receipt.high_water_spent < existing.spent {
            return Err(QuotaReconcileError::HighWaterRegression {
                reported: receipt.high_water_spent,
                committed: existing.spent,
            });
        }
        if receipt.high_water_spent > existing.face_value {
            return Err(QuotaReconcileError::HighWaterExceedsFace {
                reported: receipt.high_water_spent,
                face_value: existing.face_value,
            });
        }
        if receipt.returned_to_available < existing.returned {
            return Err(QuotaReconcileError::ReturnedRegression {
                reported: receipt.returned_to_available,
                committed: existing.returned,
            });
        }
        if !receipt.verify_conservation() {
            return Err(QuotaReconcileError::ConservationViolated {
                face_value: receipt.face_value,
                settled_spent: receipt.settled_spent,
                returned_to_available: receipt.returned_to_available,
                remaining_quarantined: receipt.remaining_quarantined,
            });
        }

        let delta_returned = receipt.returned_to_available - existing.returned;
        let next_available = self.available + delta_returned;
        let next_state = if receipt.remaining_quarantined == 0 {
            QuotaLeaseState::Closed
        } else {
            QuotaLeaseState::Quarantined
        };
        let next = QuotaLeaseGrant {
            spent: receipt.settled_spent,
            returned: receipt.returned_to_available,
            remaining: receipt.remaining_quarantined,
            state: next_state,
            ..existing
        };

        if let Some(store) = &self.store {
            store.persist_quota_reconcile(
                existing.state,
                &next,
                &receipt,
                (self.available, next_available),
            )?;
        }

        self.available = next_available;
        self.leases.insert(receipt.lease_id, next);
        self.reconciliation_receipts
            .entry(receipt.lease_id)
            .or_default()
            .push(receipt);
        Ok(next)
    }

    /// Alias for [`Self::reconcile_quarantined`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::reconcile_quarantined`].
    pub fn reconcile(
        &mut self,
        presented: &CellFence,
        receipt: QuotaReconciliationReceipt,
    ) -> Result<QuotaLeaseGrant, QuotaReconcileError> {
        self.reconcile_quarantined(presented, receipt)
    }

    /// Executes cross-node / cross-generation `QuotaLease` migration:
    /// `drain -> close source lease -> fence source -> grant target lease`
    /// (`[LEASE-MIGRATE-001]`).
    ///
    /// Refuses migration if the source lease is in `QUARANTINED` (partitioned /
    /// unreconciled) or has not been drained into `CLOSING` / `CLOSED`, if the
    /// source `FenceBarrierAck` is invalid, or if `target_face_value` exceeds
    /// the unspent remainder returned from the source lease.
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`QuotaMigrationError`].
    pub fn migrate_lease(
        &mut self,
        presented: &CellFence,
        source_lease_id: QuotaLeaseId,
        source_fence_ack: &FenceBarrierAck,
        target_lease_id: QuotaLeaseId,
        target_face_value: u64,
        target_expires_at: Option<LeaseInstant>,
    ) -> Result<QuotaMigrationOutcome, QuotaMigrationError> {
        self.authority
            .admit(presented)
            .map_err(|err| QuotaMigrationError::Grant(QuotaLeaseGrantError::from(err)))?;

        let source = self
            .query(source_lease_id)
            .map_err(QuotaMigrationError::Ledger)?;

        match source.state {
            QuotaLeaseState::Quarantined => {
                return Err(QuotaMigrationError::SourceQuarantinedInPartition {
                    lease_id: source_lease_id,
                });
            }
            QuotaLeaseState::Closing | QuotaLeaseState::Closed => {}
            state => {
                return Err(QuotaMigrationError::SourceNotDrainedOrClosed {
                    lease_id: source_lease_id,
                    state,
                });
            }
        }

        if let Err(reason) = verify_quota_fence_ack(source_fence_ack, &source, source.spent) {
            return Err(QuotaMigrationError::InvalidSourceFenceBarrierAck { reason });
        }

        let closed_source = if source.state == QuotaLeaseState::Closing {
            self.ack_close(presented, source_lease_id)
                .map_err(QuotaMigrationError::Ledger)?
        } else {
            source
        };

        if target_face_value > closed_source.returned {
            return Err(QuotaMigrationError::TargetExceedsUnspentRemainder {
                requested: target_face_value,
                unspent: closed_source.returned,
            });
        }

        let target_grant = self
            .grant(
                presented,
                target_lease_id,
                target_face_value,
                target_expires_at,
            )
            .map_err(QuotaMigrationError::Grant)?;

        Ok(QuotaMigrationOutcome {
            closed_source,
            target_grant,
        })
    }

    /// Handles node local lease WAL loss (`[LEASE-WAL-001]`): verifies that the
    /// node rebooted with a strictly higher `boot_generation`, advances the
    /// Cell epoch/fencing token, and moves all unsettled leases into
    /// `QUARANTINED` without returning any funds to `AVAILABLE`.
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`QuotaWalLossError`].
    pub fn fence_boot_generation_on_wal_loss(
        &mut self,
        lost_boot_generation: Generation,
        reconnected_boot_generation: Generation,
    ) -> Result<(CellFence, Vec<QuotaLeaseId>), QuotaWalLossError> {
        if reconnected_boot_generation <= lost_boot_generation {
            return Err(QuotaWalLossError::BootGenerationNotAdvanced {
                lost: lost_boot_generation,
                reconnected: reconnected_boot_generation,
            });
        }
        let mut quarantined_ids: Vec<QuotaLeaseId> = self
            .leases
            .values()
            .filter(|lease| {
                matches!(
                    lease.state,
                    QuotaLeaseState::Issued
                        | QuotaLeaseState::Active
                        | QuotaLeaseState::Closing
                        | QuotaLeaseState::Fenced
                )
            })
            .map(|lease| lease.lease_id)
            .collect();
        quarantined_ids.sort_unstable();

        let new_fence = self.advance_epoch_and_quarantine()?;
        Ok((new_fence, quarantined_ids))
    }

    /// Alias for [`Self::fence_boot_generation_on_wal_loss`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::fence_boot_generation_on_wal_loss`].
    pub fn handle_node_wal_loss(
        &mut self,
        lost_boot_generation: Generation,
        reconnected_boot_generation: Generation,
    ) -> Result<(CellFence, Vec<QuotaLeaseId>), QuotaWalLossError> {
        self.fence_boot_generation_on_wal_loss(lost_boot_generation, reconnected_boot_generation)
    }

    /// Explicitly rejects any attempt to recover a WAL-lost lease by guessing
    /// `remaining` from `face_value` (`[LEASE-WAL-001]`).
    ///
    /// # Errors
    ///
    /// Always returns [`QuotaWalLossError::GuessedRemainingForbidden`] (or
    /// [`QuotaWalLossError::UnknownLease`] if the lease id does not exist).
    pub fn refuse_guessed_remaining_on_wal_loss(
        &self,
        lease_id: QuotaLeaseId,
        guessed_remaining: u64,
    ) -> Result<QuotaLeaseGrant, QuotaWalLossError> {
        let lease = self
            .leases
            .get(&lease_id)
            .copied()
            .ok_or(QuotaWalLossError::UnknownLease)?;
        Err(QuotaWalLossError::GuessedRemainingForbidden {
            lease_id,
            face_value: lease.face_value,
            guessed_remaining,
        })
    }
}

impl CapacityLeaseGrantor {
    /// Returns the committed [`CapacityReconciliationReceipt`] for
    /// `capacity_lease_id`, if reconciled.
    #[must_use]
    pub fn reconciliation_receipt(
        &self,
        capacity_lease_id: CapacityLeaseId,
    ) -> Option<CapacityReconciliationReceipt> {
        self.reconciliation_receipts
            .get(&capacity_lease_id)
            .copied()
    }

    /// Fences a capacity lease (`GLOBAL_RESERVED` / `TARGET_PREPARED` /
    /// `ACTIVE` / `RETURNING` → `FENCED`) without returning capacity to the
    /// allocatable pool (`[LEASE-CAPACITY-002]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`CapacityLeaseReturnError`].
    pub fn fence_lease(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.authority.admit(presented)?;
        let existing = self.query(capacity_lease_id)?;
        match existing.state {
            prev @ (CapacityLeaseState::GlobalReserved
            | CapacityLeaseState::TargetPrepared
            | CapacityLeaseState::Active
            | CapacityLeaseState::Returning) => {
                let next = CapacityLeaseGrant {
                    state: CapacityLeaseState::Fenced,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_capacity_state_transition(
                        capacity_lease_id,
                        prev,
                        CapacityLeaseState::Fenced,
                        None,
                    )?;
                }
                self.leases.insert(capacity_lease_id, next);
                Ok(next)
            }
            CapacityLeaseState::Fenced => Ok(existing),
            state @ (CapacityLeaseState::Quarantined | CapacityLeaseState::Returned) => {
                Err(CapacityLeaseReturnError::NotReserved { state })
            }
        }
    }

    /// Moves an unsettled capacity lease into `QUARANTINED` without returning
    /// capacity to the source pool (`[LEASE-CAPACITY-002]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`CapacityLeaseReturnError`].
    pub fn quarantine_lease(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
    ) -> Result<CapacityLeaseGrant, CapacityLeaseReturnError> {
        self.authority.admit(presented)?;
        let existing = self.query(capacity_lease_id)?;
        match existing.state {
            prev @ (CapacityLeaseState::GlobalReserved
            | CapacityLeaseState::TargetPrepared
            | CapacityLeaseState::Active
            | CapacityLeaseState::Returning
            | CapacityLeaseState::Fenced) => {
                let next = CapacityLeaseGrant {
                    state: CapacityLeaseState::Quarantined,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_capacity_state_transition(
                        capacity_lease_id,
                        prev,
                        CapacityLeaseState::Quarantined,
                        None,
                    )?;
                }
                self.leases.insert(capacity_lease_id, next);
                Ok(next)
            }
            CapacityLeaseState::Quarantined => Ok(existing),
            state @ CapacityLeaseState::Returned => {
                Err(CapacityLeaseReturnError::NotReserved { state })
            }
        }
    }

    /// Reconciles a `FENCED`, `QUARANTINED`, or `RETURNING` `CapacityLease`
    /// upon a valid [`CapacityReconciliationReceipt`] proving `amount`,
    /// `FenceBarrierAck`, and `CleanupAttestation`, transitioning the lease to
    /// `RETURNED` and restoring `amount` to `pool_remaining` once
    /// (`[LEASE-CAPACITY-002]`, `[LEASE-RECON-001]`, `[LEASE-FENCE-001]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`CapacityReconcileError`].
    pub fn reconcile(
        &mut self,
        presented: &CellFence,
        receipt: CapacityReconciliationReceipt,
    ) -> Result<CapacityLeaseGrant, CapacityReconcileError> {
        self.authority.admit(presented)?;

        for existing in self.reconciliation_receipts.values() {
            if existing.receipt_id == receipt.receipt_id {
                if *existing == receipt {
                    return self
                        .leases
                        .get(&receipt.capacity_lease_id)
                        .copied()
                        .ok_or(CapacityReconcileError::UnknownLease);
                }
                return Err(CapacityReconcileError::ConflictingReconciliationReceipt {
                    existing: existing.receipt_id,
                    requested: receipt.receipt_id,
                });
            }
        }

        if let Some(committed) = self.reconciliation_receipts.get(&receipt.capacity_lease_id) {
            if *committed == receipt {
                return self
                    .leases
                    .get(&receipt.capacity_lease_id)
                    .copied()
                    .ok_or(CapacityReconcileError::UnknownLease);
            }
            return Err(CapacityReconcileError::ConflictingReconciliationReceipt {
                existing: committed.receipt_id,
                requested: receipt.receipt_id,
            });
        }

        let lease = self
            .leases
            .get(&receipt.capacity_lease_id)
            .copied()
            .ok_or(CapacityReconcileError::UnknownLease)?;

        if !matches!(
            lease.state,
            CapacityLeaseState::Fenced
                | CapacityLeaseState::Quarantined
                | CapacityLeaseState::Returning
        ) {
            return Err(CapacityReconcileError::NotReconcilableState { state: lease.state });
        }

        if receipt.target_node != lease.target_node
            || receipt.target_node_boot_generation != lease.target_node_boot_generation
            || receipt.capacity_epoch != lease.capacity_epoch
            || receipt.fencing_token != lease.fencing_token
            || receipt.amount != lease.amount
        {
            return Err(CapacityReconcileError::ReceiptBindMismatch);
        }

        if let Err(reason) = verify_capacity_fence_ack(&receipt.fence_barrier_ack, &lease) {
            return Err(CapacityReconcileError::InvalidFenceBarrierAck { reason });
        }

        if let Err(reason) = verify_cleanup_attestation(&receipt.cleanup_attestation, &lease) {
            return Err(CapacityReconcileError::InvalidCleanupAttestation { reason });
        }

        let next_pool = self.pool_remaining + lease.amount;
        let next = CapacityLeaseGrant {
            state: CapacityLeaseState::Returned,
            ..lease
        };

        if let Some(store) = &self.store {
            store.persist_capacity_reconcile(
                lease.capacity_lease_id,
                lease.state,
                &receipt,
                (self.pool_remaining, next_pool),
            )?;
        }

        self.pool_remaining = next_pool;
        self.leases.insert(lease.capacity_lease_id, next);
        self.reconciliation_receipts
            .insert(lease.capacity_lease_id, receipt);
        Ok(next)
    }

    /// Alias for [`Self::reconcile`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::reconcile`].
    pub fn reconcile_quarantined(
        &mut self,
        presented: &CellFence,
        receipt: CapacityReconciliationReceipt,
    ) -> Result<CapacityLeaseGrant, CapacityReconcileError> {
        self.reconcile(presented, receipt)
    }

    /// Executes `CapacityLease` migration:
    /// `freeze new use -> fence source holder/boot generation -> cleanup/reconcile -> return & regrant to target node`
    /// (`[LEASE-CAPACITY-002]`, `[LEASE-MIGRATE-001]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`CapacityMigrationError`].
    pub fn migrate_lease(
        &mut self,
        presented: &CellFence,
        source_reconcile_receipt: CapacityReconciliationReceipt,
        target_lease_id: CapacityLeaseId,
        target_amount: u64,
    ) -> Result<CapacityMigrationOutcome, CapacityMigrationError> {
        self.authority
            .admit(presented)
            .map_err(|err| CapacityMigrationError::Grant(CapacityLeaseGrantError::from(err)))?;

        let source_id = source_reconcile_receipt.capacity_lease_id();
        let source = self
            .query(source_id)
            .map_err(CapacityMigrationError::Return)?;

        if target_amount > source.amount {
            return Err(CapacityMigrationError::TargetExceedsSourceAmount {
                requested: target_amount,
                source_amount: source.amount,
            });
        }

        if matches!(
            source.state,
            CapacityLeaseState::GlobalReserved
                | CapacityLeaseState::TargetPrepared
                | CapacityLeaseState::Active
        ) {
            self.fence_lease(presented, source_id)
                .map_err(CapacityMigrationError::Return)?;
        }

        let returned_source = self
            .reconcile(presented, source_reconcile_receipt)
            .map_err(CapacityMigrationError::Reconcile)?;

        let target_grant = self
            .grant(presented, target_lease_id, target_amount)
            .map_err(CapacityMigrationError::Grant)?;

        Ok(CapacityMigrationOutcome {
            returned_source,
            target_grant,
        })
    }

    /// Quarantines all unsettled capacity leases bound to `lost_boot_generation`
    /// when a node's local lease WAL is lost and it reconnects with a higher
    /// `reconnected_boot_generation` (`[LEASE-WAL-001]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`CapacityReconcileError`].
    pub fn fence_boot_generation_on_wal_loss(
        &mut self,
        presented: &CellFence,
        lost_boot_generation: Generation,
        reconnected_boot_generation: Generation,
    ) -> Result<Vec<CapacityLeaseId>, CapacityReconcileError> {
        self.authority.admit(presented)?;
        if reconnected_boot_generation <= lost_boot_generation {
            return Err(CapacityReconcileError::BootGenerationNotAdvanced {
                lost: lost_boot_generation,
                reconnected: reconnected_boot_generation,
            });
        }
        let mut to_quarantine: Vec<CapacityLeaseId> = self
            .leases
            .values()
            .filter(|lease| lease.target_node_boot_generation <= lost_boot_generation)
            .filter(|lease| {
                !matches!(
                    lease.state,
                    CapacityLeaseState::Returned | CapacityLeaseState::Quarantined
                )
            })
            .map(|lease| lease.capacity_lease_id)
            .collect();
        to_quarantine.sort_unstable();

        for lease_id in &to_quarantine {
            let prev = self.leases[lease_id].state;
            if let Some(store) = &self.store {
                store.persist_capacity_state_transition(
                    *lease_id,
                    prev,
                    CapacityLeaseState::Quarantined,
                    None,
                )?;
            }
            if let Some(lease) = self.leases.get_mut(lease_id) {
                lease.state = CapacityLeaseState::Quarantined;
            }
        }
        Ok(to_quarantine)
    }
}

impl ExclusiveDeviceLeaseGrantor {
    /// Returns the committed [`DeviceReconciliationReceipt`] for
    /// `device_lease_id`, if reconciled.
    #[must_use]
    pub fn reconciliation_receipt(
        &self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Option<DeviceReconciliationReceipt> {
        self.reconciliation_receipts.get(&device_lease_id).copied()
    }

    /// Fences an exclusive device lease (`DEVICE_RESERVED` / `RESETTING` →
    /// `FENCING`) while keeping the `DeviceLeaseHead` occupied (`!is_free`)
    /// (`[LEASE-DEVICE-001]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`ExclusiveDeviceLeaseReturnError`].
    pub fn fence_lease(
        &mut self,
        presented: &CellFence,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseReturnError> {
        self.authority.admit(presented)?;
        let existing = self.query(device_lease_id)?;
        match existing.state {
            prev @ (ExclusiveDeviceLeaseState::DeviceReserved
            | ExclusiveDeviceLeaseState::Resetting) => {
                let next = ExclusiveDeviceLeaseGrant {
                    state: ExclusiveDeviceLeaseState::Fencing,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_device_state_transition(
                        device_lease_id,
                        prev,
                        ExclusiveDeviceLeaseState::Fencing,
                    )?;
                }
                self.leases.insert(device_lease_id, next);
                Ok(next)
            }
            ExclusiveDeviceLeaseState::Fencing => Ok(existing),
            state @ (ExclusiveDeviceLeaseState::Quarantined
            | ExclusiveDeviceLeaseState::Returned) => {
                Err(ExclusiveDeviceLeaseReturnError::DeclareRequiresReserved { state })
            }
        }
    }

    /// Quarantines an unsettled exclusive device lease (`DEVICE_RESERVED` /
    /// `RESETTING` / `FENCING` → `QUARANTINED`) while keeping the
    /// `DeviceLeaseHead` occupied (`!is_free`) (`[LEASE-DEVICE-001]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`ExclusiveDeviceLeaseReturnError`].
    pub fn quarantine_lease(
        &mut self,
        presented: &CellFence,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseReturnError> {
        self.authority.admit(presented)?;
        let existing = self.query(device_lease_id)?;
        match existing.state {
            prev @ (ExclusiveDeviceLeaseState::DeviceReserved
            | ExclusiveDeviceLeaseState::Resetting
            | ExclusiveDeviceLeaseState::Fencing) => {
                let next = ExclusiveDeviceLeaseGrant {
                    state: ExclusiveDeviceLeaseState::Quarantined,
                    ..existing
                };
                if let Some(store) = &self.store {
                    store.persist_device_state_transition(
                        device_lease_id,
                        prev,
                        ExclusiveDeviceLeaseState::Quarantined,
                    )?;
                }
                self.leases.insert(device_lease_id, next);
                Ok(next)
            }
            ExclusiveDeviceLeaseState::Quarantined => Ok(existing),
            state @ ExclusiveDeviceLeaseState::Returned => {
                Err(ExclusiveDeviceLeaseReturnError::DeclareRequiresReserved { state })
            }
        }
    }

    /// Reconciles a `QUARANTINED`, `FENCING`, or `RESETTING`
    /// `ExclusiveDeviceLease` upon a valid [`DeviceReconciliationReceipt`],
    /// proving `exclusivity_epoch`, old fencing token rejection, queue drain,
    /// `FenceBarrierAck`, and `DeviceResetReceipt` zeroization attestation
    /// before clearing the `DeviceLeaseHead` back to `FREE` with a bumped
    /// `reset_generation` (`[LEASE-DEVICE-001]`, `[LEASE-RECON-001]`,
    /// `[LEASE-FENCE-001]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`DeviceReconcileError`].
    #[allow(clippy::too_many_lines)]
    pub fn reconcile(
        &mut self,
        presented: &CellFence,
        receipt: DeviceReconciliationReceipt,
    ) -> Result<ExclusiveDeviceLeaseGrant, DeviceReconcileError> {
        self.authority.admit(presented)?;

        for existing in self.reconciliation_receipts.values() {
            if existing.receipt_id == receipt.receipt_id {
                if *existing == receipt {
                    return self
                        .leases
                        .get(&receipt.device_lease_id)
                        .copied()
                        .ok_or(DeviceReconcileError::UnknownLease);
                }
                return Err(DeviceReconcileError::ConflictingReconciliationReceipt {
                    existing: existing.receipt_id,
                    requested: receipt.receipt_id,
                });
            }
        }

        if let Some(committed) = self.reconciliation_receipts.get(&receipt.device_lease_id) {
            if *committed == receipt {
                return self
                    .leases
                    .get(&receipt.device_lease_id)
                    .copied()
                    .ok_or(DeviceReconcileError::UnknownLease);
            }
            return Err(DeviceReconcileError::ConflictingReconciliationReceipt {
                existing: committed.receipt_id,
                requested: receipt.receipt_id,
            });
        }

        let lease = self
            .leases
            .get(&receipt.device_lease_id)
            .copied()
            .ok_or(DeviceReconcileError::UnknownLease)?;

        if !matches!(
            lease.state,
            ExclusiveDeviceLeaseState::Quarantined
                | ExclusiveDeviceLeaseState::Fencing
                | ExclusiveDeviceLeaseState::Resetting
        ) {
            return Err(DeviceReconcileError::NotReconcilableState { state: lease.state });
        }

        if receipt.device_id != self.device_id
            || receipt.device_id != lease.device_id
            || receipt.holder_node != lease.holder_node
            || receipt.holder_node_boot_generation != lease.holder_node_boot_generation
            || receipt.exclusivity_epoch != lease.exclusivity_epoch
            || receipt.fencing_token != lease.fencing_token
        {
            return Err(DeviceReconcileError::ReceiptBindMismatch);
        }

        if receipt.reset_generation != self.reset_generation {
            return Err(DeviceReconcileError::StaleResetGeneration {
                receipt: receipt.reset_generation,
                head: self.reset_generation,
            });
        }

        if let Err(reason) = verify_device_fence_ack(&receipt.fence_barrier_ack, &lease) {
            return Err(DeviceReconcileError::InvalidFenceBarrierAck { reason });
        }

        if !receipt.endpoint_fenced_old_token {
            return Err(DeviceReconcileError::OldFencingTokenNotRejected);
        }
        if !receipt.queue_drained {
            return Err(DeviceReconcileError::QueueNotDrained);
        }
        if !receipt.vram_and_secrets_zeroized {
            return Err(DeviceReconcileError::ZeroizationNotAttested);
        }

        if !receipt_binds_lease(&receipt.reset_receipt, &lease) {
            return Err(DeviceReconcileError::ResetReceiptMismatch);
        }

        if let Some(conflict) = self
            .reset_receipts
            .iter()
            .find(|logged| logged.receipt_id == receipt.reset_receipt.receipt_id)
        {
            return Err(DeviceReconcileError::ConflictingReconciliationReceipt {
                existing: conflict.receipt_id,
                requested: receipt.reset_receipt.receipt_id,
            });
        }

        let next_gen = self
            .reset_generation
            .checked_next()
            .ok_or(DeviceReconcileError::ResetGenerationExhausted)?;
        let returned = ExclusiveDeviceLeaseGrant {
            state: ExclusiveDeviceLeaseState::Returned,
            ..lease
        };

        if let Some(store) = &self.store {
            store.persist_device_reconcile(
                self.device_id,
                lease.state,
                &receipt,
                self.reset_generation,
                next_gen,
            )?;
        }

        self.leases.insert(receipt.device_lease_id, returned);
        self.reset_receipts.push(receipt.reset_receipt);
        self.reconciliation_receipts
            .insert(receipt.device_lease_id, receipt);
        self.reset_generation = next_gen;
        self.free = true;
        Ok(returned)
    }

    /// Alias for [`Self::reconcile`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::reconcile`].
    pub fn reconcile_quarantined(
        &mut self,
        presented: &CellFence,
        receipt: DeviceReconciliationReceipt,
    ) -> Result<ExclusiveDeviceLeaseGrant, DeviceReconcileError> {
        self.reconcile(presented, receipt)
    }

    /// Executes `ExclusiveDeviceLease` migration:
    /// `fence source -> reset & zeroization receipt -> return head to FREE -> regrant to target holder with bumped exclusivity epoch`
    /// (`[LEASE-DEVICE-001]`, `[LEASE-MIGRATE-001]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`DeviceMigrationError`].
    pub fn migrate_lease(
        &mut self,
        presented: &CellFence,
        source_reconcile_receipt: DeviceReconciliationReceipt,
        target_lease_id: ExclusiveDeviceLeaseId,
    ) -> Result<DeviceMigrationOutcome, DeviceMigrationError> {
        self.authority.admit(presented).map_err(|err| {
            DeviceMigrationError::Grant(ExclusiveDeviceLeaseGrantError::from(err))
        })?;

        let source_id = source_reconcile_receipt.device_lease_id();
        let source = self
            .query(source_id)
            .map_err(DeviceMigrationError::Return)?;

        if presented.epoch() <= source.exclusivity_epoch {
            return Err(DeviceMigrationError::ExclusivityEpochNotBumped {
                source_epoch: source.exclusivity_epoch,
                target_epoch: presented.epoch(),
            });
        }

        if matches!(
            source.state,
            ExclusiveDeviceLeaseState::DeviceReserved | ExclusiveDeviceLeaseState::Resetting
        ) {
            self.fence_lease(presented, source_id)
                .map_err(DeviceMigrationError::Return)?;
        }

        let returned_source = self
            .reconcile(presented, source_reconcile_receipt)
            .map_err(DeviceMigrationError::Reconcile)?;

        let target_grant = self
            .grant(presented, target_lease_id)
            .map_err(DeviceMigrationError::Grant)?;

        Ok(DeviceMigrationOutcome {
            returned_source,
            target_grant,
        })
    }

    /// Quarantines all unsettled device leases bound to `lost_boot_generation`
    /// when a node's local lease WAL is lost and it reconnects with a higher
    /// `reconnected_boot_generation`, keeping the `DeviceLeaseHead` occupied
    /// until reset + zeroization reconciliation completes (`[LEASE-WAL-001]`).
    ///
    /// # Errors
    ///
    /// Typed fail-closed reject: [`DeviceReconcileError`].
    pub fn fence_boot_generation_on_wal_loss(
        &mut self,
        presented: &CellFence,
        lost_boot_generation: Generation,
        reconnected_boot_generation: Generation,
    ) -> Result<Vec<ExclusiveDeviceLeaseId>, DeviceReconcileError> {
        self.authority.admit(presented)?;
        if reconnected_boot_generation <= lost_boot_generation {
            return Err(DeviceReconcileError::BootGenerationNotAdvanced {
                lost: lost_boot_generation,
                reconnected: reconnected_boot_generation,
            });
        }
        let mut to_quarantine: Vec<ExclusiveDeviceLeaseId> = self
            .leases
            .values()
            .filter(|lease| lease.holder_node_boot_generation <= lost_boot_generation)
            .filter(|lease| {
                !matches!(
                    lease.state,
                    ExclusiveDeviceLeaseState::Returned | ExclusiveDeviceLeaseState::Quarantined
                )
            })
            .map(|lease| lease.device_lease_id)
            .collect();
        to_quarantine.sort_unstable();

        for lease_id in &to_quarantine {
            let prev = self.leases[lease_id].state;
            if let Some(store) = &self.store {
                store.persist_device_state_transition(
                    *lease_id,
                    prev,
                    ExclusiveDeviceLeaseState::Quarantined,
                )?;
            }
            if let Some(lease) = self.leases.get_mut(lease_id) {
                lease.state = ExclusiveDeviceLeaseState::Quarantined;
            }
        }
        Ok(to_quarantine)
    }
}
