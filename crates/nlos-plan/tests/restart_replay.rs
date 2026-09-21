//! Restart replay + idempotency convergence: a crash between any two
//! durable effects leaves the authority at a complete prefix, and key
//! replay after restart converges to the same state without double-applied
//! revisions or transitions (ADR-0016 决定 2 skeleton, house pattern of
//! `semantic_recovery_ledger.rs` restart rounds).
//!
//! Each round drops the authority (process-death stand-in; hard kill-9 and
//! power-loss rows are the fault matrix) and reopens the same durable
//! bytes. The final state is asserted bitwise: one receipt per revision,
//! dense vouchers, frozen node untouched, chain intact.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, MaterializationAdmission, MaterializationAdmissionVerdict,
    MaterializationRequest, MaterializationRequestDecision, MaterializationResolution,
    MaterializationResolutionDecision, NodeTransitionDecision, NodeTransitionRequest,
    PlanNodeDeclaration, PlanNodeKind, PlanNodeState, PlanRevisionDecision, PlanStoreError,
    SqlitePlanAuthority,
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
            "nlos-plan-replay-{label}-{}-{nonce}-{}",
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

fn transition_request(
    plan_id: TaskPlanId,
    node_id: nlos_types::TaskNodeId,
    from: PlanNodeState,
    to: PlanNodeState,
    expected_revision: u64,
    key: u8,
) -> NodeTransitionRequest {
    NodeTransitionRequest {
        plan_id,
        node_id,
        from_state: from,
        to_state: to,
        expected_declared_revision: expected_revision,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        transitioned_at_ms: 2_000,
    }
}

fn raw_count(path: &std::path::Path, table: &str) -> i64 {
    let connection = Connection::open(path).expect("raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn assert_integrity(path: &std::path::Path) {
    let connection = Connection::open(path).expect("integrity reader");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity check");
    assert_eq!(result, "ok");
}

/// One durable effect per round, restart between every pair, replay each
/// key after the restart, and converge: the executed node keeps its
/// revision, revisions chain, vouchers stay dense, nothing double-applies.
#[test]
#[allow(clippy::too_many_lines)] // One test walks the full restart-between-effects sequence.
fn restart_between_every_effect_replays_once_and_converges() {
    let root = Root::new("rounds");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let mut authority = SqlitePlanAuthority::open(&db_path).expect("open");

    // Effect 1: revision 1 (plan creation). Crash. Replay converges.
    let first_receipt = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x11,
        ))
        .expect("revision 1")
        .receipt();
    let plan_id = first_receipt.plan_id;
    let node_a = authority
        .list_plan_nodes(plan_id)
        .expect("list")
        .into_iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("node a")
        .node_id;
    drop(authority);
    authority = SqlitePlanAuthority::open(&db_path).expect("reopen after effect 1");

    let replay = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x11,
        ))
        .expect("replay revision 1 after restart");
    assert_eq!(replay.clone().receipt(), first_receipt);
    assert!(matches!(replay, PlanRevisionDecision::Replayed(_)));
    assert_eq!(raw_count(&db_path, "plans"), 1);
    assert_eq!(raw_count(&db_path, "plan_revisions"), 1);
    assert_eq!(raw_count(&db_path, "plan_nodes"), 2);

    // Effect 2..4: walk node a across the execution boundary with a
    // restart between every effect; every key replays exactly once. The
    // `→ MATERIALIZING` entry goes through the W31-A gate (one pending
    // request effect + one approving resolution effect), since the raw
    // materializing edge is storage-gated.
    let walk = [
        (PlanNodeState::Declared, PlanNodeState::Eligible, 0xc1),
        (
            PlanNodeState::Eligible,
            PlanNodeState::WaitingAuthorization,
            0xc2,
        ),
    ];
    for (from, to, key) in walk {
        let recorded = authority
            .record_node_transition(transition_request(plan_id, node_a, from, to, 1, key))
            .expect("voucher");
        assert!(matches!(recorded, NodeTransitionDecision::Recorded(_)));
        drop(authority);
        authority = SqlitePlanAuthority::open(&db_path).expect("reopen between effects");

        let replayed = authority
            .record_node_transition(transition_request(plan_id, node_a, from, to, 1, key))
            .expect("voucher replay");
        assert!(matches!(replayed, NodeTransitionDecision::Replayed(_)));
        let row = authority
            .inspect_node(plan_id, node_a)
            .expect("inspect")
            .expect("node");
        assert_eq!(row.state, to, "replay does not advance the state twice");
    }

    // Effect 3a: the gate request. Crash. Replay converges.
    let gate_request = MaterializationRequest {
        plan_id,
        node_id: node_a,
        idempotency_key: IdempotencyKey::from_bytes([0xc3; 16]),
        requested_at_ms: 2_000,
    };
    let requested = authority
        .request_materialization(gate_request)
        .expect("gate request");
    assert!(matches!(
        requested,
        MaterializationRequestDecision::Requested(_)
    ));
    drop(authority);
    authority = SqlitePlanAuthority::open(&db_path).expect("reopen after gate request");
    let request_replay = authority
        .request_materialization(gate_request)
        .expect("gate request replay");
    assert!(matches!(
        request_replay,
        MaterializationRequestDecision::Replayed(_)
    ));

    // Effect 3b: the approving resolution. Crash. Replay converges.
    let gate_resolution = MaterializationResolution {
        request_key: IdempotencyKey::from_bytes([0xc3; 16]),
        verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
            profile_id: "task-10k".to_string(),
            projected_task_nodes: 2,
            projected_active_working_set: 1,
        }),
        resolved_at_ms: 2_500,
    };
    let approved = authority
        .resolve_materialization(gate_resolution.clone())
        .expect("gate approval");
    assert!(matches!(
        approved,
        MaterializationResolutionDecision::Approved(_)
    ));
    drop(authority);
    authority = SqlitePlanAuthority::open(&db_path).expect("reopen after gate approval");
    let approval_replay = authority
        .resolve_materialization(gate_resolution)
        .expect("gate approval replay");
    assert!(matches!(
        approval_replay,
        MaterializationResolutionDecision::ReplayedApproved(_)
    ));
    assert_eq!(raw_count(&db_path, "plan_materialization_requests"), 1);
    let frozen = authority
        .inspect_node(plan_id, node_a)
        .expect("inspect")
        .expect("node");
    assert_eq!(frozen.state, PlanNodeState::Materializing);
    assert_eq!(frozen.transition_count, 3);

    // A rebound transition key after restart is a typed conflict, never a
    // second effect.
    let rebound = authority.record_node_transition(transition_request(
        plan_id,
        node_a,
        PlanNodeState::Declared,
        PlanNodeState::BlockedDependency,
        1,
        0xc1,
    ));
    assert!(matches!(rebound, Err(PlanStoreError::IdempotencyConflict)));

    // Effect 5: revision 2 (frozen node re-declared bit-identically, node
    // b reshaped, node c added). Crash. Replay converges.
    let second_receipt = authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x01), node(0x0b, 0x42), node(0x0c, 0x43)],
            0x12,
        ))
        .expect("revision 2")
        .receipt();
    drop(authority);
    authority = SqlitePlanAuthority::open(&db_path).expect("reopen after effect 5");

    let replay2 = authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![node(0x0a, 0x01), node(0x0b, 0x42), node(0x0c, 0x43)],
            0x12,
        ))
        .expect("replay revision 2");
    assert_eq!(replay2.clone().receipt(), second_receipt);
    assert!(matches!(replay2, PlanRevisionDecision::Replayed(_)));
    assert_eq!(raw_count(&db_path, "plan_revisions"), 2);

    // Converged terminal state: frozen node keeps revision 1 with its
    // original digest, pre-execution nodes advanced, chain verifies.
    let converged_a = authority
        .inspect_node(plan_id, node_a)
        .expect("inspect a")
        .expect("node a");
    assert_eq!(converged_a.declared_revision, 1);
    assert_eq!(converged_a.state, PlanNodeState::Materializing);
    assert_eq!(converged_a.node_digest, frozen.node_digest);
    let converged_b = authority
        .inspect_node(
            plan_id,
            authority
                .list_plan_nodes(plan_id)
                .expect("list")
                .into_iter()
                .find(|record| record.node_key == [0x0b; 16])
                .expect("node b")
                .node_id,
        )
        .expect("inspect b")
        .expect("node b");
    assert_eq!(converged_b.declared_revision, 2);
    assert_eq!(raw_count(&db_path, "plan_nodes"), 3);
    assert_eq!(
        authority
            .inspect_node_vouchers(plan_id, node_a)
            .expect("vouchers")
            .len(),
        3
    );
    let verification = authority.verify_revision_chain(plan_id).expect("chain");
    assert_eq!(verification.revision_count, 2);
    assert_eq!(verification.head_digest, Some(second_receipt.plan_digest));
    assert_integrity(&db_path);
}

/// The pre-restart view of a node (state, revision) stays fenced after the
/// restart: replaying an in-flight stale transition cannot double-apply.
#[test]
fn restart_preserves_cas_fences_against_pre_crash_views() {
    let root = Root::new("fence");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let authority = SqlitePlanAuthority::open(&db_path).expect("open");
    let plan_id = authority
        .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x01)], 0x21))
        .expect("revision 1")
        .receipt()
        .plan_id;
    let node_id = authority
        .list_plan_nodes(plan_id)
        .expect("list")
        .into_iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("node")
        .node_id;
    authority
        .record_node_transition(transition_request(
            plan_id,
            node_id,
            PlanNodeState::Declared,
            PlanNodeState::Eligible,
            1,
            0xd1,
        ))
        .expect("advance to eligible");
    drop(authority);

    let reopened = SqlitePlanAuthority::open(&db_path).expect("reopen");
    let stale = reopened.record_node_transition(transition_request(
        plan_id,
        node_id,
        PlanNodeState::Declared,
        PlanNodeState::Eligible,
        1,
        0xd2,
    ));
    assert!(matches!(
        stale,
        Err(PlanStoreError::NodeStateCasMismatch {
            expected_from: PlanNodeState::Declared,
            current: PlanNodeState::Eligible,
            ..
        })
    ));
    assert_eq!(
        reopened
            .inspect_node(plan_id, node_id)
            .expect("inspect")
            .expect("node")
            .transition_count,
        1,
        "exactly one voucher survives the restart"
    );
    assert_integrity(&db_path);
}
