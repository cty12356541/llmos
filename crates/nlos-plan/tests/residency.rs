//! Context residency tiering integration tests for `nlos-plan` schema v3
//! (W31-E, B4-5; v0.5 §25.2.1 `ResidencyClass`, 议题 28 定案 2).
//!
//! The residency axis is a *separate* axis from the §25.2.1 lifecycle
//! state machine: its own voucher table, its own dense per-node
//! sequence, its own tier CAS, and no coupling guard either way. The
//! lane gate is 分级读回 (typed tier readback) + evict 边界 (HOT→WARM→COLD
//! eviction durable, idempotent, replay-safe, METADATA facts preserved).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, MaterializationAdmission, MaterializationAdmissionVerdict,
    MaterializationRequest, MaterializationResolution, NodeResidencyTier, NodeTransitionRequest,
    PlanNodeDeclaration, PlanNodeKind, PlanNodeState, PlanStoreError, ResidencyTransitionDecision,
    ResidencyTransitionRequest, SqlitePlanAuthority,
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
            "nlos-plan-residency-{label}-{}-{nonce}-{}",
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
        .apply_plan_revision(revision_request(None, vec![node(0x01, 0x11)], 0x01))
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

fn residency_step(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: nlos_types::TaskNodeId,
    from: NodeResidencyTier,
    to: NodeResidencyTier,
    key: u8,
) -> Result<ResidencyTransitionDecision, PlanStoreError> {
    authority.record_residency_transition(ResidencyTransitionRequest {
        plan_id,
        node_id,
        from_tier: from,
        to_tier: to,
        expected_declared_revision: 1,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        transitioned_at_ms: 2_000,
    })
}

/// Walks a node up the whole chain, then evicts it back down to COLD,
/// returning the eviction vouchers (the §25.2.1 `EVICTED (residency=WARM|COLD)`
/// posture reached stepwise HOT→WARM→COLD).
fn raise_to_hot_then_evict_to_cold(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: nlos_types::TaskNodeId,
    key_base: u8,
) -> Vec<nlos_plan::ResidencyTransitionVoucher> {
    let up = [
        (NodeResidencyTier::MetadataOnly, NodeResidencyTier::Cold),
        (NodeResidencyTier::Cold, NodeResidencyTier::Warm),
        (NodeResidencyTier::Warm, NodeResidencyTier::Hot),
    ];
    for (index, (from, to)) in up.iter().enumerate() {
        let decision = residency_step(
            authority,
            plan_id,
            node_id,
            *from,
            *to,
            key_base + u8::try_from(index).expect("index fits u8"),
        )
        .expect("adjacent up-step");
        assert!(matches!(decision, ResidencyTransitionDecision::Recorded(_)));
    }
    let down = [
        (NodeResidencyTier::Hot, NodeResidencyTier::Warm),
        (NodeResidencyTier::Warm, NodeResidencyTier::Cold),
    ];
    down.iter()
        .enumerate()
        .map(|(index, (from, to))| {
            residency_step(
                authority,
                plan_id,
                node_id,
                *from,
                *to,
                key_base + 10 + u8::try_from(index).expect("index fits u8"),
            )
            .expect("adjacent evict-step")
            .voucher()
        })
        .collect()
}

fn user_version(db_path: &std::path::Path) -> i64 {
    let raw = Connection::open(db_path).expect("raw reader");
    raw.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version")
}

fn assert_integrity(db_path: &std::path::Path) {
    let raw = Connection::open(db_path).expect("raw reader");
    let integrity: String = raw
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity_check");
    assert_eq!(integrity, "ok");
}

#[test]
fn nodes_default_to_metadata_only_tier() {
    let root = Root::new("default");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);

    // `[SCALE-LOGICAL-001]`: a declared node holds only bounded durable
    // metadata — the residency axis starts at METADATA_ONLY with no
    // vouchers and no footprint claims.
    let record = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(record.residency_tier, NodeResidencyTier::MetadataOnly);
    assert_eq!(record.residency_transition_count, 0);

    let view = authority
        .inspect_node_residency(plan_id, node_id)
        .expect("residency view")
        .expect("node");
    assert_eq!(view.tier, NodeResidencyTier::MetadataOnly);
    assert_eq!(view.transition_count, 0);
    assert_eq!(view.last_voucher, None);

    // Unknown nodes read back `None` on both faces.
    let stranger = nlos_types::TaskNodeId::from_bytes([0xee; 16]);
    assert!(
        authority
            .inspect_node_residency(plan_id, stranger)
            .expect("view unknown")
            .is_none()
    );
}

#[test]
fn residency_round_trip_walks_adjacent_chain_and_reads_back() {
    let root = Root::new("roundtrip");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);

    let walk = [
        (
            NodeResidencyTier::MetadataOnly,
            NodeResidencyTier::Cold,
            0x21,
        ),
        (NodeResidencyTier::Cold, NodeResidencyTier::Warm, 0x22),
        (NodeResidencyTier::Warm, NodeResidencyTier::Hot, 0x23),
        (NodeResidencyTier::Hot, NodeResidencyTier::Running, 0x24),
        (NodeResidencyTier::Running, NodeResidencyTier::Hot, 0x25),
        (NodeResidencyTier::Hot, NodeResidencyTier::Warm, 0x26),
        (NodeResidencyTier::Warm, NodeResidencyTier::Cold, 0x27),
        (
            NodeResidencyTier::Cold,
            NodeResidencyTier::MetadataOnly,
            0x28,
        ),
        (
            NodeResidencyTier::MetadataOnly,
            NodeResidencyTier::Cold,
            0x29,
        ),
    ];
    for (index, (from, to, key)) in walk.iter().enumerate() {
        let decision =
            residency_step(&authority, plan_id, node_id, *from, *to, *key).expect("legal step");
        assert!(matches!(decision, ResidencyTransitionDecision::Recorded(_)));
        let voucher = decision.voucher();
        assert_eq!(voucher.transition_seq, index as u64 + 1);
        assert_eq!(voucher.from_tier, *from);
        assert_eq!(voucher.to_tier, *to);
        assert_eq!(voucher.observed_revision, 1);

        // Typed readback: the tier and the last transition voucher move
        // together (分级读回).
        let view = authority
            .inspect_node_residency(plan_id, node_id)
            .expect("view")
            .expect("node");
        assert_eq!(view.tier, *to);
        assert_eq!(view.transition_count, index as u64 + 1);
        assert_eq!(view.last_voucher.as_ref(), Some(&voucher));
    }

    let vouchers = authority
        .inspect_node_residency_vouchers(plan_id, node_id)
        .expect("voucher list");
    assert_eq!(vouchers.len(), walk.len());
    for (index, voucher) in vouchers.iter().enumerate() {
        assert_eq!(voucher.transition_seq, index as u64 + 1);
    }
    // The residency walk never touched the lifecycle axis.
    let record = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(record.state, PlanNodeState::Declared);
    assert_eq!(record.transition_count, 0);
}

#[test]
fn illegal_residency_edges_fail_typed() {
    let root = Root::new("illegal");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);

    // Conservative legal edge set: single-step adjacent moves along the
    // 议题 28 linear chain, both directions; self-loops and skips illegal.
    let tiers = [
        NodeResidencyTier::MetadataOnly,
        NodeResidencyTier::Cold,
        NodeResidencyTier::Warm,
        NodeResidencyTier::Hot,
        NodeResidencyTier::Running,
    ];
    for (from_index, from) in tiers.iter().enumerate() {
        for (to_index, to) in tiers.iter().enumerate() {
            let expected = from_index != to_index && from_index.abs_diff(to_index) == 1;
            assert_eq!(
                NodeResidencyTier::tier_transition_is_legal(*from, *to),
                expected,
                "edge {from:?} -> {to:?} legality mismatch"
            );
        }
    }

    // The typed negative matrix on the API face (legality is checked
    // before any CAS, so the node may sit at METADATA_ONLY throughout).
    let illegal_edges = [
        (NodeResidencyTier::MetadataOnly, NodeResidencyTier::Warm),
        (NodeResidencyTier::MetadataOnly, NodeResidencyTier::Hot),
        (NodeResidencyTier::MetadataOnly, NodeResidencyTier::Running),
        (NodeResidencyTier::Cold, NodeResidencyTier::Hot),
        (NodeResidencyTier::Cold, NodeResidencyTier::Running),
        (NodeResidencyTier::Warm, NodeResidencyTier::Running),
        (NodeResidencyTier::Running, NodeResidencyTier::Warm),
        (NodeResidencyTier::Running, NodeResidencyTier::MetadataOnly),
        (NodeResidencyTier::Hot, NodeResidencyTier::Cold),
        (NodeResidencyTier::Hot, NodeResidencyTier::MetadataOnly),
        (NodeResidencyTier::Cold, NodeResidencyTier::Cold),
        (NodeResidencyTier::Hot, NodeResidencyTier::Hot),
    ];
    for (index, (from, to)) in illegal_edges.iter().enumerate() {
        let error = residency_step(
            &authority,
            plan_id,
            node_id,
            *from,
            *to,
            0x40 + u8::try_from(index).expect("index fits u8"),
        )
        .expect_err("illegal residency edge must fail typed");
        assert!(
            matches!(
                error,
                PlanStoreError::IllegalResidencyTransition {
                    from: illegal_from,
                    to: illegal_to,
                    ..
                } if illegal_from == *from && illegal_to == *to
            ),
            "edge {from:?} -> {to:?} must fail IllegalResidencyTransition, got {error:?}"
        );
    }

    // Every refusal left the durable tier untouched.
    let view = authority
        .inspect_node_residency(plan_id, node_id)
        .expect("view")
        .expect("node");
    assert_eq!(view.tier, NodeResidencyTier::MetadataOnly);
    assert_eq!(view.transition_count, 0);

    // Unknown nodes fail typed on the write face.
    assert!(matches!(
        residency_step(
            &authority,
            plan_id,
            nlos_types::TaskNodeId::from_bytes([0xee; 16]),
            NodeResidencyTier::MetadataOnly,
            NodeResidencyTier::Cold,
            0x7f
        ),
        Err(PlanStoreError::NodeNotFound { .. })
    ));
}

#[test]
fn residency_transition_cas_fences_stale_tier_and_revision() {
    let root = Root::new("cas");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);

    // Tier CAS: the caller believes COLD, the durable tier is METADATA_ONLY.
    let stale_tier = residency_step(
        &authority,
        plan_id,
        node_id,
        NodeResidencyTier::Cold,
        NodeResidencyTier::Warm,
        0x51,
    )
    .expect_err("stale tier CAS");
    assert!(matches!(
        stale_tier,
        PlanStoreError::ResidencyTierCasMismatch {
            expected_from: NodeResidencyTier::Cold,
            current: NodeResidencyTier::MetadataOnly,
            ..
        }
    ));

    // Revision CAS: a reshaping revision fences in-flight residency moves
    // that observed the pre-reshape revision ([PLAN-DAG-001] fence,
    // same discipline as the lifecycle face).
    authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x01, 0x12)],
            0x02,
        ))
        .expect("revision 2 reshapes the pre-execution node");
    let stale_revision = authority
        .record_residency_transition(ResidencyTransitionRequest {
            plan_id,
            node_id,
            from_tier: NodeResidencyTier::MetadataOnly,
            to_tier: NodeResidencyTier::Cold,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x52; 16]),
            transitioned_at_ms: 2_000,
        })
        .expect_err("stale revision CAS");
    assert!(matches!(
        stale_revision,
        PlanStoreError::StaleNodeRevision {
            expected: 1,
            current: 2,
            ..
        }
    ));

    // With the observed revision corrected, the step commits; the reshape
    // never reset the residency axis.
    let decision = authority
        .record_residency_transition(ResidencyTransitionRequest {
            plan_id,
            node_id,
            from_tier: NodeResidencyTier::MetadataOnly,
            to_tier: NodeResidencyTier::Cold,
            expected_declared_revision: 2,
            idempotency_key: IdempotencyKey::from_bytes([0x53; 16]),
            transitioned_at_ms: 2_000,
        })
        .expect("legal step after fence");
    assert!(matches!(decision, ResidencyTransitionDecision::Recorded(_)));

    // A timestamp preceding durable history is refused (house rule).
    let early = authority
        .record_residency_transition(ResidencyTransitionRequest {
            plan_id,
            node_id,
            from_tier: NodeResidencyTier::Cold,
            to_tier: NodeResidencyTier::Warm,
            expected_declared_revision: 2,
            idempotency_key: IdempotencyKey::from_bytes([0x54; 16]),
            transitioned_at_ms: 1,
        })
        .expect_err("precedes first declaration");
    assert!(matches!(early, PlanStoreError::InvalidRequest { .. }));
}

#[test]
fn residency_eviction_is_idempotent_and_replay_safe() {
    let root = Root::new("evict");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);

    let eviction = raise_to_hot_then_evict_to_cold(&authority, plan_id, node_id, 0x60);
    assert_eq!(eviction.len(), 2);

    // Durable replay: every eviction key returns the original voucher
    // byte-equal, never a second effect.
    let replay_pairs = [
        (NodeResidencyTier::Hot, NodeResidencyTier::Warm, 0x6a),
        (NodeResidencyTier::Warm, NodeResidencyTier::Cold, 0x6b),
    ];
    for (from, to, key) in replay_pairs {
        let replay = residency_step(&authority, plan_id, node_id, from, to, key)
            .expect("replay eviction key");
        assert!(matches!(replay, ResidencyTransitionDecision::Replayed(_)));
        let voucher = replay.voucher();
        assert_eq!(voucher.from_tier, from);
        assert_eq!(voucher.to_tier, to);
    }

    // Rebinding a used key to different content is a typed conflict.
    let rebound = residency_step(
        &authority,
        plan_id,
        node_id,
        NodeResidencyTier::Hot,
        NodeResidencyTier::Running,
        0x6a,
    )
    .expect_err("rebound key");
    assert!(matches!(rebound, PlanStoreError::IdempotencyConflict));

    // Exactly one voucher per eviction key: 3 up-steps + 2 evict-steps.
    let vouchers = authority
        .inspect_node_residency_vouchers(plan_id, node_id)
        .expect("vouchers");
    assert_eq!(vouchers.len(), 5);
    let view = authority
        .inspect_node_residency(plan_id, node_id)
        .expect("view")
        .expect("node");
    assert_eq!(view.tier, NodeResidencyTier::Cold);
    assert_eq!(view.transition_count, 5);
    assert_eq!(view.last_voucher.as_ref(), Some(&eviction[1]));
}

#[test]
fn evicted_to_cold_node_preserves_metadata_facts() {
    let root = Root::new("metadata");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);
    // Cross the execution boundary through the W31-A materialization
    // gate so the node's shape is G1-frozen while it carries residency,
    // then evict: the frozen METADATA facts must survive the eviction
    // untouched (G2 posture).
    for (from, to, key) in [
        (PlanNodeState::Declared, PlanNodeState::Eligible, 0x71),
        (
            PlanNodeState::Eligible,
            PlanNodeState::WaitingAuthorization,
            0x72,
        ),
    ] {
        authority
            .record_node_transition(NodeTransitionRequest {
                plan_id,
                node_id,
                from_state: from,
                to_state: to,
                expected_declared_revision: 1,
                idempotency_key: IdempotencyKey::from_bytes([key; 16]),
                transitioned_at_ms: 2_000,
            })
            .expect("lifecycle step");
    }
    authority
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id,
            idempotency_key: IdempotencyKey::from_bytes([0x73; 16]),
            requested_at_ms: 2_000,
        })
        .expect("gate request");
    authority
        .resolve_materialization(MaterializationResolution {
            request_key: IdempotencyKey::from_bytes([0x73; 16]),
            verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
                profile_id: "task-10k".to_string(),
                projected_task_nodes: 1,
                projected_active_working_set: 1,
            }),
            resolved_at_ms: 2_100,
        })
        .expect("gate approval crosses the execution boundary");
    let gated = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(gated.state, PlanNodeState::Materializing);
    assert!(gated.state.is_execution_frozen());
    authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Materializing,
            to_state: PlanNodeState::Active,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x74; 16]),
            transitioned_at_ms: 2_200,
        })
        .expect("lifecycle step");
    let before = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect before")
        .expect("node before");
    assert!(before.state.is_execution_frozen());

    let eviction = raise_to_hot_then_evict_to_cold(&authority, plan_id, node_id, 0x75);

    let after = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect after")
        .expect("node after");
    assert_eq!(after.residency_tier, NodeResidencyTier::Cold);
    // Bounded METADATA facts are preserved through eviction.
    assert_eq!(after.node_key, before.node_key);
    assert_eq!(after.kind, before.kind);
    assert_eq!(after.declared_revision, before.declared_revision);
    assert_eq!(after.node_digest, before.node_digest);
    assert_eq!(after.state, PlanNodeState::Active);
    assert_eq!(after.transition_count, before.transition_count);
    assert_eq!(after.first_declared_at_ms, before.first_declared_at_ms);
    // The lifecycle vouchers are untouched by the residency walk.
    assert_eq!(
        authority
            .inspect_node_vouchers(plan_id, node_id)
            .expect("lifecycle vouchers")
            .len() as u64,
        before.transition_count
    );
    // The immutable revision chain still verifies.
    let verification = authority.verify_revision_chain(plan_id).expect("verify");
    assert_eq!(verification.head_revision, Some(1));
    // The graded readback reports the eviction landing tier with the
    // last voucher.
    let view = authority
        .inspect_node_residency(plan_id, node_id)
        .expect("view")
        .expect("node");
    assert_eq!(view.tier, NodeResidencyTier::Cold);
    assert_eq!(view.last_voucher.as_ref(), Some(&eviction[1]));
}

#[test]
fn residency_axis_is_orthogonal_to_lifecycle_state_machine() {
    let root = Root::new("orthogonal");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);

    // A still-DECLARED node may already hold COLD residency: the two axes
    // advance independently and neither observes the other's counters.
    authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Declared,
            to_state: PlanNodeState::Eligible,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x81; 16]),
            transitioned_at_ms: 2_000,
        })
        .expect("lifecycle step");
    residency_step(
        &authority,
        plan_id,
        node_id,
        NodeResidencyTier::MetadataOnly,
        NodeResidencyTier::Cold,
        0x82,
    )
    .expect("residency step on an ELIGIBLE node");

    let record = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(record.state, PlanNodeState::Eligible);
    assert_eq!(record.transition_count, 1);
    assert_eq!(record.residency_tier, NodeResidencyTier::Cold);
    assert_eq!(record.residency_transition_count, 1);

    assert_eq!(
        authority
            .inspect_node_vouchers(plan_id, node_id)
            .expect("lifecycle vouchers")
            .len(),
        1
    );
    assert_eq!(
        authority
            .inspect_node_residency_vouchers(plan_id, node_id)
            .expect("residency vouchers")
            .len(),
        1
    );

    // A residency eviction does not drive the lifecycle machine: the node
    // stays ELIGIBLE (only WAITING_* → MATERIALIZING would advance it).
    residency_step(
        &authority,
        plan_id,
        node_id,
        NodeResidencyTier::Cold,
        NodeResidencyTier::MetadataOnly,
        0x83,
    )
    .expect("evict to metadata");
    let record = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(record.state, PlanNodeState::Eligible);
    assert_eq!(record.transition_count, 1);
    assert_eq!(record.residency_tier, NodeResidencyTier::MetadataOnly);
    assert_eq!(record.residency_transition_count, 2);
}

#[test]
fn residency_restart_replay_converges() {
    let root = Root::new("restart");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);

    let up: [(NodeResidencyTier, NodeResidencyTier, u8); 3] = [
        (
            NodeResidencyTier::MetadataOnly,
            NodeResidencyTier::Cold,
            0x91,
        ),
        (NodeResidencyTier::Cold, NodeResidencyTier::Warm, 0x92),
        (NodeResidencyTier::Warm, NodeResidencyTier::Hot, 0x93),
    ];
    let evict: [(NodeResidencyTier, NodeResidencyTier, u8); 2] = [
        (NodeResidencyTier::Hot, NodeResidencyTier::Warm, 0x94),
        (NodeResidencyTier::Warm, NodeResidencyTier::Cold, 0x95),
    ];

    // Phase 1: raise to HOT before a crash/restart.
    let mut expected_vouchers = Vec::new();
    for (from, to, key) in up.iter().copied() {
        let decision =
            residency_step(&authority, plan_id, node_id, from, to, key).expect("up-step");
        assert!(matches!(decision, ResidencyTransitionDecision::Recorded(_)));
        expected_vouchers.push(decision.voucher());
    }
    drop(authority);

    // Phase 2 after restart: replaying the phase-1 keys answers from the
    // original vouchers (crash-retry semantics), then eviction proceeds.
    let authority = SqlitePlanAuthority::open(&db_path).expect("reopen mid-walk");
    for (index, (from, to, key)) in up.iter().copied().enumerate() {
        let replay = residency_step(&authority, plan_id, node_id, from, to, key)
            .expect("up-step replay after restart");
        assert!(matches!(replay, ResidencyTransitionDecision::Replayed(_)));
        assert_eq!(replay.voucher(), expected_vouchers[index]);
    }
    for (from, to, key) in evict.iter().copied() {
        let decision = residency_step(&authority, plan_id, node_id, from, to, key)
            .expect("evict-step after restart");
        assert!(matches!(decision, ResidencyTransitionDecision::Recorded(_)));
        expected_vouchers.push(decision.voucher());
    }
    drop(authority);

    let authority = SqlitePlanAuthority::open(&db_path).expect("reopen after eviction");
    let view = authority
        .inspect_node_residency(plan_id, node_id)
        .expect("view")
        .expect("node");
    assert_eq!(view.tier, NodeResidencyTier::Cold);
    assert_eq!(view.transition_count, 5);
    assert_eq!(view.last_voucher.as_ref(), Some(&expected_vouchers[4]));

    // A late replay of every key still answers from the original
    // vouchers; no second effect, no drift.
    for (index, (from, to, key)) in up.iter().chain(evict.iter()).copied().enumerate() {
        let replay =
            residency_step(&authority, plan_id, node_id, from, to, key).expect("late replay");
        assert!(matches!(replay, ResidencyTransitionDecision::Replayed(_)));
        assert_eq!(
            replay.voucher(),
            expected_vouchers[index],
            "late replay must return the original voucher"
        );
    }
    let final_view = authority
        .inspect_node_residency(plan_id, node_id)
        .expect("final view")
        .expect("node");
    assert_eq!(final_view.transition_count, 5);
    assert_integrity(&db_path);
}

#[test]
fn schema_v3_migration_paths() {
    let root = Root::new("migration");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("fresh open");
    assert_eq!(user_version(&db_path), 7);
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);
    let decision = residency_step(
        &authority,
        plan_id,
        node_id,
        NodeResidencyTier::MetadataOnly,
        NodeResidencyTier::Cold,
        0xa1,
    )
    .expect("one residency step");
    drop(authority);

    let reopened = SqlitePlanAuthority::open(&db_path).expect("reopen at v3");
    assert_eq!(user_version(&db_path), 7);
    let view = reopened
        .inspect_node_residency(plan_id, node_id)
        .expect("view after reopen")
        .expect("node after reopen");
    assert_eq!(view.tier, NodeResidencyTier::Cold);
    assert_eq!(
        view.last_voucher.as_ref(),
        Some(&decision.voucher()),
        "residency facts survive reopen"
    );
    drop(reopened);

    // A database stamped v2 whose v3 schema already exists re-migrates
    // idempotently (no partial-state error, version restored to the head).
    let raw = Connection::open(&db_path).expect("raw writer");
    raw.pragma_update(None, "user_version", 2)
        .expect("stamp v2");
    drop(raw);
    let remigrated = SqlitePlanAuthority::open(&db_path).expect("idempotent re-migration");
    assert_eq!(user_version(&db_path), 7);
    let view = remigrated
        .inspect_node_residency(plan_id, node_id)
        .expect("view after re-migration")
        .expect("node after re-migration");
    assert_eq!(view.tier, NodeResidencyTier::Cold);
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

#[test]
fn storage_triggers_guard_raw_residency_rewrites() {
    let root = Root::new("triggers");
    let authority = SqlitePlanAuthority::open(&root.0).expect("open");
    let plan_id = first_plan(&authority);
    let node_id = first_node_id(&authority, plan_id);
    let voucher = residency_step(
        &authority,
        plan_id,
        node_id,
        NodeResidencyTier::MetadataOnly,
        NodeResidencyTier::Cold,
        0xb1,
    )
    .expect("one step")
    .voucher();

    let raw = Connection::open(&root.0).expect("raw writer");
    // A raw skip-tier rewrite of the node row is aborted at the storage
    // layer (adjacency guard).
    assert!(
        raw.execute(
            "UPDATE plan_nodes SET residency_tier = 5
             WHERE plan_id = ?1 AND task_node_id = ?2",
            rusqlite::params![plan_id.as_bytes().as_slice(), node_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    // Residency vouchers are immutable and durable.
    assert!(
        raw.execute(
            "UPDATE plan_node_residency_transitions SET to_tier = 5 WHERE voucher_id = ?1",
            rusqlite::params![voucher.voucher_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM plan_node_residency_transitions WHERE voucher_id = ?1",
            rusqlite::params![voucher.voucher_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    drop(raw);

    let view = authority
        .inspect_node_residency(plan_id, node_id)
        .expect("view")
        .expect("node");
    assert_eq!(view.tier, NodeResidencyTier::Cold);
    assert_eq!(view.last_voucher.as_ref(), Some(&voucher));
    assert_integrity(&root.0);
}
