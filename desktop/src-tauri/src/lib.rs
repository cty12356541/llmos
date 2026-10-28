//! llmos 任务管理器 Tauri 壳(W32-A 只读半 + W32-B 写入半 + W69 IPC-D1..D8 12 域显控同构桥)。
//!
//! 后端命令层(`ipc`)经 ADR-0011 认证入口连接真实 SystemControl IPC;
//! 前端渲染 SABI Receipt 数据。W32-B 增加授权控制动作
//! (ack/resume/pause/cancel/kill/throttle/reclaim)的 GUI 派发与 Receipt
//! 展示;W69-D-IPC-ENUM 补齐 inspect_search / inspect_notification 与
//! IPC-D1..D8 12 域有界游标枚举、爆炸半径预演及控制命令状态对账桥。

pub mod compositor;
pub mod dto;
pub mod error;
pub mod input_router;
pub mod ipc;
pub mod omnibar;
pub mod session;
pub mod surfaces;
pub mod trusted_ui;
pub mod window_lifecycle;

/// 开发夹具:仅在 Unix + `dev-fixture` feature 下编译(dev_server 示例与
/// unix 侧集成测试)。夹具 harness 硬编码 Unix socket 路径与
/// `/dev/urandom`,Windows 移植属后续车道;Windows 侧认证面证据走
/// `tests/windows_authenticated_pipe_side.rs` 与 `tests/ipc_enum_symmetry_side.rs`(命名管道端到端)。
#[cfg(all(unix, feature = "dev-fixture"))]
pub mod devfixture;

pub fn run() {
    if let Err(error) = tauri::Builder::default()
        .manage(ipc::AppState::from_env())
        .manage(trusted_ui::TrustedUiState::default())
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
        ])
        .run(tauri::generate_context!())
    {
        eprintln!("llmos-desktop 启动失败: {error}");
        std::process::exit(1);
    }
}
