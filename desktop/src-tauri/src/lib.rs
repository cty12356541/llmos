//! llmos 任务管理器 Tauri 壳(W32-A 只读半 + W32-B 写入半)。
//!
//! 后端命令层(`ipc`)经 ADR-0011 认证入口连接真实 SystemControl IPC;
//! 前端渲染 SABI Receipt 数据。W32-B 增加授权控制动作
//! (ack/resume/pause/cancel/kill/throttle/reclaim)的 GUI 派发与 Receipt
//! 展示;parity 钉死(W32-C)不在本壳的实现范围内。

pub mod dto;
pub mod error;
pub mod ipc;
pub mod surfaces;
pub mod transport;
pub mod window_lifecycle;

/// 开发夹具:仅在 `dev-fixture` feature 下编译(dev_server 示例与集成测试)。
#[cfg(feature = "dev-fixture")]
pub mod devfixture;

/// Windows 安全原语(dev-fixture 凭据面,F3-c3:密钥文件 owner-only DACL)。
/// 仅 dev-fixture 使用;生产凭据面属后续 Windows daemon 车道。
#[cfg(all(windows, feature = "dev-fixture"))]
pub mod winsec;

pub fn run() {
    if let Err(error) = tauri::Builder::default()
        .manage(ipc::AppState::from_env())
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
        ])
        .run(tauri::generate_context!())
    {
        eprintln!("llmos-desktop 启动失败: {error}");
        std::process::exit(1);
    }
}
