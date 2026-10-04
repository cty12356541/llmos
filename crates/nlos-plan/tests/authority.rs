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

/// Deterministic 16-byte key for one chain position.
fn chain_key(index: usize) -> [u8; 16] {
    (index as u128).to_be_bytes()
}

/// One link of a linear dependency chain: node `index` depends on
/// node `index - 1` (nothing for the head).
fn chain_node(index: usize) -> PlanNodeDeclaration {
    let payload = u8::try_from(index % 256).expect("index modulo 256 fits u8");
    PlanNodeDeclaration {
        node_key: chain_key(index),
        kind: PlanNodeKind::AgentRole,
        binding_digest: [payload; 32],
        dependency_keys: if index == 0 {
            Vec::new()
        } else {
            vec![chain_key(index - 1)]
        },
        input_selectors_digest: [payload; 32],
        output_contract_digest: [payload; 32],
        policy_digest: [payload; 32],
        resource_ceiling_digest: [payload; 32],
        conditions: None,
    }
}

/// Deep linear chains are legal declarations bounded only by
/// `MAX_DECLARED_NODES_PER_REVISION`; the cycle detector must not
/// recurse per node (a 50k-deep chain overflowed the default test
/// thread stack — an abort, not a recoverable error;
/// deep-audit/29 #1). The same chain closed into a cycle still fails
/// typed.
#[test]
fn deep_linear_chain_applies_without_recursion_overflow() {
    const CHAIN: usize = 50_000;
    let root = Root::new("deep-chain");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");

    let nodes: Vec<PlanNodeDeclaration> = (0..CHAIN).map(chain_node).collect();
    let decision = authority
        .apply_plan_revision_ungated(revision_request(None, nodes, 0x01))
        .expect("deep acyclic chain applies without overflowing the stack");
    assert_eq!(decision.receipt().declared_node_count, CHAIN as u64);

    // Close the chain into a cycle: still typed, still stack-safe.
    let mut cyclic: Vec<PlanNodeDeclaration> = (0..CHAIN).map(chain_node).collect();
    cyclic[0].dependency_keys = vec![chain_key(CHAIN - 1)];
    let refused = authority
        .apply_plan_revision_ungated(revision_request(None, cyclic, 0x02))
        .expect_err("the closed chain is a cycle");
    assert!(matches!(refused, PlanStoreError::PlanCycle));
}

/// A total revision may not omit an execution-frozen node: the shape
/// fence pins *how* such a node is re-declared, this pins *that* it is
/// — omission would silently drop the node from the plan's current
/// shape while its Task-side execution footprint persists
/// (deep-audit/29 #2). Re-declaring the frozen shape passes.
#[test]
fn total_revision_cannot_omit_execution_frozen_node() {
    let root = Root::new("frozen-omission");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = authority
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x01,
        ))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    let node_a = authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("node a exists")
        .node_id;
    gate_into_materializing(&authority, plan_id, node_a, 0x21);

    // Revision 2 omits the frozen node: typed refusal, nothing written.
    let refused = authority
        .apply_plan_revision_ungated(revision_request(
            Some(plan_id),
            vec![node(0x0b, 0x02)],
            0x02,
        ))
        .expect_err("omitting an execution-frozen node must fail typed");
    assert!(matches!(
        refused,
        PlanStoreError::FrozenNodeOmitted { node_id, .. } if node_id == node_a
    ));
    let head = authority.inspect_plan(plan_id).expect("inspect plan");
    assert_eq!(
        head.expect("plan exists").current_revision,
        1,
        "the refused revision wrote nothing"
    );

    // Re-declaring the frozen node with its exact shape passes the fence.
    let applied = authority
        .apply_plan_revision_ungated(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x03,
        ))
        .expect("frozen node re-declared bit-identically");
    assert_eq!(applied.receipt().revision, 2);
    let row = authority
        .inspect_node(plan_id, node_a)
        .expect("inspect node")
        .expect("node exists");
    assert_eq!(row.state, PlanNodeState::Materializing);
    assert_eq!(
        row.declared_revision, 1,
        "the frozen row keeps its original revision (PLAN-DAG-001)"
    );
}

/// W57-A #4: the terminal exits of the §25.2.1 executing pipeline
/// distribute over its executing states, so a failed materialization
/// attempt and a checkpointed node's direct completion/failure are
/// legal durable edges. Spec basis (v0.5 §25.2.1):
///
/// ```text
/// → MATERIALIZING → ACTIVE (…) → CHECKPOINTED
///   → EVICTED (residency=WARM|COLD) | COMPLETED | FAILED | CANCELLED
/// ```
///
/// `CHECKPOINTED` sits directly before the terminal-exit list, so
/// `CHECKPOINTED → COMPLETED | FAILED` is on the chain; the same
/// exits extend to the pipeline's entry (`MATERIALIZING → FAILED`).
/// Each new edge is walked through the durable face, and the
/// out-of-spec neighbors (`EVICTED → COMPLETED/FAILED`,
/// `CHECKPOINTED → ACTIVE`, terminal → anywhere) refuse typed.
#[test]
#[allow(clippy::too_many_lines)] // One test walks every W57-A edge with typed refusals.
fn terminal_exit_edges_reach_failed_and_completed_from_executing_states() {
    let root = Root::new("terminal-exits");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = authority
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![
                node(0x01, 0x11),
                node(0x02, 0x22),
                node(0x03, 0x33),
                node(0x04, 0x44),
            ],
            0x01,
        ))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    let node_of = |key: [u8; 16]| {
        authority
            .list_plan_nodes(plan_id)
            .expect("list nodes")
            .into_iter()
            .find(|record| record.node_key == key)
            .expect("declared node")
            .node_id
    };
    let step =
        |node_id: nlos_types::TaskNodeId, from: PlanNodeState, to: PlanNodeState, key: u8| {
            authority.record_node_transition(NodeTransitionRequest {
                plan_id,
                node_id,
                from_state: from,
                to_state: to,
                expected_declared_revision: 1,
                idempotency_key: IdempotencyKey::from_bytes([key; 16]),
                transitioned_at_ms: 3_000,
            })
        };
    let drive_to = |node_id: nlos_types::TaskNodeId, key: u8, to: PlanNodeState| {
        // Walk DECLARED → ELIGIBLE → WAITING_RESOURCE → [gate]
        // MATERIALIZING → ACTIVE → CHECKPOINTED, reusing the gate face
        // for the boundary crossing. Voucher idempotency keys are
        // globally unique, so every node walks on its own disjoint
        // base+index key block.
        step(
            node_id,
            PlanNodeState::Declared,
            PlanNodeState::Eligible,
            key,
        )
        .expect("declared → eligible");
        step(
            node_id,
            PlanNodeState::Eligible,
            PlanNodeState::WaitingResource,
            key + 0x01,
        )
        .expect("eligible → waiting resource");
        gate_into_materializing(&authority, plan_id, node_id, key + 0x02);
        if to == PlanNodeState::Materializing {
            return;
        }
        step(
            node_id,
            PlanNodeState::Materializing,
            PlanNodeState::Active,
            key + 0x03,
        )
        .expect("materializing → active");
        if to == PlanNodeState::Active {
            return;
        }
        step(
            node_id,
            PlanNodeState::Active,
            PlanNodeState::Checkpointed,
            key + 0x04,
        )
        .expect("active → checkpointed");
    };

    // New edge 1: MATERIALIZING → FAILED (failed materialization
    // attempt exits the pipeline's entry state).
    let failed_from_materializing = node_of([0x01; 16]);
    drive_to(
        failed_from_materializing,
        0x40,
        PlanNodeState::Materializing,
    );
    let decision = step(
        failed_from_materializing,
        PlanNodeState::Materializing,
        PlanNodeState::Failed,
        0x4f,
    )
    .expect("MATERIALIZING → FAILED is a legal durable edge");
    assert!(
        matches!(decision, NodeTransitionDecision::Recorded(ref voucher)
            if voucher.from_state == PlanNodeState::Materializing
                && voucher.to_state == PlanNodeState::Failed)
    );
    assert_eq!(
        authority
            .inspect_node(plan_id, failed_from_materializing)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::Failed
    );

    // New edge 2: CHECKPOINTED → COMPLETED (the chain's terminal exit
    // from its last executing state).
    let completed_from_checkpointed = node_of([0x02; 16]);
    drive_to(
        completed_from_checkpointed,
        0x50,
        PlanNodeState::Checkpointed,
    );
    step(
        completed_from_checkpointed,
        PlanNodeState::Checkpointed,
        PlanNodeState::Completed,
        0x5f,
    )
    .expect("CHECKPOINTED → COMPLETED is a legal durable edge");
    assert_eq!(
        authority
            .inspect_node(plan_id, completed_from_checkpointed)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::Completed
    );

    // New edge 3: CHECKPOINTED → FAILED (the terminal exits distribute
    // over the pipeline's states, not just its head).
    let failed_from_checkpointed = node_of([0x03; 16]);
    drive_to(failed_from_checkpointed, 0x60, PlanNodeState::Checkpointed);
    step(
        failed_from_checkpointed,
        PlanNodeState::Checkpointed,
        PlanNodeState::Failed,
        0x6f,
    )
    .expect("CHECKPOINTED → FAILED is a legal durable edge");
    assert_eq!(
        authority
            .inspect_node(plan_id, failed_from_checkpointed)
            .expect("inspect")
            .expect("node")
            .state,
        PlanNodeState::Failed
    );

    // Out-of-spec refusals from the same walk: EVICTED's only exit is
    // REHYDRATING (`EVICTED → REHYDRATING → MATERIALIZING`), backward
    // pipeline edges never un-execute, and terminal states have no
    // outgoing edge at all.
    let evicted = node_of([0x04; 16]);
    drive_to(evicted, 0x70, PlanNodeState::Checkpointed);
    step(
        evicted,
        PlanNodeState::Checkpointed,
        PlanNodeState::Evicted,
        0x78,
    )
    .expect("CHECKPOINTED → EVICTED stays legal (residency eviction)");
    for (from, to) in [
        (PlanNodeState::Evicted, PlanNodeState::Completed),
        (PlanNodeState::Evicted, PlanNodeState::Failed),
        (PlanNodeState::Evicted, PlanNodeState::Active),
    ] {
        // One key serves every iteration: a typed refusal commits
        // nothing, so the key is never bound (the refusal must come
        // from the edge check, not an idempotency replay).
        assert!(matches!(
            step(evicted, from, to, 0xf1),
            Err(PlanStoreError::IllegalNodeTransition {
                from: illegal_from,
                to: illegal_to,
                ..
            }) if illegal_from == from && illegal_to == to
        ));
    }
    assert!(matches!(
        step(
            evicted,
            PlanNodeState::Checkpointed,
            PlanNodeState::Active,
            0xf2
        ),
        Err(PlanStoreError::IllegalNodeTransition { .. })
    ));
    for node_id in [failed_from_materializing, failed_from_checkpointed] {
        assert!(matches!(
            step(
                node_id,
                PlanNodeState::Failed,
                PlanNodeState::Rehydrating,
                0xf3
            ),
            Err(PlanStoreError::IllegalNodeTransition { .. })
        ));
        assert!(matches!(
            step(
                node_id,
                PlanNodeState::Failed,
                PlanNodeState::Cancelled,
                0xf4
            ),
            Err(PlanStoreError::IllegalNodeTransition { .. })
        ));
    }
    assert!(matches!(
        step(
            completed_from_checkpointed,
            PlanNodeState::Completed,
            PlanNodeState::Cancelled,
            0xf5
        ),
        Err(PlanStoreError::IllegalNodeTransition { .. })
    ));
}

/// W57-A #4: the legal edge predicate is exactly the §25.2.1 subset —
/// the executing chain
/// `DECLARED → BLOCKED_DEPENDENCY → ELIGIBLE → WAITING_AUTHORIZATION |
/// WAITING_RESOURCE → MATERIALIZING → ACTIVE → CHECKPOINTED → EVICTED |
/// COMPLETED | FAILED | CANCELLED` with `EVICTED → REHYDRATING →
/// MATERIALIZING` as the only evicted exit and `→ CANCELLED` from
/// every non-terminal state. Every one of the 13×13 ordered pairs
/// outside that table must be refused (规范外边拒绝), so an accidental
/// extra edge fails this sweep by name.
#[test]
fn state_machine_edge_set_matches_the_spec_subset_exactly() {
    use PlanNodeState as S;
    let legal: std::collections::HashSet<(PlanNodeState, PlanNodeState)> = [
        (S::Declared, S::BlockedDependency),
        (S::Declared, S::Eligible),
        (S::BlockedDependency, S::Eligible),
        (S::Eligible, S::WaitingAuthorization),
        (S::Eligible, S::WaitingResource),
        (S::WaitingAuthorization, S::Materializing),
        (S::WaitingResource, S::Materializing),
        (S::Materializing, S::Active),
        (S::Materializing, S::Failed),
        (S::Active, S::Checkpointed),
        (S::Active, S::Completed),
        (S::Active, S::Failed),
        (S::Checkpointed, S::Evicted),
        (S::Checkpointed, S::Completed),
        (S::Checkpointed, S::Failed),
        (S::Evicted, S::Rehydrating),
        (S::Rehydrating, S::Materializing),
    ]
    .into_iter()
    .collect();
    let states = [
        S::Declared,
        S::BlockedDependency,
        S::Eligible,
        S::WaitingAuthorization,
        S::WaitingResource,
        S::Materializing,
        S::Active,
        S::Checkpointed,
        S::Evicted,
        S::Rehydrating,
        S::Completed,
        S::Failed,
        S::Cancelled,
    ];
    for from in states {
        for to in states {
            let expected = legal.contains(&(from, to))
                || (to == S::Cancelled && !from.is_terminal() && from != S::Cancelled);
            assert_eq!(
                PlanNodeState::transition_is_legal(from, to),
                expected,
                "edge {from:?} -> {to:?} must be {} per §25.2.1",
                if expected { "legal" } else { "refused" }
            );
        }
    }
    // The terminal set is exactly the chain's three exits.
    for state in states {
        assert_eq!(
            state.is_terminal(),
            matches!(state, S::Completed | S::Failed | S::Cancelled),
            "terminal set drift for {state:?}"
        );
    }
}

/// W57-A #8: a terminal node is an undeletable tombstone row
/// (`plan_nodes_no_delete`) but leaves the declared-TaskNode admission
/// population — it has no outgoing §25.2.1 edge, so it can never again
/// await or hold materialization. Every non-terminal state (including
/// `EVICTED`, whose live exit `EVICTED → REHYDRATING → MATERIALIZING`
/// keeps it admissible) still counts. The physical row is asserted to
/// survive, pinning "excluded from the dimension" against "deleted".
#[test]
fn terminal_tombstone_leaves_the_declared_population_and_keeps_its_row() {
    let root = Root::new("tombstone-count");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = authority
        .apply_plan_revision_ungated(revision_request(None, vec![node(0x01, 0x11)], 0x01))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    let node_id = authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
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
            transitioned_at_ms: 3_000,
        })
    };
    let declared_count = || {
        authority
            .inspect_declared_task_node_count()
            .expect("declared task-node count")
    };

    // Every non-terminal state keeps the node in the population.
    step(PlanNodeState::Declared, PlanNodeState::Eligible, 0xa1).expect("→ eligible");
    assert_eq!(declared_count(), 1, "ELIGIBLE counts");
    step(
        PlanNodeState::Eligible,
        PlanNodeState::WaitingResource,
        0xa2,
    )
    .expect("→ waiting resource");
    gate_into_materializing(&authority, plan_id, node_id, 0xa3);
    assert_eq!(declared_count(), 1, "MATERIALIZING counts");
    step(PlanNodeState::Materializing, PlanNodeState::Active, 0xa4).expect("→ active");
    step(PlanNodeState::Active, PlanNodeState::Checkpointed, 0xa5).expect("→ checkpointed");
    step(PlanNodeState::Checkpointed, PlanNodeState::Evicted, 0xa6).expect("→ evicted");
    assert_eq!(
        declared_count(),
        1,
        "EVICTED still counts: REHYDRATING → MATERIALIZING is a live exit"
    );
    step(PlanNodeState::Evicted, PlanNodeState::Rehydrating, 0xa7).expect("→ rehydrating");
    gate_into_materializing(&authority, plan_id, node_id, 0xa8);
    assert_eq!(declared_count(), 1, "REHYDRATING and re-entry count");

    // The terminal transition leaves the population…
    step(PlanNodeState::Materializing, PlanNodeState::Failed, 0xa9).expect("→ failed (W57-A edge)");
    assert_eq!(
        declared_count(),
        0,
        "a terminal tombstone occupies no declared-TaskNode seat"
    );

    // …but the row itself is durable lifetime metadata, never deleted.
    let raw = Connection::open(&root.0).expect("raw open");
    let rows: i64 = raw
        .query_row("SELECT COUNT(*) FROM plan_nodes", [], |row| row.get(0))
        .expect("count plan_nodes");
    assert_eq!(rows, 1, "the tombstone row survives (plan_nodes_no_delete)");
    assert_eq!(
        raw.query_row(
            "SELECT node_state FROM plan_nodes WHERE task_node_id = ?1",
            [node_id.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .expect("read tombstone state"),
        12_i64,
        "FAILED is stable discriminant 12"
    );
}
