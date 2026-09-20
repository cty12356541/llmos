//! `dev_server` —— 开发夹具服务器(feature `dev-fixture`)。
//!
//! 装配真实权威(IdentityAuthority / AuthorityClock / SqliteTaskAuthority)
//! 并同时开放 ADR-0011 认证入口(GUI 唯一接线方式)与 plain 入口(供真实
//! `system-control-cli` 做一致性比对)。启动后把 GUI 连接所需的全部环境
//! 变量打印到 stdout,把 Ed25519 种子写入 0600 临时密钥文件。
//!
//! 运行方式与完整演示步骤见 `desktop/README.md`。

use std::path::PathBuf;

use llmos_desktop_lib::devfixture::DevFixture;

fn repo_cli_path() -> PathBuf {
    // CARGO_MANIFEST_DIR = desktop/src-tauri;仓库 target 目录在其上两级。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/system-control-cli")
        .canonicalize()
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/system-control-cli")
        })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut fixture = match DevFixture::spawn("dev") {
        Ok(fixture) => fixture,
        Err(error) => {
            eprintln!("dev_server: 夹具装配失败: {error}");
            std::process::exit(1);
        }
    };
    let key_file = std::env::temp_dir().join(format!("llmosdt-key-{}.key", std::process::id()));
    if let Err(error) = fixture.write_key_file(&key_file) {
        eprintln!("dev_server: 密钥文件写入失败: {error}");
        std::process::exit(1);
    }

    println!("# ---- llmos 桌面壳开发夹具(认证 + plain 双入口)----");
    println!("# GUI 环境变量(export 后启动 `npm run tauri dev`):");
    println!(
        "export LLMOS_DESKTOP_SOCKET={}",
        fixture.socket_authenticated().display()
    );
    println!("export LLMOS_DESKTOP_PRINCIPAL={}", fixture.principal_hex());
    println!("export LLMOS_DESKTOP_KEY_FILE={}", key_file.display());
    println!(
        "export LLMOS_DESKTOP_CLI_SOCKET={}",
        fixture.socket_plain().display()
    );
    println!("export LLMOS_DESKTOP_CLI={}", repo_cli_path().display());
    println!();
    println!("# 演示数据:escalated 恢复计划(任务查询/一致性自检用)");
    println!("plan_id = {}", fixture.plan_id_hex());
    println!();
    println!("# CLI 一致性手动比对(先 cargo build -p nlos-system-control):");
    println!(
        "{} {} inspect-health",
        repo_cli_path().display(),
        fixture.socket_plain().display()
    );
    println!();
    println!("# 密钥种子已写入 0600 文件:{}", key_file.display());
    println!("# Ctrl-C 停止。");

    let (authenticated, plain) = fixture.serve_forever();
    let _ = tokio::join!(authenticated, plain);
}
