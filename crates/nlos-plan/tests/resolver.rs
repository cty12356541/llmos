//! Resolver-surface integration tests for `nlos-plan` schema v2:
//! deterministic topological resolution, the typed selector negative
//! matrix, the idempotency matrix, storage-tamper fail-closed rows
//! (cycle members + root re-verification), missing-shape fencing, and
//! the v1→v2 migration paths.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, PlanNodeDeclaration, PlanNodeKind, PlanResolutionDecision,
    PlanRevisionSelector, PlanStoreError, ResolvePlanRequest, SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, ReceiptId, TaskNodeId, TaskPlanId};
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
            "nlos-plan-resolver-{label}-{}-{nonce}-{}",
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

fn resolve_request(selector: PlanRevisionSelector, key: u8) -> ResolvePlanRequest {
    ResolvePlanRequest {
        selector,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        resolved_at_ms: 3_000,
    }
}

fn resolve_current(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    key: u8,
) -> nlos_plan::PlanResolutionHandle {
    authority
        .resolve_plan(resolve_request(PlanRevisionSelector::Current(plan_id), key))
        .expect("resolve current head")
        .handle()
}

fn node_id_of(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_key: [u8; 16],
) -> TaskNodeId {
    authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == node_key)
        .expect("declared node exists")
        .node_id
}

fn user_version(path: &std::path::Path) -> i64 {
    let connection = Connection::open(path).expect("raw reader");
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version")
}

fn assert_integrity(path: &std::path::Path) {
    let connection = Connection::open(path).expect("integrity reader");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity check");
    assert_eq!(result, "ok");
}

/// Diamond `a ← b ← d`, `a ← c ← d`: the resolved order is a valid
/// dependencies-first topological order with the deterministic
/// minimum-node-id tiebreak, the edge set is the canonical sorted set, and
/// an independent resolution of the same head reproduces the same content
/// (distinct receipt id per key).
#[test]
fn diamond_graph_resolves_deterministic_topological_order() {
    let root = Root::new("diamond");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let plan_id = authority
        .apply_plan_revision(revision_request(
            None,
            vec![
                node(0x0a, 0x11, &[]),
                node(0x0b, 0x22, &[0x0a]),
                node(0x0c, 0x33, &[0x0a]),
                node(0x0d, 0x44, &[0x0b, 0x0c]),
            ],
            0x01,
        ))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let id_a = node_id_of(&authority, plan_id, [0x0a; 16]);
    let id_b = node_id_of(&authority, plan_id, [0x0b; 16]);
    let id_c = node_id_of(&authority, plan_id, [0x0c; 16]);
    let id_d = node_id_of(&authority, plan_id, [0x0d; 16]);

    let handle = resolve_current(&authority, plan_id, 0x51);
    assert_eq!(handle.revision, 1);
    assert_eq!(handle.resolved_order.len(), 4);
    assert_eq!(handle.resolved_order.first(), Some(&id_a));
    assert_eq!(handle.resolved_order.last(), Some(&id_d));
    let position = |target: TaskNodeId| {
        handle
            .resolved_order
            .iter()
            .position(|id| *id == target)
            .expect("in order")
    };
    assert!(position(id_a) < position(id_b));
    assert!(position(id_a) < position(id_c));
    assert!(position(id_b) < position(id_d));
    assert!(position(id_c) < position(id_d));
    // Deterministic tiebreak: b and c appear in ascending node-id order.
    let (middle_first, middle_second) = (handle.resolved_order[1], handle.resolved_order[2]);
    assert!(
        (middle_first == id_b && middle_second == id_c && id_b < id_c)
            || (middle_first == id_c && middle_second == id_b && id_c < id_b)
    );

    let mut canonical_edges = vec![(id_b, id_a), (id_c, id_a), (id_d, id_b), (id_d, id_c)];
    canonical_edges.sort_unstable();
    assert_eq!(handle.resolved_edges, canonical_edges);

    // An independent key resolving the same head reproduces the content.
    let second = resolve_current(&authority, plan_id, 0x52);
    assert_ne!(second.resolution_id, handle.resolution_id);
    assert_eq!(second.resolution_digest, handle.resolution_digest);
    assert_eq!(second.resolved_order, handle.resolved_order);
    assert_eq!(second.resolved_edges, handle.resolved_edges);

    // The explicit selector pins the same generation.
    let at_head = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::At {
                plan_id,
                revision: 1,
            },
            0x53,
        ))
        .expect("resolve explicit revision")
        .handle();
    assert_eq!(at_head.resolution_digest, handle.resolution_digest);
    assert_eq!(at_head.plan_digest, handle.plan_digest);
}

/// Same request bytes derive the same durable facts in two independent
/// databases (mirror of the W28-A cross-database determinism test).
#[test]
fn resolution_is_deterministic_across_databases() {
    let root_a = Root::new("det-a");
    let root_b = Root::new("det-b");
    let authority_a = SqlitePlanAuthority::open(&root_a.0).expect("open a");
    let authority_b = SqlitePlanAuthority::open(&root_b.0).expect("open b");
    let nodes = vec![node(0x0a, 0x11, &[]), node(0x0b, 0x22, &[0x0a])];

    let plan_id = authority_a
        .apply_plan_revision(revision_request(None, nodes.clone(), 0x01))
        .expect("apply a")
        .receipt()
        .plan_id;
    authority_b
        .apply_plan_revision(revision_request(None, nodes, 0x01))
        .expect("apply b");

    let handle_a = resolve_current(&authority_a, plan_id, 0x51);
    let handle_b = resolve_current(&authority_b, plan_id, 0x51);
    assert_eq!(handle_a, handle_b);
    assert_eq!(handle_a.resolution_digest, handle_b.resolution_digest);
}

/// The typed selector negative matrix: unknown plans, unknown revisions,
/// unknown receipts.
#[test]
fn selector_and_receipt_negatives_fail_typed() {
    let root = Root::new("negative");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let plan_id = authority
        .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x11, &[])], 0x01))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let unknown_plan = TaskPlanId::from_bytes([0xee; 16]);

    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::Current(unknown_plan),
            0x51
        )),
        Err(PlanStoreError::PlanNotFound(plan)) if plan == unknown_plan
    ));
    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::At {
                plan_id: unknown_plan,
                revision: 1,
            },
            0x52,
        )),
        Err(PlanStoreError::PlanNotFound(plan)) if plan == unknown_plan
    ));
    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::At {
                plan_id,
                revision: 2,
            },
            0x53,
        )),
        Err(PlanStoreError::RevisionNotFound {
            plan_id: resolved_plan,
            revision: 2,
        }) if resolved_plan == plan_id
    ));
    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::At {
                plan_id,
                revision: 0,
            },
            0x54,
        )),
        Err(PlanStoreError::RevisionNotFound { revision: 0, .. })
    ));
    let unknown_receipt = ReceiptId::from_bytes([0xee; 16]);
    assert!(matches!(
        authority.inspect_resolution(unknown_receipt),
        Ok(None)
    ));
    assert!(matches!(
        authority.inspect_resolved_nodes(unknown_receipt),
        Err(PlanStoreError::ResolutionNotFound(receipt)) if receipt == unknown_receipt
    ));
}

/// The resolution idempotency matrix: byte-equal replays of both selector
/// forms answer from the durable receipt; rebinding the key to another
/// observation time, plan, or explicit revision target is a typed
/// conflict.
#[test]
fn resolution_idempotency_matrix() {
    let root = Root::new("idem");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let plan_id = authority
        .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x11, &[])], 0x01))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let other_plan = {
        let root_b = Root::new("idem-other");
        let authority_b = SqlitePlanAuthority::open(&root_b.0).expect("open b");
        authority_b
            .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x11, &[])], 0x81))
            .expect("other plan")
            .receipt()
            .plan_id
        // root_b cleans up on drop
    };

    let original = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x61,
        ))
        .expect("resolve")
        .handle();
    let replay_same = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x61,
        ))
        .expect("replay same bytes");
    assert!(matches!(replay_same, PlanResolutionDecision::Replayed(_)));
    assert_eq!(replay_same.handle(), original);
    let replay_at_form = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::At {
                plan_id,
                revision: original.revision,
            },
            0x61,
        ))
        .expect("replay targeting the same pinned revision");
    assert!(matches!(
        replay_at_form,
        PlanResolutionDecision::Replayed(_)
    ));
    assert_eq!(replay_at_form.handle(), original);

    let different_time = ResolvePlanRequest {
        selector: PlanRevisionSelector::Current(plan_id),
        idempotency_key: IdempotencyKey::from_bytes([0x61; 16]),
        resolved_at_ms: 3_001,
    };
    assert!(matches!(
        authority.resolve_plan(different_time),
        Err(PlanStoreError::IdempotencyConflict)
    ));
    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::At {
                plan_id,
                revision: 9,
            },
            0x61,
        )),
        Err(PlanStoreError::IdempotencyConflict)
    ));
    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::Current(other_plan),
            0x61,
        )),
        Err(PlanStoreError::IdempotencyConflict)
    ));

    // Exactly one durable receipt for the key.
    let raw = Connection::open(&root.0).expect("raw reader");
    let receipts: i64 = raw
        .query_row("SELECT COUNT(*) FROM plan_resolution_receipts", [], |row| {
            row.get(0)
        })
        .expect("count receipts");
    assert_eq!(receipts, 1);
}

/// v2 admission tightening: a declaration repeating one dependency key is
/// refused typed (the repeated edge would otherwise collide on the
/// per-revision edge primary key and double-count in the roots).
#[test]
fn duplicate_dependency_keys_are_refused_typed() {
    let root = Root::new("dup-dep");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    assert!(matches!(
        authority.apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x11, &[]), node(0x0b, 0x22, &[0x0a, 0x0a]),],
            0x01,
        )),
        Err(PlanStoreError::InvalidRequest {
            reason: "duplicate dependency key in one node"
        })
    ));
}

/// Storage tamper, non-cyclic shape: an extra edge (or a phantom declared
/// node) appended below an immutable receipt breaks the root
/// re-verification — resolution fails `CorruptRecord`, never resolves
/// tampered content.
#[test]
fn tampered_non_cyclic_shape_fails_closed_on_root_verification() {
    let root = Root::new("tamper-roots");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let plan_id = authority
        .apply_plan_revision(revision_request(
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
    let id_d = node_id_of(&authority, plan_id, [0x0d; 16]);

    // An extra acyclic edge slips past the insert-time triggers (both
    // endpoints are declared); the resolve-time root verification catches
    // it.
    let raw = Connection::open(&root.0).expect("raw writer");
    raw.execute(
        "INSERT INTO plan_revision_edges (
            plan_id, revision, dependent_node_id, dependency_node_id
         ) VALUES (?1, 1, ?2, ?3)",
        rusqlite::params![
            plan_id.as_bytes().as_slice(),
            id_d.as_bytes().as_slice(),
            id_a.as_bytes().as_slice(),
        ],
    )
    .expect("append extra edge (insert is guarded only by declaration triggers)");
    drop(raw);

    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x51
        )),
        Err(PlanStoreError::CorruptRecord(
            "declared edge set does not match the revision dependencies root"
        ))
    ));

    // A phantom declared node row fails the nodes-root verification.
    let raw = Connection::open(&root.0).expect("raw writer");
    raw.execute(
        "INSERT INTO plan_revision_nodes (
            plan_id, revision, task_node_id, node_key, node_kind, node_digest
         ) VALUES (?1, 1, ?2, ?3, 1, ?4)",
        rusqlite::params![
            plan_id.as_bytes().as_slice(),
            [0x77u8; 16].as_slice(),
            [0x77u8; 16].as_slice(),
            [0x88u8; 32].as_slice(),
        ],
    )
    .expect("append phantom declared node");
    drop(raw);
    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x52
        )),
        Err(PlanStoreError::CorruptRecord(
            "declared node set does not match the revision nodes root"
        ))
    ));
    assert_integrity(&root.0);
}

/// Storage tamper, cyclic shape: an injected back edge turns the chain
/// into a cycle — resolution fails closed with `PlanCycleMembers` naming
/// exactly the nodes on the cycle; a node merely downstream of the cycle
/// is not a member.
#[test]
fn injected_cycle_fails_closed_naming_exact_members() {
    let root = Root::new("tamper-cycle");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let plan_id = authority
        .apply_plan_revision(revision_request(
            None,
            vec![
                node(0x0a, 0x11, &[]),
                node(0x0b, 0x22, &[0x0a]),
                node(0x0c, 0x33, &[0x0b]),
                node(0x0d, 0x44, &[0x0c]),
            ],
            0x01,
        ))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let id_a = node_id_of(&authority, plan_id, [0x0a; 16]);
    let id_b = node_id_of(&authority, plan_id, [0x0b; 16]);
    let id_c = node_id_of(&authority, plan_id, [0x0c; 16]);
    let id_d = node_id_of(&authority, plan_id, [0x0d; 16]);

    // Back edge a → c closes the cycle {a, b, c}; d stays downstream.
    let raw = Connection::open(&root.0).expect("raw writer");
    raw.execute(
        "INSERT INTO plan_revision_edges (
            plan_id, revision, dependent_node_id, dependency_node_id
         ) VALUES (?1, 1, ?2, ?3)",
        rusqlite::params![
            plan_id.as_bytes().as_slice(),
            id_a.as_bytes().as_slice(),
            id_c.as_bytes().as_slice(),
        ],
    )
    .expect("inject back edge");
    drop(raw);

    let mut expected_members = vec![id_a, id_b, id_c];
    expected_members.sort_unstable();
    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x51
        )),
        Err(PlanStoreError::PlanCycleMembers { members, .. }) if members == expected_members
    ));
    // The downstream node d is provably not named.
    assert!(!expected_members.contains(&id_d));
    assert_integrity(&root.0);
}

/// A revision whose durable shape rows are absent (applied before schema
/// v2 persisted per-revision shape, or the rows were lost) fails typed
/// `RevisionShapeUnavailable` on every resolution face — the shape is
/// never silently re-derived from mutable current rows.
#[test]
fn missing_revision_shape_fails_typed_never_rederived() {
    let root = Root::new("no-shape");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let plan_id = authority
        .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x11, &[])], 0x01))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let existing_resolution = resolve_current(&authority, plan_id, 0x51);

    let raw = Connection::open(&root.0).expect("raw writer");
    raw.execute_batch("DROP TRIGGER plan_revision_nodes_no_delete")
        .expect("drop guard");
    raw.execute(
        "DELETE FROM plan_revision_nodes WHERE plan_id = ?1 AND revision = 1",
        rusqlite::params![plan_id.as_bytes().as_slice()],
    )
    .expect("remove shape rows");
    drop(raw);

    assert!(matches!(
        authority.resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x52
        )),
        Err(PlanStoreError::RevisionShapeUnavailable {
            plan_id: missing_plan,
            revision: 1,
        }) if missing_plan == plan_id
    ));
    assert!(matches!(
        authority.inspect_resolved_nodes(existing_resolution.resolution_id),
        Err(PlanStoreError::RevisionShapeUnavailable { revision: 1, .. })
    ));
    // The already-committed receipt itself stays durable and immutable.
    assert_eq!(
        authority
            .inspect_resolution(existing_resolution.resolution_id)
            .expect("inspect receipt")
            .expect("receipt exists"),
        existing_resolution
    );
}

/// The v2 migration chain: fresh databases open at the chain head (v3
/// after W31-E), reopens recognize it, a v1-versioned database holding
/// the newer schemas re-migrates idempotently along the chain, and
/// unknown versions fail typed.
#[test]
fn schema_v2_migration_paths() {
    let root = Root::new("migration");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("fresh open");
    assert_eq!(user_version(&db_path), 6);
    let plan_id = authority
        .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x11, &[])], 0x01))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let handle = resolve_current(&authority, plan_id, 0x51);
    drop(authority);

    let reopened = SqlitePlanAuthority::open(&db_path).expect("reopen at head");
    assert_eq!(user_version(&db_path), 6);
    drop(reopened);

    // A database stamped v1 whose newer schemas already exist re-migrates
    // idempotently (no partial-state error, version restored to the head).
    let raw = Connection::open(&db_path).expect("raw writer");
    raw.pragma_update(None, "user_version", 1)
        .expect("stamp v1");
    drop(raw);
    let remigrated = SqlitePlanAuthority::open(&db_path).expect("idempotent re-migration");
    assert_eq!(user_version(&db_path), 6);
    assert_eq!(
        remigrated
            .inspect_resolution(handle.resolution_id)
            .expect("inspect after re-migration")
            .expect("receipt survives"),
        handle
    );
    drop(remigrated);

    let raw = Connection::open(&db_path).expect("raw writer");
    raw.pragma_update(None, "user_version", 99)
        .expect("stamp 99");
    drop(raw);
    assert!(matches!(
        SqlitePlanAuthority::open(&db_path),
        Err(PlanStoreError::SchemaVersionUnsupported(99))
    ));
    assert_integrity(&db_path);
}
