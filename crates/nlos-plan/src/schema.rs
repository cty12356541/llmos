//! Linear `SQLite` schema migration chain for the durable plan authority.
//!
//! Every `migrate_vN` advances `user_version` by exactly one step,
//! committed in a single `BEGIN IMMEDIATE` transaction so a failure
//! anywhere rolls back to a complete v(N-1) database, never a
//! half-migrated one (the `nlos-task` migration discipline).

use rusqlite::{Connection, TransactionBehavior};

use crate::PlanStoreError;

pub(crate) const SCHEMA_VERSION: i64 = 1;

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
