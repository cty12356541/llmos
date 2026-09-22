//! PINNED overlay on the residency axis (W36-P8; W31-G §8.2.6).
//!
//! A pin is a durable side-ledger, not a sixth `NodeResidencyTier`
//! discriminant: eviction (`to.discriminant() < from.discriminant()`)
//! of a pinned node is typed-refused; unpin is the degrade path that
//! restores the ordinary HOT→WARM→COLD walk. Full `[SCALE-PIN-001]`
//! bindings (ResourceAllocation, owner, reason, resident-bytes,
//! rebuild-cost, expiry, fence) are out of this slice.

use nlos_types::{IdempotencyKey, ReceiptId, TaskNodeId, TaskPlanId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::PlanStoreError;
use crate::model::{
    NodePinDecision, NodePinRequest, NodePinView, NodePinVoucher, PIN_VOUCHER_ID_DOMAIN,
};
use crate::store::{
    SqlitePlanAuthority, decode_u64, digest16, encode_u64, fixed16, load_plan_node,
};

impl SqlitePlanAuthority {
    /// Pins one node: subsequent evict-direction residency steps fail
    /// [`PlanStoreError::PinnedNodeNotEvictable`] until unpin.
    ///
    /// # Errors
    ///
    /// Fails typed on unknown nodes, stale revision CAS, already-pinned
    /// CAS, idempotency rebinding, or storage failure.
    #[allow(clippy::needless_pass_by_value)] // Owned request is the caller's exactly-once intent.
    pub fn record_node_pin(
        &self,
        request: NodePinRequest,
    ) -> Result<NodePinDecision, PlanStoreError> {
        self.record_pin_transition(request, true)
    }

    /// Unpins one node (the degrade half of the PINNED overlay).
    ///
    /// # Errors
    ///
    /// Same typed surfaces as [`Self::record_node_pin`].
    #[allow(clippy::needless_pass_by_value)]
    pub fn record_node_unpin(
        &self,
        request: NodePinRequest,
    ) -> Result<NodePinDecision, PlanStoreError> {
        self.record_pin_transition(request, false)
    }

    /// Typed readback of one node's PINNED overlay, `None` when the
    /// node does not exist.
    ///
    /// # Errors
    ///
    /// Fails on storage failure or a corrupt row.
    pub fn inspect_node_pin(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
    ) -> Result<Option<NodePinView>, PlanStoreError> {
        let connection = self.lock()?;
        if load_plan_node(&connection, plan_id, node_id)?.is_none() {
            return Ok(None);
        }
        let (pinned, count) = load_pin_state(&connection, plan_id, node_id)?;
        let last_voucher = load_pin_voucher_by_seq(&connection, plan_id, node_id, count)?;
        Ok(Some(NodePinView {
            plan_id,
            node_id,
            pinned,
            transition_count: count,
            last_voucher,
        }))
    }

    fn record_pin_transition(
        &self,
        request: NodePinRequest,
        to_pinned: bool,
    ) -> Result<NodePinDecision, PlanStoreError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = load_pin_voucher_by_key(&transaction, request.idempotency_key)? {
            if pin_voucher_matches_request(&existing, &request, to_pinned) {
                transaction.commit()?;
                return Ok(NodePinDecision::Replayed(existing));
            }
            return Err(PlanStoreError::IdempotencyConflict);
        }

        let node = load_plan_node(&transaction, request.plan_id, request.node_id)?.ok_or(
            PlanStoreError::NodeNotFound {
                plan_id: request.plan_id,
                node_id: request.node_id,
            },
        )?;
        if node.declared_revision != request.expected_declared_revision {
            return Err(PlanStoreError::StaleNodeRevision {
                node_id: request.node_id,
                expected: request.expected_declared_revision,
                current: node.declared_revision,
            });
        }
        if request.transitioned_at_ms < node.first_declared_at_ms {
            return Err(PlanStoreError::InvalidRequest {
                reason: "pin transition precedes the node's first declaration",
            });
        }
        let (current_pinned, current_count) =
            load_pin_state(&transaction, request.plan_id, request.node_id)?;
        if current_pinned == to_pinned {
            return Err(PlanStoreError::PinStateCasMismatch {
                node_id: request.node_id,
                expected_pinned: !to_pinned,
                current: current_pinned,
            });
        }

        let transition_seq = current_count + 1;
        let voucher_id = derive_pin_voucher_id(request.idempotency_key, request.node_id, to_pinned);
        transaction.execute(
            "INSERT INTO plan_node_pin_transitions (
                voucher_id, idempotency_key, plan_id, task_node_id, transition_seq,
                from_pinned, to_pinned, observed_revision, transitioned_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                voucher_id.as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice(),
                request.plan_id.as_bytes().as_slice(),
                request.node_id.as_bytes().as_slice(),
                encode_u64(transition_seq)?,
                i64::from(current_pinned),
                i64::from(to_pinned),
                encode_u64(node.declared_revision)?,
                encode_u64(request.transitioned_at_ms)?,
            ],
        )?;
        transaction.execute(
            "UPDATE plan_nodes
             SET pinned = ?3, pin_transition_count = ?4, updated_at_ms = ?5
             WHERE plan_id = ?1 AND task_node_id = ?2",
            params![
                request.plan_id.as_bytes().as_slice(),
                request.node_id.as_bytes().as_slice(),
                i64::from(to_pinned),
                encode_u64(transition_seq)?,
                encode_u64(request.transitioned_at_ms)?,
            ],
        )?;
        transaction.commit()?;

        Ok(NodePinDecision::Recorded(NodePinVoucher {
            voucher_id,
            plan_id: request.plan_id,
            node_id: request.node_id,
            transition_seq,
            from_pinned: current_pinned,
            to_pinned,
            observed_revision: node.declared_revision,
            idempotency_key: request.idempotency_key,
            transitioned_at_ms: request.transitioned_at_ms,
        }))
    }
}

pub(crate) fn load_pin_state(
    connection: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
) -> Result<(bool, u64), PlanStoreError> {
    let (pinned, count): (i64, i64) = connection.query_row(
        "SELECT pinned, pin_transition_count FROM plan_nodes
         WHERE plan_id = ?1 AND task_node_id = ?2",
        params![plan_id.as_bytes().as_slice(), node_id.as_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((pinned != 0, decode_u64(count)?))
}

fn derive_pin_voucher_id(key: IdempotencyKey, node_id: TaskNodeId, to_pinned: bool) -> ReceiptId {
    ReceiptId::from_bytes(digest16(
        PIN_VOUCHER_ID_DOMAIN,
        &[key.as_bytes(), node_id.as_bytes(), &[u8::from(to_pinned)]],
    ))
}

type PinVoucherRow = (Vec<u8>, Vec<u8>, Vec<u8>, i64, i64, i64, i64, Vec<u8>, i64);

fn raw_pin_voucher_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PinVoucherRow> {
    Ok((
        row.get::<_, Vec<u8>>(0)?,
        row.get::<_, Vec<u8>>(1)?,
        row.get::<_, Vec<u8>>(2)?,
        row.get::<_, i64>(3)?,
        row.get::<_, i64>(4)?,
        row.get::<_, i64>(5)?,
        row.get::<_, i64>(6)?,
        row.get::<_, Vec<u8>>(7)?,
        row.get::<_, i64>(8)?,
    ))
}

fn decode_pin_voucher_row(row: PinVoucherRow) -> Result<NodePinVoucher, PlanStoreError> {
    Ok(NodePinVoucher {
        voucher_id: ReceiptId::from_bytes(fixed16(row.0, "pin voucher id")?),
        plan_id: TaskPlanId::from_bytes(fixed16(row.1, "pin voucher plan id")?),
        node_id: TaskNodeId::from_bytes(fixed16(row.2, "pin voucher node id")?),
        transition_seq: decode_u64(row.3)?,
        from_pinned: row.4 != 0,
        to_pinned: row.5 != 0,
        observed_revision: decode_u64(row.6)?,
        idempotency_key: IdempotencyKey::from_bytes(fixed16(row.7, "pin voucher idempotency key")?),
        transitioned_at_ms: decode_u64(row.8)?,
    })
}

fn load_pin_voucher_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<NodePinVoucher>, PlanStoreError> {
    connection
        .query_row(
            "SELECT voucher_id, plan_id, task_node_id, transition_seq, from_pinned,
                    to_pinned, observed_revision, idempotency_key, transitioned_at_ms
             FROM plan_node_pin_transitions WHERE idempotency_key = ?1",
            [key.as_bytes().as_slice()],
            raw_pin_voucher_row,
        )
        .optional()?
        .map(decode_pin_voucher_row)
        .transpose()
}

fn load_pin_voucher_by_seq(
    connection: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    seq: u64,
) -> Result<Option<NodePinVoucher>, PlanStoreError> {
    if seq == 0 {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT voucher_id, plan_id, task_node_id, transition_seq, from_pinned,
                    to_pinned, observed_revision, idempotency_key, transitioned_at_ms
             FROM plan_node_pin_transitions
             WHERE plan_id = ?1 AND task_node_id = ?2 AND transition_seq = ?3",
            params![
                plan_id.as_bytes().as_slice(),
                node_id.as_bytes().as_slice(),
                encode_u64(seq)?,
            ],
            raw_pin_voucher_row,
        )
        .optional()?
        .map(decode_pin_voucher_row)
        .transpose()
}

fn pin_voucher_matches_request(
    voucher: &NodePinVoucher,
    request: &NodePinRequest,
    to_pinned: bool,
) -> bool {
    voucher.plan_id == request.plan_id
        && voucher.node_id == request.node_id
        && voucher.to_pinned == to_pinned
        && voucher.observed_revision == request.expected_declared_revision
        && voucher.transitioned_at_ms == request.transitioned_at_ms
}
