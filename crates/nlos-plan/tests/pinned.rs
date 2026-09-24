//! W36-P8 PINNED overlay (W31-G §8.2.6 / `[SCALE-PIN-001]` 最小可测档):
//! a durable pin on the existing 5-tier residency axis that **refuses
//! eviction** and **degrades** by unpin-then-evict.
//!
//! This is deliberately *not* a sixth `NodeResidencyTier` discriminant
//! (that would break the out-of-write-set `ContextResidencyTier`
//! mapping). It is also not the full SCALE-PIN-001 ledger
//! (`ResourceAllocation` / owner / reason / resident-bytes / rebuild-cost
//! / expiry / fence) — those stay named boundaries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, NodePinDecision, NodePinRequest, NodeResidencyTier,
    PlanNodeDeclaration, PlanNodeKind, PlanStoreError, ResidencyTransitionRequest,
    SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, TaskPlanId};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-plan-pinned-{label}-{}-{nonce}-{}",
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

fn first_plan(authority: &SqlitePlanAuthority) -> TaskPlanId {
    authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![node(0x01, 0x11)],
            idempotency_key: IdempotencyKey::from_bytes([0x01; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply revision 1")
        .receipt()
        .plan_id
}

fn first_node_id(authority: &SqlitePlanAuthority, plan_id: TaskPlanId) -> nlos_types::TaskNodeId {
    authority
        .list_plan_nodes(plan_id)
        .expect("list")
        .into_iter()
        .find(|record| record.node_key == [0x01; 16])
        .expect("declared node")
        .node_id
}

fn raise_to_hot(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: nlos_types::TaskNodeId,
) {
    let steps = [
        (
            NodeResidencyTier::MetadataOnly,
            NodeResidencyTier::Cold,
            0x10,
        ),
        (NodeResidencyTier::Cold, NodeResidencyTier::Warm, 0x11),
        (NodeResidencyTier::Warm, NodeResidencyTier::Hot, 0x12),
    ];
    for (from, to, key) in steps {
        authority
            .record_residency_transition(ResidencyTransitionRequest {
                plan_id,
                node_id,
                from_tier: from,
                to_tier: to,
                expected_declared_revision: 1,
                idempotency_key: IdempotencyKey::from_bytes([key; 16]),
                transitioned_at_ms: 2_000,
            })
            .expect("raise toward HOT");
    }
}

fn user_version(path: &std::path::Path) -> i64 {
    let raw = rusqlite::Connection::open(path).expect("raw");
    raw.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version")
}

/// A newly declared node is unpinned: no voucher, and once raised to
/// HOT an evict-direction step (HOT→WARM) is still legal.
#[test]
fn nodes_default_to_unpinned() {
    let root = Root::new("default");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);

    let view = authority
        .inspect_node_pin(plan_id, node_id)
        .expect("inspect pin")
        .expect("node");
    assert!(!view.pinned);
    assert_eq!(view.transition_count, 0);
    assert_eq!(view.last_voucher, None);

    raise_to_hot(&authority, plan_id, node_id);
    authority
        .record_residency_transition(ResidencyTransitionRequest {
            plan_id,
            node_id,
            from_tier: NodeResidencyTier::Hot,
            to_tier: NodeResidencyTier::Warm,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x13; 16]),
            transitioned_at_ms: 2_100,
        })
        .expect("unpinned node allows HOT→WARM evict");
    assert_eq!(
        authority
            .inspect_node_residency(plan_id, node_id)
            .expect("residency after evict")
            .expect("node")
            .tier,
        NodeResidencyTier::Warm
    );
}

/// PINNED 拒绝/降级: pin a HOT node, HOT→WARM is typed-refused with
/// zero tier movement; unpin degrades the overlay and the same evict
/// step then records.
#[test]
fn pin_refuses_evict_and_unpin_degrades() {
    let root = Root::new("refuse-degrade");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);
    raise_to_hot(&authority, plan_id, node_id);

    let pinned = authority
        .record_node_pin(NodePinRequest {
            plan_id,
            node_id,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x80; 16]),
            transitioned_at_ms: 3_000,
        })
        .expect("pin");
    assert!(matches!(pinned, NodePinDecision::Recorded(_)));
    let pin_view = authority
        .inspect_node_pin(plan_id, node_id)
        .expect("inspect pin")
        .expect("node");
    assert!(pin_view.pinned);
    assert_eq!(pin_view.transition_count, 1);

    let refused = authority
        .record_residency_transition(ResidencyTransitionRequest {
            plan_id,
            node_id,
            from_tier: NodeResidencyTier::Hot,
            to_tier: NodeResidencyTier::Warm,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x81; 16]),
            transitioned_at_ms: 3_100,
        })
        .expect_err("PINNED node must refuse eviction");
    assert!(matches!(
        refused,
        PlanStoreError::PinnedNodeNotEvictable { node_id: denied, .. } if denied == node_id
    ));
    let residency = authority
        .inspect_node_residency(plan_id, node_id)
        .expect("residency")
        .expect("node");
    assert_eq!(residency.tier, NodeResidencyTier::Hot);

    let unpinned = authority
        .record_node_unpin(NodePinRequest {
            plan_id,
            node_id,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
            transitioned_at_ms: 3_200,
        })
        .expect("unpin degrades");
    assert!(matches!(unpinned, NodePinDecision::Recorded(_)));
    assert!(
        !authority
            .inspect_node_pin(plan_id, node_id)
            .expect("inspect after unpin")
            .expect("node")
            .pinned
    );

    authority
        .record_residency_transition(ResidencyTransitionRequest {
            plan_id,
            node_id,
            from_tier: NodeResidencyTier::Hot,
            to_tier: NodeResidencyTier::Warm,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x83; 16]),
            transitioned_at_ms: 3_300,
        })
        .expect("evict after unpin");
    assert_eq!(
        authority
            .inspect_node_residency(plan_id, node_id)
            .expect("residency after evict")
            .expect("node")
            .tier,
        NodeResidencyTier::Warm
    );
}

/// Pin/unpin replay is byte-equal; a rebound key conflicts; a second
/// pin on an already-pinned node is a typed CAS miss, not a silent
/// no-op.
#[test]
fn pin_is_idempotent_and_cas_safe() {
    let root = Root::new("idempotent");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);

    let request = NodePinRequest {
        plan_id,
        node_id,
        expected_declared_revision: 1,
        idempotency_key: IdempotencyKey::from_bytes([0x90; 16]),
        transitioned_at_ms: 4_000,
    };
    let first = authority.record_node_pin(request).expect("first pin");
    let replay = authority.record_node_pin(request).expect("replay pin");
    assert!(matches!(first, NodePinDecision::Recorded(_)));
    assert!(matches!(replay, NodePinDecision::Replayed(_)));
    assert_eq!(first.voucher(), replay.voucher());

    let rebound = authority
        .record_node_pin(NodePinRequest {
            plan_id,
            node_id,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x90; 16]),
            transitioned_at_ms: 4_001,
        })
        .expect_err("rebound key");
    assert!(matches!(rebound, PlanStoreError::IdempotencyConflict));

    let already = authority
        .record_node_pin(NodePinRequest {
            plan_id,
            node_id,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x91; 16]),
            transitioned_at_ms: 4_100,
        })
        .expect_err("second pin is a CAS miss");
    assert!(matches!(
        already,
        PlanStoreError::PinStateCasMismatch { .. }
    ));
}

#[test]
fn schema_v7_fresh_open_is_current_head() {
    let root = Root::new("schema");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    drop(authority);
    assert_eq!(user_version(&root.0), 7);
}
