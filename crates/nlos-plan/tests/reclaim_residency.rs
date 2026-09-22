//! W36-P8 Task reclaim × plan residency interconnection (W31-G §8.2.5):
//! a driven working-set eviction must record `record_residency_transition`
//! on the bound plan nodes — the two ledgers may not write independently.
//!
//! The production path is [`SqliteTaskAuthority::drive_working_set_reclaim`]:
//! permit closure and the plan-side residency walk happen in that one
//! call. The assembler supplies a [`nlos_task::ReclaimResidencyDrive`]
//! (1:1 task→node binding); it is not an after-the-fact seam.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    ApplyPlanRevisionRequest, NodePinRequest, NodeResidencyTier, PlanNodeDeclaration, PlanNodeKind,
    PlanStoreError, ReclaimResidencyEviction, ResidencyTransitionRequest, SqlitePlanAuthority,
};
use nlos_task::{
    Authorities, PermitDecision, PermitRequest, ReclaimResidencyDrive, ScaleProfile,
    SnapshotBundle, SqliteTaskAuthority, TaskSpec, TaskStoreError, WorkingSetReclaimEviction,
    WorkingSetReclaimExecutionRequest, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId, TaskNodeId, TaskPlanId,
    TaskSnapshotId,
};

/// Cross-authority error so the production drive can `?` Task failures
/// while still surfacing plan-side PINNED / CAS refusals.
#[derive(Debug)]
enum ReclaimDriveError {
    #[allow(dead_code)] // carried for `From<TaskStoreError>` on the drive path
    Task(TaskStoreError),
    Plan(PlanStoreError),
}

impl From<TaskStoreError> for ReclaimDriveError {
    fn from(error: TaskStoreError) -> Self {
        Self::Task(error)
    }
}

/// Assembler binding: each evicted Task maps onto one declared node;
/// the drive calls [`SqlitePlanAuthority::apply_reclaim_residency`]
/// inside `drive_working_set_reclaim`, not after it.
struct BoundPlanResidency<'a> {
    plan: &'a SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_a: TaskNodeId,
    node_b: TaskNodeId,
}

impl BoundPlanResidency<'_> {
    fn node_for(&self, id: TaskId) -> Result<TaskNodeId, ReclaimDriveError> {
        if id == task_id(0) {
            Ok(self.node_a)
        } else if id == task_id(1) {
            Ok(self.node_b)
        } else {
            Err(ReclaimDriveError::Task(TaskStoreError::TaskNotFound))
        }
    }
}

impl ReclaimResidencyDrive for BoundPlanResidency<'_> {
    type Error = ReclaimDriveError;

    fn drive_reclaim_residency(
        &self,
        evictions: &[WorkingSetReclaimEviction],
        executed_at_ms: i64,
    ) -> Result<(), ReclaimDriveError> {
        let mapped = evictions
            .iter()
            .map(|eviction| {
                let mut key = [0xd0; 16];
                key[8..].copy_from_slice(&eviction.task_id.as_bytes()[8..]);
                Ok(ReclaimResidencyEviction {
                    plan_id: self.plan_id,
                    node_id: self.node_for(eviction.task_id)?,
                    expected_declared_revision: 1,
                    idempotency_key: IdempotencyKey::from_bytes(key),
                    transitioned_at_ms: u64::try_from(executed_at_ms.saturating_add(100))
                        .expect("executed_at_ms is non-negative"),
                })
            })
            .collect::<Result<Vec<_>, ReclaimDriveError>>()?;
        self.plan
            .apply_reclaim_residency(&mapped)
            .map_err(ReclaimDriveError::Plan)?;
        Ok(())
    }
}

static RECLAIM_PROFILE: ScaleProfile = ScaleProfile {
    profile_id: "task-reclaim-residency",
    max_task_nodes: 64,
    max_task_registrations: 64,
    max_active_working_set: 2,
    reclaim_threshold_ratio: Some(50),
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
            "nlos-plan-reclaim-residency-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create fixture directory");
        Self(path)
    }

    fn plan(&self) -> SqlitePlanAuthority {
        SqlitePlanAuthority::open(self.0.join("plan.sqlite3")).expect("open plan")
    }

    fn task(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open_with_scale_profile(&self.0.join("task.sqlite3"), &RECLAIM_PROFILE)
            .expect("open task")
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

fn id_bytes(domain: u8, index: u64) -> [u8; 16] {
    let mut bytes = [domain; 16];
    bytes[8..].copy_from_slice(&index.to_be_bytes());
    bytes
}

fn task_id(index: u64) -> TaskId {
    TaskId::from_bytes(id_bytes(0x01, index))
}

fn attempt_id(index: u64) -> TaskAttemptId {
    TaskAttemptId::from_bytes(id_bytes(0x02, index))
}

fn raise_to_hot(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: nlos_types::TaskNodeId,
    key_base: u8,
) {
    let steps = [
        (NodeResidencyTier::MetadataOnly, NodeResidencyTier::Cold),
        (NodeResidencyTier::Cold, NodeResidencyTier::Warm),
        (NodeResidencyTier::Warm, NodeResidencyTier::Hot),
    ];
    for (index, (from, to)) in steps.iter().enumerate() {
        authority
            .record_residency_transition(ResidencyTransitionRequest {
                plan_id,
                node_id,
                from_tier: *from,
                to_tier: *to,
                expected_declared_revision: 1,
                idempotency_key: IdempotencyKey::from_bytes(
                    [key_base + u8::try_from(index).expect("index fits"); 16],
                ),
                transitioned_at_ms: 2_000,
            })
            .expect("raise to HOT");
    }
}

fn issue_permit(task: &SqliteTaskAuthority, index: u64) -> nlos_task::CommitPermitDecision {
    task.register_task(TaskSpec {
        task_id: task_id(index),
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
        application_id: None,
        plan_revision: None,
    })
    .expect("register task");
    task.register_attempt(nlos_task::AttemptSpec {
        task_id: task_id(index),
        attempt_id: attempt_id(index),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(id_bytes(0x10, index)),
            snapshot_digest: [0x20; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(0xc0, index)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xa0, index)),
        registered_at_ms: 2_000,
    })
    .expect("register attempt");
    task.request_commit_permit_decision_with_authorities_struct(
        Authorities::default(),
        PermitRequest {
            task_id: task_id(index),
            attempt_id: attempt_id(index),
            attempt_generation: Generation::INITIAL,
            write_set_root: [0x33; 32],
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xb0, index)),
            valid_until_ms: 99_999,
            requested_at_ms: 3_000 + i64::try_from(index).expect("index fits"),
        },
    )
    .expect("issue permit")
}

/// After Task reclaim evicts a working-set member, the bound plan node
/// walks one evict step (HOT→WARM) and the untouched node stays HOT —
/// the two ledgers converge in the same direction.
#[test]
fn reclaim_drive_records_plan_residency_evict_step() {
    let root = Root::new("converge");
    let plan = root.plan();
    let task = root.task();

    let plan_id = plan
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![node(0x0a, 1), node(0x0b, 2)],
            idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
            applied_at_ms: 1_000,
        })
        .expect("declare two nodes")
        .receipt()
        .plan_id;
    let nodes = plan.list_plan_nodes(plan_id).expect("list");
    let node_a = nodes
        .iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("node a")
        .node_id;
    let node_b = nodes
        .iter()
        .find(|record| record.node_key == [0x0b; 16])
        .expect("node b")
        .node_id;
    raise_to_hot(&plan, plan_id, node_a, 0x20);
    raise_to_hot(&plan, plan_id, node_b, 0x30);

    let first = issue_permit(&task, 0);
    assert!(matches!(first.permit, PermitDecision::Issued(_)));
    let second = issue_permit(&task, 1);
    let warrant = second
        .reclaim_execution
        .expect("second issuance crosses the soft threshold");

    let binding = BoundPlanResidency {
        plan: &plan,
        plan_id,
        node_a,
        node_b,
    };
    let report = task
        .drive_working_set_reclaim(
            WorkingSetReclaimExecutionRequest {
                execution: warrant,
                executed_at_ms: 9_000,
            },
            &binding,
        )
        .expect("drive reclaim records residency");
    assert!(report.post_active_count < report.pre_active_count);
    let victims = SqliteTaskAuthority::reclaim_residency_victims(&report);
    assert_eq!(victims.len(), 1, "soft-threshold overshoot is one unit");
    let evicted_node = binding
        .node_for(victims[0].0)
        .expect("victim is one of the two bound tasks");
    let stayed = if evicted_node == node_a {
        node_b
    } else {
        node_a
    };

    assert_eq!(
        plan.inspect_node_residency(plan_id, evicted_node)
            .expect("inspect evicted")
            .expect("node")
            .tier,
        NodeResidencyTier::Warm
    );
    assert_eq!(
        plan.inspect_node_residency(plan_id, stayed)
            .expect("inspect stayed")
            .expect("node")
            .tier,
        NodeResidencyTier::Hot
    );
}

/// A PINNED victim refuses on the production drive **before** Task
/// permit close: plan stays HOT and working-set occupancy does not
/// shrink (the two ledgers stay in the same direction).
#[test]
fn reclaim_residency_refuses_pinned_victim() {
    let root = Root::new("pinned-refuse");
    let plan = root.plan();
    let task = root.task();

    let plan_id = plan
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![node(0x0a, 1), node(0x0b, 2)],
            idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
            applied_at_ms: 1_000,
        })
        .expect("declare")
        .receipt()
        .plan_id;
    let nodes = plan.list_plan_nodes(plan_id).expect("list");
    let node_a = nodes
        .iter()
        .find(|record| record.node_key == [0x0a; 16])
        .expect("a")
        .node_id;
    let node_b = nodes
        .iter()
        .find(|record| record.node_key == [0x0b; 16])
        .expect("b")
        .node_id;
    raise_to_hot(&plan, plan_id, node_a, 0x20);
    raise_to_hot(&plan, plan_id, node_b, 0x30);
    for node_id in [node_a, node_b] {
        plan.record_node_pin(NodePinRequest {
            plan_id,
            node_id,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes({
                let mut key = [0xe0; 16];
                key[15] = node_id.as_bytes()[15];
                key
            }),
            transitioned_at_ms: 4_000,
        })
        .expect("pin both so whichever victim is PINNED");
    }

    let _ = issue_permit(&task, 0);
    let second = issue_permit(&task, 1);
    let warrant = second.reclaim_execution.expect("warrant");
    let before = task
        .inspect_working_set_pressure()
        .expect("pressure before PINNED refuse");
    assert_eq!(before.active_count, 2, "both permits are still issued");
    let binding = BoundPlanResidency {
        plan: &plan,
        plan_id,
        node_a,
        node_b,
    };
    let denied = task
        .drive_working_set_reclaim(
            WorkingSetReclaimExecutionRequest {
                execution: warrant,
                executed_at_ms: 9_000,
            },
            &binding,
        )
        .expect_err("PINNED victim must refuse on the production drive");
    assert!(matches!(
        denied,
        ReclaimDriveError::Plan(PlanStoreError::PinnedNodeNotEvictable { .. })
    ));
    for node_id in [node_a, node_b] {
        assert_eq!(
            plan.inspect_node_residency(plan_id, node_id)
                .expect("inspect")
                .expect("node")
                .tier,
            NodeResidencyTier::Hot
        );
    }
    let after = task
        .inspect_working_set_pressure()
        .expect("pressure after PINNED refuse");
    assert_eq!(
        after.active_count, before.active_count,
        "PINNED refuse must not shrink Task while plan stays HOT"
    );
}
