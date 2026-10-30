//! `dev_server` —— 开发夹具服务器(feature `dev-fixture`)。
//!
//! 装配真实权威(IdentityAuthority / AuthorityClock / SqliteTaskAuthority)
//! 并同时开放 ADR-0011 认证入口(GUI 唯一接线方式,Unix socket / Windows Named Pipe)
//! 与 plain 入口(供真实 `system-control-cli` 做一致性比对)。启动后把 GUI 连接所需的
//! 全部环境变量打印到 stdout,把 Ed25519 种子写入临时密钥文件(Unix `0600`)。
//!
//! 运行方式与完整演示步骤见 `desktop/README.md`。

#[cfg(any(unix, windows))]
use std::path::{Path, PathBuf};

#[cfg(any(unix, windows))]
use llmos_desktop_lib::devfixture::DevFixture;

#[cfg(unix)]
const CLI_RELATIVE_PATH: &str = "../../target/debug/system-control-cli";

#[cfg(windows)]
const CLI_RELATIVE_PATH: &str = "../../target/debug/system-control-cli.exe";

#[cfg(any(unix, windows))]
fn repo_cli_path() -> PathBuf {
    // CARGO_MANIFEST_DIR = desktop/src-tauri;仓库 target 目录在其上两级。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(CLI_RELATIVE_PATH)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CLI_RELATIVE_PATH))
}

#[cfg(unix)]
fn print_platform_instructions(fixture: &DevFixture, key_file: &Path, cli_path: &Path) {
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
    println!("export LLMOS_DESKTOP_CLI={}", cli_path.display());
    println!(
        "export LLMOS_DESKTOP_RESOURCE_ROOT={}",
        fixture.resource_root().display()
    );
    println!();
    println!("# 演示数据:escalated 恢复计划(任务查询/一致性自检用)");
    println!("plan_id = {}", fixture.plan_id_hex());
    let facts = fixture.reservation_facts();
    println!();
    println!(
        "# 演示数据:已结清资源预留(「权限/预算」视图与成本自检用;upper_bound={} usage_high_water={} consumption_count={})",
        facts.upper_bound, facts.usage_high_water, facts.consumption_count
    );
    println!("reservation_id = {}", facts.reservation_id_hex);
    println!("account_id    = {}", facts.account_id_hex);
    println!();
    println!("# CLI 一致性手动比对(先 cargo build -p nlos-system-control):");
    println!(
        "{} {} inspect-health",
        cli_path.display(),
        fixture.socket_plain().display()
    );
    println!();
    println!("# 写路径自检(pause-operation 探针)默认关闭:探针会执行真实");
    println!("# mutation,仅在开发夹具宿主显式 opt-in 后可用(深审计 42 D9):");
    println!("export LLMOS_DESKTOP_ALLOW_WRITE_PARITY=1");
    println!();
    println!("# 密钥种子已写入 0600 文件:{}", key_file.display());
    println!("# Ctrl-C 停止。");
}

#[cfg(windows)]
fn print_platform_instructions(fixture: &DevFixture, key_file: &Path, cli_path: &Path) {
    println!("# ---- llmos 桌面壳开发夹具(认证 + plain 双入口 · Windows Named Pipe)----");
    println!("# GUI 环境变量(粘贴到 PowerShell 后启动 `npm run tauri dev`):");
    println!(
        "$env:LLMOS_DESKTOP_SOCKET = \"{}\"",
        fixture.socket_authenticated().display()
    );
    println!(
        "$env:LLMOS_DESKTOP_PRINCIPAL = \"{}\"",
        fixture.principal_hex()
    );
    println!("$env:LLMOS_DESKTOP_KEY_FILE = \"{}\"", key_file.display());
    println!(
        "$env:LLMOS_DESKTOP_CLI_SOCKET = \"{}\"",
        fixture.socket_plain().display()
    );
    println!("$env:LLMOS_DESKTOP_CLI = \"{}\"", cli_path.display());
    println!(
        "$env:LLMOS_DESKTOP_RESOURCE_ROOT = \"{}\"",
        fixture.resource_root().display()
    );
    println!();
    println!("# 演示数据:escalated 恢复计划(任务查询/一致性自检用)");
    println!("plan_id = {}", fixture.plan_id_hex());
    let facts = fixture.reservation_facts();
    println!();
    println!(
        "# 演示数据:已结清资源预留(「权限/预算」视图与成本自检用;upper_bound={} usage_high_water={} consumption_count={})",
        facts.upper_bound, facts.usage_high_water, facts.consumption_count
    );
    println!("reservation_id = {}", facts.reservation_id_hex);
    println!("account_id    = {}", facts.account_id_hex);
    println!();
    println!("# CLI 一致性手动比对(先 cargo build -p nlos-system-control):");
    println!(
        "& \"{}\" \"{}\" inspect-health",
        cli_path.display(),
        fixture.socket_plain().display()
    );
    println!();
    println!("# 写路径自检(pause-operation 探针)默认关闭:探针会执行真实");
    println!("# mutation,仅在开发夹具宿主显式 opt-in 后可用(深审计 42 D9):");
    println!("$env:LLMOS_DESKTOP_ALLOW_WRITE_PARITY = \"1\"");
    println!();
    println!("# 密钥种子已写入文件:{}", key_file.display());
    println!("# Ctrl-C 停止。");
}

#[cfg(any(unix, windows))]
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
    let _ = std::fs::remove_file(&key_file);
    if let Err(error) = fixture.write_key_file(&key_file) {
        eprintln!("dev_server: 密钥文件写入失败: {error}");
        std::process::exit(1);
    }

    let cli_path = repo_cli_path();
    print_platform_instructions(&fixture, &key_file, &cli_path);

    let (authenticated, plain) = fixture.serve_forever();
    let _ = tokio::join!(authenticated, plain);
}

#[cfg(not(any(unix, windows)))]
fn main() {
    eprintln!("dev_server: 仅支持 Unix domain socket 与 Windows Named Pipe 平台。");
    std::process::exit(1);
}
