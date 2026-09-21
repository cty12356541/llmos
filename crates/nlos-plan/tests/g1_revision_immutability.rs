//! G1 falsification battery (ADR-0016 / 议题 35 §6, `[PLAN-DAG-001]` final
//! clause): an executed node's revision/digest must have NO rewrite path.
//!
//! The gate's falsification condition is "存在改写已执行节点 revision 的
//! 路径". Each test here closes one candidate path:
//! - the public `apply_plan_revision` face (typed fail-closed),
//! - the storage layer (DDL triggers abort raw rewrites of frozen node
//!   shape and of immutable revision receipts),
//! - and the dual case: a bit-identical re-declaration keeps the executed
//!   node's original revision/digest while the plan head still advances.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, MaterializationAdmission, MaterializationAdmissionVerdict,
    MaterializationRequest, MaterializationResolution, NodeTransitionDecision,
    NodeTransitionRequest, PlanNodeDeclaration, PlanNodeKind, PlanNodeState, PlanRevisionDecision,
    PlanStoreError, SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, TaskNodeId, TaskPlanId};
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
            "nlos-plan-g1-{label}-{}-{nonce}-{}",
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

fn node(key: u8, payload: u8) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [key; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [payload; 32],
        dependency_keys: Vec::new(),
        input_selectors_digest: [payload; 32],
        output_contract_digest: [payload; 32],
        policy_digest: [payload; 32],
        resource_ceiling_digest: [payload; 32],
        conditions: None,
    }
}

fn node_with_dependency(key: u8, payload: u8, dependency: u8) -> PlanNodeDeclaration {
    let mut declaration = node(key, payload);
    declaration.dependency_keys = vec![[dependency; 16]];
    declaration
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

fn transition(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    from: PlanNodeState,
    to: PlanNodeState,
    expected_revision: u64,
    key: u8,
) -> NodeTransitionDecision {
    authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: from,
            to_state: to,
            expected_declared_revision: expected_revision,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            transitioned_at_ms: 2_000,
        })
        .expect("record legal node transition")
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

/// Drives one node across the execution boundary: DECLARED → ELIGIBLE →
/// `WAITING_AUTHORIZATION` → gate-approved `MATERIALIZING` (the §25.2.1
/// path), where its declared shape becomes frozen.
fn drive_to_materializing(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
) {
    transition(
        authority,
        plan_id,
        node_id,
        PlanNodeState::Declared,
        PlanNodeState::Eligible,
        1,
        0xe1,
    );
    transition(
        authority,
        plan_id,
        node_id,
        PlanNodeState::Eligible,
        PlanNodeState::WaitingAuthorization,
        1,
        0xe2,
    );
    authority
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id,
            idempotency_key: IdempotencyKey::from_bytes([0xe3; 16]),
            requested_at_ms: 2_000,
        })
        .expect("gate request");
    authority
        .resolve_materialization(MaterializationResolution {
            request_key: IdempotencyKey::from_bytes([0xe3; 16]),
            verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
                profile_id: "task-10k".to_string(),
                projected_task_nodes: 2,
                projected_active_working_set: 1,
            }),
            resolved_at_ms: 2_500,
        })
        .expect("gate approval crosses the execution boundary");
}

/// G1 primary: a new revision that re-declares an executed node with a
/// different shape must fail closed with the typed
/// `FrozenNodeShapeRewrite` error, and the failed attempt must leave zero
/// durable damage (head still at the old revision, node row unchanged,
/// chain still intact).
#[test]
fn g1_executed_node_revision_cannot_be_rewritten_by_new_revision() {
    let root = Root::new("rewrite");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let first = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x01), node_with_dependency(0x0b, 0x02, 0x0a)],
            0x11,
        ))
        .expect("apply revision 1");
    let plan_id = first.clone().receipt().plan_id;
    let node_a = node_id_of(&authority, plan_id, [0x0a; 16]);
    drive_to_materializing(&authority, plan_id, node_a);
    let frozen_row = authority
        .inspect_node(plan_id, node_a)
        .expect("inspect node")
        .expect("node exists");

    // The rewrite attempt: revision 2 re-declares the executed node A with
    // a different shape digest.
    let attempt = authority.apply_plan_revision(revision_request(
        Some(plan_id),
        vec![node(0x0a, 0x7f), node_with_dependency(0x0b, 0x02, 0x0a)],
        0x12,
    ));
    assert!(
        matches!(
            &attempt,
            Err(PlanStoreError::FrozenNodeShapeRewrite {
                plan_id: frozen_plan,
                node_id,
                declared_revision: 1,
            }) if *frozen_plan == plan_id && *node_id == node_a
        ),
        "reshaping an executed node must fail closed with the typed G1 error, got {attempt:?}"
    );

    // Zero durable damage from the failed attempt.
    assert_eq!(
        authority
            .inspect_plan(plan_id)
            .expect("inspect plan")
            .expect("plan exists")
            .current_revision,
        1
    );
    let after = authority
        .inspect_node(plan_id, node_a)
        .expect("inspect node after")
        .expect("node exists");
    assert_eq!(after, frozen_row);
    let verification = authority.verify_revision_chain(plan_id).expect("chain");
    assert_eq!(verification.revision_count, 1);
}

/// G1 dual: a bit-identical re-declaration is legal and the executed node
/// keeps its original revision/digest/state while the plan head advances
/// and pre-execution nodes reshape freely.
#[test]
fn g1_frozen_node_redeclared_bit_identical_keeps_original_revision_and_digest() {
    let root = Root::new("identical");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let first = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x01), node_with_dependency(0x0b, 0x02, 0x0a)],
            0x21,
        ))
        .expect("apply revision 1");
    let plan_id = first.clone().receipt().plan_id;
    let node_a = node_id_of(&authority, plan_id, [0x0a; 16]);
    let node_b = node_id_of(&authority, plan_id, [0x0b; 16]);
    drive_to_materializing(&authority, plan_id, node_a);
    let frozen_row = authority
        .inspect_node(plan_id, node_a)
        .expect("inspect node")
        .expect("node exists");
    let pre_execution_row_b = authority
        .inspect_node(plan_id, node_b)
        .expect("inspect node b")
        .expect("node b exists");

    let second = authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x01), node_with_dependency(0x0b, 0x42, 0x0a)],
            0x22,
        ))
        .expect("identical re-declaration applies");
    assert!(matches!(second, PlanRevisionDecision::Applied(_)));
    let second_receipt = second.receipt();
    assert_eq!(second_receipt.revision, 2);
    assert_eq!(
        second_receipt.parent_revision_digest,
        Some(first.receipt().plan_digest),
        "revision 2 must chain to revision 1's digest"
    );

    let after_a = authority
        .inspect_node(plan_id, node_a)
        .expect("inspect node a")
        .expect("node a exists");
    assert_eq!(after_a, frozen_row, "executed node row is untouched");
    assert_eq!(after_a.declared_revision, 1);
    assert_eq!(after_a.state, PlanNodeState::Materializing);

    let after_b = authority
        .inspect_node(plan_id, node_b)
        .expect("inspect node b")
        .expect("node b exists");
    assert_eq!(after_b.declared_revision, 2, "pre-execution node reshapes");
    assert_ne!(
        after_b.node_digest, pre_execution_row_b.node_digest,
        "pre-execution node carries the new shape digest"
    );
    assert_eq!(after_b.node_key, pre_execution_row_b.node_key);
    assert_eq!(after_b.state, PlanNodeState::Declared);

    let verification = authority.verify_revision_chain(plan_id).expect("chain");
    assert_eq!(verification.revision_count, 2);
    assert_eq!(verification.head_revision, Some(2));
}

/// G1 storage layer: even a raw SQL writer must not be able to rewrite a
/// frozen node's declared revision/digest, nor any committed revision
/// receipt — the DDL triggers abort both.
#[test]
fn g1_storage_triggers_block_raw_rewrites() {
    let root = Root::new("trigger");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let first = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x01), node_with_dependency(0x0b, 0x02, 0x0a)],
            0x31,
        ))
        .expect("apply revision 1");
    let plan_id = first.clone().receipt().plan_id;
    let node_a = node_id_of(&authority, plan_id, [0x0a; 16]);
    drive_to_materializing(&authority, plan_id, node_a);

    let raw = Connection::open(&root.0).expect("raw connection");
    let rewrite_frozen_revision = raw.execute(
        "UPDATE plan_nodes SET declared_revision = 9
         WHERE plan_id = ?1 AND task_node_id = ?2",
        rusqlite::params![plan_id.as_bytes().as_slice(), node_a.as_bytes().as_slice()],
    );
    assert!(
        rewrite_frozen_revision.is_err(),
        "frozen node revision rewrite must abort at the storage layer"
    );
    let rewrite_frozen_digest = raw.execute(
        "UPDATE plan_nodes SET node_digest = ?3
         WHERE plan_id = ?1 AND task_node_id = ?2",
        rusqlite::params![
            plan_id.as_bytes().as_slice(),
            node_a.as_bytes().as_slice(),
            [0x99u8; 32].as_slice()
        ],
    );
    assert!(
        rewrite_frozen_digest.is_err(),
        "frozen node digest rewrite must abort at the storage layer"
    );
    let rewrite_receipt = raw.execute(
        "UPDATE plan_revisions SET plan_digest = ?2 WHERE plan_id = ?1 AND revision = 1",
        rusqlite::params![plan_id.as_bytes().as_slice(), [0x88u8; 32].as_slice()],
    );
    assert!(
        rewrite_receipt.is_err(),
        "committed revision receipts are immutable at the storage layer"
    );
    let integrity: String = raw
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity check");
    assert_eq!(integrity, "ok");
}
