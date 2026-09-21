//! W31-F two-tier materialization scheduler battery (B4-6; v0.5 §25.2.2
//! minimal two-layer form, 行 4503 `[SCALE-MATERIALIZE-001]` window
//! shaping, 行 4538 `[SCHED-BACKPRESSURE-001]` posture).
//!
//! The minimal form is "足以支撑 benchmark 的两层形态", not a full OS
//! scheduler:
//!
//! - **Global tier** (`MaterializationScheduler::select`): a pure
//!   eligibility scan over durable plan state — dependency-ready nodes
//!   that can still await materialization, ordered ready-FIFO
//!   (`first_declared_at_ms`, then `TaskNodeId` bytes), bounded by the
//!   materialization window's free seats;
//! - **Worker tier** (`MaterializationScheduler::drive`): maps each
//!   selection to the W31-A gate (`request_materialization` → admission
//!   consult → `resolve_materialization`); an admission rejection shrinks
//!   the window (one seat fewer per rejection, floor 1 so pressure stays
//!   observable as a durable probe), and a crashed gate round (a durable
//!   `PENDING` request) is adopted and resolved by the next pass;
//! - **inspectability**: the recent decision trail (selected/skipped +
//!   reason, approved/rejected) is a bounded in-memory readback — the
//!   durable audit trail remains the plan authority's request rows and
//!   vouchers (W31-A faces).
//!
//! The scheduler may not materialize anything past the admission
//! consult (no-bypass), and it makes no dispatch decisions — 派发决策留
//! 控制器; it only selects nodes for materialization and drives the gate.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    AdmissionConsult, AdmissionConsultOutcome, ApplyPlanRevisionRequest, MaterializationAdmission,
    MaterializationRejection, MaterializationRequest, MaterializationScheduler,
    PlanNodeDeclaration, PlanNodeKind, PlanNodeState, PlanStoreError, SchedulerDecision,
    SelectionKind, SelectionSkipReason, SqlitePlanAuthority,
};
use nlos_task::{
    MaterializationAdmissionFacts, ScaleProfile, SqliteTaskAuthority, TASK_PROFILE_10K,
    TaskStoreError,
};
use nlos_types::{IdempotencyKey, TaskNodeId, TaskPlanId};

/// Tier whose working-set dimension is zero: every consult projects
/// `active + 1 > 0` and denies (`WorkingSetAdmissionDenied`).
static ZERO_WORKING_SET: ScaleProfile = ScaleProfile {
    profile_id: "task-sched-zero-working-set",
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
            "nlos-plan-sched-{label}-{}-{nonce}-{}",
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
    applied_at_ms: u64,
) -> ApplyPlanRevisionRequest {
    ApplyPlanRevisionRequest {
        plan_id,
        nodes,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        applied_at_ms,
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

/// Nodes currently holding a materialization seat (the W31-A window
/// band: `MATERIALIZING/ACTIVE/CHECKPOINTED/REHYDRATING`).
fn materialized_count(authority: &SqlitePlanAuthority, plan_id: TaskPlanId) -> u64 {
    authority
        .list_plan_nodes(plan_id)
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

/// The ready-FIFO order the Global tier must produce for `keys`:
/// `(first_declared_at_ms, TaskNodeId bytes)`, computed independently
/// from the plain `list_plan_nodes` read face.
fn expected_fifo_order(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    keys: &[[u8; 16]],
) -> Vec<TaskNodeId> {
    let mut records: Vec<_> = authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .filter(|record| keys.contains(&record.node_key))
        .collect();
    records.sort_by(|a, b| {
        a.first_declared_at_ms
            .cmp(&b.first_declared_at_ms)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    records.into_iter().map(|record| record.node_id).collect()
}

fn open_task(root: &Root, profile: &'static ScaleProfile) -> SqliteTaskAuthority {
    let path = root.0.join(format!(
        "task-{}.sqlite3",
        profile.profile_id.replace('-', "_")
    ));
    SqliteTaskAuthority::open_with_scale_profile(&path, profile).expect("open task authority")
}

/// The production-shaped Worker-tier consult boundary: the Task
/// authority's consumption path (`answer_plan_materialization`, W31-A)
/// mapped onto the scheduler's typed consult outcome — the same wiring
/// the G3 battery drives inline.
struct TaskConsult<'a>(&'a SqliteTaskAuthority);

impl AdmissionConsult for TaskConsult<'_> {
    type Error = TaskStoreError;

    fn consult_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, TaskStoreError> {
        match self
            .0
            .answer_plan_materialization(other_declared_task_nodes)
        {
            Ok(MaterializationAdmissionFacts {
                profile_id,
                projected_task_nodes,
                projected_active_working_set,
            }) => Ok(AdmissionConsultOutcome::Admitted(
                MaterializationAdmission {
                    profile_id: profile_id.to_string(),
                    projected_task_nodes,
                    projected_active_working_set,
                },
            )),
            Err(TaskStoreError::WorkingSetAdmissionDenied {
                profile_id,
                active_count,
                max_active_working_set,
            }) => Ok(AdmissionConsultOutcome::Denied(
                MaterializationRejection::WorkingSetFull {
                    profile_id: profile_id.to_string(),
                    active_count,
                    max_active_working_set,
                },
            )),
            Err(TaskStoreError::TaskNodeAdmissionDenied {
                profile_id,
                task_count,
                max_task_nodes,
            }) => Ok(AdmissionConsultOutcome::Denied(
                MaterializationRejection::TaskNodeCapExceeded {
                    profile_id: profile_id.to_string(),
                    task_count,
                    max_task_nodes,
                },
            )),
            Err(other) => Err(other),
        }
    }
}

/// A consult that always denies (zero working-set posture) without a
/// Task authority — for selection/no-bypass tests that never approve.
struct DenyAll;

impl AdmissionConsult for DenyAll {
    type Error = Infallible;

    fn consult_materialization(
        &self,
        _other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, Infallible> {
        Ok(AdmissionConsultOutcome::Denied(
            MaterializationRejection::WorkingSetFull {
                profile_id: "task-sched-deny-all".to_string(),
                active_count: 0,
                max_active_working_set: 0,
            },
        ))
    }
}

/// A consult whose first `failures` calls fail (transport posture);
/// afterwards it admits everything.
struct FlakyConsult {
    failures_left: AtomicU64,
}

impl FlakyConsult {
    fn new(failures: u64) -> Self {
        Self {
            failures_left: AtomicU64::new(failures),
        }
    }
}

#[derive(Debug)]
struct ConsultUnavailable;

impl AdmissionConsult for FlakyConsult {
    type Error = ConsultUnavailable;

    fn consult_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, ConsultUnavailable> {
        if self
            .failures_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(ConsultUnavailable);
        }
        Ok(AdmissionConsultOutcome::Admitted(
            MaterializationAdmission {
                profile_id: "task-sched-flaky-wide".to_string(),
                projected_task_nodes: other_declared_task_nodes + 1,
                projected_active_working_set: 1,
            },
        ))
    }
}

/// Global-tier selection is a pure deterministic ready-FIFO scan over
/// durable state, bounded by the window's free seats: dependency-ready
/// nodes in `(first_declared_at_ms, TaskNodeId)` order fill the window;
/// unready/cancelled nodes are skipped with typed reasons; a fresh
/// scheduler instance produces the identical report.
#[test]
fn selection_is_deterministic_ready_fifo_bounded_by_window() {
    let root = Root::new("determinism");
    let authority = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    // Revision 1 (declared at t=1_000): a, b, c independent; d depends
    // on c. Revision 2 (t=2_000) re-declares the total set and adds e,
    // so e's first-declared time is later — FIFO must rank it after the
    // revision-1 nodes even though its node id may sort anywhere.
    let plan_id = authority
        .apply_plan_revision(revision_request(
            None,
            vec![
                node(0x0a, 0x01),
                node(0x0b, 0x02),
                node(0x0c, 0x03),
                node_with_dependency(0x0d, 0x04, 0x0c),
            ],
            0x11,
            1_000,
        ))
        .expect("apply revision 1")
        .receipt()
        .plan_id;
    authority
        .apply_plan_revision(revision_request(
            Some(plan_id),
            vec![
                node(0x0a, 0x01),
                node(0x0b, 0x02),
                node(0x0c, 0x03),
                node_with_dependency(0x0d, 0x04, 0x0c),
                node(0x0e, 0x05),
            ],
            0x12,
            2_000,
        ))
        .expect("apply revision 2");
    // Cancel b: `DECLARED → CANCELLED` is a legal §25.2.1 edge.
    let node_b = node_id_of(&authority, plan_id, [0x0b; 16]);
    authority
        .record_node_transition(nlos_plan::NodeTransitionRequest {
            plan_id,
            node_id: node_b,
            from_state: PlanNodeState::Declared,
            to_state: PlanNodeState::Cancelled,
            expected_declared_revision: 2,
            idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
            transitioned_at_ms: 2_500,
        })
        .expect("cancel node b");
    let node_c = node_id_of(&authority, plan_id, [0x0c; 16]);
    let node_d = node_id_of(&authority, plan_id, [0x0d; 16]);

    // Window 2: exactly two of the three ready nodes (a, c, e) are
    // selected, in ready-FIFO order.
    let scheduler = MaterializationScheduler::new(2);
    let report = scheduler
        .select(&authority, plan_id)
        .expect("global-tier select");
    let expected_order =
        expected_fifo_order(&authority, plan_id, &[[0x0a; 16], [0x0c; 16], [0x0e; 16]]);
    assert_eq!(
        report
            .selections
            .iter()
            .map(|entry| entry.node_id)
            .collect::<Vec<_>>(),
        expected_order[..2],
        "selections fill the window in ready-FIFO order"
    );
    assert!(
        report
            .selections
            .iter()
            .all(|entry| matches!(entry.kind, SelectionKind::NewGateRound))
    );
    assert_eq!(report.window, 2);
    assert_eq!(report.seats_in_use, 0, "no node holds a seat yet");

    let mut skip_by_node = std::collections::HashMap::new();
    for skip in &report.skips {
        skip_by_node.insert(skip.node_id, &skip.reason);
    }
    assert_eq!(skip_by_node.len(), 3, "b, d, and the third ready node skip");
    assert!(matches!(
        skip_by_node.get(&node_b),
        Some(SelectionSkipReason::NotAwaitingMaterialization {
            current: PlanNodeState::Cancelled
        })
    ));
    assert!(matches!(
        skip_by_node.get(&node_d),
        Some(SelectionSkipReason::DependenciesNotReady { unresolved })
            if unresolved == &vec![node_c]
    ));
    let exhausted = expected_order[2];
    assert!(matches!(
        skip_by_node.get(&exhausted),
        Some(SelectionSkipReason::WindowExhausted)
    ));

    // Determinism: a fresh scheduler instance over the same durable
    // state selects byte-identically.
    let replay = MaterializationScheduler::new(2)
        .select(&authority, plan_id)
        .expect("fresh scheduler select");
    assert_eq!(report, replay);

    // Unknown plans fail typed, house style.
    let missing = MaterializationScheduler::new(2)
        .select(&authority, TaskPlanId::from_bytes([0xff; 16]))
        .expect_err("unknown plan must fail typed");
    assert!(matches!(missing, PlanStoreError::PlanNotFound(_)));
}

/// An admission rejection shrinks the window by one seat per rejection
/// (floor 1), observably: the summary reports `window_after`, the
/// decision trail carries the typed rejections, and the next pass
/// selects fewer nodes. Nothing ever materializes.
#[test]
fn window_shrinks_on_admission_rejection_and_is_inspectable() {
    let root = Root::new("shrink");
    let authority = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let task = open_task(&root, &ZERO_WORKING_SET);
    let plan_id = authority
        .apply_plan_revision(revision_request(
            None,
            vec![
                node(0x0a, 0x01),
                node(0x0b, 0x02),
                node(0x0c, 0x03),
                node(0x0d, 0x04),
            ],
            0x11,
            1_000,
        ))
        .expect("apply revision")
        .receipt()
        .plan_id;
    let consult = TaskConsult(&task);

    let mut scheduler = MaterializationScheduler::new(4);
    let summary = scheduler
        .run_pass(&authority, plan_id, &consult, 3_000)
        .expect("first pass");
    assert_eq!(summary.window_before, 4);
    assert_eq!(
        summary.window_after, 1,
        "four rejections shrink 4 to the floor"
    );
    assert_eq!(summary.selected, 4);
    assert_eq!(summary.rejected, 4);
    assert_eq!(summary.approved, 0);
    assert_eq!(scheduler.window(), 1, "window shrink is inspectable");
    assert_eq!(materialized_count(&authority, plan_id), 0);
    let rejected = scheduler
        .decisions()
        .iter()
        .filter(|record| {
            matches!(
                record.decision,
                SchedulerDecision::Rejected {
                    reason: MaterializationRejection::WorkingSetFull { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(rejected, 4, "every rejection is in the decision trail");
    // Every node durably WAITING_RESOURCE with exactly one rejected
    // request row (the W31-A observable window-shrink fact).
    for key in [[0x0a; 16], [0x0b; 16], [0x0c; 16], [0x0d; 16]] {
        let node_id = node_id_of(&authority, plan_id, key);
        let row = authority
            .inspect_node(plan_id, node_id)
            .expect("inspect node")
            .expect("node exists");
        assert_eq!(row.state, PlanNodeState::WaitingResource);
        let history = authority
            .inspect_node_materialization_requests(plan_id, node_id)
            .expect("request history");
        assert_eq!(history.len(), 1);
        assert!(matches!(
            history[0].rejection,
            Some(MaterializationRejection::WorkingSetFull { .. })
        ));
    }

    // Second pass: the shrunken window selects exactly one node.
    let summary = scheduler
        .run_pass(&authority, plan_id, &consult, 3_500)
        .expect("second pass");
    assert_eq!(summary.selected, 1, "fewer selections after the shrink");
    assert_eq!(summary.skipped, 3);
    assert_eq!(summary.window_after, 1, "the floor keeps a durable probe");
    assert_eq!(materialized_count(&authority, plan_id), 0);
    assert_eq!(scheduler.window(), 1);
}

/// The scheduler cannot materialize past the admission consult: under a
/// denying consult no node ever crosses `MATERIALIZING` through the
/// scheduler face, the raw transition face stays storage-gated, and
/// approvals appear only once the consult admits.
#[test]
fn scheduler_cannot_materialize_past_admission_gate() {
    let root = Root::new("no-bypass");
    let authority = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let plan_id = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x11,
            1_000,
        ))
        .expect("apply revision")
        .receipt()
        .plan_id;

    let mut scheduler = MaterializationScheduler::new(8);
    for at_ms in [2_000_u64, 2_500, 3_000] {
        scheduler
            .run_pass(&authority, plan_id, &DenyAll, at_ms)
            .expect("denied pass");
    }
    assert_eq!(
        materialized_count(&authority, plan_id),
        0,
        "no node materializes past a denying consult"
    );
    for key in [[0x0a; 16], [0x0b; 16]] {
        assert_eq!(
            authority
                .inspect_node(plan_id, node_id_of(&authority, plan_id, key))
                .expect("inspect")
                .expect("node")
                .state,
            PlanNodeState::WaitingResource
        );
    }

    // The raw face stays gated: WAITING_RESOURCE → MATERIALIZING without
    // an approved request aborts at the storage layer.
    let raw = authority
        .record_node_transition(nlos_plan::NodeTransitionRequest {
            plan_id,
            node_id: node_id_of(&authority, plan_id, [0x0a; 16]),
            from_state: PlanNodeState::WaitingResource,
            to_state: PlanNodeState::Materializing,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes([0x71; 16]),
            transitioned_at_ms: 3_100,
        })
        .expect_err("raw materializing edge must abort");
    assert!(matches!(raw, PlanStoreError::Sqlite(_)));
    assert_eq!(materialized_count(&authority, plan_id), 0);

    // Once the consult admits (a wide tier), the same scheduler face
    // approves through the gate and only through the gate.
    let task = open_task(&root, &TASK_PROFILE_10K);
    let consult = TaskConsult(&task);
    let summary = scheduler
        .run_pass(&authority, plan_id, &consult, 3_200)
        .expect("admitting pass");
    assert_eq!(summary.approved, 2);
    assert_eq!(summary.rejected, 0);
    assert_eq!(summary.window_after, summary.window_before);
    assert_eq!(materialized_count(&authority, plan_id), 2);
    for key in [[0x0a; 16], [0x0b; 16]] {
        let node_id = node_id_of(&authority, plan_id, key);
        let history = authority
            .inspect_node_materialization_requests(plan_id, node_id)
            .expect("history");
        let approved_rows = history
            .iter()
            .filter(|row| row.approved_voucher_id.is_some())
            .count();
        assert_eq!(approved_rows, 1, "approval is a durable gate round");
    }
}

/// A gate round that crashed mid-flight (request committed, verdict
/// never resolved — the W31-A F2 window) converges: the next pass
/// adopts the durable PENDING round, resolves it under its original
/// exactly-once key, and never opens a second round for the node.
#[test]
fn crashed_gate_round_converges_via_adoption_on_next_pass() {
    let root = Root::new("crash-adopt");
    let authority = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let task = open_task(&root, &TASK_PROFILE_10K);
    let plan_id = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x11,
            1_000,
        ))
        .expect("apply revision")
        .receipt()
        .plan_id;
    let node_a = node_id_of(&authority, plan_id, [0x0a; 16]);
    let node_b = node_id_of(&authority, plan_id, [0x0b; 16]);

    // The crash simulation: a first scheduler instance selected both
    // nodes and opened node a's gate round, then died before the
    // consult. Its in-memory state is gone; the durable PENDING row (an
    // arbitrary caller key here, exactly like any pre-crash worker)
    // survives.
    let crashed_key = IdempotencyKey::from_bytes([0x99; 16]);
    authority
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id: node_a,
            idempotency_key: crashed_key,
            requested_at_ms: 2_000,
        })
        .expect("pre-crash gate round");

    // The restarted scheduler: the Global tier classifies node a as an
    // adoption (not a new round), and the Worker tier resolves it under
    // the original key.
    let mut scheduler = MaterializationScheduler::new(2);
    let report = scheduler
        .select(&authority, plan_id)
        .expect("restart select");
    assert_eq!(report.seats_in_use, 1, "the pending round holds a seat");
    let adoption = report
        .selections
        .iter()
        .find(|entry| entry.node_id == node_a)
        .expect("node a selected");
    assert_eq!(
        adoption.kind,
        SelectionKind::AdoptPendingGateRound {
            request_key: crashed_key
        }
    );

    let summary = scheduler
        .run_pass(&authority, plan_id, &TaskConsult(&task), 2_500)
        .expect("converging pass");
    assert_eq!(summary.approved, 2);
    assert_eq!(materialized_count(&authority, plan_id), 2);
    // Node a's history is exactly the adopted round — no second request.
    let history = authority
        .inspect_node_materialization_requests(plan_id, node_a)
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].idempotency_key, crashed_key);
    assert!(history[0].approved_voucher_id.is_some());
    // Node b went through a fresh scheduler-derived round.
    let history_b = authority
        .inspect_node_materialization_requests(plan_id, node_b)
        .expect("history b");
    assert_eq!(history_b.len(), 1);
    assert_ne!(history_b[0].idempotency_key, crashed_key);
    assert!(scheduler.decisions().iter().any(|record| matches!(
        &record.decision,
        SchedulerDecision::Approved { node_id, .. } if *node_id == node_b
    )));
}

/// A consult that itself fails (transport posture) leaves the gate
/// round PENDING and the window unshrunken; the next pass adopts and
/// resolves it once the consult works again.
#[test]
fn consult_failure_leaves_pending_round_and_next_pass_adopts() {
    let root = Root::new("consult-fail");
    let authority = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let plan_id = authority
        .apply_plan_revision(revision_request(None, vec![node(0x0a, 0x01)], 0x11, 1_000))
        .expect("apply revision")
        .receipt()
        .plan_id;
    let node_id = node_id_of(&authority, plan_id, [0x0a; 16]);

    let flaky = FlakyConsult::new(1);
    let mut scheduler = MaterializationScheduler::new(1);
    let summary = scheduler
        .run_pass(&authority, plan_id, &flaky, 2_000)
        .expect("pass with failing consult");
    assert_eq!(summary.approved, 0);
    assert_eq!(summary.rejected, 0);
    assert_eq!(
        summary.window_after, summary.window_before,
        "no shrink: the consult never answered"
    );
    assert_eq!(materialized_count(&authority, plan_id), 0);
    assert!(
        scheduler
            .decisions()
            .iter()
            .any(|record| matches!(record.decision, SchedulerDecision::ConsultFailed { .. })),
        "the consult failure is inspectable"
    );
    // The gate round stays durable PENDING; the node stays WAITING.
    let row = authority
        .inspect_node(plan_id, node_id)
        .expect("inspect")
        .expect("node");
    assert_eq!(row.state, PlanNodeState::WaitingResource);
    let history = authority
        .inspect_node_materialization_requests(plan_id, node_id)
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].status,
        nlos_plan::MaterializationRequestStatus::Pending
    );

    // The recovered consult adopts and resolves the same round.
    let summary = scheduler
        .run_pass(&authority, plan_id, &flaky, 2_500)
        .expect("converging pass");
    assert_eq!(summary.approved, 1);
    assert_eq!(materialized_count(&authority, plan_id), 1);
    let history = authority
        .inspect_node_materialization_requests(plan_id, node_id)
        .expect("history");
    assert_eq!(history.len(), 1, "adoption, not a second round");
    assert!(history[0].approved_voucher_id.is_some());
}

/// Window seats release on completion and the controller owns the
/// lever: with window 1 the second node stays skipped until the first
/// node completes (seat released) or the controller widens the window.
#[test]
fn controller_lever_and_seat_release_drive_progress() {
    let root = Root::new("lever");
    let authority = SqlitePlanAuthority::open(root.0.join("plan.sqlite3")).expect("open plan");
    let task = open_task(&root, &TASK_PROFILE_10K);
    let plan_id = authority
        .apply_plan_revision(revision_request(
            None,
            vec![node(0x0a, 0x01), node(0x0b, 0x02)],
            0x11,
            1_000,
        ))
        .expect("apply revision")
        .receipt()
        .plan_id;
    let order = expected_fifo_order(&authority, plan_id, &[[0x0a; 16], [0x0b; 16]]);
    let (first, second) = (order[0], order[1]);
    let consult = TaskConsult(&task);

    let mut scheduler = MaterializationScheduler::new(1);
    let summary = scheduler
        .run_pass(&authority, plan_id, &consult, 2_000)
        .expect("first pass");
    assert_eq!(summary.approved, 1);
    assert_eq!(summary.selected, 1);
    assert_eq!(summary.skipped, 1, "the window bound skips the second node");
    assert!(scheduler.decisions().iter().any(|record| matches!(
        &record.decision,
        SchedulerDecision::Skipped {
            node_id,
            reason: SelectionSkipReason::WindowExhausted
        } if *node_id == second
    )));

    // The seat is held: pass 2 still cannot select the second node.
    let summary = scheduler
        .run_pass(&authority, plan_id, &consult, 2_100)
        .expect("second pass");
    assert_eq!(summary.selected, 0);
    assert_eq!(summary.skipped, 2);
    assert_eq!(summary.approved, 0);

    // Completion releases the seat (raw legal edges past the gate) and
    // the third pass materializes the second node.
    for (from, to, key) in [
        (PlanNodeState::Materializing, PlanNodeState::Active, 0x51_u8),
        (PlanNodeState::Active, PlanNodeState::Completed, 0x52),
    ] {
        authority
            .record_node_transition(nlos_plan::NodeTransitionRequest {
                plan_id,
                node_id: first,
                from_state: from,
                to_state: to,
                expected_declared_revision: 1,
                idempotency_key: IdempotencyKey::from_bytes([key; 16]),
                transitioned_at_ms: 2_200,
            })
            .expect("complete the first node");
    }
    let summary = scheduler
        .run_pass(&authority, plan_id, &consult, 2_300)
        .expect("third pass");
    assert_eq!(
        summary.approved, 1,
        "the released seat admits the second node"
    );
    assert_eq!(materialized_count(&authority, plan_id), 1);

    // The controller lever: `set_window` is the only growth path (the
    // scheduler never auto-grows).
    scheduler.set_window(4);
    assert_eq!(scheduler.window(), 4);
}
