//! Stage D (`IPC-D1..D8` & `[UI-PARITY-001]`) pluggable twelve-domain bounded
//! enumeration, monotonic watermark digest, settings/association/keychain/grant
//! seams, blast-radius dry-run (`[CTRL-HIER-001]`), and idempotent command
//! status reconciliation (`[CTRL-RECOVERY-001]`).

#![allow(clippy::missing_errors_doc)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use nlos_schema::sabi::v1::{
    BlastRadiusImpactSign, ContextResidencyTier, ControlCommandLifecycleState, ControlScope,
    DurableOperationState, ExecutionFiberLifecycleState, ExecutionFiberPhase, PlanNodeKind,
    PlanNodeLifecycleState, RetryDirective, SabiErrorCode, SabiFailure, SettingsDomainKind,
    TaskAttemptLifecycleState, TaskGroupLifecycleState, TaskLifecycleState, TaskUndoClass,
};
use nlos_schema::{
    MAX_DESKTOP_WATERMARK_EVENT_LIMIT, MAX_SYSTEM_CONTROL_ENUM_LIMIT,
    SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
};
use nlos_types::ReceiptId;
use sha2::{Digest, Sha256};

use crate::control::{
    ApplicationInspection, ApplicationInspector, DurableOperationInspection,
    ExecutionFiberInspection, NotificationInspection, NotificationInspector, ProcessInspection,
    ProcessInspector, ResourceInspection, ResourceInspector, SearchInspection, SearchInspector,
    TaskNodeInspection, TopicInspection,
};
use crate::executor_receipt::derive_executor_receipt_id;
use crate::{
    ApplicationCommandExecutor, ApplicationControlRequest, ExecutionFiberInspectSource,
    OperationCommandExecutor, OperationControlRequest, OperationInspectSource,
    TaskNodeInspectSource, TopicInspectSource,
};

/// Maximum retained command status entries in [`CommandStatusLedger`].
pub const MAX_COMMAND_STATUS_LEDGER_ENTRIES: usize = 512;

/// Maximum retained watermark events in [`InMemoryDesktopControlSurface`].
pub const MAX_RETAINED_WATERMARK_EVENTS: usize = 512;

/// `IPC-D1`: Bounded `TaskSummaryProjection` record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSummaryRecord {
    pub task_id: [u8; 16],
    pub plan_id: [u8; 16],
    pub application_id: Option<[u8; 16]>,
    pub installation_generation: u64,
    pub plan_revision: u64,
    pub task_state: TaskLifecycleState,
    pub undo_class: TaskUndoClass,
    pub reservation_id: Option<[u8; 16]>,
    pub cell_id: Option<[u8; 16]>,
    pub updated_at_ms: u64,
    pub expected_generation_or_revision: u64,
}

/// `IPC-D1`: Bounded cursor page of `TaskSummaryRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskListPage {
    pub tasks: Vec<TaskSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D2`: Bounded `TaskGroup` summary record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskGroupSummaryRecord {
    pub group_id: [u8; 16],
    pub task_id: [u8; 16],
    pub parent_group_id: Option<[u8; 16]>,
    pub state: TaskGroupLifecycleState,
    pub membership_generation: u64,
    pub state_seq: u64,
    pub depth: u32,
    pub cancel_epoch: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// `IPC-D2`: Bounded cursor page of `TaskGroupSummaryRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskGroupListPage {
    pub groups: Vec<TaskGroupSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D3`: Bounded `TaskNode` list page with `[PLAN-LAZY-001]` collapsed
/// `METADATA_ONLY` and `COLD` super-node counts and explicit `stale_generation`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskNodeListPage {
    pub plan_id: [u8; 16],
    pub current_plan_revision: u64,
    pub stale_generation: bool,
    pub nodes: Vec<TaskNodeInspection>,
    pub metadata_only_collapsed_count: u64,
    pub cold_collapsed_count: u64,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D3`: Competitive `TaskAttempt` projection row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskAttemptSummaryRecord {
    pub attempt_id: [u8; 16],
    pub task_id: [u8; 16],
    pub node_id: Option<[u8; 16]>,
    pub state: TaskAttemptLifecycleState,
    pub isolation_domain_id: [u8; 16],
    pub provider_isolation_unverified: bool,
    pub cas_winner: bool,
    pub expected_generation_or_revision: u64,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
}

/// `IPC-D3`: Bounded `TaskAttempt` page with `stale_generation` fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskAttemptListPage {
    pub current_generation: u64,
    pub stale_generation: bool,
    pub attempts: Vec<TaskAttemptSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D3`: `ExecutionFiber` summary with 5-phase timings + `shared_thread_like`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionFiberSummaryRecord {
    pub fiber: ExecutionFiberInspection,
    pub process_id: Option<[u8; 16]>,
    pub shared_thread_like: bool,
}

/// `IPC-D3`: Bounded `ExecutionFiber` page with `stale_generation` fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionFiberListPage {
    pub current_generation: u64,
    pub stale_generation: bool,
    pub fibers: Vec<ExecutionFiberSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D3`: Bounded `DurableOperation` page with `stale_generation` fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableOperationListPage {
    pub current_generation: u64,
    pub stale_generation: bool,
    pub operations: Vec<DurableOperationInspection>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D4`: Installed `Application` summary row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledApplicationSummaryRecord {
    pub application_id: [u8; 16],
    pub installation_id: [u8; 16],
    pub package_id: [u8; 16],
    pub package_manifest_digest: [u8; 32],
    pub current_installation_generation: u64,
    pub status: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// `IPC-D4`: Bounded cursor page of `InstalledApplicationSummaryRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledApplicationListPage {
    pub applications: Vec<InstalledApplicationSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D4`: Non-authoritative search candidate row (`0x1F` pinned).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchCandidateRecord {
    pub candidate_id: [u8; 16],
    pub object_id: [u8; 16],
    pub scope_id: [u8; 32],
    pub title_snippet: String,
    pub rank_score_milli: u32,
    pub non_authoritative_restrictions_bits: u32,
}

/// `IPC-D4`: Bounded `QuerySearchCandidates` page (`0x1F` pinned).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchCandidateQueryPage {
    pub scope_id: [u8; 32],
    pub non_authoritative_restrictions_bits: u32,
    pub achieved_consistency: u32,
    pub failed_shards: Vec<u32>,
    pub candidates: Vec<SearchCandidateRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D5`: Monotonic desktop watermark digest event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopWatermarkEventRecord {
    pub watermark: u64,
    pub domain_scope: ControlScope,
    pub entity_id: [u8; 16],
    pub new_generation_or_revision: u64,
    pub state_digest: [u8; 32],
    pub occurred_at_ms: u64,
}

/// `IPC-D5`: Bounded `PollDesktopWatermarkEvents` snapshot (`limit <= 128`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopWatermarkPollPage {
    pub current_watermark: u64,
    pub events: Vec<DesktopWatermarkEventRecord>,
    pub truncated: bool,
}

/// `IPC-D5`: Settings entry projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsEntryRecord {
    pub key: String,
    pub value_cbor: Vec<u8>,
    pub revision: u64,
}

/// `IPC-D5`: Settings domain read projection (`system/user/workspace/application`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsDomainProjection {
    pub domain_kind: SettingsDomainKind,
    pub scope_target_id: Option<[u8; 16]>,
    pub domain_revision: u64,
    pub entries: Vec<SettingsEntryRecord>,
}

/// `IPC-D5`: Content-type to `ApplicationId` association row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentTypeAssociationRecord {
    pub association_id: [u8; 16],
    pub content_type: String,
    pub application_id: [u8; 16],
    pub expected_generation_or_revision: u64,
}

/// `IPC-D5`: Bounded page of `ContentTypeAssociationRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentTypeAssociationListPage {
    pub associations: Vec<ContentTypeAssociationRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D5`: Zero-secret `KeychainReference` metadata handle row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeychainReferenceRecord {
    pub keychain_ref_id: [u8; 16],
    pub owner_principal_id: [u8; 16],
    pub label: String,
    pub public_fingerprint: [u8; 32],
    pub expected_generation_or_revision: u64,
    pub hardware_backed: bool,
}

/// `IPC-D5`: Bounded page of `KeychainReferenceRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeychainReferenceListPage {
    pub references: Vec<KeychainReferenceRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D5`: Scoped `ChooserGrant` capability descriptor row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChooserGrantRecord {
    pub grant_id: [u8; 16],
    pub application_id: [u8; 16],
    pub chooser_kind: u32,
    pub resource_handle: [u8; 16],
    pub expected_generation_or_revision: u64,
    pub expires_at_ms: u64,
}

/// `IPC-D5`: Bounded page of `ChooserGrantRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChooserGrantListPage {
    pub grants: Vec<ChooserGrantRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D6`: `Workspace` projection row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRecord {
    pub workspace_id: [u8; 16],
    pub name: String,
    pub layout_preset: u32,
    pub active_surface_count: u32,
    pub focused_task_id: Option<[u8; 16]>,
    pub expected_generation_or_revision: u64,
    pub updated_at_ms: u64,
}

/// `IPC-D6`: Bounded page of `WorkspaceRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceListPage {
    pub workspaces: Vec<WorkspaceRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D6`: Single `Workspace` inspect result with `stale_generation` fence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceInspectResult {
    pub stale_generation: bool,
    pub current_generation: u64,
    pub workspace: Option<WorkspaceRecord>,
}

/// `IPC-D6`: `Process` summary projection row with CAS generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSummaryRecord {
    pub process_id: [u8; 16],
    pub expected_generation_or_revision: u64,
    pub agent_instance_id: [u8; 16],
    pub task_id: [u8; 16],
    pub task_attempt_id: [u8; 16],
    pub isolation_domain_id: [u8; 16],
    pub cell_id: Option<[u8; 16]>,
    pub lifecycle_state: u32,
}

/// `IPC-D6`: Bounded page of `ProcessSummaryRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessListPage {
    pub processes: Vec<ProcessSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D6`: Nomad triple-watermark `ResourceReservation` summary + `ContextResidencyTier`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceReservationSummaryRecord {
    pub reservation_id: [u8; 16],
    pub account_id: [u8; 16],
    pub actual_usage: u64,
    pub effective_granted: u64,
    pub requested_ceiling: u64,
    pub context_residency_tier: ContextResidencyTier,
    pub expected_generation_or_revision: u64,
}

/// `IPC-D6`: Bounded page of `ResourceReservationSummaryRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceReservationListPage {
    pub reservations: Vec<ResourceReservationSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D6`: `Topic` summary row with CAS token and retained message count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopicSummaryRecord {
    pub topic: TopicInspection,
    pub expected_generation_or_revision: u64,
    pub retained_messages: u64,
}

/// `IPC-D6`: Bounded page of `TopicSummaryRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopicListPage {
    pub topics: Vec<TopicSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D6`: `Notification` summary row with CAS sequence token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationSummaryRecord {
    pub notification: NotificationInspection,
    pub expected_generation_or_revision: u64,
}

/// `IPC-D6`: Bounded page of `NotificationSummaryRecord`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationListPage {
    pub notifications: Vec<NotificationSummaryRecord>,
    pub next_cursor: Option<[u8; 16]>,
    pub truncated: bool,
}

/// `IPC-D8`: One `+ / ~ / - / =` descendant impact item (`[CTRL-HIER-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlBlastRadiusImpactRecord {
    pub sign: BlastRadiusImpactSign,
    pub affected_scope: ControlScope,
    pub affected_id: [u8; 16],
    pub current_generation_or_revision: u64,
    pub summary_code: String,
}

/// `IPC-D8`: Read-only `PreviewControlBlastRadius` dry-run result (`[CTRL-HIER-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlBlastRadiusPreviewResult {
    pub target_scope: ControlScope,
    pub target_id: [u8; 16],
    pub preview_action: u32,
    pub statutory_lock_blocked: bool,
    pub statutory_lock_code: String,
    pub impacts: Vec<ControlBlastRadiusImpactRecord>,
    pub truncated: bool,
}

/// `IPC-D8`: Idempotent command status reconciliation record (`[CTRL-RECOVERY-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlCommandStatusRecord {
    pub control_command_id: [u8; 16],
    pub state: ControlCommandLifecycleState,
    pub scope: ControlScope,
    pub target_id: [u8; 16],
    pub receipt_id: Option<[u8; 16]>,
    pub failure: Option<SabiFailure>,
}

/// Pluggable read/enumeration seam for `IPC-D1..D6`.
pub trait DesktopEnumerationSource: Send + Sync {
    fn list_tasks(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TaskListPage, SabiFailure>;

    fn list_task_groups(
        &self,
        task_id_filter: Option<[u8; 16]>,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TaskGroupListPage, SabiFailure>;

    fn list_task_nodes(
        &self,
        plan_id: [u8; 16],
        expected_plan_revision: u64,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TaskNodeListPage, SabiFailure>;

    fn list_task_attempts(
        &self,
        scope_filter_id: Option<[u8; 16]>,
        expected_generation: u64,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TaskAttemptListPage, SabiFailure>;

    fn list_execution_fibers(
        &self,
        process_filter_id: Option<[u8; 16]>,
        expected_generation: u64,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ExecutionFiberListPage, SabiFailure>;

    fn list_operations(
        &self,
        fiber_filter_id: Option<[u8; 16]>,
        expected_generation: u64,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<DurableOperationListPage, SabiFailure>;

    fn list_installed_applications(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<InstalledApplicationListPage, SabiFailure>;

    fn query_search_candidates(
        &self,
        scope_id: [u8; 32],
        query_utf8: &[u8],
        cursor_offset: u64,
        limit: usize,
    ) -> Result<SearchCandidateQueryPage, SabiFailure>;

    fn poll_desktop_watermark_events(
        &self,
        since_watermark: u64,
        limit: usize,
    ) -> Result<DesktopWatermarkPollPage, SabiFailure>;

    fn inspect_settings_domain(
        &self,
        domain_kind: SettingsDomainKind,
        scope_target_id: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<SettingsDomainProjection, SabiFailure>;

    fn list_content_type_associations(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ContentTypeAssociationListPage, SabiFailure>;

    fn list_keychain_references(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<KeychainReferenceListPage, SabiFailure>;

    fn list_chooser_grants(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ChooserGrantListPage, SabiFailure>;

    fn list_workspaces(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<WorkspaceListPage, SabiFailure>;

    fn inspect_workspace(
        &self,
        workspace_id: [u8; 16],
        expected_revision: u64,
    ) -> Result<WorkspaceInspectResult, SabiFailure>;

    fn list_processes(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ProcessListPage, SabiFailure>;

    fn list_resource_reservations(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ResourceReservationListPage, SabiFailure>;

    fn list_topics(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TopicListPage, SabiFailure>;

    fn list_notifications(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<NotificationListPage, SabiFailure>;
}

/// Request envelope handed to [`DesktopDomainMutationExecutor`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopMutationRequest {
    pub scope: ControlScope,
    pub target_id: [u8; 16],
    pub expected_generation_or_revision: u64,
    pub issuer_principal_id: [u8; 16],
    pub idempotency_key: [u8; 16],
    pub requested_at_ms: i64,
}

/// Pluggable write/CAS seam for `IPC-D5..D8` additive mutations.
pub trait DesktopDomainMutationExecutor: Send + Sync {
    fn purge_topic(&self, request: DesktopMutationRequest) -> Result<ReceiptId, SabiFailure>;

    fn update_settings_domain(
        &self,
        request: DesktopMutationRequest,
        domain_kind: SettingsDomainKind,
        key: &str,
        value_cbor: &[u8],
    ) -> Result<ReceiptId, SabiFailure>;

    fn set_content_type_association(
        &self,
        request: DesktopMutationRequest,
        content_type: &str,
        application_id: [u8; 16],
    ) -> Result<ReceiptId, SabiFailure>;

    fn issue_chooser_grant(
        &self,
        request: DesktopMutationRequest,
        application_id: [u8; 16],
        chooser_kind: u32,
        resource_handle: [u8; 16],
    ) -> Result<ReceiptId, SabiFailure>;

    fn undo_reversible_receipt(
        &self,
        request: DesktopMutationRequest,
        undo_class: TaskUndoClass,
    ) -> Result<ReceiptId, SabiFailure>;
}

/// Pluggable blast-radius dry-run & command reconciliation seam (`IPC-D8`).
pub trait ControlReconciliationSource: Send + Sync {
    fn preview_blast_radius(
        &self,
        target_scope: ControlScope,
        target_id: [u8; 16],
        secondary_id: Option<[u8; 16]>,
        preview_action: u32,
        limit: usize,
    ) -> Result<ControlBlastRadiusPreviewResult, SabiFailure>;

    fn query_command_status(
        &self,
        control_command_id: [u8; 16],
    ) -> Result<Option<ControlCommandStatusRecord>, SabiFailure>;
}

/// Bounded in-memory idempotent command status ledger (`[CTRL-RECOVERY-001]`).
#[derive(Default)]
pub struct CommandStatusLedger {
    records: Mutex<BTreeMap<[u8; 16], ControlCommandStatusRecord>>,
}

impl CommandStatusLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, status: ControlCommandStatusRecord) {
        if let Ok(mut guard) = self.records.lock() {
            if guard.len() >= MAX_COMMAND_STATUS_LEDGER_ENTRIES
                && !guard.contains_key(&status.control_command_id)
                && let Some(oldest_key) = guard.keys().next().copied()
            {
                guard.remove(&oldest_key);
            }
            guard.insert(status.control_command_id, status);
        }
    }

    #[must_use]
    pub fn lookup(&self, control_command_id: [u8; 16]) -> Option<ControlCommandStatusRecord> {
        self.records
            .lock()
            .ok()
            .and_then(|guard| guard.get(&control_command_id).cloned())
    }
}

/// Canonical preview action codes for `PreviewControlBlastRadius` (`IPC-D8`).
pub mod preview_action {
    pub const PAUSE: u32 = 1;
    pub const RESUME: u32 = 2;
    pub const CANCEL: u32 = 3;
    pub const KILL: u32 = 4;
    pub const THROTTLE: u32 = 5;
    pub const RECLAIM: u32 = 6;
    pub const DISABLE_APPLICATION: u32 = 7;
    pub const UNINSTALL_APPLICATION: u32 = 8;
    pub const MIGRATE_PROCESS: u32 = 9;
    pub const PURGE_TOPIC: u32 = 10;
    pub const MUTATE_SEARCH_CANDIDATE_LOCK: u32 = 201;
    pub const UNDO_IRREVERSIBLE_RECEIPT_LOCK: u32 = 202;
    pub const MUTATE_COMPLETED_OR_ACTIVE_NODE_LOCK: u32 = 203;
    pub const OVERRIDE_TASK_ATTEMPT_COMMIT_LOCK: u32 = 204;
    pub const OVERRIDE_EFFECT_UNKNOWN_OPERATION_LOCK: u32 = 205;
}

/// Helper performing deterministic `limit <= 64` cursor pagination on sorted items.
fn paginate_by_id<T: Clone>(
    items: &[T],
    cursor: Option<[u8; 16]>,
    limit: usize,
    id_of: impl Fn(&T) -> [u8; 16],
) -> (Vec<T>, Option<[u8; 16]>, bool) {
    let clamped = limit.clamp(1, MAX_SYSTEM_CONTROL_ENUM_LIMIT);
    let filtered: Vec<&T> = items
        .iter()
        .filter(|item| cursor.is_none_or(|after| id_of(item) > after))
        .collect();
    let truncated = filtered.len() > clamped;
    let page: Vec<T> = filtered.into_iter().take(clamped).cloned().collect();
    let next_cursor = if truncated {
        page.last().map(&id_of)
    } else {
        None
    };
    (page, next_cursor, truncated)
}

fn not_found(safe_message: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: safe_message.to_owned(),
    }
}

fn cas_conflict(safe_message: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::Conflict.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: safe_message.to_owned(),
    }
}

fn sha256_digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

#[derive(Default)]
struct MutableDesktopState {
    tasks: BTreeMap<[u8; 16], TaskSummaryRecord>,
    task_groups: BTreeMap<[u8; 16], TaskGroupSummaryRecord>,
    plan_revisions: BTreeMap<[u8; 16], u64>,
    task_nodes: BTreeMap<([u8; 16], [u8; 16]), TaskNodeInspection>,
    attempt_scope_generations: BTreeMap<[u8; 16], u64>,
    task_attempts: BTreeMap<[u8; 16], TaskAttemptSummaryRecord>,
    fiber_scope_generations: BTreeMap<[u8; 16], u64>,
    execution_fibers: BTreeMap<[u8; 16], ExecutionFiberSummaryRecord>,
    operation_scope_generations: BTreeMap<[u8; 16], u64>,
    operations: BTreeMap<[u8; 16], DurableOperationInspection>,
    applications: BTreeMap<[u8; 16], InstalledApplicationSummaryRecord>,
    search_scopes: BTreeMap<[u8; 32], SearchInspection>,
    search_candidates: BTreeMap<([u8; 32], [u8; 16]), SearchCandidateRecord>,
    search_failed_shards: BTreeMap<[u8; 32], Vec<u32>>,
    watermark_counter: u64,
    watermark_events: Vec<DesktopWatermarkEventRecord>,
    settings_domains: BTreeMap<(i32, [u8; 16]), SettingsDomainProjection>,
    associations: BTreeMap<[u8; 16], ContentTypeAssociationRecord>,
    keychain_refs: BTreeMap<[u8; 16], KeychainReferenceRecord>,
    chooser_grants: BTreeMap<[u8; 16], ChooserGrantRecord>,
    workspaces: BTreeMap<[u8; 16], WorkspaceRecord>,
    processes: BTreeMap<[u8; 16], ProcessSummaryRecord>,
    reservations: BTreeMap<[u8; 16], ResourceReservationSummaryRecord>,
    topics: BTreeMap<[u8; 16], TopicSummaryRecord>,
    notifications: BTreeMap<[u8; 16], NotificationSummaryRecord>,
    idempotent_receipts: BTreeMap<[u8; 16], ReceiptId>,
}

/// Deterministic in-memory / workstation fixture implementing all Stage D
/// enumeration, settings, watermark, blast-radius, reconciliation, search, and
/// notification seams (`IPC-D1..D8`).
pub struct InMemoryDesktopControlSurface {
    state: Mutex<MutableDesktopState>,
    ledger: CommandStatusLedger,
}

impl Default for InMemoryDesktopControlSurface {
    fn default() -> Self {
        Self::new_workstation_defaults()
    }
}

impl InMemoryDesktopControlSurface {
    /// Creates an empty surface with no seeded records.
    #[must_use]
    pub fn new_empty() -> Self {
        Self {
            state: Mutex::new(MutableDesktopState::default()),
            ledger: CommandStatusLedger::new(),
        }
    }

    /// Creates a workstation-seeded surface with default `System` & `User`
    /// settings domains and one primary `Workspace`.
    #[must_use]
    pub fn new_workstation_defaults() -> Self {
        let surface = Self::new_empty();
        surface.upsert_settings_domain(SettingsDomainProjection {
            domain_kind: SettingsDomainKind::System,
            scope_target_id: None,
            domain_revision: 1,
            entries: vec![SettingsEntryRecord {
                key: "desktop.theme".to_owned(),
                value_cbor: vec![0x64, b'd', b'a', b'r', b'k'],
                revision: 1,
            }],
        });
        surface.upsert_settings_domain(SettingsDomainProjection {
            domain_kind: SettingsDomainKind::User,
            scope_target_id: None,
            domain_revision: 1,
            entries: vec![SettingsEntryRecord {
                key: "desktop.locale".to_owned(),
                value_cbor: vec![0x65, b'z', b'h', b'-', b'C', b'N'],
                revision: 1,
            }],
        });
        surface.upsert_workspace(WorkspaceRecord {
            workspace_id: [0xD0; 16],
            name: "Primary Workstation".to_owned(),
            layout_preset: 4,
            active_surface_count: 1,
            focused_task_id: None,
            expected_generation_or_revision: 1,
            updated_at_ms: 1_000,
        });
        surface
    }

    #[must_use]
    pub const fn command_ledger(&self) -> &CommandStatusLedger {
        &self.ledger
    }

    pub fn upsert_task(&self, record: TaskSummaryRecord) {
        if let Ok(mut guard) = self.state.lock() {
            let task_id = record.task_id;
            let rev = record.expected_generation_or_revision;
            let ts = record.updated_at_ms;
            guard.tasks.insert(task_id, record);
            Self::push_watermark_locked(&mut guard, ControlScope::Task, task_id, rev, ts);
        }
    }

    pub fn upsert_task_group(&self, record: TaskGroupSummaryRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard.task_groups.insert(record.group_id, record);
        }
    }

    pub fn upsert_plan_nodes(
        &self,
        plan_id: [u8; 16],
        current_plan_revision: u64,
        nodes: Vec<TaskNodeInspection>,
    ) {
        if let Ok(mut guard) = self.state.lock() {
            guard.plan_revisions.insert(plan_id, current_plan_revision);
            for node in nodes {
                guard.task_nodes.insert((plan_id, node.node_id), node);
            }
        }
    }

    pub fn upsert_task_attempt(&self, scope_generation: u64, record: TaskAttemptSummaryRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard
                .attempt_scope_generations
                .insert(record.task_id, scope_generation);
            guard.task_attempts.insert(record.attempt_id, record);
        }
    }

    pub fn upsert_execution_fiber(
        &self,
        process_generation: u64,
        record: ExecutionFiberSummaryRecord,
    ) {
        if let Ok(mut guard) = self.state.lock() {
            if let Some(pid) = record.process_id {
                guard
                    .fiber_scope_generations
                    .insert(pid, process_generation);
            }
            guard.execution_fibers.insert(record.fiber.fiber_id, record);
        }
    }

    pub fn upsert_operation(&self, record: DurableOperationInspection) {
        if let Ok(mut guard) = self.state.lock() {
            guard
                .operation_scope_generations
                .insert(record.owner_fiber_id, record.owner_fiber_generation);
            guard.operations.insert(record.operation_id, record);
        }
    }

    pub fn upsert_application(&self, record: InstalledApplicationSummaryRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard.applications.insert(record.application_id, record);
        }
    }

    pub fn upsert_search_scope(
        &self,
        inspection: SearchInspection,
        candidates: Vec<SearchCandidateRecord>,
        failed_shards: Vec<u32>,
    ) {
        if let Ok(mut guard) = self.state.lock() {
            let scope_id = inspection.scope_id;
            guard.search_scopes.insert(scope_id, inspection);
            guard.search_failed_shards.insert(scope_id, failed_shards);
            for mut candidate in candidates {
                candidate.non_authoritative_restrictions_bits =
                    SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL;
                guard
                    .search_candidates
                    .insert((scope_id, candidate.candidate_id), candidate);
            }
        }
    }

    pub fn upsert_settings_domain(&self, domain: SettingsDomainProjection) {
        if let Ok(mut guard) = self.state.lock() {
            let key = (
                domain.domain_kind as i32,
                domain.scope_target_id.unwrap_or([0_u8; 16]),
            );
            guard.settings_domains.insert(key, domain);
        }
    }

    pub fn upsert_content_type_association(&self, assoc: ContentTypeAssociationRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard.associations.insert(assoc.association_id, assoc);
        }
    }

    pub fn upsert_keychain_reference(&self, reference: KeychainReferenceRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard
                .keychain_refs
                .insert(reference.keychain_ref_id, reference);
        }
    }

    pub fn upsert_chooser_grant(&self, grant: ChooserGrantRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard.chooser_grants.insert(grant.grant_id, grant);
        }
    }

    pub fn upsert_workspace(&self, workspace: WorkspaceRecord) {
        if let Ok(mut guard) = self.state.lock() {
            let id = workspace.workspace_id;
            let rev = workspace.expected_generation_or_revision;
            let ts = workspace.updated_at_ms;
            guard.workspaces.insert(id, workspace);
            Self::push_watermark_locked(&mut guard, ControlScope::Workspace, id, rev, ts);
        }
    }

    pub fn upsert_process(&self, process: ProcessSummaryRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard.processes.insert(process.process_id, process);
        }
    }

    pub fn upsert_resource_reservation(&self, reservation: ResourceReservationSummaryRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard
                .reservations
                .insert(reservation.reservation_id, reservation);
        }
    }

    pub fn upsert_topic(&self, topic: TopicSummaryRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard.topics.insert(topic.topic.topic_id, topic);
        }
    }

    pub fn upsert_notification(&self, notification: NotificationSummaryRecord) {
        if let Ok(mut guard) = self.state.lock() {
            guard
                .notifications
                .insert(notification.notification.notification_id, notification);
        }
    }

    fn push_watermark_locked(
        state: &mut MutableDesktopState,
        domain_scope: ControlScope,
        entity_id: [u8; 16],
        new_generation_or_revision: u64,
        occurred_at_ms: u64,
    ) {
        state.watermark_counter = state.watermark_counter.saturating_add(1);
        let watermark = state.watermark_counter;
        let scope_bytes = (domain_scope as i32).to_be_bytes();
        let digest = sha256_digest(&[
            b"llmos/desktop-watermark/v1\0",
            &watermark.to_be_bytes(),
            &scope_bytes,
            &entity_id,
            &new_generation_or_revision.to_be_bytes(),
        ]);
        if state.watermark_events.len() >= MAX_RETAINED_WATERMARK_EVENTS {
            state.watermark_events.remove(0);
        }
        state.watermark_events.push(DesktopWatermarkEventRecord {
            watermark,
            domain_scope,
            entity_id,
            new_generation_or_revision,
            state_digest: digest,
            occurred_at_ms,
        });
    }
}

impl SearchInspector for InMemoryDesktopControlSurface {
    fn inspect_search(&self, scope_id: [u8; 32]) -> Result<SearchInspection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("search inspector state poisoned"))?;
        guard
            .search_scopes
            .get(&scope_id)
            .cloned()
            .ok_or_else(|| not_found("requested search scope was not found"))
    }
}

impl NotificationInspector for InMemoryDesktopControlSurface {
    fn inspect_notification(
        &self,
        notification_id: [u8; 16],
    ) -> Result<NotificationInspection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("notification inspector state poisoned"))?;
        guard
            .notifications
            .get(&notification_id)
            .map(|entry| entry.notification.clone())
            .ok_or_else(|| not_found("requested notification was not found"))
    }
}

impl DesktopEnumerationSource for InMemoryDesktopControlSurface {
    fn list_tasks(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TaskListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<TaskSummaryRecord> = guard.tasks.values().cloned().collect();
        let (tasks, next_cursor, truncated) = paginate_by_id(&all, cursor, limit, |t| t.task_id);
        Ok(TaskListPage {
            tasks,
            next_cursor,
            truncated,
        })
    }

    fn list_task_groups(
        &self,
        task_id_filter: Option<[u8; 16]>,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TaskGroupListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<TaskGroupSummaryRecord> = guard
            .task_groups
            .values()
            .filter(|g| task_id_filter.is_none_or(|tid| g.task_id == tid))
            .cloned()
            .collect();
        let (groups, next_cursor, truncated) = paginate_by_id(&all, cursor, limit, |g| g.group_id);
        Ok(TaskGroupListPage {
            groups,
            next_cursor,
            truncated,
        })
    }

    fn list_task_nodes(
        &self,
        plan_id: [u8; 16],
        expected_plan_revision: u64,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TaskNodeListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let Some(&current_plan_revision) = guard.plan_revisions.get(&plan_id) else {
            return Err(not_found("requested task plan was not found"));
        };
        if expected_plan_revision != 0 && expected_plan_revision != current_plan_revision {
            return Ok(TaskNodeListPage {
                plan_id,
                current_plan_revision,
                stale_generation: true,
                nodes: Vec::new(),
                metadata_only_collapsed_count: 0,
                cold_collapsed_count: 0,
                next_cursor: None,
                truncated: false,
            });
        }
        let plan_nodes: Vec<TaskNodeInspection> = guard
            .task_nodes
            .range((plan_id, [0_u8; 16])..=(plan_id, [0xFF_u8; 16]))
            .map(|(_, node)| node.clone())
            .collect();
        let metadata_only_collapsed_count = plan_nodes
            .iter()
            .filter(|n| n.residency_tier == ContextResidencyTier::MetadataOnly)
            .count() as u64;
        let cold_collapsed_count = plan_nodes
            .iter()
            .filter(|n| n.residency_tier == ContextResidencyTier::Cold)
            .count() as u64;
        let (nodes, next_cursor, truncated) =
            paginate_by_id(&plan_nodes, cursor, limit, |n| n.node_id);
        Ok(TaskNodeListPage {
            plan_id,
            current_plan_revision,
            stale_generation: false,
            nodes,
            metadata_only_collapsed_count,
            cold_collapsed_count,
            next_cursor,
            truncated,
        })
    }

    fn list_task_attempts(
        &self,
        scope_filter_id: Option<[u8; 16]>,
        expected_generation: u64,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TaskAttemptListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let current_generation = if let Some(scope_id) = scope_filter_id {
            let Some(&generation) = guard.attempt_scope_generations.get(&scope_id) else {
                return Err(not_found("requested task attempt scope was not found"));
            };
            generation
        } else {
            guard
                .attempt_scope_generations
                .values()
                .copied()
                .max()
                .unwrap_or(1)
        };
        if expected_generation != 0 && expected_generation != current_generation {
            return Ok(TaskAttemptListPage {
                current_generation,
                stale_generation: true,
                attempts: Vec::new(),
                next_cursor: None,
                truncated: false,
            });
        }
        let matching: Vec<TaskAttemptSummaryRecord> = guard
            .task_attempts
            .values()
            .filter(|a| {
                scope_filter_id.is_none_or(|sid| a.task_id == sid || a.node_id == Some(sid))
            })
            .cloned()
            .collect();
        let (attempts, next_cursor, truncated) =
            paginate_by_id(&matching, cursor, limit, |a| a.attempt_id);
        Ok(TaskAttemptListPage {
            current_generation,
            stale_generation: false,
            attempts,
            next_cursor,
            truncated,
        })
    }

    fn list_execution_fibers(
        &self,
        process_filter_id: Option<[u8; 16]>,
        expected_generation: u64,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ExecutionFiberListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let current_generation = if let Some(pid) = process_filter_id {
            let Some(&generation) = guard.fiber_scope_generations.get(&pid) else {
                return Err(not_found("requested process fiber scope was not found"));
            };
            generation
        } else {
            guard
                .fiber_scope_generations
                .values()
                .copied()
                .max()
                .unwrap_or(1)
        };
        if expected_generation != 0 && expected_generation != current_generation {
            return Ok(ExecutionFiberListPage {
                current_generation,
                stale_generation: true,
                fibers: Vec::new(),
                next_cursor: None,
                truncated: false,
            });
        }
        let matching: Vec<ExecutionFiberSummaryRecord> = guard
            .execution_fibers
            .values()
            .filter(|f| process_filter_id.is_none_or(|pid| f.process_id == Some(pid)))
            .cloned()
            .collect();
        let (fibers, next_cursor, truncated) =
            paginate_by_id(&matching, cursor, limit, |f| f.fiber.fiber_id);
        Ok(ExecutionFiberListPage {
            current_generation,
            stale_generation: false,
            fibers,
            next_cursor,
            truncated,
        })
    }

    fn list_operations(
        &self,
        fiber_filter_id: Option<[u8; 16]>,
        expected_generation: u64,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<DurableOperationListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let current_generation = if let Some(fid) = fiber_filter_id {
            let Some(&generation) = guard.operation_scope_generations.get(&fid) else {
                return Err(not_found(
                    "requested owner fiber operation scope was not found",
                ));
            };
            generation
        } else {
            guard
                .operation_scope_generations
                .values()
                .copied()
                .max()
                .unwrap_or(1)
        };
        if expected_generation != 0 && expected_generation != current_generation {
            return Ok(DurableOperationListPage {
                current_generation,
                stale_generation: true,
                operations: Vec::new(),
                next_cursor: None,
                truncated: false,
            });
        }
        let matching: Vec<DurableOperationInspection> = guard
            .operations
            .values()
            .filter(|op| fiber_filter_id.is_none_or(|fid| op.owner_fiber_id == fid))
            .cloned()
            .collect();
        let (operations, next_cursor, truncated) =
            paginate_by_id(&matching, cursor, limit, |op| op.operation_id);
        Ok(DurableOperationListPage {
            current_generation,
            stale_generation: false,
            operations,
            next_cursor,
            truncated,
        })
    }

    fn list_installed_applications(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<InstalledApplicationListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<InstalledApplicationSummaryRecord> =
            guard.applications.values().cloned().collect();
        let (applications, next_cursor, truncated) =
            paginate_by_id(&all, cursor, limit, |app| app.application_id);
        Ok(InstalledApplicationListPage {
            applications,
            next_cursor,
            truncated,
        })
    }

    fn query_search_candidates(
        &self,
        scope_id: [u8; 32],
        query_utf8: &[u8],
        cursor_offset: u64,
        limit: usize,
    ) -> Result<SearchCandidateQueryPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let Some(scope) = guard.search_scopes.get(&scope_id) else {
            return Err(not_found("requested search scope was not found"));
        };
        let query_str = std::str::from_utf8(query_utf8).unwrap_or("");
        let query_lower = query_str.to_ascii_lowercase();
        let matching: Vec<SearchCandidateRecord> = guard
            .search_candidates
            .range((scope_id, [0_u8; 16])..=(scope_id, [0xFF_u8; 16]))
            .map(|(_, entry)| entry)
            .filter(|entry| {
                query_lower.is_empty()
                    || entry
                        .title_snippet
                        .to_ascii_lowercase()
                        .contains(&query_lower)
            })
            .cloned()
            .collect();
        let clamped = limit.clamp(1, MAX_SYSTEM_CONTROL_ENUM_LIMIT);
        let skip = usize::try_from(cursor_offset).unwrap_or(usize::MAX);
        let remaining: Vec<SearchCandidateRecord> = matching.into_iter().skip(skip).collect();
        let truncated = remaining.len() > clamped;
        let candidates: Vec<SearchCandidateRecord> = remaining.into_iter().take(clamped).collect();
        let next_cursor = if truncated {
            candidates.last().map(|last| last.candidate_id)
        } else {
            None
        };
        let failed_shards = guard
            .search_failed_shards
            .get(&scope_id)
            .cloned()
            .unwrap_or_default();
        Ok(SearchCandidateQueryPage {
            scope_id,
            non_authoritative_restrictions_bits: SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
            achieved_consistency: u32::from(scope.consistency_class.clamp(1, 3)),
            failed_shards,
            candidates,
            next_cursor,
            truncated,
        })
    }

    fn poll_desktop_watermark_events(
        &self,
        since_watermark: u64,
        limit: usize,
    ) -> Result<DesktopWatermarkPollPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let clamped = limit.clamp(1, MAX_DESKTOP_WATERMARK_EVENT_LIMIT);
        let matching: Vec<&DesktopWatermarkEventRecord> = guard
            .watermark_events
            .iter()
            .filter(|event| event.watermark > since_watermark)
            .collect();
        let truncated = matching.len() > clamped;
        let events = matching.into_iter().take(clamped).cloned().collect();
        Ok(DesktopWatermarkPollPage {
            current_watermark: guard.watermark_counter,
            events,
            truncated,
        })
    }

    fn inspect_settings_domain(
        &self,
        domain_kind: SettingsDomainKind,
        scope_target_id: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<SettingsDomainProjection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let key = (domain_kind as i32, scope_target_id.unwrap_or([0_u8; 16]));
        let Some(mut domain) = guard.settings_domains.get(&key).cloned() else {
            return Err(not_found("requested settings domain was not found"));
        };
        let clamped = limit.clamp(1, MAX_SYSTEM_CONTROL_ENUM_LIMIT);
        domain.entries.truncate(clamped);
        Ok(domain)
    }

    fn list_content_type_associations(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ContentTypeAssociationListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<ContentTypeAssociationRecord> = guard.associations.values().cloned().collect();
        let (associations, next_cursor, truncated) =
            paginate_by_id(&all, cursor, limit, |a| a.association_id);
        Ok(ContentTypeAssociationListPage {
            associations,
            next_cursor,
            truncated,
        })
    }

    fn list_keychain_references(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<KeychainReferenceListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<KeychainReferenceRecord> = guard.keychain_refs.values().cloned().collect();
        let (references, next_cursor, truncated) =
            paginate_by_id(&all, cursor, limit, |r| r.keychain_ref_id);
        Ok(KeychainReferenceListPage {
            references,
            next_cursor,
            truncated,
        })
    }

    fn list_chooser_grants(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ChooserGrantListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<ChooserGrantRecord> = guard.chooser_grants.values().cloned().collect();
        let (grants, next_cursor, truncated) = paginate_by_id(&all, cursor, limit, |g| g.grant_id);
        Ok(ChooserGrantListPage {
            grants,
            next_cursor,
            truncated,
        })
    }

    fn list_workspaces(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<WorkspaceListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<WorkspaceRecord> = guard.workspaces.values().cloned().collect();
        let (workspaces, next_cursor, truncated) =
            paginate_by_id(&all, cursor, limit, |w| w.workspace_id);
        Ok(WorkspaceListPage {
            workspaces,
            next_cursor,
            truncated,
        })
    }

    fn inspect_workspace(
        &self,
        workspace_id: [u8; 16],
        expected_revision: u64,
    ) -> Result<WorkspaceInspectResult, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let Some(workspace) = guard.workspaces.get(&workspace_id).cloned() else {
            return Err(not_found("requested workspace was not found"));
        };
        let current_generation = workspace.expected_generation_or_revision;
        if expected_revision != 0 && expected_revision != current_generation {
            return Ok(WorkspaceInspectResult {
                stale_generation: true,
                current_generation,
                workspace: None,
            });
        }
        Ok(WorkspaceInspectResult {
            stale_generation: false,
            current_generation,
            workspace: Some(workspace),
        })
    }

    fn list_processes(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ProcessListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<ProcessSummaryRecord> = guard.processes.values().cloned().collect();
        let (processes, next_cursor, truncated) =
            paginate_by_id(&all, cursor, limit, |p| p.process_id);
        Ok(ProcessListPage {
            processes,
            next_cursor,
            truncated,
        })
    }

    fn list_resource_reservations(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<ResourceReservationListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<ResourceReservationSummaryRecord> =
            guard.reservations.values().cloned().collect();
        let (reservations, next_cursor, truncated) =
            paginate_by_id(&all, cursor, limit, |r| r.reservation_id);
        Ok(ResourceReservationListPage {
            reservations,
            next_cursor,
            truncated,
        })
    }

    fn list_topics(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<TopicListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<TopicSummaryRecord> = guard.topics.values().cloned().collect();
        let (topics, next_cursor, truncated) =
            paginate_by_id(&all, cursor, limit, |t| t.topic.topic_id);
        Ok(TopicListPage {
            topics,
            next_cursor,
            truncated,
        })
    }

    fn list_notifications(
        &self,
        cursor: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<NotificationListPage, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let all: Vec<NotificationSummaryRecord> = guard.notifications.values().cloned().collect();
        let (notifications, next_cursor, truncated) =
            paginate_by_id(&all, cursor, limit, |n| n.notification.notification_id);
        Ok(NotificationListPage {
            notifications,
            next_cursor,
            truncated,
        })
    }
}

impl DesktopDomainMutationExecutor for InMemoryDesktopControlSurface {
    fn purge_topic(&self, request: DesktopMutationRequest) -> Result<ReceiptId, SabiFailure> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop mutation state unavailable"))?;
        if let Some(&receipt) = guard.idempotent_receipts.get(&request.idempotency_key) {
            return Ok(receipt);
        }
        let Some(topic) = guard.topics.get_mut(&request.target_id) else {
            return Err(not_found("requested topic was not found"));
        };
        if topic.expected_generation_or_revision != request.expected_generation_or_revision {
            return Err(cas_conflict(
                "topic expected_generation_or_revision CAS mismatch",
            ));
        }
        topic.retained_messages = 0;
        topic.expected_generation_or_revision =
            topic.expected_generation_or_revision.saturating_add(1);
        let new_rev = topic.expected_generation_or_revision;
        let receipt = derive_executor_receipt_id(
            b"llmos/topic-purge/v1\0",
            &[
                &request.target_id,
                &request.expected_generation_or_revision.to_be_bytes(),
                &request.idempotency_key,
            ],
        );
        guard
            .idempotent_receipts
            .insert(request.idempotency_key, receipt);
        let ts = u64::try_from(request.requested_at_ms.max(0)).unwrap_or(0);
        Self::push_watermark_locked(
            &mut guard,
            ControlScope::Topic,
            request.target_id,
            new_rev,
            ts,
        );
        Ok(receipt)
    }

    fn update_settings_domain(
        &self,
        request: DesktopMutationRequest,
        domain_kind: SettingsDomainKind,
        key: &str,
        value_cbor: &[u8],
    ) -> Result<ReceiptId, SabiFailure> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop mutation state unavailable"))?;
        if let Some(&receipt) = guard.idempotent_receipts.get(&request.idempotency_key) {
            return Ok(receipt);
        }
        let scope_target = match domain_kind {
            SettingsDomainKind::System | SettingsDomainKind::User => [0_u8; 16],
            SettingsDomainKind::Workspace | SettingsDomainKind::Application => request.target_id,
            SettingsDomainKind::Unspecified => {
                return Err(not_found("unspecified settings domain"));
            }
        };
        let domain_key = (domain_kind as i32, scope_target);
        let entry_domain =
            guard
                .settings_domains
                .entry(domain_key)
                .or_insert_with(|| SettingsDomainProjection {
                    domain_kind,
                    scope_target_id: matches!(
                        domain_kind,
                        SettingsDomainKind::Workspace | SettingsDomainKind::Application
                    )
                    .then_some(request.target_id),
                    domain_revision: 1,
                    entries: Vec::new(),
                });
        if entry_domain.domain_revision != request.expected_generation_or_revision {
            return Err(cas_conflict("settings domain_revision CAS mismatch"));
        }
        entry_domain.domain_revision = entry_domain.domain_revision.saturating_add(1);
        let new_rev = entry_domain.domain_revision;
        if let Some(existing) = entry_domain.entries.iter_mut().find(|e| e.key == key) {
            existing.value_cbor = value_cbor.to_vec();
            existing.revision = new_rev;
        } else {
            entry_domain.entries.push(SettingsEntryRecord {
                key: key.to_owned(),
                value_cbor: value_cbor.to_vec(),
                revision: new_rev,
            });
        }
        let receipt = derive_executor_receipt_id(
            b"llmos/settings-domain-update/v1\0",
            &[
                &request.target_id,
                key.as_bytes(),
                &request.expected_generation_or_revision.to_be_bytes(),
                &request.idempotency_key,
            ],
        );
        guard
            .idempotent_receipts
            .insert(request.idempotency_key, receipt);
        let ts = u64::try_from(request.requested_at_ms.max(0)).unwrap_or(0);
        Self::push_watermark_locked(&mut guard, request.scope, request.target_id, new_rev, ts);
        Ok(receipt)
    }

    fn set_content_type_association(
        &self,
        request: DesktopMutationRequest,
        content_type: &str,
        application_id: [u8; 16],
    ) -> Result<ReceiptId, SabiFailure> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop mutation state unavailable"))?;
        if let Some(&receipt) = guard.idempotent_receipts.get(&request.idempotency_key) {
            return Ok(receipt);
        }
        let next_rev = if let Some(existing) = guard.associations.get(&request.target_id) {
            if existing.expected_generation_or_revision != request.expected_generation_or_revision {
                return Err(cas_conflict(
                    "content-type association expected_generation_or_revision CAS mismatch",
                ));
            }
            existing.expected_generation_or_revision.saturating_add(1)
        } else {
            if request.expected_generation_or_revision != 1 {
                return Err(cas_conflict(
                    "new content-type association requires expected_generation_or_revision=1",
                ));
            }
            1
        };
        guard.associations.insert(
            request.target_id,
            ContentTypeAssociationRecord {
                association_id: request.target_id,
                content_type: content_type.to_owned(),
                application_id,
                expected_generation_or_revision: next_rev,
            },
        );
        let receipt = derive_executor_receipt_id(
            b"llmos/content-type-assoc/v1\0",
            &[
                &request.target_id,
                content_type.as_bytes(),
                &application_id,
                &request.idempotency_key,
            ],
        );
        guard
            .idempotent_receipts
            .insert(request.idempotency_key, receipt);
        let ts = u64::try_from(request.requested_at_ms.max(0)).unwrap_or(0);
        Self::push_watermark_locked(
            &mut guard,
            ControlScope::Application,
            request.target_id,
            next_rev,
            ts,
        );
        Ok(receipt)
    }

    fn issue_chooser_grant(
        &self,
        request: DesktopMutationRequest,
        application_id: [u8; 16],
        chooser_kind: u32,
        resource_handle: [u8; 16],
    ) -> Result<ReceiptId, SabiFailure> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop mutation state unavailable"))?;
        if let Some(&receipt) = guard.idempotent_receipts.get(&request.idempotency_key) {
            return Ok(receipt);
        }
        let ts = u64::try_from(request.requested_at_ms.max(0)).unwrap_or(0);
        guard.chooser_grants.insert(
            request.target_id,
            ChooserGrantRecord {
                grant_id: request.target_id,
                application_id,
                chooser_kind,
                resource_handle,
                expected_generation_or_revision: request.expected_generation_or_revision,
                expires_at_ms: ts.saturating_add(3_600_000),
            },
        );
        let receipt = derive_executor_receipt_id(
            b"llmos/chooser-grant/v1\0",
            &[
                &request.target_id,
                &application_id,
                &chooser_kind.to_be_bytes(),
                &resource_handle,
                &request.idempotency_key,
            ],
        );
        guard
            .idempotent_receipts
            .insert(request.idempotency_key, receipt);
        Self::push_watermark_locked(
            &mut guard,
            ControlScope::Application,
            request.target_id,
            request.expected_generation_or_revision,
            ts,
        );
        Ok(receipt)
    }

    fn undo_reversible_receipt(
        &self,
        request: DesktopMutationRequest,
        undo_class: TaskUndoClass,
    ) -> Result<ReceiptId, SabiFailure> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop mutation state unavailable"))?;
        if let Some(&receipt) = guard.idempotent_receipts.get(&request.idempotency_key) {
            return Ok(receipt);
        }
        let receipt = derive_executor_receipt_id(
            b"llmos/undo-reversible-receipt/v1\0",
            &[
                &request.target_id,
                &(undo_class as i32).to_be_bytes(),
                &request.expected_generation_or_revision.to_be_bytes(),
                &request.idempotency_key,
            ],
        );
        guard
            .idempotent_receipts
            .insert(request.idempotency_key, receipt);
        Ok(receipt)
    }
}

impl ControlReconciliationSource for InMemoryDesktopControlSurface {
    fn preview_blast_radius(
        &self,
        target_scope: ControlScope,
        target_id: [u8; 16],
        _secondary_id: Option<[u8; 16]>,
        preview_action: u32,
        limit: usize,
    ) -> Result<ControlBlastRadiusPreviewResult, SabiFailure> {
        // Statutory non-tamperable locks (`[UI-PARITY-001]`) are surfaced directly
        // in dry-run previews before any mutation is attempted.
        if let Some(lock_code) = statutory_lock_code_for_preview(preview_action) {
            return Ok(ControlBlastRadiusPreviewResult {
                target_scope,
                target_id,
                preview_action,
                statutory_lock_blocked: true,
                statutory_lock_code: lock_code.to_owned(),
                impacts: Vec::new(),
                truncated: false,
            });
        }

        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("desktop enumeration state unavailable"))?;
        let mut impacts = Vec::new();
        let sign = match preview_action {
            preview_action::CANCEL
            | preview_action::KILL
            | preview_action::UNINSTALL_APPLICATION
            | preview_action::PURGE_TOPIC => BlastRadiusImpactSign::Remove,
            preview_action::MIGRATE_PROCESS => BlastRadiusImpactSign::Add,
            _ => BlastRadiusImpactSign::Change,
        };

        impacts.push(ControlBlastRadiusImpactRecord {
            sign,
            affected_scope: target_scope,
            affected_id: target_id,
            current_generation_or_revision: 1,
            summary_code: "primary_target".to_owned(),
        });

        if target_scope == ControlScope::Task {
            for group in guard
                .task_groups
                .values()
                .filter(|g| g.task_id == target_id)
            {
                impacts.push(ControlBlastRadiusImpactRecord {
                    sign,
                    affected_scope: ControlScope::TaskGroup,
                    affected_id: group.group_id,
                    current_generation_or_revision: group.membership_generation.max(1),
                    summary_code: "descendant_task_group".to_owned(),
                });
            }
            for attempt in guard
                .task_attempts
                .values()
                .filter(|a| a.task_id == target_id)
            {
                impacts.push(ControlBlastRadiusImpactRecord {
                    sign,
                    affected_scope: ControlScope::TaskAttempt,
                    affected_id: attempt.attempt_id,
                    current_generation_or_revision: attempt.expected_generation_or_revision,
                    summary_code: "descendant_task_attempt".to_owned(),
                });
            }
            for proc_row in guard.processes.values().filter(|p| p.task_id == target_id) {
                impacts.push(ControlBlastRadiusImpactRecord {
                    sign,
                    affected_scope: ControlScope::Process,
                    affected_id: proc_row.process_id,
                    current_generation_or_revision: proc_row.expected_generation_or_revision,
                    summary_code: "descendant_process".to_owned(),
                });
                for fiber in guard
                    .execution_fibers
                    .values()
                    .filter(|f| f.process_id == Some(proc_row.process_id))
                {
                    impacts.push(ControlBlastRadiusImpactRecord {
                        sign,
                        affected_scope: ControlScope::ExecutionFiber,
                        affected_id: fiber.fiber.fiber_id,
                        current_generation_or_revision: fiber.fiber.generation,
                        summary_code: "descendant_execution_fiber".to_owned(),
                    });
                }
            }
        }

        let clamped = limit.clamp(1, MAX_SYSTEM_CONTROL_ENUM_LIMIT);
        let truncated = impacts.len() > clamped;
        impacts.truncate(clamped);
        Ok(ControlBlastRadiusPreviewResult {
            target_scope,
            target_id,
            preview_action,
            statutory_lock_blocked: false,
            statutory_lock_code: String::new(),
            impacts,
            truncated,
        })
    }

    fn query_command_status(
        &self,
        control_command_id: [u8; 16],
    ) -> Result<Option<ControlCommandStatusRecord>, SabiFailure> {
        Ok(self.ledger.lookup(control_command_id))
    }
}

/// Maps statutory lock preview action codes to their canonical lock identifiers.
#[must_use]
pub const fn statutory_lock_code_for_preview(preview_action: u32) -> Option<&'static str> {
    match preview_action {
        preview_action::MUTATE_SEARCH_CANDIDATE_LOCK => {
            Some("STATUTORY_LOCK_SEARCH_CANDIDATE_0X1F")
        }
        preview_action::UNDO_IRREVERSIBLE_RECEIPT_LOCK => {
            Some("STATUTORY_LOCK_IRREVERSIBLE_RECEIPT_NO_UNDO")
        }
        preview_action::MUTATE_COMPLETED_OR_ACTIVE_NODE_LOCK => {
            Some("STATUTORY_LOCK_ACTIVE_OR_COMPLETED_NODE_IMMUTABLE")
        }
        preview_action::OVERRIDE_TASK_ATTEMPT_COMMIT_LOCK => {
            Some("STATUTORY_LOCK_TASK_ATTEMPT_NO_MANUAL_COMMIT")
        }
        preview_action::OVERRIDE_EFFECT_UNKNOWN_OPERATION_LOCK => {
            Some("STATUTORY_LOCK_EFFECT_UNKNOWN_NO_MANUAL_OVERRIDE")
        }
        _ => None,
    }
}

impl TaskNodeInspectSource for InMemoryDesktopControlSurface {
    fn inspect_task_node(
        &self,
        plan_id: [u8; 16],
        node_id: [u8; 16],
    ) -> Result<TaskNodeInspection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("task node inspector state poisoned"))?;
        guard
            .task_nodes
            .get(&(plan_id, node_id))
            .cloned()
            .ok_or_else(|| not_found("requested task node was not found"))
    }
}

impl ExecutionFiberInspectSource for InMemoryDesktopControlSurface {
    fn inspect_execution_fiber(
        &self,
        fiber_id: [u8; 16],
        generation: u64,
    ) -> Result<ExecutionFiberInspection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("execution fiber inspector state poisoned"))?;
        let entry = guard
            .execution_fibers
            .get(&fiber_id)
            .ok_or_else(|| not_found("requested execution fiber was not found"))?;
        if entry.fiber.generation != generation {
            return Err(cas_conflict(
                "execution fiber handle generation does not match live generation",
            ));
        }
        Ok(entry.fiber.clone())
    }
}

impl TopicInspectSource for InMemoryDesktopControlSurface {
    fn inspect_topic(&self, topic_id: [u8; 16]) -> Result<TopicInspection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("topic inspector state poisoned"))?;
        guard
            .topics
            .get(&topic_id)
            .map(|entry| entry.topic.clone())
            .ok_or_else(|| not_found("requested topic was not found"))
    }
}

impl OperationInspectSource for InMemoryDesktopControlSurface {
    fn inspect_operation(
        &self,
        operation_id: [u8; 16],
        generation: u64,
    ) -> Result<DurableOperationInspection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("operation inspector state poisoned"))?;
        let op = guard
            .operations
            .get(&operation_id)
            .ok_or_else(|| not_found("requested durable operation was not found"))?;
        if op.generation != generation {
            return Err(cas_conflict(
                "durable operation handle generation does not match live generation",
            ));
        }
        Ok(op.clone())
    }
}

impl ProcessInspector for InMemoryDesktopControlSurface {
    fn inspect_process(&self, process_id: [u8; 16]) -> Result<ProcessInspection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("process inspector state poisoned"))?;
        let p = guard
            .processes
            .get(&process_id)
            .ok_or_else(|| not_found("requested process was not found"))?;
        Ok(ProcessInspection {
            process_id: p.process_id,
            process_generation: p.expected_generation_or_revision,
            agent_instance_id: p.agent_instance_id,
            task_id: p.task_id,
            task_attempt_id: p.task_attempt_id,
            isolation_domain_id: p.isolation_domain_id,
        })
    }
}

impl ResourceInspector for InMemoryDesktopControlSurface {
    fn inspect_resource(
        &self,
        reservation_id: [u8; 16],
    ) -> Result<ResourceInspection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("resource inspector state poisoned"))?;
        let r = guard
            .reservations
            .get(&reservation_id)
            .ok_or_else(|| not_found("requested resource reservation was not found"))?;
        Ok(ResourceInspection {
            reservation_id: r.reservation_id,
            account_id: r.account_id,
            upper_bound: r.requested_ceiling,
            usage_high_water: r.actual_usage,
            consumption_count: u32::try_from(r.expected_generation_or_revision).unwrap_or(1),
        })
    }
}

impl ApplicationInspector for InMemoryDesktopControlSurface {
    fn inspect_application(
        &self,
        package_id: [u8; 16],
    ) -> Result<ApplicationInspection, SabiFailure> {
        let guard = self
            .state
            .lock()
            .map_err(|_| not_found("application inspector state poisoned"))?;
        let app = guard
            .applications
            .values()
            .find(|app| app.package_id == package_id || app.application_id == package_id)
            .ok_or_else(|| not_found("requested application was not found"))?;
        Ok(ApplicationInspection {
            package_id: app.package_id,
            application_id: app.application_id,
            package_manifest_digest: app.package_manifest_digest,
            current_installation_generation: app.current_installation_generation,
            status: u8::try_from(app.status).unwrap_or(1),
            created_at_ms: app.created_at_ms,
            updated_at_ms: app.updated_at_ms,
        })
    }
}

impl InMemoryDesktopControlSurface {
    fn execute_op_arm(
        &self,
        domain: &[u8],
        request: &OperationControlRequest,
        extra: &[u8],
    ) -> Result<ReceiptId, SabiFailure> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| not_found("operation executor state poisoned"))?;
        if let Some(&receipt) = guard.idempotent_receipts.get(&request.idempotency_key) {
            return Ok(receipt);
        }
        let next_gen = if let Some(task) = guard.tasks.get_mut(&request.target_id) {
            if task.expected_generation_or_revision != request.expected_generation_or_revision {
                return Err(cas_conflict("task generation CAS mismatch"));
            }
            task.expected_generation_or_revision =
                task.expected_generation_or_revision.saturating_add(1);
            task.expected_generation_or_revision
        } else if let Some(proc_row) = guard.processes.get_mut(&request.target_id) {
            if proc_row.expected_generation_or_revision != request.expected_generation_or_revision {
                return Err(cas_conflict("process generation CAS mismatch"));
            }
            proc_row.expected_generation_or_revision =
                proc_row.expected_generation_or_revision.saturating_add(1);
            proc_row.expected_generation_or_revision
        } else if let Some(res) = guard.reservations.get_mut(&request.target_id) {
            if res.expected_generation_or_revision != request.expected_generation_or_revision {
                return Err(cas_conflict("resource reservation CAS mismatch"));
            }
            res.expected_generation_or_revision =
                res.expected_generation_or_revision.saturating_add(1);
            res.expected_generation_or_revision
        } else if let Some(group) = guard.task_groups.get_mut(&request.target_id) {
            if group.membership_generation != request.expected_generation_or_revision {
                return Err(cas_conflict("task group generation CAS mismatch"));
            }
            group.membership_generation = group.membership_generation.saturating_add(1);
            group.membership_generation
        } else {
            return Err(not_found("requested operational target was not found"));
        };
        let receipt = derive_executor_receipt_id(
            domain,
            &[
                &request.target_id,
                &request.expected_generation_or_revision.to_be_bytes(),
                extra,
                &request.idempotency_key,
            ],
        );
        guard
            .idempotent_receipts
            .insert(request.idempotency_key, receipt);
        let ts = u64::try_from(request.requested_at_ms.max(0)).unwrap_or(0);
        Self::push_watermark_locked(
            &mut guard,
            ControlScope::Operation,
            request.target_id,
            next_gen,
            ts,
        );
        Ok(receipt)
    }
}

impl OperationCommandExecutor for InMemoryDesktopControlSurface {
    fn pause_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        self.execute_op_arm(b"llmos/surface-pause/v1\0", &request, &[])
    }

    fn resume_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        self.execute_op_arm(b"llmos/surface-resume/v1\0", &request, &[])
    }

    fn cancel_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        self.execute_op_arm(b"llmos/surface-cancel/v1\0", &request, &[])
    }

    fn kill_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        self.execute_op_arm(b"llmos/surface-kill/v1\0", &request, &[])
    }

    fn throttle_operation(
        &self,
        request: OperationControlRequest,
        throttle_percent: u64,
    ) -> Result<ReceiptId, SabiFailure> {
        self.execute_op_arm(
            b"llmos/surface-throttle/v1\0",
            &request,
            &throttle_percent.to_be_bytes(),
        )
    }

    fn reclaim_operation(
        &self,
        request: OperationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        self.execute_op_arm(b"llmos/surface-reclaim/v1\0", &request, &[])
    }
}

impl ApplicationCommandExecutor for InMemoryDesktopControlSurface {
    fn disable_application(
        &self,
        request: ApplicationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| not_found("application executor state poisoned"))?;
        if let Some(&receipt) = guard.idempotent_receipts.get(&request.idempotency_key) {
            return Ok(receipt);
        }
        let app = guard
            .applications
            .values_mut()
            .find(|app| {
                app.package_id == request.package_id || app.application_id == request.package_id
            })
            .ok_or_else(|| not_found("requested application package was not found"))?;
        if app.current_installation_generation != request.expected_generation_or_revision {
            return Err(cas_conflict(
                "application installation generation CAS mismatch",
            ));
        }
        app.status = 2;
        app.current_installation_generation = app.current_installation_generation.saturating_add(1);
        let new_gen = app.current_installation_generation;
        let app_id = app.application_id;
        let ts = u64::try_from(request.requested_at_ms.max(0)).unwrap_or(0);
        app.updated_at_ms = ts;
        let receipt = derive_executor_receipt_id(
            b"llmos/surface-app-disable/v1\0",
            &[
                &request.package_id,
                &request.expected_generation_or_revision.to_be_bytes(),
                &request.idempotency_key,
            ],
        );
        guard
            .idempotent_receipts
            .insert(request.idempotency_key, receipt);
        Self::push_watermark_locked(&mut guard, ControlScope::Application, app_id, new_gen, ts);
        Ok(receipt)
    }

    fn uninstall_application(
        &self,
        request: ApplicationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| not_found("application executor state poisoned"))?;
        if let Some(&receipt) = guard.idempotent_receipts.get(&request.idempotency_key) {
            return Ok(receipt);
        }
        let app = guard
            .applications
            .values_mut()
            .find(|app| {
                app.package_id == request.package_id || app.application_id == request.package_id
            })
            .ok_or_else(|| not_found("requested application package was not found"))?;
        if app.current_installation_generation != request.expected_generation_or_revision {
            return Err(cas_conflict(
                "application installation generation CAS mismatch",
            ));
        }
        app.status = 3;
        app.current_installation_generation = app.current_installation_generation.saturating_add(1);
        let new_gen = app.current_installation_generation;
        let app_id = app.application_id;
        let ts = u64::try_from(request.requested_at_ms.max(0)).unwrap_or(0);
        app.updated_at_ms = ts;
        let receipt = derive_executor_receipt_id(
            b"llmos/surface-app-uninstall/v1\0",
            &[
                &request.package_id,
                &request.expected_generation_or_revision.to_be_bytes(),
                &request.idempotency_key,
            ],
        );
        guard
            .idempotent_receipts
            .insert(request.idempotency_key, receipt);
        Self::push_watermark_locked(&mut guard, ControlScope::Application, app_id, new_gen, ts);
        Ok(receipt)
    }
}

/// Suppresses unused-import warnings when helper enum re-exports are referenced
/// across feature matrices.
#[allow(dead_code)]
const fn _assert_wire_enums_linked(
    _op: DurableOperationState,
    _fs: ExecutionFiberLifecycleState,
    _fp: ExecutionFiberPhase,
    _nk: PlanNodeKind,
    _ns: PlanNodeLifecycleState,
) {
}
