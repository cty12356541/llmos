//! Stage D (`IPC-D1..D8` & `[UI-PARITY-001]`) restricted-grammar natural
//! language compiler extensions.

#![allow(clippy::missing_errors_doc)]

use nlos_schema::SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL;
use nlos_schema::sabi::v1::{
    ControlScope, PlanNodeLifecycleState, SettingsDomainKind, TaskUndoClass,
};

use crate::control::{
    ControlCommand, ControlError, ScopedControlAction, parse_hex_32, parse_hex_id,
};
use crate::desktop_enum::preview_action;
use crate::nl::{
    NL_CANCEL_REASON, NL_DISABLE_REASON, NL_KILL_REASON, NL_PAUSE_REASON, NL_RECLAIM_REASON,
    NL_RESUME_REASON, NL_THROTTLE_REASON, NL_UNINSTALL_REASON,
};

pub const NL_MIGRATE_REASON: &str =
    "process migration requested through the restricted natural-language control prefix";
pub const NL_PURGE_TOPIC_REASON: &str =
    "topic purged through the restricted natural-language control prefix";
pub const NL_UPDATE_SETTINGS_REASON: &str =
    "settings domain updated through the restricted natural-language control prefix";
pub const NL_SET_ASSOCIATION_REASON: &str =
    "content-type association set through the restricted natural-language control prefix";
pub const NL_ISSUE_CHOOSER_REASON: &str =
    "chooser grant issued through the restricted natural-language control prefix";
pub const NL_UNDO_RECEIPT_REASON: &str =
    "receipt undo requested through the restricted natural-language control prefix";
pub const NL_MUTATE_SEARCH_REASON: &str =
    "search candidate mutation requested through the restricted natural-language control prefix";
pub const NL_MUTATE_TOPOLOGY_REASON: &str =
    "task node topology mutation requested through the restricted natural-language control prefix";
pub const NL_OVERRIDE_ATTEMPT_REASON: &str =
    "task attempt commit override requested through the restricted natural-language control prefix";
pub const NL_OVERRIDE_OPERATION_REASON: &str = "effect-unknown operation override requested through the restricted natural-language control prefix";

/// Parses an explicit non-unspecified [`ControlScope`] token (EN snake/kebab/proto or ZH).
pub fn parse_control_scope(token: &str) -> Result<ControlScope, ControlError> {
    if token.eq_ignore_ascii_case("system")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_SYSTEM")
        || token == "系统"
    {
        Ok(ControlScope::System)
    } else if token.eq_ignore_ascii_case("operation")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_OPERATION")
        || token == "操作"
    {
        Ok(ControlScope::Operation)
    } else if token.eq_ignore_ascii_case("workspace")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_WORKSPACE")
        || token == "工作区"
    {
        Ok(ControlScope::Workspace)
    } else if token.eq_ignore_ascii_case("application")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_APPLICATION")
        || token == "应用"
    {
        Ok(ControlScope::Application)
    } else if token.eq_ignore_ascii_case("task")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_TASK")
        || token == "任务"
    {
        Ok(ControlScope::Task)
    } else if token.eq_ignore_ascii_case("task_group")
        || token.eq_ignore_ascii_case("task-group")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_TASK_GROUP")
        || token == "任务组"
    {
        Ok(ControlScope::TaskGroup)
    } else if token.eq_ignore_ascii_case("task_node")
        || token.eq_ignore_ascii_case("task-node")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_TASK_NODE")
        || token == "任务节点"
    {
        Ok(ControlScope::TaskNode)
    } else if token.eq_ignore_ascii_case("task_attempt")
        || token.eq_ignore_ascii_case("task-attempt")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_TASK_ATTEMPT")
        || token == "任务尝试"
    {
        Ok(ControlScope::TaskAttempt)
    } else if token.eq_ignore_ascii_case("process")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_PROCESS")
        || token == "进程"
    {
        Ok(ControlScope::Process)
    } else if token.eq_ignore_ascii_case("agent_instance")
        || token.eq_ignore_ascii_case("agent-instance")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_AGENT_INSTANCE")
        || token == "智能体实例"
    {
        Ok(ControlScope::AgentInstance)
    } else if token.eq_ignore_ascii_case("execution_fiber")
        || token.eq_ignore_ascii_case("execution-fiber")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_EXECUTION_FIBER")
        || token == "纤程"
    {
        Ok(ControlScope::ExecutionFiber)
    } else if token.eq_ignore_ascii_case("resource_group")
        || token.eq_ignore_ascii_case("resource-group")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_RESOURCE_GROUP")
        || token == "资源组"
    {
        Ok(ControlScope::ResourceGroup)
    } else if token.eq_ignore_ascii_case("context")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_CONTEXT")
        || token == "上下文"
    {
        Ok(ControlScope::Context)
    } else if token.eq_ignore_ascii_case("topic")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_TOPIC")
        || token == "主题"
    {
        Ok(ControlScope::Topic)
    } else if token.eq_ignore_ascii_case("channel")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_CHANNEL")
        || token == "通道"
    {
        Ok(ControlScope::Channel)
    } else if token.eq_ignore_ascii_case("device")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_DEVICE")
        || token == "设备"
    {
        Ok(ControlScope::Device)
    } else if token.eq_ignore_ascii_case("cell")
        || token.eq_ignore_ascii_case("CONTROL_SCOPE_CELL")
        || token == "单元"
    {
        Ok(ControlScope::Cell)
    } else {
        Err(ControlError::InvalidCommand(
            "unknown or unspecified control scope",
        ))
    }
}

/// Parses a [`SettingsDomainKind`] token (EN or ZH).
pub fn parse_settings_domain_kind(token: &str) -> Result<SettingsDomainKind, ControlError> {
    if token.eq_ignore_ascii_case("system") || token == "系统" {
        Ok(SettingsDomainKind::System)
    } else if token.eq_ignore_ascii_case("user") || token == "用户" {
        Ok(SettingsDomainKind::User)
    } else if token.eq_ignore_ascii_case("workspace") || token == "工作区" {
        Ok(SettingsDomainKind::Workspace)
    } else if token.eq_ignore_ascii_case("application") || token == "应用" {
        Ok(SettingsDomainKind::Application)
    } else {
        Err(ControlError::InvalidCommand("unknown settings domain kind"))
    }
}

/// Parses a [`TaskUndoClass`] token (EN or ZH).
pub fn parse_undo_class(token: &str) -> Result<TaskUndoClass, ControlError> {
    if token.eq_ignore_ascii_case("read_only")
        || token.eq_ignore_ascii_case("read-only")
        || token == "只读"
    {
        Ok(TaskUndoClass::ReadOnly)
    } else if token.eq_ignore_ascii_case("reversible") || token == "可逆" {
        Ok(TaskUndoClass::Reversible)
    } else if token.eq_ignore_ascii_case("compensatable") || token == "可补偿" {
        Ok(TaskUndoClass::Compensatable)
    } else if token.eq_ignore_ascii_case("irreversible") || token == "不可逆" {
        Ok(TaskUndoClass::Irreversible)
    } else {
        Err(ControlError::InvalidCommand("unknown task undo class"))
    }
}

/// Parses a [`PlanNodeLifecycleState`] token.
pub fn parse_plan_node_state(token: &str) -> Result<PlanNodeLifecycleState, ControlError> {
    if token.eq_ignore_ascii_case("declared") {
        Ok(PlanNodeLifecycleState::Declared)
    } else if token.eq_ignore_ascii_case("eligible") {
        Ok(PlanNodeLifecycleState::Eligible)
    } else if token.eq_ignore_ascii_case("active") {
        Ok(PlanNodeLifecycleState::Active)
    } else if token.eq_ignore_ascii_case("completed") {
        Ok(PlanNodeLifecycleState::Completed)
    } else if token.eq_ignore_ascii_case("failed") {
        Ok(PlanNodeLifecycleState::Failed)
    } else if token.eq_ignore_ascii_case("cancelled") {
        Ok(PlanNodeLifecycleState::Cancelled)
    } else {
        Err(ControlError::InvalidCommand("unknown plan node state"))
    }
}

/// Parses a preview action verb (EN snake/kebab or ZH) into a canonical `preview_action` code.
pub fn parse_preview_action_code(verb: &str) -> Result<u32, ControlError> {
    if verb.eq_ignore_ascii_case("pause") || verb == "暂停" {
        Ok(preview_action::PAUSE)
    } else if verb.eq_ignore_ascii_case("resume") || verb == "恢复" {
        Ok(preview_action::RESUME)
    } else if verb.eq_ignore_ascii_case("cancel") || verb == "取消" {
        Ok(preview_action::CANCEL)
    } else if verb.eq_ignore_ascii_case("kill") || verb == "终止" {
        Ok(preview_action::KILL)
    } else if verb.eq_ignore_ascii_case("throttle") || verb == "限流" {
        Ok(preview_action::THROTTLE)
    } else if verb.eq_ignore_ascii_case("reclaim") || verb == "回收" {
        Ok(preview_action::RECLAIM)
    } else if verb.eq_ignore_ascii_case("disable")
        || verb.eq_ignore_ascii_case("disable-application")
        || verb.eq_ignore_ascii_case("disable_application")
        || verb == "禁用"
    {
        Ok(preview_action::DISABLE_APPLICATION)
    } else if verb.eq_ignore_ascii_case("uninstall")
        || verb.eq_ignore_ascii_case("uninstall-application")
        || verb.eq_ignore_ascii_case("uninstall_application")
        || verb == "卸载"
    {
        Ok(preview_action::UNINSTALL_APPLICATION)
    } else if verb.eq_ignore_ascii_case("migrate")
        || verb.eq_ignore_ascii_case("migrate-process")
        || verb.eq_ignore_ascii_case("migrate_process")
        || verb == "迁移"
    {
        Ok(preview_action::MIGRATE_PROCESS)
    } else if verb.eq_ignore_ascii_case("purge")
        || verb.eq_ignore_ascii_case("purge-topic")
        || verb.eq_ignore_ascii_case("purge_topic")
        || verb == "清理"
    {
        Ok(preview_action::PURGE_TOPIC)
    } else if verb.eq_ignore_ascii_case("mutate_search_lock")
        || verb.eq_ignore_ascii_case("lock-mutate-search-candidate")
    {
        Ok(preview_action::MUTATE_SEARCH_CANDIDATE_LOCK)
    } else if verb.eq_ignore_ascii_case("undo_irreversible_lock")
        || verb.eq_ignore_ascii_case("lock-undo-irreversible")
    {
        Ok(preview_action::UNDO_IRREVERSIBLE_RECEIPT_LOCK)
    } else if verb.eq_ignore_ascii_case("mutate_node_lock")
        || verb.eq_ignore_ascii_case("lock-mutate-node-topology")
    {
        Ok(preview_action::MUTATE_COMPLETED_OR_ACTIVE_NODE_LOCK)
    } else if verb.eq_ignore_ascii_case("override_attempt_lock")
        || verb.eq_ignore_ascii_case("lock-override-attempt-commit")
    {
        Ok(preview_action::OVERRIDE_TASK_ATTEMPT_COMMIT_LOCK)
    } else if verb.eq_ignore_ascii_case("override_operation_lock")
        || verb.eq_ignore_ascii_case("lock-override-unknown-operation")
    {
        Ok(preview_action::OVERRIDE_EFFECT_UNKNOWN_OPERATION_LOCK)
    } else {
        Err(ControlError::InvalidCommand("unknown preview action verb"))
    }
}

/// Parses a non-throttle [`ScopedControlAction`] verb (EN or ZH) and returns
/// its canonical NL audit reason.
pub fn parse_unit_scoped_action(
    verb: &str,
) -> Result<(ScopedControlAction, &'static str), ControlError> {
    if verb.eq_ignore_ascii_case("pause")
        || verb.eq_ignore_ascii_case("halt")
        || verb.eq_ignore_ascii_case("suspend")
        || verb == "暂停"
    {
        Ok((ScopedControlAction::Pause, NL_PAUSE_REASON))
    } else if verb.eq_ignore_ascii_case("resume") || verb == "恢复" {
        Ok((ScopedControlAction::Resume, NL_RESUME_REASON))
    } else if verb.eq_ignore_ascii_case("cancel")
        || verb.eq_ignore_ascii_case("abort")
        || verb == "取消"
    {
        Ok((ScopedControlAction::Cancel, NL_CANCEL_REASON))
    } else if verb.eq_ignore_ascii_case("kill")
        || verb.eq_ignore_ascii_case("terminate")
        || verb == "终止"
    {
        Ok((ScopedControlAction::Kill, NL_KILL_REASON))
    } else if verb.eq_ignore_ascii_case("reclaim") || verb == "回收" {
        Ok((ScopedControlAction::Reclaim, NL_RECLAIM_REASON))
    } else if verb.eq_ignore_ascii_case("disable")
        || verb.eq_ignore_ascii_case("disable-application")
        || verb.eq_ignore_ascii_case("disable_application")
        || verb == "禁用"
    {
        Ok((ScopedControlAction::DisableApplication, NL_DISABLE_REASON))
    } else if verb.eq_ignore_ascii_case("uninstall")
        || verb.eq_ignore_ascii_case("uninstall-application")
        || verb.eq_ignore_ascii_case("uninstall_application")
        || verb == "卸载"
    {
        Ok((
            ScopedControlAction::UninstallApplication,
            NL_UNINSTALL_REASON,
        ))
    } else {
        Err(ControlError::InvalidCommand("unknown scoped control verb"))
    }
}

fn parse_u32_limit(token: &str) -> Result<u32, ControlError> {
    if !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ControlError::InvalidCommand(
            "pagination limit must be a plain decimal count (digits only)",
        ));
    }
    token
        .parse::<u32>()
        .map_err(|_| ControlError::InvalidCommand("pagination limit exceeds u32 bound"))
}

fn parse_u64_plain(token: &str) -> Result<u64, ControlError> {
    if !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ControlError::InvalidCommand(
            "numeric parameter must be a plain decimal count (digits only)",
        ));
    }
    token
        .parse::<u64>()
        .map_err(|_| ControlError::InvalidCommand("numeric parameter exceeds 64-bit bound"))
}

fn parse_percent_plain(token: &str) -> Result<u64, ControlError> {
    let pct = parse_u64_plain(token)?;
    if !(1..=100).contains(&pct) {
        return Err(ControlError::InvalidCommand(
            "throttle percent must be a whole percent from 1 to 100",
        ));
    }
    Ok(pct)
}

fn parse_opt_hex(token: &str) -> Result<Option<[u8; 16]>, ControlError> {
    if token.eq_ignore_ascii_case("all") || token.eq_ignore_ascii_case("none") || token == "全部"
    {
        Ok(None)
    } else {
        parse_hex_id(token).map(Some)
    }
}

fn parse_limit_and_hex_cursor(tail: &[&str]) -> Result<(u32, Option<[u8; 16]>), ControlError> {
    match tail {
        [kw, limit] if kw.eq_ignore_ascii_case("limit") || *kw == "限制" => {
            Ok((parse_u32_limit(limit)?, None))
        }
        [kw, limit, after_kw, cursor_hex]
            if (kw.eq_ignore_ascii_case("limit") || *kw == "限制")
                && (after_kw.eq_ignore_ascii_case("after") || *after_kw == "游标") =>
        {
            Ok((parse_u32_limit(limit)?, Some(parse_hex_id(cursor_hex)?)))
        }
        _ => Err(ControlError::InvalidCommand(
            "expected \"limit <count> [after <32-hex>]\"",
        )),
    }
}

fn is_inspect_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("inspect")
        || token.eq_ignore_ascii_case("check")
        || token.eq_ignore_ascii_case("show")
        || token.eq_ignore_ascii_case("status")
        || token.eq_ignore_ascii_case("get")
}

#[allow(clippy::too_many_lines)]
pub(crate) fn try_parse_stage_d(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    if let Some(res) = try_parse_stage_d_reads(tokens) {
        return Some(res);
    }
    try_parse_stage_d_mutations(tokens)
}

#[allow(clippy::too_many_lines)]
fn try_parse_stage_d_reads(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        // --- EN `list ...` ---
        [head, second, tail @ ..] if head.eq_ignore_ascii_case("list") => {
            if second.eq_ignore_ascii_case("tasks") {
                return Some(
                    parse_limit_and_hex_cursor(tail)
                        .map(|(limit, cursor)| ControlCommand::ListTasks { cursor, limit }),
                );
            }
            if second.eq_ignore_ascii_case("task") {
                return match tail {
                    [third, filter_tok, rest @ ..]
                        if third.eq_ignore_ascii_case("groups")
                            && !filter_tok.eq_ignore_ascii_case("limit") =>
                    {
                        Some(parse_opt_hex(filter_tok).and_then(|task_id| {
                            parse_limit_and_hex_cursor(rest).map(|(limit, cursor)| {
                                ControlCommand::ListTaskGroups {
                                    task_id,
                                    cursor,
                                    limit,
                                }
                            })
                        }))
                    }
                    [third, rest @ ..] if third.eq_ignore_ascii_case("groups") => {
                        Some(parse_limit_and_hex_cursor(rest).map(|(limit, cursor)| {
                            ControlCommand::ListTaskGroups {
                                task_id: None,
                                cursor,
                                limit,
                            }
                        }))
                    }
                    [third, plan_hex, rev_kw, rev_tok, rest @ ..]
                        if third.eq_ignore_ascii_case("nodes")
                            && rev_kw.eq_ignore_ascii_case("revision") =>
                    {
                        Some(
                            parse_hex_id(plan_hex)
                                .and_then(|plan_id| {
                                    parse_u64_plain(rev_tok).map(|rev| (plan_id, rev))
                                })
                                .and_then(|(plan_id, expected_plan_revision)| {
                                    parse_limit_and_hex_cursor(rest).map(|(limit, cursor)| {
                                        ControlCommand::ListTaskNodes {
                                            plan_id,
                                            expected_plan_revision,
                                            cursor,
                                            limit,
                                        }
                                    })
                                }),
                        )
                    }
                    [third, filter_tok, gen_kw, gen_tok, rest @ ..]
                        if third.eq_ignore_ascii_case("attempts")
                            && gen_kw.eq_ignore_ascii_case("generation") =>
                    {
                        Some(
                            parse_opt_hex(filter_tok)
                                .and_then(|scope_filter_id| {
                                    parse_u64_plain(gen_tok).map(|expected_generation| {
                                        (scope_filter_id, expected_generation)
                                    })
                                })
                                .and_then(|(scope_filter_id, expected_generation)| {
                                    parse_limit_and_hex_cursor(rest).map(|(limit, cursor)| {
                                        ControlCommand::ListTaskAttempts {
                                            scope_filter_id,
                                            expected_generation,
                                            cursor,
                                            limit,
                                        }
                                    })
                                }),
                        )
                    }
                    _ => Some(Err(ControlError::InvalidCommand(
                        "\"list task\" expects \"groups\", \"nodes <plan-32-hex> revision <u64>\", or \"attempts <all|32-hex> generation <u64>\"",
                    ))),
                };
            }
            if second.eq_ignore_ascii_case("fibers") {
                return Some(match tail {
                    [proc_tok, gen_kw, gen_tok, rest @ ..]
                        if gen_kw.eq_ignore_ascii_case("generation") =>
                    {
                        parse_opt_hex(proc_tok)
                            .and_then(|process_id| {
                                parse_u64_plain(gen_tok)
                                    .map(|expected_generation| (process_id, expected_generation))
                            })
                            .and_then(|(process_id, expected_generation)| {
                                parse_limit_and_hex_cursor(rest).map(|(limit, cursor)| {
                                    ControlCommand::ListExecutionFibers {
                                        process_id,
                                        expected_generation,
                                        cursor,
                                        limit,
                                    }
                                })
                            })
                    }
                    _ => Err(ControlError::InvalidCommand(
                        "\"list fibers\" expects \"<all|32-hex> generation <u64> limit <count>\"",
                    )),
                });
            }
            if second.eq_ignore_ascii_case("operations") {
                return Some(match tail {
                    [fiber_tok, gen_kw, gen_tok, rest @ ..]
                        if gen_kw.eq_ignore_ascii_case("generation") =>
                    {
                        parse_opt_hex(fiber_tok)
                            .and_then(|fiber_id| {
                                parse_u64_plain(gen_tok)
                                    .map(|expected_generation| (fiber_id, expected_generation))
                            })
                            .and_then(|(fiber_id, expected_generation)| {
                                parse_limit_and_hex_cursor(rest).map(|(limit, cursor)| {
                                    ControlCommand::ListOperations {
                                        fiber_id,
                                        expected_generation,
                                        cursor,
                                        limit,
                                    }
                                })
                            })
                    }
                    _ => Err(ControlError::InvalidCommand(
                        "\"list operations\" expects \"<all|32-hex> generation <u64> limit <count>\"",
                    )),
                });
            }
            if second.eq_ignore_ascii_case("applications") {
                return Some(parse_limit_and_hex_cursor(tail).map(|(limit, cursor)| {
                    ControlCommand::ListInstalledApplications { cursor, limit }
                }));
            }
            if second.eq_ignore_ascii_case("content") {
                return Some(match tail {
                    [third, rest @ ..] if third.eq_ignore_ascii_case("associations") => {
                        parse_limit_and_hex_cursor(rest).map(|(limit, cursor)| {
                            ControlCommand::ListContentTypeAssociations { cursor, limit }
                        })
                    }
                    _ => Err(ControlError::InvalidCommand(
                        "\"list content\" expects \"associations limit <count>\"",
                    )),
                });
            }
            if second.eq_ignore_ascii_case("keychain") {
                return Some(match tail {
                    [third, rest @ ..] if third.eq_ignore_ascii_case("references") => {
                        parse_limit_and_hex_cursor(rest).map(|(limit, cursor)| {
                            ControlCommand::ListKeychainReferences { cursor, limit }
                        })
                    }
                    _ => Err(ControlError::InvalidCommand(
                        "\"list keychain\" expects \"references limit <count>\"",
                    )),
                });
            }
            if second.eq_ignore_ascii_case("chooser") {
                return Some(match tail {
                    [third, rest @ ..] if third.eq_ignore_ascii_case("grants") => {
                        parse_limit_and_hex_cursor(rest).map(|(limit, cursor)| {
                            ControlCommand::ListChooserGrants { cursor, limit }
                        })
                    }
                    _ => Err(ControlError::InvalidCommand(
                        "\"list chooser\" expects \"grants limit <count>\"",
                    )),
                });
            }
            if second.eq_ignore_ascii_case("workspaces") {
                return Some(
                    parse_limit_and_hex_cursor(tail)
                        .map(|(limit, cursor)| ControlCommand::ListWorkspaces { cursor, limit }),
                );
            }
            if second.eq_ignore_ascii_case("processes") {
                return Some(
                    parse_limit_and_hex_cursor(tail)
                        .map(|(limit, cursor)| ControlCommand::ListProcesses { cursor, limit }),
                );
            }
            if second.eq_ignore_ascii_case("resources") {
                return Some(parse_limit_and_hex_cursor(tail).map(|(limit, cursor)| {
                    ControlCommand::ListResourceReservations { cursor, limit }
                }));
            }
            if second.eq_ignore_ascii_case("topics") {
                return Some(
                    parse_limit_and_hex_cursor(tail)
                        .map(|(limit, cursor)| ControlCommand::ListTopics { cursor, limit }),
                );
            }
            if second.eq_ignore_ascii_case("notifications") {
                return Some(
                    parse_limit_and_hex_cursor(tail)
                        .map(|(limit, cursor)| ControlCommand::ListNotifications { cursor, limit }),
                );
            }
            if second.eq_ignore_ascii_case("cells") {
                return Some(
                    parse_limit_and_hex_cursor(tail)
                        .map(|(limit, cursor)| ControlCommand::ListCells { cursor, limit }),
                );
            }
            Some(Err(ControlError::InvalidCommand("unknown \"list\" target")))
        }
        [head] if head.eq_ignore_ascii_case("list") => Some(Err(ControlError::InvalidCommand(
            "\"list\" requires a domain target and \"limit <count>\"",
        ))),

        // --- ZH `列出...` ---
        ["列出任务", tail @ ..] | ["列出", "任务", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListTasks { cursor, limit }),
        ),
        ["列出任务组", tail @ ..] | ["列出", "任务组", tail @ ..] => {
            Some(parse_limit_and_hex_cursor(tail).map(|(limit, cursor)| {
                ControlCommand::ListTaskGroups {
                    task_id: None,
                    cursor,
                    limit,
                }
            }))
        }
        ["列出任务节点", plan_hex, "版本", rev_tok, tail @ ..]
        | ["列出", "任务节点", plan_hex, "版本", rev_tok, tail @ ..] => Some(
            parse_hex_id(plan_hex)
                .and_then(|plan_id| parse_u64_plain(rev_tok).map(|rev| (plan_id, rev)))
                .and_then(|(plan_id, expected_plan_revision)| {
                    parse_limit_and_hex_cursor(tail).map(|(limit, cursor)| {
                        ControlCommand::ListTaskNodes {
                            plan_id,
                            expected_plan_revision,
                            cursor,
                            limit,
                        }
                    })
                }),
        ),
        ["列出任务尝试", filter_tok, "世代", gen_tok, tail @ ..]
        | ["列出", "任务尝试", filter_tok, "世代", gen_tok, tail @ ..] => Some(
            parse_opt_hex(filter_tok)
                .and_then(|scope_filter_id| {
                    parse_u64_plain(gen_tok).map(|generation| (scope_filter_id, generation))
                })
                .and_then(|(scope_filter_id, expected_generation)| {
                    parse_limit_and_hex_cursor(tail).map(|(limit, cursor)| {
                        ControlCommand::ListTaskAttempts {
                            scope_filter_id,
                            expected_generation,
                            cursor,
                            limit,
                        }
                    })
                }),
        ),
        ["列出纤程", proc_tok, "世代", gen_tok, tail @ ..]
        | ["列出", "纤程", proc_tok, "世代", gen_tok, tail @ ..] => Some(
            parse_opt_hex(proc_tok)
                .and_then(|process_id| {
                    parse_u64_plain(gen_tok).map(|generation| (process_id, generation))
                })
                .and_then(|(process_id, expected_generation)| {
                    parse_limit_and_hex_cursor(tail).map(|(limit, cursor)| {
                        ControlCommand::ListExecutionFibers {
                            process_id,
                            expected_generation,
                            cursor,
                            limit,
                        }
                    })
                }),
        ),
        ["列出操作", fiber_tok, "世代", gen_tok, tail @ ..]
        | ["列出", "操作", fiber_tok, "世代", gen_tok, tail @ ..] => Some(
            parse_opt_hex(fiber_tok)
                .and_then(|fiber_id| {
                    parse_u64_plain(gen_tok).map(|generation| (fiber_id, generation))
                })
                .and_then(|(fiber_id, expected_generation)| {
                    parse_limit_and_hex_cursor(tail).map(|(limit, cursor)| {
                        ControlCommand::ListOperations {
                            fiber_id,
                            expected_generation,
                            cursor,
                            limit,
                        }
                    })
                }),
        ),
        ["列出应用", tail @ ..] | ["列出", "应用", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListInstalledApplications { cursor, limit }),
        ),
        ["列出内容关联", tail @ ..] | ["列出", "内容关联", tail @ ..] => {
            Some(parse_limit_and_hex_cursor(tail).map(|(limit, cursor)| {
                ControlCommand::ListContentTypeAssociations { cursor, limit }
            }))
        }
        ["列出密钥引用", tail @ ..] | ["列出", "密钥引用", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListKeychainReferences { cursor, limit }),
        ),
        ["列出选择器授权", tail @ ..] | ["列出", "选择器授权", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListChooserGrants { cursor, limit }),
        ),
        ["列出工作区", tail @ ..] | ["列出", "工作区", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListWorkspaces { cursor, limit }),
        ),
        ["列出进程", tail @ ..] | ["列出", "进程", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListProcesses { cursor, limit }),
        ),
        ["列出资源", tail @ ..] | ["列出", "资源", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListResourceReservations { cursor, limit }),
        ),
        ["列出主题", tail @ ..] | ["列出", "主题", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListTopics { cursor, limit }),
        ),
        ["列出通知", tail @ ..] | ["列出", "通知", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListNotifications { cursor, limit }),
        ),
        ["列出单元", tail @ ..] | ["列出", "单元", tail @ ..] => Some(
            parse_limit_and_hex_cursor(tail)
                .map(|(limit, cursor)| ControlCommand::ListCells { cursor, limit }),
        ),
        ["列出", ..] => Some(Err(ControlError::InvalidCommand("未知的\"列出\"域或参数"))),

        // --- Query Search Candidates & Command Status ---
        [
            head,
            second,
            scope_hex,
            query_text,
            offset_kw,
            offset_tok,
            lim_kw,
            lim_tok,
        ] if head.eq_ignore_ascii_case("query")
            && second.eq_ignore_ascii_case("search")
            && offset_kw.eq_ignore_ascii_case("offset")
            && lim_kw.eq_ignore_ascii_case("limit") =>
        {
            Some(
                parse_hex_32(scope_hex)
                    .and_then(|scope_id| {
                        parse_u64_plain(offset_tok).map(|cursor_offset| (scope_id, cursor_offset))
                    })
                    .and_then(|(scope_id, cursor_offset)| {
                        parse_u32_limit(lim_tok).map(|limit| {
                            ControlCommand::QuerySearchCandidates {
                                scope_id,
                                query: (*query_text).to_owned(),
                                cursor_offset,
                                limit,
                            }
                        })
                    }),
            )
        }
        [
            "查询搜索",
            scope_hex,
            query_text,
            "偏移",
            offset_tok,
            "限制",
            lim_tok,
        ]
        | [
            "查询",
            "搜索",
            scope_hex,
            query_text,
            "偏移",
            offset_tok,
            "限制",
            lim_tok,
        ] => Some(
            parse_hex_32(scope_hex)
                .and_then(|scope_id| {
                    parse_u64_plain(offset_tok).map(|cursor_offset| (scope_id, cursor_offset))
                })
                .and_then(|(scope_id, cursor_offset)| {
                    parse_u32_limit(lim_tok).map(|limit| ControlCommand::QuerySearchCandidates {
                        scope_id,
                        query: (*query_text).to_owned(),
                        cursor_offset,
                        limit,
                    })
                }),
        ),
        [head, second, third, cmd_hex]
            if (head.eq_ignore_ascii_case("query") || is_inspect_verb(head))
                && second.eq_ignore_ascii_case("command")
                && third.eq_ignore_ascii_case("status") =>
        {
            Some(parse_hex_id(cmd_hex).map(|target_command_id| {
                ControlCommand::QueryControlCommandStatus { target_command_id }
            }))
        }
        ["查看命令状态" | "查询命令状态", cmd_hex] | ["查看" | "查询", "命令状态", cmd_hex] => {
            Some(parse_hex_id(cmd_hex).map(|target_command_id| {
                ControlCommand::QueryControlCommandStatus { target_command_id }
            }))
        }
        [head, ..] if head.eq_ignore_ascii_case("query") || *head == "查询" => Some(Err(
            ControlError::InvalidCommand("\"query\" expects \"search\" or \"command status\""),
        )),

        // --- Poll Watermark Events ---
        [head, second, third, since_wm, fourth, limit]
            if head.eq_ignore_ascii_case("poll")
                && second.eq_ignore_ascii_case("watermark")
                && third.eq_ignore_ascii_case("since")
                && fourth.eq_ignore_ascii_case("limit") =>
        {
            Some(
                parse_u64_plain(since_wm)
                    .and_then(|wm| parse_u32_limit(limit).map(|lim| (wm, lim)))
                    .map(
                        |(since_watermark, limit)| ControlCommand::PollDesktopWatermarkEvents {
                            since_watermark,
                            limit,
                        },
                    ),
            )
        }
        ["轮询水位", "自", since_wm, "限制", limit]
        | ["轮询", "水位", "自", since_wm, "限制", limit] => Some(
            parse_u64_plain(since_wm)
                .and_then(|wm| parse_u32_limit(limit).map(|lim| (wm, lim)))
                .map(
                    |(since_watermark, limit)| ControlCommand::PollDesktopWatermarkEvents {
                        since_watermark,
                        limit,
                    },
                ),
        ),
        [head, ..] if head.eq_ignore_ascii_case("poll") || *head == "轮询" => Some(Err(
            ControlError::InvalidCommand("\"poll watermark\" expects \"since <u64> limit <u32>\""),
        )),

        // --- Inspect Settings / Workspace / Cell Leases ---
        [head, second, kind_tok, target_tok, lim_kw, lim_tok]
            if is_inspect_verb(head)
                && second.eq_ignore_ascii_case("settings")
                && lim_kw.eq_ignore_ascii_case("limit") =>
        {
            Some(
                parse_settings_domain_kind(kind_tok)
                    .and_then(|domain_kind| {
                        parse_opt_hex(target_tok)
                            .map(|scope_target_id| (domain_kind, scope_target_id))
                    })
                    .and_then(|(domain_kind, scope_target_id)| {
                        parse_u32_limit(lim_tok).map(|limit| {
                            ControlCommand::InspectSettingsDomain {
                                domain_kind,
                                scope_target_id,
                                limit,
                            }
                        })
                    }),
            )
        }
        [
            "查看设置" | "检查设置",
            kind_tok,
            target_tok,
            "限制",
            lim_tok,
        ]
        | [
            "查看" | "检查",
            "设置",
            kind_tok,
            target_tok,
            "限制",
            lim_tok,
        ] => Some(
            parse_settings_domain_kind(kind_tok)
                .and_then(|domain_kind| {
                    parse_opt_hex(target_tok).map(|scope_target_id| (domain_kind, scope_target_id))
                })
                .and_then(|(domain_kind, scope_target_id)| {
                    parse_u32_limit(lim_tok).map(|limit| ControlCommand::InspectSettingsDomain {
                        domain_kind,
                        scope_target_id,
                        limit,
                    })
                }),
        ),
        [head, second, ws_hex, rev_kw, rev_tok]
            if is_inspect_verb(head)
                && second.eq_ignore_ascii_case("workspace")
                && rev_kw.eq_ignore_ascii_case("revision") =>
        {
            Some(
                parse_hex_id(ws_hex)
                    .and_then(|workspace_id| {
                        parse_u64_plain(rev_tok)
                            .map(|expected_revision| (workspace_id, expected_revision))
                    })
                    .map(
                        |(workspace_id, expected_revision)| ControlCommand::InspectWorkspace {
                            workspace_id,
                            expected_revision,
                        },
                    ),
            )
        }
        ["查看工作区" | "检查工作区", ws_hex, "版本", rev_tok]
        | ["查看" | "检查", "工作区", ws_hex, "版本", rev_tok] => Some(
            parse_hex_id(ws_hex)
                .and_then(|workspace_id| {
                    parse_u64_plain(rev_tok)
                        .map(|expected_revision| (workspace_id, expected_revision))
                })
                .map(
                    |(workspace_id, expected_revision)| ControlCommand::InspectWorkspace {
                        workspace_id,
                        expected_revision,
                    },
                ),
        ),
        [head, second, third, cell_tok, lim_kw, lim_tok]
            if is_inspect_verb(head)
                && second.eq_ignore_ascii_case("cell")
                && third.eq_ignore_ascii_case("leases")
                && lim_kw.eq_ignore_ascii_case("limit") =>
        {
            Some(parse_opt_hex(cell_tok).and_then(|cell_id| {
                parse_u32_limit(lim_tok)
                    .map(|limit| ControlCommand::InspectCellLeases { cell_id, limit })
            }))
        }
        ["查看单元租约" | "检查单元租约", cell_tok, "限制", lim_tok]
        | ["查看" | "检查", "单元租约", cell_tok, "限制", lim_tok] => {
            Some(parse_opt_hex(cell_tok).and_then(|cell_id| {
                parse_u32_limit(lim_tok)
                    .map(|limit| ControlCommand::InspectCellLeases { cell_id, limit })
            }))
        }

        // --- Preview Blast Radius ---
        [head, verb, scope_kw, scope_tok, target_hex, lim_kw, lim_tok]
            if (head.eq_ignore_ascii_case("preview") || *head == "预览")
                && (scope_kw.eq_ignore_ascii_case("scope") || *scope_kw == "作用域")
                && (lim_kw.eq_ignore_ascii_case("limit") || *lim_kw == "限制") =>
        {
            Some(
                parse_preview_action_code(verb)
                    .and_then(|preview_action| {
                        parse_control_scope(scope_tok)
                            .map(|target_scope| (preview_action, target_scope))
                    })
                    .and_then(|(preview_action, target_scope)| {
                        parse_hex_id(target_hex)
                            .map(|target_id| (preview_action, target_scope, target_id))
                    })
                    .and_then(|(preview_action, target_scope, target_id)| {
                        parse_u32_limit(lim_tok).map(|limit| {
                            ControlCommand::PreviewControlBlastRadius {
                                target_scope,
                                target_id,
                                secondary_id: None,
                                preview_action,
                                limit,
                            }
                        })
                    }),
            )
        }
        [head, ..] if head.eq_ignore_ascii_case("preview") || *head == "预览" => {
            Some(Err(ControlError::InvalidCommand(
                "\"preview\" expects \"<verb> scope <scope> <32-hex> limit <count>\"",
            )))
        }

        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
fn try_parse_stage_d_mutations(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        // --- Explicit ControlScope throttle ---
        [
            verb,
            scope_kw,
            scope_tok,
            target_hex,
            to_kw,
            pct_tok,
            pct_kw,
            exp_kw,
            cas_tok,
        ] if verb.eq_ignore_ascii_case("throttle")
            && scope_kw.eq_ignore_ascii_case("scope")
            && to_kw.eq_ignore_ascii_case("to")
            && pct_kw.eq_ignore_ascii_case("percent")
            && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(
                parse_control_scope(scope_tok)
                    .and_then(|scope| parse_hex_id(target_hex).map(|id| (scope, id)))
                    .and_then(|(scope, id)| {
                        parse_percent_plain(pct_tok).map(|pct| (scope, id, pct))
                    })
                    .and_then(|(scope, target_id, pct)| {
                        parse_u64_plain(cas_tok).map(|cas| ControlCommand::ScopedOperation {
                            scope,
                            action: ScopedControlAction::Throttle {
                                throttle_percent: pct,
                            },
                            control_command_id: target_id,
                            target_id,
                            expected_generation_or_revision: cas,
                            reason: NL_THROTTLE_REASON.to_owned(),
                        })
                    }),
            )
        }
        // --- Explicit ControlScope unit verbs (EN + ZH) ---
        [verb, scope_kw, scope_tok, target_hex, exp_kw, cas_tok]
            if (scope_kw.eq_ignore_ascii_case("scope") || *scope_kw == "作用域")
                && (exp_kw.eq_ignore_ascii_case("expecting") || *exp_kw == "期望") =>
        {
            Some(
                parse_unit_scoped_action(verb)
                    .and_then(|(action, reason)| {
                        parse_control_scope(scope_tok).map(|scope| (action, reason, scope))
                    })
                    .and_then(|(action, reason, scope)| {
                        parse_hex_id(target_hex).map(|target_id| (action, reason, scope, target_id))
                    })
                    .and_then(|(action, reason, scope, target_id)| {
                        parse_u64_plain(cas_tok).map(|cas| ControlCommand::ScopedOperation {
                            scope,
                            action,
                            control_command_id: target_id,
                            target_id,
                            expected_generation_or_revision: cas,
                            reason: reason.to_owned(),
                        })
                    }),
            )
        }

        // --- Request Process Migration ---
        [
            head,
            second,
            proc_hex,
            to_kw,
            cell_kw,
            cell_hex,
            exp_kw,
            cas_tok,
        ] if head.eq_ignore_ascii_case("migrate")
            && second.eq_ignore_ascii_case("process")
            && to_kw.eq_ignore_ascii_case("to")
            && cell_kw.eq_ignore_ascii_case("cell")
            && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(
                parse_hex_id(proc_hex)
                    .and_then(|process_id| {
                        parse_hex_id(cell_hex)
                            .map(|destination_cell_id| (process_id, destination_cell_id))
                    })
                    .and_then(|(process_id, destination_cell_id)| {
                        parse_u64_plain(cas_tok).map(|cas| {
                            ControlCommand::RequestProcessMigration {
                                control_command_id: process_id,
                                process_id,
                                destination_cell_id,
                                expected_generation_or_revision: cas,
                                reason: NL_MIGRATE_REASON.to_owned(),
                            }
                        })
                    }),
            )
        }
        ["迁移进程", proc_hex, "到单元", cell_hex, "期望", cas_tok]
        | [
            "迁移",
            "进程",
            proc_hex,
            "到单元",
            cell_hex,
            "期望",
            cas_tok,
        ] => Some(
            parse_hex_id(proc_hex)
                .and_then(|process_id| {
                    parse_hex_id(cell_hex)
                        .map(|destination_cell_id| (process_id, destination_cell_id))
                })
                .and_then(|(process_id, destination_cell_id)| {
                    parse_u64_plain(cas_tok).map(|cas| ControlCommand::RequestProcessMigration {
                        control_command_id: process_id,
                        process_id,
                        destination_cell_id,
                        expected_generation_or_revision: cas,
                        reason: NL_MIGRATE_REASON.to_owned(),
                    })
                }),
        ),
        [head, ..] if head.eq_ignore_ascii_case("migrate") || *head == "迁移进程" => {
            Some(Err(ControlError::InvalidCommand(
                "\"migrate process\" expects \"<32-hex> to cell <32-hex> expecting <count>\"",
            )))
        }

        // --- Purge Topic ---
        [head, second, topic_hex, ack_kw, exp_kw, cas_tok]
            if head.eq_ignore_ascii_case("purge")
                && second.eq_ignore_ascii_case("topic")
                && ack_kw.eq_ignore_ascii_case("confirmed")
                && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_hex_id(topic_hex).and_then(|topic_id| {
                parse_u64_plain(cas_tok).map(|cas| ControlCommand::PurgeTopic {
                    control_command_id: topic_id,
                    topic_id,
                    expected_generation_or_revision: cas,
                    acknowledge_data_loss: true,
                    reason: NL_PURGE_TOPIC_REASON.to_owned(),
                })
            }))
        }
        ["清理主题", topic_hex, "确认", "期望", cas_tok]
        | ["清理", "主题", topic_hex, "确认", "期望", cas_tok] => {
            Some(parse_hex_id(topic_hex).and_then(|topic_id| {
                parse_u64_plain(cas_tok).map(|cas| ControlCommand::PurgeTopic {
                    control_command_id: topic_id,
                    topic_id,
                    expected_generation_or_revision: cas,
                    acknowledge_data_loss: true,
                    reason: NL_PURGE_TOPIC_REASON.to_owned(),
                })
            }))
        }
        [head, ..] if head.eq_ignore_ascii_case("purge") || *head == "清理主题" => {
            Some(Err(ControlError::InvalidCommand(
                "\"purge topic\" expects \"<32-hex> confirmed expecting <count>\"",
            )))
        }

        // --- Update Settings Domain ---
        [
            head,
            second,
            kind_tok,
            scope_tok,
            target_hex,
            key_tok,
            val_tok,
            exp_kw,
            cas_tok,
        ] if head.eq_ignore_ascii_case("update")
            && second.eq_ignore_ascii_case("settings")
            && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(
                parse_settings_domain_kind(kind_tok)
                    .and_then(|domain_kind| {
                        parse_control_scope(scope_tok).map(|scope| (domain_kind, scope))
                    })
                    .and_then(|(domain_kind, scope)| {
                        parse_hex_id(target_hex).map(|target_id| (domain_kind, scope, target_id))
                    })
                    .and_then(|(domain_kind, scope, target_id)| {
                        parse_u64_plain(cas_tok).map(|cas| ControlCommand::UpdateSettingsDomain {
                            control_command_id: target_id,
                            scope,
                            target_id,
                            expected_generation_or_revision: cas,
                            domain_kind,
                            key: (*key_tok).to_owned(),
                            value_cbor: val_tok.as_bytes().to_vec(),
                            reason: NL_UPDATE_SETTINGS_REASON.to_owned(),
                        })
                    }),
            )
        }

        // --- Set Content Type Association ---
        [
            head,
            second,
            assoc_hex,
            content_type,
            to_kw,
            app_hex,
            exp_kw,
            cas_tok,
        ] if head.eq_ignore_ascii_case("set")
            && second.eq_ignore_ascii_case("association")
            && to_kw.eq_ignore_ascii_case("to")
            && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(
                parse_hex_id(assoc_hex)
                    .and_then(|association_id| {
                        parse_hex_id(app_hex).map(|application_id| (association_id, application_id))
                    })
                    .and_then(|(association_id, application_id)| {
                        parse_u64_plain(cas_tok).map(|cas| {
                            ControlCommand::SetContentTypeAssociation {
                                control_command_id: association_id,
                                association_id,
                                expected_generation_or_revision: cas,
                                content_type: (*content_type).to_owned(),
                                application_id,
                                reason: NL_SET_ASSOCIATION_REASON.to_owned(),
                            }
                        })
                    }),
            )
        }

        // --- Issue Chooser Grant ---
        [
            head,
            second,
            grant_hex,
            app_hex,
            kind_tok,
            handle_hex,
            exp_kw,
            cas_tok,
        ] if head.eq_ignore_ascii_case("issue")
            && second.eq_ignore_ascii_case("chooser")
            && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(
                parse_hex_id(grant_hex)
                    .and_then(|grant_id| {
                        parse_hex_id(app_hex).map(|application_id| (grant_id, application_id))
                    })
                    .and_then(|(grant_id, application_id)| {
                        parse_u32_limit(kind_tok)
                            .map(|chooser_kind| (grant_id, application_id, chooser_kind))
                    })
                    .and_then(|(grant_id, application_id, chooser_kind)| {
                        parse_hex_id(handle_hex).map(|resource_handle| {
                            (grant_id, application_id, chooser_kind, resource_handle)
                        })
                    })
                    .and_then(
                        |(grant_id, application_id, chooser_kind, resource_handle)| {
                            parse_u64_plain(cas_tok).map(|cas| ControlCommand::IssueChooserGrant {
                                control_command_id: grant_id,
                                grant_id,
                                expected_generation_or_revision: cas,
                                application_id,
                                chooser_kind,
                                resource_handle,
                                reason: NL_ISSUE_CHOOSER_REASON.to_owned(),
                            })
                        },
                    ),
            )
        }

        // --- Undo Receipt ---
        [
            head,
            second,
            scope_tok,
            target_hex,
            class_tok,
            exp_kw,
            cas_tok,
        ] if head.eq_ignore_ascii_case("undo")
            && second.eq_ignore_ascii_case("receipt")
            && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(
                parse_control_scope(scope_tok)
                    .and_then(|scope| parse_hex_id(target_hex).map(|target_id| (scope, target_id)))
                    .and_then(|(scope, target_id)| {
                        parse_undo_class(class_tok)
                            .map(|receipt_undo_class| (scope, target_id, receipt_undo_class))
                    })
                    .and_then(|(scope, target_id, receipt_undo_class)| {
                        parse_u64_plain(cas_tok).map(|cas| ControlCommand::UndoReceipt {
                            control_command_id: target_id,
                            scope,
                            target_id,
                            receipt_undo_class,
                            expected_generation_or_revision: cas,
                            reason: NL_UNDO_RECEIPT_REASON.to_owned(),
                        })
                    }),
            )
        }
        [
            "撤销回执",
            scope_tok,
            target_hex,
            class_tok,
            "期望",
            cas_tok,
        ] => Some(
            parse_control_scope(scope_tok)
                .and_then(|scope| parse_hex_id(target_hex).map(|target_id| (scope, target_id)))
                .and_then(|(scope, target_id)| {
                    parse_undo_class(class_tok)
                        .map(|receipt_undo_class| (scope, target_id, receipt_undo_class))
                })
                .and_then(|(scope, target_id, receipt_undo_class)| {
                    parse_u64_plain(cas_tok).map(|cas| ControlCommand::UndoReceipt {
                        control_command_id: target_id,
                        scope,
                        target_id,
                        receipt_undo_class,
                        expected_generation_or_revision: cas,
                        reason: NL_UNDO_RECEIPT_REASON.to_owned(),
                    })
                }),
        ),
        [head, ..] if head.eq_ignore_ascii_case("undo") || *head == "撤销回执" => {
            Some(Err(ControlError::InvalidCommand(
                "\"undo receipt\" expects \"<scope> <32-hex> <undo_class> expecting <count>\"",
            )))
        }

        // --- Statutory Lock Probes ---
        [head, second, third, cand_hex, scope_hex, exp_kw, cas_tok]
            if head.eq_ignore_ascii_case("mutate")
                && second.eq_ignore_ascii_case("search")
                && third.eq_ignore_ascii_case("candidate")
                && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(
                parse_hex_id(cand_hex)
                    .and_then(|candidate_id| {
                        parse_hex_32(scope_hex).map(|scope_id| (candidate_id, scope_id))
                    })
                    .and_then(|(candidate_id, scope_id)| {
                        parse_u64_plain(cas_tok).map(|cas| ControlCommand::MutateSearchCandidate {
                            control_command_id: candidate_id,
                            candidate_id,
                            scope_id,
                            non_authoritative_restrictions_bits:
                                SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
                            expected_generation_or_revision: cas,
                            reason: NL_MUTATE_SEARCH_REASON.to_owned(),
                        })
                    }),
            )
        }
        [
            head,
            second,
            third,
            plan_hex,
            node_hex,
            state_tok,
            exp_kw,
            cas_tok,
        ] if head.eq_ignore_ascii_case("mutate")
            && second.eq_ignore_ascii_case("node")
            && third.eq_ignore_ascii_case("topology")
            && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(
                parse_hex_id(plan_hex)
                    .and_then(|plan_id| parse_hex_id(node_hex).map(|node_id| (plan_id, node_id)))
                    .and_then(|(plan_id, node_id)| {
                        parse_plan_node_state(state_tok)
                            .map(|observed_node_state| (plan_id, node_id, observed_node_state))
                    })
                    .and_then(|(plan_id, node_id, observed_node_state)| {
                        parse_u64_plain(cas_tok).map(|cas| ControlCommand::MutateTaskNodeTopology {
                            control_command_id: node_id,
                            plan_id,
                            node_id,
                            observed_node_state,
                            expected_generation_or_revision: cas,
                            reason: NL_MUTATE_TOPOLOGY_REASON.to_owned(),
                        })
                    }),
            )
        }
        [head, second, third, attempt_hex, exp_kw, cas_tok]
            if head.eq_ignore_ascii_case("override")
                && second.eq_ignore_ascii_case("attempt")
                && third.eq_ignore_ascii_case("commit")
                && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_hex_id(attempt_hex).and_then(|attempt_id| {
                parse_u64_plain(cas_tok).map(|cas| ControlCommand::OverrideTaskAttemptCommit {
                    control_command_id: attempt_id,
                    attempt_id,
                    expected_generation_or_revision: cas,
                    reason: NL_OVERRIDE_ATTEMPT_REASON.to_owned(),
                })
            }))
        }
        [
            head,
            second,
            third,
            op_hex,
            as_kw,
            outcome_tok,
            exp_kw,
            cas_tok,
        ] if head.eq_ignore_ascii_case("override")
            && second.eq_ignore_ascii_case("unknown")
            && third.eq_ignore_ascii_case("operation")
            && as_kw.eq_ignore_ascii_case("as")
            && exp_kw.eq_ignore_ascii_case("expecting") =>
        {
            let mark_succeeded = match *outcome_tok {
                s if s.eq_ignore_ascii_case("succeeded") => true,
                s if s.eq_ignore_ascii_case("not_happened") => false,
                _ => {
                    return Some(Err(ControlError::InvalidCommand(
                        "expected \"succeeded\" or \"not_happened\"",
                    )));
                }
            };
            Some(parse_hex_id(op_hex).and_then(|operation_id| {
                parse_u64_plain(cas_tok).map(|cas| ControlCommand::OverrideEffectUnknownOperation {
                    control_command_id: operation_id,
                    operation_id,
                    mark_succeeded,
                    expected_generation_or_revision: cas,
                    reason: NL_OVERRIDE_OPERATION_REASON.to_owned(),
                })
            }))
        }

        _ => None,
    }
}
