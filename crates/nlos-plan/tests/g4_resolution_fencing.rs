//! G4 falsification battery (ADR-0016 决定 5 / 议题 35 §6 G4, v0.5 行 3652
//! `[PLAN-DEPENDENCY-001]`): a resolution must be a *pinned,
//! generation-carrying* fact — `current` pins exactly once, and a later
//! revision that reshapes unresolved nodes must never be silently observed
//! through an in-flight resolution handle.
//!
//! 议题 35 §6 G4 证伪条件：「存在绕过版本解析直达授权的路径」。本文件
//! 逐路径关闭：
//! - 形状读取路径：通过旧 resolution 读取节点形状必须返回该 resolution
//!   钉住的 revision 形状（pinned view），而非可变 `plan_nodes` 当前行
//!   （naive 读法即「latest 静默漂移」——G4 证伪面，红→绿记录见
//!   evidence §7.3）；
//! - `Current` selector 只钉一次：头 revision 推进后，重放/再读旧
//!   resolution 永不浮到新形状；
//! - 授权/状态推进路径：持旧 resolution revision 的迁移被
//!   `StaleNodeRevision` 栅栏拒绝（typed fence，非静默）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, NodeTransitionRequest, PlanNodeDeclaration, PlanNodeKind,
    PlanNodeState, PlanResolutionDecision, PlanRevisionSelector, PlanStoreError,
    ResolvePlanRequest, SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, TaskNodeId, TaskPlanId};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-plan-g4-{label}-{}-{nonce}-{}",
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
    }
}

fn chain(key: u8, payload: u8, dependency: u8) -> PlanNodeDeclaration {
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

fn resolve_request(selector: PlanRevisionSelector, key: u8) -> ResolvePlanRequest {
    ResolvePlanRequest {
        selector,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        resolved_at_ms: 3_000,
    }
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

/// G4 primary: resolve at revision N, apply revision N+1 that reshapes the
/// (still pre-execution) nodes, then read shapes through the OLD
/// resolution — every digest must stay pinned to revision N. The naive
/// implementation reads the mutable `plan_nodes` rows and silently returns
/// the revision N+1 shapes; that bypass path is the red this test
/// falsifies.
#[test]
fn g4_resolution_pins_shapes_against_later_revision_reshape() {
    let root = Root::new("pin-shape");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let first_receipt = authority
        .apply_plan_revision(revision_request(
            None,
            vec![
                node(0x0a, 0x11),
                chain(0x0b, 0x22, 0x0a),
                chain(0x0c, 0x33, 0x0b),
            ],
            0x01,
        ))
        .expect("revision 1")
        .receipt();
    let plan_id = first_receipt.plan_id;
    let id_a = node_id_of(&authority, plan_id, [0x0a; 16]);
    let id_b = node_id_of(&authority, plan_id, [0x0b; 16]);
    let id_c = node_id_of(&authority, plan_id, [0x0c; 16]);

    // Resolution against the current head (revision 1).
    let resolved = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x51,
        ))
        .expect("resolve current at revision 1");
    assert!(matches!(resolved, PlanResolutionDecision::Resolved(_)));
    let handle = resolved.handle();
    assert_eq!(handle.plan_id, plan_id);
    assert_eq!(handle.revision, 1);
    assert_eq!(handle.plan_digest, first_receipt.plan_digest);
    assert_eq!(handle.resolved_order, vec![id_a, id_b, id_c]);
    let mut canonical_edges = vec![(id_b, id_a), (id_c, id_b)];
    canonical_edges.sort_unstable();
    assert_eq!(handle.resolved_edges, canonical_edges);

    let pinned = authority
        .inspect_resolved_nodes(handle.resolution_id)
        .expect("pinned shape view");
    assert_eq!(pinned.len(), 3);
    assert_eq!(pinned[0].node_id, id_a);
    assert_eq!(pinned[0].position, 1);
    let digest_b_at_revision_1 = pinned[1].node_digest;
    let digest_tail_at_revision_1 = pinned[2].node_digest;
    assert_eq!(pinned[1].node_id, id_b);
    assert_eq!(pinned[2].node_id, id_c);

    // Revision 2 reshapes the still-unresolved nodes b and c (legal:
    // nothing crossed the execution boundary).
    authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![
                node(0x0a, 0x11),
                chain(0x0b, 0x99, 0x0a),
                chain(0x0c, 0x99, 0x0b),
            ],
            0x02,
        ))
        .expect("revision 2 reshapes unresolved nodes");

    // The durable current rows HAVE moved (that is the reshape).
    let current_b = authority
        .inspect_node(plan_id, id_b)
        .expect("inspect node b")
        .expect("node b exists");
    assert_eq!(current_b.declared_revision, 2);
    assert_ne!(current_b.node_digest, digest_b_at_revision_1);

    // G4 fence: the OLD resolution must NOT silently observe the new
    // shapes — its view stays byte-equal to the revision-1 pinning.
    let pinned_after = authority
        .inspect_resolved_nodes(handle.resolution_id)
        .expect("pinned shape view after reshape");
    assert_eq!(
        pinned_after, pinned,
        "resolution handle must stay pinned to the shapes of the revision it was computed against"
    );
    assert_eq!(pinned_after[1].node_digest, digest_b_at_revision_1);
    assert_eq!(pinned_after[2].node_digest, digest_tail_at_revision_1);

    // Replaying the old key returns the original receipt byte-equal; the
    // resolution never floats to the new head.
    let replayed = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x51,
        ))
        .expect("replay resolution key");
    assert!(matches!(replayed, PlanResolutionDecision::Replayed(_)));
    assert_eq!(replayed.handle(), handle);

    // A fresh resolution of the new head carries the new generation and
    // the new shapes; both receipts coexist for audit.
    let second = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x52,
        ))
        .expect("resolve current at revision 2")
        .handle();
    assert_eq!(second.revision, 2);
    let second_view = authority
        .inspect_resolved_nodes(second.resolution_id)
        .expect("new pinned shape view");
    assert_eq!(second_view[1].node_digest, current_b.node_digest);
    assert_eq!(
        authority
            .inspect_resolution(handle.resolution_id)
            .expect("inspect old resolution")
            .expect("old receipt still exists")
            .revision,
        1
    );
}

/// G4 selector semantics: `Current` pins exactly once. After the head
/// advances, replaying the original key still answers from the original
/// receipt (crash-retry semantics), a same-head re-resolution with a new
/// key pins the same generation, and only a fresh resolution observes the
/// new head.
#[test]
fn g4_current_selector_pins_once_and_receipt_never_floats() {
    let root = Root::new("pin-once");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let first_receipt = authority
        .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x11)], 0x01))
        .expect("revision 1")
        .receipt();
    let plan_id = first_receipt.plan_id;

    let original = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x61,
        ))
        .expect("resolve at revision 1")
        .handle();
    assert_eq!(original.revision, 1);
    assert_eq!(original.plan_digest, first_receipt.plan_digest);

    // Same head, second key: same pinned generation, distinct receipt.
    let same_head = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::At {
                plan_id,
                revision: 1,
            },
            0x63,
        ))
        .expect("resolve explicit revision 1")
        .handle();
    assert_eq!(same_head.revision, 1);
    assert_eq!(same_head.plan_digest, original.plan_digest);
    assert_eq!(same_head.resolved_order, original.resolved_order);
    assert_eq!(same_head.resolution_digest, original.resolution_digest);
    assert_ne!(same_head.resolution_id, original.resolution_id);

    authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x11), node(0x0d, 0x44)],
            0x02,
        ))
        .expect("revision 2 adds a node");

    // The pre-advance replay is answered from the durable original: it
    // must NOT re-resolve against the new head.
    let replay = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x61,
        ))
        .expect("replay after head advance");
    assert!(matches!(replay, PlanResolutionDecision::Replayed(_)));
    assert_eq!(replay.handle(), original);

    // Rebinding the original key to a different revision target is a
    // typed conflict, never a floating re-resolution.
    let rebound = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::At {
                plan_id,
                revision: 2,
            },
            0x61,
        ))
        .expect_err("key rebound to another revision");
    assert!(matches!(rebound, PlanStoreError::IdempotencyConflict));

    let fresh = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x62,
        ))
        .expect("fresh resolution pins the new head")
        .handle();
    assert_eq!(fresh.revision, 2);
    assert_eq!(fresh.resolved_order.len(), 2);
}

/// G4 fence half: an in-flight view carrying the resolution's revision
/// cannot drive current-state advances past the generation fence — the
/// `StaleNodeRevision` CAS rejects it typed (never silently rebinding).
#[test]
fn g4_stale_resolution_revision_cannot_drive_transitions_past_fence() {
    let root = Root::new("fence");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let plan_id = authority
        .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x11)], 0x01))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let node_id = node_id_of(&authority, plan_id, [0x0a; 16]);
    let resolution = authority
        .resolve_plan(resolve_request(
            PlanRevisionSelector::Current(plan_id),
            0x71,
        ))
        .expect("resolve revision 1")
        .handle();
    assert_eq!(resolution.revision, 1);

    authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x22)],
            0x02,
        ))
        .expect("revision 2 reshapes the unresolved node");

    // A transition presented with the resolution's (now stale) revision
    // view is fenced typed — the handle grants no bypass path.
    let fenced = authority.record_node_transition(NodeTransitionRequest {
        plan_id,
        node_id,
        from_state: PlanNodeState::Declared,
        to_state: PlanNodeState::Eligible,
        expected_declared_revision: resolution.revision,
        idempotency_key: IdempotencyKey::from_bytes([0x72; 16]),
        transitioned_at_ms: 2_000,
    });
    assert!(matches!(
        fenced,
        Err(PlanStoreError::StaleNodeRevision {
            expected: 1,
            current: 2,
            ..
        })
    ));
}
