//! Lazy materialization gate for plan nodes (W31-A, B4-4; ADR-0016 G3;
//! v0.5 行 3650 `[PLAN-LAZY-001]`、行 4503 `[SCALE-MATERIALIZE-001]`).
//!
//! The gate is the bridge between the logical declaration surface
//! (`plan_nodes`, W28-A) and physical execution: a node may enter
//! `MATERIALIZING` only through a gate-approved materialization request.
//! The protocol is the ADR-0013 verify-then-commit shape across the
//! plan/task authorities:
//!
//! 1.  **Request** (readiness fact, plan authority):
//!     [`SqlitePlanAuthority::request_materialization`] verifies
//!     dependency readiness (every declared dependency of the node's
//!     pinned declared-revision shape must be `COMPLETED`), drives the
//!     node through the legal §25.2.1 edges
//!     (`DECLARED/BLOCKED_DEPENDENCY → ELIGIBLE → WAITING_RESOURCE`) as
//!     dense vouchers, and records one durable `PENDING` request,
//!     idempotent by key. Unmet dependencies fail typed
//!     ([`PlanStoreError::DependenciesNotReady`]); a `DECLARED` node is
//!     durably advanced to `BLOCKED_DEPENDENCY` on that path.
//! 2.  **Consult** (Task authority, `nlos-task` W31-A wiring): the
//!     consumption path
//!     (`SqliteTaskAuthority::answer_plan_materialization`) consults the
//!     existing `ScaleProfile` admission APIs (declared-TaskNode
//!     dimension + working-set dimension) and returns the admission
//!     facts or the typed denial.
//! 3.  **Resolve** (commit, plan authority):
//!     [`SqlitePlanAuthority::resolve_materialization`] re-verifies the
//!     declared-revision fence and dependency readiness inside the
//!     commit transaction, then commits the verdict: an approval flips
//!     the node `WAITING_* → MATERIALIZING` and the request to
//!     `APPROVED` (admission facts recorded) in **one** transaction; a
//!     rejection records the typed reason and leaves the node
//!     `WAITING_*` — the materialization window shrinks, the plan does
//!     not fail (G3 falsification #2).
//!
//! G3 falsification #1 (a node with unmet dependencies materializing
//! through any face) is closed at three layers: the typed request
//! refusal, the resolution-time re-verification, and the storage-layer
//! `plan_node_transitions_materializing_gated` trigger, which refuses
//! every `→ MATERIALIZING` voucher without an `APPROVED` request — the
//! raw `record_node_transition` face included.
//!
//! Honest scope (deferred, see the lane evidence): of the five G3
//! conditions only dependency readiness and Task admission are enforced
//! here; Namespace/ResourceContract/fanout remain declared digests until
//! their authorities land. The materialization controller policy
//! (priority/deadline/locality window shaping) is W31-F.

use nlos_types::{IdempotencyKey, ReceiptId, TaskNodeId, TaskPlanId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::PlanStoreError;
use crate::model::{
    MATERIALIZATION_APPROVAL_KEY_DOMAIN, MATERIALIZATION_DRIVE_KEY_DOMAIN,
    MATERIALIZATION_REQUEST_ID_DOMAIN, MaterializationAdmission, MaterializationAdmissionVerdict,
    MaterializationApproval, MaterializationRejection, MaterializationRequest,
    MaterializationRequestDecision, MaterializationRequestRecord, MaterializationRequestStatus,
    MaterializationResolution, MaterializationResolutionDecision, NodeTransitionVoucher,
    PlanNodeRecord, PlanNodeState,
};
use crate::store::{
    SqlitePlanAuthority, decode_u64, derive_voucher_id, digest16, encode_u64, fixed16,
    load_plan_node,
};

impl SqlitePlanAuthority {
    /// Opens one materialization gate round for a node (ADR-0013
    /// readiness half). See the [module documentation](self) for the
    /// full protocol.
    ///
    /// # Errors
    ///
    /// Fails typed on unknown nodes, unmet dependencies
    /// ([`PlanStoreError::DependenciesNotReady`]), nodes that cannot
    /// await materialization (past the boundary or terminal,
    /// [`PlanStoreError::NodeNotAwaitingMaterialization`]), a concurrent
    /// pending request
    /// ([`PlanStoreError::MaterializationRequestAlreadyPending`]),
    /// idempotency rebinding, or storage failure.
    #[allow(clippy::needless_pass_by_value)] // The owned request is the caller's exactly-once intent (house API shape).
    pub fn request_materialization(
        &self,
        request: MaterializationRequest,
    ) -> Result<MaterializationRequestDecision, PlanStoreError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = load_request_by_key(&transaction, request.idempotency_key)? {
            if existing.plan_id != request.plan_id
                || existing.node_id != request.node_id
                || existing.requested_at_ms != request.requested_at_ms
            {
                return Err(PlanStoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(MaterializationRequestDecision::Replayed(existing));
        }

        let node = load_plan_node(&transaction, request.plan_id, request.node_id)?.ok_or(
            PlanStoreError::NodeNotFound {
                plan_id: request.plan_id,
                node_id: request.node_id,
            },
        )?;
        if let Some(pending) =
            load_pending_request_for_node(&transaction, request.plan_id, request.node_id)?
        {
            return Err(PlanStoreError::MaterializationRequestAlreadyPending {
                // The in-flight key is part of the diagnostic, not the
                // caller's identity.
                node_id: pending.node_id,
                #[allow(clippy::redundant_field_names)]
                pending_key: pending.idempotency_key,
            });
        }
        if request.requested_at_ms < node.first_declared_at_ms {
            return Err(PlanStoreError::InvalidRequest {
                reason: "materialization request precedes the node's first declaration",
            });
        }

        let unresolved = unresolved_dependencies(&transaction, &node)?;
        if !unresolved.is_empty() {
            // The blocked fact is durable: a DECLARED node advances to
            // BLOCKED_DEPENDENCY before the typed refusal.
            let mut node = node;
            if node.state == PlanNodeState::Declared {
                let key = drive_key(request.idempotency_key, 1);
                drive_transition(
                    &transaction,
                    &mut node,
                    PlanNodeState::BlockedDependency,
                    key,
                    request.requested_at_ms,
                )?;
            }
            transaction.commit()?;
            return Err(PlanStoreError::DependenciesNotReady {
                node_id: request.node_id,
                unresolved,
            });
        }

        let mut node = node;
        match node.state {
            PlanNodeState::Declared | PlanNodeState::BlockedDependency => {
                drive_transition(
                    &transaction,
                    &mut node,
                    PlanNodeState::Eligible,
                    drive_key(request.idempotency_key, 1),
                    request.requested_at_ms,
                )?;
                drive_transition(
                    &transaction,
                    &mut node,
                    PlanNodeState::WaitingResource,
                    drive_key(request.idempotency_key, 2),
                    request.requested_at_ms,
                )?;
            }
            PlanNodeState::Eligible => {
                drive_transition(
                    &transaction,
                    &mut node,
                    PlanNodeState::WaitingResource,
                    drive_key(request.idempotency_key, 1),
                    request.requested_at_ms,
                )?;
            }
            PlanNodeState::WaitingAuthorization
            | PlanNodeState::WaitingResource
            | PlanNodeState::Rehydrating => {}
            current => {
                return Err(PlanStoreError::NodeNotAwaitingMaterialization {
                    node_id: request.node_id,
                    current,
                });
            }
        }

        let record = insert_pending_request(&transaction, &request, node.declared_revision)?;
        transaction.commit()?;
        Ok(MaterializationRequestDecision::Requested(record))
    }

    /// Resolves one pending materialization request with the Task-side
    /// admission verdict (ADR-0013 commit half). Approval commits the
    /// request row and the `WAITING_* → MATERIALIZING` voucher in one
    /// transaction; rejection records the typed reason and keeps the
    /// node `WAITING_*` (the window shrinks). Replays return the
    /// original outcome; a different verdict or timestamp on a resolved
    /// request is a typed idempotency conflict.
    ///
    /// # Errors
    ///
    /// Fails typed on unknown request keys
    /// ([`PlanStoreError::MaterializationRequestNotFound`]), stale
    /// declared-revision fences
    /// ([`PlanStoreError::StaleNodeRevision`]), unmet dependencies at
    /// commit time, nodes no longer awaiting materialization,
    /// idempotency rebinding, or storage failure.
    pub fn resolve_materialization(
        &self,
        resolution: MaterializationResolution,
    ) -> Result<MaterializationResolutionDecision, PlanStoreError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing = load_request_by_key(&transaction, resolution.request_key)?.ok_or(
            PlanStoreError::MaterializationRequestNotFound(resolution.request_key),
        )?;
        if existing.status != MaterializationRequestStatus::Pending {
            if resolution_matches_record(&resolution, &existing) {
                let replayed = replay_resolved(&transaction, existing)?;
                transaction.commit()?;
                return Ok(replayed);
            }
            return Err(PlanStoreError::IdempotencyConflict);
        }
        if resolution.resolved_at_ms < existing.requested_at_ms {
            return Err(PlanStoreError::InvalidRequest {
                reason: "materialization resolution precedes its request",
            });
        }

        let node = load_plan_node(&transaction, existing.plan_id, existing.node_id)?.ok_or(
            PlanStoreError::NodeNotFound {
                plan_id: existing.plan_id,
                node_id: existing.node_id,
            },
        )?;
        if node.declared_revision != existing.observed_declared_revision {
            return Err(PlanStoreError::StaleNodeRevision {
                node_id: existing.node_id,
                expected: existing.observed_declared_revision,
                current: node.declared_revision,
            });
        }
        let unresolved = unresolved_dependencies(&transaction, &node)?;
        if !unresolved.is_empty() {
            return Err(PlanStoreError::DependenciesNotReady {
                node_id: existing.node_id,
                unresolved,
            });
        }

        match resolution.verdict {
            MaterializationAdmissionVerdict::Approved(admission) => {
                if !matches!(
                    node.state,
                    PlanNodeState::WaitingAuthorization
                        | PlanNodeState::WaitingResource
                        | PlanNodeState::Rehydrating
                ) {
                    return Err(PlanStoreError::NodeNotAwaitingMaterialization {
                        node_id: existing.node_id,
                        current: node.state,
                    });
                }
                // Order matters: the request row must read APPROVED
                // before the voucher insert, because the storage-layer
                // materializing gate consults it.
                let approval_key = approval_key(resolution.request_key);
                let voucher_id =
                    derive_voucher_id(approval_key, node.node_id, PlanNodeState::Materializing);
                let mut record = existing;
                apply_approval(
                    &transaction,
                    &mut record,
                    &admission,
                    voucher_id,
                    resolution.resolved_at_ms,
                )?;
                let mut node = node;
                let voucher = drive_transition(
                    &transaction,
                    &mut node,
                    PlanNodeState::Materializing,
                    approval_key,
                    resolution.resolved_at_ms,
                )?;
                debug_assert_eq!(voucher.voucher_id, voucher_id);
                transaction.commit()?;
                let approval = MaterializationApproval {
                    request: record,
                    voucher,
                };
                Ok(MaterializationResolutionDecision::Approved(approval))
            }
            MaterializationAdmissionVerdict::Rejected(reason) => {
                let mut record = existing;
                apply_rejection(
                    &transaction,
                    &mut record,
                    &reason,
                    resolution.resolved_at_ms,
                )?;
                transaction.commit()?;
                Ok(MaterializationResolutionDecision::Rejected(record))
            }
        }
    }

    /// Reads one materialization request by its exactly-once key,
    /// `None` when absent.
    ///
    /// # Errors
    ///
    /// Fails on storage failure or a corrupt row.
    pub fn inspect_materialization_request(
        &self,
        idempotency_key: IdempotencyKey,
    ) -> Result<Option<MaterializationRequestRecord>, PlanStoreError> {
        let connection = self.lock()?;
        load_request_by_key(&connection, idempotency_key)
    }

    /// Lists one node's materialization request history in
    /// `(requested_at_ms, rowid)` order.
    ///
    /// # Errors
    ///
    /// Fails typed when the node does not exist, or on storage failure.
    pub fn inspect_node_materialization_requests(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
    ) -> Result<Vec<MaterializationRequestRecord>, PlanStoreError> {
        let connection = self.lock()?;
        if load_plan_node(&connection, plan_id, node_id)?.is_none() {
            return Err(PlanStoreError::NodeNotFound { plan_id, node_id });
        }
        let mut statement = connection.prepare(
            "SELECT request_id, idempotency_key, plan_id, task_node_id,
                    observed_declared_revision, status,
                    admission_profile, admitted_task_nodes, admitted_active_working_set,
                    approved_voucher_id, rejection_kind, rejection_profile,
                    rejection_observed, rejection_cap,
                    requested_at_ms, resolved_at_ms
             FROM plan_materialization_requests
             WHERE plan_id = ?1 AND task_node_id = ?2
             ORDER BY requested_at_ms, rowid",
        )?;
        let rows = statement.query_map(
            params![plan_id.as_bytes().as_slice(), node_id.as_bytes().as_slice()],
            raw_request_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(PlanStoreError::from)?
            .into_iter()
            .map(decode_request_row)
            .collect()
    }

    /// Store-wide persisted declared-TaskNode count (the ADR-0016 决定 4
    /// dimension read face the Task-side materialization consult
    /// consumes; every declared node of every plan).
    ///
    /// # Errors
    ///
    /// Fails on storage failure.
    pub fn inspect_declared_task_node_count(&self) -> Result<u64, PlanStoreError> {
        let connection = self.lock()?;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM plan_nodes", [], |row| row.get(0))?;
        decode_u64(count)
    }
}

// ---------------------------------------------------------------------------
// in-transaction write helpers
// ---------------------------------------------------------------------------

/// Records one §25.2.1 voucher and advances the node row, mirroring
/// `record_node_transition`'s durable writes inside an already-open
/// gate transaction. `node` is advanced in place so callers can chain
/// driving steps.
fn drive_transition(
    transaction: &rusqlite::Transaction<'_>,
    node: &mut PlanNodeRecord,
    to_state: PlanNodeState,
    voucher_key: IdempotencyKey,
    at_ms: u64,
) -> Result<NodeTransitionVoucher, PlanStoreError> {
    let from_state = node.state;
    debug_assert!(PlanNodeState::transition_is_legal(from_state, to_state));
    let transition_seq = node.transition_count + 1;
    let voucher_id = derive_voucher_id(voucher_key, node.node_id, to_state);
    transaction.execute(
        "INSERT INTO plan_node_transitions (
            voucher_id, idempotency_key, plan_id, task_node_id, transition_seq,
            from_state, to_state, observed_revision, transitioned_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            voucher_id.as_bytes().as_slice(),
            voucher_key.as_bytes().as_slice(),
            node.plan_id.as_bytes().as_slice(),
            node.node_id.as_bytes().as_slice(),
            encode_u64(transition_seq)?,
            crate::model::encode_state(from_state),
            crate::model::encode_state(to_state),
            encode_u64(node.declared_revision)?,
            encode_u64(at_ms)?,
        ],
    )?;
    transaction.execute(
        "UPDATE plan_nodes
         SET node_state = ?3, transition_count = ?4, updated_at_ms = ?5
         WHERE plan_id = ?1 AND task_node_id = ?2",
        params![
            node.plan_id.as_bytes().as_slice(),
            node.node_id.as_bytes().as_slice(),
            crate::model::encode_state(to_state),
            encode_u64(transition_seq)?,
            encode_u64(at_ms)?,
        ],
    )?;
    node.state = to_state;
    node.transition_count = transition_seq;
    node.updated_at_ms = at_ms;
    Ok(NodeTransitionVoucher {
        voucher_id,
        plan_id: node.plan_id,
        node_id: node.node_id,
        transition_seq,
        from_state,
        to_state,
        observed_revision: node.declared_revision,
        idempotency_key: voucher_key,
        transitioned_at_ms: at_ms,
    })
}

fn insert_pending_request(
    transaction: &rusqlite::Transaction<'_>,
    request: &MaterializationRequest,
    observed_declared_revision: u64,
) -> Result<MaterializationRequestRecord, PlanStoreError> {
    let request_id = ReceiptId::from_bytes(digest16(
        MATERIALIZATION_REQUEST_ID_DOMAIN,
        &[request.idempotency_key.as_bytes()],
    ));
    transaction.execute(
        "INSERT INTO plan_materialization_requests (
            request_id, idempotency_key, plan_id, task_node_id,
            observed_declared_revision, status,
            requested_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)",
        params![
            request_id.as_bytes().as_slice(),
            request.idempotency_key.as_bytes().as_slice(),
            request.plan_id.as_bytes().as_slice(),
            request.node_id.as_bytes().as_slice(),
            encode_u64(observed_declared_revision)?,
            encode_u64(request.requested_at_ms)?,
        ],
    )?;
    Ok(MaterializationRequestRecord {
        request_id,
        plan_id: request.plan_id,
        node_id: request.node_id,
        idempotency_key: request.idempotency_key,
        observed_declared_revision,
        status: MaterializationRequestStatus::Pending,
        admission: None,
        rejection: None,
        approved_voucher_id: None,
        requested_at_ms: request.requested_at_ms,
        resolved_at_ms: None,
    })
}

/// Writes the `APPROVED` resolution columns. Called before the voucher
/// insert so the storage-layer materializing gate observes the approved
/// row.
fn apply_approval(
    transaction: &rusqlite::Transaction<'_>,
    record: &mut MaterializationRequestRecord,
    admission: &MaterializationAdmission,
    voucher_id: ReceiptId,
    resolved_at_ms: u64,
) -> Result<(), PlanStoreError> {
    transaction.execute(
        "UPDATE plan_materialization_requests
         SET status = 2, admission_profile = ?3, admitted_task_nodes = ?4,
             admitted_active_working_set = ?5, approved_voucher_id = ?6,
             resolved_at_ms = ?7
         WHERE request_id = ?1 AND idempotency_key = ?2",
        params![
            record.request_id.as_bytes().as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            admission.profile_id,
            encode_u64(admission.projected_task_nodes)?,
            encode_u64(admission.projected_active_working_set)?,
            voucher_id.as_bytes().as_slice(),
            encode_u64(resolved_at_ms)?,
        ],
    )?;
    record.status = MaterializationRequestStatus::Approved;
    record.admission = Some(admission.clone());
    record.approved_voucher_id = Some(voucher_id);
    record.resolved_at_ms = Some(resolved_at_ms);
    Ok(())
}

fn apply_rejection(
    transaction: &rusqlite::Transaction<'_>,
    record: &mut MaterializationRequestRecord,
    reason: &MaterializationRejection,
    resolved_at_ms: u64,
) -> Result<(), PlanStoreError> {
    let (kind, profile, observed, cap) = match reason {
        MaterializationRejection::WorkingSetFull {
            profile_id,
            active_count,
            max_active_working_set,
        } => (
            1_i64,
            profile_id.clone(),
            *active_count,
            *max_active_working_set,
        ),
        MaterializationRejection::TaskNodeCapExceeded {
            profile_id,
            task_count,
            max_task_nodes,
        } => (2_i64, profile_id.clone(), *task_count, *max_task_nodes),
    };
    transaction.execute(
        "UPDATE plan_materialization_requests
         SET status = 3, rejection_kind = ?3, rejection_profile = ?4,
             rejection_observed = ?5, rejection_cap = ?6, resolved_at_ms = ?7
         WHERE request_id = ?1 AND idempotency_key = ?2",
        params![
            record.request_id.as_bytes().as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            kind,
            profile,
            encode_u64(observed)?,
            encode_u64(cap)?,
            encode_u64(resolved_at_ms)?,
        ],
    )?;
    record.status = MaterializationRequestStatus::Rejected;
    record.rejection = Some(reason.clone());
    record.resolved_at_ms = Some(resolved_at_ms);
    Ok(())
}

// ---------------------------------------------------------------------------
// readiness verification (pure over the durable rows)
// ---------------------------------------------------------------------------

/// The node's declared dependencies (from its pinned declared-revision
/// shape) whose durable state is not `COMPLETED`.
pub(crate) fn unresolved_dependencies(
    connection: &Connection,
    node: &PlanNodeRecord,
) -> Result<Vec<TaskNodeId>, PlanStoreError> {
    let mut statement = connection.prepare(
        "SELECT dependency_node_id FROM plan_revision_edges
         WHERE plan_id = ?1 AND revision = ?2 AND dependent_node_id = ?3
         ORDER BY dependency_node_id",
    )?;
    let dependencies = statement
        .query_map(
            params![
                node.plan_id.as_bytes().as_slice(),
                encode_u64(node.declared_revision)?,
                node.node_id.as_bytes().as_slice(),
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?
        .collect::<Result<Vec<_>, _>>()
        .map_err(PlanStoreError::from)?;
    let mut unresolved = Vec::new();
    for bytes in dependencies {
        let dependency_id = TaskNodeId::from_bytes(fixed16(bytes, "dependency node id")?);
        let dependency = load_plan_node(connection, node.plan_id, dependency_id)?
            .ok_or(PlanStoreError::CorruptRecord("dependency node row missing"))?;
        if dependency.state != PlanNodeState::Completed {
            unresolved.push(dependency_id);
        }
    }
    Ok(unresolved)
}

// ---------------------------------------------------------------------------
// key derivation and replay matching (pure)
// ---------------------------------------------------------------------------

fn drive_key(request_key: IdempotencyKey, step: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes(digest16(
        MATERIALIZATION_DRIVE_KEY_DOMAIN,
        &[request_key.as_bytes(), &[step]],
    ))
}

fn approval_key(request_key: IdempotencyKey) -> IdempotencyKey {
    IdempotencyKey::from_bytes(digest16(
        MATERIALIZATION_APPROVAL_KEY_DOMAIN,
        &[request_key.as_bytes()],
    ))
}

/// Whether a presented resolution byte-matches a resolved record's
/// durable outcome (the replay predicate).
fn resolution_matches_record(
    resolution: &MaterializationResolution,
    record: &MaterializationRequestRecord,
) -> bool {
    if resolution.resolved_at_ms != record.resolved_at_ms.unwrap_or(0) {
        return false;
    }
    match &resolution.verdict {
        MaterializationAdmissionVerdict::Approved(admission) => {
            record.status == MaterializationRequestStatus::Approved
                && record.admission.as_ref() == Some(admission)
        }
        MaterializationAdmissionVerdict::Rejected(reason) => {
            record.status == MaterializationRequestStatus::Rejected
                && record.rejection.as_ref() == Some(reason)
        }
    }
}

fn replay_resolved(
    connection: &Connection,
    record: MaterializationRequestRecord,
) -> Result<MaterializationResolutionDecision, PlanStoreError> {
    match record.status {
        MaterializationRequestStatus::Approved => {
            let voucher_id = record
                .approved_voucher_id
                .ok_or(PlanStoreError::CorruptRecord(
                    "approved materialization request has no voucher",
                ))?;
            let voucher = load_voucher_by_id(connection, voucher_id)?.ok_or(
                PlanStoreError::CorruptRecord("approved materialization voucher missing"),
            )?;
            Ok(MaterializationResolutionDecision::ReplayedApproved(
                MaterializationApproval {
                    request: record,
                    voucher,
                },
            ))
        }
        MaterializationRequestStatus::Rejected => {
            Ok(MaterializationResolutionDecision::ReplayedRejected(record))
        }
        MaterializationRequestStatus::Pending => Err(PlanStoreError::CorruptRecord(
            "replay of a pending materialization request",
        )),
    }
}

// ---------------------------------------------------------------------------
// durable row helpers
// ---------------------------------------------------------------------------

type RequestRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<Vec<u8>>,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    i64,
    Option<i64>,
);

pub(crate) fn raw_request_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok((
        row.get::<_, Vec<u8>>(0)?,
        row.get::<_, Vec<u8>>(1)?,
        row.get::<_, Vec<u8>>(2)?,
        row.get::<_, Vec<u8>>(3)?,
        row.get::<_, i64>(4)?,
        row.get::<_, i64>(5)?,
        row.get::<_, Option<String>>(6)?,
        row.get::<_, Option<i64>>(7)?,
        row.get::<_, Option<i64>>(8)?,
        row.get::<_, Option<Vec<u8>>>(9)?,
        row.get::<_, i64>(10)?,
        row.get::<_, Option<String>>(11)?,
        row.get::<_, Option<i64>>(12)?,
        row.get::<_, Option<i64>>(13)?,
        row.get::<_, i64>(14)?,
        row.get::<_, Option<i64>>(15)?,
    ))
}

pub(crate) fn decode_request_row(
    row: RequestRow,
) -> Result<MaterializationRequestRecord, PlanStoreError> {
    let status = MaterializationRequestStatus::decode(row.5)?;
    let admission = match (&row.6, row.7, row.8) {
        (Some(profile), Some(task_nodes), Some(working_set)) => Some(MaterializationAdmission {
            profile_id: profile.clone(),
            projected_task_nodes: decode_u64(task_nodes)?,
            projected_active_working_set: decode_u64(working_set)?,
        }),
        (None, None, None) => None,
        _ => {
            return Err(PlanStoreError::CorruptRecord(
                "materialization admission facts shape",
            ));
        }
    };
    let rejection = match (row.10, &row.11, row.12, row.13) {
        (0, None, None, None) => None,
        (1, Some(profile), Some(observed), Some(cap)) => {
            Some(MaterializationRejection::WorkingSetFull {
                profile_id: profile.clone(),
                active_count: decode_u64(observed)?,
                max_active_working_set: decode_u64(cap)?,
            })
        }
        (2, Some(profile), Some(observed), Some(cap)) => {
            Some(MaterializationRejection::TaskNodeCapExceeded {
                profile_id: profile.clone(),
                task_count: decode_u64(observed)?,
                max_task_nodes: decode_u64(cap)?,
            })
        }
        _ => {
            return Err(PlanStoreError::CorruptRecord(
                "materialization rejection shape",
            ));
        }
    };
    Ok(MaterializationRequestRecord {
        request_id: ReceiptId::from_bytes(fixed16(row.0, "materialization request id")?),
        plan_id: TaskPlanId::from_bytes(fixed16(row.2, "materialization plan id")?),
        node_id: TaskNodeId::from_bytes(fixed16(row.3, "materialization node id")?),
        idempotency_key: IdempotencyKey::from_bytes(fixed16(
            row.1,
            "materialization idempotency key",
        )?),
        observed_declared_revision: decode_u64(row.4)?,
        status,
        admission,
        rejection,
        approved_voucher_id: match row.9 {
            Some(bytes) => Some(ReceiptId::from_bytes(fixed16(
                bytes,
                "approval voucher id",
            )?)),
            None => None,
        },
        requested_at_ms: decode_u64(row.14)?,
        resolved_at_ms: row.15.map(decode_u64).transpose()?,
    })
}

pub(crate) const REQUEST_COLUMNS: &str = "request_id, idempotency_key, plan_id, task_node_id,
        observed_declared_revision, status,
        admission_profile, admitted_task_nodes, admitted_active_working_set,
        approved_voucher_id, rejection_kind, rejection_profile,
        rejection_observed, rejection_cap,
        requested_at_ms, resolved_at_ms";

fn load_request_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<MaterializationRequestRecord>, PlanStoreError> {
    connection
        .query_row(
            &format!(
                "SELECT {REQUEST_COLUMNS} FROM plan_materialization_requests
                 WHERE idempotency_key = ?1"
            ),
            [key.as_bytes().as_slice()],
            raw_request_row,
        )
        .optional()?
        .map(decode_request_row)
        .transpose()
}

fn load_pending_request_for_node(
    connection: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
) -> Result<Option<MaterializationRequestRecord>, PlanStoreError> {
    connection
        .query_row(
            &format!(
                "SELECT {REQUEST_COLUMNS} FROM plan_materialization_requests
                 WHERE plan_id = ?1 AND task_node_id = ?2 AND status = 1"
            ),
            params![plan_id.as_bytes().as_slice(), node_id.as_bytes().as_slice()],
            raw_request_row,
        )
        .optional()?
        .map(decode_request_row)
        .transpose()
}

fn load_voucher_by_id(
    connection: &Connection,
    voucher_id: ReceiptId,
) -> Result<Option<NodeTransitionVoucher>, PlanStoreError> {
    connection
        .query_row(
            "SELECT voucher_id, plan_id, task_node_id, transition_seq, from_state,
                    to_state, observed_revision, idempotency_key, transitioned_at_ms
             FROM plan_node_transitions WHERE voucher_id = ?1",
            [voucher_id.as_bytes().as_slice()],
            raw_voucher_row,
        )
        .optional()?
        .map(decode_voucher_row)
        .transpose()
}

type VoucherRow = (Vec<u8>, Vec<u8>, Vec<u8>, i64, i64, i64, i64, Vec<u8>, i64);

fn raw_voucher_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<VoucherRow> {
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

fn decode_voucher_row(row: VoucherRow) -> Result<NodeTransitionVoucher, PlanStoreError> {
    Ok(NodeTransitionVoucher {
        voucher_id: ReceiptId::from_bytes(fixed16(row.0, "voucher id")?),
        plan_id: TaskPlanId::from_bytes(fixed16(row.1, "voucher plan id")?),
        node_id: TaskNodeId::from_bytes(fixed16(row.2, "voucher node id")?),
        transition_seq: decode_u64(row.3)?,
        from_state: crate::model::decode_state(row.4)?,
        to_state: crate::model::decode_state(row.5)?,
        observed_revision: decode_u64(row.6)?,
        idempotency_key: IdempotencyKey::from_bytes(fixed16(row.7, "voucher idempotency key")?),
        transitioned_at_ms: decode_u64(row.8)?,
    })
}
