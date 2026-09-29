//! Authority-surface integration tests for `nlos-plan` schema v1:
//! authority-assigned domain-separated IDs, revision chain, idempotency
//! keys, the §25.2.1 transition vouchers, and the typed negative matrix.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, MaterializationAdmission, MaterializationAdmissionVerdict,
    MaterializationRequest, MaterializationResolution, NodeTransitionDecision,
    NodeTransitionRequest, PlanNodeDeclaration, PlanNodeKind, PlanNodeState, PlanRevisionDecision,
    PlanStoreError, SqlitePlanAuthority,
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
            "nlos-plan-authority-{label}-{}-{nonce}-{}",
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

fn first_plan(authority: &SqlitePlanAuthority) -> TaskPlanId {
    authority
        .apply_plan_revision_ungated(revision_request(None, vec![node(0x01, 0x11)], 0x01))
        .expect("apply revision 1")
        .receipt()
        .plan_id
}

/// Crosses the materialization boundary through the W31-A gate: one
/// pending request plus one approving resolution (each contributing one
/// `→ MATERIALIZING` voucher through the storage-layer gate).
fn gate_into_materializing(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: nlos_types::TaskNodeId,
    key: u8,
) {
    authority
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            requested_at_ms: 2_000,
        })
        .expect("gate request");
    authority
        .resolve_materialization(MaterializationResolution {
            request_key: IdempotencyKey::from_bytes([key; 16]),
            verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
                profile_id: "task-10k".to_string(),
                projected_task_nodes: 1,
                projected_active_working_set: 1,
            }),
            resolved_at_ms: 2_001,
        })
        .expect("gate approval");
}

#[test]
fn revision_one_is_authority_assigned_and_deterministic_across_databases() {
    let root_a = Root::new("det-a");
    let root_b = Root::new("det-b");
    let authority_a = SqlitePlanAuthority::open(&root_a.0).expect("open a");
    let authority_b = SqlitePlanAuthority::open(&root_b.0).expect("open b");
    let request = revision_request(None, vec![node(0x01, 0x11)], 0x01);

    let receipt_a = authority_a
        .apply_plan_revision_ungated(request.clone())
        .expect("apply a")
        .receipt();
    let receipt_b = authority_b
        .apply_plan_revision_ungated(request)
        .expect("apply b")
        .receipt();

    assert_eq!(
        receipt_a, receipt_b,
        "same request derives the same durable facts"
    );
    assert_eq!(receipt_a.revision, 1);
    assert_eq!(receipt_a.parent_revision_digest, None);
    assert_eq!(receipt_a.declared_node_count, 1);
}

#[test]
fn revision_replay_is_idempotent_and_rebinding_fails_typed() {
    let root = Root::new("idem");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let request = revision_request(None, vec![node(0x01, 0x11)], 0x01);
    let created_receipt = authority
        .apply_plan_revision_ungated(request.clone())
        .expect("apply")
        .receipt();

    let replayed = authority
        .apply_plan_revision_ungated(request.clone())
        .expect("replay same bytes");
    assert_eq!(replayed.clone().receipt(), created_receipt);
    assert!(matches!(replayed, PlanRevisionDecision::Replayed(_)));

    let rebound = authority
        .apply_plan_revision_ungated(revision_request(None, vec![node(0x01, 0x22)], 0x01))
        .expect_err("same key with different bytes must fail");
    assert!(matches!(rebound, PlanStoreError::IdempotencyConflict));

    // The plan head advanced exactly once.
    assert_eq!(
        authority
            .inspect_plan(created_receipt.plan_id)
            .expect("inspect")
            .expect("plan")
            .current_revision,
        1
    );
}

#[test]
fn later_revisions_chain_parent_digests_and_verify() {
    let root = Root::new("chain");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let first_receipt = authority
        .apply_plan_revision_ungated(revision_request(None, vec![node(0x01, 0x11)], 0x01))
        .expect("revision 1")
        .receipt();
    let plan_id = first_receipt.plan_id;

    let second_receipt = authority
        .apply_plan_revision_ungated(revision_request(
            Some(plan_id),
            vec![node(0x01, 0x11), node(0x02, 0x22)],
            0x02,
        ))
        .expect("revision 2")
        .receipt();
    let third_receipt = authority
        .apply_plan_revision_ungated(revision_request(
            Some(plan_id),
            vec![node(0x01, 0x11), node(0x02, 0x22), node(0x03, 0x33)],
            0x03,
        ))
        .expect("revision 3")
        .receipt();

    assert_eq!(second_receipt.revision, 2);
    assert_eq!(
        second_receipt.parent_revision_digest,
        Some(first_receipt.plan_digest)
    );
    assert_eq!(
        third_receipt.parent_revision_digest,
        Some(second_receipt.plan_digest)
    );

    let verification = authority.verify_revision_chain(plan_id).expect("verify");
    assert_eq!(verification.revision_count, 3);
    assert_eq!(verification.head_revision, Some(3));
    assert_eq!(verification.head_digest, Some(third_receipt.plan_digest));

    assert!(matches!(
        authority.verify_revision_chain(TaskPlanId::from_bytes([0xee; 16])),
        Err(PlanStoreError::PlanNotFound(_))
    ));
}

#[test]
fn revision_on_unknown_plan_and_structural_negatives_fail_typed() {
    let root = Root::new("negative");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");

    assert!(matches!(
        authority.apply_plan_revision_ungated(revision_request(
            Some(TaskPlanId::from_bytes([0x99; 16])),
            vec![node(0x01, 0x11)],
            0x01
        )),
        Err(PlanStoreError::PlanNotFound(_))
    ));
    assert!(matches!(
        authority.apply_plan_revision_ungated(revision_request(None, Vec::new(), 0x01)),
        Err(PlanStoreError::InvalidRequest {
            reason: "a plan revision must declare at least one node"
        })
    ));
    assert!(matches!(
        authority.apply_plan_revision_ungated(revision_request(
            None,
            vec![node(0x01, 0x11), node(0x01, 0x12)],
            0x01
        )),
        Err(PlanStoreError::InvalidRequest {
            reason: "duplicate node key in one revision"
        })
    ));
    assert!(matches!(
        authority.apply_plan_revision_ungated(revision_request(
            None,
            vec![PlanNodeDeclaration {
                dependency_keys: vec![[0x01; 16]],
                ..node(0x01, 0x11)
            }],
            0x01
        )),
        Err(PlanStoreError::InvalidRequest {
            reason: "node depends on itself"
        })
    ));
    let mut unknown_dep = node(0x01, 0x11);
    unknown_dep.dependency_keys = vec![[0xfe; 16]];
    assert!(matches!(
        authority.apply_plan_revision_ungated(revision_request(None, vec![unknown_dep], 0x01)),
        Err(PlanStoreError::UnknownDependency { node_key }) if node_key == [0xfe; 16]
    ));

    let mut a = node(0x01, 0x11);
    a.dependency_keys = vec![[0x02; 16]];
    let mut b = node(0x02, 0x22);
    b.dependency_keys = vec![[0x01; 16]];
    assert!(matches!(
        authority.apply_plan_revision_ungated(revision_request(None, vec![a, b], 0x01)),
        Err(PlanStoreError::PlanCycle)
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One test walks the full edge set with typed fences.
fn transition_vouchers_walk_the_state_machine_with_typed_fences() {
    let root = Root::new("machine");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let _ = &plan_id;
    let node_id = authority
        .list_plan_nodes(plan_id)
        .expect("list")
        .into_iter()
        .find(|record| record.node_key == [0x01; 16])
        .expect("declared node")
        .node_id;

    let step = |from: PlanNodeState, to: PlanNodeState, key: u8| {
        authority.record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: from,
            to_state: to,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            transitioned_at_ms: 2_000,
        })
    };

    let illegal = step(PlanNodeState::Declared, PlanNodeState::Active, 0xf0)
        .expect_err("DECLARED -> ACTIVE is not a legal edge");
    assert!(matches!(
        illegal,
        PlanStoreError::IllegalNodeTransition {
            from: PlanNodeState::Declared,
            to: PlanNodeState::Active,
            ..
        }
    ));
    let unknown_node = authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id: nlos_types::TaskNodeId::from_bytes([0xee; 16]),
            from_state: PlanNodeState::Declared,
            to_state: PlanNodeState::Eligible,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0xf1; 16]),
            transitioned_at_ms: 2_000,
        })
        .expect_err("unknown node");
    assert!(matches!(unknown_node, PlanStoreError::NodeNotFound { .. }));

    // The walk keeps one voucher per step; the two `→ MATERIALIZING`
    // entries go through the W31-A gate (request + approving
    // resolution), since the storage layer refuses raw materializing
    // vouchers without a gate-approved request.
    let walk = [
        (PlanNodeState::Declared, PlanNodeState::Eligible, 0xa1),
        (
            PlanNodeState::Eligible,
            PlanNodeState::WaitingAuthorization,
            0xa2,
        ),
        (
            PlanNodeState::WaitingAuthorization,
            PlanNodeState::Materializing,
            0xa3,
        ),
        (PlanNodeState::Materializing, PlanNodeState::Active, 0xa4),
        (PlanNodeState::Active, PlanNodeState::Checkpointed, 0xa5),
        (PlanNodeState::Checkpointed, PlanNodeState::Evicted, 0xa6),
        (PlanNodeState::Evicted, PlanNodeState::Rehydrating, 0xa7),
        (
            PlanNodeState::Rehydrating,
            PlanNodeState::Materializing,
            0xa8,
        ),
        (PlanNodeState::Materializing, PlanNodeState::Active, 0xa9),
        (PlanNodeState::Active, PlanNodeState::Completed, 0xaa),
    ];
    for (from, to, key) in walk {
        if to == PlanNodeState::Materializing {
            gate_into_materializing(&authority, plan_id, node_id, key);
            let record = authority
                .inspect_node(plan_id, node_id)
                .expect("inspect")
                .expect("node");
            assert_eq!(record.state, to);
            continue;
        }
        let decision = step(from, to, key).expect("legal step");
        assert!(matches!(decision, NodeTransitionDecision::Recorded(_)));
    }

    let record = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(record.state, PlanNodeState::Completed);
    assert_eq!(record.transition_count, walk.len() as u64);

    let vouchers = authority
        .inspect_node_vouchers(plan_id, node_id)
        .expect("vouchers");
    assert_eq!(vouchers.len(), walk.len());
    for (index, voucher) in vouchers.iter().enumerate() {
        assert_eq!(voucher.transition_seq, index as u64 + 1);
        assert_eq!(voucher.observed_revision, 1);
    }

    // Terminal states have no outgoing edge, not even cancel.
    assert!(matches!(
        step(PlanNodeState::Completed, PlanNodeState::Cancelled, 0xab),
        Err(PlanStoreError::IllegalNodeTransition { .. })
    ));

    // Replay of one walk key returns the original voucher; a rebound key
    // is a conflict.
    let (from, to, key) = walk[0];
    let replay = step(from, to, key).expect("replay");
    assert!(matches!(replay, NodeTransitionDecision::Replayed(_)));
    assert!(matches!(
        step(from, PlanNodeState::BlockedDependency, key),
        Err(PlanStoreError::IdempotencyConflict)
    ));
}

#[test]
fn transition_cas_fences_stale_revision_and_state() {
    let root = Root::new("cas");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let _ = &plan_id;
    let node_id = authority
        .list_plan_nodes(plan_id)
        .expect("list")
        .into_iter()
        .find(|record| record.node_key == [0x01; 16])
        .expect("declared node")
        .node_id;

    // Stale declared-revision CAS: caller believes revision 2, durable
    // node is at revision 1.
    let stale = authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Declared,
            to_state: PlanNodeState::Eligible,
            expected_declared_revision: 2,
            idempotency_key: IdempotencyKey::from_bytes([0xb1; 16]),
            transitioned_at_ms: 2_000,
        })
        .expect_err("stale revision");
    assert!(matches!(
        stale,
        PlanStoreError::StaleNodeRevision {
            expected: 2,
            current: 1,
            ..
        }
    ));

    // Advance the node, then present the pre-advance state: CAS failure.
    authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Declared,
            to_state: PlanNodeState::Eligible,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0xb2; 16]),
            transitioned_at_ms: 2_000,
        })
        .expect("advance");
    let stale_state = authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Declared,
            to_state: PlanNodeState::Eligible,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0xb3; 16]),
            transitioned_at_ms: 2_000,
        })
        .expect_err("stale state");
    assert!(matches!(
        stale_state,
        PlanStoreError::NodeStateCasMismatch {
            expected_from: PlanNodeState::Declared,
            current: PlanNodeState::Eligible,
            ..
        }
    ));

    // A revision bump fences in-flight transitions on pre-execution nodes.
    authority
        .apply_plan_revision_ungated(revision_request(
            Some(plan_id),
            vec![node(0x01, 0x12)],
            0x02,
        ))
        .expect("revision 2 reshapes the pre-execution node");
    let fenced = authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Eligible,
            to_state: PlanNodeState::WaitingAuthorization,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0xb4; 16]),
            transitioned_at_ms: 2_000,
        })
        .expect_err("stale after reshape");
    assert!(matches!(
        fenced,
        PlanStoreError::StaleNodeRevision {
            expected: 1,
            current: 2,
            ..
        }
    ));
}

#[test]
fn reopen_recognizes_schema_and_rejects_unknown_versions() {
    let root = Root::new("schema");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    drop(authority);

    let reopened = SqlitePlanAuthority::open(&root.0).expect("reopen at v1");
    drop(reopened);

    let raw = Connection::open(&root.0).expect("raw");
    raw.pragma_update(None, "user_version", 99)
        .expect("bump version");
    drop(raw);
    assert!(matches!(
        SqlitePlanAuthority::open(&root.0),
        Err(PlanStoreError::SchemaVersionUnsupported(99))
    ));
}
