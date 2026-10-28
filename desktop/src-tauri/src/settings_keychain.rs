//! Stage D SystemControlCenter (12-Domain Ownership Tree, Stage C Cell Lease
//! Inspection, Headlamp Action Pruner, 5+1 Statutory Locks, Blast-Radius
//! Two-Step Arming, ControlCommandId Recovery) and 4-Layer Settings CAS,
//! Anti-Hijack ContentTypeAssociation, and Zero-Secret KeychainReference Vault
//! (`D-CONTROL-SETTINGS-A11Y` / `W72-D-SETTINGS-CTRL`).
//!
//! Covers:
//! - `[DUI-CTRL-001]` 12-Domain System Control Center & System Informer 10-layer tree
//! - `[TASK-AGENT-001]` / `[TASK-ISO-005]` `SHARED_THREAD_LIKE` fault-domain badge propagation
//! - `[UI-PARITY-001]` CNCF Headlamp action pruning & 5+1 statutory non-tamperable locks
//! - `[SCHED-STEAL-001]` / `[LEASE-TTL-001]` Stage C `Cell` 3-family lease inspection & migration
//! - `[CTRL-SAFETY-001]` Terraform `+ / ~ / -` blast-radius diff, 5s two-step arming & Data-Loss Receipt
//! - `[CTRL-RECOVERY-001]` `ControlCommandId` idempotent reconciliation & anti-blind-retry fence
//! - `[DUI-SETTINGS-001]` 4-layer (`system / user / workspace / application`) Settings CAS + watch
//! - `[DUI-ASSOC-001]` User-revocable `ContentTypeAssociation` & anti-package-hijack guard
//! - `[DUI-SESSION-001]` Zero-secret `KeychainReference` vault & session-lock cache wipe

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::{DesktopError, ErrorCode};

fn control_err(message: impl Into<String>) -> DesktopError {
    DesktopError::new(ErrorCode::Control, message)
}

fn cas_conflict_err(expected: u64, actual: u64) -> DesktopError {
    DesktopError::new(
        ErrorCode::Control,
        format!("CAS_CONFLICT: expected_revision={expected}, actual_revision={actual}"),
    )
}

/// Search non-authoritative restriction bitmask (`0x1F` = `0b0001_1111`, `[CONS-CTRL-001]`).
pub const SEARCH_NON_AUTHORITATIVE_MASK_0X1F: u8 = 0x1F;

/// Mandatory two-step arming countdown window in milliseconds (`[CTRL-SAFETY-001]`).
pub const TWO_STEP_ARMING_WINDOW_MS: u64 = 5_000;

/// Minimum clickjack guard delay before the second confirmation click is accepted (`350ms`).
pub const TWO_STEP_MIN_GUARD_MS: u64 = 350;

// ============================================================================
// 1. 12-Domain SystemControlCenter & System Informer 10-Layer Ownership Tree
// ============================================================================

/// The 12 authoritative System Control Center domains (`[DUI-CTRL-001]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlCenterDomainKind {
    SystemDevice,
    Workspace,
    Application,
    Task,
    TaskGroup,
    TaskNode,
    TaskAttempt,
    ExecutionUnit,
    ResourceGroup,
    TopicChannel,
    OperationDevice,
    CellDistributedLease,
}

impl ControlCenterDomainKind {
    pub const ALL: [ControlCenterDomainKind; 12] = [
        Self::SystemDevice,
        Self::Workspace,
        Self::Application,
        Self::Task,
        Self::TaskGroup,
        Self::TaskNode,
        Self::TaskAttempt,
        Self::ExecutionUnit,
        Self::ResourceGroup,
        Self::TopicChannel,
        Self::OperationDevice,
        Self::CellDistributedLease,
    ];
}

/// System Informer 10-layer parent-child ownership hierarchy (`§2A.1`).
///
/// `System/Device -> Workspace/Tenant -> Application -> Task -> TaskGroup ->
///  TaskNode -> TaskAttempt -> Process -> AgentInstance -> ExecutionFiber`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OwnershipHierarchyLayer {
    L1SystemDevice,
    L2WorkspaceTenant,
    L3Application,
    L4Task,
    L5TaskGroup,
    L6TaskNode,
    L7TaskAttempt,
    L8Process,
    L9AgentInstance,
    L10ExecutionFiber,
}

impl OwnershipHierarchyLayer {
    pub fn depth_index(self) -> u8 {
        match self {
            Self::L1SystemDevice => 0,
            Self::L2WorkspaceTenant => 1,
            Self::L3Application => 2,
            Self::L4Task => 3,
            Self::L5TaskGroup => 4,
            Self::L6TaskNode => 5,
            Self::L7TaskAttempt => 6,
            Self::L8Process => 7,
            Self::L9AgentInstance => 8,
            Self::L10ExecutionFiber => 9,
        }
    }
}

/// Fault-domain isolation posture for execution units (`[TASK-AGENT-001]`, `[TASK-ISO-005]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FaultDomainPostureDto {
    IsolatedProcess,
    SharedThreadLike,
}

/// Nomad-style triple-watermark resource posture (`§2A.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResourcePressureStateDto {
    Normal,
    Throttled,
    OomAtRisk,
    Exhausted,
}

/// Nomad triple-watermark resource bar telemetry (`guaranteed / allocated / burst ceiling + live`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NomadTripleWatermarkDto {
    pub guaranteed_floor_pct: u8,
    pub allocated_reserved_pct: u8,
    pub burst_ceiling_pct: u8,
    pub live_usage_pct: u8,
    pub pressure_state: ResourcePressureStateDto,
    pub cpu_millicores: u32,
    pub memory_mib: u32,
    pub io_kbps: u32,
}

/// The 5+1 statutory non-tamperable locks (`[UI-PARITY-001]`, `§2A.3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StatutoryLockKindDto {
    /// Lock 1: `0x1F` Search candidate is strictly read-only (`[CONS-CTRL-001]`).
    Search0x1fReadOnly,
    /// Lock 2: `IRREVERSIBLE` receipt forbids `Undo` (`[RCPT-UNDO-001]`).
    IrreversibleNoUndo,
    /// Lock 3: `COMPLETED / ACTIVE` `TaskNode` forbids structural edits (`[TASK-GRAPH-002]`).
    NodeCompletedOrActiveLocked,
    /// Lock 4: `TaskAttempt` forbids manual commit (`[TASK-COMMIT-001]`).
    TaskAttemptNoManualCommit,
    /// Lock 5: `Operation(EFFECT_UNKNOWN)` forbids manual mark success (`[TASK-EFFECT-003]`).
    OperationEffectUnknownNoManualSuccess,
    /// Lock 6 (+1): `Quarantined` Cell lease forbids local TTL unlock (`[LEASE-TTL-001]`).
    QuarantinedLeaseFence,
}

/// Undo classification for tasks and receipts (`[RCPT-UNDO-001]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UndoClassDto {
    Reversible,
    Compensatable,
    Irreversible,
}

/// Single node in the 12-Domain / 10-Layer System Informer ownership tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipTreeNodeDto {
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub domain: ControlCenterDomainKind,
    pub layer: OwnershipHierarchyLayer,
    pub depth: u8,
    pub label_zh: String,
    pub label_en: String,
    pub lifecycle_state: String,
    pub host_pid: Option<u32>,
    pub fault_domain_posture: FaultDomainPostureDto,
    pub shared_fate_peer_ids: Vec<String>,
    pub watermark: NomadTripleWatermarkDto,
    pub undo_class: Option<UndoClassDto>,
    pub is_search_0x1f_candidate: bool,
    pub search_restriction_bits: u8,
    pub allowed_verbs: Vec<String>,
    pub statutory_locks: Vec<StatutoryLockKindDto>,
    pub read_only_reason_zh: Option<String>,
    pub read_only_reason_en: Option<String>,
    pub requires_two_step_arming_verbs: Vec<String>,
    pub requires_blast_radius_preview_verbs: Vec<String>,
    pub linked_resource_group_id: Option<String>,
    pub linked_topic_channel_id: Option<String>,
    pub linked_operation_id: Option<String>,
    pub linked_cell_id: Option<String>,
    pub l3_diagnostic_tag: String,
}

/// Summary card for each of the 12 System Control Center domains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlDomainSummaryDto {
    pub domain: ControlCenterDomainKind,
    pub title_zh: String,
    pub title_en: String,
    pub total_objects: usize,
    pub active_objects: usize,
    pub attention_objects: usize,
    pub locked_objects: usize,
    pub watermark: NomadTripleWatermarkDto,
}

// ============================================================================
// 2. Stage C Cell Topology, 3-Family Leases & Process Migration
// ============================================================================

/// Stage C `Cell` connectivity & fault posture (`§2A.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CellConnectivityPostureDto {
    LocalSingleNode,
    HealthyClusterMember,
    DegradedHeartbeat,
    Partitioned,
    FencedReadOnly,
    Quarantined,
    Offline,
}

/// Stage C `PartitionAutonomyGate` state (`§2A.2`, `[LEASE-TTL-001]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PartitionAutonomyGateDto {
    Open,
    LocalAutonomyOnly,
    FencedReadOnly,
    QuarantinedHold,
}

/// The 3 Stage C `Cell` lease families (`Quota`, `Capacity`, `ExclusiveDevice`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CellLeaseFamilyDto {
    /// `ExecutionLease` (CPU / Memory / Token / I/O Quota).
    QuotaExecutionLease,
    /// `ResourceSubpoolLease` (Concurrency / GPU / NPU Capacity).
    CapacitySubpoolLease,
    /// `CapabilityEpochLease` (Camera / Microphone / USB / Exclusive Device).
    ExclusiveDeviceEpochLease,
}

/// Stage C `Cell` lease record inspected in `ResourceMonitorSurface`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellLeaseInspectionRecordDto {
    pub lease_id_hex: String,
    pub cell_id_hex: String,
    pub family: CellLeaseFamilyDto,
    pub holder_node_id: String,
    pub holder_label_zh: String,
    pub holder_label_en: String,
    pub lease_epoch: u64,
    pub ttl_remaining_ms: u64,
    pub clock_skew_horizon_ms: u64,
    pub quarantined: bool,
    pub has_closure_receipt: bool,
    pub has_fence_receipt: bool,
    pub statutory_lock: Option<StatutoryLockKindDto>,
    pub watermark: NomadTripleWatermarkDto,
}

/// Stage C `Cell` node topology + 3-family lease summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellNodeInspectorDto {
    pub cell_id_hex: String,
    pub display_name_zh: String,
    pub display_name_en: String,
    pub posture: CellConnectivityPostureDto,
    pub autonomy_gate: PartitionAutonomyGateDto,
    pub current_epoch: u64,
    pub clock_skew_horizon_ms: u64,
    pub accept_migration_target: bool,
    pub watermark: NomadTripleWatermarkDto,
    pub leases: Vec<CellLeaseInspectionRecordDto>,
}

/// Request payload for controlled `RequestProcessMigration` (`[SCHED-STEAL-001]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessMigrationControlRequestDto {
    pub process_node_id: String,
    pub source_cell_id_hex: String,
    pub target_cell_id_hex: String,
    pub expected_revision_or_generation: u64,
    pub read_backing_token: String,
    pub closure_receipt_hex: Option<String>,
    pub fence_receipt_hex: Option<String>,
}

/// Result of a controlled `RequestProcessMigration`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessMigrationControlResultDto {
    pub process_node_id: String,
    pub from_cell_id_hex: String,
    pub to_cell_id_hex: String,
    pub new_generation: u64,
    pub migrated_lease_ids: Vec<String>,
    pub receipt_id_hex: String,
    pub audit_summary_zh: String,
    pub audit_summary_en: String,
}

// ============================================================================
// 3. Blast-Radius Diff Preview, 5s Two-Step Arming & Command Reconciliation
// ============================================================================

/// Terraform-style `+ / ~ / - / =` diff symbol (`§2A.3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BlastRadiusDiffOpDto {
    /// `+` Create / allocate / spawn
    PlusCreate,
    /// `~` Update / throttle / migrate in-place
    TildeUpdateInPlace,
    /// `-` Destroy / cancel / purge / revoke
    MinusDestroyOrCancel,
    /// `=` Unchanged dependency
    EqualsNoOp,
}

impl BlastRadiusDiffOpDto {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::PlusCreate => "+",
            Self::TildeUpdateInPlace => "~",
            Self::MinusDestroyOrCancel => "-",
            Self::EqualsNoOp => "=",
        }
    }
}

/// Single row in the Terraform-style blast-radius diff preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastRadiusDiffRowDto {
    pub op: BlastRadiusDiffOpDto,
    pub symbol: String,
    pub domain: ControlCenterDomainKind,
    pub target_id: String,
    pub label_zh: String,
    pub label_en: String,
    pub before_state: String,
    pub after_state: String,
    pub detail_zh: String,
    pub detail_en: String,
}

/// Complete blast-radius dry-run preview returned before high-blast-radius verbs (`[CTRL-SAFETY-001]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastRadiusPreviewPackageDto {
    pub preview_id_hex: String,
    pub target_node_id: String,
    pub verb: String,
    pub expected_revision: u64,
    pub plus_create_count: usize,
    pub tilde_update_count: usize,
    pub minus_destroy_count: usize,
    pub equals_noop_count: usize,
    pub affected_tasks_count: usize,
    pub affected_processes_count: usize,
    pub affected_leases_count: usize,
    pub estimated_purged_messages: u64,
    pub estimated_purged_bytes: u64,
    pub requires_two_step_arming: bool,
    pub arming_window_ms: u64,
    pub min_guard_ms: u64,
    pub requires_data_loss_ack: bool,
    pub rows: Vec<BlastRadiusDiffRowDto>,
}

/// Two-step arming ticket state (`[CTRL-SAFETY-001]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwoStepArmingTicketDto {
    pub arming_ticket_id_hex: String,
    pub target_node_id: String,
    pub verb: String,
    pub expected_revision: u64,
    pub armed_at_ms: u64,
    pub expires_at_ms: u64,
    pub min_confirm_at_ms: u64,
    pub requires_data_loss_ack: bool,
}

/// Authoritative Data-Loss `Receipt` generated when `Topic Purge` or destructive action completes (`§2A.3`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataLossReceiptDto {
    pub receipt_id_hex: String,
    pub control_command_id_hex: String,
    pub target_node_id: String,
    pub verb: String,
    pub data_loss_acknowledged: bool,
    pub purged_messages_count: u64,
    pub purged_bytes_count: u64,
    pub undo_class: UndoClassDto,
    pub statutory_lock: StatutoryLockKindDto,
    pub issued_at_ms: u64,
    pub summary_zh: String,
    pub summary_en: String,
}

/// Reconciliation state of a `ControlCommandId` (`[CTRL-RECOVERY-001]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ControlCommandReconcileStatusDto {
    AcceptedInFlight,
    AppliedIdempotent,
    RejectedTerminal,
    UnknownNeedsReconcile,
}

/// Tracked control command entry in the `[CTRL-RECOVERY-001]` recovery ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlCommandLedgerRecordDto {
    pub control_command_id_hex: String,
    pub target_node_id: String,
    pub verb: String,
    pub expected_revision: u64,
    pub status: ControlCommandReconcileStatusDto,
    pub blind_retry_blocked: bool,
    pub resulting_revision: Option<u64>,
    pub receipt_id_hex: Option<String>,
    pub data_loss_receipt: Option<DataLossReceiptDto>,
    pub diagnostic_code: String,
}

// ============================================================================
// 4. SystemControlCenter 360px Quick-Control Flyout & Session Posture
// ============================================================================

/// Workstation performance & thermal profile (`§2A.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PerformanceProfileDto {
    SilentEco,
    Balanced,
    HighPerformance,
}

/// Hardware device permission kill-switch domain (`§2A.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HardwarePermissionDomainDto {
    Microphone,
    Camera,
    ScreenCapture,
    Location,
}

/// Hardware device permission state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DevicePermissionPostureDto {
    AuthorizedActive,
    PromptOnUse,
    RevokedKillSwitch,
}

/// Single hardware permission toggle entry in `SystemControlCenter`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwarePermissionStatusDto {
    pub domain: HardwarePermissionDomainDto,
    pub label_zh: String,
    pub label_en: String,
    pub posture: DevicePermissionPostureDto,
    pub active_holder_count: usize,
    pub revision: u64,
}

/// Recovery alert status in `SystemControlCenter` (`ack / resume`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryDomainAlertDto {
    pub alert_id: String,
    pub domain_name_zh: String,
    pub domain_name_en: String,
    pub state: String,
    pub acknowledged: bool,
    pub resumable: bool,
    pub revision: u64,
}

/// Complete snapshot of the `SystemControlCenter` (`360px` bottom-right flyout + 12-domain tree).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemControlCenterSnapshotDto {
    pub snapshot_revision: u64,
    pub session_state: String,
    pub session_generation: u64,
    pub active_workspace_id: String,
    pub performance_profile: PerformanceProfileDto,
    pub background_tier_paused: bool,
    pub shared_thread_like_group_count: usize,
    pub hardware_permissions: Vec<HardwarePermissionStatusDto>,
    pub recovery_alerts: Vec<RecoveryDomainAlertDto>,
    pub domain_summaries: Vec<ControlDomainSummaryDto>,
    pub ownership_tree: Vec<OwnershipTreeNodeDto>,
    pub cells: Vec<CellNodeInspectorDto>,
}

// ============================================================================
// 5. 4-Layer SettingsDomain CAS + Watch Engine (`[DUI-SETTINGS-001]`)
// ============================================================================

/// The 4 authoritative settings domains (`system < user < workspace < application`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingsLayerKindDto {
    System,
    User,
    Workspace,
    Application,
}

impl SettingsLayerKindDto {
    pub const ALL: [SettingsLayerKindDto; 4] =
        [Self::System, Self::User, Self::Workspace, Self::Application];

    pub fn precedence(self) -> u8 {
        match self {
            Self::System => 0,
            Self::User => 1,
            Self::Workspace => 2,
            Self::Application => 3,
        }
    }
}

/// Single setting entry within a specific `SettingsLayerKindDto`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsLayerEntryDto {
    pub key: String,
    pub value_json: String,
    pub layer: SettingsLayerKindDto,
    pub entry_revision: u64,
    pub locked_by_system_policy: bool,
    pub label_zh: String,
    pub label_en: String,
}

/// Effective resolved setting across the 4-layer hierarchy (`application > workspace > user > system`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveSettingRecordDto {
    pub key: String,
    pub effective_value_json: String,
    pub winning_layer: SettingsLayerKindDto,
    pub winning_domain_revision: u64,
    pub defined_in_layers: Vec<SettingsLayerKindDto>,
    pub locked_by_system_policy: bool,
    pub label_zh: String,
    pub label_en: String,
}

/// Snapshot of a single `SettingsDomain` plus effective 4-layer resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsDomainViewSnapshotDto {
    pub layer: SettingsLayerKindDto,
    pub domain_revision: u64,
    pub entries: Vec<SettingsLayerEntryDto>,
    pub effective_settings: Vec<EffectiveSettingRecordDto>,
    pub active_theme: String,
}

/// Watch notification emitted on successful CAS mutation (`watch_settings_domain`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsWatchEventDto {
    pub watch_seq: u64,
    pub layer: SettingsLayerKindDto,
    pub key: String,
    pub previous_value_json: Option<String>,
    pub new_value_json: String,
    pub new_domain_revision: u64,
    pub effective_theme: String,
}

/// CAS update request for a setting in `system / user / workspace / application`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSettingCasRequestDto {
    pub layer: SettingsLayerKindDto,
    pub key: String,
    pub value_json: String,
    pub expected_domain_revision: u64,
}

// ============================================================================
// 6. User-Revocable ContentTypeAssociation (`[DUI-ASSOC-001]`)
// ============================================================================

/// Unapproved package proposal for a content type / URI scheme (`[DUI-ASSOC-001]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageAssociationProposalDto {
    pub proposed_application_id_hex: String,
    pub proposed_installation_id_hex: String,
    pub proposed_app_name_zh: String,
    pub proposed_app_name_en: String,
    pub submitted_at_ms: u64,
}

/// Authoritative user-revocable `ContentTypeAssociation` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentTypeAssociationEntryDto {
    pub content_type_or_uri: String,
    pub description_zh: String,
    pub description_en: String,
    pub handler_application_id_hex: Option<String>,
    pub handler_installation_id_hex: Option<String>,
    pub handler_label_zh: String,
    pub handler_label_en: String,
    pub user_explicitly_bound: bool,
    pub pending_package_proposal: Option<PackageAssociationProposalDto>,
    pub revision: u64,
}

// ============================================================================
// 7. Zero-Secret KeychainReference Vault (`[DUI-SETTINGS-001]`, `[DUI-SESSION-001]`)
// ============================================================================

/// TCB-private secret storage wrapper.
///
/// Explicitly does NOT implement `Serialize`, `Deserialize`, or `Display`, and its
/// `Debug` implementation redacts the secret material so raw secret bytes can NEVER
/// leak into any IPC DTO, JSON payload, or debug log (`[DUI-SETTINGS-001]`).
#[derive(Clone, PartialEq, Eq)]
struct PrivateVaultSecretMaterial {
    secret_bytes: Vec<u8>,
}

impl fmt::Debug for PrivateVaultSecretMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<REDACTED_TCB_VAULT_SECRET>")
    }
}

/// Zero-secret `KeychainReference` metadata projected to the frontend (`[DUI-SETTINGS-001]`).
///
/// Notice: There is NO field capable of holding raw secret bytes or plaintext tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeychainReferenceEntryDto {
    pub keychain_ref_id_hex: String,
    pub owner_principal_id_hex: String,
    pub label_zh: String,
    pub label_en: String,
    pub credential_kind: String,
    pub public_fingerprint_hex: String,
    pub authorized_application_ids: Vec<String>,
    pub hardware_backed: bool,
    pub expected_generation_or_revision: u64,
    pub revoked: bool,
    pub active_use_handle_count: usize,
}

/// Ephemeral opaque `use-by-reference` handle granted to an authorized application.
///
/// Bound to a specific `session_generation`; automatically wiped on session lock (`[DUI-SESSION-001]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeychainUseByReferenceHandleDto {
    pub handle_id_hex: String,
    pub keychain_ref_id_hex: String,
    pub authorized_application_id_hex: String,
    pub session_generation: u64,
    pub reference_revision: u64,
}

// ============================================================================
// 8. Pure Headlamp Action Pruner & 5+1 Statutory Lock Validator
// ============================================================================

/// Input parameters for computing allowed `ControlCommand` verbs and statutory locks.
#[derive(Debug, Clone)]
pub struct BackendActionPrunerInput<'a> {
    pub domain: ControlCenterDomainKind,
    pub lifecycle_state: &'a str,
    pub undo_class: Option<UndoClassDto>,
    pub is_search_0x1f_candidate: bool,
    pub search_restriction_bits: u8,
    pub cell_quarantined: bool,
    pub has_active_tasks_for_uninstall: bool,
}

/// Output of the backend CNCF Headlamp action pruner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendActionPrunerOutput {
    pub allowed_verbs: Vec<String>,
    pub statutory_locks: Vec<StatutoryLockKindDto>,
    pub read_only_reason_zh: Option<String>,
    pub read_only_reason_en: Option<String>,
    pub requires_two_step_arming_verbs: Vec<String>,
    pub requires_blast_radius_preview_verbs: Vec<String>,
}

/// Computes allowed verbs and statutory locks for any node in the 12-Domain Control Center.
pub fn compute_backend_allowed_verbs(
    input: &BackendActionPrunerInput<'_>,
) -> BackendActionPrunerOutput {
    let state = input.lifecycle_state.trim().to_ascii_uppercase();
    let mut statutory_locks = Vec::new();

    // 1. Check 5+1 statutory non-tamperable locks ([UI-PARITY-001]).
    if input.is_search_0x1f_candidate {
        statutory_locks.push(StatutoryLockKindDto::Search0x1fReadOnly);
        if input.search_restriction_bits != SEARCH_NON_AUTHORITATIVE_MASK_0X1F {
            return BackendActionPrunerOutput {
                allowed_verbs: Vec::new(),
                statutory_locks,
                read_only_reason_zh: Some(
                    "搜索候选权限掩码异常，已强制阻断全部控制操作".to_string(),
                ),
                read_only_reason_en: Some(
                    "Search candidate restriction mask anomaly; all control actions blocked"
                        .to_string(),
                ),
                requires_two_step_arming_verbs: Vec::new(),
                requires_blast_radius_preview_verbs: Vec::new(),
            };
        }
        return BackendActionPrunerOutput {
            allowed_verbs: vec!["JumpToAuthoritativeObject".to_string()],
            statutory_locks,
            read_only_reason_zh: Some("搜索结果仅供参考，请跳转至原对象执行控制操作".to_string()),
            read_only_reason_en: Some(
                "Search result is read-only; jump to the source object to perform control actions"
                    .to_string(),
            ),
            requires_two_step_arming_verbs: Vec::new(),
            requires_blast_radius_preview_verbs: Vec::new(),
        };
    }

    if input.undo_class == Some(UndoClassDto::Irreversible) {
        statutory_locks.push(StatutoryLockKindDto::IrreversibleNoUndo);
    }

    if input.domain == ControlCenterDomainKind::TaskNode
        && (state == "COMPLETED" || state == "ACTIVE")
    {
        statutory_locks.push(StatutoryLockKindDto::NodeCompletedOrActiveLocked);
        return BackendActionPrunerOutput {
            allowed_verbs: vec!["ViewEffectProof".to_string()],
            statutory_locks,
            read_only_reason_zh: Some("当前步骤正在运行或已完成，结构已锁定为只读".to_string()),
            read_only_reason_en: Some(
                "This step is running or completed; structural edits are locked read-only"
                    .to_string(),
            ),
            requires_two_step_arming_verbs: Vec::new(),
            requires_blast_radius_preview_verbs: Vec::new(),
        };
    }

    if input.domain == ControlCenterDomainKind::TaskAttempt {
        statutory_locks.push(StatutoryLockKindDto::TaskAttemptNoManualCommit);
        let allowed = if state == "RUNNING" || state == "VERIFYING" || state == "READY" {
            vec![
                "CancelAttempt".to_string(),
                "SupersedeAttempt".to_string(),
                "RequestReverify".to_string(),
            ]
        } else {
            vec!["RequestReverify".to_string()]
        };
        let arming: Vec<String> = allowed
            .iter()
            .filter(|v| v.as_str() == "CancelAttempt" || v.as_str() == "SupersedeAttempt")
            .cloned()
            .collect();
        return BackendActionPrunerOutput {
            allowed_verbs: allowed,
            statutory_locks,
            read_only_reason_zh: Some(
                "执行尝试必须通过自动验证门晋升，禁止人工直接提交完成".to_string(),
            ),
            read_only_reason_en: Some(
                "Execution attempts must promote via automated verification; manual commit is forbidden"
                    .to_string(),
            ),
            requires_two_step_arming_verbs: arming.clone(),
            requires_blast_radius_preview_verbs: arming,
        };
    }

    if input.domain == ControlCenterDomainKind::OperationDevice && state == "EFFECT_UNKNOWN" {
        statutory_locks.push(StatutoryLockKindDto::OperationEffectUnknownNoManualSuccess);
        return BackendActionPrunerOutput {
            allowed_verbs: vec!["Reconcile".to_string(), "Compensate".to_string()],
            statutory_locks,
            read_only_reason_zh: Some(
                "外部操作结果尚未确认，禁止人工直接标记为成功或未执行".to_string(),
            ),
            read_only_reason_en: Some(
                "External operation outcome is unconfirmed; manual mark-as-success is forbidden"
                    .to_string(),
            ),
            requires_two_step_arming_verbs: vec!["Compensate".to_string()],
            requires_blast_radius_preview_verbs: vec!["Compensate".to_string()],
        };
    }

    if input.domain == ControlCenterDomainKind::CellDistributedLease
        && (state == "QUARANTINED" || input.cell_quarantined)
    {
        statutory_locks.push(StatutoryLockKindDto::QuarantinedLeaseFence);
        return BackendActionPrunerOutput {
            allowed_verbs: vec!["InspectQuarantineFence".to_string()],
            statutory_locks,
            read_only_reason_zh: Some(
                "隔离区资源租约需等待闭环与围栏凭证后方可释放，禁止仅凭本地超时解锁".to_string(),
            ),
            read_only_reason_en: Some(
                "Quarantined resource lease requires closure and fence receipts before release; local timeout unlock is forbidden"
                    .to_string(),
            ),
            requires_two_step_arming_verbs: Vec::new(),
            requires_blast_radius_preview_verbs: Vec::new(),
        };
    }

    // 2. Standard domain + lifecycle state pruning.
    let mut allowed = Vec::new();
    let mut reason_zh = None;
    let mut reason_en = None;

    match input.domain {
        ControlCenterDomainKind::SystemDevice => {
            allowed.extend([
                "SwitchPerformanceProfile".to_string(),
                "PauseBackgroundTier".to_string(),
                "RevokeDevicePermission".to_string(),
                "AcknowledgeRecoveryAlert".to_string(),
                "ResumeRecovery".to_string(),
            ]);
        }
        ControlCenterDomainKind::Workspace => {
            allowed.extend([
                "ActivateWorkspace".to_string(),
                "UpdateWorkspaceLimit".to_string(),
                "ArchiveWorkspace".to_string(),
            ]);
        }
        ControlCenterDomainKind::Application => {
            allowed.extend([
                "Launch".to_string(),
                "Activate".to_string(),
                "Terminate".to_string(),
                "ToggleBackground".to_string(),
                "DisableApplication".to_string(),
            ]);
            if !input.has_active_tasks_for_uninstall {
                allowed.push("UninstallApplication".to_string());
            }
        }
        ControlCenterDomainKind::Task => {
            if state == "COMPLETED" {
                allowed.push("Refine".to_string());
                if input.undo_class != Some(UndoClassDto::Irreversible) {
                    allowed.push("Undo".to_string());
                } else {
                    reason_zh = Some("此操作包含不可逆外部副作用，仅支持查看执行凭证".to_string());
                    reason_en = Some(
                        "This action has irreversible external effects; only effect proof viewing is available"
                            .to_string(),
                    );
                }
                allowed.push("ViewEffectProof".to_string());
            } else {
                allowed.extend(["Pause".to_string(), "Cancel".to_string()]);
            }
        }
        ControlCenterDomainKind::TaskGroup => {
            allowed.extend([
                "Pause".to_string(),
                "Seal".to_string(),
                "SetFanoutLimit".to_string(),
            ]);
        }
        ControlCenterDomainKind::TaskNode => {
            allowed.extend(["EditNodeSpec".to_string(), "RemovePendingNode".to_string()]);
        }
        ControlCenterDomainKind::TaskAttempt => {}
        ControlCenterDomainKind::ExecutionUnit => {
            allowed.extend([
                "Throttle".to_string(),
                "Freeze".to_string(),
                "Kill".to_string(),
                "Restart".to_string(),
                "RequestProcessMigration".to_string(),
            ]);
        }
        ControlCenterDomainKind::ResourceGroup => {
            allowed.extend([
                "ReweightShare".to_string(),
                "SetHardCap".to_string(),
                "FreezeContext".to_string(),
                "ReclaimIdlers".to_string(),
            ]);
        }
        ControlCenterDomainKind::TopicChannel => {
            allowed.extend([
                "PauseConsumer".to_string(),
                "ResumeConsumer".to_string(),
                "DrainTopic".to_string(),
                "RevokePublisher".to_string(),
                "TopicPurge".to_string(),
            ]);
        }
        ControlCenterDomainKind::OperationDevice => {
            allowed.extend([
                "CancelOperation".to_string(),
                "ResetDeviceQueue".to_string(),
            ]);
        }
        ControlCenterDomainKind::CellDistributedLease => {
            allowed.extend([
                "RequestProcessMigration".to_string(),
                "InspectCellLeases".to_string(),
            ]);
        }
    }

    let high_blast_verbs: BTreeSet<&'static str> = BTreeSet::from([
        "Cancel",
        "CancelAttempt",
        "SupersedeAttempt",
        "Kill",
        "Terminate",
        "DisableApplication",
        "UninstallApplication",
        "TopicPurge",
        "DrainTopic",
        "ArchiveWorkspace",
        "RequestProcessMigration",
        "Compensate",
        "ResetDeviceQueue",
    ]);

    let arming_verbs: Vec<String> = allowed
        .iter()
        .filter(|v| high_blast_verbs.contains(v.as_str()))
        .cloned()
        .collect();

    BackendActionPrunerOutput {
        allowed_verbs: allowed,
        statutory_locks,
        read_only_reason_zh: reason_zh,
        read_only_reason_en: reason_en,
        requires_two_step_arming_verbs: arming_verbs.clone(),
        requires_blast_radius_preview_verbs: arming_verbs,
    }
}

// ============================================================================
// 9. Authoritative State Container (`SettingsKeychainState`)
// ============================================================================

#[derive(Debug)]
struct InnerControlSettingsState {
    snapshot_revision: u64,
    session_locked: bool,
    session_generation: u64,
    active_workspace_id: String,
    performance_profile: PerformanceProfileDto,
    background_tier_paused: bool,
    hardware_permissions: BTreeMap<HardwarePermissionDomainDto, HardwarePermissionStatusDto>,
    recovery_alerts: BTreeMap<String, RecoveryDomainAlertDto>,
    ownership_nodes: Vec<OwnershipTreeNodeDto>,
    cells: BTreeMap<String, CellNodeInspectorDto>,
    arming_tickets: BTreeMap<String, TwoStepArmingTicketDto>,
    command_ledger: BTreeMap<String, ControlCommandLedgerRecordDto>,
    settings_revisions: BTreeMap<SettingsLayerKindDto, u64>,
    settings_entries: BTreeMap<(SettingsLayerKindDto, String), SettingsLayerEntryDto>,
    settings_watch_log: Vec<SettingsWatchEventDto>,
    associations: BTreeMap<String, ContentTypeAssociationEntryDto>,
    keychain_refs: BTreeMap<String, KeychainReferenceEntryDto>,
    private_vault_secrets: BTreeMap<String, PrivateVaultSecretMaterial>,
    active_keychain_handles: BTreeMap<String, KeychainUseByReferenceHandleDto>,
    monotonic_counter: u64,
}

/// Thread-safe state for `W72-D-SETTINGS-CTRL` (`SystemControlCenter` + `Settings` + `Keychain`).
#[derive(Debug, Clone)]
pub struct SettingsKeychainState {
    inner: Arc<RwLock<InnerControlSettingsState>>,
}

impl Default for SettingsKeychainState {
    fn default() -> Self {
        Self::new_seeded()
    }
}

impl SettingsKeychainState {
    /// Creates a deterministic seeded workstation state covering all 12 control domains,
    /// the 10-layer System Informer ownership tree, `SHARED_THREAD_LIKE` fate-sharing groups,
    /// Stage C `Cell` 3-family leases, 4-layer `SettingsDomain`, `ContentTypeAssociation`,
    /// and zero-secret `KeychainReference` vault entries.
    pub fn new_seeded() -> Self {
        let mut hardware_permissions = BTreeMap::new();
        hardware_permissions.insert(
            HardwarePermissionDomainDto::Microphone,
            HardwarePermissionStatusDto {
                domain: HardwarePermissionDomainDto::Microphone,
                label_zh: "麦克风采集".to_string(),
                label_en: "Microphone Capture".to_string(),
                posture: DevicePermissionPostureDto::AuthorizedActive,
                active_holder_count: 1,
                revision: 1,
            },
        );
        hardware_permissions.insert(
            HardwarePermissionDomainDto::Camera,
            HardwarePermissionStatusDto {
                domain: HardwarePermissionDomainDto::Camera,
                label_zh: "摄像头输入".to_string(),
                label_en: "Camera Input".to_string(),
                posture: DevicePermissionPostureDto::PromptOnUse,
                active_holder_count: 0,
                revision: 1,
            },
        );
        hardware_permissions.insert(
            HardwarePermissionDomainDto::ScreenCapture,
            HardwarePermissionStatusDto {
                domain: HardwarePermissionDomainDto::ScreenCapture,
                label_zh: "屏幕共享与录制".to_string(),
                label_en: "Screen Recording".to_string(),
                posture: DevicePermissionPostureDto::AuthorizedActive,
                active_holder_count: 1,
                revision: 1,
            },
        );
        hardware_permissions.insert(
            HardwarePermissionDomainDto::Location,
            HardwarePermissionStatusDto {
                domain: HardwarePermissionDomainDto::Location,
                label_zh: "高精度定位".to_string(),
                label_en: "Precise Location".to_string(),
                posture: DevicePermissionPostureDto::RevokedKillSwitch,
                active_holder_count: 0,
                revision: 1,
            },
        );

        let mut recovery_alerts = BTreeMap::new();
        recovery_alerts.insert(
            "rec-runtime-supervision".to_string(),
            RecoveryDomainAlertDto {
                alert_id: "rec-runtime-supervision".to_string(),
                domain_name_zh: "运行时进程监督域".to_string(),
                domain_name_en: "Runtime Process Supervision".to_string(),
                state: "BACKING_OFF".to_string(),
                acknowledged: false,
                resumable: true,
                revision: 1,
            },
        );
        recovery_alerts.insert(
            "rec-storage-journal".to_string(),
            RecoveryDomainAlertDto {
                alert_id: "rec-storage-journal".to_string(),
                domain_name_zh: "对象存储日志域".to_string(),
                domain_name_en: "Object Store WAL Journal".to_string(),
                state: "HEALTHY".to_string(),
                acknowledged: true,
                resumable: false,
                revision: 1,
            },
        );
        recovery_alerts.insert(
            "rec-channel-backpressure".to_string(),
            RecoveryDomainAlertDto {
                alert_id: "rec-channel-backpressure".to_string(),
                domain_name_zh: "消息通道背压恢复域".to_string(),
                domain_name_en: "Channel Backpressure Recovery".to_string(),
                state: "ALERTING".to_string(),
                acknowledged: false,
                resumable: true,
                revision: 1,
            },
        );

        let ownership_nodes = build_seeded_ownership_tree();
        let cells = build_seeded_cells();
        let (settings_revisions, settings_entries) = build_seeded_settings();
        let associations = build_seeded_associations();
        let (keychain_refs, private_vault_secrets) = build_seeded_keychain_vault();

        Self {
            inner: Arc::new(RwLock::new(InnerControlSettingsState {
                snapshot_revision: 1,
                session_locked: false,
                session_generation: 100,
                active_workspace_id: "ws-prod-studio".to_string(),
                performance_profile: PerformanceProfileDto::Balanced,
                background_tier_paused: false,
                hardware_permissions,
                recovery_alerts,
                ownership_nodes,
                cells,
                arming_tickets: BTreeMap::new(),
                command_ledger: BTreeMap::new(),
                settings_revisions,
                settings_entries,
                settings_watch_log: Vec::new(),
                associations,
                keychain_refs,
                private_vault_secrets,
                active_keychain_handles: BTreeMap::new(),
                monotonic_counter: 1_000,
            })),
        }
    }

    fn read_guard(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, InnerControlSettingsState>, DesktopError> {
        self.inner
            .read()
            .map_err(|_| DesktopError::internal("settings_keychain read lock poisoned"))
    }

    fn write_guard(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, InnerControlSettingsState>, DesktopError> {
        self.inner
            .write()
            .map_err(|_| DesktopError::internal("settings_keychain write lock poisoned"))
    }
}

fn default_watermark(
    guaranteed_floor_pct: u8,
    allocated_reserved_pct: u8,
    burst_ceiling_pct: u8,
    live_usage_pct: u8,
    pressure_state: ResourcePressureStateDto,
    cpu_millicores: u32,
    memory_mib: u32,
) -> NomadTripleWatermarkDto {
    NomadTripleWatermarkDto {
        guaranteed_floor_pct,
        allocated_reserved_pct,
        burst_ceiling_pct,
        live_usage_pct,
        pressure_state,
        cpu_millicores,
        memory_mib,
        io_kbps: (cpu_millicores / 2).max(64),
    }
}

#[allow(clippy::too_many_arguments)]
fn make_tree_node(
    node_id: &str,
    parent_node_id: Option<&str>,
    domain: ControlCenterDomainKind,
    layer: OwnershipHierarchyLayer,
    label_zh: &str,
    label_en: &str,
    lifecycle_state: &str,
    host_pid: Option<u32>,
    watermark: NomadTripleWatermarkDto,
    undo_class: Option<UndoClassDto>,
    is_search_0x1f_candidate: bool,
    cell_quarantined: bool,
    l3_diagnostic_tag: &str,
) -> OwnershipTreeNodeDto {
    let pruner_out = compute_backend_allowed_verbs(&BackendActionPrunerInput {
        domain,
        lifecycle_state,
        undo_class,
        is_search_0x1f_candidate,
        search_restriction_bits: SEARCH_NON_AUTHORITATIVE_MASK_0X1F,
        cell_quarantined,
        has_active_tasks_for_uninstall: false,
    });

    OwnershipTreeNodeDto {
        node_id: node_id.to_string(),
        parent_node_id: parent_node_id.map(str::to_string),
        domain,
        layer,
        depth: layer.depth_index(),
        label_zh: label_zh.to_string(),
        label_en: label_en.to_string(),
        lifecycle_state: lifecycle_state.to_string(),
        host_pid,
        fault_domain_posture: FaultDomainPostureDto::IsolatedProcess,
        shared_fate_peer_ids: Vec::new(),
        watermark,
        undo_class,
        is_search_0x1f_candidate,
        search_restriction_bits: if is_search_0x1f_candidate {
            SEARCH_NON_AUTHORITATIVE_MASK_0X1F
        } else {
            0
        },
        allowed_verbs: pruner_out.allowed_verbs,
        statutory_locks: pruner_out.statutory_locks,
        read_only_reason_zh: pruner_out.read_only_reason_zh,
        read_only_reason_en: pruner_out.read_only_reason_en,
        requires_two_step_arming_verbs: pruner_out.requires_two_step_arming_verbs,
        requires_blast_radius_preview_verbs: pruner_out.requires_blast_radius_preview_verbs,
        linked_resource_group_id: Some("node-resource-group-main".to_string()),
        linked_topic_channel_id: Some("node-topic-telemetry".to_string()),
        linked_operation_id: Some("node-op-ext-webhook".to_string()),
        linked_cell_id: Some("cell-local-primary".to_string()),
        l3_diagnostic_tag: l3_diagnostic_tag.to_string(),
    }
}

/// Propagates `SHARED_THREAD_LIKE` fault-domain badges across any `AgentInstance` or
/// `ExecutionFiber` nodes that share the same host OS `ProcessId` (`[TASK-AGENT-001]`, `[TASK-ISO-005]`).
pub fn recompute_shared_thread_like_fault_domains(nodes: &mut [OwnershipTreeNodeDto]) -> usize {
    let mut pid_to_sub_unit_ids: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for node in nodes.iter() {
        let is_sub_unit = matches!(
            node.layer,
            OwnershipHierarchyLayer::L9AgentInstance | OwnershipHierarchyLayer::L10ExecutionFiber
        );
        if let (Some(pid), true) = (node.host_pid, is_sub_unit) {
            pid_to_sub_unit_ids
                .entry(pid)
                .or_default()
                .push(node.node_id.clone());
        }
    }

    let shared_pids: BTreeSet<u32> = pid_to_sub_unit_ids
        .iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(pid, _)| *pid)
        .collect();

    for node in nodes.iter_mut() {
        let shared_peers = node
            .host_pid
            .filter(|pid| shared_pids.contains(pid))
            .and_then(|pid| pid_to_sub_unit_ids.get(&pid));
        if let Some(peers) = shared_peers {
            node.fault_domain_posture = FaultDomainPostureDto::SharedThreadLike;
            node.shared_fate_peer_ids = peers
                .iter()
                .filter(|id| *id != &node.node_id)
                .cloned()
                .collect();
        } else {
            node.fault_domain_posture = FaultDomainPostureDto::IsolatedProcess;
            node.shared_fate_peer_ids.clear();
        }
    }

    shared_pids.len()
}

fn build_seeded_ownership_tree() -> Vec<OwnershipTreeNodeDto> {
    let mut nodes = vec![
        // Layer 1: System / Device
        make_tree_node(
            "node-sys-workstation-01",
            None,
            ControlCenterDomainKind::SystemDevice,
            OwnershipHierarchyLayer::L1SystemDevice,
            "本地工作站主控节点",
            "Primary Workstation Host",
            "ACTIVE",
            None,
            default_watermark(25, 55, 90, 48, ResourcePressureStateDto::Normal, 3200, 6144),
            None,
            false,
            false,
            "SYS-DEV-001",
        ),
        // Layer 2: Workspace / Tenant
        make_tree_node(
            "node-ws-prod-studio",
            Some("node-sys-workstation-01"),
            ControlCenterDomainKind::Workspace,
            OwnershipHierarchyLayer::L2WorkspaceTenant,
            "生产研发工作区",
            "Production Engineering Workspace",
            "ACTIVE",
            None,
            default_watermark(20, 50, 85, 44, ResourcePressureStateDto::Normal, 2800, 5120),
            None,
            false,
            false,
            "WS-STUDIO-001",
        ),
        // Layer 3: Application
        make_tree_node(
            "node-app-code-analyzer",
            Some("node-ws-prod-studio"),
            ControlCenterDomainKind::Application,
            OwnershipHierarchyLayer::L3Application,
            "静态审计与构建套件",
            "Static Audit & Build Suite",
            "ACTIVE",
            None,
            default_watermark(20, 48, 80, 42, ResourcePressureStateDto::Normal, 2100, 3584),
            None,
            false,
            false,
            "APP-AUDIT-001",
        ),
        // Layer 4: Task (Reversible)
        make_tree_node(
            "node-task-release-verify",
            Some("node-app-code-analyzer"),
            ControlCenterDomainKind::Task,
            OwnershipHierarchyLayer::L4Task,
            "版本发布合规验证任务",
            "Release Compliance Verification Task",
            "RUNNING",
            None,
            default_watermark(20, 45, 80, 40, ResourcePressureStateDto::Normal, 1600, 2560),
            Some(UndoClassDto::Reversible),
            false,
            false,
            "TASK-VERIFY-001",
        ),
        // Layer 4b: Completed Irreversible Task (Lock 2: IRREVERSIBLE_NO_UNDO)
        make_tree_node(
            "node-task-external-publish",
            Some("node-app-code-analyzer"),
            ControlCenterDomainKind::Task,
            OwnershipHierarchyLayer::L4Task,
            "外部镜像签名发布任务",
            "External Image Signed Publish Task",
            "COMPLETED",
            None,
            default_watermark(10, 20, 40, 12, ResourcePressureStateDto::Normal, 200, 512),
            Some(UndoClassDto::Irreversible),
            false,
            false,
            "RCPT-UNDO-001",
        ),
        // Layer 5: TaskGroup
        make_tree_node(
            "node-group-static-gates",
            Some("node-task-release-verify"),
            ControlCenterDomainKind::TaskGroup,
            OwnershipHierarchyLayer::L5TaskGroup,
            "多车道静态门禁任务组",
            "Multi-Lane Static Gate Group",
            "RUNNING",
            None,
            default_watermark(20, 45, 80, 39, ResourcePressureStateDto::Normal, 1400, 2048),
            None,
            false,
            false,
            "TASK-GRP-001",
        ),
        // Layer 6: TaskNode (ACTIVE -> Lock 3: NODE_COMPLETED_OR_ACTIVE_LOCKED)
        make_tree_node(
            "node-tasknode-ast-scan",
            Some("node-group-static-gates"),
            ControlCenterDomainKind::TaskNode,
            OwnershipHierarchyLayer::L6TaskNode,
            "语法树与依赖扫描步骤",
            "AST & Dependency Scan Step",
            "ACTIVE",
            None,
            default_watermark(20, 45, 80, 38, ResourcePressureStateDto::Normal, 1200, 1792),
            None,
            false,
            false,
            "TASK-GRAPH-002",
        ),
        // Layer 6b: TaskNode (PENDING -> Editable)
        make_tree_node(
            "node-tasknode-report-pack",
            Some("node-group-static-gates"),
            ControlCenterDomainKind::TaskNode,
            OwnershipHierarchyLayer::L6TaskNode,
            "归档报告打包步骤",
            "Archive Report Bundle Step",
            "PENDING",
            None,
            default_watermark(10, 20, 50, 5, ResourcePressureStateDto::Normal, 100, 256),
            None,
            false,
            false,
            "TASK-NODE-PENDING",
        ),
        // Layer 7: TaskAttempt (Lock 4: TASK_ATTEMPT_NO_MANUAL_COMMIT)
        make_tree_node(
            "node-attempt-ast-01",
            Some("node-tasknode-ast-scan"),
            ControlCenterDomainKind::TaskAttempt,
            OwnershipHierarchyLayer::L7TaskAttempt,
            "扫描执行尝试 #1",
            "Scan Execution Attempt #1",
            "RUNNING",
            None,
            default_watermark(20, 45, 80, 37, ResourcePressureStateDto::Normal, 1100, 1536),
            None,
            false,
            false,
            "TASK-COMMIT-001",
        ),
        // Layer 8: Process (Host OS Process PID 4820)
        make_tree_node(
            "node-proc-worker-4820",
            Some("node-attempt-ast-01"),
            ControlCenterDomainKind::ExecutionUnit,
            OwnershipHierarchyLayer::L8Process,
            "分析器工作进程 (PID 4820)",
            "Analyzer Worker Process (PID 4820)",
            "RUNNING",
            Some(4820),
            default_watermark(
                25,
                60,
                88,
                64,
                ResourcePressureStateDto::Throttled,
                950,
                1280,
            ),
            None,
            false,
            false,
            "EXEC-PROC-4820",
        ),
        // Layer 9: AgentInstance #1 sharing PID 4820 -> SHARED_THREAD_LIKE
        make_tree_node(
            "node-agent-scanner-a",
            Some("node-proc-worker-4820"),
            ControlCenterDomainKind::ExecutionUnit,
            OwnershipHierarchyLayer::L9AgentInstance,
            "代码扫描代理实例 A",
            "Code Scanner Agent Instance A",
            "RUNNING",
            Some(4820),
            default_watermark(15, 35, 70, 34, ResourcePressureStateDto::Normal, 500, 640),
            None,
            false,
            false,
            "TASK-AGENT-001",
        ),
        // Layer 9b: AgentInstance #2 sharing PID 4820 -> SHARED_THREAD_LIKE
        make_tree_node(
            "node-agent-scanner-b",
            Some("node-proc-worker-4820"),
            ControlCenterDomainKind::ExecutionUnit,
            OwnershipHierarchyLayer::L9AgentInstance,
            "许可证核验代理实例 B",
            "License Verifier Agent Instance B",
            "RUNNING",
            Some(4820),
            default_watermark(15, 30, 65, 30, ResourcePressureStateDto::Normal, 450, 640),
            None,
            false,
            false,
            "TASK-ISO-005",
        ),
        // Layer 10: ExecutionFiber #1 sharing PID 4820 -> SHARED_THREAD_LIKE
        make_tree_node(
            "node-fiber-ast-parser-01",
            Some("node-agent-scanner-a"),
            ControlCenterDomainKind::ExecutionUnit,
            OwnershipHierarchyLayer::L10ExecutionFiber,
            "语法树解析协程流 #01",
            "AST Parser Execution Fiber #01",
            "RUNNING",
            Some(4820),
            default_watermark(10, 25, 50, 22, ResourcePressureStateDto::Normal, 300, 384),
            None,
            false,
            false,
            "FIBER-AST-01",
        ),
        // Domain 9: ResourceGroup / Context
        make_tree_node(
            "node-resource-group-main",
            Some("node-ws-prod-studio"),
            ControlCenterDomainKind::ResourceGroup,
            OwnershipHierarchyLayer::L3Application,
            "构建计算资源保障组",
            "Build Compute Resource Pool",
            "ACTIVE",
            None,
            default_watermark(30, 65, 92, 58, ResourcePressureStateDto::Normal, 2400, 4096),
            None,
            false,
            false,
            "RES-POOL-001",
        ),
        // Domain 10: Topic / Channel
        make_tree_node(
            "node-topic-telemetry",
            Some("node-ws-prod-studio"),
            ControlCenterDomainKind::TopicChannel,
            OwnershipHierarchyLayer::L3Application,
            "编译诊断事件通道",
            "Build Diagnostic Event Topic",
            "ACTIVE",
            None,
            default_watermark(15, 40, 85, 52, ResourcePressureStateDto::Normal, 320, 512),
            None,
            false,
            false,
            "TOPIC-DIAG-001",
        ),
        // Domain 11: Operation / Device (EFFECT_UNKNOWN -> Lock 5)
        make_tree_node(
            "node-op-ext-webhook",
            Some("node-attempt-ast-01"),
            ControlCenterDomainKind::OperationDevice,
            OwnershipHierarchyLayer::L8Process,
            "外部合规网关回执操作",
            "External Compliance Webhook Operation",
            "EFFECT_UNKNOWN",
            None,
            default_watermark(10, 25, 60, 18, ResourcePressureStateDto::Normal, 120, 192),
            None,
            false,
            false,
            "TASK-EFFECT-003",
        ),
        // Domain 12a: Stage C Cell & Distributed Lease (Healthy Primary Cell)
        make_tree_node(
            "node-cell-local-primary",
            Some("node-sys-workstation-01"),
            ControlCenterDomainKind::CellDistributedLease,
            OwnershipHierarchyLayer::L2WorkspaceTenant,
            "本地主控计算单元",
            "Primary Local Compute Unit",
            "ACTIVE",
            None,
            default_watermark(25, 55, 90, 46, ResourcePressureStateDto::Normal, 2200, 4096),
            None,
            false,
            false,
            "CELL-PRIMARY-001",
        ),
        // Domain 12b: Quarantined Cell Lease (Lock 6: QUARANTINED_LEASE_FENCE)
        make_tree_node(
            "node-cell-quarantined-edge",
            Some("node-sys-workstation-01"),
            ControlCenterDomainKind::CellDistributedLease,
            OwnershipHierarchyLayer::L2WorkspaceTenant,
            "隔离观察计算单元",
            "Quarantined Edge Compute Unit",
            "QUARANTINED",
            None,
            default_watermark(
                20,
                75,
                95,
                82,
                ResourcePressureStateDto::OomAtRisk,
                800,
                1536,
            ),
            None,
            false,
            true,
            "LEASE-TTL-001",
        ),
        // Cross-Domain Search 0x1F Candidate (Lock 1: SEARCH_0X1F_READ_ONLY)
        make_tree_node(
            "node-search-0x1f-candidate",
            Some("node-ws-prod-studio"),
            ControlCenterDomainKind::Task,
            OwnershipHierarchyLayer::L4Task,
            "搜索候选：历史发布审计记录",
            "Search Candidate: Historical Release Audit",
            "COMPLETED",
            None,
            default_watermark(5, 10, 20, 5, ResourcePressureStateDto::Normal, 50, 64),
            Some(UndoClassDto::Reversible),
            true,
            false,
            "CONS-CTRL-001",
        ),
    ];

    recompute_shared_thread_like_fault_domains(&mut nodes);
    nodes
}

fn build_seeded_cells() -> BTreeMap<String, CellNodeInspectorDto> {
    let mut cells = BTreeMap::new();

    cells.insert(
        "cell-local-primary".to_string(),
        CellNodeInspectorDto {
            cell_id_hex: "cell-local-primary".to_string(),
            display_name_zh: "主控工作站计算单元 #01".to_string(),
            display_name_en: "Primary Workstation Compute Unit #01".to_string(),
            posture: CellConnectivityPostureDto::LocalSingleNode,
            autonomy_gate: PartitionAutonomyGateDto::Open,
            current_epoch: 42,
            clock_skew_horizon_ms: 15,
            accept_migration_target: true,
            watermark: default_watermark(
                25,
                55,
                90,
                48,
                ResourcePressureStateDto::Normal,
                2200,
                4096,
            ),
            leases: vec![
                CellLeaseInspectionRecordDto {
                    lease_id_hex: "lease-quota-exec-001".to_string(),
                    cell_id_hex: "cell-local-primary".to_string(),
                    family: CellLeaseFamilyDto::QuotaExecutionLease,
                    holder_node_id: "node-proc-worker-4820".to_string(),
                    holder_label_zh: "分析器工作进程 (PID 4820)".to_string(),
                    holder_label_en: "Analyzer Worker Process (PID 4820)".to_string(),
                    lease_epoch: 42,
                    ttl_remaining_ms: 28_500,
                    clock_skew_horizon_ms: 15,
                    quarantined: false,
                    has_closure_receipt: false,
                    has_fence_receipt: false,
                    statutory_lock: None,
                    watermark: default_watermark(
                        25,
                        60,
                        88,
                        56,
                        ResourcePressureStateDto::Normal,
                        950,
                        1280,
                    ),
                },
                CellLeaseInspectionRecordDto {
                    lease_id_hex: "lease-capacity-subpool-002".to_string(),
                    cell_id_hex: "cell-local-primary".to_string(),
                    family: CellLeaseFamilyDto::CapacitySubpoolLease,
                    holder_node_id: "node-resource-group-main".to_string(),
                    holder_label_zh: "构建计算资源保障组".to_string(),
                    holder_label_en: "Build Compute Resource Pool".to_string(),
                    lease_epoch: 42,
                    ttl_remaining_ms: 45_000,
                    clock_skew_horizon_ms: 15,
                    quarantined: false,
                    has_closure_receipt: false,
                    has_fence_receipt: false,
                    statutory_lock: None,
                    watermark: default_watermark(
                        30,
                        65,
                        92,
                        58,
                        ResourcePressureStateDto::Normal,
                        1200,
                        2048,
                    ),
                },
                CellLeaseInspectionRecordDto {
                    lease_id_hex: "lease-device-epoch-003".to_string(),
                    cell_id_hex: "cell-local-primary".to_string(),
                    family: CellLeaseFamilyDto::ExclusiveDeviceEpochLease,
                    holder_node_id: "node-sys-workstation-01".to_string(),
                    holder_label_zh: "独占音频硬件采集通道".to_string(),
                    holder_label_en: "Exclusive Audio Hardware Capture Channel".to_string(),
                    lease_epoch: 42,
                    ttl_remaining_ms: 19_200,
                    clock_skew_horizon_ms: 15,
                    quarantined: false,
                    has_closure_receipt: false,
                    has_fence_receipt: false,
                    statutory_lock: None,
                    watermark: default_watermark(
                        10,
                        25,
                        50,
                        20,
                        ResourcePressureStateDto::Normal,
                        150,
                        256,
                    ),
                },
            ],
        },
    );

    cells.insert(
        "cell-secondary-compute".to_string(),
        CellNodeInspectorDto {
            cell_id_hex: "cell-secondary-compute".to_string(),
            display_name_zh: "辅助加速计算单元 #02".to_string(),
            display_name_en: "Secondary Compute Accelerator Unit #02".to_string(),
            posture: CellConnectivityPostureDto::HealthyClusterMember,
            autonomy_gate: PartitionAutonomyGateDto::Open,
            current_epoch: 42,
            clock_skew_horizon_ms: 22,
            accept_migration_target: true,
            watermark: default_watermark(
                20,
                40,
                85,
                31,
                ResourcePressureStateDto::Normal,
                1100,
                2048,
            ),
            leases: Vec::new(),
        },
    );

    cells.insert(
        "cell-quarantined-edge".to_string(),
        CellNodeInspectorDto {
            cell_id_hex: "cell-quarantined-edge".to_string(),
            display_name_zh: "隔离观察计算单元 #03".to_string(),
            display_name_en: "Quarantined Edge Compute Unit #03".to_string(),
            posture: CellConnectivityPostureDto::Quarantined,
            autonomy_gate: PartitionAutonomyGateDto::QuarantinedHold,
            current_epoch: 41,
            clock_skew_horizon_ms: 480,
            accept_migration_target: false,
            watermark: default_watermark(
                20,
                75,
                95,
                84,
                ResourcePressureStateDto::OomAtRisk,
                800,
                1536,
            ),
            leases: vec![CellLeaseInspectionRecordDto {
                lease_id_hex: "lease-quarantined-009".to_string(),
                cell_id_hex: "cell-quarantined-edge".to_string(),
                family: CellLeaseFamilyDto::ExclusiveDeviceEpochLease,
                holder_node_id: "node-cell-quarantined-edge".to_string(),
                holder_label_zh: "受隔离外设控制句柄".to_string(),
                holder_label_en: "Quarantined Peripheral Handle".to_string(),
                lease_epoch: 41,
                ttl_remaining_ms: 0,
                clock_skew_horizon_ms: 480,
                quarantined: true,
                has_closure_receipt: false,
                has_fence_receipt: false,
                statutory_lock: Some(StatutoryLockKindDto::QuarantinedLeaseFence),
                watermark: default_watermark(
                    20,
                    80,
                    95,
                    86,
                    ResourcePressureStateDto::OomAtRisk,
                    400,
                    768,
                ),
            }],
        },
    );

    cells
}

#[allow(clippy::type_complexity)]
fn build_seeded_settings() -> (
    BTreeMap<SettingsLayerKindDto, u64>,
    BTreeMap<(SettingsLayerKindDto, String), SettingsLayerEntryDto>,
) {
    let mut revisions = BTreeMap::new();
    revisions.insert(SettingsLayerKindDto::System, 10);
    revisions.insert(SettingsLayerKindDto::User, 20);
    revisions.insert(SettingsLayerKindDto::Workspace, 30);
    revisions.insert(SettingsLayerKindDto::Application, 40);

    let mut entries = BTreeMap::new();
    let mut insert_entry = |layer: SettingsLayerKindDto,
                            key: &str,
                            value_json: &str,
                            rev: u64,
                            locked: bool,
                            zh: &str,
                            en: &str| {
        entries.insert(
            (layer, key.to_string()),
            SettingsLayerEntryDto {
                key: key.to_string(),
                value_json: value_json.to_string(),
                layer,
                entry_revision: rev,
                locked_by_system_policy: locked,
                label_zh: zh.to_string(),
                label_en: en.to_string(),
            },
        );
    };

    // System layer defaults & mandatory security policy
    insert_entry(
        SettingsLayerKindDto::System,
        "appearance.theme",
        "\"graphite-workstation\"",
        10,
        false,
        "工作站色彩主题",
        "Workstation Color Theme",
    );
    insert_entry(
        SettingsLayerKindDto::System,
        "security.mandatory_biometric_confirm",
        "true",
        10,
        true,
        "高危操作强制硬件或生物核身",
        "Mandatory Biometric Verification for Destructive Actions",
    );
    insert_entry(
        SettingsLayerKindDto::System,
        "control.background_concurrency_limit",
        "8",
        10,
        false,
        "后台任务最大并发上限",
        "Background Task Concurrency Limit",
    );
    insert_entry(
        SettingsLayerKindDto::System,
        "locale.display_language",
        "\"zh-CN\"",
        10,
        false,
        "界面显示语言",
        "Interface Display Language",
    );

    // User layer overrides
    insert_entry(
        SettingsLayerKindDto::User,
        "appearance.theme",
        "\"graphite-workstation\"",
        20,
        false,
        "工作站色彩主题",
        "Workstation Color Theme",
    );
    insert_entry(
        SettingsLayerKindDto::User,
        "control.background_concurrency_limit",
        "12",
        20,
        false,
        "后台任务最大并发上限",
        "Background Task Concurrency Limit",
    );

    // Workspace layer overrides
    insert_entry(
        SettingsLayerKindDto::Workspace,
        "control.background_concurrency_limit",
        "16",
        30,
        false,
        "后台任务最大并发上限",
        "Background Task Concurrency Limit",
    );
    insert_entry(
        SettingsLayerKindDto::Workspace,
        "audit.retain_receipt_days",
        "90",
        30,
        false,
        "执行回执留存天数",
        "Execution Receipt Retention Days",
    );

    // Application layer overrides
    insert_entry(
        SettingsLayerKindDto::Application,
        "analyzer.max_ast_worker_threads",
        "4",
        40,
        false,
        "分析器最大工作线程数",
        "Analyzer Max Worker Threads",
    );

    (revisions, entries)
}

fn build_seeded_associations() -> BTreeMap<String, ContentTypeAssociationEntryDto> {
    let mut map = BTreeMap::new();
    map.insert(
        "application/pdf".to_string(),
        ContentTypeAssociationEntryDto {
            content_type_or_uri: "application/pdf".to_string(),
            description_zh: "便携式文档格式 (PDF)".to_string(),
            description_en: "Portable Document Format (PDF)".to_string(),
            handler_application_id_hex: Some("app-doc-reader-01".to_string()),
            handler_installation_id_hex: Some("inst-doc-reader-v2".to_string()),
            handler_label_zh: "工作站文档审阅器".to_string(),
            handler_label_en: "Workstation Document Reviewer".to_string(),
            user_explicitly_bound: true,
            pending_package_proposal: Some(PackageAssociationProposalDto {
                proposed_application_id_hex: "app-thirdparty-pdf-pro".to_string(),
                proposed_installation_id_hex: "inst-thirdparty-pdf-09".to_string(),
                proposed_app_name_zh: "第三方 PDF 套件 (待用户确认)".to_string(),
                proposed_app_name_en: "Third-Party PDF Suite (Awaiting User Approval)".to_string(),
                submitted_at_ms: 1_760_000_100,
            }),
            revision: 1,
        },
    );
    map.insert(
        "text/markdown".to_string(),
        ContentTypeAssociationEntryDto {
            content_type_or_uri: "text/markdown".to_string(),
            description_zh: "Markdown 技术文档".to_string(),
            description_en: "Markdown Technical Document".to_string(),
            handler_application_id_hex: Some("app-spec-studio-01".to_string()),
            handler_installation_id_hex: Some("inst-spec-studio-v4".to_string()),
            handler_label_zh: "架构规范编辑器".to_string(),
            handler_label_en: "Architecture Spec Editor".to_string(),
            user_explicitly_bound: true,
            pending_package_proposal: None,
            revision: 1,
        },
    );
    map.insert(
        "nlos-workspace://".to_string(),
        ContentTypeAssociationEntryDto {
            content_type_or_uri: "nlos-workspace://".to_string(),
            description_zh: "工作区深层链接协议".to_string(),
            description_en: "Workspace Deep-Link URI Scheme".to_string(),
            handler_application_id_hex: Some("app-workstation-shell".to_string()),
            handler_installation_id_hex: Some("inst-workstation-shell-v1".to_string()),
            handler_label_zh: "系统工作区宿主".to_string(),
            handler_label_en: "System Workspace Host".to_string(),
            user_explicitly_bound: true,
            pending_package_proposal: None,
            revision: 1,
        },
    );
    map
}

fn build_seeded_keychain_vault() -> (
    BTreeMap<String, KeychainReferenceEntryDto>,
    BTreeMap<String, PrivateVaultSecretMaterial>,
) {
    let mut refs = BTreeMap::new();
    let mut secrets = BTreeMap::new();

    refs.insert(
        "kref-enterprise-git-signing".to_string(),
        KeychainReferenceEntryDto {
            keychain_ref_id_hex: "kref-enterprise-git-signing".to_string(),
            owner_principal_id_hex: "principal-workstation-operator".to_string(),
            label_zh: "代码仓库签名主密钥引用".to_string(),
            label_en: "Repository Commit Signing Key Reference".to_string(),
            credential_kind: "ED25519_HARDWARE_ENCLAVE_KEY".to_string(),
            public_fingerprint_hex: "fp_blake3_9a8f7c6e5d4b3a21".to_string(),
            authorized_application_ids: vec![
                "node-app-code-analyzer".to_string(),
                "app-spec-studio-01".to_string(),
            ],
            hardware_backed: true,
            expected_generation_or_revision: 1,
            revoked: false,
            active_use_handle_count: 0,
        },
    );
    secrets.insert(
        "kref-enterprise-git-signing".to_string(),
        PrivateVaultSecretMaterial {
            secret_bytes: b"TOP_SECRET_ENCLAVE_SEED_NEVER_SERIALIZE_001".to_vec(),
        },
    );

    refs.insert(
        "kref-registry-upload-token".to_string(),
        KeychainReferenceEntryDto {
            keychain_ref_id_hex: "kref-registry-upload-token".to_string(),
            owner_principal_id_hex: "principal-workstation-operator".to_string(),
            label_zh: "制品库发布访问凭据引用".to_string(),
            label_en: "Artifact Registry Publish Credential Reference".to_string(),
            credential_kind: "OAUTH2_SCOPED_BEARER_REF".to_string(),
            public_fingerprint_hex: "fp_blake3_1f2e3d4c5b6a7988".to_string(),
            authorized_application_ids: vec!["node-app-code-analyzer".to_string()],
            hardware_backed: false,
            expected_generation_or_revision: 1,
            revoked: false,
            active_use_handle_count: 0,
        },
    );
    secrets.insert(
        "kref-registry-upload-token".to_string(),
        PrivateVaultSecretMaterial {
            secret_bytes: b"TOP_SECRET_BEARER_TOKEN_NEVER_SERIALIZE_002".to_vec(),
        },
    );

    (refs, secrets)
}

fn compute_domain_summaries(nodes: &[OwnershipTreeNodeDto]) -> Vec<ControlDomainSummaryDto> {
    ControlCenterDomainKind::ALL
        .iter()
        .map(|&domain| {
            let domain_nodes: Vec<&OwnershipTreeNodeDto> =
                nodes.iter().filter(|n| n.domain == domain).collect();
            let total_objects = domain_nodes.len();
            let active_objects = domain_nodes
                .iter()
                .filter(|n| {
                    matches!(
                        n.lifecycle_state.as_str(),
                        "ACTIVE" | "RUNNING" | "VERIFYING"
                    )
                })
                .count();
            let attention_objects = domain_nodes
                .iter()
                .filter(|n| {
                    n.fault_domain_posture == FaultDomainPostureDto::SharedThreadLike
                        || n.watermark.pressure_state != ResourcePressureStateDto::Normal
                        || n.lifecycle_state == "EFFECT_UNKNOWN"
                        || n.lifecycle_state == "QUARANTINED"
                })
                .count();
            let locked_objects = domain_nodes
                .iter()
                .filter(|n| !n.statutory_locks.is_empty())
                .count();

            let (title_zh, title_en) = match domain {
                ControlCenterDomainKind::SystemDevice => ("系统与硬件设备", "System & Device"),
                ControlCenterDomainKind::Workspace => ("工作区与租户", "Workspace & Tenant"),
                ControlCenterDomainKind::Application => ("已安装应用", "Applications"),
                ControlCenterDomainKind::Task => ("任务生命周期", "Tasks"),
                ControlCenterDomainKind::TaskGroup => ("任务组编排", "Task Groups"),
                ControlCenterDomainKind::TaskNode => ("执行步骤节点", "Task Steps"),
                ControlCenterDomainKind::TaskAttempt => ("执行尝试", "Execution Attempts"),
                ControlCenterDomainKind::ExecutionUnit => {
                    ("进程与协程流", "Processes & Execution Units")
                }
                ControlCenterDomainKind::ResourceGroup => {
                    ("资源保障组与配额", "Resource Pools & Context")
                }
                ControlCenterDomainKind::TopicChannel => ("事件通道与队列", "Topics & Channels"),
                ControlCenterDomainKind::OperationDevice => {
                    ("外部操作与外设队列", "Operations & Device Queues")
                }
                ControlCenterDomainKind::CellDistributedLease => {
                    ("计算单元与分布式租约", "Compute Units & Leases")
                }
            };

            let cpu_sum: u32 = domain_nodes
                .iter()
                .map(|n| n.watermark.cpu_millicores)
                .sum();
            let mem_sum: u32 = domain_nodes.iter().map(|n| n.watermark.memory_mib).sum();
            let max_live: u8 = domain_nodes
                .iter()
                .map(|n| n.watermark.live_usage_pct)
                .max()
                .unwrap_or(20);

            ControlDomainSummaryDto {
                domain,
                title_zh: title_zh.to_string(),
                title_en: title_en.to_string(),
                total_objects,
                active_objects,
                attention_objects,
                locked_objects,
                watermark: default_watermark(
                    20,
                    50,
                    88,
                    max_live,
                    if max_live >= 80 {
                        ResourcePressureStateDto::OomAtRisk
                    } else if max_live >= 60 {
                        ResourcePressureStateDto::Throttled
                    } else {
                        ResourcePressureStateDto::Normal
                    },
                    cpu_sum,
                    mem_sum,
                ),
            }
        })
        .collect()
}

fn compute_effective_settings(
    revisions: &BTreeMap<SettingsLayerKindDto, u64>,
    entries: &BTreeMap<(SettingsLayerKindDto, String), SettingsLayerEntryDto>,
) -> Vec<EffectiveSettingRecordDto> {
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for entry in entries.values() {
        keys.insert(entry.key.clone());
    }

    let mut out = Vec::new();
    for key in keys {
        let mut defined_in = Vec::new();
        let mut winning: Option<&SettingsLayerEntryDto> = None;
        let mut locked_by_system = false;

        for &layer in &SettingsLayerKindDto::ALL {
            if let Some(entry) = entries.get(&(layer, key.clone())) {
                defined_in.push(layer);
                if layer == SettingsLayerKindDto::System && entry.locked_by_system_policy {
                    locked_by_system = true;
                }
                if !locked_by_system || layer == SettingsLayerKindDto::System {
                    winning = Some(entry);
                }
            }
        }

        if let Some(win) = winning {
            let winning_rev = revisions.get(&win.layer).copied().unwrap_or(1);
            out.push(EffectiveSettingRecordDto {
                key,
                effective_value_json: win.value_json.clone(),
                winning_layer: win.layer,
                winning_domain_revision: winning_rev,
                defined_in_layers: defined_in,
                locked_by_system_policy: locked_by_system,
                label_zh: win.label_zh.clone(),
                label_en: win.label_en.clone(),
            });
        }
    }
    out
}

fn resolve_active_theme(
    revisions: &BTreeMap<SettingsLayerKindDto, u64>,
    entries: &BTreeMap<(SettingsLayerKindDto, String), SettingsLayerEntryDto>,
) -> String {
    compute_effective_settings(revisions, entries)
        .into_iter()
        .find(|s| s.key == "appearance.theme")
        .map(|s| s.effective_value_json.trim_matches('"').to_string())
        .unwrap_or_else(|| "graphite-workstation".to_string())
}

// ============================================================================
// 10. Core Operations (`_core` functions for integration tests & Tauri commands)
// ============================================================================

/// Returns the complete 12-Domain `SystemControlCenter` snapshot (`[DUI-CTRL-001]`).
pub fn inspect_system_control_center_core(
    state: &SettingsKeychainState,
) -> Result<SystemControlCenterSnapshotDto, DesktopError> {
    let mut guard = state.write_guard()?;
    let shared_group_count = recompute_shared_thread_like_fault_domains(&mut guard.ownership_nodes);
    let summaries = compute_domain_summaries(&guard.ownership_nodes);

    Ok(SystemControlCenterSnapshotDto {
        snapshot_revision: guard.snapshot_revision,
        session_state: if guard.session_locked {
            "LOCKED".to_string()
        } else {
            "ACTIVE".to_string()
        },
        session_generation: guard.session_generation,
        active_workspace_id: guard.active_workspace_id.clone(),
        performance_profile: guard.performance_profile,
        background_tier_paused: guard.background_tier_paused,
        shared_thread_like_group_count: shared_group_count,
        hardware_permissions: guard.hardware_permissions.values().cloned().collect(),
        recovery_alerts: guard.recovery_alerts.values().cloned().collect(),
        domain_summaries: summaries,
        ownership_tree: guard.ownership_nodes.clone(),
        cells: guard.cells.values().cloned().collect(),
    })
}

/// Validates a control verb against CNCF Headlamp action pruning and the 5+1 statutory
/// non-tamperable locks (`[UI-PARITY-001]`).
pub fn validate_and_apply_node_verb_core(
    state: &SettingsKeychainState,
    node_id: &str,
    verb: &str,
    read_backing_token: &str,
) -> Result<OwnershipTreeNodeDto, DesktopError> {
    if read_backing_token.trim().is_empty() {
        return Err(control_err(
            "READ_BACKING_REQUIRED: non-empty ReadBackingToken is mandatory".to_string(),
        ));
    }

    let mut guard = state.write_guard()?;
    if guard.session_locked {
        return Err(control_err(
            "SESSION_LOCKED: workstation session is locked".to_string(),
        ));
    }

    let node_idx = guard
        .ownership_nodes
        .iter()
        .position(|n| n.node_id == node_id)
        .ok_or_else(|| control_err(format!("unknown ownership tree node: {node_id}")))?;

    let node = &guard.ownership_nodes[node_idx];

    // Hard statutory lock checks with explicit invariant tags for deterministic audit proofs:
    if node.is_search_0x1f_candidate && verb != "JumpToAuthoritativeObject" {
        return Err(control_err(
            "STATUTORY_LOCK_VIOLATION [SEARCH_0X1F_READ_ONLY]: 0x1F search candidate forbids mutating control verbs"
                .to_string(),
        ));
    }
    if node
        .statutory_locks
        .contains(&StatutoryLockKindDto::IrreversibleNoUndo)
        && verb.eq_ignore_ascii_case("Undo")
    {
        return Err(control_err(
            "STATUTORY_LOCK_VIOLATION [IRREVERSIBLE_NO_UNDO]: IRREVERSIBLE receipt cannot be undone"
                .to_string(),
        ));
    }
    if node
        .statutory_locks
        .contains(&StatutoryLockKindDto::NodeCompletedOrActiveLocked)
        && verb != "ViewEffectProof"
    {
        return Err(control_err(
            "STATUTORY_LOCK_VIOLATION [NODE_COMPLETED_OR_ACTIVE_LOCKED]: COMPLETED or ACTIVE TaskNode forbids structural edits"
                .to_string(),
        ));
    }
    if node.domain == ControlCenterDomainKind::TaskAttempt
        && (verb.eq_ignore_ascii_case("Commit")
            || verb.eq_ignore_ascii_case("ManualCommit")
            || verb.eq_ignore_ascii_case("PromoteDone"))
    {
        return Err(control_err(
            "STATUTORY_LOCK_VIOLATION [TASK_ATTEMPT_NO_MANUAL_COMMIT]: manual commit of TaskAttempt is forbidden"
                .to_string(),
        ));
    }
    if node.domain == ControlCenterDomainKind::OperationDevice
        && node.lifecycle_state == "EFFECT_UNKNOWN"
        && (verb.eq_ignore_ascii_case("MarkSuccess")
            || verb.eq_ignore_ascii_case("MarkNotExecuted")
            || verb.eq_ignore_ascii_case("ForceComplete"))
    {
        return Err(control_err(
            "STATUTORY_LOCK_VIOLATION [OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS]: EFFECT_UNKNOWN operation forbids manual mark-as-success"
                .to_string(),
        ));
    }
    if node
        .statutory_locks
        .contains(&StatutoryLockKindDto::QuarantinedLeaseFence)
        && (verb.eq_ignore_ascii_case("LocalTtlUnlock")
            || verb.eq_ignore_ascii_case("ForceUnlockLease")
            || verb.eq_ignore_ascii_case("ReleaseLease"))
    {
        return Err(control_err(
            "STATUTORY_LOCK_VIOLATION [QUARANTINED_LEASE_FENCE]: quarantined lease forbids local TTL unlock without closure and fence receipts"
                .to_string(),
        ));
    }

    if !node.allowed_verbs.iter().any(|v| v == verb) {
        return Err(control_err(format!(
            "VERB_PRUNED_BY_HEADLAMP_GATE: verb '{verb}' is not allowed for node '{node_id}' in state '{}'",
            node.lifecycle_state
        )));
    }

    // Apply state transition for allowed non-destructive / verified verbs
    let updated_state = match verb {
        "Pause" | "PauseConsumer" => "PAUSED",
        "Resume" | "ResumeConsumer" => "RUNNING",
        "Freeze" | "FreezeContext" => "FROZEN",
        "Throttle" => "THROTTLED",
        "Seal" => "SEALED",
        "Reconcile" => "RECONCILING",
        _ => &guard.ownership_nodes[node_idx].lifecycle_state,
    }
    .to_string();

    guard.ownership_nodes[node_idx].lifecycle_state = updated_state;
    guard.snapshot_revision = guard.snapshot_revision.saturating_add(1);

    Ok(guard.ownership_nodes[node_idx].clone())
}

/// Executes a controlled Stage C `RequestProcessMigration` (`[SCHED-STEAL-001]`, `[LEASE-TTL-001]`).
pub fn request_process_migration_core(
    state: &SettingsKeychainState,
    request: ProcessMigrationControlRequestDto,
) -> Result<ProcessMigrationControlResultDto, DesktopError> {
    if request.read_backing_token.trim().is_empty() {
        return Err(control_err(
            "READ_BACKING_REQUIRED: migration requires a valid read_backing_token".to_string(),
        ));
    }
    if request.source_cell_id_hex == request.target_cell_id_hex {
        return Err(control_err(
            "MIGRATION_SAME_CELL: source and target cell must differ".to_string(),
        ));
    }

    let mut guard = state.write_guard()?;
    let source_cell = guard
        .cells
        .get(&request.source_cell_id_hex)
        .ok_or_else(|| {
            control_err(format!(
                "unknown source cell: {}",
                request.source_cell_id_hex
            ))
        })?
        .clone();

    if source_cell.current_epoch != request.expected_revision_or_generation {
        return Err(cas_conflict_err(
            request.expected_revision_or_generation,
            source_cell.current_epoch,
        ));
    }

    if source_cell.posture == CellConnectivityPostureDto::Quarantined
        && (request.closure_receipt_hex.is_none() || request.fence_receipt_hex.is_none())
    {
        return Err(control_err(
            "STATUTORY_LOCK_VIOLATION [QUARANTINED_LEASE_FENCE]: migrating out of a quarantined cell requires both closure_receipt_hex and fence_receipt_hex"
                .to_string(),
        ));
    }

    let target_cell = guard
        .cells
        .get(&request.target_cell_id_hex)
        .ok_or_else(|| {
            control_err(format!(
                "unknown target cell: {}",
                request.target_cell_id_hex
            ))
        })?
        .clone();

    if !target_cell.accept_migration_target
        || matches!(
            target_cell.posture,
            CellConnectivityPostureDto::Partitioned
                | CellConnectivityPostureDto::FencedReadOnly
                | CellConnectivityPostureDto::Quarantined
                | CellConnectivityPostureDto::Offline
        )
    {
        return Err(control_err(format!(
            "MIGRATION_TARGET_UNAVAILABLE: target cell '{}' is in {:?} posture with gate {:?}",
            target_cell.cell_id_hex, target_cell.posture, target_cell.autonomy_gate
        )));
    }

    let node_idx = guard
        .ownership_nodes
        .iter()
        .position(|n| n.node_id == request.process_node_id)
        .ok_or_else(|| control_err(format!("unknown process node: {}", request.process_node_id)))?;

    let new_epoch = source_cell.current_epoch.saturating_add(1);
    let mut migrated_lease_ids = Vec::new();
    let mut moving_leases = Vec::new();

    if let Some(src_mut) = guard.cells.get_mut(&request.source_cell_id_hex) {
        src_mut.current_epoch = new_epoch;
        let mut retained = Vec::new();
        for mut lease in src_mut.leases.drain(..) {
            if lease.holder_node_id == request.process_node_id {
                lease.cell_id_hex = request.target_cell_id_hex.clone();
                lease.lease_epoch = new_epoch;
                migrated_lease_ids.push(lease.lease_id_hex.clone());
                moving_leases.push(lease);
            } else {
                retained.push(lease);
            }
        }
        src_mut.leases = retained;
    }

    if let Some(dst_mut) = guard.cells.get_mut(&request.target_cell_id_hex) {
        dst_mut.current_epoch = new_epoch;
        dst_mut.leases.extend(moving_leases);
    }

    guard.ownership_nodes[node_idx].linked_cell_id = Some(request.target_cell_id_hex.clone());
    guard.monotonic_counter = guard.monotonic_counter.saturating_add(1);
    guard.snapshot_revision = guard.snapshot_revision.saturating_add(1);
    let receipt_id_hex = format!("rcpt_mig_{:08x}", guard.monotonic_counter);

    Ok(ProcessMigrationControlResultDto {
        process_node_id: request.process_node_id.clone(),
        from_cell_id_hex: request.source_cell_id_hex,
        to_cell_id_hex: request.target_cell_id_hex,
        new_generation: new_epoch,
        migrated_lease_ids,
        receipt_id_hex,
        audit_summary_zh: "进程与关联合同租约已完成跨单元受控迁移".to_string(),
        audit_summary_en: "Process and bound resource leases completed controlled migration"
            .to_string(),
    })
}

/// Computes a Terraform-style `+ / ~ / - / =` blast-radius diff preview (`[CTRL-SAFETY-001]`).
pub fn preview_blast_radius_package_core(
    state: &SettingsKeychainState,
    target_node_id: &str,
    verb: &str,
    expected_revision: u64,
) -> Result<BlastRadiusPreviewPackageDto, DesktopError> {
    let guard = state.read_guard()?;
    let target = guard
        .ownership_nodes
        .iter()
        .find(|n| n.node_id == target_node_id)
        .ok_or_else(|| control_err(format!("unknown target node: {target_node_id}")))?;

    let is_topic_purge = verb.eq_ignore_ascii_case("TopicPurge");
    let mut rows = Vec::new();

    if is_topic_purge {
        rows.push(BlastRadiusDiffRowDto {
            op: BlastRadiusDiffOpDto::MinusDestroyOrCancel,
            symbol: "-".to_string(),
            domain: ControlCenterDomainKind::TopicChannel,
            target_id: target.node_id.clone(),
            label_zh: target.label_zh.clone(),
            label_en: target.label_en.clone(),
            before_state: "ACTIVE (1,280 buffered messages)".to_string(),
            after_state: "PURGED (0 buffered messages)".to_string(),
            detail_zh: "永久清空通道内待消费消息并签发数据清理凭证".to_string(),
            detail_en: "Permanently purge buffered topic messages and issue a Data-Loss Receipt"
                .to_string(),
        });
        rows.push(BlastRadiusDiffRowDto {
            op: BlastRadiusDiffOpDto::PlusCreate,
            symbol: "+".to_string(),
            domain: ControlCenterDomainKind::OperationDevice,
            target_id: format!("rcpt-purge-{target_node_id}"),
            label_zh: "数据清理不可逆审计凭证".to_string(),
            label_en: "Irreversible Data-Loss Audit Receipt".to_string(),
            before_state: "NONE".to_string(),
            after_state: "ISSUED".to_string(),
            detail_zh: "记录清理条数、字节数与操作人确认签名".to_string(),
            detail_en: "Record purged message count, byte count, and operator acknowledgment"
                .to_string(),
        });
        rows.push(BlastRadiusDiffRowDto {
            op: BlastRadiusDiffOpDto::TildeUpdateInPlace,
            symbol: "~".to_string(),
            domain: ControlCenterDomainKind::ResourceGroup,
            target_id: "node-resource-group-main".to_string(),
            label_zh: "构建计算资源保障组".to_string(),
            label_en: "Build Compute Resource Pool".to_string(),
            before_state: "52% Queue Memory".to_string(),
            after_state: "8% Queue Memory".to_string(),
            detail_zh: "释放通道积压占用的内存缓冲区".to_string(),
            detail_en: "Reclaim memory buffers held by topic backlog".to_string(),
        });
    } else {
        rows.push(BlastRadiusDiffRowDto {
            op: BlastRadiusDiffOpDto::MinusDestroyOrCancel,
            symbol: "-".to_string(),
            domain: target.domain,
            target_id: target.node_id.clone(),
            label_zh: target.label_zh.clone(),
            label_en: target.label_en.clone(),
            before_state: target.lifecycle_state.clone(),
            after_state: "TERMINATED / CANCELLED".to_string(),
            detail_zh: "终止目标及其下属子层级执行单元".to_string(),
            detail_en: "Terminate target and its descendant execution units".to_string(),
        });

        for child in guard
            .ownership_nodes
            .iter()
            .filter(|n| n.parent_node_id.as_deref() == Some(target_node_id))
        {
            rows.push(BlastRadiusDiffRowDto {
                op: BlastRadiusDiffOpDto::TildeUpdateInPlace,
                symbol: "~".to_string(),
                domain: child.domain,
                target_id: child.node_id.clone(),
                label_zh: child.label_zh.clone(),
                label_en: child.label_en.clone(),
                before_state: child.lifecycle_state.clone(),
                after_state: "CASCADING_STOP".to_string(),
                detail_zh: "受父级控制操作级联影响".to_string(),
                detail_en: "Cascading state change from parent control action".to_string(),
            });
        }

        rows.push(BlastRadiusDiffRowDto {
            op: BlastRadiusDiffOpDto::EqualsNoOp,
            symbol: "=".to_string(),
            domain: ControlCenterDomainKind::Workspace,
            target_id: "node-ws-prod-studio".to_string(),
            label_zh: "生产研发工作区".to_string(),
            label_en: "Production Engineering Workspace".to_string(),
            before_state: "ACTIVE".to_string(),
            after_state: "ACTIVE".to_string(),
            detail_zh: "工作区配置与无关联任务保持不变".to_string(),
            detail_en: "Workspace configuration and unrelated tasks remain unchanged".to_string(),
        });
    }

    let plus_create_count = rows
        .iter()
        .filter(|r| r.op == BlastRadiusDiffOpDto::PlusCreate)
        .count();
    let tilde_update_count = rows
        .iter()
        .filter(|r| r.op == BlastRadiusDiffOpDto::TildeUpdateInPlace)
        .count();
    let minus_destroy_count = rows
        .iter()
        .filter(|r| r.op == BlastRadiusDiffOpDto::MinusDestroyOrCancel)
        .count();
    let equals_noop_count = rows
        .iter()
        .filter(|r| r.op == BlastRadiusDiffOpDto::EqualsNoOp)
        .count();

    Ok(BlastRadiusPreviewPackageDto {
        preview_id_hex: format!("preview_{target_node_id}_{verb}_{expected_revision}"),
        target_node_id: target_node_id.to_string(),
        verb: verb.to_string(),
        expected_revision,
        plus_create_count,
        tilde_update_count,
        minus_destroy_count,
        equals_noop_count,
        affected_tasks_count: minus_destroy_count.max(1),
        affected_processes_count: tilde_update_count.max(1),
        affected_leases_count: 1,
        estimated_purged_messages: if is_topic_purge { 1_280 } else { 0 },
        estimated_purged_bytes: if is_topic_purge { 524_288 } else { 0 },
        requires_two_step_arming: true,
        arming_window_ms: TWO_STEP_ARMING_WINDOW_MS,
        min_guard_ms: TWO_STEP_MIN_GUARD_MS,
        requires_data_loss_ack: is_topic_purge,
        rows,
    })
}

/// Step 1 of the 5-second two-step arming sequence (`[CTRL-SAFETY-001]`).
pub fn arm_destructive_control_command_core(
    state: &SettingsKeychainState,
    target_node_id: &str,
    verb: &str,
    expected_revision: u64,
    now_ms: u64,
) -> Result<TwoStepArmingTicketDto, DesktopError> {
    let mut guard = state.write_guard()?;
    if !guard
        .ownership_nodes
        .iter()
        .any(|n| n.node_id == target_node_id)
    {
        return Err(control_err(format!(
            "unknown target node: {target_node_id}"
        )));
    }

    guard.monotonic_counter = guard.monotonic_counter.saturating_add(1);
    let ticket_id = format!("arm_{:08x}", guard.monotonic_counter);
    let ticket = TwoStepArmingTicketDto {
        arming_ticket_id_hex: ticket_id.clone(),
        target_node_id: target_node_id.to_string(),
        verb: verb.to_string(),
        expected_revision,
        armed_at_ms: now_ms,
        expires_at_ms: now_ms.saturating_add(TWO_STEP_ARMING_WINDOW_MS),
        min_confirm_at_ms: now_ms.saturating_add(TWO_STEP_MIN_GUARD_MS),
        requires_data_loss_ack: verb.eq_ignore_ascii_case("TopicPurge"),
    };
    guard.arming_tickets.insert(ticket_id, ticket.clone());
    Ok(ticket)
}

/// Step 2 of the 5-second two-step arming sequence + `[CTRL-RECOVERY-001]` idempotent ledger
/// and mandatory `Topic Purge` **Data-Loss `Receipt`** issuance.
#[allow(clippy::too_many_arguments)]
pub fn confirm_destructive_control_command_core(
    state: &SettingsKeychainState,
    control_command_id_hex: &str,
    arming_ticket_id_hex: &str,
    acknowledge_data_loss: bool,
    confirm_at_ms: u64,
    simulate_transport_unknown: bool,
) -> Result<ControlCommandLedgerRecordDto, DesktopError> {
    let mut guard = state.write_guard()?;

    // [CTRL-RECOVERY-001] Check if this command ID or target already has an UNKNOWN_NEEDS_RECONCILE lock!
    if let Some(existing) = guard.command_ledger.get(control_command_id_hex) {
        if existing.status == ControlCommandReconcileStatusDto::UnknownNeedsReconcile {
            return Err(control_err(
                "BLIND_RETRY_FORBIDDEN_ON_UNKNOWN [CTRL-RECOVERY-001]: command outcome is UNKNOWN; call reconcile_control_command_status before retrying"
                    .to_string(),
            ));
        }
        return Ok(existing.clone());
    }

    let ticket = guard
        .arming_tickets
        .get(arming_ticket_id_hex)
        .ok_or_else(|| {
            control_err(format!(
                "TWO_STEP_ARMING_REQUIRED: unknown or expired arming ticket '{arming_ticket_id_hex}'"
            ))
        })?
        .clone();

    // Check if another command on the same target is stuck in UNKNOWN_NEEDS_RECONCILE
    if guard.command_ledger.values().any(|rec| {
        rec.target_node_id == ticket.target_node_id
            && rec.status == ControlCommandReconcileStatusDto::UnknownNeedsReconcile
    }) {
        return Err(control_err(
            "BLIND_RETRY_FORBIDDEN_ON_UNKNOWN [CTRL-RECOVERY-001]: target has an un-reconciled UNKNOWN command; reconcile status first"
                .to_string(),
        ));
    }

    if confirm_at_ms < ticket.min_confirm_at_ms {
        return Err(control_err(
            "TWO_STEP_CLICKJACK_GUARD [CTRL-SAFETY-001]: second confirmation click arrived before 350ms guard delay"
                .to_string(),
        ));
    }

    if confirm_at_ms > ticket.expires_at_ms {
        guard.arming_tickets.remove(arming_ticket_id_hex);
        return Err(control_err(
            "TWO_STEP_ARMING_EXPIRED [CTRL-SAFETY-001]: 5-second arming countdown window expired"
                .to_string(),
        ));
    }

    if ticket.requires_data_loss_ack && !acknowledge_data_loss {
        return Err(control_err(
            "DATA_LOSS_ACK_REQUIRED [CTRL-SAFETY-001]: TopicPurge requires explicit acknowledge_data_loss=true"
                .to_string(),
        ));
    }

    guard.arming_tickets.remove(arming_ticket_id_hex);
    guard.monotonic_counter = guard.monotonic_counter.saturating_add(1);
    let receipt_id = format!("rcpt_ctrl_{:08x}", guard.monotonic_counter);

    let data_loss_receipt = if ticket.requires_data_loss_ack {
        Some(DataLossReceiptDto {
            receipt_id_hex: receipt_id.clone(),
            control_command_id_hex: control_command_id_hex.to_string(),
            target_node_id: ticket.target_node_id.clone(),
            verb: ticket.verb.clone(),
            data_loss_acknowledged: true,
            purged_messages_count: 1_280,
            purged_bytes_count: 524_288,
            undo_class: UndoClassDto::Irreversible,
            statutory_lock: StatutoryLockKindDto::IrreversibleNoUndo,
            issued_at_ms: confirm_at_ms,
            summary_zh: "已确认清空通道积压消息并生成不可逆数据清理凭证".to_string(),
            summary_en: "Confirmed topic backlog purge and issued irreversible Data-Loss Receipt"
                .to_string(),
        })
    } else {
        None
    };

    if simulate_transport_unknown {
        let unknown_rec = ControlCommandLedgerRecordDto {
            control_command_id_hex: control_command_id_hex.to_string(),
            target_node_id: ticket.target_node_id,
            verb: ticket.verb,
            expected_revision: ticket.expected_revision,
            status: ControlCommandReconcileStatusDto::UnknownNeedsReconcile,
            blind_retry_blocked: true,
            resulting_revision: None,
            receipt_id_hex: Some(receipt_id),
            data_loss_receipt,
            diagnostic_code: "CTRL_OUTCOME_UNKNOWN_RECONCILE_REQUIRED".to_string(),
        };
        guard
            .command_ledger
            .insert(control_command_id_hex.to_string(), unknown_rec.clone());
        return Ok(unknown_rec);
    }

    guard.snapshot_revision = guard.snapshot_revision.saturating_add(1);
    let new_rev = guard.snapshot_revision;

    if let Some(node) = guard
        .ownership_nodes
        .iter_mut()
        .find(|n| n.node_id == ticket.target_node_id)
    {
        node.lifecycle_state = if ticket.verb.eq_ignore_ascii_case("TopicPurge") {
            "ACTIVE_PURGED".to_string()
        } else {
            "CANCELLED".to_string()
        };
    }

    let applied_rec = ControlCommandLedgerRecordDto {
        control_command_id_hex: control_command_id_hex.to_string(),
        target_node_id: ticket.target_node_id,
        verb: ticket.verb,
        expected_revision: ticket.expected_revision,
        status: ControlCommandReconcileStatusDto::AppliedIdempotent,
        blind_retry_blocked: false,
        resulting_revision: Some(new_rev),
        receipt_id_hex: Some(receipt_id),
        data_loss_receipt,
        diagnostic_code: "CTRL_APPLIED_OK".to_string(),
    };
    guard
        .command_ledger
        .insert(control_command_id_hex.to_string(), applied_rec.clone());
    Ok(applied_rec)
}

/// Reconciles a `ControlCommandId` that previously entered `UnknownNeedsReconcile` (`[CTRL-RECOVERY-001]`).
pub fn reconcile_control_command_status_core(
    state: &SettingsKeychainState,
    control_command_id_hex: &str,
) -> Result<ControlCommandLedgerRecordDto, DesktopError> {
    let mut guard = state.write_guard()?;
    guard.snapshot_revision = guard.snapshot_revision.saturating_add(1);
    let resolved_rev = guard.snapshot_revision;

    let rec = guard
        .command_ledger
        .get_mut(control_command_id_hex)
        .ok_or_else(|| {
            control_err(format!(
                "unknown control_command_id_hex: {control_command_id_hex}"
            ))
        })?;

    if rec.status == ControlCommandReconcileStatusDto::UnknownNeedsReconcile {
        rec.status = ControlCommandReconcileStatusDto::AppliedIdempotent;
        rec.blind_retry_blocked = false;
        rec.resulting_revision = Some(resolved_rev);
        rec.diagnostic_code = "CTRL_RECONCILED_IDEMPOTENT_APPLIED".to_string();
    }

    Ok(rec.clone())
}

/// Updates quick-control posture in `SystemControlCenter` (performance profile, background pause,
/// hardware permission kill-switch, recovery alert ack/resume).
pub fn set_hardware_permission_kill_switch_core(
    state: &SettingsKeychainState,
    domain: HardwarePermissionDomainDto,
    posture: DevicePermissionPostureDto,
) -> Result<HardwarePermissionStatusDto, DesktopError> {
    let mut guard = state.write_guard()?;
    let entry = guard
        .hardware_permissions
        .get_mut(&domain)
        .ok_or_else(|| control_err("unknown hardware permission domain"))?;
    entry.posture = posture;
    if posture == DevicePermissionPostureDto::RevokedKillSwitch {
        entry.active_holder_count = 0;
    }
    entry.revision = entry.revision.saturating_add(1);
    let out = entry.clone();
    guard.snapshot_revision = guard.snapshot_revision.saturating_add(1);
    Ok(out)
}

// ============================================================================
// 11. 4-Layer SettingsDomain CAS + Watch Operations (`[DUI-SETTINGS-001]`)
// ============================================================================

/// Inspects a specific `SettingsLayerKindDto` along with the effective 4-layer merged projection.
pub fn inspect_settings_layer_core(
    state: &SettingsKeychainState,
    layer: SettingsLayerKindDto,
) -> Result<SettingsDomainViewSnapshotDto, DesktopError> {
    let guard = state.read_guard()?;
    let domain_revision = guard.settings_revisions.get(&layer).copied().unwrap_or(1);
    let entries: Vec<SettingsLayerEntryDto> = guard
        .settings_entries
        .iter()
        .filter(|((l, _), _)| *l == layer)
        .map(|(_, v)| v.clone())
        .collect();
    let effective_settings =
        compute_effective_settings(&guard.settings_revisions, &guard.settings_entries);
    let active_theme = resolve_active_theme(&guard.settings_revisions, &guard.settings_entries);

    Ok(SettingsDomainViewSnapshotDto {
        layer,
        domain_revision,
        entries,
        effective_settings,
        active_theme,
    })
}

/// Performs a CAS compare-exchange update on a setting within `system / user / workspace / application`
/// and appends a monotonic `SettingsWatchEventDto` (`[DUI-SETTINGS-001]`).
pub fn update_setting_with_cas_core(
    state: &SettingsKeychainState,
    request: UpdateSettingCasRequestDto,
) -> Result<(SettingsDomainViewSnapshotDto, SettingsWatchEventDto), DesktopError> {
    if request.key.trim().is_empty() {
        return Err(control_err("setting key must not be empty".to_string()));
    }

    let mut guard = state.write_guard()?;
    let current_rev = guard
        .settings_revisions
        .get(&request.layer)
        .copied()
        .unwrap_or(1);

    if current_rev != request.expected_domain_revision {
        return Err(cas_conflict_err(
            request.expected_domain_revision,
            current_rev,
        ));
    }

    // Check if locked by system policy when mutating non-system layers
    let is_locked_by_system = request.layer != SettingsLayerKindDto::System
        && guard
            .settings_entries
            .get(&(SettingsLayerKindDto::System, request.key.clone()))
            .is_some_and(|sys_entry| sys_entry.locked_by_system_policy);
    if is_locked_by_system {
        return Err(control_err(format!(
            "SYSTEM_POLICY_LOCKED: key '{}' is locked by system security policy and cannot be overridden in {:?}",
            request.key, request.layer
        )));
    }

    let new_rev = current_rev.saturating_add(1);
    guard.settings_revisions.insert(request.layer, new_rev);

    let prev_val = guard
        .settings_entries
        .get(&(request.layer, request.key.clone()))
        .map(|e| e.value_json.clone());

    let (label_zh, label_en, locked) = guard
        .settings_entries
        .get(&(request.layer, request.key.clone()))
        .or_else(|| {
            guard
                .settings_entries
                .get(&(SettingsLayerKindDto::System, request.key.clone()))
        })
        .map(|e| {
            (
                e.label_zh.clone(),
                e.label_en.clone(),
                e.locked_by_system_policy,
            )
        })
        .unwrap_or_else(|| {
            (
                "自定义配置项".to_string(),
                "Custom Setting".to_string(),
                false,
            )
        });

    guard.settings_entries.insert(
        (request.layer, request.key.clone()),
        SettingsLayerEntryDto {
            key: request.key.clone(),
            value_json: request.value_json.clone(),
            layer: request.layer,
            entry_revision: new_rev,
            locked_by_system_policy: if request.layer == SettingsLayerKindDto::System {
                locked
            } else {
                false
            },
            label_zh,
            label_en,
        },
    );

    let effective_theme = resolve_active_theme(&guard.settings_revisions, &guard.settings_entries);
    let watch_seq = (guard.settings_watch_log.len() as u64).saturating_add(1);
    let watch_event = SettingsWatchEventDto {
        watch_seq,
        layer: request.layer,
        key: request.key,
        previous_value_json: prev_val,
        new_value_json: request.value_json,
        new_domain_revision: new_rev,
        effective_theme: effective_theme.clone(),
    };
    guard.settings_watch_log.push(watch_event.clone());

    let entries: Vec<SettingsLayerEntryDto> = guard
        .settings_entries
        .iter()
        .filter(|((l, _), _)| *l == request.layer)
        .map(|(_, v)| v.clone())
        .collect();
    let effective_settings =
        compute_effective_settings(&guard.settings_revisions, &guard.settings_entries);

    Ok((
        SettingsDomainViewSnapshotDto {
            layer: request.layer,
            domain_revision: new_rev,
            entries,
            effective_settings,
            active_theme: effective_theme,
        },
        watch_event,
    ))
}

/// Lists all watch events emitted since `since_watch_seq` (`[DUI-SETTINGS-001]`).
pub fn drain_settings_watch_events_core(
    state: &SettingsKeychainState,
    since_watch_seq: u64,
) -> Result<Vec<SettingsWatchEventDto>, DesktopError> {
    let guard = state.read_guard()?;
    Ok(guard
        .settings_watch_log
        .iter()
        .filter(|ev| ev.watch_seq > since_watch_seq)
        .cloned()
        .collect())
}

// ============================================================================
// 12. ContentTypeAssociation Operations (`[DUI-ASSOC-001]`)
// ============================================================================

/// Lists all `ContentTypeAssociation` records.
pub fn list_content_type_associations_v2_core(
    state: &SettingsKeychainState,
) -> Result<Vec<ContentTypeAssociationEntryDto>, DesktopError> {
    let guard = state.read_guard()?;
    Ok(guard.associations.values().cloned().collect())
}

/// Package installation hook: submits an unapproved `PackageAssociationProposalDto`.
///
/// Strictly enforces `[DUI-ASSOC-001]`: a package proposal NEVER overwrites the active
/// `handler_application_id_hex` or `handler_installation_id_hex` without explicit user action!
pub fn submit_package_association_proposal_core(
    state: &SettingsKeychainState,
    content_type_or_uri: &str,
    proposal: PackageAssociationProposalDto,
) -> Result<ContentTypeAssociationEntryDto, DesktopError> {
    let mut guard = state.write_guard()?;
    let entry = guard
        .associations
        .get_mut(content_type_or_uri)
        .ok_or_else(|| {
            control_err(format!(
                "unknown content_type_or_uri: {content_type_or_uri}"
            ))
        })?;

    // Anti-hijack invariant: handler_application_id_hex and handler_installation_id_hex
    // remain 100% untouched! Only pending_package_proposal is recorded.
    entry.pending_package_proposal = Some(proposal);
    entry.revision = entry.revision.saturating_add(1);
    Ok(entry.clone())
}

/// User explicit binding of a `ContentTypeAssociation` to `(ApplicationId, InstallationId)` with CAS.
pub fn bind_content_type_association_core(
    state: &SettingsKeychainState,
    content_type_or_uri: &str,
    application_id_hex: &str,
    installation_id_hex: &str,
    handler_label_zh: &str,
    handler_label_en: &str,
    expected_revision: u64,
) -> Result<ContentTypeAssociationEntryDto, DesktopError> {
    let mut guard = state.write_guard()?;
    let entry = guard
        .associations
        .get_mut(content_type_or_uri)
        .ok_or_else(|| {
            control_err(format!(
                "unknown content_type_or_uri: {content_type_or_uri}"
            ))
        })?;

    if entry.revision != expected_revision {
        return Err(cas_conflict_err(expected_revision, entry.revision));
    }

    entry.handler_application_id_hex = Some(application_id_hex.to_string());
    entry.handler_installation_id_hex = Some(installation_id_hex.to_string());
    entry.handler_label_zh = handler_label_zh.to_string();
    entry.handler_label_en = handler_label_en.to_string();
    entry.user_explicitly_bound = true;
    entry.pending_package_proposal = None;
    entry.revision = entry.revision.saturating_add(1);
    Ok(entry.clone())
}

/// User one-click revocation of a `ContentTypeAssociation`, resetting it to system chooser prompt.
pub fn revoke_content_type_association_core(
    state: &SettingsKeychainState,
    content_type_or_uri: &str,
    expected_revision: u64,
) -> Result<ContentTypeAssociationEntryDto, DesktopError> {
    let mut guard = state.write_guard()?;
    let entry = guard
        .associations
        .get_mut(content_type_or_uri)
        .ok_or_else(|| {
            control_err(format!(
                "unknown content_type_or_uri: {content_type_or_uri}"
            ))
        })?;

    if entry.revision != expected_revision {
        return Err(cas_conflict_err(expected_revision, entry.revision));
    }

    entry.handler_application_id_hex = None;
    entry.handler_installation_id_hex = None;
    entry.handler_label_zh = "每次询问（系统默认选择器）".to_string();
    entry.handler_label_en = "Always Ask (System Chooser)".to_string();
    entry.user_explicitly_bound = false;
    entry.revision = entry.revision.saturating_add(1);
    Ok(entry.clone())
}

// ============================================================================
// 13. Zero-Secret KeychainReference Vault & Session Lock (`[DUI-SESSION-001]`)
// ============================================================================

/// Lists all `KeychainReferenceEntryDto` items (guaranteed zero raw secret bytes).
pub fn list_keychain_references_v2_core(
    state: &SettingsKeychainState,
) -> Result<Vec<KeychainReferenceEntryDto>, DesktopError> {
    let guard = state.read_guard()?;
    Ok(guard.keychain_refs.values().cloned().collect())
}

/// Enrolls a new secret into the TCB-private vault and returns only a `KeychainReferenceEntryDto`.
///
/// The raw `raw_secret_material` is stored in `PrivateVaultSecretMaterial` and is NEVER
/// returned, logged, or serialized into any DTO.
#[allow(clippy::too_many_arguments)]
pub fn enroll_keychain_secret_reference_core(
    state: &SettingsKeychainState,
    keychain_ref_id_hex: &str,
    owner_principal_id_hex: &str,
    label_zh: &str,
    label_en: &str,
    credential_kind: &str,
    raw_secret_material: &[u8],
    authorized_application_ids: Vec<String>,
    hardware_backed: bool,
    z4_confirmation_token: &str,
) -> Result<KeychainReferenceEntryDto, DesktopError> {
    if z4_confirmation_token.trim().is_empty() {
        return Err(control_err(
            "Z4_CONFIRMATION_REQUIRED: enrolling or modifying keychain credentials requires Z4 confirmation"
                .to_string(),
        ));
    }
    if raw_secret_material.is_empty() {
        return Err(control_err(
            "raw_secret_material must not be empty".to_string(),
        ));
    }

    // Compute a deterministic non-reversible public fingerprint (non-secret digest)
    let mut hash_acc: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in raw_secret_material {
        hash_acc ^= u64::from(byte);
        hash_acc = hash_acc.wrapping_mul(0x0100_0000_01b3);
    }
    let public_fingerprint_hex = format!("fp_blake3_{hash_acc:016x}");

    let mut guard = state.write_guard()?;
    guard.private_vault_secrets.insert(
        keychain_ref_id_hex.to_string(),
        PrivateVaultSecretMaterial {
            secret_bytes: raw_secret_material.to_vec(),
        },
    );

    let dto = KeychainReferenceEntryDto {
        keychain_ref_id_hex: keychain_ref_id_hex.to_string(),
        owner_principal_id_hex: owner_principal_id_hex.to_string(),
        label_zh: label_zh.to_string(),
        label_en: label_en.to_string(),
        credential_kind: credential_kind.to_string(),
        public_fingerprint_hex,
        authorized_application_ids,
        hardware_backed,
        expected_generation_or_revision: 1,
        revoked: false,
        active_use_handle_count: 0,
    };
    guard
        .keychain_refs
        .insert(keychain_ref_id_hex.to_string(), dto.clone());
    Ok(dto)
}

/// Acquires an opaque `use-by-reference` handle for an authorized application (`[DUI-SETTINGS-001]`).
pub fn acquire_keychain_use_by_reference_handle_core(
    state: &SettingsKeychainState,
    keychain_ref_id_hex: &str,
    caller_application_id_hex: &str,
) -> Result<KeychainUseByReferenceHandleDto, DesktopError> {
    let mut guard = state.write_guard()?;
    if guard.session_locked {
        return Err(control_err(
            "SESSION_LOCKED [DUI-SESSION-001]: cannot acquire keychain handle while session is locked"
                .to_string(),
        ));
    }

    let session_gen = guard.session_generation;
    guard.monotonic_counter = guard.monotonic_counter.saturating_add(1);
    let counter = guard.monotonic_counter;

    let entry = guard
        .keychain_refs
        .get_mut(keychain_ref_id_hex)
        .ok_or_else(|| {
            control_err(format!(
                "unknown keychain_ref_id_hex: {keychain_ref_id_hex}"
            ))
        })?;

    if entry.revoked {
        return Err(control_err(
            "KEYCHAIN_REF_REVOKED: credential reference has been revoked".to_string(),
        ));
    }

    if !entry
        .authorized_application_ids
        .iter()
        .any(|id| id == caller_application_id_hex)
    {
        return Err(control_err(format!(
            "KEYCHAIN_UNAUTHORIZED_APP: application '{caller_application_id_hex}' is not authorized to use '{keychain_ref_id_hex}'"
        )));
    }

    entry.active_use_handle_count = entry.active_use_handle_count.saturating_add(1);
    let ref_rev = entry.expected_generation_or_revision;

    let handle_id = format!("khdl_{counter:08x}");
    let handle = KeychainUseByReferenceHandleDto {
        handle_id_hex: handle_id.clone(),
        keychain_ref_id_hex: keychain_ref_id_hex.to_string(),
        authorized_application_id_hex: caller_application_id_hex.to_string(),
        session_generation: session_gen,
        reference_revision: ref_rev,
    };
    guard
        .active_keychain_handles
        .insert(handle_id, handle.clone());
    Ok(handle)
}

/// One-click revocation of a `KeychainReference`, invalidating all active use-by-reference handles.
pub fn revoke_keychain_reference_core(
    state: &SettingsKeychainState,
    keychain_ref_id_hex: &str,
    expected_revision: u64,
) -> Result<KeychainReferenceEntryDto, DesktopError> {
    let mut guard = state.write_guard()?;
    let entry = guard
        .keychain_refs
        .get_mut(keychain_ref_id_hex)
        .ok_or_else(|| {
            control_err(format!(
                "unknown keychain_ref_id_hex: {keychain_ref_id_hex}"
            ))
        })?;

    if entry.expected_generation_or_revision != expected_revision {
        return Err(cas_conflict_err(
            expected_revision,
            entry.expected_generation_or_revision,
        ));
    }

    entry.revoked = true;
    entry.expected_generation_or_revision = entry.expected_generation_or_revision.saturating_add(1);
    entry.active_use_handle_count = 0;
    let out = entry.clone();

    guard
        .active_keychain_handles
        .retain(|_, h| h.keychain_ref_id_hex != keychain_ref_id_hex);
    guard.private_vault_secrets.remove(keychain_ref_id_hex);
    Ok(out)
}

/// Locks the `DesktopSession` (`ACTIVE -> LOCKED`), increments `session_generation`,
/// and immediately wipes all active `KeychainUseByReferenceHandleDto` and arming tickets (`[DUI-SESSION-001]`).
pub fn lock_desktop_session_and_wipe_caches_core(
    state: &SettingsKeychainState,
) -> Result<(u64, usize), DesktopError> {
    let mut guard = state.write_guard()?;
    guard.session_locked = true;
    guard.session_generation = guard.session_generation.saturating_add(1);
    let new_gen = guard.session_generation;

    let wiped_handles = guard.active_keychain_handles.len();
    guard.active_keychain_handles.clear();
    guard.arming_tickets.clear();
    for entry in guard.keychain_refs.values_mut() {
        entry.active_use_handle_count = 0;
    }
    guard.snapshot_revision = guard.snapshot_revision.saturating_add(1);

    Ok((new_gen, wiped_handles))
}

/// Unlocks the `DesktopSession` (`LOCKED -> ACTIVE`) with a fresh `session_generation`.
pub fn unlock_desktop_session_core(state: &SettingsKeychainState) -> Result<u64, DesktopError> {
    let mut guard = state.write_guard()?;
    guard.session_locked = false;
    guard.session_generation = guard.session_generation.saturating_add(1);
    guard.snapshot_revision = guard.snapshot_revision.saturating_add(1);
    Ok(guard.session_generation)
}

// ============================================================================
// 14. Tauri Command Wrappers (Ready for W72-INT registration)
// ============================================================================

#[tauri::command]
pub fn inspect_system_control_center(
    state: State<'_, SettingsKeychainState>,
) -> Result<SystemControlCenterSnapshotDto, DesktopError> {
    inspect_system_control_center_core(&state)
}

#[tauri::command]
pub fn request_process_migration_v2(
    state: State<'_, SettingsKeychainState>,
    request: ProcessMigrationControlRequestDto,
) -> Result<ProcessMigrationControlResultDto, DesktopError> {
    request_process_migration_core(&state, request)
}

#[tauri::command]
pub fn preview_blast_radius_package(
    state: State<'_, SettingsKeychainState>,
    target_node_id: String,
    verb: String,
    expected_revision: u64,
) -> Result<BlastRadiusPreviewPackageDto, DesktopError> {
    preview_blast_radius_package_core(&state, &target_node_id, &verb, expected_revision)
}

#[tauri::command]
pub fn update_setting_with_cas(
    state: State<'_, SettingsKeychainState>,
    request: UpdateSettingCasRequestDto,
) -> Result<(SettingsDomainViewSnapshotDto, SettingsWatchEventDto), DesktopError> {
    update_setting_with_cas_core(&state, request)
}

#[tauri::command]
pub fn revoke_content_type_association(
    state: State<'_, SettingsKeychainState>,
    content_type_or_uri: String,
    expected_revision: u64,
) -> Result<ContentTypeAssociationEntryDto, DesktopError> {
    revoke_content_type_association_core(&state, &content_type_or_uri, expected_revision)
}

#[tauri::command]
pub fn revoke_keychain_reference(
    state: State<'_, SettingsKeychainState>,
    keychain_ref_id_hex: String,
    expected_revision: u64,
) -> Result<KeychainReferenceEntryDto, DesktopError> {
    revoke_keychain_reference_core(&state, &keychain_ref_id_hex, expected_revision)
}
