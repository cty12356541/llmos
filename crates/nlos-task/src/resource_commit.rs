//! Task-side consumption of `ResourceAuthority` cost receipts.
//!
//! The Resource authority owns every cost fact. This module re-reads the
//! FINALIZED owner aggregate (`inspect_cost_receipt`) for exactly the
//! Reservations sealed in a `TaskWriteSet`, copies that full aggregate —
//! activation, every ordered consumption, and the finalization/refund
//! receipt — into two immutable nested Task tables inside the terminal
//! Task transaction, and replays from those Task rows alone. No public or
//! internal API here accepts an activation/finalization ID, consumption
//! sequence, usage, or refund value from the caller.
//!
//! This bridge is verify-then-commit, not cross-authority atomicity: the
//! owner read happens before the Task transaction opens. The combined
//! Semantic + Resource finalize rung is a documented gap.

use nlos_types::{
    CallId, OperationId, QuoteId, ReceiptId, ReservationId, ResourceAccountId, TaskId,
};
use rusqlite::{Row, Transaction, params};

use crate::reconcile::FinalizeRequestV3;
use crate::store::{SqlRead, SqliteTaskAuthority, blob16, blob32, encode_u64, u64_from_blob};
use crate::{
    AuthorityLeaseRecord, PermitState, TaskReceiptRecord, TaskStoreError, TaskWriteSetRecord,
    TaskWriteSetResourceReservation,
};

/// Owner-derived full cost aggregate nested under one terminal Task
/// receipt. Every field is copied from a committed
/// [`nlos_resource::ResourceCostReceipt`]; the nested receipt values reuse
/// the owner's public receipt types verbatim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NestedResourceCostReceipt {
    pub reservation_id: ReservationId,
    pub account_id: ResourceAccountId,
    pub quote_id: QuoteId,
    pub call_id: CallId,
    pub operation_id: OperationId,
    pub upper_bound: u64,
    pub activation: nlos_resource::ActivationReceipt,
    pub consumptions: Vec<nlos_resource::ConsumptionReceipt>,
    pub finalization: nlos_resource::FinalizationReceipt,
}

impl NestedResourceCostReceipt {
    #[must_use]
    pub fn from_owner(owner: nlos_resource::ResourceCostReceipt) -> Self {
        Self {
            reservation_id: owner.reservation_id,
            account_id: owner.account_id,
            quote_id: owner.quote_id,
            call_id: owner.call_id,
            operation_id: owner.operation_id,
            upper_bound: owner.upper_bound,
            activation: owner.activation,
            consumptions: owner.consumptions,
            finalization: owner.finalization,
        }
    }
}

/// Task terminal receipt plus the Resource cost evidence it nests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceTaskCommitReceipt {
    pub task_receipt: TaskReceiptRecord,
    pub resource_cost_receipts: Vec<NestedResourceCostReceipt>,
}

/// Idempotent result of Resource-aware Task finalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceFinalizeDecision {
    Committed(Box<ResourceTaskCommitReceipt>),
    Replayed(Box<ResourceTaskCommitReceipt>),
}

impl ResourceFinalizeDecision {
    #[must_use]
    pub fn receipt(&self) -> &ResourceTaskCommitReceipt {
        match self {
            Self::Committed(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

impl SqliteTaskAuthority {
    /// Finalizes an issued permit after re-reading the FINALIZED owner
    /// cost aggregate for every Reservation sealed in the permit's
    /// `TaskWriteSet`. The full aggregate (activation receipt, every
    /// ordered consumption receipt, finalization/refund receipt) is copied
    /// into immutable nested Task rows inside the same terminal Task
    /// transaction as the commit receipt, permit close, and head advance.
    ///
    /// A closed permit follows the standard v3 replay path and reads only
    /// the durable Task rows; the Resource authority is not consulted
    /// again. The request/idempotency identity is the plain
    /// [`FinalizeRequestV3`]; the owner receipt set is derived from the
    /// sealed write set, never selected by the caller.
    ///
    /// # Errors
    ///
    /// Returns the normal v3 lifecycle errors, plus a typed
    /// [`TaskStoreError::ResourceParticipantAuthority`] when a sealed
    /// Reservation is not (or cannot be proven) FINALIZED on the owner, or
    /// [`TaskStoreError::TaskWriteSetResourceReservationConflict`] when the
    /// owner aggregate disagrees with the sealed binding.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_resource_authority(
        &self,
        resource_authority: &nlos_resource::ResourceAuthority,
        request: FinalizeRequestV3,
    ) -> Result<ResourceFinalizeDecision, TaskStoreError> {
        let receipts = self.verified_resource_cost_receipts(resource_authority, &request)?;
        self.finalize_impl_with_resource_receipts(&request, None, &receipts)
    }

    /// Resource-owner revalidation plus terminalization for a permit bound
    /// to a durable authority lease. The owner readback and the Task CAS
    /// remain separate facts; the lease check runs inside the Task
    /// transaction.
    ///
    /// # Errors
    ///
    /// Returns the same errors as
    /// [`Self::finalize_commit_v3_with_resource_authority`], plus a typed
    /// lease-required, lease-fenced, or lease-expired error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_resource_authority_and_authority_lease(
        &self,
        resource_authority: &nlos_resource::ResourceAuthority,
        request: FinalizeRequestV3,
        authority_lease: AuthorityLeaseRecord,
    ) -> Result<ResourceFinalizeDecision, TaskStoreError> {
        let receipts = self.verified_resource_cost_receipts(resource_authority, &request)?;
        self.finalize_impl_with_resource_receipts(&request, Some(authority_lease), &receipts)
    }

    /// Reads the immutable nested Resource cost receipt set of one Task
    /// terminal receipt. A legacy receipt without nested rows decodes as an
    /// empty set.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-record error when the nested rows violate the
    /// high-water closure, conservation, or binding invariants, or a
    /// storage error.
    pub fn inspect_resource_cost_receipts(
        &self,
        task_id: TaskId,
        task_receipt_id: ReceiptId,
    ) -> Result<Vec<NestedResourceCostReceipt>, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_resource_cost_receipts(&*connection, task_id, task_receipt_id)
    }

    /// Derives the exact sealed Reservation set from the permit's write
    /// set and re-reads each FINALIZED owner aggregate before the Task
    /// transaction opens. Non-issued permits and legacy permits without a
    /// sealed write set return an empty set (replay inserts/reads no rows).
    fn verified_resource_cost_receipts(
        &self,
        resource_authority: &nlos_resource::ResourceAuthority,
        request: &FinalizeRequestV3,
    ) -> Result<Vec<NestedResourceCostReceipt>, TaskStoreError> {
        let permit = self.inspect_permit(request.base.task_id, request.base.permit_id)?;
        if permit.state != PermitState::Issued || permit.write_set_root == [0; 32] {
            return Ok(Vec::new());
        }
        let record = {
            let connection = self.lock_connection()?;
            crate::store::load_write_set_by_root(
                &*connection,
                request.base.task_id,
                permit.write_set_root,
            )?
        }
        .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
        if record.write_set_root != crate::model::task_write_set_root(&record) {
            return Err(TaskStoreError::CorruptRecord(
                "TaskWriteSet canonical root mismatch before Resource finalization",
            ));
        }
        verify_owner_cost_receipts(resource_authority, &record)
    }
}

/// Re-reads the FINALIZED owner aggregate for every sealed Reservation and
/// compares the binding identity with the sealed fields. Owner errors are
/// wrapped in [`TaskStoreError::ResourceParticipantAuthority`]; binding
/// drift fails closed with the reservation-conflict error. This
/// deliberately does not use the RESERVED-state permit-binding readback:
/// finalization requires FINALIZED owner state.
pub(crate) fn verify_owner_cost_receipts(
    resource_authority: &nlos_resource::ResourceAuthority,
    record: &TaskWriteSetRecord,
) -> Result<Vec<NestedResourceCostReceipt>, TaskStoreError> {
    let mut sealed = record.resource_reservations.clone();
    sealed.sort_unstable_by_key(|reservation| reservation.reservation_id);
    if sealed
        .windows(2)
        .any(|pair| pair[0].reservation_id == pair[1].reservation_id)
    {
        return Err(TaskStoreError::TaskWriteSetResourceReservationConflict);
    }
    let mut nested = Vec::with_capacity(sealed.len());
    for expected in &sealed {
        let owner = resource_authority
            .inspect_cost_receipt(expected.reservation_id)
            .map_err(TaskStoreError::ResourceParticipantAuthority)?;
        if owner.reservation_id != expected.reservation_id
            || owner.account_id != expected.account_id
            || owner.quote_id != expected.quote_id
            || owner.call_id != expected.call_id
            || owner.operation_id != expected.operation_id
            || owner.upper_bound != expected.upper_bound
        {
            return Err(TaskStoreError::TaskWriteSetResourceReservationConflict);
        }
        nested.push(NestedResourceCostReceipt::from_owner(owner));
    }
    Ok(nested)
}

/// Fail-closed comparison of a nested receipt set against the exact sealed
/// Reservation set. Used both before the terminal Task CAS (verified owner
/// aggregates) and during replay (nested Task rows).
pub(crate) fn validate_receipts_against_sealed_reservations(
    record: &TaskWriteSetRecord,
    receipts: &[NestedResourceCostReceipt],
) -> Result<(), TaskStoreError> {
    let mut sealed = record.resource_reservations.clone();
    sealed.sort_unstable_by_key(|reservation| reservation.reservation_id);
    if receipts.len() != sealed.len() {
        return Err(TaskStoreError::TaskWriteSetResourceReservationConflict);
    }
    for (receipt, expected) in receipts.iter().zip(sealed.iter()) {
        if !receipt_binds_sealed(receipt, expected) {
            return Err(TaskStoreError::TaskWriteSetResourceReservationConflict);
        }
    }
    Ok(())
}

fn receipt_binds_sealed(
    receipt: &NestedResourceCostReceipt,
    expected: &TaskWriteSetResourceReservation,
) -> bool {
    receipt.reservation_id == expected.reservation_id
        && receipt.account_id == expected.account_id
        && receipt.quote_id == expected.quote_id
        && receipt.call_id == expected.call_id
        && receipt.operation_id == expected.operation_id
        && receipt.upper_bound == expected.upper_bound
}

const PARENT_COLUMNS: &str = "reservation_id, account_id, quote_id, call_id, operation_id,
     upper_bound, activation_receipt_id, activated_at_ms, finalization_receipt_id,
     effect_closed_proof_digest, high_water_seq, final_seq, high_water, final_usage,
     refund_credit, finalized_at_ms";

pub(crate) fn insert_resource_cost_receipts(
    transaction: &Transaction<'_>,
    task_id: TaskId,
    task_receipt_id: ReceiptId,
    receipts: &[NestedResourceCostReceipt],
) -> Result<(), TaskStoreError> {
    for receipt in receipts {
        transaction.execute(
            "INSERT INTO task_resource_cost_receipts (
                task_receipt_id, task_id, reservation_id, account_id, quote_id,
                call_id, operation_id, upper_bound, activation_receipt_id,
                activated_at_ms, finalization_receipt_id, effect_closed_proof_digest,
                high_water_seq, final_seq, high_water, final_usage,
                refund_credit, finalized_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                task_receipt_id.as_bytes().as_slice(),
                task_id.as_bytes().as_slice(),
                receipt.reservation_id.as_bytes().as_slice(),
                receipt.account_id.as_bytes().as_slice(),
                receipt.quote_id.as_bytes().as_slice(),
                receipt.call_id.as_bytes().as_slice(),
                receipt.operation_id.as_bytes().as_slice(),
                encode_u64(receipt.upper_bound).as_slice(),
                receipt.activation.receipt_id.as_bytes().as_slice(),
                encode_u64(receipt.activation.activated_at_ms).as_slice(),
                receipt.finalization.receipt_id.as_bytes().as_slice(),
                receipt.finalization.effect_closed_proof_digest.as_slice(),
                encode_u64(receipt.finalization.high_water_seq).as_slice(),
                encode_u64(receipt.finalization.final_seq).as_slice(),
                encode_u64(receipt.finalization.high_water).as_slice(),
                encode_u64(receipt.finalization.final_usage).as_slice(),
                encode_u64(receipt.finalization.refund_credit).as_slice(),
                encode_u64(receipt.finalization.finalized_at_ms).as_slice(),
            ],
        )?;
        for consumption in &receipt.consumptions {
            transaction.execute(
                "INSERT INTO task_resource_cost_consumptions (
                    task_receipt_id, reservation_id, sequence, receipt_id,
                    cumulative_usage, consumed_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    task_receipt_id.as_bytes().as_slice(),
                    receipt.reservation_id.as_bytes().as_slice(),
                    encode_u64(consumption.sequence).as_slice(),
                    consumption.receipt_id.as_bytes().as_slice(),
                    encode_u64(consumption.cumulative_usage).as_slice(),
                    encode_u64(consumption.consumed_at_ms).as_slice(),
                ],
            )?;
        }
    }
    Ok(())
}

/// Loads the nested owner aggregate set of one Task terminal receipt.
/// Children are ordered by sequence and the parent row must close exactly:
/// the last child `(sequence, cumulative_usage)` equals the parent
/// `(high_water_seq, high_water)`, an empty child set implies `(0, 0)`,
/// cumulative usage never regresses, and
/// `upper_bound - final_usage == refund_credit` holds. Any violation is a
/// `CorruptRecord` fail-close.
pub(crate) fn load_resource_cost_receipts(
    source: &impl SqlRead,
    task_id: TaskId,
    task_receipt_id: ReceiptId,
) -> Result<Vec<NestedResourceCostReceipt>, TaskStoreError> {
    let mut parents = Vec::new();
    {
        let mut statement = source.prepare_statement(&format!(
            "SELECT {PARENT_COLUMNS} FROM task_resource_cost_receipts
             WHERE task_id = ?1 AND task_receipt_id = ?2 ORDER BY reservation_id"
        ))?;
        let mut rows = statement.query(params![
            task_id.as_bytes().as_slice(),
            task_receipt_id.as_bytes().as_slice(),
        ])?;
        while let Some(row) = rows.next()? {
            parents.push(decode_parent_row(row)?);
        }
    }
    let mut receipts = Vec::with_capacity(parents.len());
    for parent in parents {
        let mut consumptions = Vec::new();
        {
            let mut statement = source.prepare_statement(
                "SELECT sequence, receipt_id, cumulative_usage, consumed_at_ms
                 FROM task_resource_cost_consumptions
                 WHERE task_receipt_id = ?1 AND reservation_id = ?2
                 ORDER BY sequence",
            )?;
            let mut rows = statement.query(params![
                task_receipt_id.as_bytes().as_slice(),
                parent.reservation_id.as_bytes().as_slice(),
            ])?;
            while let Some(row) = rows.next()? {
                consumptions.push(nlos_resource::ConsumptionReceipt {
                    receipt_id: ReceiptId::from_bytes(blob16(row, 1)?),
                    reservation_id: parent.reservation_id,
                    operation_id: parent.operation_id,
                    activation_receipt_id: parent.activation.receipt_id,
                    sequence: u64_from_blob(row, 0)?,
                    cumulative_usage: u64_from_blob(row, 2)?,
                    consumed_at_ms: u64_from_blob(row, 3)?,
                });
            }
        }
        validate_parent_closure(&parent, &consumptions)?;
        receipts.push(NestedResourceCostReceipt {
            reservation_id: parent.reservation_id,
            account_id: parent.account_id,
            quote_id: parent.quote_id,
            call_id: parent.call_id,
            operation_id: parent.operation_id,
            upper_bound: parent.upper_bound,
            activation: parent.activation,
            consumptions,
            finalization: parent.finalization,
        });
    }
    Ok(receipts)
}

struct ParentRow {
    reservation_id: ReservationId,
    account_id: ResourceAccountId,
    quote_id: QuoteId,
    call_id: CallId,
    operation_id: OperationId,
    upper_bound: u64,
    activation: nlos_resource::ActivationReceipt,
    finalization: nlos_resource::FinalizationReceipt,
}

fn decode_parent_row(row: &Row<'_>) -> Result<ParentRow, TaskStoreError> {
    let reservation_id = ReservationId::from_bytes(blob16(row, 0)?);
    let operation_id = OperationId::from_bytes(blob16(row, 4)?);
    let activation_receipt_id = ReceiptId::from_bytes(blob16(row, 6)?);
    Ok(ParentRow {
        reservation_id,
        account_id: ResourceAccountId::from_bytes(blob16(row, 1)?),
        quote_id: QuoteId::from_bytes(blob16(row, 2)?),
        call_id: CallId::from_bytes(blob16(row, 3)?),
        operation_id,
        upper_bound: u64_from_blob(row, 5)?,
        activation: nlos_resource::ActivationReceipt {
            receipt_id: activation_receipt_id,
            reservation_id,
            operation_id,
            activated_at_ms: u64_from_blob(row, 7)?,
        },
        finalization: nlos_resource::FinalizationReceipt {
            receipt_id: ReceiptId::from_bytes(blob16(row, 8)?),
            reservation_id,
            operation_id,
            activation_receipt_id,
            effect_closed_proof_digest: blob32(row, 9)?,
            high_water_seq: u64_from_blob(row, 10)?,
            final_seq: u64_from_blob(row, 11)?,
            high_water: u64_from_blob(row, 12)?,
            final_usage: u64_from_blob(row, 13)?,
            refund_credit: u64_from_blob(row, 14)?,
            finalized_at_ms: u64_from_blob(row, 15)?,
        },
    })
}

fn validate_parent_closure(
    parent: &ParentRow,
    consumptions: &[nlos_resource::ConsumptionReceipt],
) -> Result<(), TaskStoreError> {
    if parent
        .upper_bound
        .checked_sub(parent.finalization.final_usage)
        != Some(parent.finalization.refund_credit)
    {
        return Err(TaskStoreError::CorruptRecord(
            "nested Resource receipt violates usage/refund conservation",
        ));
    }
    if parent.finalization.final_seq < parent.finalization.high_water_seq {
        return Err(TaskStoreError::CorruptRecord(
            "nested Resource final sequence regresses below its high-water",
        ));
    }
    let expected_high_water = consumptions
        .last()
        .map_or((0, 0), |last| (last.sequence, last.cumulative_usage));
    if expected_high_water
        != (
            parent.finalization.high_water_seq,
            parent.finalization.high_water,
        )
        || consumptions
            .windows(2)
            .any(|pair| pair[0].cumulative_usage > pair[1].cumulative_usage)
    {
        return Err(TaskStoreError::CorruptRecord(
            "nested Resource consumptions do not close the high-water",
        ));
    }
    Ok(())
}
