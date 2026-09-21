//! W31-A G3 falsification battery (ADR-0016 / 议题 35 §6 G3; v0.5 行 3650
//! `[PLAN-LAZY-001]`、行 4503 `[SCALE-MATERIALIZE-001]`): only a node whose
//! dependency readiness holds AND whose Task-side admission (`ScaleProfile`
//! working-set / `max_task_nodes` consult, ADR-0016 决定 4) approves may
//! enter `MATERIALIZING`; a window shrink must stop new materializations
//! and stay observable as a durable typed rejection, not a plan failure.
//!
//! The gate's falsification conditions are:
//! - "未满足依赖的节点可物化" — a node with unmet dependencies reaches
//!   `MATERIALIZING` through ANY face (gate approval, raw transition, or
//!   forged resolution);
//! - "窗口收缩后仍新增物化" — after the Task admission says no, any new
//!   node still enters `MATERIALIZING`, or the shrink leaves no durable
//!   typed trace.
//!
//! The consult wiring follows ADR-0013 verify-then-commit: the plan
//! authority records the readiness fact (the pending materialization
//! request), the Task authority's consumption path
//! (`SqliteTaskAuthority::answer_plan_materialization`) consults the
//! existing admission APIs, and the plan authority commits the verdict
//! (approval atomically flips `WAITING_* → MATERIALIZING`; rejection keeps
//! the node `WAITING_*` with a typed durable reason — the window shrinks,
//! the plan does not fail).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, MaterializationAdmissionVerdict, MaterializationRejection,
    MaterializationRequest, MaterializationRequestDecision, MaterializationRequestStatus,
    MaterializationResolution, MaterializationResolutionDecision, NodeTransitionDecision,
    NodeTransitionRequest, PlanNodeDeclaration, PlanNodeKind, PlanNodeState, PlanStoreError,
    ResidencyTransitionRequest, SqlitePlanAuthority,
};
use nlos_task::{
    MaterializationAdmissionFacts, ScaleProfile, SqliteTaskAuthority, TASK_PROFILE_10K,
    TaskStoreError,
};
use nlos_types::{IdempotencyKey, TaskNodeId, TaskPlanId};

/// Tier whose declared-TaskNode dimension (ADR-0016 决定 4) admits a
/// single declared node: with two declared nodes, every materialization
/// consult projects `declared + 1` over the cap and must deny.
static G3_NODE_CAP_ONE: ScaleProfile = ScaleProfile {
    profile_id: "task-g3-node-cap-1",
    max_task_nodes: 1,
    max_task_registrations: 100,
    max_active_working_set: 64,
    reclaim_threshold_ratio: None,
};

/// Tier whose working-set dimension is zero: every materialization
/// consult projects `active + 1 > 0` and must deny on
/// `WorkingSetAdmissionDenied`.
static G3_ZERO_WORKING_SET: ScaleProfile = ScaleProfile {
    profile_id: "task-g3-zero-working-set",
    max_task_nodes: 100_000,
    max_task_registrations: 100_000,
    max_active_working_set: 0,
    reclaim_threshold_ratio: None,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nlos-plan-g3-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create fixture directory");
        Self(path)
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

fn transition(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    from: PlanNodeState,
    to: PlanNodeState,
    key: u8,
) -> NodeTransitionDecision {
    authority
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: from,
            to_state: to,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            transitioned_at_ms: 3_000,
        })
        .expect("record legal node transition")
}

/// The W31-A consumption wiring: the Task authority answers one pending
/// plan-side materialization request through the existing admission APIs
/// (`enforce_task_node_admission` / `enforce_working_set_admission`), and
/// the plan authority commits the verdict. `other_declared_task_nodes` is
/// the plan authority's persisted declared-TaskNode count excluding the
/// candidate (the consult projects `+1` for the candidate itself).
fn consult_and_resolve(
    plan: &SqlitePlanAuthority,
    task: &SqliteTaskAuthority,
    request_key: IdempotencyKey,
    resolved_at_ms: u64,
) -> MaterializationResolutionDecision {
    let other_declared = plan
        .inspect_declared_task_node_count()
        .expect("declared task-node count")
        .saturating_sub(1);
    let verdict = match task.answer_plan_materialization(other_declared) {
        Ok(MaterializationAdmissionFacts {
            profile_id,
            projected_task_nodes,
            projected_active_working_set,
        }) => MaterializationAdmissionVerdict::Approved(nlos_plan::MaterializationAdmission {
            profile_id: profile_id.to_string(),
            projected_task_nodes,
            projected_active_working_set,
        }),
        Err(TaskStoreError::WorkingSetAdmissionDenied {
            profile_id,
            active_count,
            max_active_working_set,
        }) => MaterializationAdmissionVerdict::Rejected(MaterializationRejection::WorkingSetFull {
            profile_id: profile_id.to_string(),
            active_count,
            max_active_working_set,
        }),
        Err(TaskStoreError::TaskNodeAdmissionDenied {
            profile_id,
            task_count,
            max_task_nodes,
        }) => MaterializationAdmissionVerdict::Rejected(
            MaterializationRejection::TaskNodeCapExceeded {
                profile_id: profile_id.to_string(),
                task_count,
                max_task_nodes,
            },
        ),
        Err(other) => panic!("unexpected admission error: {other}"),
    };
    plan.resolve_materialization(MaterializationResolution {
        request_key,
        verdict,
        resolved_at_ms,
    })
    .expect("resolve materialization")
}

/// Full gated walk of one dependency-free node into `MATERIALIZING`.
fn gate_into_materializing(
    plan: &SqlitePlanAuthority,
    task: &SqliteTaskAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    key: u8,
) -> MaterializationResolutionDecision {
    plan.request_materialization(MaterializationRequest {
        plan_id,
        node_id,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: 2_000,
    })
    .expect("gate request");
    consult_and_resolve(plan, task, IdempotencyKey::from_bytes([key; 16]), 2_500)
}

/// The materialization window: nodes currently holding a
/// materialization seat (`MATERIALIZING/ACTIVE/CHECKPOINTED/
/// REHYDRATING`). `EVICTED` released its seat (residency WARM|COLD) and
/// terminal states never held one at read time.
fn materialized_count(plan: &SqlitePlanAuthority, plan_id: TaskPlanId) -> u64 {
    plan.list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .filter(|record| {
            matches!(
                record.state,
                PlanNodeState::Materializing
                    | PlanNodeState::Active
                    | PlanNodeState::Checkpointed
                    | PlanNodeState::Rehydrating
            )
        })
        .count() as u64
}

fn open_task(root: &Root, profile: &'static ScaleProfile) -> SqliteTaskAuthority {
    let path = root.0.join(format!(
        "task-{}.sqlite3",
        profile.profile_id.replace('-', "_")
    ));
    SqliteTaskAuthority::open_with_scale_profile(&path, profile).expect("open task authority")
}

/// G3 falsification #1: a node with unmet dependencies must have NO path
/// into `MATERIALIZING` — the gate refuses to even open a request (typed
/// `DependenciesNotReady`, the node durably `BLOCKED_DEPENDENCY`), a
/// forged resolution names no durable request, and the raw
/// `record_node_transition` face is blocked at the storage layer by the
/// materialization-gate trigger.
#[test]
#[allow(clippy::too_many_lines)] // One falsification test walks every bypass face end to end.
fn g3_unmet_dependency_cannot_materialize_through_any_face() {
    let root = Root::new("deps");
    let plan = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let task = open_task(&root, &TASK_PROFILE_10K);
    // A ← B ← C: B blocks on A, C blocks on B. A is walked to COMPLETED
    // through the gate so only C's dependency (B) stays unmet.
    let plan_id = plan
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![
                node(0x0a, 0x01),
                node_with_dependency(0x0b, 0x02, 0x0a),
                node_with_dependency(0x0c, 0x03, 0x0b),
            ],
            0x11,
        ))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    let node_a = node_id_of(&plan, plan_id, [0x0a; 16]);
    let node_c = node_id_of(&plan, plan_id, [0x0c; 16]);

    gate_into_materializing(&plan, &task, plan_id, node_a, 0x21);
    transition(
        &plan,
        plan_id,
        node_a,
        PlanNodeState::Materializing,
        PlanNodeState::Active,
        0x22,
    );
    transition(
        &plan,
        plan_id,
        node_a,
        PlanNodeState::Active,
        PlanNodeState::Completed,
        0x23,
    );

    // The falsification attempt: request materialization for C while its
    // dependency B is still DECLARED.
    let denied = plan
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id: node_c,
            idempotency_key: IdempotencyKey::from_bytes([0x31; 16]),
            requested_at_ms: 2_000,
        })
        .expect_err("unmet dependency must fail typed");
    assert!(matches!(
        &denied,
        PlanStoreError::DependenciesNotReady { node_id, unresolved }
            if *node_id == node_c && unresolved.len() == 1
    ));
    let c_row = plan
        .inspect_node(plan_id, node_c)
        .expect("inspect c")
        .expect("c");
    assert_eq!(c_row.state, PlanNodeState::BlockedDependency);

    // A forged approval names no durable request: fail typed, no state.
    let forged = plan
        .resolve_materialization(MaterializationResolution {
            request_key: IdempotencyKey::from_bytes([0x31; 16]),
            verdict: MaterializationAdmissionVerdict::Approved(
                nlos_plan::MaterializationAdmission {
                    profile_id: "task-10k".to_string(),
                    projected_task_nodes: 2,
                    projected_active_working_set: 1,
                },
            ),
            resolved_at_ms: 2_500,
        })
        .expect_err("no durable request exists");
    assert!(matches!(
        forged,
        PlanStoreError::MaterializationRequestNotFound(_)
    ));
    assert!(
        plan.inspect_materialization_request(IdempotencyKey::from_bytes([0x31; 16]))
            .expect("inspect request")
            .is_none()
    );

    // The raw face: drive C forward manually, then try to cross the
    // materialization boundary without any gate-approved request. The
    // storage-layer trigger must abort the voucher insert.
    transition(
        &plan,
        plan_id,
        node_c,
        PlanNodeState::BlockedDependency,
        PlanNodeState::Eligible,
        0x32,
    );
    transition(
        &plan,
        plan_id,
        node_c,
        PlanNodeState::Eligible,
        PlanNodeState::WaitingResource,
        0x33,
    );
    let raw_bypass = plan.record_node_transition(NodeTransitionRequest {
        plan_id,
        node_id: node_c,
        from_state: PlanNodeState::WaitingResource,
        to_state: PlanNodeState::Materializing,
        expected_declared_revision: 1,
        idempotency_key: IdempotencyKey::from_bytes([0x34; 16]),
        transitioned_at_ms: 3_000,
    });
    assert!(
        matches!(raw_bypass, Err(PlanStoreError::Sqlite(_))),
        "raw MATERIALIZING entry without gate approval must abort at the storage layer, got {raw_bypass:?}"
    );
    let c_after = plan
        .inspect_node(plan_id, node_c)
        .expect("inspect c")
        .expect("c");
    assert_eq!(c_after.state, PlanNodeState::WaitingResource);
    let crossed = plan
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .filter(|record| record.state.is_execution_frozen())
        .count();
    assert_eq!(crossed, 1, "only node A crossed the boundary");
}

/// G3 falsification #2: when the Task admission says no (task-node
/// dimension over cap), approval is impossible through the consult
/// wiring, the rejection is durable and typed, the node stays
/// `WAITING_RESOURCE`, and the materialization window stops growing.
#[test]
#[allow(clippy::too_many_lines)] // One falsification test covers denial, durability, replay, and re-open.
fn g3_admission_denial_shrinks_window_with_typed_durable_reason() {
    let root = Root::new("shrink");
    let plan = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let task = open_task(&root, &G3_NODE_CAP_ONE);
    let plan_id = plan
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x11,
        ))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    let node_a = node_id_of(&plan, plan_id, [0x0a; 16]);
    let node_b = node_id_of(&plan, plan_id, [0x0b; 16]);

    plan.request_materialization(MaterializationRequest {
        plan_id,
        node_id: node_a,
        idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
        requested_at_ms: 2_000,
    })
    .expect("gate request drives A to WAITING_RESOURCE");

    // The consult: two declared nodes against a one-node tier projects
    // over cap — admission says no.
    let decision = consult_and_resolve(&plan, &task, IdempotencyKey::from_bytes([0x21; 16]), 2_500);
    let MaterializationResolutionDecision::Rejected(record) = &decision else {
        panic!("task-node over-cap consult must reject, got {decision:?}");
    };
    assert_eq!(record.status, MaterializationRequestStatus::Rejected);
    assert_eq!(
        record.rejection.as_ref(),
        Some(&MaterializationRejection::TaskNodeCapExceeded {
            profile_id: "task-g3-node-cap-1".to_string(),
            task_count: 2,
            max_task_nodes: 1,
        }),
        "the typed window-shrink reason must survive the durable readback"
    );
    assert_eq!(record.resolved_at_ms, Some(2_500));

    // The window shrank: A never entered MATERIALIZING and stays WAITING.
    let a_row = plan
        .inspect_node(plan_id, node_a)
        .expect("inspect a")
        .expect("a");
    assert_eq!(a_row.state, PlanNodeState::WaitingResource);
    assert_eq!(materialized_count(&plan, plan_id), 0);
    let b_row = plan
        .inspect_node(plan_id, node_b)
        .expect("inspect b")
        .expect("b");
    assert_eq!(
        b_row.state,
        PlanNodeState::Declared,
        "the plan did not fail: B is untouched"
    );

    // The raw face stays closed for the rejected node.
    let raw_bypass = plan.record_node_transition(NodeTransitionRequest {
        plan_id,
        node_id: node_a,
        from_state: PlanNodeState::WaitingResource,
        to_state: PlanNodeState::Materializing,
        expected_declared_revision: 1,
        idempotency_key: IdempotencyKey::from_bytes([0x2f; 16]),
        transitioned_at_ms: 3_000,
    });
    assert!(
        matches!(raw_bypass, Err(PlanStoreError::Sqlite(_))),
        "no path into MATERIALIZING after rejection, got {raw_bypass:?}"
    );

    // The durable rejection replays byte-equal; a different verdict on the
    // resolved request is a typed idempotency conflict.
    let replay = consult_and_resolve(&plan, &task, IdempotencyKey::from_bytes([0x21; 16]), 2_500);
    assert!(matches!(
        replay,
        MaterializationResolutionDecision::ReplayedRejected(_)
    ));
    let rebound = plan
        .resolve_materialization(MaterializationResolution {
            request_key: IdempotencyKey::from_bytes([0x21; 16]),
            verdict: MaterializationAdmissionVerdict::Approved(
                nlos_plan::MaterializationAdmission {
                    profile_id: "task-g3-node-cap-1".to_string(),
                    projected_task_nodes: 2,
                    projected_active_working_set: 1,
                },
            ),
            resolved_at_ms: 2_500,
        })
        .expect_err("rebinding a resolved request must fail typed");
    assert!(matches!(rebound, PlanStoreError::IdempotencyConflict));

    // The window can re-open only through a fresh gated request once the
    // tier admits (retry under a wider profile).
    let wide = open_task(&root, &TASK_PROFILE_10K);
    plan.request_materialization(MaterializationRequest {
        plan_id,
        node_id: node_a,
        idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
        requested_at_ms: 4_000,
    })
    .expect("retry with a fresh key after the shrink");
    let recovered =
        consult_and_resolve(&plan, &wide, IdempotencyKey::from_bytes([0x22; 16]), 4_500);
    assert!(matches!(
        recovered,
        MaterializationResolutionDecision::Approved(_)
    ));
    assert_eq!(materialized_count(&plan, plan_id), 1);

    // Both requests are durable history on the node (rejected + approved).
    let history = plan
        .inspect_node_materialization_requests(plan_id, node_a)
        .expect("request history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].status, MaterializationRequestStatus::Rejected);
    assert_eq!(history[1].status, MaterializationRequestStatus::Approved);
}

/// G3 working-set dimension: a zero working-set tier denies every
/// materialization with the typed `WorkingSetFull` reason and the window
/// never grows.
#[test]
fn g3_working_set_dimension_denies_and_window_stops_growing() {
    let root = Root::new("ws");
    let plan = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let task = open_task(&root, &G3_ZERO_WORKING_SET);
    let plan_id = plan
        .apply_plan_revision_ungated(revision_request(None, vec![node(0x0a, 0x01)], 0x11))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    let node_a = node_id_of(&plan, plan_id, [0x0a; 16]);

    let decision = gate_into_materializing(&plan, &task, plan_id, node_a, 0x21);
    let MaterializationResolutionDecision::Rejected(record) = &decision else {
        panic!("zero working-set tier must reject, got {decision:?}");
    };
    assert_eq!(
        record.rejection.as_ref(),
        Some(&MaterializationRejection::WorkingSetFull {
            profile_id: "task-g3-zero-working-set".to_string(),
            active_count: 1,
            max_active_working_set: 0,
        })
    );
    let a_row = plan
        .inspect_node(plan_id, node_a)
        .expect("inspect a")
        .expect("a");
    assert_eq!(a_row.state, PlanNodeState::WaitingResource);
    assert_eq!(materialized_count(&plan, plan_id), 0);
}

/// G3 window-shrink response composes with the checkpoint/evict faces
/// landed in W28-A/W31-E: after a shrink rejection, an already
/// materialized node walks `ACTIVE → CHECKPOINTED → EVICTED` on the
/// lifecycle axis and `HOT → WARM → COLD` on the residency axis
/// (`[SCALE-MATERIALIZE-001]` shrink policy posture), and re-enters
/// `MATERIALIZING` only through a fresh gated request.
#[test]
#[allow(clippy::too_many_lines)] // One test walks the full shrink response across both axes.
fn g3_window_shrink_composes_with_checkpoint_evict_and_residency_eviction() {
    let root = Root::new("evict");
    let plan = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let wide = open_task(&root, &TASK_PROFILE_10K);
    let tiny = open_task(&root, &G3_NODE_CAP_ONE);
    let plan_id = plan
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x11,
        ))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    let node_a = node_id_of(&plan, plan_id, [0x0a; 16]);
    let node_b = node_id_of(&plan, plan_id, [0x0b; 16]);

    // A materializes under the wide tier and becomes ACTIVE/HOT.
    gate_into_materializing(&plan, &wide, plan_id, node_a, 0x21);
    transition(
        &plan,
        plan_id,
        node_a,
        PlanNodeState::Materializing,
        PlanNodeState::Active,
        0x22,
    );
    for (from, to, key) in [
        (
            nlos_plan::NodeResidencyTier::MetadataOnly,
            nlos_plan::NodeResidencyTier::Cold,
            0x81,
        ),
        (
            nlos_plan::NodeResidencyTier::Cold,
            nlos_plan::NodeResidencyTier::Warm,
            0x82,
        ),
        (
            nlos_plan::NodeResidencyTier::Warm,
            nlos_plan::NodeResidencyTier::Hot,
            0x83,
        ),
    ] {
        plan.record_residency_transition(ResidencyTransitionRequest {
            plan_id,
            node_id: node_a,
            from_tier: from,
            to_tier: to,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            transitioned_at_ms: 3_000,
        })
        .expect("residency step");
    }

    // The window shrinks: B's consult under the tiny tier rejects.
    plan.request_materialization(MaterializationRequest {
        plan_id,
        node_id: node_b,
        idempotency_key: IdempotencyKey::from_bytes([0x31; 16]),
        requested_at_ms: 3_500,
    })
    .expect("gate request for B");
    let shrunk = consult_and_resolve(&plan, &tiny, IdempotencyKey::from_bytes([0x31; 16]), 3_600);
    assert!(matches!(
        shrunk,
        MaterializationResolutionDecision::Rejected(_)
    ));

    // Shrink policy: stop new materializations (B stays WAITING_RESOURCE)
    // and evict the recoverable A down both axes.
    let b_row = plan
        .inspect_node(plan_id, node_b)
        .expect("inspect b")
        .expect("b");
    assert_eq!(b_row.state, PlanNodeState::WaitingResource);
    transition(
        &plan,
        plan_id,
        node_a,
        PlanNodeState::Active,
        PlanNodeState::Checkpointed,
        0x23,
    );
    transition(
        &plan,
        plan_id,
        node_a,
        PlanNodeState::Checkpointed,
        PlanNodeState::Evicted,
        0x24,
    );
    for (from, to, key) in [
        (
            nlos_plan::NodeResidencyTier::Hot,
            nlos_plan::NodeResidencyTier::Warm,
            0x84,
        ),
        (
            nlos_plan::NodeResidencyTier::Warm,
            nlos_plan::NodeResidencyTier::Cold,
            0x85,
        ),
    ] {
        plan.record_residency_transition(ResidencyTransitionRequest {
            plan_id,
            node_id: node_a,
            from_tier: from,
            to_tier: to,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            transitioned_at_ms: 3_800,
        })
        .expect("residency eviction step");
    }
    let a_row = plan
        .inspect_node(plan_id, node_a)
        .expect("inspect a")
        .expect("a");
    assert_eq!(a_row.state, PlanNodeState::Evicted);
    assert_eq!(a_row.residency_tier, nlos_plan::NodeResidencyTier::Cold);
    assert_eq!(
        materialized_count(&plan, plan_id),
        0,
        "the window fully shrank"
    );

    // Rehydrate + re-materialize: only through a fresh gated request.
    transition(
        &plan,
        plan_id,
        node_a,
        PlanNodeState::Evicted,
        PlanNodeState::Rehydrating,
        0x25,
    );
    let reentry = gate_into_materializing(&plan, &wide, plan_id, node_a, 0x26);
    assert!(matches!(
        reentry,
        MaterializationResolutionDecision::Approved(_)
    ));
    let a_final = plan
        .inspect_node(plan_id, node_a)
        .expect("inspect a")
        .expect("a");
    assert_eq!(a_final.state, PlanNodeState::Materializing);
}

/// Gate authority semantics: the request drives the legal edge walk
/// `DECLARED → ELIGIBLE → WAITING_RESOURCE` as dense vouchers, is
/// idempotent by key, and refuses a second concurrent pending request on
/// the same node.
#[test]
fn gate_request_drives_legal_edges_is_idempotent_and_single_pending() {
    let root = Root::new("auth");
    let plan = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let plan_id = plan
        .apply_plan_revision_ungated(revision_request(None, vec![node(0x0a, 0x01)], 0x11))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    let node_a = node_id_of(&plan, plan_id, [0x0a; 16]);

    let request = MaterializationRequest {
        plan_id,
        node_id: node_a,
        idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
        requested_at_ms: 2_000,
    };
    let first = plan
        .request_materialization(request)
        .expect("first request");
    assert!(matches!(
        first,
        MaterializationRequestDecision::Requested(_)
    ));
    let replay = plan
        .request_materialization(request)
        .expect("replay request");
    assert!(matches!(
        replay,
        MaterializationRequestDecision::Replayed(_)
    ));

    let row = plan
        .inspect_node(plan_id, node_a)
        .expect("inspect a")
        .expect("a");
    assert_eq!(row.state, PlanNodeState::WaitingResource);
    assert_eq!(
        row.transition_count, 2,
        "the gate drove ELIGIBLE then WAITING_RESOURCE"
    );
    let vouchers = plan
        .inspect_node_vouchers(plan_id, node_a)
        .expect("vouchers");
    assert_eq!(vouchers[0].to_state, PlanNodeState::Eligible);
    assert_eq!(vouchers[1].to_state, PlanNodeState::WaitingResource);

    let second_pending = plan
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id: node_a,
            idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
            requested_at_ms: 2_100,
        })
        .expect_err("one pending request per node");
    assert!(matches!(
        second_pending,
        PlanStoreError::MaterializationRequestAlreadyPending { .. }
    ));

    // A node past the boundary is not requestable.
    let wide = open_task(&root, &TASK_PROFILE_10K);
    let approved = consult_and_resolve(&plan, &wide, IdempotencyKey::from_bytes([0x21; 16]), 2_500);
    assert!(matches!(
        approved,
        MaterializationResolutionDecision::Approved(_)
    ));
    let re_request = plan
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id: node_a,
            idempotency_key: IdempotencyKey::from_bytes([0x23; 16]),
            requested_at_ms: 2_600,
        })
        .expect_err("materializing node cannot re-request");
    assert!(matches!(
        re_request,
        PlanStoreError::NodeNotAwaitingMaterialization { .. }
    ));
}

/// Gate fence: a reshaping revision fences an in-flight request — the
/// approval's declared-revision CAS fails typed and the caller re-requests
/// against the new revision.
#[test]
fn gate_resolve_is_fenced_by_declared_revision_cas() {
    let root = Root::new("fence");
    let plan = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let plan_id = plan
        .apply_plan_revision_ungated(revision_request(
            None,
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x11,
        ))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    let node_a = node_id_of(&plan, plan_id, [0x0a; 16]);

    plan.request_materialization(MaterializationRequest {
        plan_id,
        node_id: node_a,
        idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
        requested_at_ms: 2_000,
    })
    .expect("request at revision 1");

    // Reshape the (still pre-execution) node with revision 2.
    plan.apply_plan_revision_ungated(revision_request(
        Some(plan_id),
        vec![node(0x0a, 0x7f), node(0x0b, 0x02)],
        0x12,
    ))
    .expect("apply revision 2");

    let fenced = plan
        .resolve_materialization(MaterializationResolution {
            request_key: IdempotencyKey::from_bytes([0x21; 16]),
            verdict: MaterializationAdmissionVerdict::Approved(
                nlos_plan::MaterializationAdmission {
                    profile_id: "task-10k".to_string(),
                    projected_task_nodes: 2,
                    projected_active_working_set: 1,
                },
            ),
            resolved_at_ms: 2_500,
        })
        .expect_err("stale request revision must fence the approval");
    assert!(matches!(
        fenced,
        PlanStoreError::StaleNodeRevision {
            expected: 1,
            current: 2,
            ..
        }
    ));

    let a_row = plan
        .inspect_node(plan_id, node_a)
        .expect("inspect a")
        .expect("a");
    assert_eq!(a_row.state, PlanNodeState::WaitingResource);
    assert_eq!(a_row.declared_revision, 2);
    assert_eq!(materialized_count(&plan, plan_id), 0);
}
