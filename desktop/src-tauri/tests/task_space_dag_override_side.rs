//! Stage D (`W72-D-TASK-DAG` / `D-TASK-SPACE-DAG`) Integration Tests:
//! `TaskSpace` `L1 -> L2 -> L3` Progressive Disclosure, Dagster Orthogonal `TaskPlan` DAG
//! Override Editor (Path A vs Path B + `Z4` Re-Authorization), `[PLAN-LAZY-001]` Super-Node
//! Expansion, Statutory Locks #3/#4/#5/#6, and `[TASK-REOPEN-001]` Terminal Reopen Semantics.

use llmos_desktop_lib::plan_editor::{
    CLOSED_TASK_SPACE_GAPS_COUNT, MAX_PLAN_EDITOR_ENUM_LIMIT, OPEN_TASK_SPACE_GAPS_COUNT,
    PlanEditorError, PlanEditorState, PlanOverrideOpDto, PlanOverridePathKind, PlanResidencyTier,
    ReopenTaskMode, STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT,
    STATUTORY_LOCK_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS, STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE,
    TaskNodeLifecycleState, clamp_plan_editor_limit, execute_task_attempt_control,
    expand_lazy_plan_supernode, inspect_task_attempt_lanes, inspect_task_plan_dag,
    issue_z4_reauthorization_grant, preview_plan_override_diff, reopen_terminal_task,
    submit_plan_override,
};

#[test]
fn task_space_gaps_closure_and_cursor_limit_clamp() {
    assert_eq!(CLOSED_TASK_SPACE_GAPS_COUNT, 3);
    assert_eq!(OPEN_TASK_SPACE_GAPS_COUNT, 0);
    assert_eq!(clamp_plan_editor_limit(None), MAX_PLAN_EDITOR_ENUM_LIMIT);
    assert_eq!(clamp_plan_editor_limit(Some(0)), MAX_PLAN_EDITOR_ENUM_LIMIT);
    assert_eq!(
        clamp_plan_editor_limit(Some(256)),
        MAX_PLAN_EDITOR_ENUM_LIMIT
    );
    assert_eq!(clamp_plan_editor_limit(Some(24)), 24);
}

#[test]
fn inspect_task_plan_dag_projects_w29a_fields_and_distinguishes_not_found_vs_stale_generation() {
    let state = PlanEditorState::new();

    let dag = inspect_task_plan_dag(&state, "task-01", Some(3)).expect("inspect task-01 r3");
    assert_eq!(dag.task_id, "task-01");
    assert_eq!(dag.plan_id, "plan-01");
    assert_eq!(dag.application_id, "app.workstation.suite.01");
    assert_eq!(dag.installation_generation, 2);
    assert_eq!(dag.plan_revision, 3);
    assert_eq!(dag.groups.len(), 2);
    assert_eq!(dag.nodes.len(), 4);
    assert_eq!(dag.lazy_supernodes.len(), 1);
    assert!(!dag.receipt_hex.is_empty());

    let missing = inspect_task_plan_dag(&state, "task-nonexistent", None)
        .expect_err("missing task must return NotFound");
    assert!(matches!(
        missing,
        PlanEditorError::NotFound { ref entity_id, .. } if entity_id == "task-nonexistent"
    ));

    let stale = inspect_task_plan_dag(&state, "task-01", Some(1))
        .expect_err("stale revision must return StaleGeneration");
    assert!(matches!(
        stale,
        PlanEditorError::StaleGeneration {
            expected_generation: 1,
            current_generation: 3,
            ..
        }
    ));
}

#[test]
fn lazy_plan_supernode_expands_on_demand_with_bounded_cursor_and_zero_preallocated_processes() {
    let state = PlanEditorState::new();
    let dag = inspect_task_plan_dag(&state, "task-01", None).expect("dag");
    let supernode = &dag.lazy_supernodes[0];
    assert_eq!(supernode.supernode_id, "plan-01-supernode-cold");
    assert_eq!(supernode.unmaterialized_step_count, 18);
    assert_eq!(supernode.allocated_process_count, 0);
    assert_eq!(supernode.residency_tier, PlanResidencyTier::MetadataOnly);
    assert_eq!(supernode.badge_zh, "+18 待物化步骤 · 0 进程");
    assert_eq!(supernode.badge_en, "+18 Unmaterialized Steps · 0 Processes");

    let page1 = expand_lazy_plan_supernode(&state, "task-01", "plan-01-supernode-cold", Some(10))
        .expect("expand 10 steps");
    assert_eq!(page1.effective_limit, 10);
    assert_eq!(page1.allocated_process_count_before, 0);
    assert_eq!(page1.materialized_nodes.len(), 10);
    assert_eq!(page1.remaining_unmaterialized_count, 8);
    assert!(page1.next_cursor_hex.is_some());

    let full_clamped =
        expand_lazy_plan_supernode(&state, "task-01", "plan-01-supernode-cold", Some(500))
            .expect("expand clamped to 64");
    assert_eq!(full_clamped.effective_limit, MAX_PLAN_EDITOR_ENUM_LIMIT);
    assert_eq!(full_clamped.materialized_nodes.len(), 18);
    assert_eq!(full_clamped.remaining_unmaterialized_count, 0);
    assert!(full_clamped.next_cursor_hex.is_none());
}

#[test]
fn statutory_lock_3_blocks_overrides_on_completed_or_active_task_nodes() {
    let state = PlanEditorState::new();

    let forbidden_ops = [
        vec![PlanOverrideOpDto::RemoveNode {
            target_node_id: "plan-01-node-completed".to_owned(),
            alters_acceptance_or_side_effects: true,
        }],
        vec![PlanOverrideOpDto::PauseNode {
            target_node_id: "plan-01-node-active".to_owned(),
            reason: Some("Attempt to pause running node".to_owned()),
        }],
        vec![PlanOverrideOpDto::ReplaceNode {
            target_node_id: "plan-01-node-completed".to_owned(),
            replacement_node_id: "plan-01-node-hack".to_owned(),
            replacement_title: "Tampered step".to_owned(),
            capability_id: "cap.fs.workspace.read".to_owned(),
            requested_budget_units: 10,
            cross_application_target: None,
        }],
        vec![PlanOverrideOpDto::MergeNodes {
            primary_node_id: "plan-01-node-active".to_owned(),
            secondary_node_id: "plan-01-node-pending-a".to_owned(),
            merged_title: "Illegal merge with active".to_owned(),
            cross_scope_or_capability: false,
        }],
        vec![PlanOverrideOpDto::ReorderNodes {
            node_id: "plan-01-node-completed".to_owned(),
            new_dependency_node_ids: vec!["plan-01-node-pending-a".to_owned()],
            within_authorized_envelope: true,
        }],
    ];

    for ops in forbidden_ops {
        let preview_err = preview_plan_override_diff(&state, "task-01", 3, &ops)
            .expect_err("executed/active node edit must fail");
        assert!(matches!(
            preview_err,
            PlanEditorError::ReadOnlyExecutedTaskNode {
                ref statutory_lock,
                node_state: TaskNodeLifecycleState::Completed | TaskNodeLifecycleState::Active,
                ..
            } if statutory_lock == STATUTORY_LOCK_NODE_COMPLETED_OR_ACTIVE
        ));

        let submit_err = submit_plan_override(&state, "task-01", 3, &ops, None)
            .expect_err("executed/active node submit must fail");
        assert!(matches!(
            submit_err,
            PlanEditorError::ReadOnlyExecutedTaskNode { .. }
        ));
    }

    let dag_after = inspect_task_plan_dag(&state, "task-01", Some(3)).expect("revision unchanged");
    assert_eq!(dag_after.plan_revision, 3);
}

#[test]
fn path_a_bounded_control_commands_compile_and_commit_without_z4_reauthorization() {
    let state = PlanEditorState::new();
    let ops = vec![
        PlanOverrideOpDto::PauseNode {
            target_node_id: "plan-01-node-pending-a".to_owned(),
            reason: Some("Hold before report generation".to_owned()),
        },
        PlanOverrideOpDto::ReorderNodes {
            node_id: "plan-01-node-pending-b".to_owned(),
            new_dependency_node_ids: vec!["plan-01-node-active".to_owned()],
            within_authorized_envelope: true,
        },
    ];

    let preview = preview_plan_override_diff(&state, "task-01", 3, &ops).expect("Path A preview");
    assert_eq!(
        preview.path_kind,
        PlanOverridePathKind::BoundedControlCommand
    );
    assert!(!preview.requires_z4_reauthorization);
    assert_eq!(preview.compiled_control_commands.len(), 2);
    assert_eq!(
        preview.compiled_control_commands[0].control_scope,
        "task-node"
    );
    assert_eq!(
        preview.compiled_control_commands[1].control_scope,
        "task-plan"
    );
    assert_eq!(preview.modified_count, 2);

    let commit =
        submit_plan_override(&state, "task-01", 3, &ops, None).expect("Path A commit without Z4");
    assert_eq!(
        commit.path_kind,
        PlanOverridePathKind::BoundedControlCommand
    );
    assert_eq!(commit.new_plan_revision, 4);
    assert!(commit.z4_grant_token_hex.is_none());
    assert!(!commit.receipt_hex.is_empty());

    let paused_node = commit
        .updated_dag
        .nodes
        .iter()
        .find(|n| n.node_id == "plan-01-node-pending-a")
        .expect("pending-a node");
    assert_eq!(paused_node.state, TaskNodeLifecycleState::Suspended);
}

#[test]
fn path_b_structural_and_capability_budget_expansions_require_z4_reauthorization_and_emit_terraform_diff()
 {
    let state = PlanEditorState::new();
    let ops = vec![
        PlanOverrideOpDto::AddNode {
            node_id: "plan-02-node-audit-export".to_owned(),
            group_id: "plan-02-grp-commit".to_owned(),
            title: "跨应用安全审计同步".to_owned(),
            dependency_node_ids: vec!["plan-02-node-pending-b".to_owned()],
            capability_id: "cap.net.audit.export".to_owned(),
            requested_budget_units: 60,
            cross_application_target: Some("app.security.auditor".to_owned()),
        },
        PlanOverrideOpDto::ReplaceNode {
            target_node_id: "plan-02-node-pending-a".to_owned(),
            replacement_node_id: "plan-02-node-pending-a-v2".to_owned(),
            replacement_title: "生成增强阶段报告 v2".to_owned(),
            capability_id: "cap.fs.workspace.write".to_owned(),
            requested_budget_units: 30,
            cross_application_target: None,
        },
        PlanOverrideOpDto::RemoveNode {
            target_node_id: "plan-02-node-pending-b".to_owned(),
            alters_acceptance_or_side_effects: true,
        },
    ];

    let preview = preview_plan_override_diff(&state, "task-02", 3, &ops).expect("Path B preview");
    assert_eq!(
        preview.path_kind,
        PlanOverridePathKind::RecompileAndZ4Reauthorize
    );
    assert!(preview.requires_z4_reauthorization);
    assert!(preview.compiled_control_commands.is_empty());
    assert!(preview.added_count >= 3);
    assert!(preview.modified_count >= 1);
    assert_eq!(preview.removed_count, 1);

    // Submitting Path B without Z4 grant must be rejected
    let missing_z4 = submit_plan_override(&state, "task-02", 3, &ops, None)
        .expect_err("Path B without Z4 must fail");
    assert!(matches!(
        missing_z4,
        PlanEditorError::Z4ReauthorizationRequired {
            expected_plan_revision: 3,
            ..
        }
    ));

    // Submitting Path B with a forged/mismatched Z4 digest must also be rejected
    let mut forged_grant = issue_z4_reauthorization_grant(&preview, 11);
    forged_grant.approved_plan_digest_hex = "00000000000000000000000000000000".to_owned();
    let forged_err = submit_plan_override(&state, "task-02", 3, &ops, Some(&forged_grant))
        .expect_err("forged Z4 grant must fail");
    assert!(matches!(
        forged_err,
        PlanEditorError::Z4ReauthorizationRequired { .. }
    ));

    // Submitting Path B with a valid Z4 grant succeeds
    let valid_grant = issue_z4_reauthorization_grant(&preview, 11);
    let commit = submit_plan_override(&state, "task-02", 3, &ops, Some(&valid_grant))
        .expect("Path B with valid Z4 grant");
    assert_eq!(
        commit.path_kind,
        PlanOverridePathKind::RecompileAndZ4Reauthorize
    );
    assert_eq!(commit.new_plan_revision, 4);
    assert_eq!(
        commit.z4_grant_token_hex.as_deref(),
        Some(valid_grant.grant_token_hex.as_str())
    );
    assert!(
        commit
            .updated_dag
            .nodes
            .iter()
            .any(|n| n.node_id == "plan-02-node-audit-export")
    );
    assert!(
        commit
            .updated_dag
            .nodes
            .iter()
            .any(|n| n.node_id == "plan-02-node-pending-a-v2")
    );
    assert!(
        !commit
            .updated_dag
            .nodes
            .iter()
            .any(|n| n.node_id == "plan-02-node-pending-b")
    );
}

#[test]
fn inspect_task_attempt_lanes_and_statutory_lock_4_forbids_manual_attempt_commit() {
    let state = PlanEditorState::new();
    let lanes = inspect_task_attempt_lanes(&state, "task-01", Some(3)).expect("attempt lanes");
    assert_eq!(lanes.attempts.len(), 2);
    assert_eq!(
        lanes.winning_attempt_id.as_deref(),
        Some("task-01-attempt-2")
    );

    let a1 = &lanes.attempts[0];
    assert!(a1.superseded);
    assert!(!a1.cas_winner);
    assert!(a1.provider_isolation_unverified);
    assert_eq!(a1.statutory_lock, STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT);
    assert!(a1.fibers[0].shared_thread_like);
    assert_eq!(
        a1.fibers[0].fault_domain_badge.as_deref(),
        Some("共享进程域 · SHARED_THREAD_LIKE")
    );
    assert_eq!(
        a1.operations[0].statutory_lock.as_deref(),
        Some(STATUTORY_LOCK_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS)
    );

    let a2 = &lanes.attempts[1];
    assert!(a2.cas_winner);
    assert!(!a2.superseded);
    assert!(!a2.provider_isolation_unverified);

    // Statutory Lock #4: Manual commit verbs are deterministically rejected
    for forbidden in [
        "manual_commit",
        "commit",
        "force_commit",
        "mark_winner",
        "override_commit",
    ] {
        let err = execute_task_attempt_control(&state, "task-01", "task-01-attempt-1", forbidden)
            .expect_err("manual commit verb must be rejected");
        assert!(matches!(
            err,
            PlanEditorError::ForbiddenManualAttemptCommit {
                ref statutory_lock,
                ..
            } if statutory_lock == STATUTORY_LOCK_ATTEMPT_NO_MANUAL_COMMIT
        ));
    }

    // Allowed verb succeeds
    let updated =
        execute_task_attempt_control(&state, "task-01", "task-01-attempt-1", "request_reverify")
            .expect("request_reverify");
    assert_eq!(updated.task_head_cas_epoch, 8);
    assert_eq!(
        updated.attempts[0].state,
        TaskNodeLifecycleState::Recovering
    );
}

#[test]
fn reopen_terminal_task_spawns_new_attempt_on_retry_and_new_generation_on_refine_preserving_history()
 {
    let state = PlanEditorState::new();

    // Active healthy task without failed targets rejects terminal reopen
    let not_terminal = reopen_terminal_task(&state, "task-01", ReopenTaskMode::RetryNewAttempt)
        .expect_err("healthy running task must reject reopen");
    assert!(matches!(
        not_terminal,
        PlanEditorError::TaskNotTerminalOrPartial { .. }
    ));

    // RetryNewAttempt on Failed task-03 spawns attempt #3 and preserves attempts #1 and #2
    let retry_res = reopen_terminal_task(&state, "task-03", ReopenTaskMode::RetryNewAttempt)
        .expect("retry failed task-03");
    assert_eq!(retry_res.mode, ReopenTaskMode::RetryNewAttempt);
    assert_eq!(
        retry_res.spawned_attempt_id.as_deref(),
        Some("task-03-attempt-3")
    );
    assert_eq!(retry_res.retry_fence_epoch, 3);
    let lanes_after_retry =
        inspect_task_attempt_lanes(&state, "task-03", None).expect("task-03 lanes");
    assert_eq!(lanes_after_retry.attempts.len(), 3);

    // RefineNewGeneration on Cancelled task-04 spawns task-04-gen3 while preserving task-04 in history
    let refine_res = reopen_terminal_task(&state, "task-04", ReopenTaskMode::RefineNewGeneration)
        .expect("refine cancelled task-04");
    assert_eq!(refine_res.mode, ReopenTaskMode::RefineNewGeneration);
    assert_eq!(refine_res.resulting_task_id, "task-04-gen3");
    assert_eq!(refine_res.new_installation_generation, 3);
    assert_eq!(refine_res.new_plan_revision, 4);
    assert!(
        refine_res
            .preserved_history_task_ids
            .contains(&"task-04".to_owned())
    );
    assert!(
        refine_res
            .preserved_history_task_ids
            .contains(&"task-04-gen3".to_owned())
    );

    let orig_dag =
        inspect_task_plan_dag(&state, "task-04", Some(3)).expect("original task-04 preserved");
    assert_eq!(orig_dag.task_state, TaskNodeLifecycleState::Cancelled);

    let refined_dag =
        inspect_task_plan_dag(&state, "task-04-gen3", Some(4)).expect("new task-04-gen3 exists");
    assert_eq!(refined_dag.installation_generation, 3);
    assert_eq!(refined_dag.plan_revision, 4);
}
