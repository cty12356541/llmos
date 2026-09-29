//! Storage-layer edge-declaration guard tests for schema v8: the
//! corrected `plan_revision_edges_declared_dependency` predicate (the
//! v2 body probed `NEW.dependent_node_id`, so an edge whose dependency
//! endpoint was not declared in the same revision survived a raw SQL
//! insert), the nodes-before-edges apply insert order the corrected
//! predicate requires, and the v7→v8 trigger-rebuild migration paths
//! (defective body rebuilt, corrected body idempotently restamped,
//! durable data untouched).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, PlanNodeDeclaration, PlanNodeKind, PlanRevisionSelector,
    ResolvePlanRequest, SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, TaskPlanId};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-plan-schema-edge-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node(key: u8, payload: u8, dependencies: &[u8]) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [key; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [payload; 32],
        dependency_keys: dependencies.iter().map(|key| [*key; 16]).collect(),
        input_selectors_digest: [payload; 32],
        output_contract_digest: [payload; 32],
        policy_digest: [payload; 32],
        resource_ceiling_digest: [payload; 32],
        conditions: None,
    }
}

fn revision_request(
    plan_id: Option<TaskPlanId>,
    nodes: Vec<PlanNodeDeclaration>,
    key: u8,
) -> ApplyPlanRevisionRequest {
    ApplyPlanRevisionRequest {
        plan_id,
        nodes,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        applied_at_ms: 1_000,
    }
}

fn resolve_current(authority: &SqlitePlanAuthority, plan_id: TaskPlanId, key: u8) {
    let decision = authority
        .resolve_plan(ResolvePlanRequest {
            selector: PlanRevisionSelector::Current(plan_id),
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            resolved_at_ms: 3_000,
        })
        .expect("resolve current head");
    drop(decision.handle());
}

fn node_id_of(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_key: [u8; 16],
) -> nlos_types::TaskNodeId {
    authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == node_key)
        .expect("declared node exists")
        .node_id
}

fn user_version(db_path: &std::path::Path) -> i64 {
    let raw = Connection::open(db_path).expect("raw");
    raw.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version")
}

fn scalar(db_path: &std::path::Path, sql: &str) -> i64 {
    let raw = Connection::open(db_path).expect("raw");
    raw.query_row(sql, [], |row| row.get(0))
        .expect("scalar count")
}

fn insert_edge(
    db_path: &std::path::Path,
    plan_id: TaskPlanId,
    revision: i64,
    dependent: &[u8],
    dependency: &[u8],
) -> Result<usize, rusqlite::Error> {
    let raw = Connection::open(db_path).expect("raw writer");
    raw.execute(
        "INSERT INTO plan_revision_edges (
            plan_id, revision, dependent_node_id, dependency_node_id
         ) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            plan_id.as_bytes().as_slice(),
            revision,
            dependent,
            dependency,
        ],
    )
}

/// The exact v2 trigger pair as every pre-v8 build shipped it: the
/// dependency trigger's WHEN clause probed `NEW.dependent_node_id` —
/// the copy-paste defect v8 rebuilds.
const V7_DEFECTIVE_EDGE_TRIGGERS: &str =
    "DROP TRIGGER IF EXISTS plan_revision_edges_declared_dependent;
DROP TRIGGER IF EXISTS plan_revision_edges_declared_dependency;
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
      AND task_node_id = NEW.dependent_node_id
)
BEGIN
    SELECT RAISE(ABORT, 'dependency edge dependency is not declared in this revision');
END;";

/// The corrected triggers abort a raw SQL edge insert whose dependency
/// (and, symmetrically, dependent) endpoint is not declared in the same
/// revision — the storage-layer guard the defective v2 body left at
/// zero enforcement. An edge with both endpoints declared still lands.
#[test]
fn fresh_v8_aborts_edges_with_undeclared_endpoints() {
    let root = Root::new("ddl-abort");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    assert_eq!(user_version(&db_path), 8);
    let plan_id = authority
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![node(0x0a, 0x11, &[]), node(0x0b, 0x22, &[0x0a])],
            0x01,
        ))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let id_a = node_id_of(&authority, plan_id, [0x0a; 16]);
    let id_b = node_id_of(&authority, plan_id, [0x0b; 16]);
    drop(authority);

    let undeclared = [0xeeu8; 16];

    let error = insert_edge(&db_path, plan_id, 1, id_a.as_bytes(), &undeclared)
        .expect_err("undeclared dependency endpoint must abort");
    assert!(
        error
            .to_string()
            .contains("dependency edge dependency is not declared in this revision"),
        "unexpected error: {error}"
    );

    let error = insert_edge(&db_path, plan_id, 1, &undeclared, id_a.as_bytes())
        .expect_err("undeclared dependent endpoint must abort");
    assert!(
        error
            .to_string()
            .contains("dependency edge dependent is not declared in this revision"),
        "unexpected error: {error}"
    );

    // Positive control: an edge with both endpoints declared that the
    // declaration itself did not carry still lands (the resolver's root
    // re-verification, not the triggers, is what fences it).
    insert_edge(&db_path, plan_id, 1, id_a.as_bytes(), id_b.as_bytes())
        .expect("declared endpoints insert");
}

/// The apply path persists shape rows nodes-first-then-edges, so a
/// declaration may name a dependency that appears later in the node
/// list: the edge row's dependency endpoint is already declared when
/// the edge lands. Resolution re-verifies the stored shape.
#[test]
fn apply_supports_dependency_declared_after_dependent() {
    let root = Root::new("late-dependency");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    let plan_id = authority
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![
                node(0x0a, 0x11, &[0x0c]),
                node(0x0b, 0x22, &[]),
                node(0x0c, 0x33, &[0x0b]),
            ],
            0x01,
        ))
        .expect("revision with forward dependencies")
        .receipt()
        .plan_id;
    resolve_current(&authority, plan_id, 0x51);
}

/// A legacy v7 database (its dependency trigger carries the defective
/// body) upgrades to v8 on open: both edge triggers are rebuilt with
/// the corrected predicate, `user_version` reaches 8, every durable
/// row survives untouched (the probe edge the defective v7 body let
/// through included — edges are durable), the resolver still resolves
/// the current head, and the corrected guard now aborts the raw
/// undeclared-dependency insert the defective v7 body accepted.
#[test]
fn legacy_v7_defective_triggers_rebuilt_losslessly() {
    let root = Root::new("v7-rebuild");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    let plan_id = authority
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![
                node(0x0a, 0x11, &[]),
                node(0x0b, 0x22, &[0x0a]),
                node(0x0d, 0x44, &[0x0b]),
            ],
            0x01,
        ))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let id_a = node_id_of(&authority, plan_id, [0x0a; 16]);
    // Revision 2 becomes the current head: the probe edge below rides
    // revision 1, whose shape the current-head resolution no longer
    // reads (and the durable-edge no-delete trigger forbids removing).
    authority
        .apply_plan_revision_ungated(revision_request(
            Some(plan_id),
            vec![
                node(0x0a, 0x99, &[]),
                node(0x0b, 0x88, &[0x0a]),
                node(0x0d, 0x77, &[0x0b]),
            ],
            0x02,
        ))
        .expect("revision 2");
    drop(authority);

    // Regress the database to v7 exactly as a pre-v8 build left it.
    let raw = Connection::open(&db_path).expect("raw writer");
    raw.execute_batch(V7_DEFECTIVE_EDGE_TRIGGERS)
        .expect("install v7 defective triggers");
    raw.pragma_update(None, "user_version", 7)
        .expect("stamp v7");
    drop(raw);
    assert_eq!(user_version(&db_path), 7);

    // The defective v7 body lets the undeclared-dependency edge into
    // revision 1: both misdirected predicates probe the declared
    // dependent endpoint and enforce nothing on the dependency side.
    let undeclared = [0xeeu8; 16];
    insert_edge(&db_path, plan_id, 1, id_a.as_bytes(), &undeclared)
        .expect("defective v7 body enforces nothing on the dependency endpoint");
    let counts_before_upgrade = (
        scalar(&db_path, "SELECT COUNT(*) FROM plans"),
        scalar(&db_path, "SELECT COUNT(*) FROM plan_revisions"),
        scalar(&db_path, "SELECT COUNT(*) FROM plan_nodes"),
        scalar(&db_path, "SELECT COUNT(*) FROM plan_revision_nodes"),
        scalar(&db_path, "SELECT COUNT(*) FROM plan_revision_edges"),
    );

    let upgraded = SqlitePlanAuthority::open(&db_path).expect("open upgrades v7 to v8");
    assert_eq!(user_version(&db_path), 8);
    assert_eq!(
        (
            scalar(&db_path, "SELECT COUNT(*) FROM plans"),
            scalar(&db_path, "SELECT COUNT(*) FROM plan_revisions"),
            scalar(&db_path, "SELECT COUNT(*) FROM plan_nodes"),
            scalar(&db_path, "SELECT COUNT(*) FROM plan_revision_nodes"),
            scalar(&db_path, "SELECT COUNT(*) FROM plan_revision_edges"),
        ),
        counts_before_upgrade,
        "the v8 rebuild touches no durable row"
    );
    resolve_current(&upgraded, plan_id, 0x51);
    drop(upgraded);

    // The rebuilt guard aborts the same undeclared-dependency insert
    // against the current revision.
    let error = insert_edge(&db_path, plan_id, 2, id_a.as_bytes(), &undeclared)
        .expect_err("corrected v8 guard aborts the same insert");
    assert!(
        error
            .to_string()
            .contains("dependency edge dependency is not declared in this revision"),
        "unexpected error: {error}"
    );
}

/// A database restamped to v7 whose triggers already carry the
/// corrected bodies takes the census fast path: no rebuild, version
/// restored to 8, data intact (the house idempotent re-migration
/// discipline).
#[test]
fn v8_restamp_with_corrected_bodies_is_idempotent() {
    let root = Root::new("v8-restamp");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    let plan_id = authority
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![node(0x0a, 0x11, &[]), node(0x0b, 0x22, &[0x0a])],
            0x01,
        ))
        .expect("revision 1")
        .receipt()
        .plan_id;
    drop(authority);

    let raw = Connection::open(&db_path).expect("raw writer");
    raw.pragma_update(None, "user_version", 7)
        .expect("stamp v7");
    drop(raw);

    let reopened = SqlitePlanAuthority::open(&db_path).expect("idempotent restamp");
    assert_eq!(user_version(&db_path), 8);
    resolve_current(&reopened, plan_id, 0x51);
}
