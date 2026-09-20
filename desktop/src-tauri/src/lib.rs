//! llmos 任务管理器 Tauri 壳(W32-A 只读半)。
//!
//! 后端命令层(`ipc`)经 ADR-0011 认证入口连接真实 SystemControl IPC;
//! 前端渲染 SABI Receipt 数据。写入/控制动作(W32-B)与 parity 钉死
//! (W32-C)不在本壳的实现范围内。

pub mod dto;
pub mod error;
pub mod ipc;

/// 开发夹具:仅在 `dev-fixture` feature 下编译(dev_server 示例与集成测试)。
#[cfg(feature = "dev-fixture")]
pub mod devfixture;

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
            ipc::inspect_task,
            ipc::inspect_process,
            ipc::inspect_resource,
            ipc::parity_check,
        ])
        .run(tauri::generate_context!())
    {
        eprintln!("llmos-desktop 启动失败: {error}");
        std::process::exit(1);
    }
}
