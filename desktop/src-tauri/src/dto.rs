//! 前端 DTO:后端把 `ControlReceipt` 投影为可序列化的 JSON 形态。
//! `receipt_hex` 恒为 [`nlos_system_control::control::receipt_to_hex`] 的输出,
//! 与 CLI 的 `RECEIPT <hex>` 行共用同一等价契约(B-TASK-006L 三入口字节一致)。

use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_system_control::cell_inspector::{
    CellLeaseInspectPage, CellLeaseRecord, CellListPage, CellSummaryRecord,
};
use nlos_system_control::control::{
    ApplicationInspection, ControlOutcome, ControlReceipt, DurableOperationInspection,
    ExecutionFiberInspection, NotificationInspection, ProcessInspection, RecoveryInspection,
    ResourceInspection, ResourceRecoveryInspection, SearchInspection, SemanticRecoveryInspection,
    TaskGroupInspection, TaskNodeInspection, TopicInspection, receipt_to_hex,
};
use nlos_system_control::desktop_enum::{
    ChooserGrantListPage, ChooserGrantRecord, ContentTypeAssociationListPage,
    ContentTypeAssociationRecord, ControlBlastRadiusImpactRecord, ControlBlastRadiusPreviewResult,
    ControlCommandStatusRecord, DesktopWatermarkEventRecord, DesktopWatermarkPollPage,
    DurableOperationListPage, ExecutionFiberListPage, ExecutionFiberSummaryRecord,
    InstalledApplicationListPage, InstalledApplicationSummaryRecord, KeychainReferenceListPage,
    KeychainReferenceRecord, NotificationListPage, NotificationSummaryRecord, ProcessListPage,
    ProcessSummaryRecord, ResourceReservationListPage, ResourceReservationSummaryRecord,
    SearchCandidateQueryPage, SearchCandidateRecord, SettingsDomainProjection, SettingsEntryRecord,
    TaskAttemptListPage, TaskAttemptSummaryRecord, TaskGroupListPage, TaskGroupSummaryRecord,
    TaskListPage, TaskNodeListPage, TaskSummaryRecord, TopicListPage, TopicSummaryRecord,
    WorkspaceListPage, WorkspaceRecord,
};

/// 一条 escalated 告警(artifact/semantic 共用形态)。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryAlertDto {
    pub plan_id_hex: String,
    pub total_failures: u64,
    pub acknowledged_receipt_id_hex: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSummaryDto {
    pub task_id_hex: String,
    pub plan_id_hex: String,
    pub application_id_hex: Option<String>,
    pub installation_generation: u64,
    pub plan_revision: u64,
    pub task_state: String,
    pub undo_class: String,
    pub reservation_id_hex: Option<String>,
    pub cell_id_hex: Option<String>,
    pub updated_at_ms: u64,
    pub expected_generation_or_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGroupSummaryDto {
    pub group_id_hex: String,
    pub task_id_hex: String,
    pub parent_group_id_hex: Option<String>,
    pub state: String,
    pub membership_generation: u64,
    pub state_seq: u64,
    pub depth: u32,
    pub cancel_epoch: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskNodeSummaryDto {
    pub plan_id_hex: String,
    pub node_id_hex: String,
    pub node_kind: String,
    pub state: String,
    pub declared_revision: u64,
    pub node_digest_hex: String,
    pub transition_count: u64,
    pub residency_tier: String,
    pub residency_transition_count: u64,
    pub first_declared_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAttemptSummaryDto {
    pub attempt_id_hex: String,
    pub task_id_hex: String,
    pub node_id_hex: Option<String>,
    pub state: String,
    pub isolation_domain_id_hex: String,
    pub provider_isolation_unverified: bool,
    pub cas_winner: bool,
    pub expected_generation_or_revision: u64,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionFiberSummaryDto {
    pub fiber_id_hex: String,
    pub generation: u64,
    pub state: String,
    pub lifecycle_phase: String,
    pub active_cpu_ms: u64,
    pub elapsed_wall_ms: u64,
    pub scheduler_wait_ms: u64,
    pub external_wait_ms: u64,
    pub backpressure_wait_ms: u64,
    pub suspended_ms: u64,
    pub process_id_hex: Option<String>,
    pub shared_thread_like: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableOperationSummaryDto {
    pub operation_id_hex: String,
    pub generation: u64,
    pub state: String,
    pub cancel_epoch: u64,
    pub owner_fiber_id_hex: String,
    pub owner_fiber_generation: u64,
    pub outcome_receipt_id_hex: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledApplicationSummaryDto {
    pub application_id_hex: String,
    pub installation_id_hex: String,
    pub package_id_hex: String,
    pub package_manifest_digest_hex: String,
    pub current_installation_generation: u64,
    pub status: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchCandidateDto {
    pub candidate_id_hex: String,
    pub object_id_hex: String,
    pub scope_id_hex: String,
    pub title_snippet: String,
    pub rank_score_milli: u32,
    pub non_authoritative_restrictions_bits: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopWatermarkEventDto {
    pub watermark: u64,
    pub domain_scope: String,
    pub entity_id_hex: String,
    pub new_generation_or_revision: u64,
    pub state_digest_hex: String,
    pub occurred_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsEntryDto {
    pub key: String,
    pub value_cbor_hex: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentTypeAssociationDto {
    pub association_id_hex: String,
    pub content_type: String,
    pub application_id_hex: String,
    pub expected_generation_or_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeychainReferenceDto {
    pub keychain_ref_id_hex: String,
    pub owner_principal_id_hex: String,
    pub label: String,
    pub public_fingerprint_hex: String,
    pub expected_generation_or_revision: u64,
    pub hardware_backed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChooserGrantDto {
    pub grant_id_hex: String,
    pub application_id_hex: String,
    pub chooser_kind: u32,
    pub resource_handle_hex: String,
    pub expected_generation_or_revision: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDto {
    pub workspace_id_hex: String,
    pub name: String,
    pub layout_preset: u32,
    pub active_surface_count: u32,
    pub focused_task_id_hex: Option<String>,
    pub expected_generation_or_revision: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSummaryDto {
    pub process_id_hex: String,
    pub expected_generation_or_revision: u64,
    pub agent_instance_id_hex: String,
    pub task_id_hex: String,
    pub task_attempt_id_hex: String,
    pub isolation_domain_id_hex: String,
    pub cell_id_hex: Option<String>,
    pub lifecycle_state: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceReservationSummaryDto {
    pub reservation_id_hex: String,
    pub account_id_hex: String,
    pub actual_usage: u64,
    pub effective_granted: u64,
    pub requested_ceiling: u64,
    pub context_residency_tier: String,
    pub expected_generation_or_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicSummaryDto {
    pub topic_id_hex: String,
    pub channel_id_hex: String,
    pub channel_generation: u64,
    pub name_hex: String,
    pub active_subscriptions: u64,
    pub policy_digest_hex: String,
    pub created_at_ms: u64,
    pub expected_generation_or_revision: u64,
    pub retained_messages: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationSummaryDto {
    pub notification_id_hex: String,
    pub topic_id_hex: String,
    pub subscriber_key_hex: String,
    pub next_sequence: u64,
    pub history_entry_count: u64,
    pub history_truncated: bool,
    pub prune_watermark_sequence: u64,
    pub expected_generation_or_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CellSummaryDto {
    pub cell_id_hex: String,
    pub cell_label: String,
    pub reachability: String,
    pub partition_autonomy_gate: String,
    pub current_epoch: u64,
    pub active_lease_count: u64,
    pub expected_generation_or_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CellLeaseDto {
    pub lease_id_hex: String,
    pub cell_id_hex: String,
    pub kind: String,
    pub lock_state: String,
    pub holder_id_hex: String,
    pub remaining_holder_ttl_ms: u64,
    pub lease_epoch: u64,
    pub clock_skew_horizon_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlBlastRadiusImpactDto {
    pub sign: String,
    pub affected_scope: String,
    pub affected_id_hex: String,
    pub current_generation_or_revision: u64,
    pub summary_code: String,
}

/// 一条派发命令的回执结果:只读巡检形态 + W32-B 写入形态 + Stage D 枚举/对称形态 + 类型化失败。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutcomeDto {
    Inspected {
        worker_state: String,
        completed_cycles: u64,
        durable_retrying: u64,
        durable_escalated: u64,
        durable_unacknowledged_escalated: u64,
        durable_resolved: u64,
        alerts: Vec<RecoveryAlertDto>,
    },
    SemanticInspected {
        total_inspected: u64,
        total_finalized: u64,
        consecutive_failed_cycles: u64,
        domain_faulted: bool,
        durable_retrying: u64,
        durable_escalated: u64,
        durable_unacknowledged_escalated: u64,
        durable_resolved: u64,
        alerts: Vec<RecoveryAlertDto>,
    },
    ResourceRecoveryInspected {
        total_inspected: u64,
        total_finalized: u64,
        consecutive_failed_cycles: u64,
        domain_faulted: bool,
        durable_retrying: u64,
        durable_escalated: u64,
        durable_unacknowledged_escalated: u64,
        durable_resolved: u64,
        alerts: Vec<RecoveryAlertDto>,
    },
    ProcessInspected {
        process_id_hex: String,
        process_generation: u64,
        agent_instance_id_hex: String,
        task_id_hex: String,
        task_attempt_id_hex: String,
        isolation_domain_id_hex: String,
    },
    ResourceInspected {
        reservation_id_hex: String,
        account_id_hex: String,
        upper_bound: u64,
        usage_high_water: u64,
        consumption_count: u32,
    },
    /// W38 Application 层 inspect 形态(desktop 穷尽匹配补齐;本壳派发面属
    /// 控制中心读,非本波次 Task Space 五层接线范围)。
    ApplicationInspected {
        package_id_hex: String,
        application_id_hex: String,
        package_manifest_digest_hex: String,
        current_installation_generation: u64,
        status: u8,
        created_at_ms: u64,
        updated_at_ms: u64,
    },
    /// W66 (`C-UNFREEZE-001`) Search 非权威查询观测形态(有界标量事实)。
    #[serde(rename_all = "camelCase")]
    SearchInspected {
        scope_id_hex: String,
        consistency_class: u8,
        degradation_policy: u8,
        non_authoritative_restrictions_bits: u8,
        admitted_event_count: u64,
        active_assertion_count: u64,
        retracted_event_count: u64,
        truncated: bool,
    },
    /// W66 (`C-UNFREEZE-001`) Notification 订阅与离线历史分页观测形态(有界标量事实)。
    #[serde(rename_all = "camelCase")]
    NotificationInspected {
        notification_id_hex: String,
        topic_id_hex: String,
        subscriber_key_hex: String,
        next_sequence: u64,
        history_entry_count: u64,
        history_truncated: bool,
        prune_watermark_sequence: u64,
    },
    /// W32-G(SABI v1.5)五层 inspect 形态:有界标量事实;成员行以计数 +
    /// 截断标志呈现,不逐行展开。W39-D 接通 desktop 派发。
    TaskGroupInspected {
        group_id_hex: String,
        task_id_hex: String,
        parent_group_id_hex: Option<String>,
        state: String,
        membership_generation: u64,
        state_seq: u64,
        depth: u64,
        cancel_epoch: u64,
        created_at_ms: i64,
        updated_at_ms: i64,
        member_count: u64,
        members_truncated: bool,
    },
    TaskNodeInspected {
        plan_id_hex: String,
        node_id_hex: String,
        node_kind: String,
        state: String,
        declared_revision: u64,
        node_digest_hex: String,
        transition_count: u64,
        residency_tier: String,
        residency_transition_count: u64,
        first_declared_at_ms: u64,
        updated_at_ms: u64,
    },
    ExecutionFiberInspected {
        fiber_id_hex: String,
        generation: u64,
        state: String,
        lifecycle_phase: String,
        active_cpu_ms: u64,
        elapsed_wall_ms: u64,
        scheduler_wait_ms: u64,
        external_wait_ms: u64,
        backpressure_wait_ms: u64,
        suspended_ms: u64,
    },
    TopicInspected {
        topic_id_hex: String,
        channel_id_hex: String,
        channel_generation: u64,
        name_hex: String,
        active_subscriptions: u64,
        policy_digest_hex: String,
        created_at_ms: u64,
    },
    DurableOperationInspected {
        operation_id_hex: String,
        generation: u64,
        state: String,
        cancel_epoch: u64,
        owner_fiber_id_hex: String,
        owner_fiber_generation: u64,
        outcome_receipt_id_hex: Option<String>,
    },
    MetricsExported {
        openmetrics_text: String,
    },
    Failure {
        code: String,
        retry: String,
        safe_message: String,
    },
    /// W32-B 写入半:mutation 回执的第一类形态——各携带权威 receipt 引用
    /// (§24.3),由「控制动作」视图按动作类型化渲染。
    Acknowledged {
        receipt_id_hex: String,
    },
    Resumed {
        receipt_id_hex: String,
    },
    OperationPaused {
        receipt_id_hex: String,
    },
    OperationResumed {
        receipt_id_hex: String,
    },
    OperationCancelled {
        receipt_id_hex: String,
    },
    OperationKilled {
        receipt_id_hex: String,
    },
    OperationThrottled {
        receipt_id_hex: String,
    },
    OperationReclaimed {
        receipt_id_hex: String,
    },
    ApplicationDisabled {
        receipt_id_hex: String,
    },
    ApplicationUninstalled {
        receipt_id_hex: String,
    },
    /// Stage D (`IPC-D3` / `IPC-D6`) 显式代际失配投影(与 `NOT_FOUND` 严格区分)。
    #[serde(rename_all = "camelCase")]
    StaleGeneration {
        view: String,
        target_id_hex: Option<String>,
        expected_generation: u64,
        current_generation: u64,
    },
    #[serde(rename_all = "camelCase")]
    TasksListed {
        tasks: Vec<TaskSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    TaskGroupsListed {
        groups: Vec<TaskGroupSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    TaskNodesListed {
        plan_id_hex: String,
        current_plan_revision: u64,
        stale_generation: bool,
        nodes: Vec<TaskNodeSummaryDto>,
        metadata_only_collapsed_count: u64,
        cold_collapsed_count: u64,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    TaskAttemptsListed {
        current_generation: u64,
        stale_generation: bool,
        attempts: Vec<TaskAttemptSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    ExecutionFibersListed {
        current_generation: u64,
        stale_generation: bool,
        fibers: Vec<ExecutionFiberSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    OperationsListed {
        current_generation: u64,
        stale_generation: bool,
        operations: Vec<DurableOperationSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    InstalledApplicationsListed {
        applications: Vec<InstalledApplicationSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    SearchCandidatesQueried {
        scope_id_hex: String,
        non_authoritative_restrictions_bits: u32,
        achieved_consistency: u32,
        failed_shards: Vec<u32>,
        candidates: Vec<SearchCandidateDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    DesktopWatermarkPolled {
        current_watermark: u64,
        events: Vec<DesktopWatermarkEventDto>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    SettingsDomainInspected {
        domain_kind: String,
        scope_target_id_hex: Option<String>,
        domain_revision: u64,
        entries: Vec<SettingsEntryDto>,
    },
    #[serde(rename_all = "camelCase")]
    ContentTypeAssociationsListed {
        associations: Vec<ContentTypeAssociationDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    KeychainReferencesListed {
        references: Vec<KeychainReferenceDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    ChooserGrantsListed {
        grants: Vec<ChooserGrantDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    WorkspacesListed {
        workspaces: Vec<WorkspaceDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    WorkspaceInspected {
        workspace_id_hex: String,
        name: String,
        layout_preset: u32,
        active_surface_count: u32,
        focused_task_id_hex: Option<String>,
        expected_generation_or_revision: u64,
        updated_at_ms: u64,
    },
    #[serde(rename_all = "camelCase")]
    ProcessesListed {
        processes: Vec<ProcessSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    ResourceReservationsListed {
        reservations: Vec<ResourceReservationSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    TopicsListed {
        topics: Vec<TopicSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    NotificationsListed {
        notifications: Vec<NotificationSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    CellsListed {
        cells: Vec<CellSummaryDto>,
        next_cursor_hex: Option<String>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    CellLeasesInspected {
        cell_id_hex: String,
        leases: Vec<CellLeaseDto>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    BlastRadiusPreviewed {
        target_scope: String,
        target_id_hex: String,
        preview_action: u32,
        statutory_lock_blocked: bool,
        statutory_lock_code: String,
        impacts: Vec<ControlBlastRadiusImpactDto>,
        truncated: bool,
    },
    #[serde(rename_all = "camelCase")]
    CommandStatusQueried {
        control_command_id_hex: String,
        state: String,
        scope: String,
        target_id_hex: String,
        receipt_id_hex: Option<String>,
        failure_code: Option<String>,
        failure_retry: Option<String>,
        failure_safe_message: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ProcessMigrationRequested {
        receipt_id_hex: String,
    },
    #[serde(rename_all = "camelCase")]
    TopicPurged {
        receipt_id_hex: String,
    },
    #[serde(rename_all = "camelCase")]
    SettingsDomainUpdated {
        receipt_id_hex: String,
    },
    #[serde(rename_all = "camelCase")]
    ContentTypeAssociationSet {
        receipt_id_hex: String,
    },
    #[serde(rename_all = "camelCase")]
    ChooserGrantIssued {
        receipt_id_hex: String,
    },
    #[serde(rename_all = "camelCase")]
    ReceiptUndone {
        receipt_id_hex: String,
    },
}

/// 一条派发命令的 SABI Receipt 投影。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptDto {
    pub control_command_id_hex: String,
    pub correlation_id_hex: String,
    pub receipt_hex: String,
    pub outcome: OutcomeDto,
}

/// 会话连接配置(环境变量或 GUI 会话内设置)。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDto {
    pub socket_path: Option<String>,
    pub principal_hex: Option<String>,
    pub key_file: Option<String>,
    pub cli_socket: Option<String>,
    pub cli_path: Option<String>,
    pub resource_root: Option<String>,
    /// W32-F:本地应用权威根目录(UI Surface 呈现接线;null=未接线)。
    pub application_root: Option<String>,
    pub source: ConfigSourceDto,
    pub platform_supported: bool,
}

/// W32-D 控制面授权事实:客户端路径常量(非 inspect 数据)。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlPlaneFactsDto {
    pub service: String,
    pub capability_slot: u64,
    pub capability_generation: u64,
}

/// W32-D 一致性自检的一行事实比对(渲染值 vs 复检值)。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactRowDto {
    pub field: String,
    pub first: String,
    pub second: String,
    pub matched: bool,
}

/// W32-D 一致性自检结果:同一 reservation 两次独立认证派发的有界成本
/// 事实逐字段比对 + receipt hex 比对。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactCheckDto {
    pub reservation_id_hex: String,
    pub matched: bool,
    pub receipt_hex_matched: bool,
    pub first_receipt_hex: String,
    pub second_receipt_hex: String,
    pub rows: Vec<FactRowDto>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSourceDto {
    Env,
    Session,
    #[default]
    Unset,
}

/// 一致性自检结果:GUI(认证入口)与真实 CLI(plain 入口)的 receipt hex 比对。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParityDto {
    pub operation: String,
    pub gui_receipt_hex: String,
    pub cli_receipt_hex: Option<String>,
    pub cli_exit_code: Option<i32>,
    pub matched: bool,
    pub cli_stderr: Option<String>,
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn opt_hex(bytes: Option<[u8; 16]>) -> Option<String> {
    bytes.map(|raw| hex(&raw))
}

fn enum_name(pending: i32) -> String {
    // 数值未知时保留数值本身,绝不静默丢失错误面。
    SabiErrorCode::try_from(pending)
        .map(|code| code.as_str_name().to_owned())
        .unwrap_or_else(|_| format!("UNKNOWN({pending})"))
}

fn retry_name(pending: i32) -> String {
    RetryDirective::try_from(pending)
        .map(|retry| retry.as_str_name().to_owned())
        .unwrap_or_else(|_| format!("UNKNOWN({pending})"))
}

fn failure_dto(failure: &SabiFailure) -> OutcomeDto {
    OutcomeDto::Failure {
        code: enum_name(failure.code),
        retry: retry_name(failure.retry),
        safe_message: failure.safe_message.clone(),
    }
}

fn artifact_alerts(inspection: &RecoveryInspection) -> Vec<RecoveryAlertDto> {
    inspection
        .alerts
        .iter()
        .map(|alert| RecoveryAlertDto {
            plan_id_hex: hex(&alert.plan_id),
            total_failures: alert.total_failures,
            acknowledged_receipt_id_hex: alert.acknowledged_receipt_id.clone().map(|id| hex(&id)),
        })
        .collect()
}

fn semantic_alerts(inspection: &SemanticRecoveryInspection) -> Vec<RecoveryAlertDto> {
    inspection
        .alerts
        .iter()
        .map(|alert| RecoveryAlertDto {
            plan_id_hex: hex(&alert.plan_id),
            total_failures: alert.total_failures,
            acknowledged_receipt_id_hex: alert.acknowledged_receipt_id.clone().map(|id| hex(&id)),
        })
        .collect()
}

fn resource_recovery_alerts(inspection: &ResourceRecoveryInspection) -> Vec<RecoveryAlertDto> {
    inspection
        .alerts
        .iter()
        .map(|alert| RecoveryAlertDto {
            plan_id_hex: hex(&alert.plan_id),
            total_failures: alert.total_failures,
            acknowledged_receipt_id_hex: alert.acknowledged_receipt_id.clone().map(|id| hex(&id)),
        })
        .collect()
}

fn resource_recovery_inspected_dto(inspection: &ResourceRecoveryInspection) -> OutcomeDto {
    OutcomeDto::ResourceRecoveryInspected {
        total_inspected: inspection.total_inspected,
        total_finalized: inspection.total_finalized,
        consecutive_failed_cycles: inspection.consecutive_failed_cycles,
        domain_faulted: inspection.domain_faulted,
        durable_retrying: inspection.durable_retrying,
        durable_escalated: inspection.durable_escalated,
        durable_unacknowledged_escalated: inspection.durable_unacknowledged_escalated,
        durable_resolved: inspection.durable_resolved,
        alerts: resource_recovery_alerts(inspection),
    }
}

fn inspected_dto(inspection: &RecoveryInspection) -> OutcomeDto {
    OutcomeDto::Inspected {
        worker_state: format!("{:?}", inspection.worker_state),
        completed_cycles: inspection.completed_cycles,
        durable_retrying: inspection.durable_retrying,
        durable_escalated: inspection.durable_escalated,
        durable_unacknowledged_escalated: inspection.durable_unacknowledged_escalated,
        durable_resolved: inspection.durable_resolved,
        alerts: artifact_alerts(inspection),
    }
}

fn semantic_inspected_dto(inspection: &SemanticRecoveryInspection) -> OutcomeDto {
    OutcomeDto::SemanticInspected {
        total_inspected: inspection.total_inspected,
        total_finalized: inspection.total_finalized,
        consecutive_failed_cycles: inspection.consecutive_failed_cycles,
        domain_faulted: inspection.domain_faulted,
        durable_retrying: inspection.durable_retrying,
        durable_escalated: inspection.durable_escalated,
        durable_unacknowledged_escalated: inspection.durable_unacknowledged_escalated,
        durable_resolved: inspection.durable_resolved,
        alerts: semantic_alerts(inspection),
    }
}

fn process_inspected_dto(inspection: &ProcessInspection) -> OutcomeDto {
    OutcomeDto::ProcessInspected {
        process_id_hex: hex(&inspection.process_id),
        process_generation: inspection.process_generation,
        agent_instance_id_hex: hex(&inspection.agent_instance_id),
        task_id_hex: hex(&inspection.task_id),
        task_attempt_id_hex: hex(&inspection.task_attempt_id),
        isolation_domain_id_hex: hex(&inspection.isolation_domain_id),
    }
}

fn resource_inspected_dto(inspection: &ResourceInspection) -> OutcomeDto {
    OutcomeDto::ResourceInspected {
        reservation_id_hex: hex(&inspection.reservation_id),
        account_id_hex: hex(&inspection.account_id),
        upper_bound: inspection.upper_bound,
        usage_high_water: inspection.usage_high_water,
        consumption_count: inspection.consumption_count,
    }
}

fn application_inspected_dto(inspection: &ApplicationInspection) -> OutcomeDto {
    OutcomeDto::ApplicationInspected {
        package_id_hex: hex(&inspection.package_id),
        application_id_hex: hex(&inspection.application_id),
        package_manifest_digest_hex: hex(&inspection.package_manifest_digest),
        current_installation_generation: inspection.current_installation_generation,
        status: inspection.status,
        created_at_ms: inspection.created_at_ms,
        updated_at_ms: inspection.updated_at_ms,
    }
}

fn search_inspected_dto(inspection: &SearchInspection) -> OutcomeDto {
    OutcomeDto::SearchInspected {
        scope_id_hex: hex(&inspection.scope_id),
        consistency_class: inspection.consistency_class,
        degradation_policy: inspection.degradation_policy,
        non_authoritative_restrictions_bits: inspection.non_authoritative_restrictions_bits,
        admitted_event_count: inspection.admitted_event_count,
        active_assertion_count: inspection.active_assertion_count,
        retracted_event_count: inspection.retracted_event_count,
        truncated: inspection.truncated,
    }
}

fn notification_inspected_dto(inspection: &NotificationInspection) -> OutcomeDto {
    OutcomeDto::NotificationInspected {
        notification_id_hex: hex(&inspection.notification_id),
        topic_id_hex: hex(&inspection.topic_id),
        subscriber_key_hex: hex(&inspection.subscriber_key),
        next_sequence: inspection.next_sequence,
        history_entry_count: inspection.history_entry_count,
        history_truncated: inspection.history_truncated,
        prune_watermark_sequence: inspection.prune_watermark_sequence,
    }
}

fn task_group_inspected_dto(inspection: &TaskGroupInspection) -> OutcomeDto {
    OutcomeDto::TaskGroupInspected {
        group_id_hex: hex(&inspection.group_id),
        task_id_hex: hex(&inspection.task_id),
        parent_group_id_hex: inspection.parent_group_id.map(|id| hex(&id)),
        state: format!("{:?}", inspection.state),
        membership_generation: inspection.membership_generation,
        state_seq: inspection.state_seq,
        depth: inspection.depth,
        cancel_epoch: inspection.cancel_epoch,
        created_at_ms: inspection.created_at_ms,
        updated_at_ms: inspection.updated_at_ms,
        member_count: u64::try_from(inspection.members.len()).unwrap_or(u64::MAX),
        members_truncated: inspection.members_truncated,
    }
}

fn task_node_summary_dto(inspection: &TaskNodeInspection) -> TaskNodeSummaryDto {
    TaskNodeSummaryDto {
        plan_id_hex: hex(&inspection.plan_id),
        node_id_hex: hex(&inspection.node_id),
        node_kind: format!("{:?}", inspection.kind),
        state: format!("{:?}", inspection.state),
        declared_revision: inspection.declared_revision,
        node_digest_hex: hex(&inspection.node_digest),
        transition_count: inspection.transition_count,
        residency_tier: format!("{:?}", inspection.residency_tier),
        residency_transition_count: inspection.residency_transition_count,
        first_declared_at_ms: inspection.first_declared_at_ms,
        updated_at_ms: inspection.updated_at_ms,
    }
}

fn task_node_inspected_dto(inspection: &TaskNodeInspection) -> OutcomeDto {
    let summary = task_node_summary_dto(inspection);
    OutcomeDto::TaskNodeInspected {
        plan_id_hex: summary.plan_id_hex,
        node_id_hex: summary.node_id_hex,
        node_kind: summary.node_kind,
        state: summary.state,
        declared_revision: summary.declared_revision,
        node_digest_hex: summary.node_digest_hex,
        transition_count: summary.transition_count,
        residency_tier: summary.residency_tier,
        residency_transition_count: summary.residency_transition_count,
        first_declared_at_ms: summary.first_declared_at_ms,
        updated_at_ms: summary.updated_at_ms,
    }
}

fn execution_fiber_inspected_dto(inspection: &ExecutionFiberInspection) -> OutcomeDto {
    OutcomeDto::ExecutionFiberInspected {
        fiber_id_hex: hex(&inspection.fiber_id),
        generation: inspection.generation,
        state: format!("{:?}", inspection.state),
        lifecycle_phase: format!("{:?}", inspection.lifecycle_phase),
        active_cpu_ms: inspection.active_cpu_ms,
        elapsed_wall_ms: inspection.elapsed_wall_ms,
        scheduler_wait_ms: inspection.scheduler_wait_ms,
        external_wait_ms: inspection.external_wait_ms,
        backpressure_wait_ms: inspection.backpressure_wait_ms,
        suspended_ms: inspection.suspended_ms,
    }
}

fn topic_inspected_dto(inspection: &TopicInspection) -> OutcomeDto {
    OutcomeDto::TopicInspected {
        topic_id_hex: hex(&inspection.topic_id),
        channel_id_hex: hex(&inspection.channel_id),
        channel_generation: inspection.channel_generation,
        name_hex: hex(&inspection.name),
        active_subscriptions: inspection.active_subscriptions,
        policy_digest_hex: hex(&inspection.policy_digest),
        created_at_ms: inspection.created_at_ms,
    }
}

fn durable_operation_summary_dto(
    inspection: &DurableOperationInspection,
) -> DurableOperationSummaryDto {
    DurableOperationSummaryDto {
        operation_id_hex: hex(&inspection.operation_id),
        generation: inspection.generation,
        state: format!("{:?}", inspection.state),
        cancel_epoch: inspection.cancel_epoch,
        owner_fiber_id_hex: hex(&inspection.owner_fiber_id),
        owner_fiber_generation: inspection.owner_fiber_generation,
        outcome_receipt_id_hex: inspection.outcome_receipt_id.as_ref().map(|id| hex(id)),
    }
}

fn durable_operation_inspected_dto(inspection: &DurableOperationInspection) -> OutcomeDto {
    let summary = durable_operation_summary_dto(inspection);
    OutcomeDto::DurableOperationInspected {
        operation_id_hex: summary.operation_id_hex,
        generation: summary.generation,
        state: summary.state,
        cancel_epoch: summary.cancel_epoch,
        owner_fiber_id_hex: summary.owner_fiber_id_hex,
        owner_fiber_generation: summary.owner_fiber_generation,
        outcome_receipt_id_hex: summary.outcome_receipt_id_hex,
    }
}

fn tasks_listed_dto(page: &TaskListPage) -> OutcomeDto {
    OutcomeDto::TasksListed {
        tasks: page
            .tasks
            .iter()
            .map(|row: &TaskSummaryRecord| TaskSummaryDto {
                task_id_hex: hex(&row.task_id),
                plan_id_hex: hex(&row.plan_id),
                application_id_hex: opt_hex(row.application_id),
                installation_generation: row.installation_generation,
                plan_revision: row.plan_revision,
                task_state: row.task_state.as_str_name().to_owned(),
                undo_class: row.undo_class.as_str_name().to_owned(),
                reservation_id_hex: opt_hex(row.reservation_id),
                cell_id_hex: opt_hex(row.cell_id),
                updated_at_ms: row.updated_at_ms,
                expected_generation_or_revision: row.expected_generation_or_revision,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn task_groups_listed_dto(page: &TaskGroupListPage) -> OutcomeDto {
    OutcomeDto::TaskGroupsListed {
        groups: page
            .groups
            .iter()
            .map(|row: &TaskGroupSummaryRecord| TaskGroupSummaryDto {
                group_id_hex: hex(&row.group_id),
                task_id_hex: hex(&row.task_id),
                parent_group_id_hex: opt_hex(row.parent_group_id),
                state: row.state.as_str_name().to_owned(),
                membership_generation: row.membership_generation,
                state_seq: row.state_seq,
                depth: row.depth,
                cancel_epoch: row.cancel_epoch,
                created_at_ms: row.created_at_ms,
                updated_at_ms: row.updated_at_ms,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn task_nodes_listed_dto(page: &TaskNodeListPage) -> OutcomeDto {
    OutcomeDto::TaskNodesListed {
        plan_id_hex: hex(&page.plan_id),
        current_plan_revision: page.current_plan_revision,
        stale_generation: page.stale_generation,
        nodes: page.nodes.iter().map(task_node_summary_dto).collect(),
        metadata_only_collapsed_count: page.metadata_only_collapsed_count,
        cold_collapsed_count: page.cold_collapsed_count,
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn task_attempts_listed_dto(page: &TaskAttemptListPage) -> OutcomeDto {
    OutcomeDto::TaskAttemptsListed {
        current_generation: page.current_generation,
        stale_generation: page.stale_generation,
        attempts: page
            .attempts
            .iter()
            .map(|row: &TaskAttemptSummaryRecord| TaskAttemptSummaryDto {
                attempt_id_hex: hex(&row.attempt_id),
                task_id_hex: hex(&row.task_id),
                node_id_hex: opt_hex(row.node_id),
                state: row.state.as_str_name().to_owned(),
                isolation_domain_id_hex: hex(&row.isolation_domain_id),
                provider_isolation_unverified: row.provider_isolation_unverified,
                cas_winner: row.cas_winner,
                expected_generation_or_revision: row.expected_generation_or_revision,
                started_at_ms: row.started_at_ms,
                updated_at_ms: row.updated_at_ms,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn execution_fibers_listed_dto(page: &ExecutionFiberListPage) -> OutcomeDto {
    OutcomeDto::ExecutionFibersListed {
        current_generation: page.current_generation,
        stale_generation: page.stale_generation,
        fibers: page
            .fibers
            .iter()
            .map(
                |row: &ExecutionFiberSummaryRecord| ExecutionFiberSummaryDto {
                    fiber_id_hex: hex(&row.fiber.fiber_id),
                    generation: row.fiber.generation,
                    state: format!("{:?}", row.fiber.state),
                    lifecycle_phase: format!("{:?}", row.fiber.lifecycle_phase),
                    active_cpu_ms: row.fiber.active_cpu_ms,
                    elapsed_wall_ms: row.fiber.elapsed_wall_ms,
                    scheduler_wait_ms: row.fiber.scheduler_wait_ms,
                    external_wait_ms: row.fiber.external_wait_ms,
                    backpressure_wait_ms: row.fiber.backpressure_wait_ms,
                    suspended_ms: row.fiber.suspended_ms,
                    process_id_hex: opt_hex(row.process_id),
                    shared_thread_like: row.shared_thread_like,
                },
            )
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn operations_listed_dto(page: &DurableOperationListPage) -> OutcomeDto {
    OutcomeDto::OperationsListed {
        current_generation: page.current_generation,
        stale_generation: page.stale_generation,
        operations: page
            .operations
            .iter()
            .map(durable_operation_summary_dto)
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn installed_applications_listed_dto(page: &InstalledApplicationListPage) -> OutcomeDto {
    OutcomeDto::InstalledApplicationsListed {
        applications: page
            .applications
            .iter()
            .map(
                |row: &InstalledApplicationSummaryRecord| InstalledApplicationSummaryDto {
                    application_id_hex: hex(&row.application_id),
                    installation_id_hex: hex(&row.installation_id),
                    package_id_hex: hex(&row.package_id),
                    package_manifest_digest_hex: hex(&row.package_manifest_digest),
                    current_installation_generation: row.current_installation_generation,
                    status: row.status,
                    created_at_ms: row.created_at_ms,
                    updated_at_ms: row.updated_at_ms,
                },
            )
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn search_candidates_queried_dto(page: &SearchCandidateQueryPage) -> OutcomeDto {
    OutcomeDto::SearchCandidatesQueried {
        scope_id_hex: hex(&page.scope_id),
        non_authoritative_restrictions_bits: page.non_authoritative_restrictions_bits,
        achieved_consistency: page.achieved_consistency,
        failed_shards: page.failed_shards.clone(),
        candidates: page
            .candidates
            .iter()
            .map(|row: &SearchCandidateRecord| SearchCandidateDto {
                candidate_id_hex: hex(&row.candidate_id),
                object_id_hex: hex(&row.object_id),
                scope_id_hex: hex(&row.scope_id),
                title_snippet: row.title_snippet.clone(),
                rank_score_milli: row.rank_score_milli,
                non_authoritative_restrictions_bits: row.non_authoritative_restrictions_bits,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn desktop_watermark_polled_dto(page: &DesktopWatermarkPollPage) -> OutcomeDto {
    OutcomeDto::DesktopWatermarkPolled {
        current_watermark: page.current_watermark,
        events: page
            .events
            .iter()
            .map(
                |row: &DesktopWatermarkEventRecord| DesktopWatermarkEventDto {
                    watermark: row.watermark,
                    domain_scope: row.domain_scope.as_str_name().to_owned(),
                    entity_id_hex: hex(&row.entity_id),
                    new_generation_or_revision: row.new_generation_or_revision,
                    state_digest_hex: hex(&row.state_digest),
                    occurred_at_ms: row.occurred_at_ms,
                },
            )
            .collect(),
        truncated: page.truncated,
    }
}

fn settings_domain_inspected_dto(proj: &SettingsDomainProjection) -> OutcomeDto {
    OutcomeDto::SettingsDomainInspected {
        domain_kind: proj.domain_kind.as_str_name().to_owned(),
        scope_target_id_hex: opt_hex(proj.scope_target_id),
        domain_revision: proj.domain_revision,
        entries: proj
            .entries
            .iter()
            .map(|entry: &SettingsEntryRecord| SettingsEntryDto {
                key: entry.key.clone(),
                value_cbor_hex: hex(&entry.value_cbor),
                revision: entry.revision,
            })
            .collect(),
    }
}

fn content_type_associations_listed_dto(page: &ContentTypeAssociationListPage) -> OutcomeDto {
    OutcomeDto::ContentTypeAssociationsListed {
        associations: page
            .associations
            .iter()
            .map(
                |row: &ContentTypeAssociationRecord| ContentTypeAssociationDto {
                    association_id_hex: hex(&row.association_id),
                    content_type: row.content_type.clone(),
                    application_id_hex: hex(&row.application_id),
                    expected_generation_or_revision: row.expected_generation_or_revision,
                },
            )
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn keychain_references_listed_dto(page: &KeychainReferenceListPage) -> OutcomeDto {
    OutcomeDto::KeychainReferencesListed {
        references: page
            .references
            .iter()
            .map(|row: &KeychainReferenceRecord| KeychainReferenceDto {
                keychain_ref_id_hex: hex(&row.keychain_ref_id),
                owner_principal_id_hex: hex(&row.owner_principal_id),
                label: row.label.clone(),
                public_fingerprint_hex: hex(&row.public_fingerprint),
                expected_generation_or_revision: row.expected_generation_or_revision,
                hardware_backed: row.hardware_backed,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn chooser_grants_listed_dto(page: &ChooserGrantListPage) -> OutcomeDto {
    OutcomeDto::ChooserGrantsListed {
        grants: page
            .grants
            .iter()
            .map(|row: &ChooserGrantRecord| ChooserGrantDto {
                grant_id_hex: hex(&row.grant_id),
                application_id_hex: hex(&row.application_id),
                chooser_kind: row.chooser_kind,
                resource_handle_hex: hex(&row.resource_handle),
                expected_generation_or_revision: row.expected_generation_or_revision,
                expires_at_ms: row.expires_at_ms,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn workspace_dto(row: &WorkspaceRecord) -> WorkspaceDto {
    WorkspaceDto {
        workspace_id_hex: hex(&row.workspace_id),
        name: row.name.clone(),
        layout_preset: row.layout_preset,
        active_surface_count: row.active_surface_count,
        focused_task_id_hex: opt_hex(row.focused_task_id),
        expected_generation_or_revision: row.expected_generation_or_revision,
        updated_at_ms: row.updated_at_ms,
    }
}

fn workspaces_listed_dto(page: &WorkspaceListPage) -> OutcomeDto {
    OutcomeDto::WorkspacesListed {
        workspaces: page.workspaces.iter().map(workspace_dto).collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn workspace_inspected_dto(row: &WorkspaceRecord) -> OutcomeDto {
    let dto = workspace_dto(row);
    OutcomeDto::WorkspaceInspected {
        workspace_id_hex: dto.workspace_id_hex,
        name: dto.name,
        layout_preset: dto.layout_preset,
        active_surface_count: dto.active_surface_count,
        focused_task_id_hex: dto.focused_task_id_hex,
        expected_generation_or_revision: dto.expected_generation_or_revision,
        updated_at_ms: dto.updated_at_ms,
    }
}

fn processes_listed_dto(page: &ProcessListPage) -> OutcomeDto {
    OutcomeDto::ProcessesListed {
        processes: page
            .processes
            .iter()
            .map(|row: &ProcessSummaryRecord| ProcessSummaryDto {
                process_id_hex: hex(&row.process_id),
                expected_generation_or_revision: row.expected_generation_or_revision,
                agent_instance_id_hex: hex(&row.agent_instance_id),
                task_id_hex: hex(&row.task_id),
                task_attempt_id_hex: hex(&row.task_attempt_id),
                isolation_domain_id_hex: hex(&row.isolation_domain_id),
                cell_id_hex: opt_hex(row.cell_id),
                lifecycle_state: row.lifecycle_state,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn resource_reservations_listed_dto(page: &ResourceReservationListPage) -> OutcomeDto {
    OutcomeDto::ResourceReservationsListed {
        reservations: page
            .reservations
            .iter()
            .map(
                |row: &ResourceReservationSummaryRecord| ResourceReservationSummaryDto {
                    reservation_id_hex: hex(&row.reservation_id),
                    account_id_hex: hex(&row.account_id),
                    actual_usage: row.actual_usage,
                    effective_granted: row.effective_granted,
                    requested_ceiling: row.requested_ceiling,
                    context_residency_tier: row.context_residency_tier.as_str_name().to_owned(),
                    expected_generation_or_revision: row.expected_generation_or_revision,
                },
            )
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn topics_listed_dto(page: &TopicListPage) -> OutcomeDto {
    OutcomeDto::TopicsListed {
        topics: page
            .topics
            .iter()
            .map(|row: &TopicSummaryRecord| TopicSummaryDto {
                topic_id_hex: hex(&row.topic.topic_id),
                channel_id_hex: hex(&row.topic.channel_id),
                channel_generation: row.topic.channel_generation,
                name_hex: hex(&row.topic.name),
                active_subscriptions: row.topic.active_subscriptions,
                policy_digest_hex: hex(&row.topic.policy_digest),
                created_at_ms: row.topic.created_at_ms,
                expected_generation_or_revision: row.expected_generation_or_revision,
                retained_messages: row.retained_messages,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn notifications_listed_dto(page: &NotificationListPage) -> OutcomeDto {
    OutcomeDto::NotificationsListed {
        notifications: page
            .notifications
            .iter()
            .map(|row: &NotificationSummaryRecord| NotificationSummaryDto {
                notification_id_hex: hex(&row.notification.notification_id),
                topic_id_hex: hex(&row.notification.topic_id),
                subscriber_key_hex: hex(&row.notification.subscriber_key),
                next_sequence: row.notification.next_sequence,
                history_entry_count: row.notification.history_entry_count,
                history_truncated: row.notification.history_truncated,
                prune_watermark_sequence: row.notification.prune_watermark_sequence,
                expected_generation_or_revision: row.expected_generation_or_revision,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn cells_listed_dto(page: &CellListPage) -> OutcomeDto {
    OutcomeDto::CellsListed {
        cells: page
            .cells
            .iter()
            .map(|row: &CellSummaryRecord| CellSummaryDto {
                cell_id_hex: hex(&row.cell_id),
                cell_label: row.cell_label.clone(),
                reachability: row.reachability.as_str_name().to_owned(),
                partition_autonomy_gate: row.partition_autonomy_gate.as_str_name().to_owned(),
                current_epoch: row.current_epoch,
                active_lease_count: row.active_lease_count,
                expected_generation_or_revision: row.expected_generation_or_revision,
            })
            .collect(),
        next_cursor_hex: opt_hex(page.next_cursor),
        truncated: page.truncated,
    }
}

fn cell_leases_inspected_dto(page: &CellLeaseInspectPage) -> OutcomeDto {
    OutcomeDto::CellLeasesInspected {
        cell_id_hex: hex(&page.cell_id),
        leases: page
            .leases
            .iter()
            .map(|row: &CellLeaseRecord| CellLeaseDto {
                lease_id_hex: hex(&row.lease_id),
                cell_id_hex: hex(&row.cell_id),
                kind: row.kind.as_str_name().to_owned(),
                lock_state: row.lock_state.as_str_name().to_owned(),
                holder_id_hex: hex(&row.holder_id),
                remaining_holder_ttl_ms: row.remaining_holder_ttl_ms,
                lease_epoch: row.lease_epoch,
                clock_skew_horizon_ms: row.clock_skew_horizon_ms,
            })
            .collect(),
        truncated: page.truncated,
    }
}

fn blast_radius_previewed_dto(result: &ControlBlastRadiusPreviewResult) -> OutcomeDto {
    OutcomeDto::BlastRadiusPreviewed {
        target_scope: result.target_scope.as_str_name().to_owned(),
        target_id_hex: hex(&result.target_id),
        preview_action: result.preview_action,
        statutory_lock_blocked: result.statutory_lock_blocked,
        statutory_lock_code: result.statutory_lock_code.clone(),
        impacts: result
            .impacts
            .iter()
            .map(
                |row: &ControlBlastRadiusImpactRecord| ControlBlastRadiusImpactDto {
                    sign: row.sign.as_str_name().to_owned(),
                    affected_scope: row.affected_scope.as_str_name().to_owned(),
                    affected_id_hex: hex(&row.affected_id),
                    current_generation_or_revision: row.current_generation_or_revision,
                    summary_code: row.summary_code.clone(),
                },
            )
            .collect(),
        truncated: result.truncated,
    }
}

fn command_status_queried_dto(record: &ControlCommandStatusRecord) -> OutcomeDto {
    let (failure_code, failure_retry, failure_safe_message) = match record.failure.as_ref() {
        Some(failure) => (
            Some(enum_name(failure.code)),
            Some(retry_name(failure.retry)),
            Some(failure.safe_message.clone()),
        ),
        None => (None, None, None),
    };
    OutcomeDto::CommandStatusQueried {
        control_command_id_hex: hex(&record.control_command_id),
        state: record.state.as_str_name().to_owned(),
        scope: record.scope.as_str_name().to_owned(),
        target_id_hex: hex(&record.target_id),
        receipt_id_hex: opt_hex(record.receipt_id),
        failure_code,
        failure_retry,
        failure_safe_message,
    }
}

/// [`ControlReceipt`] → [`ReceiptDto`] 的单一投影点(穷尽匹配,无通配臂:
/// 新的 outcome 变体必须落成显式 DTO 形态,不得静默丢进失败)。
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn receipt_dto(receipt: &ControlReceipt) -> ReceiptDto {
    let outcome = match receipt.outcome.as_ref() {
        Ok(ControlOutcome::Inspected(inspection)) => inspected_dto(inspection),
        Ok(ControlOutcome::SemanticInspected(inspection)) => semantic_inspected_dto(inspection),
        Ok(ControlOutcome::ResourceRecoveryInspected(inspection)) => {
            resource_recovery_inspected_dto(inspection)
        }
        Ok(ControlOutcome::ProcessInspected(inspection)) => process_inspected_dto(inspection),
        Ok(ControlOutcome::ResourceInspected(inspection)) => resource_inspected_dto(inspection),
        Ok(ControlOutcome::ApplicationInspected(inspection)) => {
            application_inspected_dto(inspection)
        }
        Ok(ControlOutcome::SearchInspected(inspection)) => search_inspected_dto(inspection),
        Ok(ControlOutcome::NotificationInspected(inspection)) => {
            notification_inspected_dto(inspection)
        }
        Ok(ControlOutcome::TaskGroupInspected(inspection)) => task_group_inspected_dto(inspection),
        Ok(ControlOutcome::TaskNodeInspected(inspection)) => task_node_inspected_dto(inspection),
        Ok(ControlOutcome::ExecutionFiberInspected(inspection)) => {
            execution_fiber_inspected_dto(inspection)
        }
        Ok(ControlOutcome::TopicInspected(inspection)) => topic_inspected_dto(inspection),
        Ok(ControlOutcome::DurableOperationInspected(inspection)) => {
            durable_operation_inspected_dto(inspection)
        }
        Ok(ControlOutcome::MetricsExported(export)) => OutcomeDto::MetricsExported {
            openmetrics_text: export.openmetrics_text.clone(),
        },
        Ok(ControlOutcome::Acknowledged { receipt_id }) => OutcomeDto::Acknowledged {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::Resumed { receipt_id }) => OutcomeDto::Resumed {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationPaused { receipt_id }) => OutcomeDto::OperationPaused {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationResumed { receipt_id }) => OutcomeDto::OperationResumed {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationCancelled { receipt_id }) => OutcomeDto::OperationCancelled {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationKilled { receipt_id }) => OutcomeDto::OperationKilled {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationThrottled { receipt_id }) => OutcomeDto::OperationThrottled {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationReclaimed { receipt_id }) => OutcomeDto::OperationReclaimed {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::ApplicationDisabled { receipt_id }) => OutcomeDto::ApplicationDisabled {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::ApplicationUninstalled { receipt_id }) => {
            OutcomeDto::ApplicationUninstalled {
                receipt_id_hex: hex(receipt_id),
            }
        }
        Ok(ControlOutcome::StaleGeneration {
            view,
            target_id,
            expected_generation,
            current_generation,
        }) => OutcomeDto::StaleGeneration {
            view: view.as_str_name().to_owned(),
            target_id_hex: opt_hex(*target_id),
            expected_generation: *expected_generation,
            current_generation: *current_generation,
        },
        Ok(ControlOutcome::TasksListed(page)) => tasks_listed_dto(page),
        Ok(ControlOutcome::TaskGroupsListed(page)) => task_groups_listed_dto(page),
        Ok(ControlOutcome::TaskNodesListed(page)) => task_nodes_listed_dto(page),
        Ok(ControlOutcome::TaskAttemptsListed(page)) => task_attempts_listed_dto(page),
        Ok(ControlOutcome::ExecutionFibersListed(page)) => execution_fibers_listed_dto(page),
        Ok(ControlOutcome::OperationsListed(page)) => operations_listed_dto(page),
        Ok(ControlOutcome::InstalledApplicationsListed(page)) => {
            installed_applications_listed_dto(page)
        }
        Ok(ControlOutcome::SearchCandidatesQueried(page)) => search_candidates_queried_dto(page),
        Ok(ControlOutcome::DesktopWatermarkPolled(page)) => desktop_watermark_polled_dto(page),
        Ok(ControlOutcome::SettingsDomainInspected(proj)) => settings_domain_inspected_dto(proj),
        Ok(ControlOutcome::ContentTypeAssociationsListed(page)) => {
            content_type_associations_listed_dto(page)
        }
        Ok(ControlOutcome::KeychainReferencesListed(page)) => keychain_references_listed_dto(page),
        Ok(ControlOutcome::ChooserGrantsListed(page)) => chooser_grants_listed_dto(page),
        Ok(ControlOutcome::WorkspacesListed(page)) => workspaces_listed_dto(page),
        Ok(ControlOutcome::WorkspaceInspected(row)) => workspace_inspected_dto(row),
        Ok(ControlOutcome::ProcessesListed(page)) => processes_listed_dto(page),
        Ok(ControlOutcome::ResourceReservationsListed(page)) => {
            resource_reservations_listed_dto(page)
        }
        Ok(ControlOutcome::TopicsListed(page)) => topics_listed_dto(page),
        Ok(ControlOutcome::NotificationsListed(page)) => notifications_listed_dto(page),
        Ok(ControlOutcome::CellsListed(page)) => cells_listed_dto(page),
        Ok(ControlOutcome::CellLeasesInspected(page)) => cell_leases_inspected_dto(page),
        Ok(ControlOutcome::BlastRadiusPreviewed(preview)) => blast_radius_previewed_dto(preview),
        Ok(ControlOutcome::CommandStatusQueried(status)) => command_status_queried_dto(status),
        Ok(ControlOutcome::ProcessMigrationRequested { receipt_id }) => {
            OutcomeDto::ProcessMigrationRequested {
                receipt_id_hex: hex(receipt_id),
            }
        }
        Ok(ControlOutcome::TopicPurged { receipt_id }) => OutcomeDto::TopicPurged {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::SettingsDomainUpdated { receipt_id }) => {
            OutcomeDto::SettingsDomainUpdated {
                receipt_id_hex: hex(receipt_id),
            }
        }
        Ok(ControlOutcome::ContentTypeAssociationSet { receipt_id }) => {
            OutcomeDto::ContentTypeAssociationSet {
                receipt_id_hex: hex(receipt_id),
            }
        }
        Ok(ControlOutcome::ChooserGrantIssued { receipt_id }) => OutcomeDto::ChooserGrantIssued {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::ReceiptUndone { receipt_id }) => OutcomeDto::ReceiptUndone {
            receipt_id_hex: hex(receipt_id),
        },
        Err(failure) => failure_dto(failure),
    };
    ReceiptDto {
        control_command_id_hex: hex(&receipt.control_command_id),
        correlation_id_hex: hex(&receipt.correlation_id),
        receipt_hex: receipt_to_hex(receipt),
        outcome,
    }
}

fn fact_row(field: &str, first: &str, second: &str) -> FactRowDto {
    FactRowDto {
        field: field.to_owned(),
        first: first.to_owned(),
        second: second.to_owned(),
        matched: first == second,
    }
}

/// 非 ResourceInspected 形态(如未接线的类型化失败)的一行摘要标签:
/// 保留失败码,绝不把失败伪装成事实。
#[allow(clippy::too_many_lines)]
fn outcome_label(outcome: &OutcomeDto) -> String {
    match outcome {
        OutcomeDto::ResourceInspected { .. } => "resource_inspected".to_owned(),
        OutcomeDto::Failure { code, .. } => format!("failure/{code}"),
        OutcomeDto::Inspected { .. } => "inspected".to_owned(),
        OutcomeDto::SemanticInspected { .. } => "semantic_inspected".to_owned(),
        OutcomeDto::ResourceRecoveryInspected { .. } => "resource_recovery_inspected".to_owned(),
        OutcomeDto::ProcessInspected { .. } => "process_inspected".to_owned(),
        OutcomeDto::ApplicationInspected { .. } => "application_inspected".to_owned(),
        OutcomeDto::SearchInspected { .. } => "search_inspected".to_owned(),
        OutcomeDto::NotificationInspected { .. } => "notification_inspected".to_owned(),
        OutcomeDto::TaskGroupInspected { .. } => "task_group_inspected".to_owned(),
        OutcomeDto::TaskNodeInspected { .. } => "task_node_inspected".to_owned(),
        OutcomeDto::ExecutionFiberInspected { .. } => "execution_fiber_inspected".to_owned(),
        OutcomeDto::TopicInspected { .. } => "topic_inspected".to_owned(),
        OutcomeDto::DurableOperationInspected { .. } => "durable_operation_inspected".to_owned(),
        OutcomeDto::MetricsExported { .. } => "metrics_exported".to_owned(),
        OutcomeDto::Acknowledged { .. } => "acknowledged".to_owned(),
        OutcomeDto::Resumed { .. } => "resumed".to_owned(),
        OutcomeDto::OperationPaused { .. } => "operation_paused".to_owned(),
        OutcomeDto::OperationResumed { .. } => "operation_resumed".to_owned(),
        OutcomeDto::OperationCancelled { .. } => "operation_cancelled".to_owned(),
        OutcomeDto::OperationKilled { .. } => "operation_killed".to_owned(),
        OutcomeDto::OperationThrottled { .. } => "operation_throttled".to_owned(),
        OutcomeDto::OperationReclaimed { .. } => "operation_reclaimed".to_owned(),
        OutcomeDto::ApplicationDisabled { .. } => "application_disabled".to_owned(),
        OutcomeDto::ApplicationUninstalled { .. } => "application_uninstalled".to_owned(),
        OutcomeDto::StaleGeneration {
            current_generation, ..
        } => {
            format!("stale_generation/{current_generation}")
        }
        OutcomeDto::TasksListed { .. } => "tasks_listed".to_owned(),
        OutcomeDto::TaskGroupsListed { .. } => "task_groups_listed".to_owned(),
        OutcomeDto::TaskNodesListed { .. } => "task_nodes_listed".to_owned(),
        OutcomeDto::TaskAttemptsListed { .. } => "task_attempts_listed".to_owned(),
        OutcomeDto::ExecutionFibersListed { .. } => "execution_fibers_listed".to_owned(),
        OutcomeDto::OperationsListed { .. } => "operations_listed".to_owned(),
        OutcomeDto::InstalledApplicationsListed { .. } => {
            "installed_applications_listed".to_owned()
        }
        OutcomeDto::SearchCandidatesQueried { .. } => "search_candidates_queried".to_owned(),
        OutcomeDto::DesktopWatermarkPolled { .. } => "desktop_watermark_polled".to_owned(),
        OutcomeDto::SettingsDomainInspected { .. } => "settings_domain_inspected".to_owned(),
        OutcomeDto::ContentTypeAssociationsListed { .. } => {
            "content_type_associations_listed".to_owned()
        }
        OutcomeDto::KeychainReferencesListed { .. } => "keychain_references_listed".to_owned(),
        OutcomeDto::ChooserGrantsListed { .. } => "chooser_grants_listed".to_owned(),
        OutcomeDto::WorkspacesListed { .. } => "workspaces_listed".to_owned(),
        OutcomeDto::WorkspaceInspected { .. } => "workspace_inspected".to_owned(),
        OutcomeDto::ProcessesListed { .. } => "processes_listed".to_owned(),
        OutcomeDto::ResourceReservationsListed { .. } => "resource_reservations_listed".to_owned(),
        OutcomeDto::TopicsListed { .. } => "topics_listed".to_owned(),
        OutcomeDto::NotificationsListed { .. } => "notifications_listed".to_owned(),
        OutcomeDto::CellsListed { .. } => "cells_listed".to_owned(),
        OutcomeDto::CellLeasesInspected { .. } => "cell_leases_inspected".to_owned(),
        OutcomeDto::BlastRadiusPreviewed { .. } => "blast_radius_previewed".to_owned(),
        OutcomeDto::CommandStatusQueried { .. } => "command_status_queried".to_owned(),
        OutcomeDto::ProcessMigrationRequested { .. } => "process_migration_requested".to_owned(),
        OutcomeDto::TopicPurged { .. } => "topic_purged".to_owned(),
        OutcomeDto::SettingsDomainUpdated { .. } => "settings_domain_updated".to_owned(),
        OutcomeDto::ContentTypeAssociationSet { .. } => "content_type_association_set".to_owned(),
        OutcomeDto::ChooserGrantIssued { .. } => "chooser_grant_issued".to_owned(),
        OutcomeDto::ReceiptUndone { .. } => "receipt_undone".to_owned(),
    }
}

/// 两次独立派发的回执 → [`FactCheckDto`](渲染事实 vs 直接复检)。
/// 双 ResourceInspected 时逐字段比较五个有界成本事实;否则比较形态标签
/// (未接线形态两侧同为 `failure/NOT_FOUND` 亦如实可比)。
#[must_use]
pub fn fact_check_dto(
    reservation_id_hex: &str,
    first: &ReceiptDto,
    second: &ReceiptDto,
) -> FactCheckDto {
    let rows = match (&first.outcome, &second.outcome) {
        (
            OutcomeDto::ResourceInspected {
                reservation_id_hex: first_id,
                account_id_hex: first_account,
                upper_bound: first_bound,
                usage_high_water: first_water,
                consumption_count: first_count,
            },
            OutcomeDto::ResourceInspected {
                reservation_id_hex: second_id,
                account_id_hex: second_account,
                upper_bound: second_bound,
                usage_high_water: second_water,
                consumption_count: second_count,
            },
        ) => vec![
            fact_row("reservation_id", first_id, second_id),
            fact_row("account_id", first_account, second_account),
            fact_row(
                "upper_bound",
                &first_bound.to_string(),
                &second_bound.to_string(),
            ),
            fact_row(
                "usage_high_water",
                &first_water.to_string(),
                &second_water.to_string(),
            ),
            fact_row(
                "consumption_count",
                &first_count.to_string(),
                &second_count.to_string(),
            ),
        ],
        (first_outcome, second_outcome) => {
            vec![fact_row(
                "outcome",
                &outcome_label(first_outcome),
                &outcome_label(second_outcome),
            )]
        }
    };
    let receipt_hex_matched = first.receipt_hex == second.receipt_hex;
    let matched = receipt_hex_matched && rows.iter().all(|row| row.matched);
    FactCheckDto {
        reservation_id_hex: reservation_id_hex.to_owned(),
        matched,
        receipt_hex_matched,
        first_receipt_hex: first.receipt_hex.clone(),
        second_receipt_hex: second.receipt_hex.clone(),
        rows,
    }
}
