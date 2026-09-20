//! Dependency Resolver over a plan revision's declared dependency DAG
//! (ADR-0016 决定 5, B4-3; `[PLAN-DEPENDENCY-001]`, `[PLAN-DAG-001]`).
//!
//! A typed [`PlanRevisionSelector`] (`current` or an exact revision) is
//! resolved once, inside one `BEGIN IMMEDIATE` transaction, into a durable
//! immutable [`PlanResolutionHandle`] carrying the generation it was
//! computed against (`revision` + `plan_digest`), the deterministic
//! topological order, and the canonical edge set. Resolution re-verifies
//! the revision's stored shape against the immutable receipt chain's
//! `nodes_root`/`dependencies_root` before resolving, and cycle detection
//! fails closed with the cycle members ([`PlanStoreError::PlanCycleMembers`]).
//!
//! G4 fencing: the handle is pinned — a later revision that reshapes
//! unresolved nodes never changes an in-flight resolution's view. Reads
//! through the handle ([`SqlitePlanAuthority::inspect_resolved_nodes`])
//! observe the revision's immutable declared-shape rows, never the mutable
//! current node rows.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use nlos_types::{IdempotencyKey, ReceiptId, TaskNodeId, TaskPlanId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::PlanStoreError;
use crate::model::{
    PlanNodeKind, PlanResolutionDecision, PlanResolutionHandle, PlanRevisionReceipt,
    PlanRevisionSelector, RESOLUTION_DIGEST_DOMAIN, RESOLUTION_ID_DOMAIN, ResolvePlanRequest,
    ResolvedPlanNode, decode_kind,
};
use crate::store::{
    SqlitePlanAuthority, dependencies_root_of_edges, digest16, fixed16, fixed32, load_plan_head,
    load_revision_receipt, nodes_root,
};

impl SqlitePlanAuthority {
    /// Resolves one plan revision's declared dependency DAG into a durable
    /// immutable resolution receipt (the version/generation-carrying
    /// handle). `Current` pins exactly once, at the head observed when the
    /// resolution commits; a replay of the same key after the head
    /// advanced is answered from the original receipt (crash-retry
    /// semantics), it never floats to the new head.
    ///
    /// The resolution re-verifies the revision's durable shape rows
    /// against the immutable receipt chain before resolving: shape
    /// tampering fails with [`PlanStoreError::CorruptRecord`], a cyclic
    /// shape fails with [`PlanStoreError::PlanCycleMembers`] naming the
    /// cycle members, and a revision without durable shape rows (applied
    /// before schema v2) fails with
    /// [`PlanStoreError::RevisionShapeUnavailable`] instead of being
    /// silently re-derived.
    ///
    /// # Errors
    ///
    /// Fails typed on unknown plans/revisions, unavailable or corrupt
    /// declared shape, dependency cycles, idempotency rebinding, or
    /// storage failure.
    #[allow(clippy::needless_pass_by_value)] // The owned request is the caller's exactly-once intent (house API shape).
    pub fn resolve_plan(
        &self,
        request: ResolvePlanRequest,
    ) -> Result<PlanResolutionDecision, PlanStoreError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Idempotent replay first: the durable receipt is the authority.
        // A `Current` retry after the head advanced is the same operation
        // and must be answered from the original receipt; an `At` retry
        // rebound to another revision (or another plan, or another
        // observation time) is a typed conflict.
        if let Some(existing) = load_resolution_by_key(&transaction, request.idempotency_key)? {
            if request.selector.plan_id() != existing.plan_id
                || request.resolved_at_ms != existing.resolved_at_ms
                || matches!(
                    request.selector,
                    PlanRevisionSelector::At { revision, .. } if revision != existing.revision
                )
            {
                return Err(PlanStoreError::IdempotencyConflict);
            }
            let content =
                compute_resolution_content(&transaction, existing.plan_id, existing.revision)?;
            if content.order != existing.resolved_order
                || content.edges != existing.resolved_edges
                || content.receipt.plan_digest != existing.plan_digest
                || content.resolution_digest != existing.resolution_digest
            {
                return Err(PlanStoreError::CorruptRecord(
                    "stored resolution receipt does not reproduce from its pinned revision",
                ));
            }
            transaction.commit()?;
            return Ok(PlanResolutionDecision::Replayed(existing));
        }

        let plan_id = request.selector.plan_id();
        let head =
            load_plan_head(&transaction, plan_id)?.ok_or(PlanStoreError::PlanNotFound(plan_id))?;
        let revision = match request.selector {
            PlanRevisionSelector::Current(_) => head.current_revision,
            PlanRevisionSelector::At { revision, .. } => revision,
        };
        let content = compute_resolution_content(&transaction, plan_id, revision)?;
        let handle = PlanResolutionHandle {
            resolution_id: derive_resolution_id(request.idempotency_key, plan_id, revision),
            plan_id,
            revision,
            plan_digest: content.receipt.plan_digest,
            resolution_digest: content.resolution_digest,
            resolved_order: content.order,
            resolved_edges: content.edges,
            idempotency_key: request.idempotency_key,
            resolved_at_ms: request.resolved_at_ms,
        };
        insert_resolution(&transaction, &handle)?;
        transaction.commit()?;
        Ok(PlanResolutionDecision::Resolved(handle))
    }

    /// Reads one durable resolution receipt, `None` when absent.
    ///
    /// # Errors
    ///
    /// Fails on storage failure or a corrupt receipt row.
    pub fn inspect_resolution(
        &self,
        resolution_id: ReceiptId,
    ) -> Result<Option<PlanResolutionHandle>, PlanStoreError> {
        let connection = self.lock()?;
        load_resolution_by_id(&connection, resolution_id)
    }

    /// Reads the node shapes a resolution was computed against, in
    /// resolved (topological) order. The view is pinned to the
    /// resolution's revision: a later revision that reshapes unresolved
    /// nodes is never silently observed through this face (G4).
    ///
    /// # Errors
    ///
    /// Fails typed when the resolution does not exist
    /// ([`PlanStoreError::ResolutionNotFound`]), when its revision's
    /// declared shape is unavailable, or on storage failure.
    pub fn inspect_resolved_nodes(
        &self,
        resolution_id: ReceiptId,
    ) -> Result<Vec<ResolvedPlanNode>, PlanStoreError> {
        let connection = self.lock()?;
        let handle = load_resolution_by_id(&connection, resolution_id)?
            .ok_or(PlanStoreError::ResolutionNotFound(resolution_id))?;
        // Pinned view: the revision's write-once declared shape rows, not
        // the mutable current `plan_nodes` rows.
        let shape = load_revision_shape(&connection, handle.plan_id, handle.revision)?;
        if shape.is_empty() {
            return Err(PlanStoreError::RevisionShapeUnavailable {
                plan_id: handle.plan_id,
                revision: handle.revision,
            });
        }
        let by_id: HashMap<TaskNodeId, RevisionShapeRow> =
            shape.into_iter().map(|row| (row.id, row)).collect();
        if by_id.len() != handle.resolved_order.len() {
            return Err(PlanStoreError::CorruptRecord(
                "declared shape does not cover the resolved order",
            ));
        }
        handle
            .resolved_order
            .iter()
            .enumerate()
            .map(|(index, node_id)| {
                let row = by_id.get(node_id).ok_or(PlanStoreError::CorruptRecord(
                    "resolved order references a node absent from the declared shape",
                ))?;
                Ok(ResolvedPlanNode {
                    node_id: row.id,
                    node_key: row.key,
                    kind: row.kind,
                    node_digest: row.digest,
                    position: u64::try_from(index + 1)
                        .map_err(|_| PlanStoreError::CorruptRecord("resolved position"))?,
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// resolution computation (pure over the durable shape rows)
// ---------------------------------------------------------------------------

struct ResolutionContent {
    receipt: PlanRevisionReceipt,
    resolution_digest: [u8; 32],
    order: Vec<TaskNodeId>,
    edges: Vec<(TaskNodeId, TaskNodeId)>,
}

/// Loads the revision's durable declared shape, re-verifies it against the
/// immutable receipt chain, and computes the deterministic topological
/// order. Every input is immutable (write-once shape rows, immutable
/// receipt), so the same revision always reproduces the same content —
/// the foundation of byte-equal receipt replay.
fn compute_resolution_content(
    connection: &Connection,
    plan_id: TaskPlanId,
    revision: u64,
) -> Result<ResolutionContent, PlanStoreError> {
    let receipt = load_revision_receipt(connection, plan_id, revision)?
        .ok_or(PlanStoreError::RevisionNotFound { plan_id, revision })?;
    let shape = load_revision_shape(connection, plan_id, revision)?;
    if shape.is_empty() {
        return Err(PlanStoreError::RevisionShapeUnavailable { plan_id, revision });
    }
    let mut edges = load_revision_edges(connection, plan_id, revision)?;
    let declared: HashSet<TaskNodeId> = shape.iter().map(|row| row.id).collect();
    for (dependent, dependency) in &edges {
        if !declared.contains(dependent) || !declared.contains(dependency) {
            return Err(PlanStoreError::CorruptRecord(
                "revision dependency edge references an undeclared node",
            ));
        }
    }
    // Cycle check first ([PLAN-DAG-001] structural safety): a cyclic shape
    // is reported with its members; non-cyclic shape tampering falls
    // through to the root re-verification and reports corruption.
    let order = topological_order(&declared, &edges).map_err(|members| {
        PlanStoreError::PlanCycleMembers {
            plan_id,
            revision,
            members,
        }
    })?;
    edges.sort_unstable();
    let digests: Vec<([u8; 32], TaskNodeId)> =
        shape.iter().map(|row| (row.digest, row.id)).collect();
    if nodes_root(plan_id, revision, &digests) != receipt.nodes_root {
        return Err(PlanStoreError::CorruptRecord(
            "declared node set does not match the revision nodes root",
        ));
    }
    if dependencies_root_of_edges(plan_id, revision, &edges) != receipt.dependencies_root {
        return Err(PlanStoreError::CorruptRecord(
            "declared edge set does not match the revision dependencies root",
        ));
    }
    let resolution_digest =
        resolution_digest(plan_id, revision, receipt.plan_digest, &order, &edges);
    Ok(ResolutionContent {
        receipt,
        resolution_digest,
        order,
        edges,
    })
}

// ---------------------------------------------------------------------------
// topological order + cycle member extraction (pure)
// ---------------------------------------------------------------------------

/// Kahn's algorithm over the `(dependent, dependency)` edge set with the
/// minimum-`TaskNodeId` tiebreak (deterministic order). `Err` carries the
/// nodes on the cycles of the unresolvable remainder.
fn topological_order(
    declared: &HashSet<TaskNodeId>,
    edges: &[(TaskNodeId, TaskNodeId)],
) -> Result<Vec<TaskNodeId>, Vec<TaskNodeId>> {
    let mut pending_dependencies: BTreeMap<TaskNodeId, usize> =
        declared.iter().map(|node_id| (*node_id, 0_usize)).collect();
    let mut dependents: HashMap<TaskNodeId, Vec<TaskNodeId>> = HashMap::new();
    for (dependent, dependency) in edges {
        *pending_dependencies.entry(*dependent).or_default() += 1;
        dependents.entry(*dependency).or_default().push(*dependent);
    }
    let mut ready: BTreeSet<TaskNodeId> = pending_dependencies
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(node_id, _)| *node_id)
        .collect();
    let mut order = Vec::with_capacity(declared.len());
    while let Some(node_id) = ready.pop_first() {
        order.push(node_id);
        if let Some(blocked) = dependents.get(&node_id) {
            for dependent in blocked {
                if let Some(count) = pending_dependencies.get_mut(dependent) {
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(*dependent);
                    }
                }
            }
        }
    }
    if order.len() == declared.len() {
        return Ok(order);
    }
    let ordered: HashSet<TaskNodeId> = order.iter().copied().collect();
    let remaining: BTreeSet<TaskNodeId> = declared.difference(&ordered).copied().collect();
    Err(cycle_members(&remaining, edges))
}

/// Nodes on cycles inside the unresolvable remainder: iterative three-color
/// DFS; each back edge to a node on the current path exposes that path
/// slice as a cycle. Nodes merely downstream of a cycle are not members.
fn cycle_members(
    remaining: &BTreeSet<TaskNodeId>,
    edges: &[(TaskNodeId, TaskNodeId)],
) -> Vec<TaskNodeId> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    let mut dependencies: HashMap<TaskNodeId, Vec<TaskNodeId>> = HashMap::new();
    for (dependent, dependency) in edges {
        if remaining.contains(dependent) && remaining.contains(dependency) {
            dependencies
                .entry(*dependent)
                .or_default()
                .push(*dependency);
        }
    }
    let mut color: HashMap<TaskNodeId, Color> = remaining
        .iter()
        .map(|node_id| (*node_id, Color::White))
        .collect();
    let mut members: Vec<TaskNodeId> = Vec::new();
    for start in remaining {
        if color.get(start) != Some(&Color::White) {
            continue;
        }
        let mut stack: Vec<(TaskNodeId, usize)> = vec![(*start, 0)];
        let mut path: Vec<TaskNodeId> = vec![*start];
        color.insert(*start, Color::Gray);
        while let Some((node, next_index)) = stack.pop() {
            let mut advanced = false;
            let deps: &[TaskNodeId] = dependencies.get(&node).map_or(&[], Vec::as_slice);
            for (index, dependency) in deps.iter().enumerate().skip(next_index) {
                match color.get(dependency) {
                    Some(Color::Gray) => {
                        if let Some(position) = path.iter().position(|id| id == dependency) {
                            members.extend_from_slice(&path[position..]);
                        }
                        stack.push((node, index + 1));
                        advanced = true;
                        break;
                    }
                    Some(Color::White) => {
                        stack.push((node, index + 1));
                        color.insert(*dependency, Color::Gray);
                        path.push(*dependency);
                        stack.push((*dependency, 0));
                        advanced = true;
                        break;
                    }
                    _ => {}
                }
            }
            if !advanced {
                color.insert(node, Color::Black);
                path.pop();
            }
        }
    }
    members.sort_unstable();
    members.dedup();
    members
}

// ---------------------------------------------------------------------------
// domain-separated digest derivation (pure, replay-relevant)
// ---------------------------------------------------------------------------

/// The receipt content root over the plan identity, generation, revision
/// digest, resolved order, and canonical edge set. The formula is fixed;
/// any field change must bump the domain version.
fn resolution_digest(
    plan_id: TaskPlanId,
    revision: u64,
    plan_digest: [u8; 32],
    order: &[TaskNodeId],
    edges: &[(TaskNodeId, TaskNodeId)],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(RESOLUTION_DIGEST_DOMAIN);
    hasher.update(plan_id.as_bytes());
    hasher.update(revision.to_be_bytes());
    hasher.update(plan_digest);
    hasher.update((order.len() as u64).to_be_bytes());
    for node_id in order {
        hasher.update(node_id.as_bytes());
    }
    hasher.update((edges.len() as u64).to_be_bytes());
    for (dependent, dependency) in edges {
        hasher.update(dependent.as_bytes());
        hasher.update(dependency.as_bytes());
    }
    hasher.finalize().into()
}

fn derive_resolution_id(key: IdempotencyKey, plan_id: TaskPlanId, revision: u64) -> ReceiptId {
    ReceiptId::from_bytes(digest16(
        RESOLUTION_ID_DOMAIN,
        &[key.as_bytes(), plan_id.as_bytes(), &revision.to_be_bytes()],
    ))
}

// ---------------------------------------------------------------------------
// durable shape and receipt rows
// ---------------------------------------------------------------------------

struct RevisionShapeRow {
    id: TaskNodeId,
    key: [u8; 16],
    kind: PlanNodeKind,
    digest: [u8; 32],
}

fn load_revision_shape(
    connection: &Connection,
    plan_id: TaskPlanId,
    revision: u64,
) -> Result<Vec<RevisionShapeRow>, PlanStoreError> {
    let mut statement = connection.prepare(
        "SELECT task_node_id, node_key, node_kind, node_digest
         FROM plan_revision_nodes WHERE plan_id = ?1 AND revision = ?2
         ORDER BY task_node_id",
    )?;
    let rows = statement.query_map(
        params![
            plan_id.as_bytes().as_slice(),
            crate::store::encode_u64(revision)?
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        },
    )?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(PlanStoreError::from)?
        .into_iter()
        .map(|(node_id, node_key, node_kind, node_digest)| {
            Ok(RevisionShapeRow {
                id: TaskNodeId::from_bytes(fixed16(node_id, "revision shape node id")?),
                key: fixed16(node_key, "revision shape node key")?,
                kind: decode_kind(node_kind)?,
                digest: fixed32(node_digest, "revision shape node digest")?,
            })
        })
        .collect()
}

fn load_revision_edges(
    connection: &Connection,
    plan_id: TaskPlanId,
    revision: u64,
) -> Result<Vec<(TaskNodeId, TaskNodeId)>, PlanStoreError> {
    let mut statement = connection.prepare(
        "SELECT dependent_node_id, dependency_node_id
         FROM plan_revision_edges WHERE plan_id = ?1 AND revision = ?2
         ORDER BY dependent_node_id, dependency_node_id",
    )?;
    let rows = statement.query_map(
        params![
            plan_id.as_bytes().as_slice(),
            crate::store::encode_u64(revision)?
        ],
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(PlanStoreError::from)?
        .into_iter()
        .map(|(dependent, dependency)| {
            Ok((
                TaskNodeId::from_bytes(fixed16(dependent, "revision edge dependent")?),
                TaskNodeId::from_bytes(fixed16(dependency, "revision edge dependency")?),
            ))
        })
        .collect()
}

const RESOLUTION_COLUMNS: &str = "resolution_id, idempotency_key, plan_id, revision, plan_digest,
        resolved_order, resolved_edges, resolution_digest, resolved_at_ms";

type ResolutionRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
);

fn raw_resolution_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResolutionRow> {
    Ok((
        row.get::<_, Vec<u8>>(0)?,
        row.get::<_, Vec<u8>>(1)?,
        row.get::<_, Vec<u8>>(2)?,
        row.get::<_, i64>(3)?,
        row.get::<_, Vec<u8>>(4)?,
        row.get::<_, Vec<u8>>(5)?,
        row.get::<_, Vec<u8>>(6)?,
        row.get::<_, Vec<u8>>(7)?,
        row.get::<_, i64>(8)?,
    ))
}

fn decode_resolution_row(row: ResolutionRow) -> Result<PlanResolutionHandle, PlanStoreError> {
    let handle = PlanResolutionHandle {
        resolution_id: ReceiptId::from_bytes(fixed16(row.0, "resolution id")?),
        idempotency_key: IdempotencyKey::from_bytes(fixed16(row.1, "resolution idempotency key")?),
        plan_id: TaskPlanId::from_bytes(fixed16(row.2, "resolution plan id")?),
        revision: crate::store::decode_u64(row.3)?,
        plan_digest: fixed32(row.4, "resolution plan digest")?,
        resolved_order: decode_order_blob(row.5.as_slice())?,
        resolved_edges: decode_edges_blob(row.6.as_slice())?,
        resolution_digest: fixed32(row.7, "resolution digest")?,
        resolved_at_ms: crate::store::decode_u64(row.8)?,
    };
    if resolution_digest(
        handle.plan_id,
        handle.revision,
        handle.plan_digest,
        &handle.resolved_order,
        &handle.resolved_edges,
    ) != handle.resolution_digest
    {
        return Err(PlanStoreError::CorruptRecord(
            "resolution digest does not verify",
        ));
    }
    Ok(handle)
}

fn decode_order_blob(bytes: &[u8]) -> Result<Vec<TaskNodeId>, PlanStoreError> {
    if !bytes.len().is_multiple_of(16) {
        return Err(PlanStoreError::CorruptRecord("resolved order width"));
    }
    bytes
        .chunks_exact(16)
        .map(|chunk| {
            let node: [u8; 16] = chunk
                .try_into()
                .map_err(|_| PlanStoreError::CorruptRecord("resolved order chunk"))?;
            Ok(TaskNodeId::from_bytes(node))
        })
        .collect()
}

fn decode_edges_blob(bytes: &[u8]) -> Result<Vec<(TaskNodeId, TaskNodeId)>, PlanStoreError> {
    if !bytes.len().is_multiple_of(32) {
        return Err(PlanStoreError::CorruptRecord("resolved edges width"));
    }
    bytes
        .chunks_exact(32)
        .map(|chunk| {
            let pair: [u8; 32] = chunk
                .try_into()
                .map_err(|_| PlanStoreError::CorruptRecord("resolved edges chunk"))?;
            Ok((
                TaskNodeId::from_bytes(
                    pair[..16]
                        .try_into()
                        .map_err(|_| PlanStoreError::CorruptRecord("resolved edge dependent"))?,
                ),
                TaskNodeId::from_bytes(
                    pair[16..]
                        .try_into()
                        .map_err(|_| PlanStoreError::CorruptRecord("resolved edge dependency"))?,
                ),
            ))
        })
        .collect()
}

fn load_resolution_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<PlanResolutionHandle>, PlanStoreError> {
    connection
        .query_row(
            &format!(
                "SELECT {RESOLUTION_COLUMNS} FROM plan_resolution_receipts
                 WHERE idempotency_key = ?1"
            ),
            [key.as_bytes().as_slice()],
            raw_resolution_row,
        )
        .optional()?
        .map(decode_resolution_row)
        .transpose()
}

fn load_resolution_by_id(
    connection: &Connection,
    resolution_id: ReceiptId,
) -> Result<Option<PlanResolutionHandle>, PlanStoreError> {
    connection
        .query_row(
            &format!(
                "SELECT {RESOLUTION_COLUMNS} FROM plan_resolution_receipts
                 WHERE resolution_id = ?1"
            ),
            [resolution_id.as_bytes().as_slice()],
            raw_resolution_row,
        )
        .optional()?
        .map(decode_resolution_row)
        .transpose()
}

fn insert_resolution(
    transaction: &rusqlite::Transaction<'_>,
    handle: &PlanResolutionHandle,
) -> Result<(), PlanStoreError> {
    let mut order_blob = Vec::with_capacity(handle.resolved_order.len() * 16);
    for node_id in &handle.resolved_order {
        order_blob.extend_from_slice(node_id.as_bytes());
    }
    let mut edges_blob = Vec::with_capacity(handle.resolved_edges.len() * 32);
    for (dependent, dependency) in &handle.resolved_edges {
        edges_blob.extend_from_slice(dependent.as_bytes());
        edges_blob.extend_from_slice(dependency.as_bytes());
    }
    transaction.execute(
        "INSERT INTO plan_resolution_receipts (
            resolution_id, idempotency_key, plan_id, revision, plan_digest,
            resolved_order, resolved_edges, resolution_digest, resolved_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            handle.resolution_id.as_bytes().as_slice(),
            handle.idempotency_key.as_bytes().as_slice(),
            handle.plan_id.as_bytes().as_slice(),
            crate::store::encode_u64(handle.revision)?,
            handle.plan_digest.as_slice(),
            order_blob.as_slice(),
            edges_blob.as_slice(),
            handle.resolution_digest.as_slice(),
            crate::store::encode_u64(handle.resolved_at_ms)?,
        ],
    )?;
    Ok(())
}
