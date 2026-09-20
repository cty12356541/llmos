//! Context residency tiering for plan nodes (W31-E, B4-5; v0.5 §25.2.1
//! `ResidencyClass`, 议题 28 定案 2).
//!
//! The residency axis records where a node's working set lives
//! (`METADATA_ONLY → COLD → WARM → HOT → RUNNING`, 议题 28 linear chain).
//! It is a **separate axis** from the §25.2.1 execution state machine in
//! [`crate::model::PlanNodeState`]: its own immutable voucher table with
//! its own dense per-node sequence and idempotency keys, its own tier
//! CAS, and no coupling guard in either direction. Eviction
//! (`HOT→WARM→COLD`, the §25.2.1 `EVICTED (residency=WARM|COLD)` posture)
//! walks the chain stepwise, so every tier step is one auditable typed
//! voucher; the declared-revision CAS reuses the lifecycle face's
//! `[PLAN-DAG-001]` fence so a reshaping revision fences in-flight
//! residency moves that observed the pre-reshape revision.
//!
//! Evicted nodes keep their bounded `METADATA` facts (the G2 posture,
//! `[SCALE-LOGICAL-001]`): residency columns are not shape, so the G1
//! executed-shape-freeze trigger never blocks a tier move and the node
//! row's identity/shape/state facts survive eviction untouched.

use nlos_types::{IdempotencyKey, ReceiptId, TaskNodeId, TaskPlanId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::PlanStoreError;
use crate::model::{
    NodeResidencyTier, NodeResidencyView, RESIDENCY_VOUCHER_ID_DOMAIN, ResidencyTransitionDecision,
    ResidencyTransitionRequest, ResidencyTransitionVoucher, decode_tier, encode_tier,
};
use crate::store::{
    SqlitePlanAuthority, decode_u64, digest16, encode_u64, fixed16, load_plan_node,
};

impl SqlitePlanAuthority {
    /// Records one residency tier transition: validates the conservative
    /// adjacent-step edge set, the declared-revision CAS, and the tier
    /// CAS, then commits the immutable voucher and the tier advance in
    /// one transaction.
    ///
    /// # Errors
    ///
    /// Fails typed on illegal tier edges
    /// ([`PlanStoreError::IllegalResidencyTransition`]), stale
    /// revision/tier CAS, idempotency rebinding, unknown nodes, or
    /// storage failure.
    #[allow(clippy::needless_pass_by_value)] // The owned request is the caller's exactly-once intent (house API shape).
    pub fn record_residency_transition(
        &self,
        request: ResidencyTransitionRequest,
    ) -> Result<ResidencyTransitionDecision, PlanStoreError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Idempotent replay first: the durable voucher is the authority.
        if let Some(existing) =
            load_residency_voucher_by_key(&transaction, request.idempotency_key)?
        {
            if residency_voucher_matches_request(&existing, &request) {
                transaction.commit()?;
                return Ok(ResidencyTransitionDecision::Replayed(existing));
            }
            return Err(PlanStoreError::IdempotencyConflict);
        }

        let node = load_plan_node(&transaction, request.plan_id, request.node_id)?.ok_or(
            PlanStoreError::NodeNotFound {
                plan_id: request.plan_id,
                node_id: request.node_id,
            },
        )?;
        if !NodeResidencyTier::tier_transition_is_legal(request.from_tier, request.to_tier) {
            return Err(PlanStoreError::IllegalResidencyTransition {
                node_id: request.node_id,
                from: request.from_tier,
                to: request.to_tier,
            });
        }
        if node.declared_revision != request.expected_declared_revision {
            return Err(PlanStoreError::StaleNodeRevision {
                node_id: request.node_id,
                expected: request.expected_declared_revision,
                current: node.declared_revision,
            });
        }
        if node.residency_tier != request.from_tier {
            return Err(PlanStoreError::ResidencyTierCasMismatch {
                node_id: request.node_id,
                expected_from: request.from_tier,
                current: node.residency_tier,
            });
        }
        if request.transitioned_at_ms < node.first_declared_at_ms {
            return Err(PlanStoreError::InvalidRequest {
                reason: "residency transition precedes the node's first declaration",
            });
        }

        let transition_seq = node.residency_transition_count + 1;
        let voucher_id =
            derive_residency_voucher_id(request.idempotency_key, request.node_id, request.to_tier);
        transaction.execute(
            "INSERT INTO plan_node_residency_transitions (
                voucher_id, idempotency_key, plan_id, task_node_id, transition_seq,
                from_tier, to_tier, observed_revision, transitioned_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                voucher_id.as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice(),
                request.plan_id.as_bytes().as_slice(),
                request.node_id.as_bytes().as_slice(),
                encode_u64(transition_seq)?,
                encode_tier(request.from_tier),
                encode_tier(request.to_tier),
                encode_u64(node.declared_revision)?,
                encode_u64(request.transitioned_at_ms)?,
            ],
        )?;
        transaction.execute(
            "UPDATE plan_nodes
             SET residency_tier = ?3, residency_transition_count = ?4, updated_at_ms = ?5
             WHERE plan_id = ?1 AND task_node_id = ?2",
            params![
                request.plan_id.as_bytes().as_slice(),
                request.node_id.as_bytes().as_slice(),
                encode_tier(request.to_tier),
                encode_u64(transition_seq)?,
                encode_u64(request.transitioned_at_ms)?,
            ],
        )?;
        transaction.commit()?;

        Ok(ResidencyTransitionDecision::Recorded(
            ResidencyTransitionVoucher {
                voucher_id,
                plan_id: request.plan_id,
                node_id: request.node_id,
                transition_seq,
                from_tier: request.from_tier,
                to_tier: request.to_tier,
                observed_revision: node.declared_revision,
                idempotency_key: request.idempotency_key,
                transitioned_at_ms: request.transitioned_at_ms,
            },
        ))
    }

    /// Typed graded readback of one node's residency axis: the current
    /// tier, the transition count, and the last transition voucher,
    /// `None` when the node does not exist.
    ///
    /// # Errors
    ///
    /// Fails on storage failure or a corrupt row.
    pub fn inspect_node_residency(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
    ) -> Result<Option<NodeResidencyView>, PlanStoreError> {
        let connection = self.lock()?;
        let Some(node) = load_plan_node(&connection, plan_id, node_id)? else {
            return Ok(None);
        };
        let last_voucher = load_residency_voucher_by_seq(
            &connection,
            plan_id,
            node_id,
            node.residency_transition_count,
        )?;
        Ok(Some(NodeResidencyView {
            plan_id,
            node_id,
            tier: node.residency_tier,
            transition_count: node.residency_transition_count,
            last_voucher,
        }))
    }

    /// Lists one node's residency transition vouchers in dense sequence
    /// order.
    ///
    /// # Errors
    ///
    /// Fails typed when the node does not exist, or on storage failure.
    pub fn inspect_node_residency_vouchers(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
    ) -> Result<Vec<ResidencyTransitionVoucher>, PlanStoreError> {
        let connection = self.lock()?;
        if load_plan_node(&connection, plan_id, node_id)?.is_none() {
            return Err(PlanStoreError::NodeNotFound { plan_id, node_id });
        }
        let mut statement = connection.prepare(
            "SELECT voucher_id, plan_id, task_node_id, transition_seq, from_tier,
                    to_tier, observed_revision, idempotency_key, transitioned_at_ms
             FROM plan_node_residency_transitions
             WHERE plan_id = ?1 AND task_node_id = ?2
             ORDER BY transition_seq",
        )?;
        let rows = statement.query_map(
            params![plan_id.as_bytes().as_slice(), node_id.as_bytes().as_slice()],
            raw_residency_voucher_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(PlanStoreError::from)?
            .into_iter()
            .map(decode_residency_voucher_row)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// residency voucher rows (pure helpers)
// ---------------------------------------------------------------------------

fn derive_residency_voucher_id(
    key: IdempotencyKey,
    node_id: TaskNodeId,
    to_tier: NodeResidencyTier,
) -> ReceiptId {
    ReceiptId::from_bytes(digest16(
        RESIDENCY_VOUCHER_ID_DOMAIN,
        &[
            key.as_bytes(),
            node_id.as_bytes(),
            &[to_tier.discriminant()],
        ],
    ))
}

type ResidencyVoucherRow = (Vec<u8>, Vec<u8>, Vec<u8>, i64, i64, i64, i64, Vec<u8>, i64);

fn raw_residency_voucher_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResidencyVoucherRow> {
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

fn decode_residency_voucher_row(
    row: ResidencyVoucherRow,
) -> Result<ResidencyTransitionVoucher, PlanStoreError> {
    Ok(ResidencyTransitionVoucher {
        voucher_id: ReceiptId::from_bytes(fixed16(row.0, "residency voucher id")?),
        plan_id: TaskPlanId::from_bytes(fixed16(row.1, "residency voucher plan id")?),
        node_id: TaskNodeId::from_bytes(fixed16(row.2, "residency voucher node id")?),
        transition_seq: decode_u64(row.3)?,
        from_tier: decode_tier(row.4)?,
        to_tier: decode_tier(row.5)?,
        observed_revision: decode_u64(row.6)?,
        idempotency_key: IdempotencyKey::from_bytes(fixed16(
            row.7,
            "residency voucher idempotency key",
        )?),
        transitioned_at_ms: decode_u64(row.8)?,
    })
}

fn load_residency_voucher_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<ResidencyTransitionVoucher>, PlanStoreError> {
    connection
        .query_row(
            "SELECT voucher_id, plan_id, task_node_id, transition_seq, from_tier,
                    to_tier, observed_revision, idempotency_key, transitioned_at_ms
             FROM plan_node_residency_transitions WHERE idempotency_key = ?1",
            [key.as_bytes().as_slice()],
            raw_residency_voucher_row,
        )
        .optional()?
        .map(decode_residency_voucher_row)
        .transpose()
}

fn load_residency_voucher_by_seq(
    connection: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    seq: u64,
) -> Result<Option<ResidencyTransitionVoucher>, PlanStoreError> {
    if seq == 0 {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT voucher_id, plan_id, task_node_id, transition_seq, from_tier,
                    to_tier, observed_revision, idempotency_key, transitioned_at_ms
             FROM plan_node_residency_transitions
             WHERE plan_id = ?1 AND task_node_id = ?2 AND transition_seq = ?3",
            params![
                plan_id.as_bytes().as_slice(),
                node_id.as_bytes().as_slice(),
                encode_u64(seq)?,
            ],
            raw_residency_voucher_row,
        )
        .optional()?
        .map(decode_residency_voucher_row)
        .transpose()
}

fn residency_voucher_matches_request(
    voucher: &ResidencyTransitionVoucher,
    request: &ResidencyTransitionRequest,
) -> bool {
    voucher.plan_id == request.plan_id
        && voucher.node_id == request.node_id
        && voucher.from_tier == request.from_tier
        && voucher.to_tier == request.to_tier
        && voucher.observed_revision == request.expected_declared_revision
        && voucher.transitioned_at_ms == request.transitioned_at_ms
}
