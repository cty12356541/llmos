//! Stage D (`W72-D-TASK-DAG` / `D-TASK-SPACE-DAG`): `TaskSpace` `L1 -> L2 -> L3` Progressive
//! Disclosure, Dagster-Style Orthogonal `TaskPlan` DAG Override Editor, and Temporal `TaskAttempt`
//! Racing Swimlanes & `ExecutionFiber` Phase Waterfall Backend Authority.
//!
//! Authority:
//! - `docs/design/06-架构设计总纲-v0.5.md` §19, §28.4 (`[ROAD-D-001]`, `[TASK-SPACE-001]`,
//!   `[TASK-VIS-001]`, `[TASK-REOPEN-001]`, `[RCPT-UNDO-001]`, `[RCPT-PARTIAL-001]`,
//!   `[PLAN-CHANGE-001]`, `[PLAN-OVERRIDE-001]`, `[CTRL-PLAN-001]`, `[PLAN-LAZY-001]`,
//!   `[TASK-SPEC-001]`, `[TASK-PROVIDER-001]`, `[TASK-AGENT-001]`, `[TASK-ISO-005]`).
//! - `docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md` §2A, §3.2.
//! - `docs/management/stage-d-progress.md` §D.3.1 #6, §D.3.2 (`TASK_SPACE_GAPS` 1–3 closure), §D.5.2.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Maximum cursor page width (`limit <= 64`) matching `MAX_SYSTEM_CONTROL_ENUM_LIMIT`.
pub const MAX_PLAN_EDITOR_ENUM_LIMIT: usize = 64;

/// Number of historical Stage B/C `TASK_SPACE_GAPS` closed by `IPC-D1..D3` + `W72-D-TASK-DAG`.
pub const CLOSED_TASK_SPACE_GAPS_COUNT: usize = 3;

/// Number of remaining open `TASK_SPACE_GAPS` in Stage D (`0`).
pub const OPEN_TASK_SPACE_GAPS_COUNT: usize = 0;

/// Statutory Lock #3 code for `COMPLETED` / `ACTIVE` / `COMMITTED` `TaskNode` immutability.
pub const STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE: &str = "NODE_COMPLETED_OR_ACTIVE_LOCKED";

/// Statutory Lock #4 code forbidding manual `TaskAttempt` winner/commit overrides.
pub const STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT: &str = "TASK_ATTEMPT_NO_MANUAL_COMMIT";

/// Statutory Lock #5 code forbidding silent skip/manual success on `EFFECT_UNKNOWN` operations.
pub const STATUTORY_LOCK_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS: &str =
    "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS";

/// Statutory Lock #6 code forbidding `Undo` on `IRREVERSIBLE` external actions.
pub const STATUTORY_LOCK_IRREVERSIBLE_NO_UNDO: &str = "IRREVERSIBLE_NO_UNDO";

/// Clamps any requested enumeration page width to `1..=64`.
#[must_use]
pub fn clamp_plan_editor_limit(requested: Option<usize>) -> usize {
    match requested {
        Some(0) | None => MAX_PLAN_EDITOR_ENUM_LIMIT,
        Some(n) => n.min(MAX_PLAN_EDITOR_ENUM_LIMIT),
    }
}

/// Canonical 16 `TaskState` values + `TaskNode` execution lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskNodeLifecycleState {
    Declared,
    Pending,
    BlockedDependency,
    WaitingConsent,
    WaitingInput,
    WaitingResource,
    WaitingNetwork,
    Eligible,
    Ready,
    Active,
    Running,
    Suspended,
    Recovering,
    Cancelling,
    Completing,
    Completed,
    Committed,
    Partial,
    Disputed,
    Failed,
    Cancelled,
    Superseded,
}

impl TaskNodeLifecycleState {
    /// Statutory Lock #3 (`NODE_COMPLETED_OR_ACTIVE_LOCKED`):
    /// `Completed`, `Committed`, `Active`, `Running`, `Completing`, and `Cancelling` nodes are
    /// strictly read-only in the DAG override editor.
    #[must_use]
    pub const fn is_editable_for_override(self) -> bool {
        matches!(
            self,
            Self::Declared
                | Self::Pending
                | Self::BlockedDependency
                | Self::WaitingConsent
                | Self::WaitingInput
                | Self::WaitingResource
                | Self::WaitingNetwork
                | Self::Eligible
                | Self::Ready
                | Self::Suspended
        )
    }

    /// Whether a `Task` in this state (or with partial target failures) can be reopened via
    /// `[TASK-REOPEN-001]` (`RetryNewAttempt` or `RefineNewGeneration`).
    #[must_use]
    pub const fn is_terminal_or_reopenable(self) -> bool {
        matches!(
            self,
            Self::Failed
                | Self::Cancelled
                | Self::Disputed
                | Self::Partial
                | Self::Completed
                | Self::Committed
        )
    }
}

/// `[PLAN-LAZY-001]` residency tier for `TaskNode` and collapsed supernodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanResidencyTier {
    HotMaterialized,
    MetadataOnly,
    Cold,
}

/// `TaskGroup` execution topology (`Sequential`, `Dag`, `Streaming`, `Service`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskGroupTopologyDto {
    Sequential,
    Dag,
    Streaming,
    Service,
}

/// `[RCPT-UNDO-001]` reversibility classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskUndoClassKind {
    Reversible,
    Compensatable,
    Irreversible,
}

/// `TaskGroup` projection in the `L3` `TaskPlan` DAG canvas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGroupPlanDto {
    pub group_id: String,
    pub title: String,
    pub topology: TaskGroupTopologyDto,
    pub scheduling_order: u32,
    pub failure_policy: String,
    pub cancellation_scope: String,
    pub runtime_expanded: bool,
    pub attributed_budget_units: u64,
}

/// `TaskNode` projection in the `L3` `TaskPlan` DAG canvas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskNodePlanDto {
    pub node_id: String,
    pub group_id: String,
    pub title: String,
    pub state: TaskNodeLifecycleState,
    pub dependency_node_ids: Vec<String>,
    pub capability_id: String,
    pub requested_budget_units: u64,
    pub effective_budget_units: u64,
    pub usage_budget_units: u64,
    pub residency_tier: PlanResidencyTier,
    pub runtime_expanded: bool,
    pub cross_application_target: Option<String>,
    pub editable_for_override: bool,
    pub statutory_lock: Option<String>,
}

/// `[PLAN-LAZY-001]` unmaterialized subgraph super-node (`+N 待物化步骤 · 0 进程`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LazyPlanSupernodeDto {
    pub supernode_id: String,
    pub group_id: String,
    pub title_zh: String,
    pub title_en: String,
    pub badge_zh: String,
    pub badge_en: String,
    pub dependency_node_ids: Vec<String>,
    pub unmaterialized_step_count: usize,
    pub allocated_process_count: usize,
    pub residency_tier: PlanResidencyTier,
    pub expanded: bool,
    pub materialized_nodes: Vec<TaskNodePlanDto>,
    pub next_cursor_hex: Option<String>,
}

/// Complete `TaskPlan` DAG snapshot returned by `inspect_task_plan_dag`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPlanDagDto {
    pub task_id: String,
    pub plan_id: String,
    pub application_id: String,
    pub installation_generation: u64,
    pub plan_revision: u64,
    pub plan_digest_hex: String,
    pub task_state: TaskNodeLifecycleState,
    pub undo_class: TaskUndoClassKind,
    pub authorized_capabilities: Vec<String>,
    pub authorized_budget_ceiling_units: u64,
    pub succeeded_targets: u32,
    pub failed_targets: u32,
    pub groups: Vec<TaskGroupPlanDto>,
    pub nodes: Vec<TaskNodePlanDto>,
    pub lazy_supernodes: Vec<LazyPlanSupernodeDto>,
    pub receipt_hex: String,
}

/// Result of `expand_lazy_plan_supernode` (`[PLAN-LAZY-001]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupernodeExpansionResultDto {
    pub task_id: String,
    pub plan_id: String,
    pub supernode_id: String,
    pub effective_limit: usize,
    pub allocated_process_count_before: usize,
    pub materialized_nodes: Vec<TaskNodePlanDto>,
    pub remaining_unmaterialized_count: usize,
    pub next_cursor_hex: Option<String>,
    pub receipt_hex: String,
}

/// Interactive `TaskPlan` DAG override operations (`[PLAN-CHANGE-001]`, `[PLAN-OVERRIDE-001]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanOverrideOpDto {
    AddNode {
        node_id: String,
        group_id: String,
        title: String,
        dependency_node_ids: Vec<String>,
        capability_id: String,
        requested_budget_units: u64,
        cross_application_target: Option<String>,
    },
    RemoveNode {
        target_node_id: String,
        #[serde(default = "default_true")]
        alters_acceptance_or_side_effects: bool,
    },
    PauseNode {
        target_node_id: String,
        reason: Option<String>,
    },
    MergeNodes {
        primary_node_id: String,
        secondary_node_id: String,
        merged_title: String,
        #[serde(default = "default_true")]
        cross_scope_or_capability: bool,
    },
    ReorderNodes {
        node_id: String,
        new_dependency_node_ids: Vec<String>,
        #[serde(default = "default_true")]
        within_authorized_envelope: bool,
    },
    ReplaceNode {
        target_node_id: String,
        replacement_node_id: String,
        replacement_title: String,
        capability_id: String,
        requested_budget_units: u64,
        cross_application_target: Option<String>,
    },
}

const fn default_true() -> bool {
    true
}

/// 2-Path classification for `TaskPlan` overrides (`[CTRL-PLAN-001]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanOverridePathKind {
    /// Path A: Bounded parameter/schedule/pause/reorder within authorized envelope -> `ControlCommand`.
    BoundedControlCommand,
    /// Path B: Structural step addition/replacement/removal/merge or budget/capability expansion ->
    /// recompile `AdmissionPlan` (`new plan_digest`) and require `Z4` re-authorization.
    RecompileAndZ4Reauthorize,
}

/// Terraform-style `+ / ~ / -` blast-radius diff row for `TaskPlan` override preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanBlastRadiusRowDto {
    pub symbol: String,
    pub category: String,
    pub scope_label: String,
    pub target_label: String,
    pub reason: String,
}

/// Typed `ControlCommand` descriptor emitted by Path A (`BoundedControlCommand`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompiledPlanControlCommandDto {
    pub command_id_hex: String,
    pub control_scope: String,
    pub target_id: String,
    pub action: String,
    pub expected_plan_revision: u64,
    pub idempotency_key_hex: String,
}

/// `Z4` policy re-authorization grant token required before admitting Path B overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Z4ReauthorizationGrantDto {
    pub grant_token_hex: String,
    pub plan_id: String,
    pub expected_plan_revision: u64,
    pub approved_plan_digest_hex: String,
    pub authorizer_policy_epoch: u64,
}

/// Preview outcome returned by `preview_plan_override_diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanOverridePreviewDto {
    pub task_id: String,
    pub plan_id: String,
    pub path_kind: PlanOverridePathKind,
    pub requires_z4_reauthorization: bool,
    pub base_plan_revision: u64,
    pub candidate_plan_revision: u64,
    pub base_plan_digest_hex: String,
    pub candidate_plan_digest_hex: String,
    pub compiled_control_commands: Vec<CompiledPlanControlCommandDto>,
    pub diff_rows: Vec<PlanBlastRadiusRowDto>,
    pub added_count: usize,
    pub modified_count: usize,
    pub removed_count: usize,
}

/// Commit receipt returned by `submit_plan_override`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanOverrideCommitReceiptDto {
    pub task_id: String,
    pub plan_id: String,
    pub path_kind: PlanOverridePathKind,
    pub new_plan_revision: u64,
    pub new_plan_digest_hex: String,
    pub compiled_control_commands: Vec<CompiledPlanControlCommandDto>,
    pub z4_grant_token_hex: Option<String>,
    pub receipt_hex: String,
    pub updated_dag: TaskPlanDagDto,
}

/// `ExecutionFiber` 5-segment phase duration waterfall (`[TASK-AGENT-001]`, `[TASK-ISO-005]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionFiberWaterfallDto {
    pub fiber_id: String,
    pub attempt_id: String,
    pub process_id: String,
    pub label: String,
    pub shared_thread_like: bool,
    pub fault_domain_badge: Option<String>,
    pub active_cpu_ms: u64,
    pub scheduler_wait_ms: u64,
    pub external_wait_ms: u64,
    pub backpressure_wait_ms: u64,
    pub suspended_ms: u64,
}

/// `DurableOperation` forensic item under a `TaskAttempt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableOperationForensicDto {
    pub operation_id: String,
    pub attempt_id: String,
    pub target_resource_uri: String,
    pub effect_state: String,
    pub statutory_lock: Option<String>,
    pub fencing_token_epoch: u64,
    pub receipt_hex: String,
}

/// `Cell` placement and fencing forensic metadata (`L3`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellPlacementForensicDto {
    pub cell_id_hex: String,
    pub lease_epoch: u64,
    pub node_boot_generation: u64,
    pub placement_mode: String,
    pub fencing_token_hex: String,
}

/// Single `TaskAttempt` racing swimlane (`[TASK-SPEC-001]`, `[TASK-PROVIDER-001]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAttemptSwimlaneDto {
    pub attempt_id: String,
    pub task_node_id: String,
    pub attempt_index: u32,
    pub state: TaskNodeLifecycleState,
    pub input_snapshot_digest_hex: String,
    pub isolation_domain_id_hex: String,
    pub isolation_enforcement_profile: String,
    pub provider_isolation_unverified: bool,
    pub started_at_offset_ms: u64,
    pub duration_ms: u64,
    pub cas_winner: bool,
    pub superseded: bool,
    pub retry_fence_epoch: u64,
    pub allowed_control_verbs: Vec<String>,
    pub statutory_lock: String,
    pub fibers: Vec<ExecutionFiberWaterfallDto>,
    pub operations: Vec<DurableOperationForensicDto>,
    pub cell_placement: CellPlacementForensicDto,
    pub receipt_hex: String,
}

/// Complete `TaskAttempt` racing swimlanes snapshot returned by `inspect_task_attempt_lanes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAttemptSwimlanesInspectDto {
    pub task_id: String,
    pub task_node_id: String,
    pub plan_revision: u64,
    pub task_head_cas_epoch: u64,
    pub winning_attempt_id: Option<String>,
    pub attempts: Vec<TaskAttemptSwimlaneDto>,
    pub receipt_hex: String,
}

/// `[TASK-REOPEN-001]` terminal task reopen mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReopenTaskMode {
    /// Spawns a new `TaskAttempt` under the existing `Task` while keeping prior attempts immutable.
    RetryNewAttempt,
    /// Spawns a new `Task` generation (`installation_generation + 1`, `plan_revision + 1`) while
    /// preserving the prior task's historical receipt chain.
    RefineNewGeneration,
}

/// Result of `reopen_terminal_task` (`[TASK-REOPEN-001]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReopenTerminalTaskResultDto {
    pub mode: ReopenTaskMode,
    pub source_task_id: String,
    pub resulting_task_id: String,
    pub spawned_attempt_id: Option<String>,
    pub new_installation_generation: u64,
    pub new_plan_revision: u64,
    pub retry_fence_epoch: u64,
    pub preserved_history_task_ids: Vec<String>,
    pub receipt_hex: String,
}

/// Typed errors returned by `plan_editor` operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "errorCode", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanEditorError {
    NotFound {
        entity_id: String,
        message: String,
    },
    StaleGeneration {
        entity_id: String,
        expected_generation: u64,
        current_generation: u64,
        message: String,
    },
    ReadOnlyExecutedTaskNode {
        node_id: String,
        node_state: TaskNodeLifecycleState,
        statutory_lock: String,
        message: String,
    },
    Z4ReauthorizationRequired {
        plan_id: String,
        expected_plan_revision: u64,
        candidate_plan_digest_hex: String,
        message: String,
    },
    ForbiddenManualAttemptCommit {
        attempt_id: String,
        rejected_verb: String,
        statutory_lock: String,
        message: String,
    },
    TaskNotTerminalOrPartial {
        task_id: String,
        current_state: TaskNodeLifecycleState,
        message: String,
    },
    EmptyOverrideBatch {
        plan_id: String,
        message: String,
    },
}

#[must_use]
pub fn deterministic_hex_digest(seed: &str) -> String {
    let mut h1: u32 = 0x811c_9dc5;
    let mut h2: u32 = 0x0100_0193;
    for (idx, b) in seed.bytes().enumerate() {
        h1 ^= u32::from(b);
        h1 = h1.wrapping_mul(0x0100_0193);
        h2 ^= u32::from(b).wrapping_add(idx as u32);
        h2 = h2.wrapping_mul(0x85eb_ca6b);
    }
    let p1 = format!("{h1:08x}");
    let p2 = format!("{h2:08x}");
    let p3 = format!("{:08x}", h1 ^ h2);
    let p4 = format!("{:08x}", h1.wrapping_mul(31));
    format!("{p1}{p2}{p3}{p4}{p1}{p2}{p3}{p4}")
}

#[must_use]
pub fn format_lazy_supernode_badge(count: usize, locale: &str) -> String {
    if locale.eq_ignore_ascii_case("en-US") {
        format!("+{count} Unmaterialized Steps · 0 Processes")
    } else {
        format!("+{count} 待物化步骤 · 0 进程")
    }
}

#[derive(Debug, Clone)]
struct StoredTaskEntry {
    dag: TaskPlanDagDto,
    swimlanes: TaskAttemptSwimlanesInspectDto,
}

#[derive(Debug, Clone)]
struct PlanEditorInner {
    tasks: BTreeMap<String, StoredTaskEntry>,
}

impl Default for PlanEditorInner {
    fn default() -> Self {
        let mut tasks = BTreeMap::new();
        let seed_states = [
            (
                "task-01",
                "plan-01",
                TaskNodeLifecycleState::Running,
                TaskUndoClassKind::Irreversible,
                0,
            ),
            (
                "task-02",
                "plan-02",
                TaskNodeLifecycleState::Partial,
                TaskUndoClassKind::Compensatable,
                1,
            ),
            (
                "task-03",
                "plan-03",
                TaskNodeLifecycleState::Failed,
                TaskUndoClassKind::Reversible,
                1,
            ),
            (
                "task-04",
                "plan-04",
                TaskNodeLifecycleState::Cancelled,
                TaskUndoClassKind::Reversible,
                0,
            ),
            (
                "task-05",
                "plan-05",
                TaskNodeLifecycleState::Disputed,
                TaskUndoClassKind::Compensatable,
                1,
            ),
            (
                "task-06",
                "plan-06",
                TaskNodeLifecycleState::Completed,
                TaskUndoClassKind::Irreversible,
                0,
            ),
        ];

        for (idx, (task_id, plan_id, task_state, undo_class, failed_targets)) in
            seed_states.into_iter().enumerate()
        {
            let num = idx + 1;
            let plan_digest_hex = deterministic_hex_digest(&format!("plan_digest:{plan_id}:r3"));
            let receipt_hex = deterministic_hex_digest(&format!("rcpt:dag:{task_id}:r3"));

            let groups = vec![
                TaskGroupPlanDto {
                    group_id: format!("{plan_id}-grp-ingest"),
                    title: "数据采集与校验组".to_owned(),
                    topology: TaskGroupTopologyDto::Dag,
                    scheduling_order: 1,
                    failure_policy: "CONTINUE_ON_PARTIAL".to_owned(),
                    cancellation_scope: "GROUP_BOUNDED".to_owned(),
                    runtime_expanded: true,
                    attributed_budget_units: 60,
                },
                TaskGroupPlanDto {
                    group_id: format!("{plan_id}-grp-commit"),
                    title: "归档与提交组".to_owned(),
                    topology: TaskGroupTopologyDto::Sequential,
                    scheduling_order: 2,
                    failure_policy: "FAIL_FAST".to_owned(),
                    cancellation_scope: "PLAN_WIDE".to_owned(),
                    runtime_expanded: false,
                    attributed_budget_units: 40,
                },
            ];

            let nodes = vec![
                TaskNodePlanDto {
                    node_id: format!("{plan_id}-node-completed"),
                    group_id: format!("{plan_id}-grp-ingest"),
                    title: "读取工作区基线清单".to_owned(),
                    state: TaskNodeLifecycleState::Completed,
                    dependency_node_ids: vec![],
                    capability_id: "cap.fs.workspace.read".to_owned(),
                    requested_budget_units: 20,
                    effective_budget_units: 20,
                    usage_budget_units: 18,
                    residency_tier: PlanResidencyTier::HotMaterialized,
                    runtime_expanded: false,
                    cross_application_target: None,
                    editable_for_override: false,
                    statutory_lock: Some(STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE.to_owned()),
                },
                TaskNodePlanDto {
                    node_id: format!("{plan_id}-node-active"),
                    group_id: format!("{plan_id}-grp-ingest"),
                    title: "执行差异归因分析".to_owned(),
                    state: TaskNodeLifecycleState::Active,
                    dependency_node_ids: vec![format!("{plan_id}-node-completed")],
                    capability_id: "cap.fs.workspace.read".to_owned(),
                    requested_budget_units: 30,
                    effective_budget_units: 25,
                    usage_budget_units: 22,
                    residency_tier: PlanResidencyTier::HotMaterialized,
                    runtime_expanded: true,
                    cross_application_target: None,
                    editable_for_override: false,
                    statutory_lock: Some(STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE.to_owned()),
                },
                TaskNodePlanDto {
                    node_id: format!("{plan_id}-node-pending-a"),
                    group_id: format!("{plan_id}-grp-commit"),
                    title: "生成结构化阶段报告".to_owned(),
                    state: TaskNodeLifecycleState::Declared,
                    dependency_node_ids: vec![format!("{plan_id}-node-active")],
                    capability_id: "cap.fs.workspace.write".to_owned(),
                    requested_budget_units: 25,
                    effective_budget_units: 20,
                    usage_budget_units: 0,
                    residency_tier: PlanResidencyTier::HotMaterialized,
                    runtime_expanded: false,
                    cross_application_target: None,
                    editable_for_override: true,
                    statutory_lock: None,
                },
                TaskNodePlanDto {
                    node_id: format!("{plan_id}-node-pending-b"),
                    group_id: format!("{plan_id}-grp-commit"),
                    title: "写入归档回执摘要".to_owned(),
                    state: TaskNodeLifecycleState::Eligible,
                    dependency_node_ids: vec![format!("{plan_id}-node-pending-a")],
                    capability_id: "cap.fs.workspace.write".to_owned(),
                    requested_budget_units: 20,
                    effective_budget_units: 15,
                    usage_budget_units: 0,
                    residency_tier: PlanResidencyTier::HotMaterialized,
                    runtime_expanded: false,
                    cross_application_target: None,
                    editable_for_override: true,
                    statutory_lock: None,
                },
            ];

            let lazy_supernodes = vec![LazyPlanSupernodeDto {
                supernode_id: format!("{plan_id}-supernode-cold"),
                group_id: format!("{plan_id}-grp-commit"),
                title_zh: "后续批量归档子图".to_owned(),
                title_en: "Batch Archive Tail Subgraph".to_owned(),
                badge_zh: format_lazy_supernode_badge(18, "zh-CN"),
                badge_en: format_lazy_supernode_badge(18, "en-US"),
                dependency_node_ids: vec![format!("{plan_id}-node-pending-b")],
                unmaterialized_step_count: 18,
                allocated_process_count: 0,
                residency_tier: PlanResidencyTier::MetadataOnly,
                expanded: false,
                materialized_nodes: vec![],
                next_cursor_hex: None,
            }];

            let dag = TaskPlanDagDto {
                task_id: task_id.to_owned(),
                plan_id: plan_id.to_owned(),
                application_id: format!("app.workstation.suite.{num:02}"),
                installation_generation: 2,
                plan_revision: 3,
                plan_digest_hex,
                task_state,
                undo_class,
                authorized_capabilities: vec![
                    "cap.fs.workspace.read".to_owned(),
                    "cap.fs.workspace.write".to_owned(),
                ],
                authorized_budget_ceiling_units: 120,
                succeeded_targets: if failed_targets > 0 { 4 } else { 5 },
                failed_targets,
                groups,
                nodes,
                lazy_supernodes,
                receipt_hex: receipt_hex.clone(),
            };

            let allowed_verbs = vec![
                "cancel_attempt".to_owned(),
                "supersede_attempt".to_owned(),
                "request_reverify".to_owned(),
            ];

            let swimlanes = TaskAttemptSwimlanesInspectDto {
                task_id: task_id.to_owned(),
                task_node_id: format!("{plan_id}-node-active"),
                plan_revision: 3,
                task_head_cas_epoch: 7,
                winning_attempt_id: Some(format!("{task_id}-attempt-2")),
                attempts: vec![
                    TaskAttemptSwimlaneDto {
                        attempt_id: format!("{task_id}-attempt-1"),
                        task_node_id: format!("{plan_id}-node-active"),
                        attempt_index: 1,
                        state: TaskNodeLifecycleState::Superseded,
                        input_snapshot_digest_hex: deterministic_hex_digest(&format!(
                            "snap:{task_id}:a1"
                        )),
                        isolation_domain_id_hex: deterministic_hex_digest(&format!(
                            "iso:{task_id}:a1"
                        ))[..32]
                            .to_owned(),
                        isolation_enforcement_profile: "PROVIDER_EXTERNAL".to_owned(),
                        provider_isolation_unverified: true,
                        started_at_offset_ms: 0,
                        duration_ms: 180,
                        cas_winner: false,
                        superseded: true,
                        retry_fence_epoch: 1,
                        allowed_control_verbs: allowed_verbs.clone(),
                        statutory_lock: STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT.to_owned(),
                        fibers: vec![ExecutionFiberWaterfallDto {
                            fiber_id: format!("{task_id}-a1-fiber-1"),
                            attempt_id: format!("{task_id}-attempt-1"),
                            process_id: format!("proc-shared-{num}"),
                            label: "External Provider Adapter Worker".to_owned(),
                            shared_thread_like: true,
                            fault_domain_badge: Some("共享进程域 · SHARED_THREAD_LIKE".to_owned()),
                            active_cpu_ms: 40,
                            scheduler_wait_ms: 25,
                            external_wait_ms: 95,
                            backpressure_wait_ms: 20,
                            suspended_ms: 10,
                        }],
                        operations: vec![DurableOperationForensicDto {
                            operation_id: format!("{task_id}-op-1"),
                            attempt_id: format!("{task_id}-attempt-1"),
                            target_resource_uri: "workspace://staging/report-draft.json".to_owned(),
                            effect_state: "EFFECT_UNKNOWN".to_owned(),
                            statutory_lock: Some(
                                STATUTORY_LOCK_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS.to_owned(),
                            ),
                            fencing_token_epoch: 1,
                            receipt_hex: deterministic_hex_digest(&format!("rcpt:{task_id}:op1")),
                        }],
                        cell_placement: CellPlacementForensicDto {
                            cell_id_hex: format!("cell-local-{num:02}"),
                            lease_epoch: 4,
                            node_boot_generation: 2,
                            placement_mode: "LOCAL_SINGLE_NODE".to_owned(),
                            fencing_token_hex: deterministic_hex_digest(&format!(
                                "fence:{task_id}:a1"
                            ))[..32]
                                .to_owned(),
                        },
                        receipt_hex: deterministic_hex_digest(&format!("rcpt:{task_id}:a1")),
                    },
                    TaskAttemptSwimlaneDto {
                        attempt_id: format!("{task_id}-attempt-2"),
                        task_node_id: format!("{plan_id}-node-active"),
                        attempt_index: 2,
                        state: TaskNodeLifecycleState::Committed,
                        input_snapshot_digest_hex: deterministic_hex_digest(&format!(
                            "snap:{task_id}:a2"
                        )),
                        isolation_domain_id_hex: deterministic_hex_digest(&format!(
                            "iso:{task_id}:a2"
                        ))[..32]
                            .to_owned(),
                        isolation_enforcement_profile: "STRICT_SANDBOX".to_owned(),
                        provider_isolation_unverified: false,
                        started_at_offset_ms: 20,
                        duration_ms: 110,
                        cas_winner: true,
                        superseded: false,
                        retry_fence_epoch: 2,
                        allowed_control_verbs: allowed_verbs,
                        statutory_lock: STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT.to_owned(),
                        fibers: vec![ExecutionFiberWaterfallDto {
                            fiber_id: format!("{task_id}-a2-fiber-1"),
                            attempt_id: format!("{task_id}-attempt-2"),
                            process_id: format!("proc-sandbox-{num}"),
                            label: "Sandbox Verification Worker".to_owned(),
                            shared_thread_like: false,
                            fault_domain_badge: None,
                            active_cpu_ms: 65,
                            scheduler_wait_ms: 15,
                            external_wait_ms: 20,
                            backpressure_wait_ms: 10,
                            suspended_ms: 0,
                        }],
                        operations: vec![DurableOperationForensicDto {
                            operation_id: format!("{task_id}-op-2"),
                            attempt_id: format!("{task_id}-attempt-2"),
                            target_resource_uri: "workspace://canonical/report-final.json"
                                .to_owned(),
                            effect_state: "COMMITTED".to_owned(),
                            statutory_lock: None,
                            fencing_token_epoch: 2,
                            receipt_hex: deterministic_hex_digest(&format!("rcpt:{task_id}:op2")),
                        }],
                        cell_placement: CellPlacementForensicDto {
                            cell_id_hex: format!("cell-local-{num:02}"),
                            lease_epoch: 5,
                            node_boot_generation: 2,
                            placement_mode: "LOCAL_SINGLE_NODE".to_owned(),
                            fencing_token_hex: deterministic_hex_digest(&format!(
                                "fence:{task_id}:a2"
                            ))[..32]
                                .to_owned(),
                        },
                        receipt_hex: deterministic_hex_digest(&format!("rcpt:{task_id}:a2")),
                    },
                ],
                receipt_hex,
            };

            tasks.insert(task_id.to_owned(), StoredTaskEntry { dag, swimlanes });
        }

        Self { tasks }
    }
}

/// Thread-safe `PlanEditorState` authority for `W72-D-TASK-DAG`.
#[derive(Debug, Clone, Default)]
pub struct PlanEditorState {
    inner: Arc<Mutex<PlanEditorInner>>,
}

impl PlanEditorState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

fn find_node<'a>(dag: &'a TaskPlanDagDto, node_id: &str) -> Option<&'a TaskNodePlanDto> {
    if let Some(n) = dag.nodes.iter().find(|n| n.node_id == node_id) {
        return Some(n);
    }
    for supernode in &dag.lazy_supernodes {
        if let Some(n) = supernode
            .materialized_nodes
            .iter()
            .find(|n| n.node_id == node_id)
        {
            return Some(n);
        }
    }
    None
}

fn referenced_existing_node_ids(op: &PlanOverrideOpDto) -> Vec<&str> {
    match op {
        PlanOverrideOpDto::AddNode { .. } => vec![],
        PlanOverrideOpDto::RemoveNode { target_node_id, .. }
        | PlanOverrideOpDto::PauseNode { target_node_id, .. }
        | PlanOverrideOpDto::ReplaceNode { target_node_id, .. } => vec![target_node_id.as_str()],
        PlanOverrideOpDto::MergeNodes {
            primary_node_id,
            secondary_node_id,
            ..
        } => vec![primary_node_id.as_str(), secondary_node_id.as_str()],
        PlanOverrideOpDto::ReorderNodes { node_id, .. } => vec![node_id.as_str()],
    }
}

/// Command #1 (`inspect_task_plan_dag`):
/// Inspects the `TaskPlan` DAG for `task_id`, distinguishing `NOT_FOUND` vs `STALE_GENERATION`
/// (carrying `current_generation = dag.plan_revision` when `expected_plan_revision` mismatches).
pub fn inspect_task_plan_dag(
    state: &PlanEditorState,
    task_id: &str,
    expected_plan_revision: Option<u64>,
) -> Result<TaskPlanDagDto, PlanEditorError> {
    let guard = state.inner.lock().expect("plan_editor mutex poisoned");
    let Some(entry) = guard.tasks.get(task_id) else {
        return Err(PlanEditorError::NotFound {
            entity_id: task_id.to_owned(),
            message: format!("Task {task_id} does not exist in TaskSpace authority"),
        });
    };

    if let Some(expected) = expected_plan_revision
        && expected != entry.dag.plan_revision
    {
        return Err(PlanEditorError::StaleGeneration {
            entity_id: task_id.to_owned(),
            expected_generation: expected,
            current_generation: entry.dag.plan_revision,
            message: format!(
                "Requested plan_revision r{expected} is stale (current_generation = r{})",
                entry.dag.plan_revision
            ),
        });
    }

    Ok(entry.dag.clone())
}

/// Command #2 (`expand_lazy_plan_supernode`, `[PLAN-LAZY-001]`):
/// Expands an unmaterialized (`METADATA_ONLY` / `COLD`) subgraph super-node on demand with
/// cursor pagination strictly clamped to `limit <= 64`, confirming `0` pre-allocated OS processes.
pub fn expand_lazy_plan_supernode(
    state: &PlanEditorState,
    task_id: &str,
    supernode_id: &str,
    requested_limit: Option<usize>,
) -> Result<SupernodeExpansionResultDto, PlanEditorError> {
    let effective_limit = clamp_plan_editor_limit(requested_limit);
    let mut guard = state.inner.lock().expect("plan_editor mutex poisoned");
    let Some(entry) = guard.tasks.get_mut(task_id) else {
        return Err(PlanEditorError::NotFound {
            entity_id: task_id.to_owned(),
            message: format!("Task {task_id} was not found"),
        });
    };

    let plan_id = entry.dag.plan_id.clone();
    let default_cap = entry
        .dag
        .authorized_capabilities
        .first()
        .cloned()
        .unwrap_or_else(|| "cap.fs.workspace.read".to_owned());

    let Some(supernode) = entry
        .dag
        .lazy_supernodes
        .iter_mut()
        .find(|s| s.supernode_id == supernode_id)
    else {
        return Err(PlanEditorError::NotFound {
            entity_id: supernode_id.to_owned(),
            message: format!("Lazy supernode {supernode_id} was not found in plan {plan_id}"),
        });
    };

    let allocated_before = supernode.allocated_process_count;
    let materialize_count = effective_limit.min(supernode.unmaterialized_step_count);
    let mut materialized_nodes = Vec::with_capacity(materialize_count);

    for idx in 0..materialize_count {
        let step_num = idx + 1;
        let deps = if idx == 0 {
            supernode.dependency_node_ids.clone()
        } else {
            vec![format!("{supernode_id}-step-{idx}")]
        };
        materialized_nodes.push(TaskNodePlanDto {
            node_id: format!("{supernode_id}-step-{step_num}"),
            group_id: supernode.group_id.clone(),
            title: format!("{} #{step_num}", supernode.title_zh),
            state: TaskNodeLifecycleState::Declared,
            dependency_node_ids: deps,
            capability_id: default_cap.clone(),
            requested_budget_units: 5,
            effective_budget_units: 5,
            usage_budget_units: 0,
            residency_tier: PlanResidencyTier::HotMaterialized,
            runtime_expanded: false,
            cross_application_target: None,
            editable_for_override: true,
            statutory_lock: None,
        });
    }

    let remaining = supernode
        .unmaterialized_step_count
        .saturating_sub(materialize_count);
    let next_cursor_hex = if remaining > 0 {
        Some(
            deterministic_hex_digest(&format!("{supernode_id}:cursor:{materialize_count}"))[..16]
                .to_owned(),
        )
    } else {
        None
    };

    supernode.expanded = true;
    supernode.materialized_nodes = materialized_nodes.clone();
    supernode.next_cursor_hex = next_cursor_hex.clone();

    let receipt_hex = deterministic_hex_digest(&format!(
        "rcpt:expand_supernode:{task_id}:{supernode_id}:{materialize_count}"
    ));

    Ok(SupernodeExpansionResultDto {
        task_id: task_id.to_owned(),
        plan_id,
        supernode_id: supernode_id.to_owned(),
        effective_limit,
        allocated_process_count_before: allocated_before,
        materialized_nodes,
        remaining_unmaterialized_count: remaining,
        next_cursor_hex,
        receipt_hex,
    })
}

/// Command #3 (`preview_plan_override_diff`, `[PLAN-CHANGE-001]`, `[PLAN-OVERRIDE-001]`, `[CTRL-PLAN-001]`):
/// Enforces Statutory Lock #3 (`NODE_COMPLETED_OR_ACTIVE_LOCKED`) and classifies override batches
/// into Path A (`BoundedControlCommand`) vs Path B (`RecompileAndZ4Reauthorize`), generating
/// Terraform-style `+ / ~ / -` `PlanBlastRadiusRowDto` items.
pub fn preview_plan_override_diff(
    state: &PlanEditorState,
    task_id: &str,
    expected_plan_revision: u64,
    operations: &[PlanOverrideOpDto],
) -> Result<PlanOverridePreviewDto, PlanEditorError> {
    let guard = state.inner.lock().expect("plan_editor mutex poisoned");
    let Some(entry) = guard.tasks.get(task_id) else {
        return Err(PlanEditorError::NotFound {
            entity_id: task_id.to_owned(),
            message: format!("Task {task_id} was not found"),
        });
    };
    let dag = &entry.dag;

    if expected_plan_revision != dag.plan_revision {
        return Err(PlanEditorError::StaleGeneration {
            entity_id: dag.plan_id.clone(),
            expected_generation: expected_plan_revision,
            current_generation: dag.plan_revision,
            message: format!(
                "Plan revision mismatch: expected r{expected_plan_revision}, current r{}",
                dag.plan_revision
            ),
        });
    }

    if operations.is_empty() {
        return Err(PlanEditorError::EmptyOverrideBatch {
            plan_id: dag.plan_id.clone(),
            message: "Plan override batch cannot be empty".to_owned(),
        });
    }

    // Enforce Statutory Lock #3: COMPLETED / ACTIVE / COMMITTED nodes are strictly read-only
    for op in operations {
        for target_id in referenced_existing_node_ids(op) {
            let Some(node) = find_node(dag, target_id) else {
                return Err(PlanEditorError::NotFound {
                    entity_id: target_id.to_owned(),
                    message: format!("Target TaskNode {target_id} was not found in plan"),
                });
            };
            if !node.state.is_editable_for_override() {
                return Err(PlanEditorError::ReadOnlyExecutedTaskNode {
                    node_id: node.node_id.clone(),
                    node_state: node.state,
                    statutory_lock: STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE.to_owned(),
                    message: format!(
                        "TaskNode {} in state {:?} is locked against structural edits ({})",
                        node.node_id, node.state, STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE
                    ),
                });
            }
        }
    }

    let mut requires_recompile_and_z4 = false;
    let mut current_total_budget: u64 = dag.nodes.iter().map(|n| n.requested_budget_units).sum();
    let mut diff_rows = Vec::new();
    let mut compiled_commands = Vec::new();

    for (idx, op) in operations.iter().enumerate() {
        match op {
            PlanOverrideOpDto::PauseNode {
                target_node_id,
                reason,
            } => {
                let target = find_node(dag, target_node_id).expect("checked above");
                compiled_commands.push(CompiledPlanControlCommandDto {
                    command_id_hex: deterministic_hex_digest(&format!(
                        "cmd:pause:{}:{}:{target_node_id}:{idx}",
                        dag.plan_id, dag.plan_revision
                    )),
                    control_scope: "task-node".to_owned(),
                    target_id: target_node_id.clone(),
                    action: "PAUSE_TASK_NODE".to_owned(),
                    expected_plan_revision,
                    idempotency_key_hex: deterministic_hex_digest(&format!(
                        "idem:pause:{}:{expected_plan_revision}:{target_node_id}",
                        dag.plan_id
                    )),
                });
                diff_rows.push(PlanBlastRadiusRowDto {
                    symbol: "~".to_owned(),
                    category: "paused".to_owned(),
                    scope_label: "TaskNode".to_owned(),
                    target_label: format!("{} ({target_node_id})", target.title),
                    reason: reason.clone().unwrap_or_else(|| {
                        "Bounded step pause within authorized envelope".to_owned()
                    }),
                });
            }

            PlanOverrideOpDto::ReorderNodes {
                node_id,
                new_dependency_node_ids,
                within_authorized_envelope,
            } => {
                let target = find_node(dag, node_id).expect("checked above");
                if *within_authorized_envelope {
                    compiled_commands.push(CompiledPlanControlCommandDto {
                        command_id_hex: deterministic_hex_digest(&format!(
                            "cmd:reorder:{}:{}:{node_id}:{idx}",
                            dag.plan_id, dag.plan_revision
                        )),
                        control_scope: "task-plan".to_owned(),
                        target_id: dag.plan_id.clone(),
                        action: "REORDER_TASK_PLAN_NODES".to_owned(),
                        expected_plan_revision,
                        idempotency_key_hex: deterministic_hex_digest(&format!(
                            "idem:reorder:{}:{expected_plan_revision}:{node_id}:{}",
                            dag.plan_id,
                            new_dependency_node_ids.join(",")
                        )),
                    });
                } else {
                    requires_recompile_and_z4 = true;
                }
                diff_rows.push(PlanBlastRadiusRowDto {
                    symbol: "~".to_owned(),
                    category: "reverified".to_owned(),
                    scope_label: "TaskNode".to_owned(),
                    target_label: format!("{} ({node_id})", target.title),
                    reason: format!(
                        "Reorder dependencies {:?} -> {:?}",
                        target.dependency_node_ids, new_dependency_node_ids
                    ),
                });
            }

            PlanOverrideOpDto::AddNode {
                node_id,
                group_id,
                title,
                capability_id,
                requested_budget_units,
                cross_application_target,
                ..
            } => {
                requires_recompile_and_z4 = true;
                current_total_budget = current_total_budget.saturating_add(*requested_budget_units);
                diff_rows.push(PlanBlastRadiusRowDto {
                    symbol: "+".to_owned(),
                    category: "created".to_owned(),
                    scope_label: "TaskNode".to_owned(),
                    target_label: format!("{title} ({node_id})"),
                    reason: format!(
                        "Add step in group {group_id} (cap={capability_id}, budget={requested_budget_units})"
                    ),
                });
                if !dag
                    .authorized_capabilities
                    .iter()
                    .any(|c| c == capability_id)
                {
                    diff_rows.push(PlanBlastRadiusRowDto {
                        symbol: "+".to_owned(),
                        category: "reverified".to_owned(),
                        scope_label: "Capability".to_owned(),
                        target_label: capability_id.clone(),
                        reason: "New capability required by added TaskNode".to_owned(),
                    });
                }
                if let Some(cross_app) = cross_application_target {
                    diff_rows.push(PlanBlastRadiusRowDto {
                        symbol: "+".to_owned(),
                        category: "reverified".to_owned(),
                        scope_label: "CrossApplication".to_owned(),
                        target_label: cross_app.clone(),
                        reason: "Cross-application collaboration target introduced".to_owned(),
                    });
                }
            }

            PlanOverrideOpDto::RemoveNode {
                target_node_id,
                alters_acceptance_or_side_effects,
            } => {
                let target = find_node(dag, target_node_id).expect("checked above");
                if *alters_acceptance_or_side_effects {
                    requires_recompile_and_z4 = true;
                }
                current_total_budget =
                    current_total_budget.saturating_sub(target.requested_budget_units);
                diff_rows.push(PlanBlastRadiusRowDto {
                    symbol: "-".to_owned(),
                    category: "terminated".to_owned(),
                    scope_label: "TaskNode".to_owned(),
                    target_label: format!("{} ({target_node_id})", target.title),
                    reason: "Remove unexecuted step from TaskPlan DAG".to_owned(),
                });
            }

            PlanOverrideOpDto::MergeNodes {
                primary_node_id,
                secondary_node_id,
                merged_title,
                cross_scope_or_capability,
            } => {
                let primary = find_node(dag, primary_node_id).expect("checked above");
                let secondary = find_node(dag, secondary_node_id).expect("checked above");
                if *cross_scope_or_capability
                    || primary.capability_id != secondary.capability_id
                    || primary.group_id != secondary.group_id
                {
                    requires_recompile_and_z4 = true;
                }
                diff_rows.push(PlanBlastRadiusRowDto {
                    symbol: "~".to_owned(),
                    category: "reverified".to_owned(),
                    scope_label: "TaskNode".to_owned(),
                    target_label: format!(
                        "{merged_title} ({primary_node_id} + {secondary_node_id})"
                    ),
                    reason: format!("Merge {} and {}", primary.title, secondary.title),
                });
            }

            PlanOverrideOpDto::ReplaceNode {
                target_node_id,
                replacement_node_id,
                replacement_title,
                capability_id,
                requested_budget_units,
                cross_application_target,
            } => {
                let target = find_node(dag, target_node_id).expect("checked above");
                requires_recompile_and_z4 = true;
                current_total_budget = current_total_budget
                    .saturating_sub(target.requested_budget_units)
                    .saturating_add(*requested_budget_units);
                diff_rows.push(PlanBlastRadiusRowDto {
                    symbol: "~".to_owned(),
                    category: "reverified".to_owned(),
                    scope_label: "TaskNode".to_owned(),
                    target_label: format!(
                        "{} -> {replacement_title} ({replacement_node_id})",
                        target.title
                    ),
                    reason: format!(
                        "Replace {target_node_id} with {replacement_node_id} (cap={capability_id}, budget={requested_budget_units})"
                    ),
                });
                if !dag
                    .authorized_capabilities
                    .iter()
                    .any(|c| c == capability_id)
                {
                    diff_rows.push(PlanBlastRadiusRowDto {
                        symbol: "+".to_owned(),
                        category: "reverified".to_owned(),
                        scope_label: "Capability".to_owned(),
                        target_label: capability_id.clone(),
                        reason: "New capability required by replacement TaskNode".to_owned(),
                    });
                }
                if let Some(cross_app) = cross_application_target {
                    diff_rows.push(PlanBlastRadiusRowDto {
                        symbol: "+".to_owned(),
                        category: "reverified".to_owned(),
                        scope_label: "CrossApplication".to_owned(),
                        target_label: cross_app.clone(),
                        reason: "Cross-application collaboration target introduced".to_owned(),
                    });
                }
            }
        }
    }

    if current_total_budget > dag.authorized_budget_ceiling_units {
        requires_recompile_and_z4 = true;
        diff_rows.push(PlanBlastRadiusRowDto {
            symbol: "~".to_owned(),
            category: "throttled".to_owned(),
            scope_label: "BudgetEnvelope".to_owned(),
            target_label: "TaskPlan Authorized Budget Ceiling".to_owned(),
            reason: format!(
                "Budget ceiling expands from {} to {} units",
                dag.authorized_budget_ceiling_units, current_total_budget
            ),
        });
    }

    let path_kind = if requires_recompile_and_z4 {
        PlanOverridePathKind::RecompileAndZ4Reauthorize
    } else {
        PlanOverridePathKind::BoundedControlCommand
    };
    let candidate_plan_revision = dag.plan_revision + 1;
    let ops_json = serde_json::to_string(operations).unwrap_or_default();
    let candidate_plan_digest_hex = deterministic_hex_digest(&format!(
        "{}:r{candidate_plan_revision}:{}:{ops_json}",
        dag.plan_id, dag.plan_digest_hex
    ));

    let added_count = diff_rows.iter().filter(|r| r.symbol == "+").count();
    let modified_count = diff_rows.iter().filter(|r| r.symbol == "~").count();
    let removed_count = diff_rows.iter().filter(|r| r.symbol == "-").count();

    Ok(PlanOverridePreviewDto {
        task_id: dag.task_id.clone(),
        plan_id: dag.plan_id.clone(),
        path_kind,
        requires_z4_reauthorization: requires_recompile_and_z4,
        base_plan_revision: dag.plan_revision,
        candidate_plan_revision,
        base_plan_digest_hex: dag.plan_digest_hex.clone(),
        candidate_plan_digest_hex,
        compiled_control_commands: if requires_recompile_and_z4 {
            vec![]
        } else {
            compiled_commands
        },
        diff_rows,
        added_count,
        modified_count,
        removed_count,
    })
}

/// Helper to issue a deterministic `Z4ReauthorizationGrantDto` for a previewed Path B override.
#[must_use]
pub fn issue_z4_reauthorization_grant(
    preview: &PlanOverridePreviewDto,
    policy_epoch: u64,
) -> Z4ReauthorizationGrantDto {
    let grant_token_hex = deterministic_hex_digest(&format!(
        "z4_grant:{}:r{}:{}:epoch{policy_epoch}",
        preview.plan_id, preview.base_plan_revision, preview.candidate_plan_digest_hex
    ));
    Z4ReauthorizationGrantDto {
        grant_token_hex,
        plan_id: preview.plan_id.clone(),
        expected_plan_revision: preview.base_plan_revision,
        approved_plan_digest_hex: preview.candidate_plan_digest_hex.clone(),
        authorizer_policy_epoch: policy_epoch,
    }
}

/// Command #4 (`submit_plan_override`, `[PLAN-CHANGE-001]`, `[PLAN-OVERRIDE-001]`, `[CTRL-PLAN-001]`):
/// Commits a batch of `TaskPlan` DAG overrides:
/// - Rejects edits on `COMPLETED` / `ACTIVE` / `COMMITTED` nodes (`READ_ONLY_EXECUTED_TASK_NODE`).
/// - Rejects stale `expected_plan_revision` (`STALE_GENERATION`).
/// - Rejects Path B (`RecompileAndZ4Reauthorize`) overrides if `z4_grant` is missing or does not
///   match `(plan_id, expected_plan_revision, candidate_plan_digest_hex)` (`Z4_REAUTHORIZATION_REQUIRED`).
pub fn submit_plan_override(
    state: &PlanEditorState,
    task_id: &str,
    expected_plan_revision: u64,
    operations: &[PlanOverrideOpDto],
    z4_grant: Option<&Z4ReauthorizationGrantDto>,
) -> Result<PlanOverrideCommitReceiptDto, PlanEditorError> {
    let preview = preview_plan_override_diff(state, task_id, expected_plan_revision, operations)?;

    if preview.requires_z4_reauthorization {
        let valid_grant = z4_grant.is_some_and(|g| {
            !g.grant_token_hex.trim().is_empty()
                && g.plan_id == preview.plan_id
                && g.expected_plan_revision == expected_plan_revision
                && g.approved_plan_digest_hex == preview.candidate_plan_digest_hex
        });
        if !valid_grant {
            return Err(PlanEditorError::Z4ReauthorizationRequired {
                plan_id: preview.plan_id,
                expected_plan_revision,
                candidate_plan_digest_hex: preview.candidate_plan_digest_hex,
                message: "Path B structural plan override or capability/budget expansion requires Z4 policy re-authorization and cannot bypass AdmissionPlan".to_owned(),
            });
        }
    }

    let mut guard = state.inner.lock().expect("plan_editor mutex poisoned");
    let entry = guard
        .tasks
        .get_mut(task_id)
        .expect("verified in preview_plan_override_diff");

    for op in operations {
        match op {
            PlanOverrideOpDto::PauseNode { target_node_id, .. } => {
                for n in &mut entry.dag.nodes {
                    if &n.node_id == target_node_id {
                        n.state = TaskNodeLifecycleState::Suspended;
                    }
                }
            }
            PlanOverrideOpDto::ReorderNodes {
                node_id,
                new_dependency_node_ids,
                ..
            } => {
                for n in &mut entry.dag.nodes {
                    if &n.node_id == node_id {
                        n.dependency_node_ids.clone_from(new_dependency_node_ids);
                    }
                }
            }
            PlanOverrideOpDto::AddNode {
                node_id,
                group_id,
                title,
                dependency_node_ids,
                capability_id,
                requested_budget_units,
                cross_application_target,
            } => {
                if !entry
                    .dag
                    .authorized_capabilities
                    .iter()
                    .any(|c| c == capability_id)
                {
                    entry
                        .dag
                        .authorized_capabilities
                        .push(capability_id.clone());
                }
                entry.dag.nodes.push(TaskNodePlanDto {
                    node_id: node_id.clone(),
                    group_id: group_id.clone(),
                    title: title.clone(),
                    state: TaskNodeLifecycleState::Declared,
                    dependency_node_ids: dependency_node_ids.clone(),
                    capability_id: capability_id.clone(),
                    requested_budget_units: *requested_budget_units,
                    effective_budget_units: *requested_budget_units,
                    usage_budget_units: 0,
                    residency_tier: PlanResidencyTier::HotMaterialized,
                    runtime_expanded: false,
                    cross_application_target: cross_application_target.clone(),
                    editable_for_override: true,
                    statutory_lock: None,
                });
            }
            PlanOverrideOpDto::RemoveNode { target_node_id, .. } => {
                entry.dag.nodes.retain(|n| &n.node_id != target_node_id);
                for n in &mut entry.dag.nodes {
                    n.dependency_node_ids.retain(|d| d != target_node_id);
                }
            }
            PlanOverrideOpDto::MergeNodes {
                primary_node_id,
                secondary_node_id,
                merged_title,
                ..
            } => {
                let secondary_clone = entry
                    .dag
                    .nodes
                    .iter()
                    .find(|n| &n.node_id == secondary_node_id)
                    .cloned();
                if let Some(sec) = secondary_clone {
                    entry.dag.nodes.retain(|n| &n.node_id != secondary_node_id);
                    for n in &mut entry.dag.nodes {
                        if &n.node_id == primary_node_id {
                            n.title.clone_from(merged_title);
                            n.requested_budget_units = n
                                .requested_budget_units
                                .saturating_add(sec.requested_budget_units);
                            n.effective_budget_units = n
                                .effective_budget_units
                                .saturating_add(sec.effective_budget_units);
                            for dep in &sec.dependency_node_ids {
                                if dep != primary_node_id
                                    && dep != secondary_node_id
                                    && !n.dependency_node_ids.contains(dep)
                                {
                                    n.dependency_node_ids.push(dep.clone());
                                }
                            }
                        } else {
                            for dep in &mut n.dependency_node_ids {
                                if dep == secondary_node_id {
                                    dep.clone_from(primary_node_id);
                                }
                            }
                        }
                    }
                }
            }
            PlanOverrideOpDto::ReplaceNode {
                target_node_id,
                replacement_node_id,
                replacement_title,
                capability_id,
                requested_budget_units,
                cross_application_target,
            } => {
                if !entry
                    .dag
                    .authorized_capabilities
                    .iter()
                    .any(|c| c == capability_id)
                {
                    entry
                        .dag
                        .authorized_capabilities
                        .push(capability_id.clone());
                }
                for n in &mut entry.dag.nodes {
                    if &n.node_id == target_node_id {
                        n.node_id.clone_from(replacement_node_id);
                        n.title.clone_from(replacement_title);
                        n.capability_id.clone_from(capability_id);
                        n.requested_budget_units = *requested_budget_units;
                        n.effective_budget_units = *requested_budget_units;
                        n.cross_application_target
                            .clone_from(cross_application_target);
                    } else {
                        for dep in &mut n.dependency_node_ids {
                            if dep == target_node_id {
                                dep.clone_from(replacement_node_id);
                            }
                        }
                    }
                }
            }
        }
    }

    let total_budget: u64 = entry
        .dag
        .nodes
        .iter()
        .map(|n| n.requested_budget_units)
        .sum();
    if total_budget > entry.dag.authorized_budget_ceiling_units {
        entry.dag.authorized_budget_ceiling_units = total_budget;
    }

    entry.dag.plan_revision = preview.candidate_plan_revision;
    entry
        .dag
        .plan_digest_hex
        .clone_from(&preview.candidate_plan_digest_hex);
    entry.swimlanes.plan_revision = preview.candidate_plan_revision;

    let receipt_hex = deterministic_hex_digest(&format!(
        "rcpt:plan_override_commit:{}:r{}:{}",
        entry.dag.plan_id, entry.dag.plan_revision, entry.dag.plan_digest_hex
    ));
    entry.dag.receipt_hex.clone_from(&receipt_hex);

    Ok(PlanOverrideCommitReceiptDto {
        task_id: task_id.to_owned(),
        plan_id: entry.dag.plan_id.clone(),
        path_kind: preview.path_kind,
        new_plan_revision: entry.dag.plan_revision,
        new_plan_digest_hex: entry.dag.plan_digest_hex.clone(),
        compiled_control_commands: preview.compiled_control_commands,
        z4_grant_token_hex: z4_grant.map(|g| g.grant_token_hex.clone()),
        receipt_hex,
        updated_dag: entry.dag.clone(),
    })
}

/// Command #5 (`inspect_task_attempt_lanes`, `[TASK-SPEC-001]`, `[TASK-PROVIDER-001]`, `[TASK-AGENT-001]`, `[TASK-ISO-005]`):
/// Returns the Temporal proportional `TaskAttempt` racing swimlanes and `ExecutionFiber` phase
/// duration waterfalls for `task_id`, distinguishing `NOT_FOUND` vs `STALE_GENERATION`.
pub fn inspect_task_attempt_lanes(
    state: &PlanEditorState,
    task_id: &str,
    expected_plan_revision: Option<u64>,
) -> Result<TaskAttemptSwimlanesInspectDto, PlanEditorError> {
    let guard = state.inner.lock().expect("plan_editor mutex poisoned");
    let Some(entry) = guard.tasks.get(task_id) else {
        return Err(PlanEditorError::NotFound {
            entity_id: task_id.to_owned(),
            message: format!("Task {task_id} was not found"),
        });
    };

    if let Some(expected) = expected_plan_revision
        && expected != entry.swimlanes.plan_revision
    {
        return Err(PlanEditorError::StaleGeneration {
            entity_id: task_id.to_owned(),
            expected_generation: expected,
            current_generation: entry.swimlanes.plan_revision,
            message: format!(
                "Requested attempt lanes revision r{expected} is stale (current_generation = r{})",
                entry.swimlanes.plan_revision
            ),
        });
    }

    Ok(entry.swimlanes.clone())
}

/// Enforces Statutory Lock #4 (`TASK_ATTEMPT_NO_MANUAL_COMMIT` / `FORBIDDEN_MANUAL_ATTEMPT_COMMIT`):
/// Deterministically rejects any attempt to manually mark a `TaskAttempt` as committed/winner,
/// while allowing `cancel_attempt`, `supersede_attempt`, and `request_reverify`.
pub fn execute_task_attempt_control(
    state: &PlanEditorState,
    task_id: &str,
    attempt_id: &str,
    verb: &str,
) -> Result<TaskAttemptSwimlanesInspectDto, PlanEditorError> {
    let normalized = verb.trim().to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "manual_commit" | "commit" | "force_commit" | "mark_winner" | "override_commit"
    ) {
        return Err(PlanEditorError::ForbiddenManualAttemptCommit {
            attempt_id: attempt_id.to_owned(),
            rejected_verb: verb.to_owned(),
            statutory_lock: STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT.to_owned(),
            message: format!(
                "Winner commit is adjudicated atomically by kernel TaskHead CAS; manual commit verb '{verb}' on {attempt_id} is forbidden ({STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT})"
            ),
        });
    }

    if !matches!(
        normalized.as_str(),
        "cancel_attempt" | "supersede_attempt" | "request_reverify"
    ) {
        return Err(PlanEditorError::ForbiddenManualAttemptCommit {
            attempt_id: attempt_id.to_owned(),
            rejected_verb: verb.to_owned(),
            statutory_lock: STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT.to_owned(),
            message: format!("Unauthorized TaskAttempt verb '{verb}'"),
        });
    }

    let mut guard = state.inner.lock().expect("plan_editor mutex poisoned");
    let Some(entry) = guard.tasks.get_mut(task_id) else {
        return Err(PlanEditorError::NotFound {
            entity_id: task_id.to_owned(),
            message: format!("Task {task_id} was not found"),
        });
    };

    let Some(attempt) = entry
        .swimlanes
        .attempts
        .iter_mut()
        .find(|a| a.attempt_id == attempt_id)
    else {
        return Err(PlanEditorError::NotFound {
            entity_id: attempt_id.to_owned(),
            message: format!("TaskAttempt {attempt_id} was not found"),
        });
    };

    match normalized.as_str() {
        "cancel_attempt" => {
            attempt.state = TaskNodeLifecycleState::Cancelled;
            attempt.retry_fence_epoch += 1;
        }
        "supersede_attempt" => {
            attempt.state = TaskNodeLifecycleState::Superseded;
            attempt.superseded = true;
            attempt.cas_winner = false;
            attempt.retry_fence_epoch += 1;
        }
        "request_reverify" => {
            attempt.state = TaskNodeLifecycleState::Recovering;
        }
        _ => unreachable!(),
    }

    entry.swimlanes.task_head_cas_epoch += 1;
    entry.swimlanes.receipt_hex = deterministic_hex_digest(&format!(
        "rcpt:attempt_ctrl:{task_id}:{attempt_id}:{normalized}:epoch{}",
        entry.swimlanes.task_head_cas_epoch
    ));

    Ok(entry.swimlanes.clone())
}

/// Command #6 (`reopen_terminal_task`, `[TASK-REOPEN-001]`):
/// - `RetryNewAttempt`: spawns a new `TaskAttempt` (`attempt_index + 1`, incremented `retry_fence_epoch`)
///   on a terminal or partially failed task while keeping all prior attempts immutable.
/// - `RefineNewGeneration`: spawns a new `Task` generation (`installation_generation + 1`,
///   `plan_revision + 1`) while preserving the prior task's historical receipt chain.
pub fn reopen_terminal_task(
    state: &PlanEditorState,
    task_id: &str,
    mode: ReopenTaskMode,
) -> Result<ReopenTerminalTaskResultDto, PlanEditorError> {
    let mut guard = state.inner.lock().expect("plan_editor mutex poisoned");
    let Some(existing_entry) = guard.tasks.get(task_id).cloned() else {
        return Err(PlanEditorError::NotFound {
            entity_id: task_id.to_owned(),
            message: format!("Task {task_id} was not found"),
        });
    };

    let is_reopenable = existing_entry.dag.task_state.is_terminal_or_reopenable()
        || existing_entry.dag.failed_targets > 0;
    if !is_reopenable {
        return Err(PlanEditorError::TaskNotTerminalOrPartial {
            task_id: task_id.to_owned(),
            current_state: existing_entry.dag.task_state,
            message: format!(
                "Task {task_id} in state {:?} without failed targets cannot be reopened",
                existing_entry.dag.task_state
            ),
        });
    }

    match mode {
        ReopenTaskMode::RetryNewAttempt => {
            let entry = guard.tasks.get_mut(task_id).expect("verified above");
            let next_idx = entry
                .swimlanes
                .attempts
                .iter()
                .map(|a| a.attempt_index)
                .max()
                .unwrap_or(0)
                + 1;
            let next_fence = entry
                .swimlanes
                .attempts
                .iter()
                .map(|a| a.retry_fence_epoch)
                .max()
                .unwrap_or(0)
                + 1;
            let spawned_attempt_id = format!("{task_id}-attempt-{next_idx}");
            let receipt_hex = deterministic_hex_digest(&format!(
                "rcpt:reopen_retry:{task_id}:{spawned_attempt_id}:fence{next_fence}"
            ));

            entry.swimlanes.attempts.push(TaskAttemptSwimlaneDto {
                attempt_id: spawned_attempt_id.clone(),
                task_node_id: entry.swimlanes.task_node_id.clone(),
                attempt_index: next_idx,
                state: TaskNodeLifecycleState::Active,
                input_snapshot_digest_hex: deterministic_hex_digest(&format!(
                    "snap:{spawned_attempt_id}:fence{next_fence}"
                )),
                isolation_domain_id_hex: deterministic_hex_digest(&format!(
                    "iso:{spawned_attempt_id}"
                ))[..32]
                    .to_owned(),
                isolation_enforcement_profile: "STRICT_SANDBOX".to_owned(),
                provider_isolation_unverified: false,
                started_at_offset_ms: 200,
                duration_ms: 45,
                cas_winner: false,
                superseded: false,
                retry_fence_epoch: next_fence,
                allowed_control_verbs: vec![
                    "cancel_attempt".to_owned(),
                    "supersede_attempt".to_owned(),
                    "request_reverify".to_owned(),
                ],
                statutory_lock: STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT.to_owned(),
                fibers: vec![ExecutionFiberWaterfallDto {
                    fiber_id: format!("{spawned_attempt_id}-fiber-1"),
                    attempt_id: spawned_attempt_id.clone(),
                    process_id: format!("proc-{spawned_attempt_id}"),
                    label: "Retry Worker Fiber".to_owned(),
                    shared_thread_like: false,
                    fault_domain_badge: None,
                    active_cpu_ms: 25,
                    scheduler_wait_ms: 5,
                    external_wait_ms: 10,
                    backpressure_wait_ms: 5,
                    suspended_ms: 0,
                }],
                operations: vec![],
                cell_placement: CellPlacementForensicDto {
                    cell_id_hex: "cell-local-01".to_owned(),
                    lease_epoch: next_fence,
                    node_boot_generation: 2,
                    placement_mode: "LOCAL_SINGLE_NODE".to_owned(),
                    fencing_token_hex: deterministic_hex_digest(&format!(
                        "fence:{spawned_attempt_id}"
                    ))[..32]
                        .to_owned(),
                },
                receipt_hex: receipt_hex.clone(),
            });

            entry.swimlanes.task_head_cas_epoch += 1;
            entry.dag.task_state = TaskNodeLifecycleState::Running;
            entry.dag.receipt_hex.clone_from(&receipt_hex);

            let new_gen = entry.dag.installation_generation;
            let new_rev = entry.dag.plan_revision;
            let preserved_ids: Vec<String> = guard.tasks.keys().cloned().collect();

            Ok(ReopenTerminalTaskResultDto {
                mode,
                source_task_id: task_id.to_owned(),
                resulting_task_id: task_id.to_owned(),
                spawned_attempt_id: Some(spawned_attempt_id),
                new_installation_generation: new_gen,
                new_plan_revision: new_rev,
                retry_fence_epoch: next_fence,
                preserved_history_task_ids: preserved_ids,
                receipt_hex,
            })
        }

        ReopenTaskMode::RefineNewGeneration => {
            let next_gen = existing_entry.dag.installation_generation + 1;
            let next_rev = existing_entry.dag.plan_revision + 1;
            let new_task_id = format!("{task_id}-gen{next_gen}");
            let new_plan_id = format!("{}-gen{next_gen}", existing_entry.dag.plan_id);
            let receipt_hex = deterministic_hex_digest(&format!(
                "rcpt:reopen_refine:{task_id}->{new_task_id}:gen{next_gen}:r{next_rev}"
            ));

            let mut refined = existing_entry;
            refined.dag.task_id.clone_from(&new_task_id);
            refined.dag.plan_id.clone_from(&new_plan_id);
            refined.dag.installation_generation = next_gen;
            refined.dag.plan_revision = next_rev;
            refined.dag.task_state = TaskNodeLifecycleState::Ready;
            refined.dag.failed_targets = 0;
            refined.dag.plan_digest_hex =
                deterministic_hex_digest(&format!("plan_digest:{new_plan_id}:r{next_rev}"));
            refined.dag.receipt_hex.clone_from(&receipt_hex);

            refined.swimlanes.task_id.clone_from(&new_task_id);
            refined.swimlanes.plan_revision = next_rev;
            refined.swimlanes.task_head_cas_epoch += 1;
            refined.swimlanes.receipt_hex.clone_from(&receipt_hex);

            let fence_epoch = refined.swimlanes.task_head_cas_epoch;
            guard.tasks.insert(new_task_id.clone(), refined);
            let preserved_ids: Vec<String> = guard.tasks.keys().cloned().collect();

            Ok(ReopenTerminalTaskResultDto {
                mode,
                source_task_id: task_id.to_owned(),
                resulting_task_id: new_task_id,
                spawned_attempt_id: None,
                new_installation_generation: next_gen,
                new_plan_revision: next_rev,
                retry_fence_epoch: fence_epoch,
                preserved_history_task_ids: preserved_ids,
                receipt_hex,
            })
        }
    }
}

// ============================================================================
// Tauri Command Wrappers
// ============================================================================

#[tauri::command]
pub fn inspect_task_plan_dag_cmd(
    state: tauri::State<'_, PlanEditorState>,
    task_id: String,
    expected_plan_revision: Option<u64>,
) -> Result<TaskPlanDagDto, PlanEditorError> {
    inspect_task_plan_dag(&state, &task_id, expected_plan_revision)
}

#[tauri::command]
pub fn expand_lazy_plan_supernode_cmd(
    state: tauri::State<'_, PlanEditorState>,
    task_id: String,
    supernode_id: String,
    requested_limit: Option<usize>,
) -> Result<SupernodeExpansionResultDto, PlanEditorError> {
    expand_lazy_plan_supernode(&state, &task_id, &supernode_id, requested_limit)
}

#[tauri::command]
pub fn preview_plan_override_diff_cmd(
    state: tauri::State<'_, PlanEditorState>,
    task_id: String,
    expected_plan_revision: u64,
    operations: Vec<PlanOverrideOpDto>,
) -> Result<PlanOverridePreviewDto, PlanEditorError> {
    preview_plan_override_diff(&state, &task_id, expected_plan_revision, &operations)
}

#[tauri::command]
pub fn submit_plan_override_cmd(
    state: tauri::State<'_, PlanEditorState>,
    task_id: String,
    expected_plan_revision: u64,
    operations: Vec<PlanOverrideOpDto>,
    z4_grant: Option<Z4ReauthorizationGrantDto>,
) -> Result<PlanOverrideCommitReceiptDto, PlanEditorError> {
    submit_plan_override(
        &state,
        &task_id,
        expected_plan_revision,
        &operations,
        z4_grant.as_ref(),
    )
}

#[tauri::command]
pub fn inspect_task_attempt_lanes_cmd(
    state: tauri::State<'_, PlanEditorState>,
    task_id: String,
    expected_plan_revision: Option<u64>,
) -> Result<TaskAttemptSwimlanesInspectDto, PlanEditorError> {
    inspect_task_attempt_lanes(&state, &task_id, expected_plan_revision)
}

#[tauri::command]
pub fn reopen_terminal_task_cmd(
    state: tauri::State<'_, PlanEditorState>,
    task_id: String,
    mode: ReopenTaskMode,
) -> Result<ReopenTerminalTaskResultDto, PlanEditorError> {
    reopen_terminal_task(&state, &task_id, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_clamp_plan_editor_limit_enforces_max_64() {
        assert_eq!(clamp_plan_editor_limit(None), MAX_PLAN_EDITOR_ENUM_LIMIT);
        assert_eq!(clamp_plan_editor_limit(Some(0)), MAX_PLAN_EDITOR_ENUM_LIMIT);
        assert_eq!(clamp_plan_editor_limit(Some(24)), 24);
        assert_eq!(
            clamp_plan_editor_limit(Some(256)),
            MAX_PLAN_EDITOR_ENUM_LIMIT
        );
    }

    #[test]
    fn unit_statutory_lock_3_blocks_active_and_completed_nodes() {
        let state = PlanEditorState::new();
        let active_err = preview_plan_override_diff(
            &state,
            "task-01",
            3,
            &[PlanOverrideOpDto::PauseNode {
                target_node_id: "plan-01-node-active".to_owned(),
                reason: Some("unit test".to_owned()),
            }],
        )
        .expect_err("ACTIVE node must reject override");
        assert!(matches!(
            active_err,
            PlanEditorError::ReadOnlyExecutedTaskNode {
                ref statutory_lock,
                node_state: TaskNodeLifecycleState::Active,
                ..
            } if statutory_lock == STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE
        ));
    }

    #[test]
    fn unit_path_a_vs_path_b_classification_is_deterministic() {
        let state = PlanEditorState::new();
        let path_a = preview_plan_override_diff(
            &state,
            "task-01",
            3,
            &[PlanOverrideOpDto::PauseNode {
                target_node_id: "plan-01-node-pending-a".to_owned(),
                reason: Some("Hold pending".to_owned()),
            }],
        )
        .expect("Path A preview must succeed");
        assert_eq!(
            path_a.path_kind,
            PlanOverridePathKind::BoundedControlCommand
        );
        assert!(!path_a.requires_z4_reauthorization);

        let path_b = preview_plan_override_diff(
            &state,
            "task-02",
            3,
            &[PlanOverrideOpDto::AddNode {
                node_id: "plan-02-node-unit-gate".to_owned(),
                group_id: "plan-02-grp-commit".to_owned(),
                title: "Unit Gate".to_owned(),
                dependency_node_ids: vec!["plan-02-node-pending-b".to_owned()],
                capability_id: "cap.net.audit.export".to_owned(),
                requested_budget_units: 60,
                cross_application_target: Some("app.security.auditor".to_owned()),
            }],
        )
        .expect("Path B preview must succeed");
        assert_eq!(
            path_b.path_kind,
            PlanOverridePathKind::RecompileAndZ4Reauthorize
        );
        assert!(path_b.requires_z4_reauthorization);
    }

    #[test]
    fn unit_statutory_lock_4_and_5_refuse_manual_commit_and_effect_unknown_skip() {
        let state = PlanEditorState::new();
        let err4 = execute_task_attempt_control(&state, "task-01", "task-01-attempt-1", "commit")
            .expect_err("Statutory Lock #4 must refuse manual attempt commit");
        assert!(matches!(
            err4,
            PlanEditorError::ForbiddenManualAttemptCommit {
                ref statutory_lock,
                ..
            } if statutory_lock == STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT
        ));

        let lanes = inspect_task_attempt_lanes(&state, "task-01", Some(3)).expect("lanes");
        assert_eq!(
            lanes.attempts[0].operations[0].statutory_lock.as_deref(),
            Some(STATUTORY_LOCK_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS)
        );
    }
}
