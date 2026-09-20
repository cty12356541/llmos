//! Linear `SQLite` schema migration chain for the durable plan authority.
//!
//! Every `migrate_vN` advances `user_version` by exactly one step,
//! committed in a single `BEGIN IMMEDIATE` transaction so a failure
//! anywhere rolls back to a complete v(N-1) database, never a
//! half-migrated one (the `nlos-task` migration discipline).

use rusqlite::{Connection, TransactionBehavior};

use crate::PlanStoreError;

pub(crate) const SCHEMA_VERSION: i64 = 2;

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
