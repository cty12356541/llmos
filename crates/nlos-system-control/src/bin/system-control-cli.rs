//! `system-control-cli` — minimal typed control CLI over a real local IPC
//! endpoint (Unix-domain socket on Unix, Named Pipe on Windows).

#[cfg(any(unix, windows))]
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(any(unix, windows))]
use nlos_schema::SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL;
#[cfg(any(unix, windows))]
use nlos_system_control::control::{
    ApplicationInspector, ControlCommand, ControlError, ControlOutcome, ControlReceipt,
    NotificationInspector, ProcessInspector, ResourceInspector, ScopedControlAction,
    SearchInspector, parse_hex_32, parse_hex_id, receipt_to_hex,
};
#[cfg(any(unix, windows))]
use nlos_system_control::nl::{
    parse_control_scope, parse_plan_node_state, parse_preview_action_code,
    parse_settings_domain_kind, parse_undo_class, parse_unit_scoped_action,
};

#[cfg(any(unix, windows))]
const USAGE: &str = "usage: system-control-cli [--root <STATE_ROOT>] <ENDPOINT> inspect-health \
| inspect-semantic-health | inspect-resource-health | export-metrics | export-semantic-metrics \
| export-resource-metrics | inspect-task <PLAN_ID_HEX_32> | inspect-process <PROCESS_ID_HEX_32> \
| inspect-application <PACKAGE_ID_HEX_32> | inspect-search <SCOPE_ID_HEX_64> \
| inspect-notification <NOTIFICATION_ID_HEX_32> | inspect-resource <RESERVATION_ID_HEX_32> \
| inspect-task-group <GROUP_ID_HEX_32> | inspect-task-node <PLAN_ID_HEX_32> <NODE_ID_HEX_32> \
| inspect-fiber <FIBER_ID_HEX_32> <GENERATION> | inspect-topic <TOPIC_ID_HEX_32> \
| inspect-operation <OPERATION_ID_HEX_32> <GENERATION> \
| list-tasks <LIMIT> [<CURSOR_HEX_32>] \
| list-task-groups <ALL_OR_TASK_ID_HEX_32> <LIMIT> [<CURSOR_HEX_32>] \
| list-task-nodes <PLAN_ID_HEX_32> <EXPECTED_REVISION> <LIMIT> [<CURSOR_HEX_32>] \
| list-task-attempts <ALL_OR_FILTER_HEX_32> <EXPECTED_GENERATION> <LIMIT> [<CURSOR_HEX_32>] \
| list-execution-fibers <ALL_OR_PROCESS_HEX_32> <EXPECTED_GENERATION> <LIMIT> [<CURSOR_HEX_32>] \
| list-operations <ALL_OR_FIBER_HEX_32> <EXPECTED_GENERATION> <LIMIT> [<CURSOR_HEX_32>] \
| list-installed-applications <LIMIT> [<CURSOR_HEX_32>] \
| query-search-candidates <SCOPE_ID_HEX_64> <QUERY> <CURSOR_OFFSET> <LIMIT> \
| poll-watermark-events <SINCE_WATERMARK> <LIMIT> \
| inspect-settings-domain <DOMAIN_KIND> <ALL_OR_TARGET_HEX_32> <LIMIT> \
| list-content-type-associations <LIMIT> [<CURSOR_HEX_32>] \
| list-keychain-references <LIMIT> [<CURSOR_HEX_32>] \
| list-chooser-grants <LIMIT> [<CURSOR_HEX_32>] \
| list-workspaces <LIMIT> [<CURSOR_HEX_32>] \
| inspect-workspace <WORKSPACE_ID_HEX_32> <EXPECTED_REVISION> \
| list-processes <LIMIT> [<CURSOR_HEX_32>] \
| list-resource-reservations <LIMIT> [<CURSOR_HEX_32>] \
| list-topics <LIMIT> [<CURSOR_HEX_32>] \
| list-notifications <LIMIT> [<CURSOR_HEX_32>] \
| list-cells <LIMIT> [<CURSOR_HEX_32>] \
| inspect-cell-leases <ALL_OR_CELL_ID_HEX_32> <LIMIT> \
| preview-blast-radius <VERB> <SCOPE> <TARGET_ID_HEX_32> <LIMIT> \
| query-command-status <COMMAND_ID_HEX_32> \
| ack-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
| ack-semantic-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
| resume-semantic-recovery <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
| ack-resource-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
| resume-resource-recovery <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
| pause-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
| resume-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
| cancel-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
| kill-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
| throttle-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <PERCENT_1_TO_100> <EXPECTED_REVISION> <REASON> \
| reclaim-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
| disable-application <COMMAND_ID_HEX_32> <PACKAGE_ID_HEX_32> <EXPECTED_GENERATION> <REASON> \
| uninstall-application <COMMAND_ID_HEX_32> <PACKAGE_ID_HEX_32> <EXPECTED_GENERATION> <REASON> \
| scoped-operation <VERB> <SCOPE> <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> [<PERCENT>] \
| migrate-process <COMMAND_ID_HEX_32> <PROCESS_ID_HEX_32> <DESTINATION_CELL_ID_HEX_32> <EXPECTED_GENERATION> <REASON> \
| purge-topic <COMMAND_ID_HEX_32> <TOPIC_ID_HEX_32> <ACKNOWLEDGE_BOOL> <EXPECTED_GENERATION> <REASON> \
| update-settings-domain <COMMAND_ID_HEX_32> <DOMAIN_KIND> <SCOPE> <TARGET_ID_HEX_32> <KEY> <VALUE> <EXPECTED_REVISION> <REASON> \
| set-content-type-association <COMMAND_ID_HEX_32> <ASSOCIATION_ID_HEX_32> <CONTENT_TYPE> <APPLICATION_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
| issue-chooser-grant <COMMAND_ID_HEX_32> <GRANT_ID_HEX_32> <APPLICATION_ID_HEX_32> <KIND_U32> <RESOURCE_HANDLE_HEX_32> <EXPECTED_REVISION> <REASON> \
| undo-receipt <COMMAND_ID_HEX_32> <SCOPE> <TARGET_ID_HEX_32> <UNDO_CLASS> <EXPECTED_REVISION> <REASON> \
| mutate-search-candidate <COMMAND_ID_HEX_32> <CANDIDATE_ID_HEX_32> <SCOPE_ID_HEX_64> <EXPECTED_REVISION> <REASON> \
| mutate-task-node-topology <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <NODE_ID_HEX_32> <OBSERVED_STATE> <EXPECTED_REVISION> <REASON> \
| override-task-attempt-commit <COMMAND_ID_HEX_32> <ATTEMPT_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
| override-effect-unknown-operation <COMMAND_ID_HEX_32> <OPERATION_ID_HEX_32> <MARK_SUCCEEDED_BOOL> <EXPECTED_REVISION> <REASON>";

#[cfg(any(unix, windows))]
fn parse_u64(value: &str) -> Result<u64, ControlError> {
    value
        .parse::<u64>()
        .map_err(|_| ControlError::InvalidCommand("expected failure count must be a u64"))
}

#[cfg(any(unix, windows))]
fn parse_u32(value: &str) -> Result<u32, ControlError> {
    value
        .parse::<u32>()
        .map_err(|_| ControlError::InvalidCommand("limit must be a u32"))
}

#[cfg(any(unix, windows))]
fn parse_bool(value: &str) -> Result<bool, ControlError> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ControlError::InvalidCommand(
            "boolean must be true or false",
        )),
    }
}

#[cfg(any(unix, windows))]
fn parse_opt_id(value: &str) -> Result<Option<[u8; 16]>, ControlError> {
    if value.eq_ignore_ascii_case("all") || value.eq_ignore_ascii_case("none") {
        Ok(None)
    } else {
        parse_hex_id(value).map(Some)
    }
}

#[cfg(any(unix, windows))]
fn parse_throttle_percent(value: &str) -> Result<u64, ControlError> {
    let percent = parse_u64(value).map_err(|_| {
        ControlError::InvalidCommand("throttle percent must be a plain decimal u64")
    })?;
    if !(1..=100).contains(&percent) {
        return Err(ControlError::InvalidCommand(
            "throttle percent must be a whole percent from 1 to 100",
        ));
    }
    Ok(percent)
}

#[cfg(any(unix, windows))]
fn parse_optional_cursor_hex(
    arguments: &[String],
    index: usize,
) -> Result<Option<[u8; 16]>, ControlError> {
    match arguments.get(index) {
        None => Ok(None),
        Some(raw) => Ok(Some(parse_hex_id(raw)?)),
    }
}

#[cfg(any(unix, windows))]
#[allow(clippy::too_many_lines)]
fn parsed_stage_d_command(
    operation: &str,
    arguments: &[String],
) -> Option<Result<ControlCommand, ControlError>> {
    match operation {
        "list-tasks" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListTasks { cursor, limit })
            }))
        }
        "list-task-groups" if (3..=4).contains(&arguments.len()) => {
            Some(parse_opt_id(&arguments[1]).and_then(|task_id| {
                parse_u32(&arguments[2]).and_then(|limit| {
                    parse_optional_cursor_hex(arguments, 3).map(|cursor| {
                        ControlCommand::ListTaskGroups {
                            task_id,
                            cursor,
                            limit,
                        }
                    })
                })
            }))
        }
        "list-task-nodes" if (4..=5).contains(&arguments.len()) => {
            Some(parse_hex_id(&arguments[1]).and_then(|plan_id| {
                parse_u64(&arguments[2]).and_then(|expected_plan_revision| {
                    parse_u32(&arguments[3]).and_then(|limit| {
                        parse_optional_cursor_hex(arguments, 4).map(|cursor| {
                            ControlCommand::ListTaskNodes {
                                plan_id,
                                expected_plan_revision,
                                cursor,
                                limit,
                            }
                        })
                    })
                })
            }))
        }
        "list-task-attempts" if (4..=5).contains(&arguments.len()) => {
            Some(parse_opt_id(&arguments[1]).and_then(|scope_filter_id| {
                parse_u64(&arguments[2]).and_then(|expected_generation| {
                    parse_u32(&arguments[3]).and_then(|limit| {
                        parse_optional_cursor_hex(arguments, 4).map(|cursor| {
                            ControlCommand::ListTaskAttempts {
                                scope_filter_id,
                                expected_generation,
                                cursor,
                                limit,
                            }
                        })
                    })
                })
            }))
        }
        "list-execution-fibers" if (4..=5).contains(&arguments.len()) => {
            Some(parse_opt_id(&arguments[1]).and_then(|process_id| {
                parse_u64(&arguments[2]).and_then(|expected_generation| {
                    parse_u32(&arguments[3]).and_then(|limit| {
                        parse_optional_cursor_hex(arguments, 4).map(|cursor| {
                            ControlCommand::ListExecutionFibers {
                                process_id,
                                expected_generation,
                                cursor,
                                limit,
                            }
                        })
                    })
                })
            }))
        }
        "list-operations" if (4..=5).contains(&arguments.len()) => {
            Some(parse_opt_id(&arguments[1]).and_then(|fiber_id| {
                parse_u64(&arguments[2]).and_then(|expected_generation| {
                    parse_u32(&arguments[3]).and_then(|limit| {
                        parse_optional_cursor_hex(arguments, 4).map(|cursor| {
                            ControlCommand::ListOperations {
                                fiber_id,
                                expected_generation,
                                cursor,
                                limit,
                            }
                        })
                    })
                })
            }))
        }
        "list-installed-applications" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListInstalledApplications { cursor, limit })
            }))
        }
        "query-search-candidates" if arguments.len() == 5 => {
            Some(parse_hex_32(&arguments[1]).and_then(|scope_id| {
                parse_u64(&arguments[3]).and_then(|cursor_offset| {
                    parse_u32(&arguments[4]).map(|limit| ControlCommand::QuerySearchCandidates {
                        scope_id,
                        query: arguments[2].clone(),
                        cursor_offset,
                        limit,
                    })
                })
            }))
        }
        "poll-watermark-events" if arguments.len() == 3 => {
            Some(parse_u64(&arguments[1]).and_then(|since_watermark| {
                parse_u32(&arguments[2]).map(|limit| ControlCommand::PollDesktopWatermarkEvents {
                    since_watermark,
                    limit,
                })
            }))
        }
        "inspect-settings-domain" if arguments.len() == 4 => Some(
            parse_settings_domain_kind(&arguments[1]).and_then(|domain_kind| {
                parse_opt_id(&arguments[2]).and_then(|scope_target_id| {
                    parse_u32(&arguments[3]).map(|limit| ControlCommand::InspectSettingsDomain {
                        domain_kind,
                        scope_target_id,
                        limit,
                    })
                })
            }),
        ),
        "list-content-type-associations" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListContentTypeAssociations { cursor, limit })
            }))
        }
        "list-keychain-references" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListKeychainReferences { cursor, limit })
            }))
        }
        "list-chooser-grants" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListChooserGrants { cursor, limit })
            }))
        }
        "list-workspaces" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListWorkspaces { cursor, limit })
            }))
        }
        "inspect-workspace" if arguments.len() == 3 => {
            Some(parse_hex_id(&arguments[1]).and_then(|workspace_id| {
                parse_u64(&arguments[2]).map(|expected_revision| ControlCommand::InspectWorkspace {
                    workspace_id,
                    expected_revision,
                })
            }))
        }
        "list-processes" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListProcesses { cursor, limit })
            }))
        }
        "list-resource-reservations" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListResourceReservations { cursor, limit })
            }))
        }
        "list-topics" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListTopics { cursor, limit })
            }))
        }
        "list-notifications" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListNotifications { cursor, limit })
            }))
        }
        "list-cells" if (2..=3).contains(&arguments.len()) => {
            Some(parse_u32(&arguments[1]).and_then(|limit| {
                parse_optional_cursor_hex(arguments, 2)
                    .map(|cursor| ControlCommand::ListCells { cursor, limit })
            }))
        }
        "inspect-cell-leases" if arguments.len() == 3 => {
            Some(parse_opt_id(&arguments[1]).and_then(|cell_id| {
                parse_u32(&arguments[2])
                    .map(|limit| ControlCommand::InspectCellLeases { cell_id, limit })
            }))
        }
        "preview-blast-radius" if (5..=6).contains(&arguments.len()) => Some((|| {
            let preview_action = parse_preview_action_code(&arguments[1])?;
            let target_scope = parse_control_scope(&arguments[2])?;
            let target_id = parse_hex_id(&arguments[3])?;
            let limit = parse_u32(&arguments[4])?;
            let secondary_id = match arguments.get(5) {
                Some(raw) => Some(parse_hex_id(raw)?),
                None => None,
            };
            Ok(ControlCommand::PreviewControlBlastRadius {
                target_scope,
                target_id,
                secondary_id,
                preview_action,
                limit,
            })
        })()),
        "query-command-status" if arguments.len() == 2 => {
            Some(parse_hex_id(&arguments[1]).map(|target_command_id| {
                ControlCommand::QueryControlCommandStatus { target_command_id }
            }))
        }
        "scoped-operation" if (7..=8).contains(&arguments.len()) => Some((|| {
            let scope = parse_control_scope(&arguments[2])?;
            let control_command_id = parse_hex_id(&arguments[3])?;
            let target_id = parse_hex_id(&arguments[4])?;
            let expected_generation_or_revision = parse_u64(&arguments[5])?;
            let reason = arguments[6].clone();
            let action = if arguments[1].eq_ignore_ascii_case("throttle") {
                let Some(pct_raw) = arguments.get(7) else {
                    return Err(ControlError::InvalidCommand(
                        "scoped-operation throttle requires <PERCENT>",
                    ));
                };
                ScopedControlAction::Throttle {
                    throttle_percent: parse_throttle_percent(pct_raw)?,
                }
            } else {
                if arguments.len() != 7 {
                    return Err(ControlError::InvalidCommand("unknown operation or arity"));
                }
                parse_unit_scoped_action(&arguments[1])?.0
            };
            Ok(ControlCommand::ScopedOperation {
                scope,
                action,
                control_command_id,
                target_id,
                expected_generation_or_revision,
                reason,
            })
        })()),
        "migrate-process" if arguments.len() == 6 => Some((|| {
            Ok(ControlCommand::RequestProcessMigration {
                control_command_id: parse_hex_id(&arguments[1])?,
                process_id: parse_hex_id(&arguments[2])?,
                destination_cell_id: parse_hex_id(&arguments[3])?,
                expected_generation_or_revision: parse_u64(&arguments[4])?,
                reason: arguments[5].clone(),
            })
        })()),
        "purge-topic" if arguments.len() == 6 => Some((|| {
            Ok(ControlCommand::PurgeTopic {
                control_command_id: parse_hex_id(&arguments[1])?,
                topic_id: parse_hex_id(&arguments[2])?,
                acknowledge_data_loss: parse_bool(&arguments[3])?,
                expected_generation_or_revision: parse_u64(&arguments[4])?,
                reason: arguments[5].clone(),
            })
        })()),
        "update-settings-domain" if arguments.len() == 9 => Some((|| {
            Ok(ControlCommand::UpdateSettingsDomain {
                control_command_id: parse_hex_id(&arguments[1])?,
                domain_kind: parse_settings_domain_kind(&arguments[2])?,
                scope: parse_control_scope(&arguments[3])?,
                target_id: parse_hex_id(&arguments[4])?,
                key: arguments[5].clone(),
                value_cbor: arguments[6].as_bytes().to_vec(),
                expected_generation_or_revision: parse_u64(&arguments[7])?,
                reason: arguments[8].clone(),
            })
        })()),
        "set-content-type-association" if arguments.len() == 7 => Some((|| {
            Ok(ControlCommand::SetContentTypeAssociation {
                control_command_id: parse_hex_id(&arguments[1])?,
                association_id: parse_hex_id(&arguments[2])?,
                content_type: arguments[3].clone(),
                application_id: parse_hex_id(&arguments[4])?,
                expected_generation_or_revision: parse_u64(&arguments[5])?,
                reason: arguments[6].clone(),
            })
        })()),
        "issue-chooser-grant" if arguments.len() == 8 => Some((|| {
            Ok(ControlCommand::IssueChooserGrant {
                control_command_id: parse_hex_id(&arguments[1])?,
                grant_id: parse_hex_id(&arguments[2])?,
                application_id: parse_hex_id(&arguments[3])?,
                chooser_kind: parse_u32(&arguments[4])?,
                resource_handle: parse_hex_id(&arguments[5])?,
                expected_generation_or_revision: parse_u64(&arguments[6])?,
                reason: arguments[7].clone(),
            })
        })()),
        "undo-receipt" if arguments.len() == 7 => Some((|| {
            Ok(ControlCommand::UndoReceipt {
                control_command_id: parse_hex_id(&arguments[1])?,
                scope: parse_control_scope(&arguments[2])?,
                target_id: parse_hex_id(&arguments[3])?,
                receipt_undo_class: parse_undo_class(&arguments[4])?,
                expected_generation_or_revision: parse_u64(&arguments[5])?,
                reason: arguments[6].clone(),
            })
        })()),
        "mutate-search-candidate" if arguments.len() == 6 => Some((|| {
            Ok(ControlCommand::MutateSearchCandidate {
                control_command_id: parse_hex_id(&arguments[1])?,
                candidate_id: parse_hex_id(&arguments[2])?,
                scope_id: parse_hex_32(&arguments[3])?,
                non_authoritative_restrictions_bits: SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
                expected_generation_or_revision: parse_u64(&arguments[4])?,
                reason: arguments[5].clone(),
            })
        })()),
        "mutate-task-node-topology" if arguments.len() == 7 => Some((|| {
            Ok(ControlCommand::MutateTaskNodeTopology {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                node_id: parse_hex_id(&arguments[3])?,
                observed_node_state: parse_plan_node_state(&arguments[4])?,
                expected_generation_or_revision: parse_u64(&arguments[5])?,
                reason: arguments[6].clone(),
            })
        })()),
        "override-task-attempt-commit" if arguments.len() == 5 => Some((|| {
            Ok(ControlCommand::OverrideTaskAttemptCommit {
                control_command_id: parse_hex_id(&arguments[1])?,
                attempt_id: parse_hex_id(&arguments[2])?,
                expected_generation_or_revision: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        })()),
        "override-effect-unknown-operation" if arguments.len() == 6 => Some((|| {
            Ok(ControlCommand::OverrideEffectUnknownOperation {
                control_command_id: parse_hex_id(&arguments[1])?,
                operation_id: parse_hex_id(&arguments[2])?,
                mark_succeeded: parse_bool(&arguments[3])?,
                expected_generation_or_revision: parse_u64(&arguments[4])?,
                reason: arguments[5].clone(),
            })
        })()),
        _ => None,
    }
}

#[cfg(any(unix, windows))]
#[allow(clippy::too_many_lines)]
fn parsed_command(arguments: &[String]) -> Result<ControlCommand, ControlError> {
    let Some(operation) = arguments.first().map(String::as_str) else {
        return Err(ControlError::InvalidCommand("missing control operation"));
    };
    if let Some(stage_d) = parsed_stage_d_command(operation, arguments) {
        return stage_d;
    }
    match operation {
        "inspect-health" if arguments.len() == 1 => Ok(ControlCommand::InspectHealth),
        "inspect-semantic-health" if arguments.len() == 1 => {
            Ok(ControlCommand::InspectSemanticHealth)
        }
        "inspect-resource-health" if arguments.len() == 1 => {
            Ok(ControlCommand::InspectResourceHealth)
        }
        "export-metrics" if arguments.len() == 1 => Ok(ControlCommand::ExportMetrics),
        "export-semantic-metrics" if arguments.len() == 1 => {
            Ok(ControlCommand::ExportSemanticMetrics)
        }
        "export-resource-metrics" if arguments.len() == 1 => {
            Ok(ControlCommand::ExportResourceMetrics)
        }
        "inspect-task" if arguments.len() == 2 => Ok(ControlCommand::InspectTask {
            plan_id: parse_hex_id(&arguments[1])?,
        }),
        "inspect-process" if arguments.len() == 2 => Ok(ControlCommand::InspectProcess {
            process_id: parse_hex_id(&arguments[1])?,
        }),
        "inspect-application" if arguments.len() == 2 => Ok(ControlCommand::InspectApplication {
            package_id: parse_hex_id(&arguments[1])?,
        }),
        "inspect-search" if arguments.len() == 2 => Ok(ControlCommand::InspectSearch {
            scope_id: parse_hex_32(&arguments[1])?,
        }),
        "inspect-notification" if arguments.len() == 2 => Ok(ControlCommand::InspectNotification {
            notification_id: parse_hex_id(&arguments[1])?,
        }),
        "inspect-resource" if arguments.len() == 2 => Ok(ControlCommand::InspectResource {
            reservation_id: parse_hex_id(&arguments[1])?,
        }),
        "inspect-task-group" if arguments.len() == 2 => Ok(ControlCommand::InspectTaskGroup {
            group_id: parse_hex_id(&arguments[1])?,
        }),
        "inspect-task-node" if arguments.len() == 3 => Ok(ControlCommand::InspectTaskNode {
            plan_id: parse_hex_id(&arguments[1])?,
            node_id: parse_hex_id(&arguments[2])?,
        }),
        "inspect-fiber" if arguments.len() == 3 => {
            let generation = parse_u64(&arguments[2]).map_err(|_| {
                ControlError::InvalidCommand("fiber generation must be a non-zero u64")
            })?;
            if generation == 0 {
                return Err(ControlError::InvalidCommand(
                    "fiber generation must be a non-zero u64",
                ));
            }
            Ok(ControlCommand::InspectExecutionFiber {
                fiber_id: parse_hex_id(&arguments[1])?,
                generation,
            })
        }
        "inspect-topic" if arguments.len() == 2 => Ok(ControlCommand::InspectTopic {
            topic_id: parse_hex_id(&arguments[1])?,
        }),
        "inspect-operation" if arguments.len() == 3 => {
            let generation = parse_u64(&arguments[2]).map_err(|_| {
                ControlError::InvalidCommand("operation generation must be a non-zero u64")
            })?;
            if generation == 0 {
                return Err(ControlError::InvalidCommand(
                    "operation generation must be a non-zero u64",
                ));
            }
            Ok(ControlCommand::InspectOperation {
                operation_id: parse_hex_id(&arguments[1])?,
                generation,
            })
        }
        "ack-recovery-alert" if arguments.len() == 5 => {
            Ok(ControlCommand::AcknowledgeRecoveryAlert {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "ack-semantic-recovery-alert" if arguments.len() == 5 => {
            Ok(ControlCommand::AcknowledgeSemanticRecoveryAlert {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "resume-semantic-recovery" if arguments.len() == 5 => {
            Ok(ControlCommand::ResumeSemanticRecovery {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "ack-resource-recovery-alert" if arguments.len() == 5 => {
            Ok(ControlCommand::AcknowledgeResourceRecoveryAlert {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "resume-resource-recovery" if arguments.len() == 5 => {
            Ok(ControlCommand::ResumeResourceRecovery {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "pause-operation" if arguments.len() == 5 => Ok(ControlCommand::PauseOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "resume-operation" if arguments.len() == 5 => Ok(ControlCommand::ResumeOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "cancel-operation" if arguments.len() == 5 => Ok(ControlCommand::CancelOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "kill-operation" if arguments.len() == 5 => Ok(ControlCommand::KillOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "throttle-operation" if arguments.len() == 6 => Ok(ControlCommand::ThrottleOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            throttle_percent: parse_throttle_percent(&arguments[3])?,
            expected_generation_or_revision: parse_u64(&arguments[4])?,
            reason: arguments[5].clone(),
        }),
        "reclaim-operation" if arguments.len() == 5 => Ok(ControlCommand::ReclaimOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "disable-application" if arguments.len() == 5 => Ok(ControlCommand::DisableApplication {
            control_command_id: parse_hex_id(&arguments[1])?,
            package_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "uninstall-application" if arguments.len() == 5 => {
            Ok(ControlCommand::UninstallApplication {
                control_command_id: parse_hex_id(&arguments[1])?,
                package_id: parse_hex_id(&arguments[2])?,
                expected_generation_or_revision: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        _ => Err(ControlError::InvalidCommand("unknown operation or arity")),
    }
}

#[cfg(any(unix, windows))]
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(any(unix, windows))]
#[allow(clippy::too_many_lines)]
fn stage_d_summary(outcome: &ControlOutcome) -> Option<String> {
    Some(match outcome {
        ControlOutcome::StaleGeneration {
            view,
            target_id,
            expected_generation,
            current_generation,
        } => format!(
            "outcome=stale_generation view={view:?} target_id={} expected={expected_generation} current={current_generation}",
            target_id
                .as_ref()
                .map_or_else(|| "none".to_owned(), |id| hex(id)),
        ),
        ControlOutcome::TasksListed(s) => format!(
            "outcome=tasks_listed items={} truncated={}",
            s.tasks.len(),
            s.truncated
        ),
        ControlOutcome::TaskGroupsListed(s) => format!(
            "outcome=task_groups_listed items={} truncated={}",
            s.groups.len(),
            s.truncated
        ),
        ControlOutcome::TaskNodesListed(s) => format!(
            "outcome=task_nodes_listed plan_id={} items={} collapsed_meta={} collapsed_cold={} truncated={}",
            hex(&s.plan_id),
            s.nodes.len(),
            s.metadata_only_collapsed_count,
            s.cold_collapsed_count,
            s.truncated
        ),
        ControlOutcome::TaskAttemptsListed(s) => format!(
            "outcome=task_attempts_listed items={} current_generation={} truncated={}",
            s.attempts.len(),
            s.current_generation,
            s.truncated
        ),
        ControlOutcome::ExecutionFibersListed(s) => format!(
            "outcome=execution_fibers_listed items={} current_generation={} truncated={}",
            s.fibers.len(),
            s.current_generation,
            s.truncated
        ),
        ControlOutcome::OperationsListed(s) => format!(
            "outcome=operations_listed items={} current_generation={} truncated={}",
            s.operations.len(),
            s.current_generation,
            s.truncated
        ),
        ControlOutcome::InstalledApplicationsListed(s) => format!(
            "outcome=installed_applications_listed items={} truncated={}",
            s.applications.len(),
            s.truncated
        ),
        ControlOutcome::SearchCandidatesQueried(s) => format!(
            "outcome=search_candidates_queried scope_id={} items={} restrictions=0x{:x} truncated={}",
            hex(&s.scope_id),
            s.candidates.len(),
            s.non_authoritative_restrictions_bits,
            s.truncated
        ),
        ControlOutcome::DesktopWatermarkPolled(s) => format!(
            "outcome=desktop_watermark_polled current={} events={} truncated={}",
            s.current_watermark,
            s.events.len(),
            s.truncated
        ),
        ControlOutcome::SettingsDomainInspected(s) => format!(
            "outcome=settings_domain_inspected kind={:?} revision={} entries={}",
            s.domain_kind,
            s.domain_revision,
            s.entries.len()
        ),
        ControlOutcome::ContentTypeAssociationsListed(s) => format!(
            "outcome=content_type_associations_listed items={} truncated={}",
            s.associations.len(),
            s.truncated
        ),
        ControlOutcome::KeychainReferencesListed(s) => format!(
            "outcome=keychain_references_listed items={} truncated={}",
            s.references.len(),
            s.truncated
        ),
        ControlOutcome::ChooserGrantsListed(s) => format!(
            "outcome=chooser_grants_listed items={} truncated={}",
            s.grants.len(),
            s.truncated
        ),
        ControlOutcome::WorkspacesListed(s) => format!(
            "outcome=workspaces_listed items={} truncated={}",
            s.workspaces.len(),
            s.truncated
        ),
        ControlOutcome::WorkspaceInspected(s) => format!(
            "outcome=workspace_inspected workspace_id={} revision={}",
            hex(&s.workspace_id),
            s.expected_generation_or_revision
        ),
        ControlOutcome::ProcessesListed(s) => format!(
            "outcome=processes_listed items={} truncated={}",
            s.processes.len(),
            s.truncated
        ),
        ControlOutcome::ResourceReservationsListed(s) => format!(
            "outcome=resource_reservations_listed items={} truncated={}",
            s.reservations.len(),
            s.truncated
        ),
        ControlOutcome::TopicsListed(s) => format!(
            "outcome=topics_listed items={} truncated={}",
            s.topics.len(),
            s.truncated
        ),
        ControlOutcome::NotificationsListed(s) => format!(
            "outcome=notifications_listed items={} truncated={}",
            s.notifications.len(),
            s.truncated
        ),
        ControlOutcome::CellsListed(s) => format!(
            "outcome=cells_listed items={} truncated={}",
            s.cells.len(),
            s.truncated
        ),
        ControlOutcome::CellLeasesInspected(s) => format!(
            "outcome=cell_leases_inspected cell_id={} items={} truncated={}",
            hex(&s.cell_id),
            s.leases.len(),
            s.truncated
        ),
        ControlOutcome::BlastRadiusPreviewed(s) => format!(
            "outcome=blast_radius_previewed scope={:?} target_id={} items={} blocked={}",
            s.target_scope,
            hex(&s.target_id),
            s.impacts.len(),
            s.statutory_lock_blocked
        ),
        ControlOutcome::CommandStatusQueried(s) => format!(
            "outcome=command_status_queried command_id={} state={:?}",
            hex(&s.control_command_id),
            s.state
        ),
        ControlOutcome::ProcessMigrationRequested { receipt_id } => format!(
            "outcome=process_migration_requested receipt_id={}",
            hex(receipt_id)
        ),
        ControlOutcome::TopicPurged { receipt_id } => {
            format!("outcome=topic_purged receipt_id={}", hex(receipt_id))
        }
        ControlOutcome::SettingsDomainUpdated { receipt_id } => format!(
            "outcome=settings_domain_updated receipt_id={}",
            hex(receipt_id)
        ),
        ControlOutcome::ContentTypeAssociationSet { receipt_id } => format!(
            "outcome=content_type_association_set receipt_id={}",
            hex(receipt_id)
        ),
        ControlOutcome::ChooserGrantIssued { receipt_id } => format!(
            "outcome=chooser_grant_issued receipt_id={}",
            hex(receipt_id)
        ),
        ControlOutcome::ReceiptUndone { receipt_id } => {
            format!("outcome=receipt_undone receipt_id={}", hex(receipt_id))
        }
        _ => return None,
    })
}

#[cfg(any(unix, windows))]
#[allow(clippy::too_many_lines)]
fn layer_summary(receipt: &ControlReceipt) -> Option<String> {
    Some(match receipt.outcome.as_ref().ok()? {
        ControlOutcome::TaskGroupInspected(inspection) => format!(
            "outcome=task_group_inspected group_id={} task_id={} state={:?} \
             membership_generation={} state_seq={} depth={} cancel_epoch={} \
             members={} members_truncated={}",
            hex(&inspection.group_id),
            hex(&inspection.task_id),
            inspection.state,
            inspection.membership_generation,
            inspection.state_seq,
            inspection.depth,
            inspection.cancel_epoch,
            inspection.members.len(),
            inspection.members_truncated,
        ),
        ControlOutcome::TaskNodeInspected(inspection) => format!(
            "outcome=task_node_inspected plan_id={} node_id={} kind={:?} state={:?} \
             declared_revision={} residency={:?} transition_count={}",
            hex(&inspection.plan_id),
            hex(&inspection.node_id),
            inspection.kind,
            inspection.state,
            inspection.declared_revision,
            inspection.residency_tier,
            inspection.transition_count,
        ),
        ControlOutcome::ExecutionFiberInspected(inspection) => format!(
            "outcome=execution_fiber_inspected fiber_id={} generation={} state={:?} \
             phase={:?} active_cpu_ms={} elapsed_wall_ms={}",
            hex(&inspection.fiber_id),
            inspection.generation,
            inspection.state,
            inspection.lifecycle_phase,
            inspection.active_cpu_ms,
            inspection.elapsed_wall_ms,
        ),
        ControlOutcome::TopicInspected(inspection) => format!(
            "outcome=topic_inspected topic_id={} channel_id={} channel_generation={} \
             active_subscriptions={}",
            hex(&inspection.topic_id),
            hex(&inspection.channel_id),
            inspection.channel_generation,
            inspection.active_subscriptions,
        ),
        ControlOutcome::DurableOperationInspected(inspection) => format!(
            "outcome=operation_inspected operation_id={} generation={} state={:?} \
             cancel_epoch={} owner_fiber={} outcome_receipt={}",
            hex(&inspection.operation_id),
            inspection.generation,
            inspection.state,
            inspection.cancel_epoch,
            hex(&inspection.owner_fiber_id),
            inspection
                .outcome_receipt_id
                .as_deref()
                .map_or_else(|| "none".to_owned(), hex),
        ),
        other => return stage_d_summary(other),
    })
}

#[cfg(any(unix, windows))]
#[allow(clippy::too_many_lines)]
fn summary(receipt: &ControlReceipt) -> String {
    if let Some(layer) = layer_summary(receipt) {
        return layer;
    }
    match receipt.outcome.as_ref() {
        Ok(ControlOutcome::Inspected(inspection)) => format!(
            "outcome=inspected worker_state={:?} completed_cycles={} \
             durable_retrying={} durable_escalated={} \
             durable_unacknowledged_escalated={} durable_resolved={} alerts={}",
            inspection.worker_state,
            inspection.completed_cycles,
            inspection.durable_retrying,
            inspection.durable_escalated,
            inspection.durable_unacknowledged_escalated,
            inspection.durable_resolved,
            inspection.alerts.len(),
        ),
        Ok(ControlOutcome::SemanticInspected(inspection)) => format!(
            "outcome=semantic_inspected total_inspected={} total_finalized={} \
             consecutive_failed_cycles={} domain_faulted={} \
             durable_retrying={} durable_escalated={} \
             durable_unacknowledged_escalated={} durable_resolved={} alerts={}",
            inspection.total_inspected,
            inspection.total_finalized,
            inspection.consecutive_failed_cycles,
            inspection.domain_faulted,
            inspection.durable_retrying,
            inspection.durable_escalated,
            inspection.durable_unacknowledged_escalated,
            inspection.durable_resolved,
            inspection.alerts.len(),
        ),
        Ok(ControlOutcome::ResourceRecoveryInspected(inspection)) => format!(
            "outcome=resource_recovery_inspected total_inspected={} total_finalized={} \
             consecutive_failed_cycles={} domain_faulted={} \
             durable_retrying={} durable_escalated={} \
             durable_unacknowledged_escalated={} durable_resolved={} alerts={}",
            inspection.total_inspected,
            inspection.total_finalized,
            inspection.consecutive_failed_cycles,
            inspection.domain_faulted,
            inspection.durable_retrying,
            inspection.durable_escalated,
            inspection.durable_unacknowledged_escalated,
            inspection.durable_resolved,
            inspection.alerts.len(),
        ),
        Ok(ControlOutcome::ProcessInspected(inspection)) => format!(
            "outcome=process_inspected process_id={} generation={} task_id={}",
            hex(&inspection.process_id),
            inspection.process_generation,
            hex(&inspection.task_id),
        ),
        Ok(ControlOutcome::ApplicationInspected(inspection)) => format!(
            "outcome=application_inspected package_id={} application_id={} generation={} \
             status={}",
            hex(&inspection.package_id),
            hex(&inspection.application_id),
            inspection.current_installation_generation,
            inspection.status,
        ),
        Ok(ControlOutcome::SearchInspected(inspection)) => format!(
            "outcome=search_inspected scope_id={} consistency_class={} degradation_policy={} \
             restrictions_bits={} admitted={} active={} retracted={} truncated={}",
            hex(&inspection.scope_id),
            inspection.consistency_class,
            inspection.degradation_policy,
            inspection.non_authoritative_restrictions_bits,
            inspection.admitted_event_count,
            inspection.active_assertion_count,
            inspection.retracted_event_count,
            inspection.truncated,
        ),
        Ok(ControlOutcome::NotificationInspected(inspection)) => format!(
            "outcome=notification_inspected notification_id={} topic_id={} subscriber_key={} \
             next_sequence={} history_entries={} history_truncated={} prune_watermark={}",
            hex(&inspection.notification_id),
            hex(&inspection.topic_id),
            hex(&inspection.subscriber_key),
            inspection.next_sequence,
            inspection.history_entry_count,
            inspection.history_truncated,
            inspection.prune_watermark_sequence,
        ),
        Ok(ControlOutcome::ResourceInspected(inspection)) => format!(
            "outcome=resource_inspected reservation_id={} account_id={} upper_bound={} \
             usage_high_water={} consumptions={}",
            hex(&inspection.reservation_id),
            hex(&inspection.account_id),
            inspection.upper_bound,
            inspection.usage_high_water,
            inspection.consumption_count,
        ),
        Ok(ControlOutcome::MetricsExported(export)) => format!(
            "outcome=metrics_exported bytes={}",
            export.openmetrics_text.len(),
        ),
        Ok(ControlOutcome::Acknowledged { receipt_id }) => {
            format!("outcome=acknowledged receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::Resumed { receipt_id }) => {
            format!("outcome=resumed receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationPaused { receipt_id }) => {
            format!("outcome=operation_paused receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationResumed { receipt_id }) => {
            format!("outcome=operation_resumed receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationCancelled { receipt_id }) => {
            format!("outcome=operation_cancelled receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationKilled { receipt_id }) => {
            format!("outcome=operation_killed receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationThrottled { receipt_id }) => {
            format!("outcome=operation_throttled receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationReclaimed { receipt_id }) => {
            format!("outcome=operation_reclaimed receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::ApplicationDisabled { receipt_id }) => format!(
            "outcome=application_disabled receipt_id={}",
            hex(receipt_id)
        ),
        Ok(ControlOutcome::ApplicationUninstalled { receipt_id }) => format!(
            "outcome=application_uninstalled receipt_id={}",
            hex(receipt_id)
        ),
        Ok(_) => unreachable!("layer and stage D outcomes are summarized by layer_summary"),
        Err(failure) => format!(
            "outcome=failure code={} retry={} message={}",
            failure.code, failure.retry, failure.safe_message,
        ),
    }
}

#[cfg(all(
    any(unix, windows),
    any(
        feature = "process",
        feature = "resource",
        feature = "application",
        feature = "search",
        feature = "notify",
    )
))]
struct RootState {
    #[cfg(feature = "process")]
    process: nlos_process::ProcessAuthority,
    #[cfg(feature = "resource")]
    resource: nlos_resource::ResourceAuthority,
    #[cfg(feature = "application")]
    application: nlos_application::ApplicationAuthority,
    #[cfg(feature = "search")]
    semantic: std::sync::Arc<nlos_semantic::SemanticAuthority>,
    #[cfg(feature = "search")]
    search: nlos_search::SearchService,
    #[cfg(feature = "notify")]
    notify_topics: std::sync::Arc<nlos_topic::TopicAuthority>,
    #[cfg(feature = "notify")]
    notify: nlos_notify::NotificationService,
}

#[cfg(all(
    any(unix, windows),
    any(
        feature = "process",
        feature = "resource",
        feature = "application",
        feature = "search",
        feature = "notify",
    )
))]
impl RootState {
    fn open(root: &std::path::Path) -> Result<Self, String> {
        #[cfg(not(any(
            feature = "process",
            feature = "resource",
            feature = "application",
            feature = "search",
            feature = "notify",
        )))]
        let _ = root;
        #[cfg(feature = "process")]
        let process = nlos_process::ProcessAuthority::open(root.join("process"))
            .map_err(|error| format!("--root process authority: {error}"))?;
        #[cfg(feature = "resource")]
        let resource = nlos_resource::ResourceAuthority::open(root.join("resource"))
            .map_err(|error| format!("--root resource authority: {error}"))?;
        #[cfg(feature = "application")]
        let application = nlos_application::ApplicationAuthority::open(root.join("application"))
            .map_err(|error| format!("--root application authority: {error}"))?;
        #[cfg(feature = "search")]
        let semantic = std::sync::Arc::new(
            nlos_semantic::SemanticAuthority::open(root.join("semantic"))
                .map_err(|error| format!("--root semantic authority: {error}"))?,
        );
        #[cfg(feature = "search")]
        let search = nlos_search::SearchService::open(
            root.join("semantic"),
            std::sync::Arc::clone(&semantic),
        )
        .map_err(|error| format!("--root search service: {error}"))?;
        #[cfg(feature = "notify")]
        let (notify_topics, notify) = {
            let channel = std::sync::Arc::new(
                nlos_channel::ChannelAuthority::open(root.join("channel"))
                    .map_err(|error| format!("--root channel authority: {error}"))?,
            );
            let topics = std::sync::Arc::new(
                nlos_topic::TopicAuthority::open(
                    root.join("topics"),
                    std::sync::Arc::clone(&channel),
                )
                .map_err(|error| format!("--root topic authority: {error}"))?,
            );
            let waits = std::sync::Arc::new(
                nlos_wait::WaitAuthority::open(root.join("waits"), channel)
                    .map_err(|error| format!("--root wait authority: {error}"))?,
            );
            let notify = nlos_notify::NotificationService::open(
                root.join("notify"),
                std::sync::Arc::clone(&topics),
                waits,
            )
            .map_err(|error| format!("--root notification service: {error}"))?;
            (topics, notify)
        };
        Ok(Self {
            #[cfg(feature = "process")]
            process,
            #[cfg(feature = "resource")]
            resource,
            #[cfg(feature = "application")]
            application,
            #[cfg(feature = "search")]
            semantic,
            #[cfg(feature = "search")]
            search,
            #[cfg(feature = "notify")]
            notify_topics,
            #[cfg(feature = "notify")]
            notify,
        })
    }
}

#[cfg(any(unix, windows))]
fn composed_read_command(command: &ControlCommand) -> Option<(&'static str, &'static str)> {
    match command {
        ControlCommand::InspectProcess { .. } => Some(("inspect-process", "process")),
        ControlCommand::InspectResource { .. } => Some(("inspect-resource", "resource")),
        ControlCommand::InspectApplication { .. } => Some(("inspect-application", "application")),
        ControlCommand::InspectSearch { .. } => Some(("inspect-search", "search")),
        ControlCommand::InspectNotification { .. } => Some(("inspect-notification", "notify")),
        _ => None,
    }
}

#[cfg(any(unix, windows))]
fn split_root_flag(arguments: &[String]) -> Result<(Option<PathBuf>, &[String]), ControlError> {
    if arguments.first().map(String::as_str) != Some("--root") {
        return Ok((None, arguments));
    }
    let Some(value) = arguments.get(1) else {
        return Err(ControlError::InvalidCommand(
            "--root requires a <STATE_ROOT> value",
        ));
    };
    Ok((Some(PathBuf::from(value)), &arguments[2..]))
}

#[cfg(any(unix, windows))]
#[allow(clippy::too_many_lines)]
async fn run() -> Result<ExitCode, ControlError> {
    use nlos_system_control::control::dispatch_over_plain_endpoint_with_services;

    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (root, positional) = split_root_flag(&arguments).inspect_err(|_| eprintln!("{USAGE}"))?;
    let Some(endpoint) = positional.first().cloned() else {
        eprintln!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let command = parsed_command(&positional[1..]).inspect_err(|_| eprintln!("{USAGE}"))?;

    if let Some((command_name, feature)) = composed_read_command(&command) {
        #[allow(clippy::match_like_matches_macro)]
        let feature_enabled = match feature {
            "process" => cfg!(feature = "process"),
            "resource" => cfg!(feature = "resource"),
            "application" => cfg!(feature = "application"),
            "search" => cfg!(feature = "search"),
            "notify" => cfg!(feature = "notify"),
            _ => false,
        };
        if root.is_none() {
            eprintln!(
                "system-control-cli: {command_name} composes its projection client-side and \
                 needs --root <STATE_ROOT> pointing at the system-control daemon's state root; \
                 without it the read is structurally unwired, not merely empty"
            );
            eprintln!("{USAGE}");
            return Ok(ExitCode::from(2));
        }
        if !feature_enabled {
            eprintln!(
                "system-control-cli: this build lacks the `{feature}` inspector feature; \
                 rebuild with --features {feature} (or `daemon`) to use --root with {command_name}"
            );
            return Ok(ExitCode::from(2));
        }
    }
    #[cfg(any(
        feature = "process",
        feature = "resource",
        feature = "application",
        feature = "search",
        feature = "notify",
    ))]
    let root_state = match root.as_ref() {
        None => None,
        Some(path) => match RootState::open(path) {
            Ok(state) => Some(state),
            Err(error) => {
                eprintln!("system-control-cli: {error}");
                return Ok(ExitCode::from(2));
            }
        },
    };
    #[cfg(not(any(
        feature = "process",
        feature = "resource",
        feature = "application",
        feature = "search",
        feature = "notify",
    )))]
    let _ = root.as_ref();

    #[cfg(feature = "process")]
    let process_adapter = root_state.as_ref().map(|state| {
        nlos_system_control::process_inspector::ProcessAuthorityInspector::new(&state.process)
    });
    #[cfg(feature = "process")]
    let process: Option<&dyn ProcessInspector> = process_adapter
        .as_ref()
        .map(|inspector| inspector as &dyn ProcessInspector);
    #[cfg(not(feature = "process"))]
    let process: Option<&dyn ProcessInspector> = None;

    #[cfg(feature = "resource")]
    let resource_adapter = root_state.as_ref().map(|state| {
        nlos_system_control::resource_inspector::ResourceAuthorityInspector::new(&state.resource)
    });
    #[cfg(feature = "resource")]
    let resource: Option<&dyn ResourceInspector> = resource_adapter
        .as_ref()
        .map(|inspector| inspector as &dyn ResourceInspector);
    #[cfg(not(feature = "resource"))]
    let resource: Option<&dyn ResourceInspector> = None;

    #[cfg(feature = "application")]
    let application_adapter = root_state.as_ref().map(|state| {
        nlos_system_control::application_inspector::ApplicationAuthorityInspector::new(
            &state.application,
        )
    });
    #[cfg(feature = "application")]
    let application: Option<&dyn ApplicationInspector> = application_adapter
        .as_ref()
        .map(|inspector| inspector as &dyn ApplicationInspector);
    #[cfg(not(feature = "application"))]
    let application: Option<&dyn ApplicationInspector> = None;

    #[cfg(feature = "search")]
    let search_adapter = root_state.as_ref().map(|state| {
        nlos_system_control::search_inspector::SearchServiceInspector::new(
            &state.search,
            &state.semantic,
        )
    });
    #[cfg(feature = "search")]
    let search: Option<&dyn SearchInspector> = search_adapter
        .as_ref()
        .map(|inspector| inspector as &dyn SearchInspector);
    #[cfg(not(feature = "search"))]
    let search: Option<&dyn SearchInspector> = None;

    #[cfg(feature = "notify")]
    let notify_adapter = root_state.as_ref().map(|state| {
        nlos_system_control::notify_inspector::NotificationServiceInspector::new(
            &state.notify,
            &state.notify_topics,
        )
    });
    #[cfg(feature = "notify")]
    let notify: Option<&dyn NotificationInspector> = notify_adapter
        .as_ref()
        .map(|inspector| inspector as &dyn NotificationInspector);
    #[cfg(not(feature = "notify"))]
    let notify: Option<&dyn NotificationInspector> = None;

    let receipt = dispatch_over_plain_endpoint_with_services(
        &endpoint,
        &command,
        process,
        resource,
        application,
        search,
        notify,
    )
    .await?;
    println!("RECEIPT {}", receipt_to_hex(&receipt));
    println!("{}", summary(&receipt));
    if receipt.outcome.is_ok() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

#[cfg(any(unix, windows))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("system-control-cli: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn main() -> ExitCode {
    eprintln!("system-control-cli: local IPC dispatch requires a Unix or Windows host");
    ExitCode::from(2)
}
