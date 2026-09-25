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
//! Semantic + Resource finalize rung below reuses exactly the two
//! single-authority validation precedents (Semantic owner-proof re-read +
//! READY publication plan, Resource FINALIZED aggregate re-read) and then
//! persists BOTH nested evidence sets in one terminal Task transaction.
//!
//! The prepare/finalize coordinator half (ADR-0017 decision R-C, schema
//! v43) persists the terminal request identity as an immutable envelope
//! plus a mutable plan state machine so a restart can converge the Task
//! side from durable bytes alone: the owner half (per-reservation
//! `finalize_reservation`) stays with the caller or a future
//! enforcement-gateway, and a plan whose owner reservations are not all
//! FINALIZED is *not due* — converge makes zero owner mutation and
//! records zero ledger failure, keeping the plan an inspectable durable
//! fact.

use std::fmt;

use nlos_types::{
    CallId, CommitPermitId, Generation, IdempotencyKey, OperationId, QuoteId, ReceiptId,
    ReservationId, ResourceAccountId, TaskAttemptId, TaskId,
};
use rusqlite::{Row, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::effect::list_slots;
use crate::reconcile::{FinalizeRequestV3, validate_semantic_finalization};
use crate::semantic_commit::{
    NestedSemanticPublicationReceipt, SemanticCommitPlanId, validate_finalize_satisfaction_shape,
};
use crate::store::{
    SqlRead, SqliteTaskAuthority, blob16, blob32, encode_u64, generation_from_blob, load_attempt,
    load_permit_by_id, load_task, load_write_set_by_root, optional_blob16, u64_from_blob,
};
use crate::{
    AuthorityLeaseRecord, FinalizeRequest, PermitState, RequiredSatisfaction, TaskReceiptRecord,
    TaskStoreError, TaskWriteSetRecord, TaskWriteSetResourceReservation,
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

/// Task terminal receipt plus BOTH nested evidence sets of a combined
/// Semantic + Resource finalize. Each element type is the exact nested
/// copy used by its single-authority variant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticResourceTaskCommitReceipt {
    pub task_receipt: TaskReceiptRecord,
    pub semantic_publications: Vec<NestedSemanticPublicationReceipt>,
    pub resource_cost_receipts: Vec<NestedResourceCostReceipt>,
}

/// Idempotent result of combined Semantic + Resource-aware Task
/// finalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticResourceFinalizeDecision {
    Committed(Box<SemanticResourceTaskCommitReceipt>),
    Replayed(Box<SemanticResourceTaskCommitReceipt>),
}

impl SemanticResourceFinalizeDecision {
    #[must_use]
    pub fn receipt(&self) -> &SemanticResourceTaskCommitReceipt {
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

    /// Finalizes an issued permit whose sealed `TaskWriteSet` carries BOTH
    /// `semantic_appends` and `resource_reservations`. Before the Task
    /// transaction opens, the Semantic side is re-read exactly as
    /// [`Self::finalize_commit_v3_with_semantic_publications`] does
    /// (owner proof re-read per sealed append) and the Resource side
    /// exactly as [`Self::finalize_commit_v3_with_resource_authority`]
    /// does (every sealed Reservation FINALIZED via `inspect_cost_receipt`
    /// with sealed-field comparison). The terminal Task transaction then
    /// persists the commit receipt, the nested Semantic publication rows
    /// (after the READY plan gate), the nested Resource cost rows, the
    /// permit close, and the head advance as ONE transaction.
    ///
    /// A closed permit follows the standard v3 replay path: both nested
    /// sets are loaded from the durable Task rows and neither owner
    /// authority is consulted again.
    ///
    /// # Errors
    ///
    /// Returns the normal v3 lifecycle errors, plus a typed
    /// [`TaskStoreError::SemanticParticipantAuthority`] /
    /// [`TaskStoreError::TaskWriteSetConflict`] when a sealed Semantic
    /// proof cannot be re-read exactly,
    /// [`TaskStoreError::ResourceParticipantAuthority`] when a sealed
    /// Reservation is not FINALIZED on the owner,
    /// [`TaskStoreError::TaskWriteSetResourceReservationConflict`] when the
    /// owner aggregate disagrees with the sealed binding, or
    /// [`TaskStoreError::SemanticCommitPlanNotReady`] when the publication
    /// plan has not reached READY.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_semantic_publications_and_resource_authority(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        resource_authority: &nlos_resource::ResourceAuthority,
        plan_id: SemanticCommitPlanId,
        request: FinalizeRequestV3,
    ) -> Result<SemanticResourceFinalizeDecision, TaskStoreError> {
        let receipts = self.verified_semantic_and_resource_receipts(
            semantic_authority,
            resource_authority,
            &request,
        )?;
        self.finalize_impl_with_semantic_and_resource_receipts(&request, None, plan_id, &receipts)
    }

    /// Combined Semantic + Resource revalidation plus terminalization for
    /// a permit bound to a durable authority lease. Both owner readbacks
    /// and the Task CAS remain separate facts; the lease check runs inside
    /// the Task transaction.
    ///
    /// # Errors
    ///
    /// Returns the same errors as
    /// [`Self::finalize_commit_v3_with_semantic_publications_and_resource_authority`],
    /// plus a typed lease-required, lease-fenced, or lease-expired error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_semantic_publications_and_resource_authority_and_authority_lease(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        resource_authority: &nlos_resource::ResourceAuthority,
        plan_id: SemanticCommitPlanId,
        request: FinalizeRequestV3,
        authority_lease: AuthorityLeaseRecord,
    ) -> Result<SemanticResourceFinalizeDecision, TaskStoreError> {
        let receipts = self.verified_semantic_and_resource_receipts(
            semantic_authority,
            resource_authority,
            &request,
        )?;
        self.finalize_impl_with_semantic_and_resource_receipts(
            &request,
            Some(authority_lease),
            plan_id,
            &receipts,
        )
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

    /// Combined pre-transaction validation: loads the sealed write set of
    /// an issued permit once, re-reads the Semantic owner proofs exactly as
    /// the Semantic publications variant does, then re-reads every
    /// FINALIZED Resource aggregate exactly as the Resource variant does.
    /// Non-issued permits and legacy permits without a sealed write set
    /// return an empty receipt set (replay inserts/reads no rows).
    fn verified_semantic_and_resource_receipts(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
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
                "TaskWriteSet canonical root mismatch before combined Semantic+Resource finalization",
            ));
        }
        validate_semantic_finalization(semantic_authority, &record)?;
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

// ---------------------------------------------------------------------------
// prepare/finalize coordinator (ADR-0017 decision R-C, schema v43)
// ---------------------------------------------------------------------------

/// Durable state of a Task-side Resource finalize plan. `Planned` is the
/// only pre-terminal state: the owner-side settle steps belong to the
/// caller/gateway, and the single Task-side terminal step (the
/// resource-aware v3 finalize transaction) flips the plan to `Finalized`
/// inside that same transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceCommitPlanState {
    Planned,
    Finalized,
}

impl ResourceCommitPlanState {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Planned => 0,
            Self::Finalized => 1,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Planned),
            1 => Ok(Self::Finalized),
            _ => Err(TaskStoreError::CorruptRecord(
                "unknown resource commit plan state",
            )),
        }
    }
}

/// Authority-derived identity of one Resource finalize plan.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourceCommitPlanId([u8; 16]);

impl ResourceCommitPlanId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for ResourceCommitPlanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ResourceCommitPlanId(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// Request to durably bind the Resource finalize envelope to one issued
/// permit. The Reservation set is derived from the sealed `TaskWriteSet`;
/// caller-supplied cost facts are never accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareResourceFinalizeRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub permit_id: CommitPermitId,
    pub idempotency_key: IdempotencyKey,
    pub required_satisfaction: Vec<RequiredSatisfaction>,
    pub fenced_participant_digest: [u8; 32],
    pub prepared_at_ms: i64,
}

/// Immutable durable Resource finalize envelope bound to one plan. These
/// are exactly the `FinalizeRequestV3` bytes that cannot be re-derived
/// from the plan/permit identity; exact retries replay byte-for-byte.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceFinalizeEnvelopeRecord {
    pub plan_id: ResourceCommitPlanId,
    pub required_satisfaction: Vec<RequiredSatisfaction>,
    pub fenced_participant_digest: [u8; 32],
    pub prepared_at_ms: i64,
}

/// Idempotent result of preparing the Resource finalize envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceFinalizeEnvelopeDecision {
    Prepared(Box<ResourceFinalizeEnvelopeRecord>),
    Replayed(Box<ResourceFinalizeEnvelopeRecord>),
}

impl ResourceFinalizeEnvelopeDecision {
    #[must_use]
    pub fn record(&self) -> &ResourceFinalizeEnvelopeRecord {
        match self {
            Self::Prepared(record) | Self::Replayed(record) => record,
        }
    }
}

/// Durable Resource finalize plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceCommitPlanRecord {
    pub plan_id: ResourceCommitPlanId,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub write_set_root: [u8; 32],
    pub resource_reservation_set_root: [u8; 32],
    pub expected_reservation_count: u64,
    pub state: ResourceCommitPlanState,
    pub task_receipt_id: Option<ReceiptId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Bounded decision of one coordinator converge step. `NotDue` is the
/// honest boundary of decision R-C: the owner is not settled, so nothing
/// is mutated and nothing is recorded — the plan remains an inspectable
/// durable fact for the operations surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceConvergeDecision {
    Finalized(Box<ResourceTaskCommitReceipt>),
    Replayed(Box<ResourceTaskCommitReceipt>),
    NotDue(Box<ResourceCommitPlanRecord>),
}

enum OwnerSettlement {
    Settled,
    NotSettled,
}

impl SqliteTaskAuthority {
    /// Persists the Resource finalize envelope (terminal request identity)
    /// and the plan state machine for one issued permit in a single
    /// transaction. The Reservation set is derived from the permit's
    /// sealed `TaskWriteSet`; the envelope is immutable and exact retries
    /// replay its bytes. Owner-side settlement is NOT driven here.
    ///
    /// # Errors
    ///
    /// Returns a typed not-found/holder/stale-head error, a typed
    /// invalid-plan error for write sets without Reservations or with
    /// Semantic appends (the combined rung stays on its direct API), or a
    /// storage error. No partial plan/envelope row is committed on error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn prepare_resource_finalize(
        &self,
        request: PrepareResourceFinalizeRequest,
    ) -> Result<ResourceFinalizeEnvelopeDecision, TaskStoreError> {
        if request.prepared_at_ms < 0 {
            return Err(TaskStoreError::InvalidResourcePlan {
                reason: "resource finalize envelope timestamp must be non-negative",
            });
        }
        let plan_id = derive_resource_plan_id(request.permit_id);
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing_plan) = load_resource_plan_optional(&transaction, plan_id)? {
            let envelope = load_finalize_envelope_optional(&transaction, plan_id)?.ok_or(
                TaskStoreError::CorruptRecord("resource commit plan lacks its finalize envelope"),
            )?;
            let same_request = existing_plan.task_id == request.task_id
                && existing_plan.permit_id == request.permit_id
                && existing_plan.attempt_id == request.attempt_id
                && existing_plan.attempt_generation == request.attempt_generation
                && envelope.required_satisfaction == request.required_satisfaction
                && envelope.fenced_participant_digest == request.fenced_participant_digest
                && envelope.prepared_at_ms == request.prepared_at_ms;
            if !same_request {
                return Err(TaskStoreError::InvalidResourcePlan {
                    reason: "resource finalize envelope request conflicts with durable bytes",
                });
            }
            transaction.commit()?;
            return Ok(ResourceFinalizeEnvelopeDecision::Replayed(Box::new(
                envelope,
            )));
        }
        let envelope = prepare_new_resource_finalize(&transaction, &request, plan_id)?;
        transaction.commit()?;
        Ok(ResourceFinalizeEnvelopeDecision::Prepared(Box::new(
            envelope,
        )))
    }

    /// Reads one durable Resource finalize plan.
    ///
    /// # Errors
    ///
    /// Returns [`TaskStoreError::ResourceCommitPlanNotFound`] or a
    /// corrupt-record/storage error.
    pub fn inspect_resource_commit_plan(
        &self,
        plan_id: ResourceCommitPlanId,
    ) -> Result<ResourceCommitPlanRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_resource_plan_optional(&*connection, plan_id)?
            .ok_or(TaskStoreError::ResourceCommitPlanNotFound)
    }

    /// Reads the immutable Resource finalize envelope, if one was prepared
    /// for the plan.
    ///
    /// # Errors
    ///
    /// Returns a storage or corrupt-record error.
    pub fn inspect_resource_finalize_envelope(
        &self,
        plan_id: ResourceCommitPlanId,
    ) -> Result<Option<ResourceFinalizeEnvelopeRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_finalize_envelope_optional(&*connection, plan_id)
    }

    /// Lists non-finalized Resource finalize plans in stable
    /// identity order for a restart coordinator scan.
    ///
    /// # Errors
    ///
    /// Returns a corrupt-record or storage error.
    pub fn list_incomplete_resource_commit_plans(
        &self,
        limit: usize,
    ) -> Result<Vec<ResourceCommitPlanRecord>, TaskStoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT plan_id FROM task_resource_commit_plans
             WHERE plan_state != ?1 ORDER BY created_at_ms, plan_id LIMIT ?2",
        )?;
        let mut rows = statement.query(params![
            ResourceCommitPlanState::Finalized.code(),
            i64::try_from(limit).unwrap_or(i64::MAX),
        ])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(ResourceCommitPlanId::from_bytes(blob16(row, 0)?));
        }
        drop(rows);
        drop(statement);
        ids.into_iter()
            .map(|plan_id| {
                load_resource_plan_optional(&*connection, plan_id)?
                    .ok_or(TaskStoreError::ResourceCommitPlanNotFound)
            })
            .collect()
    }

    /// Converges one incomplete Resource finalize plan from durable bytes
    /// alone (ADR-0017 decision R-C): when every sealed Reservation is
    /// FINALIZED on the owner, the Task side converges through the
    /// existing resource-aware v3 single-transaction path (re-reading the
    /// FINALIZED owner aggregate before the transaction, then flipping
    /// the plan inside it); when any Reservation is not settled — or is
    /// unknown to this owner — the plan is *not due* and this call makes
    /// zero owner mutation and zero ledger write. A finalized plan
    /// replays from the durable Task rows only.
    ///
    /// # Errors
    ///
    /// Returns a typed plan/envelope error, the resource-aware v3
    /// lifecycle errors, [`TaskStoreError::ResourceParticipantAuthority`]
    /// when the owner read itself fails (an infrastructure failure the
    /// caller may ledger), or [`TaskStoreError::ResourceConvergeProofDiverged`]
    /// when the permit was already terminalized out-of-band with finalize
    /// bytes that differ from the sealed envelope (a plan-level failure
    /// the recovery worker ledgers with backoff). The not-due outcome is
    /// a decision, not an error.
    pub fn converge_resource_commit_plan(
        &self,
        resource_authority: &nlos_resource::ResourceAuthority,
        plan_id: ResourceCommitPlanId,
        now_ms: i64,
    ) -> Result<ResourceConvergeDecision, TaskStoreError> {
        if now_ms < 0 {
            return Err(TaskStoreError::InvalidResourceRecoveryPolicy {
                reason: "converge timestamp must be non-negative",
            });
        }
        let plan = self.inspect_resource_commit_plan(plan_id)?;
        let envelope = self.inspect_resource_finalize_envelope(plan_id)?.ok_or(
            TaskStoreError::CorruptRecord("resource commit plan lacks its finalize envelope"),
        )?;
        let request = FinalizeRequestV3 {
            base: FinalizeRequest {
                task_id: plan.task_id,
                attempt_id: plan.attempt_id,
                attempt_generation: plan.attempt_generation,
                permit_id: plan.permit_id,
                new_effect_history_root: [0; 32],
                new_retry_fence_epoch: 0,
                finalized_at_ms: now_ms,
            },
            required_satisfaction: envelope.required_satisfaction,
            fenced_participant_digest: envelope.fenced_participant_digest,
        };
        if plan.state == ResourceCommitPlanState::Finalized {
            return match self.finalize_impl_with_resource_plan(&request, plan_id, &[])? {
                ResourceFinalizeDecision::Replayed(receipt) => {
                    Ok(ResourceConvergeDecision::Replayed(receipt))
                }
                ResourceFinalizeDecision::Committed(_) => Err(TaskStoreError::CorruptRecord(
                    "finalized Resource plan converged through a fresh commit",
                )),
            };
        }
        let write_set = {
            let connection = self.lock_connection()?;
            let permit = load_permit_by_id(&*connection, plan.task_id, plan.permit_id)?;
            if crate::reconcile::resource_converge_replay_diverged(&*connection, &permit, &request)?
            {
                // Converge liveness: the direct resource-aware v3 API
                // terminalized this permit with satisfaction bytes that
                // differ from the sealed envelope, so envelope replay can
                // never succeed. Report the durable divergence as a typed
                // plan-level failure for upper-layer adjudication instead
                // of looping on `HistoryConflict`; the recovery worker
                // ledgers it with backoff.
                return Err(TaskStoreError::ResourceConvergeProofDiverged {
                    plan_id: plan.plan_id,
                });
            }
            let record = load_write_set_by_root(&*connection, plan.task_id, permit.write_set_root)?
                .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
            if record.write_set_root != crate::model::task_write_set_root(&record) {
                return Err(TaskStoreError::CorruptRecord(
                    "TaskWriteSet canonical root mismatch before Resource converge",
                ));
            }
            record
        };
        validate_resource_plan_against_write_set(&plan, &write_set)?;
        match owner_settlement(resource_authority, &write_set)? {
            OwnerSettlement::NotSettled => Ok(ResourceConvergeDecision::NotDue(Box::new(plan))),
            OwnerSettlement::Settled => {
                let receipts = verify_owner_cost_receipts(resource_authority, &write_set)?;
                match self.finalize_impl_with_resource_plan(&request, plan_id, &receipts)? {
                    ResourceFinalizeDecision::Committed(receipt) => {
                        Ok(ResourceConvergeDecision::Finalized(receipt))
                    }
                    ResourceFinalizeDecision::Replayed(receipt) => {
                        Ok(ResourceConvergeDecision::Replayed(receipt))
                    }
                }
            }
        }
    }
}

/// Validates the permit/attempt/head/write-set context of a fresh prepare
/// and inserts the plan row plus the immutable envelope (and satisfaction
/// rows) inside the caller's `Immediate` transaction; the caller commits.
fn prepare_new_resource_finalize(
    transaction: &Transaction<'_>,
    request: &PrepareResourceFinalizeRequest,
    plan_id: ResourceCommitPlanId,
) -> Result<ResourceFinalizeEnvelopeRecord, TaskStoreError> {
    let permit = load_permit_by_id(transaction, request.task_id, request.permit_id)?;
    if permit.state != PermitState::Issued {
        return Err(TaskStoreError::PermitNotIssued);
    }
    let attempt = load_attempt(transaction, request.task_id, request.attempt_id)?;
    if attempt.attempt_generation != request.attempt_generation {
        return Err(TaskStoreError::InvalidGeneration);
    }
    if permit.attempt_id != request.attempt_id
        || permit.attempt_generation != request.attempt_generation
    {
        return Err(TaskStoreError::NotPermitHolder);
    }
    let task = load_task(transaction, request.task_id)?;
    if task.record.head_commit_seq != permit.expected_head_commit_seq
        || task.record.head_effect_history_root != permit.expected_effect_history_root
        || task.record.retry_fence_epoch != permit.expected_retry_fence_epoch
    {
        return Err(TaskStoreError::StaleTaskHead);
    }
    let write_set = load_write_set_by_root(transaction, request.task_id, permit.write_set_root)?
        .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
    if write_set.write_set_root != crate::model::task_write_set_root(&write_set) {
        return Err(TaskStoreError::CorruptRecord(
            "TaskWriteSet canonical root mismatch before Resource finalize preparation",
        ));
    }
    if !write_set.semantic_appends.is_empty() {
        return Err(TaskStoreError::InvalidResourcePlan {
            reason: "resource finalize envelope requires a write set without Semantic appends",
        });
    }
    let mut sealed = write_set.resource_reservations.clone();
    sealed.sort_unstable_by_key(|reservation| reservation.reservation_id);
    if sealed
        .windows(2)
        .any(|pair| pair[0].reservation_id == pair[1].reservation_id)
    {
        return Err(TaskStoreError::TaskWriteSetResourceReservationConflict);
    }
    if sealed.is_empty() {
        return Err(TaskStoreError::InvalidResourcePlan {
            reason: "sealed TaskWriteSet has no Resource reservations",
        });
    }
    let slots = list_slots(transaction, permit.permit_id)?;
    if slots.is_empty() {
        if !request.required_satisfaction.is_empty() {
            return Err(TaskStoreError::InvalidResourcePlan {
                reason: "satisfaction proofs require declared Effect slots",
            });
        }
    } else {
        validate_finalize_satisfaction_shape(&slots, &request.required_satisfaction)?;
    }
    let expected_reservation_count = u64::try_from(sealed.len())
        .map_err(|_| TaskStoreError::CorruptRecord("Resource reservation count exceeds u64"))?;
    let plan = ResourceCommitPlanRecord {
        plan_id,
        task_id: request.task_id,
        permit_id: request.permit_id,
        attempt_id: request.attempt_id,
        attempt_generation: request.attempt_generation,
        write_set_root: write_set.write_set_root,
        resource_reservation_set_root: write_set.resource_reservation_set_root,
        expected_reservation_count,
        state: ResourceCommitPlanState::Planned,
        task_receipt_id: None,
        created_at_ms: request.prepared_at_ms,
        updated_at_ms: request.prepared_at_ms,
    };
    insert_resource_plan(transaction, &plan, request.idempotency_key)?;
    let envelope = ResourceFinalizeEnvelopeRecord {
        plan_id,
        required_satisfaction: request.required_satisfaction.clone(),
        fenced_participant_digest: request.fenced_participant_digest,
        prepared_at_ms: request.prepared_at_ms,
    };
    insert_finalize_envelope(transaction, &envelope)?;
    Ok(envelope)
}

fn owner_settlement(
    resource_authority: &nlos_resource::ResourceAuthority,
    record: &TaskWriteSetRecord,
) -> Result<OwnerSettlement, TaskStoreError> {
    for expected in &record.resource_reservations {
        match resource_authority.inspect_reservation(expected.reservation_id) {
            Ok(owner) => {
                if owner.state != nlos_resource::ReservationState::Finalized {
                    return Ok(OwnerSettlement::NotSettled);
                }
            }
            Err(nlos_resource::ResourceAuthorityError::ReservationNotFound) => {
                return Ok(OwnerSettlement::NotSettled);
            }
            Err(other) => return Err(TaskStoreError::ResourceParticipantAuthority(other)),
        }
    }
    Ok(OwnerSettlement::Settled)
}

fn validate_resource_plan_against_write_set(
    plan: &ResourceCommitPlanRecord,
    record: &TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    let expected_count = u64::try_from(record.resource_reservations.len())
        .map_err(|_| TaskStoreError::CorruptRecord("Resource reservation count exceeds u64"))?;
    if plan.write_set_root != record.write_set_root
        || plan.resource_reservation_set_root != record.resource_reservation_set_root
        || plan.expected_reservation_count != expected_count
    {
        return Err(TaskStoreError::CorruptRecord(
            "Resource commit plan disagrees with sealed TaskWriteSet",
        ));
    }
    Ok(())
}

fn derive_resource_plan_id(permit_id: CommitPermitId) -> ResourceCommitPlanId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-resource-commit-plan/v1");
    hasher.update(permit_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ResourceCommitPlanId::from_bytes(bytes)
}

const RESOURCE_PLAN_COLUMNS: &str = "plan_id, task_id, permit_id, attempt_id,
     attempt_generation, write_set_root, resource_reservation_set_root,
     expected_reservation_count, plan_state, task_receipt_id, created_at_ms, updated_at_ms";

fn insert_resource_plan(
    transaction: &Transaction<'_>,
    record: &ResourceCommitPlanRecord,
    idempotency_key: IdempotencyKey,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_resource_commit_plans (
            plan_id, task_id, permit_id, idempotency_key, attempt_id,
            attempt_generation, write_set_root, resource_reservation_set_root,
            expected_reservation_count, plan_state, task_receipt_id,
            created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?11)",
        params![
            record.plan_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record.permit_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            record.attempt_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            record.write_set_root.as_slice(),
            record.resource_reservation_set_root.as_slice(),
            encode_u64(record.expected_reservation_count).as_slice(),
            record.state.code(),
            record.created_at_ms,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_resource_plan_optional(
    source: &impl SqlRead,
    plan_id: ResourceCommitPlanId,
) -> Result<Option<ResourceCommitPlanRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {RESOURCE_PLAN_COLUMNS} FROM task_resource_commit_plans WHERE plan_id = ?1"
    ))?;
    let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_resource_plan_row).transpose()
}

fn decode_resource_plan_row(row: &Row<'_>) -> Result<ResourceCommitPlanRecord, TaskStoreError> {
    Ok(ResourceCommitPlanRecord {
        plan_id: ResourceCommitPlanId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 2)?),
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 3)?),
        attempt_generation: generation_from_blob(row, 4)?,
        write_set_root: blob32(row, 5)?,
        resource_reservation_set_root: blob32(row, 6)?,
        expected_reservation_count: u64_from_blob(row, 7)?,
        state: ResourceCommitPlanState::from_code(row.get(8)?)?,
        task_receipt_id: optional_blob16(row, 9)?.map(ReceiptId::from_bytes),
        created_at_ms: row.get(10)?,
        updated_at_ms: row.get(11)?,
    })
}

/// Flips a `Planned` Resource finalize plan to `Finalized` bound to one
/// Task receipt inside the caller's terminal transaction, or verifies an
/// already-finalized plan binds exactly that receipt (idempotent replay).
pub(crate) fn bind_resource_plan_receipt(
    transaction: &Transaction<'_>,
    plan_id: ResourceCommitPlanId,
    receipt_id: ReceiptId,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let plan = load_resource_plan_optional(transaction, plan_id)?
        .ok_or(TaskStoreError::ResourceCommitPlanNotFound)?;
    match plan.state {
        ResourceCommitPlanState::Finalized => {
            if plan.task_receipt_id != Some(receipt_id) {
                return Err(TaskStoreError::CorruptRecord(
                    "finalized Resource plan binds a different Task receipt",
                ));
            }
            Ok(())
        }
        ResourceCommitPlanState::Planned => {
            let changed = transaction.execute(
                "UPDATE task_resource_commit_plans
                 SET plan_state = ?1, task_receipt_id = ?2, updated_at_ms = ?3
                 WHERE plan_id = ?4 AND plan_state = ?5 AND task_receipt_id IS NULL",
                params![
                    ResourceCommitPlanState::Finalized.code(),
                    receipt_id.as_bytes().as_slice(),
                    now_ms,
                    plan_id.as_bytes().as_slice(),
                    ResourceCommitPlanState::Planned.code(),
                ],
            )?;
            if changed != 1 {
                return Err(TaskStoreError::CorruptRecord(
                    "resource commit plan finalize compare-and-swap failed",
                ));
            }
            Ok(())
        }
    }
}

fn insert_finalize_envelope(
    transaction: &Transaction<'_>,
    envelope: &ResourceFinalizeEnvelopeRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_resource_finalize_envelopes (
            plan_id, fenced_participant_digest, prepared_at_ms
         ) VALUES (?1, ?2, ?3)",
        params![
            envelope.plan_id.as_bytes().as_slice(),
            envelope.fenced_participant_digest.as_slice(),
            envelope.prepared_at_ms,
        ],
    )?;
    for satisfaction in &envelope.required_satisfaction {
        let (proof_kind, proof_digest) = match satisfaction.proof {
            crate::RequiredSatisfactionProof::EffectClosedSuccess {
                success_assertion_digest,
            } => (0_i64, success_assertion_digest),
            crate::RequiredSatisfactionProof::ConditionNotApplicable {
                condition_false_proof_digest,
            } => (1_i64, condition_false_proof_digest),
        };
        transaction.execute(
            "INSERT INTO task_resource_finalize_satisfactions (
                plan_id, effect_seq, proof_kind, proof_digest
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                envelope.plan_id.as_bytes().as_slice(),
                encode_u64(satisfaction.effect_seq).as_slice(),
                proof_kind,
                proof_digest.as_slice(),
            ],
        )?;
    }
    Ok(())
}

fn load_finalize_envelope_optional(
    source: &impl SqlRead,
    plan_id: ResourceCommitPlanId,
) -> Result<Option<ResourceFinalizeEnvelopeRecord>, TaskStoreError> {
    let (fenced_participant_digest, prepared_at_ms) = {
        let mut statement = source.prepare_statement(
            "SELECT fenced_participant_digest, prepared_at_ms
             FROM task_resource_finalize_envelopes WHERE plan_id = ?1",
        )?;
        let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        (blob32(row, 0)?, row.get::<_, i64>(1)?)
    };
    let mut statement = source.prepare_statement(
        "SELECT effect_seq, proof_kind, proof_digest
         FROM task_resource_finalize_satisfactions
         WHERE plan_id = ?1 ORDER BY effect_seq",
    )?;
    let mut rows = statement.query([plan_id.as_bytes().as_slice()])?;
    let mut required_satisfaction = Vec::new();
    while let Some(row) = rows.next()? {
        let effect_seq = u64_from_blob(row, 0)?;
        let proof_digest = blob32(row, 2)?;
        let proof = match row.get::<_, i64>(1)? {
            0 => crate::RequiredSatisfactionProof::EffectClosedSuccess {
                success_assertion_digest: proof_digest,
            },
            1 => crate::RequiredSatisfactionProof::ConditionNotApplicable {
                condition_false_proof_digest: proof_digest,
            },
            _ => {
                return Err(TaskStoreError::CorruptRecord(
                    "unknown resource finalize proof kind",
                ));
            }
        };
        required_satisfaction.push(RequiredSatisfaction { effect_seq, proof });
    }
    Ok(Some(ResourceFinalizeEnvelopeRecord {
        plan_id,
        required_satisfaction,
        fenced_participant_digest,
        prepared_at_ms,
    }))
}
