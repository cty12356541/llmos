//! Single-writer `SQLite` implementation of the durable plan authority.
//!
//! The process-local mutex is an admission gate only; `BEGIN IMMEDIATE`
//! remains the storage-level writer fence, identical to `nlos-task`. Every
//! linearized decision (revision apply, node transition) commits its
//! durable effects in one transaction, so a crash cannot split a decision
//! from its receipt.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_types::{IdempotencyKey, ReceiptId, TaskNodeId, TaskPlanId};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::PlanStoreError;
use crate::model::{
    ApplyPlanRevisionRequest, ChainVerification, DEPENDENCIES_ROOT_DOMAIN,
    MAX_DECLARED_NODES_PER_REVISION, MAX_DEPENDENCIES_PER_NODE, NODES_ROOT_DOMAIN, NodeConditions,
    NodeResidencyTier, NodeTransitionDecision, NodeTransitionRequest, NodeTransitionVoucher,
    PLAN_ID_DOMAIN, PlanNodeDeclaration, PlanNodeKind, PlanNodeRecord, PlanNodeState,
    PlanRevisionDecision, PlanRevisionReceipt, PlanView, REVISION_DIGEST_DOMAIN,
    TASK_NODE_ID_DOMAIN, VOUCHER_ID_DOMAIN, decode_kind, decode_state, decode_tier, encode_kind,
    encode_state, encode_tier,
};
use crate::schema::{
    SCHEMA_VERSION, migrate_v1, migrate_v2, migrate_v3, migrate_v4, migrate_v5, migrate_v6,
    migrate_v7, migrate_v8,
};

/// A single-writer `SQLite` plan authority.
pub struct SqlitePlanAuthority {
    connection: Mutex<Connection>,
}

/// The answer of one apply-time declared-population consult
/// (W36-P8; W31-G §8.2.4): the Task tier either admits the projected
/// store-wide declared-TaskNode population or denies it with the
/// dimension's reason body (the Task authority owns the tier identity
/// and cap, mirroring the W31-A materialization consult vocabulary).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeclarationAdmissionOutcome {
    /// The projected population fits the tier's `max_task_nodes`.
    Admits,
    /// The tier's declared-TaskNode dimension denied the projection.
    Denied {
        /// Tier identifier of the denying profile.
        profile_id: String,
        /// Inclusive hard cap of the declared-TaskNode dimension.
        max_task_nodes: u64,
    },
}

/// The apply-time declared-population admission consult boundary
/// (W36-P8; W31-G §8.2.4): the Task authority's cross-authority answer
/// to "does a plan revision projecting this many declared `TaskNode`s
/// still fit the tier?" — a read-only consult through the W31-A seam
/// posture (the same boundary shape as the W31-F `AdmissionConsult`;
/// the 1:1 mapping from `SqliteTaskAuthority::answer_plan_declaration`
/// is assembler wiring, slice-k territory). `Err` denotes a failed
/// consult (transport/storage posture) — never a denial; the gated
/// apply fails closed on it.
pub trait DeclarationAdmissionConsult {
    /// The consult's own failure type (diagnostics stay with the
    /// implementation; the plan side records only the fact).
    type Error;

    /// Consults the Task-side declared-TaskNode dimension for one
    /// projected store-wide population.
    ///
    /// # Errors
    ///
    /// `Err` denotes a failed consult (transport/storage posture), not
    /// a denial — a denial is the
    /// `Ok(DeclarationAdmissionOutcome::Denied)` answer.
    fn consult_plan_declaration(
        &self,
        projected_task_nodes: u64,
    ) -> Result<DeclarationAdmissionOutcome, Self::Error>;
}

impl SqlitePlanAuthority {
    /// Opens or creates a plan authority database and validates its schema.
    ///
    /// Equivalent to [`SqlitePlanAuthority::open_with_vfs`] with `None`,
    /// i.e. the process-default `SQLite` VFS.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened, when WAL/FULL
    /// durability cannot be established (verified by reading the pragmas
    /// back; a silent fallback is rejected with
    /// [`PlanStoreError::DurabilityUnavailable`]), or when the stored
    /// schema version cannot be migrated or validated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PlanStoreError> {
        Self::open_with_vfs(path, None)
    }

    /// Opens or creates a plan authority database through a named
    /// `SQLite` VFS.
    ///
    /// `vfs = None` uses the process-default VFS; `Some(name)` selects a
    /// VFS previously registered under that name (e.g. a fault-injection
    /// shim registered by tests).
    ///
    /// # Errors
    ///
    /// Same as [`Self::open`].
    pub fn open_with_vfs(
        path: impl AsRef<Path>,
        vfs: Option<&str>,
    ) -> Result<Self, PlanStoreError> {
        let mut connection = match vfs {
            None => Connection::open(path)?,
            Some(name) => Connection::open_with_flags_and_vfs(path, OpenFlags::default(), name)?,
        };
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        // `pragma_update` discards the result row of `journal_mode`, so a
        // failed WAL transition would silently fall back (e.g. to
        // `delete`). Read both durability pragmas back and fail closed.
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(PlanStoreError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                migrate_v1(&mut connection)?;
                migrate_v2(&mut connection)?;
                migrate_v3(&mut connection)?;
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
            }
            1 => {
                migrate_v2(&mut connection)?;
                migrate_v3(&mut connection)?;
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
            }
            2 => {
                migrate_v3(&mut connection)?;
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
            }
            3 => {
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
            }
            4 => {
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
            }
            5 => {
                migrate_v6(&mut connection)?;
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
            }
            6 => {
                migrate_v7(&mut connection)?;
                migrate_v8(&mut connection)?;
            }
            7 => migrate_v8(&mut connection)?,
            SCHEMA_VERSION => {}
            other => return Err(PlanStoreError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>, PlanStoreError> {
        self.connection
            .lock()
            .map_err(|_| PlanStoreError::LockPoisoned)
    }

    /// Applies one plan revision. Revision 1 creates the plan (the
    /// authority derives the [`TaskPlanId`] from the idempotency key);
    /// later revisions must name the plan and re-declare the complete node
    /// set. Every revision appends one immutable receipt to the plan's
    /// digest chain.
    ///
    /// Nodes that already crossed the execution boundary (`MATERIALIZING`
    /// or beyond) must be re-declared with their exact original shape and
    /// keep their original revision/digest (`[PLAN-DAG-001]`, G1);
    /// re-shaping such a node fails closed with
    /// [`PlanStoreError::FrozenNodeShapeRewrite`]. Pre-execution nodes may
    /// be freely re-shaped, added, or dropped; their declared revision
    /// advances to the applied revision.
    ///
    /// A Task-side declared-population consult is mandatory (W31-G
    /// §8.2.4). This face carries no consult argument, so it is a typed
    /// refusal ([`PlanStoreError::DeclarationConsultUnavailable`]) with
    /// zero durable writes — never a silent admit. Production callers
    /// use [`Self::apply_plan_revision_with_admission`]. The consult-free
    /// bypass is [`Self::apply_plan_revision_ungated`] and is a
    /// test/fixture-only surface.
    ///
    /// # Errors
    ///
    /// Always [`PlanStoreError::DeclarationConsultUnavailable`].
    #[allow(clippy::needless_pass_by_value)] // The owned request is the caller's exactly-once intent (house API shape).
    pub fn apply_plan_revision(
        &self,
        request: ApplyPlanRevisionRequest,
    ) -> Result<PlanRevisionDecision, PlanStoreError> {
        let _ = request;
        Err(PlanStoreError::DeclarationConsultUnavailable)
    }

    /// Test/fixture-only consult-free apply. Production declaration
    /// must go through [`Self::apply_plan_revision_with_admission`]; the
    /// public default [`Self::apply_plan_revision`] typed-denies a
    /// missing consult (W31-G §8.2.4).
    ///
    /// # Errors
    ///
    /// Fails typed on structural violations (empty set, duplicate keys,
    /// unknown or self dependencies, cycles, bound exceedance), on
    /// idempotency rebinding, on unknown plans, and on storage failure.
    #[allow(clippy::needless_pass_by_value)] // The owned request is the caller's exactly-once intent (house API shape).
    #[allow(clippy::too_many_lines)] // One auditable transaction carries the full revision write set.
    pub fn apply_plan_revision_ungated(
        &self,
        request: ApplyPlanRevisionRequest,
    ) -> Result<PlanRevisionDecision, PlanStoreError> {
        validate_declaration(&request.nodes)?;
        let plan_id = match request.plan_id {
            Some(plan_id) => plan_id,
            None => derive_plan_id(&request.idempotency_key),
        };
        let digests: Vec<([u8; 32], TaskNodeId)> = request
            .nodes
            .iter()
            .map(|node| (node_digest(node), derive_node_id(plan_id, &node.node_key)))
            .collect();

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Idempotent replay first: the durable receipt is the authority.
        if let Some(existing) = load_revision_receipt_by_key(&transaction, request.idempotency_key)?
        {
            let replay_nodes_root = nodes_root(plan_id, existing.revision, &digests);
            let replay_dependencies_root =
                dependencies_root(plan_id, existing.revision, &request.nodes);
            if existing.plan_id != plan_id
                || existing.nodes_root != replay_nodes_root
                || existing.dependencies_root != replay_dependencies_root
                || existing.declared_node_count != request.nodes.len() as u64
                || existing.applied_at_ms != request.applied_at_ms
            {
                return Err(PlanStoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(PlanRevisionDecision::Replayed(existing));
        }

        let (revision, parent_digest) = if request.plan_id.is_none() {
            (1, None)
        } else {
            let current = load_plan_head(&transaction, plan_id)?
                .ok_or(PlanStoreError::PlanNotFound(plan_id))?;
            let parent = load_revision_digest(&transaction, plan_id, current.current_revision)?
                .ok_or(PlanStoreError::CorruptRecord(
                    "plan head has no revision receipt",
                ))?;
            (current.current_revision + 1, Some(parent))
        };

        let nodes_root = nodes_root(plan_id, revision, &digests);
        let dependencies_root = dependencies_root(plan_id, revision, &request.nodes);
        let plan_digest = revision_digest(
            plan_id,
            revision,
            parent_digest,
            nodes_root,
            dependencies_root,
        );

        let receipt = write_revision(&WriteRevisionArgs {
            transaction: &transaction,
            plan_id,
            revision,
            parent_digest,
            nodes_root,
            dependencies_root,
            plan_digest,
            digests: &digests,
            nodes: &request.nodes,
            idempotency_key: request.idempotency_key,
            applied_at_ms: request.applied_at_ms,
        })?;
        transaction.commit()?;

        Ok(PlanRevisionDecision::Applied(receipt))
    }

    /// Applies one plan revision behind the apply-time declared-population
    /// admission consult (W36-P8; W31-G §8.2.4 — the declaration half the
    /// W31-A materialization consult left open). The projection is the
    /// store-wide persisted `plan_nodes` count plus the node keys this
    /// revision declares that no row carries yet (rows are lifetime
    /// metadata, so each new key is exactly one future row); the consult
    /// answers inside the already-open `BEGIN IMMEDIATE` transaction, so
    /// the verified population and the committed revision are one
    /// consistent snapshot. A denial is a typed refusal before any write;
    /// a failed consult fails closed (ADR-0013: cannot verify ⇒ do not
    /// commit). Idempotent replays bypass the consult — the durable
    /// receipt is the authority, mirroring the registration-gate
    /// discipline.
    ///
    /// # Errors
    ///
    /// Same structural surface as [`Self::apply_plan_revision_ungated`],
    /// plus [`PlanStoreError::DeclarationAdmissionDenied`] when the Task
    /// tier denies the projected population and
    /// [`PlanStoreError::DeclarationConsultUnavailable`] when the consult
    /// itself fails. This is the production declaration face; the
    /// no-consult default [`Self::apply_plan_revision`] typed-denies.
    pub fn apply_plan_revision_with_admission<C: DeclarationAdmissionConsult>(
        &self,
        request: ApplyPlanRevisionRequest,
        consult: &C,
    ) -> Result<PlanRevisionDecision, PlanStoreError> {
        // Consume the owned request (exactly-once intent stays at the API
        // boundary) so needless_pass_by_value does not fire without #[allow].
        let ApplyPlanRevisionRequest {
            plan_id: request_plan_id,
            nodes,
            idempotency_key,
            applied_at_ms,
        } = request;
        validate_declaration(&nodes)?;
        let plan_id = match request_plan_id {
            Some(plan_id) => plan_id,
            None => derive_plan_id(&idempotency_key),
        };
        let digests: Vec<([u8; 32], TaskNodeId)> = nodes
            .iter()
            .map(|node| (node_digest(node), derive_node_id(plan_id, &node.node_key)))
            .collect();

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Idempotent replay first: the durable receipt is the authority
        // (the consult is bypassed exactly like the registration gate).
        if let Some(existing) = load_revision_receipt_by_key(&transaction, idempotency_key)? {
            let replay_nodes_root = nodes_root(plan_id, existing.revision, &digests);
            let replay_dependencies_root = dependencies_root(plan_id, existing.revision, &nodes);
            if existing.plan_id != plan_id
                || existing.nodes_root != replay_nodes_root
                || existing.dependencies_root != replay_dependencies_root
                || existing.declared_node_count != nodes.len() as u64
                || existing.applied_at_ms != applied_at_ms
            {
                return Err(PlanStoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(PlanRevisionDecision::Replayed(existing));
        }

        let (revision, parent_digest) = if request_plan_id.is_none() {
            (1, None)
        } else {
            let current = load_plan_head(&transaction, plan_id)?
                .ok_or(PlanStoreError::PlanNotFound(plan_id))?;
            let parent = load_revision_digest(&transaction, plan_id, current.current_revision)?
                .ok_or(PlanStoreError::CorruptRecord(
                    "plan head has no revision receipt",
                ))?;
            (current.current_revision + 1, Some(parent))
        };

        let (projected, fresh) = projected_declared_population(&transaction, plan_id, &nodes)?;
        // Growth-only consult: a reshape that adds no `plan_nodes` row
        // is not a declaration-population admission question (the
        // lifetime rows already exist). Replay already bypasses;
        // no-growth follows the same discipline so an already-over-tier
        // store can still reshape without a silent new-key pass.
        if fresh > 0 {
            match consult.consult_plan_declaration(projected) {
                Ok(DeclarationAdmissionOutcome::Admits) => {}
                Ok(DeclarationAdmissionOutcome::Denied {
                    profile_id,
                    max_task_nodes,
                }) => {
                    return Err(PlanStoreError::DeclarationAdmissionDenied {
                        profile_id,
                        projected_task_nodes: projected,
                        max_task_nodes,
                    });
                }
                Err(_) => return Err(PlanStoreError::DeclarationConsultUnavailable),
            }
        }

        let nodes_root = nodes_root(plan_id, revision, &digests);
        let dependencies_root = dependencies_root(plan_id, revision, &nodes);
        let plan_digest = revision_digest(
            plan_id,
            revision,
            parent_digest,
            nodes_root,
            dependencies_root,
        );
        let receipt = write_revision(&WriteRevisionArgs {
            transaction: &transaction,
            plan_id,
            revision,
            parent_digest,
            nodes_root,
            dependencies_root,
            plan_digest,
            digests: &digests,
            nodes: &nodes,
            idempotency_key,
            applied_at_ms,
        })?;
        transaction.commit()?;

        Ok(PlanRevisionDecision::Applied(receipt))
    }

    /// Reads one plan's current head, `None` when the plan does not exist.
    ///
    /// # Errors
    ///
    /// Fails on storage failure or a corrupt row.
    pub fn inspect_plan(&self, plan_id: TaskPlanId) -> Result<Option<PlanView>, PlanStoreError> {
        let connection = self.lock()?;
        load_plan_head(&connection, plan_id)
    }

    /// Reads one immutable revision receipt, `None` when absent.
    ///
    /// # Errors
    ///
    /// Fails on storage failure or a corrupt row.
    pub fn inspect_plan_revision(
        &self,
        plan_id: TaskPlanId,
        revision: u64,
    ) -> Result<Option<PlanRevisionReceipt>, PlanStoreError> {
        let connection = self.lock()?;
        load_revision_receipt(&connection, plan_id, revision)
    }

    /// Reads one node's durable metadata, `None` when absent.
    ///
    /// # Errors
    ///
    /// Fails on storage failure or a corrupt row.
    pub fn inspect_node(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
    ) -> Result<Option<PlanNodeRecord>, PlanStoreError> {
        let connection = self.lock()?;
        load_plan_node(&connection, plan_id, node_id)
    }

    /// Lists all node metadata rows ever declared under the plan, ordered
    /// by node id (deterministic). Rows are lifetime metadata: nodes
    /// dropped from the current revision's declared set keep their rows
    /// and history; the current declared set is defined by the head
    /// revision's `nodes_root`.
    ///
    /// # Errors
    ///
    /// Fails typed when the plan does not exist, or on storage failure.
    pub fn list_plan_nodes(
        &self,
        plan_id: TaskPlanId,
    ) -> Result<Vec<PlanNodeRecord>, PlanStoreError> {
        let connection = self.lock()?;
        if load_plan_head(&connection, plan_id)?.is_none() {
            return Err(PlanStoreError::PlanNotFound(plan_id));
        }
        let mut statement = connection.prepare(
            "SELECT plan_id, task_node_id, node_key, node_kind, declared_revision,
                    node_digest, node_state, transition_count,
                    residency_tier, residency_transition_count,
                    first_declared_at_ms, updated_at_ms
             FROM plan_nodes WHERE plan_id = ?1 ORDER BY task_node_id",
        )?;
        let rows = statement.query_map([plan_id.as_bytes().as_slice()], raw_node_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(PlanStoreError::from)?
            .into_iter()
            .map(decode_node_row)
            .collect()
    }

    /// Reads the structured G3 gate conditions one revision's declared
    /// shape row carries (W36-P7). `Ok(None)` means the node was
    /// declared in the digest-only form (or the shape row is absent).
    ///
    /// # Errors
    ///
    /// Fails typed on a corrupt (malformed or non-canonical) stored
    /// body, or on storage failure.
    pub fn inspect_node_conditions(
        &self,
        plan_id: TaskPlanId,
        revision: u64,
        node_id: TaskNodeId,
    ) -> Result<Option<NodeConditions>, PlanStoreError> {
        let connection = self.lock()?;
        let body: Option<Vec<u8>> = connection
            .query_row(
                "SELECT conditions_body FROM plan_revision_nodes
                 WHERE plan_id = ?1 AND revision = ?2 AND task_node_id = ?3",
                params![
                    plan_id.as_bytes().as_slice(),
                    encode_u64(revision)?,
                    node_id.as_bytes().as_slice(),
                ],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .flatten();
        body.as_deref().map(NodeConditions::decode).transpose()
    }

    /// Lists one node's transition vouchers in dense sequence order.
    ///
    /// # Errors
    ///
    /// Fails typed when the node does not exist, or on storage failure.
    pub fn inspect_node_vouchers(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
    ) -> Result<Vec<NodeTransitionVoucher>, PlanStoreError> {
        let connection = self.lock()?;
        if load_plan_node(&connection, plan_id, node_id)?.is_none() {
            return Err(PlanStoreError::NodeNotFound { plan_id, node_id });
        }
        let mut statement = connection.prepare(
            "SELECT voucher_id, plan_id, task_node_id, transition_seq, from_state,
                    to_state, observed_revision, idempotency_key, transitioned_at_ms
             FROM plan_node_transitions
             WHERE plan_id = ?1 AND task_node_id = ?2
             ORDER BY transition_seq",
        )?;
        let rows = statement.query_map(
            params![plan_id.as_bytes().as_slice(), node_id.as_bytes().as_slice()],
            raw_voucher_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(PlanStoreError::from)?
            .into_iter()
            .map(decode_voucher_row)
            .collect()
    }

    /// Records one node state transition: validates the §25.2.1 edge, the
    /// declared-revision CAS, and the state CAS, then commits the immutable
    /// voucher and the node-state advance in one transaction.
    ///
    /// # Errors
    ///
    /// Fails typed on illegal edges, stale revision/state CAS,
    /// idempotency rebinding, unknown nodes, or storage failure.
    pub fn record_node_transition(
        &self,
        request: NodeTransitionRequest,
    ) -> Result<NodeTransitionDecision, PlanStoreError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Idempotent replay first: the durable voucher is the authority.
        if let Some(existing) = load_voucher_by_key(&transaction, request.idempotency_key)? {
            if voucher_matches_request(&existing, &request) {
                transaction.commit()?;
                return Ok(NodeTransitionDecision::Replayed(existing));
            }
            return Err(PlanStoreError::IdempotencyConflict);
        }

        let node = load_plan_node(&transaction, request.plan_id, request.node_id)?.ok_or(
            PlanStoreError::NodeNotFound {
                plan_id: request.plan_id,
                node_id: request.node_id,
            },
        )?;
        if !PlanNodeState::transition_is_legal(request.from_state, request.to_state) {
            return Err(PlanStoreError::IllegalNodeTransition {
                node_id: request.node_id,
                from: request.from_state,
                to: request.to_state,
            });
        }
        if node.declared_revision != request.expected_declared_revision {
            return Err(PlanStoreError::StaleNodeRevision {
                node_id: request.node_id,
                expected: request.expected_declared_revision,
                current: node.declared_revision,
            });
        }
        if node.state != request.from_state {
            return Err(PlanStoreError::NodeStateCasMismatch {
                node_id: request.node_id,
                expected_from: request.from_state,
                current: node.state,
            });
        }
        if request.transitioned_at_ms < node.first_declared_at_ms {
            return Err(PlanStoreError::InvalidRequest {
                reason: "transition precedes the node's first declaration",
            });
        }

        let transition_seq = node.transition_count + 1;
        let voucher_id =
            derive_voucher_id(request.idempotency_key, request.node_id, request.to_state);
        transaction.execute(
            "INSERT INTO plan_node_transitions (
                voucher_id, idempotency_key, plan_id, task_node_id, transition_seq,
                from_state, to_state, observed_revision, transitioned_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                voucher_id.as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice(),
                request.plan_id.as_bytes().as_slice(),
                request.node_id.as_bytes().as_slice(),
                encode_u64(transition_seq)?,
                encode_state(request.from_state),
                encode_state(request.to_state),
                encode_u64(node.declared_revision)?,
                encode_u64(request.transitioned_at_ms)?,
            ],
        )?;
        transaction.execute(
            "UPDATE plan_nodes
             SET node_state = ?3, transition_count = ?4, updated_at_ms = ?5
             WHERE plan_id = ?1 AND task_node_id = ?2",
            params![
                request.plan_id.as_bytes().as_slice(),
                request.node_id.as_bytes().as_slice(),
                encode_state(request.to_state),
                encode_u64(transition_seq)?,
                encode_u64(request.transitioned_at_ms)?,
            ],
        )?;
        transaction.commit()?;

        Ok(NodeTransitionDecision::Recorded(NodeTransitionVoucher {
            voucher_id,
            plan_id: request.plan_id,
            node_id: request.node_id,
            transition_seq,
            from_state: request.from_state,
            to_state: request.to_state,
            observed_revision: node.declared_revision,
            idempotency_key: request.idempotency_key,
            transitioned_at_ms: request.transitioned_at_ms,
        }))
    }

    /// Walks the plan's immutable revision chain from revision 1 to the
    /// head, re-deriving every digest link; any tampered or skipped link
    /// fails typed instead of returning a fake intact result.
    ///
    /// # Errors
    ///
    /// Fails with [`PlanStoreError::CorruptRecord`] when the chain does
    /// not verify, or on storage failure.
    pub fn verify_revision_chain(
        &self,
        plan_id: TaskPlanId,
    ) -> Result<ChainVerification, PlanStoreError> {
        let connection = self.lock()?;
        let head =
            load_plan_head(&connection, plan_id)?.ok_or(PlanStoreError::PlanNotFound(plan_id))?;
        let mut statement = connection.prepare(
            "SELECT revision, parent_revision_digest, nodes_root, dependencies_root, plan_digest
             FROM plan_revisions WHERE plan_id = ?1 ORDER BY revision",
        )?;
        let rows = statement.query_map([plan_id.as_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })?;
        let mut previous_digest: Option<[u8; 32]> = None;
        let mut head_digest: Option<[u8; 32]> = None;
        let mut count = 0_u64;
        for row in rows {
            let (revision, parent, nodes_root, dependencies_root, digest) = row?;
            count += 1;
            let expected_revision = count;
            if encode_u64(expected_revision)? != revision {
                return Err(PlanStoreError::CorruptRecord(
                    "plan revision chain is not dense",
                ));
            }
            let parent = parent
                .map(|bytes| {
                    bytes
                        .try_into()
                        .map_err(|_| PlanStoreError::CorruptRecord("parent digest width"))
                })
                .transpose()?;
            let nodes_root: [u8; 32] = nodes_root
                .try_into()
                .map_err(|_| PlanStoreError::CorruptRecord("nodes root width"))?;
            let dependencies_root: [u8; 32] = dependencies_root
                .try_into()
                .map_err(|_| PlanStoreError::CorruptRecord("dependencies root width"))?;
            let digest: [u8; 32] = digest
                .try_into()
                .map_err(|_| PlanStoreError::CorruptRecord("revision digest width"))?;
            if parent != previous_digest {
                return Err(PlanStoreError::CorruptRecord(
                    "plan revision parent digest does not chain",
                ));
            }
            if revision_digest(
                plan_id,
                expected_revision,
                parent,
                nodes_root,
                dependencies_root,
            ) != digest
            {
                return Err(PlanStoreError::CorruptRecord(
                    "plan revision digest does not verify",
                ));
            }
            previous_digest = Some(digest);
            head_digest = Some(digest);
        }
        if count != head.current_revision {
            return Err(PlanStoreError::CorruptRecord(
                "plan head revision is not the chain length",
            ));
        }
        Ok(ChainVerification {
            plan_id,
            revision_count: count,
            head_revision: Some(head.current_revision),
            head_digest,
        })
    }
}

// ---------------------------------------------------------------------------
// declaration validation (pure)
// ---------------------------------------------------------------------------

fn validate_declaration(nodes: &[PlanNodeDeclaration]) -> Result<(), PlanStoreError> {
    if nodes.is_empty() {
        return Err(PlanStoreError::InvalidRequest {
            reason: "a plan revision must declare at least one node",
        });
    }
    if nodes.len() > MAX_DECLARED_NODES_PER_REVISION {
        return Err(PlanStoreError::InvalidRequest {
            reason: "declared node set exceeds the admission bound",
        });
    }
    let mut seen = std::collections::HashSet::with_capacity(nodes.len());
    for node in nodes {
        if !seen.insert(node.node_key) {
            return Err(PlanStoreError::InvalidRequest {
                reason: "duplicate node key in one revision",
            });
        }
        if node.dependency_keys.len() > MAX_DEPENDENCIES_PER_NODE {
            return Err(PlanStoreError::InvalidRequest {
                reason: "node dependency set exceeds the admission bound",
            });
        }
        if let Some(conditions) = &node.conditions {
            conditions.validate()?;
        }
        let mut declared_dependencies =
            std::collections::HashSet::with_capacity(node.dependency_keys.len());
        for dependency in &node.dependency_keys {
            if *dependency == node.node_key {
                return Err(PlanStoreError::InvalidRequest {
                    reason: "node depends on itself",
                });
            }
            if !declared_dependencies.insert(*dependency) {
                return Err(PlanStoreError::InvalidRequest {
                    reason: "duplicate dependency key in one node",
                });
            }
        }
    }
    for node in nodes {
        for dependency in &node.dependency_keys {
            if !seen.contains(dependency) {
                return Err(PlanStoreError::UnknownDependency {
                    node_key: *dependency,
                });
            }
        }
    }
    if declaration_has_cycle(nodes) {
        return Err(PlanStoreError::PlanCycle);
    }
    Ok(())
}

/// Three-color cycle detection over the declared dependency edges
/// (`[PLAN-DAG-001]` requires a DAG).
fn declaration_has_cycle(nodes: &[PlanNodeDeclaration]) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    fn visit(
        node: &PlanNodeDeclaration,
        by_key: &HashMap<[u8; 16], &PlanNodeDeclaration>,
        colors: &mut HashMap<[u8; 16], Color>,
    ) -> bool {
        if colors.get(&node.node_key) == Some(&Color::Gray) {
            return true;
        }
        colors.insert(node.node_key, Color::Gray);
        for dependency in &node.dependency_keys {
            if let Some(next) = by_key.get(dependency) {
                let color = colors.get(dependency).copied().unwrap_or(Color::White);
                if color == Color::Gray {
                    return true;
                }
                if color == Color::White && visit(next, by_key, colors) {
                    return true;
                }
            }
        }
        colors.insert(node.node_key, Color::Black);
        false
    }

    let by_key: HashMap<[u8; 16], &PlanNodeDeclaration> =
        nodes.iter().map(|node| (node.node_key, node)).collect();
    let mut colors: HashMap<[u8; 16], Color> = HashMap::with_capacity(nodes.len());
    for node in nodes {
        colors.insert(node.node_key, Color::White);
    }
    nodes.iter().any(|node| {
        colors.get(&node.node_key) == Some(&Color::White) && visit(node, &by_key, &mut colors)
    })
}

// ---------------------------------------------------------------------------
// domain-separated digest derivation (pure, replay-relevant)
// ---------------------------------------------------------------------------

/// One node's canonical shape digest: kind, binding, the sorted dependency
/// key set, and the selector/contract/policy/ceiling digest bindings. The
/// digest excludes the plan identity: the same shape re-declared in a
/// later revision must hash identically so the frozen-shape comparison is
/// bitwise.
///
/// W36-P7 additive fold: a `Some(conditions)` declaration appends one
/// presence byte plus the canonical [`NodeConditions`] body; `None`
/// appends **nothing**, so pre-v6 node digests stay bit-identical (the
/// legacy digest-only form and every stored v1..v5 row compare bitwise
/// against the new formula's absence branch).
fn node_digest(node: &PlanNodeDeclaration) -> [u8; 32] {
    let kind_byte = match node.kind {
        PlanNodeKind::AgentRole => 1_u8,
        PlanNodeKind::Executable => 2_u8,
    };
    let mut dependencies: Vec<&[u8; 16]> = node.dependency_keys.iter().collect();
    dependencies.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(crate::model::NODE_DIGEST_DOMAIN);
    hasher.update(node.node_key);
    hasher.update([kind_byte]);
    hasher.update(node.binding_digest);
    hasher.update((dependencies.len() as u64).to_be_bytes());
    for dependency in dependencies {
        hasher.update(*dependency);
    }
    hasher.update(node.input_selectors_digest);
    hasher.update(node.output_contract_digest);
    hasher.update(node.policy_digest);
    hasher.update(node.resource_ceiling_digest);
    if let Some(conditions) = &node.conditions {
        hasher.update([1_u8]);
        hasher.update(conditions.canonical_bytes());
    }
    hasher.finalize().into()
}

fn derive_plan_id(key: &IdempotencyKey) -> TaskPlanId {
    TaskPlanId::from_bytes(digest16(PLAN_ID_DOMAIN, &[key.as_bytes()]))
}

pub(crate) fn derive_node_id(plan_id: TaskPlanId, node_key: &[u8; 16]) -> TaskNodeId {
    TaskNodeId::from_bytes(digest16(
        TASK_NODE_ID_DOMAIN,
        &[plan_id.as_bytes(), node_key],
    ))
}

pub(crate) fn derive_voucher_id(
    key: IdempotencyKey,
    node_id: TaskNodeId,
    to_state: PlanNodeState,
) -> ReceiptId {
    ReceiptId::from_bytes(digest16(
        VOUCHER_ID_DOMAIN,
        &[
            key.as_bytes(),
            node_id.as_bytes(),
            &[to_state.discriminant()],
        ],
    ))
}

pub(crate) fn nodes_root(
    plan_id: TaskPlanId,
    revision: u64,
    digests: &[([u8; 32], TaskNodeId)],
) -> [u8; 32] {
    let mut entries: Vec<(TaskNodeId, [u8; 32])> = digests
        .iter()
        .map(|(digest, node_id)| (*node_id, *digest))
        .collect();
    entries.sort_unstable_by_key(|(node_id, _)| *node_id);
    let mut hasher = Sha256::new();
    hasher.update(NODES_ROOT_DOMAIN);
    hasher.update(plan_id.as_bytes());
    hasher.update(revision.to_be_bytes());
    hasher.update((entries.len() as u64).to_be_bytes());
    for (node_id, digest) in entries {
        hasher.update(node_id.as_bytes());
        hasher.update(digest);
    }
    hasher.finalize().into()
}

fn dependencies_root(
    plan_id: TaskPlanId,
    revision: u64,
    nodes: &[PlanNodeDeclaration],
) -> [u8; 32] {
    let mut edges: Vec<(TaskNodeId, TaskNodeId)> = Vec::new();
    for node in nodes {
        let from = derive_node_id(plan_id, &node.node_key);
        for dependency in &node.dependency_keys {
            edges.push((from, derive_node_id(plan_id, dependency)));
        }
    }
    dependencies_root_of_edges(plan_id, revision, &edges)
}

/// The dependencies root over a canonical `(dependent, dependency)` edge
/// set (byte-identical to the declaration-driven path; shared with the
/// resolver's resolve-time root re-verification).
pub(crate) fn dependencies_root_of_edges(
    plan_id: TaskPlanId,
    revision: u64,
    edges: &[(TaskNodeId, TaskNodeId)],
) -> [u8; 32] {
    let mut edges = edges.to_vec();
    edges.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(DEPENDENCIES_ROOT_DOMAIN);
    hasher.update(plan_id.as_bytes());
    hasher.update(revision.to_be_bytes());
    hasher.update((edges.len() as u64).to_be_bytes());
    for (from, to) in edges {
        hasher.update(from.as_bytes());
        hasher.update(to.as_bytes());
    }
    hasher.finalize().into()
}

/// The immutable chain link: the revision digest covers the plan identity,
/// the dense revision number, the parent digest, and both content roots.
/// The formula is fixed; any field change must bump the domain version.
fn revision_digest(
    plan_id: TaskPlanId,
    revision: u64,
    parent: Option<[u8; 32]>,
    nodes_root: [u8; 32],
    dependencies_root: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(REVISION_DIGEST_DOMAIN);
    hasher.update(plan_id.as_bytes());
    hasher.update(revision.to_be_bytes());
    match parent {
        Some(parent) => {
            hasher.update([1_u8]);
            hasher.update(parent);
        }
        None => {
            hasher.update([0_u8]);
        }
    }
    hasher.update(nodes_root);
    hasher.update(dependencies_root);
    hasher.finalize().into()
}

pub(crate) fn digest16(domain: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update(part);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut out = [0_u8; 16];
    // Fixed-width slice of a fixed-width digest; cannot panic.
    out.copy_from_slice(&digest[..16]);
    out
}

// ---------------------------------------------------------------------------
// durable row helpers
// ---------------------------------------------------------------------------

pub(crate) fn encode_u64(value: u64) -> Result<i64, PlanStoreError> {
    i64::try_from(value).map_err(|_| PlanStoreError::CorruptRecord("u64 exceeds SQLite"))
}

pub(crate) fn decode_u64(value: i64) -> Result<u64, PlanStoreError> {
    u64::try_from(value).map_err(|_| PlanStoreError::CorruptRecord("negative integer"))
}

pub(crate) fn load_plan_head(
    connection: &Connection,
    plan_id: TaskPlanId,
) -> Result<Option<PlanView>, PlanStoreError> {
    connection
        .query_row(
            "SELECT current_revision, created_at_ms, updated_at_ms
             FROM plans WHERE plan_id = ?1",
            [plan_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(revision, created, updated)| {
            Ok(PlanView {
                plan_id,
                current_revision: decode_u64(revision)?,
                created_at_ms: decode_u64(created)?,
                updated_at_ms: decode_u64(updated)?,
            })
        })
        .transpose()
}

fn load_revision_digest(
    connection: &Connection,
    plan_id: TaskPlanId,
    revision: u64,
) -> Result<Option<[u8; 32]>, PlanStoreError> {
    connection
        .query_row(
            "SELECT plan_digest FROM plan_revisions
             WHERE plan_id = ?1 AND revision = ?2",
            params![plan_id.as_bytes().as_slice(), encode_u64(revision)?],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| PlanStoreError::CorruptRecord("revision digest width"))
        })
        .transpose()
}

const REVISION_COLUMNS: &str = "plan_id, revision, idempotency_key, parent_revision_digest,
        nodes_root, dependencies_root, plan_digest, declared_node_count, applied_at_ms";

fn raw_revision_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RevisionRow> {
    Ok((
        row.get::<_, Vec<u8>>(0)?,
        row.get::<_, i64>(1)?,
        row.get::<_, Vec<u8>>(2)?,
        row.get::<_, Option<Vec<u8>>>(3)?,
        row.get::<_, Vec<u8>>(4)?,
        row.get::<_, Vec<u8>>(5)?,
        row.get::<_, Vec<u8>>(6)?,
        row.get::<_, i64>(7)?,
        row.get::<_, i64>(8)?,
    ))
}

type RevisionRow = (
    Vec<u8>,
    i64,
    Vec<u8>,
    Option<Vec<u8>>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
);

fn decode_revision_row(row: RevisionRow) -> Result<PlanRevisionReceipt, PlanStoreError> {
    let plan_id = TaskPlanId::from_bytes(fixed16(row.0, "revision plan id")?);
    let revision = decode_u64(row.1)?;
    let idempotency_key = IdempotencyKey::from_bytes(fixed16(row.2, "revision idempotency key")?);
    let parent_revision_digest = row
        .3
        .map(|bytes| fixed32(bytes, "parent digest"))
        .transpose()?;
    let nodes_root = fixed32(row.4, "nodes root")?;
    let dependencies_root = fixed32(row.5, "dependencies root")?;
    let plan_digest = fixed32(row.6, "revision digest")?;
    Ok(PlanRevisionReceipt {
        plan_id,
        revision,
        idempotency_key,
        parent_revision_digest,
        nodes_root,
        dependencies_root,
        plan_digest,
        declared_node_count: decode_u64(row.7)?,
        applied_at_ms: decode_u64(row.8)?,
    })
}

pub(crate) fn load_revision_receipt(
    connection: &Connection,
    plan_id: TaskPlanId,
    revision: u64,
) -> Result<Option<PlanRevisionReceipt>, PlanStoreError> {
    connection
        .query_row(
            &format!(
                "SELECT {REVISION_COLUMNS} FROM plan_revisions
                 WHERE plan_id = ?1 AND revision = ?2"
            ),
            params![plan_id.as_bytes().as_slice(), encode_u64(revision)?],
            raw_revision_row,
        )
        .optional()?
        .map(decode_revision_row)
        .transpose()
}

fn load_revision_receipt_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<PlanRevisionReceipt>, PlanStoreError> {
    connection
        .query_row(
            &format!(
                "SELECT {REVISION_COLUMNS} FROM plan_revisions
                 WHERE idempotency_key = ?1"
            ),
            [key.as_bytes().as_slice()],
            raw_revision_row,
        )
        .optional()?
        .map(decode_revision_row)
        .transpose()
}

type NodeRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
    Vec<u8>,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
);

pub(crate) fn raw_node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRow> {
    Ok((
        row.get::<_, Vec<u8>>(0)?,
        row.get::<_, Vec<u8>>(1)?,
        row.get::<_, Vec<u8>>(2)?,
        row.get::<_, i64>(3)?,
        row.get::<_, i64>(4)?,
        row.get::<_, Vec<u8>>(5)?,
        row.get::<_, i64>(6)?,
        row.get::<_, i64>(7)?,
        row.get::<_, i64>(8)?,
        row.get::<_, i64>(9)?,
        row.get::<_, i64>(10)?,
        row.get::<_, i64>(11)?,
    ))
}

pub(crate) fn decode_node_row(row: NodeRow) -> Result<PlanNodeRecord, PlanStoreError> {
    Ok(PlanNodeRecord {
        plan_id: TaskPlanId::from_bytes(fixed16(row.0, "node plan id")?),
        node_id: TaskNodeId::from_bytes(fixed16(row.1, "node id")?),
        node_key: fixed16(row.2, "node key")?,
        kind: decode_kind(row.3)?,
        declared_revision: decode_u64(row.4)?,
        node_digest: fixed32(row.5, "node digest")?,
        state: decode_state(row.6)?,
        transition_count: decode_u64(row.7)?,
        residency_tier: decode_tier(row.8)?,
        residency_transition_count: decode_u64(row.9)?,
        first_declared_at_ms: decode_u64(row.10)?,
        updated_at_ms: decode_u64(row.11)?,
    })
}

pub(crate) fn load_plan_node(
    connection: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
) -> Result<Option<PlanNodeRecord>, PlanStoreError> {
    connection
        .query_row(
            "SELECT plan_id, task_node_id, node_key, node_kind, declared_revision,
                    node_digest, node_state, transition_count,
                    residency_tier, residency_transition_count,
                    first_declared_at_ms, updated_at_ms
             FROM plan_nodes WHERE plan_id = ?1 AND task_node_id = ?2",
            params![plan_id.as_bytes().as_slice(), node_id.as_bytes().as_slice()],
            raw_node_row,
        )
        .optional()?
        .map(decode_node_row)
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
        from_state: decode_state(row.4)?,
        to_state: decode_state(row.5)?,
        observed_revision: decode_u64(row.6)?,
        idempotency_key: IdempotencyKey::from_bytes(fixed16(row.7, "voucher idempotency key")?),
        transitioned_at_ms: decode_u64(row.8)?,
    })
}

fn load_voucher_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<NodeTransitionVoucher>, PlanStoreError> {
    connection
        .query_row(
            "SELECT voucher_id, plan_id, task_node_id, transition_seq, from_state,
                    to_state, observed_revision, idempotency_key, transitioned_at_ms
             FROM plan_node_transitions WHERE idempotency_key = ?1",
            [key.as_bytes().as_slice()],
            raw_voucher_row,
        )
        .optional()?
        .map(decode_voucher_row)
        .transpose()
}

pub(crate) fn fixed16(bytes: Vec<u8>, field: &'static str) -> Result<[u8; 16], PlanStoreError> {
    bytes
        .try_into()
        .map_err(|_| PlanStoreError::CorruptRecord(field))
}

pub(crate) fn fixed32(bytes: Vec<u8>, field: &'static str) -> Result<[u8; 32], PlanStoreError> {
    bytes
        .try_into()
        .map_err(|_| PlanStoreError::CorruptRecord(field))
}

fn voucher_matches_request(
    voucher: &NodeTransitionVoucher,
    request: &NodeTransitionRequest,
) -> bool {
    voucher.plan_id == request.plan_id
        && voucher.node_id == request.node_id
        && voucher.from_state == request.from_state
        && voucher.to_state == request.to_state
        && voucher.observed_revision == request.expected_declared_revision
        && voucher.transitioned_at_ms == request.transitioned_at_ms
}

// ---------------------------------------------------------------------------
// revision write set
// ---------------------------------------------------------------------------

/// Persists the declared node set: new nodes are inserted at this revision,
/// pre-execution nodes are reshaped to this revision, execution-frozen
/// nodes must re-declare their exact original shape and are left
/// untouched.
fn upsert_plan_nodes(
    transaction: &rusqlite::Transaction<'_>,
    plan_id: TaskPlanId,
    revision: u64,
    nodes: &[PlanNodeDeclaration],
    digests: &[([u8; 32], TaskNodeId)],
    applied_at_ms: u64,
) -> Result<(), PlanStoreError> {
    for (node, (digest, node_id)) in nodes.iter().zip(digests) {
        let existing = load_plan_node(transaction, plan_id, *node_id)?;
        match existing {
            None => {
                transaction.execute(
                    "INSERT INTO plan_nodes (
                        plan_id, task_node_id, node_key, node_kind, declared_revision,
                        node_digest, node_state, transition_count,
                        residency_tier, residency_transition_count,
                        first_declared_at_ms, updated_at_ms
                      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 0, ?8, 0, ?7, ?7)",
                    params![
                        plan_id.as_bytes().as_slice(),
                        node_id.as_bytes().as_slice(),
                        node.node_key.as_slice(),
                        encode_kind(node.kind),
                        encode_u64(revision)?,
                        digest.as_slice(),
                        encode_u64(applied_at_ms)?,
                        encode_tier(NodeResidencyTier::MetadataOnly),
                    ],
                )?;
            }
            Some(existing) => {
                if existing.state.is_execution_frozen() {
                    if *digest != existing.node_digest || node.kind != existing.kind {
                        return Err(PlanStoreError::FrozenNodeShapeRewrite {
                            plan_id,
                            node_id: existing.node_id,
                            declared_revision: existing.declared_revision,
                        });
                    }
                    // Bit-identical re-declaration keeps the executed
                    // node's original revision/digest row (`PLAN-DAG-001`).
                    continue;
                }
                transaction.execute(
                    "UPDATE plan_nodes
                     SET node_kind = ?3, declared_revision = ?4, node_digest = ?5,
                         updated_at_ms = ?6
                     WHERE plan_id = ?1 AND task_node_id = ?2",
                    params![
                        plan_id.as_bytes().as_slice(),
                        node_id.as_bytes().as_slice(),
                        encode_kind(node.kind),
                        encode_u64(revision)?,
                        digest.as_slice(),
                        encode_u64(applied_at_ms)?,
                    ],
                )?;
            }
        }
    }
    Ok(())
}

/// Persists the revision's complete declared shape (node set + dependency
/// edges + structured gate conditions) beside its immutable receipt
/// (schema v2/v6, same transaction). These rows are the resolver's
/// durable input; they are write-once and are re-verified against the
/// receipt's roots at every resolution.
///
/// All declared node rows land before any edge row: the storage-layer
/// declaration triggers (schema v8) require *both* endpoints of an edge
/// to be declared in this revision, and a dependency may be declared
/// after its dependent (the declaration order is the caller's choice),
/// so an interleaved per-node insert would abort on the corrected
/// dependency predicate.
fn persist_revision_shape(
    transaction: &rusqlite::Transaction<'_>,
    plan_id: TaskPlanId,
    revision: u64,
    nodes: &[PlanNodeDeclaration],
    digests: &[([u8; 32], TaskNodeId)],
) -> Result<(), PlanStoreError> {
    for (node, (digest, node_id)) in nodes.iter().zip(digests) {
        let conditions_body = node
            .conditions
            .as_ref()
            .map(NodeConditions::canonical_bytes);
        transaction.execute(
            "INSERT INTO plan_revision_nodes (
                plan_id, revision, task_node_id, node_key, node_kind, node_digest,
                conditions_body
              ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                plan_id.as_bytes().as_slice(),
                encode_u64(revision)?,
                node_id.as_bytes().as_slice(),
                node.node_key.as_slice(),
                encode_kind(node.kind),
                digest.as_slice(),
                conditions_body.as_deref(),
            ],
        )?;
    }
    for (node, (_digest, node_id)) in nodes.iter().zip(digests) {
        for dependency in &node.dependency_keys {
            transaction.execute(
                "INSERT INTO plan_revision_edges (
                    plan_id, revision, dependent_node_id, dependency_node_id
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    plan_id.as_bytes().as_slice(),
                    encode_u64(revision)?,
                    node_id.as_bytes().as_slice(),
                    derive_node_id(plan_id, dependency).as_bytes().as_slice(),
                ],
            )?;
        }
    }
    Ok(())
}

/// Arguments for [`write_revision`] (keeps the shared write-set helper
/// under the clippy argument cap without changing commit semantics).
struct WriteRevisionArgs<'a> {
    transaction: &'a rusqlite::Transaction<'a>,
    plan_id: TaskPlanId,
    revision: u64,
    parent_digest: Option<[u8; 32]>,
    nodes_root: [u8; 32],
    dependencies_root: [u8; 32],
    plan_digest: [u8; 32],
    digests: &'a [([u8; 32], TaskNodeId)],
    nodes: &'a [PlanNodeDeclaration],
    idempotency_key: IdempotencyKey,
    applied_at_ms: u64,
}

/// The revision write set shared by both apply faces: plan head advance,
/// node upserts, the immutable receipt link, and the declared shape rows
/// (one auditable transaction; callers own the surrounding consult/replay
/// protocol and the commit).
fn write_revision(args: &WriteRevisionArgs<'_>) -> Result<PlanRevisionReceipt, PlanStoreError> {
    let WriteRevisionArgs {
        transaction,
        plan_id,
        revision,
        parent_digest,
        nodes_root,
        dependencies_root,
        plan_digest,
        digests,
        nodes,
        idempotency_key,
        applied_at_ms,
    } = args;
    if *revision == 1 {
        transaction.execute(
            "INSERT INTO plans (
                plan_id, current_revision, created_at_ms, updated_at_ms
             ) VALUES (?1, 1, ?2, ?2)",
            params![plan_id.as_bytes().as_slice(), encode_u64(*applied_at_ms)?,],
        )?;
    } else {
        transaction.execute(
            "UPDATE plans
             SET current_revision = ?2, updated_at_ms = ?3
             WHERE plan_id = ?1",
            params![
                plan_id.as_bytes().as_slice(),
                encode_u64(*revision)?,
                encode_u64(*applied_at_ms)?,
            ],
        )?;
    }

    upsert_plan_nodes(
        transaction,
        *plan_id,
        *revision,
        nodes,
        digests,
        *applied_at_ms,
    )?;

    transaction.execute(
        "INSERT INTO plan_revisions (
            plan_id, revision, idempotency_key, parent_revision_digest,
            nodes_root, dependencies_root, plan_digest,
            declared_node_count, applied_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            plan_id.as_bytes().as_slice(),
            encode_u64(*revision)?,
            idempotency_key.as_bytes().as_slice(),
            parent_digest.as_ref().map(<[u8; 32]>::as_slice),
            nodes_root.as_slice(),
            dependencies_root.as_slice(),
            plan_digest.as_slice(),
            encode_u64(nodes.len() as u64)?,
            encode_u64(*applied_at_ms)?,
        ],
    )?;
    persist_revision_shape(transaction, *plan_id, *revision, nodes, digests)?;

    Ok(PlanRevisionReceipt {
        plan_id: *plan_id,
        revision: *revision,
        idempotency_key: *idempotency_key,
        parent_revision_digest: *parent_digest,
        nodes_root: *nodes_root,
        dependencies_root: *dependencies_root,
        plan_digest: *plan_digest,
        declared_node_count: nodes.len() as u64,
        applied_at_ms: *applied_at_ms,
    })
}

/// The store-wide declared-TaskNode population this revision projects
/// and the number of new keys it would insert: every persisted
/// `plan_nodes` row (lifetime metadata, all plans) plus the declared
/// keys that carry no row under this plan yet — each new key is
/// exactly one future row (ADR-0016 决定 4 dimension).
fn projected_declared_population(
    transaction: &rusqlite::Transaction<'_>,
    plan_id: TaskPlanId,
    nodes: &[PlanNodeDeclaration],
) -> Result<(u64, u64), PlanStoreError> {
    let existing: i64 =
        transaction.query_row("SELECT COUNT(*) FROM plan_nodes", [], |row| row.get(0))?;
    let mut statement =
        transaction.prepare("SELECT node_key FROM plan_nodes WHERE plan_id = ?1")?;
    let declared_keys = statement.query_map([plan_id.as_bytes().as_slice()], |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    let mut known = std::collections::HashSet::new();
    for key in declared_keys {
        known.insert(fixed16(key?, "plan node key")?);
    }
    let fresh = u64::try_from(
        nodes
            .iter()
            .filter(|node| !known.contains(&node.node_key))
            .count(),
    )
    .map_err(|_| PlanStoreError::InvalidRequest {
        reason: "projected declared population overflows",
    })?;
    let projected = u64::try_from(existing)
        .ok()
        .and_then(|existing| existing.checked_add(fresh))
        .ok_or(PlanStoreError::InvalidRequest {
            reason: "projected declared population overflows",
        })?;
    Ok((projected, fresh))
}
