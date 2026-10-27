//! Stage D (`IPC-D1..D8` & `[UI-PARITY-001]`) SABI `SystemControl` handler
//! projections and statutory non-tamperable lock guards.

use nlos_schema::sabi;
use nlos_schema::sabi::v1::{
    ControlCommandLifecycleState, ControlScope, Envelope, PlanNodeLifecycleState, ReceiptReference,
    RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext, SettingsDomainKind,
    SystemControlView, TaskUndoClass,
};
use nlos_schema::{
    CompatibilityError, MAX_DESKTOP_WATERMARK_EVENT_LIMIT, MAX_SYSTEM_CONTROL_ENUM_LIMIT,
    REQUEST_ID_BYTES, SHA256_DIGEST_BYTES, system_control_schema_identity,
    unpack_blast_radius_preview_generation,
};
use nlos_types::ReceiptId;

use crate::cell_inspector::ProcessMigrationControlRequest;
use crate::desktop_enum::{ControlCommandStatusRecord, DesktopMutationRequest};
use crate::{
    RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer, SystemControlError,
    fixed16, response_envelope,
};

fn optional_fixed16(bytes: &[u8]) -> Result<Option<[u8; 16]>, SystemControlError> {
    if bytes.is_empty() {
        Ok(None)
    } else {
        fixed16(bytes).map(Some)
    }
}

fn fixed32(bytes: &[u8]) -> Result<[u8; SHA256_DIGEST_BYTES], SystemControlError> {
    bytes
        .try_into()
        .map_err(|_| SystemControlError::Schema(CompatibilityError::InvalidSystemControlIdentifier))
}

fn statutory_lock_failure(code: SabiErrorCode, message: &'static str) -> SystemControlError {
    SystemControlError::OperationExecution(SabiFailure {
        code: code.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    })
}

impl<H, A> RecoverySystemControl<'_, H, A>
where
    H: RecoveryHealthSource,
    A: SystemControlAuthorizer,
{
    /// Records the terminal outcome of a validated `ControlCommand` in the
    /// optional [`crate::desktop_enum::CommandStatusLedger`] (`[CTRL-RECOVERY-001]`).
    pub(crate) fn record_command_outcome(
        &self,
        command: &sabi::v1::ControlCommand,
        outcome: &Result<ReceiptId, SystemControlError>,
    ) {
        let Some(ledger) = self.command_ledger else {
            return;
        };
        let Ok(control_command_id) = fixed16(&command.control_command_id) else {
            return;
        };
        let Ok(target_id) = fixed16(&command.target_id) else {
            return;
        };
        let scope = ControlScope::try_from(command.scope).unwrap_or(ControlScope::Operation);
        match outcome {
            Ok(receipt_id) => ledger.record(ControlCommandStatusRecord {
                control_command_id,
                state: ControlCommandLifecycleState::Completed,
                scope,
                target_id,
                receipt_id: Some(receipt_id.into_bytes()),
                failure: None,
            }),
            Err(error) => ledger.record(ControlCommandStatusRecord {
                control_command_id,
                state: ControlCommandLifecycleState::Rejected,
                scope,
                target_id,
                receipt_id: None,
                failure: Some(error.to_sabi_failure()),
            }),
        }
    }

    /// Dispatches Stage D (`SystemControlView` `9..=31`) read/enumeration views.
    /// Returns `Ok(None)` when `payload.view` belongs to an earlier recovery or
    /// W32-G view (`1..=8`).
    #[allow(clippy::too_many_lines)] // Flat dispatch over the 23 Stage D views keeps wire mapping auditable.
    pub(crate) fn try_handle_stage_d_get(
        &self,
        request: &Envelope,
        context: &SabiRequestContext,
        payload: &sabi::v1::GetSystemControlRequest,
    ) -> Result<Option<Envelope>, SystemControlError> {
        let Ok(view) = SystemControlView::try_from(payload.view) else {
            return Ok(None);
        };
        let encoded_payload = match view {
            SystemControlView::TaskList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_tasks(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::TaskListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    tasks: page
                        .tasks
                        .into_iter()
                        .map(|t| sabi::v1::TaskSummaryProjection {
                            task_id: t.task_id.to_vec(),
                            plan_id: t.plan_id.to_vec(),
                            application_id: t
                                .application_id
                                .map_or_else(Vec::new, |id| id.to_vec()),
                            installation_generation: t.installation_generation,
                            plan_revision: t.plan_revision,
                            task_state: t.task_state.into(),
                            undo_class: t.undo_class.into(),
                            reservation_id: t
                                .reservation_id
                                .map_or_else(Vec::new, |id| id.to_vec()),
                            cell_id: t.cell_id.map_or_else(Vec::new, |id| id.to_vec()),
                            updated_at_ms: t.updated_at_ms,
                            expected_generation_or_revision: t.expected_generation_or_revision,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_task_list_snapshot(&snapshot)?
            }
            SystemControlView::TaskGroupList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let task_filter = optional_fixed16(&payload.plan_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_task_groups(task_filter, cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::TaskGroupListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    groups: page
                        .groups
                        .into_iter()
                        .map(|g| sabi::v1::TaskGroupStatus {
                            group_id: g.group_id.to_vec(),
                            task_id: g.task_id.to_vec(),
                            parent_group_id: g
                                .parent_group_id
                                .map_or_else(Vec::new, |id| id.to_vec()),
                            state: g.state.into(),
                            membership_generation: g.membership_generation,
                            state_seq: g.state_seq,
                            depth: u64::from(g.depth),
                            cancel_epoch: g.cancel_epoch,
                            created_at_ms: i64::try_from(g.created_at_ms).unwrap_or(i64::MAX),
                            updated_at_ms: i64::try_from(g.updated_at_ms).unwrap_or(i64::MAX),
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_task_group_list_snapshot(&snapshot)?
            }
            SystemControlView::TaskNodeList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let plan_id = fixed16(&payload.plan_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_task_nodes(plan_id, payload.target_generation, cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::TaskNodeListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    plan_id: page.plan_id.to_vec(),
                    current_plan_revision: page.current_plan_revision,
                    stale_generation: page.stale_generation,
                    nodes: page
                        .nodes
                        .into_iter()
                        .map(|n| sabi::v1::TaskNodeStatus {
                            plan_id: n.plan_id.to_vec(),
                            node_id: n.node_id.to_vec(),
                            kind: n.kind.into(),
                            state: n.state.into(),
                            declared_revision: n.declared_revision,
                            node_digest: n.node_digest,
                            transition_count: n.transition_count,
                            residency_tier: n.residency_tier.into(),
                            residency_transition_count: n.residency_transition_count,
                            first_declared_at_ms: n.first_declared_at_ms,
                            updated_at_ms: n.updated_at_ms,
                        })
                        .collect(),
                    metadata_only_collapsed_count: page.metadata_only_collapsed_count,
                    cold_collapsed_count: page.cold_collapsed_count,
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_task_node_list_snapshot(&snapshot)?
            }
            SystemControlView::TaskAttemptList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let scope_filter = optional_fixed16(&payload.plan_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_task_attempts(scope_filter, payload.target_generation, cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::TaskAttemptListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    current_generation: page.current_generation,
                    stale_generation: page.stale_generation,
                    attempts: page
                        .attempts
                        .into_iter()
                        .map(|a| sabi::v1::TaskAttemptStatus {
                            attempt_id: a.attempt_id.to_vec(),
                            task_id: a.task_id.to_vec(),
                            node_id: a.node_id.map_or_else(Vec::new, |id| id.to_vec()),
                            state: a.state.into(),
                            isolation_domain_id: a.isolation_domain_id.to_vec(),
                            provider_isolation_unverified: a.provider_isolation_unverified,
                            cas_winner: a.cas_winner,
                            expected_generation_or_revision: a.expected_generation_or_revision,
                            started_at_ms: a.started_at_ms,
                            updated_at_ms: a.updated_at_ms,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_task_attempt_list_snapshot(&snapshot)?
            }
            SystemControlView::ExecutionFiberList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let process_filter = optional_fixed16(&payload.plan_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_execution_fibers(process_filter, payload.target_generation, cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::ExecutionFiberListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    current_generation: page.current_generation,
                    stale_generation: page.stale_generation,
                    fibers: page
                        .fibers
                        .into_iter()
                        .map(|entry| sabi::v1::ExecutionFiberSummaryStatus {
                            fiber: Some(sabi::v1::ExecutionFiberStatus {
                                fiber_id: entry.fiber.fiber_id.to_vec(),
                                generation: entry.fiber.generation,
                                state: entry.fiber.state.into(),
                                lifecycle_phase: entry.fiber.lifecycle_phase.into(),
                                active_cpu_ms: entry.fiber.active_cpu_ms,
                                elapsed_wall_ms: entry.fiber.elapsed_wall_ms,
                                scheduler_wait_ms: entry.fiber.scheduler_wait_ms,
                                external_wait_ms: entry.fiber.external_wait_ms,
                                backpressure_wait_ms: entry.fiber.backpressure_wait_ms,
                                suspended_ms: entry.fiber.suspended_ms,
                            }),
                            process_id: entry.process_id.map_or_else(Vec::new, |id| id.to_vec()),
                            shared_thread_like: entry.shared_thread_like,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_execution_fiber_list_snapshot(&snapshot)?
            }
            SystemControlView::OperationList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let fiber_filter = optional_fixed16(&payload.plan_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_operations(fiber_filter, payload.target_generation, cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::DurableOperationListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    current_generation: page.current_generation,
                    stale_generation: page.stale_generation,
                    operations: page
                        .operations
                        .into_iter()
                        .map(|op| sabi::v1::DurableOperationStatus {
                            operation_id: op.operation_id.to_vec(),
                            generation: op.generation,
                            state: op.state.into(),
                            cancel_epoch: op.cancel_epoch,
                            owner_fiber_id: op.owner_fiber_id.to_vec(),
                            owner_fiber_generation: op.owner_fiber_generation,
                            outcome_receipt: op
                                .outcome_receipt_id
                                .map(|receipt_id| ReceiptReference { receipt_id }),
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_durable_operation_list_snapshot(&snapshot)?
            }
            SystemControlView::InstalledApplicationList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_installed_applications(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::InstalledApplicationListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    applications: page
                        .applications
                        .into_iter()
                        .map(|app| sabi::v1::InstalledApplicationStatus {
                            application_id: app.application_id.to_vec(),
                            installation_id: app.installation_id.to_vec(),
                            package_id: app.package_id.to_vec(),
                            package_manifest_digest: app.package_manifest_digest.to_vec(),
                            current_installation_generation: app.current_installation_generation,
                            status: app.status,
                            created_at_ms: app.created_at_ms,
                            updated_at_ms: app.updated_at_ms,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_installed_application_list_snapshot(&snapshot)?
            }
            SystemControlView::SearchCandidateQuery => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let scope_id = fixed32(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .query_search_candidates(
                        scope_id,
                        &payload.plan_id,
                        payload.target_generation,
                        limit,
                    )
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::SearchCandidateQuerySnapshot {
                    schema: Some(system_control_schema_identity()),
                    scope_id: page.scope_id.to_vec(),
                    non_authoritative_restrictions_bits: page.non_authoritative_restrictions_bits,
                    achieved_consistency: page.achieved_consistency,
                    failed_shards: page.failed_shards,
                    candidates: page
                        .candidates
                        .into_iter()
                        .map(|c| sabi::v1::SearchCandidateEntry {
                            candidate_id: c.candidate_id.to_vec(),
                            object_id: c.object_id.to_vec(),
                            scope_id: c.scope_id.to_vec(),
                            title_snippet: c.title_snippet,
                            rank_score_milli: c.rank_score_milli,
                            non_authoritative_restrictions_bits: c
                                .non_authoritative_restrictions_bits,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_search_candidate_query_snapshot(&snapshot)?
            }
            SystemControlView::DesktopWatermarkPoll => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_DESKTOP_WATERMARK_EVENT_LIMIT)
                    .min(MAX_DESKTOP_WATERMARK_EVENT_LIMIT);
                let page = source
                    .poll_desktop_watermark_events(payload.target_generation, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::DesktopWatermarkPollSnapshot {
                    schema: Some(system_control_schema_identity()),
                    current_watermark: page.current_watermark,
                    events: page
                        .events
                        .into_iter()
                        .map(|e| sabi::v1::DesktopWatermarkEvent {
                            watermark: e.watermark,
                            domain_scope: e.domain_scope.into(),
                            entity_id: e.entity_id.to_vec(),
                            new_generation_or_revision: e.new_generation_or_revision,
                            state_digest: e.state_digest.to_vec(),
                            occurred_at_ms: e.occurred_at_ms,
                        })
                        .collect(),
                    truncated: page.truncated,
                };
                nlos_schema::encode_desktop_watermark_poll_snapshot(&snapshot)?
            }
            SystemControlView::SettingsDomain => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let raw_kind = i32::try_from(payload.target_generation).map_err(|_| {
                    SystemControlError::Schema(CompatibilityError::InvalidSystemControlIdentifier)
                })?;
                let domain_kind = SettingsDomainKind::try_from(raw_kind).map_err(|_| {
                    SystemControlError::Schema(CompatibilityError::InvalidSystemControlIdentifier)
                })?;
                let scope_target_id = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let proj = source
                    .inspect_settings_domain(domain_kind, scope_target_id, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::SettingsDomainSnapshot {
                    schema: Some(system_control_schema_identity()),
                    domain_kind: proj.domain_kind.into(),
                    scope_target_id: proj.scope_target_id.map_or_else(Vec::new, |id| id.to_vec()),
                    domain_revision: proj.domain_revision,
                    entries: proj
                        .entries
                        .into_iter()
                        .map(|entry| sabi::v1::SettingsEntryStatus {
                            key: entry.key,
                            value_cbor: entry.value_cbor,
                            revision: entry.revision,
                        })
                        .collect(),
                };
                nlos_schema::encode_settings_domain_snapshot(&snapshot)?
            }
            SystemControlView::ContentTypeAssociationList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_content_type_associations(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::ContentTypeAssociationListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    associations: page
                        .associations
                        .into_iter()
                        .map(|assoc| sabi::v1::ContentTypeAssociationStatus {
                            association_id: assoc.association_id.to_vec(),
                            content_type: assoc.content_type,
                            application_id: assoc.application_id.to_vec(),
                            expected_generation_or_revision: assoc.expected_generation_or_revision,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_content_type_association_list_snapshot(&snapshot)?
            }
            SystemControlView::KeychainReferenceList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_keychain_references(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::KeychainReferenceListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    references: page
                        .references
                        .into_iter()
                        .map(|r| sabi::v1::KeychainReferenceStatus {
                            keychain_ref_id: r.keychain_ref_id.to_vec(),
                            owner_principal_id: r.owner_principal_id.to_vec(),
                            label: r.label,
                            public_fingerprint: r.public_fingerprint.to_vec(),
                            expected_generation_or_revision: r.expected_generation_or_revision,
                            hardware_backed: r.hardware_backed,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_keychain_reference_list_snapshot(&snapshot)?
            }
            SystemControlView::ChooserGrantList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_chooser_grants(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::ChooserGrantListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    grants: page
                        .grants
                        .into_iter()
                        .map(|g| sabi::v1::ChooserGrantStatus {
                            grant_id: g.grant_id.to_vec(),
                            application_id: g.application_id.to_vec(),
                            chooser_kind: g.chooser_kind,
                            resource_handle: g.resource_handle.to_vec(),
                            expected_generation_or_revision: g.expected_generation_or_revision,
                            expires_at_ms: g.expires_at_ms,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_chooser_grant_list_snapshot(&snapshot)?
            }
            SystemControlView::WorkspaceList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_workspaces(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::WorkspaceListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    workspaces: page
                        .workspaces
                        .into_iter()
                        .map(|w| sabi::v1::WorkspaceStatus {
                            workspace_id: w.workspace_id.to_vec(),
                            name: w.name,
                            layout_preset: w.layout_preset,
                            active_surface_count: w.active_surface_count,
                            focused_task_id: w
                                .focused_task_id
                                .map_or_else(Vec::new, |id| id.to_vec()),
                            expected_generation_or_revision: w.expected_generation_or_revision,
                            updated_at_ms: w.updated_at_ms,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_workspace_list_snapshot(&snapshot)?
            }
            SystemControlView::Workspace => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let workspace_id = fixed16(&payload.target_id)?;
                let result = source
                    .inspect_workspace(workspace_id, payload.target_generation)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::WorkspaceSnapshot {
                    schema: Some(system_control_schema_identity()),
                    stale_generation: result.stale_generation,
                    current_generation: result.current_generation,
                    workspace: result.workspace.map(|w| sabi::v1::WorkspaceStatus {
                        workspace_id: w.workspace_id.to_vec(),
                        name: w.name,
                        layout_preset: w.layout_preset,
                        active_surface_count: w.active_surface_count,
                        focused_task_id: w.focused_task_id.map_or_else(Vec::new, |id| id.to_vec()),
                        expected_generation_or_revision: w.expected_generation_or_revision,
                        updated_at_ms: w.updated_at_ms,
                    }),
                };
                nlos_schema::encode_workspace_snapshot(&snapshot)?
            }
            SystemControlView::ProcessList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_processes(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::ProcessListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    processes: page
                        .processes
                        .into_iter()
                        .map(|p| sabi::v1::ProcessSummaryStatus {
                            process_id: p.process_id.to_vec(),
                            expected_generation_or_revision: p.expected_generation_or_revision,
                            agent_instance_id: p.agent_instance_id.to_vec(),
                            task_id: p.task_id.to_vec(),
                            task_attempt_id: p.task_attempt_id.to_vec(),
                            isolation_domain_id: p.isolation_domain_id.to_vec(),
                            cell_id: p.cell_id.map_or_else(Vec::new, |id| id.to_vec()),
                            lifecycle_state: p.lifecycle_state,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_process_list_snapshot(&snapshot)?
            }
            SystemControlView::ResourceReservationList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_resource_reservations(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::ResourceReservationListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    reservations: page
                        .reservations
                        .into_iter()
                        .map(|r| sabi::v1::ResourceReservationSummaryStatus {
                            reservation_id: r.reservation_id.to_vec(),
                            account_id: r.account_id.to_vec(),
                            actual_usage: r.actual_usage,
                            effective_granted: r.effective_granted,
                            requested_ceiling: r.requested_ceiling,
                            context_residency_tier: r.context_residency_tier.into(),
                            expected_generation_or_revision: r.expected_generation_or_revision,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_resource_reservation_list_snapshot(&snapshot)?
            }
            SystemControlView::TopicList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_topics(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::TopicListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    topics: page
                        .topics
                        .into_iter()
                        .map(|t| sabi::v1::TopicSummaryStatus {
                            topic: Some(sabi::v1::TopicStatus {
                                topic_id: t.topic.topic_id.to_vec(),
                                channel_id: t.topic.channel_id.to_vec(),
                                channel_generation: t.topic.channel_generation,
                                name: t.topic.name,
                                active_subscriptions: t.topic.active_subscriptions,
                                policy_digest: t.topic.policy_digest,
                                created_at_ms: t.topic.created_at_ms,
                            }),
                            expected_generation_or_revision: t.expected_generation_or_revision,
                            retained_messages: t.retained_messages,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_topic_list_snapshot(&snapshot)?
            }
            SystemControlView::NotificationList => {
                let source = self
                    .desktop_enum_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = source
                    .list_notifications(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::NotificationListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    notifications: page
                        .notifications
                        .into_iter()
                        .map(|n| sabi::v1::NotificationSummaryStatus {
                            notification_id: n.notification.notification_id.to_vec(),
                            topic_id: n.notification.topic_id.to_vec(),
                            subscriber_key: n.notification.subscriber_key.to_vec(),
                            next_sequence: n.notification.next_sequence,
                            history_entry_count: n.notification.history_entry_count,
                            history_truncated: n.notification.history_truncated,
                            prune_watermark_sequence: n.notification.prune_watermark_sequence,
                            expected_generation_or_revision: n.expected_generation_or_revision,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_notification_list_snapshot(&snapshot)?
            }
            SystemControlView::CellList => {
                let inspector = self
                    .cell_topology_inspector
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cursor = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = inspector
                    .list_cells(cursor, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::CellListSnapshot {
                    schema: Some(system_control_schema_identity()),
                    cells: page
                        .cells
                        .into_iter()
                        .map(|cell| sabi::v1::CellSummaryStatus {
                            cell_id: cell.cell_id.to_vec(),
                            cell_label: cell.cell_label,
                            reachability: cell.reachability.into(),
                            partition_autonomy_gate: cell.partition_autonomy_gate.into(),
                            current_epoch: cell.current_epoch,
                            active_lease_count: cell.active_lease_count,
                            expected_generation_or_revision: cell.expected_generation_or_revision,
                        })
                        .collect(),
                    next_cursor: page.next_cursor.map_or_else(Vec::new, |c| c.to_vec()),
                    truncated: page.truncated,
                };
                nlos_schema::encode_cell_list_snapshot(&snapshot)?
            }
            SystemControlView::CellLeaseInspect => {
                let inspector = self
                    .cell_lease_inspector
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let cell_id = optional_fixed16(&payload.target_id)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let page = inspector
                    .inspect_cell_leases(cell_id, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::CellLeaseInspectSnapshot {
                    schema: Some(system_control_schema_identity()),
                    cell_id: page.cell_id.to_vec(),
                    leases: page
                        .leases
                        .into_iter()
                        .map(|lease| sabi::v1::CellLeaseStatus {
                            lease_id: lease.lease_id.to_vec(),
                            cell_id: lease.cell_id.to_vec(),
                            kind: lease.kind.into(),
                            lock_state: lease.lock_state.into(),
                            holder_id: lease.holder_id.to_vec(),
                            remaining_holder_ttl_ms: lease.remaining_holder_ttl_ms,
                            lease_epoch: lease.lease_epoch,
                            clock_skew_horizon_ms: lease.clock_skew_horizon_ms,
                        })
                        .collect(),
                    truncated: page.truncated,
                };
                nlos_schema::encode_cell_lease_inspect_snapshot(&snapshot)?
            }
            SystemControlView::ControlBlastRadiusPreview => {
                let source = self
                    .reconciliation_source
                    .ok_or(SystemControlError::LayerInspectionUnwired)?;
                let target_id = fixed16(&payload.target_id)?;
                let secondary_id = optional_fixed16(&payload.plan_id)?;
                let (scope, preview_action) =
                    unpack_blast_radius_preview_generation(payload.target_generation)?;
                let limit = usize::try_from(payload.alert_limit)
                    .unwrap_or(MAX_SYSTEM_CONTROL_ENUM_LIMIT)
                    .min(MAX_SYSTEM_CONTROL_ENUM_LIMIT);
                let preview = source
                    .preview_blast_radius(scope, target_id, secondary_id, preview_action, limit)
                    .map_err(SystemControlError::LayerInspection)?;
                let snapshot = sabi::v1::ControlBlastRadiusPreviewSnapshot {
                    schema: Some(system_control_schema_identity()),
                    target_scope: preview.target_scope.into(),
                    target_id: preview.target_id.to_vec(),
                    preview_action: preview.preview_action,
                    statutory_lock_blocked: preview.statutory_lock_blocked,
                    statutory_lock_code: preview.statutory_lock_code,
                    impacts: preview
                        .impacts
                        .into_iter()
                        .map(|item| sabi::v1::ControlBlastRadiusImpactItem {
                            sign: item.sign.into(),
                            affected_scope: item.affected_scope.into(),
                            affected_id: item.affected_id.to_vec(),
                            current_generation_or_revision: item.current_generation_or_revision,
                            summary_code: item.summary_code,
                        })
                        .collect(),
                    truncated: preview.truncated,
                };
                nlos_schema::encode_control_blast_radius_preview_snapshot(&snapshot)?
            }
            SystemControlView::ControlCommandStatus => {
                let control_command_id = fixed16(&payload.target_id)?;
                let found = if let Some(ledger) = self.command_ledger
                    && let Some(record) = ledger.lookup(control_command_id)
                {
                    Some(record)
                } else if let Some(source) = self.reconciliation_source {
                    source
                        .query_command_status(control_command_id)
                        .map_err(SystemControlError::LayerInspection)?
                } else if self.command_ledger.is_some() {
                    None
                } else {
                    return Err(SystemControlError::LayerInspectionUnwired);
                };
                let Some(record) = found else {
                    return Err(SystemControlError::LayerInspection(SabiFailure {
                        code: SabiErrorCode::NotFound.into(),
                        retry: RetryDirective::DoNotRetry.into(),
                        safe_message: "control_command_id was not found in reconciliation ledger"
                            .to_owned(),
                    }));
                };
                let snapshot = sabi::v1::ControlCommandStatusSnapshot {
                    schema: Some(system_control_schema_identity()),
                    control_command_id: record.control_command_id.to_vec(),
                    state: record.state.into(),
                    scope: record.scope.into(),
                    target_id: record.target_id.to_vec(),
                    receipt: record.receipt_id.map(|id| ReceiptReference {
                        receipt_id: id.to_vec(),
                    }),
                    failure: record.failure,
                };
                nlos_schema::encode_control_command_status_snapshot(&snapshot)?
            }
            _ => return Ok(None),
        };
        Ok(Some(response_envelope(
            request,
            context.correlation_id.clone(),
            encoded_payload,
            Vec::new(),
        )))
    }

    /// Dispatches Stage D (`ControlCommand` arms `21..=30`) mutations and
    /// enforces the Five Statutory Non-Tamperable Locks (`[UI-PARITY-001]`).
    #[allow(clippy::too_many_lines)]
    pub(crate) fn execute_stage_d_submit_arm(
        &self,
        command: &sabi::v1::ControlCommand,
        caller: &sabi::v1::CallerIdentity,
        context: &SabiRequestContext,
        now_wall_ms: i64,
    ) -> Result<ReceiptId, SystemControlError> {
        use sabi::v1::control_command::Command;

        let scope = ControlScope::try_from(command.scope)
            .map_err(|_| SystemControlError::Schema(CompatibilityError::UnspecifiedControlScope))?;
        let target_id = fixed16(&command.target_id)?;
        let issuer_principal_id = fixed16(&caller.principal_id)?;
        let idempotency_key = fixed16(&context.idempotency_key)?;
        let base_request = DesktopMutationRequest {
            scope,
            target_id,
            expected_generation_or_revision: command.expected_generation_or_revision,
            issuer_principal_id,
            idempotency_key,
            requested_at_ms: now_wall_ms,
        };

        match command.command.as_ref() {
            Some(Command::RequestProcessMigration(migration)) => {
                let inspector = self
                    .cell_topology_inspector
                    .ok_or(SystemControlError::OperationControlExecutionUnwired)?;
                let destination_cell_id = fixed16(&migration.destination_cell_id)?;
                inspector
                    .request_process_migration(ProcessMigrationControlRequest {
                        process_id: target_id,
                        destination_cell_id,
                        expected_generation_or_revision: command.expected_generation_or_revision,
                        issuer_principal_id,
                        idempotency_key,
                        requested_at_ms: now_wall_ms,
                    })
                    .map_err(SystemControlError::OperationExecution)
            }
            Some(Command::PurgeTopic(purge)) => {
                if !purge.acknowledge_data_loss {
                    return Err(SystemControlError::OperationExecution(SabiFailure {
                        code: SabiErrorCode::InvalidArgument.into(),
                        retry: RetryDirective::DoNotRetry.into(),
                        safe_message:
                            "PurgeTopic requires explicit acknowledge_data_loss=true confirmation"
                                .to_owned(),
                    }));
                }
                let executor = self
                    .desktop_mutation_executor
                    .ok_or(SystemControlError::OperationControlExecutionUnwired)?;
                executor
                    .purge_topic(base_request)
                    .map_err(SystemControlError::OperationExecution)
            }
            Some(Command::UpdateSettingsDomain(update)) => {
                let executor = self
                    .desktop_mutation_executor
                    .ok_or(SystemControlError::OperationControlExecutionUnwired)?;
                let domain_kind =
                    SettingsDomainKind::try_from(update.domain_kind).map_err(|_| {
                        SystemControlError::Schema(
                            CompatibilityError::InvalidSystemControlIdentifier,
                        )
                    })?;
                executor
                    .update_settings_domain(
                        base_request,
                        domain_kind,
                        &update.key,
                        &update.value_cbor,
                    )
                    .map_err(SystemControlError::OperationExecution)
            }
            Some(Command::SetContentTypeAssociation(assoc)) => {
                let executor = self
                    .desktop_mutation_executor
                    .ok_or(SystemControlError::OperationControlExecutionUnwired)?;
                let application_id = fixed16(&assoc.application_id)?;
                executor
                    .set_content_type_association(base_request, &assoc.content_type, application_id)
                    .map_err(SystemControlError::OperationExecution)
            }
            Some(Command::IssueChooserGrant(grant)) => {
                let executor = self
                    .desktop_mutation_executor
                    .ok_or(SystemControlError::OperationControlExecutionUnwired)?;
                let application_id = fixed16(&grant.application_id)?;
                let resource_handle = fixed16(&grant.resource_handle)?;
                if grant.application_id.len() != REQUEST_ID_BYTES
                    || grant.resource_handle.len() != REQUEST_ID_BYTES
                {
                    return Err(SystemControlError::Schema(
                        CompatibilityError::InvalidSystemControlIdentifier,
                    ));
                }
                executor
                    .issue_chooser_grant(
                        base_request,
                        application_id,
                        grant.chooser_kind,
                        resource_handle,
                    )
                    .map_err(SystemControlError::OperationExecution)
            }
            // Statutory Lock #1 (`[UI-PARITY-001]`): direct mutation against a
            // non-authoritative `0x1F` search candidate token is forbidden.
            Some(Command::MutateSearchCandidate(_)) => Err(statutory_lock_failure(
                SabiErrorCode::Rights,
                "STATUTORY_LOCK[UI-PARITY-001/1]: non-authoritative 0x1F search candidate cannot be mutated directly; resolve authoritative object_id first",
            )),
            // Statutory Lock #2 (`[UI-PARITY-001]`): `IRREVERSIBLE` receipts
            // cannot be undone.
            Some(Command::UndoReceipt(undo)) => {
                let undo_class =
                    TaskUndoClass::try_from(undo.receipt_undo_class).map_err(|_| {
                        SystemControlError::Schema(
                            CompatibilityError::InvalidSystemControlIdentifier,
                        )
                    })?;
                match undo_class {
                    TaskUndoClass::Irreversible => Err(statutory_lock_failure(
                        SabiErrorCode::State,
                        "STATUTORY_LOCK[UI-PARITY-001/2]: IRREVERSIBLE receipt cannot be undone",
                    )),
                    TaskUndoClass::ReadOnly => Err(statutory_lock_failure(
                        SabiErrorCode::State,
                        "READ_ONLY receipt carries no state transition to undo",
                    )),
                    TaskUndoClass::Reversible | TaskUndoClass::Compensatable => {
                        let executor = self
                            .desktop_mutation_executor
                            .ok_or(SystemControlError::OperationControlExecutionUnwired)?;
                        executor
                            .undo_reversible_receipt(base_request, undo_class)
                            .map_err(SystemControlError::OperationExecution)
                    }
                    TaskUndoClass::Unspecified => Err(SystemControlError::Schema(
                        CompatibilityError::InvalidSystemControlIdentifier,
                    )),
                }
            }
            // Statutory Lock #3 (`[UI-PARITY-001]`): `ACTIVE` or `COMPLETED`
            // `TaskNode` topology is immutable.
            Some(Command::MutateTaskNodeTopology(topology)) => {
                let plan_id = fixed16(&topology.plan_id)?;
                let observed = PlanNodeLifecycleState::try_from(topology.observed_node_state)
                    .unwrap_or(PlanNodeLifecycleState::Unspecified);
                let live_state = self
                    .task_node_source
                    .and_then(|src| src.inspect_task_node(plan_id, target_id).ok())
                    .map(|node| node.state);
                if matches!(
                    observed,
                    PlanNodeLifecycleState::Active | PlanNodeLifecycleState::Completed
                ) || matches!(
                    live_state,
                    Some(PlanNodeLifecycleState::Active | PlanNodeLifecycleState::Completed)
                ) {
                    return Err(statutory_lock_failure(
                        SabiErrorCode::State,
                        "STATUTORY_LOCK[UI-PARITY-001/3]: COMPLETED or ACTIVE TaskNode plan topology is immutable",
                    ));
                }
                Err(statutory_lock_failure(
                    SabiErrorCode::NotSupported,
                    "TaskNode topology mutation requires a signed PlanAuthority revision proposal",
                ))
            }
            // Statutory Lock #4 (`[UI-PARITY-001]`): manual commit-success
            // override on `TaskAttempt` is forbidden.
            Some(Command::OverrideTaskAttemptCommit(_)) => Err(statutory_lock_failure(
                SabiErrorCode::Rights,
                "STATUTORY_LOCK[UI-PARITY-001/4]: TaskAttempt commit winner is determined solely by CommitCoordinator CAS; manual commit override is forbidden",
            )),
            // Statutory Lock #5 (`[UI-PARITY-001]`): manual mark-succeeded or
            // mark-not-happened override on `Operation(EFFECT_UNKNOWN)` is forbidden.
            Some(Command::OverrideEffectUnknownOperation(_)) => Err(statutory_lock_failure(
                SabiErrorCode::State,
                "STATUTORY_LOCK[UI-PARITY-001/5]: EFFECT_UNKNOWN operation cannot be manually marked succeeded or not-happened without reconciliation evidence",
            )),
            _ => Err(CompatibilityError::MissingSystemControlCommand.into()),
        }
    }
}
