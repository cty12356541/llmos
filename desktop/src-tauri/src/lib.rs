//! llmos 任务管理器 Tauri 壳(W32-A 只读半 + W32-B 写入半 + W69 IPC-D1..D8 12 域显控同构桥)。
//!
//! 后端命令层(`ipc`)经 ADR-0011 认证入口连接真实 SystemControl IPC;
//! 前端渲染 SABI Receipt 数据。W32-B 增加授权控制动作
//! (ack/resume/pause/cancel/kill/throttle/reclaim)的 GUI 派发与 Receipt
//! 展示;W69-D-IPC-ENUM 补齐 inspect_search / inspect_notification 与
//! IPC-D1..D8 12 域有界游标枚举、爆炸半径预演及控制命令状态对账桥。

pub mod a11y_i18n;
pub mod compositor;
pub mod dto;
pub mod error;
pub mod input_router;
pub mod ipc;
pub mod keypool_bridge;
pub mod notify_search_offline;
pub mod omnibar;
pub mod plan_editor;
pub mod session;
pub mod settings_keychain;
pub mod surfaces;
pub mod trusted_ui;
pub mod window_lifecycle;

/// 开发夹具:在 Unix / Windows + `dev-fixture` feature 下编译(`dev_server` 示例与
/// 集成测试共享)。Unix 侧绑定临时 Unix socket 并读 `/dev/urandom`;Windows 侧绑定
/// 本地命名管道(`NamedPipeListenerAdapter`)并经 `getrandom` 读取 OS CSPRNG。
#[cfg(all(any(unix, windows), feature = "dev-fixture"))]
pub mod devfixture;

pub fn run() {
    if let Err(error) = tauri::Builder::default()
        .manage(ipc::AppState::from_env())
        .manage(trusted_ui::TrustedUiState::default())
        .manage(plan_editor::PlanEditorState::default())
        .manage(notify_search_offline::NotifySearchOfflineState::default())
        .manage(settings_keychain::SettingsKeychainState::default())
        .manage(a11y_i18n::A11yI18nState::default())
        .invoke_handler(tauri::generate_handler![
            ipc::get_config,
            ipc::set_config,
            ipc::inspect_health,
            ipc::inspect_semantic_health,
            ipc::export_metrics,
            ipc::export_semantic_metrics,
            ipc::export_resource_metrics,
            ipc::inspect_task,
            ipc::inspect_task_group,
            ipc::inspect_task_node,
            ipc::inspect_execution_fiber,
            ipc::inspect_topic,
            ipc::inspect_operation,
            ipc::inspect_process,
            ipc::inspect_application,
            ipc::inspect_resource,
            ipc::inspect_resource_health,
            ipc::submit_control,
            ipc::parity_check,
            ipc::parity_check_write,
            ipc::control_plane_facts,
            ipc::inspect_resource_cost,
            ipc::cost_fact_check,
            ipc::present_surfaces,
            ipc::inspect_search,
            ipc::inspect_notification,
            ipc::list_tasks,
            ipc::list_task_groups,
            ipc::list_task_nodes,
            ipc::list_task_attempts,
            ipc::list_execution_fibers,
            ipc::list_operations,
            ipc::list_installed_applications,
            ipc::query_search_candidates,
            ipc::poll_desktop_watermark_events,
            ipc::inspect_settings_domain,
            ipc::list_content_type_associations,
            ipc::list_keychain_references,
            ipc::list_chooser_grants,
            ipc::list_workspaces,
            ipc::inspect_workspace,
            ipc::list_processes,
            ipc::list_resource_reservations,
            ipc::list_topics,
            ipc::list_notifications,
            ipc::list_cells,
            ipc::inspect_cell_leases,
            ipc::preview_control_blast_radius,
            ipc::query_control_command_status,
            ipc::compositor_get_snapshot,
            ipc::compositor_session_transition,
            ipc::compositor_workspace_action,
            ipc::compositor_set_layout,
            ipc::compositor_launch_application,
            ipc::compositor_window_action,
            ipc::compositor_set_z3_overlay,
            ipc::compositor_set_z4_modal,
            ipc::compositor_route_input,
            omnibar::omnibar_get_snapshot,
            omnibar::omnibar_toggle_palette,
            omnibar::omnibar_update_draft,
            omnibar::omnibar_clear_draft,
            omnibar::omnibar_attach_item,
            omnibar::omnibar_remove_attachment,
            omnibar::omnibar_compile_preview,
            omnibar::omnibar_select_candidate,
            omnibar::omnibar_submit_proposal,
            omnibar::omnibar_voice_action,
            omnibar::omnibar_set_device_permission,
            keypool_bridge::llm_keypool_inspect_status,
            keypool_bridge::llm_keypool_assist_intent,
            keypool_bridge::llm_keypool_chat,
            trusted_ui::trusted_ui_open_z4_consent,
            trusted_ui::trusted_ui_notify_z4_focus_regained,
            trusted_ui::trusted_ui_decide_z4_consent,
            trusted_ui::trusted_ui_get_public_posture,
            trusted_ui::trusted_ui_open_chooser,
            trusted_ui::trusted_ui_confirm_chooser_grant,
            trusted_ui::trusted_ui_revoke_chooser_grant,
            trusted_ui::trusted_ui_mint_gesture_token,
            trusted_ui::trusted_ui_publish_transfer,
            trusted_ui::trusted_ui_request_transfer_read,
            trusted_ui::trusted_ui_complete_escalated_transfer,
            trusted_ui::trusted_ui_get_security_audit,
            plan_editor::inspect_task_plan_dag_cmd,
            plan_editor::expand_lazy_plan_supernode_cmd,
            plan_editor::preview_plan_override_diff_cmd,
            plan_editor::submit_plan_override_cmd,
            plan_editor::inspect_task_attempt_lanes_cmd,
            plan_editor::reopen_terminal_task_cmd,
            notify_search_offline::list_notification_feed,
            notify_search_offline::redeem_notification_action_token,
            notify_search_offline::dismiss_notification_item,
            notify_search_offline::query_spotlight_candidates,
            notify_search_offline::verify_spotlight_candidate_authority,
            notify_search_offline::attempt_mutation_from_search_hit,
            notify_search_offline::inspect_offline_cluster_posture,
            notify_search_offline::evaluate_offline_action_availability,
            notify_search_offline::set_cluster_connectivity_mode,
            settings_keychain::inspect_system_control_center,
            settings_keychain::request_process_migration_v2,
            settings_keychain::preview_blast_radius_package,
            settings_keychain::update_setting_with_cas,
            settings_keychain::revoke_content_type_association,
            settings_keychain::revoke_keychain_reference,
            a11y_i18n::a11y_get_locale_bundle,
            a11y_i18n::a11y_switch_locale,
            a11y_i18n::a11y_audit_wcag_contrast,
            a11y_i18n::a11y_audit_aria_landmarks,
            a11y_i18n::a11y_verify_keyboard_focus_trap,
            a11y_i18n::a11y_set_reduced_motion_override,
        ])
        .run(tauri::generate_context!())
    {
        eprintln!("llmos-desktop 启动失败: {error}");
        std::process::exit(1);
    }
}
