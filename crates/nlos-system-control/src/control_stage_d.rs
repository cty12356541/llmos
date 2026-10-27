//! Stage D (`IPC-D1..D8` & `[UI-PARITY-001]`) client-side compilation,
//! snapshot decoding, and deterministic `ControlReceipt::to_bytes` encoding.

use nlos_schema::sabi::v1::{
    BlastRadiusImpactSign, CancelCommand, CellLeaseKind, CellLeaseLockState, CellReachabilityState,
    ContextResidencyTier, ControlCommandLifecycleState, ControlCommandSource, ControlScope,
    DisableApplicationCommand, DurableOperationState, Envelope, ExecutionFiberLifecycleState,
    ExecutionFiberPhase, GetSystemControlRequest, IssueChooserGrantCommand, KillCommand,
    MutateSearchCandidateCommand, MutateTaskNodeTopologyCommand,
    OverrideEffectUnknownOperationCommand, OverrideTaskAttemptCommitCommand,
    PartitionAutonomyGateState, PauseCommand, PlanNodeKind, PlanNodeLifecycleState,
    PurgeTopicCommand, ReclaimCommand, RequestProcessMigrationCommand, ResumeCommand,
    SetContentTypeAssociationCommand, SettingsDomainKind, SubmitControlCommandRequest,
    SystemControlView, TaskAttemptLifecycleState, TaskGroupLifecycleState, TaskLifecycleState,
    TaskUndoClass, ThrottleCommand, UndoReceiptCommand, UninstallApplicationCommand,
    UpdateSettingsDomainCommand, control_command,
};
use nlos_schema::{
    MAX_DESKTOP_WATERMARK_EVENT_LIMIT, MAX_SEARCH_QUERY_BYTES, MAX_SETTINGS_KEY_BYTES,
    MAX_SETTINGS_VALUE_CBOR_BYTES, MAX_SYSTEM_CONTROL_ENUM_LIMIT,
    SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL, SHA256_DIGEST_BYTES,
    decode_cell_lease_inspect_snapshot, decode_cell_list_snapshot,
    decode_chooser_grant_list_snapshot, decode_content_type_association_list_snapshot,
    decode_control_blast_radius_preview_snapshot, decode_control_command_status_snapshot,
    decode_desktop_watermark_poll_snapshot, decode_durable_operation_list_snapshot,
    decode_execution_fiber_list_snapshot, decode_installed_application_list_snapshot,
    decode_keychain_reference_list_snapshot, decode_notification_list_snapshot,
    decode_process_list_snapshot, decode_resource_reservation_list_snapshot,
    decode_search_candidate_query_snapshot, decode_settings_domain_snapshot,
    decode_task_attempt_list_snapshot, decode_task_group_list_snapshot, decode_task_list_snapshot,
    decode_task_node_list_snapshot, decode_topic_list_snapshot, decode_workspace_list_snapshot,
    decode_workspace_snapshot, encode_get_system_control_request,
    encode_submit_control_command_request, pack_blast_radius_preview_generation,
    system_control_schema_identity,
};

use crate::cell_inspector::{
    CellLeaseInspectPage, CellLeaseRecord, CellListPage, CellSummaryRecord,
};
use crate::control::{
    ControlCommand, ControlError, ControlOutcome, DurableOperationInspection,
    ExecutionFiberInspection, INSPECT_ALERT_LIMIT, LOCAL_ISSUER_PRINCIPAL_ID,
    NotificationInspection, ScopedControlAction, TaskNodeInspection, TopicInspection,
    decoded_result_receipt_pub,
};
use crate::desktop_enum::{
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
use crate::{GET_METHOD, SUBMIT_METHOD};

const fn fold_read_id(
    domain_tag: u8,
    primary: Option<&[u8; 16]>,
    secondary: Option<&[u8; 16]>,
    aux_u64: u64,
    limit: u32,
) -> [u8; 16] {
    let mut out = [domain_tag; 16];
    if let Some(id) = primary {
        let mut i = 0;
        while i < 16 {
            out[i] ^= id[i];
            i += 1;
        }
    }
    if let Some(sec) = secondary {
        let mut i = 0;
        while i < 16 {
            out[i] ^= sec[15 - i];
            i += 1;
        }
    }
    let aux_bytes = aux_u64.to_le_bytes();
    let lim_bytes = limit.to_le_bytes();
    let mut j = 0;
    while j < 8 {
        out[j] ^= aux_bytes[j];
        j += 1;
    }
    let mut k = 0;
    while k < 4 {
        out[8 + k] ^= lim_bytes[k];
        k += 1;
    }
    let mut all_zero = true;
    let mut m = 0;
    while m < 16 {
        if out[m] != 0 {
            all_zero = false;
            break;
        }
        m += 1;
    }
    if all_zero { [domain_tag; 16] } else { out }
}

const fn fold_search_query_id(scope_id: &[u8; 32], cursor_offset: u64, limit: u32) -> [u8; 16] {
    let mut half_a = [0_u8; 16];
    let mut half_b = [0_u8; 16];
    let mut i = 0;
    while i < 16 {
        half_a[i] = scope_id[i];
        half_b[i] = scope_id[16 + i];
        i += 1;
    }
    fold_read_id(0xD8, Some(&half_a), Some(&half_b), cursor_offset, limit)
}

#[allow(clippy::too_many_lines)]
pub(crate) const fn stage_d_command_id(command: &ControlCommand) -> Option<[u8; 16]> {
    match command {
        ControlCommand::ListTasks { cursor, limit } => {
            Some(fold_read_id(0xD1, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::ListTaskGroups {
            task_id,
            cursor,
            limit,
        } => Some(fold_read_id(
            0xD2,
            cursor.as_ref(),
            task_id.as_ref(),
            0,
            *limit,
        )),
        ControlCommand::ListTaskNodes {
            plan_id,
            expected_plan_revision,
            cursor,
            limit,
        } => Some(fold_read_id(
            0xD3,
            Some(plan_id),
            cursor.as_ref(),
            *expected_plan_revision,
            *limit,
        )),
        ControlCommand::ListTaskAttempts {
            scope_filter_id,
            expected_generation,
            cursor,
            limit,
        } => Some(fold_read_id(
            0xD4,
            scope_filter_id.as_ref(),
            cursor.as_ref(),
            *expected_generation,
            *limit,
        )),
        ControlCommand::ListExecutionFibers {
            process_id,
            expected_generation,
            cursor,
            limit,
        } => Some(fold_read_id(
            0xD5,
            process_id.as_ref(),
            cursor.as_ref(),
            *expected_generation,
            *limit,
        )),
        ControlCommand::ListOperations {
            fiber_id,
            expected_generation,
            cursor,
            limit,
        } => Some(fold_read_id(
            0xD6,
            fiber_id.as_ref(),
            cursor.as_ref(),
            *expected_generation,
            *limit,
        )),
        ControlCommand::ListInstalledApplications { cursor, limit } => {
            Some(fold_read_id(0xD7, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::QuerySearchCandidates {
            scope_id,
            cursor_offset,
            limit,
            ..
        } => Some(fold_search_query_id(scope_id, *cursor_offset, *limit)),
        ControlCommand::PollDesktopWatermarkEvents {
            since_watermark,
            limit,
        } => Some(fold_read_id(0xD9, None, None, *since_watermark, *limit)),
        ControlCommand::InspectSettingsDomain {
            domain_kind,
            scope_target_id,
            limit,
        } => Some(fold_read_id(
            0xDA,
            scope_target_id.as_ref(),
            None,
            (*domain_kind as i32).cast_unsigned() as u64,
            *limit,
        )),
        ControlCommand::ListContentTypeAssociations { cursor, limit } => {
            Some(fold_read_id(0xDB, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::ListKeychainReferences { cursor, limit } => {
            Some(fold_read_id(0xDC, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::ListChooserGrants { cursor, limit } => {
            Some(fold_read_id(0xDD, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::ListWorkspaces { cursor, limit } => {
            Some(fold_read_id(0xDE, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::InspectWorkspace {
            workspace_id,
            expected_revision,
        } => Some(fold_read_id(
            0xDF,
            Some(workspace_id),
            None,
            *expected_revision,
            INSPECT_ALERT_LIMIT,
        )),
        ControlCommand::ListProcesses { cursor, limit } => {
            Some(fold_read_id(0xE0, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::ListResourceReservations { cursor, limit } => {
            Some(fold_read_id(0xE1, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::ListTopics { cursor, limit } => {
            Some(fold_read_id(0xE2, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::ListNotifications { cursor, limit } => {
            Some(fold_read_id(0xE3, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::ListCells { cursor, limit } => {
            Some(fold_read_id(0xE4, cursor.as_ref(), None, 0, *limit))
        }
        ControlCommand::InspectCellLeases { cell_id, limit } => {
            Some(fold_read_id(0xE5, cell_id.as_ref(), None, 0, *limit))
        }
        ControlCommand::PreviewControlBlastRadius {
            target_scope,
            target_id,
            secondary_id,
            preview_action,
            limit,
        } => {
            let aux =
                (((*target_scope as i32).cast_unsigned() as u64) << 32) | (*preview_action as u64);
            Some(fold_read_id(
                0xE6,
                Some(target_id),
                secondary_id.as_ref(),
                aux,
                *limit,
            ))
        }
        ControlCommand::QueryControlCommandStatus { target_command_id } => Some(fold_read_id(
            0xE7,
            Some(target_command_id),
            None,
            0,
            INSPECT_ALERT_LIMIT,
        )),
        ControlCommand::ScopedOperation {
            control_command_id, ..
        }
        | ControlCommand::RequestProcessMigration {
            control_command_id, ..
        }
        | ControlCommand::PurgeTopic {
            control_command_id, ..
        }
        | ControlCommand::UpdateSettingsDomain {
            control_command_id, ..
        }
        | ControlCommand::SetContentTypeAssociation {
            control_command_id, ..
        }
        | ControlCommand::IssueChooserGrant {
            control_command_id, ..
        }
        | ControlCommand::MutateSearchCandidate {
            control_command_id, ..
        }
        | ControlCommand::UndoReceipt {
            control_command_id, ..
        }
        | ControlCommand::MutateTaskNodeTopology {
            control_command_id, ..
        }
        | ControlCommand::OverrideTaskAttemptCommit {
            control_command_id, ..
        }
        | ControlCommand::OverrideEffectUnknownOperation {
            control_command_id, ..
        } => Some(*control_command_id),
        _ => None,
    }
}

pub(crate) const fn is_stage_d_mutation(command: &ControlCommand) -> bool {
    matches!(
        command,
        ControlCommand::ScopedOperation { .. }
            | ControlCommand::RequestProcessMigration { .. }
            | ControlCommand::PurgeTopic { .. }
            | ControlCommand::UpdateSettingsDomain { .. }
            | ControlCommand::SetContentTypeAssociation { .. }
            | ControlCommand::IssueChooserGrant { .. }
            | ControlCommand::MutateSearchCandidate { .. }
            | ControlCommand::UndoReceipt { .. }
            | ControlCommand::MutateTaskNodeTopology { .. }
            | ControlCommand::OverrideTaskAttemptCommit { .. }
            | ControlCommand::OverrideEffectUnknownOperation { .. }
    )
}

fn validate_enum_limit(limit: u32) -> Result<u32, ControlError> {
    if limit == 0 || usize::try_from(limit).unwrap_or(usize::MAX) > MAX_SYSTEM_CONTROL_ENUM_LIMIT {
        Err(ControlError::InvalidCommand(
            "enumeration limit must be between 1 and 64",
        ))
    } else {
        Ok(limit)
    }
}

fn validate_watermark_limit(limit: u32) -> Result<u32, ControlError> {
    if limit == 0
        || usize::try_from(limit).unwrap_or(usize::MAX) > MAX_DESKTOP_WATERMARK_EVENT_LIMIT
    {
        Err(ControlError::InvalidCommand(
            "watermark event limit must be between 1 and 128",
        ))
    } else {
        Ok(limit)
    }
}

fn opt_bytes(opt: Option<[u8; 16]>) -> Vec<u8> {
    opt.map_or_else(Vec::new, |id| id.to_vec())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn compile_stage_d_envelope_arm(
    command: &ControlCommand,
) -> Option<Result<(&'static str, Vec<u8>), ControlError>> {
    let get_req = match command {
        ControlCommand::ListTasks { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::TaskList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::ListTaskGroups {
            task_id,
            cursor,
            limit,
        } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::TaskGroupList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: opt_bytes(*task_id),
                target_generation: 0,
            }
        }
        ControlCommand::ListTaskNodes {
            plan_id,
            expected_plan_revision,
            cursor,
            limit,
        } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::TaskNodeList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: plan_id.to_vec(),
                target_generation: *expected_plan_revision,
            }
        }
        ControlCommand::ListTaskAttempts {
            scope_filter_id,
            expected_generation,
            cursor,
            limit,
        } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::TaskAttemptList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: opt_bytes(*scope_filter_id),
                target_generation: *expected_generation,
            }
        }
        ControlCommand::ListExecutionFibers {
            process_id,
            expected_generation,
            cursor,
            limit,
        } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::ExecutionFiberList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: opt_bytes(*process_id),
                target_generation: *expected_generation,
            }
        }
        ControlCommand::ListOperations {
            fiber_id,
            expected_generation,
            cursor,
            limit,
        } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::OperationList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: opt_bytes(*fiber_id),
                target_generation: *expected_generation,
            }
        }
        ControlCommand::ListInstalledApplications { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::InstalledApplicationList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::QuerySearchCandidates {
            scope_id,
            query,
            cursor_offset,
            limit,
        } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            if query.len() > MAX_SEARCH_QUERY_BYTES {
                return Some(Err(ControlError::InvalidCommand(
                    "search query exceeds bounded 256-byte limit",
                )));
            }
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::SearchCandidateQuery.into(),
                alert_limit,
                target_id: scope_id.to_vec(),
                plan_id: query.as_bytes().to_vec(),
                target_generation: *cursor_offset,
            }
        }
        ControlCommand::PollDesktopWatermarkEvents {
            since_watermark,
            limit,
        } => {
            let alert_limit = match validate_watermark_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::DesktopWatermarkPoll.into(),
                alert_limit,
                target_id: Vec::new(),
                plan_id: Vec::new(),
                target_generation: *since_watermark,
            }
        }
        ControlCommand::InspectSettingsDomain {
            domain_kind,
            scope_target_id,
            limit,
        } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            if *domain_kind == SettingsDomainKind::Unspecified {
                return Some(Err(ControlError::InvalidCommand(
                    "settings domain kind must be explicitly specified",
                )));
            }
            let Ok(raw_kind) = u64::try_from(i32::from(*domain_kind)) else {
                return Some(Err(ControlError::InvalidCommand(
                    "invalid settings domain kind",
                )));
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::SettingsDomain.into(),
                alert_limit,
                target_id: opt_bytes(*scope_target_id),
                plan_id: Vec::new(),
                target_generation: raw_kind,
            }
        }
        ControlCommand::ListContentTypeAssociations { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::ContentTypeAssociationList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::ListKeychainReferences { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::KeychainReferenceList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::ListChooserGrants { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::ChooserGrantList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::ListWorkspaces { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::WorkspaceList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::InspectWorkspace {
            workspace_id,
            expected_revision,
        } => GetSystemControlRequest {
            schema: Some(system_control_schema_identity()),
            view: SystemControlView::Workspace.into(),
            alert_limit: INSPECT_ALERT_LIMIT,
            target_id: workspace_id.to_vec(),
            plan_id: Vec::new(),
            target_generation: *expected_revision,
        },
        ControlCommand::ListProcesses { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::ProcessList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::ListResourceReservations { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::ResourceReservationList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::ListTopics { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::TopicList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::ListNotifications { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::NotificationList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::ListCells { cursor, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::CellList.into(),
                alert_limit,
                target_id: opt_bytes(*cursor),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::InspectCellLeases { cell_id, limit } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::CellLeaseInspect.into(),
                alert_limit,
                target_id: opt_bytes(*cell_id),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        ControlCommand::PreviewControlBlastRadius {
            target_scope,
            target_id,
            secondary_id,
            preview_action,
            limit,
        } => {
            let alert_limit = match validate_enum_limit(*limit) {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            let packed = pack_blast_radius_preview_generation(*target_scope, *preview_action);
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::ControlBlastRadiusPreview.into(),
                alert_limit,
                target_id: target_id.to_vec(),
                plan_id: opt_bytes(*secondary_id),
                target_generation: packed,
            }
        }
        ControlCommand::QueryControlCommandStatus { target_command_id } => {
            GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::ControlCommandStatus.into(),
                alert_limit: INSPECT_ALERT_LIMIT,
                target_id: target_command_id.to_vec(),
                plan_id: Vec::new(),
                target_generation: 0,
            }
        }
        _ => return compile_stage_d_submit_arm(command),
    };

    Some(
        encode_get_system_control_request(&get_req)
            .map(|payload| (GET_METHOD, payload))
            .map_err(ControlError::from),
    )
}

#[allow(clippy::too_many_lines)]
fn compile_stage_d_submit_arm(
    command: &ControlCommand,
) -> Option<Result<(&'static str, Vec<u8>), ControlError>> {
    let (control_command_id, scope, target_id, cas, reason, wire_cmd) = match command {
        ControlCommand::ScopedOperation {
            scope,
            action,
            control_command_id,
            target_id,
            expected_generation_or_revision,
            reason,
        } => {
            if *scope == ControlScope::Unspecified {
                return Some(Err(ControlError::InvalidCommand(
                    "control scope must be explicitly specified",
                )));
            }
            let wire_cmd = match action {
                ScopedControlAction::Pause => {
                    control_command::Command::PauseOperation(PauseCommand {})
                }
                ScopedControlAction::Resume => {
                    control_command::Command::ResumeOperation(ResumeCommand {})
                }
                ScopedControlAction::Cancel => {
                    control_command::Command::CancelOperation(CancelCommand {})
                }
                ScopedControlAction::Kill => {
                    control_command::Command::KillOperation(KillCommand {})
                }
                ScopedControlAction::Throttle { throttle_percent } => {
                    if !(1..=100).contains(throttle_percent) {
                        return Some(Err(ControlError::InvalidCommand(
                            "throttle percent must be a whole percent from 1 to 100",
                        )));
                    }
                    control_command::Command::ThrottleOperation(ThrottleCommand {
                        throttle_percent: *throttle_percent,
                    })
                }
                ScopedControlAction::Reclaim => {
                    control_command::Command::ReclaimOperation(ReclaimCommand {})
                }
                ScopedControlAction::DisableApplication => {
                    control_command::Command::DisableApplication(DisableApplicationCommand {})
                }
                ScopedControlAction::UninstallApplication => {
                    control_command::Command::UninstallApplication(UninstallApplicationCommand {})
                }
            };
            (
                *control_command_id,
                *scope,
                *target_id,
                *expected_generation_or_revision,
                reason.as_str(),
                wire_cmd,
            )
        }
        ControlCommand::RequestProcessMigration {
            control_command_id,
            process_id,
            destination_cell_id,
            expected_generation_or_revision,
            reason,
        } => (
            *control_command_id,
            ControlScope::Process,
            *process_id,
            *expected_generation_or_revision,
            reason.as_str(),
            control_command::Command::RequestProcessMigration(RequestProcessMigrationCommand {
                destination_cell_id: destination_cell_id.to_vec(),
            }),
        ),
        ControlCommand::PurgeTopic {
            control_command_id,
            topic_id,
            expected_generation_or_revision,
            acknowledge_data_loss,
            reason,
        } => (
            *control_command_id,
            ControlScope::Topic,
            *topic_id,
            *expected_generation_or_revision,
            reason.as_str(),
            control_command::Command::PurgeTopic(PurgeTopicCommand {
                acknowledge_data_loss: *acknowledge_data_loss,
            }),
        ),
        ControlCommand::UpdateSettingsDomain {
            control_command_id,
            scope,
            target_id,
            expected_generation_or_revision,
            domain_kind,
            key,
            value_cbor,
            reason,
        } => {
            if *scope == ControlScope::Unspecified {
                return Some(Err(ControlError::InvalidCommand(
                    "control scope must be explicitly specified",
                )));
            }
            if key.is_empty() || key.len() > MAX_SETTINGS_KEY_BYTES {
                return Some(Err(ControlError::InvalidCommand(
                    "settings key must be non-empty and at most 128 bytes",
                )));
            }
            if value_cbor.len() > MAX_SETTINGS_VALUE_CBOR_BYTES {
                return Some(Err(ControlError::InvalidCommand(
                    "settings value_cbor exceeds bounded 4096-byte limit",
                )));
            }
            (
                *control_command_id,
                *scope,
                *target_id,
                *expected_generation_or_revision,
                reason.as_str(),
                control_command::Command::UpdateSettingsDomain(UpdateSettingsDomainCommand {
                    domain_kind: (*domain_kind).into(),
                    key: key.clone(),
                    value_cbor: value_cbor.clone(),
                }),
            )
        }
        ControlCommand::SetContentTypeAssociation {
            control_command_id,
            association_id,
            expected_generation_or_revision,
            content_type,
            application_id,
            reason,
        } => (
            *control_command_id,
            ControlScope::Application,
            *association_id,
            *expected_generation_or_revision,
            reason.as_str(),
            control_command::Command::SetContentTypeAssociation(SetContentTypeAssociationCommand {
                content_type: content_type.clone(),
                application_id: application_id.to_vec(),
            }),
        ),
        ControlCommand::IssueChooserGrant {
            control_command_id,
            grant_id,
            expected_generation_or_revision,
            application_id,
            chooser_kind,
            resource_handle,
            reason,
        } => (
            *control_command_id,
            ControlScope::Application,
            *grant_id,
            *expected_generation_or_revision,
            reason.as_str(),
            control_command::Command::IssueChooserGrant(IssueChooserGrantCommand {
                application_id: application_id.to_vec(),
                chooser_kind: *chooser_kind,
                resource_handle: resource_handle.to_vec(),
            }),
        ),
        ControlCommand::MutateSearchCandidate {
            control_command_id,
            candidate_id,
            scope_id,
            non_authoritative_restrictions_bits,
            expected_generation_or_revision,
            reason,
        } => (
            *control_command_id,
            ControlScope::Context,
            *candidate_id,
            *expected_generation_or_revision,
            reason.as_str(),
            control_command::Command::MutateSearchCandidate(MutateSearchCandidateCommand {
                scope_id: scope_id.to_vec(),
                non_authoritative_restrictions_bits: *non_authoritative_restrictions_bits,
            }),
        ),
        ControlCommand::UndoReceipt {
            control_command_id,
            scope,
            target_id,
            receipt_undo_class,
            expected_generation_or_revision,
            reason,
        } => {
            if *scope == ControlScope::Unspecified {
                return Some(Err(ControlError::InvalidCommand(
                    "control scope must be explicitly specified",
                )));
            }
            (
                *control_command_id,
                *scope,
                *target_id,
                *expected_generation_or_revision,
                reason.as_str(),
                control_command::Command::UndoReceipt(UndoReceiptCommand {
                    receipt_undo_class: (*receipt_undo_class).into(),
                }),
            )
        }
        ControlCommand::MutateTaskNodeTopology {
            control_command_id,
            plan_id,
            node_id,
            observed_node_state,
            expected_generation_or_revision,
            reason,
        } => (
            *control_command_id,
            ControlScope::TaskNode,
            *node_id,
            *expected_generation_or_revision,
            reason.as_str(),
            control_command::Command::MutateTaskNodeTopology(MutateTaskNodeTopologyCommand {
                plan_id: plan_id.to_vec(),
                observed_node_state: (*observed_node_state).into(),
            }),
        ),
        ControlCommand::OverrideTaskAttemptCommit {
            control_command_id,
            attempt_id,
            expected_generation_or_revision,
            reason,
        } => (
            *control_command_id,
            ControlScope::TaskAttempt,
            *attempt_id,
            *expected_generation_or_revision,
            reason.as_str(),
            control_command::Command::OverrideTaskAttemptCommit(
                OverrideTaskAttemptCommitCommand {},
            ),
        ),
        ControlCommand::OverrideEffectUnknownOperation {
            control_command_id,
            operation_id,
            mark_succeeded,
            expected_generation_or_revision,
            reason,
        } => (
            *control_command_id,
            ControlScope::Operation,
            *operation_id,
            *expected_generation_or_revision,
            reason.as_str(),
            control_command::Command::OverrideEffectUnknownOperation(
                OverrideEffectUnknownOperationCommand {
                    mark_succeeded: *mark_succeeded,
                },
            ),
        ),
        _ => return None,
    };

    if reason.is_empty() {
        return Some(Err(ControlError::InvalidCommand(
            "control mutations require a non-empty bounded reason",
        )));
    }
    let req = SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(nlos_schema::sabi::v1::ControlCommand {
            control_command_id: control_command_id.to_vec(),
            issuer_principal_id: LOCAL_ISSUER_PRINCIPAL_ID.to_vec(),
            source: ControlCommandSource::Cli.into(),
            scope: scope.into(),
            target_id: target_id.to_vec(),
            expected_generation_or_revision: cas,
            command: Some(wire_cmd),
            reason: reason.to_owned(),
        }),
    };
    Some(
        encode_submit_control_command_request(&req)
            .map(|payload| (SUBMIT_METHOD, payload))
            .map_err(ControlError::from),
    )
}

fn fixed16(bytes: &[u8]) -> Result<[u8; 16], ControlError> {
    bytes
        .try_into()
        .map_err(|_| ControlError::UnexpectedResponse("snapshot carried an invalid 16-byte id"))
}

fn opt_fixed16(bytes: &[u8]) -> Result<Option<[u8; 16]>, ControlError> {
    if bytes.is_empty() {
        Ok(None)
    } else {
        fixed16(bytes).map(Some)
    }
}

fn fixed32(bytes: &[u8]) -> Result<[u8; SHA256_DIGEST_BYTES], ControlError> {
    bytes
        .try_into()
        .map_err(|_| ControlError::UnexpectedResponse("snapshot carried an invalid 32-byte digest"))
}

#[allow(clippy::too_many_lines)]
pub(crate) fn decode_stage_d_outcome(
    command: &ControlCommand,
    response: &Envelope,
) -> Option<Result<ControlOutcome, ControlError>> {
    match command {
        ControlCommand::ListTasks { .. } => Some((|| {
            let snap = decode_task_list_snapshot(&response.payload)?;
            let mut tasks = Vec::with_capacity(snap.tasks.len());
            for t in snap.tasks {
                tasks.push(TaskSummaryRecord {
                    task_id: fixed16(&t.task_id)?,
                    plan_id: fixed16(&t.plan_id)?,
                    application_id: opt_fixed16(&t.application_id)?,
                    installation_generation: t.installation_generation,
                    plan_revision: t.plan_revision,
                    task_state: TaskLifecycleState::try_from(t.task_state).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified task lifecycle state")
                    })?,
                    undo_class: TaskUndoClass::try_from(t.undo_class).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified task undo class")
                    })?,
                    reservation_id: opt_fixed16(&t.reservation_id)?,
                    cell_id: opt_fixed16(&t.cell_id)?,
                    updated_at_ms: t.updated_at_ms,
                    expected_generation_or_revision: t.expected_generation_or_revision,
                });
            }
            Ok(ControlOutcome::TasksListed(TaskListPage {
                tasks,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::ListTaskGroups { .. } => Some((|| {
            let snap = decode_task_group_list_snapshot(&response.payload)?;
            let mut groups = Vec::with_capacity(snap.groups.len());
            for g in snap.groups {
                groups.push(TaskGroupSummaryRecord {
                    group_id: fixed16(&g.group_id)?,
                    task_id: fixed16(&g.task_id)?,
                    parent_group_id: opt_fixed16(&g.parent_group_id)?,
                    state: TaskGroupLifecycleState::try_from(g.state).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified task group state")
                    })?,
                    membership_generation: g.membership_generation,
                    state_seq: g.state_seq,
                    depth: u32::try_from(g.depth).unwrap_or(u32::MAX),
                    cancel_epoch: g.cancel_epoch,
                    created_at_ms: u64::try_from(g.created_at_ms.max(0)).unwrap_or(0),
                    updated_at_ms: u64::try_from(g.updated_at_ms.max(0)).unwrap_or(0),
                });
            }
            Ok(ControlOutcome::TaskGroupsListed(TaskGroupListPage {
                groups,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::ListTaskNodes {
            plan_id,
            expected_plan_revision,
            ..
        } => Some((|| {
            let snap = decode_task_node_list_snapshot(&response.payload)?;
            if snap.stale_generation {
                return Ok(ControlOutcome::StaleGeneration {
                    view: SystemControlView::TaskNodeList,
                    target_id: Some(*plan_id),
                    expected_generation: *expected_plan_revision,
                    current_generation: snap.current_plan_revision,
                });
            }
            let mut nodes = Vec::with_capacity(snap.nodes.len());
            for n in snap.nodes {
                nodes.push(TaskNodeInspection {
                    plan_id: fixed16(&n.plan_id)?,
                    node_id: fixed16(&n.node_id)?,
                    kind: PlanNodeKind::try_from(n.kind).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified plan node kind")
                    })?,
                    state: PlanNodeLifecycleState::try_from(n.state).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified plan node state")
                    })?,
                    declared_revision: n.declared_revision,
                    node_digest: n.node_digest,
                    transition_count: n.transition_count,
                    residency_tier: ContextResidencyTier::try_from(n.residency_tier).map_err(
                        |_| ControlError::UnexpectedResponse("unspecified residency tier"),
                    )?,
                    residency_transition_count: n.residency_transition_count,
                    first_declared_at_ms: n.first_declared_at_ms,
                    updated_at_ms: n.updated_at_ms,
                });
            }
            Ok(ControlOutcome::TaskNodesListed(TaskNodeListPage {
                plan_id: fixed16(&snap.plan_id)?,
                current_plan_revision: snap.current_plan_revision,
                stale_generation: false,
                nodes,
                metadata_only_collapsed_count: snap.metadata_only_collapsed_count,
                cold_collapsed_count: snap.cold_collapsed_count,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::ListTaskAttempts {
            scope_filter_id,
            expected_generation,
            ..
        } => Some((|| {
            let snap = decode_task_attempt_list_snapshot(&response.payload)?;
            if snap.stale_generation {
                return Ok(ControlOutcome::StaleGeneration {
                    view: SystemControlView::TaskAttemptList,
                    target_id: *scope_filter_id,
                    expected_generation: *expected_generation,
                    current_generation: snap.current_generation,
                });
            }
            let mut attempts = Vec::with_capacity(snap.attempts.len());
            for a in snap.attempts {
                attempts.push(TaskAttemptSummaryRecord {
                    attempt_id: fixed16(&a.attempt_id)?,
                    task_id: fixed16(&a.task_id)?,
                    node_id: opt_fixed16(&a.node_id)?,
                    state: TaskAttemptLifecycleState::try_from(a.state).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified task attempt state")
                    })?,
                    isolation_domain_id: fixed16(&a.isolation_domain_id)?,
                    provider_isolation_unverified: a.provider_isolation_unverified,
                    cas_winner: a.cas_winner,
                    expected_generation_or_revision: a.expected_generation_or_revision,
                    started_at_ms: a.started_at_ms,
                    updated_at_ms: a.updated_at_ms,
                });
            }
            Ok(ControlOutcome::TaskAttemptsListed(TaskAttemptListPage {
                current_generation: snap.current_generation,
                stale_generation: false,
                attempts,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::ListExecutionFibers {
            process_id,
            expected_generation,
            ..
        } => Some((|| {
            let snap = decode_execution_fiber_list_snapshot(&response.payload)?;
            if snap.stale_generation {
                return Ok(ControlOutcome::StaleGeneration {
                    view: SystemControlView::ExecutionFiberList,
                    target_id: *process_id,
                    expected_generation: *expected_generation,
                    current_generation: snap.current_generation,
                });
            }
            let mut fibers = Vec::with_capacity(snap.fibers.len());
            for entry in snap.fibers {
                let f = entry.fiber.ok_or(ControlError::UnexpectedResponse(
                    "execution fiber summary lacked inner fiber status",
                ))?;
                fibers.push(ExecutionFiberSummaryRecord {
                    fiber: ExecutionFiberInspection {
                        fiber_id: fixed16(&f.fiber_id)?,
                        generation: f.generation,
                        state: ExecutionFiberLifecycleState::try_from(f.state).map_err(|_| {
                            ControlError::UnexpectedResponse("unspecified fiber state")
                        })?,
                        lifecycle_phase: ExecutionFiberPhase::try_from(f.lifecycle_phase).map_err(
                            |_| ControlError::UnexpectedResponse("unspecified fiber phase"),
                        )?,
                        active_cpu_ms: f.active_cpu_ms,
                        elapsed_wall_ms: f.elapsed_wall_ms,
                        scheduler_wait_ms: f.scheduler_wait_ms,
                        external_wait_ms: f.external_wait_ms,
                        backpressure_wait_ms: f.backpressure_wait_ms,
                        suspended_ms: f.suspended_ms,
                    },
                    process_id: opt_fixed16(&entry.process_id)?,
                    shared_thread_like: entry.shared_thread_like,
                });
            }
            Ok(ControlOutcome::ExecutionFibersListed(
                ExecutionFiberListPage {
                    current_generation: snap.current_generation,
                    stale_generation: false,
                    fibers,
                    next_cursor: opt_fixed16(&snap.next_cursor)?,
                    truncated: snap.truncated,
                },
            ))
        })()),
        ControlCommand::ListOperations {
            fiber_id,
            expected_generation,
            ..
        } => Some((|| {
            let snap = decode_durable_operation_list_snapshot(&response.payload)?;
            if snap.stale_generation {
                return Ok(ControlOutcome::StaleGeneration {
                    view: SystemControlView::OperationList,
                    target_id: *fiber_id,
                    expected_generation: *expected_generation,
                    current_generation: snap.current_generation,
                });
            }
            let mut operations = Vec::with_capacity(snap.operations.len());
            for op in snap.operations {
                operations.push(DurableOperationInspection {
                    operation_id: fixed16(&op.operation_id)?,
                    generation: op.generation,
                    state: DurableOperationState::try_from(op.state).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified durable operation state")
                    })?,
                    cancel_epoch: op.cancel_epoch,
                    owner_fiber_id: fixed16(&op.owner_fiber_id)?,
                    owner_fiber_generation: op.owner_fiber_generation,
                    outcome_receipt_id: op.outcome_receipt.map(|r| r.receipt_id),
                });
            }
            Ok(ControlOutcome::OperationsListed(DurableOperationListPage {
                current_generation: snap.current_generation,
                stale_generation: false,
                operations,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::ListInstalledApplications { .. } => Some((|| {
            let snap = decode_installed_application_list_snapshot(&response.payload)?;
            let mut applications = Vec::with_capacity(snap.applications.len());
            for app in snap.applications {
                applications.push(InstalledApplicationSummaryRecord {
                    application_id: fixed16(&app.application_id)?,
                    installation_id: fixed16(&app.installation_id)?,
                    package_id: fixed16(&app.package_id)?,
                    package_manifest_digest: fixed32(&app.package_manifest_digest)?,
                    current_installation_generation: app.current_installation_generation,
                    status: app.status,
                    created_at_ms: app.created_at_ms,
                    updated_at_ms: app.updated_at_ms,
                });
            }
            Ok(ControlOutcome::InstalledApplicationsListed(
                InstalledApplicationListPage {
                    applications,
                    next_cursor: opt_fixed16(&snap.next_cursor)?,
                    truncated: snap.truncated,
                },
            ))
        })()),
        ControlCommand::QuerySearchCandidates { .. } => Some((|| {
            let snap = decode_search_candidate_query_snapshot(&response.payload)?;
            if snap.non_authoritative_restrictions_bits != SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
            {
                return Err(ControlError::UnexpectedResponse(
                    "search candidate snapshot lacked 0x1F non-authoritative restriction mask",
                ));
            }
            let mut candidates = Vec::with_capacity(snap.candidates.len());
            for c in snap.candidates {
                if c.non_authoritative_restrictions_bits
                    != SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
                {
                    return Err(ControlError::UnexpectedResponse(
                        "search candidate entry lacked 0x1F non-authoritative restriction mask",
                    ));
                }
                candidates.push(SearchCandidateRecord {
                    candidate_id: fixed16(&c.candidate_id)?,
                    object_id: fixed16(&c.object_id)?,
                    scope_id: fixed32(&c.scope_id)?,
                    title_snippet: c.title_snippet,
                    rank_score_milli: c.rank_score_milli,
                    non_authoritative_restrictions_bits: c.non_authoritative_restrictions_bits,
                });
            }
            Ok(ControlOutcome::SearchCandidatesQueried(
                SearchCandidateQueryPage {
                    scope_id: fixed32(&snap.scope_id)?,
                    non_authoritative_restrictions_bits: snap.non_authoritative_restrictions_bits,
                    achieved_consistency: snap.achieved_consistency,
                    failed_shards: snap.failed_shards,
                    candidates,
                    next_cursor: opt_fixed16(&snap.next_cursor)?,
                    truncated: snap.truncated,
                },
            ))
        })()),
        ControlCommand::PollDesktopWatermarkEvents { .. } => Some((|| {
            let snap = decode_desktop_watermark_poll_snapshot(&response.payload)?;
            let mut events = Vec::with_capacity(snap.events.len());
            for e in snap.events {
                events.push(DesktopWatermarkEventRecord {
                    watermark: e.watermark,
                    domain_scope: ControlScope::try_from(e.domain_scope).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified watermark domain scope")
                    })?,
                    entity_id: fixed16(&e.entity_id)?,
                    new_generation_or_revision: e.new_generation_or_revision,
                    state_digest: fixed32(&e.state_digest)?,
                    occurred_at_ms: e.occurred_at_ms,
                });
            }
            Ok(ControlOutcome::DesktopWatermarkPolled(
                DesktopWatermarkPollPage {
                    current_watermark: snap.current_watermark,
                    events,
                    truncated: snap.truncated,
                },
            ))
        })()),
        ControlCommand::InspectSettingsDomain { .. } => Some((|| {
            let snap = decode_settings_domain_snapshot(&response.payload)?;
            let domain_kind = SettingsDomainKind::try_from(snap.domain_kind).map_err(|_| {
                ControlError::UnexpectedResponse("unspecified settings domain kind")
            })?;
            Ok(ControlOutcome::SettingsDomainInspected(
                SettingsDomainProjection {
                    domain_kind,
                    scope_target_id: opt_fixed16(&snap.scope_target_id)?,
                    domain_revision: snap.domain_revision,
                    entries: snap
                        .entries
                        .into_iter()
                        .map(|entry| SettingsEntryRecord {
                            key: entry.key,
                            value_cbor: entry.value_cbor,
                            revision: entry.revision,
                        })
                        .collect(),
                },
            ))
        })()),
        ControlCommand::ListContentTypeAssociations { .. } => Some((|| {
            let snap = decode_content_type_association_list_snapshot(&response.payload)?;
            let mut associations = Vec::with_capacity(snap.associations.len());
            for assoc in snap.associations {
                associations.push(ContentTypeAssociationRecord {
                    association_id: fixed16(&assoc.association_id)?,
                    content_type: assoc.content_type,
                    application_id: fixed16(&assoc.application_id)?,
                    expected_generation_or_revision: assoc.expected_generation_or_revision,
                });
            }
            Ok(ControlOutcome::ContentTypeAssociationsListed(
                ContentTypeAssociationListPage {
                    associations,
                    next_cursor: opt_fixed16(&snap.next_cursor)?,
                    truncated: snap.truncated,
                },
            ))
        })()),
        ControlCommand::ListKeychainReferences { .. } => Some((|| {
            let snap = decode_keychain_reference_list_snapshot(&response.payload)?;
            let mut references = Vec::with_capacity(snap.references.len());
            for r in snap.references {
                references.push(KeychainReferenceRecord {
                    keychain_ref_id: fixed16(&r.keychain_ref_id)?,
                    owner_principal_id: fixed16(&r.owner_principal_id)?,
                    label: r.label,
                    public_fingerprint: fixed32(&r.public_fingerprint)?,
                    expected_generation_or_revision: r.expected_generation_or_revision,
                    hardware_backed: r.hardware_backed,
                });
            }
            Ok(ControlOutcome::KeychainReferencesListed(
                KeychainReferenceListPage {
                    references,
                    next_cursor: opt_fixed16(&snap.next_cursor)?,
                    truncated: snap.truncated,
                },
            ))
        })()),
        ControlCommand::ListChooserGrants { .. } => Some((|| {
            let snap = decode_chooser_grant_list_snapshot(&response.payload)?;
            let mut grants = Vec::with_capacity(snap.grants.len());
            for g in snap.grants {
                grants.push(ChooserGrantRecord {
                    grant_id: fixed16(&g.grant_id)?,
                    application_id: fixed16(&g.application_id)?,
                    chooser_kind: g.chooser_kind,
                    resource_handle: fixed16(&g.resource_handle)?,
                    expected_generation_or_revision: g.expected_generation_or_revision,
                    expires_at_ms: g.expires_at_ms,
                });
            }
            Ok(ControlOutcome::ChooserGrantsListed(ChooserGrantListPage {
                grants,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::ListWorkspaces { .. } => Some((|| {
            let snap = decode_workspace_list_snapshot(&response.payload)?;
            let mut workspaces = Vec::with_capacity(snap.workspaces.len());
            for w in snap.workspaces {
                workspaces.push(WorkspaceRecord {
                    workspace_id: fixed16(&w.workspace_id)?,
                    name: w.name,
                    layout_preset: w.layout_preset,
                    active_surface_count: w.active_surface_count,
                    focused_task_id: opt_fixed16(&w.focused_task_id)?,
                    expected_generation_or_revision: w.expected_generation_or_revision,
                    updated_at_ms: w.updated_at_ms,
                });
            }
            Ok(ControlOutcome::WorkspacesListed(WorkspaceListPage {
                workspaces,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::InspectWorkspace {
            workspace_id,
            expected_revision,
        } => Some((|| {
            let snap = decode_workspace_snapshot(&response.payload)?;
            if snap.stale_generation {
                return Ok(ControlOutcome::StaleGeneration {
                    view: SystemControlView::Workspace,
                    target_id: Some(*workspace_id),
                    expected_generation: *expected_revision,
                    current_generation: snap.current_generation,
                });
            }
            let w = snap.workspace.ok_or(ControlError::UnexpectedResponse(
                "workspace snapshot lacked workspace status",
            ))?;
            Ok(ControlOutcome::WorkspaceInspected(WorkspaceRecord {
                workspace_id: fixed16(&w.workspace_id)?,
                name: w.name,
                layout_preset: w.layout_preset,
                active_surface_count: w.active_surface_count,
                focused_task_id: opt_fixed16(&w.focused_task_id)?,
                expected_generation_or_revision: w.expected_generation_or_revision,
                updated_at_ms: w.updated_at_ms,
            }))
        })()),
        ControlCommand::ListProcesses { .. } => Some((|| {
            let snap = decode_process_list_snapshot(&response.payload)?;
            let mut processes = Vec::with_capacity(snap.processes.len());
            for p in snap.processes {
                processes.push(ProcessSummaryRecord {
                    process_id: fixed16(&p.process_id)?,
                    expected_generation_or_revision: p.expected_generation_or_revision,
                    agent_instance_id: fixed16(&p.agent_instance_id)?,
                    task_id: fixed16(&p.task_id)?,
                    task_attempt_id: fixed16(&p.task_attempt_id)?,
                    isolation_domain_id: fixed16(&p.isolation_domain_id)?,
                    cell_id: opt_fixed16(&p.cell_id)?,
                    lifecycle_state: p.lifecycle_state,
                });
            }
            Ok(ControlOutcome::ProcessesListed(ProcessListPage {
                processes,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::ListResourceReservations { .. } => Some((|| {
            let snap = decode_resource_reservation_list_snapshot(&response.payload)?;
            let mut reservations = Vec::with_capacity(snap.reservations.len());
            for r in snap.reservations {
                reservations.push(ResourceReservationSummaryRecord {
                    reservation_id: fixed16(&r.reservation_id)?,
                    account_id: fixed16(&r.account_id)?,
                    actual_usage: r.actual_usage,
                    effective_granted: r.effective_granted,
                    requested_ceiling: r.requested_ceiling,
                    context_residency_tier: ContextResidencyTier::try_from(
                        r.context_residency_tier,
                    )
                    .map_err(|_| ControlError::UnexpectedResponse("unspecified residency tier"))?,
                    expected_generation_or_revision: r.expected_generation_or_revision,
                });
            }
            Ok(ControlOutcome::ResourceReservationsListed(
                ResourceReservationListPage {
                    reservations,
                    next_cursor: opt_fixed16(&snap.next_cursor)?,
                    truncated: snap.truncated,
                },
            ))
        })()),
        ControlCommand::ListTopics { .. } => Some((|| {
            let snap = decode_topic_list_snapshot(&response.payload)?;
            let mut topics = Vec::with_capacity(snap.topics.len());
            for t in snap.topics {
                let inner = t.topic.ok_or(ControlError::UnexpectedResponse(
                    "topic summary lacked inner topic status",
                ))?;
                topics.push(TopicSummaryRecord {
                    topic: TopicInspection {
                        topic_id: fixed16(&inner.topic_id)?,
                        channel_id: fixed16(&inner.channel_id)?,
                        channel_generation: inner.channel_generation,
                        name: inner.name,
                        active_subscriptions: inner.active_subscriptions,
                        policy_digest: inner.policy_digest,
                        created_at_ms: inner.created_at_ms,
                    },
                    expected_generation_or_revision: t.expected_generation_or_revision,
                    retained_messages: t.retained_messages,
                });
            }
            Ok(ControlOutcome::TopicsListed(TopicListPage {
                topics,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::ListNotifications { .. } => Some((|| {
            let snap = decode_notification_list_snapshot(&response.payload)?;
            let mut notifications = Vec::with_capacity(snap.notifications.len());
            for n in snap.notifications {
                notifications.push(NotificationSummaryRecord {
                    notification: NotificationInspection {
                        notification_id: fixed16(&n.notification_id)?,
                        topic_id: fixed16(&n.topic_id)?,
                        subscriber_key: fixed16(&n.subscriber_key)?,
                        next_sequence: n.next_sequence,
                        history_entry_count: n.history_entry_count,
                        history_truncated: n.history_truncated,
                        prune_watermark_sequence: n.prune_watermark_sequence,
                    },
                    expected_generation_or_revision: n.expected_generation_or_revision,
                });
            }
            Ok(ControlOutcome::NotificationsListed(NotificationListPage {
                notifications,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::ListCells { .. } => Some((|| {
            let snap = decode_cell_list_snapshot(&response.payload)?;
            let mut cells = Vec::with_capacity(snap.cells.len());
            for c in snap.cells {
                cells.push(CellSummaryRecord {
                    cell_id: fixed16(&c.cell_id)?,
                    cell_label: c.cell_label,
                    reachability: CellReachabilityState::try_from(c.reachability).map_err(
                        |_| ControlError::UnexpectedResponse("unspecified cell reachability"),
                    )?,
                    partition_autonomy_gate: PartitionAutonomyGateState::try_from(
                        c.partition_autonomy_gate,
                    )
                    .map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified cell autonomy gate")
                    })?,
                    current_epoch: c.current_epoch,
                    active_lease_count: c.active_lease_count,
                    expected_generation_or_revision: c.expected_generation_or_revision,
                });
            }
            Ok(ControlOutcome::CellsListed(CellListPage {
                cells,
                next_cursor: opt_fixed16(&snap.next_cursor)?,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::InspectCellLeases { .. } => Some((|| {
            let snap = decode_cell_lease_inspect_snapshot(&response.payload)?;
            let mut leases = Vec::with_capacity(snap.leases.len());
            for l in snap.leases {
                leases.push(CellLeaseRecord {
                    lease_id: fixed16(&l.lease_id)?,
                    cell_id: fixed16(&l.cell_id)?,
                    kind: CellLeaseKind::try_from(l.kind).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified cell lease kind")
                    })?,
                    lock_state: CellLeaseLockState::try_from(l.lock_state).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified cell lease lock state")
                    })?,
                    holder_id: fixed16(&l.holder_id)?,
                    remaining_holder_ttl_ms: l.remaining_holder_ttl_ms,
                    lease_epoch: l.lease_epoch,
                    clock_skew_horizon_ms: l.clock_skew_horizon_ms,
                });
            }
            Ok(ControlOutcome::CellLeasesInspected(CellLeaseInspectPage {
                cell_id: fixed16(&snap.cell_id)?,
                leases,
                truncated: snap.truncated,
            }))
        })()),
        ControlCommand::PreviewControlBlastRadius { .. } => Some((|| {
            let snap = decode_control_blast_radius_preview_snapshot(&response.payload)?;
            let mut impacts = Vec::with_capacity(snap.impacts.len());
            for item in snap.impacts {
                impacts.push(ControlBlastRadiusImpactRecord {
                    sign: BlastRadiusImpactSign::try_from(item.sign).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified blast radius impact sign")
                    })?,
                    affected_scope: ControlScope::try_from(item.affected_scope).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified blast radius scope")
                    })?,
                    affected_id: fixed16(&item.affected_id)?,
                    current_generation_or_revision: item.current_generation_or_revision,
                    summary_code: item.summary_code,
                });
            }
            Ok(ControlOutcome::BlastRadiusPreviewed(
                ControlBlastRadiusPreviewResult {
                    target_scope: ControlScope::try_from(snap.target_scope).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified blast radius target scope")
                    })?,
                    target_id: fixed16(&snap.target_id)?,
                    preview_action: snap.preview_action,
                    statutory_lock_blocked: snap.statutory_lock_blocked,
                    statutory_lock_code: snap.statutory_lock_code,
                    impacts,
                    truncated: snap.truncated,
                },
            ))
        })()),
        ControlCommand::QueryControlCommandStatus { .. } => Some((|| {
            let snap = decode_control_command_status_snapshot(&response.payload)?;
            let receipt_id = match snap.receipt {
                Some(r) => Some(fixed16(&r.receipt_id)?),
                None => None,
            };
            Ok(ControlOutcome::CommandStatusQueried(
                ControlCommandStatusRecord {
                    control_command_id: fixed16(&snap.control_command_id)?,
                    state: ControlCommandLifecycleState::try_from(snap.state).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified command lifecycle state")
                    })?,
                    scope: ControlScope::try_from(snap.scope).map_err(|_| {
                        ControlError::UnexpectedResponse("unspecified command scope")
                    })?,
                    target_id: fixed16(&snap.target_id)?,
                    receipt_id,
                    failure: snap.failure,
                },
            ))
        })()),
        ControlCommand::ScopedOperation { action, .. } => Some((|| {
            let receipt_id = decoded_result_receipt_pub(command, response)?;
            Ok(match action {
                ScopedControlAction::Pause => ControlOutcome::OperationPaused { receipt_id },
                ScopedControlAction::Resume => ControlOutcome::OperationResumed { receipt_id },
                ScopedControlAction::Cancel => ControlOutcome::OperationCancelled { receipt_id },
                ScopedControlAction::Kill => ControlOutcome::OperationKilled { receipt_id },
                ScopedControlAction::Throttle { .. } => {
                    ControlOutcome::OperationThrottled { receipt_id }
                }
                ScopedControlAction::Reclaim => ControlOutcome::OperationReclaimed { receipt_id },
                ScopedControlAction::DisableApplication => {
                    ControlOutcome::ApplicationDisabled { receipt_id }
                }
                ScopedControlAction::UninstallApplication => {
                    ControlOutcome::ApplicationUninstalled { receipt_id }
                }
            })
        })()),
        ControlCommand::RequestProcessMigration { .. } => Some(
            decoded_result_receipt_pub(command, response)
                .map(|receipt_id| ControlOutcome::ProcessMigrationRequested { receipt_id }),
        ),
        ControlCommand::PurgeTopic { .. } => Some(
            decoded_result_receipt_pub(command, response)
                .map(|receipt_id| ControlOutcome::TopicPurged { receipt_id }),
        ),
        ControlCommand::UpdateSettingsDomain { .. } => Some(
            decoded_result_receipt_pub(command, response)
                .map(|receipt_id| ControlOutcome::SettingsDomainUpdated { receipt_id }),
        ),
        ControlCommand::SetContentTypeAssociation { .. } => Some(
            decoded_result_receipt_pub(command, response)
                .map(|receipt_id| ControlOutcome::ContentTypeAssociationSet { receipt_id }),
        ),
        ControlCommand::IssueChooserGrant { .. } => Some(
            decoded_result_receipt_pub(command, response)
                .map(|receipt_id| ControlOutcome::ChooserGrantIssued { receipt_id }),
        ),
        ControlCommand::UndoReceipt { .. } => Some(
            decoded_result_receipt_pub(command, response)
                .map(|receipt_id| ControlOutcome::ReceiptUndone { receipt_id }),
        ),
        // Statutory lock probes always reject on the server (`context.failure.is_some()`),
        // which `compose_with_services` handles before calling `decode_stage_d_outcome`.
        ControlCommand::MutateSearchCandidate { .. }
        | ControlCommand::MutateTaskNodeTopology { .. }
        | ControlCommand::OverrideTaskAttemptCommit { .. }
        | ControlCommand::OverrideEffectUnknownOperation { .. } => Some(Err(
            ControlError::UnexpectedResponse("statutory-locked command unexpectedly returned Ok"),
        )),
        _ => None,
    }
}

fn push_len_bytes(bytes: &mut Vec<u8>, value: &[u8]) {
    let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(value);
}

fn push_opt_id(bytes: &mut Vec<u8>, id: Option<[u8; 16]>) {
    match id {
        Some(raw) => {
            bytes.push(1);
            bytes.extend_from_slice(&raw);
        }
        None => bytes.push(0),
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn encode_stage_d_outcome_bytes(bytes: &mut Vec<u8>, outcome: &ControlOutcome) -> bool {
    match outcome {
        ControlOutcome::StaleGeneration {
            view,
            target_id,
            expected_generation,
            current_generation,
        } => {
            bytes.push(25);
            bytes.extend_from_slice(&i32::from(*view).to_le_bytes());
            push_opt_id(bytes, *target_id);
            bytes.extend_from_slice(&expected_generation.to_le_bytes());
            bytes.extend_from_slice(&current_generation.to_le_bytes());
        }
        ControlOutcome::TasksListed(page) => {
            bytes.push(26);
            bytes.extend_from_slice(
                &u32::try_from(page.tasks.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for t in &page.tasks {
                bytes.extend_from_slice(&t.task_id);
                bytes.extend_from_slice(&t.plan_id);
                push_opt_id(bytes, t.application_id);
                bytes.extend_from_slice(&t.installation_generation.to_le_bytes());
                bytes.extend_from_slice(&t.plan_revision.to_le_bytes());
                bytes.extend_from_slice(&i32::from(t.task_state).to_le_bytes());
                bytes.extend_from_slice(&i32::from(t.undo_class).to_le_bytes());
                push_opt_id(bytes, t.reservation_id);
                push_opt_id(bytes, t.cell_id);
                bytes.extend_from_slice(&t.updated_at_ms.to_le_bytes());
                bytes.extend_from_slice(&t.expected_generation_or_revision.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::TaskGroupsListed(page) => {
            bytes.push(27);
            bytes.extend_from_slice(
                &u32::try_from(page.groups.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for g in &page.groups {
                bytes.extend_from_slice(&g.group_id);
                bytes.extend_from_slice(&g.task_id);
                push_opt_id(bytes, g.parent_group_id);
                bytes.extend_from_slice(&i32::from(g.state).to_le_bytes());
                bytes.extend_from_slice(&g.membership_generation.to_le_bytes());
                bytes.extend_from_slice(&g.state_seq.to_le_bytes());
                bytes.extend_from_slice(&g.depth.to_le_bytes());
                bytes.extend_from_slice(&g.cancel_epoch.to_le_bytes());
                bytes.extend_from_slice(&g.created_at_ms.to_le_bytes());
                bytes.extend_from_slice(&g.updated_at_ms.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::TaskNodesListed(page) => {
            bytes.push(28);
            bytes.extend_from_slice(&page.plan_id);
            bytes.extend_from_slice(&page.current_plan_revision.to_le_bytes());
            bytes.extend_from_slice(
                &u32::try_from(page.nodes.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for n in &page.nodes {
                bytes.extend_from_slice(&n.plan_id);
                bytes.extend_from_slice(&n.node_id);
                bytes.extend_from_slice(&i32::from(n.kind).to_le_bytes());
                bytes.extend_from_slice(&i32::from(n.state).to_le_bytes());
                bytes.extend_from_slice(&n.declared_revision.to_le_bytes());
                push_len_bytes(bytes, &n.node_digest);
                bytes.extend_from_slice(&n.transition_count.to_le_bytes());
                bytes.extend_from_slice(&i32::from(n.residency_tier).to_le_bytes());
                bytes.extend_from_slice(&n.residency_transition_count.to_le_bytes());
                bytes.extend_from_slice(&n.first_declared_at_ms.to_le_bytes());
                bytes.extend_from_slice(&n.updated_at_ms.to_le_bytes());
            }
            bytes.extend_from_slice(&page.metadata_only_collapsed_count.to_le_bytes());
            bytes.extend_from_slice(&page.cold_collapsed_count.to_le_bytes());
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::TaskAttemptsListed(page) => {
            bytes.push(29);
            bytes.extend_from_slice(&page.current_generation.to_le_bytes());
            bytes.extend_from_slice(
                &u32::try_from(page.attempts.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for a in &page.attempts {
                bytes.extend_from_slice(&a.attempt_id);
                bytes.extend_from_slice(&a.task_id);
                push_opt_id(bytes, a.node_id);
                bytes.extend_from_slice(&i32::from(a.state).to_le_bytes());
                bytes.extend_from_slice(&a.isolation_domain_id);
                bytes.push(u8::from(a.provider_isolation_unverified));
                bytes.push(u8::from(a.cas_winner));
                bytes.extend_from_slice(&a.expected_generation_or_revision.to_le_bytes());
                bytes.extend_from_slice(&a.started_at_ms.to_le_bytes());
                bytes.extend_from_slice(&a.updated_at_ms.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::ExecutionFibersListed(page) => {
            bytes.push(30);
            bytes.extend_from_slice(&page.current_generation.to_le_bytes());
            bytes.extend_from_slice(
                &u32::try_from(page.fibers.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for entry in &page.fibers {
                bytes.extend_from_slice(&entry.fiber.fiber_id);
                bytes.extend_from_slice(&entry.fiber.generation.to_le_bytes());
                bytes.extend_from_slice(&i32::from(entry.fiber.state).to_le_bytes());
                bytes.extend_from_slice(&i32::from(entry.fiber.lifecycle_phase).to_le_bytes());
                bytes.extend_from_slice(&entry.fiber.active_cpu_ms.to_le_bytes());
                bytes.extend_from_slice(&entry.fiber.elapsed_wall_ms.to_le_bytes());
                bytes.extend_from_slice(&entry.fiber.scheduler_wait_ms.to_le_bytes());
                bytes.extend_from_slice(&entry.fiber.external_wait_ms.to_le_bytes());
                bytes.extend_from_slice(&entry.fiber.backpressure_wait_ms.to_le_bytes());
                bytes.extend_from_slice(&entry.fiber.suspended_ms.to_le_bytes());
                push_opt_id(bytes, entry.process_id);
                bytes.push(u8::from(entry.shared_thread_like));
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::OperationsListed(page) => {
            bytes.push(31);
            bytes.extend_from_slice(&page.current_generation.to_le_bytes());
            bytes.extend_from_slice(
                &u32::try_from(page.operations.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for op in &page.operations {
                bytes.extend_from_slice(&op.operation_id);
                bytes.extend_from_slice(&op.generation.to_le_bytes());
                bytes.extend_from_slice(&i32::from(op.state).to_le_bytes());
                bytes.extend_from_slice(&op.cancel_epoch.to_le_bytes());
                bytes.extend_from_slice(&op.owner_fiber_id);
                bytes.extend_from_slice(&op.owner_fiber_generation.to_le_bytes());
                match op.outcome_receipt_id.as_ref() {
                    Some(rcpt) => {
                        bytes.push(1);
                        push_len_bytes(bytes, rcpt);
                    }
                    None => bytes.push(0),
                }
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::InstalledApplicationsListed(page) => {
            bytes.push(32);
            bytes.extend_from_slice(
                &u32::try_from(page.applications.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for app in &page.applications {
                bytes.extend_from_slice(&app.application_id);
                bytes.extend_from_slice(&app.installation_id);
                bytes.extend_from_slice(&app.package_id);
                bytes.extend_from_slice(&app.package_manifest_digest);
                bytes.extend_from_slice(&app.current_installation_generation.to_le_bytes());
                bytes.extend_from_slice(&app.status.to_le_bytes());
                bytes.extend_from_slice(&app.created_at_ms.to_le_bytes());
                bytes.extend_from_slice(&app.updated_at_ms.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::SearchCandidatesQueried(page) => {
            bytes.push(33);
            bytes.extend_from_slice(&page.scope_id);
            bytes.extend_from_slice(&page.non_authoritative_restrictions_bits.to_le_bytes());
            bytes.extend_from_slice(&page.achieved_consistency.to_le_bytes());
            bytes.extend_from_slice(
                &u32::try_from(page.failed_shards.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for shard in &page.failed_shards {
                bytes.extend_from_slice(&shard.to_le_bytes());
            }
            bytes.extend_from_slice(
                &u32::try_from(page.candidates.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for c in &page.candidates {
                bytes.extend_from_slice(&c.candidate_id);
                bytes.extend_from_slice(&c.object_id);
                bytes.extend_from_slice(&c.scope_id);
                push_len_bytes(bytes, c.title_snippet.as_bytes());
                bytes.extend_from_slice(&c.rank_score_milli.to_le_bytes());
                bytes.extend_from_slice(&c.non_authoritative_restrictions_bits.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::DesktopWatermarkPolled(page) => {
            bytes.push(34);
            bytes.extend_from_slice(&page.current_watermark.to_le_bytes());
            bytes.extend_from_slice(
                &u32::try_from(page.events.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for e in &page.events {
                bytes.extend_from_slice(&e.watermark.to_le_bytes());
                bytes.extend_from_slice(&i32::from(e.domain_scope).to_le_bytes());
                bytes.extend_from_slice(&e.entity_id);
                bytes.extend_from_slice(&e.new_generation_or_revision.to_le_bytes());
                bytes.extend_from_slice(&e.state_digest);
                bytes.extend_from_slice(&e.occurred_at_ms.to_le_bytes());
            }
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::SettingsDomainInspected(proj) => {
            bytes.push(35);
            bytes.extend_from_slice(&i32::from(proj.domain_kind).to_le_bytes());
            push_opt_id(bytes, proj.scope_target_id);
            bytes.extend_from_slice(&proj.domain_revision.to_le_bytes());
            bytes.extend_from_slice(
                &u32::try_from(proj.entries.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for entry in &proj.entries {
                push_len_bytes(bytes, entry.key.as_bytes());
                push_len_bytes(bytes, &entry.value_cbor);
                bytes.extend_from_slice(&entry.revision.to_le_bytes());
            }
        }
        ControlOutcome::ContentTypeAssociationsListed(page) => {
            bytes.push(36);
            bytes.extend_from_slice(
                &u32::try_from(page.associations.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for assoc in &page.associations {
                bytes.extend_from_slice(&assoc.association_id);
                push_len_bytes(bytes, assoc.content_type.as_bytes());
                bytes.extend_from_slice(&assoc.application_id);
                bytes.extend_from_slice(&assoc.expected_generation_or_revision.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::KeychainReferencesListed(page) => {
            bytes.push(37);
            bytes.extend_from_slice(
                &u32::try_from(page.references.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for r in &page.references {
                bytes.extend_from_slice(&r.keychain_ref_id);
                bytes.extend_from_slice(&r.owner_principal_id);
                push_len_bytes(bytes, r.label.as_bytes());
                bytes.extend_from_slice(&r.public_fingerprint);
                bytes.extend_from_slice(&r.expected_generation_or_revision.to_le_bytes());
                bytes.push(u8::from(r.hardware_backed));
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::ChooserGrantsListed(page) => {
            bytes.push(38);
            bytes.extend_from_slice(
                &u32::try_from(page.grants.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for g in &page.grants {
                bytes.extend_from_slice(&g.grant_id);
                bytes.extend_from_slice(&g.application_id);
                bytes.extend_from_slice(&g.chooser_kind.to_le_bytes());
                bytes.extend_from_slice(&g.resource_handle);
                bytes.extend_from_slice(&g.expected_generation_or_revision.to_le_bytes());
                bytes.extend_from_slice(&g.expires_at_ms.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::WorkspacesListed(page) => {
            bytes.push(39);
            bytes.extend_from_slice(
                &u32::try_from(page.workspaces.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for w in &page.workspaces {
                bytes.extend_from_slice(&w.workspace_id);
                push_len_bytes(bytes, w.name.as_bytes());
                bytes.extend_from_slice(&w.layout_preset.to_le_bytes());
                bytes.extend_from_slice(&w.active_surface_count.to_le_bytes());
                push_opt_id(bytes, w.focused_task_id);
                bytes.extend_from_slice(&w.expected_generation_or_revision.to_le_bytes());
                bytes.extend_from_slice(&w.updated_at_ms.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::WorkspaceInspected(w) => {
            bytes.push(40);
            bytes.extend_from_slice(&w.workspace_id);
            push_len_bytes(bytes, w.name.as_bytes());
            bytes.extend_from_slice(&w.layout_preset.to_le_bytes());
            bytes.extend_from_slice(&w.active_surface_count.to_le_bytes());
            push_opt_id(bytes, w.focused_task_id);
            bytes.extend_from_slice(&w.expected_generation_or_revision.to_le_bytes());
            bytes.extend_from_slice(&w.updated_at_ms.to_le_bytes());
        }
        ControlOutcome::ProcessesListed(page) => {
            bytes.push(41);
            bytes.extend_from_slice(
                &u32::try_from(page.processes.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for p in &page.processes {
                bytes.extend_from_slice(&p.process_id);
                bytes.extend_from_slice(&p.expected_generation_or_revision.to_le_bytes());
                bytes.extend_from_slice(&p.agent_instance_id);
                bytes.extend_from_slice(&p.task_id);
                bytes.extend_from_slice(&p.task_attempt_id);
                bytes.extend_from_slice(&p.isolation_domain_id);
                push_opt_id(bytes, p.cell_id);
                bytes.extend_from_slice(&p.lifecycle_state.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::ResourceReservationsListed(page) => {
            bytes.push(42);
            bytes.extend_from_slice(
                &u32::try_from(page.reservations.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for r in &page.reservations {
                bytes.extend_from_slice(&r.reservation_id);
                bytes.extend_from_slice(&r.account_id);
                bytes.extend_from_slice(&r.actual_usage.to_le_bytes());
                bytes.extend_from_slice(&r.effective_granted.to_le_bytes());
                bytes.extend_from_slice(&r.requested_ceiling.to_le_bytes());
                bytes.extend_from_slice(&i32::from(r.context_residency_tier).to_le_bytes());
                bytes.extend_from_slice(&r.expected_generation_or_revision.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::TopicsListed(page) => {
            bytes.push(43);
            bytes.extend_from_slice(
                &u32::try_from(page.topics.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for t in &page.topics {
                bytes.extend_from_slice(&t.topic.topic_id);
                bytes.extend_from_slice(&t.topic.channel_id);
                bytes.extend_from_slice(&t.topic.channel_generation.to_le_bytes());
                push_len_bytes(bytes, &t.topic.name);
                bytes.extend_from_slice(&t.topic.active_subscriptions.to_le_bytes());
                push_len_bytes(bytes, &t.topic.policy_digest);
                bytes.extend_from_slice(&t.topic.created_at_ms.to_le_bytes());
                bytes.extend_from_slice(&t.expected_generation_or_revision.to_le_bytes());
                bytes.extend_from_slice(&t.retained_messages.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::NotificationsListed(page) => {
            bytes.push(44);
            bytes.extend_from_slice(
                &u32::try_from(page.notifications.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for n in &page.notifications {
                bytes.extend_from_slice(&n.notification.notification_id);
                bytes.extend_from_slice(&n.notification.topic_id);
                bytes.extend_from_slice(&n.notification.subscriber_key);
                bytes.extend_from_slice(&n.notification.next_sequence.to_le_bytes());
                bytes.extend_from_slice(&n.notification.history_entry_count.to_le_bytes());
                bytes.push(u8::from(n.notification.history_truncated));
                bytes.extend_from_slice(&n.notification.prune_watermark_sequence.to_le_bytes());
                bytes.extend_from_slice(&n.expected_generation_or_revision.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::CellsListed(page) => {
            bytes.push(45);
            bytes.extend_from_slice(
                &u32::try_from(page.cells.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for c in &page.cells {
                bytes.extend_from_slice(&c.cell_id);
                push_len_bytes(bytes, c.cell_label.as_bytes());
                bytes.extend_from_slice(&i32::from(c.reachability).to_le_bytes());
                bytes.extend_from_slice(&i32::from(c.partition_autonomy_gate).to_le_bytes());
                bytes.extend_from_slice(&c.current_epoch.to_le_bytes());
                bytes.extend_from_slice(&c.active_lease_count.to_le_bytes());
                bytes.extend_from_slice(&c.expected_generation_or_revision.to_le_bytes());
            }
            push_opt_id(bytes, page.next_cursor);
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::CellLeasesInspected(page) => {
            bytes.push(46);
            bytes.extend_from_slice(&page.cell_id);
            bytes.extend_from_slice(
                &u32::try_from(page.leases.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for l in &page.leases {
                bytes.extend_from_slice(&l.lease_id);
                bytes.extend_from_slice(&l.cell_id);
                bytes.extend_from_slice(&i32::from(l.kind).to_le_bytes());
                bytes.extend_from_slice(&i32::from(l.lock_state).to_le_bytes());
                bytes.extend_from_slice(&l.holder_id);
                bytes.extend_from_slice(&l.remaining_holder_ttl_ms.to_le_bytes());
                bytes.extend_from_slice(&l.lease_epoch.to_le_bytes());
                bytes.extend_from_slice(&l.clock_skew_horizon_ms.to_le_bytes());
            }
            bytes.push(u8::from(page.truncated));
        }
        ControlOutcome::BlastRadiusPreviewed(preview) => {
            bytes.push(47);
            bytes.extend_from_slice(&i32::from(preview.target_scope).to_le_bytes());
            bytes.extend_from_slice(&preview.target_id);
            bytes.extend_from_slice(&preview.preview_action.to_le_bytes());
            bytes.push(u8::from(preview.statutory_lock_blocked));
            push_len_bytes(bytes, preview.statutory_lock_code.as_bytes());
            bytes.extend_from_slice(
                &u32::try_from(preview.impacts.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for item in &preview.impacts {
                bytes.extend_from_slice(&i32::from(item.sign).to_le_bytes());
                bytes.extend_from_slice(&i32::from(item.affected_scope).to_le_bytes());
                bytes.extend_from_slice(&item.affected_id);
                bytes.extend_from_slice(&item.current_generation_or_revision.to_le_bytes());
                push_len_bytes(bytes, item.summary_code.as_bytes());
            }
            bytes.push(u8::from(preview.truncated));
        }
        ControlOutcome::CommandStatusQueried(status) => {
            bytes.push(48);
            bytes.extend_from_slice(&status.control_command_id);
            bytes.extend_from_slice(&i32::from(status.state).to_le_bytes());
            bytes.extend_from_slice(&i32::from(status.scope).to_le_bytes());
            bytes.extend_from_slice(&status.target_id);
            push_opt_id(bytes, status.receipt_id);
            match status.failure.as_ref() {
                Some(failure) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&failure.code.to_le_bytes());
                    bytes.extend_from_slice(&failure.retry.to_le_bytes());
                    push_len_bytes(bytes, failure.safe_message.as_bytes());
                }
                None => bytes.push(0),
            }
        }
        ControlOutcome::ProcessMigrationRequested { receipt_id } => {
            bytes.push(49);
            push_len_bytes(bytes, receipt_id);
        }
        ControlOutcome::TopicPurged { receipt_id } => {
            bytes.push(50);
            push_len_bytes(bytes, receipt_id);
        }
        ControlOutcome::SettingsDomainUpdated { receipt_id } => {
            bytes.push(51);
            push_len_bytes(bytes, receipt_id);
        }
        ControlOutcome::ContentTypeAssociationSet { receipt_id } => {
            bytes.push(52);
            push_len_bytes(bytes, receipt_id);
        }
        ControlOutcome::ChooserGrantIssued { receipt_id } => {
            bytes.push(53);
            push_len_bytes(bytes, receipt_id);
        }
        ControlOutcome::ReceiptUndone { receipt_id } => {
            bytes.push(54);
            push_len_bytes(bytes, receipt_id);
        }
        _ => return false,
    }
    true
}
