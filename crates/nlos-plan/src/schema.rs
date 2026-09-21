//! Linear `SQLite` schema migration chain for the durable plan authority.
//!
//! Every `migrate_vN` advances `user_version` by exactly one step,
//! committed in a single `BEGIN IMMEDIATE` transaction so a failure
//! anywhere rolls back to a complete v(N-1) database, never a
//! half-migrated one (the `nlos-task` migration discipline).

use rusqlite::{Connection, TransactionBehavior};

use crate::PlanStoreError;

pub(crate) const SCHEMA_VERSION: i64 = 7;

/// Creates the durable plan authority schema v1: the per-plan
/// `plans` current-state head (monotonic current revision), the immutable
/// `plan_revisions` digest chain (`[PLAN-DAG-001]`: every revision produces
/// a new digest and chains to its parent), the bounded per-node
/// `plan_nodes` durable metadata (`[SCALE-LOGICAL-001]`), and the immutable
/// `plan_node_transitions` state-machine vouchers.
///
/// DDL guards carry the invariants at the storage layer: revision receipts
/// and transition vouchers are immutable and undeletable, the receipt chain
/// link (parent digest + dense next revision + binding to the plan's
/// current revision) aborts any forged or skipped link, and the plan head's
/// revision is monotonic. The `[PLAN-DAG-001]` executed-node shape freeze
/// is enforced by `plan_nodes_executed_shape_frozen`.
pub(crate) fn migrate_v1(connection: &mut Connection) -> Result<(), PlanStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'plans', 'plan_revisions', 'plan_nodes', 'plan_node_transitions'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'plans_monotonic_revision',
            'plans_frozen_identity',
            'plans_no_delete',
            'plan_revisions_immutable_update',
            'plan_revisions_no_delete',
            'plan_revisions_chain_link',
            'plan_nodes_frozen_identity',
            'plan_nodes_no_delete',
            'plan_nodes_executed_shape_frozen',
            'plan_node_transitions_immutable_update',
            'plan_node_transitions_no_delete',
            'plan_node_transitions_seq_bound'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 4 && trigger_count == 12 {
        connection.pragma_update(None, "user_version", 1)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(PlanStoreError::CorruptRecord(
            "partial plan authority schema",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V1_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// Advances a v1 database to v2 (additive, one `BEGIN IMMEDIATE`
/// transaction): the per-revision declared shape set
/// (`plan_revision_nodes` / `plan_revision_edges` — the durable input the
/// Dependency Resolver resolves over, and the first face from which a
/// historical revision's node set is rebuildable) and the immutable
/// `plan_resolution_receipts` (ADR-0016 决定 5). Revisions applied before
/// this migration carry no shape rows; resolving them fails typed
/// (`RevisionShapeUnavailable`), they are never silently re-derived.
///
/// Revisions applied after v2 persist their shape rows inside the same
/// transaction as the revision receipt; the edge triggers carry the
/// same-revision declaration rule at the storage layer, and resolve-time
/// root verification binds the stored shape set to the immutable receipt
/// chain.
pub(crate) fn migrate_v2(connection: &mut Connection) -> Result<(), PlanStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'plan_revision_nodes', 'plan_revision_edges', 'plan_resolution_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'plan_revision_nodes_immutable_update',
            'plan_revision_nodes_no_delete',
            'plan_revision_edges_immutable_update',
            'plan_revision_edges_no_delete',
            'plan_revision_edges_declared_dependent',
            'plan_revision_edges_declared_dependency',
            'plan_resolution_receipts_immutable_update',
            'plan_resolution_receipts_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 3 && trigger_count == 8 {
        connection.pragma_update(None, "user_version", 2)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(PlanStoreError::CorruptRecord(
            "partial plan authority v2 schema",
        ));
    }
    let v1_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'plans', 'plan_revisions', 'plan_nodes', 'plan_node_transitions'
         )",
        [],
        |row| row.get(0),
    )?;
    if v1_tables != 4 {
        return Err(PlanStoreError::CorruptRecord(
            "plan authority v1 schema missing",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V2_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// Advances a v2 database to v3 (additive, one `BEGIN IMMEDIATE`
/// transaction): the Context residency axis of the W31-E minimal lane
/// (v0.5 §25.2.1 `ResidencyClass`, 议题 28 定案 2). `plan_nodes` gains a
/// `residency_tier` column (backfilled to `METADATA_ONLY` — before this
/// migration the authority recorded declarations only, so the
/// conservative per-node tier is metadata-only) and a
/// `residency_transition_count` sequence anchor; the immutable
/// `plan_node_residency_transitions` carries the tier-transition
/// vouchers (a separate axis from `plan_node_transitions`: own table,
/// own dense sequence, own idempotency keys). Residency columns are not
/// shape: the G1 executed-shape-freeze trigger never blocks a tier
/// move, so evicting an execution-frozen node stays legal — while the
/// storage-layer adjacency guard refuses any raw skip-tier rewrite.
pub(crate) fn migrate_v3(connection: &mut Connection) -> Result<(), PlanStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name = 'plan_node_residency_transitions'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'plan_nodes_residency_adjacent',
            'plan_node_residency_transitions_immutable_update',
            'plan_node_residency_transitions_no_delete',
            'plan_node_residency_transitions_seq_bound'
         )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 1 && trigger_count == 4 {
        connection.pragma_update(None, "user_version", 3)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(PlanStoreError::CorruptRecord(
            "partial plan authority v3 schema",
        ));
    }
    let v2_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
            'plans', 'plan_revisions', 'plan_nodes', 'plan_node_transitions',
            'plan_revision_nodes', 'plan_revision_edges', 'plan_resolution_receipts'
         )",
        [],
        |row| row.get(0),
    )?;
    if v2_tables != 7 {
        return Err(PlanStoreError::CorruptRecord(
            "plan authority v2 schema missing",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V3_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// Advances a v3 database to v4 (additive, one `BEGIN IMMEDIATE`
/// transaction): the W31-A lazy-materialization gate (ADR-0016 G3,
/// B4-4). `plan_materialization_requests` carries the durable
/// verify-then-commit protocol rows — `PENDING` requests the Task-side
/// consumption path answers, `APPROVED` rows carrying the admission
/// facts, `REJECTED` rows carrying the typed window-shrink reason. Two
/// storage-layer invariants close the G3 falsification paths:
/// - `plan_node_transitions_materializing_gated`: no voucher may enter
///   `MATERIALIZING` unless the node has an `APPROVED` request — the raw
///   `record_node_transition` face is closed for that edge;
/// - `plan_materialization_requests_one_pending`: at most one in-flight
///   request per node, and resolution is one-way (`PENDING` is the only
///   updatable status; identity columns are frozen).
pub(crate) fn migrate_v4(connection: &mut Connection) -> Result<(), PlanStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type='table' AND name = 'plan_materialization_requests'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'plan_materialization_requests_no_delete',
            'plan_materialization_requests_pending_shape',
            'plan_materialization_requests_resolve_once',
            'plan_materialization_requests_resolved_shape',
            'plan_node_transitions_materializing_gated'
          )",
        [],
        |row| row.get(0),
    )?;
    let index_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type='index' AND name = 'plan_materialization_requests_one_pending'",
        [],
        |row| row.get(0),
    )?;
    if table_count == 1 && trigger_count == 5 && index_count == 1 {
        connection.pragma_update(None, "user_version", 4)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 || index_count != 0 {
        return Err(PlanStoreError::CorruptRecord(
            "partial plan authority v4 schema",
        ));
    }
    let v3_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type='table' AND name IN (
            'plans', 'plan_revisions', 'plan_nodes', 'plan_node_transitions',
            'plan_revision_nodes', 'plan_revision_edges', 'plan_resolution_receipts',
            'plan_node_residency_transitions'
          )",
        [],
        |row| row.get(0),
    )?;
    if v3_tables != 8 {
        return Err(PlanStoreError::CorruptRecord(
            "plan authority v3 schema missing",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V4_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// Advances a v4 database to v5 (additive, one `BEGIN IMMEDIATE`
/// transaction): the immutable `ecosystem_resolution_receipts` of the
/// resolver's ecosystem selector half (W36-P7, ADR-0016 决定 5 second
/// half; `[PLAN-DEPENDENCY-001]`). One row is one durable
/// generation-carrying handle: the pinned generation and content digest
/// observed when the resolution committed, never floated by later
/// source generations, and re-derived binding-checked on readback. The
/// `entity_kind` domain is deliberately not a `CHECK` IN-list: the
/// closed enum's decode face ([`crate::PlanStoreError::EcosystemKindUnknown`])
/// is the fail-closed guard, so a row with a kind outside this build's
/// enum (e.g. written by a later schema version) is refused typed on
/// read instead of aborting at insert-time domain policing.
pub(crate) fn migrate_v5(connection: &mut Connection) -> Result<(), PlanStoreError> {
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type='table' AND name = 'ecosystem_resolution_receipts'",
        [],
        |row| row.get(0),
    )?;
    let trigger_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN (
            'ecosystem_resolution_receipts_immutable_update',
            'ecosystem_resolution_receipts_no_delete'
          )",
        [],
        |row| row.get(0),
    )?;
    if table_count == 1 && trigger_count == 2 {
        connection.pragma_update(None, "user_version", 5)?;
        return Ok(());
    }
    if table_count != 0 || trigger_count != 0 {
        return Err(PlanStoreError::CorruptRecord(
            "partial plan authority v5 schema",
        ));
    }
    let v4_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type='table' AND name IN (
            'plans', 'plan_revisions', 'plan_nodes', 'plan_node_transitions',
            'plan_revision_nodes', 'plan_revision_edges', 'plan_resolution_receipts',
            'plan_node_residency_transitions', 'plan_materialization_requests'
          )",
        [],
        |row| row.get(0),
    )?;
    if v4_tables != 9 {
        return Err(PlanStoreError::CorruptRecord(
            "plan authority v4 schema missing",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V5_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// Advances a v5 database to v6 (additive, one `BEGIN IMMEDIATE`
/// transaction): the structured G3 gate conditions column (W36-P7; W31-G
/// §8.2.2). `plan_revision_nodes` gains a nullable `conditions_body`
/// holding the canonical [`crate::NodeConditions`] encoding; `NULL` is
/// the digest-only legacy form, which stays fully accepted — pre-v6
/// rows and re-declarations hash bit-identically (the node digest
/// formula's absence branch appends nothing). Existing rows backfill to
/// `NULL` (their conditions rode the digest slots by convention), and
/// the write-once UPDATE/DELETE triggers keep covering the new column
/// with the row.
pub(crate) fn migrate_v6(connection: &mut Connection) -> Result<(), PlanStoreError> {
    let column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('plan_revision_nodes')
          WHERE name = 'conditions_body'",
        [],
        |row| row.get(0),
    )?;
    if column_count == 1 {
        connection.pragma_update(None, "user_version", 6)?;
        return Ok(());
    }
    if column_count != 0 {
        return Err(PlanStoreError::CorruptRecord(
            "partial plan authority v6 schema",
        ));
    }
    let v5_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type='table' AND name IN (
            'plans', 'plan_revisions', 'plan_nodes', 'plan_node_transitions',
            'plan_revision_nodes', 'plan_revision_edges', 'plan_resolution_receipts',
            'plan_node_residency_transitions', 'plan_materialization_requests',
            'ecosystem_resolution_receipts'
          )",
        [],
        |row| row.get(0),
    )?;
    if v5_tables != 10 {
        return Err(PlanStoreError::CorruptRecord(
            "plan authority v5 schema missing",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V6_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// Advances a v6 database to v7 (additive, one `BEGIN IMMEDIATE`
/// transaction): the PINNED overlay (W36-P8; W31-G §8.2.6). `plan_nodes`
/// gains `pinned` (default unpinned) and `pin_transition_count`; the
/// immutable `plan_node_pin_transitions` table carries pin/unpin
/// vouchers. The 5-tier residency CHECK is untouched — PINNED is not a
/// sixth discriminant.
pub(crate) fn migrate_v7(connection: &mut Connection) -> Result<(), PlanStoreError> {
    let column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('plan_nodes')
          WHERE name = 'pinned'",
        [],
        |row| row.get(0),
    )?;
    if column_count == 1 {
        connection.pragma_update(None, "user_version", 7)?;
        return Ok(());
    }
    if column_count != 0 {
        return Err(PlanStoreError::CorruptRecord(
            "partial plan authority v7 schema",
        ));
    }
    let v6_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type='table' AND name IN (
            'plans', 'plan_revisions', 'plan_nodes', 'plan_node_transitions',
            'plan_revision_nodes', 'plan_revision_edges', 'plan_resolution_receipts',
            'plan_node_residency_transitions', 'plan_materialization_requests',
            'ecosystem_resolution_receipts'
          )",
        [],
        |row| row.get(0),
    )?;
    if v6_tables != 10 {
        return Err(PlanStoreError::CorruptRecord(
            "plan authority v6 schema missing",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V7_SQL)?;
    transaction.commit()?;
    Ok(())
}

pub(crate) const SCHEMA_V1_SQL: &str = "CREATE TABLE plans (
    plan_id BLOB PRIMARY KEY NOT NULL CHECK(length(plan_id) = 16),
    current_revision INTEGER NOT NULL CHECK(current_revision >= 1),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE TABLE plan_revisions (
    plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
    revision INTEGER NOT NULL CHECK(revision >= 1),
    idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
    parent_revision_digest BLOB CHECK(
        parent_revision_digest IS NULL OR length(parent_revision_digest) = 32
    ),
    nodes_root BLOB NOT NULL CHECK(length(nodes_root) = 32),
    dependencies_root BLOB NOT NULL CHECK(length(dependencies_root) = 32),
    plan_digest BLOB NOT NULL UNIQUE CHECK(length(plan_digest) = 32),
    declared_node_count INTEGER NOT NULL CHECK(declared_node_count >= 1),
    applied_at_ms INTEGER NOT NULL CHECK(applied_at_ms >= 0),
    PRIMARY KEY(plan_id, revision),
    FOREIGN KEY(plan_id) REFERENCES plans(plan_id)
) STRICT;

CREATE TABLE plan_nodes (
    plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
    task_node_id BLOB NOT NULL CHECK(length(task_node_id) = 16),
    node_key BLOB NOT NULL CHECK(length(node_key) = 16),
    node_kind INTEGER NOT NULL CHECK(node_kind IN (1, 2)),
    declared_revision INTEGER NOT NULL CHECK(declared_revision >= 1),
    node_digest BLOB NOT NULL CHECK(length(node_digest) = 32),
    node_state INTEGER NOT NULL CHECK(node_state BETWEEN 1 AND 13),
    transition_count INTEGER NOT NULL CHECK(transition_count >= 0),
    first_declared_at_ms INTEGER NOT NULL CHECK(first_declared_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= first_declared_at_ms),
    PRIMARY KEY(plan_id, task_node_id),
    UNIQUE(plan_id, node_key),
    FOREIGN KEY(plan_id) REFERENCES plans(plan_id)
) STRICT;

CREATE TABLE plan_node_transitions (
    voucher_id BLOB PRIMARY KEY NOT NULL CHECK(length(voucher_id) = 16),
    idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
    plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
    task_node_id BLOB NOT NULL CHECK(length(task_node_id) = 16),
    transition_seq INTEGER NOT NULL CHECK(transition_seq >= 1),
    from_state INTEGER NOT NULL CHECK(from_state BETWEEN 1 AND 13),
    to_state INTEGER NOT NULL CHECK(to_state BETWEEN 1 AND 13),
    observed_revision INTEGER NOT NULL CHECK(observed_revision >= 1),
    transitioned_at_ms INTEGER NOT NULL CHECK(transitioned_at_ms >= 0),
    UNIQUE(plan_id, task_node_id, transition_seq),
    FOREIGN KEY(plan_id, task_node_id) REFERENCES plan_nodes(plan_id, task_node_id)
) STRICT;

CREATE TRIGGER plans_monotonic_revision
BEFORE UPDATE ON plans
WHEN NEW.current_revision < OLD.current_revision
BEGIN
    SELECT RAISE(ABORT, 'plan current revision is monotonic');
END;
CREATE TRIGGER plans_frozen_identity
BEFORE UPDATE ON plans
WHEN NEW.plan_id != OLD.plan_id
BEGIN
    SELECT RAISE(ABORT, 'plan identity is frozen');
END;
CREATE TRIGGER plans_no_delete
BEFORE DELETE ON plans BEGIN
    SELECT RAISE(ABORT, 'plan head is durable');
END;

CREATE TRIGGER plan_revisions_immutable_update
BEFORE UPDATE ON plan_revisions BEGIN
    SELECT RAISE(ABORT, 'plan revision receipt is immutable');
END;
CREATE TRIGGER plan_revisions_no_delete
BEFORE DELETE ON plan_revisions BEGIN
    SELECT RAISE(ABORT, 'plan revision receipt is durable');
END;
CREATE TRIGGER plan_revisions_chain_link
AFTER INSERT ON plan_revisions
BEGIN
    SELECT CASE
        WHEN NEW.revision != (
            SELECT current_revision FROM plans WHERE plan_id = NEW.plan_id
        ) THEN RAISE(ABORT, 'plan revision is not bound to the current plan revision')
        WHEN NEW.revision = 1 AND NEW.parent_revision_digest IS NOT NULL THEN
            RAISE(ABORT, 'initial plan revision has no parent digest')
        WHEN NEW.revision > 1 AND (
            NEW.parent_revision_digest IS NULL
            OR (SELECT plan_digest FROM plan_revisions
                WHERE plan_id = NEW.plan_id AND revision = NEW.revision - 1) IS NULL
            OR NEW.parent_revision_digest != (SELECT plan_digest FROM plan_revisions
                WHERE plan_id = NEW.plan_id AND revision = NEW.revision - 1)
        ) THEN RAISE(ABORT, 'plan revision parent digest does not chain')
    END;
END;

CREATE TRIGGER plan_nodes_frozen_identity
BEFORE UPDATE ON plan_nodes
WHEN NEW.plan_id != OLD.plan_id
    OR NEW.task_node_id != OLD.task_node_id
    OR NEW.node_key != OLD.node_key
BEGIN
    SELECT RAISE(ABORT, 'plan node identity is frozen');
END;
CREATE TRIGGER plan_nodes_no_delete
BEFORE DELETE ON plan_nodes BEGIN
    SELECT RAISE(ABORT, 'plan node metadata is durable');
END;
CREATE TRIGGER plan_nodes_executed_shape_frozen
BEFORE UPDATE ON plan_nodes
WHEN OLD.node_state >= 6 AND (
    NEW.declared_revision != OLD.declared_revision
    OR NEW.node_digest != OLD.node_digest
    OR NEW.node_kind != OLD.node_kind
)
BEGIN
    SELECT RAISE(ABORT, 'executed plan node shape is frozen (PLAN-DAG-001)');
END;

CREATE TRIGGER plan_node_transitions_immutable_update
BEFORE UPDATE ON plan_node_transitions BEGIN
    SELECT RAISE(ABORT, 'plan node transition voucher is immutable');
END;
CREATE TRIGGER plan_node_transitions_no_delete
BEFORE DELETE ON plan_node_transitions BEGIN
    SELECT RAISE(ABORT, 'plan node transition voucher is durable');
END;
CREATE TRIGGER plan_node_transitions_seq_bound
AFTER INSERT ON plan_node_transitions
WHEN NEW.transition_seq != (
    SELECT transition_count + 1 FROM plan_nodes
    WHERE plan_id = NEW.plan_id AND task_node_id = NEW.task_node_id
)
BEGIN
    SELECT RAISE(ABORT, 'transition voucher is not the next dense sequence');
END;

PRAGMA user_version = 1;";

pub(crate) const SCHEMA_V2_SQL: &str = "CREATE TABLE plan_revision_nodes (
    plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
    revision INTEGER NOT NULL CHECK(revision >= 1),
    task_node_id BLOB NOT NULL CHECK(length(task_node_id) = 16),
    node_key BLOB NOT NULL CHECK(length(node_key) = 16),
    node_kind INTEGER NOT NULL CHECK(node_kind IN (1, 2)),
    node_digest BLOB NOT NULL CHECK(length(node_digest) = 32),
    PRIMARY KEY(plan_id, revision, task_node_id),
    UNIQUE(plan_id, revision, node_key),
    FOREIGN KEY(plan_id, revision) REFERENCES plan_revisions(plan_id, revision)
) STRICT;

CREATE TABLE plan_revision_edges (
    plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
    revision INTEGER NOT NULL CHECK(revision >= 1),
    dependent_node_id BLOB NOT NULL CHECK(length(dependent_node_id) = 16),
    dependency_node_id BLOB NOT NULL CHECK(length(dependency_node_id) = 16),
    PRIMARY KEY(plan_id, revision, dependent_node_id, dependency_node_id),
    FOREIGN KEY(plan_id, revision) REFERENCES plan_revisions(plan_id, revision)
) STRICT;

CREATE TABLE plan_resolution_receipts (
    resolution_id BLOB PRIMARY KEY NOT NULL CHECK(length(resolution_id) = 16),
    idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
    plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
    revision INTEGER NOT NULL CHECK(revision >= 1),
    plan_digest BLOB NOT NULL CHECK(length(plan_digest) = 32),
    resolved_order BLOB NOT NULL CHECK(
        length(resolved_order) >= 16 AND length(resolved_order) % 16 = 0
    ),
    resolved_edges BLOB NOT NULL CHECK(length(resolved_edges) % 32 = 0),
    resolution_digest BLOB NOT NULL CHECK(length(resolution_digest) = 32),
    resolved_at_ms INTEGER NOT NULL CHECK(resolved_at_ms >= 0),
    FOREIGN KEY(plan_id, revision) REFERENCES plan_revisions(plan_id, revision)
) STRICT;

CREATE TRIGGER plan_revision_nodes_immutable_update
BEFORE UPDATE ON plan_revision_nodes BEGIN
    SELECT RAISE(ABORT, 'plan revision declared node is immutable');
END;
CREATE TRIGGER plan_revision_nodes_no_delete
BEFORE DELETE ON plan_revision_nodes BEGIN
    SELECT RAISE(ABORT, 'plan revision declared node is durable');
END;

CREATE TRIGGER plan_revision_edges_immutable_update
BEFORE UPDATE ON plan_revision_edges BEGIN
    SELECT RAISE(ABORT, 'plan revision dependency edge is immutable');
END;
CREATE TRIGGER plan_revision_edges_no_delete
BEFORE DELETE ON plan_revision_edges BEGIN
    SELECT RAISE(ABORT, 'plan revision dependency edge is durable');
END;
CREATE TRIGGER plan_revision_edges_declared_dependent
AFTER INSERT ON plan_revision_edges
WHEN NOT EXISTS (
    SELECT 1 FROM plan_revision_nodes
    WHERE plan_id = NEW.plan_id AND revision = NEW.revision
      AND task_node_id = NEW.dependent_node_id
)
BEGIN
    SELECT RAISE(ABORT, 'dependency edge dependent is not declared in this revision');
END;
CREATE TRIGGER plan_revision_edges_declared_dependency
AFTER INSERT ON plan_revision_edges
WHEN NOT EXISTS (
    SELECT 1 FROM plan_revision_nodes
    WHERE plan_id = NEW.plan_id AND revision = NEW.revision
      AND task_node_id = NEW.dependency_node_id
)
BEGIN
    SELECT RAISE(ABORT, 'dependency edge dependency is not declared in this revision');
END;

CREATE TRIGGER plan_resolution_receipts_immutable_update
BEFORE UPDATE ON plan_resolution_receipts BEGIN
    SELECT RAISE(ABORT, 'plan resolution receipt is immutable');
END;
CREATE TRIGGER plan_resolution_receipts_no_delete
BEFORE DELETE ON plan_resolution_receipts BEGIN
    SELECT RAISE(ABORT, 'plan resolution receipt is durable');
END;

PRAGMA user_version = 2;";

pub(crate) const SCHEMA_V3_SQL: &str = "ALTER TABLE plan_nodes
    ADD COLUMN residency_tier INTEGER NOT NULL DEFAULT 1
    CHECK(residency_tier BETWEEN 1 AND 5);
ALTER TABLE plan_nodes
    ADD COLUMN residency_transition_count INTEGER NOT NULL DEFAULT 0
    CHECK(residency_transition_count >= 0);

CREATE TABLE plan_node_residency_transitions (
    voucher_id BLOB PRIMARY KEY NOT NULL CHECK(length(voucher_id) = 16),
    idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
    plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
    task_node_id BLOB NOT NULL CHECK(length(task_node_id) = 16),
    transition_seq INTEGER NOT NULL CHECK(transition_seq >= 1),
    from_tier INTEGER NOT NULL CHECK(from_tier BETWEEN 1 AND 5),
    to_tier INTEGER NOT NULL CHECK(to_tier BETWEEN 1 AND 5),
    observed_revision INTEGER NOT NULL CHECK(observed_revision >= 1),
    transitioned_at_ms INTEGER NOT NULL CHECK(transitioned_at_ms >= 0),
    UNIQUE(plan_id, task_node_id, transition_seq),
    FOREIGN KEY(plan_id, task_node_id) REFERENCES plan_nodes(plan_id, task_node_id)
) STRICT;

CREATE TRIGGER plan_nodes_residency_adjacent
BEFORE UPDATE OF residency_tier ON plan_nodes
WHEN NEW.residency_tier != OLD.residency_tier
    AND ABS(NEW.residency_tier - OLD.residency_tier) != 1
BEGIN
    SELECT RAISE(ABORT, 'residency tier transitions must be adjacent');
END;
CREATE TRIGGER plan_node_residency_transitions_immutable_update
BEFORE UPDATE ON plan_node_residency_transitions BEGIN
    SELECT RAISE(ABORT, 'plan node residency voucher is immutable');
END;
CREATE TRIGGER plan_node_residency_transitions_no_delete
BEFORE DELETE ON plan_node_residency_transitions BEGIN
    SELECT RAISE(ABORT, 'plan node residency voucher is durable');
END;
CREATE TRIGGER plan_node_residency_transitions_seq_bound
AFTER INSERT ON plan_node_residency_transitions
WHEN NEW.transition_seq != (
    SELECT residency_transition_count + 1 FROM plan_nodes
    WHERE plan_id = NEW.plan_id AND task_node_id = NEW.task_node_id
)
BEGIN
    SELECT RAISE(ABORT, 'residency voucher is not the next dense sequence');
END;

PRAGMA user_version = 3;";

pub(crate) const SCHEMA_V4_SQL: &str = "CREATE TABLE plan_materialization_requests (
    request_id BLOB PRIMARY KEY NOT NULL CHECK(length(request_id) = 16),
    idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
    plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
    task_node_id BLOB NOT NULL CHECK(length(task_node_id) = 16),
    observed_declared_revision INTEGER NOT NULL CHECK(observed_declared_revision >= 1),
    status INTEGER NOT NULL CHECK(status IN (1, 2, 3)),
    admission_profile TEXT,
    admitted_task_nodes INTEGER,
    admitted_active_working_set INTEGER,
    approved_voucher_id BLOB CHECK(
        approved_voucher_id IS NULL OR length(approved_voucher_id) = 16
    ),
    rejection_kind INTEGER NOT NULL DEFAULT 0 CHECK(rejection_kind IN (0, 1, 2)),
    rejection_profile TEXT,
    rejection_observed INTEGER,
    rejection_cap INTEGER,
    requested_at_ms INTEGER NOT NULL CHECK(requested_at_ms >= 0),
    resolved_at_ms INTEGER CHECK(resolved_at_ms IS NULL OR resolved_at_ms >= 0),
    FOREIGN KEY(plan_id, task_node_id) REFERENCES plan_nodes(plan_id, task_node_id)
) STRICT;

CREATE TRIGGER plan_materialization_requests_no_delete
BEFORE DELETE ON plan_materialization_requests BEGIN
    SELECT RAISE(ABORT, 'materialization request is durable');
END;

CREATE TRIGGER plan_materialization_requests_pending_shape
AFTER INSERT ON plan_materialization_requests
WHEN NEW.status != 1
    OR NEW.admission_profile IS NOT NULL
    OR NEW.admitted_task_nodes IS NOT NULL
    OR NEW.admitted_active_working_set IS NOT NULL
    OR NEW.approved_voucher_id IS NOT NULL
    OR NEW.rejection_kind != 0
    OR NEW.rejection_profile IS NOT NULL
    OR NEW.rejection_observed IS NOT NULL
    OR NEW.rejection_cap IS NOT NULL
    OR NEW.resolved_at_ms IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'new materialization requests must be pending');
END;

CREATE TRIGGER plan_materialization_requests_resolve_once
BEFORE UPDATE ON plan_materialization_requests
WHEN OLD.status != 1
    OR NEW.status NOT IN (2, 3)
    OR NEW.request_id != OLD.request_id
    OR NEW.idempotency_key != OLD.idempotency_key
    OR NEW.plan_id != OLD.plan_id
    OR NEW.task_node_id != OLD.task_node_id
    OR NEW.observed_declared_revision != OLD.observed_declared_revision
    OR NEW.requested_at_ms != OLD.requested_at_ms
BEGIN
    SELECT RAISE(ABORT, 'materialization request resolution is one-way');
END;

CREATE TRIGGER plan_materialization_requests_resolved_shape
BEFORE UPDATE ON plan_materialization_requests
WHEN (NEW.status = 2 AND (
        NEW.admission_profile IS NULL
        OR NEW.admitted_task_nodes IS NULL
        OR NEW.admitted_active_working_set IS NULL
        OR NEW.approved_voucher_id IS NULL
        OR NEW.rejection_kind != 0
        OR NEW.resolved_at_ms IS NULL))
  OR (NEW.status = 3 AND (
        NEW.rejection_kind NOT IN (1, 2)
        OR NEW.admission_profile IS NOT NULL
        OR NEW.admitted_task_nodes IS NOT NULL
        OR NEW.admitted_active_working_set IS NOT NULL
        OR NEW.approved_voucher_id IS NOT NULL
        OR NEW.resolved_at_ms IS NULL))
BEGIN
    SELECT RAISE(ABORT, 'materialization resolution shape mismatch');
END;

CREATE UNIQUE INDEX plan_materialization_requests_one_pending
ON plan_materialization_requests(task_node_id) WHERE status = 1;

CREATE TRIGGER plan_node_transitions_materializing_gated
BEFORE INSERT ON plan_node_transitions
WHEN NEW.to_state = 6 AND NOT EXISTS (
    SELECT 1 FROM plan_materialization_requests
    WHERE plan_id = NEW.plan_id
      AND task_node_id = NEW.task_node_id
      AND status = 2
)
BEGIN
    SELECT RAISE(ABORT, 'MATERIALIZING entry requires a gate-approved materialization request');
END;

PRAGMA user_version = 4;";

pub(crate) const SCHEMA_V5_SQL: &str = "CREATE TABLE ecosystem_resolution_receipts (
    resolution_id BLOB PRIMARY KEY NOT NULL CHECK(length(resolution_id) = 16),
    idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
    entity_kind INTEGER NOT NULL,
    entity_id BLOB NOT NULL CHECK(length(entity_id) = 16),
    generation INTEGER NOT NULL CHECK(generation >= 1),
    content_digest BLOB NOT NULL CHECK(length(content_digest) = 32),
    resolved_at_ms INTEGER NOT NULL CHECK(resolved_at_ms >= 0)
) STRICT;

CREATE TRIGGER ecosystem_resolution_receipts_immutable_update
BEFORE UPDATE ON ecosystem_resolution_receipts BEGIN
    SELECT RAISE(ABORT, 'ecosystem resolution receipt is immutable');
END;
CREATE TRIGGER ecosystem_resolution_receipts_no_delete
BEFORE DELETE ON ecosystem_resolution_receipts BEGIN
    SELECT RAISE(ABORT, 'ecosystem resolution receipt is durable');
END;

PRAGMA user_version = 5;";

pub(crate) const SCHEMA_V6_SQL: &str = "ALTER TABLE plan_revision_nodes
    ADD COLUMN conditions_body BLOB;

PRAGMA user_version = 6;";

pub(crate) const SCHEMA_V7_SQL: &str = "ALTER TABLE plan_nodes
    ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0
    CHECK(pinned IN (0, 1));
ALTER TABLE plan_nodes
    ADD COLUMN pin_transition_count INTEGER NOT NULL DEFAULT 0
    CHECK(pin_transition_count >= 0);

CREATE TABLE plan_node_pin_transitions (
    voucher_id BLOB PRIMARY KEY NOT NULL CHECK(length(voucher_id) = 16),
    idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
    plan_id BLOB NOT NULL CHECK(length(plan_id) = 16),
    task_node_id BLOB NOT NULL CHECK(length(task_node_id) = 16),
    transition_seq INTEGER NOT NULL CHECK(transition_seq >= 1),
    from_pinned INTEGER NOT NULL CHECK(from_pinned IN (0, 1)),
    to_pinned INTEGER NOT NULL CHECK(to_pinned IN (0, 1)),
    observed_revision INTEGER NOT NULL CHECK(observed_revision >= 1),
    transitioned_at_ms INTEGER NOT NULL CHECK(transitioned_at_ms >= 0),
    UNIQUE(plan_id, task_node_id, transition_seq),
    FOREIGN KEY(plan_id, task_node_id) REFERENCES plan_nodes(plan_id, task_node_id)
) STRICT;

CREATE TRIGGER plan_node_pin_transitions_immutable_update
BEFORE UPDATE ON plan_node_pin_transitions BEGIN
    SELECT RAISE(ABORT, 'plan node pin voucher is immutable');
END;
CREATE TRIGGER plan_node_pin_transitions_no_delete
BEFORE DELETE ON plan_node_pin_transitions BEGIN
    SELECT RAISE(ABORT, 'plan node pin voucher is durable');
END;
CREATE TRIGGER plan_node_pin_transitions_seq_bound
AFTER INSERT ON plan_node_pin_transitions
WHEN NEW.transition_seq != (
    SELECT pin_transition_count + 1 FROM plan_nodes
    WHERE plan_id = NEW.plan_id AND task_node_id = NEW.task_node_id
)
BEGIN
    SELECT RAISE(ABORT, 'pin voucher is not the next dense sequence');
END;

PRAGMA user_version = 7;";
